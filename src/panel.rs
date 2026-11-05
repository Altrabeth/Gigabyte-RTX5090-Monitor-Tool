//! High-level panel operations, built on a [`Transport`].
//!
//! [`Panel`] turns one operation into its frames, using the builders and parsers
//! in [`protocol`](crate::protocol), and adds no policy: no retry, no fallback
//! and no implicit frame. A failed transfer ends the operation at once, with
//! nothing further written. To retry, wrap the link in
//! [`Retrying`](crate::bus::transport::Retrying).
//!
//! # Over which link
//!
//! Building a [`Panel`] does no I/O, so it can live exactly as long as the
//! caller's lock does. A command keeps one for its whole run, over a
//! [`Session`](crate::session::Session) or a
//! [`Verified`](crate::session::Verified). The daemon builds one per tick, over
//! the short-lived lock of [`Unlocked::lock`], and the lock goes when the panel
//! does:
//!
//! ```
//! use aorus_lcd::bus::transport::Transport;
//! use aorus_lcd::panel::Panel;
//! use aorus_lcd::protocol::{FeedFrameLen, SensorSample};
//! use aorus_lcd::session::Unlocked;
//! use aorus_lcd::{Error, Result};
//!
//! fn tick<T: Transport>(session: &mut Unlocked<T>, sample: &SensorSample) -> Result<()> {
//!     match session.lock() {
//!         Ok(bus) => Panel::new(bus).send_sensors(sample, FeedFrameLen::Full),
//!         // An upload holds the bus: skip this tick.
//!         Err(Error::LockBusy { .. }) => Ok(()),
//!         Err(error) => Err(error),
//!     }
//! }
//! ```
//!
//! An [`Unlocked`] is no [`Transport`], so a panel cannot skip that lock:
//!
//! ```compile_fail,E0277
//! use aorus_lcd::bus::transport::Transport;
//! use aorus_lcd::panel::Panel;
//! use aorus_lcd::session::Unlocked;
//! fn turn_on_without_the_lock<T: Transport>(session: Unlocked<T>) {
//!     Panel::new(session).on().unwrap();
//! }
//! ```
//!
//! # Uploads
//!
//! [`Panel::upload`] writes under the firmware's pacing, which makes an image
//! take about 6 seconds and a GIF longer. Three things stay with the caller, so
//! that `Panel` remains generic over its link:
//!
//! - Retries: wrap the link in [`Retrying`](crate::bus::transport::Retrying). A
//!   transfer the GPU's I2C engine rejects should not cost the whole upload.
//! - The lock: a [`Verified`](crate::session::Verified) holds it for its whole
//!   life, so running the upload on one keeps the daemon off the bus for every
//!   pause. Do not use the per-write lock of [`Unlocked::lock`] here.
//! - The clock: pass [`ThreadSleep`] for a real upload. A dry run has no firmware
//!   to wait for, so pass [`NoSleep`] there.
//!
//! ```
//! use std::io::Write;
//!
//! use aorus_lcd::bus::transport::{Retrying, Transport};
//! use aorus_lcd::panel::{NoSleep, Panel, Sleep, ThreadSleep, UploadOptions};
//! use aorus_lcd::protocol::{Framebuffer, UploadParams};
//! use aorus_lcd::session::Session;
//! use aorus_lcd::Result;
//!
//! fn show_image<T: Transport, W: Write>(session: Session<T, W>, payload: &[u8]) -> Result<()> {
//!     let mut clock: Box<dyn Sleep> = match session {
//!         Session::Live(_) => Box::new(ThreadSleep),
//!         Session::DryRun(_) => Box::new(NoSleep),
//!     };
//!     Panel::new(Retrying::new(session)).upload(
//!         payload,
//!         &UploadParams::still(Framebuffer::Static),
//!         UploadOptions::default(),
//!         &mut *clock,
//!         |progress| log::debug!("{} of {}", progress.sent, progress.total),
//!     )
//! }
//! ```
//!
//! [`Unlocked`]: crate::session::Unlocked
//! [`Unlocked::lock`]: crate::session::Unlocked::lock

use std::time::Duration;

use crate::bus::transport::Transport;
use crate::error::{Error, Result};
use crate::protocol::{
    build_upload, carousel_frame, display_frame, display_query_frame, firmware_query_frame,
    loop_query_frame, mode_frame, mode_query_frame, parse_display, parse_firmware, parse_loop_bank,
    parse_mode, power_frame, save_frame, sensor_frame, DisplayStatus, Elements, FeedFrameLen,
    FirmwareVersion, Frame, Framebuffer, LoopBank, Mode, ModeStatus, SensorSample, UploadFrame,
    UploadFrameKind, UploadParams, LOOP_BANK_FIRST, LOOP_BANK_LAST, LOOP_READ_LEN, STATUS_READ_LEN,
};

/// Pause after the `F2` BEGIN marker. `PACE_BEGIN` in `aorus_lcd.py`.
const PACE_BEGIN: Duration = Duration::from_millis(500);

/// Pause after the `F1` header. `PACE_HEADER` in `aorus_lcd.py`.
const PACE_HEADER: Duration = Duration::from_millis(1000);

/// Default pause after each chunk and after the `F2` END marker. `PACE_CHUNK` in
/// `aorus_lcd.py`, `DefaultChunkDelayMs` in `PanelController.cs`.
pub const DEFAULT_CHUNK_DELAY: Duration = Duration::from_millis(10);

/// Pause between `SetMode` and the first frame of a GIF. `upload_content` in
/// `aorus_lcd.py`, `GifModeSettleMs` in `PanelController.cs`.
const GIF_MODE_SETTLE: Duration = Duration::from_millis(200);

/// Waits for a while. All delays in this crate go through this trait, because
/// the firmware's pacing has to be asserted in tests without real waiting.
pub trait Sleep {
    /// Blocks for `duration`.
    fn sleep(&mut self, duration: Duration);
}

/// The real clock, for production use.
#[derive(Debug, Default, Clone, Copy)]
pub struct ThreadSleep;

impl Sleep for ThreadSleep {
    fn sleep(&mut self, duration: Duration) {
        std::thread::sleep(duration);
    }
}

/// A clock that never waits, for dry-run uploads, which would otherwise sit
/// through the firmware's pacing with no firmware listening.
#[derive(Debug, Default, Clone, Copy)]
pub struct NoSleep;

impl Sleep for NoSleep {
    fn sleep(&mut self, _duration: Duration) {}
}

/// Test double: returns at once and records every requested delay in order.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct RecordingSleep {
    /// The delays requested so far.
    pub delays: Vec<Duration>,
}

impl Sleep for RecordingSleep {
    fn sleep(&mut self, duration: Duration) {
        self.delays.push(duration);
    }
}

/// One carousel bank as `F4` returned it.
///
/// `raw` is the record. The slot offsets are inferred and have not been seen on
/// real hardware, so interpreting them is a separate step, which can be
/// corrected without losing what the panel sent.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LoopBankRead {
    /// The bank number, 1 to 5.
    pub bank: u8,
    /// The bytes read back.
    pub raw: [u8; LOOP_READ_LEN],
}

impl LoopBankRead {
    /// The bank as [`parse_loop_bank`] understands it.
    pub fn parsed(&self) -> Result<LoopBank> {
        parse_loop_bank(&self.raw)
    }
}

/// Everything the panel reports, from one [`Panel::status`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PanelStatus {
    /// The firmware version.
    pub firmware: FirmwareVersion,
    /// The mode and on-state.
    pub mode: ModeStatus,
    /// The dashboard configuration.
    pub display: DisplayStatus,
    /// The carousel banks, 1 to 5.
    pub loop_banks: Vec<LoopBankRead>,
}

/// How to run an upload. The default is what both references do.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct UploadOptions {
    /// The pause after each chunk and after the `F2` END marker.
    pub chunk_delay: Duration,
    /// Whether to select the content's mode. When this is off the frames are
    /// stored but stay out of sight, and there is no `SetMode` to order.
    pub switch_mode: bool,
}

impl Default for UploadOptions {
    fn default() -> Self {
        Self {
            chunk_delay: DEFAULT_CHUNK_DELAY,
            switch_mode: true,
        }
    }
}

/// How far an upload has got, reported after each frame is written.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct UploadProgress {
    /// Frames written so far, counting the markers and the header.
    pub sent: usize,
    /// Frames in the whole upload.
    pub total: usize,
}

/// The panel's operations over a link. See the [module docs](self).
///
/// Errors from the link are returned as they are. In dry-run that includes
/// [`Error::ReadbackUnavailable`](crate::Error::ReadbackUnavailable) from every
/// read-back, once its query frame has been shown.
#[derive(Debug)]
pub struct Panel<T> {
    transport: T,
}

impl<T: Transport> Panel<T> {
    /// Wraps `transport`. Sends nothing.
    pub fn new(transport: T) -> Self {
        Self { transport }
    }

    /// Turns the panel on (`E7`).
    pub fn on(&mut self) -> Result<()> {
        self.transport.write(&power_frame(true))
    }

    /// Turns the panel off (`E7`).
    pub fn off(&mut self) -> Result<()> {
        self.transport.write(&power_frame(false))
    }

    /// Selects the display mode (`E5`).
    pub fn set_mode(&mut self, mode: Mode) -> Result<()> {
        self.transport.write(&mode_frame(mode))
    }

    /// Persists the panel configuration to NVRAM (`AA`).
    ///
    /// **This writes NVRAM.** The same frame also triggers the text rainbow
    /// effect. Only an explicit user request may lead here, and the daemon must
    /// never call it.
    pub fn save(&mut self) -> Result<()> {
        self.transport.write(&save_frame())
    }

    /// Sets the carousel (`F3`).
    ///
    /// # Errors
    ///
    /// [`Error::InvalidArgument`](crate::Error::InvalidArgument), with nothing
    /// sent, for a list [`carousel_frame`] refuses.
    pub fn set_carousel(&mut self, interval: u8, modes: &[Mode]) -> Result<()> {
        self.transport.write(&carousel_frame(interval, modes)?)
    }

    /// Configures the sensor dashboard (`E1`).
    ///
    /// This is the only sender of `E1`, the prime suspect in dark-panel
    /// incidents (`RECOVERY.md`). Call it once on (re)connect or on an explicit
    /// user request, never periodically.
    pub fn set_dashboard(&mut self, elements: Elements, interval_s: u8) -> Result<()> {
        self.transport.write(&display_frame(elements, interval_s))
    }

    /// Pushes live sensor values (`E3`). It only ever sends that one frame, so
    /// it is safe to call on every tick.
    pub fn send_sensors(&mut self, sample: &SensorSample, len: FeedFrameLen) -> Result<()> {
        self.transport.write(&sensor_frame(sample, len))
    }

    /// Uploads `payload` and selects the mode that shows it. See the
    /// [module docs](self) for what to pass.
    ///
    /// `params.framebuffer` says what the content is, and that alone decides
    /// the order, so frames and order cannot disagree. A GIF goes to a live
    /// buffer the panel has to be listening on, so `SetMode` comes first, and
    /// switching afterwards shows black. Image and text are stored in numbered
    /// buffers, and their `SetMode` follows the upload, as in the captures.
    /// `upload_content` in `aorus_lcd.py`.
    ///
    /// Image and text payloads must already start with
    /// [`DESCRIPTOR`](crate::protocol::DESCRIPTOR).
    ///
    /// `progress` is called after each frame of the upload, and not for
    /// `SetMode`. Pass `|_| {}` to ignore it.
    ///
    /// # Errors
    ///
    /// [`Error::Upload`] for a lost frame, with `frame` its index from the `F2`
    /// BEGIN marker on. Nothing further is written, so neither `F2` END nor
    /// `SetMode`: a half upload must not be closed as if it had worked.
    ///
    /// A failed `SetMode` is no lost frame, so its error is returned as it is.
    /// Before a GIF nothing else has been written. After an image or text the
    /// upload is complete and only the switch to it is missing.
    ///
    /// [`Error::InvalidArgument`], with nothing sent, for a payload
    /// [`build_upload`] refuses.
    pub fn upload(
        &mut self,
        payload: &[u8],
        params: &UploadParams,
        options: UploadOptions,
        sleep: &mut dyn Sleep,
        progress: impl FnMut(UploadProgress),
    ) -> Result<()> {
        let frames = build_upload(payload, params)?;
        let mode = mode_showing(params.framebuffer);
        let mode_first = params.framebuffer == Framebuffer::Gif;

        if options.switch_mode && mode_first {
            self.set_mode(mode)?;
            sleep.sleep(GIF_MODE_SETTLE);
        }
        self.stream(&frames, options.chunk_delay, sleep, progress)?;
        if options.switch_mode && !mode_first {
            self.set_mode(mode)?;
        }
        Ok(())
    }

    /// Writes `frames` under the firmware's pacing. `send_upload` in
    /// `aorus_lcd.py`.
    fn stream(
        &mut self,
        frames: &[UploadFrame],
        chunk_delay: Duration,
        sleep: &mut dyn Sleep,
        mut progress: impl FnMut(UploadProgress),
    ) -> Result<()> {
        for (index, frame) in frames.iter().enumerate() {
            self.transport
                .write(&frame.data)
                .map_err(|source| Error::Upload {
                    frame: index,
                    source: Box::new(source),
                })?;
            progress(UploadProgress {
                sent: index + 1,
                total: frames.len(),
            });
            sleep.sleep(pace_after(frame.kind, chunk_delay));
        }
        Ok(())
    }

    /// Reads the firmware version (`D6`).
    pub fn firmware(&mut self) -> Result<FirmwareVersion> {
        let reply = self.read_cmd::<STATUS_READ_LEN>(&firmware_query_frame())?;
        parse_firmware(&reply)
    }

    /// Reads the mode and on-state (`DE`).
    pub fn mode(&mut self) -> Result<ModeStatus> {
        let reply = self.read_cmd::<STATUS_READ_LEN>(&mode_query_frame())?;
        parse_mode(&reply)
    }

    /// Reads the dashboard configuration (`DF`).
    pub fn display(&mut self) -> Result<DisplayStatus> {
        let reply = self.read_cmd::<STATUS_READ_LEN>(&display_query_frame())?;
        parse_display(&reply)
    }

    /// Reads carousel banks 1 to 5 (`F4`), in order. The first failure ends it.
    pub fn loop_banks(&mut self) -> Result<Vec<LoopBankRead>> {
        (LOOP_BANK_FIRST..=LOOP_BANK_LAST)
            .map(|bank| self.read_loop_bank(bank))
            .collect()
    }

    /// Reads firmware, mode, display and loop banks, in that order.
    ///
    /// The first failure ends it, so there is no partial status. Reads can fail
    /// transiently on real hardware (`RECOVERY.md`), and that is for the link to
    /// absorb: see the [module docs](self).
    pub fn status(&mut self) -> Result<PanelStatus> {
        Ok(PanelStatus {
            firmware: self.firmware()?,
            mode: self.mode()?,
            display: self.display()?,
            loop_banks: self.loop_banks()?,
        })
    }

    fn read_loop_bank(&mut self, bank: u8) -> Result<LoopBankRead> {
        let raw = self.read_cmd(&loop_query_frame(bank)?)?;
        Ok(LoopBankRead { bank, raw })
    }

    /// Writes a query, then reads `N` bytes in a separate transfer. The write
    /// comes first even though dry-run's read can only fail, so that a dry run
    /// shows the query.
    fn read_cmd<const N: usize>(&mut self, query: &Frame) -> Result<[u8; N]> {
        self.transport.write(query)?;
        let mut reply = [0; N];
        self.transport.read(&mut reply)?;
        Ok(reply)
    }
}

/// The total of the pauses [`Panel::upload`] will make, the firmware's pacing and
/// nothing else, so a real upload takes longer by its time on the bus.
///
/// # Errors
///
/// [`Error::InvalidArgument`] for a payload [`build_upload`] refuses.
pub fn estimated_upload_pauses(
    payload: &[u8],
    params: &UploadParams,
    options: UploadOptions,
) -> Result<Duration> {
    let frames = build_upload(payload, params)?;
    let pacing: Duration = frames
        .iter()
        .map(|frame| pace_after(frame.kind, options.chunk_delay))
        .sum();
    if options.switch_mode && params.framebuffer == Framebuffer::Gif {
        Ok(pacing + GIF_MODE_SETTLE)
    } else {
        Ok(pacing)
    }
}

/// The mode that shows what a framebuffer holds. Both references only ever pair
/// them this way.
const fn mode_showing(framebuffer: Framebuffer) -> Mode {
    match framebuffer {
        Framebuffer::Static => Mode::Image,
        Framebuffer::Text => Mode::Text,
        Framebuffer::Gif => Mode::Gif,
    }
}

/// The pause the firmware needs after a frame. This goes by the frame's role,
/// because a chunk of pixels can start with any bytes, those of a marker
/// included. Both references also pause after END, even when nothing follows.
const fn pace_after(kind: UploadFrameKind, chunk_delay: Duration) -> Duration {
    match kind {
        UploadFrameKind::Begin => PACE_BEGIN,
        UploadFrameKind::Header => PACE_HEADER,
        UploadFrameKind::Chunk | UploadFrameKind::End => chunk_delay,
    }
}

#[cfg(test)]
mod tests {
    use std::iter::repeat_n;

    use super::*;
    use crate::bus::transport::testing::{i2c_error, Mock};
    use crate::bus::transport::Retrying;
    use crate::protocol::{
        marker_frame, Element, LoopEntry, Marker, DESCRIPTOR, FRAME_BYTES, FRAME_LEN,
        OP_SENSOR_FEED, OP_SET_MODE, OP_UPLOAD_HEADER,
    };

    const BANKS: std::ops::RangeInclusive<u8> = LOOP_BANK_FIRST..=LOOP_BANK_LAST;

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

    type Op = fn(&mut Panel<Mock>) -> Result<()>;

    /// Every operation that only writes.
    const WRITE_OPS: [Op; 8] = [
        Panel::on,
        Panel::off,
        |panel| panel.set_mode(Mode::Gif),
        Panel::save,
        |panel| panel.set_carousel(5, &[Mode::Faith1]),
        |panel| panel.set_dashboard(Elements::ALL, 3),
        |panel| panel.send_sensors(&SAMPLE, FeedFrameLen::Full),
        |panel| panel.send_sensors(&SAMPLE, FeedFrameLen::Truncated),
    ];

    /// Every operation that reads, with the bytes its first read asks for.
    const READ_OPS: [(Op, usize); 5] = [
        (|panel| panel.firmware().map(drop), STATUS_READ_LEN),
        (|panel| panel.mode().map(drop), STATUS_READ_LEN),
        (|panel| panel.display().map(drop), STATUS_READ_LEN),
        (|panel| panel.loop_banks().map(drop), LOOP_READ_LEN),
        (|panel| panel.status().map(drop), STATUS_READ_LEN),
    ];

    fn panel() -> (Panel<Mock>, Mock) {
        let mock = Mock::new();
        (Panel::new(mock.clone()), mock)
    }

    fn loop_queries() -> Vec<Vec<u8>> {
        BANKS
            .map(|bank| loop_query_frame(bank).unwrap().to_vec())
            .collect()
    }

    /// What `status` sends, in order.
    fn status_queries() -> Vec<Vec<u8>> {
        let mut frames = vec![
            firmware_query_frame().to_vec(),
            mode_query_frame().to_vec(),
            display_query_frame().to_vec(),
        ];
        frames.extend(loop_queries());
        frames
    }

    /// A valid reply to each of the first `count` queries of `status`.
    fn script_status_replies(mock: &Mock, count: usize) {
        for index in 0..count {
            match index {
                0 => mock.script_read([0, 0x1A, 0, 0]),
                1 => mock.script_read([0, 0x06, 1, 0]),
                2 => mock.script_read([0, 0x05, 3, 0]),
                _ => mock.script_read([index as u8; LOOP_READ_LEN]),
            }
        }
    }

    #[test]
    fn building_a_panel_touches_nothing() {
        let (_panel, mock) = panel();

        assert_eq!(mock.write_attempts(), 0);
        assert_eq!(mock.read_attempts(), 0);
    }

    #[test]
    fn on_sends_power_on() {
        let (mut panel, mock) = panel();
        panel.on().unwrap();
        assert_eq!(mock.writes(), [power_frame(true)]);
    }

    #[test]
    fn off_sends_power_off() {
        let (mut panel, mock) = panel();
        panel.off().unwrap();
        assert_eq!(mock.writes(), [power_frame(false)]);
    }

    #[test]
    fn set_mode_sends_the_mode_frame() {
        let (mut panel, mock) = panel();
        panel.set_mode(Mode::Carousel).unwrap();
        assert_eq!(mock.writes(), [mode_frame(Mode::Carousel)]);
    }

    #[test]
    fn save_sends_the_save_frame() {
        let (mut panel, mock) = panel();
        panel.save().unwrap();
        assert_eq!(mock.writes(), [save_frame()]);
    }

    #[test]
    fn set_carousel_sends_the_carousel_frame() {
        let (mut panel, mock) = panel();
        let modes = [Mode::Faith1, Mode::Gif];

        panel.set_carousel(5, &modes).unwrap();

        assert_eq!(mock.writes(), [carousel_frame(5, &modes).unwrap()]);
    }

    #[test]
    fn a_refused_carousel_sends_nothing() {
        for modes in [
            &[][..],
            &[Mode::Faith1, Mode::Carousel][..],
            &[Mode::Faith1, Mode::PresetRotation][..],
        ] {
            let (mut panel, mock) = panel();

            let error = panel.set_carousel(5, modes).unwrap_err();

            assert!(matches!(error, Error::InvalidArgument(_)), "{error:?}");
            assert_eq!(mock.write_attempts(), 0);
        }
    }

    #[test]
    fn set_dashboard_sends_the_display_frame() {
        let (mut panel, mock) = panel();
        let elements = Elements::NONE.with(Element::GpuTemp).with(Element::Tgp);

        panel.set_dashboard(elements, 3).unwrap();

        assert_eq!(mock.writes(), [display_frame(elements, 3)]);
    }

    #[test]
    fn send_sensors_sends_one_e3_of_the_asked_length() {
        for len in [FeedFrameLen::Full, FeedFrameLen::Truncated] {
            let (mut panel, mock) = panel();

            panel.send_sensors(&SAMPLE, len).unwrap();

            assert_eq!(mock.writes(), [sensor_frame(&SAMPLE, len)]);
            assert_eq!(mock.writes()[0].len(), len.bytes());
            assert_eq!(mock.writes()[0][0], OP_SENSOR_FEED);
        }
    }

    #[test]
    fn firmware_queries_then_parses_the_reply() {
        let (mut panel, mock) = panel();
        mock.script_read([0, 0x1A, 0, 0]);

        let firmware = panel.firmware().unwrap();

        assert_eq!((firmware.major, firmware.minor), (1, 10));
        assert_eq!(mock.writes(), [firmware_query_frame()]);
        assert_eq!(mock.read_attempts(), 1);
    }

    #[test]
    fn mode_queries_then_parses_the_reply() {
        let (mut panel, mock) = panel();
        mock.script_read([0, 0x06, 1, 0]);

        let status = panel.mode().unwrap();

        assert_eq!(status.mode(), Some(Mode::Gif));
        assert!(status.on);
        assert_eq!(mock.writes(), [mode_query_frame()]);
        assert_eq!(mock.read_attempts(), 1);
    }

    #[test]
    fn display_queries_then_parses_the_reply() {
        let (mut panel, mock) = panel();
        mock.script_read([0, 0x05, 3, 0]);

        let display = panel.display().unwrap();

        assert_eq!(display.elements, Elements::from_bits(0x05));
        assert_eq!(display.interval_s, 3);
        assert_eq!(mock.writes(), [display_query_frame()]);
        assert_eq!(mock.read_attempts(), 1);
    }

    #[test]
    fn loop_banks_reads_banks_one_to_five_in_order() {
        let (mut panel, mock) = panel();
        for bank in BANKS {
            mock.script_read([bank; LOOP_READ_LEN]);
        }

        let banks = panel.loop_banks().unwrap();

        let expected: Vec<LoopBankRead> = BANKS
            .map(|bank| LoopBankRead {
                bank,
                raw: [bank; LOOP_READ_LEN],
            })
            .collect();
        assert_eq!(banks, expected);
        assert_eq!(mock.writes(), loop_queries());
        assert_eq!(mock.read_attempts(), BANKS.count());
    }

    #[test]
    fn a_loop_bank_keeps_every_raw_byte_next_to_its_interpretation() {
        let raw = [5, 1, 2, 0xEE, 0xA1, 0xA2, 0xA3, 0xA4];
        let (mut panel, mock) = panel();
        for _ in BANKS {
            mock.script_read(raw);
        }

        let first = panel.loop_banks().unwrap()[0];

        assert_eq!(first.raw, raw);
        let parsed = first.parsed().unwrap();
        assert_eq!(parsed.interval, 5);
        assert_eq!(
            parsed.entries,
            [
                LoopEntry::Mode(Mode::Faith1),
                LoopEntry::Mode(Mode::Faith2),
                LoopEntry::Unknown(0xEE),
            ]
        );
    }

    #[test]
    fn status_reads_firmware_mode_display_and_banks_in_order() {
        let queries = status_queries();
        let (mut panel, mock) = panel();
        script_status_replies(&mock, queries.len());

        let status = panel.status().unwrap();

        assert_eq!(mock.writes(), queries);
        assert_eq!(mock.read_attempts(), queries.len());
        assert_eq!(status.firmware.to_string(), "1.10");
        assert_eq!(status.mode.mode(), Some(Mode::Gif));
        assert_eq!(status.display.interval_s, 3);
        assert_eq!(status.loop_banks.len(), BANKS.count());
        assert_eq!(status.loop_banks[4].bank, 5);
        assert_eq!(status.loop_banks[4].raw, [7; LOOP_READ_LEN]);
    }

    #[test]
    fn a_failed_write_is_returned_and_ends_the_operation() {
        let read_ops = READ_OPS.map(|(op, _)| op);
        for op in WRITE_OPS.into_iter().chain(read_ops) {
            let (mut panel, mock) = panel();
            mock.fail_write(0, i2c_error());

            let error = op(&mut panel).unwrap_err();

            assert!(matches!(error, Error::I2c { .. }), "{error:?}");
            assert_eq!(mock.write_attempts(), 1);
            assert_eq!(mock.read_attempts(), 0);
        }
    }

    #[test]
    fn a_failed_read_is_returned_and_ends_the_operation() {
        for (op, _) in READ_OPS {
            let (mut panel, mock) = panel();
            mock.script_read_error(i2c_error());

            let error = op(&mut panel).unwrap_err();

            assert!(matches!(error, Error::I2c { .. }), "{error:?}");
            assert_eq!(mock.write_attempts(), 1);
            assert_eq!(mock.read_attempts(), 1);
        }
    }

    #[test]
    fn a_short_read_is_returned_with_the_length_asked_for() {
        for (op, expected) in READ_OPS {
            let (mut panel, mock) = panel();
            mock.script_read([0; 3]);

            let error = op(&mut panel).unwrap_err();

            assert!(
                matches!(error, Error::ShortRead { expected: asked, actual: 3 } if asked == expected),
                "{error:?}"
            );
            assert_eq!(mock.write_attempts(), 1);
        }
    }

    #[test]
    fn a_missing_readback_reaches_the_caller_of_every_read() {
        for (op, _) in READ_OPS {
            let (mut panel, mock) = panel();
            mock.script_read_error(Error::ReadbackUnavailable);

            let error = op(&mut panel).unwrap_err();

            assert!(matches!(error, Error::ReadbackUnavailable), "{error:?}");
            assert_eq!(mock.write_attempts(), 1);
        }
    }

    #[test]
    fn status_stops_at_a_failed_write_anywhere_and_does_not_retry_it() {
        let queries = status_queries();
        for failing in 0..queries.len() {
            let (mut panel, mock) = panel();
            script_status_replies(&mock, failing);
            mock.fail_write(failing, i2c_error());

            let error = panel.status().unwrap_err();

            assert!(matches!(error, Error::I2c { .. }), "{error:?}");
            assert_eq!(mock.writes(), queries[..failing]);
            assert_eq!(mock.write_attempts(), failing + 1);
            assert_eq!(mock.read_attempts(), failing);
        }
    }

    #[test]
    fn status_stops_at_a_failed_read_anywhere_and_does_not_retry_it() {
        let queries = status_queries();
        for failing in 0..queries.len() {
            let (mut panel, mock) = panel();
            script_status_replies(&mock, failing);
            mock.script_read_error(i2c_error());

            let error = panel.status().unwrap_err();

            assert!(matches!(error, Error::I2c { .. }), "{error:?}");
            assert_eq!(mock.writes(), queries[..=failing]);
            assert_eq!(mock.write_attempts(), failing + 1);
            assert_eq!(mock.read_attempts(), failing + 1);
        }
    }

    const CHUNK_PACE: Duration = Duration::from_millis(10);
    const GIF_SETTLE: Duration = Duration::from_millis(200);
    const BEGIN_PACE: Duration = Duration::from_millis(500);
    const HEADER_PACE: Duration = Duration::from_secs(1);

    /// Two chunks.
    const PAYLOAD: [u8; 300] = [0x44; 300];
    /// Begin, Header, two chunks, End.
    const FRAMES: usize = 5;
    const FIRST_CHUNK: usize = 2;
    const END: usize = FRAMES - 1;

    fn image() -> UploadParams {
        UploadParams::still(Framebuffer::Static)
    }

    fn text() -> UploadParams {
        UploadParams::still(Framebuffer::Text)
    }

    fn gif() -> UploadParams {
        UploadParams::gif(2, 50)
    }

    /// The frames of the upload, without whatever selects the mode.
    fn upload_frames(params: &UploadParams) -> Vec<Vec<u8>> {
        build_upload(&PAYLOAD, params)
            .unwrap()
            .iter()
            .map(|frame| frame.data.to_vec())
            .collect()
    }

    /// The pauses of a stream with `chunks` chunks: everything after the header
    /// waits for the chunk delay, End included.
    fn stream_delays(chunks: usize, chunk_delay: Duration) -> Vec<Duration> {
        [BEGIN_PACE, HEADER_PACE]
            .into_iter()
            .chain(repeat_n(chunk_delay, chunks + 1))
            .collect()
    }

    /// Uploads `PAYLOAD` with the default options. Returns the pacing delays.
    fn upload(
        panel: &mut Panel<impl Transport>,
        params: &UploadParams,
    ) -> (Result<()>, Vec<Duration>) {
        let mut sleep = RecordingSleep::default();
        let options = UploadOptions::default();
        let result = panel.upload(&PAYLOAD, params, options, &mut sleep, |_| {});
        (result, sleep.delays)
    }

    fn assert_lost_frame(result: Result<()>, expected: usize) {
        match result {
            Err(Error::Upload { frame, source }) => {
                assert_eq!(frame, expected);
                assert!(matches!(*source, Error::I2c { .. }), "{source:?}");
            }
            other => panic!("expected frame {expected} to be lost, got {other:?}"),
        }
    }

    #[test]
    fn image_and_text_stream_and_then_switch_mode() {
        for (params, mode) in [(image(), Mode::Image), (text(), Mode::Text)] {
            let (mut panel, mock) = panel();

            let (result, delays) = upload(&mut panel, &params);

            result.unwrap();
            let mut expected = upload_frames(&params);
            expected.push(mode_frame(mode).to_vec());
            assert_eq!(mock.writes(), expected);
            assert_eq!(delays, stream_delays(2, CHUNK_PACE));
        }
    }

    #[test]
    fn gif_switches_mode_settles_and_then_streams() {
        let (mut panel, mock) = panel();

        let (result, delays) = upload(&mut panel, &gif());

        result.unwrap();
        let mut expected = vec![mode_frame(Mode::Gif).to_vec()];
        expected.extend(upload_frames(&gif()));
        assert_eq!(mock.writes(), expected);
        assert_eq!(delays[0], GIF_SETTLE);
        assert_eq!(delays[1..], stream_delays(2, CHUNK_PACE));
    }

    #[test]
    fn switching_the_mode_can_be_skipped_along_with_its_settle() {
        for params in [image(), text(), gif()] {
            let (mut panel, mock) = panel();
            let mut sleep = RecordingSleep::default();
            let options = UploadOptions {
                switch_mode: false,
                ..UploadOptions::default()
            };

            panel
                .upload(&PAYLOAD, &params, options, &mut sleep, |_| {})
                .unwrap();

            assert_eq!(mock.writes(), upload_frames(&params));
            assert!(mock.writes().iter().all(|frame| frame[0] != OP_SET_MODE));
            assert_eq!(sleep.delays, stream_delays(2, CHUNK_PACE));
        }
    }

    #[test]
    fn the_chunk_delay_applies_to_chunks_and_end_only() {
        let chunk_delay = Duration::from_millis(3);
        let (mut panel, _mock) = panel();
        let mut sleep = RecordingSleep::default();
        let options = UploadOptions {
            chunk_delay,
            ..UploadOptions::default()
        };

        panel
            .upload(&PAYLOAD, &gif(), options, &mut sleep, |_| {})
            .unwrap();

        assert_eq!(sleep.delays[0], GIF_SETTLE);
        assert_eq!(sleep.delays[1..], stream_delays(2, chunk_delay));
    }

    #[test]
    fn a_chunk_that_looks_like_a_marker_or_header_paces_as_a_chunk() {
        let mut payload = vec![0; 3 * FRAME_LEN];
        payload[..FRAME_LEN].copy_from_slice(&marker_frame(Marker::Begin));
        payload[FRAME_LEN] = OP_UPLOAD_HEADER;
        let (mut panel, _mock) = panel();
        let mut sleep = RecordingSleep::default();
        let options = UploadOptions::default();

        panel
            .upload(&payload, &image(), options, &mut sleep, |_| {})
            .unwrap();

        // Three chunks, plus the padding chunk of an exact multiple.
        assert_eq!(sleep.delays, stream_delays(4, CHUNK_PACE));
    }

    #[test]
    fn a_full_image_takes_about_six_seconds() {
        let mut payload = DESCRIPTOR.to_vec();
        payload.resize(DESCRIPTOR.len() + FRAME_BYTES, 0x12);
        let (mut panel, mock) = panel();
        let mut sleep = RecordingSleep::default();
        let options = UploadOptions::default();

        panel
            .upload(&payload, &image(), options, &mut sleep, |_| {})
            .unwrap();

        // 426 chunks, and Begin, Header, End and SetMode.
        assert_eq!(mock.writes().len(), 430);
        let total: Duration = sleep.delays.iter().sum();
        assert_eq!(total, Duration::from_millis(5770));
    }

    #[test]
    fn the_estimate_is_the_pauses_of_the_upload() {
        let default = UploadOptions::default();
        let cases = [
            (image(), default),
            (text(), default),
            (gif(), default),
            (
                gif(),
                UploadOptions {
                    switch_mode: false,
                    ..default
                },
            ),
            (
                image(),
                UploadOptions {
                    chunk_delay: Duration::from_millis(3),
                    ..default
                },
            ),
        ];
        for (params, options) in cases {
            let payload = [0x12; 1000];
            let (mut panel, _mock) = panel();
            let mut sleep = RecordingSleep::default();

            panel
                .upload(&payload, &params, options, &mut sleep, |_| {})
                .unwrap();

            let total: Duration = sleep.delays.iter().sum();
            assert_eq!(
                estimated_upload_pauses(&payload, &params, options).unwrap(),
                total,
                "{params:?}, {options:?}"
            );
        }
    }

    #[test]
    fn a_lost_frame_ends_the_upload_where_it_happened() {
        let failures = [
            ("begin", 0),
            ("header", 1),
            ("first chunk", FIRST_CHUNK),
            ("last chunk", END - 1),
            ("end", END),
        ];
        for (name, failing) in failures {
            for params in [image(), text(), gif()] {
                let is_gif = params.framebuffer == Framebuffer::Gif;
                let leading_set_mode = usize::from(is_gif);
                let (mut panel, mock) = panel();
                mock.fail_write(leading_set_mode + failing, i2c_error());

                let (result, delays) = upload(&mut panel, &params);

                assert_lost_frame(result, failing);
                // Exactly what preceded it, so neither F2 END nor SetMode after.
                let writes = mock.writes();
                let frames = upload_frames(&params);
                assert_eq!(writes[leading_set_mode..], frames[..failing], "{name}");
                assert_eq!(writes.len(), leading_set_mode + failing, "{name}");
                assert_eq!(mock.write_attempts(), writes.len() + 1, "{name}");
                // No pause after the frame that failed.
                let paced = &delays[leading_set_mode..];
                assert_eq!(paced, &stream_delays(2, CHUNK_PACE)[..failing], "{name}");
            }
        }
    }

    #[test]
    fn a_failed_gif_mode_switch_writes_nothing_else_and_names_no_frame() {
        let (mut panel, mock) = panel();
        mock.fail_write(0, i2c_error());

        let (result, delays) = upload(&mut panel, &gif());

        assert!(matches!(result, Err(Error::I2c { .. })), "{result:?}");
        assert_eq!(mock.write_attempts(), 1);
        assert!(delays.is_empty());
    }

    #[test]
    fn a_failed_mode_switch_after_a_complete_upload_names_no_frame() {
        let (mut panel, mock) = panel();
        mock.fail_write(FRAMES, i2c_error());

        let (result, delays) = upload(&mut panel, &image());

        assert!(matches!(result, Err(Error::I2c { .. })), "{result:?}");
        assert_eq!(mock.writes(), upload_frames(&image()));
        assert_eq!(delays, stream_delays(2, CHUNK_PACE));
    }

    #[test]
    fn retrying_carries_an_upload_past_a_passing_fault() {
        for failing in [0, FIRST_CHUNK, END] {
            let mock = Mock::new();
            mock.fail_write(failing, i2c_error());
            let retrying = Retrying::with_sleep(mock.clone(), RecordingSleep::default());

            let (result, delays) = upload(&mut Panel::new(retrying), &image());

            result.unwrap();
            let mut expected = upload_frames(&image());
            expected.push(mode_frame(Mode::Image).to_vec());
            assert_eq!(mock.writes(), expected);
            // The retry waits on its own clock and leaves the pacing alone.
            assert_eq!(delays, stream_delays(2, CHUNK_PACE));
        }
    }

    #[test]
    fn a_frame_retrying_gives_up_on_is_the_one_reported() {
        let mock = Mock::new();
        for attempt in 0..5 {
            mock.fail_write(FIRST_CHUNK + attempt, i2c_error());
        }
        let retrying = Retrying::with_sleep(mock.clone(), RecordingSleep::default());

        let (result, _) = upload(&mut Panel::new(retrying), &image());

        assert_lost_frame(result, FIRST_CHUNK);
        assert_eq!(mock.writes(), upload_frames(&image())[..FIRST_CHUNK]);
        assert_eq!(mock.write_attempts(), FIRST_CHUNK + 5);
    }

    #[test]
    fn an_upload_reads_nothing() {
        for params in [image(), text(), gif()] {
            let (mut panel, mock) = panel();

            upload(&mut panel, &params).0.unwrap();

            assert_eq!(mock.read_attempts(), 0);
        }
    }

    #[test]
    fn progress_counts_the_frames_of_the_upload() {
        for params in [image(), text(), gif()] {
            let (mut panel, _mock) = panel();
            let options = UploadOptions::default();
            let mut seen = Vec::new();

            panel
                .upload(&PAYLOAD, &params, options, &mut NoSleep, |progress| {
                    seen.push((progress.sent, progress.total));
                })
                .unwrap();

            let expected: Vec<_> = (1..=FRAMES).map(|sent| (sent, FRAMES)).collect();
            assert_eq!(seen, expected);
        }
    }

    #[test]
    fn progress_stops_before_the_lost_frame() {
        let (mut panel, mock) = panel();
        mock.fail_write(FIRST_CHUNK, i2c_error());
        let options = UploadOptions::default();
        let mut seen = Vec::new();

        let result = panel.upload(&PAYLOAD, &image(), options, &mut NoSleep, |progress| {
            seen.push(progress.sent);
        });

        assert_lost_frame(result, FIRST_CHUNK);
        assert_eq!(seen, [1, 2]);
    }

    #[test]
    fn no_sleep_does_not_wait() {
        let (mut panel, _mock) = panel();
        let options = UploadOptions::default();
        let start = std::time::Instant::now();

        panel
            .upload(&PAYLOAD, &gif(), options, &mut NoSleep, |_| {})
            .unwrap();

        // Well under the 1.73 s of pacing this upload asks for.
        assert!(start.elapsed() < Duration::from_secs(1));
    }
}
