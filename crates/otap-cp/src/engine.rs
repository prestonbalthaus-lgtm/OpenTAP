//! The engine: write and read paths, the consistency-point state machine,
//! boot and shutdown.

use core::fmt;
use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};
use std::sync::Arc;

use otap_arena::{Arena, ArenaError, ArenaSurvey, Fills, PlacedSlot, RingRef};
use otap_format::checksum::crc32c;
use otap_format::{
    boot_write_active, cp_stripes_image, map_roots_image, table_delta_image, validate_name,
    CatalogKind, CatalogRec, CpStripe, FormatError, IntentSlot, IntoBytes, MapEntry, NvCause,
    PageIndexEntry, RootRef, RunState, SbLocation, SbSlot, SegKind, SegState, SnapKind, SnapOrigin,
    SnapRecord, SnapRootPage, StripeDescriptor, StripeHdr, StripeKind, Superblock, SuperblockSlot,
    TableChainEntry, TableHead, TableKind, TableRecord, Uuid, VolBitSet, VolState, BLOCK_SIZE,
    MAX_CP_SEQ, MAX_LBA, MAX_VOLS, PIN_MASK, SECTORS_PER_BLOCK, SNAP_UNCERTAIN, VOL_OFFLINE,
    VOL_PRESENT,
};
use otap_index::{DirtyExtent, GenId, Generation, Index, IndexError};
use otap_map::{
    CpInput, FreedIndex, FreedPage, MapError, MapSet, MapUpdate, PageReader, PinnedRoot, Replaced,
    Rollback,
};
use otap_sys::HugeArena;

use crate::boot::{self, BootError, BootSnapshot};
use crate::cleaner::{self, CleanTarget, CleanerPolicy, SkipReason};
use crate::layout::{DiskLayout, LayoutError};
use crate::meta::{MetaError, MetaStream};
use crate::segments::{OpenSegment, SegmentError, SegmentTable};
use crate::tables::{plan_table, TableChain, TableFoldPolicy, TablePlan, TablePlanInput};
use crate::traits::{
    Backstore, Busy, Clock, DevClass, DiscardPolicy, Done, FlushPolicy, FrontDoor, IoError,
    NvfailIntent, Request, RequestKind, Status, Tag,
};
use crate::verify::{
    LiveVolume, SnapshotView, Verdict, VerifyJob, VerifyOptions, VerifyReport, VerifyState, View,
};

mod replica;

pub use replica::{BlockInfo, DeltaItem, DeltaKind, DeltaScan};

/// The longest read or write the engine serves, in blocks (8 MiB): bounds
/// the read buffer and the arena run one request can take.
pub const MAX_REQUEST_BLOCKS: u32 = 2048;

/// The longest discard or write of zeros the engine serves, in blocks
/// (1 GiB): one hole record in the arena, one CP log record per block.
pub const MAX_PUNCH_BLOCKS: u32 = 1 << 18;

/// Segments every allocation leaves free so a boot or a shutdown commit
/// always finds room for its metadata: the hard floor under the reserve
/// of [`reserve_for`].
pub const META_RESERVE_SEGMENTS: u32 = 2;

/// User and scheduled snapshots a volume may hold at once (S5-2).
pub const MAX_USER_SNAPSHOTS: usize = 255;

/// Replication baselines and divergent heads a volume may hold at once,
/// beyond the user snapshots: the reserved slots of S5-2.
pub const MAX_RESERVED_SNAPSHOTS: usize = 16;

/// The free segments front-door writes leave alone (S5-3): the larger of
/// 8 and a twelfth of the pool, and never more than half of it, reserved
/// for the cleaner's relocation stripes and the metadata of folds, so a
/// pool that is full for users can still commit the consistency points
/// that free space.
#[must_use]
pub const fn reserve_for(segments: u32) -> u32 {
    let twelfth = segments / 12;
    let reserve = if twelfth > 8 { twelfth } else { 8 };
    let half = segments / 2;
    if reserve > half {
        half
    } else {
        reserve
    }
}

/// How writes are paced by arena fullness (S5-1): above `pace_at` a write
/// is placed at once but acknowledged after a delay that grows
/// quadratically to `max_delay_ns` at `stall_at`; at or above `stall_at`
/// writes are held unplaced until a consistency point drains the arena.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Pacing {
    /// Occupancy at which acknowledgements start to be delayed, 0.80.
    pub pace_at: f64,
    /// Occupancy at which writes stall, 0.95.
    pub stall_at: f64,
    /// The delay at `stall_at`; 0 disables the delay (the stall stays).
    pub max_delay_ns: u64,
}

impl Default for Pacing {
    fn default() -> Self {
        Self {
            pace_at: 0.80,
            stall_at: 0.95,
            max_delay_ns: 50_000_000,
        }
    }
}

impl Pacing {
    /// The acknowledgement delay at `occupancy`, or 0 below `pace_at`.
    #[must_use]
    pub fn delay_ns(&self, occupancy: f64) -> u64 {
        if self.max_delay_ns == 0 || occupancy < self.pace_at || self.stall_at <= self.pace_at {
            return 0;
        }
        let x = ((occupancy - self.pace_at) / (self.stall_at - self.pace_at)).clamp(0.0, 1.0);
        (self.max_delay_ns as f64 * x * x) as u64
    }
}

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
    /// When the snapshot table and the catalogue fold (F20).
    pub table_fold: TableFoldPolicy,
    /// Map pages the engine caches.
    pub cache_pages: usize,
    /// Free segments front-door writes leave alone; [`reserve_for`] the
    /// pool when `None` (the default).
    pub reserve_segments: Option<u32>,
    /// When and how much the cleaner runs.
    pub cleaner: CleanerPolicy,
    /// How writes are paced by arena fullness.
    pub pacing: Pacing,
    /// Recreate an arena of this pool that was made for another size even
    /// when it may hold acknowledged writes or the pool did not shut down
    /// cleanly ([`check_arena_resize`]): the operator's explicit choice to
    /// lose them, which a dirty boot answers by holding the write-active
    /// volumes. `false` by default: such a boot is refused
    /// ([`EngineError::ArenaResize`]).
    pub discard_arena: bool,
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
            table_fold: TableFoldPolicy::default(),
            cache_pages: boot::CACHE_PAGES,
            reserve_segments: None,
            cleaner: CleanerPolicy::default(),
            pacing: Pacing::default(),
            discard_arena: false,
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

/// The rule for this pool's arena found made for another size (a changed
/// arena size over an arena file, Step 6A): it may be recreated at the new
/// size only when nothing in it can be an acknowledged write the pool
/// lacks, that is when `survey` read it under its recorded geometry and
/// found no placed slot, and the pool's selected CP is a clean shutdown
/// with nothing rewound (`clean`). `discard` is the operator's explicit
/// choice to recreate it regardless. The boot applies it, and so does a
/// daemon before it resizes an arena file, which cannot be undone.
///
/// # Errors
/// [`EngineError::ArenaResize`] naming what may be lost.
pub fn check_arena_resize(
    survey: Option<&ArenaSurvey>,
    clean: bool,
    offered_bytes: u64,
    discard: bool,
) -> Result<(), EngineError> {
    if discard || (clean && survey.is_some_and(ArenaSurvey::holds_nothing)) {
        return Ok(());
    }
    Err(EngineError::ArenaResize {
        recorded_bytes: survey.map(|s| s.recorded_bytes),
        offered_bytes,
        placed: survey.map_or(0, |s| s.placed),
        clean,
    })
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
    /// A consistency point run for the cleaner or to advance the deferred
    /// frees of a full pool, carrying no generation.
    Clean,
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
    /// A snapshot's root page or pinned index did not verify at boot, or a
    /// walk found one of its map pages damaged since (Step 6A: it is
    /// quarantined): it is listed, cannot be read, diffed or rolled back
    /// to, still pins its data, and can be deleted; the pool's consistency
    /// points go on. The warning leaves with the snapshot.
    SnapshotDamaged {
        /// The snapshot.
        snap_id: u64,
        /// Its volume.
        vol: u32,
        /// What failed.
        reason: String,
    },
    /// The name catalogue (F16) did not verify whole at boot: the pool
    /// booted with the names it could vouch for (none, when the head page
    /// was lost), and the first consistency point folds the catalogue,
    /// which repairs it. A nameless volume is addressed by its index until
    /// it is named again; no volume is ever dropped for a name.
    CatalogDamaged {
        /// What failed.
        reason: String,
    },
    /// The snapshot table's chain (format 2.4, F21) did not verify whole
    /// at boot: a snapshot whose last change may have been lost is kept
    /// quarantined (`SnapshotDamaged`, its deletion releases nothing), one
    /// whose record was lost is gone with its blocks still counted, and the
    /// first consistency point folds the table, which repairs it.
    SnapshotTableDamaged {
        /// What failed.
        reason: String,
    },
    /// The last pool verification (`otapctl pool verify`) found something
    /// wrong in the pool as committed at `cp_seq`; its report lists what
    /// and where.
    AttestationFindings {
        /// The CP the verification attested.
        cp_seq: u64,
        /// Findings that mean damage or data at risk.
        errors: u64,
        /// Findings that mean a leak or a quarantine already in force.
        warnings: u64,
        /// The first finding, for the status line.
        first: String,
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
            Self::CatalogDamaged { reason } => write!(
                f,
                "the name catalogue does not verify ({reason}): the names it could not vouch for \
                 were dropped; address such a volume as #<index> and name it again"
            ),
            Self::SnapshotTableDamaged { reason } => write!(
                f,
                "the snapshot table does not verify ({reason}): snapshots whose last change may \
                 be lost are quarantined, lost records leave their blocks counted"
            ),
            Self::AttestationFindings {
                cp_seq,
                errors,
                warnings,
                first,
            } => write!(
                f,
                "the verification of cp {cp_seq} found {errors} error(s) and {warnings} \
                 warning(s); first: {first}"
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
    /// The cleaner drained a data segment: every block still referenced
    /// was relocated by the CP, and the segment is freed after the
    /// deferral.
    SegmentCleaned {
        /// The segment.
        pseg: u64,
        /// Blocks relocated.
        relocated: u64,
        /// The CP that carried the relocation.
        cp_seq: u64,
    },
    /// The cleaner left a candidate alone.
    CleanSkipped {
        /// The segment.
        pseg: u64,
        /// Why.
        reason: String,
    },
    /// A snapshot is gone: its record left the table with this CP and the
    /// blocks only it referenced were released.
    SnapshotDeleted {
        /// The snapshot.
        snap_id: u64,
        /// Its volume.
        vol: u32,
        /// The CP.
        cp_seq: u64,
        /// Data blocks released.
        released: u64,
    },
    /// A snapshot's pin flags changed durably.
    SnapshotPinned {
        /// The snapshot.
        snap_id: u64,
        /// The `PIN_*` bits now in force.
        flags: u8,
    },
    /// A volume was rolled back to a snapshot: its content is the
    /// snapshot's, every block written since was released, and its
    /// rollback generation moved on.
    VolumeRolledBack {
        /// The volume.
        vol: u32,
        /// The snapshot.
        snap_id: u64,
        /// The CP.
        cp_seq: u64,
        /// The volume's rollback generation afterwards.
        rollback_gen: u32,
    },
    /// A volume's size changed durably.
    VolumeResized {
        /// The volume.
        vol: u32,
        /// The size in blocks now.
        size_blocks: u64,
        /// The CP.
        cp_seq: u64,
    },
    /// A volume is gone, with everything it mapped.
    VolumeDeleted {
        /// The volume.
        vol: u32,
        /// The CP.
        cp_seq: u64,
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
    /// A pool verification ended ([`Engine::start_verify`]): finished,
    /// cancelled or aborted.
    VerifyFinished {
        /// The CP it attested (0 when it never began).
        cp_seq: u64,
        /// How it ended.
        state: VerifyState,
        /// What it concluded.
        verdict: Verdict,
        /// Findings of error severity.
        errors: u64,
        /// Findings of warning severity.
        warnings: u64,
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
    /// The snapshot is pinned (a replication base, an operator's pin) and
    /// refuses deletion until it is unpinned or superseded (S5-2).
    SnapshotPinned {
        /// The snapshot.
        snap_id: u64,
        /// Its `PIN_*` bits.
        flags: u8,
    },
    /// The volume holds as many snapshots of that class as it may.
    SnapshotLimit {
        /// The volume.
        vol: u32,
        /// The bound.
        limit: usize,
    },
    /// The operation needs the volume offline.
    VolumeOnline {
        /// The volume.
        vol: u32,
    },
    /// The volume still has acknowledged writes in the arena; a CP must
    /// carry them before the operation can go ahead.
    VolumeDirty {
        /// The volume.
        vol: u32,
    },
    /// The volume has snapshots newer than the one named, which a rollback
    /// would orphan: delete them first.
    NewerSnapshots {
        /// The volume.
        vol: u32,
        /// How many.
        count: usize,
    },
    /// The volume has snapshots; a volume with snapshots is not deleted.
    HasSnapshots {
        /// The volume.
        vol: u32,
        /// How many.
        count: usize,
    },
    /// A rollback, resize or deletion of the volume is already waiting for
    /// its CP.
    VolumeBusy {
        /// The volume.
        vol: u32,
    },
    /// The name is already in use in its scope.
    NameExists {
        /// The name.
        name: String,
    },
    /// No volume or snapshot of that name.
    NoSuchName {
        /// The name.
        name: String,
    },
    /// The engine is not serving (booting or stopped).
    NotServing,
    /// The memory holds this pool's arena made for another size (a changed
    /// arena size over an arena file), and recreating it at the new size
    /// could destroy acknowledged writes: it may hold placed writes, or the
    /// pool did not shut down cleanly (Step 6A; [`check_arena_resize`]).
    ArenaResize {
        /// The length the arena was made for, when its geometry could be
        /// read.
        recorded_bytes: Option<u64>,
        /// The length of the memory offered.
        offered_bytes: u64,
        /// Its slots that may hold acknowledged writes (0 when unread).
        placed: u32,
        /// The pool's selected CP is a clean shutdown and nothing rewound.
        clean: bool,
    },
    /// The volume is not a replication destination (Step 6C): a
    /// replication write was offered for a volume no sink marked.
    NotReplica {
        /// The volume.
        vol: u32,
    },
    /// The volume is a replication destination (a mirror, Step 6C): it
    /// stays offline to clients until the replication relationship is
    /// released (`otapctl replicate release`).
    ReplicaVolume {
        /// The volume.
        vol: u32,
    },
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
            Self::SnapshotPinned { snap_id, flags } => write!(
                f,
                "snapshot {snap_id} is pinned (flags {flags:#04x}); unpin it or supersede the \
                 replication base before deleting it"
            ),
            Self::SnapshotLimit { vol, limit } => {
                write!(
                    f,
                    "volume {vol} already holds {limit} snapshots of that class"
                )
            }
            Self::VolumeOnline { vol } => {
                write!(f, "volume {vol} must be offline for this operation")
            }
            Self::VolumeDirty { vol } => write!(
                f,
                "volume {vol} has acknowledged writes not yet in a consistency point"
            ),
            Self::NewerSnapshots { vol, count } => write!(
                f,
                "volume {vol} has {count} newer snapshot(s); delete them before rolling back"
            ),
            Self::HasSnapshots { vol, count } => write!(
                f,
                "volume {vol} has {count} snapshot(s); delete them before deleting the volume"
            ),
            Self::VolumeBusy { vol } => write!(
                f,
                "volume {vol} already has a rollback, resize or deletion waiting"
            ),
            Self::NameExists { name } => write!(f, "the name {name:?} is already in use"),
            Self::NoSuchName { name } => write!(f, "no volume or snapshot named {name:?}"),
            Self::NotServing => f.write_str("the engine is not serving"),
            Self::ArenaResize {
                recorded_bytes,
                offered_bytes,
                placed,
                clean,
            } => {
                match recorded_bytes {
                    Some(bytes) => write!(
                        f,
                        "the arena memory of {offered_bytes} bytes holds this pool's arena made \
                         for {bytes} bytes"
                    )?,
                    None => write!(
                        f,
                        "the arena memory of {offered_bytes} bytes holds this pool's arena made \
                         for another size, whose slots cannot be read"
                    )?,
                }
                let mut why = Vec::new();
                if *placed > 0 {
                    why.push(format!(
                        "{placed} of its blocks are not yet in a consistency point"
                    ));
                }
                if recorded_bytes.is_none() {
                    why.push("what it holds cannot be told".to_string());
                }
                if !clean {
                    why.push("the pool did not shut down cleanly".to_string());
                }
                write!(
                    f,
                    ", and {}: recreating it at the new size could destroy acknowledged writes. \
                     Boot with the arena's own size so they are replayed, stop the pool cleanly, \
                     then change the size; or boot discarding the arena",
                    why.join(" and ")
                )
            }
            Self::NotReplica { vol } => write!(
                f,
                "volume {vol} is not a replication destination; only a replication sink writes \
                 through this path"
            ),
            Self::ReplicaVolume { vol } => write!(
                f,
                "volume {vol} is a replication destination (a mirror) and stays offline; release \
                 it with `otapctl replicate release` first"
            ),
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
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct VolumeStatus {
    /// Present in the volume table (durably, or pending the next CP).
    pub present: bool,
    /// Present and serving: the creation is durable, nvfail is clear and
    /// the volume is online.
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
    /// Taken offline by the operator.
    pub offline: bool,
    /// The catalogue name, if the volume has one.
    pub name: Option<String>,
    /// A rollback, resize or deletion waits for its CP.
    pub op_pending: Option<String>,
}

/// A snapshot as the engine lists it.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
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
    /// The boot could not read its root page or pinned index, or a walk
    /// found one of its map pages damaged since (it is quarantined): reads,
    /// diffs and rollbacks of it are refused
    /// ([`OperatorWarning::SnapshotDamaged`]).
    pub damaged: bool,
    /// The `PIN_*` bits in force.
    pub flags: u8,
    /// The catalogue name, if the snapshot has one.
    pub name: Option<String>,
    /// A deletion waits for its CP.
    pub deleting: bool,
}

/// One segment whose live count disagrees with a walk of every tree
/// ([`Engine::audit_liveness`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LivenessDrift {
    /// The segment.
    pub pseg: u64,
    /// Blocks the engine counts live.
    pub counted: u64,
    /// Blocks the live map and the snapshots reference.
    pub found: u64,
}

/// The pool as an operator sees it.
#[derive(Debug, Clone, PartialEq)]
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
    /// Free segments front-door writes leave alone (S5-3).
    pub reserve_segments: u32,
    /// Segments in use over segments, `0.0..=1.0`.
    pub utilization: f64,
    /// Data blocks the admission rule would still accept from clients.
    pub admissible_blocks: u64,
    /// Data blocks acknowledged and not yet in a consistency point.
    pub unflushed_blocks: u64,
    /// Arena occupancy, `0.0..=1.0`.
    pub arena_occupancy: f64,
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
    /// Volumes present.
    pub volumes: usize,
    /// Names in the catalogue.
    pub catalog_entries: usize,
    /// Delta regions in the snapshot table's chain since its last fold
    /// (F20).
    pub snapshot_table_deltas: usize,
    /// Delta regions in the catalogue's chain since its last fold.
    pub catalog_deltas: usize,
    /// Segments the cleaner drained since boot.
    pub cleaned_segments: u64,
    /// Blocks the cleaner relocated since boot.
    pub relocated_blocks: u64,
    /// The pool verification in progress, or the last one finished.
    pub verify: Option<crate::verify::VerifySummary>,
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
    /// The segments this CP compacts, with the blocks it relocates.
    clean: Vec<CleanTarget>,
    /// The CP took the explicit clean request.
    clean_requested: bool,
}

impl CpJob {
    /// `true` when the CP was prepared as a fold.
    fn folds(&self) -> bool {
        self.fold_requested || !self.snaps.is_empty() || self.policy_fold
    }

    /// Blocks the cleaner relocates in this CP.
    fn reloc_blocks(&self) -> usize {
        self.clean.iter().map(|t| t.blocks.len()).sum()
    }
}

type Delta = (u32, u64, MapEntry);

/// The stripe images a CP's metadata phase still has to write.
type MetaStripeQueue = VecDeque<(u64, Vec<u8>)>;

#[derive(Debug)]
struct Staging {
    job: CpJob,
    next: usize,
    /// The next relocated block to stage, as `(target, block)` indexes
    /// flattened over `job.clean`.
    reloc_next: usize,
    inflight: HashMap<Tag, Vec<Delta>>,
    deltas: Vec<Delta>,
    pending: Option<(u64, Vec<u8>, Vec<Delta>)>,
    /// Every data stripe written so far, for the CP's stripe list (F17).
    stripes: Vec<CpStripe>,
    failed: Option<String>,
    /// A relocation read failed: the segment is skipped from now on and
    /// the CP is retried without it.
    reloc_failed: Option<(u64, String)>,
}

/// A snapshot the engine holds: the record and the pinned root, or the
/// damage the boot found instead of the root.
#[derive(Debug, Clone)]
struct SnapState {
    record: SnapRecord,
    pinned: Option<PinnedRoot>,
    damage: Option<String>,
}

impl SnapState {
    fn birth(&self) -> u64 {
        u64::from(self.record.birth_cp)
    }

    fn is_user_class(&self) -> bool {
        matches!(
            self.record.snap_origin(),
            Some(SnapOrigin::User | SnapOrigin::Scheduled) | None
        )
    }
}

/// A snapshot asked for and not yet taken.
#[derive(Debug, Clone)]
struct SnapRequest {
    vol: u32,
    uuid: Uuid,
    kind: SnapKind,
    origin: SnapOrigin,
    name: Option<String>,
    /// The active generation when the request was made, when it held
    /// writes: the snapshot must see them, so the CP that carries that
    /// generation (or a later one) takes the request, never a CP whose
    /// generation was cut before.
    after_gen: Option<GenId>,
}

/// A change to a volume that waits for a consistency point.
#[derive(Debug, Clone, PartialEq, Eq)]
enum VolOp {
    /// Roll back to the snapshot, bumping the rollback generation to `gen`.
    Rollback { snap_id: u64, gen: u32 },
    /// Resize to `size_blocks`.
    Resize { size_blocks: u64 },
    /// Delete.
    Delete,
}

impl fmt::Display for VolOp {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Rollback { snap_id, .. } => write!(f, "rollback to snapshot {snap_id}"),
            Self::Resize { size_blocks } => write!(f, "resize to {size_blocks} blocks"),
            Self::Delete => f.write_str("delete"),
        }
    }
}

/// One rewrite of a shared tree: the tree (its index region), the snapshots
/// that pin it, and the address changes they all take.
type RewriteGroup = (RootRef, Vec<u64>, Vec<(u64, MapEntry, MapEntry)>);

/// What a snapshot deletion found walking the snapshot's tree (E36;
/// Step 6A for the damaged pages).
#[derive(Debug, Default)]
struct DeletionWalk {
    /// The data blocks only the snapshot named, released by the deletion.
    released: Vec<MapEntry>,
    /// The snapshot's map pages that did not verify: the blocks they name
    /// cannot be known and stay counted, the safe direction.
    damaged_pages: Vec<u64>,
    /// A page of the next newer snapshot did not verify while it was asked
    /// whether it still names a block (it was assumed to).
    newer_damaged: bool,
}

/// A snapshot the cleaner rewrote in this CP: its new state and the regions
/// the old root stops referencing.
#[derive(Debug, Clone)]
struct Rewritten {
    snap_id: u64,
    state: SnapState,
    freed_pages: Vec<FreedPage>,
    freed_index: FreedIndex,
    old_root_page: RootRef,
    new_pages: Vec<(u64, Box<otap_format::MapPage>)>,
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
    /// The snapshot table's chain after this CP (F20).
    snap_chain: TableChain,
    /// The snapshot records after this CP, in id order.
    snap_records: Vec<SnapRecord>,
    catalog: Vec<CatalogRec>,
    /// The catalogue's chain after this CP.
    catalog_chain: TableChain,
    cp_stripes: RootRef,
    new_snapshots: Vec<SnapState>,
    /// Snapshots this CP deletes: `(snap_id, vol, blocks released)`.
    deleted_snapshots: Vec<(u64, u32, u64)>,
    /// Pin changes this CP makes durable: `(snap_id, flags)`.
    pins: Vec<(u64, u8)>,
    /// Snapshots the cleaner rewrote.
    rewritten: Vec<Rewritten>,
    /// Volume operations this CP carries.
    vol_ops: Vec<(u32, VolOp)>,
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
            snap_chain: self.snap_chain,
            snap_records: self.snap_records,
            catalog: self.catalog,
            catalog_chain: self.catalog_chain,
            cp_stripes: self.cp_stripes,
            new_snapshots: self.new_snapshots,
            deleted_snapshots: self.deleted_snapshots,
            pins: self.pins,
            rewritten: self.rewritten,
            vol_ops: self.vol_ops,
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
    /// The roots, directory, stripe list and segment table regions of this
    /// CP (replaced by the next CP; the table heads are not among them).
    table_regions: Vec<RootRef>,
    map_roots: RootRef,
    seg_table: RootRef,
    cp_log_head: RootRef,
    snap_chain: TableChain,
    snap_records: Vec<SnapRecord>,
    catalog: Vec<CatalogRec>,
    catalog_chain: TableChain,
    cp_stripes: RootRef,
    new_snapshots: Vec<SnapState>,
    deleted_snapshots: Vec<(u64, u32, u64)>,
    pins: Vec<(u64, u8)>,
    rewritten: Vec<Rewritten>,
    vol_ops: Vec<(u32, VolOp)>,
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

#[derive(Debug, Clone, PartialEq, Eq, Default)]
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
    /// Taken offline by the operator: nothing is served, and the state is
    /// recorded in the volume table (F18).
    offline: bool,
    /// The catalogue name, if any.
    name: Option<String>,
    /// A rollback, resize or deletion waiting for its CP.
    op: Option<VolOp>,
    /// The CP whose metadata first carried the operation.
    op_in: Option<u64>,
}

impl VolRt {
    const fn serving(&self) -> bool {
        self.present && !self.creating && self.nvfail.is_none() && !self.offline
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
    /// Snapshots deleted.
    pub snapshots_deleted: u64,
    /// Data segments the cleaner drained.
    pub cleaned: u64,
    /// Blocks the cleaner relocated.
    pub relocated: u64,
    /// Relocation stripes written.
    pub reloc_stripes: u64,
    /// Writes refused by the admission rule (the reserve).
    pub admission_refusals: u64,
    /// Acknowledgements delayed by pacing.
    pub paced: u64,
    /// Volume rollbacks committed.
    pub rollbacks: u64,
    /// Blocks written, and holes punched, by a replication sink (Step 6C).
    pub applied: u64,
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
    /// Snapshots asked to be deleted, waiting for a CP.
    snap_deletes: Vec<u64>,
    /// Pin changes waiting for a CP: `(snap_id, flags)`.
    pin_changes: Vec<(u64, u8)>,
    /// The committed catalogue, in canonical order, and its chain on disk
    /// (F20): carried forward by a CP that changes no name.
    catalog: Vec<CatalogRec>,
    catalog_chain: TableChain,
    /// The snapshot table's chain on disk (F20); the records are the
    /// snapshots' own.
    snap_chain: TableChain,
    /// The table folds the CP being prepared attempted, and whether each
    /// was the policy's (and may be put off when it does not fit): read
    /// when the attempt fails for space.
    table_folds_tried: Vec<(TableKind, bool)>,
    /// The slot the last committed CP wrote (its `A` copy): what a
    /// verification compares the ring with.
    last_slot: Option<Box<SuperblockSlot>>,
    /// The regions the last commit's release freed: still counted in the
    /// live bytes that commit's segment table recorded.
    last_released: Vec<RootRef>,
    /// The pool verification in progress or the last one finished
    /// ([`crate::verify`]).
    verify: Option<Box<VerifyJob>>,
    /// Free segments front-door writes leave alone.
    reserve: u32,
    /// A clean was requested; the next CP compacts whatever qualifies.
    clean_requested: bool,
    /// Segments the cleaner found it cannot clean (a damaged snapshot, a
    /// summary that does not read); cleared when the snapshot set changes.
    cleaner_skip: HashSet<u64>,
    /// Clean CPs run on a full pool since a segment was last freed: bounds
    /// the pumping of deferred frees.
    pump_cps: u32,
    /// Writes placed and acknowledged later (pacing): the request, its
    /// sequence number and when it is due.
    paced: VecDeque<(Request, u64, u64)>,
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
    /// A cleaning CP failed for want of a segment with this many free: no
    /// clean is planned until more are free, so the retries do not spend
    /// the reserve on an attempt that cannot land (E37).
    clean_blocked_at: Option<u32>,
    /// The data segments the CP in flight allocated: given back when the
    /// attempt fails, since no committed CP names them.
    job_data_segments: Vec<u64>,
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
    /// Volumes a replication sink placed writes to (Step 6C,
    /// [`Engine::apply_write`]) while outside the durable write-active set:
    /// the next intent slot carries them, as it carries the volumes of the
    /// writes waiting in `pending_acks`.
    apply_intent: VolBitSet,
    /// Replication destinations (Step 6C): volumes kept offline to clients
    /// that only [`Engine::apply_write`] and [`Engine::apply_punch`] write.
    /// In memory only (no format change): the sink marks them again when
    /// it attaches after a boot, and an offline volume is never served
    /// meanwhile.
    replicas: VolBitSet,
    /// The highest write sequence number a committed CP carried since the
    /// boot: every write at or below it is durable (sequence numbers are
    /// in placement order).
    durable_seq: u64,
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
        // The arena is attached or created without writing it (Step 6A):
        // the damage, the blank or the lost arena that make this boot hold
        // the write-active volumes stay in the memory until the boot
        // commit's `A` slot, which records the hold in its volume table,
        // has landed (`step_commit`). A boot that stops before then leaves
        // the next one the same evidence, so no number of kills during the
        // boot loses the hold.
        let (arena, reattach, arena_lost) = match Arena::can_attach(&memory, &cfg.agg_id) {
            // The engine fills one reservation at a time, whole, on its own
            // thread (`serve_write`, `serve_punch`, `apply_write`,
            // `apply_punch`), so an interrupted fill is its newest
            // reservation's (the Step 6 review's D10).
            Ok(()) => match Arena::attach_deferred_with(memory, &cfg.agg_id, Fills::Serial) {
                Ok((arena, report)) => (arena, Some(report), false),
                // The table could not be reconciled: the arena is lost.
                Err((_, memory)) => match Arena::create_deferred(memory, cfg.agg_id, wall) {
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
            Err(ArenaError::BadGeometry) => {
                // This pool's arena, made for another size: an arena file a
                // larger size grew keeps it whole at its start. Recreating
                // it destroys whatever it holds, so the resize must be safe
                // or asked for (Step 6A).
                let survey = Arena::survey(&memory, &cfg.agg_id).ok();
                if let Err(e) = check_arena_resize(
                    survey.as_ref(),
                    state.is_clean(),
                    memory.len() as u64,
                    cfg.discard_arena,
                ) {
                    fail!(e, memory);
                }
                let lost = !survey.is_some_and(|s| s.holds_nothing());
                if !lost && state.is_clean() && !cfg.discard_arena {
                    // A clean pool's empty arena: there is no hold to record
                    // first, so the new arena is written now. Deferred, a
                    // kill after the boot commit's `A` slot (which makes the
                    // newest CP a running one) left the old header at the
                    // new size, and every later boot refused the resize as
                    // unclean (the Step 6 review's W3).
                    match Arena::create(memory, cfg.agg_id, wall) {
                        Ok(arena) => (arena, None, false),
                        Err((e, memory)) => fail!(e.into(), memory),
                    }
                } else {
                    match Arena::create_deferred(memory, cfg.agg_id, wall) {
                        Ok(arena) => (arena, None, lost),
                        Err((e, memory)) => fail!(e.into(), memory),
                    }
                }
            }
            Err(_) => {
                let lost = !Arena::is_blank(&memory);
                match Arena::create_deferred(memory, cfg.agg_id, wall) {
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
            catalog,
            mut catalog_chain,
            catalog_damage,
            mut snap_chain,
            snapshot_table_damage,
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
                vols[i].offline = row.is_offline();
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
                // Every release here waits with the attach's own writes for
                // the boot commit's `A` slot: an orphan is a cause to hold.
                if !known {
                    orphaned_blocks += u64::from(extent.blocks);
                    if let Err(e) = arena.release_after_attach(extent.ring, slots) {
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
                            if let Err(e) = arena.release_after_attach(ring, blocks) {
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
                        if let Err(e) = arena.release_after_attach(extent.ring, slots) {
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
        // The catalogue: a name for a volume the table has, a name for a
        // snapshot the table has; anything else is noise a crash cannot
        // have left (names travel with their objects) and is reported. A
        // damaged or adjusted table is folded by the first commit, which
        // writes it as the engine holds it (F21).
        if let Some(reason) = &catalog_damage {
            let warning = OperatorWarning::CatalogDamaged {
                reason: reason.clone(),
            };
            events.push(Event::OperatorWarning(warning.clone()));
            warnings.push(warning);
            catalog_chain.repair = true;
        }
        if let Some(reason) = &snapshot_table_damage {
            let warning = OperatorWarning::SnapshotTableDamaged {
                reason: reason.clone(),
            };
            events.push(Event::OperatorWarning(warning.clone()));
            warnings.push(warning);
            snap_chain.repair = true;
        }
        let snapshot_keys: HashSet<(u32, Uuid)> = snapshots
            .iter()
            .map(|s| (s.record.vol, s.record.snap_uuid))
            .collect();
        let mut catalog_kept = Vec::new();
        for rec in catalog {
            let keep = match rec.catalog_kind() {
                Some(CatalogKind::Volume) => vols.get(rec.vol as usize).is_some_and(|v| v.present),
                Some(CatalogKind::Snapshot) => snapshot_keys.contains(&(rec.vol, rec.snap_uuid)),
                None => false,
            };
            if keep {
                if rec.catalog_kind() == Some(CatalogKind::Volume) {
                    vols[rec.vol as usize].name = Some(rec.name().to_string());
                }
                catalog_kept.push(rec);
            } else {
                catalog_chain.repair = true;
                events.push(Event::Internal {
                    what: format!(
                        "the catalogue names {:?} for volume {} which the table lacks; dropped",
                        rec.name(),
                        rec.vol
                    ),
                });
            }
        }
        let reserve = cfg
            .reserve_segments
            .unwrap_or_else(|| reserve_for(layout.segments))
            .min(layout.segments / 2);
        let snapshots: Vec<SnapState> = snapshots
            .into_iter()
            .map(
                |BootSnapshot {
                     record,
                     pinned,
                     mut damage,
                 }| {
                    // A record recovered from a damaged table, now or at an
                    // earlier boot (the bit is persisted), is quarantined:
                    // its last change may have been lost (F21).
                    if damage.is_none() && record.is_uncertain() {
                        damage = Some(
                            "its record was recovered from a damaged snapshot table; its last \
                             change may have been lost"
                                .to_string(),
                        );
                    }
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
            snap_deletes: Vec::new(),
            pin_changes: Vec::new(),
            catalog: catalog_kept,
            catalog_chain,
            snap_chain,
            table_folds_tried: Vec::new(),
            last_slot: None,
            last_released: Vec::new(),
            verify: None,
            reserve,
            clean_requested: false,
            cleaner_skip: HashSet::new(),
            pump_cps: 0,
            paced: VecDeque::new(),
            next_snap_id,
            fold_requested: false,
            last_fold_ns: now,
            fold_blocked_at: None,
            fold_damaged: false,
            clean_blocked_at: None,
            job_data_segments: Vec::new(),
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
            apply_intent: VolBitSet::EMPTY,
            replicas: VolBitSet::EMPTY,
            durable_seq: 0,
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
        self.complete_paced();
        if self.serving && !self.stopped {
            self.serve_front_door()?;
        } else if self.stopped {
            self.refuse_everything();
        }
        self.maybe_cut()?;
        self.maybe_start_cp()?;
        self.maybe_pump()?;
        self.tick_verify();
        Ok(())
    }

    /// Starts a pool verification ([`crate::verify`]): it attests the pool
    /// as the committed CP left it, advanced by every tick in slices that
    /// `opts` bounds, while the devices keep being served. A verification
    /// already running is answered with its report (the options of the
    /// first stand). Refused while the engine is not serving.
    ///
    /// # Errors
    /// [`EngineError::NotServing`].
    pub fn start_verify(&mut self, opts: VerifyOptions) -> Result<VerifyReport, EngineError> {
        if !self.serving || self.stopped {
            return Err(EngineError::NotServing);
        }
        if let Some(job) = self.verify.as_ref().filter(|j| j.is_running()) {
            return Ok(job.report());
        }
        self.verify = Some(Box::new(VerifyJob::new(opts)));
        self.tick_verify();
        self.verify
            .as_ref()
            .map(|j| j.report())
            .ok_or_else(|| EngineError::Internal("the verification vanished".into()))
    }

    /// The verification in progress, or the last one, if any.
    #[must_use]
    pub fn verify_report(&self) -> Option<VerifyReport> {
        self.verify.as_ref().map(|j| j.report())
    }

    /// `true` while a verification runs.
    #[must_use]
    pub fn verify_running(&self) -> bool {
        self.verify.as_ref().is_some_and(|j| j.is_running())
    }

    /// Cancels the verification in progress; returns its report, or `None`
    /// when none runs. The frees it held are released.
    pub fn cancel_verify(&mut self) -> Option<VerifyReport> {
        let job = self.verify.as_mut().filter(|j| j.is_running())?;
        job.stop(
            VerifyState::Cancelled,
            "cancelled by the operator".to_string(),
        );
        self.finish_verify();
        self.verify_report()
    }

    /// A fault injection for tests and support bundles: moves the live
    /// block count the engine keeps for data segment `pseg` by `delta`, so
    /// the next consistency point records a wrong `live_bytes` in the
    /// segment table. A pool verification must report it.
    pub fn skew_live_blocks(&mut self, pseg: u64, delta: i64) {
        let live = self.data_live.entry(pseg).or_insert(0);
        *live = live.saturating_add_signed(delta);
    }

    /// Advances the verification by one slice: captures its view once no
    /// superblock write is in flight, aborts it when the pool needs the
    /// space it holds or is shutting down, and ends it.
    fn tick_verify(&mut self) {
        let Some(mut job) = self.verify.take() else {
            return;
        };
        if !job.is_running() {
            self.verify = Some(job);
            return;
        }
        let abort = if self.shutdown_requested || self.stopped {
            Some("the pool is shutting down".to_string())
        } else if self.pool_full && self.verify_holds_something(&job) {
            // Only what it holds can be in the pool's way: a verification of
            // a full pool that holds nothing goes on, so a full pool can
            // still be attested (the Step 6 review's R3).
            Some("the pool is full and the verification holds deferred frees".to_string())
        } else if self.pool_tight() && self.verify_holds_something(&job) {
            Some(format!(
                "the pool is tight ({} free segments, a reserve of {}) and the verification \
                 held deferred frees",
                self.segments.free_count(),
                self.reserve
            ))
        } else {
            None
        };
        if let Some(reason) = abort {
            job.stop(VerifyState::Aborted, reason);
            self.verify = Some(job);
            self.finish_verify();
            return;
        }
        if job.needs_view() {
            // The ring is read when no slot write is in flight, so the pair
            // of the committed CP is whole and the other pair is its
            // predecessor.
            if matches!(self.state, State::CommitA { .. } | State::CommitB { .. }) {
                self.verify = Some(job);
                return;
            }
            let Some(view) = self.capture_view() else {
                job.stop(
                    VerifyState::Aborted,
                    "no commit of this engine to attest yet".to_string(),
                );
                self.verify = Some(job);
                self.finish_verify();
                return;
            };
            match boot::read_ring(&mut self.backstore, &self.layout) {
                Ok(ring) => job.begin(view, &ring, &self.layout),
                Err(e) => {
                    job.stop(
                        VerifyState::Aborted,
                        format!("the superblock ring cannot be read: {e}"),
                    );
                    self.verify = Some(job);
                    self.finish_verify();
                    return;
                }
            }
            self.verify = Some(job);
            return;
        }
        let finished = {
            let clock = &self.clock;
            let now = || clock.mono_ns();
            job.step(&mut self.backstore, &self.layout, &now)
        };
        self.verify = Some(job);
        if finished {
            self.finish_verify();
        }
    }

    /// `true` when `job` holds a free a later CP made: a region or a data
    /// segment it keeps from reuse right now (a tight pool then wants it).
    fn verify_holds_something(&self, job: &VerifyJob) -> bool {
        job.holds_frees_after().is_some_and(|n| {
            self.freed_pending.iter().any(|(cp, _)| *cp > n)
                || self.dead_data_pending.iter().any(|(cp, _)| *cp > n)
        })
    }

    /// The pool as the committed CP left it, for a verification.
    fn capture_view(&self) -> Option<View> {
        let slot = **self.last_slot.as_ref()?;
        if slot.sb.cp_seq != self.cp_seq {
            return None;
        }
        Some(View {
            agg_id: self.cfg.agg_id,
            cp_seq: self.cp_seq,
            cp_nonce: self.cp_nonce,
            slot,
            vols: self
                .map
                .volumes()
                .map(|(vol, m)| LiveVolume {
                    vol,
                    root: *m.root(),
                    index: m.index().to_vec(),
                    overlay: m.overlay().collect(),
                })
                .collect(),
            snaps: self
                .snapshots
                .iter()
                .map(|s| SnapshotView {
                    record: s.record,
                    pinned: s.pinned.clone(),
                    damage: s.damage.clone(),
                })
                .collect(),
            delta_roots: self.map.delta_roots().to_vec(),
            fold_cp: self.map.fold_cp(),
            snap_chain: self.snap_chain.clone(),
            catalog_chain: self.catalog_chain.clone(),
            snap_records: self.snapshot_records(),
            catalog: self.catalog.clone(),
            pending: self
                .freed_pending
                .iter()
                .flat_map(|(_, regions)| regions.iter().copied())
                .chain(self.last_released.iter().copied())
                .collect(),
        })
    }

    /// A verification ended: the repairs it asked for, the pool status
    /// warning, the event, and the frees it held.
    fn finish_verify(&mut self) {
        let Some(job) = self.verify.as_mut() else {
            return;
        };
        let (snapshots, catalog) = job.take_repairs();
        let report = job.report();
        if snapshots {
            self.snap_chain.repair = true;
        }
        if catalog {
            self.catalog_chain.repair = true;
        }
        if snapshots || catalog {
            self.request_cp();
        }
        self.warnings
            .retain(|w| !matches!(w, OperatorWarning::AttestationFindings { .. }));
        if report.state == VerifyState::Done && report.errors + report.warnings > 0 {
            let first = report
                .findings
                .first()
                .map(|f| format!("{}: {}", f.kind.as_str(), f.detail))
                .unwrap_or_default();
            let warning = OperatorWarning::AttestationFindings {
                cp_seq: report.cp_seq,
                errors: report.errors,
                warnings: report.warnings,
                first,
            };
            self.events.push(Event::OperatorWarning(warning.clone()));
            self.warnings.push(warning);
        }
        self.events.push(Event::VerifyFinished {
            cp_seq: report.cp_seq,
            state: report.state,
            verdict: report.verdict.unwrap_or(Verdict::Incomplete),
            errors: report.errors,
            warnings: report.warnings,
        });
        // The holds end. A CP past its metadata preparation took its copy
        // of the counts before this, and its commit releases what is due;
        // otherwise it is released now.
        if matches!(self.state, State::Idle | State::Staging(_)) {
            let cp = self.cp_seq;
            self.release_freed(cp, false);
        }
    }

    /// Asks for a consistency point at the next tick (even an empty one);
    /// on a full pool, one more attempt.
    pub fn request_cp(&mut self) {
        self.cp_requested = true;
        self.pool_retry = true;
    }

    /// Asks the next consistency point to compact every data segment that
    /// qualifies, whatever the utilisation (`otapctl pool clean`).
    pub fn request_clean(&mut self) {
        self.clean_requested = true;
        self.request_cp();
    }

    /// Acknowledges the paced writes that are due.
    fn complete_paced(&mut self) {
        let now = self.clock.mono_ns();
        while let Some((_, _, due)) = self.paced.front() {
            if *due > now {
                break;
            }
            let (req, seq, _) = self.paced.pop_front().expect("front exists");
            self.front.complete(req, Status::Ok, seq);
        }
    }

    /// On a full pool with nothing in flight, runs a consistency point for
    /// the cleaner (when a candidate exists and the relocation fits) or to
    /// advance the deferred frees (a drained or dead segment becomes free
    /// one or two commits after its CP). Bounded: three such CPs without a
    /// segment coming free, and the pool stays full until a request.
    fn maybe_pump(&mut self) -> Result<(), EngineError> {
        let tight = self.cfg.cleaner.background && self.pool_tight();
        if (!self.pool_full && !tight)
            || !matches!(self.state, State::Idle)
            || self.index.frozen_len() > 0
            || self.shutdown_requested
            || self.stopped
            || !self.boot_done()
            || self.pump_cps >= 3
        {
            return Ok(());
        }
        let frees_pending = !self.dead_data_pending.is_empty();
        // A snapshot deletion or a volume operation waiting on a full pool
        // needs a CP of its own to ride: the one that set `pool_full` may
        // have carried no generation, so nothing else would start one.
        let ops_pending = !self.snap_deletes.is_empty() || self.vols.iter().any(|v| v.op.is_some());
        let can_clean = self.free_segments() > META_RESERVE_SEGMENTS
            && !ops_pending
            && !self
                .clean_blocked_at
                .is_some_and(|at| self.free_segments() <= at)
            && !cleaner::candidates(
                &self.segments,
                &self.layout,
                |p| self.data_live.get(&p).copied().unwrap_or(0),
                &self.cleaner_excluded(),
                self.cfg.cleaner.max_live_fraction,
            )
            .is_empty();
        if !frees_pending && !can_clean && !ops_pending {
            return Ok(());
        }
        self.pump_cps += 1;
        if can_clean {
            self.clean_requested = true;
        }
        let job = self.new_job(CpTrigger::Clean, None, RunState::Running);
        self.start_job(job)
    }

    /// `true` when the free segments are down to the reserve plus the
    /// cleaner's margin.
    fn pool_tight(&self) -> bool {
        self.segments.free_count()
            <= self
                .reserve
                .saturating_add(self.cfg.cleaner.margin_segments)
    }

    /// Segments the cleaner must not scan: open, protected since the boot,
    /// awaiting a discard, already dead, or found uncleanable.
    fn cleaner_excluded(&self) -> HashSet<u64> {
        let mut out: HashSet<u64> = self.cleaner_skip.clone();
        out.extend(self.open_data.map(|o| o.pseg));
        out.extend(self.open_meta.map(|o| o.pseg));
        out.extend(
            self.boot_protect
                .iter()
                .flat_map(|(_, s)| s.iter().copied()),
        );
        out.extend(
            self.dead_data_pending
                .iter()
                .flat_map(|(_, s)| s.iter().copied()),
        );
        out.extend(self.discarding.values().copied());
        out.extend(self.pending_discards.iter().copied());
        out
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
        // A new volume at a reused index is nobody's replica.
        let _ = self.replicas.clear(vol);
        self.request_cp();
        Ok(())
    }

    /// Asks for a snapshot of `vol`: taken at the next consistency point,
    /// which folds the map so the root is complete, and durable when that
    /// CP commits ([`Event::SnapshotCreated`]). Returns nothing until then;
    /// [`snapshots`](Self::snapshots) lists it afterwards. A volume holds at
    /// most [`MAX_USER_SNAPSHOTS`] user and scheduled snapshots and
    /// [`MAX_RESERVED_SNAPSHOTS`] replication baselines and divergent
    /// heads, counting the ones waiting for their CP.
    pub fn create_snapshot(
        &mut self,
        vol: u32,
        uuid: Uuid,
        kind: SnapKind,
        origin: SnapOrigin,
    ) -> Result<(), EngineError> {
        self.create_snapshot_named(vol, uuid, kind, origin, None)
    }

    /// [`create_snapshot`](Self::create_snapshot) with a catalogue name
    /// (F16), unique among the snapshots of the volume, and an identity
    /// the engine derives; returns that identity.
    pub fn create_named_snapshot(
        &mut self,
        vol: u32,
        name: &str,
        kind: SnapKind,
        origin: SnapOrigin,
    ) -> Result<Uuid, EngineError> {
        let uuid = self.new_uuid();
        self.create_snapshot_named(vol, uuid, kind, origin, Some(name.to_string()))?;
        Ok(uuid)
    }

    fn create_snapshot_named(
        &mut self,
        vol: u32,
        uuid: Uuid,
        kind: SnapKind,
        origin: SnapOrigin,
        name: Option<String>,
    ) -> Result<(), EngineError> {
        let v = self
            .vols
            .get(vol as usize)
            .filter(|v| v.present && !v.creating)
            .ok_or(EngineError::NoSuchVolume { vol })?;
        if v.nvfail.is_some() {
            return Err(EngineError::NvFailed { vol });
        }
        if v.op.is_some() {
            return Err(EngineError::VolumeBusy { vol });
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
        if let Some(name) = &name {
            validate_name(name.as_bytes())?;
            if self.find_snapshot(vol, name).is_some()
                || self
                    .snap_requests
                    .iter()
                    .any(|r| r.vol == vol && r.name.as_deref() == Some(name))
                || self.in_flight_snapshot_name(vol, name)
            {
                return Err(EngineError::NameExists { name: name.clone() });
            }
        }
        if self.fold_damaged {
            return Err(EngineError::MapDamaged);
        }
        let user_class = matches!(origin, SnapOrigin::User | SnapOrigin::Scheduled);
        let limit = if user_class {
            MAX_USER_SNAPSHOTS
        } else {
            MAX_RESERVED_SNAPSHOTS
        };
        let same_class =
            |o: SnapOrigin| matches!(o, SnapOrigin::User | SnapOrigin::Scheduled) == user_class;
        let held = self
            .snapshots
            .iter()
            .filter(|s| s.record.vol == vol && s.is_user_class() == user_class)
            .count()
            + self
                .snap_requests
                .iter()
                .filter(|r| r.vol == vol && same_class(r.origin))
                .count()
            + self.in_flight_snapshots_of(vol, user_class);
        if held >= limit {
            return Err(EngineError::SnapshotLimit { vol, limit });
        }
        self.snap_requests.push(SnapRequest {
            vol,
            uuid,
            kind,
            origin,
            name,
            after_gen: (self.index.active_blocks() > 0).then(|| self.index.active_id()),
        });
        self.request_fold();
        Ok(())
    }

    /// Snapshot requests of `vol` and class a CP in flight holds.
    fn in_flight_snapshots_of(&self, vol: u32, user_class: bool) -> usize {
        self.in_flight_job().map_or(0, |j| {
            j.snaps
                .iter()
                .filter(|r| {
                    r.vol == vol
                        && matches!(r.origin, SnapOrigin::User | SnapOrigin::Scheduled)
                            == user_class
                })
                .count()
        })
    }

    fn in_flight_snapshot_name(&self, vol: u32, name: &str) -> bool {
        self.in_flight_job().is_some_and(|j| {
            j.snaps
                .iter()
                .any(|r| r.vol == vol && r.name.as_deref() == Some(name))
        })
    }

    fn in_flight_job(&self) -> Option<&CpJob> {
        match &self.state {
            State::Staging(st) => Some(&st.job),
            State::Meta(mw) => Some(&mw.prep.job),
            State::Flush { prep, .. }
            | State::CommitA { prep, .. }
            | State::CommitB { prep, .. } => Some(&prep.job),
            State::Idle => None,
        }
    }

    /// An identity for a snapshot the engine names itself: derived from
    /// the aggregate, the clocks and a counter, never nil, with the RFC
    /// 4122 version and variant bits of a random uuid.
    fn new_uuid(&mut self) -> Uuid {
        self.nonce_counter += 1;
        let mut hasher = blake3::Hasher::new();
        hasher.update(self.cfg.agg_id.as_array());
        hasher.update(b"snapshot");
        hasher.update(&self.clock.wall_ns().to_le_bytes());
        hasher.update(&self.clock.mono_ns().to_le_bytes());
        hasher.update(&self.nonce_counter.to_le_bytes());
        hasher.update(&self.boot_count.to_le_bytes());
        let hash = hasher.finalize();
        let mut bytes = [0u8; 16];
        bytes.copy_from_slice(&hash.as_bytes()[..16]);
        bytes[6] = (bytes[6] & 0x0f) | 0x40;
        bytes[8] = (bytes[8] & 0x3f) | 0x80;
        Uuid::from_bytes(bytes)
    }

    /// Asks for snapshot `snap_id` to be deleted: gone when the next
    /// consistency point commits ([`Event::SnapshotDeleted`]), which
    /// releases the blocks only it referenced (E36). A pinned snapshot
    /// refuses (S5-2); unpin it or move the replication base first.
    pub fn delete_snapshot(&mut self, snap_id: u64) -> Result<(), EngineError> {
        let snap = self
            .snapshots
            .iter()
            .find(|s| s.record.snap_id == snap_id)
            .ok_or(EngineError::NoSuchSnapshot { snap_id })?;
        let flags = self.effective_pins(snap_id, snap.record.flags);
        if flags & PIN_MASK != 0 {
            return Err(EngineError::SnapshotPinned { snap_id, flags });
        }
        if let Some(v) = self.vols.get(snap.record.vol as usize) {
            if v.op.is_some() {
                return Err(EngineError::VolumeBusy {
                    vol: snap.record.vol,
                });
            }
        }
        if !self.snap_deletes.contains(&snap_id) {
            self.snap_deletes.push(snap_id);
        }
        self.request_cp();
        Ok(())
    }

    /// The pin flags of `snap_id` once the pending changes land (the
    /// `PIN_*` bits only; [`SNAP_UNCERTAIN`] is not a pin).
    fn effective_pins(&self, snap_id: u64, committed: u8) -> u8 {
        self.pin_changes
            .iter()
            .rev()
            .find(|(id, _)| *id == snap_id)
            .map_or(committed & PIN_MASK, |(_, f)| *f)
    }

    /// Sets the `PIN_*` flags of `snap_id`, durable with the next
    /// consistency point ([`Event::SnapshotPinned`]). A pinned snapshot
    /// cannot be deleted.
    pub fn set_snapshot_pins(&mut self, snap_id: u64, flags: u8) -> Result<(), EngineError> {
        if flags & !PIN_MASK != 0 {
            return Err(EngineError::Format(FormatError::BadField {
                field: "flags",
                value: u64::from(flags),
            }));
        }
        if !self.snapshots.iter().any(|s| s.record.snap_id == snap_id) {
            return Err(EngineError::NoSuchSnapshot { snap_id });
        }
        if self.snap_deletes.contains(&snap_id) {
            return Err(EngineError::NoSuchSnapshot { snap_id });
        }
        self.pin_changes.retain(|(id, _)| *id != snap_id);
        self.pin_changes.push((snap_id, flags));
        self.request_cp();
        Ok(())
    }

    /// Makes `snap_id` the replication base of its volume: it takes
    /// `PIN_REPLICATION`, and the previous base of the volume loses it in
    /// the same consistency point (a base is superseded by a newer
    /// committed one, S5-2). `None` releases the volume's base.
    pub fn set_replication_base(
        &mut self,
        vol: u32,
        snap_id: Option<u64>,
    ) -> Result<(), EngineError> {
        if let Some(id) = snap_id {
            let snap = self
                .snapshots
                .iter()
                .find(|s| s.record.snap_id == id)
                .ok_or(EngineError::NoSuchSnapshot { snap_id: id })?;
            if snap.record.vol != vol {
                return Err(EngineError::NoSuchSnapshot { snap_id: id });
            }
        }
        let current: Vec<(u64, u8)> = self
            .snapshots
            .iter()
            .filter(|s| s.record.vol == vol)
            .map(|s| {
                (
                    s.record.snap_id,
                    self.effective_pins(s.record.snap_id, s.record.flags),
                )
            })
            .collect();
        for (id, flags) in current {
            let wanted = if Some(id) == snap_id {
                flags | otap_format::PIN_REPLICATION
            } else {
                flags & !otap_format::PIN_REPLICATION
            };
            if wanted != flags {
                self.set_snapshot_pins(id, wanted)?;
            }
        }
        Ok(())
    }

    /// Takes `vol` offline (nothing is served on it; `Status::Offline`) or
    /// back online. Immediate; recorded in the volume table with the next
    /// consistency point (F18) so a reboot keeps the state. A replication
    /// destination (Step 6C) refuses to come online while it is a mirror
    /// ([`EngineError::ReplicaVolume`]).
    pub fn set_volume_online(&mut self, vol: u32, online: bool) -> Result<(), EngineError> {
        let replica = self.replicas.get(vol);
        let v = self
            .vols
            .get_mut(vol as usize)
            .filter(|v| v.present)
            .ok_or(EngineError::NoSuchVolume { vol })?;
        if online && v.op.is_some() {
            return Err(EngineError::VolumeBusy { vol });
        }
        if online && replica {
            return Err(EngineError::ReplicaVolume { vol });
        }
        if v.offline == !online {
            return Ok(());
        }
        v.offline = !online;
        self.request_cp();
        Ok(())
    }

    /// Checks that `vol` may take a rollback, a resize or a deletion:
    /// present, offline, nothing of it in the arena, no operation waiting.
    fn check_vol_op(&self, vol: u32) -> Result<&VolRt, EngineError> {
        let v = self
            .vols
            .get(vol as usize)
            .filter(|v| v.present && !v.creating)
            .ok_or(EngineError::NoSuchVolume { vol })?;
        if !v.offline {
            return Err(EngineError::VolumeOnline { vol });
        }
        if v.op.is_some() {
            return Err(EngineError::VolumeBusy { vol });
        }
        if self.unflushed_bytes_after(vol, None) > 0 || self.in_flight_touches(vol) {
            return Err(EngineError::VolumeDirty { vol });
        }
        Ok(v)
    }

    /// `true` while a CP in flight carries blocks or holes of `vol`.
    fn in_flight_touches(&self, vol: u32) -> bool {
        self.in_flight_job().is_some_and(|j| {
            j.blocks.iter().any(|b| b.vol == vol) || j.holes.iter().any(|(v, _, _)| *v == vol)
        })
    }

    /// `true` while a snapshot request of `vol` waits, or rides the CP in
    /// flight: a rollback, a resize or a deletion asked for now would land
    /// behind a snapshot newer than the state it was asked for (a forked
    /// history, whose deletion then releases blocks the live map names).
    fn snapshot_in_flight(&self, vol: u32) -> bool {
        self.snap_requests.iter().any(|r| r.vol == vol)
            || self
                .in_flight_job()
                .is_some_and(|j| j.snaps.iter().any(|r| r.vol == vol))
    }

    /// Asks for `vol` to be rolled back to snapshot `snap_id`: the volume
    /// must be offline with nothing in the arena, the snapshot its own and
    /// readable, and no newer snapshot of the volume may exist (delete
    /// them first). Done at the next consistency point, a fold
    /// ([`Event::VolumeRolledBack`]): the live tree becomes the snapshot's,
    /// every block written since is released, and the rollback generation
    /// moves on so clients see a new identity (section 7.6).
    pub fn rollback_volume(&mut self, vol: u32, snap_id: u64) -> Result<(), EngineError> {
        let snap = self
            .snapshots
            .iter()
            .find(|s| s.record.snap_id == snap_id)
            .ok_or(EngineError::NoSuchSnapshot { snap_id })?;
        if snap.record.vol != vol || self.snap_deletes.contains(&snap_id) {
            return Err(EngineError::NoSuchSnapshot { snap_id });
        }
        // A damaged snapshot (at boot, or quarantined since) is never
        // rolled back to: its damage would become the volume's.
        if snap.pinned.is_none() || snap.damage.is_some() {
            return Err(EngineError::SnapshotDamaged { snap_id });
        }
        let birth = snap.birth();
        let gen = self.check_vol_op(vol)?.rollback_gen.wrapping_add(1);
        let newer = self
            .snapshots
            .iter()
            .filter(|s| {
                s.record.vol == vol
                    && s.birth() > birth
                    && !self.snap_deletes.contains(&s.record.snap_id)
            })
            .count();
        if newer > 0 {
            return Err(EngineError::NewerSnapshots { vol, count: newer });
        }
        if self.snapshot_in_flight(vol) {
            return Err(EngineError::VolumeBusy { vol });
        }
        let v = &mut self.vols[vol as usize];
        v.op = Some(VolOp::Rollback { snap_id, gen });
        v.op_in = None;
        self.request_fold();
        Ok(())
    }

    /// Asks for `vol` to be resized to `size_blocks`: a grow needs the
    /// volume present and quiet, a shrink needs it offline with nothing in
    /// the arena (what lies beyond the new end is released). Done at the
    /// next consistency point, a fold ([`Event::VolumeResized`]).
    pub fn resize_volume(&mut self, vol: u32, size_blocks: u64) -> Result<(), EngineError> {
        check_size(vol, size_blocks)?;
        check_index_fits(&self.layout, size_blocks)?;
        let current = self
            .vols
            .get(vol as usize)
            .filter(|v| v.present && !v.creating)
            .ok_or(EngineError::NoSuchVolume { vol })?;
        if current.op.is_some() {
            return Err(EngineError::VolumeBusy { vol });
        }
        if size_blocks < current.size_blocks {
            self.check_vol_op(vol)?;
        } else if size_blocks == current.size_blocks {
            return Ok(());
        }
        if self.snapshot_in_flight(vol) {
            return Err(EngineError::VolumeBusy { vol });
        }
        let v = &mut self.vols[vol as usize];
        v.op = Some(VolOp::Resize { size_blocks });
        v.op_in = None;
        self.request_fold();
        Ok(())
    }

    /// Asks for `vol` to be deleted: offline, nothing in the arena, no
    /// snapshots. Done at the next consistency point, a fold
    /// ([`Event::VolumeDeleted`]): everything it mapped is released and
    /// its index may be reused.
    pub fn delete_volume(&mut self, vol: u32) -> Result<(), EngineError> {
        self.check_vol_op(vol)?;
        let count = self
            .snapshots
            .iter()
            .filter(|s| s.record.vol == vol && !self.snap_deletes.contains(&s.record.snap_id))
            .count();
        if count > 0 {
            return Err(EngineError::HasSnapshots { vol, count });
        }
        if self.snapshot_in_flight(vol) {
            return Err(EngineError::VolumeBusy { vol });
        }
        let v = &mut self.vols[vol as usize];
        v.op = Some(VolOp::Delete);
        v.op_in = None;
        self.request_fold();
        Ok(())
    }

    /// Creates a volume with a catalogue name (F16) at the lowest free
    /// index; returns the index. Serving once its CP commits, like
    /// [`create_volume`](Self::create_volume).
    pub fn create_named_volume(
        &mut self,
        name: &str,
        size_blocks: u64,
    ) -> Result<u32, EngineError> {
        validate_name(name.as_bytes())?;
        if self.find_volume(name).is_some() {
            return Err(EngineError::NameExists {
                name: name.to_string(),
            });
        }
        let vol = self
            .vols
            .iter()
            .position(|v| !v.present)
            .map(|i| i as u32)
            .ok_or(EngineError::NoSpace)?;
        self.create_volume(vol, size_blocks)?;
        self.vols[vol as usize].name = Some(name.to_string());
        Ok(vol)
    }

    /// Names, or renames, `vol` in the catalogue; durable with the next
    /// consistency point.
    pub fn set_volume_name(&mut self, vol: u32, name: &str) -> Result<(), EngineError> {
        validate_name(name.as_bytes())?;
        if !self.vols.get(vol as usize).is_some_and(|v| v.present) {
            return Err(EngineError::NoSuchVolume { vol });
        }
        if self.find_volume(name).is_some_and(|v| v != vol) {
            return Err(EngineError::NameExists {
                name: name.to_string(),
            });
        }
        self.vols[vol as usize].name = Some(name.to_string());
        self.request_cp();
        Ok(())
    }

    /// The catalogue name of `vol`, if any.
    #[must_use]
    pub fn volume_name(&self, vol: u32) -> Option<&str> {
        self.vols.get(vol as usize).and_then(|v| v.name.as_deref())
    }

    /// The volume named `name`, if any (present volumes, including those
    /// whose creation is still pending).
    #[must_use]
    pub fn find_volume(&self, name: &str) -> Option<u32> {
        self.vols
            .iter()
            .position(|v| v.present && v.name.as_deref() == Some(name))
            .map(|i| i as u32)
    }

    /// The catalogue name of snapshot `snap_id`, if any.
    #[must_use]
    pub fn snapshot_name(&self, snap_id: u64) -> Option<&str> {
        let snap = self
            .snapshots
            .iter()
            .find(|s| s.record.snap_id == snap_id)?;
        // The catalogue is kept in its canonical (key) order.
        let key = otap_format::snapshot_name_key(snap.record.vol, snap.record.snap_uuid);
        self.catalog
            .binary_search_by_key(&key, CatalogRec::sort_key)
            .ok()
            .map(|i| self.catalog[i].name())
    }

    /// The snapshot of `vol` named `name`, if any.
    #[must_use]
    pub fn find_snapshot(&self, vol: u32, name: &str) -> Option<u64> {
        let rec = self.catalog.iter().find(|c| {
            c.catalog_kind() == Some(CatalogKind::Snapshot) && c.vol == vol && c.name() == name
        })?;
        self.snapshots
            .iter()
            .find(|s| s.record.vol == vol && s.record.snap_uuid == rec.snap_uuid)
            .map(|s| s.record.snap_id)
    }

    /// Every present volume, with its state.
    #[must_use]
    pub fn volumes(&self) -> Vec<(u32, VolumeStatus)> {
        self.vols
            .iter()
            .enumerate()
            .filter(|(_, v)| v.present)
            .map(|(i, _)| (i as u32, self.volume(i as u32).expect("present")))
            .collect()
    }

    /// Walks the live map and every snapshot and counts, per data
    /// segment, the blocks referenced; reports every segment whose count
    /// differs from the engine's live count. Empty means the accounting
    /// is exact. A checker's and the support bundle's tool: it reads every
    /// map page of every tree.
    pub fn audit_liveness(&mut self) -> Result<Vec<LivenessDrift>, EngineError> {
        let mut found: HashMap<u64, u64> = HashMap::new();
        let mut seen: HashSet<(u64, u32)> = HashSet::new();
        let vols: Vec<u32> = self.map.volumes().map(|(v, _)| v).collect();
        for vol in vols {
            let mut entries = Vec::new();
            {
                let Engine { map, backstore, .. } = self;
                map.diff(&mut BackstoreReader(backstore), vol, 0, |_, e| {
                    entries.push(e)
                })?;
            }
            for e in entries {
                if e.is_data() && seen.insert((e.lseg(), e.off())) {
                    *found.entry(e.lseg()).or_insert(0) += 1;
                }
            }
        }
        let pinned: Vec<(u64, PinnedRoot)> = self
            .snapshots
            .iter()
            .filter_map(|s| s.pinned.clone().map(|p| (s.record.snap_id, p)))
            .collect();
        for (snap_id, root) in pinned {
            let mut entries = Vec::new();
            let damaged = {
                let Engine { map, backstore, .. } = self;
                map.pinned_diff_tolerant(&mut BackstoreReader(backstore), &root, 0, |_, e| {
                    entries.push(e)
                })?
            };
            // A damaged page's blocks cannot be found: the audit reports
            // the drift, and the snapshot is quarantined.
            if !damaged.is_empty() {
                self.quarantine_snapshot(snap_id, format!("map pages {damaged:?} do not verify"));
            }
            for e in entries {
                if e.is_data() && seen.insert((e.lseg(), e.off())) {
                    *found.entry(e.lseg()).or_insert(0) += 1;
                }
            }
        }
        let mut out = Vec::new();
        let mut segs: Vec<u64> = self
            .data_live
            .keys()
            .copied()
            .chain(found.keys().copied())
            .collect();
        segs.sort_unstable();
        segs.dedup();
        for pseg in segs {
            let counted = self.data_live.get(&pseg).copied().unwrap_or(0);
            let got = found.get(&pseg).copied().unwrap_or(0);
            if counted != got {
                out.push(LivenessDrift {
                    pseg,
                    counted,
                    found: got,
                });
            }
        }
        Ok(out)
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

    /// Quarantines snapshot `snap_id` (Step 6A): a walk found one of its
    /// map pages damaged. It stays listed and keeps pinning its data, can
    /// be deleted, and is no longer read, diffed or rolled back to; the
    /// pool status warns ([`OperatorWarning::SnapshotDamaged`]). The
    /// consistency points of the pool go on. The mark lives in memory: the
    /// boot checks a snapshot's root page and index only, and the next walk
    /// that meets the page quarantines the snapshot again.
    fn quarantine_snapshot(&mut self, snap_id: u64, reason: String) {
        quarantine(
            &mut self.snapshots,
            &mut self.events,
            &mut self.warnings,
            snap_id,
            reason,
        );
    }

    /// The snapshot `snap_id` if it may be read: present, its tree loaded
    /// at boot and not quarantined since.
    fn readable_snapshot(&self, snap_id: u64) -> Result<(&SnapState, PinnedRoot), EngineError> {
        let snap = self
            .snapshots
            .iter()
            .find(|s| s.record.snap_id == snap_id)
            .ok_or(EngineError::NoSuchSnapshot { snap_id })?;
        match (&snap.pinned, &snap.damage) {
            (Some(pinned), None) => Ok((snap, pinned.clone())),
            _ => Err(EngineError::SnapshotDamaged { snap_id }),
        }
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
                flags: s.record.pins(),
                name: self.snapshot_name(s.record.snap_id).map(str::to_string),
                deleting: self.snap_deletes.contains(&s.record.snap_id),
            })
            .collect()
    }

    /// Reads `dst.len() / 4096` blocks at `lba` of snapshot `snap_id`: the
    /// volume as it was at the snapshot's CP, whatever happened since. A
    /// damaged snapshot is refused ([`EngineError::SnapshotDamaged`]), and
    /// a map page found damaged here quarantines it.
    pub fn snapshot_read(
        &mut self,
        snap_id: u64,
        lba: u64,
        dst: &mut [u8],
    ) -> Result<Vec<PeekSource>, EngineError> {
        if !dst.len().is_multiple_of(BLOCK_SIZE) {
            return Err(EngineError::Io(IoError::BadLength { bytes: dst.len() }));
        }
        let blocks = (dst.len() / BLOCK_SIZE) as u64;
        let pinned = self.readable_snapshot(snap_id)?.1;
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
            let got = {
                let Engine { map, backstore, .. } = self;
                map.pinned_get(&mut BackstoreReader(backstore), &pinned, lba + k)
            };
            let entry = match got {
                Ok(entry) => entry,
                Err(MapError::Format(e)) => {
                    self.quarantine_snapshot(snap_id, format!("a map page does not verify: {e}"));
                    return Err(EngineError::SnapshotDamaged { snap_id });
                }
                Err(e) => return Err(e.into()),
            };
            sources.push(self.read_entry(pinned.vol, lba + k, entry, out)?);
        }
        Ok(sources)
    }

    /// Every block of snapshot `snap_id` born after `base` (another
    /// snapshot of the same volume, older, or none): what a delta ships. A
    /// damaged snapshot is refused ([`EngineError::SnapshotDamaged`]), and
    /// a map page found damaged here quarantines it: a delta that leaves
    /// out a page's blocks is never returned.
    pub fn snapshot_diff(
        &mut self,
        snap_id: u64,
        base: Option<u64>,
    ) -> Result<Vec<(u64, MapEntry)>, EngineError> {
        let (snap, pinned) = self.readable_snapshot(snap_id)?;
        let birth = u64::from(snap.record.birth_cp);
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
        let walked = {
            let Engine { map, backstore, .. } = self;
            map.pinned_diff(
                &mut BackstoreReader(backstore),
                &pinned,
                base_cp,
                |lba, e| out.push((lba, e)),
            )
        };
        match walked {
            Ok(()) => Ok(out),
            Err(MapError::Format(e)) => {
                self.quarantine_snapshot(snap_id, format!("a map page does not verify: {e}"));
                Err(EngineError::SnapshotDamaged { snap_id })
            }
            Err(e) => Err(e.into()),
        }
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
        let segments = self.segments.segments();
        let free = self.segments.free_count();
        PoolStatus {
            cp_seq: self.cp_seq,
            fold_cp: self.map.fold_cp(),
            cps_since_fold: self.map.cps_since_fold(),
            log_bytes_since_fold: self.map.log_bytes_since_fold(),
            dirty_pages: self.map.dirty_pages(),
            overlay_entries: self.map.overlay_entries(),
            free_segments: free,
            segments,
            reserve_segments: self.reserve,
            utilization: self.utilization(),
            admissible_blocks: self
                .free_data_blocks()
                .saturating_sub(self.index.unflushed_blocks()),
            unflushed_blocks: self.index.unflushed_blocks(),
            arena_occupancy: self.arena.occupancy(),
            flush_policy: profile.flush_policy,
            flush_opaque: profile.geom.flush_opaque,
            discard: profile.discard,
            warnings: self.warnings.clone(),
            snapshots: self.snapshots.len(),
            pool_full: self.pool_full,
            serving: self.serving,
            volumes: self.vols.iter().filter(|v| v.present).count(),
            catalog_entries: self.catalog.len(),
            snapshot_table_deltas: self.snap_chain.deltas.len(),
            catalog_deltas: self.catalog_chain.deltas.len(),
            cleaned_segments: self.stats.cleaned,
            relocated_blocks: self.stats.relocated,
            verify: self.verify.as_ref().map(|v| v.summary()),
        }
    }

    /// The committed catalogue, in canonical order: what the catalogue's
    /// chain on disk replays to.
    #[must_use]
    pub fn catalog_records(&self) -> &[CatalogRec] {
        &self.catalog
    }

    /// The committed snapshot records, in id order: what the snapshot
    /// table's chain on disk replays to.
    #[must_use]
    pub fn snapshot_records(&self) -> Vec<SnapRecord> {
        self.snapshots.iter().map(|s| s.record).collect()
    }

    /// The chains of the snapshot table and the catalogue as the committed
    /// CP left them (F20).
    #[must_use]
    pub fn table_chains(&self) -> (&TableChain, &TableChain) {
        (&self.snap_chain, &self.catalog_chain)
    }

    /// The tables the consistency point in flight will commit, once its
    /// metadata is laid out: `(cp_seq, catalogue, snapshot records)`.
    /// `None` while no CP is past its staging. What a checker compares a
    /// boot with when the crash fell between the metadata and the commit
    /// (the slot may or may not have landed).
    #[must_use]
    pub fn in_flight_tables(&self) -> Option<(u64, Vec<CatalogRec>, Vec<SnapRecord>)> {
        let prep = match &self.state {
            State::Meta(mw) => &mw.prep,
            State::Flush { prep, .. }
            | State::CommitA { prep, .. }
            | State::CommitB { prep, .. } => prep.as_ref(),
            State::Idle | State::Staging(_) => return None,
        };
        Some((
            prep.job.cp_seq,
            prep.catalog.clone(),
            prep.snap_records.clone(),
        ))
    }

    /// Segments in use over segments.
    fn utilization(&self) -> f64 {
        let segments = self.segments.segments();
        if segments == 0 {
            return 1.0;
        }
        f64::from(segments - self.segments.free_count()) / f64::from(segments)
    }

    /// Data blocks the pool can still take from clients before the reserve
    /// (S5-3): the free segments beyond the reserve, plus what is left of
    /// the open data segment.
    fn free_data_blocks(&self) -> u64 {
        let per_stripe = u64::from(self.layout.data_blocks_per_stripe());
        let per_segment = per_stripe * u64::from(self.layout.stripes_per_segment);
        let free = u64::from(self.segments.free_count().saturating_sub(self.reserve));
        let open = self.open_data.map_or(0, |o| {
            u64::from(self.layout.stripes_per_segment.saturating_sub(o.next_idx)) * per_stripe
        });
        free * per_segment + open
    }

    /// The admission rule: a write of `blocks` is taken only when every
    /// acknowledged block not yet in a consistency point fits, with it,
    /// in the space outside the reserve.
    fn admits(&self, blocks: u32) -> bool {
        self.index.unflushed_blocks() + u64::from(blocks) <= self.free_data_blocks()
    }

    /// The reserve in force.
    #[must_use]
    pub const fn reserve_segments(&self) -> u32 {
        self.reserve
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

    /// `true` when no consistency point is in flight, none is waiting to
    /// start (no frozen generation, no request, no boot retry) and no
    /// paced acknowledgement is held: what a harness waits for after a
    /// request, so the CP it sees commit is the one that took the request.
    #[must_use]
    pub fn is_quiet(&self) -> bool {
        matches!(self.state, State::Idle)
            && self.index.frozen_len() == 0
            && !self.cp_requested
            && !self.boot_retry
            && self.paced.is_empty()
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
            offline: v.offline,
            name: v.name.clone(),
            op_pending: v.op.as_ref().map(|op| op.to_string()),
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
        // A paced write was placed, and the stop that ended the engine
        // carried it (a clean shutdown) or kept it in the arena: it is as
        // done as any acknowledged write.
        for (req, seq, _) in std::mem::take(&mut self.paced) {
            self.front.complete(req, Status::Ok, seq);
        }
    }

    /// Serves one request; `Some(req)` hands it back to be retried later.
    fn serve(&mut self, req: Request) -> Result<Option<Request>, EngineError> {
        let vol = req.vol();
        let Some(v) = self
            .vols
            .get(vol as usize)
            .filter(|v| v.present && !v.creating)
            .map(|v| (v.nvfail, v.offline, v.size_blocks))
        else {
            self.front.complete(req, Status::NoSuchVolume, 0);
            return Ok(None);
        };
        let (nvfail, offline, size_blocks) = v;
        if offline {
            self.front.complete(req, Status::Offline, 0);
            return Ok(None);
        }
        if nvfail.is_some() {
            self.front.complete(req, Status::NvFailed, 0);
            return Ok(None);
        }
        let in_range = |lba: u64, blocks: u32| {
            blocks > 0
                && lba
                    .checked_add(u64::from(blocks))
                    .is_some_and(|end| end <= size_blocks)
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
                if self.pool_full || !self.admits(blocks) {
                    if !self.pool_full {
                        self.stats.admission_refusals += 1;
                    }
                    self.front.complete(req, Status::NoSpace, 0);
                    return Ok(None);
                }
                // The hard stall: at or above the stall occupancy a write
                // waits, unplaced, for a consistency point to drain the
                // arena (S5-1).
                if self.arena.occupancy() >= self.cfg.pacing.stall_at {
                    return Ok(Some(req));
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
    /// mode needs one, and after the pacing delay when the arena is past
    /// the pacing occupancy (S5-1).
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
        let delay = self.cfg.pacing.delay_ns(self.arena.occupancy());
        if delay > 0 || !self.paced.is_empty() {
            // Later acknowledgements never pass earlier ones.
            let due = self.clock.mono_ns() + delay;
            let due = self.paced.back().map_or(due, |(_, _, d)| due.max(*d));
            self.stats.paced += 1;
            self.paced.push_back((req, seq, due));
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
                // The replication writes' volumes this intent carried are
                // covered now; one placed since waits for the next.
                for vol in set.iter() {
                    let _ = self.apply_intent.clear(vol);
                }
                if !self.pending_acks.is_empty() || !self.apply_intent.is_empty() {
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
        set.union_with(&self.apply_intent);
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
                self.cleaner_skip.remove(&pseg);
                self.stats.recycled += 1;
                self.pump_cps = 0;
                self.clean_blocked_at = None;
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
        let mut job = CpJob {
            snaps: Vec::new(),
            fold_requested: false,
            policy_fold: false,
            clean: Vec::new(),
            clean_requested: false,
            cp_seq,
            nonce,
            trigger,
            gen,
            blocks,
            holes,
            max_seq,
            run_state,
        };
        self.plan_clean(&mut job);
        job
    }

    /// Decides what the cleaner does in `job` (E37): nothing in a boot or
    /// shutdown commit, nothing while a snapshot deletion or a volume
    /// operation waits (their accounting would cross), nothing below the
    /// utilisation bound unless asked; else the best candidates, scanned,
    /// as many as the policy and the free space allow.
    fn plan_clean(&mut self, job: &mut CpJob) {
        if matches!(job.trigger, CpTrigger::Boot | CpTrigger::Shutdown) || self.fold_damaged {
            return;
        }
        if !self.snap_deletes.is_empty() || self.vols.iter().any(|v| v.op.is_some()) {
            return;
        }
        let requested = self.clean_requested;
        let tight = self.pool_tight();
        if !requested
            && !(self.cfg.cleaner.background
                && (tight || self.utilization() > self.cfg.cleaner.utilization))
        {
            return;
        }
        // The last cleaning CP found no segment and nothing was freed
        // since: another attempt would fail the same way and spend the
        // reserve on its stripes. The request stands and the plan waits.
        if self
            .clean_blocked_at
            .is_some_and(|at| self.free_segments() <= at)
        {
            return;
        }
        job.clean_requested = requested;
        let budget = if tight {
            self.cfg.cleaner.max_segments_per_cp.saturating_mul(2)
        } else {
            self.cfg.cleaner.max_segments_per_cp
        };
        let excluded = self.cleaner_excluded();
        let candidates = cleaner::candidates(
            &self.segments,
            &self.layout,
            |p| self.data_live.get(&p).copied().unwrap_or(0),
            &excluded,
            self.cfg.cleaner.max_live_fraction,
        );
        let per_segment = u64::from(self.layout.data_blocks_per_stripe())
            * u64::from(self.layout.stripes_per_segment);
        let blocks_per_segment = u64::from(self.layout.blocks_per_segment()).max(1);
        let replaced: HashSet<(u32, u64)> = job.blocks.iter().map(|b| (b.vol, b.lba)).collect();
        let holes = job.holes.clone();
        let mut taken = 0u32;
        let mut planned_blocks = 0u64;
        let mut planned_meta = 0u64;
        // Segments the relocation stripes and the rewrites' metadata take,
        // with one to spare for the CP's own metadata: everything planned
        // must fit outside the hard floor.
        let fits = |free: u32, data: u64, meta: u64| {
            let needed = data.div_ceil(per_segment) + meta.div_ceil(blocks_per_segment) + 1;
            u64::from(free) >= needed + u64::from(META_RESERVE_SEGMENTS)
        };
        for (pseg, counted_live, _) in candidates {
            if taken >= budget {
                break;
            }
            if !fits(
                self.segments.free_count(),
                planned_blocks + counted_live,
                planned_meta,
            ) {
                break;
            }
            let scanned = {
                let Engine {
                    backstore,
                    map,
                    snapshots,
                    layout,
                    cfg,
                    ..
                } = self;
                let mut refs = EngineRefs {
                    map,
                    backstore: &mut *backstore,
                    snapshots,
                    replaced: &replaced,
                    holes: &holes,
                };
                cleaner::scan(layout, &cfg.agg_id, pseg, &mut refs)
            };
            match scanned {
                Ok(scan) => {
                    // Fewer referenced blocks than the table counts: a
                    // stripe the scan could not read as written holds
                    // blocks it cannot vouch for, and draining the segment
                    // would free them.
                    if scan.referenced < counted_live {
                        self.cleaner_skip.insert(pseg);
                        self.events.push(Event::CleanSkipped {
                            pseg,
                            reason: format!(
                                "the scan found {} referenced blocks where the table counts \
                                 {counted_live}",
                                scan.referenced
                            ),
                        });
                        continue;
                    }
                    let meta = self.rewrite_cost(&scan.blocks);
                    if !fits(
                        self.segments.free_count(),
                        planned_blocks + scan.blocks.len() as u64,
                        planned_meta + meta,
                    ) {
                        break;
                    }
                    planned_blocks += scan.blocks.len() as u64;
                    planned_meta += meta;
                    taken += 1;
                    job.clean.push(CleanTarget {
                        pseg,
                        blocks: scan.blocks,
                        counted_live,
                    });
                }
                Err(reason) => {
                    self.cleaner_skip.insert(pseg);
                    // A snapshot page the scan could not read: the snapshot
                    // is quarantined (Step 6A); the segment waits.
                    if let SkipReason::DamagedSnapshot { snap_id } = &reason {
                        self.quarantine_snapshot(
                            *snap_id,
                            "a map page does not verify (found by the cleaner)".to_string(),
                        );
                    }
                    self.events.push(Event::CleanSkipped {
                        pseg,
                        reason: reason.to_string(),
                    });
                }
            }
        }
    }

    /// The metadata blocks the rewrites for relocating `blocks` take
    /// (E37): a copy of every map page a relocated block touches, once per
    /// distinct pinned tree (snapshots that share a tree share the copy);
    /// a new index region per distinct tree; a root page per snapshot; and
    /// the records of the live map's deltas.
    fn rewrite_cost(&self, blocks: &[cleaner::RelocBlock]) -> u64 {
        let page_blocks = (otap_format::MAP_PAGE_SIZE / BLOCK_SIZE) as u64;
        // (tree, page) pairs, a tree named by its index region.
        let mut pages: HashSet<(u64, u64)> = HashSet::new();
        let mut trees: HashMap<u64, u64> = HashMap::new(); // tree -> index blocks
        let mut snaps: HashSet<u64> = HashSet::new();
        let mut live_deltas = 0u64;
        for block in blocks {
            if block.live {
                live_deltas += 1;
            }
            for snap_id in &block.snaps {
                snaps.insert(*snap_id);
                let Some(pinned) = self
                    .snapshots
                    .iter()
                    .find(|s| s.record.snap_id == *snap_id)
                    .and_then(|s| s.pinned.as_ref())
                else {
                    continue;
                };
                let tree = otap_map::index_root_ref(&pinned.root).pba;
                pages.insert((tree, block.lba / otap_format::MAP_ENTRIES_PER_PAGE as u64));
                trees.entry(tree).or_insert_with(|| {
                    ((pinned.index.len() * otap_format::INDEX_ENTRY_SIZE) as u64)
                        .div_ceil(BLOCK_SIZE as u64)
                        .max(1)
                });
            }
        }
        pages.len() as u64 * page_blocks
            + trees.values().sum::<u64>()
            + snaps.len() as u64
            + (live_deltas * 32).div_ceil(BLOCK_SIZE as u64)
    }

    fn start_job(&mut self, job: CpJob) -> Result<(), EngineError> {
        if job.cp_seq > MAX_CP_SEQ {
            return Err(EngineError::Internal("CP sequence exhausted".into()));
        }
        self.job_data_segments.clear();
        let staging = Staging {
            job,
            next: 0,
            reloc_next: 0,
            inflight: HashMap::new(),
            deltas: Vec::new(),
            pending: None,
            stripes: Vec::new(),
            failed: None,
            reloc_failed: None,
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
        self.snap_requests.splice(0..0, job.snaps.iter().cloned());
        self.fold_requested |= job.fold_requested;
        self.clean_requested |= job.clean_requested;
        // A cleaning CP that found no segment would fail the same way until
        // something is freed, and every attempt costs segments: the plan
        // waits for a free (E37).
        if no_space && !job.clean.is_empty() {
            self.clean_blocked_at = Some(self.free_segments());
        }
        // The data segments this attempt allocated hold nothing a committed
        // CP names: they go back at once, so the retry does not pay for
        // them (the dead-data pass would reclaim them only after a later
        // commit).
        for pseg in std::mem::take(&mut self.job_data_segments) {
            if self.open_data.is_some_and(|o| o.pseg == pseg) {
                self.open_data = None;
            }
            match self.segments.free(pseg) {
                Ok(()) => {
                    self.data_live.remove(&pseg);
                    self.cleaner_skip.remove(&pseg);
                    self.events.push(Event::SegmentFreed {
                        pseg,
                        discarded: false,
                    });
                }
                Err(e) => self.events.push(Event::Internal {
                    what: format!("returning segment {pseg} of a failed consistency point: {e}"),
                }),
            }
        }
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
        if let Some((pseg, reason)) = st.reloc_failed.take() {
            // A block of a segment being cleaned could not be read: the
            // segment is left alone from now on and the CP is retried
            // without it (its stripes so far land in a segment nothing
            // names).
            if st.inflight.is_empty() {
                self.cleaner_skip.insert(pseg);
                self.events.push(Event::CleanSkipped {
                    pseg,
                    reason: reason.clone(),
                });
                return Ok(self.fail_cp(&st.job, format!("relocation: {reason}"), false));
            }
            st.reloc_failed = Some((pseg, reason));
            return Ok(State::Staging(st));
        }
        let max_inflight = usize::from(self.backstore.profile().inflight_stripes.clamp(1, 64));
        let reloc_total = st.job.reloc_blocks();
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
            } else if st.pending.is_none()
                && st.next == st.job.blocks.len()
                && st.reloc_next < reloc_total
            {
                match self.build_reloc_stripe(&mut st.job, st.reloc_next) {
                    Ok((next, pba, image, entries)) => {
                        st.reloc_next = next;
                        st.pending = Some((pba, image, entries));
                    }
                    Err(RelocError::NoSpace) => {
                        return Ok(self.fail_cp(&st.job, "no free segment".into(), true));
                    }
                    Err(RelocError::Read { pseg, reason }) => {
                        st.reloc_failed = Some((pseg, reason));
                        return Ok(State::Staging(st));
                    }
                    Err(RelocError::Engine(e)) => return Err(e),
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
                    if let Some((pseg, idx)) = self.layout.stripe_of_pba(pba) {
                        let kind = if st.next == st.job.blocks.len() && st.reloc_next > 0 {
                            StripeKind::Relocate
                        } else {
                            StripeKind::Ingest
                        };
                        // The kind is read back from the image's header,
                        // which is authoritative.
                        let kind = StripeHdr::read(&image[..otap_format::STRIPE_HDR_SIZE])
                            .ok()
                            .and_then(|h| h.stripe_kind())
                            .unwrap_or(kind);
                        if kind == StripeKind::Relocate {
                            self.stats.reloc_stripes += 1;
                        }
                        if let Ok(entry) = CpStripe::new(pseg, idx, kind) {
                            st.stripes.push(entry);
                        }
                    }
                }
                Err(Busy) => {
                    st.pending = Some((pba, image, entries));
                    break;
                }
            }
        }
        if st.next == st.job.blocks.len()
            && st.reloc_next == reloc_total
            && st.inflight.is_empty()
            && st.pending.is_none()
        {
            let st = *st;
            return match self.prepare_meta(st.job, st.deltas, st.stripes) {
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
                            // A table fold the policy asked for is put off
                            // like the map's (F20), and a CP that is no
                            // map fold is retried at once without it: a
                            // policy choice does not make the pool full.
                            let free = self.free_segments();
                            let mut table_fold_put_off = false;
                            for (table, droppable) in std::mem::take(&mut self.table_folds_tried) {
                                if droppable {
                                    table_fold_put_off = true;
                                    let chain = match table {
                                        TableKind::Snapshots => &mut self.snap_chain,
                                        TableKind::Catalog => &mut self.catalog_chain,
                                    };
                                    chain.fold_blocked_at = Some(free);
                                }
                            }
                            let no_space = if job.folds() {
                                !shutdown_fold
                            } else {
                                !table_fold_put_off
                            };
                            Ok(self.fail_cp(&job, "no free segment".into(), no_space))
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

    /// Builds the next relocation stripe from the blocks `job.clean` lists
    /// at flat position `from` and on: each block is read from its old
    /// address, verified against its CRC (a block that fails is never
    /// moved: its segment is skipped, `RelocError::Read`), and placed under
    /// its old birth and CRC at a new address recorded in the block. The entries returned are the live map's
    /// deltas (snapshot-only blocks have none; their pages are rewritten in
    /// the metadata phase).
    fn build_reloc_stripe(
        &mut self,
        job: &mut CpJob,
        from: usize,
    ) -> Result<(usize, u64, Vec<u8>, Vec<Delta>), RelocError> {
        let per_stripe = self.layout.data_blocks_per_stripe() as usize;
        let total = job.reloc_blocks();
        let n = per_stripe.min(total - from);
        let open = self
            .open_data_segment(job.cp_seq as u32)
            .map_err(|e| match e {
                EngineError::NoSpace => RelocError::NoSpace,
                other => RelocError::Engine(other),
            })?;
        let (pseg, idx) = (open.pseg, open.next_idx);
        let stripe_bytes = self.layout.stripe_bytes as usize;
        let mut image = vec![0u8; stripe_bytes];
        let mut descs = Vec::with_capacity(n);
        let mut entries = Vec::with_capacity(n);
        for i in 0..n {
            let (target, k) = flat_index(&job.clean, from + i);
            let old = job.clean[target].blocks[k].old;
            let (vol, lba) = (
                job.clean[target].blocks[k].vol,
                job.clean[target].blocks[k].lba,
            );
            let source = self
                .layout
                .block_pba(old.lseg(), old.off())
                .ok_or_else(|| {
                    RelocError::Engine(EngineError::Internal(format!(
                        "relocated block {vol}/{lba} is off the device"
                    )))
                })?;
            let at = self.layout.summary.block_offset(i);
            if let Err(e) = self.backstore.read(source, &mut image[at..at + BLOCK_SIZE]) {
                return Err(RelocError::Read {
                    pseg: job.clean[target].pseg,
                    reason: format!("block {vol}/{lba} at {source}: {e}"),
                });
            }
            // A block that no longer matches its CRC is not moved: copied
            // under its recorded CRC it would make this CP's relocation
            // stripe fail the unclean boot's data check under a trusted
            // opaque barrier, and the boot would rewind past a sound CP
            // (the Step 5 review's hazard 8, the Step 6 review's D1). Its
            // segment is skipped like one with an unreadable block, so the
            // damage stays where it is and keeps reading `Corrupt`.
            let computed = crc32c(&image[at..at + BLOCK_SIZE]);
            if computed != old.crc32c {
                return Err(RelocError::Read {
                    pseg: job.clean[target].pseg,
                    reason: format!(
                        "block {vol}/{lba} at {source} fails its CRC \
                         (computed {computed:#010x}, recorded {:#010x})",
                        old.crc32c
                    ),
                });
            }
            descs.push(
                StripeDescriptor::data(vol, lba, old.birth_cp, old.crc32c)
                    .map_err(|e| RelocError::Engine(e.into()))?,
            );
            let off = self.layout.block_off(idx, i as u32);
            let new = MapEntry::data(pseg, off, SECTORS_PER_BLOCK, old.birth_cp, old.crc32c)
                .map_err(|e| RelocError::Engine(e.into()))?;
            let block = &mut job.clean[target].blocks[k];
            block.new = Some(new);
            if block.live {
                entries.push((vol, lba, new));
            }
        }
        let mut hdr = StripeHdr::new(self.cfg.agg_id, StripeKind::Relocate);
        hdr.cp_seq = job.cp_seq;
        hdr.cp_nonce = job.nonce;
        hdr.lseg = pseg;
        hdr.stripe_idx = idx;
        hdr.nblocks = n as u16;
        hdr.group = 0;
        let summary_len = self.layout.summary.summary_bytes;
        otap_format::write_summary(&mut image[..summary_len], &hdr, &descs)
            .map_err(|e| RelocError::Engine(e.into()))?;
        let pba = self.layout.stripe_pba(pseg, idx).ok_or_else(|| {
            RelocError::Engine(EngineError::Internal(format!(
                "stripe {idx} of segment {pseg} is off the device"
            )))
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
        self.job_data_segments.push(pseg);
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
        // A volume operation folds, but not in a CP that relocates blocks:
        // the operation waits for the next CP, with the deletions (E37; a
        // rollback built from a root the same CP rewrites would name the
        // drained segment).
        let ops_here = job.clean.is_empty() && self.vols.iter().any(|v| v.op.is_some());
        if self.fold_requested
            || self
                .snap_requests
                .iter()
                .any(|r| self.snap_eligible(r, job))
            || ops_here
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
        written: Vec<CpStripe>,
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
                && job.run_state != RunState::CleanShutdown
                && !(job.clean.is_empty() && self.vols.iter().any(|v| v.op.is_some()));
        }
        match self.prepare_meta_inner(&job, deltas, fold, written) {
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
        written: Vec<CpStripe>,
    ) -> Result<(PreparedParts, MetaStripeQueue), EngineError> {
        let cp32 = job.cp_seq as u32;
        // The relocation deltas come first: a block the generation wrote
        // over wins ("the last entry for a block wins").
        let mut reloc_deltas: Vec<Delta> = Vec::new();
        for target in &job.clean {
            for block in &target.blocks {
                let new = block.new.ok_or_else(|| {
                    EngineError::Internal(format!(
                        "relocated block {}/{} was never written",
                        block.vol, block.lba
                    ))
                })?;
                if block.live {
                    reloc_deltas.push((block.vol, block.lba, new));
                }
            }
        }
        reloc_deltas.append(&mut deltas);
        let mut deltas = reloc_deltas;
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
        // The volume operations this CP carries (a fold): a rollback to a
        // snapshot that still exists and reads, a resize, a deletion. The
        // volume is offline with nothing in the arena, so no delta of its
        // own can be in this CP; one would be the engine's bug.
        let mut vol_ops: Vec<(u32, VolOp)> = Vec::new();
        let mut rollbacks = Vec::new();
        let mut resizes = Vec::new();
        let mut deletes = Vec::new();
        // A CP that relocates blocks carries no volume operation and no
        // snapshot deletion (E37): the cleaner planned against the trees as
        // they were, a rollback would take a root this CP rewrites, and a
        // deletion would remove a tree the rewrites below need. What
        // arrived after the plan waits for the next CP, which plans no
        // clean while it is pending.
        let ops_here = fold && job.clean.is_empty();
        if ops_here {
            for (i, v) in self.vols.iter().enumerate() {
                let vol = i as u32;
                let Some(op) = &v.op else {
                    continue;
                };
                // A grow keeps the volume online, so its deltas ride along;
                // everything else needed the volume offline and quiet.
                let grow =
                    matches!(op, VolOp::Resize { size_blocks } if *size_blocks >= v.size_blocks);
                if !grow && deltas.iter().any(|(dv, _, _)| *dv == vol) {
                    return Err(EngineError::Internal(format!(
                        "volume {vol} has deltas in the CP that carries its {op}"
                    )));
                }
                match op {
                    VolOp::Rollback { snap_id, .. } => {
                        let snap = self
                            .snapshots
                            .iter()
                            .find(|s| s.record.snap_id == *snap_id && s.record.vol == vol);
                        // A rollback accepted before its snapshot was
                        // quarantined goes ahead: the volume takes the tree
                        // with its damaged page, which reads `Corrupt` (E32);
                        // a new request is refused (`rollback_volume`).
                        let Some(pinned) = snap.and_then(|s| s.pinned.clone()) else {
                            // The snapshot went (deleted, or lost to a
                            // rewind): the rollback cannot happen and is
                            // dropped at commit.
                            vol_ops.push((vol, op.clone()));
                            continue;
                        };
                        rollbacks.push(Rollback {
                            vol,
                            birth_cp: snap.map_or(0, SnapState::birth),
                            root: pinned.root,
                            index: pinned.index,
                        });
                    }
                    VolOp::Resize { size_blocks } => resizes.push((vol, *size_blocks)),
                    VolOp::Delete => deletes.push(vol),
                }
                vol_ops.push((vol, op.clone()));
            }
        }
        let input = CpInput {
            cp_seq: job.cp_seq,
            cp_nonce: job.nonce,
            deltas,
            fold,
            new_volumes: new_volumes.clone(),
            max_region_pages: self.layout.blocks_per_segment(),
            rollbacks,
            resizes,
            deletes: deletes.clone(),
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
        // The live indexes after this CP, for the "is this page still
        // referenced" questions below.
        let live_after: HashMap<u32, (otap_format::MapRoot, Vec<PageIndexEntry>)> = {
            let mut m: HashMap<u32, (otap_format::MapRoot, Vec<PageIndexEntry>)> = self
                .map
                .volumes()
                .filter(|(v, _)| !deletes.contains(v))
                .map(|(v, m)| (v, (*m.root(), m.index().to_vec())))
                .collect();
            for (vol, root, index) in &prepared.fold_roots {
                m.insert(*vol, (*root, index.clone()));
            }
            for (vol, root, index) in prepared
                .update
                .is_fold()
                .then_some(&prepared.fold_roots)
                .into_iter()
                .flatten()
            {
                m.insert(*vol, (*root, index.clone()));
            }
            m
        };

        // Snapshot deletions (E36): processed in id order against the set
        // as it shrinks. For each, the blocks only it referenced: its
        // entries born after the next older snapshot of the volume (what
        // the older one shares, it keeps) that the next newer snapshot, or
        // the live map when there is none, does not name.
        let mut remaining: Vec<SnapState> = self.snapshots.clone();
        let mut deleted_snapshots: Vec<(u64, u32, u64)> = Vec::new();
        let mut released_blocks: Vec<MapEntry> = Vec::new();
        let mut freed_candidates: Vec<(u32, u64, RootRef)> = Vec::new(); // pages: (vol, page_no, root)
        let mut freed_indexes: Vec<(u32, RootRef)> = Vec::new();
        let mut freed_other: Vec<RootRef> = Vec::new();
        let mut deletes_sorted = if job.clean.is_empty() {
            self.snap_deletes.clone()
        } else {
            // Deferred to the next CP, with the volume operations (above).
            Vec::new()
        };
        deletes_sorted.sort_unstable();
        // Snapshots a walk found damaged (Step 6A): quarantined once the
        // deletions are laid out.
        let mut quarantined: Vec<(u64, String)> = Vec::new();
        for snap_id in deletes_sorted {
            let Some(pos) = remaining.iter().position(|s| s.record.snap_id == snap_id) else {
                continue;
            };
            let snap = remaining.remove(pos);
            let vol = snap.record.vol;
            let birth = snap.birth();
            let older = remaining
                .iter()
                .filter(|s| s.record.vol == vol && s.birth() < birth)
                .map(SnapState::birth)
                .max()
                .unwrap_or(0);
            let newer_state = remaining
                .iter()
                .filter(|s| s.record.vol == vol && s.birth() > birth)
                .min_by_key(|s| s.birth());
            // A damaged newer neighbour (its root cannot be walked) may
            // still name any of these blocks: nothing is released, the
            // safe direction, and the drift is recorded. So does an
            // uncertain one (F21): the tree it holds may not be its last.
            let newer_damaged =
                newer_state.is_some_and(|s| s.pinned.is_none() || s.record.is_uncertain());
            let newer_id = newer_state.map(|s| s.record.snap_id);
            let newer = newer_state.and_then(|s| s.pinned.clone());
            // An uncertain snapshot (F21) may be one whose deletion or
            // relocation was lost with a damaged table: its blocks and
            // pages may already be released and reused, so its deletion
            // releases and frees nothing (a leak, the safe direction).
            let uncertain = snap.record.is_uncertain();
            let walk = if uncertain {
                self.events.push(Event::Internal {
                    what: format!(
                        "snapshot {snap_id} is deleted uncertain (recovered from a damaged \
                         snapshot table): its blocks and pages stay counted"
                    ),
                });
                DeletionWalk::default()
            } else if snap.pinned.is_some() && newer_damaged {
                self.events.push(Event::Internal {
                    what: format!(
                        "snapshot {snap_id} is deleted next to a damaged newer snapshot: its \
                         blocks stay counted"
                    ),
                });
                DeletionWalk::default()
            } else {
                let walked = {
                    let Engine { map, backstore, .. } = self;
                    Self::deletion_walk(
                        map,
                        backstore,
                        &snap,
                        older,
                        newer.as_ref(),
                        deletes.contains(&vol),
                    )
                };
                match walked {
                    Ok(walk) => walk,
                    Err(e) => {
                        // A read error, which may be transient: this
                        // deletion waits for the next CP, and this CP goes
                        // on without it (the pool's other work is not held
                        // hostage by one snapshot's tree).
                        self.events.push(Event::Internal {
                            what: format!(
                                "the deletion of snapshot {snap_id} waits for the next \
                                 consistency point: {e}"
                            ),
                        });
                        remaining.insert(pos, snap);
                        continue;
                    }
                }
            };
            if !walk.damaged_pages.is_empty() {
                self.events.push(Event::Internal {
                    what: format!(
                        "snapshot {snap_id} is deleted with damaged map pages {:?}: the blocks \
                         they name stay counted",
                        walk.damaged_pages
                    ),
                });
                quarantined.push((
                    snap_id,
                    format!("map pages {:?} do not verify", walk.damaged_pages),
                ));
            }
            if let Some(id) = newer_id.filter(|_| walk.newer_damaged) {
                quarantined.push((id, "a map page does not verify".to_string()));
            }
            let released = walk.released.len() as u64;
            released_blocks.extend(walk.released);
            // Only what the engine counts is freed: the root page, the
            // index and the pages of a snapshot whose root loaded (a boot
            // counts them as live; a damaged root's never were, and freeing
            // them would take bytes from another region's count).
            if let Some(pinned) = snap.pinned.as_ref().filter(|_| !uncertain) {
                freed_other.push(RootRef::new(snap.record.map_root_pba, 1, 0));
                for (page_no, entry) in pinned.index.iter().enumerate() {
                    if !entry.is_none() {
                        freed_candidates.push((
                            vol,
                            page_no as u64,
                            otap_map::page_root_ref(entry),
                        ));
                    }
                }
                freed_indexes.push((vol, otap_map::index_root_ref(&pinned.root)));
            } else if !uncertain {
                self.events.push(Event::Internal {
                    what: format!(
                        "snapshot {snap_id} is deleted damaged: its blocks and pages stay counted"
                    ),
                });
            }
            deleted_snapshots.push((snap_id, vol, released));
        }
        {
            let Engine {
                snapshots,
                events,
                warnings,
                ..
            } = self;
            for (snap_id, reason) in quarantined {
                quarantine(snapshots, events, warnings, snap_id, reason);
            }
        }

        // The cleaner's rewrites of snapshot trees (E37): every snapshot
        // that referenced a relocated block gets the new address in a
        // copy-on-write page, a new index and a new root page.
        let mut rewritten: Vec<Rewritten> = Vec::new();
        let mut extra_pages: Vec<RootRef> = Vec::new();
        {
            let mut changes: BTreeMap<u64, Vec<(u64, MapEntry, MapEntry)>> = BTreeMap::new();
            for target in &job.clean {
                for block in &target.blocks {
                    if let Some(new) = block.new {
                        for snap_id in &block.snaps {
                            changes
                                .entry(*snap_id)
                                .or_default()
                                .push((block.lba, block.old, new));
                        }
                    }
                }
            }
            // Snapshots that share a tree (the same index region, as folds
            // leave them when nothing changed between them) are rewritten
            // once and point at the one copy, so a clean never un-shares a
            // tree and the metadata is paid once per tree, not per
            // snapshot.
            let mut groups: Vec<RewriteGroup> = Vec::new();
            for (snap_id, list) in changes {
                let Some(pos) = remaining.iter().position(|s| s.record.snap_id == snap_id) else {
                    return Err(EngineError::Internal(format!(
                        "relocation for snapshot {snap_id}, which is gone"
                    )));
                };
                let Some(pinned) = remaining[pos].pinned.as_ref() else {
                    return Err(EngineError::Internal(format!(
                        "relocation for damaged snapshot {snap_id}"
                    )));
                };
                let tree = otap_map::index_root_ref(&pinned.root);
                match groups
                    .iter_mut()
                    .find(|(t, _, changes)| *t == tree && *changes == list)
                {
                    Some((_, ids, _)) => ids.push(snap_id),
                    None => groups.push((tree, vec![snap_id], list)),
                }
            }
            for (_, ids, list) in groups {
                let Some(first) = ids
                    .first()
                    .and_then(|id| remaining.iter().find(|s| s.record.snap_id == *id))
                    .and_then(|s| s.pinned.clone())
                else {
                    continue;
                };
                let result = {
                    let Engine { map, backstore, .. } = self;
                    map.rewrite_pinned(&mut BackstoreReader(backstore), &mut stream, &first, &list)?
                };
                extra_pages.extend(result.pages.iter().copied());
                let mut new_pages = Some(result.new_pages);
                for snap_id in ids {
                    let Some(pos) = remaining.iter().position(|s| s.record.snap_id == snap_id)
                    else {
                        continue;
                    };
                    let mut record = remaining[pos].record;
                    let page = SnapRootPage::new(
                        self.cfg.agg_id,
                        snap_id,
                        u64::from(record.birth_cp),
                        result.pinned.root,
                        RootRef::NONE,
                    )?;
                    let page_ref = stream.place_region(page.as_bytes())?;
                    // Listed with the CP's pages (F21), so a boot verifies
                    // it as this commit's own: lost, it tears the commit
                    // rather than reading as a damaged snapshot.
                    extra_pages.push(page_ref);
                    let old_root_page = RootRef::new(record.map_root_pba, 1, 0);
                    record.map_root_pba = page_ref.pba;
                    record.seal();
                    // A relocation moves blocks, it repairs nothing: a
                    // quarantined snapshot stays quarantined (the Step 6
                    // review's D8).
                    let state = SnapState {
                        record,
                        pinned: Some(result.pinned.clone()),
                        damage: remaining[pos].damage.clone(),
                    };
                    remaining[pos] = state.clone();
                    // The old pages and index are nominated for every
                    // snapshot of the group (the free rule deduplicates by
                    // address); the new pages are cached once.
                    rewritten.push(Rewritten {
                        snap_id,
                        state,
                        freed_pages: result.freed_pages.clone(),
                        freed_index: result.freed_index,
                        old_root_page,
                        new_pages: new_pages.take().unwrap_or_default(),
                    });
                }
            }
        }

        // Pins: the changes waiting become the records' flags (an
        // uncertain record keeps its mark, F21).
        let mut pins: Vec<(u64, u8)> = Vec::new();
        for (snap_id, flags) in &self.pin_changes {
            if let Some(s) = remaining.iter_mut().find(|s| s.record.snap_id == *snap_id) {
                s.record.flags = (s.record.flags & SNAP_UNCERTAIN) | *flags;
                s.record.seal();
                pins.push((*snap_id, *flags));
            }
        }

        // Snapshots asked for: pinned at this fold.
        let mut new_snapshots = Vec::new();
        let wall = self.clock.wall_ns();
        if fold {
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
                new_snapshots.push(SnapState {
                    record,
                    pinned: Some(PinnedRoot::from_parts(req.vol, *root, index.clone())),
                    damage: None,
                });
            }
        }
        // The snapshots after this CP: what pins pages and blocks from now
        // on.
        let after: Vec<SnapState> = remaining
            .iter()
            .chain(new_snapshots.iter())
            .cloned()
            .collect();
        let mut snap_records: Vec<SnapRecord> = after.iter().map(|s| s.record).collect();
        snap_records.sort_by_key(|r| r.snap_id);

        // The snapshot table (F19, F20): this CP's changes are its deletions
        // (tombstones), its new snapshots, its pin changes and the cleaner's
        // rewrites (whole records, as they stand after this CP).
        let mut snap_changes: BTreeMap<u64, SnapRecord> = BTreeMap::new();
        for (snap_id, _, _) in &deleted_snapshots {
            if let Some(s) = self.snapshots.iter().find(|s| s.record.snap_id == *snap_id) {
                snap_changes.insert(
                    *snap_id,
                    SnapRecord::tombstone(*snap_id, s.record.snap_uuid, s.record.vol)?,
                );
            }
        }
        let changed_ids: Vec<u64> = new_snapshots
            .iter()
            .map(|s| s.record.snap_id)
            .chain(pins.iter().map(|(id, _)| *id))
            .chain(rewritten.iter().map(|rw| rw.snap_id))
            .collect();
        for id in changed_ids {
            if let Ok(i) = snap_records.binary_search_by_key(&id, |r| r.snap_id) {
                snap_changes.insert(id, snap_records[i]);
            }
        }
        let snap_changes: Vec<SnapRecord> = snap_changes.into_values().collect();

        // The catalogue (F16, F19): the names of the volumes present after
        // this CP and of the snapshots after it, derived from the committed
        // catalogue; this CP's changes are what differs.
        let mut names: BTreeMap<(u8, u32, [u8; 16]), CatalogRec> =
            self.catalog.iter().map(|c| (c.sort_key(), *c)).collect();
        let mut name_changes: BTreeMap<(u8, u32, [u8; 16]), CatalogRec> = BTreeMap::new();
        for (i, v) in self.vols.iter().enumerate() {
            let vol = i as u32;
            let key = (CatalogKind::Volume.as_u8(), vol, *Uuid::NIL.as_array());
            let existing = names.get(&key).copied();
            let wanted = match (&v.name, v.present && !deletes.contains(&vol)) {
                (Some(name), true) => Some(CatalogRec::volume(
                    vol,
                    name,
                    existing.map_or(wall, |c| c.created_wall_ns),
                )?),
                _ => None,
            };
            match (existing, wanted) {
                (Some(old), Some(new)) if old == new => {}
                (_, Some(new)) => {
                    names.insert(key, new);
                    name_changes.insert(key, new);
                }
                (Some(old), None) => {
                    names.remove(&key);
                    name_changes.insert(key, old.tombstone_of()?);
                }
                (None, None) => {}
            }
        }
        // Snapshot names change only with the snapshot set (a snapshot is
        // named when it is taken and its name leaves with it); a repair
        // checks them all.
        if !deleted_snapshots.is_empty() || !new_snapshots.is_empty() || self.catalog_chain.repair {
            let present: HashSet<(u32, Uuid)> = after
                .iter()
                .map(|s| (s.record.vol, s.record.snap_uuid))
                .collect();
            let gone: Vec<(u8, u32, [u8; 16])> = names
                .iter()
                .filter(|(_, c)| {
                    c.catalog_kind() == Some(CatalogKind::Snapshot)
                        && !present.contains(&(c.vol, c.snap_uuid))
                })
                .map(|(k, _)| *k)
                .collect();
            for key in gone {
                if let Some(old) = names.remove(&key) {
                    name_changes.insert(key, old.tombstone_of()?);
                }
            }
            for s in &new_snapshots {
                let Some(name) = job
                    .snaps
                    .iter()
                    .find(|r| r.uuid == s.record.snap_uuid)
                    .and_then(|r| r.name.as_deref())
                else {
                    continue;
                };
                let rec = CatalogRec::snapshot(s.record.vol, s.record.snap_uuid, name, wall)?;
                names.insert(rec.sort_key(), rec);
                name_changes.insert(rec.sort_key(), rec);
            }
        }
        let catalog: Vec<CatalogRec> = names.into_values().collect();
        let name_changes: Vec<CatalogRec> = name_changes.into_values().collect();

        // Each table writes its changes as a delta region and a new head,
        // or folds, or carries its head forward (F20).
        // A pool with a name or a snapshot keeps the snapshot table's head
        // (empty or not), so a format 2.3 reader always meets it and
        // refuses the pool (F20); a pool with neither writes no table, as
        // 2.3 did.
        self.table_folds_tried.clear();
        let free_now = self.segments.free_count();
        let snap_chain = table_step(
            &TableStep {
                agg_id: self.cfg.agg_id,
                policy: self.cfg.table_fold,
                job,
                free: free_now,
            },
            &mut stream,
            &self.snap_chain,
            !snap_records.is_empty() || !catalog.is_empty(),
            &snap_changes,
            &snap_records,
            &mut freed_other,
            &mut self.table_folds_tried,
        )?;
        let catalog_chain = table_step(
            &TableStep {
                agg_id: self.cfg.agg_id,
                policy: self.cfg.table_fold,
                job,
                free: free_now,
            },
            &mut stream,
            &self.catalog_chain,
            !catalog.is_empty(),
            &name_changes,
            &catalog,
            &mut freed_other,
            &mut self.table_folds_tried,
        )?;

        // The stripe list (F17): every data stripe this CP wrote.
        let cp_stripes = if written.is_empty() {
            RootRef::NONE
        } else {
            let mut list = written;
            list.sort_by_key(|s| (s.pseg, s.idx));
            list.dedup_by_key(|s| (s.pseg, s.idx));
            stream.place_region(&cp_stripes_image(&list)?)?
        };

        // The roots table and the directory (with the pages the rewrites
        // wrote), then the segment table, whose live bytes include its own
        // region.
        let map_roots = stream.place_region(&map_roots_image(&prepared.roots_table)?)?;
        let mut dir = prepared.dir.clone();
        dir.pages.extend(extra_pages);
        let cp_log_head = stream.place_region(&dir.encode(self.cfg.agg_id)?)?;
        let table_pages = (stream.segments().image().len() / BLOCK_SIZE) as u32;
        let table_pba = stream.reserve(table_pages)?;

        // Data liveness after this CP (E24, E36, E37). A relocated block
        // moves: one less at the old address, one more at the new. A
        // replaced block is counted at its new address and leaves the old
        // one unless a snapshot that remains after this CP still names it
        // (one taken at or after its birth sees it, since nothing wrote
        // the block between). A released block (a rollback, a shrink, a
        // deletion) leaves under the same rule, and a block a deleted
        // snapshot alone referenced leaves outright.
        let mut data_live = self.data_live.clone();
        let dec = |data_live: &mut HashMap<u64, u64>, lseg: u64| match data_live.get_mut(&lseg) {
            Some(n) if *n > 0 => *n -= 1,
            _ => {}
        };
        for target in &job.clean {
            for block in &target.blocks {
                if let Some(new) = block.new {
                    dec(&mut data_live, block.old.lseg());
                    *data_live.entry(new.lseg()).or_insert(0) += 1;
                }
            }
        }
        for Replaced { vol, old, new, .. } in &prepared.replaced {
            let relocation = old.is_data()
                && new.is_data()
                && old.birth_cp == new.birth_cp
                && old.crc32c == new.crc32c
                && old.addr != new.addr;
            if relocation {
                continue;
            }
            if new.is_data() {
                *data_live.entry(new.lseg()).or_insert(0) += 1;
            }
            // A snapshot this very CP takes sees the new entry, never the
            // old: only the snapshots that already exist (and remain) can
            // pin what is replaced or released here.
            if old.is_data() && !pinned_by(&remaining, *vol, old.birth_cp) {
                dec(&mut data_live, old.lseg());
            }
        }
        for (vol, _, e) in &prepared.released {
            if e.is_data() && !pinned_by(&remaining, *vol, e.birth_cp) {
                dec(&mut data_live, e.lseg());
            }
        }
        for e in &released_blocks {
            dec(&mut data_live, e.lseg());
        }
        // A drained segment holds nothing anyone names: its count is zero
        // whatever the walk found, and a difference is reported.
        for target in &job.clean {
            let left = data_live.get(&target.pseg).copied().unwrap_or(0);
            if left != 0 {
                self.events.push(Event::Internal {
                    what: format!(
                        "segment {} counted {} live blocks after draining ({} found, {} before)",
                        target.pseg,
                        left,
                        target.blocks.len(),
                        target.counted_live
                    ),
                });
            }
            data_live.insert(target.pseg, 0);
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
        // fold drops, and every page, index and root page some tree stops
        // referencing, unless another tree (the live map after this CP, a
        // snapshot that remains) still does. One rule for every nominee,
        // whoever nominated it, and nothing freed twice.
        let page_referenced = |vol: u32, page_no: u64, pba: u64| -> bool {
            live_after.get(&vol).is_some_and(|(_, index)| {
                index
                    .get(page_no as usize)
                    .is_some_and(|e| !e.is_none() && e.page_pba == pba)
            }) || after.iter().any(|s| {
                s.pinned
                    .as_ref()
                    .is_some_and(|p| p.vol == vol && p.references_page(page_no, pba))
            })
        };
        let index_referenced = |vol: u32, pba: u64| -> bool {
            live_after
                .get(&vol)
                .is_some_and(|(root, _)| root.index_pages > 0 && root.index_pba == pba)
                || after
                    .iter()
                    .any(|s| s.pinned.as_ref().is_some_and(|p| p.references_index(pba)))
        };
        let mut freed = self.table_regions.clone();
        freed.extend_from_slice(&prepared.freed_deltas);
        freed.extend(freed_other);
        for fp in &prepared.freed_pages {
            freed_candidates.push((fp.vol, fp.page_no, fp.root));
        }
        for fi in &prepared.freed_indexes {
            freed_indexes.push((fi.vol, fi.root));
        }
        for rw in &rewritten {
            for fp in &rw.freed_pages {
                freed_candidates.push((fp.vol, fp.page_no, fp.root));
            }
            freed_indexes.push((rw.freed_index.vol, rw.freed_index.root));
            freed.push(rw.old_root_page);
        }
        for (vol, page_no, root) in freed_candidates {
            if !page_referenced(vol, page_no, root.pba) {
                freed.push(root);
            }
        }
        for (vol, root) in freed_indexes {
            if !index_referenced(vol, root.pba) {
                freed.push(root);
            }
        }
        freed.sort_by_key(|r| (r.pba, r.pages));
        freed.dedup_by_key(|r| r.pba);
        let table_regions = vec![map_roots, cp_log_head, cp_stripes, seg_table]
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
        for (vol, _) in &vol_ops {
            if let Some(v) = self.vols.get_mut(*vol as usize) {
                if v.op_in.is_none() {
                    v.op_in = Some(job.cp_seq);
                }
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
            snap_chain,
            snap_records,
            catalog,
            catalog_chain,
            cp_stripes,
            new_snapshots,
            deleted_snapshots,
            pins,
            rewritten,
            vol_ops,
            carried,
            fold,
            fold_pages,
        };
        Ok((prep, stripes.into_iter().collect()))
    }

    /// The blocks snapshot `snap` alone references (E36): its entries born
    /// after `older` (what the next older snapshot shares, it keeps) that
    /// the next newer snapshot `newer`, or the live map as committed when
    /// there is none (nothing, when the volume is deleted in this CP), does
    /// not name. A page of the snapshot that does not verify is walked past
    /// and reported, never a reason to fail the CP (Step 6A); an answer the
    /// newer snapshot or the live map cannot give is "still named".
    ///
    /// # Errors
    /// A read error, which may be transient: the caller lets the deletion
    /// wait for the next CP.
    fn deletion_walk(
        map: &mut MapSet,
        backstore: &mut B,
        snap: &SnapState,
        older: u64,
        newer: Option<&PinnedRoot>,
        vol_deleted: bool,
    ) -> Result<DeletionWalk, MapError> {
        let mut walk = DeletionWalk::default();
        let Some(pinned) = &snap.pinned else {
            return Ok(walk);
        };
        let vol = snap.record.vol;
        let mut mine = Vec::new();
        walk.damaged_pages = map.pinned_diff_tolerant(
            &mut BackstoreReader(&mut *backstore),
            pinned,
            older,
            |lba, e| mine.push((lba, e)),
        )?;
        for (lba, e) in mine {
            if !e.is_data() {
                continue;
            }
            let backstore = &mut *backstore;
            let still = match newer {
                Some(n) => match map.pinned_get(&mut BackstoreReader(backstore), n, lba) {
                    Ok(ne) => ne == e,
                    // An unreadable page: the safe answer is that it may
                    // still name the block (and that snapshot is damaged).
                    Err(MapError::Format(_)) => {
                        walk.newer_damaged = true;
                        true
                    }
                    // Beyond the newer snapshot's end (the volume shrank
                    // between them): the block is this snapshot's alone.
                    Err(MapError::OutOfRange { .. }) => false,
                    Err(err) => return Err(err),
                },
                // The live map, as committed (this CP's own replacements
                // are counted by the replaced loop, against the remaining
                // set).
                None if vol_deleted => false,
                None => match map.get(&mut BackstoreReader(backstore), vol, lba) {
                    Ok(le) => le == e,
                    Err(MapError::NoSuchVolume { .. }) => false,
                    Err(MapError::Format(_)) => true,
                    // Beyond the volume's current end (it shrank since the
                    // snapshot): the snapshot's alone.
                    Err(MapError::OutOfRange { .. }) => false,
                    Err(err) => return Err(err),
                },
            };
            if !still {
                walk.released.push(e);
            }
        }
        Ok(walk)
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
                        // The boot's hold is durable in this slot's volume
                        // table: the arena may now forget what caused it
                        // (the attach's rewrites, a creation's format).
                        if prep.job.trigger == CpTrigger::Boot {
                            self.arena.apply_pending_writes();
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
        sb.snap_table = prep.snap_chain.head;
        sb.snap_head_cp = prep.snap_chain.head_cp;
        sb.catalog = prep.catalog_chain.head;
        sb.catalog_head_cp = prep.catalog_chain.head_cp;
        sb.cp_stripes = prep.cp_stripes;
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
            let v = self.vols[vol as usize].clone();
            let row = &mut slot.vols.vols[vol as usize];
            if !v.present || !prep.carried.contains(&vol) {
                *row = VolState::ABSENT;
                continue;
            }
            row.flags = VOL_PRESENT;
            if v.offline {
                row.flags |= VOL_OFFLINE;
            }
            row.acked_bytes = self.unflushed_bytes_after(vol, committed_gen);
            // A rollback this CP carries moves the generation on, like a
            // clear does.
            let rolled = prep.vol_ops.iter().find_map(|(ov, op)| match op {
                VolOp::Rollback { gen, .. } if *ov == vol => Some(*gen),
                _ => None,
            });
            match v.clearing {
                Some(c) => {
                    // The clear rides this CP: the row shows the volume
                    // cleared on its new generation.
                    row.rollback_gen = rolled.unwrap_or(c.gen);
                    row.nvfail_cause = 0;
                }
                None => {
                    row.rollback_gen = rolled.unwrap_or(v.rollback_gen);
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
            catalog,
            catalog_chain,
            snap_chain,
            new_snapshots,
            deleted_snapshots,
            pins,
            rewritten,
            vol_ops,
            fold,
            fold_pages,
            slot,
            ..
        } = prep;
        let gen = job.gen.clone();
        // The data segments this CP allocated are committed with it.
        self.job_data_segments.clear();
        self.map.apply(map_update);
        self.segments = segments;
        self.catalog = catalog;
        self.catalog_chain = catalog_chain;
        self.snap_chain = snap_chain;
        self.last_slot = slot.map(Box::new);
        // Snapshots: the deleted leave, the rewritten take their new roots,
        // the pins land. The cleaner's skip list is about the snapshot set,
        // which changed if any of these happened.
        let snapshot_set_changed =
            !deleted_snapshots.is_empty() || !rewritten.is_empty() || !new_snapshots.is_empty();
        for (snap_id, vol, released) in &deleted_snapshots {
            self.snapshots.retain(|s| s.record.snap_id != *snap_id);
            self.snap_deletes.retain(|id| id != snap_id);
            self.pin_changes.retain(|(id, _)| id != snap_id);
            // A damaged snapshot's warning goes with it.
            self.warnings.retain(|w| {
                !matches!(w, OperatorWarning::SnapshotDamaged { snap_id: id, .. } if id == snap_id)
            });
            self.stats.snapshots_deleted += 1;
            self.events.push(Event::SnapshotDeleted {
                snap_id: *snap_id,
                vol: *vol,
                cp_seq: job.cp_seq,
                released: *released,
            });
        }
        for rw in rewritten {
            if let Some(s) = self
                .snapshots
                .iter_mut()
                .find(|s| s.record.snap_id == rw.snap_id)
            {
                // Damage found while the CP was in flight (a read, a diff)
                // stays with the snapshot, as the rewrite's own does (D8).
                let found = s.damage.take();
                *s = rw.state;
                if s.damage.is_none() {
                    s.damage = found;
                }
            }
            self.map.cache_pages(rw.new_pages);
        }
        for (snap_id, flags) in &pins {
            if let Some(s) = self
                .snapshots
                .iter_mut()
                .find(|s| s.record.snap_id == *snap_id)
            {
                s.record.flags = (s.record.flags & SNAP_UNCERTAIN) | *flags;
                s.record.seal();
            }
            self.pin_changes
                .retain(|(id, f)| !(id == snap_id && f == flags));
            self.events.push(Event::SnapshotPinned {
                snap_id: *snap_id,
                flags: *flags,
            });
        }
        if snapshot_set_changed {
            self.cleaner_skip.clear();
        }
        // The segments the cleaner drained hold nothing anyone names.
        for target in &job.clean {
            self.stats.cleaned += 1;
            self.stats.relocated += target.blocks.len() as u64;
            self.events.push(Event::SegmentCleaned {
                pseg: target.pseg,
                relocated: target.blocks.len() as u64,
                cp_seq: job.cp_seq,
            });
        }
        if job.clean_requested {
            self.clean_requested = false;
        }
        // The volume operations this CP carried.
        for (vol, op) in vol_ops {
            let Some(v) = self.vols.get_mut(vol as usize) else {
                continue;
            };
            if v.op.as_ref() != Some(&op) || v.op_in != Some(job.cp_seq) {
                continue;
            }
            v.op = None;
            v.op_in = None;
            match op {
                VolOp::Rollback { snap_id, gen } => {
                    if let Some(m) = self.map.volume(vol).filter(|_| {
                        self.snapshots
                            .iter()
                            .any(|s| s.record.snap_id == snap_id && s.record.vol == vol)
                    }) {
                        // The volume is the snapshot's again, its size
                        // included.
                        v.size_blocks = m.lba_count();
                        v.rollback_gen = gen;
                        if let Some(c) = &mut v.clearing {
                            c.gen = gen;
                        }
                        self.stats.rollbacks += 1;
                        self.events.push(Event::VolumeRolledBack {
                            vol,
                            snap_id,
                            cp_seq: job.cp_seq,
                            rollback_gen: gen,
                        });
                    } else {
                        self.events.push(Event::Internal {
                            what: format!(
                                "the rollback of volume {vol} to snapshot {snap_id} was dropped: \
                                 the snapshot is gone"
                            ),
                        });
                    }
                }
                VolOp::Resize { size_blocks } => {
                    v.size_blocks = size_blocks;
                    self.events.push(Event::VolumeResized {
                        vol,
                        size_blocks,
                        cp_seq: job.cp_seq,
                    });
                }
                VolOp::Delete => {
                    *v = VolRt::default();
                    let _ = self.replicas.clear(vol);
                    self.events.push(Event::VolumeDeleted {
                        vol,
                        cp_seq: job.cp_seq,
                    });
                }
            }
        }
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
        self.snapshots.sort_by_key(|s| s.record.snap_id);
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
        self.release_freed(job.cp_seq, true);
        self.stats.cps += 1;
        self.last_commit_ns = self.clock.mono_ns();
        self.durable_seq = self.durable_seq.max(job.max_seq);
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
    /// so, discarded (D9). `from_commit` marks the release a commit makes:
    /// what it releases is what that commit's segment table still counted
    /// (a verification's view needs it); a release outside a commit (the
    /// end of a verification's holds) adds to that record.
    fn release_freed(&mut self, cp_seq: u64, from_commit: bool) {
        let keep = if self.backstore.profile().trusts_opaque_barrier() {
            2
        } else {
            1
        };
        self.boot_protect.retain(|(cp, _)| cp + keep + 1 > cp_seq);
        // A verification in progress attests the pool as CP `held`
        // committed it: what a later CP stopped referencing stays where it
        // is until the verification ends, so the view it walks is intact.
        let held = self.verify.as_ref().and_then(|v| v.holds_frees_after());
        let keeps = |cp: u64| cp + keep > cp_seq || held.is_some_and(|n| cp > n);
        // Data segments dead long enough that no rewind reaches a CP that
        // named their blocks.
        let mut dead_now = Vec::new();
        while let Some((cp, _)) = self.dead_data_pending.front() {
            if keeps(*cp) {
                break;
            }
            let (_, segs) = self.dead_data_pending.pop_front().expect("front exists");
            dead_now.extend(segs);
        }
        if from_commit {
            self.last_released.clear();
        }
        while let Some((cp, _)) = self.freed_pending.front() {
            if keeps(*cp) {
                break;
            }
            let (_, regions) = self.freed_pending.pop_front().expect("front exists");
            self.last_released.extend(regions.iter().copied());
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

/// [`Engine::quarantine_snapshot`] over the fields it touches, for the
/// places that hold other fields borrowed (the CP's metadata layout).
fn quarantine(
    snapshots: &mut [SnapState],
    events: &mut Vec<Event>,
    warnings: &mut Vec<OperatorWarning>,
    snap_id: u64,
    reason: String,
) {
    let Some(s) = snapshots.iter_mut().find(|s| s.record.snap_id == snap_id) else {
        return;
    };
    if s.damage.is_some() {
        return;
    }
    s.damage = Some(reason.clone());
    let warning = OperatorWarning::SnapshotDamaged {
        snap_id,
        vol: s.record.vol,
        reason,
    };
    events.push(Event::OperatorWarning(warning.clone()));
    warnings.push(warning);
}

/// `true` when a snapshot of `vol` among `snaps` was taken at or after the
/// CP that wrote an entry: the snapshot's root still names that entry
/// (nothing wrote the block between its birth and the snapshot, or the
/// entry would not be the one being replaced now).
fn pinned_by(snaps: &[SnapState], vol: u32, birth_cp: u32) -> bool {
    snaps
        .iter()
        .any(|s| s.record.vol == vol && s.record.birth_cp >= birth_cp)
}

/// What one table's step in a CP's metadata needs from the engine.
struct TableStep<'a> {
    agg_id: Uuid,
    policy: TableFoldPolicy,
    job: &'a CpJob,
    /// Free segments in the committed segment table (what a put-off fold
    /// waits to see grow).
    free: u32,
}

/// Lays out one table's part of a CP (F20). A table the pool does not need
/// (`wanted` false: no record, and for the snapshot table no name either)
/// has no head: an existing chain is dropped whole. Otherwise: nothing when
/// the table did not change (its head is carried forward); else a fold (a
/// base of `full`, the table as it stands after the CP, and an empty chain)
/// when the table has no head yet, the chain is full, the table needs its
/// repair, or the policy asks and the fold fits; else a delta region of
/// `changes` (keys ascending) appended to the chain. Either way a new head.
/// The regions the CP stops referencing go to `freed` (the old head; on a
/// fold the old base and deltas too), and a fold is recorded in `tried`
/// with whether the policy alone asked for it.
#[allow(clippy::too_many_arguments)]
fn table_step<R: TableRecord>(
    step: &TableStep<'_>,
    stream: &mut MetaStream<'_>,
    chain: &TableChain,
    wanted: bool,
    changes: &[R],
    full: &[R],
    freed: &mut Vec<RootRef>,
    tried: &mut Vec<(TableKind, bool)>,
) -> Result<TableChain, EngineError> {
    let job = step.job;
    let plan = plan_table(
        chain,
        &TablePlanInput {
            wanted,
            changes: changes.len() as u64,
            records: full.len() as u64,
            record_size: R::SIZE,
            free: step.free,
        },
        &step.policy,
        |pages| stream.can_place(pages),
    );
    let (fold, droppable, blocked_at) = match plan {
        TablePlan::Carry if chain.head.is_none() => return Ok(TableChain::default()),
        TablePlan::Carry => return Ok(chain.clone()),
        TablePlan::Drop => {
            // An empty table names nothing (as in format 2.3): its regions go.
            freed.extend(chain.regions());
            return Ok(TableChain::default());
        }
        TablePlan::Fold { policy } => (true, policy, None),
        TablePlan::Delta { put_off: true } => (false, false, Some(step.free)),
        TablePlan::Delta { put_off: false } => (
            false,
            false,
            chain.fold_blocked_at.filter(|at| step.free <= *at),
        ),
    };
    let mut next = TableChain {
        head: RootRef::NONE,
        head_cp: job.cp_seq,
        base: chain.base,
        deltas: chain.deltas.clone(),
        repair: false,
        fold_blocked_at: blocked_at,
    };
    if fold {
        tried.push((R::TABLE, droppable));
        let region = if full.is_empty() {
            RootRef::NONE
        } else {
            stream.place_region(&R::base_image(full)?)?
        };
        let records = u32::try_from(full.len())
            .map_err(|_| EngineError::Internal(format!("a table of {} records", full.len())))?;
        next.base = TableChainEntry::new(region, job.cp_seq, records);
        next.deltas.clear();
        freed.extend(chain.regions());
    } else {
        let region = stream.place_region(&table_delta_image(
            step.agg_id,
            job.cp_seq,
            job.nonce,
            changes,
        )?)?;
        let records = u32::try_from(changes.len()).map_err(|_| {
            EngineError::Internal(format!("a table delta of {} records", changes.len()))
        })?;
        next.deltas
            .push(TableChainEntry::new(region, job.cp_seq, records));
        if !chain.head.is_none() {
            freed.push(chain.head);
        }
    }
    let head = TableHead {
        table: R::TABLE,
        cp_seq: job.cp_seq,
        base: next.base,
        deltas: next.deltas.clone(),
    };
    next.head = stream.place_region(head.page(step.agg_id)?.as_bytes())?;
    Ok(next)
}

/// The `(target, block)` position of flat index `i` over a CP's clean
/// targets.
fn flat_index(targets: &[CleanTarget], mut i: usize) -> (usize, usize) {
    for (t, target) in targets.iter().enumerate() {
        if i < target.blocks.len() {
            return (t, i);
        }
        i -= target.blocks.len();
    }
    (targets.len().saturating_sub(1), 0)
}

/// Why a relocation stripe could not be built.
#[derive(Debug)]
enum RelocError {
    /// No free segment for the stripe.
    NoSpace,
    /// A block of the segment being cleaned could not be read.
    Read { pseg: u64, reason: String },
    /// The engine's own failure.
    Engine(EngineError),
}

/// The engine's answer to the cleaner's reference questions: the committed
/// live map, the snapshots' pinned pages, and the CP's own replacements.
struct EngineRefs<'a, B: Backstore> {
    map: &'a mut MapSet,
    backstore: &'a mut B,
    snapshots: &'a [SnapState],
    replaced: &'a HashSet<(u32, u64)>,
    holes: &'a [(u32, u64, u32)],
}

impl<B: Backstore> cleaner::References for EngineRefs<'_, B> {
    fn read(&mut self, pba: u64, dst: &mut [u8]) -> Result<(), IoError> {
        self.backstore.read(pba, dst)
    }

    fn live(&mut self, vol: u32, lba: u64, entry: &MapEntry) -> Result<bool, SkipReason> {
        match self.map.get(&mut BackstoreReader(self.backstore), vol, lba) {
            Ok(e) => Ok(e == *entry),
            Err(MapError::NoSuchVolume { .. }) | Err(MapError::OutOfRange { .. }) => Ok(false),
            Err(MapError::Format(_)) => Err(SkipReason::DamagedPage { vol }),
            Err(MapError::Io { reason, .. }) => Err(SkipReason::Summary(reason)),
            Err(e) => Err(SkipReason::Summary(e.to_string())),
        }
    }

    fn snapshots(&mut self, vol: u32, lba: u64, entry: &MapEntry) -> Result<Vec<u64>, SkipReason> {
        let mut out = Vec::new();
        for s in self.snapshots {
            if s.record.vol != vol || s.record.birth_cp < entry.birth_cp {
                continue;
            }
            let Some(pinned) = &s.pinned else {
                return Err(SkipReason::DamagedSnapshot {
                    snap_id: s.record.snap_id,
                });
            };
            if lba >= pinned.root.lba_count {
                continue;
            }
            match self
                .map
                .pinned_get(&mut BackstoreReader(self.backstore), pinned, lba)
            {
                Ok(e) if e == *entry => out.push(s.record.snap_id),
                Ok(_) => {}
                Err(MapError::Format(_)) => {
                    return Err(SkipReason::DamagedSnapshot {
                        snap_id: s.record.snap_id,
                    })
                }
                Err(e) => return Err(SkipReason::Summary(e.to_string())),
            }
        }
        Ok(out)
    }

    fn replaced_by_cp(&self, vol: u32, lba: u64) -> bool {
        self.replaced.contains(&(vol, lba))
            || self.holes.iter().any(|(v, start, blocks)| {
                *v == vol && lba >= *start && lba < start + u64::from(*blocks)
            })
    }
}
