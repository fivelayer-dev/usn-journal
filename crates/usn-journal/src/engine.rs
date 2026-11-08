use std::path::PathBuf;
use std::time::Instant;

use crate::change::{ChangeBatch, ChangeKind, FileChange};
use crate::coalesce::{Coalescer, Settled};
use crate::error::{Error, Result};
use crate::journal::{Cursor, Gap, JournalInfo, Start};
use crate::options::{ChangeOptions, SettleOptions};
use crate::paths::{PathCache, is_rename_old};
use crate::record::{RawRecord, split_buffer};
use crate::source::Source;
use crate::types::{FileAttributes, JournalId, Reason, Usn};

pub(crate) const READ_BUFFER_BYTES: usize = 1024 * 1024;
const MAX_READS_PER_POLL: usize = 64;

/// Turns raw journal records into per-file changes. The public change readers
/// are thin wrappers around this, so that it can be tested against a fake
/// journal on any platform.
pub(crate) struct Engine<S> {
    source: S,
    options: ChangeOptions,
    has_names: bool,
    journal: JournalId,
    next_usn: Usn,
    coalescer: Coalescer,
    paths: PathCache,
    buffer: Vec<u8>,
}

/// Fills the cache from a listing of the volume. Some file systems, such as
/// ReFS, can't list themselves, and their directories are looked up as needed.
fn preload<S: Source>(source: &mut S, paths: &mut PathCache) -> Result<()> {
    match source.for_each_directory(&mut |id, parent, name| paths.insert_dir(id, parent, name)) {
        Err(Error::Unsupported) => Ok(()),
        other => other,
    }
}

impl<S: Source> Engine<S> {
    pub fn new(
        mut source: S,
        start: Start,
        options: ChangeOptions,
        settle: SettleOptions,
    ) -> Result<Engine<S>> {
        let info = source.query()?;
        let (journal, next_usn) = match start {
            Start::Now => (info.id, info.next_usn),
            Start::Oldest => (info.id, info.oldest_readable()),
            Start::At(cursor) => (cursor.journal, cursor.usn),
        };
        let (root, root_id) = source.root();
        let has_names = source.has_names();
        let mut paths = PathCache::new(root, root_id);
        if options.preload_directories && has_names {
            preload(&mut source, &mut paths)?;
        }
        Ok(Engine {
            source,
            options,
            has_names,
            journal,
            next_usn,
            coalescer: Coalescer::new(settle),
            paths,
            buffer: vec![0; READ_BUFFER_BYTES],
        })
    }

    pub fn source(&self) -> &S {
        &self.source
    }

    #[cfg(test)]
    pub fn source_mut(&mut self) -> &mut S {
        &mut self.source
    }

    /// Asks the source to reopen itself after an error from `poll`.
    pub fn reconnect(&mut self) -> Result<()> {
        self.source.reconnect()
    }

    /// Reads what is available and returns the changes now ready to report.
    pub fn poll(&mut self, now: Instant) -> Result<ChangeBatch> {
        let info = self.source.query()?;
        let mut gap = info.check(Cursor {
            journal: self.journal,
            usn: self.next_usn,
        });
        let mut caught_up = false;
        if gap.is_none() {
            match self.read_available(now) {
                Ok(done) => caught_up = done,
                // Purged between the query and the read.
                Err(Error::Gap(_)) => gap = Some(self.purged(&info)),
                Err(e) => return Err(e),
            }
        }

        let settled = if gap.is_some() {
            // Pending positions may belong to a journal that no longer exists,
            // so they must not hold back the cursor.
            self.journal = info.id;
            self.next_usn = info.next_usn;
            caught_up = true;
            self.reload_paths();
            self.coalescer.flush()
        } else {
            self.coalescer.drain_ready(now)
        };
        Ok(self.batch(settled, gap, caught_up, &info))
    }

    /// Reads up to the journal's current end.
    pub fn read_to_end(&mut self, now: Instant) -> Result<ChangeBatch> {
        let end = self.source.query()?.end_cursor();
        self.read_to(end, now)
    }

    /// Reads up to `end` and reports everything before it at once.
    pub fn read_to(&mut self, end: Cursor, now: Instant) -> Result<ChangeBatch> {
        let info = self.source.query()?;
        let same_journal = end.journal == info.id;
        let mut gap = if same_journal {
            info.check(Cursor {
                journal: self.journal,
                usn: self.next_usn,
            })
        } else {
            Some(Gap::JournalRecreated {
                old: end.journal,
                new: info.id,
            })
        };
        let end_usn = end.usn.min(info.next_usn);
        if gap.is_none() {
            match self.read_available_until(end_usn, now) {
                Ok(()) => {}
                // Purged between the query and the read.
                Err(Error::Gap(_)) => gap = Some(self.purged(&info)),
                Err(e) => return Err(e),
            }
        }

        let settled = if gap.is_some() {
            self.journal = info.id;
            // Not the journal's end, because changes made after `end` would then
            // be lost. A position before the journal's start reports the gap
            // again next time, which is right: those changes are gone too.
            self.next_usn = if same_journal {
                end_usn
            } else {
                info.oldest_readable()
            };
            self.reload_paths();
            self.coalescer.flush()
        } else {
            self.coalescer.drain_complete()
        };
        Ok(self.batch(settled, gap, true, &info))
    }

    fn purged(&self, info: &JournalInfo) -> Gap {
        Gap::RecordsLost {
            cursor: self.next_usn,
            oldest_readable: info.oldest_readable(),
        }
    }

    /// Directories may have been renamed or removed in records that can no
    /// longer be read, so what the cache knows can't be trusted. If reloading
    /// fails, directories are looked up one at a time instead.
    fn reload_paths(&mut self) {
        self.paths.clear();
        if self.options.preload_directories && self.has_names {
            let _ = preload(&mut self.source, &mut self.paths);
        }
    }

    fn batch(
        &mut self,
        settled: Vec<Settled>,
        gap: Option<Gap>,
        caught_up: bool,
        info: &JournalInfo,
    ) -> ChangeBatch {
        let changes: Vec<FileChange> = settled.into_iter().filter_map(|s| self.report(s)).collect();
        let cursor = self.cursor();
        ChangeBatch {
            changes,
            gap,
            cursor,
            caught_up,
            lag_bytes: (info.next_usn.0 - self.next_usn.0).max(0) as u64,
            headroom_bytes: (cursor.usn.0 - info.oldest_readable().0).max(0) as u64,
        }
    }

    /// The position that is safe to resume from.
    pub fn cursor(&self) -> Cursor {
        let usn = self
            .coalescer
            .oldest_usn()
            .map_or(self.next_usn, |usn| usn.min(self.next_usn));
        Cursor {
            journal: self.journal,
            usn,
        }
    }

    /// Returns whether all available records were read.
    fn read_available(&mut self, now: Instant) -> Result<bool> {
        let mut buffer = std::mem::take(&mut self.buffer);
        let result = self.read_into(&mut buffer, now);
        self.buffer = buffer;
        result
    }

    fn read_into(&mut self, buffer: &mut [u8], now: Instant) -> Result<bool> {
        for _ in 0..MAX_READS_PER_POLL {
            let n = self.source.read(self.next_usn, self.journal, buffer)?;
            let (next, records) = split_buffer(&buffer[..n])?;
            let mut any = false;
            for record in records {
                let record = record?;
                any = true;
                self.consume(&record, now);
            }
            let advanced = next > self.next_usn;
            self.next_usn = next;
            if !any && !advanced {
                return Ok(true);
            }
        }
        Ok(false)
    }

    fn read_available_until(&mut self, end: Usn, now: Instant) -> Result<()> {
        let mut buffer = std::mem::take(&mut self.buffer);
        let result = self.read_into_until(&mut buffer, end, now);
        self.buffer = buffer;
        result
    }

    /// Stops at the first record at or after `end`, so that record is read again
    /// next time. The distance to `end` bounds the work, so there's no read limit.
    fn read_into_until(&mut self, buffer: &mut [u8], end: Usn, now: Instant) -> Result<()> {
        while self.next_usn < end {
            let n = self.source.read(self.next_usn, self.journal, buffer)?;
            let (next, records) = split_buffer(&buffer[..n])?;
            for record in records {
                let record = record?;
                if record.usn >= end {
                    self.next_usn = record.usn;
                    return Ok(());
                }
                self.consume(&record, now);
            }
            if next <= self.next_usn {
                break;
            }
            self.next_usn = next;
        }
        Ok(())
    }

    fn consume(&mut self, record: &RawRecord<'_>, now: Instant) {
        let needs_path = self.has_names
            && (is_rename_old(record.reason) || record.reason.contains(Reason::FILE_DELETE));
        let path_before = if needs_path {
            let source = &mut self.source;
            self.paths
                .path_of(record.parent_id, &record.name(), &mut |id| {
                    source.lookup_dir(id)
                })
        } else {
            None
        };
        if !record.source.intersects(self.options.ignored_sources) {
            self.coalescer.push(record, path_before, now);
        }
        if self.has_names {
            self.paths.apply(record);
        }
    }

    fn report(&mut self, s: Settled) -> Option<FileChange> {
        if s.attributes.contains(FileAttributes::DIRECTORY) && !self.options.directories {
            return None;
        }
        let path = self.current_path(&s);

        let root = self.paths.root();
        let filter = &self.options.filter;
        let excluded =
            |p: &Option<PathBuf>| p.as_ref().is_some_and(|p| filter.is_excluded(root, p));
        let path_excluded = excluded(&path);
        let from_excluded = excluded(&s.rename_from);

        // Moves across the filter's boundary look like a create or a delete to
        // a reader that only knows about what the filter keeps.
        let (kind, path, previous_path) =
            match (s.rename_from.is_some(), from_excluded, path_excluded) {
                (_, _, true) if s.deleted => return None,
                (false, _, true) | (true, true, true) => return None,
                (true, true, false) => (ChangeKind::Created, path, None),
                (true, false, true) => (ChangeKind::Deleted, s.rename_from.clone(), None),
                _ => (self.classify(&s), path, s.rename_from.clone()),
            };
        let reparse_tag = if s.attributes.contains(FileAttributes::REPARSE_POINT)
            && kind != ChangeKind::Deleted
        {
            self.source.reparse_tag(s.id)
        } else {
            None
        };
        Some(FileChange {
            kind,
            path,
            previous_path,
            still_open: s.still_open,
            id: s.id,
            parent_id: s.parent_id,
            reasons: s.reasons,
            attributes: s.attributes,
            reparse_tag,
            first_usn: s.first_usn,
            last_usn: s.last_usn,
            timestamp: s.timestamp,
        })
    }

    fn current_path(&mut self, s: &Settled) -> Option<PathBuf> {
        if !self.has_names {
            None
        } else if s.deleted {
            s.deleted_path.clone()
        } else {
            // Resolved now rather than per record, so it reflects any later
            // rename of a parent directory.
            let source = &mut self.source;
            self.paths
                .path_of(s.parent_id, &s.name, &mut |id| source.lookup_dir(id))
        }
    }

    fn classify(&self, s: &Settled) -> ChangeKind {
        if s.deleted {
            ChangeKind::Deleted
        } else if s.created && s.rename_from.is_none() {
            ChangeKind::Created
        } else if s.reasons.intersects(Reason::CONTENT) {
            ChangeKind::Modified
        } else if s.reasons.contains(Reason::RENAME_NEW_NAME) {
            // Without an old path a reader can't apply a rename, so this is
            // something that appeared. A volume with no names has no paths at
            // all, and there the rename is still worth knowing about.
            if s.rename_from.is_some() || !self.has_names {
                ChangeKind::Renamed
            } else {
                ChangeKind::Created
            }
        } else {
            ChangeKind::MetadataChanged
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::filter::PathFilter;
    use crate::testing::FakeJournal;
    use crate::testing::{v2, v3};
    use crate::types::{FileId, ReparseTag, SourceInfo};
    use std::path::Path;
    use std::time::Duration;

    const SETTLE: Duration = Duration::from_secs(2);
    const ROOT: u64 = 5;
    const DIR: u32 = FileAttributes::DIRECTORY.0;
    const CLOSE: u32 = Reason::CLOSE.0;

    /// The options the tests vary, in one place.
    #[derive(Clone)]
    struct Setup {
        start: Start,
        settle: SettleOptions,
        filter: PathFilter,
        include_directories: bool,
        ignored_sources: u32,
        prefill_paths: bool,
    }

    impl Default for Setup {
        fn default() -> Self {
            Setup {
                start: Start::Now,
                settle: SettleOptions::new(),
                filter: PathFilter::new(),
                include_directories: true,
                ignored_sources: 0,
                prefill_paths: true,
            }
        }
    }

    fn build(journal: FakeJournal, setup: Setup) -> Result<Engine<FakeJournal>> {
        let options = ChangeOptions::new()
            .filter(setup.filter)
            .directories(setup.include_directories)
            .ignored_sources(SourceInfo(setup.ignored_sources))
            .preload_directories(setup.prefill_paths);
        Engine::new(journal, setup.start, options, setup.settle)
    }

    fn options() -> Setup {
        Setup {
            start: Start::Oldest,
            settle: SettleOptions {
                settle: SETTLE,
                max_open_delay: Duration::from_secs(60),
                max_pending: 1000,
            },
            filter: PathFilter::new(),
            ..Setup::default()
        }
    }

    fn stream(journal: FakeJournal) -> Engine<FakeJournal> {
        build(journal, options()).unwrap()
    }

    fn poll_settled(s: &mut Engine<FakeJournal>) -> ChangeBatch {
        let t = Instant::now();
        s.poll(t).unwrap();
        s.poll(t + SETTLE).unwrap()
    }

    fn add(j: &mut FakeJournal, file: u64, parent: u64, reason: u32, attrs: u32, name: &str) {
        j.append(|usn| v2(usn, file, parent, reason, attrs, name));
    }

    #[test]
    fn reports_a_new_file_with_its_full_path() {
        let mut j = FakeJournal::new();
        add(&mut j, 20, ROOT, Reason::FILE_CREATE.0, 0, "a.txt");
        add(&mut j, 20, ROOT, Reason::DATA_EXTEND.0 | CLOSE, 0, "a.txt");
        let mut s = stream(j);
        let b = poll_settled(&mut s);
        assert_eq!(b.changes.len(), 1);
        let c = &b.changes[0];
        assert_eq!(c.kind, ChangeKind::Created);
        assert_eq!(c.path.as_deref(), Some(Path::new("C:\\a.txt")));
        assert!(c.has_new_content());
    }

    #[test]
    fn parses_v3_records_end_to_end() {
        let mut j = FakeJournal::new();
        j.append(|usn| {
            v3(
                usn,
                20,
                ROOT as u128,
                Reason::DATA_OVERWRITE.0 | CLOSE,
                0,
                "v3.txt",
            )
        });
        let b = poll_settled(&mut stream(j));
        assert_eq!(b.changes[0].kind, ChangeKind::Modified);
        assert_eq!(b.changes[0].path.as_deref(), Some(Path::new("C:\\v3.txt")));
    }

    #[test]
    fn classifies_each_action() {
        for (reason, expected) in [
            (Reason::DATA_OVERWRITE, ChangeKind::Modified),
            (Reason::STREAM_CHANGE, ChangeKind::Modified),
            (Reason::BASIC_INFO_CHANGE, ChangeKind::MetadataChanged),
            (Reason::SECURITY_CHANGE, ChangeKind::MetadataChanged),
        ] {
            let mut j = FakeJournal::new();
            add(&mut j, 20, ROOT, reason.0 | CLOSE, 0, "f");
            assert_eq!(
                poll_settled(&mut stream(j)).changes[0].kind,
                expected,
                "{reason:?}"
            );
        }
    }

    #[test]
    fn metadata_and_rename_only_changes_do_not_need_copying() {
        let mut j = FakeJournal::new();
        add(&mut j, 20, ROOT, Reason::RENAME_OLD_NAME.0, 0, "old");
        add(
            &mut j,
            20,
            ROOT,
            Reason::RENAME_NEW_NAME.0 | CLOSE,
            0,
            "new",
        );
        add(
            &mut j,
            21,
            ROOT,
            Reason::BASIC_INFO_CHANGE.0 | CLOSE,
            0,
            "m",
        );
        let b = poll_settled(&mut stream(j));
        assert_eq!(b.changes[0].kind, ChangeKind::Renamed);
        assert_eq!(b.changes[0].path.as_deref(), Some(Path::new("C:\\new")));
        assert_eq!(
            b.changes[0].previous_path.as_deref(),
            Some(Path::new("C:\\old"))
        );
        assert!(b.changes.iter().all(|c| !c.has_new_content()));
    }

    #[test]
    fn renamed_and_modified_is_a_modification_that_keeps_the_old_path() {
        let mut j = FakeJournal::new();
        add(&mut j, 20, ROOT, Reason::RENAME_OLD_NAME.0, 0, "old");
        add(&mut j, 20, ROOT, Reason::RENAME_NEW_NAME.0, 0, "new");
        add(&mut j, 20, ROOT, Reason::DATA_EXTEND.0 | CLOSE, 0, "new");
        let c = &poll_settled(&mut stream(j)).changes[0];
        assert_eq!(c.kind, ChangeKind::Modified);
        assert_eq!(c.previous_path.as_deref(), Some(Path::new("C:\\old")));
    }

    #[test]
    fn directories_are_not_flagged_for_copying() {
        let mut j = FakeJournal::new();
        add(&mut j, 20, ROOT, Reason::FILE_CREATE.0 | CLOSE, DIR, "d");
        let c = &poll_settled(&mut stream(j)).changes[0];
        assert!(c.is_directory());
        assert_eq!(c.kind, ChangeKind::Created);
        assert!(!c.has_new_content());
    }

    #[test]
    fn directories_can_be_left_out() {
        let mut j = FakeJournal::new();
        add(&mut j, 20, ROOT, Reason::FILE_CREATE.0 | CLOSE, DIR, "d");
        add(&mut j, 21, 20, Reason::FILE_CREATE.0 | CLOSE, 0, "f");
        let mut s = build(
            j,
            Setup {
                include_directories: false,
                ..options()
            },
        )
        .unwrap();
        let b = poll_settled(&mut s);
        assert_eq!(b.changes.len(), 1);
        assert_eq!(b.changes[0].path.as_deref(), Some(Path::new("C:\\d\\f")));
    }

    #[test]
    fn path_reflects_a_parent_renamed_after_the_file_changed() {
        let mut j = FakeJournal::new();
        add(
            &mut j,
            20,
            ROOT,
            Reason::FILE_CREATE.0 | CLOSE,
            DIR,
            "before",
        );
        add(&mut j, 21, 20, Reason::DATA_EXTEND.0 | CLOSE, 0, "f");
        add(&mut j, 20, ROOT, Reason::RENAME_OLD_NAME.0, DIR, "before");
        add(
            &mut j,
            20,
            ROOT,
            Reason::RENAME_NEW_NAME.0 | CLOSE,
            DIR,
            "after",
        );
        let b = poll_settled(&mut stream(j));
        let file = b.changes.iter().find(|c| c.id == FileId(21)).unwrap();
        assert_eq!(file.path.as_deref(), Some(Path::new("C:\\after\\f")));
    }

    #[test]
    fn deleted_tree_keeps_paths_when_children_go_before_parents() {
        let mut j = FakeJournal::new();
        add(&mut j, 20, ROOT, Reason::BASIC_INFO_CHANGE.0, DIR, "d");
        add(&mut j, 21, 20, Reason::FILE_DELETE.0 | CLOSE, 0, "f");
        add(&mut j, 20, ROOT, Reason::FILE_DELETE.0 | CLOSE, DIR, "d");
        let b = poll_settled(&mut stream(j));
        assert!(b.changes.iter().all(|c| c.kind == ChangeKind::Deleted));
        let paths: Vec<_> = b.changes.iter().map(|c| c.path.clone().unwrap()).collect();
        assert_eq!(paths, [PathBuf::from("C:\\d\\f"), PathBuf::from("C:\\d")]);
    }

    #[test]
    fn deleted_file_with_unknown_parent_has_no_path() {
        let mut j = FakeJournal::new();
        add(&mut j, 21, 99, Reason::FILE_DELETE.0 | CLOSE, 0, "f");
        let c = &poll_settled(&mut stream(j)).changes[0];
        assert_eq!(c.kind, ChangeKind::Deleted);
        assert_eq!(c.path, None);
    }

    #[test]
    fn unknown_parents_are_looked_up_through_the_source() {
        let mut j = FakeJournal::new();
        j.dirs.insert(FileId(99), PathBuf::from("C:\\found\\dir"));
        add(&mut j, 21, 99, Reason::DATA_EXTEND.0 | CLOSE, 0, "f");
        let c = &poll_settled(&mut stream(j)).changes[0];
        assert_eq!(c.path.as_deref(), Some(Path::new("C:\\found\\dir\\f")));
    }

    #[test]
    fn prefill_populates_paths_only_when_enabled() {
        let journal = || {
            let mut j = FakeJournal::new();
            add(&mut j, 21, 10, Reason::DATA_EXTEND.0 | CLOSE, 0, "f");
            j
        };
        let c = poll_settled(&mut stream(journal())).changes.remove(0);
        assert_eq!(c.path.as_deref(), Some(Path::new("C:\\prefilled\\f")));

        let mut s = build(
            journal(),
            Setup {
                prefill_paths: false,
                ..options()
            },
        )
        .unwrap();
        assert_eq!(poll_settled(&mut s).changes[0].path, None);
    }

    fn filtered_stream(j: FakeJournal) -> Engine<FakeJournal> {
        let filter = PathFilter::new().exclude("skip");
        build(
            j,
            Setup {
                filter,
                ..options()
            },
        )
        .unwrap()
    }

    fn with_skip_dir() -> FakeJournal {
        let mut j = FakeJournal::new();
        add(&mut j, 30, ROOT, Reason::FILE_CREATE.0 | CLOSE, DIR, "skip");
        j
    }

    fn file_change(b: &ChangeBatch) -> Option<&FileChange> {
        b.changes.iter().find(|c| c.id == FileId(21))
    }

    #[test]
    fn excluded_paths_are_dropped() {
        let mut j = with_skip_dir();
        add(&mut j, 21, 30, Reason::DATA_EXTEND.0 | CLOSE, 0, "f");
        assert!(file_change(&poll_settled(&mut filtered_stream(j))).is_none());
    }

    #[test]
    fn deletes_of_excluded_paths_are_dropped() {
        let mut j = with_skip_dir();
        add(&mut j, 21, 30, Reason::FILE_DELETE.0 | CLOSE, 0, "f");
        assert!(file_change(&poll_settled(&mut filtered_stream(j))).is_none());
    }

    #[test]
    fn move_out_of_an_excluded_path_counts_as_created() {
        let mut j = with_skip_dir();
        add(&mut j, 21, 30, Reason::RENAME_OLD_NAME.0, 0, "f");
        add(&mut j, 21, ROOT, Reason::RENAME_NEW_NAME.0 | CLOSE, 0, "f");
        let b = poll_settled(&mut filtered_stream(j));
        let c = file_change(&b).unwrap();
        assert_eq!(c.kind, ChangeKind::Created);
        assert_eq!(c.path.as_deref(), Some(Path::new("C:\\f")));
        assert_eq!(c.previous_path, None);
        assert!(c.has_new_content());
    }

    #[test]
    fn move_into_an_excluded_path_counts_as_deleted() {
        let mut j = with_skip_dir();
        add(&mut j, 21, ROOT, Reason::RENAME_OLD_NAME.0, 0, "f");
        add(&mut j, 21, 30, Reason::RENAME_NEW_NAME.0 | CLOSE, 0, "f");
        let b = poll_settled(&mut filtered_stream(j));
        let c = file_change(&b).unwrap();
        assert_eq!(c.kind, ChangeKind::Deleted);
        assert_eq!(c.path.as_deref(), Some(Path::new("C:\\f")));
    }

    #[test]
    fn move_within_excluded_paths_is_dropped() {
        let mut j = with_skip_dir();
        add(&mut j, 21, 30, Reason::RENAME_OLD_NAME.0, 0, "a");
        add(&mut j, 21, 30, Reason::RENAME_NEW_NAME.0 | CLOSE, 0, "b");
        assert!(file_change(&poll_settled(&mut filtered_stream(j))).is_none());
    }

    #[test]
    fn ignored_sources_are_skipped_but_still_update_paths() {
        let mut j = FakeJournal::new();
        j.append(|usn| {
            let mut r = v2(usn, 20, ROOT, Reason::FILE_CREATE.0 | CLOSE, DIR, "d");
            r[44..48].copy_from_slice(&1u32.to_le_bytes());
            r
        });
        add(&mut j, 21, 20, Reason::DATA_EXTEND.0 | CLOSE, 0, "f");
        let mut s = build(
            j,
            Setup {
                ignored_sources: 1,
                ..options()
            },
        )
        .unwrap();
        let b = poll_settled(&mut s);
        assert_eq!(b.changes.len(), 1);
        assert_eq!(b.changes[0].path.as_deref(), Some(Path::new("C:\\d\\f")));
    }

    #[test]
    fn cursor_stays_behind_unreported_changes() {
        let mut j = FakeJournal::new();
        add(&mut j, 20, ROOT, Reason::DATA_EXTEND.0 | CLOSE, 0, "a");
        let end = j.info.next_usn;
        let mut s = stream(j);
        let t = Instant::now();

        let held = s.poll(t).unwrap();
        assert!(held.changes.is_empty());
        assert_eq!(
            held.cursor.usn,
            Usn(0),
            "the file has not been reported yet"
        );
        assert_eq!(held.lag_bytes, 0);

        let reported = s.poll(t + SETTLE).unwrap();
        assert_eq!(reported.changes.len(), 1);
        assert_eq!(reported.cursor.usn, end);
    }

    #[test]
    fn resuming_from_a_cursor_reports_unfinished_work_again() {
        let mut j = FakeJournal::new();
        add(&mut j, 20, ROOT, Reason::DATA_EXTEND.0 | CLOSE, 0, "a");
        let cursor = {
            let mut first = stream(j);
            first.poll(Instant::now()).unwrap().cursor
        };
        // A crash here loses the coalescer's memory of file 20.
        let mut j = FakeJournal::new();
        add(&mut j, 20, ROOT, Reason::DATA_EXTEND.0 | CLOSE, 0, "a");
        let mut s = build(
            j,
            Setup {
                start: Start::At(cursor),
                ..options()
            },
        )
        .unwrap();
        assert_eq!(poll_settled(&mut s).changes.len(), 1);
    }

    #[test]
    fn start_now_skips_existing_records() {
        let mut j = FakeJournal::new();
        add(&mut j, 20, ROOT, Reason::DATA_EXTEND.0 | CLOSE, 0, "old");
        let mut s = build(
            j,
            Setup {
                start: Start::Now,
                ..options()
            },
        )
        .unwrap();
        assert!(poll_settled(&mut s).changes.is_empty());
    }

    #[test]
    fn reads_past_a_single_buffer() {
        let mut j = FakeJournal::new();
        // Long names make this several times the size of the read buffer.
        let name = "n".repeat(100);
        let count = READ_BUFFER_BYTES * 3 / v2(0, 0, 0, 0, 0, &name).len();
        for file in 0..count {
            add(
                &mut j,
                100 + file as u64,
                ROOT,
                Reason::DATA_EXTEND.0 | CLOSE,
                0,
                &name,
            );
        }
        let mut s = stream(j);
        let t = Instant::now();
        // More files than max_pending, so the first poll reports them too.
        let first = s.poll(t).unwrap();
        let second = s.poll(t + SETTLE).unwrap();
        assert_eq!(first.changes.len() + second.changes.len(), count);
        assert!(second.caught_up);
    }

    #[test]
    fn journal_recreated_reports_a_gap_and_moves_to_the_new_journal() {
        let mut j = FakeJournal::new();
        add(&mut j, 20, ROOT, Reason::DATA_EXTEND.0, 0, "open");
        let mut s = stream(j);
        let t = Instant::now();
        s.poll(t).unwrap();

        s.source.info.id = JournalId(8);
        s.source.info.next_usn = Usn(50_000);
        s.source.info.first_usn = Usn(50_000);
        let b = s.poll(t).unwrap();
        assert_eq!(
            b.gap,
            Some(Gap::JournalRecreated {
                old: JournalId(7),
                new: JournalId(8)
            })
        );
        assert_eq!(
            b.cursor,
            Cursor {
                journal: JournalId(8),
                usn: Usn(50_000)
            }
        );
        assert_eq!(
            b.changes.len(),
            1,
            "changes seen before the gap must still be reported"
        );
        assert!(s.poll(t).unwrap().gap.is_none());
    }

    #[test]
    fn purged_records_report_a_gap() {
        let mut j = FakeJournal::new();
        add(&mut j, 20, ROOT, Reason::DATA_EXTEND.0, 0, "x");
        let cursor = Cursor {
            journal: JournalId(7),
            usn: Usn(0),
        };
        j.info.first_usn = Usn(16);
        j.info.next_usn = Usn(1000);
        let mut s = build(
            j,
            Setup {
                start: Start::At(cursor),
                ..options()
            },
        )
        .unwrap();
        let b = s.poll(Instant::now()).unwrap();
        assert_eq!(
            b.gap,
            Some(Gap::RecordsLost {
                cursor: Usn(0),
                oldest_readable: Usn(16)
            })
        );
        assert_eq!(b.cursor.usn, Usn(1000));
    }

    #[test]
    fn saved_cursor_from_another_journal_reports_a_gap_on_first_poll() {
        let start = Start::At(Cursor {
            journal: JournalId(99),
            usn: Usn(5),
        });
        let mut s = build(FakeJournal::new(), Setup { start, ..options() }).unwrap();
        assert!(matches!(
            s.poll(Instant::now()).unwrap().gap,
            Some(Gap::JournalRecreated { .. })
        ));
    }

    #[test]
    fn purge_racing_with_a_read_reports_a_gap_rather_than_an_error() {
        let mut j = FakeJournal::new();
        j.read_errors.push_back(Error::Gap(Gap::RecordsLost {
            cursor: Usn(0),
            oldest_readable: Usn(0),
        }));
        let b = stream(j).poll(Instant::now()).unwrap();
        assert!(matches!(b.gap, Some(Gap::RecordsLost { .. })));
    }

    #[test]
    fn other_errors_propagate_and_the_stream_can_continue() {
        let mut j = FakeJournal::new();
        add(&mut j, 20, ROOT, Reason::DATA_EXTEND.0 | CLOSE, 0, "a");
        j.read_errors.push_back(Error::JournalNotActive);
        let mut s = stream(j);
        assert!(matches!(
            s.poll(Instant::now()),
            Err(Error::JournalNotActive)
        ));
        assert_eq!(poll_settled(&mut s).changes.len(), 1);
    }

    #[test]
    fn corrupt_data_is_an_error_not_a_panic() {
        let mut j = FakeJournal::new();
        j.append(|usn| {
            let mut r = v2(usn, 20, ROOT, CLOSE, 0, "a");
            r[56..58].copy_from_slice(&9999u16.to_le_bytes());
            r
        });
        assert!(matches!(
            stream(j).poll(Instant::now()),
            Err(Error::Corrupt(_))
        ));
    }

    #[test]
    fn reports_lag_and_headroom() {
        let mut j = FakeJournal::new();
        j.info.first_usn = Usn(100);
        j.info.next_usn = Usn(1000);
        let start = Start::At(Cursor {
            journal: JournalId(7),
            usn: Usn(600),
        });
        let mut s = build(j, Setup { start, ..options() }).unwrap();
        let b = s.poll(Instant::now()).unwrap();
        assert_eq!(b.headroom_bytes, 500);
        assert_eq!(b.lag_bytes, 400);
    }

    const REPARSE: u32 = FileAttributes::REPARSE_POINT.0;

    fn reparse_change(attrs: u32, reason: Reason, tag: Option<ReparseTag>) -> FileChange {
        let mut j = FakeJournal::new();
        add(&mut j, 20, ROOT, reason.0 | CLOSE, attrs, "f");
        if let Some(tag) = tag {
            j.reparse_tags.insert(FileId(20), tag);
        }
        poll_settled(&mut stream(j)).changes.remove(0)
    }

    #[test]
    fn symlinks_are_links_and_are_not_copied() {
        let tag = ReparseTag::SYMLINK;
        let c = reparse_change(REPARSE, Reason::FILE_CREATE, Some(tag));
        assert_eq!(c.kind, ChangeKind::Created);
        assert!(c.is_reparse_point() && c.is_link());
        assert_eq!(c.reparse_tag, Some(tag));
        assert!(!c.has_new_content());
    }

    #[test]
    fn junctions_are_links() {
        let tag = ReparseTag::MOUNT_POINT;
        let c = reparse_change(REPARSE | DIR, Reason::FILE_CREATE, Some(tag));
        assert!(c.is_directory() && c.is_link());
        assert!(!c.has_new_content());
    }

    #[test]
    fn reparse_points_holding_real_data_are_still_copied() {
        // IO_REPARSE_TAG_CLOUD_6 (OneDrive) and IO_REPARSE_TAG_DEDUP.
        for tag in [ReparseTag(0x9000_601A), ReparseTag(0x8000_0013)] {
            let c = reparse_change(REPARSE, Reason::DATA_OVERWRITE, Some(tag));
            assert!(c.is_reparse_point() && !c.is_link(), "{tag:?}");
            assert!(c.has_new_content(), "{tag:?}");
        }
    }

    #[test]
    fn reparse_point_of_unknown_kind_is_still_copied() {
        let c = reparse_change(REPARSE, Reason::DATA_OVERWRITE, None);
        assert!(c.is_reparse_point() && !c.is_link());
        assert_eq!(c.reparse_tag, None);
        assert!(c.has_new_content());
    }

    #[test]
    fn tag_is_only_looked_up_for_reparse_points_that_still_exist() {
        let tag = Some(ReparseTag::SYMLINK);
        let plain = reparse_change(0, Reason::DATA_OVERWRITE, tag);
        assert_eq!(plain.reparse_tag, None);
        assert!(plain.has_new_content());

        let deleted = reparse_change(REPARSE, Reason::FILE_DELETE, tag);
        assert_eq!(deleted.kind, ChangeKind::Deleted);
        assert_eq!(deleted.reparse_tag, None);
    }

    fn included_stream(j: FakeJournal) -> Engine<FakeJournal> {
        let filter = PathFilter::new().include("keep");
        build(
            j,
            Setup {
                filter,
                ..options()
            },
        )
        .unwrap()
    }

    /// `keep` and `other` directories, created before the stream starts.
    fn with_keep_and_other() -> FakeJournal {
        let mut j = FakeJournal::new();
        add(&mut j, 30, ROOT, Reason::FILE_CREATE.0 | CLOSE, DIR, "keep");
        add(
            &mut j,
            31,
            ROOT,
            Reason::FILE_CREATE.0 | CLOSE,
            DIR,
            "other",
        );
        j
    }

    #[test]
    fn includes_limit_what_is_reported() {
        let mut j = with_keep_and_other();
        add(&mut j, 21, 30, Reason::DATA_EXTEND.0 | CLOSE, 0, "f");
        add(&mut j, 22, 31, Reason::DATA_EXTEND.0 | CLOSE, 0, "f");
        let b = poll_settled(&mut included_stream(j));
        let files: Vec<_> = b.changes.iter().filter(|c| !c.is_directory()).collect();
        assert_eq!(files.len(), 1);
        assert_eq!(files[0].path.as_deref(), Some(Path::new("C:\\keep\\f")));
    }

    #[test]
    fn move_into_an_included_path_counts_as_created() {
        let mut j = with_keep_and_other();
        add(&mut j, 21, 31, Reason::RENAME_OLD_NAME.0, 0, "f");
        add(&mut j, 21, 30, Reason::RENAME_NEW_NAME.0 | CLOSE, 0, "f");
        let b = poll_settled(&mut included_stream(j));
        let c = file_change(&b).unwrap();
        assert_eq!(c.kind, ChangeKind::Created);
        assert_eq!(c.previous_path, None);
        assert!(c.has_new_content());
    }

    #[test]
    fn move_out_of_an_included_path_counts_as_deleted() {
        let mut j = with_keep_and_other();
        add(&mut j, 21, 30, Reason::RENAME_OLD_NAME.0, 0, "f");
        add(&mut j, 21, 31, Reason::RENAME_NEW_NAME.0 | CLOSE, 0, "f");
        let b = poll_settled(&mut included_stream(j));
        let c = file_change(&b).unwrap();
        assert_eq!(c.kind, ChangeKind::Deleted);
        assert_eq!(c.path.as_deref(), Some(Path::new("C:\\keep\\f")));
    }

    #[test]
    fn renaming_a_parent_of_an_included_tree_is_noticed() {
        let mut j = FakeJournal::new();
        add(
            &mut j,
            30,
            ROOT,
            Reason::BASIC_INFO_CHANGE.0 | CLOSE,
            DIR,
            "parent",
        );
        let mut s = build(
            j,
            Setup {
                filter: PathFilter::new().include("parent\\keep"),
                ..options()
            },
        )
        .unwrap();
        poll_settled(&mut s);

        s.source
            .append(|usn| v2(usn, 30, ROOT, Reason::RENAME_OLD_NAME.0, DIR, "parent"));
        s.source.append(|usn| {
            v2(
                usn,
                30,
                ROOT,
                Reason::RENAME_NEW_NAME.0 | CLOSE,
                DIR,
                "moved",
            )
        });
        let b = poll_settled(&mut s);
        assert_eq!(b.changes.len(), 1);
        assert_eq!(b.changes[0].kind, ChangeKind::Deleted);
        assert_eq!(b.changes[0].path.as_deref(), Some(Path::new("C:\\parent")));
    }

    /// The journal's current end.
    fn end_of(s: &mut Engine<FakeJournal>) -> Cursor {
        s.source.info.end_cursor()
    }

    fn paths_in(batch: &ChangeBatch) -> Vec<&Path> {
        batch
            .changes
            .iter()
            .map(|c| c.path.as_deref().unwrap())
            .collect()
    }

    #[test]
    fn read_until_stops_at_the_position_and_leaves_the_rest_for_next_time() {
        let mut j = FakeJournal::new();
        add(&mut j, 20, ROOT, Reason::DATA_EXTEND.0 | CLOSE, 0, "before");
        let end = j.info.end_cursor();
        add(&mut j, 21, ROOT, Reason::DATA_EXTEND.0 | CLOSE, 0, "after");
        let mut s = stream(j);

        let first = s.read_to(end, Instant::now()).unwrap();
        assert_eq!(paths_in(&first), [Path::new("C:\\before")]);
        assert_eq!(first.cursor, end);
        assert!(first.lag_bytes > 0);

        let end = end_of(&mut s);
        let second = s.read_to(end, Instant::now()).unwrap();
        assert_eq!(paths_in(&second), [Path::new("C:\\after")]);
        assert_eq!(second.cursor, end);
        assert_eq!(second.lag_bytes, 0);
    }

    #[test]
    fn read_until_a_position_in_page_padding_skips_nothing() {
        let mut j = FakeJournal::new();
        add(&mut j, 20, ROOT, Reason::DATA_EXTEND.0 | CLOSE, 0, "before");
        let end = j.info.end_cursor();
        // Records don't cross a page, so the next one starts after some padding.
        j.info.next_usn.0 += 56;
        let after_usn = j.info.next_usn;
        add(&mut j, 21, ROOT, Reason::DATA_EXTEND.0 | CLOSE, 0, "after");
        let mut s = stream(j);

        let first = s.read_to(end, Instant::now()).unwrap();
        assert_eq!(paths_in(&first), [Path::new("C:\\before")]);
        assert_eq!(first.cursor.usn, after_usn);

        let end = end_of(&mut s);
        let second = s.read_to(end, Instant::now()).unwrap();
        assert_eq!(paths_in(&second), [Path::new("C:\\after")]);
    }

    #[test]
    fn read_until_does_not_wait_for_files_to_settle() {
        let mut j = FakeJournal::new();
        add(&mut j, 20, ROOT, Reason::DATA_EXTEND.0 | CLOSE, 0, "a");
        let mut s = stream(j);
        let end = end_of(&mut s);
        let batch = s.read_to(end, Instant::now()).unwrap();
        assert_eq!(batch.changes.len(), 1);
        assert!(!batch.changes[0].still_open);
    }

    #[test]
    fn read_until_reads_past_a_single_buffer() {
        let mut j = FakeJournal::new();
        let name = "n".repeat(100);
        let count = READ_BUFFER_BYTES * 3 / v2(0, 0, 0, 0, 0, &name).len();
        for file in 0..count {
            add(
                &mut j,
                100 + file as u64,
                ROOT,
                Reason::DATA_EXTEND.0 | CLOSE,
                0,
                &name,
            );
        }
        let mut s = stream(j);
        let end = end_of(&mut s);
        let batch = s.read_to(end, Instant::now()).unwrap();
        assert_eq!(batch.changes.len(), count);
        assert_eq!(batch.cursor, end);
    }

    #[test]
    fn read_until_reports_an_open_file_and_again_when_it_closes() {
        let mut j = FakeJournal::new();
        add(&mut j, 20, ROOT, Reason::DATA_EXTEND.0, 0, "open");
        let end = j.info.end_cursor();
        add(&mut j, 20, ROOT, Reason::DATA_EXTEND.0 | CLOSE, 0, "open");
        let mut s = stream(j);

        let first = s.read_to(end, Instant::now()).unwrap();
        assert!(first.changes[0].still_open);
        assert_eq!(first.cursor, end);

        let end = end_of(&mut s);
        let second = s.read_to(end, Instant::now()).unwrap();
        assert_eq!(second.changes.len(), 1);
        assert!(!second.changes[0].still_open);
        assert!(second.changes[0].has_new_content());
    }

    #[test]
    fn read_until_does_not_split_a_rename() {
        let mut j = FakeJournal::new();
        add(&mut j, 20, ROOT, Reason::DATA_EXTEND.0 | CLOSE, 0, "other");
        let rename_start = j.info.next_usn;
        add(&mut j, 21, ROOT, Reason::RENAME_OLD_NAME.0, 0, "old");
        let end = j.info.end_cursor();
        add(
            &mut j,
            21,
            ROOT,
            Reason::RENAME_NEW_NAME.0 | CLOSE,
            0,
            "new",
        );
        let mut s = stream(j);

        let first = s.read_to(end, Instant::now()).unwrap();
        assert_eq!(paths_in(&first), [Path::new("C:\\other")]);
        assert_eq!(first.cursor.usn, rename_start);

        let end = end_of(&mut s);
        let second = s.read_to(end, Instant::now()).unwrap();
        assert_eq!(paths_in(&second), [Path::new("C:\\new")]);
        assert_eq!(second.changes[0].kind, ChangeKind::Renamed);
        assert_eq!(
            second.changes[0].previous_path.as_deref(),
            Some(Path::new("C:\\old"))
        );
        assert_eq!(second.cursor, end);
    }

    #[test]
    fn a_saved_cursor_repeats_a_held_rename_after_a_restart() {
        let mut j = FakeJournal::new();
        add(&mut j, 21, ROOT, Reason::RENAME_OLD_NAME.0, 0, "old");
        let end = j.info.end_cursor();
        add(
            &mut j,
            21,
            ROOT,
            Reason::RENAME_NEW_NAME.0 | CLOSE,
            0,
            "new",
        );
        let mut s = stream(j);
        let cursor = s.read_to(end, Instant::now()).unwrap().cursor;
        let j = std::mem::replace(&mut s.source, FakeJournal::new());

        let mut s = build(
            j,
            Setup {
                start: Start::At(cursor),
                ..options()
            },
        )
        .unwrap();
        let end = end_of(&mut s);
        let batch = s.read_to(end, Instant::now()).unwrap();
        assert_eq!(batch.changes[0].kind, ChangeKind::Renamed);
        assert_eq!(
            batch.changes[0].previous_path.as_deref(),
            Some(Path::new("C:\\old"))
        );
    }

    #[test]
    fn read_until_a_position_past_the_journal_stops_at_its_end() {
        let mut j = FakeJournal::new();
        add(&mut j, 20, ROOT, Reason::DATA_EXTEND.0 | CLOSE, 0, "a");
        let end = j.info.end_cursor();
        let mut s = stream(j);
        let batch = s
            .read_to(
                Cursor {
                    journal: JournalId(7),
                    usn: Usn(end.usn.0 + 1000),
                },
                Instant::now(),
            )
            .unwrap();
        assert_eq!(batch.changes.len(), 1);
        assert_eq!(batch.cursor, end);
    }

    #[test]
    fn read_until_a_position_already_passed_does_nothing() {
        let mut j = FakeJournal::new();
        add(&mut j, 20, ROOT, Reason::DATA_EXTEND.0 | CLOSE, 0, "a");
        let earlier = Cursor {
            journal: JournalId(7),
            usn: Usn(0),
        };
        let mut s = stream(j);
        let end = end_of(&mut s);
        s.read_to(end, Instant::now()).unwrap();
        let batch = s.read_to(earlier, Instant::now()).unwrap();
        assert!(batch.changes.is_empty());
        assert_eq!(batch.cursor, end);
    }

    #[test]
    fn read_until_after_a_purge_reports_a_gap_and_keeps_the_position() {
        let mut j = FakeJournal::new();
        add(&mut j, 20, ROOT, Reason::DATA_EXTEND.0 | CLOSE, 0, "a");
        let end = j.info.end_cursor();
        add(&mut j, 21, ROOT, Reason::DATA_EXTEND.0 | CLOSE, 0, "later");
        let mut s = stream(j);
        s.source.info.first_usn = Usn(10);
        let batch = s.read_to(end, Instant::now()).unwrap();
        assert!(matches!(batch.gap, Some(Gap::RecordsLost { .. })));
        // Not the journal's end, or "later" would be lost.
        assert_eq!(batch.cursor, end);
    }

    #[test]
    fn read_until_a_purge_racing_with_the_read_reports_a_gap() {
        let mut j = FakeJournal::new();
        add(&mut j, 20, ROOT, Reason::DATA_EXTEND.0 | CLOSE, 0, "a");
        let end = j.info.end_cursor();
        j.read_errors.push_back(Error::Gap(Gap::RecordsLost {
            cursor: Usn(0),
            oldest_readable: Usn(0),
        }));
        let mut s = stream(j);
        let batch = s.read_to(end, Instant::now()).unwrap();
        assert!(matches!(batch.gap, Some(Gap::RecordsLost { .. })));
        assert_eq!(batch.cursor, end);
    }

    #[test]
    fn a_position_before_a_purged_start_keeps_reporting_a_gap() {
        let mut j = FakeJournal::new();
        add(&mut j, 20, ROOT, Reason::DATA_EXTEND.0 | CLOSE, 0, "a");
        let end = j.info.end_cursor();
        add(&mut j, 21, ROOT, Reason::DATA_EXTEND.0 | CLOSE, 0, "later");
        let mut s = stream(j);
        // Everything up to and past the position has gone.
        s.source.info.first_usn = Usn(end.usn.0 + 1);
        let first = s.read_to(end, Instant::now()).unwrap();
        assert!(first.gap.is_some());
        let again = s.read_to(end, Instant::now()).unwrap();
        assert!(again.gap.is_some());
    }

    #[test]
    fn read_until_a_position_in_a_recreated_journal_starts_it_from_the_beginning() {
        let mut j = FakeJournal::new();
        add(&mut j, 20, ROOT, Reason::DATA_EXTEND.0 | CLOSE, 0, "a");
        let end = j.info.end_cursor();
        let mut s = stream(j);
        s.source.info.id = JournalId(8);
        s.source.info.first_usn = Usn(50_000);
        s.source.info.next_usn = Usn(60_000);
        let batch = s.read_to(end, Instant::now()).unwrap();
        assert_eq!(
            batch.gap,
            Some(Gap::JournalRecreated {
                old: JournalId(7),
                new: JournalId(8)
            })
        );
        assert_eq!(
            batch.cursor,
            Cursor {
                journal: JournalId(8),
                usn: Usn(50_000)
            }
        );
    }

    #[test]
    fn read_until_a_new_position_after_a_recreated_journal_uses_it() {
        let mut j = FakeJournal::new();
        add(&mut j, 20, ROOT, Reason::DATA_EXTEND.0 | CLOSE, 0, "a");
        let mut s = stream(j);
        s.source.info.id = JournalId(8);
        s.source.info.first_usn = Usn(50_000);
        s.source.info.next_usn = Usn(60_000);
        let end = end_of(&mut s);
        let batch = s.read_to(end, Instant::now()).unwrap();
        assert!(matches!(batch.gap, Some(Gap::JournalRecreated { .. })));
        assert_eq!(batch.cursor, end);
    }

    #[test]
    fn a_file_system_that_cannot_list_itself_falls_back_to_lookups() {
        let mut j = FakeJournal::new();
        j.listing_unsupported = true;
        j.dirs.insert(FileId(99), PathBuf::from("C:\\found"));
        add(&mut j, 21, 99, Reason::DATA_EXTEND.0 | CLOSE, 0, "f");
        let c = &poll_settled(&mut stream(j)).changes[0];
        assert_eq!(c.path.as_deref(), Some(Path::new("C:\\found\\f")));
    }

    const CREATE: u32 = Reason::FILE_CREATE.0;
    const EXTEND: u32 = Reason::DATA_EXTEND.0;
    const DELETE: u32 = Reason::FILE_DELETE.0;

    #[test]
    fn a_file_reported_while_open_is_reported_again_when_deleted() {
        let mut j = FakeJournal::new();
        add(&mut j, 20, ROOT, CREATE, 0, "temp");
        add(&mut j, 20, ROOT, CREATE | EXTEND, 0, "temp");
        let mut s = stream(j);
        let end = end_of(&mut s);
        let first = s.read_to(end, Instant::now()).unwrap();
        assert_eq!(first.changes[0].kind, ChangeKind::Created);
        assert!(first.changes[0].still_open);

        // Reason flags accumulate until the file closes, so this still has
        // the create flag even though the file was reported long ago.
        s.source
            .append(|usn| v2(usn, 20, ROOT, CREATE | EXTEND | DELETE | CLOSE, 0, "temp"));
        let end = end_of(&mut s);
        let second = s.read_to(end, Instant::now()).unwrap();
        assert_eq!(second.changes.len(), 1, "{second:?}");
        assert_eq!(second.changes[0].kind, ChangeKind::Deleted);
        assert_eq!(
            second.changes[0].path.as_deref(),
            Some(Path::new("C:\\temp"))
        );
    }

    #[test]
    fn the_delete_is_still_reported_when_resuming_after_the_open_report() {
        let mut j = FakeJournal::new();
        add(&mut j, 20, ROOT, CREATE, 0, "temp");
        add(&mut j, 20, ROOT, CREATE | EXTEND, 0, "temp");
        let mut s = stream(j);
        let end = end_of(&mut s);
        let cursor = s.read_to(end, Instant::now()).unwrap().cursor;

        // A new process has no memory of having reported the file.
        let mut j = std::mem::replace(&mut s.source, FakeJournal::new());
        j.append(|usn| v2(usn, 20, ROOT, CREATE | EXTEND | DELETE | CLOSE, 0, "temp"));
        let mut s = build(
            j,
            Setup {
                start: Start::At(cursor),
                ..options()
            },
        )
        .unwrap();
        let end = end_of(&mut s);
        let batch = s.read_to(end, Instant::now()).unwrap();
        assert_eq!(batch.changes.len(), 1, "{batch:?}");
        assert_eq!(batch.changes[0].kind, ChangeKind::Deleted);
    }

    #[test]
    fn a_temporary_file_seen_whole_is_never_reported() {
        let mut j = FakeJournal::new();
        add(&mut j, 20, ROOT, CREATE, 0, "temp");
        add(&mut j, 20, ROOT, CREATE | EXTEND, 0, "temp");
        add(
            &mut j,
            20,
            ROOT,
            CREATE | EXTEND | DELETE | CLOSE,
            0,
            "temp",
        );
        let mut s = stream(j);
        let end = end_of(&mut s);
        assert!(s.read_to(end, Instant::now()).unwrap().changes.is_empty());
    }

    #[test]
    fn a_rename_of_a_file_reported_while_open_keeps_its_old_path() {
        let mut j = FakeJournal::new();
        add(&mut j, 20, ROOT, CREATE, 0, "first");
        let mut s = stream(j);
        let end = end_of(&mut s);
        assert!(s.read_to(end, Instant::now()).unwrap().changes[0].still_open);

        let old = CREATE | Reason::RENAME_OLD_NAME.0;
        let new = CREATE | Reason::RENAME_NEW_NAME.0 | CLOSE;
        s.source.append(|usn| v2(usn, 20, ROOT, old, 0, "first"));
        s.source.append(|usn| v2(usn, 20, ROOT, new, 0, "second"));
        let end = end_of(&mut s);
        let batch = s.read_to(end, Instant::now()).unwrap();
        let change = &batch.changes[0];
        assert_eq!(change.path.as_deref(), Some(Path::new("C:\\second")));
        assert_eq!(
            change.previous_path.as_deref(),
            Some(Path::new("C:\\first")),
            "a reader was told about the old name"
        );
    }

    #[test]
    fn a_gap_reloads_directories_renamed_in_the_lost_records() {
        let mut j = FakeJournal::new();
        j.listing = vec![(FileId(20), FileId(5), "old".into())];
        let mut s = stream(j);

        // The directory is renamed, and the journal wraps before that is read.
        s.source.listing = vec![(FileId(20), FileId(5), "new".into())];
        let end = Usn(s.source.info.next_usn.0 + 1_000_000);
        s.source.info.first_usn = end;
        s.source.info.next_usn = end;
        assert!(s.poll(Instant::now()).unwrap().gap.is_some());

        s.source
            .append(|usn| v2(usn, 21, 20, EXTEND | CLOSE, 0, "f"));
        let batch = poll_settled(&mut s);
        assert_eq!(
            batch.changes[0].path.as_deref(),
            Some(Path::new("C:\\new\\f"))
        );
    }

    #[test]
    fn a_gap_also_reloads_directories_when_reading_to_a_position() {
        let mut j = FakeJournal::new();
        j.listing = vec![(FileId(20), FileId(5), "old".into())];
        let mut s = stream(j);

        s.source.listing = vec![(FileId(20), FileId(5), "new".into())];
        s.source.info.id = JournalId(8);
        s.source.info.first_usn = Usn(50_000);
        s.source.info.next_usn = Usn(50_000);
        let end = end_of(&mut s);
        assert!(s.read_to(end, Instant::now()).unwrap().gap.is_some());

        s.source
            .append(|usn| v2(usn, 21, 20, EXTEND | CLOSE, 0, "f"));
        let end = end_of(&mut s);
        let batch = s.read_to(end, Instant::now()).unwrap();
        assert_eq!(
            batch.changes[0].path.as_deref(),
            Some(Path::new("C:\\new\\f"))
        );
    }

    #[test]
    fn a_gap_forgets_looked_up_directories_too() {
        let mut j = FakeJournal::new();
        j.dirs.insert(FileId(20), PathBuf::from("C:\\old"));
        add(&mut j, 21, 20, EXTEND | CLOSE, 0, "a");
        let mut s = build(
            j,
            Setup {
                prefill_paths: false,
                ..options()
            },
        )
        .unwrap();
        assert_eq!(
            poll_settled(&mut s).changes[0].path.as_deref(),
            Some(Path::new("C:\\old\\a"))
        );

        s.source.dirs.insert(FileId(20), PathBuf::from("C:\\new"));
        let end = Usn(s.source.info.next_usn.0 + 1_000_000);
        s.source.info.first_usn = end;
        s.source.info.next_usn = end;
        assert!(s.poll(Instant::now()).unwrap().gap.is_some());

        s.source
            .append(|usn| v2(usn, 21, 20, EXTEND | CLOSE, 0, "a"));
        assert_eq!(
            poll_settled(&mut s).changes[0].path.as_deref(),
            Some(Path::new("C:\\new\\a"))
        );
    }

    #[test]
    fn a_rename_from_an_unknown_place_is_reported_as_appearing() {
        let mut j = FakeJournal::new();
        // Nothing can say where parent 99 was, so the old path is unknown.
        add(&mut j, 21, 99, Reason::RENAME_OLD_NAME.0, 0, "old");
        add(
            &mut j,
            21,
            ROOT,
            Reason::RENAME_NEW_NAME.0 | CLOSE,
            0,
            "new",
        );
        let c = &poll_settled(&mut stream(j)).changes[0];
        assert_eq!(c.kind, ChangeKind::Created);
        assert_eq!(c.path.as_deref(), Some(Path::new("C:\\new")));
        assert_eq!(c.previous_path, None);
        assert!(c.has_new_content());
    }

    #[test]
    fn every_rename_reported_has_its_old_path() {
        let mut j = FakeJournal::new();
        add(&mut j, 21, ROOT, Reason::RENAME_OLD_NAME.0, 0, "a");
        add(&mut j, 21, ROOT, Reason::RENAME_NEW_NAME.0 | CLOSE, 0, "b");
        add(&mut j, 22, 99, Reason::RENAME_OLD_NAME.0, 0, "c");
        add(&mut j, 22, ROOT, Reason::RENAME_NEW_NAME.0 | CLOSE, 0, "d");
        add(&mut j, 23, ROOT, Reason::RENAME_NEW_NAME.0 | CLOSE, 0, "e");
        let batch = poll_settled(&mut stream(j));
        assert!(batch.changes.len() >= 3);
        for change in batch.changes {
            if change.kind == ChangeKind::Renamed {
                assert!(change.previous_path.is_some(), "{change:?}");
            }
        }
    }

    #[test]
    fn nothing_is_filtered_out_by_default() {
        let mut j = FakeJournal::new();
        add(&mut j, 21, ROOT, EXTEND | CLOSE, 0, "pagefile.sys");
        let mut s = Engine::new(
            j,
            Start::Oldest,
            ChangeOptions::new(),
            SettleOptions::new().settle(SETTLE),
        )
        .unwrap();
        let batch = poll_settled(&mut s);
        assert_eq!(
            batch.changes[0].path.as_deref(),
            Some(Path::new("C:\\pagefile.sys"))
        );
    }

    #[test]
    fn a_source_without_names_still_tracks_files_by_identity() {
        let mut j = FakeJournal::new();
        j.has_names = false;
        j.dirs.insert(FileId(20), PathBuf::from("C:\\known"));
        add(&mut j, 21, 20, Reason::RENAME_OLD_NAME.0, 0, "");
        add(&mut j, 21, 20, Reason::RENAME_NEW_NAME.0 | CLOSE, 0, "");
        let c = &poll_settled(&mut stream(j)).changes[0];
        assert_eq!(c.kind, ChangeKind::Renamed);
        assert_eq!(c.id, FileId(21));
        assert_eq!(c.path, None);
        assert_eq!(c.previous_path, None);
    }

    #[test]
    fn a_source_without_names_is_never_listed() {
        let mut j = FakeJournal::new();
        j.has_names = false;
        j.listing = vec![(FileId(21), FileId(5), "should not be used".into())];
        add(&mut j, 30, 21, EXTEND | CLOSE, 0, "");
        let c = &poll_settled(&mut stream(j)).changes[0];
        assert_eq!(c.path, None);
    }

    #[test]
    fn a_delete_of_a_file_in_a_filtered_directory_stays_hidden_after_an_open_report() {
        let mut j = with_skip_dir();
        add(&mut j, 21, 30, CREATE | EXTEND | DELETE | CLOSE, 0, "f");
        assert!(file_change(&poll_settled(&mut filtered_stream(j))).is_none());
    }
}
