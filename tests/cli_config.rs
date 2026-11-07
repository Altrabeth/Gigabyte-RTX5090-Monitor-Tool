//! The config file through the binary. Every run here is a dry-run.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use aorus_lcd::bus::lock::LOCK_PATH_ENV;
use tempfile::TempDir;

/// The frames up to the first `E3`'s header, which has its length. Its values are
/// live sensor readings, so they cannot be compared between two runs.
fn setup_frames(output: &Output) -> String {
    let text = stdout(output);
    let feed = text.find("\nE3 ").expect("a dry-run shows a tick") + 1;
    let header_end = text[feed..].find('\n').unwrap();
    text[..feed + header_end].to_owned()
}

/// A dry-run with this config file, or with an empty one for `None`. Never
/// without `--config`: the binary would then read the machine's
/// `/etc/aorus-lcd.toml`, and the test would see whatever is installed there.
/// That a missing default file means defaults is a unit test in `config.rs`.
fn dry_run(config: Option<&Path>, arguments: &[&str]) -> Output {
    let (_dir, empty) = config_file(b"");
    Command::new(env!("CARGO_BIN_EXE_aorus-lcd"))
        .args(["daemon", "--dry-run", "--max-ticks", "1"])
        .args(arguments)
        .arg("--config")
        .arg(config.unwrap_or(&empty))
        .env(LOCK_PATH_ENV, "/nonexistent/aorus-lcd-test/lock")
        .output()
        .unwrap()
}

/// A [`dry_run`] on the steady feed, for tests that need the first `E3`: it reads
/// the real sensors, and on a busy GPU the default feed sends nothing on its first
/// ticks. The option beats any `feed` in the file, so a test using this says
/// nothing about that key.
fn steady_dry_run(config: Option<&Path>, arguments: &[&str]) -> Output {
    dry_run(config, &[&["--feed", "steady"], arguments].concat())
}

fn config_file(text: &[u8]) -> (TempDir, PathBuf) {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("aorus-lcd.toml");
    fs::write(&path, text).unwrap();
    (dir, path)
}

fn with_config(text: &str, arguments: &[&str]) -> Output {
    let (_dir, path) = config_file(text.as_bytes());
    dry_run(Some(&path), arguments)
}

fn steady_with_config(text: &str, arguments: &[&str]) -> Output {
    let (_dir, path) = config_file(text.as_bytes());
    steady_dry_run(Some(&path), arguments)
}

fn stdout(output: &Output) -> String {
    String::from_utf8(output.stdout.clone()).unwrap()
}

fn stderr(output: &Output) -> String {
    String::from_utf8(output.stderr.clone()).unwrap()
}

/// Exit status 1 from `main` and no frame shown: nothing got near a connect.
fn assert_refused(output: &Output, expected: &str) {
    assert_eq!(output.status.code(), Some(1), "{output:?}");
    assert_eq!(stdout(output), "", "{output:?}");
    let message = stderr(output);
    assert!(message.contains(expected), "{message}");
}

#[test]
fn the_file_sets_what_the_options_set() {
    // The steady feed, because adaptive sends nothing on its first ticks when the
    // GPU is busy, and a dry-run reads the real sensors.
    let text = "widgets = \"fps\"\ninterval = 9\nframe_bytes = 128\nfeed = \"steady\"\n";

    let output = with_config(text, &[]);

    assert!(output.status.success(), "{output:?}");
    let options = [
        "--widgets",
        "fps",
        "--interval",
        "9",
        "--frame-bytes",
        "128",
        "--feed",
        "steady",
    ];
    assert_eq!(
        setup_frames(&output),
        setup_frames(&dry_run(None, &options))
    );
    assert!(setup_frames(&output).ends_with("(128 bytes)"));
}

/// Whether a tick sends anything under this feed depends on the live GPU load,
/// so this asserts only that the file is accepted.
#[test]
fn the_file_can_choose_the_adaptive_feed() {
    let output = with_config("feed = \"adaptive\"\n", &[]);

    assert!(output.status.success(), "{output:?}");
}

#[test]
fn the_command_line_beats_the_file() {
    let text = "widgets = \"temp\"\ninterval = 7\n";

    let output = steady_with_config(text, &["--widgets", "fps", "--interval", "9"]);

    assert!(output.status.success(), "{output:?}");
    let expected = steady_with_config("widgets = \"fps\"\ninterval = 9\n", &[]);
    assert_eq!(setup_frames(&output), setup_frames(&expected));
    assert_ne!(
        setup_frames(&output),
        setup_frames(&steady_with_config(text, &[]))
    );
}

#[test]
fn the_shipped_example_is_the_same_as_no_options() {
    let example = Path::new(env!("CARGO_MANIFEST_DIR")).join("dist/aorus-lcd.toml");

    let output = steady_dry_run(Some(&example), &[]);

    assert!(output.status.success(), "{output:?}");
    assert_eq!(
        setup_frames(&output),
        setup_frames(&steady_dry_run(None, &[]))
    );
}

#[test]
fn a_bad_file_stops_a_dry_run_and_names_the_line() {
    let output = with_config("feed = \"steady\"\n\nnope = 1\n", &[]);
    assert_refused(&output, "line 3, column 1: unknown field `nope`");

    let output = with_config("\ninterval = 0\nwidgets = \"fps\"\n", &[]);
    assert_refused(&output, "line 2, column 12: interval must be 1 to 255");
}

#[test]
fn an_interval_alone_is_refused_from_either_source() {
    let output = with_config("interval = 9\n", &[]);
    assert_refused(&output, "line 1, column 12: an interval needs widgets");
    assert_refused(&output, "goes out with E1");

    assert_refused(
        &dry_run(None, &["--interval", "9"]),
        "--interval: an interval needs widgets",
    );
}

#[test]
fn an_interval_is_accepted_with_widgets_from_the_other_source() {
    let cases = [
        ("interval = 9\n", ["--widgets", "fps"]),
        ("widgets = \"fps\"\n", ["--interval", "9"]),
    ];

    for (text, arguments) in cases {
        let output = with_config(text, &arguments);

        assert!(output.status.success(), "{output:?}");
    }
}

#[test]
fn a_missing_explicit_file_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("absent.toml");

    assert_refused(&dry_run(Some(&path), &[]), "cannot read config file");
}

#[test]
fn a_file_that_is_not_utf8_is_refused() {
    let (_dir, path) = config_file(b"feed = \"\xff\"\n");

    assert_refused(&dry_run(Some(&path), &[]), "is not UTF-8");
}

#[test]
fn verbose_says_which_file_was_read() {
    let (_dir, path) = config_file(b"");

    let output = dry_run(Some(&path), &["-v"]);

    let message = stderr(&output);
    assert!(message.contains("read config file"), "{message}");
}
