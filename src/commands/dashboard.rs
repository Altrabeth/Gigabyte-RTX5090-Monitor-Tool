//! `aorus-lcd dashboard`: chooses the sensor widgets the panel shows.

use aorus_lcd::panel::Panel;
use aorus_lcd::protocol::{parse_widgets, Elements, DEFAULT_WIDGETS};
use aorus_lcd::session;
use aorus_lcd::Result;

/// Shown in `--help` and printed after sending. `E1` only picks the widgets:
/// `E3` carries their values, and this command sends no `AA`.
const NOTE: &str = "Note: this setting is not saved, and widgets freeze without a running \
                    `aorus-lcd daemon`.\nIf the panel goes dark, `aorus-lcd recover` is the way \
                    out.";

/// Arguments of `aorus-lcd dashboard`. Bad values stop in clap, before anything
/// connects.
#[derive(Debug, clap::Args)]
#[command(after_help = NOTE)]
pub struct Args {
    /// Widgets to show, comma-separated: temp, clock, usage, fan, ramclock,
    /// vram, fps, power
    #[arg(
        long,
        value_name = "LIST",
        default_value = DEFAULT_WIDGETS,
        value_parser = parse_widgets,
    )]
    pub widgets: Elements,

    /// Seconds before the panel rotates to the next widget, at least 1
    #[arg(
        long,
        value_name = "SECS",
        default_value_t = 3,
        value_parser = clap::value_parser!(u8).range(1..),
    )]
    pub interval: u8,

    /// Turn every widget off
    #[arg(long, conflicts_with_all = ["widgets", "interval"])]
    pub off: bool,
}

impl Args {
    /// What `E1` carries. Off is an interval of 0, as `PanelRecovery.cs` sends
    /// for its Clear case.
    fn display(&self) -> (Elements, u8) {
        if self.off {
            (Elements::NONE, 0)
        } else {
            (self.widgets, self.interval)
        }
    }
}

/// Sends `E1` once and prints the note.
///
/// It sends nothing else, and it does not go through
/// [`Retrying`](aorus_lcd::bus::transport::Retrying): `E1` is the prime suspect
/// in dark-panel incidents (`RECOVERY.md`), so a failed write is reported rather
/// than repeated.
pub fn run(args: Args, globals: &crate::Globals) -> Result<()> {
    let (elements, interval) = args.display();
    let session = session::connect(&globals.options())?;
    Panel::new(session).set_dashboard(elements, interval)?;
    println!("{NOTE}");
    Ok(())
}

#[cfg(test)]
mod tests {
    use aorus_lcd::protocol::Element;
    use clap::Parser;

    use super::*;

    #[derive(Parser)]
    struct Harness {
        #[command(flatten)]
        args: Args,
    }

    fn display_parsed_from(arguments: &[&str]) -> Option<(Elements, u8)> {
        Some(Harness::try_parse_from(arguments).ok()?.args.display())
    }

    #[test]
    fn the_defaults_are_the_readme_widgets_every_3_seconds() {
        let readme = [
            Element::GpuTemp,
            Element::GpuClock,
            Element::GpuUsage,
            Element::FanSpeed,
            Element::RamUsage,
            Element::Tgp,
        ];

        assert_eq!(
            display_parsed_from(&["dashboard"]),
            Some((readme.into_iter().collect(), 3))
        );
    }

    #[test]
    fn widgets_and_interval_are_taken_from_the_arguments() {
        assert_eq!(
            display_parsed_from(&["dashboard", "--widgets", "fps", "--interval", "9"]),
            Some((Elements::NONE.with(Element::Fps), 9))
        );
    }

    #[test]
    fn off_is_no_widgets_and_an_interval_of_zero() {
        assert_eq!(
            display_parsed_from(&["dashboard", "--off"]),
            Some((Elements::NONE, 0))
        );
    }

    #[test]
    fn an_interval_must_fit_and_be_at_least_one() {
        for interval in ["0", "256", "soon"] {
            assert_eq!(
                display_parsed_from(&["dashboard", "--interval", interval]),
                None,
                "{interval}"
            );
        }
    }
}
