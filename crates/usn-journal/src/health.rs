use std::fmt;
use std::time::{Duration, SystemTime};

use crate::engine::READ_BUFFER_BYTES;
use crate::error::{Error, Result};
use crate::journal::{Cursor, Gap, JournalInfo};
use crate::options::HealthOptions;
use crate::record::split_buffer;
use crate::source::Source;
use crate::types::{FileTime, Usn};

const PURGE_RETRIES: usize = 3;
/// A journal with less active time that has purged nothing says too little to
/// estimate a rate.
const MIN_RATE_SAMPLE: Duration = Duration::from_secs(3600);
/// A silence longer than this counts as the machine being off. Windows writes
/// records often enough that this mostly catches real downtime. An idle machine
/// that goes quiet for longer only makes the estimate more cautious.
const MAX_ACTIVE_GAP: Duration = Duration::from_secs(600);

/// How urgently an issue needs attention.
#[derive(Copy, Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum HealthStatus {
    /// Nothing needs attention.
    Ok,
    /// Still working, but a gap is likely if nothing changes.
    Warning,
    /// The saved cursor is already lost.
    Critical,
}

impl fmt::Display for HealthStatus {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            HealthStatus::Ok => "ok",
            HealthStatus::Warning => "warning",
            HealthStatus::Critical => "critical",
        })
    }
}

/// A problem found by a journal health check.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum Issue {
    /// The cursor can no longer be read from.
    CursorLost(Gap),
    /// The cursor is close to being trimmed from the journal.
    CursorAtRisk {
        /// Roughly how many more bytes can be written first.
        bytes_until_lost: u64,
        /// The same at the current write rate, if that is known.
        time_until_lost: Option<Duration>,
    },
    /// At the current rate the journal holds less history than required.
    ShortHistory {
        /// How much it holds.
        estimated: Duration,
        /// How much was asked for.
        required: Duration,
    },
    /// The journal must be recreated before it reaches its maximum position.
    UsnNearLimit {
        /// How many bytes remain.
        remaining_bytes: u64,
    },
}

impl Issue {
    /// How urgent this issue is.
    pub fn severity(&self) -> HealthStatus {
        match self {
            Issue::CursorLost(_) => HealthStatus::Critical,
            _ => HealthStatus::Warning,
        }
    }

    /// A stable name for this kind of issue.
    pub fn kind(&self) -> &'static str {
        match self {
            Issue::CursorLost(_) => "cursor_lost",
            Issue::CursorAtRisk { .. } => "cursor_at_risk",
            Issue::ShortHistory { .. } => "short_history",
            Issue::UsnNearLimit { .. } => "usn_near_limit",
        }
    }
}

impl fmt::Display for Issue {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Issue::CursorLost(gap) => write!(f, "cursor lost: {gap}"),
            Issue::CursorAtRisk {
                bytes_until_lost,
                time_until_lost: Some(time),
            } => write!(
                f,
                "cursor will be purged after about {bytes_until_lost} more bytes, or {:.1} hours at the current rate",
                hours(*time)
            ),
            Issue::CursorAtRisk {
                bytes_until_lost, ..
            } => write!(
                f,
                "cursor will be purged after about {bytes_until_lost} more bytes"
            ),
            Issue::ShortHistory {
                estimated,
                required,
            } => write!(
                f,
                "journal holds about {:.1} hours of history at the current rate, but {:.1} are required",
                hours(*estimated),
                hours(*required)
            ),
            Issue::UsnNearLimit { remaining_bytes } => write!(
                f,
                "only {remaining_bytes} bytes remain before the journal reaches its maximum USN"
            ),
        }
    }
}

fn hours(duration: Duration) -> f64 {
    duration.as_secs_f64() / 3600.0
}

/// The state of a saved cursor.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct CursorHealth {
    /// The cursor that was checked.
    pub cursor: Cursor,
    /// Set if it can no longer be read from.
    pub gap: Option<Gap>,
    /// Journal bytes written since the cursor.
    pub lag_bytes: u64,
    /// Roughly how many more bytes can be written before it may be trimmed.
    pub bytes_until_lost: u64,
    /// The same at the current write rate, if that is known.
    pub time_until_lost: Option<Duration>,
}

/// How much history a journal holds, and whether a reader can rely on it.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct JournalHealth {
    /// The journal's state.
    pub info: JournalInfo,
    /// True if it has trimmed records, so `history` is what it can really hold.
    /// Otherwise `history` is only its age.
    pub has_purged: bool,
    /// When the oldest record was written, or `None` if it is empty.
    pub oldest_record: Option<FileTime>,
    /// How far back it reaches. `None` if it is empty or the clock is behind
    /// its oldest record.
    pub history: Option<Duration>,
    /// The part of `history` when records were being written, leaving out any
    /// silence longer than ten minutes.
    pub active_history: Duration,
    /// The average write rate over `active_history`, if there is enough to go on.
    pub bytes_per_hour: Option<u64>,
    /// How much running time `maximum_size` holds at that rate. A burst of
    /// activity uses it up faster, so leave a margin.
    pub estimated_capacity: Option<Duration>,
    /// The history that was asked for.
    pub required_history: Duration,
    /// The cursor's state, if one was given.
    pub cursor: Option<CursorHealth>,
    /// Every problem found.
    pub issues: Vec<Issue>,
}

impl JournalHealth {
    /// The most severe issue found.
    pub fn status(&self) -> HealthStatus {
        self.issues
            .iter()
            .map(Issue::severity)
            .max()
            .unwrap_or(HealthStatus::Ok)
    }
}

pub(crate) fn check<S: Source>(
    source: &mut S,
    options: &HealthOptions,
    now: SystemTime,
) -> Result<JournalHealth> {
    let (info, span) = query_with_span(source)?;
    let has_purged = info.first_usn > info.lowest_valid_usn;
    let oldest_record = span.oldest;
    let history = oldest_record
        .and_then(FileTime::to_system_time)
        .and_then(|time| now.duration_since(time).ok());
    let active_history = span.active;
    let retained = info.next_usn.0 - info.oldest_readable().0;
    let enough = has_purged || active_history >= MIN_RATE_SAMPLE;
    let bytes_per_second = (enough && retained > 0 && !active_history.is_zero())
        .then(|| retained as f64 / active_history.as_secs_f64());
    let time_for = |bytes: u64| {
        bytes_per_second.and_then(|rate| Duration::try_from_secs_f64(bytes as f64 / rate).ok())
    };
    let estimated_capacity = time_for(info.maximum_size);
    let cursor = options.cursor.map(|cursor| {
        let gap = info.check(cursor);
        let lag_bytes = if gap.is_some() {
            0
        } else {
            (info.next_usn.0 - cursor.usn.0).max(0) as u64
        };
        let bytes_until_lost = if gap.is_some() {
            0
        } else {
            info.maximum_size.saturating_sub(lag_bytes)
        };
        CursorHealth {
            cursor,
            gap,
            lag_bytes,
            bytes_until_lost,
            time_until_lost: time_for(bytes_until_lost),
        }
    });

    let mut issues = Vec::new();
    if let Some(c) = &cursor {
        match c.gap {
            Some(gap) => issues.push(Issue::CursorLost(gap)),
            None if c.bytes_until_lost < info.maximum_size / 4 => {
                issues.push(Issue::CursorAtRisk {
                    bytes_until_lost: c.bytes_until_lost,
                    time_until_lost: c.time_until_lost,
                })
            }
            None => {}
        }
    }
    if let Some(estimated) = estimated_capacity {
        if estimated < options.required_history {
            issues.push(Issue::ShortHistory {
                estimated,
                required: options.required_history,
            });
        }
    }
    let remaining_bytes = (info.max_usn.0 - info.next_usn.0).max(0);
    if remaining_bytes < info.max_usn.0 / 64 {
        issues.push(Issue::UsnNearLimit {
            remaining_bytes: remaining_bytes as u64,
        });
    }

    Ok(JournalHealth {
        info,
        has_purged,
        oldest_record,
        history,
        active_history,
        bytes_per_hour: bytes_per_second.map(|rate| (rate * 3600.0) as u64),
        estimated_capacity,
        required_history: options.required_history,
        cursor,
        issues,
    })
}

/// When the journal's records were written.
struct Span {
    oldest: Option<FileTime>,
    active: Duration,
}

/// Queries the journal and walks its records, starting again if the start of the
/// journal is purged in between.
fn query_with_span<S: Source>(source: &mut S) -> Result<(JournalInfo, Span)> {
    let mut buffer = vec![0; READ_BUFFER_BYTES];
    let mut retries = 0;
    loop {
        let info = source.query()?;
        match read_span(source, &info, &mut buffer) {
            Err(Error::Gap(_)) if retries < PURGE_RETRIES => retries += 1,
            Ok(span) => return Ok((info, span)),
            Err(e) => return Err(e),
        }
    }
}

/// Reads up to the end of the journal as queried, so that a busy volume can't keep
/// this going.
fn read_span<S: Source>(source: &mut S, info: &JournalInfo, buffer: &mut [u8]) -> Result<Span> {
    let max_gap = MAX_ACTIVE_GAP.as_nanos() as i64 / 100;
    let mut oldest = None;
    let mut previous: Option<FileTime> = None;
    let mut active = 0i64;
    let mut position: Usn = info.oldest_readable();
    while position < info.next_usn {
        let n = source.read(position, info.id, buffer)?;
        let (next, records) = split_buffer(&buffer[..n])?;
        for record in records {
            let record = record?;
            if record.usn >= info.next_usn {
                break;
            }
            oldest.get_or_insert(record.timestamp);
            if let Some(previous) = previous {
                let gap = record.timestamp.0.saturating_sub(previous.0);
                if gap > 0 && gap <= max_gap {
                    active = active.saturating_add(gap);
                }
            }
            previous = Some(record.timestamp);
        }
        // A buffer can hold only record versions that are skipped, so keep going
        // until the journal stops advancing.
        if next <= position {
            break;
        }
        position = next;
    }
    let active = active as u64;
    Ok(Span {
        oldest,
        active: Duration::new(active / 10_000_000, (active % 10_000_000) as u32 * 100),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::FakeJournal;
    use crate::testing::v2;
    use crate::types::JournalId;

    /// The timestamp of every record that `v2` builds.
    const RECORD_TIME: i64 = 133_000_000_000_000_000;
    const HOUR: Duration = Duration::from_secs(3600);
    const FIVE_MINUTES: Duration = Duration::from_secs(300);

    fn after_record(elapsed: Duration) -> SystemTime {
        FileTime(RECORD_TIME).to_system_time().unwrap() + elapsed
    }

    fn filetime_after(elapsed: Duration) -> i64 {
        RECORD_TIME + (elapsed.as_nanos() / 100) as i64
    }

    fn append_at(journal: &mut FakeJournal, time: i64) {
        journal.append(|usn| {
            let mut record = v2(usn, 7, 5, 1, 0x20, "file");
            record[32..40].copy_from_slice(&time.to_le_bytes());
            record
        });
    }

    /// Appends a record every five minutes for `active`, from `start`.
    fn append_running(journal: &mut FakeJournal, start: Duration, active: Duration) {
        let mut elapsed = Duration::ZERO;
        while elapsed <= active {
            append_at(journal, filetime_after(start + elapsed));
            elapsed += FIVE_MINUTES;
        }
    }

    fn purged_start() -> FakeJournal {
        let mut journal = FakeJournal::new();
        journal.info.first_usn = Usn(1000);
        journal.info.next_usn = Usn(1000);
        journal
    }

    /// A journal that has purged its start and now retains `retained` bytes,
    /// written over `active` with the machine on throughout.
    fn purged_journal_over(retained: i64, maximum_size: u64, active: Duration) -> FakeJournal {
        let mut journal = purged_start();
        journal.info.maximum_size = maximum_size;
        append_running(&mut journal, Duration::ZERO, active);
        journal.info.next_usn = Usn(1000 + retained);
        journal
    }

    /// The same, written over ten hours.
    fn purged_journal(retained: i64, maximum_size: u64) -> FakeJournal {
        purged_journal_over(retained, maximum_size, 10 * HOUR)
    }

    fn check(journal: &mut FakeJournal, elapsed: Duration) -> JournalHealth {
        super::check(journal, &HealthOptions::new(), after_record(elapsed)).unwrap()
    }

    fn with_cursor(journal: &mut FakeJournal, usn: i64, elapsed: Duration) -> JournalHealth {
        let options = HealthOptions::new().cursor(Cursor {
            journal: JournalId(7),
            usn: Usn(usn),
        });
        super::check(journal, &options, after_record(elapsed)).unwrap()
    }

    #[test]
    fn measures_history_and_rate_from_the_oldest_record() {
        let mut journal = purged_journal(10_000_000, 80_000_000);
        let health = check(&mut journal, 10 * HOUR);
        assert!(health.has_purged);
        assert_eq!(health.oldest_record, Some(FileTime(RECORD_TIME)));
        assert_eq!(health.history, Some(10 * HOUR));
        assert_eq!(health.active_history, 10 * HOUR);
        assert_eq!(health.bytes_per_hour, Some(1_000_000));
        assert_eq!(health.estimated_capacity, Some(80 * HOUR));
        assert_eq!(health.status(), HealthStatus::Ok);
    }

    #[test]
    fn time_the_machine_was_off_does_not_lower_the_rate() {
        // Two hours running, five days off, then two more hours running.
        let mut journal = purged_start();
        journal.info.maximum_size = 8_000_000;
        append_running(&mut journal, Duration::ZERO, 2 * HOUR);
        append_running(&mut journal, 122 * HOUR, 2 * HOUR);
        journal.info.next_usn = Usn(1000 + 4_000_000);
        let health = check(&mut journal, 124 * HOUR);
        assert_eq!(health.history, Some(124 * HOUR));
        assert_eq!(health.active_history, 4 * HOUR);
        assert_eq!(health.bytes_per_hour, Some(1_000_000));
        assert_eq!(health.estimated_capacity, Some(8 * HOUR));
        assert!(matches!(health.issues[0], Issue::ShortHistory { .. }));
    }

    #[test]
    fn a_silence_of_exactly_the_limit_still_counts_as_running() {
        let mut journal = purged_start();
        append_at(&mut journal, filetime_after(Duration::ZERO));
        append_at(&mut journal, filetime_after(MAX_ACTIVE_GAP));
        append_at(
            &mut journal,
            filetime_after(MAX_ACTIVE_GAP * 2 + Duration::from_secs(1)),
        );
        assert_eq!(check(&mut journal, HOUR).active_history, MAX_ACTIVE_GAP);
    }

    #[test]
    fn a_clock_change_between_records_is_not_counted() {
        let mut journal = purged_start();
        append_at(&mut journal, filetime_after(HOUR));
        append_at(&mut journal, filetime_after(Duration::ZERO));
        append_at(&mut journal, filetime_after(FIVE_MINUTES));
        assert_eq!(check(&mut journal, HOUR).active_history, FIVE_MINUTES);
    }

    #[test]
    fn stops_at_the_end_of_the_journal_as_queried() {
        let mut journal = purged_start();
        append_at(&mut journal, filetime_after(Duration::ZERO));
        append_at(&mut journal, filetime_after(FIVE_MINUTES));
        let end = journal.info.next_usn;
        append_at(&mut journal, filetime_after(FIVE_MINUTES * 2));
        journal.info.next_usn = end;
        assert_eq!(check(&mut journal, HOUR).active_history, FIVE_MINUTES);
    }

    #[test]
    fn an_old_journal_that_rarely_ran_has_no_rate_yet() {
        let mut journal = FakeJournal::new();
        append_running(&mut journal, Duration::ZERO, HOUR / 2);
        let health = check(&mut journal, 100 * HOUR);
        assert_eq!(health.history, Some(100 * HOUR));
        assert_eq!(health.bytes_per_hour, None);
    }

    #[test]
    fn a_journal_that_ran_for_an_hour_has_a_rate_before_it_purges() {
        let mut journal = FakeJournal::new();
        append_running(&mut journal, Duration::ZERO, HOUR);
        let health = check(&mut journal, HOUR);
        assert!(!health.has_purged);
        assert!(health.bytes_per_hour.is_some());
    }

    #[test]
    fn warns_when_the_journal_holds_too_little_history() {
        let mut journal = purged_journal(10_000_000, 10_000_000);
        let health = check(&mut journal, 10 * HOUR);
        assert_eq!(
            health.issues,
            [Issue::ShortHistory {
                estimated: 10 * HOUR,
                required: 24 * HOUR,
            }]
        );
        assert_eq!(health.status(), HealthStatus::Warning);
    }

    #[test]
    fn the_required_history_is_configurable() {
        let mut journal = purged_journal(10_000_000, 10_000_000);
        let options = HealthOptions::new().required_history(5 * HOUR);
        let health = super::check(&mut journal, &options, after_record(10 * HOUR)).unwrap();
        assert_eq!(health.status(), HealthStatus::Ok);
    }

    #[test]
    fn a_young_journal_has_no_rate_and_no_warning() {
        let mut journal = FakeJournal::new();
        journal.append(|usn| v2(usn, 7, 5, 1, 0x20, "first"));
        let health = check(&mut journal, HOUR / 6);
        assert!(!health.has_purged);
        assert_eq!(health.history, Some(HOUR / 6));
        assert_eq!(health.bytes_per_hour, None);
        assert_eq!(health.estimated_capacity, None);
        assert_eq!(health.status(), HealthStatus::Ok);
    }

    #[test]
    fn a_journal_that_purges_within_the_hour_still_has_a_rate() {
        let mut journal = purged_journal_over(1_000_000, 1_000_000, HOUR / 6);
        let health = check(&mut journal, HOUR / 6);
        assert_eq!(health.estimated_capacity, Some(HOUR / 6));
        assert_eq!(health.status(), HealthStatus::Warning);
    }

    #[test]
    fn an_empty_journal_has_no_history() {
        let mut journal = FakeJournal::new();
        let health = check(&mut journal, HOUR);
        assert_eq!(health.oldest_record, None);
        assert_eq!(health.history, None);
        assert_eq!(health.status(), HealthStatus::Ok);
    }

    #[test]
    fn a_clock_behind_the_oldest_record_gives_no_history() {
        let mut journal = purged_journal(1_000_000, 1_000_000);
        let now = FileTime(RECORD_TIME).to_system_time().unwrap() - HOUR;
        let health = super::check(&mut journal, &HealthOptions::new(), now).unwrap();
        assert_eq!(health.history, None);
        // The rate comes from the records alone, so it doesn't need the clock.
        assert!(health.bytes_per_hour.is_some());
    }

    #[test]
    fn skips_records_of_other_versions_to_find_the_oldest() {
        let mut journal = FakeJournal::new();
        journal.append(|_| {
            let mut v4 = vec![0u8; 16];
            v4[0..4].copy_from_slice(&16u32.to_le_bytes());
            v4[4..6].copy_from_slice(&4u16.to_le_bytes());
            v4
        });
        journal.append(|usn| v2(usn, 7, 5, 1, 0x20, "first"));
        assert_eq!(
            check(&mut journal, HOUR).oldest_record,
            Some(FileTime(RECORD_TIME))
        );
    }

    #[test]
    fn starts_again_if_the_oldest_record_is_purged_while_reading_it() {
        let mut journal = purged_journal(1_000_000, 80_000_000);
        journal.read_errors.push_back(Error::Gap(Gap::RecordsLost {
            cursor: Usn(0),
            oldest_readable: Usn(0),
        }));
        assert_eq!(
            check(&mut journal, HOUR).oldest_record,
            Some(FileTime(RECORD_TIME))
        );
    }

    #[test]
    fn gives_up_if_purges_keep_winning_the_race() {
        let mut journal = purged_journal(1_000_000, 80_000_000);
        for _ in 0..=PURGE_RETRIES {
            journal.read_errors.push_back(Error::Gap(Gap::RecordsLost {
                cursor: Usn(0),
                oldest_readable: Usn(0),
            }));
        }
        let result = super::check(&mut journal, &HealthOptions::new(), after_record(HOUR));
        assert!(matches!(result, Err(Error::Gap(_))));
    }

    #[test]
    fn a_missing_journal_is_an_error() {
        let mut journal = FakeJournal::new();
        journal.query_errors.push_back(Error::JournalNotActive);
        let result = super::check(&mut journal, &HealthOptions::new(), after_record(HOUR));
        assert!(matches!(result, Err(Error::JournalNotActive)));
    }

    #[test]
    fn a_recent_cursor_is_healthy_and_says_how_long_it_has() {
        let mut journal = purged_journal(10_000_000, 80_000_000);
        // 1,000,000 bytes behind, at 1,000,000 bytes per hour.
        let health = with_cursor(&mut journal, 10_001_000 - 1_000_000, 10 * HOUR);
        let cursor = health.cursor.unwrap();
        assert_eq!(cursor.gap, None);
        assert_eq!(cursor.lag_bytes, 1_000_000);
        assert_eq!(cursor.bytes_until_lost, 79_000_000);
        assert_eq!(cursor.time_until_lost, Some(79 * HOUR));
        assert_eq!(health.status(), HealthStatus::Ok);
    }

    #[test]
    fn warns_when_a_cursor_is_close_to_being_purged() {
        let mut journal = purged_journal(10_000_000, 10_000_000);
        // Within the journal, but 9,000,000 of the 10,000,000 bytes behind.
        let health = with_cursor(&mut journal, 10_001_000 - 9_000_000, 10 * HOUR);
        assert!(health.issues.contains(&Issue::CursorAtRisk {
            bytes_until_lost: 1_000_000,
            time_until_lost: Some(HOUR),
        }));
    }

    #[test]
    fn a_cursor_past_the_maximum_size_has_nothing_left() {
        let mut journal = purged_journal(12_000_000, 10_000_000);
        let health = with_cursor(&mut journal, 2000, 10 * HOUR);
        let cursor = health.cursor.unwrap();
        assert_eq!(cursor.gap, None);
        assert_eq!(cursor.bytes_until_lost, 0);
    }

    #[test]
    fn a_purged_cursor_is_critical() {
        let mut journal = purged_journal(10_000_000, 80_000_000);
        let health = with_cursor(&mut journal, 999, 10 * HOUR);
        assert_eq!(health.status(), HealthStatus::Critical);
        assert!(matches!(health.issues[0], Issue::CursorLost(_)));
        assert_eq!(health.cursor.unwrap().bytes_until_lost, 0);
    }

    #[test]
    fn a_cursor_from_another_journal_is_critical() {
        let mut journal = purged_journal(10_000_000, 80_000_000);
        let options = HealthOptions::new().cursor(Cursor {
            journal: JournalId(8),
            usn: Usn(2000),
        });
        let health = super::check(&mut journal, &options, after_record(HOUR)).unwrap();
        assert_eq!(health.status(), HealthStatus::Critical);
    }

    #[test]
    fn warns_as_the_journal_nears_its_maximum_usn() {
        let mut journal = purged_journal(1_000_000, 80_000_000);
        journal.info.max_usn = Usn(journal.info.next_usn.0 + 1000);
        let health = check(&mut journal, HOUR);
        assert_eq!(
            health.issues,
            [Issue::UsnNearLimit {
                remaining_bytes: 1000
            }]
        );
    }

    #[test]
    fn the_most_severe_issue_sets_the_status() {
        let mut journal = purged_journal(10_000_000, 10_000_000);
        let health = with_cursor(&mut journal, 999, 10 * HOUR);
        assert_eq!(health.issues.len(), 2);
        assert_eq!(health.status(), HealthStatus::Critical);
    }

    #[test]
    fn describes_each_issue() {
        let short = Issue::ShortHistory {
            estimated: 10 * HOUR,
            required: 24 * HOUR,
        };
        assert_eq!(short.kind(), "short_history");
        assert_eq!(
            short.to_string(),
            "journal holds about 10.0 hours of history at the current rate, but 24.0 are required"
        );
        let risk = Issue::CursorAtRisk {
            bytes_until_lost: 5,
            time_until_lost: None,
        };
        assert_eq!(
            risk.to_string(),
            "cursor will be purged after about 5 more bytes"
        );
    }

    #[test]
    fn extreme_timestamps_never_panic() {
        let mut journal = purged_start();
        for time in [i64::MIN, 0, i64::MAX, -1, i64::MAX] {
            append_at(&mut journal, time);
        }
        let health = check(&mut journal, HOUR);
        assert_eq!(health.oldest_record, Some(FileTime(i64::MIN)));
        assert_eq!(health.history, None);
    }

    #[test]
    fn status_names_are_stable() {
        assert_eq!(HealthStatus::Ok.to_string(), "ok");
        assert_eq!(HealthStatus::Warning.to_string(), "warning");
        assert_eq!(HealthStatus::Critical.to_string(), "critical");
    }
}
