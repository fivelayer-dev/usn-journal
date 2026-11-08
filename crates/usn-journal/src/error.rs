use std::fmt;
use std::io;

use crate::journal::Gap;

/// The result type used throughout this crate.
pub type Result<T> = std::result::Result<T, Error>;

/// Why an operation failed.
#[derive(Debug)]
#[non_exhaustive]
pub enum Error {
    /// The process lacks the rights for this operation. Run it elevated.
    AccessDenied,
    /// The volume has no active journal. This crate never creates one implicitly.
    JournalNotActive,
    /// The journal is being deleted, and cannot be read until that finishes.
    JournalDeleteInProgress,
    /// A raw read cannot continue from its position. Change readers report this
    /// in [`ChangeBatch::gap`](crate::ChangeBatch::gap) instead.
    Gap(Gap),
    /// The file system has no journal, such as FAT, exFAT or a network share.
    Unsupported,
    /// The volume name could not be understood.
    InvalidVolume(String),
    /// The text is not a cursor written by this crate.
    InvalidCursor(String),
    /// The kernel returned data that does not parse as journal records.
    Corrupt(String),
    /// Any other operating system error.
    Io(io::Error),
}

impl Error {
    /// True if retrying later could succeed, for example after a volume that
    /// went away comes back.
    pub fn is_transient(&self) -> bool {
        matches!(self, Error::JournalDeleteInProgress | Error::Io(_))
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::AccessDenied => f.write_str("access denied: run this process elevated"),
            Error::JournalNotActive => f.write_str("the volume has no active USN journal"),
            Error::JournalDeleteInProgress => f.write_str("the USN journal is being deleted"),
            Error::Gap(gap) => write!(f, "{gap}"),
            Error::Unsupported => f.write_str("this file system has no USN journal"),
            Error::InvalidVolume(name) => write!(f, "invalid volume name: {name}"),
            Error::InvalidCursor(text) => write!(f, "invalid cursor: {text:?}"),
            Error::Corrupt(detail) => write!(f, "corrupt USN data: {detail}"),
            Error::Io(error) => write!(f, "{error}"),
        }
    }
}

impl std::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Error::Io(error) => Some(error),
            _ => None,
        }
    }
}

impl From<io::Error> for Error {
    fn from(error: io::Error) -> Self {
        Error::Io(error)
    }
}
