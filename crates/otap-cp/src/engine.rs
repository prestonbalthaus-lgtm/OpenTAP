//! The engine: write and read paths, the consistency-point state machine,
//! boot and shutdown.

use core::fmt;
use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::Arc;

use otap_arena::{Arena, ArenaError, PlacedSlot, RingRef};
use otap_format::checksum::crc32c;
use otap_format::{
    boot_write_active, FormatError, IntentSlot, MapEntry, NvCause, RootRef, RunState, SbLocation,
    SbSlot, SegKind, StripeDescriptor, StripeHdr, StripeKind, Superblock, SuperblockSlot, Uuid,
    VolBitSet, VolState, BLOCK_SIZE, CP_LOG_RECS_PER_PAGE, MAX_CP_SEQ, MAX_LBA, MAX_VOLS,
    SECTORS_PER_BLOCK, SEG_ENTRY_SIZE, VOL_PRESENT,
};
use otap_index::{DirtyExtent, GenId, Generation, Index, IndexError};
use otap_sys::HugeArena;

use crate::blockmap::BlockMap;
use crate::boot::{self, BootError};
use crate::layout::{DiskLayout, LayoutError};
use crate::segments::{OpenSegment, SegmentError, SegmentTable};
use crate::traits::{
    Backstore, Busy, Clock, DevClass, Done, FrontDoor, IoError, NvfailIntent, Request, RequestKind,
    Status, Tag,
};

/// The longest request the engine serves, in blocks (8 MiB): bounds the
/// read buffer and the arena run one request can take.
pub const MAX_REQUEST_BLOCKS: u32 = 2048;

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
    /// The CP metadata (the whole map image plus the segment table, E14)
    /// could outgrow one segment with these volumes on this geometry.
    MetadataTooLarge {
        /// Stripes the metadata could need.
        stripes: u64,
        /// Stripes in a segment.
        stripes_per_segment: u32,
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
            Self::MetadataTooLarge {
                stripes,
                stripes_per_segment,
            } => write!(
                f,
                "the CP metadata could take {stripes} stripes; a segment has {stripes_per_segment}"
            ),
            Self::NvFailed { vol } => write!(f, "volume {vol} is held by nvfail"),
            Self::Corrupt { vol, lba } => write!(f, "volume {vol} LBA {lba} failed its CRC"),
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

/// Which phase the CP state machine is in.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Phase {
    /// No CP in progress.
    Idle,
    /// Writing data stripes.
    Staging,
    /// Writing the map image and the segment table.
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
    max_seq: u64,
    run_state: RunState,
}

type Delta = (u32, u64, MapEntry);

#[derive(Debug)]
struct Staging {
    job: CpJob,
    next: usize,
    inflight: HashMap<Tag, Vec<Delta>>,
    deltas: Vec<Delta>,
    pending: Option<(u64, Vec<u8>, Vec<Delta>)>,
    failed: Option<String>,
}

#[derive(Debug)]
struct Prepared {
    job: CpJob,
    next_map: BlockMap,
    seg_table: RootRef,
    cp_log_head: RootRef,
    /// The metadata segment this CP's regions live in.
    meta_pseg: u64,
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
    /// The CP whose slot first carried the creation, once one was built.
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
    /// Reads completed.
    pub reads: u64,
    /// Writes deferred because the arena was full.
    pub deferred: u64,
    /// Intent slots written.
    pub intents: u64,
    /// Metadata segments recycled.
    pub recycled: u64,
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
    map: BlockMap,
    segments: SegmentTable,
    open_data: Option<OpenSegment>,
    open_meta: Option<OpenSegment>,
    /// Metadata segments the last two committed CPs (and, at boot, every
    /// verified slot on the ring) name: never recycled.
    meta_protect: VecDeque<(u64, Vec<u64>)>,
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

/// Stripes the CP metadata takes at most with `total_blocks` mapped (the
/// whole map image plus the segment table, packed into whole stripes).
#[must_use]
pub fn metadata_stripes(layout: &DiskLayout, total_blocks: u64) -> u64 {
    let map_pages = total_blocks.div_ceil(CP_LOG_RECS_PER_PAGE as u64);
    let table_pages =
        ((u64::from(layout.segments) + 1) * SEG_ENTRY_SIZE as u64).div_ceil(BLOCK_SIZE as u64);
    (map_pages + table_pages)
        .saturating_mul(BLOCK_SIZE as u64)
        .div_ceil(u64::from(layout.stripe_bytes))
}

fn check_metadata_fits(layout: &DiskLayout, total_blocks: u64) -> Result<(), EngineError> {
    let stripes = metadata_stripes(layout, total_blocks);
    if stripes > u64::from(layout.stripes_per_segment) {
        return Err(EngineError::MetadataTooLarge {
            stripes,
            stripes_per_segment: layout.stripes_per_segment,
        });
    }
    Ok(())
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

impl<B: Backstore, F: FrontDoor, C: Clock> Engine<B, F, C> {
    /// Formats the device for `cfg`: wipes the superblock ring and the intent
    /// slots, then writes consistency point 0, clean, with the configured
    /// volumes present and nothing mapped. Drives the backstore until both
    /// slots of the ring are durable.
    pub fn format(backstore: &mut B, clock: &C, cfg: &EngineConfig) -> Result<(), EngineError> {
        let profile = *backstore.profile();
        if profile.geom.flush_opaque {
            return Err(EngineError::Unsupported("flush-opaque backstores (E19)"));
        }
        cfg.validate()?;
        let layout = DiskLayout::new(&profile.geom)?;
        check_metadata_fits(&layout, cfg.total_blocks())?;
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
        let nonce = nonce_for(cfg.agg_id, 0, clock.wall_ns(), clock.mono_ns(), 0);
        for location in [SbLocation::A, SbLocation::B] {
            let mut slot = SuperblockSlot::new(cfg.agg_id, 0, location);
            slot.sb.owner_epoch = cfg.owner_epoch;
            slot.sb.cp_nonce = nonce;
            slot.sb.wall_ns = clock.wall_ns();
            slot.sb.vol_count = vol_count;
            slot.sb.set_run_state(RunState::CleanShutdown);
            slot.vols.vols = rows;
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
        if backstore.profile().geom.flush_opaque {
            fail!(
                EngineError::Unsupported("flush-opaque backstores (E19)"),
                memory
            );
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
            ring_roots,
            ..
        } = state;
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
            }
        }
        for spec in &cfg.volumes {
            if let Some(v) = vols.get_mut(spec.vol as usize) {
                v.size_blocks = spec.size_blocks;
                if !v.present {
                    // The control plane knows a volume the disk does not:
                    // it is created with the boot commit.
                    v.present = true;
                    v.creating = true;
                }
            }
        }

        let index = Index::new(cfg.ring, now);
        let mut events = Vec::new();
        let mut replayed_blocks = 0u64;
        let mut orphaned_blocks = 0u64;
        let mut damaged_slots = 0u32;
        if let Some(report) = &reattach {
            damaged_slots = report.damaged.len() as u32;
            for (vol, lba, extent) in runs_of(&report.placed) {
                let known = vols.get(vol as usize).is_some_and(|v| v.present);
                if !known {
                    orphaned_blocks += u64::from(extent.blocks);
                    if let Err(e) = arena.commit(extent.ring, extent.blocks) {
                        events.push(Event::Internal {
                            what: format!("releasing an orphaned run: {e}"),
                        });
                    }
                    continue;
                }
                match index.insert(vol, lba, extent) {
                    Ok((_, superseded)) => {
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
                        replayed_blocks += u64::from(extent.blocks);
                    }
                    Err(e) => {
                        orphaned_blocks += u64::from(extent.blocks);
                        events.push(Event::Internal {
                            what: format!("replaying volume {vol} LBA {lba}: {e}"),
                        });
                        if let Err(e) = arena.commit(extent.ring, extent.blocks) {
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
        // Metadata segments any verified superblock on the ring names stay
        // until two more CPs have committed.
        let mut protected: Vec<u64> = ring_roots
            .iter()
            .filter_map(|r| layout.pseg_of_pba(r.pba))
            .collect();
        if let Some(open) = open_meta {
            protected.push(open.pseg);
        }
        protected.sort_unstable();
        protected.dedup();
        let mut meta_protect = VecDeque::new();
        meta_protect.push_back((sb.cp_seq, protected));

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
            meta_protect,
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
            if !self.handle_intent_done(done) {
                cp_dones.push(done);
            }
        }
        self.advance_cp(cp_dones)?;
        self.retry_intent()?;
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
    /// its table row commits ([`Event::VolumeCreated`]); requests to it
    /// answer `NoSuchVolume` until then, so no acknowledged write can
    /// outlive a volume that was never durable.
    pub fn create_volume(&mut self, vol: u32, size_blocks: u64) -> Result<(), EngineError> {
        check_size(vol, size_blocks)?;
        let total = self.total_blocks().saturating_add(size_blocks);
        check_metadata_fits(&self.layout, total)?;
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

    /// The committed map entry of a block, if any (for checkers and the
    /// support bundle).
    #[must_use]
    pub fn map_entry(&self, vol: u32, lba: u64) -> Option<MapEntry> {
        self.map.get(vol, lba)
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

    fn total_blocks(&self) -> u64 {
        self.vols
            .iter()
            .filter(|v| v.present)
            .fold(0u64, |sum, v| sum.saturating_add(v.size_blocks))
    }

    // ----------------------------------------------------------------------
    // The front door
    // ----------------------------------------------------------------------

    /// Serves requests: deferred writes first, in order, then new ones.
    /// Writes stay first-in first-out among themselves (a write behind a
    /// deferred one waits); reads and flushes never wait for the arena, so
    /// a stalled device cannot starve cache hits.
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
            let is_write = matches!(req.kind, RequestKind::Write { .. });
            if is_write && !self.deferred.is_empty() {
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
            .insert(vol, lba, DirtyExtent { ring, blocks, seq })
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
        if let Some(v) = self.vols.get_mut(vol as usize) {
            v.windows = 2;
        }
        if self.backstore.profile().nvfail_intent == NvfailIntent::Precise
            && !self.durable_active.get(vol)
        {
            self.intent_wanted = true;
            self.pending_acks.push((req, seq));
            return Ok(None);
        }
        self.front.complete(req, Status::Ok, seq);
        Ok(None)
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
                Err(EngineError::Corrupt { .. }) => {
                    self.front.complete(req, Status::Corrupt, 0);
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
    /// the CRC recorded when it was written), then the map.
    fn read_block(
        &mut self,
        vol: u32,
        lba: u64,
        out: &mut [u8],
    ) -> Result<PeekSource, EngineError> {
        if let Some(hit) = self.index.lookup(vol, lba) {
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
        match self.map.get(vol, lba) {
            Some(entry) if entry.is_data() => {
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
            }
            _ => {
                out.fill(0);
                Ok(PeekSource::Unmapped)
            }
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
        let mut max_seq = 0u64;
        if let Some(gen) = &gen {
            gen.for_each_dirty_volume(|vol, dirty| {
                for (lba, ext) in dirty.iter() {
                    max_seq = max_seq.max(ext.seq);
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
            cp_seq,
            nonce,
            trigger,
            gen,
            blocks,
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
    fn fail_cp(&mut self, job: &CpJob, reason: String, no_space: bool) -> State {
        self.events.push(Event::CpFailed {
            cp_seq: job.cp_seq,
            reason,
        });
        if no_space {
            self.pool_full = true;
            if self.shutdown_requested {
                self.stopped = true;
                self.serving = false;
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
                Err((EngineError::NoSpace, job)) => {
                    Ok(self.fail_cp(&job, "no free segment".into(), true))
                }
                Err((EngineError::MetadataTooLarge { .. }, job)) => {
                    Ok(self.fail_cp(&job, "metadata outgrew a segment".into(), false))
                }
                Err((e, _)) => Err(e),
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

    /// Lays out the CP's metadata; gives the job back on failure so the
    /// attempt can be recorded.
    fn prepare_meta(
        &mut self,
        job: CpJob,
        deltas: Vec<Delta>,
    ) -> Result<MetaWrite, (EngineError, CpJob)> {
        match self.prepare_meta_inner(&job, deltas) {
            Ok((next_map, seg_table, cp_log_head, meta_pseg, stripes)) => Ok(MetaWrite {
                prep: Prepared {
                    job,
                    next_map,
                    seg_table,
                    cp_log_head,
                    meta_pseg,
                    slot: None,
                },
                stripes,
                inflight: HashSet::new(),
                failed: None,
            }),
            Err(e) => Err((e, job)),
        }
    }

    #[allow(clippy::type_complexity)]
    fn prepare_meta_inner(
        &mut self,
        job: &CpJob,
        deltas: Vec<Delta>,
    ) -> Result<(BlockMap, RootRef, RootRef, u64, VecDeque<(u64, Vec<u8>)>), EngineError> {
        let mut next_map = self.map.clone();
        for (vol, lba, entry) in deltas {
            next_map.set(vol, lba, entry);
        }
        for pseg in self.segments.used_segments() {
            if self.segments.entry(pseg)?.seg_kind() == Some(SegKind::Data) {
                let live = next_map.live_blocks(pseg) * BLOCK_SIZE as u64;
                self.segments
                    .set_live_bytes(pseg, u32::try_from(live).unwrap_or(u32::MAX))?;
            }
        }
        let dump = next_map.dump(self.cfg.agg_id, job.cp_seq, job.nonce)?;
        let dump_pages = (dump.len() / BLOCK_SIZE) as u32;
        let table_pages = (self.segments.image().len() / BLOCK_SIZE) as u32;
        // Both regions are packed page after page into whole stripes.
        let needed = self.layout.stripes_for_pages(dump_pages + table_pages);
        if needed > self.layout.stripes_per_segment {
            return Err(EngineError::MetadataTooLarge {
                stripes: u64::from(needed),
                stripes_per_segment: self.layout.stripes_per_segment,
            });
        }
        let open = self.open_meta_segment(job.cp_seq as u32, needed)?;
        let pseg = open.pseg;
        let first_idx = open.next_idx;
        let end_idx = first_idx + needed;
        let written = u64::from(end_idx) * u64::from(self.layout.stripe_bytes);
        self.segments
            .set_live_bytes(pseg, u32::try_from(written).unwrap_or(u32::MAX))?;
        self.open_meta = Some(OpenSegment {
            pseg,
            next_idx: end_idx,
        });
        let table = self.segments.image();
        let first_pba = self
            .layout
            .stripe_pba(pseg, first_idx)
            .ok_or_else(|| EngineError::Internal("metadata region is off the device".into()))?;
        let cp_log_head = if dump.is_empty() {
            RootRef::NONE
        } else {
            RootRef::for_region(first_pba, &dump)?
        };
        let seg_table = RootRef::for_region(first_pba + u64::from(dump_pages), &table)?;
        let mut region = dump;
        region.extend_from_slice(&table);
        let mut stripes = VecDeque::new();
        self.push_region_stripes(&mut stripes, pseg, first_idx, &region);
        Ok((next_map, seg_table, cp_log_head, pseg, stripes))
    }

    fn push_region_stripes(
        &self,
        out: &mut VecDeque<(u64, Vec<u8>)>,
        pseg: u64,
        first_idx: u32,
        region: &[u8],
    ) {
        let stripe_bytes = self.layout.stripe_bytes as usize;
        for (k, chunk) in region.chunks(stripe_bytes).enumerate() {
            let mut image = vec![0u8; stripe_bytes];
            image[..chunk.len()].copy_from_slice(chunk);
            if let Some(pba) = self.layout.stripe_pba(pseg, first_idx + k as u32) {
                out.push_back((pba, image));
            }
        }
    }

    fn open_meta_segment(&mut self, cp_seq: u32, needed: u32) -> Result<OpenSegment, EngineError> {
        if let Some(open) = self.open_meta {
            if open.next_idx + needed <= self.layout.stripes_per_segment {
                return Ok(open);
            }
            self.segments.seal(open.pseg)?;
            self.open_meta = None;
        }
        let pseg =
            match self
                .segments
                .allocate(SegKind::Metadata, cp_seq.max(1), self.layout.stripe_bytes)
            {
                Ok(pseg) => pseg,
                Err(SegmentError::NoSpace) => return Err(EngineError::NoSpace),
                Err(e) => return Err(e.into()),
            };
        let open = OpenSegment { pseg, next_idx: 0 };
        self.open_meta = Some(open);
        Ok(open)
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
        let vol_count = self
            .vols
            .iter()
            .rposition(|v| v.present)
            .map_or(0, |i| i as u32 + 1);
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
            if !v.present {
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
        // Volumes and clears this slot carries become durable with it.
        for v in &mut self.vols {
            if v.creating && v.created_in.is_none() {
                v.created_in = Some(job.cp_seq);
            }
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

    /// The CP is durable: the map, the arena, the index, the volumes and
    /// the acknowledgements catch up. Nothing here can refuse; an
    /// inconsistency is recorded as [`Event::Internal`].
    fn finalize(&mut self, prep: Prepared) {
        let Prepared {
            job,
            next_map,
            meta_pseg,
            ..
        } = prep;
        self.map = next_map;
        self.cp_seq = job.cp_seq;
        self.cp_nonce = job.nonce;
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
        if let Some(gen) = &job.gen {
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
        self.pool_full = false;
        self.recycle_metadata(job.cp_seq, meta_pseg);
        self.stats.cps += 1;
        self.last_commit_ns = self.clock.mono_ns();
        self.events.push(Event::CpCommitted {
            cp_seq: job.cp_seq,
            trigger: job.trigger,
            gen: job.gen.as_ref().map(|g| g.id()),
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
                self.events.push(Event::Stopped {
                    cp_seq: job.cp_seq,
                    clean: true,
                });
            }
            _ => {}
        }
    }

    /// Frees the metadata segments no reachable consistency point names: a
    /// CP's metadata is needed only to boot that CP, and a boot reaches the
    /// newest committed CP or the one before it, so once CP `N` committed
    /// the metadata of every CP before `N - 1` is dead. The segment a
    /// freed CP shared with a live one stays, and so does the open one.
    fn recycle_metadata(&mut self, cp_seq: u64, meta_pseg: u64) {
        self.meta_protect.push_back((cp_seq, vec![meta_pseg]));
        while self.meta_protect.len() > 2 {
            self.meta_protect.pop_front();
        }
        let mut protected: Vec<u64> = self
            .meta_protect
            .iter()
            .flat_map(|(_, segs)| segs.iter().copied())
            .collect();
        if let Some(open) = self.open_meta {
            protected.push(open.pseg);
        }
        for pseg in self.segments.used_segments() {
            let is_meta = self
                .segments
                .entry(pseg)
                .is_ok_and(|e| e.seg_kind() == Some(SegKind::Metadata));
            if is_meta && !protected.contains(&pseg) {
                match self.segments.free(pseg) {
                    Ok(()) => self.stats.recycled += 1,
                    Err(e) => self.events.push(Event::Internal {
                        what: format!("recycling metadata segment {pseg}: {e}"),
                    }),
                }
            }
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
/// write, same volume, consecutive LBAs, consecutive slots on one lap.
fn runs_of(placed: &[PlacedSlot]) -> Vec<(u32, u64, DirtyExtent)> {
    let mut out: Vec<(u32, u64, DirtyExtent)> = Vec::new();
    for p in placed {
        if let Some((vol, lba, ext)) = out.last_mut() {
            let continues = *vol == p.vol
                && ext.seq == p.seq
                && *lba + u64::from(ext.blocks) == p.lba
                && ext.ring.lap == p.ring.lap
                && ext.ring.slot + ext.blocks == p.ring.slot;
            if continues {
                ext.blocks += 1;
                continue;
            }
        }
        out.push((
            p.vol,
            p.lba,
            DirtyExtent {
                ring: p.ring,
                blocks: 1,
                seq: p.seq,
            },
        ));
    }
    out
}
