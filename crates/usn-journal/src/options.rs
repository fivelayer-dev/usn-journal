use std::time::Duration;

use crate::filter::PathFilter;
use crate::journal::{Cursor, Start};
use crate::types::{Reason, SourceInfo};

/// Options for reading raw records.
#[derive(Clone, Debug)]
pub struct ReadOptions {
    pub(crate) start: Start,
    pub(crate) reasons: Reason,
    pub(crate) only_on_close: bool,
    pub(crate) wait: Option<(u64, Duration)>,
}

impl ReadOptions {
    /// Reads every record from `start`, ending when it catches up.
    pub fn new(start: Start) -> ReadOptions {
        ReadOptions {
            start,
            reasons: Reason::ALL,
            only_on_close: false,
            wait: None,
        }
    }

    /// Returns only records with one of these reasons. The default is all.
    pub fn reasons(mut self, reasons: Reason) -> ReadOptions {
        self.reasons = reasons;
        self
    }

    /// Returns only the final record written as each file closes.
    pub fn only_on_close(mut self, only_on_close: bool) -> ReadOptions {
        self.only_on_close = only_on_close;
        self
    }

    /// Waits for `min_bytes` of new records instead of ending when caught up.
    /// The iterator ends if `timeout` passes first, and can then be resumed
    /// from its position. Bytes are counted before any reason filter applies.
    pub fn wait(mut self, min_bytes: u64, timeout: Duration) -> ReadOptions {
        self.wait = Some((min_bytes, timeout));
        self
    }
}

/// Options shared by the on-demand and live change readers.
#[derive(Clone, Debug)]
pub struct ChangeOptions {
    pub(crate) filter: PathFilter,
    pub(crate) directories: bool,
    pub(crate) ignored_sources: SourceInfo,
    pub(crate) preload_directories: bool,
}

impl Default for ChangeOptions {
    fn default() -> Self {
        ChangeOptions {
            filter: PathFilter::new(),
            directories: true,
            ignored_sources: SourceInfo::default(),
            preload_directories: true,
        }
    }
}

impl ChangeOptions {
    /// Reports every path and directory, and loads directories up front.
    pub fn new() -> ChangeOptions {
        ChangeOptions::default()
    }

    /// Limits which paths are reported. The default keeps everything.
    pub fn filter(mut self, filter: PathFilter) -> ChangeOptions {
        self.filter = filter;
        self
    }

    /// Reports changes to directories. The default is true, and renames and
    /// deletes of whole trees are only visible this way.
    pub fn directories(mut self, directories: bool) -> ChangeOptions {
        self.directories = directories;
        self
    }

    /// Skips records from these kinds of process. The default skips none.
    pub fn ignored_sources(mut self, sources: SourceInfo) -> ChangeOptions {
        self.ignored_sources = sources;
        self
    }

    /// Reads every directory at startup, so that paths resolve even for files
    /// since deleted. The default is true. This reads the master file table,
    /// which can take a while on a large volume after a reboot. On a file
    /// system that can't list itself, such as ReFS, directories are looked up
    /// as needed instead.
    pub fn preload_directories(mut self, preload: bool) -> ChangeOptions {
        self.preload_directories = preload;
        self
    }
}

/// How the live change reader decides a file is ready to report.
#[derive(Copy, Clone, Debug)]
pub struct SettleOptions {
    pub(crate) settle: Duration,
    pub(crate) max_open_delay: Duration,
    pub(crate) max_pending: usize,
}

impl Default for SettleOptions {
    fn default() -> Self {
        SettleOptions {
            settle: Duration::from_secs(2),
            max_open_delay: Duration::from_secs(300),
            max_pending: 500_000,
        }
    }
}

impl SettleOptions {
    /// Uses the defaults below.
    pub fn new() -> SettleOptions {
        SettleOptions::default()
    }

    /// How long a closed file must stay untouched. The default is 2 seconds.
    pub fn settle(mut self, settle: Duration) -> SettleOptions {
        self.settle = settle;
        self
    }

    /// How long a file may stay open before it is reported anyway, with
    /// [`FileChange::still_open`](crate::FileChange::still_open) set. The
    /// default is 5 minutes.
    pub fn max_open_delay(mut self, delay: Duration) -> SettleOptions {
        self.max_open_delay = delay;
        self
    }

    /// How many files may be held before all are reported, which bounds memory.
    /// The default is 500,000.
    pub fn max_pending(mut self, files: usize) -> SettleOptions {
        self.max_pending = files;
        self
    }
}

/// Options for following the journal on a background thread.
#[derive(Copy, Clone, Debug)]
pub struct WatchOptions {
    pub(crate) poll_interval: Duration,
    pub(crate) channel_capacity: usize,
}

impl Default for WatchOptions {
    fn default() -> Self {
        WatchOptions {
            poll_interval: Duration::from_millis(500),
            channel_capacity: 16,
        }
    }
}

impl WatchOptions {
    /// Uses the defaults below.
    pub fn new() -> WatchOptions {
        WatchOptions::default()
    }

    /// How long to wait between reads once caught up. The default is 500 ms.
    pub fn poll_interval(mut self, interval: Duration) -> WatchOptions {
        self.poll_interval = interval;
        self
    }

    /// How many events to buffer before waiting for the consumer. The default
    /// is 16. Nothing is lost by waiting while the journal has room.
    pub fn channel_capacity(mut self, capacity: usize) -> WatchOptions {
        self.channel_capacity = capacity;
        self
    }
}

/// What a journal health check compares the journal against.
#[derive(Copy, Clone, Debug)]
pub struct HealthOptions {
    pub(crate) cursor: Option<Cursor>,
    pub(crate) required_history: Duration,
}

impl Default for HealthOptions {
    fn default() -> Self {
        HealthOptions {
            cursor: None,
            required_history: Duration::from_secs(24 * 3600),
        }
    }
}

impl HealthOptions {
    /// Requires 24 hours of history, and checks no cursor.
    pub fn new() -> HealthOptions {
        HealthOptions::default()
    }

    /// A saved cursor to check.
    pub fn cursor(mut self, cursor: Cursor) -> HealthOptions {
        self.cursor = Some(cursor);
        self
    }

    /// How long the journal should cover, which is the longest a reader can be
    /// stopped and still catch up from the journal alone.
    pub fn required_history(mut self, history: Duration) -> HealthOptions {
        self.required_history = history;
        self
    }
}
