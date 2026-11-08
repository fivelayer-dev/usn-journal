use std::fmt;
use std::ops::{BitOr, BitOrAssign};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// A position in a journal, measured in bytes.
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Usn(pub i64);

impl fmt::Display for Usn {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// One instance of a journal. A new ID means the journal was deleted and
/// recreated, so nothing was recorded in between.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash)]
pub struct JournalId(pub u64);

impl fmt::Display for JournalId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:x}", self.0)
    }
}

/// A file's identity on its volume. It survives renames, and hard links to one
/// file share it. 64-bit IDs are zero-extended, so V2 and V3 records for the
/// same NTFS file compare equal.
#[derive(Copy, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct FileId(pub u128);

impl fmt::Debug for FileId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "FileId({:#x})", self.0)
    }
}

impl fmt::Display for FileId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:#x}", self.0)
    }
}

/// A Windows timestamp: 100 ns intervals since 1601-01-01 UTC.
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct FileTime(pub i64);

impl FileTime {
    const UNIX_EPOCH: i64 = 116_444_736_000_000_000;
    const PER_SECOND: u64 = 10_000_000;

    /// This time as a `SystemTime`, or `None` if it is out of range.
    pub fn to_system_time(self) -> Option<SystemTime> {
        let offset = self.0.checked_sub(Self::UNIX_EPOCH)?;
        let units = offset.unsigned_abs();
        let magnitude = Duration::new(
            units / Self::PER_SECOND,
            (units % Self::PER_SECOND) as u32 * 100,
        );
        if offset >= 0 {
            UNIX_EPOCH.checked_add(magnitude)
        } else {
            UNIX_EPOCH.checked_sub(magnitude)
        }
    }
}

macro_rules! flags {
    (
        $(#[$meta:meta])*
        pub struct $name:ident {
            $( $(#[$flag_meta:meta])* $flag:ident = $value:expr, )*
        }
    ) => {
        $(#[$meta])*
        #[derive(Copy, Clone, PartialEq, Eq, Hash, Default)]
        pub struct $name(pub u32);

        impl $name {
            $( $(#[$flag_meta])* pub const $flag: $name = $name($value); )*

            const NAMES: &'static [($name, &'static str)] =
                &[ $( ($name::$flag, stringify!($flag)), )* ];

            /// True if every flag in `other` is set.
            pub const fn contains(self, other: $name) -> bool {
                self.0 & other.0 == other.0
            }

            /// True if any flag in `other` is set.
            pub const fn intersects(self, other: $name) -> bool {
                self.0 & other.0 != 0
            }

            /// The names of the known flags that are set.
            pub fn names(self) -> impl Iterator<Item = &'static str> {
                Self::NAMES
                    .iter()
                    .filter(move |(flag, _)| self.intersects(*flag))
                    .map(|(_, name)| *name)
            }
        }

        impl BitOr for $name {
            type Output = $name;
            fn bitor(self, rhs: $name) -> $name {
                $name(self.0 | rhs.0)
            }
        }

        impl BitOrAssign for $name {
            fn bitor_assign(&mut self, rhs: $name) {
                self.0 |= rhs.0;
            }
        }

        impl fmt::Debug for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                let names = self.names().collect::<Vec<_>>().join("|");
                write!(f, "{}({names})", stringify!($name))
            }
        }
    };
}

flags! {
    /// Why a journal record was written (`USN_REASON_*`). Flags accumulate on a
    /// file until its last handle closes, so a record can carry flags for
    /// earlier changes as well as the latest one.
    pub struct Reason {
        /// The main data stream was overwritten.
        DATA_OVERWRITE = 0x0000_0001,
        /// The main data stream was extended.
        DATA_EXTEND = 0x0000_0002,
        /// The main data stream was truncated.
        DATA_TRUNCATION = 0x0000_0004,
        /// A named data stream was overwritten.
        NAMED_DATA_OVERWRITE = 0x0000_0010,
        /// A named data stream was extended.
        NAMED_DATA_EXTEND = 0x0000_0020,
        /// A named data stream was truncated.
        NAMED_DATA_TRUNCATION = 0x0000_0040,
        /// The file or directory was created.
        FILE_CREATE = 0x0000_0100,
        /// The file or directory was deleted.
        FILE_DELETE = 0x0000_0200,
        /// Extended attributes changed.
        EA_CHANGE = 0x0000_0400,
        /// Access rights changed.
        SECURITY_CHANGE = 0x0000_0800,
        /// The name before a rename.
        RENAME_OLD_NAME = 0x0000_1000,
        /// The name after a rename.
        RENAME_NEW_NAME = 0x0000_2000,
        /// The content-indexed attribute changed.
        INDEXABLE_CHANGE = 0x0000_4000,
        /// Attributes or timestamps changed.
        BASIC_INFO_CHANGE = 0x0000_8000,
        /// A hard link was added or removed.
        HARD_LINK_CHANGE = 0x0001_0000,
        /// Compression was turned on or off.
        COMPRESSION_CHANGE = 0x0002_0000,
        /// Encryption was turned on or off.
        ENCRYPTION_CHANGE = 0x0004_0000,
        /// The object ID changed.
        OBJECT_ID_CHANGE = 0x0008_0000,
        /// A reparse point was added, changed or removed.
        REPARSE_POINT_CHANGE = 0x0010_0000,
        /// A named stream was added, renamed or removed.
        STREAM_CHANGE = 0x0020_0000,
        /// A change was made inside a transaction.
        TRANSACTED_CHANGE = 0x0040_0000,
        /// The integrity attribute changed.
        INTEGRITY_CHANGE = 0x0080_0000,
        /// The desired storage class changed.
        DESIRED_STORAGE_CLASS_CHANGE = 0x0100_0000,
        /// The last handle closed. No more flags accumulate on this record.
        CLOSE = 0x8000_0000,
    }
}

impl Reason {
    /// Every reason, which is the default filter for
    /// [`ReadOptions`](crate::ReadOptions).
    pub const ALL: Reason = Reason(u32::MAX);

    /// Changes to what a reader of the file would see.
    pub const CONTENT: Reason = Reason(
        Self::DATA_OVERWRITE.0
            | Self::DATA_EXTEND.0
            | Self::DATA_TRUNCATION.0
            | Self::NAMED_DATA_OVERWRITE.0
            | Self::NAMED_DATA_EXTEND.0
            | Self::NAMED_DATA_TRUNCATION.0
            | Self::STREAM_CHANGE.0
            | Self::REPARSE_POINT_CHANGE.0
            | Self::TRANSACTED_CHANGE.0,
    );
}

flags! {
    /// What kind of process made a change (`USN_SOURCE_*`).
    pub struct SourceInfo {
        /// The operating system moved data without changing it.
        DATA_MANAGEMENT = 0x0000_0001,
        /// Data was added that does not affect the file's contents.
        AUXILIARY_DATA = 0x0000_0002,
        /// Replication software made the change.
        REPLICATION_MANAGEMENT = 0x0000_0004,
        /// A client's replication software made the change.
        CLIENT_REPLICATION_MANAGEMENT = 0x0000_0008,
    }
}

flags! {
    /// A file's attributes (`FILE_ATTRIBUTE_*`).
    pub struct FileAttributes {
        /// The file is read-only.
        READONLY = 0x0000_0001,
        /// The file is hidden.
        HIDDEN = 0x0000_0002,
        /// The operating system uses this file.
        SYSTEM = 0x0000_0004,
        /// This is a directory.
        DIRECTORY = 0x0000_0010,
        /// The file is marked for archiving.
        ARCHIVE = 0x0000_0020,
        /// The file has no other attributes set.
        NORMAL = 0x0000_0080,
        /// The file is temporary.
        TEMPORARY = 0x0000_0100,
        /// The file is sparse.
        SPARSE_FILE = 0x0000_0200,
        /// This is a reparse point, such as a link or a cloud placeholder.
        REPARSE_POINT = 0x0000_0400,
        /// The file is compressed.
        COMPRESSED = 0x0000_0800,
        /// The file's data is held on remote storage.
        OFFLINE = 0x0000_1000,
        /// The file is excluded from content indexing.
        NOT_CONTENT_INDEXED = 0x0000_2000,
        /// The file is encrypted.
        ENCRYPTED = 0x0000_4000,
        /// The file has an integrity stream.
        INTEGRITY_STREAM = 0x0000_8000,
        /// The file's data is fetched from elsewhere when read.
        RECALL_ON_DATA_ACCESS = 0x0040_0000,
    }
}

/// The kind of a reparse point (`IO_REPARSE_TAG_*`).
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash)]
pub struct ReparseTag(pub u32);

impl ReparseTag {
    /// A junction or a mounted folder.
    pub const MOUNT_POINT: ReparseTag = ReparseTag(0xA000_0003);
    /// A symbolic link.
    pub const SYMLINK: ReparseTag = ReparseTag(0xA000_000C);
    /// A symbolic link created from WSL.
    pub const LX_SYMLINK: ReparseTag = ReparseTag(0xA000_001D);

    /// True for symbolic links, junctions and mounted folders. Other reparse
    /// points, such as cloud placeholders, hold real data.
    pub const fn is_link(self) -> bool {
        matches!(
            self,
            ReparseTag::MOUNT_POINT | ReparseTag::SYMLINK | ReparseTag::LX_SYMLINK
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn contains_requires_all_bits_and_intersects_any() {
        let r = Reason::DATA_EXTEND | Reason::CLOSE;
        assert!(r.contains(Reason::DATA_EXTEND));
        assert!(!r.contains(Reason::DATA_EXTEND | Reason::FILE_CREATE));
        assert!(r.intersects(Reason::DATA_EXTEND | Reason::FILE_CREATE));
        assert!(!r.intersects(Reason::FILE_DELETE));
    }

    #[test]
    fn close_flag_uses_the_top_bit_and_is_named() {
        assert_eq!(Reason::CLOSE.0, 0x8000_0000);
        let names: Vec<_> = (Reason::FILE_CREATE | Reason::CLOSE).names().collect();
        assert_eq!(names, ["FILE_CREATE", "CLOSE"]);
    }

    #[test]
    fn metadata_changes_do_not_count_as_content() {
        assert!(Reason::DATA_OVERWRITE.intersects(Reason::CONTENT));
        assert!(Reason::STREAM_CHANGE.intersects(Reason::CONTENT));
        assert!(!Reason::BASIC_INFO_CHANGE.intersects(Reason::CONTENT));
        assert!(!Reason::SECURITY_CHANGE.intersects(Reason::CONTENT));
    }

    #[test]
    fn debug_lists_the_flags_by_name() {
        assert_eq!(
            format!("{:?}", FileAttributes::DIRECTORY | FileAttributes::HIDDEN),
            "FileAttributes(HIDDEN|DIRECTORY)"
        );
    }

    #[test]
    fn only_links_count_as_links() {
        assert!(ReparseTag::SYMLINK.is_link());
        assert!(ReparseTag::MOUNT_POINT.is_link());
        assert!(ReparseTag::LX_SYMLINK.is_link());
        // A cloud placeholder and a deduplicated file.
        assert!(!ReparseTag(0x9000_601A).is_link());
        assert!(!ReparseTag(0x8000_0013).is_link());
    }

    #[test]
    fn filetime_converts_a_known_instant() {
        // 2020-01-01T00:00:00Z
        let time = FileTime(132_223_104_000_000_000).to_system_time().unwrap();
        assert_eq!(
            time.duration_since(UNIX_EPOCH).unwrap().as_secs(),
            1_577_836_800
        );
    }

    #[test]
    fn filetime_converts_times_before_the_unix_epoch() {
        let time = FileTime(FileTime::UNIX_EPOCH - 10_000_000)
            .to_system_time()
            .unwrap();
        assert_eq!(UNIX_EPOCH.duration_since(time).unwrap().as_secs(), 1);
    }

    #[test]
    fn extreme_filetimes_never_panic() {
        for value in [i64::MIN, i64::MIN + 1, -1, 0, i64::MAX - 1, i64::MAX] {
            let _ = FileTime(value).to_system_time();
        }
    }

    #[test]
    fn identifiers_print_as_documented() {
        assert_eq!(FileId(0x20).to_string(), "0x20");
        assert_eq!(format!("{:?}", FileId(0x20)), "FileId(0x20)");
        assert_eq!(JournalId(0xab).to_string(), "ab");
        assert_eq!(Usn(42).to_string(), "42");
    }
}
