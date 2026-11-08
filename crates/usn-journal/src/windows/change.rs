use std::sync::mpsc::Receiver;
use std::time::Instant;

use super::volume::Volume;
use crate::change::ChangeBatch;
use crate::engine::Engine;
use crate::error::Result;
use crate::journal::{Cursor, Start};
use crate::options::{ChangeOptions, SettleOptions, WatchOptions};
use crate::watch::{WatchEvent, WatchThread};

/// Reports what changed up to a chosen position, on demand.
///
/// Everything before the position is reported at once, without waiting for
/// files to settle, so this suits work done in cycles. A file still open is
/// reported with [`FileChange::still_open`](crate::FileChange::still_open) and
/// again when it closes.
pub struct ChangeReader {
    engine: Engine<Volume>,
}

impl ChangeReader {
    /// Starts reading from `start`.
    pub fn new(volume: Volume, start: Start, options: ChangeOptions) -> Result<ChangeReader> {
        Ok(ChangeReader {
            engine: Engine::new(volume, start, options, SettleOptions::new())?,
        })
    }

    /// The volume, for example to read
    /// [`JournalInfo::end_cursor`](crate::JournalInfo::end_cursor).
    pub fn volume(&self) -> &Volume {
        self.engine.source()
    }

    /// Reports everything before `end`, and leaves the rest for the next
    /// call. [`ChangeBatch::cursor`] can stop short of `end` if a file was
    /// halfway through a rename there, and can land just past it after
    /// padding in the journal, with nothing skipped.
    ///
    /// To pair changes with a frozen copy of the volume, such as a shadow
    /// copy, read `end` first, then freeze, then call this. Every reported
    /// change is then in the copy. A change made between the two is
    /// reported again next time, and a reported path can be missing from
    /// the copy if it was renamed or deleted after `end`.
    pub fn read_to(&mut self, end: Cursor) -> Result<ChangeBatch> {
        self.engine.read_to(end, Instant::now())
    }

    /// Reports everything up to the journal's current end.
    pub fn read_to_end(&mut self) -> Result<ChangeBatch> {
        self.engine.read_to_end(Instant::now())
    }

    /// The position that is safe to resume from.
    pub fn cursor(&self) -> Cursor {
        self.engine.cursor()
    }
}

/// Follows the journal as it is written, merging the records of each file
/// until it settles.
pub struct ChangeStream {
    engine: Engine<Volume>,
}

impl ChangeStream {
    /// Starts following the journal from `start`.
    pub fn new(
        volume: Volume,
        start: Start,
        options: ChangeOptions,
        settle: SettleOptions,
    ) -> Result<ChangeStream> {
        Ok(ChangeStream {
            engine: Engine::new(volume, start, options, settle)?,
        })
    }

    /// The volume.
    pub fn volume(&self) -> &Volume {
        self.engine.source()
    }

    /// Reads what is available and returns the changes now ready to report.
    /// This does a bounded amount of work and never blocks.
    pub fn poll(&mut self) -> Result<ChangeBatch> {
        self.engine.poll(Instant::now())
    }

    /// The position that is safe to resume from.
    pub fn cursor(&self) -> Cursor {
        self.engine.cursor()
    }

    /// Moves polling to a background thread.
    pub fn watch(self, options: WatchOptions) -> Result<Watcher> {
        Ok(Watcher {
            thread: WatchThread::start(self.engine, options)?,
        })
    }
}

/// A background thread following the journal. It stops when this is dropped.
/// Changes still settling are not lost, because they are read again from the
/// last saved cursor.
pub struct Watcher {
    thread: WatchThread,
}

impl Watcher {
    /// The events, which end when the thread stops.
    pub fn events(&self) -> &Receiver<WatchEvent> {
        self.thread.events()
    }

    /// Stops the thread and waits for it to finish.
    pub fn stop(self) {}
}
