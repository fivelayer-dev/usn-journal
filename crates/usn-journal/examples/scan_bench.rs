//! Measures the operations whose cost grows with the size of a volume: reading
//! the master file table, starting a change stream, and a full scan. It only
//! reads the volume, but needs an elevated process.
//!
//! ```text
//! cargo run --release --example scan_bench -- C: 3
//! ```
//!
//! Compare runs on the same machine. The first run after a reboot is usually
//! the slowest, because the master file table isn't cached yet.

#[cfg(windows)]
fn main() -> std::process::ExitCode {
    use std::process::ExitCode;

    let mut args = std::env::args().skip(1);
    let volume = args.next().unwrap_or_else(|| "C:".to_string());
    let runs: u32 = match args.next().map(|runs| runs.parse()) {
        None => 3,
        Some(Ok(runs)) => runs,
        Some(Err(_)) => {
            eprintln!("usage: scan_bench [volume] [runs]");
            return ExitCode::from(2);
        }
    };
    match bench::run(&volume, runs) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("error: {error}");
            ExitCode::FAILURE
        }
    }
}

#[cfg(not(windows))]
fn main() {
    eprintln!("scan_bench only runs on Windows");
}

#[cfg(windows)]
mod bench {
    use std::time::{Duration, Instant};

    use usn_journal::{ChangeOptions, ChangeReader, PathFilter, Result, Start, Volume};

    pub fn run(name: &str, runs: u32) -> Result<()> {
        if cfg!(debug_assertions) {
            eprintln!("warning: this is a debug build, so these timings mean little");
        }
        println!("volume {name}");
        for run in 1..=runs {
            println!("run {run}:");
            read_mft(name)?;
            start_reader(name)?;
            scan(name)?;
        }
        Ok(())
    }

    fn read_mft(name: &str) -> Result<()> {
        let volume = Volume::open(name)?;
        let (mut entries, mut directories) = (0u64, 0u64);
        let started = Instant::now();
        for entry in volume.mft_entries()? {
            entries += 1;
            directories += u64::from(entry?.is_directory());
        }
        let elapsed = started.elapsed();
        println!(
            "  read MFT:     {} ({entries} entries, {directories} directories, {}/s)",
            seconds(elapsed),
            per_second(entries, elapsed),
        );
        Ok(())
    }

    fn start_reader(name: &str) -> Result<()> {
        let volume = Volume::open(name)?;
        let started = Instant::now();
        let reader = ChangeReader::new(volume, Start::Now, ChangeOptions::new())?;
        println!("  start reader: {}", seconds(started.elapsed()));
        drop(reader);
        Ok(())
    }

    fn scan(name: &str) -> Result<()> {
        let volume = Volume::open(name)?;
        let (mut files, mut without_path) = (0u64, 0u64);
        let started = Instant::now();
        for entry in volume.scan(&PathFilter::new())? {
            files += 1;
            without_path += u64::from(entry?.path.is_none());
        }
        let elapsed = started.elapsed();
        println!(
            "  full scan:    {} ({files} entries, {without_path} without a path, {}/s)",
            seconds(elapsed),
            per_second(files, elapsed),
        );
        Ok(())
    }

    fn seconds(duration: Duration) -> String {
        format!("{:.2}s", duration.as_secs_f64())
    }

    fn per_second(count: u64, duration: Duration) -> u64 {
        (count as f64 / duration.as_secs_f64().max(f64::MIN_POSITIVE)) as u64
    }
}
