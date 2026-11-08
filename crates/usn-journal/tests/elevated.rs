//! Tests against the real journal of a real volume, with paths.
//!
//! Reading a journal with names needs an elevated process, so every test here
//! is ignored unless asked for. Run them from an elevated terminal with
//! `cargo test -- --ignored`. They fail rather than skip if the process isn't
//! elevated, so a pass always means they really ran.
//!
//! They only read the journal. The only writes are small files in a new temp
//! directory, which are removed afterwards.
#![cfg(windows)]

mod common;

use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use common::{TempDir, append_file, id_of, make_junction, temp_drive, write_file};
use usn_journal::{
    ChangeKind, ChangeOptions, ChangeReader, ChangeStream, Error, FileChange, HealthOptions,
    PathFilter, PathResolver, ReadOptions, Reason, SettleOptions, Start, Volume, WatchEvent,
    WatchOptions,
};

const TIMEOUT: Duration = Duration::from_secs(30);

fn open_volume() -> Volume {
    let volume = Volume::open(temp_drive()).expect("these tests need an elevated process");
    volume.journal_info().unwrap();
    volume
}

fn settle() -> SettleOptions {
    SettleOptions::new().settle(Duration::from_millis(100))
}

fn stream(volume: Volume, start: Start) -> ChangeStream {
    ChangeStream::new(volume, start, ChangeOptions::new(), settle()).unwrap()
}

/// Polls until `done` holds for the changes collected so far, or times out.
fn collect_until(
    stream: &mut ChangeStream,
    mut done: impl FnMut(&[FileChange]) -> bool,
) -> Vec<FileChange> {
    let deadline = Instant::now() + TIMEOUT;
    let mut changes = Vec::new();
    while Instant::now() < deadline {
        changes.extend(stream.poll().unwrap().changes);
        if done(&changes) {
            return changes;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    changes
}

fn for_path<'a>(changes: &'a [FileChange], path: &Path) -> Vec<&'a FileChange> {
    changes
        .iter()
        .filter(|c| c.path.as_deref() == Some(path))
        .collect()
}

/// Paths under `dir` found by a scan, with whether each is a link.
fn scan_under(volume: &Volume, filter: &PathFilter, dir: &Path) -> Vec<(PathBuf, bool)> {
    volume
        .scan(filter)
        .unwrap()
        .map(Result::unwrap)
        .filter_map(|file| {
            let is_link = file.is_link();
            file.path
                .filter(|path| path.starts_with(dir))
                .map(|path| (path, is_link))
        })
        .collect()
}

/// A filter that only keeps `path`, given as a full path.
fn include_only(path: &Path) -> PathFilter {
    PathFilter::new().include(path)
}

#[test]
#[ignore = "needs an elevated process"]
fn query_returns_a_sane_journal() {
    let info = open_volume().journal_info().unwrap();
    assert_ne!(info.id.0, 0);
    assert!(info.first_usn <= info.next_usn);
    assert!(info.next_usn <= info.max_usn);
    assert!(info.maximum_size > 0);
}

#[test]
#[ignore = "needs an elevated process"]
fn health_reports_real_history_for_a_volume() {
    let volume = open_volume();
    let cursor = volume.journal_info().unwrap().end_cursor();
    let health = volume.health(&HealthOptions::new().cursor(cursor)).unwrap();
    assert!(health.oldest_record.is_some());
    assert!(health.history.is_some());
    let state = health.cursor.unwrap();
    assert_eq!(state.gap, None);
    assert!(state.bytes_until_lost > 0);
}

#[test]
#[ignore = "needs an elevated process"]
fn watching_does_not_change_the_journal_configuration() {
    let volume = open_volume();
    let before = volume.journal_info().unwrap();
    let mut s = stream(volume, Start::Now);
    s.poll().unwrap();
    let after = s.volume().journal_info().unwrap();
    assert_eq!(before.id, after.id);
    assert_eq!(before.maximum_size, after.maximum_size);
    assert_eq!(before.allocation_delta, after.allocation_delta);
}

#[test]
#[ignore = "needs an elevated process"]
fn raw_records_have_real_names() {
    let volume = open_volume();
    let dir = TempDir::new();
    let start = volume.journal_info().unwrap().end_cursor();
    let path = dir.file("named.txt");
    write_file(&path, b"hello");
    let id = id_of(&path);
    let found = volume
        .records(ReadOptions::new(Start::At(start)))
        .unwrap()
        .map(Result::unwrap)
        .find(|r| r.id == id)
        .expect("the file should have a record");
    assert_eq!(found.name, "named.txt");
}

#[test]
#[ignore = "needs an elevated process"]
fn the_path_resolver_follows_records_to_full_paths() {
    let volume = open_volume();
    let dir = TempDir::new();
    let start = volume.journal_info().unwrap().end_cursor();

    let old = dir.file("old");
    fs::create_dir(&old).unwrap();
    let file = old.join("inside.txt");
    write_file(&file, b"hello");
    let file_id = id_of(&file);
    let new = dir.file("new");
    fs::rename(&old, &new).unwrap();
    append_file(&new.join("inside.txt"), b"more");

    let mut resolver = PathResolver::new(&volume).unwrap();
    let mut paths = Vec::new();
    for record in volume.records(ReadOptions::new(Start::At(start))).unwrap() {
        let record = record.unwrap();
        let path = resolver.resolve(&record);
        if record.id == file_id {
            paths.push(path.unwrap());
        }
    }
    assert_eq!(paths.first(), Some(&file), "{paths:#?}");
    assert_eq!(paths.last(), Some(&new.join("inside.txt")), "{paths:#?}");
}

#[test]
#[ignore = "needs an elevated process"]
fn a_lazy_path_resolver_looks_up_existing_directories() {
    let volume = open_volume();
    let dir = TempDir::new();
    let start = volume.journal_info().unwrap().end_cursor();
    let path = dir.file("lazy.txt");
    write_file(&path, b"hello");
    let id = id_of(&path);

    let mut resolver = PathResolver::lazy(&volume);
    let found = volume
        .records(ReadOptions::new(Start::At(start)))
        .unwrap()
        .map(Result::unwrap)
        .filter_map(|r| {
            let resolved = resolver.resolve(&r);
            (r.id == id).then_some(resolved)
        })
        .next()
        .unwrap();
    assert_eq!(found, Some(path));
}

#[test]
#[ignore = "needs an elevated process"]
fn the_master_file_table_lists_a_known_file() {
    let volume = open_volume();
    let dir = TempDir::new();
    let path = dir.file("listed.txt");
    write_file(&path, b"hello");
    let id = id_of(&path);
    let found = volume
        .mft_entries()
        .unwrap()
        .map(Result::unwrap)
        .find(|entry| entry.id == id)
        .expect("the file should be listed");
    assert_eq!(found.name, "listed.txt");
    assert!(!found.is_directory());
}

#[test]
#[ignore = "needs an elevated process"]
fn reports_a_created_file_at_its_full_path() {
    let dir = TempDir::new();
    let mut s = stream(open_volume(), Start::Now);
    let path = dir.file("created.txt");
    write_file(&path, b"hello");

    let changes = collect_until(&mut s, |c| !for_path(c, &path).is_empty());
    let found = for_path(&changes, &path);
    assert_eq!(found.len(), 1, "expected {path:?} in {changes:#?}");
    assert_eq!(found[0].kind, ChangeKind::Created);
    assert!(found[0].has_new_content());
    assert!(!found[0].is_directory());
}

#[test]
#[ignore = "needs an elevated process"]
fn reports_a_modified_file() {
    let dir = TempDir::new();
    let path = dir.file("modified.txt");
    write_file(&path, b"first");
    let mut s = stream(open_volume(), Start::Now);

    append_file(&path, b"second");

    let changes = collect_until(&mut s, |c| !for_path(c, &path).is_empty());
    let found = for_path(&changes, &path);
    assert_eq!(found.len(), 1, "expected {path:?} in {changes:#?}");
    assert_eq!(found[0].kind, ChangeKind::Modified);
    assert!(found[0].has_new_content());
}

#[test]
#[ignore = "needs an elevated process"]
fn reports_a_rename_with_both_paths() {
    let dir = TempDir::new();
    let old = dir.file("old-name.txt");
    let new = dir.file("new-name.txt");
    write_file(&old, b"contents");
    let mut s = stream(open_volume(), Start::Now);

    fs::rename(&old, &new).unwrap();

    let changes = collect_until(&mut s, |c| !for_path(c, &new).is_empty());
    let found = for_path(&changes, &new);
    assert_eq!(found.len(), 1, "expected {new:?} in {changes:#?}");
    assert_eq!(found[0].kind, ChangeKind::Renamed);
    assert_eq!(found[0].previous_path.as_deref(), Some(old.as_path()));
    assert!(!found[0].has_new_content());
}

#[test]
#[ignore = "needs an elevated process"]
fn reports_a_delete_of_an_existing_file() {
    let dir = TempDir::new();
    let path = dir.file("deleted.txt");
    write_file(&path, b"contents");
    let mut s = stream(open_volume(), Start::Now);

    fs::remove_file(&path).unwrap();

    let changes = collect_until(&mut s, |c| !for_path(c, &path).is_empty());
    let found = for_path(&changes, &path);
    assert_eq!(found.len(), 1, "expected {path:?} in {changes:#?}");
    assert_eq!(found[0].kind, ChangeKind::Deleted);
}

#[test]
#[ignore = "needs an elevated process"]
fn file_created_and_deleted_while_settling_is_not_reported() {
    let dir = TempDir::new();
    let mut s = stream(open_volume(), Start::Now);
    let transient = dir.file("transient.txt");
    let marker = dir.file("marker.txt");
    write_file(&transient, b"x");
    fs::remove_file(&transient).unwrap();
    write_file(&marker, b"x");

    let changes = collect_until(&mut s, |c| !for_path(c, &marker).is_empty());
    assert!(
        !for_path(&changes, &marker).is_empty(),
        "the marker should arrive"
    );
    assert!(for_path(&changes, &transient).is_empty(), "{changes:#?}");
}

#[test]
#[ignore = "needs an elevated process"]
fn resuming_from_a_cursor_reports_changes_made_while_stopped() {
    let dir = TempDir::new();
    let mut s = stream(open_volume(), Start::Now);
    let cursor = s.poll().unwrap().cursor;
    drop(s);

    let path = dir.file("while-stopped.txt");
    write_file(&path, b"hello");

    let mut s = stream(open_volume(), Start::At(cursor));
    let changes = collect_until(&mut s, |c| !for_path(c, &path).is_empty());
    assert!(
        !for_path(&changes, &path).is_empty(),
        "expected {path:?} in {changes:#?}"
    );
}

#[test]
#[ignore = "needs an elevated process"]
fn read_to_reports_real_paths_up_to_a_position() {
    let volume = open_volume();
    let dir = TempDir::new();
    let start = volume.journal_info().unwrap().end_cursor();
    let before = dir.file("before.txt");
    write_file(&before, b"before");

    let end = volume.journal_info().unwrap().end_cursor();
    let after = dir.file("after.txt");
    write_file(&after, b"after");

    let mut reader = ChangeReader::new(volume, Start::At(start), ChangeOptions::new()).unwrap();
    let first = reader.read_to(end).unwrap();
    assert_eq!(for_path(&first.changes, &before).len(), 1, "{first:?}");
    assert!(for_path(&first.changes, &after).is_empty(), "{first:?}");
    assert_eq!(first.gap, None);
    // Earlier than `end` if another process was halfway through a rename, and
    // just past it if `end` is in the padding before the next 4 KiB page.
    assert!(first.cursor.usn.0 < end.usn.0 + 4096);

    let second = reader.read_to_end().unwrap();
    assert_eq!(for_path(&second.changes, &after).len(), 1, "{second:?}");
}

#[test]
#[ignore = "needs an elevated process"]
fn a_scan_finds_a_known_file_and_directory() {
    let volume = open_volume();
    let dir = TempDir::new();
    let path = dir.file("scanned.txt");
    write_file(&path, b"hello");

    let scan = volume
        .scan(&PathFilter::new().exclude_system_files())
        .unwrap();
    let cursor = scan.start_cursor();
    let mut saw_file = false;
    let mut saw_dir = false;
    let mut count = 0u64;
    for file in scan {
        let file = file.unwrap();
        count += 1;
        if file.path.as_deref() == Some(path.as_path()) {
            saw_file = !file.is_directory();
        }
        if file.path.as_deref() == Some(dir.0.as_path()) {
            saw_dir = file.is_directory();
        }
    }
    assert!(saw_file, "{path:?} not found among {count} entries");
    assert!(saw_dir, "{:?} not found among {count} entries", dir.0);
    assert_eq!(cursor.journal, volume.journal_info().unwrap().id);
}

#[test]
#[ignore = "needs an elevated process"]
fn a_scan_can_stop_early() {
    let volume = open_volume();
    assert_eq!(volume.scan(&PathFilter::new()).unwrap().take(3).count(), 3);
}

#[test]
#[ignore = "needs an elevated process"]
fn directory_lookup_resolves_without_preloading() {
    let dir = TempDir::new();
    let options = ChangeOptions::new().preload_directories(false);
    let mut s = ChangeStream::new(open_volume(), Start::Now, options, settle()).unwrap();
    let path = dir.file("lazy.txt");
    write_file(&path, b"hello");
    let changes = collect_until(&mut s, |c| !for_path(c, &path).is_empty());
    assert!(
        !for_path(&changes, &path).is_empty(),
        "expected {path:?} in {changes:#?}"
    );
}

#[test]
#[ignore = "needs an elevated process"]
fn watching_reports_a_junction_at_its_own_path_as_a_link() {
    let dir = TempDir::new();
    let target = dir.file("target");
    fs::create_dir(&target).unwrap();
    let mut s = stream(open_volume(), Start::Now);

    let link = dir.file("junction");
    make_junction(&link, &target);

    let changes = collect_until(&mut s, |c| for_path(c, &link).iter().any(|c| c.is_link()));
    let found = for_path(&changes, &link);
    assert!(found.iter().any(|c| c.is_link()), "{changes:#?}");
    assert!(found.iter().all(|c| !c.has_new_content()), "{changes:#?}");
}

#[test]
#[ignore = "needs an elevated process"]
fn a_write_through_a_junction_is_reported_at_the_real_path() {
    let dir = TempDir::new();
    let target = dir.file("target");
    fs::create_dir(&target).unwrap();
    let link = dir.file("junction");
    make_junction(&link, &target);
    let real = target.join("through.txt");
    write_file(&real, b"first");
    let mut s = stream(open_volume(), Start::Now);

    write_file(&link.join("through.txt"), b"second");

    let changes = collect_until(&mut s, |c| !for_path(c, &real).is_empty());
    assert!(!for_path(&changes, &real).is_empty(), "{changes:#?}");
    assert!(
        for_path(&changes, &link.join("through.txt")).is_empty(),
        "{changes:#?}"
    );
}

#[test]
#[ignore = "needs an elevated process"]
fn a_scan_lists_a_junction_as_a_link_and_does_not_follow_it() {
    let volume = open_volume();
    let dir = TempDir::new();
    let target = dir.file("target");
    fs::create_dir(&target).unwrap();
    write_file(&target.join("inside.txt"), b"x");
    let link = dir.file("junction");
    make_junction(&link, &target);

    let found = scan_under(&volume, &include_only(&dir.0), &dir.0);
    assert!(found.contains(&(link.clone(), true)), "{found:#?}");
    assert!(
        found.contains(&(target.join("inside.txt"), false)),
        "{found:#?}"
    );
    assert!(
        !found.iter().any(|(p, _)| p == &link.join("inside.txt")),
        "{found:#?}"
    );
}

#[test]
#[ignore = "needs an elevated process"]
fn a_file_symlink_is_a_link_and_its_target_is_not() {
    let volume = open_volume();
    let dir = TempDir::new();
    let target = dir.file("target.txt");
    write_file(&target, b"x");
    let link = dir.file("link.txt");
    std::os::windows::fs::symlink_file(&target, &link).unwrap();

    let found = scan_under(&volume, &include_only(&dir.0), &dir.0);
    assert!(found.contains(&(link, true)), "{found:#?}");
    assert!(found.contains(&(target, false)), "{found:#?}");
}

#[test]
#[ignore = "needs an elevated process"]
fn a_scan_lists_a_hard_linked_file_under_at_least_one_name() {
    let volume = open_volume();
    let dir = TempDir::new();
    let first = dir.file("first.txt");
    write_file(&first, b"x");
    let second = dir.file("second.txt");
    fs::hard_link(&first, &second).unwrap();

    let found = scan_under(&volume, &include_only(&dir.0), &dir.0);
    let names = found
        .iter()
        .filter(|(p, _)| *p == first || *p == second)
        .count();
    assert!(names >= 1, "{found:#?}");
}

#[test]
#[ignore = "needs an elevated process"]
fn include_limits_a_scan_and_a_watch() {
    let volume = open_volume();
    let dir = TempDir::new();
    let kept = dir.file("kept");
    let other = dir.file("other");
    fs::create_dir(&kept).unwrap();
    fs::create_dir(&other).unwrap();
    let kept_file = kept.join("a.txt");
    let other_file = other.join("a.txt");
    write_file(&kept_file, b"x");
    write_file(&other_file, b"x");

    let filter = include_only(&kept);
    let found = scan_under(&volume, &filter, &dir.0);
    assert!(found.iter().any(|(p, _)| p == &kept_file), "{found:#?}");
    assert!(!found.iter().any(|(p, _)| p == &other_file), "{found:#?}");

    let options = ChangeOptions::new().filter(filter);
    let mut s = ChangeStream::new(volume, Start::Now, options, settle()).unwrap();
    write_file(&other_file, b"changed");
    write_file(&kept_file, b"changed");
    let changes = collect_until(&mut s, |c| !for_path(c, &kept_file).is_empty());
    assert!(!for_path(&changes, &kept_file).is_empty(), "{changes:#?}");
    assert!(for_path(&changes, &other_file).is_empty(), "{changes:#?}");
}

#[test]
#[ignore = "needs an elevated process"]
fn a_watcher_reports_full_paths() {
    let dir = TempDir::new();
    let watcher = stream(open_volume(), Start::Now)
        .watch(WatchOptions::new().poll_interval(Duration::from_millis(25)))
        .unwrap();
    let path = dir.file("watched.txt");
    write_file(&path, b"hello");

    let deadline = Instant::now() + TIMEOUT;
    while Instant::now() < deadline {
        match watcher.events().recv_timeout(TIMEOUT).unwrap() {
            WatchEvent::Batch(batch) if !for_path(&batch.changes, &path).is_empty() => return,
            WatchEvent::Batch(_) => {}
            other => panic!("unexpected event: {other:?}"),
        }
    }
    panic!("the change was never delivered");
}

#[test]
#[ignore = "needs an elevated process"]
fn reading_records_needs_a_journal_to_exist() {
    // A reason mask of zero asks for nothing, which still goes to the kernel.
    let volume = open_volume();
    let options = ReadOptions::new(Start::Now).reasons(Reason(0));
    assert_eq!(volume.records(options).unwrap().count(), 0);
}

#[test]
fn opening_a_volume_without_elevation_reports_access_denied() {
    // When run elevated this succeeds, which is just as correct.
    match Volume::open(temp_drive()) {
        Ok(_) | Err(Error::AccessDenied) => {}
        Err(other) => panic!("unexpected error: {other}"),
    }
}
