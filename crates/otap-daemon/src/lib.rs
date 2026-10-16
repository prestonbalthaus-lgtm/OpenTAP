//! `otap-daemon`: `dbladed`, the data daemon (Step 5 brief,
//! "Opus-Daemon-CLI"; E38 in `docs/07-engineering-decisions.md`).
//!
//! The daemon is one engine thread and a control server:
//!
//! - The engine thread owns the [`otap_cp::Engine`] over the pool's
//!   backstore ([`otap_backstore::UringBackstore`] for a block device on
//!   Linux, [`otap_backstore::FileBackstore`] for a file anywhere), ticks
//!   it (the CP timer, the cleaner and the deferred frees all run inside
//!   the tick), serves the ublk devices of the online volumes on Linux
//!   through the composite [`frontdoor::DaemonFrontDoor`], and executes
//!   every control request between ticks, so nothing else touches the
//!   engine. A request that has to wait for a consistency point (a
//!   volume creation, a snapshot, a deletion, a rollback, a sync) ticks
//!   the engine until the event it needs arrives, within a timeout; the
//!   data path keeps flowing meanwhile, because those ticks serve it.
//! - The control server ([`server`]) accepts connections on the
//!   [`otap_control::Endpoint`], reads one request per frame, hands it to
//!   the engine thread over a channel and writes the reply back.
//!
//! `SIGTERM` and `SIGINT` set a flag the engine thread polls: the pool is
//! shut down cleanly (every volume's device stopped, a final consistency
//! point with the `CleanShutdown` marker) and the process exits, so a
//! planned restart never trips nvfail (S5-4). A heap arena
//! (`--heap-arena`, S5-1) does not survive the process, which the clean
//! shutdown makes harmless; a hugetlbfs arena is re-attached after a
//! crash.
//!
//! Everything durable lives in the pool: the volumes and their names (the
//! catalogue, F16), the snapshots, the offline flags. The daemon keeps no
//! state of its own beyond the command line it was started with.

#![forbid(unsafe_code)]
#![deny(missing_docs)]
#![warn(rust_2018_idioms, unreachable_pub)]

pub mod arena;
pub mod backstore;
pub mod clock;
pub mod config;
pub mod daemon;
#[cfg(target_os = "linux")]
pub mod device;
pub mod frontdoor;
pub mod pool;
pub mod report;
pub mod server;

pub use arena::{check_resize, open_arena, open_pool_arena};
pub use backstore::{open_backstore, probe_superblock, Opened};
pub use clock::SystemClock;
pub use config::{DaemonConfig, Overrides};
pub use daemon::{install_signal_flag, Daemon, DaemonHandle};
pub use frontdoor::{ChannelFrontDoor, DaemonFrontDoor, IoCompletion, IoHandle};
pub use pool::{engine_error, status_word, BootSpec, Pool, PoolEngine};
pub use server::{serve, Server};

/// The crate version, which `Ping` reports.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");
