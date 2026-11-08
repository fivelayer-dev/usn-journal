//! Read the Windows USN change journal.
//!
//! A volume's change journal is a log the file system keeps of every change to
//! every file. Reading it is how indexers, sync tools and backup software find
//! out what changed without walking the whole volume.
//!
//! The crate is built in layers, and each one can be used on its own:
//!
//! | Layer | Type | Use it to |
//! |---|---|---|
//! | Volume | [`Volume`] | Open a volume and query its journal |
//! | Records | [`Volume::records`] | Read raw journal records |
//! | Listing | [`Volume::mft_entries`], [`Volume::scan`] | List every file on the volume |
//! | Paths | [`PathResolver`] | Turn records into full paths |
//! | On-demand changes | [`ChangeReader`] | Ask what changed up to a position |
//! | Live changes | [`ChangeStream`], [`Watcher`] | Follow changes as they happen |
//! | Health | [`Volume::health`] | See how much history a journal holds |
//!
//! Reading a journal needs an elevated process, except through
//! [`Volume::open_unprivileged`], which returns records with the names removed.
//!
//! The default build only reads. Creating or deleting a journal needs the
//! `journal-admin` feature.
//!
//! # Reading raw records
//!
//! ```no_run
//! # fn main() -> usn_journal::Result<()> {
//! use usn_journal::{ReadOptions, Reason, Start, Volume};
//!
//! let volume = Volume::open("C:")?;
//! let options = ReadOptions::new(Start::Oldest).reasons(Reason::FILE_CREATE | Reason::FILE_DELETE);
//! for record in volume.records(options)? {
//!     let record = record?;
//!     println!("{} {:?} {:?}", record.usn, record.reason, record.name);
//! }
//! # Ok(())
//! # }
//! ```
//!
//! # Following changes live
//!
//! ```no_run
//! # fn main() -> usn_journal::Result<()> {
//! use usn_journal::{
//!     ChangeOptions, ChangeStream, SettleOptions, Start, Volume, WatchEvent, WatchOptions,
//! };
//!
//! let volume = Volume::open("C:")?;
//! let stream = ChangeStream::new(volume, Start::Now, ChangeOptions::new(), SettleOptions::new())?;
//! let watcher = stream.watch(WatchOptions::new())?;
//! for event in watcher.events() {
//!     match event {
//!         WatchEvent::Batch(batch) => {
//!             for change in &batch.changes {
//!                 println!("{} {:?}", change.kind, change.path);
//!             }
//!         }
//!         WatchEvent::Retrying(error) => eprintln!("retrying: {error}"),
//!         WatchEvent::Stopped(error) => return Err(error),
//!     }
//! }
//! # Ok(())
//! # }
//! ```
//!
//! # Asking what changed since last time
//!
//! ```no_run
//! # fn main() -> usn_journal::Result<()> {
//! # fn load_cursor() -> Option<String> { None }
//! # fn save_cursor(_: &str) {}
//! use usn_journal::{ChangeOptions, ChangeReader, Cursor, PathFilter, Start, Volume};
//!
//! let volume = Volume::open("D:")?;
//! let cursor = match load_cursor() {
//!     Some(text) => text.parse::<Cursor>()?,
//!     None => {
//!         // First run: list everything. Anything that changes during the
//!         // listing is picked up from this cursor, so nothing slips through.
//!         let scan = volume.scan(&PathFilter::new())?;
//!         let start = scan.start_cursor();
//!         for entry in scan {
//!             println!("{:?}", entry?.path);
//!         }
//!         start
//!     }
//! };
//!
//! let mut reader = ChangeReader::new(volume, Start::At(cursor), ChangeOptions::new())?;
//! let batch = reader.read_to_end()?;
//! if batch.gap.is_some() {
//!     // The journal no longer covers the whole period, so list the volume again.
//! }
//! for change in &batch.changes {
//!     println!("{} {:?}", change.kind, change.path);
//! }
//! // Only once the batch is safely handled. A crash repeats work but loses nothing.
//! save_cursor(&batch.cursor.to_string());
//! # Ok(())
//! # }
//! ```

#![warn(missing_docs)]
// The journal logic is portable and is tested on every platform, but outside
// of tests only the Windows layer uses it.
#![cfg_attr(not(windows), allow(dead_code, unused_imports))]

mod change;
mod coalesce;
mod engine;
mod error;
mod filter;
mod health;
mod journal;
mod options;
mod paths;
mod record;
mod source;
#[cfg(test)]
mod testing;
mod types;
mod watch;
mod wide;
#[cfg(windows)]
mod windows;

// Compiles the code samples in the documentation, so that they can't go stale.
#[cfg(all(doctest, windows))]
#[doc = include_str!("../../../README.md")]
mod readme_samples {}

#[cfg(all(doctest, windows))]
#[doc = include_str!("../../../docs/tracking-changes.md")]
mod guide_samples {}

pub use change::{ChangeBatch, ChangeKind, FileChange};
pub use error::{Error, Result};
pub use filter::PathFilter;
pub use health::{CursorHealth, HealthStatus, Issue, JournalHealth};
pub use journal::{Cursor, Gap, JournalInfo, Start};
pub use options::{ChangeOptions, HealthOptions, ReadOptions, SettleOptions, WatchOptions};
pub use record::{MftEntry, Record, ScanEntry};
pub use types::{FileAttributes, FileId, FileTime, JournalId, Reason, ReparseTag, SourceInfo, Usn};
pub use watch::WatchEvent;
#[cfg(windows)]
pub use windows::{
    ChangeReader, ChangeStream, MftEntries, PathResolver, Records, Scan, Volume, Watcher,
};
