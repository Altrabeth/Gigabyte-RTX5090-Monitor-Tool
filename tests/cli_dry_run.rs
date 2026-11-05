//! The real binary, in `--dry-run` only, as in every test that starts it.
//!
//! As a second line of defence the child gets a lock path that cannot exist.
//! The gate takes the lock before it opens a device, so if `--dry-run` ever
//! stopped short-circuiting, this would fail at the lock and not reach the bus.

use std::process::{Command, Output};

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
