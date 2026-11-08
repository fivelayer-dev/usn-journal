use std::ffi::OsString;
use std::path::PathBuf;

use crate::error::{Error, Result};
use crate::types::{FileAttributes, FileId, FileTime, Reason, ReparseTag, SourceInfo, Usn};
use crate::wide;

const V2_HEADER: usize = 60;
const V3_HEADER: usize = 76;

/// One raw journal record.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct Record {
    /// The record's position in the journal.
    pub usn: Usn,
    /// The file or directory this record is about.
    pub id: FileId,
    /// The directory holding it when the record was written.
    pub parent_id: FileId,
    /// When the record was written.
    pub timestamp: FileTime,
    /// Why it was written.
    pub reason: Reason,
    /// What kind of process made the change.
    pub source: SourceInfo,
    /// The file's security ID.
    pub security_id: u32,
    /// The file's attributes.
    pub attributes: FileAttributes,
    /// The file's name, without its path. NTFS names can hold unpaired
    /// surrogates, so this is not a `String`. It is empty on a volume opened
    /// without elevation.
    pub name: OsString,
    /// The record's format version.
    pub version: u16,
}

impl Record {
    /// True if this record is about a directory.
    pub fn is_directory(&self) -> bool {
        self.attributes.contains(FileAttributes::DIRECTORY)
    }
}

/// One file or directory read from the master file table.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct MftEntry {
    /// The file or directory.
    pub id: FileId,
    /// The directory holding it.
    pub parent_id: FileId,
    /// Its name, without its path.
    pub name: OsString,
    /// Its attributes.
    pub attributes: FileAttributes,
    /// The position of its latest journal record.
    pub usn: Usn,
}

impl MftEntry {
    /// True if this is a directory.
    pub fn is_directory(&self) -> bool {
        self.attributes.contains(FileAttributes::DIRECTORY)
    }
}

/// One file or directory found by a scan of the volume, with its full path.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct ScanEntry {
    /// The full path, or `None` if its parent chain could not be resolved.
    pub path: Option<PathBuf>,
    /// The file or directory.
    pub id: FileId,
    /// The directory holding it.
    pub parent_id: FileId,
    /// Its attributes.
    pub attributes: FileAttributes,
    /// The kind of reparse point, if it is one and that could be read.
    pub reparse_tag: Option<ReparseTag>,
    /// The position of its latest journal record.
    pub usn: Usn,
}

impl ScanEntry {
    /// True if this is a directory.
    pub fn is_directory(&self) -> bool {
        self.attributes.contains(FileAttributes::DIRECTORY)
    }

    /// True for symbolic links, junctions and mounted folders.
    pub fn is_link(&self) -> bool {
        self.reparse_tag.is_some_and(ReparseTag::is_link)
    }
}

/// One record borrowing its name from the read buffer, which saves copying the
/// name of every record that is only needed for its flags.
#[derive(Copy, Clone, Debug)]
pub(crate) struct RawRecord<'a> {
    pub version: u16,
    pub id: FileId,
    pub parent_id: FileId,
    pub usn: Usn,
    pub timestamp: FileTime,
    pub reason: Reason,
    pub source: SourceInfo,
    pub security_id: u32,
    pub attributes: FileAttributes,
    name_bytes: &'a [u8],
}

impl RawRecord<'_> {
    pub fn name(&self) -> OsString {
        let units: Vec<u16> = self
            .name_bytes
            .chunks_exact(2)
            .map(|pair| u16::from_le_bytes([pair[0], pair[1]]))
            .collect();
        wide::from_utf16(&units)
    }

    pub fn is_directory(&self) -> bool {
        self.attributes.contains(FileAttributes::DIRECTORY)
    }

    pub fn to_record(self) -> Record {
        Record {
            usn: self.usn,
            id: self.id,
            parent_id: self.parent_id,
            timestamp: self.timestamp,
            reason: self.reason,
            source: self.source,
            security_id: self.security_id,
            attributes: self.attributes,
            name: self.name(),
            version: self.version,
        }
    }

    pub fn to_mft_entry(self) -> MftEntry {
        MftEntry {
            id: self.id,
            parent_id: self.parent_id,
            name: self.name(),
            attributes: self.attributes,
            usn: self.usn,
        }
    }
}

/// Splits a buffer filled by a journal read or an MFT enumeration into its
/// leading continuation value and the records after it.
pub(crate) fn split_buffer(buffer: &[u8]) -> Result<(Usn, RecordIter<'_>)> {
    if buffer.len() < 8 {
        return Err(Error::Corrupt(format!(
            "buffer of {} bytes has no header",
            buffer.len()
        )));
    }
    Ok((Usn(i64_at(buffer, 0)), RecordIter { rest: &buffer[8..] }))
}

/// Iterates over V2 and V3 records. V4 records only describe modified ranges
/// and always accompany a V3 record, so they are skipped. Iteration ends after
/// the first error, since the next record's position is then unknown.
pub(crate) struct RecordIter<'a> {
    rest: &'a [u8],
}

impl<'a> Iterator for RecordIter<'a> {
    type Item = Result<RawRecord<'a>>;

    fn next(&mut self) -> Option<Self::Item> {
        while !self.rest.is_empty() {
            match next_record(self.rest) {
                Ok((length, record)) => {
                    self.rest = &self.rest[length..];
                    if let Some(record) = record {
                        return Some(Ok(record));
                    }
                }
                Err(error) => {
                    self.rest = &[];
                    return Some(Err(error));
                }
            }
        }
        None
    }
}

/// Parses the record at the start of `buffer`, returning its length, and the
/// record itself unless it is a version this crate skips.
pub(crate) fn next_record(buffer: &[u8]) -> Result<(usize, Option<RawRecord<'_>>)> {
    if buffer.len() < 8 {
        return Err(Error::Corrupt(format!("{} trailing bytes", buffer.len())));
    }
    let length = u32_at(buffer, 0) as usize;
    if length < 8 || length > buffer.len() {
        return Err(Error::Corrupt(format!(
            "record length {length} with {} bytes remaining",
            buffer.len()
        )));
    }
    let record = &buffer[..length];
    let version = u16_at(record, 4);
    let parsed = match version {
        2 => {
            require(record, V2_HEADER)?;
            RawRecord {
                version,
                id: FileId(u64_at(record, 8) as u128),
                parent_id: FileId(u64_at(record, 16) as u128),
                usn: Usn(i64_at(record, 24)),
                timestamp: FileTime(i64_at(record, 32)),
                reason: Reason(u32_at(record, 40)),
                source: SourceInfo(u32_at(record, 44)),
                security_id: u32_at(record, 48),
                attributes: FileAttributes(u32_at(record, 52)),
                name_bytes: name_bytes(record, 56, 58)?,
            }
        }
        3 => {
            require(record, V3_HEADER)?;
            RawRecord {
                version,
                id: FileId(u128_at(record, 8)),
                parent_id: FileId(u128_at(record, 24)),
                usn: Usn(i64_at(record, 40)),
                timestamp: FileTime(i64_at(record, 48)),
                reason: Reason(u32_at(record, 56)),
                source: SourceInfo(u32_at(record, 60)),
                security_id: u32_at(record, 64),
                attributes: FileAttributes(u32_at(record, 68)),
                name_bytes: name_bytes(record, 72, 74)?,
            }
        }
        _ => return Ok((length, None)),
    };
    Ok((length, Some(parsed)))
}

fn require(record: &[u8], header: usize) -> Result<()> {
    if record.len() < header {
        return Err(Error::Corrupt(format!(
            "record of {} bytes is shorter than its {header} byte header",
            record.len()
        )));
    }
    Ok(())
}

fn name_bytes(record: &[u8], length_at: usize, offset_at: usize) -> Result<&[u8]> {
    let length = u16_at(record, length_at) as usize;
    let offset = u16_at(record, offset_at) as usize;
    if length % 2 != 0 {
        return Err(Error::Corrupt(format!("odd file name length {length}")));
    }
    record.get(offset..offset + length).ok_or_else(|| {
        Error::Corrupt(format!(
            "file name at {offset}+{length} overruns record of {} bytes",
            record.len()
        ))
    })
}

// Records are read with from_le_bytes because a read buffer has no alignment
// guarantee that would make casting to a struct sound.
pub(crate) fn u16_at(bytes: &[u8], at: usize) -> u16 {
    u16::from_le_bytes(bytes[at..at + 2].try_into().unwrap())
}

pub(crate) fn u32_at(bytes: &[u8], at: usize) -> u32 {
    u32::from_le_bytes(bytes[at..at + 4].try_into().unwrap())
}

pub(crate) fn u64_at(bytes: &[u8], at: usize) -> u64 {
    u64::from_le_bytes(bytes[at..at + 8].try_into().unwrap())
}

pub(crate) fn i64_at(bytes: &[u8], at: usize) -> i64 {
    i64::from_le_bytes(bytes[at..at + 8].try_into().unwrap())
}

fn u128_at(bytes: &[u8], at: usize) -> u128 {
    u128::from_le_bytes(bytes[at..at + 16].try_into().unwrap())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::*;
    use std::ffi::OsStr;

    #[test]
    fn parses_v2_fields() {
        let b = buffer(
            999,
            &[v2(42, 7, 5, Reason::DATA_EXTEND.0, 0x20, "file.txt")],
        );
        let (next, mut it) = split_buffer(&b).unwrap();
        assert_eq!(next, Usn(999));
        let r = it.next().unwrap().unwrap();
        assert_eq!(r.version, 2);
        assert_eq!(r.usn, Usn(42));
        assert_eq!(r.id, FileId(7));
        assert_eq!(r.parent_id, FileId(5));
        assert_eq!(r.reason, Reason::DATA_EXTEND);
        assert_eq!(r.attributes, FileAttributes(0x20));
        assert_eq!(r.name(), OsStr::new("file.txt"));
        assert!(it.next().is_none());
    }

    #[test]
    fn parses_v3_fields_with_full_128_bit_references() {
        let file = 0x0011_2233_4455_6677_8899_aabb_ccdd_eeff_u128;
        let b = buffer(0, &[v3(42, file, 9, Reason::CLOSE.0, 0x10, "dir")]);
        let (_, mut it) = split_buffer(&b).unwrap();
        let r = it.next().unwrap().unwrap();
        assert_eq!(r.version, 3);
        assert_eq!(r.id, FileId(file));
        assert_eq!(r.parent_id, FileId(9));
        assert!(r.is_directory());
        assert_eq!(r.name(), OsStr::new("dir"));
    }

    #[test]
    fn v2_and_v3_references_to_the_same_file_compare_equal() {
        let a = buffer(0, &[v2(1, 7, 5, 0, 0, "a")]);
        let b = buffer(0, &[v3(1, 7, 5, 0, 0, "a")]);
        let ra = split_buffer(&a).unwrap().1.next().unwrap().unwrap();
        let rb = split_buffer(&b).unwrap().1.next().unwrap().unwrap();
        assert_eq!(ra.id, rb.id);
    }

    #[test]
    fn iterates_mixed_records_and_decodes_non_ascii_names() {
        let b = buffer(
            0,
            &[v2(1, 1, 5, 0, 0, "日本語.txt"), v3(2, 2, 5, 0, 0, "é")],
        );
        let names: Vec<_> = split_buffer(&b)
            .unwrap()
            .1
            .map(|r| r.unwrap().name())
            .collect();
        assert_eq!(names, [OsStr::new("日本語.txt"), OsStr::new("é")]);
    }

    #[test]
    fn empty_buffer_with_only_a_header_has_no_records() {
        let header = 5i64.to_le_bytes();
        let (next, mut it) = split_buffer(&header).unwrap();
        assert_eq!(next, Usn(5));
        assert!(it.next().is_none());
    }

    #[test]
    fn short_buffer_is_rejected() {
        assert!(matches!(split_buffer(&[0; 7]), Err(Error::Corrupt(_))));
    }

    #[test]
    fn skips_v4_range_tracking_records() {
        let mut v4 = vec![0u8; 80];
        v4[0..4].copy_from_slice(&80u32.to_le_bytes());
        v4[4..6].copy_from_slice(&4u16.to_le_bytes());
        let b = buffer(0, &[v4, v2(9, 1, 5, 0, 0, "after")]);
        let recs: Vec<_> = split_buffer(&b).unwrap().1.collect::<Result<_>>().unwrap();
        assert_eq!(recs.len(), 1);
        assert_eq!(recs[0].usn, Usn(9));
    }

    fn first_error(mut record: Vec<u8>, edit: impl FnOnce(&mut Vec<u8>)) -> Error {
        edit(&mut record);
        let b = buffer(0, &[record]);
        let (_, mut it) = split_buffer(&b).unwrap();
        let e = it.next().unwrap().unwrap_err();
        assert!(it.next().is_none(), "iteration must stop after an error");
        e
    }

    #[test]
    fn rejects_zero_record_length_rather_than_looping_forever() {
        let e = first_error(v2(1, 1, 5, 0, 0, "x"), |r| r[0..4].fill(0));
        assert!(matches!(e, Error::Corrupt(_)));
    }

    #[test]
    fn rejects_record_longer_than_the_buffer() {
        let e = first_error(v2(1, 1, 5, 0, 0, "x"), |r| {
            r[0..4].copy_from_slice(&4096u32.to_le_bytes())
        });
        assert!(matches!(e, Error::Corrupt(_)));
    }

    #[test]
    fn rejects_record_shorter_than_its_header() {
        let e = first_error(v2(1, 1, 5, 0, 0, "x"), |r| {
            r.truncate(40);
            r[0..4].copy_from_slice(&40u32.to_le_bytes());
        });
        assert!(matches!(e, Error::Corrupt(_)));
    }

    #[test]
    fn rejects_name_that_overruns_the_record() {
        let e = first_error(v2(1, 1, 5, 0, 0, "x"), |r| {
            r[56..58].copy_from_slice(&1000u16.to_le_bytes())
        });
        assert!(matches!(e, Error::Corrupt(_)));
    }

    #[test]
    fn rejects_odd_name_length() {
        let e = first_error(v2(1, 1, 5, 0, 0, "xy"), |r| {
            r[56..58].copy_from_slice(&3u16.to_le_bytes())
        });
        assert!(matches!(e, Error::Corrupt(_)));
    }

    #[cfg(windows)]
    #[test]
    fn lone_surrogates_in_names_are_preserved() {
        use std::os::windows::ffi::OsStrExt;
        let mut r = v2(1, 1, 5, 0, 0, "ab");
        r[60..62].copy_from_slice(&0xD800u16.to_le_bytes());
        let b = buffer(0, &[r]);
        let rec = split_buffer(&b).unwrap().1.next().unwrap().unwrap();
        let units: Vec<u16> = rec.name().encode_wide().collect();
        assert_eq!(units, [0xD800, b'b' as u16]);
    }

    /// Small and fast, and deterministic so that a failure can be reproduced.
    fn xorshift(state: &mut u64) -> u64 {
        *state ^= *state << 13;
        *state ^= *state >> 7;
        *state ^= *state << 17;
        *state
    }

    #[test]
    fn damaged_buffers_are_errors_and_never_panic() {
        let original = buffer(
            7,
            &[
                v2(1, 2, 3, Reason::DATA_EXTEND.0, 0x20, "first.txt"),
                v3(4, 5, 6, Reason::CLOSE.0, 0x10, "second"),
                v2(7, 8, 9, Reason::FILE_DELETE.0, 0, "日本語"),
            ],
        );
        let mut state = 0x1234_5678_9abc_def0;
        for _ in 0..100_000 {
            let mut damaged = original.clone();
            for _ in 0..=(xorshift(&mut state) % 8) {
                let at = xorshift(&mut state) as usize % damaged.len();
                damaged[at] = xorshift(&mut state) as u8;
            }
            if xorshift(&mut state) % 4 == 0 {
                damaged.truncate(xorshift(&mut state) as usize % damaged.len());
            }
            if let Ok((_, records)) = split_buffer(&damaged) {
                for record in records.flatten() {
                    let _ = record.to_record();
                    let _ = record.to_mft_entry();
                }
            }
        }
    }

    #[test]
    fn a_record_copies_every_field() {
        let mut bytes = v3(42, 7, 5, Reason::DATA_EXTEND.0, 0x20, "file.txt");
        bytes[60..64].copy_from_slice(&SourceInfo::DATA_MANAGEMENT.0.to_le_bytes());
        bytes[64..68].copy_from_slice(&9u32.to_le_bytes());
        bytes[48..56].copy_from_slice(&1234i64.to_le_bytes());
        let b = buffer(0, &[bytes]);
        let record = split_buffer(&b)
            .unwrap()
            .1
            .next()
            .unwrap()
            .unwrap()
            .to_record();
        assert_eq!(record.usn, Usn(42));
        assert_eq!(record.id, FileId(7));
        assert_eq!(record.parent_id, FileId(5));
        assert_eq!(record.timestamp, FileTime(1234));
        assert_eq!(record.reason, Reason::DATA_EXTEND);
        assert_eq!(record.source, SourceInfo::DATA_MANAGEMENT);
        assert_eq!(record.security_id, 9);
        assert_eq!(record.attributes, FileAttributes(0x20));
        assert_eq!(record.name, OsStr::new("file.txt"));
        assert_eq!(record.version, 3);
        assert!(!record.is_directory());
    }

    #[test]
    fn an_mft_entry_copies_every_field() {
        let b = buffer(0, &[v3(42, 7, 5, 0, 0x10, "dir")]);
        let entry = split_buffer(&b)
            .unwrap()
            .1
            .next()
            .unwrap()
            .unwrap()
            .to_mft_entry();
        assert_eq!(entry.usn, Usn(42));
        assert_eq!(entry.id, FileId(7));
        assert_eq!(entry.parent_id, FileId(5));
        assert_eq!(entry.name, OsStr::new("dir"));
        assert!(entry.is_directory());
    }
}
