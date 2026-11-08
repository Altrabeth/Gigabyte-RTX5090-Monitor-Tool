//! `aorus-lcd daemon`: keeps the panel's sensor feed running.

use std::path::PathBuf;

use aorus_lcd::config::{self, Resolved, Settings};
use aorus_lcd::daemon::{self, DaemonConfig, FeedStrategy};
use aorus_lcd::protocol::{parse_widgets, Elements, FeedFrameLen, Mode};
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

    /// Bytes per E3 frame: 256, the default, or 128, which is unreliable on this
    /// card. An RTX 5090 refused 21 of 163 writes at 128 bytes against 1 of 162 at
    /// 256, and status polls failed during the feed
    #[arg(long, value_name = "256|128", value_parser = parse_frame_bytes)]
    pub frame_bytes: Option<FeedFrameLen>,

    /// How often E3 goes out. adaptive, the default, sends a frame every second
    /// under 50% GPU usage, and from there up a pair of frames about every 6 s,
    /// carrying the peaks and averages of the last 5 s. steady sends a frame every
    /// second at any load. On an RTX 5090 each E3 stalled other NVIDIA driver calls
    /// by up to 35 ms, which is a proxy for the cost, not a measured frame drop
    #[arg(long, value_name = "steady|adaptive")]
    pub feed: Option<FeedStrategy>,

    /// Display mode to keep, as for `aorus-lcd mode`: E5 goes out on connect when
    /// the panel is in another mode, and the mode is read again once a minute (every
    /// 10 s for two minutes after a setup), so a mode set by hand is switched back. The panel forgets its mode on a power
    /// loss, and save does not keep it. Without this the mode is left alone
    #[arg(long, value_name = "0-8", value_parser = parse_mode)]
    pub mode: Option<Mode>,

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
            mode: self.mode,
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

/// The number as `aorus-lcd mode` takes it. The range is [`Mode::try_from`]'s.
fn parse_mode(value: &str) -> Result<Mode> {
    let number: u8 = value
        .parse()
        .map_err(|_| Error::InvalidArgument(format!("display mode must be 0 to 8, not {value}")))?;
    Mode::try_from(number)
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
    fn without_options_the_feed_is_adaptive_at_256_bytes() {
        let config = config_parsed_from(&[]);

        assert_eq!(config.strategy, FeedStrategy::Adaptive);
        assert_eq!(config.feed, FeedFrameLen::Full);
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
    fn the_mode_reaches_the_config_by_its_cli_number() {
        assert_eq!(config_parsed_from(&[]).mode, None);
        assert_eq!(
            config_parsed_from(&["--mode", "0"]).mode,
            Some(Mode::Faith1)
        );
        assert_eq!(
            config_parsed_from(&["--mode", "8"]).mode,
            Some(Mode::PresetRotation)
        );
    }

    #[test]
    fn bad_modes_do_not_parse() {
        for value in ["9", "-1", "256", "zero", ""] {
            let line = ["daemon", "--mode", value];
            assert!(Harness::try_parse_from(line).is_err(), "{value}");
        }
    }

    #[test]
    fn the_mode_on_the_command_line_beats_the_file() {
        let file = config::parse("mode = 8\n", "test.toml".as_ref()).unwrap();
        let args = Harness::try_parse_from(["daemon", "--mode", "0"])
            .unwrap()
            .args;

        let resolved = config::resolve(args.settings(None), &file).unwrap();

        assert_eq!(resolved.daemon.mode, Some(Mode::Faith1));
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

    #[test]
    fn the_help_describes_the_default_feed_and_warns_about_128_bytes() {
        let help = Harness::command().render_help().to_string();
        let help = help.split_whitespace().collect::<Vec<_>>().join(" ");

        for expected in [
            "adaptive, the default",
            "under 50% GPU usage",
            "a pair of frames about every 6 s",
            "peaks and averages",
            "not a measured frame drop",
            "unreliable on this card",
            "21 of 163 writes at 128 bytes against 1 of 162",
            "status polls failed during the feed",
        ] {
            assert!(help.contains(expected), "{expected}: {help}");
        }
    }
}
