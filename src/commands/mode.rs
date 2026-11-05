//! `aorus-lcd mode`: selects the display mode.

use aorus_lcd::panel::Panel;
use aorus_lcd::protocol::Mode;
use aorus_lcd::session;
use aorus_lcd::Result;

/// Arguments of `aorus-lcd mode`.
#[derive(Debug, clap::Args)]
pub struct Args {
    /// 0-2 stat screens, 3 image, 4 text, 5 GIF, 6 chibi clock, 7 carousel, 8 preset rotation
    #[arg(value_name = "0-8")]
    pub mode: u8,
}

/// Sends `E5`. The number is checked first, because connecting already puts the
/// probe on the bus, or in a dry run prints it.
pub fn run(args: Args, globals: &crate::Globals) -> Result<()> {
    let mode = Mode::try_from(args.mode)?;
    Panel::new(session::connect(&globals.options())?).set_mode(mode)
}
