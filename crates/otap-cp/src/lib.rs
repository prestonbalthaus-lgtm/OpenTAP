//! `otap-cp`: the consistency-point engine (round 3, sections 1.3, 6.2,
//! 7.2 and 7.6; Step 3 brief, "Opus-Storage-CP"; Step 4 brief,
//! "Opus-Storage-Map" for the persistent map, folds, snapshots and holes).
//!
//! The engine ties the arena ([`otap_arena`]) and the dirty index
//! ([`otap_index`]) to the machine through four traits, and nothing else:
//!
//! | Trait | Who implements it | What the engine does through it |
//! |---|---|---|
//! | [`Backstore`] | `otap-backstore` (`UringBackstore` on Linux, `FileBackstore` anywhere); the simulator's fake | stripe, metadata, intent and superblock writes, flushes, discards, reads |
//! | [`FrontDoor`] | `otap-ublk`; the simulator's request script | fetches requests (reads, writes, flushes, discards, writes of zeros), copies data in and out, acknowledges |
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
//! whole; then it writes the CP's metadata through the [`MetaStream`]: the
//! map's delta regions (one [`otap_format::CpLogRec`] per block written or
//! hole punched; E22), the pages and indexes of a fold, the snapshot root
//! pages, the changed records of the snapshot table and the catalogue as
//! delta regions with their heads (or a fold of a table; format 2.4,
//! [`tables`]), the roots table, the directory and the segment table,
//! packed into whole stripes of the open metadata segment and into runs of
//! free segments when a region does not fit; then it flushes the pool and
//! commits the superblock slot to the `A` location and then the `B`
//! location, each a FUA write (F7). Only then does the map change, the
//! generation retire and the arena slots become reusable. The on-disk order
//! (data, metadata, flush, superblock) is what makes a torn write damage
//! only the uncommitted CP (invariant 5). A region a CP stops referencing
//! is released after the next commit (one more under a flush-opaque
//! profile, E21), and a freed segment stays allocated until its discard
//! completes (E24).
//!
//! A *fold* (`FoldPolicy`, a request, a snapshot, a clean shutdown) rewrites
//! the map pages the log touched and empties the log, so a boot replays
//! only the deltas born after the fold. Snapshots are taken at folds and
//! pinned by a root page (E25); discards and writes of zeros are holes
//! (E23), read as zeros and logged, never written to a stripe.
//!
//! # Boot and nvfail
//!
//! [`boot::evaluate`] ranks the superblock ring, loads the newest candidate's
//! roots, indexes, directory, delta regions and snapshot roots, verifies
//! the fold's pages when the candidate is the fold, replays the deltas,
//! and verifies every stripe that CP wrote by its summary; a candidate that
//! fails is torn and the next one is taken. A boot is dirty when the
//! selected CP's `run_state` is not `CleanShutdown` or when it rewound (the
//! engine ran past the selected CP either way). If the arena re-attaches
//! whole (a daemon crash: every descriptor and block verifies, hole records
//! included) and nothing was rewound, its placed writes and punches are
//! replayed into the active generation and nothing is lost; otherwise (a
//! power loss, a rewind, or a damaged arena) every volume in the boot's
//! write-active set (the superblock's bitset united with newer intent
//! slots) is held by nvfail, cause `TornCp` after a rewind and `DirtyBoot`
//! else, until the operator clears it, and the engine answers EIO for it.
//! Either way the boot writes a superblock before it serves (R27, section
//! 7.6), and retries that commit after a failure that was not a lack of
//! space. A clear, like a volume created at runtime, takes effect only when
//! the CP that records it commits. A flush-opaque backstore boots under the
//! trust policy with a structured warning in [`PoolStatus`], or is refused
//! under the strict one (E21).
//!
//! # Verification
//!
//! [`Engine::start_verify`] attests the pool as the committed CP left it
//! ([`verify`]): the superblock ring, every table, tree and live block, and
//! the live bytes of every segment, in slices between ticks while the
//! deferred frees are held so the view stays on disk; the report lists
//! what it found and the pool status carries it.
//!
//! # What this step leaves out
//!
//! Relocation and the cleaner for data segments (metadata segments are
//! recycled; a segment with one live data block stays allocated; a pool
//! that fills with data refuses writes with `NoSpace` and keeps serving
//! reads), snapshot deletion and deadlists (a snapshot pins what it
//! references until Step 5), the metadata device class, the back-pressure
//! delay curve (a full arena defers writes, first in first out, while
//! reads are served past them), `cut_within`, and every concurrency beyond
//! one engine thread (E16). The Linux `Backstore` and the ublk device
//! module exist in `otap-backstore` and `otap-ublk` and are type-checked
//! only; the engine behind them has run on the simulator's device and on a
//! plain file.

#![forbid(unsafe_code)]
#![deny(missing_docs)]
#![warn(rust_2018_idioms, unreachable_pub)]

pub mod boot;
pub mod cleaner;
pub mod engine;
pub mod layout;
pub mod meta;
pub mod segments;
pub mod tables;
pub mod traits;
pub mod verify;

pub use boot::{BootError, BootSnapshot, BootState, TornReason};
pub use cleaner::{CleanTarget, CleanerPolicy, RelocBlock, Scan, SkipReason};
pub use engine::{
    check_arena_resize, reserve_for, BootFailure, CpTrigger, Engine, EngineConfig, EngineError,
    Event, FoldPolicy, LivenessDrift, OperatorWarning, Pacing, PeekSource, Phase, PoolStatus,
    SnapshotInfo, Stats, VolumeSpec, VolumeStatus, MAX_PUNCH_BLOCKS, MAX_REQUEST_BLOCKS,
    MAX_RESERVED_SNAPSHOTS, MAX_USER_SNAPSHOTS, META_RESERVE_SEGMENTS,
};
pub use layout::{DiskLayout, LayoutError};
pub use meta::{MetaError, MetaStream};
pub use segments::{OpenSegment, SegmentError, SegmentTable};
pub use tables::{
    load_table, plan_table, TableChain, TableFoldPolicy, TableLoad, TablePlan, TablePlanInput,
};
pub use traits::{
    Backstore, Busy, Clock, DevClass, DiscardPolicy, Done, EndpointId, FlushPolicy, FrontDoor,
    Geometry, IoError, Media, MediaProfile, Net, NetError, NvfailIntent, ReqId, Request,
    RequestKind, Status, Tag,
};
pub use verify::{
    Finding, FindingKind, Severity, Verdict, VerifyCounts, VerifyOptions, VerifyPhase,
    VerifyReport, VerifyState, VerifySummary, MAX_FINDINGS,
};
