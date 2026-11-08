use std::collections::HashMap;
use std::ffi::OsString;
use std::path::PathBuf;
use std::time::Instant;

use crate::options::SettleOptions;
use crate::paths::is_rename_old;
use crate::record::RawRecord;
use crate::types::{FileAttributes, FileId, FileTime, Reason, Usn};

/// The merged history of one file over a window of the journal.
#[derive(Clone, Debug)]
pub(crate) struct Settled {
    pub id: FileId,
    pub parent_id: FileId,
    pub name: OsString,
    pub reasons: Reason,
    pub attributes: FileAttributes,
    pub first_usn: Usn,
    pub last_usn: Usn,
    pub timestamp: FileTime,
    pub still_open: bool,
    /// A record in this window carried the create flag. The flag accumulates
    /// until the file closes, so this can be left over from an earlier window.
    pub created: bool,
    /// This window holds the file's very first record, so no reader can have
    /// been told about it before.
    pub born: bool,
    pub deleted: bool,
    /// The path a reader would know a deleted file by.
    pub deleted_path: Option<PathBuf>,
    /// The path before the first rename in this window, if it was renamed.
    pub rename_from: Option<PathBuf>,
}

struct Entry {
    settled: Settled,
    first_seen: Instant,
    last_seen: Instant,
    closed: bool,
    /// Its old-name record has been seen but not yet its new-name record.
    mid_rename: bool,
}

/// Merges the many records one file produces into a single change.
pub(crate) struct Coalescer {
    options: SettleOptions,
    pending: HashMap<FileId, Entry>,
}

impl Coalescer {
    pub fn new(options: SettleOptions) -> Coalescer {
        Coalescer {
            options,
            pending: HashMap::new(),
        }
    }

    /// Adds a record. `path_before` is the file's path before this record and
    /// is only used for rename-old and delete records, where it can no longer
    /// be worked out later.
    pub fn push(&mut self, record: &RawRecord<'_>, path_before: Option<PathBuf>, now: Instant) {
        let entry = self.pending.entry(record.id).or_insert_with(|| Entry {
            settled: Settled {
                id: record.id,
                parent_id: record.parent_id,
                name: record.name(),
                reasons: Reason::default(),
                attributes: record.attributes,
                first_usn: record.usn,
                last_usn: record.usn,
                timestamp: record.timestamp,
                still_open: false,
                created: false,
                // A file's first record carries the create flag alone. Any later
                // record that still has it also has the flags added since.
                born: record.reason == Reason::FILE_CREATE,
                deleted: false,
                deleted_path: None,
                rename_from: None,
            },
            first_seen: now,
            last_seen: now,
            closed: false,
            mid_rename: false,
        });
        let s = &mut entry.settled;
        s.reasons |= record.reason;
        s.attributes = record.attributes;
        s.last_usn = record.usn;
        s.timestamp = record.timestamp;
        entry.last_seen = now;
        entry.closed = record.reason.contains(Reason::CLOSE);

        if record.reason.contains(Reason::FILE_CREATE) {
            s.created = true;
        }
        if record.reason.contains(Reason::RENAME_NEW_NAME) {
            s.name = record.name();
            s.parent_id = record.parent_id;
            entry.mid_rename = false;
        }
        if is_rename_old(record.reason) {
            entry.mid_rename = true;
            if s.rename_from.is_none() {
                s.rename_from = path_before.clone();
            }
        }
        if record.reason.contains(Reason::FILE_DELETE) {
            s.deleted = true;
            s.deleted_path = path_before;
        }
    }

    /// The earliest USN not yet reported. Resuming from here loses nothing.
    pub fn oldest_usn(&self) -> Option<Usn> {
        self.pending.values().map(|e| e.settled.first_usn).min()
    }

    /// Removes and returns the changes that are ready to report.
    pub fn drain_ready(&mut self, now: Instant) -> Vec<Settled> {
        if self.pending.len() > self.options.max_pending {
            return self.drain_complete();
        }
        let options = self.options;
        let mut ready = Vec::new();
        self.pending.retain(|_, e| {
            let is_ready = if e.mid_rename {
                // The other half is on its way, so only give up on it if it
                // never arrives.
                now.duration_since(e.last_seen) >= options.max_open_delay
            } else if e.closed {
                now.duration_since(e.last_seen) >= options.settle
            } else {
                now.duration_since(e.first_seen) >= options.max_open_delay
            };
            if is_ready {
                ready.extend(finish(e));
            }
            !is_ready
        });
        order(&mut ready);
        ready
    }

    /// Removes and returns everything without waiting to settle, except files
    /// caught halfway through a rename. Their two records can fall either side of
    /// a chosen position, and reporting only the first would lose the link between
    /// the old and new names. Holding them also holds the cursor before them.
    pub fn drain_complete(&mut self) -> Vec<Settled> {
        let mut ready = Vec::new();
        self.pending.retain(|_, e| {
            if !e.mid_rename {
                ready.extend(finish(e));
            }
            e.mid_rename
        });
        order(&mut ready);
        ready
    }

    /// Removes and returns everything, whether settled or not.
    pub fn flush(&mut self) -> Vec<Settled> {
        let mut ready: Vec<_> = self
            .pending
            .drain()
            .filter_map(|(_, e)| finish(&e))
            .collect();
        order(&mut ready);
        ready
    }
}

/// Applies the rules that need the whole window. Returns `None` if there is
/// nothing to report.
fn finish(e: &Entry) -> Option<Settled> {
    let mut s = e.settled.clone();
    s.still_open = !e.closed;
    if s.born && s.deleted {
        // Born and gone within the window, so no reader needs to know. A file
        // reported earlier while open is not `born` here, so its delete is kept.
        return None;
    }
    if s.reasons == Reason::CLOSE {
        return None;
    }
    if s.born || e.mid_rename {
        // No reader saw the old name of a file created in this window, which is
        // the common "write a temp file, rename it into place" pattern. A rename
        // that never finished has no new name to report.
        s.rename_from = None;
    }
    if s.deleted {
        // A reader knows this file by its name before any renames.
        if let Some(original) = s.rename_from.take() {
            s.deleted_path = Some(original);
        }
    }
    Some(s)
}

/// Deletes go first so that when one file replaces another at the same path,
/// applying a batch in order leaves the new file in place.
fn order(changes: &mut [Settled]) {
    changes.sort_by_key(|s| (!s.deleted, s.last_usn));
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::record::split_buffer;
    use crate::testing::{buffer, v2};
    use std::time::Duration;

    const SETTLE: Duration = Duration::from_secs(2);

    fn coalescer() -> Coalescer {
        Coalescer::new(SettleOptions {
            settle: SETTLE,
            max_open_delay: Duration::from_secs(60),
            max_pending: 100,
        })
    }

    fn push_path(
        c: &mut Coalescer,
        usn: i64,
        file: u64,
        reason: Reason,
        name: &str,
        path: Option<&str>,
        now: Instant,
    ) {
        let b = buffer(0, &[v2(usn, file, 5, reason.0, 0, name)]);
        let rec = split_buffer(&b).unwrap().1.next().unwrap().unwrap();
        c.push(&rec, path.map(PathBuf::from), now);
    }

    fn push(c: &mut Coalescer, usn: i64, file: u64, reason: Reason, name: &str, now: Instant) {
        push_path(c, usn, file, reason, name, None, now);
    }

    #[test]
    fn merges_all_records_for_a_file_into_one_change() {
        let mut c = coalescer();
        let t = Instant::now();
        push(&mut c, 10, 1, Reason::DATA_OVERWRITE, "f", t);
        push(&mut c, 20, 1, Reason::DATA_EXTEND, "f", t);
        push(&mut c, 30, 1, Reason::DATA_EXTEND | Reason::CLOSE, "f", t);
        let out = c.drain_ready(t + SETTLE);
        assert_eq!(out.len(), 1);
        assert!(
            out[0]
                .reasons
                .contains(Reason::DATA_OVERWRITE | Reason::DATA_EXTEND)
        );
        assert_eq!((out[0].first_usn, out[0].last_usn), (Usn(10), Usn(30)));
        assert!(!out[0].still_open);
        assert!(c.pending.is_empty());
    }

    #[test]
    fn drain_complete_holds_a_file_halfway_through_a_rename() {
        let mut c = coalescer();
        let t = Instant::now();
        push(&mut c, 10, 1, Reason::DATA_EXTEND, "done", t);
        push_path(
            &mut c,
            20,
            2,
            Reason::RENAME_OLD_NAME,
            "old",
            Some("C:\\old"),
            t,
        );
        let out = c.drain_complete();
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].last_usn, Usn(10));
        assert_eq!(c.oldest_usn(), Some(Usn(20)));

        push(&mut c, 30, 2, Reason::RENAME_NEW_NAME, "new", t);
        let out = c.drain_complete();
        assert_eq!(out.len(), 1);
        assert_eq!(
            out[0].rename_from.as_deref(),
            Some(std::path::Path::new("C:\\old"))
        );
        assert_eq!(c.oldest_usn(), None);
    }

    #[test]
    fn closed_file_waits_for_the_settle_period() {
        let mut c = coalescer();
        let t = Instant::now();
        push(&mut c, 10, 1, Reason::DATA_EXTEND | Reason::CLOSE, "f", t);
        assert!(
            c.drain_ready(t + SETTLE - Duration::from_millis(1))
                .is_empty()
        );
        assert_eq!(c.drain_ready(t + SETTLE).len(), 1);
    }

    #[test]
    fn new_activity_restarts_the_settle_period() {
        let mut c = coalescer();
        let t = Instant::now();
        push(&mut c, 10, 1, Reason::DATA_EXTEND | Reason::CLOSE, "f", t);
        let later = t + Duration::from_secs(1);
        push(
            &mut c,
            20,
            1,
            Reason::DATA_OVERWRITE | Reason::CLOSE,
            "f",
            later,
        );
        assert!(c.drain_ready(t + SETTLE).is_empty());
        assert_eq!(c.drain_ready(later + SETTLE).len(), 1);
    }

    #[test]
    fn reopened_file_is_not_treated_as_closed() {
        let mut c = coalescer();
        let t = Instant::now();
        push(&mut c, 10, 1, Reason::DATA_EXTEND | Reason::CLOSE, "f", t);
        push(&mut c, 20, 1, Reason::DATA_OVERWRITE, "f", t);
        assert!(c.drain_ready(t + SETTLE).is_empty());
    }

    #[test]
    fn open_file_is_held_until_max_delay_then_reported_as_still_open() {
        let mut c = coalescer();
        let t = Instant::now();
        push(&mut c, 10, 1, Reason::DATA_EXTEND, "log", t);
        assert!(c.drain_ready(t + Duration::from_secs(59)).is_empty());
        let out = c.drain_ready(t + Duration::from_secs(60));
        assert_eq!(out.len(), 1);
        assert!(out[0].still_open);

        // The record written when it finally closes must report it again.
        let later = t + Duration::from_secs(120);
        push(
            &mut c,
            20,
            1,
            Reason::DATA_EXTEND | Reason::CLOSE,
            "log",
            later,
        );
        let out = c.drain_ready(later + SETTLE);
        assert_eq!(out.len(), 1);
        assert!(!out[0].still_open);
    }

    #[test]
    fn file_created_and_deleted_in_the_window_is_dropped() {
        let mut c = coalescer();
        let t = Instant::now();
        push(&mut c, 10, 1, Reason::FILE_CREATE, "tmp", t);
        push(&mut c, 20, 1, Reason::DATA_EXTEND, "tmp", t);
        push(&mut c, 30, 1, Reason::FILE_DELETE | Reason::CLOSE, "tmp", t);
        assert!(c.drain_ready(t + SETTLE).is_empty());
        assert!(c.pending.is_empty());
    }

    #[test]
    fn delete_of_a_pre_existing_file_is_reported_with_its_path() {
        let mut c = coalescer();
        let t = Instant::now();
        push(&mut c, 10, 1, Reason::DATA_EXTEND, "f", t);
        push_path(
            &mut c,
            20,
            1,
            Reason::FILE_DELETE | Reason::CLOSE,
            "f",
            Some("C:\\f"),
            t,
        );
        let out = c.drain_ready(t + SETTLE);
        assert!(out[0].deleted);
        assert_eq!(out[0].deleted_path, Some(PathBuf::from("C:\\f")));
    }

    #[test]
    fn rename_keeps_the_original_path_and_the_final_name() {
        let mut c = coalescer();
        let t = Instant::now();
        push_path(
            &mut c,
            10,
            1,
            Reason::RENAME_OLD_NAME,
            "a",
            Some("C:\\a"),
            t,
        );
        push(&mut c, 20, 1, Reason::RENAME_NEW_NAME, "b", t);
        push_path(
            &mut c,
            30,
            1,
            Reason::RENAME_OLD_NAME,
            "b",
            Some("C:\\b"),
            t,
        );
        push(
            &mut c,
            40,
            1,
            Reason::RENAME_NEW_NAME | Reason::CLOSE,
            "c",
            t,
        );
        let out = c.drain_ready(t + SETTLE);
        assert_eq!(out[0].rename_from, Some(PathBuf::from("C:\\a")));
        assert_eq!(out[0].name, OsString::from("c"));
    }

    #[test]
    fn created_then_renamed_has_no_rename_source() {
        let mut c = coalescer();
        let t = Instant::now();
        push(&mut c, 10, 1, Reason::FILE_CREATE, "tmp", t);
        push_path(
            &mut c,
            20,
            1,
            Reason::RENAME_OLD_NAME,
            "tmp",
            Some("C:\\tmp"),
            t,
        );
        push(
            &mut c,
            30,
            1,
            Reason::RENAME_NEW_NAME | Reason::CLOSE,
            "doc",
            t,
        );
        let out = c.drain_ready(t + SETTLE);
        assert!(out[0].created);
        assert_eq!(out[0].rename_from, None);
        assert_eq!(out[0].name, OsString::from("doc"));
    }

    #[test]
    fn renamed_then_deleted_reports_the_original_path() {
        let mut c = coalescer();
        let t = Instant::now();
        push_path(
            &mut c,
            10,
            1,
            Reason::RENAME_OLD_NAME,
            "a",
            Some("C:\\a"),
            t,
        );
        push(&mut c, 20, 1, Reason::RENAME_NEW_NAME, "b", t);
        push_path(
            &mut c,
            30,
            1,
            Reason::FILE_DELETE | Reason::CLOSE,
            "b",
            Some("C:\\b"),
            t,
        );
        let out = c.drain_ready(t + SETTLE);
        assert!(out[0].deleted);
        assert_eq!(out[0].deleted_path, Some(PathBuf::from("C:\\a")));
        assert_eq!(out[0].rename_from, None);
    }

    #[test]
    fn batch_puts_deletes_first_then_orders_by_usn() {
        let mut c = coalescer();
        let t = Instant::now();
        push(&mut c, 30, 1, Reason::DATA_EXTEND | Reason::CLOSE, "x", t);
        push(&mut c, 10, 2, Reason::DATA_EXTEND | Reason::CLOSE, "y", t);
        push_path(
            &mut c,
            40,
            3,
            Reason::FILE_DELETE | Reason::CLOSE,
            "z",
            Some("C:\\z"),
            t,
        );
        let usns: Vec<_> = c
            .drain_ready(t + SETTLE)
            .iter()
            .map(|s| s.last_usn)
            .collect();
        assert_eq!(usns, [Usn(40), Usn(10), Usn(30)]);
    }

    #[test]
    fn oldest_usn_tracks_the_first_record_still_pending() {
        let mut c = coalescer();
        let t = Instant::now();
        assert_eq!(c.oldest_usn(), None);
        push(&mut c, 30, 1, Reason::DATA_EXTEND | Reason::CLOSE, "x", t);
        push(&mut c, 40, 1, Reason::DATA_EXTEND, "x", t);
        push(&mut c, 50, 2, Reason::DATA_EXTEND | Reason::CLOSE, "y", t);
        assert_eq!(c.oldest_usn(), Some(Usn(30)));
    }

    #[test]
    fn exceeding_max_pending_flushes_everything() {
        let mut c = Coalescer::new(SettleOptions {
            max_pending: 2,
            ..SettleOptions::default()
        });
        let t = Instant::now();
        for file in 1..=3 {
            push(&mut c, file as i64, file, Reason::DATA_EXTEND, "f", t);
        }
        let out = c.drain_ready(t);
        assert_eq!(out.len(), 3);
        assert!(out.iter().all(|s| s.still_open));
        assert!(c.pending.is_empty());
    }

    #[test]
    fn close_only_record_is_dropped() {
        let mut c = coalescer();
        let t = Instant::now();
        push(&mut c, 10, 1, Reason::CLOSE, "f", t);
        assert!(c.drain_ready(t + SETTLE).is_empty());
    }

    #[test]
    fn a_file_is_born_only_if_its_first_record_is_the_bare_create() {
        let mut c = coalescer();
        let t = Instant::now();
        push(&mut c, 10, 1, Reason::FILE_CREATE, "a", t);
        push(
            &mut c,
            20,
            2,
            Reason::FILE_CREATE | Reason::DATA_EXTEND,
            "b",
            t,
        );
        push(&mut c, 30, 3, Reason::FILE_CREATE | Reason::CLOSE, "c", t);
        let out = c.flush();
        assert!(out[0].born);
        assert!(!out[1].born);
        assert!(!out[2].born);
    }

    #[test]
    fn a_leftover_create_flag_does_not_hide_a_delete() {
        let mut c = coalescer();
        let t = Instant::now();
        let flags = Reason::FILE_CREATE | Reason::DATA_EXTEND | Reason::FILE_DELETE | Reason::CLOSE;
        push_path(&mut c, 30, 1, flags, "f", Some("C:\\f"), t);
        let out = c.drain_ready(t + SETTLE);
        assert_eq!(out.len(), 1);
        assert!(out[0].deleted);
    }

    #[test]
    fn half_a_rename_waits_longer_than_the_settle_period() {
        let mut c = coalescer();
        let t = Instant::now();
        push(&mut c, 5, 1, Reason::DATA_EXTEND, "f", t);
        let later = t + Duration::from_secs(59);
        push_path(
            &mut c,
            10,
            1,
            Reason::RENAME_OLD_NAME,
            "f",
            Some("C:\\f"),
            later,
        );
        // The first record alone would have made this due at 60 seconds.
        assert!(c.drain_ready(t + Duration::from_secs(61)).is_empty());
        let out = c.drain_ready(later + Duration::from_secs(60));
        assert_eq!(out.len(), 1);
    }

    #[test]
    fn a_full_coalescer_keeps_half_a_rename_back() {
        let mut c = Coalescer::new(SettleOptions {
            max_pending: 1,
            ..SettleOptions::default()
        });
        let t = Instant::now();
        push(&mut c, 10, 1, Reason::DATA_EXTEND, "f", t);
        push_path(
            &mut c,
            20,
            2,
            Reason::RENAME_OLD_NAME,
            "g",
            Some("C:\\g"),
            t,
        );
        let out = c.drain_ready(t);
        assert_eq!(out.len(), 1);
        assert_eq!(c.oldest_usn(), Some(Usn(20)));
    }

    #[test]
    fn a_rename_that_never_finished_has_no_source_path() {
        let mut c = coalescer();
        let t = Instant::now();
        push_path(
            &mut c,
            10,
            1,
            Reason::RENAME_OLD_NAME,
            "f",
            Some("C:\\f"),
            t,
        );
        let out = c.flush();
        assert_eq!(out[0].rename_from, None);
    }
}
