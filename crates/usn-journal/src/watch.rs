use std::sync::mpsc::{
    self, Receiver, RecvTimeoutError, Sender, SyncSender, TryRecvError, TrySendError,
};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use crate::change::ChangeBatch;
use crate::engine::Engine;
use crate::error::{Error, Result};
use crate::options::WatchOptions;
use crate::source::Source;

const MAX_BACKOFF: Duration = Duration::from_secs(30);
const SEND_RETRY: Duration = Duration::from_millis(50);
/// How often to report cursor progress when all changes were filtered out, so
/// that a saved cursor keeps moving on a volume with only excluded activity.
const CHECKPOINT_INTERVAL: Duration = Duration::from_secs(5);

/// What a watcher delivers. This enum is deliberately exhaustive.
#[derive(Debug)]
pub enum WatchEvent {
    /// New changes, or progress worth saving.
    Batch(ChangeBatch),
    /// Reading failed with a transient error, and will be retried with backoff.
    Retrying(Error),
    /// Reading failed permanently. This is the last event.
    Stopped(Error),
}

/// A background thread polling an engine. It stops when this is dropped.
pub(crate) struct WatchThread {
    events: Receiver<WatchEvent>,
    stop: Option<Sender<()>>,
    thread: Option<JoinHandle<()>>,
}

impl WatchThread {
    pub fn start<S>(engine: Engine<S>, options: WatchOptions) -> Result<WatchThread>
    where
        S: Source + Send + 'static,
    {
        let (events_tx, events) = mpsc::sync_channel(options.channel_capacity);
        let (stop_tx, stop_rx) = mpsc::channel();
        let thread = thread::Builder::new()
            .name("usn-watch".into())
            .spawn(move || run(engine, options.poll_interval, events_tx, stop_rx))?;
        Ok(WatchThread {
            events,
            stop: Some(stop_tx),
            thread: Some(thread),
        })
    }

    pub fn events(&self) -> &Receiver<WatchEvent> {
        &self.events
    }
}

impl Drop for WatchThread {
    fn drop(&mut self) {
        drop(self.stop.take());
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

fn run<S: Source>(
    mut engine: Engine<S>,
    poll_interval: Duration,
    events: SyncSender<WatchEvent>,
    stop: Receiver<()>,
) {
    let mut backoff = poll_interval;
    let mut last_sent = Instant::now();
    let mut last_cursor = engine.cursor();
    loop {
        let wait = match engine.poll(Instant::now()) {
            Ok(batch) => {
                backoff = poll_interval;
                let has_news = !batch.changes.is_empty() || batch.gap.is_some();
                let checkpoint_due =
                    batch.cursor != last_cursor && last_sent.elapsed() >= CHECKPOINT_INTERVAL;
                let caught_up = batch.caught_up;
                if has_news || checkpoint_due {
                    last_cursor = batch.cursor;
                    last_sent = Instant::now();
                    if !send(&events, &stop, WatchEvent::Batch(batch)) {
                        return;
                    }
                }
                if caught_up {
                    poll_interval
                } else {
                    Duration::ZERO
                }
            }
            Err(error) if error.is_transient() => {
                if !send(&events, &stop, WatchEvent::Retrying(error)) {
                    return;
                }
                let wait = backoff;
                backoff = (backoff * 2).min(MAX_BACKOFF);
                let _ = engine.reconnect();
                wait
            }
            Err(error) => {
                send(&events, &stop, WatchEvent::Stopped(error));
                return;
            }
        };
        if should_stop(&stop, wait) {
            return;
        }
    }
}

fn should_stop(stop: &Receiver<()>, wait: Duration) -> bool {
    if wait.is_zero() {
        return !matches!(stop.try_recv(), Err(TryRecvError::Empty));
    }
    !matches!(stop.recv_timeout(wait), Err(RecvTimeoutError::Timeout))
}

/// Returns false if the watcher should exit. Blocks while the channel is full,
/// but stays responsive to a stop request.
fn send(events: &SyncSender<WatchEvent>, stop: &Receiver<()>, mut event: WatchEvent) -> bool {
    loop {
        match events.try_send(event) {
            Ok(()) => return true,
            Err(TrySendError::Disconnected(_)) => return false,
            Err(TrySendError::Full(returned)) => {
                event = returned;
                if should_stop(stop, SEND_RETRY) {
                    return false;
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::io;

    use super::*;
    use crate::journal::Start;
    use crate::options::{ChangeOptions, SettleOptions};
    use crate::testing::{FakeJournal, v2};
    use crate::types::Reason;

    const TIMEOUT: Duration = Duration::from_secs(10);

    fn engine(journal: FakeJournal) -> Engine<FakeJournal> {
        let settle = SettleOptions::new().settle(Duration::ZERO);
        Engine::new(journal, Start::Oldest, ChangeOptions::new(), settle).unwrap()
    }

    fn fast() -> WatchOptions {
        WatchOptions::new()
            .poll_interval(Duration::from_millis(5))
            .channel_capacity(1)
    }

    fn journal_with_change() -> FakeJournal {
        let mut j = FakeJournal::new();
        j.append(|usn| v2(usn, 20, 5, Reason::DATA_EXTEND.0 | Reason::CLOSE.0, 0, "a"));
        j
    }

    fn transient() -> Error {
        Error::Io(io::Error::other("the volume went away"))
    }

    /// `Engine::new` also queries, so requeue the errors for the watcher.
    fn engine_after_first_query(mut j: FakeJournal) -> Engine<FakeJournal> {
        let errors = std::mem::take(&mut j.query_errors);
        let mut e = engine(j);
        e.source_mut().query_errors = errors;
        e
    }

    #[test]
    fn delivers_changes_over_the_channel() {
        let thread = WatchThread::start(engine(journal_with_change()), fast()).unwrap();
        match thread.events().recv_timeout(TIMEOUT).unwrap() {
            WatchEvent::Batch(b) => assert_eq!(b.changes.len(), 1),
            other => panic!("unexpected event: {other:?}"),
        }
    }

    #[test]
    fn reports_transient_errors_then_recovers() {
        let mut j = journal_with_change();
        j.query_errors.push_back(transient());
        let thread = WatchThread::start(engine_after_first_query(j), fast()).unwrap();
        let mut saw_error = false;
        loop {
            match thread.events().recv_timeout(TIMEOUT).unwrap() {
                WatchEvent::Retrying(_) => saw_error = true,
                WatchEvent::Batch(b) => {
                    assert!(saw_error, "the error should arrive first");
                    assert_eq!(b.changes.len(), 1);
                    break;
                }
                other => panic!("unexpected event: {other:?}"),
            }
        }
    }

    #[test]
    fn a_permanent_error_is_the_last_event() {
        let mut j = journal_with_change();
        j.query_errors.push_back(Error::JournalNotActive);
        let thread = WatchThread::start(engine_after_first_query(j), fast()).unwrap();
        match thread.events().recv_timeout(TIMEOUT).unwrap() {
            WatchEvent::Stopped(Error::JournalNotActive) => {}
            other => panic!("unexpected event: {other:?}"),
        }
        assert!(matches!(
            thread.events().recv_timeout(TIMEOUT),
            Err(RecvTimeoutError::Disconnected)
        ));
    }

    #[test]
    fn corrupt_data_stops_the_watcher_rather_than_retrying_forever() {
        let mut j = FakeJournal::new();
        j.append(|usn| {
            let mut r = v2(usn, 20, 5, 0, 0, "a");
            r[56..58].copy_from_slice(&9999u16.to_le_bytes());
            r
        });
        let thread = WatchThread::start(engine(j), fast()).unwrap();
        assert!(matches!(
            thread.events().recv_timeout(TIMEOUT).unwrap(),
            WatchEvent::Stopped(Error::Corrupt(_))
        ));
    }

    #[test]
    fn stop_returns_promptly_while_the_consumer_is_not_reading() {
        let mut j = FakeJournal::new();
        for file in 0..50 {
            j.append(|usn| {
                v2(
                    usn,
                    file,
                    5,
                    Reason::DATA_EXTEND.0 | Reason::CLOSE.0,
                    0,
                    "a",
                )
            });
        }
        j.query_errors.extend((0..50).map(|_| transient()));
        let thread = WatchThread::start(engine_after_first_query(j), fast()).unwrap();
        std::thread::sleep(Duration::from_millis(50));
        let started = Instant::now();
        drop(thread);
        assert!(started.elapsed() < TIMEOUT);
    }

    #[test]
    fn a_reconnect_is_attempted_after_a_transient_error() {
        let mut j = journal_with_change();
        j.query_errors.push_back(transient());
        let mut e = engine_after_first_query(j);
        assert!(e.poll(Instant::now()).is_err());
        e.reconnect().unwrap();
        assert_eq!(e.source().reconnects, 1);
    }

    #[test]
    fn dropped_events_end_the_thread() {
        let thread = WatchThread::start(engine(journal_with_change()), fast()).unwrap();
        drop(thread);
    }

    #[test]
    fn change_readers_can_move_between_threads() {
        fn assert_send<T: Send>() {}
        assert_send::<Engine<FakeJournal>>();
    }
}
