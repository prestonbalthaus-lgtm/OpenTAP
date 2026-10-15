//! The real clocks.

use std::time::{Instant, SystemTime, UNIX_EPOCH};

use otap_cp::Clock;

/// The host's monotonic clock (since the daemon started) and wall clock.
#[derive(Debug, Clone)]
pub struct SystemClock {
    started: Instant,
}

impl SystemClock {
    /// A clock whose monotonic origin is now.
    #[must_use]
    pub fn new() -> Self {
        Self {
            started: Instant::now(),
        }
    }
}

impl Default for SystemClock {
    fn default() -> Self {
        Self::new()
    }
}

impl Clock for SystemClock {
    fn mono_ns(&self) -> u64 {
        u64::try_from(self.started.elapsed().as_nanos()).unwrap_or(u64::MAX)
    }

    fn wall_ns(&self) -> u64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| u64::try_from(d.as_nanos()).unwrap_or(u64::MAX))
            .unwrap_or(0)
    }
}
