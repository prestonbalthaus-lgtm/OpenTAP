//! `otap-cp`: the consistency-point engine (round 3, sections 1.3, 6.2,
//! 7.2 and 7.6; Step 3 brief, "Opus-Storage-CP").
//!
//! The engine ties the arena ([`otap_arena`]) and the dirty index
//! ([`otap_index`]) to the machine through four traits, and nothing else:
//!
//! | Trait | Who implements it | What the engine does through it |
//! |---|---|---|
//! | [`Backstore`] | md, a single device, a hardware VD (`dbladed`); the simulator's fake | stripe, metadata, intent and superblock writes, flushes, reads |
//! | [`FrontDoor`] | ublk (`dbladed`); the simulator's request script | fetches requests, copies data in and out, acknowledges |
//! | [`Clock`] | the monotonic and wall clocks; the simulator's virtual time | CP timer, nonces, timestamps |
//! | [`Net`] | `TlsTcp` (later); the simulator's loopback | the replication transport (not used by the engine itself in Step 3) |
//!
//! # The write path
//!
//! A write reserves a run of arena slots, copies its data in, computes a
//! CRC-32C per block, takes a sequence number, inserts into the active
//! generation of the index and marks the run `Placed`; then it is
//! acknowledged. Nothing on that path waits for a disk, except once per
//! volume activation in precise nvfail mode, when an intent slot is written
//! before the acknowledgement (section 7.6).
//!
//! # The consistency point
//!
//! A cut freezes the active generation (a trigger: timer, arena half full,
//! back to back, a request, shutdown, boot). The flusher takes the oldest
//! frozen generation and, in volume and LBA order, assembles full stripes in
//! memory (summary page first, then the data blocks; F3) and writes each one
//! whole; then it writes the map image and the segment table into the open
//! metadata segment, flushes the pool, and commits the superblock slot to the
//! `A` location and then the `B` location, each a FUA write (F7). Only then
//! does the map change, the generation retire and the arena slots become
//! reusable. The on-disk order (data, metadata, flush, superblock) is what
//! makes a torn write damage only the uncommitted CP (invariant 5).
//!
//! # Boot and nvfail
//!
//! [`boot::evaluate`] ranks the superblock ring, loads the newest candidate's
//! map and segment table and verifies every stripe that CP wrote by its
//! summary; a candidate that fails is torn and the next one is taken. A boot
//! is dirty when the selected CP's `run_state` is not `CleanShutdown` or
//! when it rewound (the engine ran past the selected CP either way). If the
//! arena re-attaches whole (a daemon crash: every descriptor and block
//! verifies) and nothing was rewound, its placed writes are replayed into
//! the active generation and nothing is lost; otherwise (a power loss, a
//! rewind, or a damaged arena) every volume in the boot's write-active set
//! (the superblock's bitset united with newer intent slots) is held by
//! nvfail, cause `TornCp` after a rewind and `DirtyBoot` else, until the
//! operator clears it, and the engine answers EIO for it. Either way the
//! boot writes a superblock before it serves (R27, section 7.6). A clear,
//! like a volume created at runtime, takes effect only when the CP that
//! records it commits.
//!
//! # What this step leaves out
//!
//! Relocation and the cleaner for data segments (metadata segments are
//! recycled; a pool that fills with data refuses writes with `NoSpace` and
//! keeps serving reads), the radix map and incremental checkpoints (the
//! whole map is written at every CP and must fit one segment; E14),
//! snapshots, discard, the metadata device class, the back-pressure delay
//! curve (a full arena defers writes, first in first out, while reads are
//! served past them), flush-opaque backstores (refused until the owner
//! decides; E19), and every concurrency beyond one engine thread (E16).

#![forbid(unsafe_code)]
#![deny(missing_docs)]
#![warn(rust_2018_idioms, unreachable_pub)]

pub mod blockmap;
pub mod boot;
pub mod engine;
pub mod layout;
pub mod segments;
pub mod traits;

pub use blockmap::BlockMap;
pub use boot::{BootError, BootState, TornReason};
pub use engine::{
    BootFailure, CpTrigger, Engine, EngineConfig, EngineError, Event, PeekSource, Phase,
    VolumeSpec, VolumeStatus, MAX_REQUEST_BLOCKS, META_RESERVE_SEGMENTS,
};
pub use layout::{DiskLayout, LayoutError};
pub use segments::{OpenSegment, SegmentError, SegmentTable};
pub use traits::{
    Backstore, Busy, Clock, DevClass, Done, EndpointId, FrontDoor, Geometry, IoError, Media,
    MediaProfile, Net, NetError, NvfailIntent, ReqId, Request, RequestKind, Status, Tag,
};
