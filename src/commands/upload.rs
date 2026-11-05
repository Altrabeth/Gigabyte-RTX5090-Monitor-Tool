//! What every upload command shares: the path from a finished payload to the
//! panel. This is a helper of the upload commands, not a subcommand.

use std::io::{IsTerminal, Write};
use std::time::{Duration, Instant};

use aorus_lcd::bus::transport::Retrying;
use aorus_lcd::panel::{NoSleep, Panel, Sleep, ThreadSleep, UploadOptions, UploadProgress};
use aorus_lcd::protocol::UploadParams;
use aorus_lcd::session::{self, Session};
use aorus_lcd::Result;

use super::save;

/// Keeps the progress line to a few updates per second, however fast frames go.
const PROGRESS_INTERVAL: Duration = Duration::from_millis(250);

/// Connects and uploads `payload`, then with `persist` saves to NVRAM.
///
/// Call this last: it starts with the probe, so everything that can be refused
/// must have been refused by then. A real run pauses as the firmware needs and a
/// dry run does not, and only a real run on a terminal shows progress.
///
/// `AA` follows a successful upload only. It skips [`Retrying`], as the `save`
/// command does: a repeat would be a second NVRAM write.
pub fn send(
    payload: &[u8],
    params: &UploadParams,
    persist: bool,
    globals: &crate::Globals,
) -> Result<()> {
    let mut session = session::connect(&globals.options())?;
    let dry_run = matches!(session, Session::DryRun(_));
    let mut clock: Box<dyn Sleep> = if dry_run {
        Box::new(NoSleep)
    } else {
        Box::new(ThreadSleep)
    };
    let mut meter = Meter::new(!dry_run && std::io::stderr().is_terminal());

    let uploaded = Panel::new(Retrying::new(&mut session)).upload(
        payload,
        params,
        UploadOptions::default(),
        &mut *clock,
        |progress| meter.update(progress),
    );
    meter.finish();
    uploaded?;

    if persist {
        println!("{}", save::notice(&session));
        Panel::new(&mut session).save()?;
    }
    Ok(())
}

/// A progress line on stderr, away from the dump and any piped output.
struct Meter {
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

    fn update(&mut self, progress: UploadProgress) {
        let now = Instant::now();
        if self.enabled && is_due(self.last_drawn, now, progress) {
            let mut stderr = std::io::stderr();
            // Progress is a courtesy, so a closed stderr must not end an upload.
            let _ = write!(stderr, "\r{}", progress_line(progress));
            let _ = stderr.flush();
            self.last_drawn = Some(now);
        }
    }

    /// Ends the line, so that an error starts on its own.
    fn finish(&self) {
        if self.last_drawn.is_some() {
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
    use super::*;

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
