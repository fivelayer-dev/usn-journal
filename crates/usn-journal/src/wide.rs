//! NTFS names are arbitrary UTF-16 and may contain unpaired surrogates, which do
//! not survive conversion to `String`. Names are kept as `OsString` so that a
//! file with such a name can still be opened.

use std::ffi::{OsStr, OsString};

#[cfg(windows)]
pub(crate) fn from_utf16(units: &[u16]) -> OsString {
    use std::os::windows::ffi::OsStringExt;
    OsString::from_wide(units)
}

#[cfg(not(windows))]
pub(crate) fn from_utf16(units: &[u16]) -> OsString {
    OsString::from(String::from_utf16_lossy(units))
}

#[cfg(windows)]
pub(crate) fn to_utf16(text: &OsStr) -> Vec<u16> {
    use std::os::windows::ffi::OsStrExt;
    text.encode_wide().collect()
}

#[cfg(not(windows))]
pub(crate) fn to_utf16(text: &OsStr) -> Vec<u16> {
    text.to_string_lossy().encode_utf16().collect()
}
