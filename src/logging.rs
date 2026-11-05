//! A minimal logger, so that the library's `log` output has somewhere to go.

use log::{LevelFilter, Log, Metadata, Record};

/// Prints to stderr, which keeps stdout clean for command output and frames.
struct StderrLogger;

impl Log for StderrLogger {
    fn enabled(&self, metadata: &Metadata<'_>) -> bool {
        metadata.level() <= log::max_level()
    }

    fn log(&self, record: &Record<'_>) {
        if self.enabled(record.metadata()) {
            eprintln!("{}: {}", record.level(), record.args());
        }
    }

    fn flush(&self) {}
}

/// Installs the logger for a count of `-v`. Warnings always show.
pub fn init(verbose: u8) {
    // This fails only if a logger is already set, and then there is one.
    if log::set_logger(&StderrLogger).is_ok() {
        log::set_max_level(level_for(verbose));
    }
}

/// `sensors` logs at debug, so a single `-v` has to reach that level.
fn level_for(verbose: u8) -> LevelFilter {
    match verbose {
        0 => LevelFilter::Warn,
        1 => LevelFilter::Debug,
        _ => LevelFilter::Trace,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn each_v_shows_more() {
        assert_eq!(level_for(0), LevelFilter::Warn);
        assert_eq!(level_for(1), LevelFilter::Debug);
        assert_eq!(level_for(2), LevelFilter::Trace);
        assert_eq!(level_for(9), LevelFilter::Trace);
    }
}
