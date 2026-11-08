//! Runs the parser and change pipeline over real kernel data with no elevation.
//!
//! A volume opened this way serves real records with the file names removed,
//! which proves the record layouts, reason flags and merging against what
//! Windows really produces. Files are identified by their ID instead of by path.
//! Paths and the master file table are covered by `tests/elevated.rs`.
//!
//! The volume holding the temp directory must have a journal, and a test fails
//! if it doesn't. Tests that create symbolic links are ignored unless asked for,
//! because they need a privilege that an ordinary account doesn't have.
#![cfg(windows)]

mod common;

use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::os::windows::fs::{symlink_dir, symlink_file};
use std::path::PathBuf;
use std::time::{Duration, Instant};

use common::{TempDir, append_file, id_of, id_of_link, make_junction, write_file};
use usn_journal::{
    ChangeBatch, ChangeKind, ChangeOptions, ChangeReader, ChangeStream, Cursor, Error, FileChange,
    FileId, Gap, HealthOptions, JournalId, PathFilter, ReadOptions, Reason, ReparseTag,
    SettleOptions, Start, Usn, Volume, WatchEvent, WatchOptions,
};

const TIMEOUT: Duration = Duration::from_secs(30);

fn open() -> Volume {
    Volume::open_unprivileged(std::env::temp_dir()).unwrap()
}

fn stream(volume: Volume, start: Start) -> ChangeStream {
    let settle = SettleOptions::new().settle(Duration::from_millis(100));
    ChangeStream::new(volume, start, ChangeOptions::new(), settle).unwrap()
}

fn reader(volume: Volume, start: Start) -> ChangeReader {
    ChangeReader::new(volume, start, ChangeOptions::new()).unwrap()
}

/// Polls until a change to `id` is reported.
fn wait_for(stream: &mut ChangeStream, id: FileId) -> Vec<FileChange> {
    let deadline = Instant::now() + TIMEOUT;
    let mut seen = Vec::new();
    while Instant::now() < deadline {
        let batch = stream.poll().unwrap();
        assert_eq!(batch.gap, None);
        seen.extend(batch.changes.into_iter().filter(|c| c.id == id));
        if !seen.is_empty() {
            return seen;
        }
        std::thread::sleep(Duration::from_millis(25));
    }
    panic!("no change reported for {id:?} within {TIMEOUT:?}");
}

/// Polls until a change to `id` satisfies `done`.
fn wait_until(
    stream: &mut ChangeStream,
    id: FileId,
    done: impl Fn(&FileChange) -> bool,
) -> FileChange {
    let deadline = Instant::now() + TIMEOUT;
    let mut last = None;
    while Instant::now() < deadline {
        for change in stream.poll().unwrap().changes {
            if change.id == id {
                if done(&change) {
                    return change;
                }
                last = Some(change);
            }
        }
        std::thread::sleep(Duration::from_millis(25));
    }
    panic!("no matching change for {id:?} within {TIMEOUT:?}; last was {last:#?}");
}

fn has(batch: &ChangeBatch, id: FileId) -> bool {
    batch.changes.iter().any(|c| c.id == id)
}

// ------------------------------------------------------------------ journal state

#[test]
fn query_parses_the_real_journal_state() {
    let info = open().journal_info().unwrap();
    assert_ne!(info.id.0, 0);
    assert!(
        info.first_usn.0 >= 0 && info.first_usn <= info.next_usn,
        "{info:?}"
    );
    assert!(info.next_usn < info.max_usn, "{info:?}");
    assert!(info.maximum_size >= 1024 * 1024, "{info:?}");
    assert_eq!(info.min_version, 2, "{info:?}");
    assert!(info.max_version >= 3, "{info:?}");
}

#[test]
fn opening_any_path_finds_the_root_of_its_volume() {
    let dir = TempDir::new();
    let volume = Volume::open_unprivileged(&dir.0).unwrap();
    assert_eq!(
        volume.root(),
        PathBuf::from(format!("{}\\", common::temp_drive())).as_path()
    );
}

#[test]
fn health_measures_the_real_journal() {
    let health = open().health(&HealthOptions::new()).unwrap();
    let oldest = health.oldest_record.expect("a real journal has records");
    assert!(oldest.0 > 0);
    // NTFS was released in 1993, so a real journal can't reach back further.
    let history = health.history.expect("the oldest record is in the past");
    assert!(
        history < Duration::from_secs(40 * 365 * 24 * 3600),
        "{history:?}"
    );
    // Allows for records written after the check took its time.
    assert!(
        health.active_history <= history + Duration::from_secs(60),
        "active {:?} of {history:?}",
        health.active_history
    );
}

#[test]
fn health_checks_a_real_cursor() {
    let volume = open();
    let cursor = volume.journal_info().unwrap().end_cursor();
    let health = volume.health(&HealthOptions::new().cursor(cursor)).unwrap();
    let state = health.cursor.unwrap();
    assert_eq!(state.gap, None);
    assert!(state.bytes_until_lost > 0);
}

// ------------------------------------------------------------------ raw records

#[test]
fn every_real_record_parses_with_plausible_fields() {
    let volume = open();
    let mut last_usn = Usn(-1);
    let mut count = 0u64;
    for record in volume.records(ReadOptions::new(Start::Oldest)).unwrap() {
        let r = match record {
            Ok(r) => r,
            // A busy machine can purge the start of the journal mid-walk.
            Err(Error::Gap(_)) => break,
            Err(e) => panic!("read failed: {e}"),
        };
        count += 1;
        assert!(matches!(r.version, 2 | 3), "{r:?}");
        assert!(
            r.usn > last_usn,
            "USNs must increase: {last_usn} then {r:?}"
        );
        last_usn = r.usn;
        assert_ne!(r.id.0, 0);
        assert_ne!(r.parent_id.0, 0);
        assert_ne!(r.reason.0, 0);
        assert_eq!(r.name, "", "names are removed without elevation");
        // 2015 to 2100: only true if the timestamp offset is right.
        let since_epoch = r
            .timestamp
            .to_system_time()
            .unwrap()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs();
        assert!(
            (1_420_070_400..4_102_444_800).contains(&since_epoch),
            "timestamp {since_epoch}"
        );
    }
    assert!(count > 0, "the journal should not be empty");
}

/// A file's first record carries the create flag alone. Telling a new file from
/// one already reported while open depends on this, so it is checked for each
/// common way of making a file.
#[test]
fn every_new_file_starts_with_a_bare_create_record() {
    let volume = open();
    let dir = TempDir::new();
    let start = volume.journal_info().unwrap().end_cursor();

    let source = dir.file("created.txt");
    write_file(&source, b"contents");
    let copied = dir.file("copied.txt");
    fs::copy(&source, &copied).unwrap();
    let written = dir.file("written.txt");
    fs::write(&written, b"contents").unwrap();
    let directory = dir.file("directory");
    fs::create_dir(&directory).unwrap();

    let ids = [
        id_of(&source),
        id_of(&copied),
        id_of(&written),
        id_of(&directory),
    ];
    let records: Vec<_> = volume
        .records(ReadOptions::new(Start::At(start)))
        .unwrap()
        .map(Result::unwrap)
        .collect();
    for id in ids {
        let first = records
            .iter()
            .find(|r| r.id == id)
            .expect("the file should have a record");
        assert_eq!(first.reason, Reason::FILE_CREATE, "{first:?}");
    }
}

#[test]
fn a_reason_filter_returns_only_those_reasons() {
    let volume = open();
    let dir = TempDir::new();
    let start = volume.journal_info().unwrap().end_cursor();
    let path = dir.file("filtered.txt");
    write_file(&path, b"contents");
    let id = id_of(&path);

    let options = ReadOptions::new(Start::At(start)).reasons(Reason::DATA_EXTEND);
    let records: Vec<_> = volume
        .records(options)
        .unwrap()
        .map(Result::unwrap)
        .collect();
    assert!(records.iter().any(|r| r.id == id), "{records:#?}");
    for r in records {
        assert!(r.reason.intersects(Reason::DATA_EXTEND), "{r:?}");
    }
}

#[test]
fn only_on_close_returns_the_final_record_of_each_file() {
    let volume = open();
    let dir = TempDir::new();
    let start = volume.journal_info().unwrap().end_cursor();
    let path = dir.file("closed.txt");
    write_file(&path, b"contents");
    let id = id_of(&path);

    let options = ReadOptions::new(Start::At(start)).only_on_close(true);
    let records: Vec<_> = volume
        .records(options)
        .unwrap()
        .map(Result::unwrap)
        .filter(|r| r.id == id)
        .collect();
    assert_eq!(records.len(), 1, "{records:#?}");
    assert!(records[0].reason.contains(Reason::CLOSE));
}

#[test]
fn a_position_resumes_without_skipping_or_repeating() {
    let volume = open();
    let dir = TempDir::new();
    let start = volume.journal_info().unwrap().end_cursor();
    for n in 0..5 {
        write_file(&dir.file(&format!("{n}.txt")), b"contents");
    }

    let everything: Vec<_> = volume
        .records(ReadOptions::new(Start::At(start)))
        .unwrap()
        .map(|r| r.unwrap().usn)
        .collect();
    assert!(everything.len() > 5);

    let mut records = volume.records(ReadOptions::new(Start::At(start))).unwrap();
    let first_half: Vec<_> = records
        .by_ref()
        .take(everything.len() / 2)
        .map(|r| r.unwrap().usn)
        .collect();
    let resume = records.position();
    drop(records);
    let second_half: Vec<_> = volume
        .records(ReadOptions::new(Start::At(resume)))
        .unwrap()
        .map(|r| r.unwrap().usn)
        .collect();

    let joined = [first_half, second_half].concat();
    assert_eq!(joined[..everything.len()], everything[..]);
}

#[test]
fn iteration_ends_when_it_catches_up() {
    let volume = open();
    let started = Instant::now();
    let count = volume
        .records(ReadOptions::new(Start::Now))
        .unwrap()
        .count();
    assert!(
        started.elapsed() < Duration::from_secs(5),
        "{count} records"
    );
}

#[test]
fn a_wait_ends_once_its_timeout_passes() {
    let volume = open();
    // More bytes than will ever arrive, so only the timeout can end this. The
    // reason filter means other activity on the volume returns nothing.
    let options = ReadOptions::new(Start::Now)
        .reasons(Reason::TRANSACTED_CHANGE)
        .wait(1 << 40, Duration::from_millis(500));
    let started = Instant::now();
    let count = volume.records(options).unwrap().count();
    let elapsed = started.elapsed();
    assert_eq!(count, 0);
    assert!(
        elapsed >= Duration::from_millis(400) && elapsed < Duration::from_secs(10),
        "{elapsed:?}"
    );
}

#[test]
fn a_wait_for_more_than_will_arrive_ends_on_time() {
    let volume = open();
    let options = ReadOptions::new(Start::Now).wait(1 << 40, Duration::from_millis(300));
    let started = Instant::now();
    assert_eq!(volume.records(options).unwrap().count(), 0);
    let elapsed = started.elapsed();
    assert!(
        elapsed >= Duration::from_millis(250) && elapsed < Duration::from_secs(5),
        "{elapsed:?}"
    );
}

#[test]
fn a_wait_returns_as_soon_as_records_arrive() {
    let volume = open();
    let dir = TempDir::new();
    let path = dir.file("arrives.txt");
    let writer = {
        let path = path.clone();
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(200));
            write_file(&path, b"contents");
        })
    };
    let options = ReadOptions::new(Start::Now).wait(1, Duration::from_secs(20));
    let started = Instant::now();
    let first = volume.records(options).unwrap().next();
    assert!(matches!(first, Some(Ok(_))), "{first:?}");
    assert!(started.elapsed() < Duration::from_secs(15));
    writer.join().unwrap();
}

#[test]
fn reading_from_another_journal_is_a_gap_error() {
    let volume = open();
    let id = volume.journal_info().unwrap().id;
    let stale = Cursor {
        journal: JournalId(id.0.wrapping_add(1)),
        usn: Usn(0),
    };
    let result = volume.records(ReadOptions::new(Start::At(stale)));
    assert!(matches!(
        result.err(),
        Some(Error::Gap(Gap::JournalRecreated { .. }))
    ));
}

#[test]
fn listing_the_volume_needs_elevation() {
    let volume = open();
    assert!(matches!(volume.mft_entries(), Err(Error::AccessDenied)));
    assert!(matches!(
        volume.scan(&PathFilter::new()),
        Err(Error::AccessDenied)
    ));
}

// ------------------------------------------------------------------ lookups by identity

#[test]
fn path_of_finds_a_file_by_its_identity() {
    let volume = open();
    let dir = TempDir::new();
    let path = dir.file("found.txt");
    write_file(&path, b"contents");
    assert_eq!(volume.path_of(id_of(&path)).unwrap(), path);
}

#[test]
fn path_of_a_link_is_the_link_itself() {
    let volume = open();
    let dir = TempDir::new();
    let target = dir.file("target");
    fs::create_dir(&target).unwrap();
    let link = dir.file("junction");
    make_junction(&link, &target);
    assert_eq!(volume.path_of(id_of_link(&link)).unwrap(), link);
}

#[test]
fn reparse_tag_is_none_for_a_plain_file() {
    let volume = open();
    let dir = TempDir::new();
    let path = dir.file("plain.txt");
    write_file(&path, b"contents");
    assert_eq!(volume.reparse_tag(id_of(&path)).unwrap(), None);
}

#[test]
fn reparse_tag_identifies_a_junction() {
    let volume = open();
    let dir = TempDir::new();
    let target = dir.file("target");
    fs::create_dir(&target).unwrap();
    let link = dir.file("junction");
    make_junction(&link, &target);
    assert_eq!(
        volume.reparse_tag(id_of_link(&link)).unwrap(),
        Some(ReparseTag::MOUNT_POINT)
    );
}

// ------------------------------------------------------------------ merged changes

#[test]
fn reports_the_real_lifecycle_of_a_file() {
    let dir = TempDir::new();
    let mut s = stream(open(), Start::Now);

    let path = dir.file("lifecycle.txt");
    write_file(&path, b"hello");
    let id = id_of(&path);
    let created = wait_for(&mut s, id);
    assert_eq!(created[0].kind, ChangeKind::Created, "{created:#?}");
    assert!(created[0].has_new_content());
    assert!(
        created[0]
            .reasons
            .contains(Reason::FILE_CREATE | Reason::CLOSE),
        "{created:#?}"
    );
    assert!(!created[0].still_open);

    append_file(&path, b" world");
    let modified = wait_for(&mut s, id);
    assert_eq!(modified[0].kind, ChangeKind::Modified, "{modified:#?}");
    assert!(
        modified[0].reasons.contains(Reason::DATA_EXTEND),
        "{modified:#?}"
    );

    let renamed_path = dir.file("renamed.txt");
    fs::rename(&path, &renamed_path).unwrap();
    assert_eq!(id_of(&renamed_path), id, "a rename must keep the file ID");
    let renamed = wait_for(&mut s, id);
    assert_eq!(renamed[0].kind, ChangeKind::Renamed, "{renamed:#?}");
    assert!(!renamed[0].has_new_content());

    fs::remove_file(&renamed_path).unwrap();
    let deleted = wait_for(&mut s, id);
    assert_eq!(deleted[0].kind, ChangeKind::Deleted, "{deleted:#?}");
    assert!(deleted[0].reasons.contains(Reason::FILE_DELETE));
}

#[test]
fn changes_have_no_paths_without_elevation() {
    let dir = TempDir::new();
    let mut s = stream(open(), Start::Now);
    let path = dir.file("nameless.txt");
    write_file(&path, b"hello");
    let changes = wait_for(&mut s, id_of(&path));
    assert_eq!(changes[0].path, None);
}

#[test]
fn merges_repeated_writes_into_one_change() {
    let dir = TempDir::new();
    let path = dir.file("repeated.txt");
    write_file(&path, b"start");
    let id = id_of(&path);
    let mut s = stream(open(), Start::Now);

    for _ in 0..20 {
        let mut f = OpenOptions::new().append(true).open(&path).unwrap();
        f.write_all(b"more").unwrap();
    }
    let changes = wait_for(&mut s, id);
    assert_eq!(changes.len(), 1, "{changes:#?}");
    assert_eq!(changes[0].kind, ChangeKind::Modified);
    assert!(changes[0].first_usn < changes[0].last_usn);
}

#[test]
fn does_not_report_a_file_created_and_deleted_while_settling() {
    let dir = TempDir::new();
    let mut s = stream(open(), Start::Now);

    let transient = dir.file("transient.txt");
    write_file(&transient, b"x");
    let transient_id = id_of(&transient);
    fs::remove_file(&transient).unwrap();
    let marker = dir.file("marker.txt");
    write_file(&marker, b"x");
    let marker_id = id_of(&marker);

    let deadline = Instant::now() + TIMEOUT;
    let mut saw_marker = false;
    while !saw_marker && Instant::now() < deadline {
        for change in s.poll().unwrap().changes {
            assert_ne!(change.id, transient_id, "{change:#?}");
            saw_marker |= change.id == marker_id;
        }
        std::thread::sleep(Duration::from_millis(25));
    }
    assert!(saw_marker, "the marker file should be reported");
}

/// The file is reported while still open, so a reader now believes it exists.
/// When it is deleted, the final record still has the create flag, because
/// flags accumulate until the file closes.
#[test]
fn a_file_deleted_after_being_reported_open_gets_its_delete() {
    let dir = TempDir::new();
    let mut r = reader(open(), Start::Now);

    let path = dir.file("reported-open.txt");
    let mut f = File::create(&path).unwrap();
    f.write_all(b"x").unwrap();
    f.sync_all().unwrap();
    let id = id_of(&path);

    let first = r.read_to_end().unwrap();
    let report = first.changes.iter().find(|c| c.id == id).unwrap();
    assert_eq!(report.kind, ChangeKind::Created, "{report:#?}");
    assert!(report.still_open, "{report:#?}");

    fs::remove_file(&path).unwrap();
    drop(f);

    let deadline = Instant::now() + TIMEOUT;
    while Instant::now() < deadline {
        let batch = r.read_to_end().unwrap();
        if let Some(change) = batch.changes.iter().find(|c| c.id == id) {
            if change.kind == ChangeKind::Deleted {
                return;
            }
        }
        std::thread::sleep(Duration::from_millis(25));
    }
    panic!("the delete was never reported");
}

#[test]
fn resuming_reports_changes_made_while_stopped() {
    let dir = TempDir::new();
    let mut s = stream(open(), Start::Now);
    let cursor = s.poll().unwrap().cursor;
    drop(s);

    let path = dir.file("while-stopped.txt");
    write_file(&path, b"hello");
    let id = id_of(&path);

    let mut s = stream(open(), Start::At(cursor));
    assert!(!wait_for(&mut s, id).is_empty());
}

#[test]
fn read_to_stops_at_a_real_position() {
    let volume = open();
    let start = volume.journal_info().unwrap().end_cursor();
    let dir = TempDir::new();
    let before = dir.file("before.txt");
    write_file(&before, b"before");
    let before_id = id_of(&before);

    let end = volume.journal_info().unwrap().end_cursor();
    let after = dir.file("after.txt");
    write_file(&after, b"after");
    let after_id = id_of(&after);

    let mut r = reader(volume, Start::At(start));
    let first = r.read_to(end).unwrap();
    assert!(has(&first, before_id), "{first:?}");
    assert!(!has(&first, after_id), "{first:?}");
    assert_eq!(first.gap, None);
    // Earlier than `end` if another process was halfway through a rename, and
    // just past it if `end` is in the padding before the next 4 KiB page.
    assert!(first.cursor.usn.0 < end.usn.0 + 4096);

    let end = r.volume().journal_info().unwrap().end_cursor();
    let second = r.read_to(end).unwrap();
    assert!(has(&second, after_id), "{second:?}");
    assert!(second.cursor.usn.0 < end.usn.0 + 4096);
}

#[test]
fn a_cursor_from_another_journal_is_a_gap() {
    let volume = open();
    let id = volume.journal_info().unwrap().id;
    let stale = Cursor {
        journal: JournalId(id.0.wrapping_add(1)),
        usn: Usn(0),
    };
    let mut s = stream(volume, Start::At(stale));
    let batch = s.poll().unwrap();
    assert!(
        matches!(batch.gap, Some(Gap::JournalRecreated { .. })),
        "{batch:?}"
    );
    assert_eq!(
        batch.cursor.journal, id,
        "the stream should move to the real journal"
    );
    assert_eq!(s.poll().unwrap().gap, None);
}

#[test]
fn a_cursor_outside_the_journal_is_a_gap() {
    let volume = open();
    let info = volume.journal_info().unwrap();
    // A journal that has never wrapped has no position older than its start,
    // so use one past its end, which is just as unreadable.
    let usn = if info.oldest_readable().0 > 0 {
        Usn(info.oldest_readable().0 - 1)
    } else {
        Usn(info.next_usn.0 + 1_000_000)
    };
    let lost = Cursor {
        journal: info.id,
        usn,
    };
    let mut s = stream(volume, Start::At(lost));
    let batch = s.poll().unwrap();
    assert!(
        matches!(batch.gap, Some(Gap::RecordsLost { .. })),
        "{batch:?}"
    );
}

#[test]
fn a_watcher_delivers_real_changes_and_stops() {
    let dir = TempDir::new();
    let watcher = stream(open(), Start::Now)
        .watch(WatchOptions::new().poll_interval(Duration::from_millis(25)))
        .unwrap();
    let path = dir.file("watched.txt");
    write_file(&path, b"hello");
    let id = id_of(&path);

    let deadline = Instant::now() + TIMEOUT;
    while Instant::now() < deadline {
        match watcher.events().recv_timeout(TIMEOUT).unwrap() {
            WatchEvent::Batch(batch) if has(&batch, id) => {
                watcher.stop();
                return;
            }
            WatchEvent::Batch(_) => {}
            other => panic!("unexpected event: {other:?}"),
        }
    }
    panic!("the change was never delivered");
}

// ------------------------------------------------------------------ links

#[test]
fn a_junction_is_reported_as_a_link_with_no_content() {
    let dir = TempDir::new();
    let target = dir.file("target");
    fs::create_dir(&target).unwrap();
    let mut s = stream(open(), Start::Now);

    let link = dir.file("junction");
    make_junction(&link, &target);
    let id = id_of_link(&link);
    assert_ne!(id, id_of(&target));

    let c = wait_until(&mut s, id, FileChange::is_link);
    assert!(c.is_directory(), "{c:#?}");
    assert!(c.is_reparse_point(), "{c:#?}");
    assert_eq!(c.reparse_tag, Some(ReparseTag::MOUNT_POINT));
    assert!(!c.has_new_content());
}

#[test]
fn a_write_through_a_junction_is_reported_on_the_real_file() {
    let dir = TempDir::new();
    let target = dir.file("target");
    fs::create_dir(&target).unwrap();
    let link = dir.file("junction");
    make_junction(&link, &target);
    let real = target.join("through.txt");
    write_file(&real, b"first");
    let id = id_of(&real);
    let parent = id_of(&target);
    let mut s = stream(open(), Start::Now);

    append_file(&link.join("through.txt"), b"second");

    let c = wait_until(&mut s, id, FileChange::has_new_content);
    assert_eq!(c.kind, ChangeKind::Modified);
    assert_eq!(c.parent_id, parent, "the parent must be the real directory");
    assert!(!c.is_link());
}

#[test]
#[ignore = "needs the privilege to create symbolic links"]
fn a_file_symlink_is_reported_as_a_link_with_no_content() {
    let dir = TempDir::new();
    let target = dir.file("target.txt");
    write_file(&target, b"data");
    let mut s = stream(open(), Start::Now);

    let link = dir.file("link.txt");
    symlink_file(&target, &link).unwrap();
    let id = id_of_link(&link);
    assert_ne!(id, id_of(&target));

    let c = wait_until(&mut s, id, FileChange::is_link);
    assert!(!c.is_directory(), "{c:#?}");
    assert_eq!(c.reparse_tag, Some(ReparseTag::SYMLINK));
    assert!(!c.has_new_content());
}

#[test]
#[ignore = "needs the privilege to create symbolic links"]
fn a_directory_symlink_is_reported_as_a_link() {
    let dir = TempDir::new();
    let target = dir.file("target");
    fs::create_dir(&target).unwrap();
    let mut s = stream(open(), Start::Now);

    let link = dir.file("link");
    symlink_dir(&target, &link).unwrap();
    let c = wait_until(&mut s, id_of_link(&link), FileChange::is_link);
    assert!(c.is_directory(), "{c:#?}");
    assert_eq!(c.reparse_tag, Some(ReparseTag::SYMLINK));
}

#[test]
fn a_plain_file_has_no_reparse_tag() {
    let dir = TempDir::new();
    let mut s = stream(open(), Start::Now);
    let path = dir.file("plain.txt");
    write_file(&path, b"data");
    let c = wait_until(&mut s, id_of(&path), |_| true);
    assert!(!c.is_reparse_point());
    assert_eq!(c.reparse_tag, None);
    assert!(c.has_new_content());
}

#[test]
fn adding_a_hard_link_is_a_metadata_change_on_the_same_file() {
    let dir = TempDir::new();
    let original = dir.file("original.txt");
    write_file(&original, b"data");
    let id = id_of(&original);
    let mut s = stream(open(), Start::Now);

    let second = dir.file("second.txt");
    fs::hard_link(&original, &second).unwrap();
    assert_eq!(id_of(&second), id, "hard links share one identity");

    let c = wait_until(&mut s, id, |c| c.reasons.contains(Reason::HARD_LINK_CHANGE));
    assert_eq!(c.kind, ChangeKind::MetadataChanged, "{c:#?}");
    assert!(!c.is_reparse_point());
    assert!(!c.has_new_content());
}

#[test]
fn a_write_through_either_hard_link_is_reported_on_the_one_identity() {
    let dir = TempDir::new();
    let original = dir.file("original.txt");
    write_file(&original, b"data");
    let second = dir.file("second.txt");
    fs::hard_link(&original, &second).unwrap();
    let id = id_of(&original);
    let mut s = stream(open(), Start::Now);

    append_file(&second, b"more");

    let c = wait_until(&mut s, id, FileChange::has_new_content);
    assert_eq!(c.kind, ChangeKind::Modified, "{c:#?}");
}

#[test]
fn deleting_one_hard_link_does_not_report_the_file_as_deleted() {
    let dir = TempDir::new();
    let original = dir.file("original.txt");
    write_file(&original, b"data");
    let second = dir.file("second.txt");
    fs::hard_link(&original, &second).unwrap();
    let id = id_of(&original);
    let mut s = stream(open(), Start::Now);

    fs::remove_file(&second).unwrap();
    assert_eq!(fs::read(&original).unwrap(), b"data");

    let c = wait_until(&mut s, id, |_| true);
    assert_ne!(c.kind, ChangeKind::Deleted, "{c:#?}");
}

#[test]
fn the_public_types_can_be_shared_between_threads() {
    fn assert_send_sync<T: Send + Sync>() {}
    fn assert_send<T: Send>() {}
    assert_send_sync::<Volume>();
    assert_send::<ChangeStream>();
    assert_send::<ChangeReader>();
}
