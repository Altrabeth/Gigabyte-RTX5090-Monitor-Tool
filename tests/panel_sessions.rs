//! [`Panel`] over every link it has to run on, through the public API. Nothing
//! here opens a device: the gate only ever receives a [`Mock`], and the sysfs
//! tree and lock file are temporary.

use std::cell::Cell;
use std::fs;
use std::io::Write;
use std::path::PathBuf;
use std::rc::Rc;
use std::time::Duration;

use aorus_lcd::bus::lock::BusLock;
use aorus_lcd::bus::transport::testing::{i2c_error, Mock};
use aorus_lcd::bus::transport::{DryRun, Retrying, Transport};
use aorus_lcd::panel::{NoSleep, Panel, RecordingSleep, Sleep, UploadOptions};
use aorus_lcd::protocol::{
    build_upload, carousel_frame, display_frame, firmware_query_frame, loop_query_frame,
    mode_frame, power_frame, probe_frame, sensor_frame, Elements, FeedFrameLen, Framebuffer, Mode,
    SensorSample, UploadParams, LOOP_BANK_FIRST, PROBE_READ_LEN,
};
use aorus_lcd::session::{connect_with, Environment, Options, Session, Unlocked, Verified};
use aorus_lcd::{Error, Result};
use tempfile::TempDir;

const SAMPLE: SensorSample = SensorSample {
    gpu_temp_c: 65,
    gpu_clock_mhz: 2100,
    gpu_usage_percent: 97,
    fan_speed: 1500,
    ram_clock_mhz: 14000,
    ram_usage_percent: 42,
    fps: 0,
    tgp_watts: 450,
};

/// Two chunks.
const PAYLOAD: [u8; 300] = [0x44; 300];

/// The frames of an upload of [`PAYLOAD`], without the `SetMode`.
fn upload_writes(params: &UploadParams) -> Vec<Vec<u8>> {
    build_upload(&PAYLOAD, params)
        .unwrap()
        .iter()
        .map(|frame| frame.data.to_vec())
        .collect()
}

/// A temporary sysfs tree and lock path, and a scripted device behind them.
struct Rig {
    dir: TempDir,
    mock: Mock,
}

impl Rig {
    fn new() -> Self {
        let dir = TempDir::new().unwrap();
        let adapter = dir.path().join("sysfs/i2c-7");
        fs::create_dir_all(&adapter).unwrap();
        fs::write(adapter.join("name"), "NVIDIA i2c adapter 1 at 1:00.0\n").unwrap();
        Self {
            dir,
            mock: Mock::new(),
        }
    }

    fn lock_path(&self) -> PathBuf {
        self.dir.path().join("test.lock")
    }

    fn session<W: Write>(&self, dry_run: bool, output: W) -> Session<Mock, W> {
        let mock = self.mock.clone();
        let options = Options {
            dry_run,
            ..Options::default()
        };
        let environment = Environment {
            sysfs_root: self.dir.path().join("sysfs"),
            lock_path: self.lock_path(),
            open: move |_| Ok(mock.clone()),
            sleep: RecordingSleep::default(),
            dry_run_output: output,
        };
        connect_with(&options, environment).unwrap()
    }

    fn live_session(&self) -> Session<Mock, Vec<u8>> {
        self.mock.script_read([0xFF; PROBE_READ_LEN]);
        self.session(false, Vec::new())
    }

    fn verified(&self) -> Verified<Mock> {
        match self.live_session() {
            Session::Live(verified) => verified,
            Session::DryRun(_) => panic!("a live connection was asked for"),
        }
    }

    /// What the device received after the probe.
    fn writes_after_probe(&self) -> Vec<Vec<u8>> {
        let writes = self.mock.writes();
        assert_eq!(writes[0], probe_frame());
        writes[1..].to_vec()
    }

    fn lock_is_free(&self) -> bool {
        BusLock::try_acquire(&self.lock_path()).is_ok()
    }
}

/// The daemon's tick, as `src/session.rs` describes it: sensors were read before
/// this, and an upload holding the bus costs one tick.
fn tick(session: &mut Unlocked<Mock>, len: FeedFrameLen) -> Result<()> {
    match session.lock() {
        Ok(bus) => Panel::new(bus).send_sensors(&SAMPLE, len),
        Err(Error::LockBusy { .. }) => Ok(()),
        Err(error) => Err(error),
    }
}

/// The dump `DryRun` prints for `frames`.
fn dump_of(frames: &[&[u8]]) -> String {
    let mut out = Vec::new();
    let mut dry_run = DryRun::with_output(&mut out, false);
    for frame in frames {
        dry_run.write(frame).unwrap();
    }
    String::from_utf8(out).unwrap()
}

/// Runs `operation` on a dry-run session. Returns its result and the dump, which
/// starts with what `connect` printed.
fn in_dry_run<R>(
    operation: impl FnOnce(&mut Panel<Session<Mock, &mut Vec<u8>>>) -> R,
) -> (R, String) {
    let rig = Rig::new();
    let mut out = Vec::new();
    let result = operation(&mut Panel::new(rig.session(true, &mut out)));
    assert_eq!(rig.mock.write_attempts(), 0, "dry-run reached the device");
    assert_eq!(rig.mock.read_attempts(), 0, "dry-run reached the device");
    (result, String::from_utf8(out).unwrap())
}

#[test]
fn a_panel_runs_over_a_cli_session() {
    let rig = Rig::new();
    let mut panel = Panel::new(rig.live_session());
    let modes = [Mode::Faith1, Mode::Gif];

    panel.on().unwrap();
    panel.set_mode(Mode::Carousel).unwrap();
    panel.set_carousel(5, &modes).unwrap();

    assert_eq!(
        rig.writes_after_probe(),
        [
            power_frame(true).to_vec(),
            mode_frame(Mode::Carousel).to_vec(),
            carousel_frame(5, &modes).unwrap().to_vec(),
        ]
    );
}

#[test]
fn a_panel_over_a_verified_holds_the_lock_for_the_whole_command() {
    let rig = Rig::new();
    let mut panel = Panel::new(rig.verified());

    panel.on().unwrap();
    assert!(!rig.lock_is_free());
    panel.set_dashboard(Elements::ALL, 3).unwrap();
    assert!(!rig.lock_is_free());

    drop(panel);
    assert!(rig.lock_is_free());
    assert_eq!(
        rig.writes_after_probe(),
        [
            power_frame(true).to_vec(),
            display_frame(Elements::ALL, 3).to_vec(),
        ]
    );
}

#[test]
fn a_panel_can_borrow_a_verified_the_caller_keeps() {
    let rig = Rig::new();
    let mut verified = rig.verified();

    Panel::new(&mut verified).off().unwrap();
    Panel::new(&mut verified).on().unwrap();

    assert!(!rig.lock_is_free());
    assert_eq!(
        rig.writes_after_probe(),
        [power_frame(false).to_vec(), power_frame(true).to_vec()]
    );
}

#[test]
fn the_daemon_builds_a_panel_per_tick_and_holds_the_lock_only_for_the_write() {
    let rig = Rig::new();
    let mut session = rig.verified().unlock();

    for _ in 0..3 {
        tick(&mut session, FeedFrameLen::Truncated).unwrap();
        assert!(rig.lock_is_free());
    }

    let frame = sensor_frame(&SAMPLE, FeedFrameLen::Truncated);
    assert_eq!(rig.writes_after_probe(), vec![frame; 3]);
    // The probe's read, and no read-back for any tick.
    assert_eq!(rig.mock.read_attempts(), 1);
}

#[test]
fn a_per_write_panel_holds_the_lock_while_it_lives() {
    let rig = Rig::new();
    let mut session = rig.verified().unlock();

    let panel = Panel::new(session.lock().unwrap());

    assert!(!rig.lock_is_free());
    drop(panel);
    assert!(rig.lock_is_free());
}

#[test]
fn a_tick_during_an_upload_writes_nothing() {
    let rig = Rig::new();
    let mut session = rig.verified().unlock();
    let upload = BusLock::try_acquire(&rig.lock_path()).unwrap();

    tick(&mut session, FeedFrameLen::Full).unwrap();

    assert!(rig.writes_after_probe().is_empty());
    drop(upload);
}

#[test]
fn retrying_comes_from_wrapping_the_link() {
    let rig = Rig::new();
    let link = Retrying::with_sleep(rig.verified(), RecordingSleep::default());
    let mut panel = Panel::new(link);
    rig.mock.script_read_error(i2c_error());
    rig.mock.script_read([0, 0x1A, 0, 0]);

    assert_eq!(panel.firmware().unwrap().to_string(), "1.10");

    // The probe's read, then the failed read and its retry.
    assert_eq!(rig.mock.read_attempts(), 3);
    assert_eq!(rig.writes_after_probe(), [firmware_query_frame().to_vec()]);
}

/// Asserts on every pause that the bus lock is taken, so a passing test shows
/// that it was held across all of them. Clones count together.
#[derive(Clone)]
struct LockHeldCheck {
    path: PathBuf,
    checks: Rc<Cell<usize>>,
}

impl Sleep for LockHeldCheck {
    fn sleep(&mut self, _duration: Duration) {
        let attempt = BusLock::try_acquire(&self.path);
        assert!(
            matches!(attempt, Err(Error::LockBusy { .. })),
            "{attempt:?}"
        );
        self.checks.set(self.checks.get() + 1);
    }
}

#[test]
fn a_verified_holds_the_lock_across_every_pause_of_an_upload() {
    let cases = [
        (UploadParams::still(Framebuffer::Static), 5),
        (UploadParams::still(Framebuffer::Text), 5),
        // The settle comes on top.
        (UploadParams::gif(2, 50), 6),
    ];
    for (params, pauses) in cases {
        let rig = Rig::new();
        let verified = rig.verified();
        let mut clock = LockHeldCheck {
            path: rig.lock_path(),
            checks: Rc::default(),
        };
        let options = UploadOptions::default();

        Panel::new(verified)
            .upload(&PAYLOAD, &params, options, &mut clock, |_| {})
            .unwrap();

        assert_eq!(clock.checks.get(), pauses);
        assert!(rig.lock_is_free());
    }
}

#[test]
fn the_lock_is_also_held_across_the_pauses_of_a_retry() {
    let rig = Rig::new();
    let verified = rig.verified();
    let mut clock = LockHeldCheck {
        path: rig.lock_path(),
        checks: Rc::default(),
    };
    // The probe was attempt 0, so these are Begin and its first retry.
    rig.mock.fail_write(1, i2c_error());
    rig.mock.fail_write(2, i2c_error());
    let options = UploadOptions::default();
    let params = UploadParams::still(Framebuffer::Static);

    Panel::new(Retrying::with_sleep(verified, clock.clone()))
        .upload(&PAYLOAD, &params, options, &mut clock, |_| {})
        .unwrap();

    // Two retries, and the five pauses of the upload.
    assert_eq!(clock.checks.get(), 7);
    assert_eq!(rig.writes_after_probe().len(), 6);
}

#[test]
fn a_tick_cannot_get_in_while_an_upload_pauses() {
    /// Runs the daemon's tick on every pause of the upload.
    struct TickingClock {
        daemon: Unlocked<Mock>,
    }

    impl Sleep for TickingClock {
        fn sleep(&mut self, _duration: Duration) {
            tick(&mut self.daemon, FeedFrameLen::Full).unwrap();
        }
    }

    let rig = Rig::new();
    let daemon = rig.verified().unlock();
    let verified = rig.verified();
    let mut clock = TickingClock { daemon };
    let params = UploadParams::still(Framebuffer::Static);
    let options = UploadOptions::default();

    Panel::new(verified)
        .upload(&PAYLOAD, &params, options, &mut clock, |_| {})
        .unwrap();

    // Only the two probes and the upload: no tick got through.
    let mut expected = vec![probe_frame().to_vec(); 2];
    expected.extend(upload_writes(&params));
    expected.push(mode_frame(Mode::Image).to_vec());
    assert_eq!(rig.mock.writes(), expected);
}

#[test]
fn dry_run_prints_an_upload_without_waiting() {
    let params = UploadParams::still(Framebuffer::Static);
    let mut expected = vec![probe_frame().to_vec()];
    expected.extend(upload_writes(&params));
    expected.push(mode_frame(Mode::Image).to_vec());
    let expected: Vec<&[u8]> = expected.iter().map(Vec::as_slice).collect();

    let (result, text) = in_dry_run(|panel| {
        let options = UploadOptions::default();
        panel.upload(&PAYLOAD, &params, options, &mut NoSleep, |_| {})
    });

    result.unwrap();
    assert_eq!(text, dump_of(&expected));
}

#[test]
fn dry_run_prints_write_operations_in_order() {
    let ((), text) = in_dry_run(|panel| {
        panel.on().unwrap();
        panel.set_mode(Mode::Gif).unwrap();
        panel.send_sensors(&SAMPLE, FeedFrameLen::Full).unwrap();
    });

    assert_eq!(
        text,
        dump_of(&[
            &probe_frame(),
            &power_frame(true),
            &mode_frame(Mode::Gif),
            &sensor_frame(&SAMPLE, FeedFrameLen::Full),
        ])
    );
}

#[test]
fn dry_run_shows_the_query_and_then_reports_the_missing_readback() {
    let (result, text) = in_dry_run(|panel| panel.firmware());

    assert!(matches!(result, Err(Error::ReadbackUnavailable)));
    assert_eq!(text, dump_of(&[&probe_frame(), &firmware_query_frame()]));
}

#[test]
fn dry_run_loop_banks_stops_at_the_first_missing_readback() {
    let (result, text) = in_dry_run(|panel| panel.loop_banks());

    let query = loop_query_frame(LOOP_BANK_FIRST).unwrap();
    assert!(matches!(result, Err(Error::ReadbackUnavailable)));
    assert_eq!(text, dump_of(&[&probe_frame(), &query]));
}

#[test]
fn dry_run_status_does_not_swallow_the_missing_readback() {
    let (result, text) = in_dry_run(|panel| panel.status());

    assert!(matches!(result, Err(Error::ReadbackUnavailable)));
    assert_eq!(text, dump_of(&[&probe_frame(), &firmware_query_frame()]));
}
