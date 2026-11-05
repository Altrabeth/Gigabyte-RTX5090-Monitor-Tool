//! [`Panel::recover`] on a fake clock: exact frames and exact delays, in order.
//! Nothing here opens a device. The frames are written out by hand from
//! `PanelRecovery.cs` and `docs/PROTOCOL.md`, not built with the code under test.
//!
//! A full frame is 12 descriptor bytes and 108800 bytes of pixels: 108812 bytes,
//! which is 425 chunks of 256 and 12 left over, so 426 chunks, or `0x01AA`.

use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;

use aorus_lcd::bus::lock::BusLock;
use aorus_lcd::bus::transport::testing::{i2c_error, Mock};
use aorus_lcd::bus::transport::Retrying;
use aorus_lcd::panel::{
    NoSleep, Panel, RecordingSleep, RecoveryEvent, RecoveryOptions, RecoveryOverlay, RecoveryStep,
    Sleep, UploadProgress,
};
use aorus_lcd::protocol::Mode;
use aorus_lcd::session::{connect_with, Environment, Options, Session};
use aorus_lcd::{Error, Result};
use tempfile::TempDir;

const FRAME_LEN: usize = 256;
const CHUNKS: usize = 426;
const DESCRIPTOR: [u8; 12] = [
    0x01, 0x00, 0x0B, 0xA9, 0x01, 0x00, 0x40, 0x01, 0xAA, 0x00, 0x01, 0x00,
];

const POWER_SETTLE: Duration = Duration::from_millis(1000);
const UPLOAD_SETTLE: Duration = Duration::from_millis(500);
const MODE_SETTLE: Duration = Duration::from_millis(300);

/// `opcode`, the magic, `tail`, and zeros up to a full frame.
fn command(opcode: u8, tail: &[u8]) -> Vec<u8> {
    let mut frame = vec![opcode, 0xCB, 0x55, 0xAC, 0x38];
    frame.extend_from_slice(tail);
    frame.resize(FRAME_LEN, 0);
    frame
}

fn power_off() -> Vec<u8> {
    command(0xE7, &[2])
}

fn power_on() -> Vec<u8> {
    command(0xE7, &[1])
}

fn set_mode(wire_byte: u8) -> Vec<u8> {
    command(0xE5, &[wire_byte])
}

fn save() -> Vec<u8> {
    command(0xAA, &[])
}

/// `E1` with GPU temperature in `[5]`, TGP in `[12]`, and interval 3.
fn overlay_on() -> Vec<u8> {
    command(0xE1, &[1, 0, 0, 0, 0, 0, 0, 1, 3])
}

/// `E1` with every widget off and interval 0.
fn overlay_off() -> Vec<u8> {
    command(0xE1, &[0; 9])
}

/// The upload of `pixel`, which repeats to fill the frame, up to and including
/// the `SetMode` of [`Panel::upload`].
fn upload_of(pixel: [u8; 2]) -> Vec<Vec<u8>> {
    let mut payload = DESCRIPTOR.to_vec();
    payload.extend(pixel.repeat(54_400));

    let mut frames = vec![
        command(0xF2, &[1]),
        // Static framebuffer, flag 1, 426 chunks, no frames, no delay, mode 2.
        command(
            0xF1,
            &[0x01, 0x30, 0x00, 0x00, 1, 0, 0, 0x01, 0xAA, 0, 0, 0, 2, 0],
        ),
    ];
    for chunk in payload.chunks(FRAME_LEN) {
        let mut frame = chunk.to_vec();
        frame.resize(FRAME_LEN, 0);
        frames.push(frame);
    }
    frames.push(command(0xF2, &[2]));
    frames.push(set_mode(4));
    frames
}

fn black_upload() -> Vec<Vec<u8>> {
    upload_of([0, 0])
}

/// The firmware's pacing: `F2`, `F1`, each chunk, `F2` END.
fn upload_delays() -> Vec<Duration> {
    let mut delays = vec![Duration::from_millis(500), Duration::from_millis(1000)];
    delays.extend([Duration::from_millis(10)].repeat(CHUNKS + 1));
    delays
}

/// What a recovery did.
struct Run {
    writes: Vec<Vec<u8>>,
    delays: Vec<Duration>,
    events: Vec<RecoveryEvent>,
    result: Result<()>,
}

fn run_on(mock: &Mock, options: &RecoveryOptions) -> Run {
    let mut clock = RecordingSleep::default();
    let mut events = Vec::new();
    let result = Panel::new(Retrying::with_sleep(mock.clone(), NoSleep)).recover(
        options,
        &mut clock,
        |event| events.push(event),
    );
    Run {
        writes: mock.writes(),
        delays: clock.delays,
        events,
        result,
    }
}

fn run(options: &RecoveryOptions) -> Run {
    let run = run_on(&Mock::new(), options);
    run.result.as_ref().unwrap();
    run
}

/// The steps reported, without the upload's progress.
fn steps(run: &Run) -> Vec<RecoveryStep> {
    run.events
        .iter()
        .filter_map(|event| match event {
            RecoveryEvent::Step(step) => Some(*step),
            RecoveryEvent::Upload(_) => None,
        })
        .collect()
}

/// No flags: everything up to `SetMode`, and only that.
fn default_writes() -> Vec<Vec<u8>> {
    let mut writes = vec![power_off(), power_on()];
    writes.extend(black_upload());
    writes.push(set_mode(1));
    writes
}

fn default_delays() -> Vec<Duration> {
    let mut delays = vec![POWER_SETTLE, POWER_SETTLE];
    delays.extend(upload_delays());
    delays.extend([UPLOAD_SETTLE, MODE_SETTLE]);
    delays
}

#[test]
fn the_default_run_is_exactly_the_button_sequence() {
    let run = run(&RecoveryOptions::default());

    assert_eq!(run.writes, default_writes());
    assert_eq!(run.delays, default_delays());
}

#[test]
fn the_default_run_sends_neither_e1_nor_aa() {
    let run = run(&RecoveryOptions::default());

    // Chunks are pixels, so only frames with the magic are commands.
    let opcodes: Vec<u8> = run
        .writes
        .iter()
        .filter(|frame| frame[1..5] == [0xCB, 0x55, 0xAC, 0x38])
        .map(|frame| frame[0])
        .collect();
    assert_eq!(opcodes, [0xE7, 0xE7, 0xF2, 0xF1, 0xF2, 0xE5, 0xE5]);
}

#[test]
fn a_black_frame_is_all_zeros_after_the_descriptor() {
    let run = run(&RecoveryOptions::default());

    let chunks = &run.writes[4..4 + CHUNKS];
    assert_eq!(chunks[0][..12], DESCRIPTOR);
    assert!(chunks[0][12..].iter().all(|&byte| byte == 0));
    assert!(chunks[1..].iter().flatten().all(|&byte| byte == 0));
    assert_eq!(run.writes[4 + CHUNKS][0], 0xF2);
}

#[test]
fn the_fill_colour_reaches_the_chunks_as_little_endian_565() {
    let run = run(&RecoveryOptions {
        fill: Some([0xFF, 0, 0]),
        ..RecoveryOptions::default()
    });

    let chunks = &run.writes[4..4 + CHUNKS];
    assert_eq!(chunks[0][..14], [&DESCRIPTOR[..], &[0x00, 0xF8]].concat());
    assert_eq!(chunks[1], [0x00, 0xF8].repeat(128));
    // The last chunk holds the 12 bytes left over, then padding.
    let last = &chunks[CHUNKS - 1];
    assert_eq!(last[..12], [0x00, 0xF8].repeat(6));
    assert!(last[12..].iter().all(|&byte| byte == 0));
}

#[test]
fn no_powercycle_drops_the_e7s_and_their_delays() {
    let run = run(&RecoveryOptions {
        power_cycle: false,
        ..RecoveryOptions::default()
    });

    assert_eq!(run.writes, default_writes()[2..]);
    assert_eq!(run.delays, default_delays()[2..]);
}

#[test]
fn no_upload_drops_the_upload_and_its_delays() {
    let run = run(&RecoveryOptions {
        fill: None,
        ..RecoveryOptions::default()
    });

    assert_eq!(run.writes, [power_off(), power_on(), set_mode(1)]);
    assert_eq!(run.delays, [POWER_SETTLE, POWER_SETTLE, MODE_SETTLE]);
}

#[test]
fn neither_step_leaves_only_the_mode() {
    let run = run(&RecoveryOptions {
        power_cycle: false,
        fill: None,
        ..RecoveryOptions::default()
    });

    assert_eq!(run.writes, [set_mode(1)]);
    assert_eq!(run.delays, [MODE_SETTLE]);
}

#[test]
fn overlay_sends_e1_after_the_mode_with_temp_tgp_and_interval_three() {
    let run = run(&RecoveryOptions {
        overlay: RecoveryOverlay::Enable,
        ..RecoveryOptions::default()
    });

    let mut expected = default_writes();
    expected.push(overlay_on());
    assert_eq!(run.writes, expected);
    assert_eq!(run.delays, default_delays());
}

#[test]
fn clear_overlay_sends_e1_with_nothing_and_interval_zero() {
    let run = run(&RecoveryOptions {
        overlay: RecoveryOverlay::Clear,
        ..RecoveryOptions::default()
    });

    let mut expected = default_writes();
    expected.push(overlay_off());
    assert_eq!(run.writes, expected);
    assert_eq!(run.delays, default_delays());
}

#[test]
fn save_sends_aa_last() {
    let run = run(&RecoveryOptions {
        save: true,
        ..RecoveryOptions::default()
    });

    let mut expected = default_writes();
    expected.push(save());
    assert_eq!(run.writes, expected);
    assert_eq!(run.delays, default_delays());
}

#[test]
fn overlay_comes_before_save() {
    let run = run(&RecoveryOptions {
        overlay: RecoveryOverlay::Enable,
        save: true,
        ..RecoveryOptions::default()
    });

    assert_eq!(run.writes[run.writes.len() - 2..], [overlay_on(), save()]);
}

#[test]
fn mode_three_is_nudged_through_mode_six_after_the_upload() {
    let run = run(&RecoveryOptions {
        target: Mode::Image,
        ..RecoveryOptions::default()
    });

    let mut writes = vec![power_off(), power_on()];
    writes.extend(black_upload());
    writes.extend([set_mode(7), set_mode(4)]);
    assert_eq!(run.writes, writes);

    let mut delays = vec![POWER_SETTLE, POWER_SETTLE];
    delays.extend(upload_delays());
    delays.extend([UPLOAD_SETTLE, MODE_SETTLE, MODE_SETTLE]);
    assert_eq!(run.delays, delays);
}

#[test]
fn mode_three_is_nudged_without_an_upload_too() {
    let run = run(&RecoveryOptions {
        power_cycle: false,
        fill: None,
        target: Mode::Image,
        ..RecoveryOptions::default()
    });

    assert_eq!(run.writes, [set_mode(7), set_mode(4)]);
    assert_eq!(run.delays, [MODE_SETTLE, MODE_SETTLE]);
}

#[test]
fn no_other_mode_is_nudged_and_each_goes_out_on_its_wire_byte() {
    let targets = [
        (0, 0x01),
        (1, 0x02),
        (2, 0x03),
        (4, 0x05),
        (5, 0x06),
        (6, 0x07),
        (7, 0x0A),
        (8, 0x0B),
    ];
    for (number, wire_byte) in targets {
        let run = run(&RecoveryOptions {
            power_cycle: false,
            fill: None,
            target: Mode::try_from(number).unwrap(),
            ..RecoveryOptions::default()
        });

        assert_eq!(run.writes, [set_mode(wire_byte)], "mode {number}");
        assert_eq!(run.delays, [MODE_SETTLE], "mode {number}");
    }
}

#[test]
fn each_step_is_reported_before_its_frames_go_out() {
    let mock = Mock::new();
    let probe = mock.clone();
    let mut seen = Vec::new();

    Panel::new(Retrying::with_sleep(mock, NoSleep))
        .recover(
            &RecoveryOptions {
                overlay: RecoveryOverlay::Clear,
                save: true,
                ..RecoveryOptions::default()
            },
            &mut NoSleep,
            |event| {
                if let RecoveryEvent::Step(step) = event {
                    seen.push((step, probe.writes().len()));
                }
            },
        )
        .unwrap();

    let black = [0, 0, 0];
    assert_eq!(
        seen,
        [
            (RecoveryStep::PowerCycle, 0),
            (RecoveryStep::Upload(black), 2),
            (RecoveryStep::SetMode(Mode::Faith1), CHUNKS + 6),
            (RecoveryStep::ClearOverlay, CHUNKS + 7),
            (RecoveryStep::Save, CHUNKS + 8),
        ]
    );
}

#[test]
fn the_overlay_step_is_only_reported_when_asked_for() {
    let quiet = run(&RecoveryOptions::default());
    assert_eq!(
        steps(&quiet),
        [
            RecoveryStep::PowerCycle,
            RecoveryStep::Upload([0, 0, 0]),
            RecoveryStep::SetMode(Mode::Faith1),
        ]
    );

    let overlay = run(&RecoveryOptions {
        overlay: RecoveryOverlay::Enable,
        ..RecoveryOptions::default()
    });
    assert_eq!(steps(&overlay)[3..], [RecoveryStep::EnableOverlay]);
}

#[test]
fn upload_progress_counts_every_frame_of_the_upload() {
    let run = run(&RecoveryOptions::default());

    let progress: Vec<UploadProgress> = run
        .events
        .iter()
        .filter_map(|event| match event {
            RecoveryEvent::Upload(progress) => Some(*progress),
            RecoveryEvent::Step(_) => None,
        })
        .collect();
    let total = CHUNKS + 3;
    assert_eq!(progress.len(), total);
    assert_eq!(progress[total - 1], UploadProgress { sent: total, total });
}

#[test]
fn a_rejected_transfer_is_retried_on_the_retry_clock_and_not_the_sequences() {
    let mock = Mock::new();
    mock.fail_write(1, i2c_error());

    let run = run_on(&mock, &RecoveryOptions::default());

    run.result.unwrap();
    assert_eq!(run.writes, default_writes());
    assert_eq!(run.delays, default_delays());
}

#[test]
fn aa_is_not_retried_because_a_repeat_is_a_second_nvram_write() {
    let mock = Mock::new();
    // Attempt 0 is the `SetMode`.
    mock.fail_write(1, i2c_error());

    let run = run_on(
        &mock,
        &RecoveryOptions {
            power_cycle: false,
            fill: None,
            save: true,
            ..RecoveryOptions::default()
        },
    );

    assert!(matches!(run.result, Err(Error::I2c { .. })));
    assert_eq!(mock.write_attempts(), 2);
    assert_eq!(run.writes, [set_mode(1)]);
}

#[test]
fn a_failure_ends_the_recovery_with_nothing_further_written() {
    let mock = Mock::new();
    // A short read stands for any error that a retry does not touch.
    mock.fail_write(
        1,
        Error::ShortRead {
            expected: 1,
            actual: 0,
        },
    );

    let run = run_on(&mock, &RecoveryOptions::default());

    assert!(matches!(run.result, Err(Error::ShortRead { .. })));
    assert_eq!(run.writes, [power_off()]);
    assert_eq!(run.delays, [POWER_SETTLE]);
}

#[test]
fn a_lost_chunk_stops_before_the_settle_and_the_mode() {
    let mock = Mock::new();
    // After both `E7`, `F2` and `F1`, this is the third chunk.
    mock.fail_write(
        6,
        Error::ShortRead {
            expected: 1,
            actual: 0,
        },
    );

    let run = run_on(&mock, &RecoveryOptions::default());

    assert!(
        matches!(run.result, Err(Error::Upload { frame: 4, .. })),
        "{:?}",
        run.result
    );
    assert_eq!(run.writes.len(), 6);
    assert_eq!(run.delays.len(), 6);
}

/// Answers every pause by checking that nobody else could take the bus lock.
struct LockProbe {
    path: PathBuf,
    pauses: usize,
}

impl Sleep for LockProbe {
    fn sleep(&mut self, _duration: Duration) {
        self.pauses += 1;
        assert!(
            matches!(
                BusLock::try_acquire(&self.path),
                Err(Error::LockBusy { .. })
            ),
            "the bus lock was free at pause {}",
            self.pauses
        );
    }
}

fn fake_sysfs(root: &Path) -> PathBuf {
    let sysfs = root.join("sysfs");
    let adapter = sysfs.join("i2c-7");
    fs::create_dir_all(&adapter).unwrap();
    fs::write(adapter.join("name"), "NVIDIA i2c adapter 1 at 1:00.0\n").unwrap();
    sysfs
}

#[test]
fn the_bus_lock_stays_held_through_every_pause_of_a_recovery() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("test.lock");
    let mock = Mock::new();
    mock.script_read([0xFF; 8]);
    let device = mock.clone();
    let environment = Environment {
        sysfs_root: fake_sysfs(dir.path()),
        lock_path: path.clone(),
        open: move |_| Ok(device.clone()),
        sleep: RecordingSleep::default(),
        dry_run_output: Vec::new(),
    };
    let mut session: Session<Mock, Vec<u8>> =
        connect_with(&Options::default(), environment).unwrap();

    let mut clock = LockProbe {
        path: path.clone(),
        pauses: 0,
    };
    Panel::new(Retrying::with_sleep(&mut session, NoSleep))
        .recover(&RecoveryOptions::default(), &mut clock, |_| {})
        .unwrap();

    assert_eq!(clock.pauses, default_delays().len());
    drop(session);
    assert!(BusLock::try_acquire(&path).is_ok());
}
