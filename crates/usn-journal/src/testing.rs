//! Builders for records as the kernel lays them out, and a scripted journal.

use std::collections::{HashMap, VecDeque};
use std::ffi::{OsStr, OsString};
use std::path::PathBuf;

use crate::error::{Error, Result};
use crate::journal::JournalInfo;
use crate::source::Source;
use crate::types::{FileId, JournalId, ReparseTag, Usn};

/// Builds a V2 record, padded to 8 bytes.
pub(crate) fn v2(
    usn: i64,
    file: u64,
    parent: u64,
    reason: u32,
    attributes: u32,
    name: &str,
) -> Vec<u8> {
    let name: Vec<u16> = name.encode_utf16().collect();
    let len = (60 + name.len() * 2 + 7) & !7;
    let mut r = vec![0u8; len];
    r[0..4].copy_from_slice(&(len as u32).to_le_bytes());
    r[4..6].copy_from_slice(&2u16.to_le_bytes());
    r[8..16].copy_from_slice(&file.to_le_bytes());
    r[16..24].copy_from_slice(&parent.to_le_bytes());
    r[24..32].copy_from_slice(&usn.to_le_bytes());
    r[32..40].copy_from_slice(&133_000_000_000_000_000i64.to_le_bytes());
    r[40..44].copy_from_slice(&reason.to_le_bytes());
    r[52..56].copy_from_slice(&attributes.to_le_bytes());
    r[56..58].copy_from_slice(&((name.len() * 2) as u16).to_le_bytes());
    r[58..60].copy_from_slice(&60u16.to_le_bytes());
    for (i, unit) in name.iter().enumerate() {
        r[60 + i * 2..62 + i * 2].copy_from_slice(&unit.to_le_bytes());
    }
    r
}

/// Builds a V3 record, padded to 8 bytes.
pub(crate) fn v3(
    usn: i64,
    file: u128,
    parent: u128,
    reason: u32,
    attributes: u32,
    name: &str,
) -> Vec<u8> {
    let name: Vec<u16> = name.encode_utf16().collect();
    let len = (76 + name.len() * 2 + 7) & !7;
    let mut r = vec![0u8; len];
    r[0..4].copy_from_slice(&(len as u32).to_le_bytes());
    r[4..6].copy_from_slice(&3u16.to_le_bytes());
    r[8..24].copy_from_slice(&file.to_le_bytes());
    r[24..40].copy_from_slice(&parent.to_le_bytes());
    r[40..48].copy_from_slice(&usn.to_le_bytes());
    r[56..60].copy_from_slice(&reason.to_le_bytes());
    r[68..72].copy_from_slice(&attributes.to_le_bytes());
    r[72..74].copy_from_slice(&((name.len() * 2) as u16).to_le_bytes());
    r[74..76].copy_from_slice(&76u16.to_le_bytes());
    for (i, unit) in name.iter().enumerate() {
        r[76 + i * 2..78 + i * 2].copy_from_slice(&unit.to_le_bytes());
    }
    r
}

pub(crate) fn buffer(next: i64, records: &[Vec<u8>]) -> Vec<u8> {
    let mut b = next.to_le_bytes().to_vec();
    for r in records {
        b.extend_from_slice(r);
    }
    b
}

/// A scripted journal that serves records by position like the kernel would.
pub(crate) struct FakeJournal {
    pub info: JournalInfo,
    pub records: Vec<(Usn, Vec<u8>)>,
    pub dirs: HashMap<FileId, PathBuf>,
    /// What listing the volume reports: id, parent and name.
    pub listing: Vec<(FileId, FileId, OsString)>,
    pub read_errors: VecDeque<Error>,
    pub query_errors: VecDeque<Error>,
    pub reconnects: usize,
    pub reparse_tags: HashMap<FileId, ReparseTag>,
    pub has_names: bool,
    pub listing_unsupported: bool,
}

impl FakeJournal {
    pub fn new() -> FakeJournal {
        FakeJournal {
            info: JournalInfo {
                id: JournalId(7),
                first_usn: Usn(0),
                next_usn: Usn(0),
                lowest_valid_usn: Usn(0),
                max_usn: Usn(i64::MAX),
                maximum_size: 1 << 20,
                allocation_delta: 1 << 16,
                min_version: 2,
                max_version: 3,
            },
            records: Vec::new(),
            dirs: HashMap::new(),
            listing: vec![(FileId(10), FileId(5), OsString::from("prefilled"))],
            read_errors: VecDeque::new(),
            query_errors: VecDeque::new(),
            reconnects: 0,
            reparse_tags: HashMap::new(),
            has_names: true,
            listing_unsupported: false,
        }
    }

    pub fn append(&mut self, build: impl FnOnce(i64) -> Vec<u8>) {
        let usn = self.info.next_usn;
        let record = build(usn.0);
        self.info.next_usn = Usn(usn.0 + record.len() as i64);
        self.records.push((usn, record));
    }
}

impl Source for FakeJournal {
    fn query(&mut self) -> Result<JournalInfo> {
        match self.query_errors.pop_front() {
            Some(e) => Err(e),
            None => Ok(self.info),
        }
    }

    fn read(&mut self, start: Usn, _journal: JournalId, out: &mut [u8]) -> Result<usize> {
        if let Some(e) = self.read_errors.pop_front() {
            return Err(e);
        }
        let mut records = Vec::new();
        let mut next = start.0;
        let mut size = 8;
        for (usn, record) in self.records.iter().filter(|(u, _)| *u >= start) {
            if size + record.len() > out.len() {
                break;
            }
            size += record.len();
            records.push(record.clone());
            next = usn.0 + record.len() as i64;
        }
        let bytes = buffer(next, &records);
        out[..bytes.len()].copy_from_slice(&bytes);
        Ok(bytes.len())
    }

    fn root(&self) -> (PathBuf, FileId) {
        (PathBuf::from("C:\\"), FileId(5))
    }

    fn has_names(&self) -> bool {
        self.has_names
    }

    fn lookup_dir(&mut self, id: FileId) -> Option<PathBuf> {
        self.dirs.get(&id).cloned()
    }

    fn reparse_tag(&mut self, id: FileId) -> Option<ReparseTag> {
        self.reparse_tags.get(&id).copied()
    }

    fn for_each_directory(&mut self, visit: &mut dyn FnMut(FileId, FileId, &OsStr)) -> Result<()> {
        if self.listing_unsupported {
            return Err(Error::Unsupported);
        }
        for (id, parent, name) in &self.listing {
            visit(*id, *parent, name);
        }
        Ok(())
    }

    fn reconnect(&mut self) -> Result<()> {
        self.reconnects += 1;
        Ok(())
    }
}
