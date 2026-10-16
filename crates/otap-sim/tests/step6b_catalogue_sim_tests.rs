//! Step 6B: the incremental snapshot table and catalogue (format 2.4, F19
//! to F21) over the simulator's device. A consistency point writes only
//! the records it changed, as a delta region and a new head; the policy
//! folds the chain within its bounds; a boot replays the chain exactly
//! (the boot's tables equal what the engine held at the CP it boots),
//! through crashes at any point, mid-CP included; damage to each kind of
//! region and record is contained the way F21 says, and the next fold
//! repairs the chain; the table pages obey the reserve of every kind of
//! commit; and a pool written by 2.3 is refused.

use otap_cp::{
    plan_table, DiskLayout, EngineError, Event, FindingKind, Geometry, MetaStream, OperatorWarning,
    SegmentTable, Status, TableChain, TableFoldPolicy, TablePlan, TablePlanInput, Verdict,
};
use otap_format::{
    CatalogRec, RootRef, SbSlot, SegKind, SnapKind, SnapOrigin, SnapRecord, SuperblockSlot,
    TableChainEntry, TableHead, TableKind, BLOCK_SIZE, PIN_USER, SUPERBLOCK_SLOT_SIZE,
    TABLE_HEAD_DELTAS,
};
use otap_sim::{Schedule, Sim, SimConfig, TearPolicy};
use proptest::prelude::*;

fn small(seed: u64) -> SimConfig {
    SimConfig::small(seed)
}

fn sim_with(cfg: SimConfig) -> Sim {
    Sim::new(cfg).expect("the pool formats")
}

fn ok(s: &mut Sim, vol: u32, lba: u64, blocks: u32) {
    let c = s.write(vol, lba, blocks).expect("the write completes");
    assert_eq!(c.status, Status::Ok, "write {vol}/{lba}+{blocks}");
}

/// The newest verified slot of the ring, read from the medium.
fn newest_slot(s: &mut Sim) -> SuperblockSlot {
    let layout = *s.disk().layout();
    let mut best: Option<SuperblockSlot> = None;
    for slot in SbSlot::ALL {
        let at = layout.sb_pba(slot) as usize * BLOCK_SIZE;
        let bytes = s.disk().durable()[at..at + SUPERBLOCK_SLOT_SIZE].to_vec();
        if let Ok(parsed) = SuperblockSlot::read(&bytes) {
            if best.is_none_or(|b| parsed.sb.cp_seq > b.sb.cp_seq) {
                best = Some(parsed);
            }
        }
    }
    best.expect("a verified slot")
}

/// The head of `table` the newest slot names, and its root.
fn head_of(s: &mut Sim, table: TableKind) -> (TableHead, RootRef) {
    let slot = newest_slot(s);
    let root = match table {
        TableKind::Snapshots => slot.sb.snap_table,
        TableKind::Catalog => slot.sb.catalog,
    };
    assert!(!root.is_none(), "the {table:?} head exists");
    let at = root.pba as usize * BLOCK_SIZE;
    let bytes = s.disk().durable()[at..at + root.bytes() as usize].to_vec();
    (
        TableHead::decode(&bytes, &slot.sb.agg_id, table).expect("the head decodes"),
        root,
    )
}

/// Flips one byte on the medium.
fn flip(s: &mut Sim, at: usize) {
    let byte = s.disk().durable()[at] ^ 0x5A;
    s.disk().corrupt_durable(at, &[byte]);
}

/// The committed tables the engine holds.
fn tables(s: &mut Sim) -> (Vec<CatalogRec>, Vec<SnapRecord>) {
    (
        s.engine().catalog_records().to_vec(),
        s.engine().snapshot_records(),
    )
}

fn names(s: &mut Sim, vols: &[u32]) -> Vec<Option<String>> {
    vols.iter()
        .map(|v| s.engine().volume_name(*v).map(str::to_string))
        .collect()
}

fn warned(s: &mut Sim, f: impl Fn(&OperatorWarning) -> bool) -> bool {
    s.engine().pool_status().warnings.iter().any(f)
}

// ---------------------------------------------------------------------------
// The chain
// ---------------------------------------------------------------------------

#[test]
fn a_change_writes_a_delta_and_a_quiet_cp_carries_the_head() {
    let mut cfg = small(6300);
    cfg.volumes = vec![(0, 64), (1, 64)];
    let mut s = sim_with(cfg);
    s.boot().unwrap();
    // A pool with no name and no snapshot writes no table (as 2.3 did).
    let slot = newest_slot(&mut s);
    assert!(slot.sb.catalog.is_none() && slot.sb.snap_table.is_none());
    assert_eq!((slot.sb.catalog_head_cp, slot.sb.snap_head_cp), (0, 0));
    // The first name: the catalogue's first head, a fold of one record,
    // and the snapshot table's head with no record (F20).
    s.engine().set_volume_name(0, "alpha").unwrap();
    let first = s.request_cp().unwrap();
    let (cat, _) = head_of(&mut s, TableKind::Catalog);
    assert_eq!(
        (cat.base.cp_seq, cat.base_records(), cat.deltas.len()),
        (first, 1, 0)
    );
    let (snaps, snap_root) = head_of(&mut s, TableKind::Snapshots);
    assert!(snaps.base.region.is_none() && snaps.deltas.is_empty());
    // A format 2.3 reader takes this head for a snapshot table image and
    // refuses it: byte 56 (a record's `kind`) is zero.
    let at = snap_root.pba as usize * BLOCK_SIZE;
    let bytes = s.disk().durable()[at..at + BLOCK_SIZE].to_vec();
    assert_eq!(bytes[56], 0);
    assert!(otap_format::parse_snap_table(&bytes).is_err());
    // A rename: one delta region of one page, and a new head.
    s.engine().set_volume_name(1, "beta").unwrap();
    let second = s.request_cp().unwrap();
    let (cat, cat_root) = head_of(&mut s, TableKind::Catalog);
    assert_eq!(cat.deltas.len(), 1);
    assert_eq!((cat.deltas[0].cp_seq, cat.deltas[0].records), (second, 1));
    assert_eq!(cat.deltas[0].region.pages, 1);
    assert_eq!(newest_slot(&mut s).sb.catalog_head_cp, second);
    // A CP that changes no name carries the head forward.
    ok(&mut s, 0, 0, 4);
    s.request_cp().unwrap();
    let slot = newest_slot(&mut s);
    assert_eq!(slot.sb.catalog, cat_root);
    assert_eq!(slot.sb.catalog_head_cp, second);
    assert_eq!(s.engine().pool_status().catalog_deltas, 1);
    // A snapshot and a pin: deltas of the snapshot table.
    let snap = s.snapshot(0).unwrap();
    s.engine().set_snapshot_pins(snap, PIN_USER).unwrap();
    s.request_cp().unwrap();
    let (snaps, _) = head_of(&mut s, TableKind::Snapshots);
    assert_eq!(snaps.delta_records(), 2, "{snaps:?}");
    // Every boot reads back exactly the tables the engine held.
    let before = tables(&mut s);
    s.crash_daemon().unwrap();
    s.boot().unwrap();
    s.verify_recovery().unwrap();
    assert_eq!(tables(&mut s), before);
    assert_eq!(
        names(&mut s, &[0, 1]),
        [Some("alpha".into()), Some("beta".into())]
    );
    s.attest_clean().unwrap();
    // The last snapshot and the last named volume gone: no table at all
    // again, and the chains' regions are freed.
    s.engine().set_snapshot_pins(snap, 0).unwrap();
    s.request_cp().unwrap();
    s.delete_snapshot(snap).unwrap();
    assert!(
        !newest_slot(&mut s).sb.snap_table.is_none(),
        "the names keep it"
    );
    s.delete_volume(1).unwrap();
    s.delete_volume(0).unwrap();
    let slot = newest_slot(&mut s);
    assert!(slot.sb.catalog.is_none() && slot.sb.snap_table.is_none());
    s.attest_clean().unwrap();
}

/// Renames in CP after CP: the chain never holds more deltas than the
/// policy allows, folds keep it there, and the hard bound of the head
/// holds when the policy asks for more.
#[test]
fn the_chain_never_outgrows_the_fold_policy() {
    for (max, renames) in [(4usize, 25u32), (10_000, TABLE_HEAD_DELTAS as u32 + 6)] {
        let mut cfg = small(6310 + max as u64);
        cfg.volumes = vec![(0, 64)];
        cfg.table_fold = TableFoldPolicy {
            max_deltas: max,
            min_fold_records: 1 << 20,
        };
        cfg.segments = 96;
        let mut s = sim_with(cfg);
        s.boot().unwrap();
        let bound = max.min(TABLE_HEAD_DELTAS);
        let mut longest = 0;
        let mut folds = 0;
        let mut last_base = 0;
        for i in 0..renames {
            s.engine().set_volume_name(0, &format!("name-{i}")).unwrap();
            s.request_cp().unwrap();
            let deltas = s.engine().pool_status().catalog_deltas;
            assert!(
                deltas <= bound,
                "{deltas} deltas over the bound {bound} after rename {i}"
            );
            longest = longest.max(deltas);
            let base = s.engine().table_chains().1.base.cp_seq;
            if base != last_base {
                folds += 1;
                last_base = base;
            }
        }
        assert_eq!(longest, bound, "the chain reached its bound");
        assert!(folds >= 2, "the chain was folded ({folds} bases)");
        let before = tables(&mut s);
        s.crash_daemon().unwrap();
        s.boot().unwrap();
        assert_eq!(tables(&mut s), before);
        assert_eq!(
            s.engine().volume_name(0),
            Some(format!("name-{}", renames - 1).as_str())
        );
        s.attest_clean().unwrap();
    }
}

#[test]
fn the_fold_policy_decides_as_documented() {
    let entry =
        |cp: u64, records: u32| TableChainEntry::new(RootRef::new(100 + cp, 1, 7), cp, records);
    let chain = |base: u32, deltas: usize| TableChain {
        head: RootRef::new(90, 1, 1),
        head_cp: 50,
        base: entry(10, base),
        deltas: (0..deltas).map(|i| entry(11 + i as u64, 1)).collect(),
        repair: false,
        fold_blocked_at: None,
    };
    let policy = TableFoldPolicy {
        max_deltas: 4,
        min_fold_records: 8,
    };
    let input = |changes: u64| TablePlanInput {
        wanted: true,
        changes,
        records: 20,
        record_size: 64,
        free: 10,
    };
    let fits = |_: u64| true;
    // Nothing changed: the head is carried.
    assert_eq!(
        plan_table(&chain(20, 2), &input(0), &policy, fits),
        TablePlan::Carry
    );
    // Within the bounds: a delta.
    assert_eq!(
        plan_table(&chain(20, 2), &input(1), &policy, fits),
        TablePlan::Delta { put_off: false }
    );
    // The fifth delta would exceed four: a fold by the policy.
    assert_eq!(
        plan_table(&chain(20, 4), &input(1), &policy, fits),
        TablePlan::Fold { policy: true }
    );
    // The deltas would outweigh both the base and the floor: a fold.
    assert_eq!(
        plan_table(&chain(5, 3), &input(6), &policy, fits),
        TablePlan::Fold { policy: true }
    );
    // A small base, deltas under the floor: still a delta.
    assert_eq!(
        plan_table(&chain(2, 1), &input(3), &policy, fits),
        TablePlan::Delta { put_off: false }
    );
    // A fold that does not fit is put off, and waits for more free space.
    assert_eq!(
        plan_table(&chain(20, 4), &input(1), &policy, |_| false),
        TablePlan::Delta { put_off: true }
    );
    let mut blocked = chain(20, 4);
    blocked.fold_blocked_at = Some(10);
    assert_eq!(
        plan_table(&blocked, &input(1), &policy, fits),
        TablePlan::Delta { put_off: false }
    );
    assert_eq!(
        plan_table(
            &blocked,
            &TablePlanInput {
                free: 11,
                ..input(1)
            },
            &policy,
            fits
        ),
        TablePlan::Fold { policy: true }
    );
    // Forced folds: the first head, a repair, a full chain, whatever the
    // space.
    let none = TableChain::default();
    assert_eq!(
        plan_table(&none, &input(1), &policy, |_| false),
        TablePlan::Fold { policy: false }
    );
    let mut repair = chain(20, 1);
    repair.repair = true;
    assert_eq!(
        plan_table(&repair, &input(0), &policy, |_| false),
        TablePlan::Fold { policy: false }
    );
    let huge = TableFoldPolicy {
        max_deltas: 10_000,
        min_fold_records: u64::MAX,
    };
    assert_eq!(
        plan_table(&chain(20, TABLE_HEAD_DELTAS - 1), &input(1), &huge, fits),
        TablePlan::Delta { put_off: false }
    );
    assert_eq!(
        plan_table(&chain(20, TABLE_HEAD_DELTAS), &input(1), &huge, |_| false),
        TablePlan::Fold { policy: false }
    );
    // An empty table names nothing: its chain goes.
    let gone = TablePlanInput {
        wanted: false,
        ..input(1)
    };
    assert_eq!(
        plan_table(&chain(20, 2), &gone, &policy, fits),
        TablePlan::Drop
    );
    assert_eq!(plan_table(&none, &gone, &policy, fits), TablePlan::Carry);
}

// ---------------------------------------------------------------------------
// Damage (F21)
// ---------------------------------------------------------------------------

#[test]
fn a_damaged_catalogue_delta_record_drops_only_the_names_it_may_have_changed() {
    let mut cfg = small(6320);
    cfg.volumes = vec![(0, 64), (1, 64), (2, 64)];
    let mut s = sim_with(cfg);
    s.boot().unwrap();
    s.engine().set_volume_name(0, "alpha").unwrap();
    s.engine().set_volume_name(1, "beta").unwrap();
    s.request_cp().unwrap();
    s.engine().set_volume_name(1, "gamma").unwrap();
    s.request_cp().unwrap();
    s.engine().set_volume_name(2, "delta").unwrap();
    s.request_cp().unwrap();
    ok(&mut s, 0, 0, 4);
    s.request_cp().unwrap();
    let (head, _) = head_of(&mut s, TableKind::Catalog);
    assert_eq!(head.deltas.len(), 2);
    // A byte of the name in the first delta's only record.
    flip(
        &mut s,
        head.deltas[0].region.pba as usize * BLOCK_SIZE + 64 + 40,
    );
    s.crash_daemon().unwrap();
    s.boot().unwrap();
    s.verify_recovery().unwrap();
    // The lost record may have renamed any name written before it: alpha
    // and beta are dropped, delta (written after) stays. No volume goes.
    assert_eq!(
        names(&mut s, &[0, 1, 2]),
        [None, None, Some("delta".into())]
    );
    assert!(warned(&mut s, |w| matches!(
        w,
        OperatorWarning::CatalogDamaged { .. }
    )));
    assert_eq!(s.engine().volumes().len(), 3);
    s.read(1, 0, 4).unwrap();
    // The boot commit folded the catalogue: the chain is whole again.
    assert_eq!(s.engine().pool_status().catalog_deltas, 0);
    let before = tables(&mut s);
    s.crash_daemon().unwrap();
    s.boot().unwrap();
    s.verify_recovery().unwrap();
    assert!(!warned(&mut s, |w| matches!(
        w,
        OperatorWarning::CatalogDamaged { .. }
    )));
    assert_eq!(tables(&mut s), before);
    s.attest_clean().unwrap();
    // A dropped name is given again by index.
    s.engine().set_volume_name(0, "alpha").unwrap();
    s.request_cp().unwrap();
    assert_eq!(s.engine().find_volume("alpha"), Some(0));
}

#[test]
fn a_damaged_page_header_drops_its_records_and_keeps_the_next_page() {
    let mut cfg = small(6325);
    cfg.volumes = (0..9).map(|v| (v, 32)).collect();
    cfg.segments = 64;
    let mut s = sim_with(cfg);
    s.boot().unwrap();
    s.engine().set_volume_name(0, "v0").unwrap();
    s.request_cp().unwrap();
    // Eight names in one CP: a delta of two pages (seven records, then one).
    for v in 1..9u32 {
        s.engine().set_volume_name(v, &format!("v{v}")).unwrap();
    }
    s.request_cp().unwrap();
    ok(&mut s, 0, 0, 2);
    s.request_cp().unwrap();
    let (head, _) = head_of(&mut s, TableKind::Catalog);
    assert_eq!(head.deltas.len(), 1);
    assert_eq!(head.deltas[0].region.pages, 2);
    // The first page's header (its CP field).
    flip(&mut s, head.deltas[0].region.pba as usize * BLOCK_SIZE + 9);
    s.crash_daemon().unwrap();
    s.boot().unwrap();
    s.verify_recovery().unwrap();
    let got = names(&mut s, &(0..9).collect::<Vec<u32>>());
    let mut want: Vec<Option<String>> = vec![None; 9];
    want[8] = Some("v8".into());
    assert_eq!(got, want);
    assert!(warned(&mut s, |w| matches!(
        w,
        OperatorWarning::CatalogDamaged { .. }
    )));
    s.attest_clean().unwrap();
}

#[test]
fn a_damaged_base_keeps_every_other_record_and_the_names_after_it() {
    let mut cfg = small(6330);
    cfg.volumes = vec![(0, 64), (1, 64), (2, 64), (3, 64)];
    let mut s = sim_with(cfg);
    s.boot().unwrap();
    s.engine().set_volume_name(0, "alpha").unwrap();
    s.engine().set_volume_name(1, "beta").unwrap();
    s.request_cp().unwrap();
    s.engine().set_volume_name(2, "gamma").unwrap();
    s.request_cp().unwrap();
    s.engine().set_volume_name(3, "delta").unwrap();
    s.request_cp().unwrap();
    let (head, _) = head_of(&mut s, TableKind::Catalog);
    assert_eq!(head.base_records(), 2);
    // The base's first record (volume 0's name).
    flip(&mut s, head.base.region.pba as usize * BLOCK_SIZE + 40);
    s.crash_daemon().unwrap();
    s.boot().unwrap();
    s.verify_recovery().unwrap();
    assert_eq!(
        names(&mut s, &[0, 1, 2, 3]),
        [
            None,
            Some("beta".into()),
            Some("gamma".into()),
            Some("delta".into())
        ]
    );
    assert!(warned(&mut s, |w| matches!(
        w,
        OperatorWarning::CatalogDamaged { .. }
    )));
    s.attest_clean().unwrap();
}

#[test]
fn a_damaged_snapshot_record_quarantines_what_it_may_have_changed_and_the_mark_persists() {
    let mut cfg = small(6340);
    cfg.volumes = vec![(0, 64)];
    let mut s = sim_with(cfg);
    s.boot().unwrap();
    ok(&mut s, 0, 0, 8);
    let s1 = s.snapshot(0).unwrap();
    ok(&mut s, 0, 8, 8);
    let s2 = s.snapshot(0).unwrap();
    // Overwritten after s2: only s2 holds these blocks.
    ok(&mut s, 0, 8, 8);
    s.engine().set_snapshot_pins(s1, PIN_USER).unwrap();
    s.request_cp().unwrap();
    ok(&mut s, 0, 16, 8);
    let s3 = s.snapshot(0).unwrap();
    ok(&mut s, 0, 30, 2);
    s.request_cp().unwrap();
    let (head, _) = head_of(&mut s, TableKind::Snapshots);
    // s1 in the base; s2, the pin of s1 and s3 in three deltas.
    assert_eq!((head.base_records(), head.deltas.len()), (1, 3), "{head:?}");
    // The pin's record (the second delta's only record), in its uuid.
    flip(
        &mut s,
        head.deltas[1].region.pba as usize * BLOCK_SIZE + 64 + 20,
    );
    s.crash_daemon().unwrap();
    s.boot().unwrap();
    assert!(warned(&mut s, |w| matches!(
        w,
        OperatorWarning::SnapshotTableDamaged { .. }
    )));
    // s1 and s2 last changed before the damaged region: uncertain,
    // quarantined; s3 came after it: whole. The lost pin is lost.
    let listed = s.engine().snapshots();
    let get = |id: u64| {
        listed
            .iter()
            .find(|x| x.snap_id == id)
            .cloned()
            .expect("listed")
    };
    assert!(
        get(s1).damaged && get(s2).damaged && !get(s3).damaged,
        "{listed:?}"
    );
    assert_eq!(get(s1).flags, 0);
    let mut buf = vec![0u8; BLOCK_SIZE];
    assert!(matches!(
        s.engine().snapshot_read(s1, 0, &mut buf),
        Err(EngineError::SnapshotDamaged { .. })
    ));
    s.engine().snapshot_read(s3, 0, &mut buf).unwrap();
    s.snaps.retain(|x| x.snap_id == s3);
    s.verify_recovery().unwrap();
    // The boot commit repaired the table; the mark stays across reboots.
    assert_eq!(s.engine().pool_status().snapshot_table_deltas, 0);
    s.crash_daemon().unwrap();
    s.boot().unwrap();
    assert!(!warned(&mut s, |w| matches!(
        w,
        OperatorWarning::SnapshotTableDamaged { .. }
    )));
    let listed = s.engine().snapshots();
    assert!(
        listed.iter().filter(|x| x.damaged).count() == 2,
        "{listed:?}"
    );
    let records = s.engine().snapshot_records();
    assert!(records.iter().filter(|r| r.is_uncertain()).count() == 2);
    // Deleting an uncertain snapshot releases nothing: its blocks may
    // already belong to others. What it held stays counted, a leak the
    // verification reports as warnings, never as damage.
    let seen = s.events.len();
    s.delete_snapshot(s2).unwrap();
    let released = s.events[seen..].iter().find_map(|e| match e {
        Event::SnapshotDeleted {
            snap_id, released, ..
        } if *snap_id == s2 => Some(*released),
        _ => None,
    });
    assert_eq!(released, Some(0));
    let report = s.attest_with(otap_cp::VerifyOptions::default()).unwrap();
    assert_eq!(report.errors, 0, "{:?}", report.findings);
    assert_eq!(report.verdict, Some(Verdict::Warnings));
    assert!(report
        .findings
        .iter()
        .any(|f| f.kind == FindingKind::LiveBlocks));
    assert!(report
        .findings
        .iter()
        .any(|f| f.kind == FindingKind::SnapshotQuarantined && f.snap_id == Some(s1)));
}

#[test]
fn a_damaged_tombstone_brings_back_a_quarantined_snapshot_whose_deletion_frees_nothing() {
    let mut cfg = small(6350);
    cfg.volumes = vec![(0, 64)];
    let mut s = sim_with(cfg);
    s.boot().unwrap();
    ok(&mut s, 0, 0, 8);
    let s1 = s.snapshot(0).unwrap();
    ok(&mut s, 0, 8, 8);
    let s2 = s.snapshot(0).unwrap();
    ok(&mut s, 0, 0, 8);
    s.delete_snapshot(s1).unwrap();
    ok(&mut s, 0, 20, 4);
    s.request_cp().unwrap();
    s.request_cp().unwrap();
    let (head, _) = head_of(&mut s, TableKind::Snapshots);
    let tomb = head.deltas.last().copied().expect("the tombstone's delta");
    assert_eq!(tomb.records, 1);
    flip(&mut s, tomb.region.pba as usize * BLOCK_SIZE + 64 + 4);
    s.crash_daemon().unwrap();
    s.boot().unwrap();
    assert!(warned(&mut s, |w| matches!(
        w,
        OperatorWarning::SnapshotTableDamaged { .. }
    )));
    let listed = s.engine().snapshots();
    let back = listed.iter().find(|x| x.snap_id == s1).expect("s1 is back");
    assert!(back.damaged, "a resurrected snapshot is quarantined");
    assert!(listed.iter().find(|x| x.snap_id == s2).unwrap().damaged);
    let mut buf = vec![0u8; BLOCK_SIZE];
    assert!(s.engine().snapshot_read(s1, 0, &mut buf).is_err());
    // Its deletion frees nothing it might share with what reused its space.
    let seen = s.events.len();
    s.delete_snapshot(s1).unwrap();
    for e in &s.events[seen..] {
        if let Event::Internal { what } = e {
            assert!(!what.contains("went negative"), "{what}");
        }
        if let Event::SnapshotDeleted {
            snap_id, released, ..
        } = e
        {
            assert_eq!((*snap_id, *released), (s1, 0));
        }
    }
    s.request_cp().unwrap();
    let report = s.attest_with(otap_cp::VerifyOptions::default()).unwrap();
    assert_eq!(report.errors, 0, "{:?}", report.findings);
    s.read(0, 0, 64).unwrap();
}

#[test]
fn a_lost_head_written_by_an_older_cp_boots_without_the_table() {
    let mut cfg = small(6360);
    cfg.volumes = vec![(0, 64)];
    let mut s = sim_with(cfg);
    s.boot().unwrap();
    ok(&mut s, 0, 0, 8);
    s.snapshot(0).unwrap();
    ok(&mut s, 0, 0, 8);
    s.request_cp().unwrap();
    s.request_cp().unwrap();
    let (_, root) = head_of(&mut s, TableKind::Snapshots);
    let slot = newest_slot(&mut s);
    assert!(
        slot.sb.snap_head_cp < slot.sb.cp_seq,
        "the head is an older CP's"
    );
    flip(&mut s, root.pba as usize * BLOCK_SIZE + 100);
    s.crash_daemon().unwrap();
    let info = s.boot().unwrap();
    assert!(
        !info.rewound,
        "damage that every candidate shares tears none"
    );
    assert!(warned(&mut s, |w| matches!(
        w,
        OperatorWarning::SnapshotTableDamaged { .. }
    )));
    assert!(s.engine().snapshots().is_empty());
    // The volume and its data are intact; what the snapshot held stays
    // counted (a leak, reported as a warning).
    s.snaps.clear();
    s.check_disk_prefix().unwrap();
    assert_eq!(s.read(0, 0, 16).unwrap().0, Status::Ok);
    let report = s.attest_with(otap_cp::VerifyOptions::default()).unwrap();
    assert_eq!(report.errors, 0, "{:?}", report.findings);
}

#[test]
fn a_table_region_the_newest_commit_wrote_tears_that_commit() {
    let mut cfg = small(6370);
    cfg.volumes = vec![(0, 64)];
    let mut s = sim_with(cfg);
    s.boot().unwrap();
    s.engine().set_volume_name(0, "first").unwrap();
    s.request_cp().unwrap();
    s.engine().set_volume_name(0, "second").unwrap();
    let newest = s.request_cp().unwrap();
    let (head, _) = head_of(&mut s, TableKind::Catalog);
    let delta = *head.deltas.last().unwrap();
    assert_eq!(delta.cp_seq, newest);
    flip(&mut s, delta.region.pba as usize * BLOCK_SIZE + 64 + 40);
    s.crash_daemon().unwrap();
    let info = s.boot().unwrap();
    assert!(info.rewound, "{info:?}");
    assert!(
        info.torn
            .iter()
            .any(|(cp, why)| *cp == newest && why.contains("Table")),
        "{info:?}"
    );
    assert_eq!(s.engine().volume_name(0), Some("first"));
    assert!(!warned(&mut s, |w| matches!(
        w,
        OperatorWarning::CatalogDamaged { .. }
    )));
}

/// A snapshot whose root page did not verify at boot was never counted:
/// its deletion frees nothing (it used to free its root page, taking the
/// bytes from whatever region now shares that segment).
#[test]
fn deleting_a_snapshot_whose_root_did_not_load_frees_nothing_it_never_counted() {
    let mut cfg = small(6380);
    cfg.volumes = vec![(0, 64)];
    let mut s = sim_with(cfg);
    s.boot().unwrap();
    ok(&mut s, 0, 0, 8);
    let s1 = s.snapshot(0).unwrap();
    ok(&mut s, 0, 0, 8);
    s.request_cp().unwrap();
    let root_pba = s
        .engine()
        .snapshot_records()
        .iter()
        .find(|r| r.snap_id == s1)
        .unwrap()
        .map_root_pba;
    flip(&mut s, root_pba as usize * BLOCK_SIZE + 300);
    s.crash_daemon().unwrap();
    s.boot().unwrap();
    assert!(s.engine().snapshots()[0].damaged);
    let seen = s.events.len();
    s.delete_snapshot(s1).unwrap();
    for _ in 0..3 {
        s.request_cp().unwrap();
    }
    for e in &s.events[seen..] {
        if let Event::Internal { what } = e {
            assert!(!what.contains("went negative"), "{what}");
        }
    }
    let report = s.attest_with(otap_cp::VerifyOptions::default()).unwrap();
    assert!(
        !report
            .findings
            .iter()
            .any(|f| f.kind == FindingKind::MetaBytes && f.severity == otap_cp::Severity::Error),
        "{:?}",
        report.findings
    );
}

/// A map page or index the live tree and a snapshot share is one region:
/// a boot counts its bytes once (the Step 4 review's hazard 12: counted
/// twice, the segment could not be freed until the next boot recounted
/// it, and the verification's reconciliation reports the drift).
#[test]
fn a_region_two_trees_share_is_counted_once_at_boot() {
    let mut cfg = small(6390);
    cfg.volumes = vec![(0, 64), (1, 64)];
    let mut s = sim_with(cfg);
    s.boot().unwrap();
    ok(&mut s, 0, 0, 16);
    ok(&mut s, 1, 0, 16);
    // Taken at a fold with nothing written since: the snapshot shares the
    // live tree's index and every page.
    s.snapshot(0).unwrap();
    s.snapshot(0).unwrap();
    s.crash_daemon().unwrap();
    s.boot().unwrap();
    s.verify_recovery().unwrap();
    s.attest_clean().unwrap();
    s.shutdown_clean().unwrap();
    s.boot().unwrap();
    s.attest_clean().unwrap();
}

// ---------------------------------------------------------------------------
// The reserve
// ---------------------------------------------------------------------------

/// The pre-check every table fold makes places nothing and honours the
/// reserve of the commit's kind: the boot's 0, the shutdown's 1, and the
/// 2 every other commit leaves free (E31).
#[test]
fn the_fold_check_honours_the_reserve_of_each_kind_of_commit() {
    let geom = Geometry {
        stripe_bytes: 32 * 1024,
        segment_bytes: 8 * 32 * 1024,
        capacity_bytes: 64 * 1024 * 1024,
        flush_opaque: false,
    };
    let layout = DiskLayout::new(&geom).unwrap();
    let per_segment = u64::from(layout.blocks_per_segment());
    let mut table = SegmentTable::empty(layout.segments);
    // Every segment in use but two, apart.
    for _ in 0..layout.segments {
        table
            .allocate(SegKind::Metadata, 1, layout.stripe_bytes)
            .unwrap();
    }
    for pseg in [3u64, 9] {
        table.free(pseg).unwrap();
    }
    assert_eq!(table.free_count(), 2);
    let check = |reserve: u32, pages: u64| {
        MetaStream::new(&layout, &table, None, 7, reserve).can_place(pages)
    };
    // One segment's worth: the boot (0) and the shutdown (1) may take one
    // of the last two; a regular commit (2) may not.
    assert!(check(0, per_segment));
    assert!(check(1, per_segment));
    assert!(!check(2, per_segment));
    // Two segments' worth needs two adjacent free segments: none here.
    assert!(!check(0, per_segment + 1));
    // The check placed nothing.
    assert_eq!(table.free_count(), 2);
}

/// A pool full for its users: the commits that carry table changes keep
/// the floor (a regular commit never takes the last two segments), the
/// shutdown and the boot commits still land with them, and the tables
/// survive.
#[test]
fn table_changes_on_a_full_pool_keep_the_floor_and_land_with_the_shutdown_and_the_boot() {
    let mut cfg = small(6395);
    cfg.segments = 10;
    cfg.stripes_per_segment = 4;
    cfg.volumes = vec![(0, 2048)];
    cfg.reserve_segments = Some(0);
    cfg.cleaner.background = false;
    let mut s = sim_with(cfg);
    s.boot().unwrap();
    s.engine().set_volume_name(0, "data").unwrap();
    s.request_cp().unwrap();
    // Fill until the pool refuses writes.
    let mut lba = 0u64;
    loop {
        let c = s.write(0, lba, 8).unwrap();
        if c.status != Status::Ok {
            break;
        }
        lba += 8;
        s.engine().request_cp();
        s.ticks(50).unwrap();
        assert!(
            s.engine().free_segments() >= otap_cp::META_RESERVE_SEGMENTS,
            "a regular commit took the floor"
        );
        assert!(lba < 2048, "the pool never filled");
    }
    // Names change on the full pool.
    s.engine().set_volume_name(0, "full").unwrap();
    for _ in 0..4 {
        s.engine().request_cp();
        s.ticks(50).unwrap();
        assert!(s.engine().free_segments() >= otap_cp::META_RESERVE_SEGMENTS);
    }
    let landed =
        s.engine().pool_status().catalog_entries == 1 && newest_slot(&mut s).sb.catalog_head_cp > 0;
    assert!(landed);
    s.shutdown_clean().unwrap();
    s.boot().unwrap();
    // Whatever commit carried the rename, the name is the engine's last.
    let name = s.engine().volume_name(0).map(str::to_string);
    assert!(
        name.as_deref() == Some("full") || name.as_deref() == Some("data"),
        "{name:?}"
    );
    s.engine().request_cp();
    s.ticks(50).unwrap();
    assert!(s.engine().free_segments() >= otap_cp::META_RESERVE_SEGMENTS);
}

// ---------------------------------------------------------------------------
// The format
// ---------------------------------------------------------------------------

#[test]
fn a_pool_written_by_format_2_3_is_refused() {
    let mut cfg = small(6398);
    cfg.volumes = vec![(0, 64)];
    let mut s = sim_with(cfg);
    s.boot().unwrap();
    ok(&mut s, 0, 0, 4);
    s.shutdown_clean().unwrap();
    // Every slot of the ring rewritten as a 2.3 writer would have.
    let layout = *s.disk().layout();
    for slot in SbSlot::ALL {
        let at = layout.sb_pba(slot) as usize * BLOCK_SIZE;
        let bytes = s.disk().durable()[at..at + SUPERBLOCK_SLOT_SIZE].to_vec();
        if let Ok(mut parsed) = SuperblockSlot::read(&bytes) {
            parsed.sb.ver = 0x0203;
            parsed.seal().unwrap();
            s.disk()
                .corrupt_durable(at, otap_format::IntoBytes::as_bytes(&parsed));
        }
    }
    let err = s.boot().unwrap_err();
    assert!(err.contains("2.4 and later"), "{err}");
}

// ---------------------------------------------------------------------------
// The random walk: the boot's tables are the engine's
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
enum Op {
    Name { vol: u8, n: u8 },
    Create { vol: u8 },
    Delete { vol: u8 },
    Snapshot { vol: u8, named: bool },
    DeleteSnapshot { which: u8 },
    Pin { which: u8, flags: u8 },
    Base { which: u8 },
    Write { vol: u8, lba: u8 },
    Cp,
    Fold,
    Power { tear: u8 },
    Daemon,
    MidCp { ticks: u8, power: bool },
    Restart,
    Attest,
}

fn op() -> impl Strategy<Value = Op> {
    prop_oneof![
        4 => (any::<u8>(), any::<u8>()).prop_map(|(vol, n)| Op::Name { vol, n }),
        1 => any::<u8>().prop_map(|vol| Op::Create { vol }),
        1 => any::<u8>().prop_map(|vol| Op::Delete { vol }),
        3 => (any::<u8>(), any::<bool>()).prop_map(|(vol, named)| Op::Snapshot { vol, named }),
        2 => any::<u8>().prop_map(|which| Op::DeleteSnapshot { which }),
        2 => (any::<u8>(), any::<u8>()).prop_map(|(which, flags)| Op::Pin { which, flags }),
        1 => any::<u8>().prop_map(|which| Op::Base { which }),
        3 => (any::<u8>(), any::<u8>()).prop_map(|(vol, lba)| Op::Write { vol, lba }),
        3 => Just(Op::Cp),
        1 => Just(Op::Fold),
        1 => (0..4u8).prop_map(|tear| Op::Power { tear }),
        1 => Just(Op::Daemon),
        2 => (1..40u8, any::<bool>()).prop_map(|(ticks, power)| Op::MidCp { ticks, power }),
        1 => Just(Op::Restart),
        1 => Just(Op::Attest),
    ]
}

/// What the engine held: the CP and its tables.
type Held = (u64, Vec<CatalogRec>, Vec<SnapRecord>);

fn held(s: &mut Sim) -> Held {
    (
        s.engine().cp_seq(),
        s.engine().catalog_records().to_vec(),
        s.engine().snapshot_records(),
    )
}

/// After a stop: the boot's tables must be what the engine held at the CP
/// the boot selected: the last committed one, or the one in flight when
/// its slot landed.
fn check_boot(
    s: &mut Sim,
    last: &Held,
    in_flight: Option<&Held>,
    what: &str,
) -> Result<(), String> {
    let boot = s.last_boot().ok_or("no boot")?;
    let got = held(s);
    let want = if boot.cp_seq == last.0 {
        last
    } else {
        match in_flight {
            Some(f) if f.0 == boot.cp_seq => f,
            _ => {
                return Err(format!(
                    "{what}: the boot selected CP {} where CP {} was committed and {:?} was in \
                     flight",
                    boot.cp_seq,
                    last.0,
                    in_flight.map(|f| f.0)
                ))
            }
        }
    };
    if got.1 != want.1 {
        return Err(format!(
            "{what}: the boot's catalogue {:?} differs from the engine's at CP {}: {:?}",
            got.1
                .iter()
                .map(|c| (c.vol, c.name().to_string()))
                .collect::<Vec<_>>(),
            want.0,
            want.1
                .iter()
                .map(|c| (c.vol, c.name().to_string()))
                .collect::<Vec<_>>()
        ));
    }
    if got.2 != want.2 {
        return Err(format!(
            "{what}: the boot's snapshot records differ from the engine's at CP {}: {:?} vs {:?}",
            want.0, got.2, want.2
        ));
    }
    Ok(())
}

fn tear_of(n: u8) -> TearPolicy {
    match n {
        0 => TearPolicy {
            drop: 1,
            whole: 0,
            torn: 0,
        },
        1 => TearPolicy {
            drop: 0,
            whole: 1,
            torn: 0,
        },
        2 => TearPolicy {
            drop: 0,
            whole: 0,
            torn: 1,
        },
        _ => TearPolicy::default(),
    }
}

/// Random names, renames, volume creations and deletions, snapshots named
/// and not, their deletion, pins and replication bases, writes, folds and
/// crashes of every kind, mid-CP ones included, under a fold policy small
/// enough that both chains fold often: every boot's tables equal the
/// engine's at the CP it booted, the chains stay within the policy, and
/// the pool attests clean.
fn walk(seed: u64, ops: &[Op]) -> Result<(), String> {
    let mut cfg = small(seed);
    cfg.volumes = vec![(0, 64), (1, 64)];
    cfg.segments = 64;
    cfg.table_fold = TableFoldPolicy {
        max_deltas: 3,
        min_fold_records: 4,
    };
    let bound = 3;
    let mut s = Sim::new(cfg)?;
    s.boot()?;
    let mut counter = 0u32;
    for (i, op) in ops.iter().enumerate() {
        let step = |e: String| format!("seed {seed} op {i} {op:?}: {e}");
        match *op {
            Op::Name { vol, n } => {
                let vol = u32::from(vol % 4);
                if s.engine().volume(vol).is_some_and(|v| v.present) {
                    counter += 1;
                    let name = format!("n{}-{counter}", n % 7);
                    match s.engine().set_volume_name(vol, &name) {
                        Ok(()) | Err(EngineError::NameExists { .. }) => {}
                        Err(e) => return Err(step(e.to_string())),
                    }
                }
            }
            Op::Create { vol } => {
                let vol = 2 + u32::from(vol % 2);
                if !s.engine().volume(vol).is_some_and(|v| v.present) {
                    s.create_volume(vol, 32).map_err(step)?;
                }
            }
            Op::Delete { vol } => {
                let vol = 2 + u32::from(vol % 2);
                let has_snaps = s.engine().snapshots().iter().any(|x| x.vol == vol);
                if s.engine()
                    .volume(vol)
                    .is_some_and(|v| v.present && v.serving)
                    && !has_snaps
                {
                    s.delete_volume(vol).map_err(step)?;
                }
            }
            Op::Snapshot { vol, named } => {
                let vol = u32::from(vol % 2);
                if s.engine().snapshots().len() < 6 && !s.engine().nvfail_set().get(vol) {
                    if named {
                        counter += 1;
                        let before = s.engine().cp_seq();
                        s.engine()
                            .create_named_snapshot(
                                vol,
                                &format!("s{counter}"),
                                SnapKind::Crash,
                                SnapOrigin::User,
                            )
                            .map_err(|e| step(e.to_string()))?;
                        s.tick_until(|e| e.cp_seq() > before && e.is_quiet(), "the snapshot")
                            .map_err(step)?;
                    } else {
                        s.snapshot(vol).map_err(step)?;
                    }
                }
            }
            Op::DeleteSnapshot { which } => {
                let listed = s.engine().snapshots();
                if !listed.is_empty() {
                    let id = listed[usize::from(which) % listed.len()].snap_id;
                    // A pin waiting for its CP refuses the deletion, as a
                    // committed one does.
                    match s.engine().delete_snapshot(id) {
                        Ok(()) => {
                            s.tick_until(
                                |e| !e.snapshots().iter().any(|x| x.snap_id == id) && e.is_quiet(),
                                "the deletion",
                            )
                            .map_err(step)?;
                            s.snaps.retain(|x| x.snap_id != id);
                        }
                        Err(
                            EngineError::SnapshotPinned { .. } | EngineError::VolumeBusy { .. },
                        ) => {}
                        Err(e) => return Err(step(e.to_string())),
                    }
                }
            }
            Op::Pin { which, flags } => {
                let listed = s.engine().snapshots();
                if !listed.is_empty() {
                    let snap = &listed[usize::from(which) % listed.len()];
                    if !snap.deleting {
                        let flags = flags & (PIN_USER | otap_format::PIN_SCHEDULE);
                        s.engine()
                            .set_snapshot_pins(snap.snap_id, flags)
                            .map_err(|e| step(e.to_string()))?;
                    }
                }
            }
            Op::Base { which } => {
                let listed = s.engine().snapshots();
                if !listed.is_empty() {
                    let snap = &listed[usize::from(which) % listed.len()];
                    if !snap.deleting {
                        let (vol, id) = (snap.vol, snap.snap_id);
                        s.engine()
                            .set_replication_base(vol, Some(id))
                            .map_err(|e| step(e.to_string()))?;
                    }
                }
            }
            Op::Write { vol, lba } => {
                let vol = u32::from(vol % 2);
                let lba = u64::from(lba) % 60;
                let c = s.write(vol, lba, 4).map_err(step)?;
                if !matches!(c.status, Status::Ok | Status::NvFailed | Status::NoSpace) {
                    return Err(step(format!("write {:?}", c.status)));
                }
            }
            Op::Cp => {
                s.request_cp().map_err(step)?;
            }
            Op::Fold => {
                s.request_fold().map_err(step)?;
            }
            Op::Power { tear } => {
                let last = held(&mut s);
                let in_flight = s.engine().in_flight_tables();
                s.disk().set_tear_policy(tear_of(tear));
                s.crash_power().map_err(step)?;
                s.boot().map_err(step)?;
                check_boot(&mut s, &last, in_flight.as_ref(), "power").map_err(step)?;
                s.verify_recovery().map_err(step)?;
                s.clear_nvfail().map_err(step)?;
            }
            Op::Daemon => {
                let last = held(&mut s);
                let in_flight = s.engine().in_flight_tables();
                s.crash_daemon().map_err(step)?;
                s.boot().map_err(step)?;
                check_boot(&mut s, &last, in_flight.as_ref(), "daemon").map_err(step)?;
                s.verify_recovery().map_err(step)?;
            }
            Op::MidCp { ticks, power } => {
                // A CP with the pending changes, crashed after a few ticks
                // of a slow device: wherever it stands.
                s.disk().set_schedule(Schedule {
                    max_per_poll: 1,
                    reorder: false,
                    queue_depth: 64,
                });
                let mut committed = held(&mut s);
                s.engine().request_cp();
                let mut in_flight: Option<Held> = None;
                for _ in 0..ticks {
                    s.tick().map_err(step)?;
                    if let Some(tables) = s.engine().in_flight_tables() {
                        in_flight = Some(tables);
                    }
                    if s.engine().cp_seq() != committed.0 {
                        committed = held(&mut s);
                    }
                }
                let in_flight = in_flight.filter(|f| f.0 > committed.0);
                if power {
                    s.disk().set_tear_policy(TearPolicy::default());
                    s.crash_power().map_err(step)?;
                } else {
                    s.crash_daemon().map_err(step)?;
                }
                s.disk().set_schedule(Schedule::default());
                s.boot().map_err(step)?;
                check_boot(&mut s, &committed, in_flight.as_ref(), "mid-cp").map_err(step)?;
                s.verify_recovery().map_err(step)?;
                s.clear_nvfail().map_err(step)?;
            }
            Op::Restart => {
                s.request_cp().map_err(step)?;
                let last = held(&mut s);
                s.shutdown_clean().map_err(step)?;
                s.boot().map_err(step)?;
                // The shutdown CP carries nothing new: the boot is `last`
                // with the CP number of the shutdown's commit.
                let got = held(&mut s);
                if (got.1.clone(), got.2.clone()) != (last.1.clone(), last.2.clone()) {
                    return Err(step("a clean restart changed the tables".to_string()));
                }
                s.verify_recovery().map_err(step)?;
            }
            Op::Attest => {
                s.attest_clean().map_err(step)?;
            }
        }
        let (snap_chain, catalog_chain) = s.engine().table_chains();
        if snap_chain.deltas.len() > bound || catalog_chain.deltas.len() > bound {
            return Err(step(format!(
                "a chain outgrew the policy: {} and {} deltas",
                snap_chain.deltas.len(),
                catalog_chain.deltas.len()
            )));
        }
    }
    s.attest_clean()
        .map_err(|e| format!("seed {seed} at the end: {e}"))?;
    let last = {
        s.request_cp()?;
        held(&mut s)
    };
    s.crash_daemon()?;
    s.boot()?;
    check_boot(&mut s, &last, None, "the end").map_err(|e| format!("seed {seed}: {e}"))?;
    Ok(())
}

/// The case count: `PROPTEST_CASES` when set (the 10,000-case gate), else
/// `default` for the plain run.
fn proptest_cases(default: u32) -> u32 {
    std::env::var("PROPTEST_CASES")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(default)
}

proptest! {
    #![proptest_config(ProptestConfig {
        failure_persistence: None,
        cases: proptest_cases(64),
        max_shrink_iters: 256,
        ..ProptestConfig::default()
    })]

    /// The boot result equals the in-memory model, whatever ran and
    /// wherever the crash fell.
    #[test]
    fn random_table_changes_with_folds_and_crashes_boot_to_the_engines_tables(
        seed in any::<u64>(),
        ops in prop::collection::vec(op(), 4..28),
    ) {
        if let Err(e) = walk(seed, &ops) {
            prop_assert!(false, "{e}");
        }
    }
}
