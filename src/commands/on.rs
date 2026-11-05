//! `aorus-lcd on`: turns the panel on.

use aorus_lcd::panel::Panel;
use aorus_lcd::session;
use aorus_lcd::Result;

/// Arguments of `aorus-lcd on`.
#[derive(Debug, clap::Args)]
pub struct Args {}

/// Sends `E7`.
pub fn run(_args: Args, globals: &crate::Globals) -> Result<()> {
    Panel::new(session::connect(&globals.options())?).on()
}
