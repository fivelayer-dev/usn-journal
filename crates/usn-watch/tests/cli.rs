//! Runs the real program. Tests that read a journal use `--unprivileged`, so they
//! need no elevation, except the ones marked ignored, which need real paths.
#![cfg(windows)]

use std::fs;
use std::io::{BufRead, BufReader};
use std::path::PathBuf;
use std::process::{Command, Output, Stdio};
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::{Duration, Instant};

use serde_json::Value;

static NEXT_DIR: AtomicU32 = AtomicU32::new(0);

fn program() -> Command {
    Command::new(env!("CARGO_BIN_EXE_usn-watch"))
}

fn run(args: &[&str]) -> Output {
    program().args(args).output().unwrap()
}

fn drive() -> String {
    std::env::temp_dir()
        .to_string_lossy()
        .chars()
        .take(2)
        .collect()
}

fn temp_dir() -> PathBuf {
    let n = NEXT_DIR.fetch_add(1, Ordering::Relaxed);
    let path = std::env::temp_dir().join(format!("usn-watch-test-{}-{n}", std::process::id()));
    fs::create_dir_all(&path).unwrap();
    path
}

/// Parses every line, which fails the test if any of them isn't valid JSON.
fn lines(output: &Output) -> Vec<Value> {
    String::from_utf8(output.stdout.clone())
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap_or_else(|e| panic!("{e}: {line}")))
        .collect()
}

fn position(extra: &[&str]) -> String {
    let drive = drive();
    let mut args = vec!["position", drive.as_str()];
    args.extend_from_slice(extra);
    let output = run(&args);
    assert!(output.status.success(), "{output:?}");
    lines(&output)[0]["cursor"].as_str().unwrap().to_string()
}

fn assert_usage_error(args: &[&str]) {
    let output = run(args);
    assert_eq!(output.status.code(), Some(2), "{args:?}: {output:?}");
    assert!(output.stdout.is_empty(), "{args:?}");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("usage:"), "{args:?}: {stderr}");
}

#[test]
fn bad_usage_exits_with_2_and_prints_the_usage() {
    assert_usage_error(&[]);
    assert_usage_error(&["nonsense", "C:"]);
    assert_usage_error(&["info"]);
    assert_usage_error(&["info", "C:", "--nope"]);
    assert_usage_error(&["watch", "C:", "--poll-ms"]);
    assert_usage_error(&["watch", "C:", "--poll-ms", "soon"]);
    assert_usage_error(&["changes", "C:", "--resume", "not-a-cursor"]);
}

#[test]
fn changes_needs_a_position_to_resume_from() {
    let drive = drive();
    let output = run(&["changes", &drive, "--unprivileged"]);
    assert_eq!(output.status.code(), Some(2), "{output:?}");
}

#[test]
fn a_malformed_volume_is_a_failure() {
    let output = run(&["info", "not a volume"]);
    assert_eq!(output.status.code(), Some(1), "{output:?}");
    assert!(output.stdout.is_empty());
    assert!(String::from_utf8_lossy(&output.stderr).contains("invalid volume name"));
}

#[test]
fn info_describes_the_journal() {
    let drive = drive();
    let output = run(&["info", &drive, "--unprivileged"]);
    assert!(output.status.success(), "{output:?}");
    let parsed = lines(&output);
    assert_eq!(parsed.len(), 1);
    assert_eq!(parsed[0]["v"], 1);
    assert_eq!(parsed[0]["type"], "journal");
    assert!(parsed[0]["next_usn"].as_i64().unwrap() > 0);
    assert!(parsed[0]["maximum_size"].as_u64().unwrap() > 0);
}

#[test]
fn health_writes_one_line_and_a_documented_exit_code() {
    let drive = drive();
    let output = run(&["health", &drive, "--unprivileged"]);
    assert!(matches!(output.status.code(), Some(0 | 3)), "{output:?}");
    let parsed = lines(&output);
    assert_eq!(parsed.len(), 1);
    assert_eq!(parsed[0]["type"], "health");
    assert!(parsed[0]["issues"].is_array());
}

#[test]
fn health_checks_a_cursor() {
    let drive = drive();
    let cursor = position(&["--unprivileged"]);
    let output = run(&["health", &drive, "--unprivileged", "--cursor", &cursor]);
    let parsed = lines(&output);
    assert_eq!(parsed[0]["cursor"]["cursor"], cursor.as_str());
    assert_eq!(parsed[0]["cursor"]["lost"], false);
}

#[test]
fn health_reports_a_lost_cursor_with_exit_code_3() {
    let drive = drive();
    let output = run(&["health", &drive, "--unprivileged", "--cursor", "1:0"]);
    assert_eq!(output.status.code(), Some(3), "{output:?}");
    let parsed = lines(&output);
    assert_eq!(parsed[0]["status"], "critical");
    assert_eq!(parsed[0]["issues"][0]["kind"], "cursor_lost");
}

#[test]
fn changes_reports_a_new_file_and_ends_with_a_checkpoint() {
    let dir = temp_dir();
    let drive = drive();
    let start = position(&["--unprivileged"]);
    fs::write(dir.join("new.txt"), b"contents").unwrap();

    let output = run(&["changes", &drive, "--unprivileged", "--resume", &start]);
    assert!(output.status.success(), "{output:?}");
    let parsed = lines(&output);
    fs::remove_dir_all(&dir).unwrap();

    assert_eq!(parsed.last().unwrap()["type"], "checkpoint");
    let created: Vec<_> = parsed
        .iter()
        .filter(|l| l["type"] == "change" && l["kind"] == "created" && l["is_directory"] == false)
        .collect();
    assert!(!created.is_empty(), "{parsed:#?}");
    assert_eq!(created[0]["path"], Value::Null, "names are removed");
    assert!(created[0]["id"].as_str().unwrap().starts_with("0x"));
    assert!(!created[0]["reasons"].as_array().unwrap().is_empty());
}

#[test]
fn changes_stops_at_a_chosen_position() {
    let dir = temp_dir();
    let drive = drive();
    let start = position(&["--unprivileged"]);
    let output = run(&[
        "changes",
        &drive,
        "--unprivileged",
        "--resume",
        &start,
        "--until",
        &start,
    ]);
    assert!(output.status.success(), "{output:?}");
    let parsed = lines(&output);
    fs::remove_dir_all(&dir).unwrap();
    assert_eq!(parsed.len(), 1, "{parsed:#?}");
    assert_eq!(parsed[0]["type"], "checkpoint");
}

#[test]
fn a_cursor_from_another_journal_reports_a_gap() {
    let drive = drive();
    let output = run(&["changes", &drive, "--unprivileged", "--resume", "1:0"]);
    assert!(output.status.success(), "{output:?}");
    let parsed = lines(&output);
    assert!(parsed.iter().any(|l| l["type"] == "gap"), "{parsed:#?}");
}

#[test]
fn watch_reports_a_change_and_stops_when_its_output_closes() {
    let dir = temp_dir();
    let drive = drive();
    let mut child = program()
        .args([
            "watch",
            &drive,
            "--unprivileged",
            "--settle-ms",
            "50",
            "--poll-ms",
            "25",
        ])
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let stdout = child.stdout.take().unwrap();
    // Give it a moment to reach the journal's end before making a change.
    std::thread::sleep(Duration::from_millis(500));
    fs::write(dir.join("first.txt"), b"contents").unwrap();

    let mut reader = BufReader::new(stdout);
    let mut line = String::new();
    reader.read_line(&mut line).unwrap();
    let parsed: Value = serde_json::from_str(&line).unwrap();
    assert_eq!(parsed["v"], 1);

    // Closing the pipe makes the next write fail, and the program should exit.
    drop(reader);
    let deadline = Instant::now() + Duration::from_secs(30);
    let mut n = 0;
    let status = loop {
        if let Some(status) = child.try_wait().unwrap() {
            break Some(status);
        }
        if Instant::now() >= deadline {
            child.kill().unwrap();
            break None;
        }
        n += 1;
        fs::write(dir.join(format!("more-{n}.txt")), b"contents").unwrap();
        std::thread::sleep(Duration::from_millis(100));
    };
    fs::remove_dir_all(&dir).unwrap();
    assert!(
        status.is_some(),
        "the program should exit once its output closes"
    );
}

#[test]
#[ignore = "needs an elevated process"]
fn scan_lists_a_file_with_its_path_and_ends_with_a_checkpoint() {
    let dir = temp_dir();
    let drive = drive();
    let path = fs::canonicalize(&dir).unwrap();
    let real = path
        .to_string_lossy()
        .trim_start_matches(r"\\?\")
        .to_string();
    fs::write(dir.join("scanned.txt"), b"contents").unwrap();

    let output = run(&["scan", &drive, "--include", &real]);
    let parsed = lines(&output);
    fs::remove_dir_all(&dir).unwrap();

    assert!(output.status.success(), "{output:?}");
    assert_eq!(parsed.last().unwrap()["type"], "checkpoint");
    let expected = format!(r"{real}\scanned.txt");
    assert!(
        parsed.iter().any(|l| l["path"] == expected.as_str()),
        "{expected} not in {parsed:#?}"
    );
}

#[test]
#[ignore = "needs an elevated process"]
fn changes_reports_full_paths_when_elevated() {
    let dir = temp_dir();
    let drive = drive();
    let path = fs::canonicalize(&dir).unwrap();
    let real = path
        .to_string_lossy()
        .trim_start_matches(r"\\?\")
        .to_string();
    let start = position(&[]);
    fs::write(dir.join("new.txt"), b"contents").unwrap();

    let output = run(&["changes", &drive, "--resume", &start]);
    let parsed = lines(&output);
    fs::remove_dir_all(&dir).unwrap();

    assert!(output.status.success(), "{output:?}");
    let expected = format!(r"{real}\new.txt");
    let change = parsed
        .iter()
        .find(|l| l["path"] == expected.as_str())
        .unwrap_or_else(|| panic!("{expected} not in {parsed:#?}"));
    assert_eq!(change["kind"], "created");
    assert_eq!(change["has_new_content"], true);
}
