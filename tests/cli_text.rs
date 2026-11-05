//! `text` through the real binary, in `--dry-run` only.
//!
//! As in `cli_dry_run.rs`, the child gets a lock path that cannot exist, so a
//! broken `--dry-run` stops at the lock and does not reach the bus. The one
//! helper that starts the child always passes `--dry-run`.
//!
//! The pixels themselves are tested in `src/text.rs`. Only the frame sequence
//! has to match the reference, because glyphs cannot match Pillow's.

use std::process::{Command, Output};

use aorus_lcd::bus::lock::LOCK_PATH_ENV;

const NOTICE: &str = "dry-run: a real run writes the panel configuration to NVRAM";
/// The first row of `AA`, which has no parameters.
const AA_ROW: &str = "AA CB 55 AC 38 00 00 00 00 00 00 00 00 00 00 00";

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

/// Runs a command that has to work, and returns its dump.
fn dump_of(arguments: &[&str]) -> String {
    let output = dry_run(arguments);
    assert!(output.status.success(), "{arguments:?}: {output:?}");
    stdout(&output)
}

/// The first line of every frame, without the first eight pixels of a chunk.
fn frame_sequence(dump: &str) -> Vec<&str> {
    dump.lines()
        .filter(|line| !line.starts_with(' ') && !line.starts_with("dry-run:"))
        .map(|line| line.split_once(':').map_or(line, |(start, _)| start))
        .collect()
}

/// The hex of the first `count` rows after the first line that starts with
/// `header`.
fn rows_after<'a>(dump: &'a str, header: &str, count: usize) -> Vec<&'a str> {
    dump.lines()
        .skip_while(|line| !line.starts_with(header))
        .skip(1)
        .take(count)
        .map(|line| line.trim_start().split_once("  ").unwrap().1)
        .collect()
}

/// Refused with nothing on stdout, so before the probe and any bus access.
fn assert_refused(arguments: &[&str], message: &str) {
    let output = dry_run(arguments);

    assert!(!output.status.success(), "{arguments:?}: {output:?}");
    assert_eq!(stdout(&output), "", "{arguments:?}");
    let errors = String::from_utf8(output.stderr).unwrap();
    assert!(errors.contains(message), "{arguments:?}: {errors}");
}

#[test]
fn text_is_the_probe_then_the_upload_then_set_mode_then_aa() {
    let dump = dump_of(&["text", "hello"]);

    // 12 + 108800 = 108812 bytes, so 425 full chunks and one more. With both
    // markers, the header, `E5`, `AA` and the probe, that is 432.
    let frames = frame_sequence(&dump);
    assert_eq!(frames.len(), 432, "{frames:?}");
    assert_eq!(
        frames[..3],
        [
            "EB GetImageTpl (256 bytes)",
            "F2 UploadMarker (256 bytes)",
            "F1 UploadHeader (256 bytes)",
        ]
    );
    assert!(
        frames[3..429]
            .iter()
            .all(|frame| *frame == "payload chunk (256 bytes)"),
        "{frames:?}"
    );
    assert_eq!(
        frames[429..],
        [
            "F2 UploadMarker (256 bytes)",
            "E5 SetMode (256 bytes)",
            "AA Save (256 bytes)",
        ]
    );
}

#[test]
fn no_effect_leaves_aa_out_and_changes_nothing_else() {
    let plain = dump_of(&["text", "hello", "--no-effect"]);
    let with_effect = dump_of(&["text", "hello"]);

    assert!(!plain.contains("AA Save"), "{plain}");
    assert!(!plain.contains("NVRAM"), "{plain}");
    let frames = frame_sequence(&plain);
    assert_eq!(frames.len(), 431);
    assert_eq!(frames[430], "E5 SetMode (256 bytes)");
    assert!(with_effect.starts_with(&plain));
}

#[test]
fn the_notice_comes_right_before_aa() {
    let dump = dump_of(&["text", "hello"]);

    assert!(dump.contains(&format!("{NOTICE}\nAA Save")), "{dump}");
    assert_eq!(rows_after(&dump, "AA ", 1), [AA_ROW]);
    assert_eq!(dump.matches("AA Save").count(), 1);
}

#[test]
fn the_header_is_the_text_framebuffer_and_set_mode_is_mode_4() {
    let dump = dump_of(&["text", "hello"]);

    assert_eq!(
        rows_after(&dump, "F1 ", 2),
        [
            // Framebuffer 0x01320000, flag 1, 426 chunks (0x1AA), no frames.
            "F1 CB 55 AC 38 01 32 00 00 01 00 00 01 AA 00 00",
            // No delay, then transfer mode 2 for at least 20480 bytes.
            "00 02 00 00 00 00 00 00 00 00 00 00 00 00 00 00",
        ]
    );
    assert_eq!(
        // Byte 5 is the mode plus one, so 5 for mode 4.
        rows_after(&dump, "E5 ", 1),
        ["E5 CB 55 AC 38 05 00 00 00 00 00 00 00 00 00 00"]
    );
}

#[test]
fn the_payload_starts_with_the_descriptor_on_the_background() {
    let dump = dump_of(&["-v", "text", "hello", "--bg", "#ff0000"]);

    assert_eq!(
        rows_after(&dump, "payload chunk", 1),
        ["01 00 0B A9 01 00 40 01 AA 00 01 00 00 F8 00 F8"]
    );
}

#[test]
fn a_literal_backslash_n_and_a_line_break_are_the_same() {
    assert_eq!(
        dump_of(&["-v", "text", r"one\ntwo", "--no-effect"]),
        dump_of(&["-v", "text", "one\ntwo", "--no-effect"])
    );
}

#[test]
fn empty_text_is_refused_before_the_probe() {
    assert_refused(&["text", ""], "empty");
    assert_refused(&["text", "  \n "], "empty");
}

#[test]
fn a_size_of_zero_is_refused_before_the_probe() {
    assert_refused(&["text", "hello", "--size", "0"], "size");
}

#[test]
fn a_bad_colour_is_refused_before_the_probe() {
    assert_refused(&["text", "hello", "--color", "12345"], "six hex digits");
    assert_refused(&["text", "hello", "--bg", "12345g"], "six hex digits");
}

#[test]
fn text_that_does_not_fit_is_refused_before_the_probe() {
    assert_refused(
        &["text", "hello", "--size", "400"],
        "at size 400, and the panel has 320",
    );
}
