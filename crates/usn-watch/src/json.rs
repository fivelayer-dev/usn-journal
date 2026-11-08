//! The JSON Lines format this program writes for other processes to consume.
//!
//! Every line starts with `"v"`, the format version, which changes only when a
//! line changes in a way that could break a reader. Written by hand so that the
//! library has no dependencies and unpaired surrogates in file names survive.

use std::ffi::OsStr;
use std::fmt::Write;
use std::path::Path;
use std::time::UNIX_EPOCH;

use usn_journal::{
    Cursor, FileAttributes, FileChange, FileTime, Gap, JournalHealth, JournalInfo, ScanEntry,
};

const VERSION: u32 = 1;

pub fn change_line(c: &FileChange) -> String {
    let mut out = String::with_capacity(256);
    let _ = write!(
        out,
        "{{\"v\":{VERSION},\"type\":\"change\",\"kind\":\"{}\",\"path\":",
        c.kind
    );
    push_opt_path(&mut out, c.path.as_deref());
    out.push_str(",\"previous_path\":");
    push_opt_path(&mut out, c.previous_path.as_deref());
    let _ = write!(
        out,
        ",\"is_directory\":{},\"has_new_content\":{},\"is_reparse_point\":{},\"is_link\":{},\"reparse_tag\":{},\"still_open\":{},\"id\":\"{}\",\"parent_id\":\"{}\",\"first_usn\":{},\"last_usn\":{},\"timestamp_ms\":{},\"attributes\":{},\"reasons\":[",
        c.is_directory(),
        c.has_new_content(),
        c.is_reparse_point(),
        c.is_link(),
        opt_number(c.reparse_tag.map(|tag| tag.0)),
        c.still_open,
        c.id,
        c.parent_id,
        c.first_usn,
        c.last_usn,
        opt_number(unix_ms(c.timestamp)),
        c.attributes.0,
    );
    for (i, name) in c.reasons.names().enumerate() {
        if i > 0 {
            out.push(',');
        }
        let _ = write!(out, "\"{name}\"");
    }
    out.push_str("]}");
    out
}

pub fn scan_line(f: &ScanEntry) -> String {
    let mut out = String::with_capacity(128);
    let _ = write!(out, "{{\"v\":{VERSION},\"type\":\"file\",\"path\":");
    push_opt_path(&mut out, f.path.as_deref());
    let _ = write!(
        out,
        ",\"is_directory\":{},\"is_reparse_point\":{},\"is_link\":{},\"reparse_tag\":{},\"id\":\"{}\",\"parent_id\":\"{}\",\"attributes\":{},\"usn\":{}}}",
        f.is_directory(),
        f.attributes.contains(FileAttributes::REPARSE_POINT),
        f.is_link(),
        opt_number(f.reparse_tag.map(|tag| tag.0)),
        f.id,
        f.parent_id,
        f.attributes.0,
        f.usn,
    );
    out
}

pub fn gap_line(gap: &Gap) -> String {
    let mut out = format!("{{\"v\":{VERSION},\"type\":\"gap\",\"detail\":");
    push_string(&mut out, OsStr::new(&gap.to_string()));
    out.push('}');
    out
}

pub fn checkpoint_line(cursor: Cursor, lag_bytes: u64, headroom_bytes: u64) -> String {
    format!(
        "{{\"v\":{VERSION},\"type\":\"checkpoint\",\"cursor\":\"{cursor}\",\"lag_bytes\":{lag_bytes},\"headroom_bytes\":{headroom_bytes}}}"
    )
}

pub fn error_line(message: &str, retrying: bool) -> String {
    let mut out =
        format!("{{\"v\":{VERSION},\"type\":\"error\",\"retrying\":{retrying},\"message\":");
    push_string(&mut out, OsStr::new(message));
    out.push('}');
    out
}

pub fn journal_line(i: &JournalInfo) -> String {
    format!(
        "{{\"v\":{VERSION},\"type\":\"journal\",\"journal_id\":\"{}\",\"first_usn\":{},\"next_usn\":{},\"lowest_valid_usn\":{},\"max_usn\":{},\"maximum_size\":{},\"allocation_delta\":{},\"min_version\":{},\"max_version\":{}}}",
        i.id,
        i.first_usn,
        i.next_usn,
        i.lowest_valid_usn,
        i.max_usn,
        i.maximum_size,
        i.allocation_delta,
        i.min_version,
        i.max_version,
    )
}

pub fn health_line(h: &JournalHealth) -> String {
    let mut out = String::with_capacity(512);
    let _ = write!(
        out,
        "{{\"v\":{VERSION},\"type\":\"health\",\"status\":\"{}\",\"journal_id\":\"{}\",\"maximum_size\":{},\"allocation_delta\":{},\"has_purged\":{},\"oldest_record_ms\":{},\"history_s\":{},\"active_history_s\":{},\"bytes_per_hour\":{},\"estimated_capacity_s\":{},\"required_history_s\":{},\"cursor\":",
        h.status(),
        h.info.id,
        h.info.maximum_size,
        h.info.allocation_delta,
        h.has_purged,
        opt_number(h.oldest_record.and_then(unix_ms)),
        opt_number(h.history.map(|d| d.as_secs())),
        h.active_history.as_secs(),
        opt_number(h.bytes_per_hour),
        opt_number(h.estimated_capacity.map(|d| d.as_secs())),
        h.required_history.as_secs(),
    );
    match &h.cursor {
        Some(c) => {
            let _ = write!(
                out,
                "{{\"cursor\":\"{}\",\"lost\":{},\"lag_bytes\":{},\"bytes_until_lost\":{},\"time_until_lost_s\":{}}}",
                c.cursor,
                c.gap.is_some(),
                c.lag_bytes,
                c.bytes_until_lost,
                opt_number(c.time_until_lost.map(|d| d.as_secs())),
            );
        }
        None => out.push_str("null"),
    }
    out.push_str(",\"issues\":[");
    for (i, issue) in h.issues.iter().enumerate() {
        if i > 0 {
            out.push(',');
        }
        let _ = write!(
            out,
            "{{\"kind\":\"{}\",\"severity\":\"{}\",\"detail\":",
            issue.kind(),
            issue.severity(),
        );
        push_string(&mut out, OsStr::new(&issue.to_string()));
        out.push('}');
    }
    out.push_str("]}");
    out
}

/// Milliseconds since the Unix epoch, or `None` for a time out of range.
fn unix_ms(time: FileTime) -> Option<i128> {
    let time = time.to_system_time()?;
    Some(match time.duration_since(UNIX_EPOCH) {
        Ok(after) => after.as_millis() as i128,
        Err(before) => -(before.duration().as_millis() as i128),
    })
}

fn opt_number<T: ToString>(value: Option<T>) -> String {
    value.map_or("null".to_string(), |v| v.to_string())
}

fn push_opt_path(out: &mut String, path: Option<&Path>) {
    match path {
        Some(p) => push_string(out, p.as_os_str()),
        None => out.push_str("null"),
    }
}

#[cfg(windows)]
fn utf16(text: &OsStr) -> Vec<u16> {
    use std::os::windows::ffi::OsStrExt;
    text.encode_wide().collect()
}

#[cfg(not(windows))]
fn utf16(text: &OsStr) -> Vec<u16> {
    text.to_string_lossy().encode_utf16().collect()
}

/// Writes a JSON string. Unpaired surrogates, which NTFS allows in names, become
/// `\uXXXX` escapes, so a consumer gets back the exact name it needs to open
/// the file.
fn push_string(out: &mut String, text: &OsStr) {
    out.push('"');
    for unit in char::decode_utf16(utf16(text)) {
        match unit {
            Ok('"') => out.push_str("\\\""),
            Ok('\\') => out.push_str("\\\\"),
            Ok('\n') => out.push_str("\\n"),
            Ok('\r') => out.push_str("\\r"),
            Ok('\t') => out.push_str("\\t"),
            Ok(c) if (c as u32) < 0x20 => {
                let _ = write!(out, "\\u{:04x}", c as u32);
            }
            Ok(c) => out.push(c),
            Err(lone) => {
                let _ = write!(out, "\\u{:04x}", lone.unpaired_surrogate());
            }
        }
    }
    out.push('"');
}

#[cfg(test)]
mod tests {
    use super::*;
    use usn_journal::{JournalId, Usn};

    fn json(text: &str) -> String {
        let mut out = String::new();
        push_string(&mut out, OsStr::new(text));
        out
    }

    #[test]
    fn escapes_json_special_characters() {
        assert_eq!(json("C:\\a\"b"), r#""C:\\a\"b""#);
        assert_eq!(json("a\nb\tc\u{1}"), r#""a\nb\tc\u0001""#);
    }

    #[test]
    fn keeps_non_ascii_and_astral_characters_literal() {
        assert_eq!(json("日本語😀"), "\"日本語😀\"");
    }

    #[cfg(windows)]
    #[test]
    fn escapes_unpaired_surrogates() {
        use std::ffi::OsString;
        use std::os::windows::ffi::OsStringExt;
        let mut out = String::new();
        push_string(&mut out, &OsString::from_wide(&[0x61, 0xD800, 0x62]));
        assert_eq!(out, r#""a\ud800b""#);
    }

    #[test]
    fn checkpoint_line_has_every_field() {
        let cursor = Cursor {
            journal: JournalId(0xab),
            usn: Usn(9),
        };
        assert_eq!(
            checkpoint_line(cursor, 1, 2),
            r#"{"v":1,"type":"checkpoint","cursor":"ab:9","lag_bytes":1,"headroom_bytes":2}"#
        );
    }

    #[test]
    fn gap_line_says_what_was_lost() {
        let gap = gap_line(&Gap::RecordsLost {
            cursor: Usn(1),
            oldest_readable: Usn(2),
        });
        assert_eq!(
            gap,
            r#"{"v":1,"type":"gap","detail":"records lost: position 1, oldest readable is 2"}"#
        );
    }

    #[test]
    fn error_line_says_whether_it_is_retrying() {
        assert_eq!(
            error_line("gone", true),
            r#"{"v":1,"type":"error","retrying":true,"message":"gone"}"#
        );
        assert_eq!(
            error_line("gone", false),
            r#"{"v":1,"type":"error","retrying":false,"message":"gone"}"#
        );
    }

    #[test]
    fn times_convert_to_milliseconds_or_null() {
        assert_eq!(
            unix_ms(FileTime(132_223_104_000_000_000)),
            Some(1_577_836_800_000)
        );
        assert_eq!(unix_ms(FileTime(i64::MIN)), None);
    }
}
