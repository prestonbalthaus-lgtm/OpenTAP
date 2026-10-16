//! `otap-map`: the persistent block map (round 3, section 1.3, "Map, stable
//! block identity, holes, `diff`"; Step 4 brief, "Opus-Storage-Map").
//!
//! Every volume's map is the two-level radix map `otap-format` lays out: a
//! [`MapRoot`] names a run of page-index pages, each [`PageIndexEntry`] names
//! a 16 KiB [`MapPage`] of 1,024 [`MapEntry`]s, one per 4 KiB block. The
//! pages on disk hold the state as of the last *fold*; everything a
//! consistency point changed since then lives in the CP log (F12) and in an
//! in-memory *overlay* per volume that answers lookups first. A fold rewrites
//! the pages the overlay touched, copy on write, and the page indexes of the
//! volumes concerned, then the overlay empties and the log starts over.
//!
//! # What this crate owns
//!
//! - [`MapSet`]: every volume's root, checkpointed index and overlay; the
//!   page cache (keyed by physical block, because a map page is immutable
//!   once written); lookups ([`MapSet::get`]); the consistency-point
//!   transaction ([`MapSet::prepare_cp`] builds every region the CP must
//!   write and the directory it must publish, [`MapSet::apply`] takes the
//!   result once the CP is durable); boot ([`MapSet::from_roots`],
//!   [`MapSet::replay`]); the birth walk ([`MapSet::diff`]).
//! - [`PinnedRoot`]: a snapshot's root, read without an overlay, with the
//!   same walk ([`PinnedRoot::diff`]).
//!
//! # What it does not own
//!
//! No I/O: pages are read through a [`PageReader`] and regions are placed
//! through a [`RegionWriter`], both implemented by the engine (and by the
//! tests over a byte vector), so the crate runs on any host. No segment
//! bookkeeping: [`PreparedCp`] reports every region written, every region
//! and page the CP stops referencing, and every map entry it replaces, and
//! the engine turns those into live-byte accounting, snapshot pinning and
//! deferred frees. No policy: the engine decides when to fold.
//!
//! # Invariants
//!
//! - A lookup answers from the overlay, else from the page the checkpointed
//!   index names, verified against the index entry's CRC and `max_birth`
//!   (invariant 2 of section 7.7: a flipped bit or a misdirected page is
//!   caught before an entry is believed).
//! - A prepared CP changes nothing until [`MapSet::apply`]: a failed CP
//!   leaves the map as it was, and reads during the CP see the committed
//!   state.
//! - After a fold at CP `K`, every record with `birth_cp <= K` is in the
//!   pages and every later one is in the log; replay skips the former and
//!   applies the latter, so a boot never applies a record twice.
//! - `diff(base)` yields exactly the entries with `birth_cp > base`
//!   (data or hole), reading only the pages whose `max_birth` says so.

#![forbid(unsafe_code)]
#![deny(missing_docs)]
#![warn(rust_2018_idioms, unreachable_pub)]

use core::fmt;
use std::collections::{BTreeMap, BTreeSet, HashMap, VecDeque};

use otap_format::{
    CpLogDir, CpLogPage, CpLogRec, FormatError, FromBytes, IntoBytes, MapEntry, MapPage, MapRoot,
    PageIndexEntry, PageIndexPage, RootRef, Uuid, BLOCK_SIZE, CP_LOG_PAGE_SIZE,
    INDEX_ENTRIES_PER_PAGE, INDEX_PAGE_SIZE, MAP_ENTRIES_PER_PAGE, MAP_PAGE_SIZE, MAX_CP_SEQ,
    MAX_LBA, MAX_VOLS,
};

/// Pages (4 KiB blocks) one map page occupies.
pub const MAP_PAGE_BLOCKS: u32 = (MAP_PAGE_SIZE / BLOCK_SIZE) as u32;
/// Pages (4 KiB blocks) one page-index page occupies.
pub const INDEX_PAGE_BLOCKS: u32 = (INDEX_PAGE_SIZE / BLOCK_SIZE) as u32;
/// Map pages the cache keeps by default (16 MiB).
pub const DEFAULT_CACHE_PAGES: usize = 1024;

/// Why the map refused or failed.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum MapError {
    /// The format refused a structure (a page, an index, a record).
    Format(FormatError),
    /// Reading or placing a region failed.
    Io {
        /// The first block.
        pba: u64,
        /// The length in bytes.
        bytes: usize,
        /// What the reader or writer said.
        reason: String,
    },
    /// The volume is not in the map.
    NoSuchVolume {
        /// The volume.
        vol: u32,
    },
    /// The volume is already in the map.
    VolumeExists {
        /// The volume.
        vol: u32,
    },
    /// A block beyond the volume.
    OutOfRange {
        /// The volume.
        vol: u32,
        /// The block.
        lba: u64,
    },
    /// A volume size of zero or beyond the LBA space, or a volume index at
    /// or above the bound.
    BadSize {
        /// The volume.
        vol: u32,
        /// The size offered.
        lba_count: u64,
    },
    /// A CP log that does not read as one journal: a page out of order, a
    /// CP going backwards, a record for an unknown volume.
    Replay {
        /// The field at fault.
        field: &'static str,
        /// Its value.
        value: u64,
    },
    /// The writer has no room for a region: the pool has no free metadata
    /// segment.
    NoSpace,
    /// An invariant the map relies on did not hold.
    Internal(String),
}

impl fmt::Display for MapError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Format(e) => write!(f, "format: {e}"),
            Self::Io { pba, bytes, reason } => {
                write!(f, "{bytes} bytes at block {pba}: {reason}")
            }
            Self::NoSuchVolume { vol } => write!(f, "volume {vol} is not mapped"),
            Self::VolumeExists { vol } => write!(f, "volume {vol} is already mapped"),
            Self::OutOfRange { vol, lba } => write!(f, "volume {vol} LBA {lba} is out of range"),
            Self::BadSize { vol, lba_count } => {
                write!(f, "volume {vol}: {lba_count} blocks is not a valid size")
            }
            Self::Replay { field, value } => write!(f, "cp log replay: {field} = {value}"),
            Self::NoSpace => f.write_str("no free metadata segment"),
            Self::Internal(s) => write!(f, "internal: {s}"),
        }
    }
}

impl std::error::Error for MapError {}

impl From<FormatError> for MapError {
    fn from(e: FormatError) -> Self {
        Self::Format(e)
    }
}

/// Where map pages come from: the engine's backstore, or a byte vector in
/// the tests.
pub trait PageReader {
    /// Reads `dst.len()` bytes (whole 4 KiB blocks) at block `pba`.
    fn read_blocks(&mut self, pba: u64, dst: &mut [u8]) -> Result<(), MapError>;
}

/// Where the regions a consistency point writes go: the engine's metadata
/// stream, which assigns each region its blocks and later writes it, or a
/// byte vector in the tests. A region is a whole number of 4 KiB pages and
/// is named exactly by the reference returned (block, length, CRC).
pub trait RegionWriter {
    /// Places `bytes` and returns its reference.
    fn place(&mut self, bytes: &[u8]) -> Result<RootRef, MapError>;
}

/// A cache of map pages keyed by their physical block. Pages are immutable
/// once written (a fold writes new pages, never rewrites one), so an entry
/// is never stale; a page is dropped only to make room.
#[derive(Debug, Default)]
pub struct PageCache {
    pages: HashMap<u64, Box<MapPage>>,
    order: VecDeque<u64>,
    cap: usize,
    hits: u64,
    misses: u64,
}

impl PageCache {
    /// A cache holding at most `cap` pages (at least 1).
    #[must_use]
    pub fn new(cap: usize) -> Self {
        Self {
            pages: HashMap::new(),
            order: VecDeque::new(),
            cap: cap.max(1),
            hits: 0,
            misses: 0,
        }
    }

    /// The page at `pba`, if cached.
    #[must_use]
    pub fn get(&mut self, pba: u64) -> Option<&MapPage> {
        match self.pages.get(&pba) {
            Some(page) => {
                self.hits += 1;
                Some(page)
            }
            None => {
                self.misses += 1;
                None
            }
        }
    }

    /// Keeps `page` as the page at `pba`, evicting the oldest insert when
    /// full.
    pub fn insert(&mut self, pba: u64, page: Box<MapPage>) {
        if self.pages.insert(pba, page).is_none() {
            self.order.push_back(pba);
        }
        while self.pages.len() > self.cap {
            match self.order.pop_front() {
                Some(old) => {
                    self.pages.remove(&old);
                }
                None => break,
            }
        }
    }

    /// Cached pages.
    #[must_use]
    pub fn len(&self) -> usize {
        self.pages.len()
    }

    /// `true` with nothing cached.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.pages.is_empty()
    }

    /// Hits and misses so far.
    #[must_use]
    pub const fn counters(&self) -> (u64, u64) {
        (self.hits, self.misses)
    }
}

/// Reads and verifies the map page `entry` names, through the cache.
fn load_page<'c>(
    cache: &'c mut PageCache,
    reader: &mut dyn PageReader,
    entry: &PageIndexEntry,
) -> Result<&'c MapPage, MapError> {
    if entry.is_none() {
        return Err(MapError::Internal("loading an absent page".into()));
    }
    // Two lookups keep the borrow checker happy: the first decides, the
    // second borrows for the caller.
    if cache.get(entry.page_pba).is_none() {
        let mut bytes = vec![0u8; MAP_PAGE_SIZE];
        reader.read_blocks(entry.page_pba, &mut bytes)?;
        let page = MapPage::read(&bytes, entry)?;
        cache.insert(entry.page_pba, Box::new(page));
    }
    cache
        .pages
        .get(&entry.page_pba)
        .map(|p| &**p)
        .ok_or_else(|| MapError::Internal("a page vanished from the cache".into()))
}

/// Map pages a volume of `lba_count` blocks has.
#[must_use]
pub fn map_pages_for(lba_count: u64) -> usize {
    MapRoot::map_pages_for(lba_count) as usize
}

/// The index region bytes of `entries` (map pages 0..): whole index pages,
/// absent entries zero.
#[must_use]
pub fn index_image(entries: &[PageIndexEntry]) -> Vec<u8> {
    let pages = entries.len().div_ceil(INDEX_ENTRIES_PER_PAGE).max(1);
    let mut out = Vec::with_capacity(pages * INDEX_PAGE_SIZE);
    for chunk in 0..pages {
        let mut page = PageIndexPage::zeroed();
        let start = chunk * INDEX_ENTRIES_PER_PAGE;
        for (i, slot) in page.entries.iter_mut().enumerate() {
            if let Some(e) = entries.get(start + i) {
                *slot = *e;
            }
        }
        out.extend_from_slice(page.as_bytes());
    }
    out
}

/// Parses an index region verified by `root` into one entry per map page.
fn index_entries(root: &MapRoot, bytes: &[u8]) -> Result<Vec<PageIndexEntry>, MapError> {
    root.verify_index(bytes)?;
    let map_pages = map_pages_for(root.lba_count);
    let all = <[PageIndexEntry]>::ref_from_bytes(bytes)
        .map_err(|_| MapError::Internal("index region is not whole entries".into()))?;
    if all.len() < map_pages {
        return Err(MapError::Internal(
            "index region shorter than its volume".into(),
        ));
    }
    for extra in &all[map_pages..] {
        if !extra.is_none() {
            return Err(MapError::Format(FormatError::BadField {
                field: "page_pba",
                value: extra.page_pba,
            }));
        }
    }
    Ok(all[..map_pages].to_vec())
}

/// The reference of the index region a root names.
#[must_use]
pub fn index_root_ref(root: &MapRoot) -> RootRef {
    RootRef::new(
        root.index_pba,
        root.index_pages * INDEX_PAGE_BLOCKS,
        root.index_crc,
    )
}

/// The reference of the map page an index entry names.
#[must_use]
pub fn page_root_ref(entry: &PageIndexEntry) -> RootRef {
    RootRef::new(entry.page_pba, MAP_PAGE_BLOCKS, entry.page_crc)
}

/// One volume's map: the checkpointed root and index, and the overlay of
/// entries changed since the fold.
#[derive(Debug, Clone)]
pub struct VolumeMap {
    root: MapRoot,
    index: Vec<PageIndexEntry>,
    overlay: BTreeMap<u64, MapEntry>,
}

impl VolumeMap {
    fn from_root(root: MapRoot, index_bytes: &[u8]) -> Result<Self, MapError> {
        root.validate()?;
        if root.is_absent() {
            return Err(MapError::BadSize {
                vol: root.vol,
                lba_count: 0,
            });
        }
        let index = index_entries(&root, index_bytes)?;
        Ok(Self {
            root,
            index,
            overlay: BTreeMap::new(),
        })
    }

    /// The checkpointed root.
    #[must_use]
    pub const fn root(&self) -> &MapRoot {
        &self.root
    }

    /// The checkpointed index: one entry per map page.
    #[must_use]
    pub fn index(&self) -> &[PageIndexEntry] {
        &self.index
    }

    /// The volume's size in blocks.
    #[must_use]
    pub const fn lba_count(&self) -> u64 {
        self.root.lba_count
    }

    /// Entries changed since the fold.
    #[must_use]
    pub fn overlay_len(&self) -> usize {
        self.overlay.len()
    }

    /// The overlay, in LBA order.
    pub fn overlay(&self) -> impl Iterator<Item = (u64, MapEntry)> + '_ {
        self.overlay.iter().map(|(l, e)| (*l, *e))
    }

    /// Map pages the overlay touches.
    #[must_use]
    pub fn dirty_pages(&self) -> BTreeSet<u64> {
        self.overlay.keys().map(|l| MapPage::page_of(*l)).collect()
    }

    fn check_lba(&self, vol: u32, lba: u64) -> Result<(), MapError> {
        if lba >= self.root.lba_count {
            return Err(MapError::OutOfRange { vol, lba });
        }
        Ok(())
    }
}

/// A map entry a consistency point replaces: what the engine needs for
/// live-byte accounting.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Replaced {
    /// The volume.
    pub vol: u32,
    /// The block.
    pub lba: u64,
    /// The entry before (unmapped when the block was never written).
    pub old: MapEntry,
    /// The entry after.
    pub new: MapEntry,
}

/// A map page a fold stops referencing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FreedPage {
    /// The volume.
    pub vol: u32,
    /// The map page number.
    pub page_no: u64,
    /// The page's region.
    pub root: RootRef,
}

/// An index region a fold stops referencing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FreedIndex {
    /// The volume.
    pub vol: u32,
    /// The index region.
    pub root: RootRef,
}

/// A volume this CP rolls back to a pinned root (Step 5, E36): its live
/// tree becomes the snapshot's, pages shared, and every live entry born
/// after the snapshot is released.
#[derive(Debug, Clone)]
pub struct Rollback {
    /// The volume.
    pub vol: u32,
    /// The CP the snapshot pins: entries born after it are released.
    pub birth_cp: u64,
    /// The pinned root.
    pub root: MapRoot,
    /// Its index.
    pub index: Vec<PageIndexEntry>,
}

/// What a consistency point asks of the map.
#[derive(Debug, Clone, Default)]
pub struct CpInput {
    /// The CP's sequence, above the committed one.
    pub cp_seq: u64,
    /// Its nonce, written into the delta pages.
    pub cp_nonce: u64,
    /// Every block the CP maps: `(vol, lba, entry)`, data or hole, in any
    /// order; the last entry for a block wins.
    pub deltas: Vec<(u32, u64, MapEntry)>,
    /// Fold the overlay (and these deltas) into the pages.
    pub fold: bool,
    /// Volumes created by this CP: `(vol, lba_count)`.
    pub new_volumes: Vec<(u32, u64)>,
    /// The longest region the writer takes, in 4 KiB pages (a delta longer
    /// than this is split into several regions).
    pub max_region_pages: u32,
    /// Volumes this CP rolls back to a snapshot (needs `fold`; the volume
    /// must have no deltas in this CP).
    pub rollbacks: Vec<Rollback>,
    /// Volumes this CP resizes to `(vol, lba_count)` (needs `fold`; a
    /// shrink must have no deltas at or beyond the new size).
    pub resizes: Vec<(u32, u64)>,
    /// Volumes this CP deletes (needs `fold`; no deltas).
    pub deletes: Vec<u32>,
}

/// What [`MapSet::apply`] takes once the CP is durable.
#[derive(Debug, Clone)]
pub struct MapUpdate {
    cp_seq: u64,
    fold: bool,
    deltas: Vec<(u32, u64, MapEntry)>,
    delta_roots: Vec<RootRef>,
    delta_bytes: u64,
    new_volumes: Vec<(u32, MapRoot, Vec<PageIndexEntry>)>,
    fold_roots: Vec<(u32, MapRoot, Vec<PageIndexEntry>)>,
    new_pages: Vec<(u64, Box<MapPage>)>,
    deletes: Vec<u32>,
}

impl MapUpdate {
    /// The CP the update belongs to.
    #[must_use]
    pub const fn cp_seq(&self) -> u64 {
        self.cp_seq
    }

    /// `true` when the update is a fold.
    #[must_use]
    pub const fn is_fold(&self) -> bool {
        self.fold
    }
}

/// Everything [`MapSet::prepare_cp`] produced: the regions it placed, the
/// directory and roots table the engine places next, and the bookkeeping
/// the engine needs.
#[derive(Debug, Clone)]
pub struct PreparedCp {
    /// Applied once the CP is durable.
    pub update: MapUpdate,
    /// The directory the CP publishes behind `cp_log_head` (the engine
    /// encodes and places it after the map's own regions).
    pub dir: CpLogDir,
    /// The roots table the CP publishes behind `map_roots`, one slot per
    /// volume index up to the highest present volume.
    pub roots_table: Vec<MapRoot>,
    /// Every entry the CP replaces (`old` unmapped for a first write).
    pub replaced: Vec<Replaced>,
    /// Every region the map placed: deltas, fold pages, index regions.
    pub written: Vec<RootRef>,
    /// Map pages the fold stops referencing (a snapshot may still pin one).
    pub freed_pages: Vec<FreedPage>,
    /// Index regions the fold stops referencing (a snapshot may still pin
    /// one).
    pub freed_indexes: Vec<FreedIndex>,
    /// Delta regions the fold drops from the directory.
    pub freed_deltas: Vec<RootRef>,
    /// After a fold: every volume's root and index, complete at this CP,
    /// which is what a snapshot pins.
    pub fold_roots: Vec<(u32, MapRoot, Vec<PageIndexEntry>)>,
    /// Map pages (`(vol, page_no)`) that did not verify when the CP looked
    /// up the entries its records replace, or when a rollback, shrink or
    /// deletion walked them: the old entry was taken as unmapped (the
    /// replaced block's bytes stay counted live, the safe direction) and
    /// the engine reports the page. A fold that must rewrite such a page
    /// fails instead.
    pub damaged_pages: Vec<(u32, u64)>,
    /// Entries a rollback, a shrink or a deletion stops referencing:
    /// `(vol, lba, entry)`, data or hole, nothing new in their place. The
    /// engine's liveness accounting takes them like replaced entries whose
    /// successor is unmapped.
    pub released: Vec<(u32, u64, MapEntry)>,
}

/// What a replayed delta region held.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Replayed {
    /// The CP the region belongs to.
    pub cp_seq: u64,
    /// The records, in the order written.
    pub records: Vec<CpLogRec>,
    /// Records applied to an overlay (born after the fold).
    pub applied: usize,
    /// Records already in the pages (born at or before the fold).
    pub skipped: usize,
}

/// Counters.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct MapStats {
    /// Map pages read from the reader.
    pub page_reads: u64,
    /// Folds applied.
    pub folds: u64,
    /// Map pages written by folds.
    pub pages_written: u64,
    /// Delta records written.
    pub records_written: u64,
}

/// Every volume's map, the cache, and the log since the last fold.
#[derive(Debug)]
pub struct MapSet {
    agg_id: Uuid,
    fold_cp: u64,
    committed_cp: u64,
    vols: BTreeMap<u32, VolumeMap>,
    cache: PageCache,
    delta_roots: Vec<RootRef>,
    log_bytes: u64,
    cps_since_fold: u64,
    last_replayed: Option<(u64, u64)>,
    stats: MapStats,
}

/// What a tolerant walk collects: the entries in LBA order, and the map
/// pages that did not verify.
type CollectedEntries = (Vec<(u64, MapEntry)>, Vec<u64>);

impl MapSet {
    /// An empty map for `agg_id` (a freshly formatted pool: CP 0 is the
    /// fold) with a cache of `cache_pages` map pages.
    #[must_use]
    pub fn new(agg_id: Uuid, cache_pages: usize) -> Self {
        Self {
            agg_id,
            fold_cp: 0,
            committed_cp: 0,
            vols: BTreeMap::new(),
            cache: PageCache::new(cache_pages),
            delta_roots: Vec::new(),
            log_bytes: 0,
            cps_since_fold: 0,
            last_replayed: None,
            stats: MapStats::default(),
        }
    }

    /// The map a committed CP's roots describe, before the log is replayed:
    /// `cp_seq` is that CP, `roots` the parsed map-roots table, `fold_cp`
    /// the directory's fold CP (or the CP itself with no directory), and
    /// `load_index` reads a root's index region (which is verified here).
    pub fn from_roots(
        agg_id: Uuid,
        cache_pages: usize,
        cp_seq: u64,
        fold_cp: u64,
        roots: &[MapRoot],
        mut load_index: impl FnMut(&MapRoot) -> Result<Vec<u8>, MapError>,
    ) -> Result<Self, MapError> {
        if fold_cp > cp_seq {
            return Err(MapError::Replay {
                field: "fold_cp_seq",
                value: fold_cp,
            });
        }
        let mut set = Self::new(agg_id, cache_pages);
        set.fold_cp = fold_cp;
        set.committed_cp = cp_seq;
        for (i, root) in roots.iter().enumerate() {
            if root.is_absent() {
                continue;
            }
            if root.vol as usize != i {
                return Err(MapError::Format(FormatError::BadField {
                    field: "vol",
                    value: u64::from(root.vol),
                }));
            }
            let bytes = load_index(root)?;
            set.vols
                .insert(root.vol, VolumeMap::from_root(*root, &bytes)?);
        }
        Ok(set)
    }

    /// The aggregate.
    #[must_use]
    pub const fn agg_id(&self) -> Uuid {
        self.agg_id
    }

    /// The CP of the last fold.
    #[must_use]
    pub const fn fold_cp(&self) -> u64 {
        self.fold_cp
    }

    /// The committed CP the map reflects.
    #[must_use]
    pub const fn committed_cp(&self) -> u64 {
        self.committed_cp
    }

    /// The volumes mapped, in order.
    pub fn volumes(&self) -> impl Iterator<Item = (u32, &VolumeMap)> + '_ {
        self.vols.iter().map(|(v, m)| (*v, m))
    }

    /// One volume's map.
    #[must_use]
    pub fn volume(&self, vol: u32) -> Option<&VolumeMap> {
        self.vols.get(&vol)
    }

    /// The delta regions since the fold, in CP order: what the directory
    /// lists.
    #[must_use]
    pub fn delta_roots(&self) -> &[RootRef] {
        &self.delta_roots
    }

    /// Bytes of delta pages written since the fold.
    #[must_use]
    pub const fn log_bytes_since_fold(&self) -> u64 {
        self.log_bytes
    }

    /// Consistency points since the fold that wrote a delta (the fold itself
    /// excluded): the CPs the directory spans.
    #[must_use]
    pub const fn cps_since_fold(&self) -> u64 {
        self.cps_since_fold
    }

    /// Map pages a fold would rewrite now.
    #[must_use]
    pub fn dirty_pages(&self) -> usize {
        self.vols.values().map(|v| v.dirty_pages().len()).sum()
    }

    /// Overlay entries over every volume.
    #[must_use]
    pub fn overlay_entries(&self) -> usize {
        self.vols.values().map(|v| v.overlay.len()).sum()
    }

    /// Counters.
    #[must_use]
    pub const fn stats(&self) -> MapStats {
        self.stats
    }

    /// The cache's hits and misses.
    #[must_use]
    pub const fn cache_counters(&self) -> (u64, u64) {
        self.cache.counters()
    }

    /// The map-roots table as it stands: one slot per volume index up to the
    /// highest present volume.
    #[must_use]
    pub fn roots_table(&self) -> Vec<MapRoot> {
        let count = self.vols.keys().next_back().map_or(0, |v| *v as usize + 1);
        let mut table = vec![MapRoot::ABSENT; count];
        for (vol, m) in &self.vols {
            table[*vol as usize] = m.root;
        }
        table
    }

    /// Parses a delta region for `agg_id`: every page verifies, belongs to
    /// one CP, and is in position; returns the CP and the records.
    pub fn parse_delta(bytes: &[u8], agg_id: &Uuid) -> Result<(u64, Vec<CpLogRec>), MapError> {
        if bytes.is_empty() || !bytes.len().is_multiple_of(CP_LOG_PAGE_SIZE) {
            return Err(MapError::Format(FormatError::BadLength {
                expected: bytes.len().div_ceil(CP_LOG_PAGE_SIZE).max(1) * CP_LOG_PAGE_SIZE,
                found: bytes.len(),
            }));
        }
        let mut cp_seq = None;
        let mut next_page = None;
        let mut records = Vec::new();
        for chunk in bytes.as_chunks::<CP_LOG_PAGE_SIZE>().0 {
            let page = CpLogPage::read(chunk)?;
            if page.hdr.agg_id != *agg_id {
                return Err(MapError::Format(FormatError::IdentityMismatch {
                    field: "agg_id",
                }));
            }
            match cp_seq {
                None => cp_seq = Some(page.hdr.cp_seq),
                Some(cp) if cp != page.hdr.cp_seq => {
                    return Err(MapError::Replay {
                        field: "cp_seq",
                        value: page.hdr.cp_seq,
                    })
                }
                Some(_) => {}
            }
            if let Some(expected) = next_page {
                if page.hdr.page_seq != expected {
                    return Err(MapError::Replay {
                        field: "page_seq",
                        value: page.hdr.page_seq,
                    });
                }
            }
            next_page = Some(page.hdr.page_seq + 1);
            records.extend_from_slice(page.records());
        }
        Ok((cp_seq.unwrap_or(0), records))
    }

    /// Replays one delta region of the directory, in directory order: the
    /// records of a CP after the fold go to the overlays, those of the fold
    /// CP itself are already in the pages (a record's own birth is not the
    /// rule, because a relocation keeps an older birth). Regions must
    /// arrive with non-decreasing CPs and, within a CP, consecutive page
    /// positions.
    pub fn replay(&mut self, bytes: &[u8]) -> Result<Replayed, MapError> {
        let first_page = if bytes.len() >= CP_LOG_PAGE_SIZE {
            CpLogPage::read(&bytes[..CP_LOG_PAGE_SIZE])?.hdr.page_seq
        } else {
            0
        };
        let (cp_seq, records) = Self::parse_delta(bytes, &self.agg_id)?;
        // A region below the fold was folded away; one above the CP being
        // replayed was never committed by it.
        if cp_seq < self.fold_cp || cp_seq > self.committed_cp {
            return Err(MapError::Replay {
                field: "cp_seq",
                value: cp_seq,
            });
        }
        match self.last_replayed {
            Some((last_cp, _)) if cp_seq < last_cp => {
                return Err(MapError::Replay {
                    field: "cp_seq",
                    value: cp_seq,
                })
            }
            Some((last_cp, next_page)) if cp_seq == last_cp && first_page != next_page => {
                return Err(MapError::Replay {
                    field: "page_seq",
                    value: first_page,
                })
            }
            // The first region of a CP starts at its page 0.
            Some((last_cp, _)) if cp_seq != last_cp && first_page != 0 => {
                return Err(MapError::Replay {
                    field: "page_seq",
                    value: first_page,
                })
            }
            None if first_page != 0 => {
                return Err(MapError::Replay {
                    field: "page_seq",
                    value: first_page,
                })
            }
            _ => {}
        }
        let mut applied = 0usize;
        let mut skipped = 0usize;
        let apply = cp_seq > self.fold_cp;
        for rec in &records {
            let entry = rec.map_entry();
            let vol = self.vols.get_mut(&rec.vol).ok_or(MapError::Replay {
                field: "vol",
                value: u64::from(rec.vol),
            })?;
            if rec.lba >= vol.root.lba_count {
                return Err(MapError::Replay {
                    field: "lba",
                    value: rec.lba,
                });
            }
            if u64::from(rec.birth_cp) > cp_seq {
                return Err(MapError::Replay {
                    field: "birth_cp",
                    value: u64::from(rec.birth_cp),
                });
            }
            if apply {
                vol.overlay.insert(rec.lba, entry);
                applied += 1;
            } else {
                skipped += 1;
            }
        }
        if cp_seq > self.fold_cp {
            self.log_bytes += bytes.len() as u64;
        }
        let pages = (bytes.len() / CP_LOG_PAGE_SIZE) as u64;
        self.last_replayed = Some((cp_seq, first_page + pages));
        Ok(Replayed {
            cp_seq,
            records,
            applied,
            skipped,
        })
    }

    /// Records the directory's delta list and the CPs it spans, after the
    /// deltas were replayed.
    pub fn set_delta_roots(&mut self, roots: Vec<RootRef>, cps_since_fold: u64) {
        self.delta_roots = roots;
        self.cps_since_fold = cps_since_fold;
    }

    /// The committed entry of `lba` of `vol`: the overlay first, then the
    /// page the index names (read and verified), else unmapped.
    pub fn get(
        &mut self,
        reader: &mut dyn PageReader,
        vol: u32,
        lba: u64,
    ) -> Result<MapEntry, MapError> {
        let v = self.vols.get(&vol).ok_or(MapError::NoSuchVolume { vol })?;
        v.check_lba(vol, lba)?;
        if let Some(e) = v.overlay.get(&lba) {
            return Ok(*e);
        }
        let entry = v.index[MapPage::page_of(lba) as usize];
        if entry.is_none() {
            return Ok(MapEntry::UNMAPPED);
        }
        let page = self.page(reader, &entry)?;
        Ok(page.entries[MapPage::slot_of(lba)])
    }

    /// The page an index entry names, through the cache.
    fn page(
        &mut self,
        reader: &mut dyn PageReader,
        entry: &PageIndexEntry,
    ) -> Result<&MapPage, MapError> {
        if !self.cache.pages.contains_key(&entry.page_pba) {
            self.stats.page_reads += 1;
        }
        load_page(&mut self.cache, reader, entry)
    }

    /// Builds everything CP `input.cp_seq` writes for the map and reports
    /// what it replaces and frees. Nothing in the map changes until
    /// [`apply`](Self::apply); the cache may fill.
    pub fn prepare_cp(
        &mut self,
        reader: &mut dyn PageReader,
        input: &CpInput,
        writer: &mut dyn RegionWriter,
    ) -> Result<PreparedCp, MapError> {
        if input.cp_seq == 0 || input.cp_seq > MAX_CP_SEQ || input.cp_seq <= self.committed_cp {
            return Err(MapError::Replay {
                field: "cp_seq",
                value: input.cp_seq,
            });
        }
        if input.max_region_pages == 0 {
            return Err(MapError::Internal("max_region_pages is zero".into()));
        }
        // Rollbacks, resizes and deletions rewrite a volume's root outside
        // the delta log, so the CP that carries them must be a fold: a boot
        // then replays no record from before the change (a record for a
        // deleted volume, or beyond a shrunk one, would refuse the boot).
        let changes_roots =
            !input.rollbacks.is_empty() || !input.resizes.is_empty() || !input.deletes.is_empty();
        if changes_roots && !input.fold {
            return Err(MapError::Internal(
                "a rollback, resize or deletion needs a fold CP".into(),
            ));
        }
        for rb in &input.rollbacks {
            let v = self
                .vols
                .get(&rb.vol)
                .ok_or(MapError::NoSuchVolume { vol: rb.vol })?;
            rb.root.validate()?;
            if rb.root.vol != rb.vol
                || rb.root.is_absent()
                || rb.index.len() != map_pages_for(rb.root.lba_count)
                || rb.birth_cp > self.committed_cp
            {
                return Err(MapError::Internal(format!(
                    "rollback of volume {} names a root that is not its own",
                    rb.vol
                )));
            }
            let _ = v;
            if input.deltas.iter().any(|(vol, _, _)| *vol == rb.vol) {
                return Err(MapError::Internal(format!(
                    "rollback of volume {} with deltas in the same CP",
                    rb.vol
                )));
            }
        }
        for (vol, lba_count) in &input.resizes {
            let v = self
                .vols
                .get(vol)
                .ok_or(MapError::NoSuchVolume { vol: *vol })?;
            if *lba_count == 0 || *lba_count > MAX_LBA + 1 {
                return Err(MapError::BadSize {
                    vol: *vol,
                    lba_count: *lba_count,
                });
            }
            if *lba_count < v.lba_count()
                && input
                    .deltas
                    .iter()
                    .any(|(dv, lba, _)| dv == vol && *lba >= *lba_count)
            {
                return Err(MapError::Internal(format!(
                    "shrink of volume {vol} with deltas beyond its new size"
                )));
            }
            if input.rollbacks.iter().any(|r| r.vol == *vol) {
                return Err(MapError::Internal(format!(
                    "volume {vol} is rolled back and resized in one CP"
                )));
            }
        }
        for vol in &input.deletes {
            if !self.vols.contains_key(vol) {
                return Err(MapError::NoSuchVolume { vol: *vol });
            }
            if input.deltas.iter().any(|(dv, _, _)| dv == vol)
                || input.rollbacks.iter().any(|r| r.vol == *vol)
                || input.resizes.iter().any(|(rv, _)| rv == vol)
                || input.new_volumes.iter().any(|(nv, _)| nv == vol)
            {
                return Err(MapError::Internal(format!(
                    "volume {vol} is deleted and changed in one CP"
                )));
            }
        }
        // New volumes: a root with an all-absent index, placed now.
        let mut new_volumes = Vec::new();
        let mut written = Vec::new();
        let fold_cp_after = if input.fold {
            input.cp_seq
        } else {
            self.fold_cp
        };
        let mut sizes: BTreeMap<u32, u64> =
            self.vols.iter().map(|(v, m)| (*v, m.lba_count())).collect();
        for (vol, lba_count) in &input.new_volumes {
            if *vol as usize >= MAX_VOLS || *lba_count == 0 || *lba_count > MAX_LBA + 1 {
                return Err(MapError::BadSize {
                    vol: *vol,
                    lba_count: *lba_count,
                });
            }
            if sizes.contains_key(vol) {
                return Err(MapError::VolumeExists { vol: *vol });
            }
            let index = vec![PageIndexEntry::NONE; map_pages_for(*lba_count)];
            let bytes = index_image(&index);
            let root_ref = writer.place(&bytes)?;
            written.push(root_ref);
            let root = MapRoot {
                vol: *vol,
                flags: 0,
                index_pba: root_ref.pba,
                index_pages: root_ref.pages / INDEX_PAGE_BLOCKS,
                index_crc: root_ref.crc32c,
                lba_count: *lba_count,
                checkpoint_cp_seq: fold_cp_after,
                max_birth: 0,
                rsvd0: 0,
                rsvd1: [0; 16],
            };
            root.validate()?;
            sizes.insert(*vol, *lba_count);
            new_volumes.push((*vol, root, index));
        }
        // The deltas, deduplicated: the last entry for a block wins.
        let mut deltas: BTreeMap<(u32, u64), MapEntry> = BTreeMap::new();
        for (vol, lba, entry) in &input.deltas {
            entry.validate()?;
            if entry.is_unmapped() || u64::from(entry.birth_cp) > input.cp_seq {
                return Err(MapError::Format(FormatError::BadField {
                    field: "birth_cp",
                    value: u64::from(entry.birth_cp),
                }));
            }
            let size = sizes
                .get(vol)
                .copied()
                .ok_or(MapError::NoSuchVolume { vol: *vol })?;
            if *lba >= size {
                return Err(MapError::OutOfRange {
                    vol: *vol,
                    lba: *lba,
                });
            }
            deltas.insert((*vol, *lba), *entry);
        }
        // What each delta replaces (a page read per cold page). A page that
        // reads but does not verify leaves the old entry unknown: the CP
        // still commits, with the old entry taken as unmapped, and the page
        // is reported; an I/O error is the reader's to report.
        let mut replaced = Vec::with_capacity(deltas.len());
        let mut damaged_pages: Vec<(u32, u64)> = Vec::new();
        for ((vol, lba), new) in &deltas {
            let old = if self.vols.contains_key(vol) {
                match self.get(reader, *vol, *lba) {
                    Ok(old) => old,
                    Err(MapError::Format(_)) => {
                        let page_no = MapPage::page_of(*lba);
                        if !damaged_pages.contains(&(*vol, page_no)) {
                            damaged_pages.push((*vol, page_no));
                        }
                        MapEntry::UNMAPPED
                    }
                    Err(e) => return Err(e),
                }
            } else {
                MapEntry::UNMAPPED
            };
            replaced.push(Replaced {
                vol: *vol,
                lba: *lba,
                old,
                new: *new,
            });
        }
        // The delta regions. A fold writes none: its records go into the
        // pages it rewrites, and a region with the fold's own CP would never
        // be replayed.
        let mut delta_roots = Vec::new();
        let mut delta_bytes = 0u64;
        if !input.fold {
            let mut region: Vec<u8> = Vec::new();
            let mut page = CpLogPage::new(self.agg_id, input.cp_seq, input.cp_nonce, 0);
            let mut page_seq = 0u64;
            let max_region_bytes = input.max_region_pages as usize * CP_LOG_PAGE_SIZE;
            let flush_page = |page: &mut CpLogPage,
                              region: &mut Vec<u8>,
                              delta_roots: &mut Vec<RootRef>,
                              writer: &mut dyn RegionWriter|
             -> Result<(), MapError> {
                page.seal();
                region.extend_from_slice(page.as_bytes());
                if region.len() >= max_region_bytes {
                    let root = writer.place(region)?;
                    delta_roots.push(root);
                    region.clear();
                }
                Ok(())
            };
            for ((vol, lba), entry) in &deltas {
                let rec = CpLogRec::from_entry(*vol, *lba, *entry)?;
                if page.push(rec).is_none() {
                    flush_page(&mut page, &mut region, &mut delta_roots, writer)?;
                    page_seq += 1;
                    page = CpLogPage::new(self.agg_id, input.cp_seq, input.cp_nonce, page_seq);
                    page.push(rec)
                        .ok_or_else(|| MapError::Internal("an empty page takes a record".into()))?;
                }
            }
            if page.hdr.count > 0 {
                flush_page(&mut page, &mut region, &mut delta_roots, writer)?;
            }
            if !region.is_empty() {
                let root = writer.place(&region)?;
                delta_roots.push(root);
            }
            for root in &delta_roots {
                delta_bytes += root.bytes();
            }
            written.extend_from_slice(&delta_roots);
            self.stats.records_written += deltas.len() as u64;
        }

        // The fold: dirty pages rewritten copy on write, indexes and roots
        // of the volumes concerned rewritten, every root re-stamped; a
        // rolled-back volume takes the snapshot's root and index whole, a
        // resized one a longer or shorter index, a deleted one leaves.
        let mut fold_roots = Vec::new();
        let mut new_pages = Vec::new();
        let mut freed_pages = Vec::new();
        let mut freed_indexes = Vec::new();
        let mut freed_deltas = Vec::new();
        let mut page_roots = Vec::new();
        let mut released = Vec::new();
        if input.fold {
            // Every volume with a root before this CP, and every volume
            // this CP creates: a record for a new volume goes into its
            // pages like any other (a fold writes no delta region).
            let mut folded: Vec<(u32, Vec<PageIndexEntry>, MapRoot, bool)> = self
                .vols
                .iter()
                .map(|(vol, v)| (*vol, v.index.clone(), v.root, false))
                .collect();
            for (vol, root, index) in &new_volumes {
                folded.push((*vol, index.clone(), *root, true));
            }
            for (vol, mut index, mut root, is_new) in folded {
                if input.deletes.contains(&vol) {
                    // Everything the volume maps is released and every
                    // region of its tree is freed.
                    let (gone, damaged) = self.collect_entries(reader, vol, 0, 0)?;
                    released.extend(gone.into_iter().map(|(lba, e)| (vol, lba, e)));
                    for page_no in damaged {
                        if !damaged_pages.contains(&(vol, page_no)) {
                            damaged_pages.push((vol, page_no));
                        }
                    }
                    for (page_no, old) in index.iter().enumerate() {
                        if !old.is_none() {
                            freed_pages.push(FreedPage {
                                vol,
                                page_no: page_no as u64,
                                root: page_root_ref(old),
                            });
                        }
                    }
                    freed_indexes.push(FreedIndex {
                        vol,
                        root: index_root_ref(&root),
                    });
                    continue;
                }
                if let Some(rb) = input.rollbacks.iter().find(|r| r.vol == vol) {
                    // The live entries born after the snapshot go; the
                    // snapshot's pages become the live pages, so a live page
                    // the snapshot does not share is freed, as is the live
                    // index when it is not the snapshot's.
                    let (gone, damaged) = self.collect_entries(reader, vol, 0, rb.birth_cp)?;
                    released.extend(gone.into_iter().map(|(lba, e)| (vol, lba, e)));
                    for page_no in damaged {
                        if !damaged_pages.contains(&(vol, page_no)) {
                            damaged_pages.push((vol, page_no));
                        }
                    }
                    for (page_no, old) in index.iter().enumerate() {
                        let shared = rb
                            .index
                            .get(page_no)
                            .is_some_and(|e| !e.is_none() && e.page_pba == old.page_pba);
                        if !old.is_none() && !shared {
                            freed_pages.push(FreedPage {
                                vol,
                                page_no: page_no as u64,
                                root: page_root_ref(old),
                            });
                        }
                    }
                    if root.index_pba != rb.root.index_pba {
                        freed_indexes.push(FreedIndex {
                            vol,
                            root: index_root_ref(&root),
                        });
                    }
                    let new_root = MapRoot {
                        checkpoint_cp_seq: input.cp_seq,
                        ..rb.root
                    };
                    new_root.validate()?;
                    fold_roots.push((vol, new_root, rb.index.clone()));
                    continue;
                }
                let mut dirty: BTreeSet<u64> = if is_new {
                    BTreeSet::new()
                } else {
                    self.vols[&vol].dirty_pages()
                };
                for ((dv, lba), _) in deltas.range((vol, 0)..=(vol, u64::MAX)) {
                    debug_assert_eq!(*dv, vol);
                    dirty.insert(MapPage::page_of(*lba));
                }
                // A resize: the index grows with absent pages, or shrinks
                // (the entries beyond the new size are released, their
                // pages freed, and a page the new end cuts through is
                // rewritten without them).
                let resize = input
                    .resizes
                    .iter()
                    .find(|(v, _)| *v == vol)
                    .map(|(_, size)| *size);
                let new_size = resize.unwrap_or(root.lba_count);
                if let Some(size) = resize {
                    let pages = map_pages_for(size);
                    if size < root.lba_count {
                        let (gone, damaged) = self.collect_entries(reader, vol, size, 0)?;
                        released.extend(gone.into_iter().map(|(lba, e)| (vol, lba, e)));
                        for page_no in damaged {
                            if !damaged_pages.contains(&(vol, page_no)) {
                                damaged_pages.push((vol, page_no));
                            }
                        }
                        for (page_no, old) in index.iter().enumerate().skip(pages) {
                            if !old.is_none() {
                                freed_pages.push(FreedPage {
                                    vol,
                                    page_no: page_no as u64,
                                    root: page_root_ref(old),
                                });
                            }
                        }
                        index.truncate(pages);
                        dirty.retain(|p| (*p as usize) < pages);
                        if !size.is_multiple_of(MAP_ENTRIES_PER_PAGE as u64)
                            && !index[pages - 1].is_none()
                        {
                            dirty.insert(pages as u64 - 1);
                        }
                    } else {
                        index.resize(pages, PageIndexEntry::NONE);
                    }
                    root.lba_count = size;
                }
                let mut max_birth = root.max_birth;
                for page_no in &dirty {
                    let old = index[*page_no as usize];
                    let mut page = if old.is_none() {
                        MapPage::zeroed()
                    } else {
                        *self.page(reader, &old)?
                    };
                    let first = MapPage::first_lba(*page_no);
                    let last = first + MAP_ENTRIES_PER_PAGE as u64 - 1;
                    if let Some(v) = self.vols.get(&vol) {
                        for (lba, e) in v.overlay.range(first..=last) {
                            page.entries[MapPage::slot_of(*lba)] = *e;
                        }
                    }
                    for ((_, lba), e) in deltas.range((vol, first)..=(vol, last)) {
                        page.entries[MapPage::slot_of(*lba)] = *e;
                    }
                    // Beyond a shrunk volume's end nothing is mapped.
                    if new_size <= last {
                        for lba in new_size.max(first)..=last {
                            page.entries[MapPage::slot_of(lba)] = MapEntry::UNMAPPED;
                        }
                    }
                    let root_ref = writer.place(page.as_bytes())?;
                    written.push(root_ref);
                    page_roots.push(root_ref);
                    let entry = page.index_entry(root_ref.pba);
                    debug_assert_eq!(entry.page_crc, root_ref.crc32c);
                    max_birth = max_birth.max(entry.max_birth);
                    index[*page_no as usize] = entry;
                    if !old.is_none() {
                        freed_pages.push(FreedPage {
                            vol,
                            page_no: *page_no,
                            root: page_root_ref(&old),
                        });
                    }
                    new_pages.push((root_ref.pba, Box::new(page)));
                    self.stats.pages_written += 1;
                }
                let new_root = if dirty.is_empty() && resize.is_none() {
                    MapRoot {
                        checkpoint_cp_seq: input.cp_seq,
                        ..root
                    }
                } else {
                    let bytes = index_image(&index);
                    let root_ref = writer.place(&bytes)?;
                    written.push(root_ref);
                    freed_indexes.push(FreedIndex {
                        vol,
                        root: index_root_ref(&root),
                    });
                    MapRoot {
                        index_pba: root_ref.pba,
                        index_pages: root_ref.pages / INDEX_PAGE_BLOCKS,
                        index_crc: root_ref.crc32c,
                        checkpoint_cp_seq: input.cp_seq,
                        max_birth,
                        ..root
                    }
                };
                new_root.validate()?;
                fold_roots.push((vol, new_root, index));
            }
            freed_deltas = self.delta_roots.clone();
        }

        // The directory and the roots table.
        let dir = CpLogDir {
            cp_seq: input.cp_seq,
            fold_cp_seq: fold_cp_after,
            deltas: if input.fold {
                delta_roots.clone()
            } else {
                let mut all = self.delta_roots.clone();
                all.extend_from_slice(&delta_roots);
                all
            },
            pages: page_roots,
        };
        let highest = sizes
            .keys()
            .rfind(|v| !input.deletes.contains(v))
            .map_or(0, |v| *v as usize + 1);
        let mut roots_table = vec![MapRoot::ABSENT; highest];
        for (vol, m) in &self.vols {
            if !input.deletes.contains(vol) {
                roots_table[*vol as usize] = m.root;
            }
        }
        for (vol, root, _) in &new_volumes {
            roots_table[*vol as usize] = *root;
        }
        // A fold's root wins over a new volume's first root: the fold
        // rewrote it with the CP's own records.
        for (vol, root, _) in &fold_roots {
            roots_table[*vol as usize] = *root;
        }
        let update = MapUpdate {
            cp_seq: input.cp_seq,
            fold: input.fold,
            deltas: deltas.iter().map(|((v, l), e)| (*v, *l, *e)).collect(),
            delta_roots,
            delta_bytes,
            new_volumes,
            fold_roots: fold_roots.clone(),
            new_pages,
            deletes: input.deletes.clone(),
        };
        Ok(PreparedCp {
            update,
            dir,
            roots_table,
            replaced,
            written,
            freed_pages,
            freed_indexes,
            freed_deltas,
            fold_roots,
            damaged_pages,
            released,
        })
    }

    /// Every committed entry of `vol` at or above `from_lba` with a birth
    /// above `base_cp` (data and holes), in LBA order, and the map pages
    /// that did not verify on the way (their entries are unknown and
    /// reported, never failing the walk). The overlay is consulted first.
    fn collect_entries(
        &mut self,
        reader: &mut dyn PageReader,
        vol: u32,
        from_lba: u64,
        base_cp: u64,
    ) -> Result<CollectedEntries, MapError> {
        let v = self.vols.get(&vol).ok_or(MapError::NoSuchVolume { vol })?;
        let index = v.index.clone();
        let overlay: Vec<(u64, MapEntry)> =
            v.overlay.range(from_lba..).map(|(l, e)| (*l, *e)).collect();
        let lba_count = v.lba_count();
        let mut out = Vec::new();
        let mut damaged = Vec::new();
        let first_page = MapPage::page_of(from_lba) as usize;
        for (page_no, entry) in index.iter().enumerate().skip(first_page) {
            let first = MapPage::first_lba(page_no as u64);
            let last = (first + MAP_ENTRIES_PER_PAGE as u64 - 1).min(lba_count - 1);
            let mut page_entries: Option<Box<MapPage>> = None;
            if !entry.is_none() && u64::from(entry.max_birth) > base_cp {
                match self.page(reader, entry) {
                    Ok(page) => page_entries = Some(Box::new(*page)),
                    Err(MapError::Format(_)) => damaged.push(page_no as u64),
                    Err(e) => return Err(e),
                }
            }
            let mut ov = overlay
                .iter()
                .filter(|(l, _)| *l >= first && *l <= last)
                .peekable();
            for lba in first.max(from_lba)..=last {
                let in_overlay = match ov.peek() {
                    Some((l, e)) if *l == lba => {
                        ov.next();
                        Some(*e)
                    }
                    _ => None,
                };
                let e = match (in_overlay, &page_entries) {
                    (Some(e), _) => e,
                    (None, Some(page)) => page.entries[MapPage::slot_of(lba)],
                    (None, None) => continue,
                };
                if !e.is_unmapped() && u64::from(e.birth_cp) > base_cp {
                    out.push((lba, e));
                }
            }
        }
        Ok((out, damaged))
    }

    /// Rewrites a pinned root's pages so the listed blocks point at new
    /// addresses (a relocation): every `(lba, old, new)` names the entry
    /// the page holds now and what it becomes, with the same birth and
    /// CRC. The touched pages are written copy on write, then a new index
    /// region; nothing in this map changes (a pinned root is not the live
    /// map), and the caller places the new root page and queues the old
    /// regions for freeing. Refuses an entry that does not match the page.
    pub fn rewrite_pinned(
        &mut self,
        reader: &mut dyn PageReader,
        writer: &mut dyn RegionWriter,
        pinned: &PinnedRoot,
        changes: &[(u64, MapEntry, MapEntry)],
    ) -> Result<RewrittenRoot, MapError> {
        let mut by_page: BTreeMap<u64, Vec<(u64, MapEntry, MapEntry)>> = BTreeMap::new();
        for change in changes {
            let (lba, old, new) = *change;
            if lba >= pinned.root.lba_count {
                return Err(MapError::OutOfRange {
                    vol: pinned.vol,
                    lba,
                });
            }
            new.validate()?;
            if !old.is_data()
                || !new.is_data()
                || old.birth_cp != new.birth_cp
                || old.crc32c != new.crc32c
            {
                return Err(MapError::Internal(format!(
                    "relocation of {}/{lba} changes more than the address",
                    pinned.vol
                )));
            }
            by_page
                .entry(MapPage::page_of(lba))
                .or_default()
                .push(*change);
        }
        let mut index = pinned.index.clone();
        let mut pages = Vec::new();
        let mut new_pages = Vec::new();
        let mut freed_pages = Vec::new();
        for (page_no, changes) in by_page {
            let old_entry = index[page_no as usize];
            if old_entry.is_none() {
                return Err(MapError::Internal(format!(
                    "relocation in an absent page {page_no} of volume {}",
                    pinned.vol
                )));
            }
            let mut page = *self.page(reader, &old_entry)?;
            for (lba, old, new) in changes {
                let slot = MapPage::slot_of(lba);
                if page.entries[slot] != old {
                    return Err(MapError::Internal(format!(
                        "relocation of {}/{lba}: the pinned page holds another entry",
                        pinned.vol
                    )));
                }
                page.entries[slot] = new;
            }
            let root_ref = writer.place(page.as_bytes())?;
            pages.push(root_ref);
            let entry = page.index_entry(root_ref.pba);
            debug_assert_eq!(entry.max_birth, old_entry.max_birth);
            index[page_no as usize] = entry;
            freed_pages.push(FreedPage {
                vol: pinned.vol,
                page_no,
                root: page_root_ref(&old_entry),
            });
            new_pages.push((root_ref.pba, Box::new(page)));
            self.stats.pages_written += 1;
        }
        let bytes = index_image(&index);
        let index_ref = writer.place(&bytes)?;
        let root = MapRoot {
            index_pba: index_ref.pba,
            index_pages: index_ref.pages / INDEX_PAGE_BLOCKS,
            index_crc: index_ref.crc32c,
            ..pinned.root
        };
        root.validate()?;
        Ok(RewrittenRoot {
            pinned: PinnedRoot {
                vol: pinned.vol,
                root,
                index,
            },
            pages,
            index: index_ref,
            freed_pages,
            freed_index: FreedIndex {
                vol: pinned.vol,
                root: index_root_ref(&pinned.root),
            },
            new_pages,
        })
    }

    /// Keeps pages a committed CP wrote (a rewritten snapshot's) in the
    /// cache.
    pub fn cache_pages(&mut self, pages: Vec<(u64, Box<MapPage>)>) {
        for (pba, page) in pages {
            self.cache.insert(pba, page);
        }
    }

    /// Makes a prepared CP the committed state. Nothing here can refuse.
    pub fn apply(&mut self, update: MapUpdate) {
        for vol in &update.deletes {
            self.vols.remove(vol);
        }
        for (vol, root, index) in update.new_volumes {
            self.vols.insert(
                vol,
                VolumeMap {
                    root,
                    index,
                    overlay: BTreeMap::new(),
                },
            );
        }
        if update.fold {
            for (vol, root, index) in update.fold_roots {
                if let Some(v) = self.vols.get_mut(&vol) {
                    v.root = root;
                    v.index = index;
                    v.overlay.clear();
                }
            }
            for v in self.vols.values_mut() {
                v.root.checkpoint_cp_seq = update.cp_seq;
            }
            for (pba, page) in update.new_pages {
                self.cache.insert(pba, page);
            }
            self.fold_cp = update.cp_seq;
            self.delta_roots = update.delta_roots;
            self.log_bytes = update.delta_bytes;
            self.cps_since_fold = 0;
            self.stats.folds += 1;
        } else {
            for (vol, lba, entry) in update.deltas {
                if let Some(v) = self.vols.get_mut(&vol) {
                    v.overlay.insert(lba, entry);
                }
            }
            if !update.delta_roots.is_empty() {
                self.cps_since_fold += 1;
            }
            self.delta_roots.extend(update.delta_roots);
            self.log_bytes += update.delta_bytes;
        }
        self.committed_cp = update.cp_seq;
        self.last_replayed = None;
    }

    /// Walks the committed map of `vol` and calls `f(lba, entry)` for every
    /// block whose birth is above `base_cp` (data and holes), in LBA order,
    /// reading only the pages whose `max_birth` says they may hold one.
    pub fn diff(
        &mut self,
        reader: &mut dyn PageReader,
        vol: u32,
        base_cp: u64,
        mut f: impl FnMut(u64, MapEntry),
    ) -> Result<(), MapError> {
        let v = self.vols.get(&vol).ok_or(MapError::NoSuchVolume { vol })?;
        let index = v.index.clone();
        let overlay: Vec<(u64, MapEntry)> = v.overlay.iter().map(|(l, e)| (*l, *e)).collect();
        let mut ov = overlay.iter().peekable();
        for (page_no, entry) in index.iter().enumerate() {
            let first = MapPage::first_lba(page_no as u64);
            let last = first + MAP_ENTRIES_PER_PAGE as u64 - 1;
            let page_may = !entry.is_none() && u64::from(entry.max_birth) > base_cp;
            let mut page_entries: Option<Box<MapPage>> = None;
            if page_may {
                page_entries = Some(Box::new(*self.page(reader, entry)?));
            }
            let mut emitted: BTreeSet<u64> = BTreeSet::new();
            // Overlay entries in this page, in order, interleaved with the
            // page's own entries that they do not override.
            let mut next_ov = ov.peek().filter(|(l, _)| *l <= last).map(|(l, e)| (*l, *e));
            let mut lba = first;
            while lba <= last {
                if let Some((ol, oe)) = next_ov {
                    if ol == lba {
                        ov.next();
                        next_ov = ov.peek().filter(|(l, _)| *l <= last).map(|(l, e)| (*l, *e));
                        if u64::from(oe.birth_cp) > base_cp && !oe.is_unmapped() {
                            f(lba, oe);
                        }
                        emitted.insert(lba);
                        lba += 1;
                        continue;
                    }
                }
                if let Some(page) = &page_entries {
                    let e = page.entries[MapPage::slot_of(lba)];
                    if u64::from(e.birth_cp) > base_cp && !e.is_unmapped() {
                        f(lba, e);
                    }
                    lba += 1;
                } else {
                    // No page to read: skip to the next overlay entry.
                    match next_ov {
                        Some((ol, _)) => lba = ol,
                        None => break,
                    }
                }
            }
        }
        Ok(())
    }

    /// A snapshot view of `vol` as it stands now, which is complete only
    /// right after a fold (the overlay must be empty).
    pub fn pin(&self, vol: u32) -> Result<PinnedRoot, MapError> {
        let v = self.vols.get(&vol).ok_or(MapError::NoSuchVolume { vol })?;
        if !v.overlay.is_empty() {
            return Err(MapError::Internal(format!(
                "volume {vol} has {} overlay entries; pin after a fold",
                v.overlay.len()
            )));
        }
        Ok(PinnedRoot {
            vol,
            root: v.root,
            index: v.index.clone(),
        })
    }

    /// Reads a map page for a pinned root through this map's cache.
    pub fn pinned_get(
        &mut self,
        reader: &mut dyn PageReader,
        pinned: &PinnedRoot,
        lba: u64,
    ) -> Result<MapEntry, MapError> {
        if lba >= pinned.root.lba_count {
            return Err(MapError::OutOfRange {
                vol: pinned.vol,
                lba,
            });
        }
        let entry = pinned.index[MapPage::page_of(lba) as usize];
        if entry.is_none() {
            return Ok(MapEntry::UNMAPPED);
        }
        let page = self.page(reader, &entry)?;
        Ok(page.entries[MapPage::slot_of(lba)])
    }

    /// The birth walk over a pinned root, through this map's cache.
    pub fn pinned_diff(
        &mut self,
        reader: &mut dyn PageReader,
        pinned: &PinnedRoot,
        base_cp: u64,
        mut f: impl FnMut(u64, MapEntry),
    ) -> Result<(), MapError> {
        for (page_no, entry) in pinned.index.iter().enumerate() {
            if entry.is_none() || u64::from(entry.max_birth) <= base_cp {
                continue;
            }
            let page = *self.page(reader, entry)?;
            let first = MapPage::first_lba(page_no as u64);
            for (slot, e) in page.entries.iter().enumerate() {
                if u64::from(e.birth_cp) > base_cp && !e.is_unmapped() {
                    f(first + slot as u64, *e);
                }
            }
        }
        Ok(())
    }

    /// [`pinned_diff`](Self::pinned_diff) that walks past a page that does
    /// not verify ([`MapError::Format`]): its entries are not reported, and
    /// its page number is returned, in order, so the caller can treat the
    /// tree as damaged without stopping (Step 6A: a snapshot's damaged page
    /// must not stop the consistency points of the pool). A read error
    /// still stops the walk: it may be transient.
    pub fn pinned_diff_tolerant(
        &mut self,
        reader: &mut dyn PageReader,
        pinned: &PinnedRoot,
        base_cp: u64,
        mut f: impl FnMut(u64, MapEntry),
    ) -> Result<Vec<u64>, MapError> {
        let mut damaged = Vec::new();
        for (page_no, entry) in pinned.index.iter().enumerate() {
            if entry.is_none() || u64::from(entry.max_birth) <= base_cp {
                continue;
            }
            let page = match self.page(reader, entry) {
                Ok(page) => *page,
                Err(MapError::Format(_)) => {
                    damaged.push(page_no as u64);
                    continue;
                }
                Err(e) => return Err(e),
            };
            let first = MapPage::first_lba(page_no as u64);
            for (slot, e) in page.entries.iter().enumerate() {
                if u64::from(e.birth_cp) > base_cp && !e.is_unmapped() {
                    f(first + slot as u64, *e);
                }
            }
        }
        Ok(damaged)
    }
}

/// What [`MapSet::rewrite_pinned`] produced: the pinned root as it stands
/// after the relocation, the regions written and the regions the old root
/// stops referencing.
#[derive(Debug, Clone)]
pub struct RewrittenRoot {
    /// The root with its new index.
    pub pinned: PinnedRoot,
    /// The map pages written, for the CP's directory.
    pub pages: Vec<RootRef>,
    /// The index region written.
    pub index: RootRef,
    /// The pages the old root referenced at the rewritten page numbers.
    pub freed_pages: Vec<FreedPage>,
    /// The old index region.
    pub freed_index: FreedIndex,
    /// The pages, for the cache once the CP commits.
    pub new_pages: Vec<(u64, Box<MapPage>)>,
}

/// A snapshot's root: a volume's map as of a fold CP, complete on disk,
/// read without an overlay.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PinnedRoot {
    /// The volume.
    pub vol: u32,
    /// The pinned root.
    pub root: MapRoot,
    /// Its index, one entry per map page.
    pub index: Vec<PageIndexEntry>,
}

impl PinnedRoot {
    /// A pinned root from its record and the index region it names
    /// (verified against the root).
    pub fn load(root: MapRoot, index_bytes: &[u8]) -> Result<Self, MapError> {
        root.validate()?;
        if root.is_absent() {
            return Err(MapError::BadSize {
                vol: root.vol,
                lba_count: 0,
            });
        }
        let index = index_entries(&root, index_bytes)?;
        Ok(Self {
            vol: root.vol,
            root,
            index,
        })
    }

    /// A pinned root built from a fold's result.
    #[must_use]
    pub fn from_parts(vol: u32, root: MapRoot, index: Vec<PageIndexEntry>) -> Self {
        Self { vol, root, index }
    }

    /// The entry of `lba`, read through `cache`.
    pub fn get(
        &self,
        cache: &mut PageCache,
        reader: &mut dyn PageReader,
        lba: u64,
    ) -> Result<MapEntry, MapError> {
        if lba >= self.root.lba_count {
            return Err(MapError::OutOfRange { vol: self.vol, lba });
        }
        let entry = self.index[MapPage::page_of(lba) as usize];
        if entry.is_none() {
            return Ok(MapEntry::UNMAPPED);
        }
        let page = load_page(cache, reader, &entry)?;
        Ok(page.entries[MapPage::slot_of(lba)])
    }

    /// Calls `f(lba, entry)` for every block born after `base_cp`, in LBA
    /// order, reading only the pages whose `max_birth` says they may hold
    /// one.
    pub fn diff(
        &self,
        cache: &mut PageCache,
        reader: &mut dyn PageReader,
        base_cp: u64,
        mut f: impl FnMut(u64, MapEntry),
    ) -> Result<(), MapError> {
        for (page_no, entry) in self.index.iter().enumerate() {
            if entry.is_none() || u64::from(entry.max_birth) <= base_cp {
                continue;
            }
            let page = load_page(cache, reader, entry)?;
            let first = MapPage::first_lba(page_no as u64);
            for (slot, e) in page.entries.iter().enumerate() {
                if u64::from(e.birth_cp) > base_cp && !e.is_unmapped() {
                    f(first + slot as u64, *e);
                }
            }
        }
        Ok(())
    }

    /// `true` when `page_pba` is a page this root references, at map page
    /// `page_no`: what keeps a page alive after a fold replaced it.
    #[must_use]
    pub fn references_page(&self, page_no: u64, page_pba: u64) -> bool {
        self.index
            .get(page_no as usize)
            .is_some_and(|e| !e.is_none() && e.page_pba == page_pba)
    }

    /// `true` when `index_pba` is this root's index region.
    #[must_use]
    pub const fn references_index(&self, index_pba: u64) -> bool {
        self.root.index_pages > 0 && self.root.index_pba == index_pba
    }

    /// Every region the root references: its index and its pages.
    #[must_use]
    pub fn regions(&self) -> Vec<RootRef> {
        let mut out = vec![index_root_ref(&self.root)];
        out.extend(
            self.index
                .iter()
                .filter(|e| !e.is_none())
                .map(page_root_ref),
        );
        out
    }
}

/// Every region a volume map references: its index and its pages (what a
/// boot counts as live metadata).
#[must_use]
pub fn volume_regions(v: &VolumeMap) -> Vec<RootRef> {
    let mut out = vec![index_root_ref(&v.root)];
    out.extend(v.index.iter().filter(|e| !e.is_none()).map(page_root_ref));
    out
}

/// A reader over a byte vector (the tests and the simulator's checks).
#[derive(Debug, Default, Clone)]
pub struct VecReader {
    /// The device image.
    pub bytes: Vec<u8>,
    /// Reads served.
    pub reads: u64,
}

impl PageReader for VecReader {
    fn read_blocks(&mut self, pba: u64, dst: &mut [u8]) -> Result<(), MapError> {
        let start = (pba as usize).saturating_mul(BLOCK_SIZE);
        let end = start.saturating_add(dst.len());
        if !dst.len().is_multiple_of(BLOCK_SIZE) || end > self.bytes.len() {
            return Err(MapError::Io {
                pba,
                bytes: dst.len(),
                reason: "beyond the image".into(),
            });
        }
        self.reads += 1;
        dst.copy_from_slice(&self.bytes[start..end]);
        Ok(())
    }
}

/// A writer that appends regions to a byte vector at page granularity and
/// names them by block number (the tests and the simulator's checks).
#[derive(Debug, Default, Clone)]
pub struct VecWriter {
    /// The image written so far.
    pub bytes: Vec<u8>,
    /// Regions placed.
    pub placed: Vec<RootRef>,
}

impl VecWriter {
    /// A writer whose first region lands at block `first_pba`.
    #[must_use]
    pub fn starting_at(first_pba: u64) -> Self {
        Self {
            bytes: vec![0u8; first_pba as usize * BLOCK_SIZE],
            placed: Vec::new(),
        }
    }
}

impl RegionWriter for VecWriter {
    fn place(&mut self, bytes: &[u8]) -> Result<RootRef, MapError> {
        if bytes.is_empty() || !bytes.len().is_multiple_of(BLOCK_SIZE) {
            return Err(MapError::Io {
                pba: (self.bytes.len() / BLOCK_SIZE) as u64,
                bytes: bytes.len(),
                reason: "not whole pages".into(),
            });
        }
        let pba = (self.bytes.len() / BLOCK_SIZE) as u64;
        let root = RootRef::for_region(pba, bytes)?;
        self.bytes.extend_from_slice(bytes);
        self.placed.push(root);
        Ok(root)
    }
}

const _: () = {
    assert!(MAP_PAGE_BLOCKS == 4);
    assert!(INDEX_PAGE_BLOCKS == 4);
    assert!(MAP_ENTRIES_PER_PAGE == 1024);
};
