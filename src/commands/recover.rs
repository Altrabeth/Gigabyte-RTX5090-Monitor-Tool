//! `aorus-lcd recover`: repairs a frozen or dark panel.
//!
//! The sequence is [`Panel::recover`]. This file is the flags, the wording, and
//! the one thing only a command can know: how to get the daemon out of the way.
//!
//! The session is a [`Verified`](aorus_lcd::session::Verified), so the bus lock is
//! held from the probe to the last frame and the daemon cannot feed `E3` between
//! the steps. Its ticks are skipped while this runs.

use aorus_lcd::bus::transport::Retrying;
use aorus_lcd::panel::{Panel, RecoveryEvent, RecoveryOptions, RecoveryOverlay, RecoveryStep};
use aorus_lcd::protocol::Mode;
use aorus_lcd::session;
use aorus_lcd::{Error, Result};

use super::color::parse_hex_color;
use super::{save, upload};

/// What to do when the bus lock stays busy.
const BUS_BUSY_HINT: &str = "cannot recover while another process holds the bus: stop the daemon \
     with `systemctl stop aorus-lcd`, or let a running upload finish, then run recover again";

/// Printed last. A running daemon only skipped ticks and carries on by itself,
/// so this must not read as a general instruction to restart it.
const DAEMON_NOTE: &str = "If you stopped the daemon for this, start it again with \
     `systemctl start aorus-lcd`. If you did not stop it, there is nothing to restart.";

/// Arguments of `aorus-lcd recover`.
#[derive(Debug, clap::Args)]
pub struct Args {
    /// The mode to end in: 0-2 stat screens, 3 image, 4 text, 5 GIF, 6 chibi clock,
    /// 7 carousel, 8 preset rotation
    #[arg(long, value_name = "0-8", default_value_t = 0)]
    pub mode: u8,

    /// Fill colour of the repaint frame
    #[arg(long, value_name = "RRGGBB", default_value = "000000", value_parser = parse_hex_color)]
    pub color: [u8; 3],

    /// Skip the E7 off and on
    #[arg(long)]
    pub no_powercycle: bool,

    /// Skip the framebuffer upload; the rest of the sequence still runs
    #[arg(long)]
    pub no_upload: bool,

    /// Also send E1 with GPU temperature and TGP. Off by default because every
    /// recovery known to have sent it left the panel dark
    #[arg(long, conflicts_with = "clear_overlay")]
    pub overlay: bool,

    /// Send E1 with no widgets and interval 0. This is the one E1 of this command
    /// that removes the overlay rather than adds it. With no widgets configured,
    /// a daemon started afterwards puts its default widgets back
    #[arg(long)]
    pub clear_overlay: bool,

    /// Also save the panel configuration to NVRAM. Off by default so that a
    /// repair persists nothing, and because the run that lit a dark panel did not
    /// save
    #[arg(long)]
    pub save: bool,
}

impl Args {
    /// What the flags ask for. The mode is refused here and the colour by clap,
    /// both before connecting puts the probe on the bus.
    fn recovery_options(&self) -> Result<RecoveryOptions> {
        let overlay = if self.overlay {
            RecoveryOverlay::Enable
        } else if self.clear_overlay {
            RecoveryOverlay::Clear
        } else {
            RecoveryOverlay::Keep
        };
        Ok(RecoveryOptions {
            power_cycle: !self.no_powercycle,
            fill: (!self.no_upload).then_some(self.color),
            target: Mode::try_from(self.mode)?,
            overlay,
            save: self.save,
        })
    }
}

/// Runs the recovery, printing each step as it starts. Stdout carries them, as
/// it does the dry-run dump, so the two stay in order.
pub fn run(args: Args, globals: &crate::Globals) -> Result<()> {
    let options = args.recovery_options()?;
    let mut session = session::connect(&globals.options()).map_err(explain_busy_bus)?;
    let saving = save::notice(&session);
    let (mut clock, mut meter) = upload::pacing(&session);

    let recovered = Panel::new(Retrying::new(&mut session)).recover(
        &options,
        &mut *clock,
        |event| match event {
            RecoveryEvent::Upload(progress) => meter.update(progress),
            RecoveryEvent::Step(step) => {
                meter.finish();
                println!("{}", describe(step, saving));
            }
        },
    );
    meter.finish();
    recovered?;

    println!("{DAEMON_NOTE}");
    Ok(())
}

/// Turns a busy lock into advice. Everything else passes through as it is.
fn explain_busy_bus(error: Error) -> Error {
    match error {
        Error::LockBusy { .. } => Error::BusBusy {
            hint: BUS_BUSY_HINT.into(),
            source: Box::new(error),
        },
        error => error,
    }
}

fn describe(step: RecoveryStep, saving: &str) -> String {
    match step {
        RecoveryStep::PowerCycle => "Power-cycling the LCD...".into(),
        RecoveryStep::Upload([red, green, blue]) => {
            format!(
                "Uploading a fresh frame (#{red:02X}{green:02X}{blue:02X}) to clear the wedge..."
            )
        }
        RecoveryStep::SetMode(mode) => format!("Switching to mode {}...", mode.number()),
        RecoveryStep::ClearOverlay => "Clearing the sensor overlay...".into(),
        RecoveryStep::EnableOverlay => "Enabling the GPU-temp + TGP overlay...".into(),
        RecoveryStep::Save => saving.into(),
    }
}

#[cfg(test)]
mod tests {
    use std::error::Error as _;

    use clap::{CommandFactory, Parser};

    use super::*;

    #[derive(Parser)]
    struct Harness {
        #[command(flatten)]
        args: Args,
    }

    fn options(arguments: &[&str]) -> Result<RecoveryOptions> {
        let line = [&["recover"], arguments].concat();
        Harness::try_parse_from(line)
            .unwrap()
            .args
            .recovery_options()
    }

    #[test]
    fn no_flags_mean_the_library_default() {
        assert_eq!(options(&[]).unwrap(), RecoveryOptions::default());
    }

    #[test]
    fn each_flag_changes_its_own_option() {
        let default = RecoveryOptions::default();
        let cases = [
            (
                &["--no-powercycle"][..],
                RecoveryOptions {
                    power_cycle: false,
                    ..default
                },
            ),
            (
                &["--no-upload"],
                RecoveryOptions {
                    fill: None,
                    ..default
                },
            ),
            (
                &["--mode", "8"],
                RecoveryOptions {
                    target: Mode::PresetRotation,
                    ..default
                },
            ),
            (
                &["--color", "ff8800"],
                RecoveryOptions {
                    fill: Some([0xFF, 0x88, 0x00]),
                    ..default
                },
            ),
            (
                &["--overlay"],
                RecoveryOptions {
                    overlay: RecoveryOverlay::Enable,
                    ..default
                },
            ),
            (
                &["--clear-overlay"],
                RecoveryOptions {
                    overlay: RecoveryOverlay::Clear,
                    ..default
                },
            ),
            (
                &["--save"],
                RecoveryOptions {
                    save: true,
                    ..default
                },
            ),
        ];

        for (arguments, expected) in cases {
            assert_eq!(options(arguments).unwrap(), expected, "{arguments:?}");
        }
    }

    #[test]
    fn every_mode_up_to_eight_is_accepted_and_nine_is_not() {
        for mode in 0..=8u8 {
            let target = options(&["--mode", &mode.to_string()]).unwrap().target;
            assert_eq!(target.number(), mode);
        }
        assert!(matches!(
            options(&["--mode", "9"]),
            Err(Error::InvalidArgument(_))
        ));
    }

    #[test]
    fn overlay_and_clear_overlay_conflict() {
        assert!(Harness::try_parse_from(["recover", "--overlay", "--clear-overlay"]).is_err());
    }

    // Rendered in-process so the test never has to run the binary.
    #[test]
    fn help_says_why_overlay_and_save_are_off() {
        let help = Harness::command().render_help().to_string();
        let help = help.split_whitespace().collect::<Vec<_>>().join(" ");

        for phrase in [
            "Off by default because every recovery known to have sent it left the panel dark",
            "Off by default so that a repair persists nothing",
            "removes the overlay rather than adds it",
        ] {
            assert!(help.contains(phrase), "{phrase} missing from:\n{help}");
        }
    }

    #[test]
    fn a_busy_lock_names_the_way_to_stop_the_daemon_and_keeps_its_cause() {
        let error = explain_busy_bus(Error::LockBusy {
            path: "/run/test.lock".into(),
        });

        assert!(
            error.to_string().contains("systemctl stop aorus-lcd"),
            "{error}"
        );
        let cause = error.source().unwrap().to_string();
        assert!(cause.contains("/run/test.lock"), "{cause}");
    }

    #[test]
    fn other_connect_errors_pass_through() {
        let error = explain_busy_bus(Error::Adapter("none".into()));

        assert!(matches!(error, Error::Adapter(_)));
    }

    #[test]
    fn the_closing_note_only_asks_for_a_restart_after_a_stop() {
        assert!(DAEMON_NOTE.starts_with("If you stopped the daemon"));
        assert!(DAEMON_NOTE.contains("If you did not stop it, there is nothing to restart"));
    }
}
