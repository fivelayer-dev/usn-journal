//! Reads the USN change journal and writes JSON Lines on stdout, so that a
//! program in any language can use it. Diagnostics go to stderr.

// Parsing and formatting are portable and tested everywhere, but only the
// Windows build has anything to run.
#![cfg_attr(not(windows), allow(dead_code))]

mod args;
mod json;

#[cfg(windows)]
fn main() -> std::process::ExitCode {
    app::main()
}

#[cfg(not(windows))]
fn main() -> std::process::ExitCode {
    eprintln!("usn-watch only runs on Windows");
    std::process::ExitCode::FAILURE
}

#[cfg(windows)]
mod app {
    use std::io::{self, Write};
    use std::process::ExitCode;
    use std::time::Duration;

    use usn_journal::{
        ChangeBatch, ChangeReader, ChangeStream, Error, HealthOptions, HealthStatus, Result,
        Volume, WatchEvent, WatchOptions,
    };

    use crate::args::{self, Args, USAGE};
    use crate::json;

    /// `health` found a problem. 1 is a failure and 2 is a usage error.
    const EXIT_UNHEALTHY: u8 = 3;
    const EXIT_USAGE: u8 = 2;

    pub fn main() -> ExitCode {
        let all: Vec<String> = std::env::args().skip(1).collect();
        let Some((command, rest)) = all.split_first() else {
            eprintln!("{USAGE}");
            return ExitCode::from(EXIT_USAGE);
        };
        let args = match args::parse(rest) {
            Ok(args) => args,
            Err(message) => {
                eprintln!("{message}\n\n{USAGE}");
                return ExitCode::from(EXIT_USAGE);
            }
        };
        let result = match command.as_str() {
            "info" => info(&args).map(|()| ExitCode::SUCCESS),
            "health" => health(&args),
            "position" => position(&args).map(|()| ExitCode::SUCCESS),
            "changes" => changes(&args),
            "scan" => scan(&args).map(|()| ExitCode::SUCCESS),
            "watch" => watch(&args).map(|()| ExitCode::SUCCESS),
            other => {
                eprintln!("unknown command: {other}\n\n{USAGE}");
                return ExitCode::from(EXIT_USAGE);
            }
        };
        match result {
            Ok(code) => code,
            Err(e) => {
                eprintln!("error: {e}");
                ExitCode::FAILURE
            }
        }
    }

    fn open(args: &Args) -> Result<Volume> {
        if args.unprivileged {
            Volume::open_unprivileged(args.volume_path())
        } else {
            Volume::open(&args.volume)
        }
    }

    fn info(args: &Args) -> Result<()> {
        println!("{}", json::journal_line(&open(args)?.journal_info()?));
        Ok(())
    }

    fn health(args: &Args) -> Result<ExitCode> {
        let required = Duration::from_secs(args.required_hours.saturating_mul(3600));
        let mut options = HealthOptions::new().required_history(required);
        if let Some(cursor) = args.resume {
            options = options.cursor(cursor);
        }
        let health = open(args)?.health(&options)?;
        println!("{}", json::health_line(&health));
        Ok(if health.status() == HealthStatus::Ok {
            ExitCode::SUCCESS
        } else {
            ExitCode::from(EXIT_UNHEALTHY)
        })
    }

    fn position(args: &Args) -> Result<()> {
        let end = open(args)?.journal_info()?.end_cursor();
        println!("{}", json::checkpoint_line(end, 0, 0));
        Ok(())
    }

    fn changes(args: &Args) -> Result<ExitCode> {
        let Some(resume) = args.resume else {
            eprintln!("changes needs --resume\n\n{USAGE}");
            return Ok(ExitCode::from(EXIT_USAGE));
        };
        let start = usn_journal::Start::At(resume);
        let mut reader = ChangeReader::new(open(args)?, start, args.change_options())?;
        let batch = match args.until {
            Some(until) => reader.read_to(until)?,
            None => reader.read_to_end()?,
        };
        let mut out = io::BufWriter::new(io::stdout().lock());
        write_batch(&mut out, &batch).and_then(|()| out.flush())?;
        Ok(ExitCode::SUCCESS)
    }

    fn scan(args: &Args) -> Result<()> {
        let volume = open(args)?;
        let scan = volume.scan(&args.filter())?;
        let cursor = scan.start_cursor();
        let mut out = io::BufWriter::new(io::stdout().lock());
        for entry in scan {
            if writeln!(out, "{}", json::scan_line(&entry?)).is_err() {
                // A closed pipe means the consuming process has gone.
                return Ok(());
            }
        }
        // Lag and headroom describe a running reader, not a scan.
        let _ = writeln!(out, "{}", json::checkpoint_line(cursor, 0, 0));
        let _ = out.flush();
        Ok(())
    }

    fn watch(args: &Args) -> Result<()> {
        let stream = ChangeStream::new(
            open(args)?,
            args.start(),
            args.change_options(),
            args.settle_options(),
        )?;
        let options = WatchOptions::new().poll_interval(Duration::from_millis(args.poll_ms));
        let watcher = stream.watch(options)?;
        let stdout = io::stdout();
        for event in watcher.events() {
            let mut out = stdout.lock();
            let mut failure: Option<Error> = None;
            let written = match event {
                WatchEvent::Batch(batch) => write_batch(&mut out, &batch),
                WatchEvent::Retrying(e) => {
                    writeln!(out, "{}", json::error_line(&e.to_string(), true))
                }
                WatchEvent::Stopped(e) => {
                    let line = writeln!(out, "{}", json::error_line(&e.to_string(), false));
                    failure = Some(e);
                    line
                }
            };
            // A closed pipe means the consuming process has gone, so stop too.
            if written.and_then(|()| out.flush()).is_err() {
                break;
            }
            if let Some(e) = failure {
                return Err(e);
            }
        }
        Ok(())
    }

    fn write_batch(out: &mut impl Write, batch: &ChangeBatch) -> io::Result<()> {
        if let Some(gap) = &batch.gap {
            writeln!(out, "{}", json::gap_line(gap))?;
        }
        for change in &batch.changes {
            writeln!(out, "{}", json::change_line(change))?;
        }
        writeln!(
            out,
            "{}",
            json::checkpoint_line(batch.cursor, batch.lag_bytes, batch.headroom_bytes)
        )
    }
}
