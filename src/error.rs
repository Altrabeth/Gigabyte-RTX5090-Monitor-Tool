//! The crate-wide error type.
//!
//! The variants are declared up front, one per kind of failure a layer has to
//! report, so a module can use them without editing this file. Callers branch
//! on some of them: dry-run tolerates [`Error::ReadbackUnavailable`], the
//! daemon skips a tick on [`Error::LockBusy`] and reconnects on [`Error::I2c`].
//!
//! `Display` describes one level only and never repeats the source, because
//! `main` prints the whole [`std::error::Error::source`] chain.

use std::path::PathBuf;

/// Any error from an external library, kept as the source of a variant.
type BoxedSource = Box<dyn std::error::Error + Send + Sync>;

/// Shorthand for a result carrying this crate's [`Error`].
pub type Result<T> = std::result::Result<T, Error>;

/// Everything that can go wrong in this crate.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum Error {
    /// No usable I2C adapter: none found, or `--bus` named a refused one. The
    /// message should list what was seen.
    #[error("{0}")]
    Adapter(String),

    /// An I2C device operation failed. `context` says what was attempted and
    /// what the errno likely means; `source.raw_os_error()` tells transient
    /// faults from permanent ones.
    #[error("{context}")]
    I2c {
        /// What was attempted, with a hint for the user.
        context: String,
        /// The OS error.
        source: std::io::Error,
    },

    /// A read returned fewer bytes than asked for. Never treat this as
    /// presence: the probe gate requires the full read-back.
    #[error("short read: expected {expected} bytes, got {actual}")]
    ShortRead {
        /// Bytes asked for.
        expected: usize,
        /// Bytes received.
        actual: usize,
    },

    /// A read was attempted in dry-run mode, where no device exists.
    #[error("read-backs are unavailable in dry-run mode")]
    ReadbackUnavailable,

    /// The bus lock file could not be created or opened. There is deliberately
    /// no fallback path: a second path would break mutual exclusion.
    #[error(
        "cannot open bus lock {}: check the aorus-lcd tmpfiles.d entry and membership of the i2c group, or until it is installed set {} to a writable path shared by every aorus-lcd process",
        path.display(),
        crate::bus::lock::LOCK_PATH_ENV
    )]
    Lock {
        /// The lock file.
        path: PathBuf,
        /// The OS error.
        source: std::io::Error,
    },

    /// Another process holds the bus lock.
    #[error("bus lock {} is held by another process", path.display())]
    LockBusy {
        /// The lock file.
        path: PathBuf,
    },

    /// A caller-supplied value was rejected. Validate before touching the bus.
    #[error("{0}")]
    InvalidArgument(String),

    /// The controller answered with bytes that do not parse.
    #[error("malformed response from the panel: {0}")]
    Protocol(String),

    /// An upload failed part-way through. `frame` is the index of the frame
    /// that was lost, counted from the start of the upload.
    #[error("upload failed at frame {frame}")]
    Upload {
        /// Index of the lost frame.
        frame: usize,
        /// Why that frame failed.
        source: Box<Error>,
    },

    /// A user input file could not be read or decoded.
    #[error("{context}")]
    Input {
        /// Which input, and what was attempted.
        context: String,
        /// The underlying I/O or decoder error.
        source: BoxedSource,
    },

    /// The sensor library failed as a whole. A single failed sensor is not an
    /// error: it zeroes that field of the sample.
    #[error("{context}")]
    Sensor {
        /// What was attempted.
        context: String,
        /// The underlying library error.
        source: BoxedSource,
    },

    /// The config file is invalid. The message names the line.
    #[error("invalid config: {0}")]
    Config(String),
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::error::Error as _;

    #[test]
    fn upload_error_keeps_the_cause_in_the_source_chain() {
        let cause = Error::ShortRead {
            expected: 8,
            actual: 3,
        };
        let error = Error::Upload {
            frame: 7,
            source: Box::new(cause),
        };

        assert_eq!(error.to_string(), "upload failed at frame 7");
        assert_eq!(
            error.source().unwrap().to_string(),
            "short read: expected 8 bytes, got 3"
        );
    }

    #[test]
    fn i2c_display_does_not_repeat_the_source() {
        let error = Error::I2c {
            context: "writing to the panel".into(),
            source: std::io::Error::from_raw_os_error(5),
        };

        assert_eq!(error.to_string(), "writing to the panel");
        assert!(error.source().is_some());
    }

    #[test]
    fn error_can_cross_threads() {
        fn assert_send_sync<T: Send + Sync + 'static>() {}
        assert_send_sync::<Error>();
    }
}
