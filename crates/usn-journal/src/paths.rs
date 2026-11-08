use std::collections::HashMap;
use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};

use crate::record::RawRecord;
use crate::types::{FileId, Reason};

/// Deeper than any path NTFS can hold, so only a corrupt parent cycle hits it.
const MAX_DEPTH: usize = 16_384;

/// Maps directory identities to paths. Journal records carry only a file's
/// parent identity and its own name, so a full path means walking up parents.
/// Only directories are stored, which keeps this small on large volumes.
pub(crate) struct PathCache {
    root: PathBuf,
    root_id: FileId,
    links: HashMap<FileId, Link>,
    /// Full paths found by lookup. Unlike links these go stale when an ancestor
    /// is renamed, so they are all dropped when any directory is renamed.
    looked_up: HashMap<FileId, PathBuf>,
}

struct Link {
    parent: FileId,
    name: Box<OsStr>,
}

impl PathCache {
    pub fn new(root: PathBuf, root_id: FileId) -> PathCache {
        PathCache {
            root,
            root_id,
            links: HashMap::new(),
            looked_up: HashMap::new(),
        }
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Forgets everything learned, for when directories may have changed in
    /// records that can no longer be read.
    pub fn clear(&mut self) {
        self.links.clear();
        self.looked_up.clear();
    }

    pub fn insert_dir(&mut self, id: FileId, parent: FileId, name: &OsStr) {
        if id != self.root_id {
            self.links.insert(
                id,
                Link {
                    parent,
                    name: name.into(),
                },
            );
        }
    }

    /// Updates the cache from a record. Call this after anything that needs
    /// the path as it was before the record, such as a rename's old path.
    pub fn apply(&mut self, record: &RawRecord<'_>) {
        if record.is_directory() {
            self.apply_directory(record.id, record.parent_id, record.reason, &record.name());
        }
    }

    /// The same for a record known to be about a directory.
    pub fn apply_directory(&mut self, id: FileId, parent: FileId, reason: Reason, name: &OsStr) {
        if reason.contains(Reason::FILE_DELETE) {
            self.links.remove(&id);
            self.looked_up.remove(&id);
            return;
        }
        if is_rename_old(reason) {
            return;
        }
        if reason.contains(Reason::RENAME_NEW_NAME) {
            self.looked_up.clear();
        }
        self.insert_dir(id, parent, name);
    }

    /// The full path of `name` in the directory `parent`, or `None` if the
    /// directory's location cannot be determined.
    pub fn path_of(
        &mut self,
        parent: FileId,
        name: &OsStr,
        lookup: &mut dyn FnMut(FileId) -> Option<PathBuf>,
    ) -> Option<PathBuf> {
        let mut path = self.dir_path(parent, lookup)?.into_os_string();
        push_component(&mut path, name);
        Some(PathBuf::from(path))
    }

    /// The full path of a directory.
    pub fn dir_path(
        &mut self,
        dir: FileId,
        lookup: &mut dyn FnMut(FileId) -> Option<PathBuf>,
    ) -> Option<PathBuf> {
        let mut names: Vec<&OsStr> = Vec::new();
        let mut current = dir;
        let mut base: Option<PathBuf> = None;
        for depth in 0.. {
            if depth == MAX_DEPTH {
                return None;
            }
            if current == self.root_id {
                base = Some(self.root.clone());
                break;
            }
            if let Some(path) = self.looked_up.get(&current) {
                base = Some(path.clone());
                break;
            }
            match self.links.get(&current) {
                Some(link) => {
                    names.push(&link.name);
                    current = link.parent;
                }
                None => break,
            }
        }
        let base = match base {
            Some(base) => base,
            None => {
                // The chain ended at a directory the cache does not know.
                let found = lookup(current)?;
                self.looked_up.insert(current, found.clone());
                found
            }
        };
        let mut path = base.into_os_string();
        for name in names.iter().rev() {
            push_component(&mut path, name);
        }
        Some(PathBuf::from(path))
    }
}

// Paths here are always Windows paths, whatever OS this code is compiled for,
// so the separator is handled explicitly rather than through `Path::join`.
fn push_component(path: &mut OsString, name: &OsStr) {
    if path.as_encoded_bytes().last() != Some(&b'\\') {
        path.push("\\");
    }
    path.push(name);
}

/// A record naming the file as it was before a rename, as opposed to the
/// records written afterwards under the new name.
pub(crate) fn is_rename_old(reason: Reason) -> bool {
    reason.contains(Reason::RENAME_OLD_NAME) && !reason.contains(Reason::RENAME_NEW_NAME)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::record::split_buffer;
    use crate::testing::{buffer, v2};
    use crate::types::FileAttributes;

    const ROOT: FileId = FileId(5);
    const DIR: u32 = FileAttributes::DIRECTORY.0;

    fn cache() -> PathCache {
        PathCache::new(PathBuf::from("C:\\"), ROOT)
    }

    fn no_lookup(_: FileId) -> Option<PathBuf> {
        None
    }

    fn apply(c: &mut PathCache, reason: Reason, id: u64, parent: u64, name: &str) {
        let b = buffer(0, &[v2(1, id, parent, reason.0, DIR, name)]);
        let (_, mut it) = split_buffer(&b).unwrap();
        c.apply(&it.next().unwrap().unwrap());
    }

    fn path(c: &mut PathCache, parent: u64, name: &str) -> Option<PathBuf> {
        c.path_of(FileId(parent as u128), OsStr::new(name), &mut no_lookup)
    }

    #[test]
    fn root_directory_path_has_a_single_separator() {
        let mut c = cache();
        assert_eq!(
            c.dir_path(ROOT, &mut no_lookup),
            Some(PathBuf::from("C:\\"))
        );
        assert_eq!(path(&mut c, 5, "f.txt"), Some(PathBuf::from("C:\\f.txt")));
    }

    #[test]
    fn walks_nested_directories() {
        let mut c = cache();
        c.insert_dir(FileId(10), ROOT, OsStr::new("a"));
        c.insert_dir(FileId(11), FileId(10), OsStr::new("b"));
        assert_eq!(path(&mut c, 11, "f"), Some(PathBuf::from("C:\\a\\b\\f")));
    }

    #[test]
    fn unknown_directory_is_none_without_a_lookup() {
        assert_eq!(path(&mut cache(), 99, "f"), None);
    }

    #[test]
    fn parent_cycle_terminates() {
        let mut c = cache();
        c.insert_dir(FileId(10), FileId(11), OsStr::new("a"));
        c.insert_dir(FileId(11), FileId(10), OsStr::new("b"));
        assert_eq!(path(&mut c, 10, "f"), None);
    }

    #[test]
    fn lookup_fills_in_an_unknown_ancestor_and_is_cached() {
        let mut c = cache();
        c.insert_dir(FileId(11), FileId(10), OsStr::new("b"));
        let mut calls = 0;
        let mut lookup = |id: FileId| {
            calls += 1;
            assert_eq!(id, FileId(10));
            Some(PathBuf::from("C:\\found"))
        };
        let parent = FileId(11);
        assert_eq!(
            c.path_of(parent, OsStr::new("f"), &mut lookup),
            Some(PathBuf::from("C:\\found\\b\\f"))
        );
        assert_eq!(
            c.path_of(parent, OsStr::new("g"), &mut lookup),
            Some(PathBuf::from("C:\\found\\b\\g"))
        );
        assert_eq!(calls, 1);
    }

    #[test]
    fn records_track_directory_creation_rename_and_delete() {
        let mut c = cache();
        apply(&mut c, Reason::FILE_CREATE, 10, 5, "old");
        assert_eq!(path(&mut c, 10, "f"), Some(PathBuf::from("C:\\old\\f")));

        apply(&mut c, Reason::RENAME_OLD_NAME, 10, 5, "old");
        assert_eq!(path(&mut c, 10, "f"), Some(PathBuf::from("C:\\old\\f")));

        apply(&mut c, Reason::RENAME_NEW_NAME, 10, 5, "new");
        assert_eq!(path(&mut c, 10, "f"), Some(PathBuf::from("C:\\new\\f")));

        apply(&mut c, Reason::FILE_DELETE | Reason::CLOSE, 10, 5, "new");
        assert_eq!(path(&mut c, 10, "f"), None);
    }

    #[test]
    fn directory_move_updates_descendant_paths() {
        let mut c = cache();
        apply(&mut c, Reason::FILE_CREATE, 10, 5, "a");
        apply(&mut c, Reason::FILE_CREATE, 11, 5, "b");
        apply(&mut c, Reason::FILE_CREATE, 12, 10, "sub");
        apply(&mut c, Reason::RENAME_NEW_NAME, 12, 11, "sub");
        assert_eq!(path(&mut c, 12, "f"), Some(PathBuf::from("C:\\b\\sub\\f")));
    }

    #[test]
    fn directory_rename_invalidates_looked_up_paths() {
        let mut c = cache();
        let mut first = |_: FileId| Some(PathBuf::from("C:\\before"));
        assert_eq!(
            c.dir_path(FileId(20), &mut first),
            Some(PathBuf::from("C:\\before"))
        );
        apply(&mut c, Reason::RENAME_NEW_NAME, 30, 5, "unrelated");
        let mut second = |_: FileId| Some(PathBuf::from("C:\\after"));
        assert_eq!(
            c.dir_path(FileId(20), &mut second),
            Some(PathBuf::from("C:\\after"))
        );
    }

    #[test]
    fn file_records_are_not_cached() {
        let mut c = cache();
        let b = buffer(0, &[v2(1, 10, 5, Reason::FILE_CREATE.0, 0, "file")]);
        c.apply(&split_buffer(&b).unwrap().1.next().unwrap().unwrap());
        assert!(c.links.is_empty());
    }

    #[test]
    fn clear_forgets_directories_and_lookups() {
        let mut c = cache();
        c.insert_dir(FileId(10), ROOT, OsStr::new("a"));
        let mut found = |_: FileId| Some(PathBuf::from("C:\\found"));
        c.dir_path(FileId(20), &mut found);
        c.clear();
        assert_eq!(path(&mut c, 10, "f"), None);
        assert_eq!(path(&mut c, 20, "f"), None);
        assert_eq!(path(&mut c, 5, "f"), Some(PathBuf::from("C:\\f")));
    }
}
