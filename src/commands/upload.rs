//! What every upload command shares: the path from a finished payload to the
//! panel. This is a helper of the upload commands, not a subcommand.

use std::io::{IsTerminal, Write};
use std::time::{Duration, Instant};

use aorus_lcd::bus::transport::{Retrying, Transport};
use aorus_lcd::panel::{NoSleep, Panel, Sleep, ThreadSleep, UploadOptions, UploadProgress};
use aorus_lcd::protocol::{Elements, UploadParams};
use aorus_lcd::session::{self, Session};
use aorus_lcd::Result;

use super::save;

/// Keeps the progress line to a few updates per second, however fast frames go.
const PROGRESS_INTERVAL: Duration = Duration::from_millis(250);

/// The `--help` of `--no-overlay`, the same for every upload command.
pub const NO_OVERLAY_HELP: &str =
    "After a successful upload, clear the sensor overlay. An upload sends no E1 unless this \
     opt-in asks for it: E1 with no widgets and interval 0. That clears the panel's widget \
     selection rather than hiding it, and it stays cleared until `dashboard` sets it. E1 goes \
     out before any NVRAM save, which then stores the cleared selection. A running daemon may \
     send widgets again when it next connects";

/// What follows a successful upload.
#[derive(Debug, Clone, Copy)]
pub struct Finish {
    /// Send `E1` with no widgets. See [`NO_OVERLAY_HELP`].
    pub clear_overlay: bool,
    /// Save to NVRAM.
    pub persist: bool,
}

/// Connects and uploads `payload`, then does what `finish` asks.
///
/// Call this last: it starts with the probe, so everything that can be refused
/// must have been refused by then.
pub fn send(
    payload: &[u8],
    params: &UploadParams,
    finish: Finish,
    globals: &crate::Globals,
) -> Result<()> {
    let mut session = session::connect(&globals.options())?;
    let (mut clock, mut meter) = pacing(&session);
    deliver(
        &mut session,
        payload,
        params,
        finish,
        &mut *clock,
        &mut meter,
    )
}

/// [`send`] on a session that is already connected.
///
/// `E1` and `AA` follow a successful upload only, so a failed one changes nothing
/// else on the panel. `E1` also comes after the mode switch, and before `AA` so
/// that the save stores the state the user asked for.
///
/// Both skip [`Retrying`]. A repeat of `AA` would be a second NVRAM write, and the
/// `save` command has the same rule. `E1` is the prime suspect in dark-panel
/// incidents (`RECOVERY.md`), so a failed one is reported rather than repeated, as
/// in `dashboard`, and no save follows it.
fn deliver<T: Transport, W: Write>(
    session: &mut Session<T, W>,
    payload: &[u8],
    params: &UploadParams,
    finish: Finish,
    clock: &mut dyn Sleep,
    meter: &mut Meter,
) -> Result<()> {
    let uploaded = Panel::new(Retrying::new(&mut *session)).upload(
        payload,
        params,
        UploadOptions::default(),
        clock,
        |progress| meter.update(progress),
    );
    meter.finish();
    uploaded?;

    if finish.clear_overlay {
        // Interval 0 is what `dashboard --off` sends.
        Panel::new(&mut *session).set_dashboard(Elements::NONE, 0)?;
    }
    if finish.persist {
        println!("{}", save::notice(session));
        Panel::new(&mut *session).save()?;
    }
    Ok(())
}

/// The clock and progress line of a run. A real run pauses as the firmware needs
/// and a dry run does not, and only a real run on a terminal shows progress.
pub fn pacing<T, W>(session: &Session<T, W>) -> (Box<dyn Sleep>, Meter) {
    match session {
        Session::Live(_) => (
            Box::new(ThreadSleep),
            Meter::new(std::io::stderr().is_terminal()),
        ),
        Session::DryRun(_) => (Box::new(NoSleep), Meter::new(false)),
    }
}

/// A progress line on stderr, away from the dump and any piped output.
pub struct Meter {
    enabled: bool,
    last_drawn: Option<Instant>,
}

impl Meter {
    fn new(enabled: bool) -> Self {
        Self {
            enabled,
            last_drawn: None,
        }
    }

    /// Draws the line, if it is time to.
    pub fn update(&mut self, progress: UploadProgress) {
        let now = Instant::now();
        if self.enabled && is_due(self.last_drawn, now, progress) {
            let mut stderr = std::io::stderr();
            // Progress is a courtesy, so a closed stderr must not end an upload.
            let _ = write!(stderr, "\r{}", progress_line(progress));
            let _ = stderr.flush();
            self.last_drawn = Some(now);
        }
    }

    /// Ends the line, so that what comes next starts on its own. Calling this
    /// again does nothing.
    pub fn finish(&mut self) {
        if self.last_drawn.take().is_some() {
            eprintln!();
        }
    }
}

/// The first and last frames are always drawn, so the line never stays behind.
fn is_due(last_drawn: Option<Instant>, now: Instant, progress: UploadProgress) -> bool {
    last_drawn.is_none_or(|last| {
        progress.sent == progress.total || now.duration_since(last) >= PROGRESS_INTERVAL
    })
}

fn progress_line(UploadProgress { sent, total }: UploadProgress) -> String {
    let percent = sent * 100 / total.max(1);
    format!("uploading: {sent} of {total} frames ({percent}%)")
}

#[cfg(test)]
mod tests {
    use std::fs;

    use aorus_lcd::bus::transport::testing::{i2c_error, Mock};
    use aorus_lcd::panel::{NoSleep, RecordingSleep};
    use aorus_lcd::protocol::{
        build_upload, display_frame, mode_frame, save_frame, Framebuffer, Mode, PROBE_READ_LEN,
    };
    use aorus_lcd::session::{connect_with, Environment, Options};
    use aorus_lcd::{Error, Result};
    use tempfile::TempDir;

    use super::*;

    /// Two chunks.
    const PAYLOAD: [u8; 300] = [0x44; 300];

    /// Write attempts in order: the probe, then `F2`, `F1`, two chunks and `F2`.
    const FIRST_CHUNK: usize = 3;
    const SET_MODE: usize = 6;
    const E1: usize = 7;

    const ALL: Finish = Finish {
        clear_overlay: true,
        persist: true,
    };

    fn image() -> UploadParams {
        UploadParams::still(Framebuffer::Static)
    }

    /// Fails `attempt`, and every retry of it that [`Retrying`] makes.
    fn fail_for_good(mock: &Mock, attempt: usize) {
        for retry in 0..5 {
            mock.fail_write(attempt + retry, i2c_error());
        }
    }

    /// Runs [`deliver`] on a live session over `mock`. Nothing here opens a device:
    /// the sysfs tree and the lock are temporary, and `open` returns the mock.
    fn deliver_to(mock: &Mock, finish: Finish) -> Result<()> {
        let dir = TempDir::new().unwrap();
        let adapter = dir.path().join("sysfs/i2c-7");
        fs::create_dir_all(&adapter).unwrap();
        fs::write(adapter.join("name"), "NVIDIA i2c adapter 1 at 1:00.0\n").unwrap();
        mock.script_read([0xFF; PROBE_READ_LEN]);

        let device = mock.clone();
        let mut session = connect_with(
            &Options::default(),
            Environment {
                sysfs_root: dir.path().join("sysfs"),
                lock_path: dir.path().join("test.lock"),
                open: move |_| Ok(device.clone()),
                sleep: RecordingSleep::default(),
                dry_run_output: Vec::new(),
            },
        )
        .unwrap();

        deliver(
            &mut session,
            &PAYLOAD,
            &image(),
            finish,
            &mut NoSleep,
            &mut Meter::new(false),
        )
    }

    /// The frames an upload of [`PAYLOAD`] writes after the probe, `SetMode` last.
    fn upload_writes() -> Vec<Vec<u8>> {
        let frames = build_upload(&PAYLOAD, &image()).unwrap();
        let mut writes: Vec<_> = frames.iter().map(|frame| frame.data.to_vec()).collect();
        writes.push(mode_frame(Mode::Image).to_vec());
        writes
    }

    fn writes_after_probe(mock: &Mock) -> Vec<Vec<u8>> {
        mock.writes()[1..].to_vec()
    }

    #[test]
    fn e1_follows_the_mode_switch_and_comes_before_the_save() {
        let mock = Mock::new();

        deliver_to(&mock, ALL).unwrap();

        let mut expected = upload_writes();
        expected.push(display_frame(Elements::NONE, 0).to_vec());
        expected.push(save_frame().to_vec());
        assert_eq!(writes_after_probe(&mock), expected);
    }

    #[test]
    fn without_the_flag_there_is_no_e1() {
        let mock = Mock::new();
        let finish = Finish {
            clear_overlay: false,
            persist: true,
        };

        deliver_to(&mock, finish).unwrap();

        let mut expected = upload_writes();
        expected.push(save_frame().to_vec());
        assert_eq!(writes_after_probe(&mock), expected);
    }

    #[test]
    fn a_lost_frame_sends_neither_e1_nor_aa() {
        let mock = Mock::new();
        fail_for_good(&mock, FIRST_CHUNK);

        let result = deliver_to(&mock, ALL);

        assert!(matches!(result, Err(Error::Upload { .. })), "{result:?}");
        assert_eq!(writes_after_probe(&mock), upload_writes()[..2]);
    }

    #[test]
    fn a_failed_mode_switch_sends_neither_e1_nor_aa() {
        let mock = Mock::new();
        fail_for_good(&mock, SET_MODE);

        let result = deliver_to(&mock, ALL);

        assert!(matches!(result, Err(Error::I2c { .. })), "{result:?}");
        assert_eq!(writes_after_probe(&mock), upload_writes()[..5]);
    }

    #[test]
    fn a_failed_e1_is_reported_once_and_not_followed_by_aa() {
        let mock = Mock::new();
        mock.fail_write(E1, i2c_error());

        let result = deliver_to(&mock, ALL);

        assert!(matches!(result, Err(Error::I2c { .. })), "{result:?}");
        assert_eq!(mock.write_attempts(), E1 + 1);
        assert_eq!(writes_after_probe(&mock), upload_writes());
    }

    fn at(sent: usize) -> UploadProgress {
        UploadProgress { sent, total: 100 }
    }

    #[test]
    fn the_first_frame_is_drawn_at_once() {
        assert!(is_due(None, Instant::now(), at(1)));
    }

    #[test]
    fn frames_inside_the_interval_are_not_drawn() {
        let start = Instant::now();

        assert!(!is_due(Some(start), start, at(2)));
        assert!(!is_due(
            Some(start),
            start + PROGRESS_INTERVAL - Duration::from_millis(1),
            at(2)
        ));
    }

    #[test]
    fn a_frame_after_the_interval_is_drawn() {
        let start = Instant::now();

        assert!(is_due(Some(start), start + PROGRESS_INTERVAL, at(2)));
    }

    #[test]
    fn the_last_frame_is_always_drawn() {
        let start = Instant::now();

        assert!(is_due(Some(start), start, at(100)));
    }

    #[test]
    fn the_line_counts_frames() {
        assert_eq!(
            progress_line(UploadProgress {
                sent: 215,
                total: 430
            }),
            "uploading: 215 of 430 frames (50%)"
        );
    }

    #[test]
    fn a_disabled_meter_never_draws() {
        let mut meter = Meter::new(false);

        meter.update(at(1));

        assert_eq!(meter.last_drawn, None);
    }
}
