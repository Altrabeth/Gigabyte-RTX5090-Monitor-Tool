//! `aorus-lcd daemon`: keeps the panel's sensor feed running.

use aorus_lcd::daemon::{self, DaemonConfig};
use aorus_lcd::Result;

/// How long `--dry-run` runs, since nothing else ends it unattended.
const DRY_RUN_TICKS: u64 = 3;

/// Arguments of `aorus-lcd daemon`.
#[derive(Debug, clap::Args)]
pub struct Args {
    /// Stop after N ticks. For tests: it does nothing else
    #[arg(long, value_name = "N", hide = true)]
    pub max_ticks: Option<u64>,
}

/// Runs until SIGTERM or SIGINT, which is `Ok`. An error is what `main` should
/// exit non-zero on, so that systemd shows the failure.
pub fn run(args: Args, globals: &crate::Globals) -> Result<()> {
    let options = globals.options();
    let config = DaemonConfig {
        max_ticks: args.max_ticks.or(options.dry_run.then_some(DRY_RUN_TICKS)),
        ..DaemonConfig::default()
    };
    daemon::run(config, &options)
}

#[cfg(test)]
mod tests {
    use clap::{CommandFactory, Parser};

    use super::*;

    #[derive(Parser)]
    struct Harness {
        #[command(flatten)]
        args: Args,
    }

    #[test]
    fn max_ticks_is_unlimited_unless_asked() {
        let harness = Harness::try_parse_from(["daemon"]).unwrap();

        assert_eq!(harness.args.max_ticks, None);
    }

    #[test]
    fn max_ticks_takes_a_count_and_stays_out_of_the_help() {
        let harness = Harness::try_parse_from(["daemon", "--max-ticks", "7"]).unwrap();

        assert_eq!(harness.args.max_ticks, Some(7));
        let help = Harness::command().render_help().to_string();
        assert!(!help.contains("--max-ticks"), "{help}");
    }
}
