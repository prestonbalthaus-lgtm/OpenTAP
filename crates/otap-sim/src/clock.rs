//! Virtual time: advanced by the harness, read by the engine.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use otap_cp::Clock;

/// A clock whose time moves only when the harness says so. Clones share the
/// same time.
#[derive(Debug, Clone)]
pub struct SimClock {
    mono: Arc<AtomicU64>,
    wall_base_ns: u64,
}

impl SimClock {
    /// A clock at monotonic 0 whose wall clock starts at `wall_base_ns`.
    #[must_use]
    pub fn new(wall_base_ns: u64) -> Self {
        Self {
            mono: Arc::new(AtomicU64::new(0)),
            wall_base_ns,
        }
    }

    /// Moves time forward.
    pub fn advance(&self, ns: u64) {
        self.mono.fetch_add(ns, Ordering::Relaxed);
    }
}

impl Clock for SimClock {
    fn mono_ns(&self) -> u64 {
        self.mono.load(Ordering::Relaxed)
    }

    fn wall_ns(&self) -> u64 {
        self.wall_base_ns + self.mono_ns()
    }
}
