//! The engine: write and read paths, the consistency-point state machine,
//! boot and shutdown.

use core::fmt;
use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::Arc;

use otap_arena::{Arena, ArenaError, PlacedSlot, RingRef};
use otap_format::checksum::crc32c;
use otap_format::{
    boot_write_active, map_roots_image, snap_table_image, FormatError, IntentSlot, IntoBytes,
    MapEntry, NvCause, RootRef, RunState, SbLocation, SbSlot, SegKind, SegState, SnapKind,
    SnapOrigin, SnapRecord, SnapRootPage, StripeDescriptor, StripeHdr, StripeKind, Superblock,
    SuperblockSlot, Uuid, VolBitSet, VolState, BLOCK_SIZE, MAX_CP_SEQ, MAX_LBA, MAX_VOLS,
    SECTORS_PER_BLOCK, VOL_PRESENT,
};
use otap_index::{DirtyExtent, GenId, Generation, Index, IndexError};
use otap_map::{CpInput, MapError, MapSet, MapUpdate, PageReader, PinnedRoot, Replaced};
use otap_sys::HugeArena;

use crate::boot::{self, BootError, BootSnapshot};
use crate::layout::{DiskLayout, LayoutError};
use crate::meta::{MetaError, MetaStream};
use crate::segments::{OpenSegment, SegmentError, SegmentTable};
use crate::traits::{
    Backstore, Busy, Clock, DevClass, DiscardPolicy, Done, FlushPolicy, FrontDoor, IoError,
    NvfailIntent, Request, RequestKind, Status, Tag,
};

/// The longest read or write the engine serves, in blocks (8 MiB): bounds
/// the read buffer and the arena run one request can take.
pub const MAX_REQUEST_BLOCKS: u32 = 2048;

/// The longest discard or write of zeros the engine serves, in blocks
/// (1 GiB): one hole record in the arena, one CP log record per block.
pub const MAX_PUNCH_BLOCKS: u32 = 1 << 18;

/// Segments data allocations leave free so a boot or a shutdown commit
/// always finds room for its metadata.
pub const META_RESERVE_SEGMENTS: u32 = 2;

/// A volume the control plane knows: its index and size.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct VolumeSpec {
    /// The volume index, below [`MAX_VOLS`].
    pub vol: u32,
    /// The size in 4 KiB blocks, `1..=MAX_LBA + 1`.
    pub size_blocks: u64,
}

/// When the map folds its overlay into the pages (E22). A fold happens at
/// the first CP after any bound is reached, at a snapshot, at a clean
/// shutdown, and on request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FoldPolicy {
    /// Bytes of CP log since the last fold.
    pub log_bytes: u64,
    /// Time since the last fold, when anything is in the overlay.
    pub interval_ns: u64,
    /// Consistency points with changes since the last fold.
    pub max_cps: u64,
    /// Map pages the committed overlay touches (the CP after the one that
    /// crosses the bound folds).
    pub max_dirty_pages: usize,
    /// Entries in the committed overlay (likewise).
    pub max_overlay_entries: usize,
}

impl Default for FoldPolicy {
    /// 1 GiB of log or ten minutes (section 1.3), 1,024 CPs, 65,536 dirty
    /// pages (1 GiB of map pages), 8 million overlay entries.
    fn default() -> Self {
        Self {
            log_bytes: 1 << 30,
            interval_ns: 600_000_000_000,
            max_cps: 1024,
            max_dirty_pages: 65_536,
            max_overlay_entries: 8 << 20,
        }
    }
}

/// What the engine is told at boot or format.
#[derive(Debug, Clone, PartialEq)]
pub struct EngineConfig {
    /// The aggregate.
    pub agg_id: Uuid,
    /// The owner epoch written into every commit (1 on a single node).
    pub owner_epoch: u64,
    /// Generations in the index ring (8 by default).
    pub ring: usize,
    /// Arena occupancy that triggers a CP, `0.0..=1.0` (0.5 by default).
    pub arena_half: f64,
    /// The volumes and their sizes. A volume listed here and absent from the
    /// disk is created at boot (durable with the boot commit).
    pub volumes: Vec<VolumeSpec>,
    /// When the map folds.
    pub fold: FoldPolicy,
    /// Map pages the engine caches.
    pub cache_pages: usize,
}

impl EngineConfig {
    /// A configuration with the defaults of section 1.3.
    #[must_use]
    pub fn new(agg_id: Uuid, volumes: Vec<VolumeSpec>) -> Self {
        Self {
            agg_id,
            owner_epoch: 1,
            ring: otap_index::DEFAULT_RING,
            arena_half: 0.5,
            volumes,
            fold: FoldPolicy::default(),
            cache_pages: boot::CACHE_PAGES,
        }
    }

    /// Checks every volume index and size, and that no index repeats.
    pub fn validate(&self) -> Result<(), EngineError> {
        let mut seen = HashSet::new();
        for spec in &self.volumes {
            if spec.vol as usize >= MAX_VOLS {
                return Err(EngineError::NoSuchVolume { vol: spec.vol });
            }
            check_size(spec.vol, spec.size_blocks)?;
            if !seen.insert(spec.vol) {
                return Err(EngineError::VolumeExists { vol: spec.vol });
            }
        }
        Ok(())
    }

    /// Blocks over every volume.
    #[must_use]
    pub fn total_blocks(&self) -> u64 {
        self.volumes
            .iter()
            .fold(0u64, |sum, v| sum.saturating_add(v.size_blocks))
    }
}

fn check_size(vol: u32, size_blocks: u64) -> Result<(), EngineError> {
    if size_blocks == 0 || size_blocks > MAX_LBA + 1 {
        return Err(EngineError::BadSize { vol, size_blocks });
    }
    Ok(())
}

/// The page index a volume needs must fit the pool's metadata budget: a
/// quarter of the segments, so data keeps the rest.
fn check_index_fits(layout: &DiskLayout, size_blocks: u64) -> Result<(), EngineError> {
    let bytes = otap_format::MapRoot::index_pages_for(size_blocks)
        .saturating_mul(otap_format::INDEX_PAGE_SIZE as u64);
    // A quarter of the pool, and never more than the largest region a boot
    // reads (4 GiB; `boot::read_root`).
    let limit = ((u64::from(layout.segments) * layout.segment_bytes) / 4).min(1 << 32);
    if bytes > limit {
        return Err(EngineError::MetadataTooLarge { bytes, limit });
    }
    Ok(())
}

/// What caused a consistency point.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum CpTrigger {
    /// The CP timer.
    Timer,
    /// The arena reached the configured occupancy.
    ArenaHalf,
    /// The previous CP committed with the unflushed bytes above the
    /// profile's back-to-back threshold.
    BackToBack,
    /// [`Engine::request_cp`].
    Request,
    /// The final CP of a clean shutdown.
    Shutdown,
    /// The commit every boot writes before it serves.
    Boot,
}

/// A structured warning an operator must see in the pool status (E21).
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum OperatorWarning {
    /// The backstore is flush-opaque and the policy trusts the host-side
    /// barrier: a power loss that also loses the controller's cache can
    /// lose committed data, which nvfail then quarantines.
    FlushOpaqueTrusted {
        /// The policy in force.
        policy: FlushPolicy,
    },
    /// A map page of `vol` does not verify: its entries read `Corrupt`,
    /// and the map is not folded until the volume is repaired.
    MapPageDamaged {
        /// The volume.
        vol: u32,
        /// The map page (1,024 blocks each).
        page_no: u64,
    },
    /// A snapshot's root page or pinned index did not verify at boot: it
    /// is listed, cannot be read or diffed, and still pins its data.
    SnapshotDamaged {
        /// The snapshot.
        snap_id: u64,
        /// Its volume.
        vol: u32,
        /// What failed.
        reason: String,
    },
}

impl fmt::Display for OperatorWarning {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::FlushOpaqueTrusted { policy } => write!(
                f,
                "the backstore cannot prove its flushes; running under {policy:?}: a power loss \
                 that loses the controller cache can lose committed consistency points, and \
                 nvfail quarantines the volumes it finds dirty"
            ),
            Self::MapPageDamaged { vol, page_no } => write!(
                f,
                "map page {page_no} of volume {vol} does not verify: its blocks read as corrupt \
                 and the map is not folded until the volume is repaired"
            ),
            Self::SnapshotDamaged {
                snap_id,
                vol,
                reason,
            } => write!(
                f,
                "snapshot {snap_id} of volume {vol} is damaged ({reason}): it cannot be read or \
                 diffed"
            ),
        }
    }
}

/// Something the engine did that an operator or a checker wants to know.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum Event {
    /// The boot evaluated the ring.
    Booted {
        /// The CP selected.
        cp_seq: u64,
        /// The boot is a dirty one: `run_state` was not `CleanShutdown`,
        /// or a newer candidate was torn.
        dirty: bool,
        /// The arena re-attached and its placed writes were replayed.
        reattached: bool,
        /// Blocks replayed from the arena.
        replayed_blocks: u64,
        /// The newest candidate was torn and an older CP was selected.
        rewound: bool,
        /// The arena memory held something that was not a usable arena:
        /// a damaged header. It was formatted anew.
        arena_lost: bool,
        /// Arena slots the re-attach could not trust and dropped.
        damaged_slots: u32,
        /// Replayed blocks that named no present volume and were dropped.
        orphaned_blocks: u64,
        /// Volumes held by nvfail after this boot.
        nvfail: VolBitSet,
        /// The candidates skipped as torn, newest first: `(cp_seq, why)`.
        torn: Vec<(u64, String)>,
    },
    /// A consistency point is durable.
    CpCommitted {
        /// Its sequence.
        cp_seq: u64,
        /// What caused it.
        trigger: CpTrigger,
        /// The generation it carried, if any.
        gen: Option<GenId>,
        /// Blocks it wrote.
        blocks: u64,
        /// The highest write sequence number it carried (0 for none).
        max_seq: u64,
    },
    /// A consistency point attempt failed; it is retried (or, with no free
    /// segment, waits for [`Engine::request_cp`]).
    CpFailed {
        /// The sequence it was going to commit.
        cp_seq: u64,
        /// Why.
        reason: String,
    },
    /// A consistency point folded the map: the pages hold everything.
    Folded {
        /// The CP.
        cp_seq: u64,
        /// Map pages rewritten.
        pages: u64,
    },
    /// An intent slot is durable.
    IntentWritten {
        /// Its sequence.
        intent_seq: u64,
        /// The set it published.
        write_active: VolBitSet,
    },
    /// nvfail was cleared on a volume: the clear is durable and the volume
    /// serves again.
    NvfailCleared {
        /// The volume.
        vol: u32,
        /// Its rollback generation afterwards.
        rollback_gen: u32,
    },
    /// A volume's creation is durable: it serves from now on.
    VolumeCreated {
        /// The volume.
        vol: u32,
        /// The CP that carried it.
        cp_seq: u64,
    },
    /// A snapshot is durable.
    SnapshotCreated {
        /// Its id.
        snap_id: u64,
        /// The volume.
        vol: u32,
        /// The CP it pins.
        cp_seq: u64,
    },
    /// A snapshot that was accepted could not be taken: the fold it needed
    /// found no free segment, or a map page it had to rewrite is damaged.
    /// The request is dropped; the control plane may ask again later.
    SnapshotFailed {
        /// The snapshot's identity.
        uuid: Uuid,
        /// The volume.
        vol: u32,
        /// Why.
        reason: String,
    },
    /// A metadata segment no reachable CP names was freed (and discarded
    /// when the policy says so).
    SegmentFreed {
        /// The segment.
        pseg: u64,
        /// A discard was issued.
        discarded: bool,
    },
    /// A warning the pool status carries.
    OperatorWarning(OperatorWarning),
    /// A block in the arena failed the CRC recorded when it was written: the
    /// block goes to disk under the recorded CRC and reads as `Corrupt`.
    ArenaDamage {
        /// The volume.
        vol: u32,
        /// The LBA.
        lba: u64,
        /// The arena slot.
        slot: u32,
    },
    /// An invariant did not hold where the engine could not stop; the
    /// engine went on and this is the record.
    Internal {
        /// What happened.
        what: String,
    },
    /// The engine serves nothing any more: the clean shutdown's final CP
    /// committed, or the final CP could not be written (`clean == false`,
    /// a full pool) and the arena keeps the unflushed writes for the next
    /// boot.
    Stopped {
        /// The committed CP at the stop.
        cp_seq: u64,
        /// `true` when the final CP recorded `CleanShutdown`.
        clean: bool,
    },
}

/// Why the engine refused or failed.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum EngineError {
    /// The format refused a structure.
    Format(FormatError),
    /// The arena refused.
    Arena(ArenaError),
    /// The index refused.
    Index(IndexError),
    /// The map refused.
    Map(MapError),
    /// The metadata stream refused.
    Meta(MetaError),
    /// The geometry cannot be laid out.
    Layout(LayoutError),
    /// The segment table refused.
    Segment(SegmentError),
    /// The boot failed.
    Boot(BootError),
    /// An I/O error.
    Io(IoError),
    /// No free segment.
    NoSpace,
    /// The volume is not present.
    NoSuchVolume {
        /// The volume.
        vol: u32,
    },
    /// The volume is already present.
    VolumeExists {
        /// The volume.
        vol: u32,
    },
    /// A volume size of zero or beyond the LBA space.
    BadSize {
        /// The volume.
        vol: u32,
        /// The size offered.
        size_blocks: u64,
    },
    /// A volume whose page index would take more than the pool's metadata
    /// budget (a quarter of the segments): legal in size, impossible here.
    MetadataTooLarge {
        /// The index bytes the volume needs.
        bytes: u64,
        /// The budget.
        limit: u64,
    },
    /// The volume is held by nvfail.
    NvFailed {
        /// The volume.
        vol: u32,
    },
    /// Data read from disk failed its CRC.
    Corrupt {
        /// The volume.
        vol: u32,
        /// The LBA.
        lba: u64,
    },
    /// No snapshot with this id.
    NoSuchSnapshot {
        /// The id.
        snap_id: u64,
    },
    /// A snapshot with this uuid exists or is pending.
    SnapshotExists {
        /// The uuid.
        uuid: Uuid,
    },
    /// The snapshot's root page or pinned index did not verify at boot: it
    /// is listed, and reads and diffs of it are refused.
    SnapshotDamaged {
        /// The id.
        snap_id: u64,
    },
    /// A diff's base is not older than the snapshot it is taken against.
    SnapshotOrder {
        /// The snapshot.
        snap_id: u64,
        /// The base offered.
        base: u64,
    },
    /// The configuration gives a volume a size other than its on-disk root:
    /// a resize goes through a consistency point, not a boot.
    VolumeSizeMismatch {
        /// The volume.
        vol: u32,
        /// The root's size in blocks.
        on_disk: u64,
        /// The configuration's size in blocks.
        configured: u64,
    },
    /// A map page is damaged and the map cannot be folded until the volume
    /// is repaired; a snapshot or a requested fold is refused.
    MapDamaged,
    /// The geometry is flush-opaque and the policy is strict (E19 / E21).
    FlushOpaqueRefused,
    /// The engine is not serving (booting or stopped).
    NotServing,
    /// Something this step does not support.
    Unsupported(&'static str),
    /// An invariant the engine relies on did not hold.
    Internal(String),
}

impl fmt::Display for EngineError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Format(e) => write!(f, "format: {e}"),
            Self::Arena(e) => write!(f, "arena: {e}"),
            Self::Index(e) => write!(f, "index: {e}"),
            Self::Map(e) => write!(f, "map: {e}"),
            Self::Meta(e) => write!(f, "metadata: {e}"),
            Self::Layout(e) => write!(f, "layout: {e}"),
            Self::Segment(e) => write!(f, "segments: {e}"),
            Self::Boot(e) => write!(f, "boot: {e}"),
            Self::Io(e) => write!(f, "io: {e}"),
            Self::NoSpace => f.write_str("no free segment"),
            Self::NoSuchVolume { vol } => write!(f, "volume {vol} is not present"),
            Self::VolumeExists { vol } => write!(f, "volume {vol} is already present"),
            Self::BadSize { vol, size_blocks } => {
                write!(
                    f,
                    "volume {vol}: a size of {size_blocks} blocks is out of bounds"
                )
            }
            Self::MetadataTooLarge { bytes, limit } => write!(
                f,
                "a page index of {bytes} bytes exceeds the pool's metadata budget of {limit}"
            ),
            Self::NvFailed { vol } => write!(f, "volume {vol} is held by nvfail"),
            Self::Corrupt { vol, lba } => write!(f, "volume {vol} LBA {lba} failed its CRC"),
            Self::NoSuchSnapshot { snap_id } => write!(f, "no snapshot {snap_id}"),
            Self::SnapshotExists { uuid } => write!(f, "snapshot {uuid} exists"),
            Self::SnapshotDamaged { snap_id } => {
                write!(f, "snapshot {snap_id} is damaged and cannot be read")
            }
            Self::SnapshotOrder { snap_id, base } => write!(
                f,
                "snapshot {base} is not older than snapshot {snap_id}, so it cannot be its base"
            ),
            Self::VolumeSizeMismatch {
                vol,
                on_disk,
                configured,
            } => write!(
                f,
                "volume {vol} is {on_disk} blocks on disk and {configured} in the configuration"
            ),
            Self::MapDamaged => {
                f.write_str("a map page is damaged; the map cannot be folded or snapshotted")
            }
            Self::FlushOpaqueRefused => f.write_str(
                "the backstore is flush-opaque and the policy is RequireFuaOrFlush (E19)",
            ),
            Self::NotServing => f.write_str("the engine is not serving"),
            Self::Unsupported(what) => write!(f, "unsupported: {what}"),
            Self::Internal(s) => write!(f, "internal: {s}"),
        }
    }
}

impl std::error::Error for EngineError {}

macro_rules! from_error {
    ($variant:ident, $ty:ty) => {
        impl From<$ty> for EngineError {
            fn from(e: $ty) -> Self {
                Self::$variant(e)
            }
        }
    };
}
from_error!(Format, FormatError);
from_error!(Arena, ArenaError);
from_error!(Index, IndexError);
from_error!(Map, MapError);
from_error!(Meta, MetaError);
from_error!(Layout, LayoutError);
from_error!(Segment, SegmentError);
from_error!(Boot, BootError);
from_error!(Io, IoError);

/// A boot that did not produce an engine: the error and everything the
/// caller lent, so a harness (or `dbladed`) can try again or hand the arena
/// memory to a support bundle instead of losing it.
pub struct BootFailure<B, F, C> {
    /// Why.
    pub error: EngineError,
    /// The backstore, untouched beyond reads.
    pub backstore: B,
    /// The front door.
    pub front: F,
    /// The clock.
    pub clock: C,
    /// The arena memory, exactly as it was.
    pub memory: HugeArena,
}

impl<B, F, C> BootFailure<B, F, C> {
    /// The parts, in the order [`Engine::boot`] took them.
    #[must_use]
    pub fn into_parts(self) -> (EngineError, B, F, C, HugeArena) {
        (
            self.error,
            self.backstore,
            self.front,
            self.clock,
            self.memory,
        )
    }
}

impl<B, F, C> fmt::Debug for BootFailure<B, F, C> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("BootFailure")
            .field("error", &self.error)
            .finish_non_exhaustive()
    }
}

impl<B, F, C> fmt::Display for BootFailure<B, F, C> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(&self.error, f)
    }
}

/// Where a peeked block came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum PeekSource {
    /// The arena (unflushed).
    Arena,
    /// The disk, CRC checked.
    Disk,
    /// Never written: zeros.
    Unmapped,
    /// A punched hole: zeros.
    Hole,
}

/// A volume as the engine sees it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct VolumeStatus {
    /// Present in the volume table (durably, or pending the next CP).
    pub present: bool,
    /// Present and serving: the creation is durable and nvfail is clear.
    pub serving: bool,
    /// Size in blocks (0 when the control plane did not say).
    pub size_blocks: u64,
    /// The rollback generation.
    pub rollback_gen: u32,
    /// The nvfail cause, if held (a clear in progress still holds).
    pub nvfail: Option<NvCause>,
    /// A clear was asked for and waits for its CP; the generation it will
    /// bring.
    pub clear_pending: Option<u32>,
    /// Write-active windows left (precise mode bookkeeping).
    pub active_windows: u8,
}

/// A snapshot as the engine lists it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct SnapshotInfo {
    /// The id: monotonic per aggregate between rewinds (a boot that rewinds
    /// below the CP that took a snapshot loses it, and the id is reused).
    pub snap_id: u64,
    /// The identity.
    pub uuid: Uuid,
    /// The volume.
    pub vol: u32,
    /// The CP it pins.
    pub cp_seq: u64,
    /// How it was taken.
    pub kind: SnapKind,
    /// Why it exists.
    pub origin: SnapOrigin,
    /// When it was taken.
    pub created_wall_ns: u64,
    /// The boot could not read its root page or pinned index: reads and
    /// diffs of it are refused ([`OperatorWarning::SnapshotDamaged`]).
    pub damaged: bool,
}

/// The pool as an operator sees it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PoolStatus {
    /// The committed CP.
    pub cp_seq: u64,
    /// The CP of the last fold.
    pub fold_cp: u64,
    /// Consistency points with changes since the fold.
    pub cps_since_fold: u64,
    /// Bytes of CP log since the fold.
    pub log_bytes_since_fold: u64,
    /// Map pages a fold would rewrite now.
    pub dirty_pages: usize,
    /// Overlay entries.
    pub overlay_entries: usize,
    /// Free segments.
    pub free_segments: u32,
    /// Segments in the pool.
    pub segments: u32,
    /// The flush policy in force.
    pub flush_policy: FlushPolicy,
    /// The geometry is flush-opaque.
    pub flush_opaque: bool,
    /// The discard policy in force.
    pub discard: DiscardPolicy,
    /// The warnings in force.
    pub warnings: Vec<OperatorWarning>,
    /// Snapshots on disk.
    pub snapshots: usize,
    /// A CP failed for want of a segment and none was requested since.
    pub pool_full: bool,
    /// The engine serves requests.
    pub serving: bool,
}

/// Which phase the CP state machine is in.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Phase {
    /// No CP in progress.
    Idle,
    /// Writing data stripes.
    Staging,
    /// Writing the map regions and the tables.
    Metadata,
    /// Waiting for the pool flush.
    Flushing,
    /// Writing the `A` superblock slot.
    CommitA,
    /// Writing the `B` superblock slot.
    CommitB,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct StagedBlock {
    vol: u32,
    lba: u64,
    ring: RingRef,
    seq: u64,
}

#[derive(Debug)]
struct CpJob {
    cp_seq: u64,
    nonce: u64,
    trigger: CpTrigger,
    gen: Option<Arc<Generation>>,
    blocks: Vec<StagedBlock>,
    /// Holes the generation holds: `(vol, lba, blocks)`.
    holes: Vec<(u32, u64, u32)>,
    max_seq: u64,
    run_state: RunState,
    /// The snapshot requests this CP took when it decided to fold: they
    /// are the CP's until it commits or fails (a request arriving while
    /// the CP is in flight stays with the engine for the next fold).
    snaps: Vec<SnapRequest>,
    /// The CP took the explicit fold request.
    fold_requested: bool,
    /// The CP folds because the policy's bounds were reached (and for no
    /// other reason): a fold that does not fit is then simply dropped.
    policy_fold: bool,
}

impl CpJob {
    /// `true` when the CP was prepared as a fold.
    fn folds(&self) -> bool {
        self.fold_requested || !self.snaps.is_empty() || self.policy_fold
    }
}

type Delta = (u32, u64, MapEntry);

/// The stripe images a CP's metadata phase still has to write.
type MetaStripeQueue = VecDeque<(u64, Vec<u8>)>;

#[derive(Debug)]
struct Staging {
    job: CpJob,
    next: usize,
    inflight: HashMap<Tag, Vec<Delta>>,
    deltas: Vec<Delta>,
    pending: Option<(u64, Vec<u8>, Vec<Delta>)>,
    failed: Option<String>,
}

/// A snapshot the engine holds: the record and the pinned root, or the
/// damage the boot found instead of the root.
#[derive(Debug, Clone)]
struct SnapState {
    record: SnapRecord,
    pinned: Option<PinnedRoot>,
    damage: Option<String>,
}

/// A snapshot asked for and not yet taken.
#[derive(Debug, Clone, Copy)]
struct SnapRequest {
    vol: u32,
    uuid: Uuid,
    kind: SnapKind,
    origin: SnapOrigin,
    /// The active generation when the request was made, when it held
    /// writes: the snapshot must see them, so the CP that carries that
    /// generation (or a later one) takes the request, never a CP whose
    /// generation was cut before.
    after_gen: Option<GenId>,
}

/// Everything a CP's metadata phase laid out, before the job joins it.
#[derive(Debug)]
struct PreparedParts {
    map_update: MapUpdate,
    segments: SegmentTable,
    open_meta: Option<OpenSegment>,
    meta_live: HashMap<u64, u64>,
    data_live: HashMap<u64, u64>,
    freed: Vec<RootRef>,
    table_regions: Vec<RootRef>,
    map_roots: RootRef,
    seg_table: RootRef,
    cp_log_head: RootRef,
    snap_table: RootRef,
    new_snapshots: Vec<SnapState>,
    carried: Vec<u32>,
    fold: bool,
    fold_pages: u64,
}

impl PreparedParts {
    fn into_prepared(self, job: CpJob) -> Prepared {
        Prepared {
            job,
            map_update: self.map_update,
            segments: self.segments,
            open_meta: self.open_meta,
            meta_live: self.meta_live,
            data_live: self.data_live,
            freed: self.freed,
            table_regions: self.table_regions,
            map_roots: self.map_roots,
            seg_table: self.seg_table,
            cp_log_head: self.cp_log_head,
            snap_table: self.snap_table,
            new_snapshots: self.new_snapshots,
            carried: self.carried,
            fold: self.fold,
            fold_pages: self.fold_pages,
            slot: None,
        }
    }
}

/// Everything a CP's metadata phase laid out; applied at commit.
#[derive(Debug)]
struct Prepared {
    job: CpJob,
    map_update: MapUpdate,
    segments: SegmentTable,
    open_meta: Option<OpenSegment>,
    meta_live: HashMap<u64, u64>,
    data_live: HashMap<u64, u64>,
    /// Regions this CP stops referencing, freed after the deferral.
    freed: Vec<RootRef>,
    /// The roots, directory and snapshot table regions of this CP.
    table_regions: Vec<RootRef>,
    map_roots: RootRef,
    seg_table: RootRef,
    cp_log_head: RootRef,
    snap_table: RootRef,
    new_snapshots: Vec<SnapState>,
    /// Volumes whose root is in this CP's table: the rows of its slot.
    carried: Vec<u32>,
    fold: bool,
    fold_pages: u64,
    /// The slot as written to `A`; `B` is the same content at the other
    /// location, so the two copies of one commit never differ.
    slot: Option<SuperblockSlot>,
}

#[derive(Debug)]
struct MetaWrite {
    prep: Prepared,
    stripes: VecDeque<(u64, Vec<u8>)>,
    inflight: HashSet<Tag>,
    failed: Option<String>,
}

#[derive(Debug)]
enum State {
    Idle,
    Staging(Box<Staging>),
    Meta(Box<MetaWrite>),
    Flush {
        prep: Box<Prepared>,
        tag: Option<Tag>,
    },
    CommitA {
        prep: Box<Prepared>,
        tag: Option<Tag>,
    },
    CommitB {
        prep: Box<Prepared>,
        tag: Option<Tag>,
    },
}

/// A clear asked for and not yet durable.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Clearing {
    /// The rollback generation the clear brings.
    gen: u32,
    /// The CP whose slot first carried the clear, once one was built.
    in_cp: Option<u64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
struct VolRt {
    present: bool,
    /// Present, but no CP has carried the creation yet: not serving.
    creating: bool,
    /// The CP whose metadata first carried the creation, once one was
    /// prepared.
    created_in: Option<u64>,
    size_blocks: u64,
    rollback_gen: u32,
    nvfail: Option<NvCause>,
    clearing: Option<Clearing>,
    windows: u8,
}

impl VolRt {
    const fn serving(&self) -> bool {
        self.present && !self.creating && self.nvfail.is_none()
    }
}

/// Counters.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Stats {
    /// Consistency points committed by this engine instance.
    pub cps: u64,
    /// Data stripes written.
    pub stripes: u64,
    /// Writes acknowledged.
    pub writes: u64,
    /// Discards and writes of zeros acknowledged.
    pub punches: u64,
    /// Reads completed.
    pub reads: u64,
    /// Writes deferred because the arena was full.
    pub deferred: u64,
    /// Intent slots written.
    pub intents: u64,
    /// Metadata segments recycled.
    pub recycled: u64,
    /// Segment discards issued.
    pub discards: u64,
    /// Folds committed.
    pub folds: u64,
    /// Snapshots created.
    pub snapshots: u64,
}

/// Reads map pages through a backstore.
struct BackstoreReader<'a, B: Backstore>(&'a mut B);

impl<B: Backstore> PageReader for BackstoreReader<'_, B> {
    fn read_blocks(&mut self, pba: u64, dst: &mut [u8]) -> Result<(), MapError> {
        self.0.read(pba, dst).map_err(|e| MapError::Io {
            pba,
            bytes: dst.len(),
            reason: e.to_string(),
        })
    }
}

/// The engine.
pub struct Engine<B: Backstore, F: FrontDoor, C: Clock> {
    cfg: EngineConfig,
    layout: DiskLayout,
    backstore: B,
    front: F,
    clock: C,
    arena: Arena,
    index: Index,
    map: MapSet,
    segments: SegmentTable,
    open_data: Option<OpenSegment>,
    open_meta: Option<OpenSegment>,
    /// Bytes of metadata the committed CP (and the regions not yet freed)
    /// reference, per metadata segment.
    meta_live: HashMap<u64, u64>,
    /// Live data blocks per data segment.
    data_live: HashMap<u64, u64>,
    /// Regions a committed CP stopped referencing, keyed by that CP; freed
    /// once a rewind can no longer reach the CP that referenced them.
    freed_pending: VecDeque<(u64, Vec<RootRef>)>,
    /// Metadata segments the other verified slot of the ring referenced at
    /// boot, with the CP they were recorded at: kept for two commits.
    boot_protect: VecDeque<(u64, Vec<u64>)>,
    /// The committed CP's roots, directory and snapshot table regions.
    table_regions: Vec<RootRef>,
    snapshots: Vec<SnapState>,
    snap_requests: Vec<SnapRequest>,
    next_snap_id: u64,
    fold_requested: bool,
    last_fold_ns: u64,
    /// A fold the policy asked for failed for want of a segment with this
    /// many free: the policy stays quiet until more are free.
    fold_blocked_at: Option<u32>,
    /// A fold failed because a map page it had to rewrite does not verify:
    /// no fold is attempted until the volume is repaired (a reboot clears
    /// the flag; the page is reported in the warnings).
    fold_damaged: bool,
    /// Data segments whose live count reached zero at a CP, with that CP:
    /// freed once a rewind can no longer reach a CP that named their
    /// blocks (the same deferral as a freed metadata region).
    dead_data_pending: VecDeque<(u64, Vec<u64>)>,
    /// Segments freed while a CP was in flight (a discard completing): the
    /// CP's copy of the table does not have the free, so it is applied
    /// again when the copy becomes the table.
    freed_during_cp: Vec<u64>,
    /// Hole record slots per generation, released when its CP commits.
    gen_holes: HashMap<GenId, Vec<RingRef>>,
    warnings: Vec<OperatorWarning>,
    /// Segments whose discard is in flight, by tag: they stay allocated
    /// in the table until the discard completes, so nothing is written
    /// where a late discard could still land.
    discarding: HashMap<Tag, u64>,
    /// Segments whose discard the backstore refused with `Busy`.
    pending_discards: Vec<u64>,
    vols: Vec<VolRt>,
    cp_seq: u64,
    cp_nonce: u64,
    cut_seq: u64,
    boot_count: u64,
    /// The highest intent sequence issued.
    intent_seq: u64,
    /// The highest intent sequence whose completion the engine has seen.
    intent_seq_landed: u64,
    /// Volumes a write may be acknowledged to without an intent: in the
    /// durable write-active set now, and in the set of every superblock
    /// slot built and not yet landed.
    durable_active: VolBitSet,
    intent_inflight: Option<(Tag, VolBitSet, u64)>,
    intent_wanted: bool,
    pending_acks: Vec<(Request, u64)>,
    state: State,
    next_tag: u64,
    nonce_counter: u64,
    deferred: VecDeque<Request>,
    serving: bool,
    stopped: bool,
    shutdown_requested: bool,
    final_gen: Option<GenId>,
    cp_requested: bool,
    /// A CP failed for want of a segment; new writes are refused and no CP
    /// is attempted until one is requested.
    pool_full: bool,
    pool_retry: bool,
    /// The boot commit failed and is issued again on the next tick (unless
    /// the pool is full, where a request is needed first).
    boot_retry: bool,
    pending_triggers: Vec<(GenId, CpTrigger)>,
    last_cut_ns: u64,
    last_commit_ns: u64,
    events: Vec<Event>,
    stats: Stats,
}

impl<B: Backstore, F: FrontDoor, C: Clock> fmt::Debug for Engine<B, F, C> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Engine")
            .field("agg_id", &self.cfg.agg_id)
            .field("cp_seq", &self.cp_seq)
            .field("phase", &self.phase())
            .field("serving", &self.serving)
            .field("stopped", &self.stopped)
            .finish_non_exhaustive()
    }
}

/// Drives one backstore synchronously (format only): submits, retrying on
/// `Busy`, and waits for the tags it was asked to.
struct SyncDriver {
    pending: HashSet<Tag>,
    next_tag: u64,
}

impl SyncDriver {
    fn new() -> Self {
        Self {
            pending: HashSet::new(),
            next_tag: 1,
        }
    }

    fn tag(&mut self) -> Tag {
        let tag = Tag(self.next_tag);
        self.next_tag += 1;
        tag
    }

    fn submit<B: Backstore>(
        &mut self,
        b: &mut B,
        tag: Tag,
        mut op: impl FnMut(&mut B) -> Result<(), Busy>,
    ) -> Result<(), EngineError> {
        loop {
            match op(b) {
                Ok(()) => {
                    self.pending.insert(tag);
                    return Ok(());
                }
                Err(Busy) => self.drain(b, false)?,
            }
        }
    }

    /// Polls once (or until nothing is pending with `all`), failing on the
    /// first error.
    fn drain<B: Backstore>(&mut self, b: &mut B, all: bool) -> Result<(), EngineError> {
        let mut dones = Vec::new();
        loop {
            b.poll(&mut dones);
            for done in dones.drain(..) {
                if self.pending.remove(&done.tag) {
                    done.result?;
                }
            }
            if !all || self.pending.is_empty() {
                return Ok(());
            }
        }
    }
}

/// Checks the E19 policy against the geometry.
fn check_flush_policy(profile: &crate::traits::MediaProfile) -> Result<(), EngineError> {
    if profile.geom.flush_opaque && profile.flush_policy == FlushPolicy::RequireFuaOrFlush {
        return Err(EngineError::FlushOpaqueRefused);
    }
    Ok(())
}

impl<B: Backstore, F: FrontDoor, C: Clock> Engine<B, F, C> {
    /// Formats the device for `cfg`: wipes the superblock ring and the intent
    /// slots, then writes consistency point 0, clean, with the configured
    /// volumes present and nothing mapped. Drives the backstore until both
    /// slots of the ring are durable.
    pub fn format(backstore: &mut B, clock: &C, cfg: &EngineConfig) -> Result<(), EngineError> {
        let profile = *backstore.profile();
        check_flush_policy(&profile)?;
        cfg.validate()?;
        let layout = DiskLayout::new(&profile.geom)?;
        for spec in &cfg.volumes {
            check_index_fits(&layout, spec.size_blocks)?;
        }
        let mut rows = [VolState::ABSENT; MAX_VOLS];
        let mut vol_count = 0u32;
        for spec in &cfg.volumes {
            let row = rows
                .get_mut(spec.vol as usize)
                .ok_or(EngineError::NoSuchVolume { vol: spec.vol })?;
            row.flags = VOL_PRESENT;
            vol_count = vol_count.max(spec.vol + 1);
        }
        let mut driver = SyncDriver::new();
        // Wipe every slot of the ring and both intent slots, so nothing an
        // earlier pool left can rank above the new one.
        let zero = vec![0u8; layout.stripe_bytes as usize];
        let per_stripe = u64::from(layout.blocks_per_stripe());
        let mut regions = Vec::new();
        for slot in SbSlot::ALL {
            regions.push((layout.sb_pba(slot), layout.slot_stripes));
        }
        for which in 0..2u8 {
            regions.push((layout.intent_pba(which), layout.intent_stripes));
        }
        for (pba, stripes) in regions {
            for k in 0..u64::from(stripes) {
                let tag = driver.tag();
                driver.submit(backstore, tag, |b| {
                    b.write_meta(pba + k * per_stripe, &zero, tag)
                })?;
            }
        }
        driver.drain(backstore, true)?;
        let tag = driver.tag();
        driver.submit(backstore, tag, |b| b.flush(DevClass::Pool, tag))?;
        driver.drain(backstore, true)?;
        // CP 0 lists the volumes as present but maps nothing: their roots
        // are written by the first boot's commit, which creates them.
        let nonce = nonce_for(cfg.agg_id, 0, clock.wall_ns(), clock.mono_ns(), 0);
        for location in [SbLocation::A, SbLocation::B] {
            let mut slot = SuperblockSlot::new(cfg.agg_id, 0, location);
            slot.sb.owner_epoch = cfg.owner_epoch;
            slot.sb.cp_nonce = nonce;
            slot.sb.wall_ns = clock.wall_ns();
            slot.sb.vol_count = vol_count;
            slot.sb.set_run_state(RunState::CleanShutdown);
            slot.vols.vols = rows;
            slot.sb.set_geometry(
                layout.stripe_bytes,
                layout.segment_bytes as u32,
                layout.capacity_bytes,
            );
            slot.seal()?;
            let pba = layout.sb_pba(slot.sb.sb_slot().expect("slot_for gave a slot"));
            let tag = driver.tag();
            driver.submit(backstore, tag, |b| b.commit(pba, &slot, tag))?;
            driver.drain(backstore, true)?;
        }
        Ok(())
    }

    /// Boots from the device: selects a verified CP, re-attaches or creates
    /// the arena in `memory`, decides nvfail, and schedules the boot commit.
    /// The engine serves once that commit is durable
    /// ([`is_serving`](Self::is_serving)); drive it with [`tick`](Self::tick).
    /// A refused boot gives everything back in the [`BootFailure`].
    pub fn boot(
        cfg: EngineConfig,
        mut backstore: B,
        front: F,
        clock: C,
        memory: HugeArena,
    ) -> Result<Self, BootFailure<B, F, C>> {
        macro_rules! fail {
            ($error:expr, $memory:expr) => {
                return Err(BootFailure {
                    error: $error,
                    backstore,
                    front,
                    clock,
                    memory: $memory,
                })
            };
        }
        if let Err(e) = check_flush_policy(backstore.profile()) {
            fail!(e, memory);
        }
        if let Err(e) = cfg.validate() {
            fail!(e, memory);
        }
        let layout = match DiskLayout::new(&backstore.profile().geom) {
            Ok(layout) => layout,
            Err(e) => fail!(e.into(), memory),
        };
        let state = match boot::evaluate(&mut backstore, &layout, &cfg.agg_id) {
            Ok(state) => state,
            Err(e) => fail!(e.into(), memory),
        };
        // The configuration is the control plane's view of the volumes; a
        // size other than the root's is a mismatch, not a resize (a resize
        // goes through a CP that rewrites the root).
        for spec in &cfg.volumes {
            if let Some(v) = state.map.volume(spec.vol) {
                if v.lba_count() != spec.size_blocks {
                    fail!(
                        EngineError::VolumeSizeMismatch {
                            vol: spec.vol,
                            on_disk: v.lba_count(),
                            configured: spec.size_blocks,
                        },
                        memory
                    );
                }
            }
        }
        let wall = clock.wall_ns();
        let (arena, reattach, arena_lost) = match Arena::can_attach(&memory, &cfg.agg_id) {
            Ok(()) => match Arena::attach(memory, &cfg.agg_id) {
                Ok((arena, report)) => (arena, Some(report), false),
                // The table could not be reconciled: the arena is lost.
                Err((_, memory)) => match Arena::create(memory, cfg.agg_id, wall) {
                    Ok(arena) => (arena, None, true),
                    Err((e, memory)) => fail!(e.into(), memory),
                },
            },
            Err(ArenaError::WrongAggregate { found }) => {
                fail!(
                    EngineError::Arena(ArenaError::WrongAggregate { found }),
                    memory
                )
            }
            Err(_) => {
                let lost = !Arena::is_blank(&memory);
                match Arena::create(memory, cfg.agg_id, wall) {
                    Ok(arena) => (arena, None, lost),
                    Err((e, memory)) => fail!(e.into(), memory),
                }
            }
        };
        let mut engine = Self::assemble(
            cfg, layout, backstore, front, clock, arena, reattach, arena_lost, state,
        );
        let job = engine.new_job(CpTrigger::Boot, None, RunState::Running);
        match engine.start_job(job) {
            Ok(()) => Ok(engine),
            Err(error) => {
                let (backstore, front, clock, memory) = engine.into_parts();
                Err(BootFailure {
                    error,
                    backstore,
                    front,
                    clock,
                    memory,
                })
            }
        }
    }

    /// Builds the engine once the device and the arena are read.
    #[allow(clippy::too_many_arguments)]
    fn assemble(
        cfg: EngineConfig,
        layout: DiskLayout,
        backstore: B,
        front: F,
        clock: C,
        mut arena: Arena,
        reattach: Option<otap_arena::Reattach>,
        arena_lost: bool,
        state: boot::BootState,
    ) -> Self {
        let rewound = state.rewound();
        let boot::BootState {
            slot,
            intents,
            map,
            segments,
            open_data,
            open_meta,
            meta_live,
            protected,
            snapshots,
            table_regions,
            torn,
            ..
        } = state;
        let torn: Vec<(u64, String)> = torn
            .into_iter()
            .map(|(cp, reason)| (cp, format!("{reason:?}")))
            .collect();
        let sb = slot.sb;
        // A rewind proves the engine ran past the selected CP, whatever that
        // CP's run state says.
        let dirty = !sb.is_clean_shutdown() || rewound;
        let now = clock.mono_ns();

        let mut vols = vec![VolRt::default(); MAX_VOLS];
        for (i, row) in slot.vols.vols.iter().enumerate() {
            if row.is_present() {
                vols[i].present = true;
                vols[i].rollback_gen = row.rollback_gen;
                vols[i].nvfail = row.nvfail_cause().filter(|c| *c != NvCause::None);
                // A present volume's size is its root's; one without a map
                // root (CP 0 of a format, or a pool whose creation CP is the
                // next one) is created with the boot commit.
                match map.volume(i as u32) {
                    Some(v) => vols[i].size_blocks = v.lba_count(),
                    None => vols[i].creating = true,
                }
            }
        }
        for spec in &cfg.volumes {
            if let Some(v) = vols.get_mut(spec.vol as usize) {
                if !v.present || v.creating {
                    v.size_blocks = spec.size_blocks;
                }
                if !v.present {
                    // The control plane knows a volume the disk does not:
                    // it is created with the boot commit. After a rewind
                    // the volume may have existed in the torn CP, with
                    // writes the client was told were done: it is created
                    // held, like the volumes whose writes the rewind lost,
                    // until the operator clears it.
                    v.present = true;
                    v.creating = true;
                    if rewound {
                        v.nvfail = Some(NvCause::TornCp);
                    }
                }
            }
        }
        for (vol, m) in map.volumes() {
            if let Some(v) = vols.get_mut(vol as usize) {
                if v.size_blocks == 0 {
                    v.size_blocks = m.lba_count();
                }
            }
        }

        let index = Index::new(cfg.ring, now);
        let mut gen_holes: HashMap<GenId, Vec<RingRef>> = HashMap::new();
        let mut events = Vec::new();
        let mut replayed_blocks = 0u64;
        let mut orphaned_blocks = 0u64;
        let mut damaged_slots = 0u32;
        if let Some(report) = &reattach {
            damaged_slots = report.damaged.len() as u32;
            for (vol, lba, extent) in runs_of(&report.placed) {
                let known = vols.get(vol as usize).is_some_and(|v| v.present);
                let slots = if extent.hole { 1 } else { extent.blocks };
                if !known {
                    orphaned_blocks += u64::from(extent.blocks);
                    if let Err(e) = arena.commit(extent.ring, slots) {
                        events.push(Event::Internal {
                            what: format!("releasing an orphaned run: {e}"),
                        });
                    }
                    continue;
                }
                match index.insert(vol, lba, extent) {
                    Ok((gen, superseded)) => {
                        if let Some(v) = vols.get_mut(vol as usize) {
                            v.windows = 2;
                        }
                        for (ring, blocks) in superseded {
                            if let Err(e) = arena.commit(ring, blocks) {
                                events.push(Event::Internal {
                                    what: format!("releasing a superseded run at replay: {e}"),
                                });
                            }
                        }
                        if extent.hole {
                            gen_holes.entry(gen).or_default().push(extent.ring);
                        }
                        replayed_blocks += u64::from(extent.blocks);
                    }
                    Err(e) => {
                        orphaned_blocks += u64::from(extent.blocks);
                        events.push(Event::Internal {
                            what: format!("replaying volume {vol} LBA {lba}: {e}"),
                        });
                        if let Err(e) = arena.commit(extent.ring, slots) {
                            events.push(Event::Internal {
                                what: format!("releasing an unreplayable run: {e}"),
                            });
                        }
                    }
                }
            }
        }

        // nvfail: a dirty boot holds the write-active set unless the arena
        // re-attached whole and nothing was rewound (then every acknowledged
        // write is still in hand).
        let arena_whole = reattach.as_ref().is_some_and(|r| !r.is_damaged());
        let hold = dirty && (!arena_whole || rewound || arena_lost || orphaned_blocks > 0);
        let active = boot_write_active(&sb, intents.iter()).unwrap_or(VolBitSet::EMPTY);
        if hold {
            let cause = if rewound {
                NvCause::TornCp
            } else {
                NvCause::DirtyBoot
            };
            for vol in active.iter() {
                if let Some(v) = vols.get_mut(vol as usize) {
                    if v.present && !v.creating && v.nvfail.is_none() {
                        v.nvfail = Some(cause);
                    }
                }
            }
        }
        let mut nvfail_now = VolBitSet::EMPTY;
        for (i, v) in vols.iter().enumerate() {
            if v.nvfail.is_some() {
                let _ = nvfail_now.set(i as u32);
            }
        }
        let mut intent_seq = sb.intent_seq_seen;
        for intent in &intents {
            if intent.verify().is_ok() && intent.agg_id == cfg.agg_id {
                intent_seq = intent_seq.max(intent.intent_seq);
            }
        }
        let mut boot_protect = VecDeque::new();
        boot_protect.push_back((sb.cp_seq, protected));

        // Data liveness as the table recorded it.
        let mut data_live = HashMap::new();
        for (pseg, entry) in segments.iter() {
            if !entry.is_free() && entry.seg_kind() == Some(SegKind::Data) {
                data_live.insert(pseg, u64::from(entry.live_bytes) / BLOCK_SIZE as u64);
            }
        }
        let next_snap_id = snapshots
            .iter()
            .map(|s| s.record.snap_id)
            .max()
            .unwrap_or(0)
            + 1;
        let mut warnings = Vec::new();
        if backstore.profile().trusts_opaque_barrier() {
            let warning = OperatorWarning::FlushOpaqueTrusted {
                policy: backstore.profile().flush_policy,
            };
            events.push(Event::OperatorWarning(warning.clone()));
            warnings.push(warning);
        }
        let snapshots: Vec<SnapState> = snapshots
            .into_iter()
            .map(
                |BootSnapshot {
                     record,
                     pinned,
                     damage,
                 }| {
                    if let Some(reason) = &damage {
                        let warning = OperatorWarning::SnapshotDamaged {
                            snap_id: record.snap_id,
                            vol: record.vol,
                            reason: reason.clone(),
                        };
                        events.push(Event::OperatorWarning(warning.clone()));
                        warnings.push(warning);
                    }
                    SnapState {
                        record,
                        pinned,
                        damage,
                    }
                },
            )
            .collect();

        events.insert(
            0,
            Event::Booted {
                cp_seq: sb.cp_seq,
                dirty,
                reattached: reattach.is_some(),
                replayed_blocks,
                rewound,
                arena_lost,
                damaged_slots,
                orphaned_blocks,
                nvfail: nvfail_now,
                torn,
            },
        );
        Self {
            layout,
            backstore,
            front,
            clock,
            arena,
            index,
            map,
            segments,
            open_data,
            open_meta,
            meta_live,
            data_live,
            freed_pending: VecDeque::new(),
            boot_protect,
            table_regions,
            snapshots,
            snap_requests: Vec::new(),
            next_snap_id,
            fold_requested: false,
            last_fold_ns: now,
            fold_blocked_at: None,
            fold_damaged: false,
            dead_data_pending: VecDeque::new(),
            freed_during_cp: Vec::new(),
            gen_holes,
            warnings,
            discarding: HashMap::new(),
            pending_discards: Vec::new(),
            vols,
            cp_seq: sb.cp_seq,
            cp_nonce: sb.cp_nonce,
            cut_seq: sb.cut_seq,
            boot_count: sb.opened_stamp + 1,
            intent_seq,
            intent_seq_landed: intent_seq,
            durable_active: active,
            intent_inflight: None,
            intent_wanted: false,
            pending_acks: Vec::new(),
            state: State::Idle,
            next_tag: 1,
            nonce_counter: 0,
            deferred: VecDeque::new(),
            serving: false,
            stopped: false,
            shutdown_requested: false,
            final_gen: None,
            cp_requested: false,
            pool_full: false,
            pool_retry: false,
            boot_retry: false,
            pending_triggers: Vec::new(),
            last_cut_ns: now,
            last_commit_ns: now,
            events,
            stats: Stats::default(),
            cfg,
        }
    }

    /// One turn of the engine: completions, the CP state machine, the front
    /// door, triggers. An `Err` is an internal failure; refused requests are
    /// completed with a status, never reported here.
    pub fn tick(&mut self) -> Result<(), EngineError> {
        let mut dones = Vec::new();
        self.backstore.poll(&mut dones);
        let mut cp_dones = Vec::new();
        for done in dones {
            if let Some(pseg) = self.discarding.remove(&done.tag) {
                // Discarded or not, nothing live is there: the segment is
                // free now that no discard can land on it late.
                if let Err(e) = done.result {
                    self.events.push(Event::Internal {
                        what: format!("the discard of segment {pseg} failed: {e}"),
                    });
                }
                self.free_segment(pseg, done.result.is_ok());
                continue;
            }
            if !self.handle_intent_done(done) {
                cp_dones.push(done);
            }
        }
        self.advance_cp(cp_dones)?;
        self.retry_intent()?;
        self.retry_discards();
        if self.boot_retry
            && matches!(self.state, State::Idle)
            && (!self.pool_full || self.pool_retry)
        {
            self.boot_retry = false;
            self.pool_retry = false;
            let job = self.new_job(CpTrigger::Boot, None, RunState::Running);
            self.start_job(job)?;
        }
        if self.serving && !self.stopped {
            self.serve_front_door()?;
        } else if self.stopped {
            self.refuse_everything();
        }
        self.maybe_cut()?;
        self.maybe_start_cp()?;
        Ok(())
    }

    /// Asks for a consistency point at the next tick (even an empty one);
    /// on a full pool, one more attempt.
    pub fn request_cp(&mut self) {
        self.cp_requested = true;
        self.pool_retry = true;
    }

    /// Asks the next consistency point to fold the map. Refused with an
    /// internal event while a map page is damaged (the warnings say which).
    pub fn request_fold(&mut self) {
        if self.fold_damaged {
            self.events.push(Event::Internal {
                what: "a fold was requested while a map page is damaged; refused".into(),
            });
            return;
        }
        self.fold_requested = true;
        self.request_cp();
    }

    /// Begins a clean shutdown: no new requests are fetched, every frozen
    /// generation is flushed, and a final CP with `CleanShutdown` is
    /// committed; afterwards [`is_stopped`](Self::is_stopped) is true. On a
    /// pool with no room for the final CP the engine stops without the
    /// clean marker and the arena keeps the unflushed writes.
    pub fn shutdown(&mut self) {
        self.shutdown_requested = true;
        self.serving = false;
    }

    /// Asks to clear nvfail on `vol`. The volume keeps answering `NvFailed`
    /// until the CP that records the bumped rollback generation commits
    /// ([`Event::NvfailCleared`]); then it serves again. A crash before that
    /// CP leaves the volume held and the generation untouched, so a
    /// generation a client saw is never handed out twice (section 7.6).
    /// Returns the generation the clear brings.
    pub fn nvfail_clear(&mut self, vol: u32) -> Result<u32, EngineError> {
        let v = self
            .vols
            .get_mut(vol as usize)
            .filter(|v| v.present)
            .ok_or(EngineError::NoSuchVolume { vol })?;
        if v.nvfail.is_none() {
            return Ok(v.rollback_gen);
        }
        if let Some(c) = v.clearing {
            return Ok(c.gen);
        }
        let gen = v.rollback_gen.wrapping_add(1);
        v.clearing = Some(Clearing { gen, in_cp: None });
        self.request_cp();
        Ok(gen)
    }

    /// Creates a volume: present at once, serving once the CP that carries
    /// its root and table row commits ([`Event::VolumeCreated`]); requests
    /// to it answer `NoSuchVolume` until then, so no acknowledged write can
    /// outlive a volume that was never durable.
    pub fn create_volume(&mut self, vol: u32, size_blocks: u64) -> Result<(), EngineError> {
        check_size(vol, size_blocks)?;
        check_index_fits(&self.layout, size_blocks)?;
        let v = self
            .vols
            .get_mut(vol as usize)
            .ok_or(EngineError::NoSuchVolume { vol })?;
        if v.present {
            return Err(EngineError::VolumeExists { vol });
        }
        v.present = true;
        v.creating = true;
        v.created_in = None;
        v.size_blocks = size_blocks;
        v.rollback_gen = 0;
        v.nvfail = None;
        v.clearing = None;
        v.windows = 0;
        self.request_cp();
        Ok(())
    }

    /// Asks for a snapshot of `vol`: taken at the next consistency point,
    /// which folds the map so the root is complete, and durable when that
    /// CP commits ([`Event::SnapshotCreated`]). Returns nothing until then;
    /// [`snapshots`](Self::snapshots) lists it afterwards.
    pub fn create_snapshot(
        &mut self,
        vol: u32,
        uuid: Uuid,
        kind: SnapKind,
        origin: SnapOrigin,
    ) -> Result<(), EngineError> {
        let v = self
            .vols
            .get(vol as usize)
            .filter(|v| v.present && !v.creating)
            .ok_or(EngineError::NoSuchVolume { vol })?;
        if v.nvfail.is_some() {
            return Err(EngineError::NvFailed { vol });
        }
        if uuid.is_nil() {
            return Err(EngineError::Format(FormatError::BadField {
                field: "snap_uuid",
                value: 0,
            }));
        }
        if self.snapshots.iter().any(|s| s.record.snap_uuid == uuid)
            || self.snap_requests.iter().any(|r| r.uuid == uuid)
            || self.in_flight_snapshot(uuid)
        {
            return Err(EngineError::SnapshotExists { uuid });
        }
        if self.fold_damaged {
            return Err(EngineError::MapDamaged);
        }
        self.snap_requests.push(SnapRequest {
            vol,
            uuid,
            kind,
            origin,
            after_gen: (self.index.active_blocks() > 0).then(|| self.index.active_id()),
        });
        self.request_fold();
        Ok(())
    }

    /// `true` while a CP in flight holds a request for `uuid`.
    fn in_flight_snapshot(&self, uuid: Uuid) -> bool {
        let job = match &self.state {
            State::Staging(st) => Some(&st.job),
            State::Meta(mw) => Some(&mw.prep.job),
            State::Flush { prep, .. }
            | State::CommitA { prep, .. }
            | State::CommitB { prep, .. } => Some(&prep.job),
            State::Idle => None,
        };
        job.is_some_and(|j| j.snaps.iter().any(|r| r.uuid == uuid))
    }

    /// The snapshots on disk, in id order.
    #[must_use]
    pub fn snapshots(&self) -> Vec<SnapshotInfo> {
        self.snapshots
            .iter()
            .map(|s| SnapshotInfo {
                snap_id: s.record.snap_id,
                uuid: s.record.snap_uuid,
                vol: s.record.vol,
                cp_seq: u64::from(s.record.birth_cp),
                kind: s.record.snap_kind().unwrap_or(SnapKind::Crash),
                origin: s.record.snap_origin().unwrap_or(SnapOrigin::User),
                created_wall_ns: s.record.created_wall_ns,
                damaged: s.damage.is_some(),
            })
            .collect()
    }

    /// Reads `dst.len() / 4096` blocks at `lba` of snapshot `snap_id`: the
    /// volume as it was at the snapshot's CP, whatever happened since.
    pub fn snapshot_read(
        &mut self,
        snap_id: u64,
        lba: u64,
        dst: &mut [u8],
    ) -> Result<Vec<PeekSource>, EngineError> {
        if !dst.len().is_multiple_of(BLOCK_SIZE) {
            return Err(EngineError::Io(IoError::BadLength { bytes: dst.len() }));
        }
        let snap = self
            .snapshots
            .iter()
            .position(|s| s.record.snap_id == snap_id)
            .ok_or(EngineError::NoSuchSnapshot { snap_id })?;
        let blocks = (dst.len() / BLOCK_SIZE) as u64;
        let pinned = self.snapshots[snap]
            .pinned
            .clone()
            .ok_or(EngineError::SnapshotDamaged { snap_id })?;
        if lba
            .checked_add(blocks)
            .is_none_or(|end| end > pinned.root.lba_count)
        {
            return Err(EngineError::Io(IoError::OutOfRange {
                pba: lba,
                bytes: dst.len(),
            }));
        }
        let mut sources = Vec::with_capacity(blocks as usize);
        for k in 0..blocks {
            let out = &mut dst[(k as usize) * BLOCK_SIZE..(k as usize + 1) * BLOCK_SIZE];
            let entry = {
                let Engine { map, backstore, .. } = self;
                map.pinned_get(&mut BackstoreReader(backstore), &pinned, lba + k)?
            };
            sources.push(self.read_entry(pinned.vol, lba + k, entry, out)?);
        }
        Ok(sources)
    }

    /// Every block of snapshot `snap_id` born after `base` (another
    /// snapshot of the same volume, older, or none): what a delta ships.
    pub fn snapshot_diff(
        &mut self,
        snap_id: u64,
        base: Option<u64>,
    ) -> Result<Vec<(u64, MapEntry)>, EngineError> {
        let snap = self
            .snapshots
            .iter()
            .find(|s| s.record.snap_id == snap_id)
            .ok_or(EngineError::NoSuchSnapshot { snap_id })?;
        let birth = u64::from(snap.record.birth_cp);
        let pinned = snap
            .pinned
            .clone()
            .ok_or(EngineError::SnapshotDamaged { snap_id })?;
        let base_cp = self.base_cp(pinned.vol, base)?;
        if let Some(base_id) = base {
            if base_cp >= birth {
                return Err(EngineError::SnapshotOrder {
                    snap_id,
                    base: base_id,
                });
            }
        }
        let mut out = Vec::new();
        let Engine { map, backstore, .. } = self;
        map.pinned_diff(
            &mut BackstoreReader(backstore),
            &pinned,
            base_cp,
            |lba, e| out.push((lba, e)),
        )?;
        Ok(out)
    }

    /// Every committed block of `vol` born after `base` (a snapshot of the
    /// volume, or none).
    pub fn live_diff(
        &mut self,
        vol: u32,
        base: Option<u64>,
    ) -> Result<Vec<(u64, MapEntry)>, EngineError> {
        let base_cp = self.base_cp(vol, base)?;
        let mut out = Vec::new();
        let Engine { map, backstore, .. } = self;
        map.diff(&mut BackstoreReader(backstore), vol, base_cp, |lba, e| {
            out.push((lba, e))
        })?;
        Ok(out)
    }

    fn base_cp(&self, vol: u32, base: Option<u64>) -> Result<u64, EngineError> {
        match base {
            None => Ok(0),
            Some(id) => {
                let s = self
                    .snapshots
                    .iter()
                    .find(|s| s.record.snap_id == id)
                    .ok_or(EngineError::NoSuchSnapshot { snap_id: id })?;
                if s.record.vol != vol {
                    return Err(EngineError::NoSuchSnapshot { snap_id: id });
                }
                Ok(u64::from(s.record.birth_cp))
            }
        }
    }

    /// The pool as an operator sees it.
    #[must_use]
    pub fn pool_status(&self) -> PoolStatus {
        let profile = self.backstore.profile();
        PoolStatus {
            cp_seq: self.cp_seq,
            fold_cp: self.map.fold_cp(),
            cps_since_fold: self.map.cps_since_fold(),
            log_bytes_since_fold: self.map.log_bytes_since_fold(),
            dirty_pages: self.map.dirty_pages(),
            overlay_entries: self.map.overlay_entries(),
            free_segments: self.segments.free_count(),
            segments: self.segments.segments(),
            flush_policy: profile.flush_policy,
            flush_opaque: profile.geom.flush_opaque,
            discard: profile.discard,
            warnings: self.warnings.clone(),
            snapshots: self.snapshots.len(),
            pool_full: self.pool_full,
            serving: self.serving,
        }
    }

    /// Reads `dst.len() / 4096` blocks at `lba` of `vol` regardless of
    /// nvfail (the support bundle's and the checker's view). Returns where
    /// each block came from.
    pub fn peek(
        &mut self,
        vol: u32,
        lba: u64,
        dst: &mut [u8],
    ) -> Result<Vec<PeekSource>, EngineError> {
        let blocks = self.peek_bounds(vol, lba, dst)?;
        let mut sources = Vec::with_capacity(blocks as usize);
        for k in 0..blocks {
            let out = &mut dst[(k as usize) * BLOCK_SIZE..(k as usize + 1) * BLOCK_SIZE];
            sources.push(self.read_block(vol, lba + k, out)?);
        }
        Ok(sources)
    }

    /// Like [`peek`](Self::peek) but through the committed map only: what a
    /// power loss right now would leave on disk.
    pub fn peek_disk(
        &mut self,
        vol: u32,
        lba: u64,
        dst: &mut [u8],
    ) -> Result<Vec<PeekSource>, EngineError> {
        let blocks = self.peek_bounds(vol, lba, dst)?;
        let mut sources = Vec::with_capacity(blocks as usize);
        for k in 0..blocks {
            let out = &mut dst[(k as usize) * BLOCK_SIZE..(k as usize + 1) * BLOCK_SIZE];
            sources.push(self.read_disk_block(vol, lba + k, out)?);
        }
        Ok(sources)
    }

    fn peek_bounds(&self, vol: u32, lba: u64, dst: &[u8]) -> Result<u64, EngineError> {
        if !dst.len().is_multiple_of(BLOCK_SIZE) {
            return Err(EngineError::Io(IoError::BadLength { bytes: dst.len() }));
        }
        let v = self
            .vols
            .get(vol as usize)
            .filter(|v| v.present)
            .ok_or(EngineError::NoSuchVolume { vol })?;
        let blocks = (dst.len() / BLOCK_SIZE) as u64;
        if lba
            .checked_add(blocks)
            .is_none_or(|end| end > v.size_blocks)
        {
            return Err(EngineError::Io(IoError::OutOfRange {
                pba: lba,
                bytes: dst.len(),
            }));
        }
        Ok(blocks)
    }

    /// The committed map entry of a block, if mapped (for checkers and the
    /// support bundle): `None` for an unmapped block, a volume without a
    /// root yet, or a page that could not be read.
    pub fn map_entry(&mut self, vol: u32, lba: u64) -> Option<MapEntry> {
        let Engine { map, backstore, .. } = self;
        match map.get(&mut BackstoreReader(backstore), vol, lba) {
            Ok(e) if !e.is_unmapped() => Some(e),
            _ => None,
        }
    }

    /// Events since the last call.
    pub fn take_events(&mut self) -> Vec<Event> {
        std::mem::take(&mut self.events)
    }

    /// `true` once the boot commit is durable and until shutdown.
    #[must_use]
    pub const fn is_serving(&self) -> bool {
        self.serving
    }

    /// `true` once the engine stopped (see [`shutdown`](Self::shutdown)).
    #[must_use]
    pub const fn is_stopped(&self) -> bool {
        self.stopped
    }

    /// `true` while a CP failed for want of a segment and none was requested
    /// since: writes answer `NoSpace`.
    #[must_use]
    pub const fn is_pool_full(&self) -> bool {
        self.pool_full
    }

    /// The committed CP.
    #[must_use]
    pub const fn cp_seq(&self) -> u64 {
        self.cp_seq
    }

    /// The committed CP's nonce.
    #[must_use]
    pub const fn cp_nonce(&self) -> u64 {
        self.cp_nonce
    }

    /// The CP state machine's phase.
    #[must_use]
    pub fn phase(&self) -> Phase {
        match &self.state {
            State::Idle => Phase::Idle,
            State::Staging(_) => Phase::Staging,
            State::Meta(_) => Phase::Metadata,
            State::Flush { .. } => Phase::Flushing,
            State::CommitA { .. } => Phase::CommitA,
            State::CommitB { .. } => Phase::CommitB,
        }
    }

    /// Volumes held by nvfail (a clear in progress still counts).
    #[must_use]
    pub fn nvfail_set(&self) -> VolBitSet {
        let mut set = VolBitSet::EMPTY;
        for (i, v) in self.vols.iter().enumerate() {
            if v.nvfail.is_some() {
                let _ = set.set(i as u32);
            }
        }
        set
    }

    /// A volume's state.
    #[must_use]
    pub fn volume(&self, vol: u32) -> Option<VolumeStatus> {
        self.vols.get(vol as usize).map(|v| VolumeStatus {
            present: v.present,
            serving: v.serving(),
            size_blocks: v.size_blocks,
            rollback_gen: v.rollback_gen,
            nvfail: v.nvfail,
            clear_pending: v.clearing.map(|c| c.gen),
            active_windows: v.windows,
        })
    }

    /// Arena occupancy, `0.0..=1.0`.
    #[must_use]
    pub fn arena_occupancy(&self) -> f64 {
        self.arena.occupancy()
    }

    /// Dirty blocks in every generation.
    #[must_use]
    pub fn unflushed_blocks(&self) -> u64 {
        self.index.unflushed_blocks()
    }

    /// Frozen generations waiting for a CP.
    #[must_use]
    pub fn frozen_generations(&self) -> usize {
        self.index.frozen_len()
    }

    /// Free segments.
    #[must_use]
    pub fn free_segments(&self) -> u32 {
        self.segments.free_count()
    }

    /// Live data blocks the engine counts in segment `pseg`.
    #[must_use]
    pub fn live_blocks(&self, pseg: u64) -> u64 {
        self.data_live.get(&pseg).copied().unwrap_or(0)
    }

    /// Bytes of metadata the committed CP references in segment `pseg`.
    #[must_use]
    pub fn meta_live_bytes(&self, pseg: u64) -> u64 {
        self.meta_live.get(&pseg).copied().unwrap_or(0)
    }

    /// Counters.
    #[must_use]
    pub const fn stats(&self) -> Stats {
        self.stats
    }

    /// The layout.
    #[must_use]
    pub const fn layout(&self) -> &DiskLayout {
        &self.layout
    }

    /// The configuration.
    #[must_use]
    pub const fn config(&self) -> &EngineConfig {
        &self.cfg
    }

    /// The map (for checkers).
    #[must_use]
    pub const fn map(&self) -> &MapSet {
        &self.map
    }

    /// The front door, for a harness that owns it.
    pub fn front_door(&mut self) -> &mut F {
        &mut self.front
    }

    /// The backstore, for a harness that owns it.
    pub fn backstore(&mut self) -> &mut B {
        &mut self.backstore
    }

    /// The arena memory, writable: for a support bundle or a fault
    /// injection (the CRCs catch what gets changed).
    pub fn arena_memory_mut(&mut self) -> &mut HugeArena {
        self.arena.memory_mut()
    }

    /// Takes the engine apart, as a `kill -9` would: the backstore, the
    /// front door, the clock and the arena memory survive; everything else
    /// is gone.
    #[must_use]
    pub fn into_parts(self) -> (B, F, C, HugeArena) {
        (
            self.backstore,
            self.front,
            self.clock,
            self.arena.into_memory(),
        )
    }

    // ----------------------------------------------------------------------
    // The front door
    // ----------------------------------------------------------------------

    /// Serves requests: deferred writes first, in order, then new ones.
    /// Writes and punches stay first-in first-out among themselves (one
    /// behind a deferred one waits); reads and flushes never wait for the
    /// arena, so a stalled device cannot starve cache hits.
    fn serve_front_door(&mut self) -> Result<(), EngineError> {
        while let Some(req) = self.deferred.pop_front() {
            if let Some(back) = self.serve(req)? {
                self.deferred.push_front(back);
                break;
            }
        }
        for _ in 0..64 {
            let Some(req) = self.front.fetch() else {
                break;
            };
            let takes_arena = matches!(req.kind, RequestKind::Write { .. }) || req.is_punch();
            if takes_arena && !self.deferred.is_empty() {
                self.deferred.push_back(req);
                self.stats.deferred += 1;
                continue;
            }
            if let Some(back) = self.serve(req)? {
                self.deferred.push_back(back);
                self.stats.deferred += 1;
            }
        }
        // A deferred write wants the arena drained: ask for a CP when there
        // is something to flush (an empty CP frees nothing).
        if !self.deferred.is_empty() && self.index.active_blocks() > 0 {
            self.cp_requested = true;
        }
        Ok(())
    }

    fn refuse_everything(&mut self) {
        while let Some(req) = self.deferred.pop_front() {
            self.front.complete(req, Status::Stopped, 0);
        }
        while let Some(req) = self.front.fetch() {
            self.front.complete(req, Status::Stopped, 0);
        }
        for (req, _) in std::mem::take(&mut self.pending_acks) {
            self.front.complete(req, Status::Stopped, 0);
        }
    }

    /// Serves one request; `Some(req)` hands it back to be retried later.
    fn serve(&mut self, req: Request) -> Result<Option<Request>, EngineError> {
        let vol = req.vol();
        let Some(v) = self
            .vols
            .get(vol as usize)
            .copied()
            .filter(|v| v.present && !v.creating)
        else {
            self.front.complete(req, Status::NoSuchVolume, 0);
            return Ok(None);
        };
        if v.nvfail.is_some() {
            self.front.complete(req, Status::NvFailed, 0);
            return Ok(None);
        }
        let in_range = |lba: u64, blocks: u32| {
            blocks > 0
                && lba
                    .checked_add(u64::from(blocks))
                    .is_some_and(|end| end <= v.size_blocks)
        };
        match req.kind {
            RequestKind::Write { lba, blocks, .. } => {
                if !in_range(lba, blocks) {
                    self.front.complete(req, Status::OutOfRange, 0);
                    return Ok(None);
                }
                if blocks > MAX_REQUEST_BLOCKS || blocks > self.arena.slots() {
                    self.front.complete(req, Status::TooLarge, 0);
                    return Ok(None);
                }
                if self.pool_full {
                    self.front.complete(req, Status::NoSpace, 0);
                    return Ok(None);
                }
                self.serve_write(req, vol, lba, blocks)
            }
            RequestKind::Discard { lba, blocks, .. }
            | RequestKind::WriteZeroes { lba, blocks, .. } => {
                if !in_range(lba, blocks) {
                    self.front.complete(req, Status::OutOfRange, 0);
                    return Ok(None);
                }
                if blocks > MAX_PUNCH_BLOCKS {
                    self.front.complete(req, Status::TooLarge, 0);
                    return Ok(None);
                }
                if self.pool_full {
                    self.front.complete(req, Status::NoSpace, 0);
                    return Ok(None);
                }
                self.serve_punch(req, vol, lba, blocks)
            }
            RequestKind::Read { lba, blocks, .. } => {
                if !in_range(lba, blocks) {
                    self.front.complete(req, Status::OutOfRange, 0);
                    return Ok(None);
                }
                if blocks > MAX_REQUEST_BLOCKS {
                    self.front.complete(req, Status::TooLarge, 0);
                    return Ok(None);
                }
                self.serve_read(req, vol, lba, blocks)?;
                Ok(None)
            }
            RequestKind::Flush { .. } => {
                self.front.complete(req, Status::Ok, 0);
                Ok(None)
            }
        }
    }

    fn serve_write(
        &mut self,
        req: Request,
        vol: u32,
        lba: u64,
        blocks: u32,
    ) -> Result<Option<Request>, EngineError> {
        let ring = match self.arena.reserve(blocks) {
            Ok(ring) => ring,
            Err(ArenaError::Full { .. }) => return Ok(Some(req)),
            Err(ArenaError::LapExhausted) => {
                self.events.push(Event::Internal {
                    what: "the arena's lap counter is exhausted".into(),
                });
                self.front.complete(req, Status::Failed, 0);
                return Ok(None);
            }
            Err(e) => return Err(e.into()),
        };
        let copied = {
            let dst = self.arena.run_mut(ring, blocks)?;
            self.front.copy_in(&req, dst)
        };
        if copied.is_err() {
            // The client's buffer is gone: give the slots back.
            self.arena.discard(ring, blocks)?;
            self.front.complete(req, Status::Failed, 0);
            return Ok(None);
        }
        let crcs: Vec<u32> = self
            .arena
            .run_mut(ring, blocks)?
            .as_chunks::<BLOCK_SIZE>()
            .0
            .iter()
            .map(|block| crc32c(block))
            .collect();
        let seq = self.arena.next_seq();
        self.arena.fill(ring, vol, lba, seq, &crcs)?;
        let superseded = match self
            .index
            .insert(vol, lba, DirtyExtent::data(ring, blocks, seq))
        {
            Ok((_, superseded)) => superseded,
            Err(e) => {
                self.arena.discard(ring, blocks)?;
                match e {
                    IndexError::BadExtent { .. } | IndexError::BadVol { .. } => {
                        self.front.complete(req, Status::OutOfRange, 0);
                        return Ok(None);
                    }
                    other => return Err(other.into()),
                }
            }
        };
        self.arena.place(ring, blocks)?;
        // Blocks this write replaced inside the active generation are dead:
        // their slots go back to the ring now rather than at the next CP.
        for (old, old_blocks) in superseded {
            self.arena.commit(old, old_blocks)?;
        }
        self.stats.writes += 1;
        self.acknowledge(req, vol, seq);
        Ok(None)
    }

    /// A discard or a write of zeros: one hole record in the arena, a hole
    /// extent in the index, acknowledged like a write.
    fn serve_punch(
        &mut self,
        req: Request,
        vol: u32,
        lba: u64,
        blocks: u32,
    ) -> Result<Option<Request>, EngineError> {
        let ring = match self.arena.reserve(1) {
            Ok(ring) => ring,
            Err(ArenaError::Full { .. }) => return Ok(Some(req)),
            Err(ArenaError::LapExhausted) => {
                self.events.push(Event::Internal {
                    what: "the arena's lap counter is exhausted".into(),
                });
                self.front.complete(req, Status::Failed, 0);
                return Ok(None);
            }
            Err(e) => return Err(e.into()),
        };
        let seq = self.arena.next_seq();
        self.arena.fill_hole(ring, vol, lba, blocks, seq)?;
        let (gen, superseded) =
            match self
                .index
                .insert(vol, lba, DirtyExtent::hole(ring, blocks, seq))
            {
                Ok(x) => x,
                Err(e) => {
                    self.arena.discard(ring, 1)?;
                    match e {
                        IndexError::BadExtent { .. } | IndexError::BadVol { .. } => {
                            self.front.complete(req, Status::OutOfRange, 0);
                            return Ok(None);
                        }
                        other => return Err(other.into()),
                    }
                }
            };
        self.arena.place(ring, 1)?;
        for (old, old_blocks) in superseded {
            self.arena.commit(old, old_blocks)?;
        }
        self.gen_holes.entry(gen).or_default().push(ring);
        self.stats.punches += 1;
        self.acknowledge(req, vol, seq);
        Ok(None)
    }

    /// Acknowledges a placed write or punch, after an intent when precise
    /// mode needs one.
    fn acknowledge(&mut self, req: Request, vol: u32, seq: u64) {
        if let Some(v) = self.vols.get_mut(vol as usize) {
            v.windows = 2;
        }
        if self.backstore.profile().nvfail_intent == NvfailIntent::Precise
            && !self.durable_active.get(vol)
        {
            self.intent_wanted = true;
            self.pending_acks.push((req, seq));
            return;
        }
        self.front.complete(req, Status::Ok, seq);
    }

    fn serve_read(
        &mut self,
        req: Request,
        vol: u32,
        lba: u64,
        blocks: u32,
    ) -> Result<(), EngineError> {
        let mut buf = vec![0u8; blocks as usize * BLOCK_SIZE];
        for k in 0..blocks {
            let out = &mut buf[(k as usize) * BLOCK_SIZE..(k as usize + 1) * BLOCK_SIZE];
            match self.read_block(vol, lba + u64::from(k), out) {
                Ok(_) => {}
                // A block, or the map page that names it, that does not
                // verify: this request's answer, not the engine's failure.
                Err(EngineError::Corrupt { .. }) | Err(EngineError::Map(MapError::Format(_))) => {
                    self.front.complete(req, Status::Corrupt, 0);
                    return Ok(());
                }
                // The device refused a read: likewise.
                Err(EngineError::Io(_)) | Err(EngineError::Map(MapError::Io { .. })) => {
                    self.front.complete(req, Status::Failed, 0);
                    return Ok(());
                }
                Err(e) => return Err(e),
            }
        }
        self.stats.reads += 1;
        match self.front.copy_out(&req, &buf) {
            Ok(()) => self.front.complete(req, Status::Ok, 0),
            Err(_) => self.front.complete(req, Status::Failed, 0),
        }
        Ok(())
    }

    /// One block: the arena first (newest generation wins, checked against
    /// the CRC recorded when it was written; a hole reads as zeros), then
    /// the map.
    fn read_block(
        &mut self,
        vol: u32,
        lba: u64,
        out: &mut [u8],
    ) -> Result<PeekSource, EngineError> {
        if let Some(hit) = self.index.lookup(vol, lba) {
            if hit.hole {
                out.fill(0);
                return Ok(PeekSource::Hole);
            }
            match self.arena.check_data(hit.ring, 1) {
                Ok(()) => {}
                Err(ArenaError::DataCrc { .. }) => return Err(EngineError::Corrupt { vol, lba }),
                Err(e) => return Err(e.into()),
            }
            let data = self.arena.run(hit.ring, 1)?;
            out.copy_from_slice(data);
            return Ok(PeekSource::Arena);
        }
        self.read_disk_block(vol, lba, out)
    }

    /// One block through the committed map only.
    fn read_disk_block(
        &mut self,
        vol: u32,
        lba: u64,
        out: &mut [u8],
    ) -> Result<PeekSource, EngineError> {
        let entry = {
            let Engine { map, backstore, .. } = self;
            match map.get(&mut BackstoreReader(backstore), vol, lba) {
                Ok(e) => e,
                // A volume without a root yet maps nothing.
                Err(MapError::NoSuchVolume { .. }) => MapEntry::UNMAPPED,
                Err(e) => return Err(e.into()),
            }
        };
        self.read_entry(vol, lba, entry, out)
    }

    /// Reads what a map entry names.
    fn read_entry(
        &mut self,
        vol: u32,
        lba: u64,
        entry: MapEntry,
        out: &mut [u8],
    ) -> Result<PeekSource, EngineError> {
        if entry.is_data() {
            let pba = self
                .layout
                .block_pba(entry.lseg(), entry.off())
                .ok_or_else(|| {
                    EngineError::Internal(format!("map entry of {vol}/{lba} is off the device"))
                })?;
            self.backstore.read(pba, out)?;
            if crc32c(out) != entry.crc32c {
                return Err(EngineError::Corrupt { vol, lba });
            }
            Ok(PeekSource::Disk)
        } else if entry.is_hole() {
            out.fill(0);
            Ok(PeekSource::Hole)
        } else {
            out.fill(0);
            Ok(PeekSource::Unmapped)
        }
    }

    // ----------------------------------------------------------------------
    // Intents (precise nvfail mode)
    // ----------------------------------------------------------------------

    fn handle_intent_done(&mut self, done: Done) -> bool {
        let Some((tag, set, seq)) = self.intent_inflight else {
            return false;
        };
        if done.tag != tag {
            return false;
        }
        self.intent_inflight = None;
        match done.result {
            Ok(()) => {
                // Every slot built so far recorded the landed sequence
                // before this one as `intent_seq_seen`, so this intent
                // widens the durable set whether or not such a slot lands.
                self.intent_seq_landed = self.intent_seq_landed.max(seq);
                self.durable_active.union_with(&set);
                self.stats.intents += 1;
                self.events.push(Event::IntentWritten {
                    intent_seq: seq,
                    write_active: set,
                });
                let (ready, waiting): (Vec<_>, Vec<_>) = std::mem::take(&mut self.pending_acks)
                    .into_iter()
                    .partition(|(req, _)| set.get(req.vol()));
                self.pending_acks = waiting;
                for (req, seq) in ready {
                    self.front.complete(req, Status::Ok, seq);
                }
                if !self.pending_acks.is_empty() {
                    self.intent_wanted = true;
                }
            }
            Err(_) => {
                self.intent_wanted = true;
            }
        }
        true
    }

    fn retry_intent(&mut self) -> Result<(), EngineError> {
        if !self.intent_wanted || self.intent_inflight.is_some() {
            return Ok(());
        }
        // What the disk guarantees plus the volumes waiting to be
        // acknowledged. Nothing else: a volume with a window left is either
        // already in the durable set or has a write waiting here, and
        // widening the slot with stale windows would hold a volume idle for
        // two CPs at the next dirty boot (invariant 6).
        let mut set = self.durable_active;
        for (req, _) in &self.pending_acks {
            set.set(req.vol())?;
        }
        let seq = self.intent_seq + 1;
        let mut slot = IntentSlot::new(self.cfg.agg_id, seq);
        slot.cp_seq = self.cp_seq;
        slot.wall_ns = self.clock.wall_ns();
        slot.write_active = set;
        slot.seal();
        let tag = self.tag();
        if self
            .backstore
            .write_intent(self.layout.intent_pba(slot.slot), &slot, tag)
            .is_ok()
        {
            self.intent_seq = seq;
            self.intent_inflight = Some((tag, set, seq));
            self.intent_wanted = false;
        }
        Ok(())
    }

    /// Issues the segment discards a `Busy` backstore refused earlier.
    fn retry_discards(&mut self) {
        let pending = std::mem::take(&mut self.pending_discards);
        for pseg in pending {
            self.discard(pseg);
        }
    }

    /// Issues a whole-segment discard, or queues it when the backstore is
    /// busy. The segment stays allocated until the discard completes.
    fn discard(&mut self, pseg: u64) {
        let Some(pba) = self.layout.segment_first_pba(pseg) else {
            self.free_segment(pseg, false);
            return;
        };
        let tag = self.tag();
        match self
            .backstore
            .discard_segment(pba, self.layout.segment_bytes, tag)
        {
            Ok(()) => {
                self.discarding.insert(tag, pseg);
                self.stats.discards += 1;
            }
            Err(Busy) => self.pending_discards.push(pseg),
        }
    }

    /// Returns a dead metadata segment to the pool.
    fn free_segment(&mut self, pseg: u64, discarded: bool) {
        match self.segments.free(pseg) {
            Ok(()) => {
                self.meta_live.remove(&pseg);
                self.data_live.remove(&pseg);
                self.stats.recycled += 1;
                self.events.push(Event::SegmentFreed { pseg, discarded });
                // A CP past its metadata preparation works on a copy of the
                // table taken before this free: the free is applied again
                // when the copy lands. (A CP still staging copies the table
                // later and sees the free itself.)
                if !matches!(self.state, State::Idle | State::Staging(_)) {
                    self.freed_during_cp.push(pseg);
                }
            }
            Err(e) => self.events.push(Event::Internal {
                what: format!("recycling segment {pseg}: {e}"),
            }),
        }
    }

    /// `true` while a discard of `pseg` is in flight or waiting.
    fn discard_pending(&self, pseg: u64) -> bool {
        self.discarding.values().any(|p| *p == pseg) || self.pending_discards.contains(&pseg)
    }

    // ----------------------------------------------------------------------
    // Triggers and the CP state machine
    // ----------------------------------------------------------------------

    fn maybe_cut(&mut self) -> Result<(), EngineError> {
        if self.stopped {
            return Ok(());
        }
        let now = self.clock.mono_ns();
        let profile = *self.backstore.profile();
        let active_blocks = self.index.active_blocks();
        let unflushed_bytes = self.index.unflushed_blocks() * BLOCK_SIZE as u64;
        // One generation at a time waits for the flusher: a trigger that
        // holds while a CP is already queued would otherwise cut a sliver
        // every tick. Shutdown is the exception: its cut must happen once
        // the queue drains, and the condition below guarantees that.
        let waiting = self.index.frozen_len() > 0;
        let idle = matches!(self.state, State::Idle) && !waiting;
        let reason = if self.shutdown_requested
            && self.final_gen.is_none()
            && self.serving_or_booted()
        {
            if waiting {
                None
            } else {
                Some(CpTrigger::Shutdown)
            }
        } else if waiting || self.pool_full {
            None
        } else if self.cp_requested && self.serving && idle {
            Some(CpTrigger::Request)
        } else if active_blocks > 0 && self.arena.occupancy() >= self.cfg.arena_half {
            Some(CpTrigger::ArenaHalf)
        } else if active_blocks > 0 && now.saturating_sub(self.last_cut_ns) >= profile.cp_timer_ns {
            Some(CpTrigger::Timer)
        } else if active_blocks > 0 && idle && unflushed_bytes >= profile.back_to_back_bytes {
            Some(CpTrigger::BackToBack)
        } else {
            None
        };
        let Some(reason) = reason else {
            return Ok(());
        };
        match self.index.cut(now) {
            Ok(gen) => {
                self.last_cut_ns = now;
                self.cut_seq = gen.id();
                self.pending_triggers.push((gen.id(), reason));
                if reason == CpTrigger::Shutdown {
                    self.final_gen = Some(gen.id());
                }
                if reason == CpTrigger::Request {
                    self.cp_requested = false;
                }
                Ok(())
            }
            Err(IndexError::RingFull { .. }) => Ok(()),
            Err(e) => Err(e.into()),
        }
    }

    /// `true` once the boot commit landed (serving) or shutdown was asked
    /// after it.
    fn serving_or_booted(&self) -> bool {
        self.serving || (self.shutdown_requested && self.boot_done())
    }

    fn boot_done(&self) -> bool {
        self.stats.cps > 0
    }

    fn maybe_start_cp(&mut self) -> Result<(), EngineError> {
        if !matches!(self.state, State::Idle) {
            return Ok(());
        }
        let Some(gen) = self.index.oldest_frozen() else {
            return Ok(());
        };
        if self.pool_full && !self.pool_retry && !self.shutdown_requested {
            return Ok(());
        }
        self.pool_retry = false;
        let trigger = self
            .pending_triggers
            .iter()
            .find(|(id, _)| *id == gen.id())
            .map_or(CpTrigger::Timer, |(_, t)| *t);
        let run_state = if self.final_gen == Some(gen.id()) {
            RunState::CleanShutdown
        } else {
            RunState::Running
        };
        let job = self.new_job(trigger, Some(gen), run_state);
        self.start_job(job)
    }

    fn new_job(
        &mut self,
        trigger: CpTrigger,
        gen: Option<Arc<Generation>>,
        run_state: RunState,
    ) -> CpJob {
        let cp_seq = self.cp_seq + 1;
        let nonce = self.new_nonce(cp_seq);
        let mut blocks = Vec::new();
        let mut holes = Vec::new();
        let mut max_seq = 0u64;
        if let Some(gen) = &gen {
            gen.for_each_dirty_volume(|vol, dirty| {
                for (lba, ext) in dirty.iter() {
                    max_seq = max_seq.max(ext.seq);
                    if ext.hole {
                        holes.push((vol, lba, ext.blocks));
                        continue;
                    }
                    for k in 0..ext.blocks {
                        blocks.push(StagedBlock {
                            vol,
                            lba: lba + u64::from(k),
                            ring: ext.ring.offset(k),
                            seq: ext.seq,
                        });
                    }
                }
            });
        }
        CpJob {
            snaps: Vec::new(),
            fold_requested: false,
            policy_fold: false,
            cp_seq,
            nonce,
            trigger,
            gen,
            blocks,
            holes,
            max_seq,
            run_state,
        }
    }

    fn start_job(&mut self, job: CpJob) -> Result<(), EngineError> {
        if job.cp_seq > MAX_CP_SEQ {
            return Err(EngineError::Internal("CP sequence exhausted".into()));
        }
        let staging = Staging {
            job,
            next: 0,
            inflight: HashMap::new(),
            deltas: Vec::new(),
            pending: None,
            failed: None,
        };
        self.state = State::Staging(Box::new(staging));
        self.advance_cp(Vec::new())
    }

    fn advance_cp(&mut self, dones: Vec<Done>) -> Result<(), EngineError> {
        let state = std::mem::replace(&mut self.state, State::Idle);
        self.state = match state {
            State::Idle => State::Idle,
            State::Staging(st) => self.step_staging(st, dones)?,
            State::Meta(mw) => self.step_meta(mw, dones)?,
            State::Flush { prep, tag } => self.step_flush(prep, tag, dones)?,
            State::CommitA { prep, tag } => self.step_commit(prep, tag, dones, SbLocation::A)?,
            State::CommitB { prep, tag } => self.step_commit(prep, tag, dones, SbLocation::B)?,
        };
        Ok(())
    }

    /// Gives a CP attempt up. With `no_space` the pool is full: writes are
    /// refused until a CP is requested, and a shutdown that cannot write
    /// what it has to stops without the clean marker.
    /// Refuses the fold requests a failed CP took: the snapshot requests
    /// are answered with [`Event::SnapshotFailed`], the explicit fold
    /// request with an internal event, and none is retried.
    fn fail_fold_requests(&mut self, job: &mut CpJob, reason: &str) {
        for req in job.snaps.drain(..) {
            self.events.push(Event::SnapshotFailed {
                uuid: req.uuid,
                vol: req.vol,
                reason: reason.to_string(),
            });
        }
        if std::mem::take(&mut job.fold_requested) {
            self.events.push(Event::Internal {
                what: format!("the requested fold was refused: {reason}"),
            });
        }
        job.policy_fold = false;
    }
    /// Answers every snapshot request still waiting with
    /// [`Event::SnapshotFailed`]: the engine is stopping and will not take
    /// them.
    fn fail_pending_snapshots(&mut self, reason: &str) {
        for req in std::mem::take(&mut self.snap_requests) {
            self.events.push(Event::SnapshotFailed {
                uuid: req.uuid,
                vol: req.vol,
                reason: reason.to_string(),
            });
        }
    }

    fn fail_cp(&mut self, job: &CpJob, reason: String, no_space: bool) -> State {
        self.events.push(Event::CpFailed {
            cp_seq: job.cp_seq,
            reason,
        });
        // The requests the CP still holds go back for the next attempt.
        self.snap_requests.splice(0..0, job.snaps.iter().copied());
        self.fold_requested |= job.fold_requested;
        // Frees that landed meanwhile stand: the committed table has them.
        self.freed_during_cp.clear();
        if job.trigger == CpTrigger::Boot && !self.boot_done() {
            // The boot commit is what makes the engine serve: a failed
            // attempt is issued again (after a request, on a full pool).
            self.boot_retry = true;
        }
        if no_space {
            self.pool_full = true;
            if self.shutdown_requested {
                self.stopped = true;
                self.serving = false;
                self.fail_pending_snapshots("the engine stopped");
                self.events.push(Event::Stopped {
                    cp_seq: self.cp_seq,
                    clean: false,
                });
            }
        }
        State::Idle
    }

    fn step_staging(
        &mut self,
        mut st: Box<Staging>,
        dones: Vec<Done>,
    ) -> Result<State, EngineError> {
        for done in dones {
            if let Some(entries) = st.inflight.remove(&done.tag) {
                match done.result {
                    Ok(()) => st.deltas.extend(entries),
                    Err(e) => st.failed = Some(format!("stripe write: {e}")),
                }
            }
        }
        if let Some(reason) = st.failed.take() {
            if st.inflight.is_empty() {
                return Ok(self.fail_cp(&st.job, reason, false));
            }
            st.failed = Some(reason);
            return Ok(State::Staging(st));
        }
        let max_inflight = usize::from(self.backstore.profile().inflight_stripes.clamp(1, 64));
        while st.inflight.len() < max_inflight {
            if st.pending.is_none() && st.next < st.job.blocks.len() {
                match self.build_stripe(&st.job, st.next) {
                    Ok((next, pba, image, entries)) => {
                        st.next = next;
                        st.pending = Some((pba, image, entries));
                    }
                    Err(EngineError::NoSpace) => {
                        // Stripes in flight land in a segment nothing will
                        // name; their completions fall through `Idle`.
                        return Ok(self.fail_cp(&st.job, "no free segment".into(), true));
                    }
                    Err(e) => return Err(e),
                }
            }
            let Some((pba, image, entries)) = st.pending.take() else {
                break;
            };
            let tag = self.tag();
            match self.backstore.write_stripe(pba, &image, tag) {
                Ok(()) => {
                    st.inflight.insert(tag, entries);
                    self.stats.stripes += 1;
                }
                Err(Busy) => {
                    st.pending = Some((pba, image, entries));
                    break;
                }
            }
        }
        if st.next == st.job.blocks.len() && st.inflight.is_empty() && st.pending.is_none() {
            let st = *st;
            return match self.prepare_meta(st.job, st.deltas) {
                Ok(mw) => self.step_meta(Box::new(mw), Vec::new()),
                Err(failed) => {
                    let (error, mut job) = *failed;
                    match error {
                        // Out of space anywhere in the CP's metadata, whoever
                        // placed the region, is a full pool: writes are
                        // refused, reads go on, and the next request tries
                        // again. A fold that did not fit is dropped (the
                        // policy stays quiet, and requests wait, until
                        // segments are freed) so the plain CP that follows
                        // can commit; a shutdown's fold is retried at once as
                        // a plain CP, so the clean marker still lands.
                        EngineError::NoSpace
                        | EngineError::Map(MapError::NoSpace)
                        | EngineError::Meta(MetaError::NoSpace) => {
                            let shutdown_fold =
                                job.folds() && job.run_state == RunState::CleanShutdown;
                            if job.folds() {
                                self.fold_blocked_at = Some(self.free_segments());
                            }
                            Ok(self.fail_cp(&job, "no free segment".into(), !shutdown_fold))
                        }
                        // A fold had to rewrite a map page that does not
                        // verify: folds stop until the volume is repaired;
                        // plain CPs go on (their records log the new entries,
                        // the old ones unknown).
                        EngineError::Map(MapError::Format(e)) if job.folds() => {
                            self.fold_damaged = true;
                            self.fail_fold_requests(
                                &mut job,
                                &format!("a map page does not verify: {e}"),
                            );
                            Ok(self.fail_cp(&job, format!("fold: map page: {e}"), false))
                        }
                        other => Err(other),
                    }
                }
            };
        }
        Ok(State::Staging(st))
    }

    /// Builds the next full stripe from `job.blocks[from..]`: the summary
    /// page(s), then the data, every byte of the stripe in memory before the
    /// backstore sees it. Each block carries the CRC recorded when it was
    /// written into the arena; a block that no longer matches it is reported
    /// and goes to disk under that CRC, so a read of it answers `Corrupt`
    /// instead of returning damaged data as good.
    fn build_stripe(
        &mut self,
        job: &CpJob,
        from: usize,
    ) -> Result<(usize, u64, Vec<u8>, Vec<Delta>), EngineError> {
        let per_stripe = self.layout.data_blocks_per_stripe() as usize;
        let n = per_stripe.min(job.blocks.len() - from);
        let open = self.open_data_segment(job.cp_seq as u32)?;
        let (pseg, idx) = (open.pseg, open.next_idx);
        let stripe_bytes = self.layout.stripe_bytes as usize;
        let mut image = vec![0u8; stripe_bytes];
        let mut descs = Vec::with_capacity(n);
        let mut entries = Vec::with_capacity(n);
        let birth = job.cp_seq as u32;
        for i in 0..n {
            let blk = job.blocks[from + i];
            let recorded = self.arena.descriptor(blk.ring.slot)?.crc32c;
            let data = self.arena.run(blk.ring, 1)?;
            let at = self.layout.summary.block_offset(i);
            image[at..at + BLOCK_SIZE].copy_from_slice(data);
            if crc32c(data) != recorded {
                self.events.push(Event::ArenaDamage {
                    vol: blk.vol,
                    lba: blk.lba,
                    slot: blk.ring.slot,
                });
            }
            descs.push(StripeDescriptor::data(blk.vol, blk.lba, birth, recorded)?);
            let off = self.layout.block_off(idx, i as u32);
            entries.push((
                blk.vol,
                blk.lba,
                MapEntry::data(pseg, off, SECTORS_PER_BLOCK, birth, recorded)?,
            ));
        }
        let mut hdr = StripeHdr::new(self.cfg.agg_id, StripeKind::Ingest);
        hdr.cp_seq = job.cp_seq;
        hdr.cp_nonce = job.nonce;
        hdr.lseg = pseg;
        hdr.stripe_idx = idx;
        hdr.nblocks = n as u16;
        hdr.group = 0;
        let summary_len = self.layout.summary.summary_bytes;
        otap_format::write_summary(&mut image[..summary_len], &hdr, &descs)?;
        let pba = self.layout.stripe_pba(pseg, idx).ok_or_else(|| {
            EngineError::Internal(format!("stripe {idx} of segment {pseg} is off the device"))
        })?;
        self.open_data = Some(OpenSegment {
            pseg,
            next_idx: idx + 1,
        });
        Ok((from + n, pba, image, entries))
    }

    fn open_data_segment(&mut self, cp_seq: u32) -> Result<OpenSegment, EngineError> {
        if let Some(open) = self.open_data {
            if open.next_idx < self.layout.stripes_per_segment {
                return Ok(open);
            }
            self.segments.seal(open.pseg)?;
            self.open_data = None;
        }
        let pseg = match self.segments.allocate_keeping(
            SegKind::Data,
            cp_seq.max(1),
            self.layout.stripe_bytes,
            META_RESERVE_SEGMENTS,
        ) {
            Ok(pseg) => pseg,
            Err(SegmentError::NoSpace) => return Err(EngineError::NoSpace),
            Err(e) => return Err(e.into()),
        };
        let open = OpenSegment { pseg, next_idx: 0 };
        self.open_data = Some(open);
        Ok(open)
    }

    /// Whether this CP folds the map (E22): on request, for a snapshot, at
    /// `true` when CP `job` may take snapshot request `req`: the request
    /// was made before the job's generation was cut (or after everything
    /// written so far was committed), so the snapshot sees every write
    /// acknowledged before the request.
    fn snap_eligible(&self, req: &SnapRequest, job: &CpJob) -> bool {
        match (req.after_gen, &job.gen) {
            (None, _) => true,
            (Some(rg), Some(g)) => rg <= g.id(),
            (Some(rg), None) => rg < self.index.active_id(),
        }
    }

    /// a clean shutdown, or when the fold policy's bounds are reached.
    fn should_fold(&self, job: &CpJob, pending_deltas: usize) -> bool {
        // A damaged map page cannot be rewritten from what it holds: no
        // fold at all until the volume is repaired (requests are refused
        // where they are made).
        if self.fold_damaged {
            return false;
        }
        // Nothing folds while a fold would not fit: requests wait, and the
        // policy stays quiet, until segments are freed (or a reboot).
        if self
            .fold_blocked_at
            .is_some_and(|at| self.free_segments() <= at)
        {
            return false;
        }
        if self.fold_requested
            || self
                .snap_requests
                .iter()
                .any(|r| self.snap_eligible(r, job))
        {
            return true;
        }
        if job.run_state == RunState::CleanShutdown
            && (self.map.overlay_entries() > 0 || pending_deltas > 0)
        {
            return true;
        }
        let policy = self.cfg.fold;
        let pending_bytes = (pending_deltas as u64).saturating_mul(32);
        let dirty = self.map.overlay_entries() > 0 || pending_deltas > 0;
        dirty
            && (self
                .map
                .log_bytes_since_fold()
                .saturating_add(pending_bytes)
                >= policy.log_bytes
                || self.map.cps_since_fold() + 1 >= policy.max_cps
                || self.map.dirty_pages() >= policy.max_dirty_pages
                || self.map.overlay_entries() >= policy.max_overlay_entries
                || self.clock.mono_ns().saturating_sub(self.last_fold_ns) >= policy.interval_ns)
    }

    /// Lays out the CP's metadata; gives the job back on failure so the
    /// attempt can be recorded.
    fn prepare_meta(
        &mut self,
        mut job: CpJob,
        deltas: Vec<Delta>,
    ) -> Result<MetaWrite, Box<(EngineError, CpJob)>> {
        let pending = deltas.len()
            + job
                .holes
                .iter()
                .map(|(_, _, blocks)| *blocks as usize)
                .sum::<usize>();
        let fold = self.should_fold(&job, pending);
        if fold {
            // The requests this CP answers are its own from here on.
            let (mine, later): (Vec<SnapRequest>, Vec<SnapRequest>) =
                std::mem::take(&mut self.snap_requests)
                    .into_iter()
                    .partition(|r| self.snap_eligible(r, &job));
            job.snaps = mine;
            self.snap_requests = later;
            job.fold_requested = std::mem::take(&mut self.fold_requested);
            job.policy_fold = !job.fold_requested
                && job.snaps.is_empty()
                && job.run_state != RunState::CleanShutdown;
        }
        match self.prepare_meta_inner(&job, deltas, fold) {
            Ok((parts, stripes)) => Ok(MetaWrite {
                prep: parts.into_prepared(job),
                stripes,
                inflight: HashSet::new(),
                failed: None,
            }),
            Err(e) => Err(Box::new((e, job))),
        }
    }

    #[allow(clippy::too_many_lines)]
    fn prepare_meta_inner(
        &mut self,
        job: &CpJob,
        mut deltas: Vec<Delta>,
        fold: bool,
    ) -> Result<(PreparedParts, MetaStripeQueue), EngineError> {
        let cp32 = job.cp_seq as u32;
        // The holes the generation holds become hole entries.
        for (vol, lba, blocks) in &job.holes {
            for k in 0..*blocks {
                deltas.push((*vol, lba + u64::from(k), MapEntry::hole(cp32)?));
            }
        }
        // Volumes without a root yet get one with this CP (a failed attempt
        // left `created_in` set but the map untouched, so the map is the
        // test).
        let new_volumes: Vec<(u32, u64)> = self
            .vols
            .iter()
            .enumerate()
            .filter(|(i, v)| v.present && v.creating && self.map.volume(*i as u32).is_none())
            .map(|(i, v)| (i as u32, v.size_blocks))
            .collect();
        let input = CpInput {
            cp_seq: job.cp_seq,
            cp_nonce: job.nonce,
            deltas,
            fold,
            new_volumes: new_volumes.clone(),
            max_region_pages: self.layout.blocks_per_segment(),
        };
        // Only the boot and shutdown commits may take the reserve: a fold or
        // a snapshot that would breach it fails for space instead, so the
        // pool can always boot and stop cleanly.
        let reserve = match job.trigger {
            CpTrigger::Boot => 0,
            CpTrigger::Shutdown => 1,
            _ => META_RESERVE_SEGMENTS,
        };
        let mut stream =
            MetaStream::new(&self.layout, &self.segments, self.open_meta, cp32, reserve);
        let prepared = {
            let Engine { map, backstore, .. } = self;
            map.prepare_cp(&mut BackstoreReader(backstore), &input, &mut stream)?
        };
        // A page that did not verify when the CP looked up what it
        // replaces: reported once; its entries read `Corrupt` and no fold
        // can rewrite it until the volume is repaired.
        for (vol, page_no) in &prepared.damaged_pages {
            let warning = OperatorWarning::MapPageDamaged {
                vol: *vol,
                page_no: *page_no,
            };
            if !self.warnings.contains(&warning) {
                self.events.push(Event::OperatorWarning(warning.clone()));
                self.warnings.push(warning);
            }
        }
        // Snapshots asked for: pinned at this fold.
        let mut new_snapshots = Vec::new();
        let mut records: Vec<SnapRecord> = self.snapshots.iter().map(|s| s.record).collect();
        if fold {
            let wall = self.clock.wall_ns();
            for (i, req) in job.snaps.iter().enumerate() {
                let Some((_, root, index)) =
                    prepared.fold_roots.iter().find(|(v, _, _)| *v == req.vol)
                else {
                    continue;
                };
                let snap_id = self.next_snap_id + i as u64;
                let page =
                    SnapRootPage::new(self.cfg.agg_id, snap_id, job.cp_seq, *root, RootRef::NONE)?;
                let page_ref = stream.place_region(page.as_bytes())?;
                let mut record = SnapRecord {
                    snap_id,
                    snap_uuid: req.uuid,
                    map_root_pba: page_ref.pba,
                    deadlist_pba: 0,
                    created_wall_ns: wall,
                    birth_cp: cp32,
                    vol: req.vol,
                    kind: req.kind.as_u8(),
                    origin: req.origin.as_u8(),
                    flags: 0,
                    rsvd: 0,
                    crc32c: 0,
                };
                record.seal();
                records.push(record);
                new_snapshots.push(SnapState {
                    record,
                    pinned: Some(PinnedRoot::from_parts(req.vol, *root, index.clone())),
                    damage: None,
                });
            }
        }
        let snap_table = if records.is_empty() {
            RootRef::NONE
        } else {
            stream.place_region(&snap_table_image(&records)?)?
        };
        // The roots table and the directory, then the segment table, whose
        // live bytes include its own region.
        let map_roots = stream.place_region(&map_roots_image(&prepared.roots_table)?)?;
        let cp_log_head = stream.place_region(&prepared.dir.encode(self.cfg.agg_id)?)?;
        let table_pages = (stream.segments().image().len() / BLOCK_SIZE) as u32;
        let table_pba = stream.reserve(table_pages)?;
        // Data liveness after this CP: every replaced entry, the old block
        // staying live when a snapshot pins it (its birth at or before the
        // snapshot's CP).
        let mut data_live = self.data_live.clone();
        for Replaced { vol, old, new, .. } in &prepared.replaced {
            if new.is_data() {
                *data_live.entry(new.lseg()).or_insert(0) += 1;
            }
            if old.is_data() && !self.pinned_by_snapshot(*vol, old.birth_cp) {
                if let Some(n) = data_live.get_mut(&old.lseg()) {
                    *n = n.saturating_sub(1);
                }
            }
        }
        // Metadata liveness after this CP: what was live plus every region
        // placed (frees are deferred).
        let mut meta_live = self.meta_live.clone();
        for (pseg, bytes) in stream.bytes_per_segment() {
            *meta_live.entry(pseg).or_insert(0) += bytes;
        }
        for (pseg, bytes) in self.layout.segments_of_region(table_pba, table_pages) {
            *meta_live.entry(pseg).or_insert(0) += bytes;
        }
        {
            let table = stream.segments_mut();
            for pseg in table.used_segments() {
                let kind = table.entry(pseg)?.seg_kind();
                let bytes = match kind {
                    Some(SegKind::Data) => {
                        data_live.get(&pseg).copied().unwrap_or(0) * BLOCK_SIZE as u64
                    }
                    Some(SegKind::Metadata) => meta_live.get(&pseg).copied().unwrap_or(0),
                    None => 0,
                };
                table.set_live_bytes(pseg, u32::try_from(bytes).unwrap_or(u32::MAX))?;
            }
        }
        let seg_table = stream.place_at(table_pba, &stream.segments().image())?;
        // What this CP stops referencing: the previous tables, the deltas a
        // fold drops, and the pages and indexes a fold replaced unless a
        // snapshot still pins them.
        let mut freed = self.table_regions.clone();
        freed.extend_from_slice(&prepared.freed_deltas);
        for fp in &prepared.freed_pages {
            let pinned = self.snapshots.iter().any(|s| {
                s.pinned
                    .as_ref()
                    .is_some_and(|p| p.vol == fp.vol && p.references_page(fp.page_no, fp.root.pba))
            });
            if !pinned {
                freed.push(fp.root);
            }
        }
        for fi in &prepared.freed_indexes {
            let pinned = self.snapshots.iter().any(|s| {
                s.pinned
                    .as_ref()
                    .is_some_and(|p| p.references_index(fi.root.pba))
            });
            if !pinned {
                freed.push(fi.root);
            }
        }
        let table_regions = vec![map_roots, cp_log_head, snap_table, seg_table]
            .into_iter()
            .filter(|r| !r.is_none())
            .collect();
        let (stripes, segments, open_meta) = stream.finish();
        // The volumes this CP's tables carry: those with a root.
        let carried: Vec<u32> = prepared
            .roots_table
            .iter()
            .enumerate()
            .filter(|(_, r)| !r.is_absent())
            .map(|(i, _)| i as u32)
            .collect();
        for (vol, _) in &new_volumes {
            if let Some(v) = self.vols.get_mut(*vol as usize) {
                v.created_in = Some(job.cp_seq);
            }
        }
        let fold_pages = prepared.dir.pages.len() as u64;
        let prep = PreparedParts {
            map_update: prepared.update,
            segments,
            open_meta,
            meta_live,
            data_live,
            freed,
            table_regions,
            map_roots,
            seg_table,
            cp_log_head,
            snap_table,
            new_snapshots,
            carried,
            fold,
            fold_pages,
        };
        Ok((prep, stripes.into_iter().collect()))
    }

    /// `true` when a snapshot of `vol` was taken at or after the CP that
    /// wrote an entry: the snapshot's root still names that entry.
    fn pinned_by_snapshot(&self, vol: u32, birth_cp: u32) -> bool {
        self.snapshots
            .iter()
            .any(|s| s.record.vol == vol && s.record.birth_cp >= birth_cp)
    }

    fn step_meta(
        &mut self,
        mut mw: Box<MetaWrite>,
        dones: Vec<Done>,
    ) -> Result<State, EngineError> {
        for done in dones {
            if mw.inflight.remove(&done.tag) {
                if let Err(e) = done.result {
                    mw.failed = Some(format!("metadata write: {e}"));
                }
            }
        }
        if let Some(reason) = mw.failed.take() {
            if mw.inflight.is_empty() {
                return Ok(self.fail_cp(&mw.prep.job, reason, false));
            }
            mw.failed = Some(reason);
            return Ok(State::Meta(mw));
        }
        let max_inflight = usize::from(self.backstore.profile().inflight_stripes.clamp(1, 64));
        while mw.inflight.len() < max_inflight {
            let Some((pba, image)) = mw.stripes.pop_front() else {
                break;
            };
            let tag = self.tag();
            match self.backstore.write_meta(pba, &image, tag) {
                Ok(()) => {
                    mw.inflight.insert(tag);
                }
                Err(Busy) => {
                    mw.stripes.push_front((pba, image));
                    break;
                }
            }
        }
        if mw.stripes.is_empty() && mw.inflight.is_empty() {
            let mw = *mw;
            return self.step_flush(Box::new(mw.prep), None, Vec::new());
        }
        Ok(State::Meta(mw))
    }

    fn step_flush(
        &mut self,
        prep: Box<Prepared>,
        tag: Option<Tag>,
        dones: Vec<Done>,
    ) -> Result<State, EngineError> {
        match tag {
            None => {
                let tag = self.tag();
                match self.backstore.flush(DevClass::Pool, tag) {
                    Ok(()) => Ok(State::Flush {
                        prep,
                        tag: Some(tag),
                    }),
                    Err(Busy) => Ok(State::Flush { prep, tag: None }),
                }
            }
            Some(tag) => {
                let Some(done) = dones.into_iter().find(|d| d.tag == tag) else {
                    return Ok(State::Flush {
                        prep,
                        tag: Some(tag),
                    });
                };
                match done.result {
                    Ok(()) => self.step_commit(prep, None, Vec::new(), SbLocation::A),
                    Err(e) => Ok(self.fail_cp(&prep.job, format!("flush: {e}"), false)),
                }
            }
        }
    }

    fn step_commit(
        &mut self,
        prep: Box<Prepared>,
        tag: Option<Tag>,
        dones: Vec<Done>,
        location: SbLocation,
    ) -> Result<State, EngineError> {
        let wrap = |prep, tag| match location {
            SbLocation::A => State::CommitA { prep, tag },
            SbLocation::B => State::CommitB { prep, tag },
        };
        match tag {
            None => {
                let mut prep = prep;
                let slot = match (location, prep.slot) {
                    (SbLocation::A, _) => {
                        let built = self.build_slot(&prep)?;
                        prep.slot = Some(built);
                        built
                    }
                    (SbLocation::B, Some(a)) => {
                        let mut b = a;
                        b.sb.slot = Superblock::slot_for(prep.job.cp_seq, SbLocation::B).index();
                        b.seal()?;
                        b
                    }
                    (SbLocation::B, None) => {
                        return Err(EngineError::Internal(
                            "the B slot is written before the A slot was built".into(),
                        ))
                    }
                };
                let sb_slot = slot.sb.sb_slot().unwrap_or(SbSlot::A0);
                let pba = self.layout.sb_pba(sb_slot);
                let tag = self.tag();
                match self.backstore.commit(pba, &slot, tag) {
                    Ok(()) => Ok(wrap(prep, Some(tag))),
                    Err(Busy) => Ok(wrap(prep, None)),
                }
            }
            Some(tag) => {
                let Some(done) = dones.into_iter().find(|d| d.tag == tag) else {
                    return Ok(wrap(prep, Some(tag)));
                };
                match (done.result, location) {
                    (Err(e), _) => Ok(self.fail_cp(&prep.job, format!("commit: {e}"), false)),
                    (Ok(()), SbLocation::A) => {
                        // The newest superblock on disk now carries this set:
                        // the disk guarantees it (plus every intent that
                        // landed since the slot was built).
                        if let Some(slot) = &prep.slot {
                            self.durable_active.union_with(&slot.sb.write_active);
                        }
                        self.step_commit(prep, None, Vec::new(), SbLocation::B)
                    }
                    (Ok(()), SbLocation::B) => {
                        self.finalize(*prep);
                        Ok(State::Idle)
                    }
                }
            }
        }
    }

    fn build_slot(&mut self, prep: &Prepared) -> Result<SuperblockSlot, EngineError> {
        let job = &prep.job;
        let mut slot = SuperblockSlot::new(self.cfg.agg_id, job.cp_seq, SbLocation::A);
        let sb = &mut slot.sb;
        sb.set_geometry(
            self.layout.stripe_bytes,
            self.layout.segment_bytes as u32,
            self.layout.capacity_bytes,
        );
        sb.owner_epoch = self.cfg.owner_epoch;
        sb.cut_seq = job.gen.as_ref().map_or(self.cut_seq, |g| g.id());
        sb.cp_nonce = job.nonce;
        // The highest intent whose completion the engine has seen: an
        // intent still in flight has a higher sequence and is honoured by
        // the boot whether or not this slot lands.
        sb.intent_seq_seen = self.intent_seq_landed;
        sb.opened_stamp = self.boot_count;
        sb.wall_ns = self.clock.wall_ns();
        sb.set_run_state(job.run_state);
        sb.seg_table = prep.seg_table;
        sb.cp_log_head = prep.cp_log_head;
        sb.map_roots = prep.map_roots;
        sb.snap_table = prep.snap_table;
        if let Some(open) = self.open_data {
            sb.open_data_pseg = open.pseg;
            sb.open_data_idx = open.next_idx;
        }
        // The rows: the volumes whose root this CP's tables carry.
        let vol_count = prep.carried.iter().max().map_or(0, |v| v + 1);
        sb.vol_count = vol_count;
        let precise = self.backstore.profile().nvfail_intent == NvfailIntent::Precise;
        let committed_gen = job.gen.as_ref().map(|g| g.id());
        // The windows as they will stand once this CP is committed: a
        // volume with no write in the committed generation loses one, so a
        // volume idle for two CPs leaves the set at the second (section 7.6).
        let dirty_now = job.gen.as_ref().map(|g| g.dirty_volumes());
        let mut write_active = VolBitSet::EMPTY;
        let mut nvfail = VolBitSet::EMPTY;
        for vol in 0..vol_count {
            let v = self.vols[vol as usize];
            let row = &mut slot.vols.vols[vol as usize];
            if !v.present || !prep.carried.contains(&vol) {
                *row = VolState::ABSENT;
                continue;
            }
            row.flags = VOL_PRESENT;
            row.acked_bytes = self.unflushed_bytes_after(vol, committed_gen);
            match v.clearing {
                Some(c) => {
                    // The clear rides this CP: the row shows the volume
                    // cleared on its new generation.
                    row.rollback_gen = c.gen;
                    row.nvfail_cause = 0;
                }
                None => {
                    row.rollback_gen = v.rollback_gen;
                    row.nvfail_cause = v.nvfail.map_or(0, NvCause::as_u8);
                    if v.nvfail.is_some() {
                        nvfail.set(vol)?;
                        continue;
                    }
                }
            }
            let windows_after = match &dirty_now {
                Some(dirty) if dirty.contains(&vol) => 2,
                Some(_) => v.windows.saturating_sub(1),
                None => v.windows,
            };
            let active = if precise { windows_after > 0 } else { true };
            if active {
                write_active.set(vol)?;
            }
        }
        slot.sb.write_active = write_active;
        slot.sb.nvfail = nvfail;
        slot.seal()?;
        // Clears this slot carries become durable with it.
        for v in &mut self.vols {
            if let Some(c) = &mut v.clearing {
                if c.in_cp.is_none() {
                    c.in_cp = Some(job.cp_seq);
                }
            }
        }
        // From this moment the disk may carry this set: a write is
        // acknowledged without an intent only to a volume in the durable
        // set and in this one, whether or not this commit lands. Building a
        // slot only ever narrows; a landed intent or this slot's own landing
        // widens.
        self.durable_active.intersect_with(&write_active);
        Ok(slot)
    }

    /// Bytes acknowledged for `vol` and not carried by the CP committing
    /// generation `committed` (or by any CP, when `None`).
    fn unflushed_bytes_after(&self, vol: u32, committed: Option<GenId>) -> u64 {
        let mut blocks = self
            .index
            .with_active(|g| g.with_volume(vol, |d| d.blocks()).unwrap_or(0));
        for gen in self.index.frozen() {
            if committed.is_none_or(|c| gen.id() > c) {
                blocks += gen.with_volume(vol, |d| d.blocks()).unwrap_or(0);
            }
        }
        blocks * BLOCK_SIZE as u64
    }

    /// The CP is durable: the map, the arena, the index, the volumes, the
    /// snapshots, the segment bookkeeping and the acknowledgements catch
    /// up. Nothing here can refuse; an inconsistency is recorded as
    /// [`Event::Internal`].
    fn finalize(&mut self, prep: Prepared) {
        let Prepared {
            job,
            map_update,
            segments,
            open_meta,
            mut meta_live,
            mut data_live,
            freed,
            table_regions,
            new_snapshots,
            fold,
            fold_pages,
            ..
        } = prep;
        let gen = job.gen.clone();
        self.map.apply(map_update);
        self.segments = segments;
        for pseg in std::mem::take(&mut self.freed_during_cp) {
            // Freed while this CP was in flight, on the table it copied.
            if let Err(e) = self.segments.free(pseg) {
                self.events.push(Event::Internal {
                    what: format!("re-applying the free of segment {pseg}: {e}"),
                });
            }
            meta_live.remove(&pseg);
            data_live.remove(&pseg);
        }
        self.open_meta = open_meta;
        self.meta_live = meta_live;
        self.data_live = data_live;
        self.table_regions = table_regions;
        self.cp_seq = job.cp_seq;
        self.cp_nonce = job.nonce;
        self.note_dead_data(job.cp_seq);
        // The arena slots the generation held: data runs and hole records.
        for blk in &job.blocks {
            if let Err(e) = self.arena.commit(blk.ring, 1) {
                self.events.push(Event::Internal {
                    what: format!(
                        "releasing slot {} after CP {}: {e}",
                        blk.ring.slot, job.cp_seq
                    ),
                });
            }
        }
        if let Some(gen) = &gen {
            for ring in self.gen_holes.remove(&gen.id()).unwrap_or_default() {
                if let Err(e) = self.arena.commit(ring, 1) {
                    self.events.push(Event::Internal {
                        what: format!(
                            "releasing hole record {} after CP {}: {e}",
                            ring.slot, job.cp_seq
                        ),
                    });
                }
            }
            if let Err(e) = self.index.retire(gen.id()) {
                self.events.push(Event::Internal {
                    what: format!("retiring generation {}: {e}", gen.id()),
                });
            }
            self.pending_triggers.retain(|(id, _)| *id != gen.id());
            // A volume the CP carried starts its two windows now; one it
            // did not carry loses one (section 7.6).
            let dirty = gen.dirty_volumes();
            for (vol, v) in self.vols.iter_mut().enumerate() {
                if dirty.contains(&(vol as u32)) {
                    v.windows = 2;
                } else {
                    v.windows = v.windows.saturating_sub(1);
                }
            }
        }
        // Writes waiting for an intent are durable once the CP carried
        // them: sequence numbers are in placement order, so everything at
        // or below the CP's highest one is on disk (or overwritten there).
        if job.max_seq > 0 {
            let (ready, waiting): (Vec<_>, Vec<_>) = std::mem::take(&mut self.pending_acks)
                .into_iter()
                .partition(|(_, seq)| *seq <= job.max_seq);
            self.pending_acks = waiting;
            for (req, seq) in ready {
                self.front.complete(req, Status::Ok, seq);
            }
        }
        for (vol, v) in self.vols.iter_mut().enumerate() {
            if v.creating && v.created_in == Some(job.cp_seq) {
                v.creating = false;
                self.events.push(Event::VolumeCreated {
                    vol: vol as u32,
                    cp_seq: job.cp_seq,
                });
            }
            if let Some(c) = v.clearing {
                if c.in_cp == Some(job.cp_seq) {
                    v.nvfail = None;
                    v.rollback_gen = c.gen;
                    v.clearing = None;
                    self.events.push(Event::NvfailCleared {
                        vol: vol as u32,
                        rollback_gen: c.gen,
                    });
                }
            }
        }
        for snap in new_snapshots {
            self.events.push(Event::SnapshotCreated {
                snap_id: snap.record.snap_id,
                vol: snap.record.vol,
                cp_seq: job.cp_seq,
            });
            self.next_snap_id = self.next_snap_id.max(snap.record.snap_id + 1);
            self.snapshots.push(snap);
            self.stats.snapshots += 1;
        }
        if fold {
            // The requests this CP took went with it (`job.snaps`,
            // `job.fold_requested`); ones that arrived meanwhile stay for
            // the next fold.
            self.fold_blocked_at = None;
            self.last_fold_ns = self.clock.mono_ns();
            self.stats.folds += 1;
            self.events.push(Event::Folded {
                cp_seq: job.cp_seq,
                pages: fold_pages,
            });
        }
        self.pool_full = false;
        self.freed_pending.push_back((job.cp_seq, freed));
        self.release_freed(job.cp_seq);
        self.stats.cps += 1;
        self.last_commit_ns = self.clock.mono_ns();
        self.events.push(Event::CpCommitted {
            cp_seq: job.cp_seq,
            trigger: job.trigger,
            gen: gen.as_ref().map(|g| g.id()),
            blocks: job.blocks.len() as u64,
            max_seq: job.max_seq,
        });
        match job.trigger {
            CpTrigger::Boot => {
                if !self.shutdown_requested {
                    self.serving = true;
                }
            }
            CpTrigger::Shutdown => {
                self.stopped = true;
                self.serving = false;
                self.fail_pending_snapshots("the engine stopped");
                self.events.push(Event::Stopped {
                    cp_seq: job.cp_seq,
                    clean: true,
                });
            }
            _ => {}
        }
    }

    /// Frees the regions a committed CP stopped referencing once no reachable
    /// consistency point can name them: a boot reaches the newest committed
    /// CP or the one before it, so a region CP `N` stopped referencing is
    /// dead once `N + 1` committed (one CP later still on a flush-opaque
    /// backstore, C07). A metadata segment with nothing live left, not open
    /// and not protected since the boot, is freed and, when the policy says
    /// so, discarded (D9).
    fn release_freed(&mut self, cp_seq: u64) {
        let keep = if self.backstore.profile().trusts_opaque_barrier() {
            2
        } else {
            1
        };
        self.boot_protect.retain(|(cp, _)| cp + keep + 1 > cp_seq);
        // Data segments dead long enough that no rewind reaches a CP that
        // named their blocks.
        let mut dead_now = Vec::new();
        while let Some((cp, _)) = self.dead_data_pending.front() {
            if cp + keep > cp_seq {
                break;
            }
            let (_, segs) = self.dead_data_pending.pop_front().expect("front exists");
            dead_now.extend(segs);
        }
        while let Some((cp, _)) = self.freed_pending.front() {
            if cp + keep > cp_seq {
                break;
            }
            let (_, regions) = self.freed_pending.pop_front().expect("front exists");
            for region in regions {
                for (pseg, bytes) in self.layout.segments_of_region(region.pba, region.pages) {
                    match self.meta_live.get_mut(&pseg) {
                        Some(live) if *live >= bytes => *live -= bytes,
                        _ => self.events.push(Event::Internal {
                            what: format!("metadata liveness of segment {pseg} went negative"),
                        }),
                    }
                }
            }
        }
        let protected: HashSet<u64> = self
            .boot_protect
            .iter()
            .flat_map(|(_, segs)| segs.iter().copied())
            .collect();
        let open_meta = self.open_meta.map(|o| o.pseg);
        let open_data = self.open_data.map(|o| o.pseg);
        let discard = self.backstore.profile().discard == DiscardPolicy::WholeSegment;
        let mut to_free = Vec::new();
        for pseg in self.segments.used_segments() {
            let kind = self.segments.entry(pseg).ok().and_then(|e| e.seg_kind());
            if Some(pseg) == open_meta
                || Some(pseg) == open_data
                || protected.contains(&pseg)
                || self.discard_pending(pseg)
            {
                continue;
            }
            let dead = match kind {
                Some(SegKind::Metadata) => self.meta_live.get(&pseg).copied().unwrap_or(0) == 0,
                // A data segment is freed only after its deferral.
                Some(SegKind::Data) => {
                    dead_now.contains(&pseg) && self.data_live.get(&pseg).copied().unwrap_or(0) == 0
                }
                _ => false,
            };
            if dead {
                to_free.push(pseg);
            }
        }
        for pseg in to_free {
            if discard {
                // Freed once the discard completes: a discard still in
                // flight must never race a write into a reused segment.
                self.discard(pseg);
            } else {
                self.free_segment(pseg, false);
            }
        }
    }

    /// Queues the data segments that CP `cp_seq` left without a live block
    /// for freeing after the deferral.
    fn note_dead_data(&mut self, cp_seq: u64) {
        let open_data = self.open_data.map(|o| o.pseg);
        let queued: HashSet<u64> = self
            .dead_data_pending
            .iter()
            .flat_map(|(_, segs)| segs.iter().copied())
            .collect();
        let dead: Vec<u64> = self
            .segments
            .used_segments()
            .into_iter()
            .filter(|pseg| {
                Some(*pseg) != open_data
                    && !queued.contains(pseg)
                    && !self.discard_pending(*pseg)
                    && self.segments.entry(*pseg).is_ok_and(|e| {
                        e.seg_kind() == Some(SegKind::Data)
                            && e.seg_state() == Some(SegState::Sealed)
                    })
                    && self.data_live.get(pseg).copied().unwrap_or(0) == 0
            })
            .collect();
        if !dead.is_empty() {
            self.dead_data_pending.push_back((cp_seq, dead));
        }
    }

    fn tag(&mut self) -> Tag {
        let tag = Tag(self.next_tag);
        self.next_tag += 1;
        tag
    }

    fn new_nonce(&mut self, cp_seq: u64) -> u64 {
        self.nonce_counter += 1;
        nonce_for(
            self.cfg.agg_id,
            cp_seq,
            self.clock.wall_ns(),
            self.clock.mono_ns(),
            self.nonce_counter ^ (self.boot_count << 32),
        )
    }
}

/// A CP nonce: unique per attempt, derived from the identity, the clocks and
/// a counter, never zero.
fn nonce_for(agg_id: Uuid, cp_seq: u64, wall_ns: u64, mono_ns: u64, counter: u64) -> u64 {
    let mut hasher = blake3::Hasher::new();
    hasher.update(agg_id.as_array());
    hasher.update(&cp_seq.to_le_bytes());
    hasher.update(&wall_ns.to_le_bytes());
    hasher.update(&mono_ns.to_le_bytes());
    hasher.update(&counter.to_le_bytes());
    let hash = hasher.finalize();
    let mut bytes = [0u8; 8];
    bytes.copy_from_slice(&hash.as_bytes()[..8]);
    u64::from_le_bytes(bytes) | 1
}

/// Groups placed slots (sorted by `(seq, position)`) into extents: same
/// write, same volume, consecutive LBAs, consecutive slots on one lap; a
/// hole record is an extent of its own.
fn runs_of(placed: &[PlacedSlot]) -> Vec<(u32, u64, DirtyExtent)> {
    let mut out: Vec<(u32, u64, DirtyExtent)> = Vec::new();
    for p in placed {
        if p.is_hole() {
            out.push((
                p.vol,
                p.lba,
                DirtyExtent::hole(p.ring, p.hole_blocks, p.seq),
            ));
            continue;
        }
        if let Some((vol, lba, ext)) = out.last_mut() {
            let continues = !ext.hole
                && *vol == p.vol
                && ext.seq == p.seq
                && *lba + u64::from(ext.blocks) == p.lba
                && ext.ring.lap == p.ring.lap
                && ext.ring.slot + ext.blocks == p.ring.slot;
            if continues {
                ext.blocks += 1;
                continue;
            }
        }
        out.push((p.vol, p.lba, DirtyExtent::data(p.ring, 1, p.seq)));
    }
    out
}
