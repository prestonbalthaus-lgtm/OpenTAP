//! The incremental tables on the engine's side (format 2.4, F19 to F21):
//! the chain each table keeps on disk, and how a boot reads it back.
//!
//! The snapshot table and the catalogue each have a head page naming a
//! base region and the delta regions written since the table's last fold
//! (`otap_format::tables`). A consistency point that changes no record of
//! a table carries its head forward; one that does writes a delta region
//! and a new head, or folds the table (a fresh base, an empty chain) when
//! the fold policy, the chain's hard bound or a repair asks for it. The
//! engine decides which in its metadata phase; this module holds the
//! chain's state and the boot's replay.
//!
//! # The boot's replay
//!
//! [`load_table`] reads the head behind the slot's root, then the base and
//! the deltas in order. A region the booted commit wrote itself (the head
//! when the slot's head CP is its own, a base or a delta whose entry names
//! its CP) that does not verify tears the commit: it did not land whole,
//! and the previous commit of the ring still has its own chain. Anything
//! older that does not verify is damage that every candidate shares: the
//! region's readable records are applied, the damage is reported, and
//! every key whose last readable record comes before the damaged region is
//! uncertain, because its last change may be what was lost. The engine
//! drops an uncertain name and quarantines an uncertain snapshot, and the
//! first commit after the boot folds the table, which repairs the chain.

use std::collections::{BTreeMap, BTreeSet, HashMap};

use otap_format::{
    parse_table_delta, scan_table_base, scan_table_delta, CatalogKind, CatalogRec, RootRef,
    SnapRecord, TableChainEntry, TableHead, TableKind, TableRecord, Uuid, BLOCK_SIZE,
    SNAP_UNCERTAIN,
};

use crate::boot::TornReason;
use crate::layout::DiskLayout;
use crate::traits::{Backstore, IoError};

/// When the snapshot table and the catalogue are folded (format 2.4, F20).
/// A consistency point that changes a table writes a delta region and a
/// new head, unless the table's chain would exceed `max_deltas` (never
/// more than [`TABLE_HEAD_DELTAS`](otap_format::TABLE_HEAD_DELTAS)) or its
/// deltas would hold more records than its base and `min_fold_records`;
/// then it writes a fresh base and an empty chain. A fold the policy asks
/// for that does not fit the free segments is put off (the table appends
/// instead) until more are free, the rule the map's folds follow (E31); a
/// full chain is folded whatever the space, and so is a table that has no
/// head yet or that the boot found damaged (the repair).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TableFoldPolicy {
    /// Delta regions a table's chain may hold before the next change folds
    /// it; clamped to `1..=TABLE_HEAD_DELTAS`.
    pub max_deltas: usize,
    /// The deltas may hold this many records whatever the base's size, so
    /// a small table is not refolded at every change.
    pub min_fold_records: u64,
}

impl Default for TableFoldPolicy {
    /// 64 deltas, and 256 records before the deltas outweigh a small base:
    /// a boot replays at most 64 regions per table, and the records it
    /// replays never exceed twice the table, plus 256.
    fn default() -> Self {
        Self {
            max_deltas: 64,
            min_fold_records: 256,
        }
    }
}

/// What one consistency point does with one table (F20).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TablePlan {
    /// The table did not change: its head is carried forward.
    Carry,
    /// The table holds nothing any more: its chain goes, and no head names
    /// it.
    Drop,
    /// The changes become a delta region and a new head. `put_off` when the
    /// policy wanted a fold that does not fit: the policy then waits until
    /// more segments are free.
    Delta {
        /// A fold was due and did not fit.
        put_off: bool,
    },
    /// A fresh base and an empty chain. `policy` when only the policy asked
    /// for it (a failure for space then puts it off); otherwise it is
    /// forced: the table's first head, a repair, or a full chain.
    Fold {
        /// The policy alone asked for it.
        policy: bool,
    },
}

/// What [`plan_table`] weighs besides the chain and the policy.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TablePlanInput {
    /// The table must have a head: it holds a record (and for the snapshot
    /// table, the catalogue does: see F20).
    pub wanted: bool,
    /// Records (and tombstones) this CP changes.
    pub changes: u64,
    /// Records the table holds after the CP (what a fold writes).
    pub records: u64,
    /// A record's size in bytes.
    pub record_size: usize,
    /// Free segments in the committed table.
    pub free: u32,
}

/// Decides what a consistency point does with a table (F20): the rules of
/// [`TableFoldPolicy`], with `fits(pages)` telling whether a region of that
/// many pages (the base and the head) would be placed now under the CP's
/// reserve.
#[must_use]
pub fn plan_table(
    chain: &TableChain,
    input: &TablePlanInput,
    policy: &TableFoldPolicy,
    fits: impl Fn(u64) -> bool,
) -> TablePlan {
    if !input.wanted {
        return if chain.head.is_none() {
            TablePlan::Carry
        } else {
            TablePlan::Drop
        };
    }
    let repair = chain.repair || chain.head.is_none();
    if input.changes == 0 && !repair {
        return TablePlan::Carry;
    }
    if repair || chain.deltas.len() >= otap_format::TABLE_HEAD_DELTAS {
        return TablePlan::Fold { policy: false };
    }
    let max = policy.max_deltas.clamp(1, otap_format::TABLE_HEAD_DELTAS);
    let due = chain.deltas.len() + 1 > max
        || chain.delta_records() + input.changes
            > chain.base_records().max(policy.min_fold_records);
    if !due || chain.fold_blocked_at.is_some_and(|at| input.free <= at) {
        return TablePlan::Delta { put_off: false };
    }
    let pages = input
        .records
        .saturating_mul(input.record_size as u64)
        .div_ceil(BLOCK_SIZE as u64);
    if fits(pages.saturating_add(1)) {
        TablePlan::Fold { policy: true }
    } else {
        TablePlan::Delta { put_off: true }
    }
}

/// A table's chain as a committed CP left it on disk, with what the engine
/// keeps about it between consistency points.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct TableChain {
    /// The head page; [`RootRef::NONE`] before the first commit that wrote
    /// one (a freshly formatted pool).
    pub head: RootRef,
    /// The CP that wrote the head.
    pub head_cp: u64,
    /// The base region, as the head lists it.
    pub base: TableChainEntry,
    /// The delta regions since the fold, oldest first.
    pub deltas: Vec<TableChainEntry>,
    /// The table must be folded by the next consistency point: the boot
    /// found it damaged or had to adjust it, or a verification found a
    /// region of the chain damaged since.
    pub repair: bool,
    /// A fold the policy asked for did not fit with this many free
    /// segments: the policy stays quiet until more are free (the map's
    /// rule for its own folds, E31).
    pub fold_blocked_at: Option<u32>,
}

impl TableChain {
    /// Records in the base.
    #[must_use]
    pub fn base_records(&self) -> u64 {
        u64::from(self.base.records)
    }

    /// Records in the deltas.
    #[must_use]
    pub fn delta_records(&self) -> u64 {
        self.deltas.iter().map(|d| u64::from(d.records)).sum()
    }

    /// Every region the chain references: the head, the base and the
    /// deltas (those that exist).
    #[must_use]
    pub fn regions(&self) -> Vec<RootRef> {
        let mut out = Vec::with_capacity(self.deltas.len() + 2);
        out.push(self.head);
        out.push(self.base.region);
        out.extend(self.deltas.iter().map(|d| d.region));
        out.retain(|r| !r.is_none());
        out
    }

    /// The head as a value, for `table`.
    #[must_use]
    pub fn head_value(&self, table: TableKind) -> TableHead {
        TableHead {
            table,
            cp_seq: self.head_cp,
            base: self.base,
            deltas: self.deltas.clone(),
        }
    }
}

/// What a boot read of one table.
#[derive(Debug, Clone)]
pub struct TableLoad<R: TableRecord> {
    /// The records after the replay, in key order.
    pub records: Vec<R>,
    /// The chain as the head describes it (empty when the head is absent
    /// or did not verify).
    pub chain: TableChain,
    /// The regions read: the head, the base and the deltas the head names
    /// (what the boot counts as live metadata).
    pub regions: Vec<RootRef>,
    /// Why the table is not whole, when it is not.
    pub damage: Option<String>,
    /// Keys whose last readable record comes before a damaged region.
    pub uncertain: BTreeSet<R::Key>,
}

impl<R: TableRecord> TableLoad<R> {
    fn empty() -> Self {
        Self {
            records: Vec::new(),
            chain: TableChain::default(),
            regions: Vec::new(),
            damage: None,
            uncertain: BTreeSet::new(),
        }
    }

    fn note(&mut self, why: String) {
        match &mut self.damage {
            Some(d) => {
                d.push_str("; ");
                d.push_str(&why);
            }
            None => self.damage = Some(why),
        }
    }
}

/// A region as read: its bytes when it verifies against its reference,
/// else what was read (empty when the reference names no readable place)
/// and why it does not verify.
enum RegionRead {
    Whole(Vec<u8>),
    Bad(Vec<u8>, String),
}

/// Reads the region `root` names and checks it against the reference. A
/// read error is the device's and is returned as such.
fn read_region<B: Backstore>(
    b: &mut B,
    layout: &DiskLayout,
    root: &RootRef,
) -> Result<RegionRead, IoError> {
    let bytes = root.bytes();
    let end = root
        .pba
        .checked_add(u64::from(root.pages))
        .map(|blocks| blocks.saturating_mul(BLOCK_SIZE as u64));
    if root.pba == 0 || end.is_none_or(|e| e > layout.capacity_bytes) || bytes > (1u64 << 32) {
        return Ok(RegionRead::Bad(
            Vec::new(),
            format!(
                "{} pages at block {} lie off the device",
                root.pages, root.pba
            ),
        ));
    }
    let mut buf = vec![0u8; bytes as usize];
    b.read(root.pba, &mut buf)?;
    match root.verify(&buf) {
        Ok(()) => Ok(RegionRead::Whole(buf)),
        Err(e) => Ok(RegionRead::Bad(buf, e.to_string())),
    }
}

fn table_name(table: TableKind) -> &'static str {
    match table {
        TableKind::Catalog => "catalogue",
        TableKind::Snapshots => "snapshot table",
    }
}

fn torn(table: TableKind, why: String) -> TornReason {
    TornReason::Table(format!("{}: {why}", table_name(table)))
}

/// Why a snapshot table region of an older CP that lost records tears the
/// commit (the Step 6 review's D9): a snapshot whose record is lost would
/// be dropped at the boot, and the blocks only it pins counted dead and
/// freed by the cleaner. The catalogue loses only names, so it boots on.
fn lost_snapshots(what: &str) -> String {
    format!(
        "{what}; records of the snapshot table are lost, and booting without them would free \
         the blocks their snapshots pin"
    )
}

/// Applies `records` of region `idx` to the replay state.
fn apply<R: TableRecord>(state: &mut BTreeMap<R::Key, (R, usize)>, records: &[R], idx: usize) {
    for rec in records {
        if rec.is_tombstone() {
            state.remove(&rec.key());
        } else {
            state.insert(rec.key(), (*rec, idx));
        }
    }
}

/// Reads table `R` of the commit `cp_seq` from its head `head`, written by
/// `head_cp` (the slot's root and head CP), and replays it (F21).
///
/// # Errors
/// [`TornReason::Io`] for a read error; [`TornReason::Table`] when a
/// region the commit wrote itself does not verify (the commit is torn).
pub fn load_table<R: TableRecord, B: Backstore>(
    b: &mut B,
    layout: &DiskLayout,
    agg_id: &Uuid,
    cp_seq: u64,
    head: RootRef,
    head_cp: u64,
) -> Result<TableLoad<R>, TornReason> {
    let mut load = TableLoad::<R>::empty();
    if head.is_none() {
        // A freshly formatted pool: no table was ever written.
        return Ok(load);
    }
    let table = R::TABLE;
    let value = match read_region(b, layout, &head).map_err(TornReason::Io)? {
        RegionRead::Whole(bytes) => match TableHead::decode(&bytes, agg_id, table) {
            Ok(value) if value.cp_seq == head_cp => Ok(value),
            Ok(value) => Err(format!(
                "the head was written by CP {}, the slot names CP {head_cp}",
                value.cp_seq
            )),
            Err(e) => Err(e.to_string()),
        },
        RegionRead::Bad(_, why) => Err(why),
    };
    let value = match value {
        Ok(value) => value,
        Err(why) => {
            if head_cp == cp_seq {
                return Err(torn(table, format!("head page: {why}")));
            }
            if table == TableKind::Snapshots {
                return Err(torn(table, lost_snapshots(&format!("head page: {why}"))));
            }
            load.note(format!(
                "the {} head page written by CP {head_cp} does not verify ({why}): every \
                 record is unknown",
                table_name(table)
            ));
            return Ok(load);
        }
    };
    load.regions.push(head);
    load.chain = TableChain {
        head,
        head_cp,
        base: value.base,
        deltas: value.deltas.clone(),
        repair: false,
        fold_blocked_at: None,
    };
    let mut state: BTreeMap<R::Key, (R, usize)> = BTreeMap::new();
    let mut damaged_last: Option<usize> = None;
    // The base, region 0 of the replay.
    if !value.base.region.is_none() {
        let base = value.base;
        load.regions.push(base.region);
        let read = read_region(b, layout, &base.region).map_err(TornReason::Io)?;
        let failure = match read {
            RegionRead::Whole(bytes) => match R::parse_base(&bytes) {
                Ok(records) if records.len() as u64 == u64::from(base.records) => {
                    apply(&mut state, &records, 0);
                    None
                }
                Ok(records) => Some((
                    bytes,
                    format!(
                        "it holds {} records where its head counts {}",
                        records.len(),
                        base.records
                    ),
                )),
                Err(e) => Some((bytes, e.to_string())),
            },
            RegionRead::Bad(bytes, why) => Some((bytes, why)),
        };
        if let Some((bytes, why)) = failure {
            if base.cp_seq == cp_seq {
                return Err(torn(table, format!("base region: {why}")));
            }
            let scan = scan_table_base::<R>(&bytes);
            if table == TableKind::Snapshots
                && (scan.records.len() as u64) < u64::from(base.records)
            {
                return Err(torn(
                    table,
                    lost_snapshots(&format!(
                        "the base region written by CP {} ({why}): {} of its {} records read",
                        base.cp_seq,
                        scan.records.len(),
                        base.records
                    )),
                ));
            }
            apply(&mut state, &scan.records, 0);
            damaged_last = Some(0);
            load.note(format!(
                "the base region written by CP {} does not verify ({why}); {} of its {} records \
                 were read",
                base.cp_seq,
                scan.records.len(),
                base.records
            ));
        }
    }
    // The deltas, regions 1.. of the replay.
    for (i, delta) in value.deltas.iter().enumerate() {
        let idx = i + 1;
        load.regions.push(delta.region);
        let read = read_region(b, layout, &delta.region).map_err(TornReason::Io)?;
        let failure = match read {
            RegionRead::Whole(bytes) => match parse_table_delta::<R>(&bytes, agg_id) {
                Ok(parsed)
                    if parsed.cp_seq == delta.cp_seq
                        && parsed.records.len() as u64 == u64::from(delta.records) =>
                {
                    apply(&mut state, &parsed.records, idx);
                    None
                }
                Ok(parsed) => Some((
                    bytes,
                    format!(
                        "it holds {} records of CP {} where its head counts {} of CP {}",
                        parsed.records.len(),
                        parsed.cp_seq,
                        delta.records,
                        delta.cp_seq
                    ),
                )),
                Err(e) => Some((bytes, e.to_string())),
            },
            RegionRead::Bad(bytes, why) => Some((bytes, why)),
        };
        if let Some((bytes, why)) = failure {
            if delta.cp_seq == cp_seq {
                return Err(torn(table, format!("delta region: {why}")));
            }
            let scan = scan_table_delta::<R>(&bytes, agg_id, delta.cp_seq);
            if table == TableKind::Snapshots
                && (scan.records.len() as u64) < u64::from(delta.records)
            {
                return Err(torn(
                    table,
                    lost_snapshots(&format!(
                        "the delta region written by CP {} ({why}): {} of its {} records read",
                        delta.cp_seq,
                        scan.records.len(),
                        delta.records
                    )),
                ));
            }
            apply(&mut state, &scan.records, idx);
            damaged_last = Some(idx);
            load.note(format!(
                "the delta region written by CP {} does not verify ({why}); {} of its {} records \
                 were read",
                delta.cp_seq,
                scan.records.len(),
                delta.records
            ));
        }
    }
    if let Some(last) = damaged_last {
        // A key whose last readable record precedes the last damaged
        // region may have lost a later change with it.
        load.uncertain = state
            .iter()
            .filter(|(_, (_, idx))| *idx < last)
            .map(|(key, _)| *key)
            .collect();
    }
    load.records = state.into_values().map(|(rec, _)| rec).collect();
    Ok(load)
}

/// The snapshot table after a boot's replay: records whose key is
/// uncertain, or whose identity (uuid) another record also claims, keep
/// [`SNAP_UNCERTAIN`] (re-sealed); returns the records and why the table
/// is not whole, if it is not.
#[must_use]
pub fn finish_snapshots(load: &TableLoad<SnapRecord>) -> (Vec<SnapRecord>, Option<String>) {
    let mut damage = load.damage.clone();
    let mut records = load.records.clone();
    let mut claims: HashMap<Uuid, usize> = HashMap::new();
    for rec in &records {
        *claims.entry(rec.snap_uuid).or_insert(0) += 1;
    }
    let mut doubled = 0usize;
    for rec in &mut records {
        let uncertain = load.uncertain.contains(&rec.snap_id);
        let shared = claims.get(&rec.snap_uuid).is_some_and(|n| *n > 1);
        if shared {
            doubled += 1;
        }
        if (uncertain || shared) && !rec.is_uncertain() {
            rec.flags |= SNAP_UNCERTAIN;
            rec.seal();
        }
    }
    if doubled > 0 {
        let why = format!("{doubled} snapshot records claim an identity another record holds");
        damage = Some(match damage {
            Some(d) => format!("{d}; {why}"),
            None => why,
        });
    }
    (records, damage)
}

/// The catalogue after a boot's replay: an uncertain name is dropped, and
/// so is every name that clashes with another in its scope (volumes; the
/// snapshots of one volume); returns the names and why the catalogue is
/// not whole, if it is not.
#[must_use]
pub fn finish_catalog(load: &TableLoad<CatalogRec>) -> (Vec<CatalogRec>, Option<String>) {
    let mut damage = load.damage.clone();
    let dropped_uncertain = load
        .records
        .iter()
        .filter(|r| load.uncertain.contains(&r.sort_key()))
        .count();
    let kept: Vec<CatalogRec> = load
        .records
        .iter()
        .filter(|r| !load.uncertain.contains(&r.sort_key()))
        .copied()
        .collect();
    let scope = |r: &CatalogRec| {
        if r.catalog_kind() == Some(CatalogKind::Volume) {
            (r.kind, 0u32, r.name_bytes().to_vec())
        } else {
            (r.kind, r.vol, r.name_bytes().to_vec())
        }
    };
    let mut uses: HashMap<(u8, u32, Vec<u8>), usize> = HashMap::new();
    for rec in &kept {
        *uses.entry(scope(rec)).or_insert(0) += 1;
    }
    let before = kept.len();
    let names: Vec<CatalogRec> = kept
        .into_iter()
        .filter(|r| uses.get(&scope(r)).is_some_and(|n| *n == 1))
        .collect();
    let clashes = before - names.len();
    let mut notes = Vec::new();
    if dropped_uncertain > 0 {
        notes.push(format!(
            "{dropped_uncertain} names whose last change may have been lost were dropped"
        ));
    }
    if clashes > 0 {
        notes.push(format!(
            "{clashes} names that clash in their scope were dropped"
        ));
    }
    if !notes.is_empty() {
        let why = notes.join("; ");
        damage = Some(match damage {
            Some(d) => format!("{d}; {why}"),
            None => why,
        });
    }
    (names, damage)
}
