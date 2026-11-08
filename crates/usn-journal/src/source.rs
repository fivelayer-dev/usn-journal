use std::ffi::OsStr;
use std::path::PathBuf;

use crate::error::Result;
use crate::journal::JournalInfo;
use crate::types::{FileId, JournalId, ReparseTag, Usn};

/// Where records come from. The real implementation is the volume. This trait
/// exists so that everything above it can be tested without a real journal.
pub(crate) trait Source {
    fn query(&mut self) -> Result<JournalInfo>;

    /// Fills `buffer` with records from `start` without blocking, returning the
    /// byte count. The format is that of `FSCTL_READ_USN_JOURNAL`.
    fn read(&mut self, start: Usn, journal: JournalId, buffer: &mut [u8]) -> Result<usize>;

    /// The volume root's path, with its trailing separator, and its identity.
    fn root(&self) -> (PathBuf, FileId);

    /// False if records come with their names removed, so no paths can be built.
    fn has_names(&self) -> bool {
        true
    }

    /// The current full path of a directory the path cache does not know.
    fn lookup_dir(&mut self, _id: FileId) -> Option<PathBuf> {
        None
    }

    /// Reports every directory on the volume, to fill the path cache up front.
    fn for_each_directory(&mut self, _visit: &mut dyn FnMut(FileId, FileId, &OsStr)) -> Result<()> {
        Ok(())
    }

    /// The reparse tag of a file. The journal only says a file is a reparse
    /// point, not what kind, so this is asked for each one that is reported.
    fn reparse_tag(&mut self, _id: FileId) -> Option<ReparseTag> {
        None
    }

    /// Reopens the source after an error, for example a volume that went away.
    fn reconnect(&mut self) -> Result<()> {
        Ok(())
    }
}
