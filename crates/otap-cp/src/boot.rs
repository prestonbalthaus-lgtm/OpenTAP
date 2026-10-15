//! Boot: select a verified consistency point, rewind a torn one.
//!
//! The ring is read raw (a slot written by a newer format must reach
//! [`SuperblockSlot::rank`] so it can stop the boot). The ranked candidates
//! are tried newest first: the candidate's segment table, map-roots table,
//! every volume's page index, the CP log directory and its delta regions,
//! the snapshot table and every pinned root are read and verified against
//! the roots (E14: a boot trusts only what a root names exactly), the
//! deltas are replayed into the map, the map pages the candidate wrote
//! (a fold's, or a relocation's rewrite of a snapshot's tree) are
//! verified, and every stripe the candidate wrote (its stripe list, F17,
//! ingest and relocation stripes alike) is checked by its summary (CRC
//! and identity, including the CP's nonce); under a trusted opaque barrier
//! after an unclean stop the data blocks of those stripes are checked
//! against their descriptors too (E21). The first candidate that passes is
//! the boot's consistency point; each one that fails is a torn CP, recorded
//! with its reason. A read error is not a torn CP: it fails the boot, which
//! can be retried (a transient error must not discard a sound CP).
//! Damage to the root page or pinned index of a snapshot an older CP took
//! is not a torn CP either: the snapshot is reported damaged and the boot
//! goes on; only a snapshot the candidate itself wrote makes it torn.
//!
//! The walk also tells the engine which metadata segments the candidate
//! references (live bytes per segment), and the same walk over the other
//! verified slots of the ring protects their segments until two more CPs
//! have committed, so a rewind onto them stays possible (E17, E22).

use core::fmt;
use std::collections::HashMap;

use otap_format::checksum::crc32c;
use otap_format::{
    parse_catalog, parse_cp_stripes, parse_map_roots, parse_snap_table, BlockKind, CatalogRec,
    CpLogDir, CpStripe, FormatError, FromBytes, FromZeros, IntentSlot, MapRoot, RootRef, SbSlot,
    SegKind, SegState, SnapRecord, SnapRootPage, StripeId, StripeSummary, SuperblockSlot, Uuid,
    BLOCK_SIZE, INTENT_SLOT_SIZE, SNAP_ROOT_PAGE_SIZE, SUPERBLOCK_SLOTS, SUPERBLOCK_SLOT_SIZE,
};
use otap_map::{index_root_ref, volume_regions, MapError, MapSet, PinnedRoot};

use crate::layout::DiskLayout;
use crate::segments::{OpenSegment, SegmentError, SegmentTable};
use crate::traits::{Backstore, IoError};

/// Map pages the engine's cache holds (16 MiB).
pub const CACHE_PAGES: usize = otap_map::DEFAULT_CACHE_PAGES;

/// Why a boot failed.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum BootError {
    /// No slot of the ring verifies for this aggregate.
    NoSuperblock(FormatError),
    /// A slot was written by a newer format: a hard stop (F7).
    NewerFormat(FormatError),
    /// The newest slot was written by a format before 2.3: 2.0's
    /// `cp_log_head` held a whole-map dump (E14), 2.1 recorded no geometry
    /// (F15) and 2.2 no stripe list (F17); no release wrote any of them.
    /// Re-format the pool.
    OldLayout {
        /// The `ver` field found.
        ver: u16,
    },
    /// Every ranked candidate was torn.
    NoVerifiableCp {
        /// `(cp_seq, reason)` for each candidate tried.
        tried: Vec<(u64, TornReason)>,
    },
    /// Reading the device failed.
    Io(IoError),
    /// The newest slot records a layout geometry other than the profile's
    /// (format 2.2, F15): the pool was laid out with another stripe,
    /// segment or capacity, and nothing is written under a layout that
    /// differs.
    Geometry {
        /// `(stripe, segment, capacity)` the slot recorded.
        recorded: (u32, u32, u64),
        /// `(stripe, segment, capacity)` of the profile booting.
        profile: (u32, u32, u64),
    },
}

impl fmt::Display for BootError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NoSuperblock(e) => write!(f, "no verified superblock: {e}"),
            Self::NewerFormat(e) => write!(f, "{e}"),
            Self::OldLayout { ver } => write!(
                f,
                "the pool was written by format {}.{}; this release reads 2.3 and later",
                ver >> 8,
                ver & 0xff
            ),
            Self::NoVerifiableCp { tried } => {
                write!(
                    f,
                    "no verifiable consistency point among {} candidates",
                    tried.len()
                )?;
                for (cp, reason) in tried {
                    write!(f, "; CP {cp}: {reason}")?;
                }
                Ok(())
            }
            Self::Geometry { recorded, profile } => write!(
                f,
                "the pool was laid out with a {} byte stripe, {} byte segments and {} bytes of \n                 capacity; the profile has {}, {} and {}",
                recorded.0, recorded.1, recorded.2, profile.0, profile.1, profile.2
            ),
            Self::Io(e) => write!(f, "{e}"),
        }
    }
}

impl std::error::Error for BootError {}

impl From<IoError> for BootError {
    fn from(e: IoError) -> Self {
        Self::Io(e)
    }
}

/// Why a candidate was torn.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum TornReason {
    /// The segment table root does not verify or the image is bad.
    SegmentTable(String),
    /// A map region (the roots table, an index, the directory, a delta, a
    /// fold page) does not verify or does not replay.
    Map(String),
    /// The snapshot table, a snapshot root page or a pinned index does not
    /// verify.
    Snapshot(String),
    /// A stripe the CP wrote fails its summary check.
    Stripe {
        /// The segment.
        lseg: u64,
        /// The stripe.
        idx: u32,
        /// The reason.
        reason: FormatError,
    },
    /// A root names a region outside the device.
    RootOutOfRange {
        /// The root's field name.
        field: &'static str,
    },
    /// Reading failed.
    Io(IoError),
}

impl fmt::Display for TornReason {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::SegmentTable(s) => write!(f, "segment table: {s}"),
            Self::Map(s) => write!(f, "map: {s}"),
            Self::Snapshot(s) => write!(f, "snapshot: {s}"),
            Self::Stripe { lseg, idx, reason } => {
                write!(f, "stripe {idx} of segment {lseg}: {reason}")
            }
            Self::RootOutOfRange { field } => write!(f, "{field} names a region off the device"),
            Self::Io(e) => write!(f, "read: {e}"),
        }
    }
}

/// The ring as read: every image, verified or not.
#[derive(Debug, Clone)]
pub struct RingImages {
    /// The four superblock slots, in [`SbSlot::ALL`] order.
    pub slots: [SuperblockSlot; SUPERBLOCK_SLOTS],
    /// The two intent slots.
    pub intents: [IntentSlot; 2],
}

/// Reads the ring and the intent slots raw.
pub fn read_ring<B: Backstore>(b: &mut B, layout: &DiskLayout) -> Result<RingImages, IoError> {
    let mut slots = [SuperblockSlot::new_zeroed(); SUPERBLOCK_SLOTS];
    let mut buf = vec![0u8; SUPERBLOCK_SLOT_SIZE];
    for (i, slot) in SbSlot::ALL.iter().enumerate() {
        b.read(layout.sb_pba(*slot), &mut buf)?;
        slots[i] = SuperblockSlot::read_from_bytes(&buf).expect("slot-sized buffer");
    }
    let mut intents = [IntentSlot::new_zeroed(); 2];
    let mut ibuf = vec![0u8; INTENT_SLOT_SIZE];
    for (which, intent) in intents.iter_mut().enumerate() {
        b.read(layout.intent_pba(which as u8), &mut ibuf)?;
        *intent = IntentSlot::read_from_bytes(&ibuf).expect("intent-sized buffer");
    }
    Ok(RingImages { slots, intents })
}

/// A snapshot as the boot found it: its record and its pinned root, or the
/// damage that keeps the root from being read.
#[derive(Debug, Clone)]
pub struct BootSnapshot {
    /// The catalogue record.
    pub record: SnapRecord,
    /// The pinned root with its index; `None` when it did not verify.
    pub pinned: Option<PinnedRoot>,
    /// Why the root could not be read, when it could not.
    pub damage: Option<String>,
}

/// What a boot selected and rebuilt.
#[derive(Debug)]
pub struct BootState {
    /// The selected slot.
    pub slot: SuperblockSlot,
    /// The intent slots, raw; [`otap_format::boot_write_active`] filters
    /// them.
    pub intents: [IntentSlot; 2],
    /// The map at that CP, deltas replayed.
    pub map: MapSet,
    /// The segment table at that CP.
    pub segments: SegmentTable,
    /// Where the next data stripe goes, if a data segment is open.
    pub open_data: Option<OpenSegment>,
    /// Where the next metadata stripe goes, if a metadata segment is open.
    pub open_meta: Option<OpenSegment>,
    /// Candidates skipped as torn, newest first.
    pub torn: Vec<(u64, TornReason)>,
    /// Bytes of metadata the selected CP references, per segment.
    pub meta_live: HashMap<u64, u64>,
    /// Metadata segments the other verified slots of the ring reference:
    /// kept until two more CPs commit.
    pub protected: Vec<u64>,
    /// The snapshots on disk, in id order.
    pub snapshots: Vec<BootSnapshot>,
    /// The regions behind the selected slot's `map_roots`, `cp_log_head`,
    /// `snap_table`, `cp_stripes` and `seg_table` roots, which the next CP
    /// replaces.
    pub table_regions: Vec<RootRef>,
    /// Consistency points since the fold that wrote a delta.
    pub cps_since_fold: u64,
    /// The catalogue (F16) as the selected CP wrote it, or empty when it
    /// did not verify.
    pub catalog: Vec<CatalogRec>,
    /// Its region, carried forward by a CP that changes no name; `NONE`
    /// when the catalogue did not verify, so the next naming CP writes a
    /// fresh one.
    pub catalog_ref: RootRef,
    /// Why the catalogue was dropped, when it did not verify: the engine
    /// reports it as an operator warning.
    pub catalog_damage: Option<String>,
}

impl BootState {
    /// `true` when the newest candidate was torn and an older CP was taken.
    #[must_use]
    pub fn rewound(&self) -> bool {
        !self.torn.is_empty()
    }
}

/// Selects and loads the boot's consistency point.
pub fn evaluate<B: Backstore>(
    b: &mut B,
    layout: &DiskLayout,
    agg_id: &Uuid,
) -> Result<BootState, BootError> {
    let images = read_ring(b, layout)?;
    let ranked = SuperblockSlot::rank(agg_id, SbSlot::ALL.iter().copied().zip(images.slots.iter()))
        .map_err(|e| match e {
            FormatError::NewerFormat { .. } => BootError::NewerFormat(e),
            other => BootError::NoSuperblock(other),
        })?;
    if let Some(newest) = ranked.first() {
        if (newest.sb.ver & 0xff) < 3 {
            return Err(BootError::OldLayout { ver: newest.sb.ver });
        }
        let profile = (
            layout.stripe_bytes,
            layout.segment_bytes as u32,
            layout.capacity_bytes,
        );
        {
            let recorded = newest.sb.geometry().unwrap_or((0, 0, 0));
            if recorded != profile {
                return Err(BootError::Geometry { recorded, profile });
            }
        }
    }
    let mut torn = Vec::new();
    let mut selected: Option<(usize, Loaded)> = None;
    for (i, candidate) in ranked.iter().enumerate() {
        match load_cp(b, layout, agg_id, candidate) {
            Ok(loaded) => {
                selected = Some((i, loaded));
                break;
            }
            // A read error says nothing about the candidate: the boot fails
            // and is retried, rather than rewinding past a sound CP whose
            // slots the boot commit would then overwrite.
            Err(TornReason::Io(e)) => return Err(BootError::Io(e)),
            Err(reason) => torn.push((candidate.sb.cp_seq, reason)),
        }
    }
    let Some((index, loaded)) = selected else {
        return Err(BootError::NoVerifiableCp { tried: torn });
    };
    // The other verified slots: their metadata segments stay for two CPs.
    let mut protected = Vec::new();
    for (i, other) in ranked.iter().enumerate() {
        if i == index || other.sb.cp_seq == ranked[index].sb.cp_seq {
            continue;
        }
        if let Ok(walk) = walk_cp(b, layout, agg_id, other) {
            for region in &walk.regions {
                for (pseg, _) in layout.segments_of_region(region.pba, region.pages) {
                    protected.push(pseg);
                }
            }
        }
    }
    protected.sort_unstable();
    protected.dedup();
    let Loaded {
        map,
        segments,
        open_data,
        open_meta,
        meta_live,
        snapshots,
        table_regions,
        cps_since_fold,
        catalog,
        catalog_ref,
        catalog_damage,
    } = loaded;
    Ok(BootState {
        slot: *ranked[index],
        intents: images.intents,
        map,
        segments,
        open_data,
        open_meta,
        torn,
        meta_live,
        protected,
        snapshots,
        table_regions,
        cps_since_fold,
        catalog,
        catalog_ref,
        catalog_damage,
    })
}

struct Loaded {
    map: MapSet,
    segments: SegmentTable,
    open_data: Option<OpenSegment>,
    open_meta: Option<OpenSegment>,
    meta_live: HashMap<u64, u64>,
    snapshots: Vec<BootSnapshot>,
    table_regions: Vec<RootRef>,
    cps_since_fold: u64,
    catalog: Vec<CatalogRec>,
    catalog_ref: RootRef,
    catalog_damage: Option<String>,
}

/// Everything a slot's roots reach: the regions, the parsed tables, the
/// index bytes per volume, the directory and the snapshots.
struct Walk {
    regions: Vec<RootRef>,
    roots: Vec<MapRoot>,
    indexes: Vec<(u32, Vec<u8>)>,
    dir: CpLogDir,
    deltas: Vec<Vec<u8>>,
    snapshots: Vec<BootSnapshot>,
    table_regions: Vec<RootRef>,
    /// The stripes the CP wrote (F17), to be verified.
    stripes: Vec<CpStripe>,
    /// The catalogue and its region.
    catalog: Vec<CatalogRec>,
    catalog_ref: RootRef,
    /// Why the catalogue was dropped, when it did not verify.
    catalog_damage: Option<String>,
}

/// Reads every region a slot's map and snapshot roots name and verifies
/// each against its reference.
fn walk_cp<B: Backstore>(
    b: &mut B,
    layout: &DiskLayout,
    agg_id: &Uuid,
    slot: &SuperblockSlot,
) -> Result<Walk, TornReason> {
    let sb = &slot.sb;
    let mut regions = Vec::new();
    let mut table_regions = Vec::new();
    // The map-roots table.
    let roots = if sb.map_roots.is_none() {
        Vec::new()
    } else {
        let bytes = read_root(b, layout, &sb.map_roots, "map_roots")?;
        regions.push(sb.map_roots);
        table_regions.push(sb.map_roots);
        parse_map_roots(&bytes, sb.vol_count)
            .map_err(|e| TornReason::Map(format!("map_roots: {e}")))?
    };
    // Every volume's index, and through it every map page.
    let mut indexes = Vec::new();
    for root in &roots {
        if root.is_absent() {
            continue;
        }
        let index_ref = index_root_ref(root);
        let bytes = read_root(b, layout, &index_ref, "index")?;
        root.verify_index(&bytes)
            .map_err(|e| TornReason::Map(format!("volume {} index: {e}", root.vol)))?;
        regions.push(index_ref);
        let entries = <[otap_format::PageIndexEntry]>::ref_from_bytes(&bytes).map_err(|_| {
            TornReason::Map(format!("volume {} index is not whole entries", root.vol))
        })?;
        for entry in entries.iter().filter(|e| !e.is_none()) {
            regions.push(otap_map::page_root_ref(entry));
        }
        indexes.push((root.vol, bytes));
    }
    // The directory and its deltas.
    let dir = if sb.cp_log_head.is_none() {
        CpLogDir {
            cp_seq: sb.cp_seq,
            fold_cp_seq: sb.cp_seq,
            deltas: Vec::new(),
            pages: Vec::new(),
        }
    } else {
        let bytes = read_root(b, layout, &sb.cp_log_head, "cp_log_head")?;
        regions.push(sb.cp_log_head);
        table_regions.push(sb.cp_log_head);
        let dir = CpLogDir::decode(&bytes, agg_id)
            .map_err(|e| TornReason::Map(format!("cp log directory: {e}")))?;
        if dir.cp_seq != sb.cp_seq {
            return Err(TornReason::Map(format!(
                "cp log directory is CP {}, the slot is CP {}",
                dir.cp_seq, sb.cp_seq
            )));
        }
        dir
    };
    let mut deltas = Vec::new();
    for delta in &dir.deltas {
        let bytes = read_root(b, layout, delta, "delta")?;
        regions.push(*delta);
        deltas.push(bytes);
    }
    // The pages this CP wrote (a fold's, or the ones a relocation wrote
    // for a snapshot's tree): every one must verify, or the CP did not
    // land whole.
    for page in &dir.pages {
        read_root(b, layout, page, "map page")?;
    }
    // The stripe list (F17): what the CP wrote and the boot verifies.
    let stripes = if sb.cp_stripes.is_none() {
        Vec::new()
    } else {
        let bytes = read_root(b, layout, &sb.cp_stripes, "cp_stripes")?;
        regions.push(sb.cp_stripes);
        table_regions.push(sb.cp_stripes);
        parse_cp_stripes(&bytes).map_err(|e| TornReason::Map(format!("stripe list: {e}")))?
    };
    // The catalogue (F16), carried forward unchanged by most CPs. One that
    // does not verify is damage to the names, not to the candidate: every
    // candidate of the ring names the same region, so tearing on it would
    // make the pool unbootable for a name. The boot goes on with no names,
    // reports it, and the next CP that names something writes a fresh
    // region. A read error is the device's, and fails the candidate as
    // every other root's does.
    let (catalog, catalog_ref, catalog_damage) = if sb.catalog.is_none() {
        (Vec::new(), RootRef::NONE, None)
    } else {
        match read_root(b, layout, &sb.catalog, "catalog") {
            Err(TornReason::Io(e)) => return Err(TornReason::Io(e)),
            Err(reason) => (Vec::new(), RootRef::NONE, Some(reason.to_string())),
            Ok(bytes) => match parse_catalog(&bytes) {
                Ok(records) => {
                    regions.push(sb.catalog);
                    (records, sb.catalog, None)
                }
                Err(e) => (Vec::new(), RootRef::NONE, Some(format!("catalogue: {e}"))),
            },
        }
    };
    // The snapshots: the table, each root page, each pinned index. Damage
    // to a snapshot an older CP took is the snapshot's, not the
    // candidate's: it is reported and the boot goes on; a root page or
    // index this CP wrote itself tears the candidate.
    let mut snapshots = Vec::new();
    if !sb.snap_table.is_none() {
        let bytes = read_root(b, layout, &sb.snap_table, "snap_table")?;
        regions.push(sb.snap_table);
        table_regions.push(sb.snap_table);
        let records = parse_snap_table(&bytes)
            .map_err(|e| TornReason::Snapshot(format!("snapshot table: {e}")))?;
        for record in records {
            let own = u64::from(record.birth_cp) == sb.cp_seq;
            match load_snapshot(b, layout, agg_id, &record) {
                Ok((pinned, mut pinned_regions)) => {
                    regions.append(&mut pinned_regions);
                    snapshots.push(BootSnapshot {
                        record,
                        pinned: Some(pinned),
                        damage: None,
                    });
                }
                Err(TornReason::Io(e)) => return Err(TornReason::Io(e)),
                Err(reason) if own => return Err(reason),
                Err(reason) => snapshots.push(BootSnapshot {
                    record,
                    pinned: None,
                    damage: Some(reason.to_string()),
                }),
            }
        }
    }
    Ok(Walk {
        regions,
        roots,
        indexes,
        dir,
        deltas,
        snapshots,
        table_regions,
        stripes,
        catalog,
        catalog_ref,
        catalog_damage,
    })
}

/// Reads and verifies a snapshot's root page and pinned index; returns the
/// pinned root and the regions it references (the page, the index, every
/// map page).
fn load_snapshot<B: Backstore>(
    b: &mut B,
    layout: &DiskLayout,
    agg_id: &Uuid,
    record: &SnapRecord,
) -> Result<(PinnedRoot, Vec<RootRef>), TornReason> {
    let mut page = vec![0u8; SNAP_ROOT_PAGE_SIZE];
    let end = record
        .map_root_pba
        .checked_add(1)
        .map(|blocks| blocks.saturating_mul(BLOCK_SIZE as u64));
    if record.map_root_pba == 0 || end.is_none_or(|e| e > layout.capacity_bytes) {
        return Err(TornReason::RootOutOfRange {
            field: "map_root_pba",
        });
    }
    b.read(record.map_root_pba, &mut page)
        .map_err(TornReason::Io)?;
    let root_page = SnapRootPage::read(&page)
        .map_err(|e| TornReason::Snapshot(format!("snapshot {} root page: {e}", record.snap_id)))?;
    if root_page.agg_id != *agg_id
        || root_page.snap_id != record.snap_id
        || root_page.birth_cp != u64::from(record.birth_cp)
        || root_page.root.vol != record.vol
    {
        return Err(TornReason::Snapshot(format!(
            "snapshot {} root page names another snapshot",
            record.snap_id
        )));
    }
    let mut regions = vec![RootRef::new(record.map_root_pba, 1, root_page.crc32c)];
    let index_ref = index_root_ref(&root_page.root);
    let index_bytes = read_root(b, layout, &index_ref, "snapshot index")?;
    let pinned = PinnedRoot::load(root_page.root, &index_bytes)
        .map_err(|e| TornReason::Snapshot(format!("snapshot {} index: {e}", record.snap_id)))?;
    regions.extend(pinned.regions());
    Ok((pinned, regions))
}

fn load_cp<B: Backstore>(
    b: &mut B,
    layout: &DiskLayout,
    agg_id: &Uuid,
    slot: &SuperblockSlot,
) -> Result<Loaded, TornReason> {
    let sb = &slot.sb;
    // A freshly formatted pool (CP 0) has no table image: every segment is
    // free.
    let mut segments = if sb.seg_table.is_none() {
        SegmentTable::empty(layout.segments)
    } else {
        let table_bytes = read_root(b, layout, &sb.seg_table, "seg_table")?;
        SegmentTable::from_image(&table_bytes, layout.segments)
            .map_err(|e: SegmentError| TornReason::SegmentTable(e.to_string()))?
    };
    let walk = walk_cp(b, layout, agg_id, slot)?;
    let mut regions = walk.regions;
    if !sb.seg_table.is_none() {
        regions.push(sb.seg_table);
    }
    // The map: roots, indexes, then the deltas replayed in directory order.
    let indexes: HashMap<u32, Vec<u8>> = walk.indexes.into_iter().collect();
    let mut map = MapSet::from_roots(
        *agg_id,
        CACHE_PAGES,
        sb.cp_seq,
        walk.dir.fold_cp_seq,
        &walk.roots,
        |root| {
            indexes
                .get(&root.vol)
                .cloned()
                .ok_or(MapError::NoSuchVolume { vol: root.vol })
        },
    )
    .map_err(|e| TornReason::Map(format!("map roots: {e}")))?;
    let mut delta_cps: Vec<u64> = Vec::new();
    for bytes in &walk.deltas {
        let replayed = map
            .replay(bytes)
            .map_err(|e| TornReason::Map(format!("cp log replay: {e}")))?;
        if replayed.cp_seq > walk.dir.fold_cp_seq && delta_cps.last() != Some(&replayed.cp_seq) {
            delta_cps.push(replayed.cp_seq);
        }
    }
    map.set_delta_roots(walk.dir.deltas.clone(), delta_cps.len() as u64);
    // Every stripe this CP wrote (its stripe list, F17: ingest and
    // relocation alike) must still read as this CP's: its summary
    // verifies, and, when the barrier was only trusted (E21) and the stop
    // was not clean, so does every data block against its descriptor (a
    // controller cache can lose a block behind an intact summary).
    let check_data = b.profile().trusts_opaque_barrier() && !sb.is_clean_shutdown();
    let summary_len = layout.summary.summary_bytes;
    let stripe_len = layout.stripe_bytes as usize;
    let mut buf = vec![0u8; if check_data { stripe_len } else { summary_len }];
    for entry in &walk.stripes {
        let (lseg, idx) = (entry.pseg, entry.idx);
        let pba = layout.stripe_pba(lseg, idx).ok_or(TornReason::Stripe {
            lseg,
            idx,
            reason: FormatError::OutOfRange {
                field: "lseg",
                value: lseg,
                max: u64::from(layout.segments),
            },
        })?;
        b.read(pba, &mut buf).map_err(TornReason::Io)?;
        let id = StripeId {
            agg_id: *agg_id,
            lseg,
            stripe_idx: idx,
            cp_seq: sb.cp_seq,
            cp_nonce: sb.cp_nonce,
        };
        let summary = StripeSummary::parse_expected(&buf[..summary_len], &id)
            .map_err(|reason| TornReason::Stripe { lseg, idx, reason })?;
        if check_data {
            for (i, desc) in summary.descriptors().iter().enumerate() {
                if desc.kind() != Some(BlockKind::Data) {
                    continue;
                }
                let at = layout.summary.block_offset(i);
                let block = &buf[at..at + BLOCK_SIZE];
                if crc32c(block) != desc.crc32c {
                    return Err(TornReason::Stripe {
                        lseg,
                        idx,
                        reason: FormatError::BadCrc {
                            region: "data block",
                            computed: crc32c(block),
                            stored: desc.crc32c,
                        },
                    });
                }
            }
        }
    }
    // The data append point is recorded in the slot (format 2.1); it must
    // name an open data segment of the table.
    let open_data = if sb.open_data_pseg == 0 {
        None
    } else {
        let entry = segments
            .entry(sb.open_data_pseg)
            .map_err(|e| TornReason::SegmentTable(e.to_string()))?;
        if entry.seg_state() != Some(SegState::Open)
            || entry.seg_kind() != Some(SegKind::Data)
            || sb.open_data_idx > layout.stripes_per_segment
        {
            return Err(TornReason::SegmentTable(format!(
                "open data segment {} is not open data or its index {} is out of range",
                sb.open_data_pseg, sb.open_data_idx
            )));
        }
        Some(OpenSegment {
            pseg: sb.open_data_pseg,
            next_idx: sb.open_data_idx,
        })
    };
    // Metadata liveness per segment.
    let mut meta_live: HashMap<u64, u64> = HashMap::new();
    for region in &regions {
        for (pseg, bytes) in layout.segments_of_region(region.pba, region.pages) {
            *meta_live.entry(pseg).or_insert(0) += bytes;
        }
    }
    for (pseg, entry) in segments.iter() {
        if entry.seg_state() == Some(SegState::Open) && entry.seg_kind().is_none() {
            return Err(TornReason::SegmentTable(format!(
                "segment {pseg} has no kind"
            )));
        }
    }
    // The metadata append point: the highest open metadata segment, after
    // the last block any region of this CP occupies in it. Older open
    // metadata segments (a run the stream moved on from) are sealed: they
    // are never appended to again, and they are freed when their live
    // bytes reach zero.
    let open_segs: Vec<u64> = segments
        .iter()
        .filter(|(_, e)| {
            e.seg_state() == Some(SegState::Open) && e.seg_kind() == Some(SegKind::Metadata)
        })
        .map(|(p, _)| p)
        .collect();
    let mut open_meta = None;
    if let Some(&pseg) = open_segs.last() {
        for other in open_segs.iter().filter(|p| **p != pseg) {
            segments
                .seal(*other)
                .map_err(|e| TornReason::SegmentTable(e.to_string()))?;
        }
        let first = layout
            .segment_first_pba(pseg)
            .ok_or(TornReason::SegmentTable(format!(
                "segment {pseg} is off the device"
            )))?;
        let seg_end = first + u64::from(layout.blocks_per_segment());
        let mut highest_end = first;
        for region in &regions {
            let end = region.pba + u64::from(region.pages);
            if region.pba < seg_end && end > first {
                highest_end = highest_end.max(end.min(seg_end));
            }
        }
        let next_idx = (highest_end - first).div_ceil(u64::from(layout.blocks_per_stripe())) as u32;
        if next_idx < layout.stripes_per_segment {
            open_meta = Some(OpenSegment { pseg, next_idx });
        } else {
            segments
                .seal(pseg)
                .map_err(|e| TornReason::SegmentTable(e.to_string()))?;
        }
    }
    let _ = volume_regions;
    Ok(Loaded {
        map,
        segments,
        open_data,
        open_meta,
        meta_live,
        snapshots: walk.snapshots,
        table_regions: {
            let mut regions = walk.table_regions;
            if !sb.seg_table.is_none() {
                regions.push(sb.seg_table);
            }
            regions
        },
        cps_since_fold: delta_cps.len() as u64,
        catalog: walk.catalog,
        catalog_ref: walk.catalog_ref,
        catalog_damage: walk.catalog_damage,
    })
}

fn read_root<B: Backstore>(
    b: &mut B,
    layout: &DiskLayout,
    root: &RootRef,
    field: &'static str,
) -> Result<Vec<u8>, TornReason> {
    if root.is_none() {
        return Ok(Vec::new());
    }
    let bytes = root.bytes();
    let end = root
        .pba
        .checked_add(u64::from(root.pages))
        .map(|blocks| blocks.saturating_mul(BLOCK_SIZE as u64));
    if root.pba == 0 || end.is_none_or(|e| e > layout.capacity_bytes) || bytes > (1u64 << 32) {
        return Err(TornReason::RootOutOfRange { field });
    }
    let mut buf = vec![0u8; bytes as usize];
    b.read(root.pba, &mut buf).map_err(TornReason::Io)?;
    root.verify(&buf)
        .map_err(|e| TornReason::Map(format!("{field}: {e}")))?;
    Ok(buf)
}
