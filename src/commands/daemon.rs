//! `aorus-lcd daemon`: keeps the panel's sensor feed running.

use std::path::PathBuf;

use aorus_lcd::config::{self, Resolved, Settings};
use aorus_lcd::daemon::{self, DaemonConfig, FeedStrategy};
use aorus_lcd::protocol::{parse_widgets, Elements, FeedFrameLen};
use aorus_lcd::{Error, Result};

/// How long `--dry-run` runs, since nothing else ends it unattended.
const DRY_RUN_TICKS: u64 = 3;

/// Arguments of `aorus-lcd daemon`.
#[derive(Debug, clap::Args)]
pub struct Args {
    /// Config file. Without this, /etc/aorus-lcd.toml is read if it exists
    #[arg(long, value_name = "PATH")]
    pub config: Option<PathBuf>,

    /// Widgets to keep on the panel, comma-separated: temp, clock, usage, fan,
    /// ramclock, vram, fps, power. This writes E1 when the panel differs; without
    /// widgets the panel keeps what it has
    #[arg(long, value_name = "LIST", value_parser = parse_widgets)]
    pub widgets: Option<Elements>,

    /// Seconds before the panel rotates to the next widget. Needs widgets, here or
    /// in the config, because it goes out with E1
    #[arg(
        long,
        value_name = "SECS",
        value_parser = clap::value_parser!(u8).range(1..),
    )]
    pub interval: Option<u8>,

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
/// exit non-zero on, so that systemd shows the failure. The config is settled
/// first, because a bad one has to stop this before anything connects.
pub fn run(args: Args, globals: &crate::Globals) -> Result<()> {
    let file = config::load(args.config.as_deref(), config::DEFAULT_PATH.as_ref())?;
    let Resolved { daemon, bus } = config::resolve(args.settings(globals.bus), &file)?;

    let mut options = globals.options();
    options.bus = bus;
    daemon::run(args.limited(daemon, options.dry_run), &options)
}

impl Args {
    /// What the command line sets. `bus` is the global `--bus`.
    fn settings(&self, bus: Option<u32>) -> Settings {
        Settings {
            widgets: self.widgets,
            interval: self.interval,
            feed: self.feed,
            frame_bytes: self.frame_bytes,
            bus,
        }
    }

    /// `config` with the tick limit, which no config file can set.
    fn limited(&self, config: DaemonConfig, dry_run: bool) -> DaemonConfig {
        DaemonConfig {
            max_ticks: self.max_ticks.or(dry_run.then_some(DRY_RUN_TICKS)),
            ..config
        }
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
    use aorus_lcd::config::FileConfig;
    use aorus_lcd::protocol::Element;
    use clap::{CommandFactory, Parser};

    use super::*;

    #[derive(Parser)]
    struct Harness {
        #[command(flatten)]
        args: Args,
    }

    fn config_parsed_from(arguments: &[&str]) -> DaemonConfig {
        let line = ["daemon"].iter().chain(arguments);
        let args = Harness::try_parse_from(line).unwrap().args;
        let resolved = config::resolve(args.settings(None), &FileConfig::default()).unwrap();
        args.limited(resolved.daemon, false)
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
    fn the_widgets_and_interval_reach_the_config() {
        let config = config_parsed_from(&["--widgets", "fps,temp", "--interval", "9"]);

        let expected = Elements::NONE.with(Element::Fps).with(Element::GpuTemp);
        assert_eq!(config.elements, Some(expected));
        assert_eq!(config.interval_s, 9);
    }

    #[test]
    fn bad_widgets_and_intervals_do_not_parse() {
        for value in ["", "nope", "temp,temp"] {
            let line = ["daemon", "--widgets", value];
            assert!(Harness::try_parse_from(line).is_err(), "{value}");
        }
        for value in ["0", "256", "soon", ""] {
            let line = ["daemon", "--interval", value];
            assert!(Harness::try_parse_from(line).is_err(), "{value}");
        }
    }

    #[test]
    fn the_global_bus_counts_as_the_command_line() {
        let args = Harness::try_parse_from(["daemon"]).unwrap().args;

        assert_eq!(args.settings(Some(4)).bus, Some(4));
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
