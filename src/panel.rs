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
//! [`Unlocked`]: crate::session::Unlocked
//! [`Unlocked::lock`]: crate::session::Unlocked::lock

use std::time::Duration;

use crate::bus::transport::Transport;
use crate::error::Result;
use crate::protocol::{
    carousel_frame, display_frame, display_query_frame, firmware_query_frame, loop_query_frame,
    mode_frame, mode_query_frame, parse_display, parse_firmware, parse_loop_bank, parse_mode,
    power_frame, save_frame, sensor_frame, DisplayStatus, Elements, FeedFrameLen, FirmwareVersion,
    Frame, LoopBank, Mode, ModeStatus, SensorSample, LOOP_BANK_FIRST, LOOP_BANK_LAST,
    LOOP_READ_LEN, STATUS_READ_LEN,
};

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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bus::transport::testing::{i2c_error, Mock};
    use crate::error::Error;
    use crate::protocol::{Element, LoopEntry, OP_SENSOR_FEED};

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
        for modes in [&[][..], &[Mode::Faith1, Mode::Carousel][..]] {
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
}
