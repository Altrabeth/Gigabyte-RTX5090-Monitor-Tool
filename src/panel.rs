//! High-level panel operations, built on a
//! [`Transport`](crate::bus::transport::Transport).

use std::time::Duration;

/// Waits for a while. All delays in this crate go through this trait, because
/// the firmware's pacing has to be asserted in tests without real waiting.
pub trait Sleep {
    /// Blocks for `duration`.
    fn sleep(&mut self, duration: Duration);
}

/// The real clock, for production use.
#[derive(Debug, Default, Clone, Copy)]
pub struct ThreadSleep;

impl Sleep for ThreadSleep {
    fn sleep(&mut self, duration: Duration) {
        std::thread::sleep(duration);
    }
}

/// Test double: returns at once and records every requested delay in order.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct RecordingSleep {
    /// The delays requested so far.
    pub delays: Vec<Duration>,
}

impl Sleep for RecordingSleep {
    fn sleep(&mut self, duration: Duration) {
        self.delays.push(duration);
    }
}
