//! The daemon loop through its public API, on a fake clock. Time only moves when
//! the code under test sleeps or a test double spends some, so nothing waits in
//! real time and every timestamp below is exact.
//!
//! The gate and the bus lock are the real ones: only the device, the sysfs tree
//! and the lock path are replaced, and nothing here opens a device.

use std::cell::{Cell, RefCell};
use std::collections::VecDeque;
use std::fs;
use std::io::{self, Write};
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Once};
use std::time::{Duration, Instant};

use aorus_lcd::bus::lock::BusLock;
use aorus_lcd::bus::transport::{DryRun, Transport};
use aorus_lcd::daemon::{
    run_with, Clock, DaemonConfig, Environment, FeedStrategy, UntilStopped, ADAPTIVE_BUSY_PERCENT,
    ADAPTIVE_PAIR_GAP_TICKS, ADAPTIVE_WINDOW_POLLS, CHECK_PERIOD, DASHBOARD_ATTEMPTS,
    MODE_ATTEMPTS, POLL_SLICE, SETTLE, WRITE_ATTEMPTS, WRITE_COST_REPORT_TICKS,
};
use aorus_lcd::panel::Sleep;
use aorus_lcd::protocol::{
    display_frame, display_query_frame, mode_frame, mode_query_frame, probe_frame, sensor_frame,
    DisplayStatus, Elements, FeedFrameLen, Mode, ModeStatus, SensorSample, OP_GET_DISPLAY,
    OP_GET_IMAGE_TPL, OP_GET_MODE, OP_SAVE, OP_SENSOR_FEED, OP_SET_DISPLAY, OP_SET_MODE,
    PROBE_READ_LEN,
};
use aorus_lcd::sensors::{FakeSensors, SensorSource};
use aorus_lcd::session::{self, connect_with, Options, Session};
use aorus_lcd::{Error, Result};
use log::{Level, Log, Metadata, Record};
use rustix::io::Errno;
use tempfile::TempDir;

const PANEL: &str = "NVIDIA i2c adapter 1 at 1:00.0";
const SMBUS: &str = "SMBus I801 adapter at 0000:00:1f.4";
const PROBE: u8 = OP_GET_IMAGE_TPL;
const READ_BACK: u8 = OP_GET_DISPLAY;
const DASHBOARD: u8 = OP_SET_DISPLAY;
const FEED: u8 = OP_SENSOR_FEED;
const SAVE: u8 = OP_SAVE;
const MODE_QUERY: u8 = OP_GET_MODE;
const SET_MODE: u8 = OP_SET_MODE;
/// The raw `DE` byte of mode 8, the preset rotation, which the real panel read
/// after a power loss.
const RAW_PRESET_ROTATION: u8 = 0x0B;
/// The raw `DE` byte of mode 0.
const RAW_STATS: u8 = 0x01;
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

/// One attempted write, as the device saw it, with the read that followed it.
#[derive(Debug, Clone)]
struct Sent {
    at: u64,
    frame: Vec<u8>,
    /// Neither the write nor the read after it failed.
    ok: bool,
    /// The lock was held for the write and the read after it.
    locked: bool,
    /// What `DF` reported at the time of the write.
    panel: DisplayStatus,
    /// The raw mode `DE` reported at the time of the write.
    mode: u8,
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
    read_faults: RefCell<Vec<Fault>>,
    /// The widget mask and interval.
    panel: Cell<(u8, u8)>,
    /// The raw mode byte `DE` reports, which `E5` sets.
    mode: Cell<u8>,
    /// `E1` goes unnoticed, as if the controller had lost it right away.
    forgets_e1: Cell<bool>,
    /// How many more `E1` go unnoticed, after which they are taken.
    ignores_e1: Cell<u32>,
    /// How many more `E5` go unnoticed. `u32::MAX` for a panel that never
    /// takes one.
    ignores_e5: Cell<u32>,
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

impl Device {
    fn locked(&self) -> bool {
        matches!(
            BusLock::try_acquire(&self.lock_path),
            Err(Error::LockBusy { .. })
        )
    }

    fn read_error(&self, opcode: u8, at: u64) -> Option<Error> {
        if at < self.state.no_answer_until.get() {
            return Some(errno_error(Errno::NXIO));
        }
        let faults = self.state.read_faults.borrow();
        let fault = faults.iter().find(|fault| fault.hits(opcode, at));
        fault.map(|fault| (fault.error)())
    }

    /// `E1` carries one flag byte per widget, then the interval.
    fn store_dashboard(&self, frame: &[u8]) {
        let flags = frame[5..13].iter().enumerate();
        let mask = flags.fold(0, |mask, (bit, flag)| mask | (flag & 1) << bit);
        self.state.panel.set((mask, frame[13]));
    }
}

impl Transport for Device {
    fn write(&mut self, frame: &[u8]) -> Result<()> {
        let at = self.clock.elapsed_ms();
        let locked = self.locked();
        let faults = self.state.faults.borrow();
        let fault = faults.iter().find(|fault| fault.hits(frame[0], at));
        self.state.trace.borrow_mut().push(Sent {
            at,
            frame: frame.to_vec(),
            ok: fault.is_none(),
            locked,
            panel: panel_status(self.state.panel.get()),
            mode: self.state.mode.get(),
        });
        let forgets_e1 = self.state.forgets_e1.get();
        if fault.is_none()
            && frame[0] == DASHBOARD
            && !forgets_e1
            && !ignored(&self.state.ignores_e1)
        {
            self.store_dashboard(frame);
        }
        // `E5` carries the wire byte, which is also what `DE` reads back.
        if fault.is_none() && frame[0] == SET_MODE && !ignored(&self.state.ignores_e5) {
            self.state.mode.set(frame[5]);
        }
        let outcome = fault.map_or(Ok(()), |fault| Err((fault.error)()));
        drop(faults);
        self.clock.advance(self.state.write_cost.get());
        outcome
    }

    fn read(&mut self, buf: &mut [u8]) -> Result<()> {
        let at = self.clock.elapsed_ms();
        let locked = self.locked();
        let mut trace = self.state.trace.borrow_mut();
        let query = trace.last_mut().expect("a read follows a query");
        query.locked &= locked;
        if let Some(error) = self.read_error(query.opcode(), at) {
            query.ok = false;
            return Err(error);
        }
        buf.fill(0x5A);
        if query.opcode() == READ_BACK {
            let (mask, interval) = self.state.panel.get();
            buf.copy_from_slice(&[READ_BACK, mask, interval, 0]);
        }
        if query.opcode() == MODE_QUERY {
            buf.copy_from_slice(&[MODE_QUERY, self.state.mode.get(), 1, 0]);
        }
        Ok(())
    }
}

/// Whether a frame goes unnoticed, using up one of `count`.
fn ignored(count: &Cell<u32>) -> bool {
    let left = count.get();
    if left > 0 && left != u32::MAX {
        count.set(left - 1);
    }
    left > 0
}

fn panel_status((mask, interval_s): (u8, u8)) -> DisplayStatus {
    DisplayStatus {
        elements: Elements::from_bits(mask),
        interval_s,
    }
}

/// The widgets and interval that `E1` sends when the config names none.
const DEFAULT_MASK: u8 = 0xAF;
const DEFAULT_INTERVAL: u8 = 3;

/// A temporary sysfs tree and lock path, and the fake device and time around
/// one run of the daemon. Its panel shows the default widgets, so the connect
/// steps send no `E1` and cost no settle unless a test empties it with
/// [`Rig::panel_has`].
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
        let rig = Self {
            dir,
            clock: FakeClock::new(),
            stop: Arc::default(),
            device: Rc::default(),
            upload: Rc::default(),
            bus: Cell::default(),
        };
        rig.panel_has(DEFAULT_MASK, DEFAULT_INTERVAL);
        rig
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

    /// The read after a frame of `opcode` fails between these times.
    fn read_fault(&self, from: u64, to: u64, opcode: u8, error: fn() -> Error) {
        self.device.read_faults.borrow_mut().push(Fault {
            from,
            to,
            opcode: Some(opcode),
            error,
        });
    }

    /// The raw mode `DE` reports from now on.
    fn panel_in_mode(&self, raw: u8) {
        self.device.mode.set(raw);
    }

    /// What `DF` reports from now on.
    fn panel_has(&self, mask: u8, interval: u8) {
        self.device.panel.set((mask, interval));
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
            ..steady()
        };
        self.run_configured(config, open_sensors)
    }

    fn run_configured<S: SensorSource>(
        &self,
        config: DaemonConfig,
        open_sensors: impl FnMut(Option<u32>) -> Result<S>,
    ) -> Result<()> {
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

    fn opcodes(&self) -> Vec<u8> {
        self.trace().iter().map(Sent::opcode).collect()
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

/// The steady feed, for tests of what happens on each tick. The default feed
/// sends the same below its threshold, but these do not depend on that.
fn steady() -> DaemonConfig {
    DaemonConfig {
        strategy: FeedStrategy::Steady,
        ..DaemonConfig::default()
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

fn assert_only_allowed_frames(trace: &[Sent]) {
    assert_allowed_frames(trace, DaemonConfig::default());
}

/// `prev`, if it is a frame of `opcode` that went out.
fn after(prev: Option<&Sent>, opcode: u8) -> Option<&Sent> {
    prev.filter(|sent| sent.ok && sent.opcode() == opcode)
}

/// Where one connection is, for [`assert_allowed_frames`].
#[derive(Default)]
struct Setup {
    e5s: u32,
    e1s: u32,
    /// When the connect steps ended, so `E3` and the check may follow.
    configured_at: Option<u64>,
    /// The earliest time the next check may read.
    check_from: u64,
    /// The check read a `DE` that needs nothing, so its `DF` comes next.
    mode_checked: bool,
    /// The check found the panel needing a new setup, so only a probe may follow.
    stale: bool,
}

/// The rules of the wire, over a whole trace. Failed attempts count: a frame that
/// reached the controller partly is still a frame sent.
///
/// Every frame is `EB 03`, `DE`, the configured `E5`, `DF`, the configured `E1`,
/// or `E3`, all under the lock. Per connection, in order:
///
/// - With a mode configured, a `DE` directly after a probe that went out. Each
///   `E5` directly follows an answered `DE` that calls for it, and each `E5` that
///   went out is followed by a `DE` at least [`SETTLE`] later. At most
///   [`MODE_ATTEMPTS`] `E5`. `DF` follows a `DE` that needs no `E5`, or the one
///   after the last `E5`. Without a mode, no `DE` or `E5`, and `DF` directly
///   follows the probe.
/// - The first `E1` directly follows an answered `DF` that calls for it, and
///   each `E1` that went out is followed by a `DF` at least [`SETTLE`] later. A
///   second `E1` only if that `DF` differs from what was sent. At most
///   [`DASHBOARD_ATTEMPTS`].
/// - `E3` only once that is done.
/// - Then the check: no sooner than [`CHECK_PERIOD`] after the setup and after
///   the last check, a `DE` if a mode is configured and then a `DF`. If either
///   calls for a new setup, nothing but a probe follows.
fn assert_allowed_frames(trace: &[Sent], config: DaemonConfig) {
    let wanted = config.dashboard();
    let dashboard = display_frame(wanted.elements, wanted.interval_s);
    let needs_e1 = |panel: DisplayStatus| match config.elements {
        Some(_) => panel != wanted,
        None => panel.elements == Elements::NONE,
    };
    let needs_e5 = |raw_mode: u8| ModeStatus { raw_mode, on: true }.mode() != config.mode;
    let period = CHECK_PERIOD.as_millis() as u64;
    let settle = SETTLE.as_millis() as u64;
    let mut setup = Setup::default();
    let mut prev: Option<&Sent> = None;
    for sent in trace {
        if sent.opcode() != PROBE {
            assert!(!setup.stale, "after a failed check, only a probe: {sent:?}");
        }
        match sent.opcode() {
            PROBE => {
                assert_eq!(sent.frame[..], probe_frame()[..], "not EB 03");
                setup = Setup::default();
            }
            MODE_QUERY => {
                assert!(
                    config.mode.is_some(),
                    "DE with no mode configured: {sent:?}"
                );
                assert_eq!(sent.frame[..], mode_query_frame()[..], "not DE");
                if setup.configured_at.is_none() {
                    let first = after(prev, PROBE).is_some();
                    let verify = after(prev, SET_MODE).is_some_and(|e5| sent.at - e5.at >= settle);
                    assert!(
                        first || verify,
                        "DE neither after the probe nor a settle after E5: {sent:?}"
                    );
                } else {
                    assert!(sent.at >= setup.check_from, "a check too soon: {sent:?}");
                    if sent.ok {
                        setup.stale = needs_e5(sent.mode);
                        setup.mode_checked = !setup.stale;
                    }
                }
            }
            SET_MODE => {
                let mode = config.mode.expect("E5 with no mode configured");
                assert_eq!(sent.frame[..], mode_frame(mode)[..], "an unexpected E5");
                assert!(setup.configured_at.is_none(), "E5 after setup: {sent:?}");
                let read = after(prev, MODE_QUERY).expect("E5 without an answered DE before it");
                assert!(needs_e5(read.mode), "E5 for a panel in that mode: {sent:?}");
                setup.e5s += 1;
                assert!(setup.e5s <= MODE_ATTEMPTS, "too many E5: {sent:?}");
            }
            READ_BACK => {
                assert_eq!(sent.frame[..], display_query_frame()[..], "not DF");
                if setup.configured_at.is_none() {
                    let mode_settled = match config.mode {
                        None => after(prev, PROBE).is_some(),
                        Some(_) => after(prev, MODE_QUERY)
                            .is_some_and(|de| !needs_e5(de.mode) || setup.e5s == MODE_ATTEMPTS),
                    };
                    let verify = after(prev, DASHBOARD).is_some_and(|e1| sent.at - e1.at >= settle);
                    assert!(
                        mode_settled || verify,
                        "DF neither after a settled mode nor a settle after E1: {sent:?}"
                    );
                    let done = if verify {
                        sent.panel == wanted || setup.e1s == DASHBOARD_ATTEMPTS
                    } else {
                        !needs_e1(sent.panel)
                    };
                    if sent.ok && done {
                        setup.configured_at = Some(sent.at);
                        setup.check_from = sent.at + period;
                    }
                } else {
                    let due = match config.mode {
                        None => sent.at >= setup.check_from,
                        Some(_) => setup.mode_checked && after(prev, MODE_QUERY).is_some(),
                    };
                    assert!(due, "a check DF out of turn: {sent:?}");
                    setup.mode_checked = false;
                    if sent.ok {
                        setup.stale = needs_e1(sent.panel);
                        setup.check_from += period;
                    }
                }
            }
            DASHBOARD => {
                assert_eq!(sent.frame[..], dashboard[..], "an unexpected E1");
                assert!(setup.configured_at.is_none(), "E1 after setup: {sent:?}");
                let read = after(prev, READ_BACK).expect("E1 without an answered DF before it");
                let needed = if setup.e1s == 0 {
                    needs_e1(read.panel)
                } else {
                    read.panel != wanted
                };
                assert!(needed, "E1 for a panel that needs none: {sent:?}");
                setup.e1s += 1;
                assert!(setup.e1s <= DASHBOARD_ATTEMPTS, "too many E1: {sent:?}");
            }
            FEED => {
                assert_eq!(sent.frame.len(), config.feed.bytes());
                assert!(
                    setup.configured_at.is_some(),
                    "E3 before the panel was ready: {sent:?}"
                );
                assert!(!setup.mode_checked, "E3 inside a check: {sent:?}");
            }
            SAVE => panic!("AA must never be sent: {sent:?}"),
            other => panic!("the daemon must not send {other:02X}: {sent:?}"),
        }
        assert!(sent.locked, "written without the lock: {sent:?}");
        prev = Some(sent);
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
fn a_panel_with_no_widgets_gets_probe_then_read_back_then_dashboard_then_feed() {
    let rig = Rig::new();
    rig.panel_has(0, 0);

    rig.ticks(3).unwrap();

    let trace = rig.trace();
    assert_eq!(
        rig.opcodes(),
        [PROBE, READ_BACK, DASHBOARD, READ_BACK, FEED, FEED, FEED]
    );
    // The E1 is read back after it has settled, and the feed starts then.
    assert_eq!(rig.attempts(READ_BACK), [0, 1000]);
    assert_eq!(rig.delivered(FEED), [1000, 2000, 3000]);
    assert_eq!(
        trace[2].frame[..],
        display_frame(Elements::from_bits(0xAF), 3)[..]
    );
    assert_eq!(rig.device.panel.get(), (0xAF, 3));
    assert_eq!(
        trace[4].frame[..],
        sensor_frame(&SAMPLE, FeedFrameLen::Full)[..]
    );
    assert_only_allowed_frames(&trace);
}

fn naming(mask: u8, interval_s: u8, ticks: u64) -> DaemonConfig {
    DaemonConfig {
        elements: Some(Elements::from_bits(mask)),
        interval_s,
        max_ticks: Some(ticks),
        ..steady()
    }
}

fn healthy(_: Option<u32>) -> Result<FakeSensors> {
    Ok(healthy_sensors())
}

#[test]
fn a_panel_that_matches_the_named_widgets_gets_no_e1() {
    let rig = Rig::new();
    rig.panel_has(0x05, 7);
    let config = naming(0x05, 7, 2);

    rig.run_configured(config, healthy).unwrap();

    assert_eq!(rig.opcodes(), [PROBE, READ_BACK, FEED, FEED]);
    assert_allowed_frames(&rig.trace(), config);
}

#[test]
fn a_panel_with_other_widgets_gets_one_e1_with_the_named_ones() {
    let rig = Rig::new();
    rig.panel_has(0xFF, 7);
    let config = naming(0x05, 7, 2);

    rig.run_configured(config, healthy).unwrap();

    assert_eq!(
        rig.opcodes(),
        [PROBE, READ_BACK, DASHBOARD, READ_BACK, FEED, FEED]
    );
    assert_eq!(rig.device.panel.get(), (0x05, 7));
    assert_allowed_frames(&rig.trace(), config);
}

#[test]
fn a_panel_with_another_interval_gets_one_e1_with_the_named_one() {
    let rig = Rig::new();
    rig.panel_has(0x05, 4);
    let config = naming(0x05, 7, 2);

    rig.run_configured(config, healthy).unwrap();

    assert_eq!(
        rig.opcodes(),
        [PROBE, READ_BACK, DASHBOARD, READ_BACK, FEED, FEED]
    );
    assert_eq!(rig.device.panel.get(), (0x05, 7));
    assert_allowed_frames(&rig.trace(), config);
}

// Seen on the real panel: all eight widgets every 4 s, with no E1 from this tool.
// Its selection is neither the defaults nor at the default interval.
#[test]
fn with_no_named_widgets_a_panel_with_widgets_is_left_alone() {
    let rig = Rig::new();
    rig.panel_has(0xFF, 4);

    rig.ticks(3).unwrap();

    assert_eq!(rig.opcodes(), [PROBE, READ_BACK, FEED, FEED, FEED]);
    assert_eq!(rig.device.panel.get(), (0xFF, 4));
    assert_only_allowed_frames(&rig.trace());
}

/// A read-back that fails until 7 seconds in sends no `E1` until then, and costs
/// the normal backoff.
fn assert_backs_off_without_e1(rig: &Rig) {
    rig.panel_has(0, 0);

    rig.ticks(1).unwrap();

    assert_eq!(rig.attempts(PROBE), [0, 2000, 6000, 14_000]);
    assert_eq!(rig.attempts(READ_BACK), [0, 2000, 6000, 14_000, 15_000]);
    assert_eq!(rig.attempts(DASHBOARD), [14_000]);
    assert_eq!(rig.delivered(FEED), [15_000]);
    assert_only_allowed_frames(&rig.trace());
}

#[test]
fn an_i2c_error_on_the_read_back_sends_no_e1_and_backs_off() {
    let rig = Rig::new();
    rig.read_fault(0, 7000, READ_BACK, eio);

    assert_backs_off_without_e1(&rig);
}

#[test]
fn a_short_read_back_sends_no_e1_and_backs_off() {
    let rig = Rig::new();
    rig.read_fault(0, 7000, READ_BACK, short_read);

    assert_backs_off_without_e1(&rig);
}

#[test]
fn a_failed_df_query_sends_no_e1_and_backs_off() {
    let rig = Rig::new();
    rig.fault(0, 7000, Some(READ_BACK), eio);

    assert_backs_off_without_e1(&rig);
}

// The probe passing is no reason to retry at once: only a good E3 resets the
// backoff, or this would hammer the panel with probes for as long as it lasts.
#[test]
fn a_panel_that_answers_the_probe_but_not_the_read_back_backs_off_to_the_cap() {
    let rig = Rig::new();
    rig.read_fault(0, TIME_LIMIT_MS, READ_BACK, short_read);
    rig.stop_at(200_000);

    rig.run(None, |_| Ok(healthy_sensors())).unwrap();

    assert_eq!(
        rig.attempts(PROBE),
        [0, 2000, 6000, 14_000, 30_000, 62_000, 122_000, 182_000]
    );
    assert_eq!(rig.attempts(DASHBOARD), []);
    assert_eq!(rig.attempts(FEED), []);
}

#[test]
fn a_permission_error_on_the_read_back_is_fatal() {
    let rig = Rig::new();
    rig.read_fault(0, TIME_LIMIT_MS, READ_BACK, || errno_error(Errno::ACCESS));

    let error = rig.run(None, no_sensors).unwrap_err();

    assert!(matches!(error, Error::I2c { .. }), "{error:?}");
    assert_eq!(rig.attempts(DASHBOARD), []);
}

#[test]
fn a_reconnect_reads_back_again_and_a_panel_that_kept_its_widgets_gets_no_e1() {
    let rig = Rig::new();
    rig.panel_has(0, 0);
    rig.outage(1500, 4500);

    rig.ticks(5).unwrap();

    assert_eq!(rig.attempts(READ_BACK), [0, 1000, 6000]);
    assert_eq!(rig.attempts(DASHBOARD), [0]);
    assert_only_allowed_frames(&rig.trace());
}

#[test]
fn a_reconnect_reads_back_again_and_a_panel_that_lost_its_widgets_gets_e1() {
    let rig = Rig::new();
    rig.panel_has(0xFF, 4);
    rig.outage(500, 3500);
    let device = rig.device.clone();
    rig.clock.at(1500, move || device.panel.set((0, 0)));

    rig.ticks(5).unwrap();

    assert_eq!(rig.attempts(READ_BACK), [0, 5000, 6000]);
    assert_eq!(rig.attempts(DASHBOARD), [5000]);
    assert_only_allowed_frames(&rig.trace());
}

#[test]
fn the_feed_length_comes_from_the_config() {
    let rig = Rig::new();
    let config = DaemonConfig {
        feed: FeedFrameLen::Truncated,
        max_ticks: Some(1),
        ..steady()
    };

    run_with(
        config,
        Environment {
            connect: || rig.connect(),
            open_sensors: healthy,
            clock: rig.clock.clone(),
            stop: rig.stop.clone(),
            dry_run: false,
        },
    )
    .unwrap();

    assert_eq!(rig.feed_frames()[0].len(), 128);
}

/// Mode 0 for `ticks` ticks.
fn in_mode_0(ticks: Option<u64>) -> DaemonConfig {
    DaemonConfig {
        mode: Some(Mode::Faith1),
        max_ticks: ticks,
        ..steady()
    }
}

#[test]
fn without_a_mode_the_mode_is_neither_read_nor_set() {
    let rig = Rig::new();
    rig.panel_has(0xFF, 4);
    rig.panel_in_mode(RAW_PRESET_ROTATION);

    rig.ticks(2).unwrap();

    assert_eq!(rig.opcodes(), [PROBE, READ_BACK, FEED, FEED]);
    assert_eq!(rig.device.mode.get(), RAW_PRESET_ROTATION);
    assert_only_allowed_frames(&rig.trace());
}

#[test]
fn a_panel_already_in_the_mode_gets_no_e5() {
    let rig = Rig::new();
    rig.panel_has(0xFF, 4);
    rig.panel_in_mode(RAW_STATS);
    let config = in_mode_0(Some(2));

    rig.run_configured(config, healthy).unwrap();

    assert_eq!(rig.opcodes(), [PROBE, MODE_QUERY, READ_BACK, FEED, FEED]);
    assert_allowed_frames(&rig.trace(), config);
}

// Seen on the real panel after a power loss: mode 8, and no widgets every 0 s.
#[test]
fn a_panel_in_another_mode_gets_one_e5_before_df_and_e1() {
    let rig = Rig::new();
    rig.panel_has(0x00, 0);
    rig.panel_in_mode(RAW_PRESET_ROTATION);
    let config = in_mode_0(Some(2));

    rig.run_configured(config, healthy).unwrap();

    assert_eq!(
        rig.opcodes(),
        [PROBE, MODE_QUERY, SET_MODE, MODE_QUERY, READ_BACK, DASHBOARD, READ_BACK, FEED, FEED]
    );
    assert_eq!(rig.trace()[2].frame[..], mode_frame(Mode::Faith1)[..]);
    assert_eq!(rig.attempts(MODE_QUERY), [0, 1000]);
    assert_eq!(rig.attempts(READ_BACK), [1000, 2000]);
    assert_eq!(rig.delivered(FEED), [2000, 3000]);
    assert_eq!(rig.device.mode.get(), RAW_STATS);
    assert_eq!(rig.device.panel.get(), (0xAF, 3));
    assert_allowed_frames(&rig.trace(), config);
}

#[test]
fn the_preset_rotation_can_be_the_mode_asked_for() {
    let rig = Rig::new();
    rig.panel_has(0xFF, 4);
    rig.panel_in_mode(RAW_STATS);
    let config = DaemonConfig {
        mode: Some(Mode::PresetRotation),
        ..in_mode_0(Some(1))
    };

    rig.run_configured(config, healthy).unwrap();

    assert_eq!(rig.attempts(SET_MODE), [0]);
    assert_eq!(rig.device.mode.get(), RAW_PRESET_ROTATION);
    assert_allowed_frames(&rig.trace(), config);
}

/// A mode read that fails until 7 seconds in sends nothing past it until then,
/// and costs the normal backoff.
fn assert_backs_off_without_e5(rig: &Rig) {
    let config = in_mode_0(Some(1));

    rig.run_configured(config, healthy).unwrap();

    assert_eq!(rig.attempts(PROBE), [0, 2000, 6000, 14_000]);
    assert_eq!(rig.attempts(MODE_QUERY), [0, 2000, 6000, 14_000, 15_000]);
    assert_eq!(rig.attempts(SET_MODE), [14_000]);
    assert_eq!(rig.attempts(READ_BACK), [15_000]);
    assert_eq!(rig.delivered(FEED), [15_000]);
    assert_allowed_frames(&rig.trace(), config);
}

/// A panel with widgets, in mode 8.
fn after_power_loss_with_widgets() -> Rig {
    let rig = Rig::new();
    rig.panel_has(0xFF, 4);
    rig.panel_in_mode(RAW_PRESET_ROTATION);
    rig
}

#[test]
fn an_i2c_error_on_the_mode_read_sends_nothing_more_and_backs_off() {
    let rig = after_power_loss_with_widgets();
    rig.read_fault(0, 7000, MODE_QUERY, eio);

    assert_backs_off_without_e5(&rig);
}

#[test]
fn a_short_mode_read_sends_nothing_more_and_backs_off() {
    let rig = after_power_loss_with_widgets();
    rig.read_fault(0, 7000, MODE_QUERY, short_read);

    assert_backs_off_without_e5(&rig);
}

#[test]
fn a_failed_de_query_sends_nothing_more_and_backs_off() {
    let rig = after_power_loss_with_widgets();
    rig.fault(0, 7000, Some(MODE_QUERY), eio);

    assert_backs_off_without_e5(&rig);
}

#[test]
fn a_failed_e5_fails_the_connection_before_df() {
    let rig = after_power_loss_with_widgets();
    rig.fault(0, 1000, Some(SET_MODE), eio);
    let config = in_mode_0(Some(1));

    rig.run_configured(config, healthy).unwrap();

    assert_eq!(rig.attempts(SET_MODE), [0, 2000]);
    assert_eq!(rig.attempts(READ_BACK), [3000]);
    assert_eq!(rig.delivered(FEED), [3000]);
    assert_allowed_frames(&rig.trace(), config);
}

#[test]
fn a_permission_error_on_the_mode_read_is_fatal() {
    let rig = Rig::new();
    rig.read_fault(0, TIME_LIMIT_MS, MODE_QUERY, || errno_error(Errno::ACCESS));

    let error = rig.run_configured(in_mode_0(None), no_sensors).unwrap_err();

    assert!(matches!(error, Error::I2c { .. }), "{error:?}");
    assert_eq!(rig.attempts(SET_MODE), []);
    assert_eq!(rig.attempts(READ_BACK), []);
}

#[test]
fn a_reconnect_reads_the_mode_again_and_sets_it_only_if_it_changed() {
    let rig = Rig::new();
    rig.panel_has(0xFF, 4);
    rig.panel_in_mode(RAW_STATS);
    rig.outage(500, 3500);
    rig.outage(5500, 8500);
    // A manual `mode 5` while the second connection runs.
    let device = rig.device.clone();
    rig.clock.at(5200, move || device.mode.set(0x06));
    let config = in_mode_0(Some(9));

    rig.run_configured(config, healthy).unwrap();

    assert_eq!(rig.attempts(MODE_QUERY), [0, 5000, 10_000, 11_000]);
    assert_eq!(rig.attempts(SET_MODE), [10_000]);
    assert_eq!(rig.device.mode.get(), RAW_STATS);
    assert_allowed_frames(&rig.trace(), config);
}

#[test]
fn shutdown_during_the_mode_read_does_not_go_on_to_e5() {
    let rig = after_power_loss_with_widgets();
    rig.device.write_cost.set(ms(10));
    rig.stop_at(15);

    rig.run_configured(in_mode_0(None), healthy).unwrap();

    assert_eq!(rig.opcodes(), [PROBE, MODE_QUERY]);
}

#[test]
fn no_forbidden_frame_when_the_mode_is_set_on_each_connection() {
    let rig = after_power_loss_with_widgets();
    rig.read_fault(49_500, 50_500, MODE_QUERY, short_read);
    // Lost again while the feed is failing, so a later connection needs E5 too.
    let device = rig.device.clone();
    rig.clock
        .at(22_000, move || device.mode.set(RAW_PRESET_ROTATION));

    run_through_everything(&rig, in_mode_0(None));

    let trace = rig.trace();
    assert_eq!(rig.delivered(SET_MODE).len(), 2, "{trace:?}");
    let (asked, answered) = (rig.attempts(MODE_QUERY), rig.delivered(MODE_QUERY));
    assert!(
        answered.len() < asked.len(),
        "the failed DE was not hit: {asked:?}"
    );
    assert_eq!(rig.attempts(DASHBOARD), []);
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
fn reconnect_backs_off_sends_e1_twice_per_connection_and_resets_after_a_good_write() {
    let rig = Rig::new();
    rig.panel_has(0, 0);
    rig.device.forgets_e1.set(true);
    rig.outage(2500, 20_000);
    rig.outage(40_500, 43_500);

    rig.ticks(14).unwrap();

    // Each connection: two E1 a settle apart, then a DF a settle later, and the
    // feed. Three failed ticks, then waits of 2, 4, 8 and 16 seconds. The second
    // outage starts over at 2 seconds.
    assert_eq!(
        rig.attempts(PROBE),
        [0, 7000, 11_000, 19_000, 35_000, 45_000]
    );
    assert_eq!(
        rig.attempts(DASHBOARD),
        [0, 1000, 35_000, 36_000, 45_000, 46_000]
    );
    assert_eq!(
        rig.delivered(FEED),
        [2000, 37_000, 38_000, 39_000, 40_000, 47_000, 48_000, 49_000]
    );
    assert_only_allowed_frames(&rig.trace());
}

#[test]
fn a_controller_that_stops_answering_is_retried_through_the_probe() {
    let rig = Rig::new();
    rig.device.no_answer_until.set(20_000);

    rig.ticks(2).unwrap();

    assert_eq!(rig.attempts(PROBE), [0, 2000, 6000, 14_000, 30_000]);
    assert_eq!(rig.attempts(READ_BACK), [30_000]);
}

#[test]
fn a_short_read_counts_as_a_bus_error() {
    let rig = Rig::new();
    rig.fault(2500, 5500, Some(FEED), short_read);

    rig.ticks(7).unwrap();

    assert_eq!(rig.attempts(PROBE), [0, 7000]);
    assert_eq!(rig.attempts(READ_BACK), [0, 7000]);
}

#[test]
fn a_failed_e1_is_not_repeated_on_the_same_connection() {
    let rig = Rig::new();
    rig.panel_has(0, 0);
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
    rig.panel_has(0, 0);
    rig.device.forgets_e1.set(true);
    rig.fault(500, TIME_LIMIT_MS, Some(FEED), eio);

    rig.ticks(15).unwrap();

    // Each connection takes two settles, then three failed ticks.
    assert_eq!(rig.attempts(PROBE), [0, 6000, 14_000, 26_000, 46_000]);
    assert_eq!(rig.delivered(DASHBOARD).len(), 10);
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
    assert_eq!(rig.attempts(READ_BACK), [182_000]);
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

    assert_eq!(rig.opcodes(), [PROBE]);
}

#[test]
fn shutdown_during_the_read_back_does_not_go_on_to_e1() {
    let rig = Rig::new();
    rig.device.write_cost.set(ms(10));
    rig.stop_at(15);

    rig.run(None, |_| Ok(healthy_sensors())).unwrap();

    assert_eq!(rig.opcodes(), [PROBE, READ_BACK]);
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
    rig.panel_has(0xFF, 4);
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
// only ever sees EB 03, DF, at most one E1 per connection, and E3.
fn run_through_everything(rig: &Rig, config: DaemonConfig) {
    run_through_everything_at(rig, config, SAMPLE, 4, 100);
}

/// [`run_through_everything`] on sensors reading `sample`. A sparse feed writes
/// less, so it meets fewer of the faults and needs other bounds on the outcome.
fn run_through_everything_at(
    rig: &Rig,
    config: DaemonConfig,
    sample: SensorSample,
    min_connections: usize,
    min_frames: usize,
) {
    rig.outage(3500, 9000);
    rig.fault(20_500, 30_500, Some(FEED), eio);
    rig.read_fault(30_000, 32_000, READ_BACK, short_read);
    rig.fault(39_000, 39_100, Some(DASHBOARD), eio);
    rig.fault(45_500, 48_500, Some(FEED), short_read);
    rig.upload(60_500, 66_500);
    rig.outage(70_500, 75_500);
    rig.device.no_answer_until.set(5000);
    let clock = rig.clock.clone();
    rig.clock.at(90_500, move || clock.advance(ms(600_000)));
    rig.stop_at(800_000);
    let opens = Rc::new(Cell::new(0));

    rig.run_configured(config, move |_| {
        opens.set(opens.get() + 1);
        if opens.get() % 3 == 0 {
            Ok(FakeSensors {
                sample,
                ..FakeSensors::default()
            })
        } else {
            Ok(broken_sensors())
        }
    })
    .unwrap();

    let trace = rig.trace();
    assert_allowed_frames(&trace, config);
    let connections = rig.delivered(READ_BACK).len();
    assert!(
        connections >= min_connections,
        "only {connections} connections: {trace:?}"
    );
    let feed = rig.delivered(FEED);
    assert!(feed.len() > min_frames, "only {} frames", feed.len());
    assert!(
        feed.windows(2).all(|pair| pair[1] - pair[0] >= 1000),
        "frames closer than a tick: {feed:?}"
    );
}

#[test]
fn no_forbidden_frame_when_the_panel_keeps_its_widgets() {
    let rig = Rig::new();
    rig.panel_has(0xFF, 4);

    run_through_everything(&rig, steady());

    assert_eq!(rig.attempts(DASHBOARD), []);
}

#[test]
fn no_forbidden_frame_when_the_panel_matches_the_named_widgets() {
    let rig = Rig::new();
    rig.panel_has(0x05, 7);
    let config = DaemonConfig {
        max_ticks: None,
        ..naming(0x05, 7, 0)
    };

    run_through_everything(&rig, config);

    assert_eq!(rig.attempts(DASHBOARD), []);
}

#[test]
fn no_forbidden_frame_when_e1_is_needed_once() {
    let rig = Rig::new();
    rig.panel_has(0, 0);

    run_through_everything(&rig, steady());

    assert_eq!(rig.delivered(DASHBOARD).len(), 1);
}

#[test]
fn no_forbidden_frame_when_each_connection_needs_e1() {
    let rig = Rig::new();
    rig.panel_has(0, 0);
    rig.device.forgets_e1.set(true);
    // The second E1 of one connection.
    rig.fault(33_900, 34_100, Some(DASHBOARD), eio);

    run_through_everything(&rig, steady());

    assert_each_connection_tried_e1_twice(&rig);
    assert!(
        rig.delivered(DASHBOARD).len() < rig.attempts(DASHBOARD).len(),
        "the failed E1 was not hit: {:?}",
        rig.attempts(DASHBOARD)
    );
}

/// For a panel that forgets `E1`: every connection that got as far as feeding
/// sent two, a settle apart.
fn assert_each_connection_tried_e1_twice(rig: &Rig) {
    let trace = rig.trace();
    let connections = trace.split(|sent| sent.opcode() == PROBE);
    let fed = connections.filter(|frames| frames.iter().any(|sent| sent.opcode() == FEED));
    let mut count = 0;
    for frames in fed {
        let e1: Vec<u64> = frames
            .iter()
            .filter(|sent| sent.opcode() == DASHBOARD)
            .map(|sent| sent.at)
            .collect();
        assert_eq!(e1.len(), 2, "{frames:?}");
        assert!(e1[1] - e1[0] >= 1000, "{e1:?}");
        count += 1;
    }
    assert!(count >= 2, "{trace:?}");
}

const BOTH_LENGTHS: [FeedFrameLen; 2] = [FeedFrameLen::Full, FeedFrameLen::Truncated];

/// Serves one sample per poll, and the last one from then on. Clones share the
/// list, so a reopened source carries on where the last one stopped.
#[derive(Clone)]
struct Polls(Rc<RefCell<VecDeque<SensorSample>>>);

impl Polls {
    fn of(samples: &[SensorSample]) -> Self {
        Self(Rc::new(RefCell::new(samples.iter().copied().collect())))
    }
}

impl SensorSource for Polls {
    fn read(&mut self) -> Result<SensorSample> {
        let mut samples = self.0.borrow_mut();
        let sample = samples[0];
        if samples.len() > 1 {
            samples.pop_front();
        }
        Ok(sample)
    }
}

/// [`SAMPLE`] at `usage` percent.
fn at_usage(usage: u32) -> SensorSample {
    SensorSample {
        gpu_usage_percent: usage,
        ..SAMPLE
    }
}

fn adaptive(ticks: Option<u64>, feed: FeedFrameLen) -> DaemonConfig {
    DaemonConfig {
        feed,
        strategy: FeedStrategy::Adaptive,
        max_ticks: ticks,
        ..DaemonConfig::default()
    }
}

/// Runs `config` on sensors reading `samples`, one per poll.
fn run_on(rig: &Rig, config: DaemonConfig, samples: &[SensorSample]) {
    let polls = Polls::of(samples);
    rig.run_configured(config, |_| Ok(polls.clone())).unwrap();
}

/// Adaptive at 256 bytes, always under load, for `ticks` polls.
fn run_under_load(rig: &Rig, ticks: u64) {
    let config = adaptive(Some(ticks), FeedFrameLen::Full);
    run_on(rig, config, &[at_usage(90)]);
}

fn frames_of(samples: &[SensorSample], feed: FeedFrameLen) -> Vec<Vec<u8>> {
    let frame = |sample| sensor_frame(sample, feed);
    samples.iter().map(frame).collect()
}

#[test]
fn the_adaptive_constants_are_the_ones_upstream_measured_with() {
    assert_eq!(ADAPTIVE_BUSY_PERCENT, 50);
    assert_eq!(ADAPTIVE_WINDOW_POLLS, 5);
    assert_eq!(ADAPTIVE_PAIR_GAP_TICKS, 1);
}

#[test]
fn the_default_config_feeds_adaptively_at_full_length() {
    let run_default_at = |usage| {
        let rig = Rig::new();
        let config = DaemonConfig {
            max_ticks: Some(6),
            ..DaemonConfig::default()
        };
        run_on(&rig, config, &[at_usage(usage)]);
        assert!(rig.feed_frames().iter().all(|frame| frame.len() == 256));
        rig.delivered(FEED)
    };

    assert_eq!(run_default_at(49), [0, 1000, 2000, 3000, 4000, 5000]);
    assert_eq!(run_default_at(50), [4000, 5000]);
}

#[test]
fn steady_sends_every_tick_at_either_length_whatever_the_load() {
    for feed in BOTH_LENGTHS {
        let rig = Rig::new();
        let samples = [at_usage(100), at_usage(0), at_usage(50), at_usage(99)];
        let config = DaemonConfig {
            feed,
            max_ticks: Some(4),
            ..steady()
        };

        run_on(&rig, config, &samples);

        assert_eq!(rig.delivered(FEED), [0, 1000, 2000, 3000]);
        assert_eq!(rig.feed_frames(), frames_of(&samples, feed));
        assert_allowed_frames(&rig.trace(), config);
    }
}

#[test]
fn adaptive_under_the_threshold_sends_like_steady_at_either_length() {
    for feed in BOTH_LENGTHS {
        let rig = Rig::new();
        let samples = [at_usage(0), at_usage(49), at_usage(12), at_usage(49)];
        let config = adaptive(Some(4), feed);

        run_on(&rig, config, &samples);

        assert_eq!(rig.delivered(FEED), [0, 1000, 2000, 3000]);
        assert_eq!(rig.feed_frames(), frames_of(&samples, feed));
        assert_allowed_frames(&rig.trace(), config);
    }
}

#[test]
fn adaptive_at_the_threshold_sends_each_summary_twice_a_second_apart_at_either_length() {
    for feed in BOTH_LENGTHS {
        let rig = Rig::new();
        let config = adaptive(Some(18), feed);

        run_on(&rig, config, &[at_usage(50)]);

        assert_eq!(
            rig.delivered(FEED),
            [4000, 5000, 10_000, 11_000, 16_000, 17_000]
        );
        assert_eq!(rig.feed_frames(), frames_of(&[at_usage(50); 6], feed));
        assert_allowed_frames(&rig.trace(), config);
    }
}

#[test]
fn each_summary_covers_its_own_window() {
    let poll = |temp, clock, usage, fan, ram_clock, ram, power| SensorSample {
        gpu_temp_c: temp,
        gpu_clock_mhz: clock,
        gpu_usage_percent: usage,
        fan_speed: fan,
        ram_clock_mhz: ram_clock,
        ram_usage_percent: ram,
        fps: 77,
        tgp_watts: power,
    };
    let polls = [
        // The first window.
        poll(60, 2000, 60, 1000, 14_000, 30, 300),
        poll(70, 2100, 70, 1400, 14_000, 31, 500),
        poll(65, 2200, 80, 1200, 14_100, 32, 400),
        poll(64, 2300, 90, 1100, 14_100, 33, 350),
        poll(63, 2400, 100, 1050, 14_300, 34, 320),
        // Between the frames of the pair, so in no window.
        poll(99, 9999, 100, 9999, 9999, 99, 999),
        // The second window.
        poll(50, 1000, 55, 800, 10_000, 40, 200),
        poll(51, 1000, 55, 800, 10_000, 41, 200),
        poll(52, 1000, 55, 800, 10_000, 42, 200),
        poll(53, 1000, 55, 800, 10_000, 43, 200),
        poll(54, 1001, 56, 801, 10_000, 44, 201),
    ];
    let first = SensorSample {
        gpu_temp_c: 70,
        gpu_clock_mhz: 2200,
        gpu_usage_percent: 80,
        fan_speed: 1400,
        ram_clock_mhz: 14_100,
        ram_usage_percent: 34,
        fps: 0,
        tgp_watts: 500,
    };
    let second = SensorSample {
        gpu_temp_c: 54,
        gpu_clock_mhz: 1000,
        gpu_usage_percent: 55,
        fan_speed: 801,
        ram_clock_mhz: 10_000,
        ram_usage_percent: 44,
        fps: 0,
        tgp_watts: 201,
    };
    let rig = Rig::new();

    run_on(&rig, adaptive(Some(12), FeedFrameLen::Full), &polls);

    assert_eq!(rig.delivered(FEED), [4000, 5000, 10_000, 11_000]);
    let expected = [first, first, second, second];
    assert_eq!(rig.feed_frames(), frames_of(&expected, FeedFrameLen::Full));
}

#[test]
fn falling_under_the_threshold_in_a_window_drops_it_and_sends_the_live_sample_at_once() {
    let rig = Rig::new();
    let mut polls = vec![at_usage(60), at_usage(70), at_usage(30)];
    polls.extend([at_usage(80); 7]);

    run_on(&rig, adaptive(Some(10), FeedFrameLen::Full), &polls);

    // Nothing for the two polls dropped. The next window is five whole polls.
    assert_eq!(rig.delivered(FEED), [2000, 7000, 8000]);
    let expected = [at_usage(30), at_usage(80), at_usage(80)];
    assert_eq!(rig.feed_frames(), frames_of(&expected, FeedFrameLen::Full));
}

#[test]
fn falling_under_the_threshold_in_a_pair_replaces_the_second_frame_with_the_live_sample() {
    let rig = Rig::new();
    let polls = [[at_usage(80); 5].as_slice(), &[at_usage(30)]].concat();

    run_on(&rig, adaptive(Some(8), FeedFrameLen::Full), &polls);

    assert_eq!(
        rig.delivered(FEED),
        [4000, 5000, 6000, 7000],
        "per-second frames resume at once"
    );
    let expected = [at_usage(80), at_usage(30), at_usage(30), at_usage(30)];
    assert_eq!(rig.feed_frames(), frames_of(&expected, FeedFrameLen::Full));
}

#[test]
fn rising_over_the_threshold_starts_a_whole_window() {
    let rig = Rig::new();
    let polls = [[at_usage(20); 3].as_slice(), &[at_usage(80)]].concat();

    run_on(&rig, adaptive(Some(9), FeedFrameLen::Full), &polls);

    assert_eq!(rig.delivered(FEED), [0, 1000, 2000, 7000, 8000]);
}

// A zero reads as a failure, and the feed must show one rather than a summary
// of what came before.
#[test]
fn sensors_failing_under_load_show_zeros_at_once() {
    let rig = Rig::new();
    let opens = Cell::new(0);
    let reads = Rc::new(Cell::new(0));
    let open = |_: Option<u32>| {
        opens.set(opens.get() + 1);
        if opens.get() > 1 {
            return Err(Error::Adapter("gone".into()));
        }
        let reads = reads.clone();
        Ok(ScriptedSensors {
            read: move || {
                reads.set(reads.get() + 1);
                if reads.get() < 3 {
                    Ok(at_usage(90))
                } else {
                    broken_sensors().read()
                }
            },
        })
    };

    rig.run_configured(adaptive(Some(4), FeedFrameLen::Full), open)
        .unwrap();

    assert_eq!(rig.delivered(FEED), [2000, 3000]);
    let zeros = [SensorSample::default(); 2];
    assert_eq!(rig.feed_frames(), frames_of(&zeros, FeedFrameLen::Full));
}

#[test]
fn a_busy_lock_on_the_first_frame_of_a_pair_moves_nothing() {
    let rig = Rig::new();
    rig.upload(3500, 4500);

    run_under_load(&rig, 12);

    assert_eq!(rig.delivered(FEED), [5000, 10_000, 11_000]);
    assert_eq!(rig.attempts(PROBE), [0], "skipping must not reconnect");
    assert_only_allowed_frames(&rig.trace());
}

#[test]
fn a_busy_lock_on_the_second_frame_of_a_pair_moves_nothing() {
    let rig = Rig::new();
    rig.upload(4500, 5500);

    run_under_load(&rig, 12);

    assert_eq!(rig.delivered(FEED), [4000, 10_000, 11_000]);
    assert_eq!(rig.attempts(PROBE), [0], "skipping must not reconnect");
}

#[test]
fn an_upload_across_whole_windows_ends_in_the_next_pair_and_not_a_burst() {
    let rig = Rig::new();
    rig.upload(500, 30_500);

    run_under_load(&rig, 36);

    assert_eq!(rig.delivered(FEED), [34_000, 35_000]);
}

#[test]
fn a_failed_first_frame_of_a_pair_is_not_sent_again() {
    let rig = Rig::new();
    rig.fault(3500, 4500, Some(FEED), eio);

    run_under_load(&rig, 12);

    assert_eq!(rig.attempts(FEED), [4000, 5000, 10_000, 11_000]);
    assert_eq!(rig.delivered(FEED), [5000, 10_000, 11_000]);
    assert_eq!(rig.attempts(PROBE), [0]);
    assert_only_allowed_frames(&rig.trace());
}

#[test]
fn a_failed_second_frame_of_a_pair_is_not_sent_again() {
    let rig = Rig::new();
    rig.fault(4500, 5500, Some(FEED), eio);

    run_under_load(&rig, 12);

    assert_eq!(rig.attempts(FEED), [4000, 5000, 10_000, 11_000]);
    assert_eq!(rig.delivered(FEED), [4000, 10_000, 11_000]);
    assert_eq!(rig.attempts(PROBE), [0]);
    assert_only_allowed_frames(&rig.trace());
}

#[test]
fn failures_count_across_the_silent_ticks_and_a_new_session_starts_a_new_window() {
    let rig = Rig::new();
    rig.fault(3500, 10_500, Some(FEED), eio);
    assert_eq!(WRITE_ATTEMPTS, 3);

    run_under_load(&rig, 17);

    assert_eq!(rig.attempts(FEED), [4000, 5000, 10_000, 16_000, 17_000]);
    assert_eq!(rig.delivered(FEED), [16_000, 17_000]);
    assert_eq!(rig.attempts(PROBE), [0, 12_000]);
    assert_only_allowed_frames(&rig.trace());
}

#[test]
fn a_good_write_clears_the_count_of_failures_in_a_pair() {
    let rig = Rig::new();
    rig.fault(3500, 4500, Some(FEED), eio);
    rig.fault(9500, 10_500, Some(FEED), eio);
    rig.fault(15_500, 16_500, Some(FEED), eio);

    run_under_load(&rig, 18);

    assert_eq!(rig.delivered(FEED), [5000, 11_000, 17_000]);
    assert_eq!(rig.attempts(PROBE), [0]);
}

#[test]
fn a_silent_tick_touches_no_lock_and_the_bus_stays_free_for_an_upload() {
    let rig = Rig::new();
    let path = rig.lock_path();
    let lock_free = move || drop(BusLock::try_acquire(&path).unwrap());
    *rig.clock.on_sleep.borrow_mut() = Some(Box::new(lock_free));

    run_under_load(&rig, 12);

    assert_eq!(rig.delivered(FEED), [4000, 5000, 10_000, 11_000]);
    assert_only_allowed_frames(&rig.trace());
}

#[test]
fn work_does_not_push_the_adaptive_cadence() {
    let rig = Rig::new();
    rig.device.write_cost.set(ms(20));
    let clock = rig.clock.clone();
    let slow_sensors = move |_: Option<u32>| {
        let clock = clock.clone();
        Ok(ScriptedSensors {
            read: move || {
                clock.advance(ms(30));
                Ok(at_usage(90))
            },
        })
    };
    let config = adaptive(Some(600), FeedFrameLen::Full);

    rig.run_configured(config, slow_sensors).unwrap();

    let feed = rig.delivered(FEED);
    let start = feed[0];
    let cycle = (0..100).flat_map(|cycle| [cycle * 6000, cycle * 6000 + 1000]);
    let expected: Vec<u64> = cycle.map(|at| start + at).collect();
    assert_eq!(feed, expected);
}

#[test]
fn a_jump_in_time_in_a_window_gives_no_catch_up() {
    let rig = Rig::new();
    let clock = rig.clock.clone();
    rig.clock.at(2500, move || clock.advance(ms(60_000)));

    run_under_load(&rig, 6);

    assert_eq!(rig.delivered(FEED), [63_500, 64_500]);
}

#[test]
fn shutdown_in_a_pair_is_prompt_and_sends_nothing() {
    let rig = Rig::new();
    rig.stop_at(4500);

    run_on(&rig, adaptive(None, FeedFrameLen::Full), &[at_usage(90)]);

    rig.assert_stopped_promptly(4500);
    assert_eq!(rig.delivered(FEED), [4000]);
}

#[test]
fn no_forbidden_frame_in_adaptive_under_load_at_either_length() {
    for feed in BOTH_LENGTHS {
        let rig = Rig::new();
        rig.panel_has(0xFF, 4);

        run_through_everything_at(&rig, adaptive(None, feed), at_usage(90), 2, 30);

        assert_eq!(rig.attempts(DASHBOARD), []);
    }
}

#[test]
fn no_forbidden_frame_in_adaptive_under_the_threshold_at_either_length() {
    for feed in BOTH_LENGTHS {
        let rig = Rig::new();
        rig.panel_has(0, 0);

        run_through_everything_at(&rig, adaptive(None, feed), at_usage(20), 4, 100);

        assert_eq!(rig.delivered(DASHBOARD).len(), 1);
    }
}

// The load swings across the threshold every few polls, so that every state of
// the schedule meets the outages.
#[test]
fn no_forbidden_frame_in_adaptive_with_a_swinging_load_and_e1_on_each_connection() {
    let rig = Rig::new();
    rig.panel_has(0, 0);
    rig.device.forgets_e1.set(true);
    rig.outage(3500, 9000);
    rig.fault(20_500, 30_500, Some(FEED), eio);
    rig.fault(45_500, 48_500, Some(FEED), short_read);
    rig.upload(60_500, 66_500);
    rig.stop_at(200_000);
    let usages = [90, 90, 90, 20, 90, 90, 90, 90, 90, 90, 10, 90, 90];
    let polls: Vec<SensorSample> = usages
        .iter()
        .cycle()
        .take(200)
        .copied()
        .map(at_usage)
        .collect();
    let config = adaptive(None, FeedFrameLen::Truncated);

    run_on(&rig, config, &polls);

    assert_allowed_frames(&rig.trace(), config);
    assert_each_connection_tried_e1_twice(&rig);
    let feed = rig.delivered(FEED);
    assert!(
        feed.windows(2).all(|pair| pair[1] - pair[0] >= 1000),
        "frames closer than a tick: {feed:?}"
    );
}

thread_local! {
    /// What the running test logged. Tests run on threads of their own, so
    /// parallel tests do not see each other's lines.
    static LOGGED: RefCell<Vec<(Level, String)>> = const { RefCell::new(Vec::new()) };
}

struct Capture;

impl Log for Capture {
    fn enabled(&self, _: &Metadata<'_>) -> bool {
        true
    }

    fn log(&self, record: &Record<'_>) {
        let line = (record.level(), record.args().to_string());
        LOGGED.with_borrow_mut(|lines| lines.push(line));
    }

    fn flush(&self) {}
}

/// Starts keeping the log lines of this thread.
fn capture_log() {
    static INSTALL: Once = Once::new();
    INSTALL.call_once(|| {
        log::set_logger(&Capture).unwrap();
        log::set_max_level(log::LevelFilter::Debug);
    });
    LOGGED.with_borrow_mut(Vec::clear);
}

/// The lines logged at `level` that start with `prefix`.
fn logged(level: Level, prefix: &str) -> Vec<String> {
    LOGGED.with_borrow(|lines| {
        let matching = lines
            .iter()
            .filter(|(at, line)| *at == level && line.starts_with(prefix));
        matching.map(|(_, line)| line.clone()).collect()
    })
}

fn write_cost_lines() -> Vec<String> {
    logged(Level::Info, "E3 write cost")
}

fn write_lines() -> Vec<String> {
    logged(Level::Debug, "E3 write of")
}

#[test]
fn the_write_cost_is_the_write_alone_and_summarized_every_60_ticks() {
    capture_log();
    assert_eq!(WRITE_COST_REPORT_TICKS, 60);
    let rig = Rig::new();
    rig.device.write_cost.set(ms(25));
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

    rig.run(Some(125), slow_sensors).unwrap();

    let writes = write_lines();
    assert_eq!(writes.len(), 125);
    assert!(
        writes
            .iter()
            .all(|line| line == "E3 write of 256 bytes took 25.0ms"),
        "{writes:?}"
    );
    assert_eq!(
        write_cost_lines(),
        ["E3 write cost: count 60, mean 25.0ms, max 25.0ms"; 2]
    );
}

#[test]
fn the_write_cost_summary_gives_mean_and_max_of_its_own_period() {
    capture_log();
    let rig = Rig::new();
    rig.device.write_cost.set(ms(10));
    let device = rig.device.clone();
    rig.clock.at(29_500, move || device.write_cost.set(ms(30)));
    let device = rig.device.clone();
    rig.clock.at(60_500, move || device.write_cost.set(ms(5)));

    rig.ticks(120).unwrap();

    assert_eq!(
        write_cost_lines(),
        [
            "E3 write cost: count 60, mean 20.0ms, max 30.0ms",
            "E3 write cost: count 60, mean 5.4ms, max 30.0ms",
        ]
    );
}

#[test]
fn the_write_cost_of_an_adaptive_feed_names_the_length_and_counts_ticks_not_writes() {
    capture_log();
    let rig = Rig::new();
    rig.device.write_cost.set(ms(12));

    run_on(
        &rig,
        adaptive(Some(60), FeedFrameLen::Truncated),
        &[at_usage(90)],
    );

    assert_eq!(write_lines(), ["E3 write of 128 bytes took 12.0ms"; 20]);
    assert_eq!(
        write_cost_lines(),
        ["E3 write cost: count 20, mean 12.0ms, max 12.0ms"]
    );
}

#[test]
fn a_failed_write_is_logged_and_stays_out_of_the_summary() {
    capture_log();
    let rig = Rig::new();
    rig.device.write_cost.set(ms(40));
    rig.fault(2500, 3500, Some(FEED), eio);

    rig.ticks(60).unwrap();

    assert_eq!(
        logged(Level::Debug, "E3 write of 256 bytes failed"),
        ["E3 write of 256 bytes failed after 40.0ms"]
    );
    assert_eq!(
        write_cost_lines(),
        ["E3 write cost: count 59, mean 40.0ms, max 40.0ms"]
    );
}

#[test]
fn a_skipped_tick_costs_no_write_and_a_period_without_writes_reports_nothing() {
    capture_log();
    let rig = Rig::new();
    rig.upload(500, 179_500);

    rig.ticks(181).unwrap();

    assert_eq!(rig.delivered(FEED), [0, 180_000]);
    assert_eq!(write_lines().len(), 2);
    assert_eq!(
        write_cost_lines(),
        ["E3 write cost: count 1, mean 0.0ns, max 0.0ns"]
    );
}

#[test]
fn a_panel_that_ignores_the_first_e5_gets_a_second_and_then_passes() {
    capture_log();
    let rig = after_power_loss_with_widgets();
    rig.device.ignores_e5.set(1);
    let config = in_mode_0(Some(2));

    rig.run_configured(config, healthy).unwrap();

    assert_eq!(
        rig.opcodes(),
        [PROBE, MODE_QUERY, SET_MODE, MODE_QUERY, SET_MODE, MODE_QUERY, READ_BACK, FEED, FEED]
    );
    assert_eq!(rig.attempts(SET_MODE), [0, 1000]);
    assert_eq!(rig.attempts(MODE_QUERY), [0, 1000, 2000]);
    assert_eq!(rig.delivered(FEED), [2000, 3000]);
    assert_eq!(rig.device.mode.get(), RAW_STATS);
    assert_eq!(
        logged(Level::Warn, "panel"),
        ["panel is in mode 8 (raw 0x0B) after E5 1 of 3 with mode 0: sending it again"]
    );
    assert_allowed_frames(&rig.trace(), config);
}

#[test]
fn a_panel_that_never_takes_e5_gets_exactly_three_then_a_warning_and_the_feed() {
    capture_log();
    let rig = after_power_loss_with_widgets();
    rig.device.ignores_e5.set(u32::MAX);
    let config = in_mode_0(Some(2));

    rig.run_configured(config, healthy).unwrap();

    assert_eq!(MODE_ATTEMPTS, 3);
    assert_eq!(rig.attempts(SET_MODE), [0, 1000, 2000]);
    assert_eq!(rig.attempts(MODE_QUERY), [0, 1000, 2000, 3000]);
    assert_eq!(rig.attempts(READ_BACK), [3000]);
    assert_eq!(rig.delivered(FEED), [3000, 4000]);
    assert_eq!(
        logged(Level::Warn, "panel")[2],
        "panel is still in mode 8 (raw 0x0B) after 3 E5 with mode 0: feeding anyway"
    );
    assert_allowed_frames(&rig.trace(), config);
}

#[test]
fn a_panel_that_ignores_the_first_e1_gets_a_second_and_then_passes() {
    capture_log();
    let rig = Rig::new();
    rig.panel_has(0, 0);
    rig.device.ignores_e1.set(1);

    rig.ticks(2).unwrap();

    assert_eq!(
        rig.opcodes(),
        [PROBE, READ_BACK, DASHBOARD, READ_BACK, DASHBOARD, READ_BACK, FEED, FEED]
    );
    assert_eq!(rig.attempts(DASHBOARD), [0, 1000]);
    assert_eq!(rig.delivered(FEED), [2000, 3000]);
    assert_eq!(rig.device.panel.get(), (DEFAULT_MASK, DEFAULT_INTERVAL));
    assert_eq!(
        logged(Level::Warn, "panel"),
        [
            "panel has widgets 0x00 every 0 s after E1 1 of 2 with widgets 0xAF every 3 s: \
             sending it again"
        ]
    );
    assert_only_allowed_frames(&rig.trace());
}

#[test]
fn a_panel_that_never_takes_e1_gets_exactly_two_then_a_warning_and_the_feed() {
    capture_log();
    let rig = Rig::new();
    rig.panel_has(0, 0);
    rig.device.forgets_e1.set(true);

    rig.ticks(2).unwrap();

    assert_eq!(DASHBOARD_ATTEMPTS, 2);
    assert_eq!(rig.attempts(DASHBOARD), [0, 1000]);
    assert_eq!(rig.attempts(READ_BACK), [0, 1000, 2000]);
    assert_eq!(rig.delivered(FEED), [2000, 3000]);
    assert_eq!(
        logged(Level::Warn, "panel")[1],
        "panel still has widgets 0x00 every 0 s after 2 E1 with widgets 0xAF every 3 s: \
         feeding anyway"
    );
    assert_only_allowed_frames(&rig.trace());
}

#[test]
fn a_named_dashboard_must_read_back_exactly_as_sent() {
    let rig = Rig::new();
    rig.panel_has(0xFF, 4);
    rig.device.ignores_e1.set(1);
    let config = naming(0x05, 7, 1);

    rig.run_configured(config, healthy).unwrap();

    assert_eq!(rig.attempts(DASHBOARD), [0, 1000]);
    assert_eq!(rig.device.panel.get(), (0x05, 7));
    assert_allowed_frames(&rig.trace(), config);
}

#[test]
fn shutdown_during_the_settle_sends_nothing_more() {
    let rig = after_power_loss_with_widgets();
    rig.stop_at(500);

    rig.run_configured(in_mode_0(None), healthy).unwrap();

    rig.assert_stopped_promptly(500);
    assert_eq!(rig.opcodes(), [PROBE, MODE_QUERY, SET_MODE]);
}

#[test]
fn an_upload_during_the_settle_ends_the_connection_and_the_next_finds_the_mode_set() {
    let rig = after_power_loss_with_widgets();
    rig.upload(500, 1500);
    let config = in_mode_0(Some(1));

    rig.run_configured(config, healthy).unwrap();

    // The read-back at 1 s finds the lock busy, so the connection is tried again
    // after the backoff, and that one finds the mode already set.
    assert_eq!(rig.attempts(PROBE), [0, 3000]);
    assert_eq!(rig.attempts(SET_MODE), [0]);
    assert_eq!(rig.attempts(MODE_QUERY), [0, 3000]);
    assert_eq!(rig.delivered(FEED), [3000]);
    assert_allowed_frames(&rig.trace(), config);
}

/// The ticks that reach just past the first check, at 60 s.
const PAST_THE_CHECK: u64 = 61;

#[test]
fn there_is_no_check_before_60_seconds() {
    assert_eq!(CHECK_PERIOD, Duration::from_secs(60));
    let rig = Rig::new();
    rig.panel_in_mode(RAW_STATS);

    rig.run_configured(in_mode_0(Some(60)), healthy).unwrap();

    assert_eq!(rig.attempts(MODE_QUERY), [0]);
    assert_eq!(rig.attempts(READ_BACK), [0]);
    assert_eq!(rig.delivered(FEED).last(), Some(&59_000));
}

#[test]
fn the_check_reads_de_and_df_between_ticks_under_the_lock() {
    let rig = Rig::new();
    rig.panel_in_mode(RAW_STATS);
    let config = in_mode_0(Some(PAST_THE_CHECK + 60));

    rig.run_configured(config, healthy).unwrap();

    assert_eq!(rig.attempts(MODE_QUERY), [0, 60_000, 120_000]);
    assert_eq!(rig.attempts(READ_BACK), [0, 60_000, 120_000]);
    assert_eq!(rig.attempts(PROBE), [0]);
    assert_eq!(rig.attempts(SET_MODE), []);
    let trace = rig.trace();
    let check = trace
        .iter()
        .position(|sent| sent.at == 60_000 && sent.opcode() == MODE_QUERY);
    let check = check.unwrap();
    assert_eq!(
        trace[check - 1].opcode(),
        FEED,
        "the check follows the tick"
    );
    assert_eq!(trace[check + 1].opcode(), READ_BACK);
    assert_eq!(trace[check + 2].opcode(), FEED);
    assert_allowed_frames(&trace, config);
}

#[test]
fn without_a_mode_the_check_reads_no_de() {
    let rig = Rig::new();

    rig.ticks(PAST_THE_CHECK).unwrap();

    assert_eq!(rig.attempts(MODE_QUERY), []);
    assert_eq!(rig.attempts(READ_BACK), [0, 60_000]);
    assert_only_allowed_frames(&rig.trace());
}

#[test]
fn the_check_finds_a_changed_mode_and_sets_the_panel_up_again() {
    capture_log();
    let rig = Rig::new();
    rig.panel_in_mode(RAW_STATS);
    // An unexplained flip to mode 5, the stored GIF.
    let device = rig.device.clone();
    rig.clock.at(30_500, move || device.mode.set(0x06));
    let config = in_mode_0(Some(PAST_THE_CHECK + 1));

    rig.run_configured(config, healthy).unwrap();

    // The reconnect goes at once: no backoff.
    assert_eq!(rig.attempts(PROBE), [0, 60_000]);
    assert_eq!(rig.attempts(MODE_QUERY), [0, 60_000, 60_000, 61_000]);
    assert_eq!(rig.attempts(SET_MODE), [60_000]);
    assert_eq!(rig.device.mode.get(), RAW_STATS);
    assert_eq!(rig.delivered(FEED)[60..], [60_000, 61_000]);
    assert_eq!(
        logged(Level::Warn, "check"),
        ["check: panel is in mode 5 (raw 0x06): setting the panel up again"]
    );
    assert_allowed_frames(&rig.trace(), config);
}

#[test]
fn the_check_finds_vanished_widgets_and_sets_the_panel_up_again() {
    capture_log();
    let rig = Rig::new();
    let device = rig.device.clone();
    rig.clock.at(30_500, move || device.panel.set((0, 0)));

    rig.ticks(PAST_THE_CHECK + 1).unwrap();

    assert_eq!(rig.attempts(PROBE), [0, 60_000]);
    assert_eq!(rig.attempts(READ_BACK), [0, 60_000, 60_000, 61_000]);
    assert_eq!(rig.attempts(DASHBOARD), [60_000]);
    assert_eq!(rig.device.panel.get(), (DEFAULT_MASK, DEFAULT_INTERVAL));
    assert_eq!(
        logged(Level::Warn, "check"),
        ["check: panel has widgets 0x00 every 0 s: setting the panel up again"]
    );
    assert_only_allowed_frames(&rig.trace());
}

#[test]
fn the_check_leaves_other_widgets_alone_when_none_are_named() {
    let rig = Rig::new();
    let device = rig.device.clone();
    rig.clock.at(30_500, move || device.panel.set((0xFF, 4)));

    rig.ticks(PAST_THE_CHECK).unwrap();

    assert_eq!(rig.attempts(PROBE), [0]);
    assert_eq!(rig.attempts(DASHBOARD), []);
}

#[test]
fn the_check_is_skipped_while_an_upload_holds_the_bus() {
    let rig = Rig::new();
    rig.upload(59_500, 60_500);

    rig.ticks(PAST_THE_CHECK + 60).unwrap();

    assert_eq!(rig.attempts(READ_BACK), [0, 120_000]);
    assert_eq!(rig.attempts(PROBE), [0]);
    assert_only_allowed_frames(&rig.trace());
}

#[test]
fn a_failed_check_is_tried_again_after_the_next_tick() {
    let rig = Rig::new();
    rig.read_fault(59_500, 60_500, READ_BACK, short_read);

    rig.ticks(PAST_THE_CHECK + 60).unwrap();

    // Retried at 61 s, and from there the next one is due a period after 60 s.
    assert_eq!(rig.attempts(READ_BACK), [0, 60_000, 61_000, 120_000]);
    assert_eq!(rig.attempts(PROBE), [0]);
    assert_only_allowed_frames(&rig.trace());
}

#[test]
fn three_failed_checks_in_a_row_end_the_session_even_with_good_writes_between() {
    let rig = Rig::new();
    rig.read_fault(59_500, 62_500, READ_BACK, short_read);

    rig.ticks(PAST_THE_CHECK + 3).unwrap();

    assert_eq!(rig.attempts(READ_BACK), [0, 60_000, 61_000, 62_000, 64_000]);
    assert_eq!(rig.delivered(FEED)[60..], [60_000, 61_000, 62_000, 64_000]);
    assert_eq!(rig.attempts(PROBE), [0, 64_000]);
    assert_only_allowed_frames(&rig.trace());
}

#[test]
fn a_permission_error_in_the_check_is_fatal() {
    let rig = Rig::new();
    rig.read_fault(59_500, 60_500, READ_BACK, || errno_error(Errno::ACCESS));

    let error = rig.ticks(PAST_THE_CHECK + 5).unwrap_err();

    assert!(matches!(error, Error::I2c { .. }), "{error:?}");
}

#[test]
fn a_panel_that_never_takes_e5_is_set_up_again_once_a_minute() {
    let rig = after_power_loss_with_widgets();
    rig.device.ignores_e5.set(u32::MAX);
    let config = in_mode_0(Some(130));

    rig.run_configured(config, healthy).unwrap();

    // Each round: three E5 a settle apart, the feed, and a check a minute later.
    assert_eq!(rig.attempts(PROBE), [0, 63_000, 126_000]);
    assert_eq!(rig.attempts(SET_MODE).len(), 9);
    assert_allowed_frames(&rig.trace(), config);
}

#[test]
fn the_mode_decision_is_logged_once_per_connection() {
    capture_log();
    let rig = after_power_loss_with_widgets();
    rig.outage(500, 3500);

    rig.run_configured(in_mode_0(Some(5)), healthy).unwrap();

    assert_eq!(
        logged(Level::Info, "panel is in"),
        [
            "panel is in mode 8 (raw 0x0B): sending E5 with mode 0",
            "panel is in mode 0 (raw 0x01): no E5 needed",
        ]
    );
}

#[test]
fn a_mode_byte_with_no_meaning_is_logged_as_unknown_and_replaced() {
    capture_log();
    let rig = Rig::new();
    rig.panel_has(0xFF, 4);
    rig.panel_in_mode(0x09);

    rig.run_configured(in_mode_0(Some(1)), healthy).unwrap();

    assert_eq!(
        logged(Level::Info, "panel is in"),
        ["panel is in unknown mode (raw 0x09): sending E5 with mode 0"]
    );
    assert_eq!(rig.device.mode.get(), RAW_STATS);
}

#[test]
fn without_a_mode_nothing_about_the_mode_is_logged() {
    capture_log();
    let rig = Rig::new();

    rig.ticks(1).unwrap();

    assert_eq!(logged(Level::Info, "panel is in"), [""; 0]);
}

#[test]
fn a_dry_run_has_no_bus_to_measure() {
    capture_log();
    let rig = Rig::new();

    dry_run(&rig, Some(60), Output::default()).unwrap();

    assert_eq!(write_lines(), [""; 0]);
    assert_eq!(write_cost_lines(), [""; 0]);
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

/// Runs a dry-run of the default feed printing to clones of `output`, with no
/// sensors. That reads as zeros, which is under the adaptive threshold.
fn dry_run<W: Write + Clone>(rig: &Rig, max_ticks: Option<u64>, output: W) -> Result<()> {
    let config = DaemonConfig {
        max_ticks,
        ..DaemonConfig::default()
    };
    dry_run_on(rig, config, no_sensors, output)
}

/// Runs `config` as a dry-run printing to clones of `output`.
fn dry_run_on<W: Write + Clone, S: SensorSource>(
    rig: &Rig,
    config: DaemonConfig,
    open_sensors: impl FnMut(Option<u32>) -> Result<S>,
    output: W,
) -> Result<()> {
    run_with(
        config,
        Environment {
            connect: || {
                let dry_run = DryRun::with_output(output.clone(), false);
                Ok(Session::<Device, W>::DryRun(dry_run))
            },
            open_sensors,
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
    let read_backs = text.lines().filter(|line| line.starts_with("DF "));
    assert_eq!(read_backs.count(), 1, "{text}");
    let feeds = text.lines().filter(|line| line.starts_with("E3 "));
    assert_eq!(feeds.count(), 3, "{text}");
    assert_eq!(rig.clock.elapsed_ms(), 2000);
}

/// What a dry-run of `config` prints at `usage` percent GPU usage.
fn dry_run_text_at(usage: u32, config: DaemonConfig) -> String {
    let rig = Rig::new();
    let output = Output::default();
    let polls = Polls::of(&[at_usage(usage)]);

    dry_run_on(&rig, config, |_| Ok(polls.clone()), output.clone()).unwrap();

    let text = String::from_utf8(output.0.borrow().clone()).unwrap();
    text
}

fn feed_count(text: &str) -> usize {
    text.lines().filter(|line| line.starts_with("E3 ")).count()
}

const WAITING: &str = "the adaptive feed is waiting for a full window";

fn notes(text: &str) -> usize {
    text.matches(WAITING).count()
}

#[test]
fn a_dry_run_under_load_says_why_it_shows_no_feed() {
    let config = DaemonConfig {
        max_ticks: Some(3),
        ..DaemonConfig::default()
    };

    let text = dry_run_text_at(90, config);

    assert_eq!(feed_count(&text), 0, "{text}");
    assert_eq!(notes(&text), 1, "{text}");
    assert!(
        text.contains("GPU usage is 50% or above"),
        "not the threshold: {text}"
    );
    assert!(
        text.contains("its next frame comes after 5 polls"),
        "not the window: {text}"
    );
}

#[test]
fn a_dry_run_that_reaches_the_frames_says_so_only_once() {
    let config = DaemonConfig {
        max_ticks: Some(12),
        ..DaemonConfig::default()
    };

    let text = dry_run_text_at(90, config);

    assert_eq!(feed_count(&text), 4, "{text}");
    assert_eq!(notes(&text), 1, "{text}");
}

#[test]
fn a_dry_run_with_a_frame_on_each_tick_has_nothing_to_explain() {
    let idle = DaemonConfig {
        max_ticks: Some(3),
        ..DaemonConfig::default()
    };
    let busy_but_steady = DaemonConfig {
        max_ticks: Some(3),
        ..steady()
    };

    for text in [
        dry_run_text_at(49, idle),
        dry_run_text_at(90, busy_but_steady),
    ] {
        assert_eq!(feed_count(&text), 3, "{text}");
        assert_eq!(notes(&text), 0, "{text}");
    }
}

#[test]
fn dry_run_shows_df_says_a_real_run_decides_and_then_shows_the_default_e1() {
    let rig = Rig::new();
    let output = Output::default();

    dry_run(&rig, Some(1), output.clone()).unwrap();

    let text = String::from_utf8(output.0.borrow().clone()).unwrap();
    let position = |needle: &str| text.find(needle).expect(needle);
    assert!(
        position("DF GetDisplay") < position("a real run decides"),
        "{text}"
    );
    assert!(
        position("a real run decides") < position("E1 SetDisplay"),
        "{text}"
    );
    assert!(
        text.contains("E1 CB 55 AC 38 01 01 01 01 00 01 00 01 03 00 00"),
        "{text}"
    );
}

#[test]
fn dry_run_ends_on_a_closed_stdout_rather_than_reconnecting() {
    let rig = Rig::new();
    let error = dry_run(&rig, None, ClosedPipe).unwrap_err();

    assert!(matches!(error, Error::I2c { .. }), "{error:?}");
    assert_eq!(rig.clock.elapsed_ms(), 0);
}

#[test]
fn dry_run_with_a_mode_shows_de_and_e5_before_df_and_e1() {
    let rig = Rig::new();
    let output = Output::default();
    let config = DaemonConfig {
        mode: Some(Mode::Faith1),
        max_ticks: Some(1),
        ..DaemonConfig::default()
    };

    dry_run_on(&rig, config, no_sensors, output.clone()).unwrap();

    let text = String::from_utf8(output.0.borrow().clone()).unwrap();
    let position = |needle: &str| text.find(needle).expect(needle);
    let order = [
        "DE GetMode",
        "so E5 is shown; a real run decides from DE",
        "E5 SetMode",
        "DF GetDisplay",
        "so E1 is shown; a real run decides from DF",
        "E1 SetDisplay",
    ];
    for pair in order.windows(2) {
        assert!(position(pair[0]) < position(pair[1]), "{pair:?}: {text}");
    }
    let modes = text.lines().filter(|line| line.starts_with("E5 "));
    assert_eq!(modes.count(), 1, "{text}");
}

#[test]
fn dry_run_without_a_mode_shows_no_de_or_e5() {
    let rig = Rig::new();
    let output = Output::default();

    dry_run(&rig, Some(1), output.clone()).unwrap();

    let text = String::from_utf8(output.0.borrow().clone()).unwrap();
    assert!(!text.lines().any(|line| line.starts_with("DE ")), "{text}");
    assert!(!text.lines().any(|line| line.starts_with("E5 ")), "{text}");
}
