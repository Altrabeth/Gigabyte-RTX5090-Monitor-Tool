//! The background loop that keeps the panel's sensor feed alive.
//!
//! The panel freezes or blanks its widgets without `E3` at about 1 Hz, so the
//! goal here is that the feed outlives everything that can go wrong around it.
//! [`run_with`] is a state machine over injected parts, which is how every
//! failure below is tested on a fake clock:
//!
//! - **Connect:** through the [write gate](crate::session), then `E1` exactly
//!   once, then `E3` until the bus fails. `EB 03`, one `E1` per connection and
//!   `E3` are all this module sends. `E1` is the prime suspect in dark-panel
//!   incidents (`RECOVERY.md`), and `AA` writes NVRAM.
//! - **Ticks** run on deadlines, each the previous one plus [`TICK`], so the
//!   time spent working does not add up. A missed deadline is dropped rather
//!   than made up for: late frames carry stale values, and a burst of them is
//!   all that a catch-up could add.
//! - **The lock** is held for one write only and sensors are read without it,
//!   so an upload can take the bus between any two ticks. If an upload holds it,
//!   the tick is skipped. That is normal operation: no error and no backoff.
//! - **Failed writes** are retried on the next tick. See below.
//! - **Sensors** failing never stop the feed, which then shows zeros. A zero
//!   reads as a failure, where a repeated value would look live.
//! - **Shutdown** sends nothing: the panel is left exactly as it is.
//!
//! # Why a failed write waits for the next tick
//!
//! The GPU's I2C engine sometimes rejects a valid transfer on a healthy bus, and
//! reconnecting over each of those would send an `E1` every time, so writes need
//! a retry. [`Retrying`](crate::bus::transport::Retrying) does not fit a tick: it
//! sleeps 800 ms across five attempts with the lock held, which keeps an upload
//! off the bus for no reason, and with slow failing transfers on top it runs past
//! the next deadline.
//!
//! So the retry is the next tick. The lock goes between attempts, each attempt
//! carries a fresh sample, and every write stays on its deadline. After
//! [`WRITE_ATTEMPTS`] failures in a row the session is dropped. That spans two
//! seconds, longer than `Retrying` does, so a fault that it would absorb is
//! absorbed here too, at the cost of a frame or two, which a keep-alive does not
//! mind.
//!
//! # Suspend
//!
//! [`Instant`] does not advance during suspend on Linux, so resuming looks like
//! nothing happened until a write fails, and then the normal reconnect applies.
//! Should the clock jump anyway, the deadline rule covers it.

use std::io::Write;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use rustix::io::Errno;
use signal_hook::consts::{SIGINT, SIGTERM};

use crate::bus::discovery::{discover, SYSFS_ROOT};
use crate::bus::transport::{DryRun, Transport};
use crate::error::{Error, Result};
use crate::panel::{Panel, Sleep, ThreadSleep};
use crate::protocol::{parse_widgets, Elements, FeedFrameLen, SensorSample, DEFAULT_WIDGETS};
use crate::sensors::{NvmlSensors, SensorSource};
use crate::session::{self, Options, Session, Unlocked};

/// The time between two deadlines. The panel wants `E3` at about 1 Hz.
pub const TICK: Duration = Duration::from_secs(1);

/// The first wait after a failure, doubling up to [`BACKOFF_MAX`]. The same
/// bounds as `FeedWorker.cs` in the C# reference.
pub const BACKOFF_FIRST: Duration = Duration::from_secs(2);

/// The longest wait between attempts. The controller can need up to a minute
/// after boot.
pub const BACKOFF_MAX: Duration = Duration::from_secs(60);

/// The longest a wait goes without looking at the stop flag, which bounds how
/// late a signal is noticed.
pub const POLL_SLICE: Duration = Duration::from_millis(100);

/// Failed writes in a row that end a session. See the [module docs](self).
pub const WRITE_ATTEMPTS: u32 = 3;

/// What the daemon shows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DaemonConfig {
    /// The widgets `E1` selects.
    pub elements: Elements,
    /// The widget rotation interval `E1` sets, in seconds.
    pub interval_s: u8,
    /// The length of each `E3`.
    pub feed: FeedFrameLen,
    /// Stops after this many ticks. Only tests and dry-run set this.
    pub max_ticks: Option<u64>,
}

impl Default for DaemonConfig {
    /// The README's six default widgets.
    fn default() -> Self {
        Self {
            elements: parse_widgets(DEFAULT_WIDGETS).expect("the default widgets are valid"),
            interval_s: 3,
            feed: FeedFrameLen::Full,
            max_ticks: None,
        }
    }
}

/// A source of time. Deadlines are computed against [`Clock::now`] and all
/// waiting goes through [`Sleep`], so a test double replaces time entirely.
pub trait Clock: Sleep {
    /// The current time.
    fn now(&self) -> Instant;
}

impl Clock for ThreadSleep {
    fn now(&self) -> Instant {
        Instant::now()
    }
}

/// A [`Sleep`] that a stop request cuts short, in slices of [`POLL_SLICE`].
///
/// [`BusLock::acquire`](crate::bus::lock::BusLock::acquire) counts requested
/// sleeps, so once the flag is set the gate's lock wait runs out at once and a
/// signal does not have to sit out an upload.
#[derive(Debug, Clone)]
pub struct UntilStopped<S> {
    /// The sleep being sliced.
    pub sleep: S,
    /// The stop flag.
    pub stop: Arc<AtomicBool>,
}

impl<S: Sleep> Sleep for UntilStopped<S> {
    fn sleep(&mut self, duration: Duration) {
        let mut left = duration;
        while !left.is_zero() && !self.stop.load(Ordering::SeqCst) {
            let slice = left.min(POLL_SLICE);
            self.sleep.sleep(slice);
            left -= slice;
        }
    }
}

/// What [`run_with`] reaches the outside world through. [`run`] fills this in
/// with the real ones, and tests with harmless ones.
#[derive(Debug)]
pub struct Environment<C, F, K> {
    /// Connects through the gate, or prepares a dry-run.
    pub connect: C,
    /// Creates the sensor source, given the PCI bus of the adapter in use.
    pub open_sensors: F,
    /// Tells the time and waits.
    pub clock: K,
    /// Set to ask for a shutdown.
    pub stop: Arc<AtomicBool>,
    /// There is no bus to lose in dry-run, so any error ends the run.
    pub dry_run: bool,
}

/// Runs the daemon on the real bus and sensors until SIGTERM or SIGINT, or in
/// dry-run mode without touching the bus or its lock.
///
/// # Errors
///
/// Only what waiting cannot cure, see [`run_with`], or a failure to install the
/// signal handlers.
pub fn run(config: DaemonConfig, options: &Options) -> Result<()> {
    let stop = stop_on_signals()?;
    let sleep = UntilStopped {
        sleep: ThreadSleep,
        stop: stop.clone(),
    };
    // A dry-run has no gate to name the adapter. Reading sysfs opens no device,
    // and failing here only means zeros.
    let dry_run_bus = options
        .dry_run
        .then(|| discover(SYSFS_ROOT.as_ref(), options.bus).ok()?.pci_bus())
        .flatten();

    run_with(
        config,
        Environment {
            connect: || session::connect_waiting(options, sleep.clone()),
            open_sensors: |pci_bus: Option<u32>| {
                let pci_bus = pci_bus.or(dry_run_bus).ok_or_else(|| {
                    Error::Adapter("cannot tell the GPU's PCI bus from the adapter name".into())
                })?;
                NvmlSensors::new(pci_bus)
            },
            clock: ThreadSleep,
            stop,
            dry_run: options.dry_run,
        },
    )
}

fn stop_on_signals() -> Result<Arc<AtomicBool>> {
    let stop = Arc::new(AtomicBool::new(false));
    for signal in [SIGTERM, SIGINT] {
        signal_hook::flag::register(signal, stop.clone()).map_err(|source| Error::Input {
            context: format!("installing a handler for signal {signal}"),
            source: source.into(),
        })?;
    }
    Ok(stop)
}

/// Runs the feed until `environment.stop` is set or `config.max_ticks` is used
/// up, which both return `Ok`.
///
/// # Errors
///
/// Only what waiting cannot cure, so that systemd shows the failure:
/// [`Error::Lock`], [`Error::AdapterRefused`], and [`Error::I2c`] for a permission
/// problem. Everything else is retried, since `i2c-dev` and the controller can
/// come up late at boot. In dry-run mode, any error.
pub fn run_with<T, W, S, C, F, K>(
    config: DaemonConfig,
    environment: Environment<C, F, K>,
) -> Result<()>
where
    T: Transport,
    W: Write,
    S: SensorSource,
    C: FnMut() -> Result<Session<T, W>>,
    F: FnMut(Option<u32>) -> Result<S>,
    K: Clock,
{
    let Environment {
        connect,
        open_sensors,
        clock,
        stop,
        dry_run,
    } = environment;
    Run {
        config,
        connect,
        sensors: SensorFeed::new(open_sensors),
        clock,
        stop,
        dry_run,
        backoff: Backoff::default(),
        connection: Problem::new("panel"),
        ticks: 0,
    }
    .run()
}

/// The open link to the panel, past `E1`.
enum Link<T, W> {
    Live(Unlocked<T>),
    DryRun(DryRun<W>),
}

impl<T: Transport, W: Write> Link<T, W> {
    /// Sends one `E3`. The lock is taken for this write and released on return.
    fn send(&mut self, sample: &SensorSample, feed: FeedFrameLen) -> Result<()> {
        match self {
            Self::Live(unlocked) => Panel::new(unlocked.lock()?).send_sensors(sample, feed),
            Self::DryRun(dry_run) => Panel::new(dry_run).send_sensors(sample, feed),
        }
    }
}

/// Why feeding ended.
enum Ended {
    /// Asked to stop, or out of ticks.
    Stopped,
    /// The bus failed, so the session is gone.
    BusLost,
}

/// Grows from [`BACKOFF_FIRST`] to [`BACKOFF_MAX`].
#[derive(Debug)]
struct Backoff {
    next: Duration,
}

impl Default for Backoff {
    fn default() -> Self {
        Self {
            next: BACKOFF_FIRST,
        }
    }
}

impl Backoff {
    fn next(&mut self) -> Duration {
        let wait = self.next;
        self.next = (wait * 2).min(BACKOFF_MAX);
        wait
    }

    fn reset(&mut self) {
        self.next = BACKOFF_FIRST;
    }
}

/// Logs a lasting problem when it changes, not on each of the attempts it can
/// take. At warning level both ways, so the journal shows when it ended.
#[derive(Debug)]
struct Problem {
    subject: &'static str,
    current: Option<String>,
}

impl Problem {
    fn new(subject: &'static str) -> Self {
        Self {
            subject,
            current: None,
        }
    }

    fn report(&mut self, error: &Error) {
        let text = error.to_string();
        if self.current.as_ref() != Some(&text) {
            log::warn!("{}: {text}", self.subject);
            self.current = Some(text);
        }
    }

    fn clear(&mut self) {
        if self.current.take().is_some() {
            log::warn!("{}: working again", self.subject);
        }
    }
}

/// The sensor source, if there is one, and when to try for one again.
struct SensorFeed<F, S> {
    open: F,
    source: Option<S>,
    retry_at: Option<Instant>,
    backoff: Backoff,
    problem: Problem,
}

impl<F, S> SensorFeed<F, S>
where
    F: FnMut(Option<u32>) -> Result<S>,
    S: SensorSource,
{
    fn new(open: F) -> Self {
        Self {
            open,
            source: None,
            retry_at: None,
            backoff: Backoff::default(),
            problem: Problem::new("sensors"),
        }
    }

    /// The current sample, or all zeros if the sensors cannot give one. A source
    /// that fails is dropped and created anew. The backoff resets on a good read
    /// only, so a source that opens but cannot read still backs off.
    fn sample(&mut self, now: Instant, pci_bus: Option<u32>) -> SensorSample {
        match self.read(now, pci_bus) {
            Some(Ok(sample)) => {
                self.backoff.reset();
                self.problem.clear();
                sample
            }
            Some(Err(error)) => {
                self.problem.report(&error);
                self.source = None;
                self.retry_at = Some(now + self.backoff.next());
                SensorSample::default()
            }
            None => SensorSample::default(),
        }
    }

    /// `None` while waiting for the next attempt.
    fn read(&mut self, now: Instant, pci_bus: Option<u32>) -> Option<Result<SensorSample>> {
        if self.source.is_none() {
            if self.retry_at.is_some_and(|retry_at| now < retry_at) {
                return None;
            }
            match (self.open)(pci_bus) {
                Ok(source) => self.source = Some(source),
                Err(error) => return Some(Err(error)),
            }
        }
        self.source.as_mut().map(SensorSource::read)
    }
}

/// A link to the panel and the PCI bus of its adapter, if the name shows one.
type Connection<T, W> = (Link<T, W>, Option<u32>);

/// The state of one [`run_with`].
struct Run<C, F, K, S> {
    config: DaemonConfig,
    connect: C,
    sensors: SensorFeed<F, S>,
    clock: K,
    stop: Arc<AtomicBool>,
    dry_run: bool,
    backoff: Backoff,
    connection: Problem,
    ticks: u64,
}

impl<T, W, S, C, F, K> Run<C, F, K, S>
where
    T: Transport,
    W: Write,
    S: SensorSource,
    C: FnMut() -> Result<Session<T, W>>,
    F: FnMut(Option<u32>) -> Result<S>,
    K: Clock,
{
    fn run(&mut self) -> Result<()> {
        // The first attempt goes at once. After a failure the bus gets a moment.
        let mut wait_first = false;
        while !self.out_of_ticks() {
            let Some((link, pci_bus)) = self.establish(wait_first)? else {
                break;
            };
            match self.feed(link, pci_bus)? {
                Ended::Stopped => break,
                Ended::BusLost => wait_first = true,
            }
        }
        Ok(())
    }

    /// Connects, however long it takes. `None` if asked to stop first.
    ///
    /// The backoff carries on from earlier sessions, and only a good `E3` resets
    /// it: a bus that passes the probe and then fails must not bring an `E1`
    /// every few seconds.
    fn establish(&mut self, mut wait_first: bool) -> Result<Option<Connection<T, W>>> {
        loop {
            if wait_first {
                let deadline = self.clock.now() + self.backoff.next();
                if !self.wait_until(deadline) {
                    return Ok(None);
                }
            }
            wait_first = true;
            if self.stopped() {
                return Ok(None);
            }
            match self.connect_once() {
                Ok(connection) => return Ok(connection),
                Err(error) if self.is_fatal(&error) => return Err(error),
                Err(Error::LockBusy { .. }) => log::debug!("an upload holds the bus"),
                Err(error) => self.connection.report(&error),
            }
        }
    }

    /// One pass through the gate, then the one `E1` of this connection, sent
    /// under the gate's lock so that an upload cannot slip in between.
    fn connect_once(&mut self) -> Result<Option<Connection<T, W>>> {
        let session = (self.connect)()?;
        // The gate can take seconds, and a shutdown sends nothing.
        if self.stopped() {
            return Ok(None);
        }
        let DaemonConfig {
            elements,
            interval_s,
            ..
        } = self.config;
        match session {
            Session::Live(mut verified) => {
                Panel::new(&mut verified).set_dashboard(elements, interval_s)?;
                let pci_bus = verified.adapter().pci_bus();
                Ok(Some((Link::Live(verified.unlock()), pci_bus)))
            }
            Session::DryRun(mut dry_run) => {
                Panel::new(&mut dry_run).set_dashboard(elements, interval_s)?;
                Ok(Some((Link::DryRun(dry_run), None)))
            }
        }
    }

    /// Ticks until the session ends. The first deadline is now: the panel has
    /// waited long enough.
    fn feed(&mut self, mut link: Link<T, W>, pci_bus: Option<u32>) -> Result<Ended> {
        let mut deadline = self.clock.now();
        let mut failures = 0;
        while !self.out_of_ticks() && self.wait_until(deadline) {
            let sample = self.sensors.sample(self.clock.now(), pci_bus);
            // A sensor read can stall, and a shutdown sends nothing.
            if self.stopped() {
                break;
            }
            self.ticks += 1;
            match link.send(&sample, self.config.feed) {
                Ok(()) => {
                    failures = 0;
                    self.backoff.reset();
                    self.connection.clear();
                }
                Err(Error::LockBusy { .. }) => log::debug!("an upload holds the bus: tick skipped"),
                Err(error) if self.is_fatal(&error) => return Err(error),
                Err(error) => {
                    failures += 1;
                    if failures == WRITE_ATTEMPTS {
                        self.connection.report(&error);
                        return Ok(Ended::BusLost);
                    }
                    log::debug!("write failed, trying again on the next tick: {error}");
                }
            }
            deadline = next_deadline(deadline, self.clock.now());
        }
        Ok(Ended::Stopped)
    }

    /// Waits for `deadline` in slices, so that a stop request is seen in time.
    /// `false` if one came first.
    fn wait_until(&mut self, deadline: Instant) -> bool {
        loop {
            if self.stopped() {
                return false;
            }
            let left = deadline.saturating_duration_since(self.clock.now());
            if left.is_zero() {
                return true;
            }
            self.clock.sleep(left.min(POLL_SLICE));
        }
    }

    fn stopped(&self) -> bool {
        self.stop.load(Ordering::SeqCst)
    }

    fn out_of_ticks(&self) -> bool {
        self.config.max_ticks.is_some_and(|max| self.ticks >= max)
    }

    fn is_fatal(&self, error: &Error) -> bool {
        self.dry_run || waiting_cannot_cure(error)
    }
}

/// The deadline after `previous`. If that one has passed already it is dropped,
/// not made up for: see the [module docs](self).
fn next_deadline(previous: Instant, now: Instant) -> Instant {
    let next = previous + TICK;
    if next > now {
        next
    } else {
        now + TICK
    }
}

/// The failures to report rather than retry. An adapter or device node that is
/// missing is not one of them: at boot either can show up late.
fn waiting_cannot_cure(error: &Error) -> bool {
    match error {
        Error::Lock { .. } | Error::AdapterRefused(_) => true,
        Error::I2c { source, .. } => Errno::from_io_error(source)
            .is_some_and(|errno| matches!(errno, Errno::ACCESS | Errno::PERM)),
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::panel::RecordingSleep;

    #[test]
    fn the_default_is_the_six_readme_widgets() {
        let config = DaemonConfig::default();

        assert_eq!(config.elements.bits(), 0xAF);
        assert_eq!(config.interval_s, 3);
        assert_eq!(config.feed, FeedFrameLen::Full);
        assert_eq!(config.max_ticks, None);
    }

    #[test]
    fn backoff_doubles_to_the_cap_and_resets() {
        let mut backoff = Backoff::default();

        let waits: Vec<u64> = (0..8).map(|_| backoff.next().as_secs()).collect();
        assert_eq!(waits, [2, 4, 8, 16, 32, 60, 60, 60]);

        backoff.reset();
        assert_eq!(backoff.next(), BACKOFF_FIRST);
    }

    #[test]
    fn a_passed_deadline_is_dropped() {
        let start = Instant::now();
        let at = |millis| start + Duration::from_millis(millis);

        assert_eq!(next_deadline(at(0), at(30)), at(1000));
        assert_eq!(next_deadline(at(0), at(999)), at(1000));
        assert_eq!(next_deadline(at(0), at(1000)), at(2000));
        assert_eq!(next_deadline(at(0), at(61_234)), at(62_234));
    }

    #[test]
    fn a_sleep_is_cut_into_slices() {
        let stop = Arc::new(AtomicBool::new(false));
        let mut sleep = UntilStopped {
            sleep: RecordingSleep::default(),
            stop,
        };

        sleep.sleep(Duration::from_millis(250));

        let slices: Vec<u128> = sleep.sleep.delays.iter().map(Duration::as_millis).collect();
        assert_eq!(slices, [100, 100, 50]);
    }

    #[test]
    fn a_stop_request_ends_a_sleep_without_waiting() {
        let stop = Arc::new(AtomicBool::new(true));
        let mut sleep = UntilStopped {
            sleep: RecordingSleep::default(),
            stop,
        };

        sleep.sleep(Duration::from_secs(5));

        assert!(sleep.sleep.delays.is_empty());
    }

    #[test]
    fn only_lock_and_permission_errors_are_fatal() {
        let i2c = |errno: Errno| Error::I2c {
            context: "test".into(),
            source: errno.into(),
        };

        assert!(waiting_cannot_cure(&i2c(Errno::ACCESS)));
        assert!(waiting_cannot_cure(&i2c(Errno::PERM)));
        assert!(waiting_cannot_cure(&Error::AdapterRefused("SMBus".into())));
        assert!(waiting_cannot_cure(&Error::Lock {
            path: "/run/test.lock".into(),
            source: Errno::ACCESS.into(),
        }));

        assert!(!waiting_cannot_cure(&i2c(Errno::NOENT)));
        assert!(!waiting_cannot_cure(&i2c(Errno::IO)));
        assert!(!waiting_cannot_cure(&i2c(Errno::NXIO)));
        assert!(!waiting_cannot_cure(&Error::Adapter("none yet".into())));
        assert!(!waiting_cannot_cure(&Error::LockBusy {
            path: "/run/test.lock".into(),
        }));
        assert!(!waiting_cannot_cure(&Error::ShortRead {
            expected: 8,
            actual: 0,
        }));
    }
}
