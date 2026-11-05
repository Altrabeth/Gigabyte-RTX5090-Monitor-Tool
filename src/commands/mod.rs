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

pub mod probe;
