//! `recover` through the real binary, in `--dry-run` only.
//!
//! As in `cli_dry_run.rs`, the child gets a lock path that cannot exist, so a
//! broken `--dry-run` stops at the lock and does not reach the bus. The one
//! helper that starts the child always passes `--dry-run`. The exact frames and
//! delays are in `recover.rs`; this covers what only the binary can show.

use std::process::{Command, Output};
use std::time::{Duration, Instant};

use aorus_lcd::bus::lock::LOCK_PATH_ENV;

/// Comfortably less than the 2 s of a power cycle alone, on top of the 6 s of a
/// paced upload.
const NO_REAL_WAITING: Duration = Duration::from_millis(1500);

fn dry_run(arguments: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_aorus-lcd"))
        .args(["--dry-run", "recover"])
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

fn dump_of(arguments: &[&str]) -> String {
    let output = dry_run(arguments);
    assert!(output.status.success(), "{arguments:?}: {output:?}");
    stdout(&output)
}

/// Is this line the start of a frame in the dump?
fn opcode(line: &str) -> Option<&str> {
    if line.starts_with("payload chunk") {
        return Some("chunk");
    }
    let (start, rest) = line.split_once(' ')?;
    let is_hex = start.len() == 2 && start.bytes().all(|byte| byte.is_ascii_hexdigit());
    (is_hex && rest.ends_with("(256 bytes)")).then_some(start)
}

/// The frames of a dump, with the chunks counted instead of listed.
fn sequence(dump: &str) -> Vec<String> {
    let mut frames: Vec<(&str, usize)> = Vec::new();
    for frame in dump.lines().filter_map(opcode) {
        match frames.last_mut() {
            Some((last, count)) if *last == "chunk" && frame == "chunk" => *count += 1,
            _ => frames.push((frame, 1)),
        }
    }
    frames
        .into_iter()
        .map(|(frame, count)| match frame {
            "chunk" => format!("{count} chunks"),
            opcode => opcode.to_string(),
        })
        .collect()
}

/// The line number of the first line that starts with `start`.
fn line_of(dump: &str, start: &str) -> usize {
    dump.lines()
        .position(|line| line.starts_with(start))
        .unwrap_or_else(|| panic!("no {start} in {dump}"))
}

#[test]
fn the_default_dump_is_the_whole_sequence_without_e1_or_aa() {
    let dump = dump_of(&[]);

    assert_eq!(
        sequence(&dump),
        ["EB", "E7", "E7", "F2", "F1", "426 chunks", "F2", "E5", "E5"]
    );
}

#[test]
fn the_first_chunk_of_the_default_frame_is_the_descriptor_on_black() {
    let dump = dump_of(&[]);

    let chunks: Vec<&str> = dump
        .lines()
        .filter(|line| line.starts_with("payload chunk"))
        .collect();
    assert!(chunks[0].ends_with(": 01 00 0B A9 01 00 40 01"), "{dump}");
    assert!(chunks[425].ends_with(": 00 00 00 00 00 00 00 00"), "{dump}");
}

#[test]
fn each_flag_changes_the_dump() {
    let cases: [(&[&str], &[&str]); 7] = [
        (
            &["--no-powercycle"],
            &["EB", "F2", "F1", "426 chunks", "F2", "E5", "E5"],
        ),
        (&["--no-upload"], &["EB", "E7", "E7", "E5"]),
        (
            &["--no-powercycle", "--no-upload", "--mode", "3"],
            &["EB", "E5", "E5"],
        ),
        (
            &["--no-powercycle", "--no-upload", "--mode", "8"],
            &["EB", "E5"],
        ),
        (
            &["--no-upload", "--overlay"],
            &["EB", "E7", "E7", "E5", "E1"],
        ),
        (
            &["--no-upload", "--clear-overlay"],
            &["EB", "E7", "E7", "E5", "E1"],
        ),
        (&["--no-upload", "--save"], &["EB", "E7", "E7", "E5", "AA"]),
    ];

    for (arguments, expected) in cases {
        assert_eq!(sequence(&dump_of(arguments)), expected, "{arguments:?}");
    }
}

#[test]
fn the_colour_reaches_the_chunks() {
    let dump = dump_of(&["--color", "#ff0000"]);

    assert!(dump.contains("#FF0000"), "{dump}");
    let second_chunk = dump
        .lines()
        .filter(|line| line.starts_with("payload chunk"))
        .nth(1)
        .unwrap();
    assert!(
        second_chunk.ends_with(": 00 F8 00 F8 00 F8 00 F8"),
        "{dump}"
    );
}

#[test]
fn each_step_prints_before_its_frames() {
    let dump = dump_of(&["--clear-overlay", "--save"]);

    // The first `E5` belongs to the upload.
    let lines: Vec<&str> = dump.lines().collect();
    let last_e5 = lines
        .iter()
        .rposition(|line| line.starts_with("E5 "))
        .unwrap();

    let order = [
        line_of(&dump, "Power-cycling"),
        line_of(&dump, "E7 "),
        line_of(&dump, "Uploading a fresh frame (#000000)"),
        line_of(&dump, "F2 "),
        line_of(&dump, "Switching to mode 0"),
        last_e5,
        line_of(&dump, "Clearing the sensor overlay"),
        line_of(&dump, "E1 "),
        line_of(&dump, "dry-run: a real run writes"),
        line_of(&dump, "AA "),
    ];
    assert!(order.is_sorted(), "{order:?}\n{dump}");
}

#[test]
fn the_output_ends_by_saying_when_the_daemon_needs_restarting() {
    let dump = dump_of(&["--no-upload"]);

    let last = dump.lines().last().unwrap();
    assert!(last.starts_with("If you stopped the daemon"), "{dump}");
    assert!(last.contains("systemctl start aorus-lcd"), "{dump}");
}

#[test]
fn a_dry_run_does_not_really_wait() {
    let start = Instant::now();

    dump_of(&["--mode", "3"]);

    assert!(start.elapsed() < NO_REAL_WAITING, "{:?}", start.elapsed());
}

#[test]
fn bad_values_are_refused_before_anything_is_printed() {
    for arguments in [
        &["--color", "red"][..],
        &["--no-upload", "--color", "red"],
        &["--mode", "9"],
    ] {
        let output = dry_run(arguments);

        assert!(!output.status.success(), "{arguments:?}");
        assert_eq!(stdout(&output), "", "{arguments:?}");
        assert!(stderr(&output).contains("error: "), "{arguments:?}");
    }
}

#[test]
fn overlay_and_clear_overlay_cannot_be_combined() {
    let output = dry_run(&["--overlay", "--clear-overlay"]);

    assert!(!output.status.success());
    assert_eq!(stdout(&output), "");
    assert!(
        stderr(&output).contains("cannot be used with"),
        "{output:?}"
    );
}
