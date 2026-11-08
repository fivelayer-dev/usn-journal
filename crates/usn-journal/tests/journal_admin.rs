//! Creates and deletes a journal, which needs a volume that can safely lose one.
//!
//! The test workflow attaches an empty virtual disk and names it in
//! `USN_TEST_DISPOSABLE_VOLUME`. These tests fail rather than skip if that
//! variable is missing, and they never touch any other volume.
#![cfg(all(windows, feature = "journal-admin"))]

use usn_journal::{Error, Gap, ReadOptions, Start, Volume};

const MIB: u64 = 1024 * 1024;

fn open() -> Volume {
    let name = std::env::var("USN_TEST_DISPOSABLE_VOLUME")
        .expect("set USN_TEST_DISPOSABLE_VOLUME to a volume that can lose its journal");
    Volume::open(name).unwrap()
}

#[test]
#[ignore = "deletes the journal on USN_TEST_DISPOSABLE_VOLUME"]
fn disposable_volume_journal_can_be_created_resized_and_deleted() {
    let volume = open();

    // Larger than the 32 MiB that ReFS reported when asked for less.
    volume.create_journal(64 * MIB, 8 * MIB).unwrap();
    let first = volume.journal_info().unwrap();
    assert_eq!(first.maximum_size, 64 * MIB);

    volume.create_journal(128 * MIB, 16 * MIB).unwrap();
    let resized = volume.journal_info().unwrap();
    assert_eq!(resized.id, first.id, "resizing keeps the journal");
    assert_eq!(resized.maximum_size, 128 * MIB);
    let cursor = resized.end_cursor();

    volume.delete_journal().unwrap();
    assert!(matches!(
        volume.journal_info(),
        Err(Error::JournalNotActive)
    ));

    // A journal's ID comes from the system clock, so one recreated within the
    // same clock tick can get the same ID.
    std::thread::sleep(std::time::Duration::from_millis(100));

    volume.create_journal(64 * MIB, 8 * MIB).unwrap();
    let recreated = volume.journal_info().unwrap();
    assert_ne!(recreated.id, first.id, "a new journal has a new identity");
    assert!(matches!(
        recreated.check(cursor),
        Some(Gap::JournalRecreated { .. })
    ));
    assert!(matches!(
        volume.records(ReadOptions::new(Start::At(cursor))).err(),
        Some(Error::Gap(Gap::JournalRecreated { .. }))
    ));
}
