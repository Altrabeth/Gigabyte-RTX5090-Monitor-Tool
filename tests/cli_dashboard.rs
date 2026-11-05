//! `aorus-lcd dashboard` through the real binary, in `--dry-run` only.
//!
//! The expected rows are written by hand from `docs/PROTOCOL.md` section 3:
//! opcode, magic, flags `[5..12]`, interval `[13]`, padding. As in
//! `cli_dry_run.rs`, the child gets a lock path that cannot exist, so if
//! `--dry-run` ever stopped short-circuiting it would fail at the lock.

use std::process::{Command, Output};

use aorus_lcd::bus::lock::LOCK_PATH_ENV;

/// All-zero padding of a frame, after its first row.
const ZERO_ROW: &str = "00 00 00 00 00 00 00 00 00 00 00 00 00 00 00 00";

fn dashboard(arguments: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_aorus-lcd"))
        .args(["dashboard", "--dry-run"])
        .args(arguments)
        .env(LOCK_PATH_ENV, "/nonexistent/aorus-lcd-test/lock")
        .output()
        .unwrap()
}

fn stdout(output: &Output) -> String {
    String::from_utf8(output.stdout.clone()).unwrap()
}

fn stderr(output: &Output) -> String {
    String::from_utf8(output.stderr.clone()).unwrap()
}

/// The first line of every frame in a dump, whose rows are indented.
fn frame_headers(dump: &str) -> Vec<&str> {
    dump.lines()
        .take_while(|line| !line.starts_with("Note:"))
        .filter(|line| !line.starts_with(' '))
        .collect()
}

/// The hex of each row of the `E1` frame.
fn e1_rows(dump: &str) -> Vec<&str> {
    dump.lines()
        .skip_while(|line| !line.starts_with("E1 "))
        .skip(1)
        .take_while(|line| line.starts_with(' '))
        .map(|line| line.trim_start().split_once("  ").unwrap().1)
        .collect()
}

/// Checks for one probe, then one `E1` with this first row and zeros after it.
fn assert_sends_only_e1(output: &Output, first_row: &str) {
    assert!(output.status.success(), "{output:?}");
    let dump = stdout(output);

    assert_eq!(
        frame_headers(&dump),
        ["EB GetImageTpl (256 bytes)", "E1 SetDisplay (256 bytes)"],
        "{dump}"
    );
    let rows = e1_rows(&dump);
    assert_eq!(rows.len(), 16, "{dump}");
    assert_eq!(rows[0], first_row);
    assert!(rows[1..].iter().all(|row| *row == ZERO_ROW), "{dump}");
}

fn assert_refused(output: &Output, message: &str) {
    assert!(!output.status.success(), "{output:?}");
    assert!(stderr(output).contains(message), "{output:?}");
    assert_eq!(stdout(output), "", "nothing may be sent");
}

#[test]
fn without_flags_it_sends_the_default_widgets_every_3_seconds() {
    assert_sends_only_e1(
        &dashboard(&[]),
        "E1 CB 55 AC 38 01 01 01 01 00 01 00 01 03 00 00",
    );
}

#[test]
fn the_readme_example_matches_the_default() {
    let output = dashboard(&[
        "--widgets",
        "temp,clock,usage,fan,vram,power",
        "--interval",
        "3",
    ]);

    assert_sends_only_e1(&output, "E1 CB 55 AC 38 01 01 01 01 00 01 00 01 03 00 00");
}

#[test]
fn the_widgets_outside_the_default_can_be_chosen() {
    assert_sends_only_e1(
        &dashboard(&["--widgets", "fps,ramclock", "--interval", "7"]),
        "E1 CB 55 AC 38 00 00 00 00 01 00 01 00 07 00 00",
    );
}

#[test]
fn every_widget_lands_on_its_own_flag_byte() {
    let cases = [
        ("temp", "01 00 00 00 00 00 00 00"),
        ("clock", "00 01 00 00 00 00 00 00"),
        ("usage", "00 00 01 00 00 00 00 00"),
        ("fan", "00 00 00 01 00 00 00 00"),
        ("ramclock", "00 00 00 00 01 00 00 00"),
        ("vram", "00 00 00 00 00 01 00 00"),
        ("fps", "00 00 00 00 00 00 01 00"),
        ("power", "00 00 00 00 00 00 00 01"),
    ];

    for (widget, flags) in cases {
        assert_sends_only_e1(
            &dashboard(&["--widgets", widget, "--interval", "1"]),
            &format!("E1 CB 55 AC 38 {flags} 01 00 00"),
        );
    }
}

#[test]
fn off_sends_all_zero_flags_and_a_zero_interval() {
    assert_sends_only_e1(
        &dashboard(&["--off"]),
        "E1 CB 55 AC 38 00 00 00 00 00 00 00 00 00 00 00",
    );
}

#[test]
fn the_note_follows_the_frame() {
    for arguments in [&[][..], &["--off"]] {
        let dump = stdout(&dashboard(arguments));
        let note = dump.split_once("Note:").unwrap().1;

        assert!(note.contains("not saved"), "{dump}");
        assert!(
            note.contains("widgets freeze without a running `aorus-lcd daemon`"),
            "{dump}"
        );
        assert!(note.contains("`aorus-lcd recover`"), "{dump}");
    }
}

#[test]
fn help_warns_about_saving_and_the_daemon() {
    let output = dashboard(&["--help"]);

    assert!(output.status.success(), "{output:?}");
    let help = stdout(&output);
    assert!(help.contains("not saved"), "{help}");
    assert!(
        help.contains("widgets freeze without a running `aorus-lcd daemon`"),
        "{help}"
    );
}

#[test]
fn bad_lists_are_refused_before_anything_is_sent() {
    assert_refused(
        &dashboard(&["--widgets", "temp,gpu"]),
        "unknown widget \"gpu\": valid widgets are \
         temp, clock, usage, fan, ramclock, vram, fps, power",
    );
    assert_refused(&dashboard(&["--widgets", ""]), "empty");
    assert_refused(&dashboard(&["--widgets", "fan,fan"]), "listed twice");
}

#[test]
fn an_interval_of_zero_is_refused_before_anything_is_sent() {
    assert_refused(&dashboard(&["--interval", "0"]), "--interval");
}

#[test]
fn off_conflicts_with_what_it_would_ignore() {
    assert_refused(
        &dashboard(&["--off", "--widgets", "temp"]),
        "cannot be used with",
    );
    assert_refused(
        &dashboard(&["--off", "--interval", "5"]),
        "cannot be used with",
    );
}
