//! `otap-control`: the control protocol between `dbladed` and `otapctl`
//! (Step 5 brief, "Opus-Daemon-CLI"; E38 in `docs/07-engineering-decisions.md`).
//!
//! One request, one reply, over a byte stream: each message is a 4-byte
//! little-endian length followed by that many bytes of JSON ([`wire`]). The
//! daemon listens on an [`Endpoint`]: a Unix socket on Linux
//! (`unix:/run/opentap/dbladed.sock`) or loopback TCP anywhere
//! (`tcp:127.0.0.1:7421`), which is also what the Windows host and the
//! integration tests use. [`Client`] is the blocking caller `otapctl` is
//! built on; the daemon's server lives in `otap-daemon`.
//!
//! The protocol carries no data path: block I/O goes through ublk. The
//! `Io*` requests exist for the smoke test and the portable harness, where
//! no block device exists, and move whole 4 KiB blocks as hexadecimal
//! strings (at most [`MAX_IO_BLOCKS`] per request).
//!
//! Every reply is either a [`Response`] or a [`ControlError`] with a
//! [`ErrorCode`] the CLI can act on and a message an operator can read.

#![forbid(unsafe_code)]
#![deny(missing_docs)]
#![warn(rust_2018_idioms, unreachable_pub)]

pub mod client;
pub mod protocol;
pub mod units;
pub mod wire;

pub use client::{Client, Endpoint};
pub use protocol::{
    ArenaKind, ControlError, ErrorCode, IoResultReport, MediaKind, PoolReport, Reply, Request,
    Response, SnapshotReport, VolumeReport, MAX_IO_BLOCKS, PROTOCOL_VERSION,
};
pub use units::{
    check_arena_bytes, check_cp_interval, format_size, parse_size, ARENA_PAGE_BYTES,
    DEFAULT_ARENA_BYTES, DEFAULT_CP_INTERVAL_SECS, DEFAULT_HUGETLB_PATH, MAX_ARENA_BYTES,
    MAX_CP_INTERVAL_SECS, MIN_ARENA_BYTES, MIN_CP_INTERVAL_SECS,
};
pub use wire::{decode_hex, encode_hex, read_frame, write_frame, WireError, MAX_FRAME_BYTES};

/// The block, and the snapshot pin bits, as the format defines them: the
/// protocol speaks in these and a client must not redefine them.
pub use otap_format::{BLOCK_SIZE, PIN_CLONE, PIN_MASK, PIN_REPLICATION, PIN_SCHEDULE, PIN_USER};
