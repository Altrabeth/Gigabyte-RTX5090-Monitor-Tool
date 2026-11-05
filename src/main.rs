//! The `aorus-lcd` command line: argument parsing and subcommand dispatch only.
//! Command logic lives in [`commands`], and below that in the library.

mod commands;
mod logging;

use std::error::Error as _;
use std::process::ExitCode;

use aorus_lcd::session::Options;
use aorus_lcd::Result;
use clap::{Args, Parser, Subcommand};

/// Drive the LCD side panel of a Gigabyte AORUS RTX 5090 Master.
#[derive(Debug, Parser)]
#[command(name = "aorus-lcd", version)]
struct Cli {
    #[command(flatten)]
    globals: Globals,

    #[command(subcommand)]
    command: Command,
}

/// Options shared by every subcommand.
#[derive(Debug, Args)]
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

impl Globals {
    /// The connection these options ask for. Options that only some commands
    /// have, such as `--wait`, are left at their defaults.
    pub fn options(&self) -> Options {
        Options {
            bus: self.bus,
            dry_run: self.dry_run,
            verbose: self.verbose > 0,
            ..Options::default()
        }
    }
}

/// The subcommands. To add one, follow [`commands`], then add a variant such as
/// `Probe(commands::probe::Args)` here and its arm in [`run`]. Keep both lists
/// alphabetical, so that parallel branches touch different lines.
#[derive(Debug, Subcommand)]
enum Command {
    /// Choose the sensor widgets the panel shows
    Dashboard(commands::dashboard::Args),
    /// Cycle through display modes
    Carousel(commands::carousel::Args),
    /// Keep the panel's sensor feed running
    Daemon(commands::daemon::Args),
    /// Play an animated GIF
    Gif(commands::gif::Args),
    /// Show an image
    Image(commands::image::Args),
    /// Select the display mode
    Mode(commands::mode::Args),
    /// Turn the panel off
    Off(commands::off::Args),
    /// Turn the panel on
    On(commands::on::Args),
    /// Check that the panel controller answers, and show its status
    Probe(commands::probe::Args),
    /// Save the panel configuration: this writes to the panel's NVRAM
    ///
    /// In text mode the same frame also starts the text rainbow effect.
    Save(commands::save::Args),
    /// Show firmware, mode, on-state, dashboard and carousel banks
    Status(commands::status::Args),
    /// Show text
    ///
    /// By default this also starts the rainbow effect, whose frame saves the panel
    /// configuration to NVRAM. Use --no-effect to avoid that write.
    Text(commands::text::Args),
}

/// Runs the chosen subcommand. An arm looks like
/// `Command::Probe(args) => commands::probe::run(args, &cli.globals),`.
fn run(cli: Cli) -> Result<()> {
    match cli.command {
        Command::Dashboard(args) => commands::dashboard::run(args, &cli.globals),
        Command::Carousel(args) => commands::carousel::run(args, &cli.globals),
        Command::Daemon(args) => commands::daemon::run(args, &cli.globals),
        Command::Gif(args) => commands::gif::run(args, &cli.globals),
        Command::Image(args) => commands::image::run(args, &cli.globals),
        Command::Mode(args) => commands::mode::run(args, &cli.globals),
        Command::Off(args) => commands::off::run(args, &cli.globals),
        Command::On(args) => commands::on::run(args, &cli.globals),
        Command::Probe(args) => commands::probe::run(args, &cli.globals),
        Command::Save(args) => commands::save::run(args, &cli.globals),
        Command::Status(args) => commands::status::run(args, &cli.globals),
        Command::Text(args) => commands::text::run(args, &cli.globals),
    }
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    logging::init(cli.globals.verbose);
    match run(cli) {
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

    #[test]
    fn global_options_become_session_options() {
        let cli = Cli::try_parse_from(["aorus-lcd", "probe", "--bus", "3", "--dry-run", "-v"]);

        assert_eq!(
            cli.unwrap().globals.options(),
            Options {
                bus: Some(3),
                dry_run: true,
                verbose: true,
                ..Options::default()
            }
        );
    }

    #[test]
    fn nothing_is_a_dry_run_unless_asked() {
        let cli = Cli::try_parse_from(["aorus-lcd", "probe"]).unwrap();

        assert_eq!(cli.globals.options(), Options::default());
    }
}
