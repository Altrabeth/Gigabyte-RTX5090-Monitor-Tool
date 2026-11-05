//! The write gate, end to end through the public API. Nothing here opens a
//! device: the gate only ever receives a [`Mock`], and the sysfs tree and lock
//! file are temporary.

mod common;

use std::cell::RefCell;
use std::fs;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::time::Duration;

use aorus_lcd::bus::discovery::Adapter;
use aorus_lcd::bus::lock::BusLock;
use aorus_lcd::bus::transport::testing::{i2c_error, Mock};
use aorus_lcd::bus::transport::{Retrying, Transport};
use aorus_lcd::panel::{RecordingSleep, Sleep};
use aorus_lcd::protocol::{probe_frame, PROBE_READ_LEN};
use aorus_lcd::session::{connect_with, Environment, Options, Session, Verified};
use aorus_lcd::{Error, Result};
use tempfile::TempDir;

const PANEL: &str = "NVIDIA i2c adapter 1 at 1:00.0";
const SMBUS: &str = "SMBus I801 adapter at 0000:00:1f.4";
const STATUS: [u8; PROBE_READ_LEN] = [1, 2, 3, 4, 5, 6, 7, 8];
const NEXT_FRAME: [u8; 4] = [0xE7, 0xCB, 0x55, 0xAC];
const EACCES: i32 = 13;

/// A sleep that returns at once. Clones share what it saw, and during every
/// sleep it tries the lock file, as an upload waiting for the bus would.
#[derive(Clone)]
struct Spy {
    lock_path: PathBuf,
    delays: Rc<RefCell<Vec<Duration>>>,
    lock_free: Rc<RefCell<Vec<bool>>>,
}

impl Spy {
    fn delays_in_seconds(&self) -> Vec<f64> {
        let delays = self.delays.borrow();
        delays.iter().map(Duration::as_secs_f64).collect()
    }
}

impl Sleep for Spy {
    fn sleep(&mut self, duration: Duration) {
        self.delays.borrow_mut().push(duration);
        let free = BusLock::try_acquire(&self.lock_path).is_ok();
        self.lock_free.borrow_mut().push(free);
    }
}

/// A temporary sysfs tree and lock path, and a scripted device behind them.
struct Rig {
    dir: TempDir,
    mock: Mock,
    opened: Rc<RefCell<Vec<u32>>>,
}

impl Rig {
    /// One panel adapter, on bus 7 so that a wrong default would show.
    fn new() -> Self {
        Self::with_adapter(7, PANEL)
    }

    fn with_adapter(bus: u32, name: &str) -> Self {
        let dir = TempDir::new().unwrap();
        let adapter = dir.path().join("sysfs").join(format!("i2c-{bus}"));
        fs::create_dir_all(&adapter).unwrap();
        fs::write(adapter.join("name"), format!("{name}\n")).unwrap();
        Self {
            dir,
            mock: Mock::new(),
            opened: Rc::default(),
        }
    }

    fn sysfs_root(&self) -> PathBuf {
        self.dir.path().join("sysfs")
    }

    fn lock_path(&self) -> PathBuf {
        self.dir.path().join("test.lock")
    }

    fn spy(&self) -> Spy {
        Spy {
            lock_path: self.lock_path(),
            delays: Rc::default(),
            lock_free: Rc::default(),
        }
    }

    /// The opener of the scripted device, which records the buses it opens.
    fn open(&self) -> impl FnMut(u32) -> Result<Mock> {
        let mock = self.mock.clone();
        let opened = self.opened.clone();
        move |bus| {
            opened.borrow_mut().push(bus);
            Ok(mock.clone())
        }
    }

    fn connect(&self, options: &Options, sleep: impl Sleep) -> Result<Verified<Mock>> {
        let environment = Environment {
            sysfs_root: self.sysfs_root(),
            lock_path: self.lock_path(),
            open: self.open(),
            sleep,
            dry_run_output: Vec::new(),
        };
        match connect_with(options, environment)? {
            Session::Live(verified) => Ok(verified),
            Session::DryRun(_) => panic!("a live connection was asked for"),
        }
    }

    fn connect_once(&self) -> Result<Verified<Mock>> {
        self.connect(&Options::default(), RecordingSleep::default())
    }

    /// Fails unless everything ever sent is `count` `EB 03` frames.
    fn assert_only_probes(&self, count: usize) {
        assert_eq!(self.mock.writes(), vec![probe_frame().to_vec(); count]);
        assert_eq!(self.mock.write_attempts(), count);
    }

    fn assert_lock_is_free(&self) {
        drop(BusLock::try_acquire(&self.lock_path()).expect("the lock should be free"));
    }

    fn assert_lock_is_held(&self) {
        assert!(matches!(
            BusLock::try_acquire(&self.lock_path()),
            Err(Error::LockBusy { .. })
        ));
    }
}

fn waiting(seconds: u64) -> Options {
    Options {
        wait: Duration::from_secs(seconds),
        ..Options::default()
    }
}

fn permission_denied() -> Error {
    Error::I2c {
        context: "injected by the test".into(),
        source: std::io::Error::from_raw_os_error(EACCES),
    }
}

// The gate itself.

#[test]
fn a_full_read_back_opens_the_gate() {
    let rig = Rig::new();
    rig.mock.script_read(STATUS);

    let mut verified = rig.connect_once().unwrap();

    let adapter = Adapter {
        bus: 7,
        name: PANEL.into(),
    };
    assert_eq!(verified.adapter(), &adapter);
    assert_eq!(verified.status(), STATUS);
    assert_eq!(*rig.opened.borrow(), [7]);
    assert_eq!(rig.mock.read_attempts(), 1);

    verified.write(&NEXT_FRAME).unwrap();

    assert_eq!(
        rig.mock.writes(),
        [probe_frame().to_vec(), NEXT_FRAME.to_vec()]
    );
}

#[test]
fn the_probe_sends_the_eb_03_of_the_vectors_and_returns_the_reply() {
    let case = common::case("commands.json", "read_cmd_eb_03");
    let rig = Rig::new();
    let reply = case.bytes("canned_reply");
    rig.mock.script_read(reply.clone());

    let verified = rig.connect_once().unwrap();

    let writes = rig.mock.writes();
    assert_eq!(writes.len(), 1, "{}", case.label());
    case.byte_expect().check(&writes[0], &case.label());
    assert_eq!(verified.status().as_slice(), reply, "{}", case.label());
    assert_eq!(rig.mock.read_attempts(), 1, "{}", case.label());
}

#[test]
fn the_probe_fails_when_the_read_back_fails_as_in_the_vectors() {
    let case = common::case("properties.json", "probe_fails_when_readback_fails");
    assert!(case.bool("fail_read"), "{}", case.label());
    let rig = Rig::new();
    rig.mock.script_read_error(i2c_error());

    let result = rig.connect_once();

    assert!(matches!(result, Err(Error::I2c { .. })), "{result:?}");
}

#[test]
fn only_eb_03_is_written_when_the_read_back_fails_errors_or_is_short() {
    let scripts: [fn(&Mock); 5] = [
        |mock| mock.script_read_error(i2c_error()),
        |mock| mock.script_read_error(permission_denied()),
        |mock| mock.script_read([]),
        |mock| mock.script_read(STATUS[..7].to_vec()),
        // Dry-run's error. A real link cannot return it, but it is no pass.
        |mock| mock.script_read_error(Error::ReadbackUnavailable),
    ];
    for script in scripts {
        let rig = Rig::new();
        script(&rig.mock);

        assert!(rig.connect_once().is_err());

        rig.assert_only_probes(1);
        assert_eq!(rig.mock.read_attempts(), 1);
        rig.assert_lock_is_free();
    }
}

#[test]
fn a_short_read_is_reported_as_one() {
    let rig = Rig::new();
    rig.mock.script_read([1, 2, 3]);

    let result = rig.connect_once();

    assert!(
        matches!(
            result,
            Err(Error::ShortRead {
                expected: 8,
                actual: 3
            })
        ),
        "{result:?}"
    );
}

#[test]
fn a_failed_probe_write_is_not_followed_by_a_read() {
    let rig = Rig::new();
    rig.mock.fail_write(0, i2c_error());

    assert!(rig.connect_once().is_err());

    assert_eq!(rig.mock.write_attempts(), 1);
    assert_eq!(rig.mock.read_attempts(), 0);
}

// The order: adapter, lock, device, probe.

#[test]
fn the_lock_is_held_when_the_device_opens() {
    let rig = Rig::new();
    rig.mock.script_read(STATUS);
    let lock_path = rig.lock_path();
    let mut open = rig.open();
    let environment = Environment {
        sysfs_root: rig.sysfs_root(),
        lock_path: rig.lock_path(),
        open: |bus| {
            assert!(matches!(
                BusLock::try_acquire(&lock_path),
                Err(Error::LockBusy { .. })
            ));
            open(bus)
        },
        sleep: RecordingSleep::default(),
        dry_run_output: Vec::new(),
    };

    let session = connect_with(&Options::default(), environment).unwrap();

    assert!(matches!(session, Session::Live(_)));
    assert_eq!(rig.opened.borrow().len(), 1);
}

#[test]
fn a_refused_adapter_stops_before_the_lock_and_the_device() {
    let rig = Rig::with_adapter(8, SMBUS);
    let spy = rig.spy();
    let options = Options {
        bus: Some(8),
        ..waiting(60)
    };

    let result = rig.connect(&options, spy.clone());

    assert!(
        matches!(result, Err(Error::AdapterRefused(_))),
        "{result:?}"
    );
    assert!(!rig.lock_path().exists());
    assert!(rig.opened.borrow().is_empty());
    assert!(spy.delays.borrow().is_empty(), "a refusal is never retried");
}

#[test]
fn no_adapter_at_all_stops_before_the_lock_and_the_device() {
    let rig = Rig::new();
    let environment = Environment {
        sysfs_root: rig.dir.path().join("absent"),
        lock_path: rig.lock_path(),
        open: rig.open(),
        sleep: RecordingSleep::default(),
        dry_run_output: Vec::new(),
    };

    let result = connect_with(&Options::default(), environment);

    assert!(matches!(result, Err(Error::Adapter(_))), "{result:?}");
    assert!(rig.opened.borrow().is_empty());
}

#[test]
fn an_unusable_lock_stops_before_the_device_and_names_the_variable() {
    let rig = Rig::new();
    let environment = Environment {
        sysfs_root: rig.sysfs_root(),
        lock_path: rig.dir.path().join("missing").join("test.lock"),
        open: rig.open(),
        sleep: RecordingSleep::default(),
        dry_run_output: Vec::new(),
    };

    let error = connect_with(&waiting(60), environment)
        .map(|_| ())
        .unwrap_err();

    assert!(matches!(error, Error::Lock { .. }), "{error:?}");
    assert!(error.to_string().contains("AORUS_LCD_LOCK"), "{error}");
    assert!(rig.opened.borrow().is_empty());
}

#[test]
fn a_held_lock_stops_before_the_device_after_a_bounded_wait() {
    let rig = Rig::new();
    let _upload = BusLock::try_acquire(&rig.lock_path()).unwrap();
    let spy = rig.spy();

    let result = rig.connect(&Options::default(), spy.clone());

    assert!(matches!(result, Err(Error::LockBusy { .. })), "{result:?}");
    assert!(rig.opened.borrow().is_empty());
    let waited: Duration = spy.delays.borrow().iter().sum();
    assert_eq!(waited, Duration::from_secs(5));
}

// Dry-run.

struct NoSleep;

impl Sleep for NoSleep {
    fn sleep(&mut self, duration: Duration) {
        panic!("dry-run slept for {duration:?}");
    }
}

fn panicking_open(bus: u32) -> Result<Mock> {
    panic!("dry-run opened bus {bus}");
}

/// Surroundings where any attempt at the real path shows: the opener and the
/// sleep panic, and the lock's directory does not exist.
fn dry_run_environment<W>(
    root: &Path,
    output: W,
) -> Environment<fn(u32) -> Result<Mock>, NoSleep, W> {
    Environment {
        sysfs_root: root.join("no-sysfs"),
        lock_path: root.join("no-such-directory").join("test.lock"),
        open: panicking_open,
        sleep: NoSleep,
        dry_run_output: output,
    }
}

#[test]
fn dry_run_takes_no_real_path_whatever_else_is_set() {
    let dir = TempDir::new().unwrap();
    let options = Options {
        bus: Some(0),
        dry_run: true,
        wait: Duration::from_secs(60),
        verbose: false,
    };

    let mut session = connect_with(&options, dry_run_environment(dir.path(), Vec::new())).unwrap();

    assert!(matches!(session, Session::DryRun(_)));
    assert!(
        fs::read_dir(dir.path()).unwrap().next().is_none(),
        "dry-run created something"
    );
    let mut status = [0u8; PROBE_READ_LEN];
    assert!(matches!(
        session.read(&mut status),
        Err(Error::ReadbackUnavailable)
    ));
}

#[test]
fn dry_run_prints_eb_03_and_then_what_it_is_given() {
    let dir = TempDir::new().unwrap();
    let mut output = Vec::new();
    let options = Options {
        dry_run: true,
        ..Options::default()
    };

    let mut session = connect_with(&options, dry_run_environment(dir.path(), &mut output)).unwrap();
    session.write(&[0x11; 8]).unwrap();
    drop(session);

    let output = String::from_utf8(output).unwrap();
    assert!(
        output.starts_with("EB GetImageTpl (256 bytes)\n"),
        "{output}"
    );
    assert!(
        output.contains("  0000  EB CB 55 AC 38 03 00 00"),
        "{output}"
    );
    assert!(
        output.ends_with("payload chunk (8 bytes): 11 11 11 11 11 11 11 11\n"),
        "{output}"
    );
}

// `--wait`.

#[test]
fn without_wait_a_silent_panel_fails_at_once() {
    let rig = Rig::new();
    let spy = rig.spy();

    let result = rig.connect(&Options::default(), spy.clone());

    assert!(matches!(result, Err(Error::I2c { .. })), "{result:?}");
    assert!(spy.delays.borrow().is_empty());
    rig.assert_only_probes(1);
}

#[test]
fn wait_probes_every_2_seconds_and_gives_up_on_time() {
    let rig = Rig::new();
    let spy = rig.spy();

    let result = rig.connect(&waiting(6), spy.clone());

    assert!(matches!(result, Err(Error::I2c { .. })), "{result:?}");
    assert_eq!(spy.delays_in_seconds(), [2.0, 2.0, 2.0]);
    rig.assert_only_probes(4);
    assert_eq!(rig.opened.borrow().len(), 4);
    rig.assert_lock_is_free();
}

#[test]
fn wait_shortens_the_last_sleep_to_end_on_time() {
    let rig = Rig::new();
    let spy = rig.spy();

    assert!(rig.connect(&waiting(5), spy.clone()).is_err());

    assert_eq!(spy.delays_in_seconds(), [2.0, 2.0, 1.0]);
    rig.assert_only_probes(4);
}

#[test]
fn wait_releases_the_lock_during_every_sleep() {
    let rig = Rig::new();
    let spy = rig.spy();

    assert!(rig.connect(&waiting(10), spy.clone()).is_err());

    assert_eq!(*spy.lock_free.borrow(), [true; 5]);
}

#[test]
fn wait_stops_at_the_first_answer_and_keeps_the_lock_from_there() {
    let rig = Rig::new();
    rig.mock.script_read_error(i2c_error());
    rig.mock.script_read([1, 2, 3]);
    rig.mock.script_read(STATUS);
    let spy = rig.spy();

    let verified = rig.connect(&waiting(60), spy.clone()).unwrap();

    assert_eq!(verified.status(), STATUS);
    assert_eq!(spy.delays_in_seconds(), [2.0, 2.0]);
    rig.assert_only_probes(3);
    rig.assert_lock_is_held();
}

#[test]
fn wait_retries_a_probe_write_that_got_no_answer() {
    let rig = Rig::new();
    rig.mock.fail_write(0, i2c_error());
    rig.mock.script_read(STATUS);
    let spy = rig.spy();

    let verified = rig.connect(&waiting(60), spy.clone()).unwrap();

    assert_eq!(verified.status(), STATUS);
    assert_eq!(spy.delays_in_seconds(), [2.0]);
}

#[test]
fn wait_does_not_retry_a_permission_error() {
    let rig = Rig::new();
    rig.mock.script_read_error(permission_denied());
    let spy = rig.spy();

    assert!(rig.connect(&waiting(60), spy.clone()).is_err());

    assert!(spy.delays.borrow().is_empty());
    rig.assert_only_probes(1);
}

#[test]
fn wait_does_not_retry_a_device_that_will_not_open() {
    let rig = Rig::new();
    let spy = rig.spy();
    let mut opens = 0;
    let environment = Environment {
        sysfs_root: rig.sysfs_root(),
        lock_path: rig.lock_path(),
        open: |_| -> Result<Mock> {
            opens += 1;
            Err(permission_denied())
        },
        sleep: spy.clone(),
        dry_run_output: Vec::new(),
    };

    assert!(connect_with(&waiting(60), environment).is_err());

    assert_eq!(opens, 1);
    assert!(spy.delays.borrow().is_empty());
    rig.assert_lock_is_free();
}

// The two lock shapes.

#[test]
fn the_cli_shape_holds_the_lock_from_the_probe_until_dropped() {
    let rig = Rig::new();
    rig.mock.script_read(STATUS);

    let mut bus = rig.connect_once().unwrap();

    rig.assert_lock_is_held();
    bus.write(&NEXT_FRAME).unwrap();
    rig.assert_lock_is_held();
    drop(bus);
    rig.assert_lock_is_free();
}

/// What the daemon will do. Sensors are read while `session` is unlocked, and
/// each write is one short lock, so an upload can get in between two ticks.
#[test]
fn the_daemon_shape_takes_the_lock_per_write_and_lets_an_upload_in() {
    let rig = Rig::new();
    rig.mock.script_read(STATUS);
    let mut session = rig.connect_once().unwrap().unlock();
    rig.assert_lock_is_free();

    {
        let mut bus = session.lock().unwrap();
        bus.write(&[0xE3, 1]).unwrap();
        rig.assert_lock_is_held();
    }
    rig.assert_lock_is_free();

    let upload = BusLock::try_acquire(&rig.lock_path()).unwrap();
    assert!(matches!(session.lock(), Err(Error::LockBusy { .. })));
    drop(upload);

    session.lock().unwrap().write(&[0xE3, 2]).unwrap();

    rig.assert_lock_is_free();
    assert_eq!(
        rig.mock.writes(),
        [probe_frame().to_vec(), vec![0xE3, 1], vec![0xE3, 2]]
    );
    assert_eq!(*rig.opened.borrow(), [7], "the device stays open");
}

#[test]
fn retries_wrap_the_link_after_the_probe() {
    let rig = Rig::new();
    rig.mock.script_read(STATUS);
    rig.mock.fail_write(1, i2c_error());
    let verified = rig.connect_once().unwrap();
    let mut retrying = Retrying::with_sleep(verified, RecordingSleep::default());

    retrying.write(&NEXT_FRAME).unwrap();

    assert_eq!(
        rig.mock.writes(),
        [probe_frame().to_vec(), NEXT_FRAME.to_vec()]
    );
}
