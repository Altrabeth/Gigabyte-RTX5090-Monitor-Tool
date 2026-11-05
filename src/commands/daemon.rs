//! `aorus-lcd daemon`: keeps the panel's sensor feed running.

use aorus_lcd::daemon::{self, DaemonConfig, FeedStrategy};
use aorus_lcd::protocol::FeedFrameLen;
use aorus_lcd::{Error, Result};

/// How long `--dry-run` runs, since nothing else ends it unattended.
const DRY_RUN_TICKS: u64 = 3;

/// Arguments of `aorus-lcd daemon`.
#[derive(Debug, clap::Args)]
pub struct Args {
    /// Bytes per E3 frame: 256, the known-good spec, or 128, untested on a 5090
    #[arg(long, value_name = "256|128", value_parser = parse_frame_bytes)]
    pub frame_bytes: Option<FeedFrameLen>,

    /// steady sends every second. adaptive does too under 50% GPU usage, and from
    /// there up sends a 5-second summary, twice, every 6 seconds
    #[arg(long, value_name = "steady|adaptive")]
    pub feed: Option<FeedStrategy>,

    /// Stop after N ticks. For tests: it does nothing else
    #[arg(long, value_name = "N", hide = true)]
    pub max_ticks: Option<u64>,
}

/// Runs until SIGTERM or SIGINT, which is `Ok`. An error is what `main` should
/// exit non-zero on, so that systemd shows the failure.
pub fn run(args: Args, globals: &crate::Globals) -> Result<()> {
    let options = globals.options();
    daemon::run(config_from(args, options.dry_run), &options)
}

/// An option left out keeps the [`DaemonConfig`] default, which stays the one
/// place that defines it.
fn config_from(args: Args, dry_run: bool) -> DaemonConfig {
    let defaults = DaemonConfig::default();
    DaemonConfig {
        feed: args.frame_bytes.unwrap_or(defaults.feed),
        strategy: args.feed.unwrap_or(defaults.strategy),
        max_ticks: args.max_ticks.or(dry_run.then_some(DRY_RUN_TICKS)),
        ..defaults
    }
}

fn parse_frame_bytes(value: &str) -> Result<FeedFrameLen> {
    let bytes: usize = value.parse().map_err(|_| {
        Error::InvalidArgument(format!("an E3 frame must be 128 or 256 bytes, not {value}"))
    })?;
    FeedFrameLen::try_from(bytes)
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

    fn config_parsed_from(arguments: &[&str]) -> DaemonConfig {
        let line = ["daemon"].iter().chain(arguments);
        config_from(Harness::try_parse_from(line).unwrap().args, false)
    }

    #[test]
    fn without_options_the_config_defaults_apply() {
        assert_eq!(config_parsed_from(&[]), DaemonConfig::default());
    }

    #[test]
    fn the_frame_length_and_strategy_reach_the_config() {
        let config = config_parsed_from(&["--frame-bytes", "128", "--feed", "adaptive"]);
        assert_eq!(config.feed, FeedFrameLen::Truncated);
        assert_eq!(config.strategy, FeedStrategy::Adaptive);

        let config = config_parsed_from(&["--frame-bytes", "256", "--feed", "steady"]);
        assert_eq!(config.feed, FeedFrameLen::Full);
        assert_eq!(config.strategy, FeedStrategy::Steady);
    }

    #[test]
    fn other_frame_lengths_and_strategies_do_not_parse() {
        for value in ["0", "18", "64", "96", "127", "129", "512", "-1", "many", ""] {
            let line = ["daemon", "--frame-bytes", value];
            assert!(Harness::try_parse_from(line).is_err(), "{value}");
        }
        for value in ["burst", "Steady", ""] {
            let line = ["daemon", "--feed", value];
            assert!(Harness::try_parse_from(line).is_err(), "{value}");
        }
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
