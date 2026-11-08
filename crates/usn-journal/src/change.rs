use std::fmt;
use std::path::PathBuf;

use crate::journal::{Cursor, Gap};
use crate::types::{FileAttributes, FileId, FileTime, Reason, ReparseTag, Usn};

/// What happened to a file, merged across its journal records. This enum is
/// deliberately exhaustive, so that handling it in a `match` stays complete.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash)]
pub enum ChangeKind {
    /// It appeared. This includes a move from outside the filter, and a move
    /// from a location that could no longer be determined.
    Created,
    /// Its contents changed. It may also have been renamed.
    Modified,
    /// Only its path changed. [`FileChange::previous_path`] is always set,
    /// except on a volume opened without elevation, which has no paths at all.
    Renamed,
    /// Only its attributes, permissions or similar changed.
    MetadataChanged,
    /// It is gone, or moved outside the filter.
    Deleted,
}

impl fmt::Display for ChangeKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            ChangeKind::Created => "created",
            ChangeKind::Modified => "modified",
            ChangeKind::Renamed => "renamed",
            ChangeKind::MetadataChanged => "metadata_changed",
            ChangeKind::Deleted => "deleted",
        })
    }
}

/// One file or directory that changed, with its journal records merged.
///
/// Treat this as a prompt to look at the file. It may have changed again since,
/// and delivery is at least once, so the same change can be reported twice.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct FileChange {
    /// What happened.
    pub kind: ChangeKind,
    /// The current path, or for a deleted file its last known path. `None` if
    /// it could not be determined. Identify files by [`FileChange::id`].
    pub path: Option<PathBuf>,
    /// The earlier path, if it was renamed or moved. Moving a directory also
    /// moves everything under it, and those files get no changes of their own.
    pub previous_path: Option<PathBuf>,
    /// True if this was reported while the file was still open. It will be
    /// reported again when it closes.
    pub still_open: bool,
    /// The file or directory, which is stable across renames.
    pub id: FileId,
    /// The directory holding it.
    pub parent_id: FileId,
    /// Every reason seen across the merged records.
    pub reasons: Reason,
    /// Its attributes as of the latest record.
    pub attributes: FileAttributes,
    /// The kind of reparse point, if it is one and that could be read.
    pub reparse_tag: Option<ReparseTag>,
    /// The first merged record.
    pub first_usn: Usn,
    /// The last merged record.
    pub last_usn: Usn,
    /// When the last merged record was written.
    pub timestamp: FileTime,
}

impl FileChange {
    /// True if this is a directory.
    pub fn is_directory(&self) -> bool {
        self.attributes.contains(FileAttributes::DIRECTORY)
    }

    /// True for reparse points of any kind, including ones that are not links.
    pub fn is_reparse_point(&self) -> bool {
        self.attributes.contains(FileAttributes::REPARSE_POINT)
    }

    /// True for symbolic links, junctions and mounted folders.
    pub fn is_link(&self) -> bool {
        self.reparse_tag.is_some_and(ReparseTag::is_link)
    }

    /// True if the file was created or modified and has contents of its own.
    /// Directories and links do not. Other reparse points do, including one
    /// whose kind could not be read.
    pub fn has_new_content(&self) -> bool {
        matches!(self.kind, ChangeKind::Created | ChangeKind::Modified)
            && !self.is_directory()
            && !self.is_link()
    }
}

/// The result of one read of the journal.
#[derive(Clone, Debug)]
#[non_exhaustive]
pub struct ChangeBatch {
    /// The changes, with deletes first, so that applying them in order leaves
    /// the right file in place when one file replaces another at a path.
    pub changes: Vec<FileChange>,
    /// Set if records were lost before this batch. List the volume again before
    /// saving `cursor`, or a crash would forget that it was needed.
    pub gap: Option<Gap>,
    /// Save this once the batch is safely handled, and resume from it to lose
    /// nothing. It never passes a change that has not been reported.
    pub cursor: Cursor,
    /// True if all available records were read.
    pub caught_up: bool,
    /// Journal bytes written but not yet read.
    pub lag_bytes: u64,
    /// Journal bytes that can be trimmed before `cursor` is lost.
    pub headroom_bytes: u64,
}
