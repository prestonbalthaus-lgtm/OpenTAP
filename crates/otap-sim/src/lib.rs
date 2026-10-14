//! `otap-sim`: the deterministic simulator (round 3, section 8.4; Step 3
//! brief, "Opus-Sim-Harness").
//!
//! Everything the engine needs from the machine is faked here, seeded, and
//! driven one tick at a time:
//!
//! - [`SimBackstore`]: a block device with pending, cached and durable
//!   layers, a seeded completion order, honest or lying flushes, and power
//!   cuts that tear writes at sector granularity;
//! - [`SimFrontDoor`]: a client that queues writes, reads and flushes and
//!   collects the engine's completions;
//! - [`SimClock`]: virtual time the harness advances;
//! - [`LoopbackNet`]: an in-memory network with delay and partitions;
//! - [`Model`]: the checker, which knows every acknowledged write and the
//!   rules a recovered image and the nvfail set must satisfy;
//! - [`Sim`]: the harness that boots, drives, crashes and reboots the
//!   engine and verifies every recovery.
//!
//! The suites in `tests/` are the proof the Step 3 brief asks for: a crash
//! injected while the superblock commit is pending rolls back to the
//! previous consistency point with no corruption; a daemon crash loses no
//! acknowledged write; a power cut holds exactly the right volumes; a clean
//! shutdown never trips nvfail; and a property test drives random I/O with
//! random crashes through all of it.

#![forbid(unsafe_code)]
#![deny(missing_docs)]
#![warn(rust_2018_idioms, unreachable_pub)]

pub mod backstore;
pub mod clock;
pub mod frontdoor;
pub mod harness;
pub mod model;
pub mod net;
pub mod rng;

pub use backstore::{BackstoreStats, CrashReport, OpKind, Schedule, SimBackstore, TearPolicy};
pub use clock::SimClock;
pub use frontdoor::{Completion, SimFrontDoor};
pub use harness::{BootInfo, Sim, SimConfig, SimEngine, Stop, Verdict, TICK_BUDGET};
pub use model::{block_data, decode, Decoded, Model, WriteRecord};
pub use net::{LoopbackNet, NetEnd};
pub use rng::SplitMix64;
