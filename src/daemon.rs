//! The background loop that keeps the panel's sensor feed alive.
//!
//! The panel freezes or blanks its widgets without a regular `E3`, so the goal
//! here is that the feed outlives everything that can go wrong around it.
//! [`run_with`] is a state machine over injected parts, which is how every
//! failure below is tested on a fake clock:
//!
//! - **Connect:** through the [write gate](crate::session), then `DF`, then at
//!   most one `E1`, then `E3` until the bus fails. `EB 03`, `DF`, that `E1` and
//!   `E3` are all this module sends: `AA` writes NVRAM. See below for when `E1`
//!   goes out.
//! - **Ticks** run on deadlines, each the previous one plus [`TICK`], so the
//!   time spent working does not add up. A missed deadline is dropped rather
//!   than made up for: late frames carry stale values, and a burst of them is
//!   all that a catch-up could add.
//! - **The lock** is held for one write only and sensors are read without it,
//!   so an upload can take the bus between any two ticks. If an upload holds it,
//!   the tick is skipped. That is normal operation: no error and no backoff.
//! - **Failed writes** are retried on the next tick. See below. The adaptive
//!   feed has no next tick to retry on, as [its section](self#the-adaptive-feed)
//!   explains.
//! - **Sensors** failing never stop the feed, which then shows zeros. A zero
//!   reads as a failure, where a repeated value would look live.
//! - **Shutdown** sends nothing: the panel is left exactly as it is.
//!
//! # When `E1` goes out
//!
//! `E1` is the prime suspect in dark-panel incidents (`RECOVERY.md`), and the
//! panel keeps its widget config across boots: `DF` read all eight widgets every
//! 4 s on a panel this tool had never written to. So each connection reads `DF`
//! and sends `E1` only for a reason:
//!
//! - If the config names widgets, when the panel's widgets or interval differ.
//! - If it names none, when the panel has no widget enabled, and then the
//!   README's defaults. With any widget enabled, nothing about the panel's
//!   selection or interval is touched.
//!
//! A failed `DF` says nothing about the panel, so it never leads to a blind `E1`.
//! It fails the connection like any other session step, and takes the normal
//! backoff. A dry-run has no read-backs: it prints the `DF` query and the `E1` a
//! real run might send, and says that a real run decides.
//!
//! # Why a failed write waits for the next tick
//!
//! The GPU's I2C engine sometimes rejects a valid transfer on a healthy bus, and
//! reconnecting over each of those would mean a `DF` and maybe an `E1` every
//! time, so writes need a retry. [`Retrying`](crate::bus::transport::Retrying) does not fit a tick: it
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
//! # Why the feed defaults to adaptive
//!
//! The panel needs `E3`, but each one is an I2C write while the user may be
//! gaming. Upstream (RTX 5080, Windows) measured 1% lows of about 141 fps with a
//! 256-byte 1 Hz feed against about 182 with almost no writes, from writes of
//! 9.8 ms. On the RTX 5090 here (firmware 1.3, driver 615.78.08) a 256-byte `E3`
//! takes 28 ms on average and 38 ms at most, which fits a 100 kHz bus. With the
//! feed off, other NVIDIA driver calls showed no stall over 10 ms in 60 s. With a
//! 1 Hz feed they stalled once per second, for up to 35 ms. That stall is a proxy,
//! measured as `nvmlDeviceGetMemoryInfo` latency. It is not a measured game frame
//! drop, and what the feed costs in frame rate on this Linux driver is unknown.
//!
//! So [`DaemonConfig::strategy`] defaults to [`FeedStrategy::Adaptive`], which
//! writes less while the GPU is busy, and [`FeedStrategy::Steady`] stays available.
//! [`DaemonConfig::feed`] stays at `FeedFrameLen::Full`. 128 bytes is less time on
//! the bus, but on this card it is unreliable: 21 of 163 writes were refused,
//! against 1 of 162 at 256 bytes, and 5 of 12 `status` polls failed during the feed.
//!
//! To judge changes here, every `E3` write is timed from after the lock is taken
//! until the write returns: logged at debug level, and as count, mean and max at
//! info level every [`WRITE_COST_REPORT_TICKS`] ticks. Only writes that went
//! through count towards the summary.
//!
//! # The adaptive feed
//!
//! [`FeedStrategy::Adaptive`] polls the sensors on every tick regardless, since
//! NVML reads cost nothing. Below [`ADAPTIVE_BUSY_PERCENT`] GPU utilization it
//! sends each sample, as [`FeedStrategy::Steady`] does. From there up it collects
//! [`ADAPTIVE_WINDOW_POLLS`] polls and sends a summary of their peaks and averages
//! twice: the first on the last poll of the window, the second
//! [`ADAPTIVE_PAIR_GAP_TICKS`] later. Upstream found that a frame only displays
//! once the next one arrives within about 1 s, so a lone frame in a sparse feed
//! would never show. That makes 2 writes every 6 seconds under load, and gaps of
//! 5 seconds that the panel has to tolerate. It does: on a 5090 the widgets stay
//! visible and update about every 6 s, confirmed by eye.
//!
//! The schedule decides this from the polls alone and never hears how a write
//! went, so nothing that happens on the bus can move the schedule:
//!
//! - **A half of a pair that fails or is skipped** is not sent again. The other
//!   half goes on its own tick, and the next summary comes when it is due. A
//!   retry would be the extra write under load that this feed exists to avoid.
//!   At worst the panel shows the previous values for one more cycle.
//! - **The failed-write count** works as ever, across the silent ticks, so three
//!   failures in a row end the session. That can span 6 seconds rather than 2.
//! - **A poll under the threshold** drops any partial window and any half of a
//!   pair still due, and that sample goes out on that tick. A summary of a window
//!   cut short would present a few polls as the whole, and the load it describes
//!   has ended. In a pair, the live frame commits the first half just the same.
//! - **A new session** starts with an empty window: its polls would be stale.
//!
//! A dry-run reads the real sensors and lasts three ticks, so on a busy GPU it can
//! end before the first summary. Its first silent tick prints a note, because
//! output with no `E3` at all would look like a broken feed.
//!
//! # Suspend
//!
//! [`Instant`] does not advance during suspend on Linux, so resuming looks like
//! nothing happened until a write fails, and then the normal reconnect applies.
//! Should the clock jump anyway, the deadline rule covers it.

use std::io::Write;
use std::str::FromStr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use rustix::io::Errno;
use signal_hook::consts::{SIGINT, SIGTERM};

use crate::bus::discovery::{discover, SYSFS_ROOT};
use crate::bus::transport::{DryRun, Transport};
use crate::error::{Error, Result};
use crate::panel::{Panel, Sleep, ThreadSleep};
use crate::protocol::{
    parse_widgets, DisplayStatus, Elements, FeedFrameLen, SensorSample, DEFAULT_WIDGETS,
};
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

/// The GPU utilization in percent from which [`FeedStrategy::Adaptive`] sends
/// summaries instead of every sample.
pub const ADAPTIVE_BUSY_PERCENT: u32 = 50;

/// The polls that one summary of [`FeedStrategy::Adaptive`] covers.
pub const ADAPTIVE_WINDOW_POLLS: usize = 5;

/// Ticks between the two frames of a summary. The panel shows a frame once the
/// next arrives within about 1 s, so this cannot grow. A poll in this gap only
/// counts towards the threshold, not towards the next window.
pub const ADAPTIVE_PAIR_GAP_TICKS: u32 = 1;

/// Ticks between two summaries of the write cost at info level.
pub const WRITE_COST_REPORT_TICKS: u64 = 60;

/// How often `E3` goes out. See the [module docs](self#the-adaptive-feed).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum FeedStrategy {
    /// One frame per tick, the README's spec.
    Steady,
    /// One frame per tick under low load, a summary as a pair of frames under
    /// high load.
    #[default]
    Adaptive,
}

impl FromStr for FeedStrategy {
    type Err = Error;

    fn from_str(name: &str) -> Result<Self> {
        match name {
            "steady" => Ok(Self::Steady),
            "adaptive" => Ok(Self::Adaptive),
            _ => Err(Error::InvalidArgument(format!(
                "a feed strategy must be steady or adaptive, not {name}"
            ))),
        }
    }
}

/// What the daemon shows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DaemonConfig {
    /// The widgets the config names. `None` leaves the panel's own selection
    /// alone unless it has no widgets: see the [module docs](self).
    pub elements: Option<Elements>,
    /// The widget rotation interval `E1` sets, in seconds. With no widgets named,
    /// this only applies when a panel with no widgets gets the defaults.
    pub interval_s: u8,
    /// The length of each `E3`.
    pub feed: FeedFrameLen,
    /// How often an `E3` goes out.
    pub strategy: FeedStrategy,
    /// Stops after this many ticks. Only tests and dry-run set this.
    pub max_ticks: Option<u64>,
}

impl Default for DaemonConfig {
    /// Names no widgets, so the panel keeps what it has.
    fn default() -> Self {
        Self {
            elements: None,
            interval_s: 3,
            feed: FeedFrameLen::Full,
            strategy: FeedStrategy::Adaptive,
            max_ticks: None,
        }
    }
}

impl DaemonConfig {
    /// What `E1` carries if it goes out: the named widgets, or else the README's
    /// six defaults.
    pub fn dashboard(&self) -> DisplayStatus {
        DisplayStatus {
            elements: self.elements.unwrap_or_else(|| {
                parse_widgets(DEFAULT_WIDGETS).expect("the default widgets are valid")
            }),
            interval_s: self.interval_s,
        }
    }

    /// What `E1` should carry for a panel showing `panel`, or `None` if it should
    /// not be sent. See the [module docs](self).
    fn update_for(&self, panel: DisplayStatus) -> Option<DisplayStatus> {
        let wanted = self.dashboard();
        let needed = match self.elements {
            Some(_) => panel != wanted,
            None => panel.elements == Elements::NONE,
        };
        needed.then_some(wanted)
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

/// What a dry-run prints between the `DF` query and `E1`.
const WOULD_DECIDE: &str =
    "dry-run: read-backs are unavailable, so E1 is shown; a real run decides from DF";

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
        write_cost: WriteCost::default(),
    }
    .run()
}

/// The open link to the panel, past the point where `E1` can go out.
enum Link<T, W> {
    Live(Unlocked<T>),
    DryRun(DryRun<W>),
}

impl<T: Transport, W: Write> Link<T, W> {
    /// Sends one `E3`. The lock is taken for this write and released on return.
    /// The time goes to `cost` without the lock around it, and only for the real
    /// bus: a dry-run would time a print.
    fn send(
        &mut self,
        sample: &SensorSample,
        feed: FeedFrameLen,
        clock: &impl Clock,
        cost: &mut WriteCost,
    ) -> Result<()> {
        match self {
            Self::Live(unlocked) => {
                let bus = unlocked.lock()?;
                let started = clock.now();
                let result = Panel::new(bus).send_sensors(sample, feed);
                let took = clock.now().saturating_duration_since(started);
                cost.record(took, feed, result.is_ok());
                result
            }
            Self::DryRun(dry_run) => Panel::new(dry_run).send_sensors(sample, feed),
        }
    }

    /// Says why a tick shows no frame. Only a dry-run has anyone to tell.
    fn explain_silent_tick(&mut self) -> Result<()> {
        match self {
            Self::Live(_) => Ok(()),
            Self::DryRun(dry_run) => dry_run.note(&format!(
                "dry-run: GPU usage is {ADAPTIVE_BUSY_PERCENT}% or above, so the adaptive feed \
                 is waiting for a full window and its next frame comes after \
                 {ADAPTIVE_WINDOW_POLLS} polls; --feed steady shows one per tick"
            )),
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

/// Decides from each poll what the tick sends, if anything. One lives for one
/// session. See the [module docs](self#the-adaptive-feed).
#[derive(Debug)]
struct FeedSchedule {
    strategy: FeedStrategy,
    window: Vec<SensorSample>,
    second_half: Option<SecondHalf>,
}

/// The frame of a pair that is still to go out.
#[derive(Debug, Clone, Copy)]
struct SecondHalf {
    summary: SensorSample,
    ticks_left: u32,
}

impl FeedSchedule {
    fn new(strategy: FeedStrategy) -> Self {
        Self {
            strategy,
            window: Vec::with_capacity(ADAPTIVE_WINDOW_POLLS),
            second_half: None,
        }
    }

    /// Takes this tick's poll and returns what to send on this tick. It takes no
    /// outcome of a write, which is what keeps the schedule off the bus's state.
    fn frame_for(&mut self, sample: SensorSample) -> Option<SensorSample> {
        if self.strategy == FeedStrategy::Steady || sample.gpu_usage_percent < ADAPTIVE_BUSY_PERCENT
        {
            self.window.clear();
            self.second_half = None;
            return Some(sample);
        }
        if let Some(mut half) = self.second_half.take() {
            half.ticks_left -= 1;
            if half.ticks_left == 0 {
                return Some(half.summary);
            }
            self.second_half = Some(half);
            return None;
        }
        self.window.push(sample);
        if self.window.len() < ADAPTIVE_WINDOW_POLLS {
            return None;
        }
        let summary = summarize(&self.window);
        self.window.clear();
        self.second_half = Some(SecondHalf {
            summary,
            ticks_left: ADAPTIVE_PAIR_GAP_TICKS,
        });
        Some(summary)
    }
}

/// One frame for a window of polls: the peaks of what can hurt the card, the
/// average of what varies with the load, and the latest RAM usage, which only
/// ever drifts. NVML has no FPS, so that stays 0.
fn summarize(window: &[SensorSample]) -> SensorSample {
    let max = |field: fn(&SensorSample) -> u32| window.iter().map(field).max().unwrap_or(0);
    let average = |field: fn(&SensorSample) -> u32| rounded_mean(window.iter().map(field));
    SensorSample {
        gpu_temp_c: max(|sample| sample.gpu_temp_c),
        gpu_clock_mhz: average(|sample| sample.gpu_clock_mhz),
        gpu_usage_percent: average(|sample| sample.gpu_usage_percent),
        fan_speed: max(|sample| sample.fan_speed),
        ram_clock_mhz: average(|sample| sample.ram_clock_mhz),
        ram_usage_percent: window.last().map_or(0, |sample| sample.ram_usage_percent),
        fps: 0,
        tgp_watts: max(|sample| sample.tgp_watts),
    }
}

/// The mean to the nearest whole number. An empty list gives 0.
fn rounded_mean(values: impl ExactSizeIterator<Item = u32>) -> u32 {
    let count = values.len() as u64;
    if count == 0 {
        return 0;
    }
    // Sums in u64, since sensor values are as wide as the type: see `SensorSample`.
    let sum: u64 = values.map(u64::from).sum();
    u32::try_from((sum + count / 2) / count).unwrap_or(u32::MAX)
}

/// What `E3` writes have cost since the last report.
#[derive(Debug, Default, PartialEq, Eq)]
struct WriteCost {
    count: u32,
    total: Duration,
    max: Duration,
}

impl WriteCost {
    /// Logs one attempt. A failed one says little about the cost of a write, as a
    /// transfer can fail at any point, so it stays out of the summary.
    fn record(&mut self, took: Duration, feed: FeedFrameLen, ok: bool) {
        let bytes = feed.bytes();
        if !ok {
            log::debug!("E3 write of {bytes} bytes failed after {took:.1?}");
            return;
        }
        log::debug!("E3 write of {bytes} bytes took {took:.1?}");
        self.count += 1;
        self.total += took;
        self.max = self.max.max(took);
    }

    /// Logs the summary and starts over. Says nothing if nothing was written, as
    /// when an upload had the bus for the whole period.
    fn report(&mut self) {
        let Self { count, total, max } = std::mem::take(self);
        if count > 0 {
            let mean = total / count;
            log::info!("E3 write cost: count {count}, mean {mean:.1?}, max {max:.1?}");
        }
    }
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
    write_cost: WriteCost,
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

    /// One pass through the gate, then `DF` and any `E1` of this connection, under
    /// the gate's lock so that an upload cannot slip in between. A failed `DF`
    /// returns from here like any other error, so the caller backs off.
    fn connect_once(&mut self) -> Result<Option<Connection<T, W>>> {
        let session = (self.connect)()?;
        // The gate can take seconds, and a shutdown sends nothing.
        if self.stopped() {
            return Ok(None);
        }
        match session {
            Session::Live(mut verified) => {
                if !self.configure(&mut verified)? {
                    return Ok(None);
                }
                let pci_bus = verified.adapter().pci_bus();
                Ok(Some((Link::Live(verified.unlock()), pci_bus)))
            }
            Session::DryRun(mut dry_run) => {
                self.preview(&mut dry_run)?;
                Ok(Some((Link::DryRun(dry_run), None)))
            }
        }
    }

    /// Reads `DF` and sends `E1` if the panel needs it. `false` if asked to stop.
    fn configure(&self, transport: &mut impl Transport) -> Result<bool> {
        let mut panel = Panel::new(transport);
        let shown = panel.display()?;
        let update = self.config.update_for(shown);
        log_decision(shown, update);
        // The read took time, and a shutdown sends nothing.
        if self.stopped() {
            return Ok(false);
        }
        if let Some(wanted) = update {
            panel.set_dashboard(wanted.elements, wanted.interval_s)?;
        }
        Ok(true)
    }

    /// Shows what [`Self::configure`] might send. There is no reply to decide
    /// from, but a failure to print still ends the run.
    fn preview(&self, dry_run: &mut DryRun<W>) -> Result<()> {
        match Panel::new(&mut *dry_run).display() {
            Err(Error::ReadbackUnavailable) => {}
            other => drop(other?),
        }
        dry_run.note(WOULD_DECIDE)?;
        let wanted = self.config.dashboard();
        Panel::new(dry_run).set_dashboard(wanted.elements, wanted.interval_s)
    }

    /// Ticks until the session ends. The first deadline is now: the panel has
    /// waited long enough.
    fn feed(&mut self, mut link: Link<T, W>, pci_bus: Option<u32>) -> Result<Ended> {
        let mut deadline = self.clock.now();
        let mut failures = 0;
        let mut schedule = FeedSchedule::new(self.config.strategy);
        let mut explained = false;
        while !self.out_of_ticks() && self.wait_until(deadline) {
            let sample = self.sensors.sample(self.clock.now(), pci_bus);
            // A sensor read can stall, and a shutdown sends nothing.
            if self.stopped() {
                break;
            }
            self.ticks += 1;
            let frame = schedule.frame_for(sample);
            if frame.is_none() && !explained {
                link.explain_silent_tick()?;
                explained = true;
            }
            if let Some(frame) = frame {
                let clock = &self.clock;
                match link.send(&frame, self.config.feed, clock, &mut self.write_cost) {
                    Ok(()) => {
                        failures = 0;
                        self.backoff.reset();
                        self.connection.clear();
                    }
                    Err(Error::LockBusy { .. }) => {
                        log::debug!("an upload holds the bus: tick skipped");
                    }
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
            }
            if self.ticks.is_multiple_of(WRITE_COST_REPORT_TICKS) {
                self.write_cost.report();
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

/// Logs what the panel has and what happens to it.
fn log_decision(shown: DisplayStatus, update: Option<DisplayStatus>) {
    let (mask, interval) = (shown.elements.bits(), shown.interval_s);
    match update {
        Some(wanted) => log::info!(
            "panel has widgets 0x{mask:02X} every {interval} s: sending E1 with 0x{:02X} every {} s",
            wanted.elements.bits(),
            wanted.interval_s
        ),
        None => log::info!("panel has widgets 0x{mask:02X} every {interval} s: no E1 needed"),
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
    fn the_default_names_no_widgets_and_falls_back_to_the_six_readme_ones() {
        let config = DaemonConfig::default();

        assert_eq!(config.elements, None);
        assert_eq!(config.dashboard().elements.bits(), 0xAF);
        assert_eq!(config.dashboard().interval_s, 3);
        assert_eq!(config.feed, FeedFrameLen::Full);
        assert_eq!(config.strategy, FeedStrategy::Adaptive);
        assert_eq!(config.max_ticks, None);
    }

    #[test]
    fn the_feed_defaults_to_adaptive_at_256_bytes() {
        assert_eq!(FeedStrategy::default(), FeedStrategy::Adaptive);
        assert_eq!(FeedFrameLen::default(), FeedFrameLen::Full);

        let config = DaemonConfig::default();

        assert_eq!(config.strategy, FeedStrategy::Adaptive);
        assert_eq!(config.feed.bytes(), 256);
    }

    fn shown(mask: u8, interval_s: u8) -> DisplayStatus {
        DisplayStatus {
            elements: Elements::from_bits(mask),
            interval_s,
        }
    }

    fn naming(mask: u8) -> DaemonConfig {
        DaemonConfig {
            elements: Some(Elements::from_bits(mask)),
            ..DaemonConfig::default()
        }
    }

    #[test]
    fn named_widgets_must_match_in_mask_and_interval() {
        let config = naming(0x05);

        assert_eq!(config.update_for(shown(0x05, 3)), None);
        assert_eq!(config.update_for(shown(0x07, 3)), Some(shown(0x05, 3)));
        assert_eq!(config.update_for(shown(0x05, 4)), Some(shown(0x05, 3)));
        assert_eq!(config.update_for(shown(0x00, 0)), Some(shown(0x05, 3)));
    }

    #[test]
    fn naming_an_empty_list_is_still_a_named_config() {
        let config = naming(0x00);

        assert_eq!(config.update_for(shown(0x00, 3)), None);
        assert_eq!(config.update_for(shown(0xFF, 4)), Some(shown(0x00, 3)));
    }

    #[test]
    fn without_named_widgets_any_enabled_widget_is_left_alone() {
        let config = DaemonConfig::default();

        for panel in [
            shown(0xFF, 4),
            shown(0x01, 3),
            shown(0x80, 0),
            shown(0xAF, 200),
        ] {
            assert_eq!(config.update_for(panel), None, "{panel:?}");
        }
    }

    #[test]
    fn without_named_widgets_a_panel_with_none_gets_the_defaults() {
        let config = DaemonConfig::default();

        for interval_s in [0, 3, 200] {
            let update = config.update_for(shown(0x00, interval_s));
            assert_eq!(update, Some(shown(0xAF, 3)));
        }
    }

    #[test]
    fn a_feed_strategy_is_steady_or_adaptive_and_nothing_else() {
        assert_eq!(
            "steady".parse::<FeedStrategy>().unwrap(),
            FeedStrategy::Steady
        );
        assert_eq!(
            "adaptive".parse::<FeedStrategy>().unwrap(),
            FeedStrategy::Adaptive
        );

        for name in ["", "Adaptive", "burst"] {
            let error = name.parse::<FeedStrategy>().unwrap_err();
            assert!(matches!(error, Error::InvalidArgument(_)), "{error:?}");
        }
    }

    fn polling(usage: u32) -> SensorSample {
        SensorSample {
            gpu_usage_percent: usage,
            ..SensorSample::default()
        }
    }

    #[test]
    fn the_window_summary_takes_peaks_averages_and_the_latest_ram_usage() {
        let sample = |temp, clock, usage, fan, ram_clock, ram, power| SensorSample {
            gpu_temp_c: temp,
            gpu_clock_mhz: clock,
            gpu_usage_percent: usage,
            fan_speed: fan,
            ram_clock_mhz: ram_clock,
            ram_usage_percent: ram,
            fps: 144,
            tgp_watts: power,
        };
        let window = [
            sample(60, 2000, 90, 1500, 14_000, 41, 300),
            sample(75, 2100, 80, 1400, 14_000, 42, 450),
            sample(70, 2200, 70, 1800, 14_002, 43, 400),
        ];

        assert_eq!(
            summarize(&window),
            SensorSample {
                gpu_temp_c: 75,
                gpu_clock_mhz: 2100,
                gpu_usage_percent: 80,
                fan_speed: 1800,
                ram_clock_mhz: 14_001,
                ram_usage_percent: 43,
                fps: 0,
                tgp_watts: 450,
            }
        );
    }

    #[test]
    fn a_mean_rounds_to_the_nearest_and_cannot_overflow() {
        let mean = |values: &[u32]| rounded_mean(values.iter().copied());

        assert_eq!(mean(&[]), 0);
        assert_eq!(mean(&[1, 2]), 2);
        assert_eq!(mean(&[1, 1, 2]), 1);
        assert_eq!(mean(&[u32::MAX; 5]), u32::MAX);
    }

    #[test]
    fn steady_sends_every_sample_whatever_the_load() {
        let mut schedule = FeedSchedule::new(FeedStrategy::Steady);

        for usage in [0, 49, 50, 100, 100, 100, 100, 100, 100, 3] {
            assert_eq!(schedule.frame_for(polling(usage)), Some(polling(usage)));
        }
    }

    #[test]
    fn adaptive_cycles_through_a_window_and_a_pair_under_load() {
        let mut schedule = FeedSchedule::new(FeedStrategy::Adaptive);

        let sent: Vec<bool> = (0..12)
            .map(|_| schedule.frame_for(polling(50)).is_some())
            .collect();

        let cycle = [false, false, false, false, true, true];
        assert_eq!(sent, [cycle, cycle].concat());
    }

    #[test]
    fn the_write_cost_summary_covers_good_writes_and_starts_over() {
        let mut cost = WriteCost::default();
        let feed = FeedFrameLen::Full;

        cost.record(Duration::from_millis(10), feed, true);
        cost.record(Duration::from_millis(500), feed, false);
        cost.record(Duration::from_millis(30), feed, true);

        assert_eq!(
            cost,
            WriteCost {
                count: 2,
                total: Duration::from_millis(40),
                max: Duration::from_millis(30),
            }
        );
        cost.report();
        assert_eq!(cost, WriteCost::default());
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
