//! The pool attestation (Step 6B, `otapctl pool verify`): a complete check
//! of a booted pool as one committed consistency point left it, run in
//! budgeted slices between engine ticks so the devices keep being served.
//!
//! # What it attests
//!
//! The pool as CP `N` committed it, `N` being the committed CP when the
//! verification starts (taken when no superblock write is in flight):
//!
//! 1. **The superblock ring.** All four slots verify (both CRCs, the
//!    aggregate, the location their `slot` names); the pair of `N`'s parity
//!    holds exactly the slot the engine wrote for `N`; the other pair holds
//!    `N - 1` in its `A` slot and `N - 1` or an older commit of the same
//!    parity in its `B` slot (a stop between the two writes of `N - 1`),
//!    whose counters do not run ahead of `N`'s.
//! 2. **The tables `N` names.** The map-roots table (and that it is the
//!    engine's), the segment table, the CP log directory (its deltas and
//!    fold are the engine's) with every delta region and every page it
//!    lists, the stripe list with every stripe's summary against `N`'s
//!    identity, and the two table chains (format 2.4), replayed and
//!    compared with the engine's snapshot records and catalogue.
//! 3. **Every tree.** Each volume's index (against its root and the
//!    engine's copy) and every map page it names (CRC, `max_birth`, every
//!    entry valid, births within the tree, nothing mapped past the end),
//!    merged with the volume's overlay; each snapshot's root page (CRC and
//!    identity), index and pages. A snapshot the boot or a walk quarantined
//!    (Step 6A) is reported as such.
//! 4. **Every live block.** For each distinct data block the trees reach:
//!    its address (a data slot of a data segment the table has in use), its
//!    payload against the entry's CRC, and its stripe's summary (CRC,
//!    identity) and descriptor (volume, LBA, birth and CRC equal to the
//!    entry's; a relocation stripe's births may be older than its CP).
//!    Holes and punched ranges have no block and are counted, not read.
//! 5. **The accounting.** Each data segment's `live_bytes` in `N`'s segment
//!    table against the distinct blocks the trees reach in it, exactly; each
//!    metadata segment's against the distinct regions `N` references plus
//!    the regions freed and not yet reusable when `N` was written.
//!
//! # How it runs
//!
//! [`Engine::start_verify`](crate::Engine::start_verify) creates the job;
//! each [`Engine::tick`](crate::Engine::tick) advances it by at most
//! [`VerifyOptions::units_per_tick`] units (a 4 KiB block read is one unit,
//! a map page four, a summary its pages) and
//! [`VerifyOptions::max_ns_per_tick`] of the engine's clock. While it runs,
//! the engine keeps every region and data segment that a CP after `N`
//! stopped referencing (the deferred frees of E24 are held), so the view
//! `N` describes stays on disk however many consistency points, folds,
//! deletions and cleanings happen meanwhile; writes go on. If the free
//! segments fall to the reserve, the pool is full, or a shutdown begins,
//! the job is aborted and the holds are released. The result is a
//! [`VerifyReport`]: counts, a bounded list of [`Finding`]s and a
//! [`Verdict`].

use std::collections::{HashMap, HashSet, VecDeque};

use otap_format::checksum::crc32c;
use otap_format::{
    parse_cp_stripes, parse_map_roots, BlockKind, CatalogRec, CpLogDir, IntoBytes, MapEntry,
    MapPage, MapRoot, PageIndexEntry, RootRef, SbSlot, SegKind, SnapRecord, SnapRootPage,
    StripeDescriptor, StripeId, StripeKind, StripeSummary, SuperblockSlot, Uuid, BLOCK_SIZE,
    MAP_ENTRIES_PER_PAGE, MAP_PAGE_SIZE, SECTORS_PER_BLOCK, SNAP_ROOT_PAGE_SIZE,
};
use otap_map::{index_root_ref, page_root_ref, MapSet, PinnedRoot, MAP_PAGE_BLOCKS};

use crate::boot::{RingImages, TornReason};
use crate::layout::DiskLayout;
use crate::segments::SegmentTable;
use crate::tables::{finish_catalog, finish_snapshots, load_table, TableChain};
use crate::traits::Backstore;

/// The findings a report lists at most; the totals count every one.
pub const MAX_FINDINGS: usize = 64;

/// Summaries the block check keeps parsed at once.
const SUMMARY_CACHE: usize = 64;

/// How fast a verification goes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VerifyOptions {
    /// Units of work one engine tick spends at most: a 4 KiB block read is
    /// one, a map page four, a stripe summary its pages.
    pub units_per_tick: u32,
    /// Engine-clock nanoseconds one tick spends at most (0: no bound but
    /// the units).
    pub max_ns_per_tick: u64,
}

impl Default for VerifyOptions {
    /// 256 units (1 MiB of blocks) and 2 ms per tick: a device request
    /// waits at most that long behind a slice.
    fn default() -> Self {
        Self {
            units_per_tick: 256,
            max_ns_per_tick: 2_000_000,
        }
    }
}

/// Where a verification stands.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum VerifyState {
    /// In progress.
    Running,
    /// Finished: every check ran.
    Done,
    /// Cancelled by the operator.
    Cancelled,
    /// Given up by the engine (space, shutdown), with the reason.
    Aborted,
}

/// What a verification concluded.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Verdict {
    /// No finding.
    Clean,
    /// Findings of warning severity only: leaks, quarantines in force.
    Warnings,
    /// At least one finding of error severity: damage, or data at risk.
    Damaged,
    /// The verification did not finish.
    Incomplete,
}

/// How bad a finding is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Severity {
    /// Damage, or accounting that could free data still referenced.
    Error,
    /// A leak (space counted that nothing references) or a quarantine the
    /// engine already enforces.
    Warning,
}

/// What a finding is about.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum FindingKind {
    /// A superblock slot does not verify, names another aggregate, or sits
    /// at another location.
    SuperblockSlot,
    /// The ring's generations disagree: the older pair is not `N - 1`, its
    /// two copies differ, or its counters run ahead of `N`'s.
    SuperblockChain,
    /// A slot of `N` is not the slot the engine wrote.
    SuperblockState,
    /// A table region `N` names (map roots, segment table, directory, a
    /// delta, a directory page, the stripe list) does not verify, or holds
    /// other than the engine's state.
    Region,
    /// A region of the snapshot table's or the catalogue's chain does not
    /// verify, or the chain replays to other records than the engine's.
    TableChain,
    /// A page index does not verify or differs from the engine's.
    MapIndex,
    /// A map page does not verify.
    MapPage,
    /// A map entry names no valid data slot, a segment not in use as data,
    /// or a birth outside its tree, or is mapped beyond its volume.
    MapEntry,
    /// A snapshot's root page does not verify or names another snapshot.
    SnapshotRoot,
    /// A snapshot is quarantined (its root or a page did not verify, or its
    /// record was recovered from a damaged table): reported, not repaired.
    SnapshotQuarantined,
    /// A stripe summary holding a live block does not verify or names
    /// another stripe.
    StripeSummary,
    /// A stripe `N`'s stripe list names does not verify as `N`'s.
    CpStripe,
    /// A summary's descriptor of a live block disagrees with the map entry.
    Descriptor,
    /// A live block's payload does not match its entry's CRC.
    BlockCrc,
    /// A data segment's recorded live bytes differ from the walk.
    LiveBlocks,
    /// A metadata segment's recorded live bytes differ from the regions.
    MetaBytes,
    /// The device refused a read.
    ReadError,
}

impl FindingKind {
    /// The kind as a word (snake case), as the control protocol carries it.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::SuperblockSlot => "superblock_slot",
            Self::SuperblockChain => "superblock_chain",
            Self::SuperblockState => "superblock_state",
            Self::Region => "region",
            Self::TableChain => "table_chain",
            Self::MapIndex => "map_index",
            Self::MapPage => "map_page",
            Self::MapEntry => "map_entry",
            Self::SnapshotRoot => "snapshot_root",
            Self::SnapshotQuarantined => "snapshot_quarantined",
            Self::StripeSummary => "stripe_summary",
            Self::CpStripe => "cp_stripe",
            Self::Descriptor => "descriptor",
            Self::BlockCrc => "block_crc",
            Self::LiveBlocks => "live_blocks",
            Self::MetaBytes => "meta_bytes",
            Self::ReadError => "read_error",
        }
    }
}

/// One thing a verification found, with where.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Finding {
    /// What.
    pub kind: FindingKind,
    /// How bad.
    pub severity: Severity,
    /// The block number concerned, when one is.
    pub pba: Option<u64>,
    /// The segment concerned, when one is.
    pub pseg: Option<u64>,
    /// The volume concerned, when one is.
    pub vol: Option<u32>,
    /// The LBA concerned, when one is.
    pub lba: Option<u64>,
    /// The snapshot concerned, when one is.
    pub snap_id: Option<u64>,
    /// What exactly, for the operator.
    pub detail: String,
}

/// What a verification counted.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct VerifyCounts {
    /// Superblock slots read.
    pub superblock_slots: u32,
    /// Table, chain, index and root page regions checked.
    pub regions: u64,
    /// Map pages checked.
    pub map_pages: u64,
    /// Distinct live data blocks read and checked.
    pub blocks: u64,
    /// Hole entries met (punched ranges, no block).
    pub holes: u64,
    /// Stripe summaries checked.
    pub stripes: u64,
    /// Segments whose live bytes were reconciled.
    pub segments: u64,
    /// Bytes read from the device.
    pub bytes_read: u64,
}

/// Which part of the work a verification is in.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum VerifyPhase {
    /// Waiting for a moment with no superblock write in flight.
    Waiting,
    /// The tables CP `N` names.
    Tables,
    /// The trees and their blocks.
    Trees,
    /// The accounting.
    Reconcile,
    /// Nothing left.
    Finished,
}

impl VerifyPhase {
    /// The phase as a word.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Waiting => "waiting",
            Self::Tables => "tables",
            Self::Trees => "trees",
            Self::Reconcile => "reconcile",
            Self::Finished => "finished",
        }
    }
}

/// A verification as it stands.
#[derive(Debug, Clone, PartialEq)]
pub struct VerifyReport {
    /// Running, done, cancelled or aborted.
    pub state: VerifyState,
    /// Once finished, the conclusion.
    pub verdict: Option<Verdict>,
    /// The CP attested (0 while waiting to start).
    pub cp_seq: u64,
    /// The part of the work in progress.
    pub phase: VerifyPhase,
    /// How far, `0.0..=1.0`.
    pub progress: f64,
    /// What was checked.
    pub counts: VerifyCounts,
    /// Findings of error severity.
    pub errors: u64,
    /// Findings of warning severity.
    pub warnings: u64,
    /// The first [`MAX_FINDINGS`] findings.
    pub findings: Vec<Finding>,
    /// Why the verification was cancelled or aborted.
    pub reason: Option<String>,
}

/// A verification in one line, for the pool status.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct VerifySummary {
    /// Running, done, cancelled or aborted.
    pub state: VerifyState,
    /// Once finished, the conclusion.
    pub verdict: Option<Verdict>,
    /// The CP attested.
    pub cp_seq: u64,
    /// How far, `0.0..=1.0`.
    pub progress: f64,
    /// Findings of error severity.
    pub errors: u64,
    /// Findings of warning severity.
    pub warnings: u64,
}

/// A live volume as CP `N` left it.
#[derive(Debug, Clone)]
pub(crate) struct LiveVolume {
    pub(crate) vol: u32,
    pub(crate) root: MapRoot,
    pub(crate) index: Vec<PageIndexEntry>,
    /// The overlay (entries since the fold), in LBA order.
    pub(crate) overlay: Vec<(u64, MapEntry)>,
}

/// A snapshot as CP `N` left it.
#[derive(Debug, Clone)]
pub(crate) struct SnapshotView {
    pub(crate) record: SnapRecord,
    pub(crate) pinned: Option<PinnedRoot>,
    pub(crate) damage: Option<String>,
}

/// Everything the engine held about CP `N` when the verification began.
#[derive(Debug, Clone)]
pub(crate) struct View {
    pub(crate) agg_id: Uuid,
    pub(crate) cp_seq: u64,
    pub(crate) cp_nonce: u64,
    /// The slot the engine wrote for `N` (its `A` copy).
    pub(crate) slot: SuperblockSlot,
    pub(crate) vols: Vec<LiveVolume>,
    pub(crate) snaps: Vec<SnapshotView>,
    /// The map's delta regions since its fold, and the fold's CP.
    pub(crate) delta_roots: Vec<RootRef>,
    pub(crate) fold_cp: u64,
    pub(crate) snap_chain: TableChain,
    pub(crate) catalog_chain: TableChain,
    pub(crate) snap_records: Vec<SnapRecord>,
    pub(crate) catalog: Vec<CatalogRec>,
    /// Regions freed and not yet reusable that `N`'s segment table still
    /// counted: the frees pending after `N` and those `N`'s commit released.
    pub(crate) pending: Vec<RootRef>,
}

/// Which tree a work item walks.
#[derive(Debug, Clone, Copy)]
enum TreeRef {
    Live(usize),
    Snap(usize),
}

/// One unit of the work queue.
#[derive(Debug, Clone)]
enum Work {
    MapRoots,
    SegTable,
    Dir,
    Delta(RootRef),
    DirPage(RootRef),
    CpStripes,
    CpStripe(u64, u32),
    SnapChain,
    CatalogChain,
    Tree(TreeRef),
    Pages {
        tree: TreeRef,
        pages: Vec<u64>,
        next: usize,
    },
    Blocks {
        vol: u32,
        snap_id: Option<u64>,
        entries: Vec<(u64, MapEntry)>,
        next: usize,
    },
    Reconcile,
}

/// A parsed stripe summary, or the failure already reported.
#[derive(Debug, Clone)]
struct CachedSummary {
    cp_seq: u64,
    kind: Option<StripeKind>,
    descriptors: Vec<StripeDescriptor>,
}

/// A verification job: the captured view, the work left and what was
/// found. Lives in the engine; advanced by its ticks.
#[derive(Debug)]
pub(crate) struct VerifyJob {
    opts: VerifyOptions,
    state: VerifyState,
    phase: VerifyPhase,
    reason: Option<String>,
    view: Option<View>,
    work: VecDeque<Work>,
    trees_total: usize,
    trees_done: usize,
    counts: VerifyCounts,
    findings: Vec<Finding>,
    errors: u64,
    warnings: u64,
    /// Per data segment, the blocks the trees reach (one bit per block
    /// offset).
    blocks: HashMap<u64, Vec<u64>>,
    /// The metadata regions `N` references, by first block.
    regions: HashMap<u64, u32>,
    /// Map pages already walked, and whether every entry of the page was
    /// taken (a live page with overlay entries is taken in part).
    pages_seen: HashMap<u64, bool>,
    summaries: HashMap<(u64, u32), Option<CachedSummary>>,
    /// `N`'s segment table as read from the device.
    disk_table: Option<SegmentTable>,
    /// A tree could not be walked whole: a drift where the table counts
    /// more than the walk found may be the unwalked part.
    walk_incomplete: bool,
    /// The chains a finding asks the engine to fold (repair).
    repair_snapshots: bool,
    repair_catalog: bool,
}

impl VerifyJob {
    /// A job waiting for its view.
    pub(crate) fn new(opts: VerifyOptions) -> Self {
        Self {
            opts,
            state: VerifyState::Running,
            phase: VerifyPhase::Waiting,
            reason: None,
            view: None,
            work: VecDeque::new(),
            trees_total: 0,
            trees_done: 0,
            counts: VerifyCounts::default(),
            findings: Vec::new(),
            errors: 0,
            warnings: 0,
            blocks: HashMap::new(),
            regions: HashMap::new(),
            pages_seen: HashMap::new(),
            summaries: HashMap::new(),
            disk_table: None,
            walk_incomplete: false,
            repair_snapshots: false,
            repair_catalog: false,
        }
    }

    /// `true` while the job has work to do.
    pub(crate) fn is_running(&self) -> bool {
        self.state == VerifyState::Running
    }

    /// `true` while the job waits for its view.
    pub(crate) fn needs_view(&self) -> bool {
        self.is_running() && self.view.is_none()
    }

    /// While the job runs over a captured view of CP `N`: `Some(N)`, and
    /// what a CP after `N` stops referencing must stay where it is.
    pub(crate) fn holds_frees_after(&self) -> Option<u64> {
        match (&self.view, self.state) {
            (Some(view), VerifyState::Running) => Some(view.cp_seq),
            _ => None,
        }
    }

    /// The chains a finding asked to be repaired, once (snapshot table,
    /// catalogue).
    pub(crate) fn take_repairs(&mut self) -> (bool, bool) {
        (
            std::mem::take(&mut self.repair_snapshots),
            std::mem::take(&mut self.repair_catalog),
        )
    }

    /// Ends the job without finishing it.
    pub(crate) fn stop(&mut self, state: VerifyState, reason: String) {
        if self.is_running() {
            self.state = state;
            self.reason = Some(reason);
            self.phase = VerifyPhase::Finished;
            self.work.clear();
        }
    }

    fn verdict(&self) -> Option<Verdict> {
        match self.state {
            VerifyState::Running => None,
            VerifyState::Cancelled | VerifyState::Aborted => Some(Verdict::Incomplete),
            VerifyState::Done if self.errors > 0 => Some(Verdict::Damaged),
            VerifyState::Done if self.warnings > 0 => Some(Verdict::Warnings),
            VerifyState::Done => Some(Verdict::Clean),
        }
    }

    fn progress(&self) -> f64 {
        match self.phase {
            VerifyPhase::Waiting => 0.0,
            VerifyPhase::Tables => 0.02,
            VerifyPhase::Trees => {
                if self.trees_total == 0 {
                    0.95
                } else {
                    0.02 + 0.93 * (self.trees_done as f64 / self.trees_total as f64)
                }
            }
            VerifyPhase::Reconcile => 0.97,
            VerifyPhase::Finished => 1.0,
        }
    }

    /// The report as it stands.
    pub(crate) fn report(&self) -> VerifyReport {
        VerifyReport {
            state: self.state,
            verdict: self.verdict(),
            cp_seq: self.view.as_ref().map_or(0, |v| v.cp_seq),
            phase: self.phase,
            progress: self.progress(),
            counts: self.counts,
            errors: self.errors,
            warnings: self.warnings,
            findings: self.findings.clone(),
            reason: self.reason.clone(),
        }
    }

    /// The report in one line.
    pub(crate) fn summary(&self) -> VerifySummary {
        VerifySummary {
            state: self.state,
            verdict: self.verdict(),
            cp_seq: self.view.as_ref().map_or(0, |v| v.cp_seq),
            progress: self.progress(),
            errors: self.errors,
            warnings: self.warnings,
        }
    }

    fn find(&mut self, kind: FindingKind, severity: Severity, detail: String, at: At) {
        match severity {
            Severity::Error => self.errors += 1,
            Severity::Warning => self.warnings += 1,
        }
        if self.findings.len() < MAX_FINDINGS {
            self.findings.push(Finding {
                kind,
                severity,
                pba: at.pba,
                pseg: at.pseg,
                vol: at.vol,
                lba: at.lba,
                snap_id: at.snap_id,
                detail,
            });
        }
    }

    fn error(&mut self, kind: FindingKind, detail: String, at: At) {
        self.find(kind, Severity::Error, detail, at);
    }

    fn note_region(&mut self, root: RootRef) {
        if root.is_none() {
            return;
        }
        let pages = self.regions.entry(root.pba).or_insert(0);
        *pages = (*pages).max(root.pages);
    }

    /// Reads `len` bytes at `pba`; a refused read is a finding and `None`.
    fn read<B: Backstore>(
        &mut self,
        b: &mut B,
        pba: u64,
        len: usize,
        what: &str,
    ) -> Option<Vec<u8>> {
        let mut buf = vec![0u8; len];
        match b.read(pba, &mut buf) {
            Ok(()) => {
                self.counts.bytes_read += len as u64;
                Some(buf)
            }
            Err(e) => {
                self.error(
                    FindingKind::ReadError,
                    format!("reading {what}: {e}"),
                    At::pba(pba),
                );
                None
            }
        }
    }

    /// Reads the region `root` names and checks it against the reference;
    /// a region that does not verify is a [`FindingKind::Region`] finding.
    fn read_region<B: Backstore>(
        &mut self,
        b: &mut B,
        layout: &DiskLayout,
        root: &RootRef,
        what: &str,
    ) -> Option<Vec<u8>> {
        self.counts.regions += 1;
        let end = root
            .pba
            .checked_add(u64::from(root.pages))
            .map(|blocks| blocks.saturating_mul(BLOCK_SIZE as u64));
        if root.pba == 0 || end.is_none_or(|e| e > layout.capacity_bytes) {
            self.error(
                FindingKind::Region,
                format!("the {what} names {} pages off the device", root.pages),
                At::pba(root.pba),
            );
            return None;
        }
        let bytes = self.read(b, root.pba, root.bytes() as usize, what)?;
        if let Err(e) = root.verify(&bytes) {
            self.error(
                FindingKind::Region,
                format!("the {what} does not verify: {e}"),
                At::pba(root.pba),
            );
            return None;
        }
        Some(bytes)
    }

    /// Takes the view the engine captured and checks the superblock ring
    /// read with it; queues the rest of the work.
    pub(crate) fn begin(&mut self, view: View, ring: &RingImages, layout: &DiskLayout) {
        self.check_ring(&view, ring, layout);
        for snap in &view.snaps {
            if let Some(reason) = &snap.damage {
                self.find(
                    FindingKind::SnapshotQuarantined,
                    Severity::Warning,
                    format!(
                        "snapshot {} of volume {} is quarantined: {reason}",
                        snap.record.snap_id, snap.record.vol
                    ),
                    At::snap(snap.record.snap_id, snap.record.vol),
                );
            }
            if snap.pinned.is_none() {
                self.walk_incomplete = true;
            }
        }
        self.work.extend([
            Work::MapRoots,
            Work::SegTable,
            Work::Dir,
            Work::CpStripes,
            Work::SnapChain,
            Work::CatalogChain,
        ]);
        for i in 0..view.vols.len() {
            self.work.push_back(Work::Tree(TreeRef::Live(i)));
        }
        for i in 0..view.snaps.len() {
            self.work.push_back(Work::Tree(TreeRef::Snap(i)));
        }
        self.work.push_back(Work::Reconcile);
        self.trees_total = view.vols.len() + view.snaps.len();
        self.phase = VerifyPhase::Tables;
        self.view = Some(view);
    }

    fn check_ring(&mut self, view: &View, ring: &RingImages, layout: &DiskLayout) {
        let n = view.cp_seq;
        let position = |slot: SbSlot| {
            SbSlot::ALL
                .iter()
                .position(|s| *s == slot)
                .expect("every slot is in ALL")
        };
        let mut good = [false; 4];
        for (i, slot) in SbSlot::ALL.iter().enumerate() {
            self.counts.superblock_slots += 1;
            self.counts.bytes_read += otap_format::SUPERBLOCK_SLOT_SIZE as u64;
            let image = &ring.slots[i];
            let pba = layout.sb_pba(*slot);
            if let Err(e) = image.verify() {
                self.error(
                    FindingKind::SuperblockSlot,
                    format!("slot {slot:?} does not verify: {e}"),
                    At::pba(pba),
                );
                continue;
            }
            if image.sb.agg_id != view.agg_id {
                self.error(
                    FindingKind::SuperblockSlot,
                    format!("slot {slot:?} belongs to another aggregate"),
                    At::pba(pba),
                );
                continue;
            }
            if image.sb.sb_slot() != Some(*slot) {
                self.error(
                    FindingKind::SuperblockSlot,
                    format!(
                        "slot {slot:?} holds a page written for slot {:?}",
                        image.sb.sb_slot()
                    ),
                    At::pba(pba),
                );
                continue;
            }
            good[i] = true;
        }
        // The pair of N holds exactly the commit the engine wrote.
        let [a, b] = SbSlot::for_commit(n);
        let mut expected_b = view.slot;
        expected_b.sb.slot = b.index();
        let b_ok = expected_b.seal().is_ok();
        for (slot, expected) in [(a, Some(view.slot)), (b, b_ok.then_some(expected_b))] {
            let i = position(slot);
            if !good[i] {
                continue;
            }
            let same = expected.is_some_and(|e| e.as_bytes() == ring.slots[i].as_bytes());
            if !same {
                self.error(
                    FindingKind::SuperblockState,
                    format!(
                        "slot {slot:?} holds CP {} where the engine committed CP {n} there",
                        ring.slots[i].sb.cp_seq
                    ),
                    At::pba(layout.sb_pba(slot)),
                );
            }
        }
        // The other pair holds N - 1, not ahead of N. Its `A` copy always
        // does (it is written first); its `B` copy may still hold an older
        // commit of that parity when the engine stopped between the two
        // writes of N - 1 (the boot then selected N - 1 from its `A`): the
        // fallback commit has one copy until the next commit of that parity
        // rewrites the pair, a state every such crash leaves, not damage.
        let prev = n.wrapping_sub(1);
        let [oa, ob] = SbSlot::for_commit(prev);
        let (ia, ib) = (position(oa), position(ob));
        if good[ia] && ring.slots[ia].sb.cp_seq != prev {
            self.error(
                FindingKind::SuperblockChain,
                format!(
                    "slot {oa:?} holds CP {} where CP {prev} belongs",
                    ring.slots[ia].sb.cp_seq
                ),
                At::pba(layout.sb_pba(oa)),
            );
            good[ia] = false;
        }
        if good[ib] && ring.slots[ib].sb.cp_seq != prev {
            let older = ring.slots[ib].sb.cp_seq;
            if older > prev || older & 1 != prev & 1 {
                self.error(
                    FindingKind::SuperblockChain,
                    format!("slot {ob:?} holds CP {older} where CP {prev} belongs"),
                    At::pba(layout.sb_pba(ob)),
                );
            }
            good[ib] = false;
        }
        if good[ia] && good[ib] {
            let mut normalized = ring.slots[ia];
            normalized.sb.slot = ring.slots[ib].sb.slot;
            if normalized.seal().is_err() || normalized.as_bytes() != ring.slots[ib].as_bytes() {
                self.error(
                    FindingKind::SuperblockChain,
                    format!("the two copies of CP {} differ", n.wrapping_sub(1)),
                    At::pba(layout.sb_pba(ob)),
                );
            }
        }
        // The counters that never run backwards across commits (the cut
        // counter restarts with a boot's index, so it is not one of them).
        let current = &view.slot.sb;
        for (slot, i) in [(oa, ia), (ob, ib)] {
            if !good[i] {
                continue;
            }
            let older = &ring.slots[i].sb;
            if older.owner_epoch > current.owner_epoch
                || older.intent_seq_seen > current.intent_seq_seen
                || older.opened_stamp > current.opened_stamp
            {
                self.error(
                    FindingKind::SuperblockChain,
                    format!(
                        "slot {slot:?} (CP {}) has counters ahead of CP {n}",
                        older.cp_seq
                    ),
                    At::pba(layout.sb_pba(slot)),
                );
            }
        }
    }

    /// One slice of work: at most the options' units and time. Returns
    /// `true` when the job just finished.
    pub(crate) fn step<B: Backstore>(
        &mut self,
        b: &mut B,
        layout: &DiskLayout,
        now_ns: &dyn Fn() -> u64,
    ) -> bool {
        if !self.is_running() {
            return false;
        }
        let Some(view) = self.view.take() else {
            return false;
        };
        let start = now_ns();
        let budget = self.opts.units_per_tick.max(1);
        let mut used = 0u32;
        let mut finished = false;
        while used < budget {
            if self.opts.max_ns_per_tick > 0
                && now_ns().saturating_sub(start) >= self.opts.max_ns_per_tick
            {
                break;
            }
            let Some(work) = self.work.pop_front() else {
                finished = true;
                break;
            };
            used = used.saturating_add(self.run(&view, work, b, layout, budget - used));
        }
        if !finished && self.work.is_empty() {
            finished = true;
        }
        self.view = Some(view);
        if finished {
            self.state = VerifyState::Done;
            self.phase = VerifyPhase::Finished;
        }
        finished
    }

    /// Runs one work item within `allowance` units; returns the units
    /// spent.
    fn run<B: Backstore>(
        &mut self,
        view: &View,
        work: Work,
        b: &mut B,
        layout: &DiskLayout,
        allowance: u32,
    ) -> u32 {
        match work {
            Work::MapRoots => self.check_map_roots(view, b, layout),
            Work::SegTable => self.check_seg_table(view, b, layout),
            Work::Dir => self.check_dir(view, b, layout),
            Work::Delta(root) => self.check_delta(view, b, layout, root),
            Work::DirPage(root) => {
                self.note_region(root);
                self.read_region(b, layout, &root, "page the directory lists");
                root.pages
            }
            Work::CpStripes => self.check_cp_stripes(view, b, layout),
            Work::CpStripe(pseg, idx) => self.check_cp_stripe(view, b, layout, pseg, idx),
            Work::SnapChain => self.check_snap_chain(view, b, layout),
            Work::CatalogChain => self.check_catalog_chain(view, b, layout),
            Work::Tree(tree) => {
                self.phase = VerifyPhase::Trees;
                self.start_tree(view, b, tree)
            }
            Work::Pages { tree, pages, next } => {
                let Some(page_no) = pages.get(next).copied() else {
                    self.trees_done += 1;
                    return 0;
                };
                if next + 1 < pages.len() {
                    self.work.push_front(Work::Pages {
                        tree,
                        pages,
                        next: next + 1,
                    });
                } else {
                    self.trees_done += 1;
                }
                self.walk_page(view, b, tree, page_no)
            }
            Work::Blocks {
                vol,
                snap_id,
                entries,
                next,
            } => self.check_blocks(view, b, layout, vol, snap_id, entries, next, allowance),
            Work::Reconcile => {
                self.phase = VerifyPhase::Reconcile;
                self.reconcile(view, layout);
                1
            }
        }
    }

    fn check_map_roots<B: Backstore>(
        &mut self,
        view: &View,
        b: &mut B,
        layout: &DiskLayout,
    ) -> u32 {
        let root = view.slot.sb.map_roots;
        if root.is_none() {
            if !view.vols.is_empty() {
                self.error(
                    FindingKind::Region,
                    "CP N names no map-roots table but the engine maps volumes".to_string(),
                    At::default(),
                );
            }
            return 1;
        }
        self.note_region(root);
        let Some(bytes) = self.read_region(b, layout, &root, "map-roots table") else {
            return root.pages;
        };
        match parse_map_roots(&bytes, view.slot.sb.vol_count) {
            Ok(roots) => {
                let mut expected = vec![MapRoot::ABSENT; roots.len()];
                for v in &view.vols {
                    if let Some(slot) = expected.get_mut(v.vol as usize) {
                        *slot = v.root;
                    }
                }
                let beyond = view.vols.iter().any(|v| v.vol as usize >= roots.len());
                if roots != expected || beyond {
                    self.error(
                        FindingKind::Region,
                        "the map-roots table is not the engine's roots".to_string(),
                        At::pba(root.pba),
                    );
                }
            }
            Err(e) => self.error(
                FindingKind::Region,
                format!("the map-roots table does not parse: {e}"),
                At::pba(root.pba),
            ),
        }
        root.pages
    }

    fn check_seg_table<B: Backstore>(
        &mut self,
        view: &View,
        b: &mut B,
        layout: &DiskLayout,
    ) -> u32 {
        let root = view.slot.sb.seg_table;
        if root.is_none() {
            self.error(
                FindingKind::Region,
                "CP N names no segment table".to_string(),
                At::default(),
            );
            return 1;
        }
        self.note_region(root);
        if let Some(bytes) = self.read_region(b, layout, &root, "segment table") {
            match SegmentTable::from_image(&bytes, layout.segments) {
                Ok(table) => self.disk_table = Some(table),
                Err(e) => self.error(
                    FindingKind::Region,
                    format!("the segment table does not parse: {e}"),
                    At::pba(root.pba),
                ),
            }
        }
        root.pages
    }

    fn check_dir<B: Backstore>(&mut self, view: &View, b: &mut B, layout: &DiskLayout) -> u32 {
        let root = view.slot.sb.cp_log_head;
        if root.is_none() {
            if !view.delta_roots.is_empty() {
                self.error(
                    FindingKind::Region,
                    "CP N names no directory but the map has deltas".to_string(),
                    At::default(),
                );
            }
            return 1;
        }
        self.note_region(root);
        let Some(bytes) = self.read_region(b, layout, &root, "CP log directory") else {
            return root.pages;
        };
        match CpLogDir::decode(&bytes, &view.agg_id) {
            Ok(dir) => {
                if dir.cp_seq != view.cp_seq
                    || dir.fold_cp_seq != view.fold_cp
                    || dir.deltas != view.delta_roots
                {
                    self.error(
                        FindingKind::Region,
                        format!(
                            "the directory is CP {} folded at {} with {} deltas; the engine has \
                             CP {} folded at {} with {}",
                            dir.cp_seq,
                            dir.fold_cp_seq,
                            dir.deltas.len(),
                            view.cp_seq,
                            view.fold_cp,
                            view.delta_roots.len()
                        ),
                        At::pba(root.pba),
                    );
                }
                for page in dir.pages.iter().rev() {
                    self.work.push_front(Work::DirPage(*page));
                }
                for delta in dir.deltas.iter().rev() {
                    self.work.push_front(Work::Delta(*delta));
                }
            }
            Err(e) => self.error(
                FindingKind::Region,
                format!("the directory does not decode: {e}"),
                At::pba(root.pba),
            ),
        }
        root.pages
    }

    fn check_delta<B: Backstore>(
        &mut self,
        view: &View,
        b: &mut B,
        layout: &DiskLayout,
        root: RootRef,
    ) -> u32 {
        self.note_region(root);
        let Some(bytes) = self.read_region(b, layout, &root, "map delta region") else {
            return root.pages;
        };
        match MapSet::parse_delta(&bytes, &view.agg_id) {
            Ok((cp_seq, records)) => {
                if cp_seq < view.fold_cp || cp_seq > view.cp_seq {
                    self.error(
                        FindingKind::Region,
                        format!(
                            "a delta region of CP {cp_seq} lies outside the log {}..={}",
                            view.fold_cp, view.cp_seq
                        ),
                        At::pba(root.pba),
                    );
                }
                for rec in records {
                    let size = view
                        .vols
                        .iter()
                        .find(|v| v.vol == rec.vol)
                        .map(|v| v.root.lba_count);
                    if size.is_none_or(|s| rec.lba >= s) {
                        self.error(
                            FindingKind::Region,
                            format!(
                                "a delta record names volume {} LBA {} beyond the map",
                                rec.vol, rec.lba
                            ),
                            At {
                                pba: Some(root.pba),
                                vol: Some(rec.vol),
                                lba: Some(rec.lba),
                                ..At::default()
                            },
                        );
                        break;
                    }
                }
            }
            Err(e) => self.error(
                FindingKind::Region,
                format!("a delta region does not parse: {e}"),
                At::pba(root.pba),
            ),
        }
        root.pages
    }

    fn check_cp_stripes<B: Backstore>(
        &mut self,
        view: &View,
        b: &mut B,
        layout: &DiskLayout,
    ) -> u32 {
        let root = view.slot.sb.cp_stripes;
        if root.is_none() {
            return 1;
        }
        self.note_region(root);
        let Some(bytes) = self.read_region(b, layout, &root, "stripe list") else {
            return root.pages;
        };
        match parse_cp_stripes(&bytes) {
            Ok(list) => {
                for s in list.iter().rev() {
                    self.work.push_front(Work::CpStripe(s.pseg, s.idx));
                }
            }
            Err(e) => self.error(
                FindingKind::Region,
                format!("the stripe list does not parse: {e}"),
                At::pba(root.pba),
            ),
        }
        root.pages
    }

    fn check_cp_stripe<B: Backstore>(
        &mut self,
        view: &View,
        b: &mut B,
        layout: &DiskLayout,
        pseg: u64,
        idx: u32,
    ) -> u32 {
        let pages = (layout.summary.summary_bytes / BLOCK_SIZE) as u32;
        let Some(pba) = layout.stripe_pba(pseg, idx) else {
            self.error(
                FindingKind::CpStripe,
                format!("the stripe list names stripe {idx} of segment {pseg}, off the device"),
                At::seg(pseg),
            );
            return 1;
        };
        let Some(buf) = self.read(b, pba, layout.summary.summary_bytes, "a listed stripe") else {
            return pages;
        };
        self.counts.stripes += 1;
        let id = StripeId {
            agg_id: view.agg_id,
            lseg: pseg,
            stripe_idx: idx,
            cp_seq: view.cp_seq,
            cp_nonce: view.cp_nonce,
        };
        if let Err(e) = StripeSummary::parse_expected(&buf, &id) {
            self.error(
                FindingKind::CpStripe,
                format!("stripe {idx} of segment {pseg}, listed by CP N, is not CP N's: {e}"),
                At {
                    pba: Some(pba),
                    pseg: Some(pseg),
                    ..At::default()
                },
            );
        }
        pages
    }

    fn check_snap_chain<B: Backstore>(
        &mut self,
        view: &View,
        b: &mut B,
        layout: &DiskLayout,
    ) -> u32 {
        let chain = &view.snap_chain;
        let pages: u32 = chain.regions().iter().map(|r| r.pages).sum();
        if !self.check_chain_head(
            chain,
            view.slot.sb.snap_table,
            view.snap_records.len(),
            "snapshot table",
        ) {
            return pages.max(1);
        }
        match load_table::<SnapRecord, B>(
            b,
            layout,
            &view.agg_id,
            view.cp_seq,
            chain.head,
            chain.head_cp,
        ) {
            Ok(load) => {
                self.count_chain(&load.regions);
                let damaged = load.damage.clone();
                let (records, _) = finish_snapshots(&load);
                if let Some(why) = damaged {
                    self.repair_snapshots = true;
                    self.error(
                        FindingKind::TableChain,
                        format!("the snapshot table's chain is damaged: {why}"),
                        At::pba(chain.head.pba),
                    );
                } else if records != view.snap_records || load.chain.deltas != chain.deltas {
                    self.repair_snapshots = true;
                    self.error(
                        FindingKind::TableChain,
                        format!(
                            "the snapshot table on disk replays to {} records; the engine holds {}",
                            records.len(),
                            view.snap_records.len()
                        ),
                        At::pba(chain.head.pba),
                    );
                }
            }
            Err(e) => self.chain_failed(e, chain, "snapshot table", true),
        }
        pages.max(1)
    }

    fn check_catalog_chain<B: Backstore>(
        &mut self,
        view: &View,
        b: &mut B,
        layout: &DiskLayout,
    ) -> u32 {
        let chain = &view.catalog_chain;
        let pages: u32 = chain.regions().iter().map(|r| r.pages).sum();
        if !self.check_chain_head(chain, view.slot.sb.catalog, view.catalog.len(), "catalogue") {
            return pages.max(1);
        }
        match load_table::<CatalogRec, B>(
            b,
            layout,
            &view.agg_id,
            view.cp_seq,
            chain.head,
            chain.head_cp,
        ) {
            Ok(load) => {
                self.count_chain(&load.regions);
                let damaged = load.damage.clone();
                let (names, _) = finish_catalog(&load);
                if let Some(why) = damaged {
                    self.repair_catalog = true;
                    self.error(
                        FindingKind::TableChain,
                        format!("the catalogue's chain is damaged: {why}"),
                        At::pba(chain.head.pba),
                    );
                } else if names != view.catalog || load.chain.deltas != chain.deltas {
                    self.repair_catalog = true;
                    self.error(
                        FindingKind::TableChain,
                        format!(
                            "the catalogue on disk replays to {} names; the engine holds {}",
                            names.len(),
                            view.catalog.len()
                        ),
                        At::pba(chain.head.pba),
                    );
                }
            }
            Err(e) => self.chain_failed(e, chain, "catalogue", false),
        }
        pages.max(1)
    }

    /// `true` when the slot of `N` names the engine's head for a table and
    /// the head is there to read (a table without records has none).
    fn check_chain_head(
        &mut self,
        chain: &TableChain,
        root: RootRef,
        records: usize,
        what: &str,
    ) -> bool {
        if root != chain.head {
            self.error(
                FindingKind::TableChain,
                format!("CP N's {what} head is not the engine's"),
                At::pba(root.pba),
            );
            return false;
        }
        if chain.head.is_none() {
            if records > 0 {
                self.error(
                    FindingKind::TableChain,
                    format!("CP N names no {what} but the engine holds {records} records"),
                    At::default(),
                );
            }
            return false;
        }
        true
    }

    fn count_chain(&mut self, regions: &[RootRef]) {
        for region in regions {
            self.note_region(*region);
            self.counts.regions += 1;
            self.counts.bytes_read += region.bytes();
        }
    }

    fn chain_failed(&mut self, e: TornReason, chain: &TableChain, what: &str, snapshots: bool) {
        for region in chain.regions() {
            self.note_region(region);
        }
        if snapshots {
            self.repair_snapshots = true;
        } else {
            self.repair_catalog = true;
        }
        let kind = match e {
            TornReason::Io(_) => FindingKind::ReadError,
            _ => FindingKind::TableChain,
        };
        self.error(
            kind,
            format!("the {what}'s chain written by CP N does not verify: {e}"),
            At::pba(chain.head.pba),
        );
    }

    /// Reads and checks a tree's index (and a snapshot's root page), notes
    /// its regions, and queues its pages.
    fn start_tree<B: Backstore>(&mut self, view: &View, b: &mut B, tree: TreeRef) -> u32 {
        let mut units = 0u32;
        let (vol, root, index, overlay_pages, snap_id) = match tree {
            TreeRef::Live(i) => {
                let v = &view.vols[i];
                let mut pages: Vec<u64> = v
                    .overlay
                    .iter()
                    .map(|(lba, _)| MapPage::page_of(*lba))
                    .collect();
                pages.dedup();
                (v.vol, v.root, &v.index, pages, None)
            }
            TreeRef::Snap(i) => {
                let s = &view.snaps[i];
                let Some(pinned) = &s.pinned else {
                    // The boot could not load its root: nothing to walk;
                    // the quarantine was reported with the view.
                    self.trees_done += 1;
                    return 1;
                };
                // The root page: its CRC and the snapshot it names.
                let page_ref = RootRef::new(s.record.map_root_pba, 1, 0);
                self.note_region(page_ref);
                self.counts.regions += 1;
                units += 1;
                if let Some(bytes) = self.read(
                    b,
                    s.record.map_root_pba,
                    SNAP_ROOT_PAGE_SIZE,
                    "a snapshot root page",
                ) {
                    let at =
                        At::snap(s.record.snap_id, s.record.vol).with_pba(s.record.map_root_pba);
                    match SnapRootPage::read(&bytes) {
                        Ok(page) => {
                            if page.agg_id != view.agg_id
                                || page.snap_id != s.record.snap_id
                                || page.birth_cp != u64::from(s.record.birth_cp)
                                || page.root != pinned.root
                            {
                                self.error(
                                    FindingKind::SnapshotRoot,
                                    format!(
                                        "the root page of snapshot {} names another snapshot or \
                                         another root",
                                        s.record.snap_id
                                    ),
                                    at,
                                );
                            }
                        }
                        Err(e) => self.error(
                            FindingKind::SnapshotRoot,
                            format!(
                                "the root page of snapshot {} does not verify: {e}",
                                s.record.snap_id
                            ),
                            at,
                        ),
                    }
                }
                (
                    pinned.vol,
                    pinned.root,
                    &pinned.index,
                    Vec::new(),
                    Some(s.record.snap_id),
                )
            }
        };
        // The index, against its root and the engine's copy.
        let index_ref = index_root_ref(&root);
        self.note_region(index_ref);
        self.counts.regions += 1;
        units += index_ref.pages;
        let at = At {
            pba: Some(index_ref.pba),
            vol: Some(vol),
            snap_id,
            ..At::default()
        };
        if let Some(bytes) = self.read(b, index_ref.pba, index_ref.bytes() as usize, "an index") {
            match PinnedRoot::load(root, &bytes) {
                Ok(loaded) if loaded.index == *index => {}
                Ok(_) => self.error(
                    FindingKind::MapIndex,
                    format!("the index of volume {vol} on disk differs from the engine's"),
                    at.clone(),
                ),
                Err(e) => self.error(
                    FindingKind::MapIndex,
                    format!("the index of volume {vol} does not verify: {e}"),
                    at.clone(),
                ),
            }
        }
        // A page holds what its tree held at a fold: the live map's pages
        // are no newer than the map's fold, a snapshot's than its CP, and
        // none newer than its root's `max_birth`.
        let bound = match tree {
            TreeRef::Snap(i) => u64::from(view.snaps[i].record.birth_cp),
            TreeRef::Live(_) => view.fold_cp,
        };
        let mut pages = Vec::new();
        let mut flagged = false;
        for (page_no, entry) in index.iter().enumerate() {
            if entry.is_none() {
                continue;
            }
            self.note_region(page_root_ref(entry));
            if !flagged && (u64::from(entry.max_birth) > bound || entry.max_birth > root.max_birth)
            {
                flagged = true;
                self.error(
                    FindingKind::MapIndex,
                    format!(
                        "page {page_no} of volume {vol} claims births up to {}, beyond its tree \
                         (CP {bound}, root {})",
                        entry.max_birth, root.max_birth
                    ),
                    at.clone(),
                );
            }
            pages.push(page_no as u64);
        }
        pages.extend(overlay_pages);
        pages.sort_unstable();
        pages.dedup();
        if pages.is_empty() {
            self.trees_done += 1;
        } else {
            self.work.push_front(Work::Pages {
                tree,
                pages,
                next: 0,
            });
        }
        units.max(1)
    }

    /// Reads and checks one map page of a tree, merges a live volume's
    /// overlay, and queues the page's blocks.
    fn walk_page<B: Backstore>(
        &mut self,
        view: &View,
        b: &mut B,
        tree: TreeRef,
        page_no: u64,
    ) -> u32 {
        let first = MapPage::first_lba(page_no);
        let last = first + MAP_ENTRIES_PER_PAGE as u64 - 1;
        let page = match tree {
            TreeRef::Live(i) => {
                let v = &view.vols[i];
                let lo = v.overlay.partition_point(|(l, _)| *l < first);
                let hi = v.overlay.partition_point(|(l, _)| *l <= last);
                PageOfTree {
                    vol: v.vol,
                    lba_count: v.root.lba_count,
                    entry: v
                        .index
                        .get(page_no as usize)
                        .copied()
                        .unwrap_or(PageIndexEntry::NONE),
                    overlay: &v.overlay[lo..hi],
                    snap_id: None,
                    bound: view.fold_cp,
                }
            }
            TreeRef::Snap(i) => {
                let s = &view.snaps[i];
                let Some(pinned) = &s.pinned else {
                    return 0;
                };
                PageOfTree {
                    vol: pinned.vol,
                    lba_count: pinned.root.lba_count,
                    entry: pinned
                        .index
                        .get(page_no as usize)
                        .copied()
                        .unwrap_or(PageIndexEntry::NONE),
                    overlay: &[],
                    snap_id: Some(s.record.snap_id),
                    bound: u64::from(s.record.birth_cp),
                }
            }
        };
        let PageOfTree {
            vol,
            lba_count,
            entry,
            overlay,
            snap_id,
            bound,
        } = page;
        let mut units = 0u32;
        let mut out: Vec<(u64, MapEntry)> = Vec::new();
        if !entry.is_none() {
            let seen = self.pages_seen.get(&entry.page_pba).copied();
            if seen != Some(true) || !overlay.is_empty() {
                units += MAP_PAGE_BLOCKS;
                self.counts.map_pages += 1;
                let at = At {
                    pba: Some(entry.page_pba),
                    vol: Some(vol),
                    lba: Some(first),
                    snap_id,
                    ..At::default()
                };
                if let Some(bytes) = self.read(b, entry.page_pba, MAP_PAGE_SIZE, "a map page") {
                    match MapPage::read(&bytes, &entry) {
                        Ok(page) => {
                            let mut bad_birth = None;
                            let mut beyond = None;
                            for (slot, e) in page.entries.iter().enumerate() {
                                let lba = first + slot as u64;
                                if e.is_unmapped() {
                                    continue;
                                }
                                if lba >= lba_count {
                                    beyond.get_or_insert(lba);
                                    continue;
                                }
                                if u64::from(e.birth_cp) > bound {
                                    bad_birth.get_or_insert((lba, e.birth_cp));
                                }
                                if overlay.binary_search_by_key(&lba, |(l, _)| *l).is_ok() {
                                    continue;
                                }
                                out.push((lba, *e));
                            }
                            if let Some(lba) = beyond {
                                self.error(
                                    FindingKind::MapEntry,
                                    format!(
                                        "map page {page_no} of volume {vol} maps LBA {lba}, past \
                                         the volume's end"
                                    ),
                                    at.clone().with_lba(lba),
                                );
                            }
                            if let Some((lba, birth)) = bad_birth {
                                self.error(
                                    FindingKind::MapEntry,
                                    format!(
                                        "volume {vol} LBA {lba} was born at CP {birth}, after its \
                                         tree's CP {bound}"
                                    ),
                                    at.clone().with_lba(lba),
                                );
                            }
                            self.pages_seen
                                .insert(entry.page_pba, overlay.is_empty() || seen == Some(true));
                        }
                        Err(e) => {
                            self.walk_incomplete = true;
                            self.error(
                                FindingKind::MapPage,
                                format!("map page {page_no} of volume {vol} does not verify: {e}"),
                                at,
                            );
                        }
                    }
                } else {
                    self.walk_incomplete = true;
                }
            }
        }
        for (lba, e) in overlay {
            if *lba >= lba_count || u64::from(e.birth_cp) > view.cp_seq {
                self.error(
                    FindingKind::MapEntry,
                    format!(
                        "volume {vol} LBA {lba}: an overlay entry born at CP {} out of bounds",
                        e.birth_cp
                    ),
                    At {
                        vol: Some(vol),
                        lba: Some(*lba),
                        ..At::default()
                    },
                );
                continue;
            }
            out.push((*lba, *e));
        }
        if !out.is_empty() {
            self.work.push_front(Work::Blocks {
                vol,
                snap_id,
                entries: out,
                next: 0,
            });
        }
        units.max(1)
    }

    /// Checks the entries of one page from `next` on, within `allowance`
    /// units; requeues what is left.
    #[allow(clippy::too_many_arguments)]
    fn check_blocks<B: Backstore>(
        &mut self,
        view: &View,
        b: &mut B,
        layout: &DiskLayout,
        vol: u32,
        snap_id: Option<u64>,
        entries: Vec<(u64, MapEntry)>,
        mut next: usize,
        allowance: u32,
    ) -> u32 {
        let mut used = 0u32;
        while next < entries.len() && used < allowance {
            let (lba, e) = entries[next];
            next += 1;
            if e.is_hole() {
                self.counts.holes += 1;
                continue;
            }
            if !e.is_data() {
                continue;
            }
            used += self.check_block(view, b, layout, vol, lba, e, snap_id);
        }
        if next < entries.len() {
            self.work.push_front(Work::Blocks {
                vol,
                snap_id,
                entries,
                next,
            });
        }
        used.max(1)
    }

    /// One data entry: its address, its segment, then (the first time the
    /// block is met) its payload and its stripe's summary.
    #[allow(clippy::too_many_arguments)]
    fn check_block<B: Backstore>(
        &mut self,
        view: &View,
        b: &mut B,
        layout: &DiskLayout,
        vol: u32,
        lba: u64,
        e: MapEntry,
        snap_id: Option<u64>,
    ) -> u32 {
        let (pseg, off) = (e.lseg(), e.off());
        let at = At {
            pseg: Some(pseg),
            vol: Some(vol),
            lba: Some(lba),
            snap_id,
            ..At::default()
        };
        let bps = layout.blocks_per_stripe();
        let summary_pages = (layout.summary.summary_bytes / BLOCK_SIZE) as u32;
        let pos = off % bps.max(1);
        let idx = off / bps.max(1);
        let slot_ok = pos >= summary_pages
            && ((pos - summary_pages) as usize) < layout.summary.nblocks
            && e.plen() == SECTORS_PER_BLOCK;
        let Some(pba) = layout.block_pba(pseg, off).filter(|_| slot_ok) else {
            self.error(
                FindingKind::MapEntry,
                format!(
                    "volume {vol} LBA {lba} names offset {off} of segment {pseg} (length {}), \
                     not a data slot",
                    e.plen()
                ),
                at,
            );
            return 1;
        };
        let at = at.with_pba(pba);
        if let Some(table) = &self.disk_table {
            let kind = table
                .entry(pseg)
                .ok()
                .filter(|s| !s.is_free())
                .and_then(|s| s.seg_kind());
            if kind != Some(SegKind::Data) {
                self.error(
                    FindingKind::MapEntry,
                    format!(
                        "volume {vol} LBA {lba} names segment {pseg}, which CP N's table has {}",
                        match kind {
                            None => "free",
                            Some(SegKind::Metadata) => "as metadata",
                            Some(_) => "as another kind",
                        }
                    ),
                    at,
                );
                return 1;
            }
        }
        let words = (layout.blocks_per_segment() as usize).div_ceil(64);
        let bits = self.blocks.entry(pseg).or_insert_with(|| vec![0u64; words]);
        let (word, bit) = (off as usize / 64, off as usize % 64);
        if word >= bits.len() || bits[word] & (1 << bit) != 0 {
            return 0;
        }
        bits[word] |= 1 << bit;
        self.counts.blocks += 1;
        let mut units = 1u32;
        if let Some(data) = self.read(b, pba, BLOCK_SIZE, "a data block") {
            let crc = crc32c(&data);
            if crc != e.crc32c {
                self.error(
                    FindingKind::BlockCrc,
                    format!(
                        "volume {vol} LBA {lba}: the block holds CRC {crc:#010x}, its entry \
                         {:#010x}",
                        e.crc32c
                    ),
                    at.clone(),
                );
            }
        }
        // The stripe's summary and the block's descriptor.
        let key = (pseg, idx);
        if !self.summaries.contains_key(&key) {
            units += summary_pages;
            if self.summaries.len() >= SUMMARY_CACHE {
                self.summaries.clear();
            }
            let parsed = self.read_summary(view, b, layout, pseg, idx, &at);
            self.summaries.insert(key, parsed);
        }
        let i = (pos - summary_pages) as usize;
        let Some(Some(summary)) = self.summaries.get(&key) else {
            return units;
        };
        let (cp_seq, kind) = (summary.cp_seq, summary.kind);
        match summary.descriptors.get(i).copied() {
            Some(desc)
                if desc.kind() == Some(BlockKind::Data)
                    && desc.vol() == vol
                    && desc.lba() == lba
                    && desc.birth_cp == e.birth_cp
                    && desc.crc32c == e.crc32c =>
            {
                let births_ok = match kind {
                    Some(StripeKind::Ingest) => u64::from(desc.birth_cp) == cp_seq,
                    _ => u64::from(desc.birth_cp) <= cp_seq,
                };
                if !births_ok {
                    self.error(
                        FindingKind::Descriptor,
                        format!(
                            "volume {vol} LBA {lba}: born at CP {} in a {kind:?} stripe of CP \
                             {cp_seq}",
                            desc.birth_cp
                        ),
                        at,
                    );
                }
            }
            other => self.error(
                FindingKind::Descriptor,
                format!(
                    "volume {vol} LBA {lba}: descriptor {i} of stripe {idx} of segment {pseg} is \
                     {other:?}, the entry names birth {} CRC {:#010x}",
                    e.birth_cp, e.crc32c
                ),
                at,
            ),
        }
        units
    }

    fn read_summary<B: Backstore>(
        &mut self,
        view: &View,
        b: &mut B,
        layout: &DiskLayout,
        pseg: u64,
        idx: u32,
        at: &At,
    ) -> Option<CachedSummary> {
        let pba = layout.stripe_pba(pseg, idx)?;
        let buf = self.read(b, pba, layout.summary.summary_bytes, "a stripe summary")?;
        self.counts.stripes += 1;
        let at = At {
            pba: Some(pba),
            ..at.clone()
        };
        match StripeSummary::parse(&buf) {
            Ok(summary) => {
                let hdr = summary.hdr();
                if hdr.agg_id != view.agg_id || hdr.lseg != pseg || hdr.stripe_idx != idx {
                    self.error(
                        FindingKind::StripeSummary,
                        format!(
                            "the summary of stripe {idx} of segment {pseg} names stripe {} of \
                             segment {}",
                            hdr.stripe_idx, hdr.lseg
                        ),
                        at,
                    );
                    return None;
                }
                Some(CachedSummary {
                    cp_seq: hdr.cp_seq,
                    kind: hdr.stripe_kind(),
                    descriptors: summary.descriptors().to_vec(),
                })
            }
            Err(e) => {
                self.error(
                    FindingKind::StripeSummary,
                    format!("the summary of stripe {idx} of segment {pseg} does not verify: {e}"),
                    at,
                );
                None
            }
        }
    }

    /// The accounting: every segment's recorded live bytes against what the
    /// walk found.
    fn reconcile(&mut self, view: &View, layout: &DiskLayout) {
        let Some(table) = self.disk_table.take() else {
            self.error(
                FindingKind::Region,
                "the accounting was not reconciled: CP N's segment table is unreadable".to_string(),
                At::default(),
            );
            return;
        };
        let mut regions = std::mem::take(&mut self.regions);
        for root in &view.pending {
            if root.is_none() {
                continue;
            }
            let pages = regions.entry(root.pba).or_insert(0);
            *pages = (*pages).max(root.pages);
        }
        let mut meta: HashMap<u64, u64> = HashMap::new();
        for (pba, pages) in &regions {
            for (pseg, bytes) in layout.segments_of_region(*pba, *pages) {
                *meta.entry(pseg).or_insert(0) += bytes;
            }
        }
        let mut seen: HashSet<u64> = HashSet::new();
        for (pseg, entry) in table.iter() {
            if entry.is_free() {
                continue;
            }
            seen.insert(pseg);
            self.counts.segments += 1;
            let recorded = u64::from(entry.live_bytes);
            match entry.seg_kind() {
                Some(SegKind::Data) => {
                    let found = self.blocks.get(&pseg).map_or(0, |bits| {
                        bits.iter().map(|w| u64::from(w.count_ones())).sum()
                    });
                    let counted = recorded / BLOCK_SIZE as u64;
                    if counted != found {
                        let (severity, why) = if found > counted {
                            (
                                Severity::Error,
                                "the pool could free blocks still referenced".to_string(),
                            )
                        } else if self.walk_incomplete {
                            (
                                Severity::Warning,
                                "a leak, or blocks of a tree that could not be walked".to_string(),
                            )
                        } else {
                            (
                                Severity::Warning,
                                "space counted that nothing references".to_string(),
                            )
                        };
                        self.find(
                            FindingKind::LiveBlocks,
                            severity,
                            format!(
                                "segment {pseg} counts {counted} live blocks; the trees reach \
                                 {found}: {why}"
                            ),
                            At::seg(pseg),
                        );
                    }
                }
                Some(SegKind::Metadata) => {
                    let expected = meta.get(&pseg).copied().unwrap_or(0);
                    if recorded != expected {
                        let severity = if recorded < expected {
                            Severity::Error
                        } else {
                            Severity::Warning
                        };
                        self.find(
                            FindingKind::MetaBytes,
                            severity,
                            format!(
                                "metadata segment {pseg} counts {recorded} live bytes; CP N \
                                 references {expected}"
                            ),
                            At::seg(pseg),
                        );
                    }
                }
                _ => {}
            }
        }
        // Regions in segments the table has free.
        for (pseg, bytes) in &meta {
            if !seen.contains(pseg) && *bytes > 0 {
                self.error(
                    FindingKind::MetaBytes,
                    format!(
                        "segment {pseg} holds {bytes} bytes of regions CP N references, and its \
                         table has it free"
                    ),
                    At::seg(*pseg),
                );
            }
        }
        self.regions = regions;
        self.disk_table = Some(table);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn findings_are_bounded_and_counted() {
        let mut job = VerifyJob::new(VerifyOptions::default());
        for i in 0..(MAX_FINDINGS as u64 + 10) {
            job.error(FindingKind::BlockCrc, format!("{i}"), At::pba(i));
        }
        job.find(
            FindingKind::LiveBlocks,
            Severity::Warning,
            "leak".into(),
            At::seg(3),
        );
        let report = job.report();
        assert_eq!(report.findings.len(), MAX_FINDINGS);
        assert_eq!(
            (report.errors, report.warnings),
            (MAX_FINDINGS as u64 + 10, 1)
        );
        assert_eq!(report.state, VerifyState::Running);
        assert_eq!(report.verdict, None);
        job.stop(VerifyState::Cancelled, "asked".into());
        assert_eq!(job.report().verdict, Some(Verdict::Incomplete));
        assert!(!job.is_running());
        assert_eq!(job.holds_frees_after(), None);
    }

    #[test]
    fn kinds_have_words() {
        assert_eq!(FindingKind::BlockCrc.as_str(), "block_crc");
        assert_eq!(
            FindingKind::SnapshotQuarantined.as_str(),
            "snapshot_quarantined"
        );
        assert_eq!(VerifyPhase::Trees.as_str(), "trees");
    }
}

/// One map page of a tree as a walk sees it: the volume, its size, the
/// index entry, the overlay entries that override the page (a live
/// volume's), the snapshot (none for the live map), and the newest birth
/// the page may hold.
struct PageOfTree<'a> {
    vol: u32,
    lba_count: u64,
    entry: PageIndexEntry,
    overlay: &'a [(u64, MapEntry)],
    snap_id: Option<u64>,
    bound: u64,
}

/// Where a finding points.
#[derive(Debug, Clone, Default)]
struct At {
    pba: Option<u64>,
    pseg: Option<u64>,
    vol: Option<u32>,
    lba: Option<u64>,
    snap_id: Option<u64>,
}

impl At {
    fn pba(pba: u64) -> Self {
        Self {
            pba: Some(pba),
            ..Self::default()
        }
    }

    fn seg(pseg: u64) -> Self {
        Self {
            pseg: Some(pseg),
            ..Self::default()
        }
    }

    fn snap(snap_id: u64, vol: u32) -> Self {
        Self {
            snap_id: Some(snap_id),
            vol: Some(vol),
            ..Self::default()
        }
    }

    fn with_pba(self, pba: u64) -> Self {
        Self {
            pba: Some(pba),
            ..self
        }
    }

    fn with_lba(self, lba: u64) -> Self {
        Self {
            lba: Some(lba),
            ..self
        }
    }
}
