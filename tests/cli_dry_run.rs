//! The real binary, in `--dry-run` only, as in every test that starts it.
//!
//! As a second line of defence the child gets a lock path that cannot exist.
//! The gate takes the lock before it opens a device, so if `--dry-run` ever
//! stopped short-circuiting, this would fail at the lock and not reach the bus.

use std::fs;
use std::io::{BufRead, BufReader, Read};
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::time::{Duration, Instant};

use aorus_lcd::bus::lock::LOCK_PATH_ENV;

fn dry_run_probe(arguments: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_aorus-lcd"))
        .args(["probe", "--dry-run"])
        .args(arguments)
        .env(LOCK_PATH_ENV, "/nonexistent/aorus-lcd-test/lock")
        .output()
        .unwrap()
}

fn stdout(output: &Output) -> String {
    String::from_utf8(output.stdout.clone()).unwrap()
}

#[test]
fn probe_dry_run_prints_the_frame_and_says_there_is_no_status() {
    let output = dry_run_probe(&[]);

    assert!(output.status.success(), "{output:?}");
    let text = stdout(&output);
    assert!(text.starts_with("EB GetImageTpl (256 bytes)\n"), "{text}");
    assert!(text.contains("  0000  EB CB 55 AC 38 03 00 00"), "{text}");
    assert!(text.contains("no status to show"), "{text}");
}

// The options that matter to a real run: none of them may make this look for
// hardware, and `--wait` must not wait.
#[test]
fn probe_dry_run_ignores_the_bus_and_does_not_wait() {
    let output = dry_run_probe(&["--bus", "99", "--wait", "3600"]);

    assert!(output.status.success(), "{output:?}");
    assert!(stdout(&output).contains("no status to show"));
}

#[test]
fn verbose_leaves_stdout_alone() {
    let plain = dry_run_probe(&[]);

    let verbose = dry_run_probe(&["-vv"]);

    assert!(verbose.status.success(), "{verbose:?}");
    assert_eq!(stdout(&verbose), stdout(&plain));
}

/// An empty config file, for runs that mean "no config". Without `--config`
/// the binary reads the machine's `/etc/aorus-lcd.toml`, and the tests would see
/// whatever is installed there. It lives in cargo's test scratch directory, not
/// a `TempDir`, because a spawned daemon may read it after this returns. Tests
/// rewrite it at the same time, but always with nothing.
fn empty_config() -> PathBuf {
    let path = Path::new(env!("CARGO_TARGET_TMPDIR")).join("cli_dry_run-empty.toml");
    fs::write(&path, "").unwrap();
    path
}

fn dry_run_daemon(arguments: &[&str]) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_aorus-lcd"));
    command
        .args(["daemon", "--dry-run"])
        .args(arguments)
        .arg("--config")
        .arg(empty_config())
        .env(LOCK_PATH_ENV, "/nonexistent/aorus-lcd-test/lock");
    command
}

fn frames(text: &str, opcode: &str) -> usize {
    let name = format!("{opcode} ");
    text.lines().filter(|line| line.starts_with(&name)).count()
}

// `--max-ticks` is not passed: dry-run has to end by itself. The steady feed,
// because a dry-run reads the real sensors, and on a busy GPU the default feed
// shows nothing in three ticks.
#[test]
fn daemon_dry_run_shows_the_probe_the_read_back_the_dashboard_and_three_ticks() {
    let output = dry_run_daemon(&["--feed", "steady"]).output().unwrap();

    assert!(output.status.success(), "{output:?}");
    let text = stdout(&output);
    assert_eq!(frames(&text, "EB"), 1, "{text}");
    assert_eq!(frames(&text, "DF"), 1, "{text}");
    assert_eq!(frames(&text, "E1"), 1, "{text}");
    assert_eq!(frames(&text, "E3"), 3, "{text}");
    assert_eq!(frames(&text, "AA"), 0, "{text}");
}

// How many frames this shows depends on the load of the machine running it, but
// a tick with no frame must be explained.
#[test]
fn daemon_dry_run_of_the_default_feed_shows_each_tick_or_says_why_not() {
    let output = dry_run_daemon(&[]).output().unwrap();

    assert!(output.status.success(), "{output:?}");
    let text = stdout(&output);
    assert_eq!(frames(&text, "EB"), 1, "{text}");
    assert_eq!(frames(&text, "DF"), 1, "{text}");
    assert_eq!(frames(&text, "E1"), 1, "{text}");
    assert_eq!(frames(&text, "AA"), 0, "{text}");
    assert!(
        frames(&text, "E3") == 3 || text.contains("waiting for a full window"),
        "{text}"
    );
}

#[test]
fn daemon_dry_run_stops_at_max_ticks() {
    let output = dry_run_daemon(&["--max-ticks", "1", "--feed", "steady"])
        .output()
        .unwrap();

    assert!(output.status.success(), "{output:?}");
    assert_eq!(frames(&stdout(&output), "E3"), 1);
}

#[test]
fn daemon_dry_run_sends_the_frame_length_it_is_given() {
    let output = dry_run_daemon(&[
        "--max-ticks",
        "1",
        "--frame-bytes",
        "128",
        "--feed",
        "steady",
    ])
    .output()
    .unwrap();

    assert!(output.status.success(), "{output:?}");
    let text = stdout(&output);
    assert!(text.contains("E3 SensorFeed (128 bytes)"), "{text}");
}

// How many frames this shows depends on the load of the machine running it.
#[test]
fn daemon_dry_run_accepts_the_adaptive_feed() {
    let output = dry_run_daemon(&["--max-ticks", "1", "--feed", "adaptive"])
        .output()
        .unwrap();

    assert!(output.status.success(), "{output:?}");
    let text = stdout(&output);
    assert_eq!(frames(&text, "EB"), 1, "{text}");
    assert_eq!(frames(&text, "AA"), 0, "{text}");
}

#[test]
fn daemon_refuses_other_frame_lengths_and_strategies_before_doing_anything() {
    for arguments in [["--frame-bytes", "64"], ["--feed", "burst"]] {
        let output = dry_run_daemon(&arguments).output().unwrap();

        assert_eq!(output.status.code(), Some(2), "{output:?}");
        assert_eq!(stdout(&output), "");
        let stderr = String::from_utf8(output.stderr).unwrap();
        assert!(stderr.contains(arguments[1]), "{stderr}");
    }
}

/// Sends `signal` to a dry-run that would otherwise last a minute, once its first
/// `E3` is out. Returns its exit and how long that took. The limit in the tests
/// is loose for a loaded machine, and still far under the minute.
fn signalled_daemon(signal: &str) -> (Output, Duration) {
    let mut child = dry_run_daemon(&["--max-ticks", "60", "--feed", "steady"])
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let mut out = BufReader::new(child.stdout.take().unwrap());
    let mut seen = String::new();
    while frames(&seen, "E3") == 0 {
        assert_ne!(out.read_line(&mut seen).unwrap(), 0, "{seen}");
    }

    let sent = Instant::now();
    let kill = Command::new("kill")
        .args([signal, &child.id().to_string()])
        .status()
        .unwrap();
    assert!(kill.success());
    // Drained so that the child cannot block on a full pipe.
    out.read_to_string(&mut seen).unwrap();
    let output = child.wait_with_output().unwrap();

    (output, sent.elapsed())
}

#[test]
fn daemon_exits_cleanly_and_promptly_on_sigterm() {
    let (output, took) = signalled_daemon("-TERM");

    assert!(output.status.success(), "{output:?}");
    assert!(took < Duration::from_secs(5), "{took:?}");
}

#[test]
fn daemon_exits_cleanly_and_promptly_on_sigint() {
    let (output, took) = signalled_daemon("-INT");

    assert!(output.status.success(), "{output:?}");
    assert!(took < Duration::from_secs(5), "{took:?}");
}
