//! Helpers shared by the tests that use a real volume.

#![allow(dead_code)]

use std::ffi::c_void;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::mem::{size_of, zeroed};
use std::os::windows::fs::OpenOptionsExt;
use std::os::windows::io::AsRawHandle;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicU32, Ordering};

use usn_journal::FileId;
use windows_sys::Win32::Storage::FileSystem::{
    FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT, FILE_ID_INFO, FILE_READ_ATTRIBUTES,
    FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE, FileIdInfo, GetFileInformationByHandleEx,
};

static NEXT_DIR: AtomicU32 = AtomicU32::new(0);

/// A new directory under the temp directory, removed when dropped. These tests
/// only read the journal, and this is the only thing they write.
pub struct TempDir(pub PathBuf);

impl TempDir {
    pub fn new() -> TempDir {
        let n = NEXT_DIR.fetch_add(1, Ordering::Relaxed);
        let path =
            std::env::temp_dir().join(format!("usn-journal-test-{}-{n}", std::process::id()));
        fs::create_dir_all(&path).unwrap();
        // The final path, because the temp directory can contain 8.3 short names
        // or reparse points, while the library reports normalized long paths.
        TempDir(fs::canonicalize(&path).map(strip_verbatim).unwrap())
    }

    pub fn file(&self, name: &str) -> PathBuf {
        self.0.join(name)
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn strip_verbatim(path: PathBuf) -> PathBuf {
    match path.to_str().and_then(|s| s.strip_prefix(r"\\?\")) {
        Some(stripped) => PathBuf::from(stripped),
        None => path,
    }
}

/// The drive holding the temp directory, such as `C:`.
pub fn temp_drive() -> String {
    std::env::temp_dir()
        .to_string_lossy()
        .chars()
        .take(2)
        .collect()
}

pub fn write_file(path: &Path, contents: &[u8]) {
    let mut f = File::create(path).unwrap();
    f.write_all(contents).unwrap();
    f.sync_all().unwrap();
}

pub fn append_file(path: &Path, contents: &[u8]) {
    let mut f = OpenOptions::new().append(true).open(path).unwrap();
    f.write_all(contents).unwrap();
    f.sync_all().unwrap();
}

fn open_for_id(path: &Path, flags: u32) -> io::Result<File> {
    OpenOptions::new()
        .access_mode(FILE_READ_ATTRIBUTES)
        .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE)
        .custom_flags(flags)
        .open(path)
}

fn file_id(file: &File) -> FileId {
    // SAFETY: FILE_ID_INFO is plain data, valid when zeroed, and the size passed
    // is its own.
    unsafe {
        let mut info: FILE_ID_INFO = zeroed();
        let ok = GetFileInformationByHandleEx(
            file.as_raw_handle(),
            FileIdInfo,
            &mut info as *mut _ as *mut c_void,
            size_of::<FILE_ID_INFO>() as u32,
        );
        assert_ne!(ok, 0, "{}", io::Error::last_os_error());
        FileId(u128::from_le_bytes(info.FileId.Identifier))
    }
}

/// The identity of a file or directory.
pub fn id_of(path: &Path) -> FileId {
    file_id(&open_for_id(path, FILE_FLAG_BACKUP_SEMANTICS).unwrap())
}

/// The identity of a link itself, not of what it points at.
pub fn id_of_link(path: &Path) -> FileId {
    let flags = FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT;
    file_id(&open_for_id(path, flags).unwrap())
}

pub fn make_junction(link: &Path, target: &Path) {
    let status = Command::new("cmd")
        .args(["/C", "mklink", "/J"])
        .arg(link)
        .arg(target)
        .stdout(Stdio::null())
        .status()
        .unwrap();
    assert!(status.success(), "mklink /J failed");
}
