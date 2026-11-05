//! `aorus-lcd carousel`: stores which modes the panel cycles through.

use aorus_lcd::panel::Panel;
use aorus_lcd::protocol::{carousel_frame, Mode};
use aorus_lcd::session;
use aorus_lcd::{Error, Result};

/// Shown in `--help`. On the real panel `F3` left the mode as it was. Whether the
/// carousel then plays after `mode 7` has not been checked, so this does not
/// promise it.
const NOTE: &str = "Note: F3 only stores this list and does not change the mode, so \
                    `aorus-lcd mode 7` is needed to play it.";

/// Arguments of `aorus-lcd carousel`.
#[derive(Debug, clap::Args)]
#[command(after_help = NOTE)]
pub struct Args {
    /// Modes to cycle through in play order, such as 0,1,4; each one 0 to 6
    #[arg(value_name = "MODES", value_delimiter = ',', required = true)]
    pub modes: Vec<u8>,

    /// Interval, sent as it is in byte 5 of the F3 frame
    // Which unit the panel reads this in has not been confirmed, so there is no
    // conversion and no unit in the help. `docs/PROTOCOL.md` section 2.
    #[arg(long, value_name = "INTERVAL", default_value_t = 5)]
    pub interval: u8,
}

/// Sends `F3`. The list is checked in full first, because connecting already
/// puts the probe on the bus, or in a dry run prints it.
pub fn run(args: Args, globals: &crate::Globals) -> Result<()> {
    let modes = parse_modes(&args.modes)?;
    carousel_frame(args.interval, &modes)?;
    Panel::new(session::connect(&globals.options())?).set_carousel(args.interval, &modes)
}

/// Modes 7 and 8 pass here, for [`carousel_frame`] to refuse along with the
/// other list rules. Only the wording is this command's: `Mode`'s own error
/// would offer a 7 and an 8 that the next check turns down.
fn parse_modes(numbers: &[u8]) -> Result<Vec<Mode>> {
    numbers
        .iter()
        .map(|&number| {
            Mode::try_from(number).map_err(|_| {
                Error::InvalidArgument(format!("carousel modes must be 0 to 6, not {number}"))
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use clap::Parser;

    use super::*;

    #[derive(Parser)]
    struct Harness {
        #[command(flatten)]
        args: Args,
    }

    fn parsed(arguments: &[&str]) -> Option<Args> {
        Some(Harness::try_parse_from(arguments).ok()?.args)
    }

    #[test]
    fn the_list_is_comma_separated_and_keeps_its_order() {
        let args = parsed(&["carousel", "4,0,1"]).unwrap();

        assert_eq!(args.modes, [4, 0, 1]);
    }

    #[test]
    fn the_interval_defaults_to_five_and_is_kept_as_given() {
        assert_eq!(parsed(&["carousel", "0"]).unwrap().interval, 5);
        let args = parsed(&["carousel", "0", "--interval", "200"]).unwrap();
        assert_eq!(args.interval, 200);
    }

    #[test]
    fn the_interval_must_fit_its_byte() {
        assert!(parsed(&["carousel", "0", "--interval", "256"]).is_none());
    }

    #[test]
    fn numbers_become_modes() {
        assert_eq!(parse_modes(&[0, 5]).unwrap(), [Mode::Faith1, Mode::Gif]);
    }

    #[test]
    fn a_number_that_is_no_mode_is_refused_in_carousel_terms() {
        let error = parse_modes(&[0, 9]).unwrap_err();

        assert!(matches!(error, Error::InvalidArgument(_)), "{error:?}");
        assert_eq!(error.to_string(), "carousel modes must be 0 to 6, not 9");
    }
}
