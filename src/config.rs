//! The daemon's config file and how it merges with the command line.
//!
//! The file is read once at start, and a bad one stops the daemon before anything
//! touches the bus: a non-zero exit is what makes systemd show the failure. See
//! `dist/aorus-lcd.toml` for the keys.
//!
//! `widgets` is one comma-separated string rather than a TOML array, so that it
//! has the syntax of `--widgets` and goes through the same [`parse_widgets`].
//!
//! # Why `interval` needs `widgets`
//!
//! The interval only reaches the panel inside `E1`, and without named widgets the
//! daemon leaves a configured panel alone, as the
//! [daemon's module docs](crate::daemon#when-e1-goes-out) explain. So an interval
//! on its own would do nothing, and making it work would mean an `E1` nobody asked
//! for, which is the prime suspect in dark-panel incidents. That is rejected
//! rather than ignored, after merging, since either source can supply either key.

use std::fs;
use std::io;
use std::ops::Range;
use std::path::Path;

use serde::Deserialize;
use toml::Spanned;

use crate::daemon::{DaemonConfig, FeedStrategy};
use crate::protocol::{parse_widgets, Elements, FeedFrameLen, Mode};
use crate::{Error, Result};

/// Where the daemon looks for a config file when `--config` does not say.
pub const DEFAULT_PATH: &str = "/etc/aorus-lcd.toml";

/// What one source, the file or the command line, sets. `None` leaves a setting
/// to the next source.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Settings {
    /// Widgets to show.
    pub widgets: Option<Elements>,
    /// Seconds between widgets, 1 to 255.
    pub interval: Option<u8>,
    /// How often an `E3` goes out.
    pub feed: Option<FeedStrategy>,
    /// The length of each `E3`.
    pub frame_bytes: Option<FeedFrameLen>,
    /// The I2C bus to use instead of autodetecting.
    pub bus: Option<u32>,
    /// The display mode to set on each connection.
    pub mode: Option<Mode>,
}

impl Settings {
    /// These settings, with `lower` filling in what they leave out.
    fn over(self, lower: Settings) -> Settings {
        Settings {
            widgets: self.widgets.or(lower.widgets),
            interval: self.interval.or(lower.interval),
            feed: self.feed.or(lower.feed),
            frame_bytes: self.frame_bytes.or(lower.frame_bytes),
            bus: self.bus.or(lower.bus),
            mode: self.mode.or(lower.mode),
        }
    }
}

/// A config file that parsed. The default value is the empty file.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct FileConfig {
    /// What the file sets.
    pub settings: Settings,
    /// Where the file sets the interval, for the error that [`resolve`] may raise
    /// once the command line is known.
    interval_at: Option<String>,
}

/// The settings that are left once every source has had its say.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Resolved {
    /// The daemon's config. `max_ticks` is left to the caller.
    pub daemon: DaemonConfig,
    /// The bus for the session options.
    pub bus: Option<u32>,
}

/// The file's keys before their values are checked. Numbers are kept wide, so that
/// a value out of range reaches our check and its message.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Keys {
    widgets: Option<Spanned<String>>,
    interval: Option<Spanned<i64>>,
    feed: Option<Spanned<String>>,
    frame_bytes: Option<Spanned<i64>>,
    bus: Option<Spanned<i64>>,
    mode: Option<Spanned<i64>>,
}

/// A config file's text, to turn byte offsets into something a person can use.
struct Source<'a> {
    path: &'a Path,
    text: &'a str,
}

impl Source<'_> {
    fn location(&self, span: Range<usize>) -> String {
        let before = self.text.get(..span.start).unwrap_or(self.text);
        let line = before.matches('\n').count() + 1;
        let line_start = before.rfind('\n').map_or(0, |newline| newline + 1);
        let column = before[line_start..].chars().count() + 1;
        format!("{} line {line}, column {column}", self.path.display())
    }

    fn error(&self, span: Range<usize>, message: impl std::fmt::Display) -> Error {
        Error::Config(format!("{}: {message}", self.location(span)))
    }

    /// Runs `check` on a key's value, if it has one, and places any error there.
    fn checked<T, U>(
        &self,
        value: Option<Spanned<T>>,
        check: impl FnOnce(T) -> Result<U>,
    ) -> Result<Option<U>> {
        value
            .map(|value| {
                let span = value.span();
                check(value.into_inner()).map_err(|error| self.error(span, error))
            })
            .transpose()
    }
}

fn interval_from(seconds: i64) -> Result<u8> {
    u8::try_from(seconds)
        .ok()
        .filter(|&seconds| seconds >= 1)
        .ok_or_else(|| {
            Error::InvalidArgument(format!("interval must be 1 to 255 seconds, not {seconds}"))
        })
}

fn frame_bytes_from(bytes: i64) -> Result<FeedFrameLen> {
    usize::try_from(bytes)
        .ok()
        .and_then(|bytes| FeedFrameLen::try_from(bytes).ok())
        .ok_or_else(|| {
            Error::InvalidArgument(format!("frame_bytes must be 256 or 128, not {bytes}"))
        })
}

/// Leaves the range to [`Mode::try_from`], so the file and `--mode` agree.
fn mode_from(number: i64) -> Result<Mode> {
    u8::try_from(number)
        .map_err(|_| Error::InvalidArgument(format!("display mode must be 0 to 8, not {number}")))
        .and_then(Mode::try_from)
}

fn bus_from(bus: i64) -> Result<u32> {
    u32::try_from(bus)
        .map_err(|_| Error::InvalidArgument(format!("bus must be a bus number, not {bus}")))
}

/// Parses and validates the text of a config file. `path` is for messages.
///
/// # Errors
///
/// [`Error::Config`] naming the line and column of a syntax error, an unknown key
/// or a bad value.
pub fn parse(text: &str, path: &Path) -> Result<FileConfig> {
    let source = Source { path, text };
    let keys: Keys = toml::from_str(text)
        .map_err(|error| source.error(error.span().unwrap_or(0..0), error.message()))?;

    let interval_at = keys.interval.as_ref().map(|at| source.location(at.span()));
    let settings = Settings {
        widgets: source.checked(keys.widgets, |list| parse_widgets(&list))?,
        interval: source.checked(keys.interval, interval_from)?,
        feed: source.checked(keys.feed, |name| name.parse())?,
        frame_bytes: source.checked(keys.frame_bytes, frame_bytes_from)?,
        bus: source.checked(keys.bus, bus_from)?,
        mode: source.checked(keys.mode, mode_from)?,
    };
    Ok(FileConfig {
        settings,
        interval_at,
    })
}

/// Reads the config file: `explicit` if given, otherwise `default`.
///
/// Only a `default` that does not exist means no config. Anything else that stops
/// the read is an error, because a config someone wrote and the daemon cannot read
/// must not be silently ignored.
///
/// # Errors
///
/// [`Error::Input`] if the file cannot be read or is not UTF-8, including an
/// `explicit` file that does not exist, and whatever [`parse`] returns.
pub fn load(explicit: Option<&Path>, default: &Path) -> Result<FileConfig> {
    let path = explicit.unwrap_or(default);
    let bytes = match fs::read(path) {
        Ok(bytes) => bytes,
        Err(error) if explicit.is_none() && is_absent(path, &error) => {
            log::info!("no config file at {}: using defaults", path.display());
            return Ok(FileConfig::default());
        }
        Err(source) => {
            return Err(Error::Input {
                context: format!("cannot read config file {}", path.display()),
                source: source.into(),
            })
        }
    };
    let text = String::from_utf8(bytes).map_err(|source| Error::Input {
        context: format!("config file {} is not UTF-8", path.display()),
        source: source.into(),
    })?;
    let config = parse(&text, path)?;
    log::info!("read config file {}", path.display());
    Ok(config)
}

/// Whether nothing is there. A dangling symlink also reads as not found, but it
/// is a config someone installed.
fn is_absent(path: &Path, error: &io::Error) -> bool {
    error.kind() == io::ErrorKind::NotFound && fs::symlink_metadata(path).is_err()
}

/// Merges the command line over the file, over [`DaemonConfig`]'s defaults.
///
/// # Errors
///
/// [`Error::Config`] if the result has an interval and no widgets: see the
/// [module docs](self).
pub fn resolve(cli: Settings, file: &FileConfig) -> Result<Resolved> {
    let merged = cli.over(file.settings);
    if merged.interval.is_some() && merged.widgets.is_none() {
        // With no interval on the command line, it came from the file.
        let origin = match (cli.interval, &file.interval_at) {
            (None, Some(location)) => location.as_str(),
            _ => "--interval",
        };
        return Err(Error::Config(format!(
            "{origin}: an interval needs widgets: it goes out with E1, and E1 is only sent when \
             widgets are named. Set widgets too, or leave the interval out"
        )));
    }

    let defaults = DaemonConfig::default();
    Ok(Resolved {
        daemon: DaemonConfig {
            elements: merged.widgets,
            interval_s: merged.interval.unwrap_or(defaults.interval_s),
            feed: merged.frame_bytes.unwrap_or(defaults.feed),
            strategy: merged.feed.unwrap_or(defaults.strategy),
            mode: merged.mode,
            ..defaults
        },
        bus: merged.bus,
    })
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::{symlink, PermissionsExt};
    use std::path::PathBuf;

    use tempfile::TempDir;

    use super::*;
    use crate::protocol::Element;

    const EXAMPLE: &str = include_str!("../dist/aorus-lcd.toml");

    fn parsed(text: &str) -> Result<Settings> {
        Ok(parse(text, "test.toml".as_ref())?.settings)
    }

    fn message(text: &str) -> String {
        parsed(text).unwrap_err().to_string()
    }

    fn fps() -> Elements {
        Elements::NONE.with(Element::Fps)
    }

    #[test]
    fn every_key_parses() {
        let text = r#"
            widgets = "fps,temp"
            interval = 255
            feed = "adaptive"
            frame_bytes = 128
            bus = 3
            mode = 8
        "#;

        assert_eq!(
            parsed(text).unwrap(),
            Settings {
                widgets: Some(fps().with(Element::GpuTemp)),
                interval: Some(255),
                feed: Some(FeedStrategy::Adaptive),
                frame_bytes: Some(FeedFrameLen::Truncated),
                bus: Some(3),
                mode: Some(Mode::PresetRotation),
            }
        );
    }

    #[test]
    fn the_other_values_parse_too() {
        let settings =
            parsed("interval = 1\nfeed = \"steady\"\nframe_bytes = 256\nmode = 0\n").unwrap();

        assert_eq!(settings.interval, Some(1));
        assert_eq!(settings.mode, Some(Mode::Faith1));
        assert_eq!(settings.feed, Some(FeedStrategy::Steady));
        assert_eq!(settings.frame_bytes, Some(FeedFrameLen::Full));
    }

    #[test]
    fn files_with_nothing_set_are_all_defaults() {
        for text in ["", "\n\n", "# only a comment\n"] {
            assert_eq!(parse(text, "x".as_ref()).unwrap(), FileConfig::default());
        }
    }

    #[test]
    fn every_rejection_names_the_line_and_says_what_is_wrong() {
        let cases = [
            // Unknown keys and sections.
            ("# c\n\nwidget = \"fps\"\n", 3, "unknown field `widget`"),
            ("feed = \"steady\"\ninterval_s = 3\n", 2, "unknown field"),
            ("[daemon]\nfeed = \"steady\"\n", 1, "unknown field `daemon`"),
            // Out of range.
            ("\ninterval = 0\n", 2, "1 to 255 seconds, not 0"),
            ("\n\ninterval = 256\n", 3, "1 to 255 seconds, not 256"),
            ("interval = -1\n", 1, "1 to 255 seconds, not -1"),
            ("\nframe_bytes = 64\n", 2, "256 or 128, not 64"),
            ("frame_bytes = -128\n", 1, "256 or 128, not -128"),
            ("\nbus = -1\n", 2, "a bus number, not -1"),
            ("bus = 4294967296\n", 1, "a bus number, not 4294967296"),
            ("\nmode = 9\n", 2, "display mode must be 0 to 8, not 9"),
            ("mode = -1\n", 1, "display mode must be 0 to 8, not -1"),
            ("mode = 256\n", 1, "display mode must be 0 to 8, not 256"),
            // Bad names.
            (
                "\n\n\nfeed = \"burst\"\n",
                4,
                "steady or adaptive, not burst",
            ),
            ("feed = \"Steady\"\n", 1, "steady or adaptive, not Steady"),
            ("\nwidgets = \"temp,nope\"\n", 2, "unknown widget \"nope\""),
            ("widgets = \"temp,temp\"\n", 1, "listed twice"),
            ("widgets = \"temp, fan\"\n", 1, "unknown widget \" fan\""),
            ("\nwidgets = \"\"\n", 2, "the widget list is empty"),
            // Wrong types.
            ("\nwidgets = [\"temp\"]\n", 2, "a string"),
            ("interval = \"3\"\n", 1, "expected i64"),
            ("\ninterval = 3.5\n", 2, "expected i64"),
            ("frame_bytes = \"256\"\n", 1, "expected i64"),
            ("feed = 1\n", 1, "a string"),
            ("bus = true\n", 1, "expected i64"),
            ("mode = \"0\"\n", 1, "expected i64"),
            // Not TOML.
            ("\nfeed = \n", 2, ""),
            ("feed = \"steady\"\nfeed = \"steady\"\n", 2, "duplicate"),
        ];

        for (text, line, expected) in cases {
            let message = message(text);

            assert!(
                message.starts_with("invalid config: test.toml line "),
                "{text:?}: {message}"
            );
            assert!(
                message.contains(&format!(" line {line}, column ")),
                "{text:?}: {message}"
            );
            assert!(message.contains(expected), "{text:?}: {message}");
        }
    }

    #[test]
    fn unknown_keys_and_bad_values_report_the_column() {
        assert!(message("\n  nope = 1\n").contains("test.toml line 2, column 3:"));
        assert!(message("interval = 0\n").contains("test.toml line 1, column 12:"));
    }

    fn cli(widgets: Option<Elements>, interval: Option<u8>) -> Settings {
        Settings {
            widgets,
            interval,
            ..Settings::default()
        }
    }

    fn file(text: &str) -> FileConfig {
        parse(text, "test.toml".as_ref()).unwrap()
    }

    #[test]
    fn the_command_line_beats_the_file_and_the_file_beats_the_defaults() {
        let temp = Some(Elements::NONE.with(Element::GpuTemp));
        let defaults = DaemonConfig::default();
        let all = "widgets = \"temp\"\ninterval = 7\nfeed = \"steady\"\n\
                   frame_bytes = 128\nbus = 1\nmode = 8\n";
        let every_option = Settings {
            widgets: Some(fps()),
            interval: Some(9),
            feed: Some(FeedStrategy::Adaptive),
            frame_bytes: Some(FeedFrameLen::Full),
            bus: Some(2),
            mode: Some(Mode::Faith1),
        };
        let from_file = DaemonConfig {
            elements: temp,
            interval_s: 7,
            feed: FeedFrameLen::Truncated,
            strategy: FeedStrategy::Steady,
            mode: Some(Mode::PresetRotation),
            ..defaults
        };
        let from_options = DaemonConfig {
            elements: Some(fps()),
            interval_s: 9,
            mode: Some(Mode::Faith1),
            ..defaults
        };

        let cases = [
            // Nothing set.
            (Settings::default(), "", defaults, None),
            // Each key from the file alone.
            (
                Settings::default(),
                "widgets = \"temp\"\n",
                DaemonConfig {
                    elements: temp,
                    ..defaults
                },
                None,
            ),
            (
                Settings::default(),
                "feed = \"steady\"\n",
                DaemonConfig {
                    strategy: FeedStrategy::Steady,
                    ..defaults
                },
                None,
            ),
            (
                Settings::default(),
                "frame_bytes = 128\n",
                DaemonConfig {
                    feed: FeedFrameLen::Truncated,
                    ..defaults
                },
                None,
            ),
            (Settings::default(), "bus = 1\n", defaults, Some(1)),
            (
                Settings::default(),
                "mode = 3\n",
                DaemonConfig {
                    mode: Some(Mode::Image),
                    ..defaults
                },
                None,
            ),
            // Everything from one source.
            (Settings::default(), all, from_file, Some(1)),
            (every_option, "", from_options, Some(2)),
            // Everything from both: the command line wins each one, even with a
            // value that equals the default.
            (every_option, all, from_options, Some(2)),
            // One key on the command line leaves the rest of the file in place.
            (
                cli(None, Some(9)),
                all,
                DaemonConfig {
                    interval_s: 9,
                    ..from_file
                },
                Some(1),
            ),
            (
                cli(Some(fps()), None),
                all,
                DaemonConfig {
                    elements: Some(fps()),
                    ..from_file
                },
                Some(1),
            ),
            (
                Settings {
                    bus: Some(2),
                    ..Settings::default()
                },
                all,
                from_file,
                Some(2),
            ),
            (
                Settings {
                    mode: Some(Mode::Faith1),
                    ..Settings::default()
                },
                all,
                DaemonConfig {
                    mode: Some(Mode::Faith1),
                    ..from_file
                },
                Some(1),
            ),
        ];

        for (index, (cli, text, daemon, bus)) in cases.into_iter().enumerate() {
            assert_eq!(
                resolve(cli, &file(text)).unwrap(),
                Resolved { daemon, bus },
                "case {index}"
            );
        }
    }

    #[test]
    fn only_a_widgets_key_names_widgets() {
        let text = "feed = \"adaptive\"\nframe_bytes = 128\nbus = 1\n";

        let resolved = resolve(Settings::default(), &file(text)).unwrap();

        assert_eq!(resolved.daemon.elements, None);
    }

    #[test]
    fn an_interval_needs_widgets_from_either_source() {
        let widgets = "widgets = \"fps\"\n";
        let interval = "\ninterval = 9\n";
        let both = "widgets = \"fps\"\ninterval = 9\n";

        let allowed = [
            (cli(Some(fps()), Some(9)), ""),
            (cli(Some(fps()), None), interval),
            (cli(None, Some(9)), widgets),
            (cli(None, None), both),
            (cli(Some(fps()), Some(9)), both),
        ];
        for (index, (cli, text)) in allowed.into_iter().enumerate() {
            let daemon = resolve(cli, &file(text)).unwrap().daemon;

            assert_eq!(daemon.elements, Some(fps()), "case {index}");
            assert_eq!(daemon.interval_s, 9, "case {index}");
        }

        let refused = [
            (cli(None, None), interval, "test.toml line 2, column 12:"),
            (cli(None, Some(9)), "", "--interval:"),
            (cli(None, Some(9)), interval, "--interval:"),
        ];
        for (index, (cli, text, origin)) in refused.into_iter().enumerate() {
            let message = resolve(cli, &file(text)).unwrap_err().to_string();

            assert!(message.contains(origin), "case {index}: {message}");
            assert!(message.contains("goes out with E1"), "case {index}");
            assert!(message.contains("only sent when"), "case {index}");
        }
    }

    /// A directory, the path of a config in it, and the path of a file that is
    /// not there.
    fn scratch() -> (TempDir, PathBuf, PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let (path, absent) = (dir.path().join("config.toml"), dir.path().join("absent"));
        (dir, path, absent)
    }

    #[test]
    fn a_missing_default_file_means_defaults() {
        let (_dir, _, absent) = scratch();

        assert_eq!(load(None, &absent).unwrap(), FileConfig::default());
    }

    #[test]
    fn the_default_file_is_read_when_it_exists() {
        let (_dir, path, _) = scratch();
        fs::write(&path, "bus = 5\n").unwrap();

        assert_eq!(load(None, &path).unwrap().settings.bus, Some(5));
    }

    #[test]
    fn an_explicit_file_replaces_the_default() {
        let (_dir, path, absent) = scratch();
        fs::write(&path, "bus = 5\n").unwrap();

        let loaded = load(Some(&path), &absent).unwrap();

        assert_eq!(loaded.settings.bus, Some(5));
    }

    #[test]
    fn a_missing_explicit_file_is_an_error() {
        let (_dir, path, absent) = scratch();
        fs::write(&path, "bus = 5\n").unwrap();

        let error = load(Some(&absent), &path).unwrap_err();

        assert!(error.to_string().contains("absent"), "{error}");
    }

    #[test]
    fn a_bad_default_file_is_an_error_and_names_its_line() {
        let (_dir, path, _) = scratch();
        fs::write(&path, "\nnope = 1\n").unwrap();

        let error = load(None, &path).unwrap_err().to_string();

        assert!(error.contains("config.toml line 2, column 1:"), "{error}");
    }

    #[test]
    fn invalid_utf8_is_an_error_even_at_the_default_path() {
        let (_dir, path, _) = scratch();
        fs::write(&path, b"feed = \"\xff\"\n").unwrap();

        for explicit in [None, Some(path.as_path())] {
            let error = load(explicit, &path).unwrap_err();

            assert!(matches!(error, Error::Input { .. }), "{error:?}");
        }
    }

    #[test]
    fn a_directory_at_the_default_path_is_an_error() {
        let (dir, _, _) = scratch();

        let error = load(None, dir.path()).unwrap_err();

        assert!(matches!(error, Error::Input { .. }), "{error:?}");
    }

    #[test]
    fn a_file_without_read_permission_is_an_error_at_the_default_path() {
        let (_dir, path, _) = scratch();
        fs::write(&path, "").unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o000)).unwrap();
        if fs::read(&path).is_ok() {
            return; // Root reads anything, so this says nothing there.
        }

        let error = load(None, &path).unwrap_err();

        assert!(matches!(error, Error::Input { .. }), "{error:?}");
    }

    #[test]
    fn a_dangling_symlink_at_the_default_path_is_an_error() {
        let (_dir, path, absent) = scratch();
        symlink(&absent, &path).unwrap();

        let error = load(None, &path).unwrap_err();

        assert!(matches!(error, Error::Input { .. }), "{error:?}");
    }

    #[test]
    fn the_shipped_example_changes_nothing_as_installed() {
        assert_eq!(
            parse(EXAMPLE, "example".as_ref()).unwrap(),
            FileConfig::default()
        );
    }

    /// Settings are commented out as `#key = value`, and prose as `# text`.
    #[test]
    fn the_shipped_example_is_valid_with_every_key_uncommented() {
        let uncommented = EXAMPLE
            .lines()
            .map(|line| match line.strip_prefix('#') {
                Some(setting) if !setting.is_empty() && !setting.starts_with(' ') => setting,
                _ => line,
            })
            .collect::<Vec<_>>()
            .join("\n");

        let config = parse(&uncommented, "example".as_ref()).unwrap();

        let settings = config.settings;
        assert!(settings.widgets.is_some() && settings.interval.is_some());
        assert!(settings.feed.is_some() && settings.frame_bytes.is_some());
        assert!(settings.bus.is_some() && settings.mode.is_some());
        assert!(resolve(Settings::default(), &config).is_ok());
    }
}
