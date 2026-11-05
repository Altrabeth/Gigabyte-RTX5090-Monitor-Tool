//! Library behind the `aorus-lcd` binary, which drives the LCD side panel of a
//! Gigabyte AORUS RTX 5090 Master over I2C. It exists so that `tests/` can
//! reach every layer without going through the CLI.
//!
//! `protocol` and `rle` are pure: they return byte buffers and do no I/O. All
//! bus access goes through [`bus::transport::Transport`], and all waiting goes
//! through [`panel::Sleep`], so everything above them runs in tests without
//! hardware.

pub mod bus;
pub mod daemon;
pub mod error;
pub mod image;
pub mod panel;
pub mod protocol;
pub mod rle;
pub mod sensors;

pub use error::{Error, Result};
