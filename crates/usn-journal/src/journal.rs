use std::fmt;
use std::str::FromStr;

use crate::error::{Error, Result};
use crate::record::{i64_at, u16_at, u64_at};
use crate::types::{JournalId, Usn};

/// The state of a volume's journal.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct JournalInfo {
    /// This instance of the journal.
    pub id: JournalId,
    /// The oldest record still available.
    pub first_usn: Usn,
    /// Where the next record will be written.
    pub next_usn: Usn,
    /// The first record ever written to this instance of the journal.
    pub lowest_valid_usn: Usn,
    /// The largest position this journal can reach before it must be recreated.
    pub max_usn: Usn,
    /// The size Windows trims the journal back to, in bytes.
    pub maximum_size: u64,
    /// How much is trimmed or added at a time, in bytes.
    pub allocation_delta: u64,
    /// The lowest record version this journal can return.
    pub min_version: u16,
    /// The highest record version this journal can return.
    pub max_version: u16,
}

impl JournalInfo {
    /// Parses the output of `FSCTL_QUERY_USN_JOURNAL`.
    pub(crate) fn parse(bytes: &[u8]) -> Result<JournalInfo> {
        if bytes.len() < 56 {
            return Err(Error::Corrupt(format!(
                "journal data of {} bytes",
                bytes.len()
            )));
        }
        // The first version of this structure ends before the record versions.
        let has_versions = bytes.len() >= 60;
        Ok(JournalInfo {
            id: JournalId(u64_at(bytes, 0)),
            first_usn: Usn(i64_at(bytes, 8)),
            next_usn: Usn(i64_at(bytes, 16)),
            lowest_valid_usn: Usn(i64_at(bytes, 24)),
            max_usn: Usn(i64_at(bytes, 32)),
            maximum_size: u64_at(bytes, 40),
            allocation_delta: u64_at(bytes, 48),
            min_version: if has_versions { u16_at(bytes, 56) } else { 2 },
            max_version: if has_versions { u16_at(bytes, 58) } else { 2 },
        })
    }

    /// The oldest position that can be read without losing records.
    pub fn oldest_readable(&self) -> Usn {
        self.first_usn.max(self.lowest_valid_usn)
    }

    /// The current end of the journal.
    pub fn end_cursor(&self) -> Cursor {
        Cursor {
            journal: self.id,
            usn: self.next_usn,
        }
    }

    /// Why `cursor` cannot be read from, if it cannot.
    pub fn check(&self, cursor: Cursor) -> Option<Gap> {
        if cursor.journal != self.id {
            return Some(Gap::JournalRecreated {
                old: cursor.journal,
                new: self.id,
            });
        }
        if cursor.usn < self.oldest_readable() || cursor.usn > self.next_usn {
            return Some(Gap::RecordsLost {
                cursor: cursor.usn,
                oldest_readable: self.oldest_readable(),
            });
        }
        None
    }
}

/// A resumable position in one instance of a journal. Store it as text with
/// `to_string`, and read it back with `parse`.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash)]
pub struct Cursor {
    /// The journal this position belongs to.
    pub journal: JournalId,
    /// The position.
    pub usn: Usn,
}

impl fmt::Display for Cursor {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}:{}", self.journal, self.usn)
    }
}

impl FromStr for Cursor {
    type Err = Error;

    fn from_str(text: &str) -> Result<Cursor> {
        let invalid = || Error::InvalidCursor(text.to_string());
        let (journal, usn) = text.split_once(':').ok_or_else(invalid)?;
        Ok(Cursor {
            journal: JournalId(u64::from_str_radix(journal, 16).map_err(|_| invalid())?),
            usn: Usn(usn.parse().map_err(|_| invalid())?),
        })
    }
}

/// Where reading starts.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum Start {
    /// Only changes from now on.
    Now,
    /// The oldest record the journal still has.
    Oldest,
    /// A saved position.
    At(Cursor),
}

/// Records went unrecorded or were purged, so the journal alone can no longer
/// say what changed in that period. List the volume again to catch up.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum Gap {
    /// The journal was deleted and recreated. Nothing was recorded in between.
    JournalRecreated {
        /// The journal the position belonged to.
        old: JournalId,
        /// The journal that exists now.
        new: JournalId,
    },
    /// The journal was trimmed past the position before it was read.
    RecordsLost {
        /// The position that was asked for.
        cursor: Usn,
        /// The oldest position still available.
        oldest_readable: Usn,
    },
}

impl fmt::Display for Gap {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Gap::JournalRecreated { old, new } => {
                write!(f, "journal recreated: id {old} is now {new}")
            }
            Gap::RecordsLost {
                cursor,
                oldest_readable,
            } => write!(
                f,
                "records lost: position {cursor}, oldest readable is {oldest_readable}"
            ),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn info() -> JournalInfo {
        JournalInfo {
            id: JournalId(7),
            first_usn: Usn(1000),
            next_usn: Usn(5000),
            lowest_valid_usn: Usn(0),
            max_usn: Usn(i64::MAX),
            maximum_size: 0,
            allocation_delta: 0,
            min_version: 2,
            max_version: 3,
        }
    }

    #[test]
    fn parses_v0_and_v2_query_results() {
        let mut b = vec![0u8; 80];
        b[0..8].copy_from_slice(&7u64.to_le_bytes());
        b[8..16].copy_from_slice(&1000i64.to_le_bytes());
        b[16..24].copy_from_slice(&5000i64.to_le_bytes());
        b[40..48].copy_from_slice(&32u64.to_le_bytes());
        b[56..58].copy_from_slice(&2u16.to_le_bytes());
        b[58..60].copy_from_slice(&3u16.to_le_bytes());
        let v2 = JournalInfo::parse(&b).unwrap();
        assert_eq!(
            (v2.id, v2.first_usn, v2.next_usn, v2.maximum_size),
            (JournalId(7), Usn(1000), Usn(5000), 32)
        );
        assert_eq!(v2.max_version, 3);
        assert_eq!(JournalInfo::parse(&b[..56]).unwrap().max_version, 2);
        assert!(JournalInfo::parse(&b[..55]).is_err());
    }

    #[test]
    fn end_cursor_is_the_next_position_in_this_journal() {
        assert_eq!(
            info().end_cursor(),
            Cursor {
                journal: JournalId(7),
                usn: Usn(5000)
            }
        );
    }

    #[test]
    fn cursor_within_the_journal_has_no_gap() {
        for usn in [1000, 3000, 5000] {
            assert_eq!(
                info().check(Cursor {
                    journal: JournalId(7),
                    usn: Usn(usn)
                }),
                None
            );
        }
    }

    #[test]
    fn cursor_from_another_journal_is_a_gap() {
        assert_eq!(
            info().check(Cursor {
                journal: JournalId(8),
                usn: Usn(3000)
            }),
            Some(Gap::JournalRecreated {
                old: JournalId(8),
                new: JournalId(7)
            })
        );
    }

    #[test]
    fn cursor_older_than_the_oldest_record_is_a_gap() {
        assert_eq!(
            info().check(Cursor {
                journal: JournalId(7),
                usn: Usn(999)
            }),
            Some(Gap::RecordsLost {
                cursor: Usn(999),
                oldest_readable: Usn(1000)
            })
        );
    }

    #[test]
    fn cursor_past_the_end_of_the_journal_is_a_gap() {
        assert!(matches!(
            info().check(Cursor {
                journal: JournalId(7),
                usn: Usn(5001)
            }),
            Some(Gap::RecordsLost { .. })
        ));
    }

    #[test]
    fn lowest_valid_usn_also_bounds_the_readable_range() {
        let i = JournalInfo {
            lowest_valid_usn: Usn(2000),
            ..info()
        };
        assert_eq!(i.oldest_readable(), Usn(2000));
        assert!(
            i.check(Cursor {
                journal: JournalId(7),
                usn: Usn(1500)
            })
            .is_some()
        );
    }

    #[test]
    fn cursor_round_trips_through_text_including_large_ids() {
        let c = Cursor {
            journal: JournalId(u64::MAX),
            usn: Usn(123_456_789),
        };
        assert_eq!(c.to_string().parse::<Cursor>().unwrap(), c);
        for bad in ["", "abc", "zz:1", "1:zz", "1:"] {
            assert!(bad.parse::<Cursor>().is_err(), "{bad:?} should not parse");
        }
    }

    #[test]
    fn parsing_never_panics_at_any_length() {
        let bytes = [0xFFu8; 80];
        for length in 0..=bytes.len() {
            assert_eq!(JournalInfo::parse(&bytes[..length]).is_ok(), length >= 56);
        }
    }

    #[test]
    fn the_middle_version_of_the_query_result_has_record_versions() {
        let mut b = vec![0u8; 60];
        b[56..58].copy_from_slice(&2u16.to_le_bytes());
        b[58..60].copy_from_slice(&4u16.to_le_bytes());
        assert_eq!(JournalInfo::parse(&b).unwrap().max_version, 4);
    }

    #[test]
    fn a_bad_cursor_is_an_invalid_cursor_error() {
        assert!(matches!(
            "nonsense".parse::<Cursor>(),
            Err(Error::InvalidCursor(_))
        ));
    }
}
