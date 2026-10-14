//! `otap-backstore`: the engine's real block devices (Step 4 brief,
//! "Opus-Storage-Geom", the `Backstore` driver).
//!
//! Two implementations of [`otap_cp::Backstore`]:
//!
//! - [`FileBackstore`]: an ordinary file (or a block device, sized by its end),
//!   on any host. Writes are synchronous and complete at the next `poll`;
//!   the flush and the FUA writes call `fdatasync` (on Windows,
//!   `FlushFileBuffers`). It is what the engine runs over on the Windows
//!   development PC and what a test uses to prove the whole stack, format
//!   to reboot, outside the simulator.
//! - [`UringBackstore`] (Linux): a block device on io_uring through
//!   [`otap_sys::BlockRing`]: `O_DIRECT` writes staged into page-aligned
//!   slots, `IORING_OP_FSYNC` with `FSYNC_DATASYNC` as the consistency
//!   point's barrier, the superblock and intent slots written whole with
//!   `RWF_DSYNC` and padded to a full stripe (R26), and `BLKDISCARD` for a
//!   freed segment when the profile asks for it (D9). Everything is
//!   type-checked for `x86_64-unknown-linux-gnu`; on other hosts the type
//!   exists and its constructor fails with `Unsupported`.
//!
//! Both keep the trait's contract: `Busy` means nothing was done, a
//! completion arrives through `poll` with the tag the engine gave, a
//! `commit` or `write_intent` is durable when its completion is reported,
//! and a `flush` completion covers every write completed before the flush
//! was submitted.

#![forbid(unsafe_code)]
#![deny(missing_docs)]
#![warn(rust_2018_idioms, unreachable_pub)]

pub mod file;
pub mod uring;

pub use file::FileBackstore;
pub use uring::UringBackstore;
