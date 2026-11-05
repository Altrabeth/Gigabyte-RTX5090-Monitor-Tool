//! The daemon loop through its public API, on a fake clock. Time only moves when
//! the code under test sleeps or a test double spends some, so nothing waits in
//! real time and every timestamp below is exact.
//!
//! The gate and the bus lock are the real ones: only the device, the sysfs tree
//! and the lock path are replaced, and nothing here opens a device.

use std::cell::{Cell, RefCell};
use std::fs;
use std::io::{self, Write};
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use aorus_lcd::bus::lock::BusLock;
use aorus_lcd::bus::transport::{DryRun, Transport};
use aorus_lcd::daemon::{
    run_with, Clock, DaemonConfig, Environment, UntilStopped, POLL_SLICE, WRITE_ATTEMPTS,
};
use aorus_lcd::panel::Sleep;
use aorus_lcd::protocol::{
    display_frame, probe_frame, sensor_frame, FeedFrameLen, SensorSample, OP_GET_IMAGE_TPL,
    OP_SAVE, OP_SENSOR_FEED, OP_SET_DISPLAY, PROBE_READ_LEN,
};
use aorus_lcd::sensors::{FakeSensors, SensorSource};
use aorus_lcd::session::{self, connect_with, Options, Session};
use aorus_lcd::{Error, Result};
use rustix::io::Errno;
use tempfile::TempDir;

const PANEL: &str = "NVIDIA i2c adapter 1 at 1:00.0";
const SMBUS: &str = "SMBus I801 adapter at 0000:00:1f.4";
const PROBE: u8 = OP_GET_IMAGE_TPL;
const DASHBOARD: u8 = OP_SET_DISPLAY;
const FEED: u8 = OP_SENSOR_FEED;
const SAVE: u8 = OP_SAVE;
/// Far past anything a test expects, so a loop that never ends fails.
const TIME_LIMIT_MS: u64 = 3_600_000;

const SAMPLE: SensorSample = SensorSample {
    gpu_temp_c: 55,
    gpu_clock_mhz: 2100,
    gpu_usage_percent: 42,
    fan_speed: 1200,
    ram_clock_mhz: 14000,
    ram_usage_percent: 30,
    fps: 0,
    tgp_watts: 250,
};

fn ms(millis: u64) -> Duration {
    Duration::from_millis(millis)
}

type Event = Box<dyn FnOnce()>;
type Hook = Box<dyn Fn()>;

/// Time, starting at 0 and moved only by [`FakeClock::advance`]. Clones share it.
#[derive(Clone)]
struct FakeClock {
    origin: Instant,
    elapsed: Rc<Cell<Duration>>,
    events: Rc<RefCell<Vec<(Duration, Event)>>>,
    on_sleep: Rc<RefCell<Option<Hook>>>,
    longest_sleep: Rc<Cell<Duration>>,
}

impl FakeClock {
    fn new() -> Self {
        Self {
            origin: Instant::now(),
            elapsed: Rc::default(),
            events: Rc::default(),
            on_sleep: Rc::default(),
            longest_sleep: Rc::default(),
        }
    }

    fn elapsed_ms(&self) -> u64 {
        self.elapsed.get().as_millis().try_into().unwrap()
    }

    /// Moves time on, and runs what became due. An event may advance time too.
    fn advance(&self, duration: Duration) {
        self.elapsed.set(self.elapsed.get() + duration);
        assert!(
            self.elapsed_ms() < TIME_LIMIT_MS,
            "the daemon did not stop in fake time"
        );
        let now = self.elapsed.get();
        let due = {
            let mut events = self.events.borrow_mut();
            let (due, later) = events.drain(..).partition(|(at, _)| *at <= now);
            *events = later;
            due
        };
        for (_, event) in due {
            event();
        }
    }

    /// Runs `event` once time reaches `millis`.
    fn at(&self, millis: u64, event: impl FnOnce() + 'static) {
        self.events.borrow_mut().push((ms(millis), Box::new(event)));
    }
}

impl Sleep for FakeClock {
    fn sleep(&mut self, duration: Duration) {
        self.longest_sleep
            .set(self.longest_sleep.get().max(duration));
        if let Some(hook) = self.on_sleep.borrow().as_ref() {
            hook();
        }
        self.advance(duration);
    }
}

impl Clock for FakeClock {
    fn now(&self) -> Instant {
        self.origin + self.elapsed.get()
    }
}

/// One attempted write, as the device saw it.
#[derive(Debug, Clone)]
struct Sent {
    at: u64,
    frame: Vec<u8>,
    ok: bool,
    locked: bool,
}

impl Sent {
    fn opcode(&self) -> u8 {
        self.frame[0]
    }
}

/// What makes a write fail: between two times, for frames of one opcode or all.
struct Fault {
    from: u64,
    to: u64,
    opcode: Option<u8>,
    error: fn() -> Error,
}

impl Fault {
    fn hits(&self, opcode: u8, at: u64) -> bool {
        (self.from..self.to).contains(&at) && self.opcode.is_none_or(|only| only == opcode)
    }
}

fn eio() -> Error {
    errno_error(Errno::IO)
}

fn short_read() -> Error {
    Error::ShortRead {
        expected: PROBE_READ_LEN,
        actual: 0,
    }
}

fn errno_error(errno: Errno) -> Error {
    Error::I2c {
        context: "injected".into(),
        source: errno.into(),
    }
}

/// What the device does, shared between the test and every open handle.
#[derive(Default)]
struct DeviceState {
    trace: RefCell<Vec<Sent>>,
    faults: RefCell<Vec<Fault>>,
    no_answer_until: Cell<u64>,
    missing_until: Cell<u64>,
    refuse_open_from: Cell<Option<u64>>,
    write_cost: Cell<Duration>,
}

struct Device {
    state: Rc<DeviceState>,
    clock: FakeClock,
    lock_path: PathBuf,
}

impl Transport for Device {
    fn write(&mut self, frame: &[u8]) -> Result<()> {
        let at = self.clock.elapsed_ms();
        let locked = matches!(
            BusLock::try_acquire(&self.lock_path),
            Err(Error::LockBusy { .. })
        );
        let faults = self.state.faults.borrow();
        let fault = faults.iter().find(|fault| fault.hits(frame[0], at));
        self.state.trace.borrow_mut().push(Sent {
            at,
            frame: frame.to_vec(),
            ok: fault.is_none(),
            locked,
        });
        let outcome = fault.map_or(Ok(()), |fault| Err((fault.error)()));
        drop(faults);
        self.clock.advance(self.state.write_cost.get());
        outcome
    }

    fn read(&mut self, buf: &mut [u8]) -> Result<()> {
        let at = self.clock.elapsed_ms();
        if at < self.state.no_answer_until.get() {
            return Err(errno_error(Errno::NXIO));
        }
        buf.fill(0x5A);
        Ok(())
    }
}

/// A temporary sysfs tree and lock path, and the fake device and time around
/// one run of the daemon.
struct Rig {
    dir: TempDir,
    clock: FakeClock,
    stop: Arc<AtomicBool>,
    device: Rc<DeviceState>,
    /// The bus lock as a competing upload would hold it.
    upload: Rc<RefCell<Option<BusLock>>>,
    /// `--bus`, if the test gives one.
    bus: Cell<Option<u32>>,
}

impl Rig {
    fn new() -> Self {
        let rig = Self::without_adapter();
        rig.add_adapter(PANEL);
        rig
    }

    fn without_adapter() -> Self {
        let dir = TempDir::new().unwrap();
        fs::create_dir(dir.path().join("run")).unwrap();
        Self {
            dir,
            clock: FakeClock::new(),
            stop: Arc::default(),
            device: Rc::default(),
            upload: Rc::default(),
            bus: Cell::default(),
        }
    }

    fn sysfs_root(&self) -> PathBuf {
        self.dir.path().join("sysfs")
    }

    fn lock_path(&self) -> PathBuf {
        self.dir.path().join("run/test.lock")
    }

    fn add_adapter(&self, name: &str) {
        let adapter = self.sysfs_root().join("i2c-7");
        fs::create_dir_all(&adapter).unwrap();
        fs::write(adapter.join("name"), format!("{name}\n")).unwrap();
    }

    fn fault(&self, from: u64, to: u64, opcode: Option<u8>, error: fn() -> Error) {
        self.device.faults.borrow_mut().push(Fault {
            from,
            to,
            opcode,
            error,
        });
    }

    /// Every write fails between these times.
    fn outage(&self, from: u64, to: u64) {
        self.fault(from, to, None, eio);
    }

    fn take_lock_now(&self) {
        let lock = BusLock::try_acquire(&self.lock_path()).unwrap();
        *self.upload.borrow_mut() = Some(lock);
    }

    /// An upload holds the bus between these times.
    fn upload(&self, from: u64, to: u64) {
        let (upload, path) = (self.upload.clone(), self.lock_path());
        self.clock.at(from, move || {
            *upload.borrow_mut() = Some(BusLock::try_acquire(&path).unwrap());
        });
        let upload = self.upload.clone();
        self.clock.at(to, move || *upload.borrow_mut() = None);
    }

    fn stop_at(&self, millis: u64) {
        let stop = self.stop.clone();
        self.clock
            .at(millis, move || stop.store(true, Ordering::SeqCst));
    }

    fn ticks(&self, ticks: u64) -> Result<()> {
        self.run(Some(ticks), |_| Ok(healthy_sensors()))
    }

    /// Runs and expects a clean end, for tests about sensors.
    fn ticks_with<S: SensorSource>(
        &self,
        ticks: u64,
        open_sensors: impl FnMut(Option<u32>) -> Result<S>,
    ) {
        self.run(Some(ticks), open_sensors).unwrap();
    }

    fn run<S: SensorSource>(
        &self,
        max_ticks: Option<u64>,
        open_sensors: impl FnMut(Option<u32>) -> Result<S>,
    ) -> Result<()> {
        let config = DaemonConfig {
            max_ticks,
            ..DaemonConfig::default()
        };
        run_with(
            config,
            Environment {
                connect: || self.connect(),
                open_sensors,
                clock: self.clock.clone(),
                stop: self.stop.clone(),
                dry_run: false,
            },
        )
    }

    /// The real gate, over the fake device.
    fn connect(&self) -> Result<Session<Device, Vec<u8>>> {
        let open = |_bus| {
            let now = self.clock.elapsed_ms();
            if self
                .device
                .refuse_open_from
                .get()
                .is_some_and(|from| now >= from)
            {
                return Err(errno_error(Errno::ACCESS));
            }
            if now < self.device.missing_until.get() {
                return Err(errno_error(Errno::NOENT));
            }
            Ok(Device {
                state: self.device.clone(),
                clock: self.clock.clone(),
                lock_path: self.lock_path(),
            })
        };
        connect_with(
            &Options {
                bus: self.bus.get(),
                ..Options::default()
            },
            session::Environment {
                sysfs_root: self.sysfs_root(),
                lock_path: self.lock_path(),
                open,
                sleep: UntilStopped {
                    sleep: self.clock.clone(),
                    stop: self.stop.clone(),
                },
                dry_run_output: Vec::new(),
            },
        )
    }

    fn trace(&self) -> Vec<Sent> {
        self.device.trace.borrow().clone()
    }

    /// When frames of `opcode` were attempted, failed ones included.
    fn attempts(&self, opcode: u8) -> Vec<u64> {
        let trace = self.trace();
        let matching = trace.iter().filter(|sent| sent.opcode() == opcode);
        matching.map(|sent| sent.at).collect()
    }

    /// When frames of `opcode` went out.
    fn delivered(&self, opcode: u8) -> Vec<u64> {
        let trace = self.trace();
        let matching = trace
            .iter()
            .filter(|sent| sent.ok && sent.opcode() == opcode);
        matching.map(|sent| sent.at).collect()
    }

    /// The samples of the `E3` frames that went out, as frames.
    fn feed_frames(&self) -> Vec<Vec<u8>> {
        let trace = self.trace();
        let feed = trace.iter().filter(|sent| sent.ok && sent.opcode() == FEED);
        feed.map(|sent| sent.frame.clone()).collect()
    }

    fn assert_stopped_promptly(&self, asked_at: u64) {
        let late = self.clock.elapsed_ms() - asked_at;
        assert!(
            ms(late) <= POLL_SLICE,
            "stopped {late} ms after the request"
        );
        let trace = self.trace();
        assert!(
            trace.iter().all(|sent| sent.at < asked_at),
            "sent after the stop request: {trace:?}"
        );
    }
}

fn healthy_sensors() -> FakeSensors {
    FakeSensors {
        sample: SAMPLE,
        ..FakeSensors::default()
    }
}

fn broken_sensors() -> FakeSensors {
    FakeSensors {
        fail: true,
        ..FakeSensors::default()
    }
}

fn no_sensors(_: Option<u32>) -> Result<FakeSensors> {
    Err(Error::Adapter("no sensors in this test".into()))
}

/// The rules of the wire, over a whole trace. Failed attempts count: a frame that
/// reached the controller partly is still a frame sent.
///
/// Every frame is `EB 03`, the configured `E1`, or `E3`, and each `E1` directly
/// follows a probe that went out, once per connection, with `E3` only after it.
fn assert_only_allowed_frames(trace: &[Sent]) {
    let config = DaemonConfig::default();
    let dashboard = display_frame(config.elements, config.interval_s);
    let mut probed = false;
    let mut configured = false;
    for sent in trace {
        match sent.opcode() {
            PROBE => {
                assert_eq!(sent.frame[..], probe_frame()[..], "not EB 03");
                probed = sent.ok;
                configured = false;
            }
            DASHBOARD => {
                assert_eq!(sent.frame[..], dashboard[..], "an unexpected E1");
                assert!(probed, "E1 without a probe just before it: {sent:?}");
                probed = false;
                configured = sent.ok;
            }
            FEED => {
                assert_eq!(sent.frame.len(), FeedFrameLen::Full.bytes());
                assert!(configured, "E3 before the connection's E1: {sent:?}");
            }
            SAVE => panic!("AA must never be sent: {sent:?}"),
            other => panic!("the daemon must not send {other:02X}: {sent:?}"),
        }
        assert!(sent.locked, "written without the lock: {sent:?}");
    }
}

/// How long the sensors take, and whether they work, tick by tick.
struct ScriptedSensors<G> {
    read: G,
}

impl<G: FnMut() -> Result<SensorSample>> SensorSource for ScriptedSensors<G> {
    fn read(&mut self) -> Result<SensorSample> {
        (self.read)()
    }
}

#[test]
fn the_panel_gets_probe_then_dashboard_then_feed() {
    let rig = Rig::new();

    rig.ticks(3).unwrap();

    let trace = rig.trace();
    let opcodes: Vec<u8> = trace.iter().map(Sent::opcode).collect();
    assert_eq!(opcodes, [PROBE, DASHBOARD, FEED, FEED, FEED]);
    assert_eq!(rig.delivered(FEED), [0, 1000, 2000]);
    assert_eq!(
        trace[1].frame[..],
        display_frame(DaemonConfig::default().elements, 3)[..]
    );
    assert_eq!(
        trace[2].frame[..],
        sensor_frame(&SAMPLE, FeedFrameLen::Full)[..]
    );
    assert_only_allowed_frames(&trace);
}

#[test]
fn the_feed_length_comes_from_the_config() {
    let rig = Rig::new();
    let config = DaemonConfig {
        feed: FeedFrameLen::Truncated,
        max_ticks: Some(1),
        ..DaemonConfig::default()
    };

    run_with(
        config,
        Environment {
            connect: || rig.connect(),
            open_sensors: |_| Ok(healthy_sensors()),
            clock: rig.clock.clone(),
            stop: rig.stop.clone(),
            dry_run: false,
        },
    )
    .unwrap();

    assert_eq!(rig.feed_frames()[0].len(), 128);
}

#[test]
fn work_does_not_push_the_cadence() {
    let rig = Rig::new();
    rig.device.write_cost.set(ms(20));
    let clock = rig.clock.clone();
    let slow_sensors = move |_: Option<u32>| {
        let clock = clock.clone();
        Ok(ScriptedSensors {
            read: move || {
                clock.advance(ms(30));
                Ok(SAMPLE)
            },
        })
    };

    rig.run(Some(600), slow_sensors).unwrap();

    let feed = rig.delivered(FEED);
    let start = feed[0];
    let expected: Vec<u64> = (0..600).map(|tick| start + tick * 1000).collect();
    assert_eq!(feed, expected);
}

#[test]
fn a_jump_in_time_gives_one_frame_and_no_catch_up() {
    let rig = Rig::new();
    let clock = rig.clock.clone();
    rig.clock.at(5500, move || clock.advance(ms(60_000)));

    rig.ticks(9).unwrap();

    assert_eq!(
        rig.delivered(FEED),
        [0, 1000, 2000, 3000, 4000, 5000, 65_500, 66_500, 67_500]
    );
}

#[test]
fn a_stalled_tick_gives_no_catch_up() {
    let rig = Rig::new();
    let clock = rig.clock.clone();
    let reads = Rc::new(Cell::new(0));
    let stalling_sensors = move |_: Option<u32>| {
        let (clock, reads) = (clock.clone(), reads.clone());
        Ok(ScriptedSensors {
            read: move || {
                reads.set(reads.get() + 1);
                if reads.get() == 4 {
                    clock.advance(ms(3500));
                }
                Ok(SAMPLE)
            },
        })
    };

    rig.run(Some(6), stalling_sensors).unwrap();

    assert_eq!(rig.delivered(FEED), [0, 1000, 2000, 6500, 7500, 8500]);
}

#[test]
fn a_busy_lock_skips_ticks_without_waiting_or_backing_off() {
    let rig = Rig::new();
    rig.upload(2500, 5500);

    rig.ticks(8).unwrap();

    assert_eq!(rig.delivered(FEED), [0, 1000, 2000, 6000, 7000]);
    assert_eq!(rig.attempts(PROBE), [0], "skipping must not reconnect");
    assert_eq!(rig.attempts(DASHBOARD), [0]);
    assert_only_allowed_frames(&rig.trace());
}

#[test]
fn a_long_upload_ends_in_one_fresh_frame_and_not_a_burst() {
    let rig = Rig::new();
    rig.upload(500, 300_500);

    rig.ticks(303).unwrap();

    assert_eq!(rig.delivered(FEED), [0, 301_000, 302_000]);
}

#[test]
fn the_lock_is_only_held_for_writes() {
    let rig = Rig::new();
    rig.outage(2500, 3500);
    let path = rig.lock_path();
    let lock_free = move || {
        drop(BusLock::try_acquire(&path).unwrap());
    };
    let during_read = lock_free.clone();
    *rig.clock.on_sleep.borrow_mut() = Some(Box::new(lock_free));
    let probing_sensors = move |_: Option<u32>| {
        let during_read = during_read.clone();
        Ok(ScriptedSensors {
            read: move || {
                during_read();
                Ok(SAMPLE)
            },
        })
    };

    rig.run(Some(5), probing_sensors).unwrap();

    assert_eq!(rig.delivered(FEED), [0, 1000, 2000, 4000]);
    assert_only_allowed_frames(&rig.trace());
}

#[test]
fn every_wait_is_short_enough_to_notice_a_stop_request() {
    let rig = Rig::new();
    rig.outage(2500, 9000);

    rig.ticks(8).unwrap();

    assert!(rig.clock.longest_sleep.get() <= POLL_SLICE);
}

#[test]
fn a_transient_failure_is_retried_on_the_next_tick_without_reconnecting() {
    let rig = Rig::new();
    rig.outage(1500, 2500);

    rig.ticks(5).unwrap();

    assert_eq!(rig.attempts(FEED), [0, 1000, 2000, 3000, 4000]);
    assert_eq!(rig.delivered(FEED), [0, 1000, 3000, 4000]);
    assert_eq!(rig.attempts(PROBE), [0]);
    assert_eq!(rig.attempts(DASHBOARD), [0]);
}

#[test]
fn a_good_write_clears_the_count_of_failures() {
    let rig = Rig::new();
    rig.outage(500, 2500);
    rig.outage(3500, 5500);
    assert_eq!(WRITE_ATTEMPTS, 3);

    rig.ticks(7).unwrap();

    assert_eq!(rig.delivered(FEED), [0, 3000, 6000]);
    assert_eq!(rig.attempts(PROBE), [0]);
}

#[test]
fn reconnect_backs_off_sends_e1_once_and_resets_after_a_good_write() {
    let rig = Rig::new();
    rig.outage(2500, 20_000);
    rig.outage(36_500, 39_500);

    rig.ticks(14).unwrap();

    // Three failed ticks, then waits of 2, 4, 8 and 16 seconds. The second outage
    // starts over at 2 seconds.
    assert_eq!(
        rig.attempts(PROBE),
        [0, 7000, 11_000, 19_000, 35_000, 41_000]
    );
    assert_eq!(rig.attempts(DASHBOARD), [0, 35_000, 41_000]);
    assert_eq!(
        rig.delivered(FEED),
        [0, 1000, 2000, 35_000, 36_000, 41_000, 42_000, 43_000]
    );
    assert_only_allowed_frames(&rig.trace());
}

#[test]
fn a_controller_that_stops_answering_is_retried_through_the_probe() {
    let rig = Rig::new();
    rig.device.no_answer_until.set(20_000);

    rig.ticks(2).unwrap();

    assert_eq!(rig.attempts(PROBE), [0, 2000, 6000, 14_000, 30_000]);
    assert_eq!(rig.attempts(DASHBOARD), [30_000]);
}

#[test]
fn a_short_read_counts_as_a_bus_error() {
    let rig = Rig::new();
    rig.fault(2500, 5500, Some(FEED), short_read);

    rig.ticks(7).unwrap();

    assert_eq!(rig.attempts(PROBE), [0, 7000]);
    assert_eq!(rig.attempts(DASHBOARD), [0, 7000]);
}

#[test]
fn a_failed_e1_is_not_repeated_on_the_same_connection() {
    let rig = Rig::new();
    rig.fault(0, 1000, Some(DASHBOARD), eio);

    rig.ticks(2).unwrap();

    assert_eq!(rig.attempts(PROBE), [0, 2000]);
    assert_eq!(rig.attempts(DASHBOARD), [0, 2000]);
    assert_eq!(rig.delivered(DASHBOARD), [2000]);
    assert_only_allowed_frames(&rig.trace());
}

// The bus passes the gate and fails on the feed. Only a good E3 resets the
// backoff, or this would send an E1 every five seconds for as long as it lasts.
#[test]
fn e1_does_not_count_as_a_good_write() {
    let rig = Rig::new();
    rig.fault(500, TIME_LIMIT_MS, Some(FEED), eio);

    rig.ticks(15).unwrap();

    assert_eq!(rig.delivered(DASHBOARD), [0, 5000, 11_000, 21_000, 39_000]);
    assert_only_allowed_frames(&rig.trace());
}

#[test]
fn startup_waits_for_a_late_adapter() {
    let rig = Rig::without_adapter();
    let (root, name) = (rig.sysfs_root().join("i2c-7"), format!("{PANEL}\n"));
    rig.clock.at(9000, move || {
        fs::create_dir_all(&root).unwrap();
        fs::write(root.join("name"), name).unwrap();
    });

    rig.ticks(2).unwrap();

    assert_eq!(rig.delivered(PROBE), [14_000]);
    assert_eq!(rig.delivered(FEED), [14_000, 15_000]);
}

// Absent is not refused: `--bus` can name an adapter that has yet to appear.
#[test]
fn startup_waits_for_a_bus_override_that_is_not_listed_yet() {
    let rig = Rig::without_adapter();
    rig.bus.set(Some(7));
    let (root, name) = (rig.sysfs_root().join("i2c-7"), format!("{PANEL}\n"));
    rig.clock.at(9000, move || {
        fs::create_dir_all(&root).unwrap();
        fs::write(root.join("name"), name).unwrap();
    });

    rig.ticks(1).unwrap();

    assert_eq!(rig.delivered(PROBE), [14_000]);
}

#[test]
fn a_refused_bus_override_is_fatal_at_startup() {
    let rig = Rig::without_adapter();
    rig.add_adapter(SMBUS);
    rig.bus.set(Some(7));

    let error = rig.run(None, no_sensors).unwrap_err();

    assert!(matches!(error, Error::AdapterRefused(_)), "{error:?}");
    assert!(error.to_string().contains(SMBUS), "{error}");
    assert!(rig.trace().is_empty());
    assert_eq!(rig.clock.elapsed_ms(), 0);
}

#[test]
fn a_refused_bus_override_is_fatal_on_reconnect() {
    let rig = Rig::new();
    rig.bus.set(Some(7));
    rig.outage(500, 3500);
    let name = rig.sysfs_root().join("i2c-7/name");
    rig.clock
        .at(3500, move || fs::write(name, format!("{SMBUS}\n")).unwrap());

    let error = rig.run(None, |_| Ok(healthy_sensors())).unwrap_err();

    assert!(matches!(error, Error::AdapterRefused(_)), "{error:?}");
    assert_eq!(rig.attempts(PROBE), [0]);
}

#[test]
fn startup_waits_for_a_late_device_node() {
    let rig = Rig::new();
    rig.device.missing_until.set(1000);

    rig.ticks(1).unwrap();

    assert_eq!(rig.delivered(PROBE), [2000]);
}

#[test]
fn startup_backs_off_to_the_cap_while_the_controller_stays_silent() {
    let rig = Rig::new();
    rig.device.no_answer_until.set(150_000);

    rig.ticks(1).unwrap();

    assert_eq!(
        rig.attempts(PROBE),
        [0, 2000, 6000, 14_000, 30_000, 62_000, 122_000, 182_000]
    );
    assert_eq!(rig.attempts(DASHBOARD), [182_000]);
}

#[test]
fn startup_retries_while_an_upload_holds_the_lock() {
    let rig = Rig::new();
    rig.take_lock_now();
    let upload = rig.upload.clone();
    rig.clock.at(12_000, move || *upload.borrow_mut() = None);

    rig.ticks(1).unwrap();

    let trace = rig.trace();
    assert_eq!(trace[0].opcode(), PROBE);
    assert!(trace[0].at >= 12_000, "{trace:?}");
    assert_only_allowed_frames(&trace);
}

#[test]
fn an_unusable_lock_file_is_fatal_at_startup() {
    let rig = Rig::new();
    fs::remove_dir(rig.dir.path().join("run")).unwrap();

    let error = rig.run(None, no_sensors).unwrap_err();

    assert!(matches!(error, Error::Lock { .. }), "{error:?}");
    assert!(rig.trace().is_empty());
    assert_eq!(rig.clock.elapsed_ms(), 0);
}

#[test]
fn a_permission_error_is_fatal_at_startup() {
    let rig = Rig::new();
    rig.device.refuse_open_from.set(Some(0));

    let error = rig.run(None, no_sensors).unwrap_err();

    assert!(matches!(error, Error::I2c { .. }), "{error:?}");
    assert!(rig.trace().is_empty());
    assert_eq!(rig.clock.elapsed_ms(), 0);
}

#[test]
fn a_permission_error_is_fatal_on_reconnect() {
    let rig = Rig::new();
    rig.outage(500, 3500);
    rig.device.refuse_open_from.set(Some(3500));

    let error = rig.run(None, |_| Ok(healthy_sensors())).unwrap_err();

    assert!(matches!(error, Error::I2c { .. }), "{error:?}");
    assert_eq!(rig.attempts(PROBE), [0]);
}

#[test]
fn a_lock_file_lost_while_running_is_fatal_and_nothing_is_written_without_it() {
    let rig = Rig::new();
    let run_dir = rig.dir.path().join("run");
    rig.clock
        .at(2500, move || fs::remove_dir_all(run_dir).unwrap());

    let error = rig.run(None, |_| Ok(healthy_sensors())).unwrap_err();

    assert!(matches!(error, Error::Lock { .. }), "{error:?}");
    assert_eq!(rig.delivered(FEED), [0, 1000, 2000]);
}

#[test]
fn without_sensors_the_feed_shows_zeros_and_the_source_is_retried_with_backoff() {
    let rig = Rig::new();
    let clock = rig.clock.clone();
    let opened_at = Rc::new(RefCell::new(Vec::new()));
    let attempts = opened_at.clone();
    let open = move |_: Option<u32>| {
        attempts.borrow_mut().push(clock.elapsed_ms());
        no_sensors(None)
    };

    rig.ticks_with(16, open);

    assert_eq!(*opened_at.borrow(), [0, 2000, 6000, 14_000]);
    let zeros = sensor_frame(&SensorSample::default(), FeedFrameLen::Full);
    let frames = rig.feed_frames();
    assert_eq!(frames.len(), 16);
    assert!(frames.iter().all(|frame| frame[..] == zeros[..]));
}

#[test]
fn a_failing_source_means_zeros_not_a_stale_value_and_it_comes_back() {
    let rig = Rig::new();
    let clock = rig.clock.clone();
    let opened_at = Rc::new(RefCell::new(Vec::new()));
    let attempts = opened_at.clone();
    let open = move |_: Option<u32>| {
        let now = clock.elapsed_ms();
        attempts.borrow_mut().push(now);
        // The first source dies at the third read. Reinitializing succeeds, but
        // its reads keep failing until the driver is back at 8 seconds.
        let healthy = now == 0 || now >= 8000;
        let reads = Rc::new(Cell::new(0));
        let first = now == 0;
        Ok(ScriptedSensors {
            read: move || {
                reads.set(reads.get() + 1);
                if (first && reads.get() >= 3) || !healthy {
                    broken_sensors().read()
                } else {
                    Ok(SAMPLE)
                }
            },
        })
    };

    rig.ticks_with(12, open);

    assert_eq!(*opened_at.borrow(), [0, 4000, 8000]);
    let live = sensor_frame(&SAMPLE, FeedFrameLen::Full);
    let zeros = sensor_frame(&SensorSample::default(), FeedFrameLen::Full);
    let shown: Vec<bool> = rig
        .feed_frames()
        .iter()
        .map(|frame| {
            assert!(frame[..] == live[..] || frame[..] == zeros[..]);
            frame[..] == live[..]
        })
        .collect();
    let mut expected = [false; 12];
    expected[..2].fill(true);
    expected[8..].fill(true);
    assert_eq!(shown, expected);
}

#[test]
fn sensors_get_the_pci_bus_of_the_adapter() {
    let rig = Rig::new();
    let seen = Rc::new(Cell::new(None));
    let saw = seen.clone();

    rig.ticks_with(1, move |bus| {
        saw.set(bus);
        Ok(healthy_sensors())
    });

    assert_eq!(seen.get(), Some(1));
}

#[test]
fn an_adapter_without_a_pci_bus_leaves_the_decision_to_the_sensor_side() {
    let rig = Rig::without_adapter();
    rig.add_adapter("NVIDIA i2c adapter 1 at an unknown place");
    let seen = Rc::new(Cell::new(Some(99)));
    let saw = seen.clone();

    rig.ticks_with(2, move |bus| {
        saw.set(bus);
        no_sensors(bus)
    });

    assert_eq!(seen.get(), None);
    assert_eq!(rig.delivered(FEED), [0, 1000]);
}

#[test]
fn sensors_survive_a_reconnect() {
    let rig = Rig::new();
    rig.outage(500, 3500);
    let opens = Rc::new(Cell::new(0));
    let count = opens.clone();

    rig.ticks_with(5, move |_| {
        count.set(count.get() + 1);
        Ok(healthy_sensors())
    });

    assert_eq!(opens.get(), 1);
    assert_eq!(rig.delivered(FEED), [0, 5000]);
}

#[test]
fn shutdown_between_ticks_is_prompt_and_sends_nothing() {
    let rig = Rig::new();
    rig.stop_at(3450);

    rig.run(None, |_| Ok(healthy_sensors())).unwrap();

    rig.assert_stopped_promptly(3450);
    assert_eq!(rig.delivered(FEED), [0, 1000, 2000, 3000]);
    assert_only_allowed_frames(&rig.trace());
}

#[test]
fn shutdown_during_backoff_is_prompt_and_sends_nothing() {
    let rig = Rig::new();
    rig.outage(0, TIME_LIMIT_MS);
    rig.stop_at(8050);

    rig.run(None, |_| Ok(healthy_sensors())).unwrap();

    rig.assert_stopped_promptly(8050);
}

#[test]
fn shutdown_during_a_lock_wait_in_the_gate_is_prompt() {
    let rig = Rig::new();
    rig.take_lock_now();
    rig.stop_at(1050);

    rig.run(None, |_| Ok(healthy_sensors())).unwrap();

    rig.assert_stopped_promptly(1050);
}

#[test]
fn a_stop_request_before_the_start_touches_nothing() {
    let rig = Rig::new();
    rig.stop.store(true, Ordering::SeqCst);

    rig.run(None, |_| Ok(healthy_sensors())).unwrap();

    assert!(rig.trace().is_empty());
}

#[test]
fn shutdown_during_the_probe_does_not_go_on_to_e1() {
    let rig = Rig::new();
    rig.device.write_cost.set(ms(10));
    rig.stop_at(5);

    rig.run(None, |_| Ok(healthy_sensors())).unwrap();

    let opcodes: Vec<u8> = rig.trace().iter().map(Sent::opcode).collect();
    assert_eq!(opcodes, [PROBE]);
}

#[test]
fn shutdown_during_a_sensor_read_sends_no_frame() {
    let rig = Rig::new();
    let stop = rig.stop.clone();
    let reads = Rc::new(Cell::new(0));
    let stopping_sensors = move |_: Option<u32>| {
        let (stop, reads) = (stop.clone(), reads.clone());
        Ok(ScriptedSensors {
            read: move || {
                reads.set(reads.get() + 1);
                stop.store(reads.get() == 3, Ordering::SeqCst);
                Ok(SAMPLE)
            },
        })
    };

    rig.run(None, stopping_sensors).unwrap();

    assert_eq!(rig.delivered(FEED), [0, 1000]);
}

#[test]
fn a_write_in_progress_finishes_before_shutdown() {
    let rig = Rig::new();
    rig.device.write_cost.set(ms(10));
    rig.stop_at(1025);

    rig.run(None, |_| Ok(healthy_sensors())).unwrap();

    let feed = rig.delivered(FEED);
    assert_eq!(feed.last(), Some(&1020), "{feed:?}");
    assert!(rig.clock.elapsed_ms() <= 1030);
}

#[test]
fn zero_ticks_touches_nothing() {
    let rig = Rig::new();

    rig.ticks(0).unwrap();

    assert!(rig.trace().is_empty());
}

// Everything that can go wrong in one run: outages, a flapping feed, uploads,
// failing sensors, a jump in time and a lost device. Whatever the order, the wire
// only ever sees EB 03, one E1 per connection, and E3.
#[test]
fn no_forbidden_frame_in_a_full_trace() {
    let rig = Rig::new();
    rig.outage(3500, 9000);
    rig.fault(20_500, 30_500, Some(FEED), eio);
    rig.fault(40_000, 40_100, Some(DASHBOARD), eio);
    rig.fault(45_500, 48_500, Some(FEED), short_read);
    rig.upload(60_500, 66_500);
    rig.outage(70_500, 75_500);
    rig.device.no_answer_until.set(5000);
    let clock = rig.clock.clone();
    rig.clock.at(90_500, move || clock.advance(ms(600_000)));
    rig.stop_at(800_000);
    let opens = Rc::new(Cell::new(0));

    rig.run(None, move |_| {
        opens.set(opens.get() + 1);
        if opens.get() % 3 == 0 {
            Ok(healthy_sensors())
        } else {
            Ok(broken_sensors())
        }
    })
    .unwrap();

    let trace = rig.trace();
    assert_only_allowed_frames(&trace);
    let connections = rig.delivered(DASHBOARD).len();
    assert!(
        connections >= 4,
        "only {connections} connections: {trace:?}"
    );
    assert!(rig.delivered(FEED).len() > 100);
    let feed = rig.delivered(FEED);
    assert!(
        feed.windows(2).all(|pair| pair[1] - pair[0] >= 1000),
        "frames closer than a tick: {feed:?}"
    );
}

/// A [`Write`] that keeps what it gets, for checking after the run.
#[derive(Clone, Default)]
struct Output(Rc<RefCell<Vec<u8>>>);

impl Write for Output {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.0.borrow_mut().extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

/// What a closed stdout looks like.
#[derive(Clone)]
struct ClosedPipe;

impl Write for ClosedPipe {
    fn write(&mut self, _: &[u8]) -> io::Result<usize> {
        Err(io::ErrorKind::BrokenPipe.into())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

/// Runs a dry-run printing to clones of `output`, with no sensors.
fn dry_run<W: Write + Clone>(rig: &Rig, max_ticks: Option<u64>, output: W) -> Result<()> {
    let config = DaemonConfig {
        max_ticks,
        ..DaemonConfig::default()
    };
    run_with(
        config,
        Environment {
            connect: || {
                let dry_run = DryRun::with_output(output.clone(), false);
                Ok(Session::<Device, W>::DryRun(dry_run))
            },
            open_sensors: no_sensors,
            clock: rig.clock.clone(),
            stop: rig.stop.clone(),
            dry_run: true,
        },
    )
}

#[test]
fn dry_run_prints_the_feed_with_zeros_if_there_are_no_sensors() {
    let rig = Rig::new();
    let output = Output::default();

    dry_run(&rig, Some(3), output.clone()).unwrap();

    let text = String::from_utf8(output.0.borrow().clone()).unwrap();
    let dashboards = text.lines().filter(|line| line.starts_with("E1 "));
    assert_eq!(dashboards.count(), 1, "{text}");
    let feeds = text.lines().filter(|line| line.starts_with("E3 "));
    assert_eq!(feeds.count(), 3, "{text}");
    assert_eq!(rig.clock.elapsed_ms(), 2000);
}

#[test]
fn dry_run_ends_on_a_closed_stdout_rather_than_reconnecting() {
    let rig = Rig::new();
    let error = dry_run(&rig, None, ClosedPipe).unwrap_err();

    assert!(matches!(error, Error::I2c { .. }), "{error:?}");
    assert_eq!(rig.clock.elapsed_ms(), 0);
}
