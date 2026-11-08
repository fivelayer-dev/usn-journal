//! Command-line arguments.

use std::time::Duration;

use usn_journal::{ChangeOptions, Cursor, PathFilter, SettleOptions, Start};

pub const USAGE: &str = r"usage:
  usn-watch info     <volume> [--unprivileged]
  usn-watch health   <volume> [--cursor CURSOR] [--required-hours N] [--unprivileged]
  usn-watch position <volume> [--unprivileged]
  usn-watch changes  <volume> --resume CURSOR [--until CURSOR] [filters] [--unprivileged]
  usn-watch scan     <volume> [filters]
  usn-watch watch    <volume> [--resume CURSOR | --oldest] [--settle-ms N]
                     [--max-open-s N] [--poll-ms N] [filters] [--unprivileged]

filters:
  --include PATH           report only this path and the directories leading to it
  --exclude PATH           leave this path out, which wins over --include
  --exclude-system-files   leave out $Extend, pagefile.sys and similar
  --no-directories         report files only

<volume> is a drive such as C:, a path such as \\?\Volume{GUID}\, or a device path
such as that of a volume shadow copy.

--unprivileged reads the journal without elevation. File names are removed, so
changes have no paths and are told apart by id.

Every command writes one JSON object per line, and diagnostics go to stderr. Save
a checkpoint's cursor once the lines before it are handled, and pass it to --resume
to continue with nothing missed.

Exit codes: 0 success, 1 failure, 2 usage error, 3 health found a problem.";

/// The parsed options that any command might use.
#[derive(Debug)]
pub struct Args {
    pub volume: String,
    pub resume: Option<Cursor>,
    pub until: Option<Cursor>,
    pub oldest: bool,
    pub settle_ms: u64,
    pub max_open_s: u64,
    pub poll_ms: u64,
    pub required_hours: u64,
    pub directories: bool,
    pub exclude_system_files: bool,
    pub unprivileged: bool,
    pub includes: Vec<String>,
    pub excludes: Vec<String>,
}

pub fn parse(args: &[String]) -> Result<Args, String> {
    let mut parsed = Args {
        volume: args.first().ok_or("missing <volume>")?.clone(),
        resume: None,
        until: None,
        oldest: false,
        settle_ms: 2_000,
        max_open_s: 300,
        poll_ms: 500,
        required_hours: 24,
        directories: true,
        exclude_system_files: false,
        unprivileged: false,
        includes: Vec::new(),
        excludes: Vec::new(),
    };
    let mut rest = args[1..].iter();
    while let Some(flag) = rest.next() {
        let mut value = || rest.next().ok_or_else(|| format!("{flag} needs a value"));
        match flag.as_str() {
            "--resume" | "--cursor" => parsed.resume = Some(cursor(value()?)?),
            "--until" => parsed.until = Some(cursor(value()?)?),
            "--oldest" => parsed.oldest = true,
            "--settle-ms" => parsed.settle_ms = number(flag, value()?)?,
            "--max-open-s" => parsed.max_open_s = number(flag, value()?)?,
            "--poll-ms" => parsed.poll_ms = number(flag, value()?)?,
            "--required-hours" => parsed.required_hours = number(flag, value()?)?,
            "--no-directories" => parsed.directories = false,
            "--exclude-system-files" => parsed.exclude_system_files = true,
            "--unprivileged" => parsed.unprivileged = true,
            "--include" => parsed.includes.push(value()?.clone()),
            "--exclude" => parsed.excludes.push(value()?.clone()),
            _ => return Err(format!("unknown argument: {flag}")),
        }
    }
    Ok(parsed)
}

fn cursor(text: &str) -> Result<Cursor, String> {
    text.parse().map_err(|e: usn_journal::Error| e.to_string())
}

fn number(flag: &str, value: &str) -> Result<u64, String> {
    value
        .parse()
        .map_err(|_| format!("{flag}: not a number: {value}"))
}

impl Args {
    pub fn filter(&self) -> PathFilter {
        let base = if self.exclude_system_files {
            PathFilter::new().exclude_system_files()
        } else {
            PathFilter::new()
        };
        let base = self.includes.iter().fold(base, |f, path| f.include(path));
        self.excludes.iter().fold(base, |f, path| f.exclude(path))
    }

    pub fn change_options(&self) -> ChangeOptions {
        ChangeOptions::new()
            .filter(self.filter())
            .directories(self.directories)
    }

    pub fn settle_options(&self) -> SettleOptions {
        SettleOptions::new()
            .settle(Duration::from_millis(self.settle_ms))
            .max_open_delay(Duration::from_secs(self.max_open_s))
    }

    pub fn start(&self) -> Start {
        match (self.resume, self.oldest) {
            (Some(cursor), _) => Start::At(cursor),
            (None, true) => Start::Oldest,
            (None, false) => Start::Now,
        }
    }

    /// The volume as a path, for opening it without elevation.
    pub fn volume_path(&self) -> String {
        let volume = self.volume.as_str();
        let bytes = volume.as_bytes();
        match bytes {
            [letter] if letter.is_ascii_alphabetic() => format!("{volume}:\\"),
            [letter, b':'] if letter.is_ascii_alphabetic() => format!("{volume}\\"),
            _ => volume.to_string(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use usn_journal::{JournalId, Usn};

    fn args(list: &[&str]) -> Result<Args, String> {
        let list: Vec<String> = list.iter().map(|s| s.to_string()).collect();
        parse(&list)
    }

    #[test]
    fn a_volume_alone_uses_the_defaults() {
        let a = args(&["C:"]).unwrap();
        assert_eq!(a.volume, "C:");
        assert_eq!((a.settle_ms, a.max_open_s, a.poll_ms), (2_000, 300, 500));
        assert_eq!(a.required_hours, 24);
        assert!(a.directories);
        assert!(!a.exclude_system_files);
        assert!(!a.unprivileged);
        assert_eq!(a.start(), Start::Now);
    }

    #[test]
    fn a_missing_volume_is_a_usage_error() {
        assert!(args(&[]).is_err());
    }

    #[test]
    fn every_flag_is_understood() {
        let a = args(&[
            "D:",
            "--resume",
            "ab:12",
            "--until",
            "ab:34",
            "--settle-ms",
            "1",
            "--max-open-s",
            "2",
            "--poll-ms",
            "3",
            "--required-hours",
            "4",
            "--no-directories",
            "--exclude-system-files",
            "--unprivileged",
            "--include",
            "a",
            "--include",
            "b",
            "--exclude",
            "c",
        ])
        .unwrap();
        let resume = Cursor {
            journal: JournalId(0xab),
            usn: Usn(12),
        };
        assert_eq!(a.resume, Some(resume));
        assert_eq!(a.start(), Start::At(resume));
        assert_eq!(a.until.unwrap().usn, Usn(34));
        assert_eq!(
            (a.settle_ms, a.max_open_s, a.poll_ms, a.required_hours),
            (1, 2, 3, 4)
        );
        assert!(!a.directories);
        assert!(a.exclude_system_files);
        assert!(a.unprivileged);
        assert_eq!(a.includes, ["a", "b"]);
        assert_eq!(a.excludes, ["c"]);
    }

    #[test]
    fn cursor_is_another_name_for_resume() {
        assert!(args(&["C:", "--cursor", "ab:12"]).unwrap().resume.is_some());
    }

    #[test]
    fn oldest_starts_at_the_beginning_unless_resuming() {
        assert_eq!(args(&["C:", "--oldest"]).unwrap().start(), Start::Oldest);
        let a = args(&["C:", "--oldest", "--resume", "ab:12"]).unwrap();
        assert!(matches!(a.start(), Start::At(_)));
    }

    #[test]
    fn bad_input_is_a_usage_error() {
        for bad in [
            &["C:", "--nope"][..],
            &["C:", "--resume"],
            &["C:", "--resume", "not-a-cursor"],
            &["C:", "--poll-ms", "soon"],
            &["C:", "--include"],
        ] {
            assert!(args(bad).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn a_drive_becomes_a_root_path() {
        for (volume, expected) in [
            ("C", "C:\\"),
            ("C:", "C:\\"),
            ("C:\\", "C:\\"),
            ("D:\\data", "D:\\data"),
        ] {
            assert_eq!(args(&[volume]).unwrap().volume_path(), expected);
        }
    }
}
