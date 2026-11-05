//! `on`, `off`, `mode`, `save`, `carousel` and `status` through the real binary,
//! in `--dry-run` only.
//!
//! As in `cli_dry_run.rs`, the child gets a lock path that cannot exist, so a
//! broken `--dry-run` stops at the lock and does not reach the bus. The one
//! helper that starts the child always passes `--dry-run`. The expected frames
//! are written out by hand, not built with the code under test.

use std::process::{Command, Output};

use aorus_lcd::bus::lock::LOCK_PATH_ENV;

const ZEROS: &str = "00 00 00 00 00 00 00 00 00 00 00 00 00 00 00 00";

const NO_STATUS: &str = "dry-run: read-backs are unavailable, so there is no status to show\n";
const WOULD_SAVE: &str = "dry-run: a real run writes the panel configuration to NVRAM\n";

fn dry_run(arguments: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_aorus-lcd"))
        .arg("--dry-run")
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

/// The dump of a 256-byte command frame whose bytes after the first row are all
/// zero, as every frame in this file is.
fn dump(header: &str, first_row: &str) -> String {
    let mut text = format!("{header} (256 bytes)\n  0000  {first_row}\n");
    for row in 1..16 {
        text.push_str(&format!("  {:04X}  {ZEROS}\n", row * 16));
    }
    text
}

/// `connect` shows the probe first, because a real run sends it first.
fn probe() -> String {
    dump(
        "EB GetImageTpl",
        "EB CB 55 AC 38 03 00 00 00 00 00 00 00 00 00 00",
    )
}

fn assert_prints(arguments: &[&str], expected: &str) {
    let output = dry_run(arguments);

    assert!(output.status.success(), "{arguments:?}: {output:?}");
    assert_eq!(stdout(&output), expected, "{arguments:?}");
}

/// Refused with nothing printed, so before the probe and any bus access.
fn assert_refused(arguments: &[&str], message: &str) {
    let output = dry_run(arguments);

    assert!(!output.status.success(), "{arguments:?}: {output:?}");
    assert_eq!(stdout(&output), "", "{arguments:?}");
    let errors = stderr(&output);
    assert!(errors.contains(message), "{arguments:?}: {errors}");
}

#[test]
fn on_prints_the_probe_then_power_on() {
    let power = dump(
        "E7 OpenLcd",
        "E7 CB 55 AC 38 01 00 00 00 00 00 00 00 00 00 00",
    );

    assert_prints(&["on"], &(probe() + &power));
}

#[test]
fn off_prints_the_probe_then_power_off() {
    let power = dump(
        "E7 OpenLcd",
        "E7 CB 55 AC 38 02 00 00 00 00 00 00 00 00 00 00",
    );

    assert_prints(&["off"], &(probe() + &power));
}

#[test]
fn mode_prints_the_wire_byte_of_each_mode() {
    let cases = [
        ("0", "01"),
        ("1", "02"),
        ("2", "03"),
        ("3", "04"),
        ("4", "05"),
        ("5", "06"),
        ("6", "07"),
        // The GCC quirk: not 08.
        ("7", "0A"),
        // Number 8 is a label: not 09.
        ("8", "0B"),
    ];
    for (mode, wire) in cases {
        let first_row = format!("E5 CB 55 AC 38 {wire} 00 00 00 00 00 00 00 00 00 00");
        let frame = dump("E5 SetMode", &first_row);

        assert_prints(&["mode", mode], &(probe() + &frame));
    }
}

#[test]
fn mode_refuses_anything_above_eight_before_printing() {
    assert_refused(&["mode", "9"], "display mode must be 0 to 8, not 9");
    assert_refused(&["mode", "255"], "display mode must be 0 to 8, not 255");
    assert_refused(&["mode", "256"], "invalid value");
    assert_refused(&["mode", "gif"], "invalid value");
    assert_refused(&["mode"], "required");
}

#[test]
fn save_says_it_writes_nvram_then_prints_the_frame() {
    let save = dump("AA Save", "AA CB 55 AC 38 00 00 00 00 00 00 00 00 00 00 00");

    assert_prints(&["save"], &(probe() + WOULD_SAVE + &save));
}

#[test]
fn save_help_says_it_writes_nvram() {
    for flag in ["-h", "--help"] {
        let output = dry_run(&["save", flag]);

        assert!(output.status.success(), "{output:?}");
        let help = stdout(&output);
        assert!(help.contains("writes to the panel's NVRAM"), "{help}");
    }
}

#[test]
fn carousel_prints_interval_then_mode_plus_one_in_play_order() {
    let frame = dump(
        "F3 SetLoop",
        "F3 CB 55 AC 38 05 01 02 05 00 00 00 00 00 00 00",
    );

    assert_prints(
        &["carousel", "0,1,4", "--interval", "5"],
        &(probe() + &frame),
    );
}

#[test]
fn carousel_sends_the_interval_unchanged() {
    let frame = dump(
        "F3 SetLoop",
        "F3 CB 55 AC 38 C8 07 01 00 00 00 00 00 00 00 00",
    );

    assert_prints(
        &["carousel", "6,0", "--interval", "200"],
        &(probe() + &frame),
    );
}

#[test]
fn carousel_interval_defaults_to_five() {
    let frame = dump(
        "F3 SetLoop",
        "F3 CB 55 AC 38 05 03 00 00 00 00 00 00 00 00 00",
    );

    assert_prints(&["carousel", "2"], &(probe() + &frame));
}

#[test]
fn mode_help_offers_0_to_8() {
    let output = dry_run(&["mode", "--help"]);

    assert!(output.status.success(), "{output:?}");
    let help = stdout(&output);
    assert!(help.contains("<0-8>"), "{help}");
    assert!(help.contains("8 preset rotation"), "{help}");
}

#[test]
fn carousel_help_calls_it_an_interval_with_a_default_of_five() {
    let output = dry_run(&["carousel", "--help"]);

    assert!(output.status.success(), "{output:?}");
    let help = stdout(&output);
    assert!(help.contains("Interval, sent as it is"), "{help}");
    assert!(help.contains("[default: 5]"), "{help}");
}

#[test]
fn carousel_refuses_a_bad_list_before_printing() {
    assert_refused(&["carousel", "0,7"], "carousel modes must be 0 to 6, not 7");
    assert_refused(&["carousel", "7"], "carousel modes must be 0 to 6, not 7");
    assert_refused(&["carousel", "1,8"], "carousel modes must be 0 to 6, not 8");
    assert_refused(&["carousel", "8"], "carousel modes must be 0 to 6, not 8");
    assert_refused(&["carousel", "1,9"], "carousel modes must be 0 to 6, not 9");
    assert_refused(&["carousel"], "required");
    assert_refused(&["carousel", ""], "invalid value");
    assert_refused(&["carousel", "0,,1"], "invalid value");
    assert_refused(&["carousel", "a"], "invalid value");
    assert_refused(&["carousel", "0", "--interval", "256"], "invalid value");
}

#[test]
fn status_previews_all_eight_queries_in_order_and_says_there_is_no_status() {
    let expected = [
        probe(),
        dump(
            "D6 GetFWVersion",
            "D6 CB 55 AC 38 00 00 00 00 00 00 00 00 00 00 00",
        ),
        dump(
            "DE GetMode",
            "DE CB 55 AC 38 00 00 00 00 00 00 00 00 00 00 00",
        ),
        dump(
            "DF GetDisplay",
            "DF CB 55 AC 38 00 00 00 00 00 00 00 00 00 00 00",
        ),
        dump(
            "F4 GetLoop",
            "F4 CB 55 AC 38 01 00 00 00 00 00 00 00 00 00 00",
        ),
        dump(
            "F4 GetLoop",
            "F4 CB 55 AC 38 02 00 00 00 00 00 00 00 00 00 00",
        ),
        dump(
            "F4 GetLoop",
            "F4 CB 55 AC 38 03 00 00 00 00 00 00 00 00 00 00",
        ),
        dump(
            "F4 GetLoop",
            "F4 CB 55 AC 38 04 00 00 00 00 00 00 00 00 00 00",
        ),
        dump(
            "F4 GetLoop",
            "F4 CB 55 AC 38 05 00 00 00 00 00 00 00 00 00 00",
        ),
        NO_STATUS.to_owned(),
    ];

    assert_prints(&["status"], &expected.concat());
}

// None of these may look for hardware, and the output must not depend on it.
#[test]
fn every_command_ignores_the_bus_in_a_dry_run() {
    let commands: [&[&str]; 6] = [
        &["on"],
        &["off"],
        &["mode", "3"],
        &["save"],
        &["carousel", "0"],
        &["status"],
    ];
    for command in commands {
        let output = dry_run(&[command, &["--bus", "99"]].concat());

        assert!(output.status.success(), "{command:?}: {output:?}");
        assert_eq!(stdout(&output), stdout(&dry_run(command)), "{command:?}");
    }
}
