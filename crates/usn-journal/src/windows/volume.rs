use std::ffi::{OsStr, OsString, c_void};
use std::fmt;
use std::io;
use std::mem::{size_of, zeroed};
use std::os::windows::ffi::{OsStrExt, OsStringExt};
use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
use std::path::{Path, PathBuf};
use std::ptr::{null, null_mut};
use std::time::SystemTime;

use windows_sys::Win32::Foundation::{
    ERROR_ACCESS_DENIED, ERROR_HANDLE_EOF, ERROR_INVALID_FUNCTION,
    ERROR_JOURNAL_DELETE_IN_PROGRESS, ERROR_JOURNAL_ENTRY_DELETED, ERROR_JOURNAL_NOT_ACTIVE,
    ERROR_NOT_SUPPORTED, ERROR_WRITE_PROTECT, GENERIC_READ, INVALID_HANDLE_VALUE,
};
use windows_sys::Win32::Storage::FileSystem::{
    CreateFileW, ExtendedFileIdType, FILE_ATTRIBUTE_NORMAL, FILE_ATTRIBUTE_TAG_INFO,
    FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT, FILE_ID_128, FILE_ID_DESCRIPTOR,
    FILE_ID_INFO, FILE_NAME_NORMALIZED, FILE_READ_ATTRIBUTES, FILE_SHARE_DELETE, FILE_SHARE_READ,
    FILE_SHARE_WRITE, FileAttributeTagInfo, FileIdInfo, FileIdType, GetFileInformationByHandleEx,
    GetFinalPathNameByHandleW, GetVolumePathNameW, OPEN_EXISTING, OpenFileById, VOLUME_NAME_NONE,
};
use windows_sys::Win32::System::IO::DeviceIoControl;
use windows_sys::Win32::System::Ioctl::{
    FSCTL_ENUM_USN_DATA, FSCTL_QUERY_USN_JOURNAL, FSCTL_READ_UNPRIVILEGED_USN_JOURNAL,
    FSCTL_READ_USN_JOURNAL, MFT_ENUM_DATA_V1, READ_USN_JOURNAL_DATA_V1,
};

use super::iter::{MftEntries, Records, Scan};
use crate::error::{Error, Result};
use crate::filter::PathFilter;
use crate::health::{self, JournalHealth};
use crate::journal::{Gap, JournalInfo};
use crate::options::{HealthOptions, ReadOptions};
use crate::record::split_buffer;
use crate::source::Source;
use crate::types::{FileAttributes, FileId, JournalId, Reason, ReparseTag, Usn};
use crate::wide;

const ENUM_BUFFER_BYTES: usize = 1024 * 1024;
const SHARE_ALL: u32 = FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE;

/// An open volume.
///
/// The handle is opened for reading only, and in the default build every call
/// this type makes is a query, so nothing on the volume changes.
pub struct Volume {
    handle: OwnedHandle,
    access: Access,
    root: PathBuf,
    root_id: FileId,
}

enum Access {
    /// Opened as a device, which needs elevation.
    Device(String),
    /// Opened through its root directory, so records have their names removed.
    RootDirectory,
}

impl fmt::Debug for Volume {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Volume").field("root", &self.root).finish()
    }
}

impl Volume {
    /// Opens a volume by drive (`C`, `C:`, `C:\`), GUID path
    /// (`\\?\Volume{...}\`) or device path, which includes a volume shadow
    /// copy (`\\?\GLOBALROOT\Device\HarddiskVolumeShadowCopy7`). Reading the
    /// journal needs an elevated process.
    pub fn open(name: impl AsRef<OsStr>) -> Result<Volume> {
        let name = name.as_ref();
        let text = name
            .to_str()
            .ok_or_else(|| Error::InvalidVolume(name.to_string_lossy().into_owned()))?;
        let (device, root) = parse_volume_name(text)?;
        let handle = open_device(&device)?;
        let root_id = file_id(&open_directory(OsStr::new(&root))?)?;
        Ok(Volume {
            handle,
            access: Access::Device(device),
            root: PathBuf::from(root),
            root_id,
        })
    }

    /// Opens the volume holding `path` without needing elevation. Records
    /// have no names, so changes have no paths and files are told apart by
    /// [`FileId`]. Listing the volume returns [`Error::AccessDenied`].
    pub fn open_unprivileged(path: impl AsRef<Path>) -> Result<Volume> {
        let root = volume_root(path.as_ref())?;
        let handle = open_directory(&root)?;
        let root_id = file_id(&handle)?;
        Ok(Volume {
            handle,
            access: Access::RootDirectory,
            root: PathBuf::from(root),
            root_id,
        })
    }

    /// The volume's root, with its trailing separator.
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Reads the journal's state.
    pub fn journal_info(&self) -> Result<JournalInfo> {
        let mut out = [0u8; 80];
        let n =
            ioctl(&self.handle, FSCTL_QUERY_USN_JOURNAL, null(), 0, &mut out).map_err(to_error)?;
        JournalInfo::parse(&out[..n])
    }

    /// Reads raw records. A start position that has been lost is
    /// [`Error::Gap`], from here or part way through iterating.
    pub fn records(&self, options: ReadOptions) -> Result<Records<'_>> {
        Records::new(self, options)
    }

    /// Lists every file and directory from the master file table, in no
    /// useful order and without paths. Iterating fails with
    /// [`Error::Unsupported`] on a file system that can't list itself, such
    /// as ReFS.
    pub fn mft_entries(&self) -> Result<MftEntries<'_>> {
        self.require_device()?;
        Ok(MftEntries::new(self))
    }

    /// Lists every file and directory with its full path. Links are listed
    /// but not followed. A hard-linked file can appear under only one of
    /// its names, and the filter sees only that name. Fails with
    /// [`Error::Unsupported`] on a file system that can't list itself, such
    /// as ReFS.
    pub fn scan(&self, filter: &PathFilter) -> Result<Scan<'_>> {
        Scan::new(self, filter.clone())
    }

    /// The current path of a file or directory. For a link, this is the path
    /// of the link itself.
    pub fn path_of(&self, id: FileId) -> Result<PathBuf> {
        let handle = self.open_by_id(id)?;
        let mut units = vec![0u16; 1024];
        loop {
            // SAFETY: the buffer is valid for the length passed.
            let n = unsafe {
                GetFinalPathNameByHandleW(
                    handle.as_raw_handle(),
                    units.as_mut_ptr(),
                    units.len() as u32,
                    FILE_NAME_NORMALIZED | VOLUME_NAME_NONE,
                )
            } as usize;
            if n == 0 {
                return Err(last_error());
            }
            if n < units.len() {
                let mut path = self.root.clone().into_os_string();
                // The relative path starts with a separator and the root ends
                // with one, so drop one of them.
                path.push(wide::from_utf16(strip_leading_separator(&units[..n])));
                return Ok(PathBuf::from(path));
            }
            units.resize(n + 1, 0);
        }
    }

    /// The kind of reparse point a file is, or `None` if it isn't one.
    pub fn reparse_tag(&self, id: FileId) -> Result<Option<ReparseTag>> {
        let handle = self.open_by_id(id)?;
        // SAFETY: FILE_ATTRIBUTE_TAG_INFO is plain data, valid when zeroed, and
        // the size passed is its own.
        let mut info: FILE_ATTRIBUTE_TAG_INFO = unsafe { zeroed() };
        let ok = unsafe {
            GetFileInformationByHandleEx(
                handle.as_raw_handle(),
                FileAttributeTagInfo,
                &mut info as *mut _ as *mut c_void,
                size_of::<FILE_ATTRIBUTE_TAG_INFO>() as u32,
            )
        };
        if ok == 0 {
            return Err(last_error());
        }
        let is_reparse_point =
            FileAttributes(info.FileAttributes).contains(FileAttributes::REPARSE_POINT);
        Ok(is_reparse_point.then_some(ReparseTag(info.ReparseTag)))
    }

    /// Reports how much history the journal holds and whether that is
    /// enough. This reads the whole journal, but changes nothing.
    pub fn health(&self, options: &HealthOptions) -> Result<JournalHealth> {
        health::check(&mut ReadOnly(self), options, SystemTime::now())
    }

    /// Creates the journal, or changes its size if it already exists. Sizes
    /// are in bytes. This writes to the volume.
    #[cfg(feature = "journal-admin")]
    pub fn create_journal(&self, maximum_size: u64, allocation_delta: u64) -> Result<()> {
        use windows_sys::Win32::System::Ioctl::{
            CREATE_USN_JOURNAL_DATA, FSCTL_CREATE_USN_JOURNAL,
        };
        let request = CREATE_USN_JOURNAL_DATA {
            MaximumSize: maximum_size,
            AllocationDelta: allocation_delta,
        };
        let handle = open_device_for_writing(self.require_device()?)?;
        ioctl(
            &handle,
            FSCTL_CREATE_USN_JOURNAL,
            &request as *const _ as *const c_void,
            size_of::<CREATE_USN_JOURNAL_DATA>() as u32,
            &mut [],
        )
        .map_err(to_error)?;
        Ok(())
    }

    /// Deletes the journal and waits for that to finish. Every saved cursor
    /// for this volume becomes a gap. This writes to the volume.
    #[cfg(feature = "journal-admin")]
    pub fn delete_journal(&self) -> Result<()> {
        use windows_sys::Win32::System::Ioctl::{
            DELETE_USN_JOURNAL_DATA, FSCTL_DELETE_USN_JOURNAL, USN_DELETE_FLAG_DELETE,
            USN_DELETE_FLAG_NOTIFY,
        };
        let request = DELETE_USN_JOURNAL_DATA {
            UsnJournalID: self.journal_info()?.id.0,
            DeleteFlags: USN_DELETE_FLAG_DELETE | USN_DELETE_FLAG_NOTIFY,
        };
        let handle = open_device_for_writing(self.require_device()?)?;
        ioctl(
            &handle,
            FSCTL_DELETE_USN_JOURNAL,
            &request as *const _ as *const c_void,
            size_of::<DELETE_USN_JOURNAL_DATA>() as u32,
            &mut [],
        )
        .map_err(to_error)?;
        Ok(())
    }

    /// Fails unless this volume was opened as a device.
    fn require_device(&self) -> Result<&str> {
        match &self.access {
            Access::Device(device) => Ok(device),
            Access::RootDirectory => Err(Error::AccessDenied),
        }
    }

    /// Reads records into `buffer`. On a read-only copy of a volume, reaching
    /// the end of the journal fails with `ERROR_WRITE_PROTECT` where a live
    /// journal returns no records, so that is treated as the end.
    pub(super) fn read_journal(
        &self,
        request: &READ_USN_JOURNAL_DATA_V1,
        buffer: &mut [u8],
    ) -> Result<usize> {
        let code = match self.access {
            Access::Device(_) => FSCTL_READ_USN_JOURNAL,
            Access::RootDirectory => FSCTL_READ_UNPRIVILEGED_USN_JOURNAL,
        };
        let result = ioctl(
            &self.handle,
            code,
            request as *const _ as *const c_void,
            size_of::<READ_USN_JOURNAL_DATA_V1>() as u32,
            buffer,
        );
        match result {
            Ok(n) => Ok(n),
            Err(e) if os_error(&e) == Some(ERROR_WRITE_PROTECT) => {
                buffer[..8].copy_from_slice(&request.StartUsn.to_le_bytes());
                Ok(8)
            }
            Err(e) if os_error(&e) == Some(ERROR_JOURNAL_ENTRY_DELETED) => {
                let start = Usn(request.StartUsn);
                let oldest_readable = self.journal_info().map_or(start, |i| i.oldest_readable());
                Err(Error::Gap(Gap::RecordsLost {
                    cursor: start,
                    oldest_readable,
                }))
            }
            Err(e) => Err(to_error(e)),
        }
    }

    /// Fills `buffer` with the next part of the master file table, returning
    /// the byte count, or `None` once all of it has been read.
    pub(super) fn read_mft(&self, start: u64, buffer: &mut [u8]) -> Result<Option<usize>> {
        self.require_device()?;
        let request = MFT_ENUM_DATA_V1 {
            StartFileReferenceNumber: start,
            LowUsn: 0,
            HighUsn: i64::MAX,
            MinMajorVersion: 2,
            MaxMajorVersion: 3,
        };
        let result = ioctl(
            &self.handle,
            FSCTL_ENUM_USN_DATA,
            &request as *const _ as *const c_void,
            size_of::<MFT_ENUM_DATA_V1>() as u32,
            buffer,
        );
        match result {
            Ok(n) => Ok(Some(n)),
            Err(e) if os_error(&e) == Some(ERROR_HANDLE_EOF) => Ok(None),
            Err(e) => Err(to_error(e)),
        }
    }

    pub(super) fn root_id(&self) -> FileId {
        self.root_id
    }

    /// Reports every directory on the volume with its parent and name.
    pub(super) fn visit_directories(
        &self,
        visit: &mut dyn FnMut(FileId, FileId, &OsStr),
    ) -> Result<()> {
        let mut buffer = vec![0u8; ENUM_BUFFER_BYTES];
        let mut start = 0u64;
        while let Some(n) = self.read_mft(start, &mut buffer)? {
            let (next, records) = split_buffer(&buffer[..n])?;
            for record in records {
                let record = record?;
                if record.is_directory() {
                    visit(record.id, record.parent_id, &record.name());
                }
            }
            start = next.0 as u64;
        }
        Ok(())
    }

    /// Opens a file or directory by identity. A link is opened rather than its
    /// target, and only read access to its attributes is requested.
    fn open_by_id(&self, id: FileId) -> Result<OwnedHandle> {
        // SAFETY: an all-zero FILE_ID_DESCRIPTOR is valid and is filled in below.
        let mut descriptor: FILE_ID_DESCRIPTOR = unsafe { zeroed() };
        descriptor.dwSize = size_of::<FILE_ID_DESCRIPTOR>() as u32;
        if id.0 >> 64 == 0 {
            descriptor.Type = FileIdType;
            descriptor.Anonymous.FileId = id.0 as i64;
        } else {
            descriptor.Type = ExtendedFileIdType;
            descriptor.Anonymous.ExtendedFileId = FILE_ID_128 {
                Identifier: id.0.to_le_bytes(),
            };
        }
        // SAFETY: the descriptor is valid and the handle outlives the call.
        let raw = unsafe {
            OpenFileById(
                self.handle.as_raw_handle(),
                &descriptor,
                FILE_READ_ATTRIBUTES,
                SHARE_ALL,
                null(),
                FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT,
            )
        };
        owned(raw)
    }
}

/// The request that reads every record from `start` without waiting.
fn read_request(start: Usn, journal: JournalId) -> READ_USN_JOURNAL_DATA_V1 {
    READ_USN_JOURNAL_DATA_V1 {
        StartUsn: start.0,
        ReasonMask: Reason::ALL.0,
        ReturnOnlyOnClose: 0,
        // Zero means return at once, even with nothing to read. Waiting is
        // left to the caller so that it can be stopped at any time.
        Timeout: 0,
        BytesToWaitFor: 0,
        UsnJournalID: journal.0,
        MinMajorVersion: 2,
        MaxMajorVersion: 3,
    }
}

impl Source for Volume {
    fn query(&mut self) -> Result<JournalInfo> {
        self.journal_info()
    }

    fn read(&mut self, start: Usn, journal: JournalId, buffer: &mut [u8]) -> Result<usize> {
        self.read_journal(&read_request(start, journal), buffer)
    }

    fn root(&self) -> (PathBuf, FileId) {
        (self.root.clone(), self.root_id)
    }

    fn has_names(&self) -> bool {
        matches!(self.access, Access::Device(_))
    }

    fn lookup_dir(&mut self, id: FileId) -> Option<PathBuf> {
        self.path_of(id).ok()
    }

    fn for_each_directory(&mut self, visit: &mut dyn FnMut(FileId, FileId, &OsStr)) -> Result<()> {
        self.visit_directories(visit)
    }

    fn reparse_tag(&mut self, id: FileId) -> Option<ReparseTag> {
        Volume::reparse_tag(self, id).ok().flatten()
    }

    fn reconnect(&mut self) -> Result<()> {
        self.handle = match &self.access {
            Access::Device(device) => open_device(device)?,
            Access::RootDirectory => open_directory(self.root.as_os_str())?,
        };
        Ok(())
    }
}

/// Lets the health check walk the journal through a shared reference.
struct ReadOnly<'a>(&'a Volume);

impl Source for ReadOnly<'_> {
    fn query(&mut self) -> Result<JournalInfo> {
        self.0.journal_info()
    }

    fn read(&mut self, start: Usn, journal: JournalId, buffer: &mut [u8]) -> Result<usize> {
        self.0.read_journal(&read_request(start, journal), buffer)
    }

    fn root(&self) -> (PathBuf, FileId) {
        (self.0.root.clone(), self.0.root_id)
    }
}

/// Returns the device path to open and the root path that prefixes files.
fn parse_volume_name(name: &str) -> Result<(String, String)> {
    let trimmed = name.trim_end_matches('\\');
    let bytes = trimmed.as_bytes();
    if !bytes.is_empty()
        && bytes.len() <= 2
        && bytes[0].is_ascii_alphabetic()
        && (bytes.len() == 1 || bytes[1] == b':')
    {
        let drive = (bytes[0] as char).to_ascii_uppercase();
        return Ok((format!("\\\\.\\{drive}:"), format!("{drive}:\\")));
    }
    let lower = trimmed.to_ascii_lowercase();
    if (lower.starts_with("\\\\?\\volume{") && lower.ends_with('}')) || is_device_path(&lower) {
        return Ok((trimmed.to_string(), format!("{trimmed}\\")));
    }
    Err(Error::InvalidVolume(name.to_string()))
}

/// True for a device itself, rather than a path to something on it.
fn is_device_path(lower: &str) -> bool {
    lower
        .strip_prefix("\\\\?\\globalroot\\device\\")
        .is_some_and(|device| !device.is_empty() && !device.contains('\\'))
}

fn to_wide_null(text: &OsStr) -> Vec<u16> {
    text.encode_wide().chain(Some(0)).collect()
}

/// The root of the volume holding `path`, with its trailing separator.
fn volume_root(path: &Path) -> Result<OsString> {
    let wide_path = to_wide_null(path.as_os_str());
    let mut units = vec![0u16; 32_768];
    // SAFETY: the path is null-terminated and the buffer is valid for its length.
    let ok =
        unsafe { GetVolumePathNameW(wide_path.as_ptr(), units.as_mut_ptr(), units.len() as u32) };
    if ok == 0 {
        return Err(last_error());
    }
    let length = units.iter().position(|&u| u == 0).unwrap_or(units.len());
    Ok(OsString::from_wide(&units[..length]))
}

fn open_device(device: &str) -> Result<OwnedHandle> {
    let path = to_wide_null(OsStr::new(device));
    // SAFETY: the path is null-terminated and outlives the call.
    let raw = unsafe {
        CreateFileW(
            path.as_ptr(),
            GENERIC_READ,
            SHARE_ALL,
            null(),
            OPEN_EXISTING,
            FILE_ATTRIBUTE_NORMAL,
            null_mut(),
        )
    };
    owned(raw)
}

#[cfg(feature = "journal-admin")]
fn open_device_for_writing(device: &str) -> Result<OwnedHandle> {
    use windows_sys::Win32::Foundation::GENERIC_WRITE;
    let path = to_wide_null(OsStr::new(device));
    // SAFETY: the path is null-terminated and outlives the call.
    let raw = unsafe {
        CreateFileW(
            path.as_ptr(),
            GENERIC_READ | GENERIC_WRITE,
            SHARE_ALL,
            null(),
            OPEN_EXISTING,
            FILE_ATTRIBUTE_NORMAL,
            null_mut(),
        )
    };
    owned(raw)
}

fn open_directory(path: &OsStr) -> Result<OwnedHandle> {
    let path = to_wide_null(path);
    // SAFETY: the path is null-terminated and outlives the call.
    let raw = unsafe {
        CreateFileW(
            path.as_ptr(),
            FILE_READ_ATTRIBUTES,
            SHARE_ALL,
            null(),
            OPEN_EXISTING,
            FILE_FLAG_BACKUP_SEMANTICS,
            null_mut(),
        )
    };
    owned(raw)
}

fn file_id(handle: &OwnedHandle) -> Result<FileId> {
    // SAFETY: FILE_ID_INFO is plain data, valid when zeroed, and the size passed
    // is its own.
    let mut info: FILE_ID_INFO = unsafe { zeroed() };
    let ok = unsafe {
        GetFileInformationByHandleEx(
            handle.as_raw_handle(),
            FileIdInfo,
            &mut info as *mut _ as *mut c_void,
            size_of::<FILE_ID_INFO>() as u32,
        )
    };
    if ok == 0 {
        return Err(last_error());
    }
    Ok(FileId(u128::from_le_bytes(info.FileId.Identifier)))
}

fn owned(raw: *mut c_void) -> Result<OwnedHandle> {
    if raw == INVALID_HANDLE_VALUE || raw.is_null() {
        return Err(last_error());
    }
    // SAFETY: the handle was just returned by the OS and has no other owner.
    Ok(unsafe { OwnedHandle::from_raw_handle(raw) })
}

fn ioctl(
    handle: &OwnedHandle,
    code: u32,
    input: *const c_void,
    input_len: u32,
    output: &mut [u8],
) -> io::Result<usize> {
    let mut returned = 0u32;
    // SAFETY: input and output are valid for the lengths passed, and the call
    // is synchronous because the handle was not opened for overlapped I/O.
    let ok = unsafe {
        DeviceIoControl(
            handle.as_raw_handle(),
            code,
            input,
            input_len,
            output.as_mut_ptr() as *mut c_void,
            output.len() as u32,
            &mut returned,
            null_mut(),
        )
    };
    if ok == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(returned as usize)
}

fn os_error(error: &io::Error) -> Option<u32> {
    error.raw_os_error().map(|code| code as u32)
}

fn last_error() -> Error {
    to_error(io::Error::last_os_error())
}

fn to_error(error: io::Error) -> Error {
    match os_error(&error) {
        Some(ERROR_ACCESS_DENIED) => Error::AccessDenied,
        Some(ERROR_JOURNAL_NOT_ACTIVE) => Error::JournalNotActive,
        Some(ERROR_JOURNAL_DELETE_IN_PROGRESS) => Error::JournalDeleteInProgress,
        Some(ERROR_INVALID_FUNCTION) | Some(ERROR_NOT_SUPPORTED) => Error::Unsupported,
        _ => Error::Io(error),
    }
}

fn strip_leading_separator(units: &[u16]) -> &[u16] {
    match units.first() {
        Some(&unit) if unit == b'\\' as u16 => &units[1..],
        _ => units,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parsed(name: &str) -> (String, String) {
        parse_volume_name(name).unwrap()
    }

    #[test]
    fn accepts_every_drive_form_in_either_case() {
        for form in ["C", "c", "C:", "c:", "C:\\", "c:\\"] {
            assert_eq!(
                parsed(form),
                ("\\\\.\\C:".to_string(), "C:\\".to_string()),
                "{form}"
            );
        }
    }

    #[test]
    fn accepts_volume_guid_paths_with_or_without_a_trailing_separator() {
        let device = "\\\\?\\Volume{0a1b2c3d-0000-0000-0000-000000000000}";
        for form in [device.to_string(), format!("{device}\\")] {
            let (d, r) = parsed(&form);
            assert_eq!(d, device);
            assert_eq!(r, format!("{device}\\"));
        }
    }

    #[test]
    fn accepts_device_paths_with_or_without_a_trailing_separator() {
        for device in [
            "\\\\?\\GLOBALROOT\\Device\\HarddiskVolumeShadowCopy7",
            "\\\\?\\GLOBALROOT\\Device\\HarddiskVolume3",
        ] {
            for form in [device.to_string(), format!("{device}\\")] {
                let (d, r) = parsed(&form);
                assert_eq!(d, device);
                assert_eq!(r, format!("{device}\\"));
            }
        }
    }

    #[test]
    fn rejects_anything_else() {
        for bad in [
            "",
            "1",
            "CC",
            "C:\\Windows",
            "\\\\server\\share",
            "\\\\?\\Volume{x}\\sub",
            "\\\\?\\GLOBALROOT\\Device\\",
            "\\\\?\\GLOBALROOT\\Device\\HarddiskVolumeShadowCopy7\\sub",
        ] {
            assert!(
                matches!(parse_volume_name(bad), Err(Error::InvalidVolume(_))),
                "{bad:?}"
            );
        }
    }

    #[test]
    fn opening_a_malformed_name_touches_nothing() {
        assert!(matches!(
            Volume::open("not a volume"),
            Err(Error::InvalidVolume(_))
        ));
    }
}
