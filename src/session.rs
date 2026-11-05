//! The write gate: the only way to get a writable link to the real panel.
//!
//! Every command starts at [`connect`]. In this order, it resolves the adapter,
//! takes the bus lock, opens the device, sends `EB 03` and reads the full
//! [`PROBE_READ_LEN`] bytes. Any failure ends there, with nothing else written.
//! This is the README's rule "refuse to write if the `EB 03` probe fails".
//!
//! # Why the gate cannot be skipped
//!
//! [`LinuxI2c`] has no public constructor, and [`connect_waiting`], behind
//! [`connect`], is the only caller of its `pub(crate)` one. The handle goes
//! straight into the gate, and leaves it only inside a [`Verified`], whose fields
//! are private and which has no way to give the handle back. The compiler refuses
//! both ways around that:
//!
//! ```compile_fail,E0624
//! let _ = aorus_lcd::bus::transport::LinuxI2c::open(0);
//! ```
//!
//! ```compile_fail,E0451
//! use aorus_lcd::session::Verified;
//! fn forge<T>(transport: T, other: Verified<T>) -> Verified<T> {
//!     Verified { transport, ..other }
//! }
//! ```
//!
//! # The two lock shapes
//!
//! A [`Verified`] holds the bus lock, which is what the CLI wants: discover,
//! lock, probe and run, under one unbroken lock. Dropping it releases the lock.
//!
//! The daemon calls [`Verified::unlock`] after the probe. The resulting
//! [`Unlocked`] keeps the open device but is no [`Transport`], so forgetting the
//! lock does not compile:
//!
//! ```compile_fail,E0599
//! use aorus_lcd::bus::transport::Transport;
//! use aorus_lcd::session::Unlocked;
//! fn write_without_the_lock<T: Transport>(session: &mut Unlocked<T>) {
//!     session.write(&[0xEB]).unwrap();
//! }
//! ```
//!
//! [`Unlocked::lock`] takes the lock again for one write, giving a short-lived
//! `Verified` that lets an upload in as soon as it is dropped. This is the
//! daemon's tick. Sensors are read before it, outside the lock:
//!
//! ```
//! use aorus_lcd::bus::transport::Transport;
//! use aorus_lcd::session::Unlocked;
//! use aorus_lcd::{Error, Result};
//!
//! fn tick<T: Transport>(session: &mut Unlocked<T>, frame: &[u8]) -> Result<()> {
//!     match session.lock() {
//!         Ok(mut bus) => bus.write(frame),
//!         // An upload holds the bus: skip this tick.
//!         Err(Error::LockBusy { .. }) => Ok(()),
//!         Err(error) => Err(error),
//!     }
//! }
//! ```
//!
//! A `Verified` can be wrapped in
//! [`Retrying`](crate::bus::transport::Retrying) once the probe is done. After
//! [`Error::I2c`](crate::Error::I2c), reconnecting means going through
//! [`connect`] again.

use std::io::{self, Stdout, Write};
use std::path::PathBuf;
use std::time::Duration;

use crate::bus::discovery::{discover, Adapter, SYSFS_ROOT};
use crate::bus::lock::{lock_path, BusLock};
use crate::bus::transport::{is_no_answer, DryRun, LinuxI2c, Transport};
use crate::error::Result;
use crate::panel::{Sleep, ThreadSleep};
use crate::protocol::{probe_frame, PROBE_READ_LEN};

/// How often `--wait` repeats the probe.
const PROBE_RETRY_INTERVAL: Duration = Duration::from_secs(2);

/// How long a command waits for the bus lock. The daemon holds it for one write
/// at a time, so this has to outlast a write for commands not to fail at random.
/// A longer wait means an upload is running, which the user should be told.
const LOCK_TIMEOUT: Duration = Duration::from_secs(5);

/// The bytes read back from `EB 03`.
pub type Status = [u8; PROBE_READ_LEN];

/// How to connect.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Options {
    /// `--bus`: this adapter instead of autodetecting. It must still be an
    /// NVIDIA adapter.
    pub bus: Option<u32>,
    /// `--dry-run`: touch nothing and print the frames instead.
    pub dry_run: bool,
    /// `--wait`: how long to keep repeating a probe that got no answer, since
    /// the controller can need up to a minute after boot. Zero is one attempt.
    pub wait: Duration,
    /// Dump payload chunks in full in dry-run mode.
    pub verbose: bool,
}

/// What [`connect_with`] reaches the outside world through. [`connect`] fills
/// this in with the real ones, and tests with harmless ones.
#[derive(Debug)]
pub struct Environment<O, S, W> {
    /// Where adapters are listed.
    pub sysfs_root: PathBuf,
    /// The bus lock file.
    pub lock_path: PathBuf,
    /// Opens the device on a bus.
    pub open: O,
    /// Waits for the lock and between probes.
    pub sleep: S,
    /// Receives the dry-run dump.
    pub dry_run_output: W,
}

/// A connection: either the proven panel, or a printer standing in for it.
#[derive(Debug)]
pub enum Session<T = LinuxI2c, W = Stdout> {
    /// The probe passed.
    Live(Verified<T>),
    /// `--dry-run`: no device, no lock and no probe result.
    DryRun(DryRun<W>),
}

impl<T: Transport, W: Write> Transport for Session<T, W> {
    fn write(&mut self, frame: &[u8]) -> Result<()> {
        match self {
            Self::Live(verified) => verified.write(frame),
            Self::DryRun(dry_run) => dry_run.write(frame),
        }
    }

    fn read(&mut self, buf: &mut [u8]) -> Result<()> {
        match self {
            Self::Live(verified) => verified.read(buf),
            Self::DryRun(dry_run) => dry_run.read(buf),
        }
    }
}

/// What a passed probe established. It stays true across lock releases, so
/// [`Verified`] and [`Unlocked`] both carry it.
#[derive(Debug, Clone)]
struct Proof {
    adapter: Adapter,
    status: Status,
    lock_path: PathBuf,
}

/// A link that passed the probe, with the bus lock held. It can be written to,
/// and it is the only thing through which the real device can be.
///
/// Fields drop in order, so the device closes while the lock is still held.
#[derive(Debug)]
#[must_use = "dropping this releases the bus lock"]
pub struct Verified<T> {
    transport: T,
    _lock: BusLock,
    proof: Proof,
}

impl<T> Verified<T> {
    /// The adapter that answered.
    pub fn adapter(&self) -> &Adapter {
        &self.proof.adapter
    }

    /// The bytes read back from the probe.
    pub fn status(&self) -> Status {
        self.proof.status
    }

    /// Releases the bus lock and keeps the link, for a caller that takes the
    /// lock per write. See the [module docs](self).
    pub fn unlock(self) -> Unlocked<T> {
        Unlocked {
            transport: self.transport,
            proof: self.proof,
        }
    }
}

impl<T: Transport> Transport for Verified<T> {
    fn write(&mut self, frame: &[u8]) -> Result<()> {
        self.transport.write(frame)
    }

    fn read(&mut self, buf: &mut [u8]) -> Result<()> {
        self.transport.read(buf)
    }
}

/// A link that passed the probe, without the bus lock. It deliberately cannot
/// write: [`Unlocked::lock`] is the way back.
#[derive(Debug)]
pub struct Unlocked<T> {
    transport: T,
    proof: Proof,
}

impl<T> Unlocked<T> {
    /// Takes the bus lock without waiting, for as long as the result lives.
    /// There is no new probe, so this costs one syscall and no traffic.
    ///
    /// # Errors
    ///
    /// [`Error::LockBusy`](crate::Error::LockBusy) if someone else holds the
    /// bus, on which the daemon skips a tick.
    /// [`Error::Lock`](crate::Error::Lock) if the lock file cannot be used.
    pub fn lock(&mut self) -> Result<Verified<&mut T>> {
        let lock = BusLock::try_acquire(&self.proof.lock_path)?;
        Ok(Verified {
            transport: &mut self.transport,
            _lock: lock,
            proof: self.proof.clone(),
        })
    }
}

/// Connects to the panel, or in dry-run mode prepares to print instead.
///
/// # Errors
///
/// [`Error::Adapter`](crate::Error::Adapter) if no usable adapter exists.
/// [`Error::AdapterRefused`](crate::Error::AdapterRefused) if `--bus` was refused.
/// [`Error::Lock`](crate::Error::Lock) or
/// [`Error::LockBusy`](crate::Error::LockBusy) if the bus lock cannot be had.
/// [`Error::I2c`](crate::Error::I2c) or
/// [`Error::ShortRead`](crate::Error::ShortRead) if the probe failed.
pub fn connect(options: &Options) -> Result<Session> {
    connect_waiting(options, ThreadSleep)
}

/// [`connect`], waiting for the bus lock through `sleep`, which lets the daemon
/// cut the wait short on a signal.
///
/// # Errors
///
/// As for [`connect`].
pub fn connect_waiting(options: &Options, sleep: impl Sleep) -> Result<Session> {
    connect_with(
        options,
        Environment {
            sysfs_root: SYSFS_ROOT.into(),
            lock_path: lock_path(),
            open: LinuxI2c::open,
            sleep,
            dry_run_output: io::stdout(),
        },
    )
}

/// [`connect`] with its surroundings passed in. Outside this crate nothing can
/// name a way to open the real device, so there `T` is always harmless.
pub fn connect_with<T, O, S, W>(
    options: &Options,
    environment: Environment<O, S, W>,
) -> Result<Session<T, W>>
where
    T: Transport,
    O: FnMut(u32) -> Result<T>,
    S: Sleep,
    W: Write,
{
    if options.dry_run {
        let mut dry_run = DryRun::with_output(environment.dry_run_output, options.verbose);
        // Shown because a real run sends it first. Nothing can be read back, so
        // this is the whole probe, and every later frame follows it unguarded
        // because a printer has no panel to protect.
        dry_run.write(&probe_frame())?;
        return Ok(Session::DryRun(dry_run));
    }
    verify(options, environment).map(Session::Live)
}

/// The gate. The `Ok(status)` arm is the only place that builds a [`Verified`]
/// from nothing; [`Unlocked::lock`] needs one to have existed.
///
/// Each attempt is a full lock, open and probe, and both are let go before the
/// sleep, so that `--wait` keeps nobody off the bus for a minute. The wait is
/// counted in requested sleeps, like [`BusLock::acquire`], so a test double uses
/// it up without real delay.
fn verify<T, O, S, W>(options: &Options, environment: Environment<O, S, W>) -> Result<Verified<T>>
where
    T: Transport,
    O: FnMut(u32) -> Result<T>,
    S: Sleep,
{
    let Environment {
        sysfs_root,
        lock_path,
        mut open,
        mut sleep,
        ..
    } = environment;

    // Outside the loop: a refused adapter will not fix itself.
    let adapter = discover(&sysfs_root, options.bus)?;
    log::info!("using i2c-{}: {}", adapter.bus, adapter.name);

    let mut waited = Duration::ZERO;
    loop {
        let lock = BusLock::acquire(&lock_path, LOCK_TIMEOUT, &mut sleep)?;
        // A failure to open is not "no answer": it means permission or a missing
        // device, which waiting does not cure.
        let mut transport = open(adapter.bus)?;

        match read_status(&mut transport) {
            Ok(status) => {
                log::debug!("probe answered: {status:02X?}");
                return Ok(Verified {
                    transport,
                    _lock: lock,
                    proof: Proof {
                        adapter,
                        status,
                        lock_path,
                    },
                });
            }
            Err(error) if is_no_answer(&error) && waited < options.wait => {
                log::info!("no answer after {waited:?}: {error}");
                drop(transport);
                drop(lock);
                let nap = PROBE_RETRY_INTERVAL.min(options.wait - waited);
                sleep.sleep(nap);
                waited += nap;
            }
            Err(error) => return Err(error),
        }
    }
}

/// Sends `EB 03` and reads the whole status. A status query rather than a quick
/// write, because the NVIDIA adapter rejects those and unconnected DDC ports
/// falsely acknowledge them, which leaves a completed read-back as the only
/// evidence of a controller. `probe` in `aorus_lcd.py`.
///
/// `transport` must be the raw link, because presence detection has to fail
/// fast.
fn read_status(transport: &mut impl Transport) -> Result<Status> {
    transport.write(&probe_frame())?;
    let mut status = [0u8; PROBE_READ_LEN];
    transport.read(&mut status)?;
    Ok(status)
}
