//! The `aorus-lcd` command line: argument parsing and subcommand dispatch only.
//! Command logic lives in [`commands`], and below that in the library.

mod commands;

use std::error::Error as _;
use std::process::ExitCode;

use aorus_lcd::Result;
use clap::{Args, Parser, Subcommand};

/// Drive the LCD side panel of a Gigabyte AORUS RTX 5090 Master.
#[derive(Debug, Parser)]
#[command(name = "aorus-lcd", version)]
// The first subcommand reads these fields; remove the allow then.
#[allow(dead_code)]
struct Cli {
    #[command(flatten)]
    globals: Globals,

    #[command(subcommand)]
    command: Command,
}

/// Options shared by every subcommand.
#[derive(Debug, Args)]
#[allow(dead_code)]
pub struct Globals {
    /// Use I2C bus N instead of autodetecting; it must still be an NVIDIA adapter
    #[arg(long, value_name = "N", global = true)]
    pub bus: Option<u32>,

    /// Print the frames that would be sent instead of touching the bus
    #[arg(long, global = true)]
    pub dry_run: bool,

    /// Print more detail; repeat for more
    #[arg(short, long, action = clap::ArgAction::Count, global = true)]
    pub verbose: u8,
}

/// The subcommands. To add one, follow [`commands`], then add a variant such as
/// `Probe(commands::probe::Args)` here and its arm in [`run`]. Keep both lists
/// alphabetical, so that parallel branches touch different lines.
#[derive(Debug, Subcommand)]
enum Command {}

/// Runs the chosen subcommand. An arm looks like
/// `Command::Probe(args) => commands::probe::run(args, &cli.globals),`.
fn run(cli: Cli) -> Result<()> {
    match cli.command {}
}

// `Command` has no variants yet, so `Cli` cannot be constructed. The first
// subcommand removes this allow.
#[allow(unreachable_code)]
fn main() -> ExitCode {
    match run(Cli::parse()) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            report(&error);
            ExitCode::FAILURE
        }
    }
}

/// Prints an error and its causes. Variants' `Display` omits the source, so
/// nothing appears twice.
fn report(error: &aorus_lcd::Error) {
    eprintln!("error: {error}");
    let mut cause = error.source();
    while let Some(error) = cause {
        eprintln!("  caused by: {error}");
        cause = error.source();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::CommandFactory;

    #[test]
    fn cli_definition_is_valid() {
        Cli::command().debug_assert();
    }

    // Rendered in-process so the test never has to run the binary.
    #[test]
    fn help_lists_the_global_options() {
        let help = Cli::command().render_help().to_string();

        for option in ["--bus <N>", "--dry-run", "-v, --verbose"] {
            assert!(help.contains(option), "{option} missing from:\n{help}");
        }
    }

    #[test]
    fn a_subcommand_is_required() {
        assert!(Cli::try_parse_from(["aorus-lcd", "--dry-run"]).is_err());
    }
}
