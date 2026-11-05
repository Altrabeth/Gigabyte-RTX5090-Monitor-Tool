//! One file per subcommand, so adding a command touches no other command.
//!
//! A command lives in `src/commands/<name>.rs` and exposes its clap arguments
//! and an entry point:
//!
//! ```text
//! #[derive(clap::Args)]
//! pub struct Args { .. }
//!
//! pub fn run(args: Args, globals: &crate::Globals) -> aorus_lcd::Result<()> { .. }
//! ```
//!
//! Registering it takes one `pub mod <name>;` here, and one variant and one
//! match arm in `main.rs`.

pub mod carousel;
pub mod color;
pub mod daemon;
pub mod dashboard;
pub mod gif;
pub mod image;
pub mod mode;
pub mod off;
pub mod on;
pub mod probe;
pub mod recover;
pub mod save;
pub mod status;
pub mod text;
pub mod upload;
