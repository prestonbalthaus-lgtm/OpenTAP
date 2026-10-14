//! `otap-sys`: the OpenTAP system boundary, and the only crate of the
//! workspace that may contain `unsafe` (round 3, Part 7 and section 8.3).
//!
//! Everything that reaches the kernel through raw memory, raw descriptors or
//! hand-encoded commands lives here behind safe owning types, so that every
//! engine crate can be `#![forbid(unsafe_code)]` and no engine crate names an
//! opcode or a command that the startup allowlist does not contain.
//!
//! - [`hugetlb`]: [`HugeArena`], the write cache's huge-page memory, a
//!   hugetlbfs file that a restarted daemon re-attaches after `kill -9`.
//! - [`ublk`]: the ublk UAPI of 6.12 as data, the control and I/O command
//!   builders and the user-copy offset; on Linux the control and character
//!   devices.
//! - [`uring`]: the io_uring opcode table and [`FixedBufferLayout`]; on Linux
//!   [`RegisteredBuffers`], buffer-table cloning and the user-copy read.
//! - [`probe`]: [`Allowlist`], the opcodes and ublk features the kernel
//!   offers, and the v0.1 requirement check.
//!
//! # Two platforms
//! The product runs on Linux 6.12; development and the simulator also run
//! elsewhere. The Linux implementation is compiled under
//! `cfg(target_os = "linux")` and a fallback under
//! `cfg(not(target_os = "linux"))`, behind one public API wherever an item
//! does not name a Linux-only type:
//!
//! - The UAPI constants, structures, command builders, offset arithmetic,
//!   the opcode table and the allowlist checks are pure and identical
//!   everywhere, and their tests run on every host.
//! - [`HugeArena::anonymous`] works everywhere: huge pages on Linux, an
//!   aligned heap buffer elsewhere. [`HugeArena::map_file`] and the
//!   constructor of [`RegisteredBuffers`] fail with
//!   [`std::io::ErrorKind::Unsupported`] off Linux.
//! - Items whose signatures name io_uring types or Linux descriptors exist on
//!   Linux only: `ublk::UblkControl`, `ublk::UblkChar`, the SQE builders,
//!   `uring::clone_buffers`, `uring::user_copy_read_sqe`, `Allowlist::probe`
//!   and `RegisteredBuffers::clone_to`.
//!
//! # Unsafe policy
//! - Every `unsafe` block is as small as the operation it wraps and opens with
//!   a `// SAFETY:` comment proving the callee's contract
//!   (`clippy::undocumented_unsafe_blocks` is denied workspace-wide).
//! - The public API is safe: no `unsafe fn` is exported, and none exists
//!   internally (`unsafe_op_in_unsafe_fn` is denied regardless).
//! - Safety rests on ownership: a mapping is unmapped only by its owner's
//!   `Drop`, and a registration borrows the memory it registers. Building an
//!   SQE is safe; pushing one is the `unsafe` step, and its proof must cover
//!   every address the entry names until its CQE is reaped.
//!
//! # Unsafe inventory
//! Five blocks, all Linux-only; the fallback contains none.
//!
//! 1. `hugetlb`, `Mapping::map_file`: `MmapOptions::map_mut`. The file is on
//!    hugetlbfs (no write path), sized by us and exclusively `flock`ed for
//!    the mapping's life, and the mapping is the process's only view of it.
//! 2. `uring`, `RegisteredBuffers::new`: `slice::from_raw_parts` reading the
//!    `IoSlice`s as `iovec`s. std guarantees the ABI match; size and
//!    alignment are checked at compile time.
//! 3. `uring`, `RegisteredBuffers::new`: `Submitter::register_buffers`. The
//!    value borrows the arena and the ring for its life and unregisters on
//!    drop; a table that outlives it (leaked value, refused unregistration)
//!    only keeps pinned pages the kernel writes for pushed operations alone.
//! 4. `probe`, `ublk_features`: `SubmissionQueue::push` of `GET_FEATURES`.
//!    The 8-byte reply buffer is freed only after the CQE, or leaked.
//! 5. `probe`, `ublk_features`: `Box::from_raw` of that buffer, once the
//!    kernel is done with it (CQE reaped, or SQE never submitted).
//!
//! # Example
//! ```
//! use otap_sys::ublk::{self, CtrlCommand, IoCommand};
//!
//! // The user-copy position of byte 0 of tag 3 on queue 1.
//! let pos = ublk::user_copy_offset(1, 3, 0);
//! assert_eq!(pos, 0x8000_0000 + (1 << 41) + (3 << 25));
//! assert_eq!(ublk::split_user_copy_offset(pos), Some((1, 3, 0)));
//!
//! // START_DEV for device 7, served by pid 1234: an 80-byte command area.
//! let start = CtrlCommand::start_dev(7, 1234);
//! assert_eq!(start.cmd_op, 0xC020_7506);
//! assert_eq!(start.cmd_bytes()[16..24], 1234u64.to_ne_bytes());
//!
//! // Every fetch carries addr = 0 under UBLK_F_USER_COPY.
//! assert_eq!(IoCommand::fetch(1, 3).cmd.addr, 0);
//! ```

#![deny(unsafe_op_in_unsafe_fn)]
#![deny(missing_docs)]
#![warn(rust_2018_idioms, unreachable_pub)]

#[cfg(not(target_pointer_width = "64"))]
compile_error!(
    "otap-sys supports 64-bit hosts only: arenas are several GiB and the UAPI layouts assume \
     8-byte alignment of u64"
);

pub mod hugetlb;
pub mod probe;
pub mod ublk;
pub mod uring;

pub use hugetlb::{HugeArena, HugePageSize};
pub use probe::{
    Allowlist, MissingCapability, OpcodeSet, REQUIRED_OPCODES, REQUIRED_UBLK_FEATURES,
};
pub use uring::{FixedBufferLayout, RegisteredBuffers};

pub use zerocopy::{FromBytes, FromZeros, Immutable, IntoBytes, KnownLayout};
