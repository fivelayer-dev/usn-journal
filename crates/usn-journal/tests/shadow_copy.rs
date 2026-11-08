//! Tests against a real volume shadow copy, which the test workflow takes.
//!
//! The workflow creates `usn-shadow-copy-test` with `kept`, `changed`, `deleted`
//! and `renamed`, takes the copy, and then changes that directory, so anything
//! these tests see from after the copy is a failure. It names the copy in
//! `USN_TEST_SHADOW_COPY`. These tests never create a copy themselves, and they
//! fail rather than skip if that variable is missing.
#![cfg(windows)]

mod common;

use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};

use common::temp_drive;
use usn_journal::{ChangeOptions, ChangeReader, PathFilter, ReadOptions, ScanEntry, Start, Volume};

const TEST_DIR: &str = "usn-shadow-copy-test";

fn device() -> String {
    std::env::var("USN_TEST_SHADOW_COPY").expect("set USN_TEST_SHADOW_COPY to a shadow copy")
}

fn open_copy() -> Volume {
    Volume::open(device()).unwrap()
}

fn scan_test_dir(volume: &Volume) -> Vec<ScanEntry> {
    let filter = PathFilter::new().include(TEST_DIR);
    volume.scan(&filter).unwrap().map(Result::unwrap).collect()
}

fn test_dir_path(volume: &Volume) -> PathBuf {
    volume.root().join(TEST_DIR)
}

fn names_in_test_dir(volume: &Volume) -> BTreeSet<String> {
    let dir = test_dir_path(volume);
    scan_test_dir(volume)
        .iter()
        .filter_map(|file| file.path.as_deref())
        .filter(|path| path.parent() == Some(&dir))
        .map(|path| path.file_name().unwrap().to_str().unwrap().to_string())
        .collect()
}

#[test]
#[ignore = "needs a shadow copy named by USN_TEST_SHADOW_COPY"]
fn shadow_copy_scan_lists_the_volume_as_it_was() {
    let volume = open_copy();
    let expected = ["changed.txt", "deleted.txt", "kept.txt", "renamed.txt"];
    assert_eq!(
        names_in_test_dir(&volume),
        expected.map(String::from).into_iter().collect()
    );
}

#[test]
#[ignore = "needs a shadow copy named by USN_TEST_SHADOW_COPY"]
fn shadow_copy_scanned_paths_read_the_files_in_the_copy() {
    let volume = open_copy();
    let dir = test_dir_path(&volume);
    assert!(dir.starts_with(Path::new(&device())), "{dir:?}");
    for name in ["kept.txt", "changed.txt", "deleted.txt", "renamed.txt"] {
        assert_eq!(fs::read(dir.join(name)).unwrap(), b"before", "{name}");
    }
}

#[test]
#[ignore = "needs a shadow copy named by USN_TEST_SHADOW_COPY"]
fn shadow_copy_directory_is_found_by_its_identity() {
    let volume = open_copy();
    let dir = test_dir_path(&volume);
    let found = scan_test_dir(&volume)
        .into_iter()
        .find(|file| file.path.as_deref() == Some(&dir))
        .expect("the scan should list the test directory");
    assert_eq!(volume.path_of(found.id).unwrap(), dir);
}

#[test]
#[ignore = "needs a shadow copy named by USN_TEST_SHADOW_COPY"]
fn shadow_copy_journal_position_is_frozen_before_the_live_one() {
    let volume = open_copy();
    let frozen = volume.journal_info().unwrap();
    let scan = volume.scan(&PathFilter::new().include(TEST_DIR)).unwrap();
    assert_eq!(scan.start_cursor(), frozen.end_cursor());
    assert!(scan.count() > 0);
    assert_eq!(volume.journal_info().unwrap(), frozen);

    let live = Volume::open(temp_drive()).unwrap().journal_info().unwrap();
    assert_eq!(live.id, frozen.id);
    assert!(frozen.next_usn < live.next_usn);
}

#[test]
#[ignore = "needs a shadow copy named by USN_TEST_SHADOW_COPY"]
fn shadow_copy_records_end_cleanly_at_the_frozen_position() {
    let volume = open_copy();
    let frozen = volume.journal_info().unwrap();
    let mut count = 0u64;
    for record in volume.records(ReadOptions::new(Start::Oldest)).unwrap() {
        let record = record.unwrap();
        assert!(record.usn < frozen.next_usn, "{record:?}");
        count += 1;
    }
    assert!(count > 0);
}

#[test]
#[ignore = "needs a shadow copy named by USN_TEST_SHADOW_COPY"]
fn shadow_copy_changes_can_be_read_to_the_end() {
    let volume = open_copy();
    let options = ChangeOptions::new().preload_directories(false);
    let mut reader = ChangeReader::new(volume, Start::Oldest, options).unwrap();
    let batch = reader.read_to_end().unwrap();
    assert_eq!(batch.gap, None);
    assert!(batch.caught_up);
}
