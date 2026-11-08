use std::path::PathBuf;
use std::time::{Duration, Instant};

use windows_sys::Win32::System::Ioctl::READ_USN_JOURNAL_DATA_V1;

use super::volume::Volume;
use crate::error::{Error, Result};
use crate::filter::PathFilter;
use crate::journal::{Cursor, Start};
use crate::options::ReadOptions;
use crate::paths::PathCache;
use crate::record::{MftEntry, RawRecord, Record, ScanEntry, next_record};
use crate::types::{FileAttributes, FileId, JournalId, Usn};

const BUFFER_BYTES: usize = 1024 * 1024;
const HEADER_BYTES: usize = 8;
/// How often a waiting reader checks whether new records have been written.
const WAIT_POLL: Duration = Duration::from_millis(50);

/// One buffer of records, consumed from the front.
struct Page {
    buffer: Vec<u8>,
    offset: usize,
    end: usize,
}

impl Page {
    fn new() -> Page {
        Page {
            buffer: vec![0; BUFFER_BYTES],
            offset: 0,
            end: 0,
        }
    }

    /// Marks the first `length` bytes of the buffer as freshly read.
    fn loaded(&mut self, length: usize) {
        self.offset = HEADER_BYTES.min(length);
        self.end = length;
    }

    /// The continuation value at the start of the buffer.
    fn next_position(&self) -> Result<i64> {
        if self.end < HEADER_BYTES {
            return Err(Error::Corrupt(format!(
                "buffer of {} bytes has no header",
                self.end
            )));
        }
        Ok(i64::from_le_bytes(
            self.buffer[..HEADER_BYTES].try_into().unwrap(),
        ))
    }

    fn has_records(&self) -> bool {
        self.end > HEADER_BYTES
    }

    fn next(&mut self) -> Option<Result<RawRecord<'_>>> {
        while self.offset < self.end {
            match next_record(&self.buffer[self.offset..self.end]) {
                Ok((length, Some(record))) => {
                    self.offset += length;
                    return Some(Ok(record));
                }
                Ok((length, None)) => self.offset += length,
                Err(error) => {
                    self.offset = self.end;
                    return Some(Err(error));
                }
            }
        }
        None
    }

    /// The position of the next record, without consuming it.
    fn peek_usn(&self) -> Option<Usn> {
        let mut offset = self.offset;
        while offset < self.end {
            match next_record(&self.buffer[offset..self.end]) {
                Ok((_, Some(record))) => return Some(record.usn),
                Ok((length, None)) => offset += length,
                Err(_) => return None,
            }
        }
        None
    }
}

/// Raw journal records, from [`Volume::records`].
pub struct Records<'v> {
    volume: &'v Volume,
    options: ReadOptions,
    journal: JournalId,
    position: Usn,
    page: Page,
    finished: bool,
}

impl<'v> Records<'v> {
    pub(super) fn new(volume: &'v Volume, options: ReadOptions) -> Result<Records<'v>> {
        let info = volume.journal_info()?;
        let (journal, position) = match options.start {
            Start::Now => (info.id, info.next_usn),
            Start::Oldest => (info.id, info.oldest_readable()),
            Start::At(cursor) => {
                if let Some(gap) = info.check(cursor) {
                    return Err(Error::Gap(gap));
                }
                (cursor.journal, cursor.usn)
            }
        };
        Ok(Records {
            volume,
            options,
            journal,
            position,
            page: Page::new(),
            finished: false,
        })
    }

    /// The position to resume from.
    pub fn position(&self) -> Cursor {
        Cursor {
            journal: self.journal,
            usn: self.page.peek_usn().unwrap_or(self.position),
        }
    }

    /// Reads the next buffer of records, returning false once there are none
    /// and any wait is over.
    fn fetch(&mut self) -> Result<bool> {
        let deadline = self
            .options
            .wait
            .and_then(|(_, timeout)| Instant::now().checked_add(timeout));
        loop {
            if self.read_available()? {
                return Ok(true);
            }
            let Some((min_bytes, _)) = self.options.wait else {
                return Ok(false);
            };
            if !self.sleep_until_written(min_bytes, deadline)? {
                return Ok(false);
            }
        }
    }

    /// Reads from the current position, returning false once caught up.
    fn read_available(&mut self) -> Result<bool> {
        loop {
            let request = READ_USN_JOURNAL_DATA_V1 {
                StartUsn: self.position.0,
                ReasonMask: self.options.reasons.0,
                ReturnOnlyOnClose: u32::from(self.options.only_on_close),
                Timeout: 0,
                BytesToWaitFor: 0,
                UsnJournalID: self.journal.0,
                MinMajorVersion: 2,
                MaxMajorVersion: 3,
            };
            let n = self.volume.read_journal(&request, &mut self.page.buffer)?;
            self.page.loaded(n);
            let next = Usn(self.page.next_position()?);
            let advanced = next > self.position;
            self.position = next;
            if self.page.has_records() {
                return Ok(true);
            }
            // A reason filter can leave a whole buffer empty while the position
            // still moves on, so being empty doesn't mean being caught up.
            if !advanced {
                return Ok(false);
            }
        }
    }

    /// Sleeps until `min_bytes` have been written past the current position,
    /// returning false if `deadline` comes first. The kernel can wait for this
    /// itself, but its timeout is not a limit: the call goes on blocking until
    /// there is a record to return, which with a reason filter may be never.
    fn sleep_until_written(&self, min_bytes: u64, deadline: Option<Instant>) -> Result<bool> {
        loop {
            let written = self.volume.journal_info()?.next_usn.0 - self.position.0;
            if written >= min_bytes.min(i64::MAX as u64) as i64 && written > 0 {
                return Ok(true);
            }
            let pause = match deadline {
                Some(deadline) => {
                    let remaining = deadline.saturating_duration_since(Instant::now());
                    if remaining.is_zero() {
                        return Ok(false);
                    }
                    remaining.min(WAIT_POLL)
                }
                None => WAIT_POLL,
            };
            std::thread::sleep(pause);
        }
    }
}

impl Iterator for Records<'_> {
    type Item = Result<Record>;

    fn next(&mut self) -> Option<Self::Item> {
        loop {
            if self.finished {
                return None;
            }
            match self.page.next() {
                Some(Ok(raw)) => return Some(Ok(raw.to_record())),
                Some(Err(error)) => {
                    self.finished = true;
                    return Some(Err(error));
                }
                None => {}
            }
            match self.fetch() {
                Ok(true) => {}
                Ok(false) => {
                    self.finished = true;
                    return None;
                }
                Err(error) => {
                    self.finished = true;
                    return Some(Err(error));
                }
            }
        }
    }
}

/// Master file table entries, from [`Volume::mft_entries`].
pub struct MftEntries<'v> {
    volume: &'v Volume,
    start: u64,
    page: Page,
    finished: bool,
}

impl<'v> MftEntries<'v> {
    pub(super) fn new(volume: &'v Volume) -> MftEntries<'v> {
        MftEntries {
            volume,
            start: 0,
            page: Page::new(),
            finished: false,
        }
    }

    fn fetch(&mut self) -> Result<bool> {
        let Some(n) = self.volume.read_mft(self.start, &mut self.page.buffer)? else {
            return Ok(false);
        };
        self.page.loaded(n);
        self.start = self.page.next_position()? as u64;
        Ok(true)
    }
}

impl Iterator for MftEntries<'_> {
    type Item = Result<MftEntry>;

    fn next(&mut self) -> Option<Self::Item> {
        loop {
            if self.finished {
                return None;
            }
            match self.page.next() {
                Some(Ok(raw)) => return Some(Ok(raw.to_mft_entry())),
                Some(Err(error)) => {
                    self.finished = true;
                    return Some(Err(error));
                }
                None => {}
            }
            match self.fetch() {
                Ok(true) => {}
                Ok(false) => {
                    self.finished = true;
                    return None;
                }
                Err(error) => {
                    self.finished = true;
                    return Some(Err(error));
                }
            }
        }
    }
}

/// A listing of the volume with full paths, from [`Volume::scan`].
pub struct Scan<'v> {
    volume: &'v Volume,
    filter: PathFilter,
    paths: PathCache,
    entries: MftEntries<'v>,
    start_cursor: Cursor,
}

impl<'v> Scan<'v> {
    pub(super) fn new(volume: &'v Volume, filter: PathFilter) -> Result<Scan<'v>> {
        let start_cursor = volume.journal_info()?.end_cursor();
        // Two passes, so only directories are ever held in memory.
        let mut paths = PathCache::new(volume.root().to_path_buf(), volume.root_id());
        volume.visit_directories(&mut |id, parent, name| paths.insert_dir(id, parent, name))?;
        Ok(Scan {
            volume,
            filter,
            paths,
            entries: volume.mft_entries()?,
            start_cursor,
        })
    }

    /// The position taken before the listing started. Reading changes from
    /// here reports anything that changed during or after the listing, so
    /// nothing can slip between the two, at the cost of some duplicates.
    pub fn start_cursor(&self) -> Cursor {
        self.start_cursor
    }

    fn resolve(&mut self, parent: FileId, entry: &MftEntry) -> Option<PathBuf> {
        let volume = self.volume;
        self.paths
            .path_of(parent, &entry.name, &mut |id| volume.path_of(id).ok())
    }
}

impl Iterator for Scan<'_> {
    type Item = Result<ScanEntry>;

    fn next(&mut self) -> Option<Self::Item> {
        loop {
            let entry = match self.entries.next()? {
                Ok(entry) => entry,
                Err(error) => return Some(Err(error)),
            };
            let path = self.resolve(entry.parent_id, &entry);
            if let Some(p) = &path {
                if self.filter.is_excluded(self.paths.root(), p) {
                    continue;
                }
            }
            let reparse_tag = if entry.attributes.contains(FileAttributes::REPARSE_POINT) {
                self.volume.reparse_tag(entry.id).ok().flatten()
            } else {
                None
            };
            return Some(Ok(ScanEntry {
                path,
                id: entry.id,
                parent_id: entry.parent_id,
                attributes: entry.attributes,
                reparse_tag,
                usn: entry.usn,
            }));
        }
    }
}

/// Turns raw records into full paths.
///
/// Records carry only a file's name and its parent's ID, so a full path
/// means walking up through parent directories.
pub struct PathResolver<'v> {
    volume: &'v Volume,
    paths: PathCache,
    preload: bool,
}

impl<'v> PathResolver<'v> {
    /// Reads every directory on the volume, so paths resolve even for files
    /// since deleted. On a file system that can't list itself, such as ReFS,
    /// directories are looked up as needed instead.
    pub fn new(volume: &'v Volume) -> Result<PathResolver<'v>> {
        let mut resolver = PathResolver::with(volume, true);
        resolver.reload()?;
        Ok(resolver)
    }

    /// Looks directories up as needed, which starts instantly but leaves
    /// a file with no path if its directory no longer exists.
    pub fn lazy(volume: &'v Volume) -> PathResolver<'v> {
        PathResolver::with(volume, false)
    }

    fn with(volume: &'v Volume, preload: bool) -> PathResolver<'v> {
        PathResolver {
            volume,
            paths: PathCache::new(volume.root().to_path_buf(), volume.root_id()),
            preload,
        }
    }

    /// The full path of `record`, as of that record. Call this for each
    /// record in order, because it also applies the record's own effect on
    /// directory names.
    pub fn resolve(&mut self, record: &Record) -> Option<PathBuf> {
        let volume = self.volume;
        let path = self
            .paths
            .path_of(record.parent_id, &record.name, &mut |id| {
                volume.path_of(id).ok()
            });
        if record.is_directory() {
            self.paths
                .apply_directory(record.id, record.parent_id, record.reason, &record.name);
        }
        path
    }

    /// Forgets what it has learned and starts again. Call this after a
    /// [`Gap`](crate::Gap), since directories may have changed in the lost
    /// records.
    pub fn reload(&mut self) -> Result<()> {
        self.paths.clear();
        if self.preload {
            let paths = &mut self.paths;
            let listed = self
                .volume
                .visit_directories(&mut |id, parent, name| paths.insert_dir(id, parent, name));
            if !matches!(listed, Err(Error::Unsupported)) {
                listed?;
            }
        }
        Ok(())
    }
}
