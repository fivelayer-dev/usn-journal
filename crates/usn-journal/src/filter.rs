use std::ffi::OsStr;
use std::path::Path;

use crate::wide;

const BACKSLASH: u16 = b'\\' as u16;
const SLASH: u16 = b'/' as u16;
const COLON: u16 = b':' as u16;

const SYSTEM_FILES: [&str; 8] = [
    "$Extend",
    "$Recycle.Bin",
    "$WinREAgent",
    "System Volume Information",
    "pagefile.sys",
    "hiberfil.sys",
    "swapfile.sys",
    "DumpStack.log.tmp",
];

/// Which paths to report, given relative to the volume root. Matching ignores
/// case and applies to whole path components, and a directory covers everything
/// beneath it. A leading drive, as in `C:\Windows`, is ignored.
///
/// With no includes, everything is kept except what is excluded. With includes,
/// only those paths are kept, along with the directories leading to them. Excludes
/// always win.
#[derive(Clone, Debug, Default)]
pub struct PathFilter {
    includes: Vec<Vec<u16>>,
    excludes: Vec<Vec<u16>>,
}

impl PathFilter {
    /// A filter that keeps everything.
    pub fn new() -> PathFilter {
        PathFilter::default()
    }

    /// Adds a path to leave out.
    pub fn exclude(mut self, path: impl AsRef<OsStr>) -> PathFilter {
        self.excludes.push(normalize(path.as_ref()));
        self
    }

    /// Limits reporting to a path. Can be called repeatedly to keep several.
    pub fn include(mut self, path: impl AsRef<OsStr>) -> PathFilter {
        self.includes.push(normalize(path.as_ref()));
        self
    }

    /// Leaves out what the file system and the OS manage for themselves, such
    /// as `$Extend`, the recycle bin, `pagefile.sys` and `hiberfil.sys`.
    pub fn exclude_system_files(self) -> PathFilter {
        SYSTEM_FILES
            .iter()
            .fold(self, |filter, path| filter.exclude(path))
    }

    /// True if `path`, a full path under `root`, should not be reported. A path
    /// that is not under `root` is not excluded.
    pub(crate) fn is_excluded(&self, root: &Path, path: &Path) -> bool {
        if self.includes.is_empty() && self.excludes.is_empty() {
            return false;
        }
        let path = folded(path.as_os_str());
        let root = folded(root.as_os_str());
        let Some(relative) = path.strip_prefix(root.as_slice()) else {
            return false;
        };
        let relative = trim_separators(relative);
        if self.excludes.iter().any(|e| is_under(relative, e)) {
            return true;
        }
        !self.includes.is_empty()
            && !self
                .includes
                .iter()
                .any(|i| is_under(relative, i) || is_under(i, relative))
    }
}

/// Makes a rule comparable with a folded path.
fn normalize(path: &OsStr) -> Vec<u16> {
    let path: Vec<u16> = folded(path)
        .into_iter()
        .map(|unit| if unit == SLASH { BACKSLASH } else { unit })
        .collect();
    let relative = match path.as_slice() {
        [_, COLON, rest @ ..] => rest,
        all => all,
    };
    trim_separators(relative).to_vec()
}

fn trim_separators(mut path: &[u16]) -> &[u16] {
    while let [BACKSLASH, rest @ ..] = path {
        path = rest;
    }
    while let [rest @ .., BACKSLASH] = path {
        path = rest;
    }
    path
}

fn folded(text: &OsStr) -> Vec<u16> {
    let mut units = wide::to_utf16(text);
    units.iter_mut().for_each(|unit| *unit = fold(*unit));
    units
}

/// Folds one UTF-16 unit the way NTFS compares names: unit by unit, using only
/// mappings that stay a single unit. Surrogates are left alone.
fn fold(unit: u16) -> u16 {
    let Some(c) = char::from_u32(unit as u32) else {
        return unit;
    };
    let mut upper = c.to_uppercase();
    match (upper.next(), upper.next()) {
        (Some(single), None) if (single as u32) <= 0xFFFF => single as u16,
        _ => unit,
    }
}

/// True if `path` is `ancestor` or lies beneath it. The empty path is the root.
fn is_under(path: &[u16], ancestor: &[u16]) -> bool {
    if ancestor.is_empty() {
        return true;
    }
    match path.strip_prefix(ancestor) {
        Some(rest) => rest.is_empty() || rest[0] == BACKSLASH,
        None => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn excluded(filter: &PathFilter, path: &str) -> bool {
        filter.is_excluded(Path::new("C:\\"), Path::new(path))
    }

    #[test]
    fn matches_whole_components_only() {
        let f = PathFilter::new().exclude("foo");
        assert!(excluded(&f, "C:\\foo"));
        assert!(excluded(&f, "C:\\foo\\bar.txt"));
        assert!(!excluded(&f, "C:\\foobar"));
        assert!(!excluded(&f, "C:\\bar\\foo"));
    }

    #[test]
    fn ignores_case() {
        let f = PathFilter::new().exclude("Windows\\Temp");
        assert!(excluded(&f, "C:\\WINDOWS\\temp\\x.tmp"));
        assert!(!excluded(&f, "C:\\Windows\\System32\\x.dll"));
    }

    #[test]
    fn accepts_absolute_and_slash_separated_forms() {
        for form in ["C:\\Windows\\Temp", "\\Windows\\Temp\\", "Windows/Temp"] {
            assert!(
                excluded(&PathFilter::new().exclude(form), "C:\\Windows\\Temp\\x"),
                "{form}"
            );
        }
    }

    #[test]
    fn include_keeps_only_that_tree_on_whole_components() {
        let f = PathFilter::new().include("Users\\me");
        assert!(!excluded(&f, "C:\\Users\\me"));
        assert!(!excluded(&f, "C:\\users\\ME\\doc.txt"));
        assert!(excluded(&f, "C:\\Users\\me2\\doc.txt"));
        assert!(excluded(&f, "C:\\Windows\\x.dll"));
    }

    #[test]
    fn include_keeps_the_directories_leading_to_it() {
        let f = PathFilter::new().include("Users\\me\\Documents");
        assert!(!excluded(&f, "C:\\"));
        assert!(!excluded(&f, "C:\\Users"));
        assert!(!excluded(&f, "C:\\Users\\me"));
        assert!(excluded(&f, "C:\\Users\\other"));
        assert!(excluded(&f, "C:\\Users\\me\\Downloads"));
    }

    #[test]
    fn several_includes_are_all_kept() {
        let f = PathFilter::new().include("a").include("b\\c");
        assert!(!excluded(&f, "C:\\a\\x"));
        assert!(!excluded(&f, "C:\\b\\c\\x"));
        assert!(excluded(&f, "C:\\b\\d"));
    }

    #[test]
    fn excludes_win_over_includes_and_their_parents() {
        let f = PathFilter::new()
            .include("a\\b")
            .exclude("a\\b\\skip")
            .exclude("a");
        assert!(excluded(&f, "C:\\a\\b\\skip\\x"));
        assert!(excluded(&f, "C:\\a\\b\\x"));
        assert!(excluded(&f, "C:\\a"));

        let carve_out = PathFilter::new().include("a").exclude("a\\skip");
        assert!(!excluded(&carve_out, "C:\\a\\keep"));
        assert!(excluded(&carve_out, "C:\\a\\skip\\x"));
    }

    #[test]
    fn including_the_root_keeps_everything() {
        let f = PathFilter::new().include("C:\\");
        assert!(!excluded(&f, "C:\\anything\\at\\all"));
    }

    #[test]
    fn empty_filter_excludes_nothing() {
        assert!(!excluded(&PathFilter::new(), "C:\\anything"));
    }

    #[test]
    fn defaults_exclude_system_paths_but_not_user_files() {
        let f = PathFilter::new().exclude_system_files();
        assert!(excluded(&f, "C:\\pagefile.sys"));
        assert!(excluded(&f, "C:\\$Recycle.Bin\\S-1-5\\file"));
        assert!(excluded(&f, "C:\\System Volume Information\\x"));
        assert!(!excluded(&f, "C:\\Users\\me\\pagefile.sys"));
        assert!(!excluded(&f, "C:\\Users\\me\\doc.txt"));
    }

    #[test]
    fn works_with_volume_guid_roots() {
        let root = Path::new("\\\\?\\Volume{abc}\\");
        let f = PathFilter::new().exclude("skip");
        assert!(f.is_excluded(root, Path::new("\\\\?\\Volume{abc}\\skip\\x")));
        assert!(!f.is_excluded(root, Path::new("\\\\?\\Volume{abc}\\keep\\x")));
    }

    #[test]
    fn folds_non_ascii_case_like_ntfs_does() {
        let f = PathFilter::new().exclude("ÉCOLE");
        assert!(excluded(&f, "C:\\école\\x"));
    }

    #[test]
    fn a_mapping_that_changes_length_is_left_alone() {
        // Uppercasing this gives "SS", which NTFS does not treat as equal.
        let f = PathFilter::new().exclude("straße");
        assert!(excluded(&f, "C:\\STRAßE\\x"));
        assert!(!excluded(&f, "C:\\STRASSE\\x"));
    }

    #[cfg(windows)]
    #[test]
    fn unpaired_surrogates_are_matched_exactly() {
        use std::ffi::OsString;
        use std::os::windows::ffi::OsStringExt;
        let mut name = OsString::from("C:\\");
        name.push(OsString::from_wide(&[0x61, 0xD800]));
        let rule = OsString::from_wide(&[0x41, 0xD800]);
        let f = PathFilter::new().exclude(&rule);
        assert!(f.is_excluded(Path::new("C:\\"), Path::new(&name)));
    }

    #[test]
    fn a_path_outside_the_root_is_kept() {
        let f = PathFilter::new().exclude("skip");
        assert!(!f.is_excluded(Path::new("D:\\"), Path::new("C:\\skip\\x")));
    }
}
