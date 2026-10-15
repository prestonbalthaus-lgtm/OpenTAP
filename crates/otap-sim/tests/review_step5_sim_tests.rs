//! Opus-QA-Adversarial, Step 5: an independent attack on snapshot
//! deletion, the segment cleaner, the reserve, rollback / resize / delete /
//! offline, and the format 2.3 additions, over the simulator.
//!
//! The review's `bug_*` tests reproduced ten defects (D1 to D4, W1 to W4 in
//! the Step 5 entry of `08-engineering-log.md`); they are ported in place
//! with the fixed behaviour as their expectation, each with a note naming
//! its defect. The other tests pin behaviour that was right as reviewed. The two property tests at
//! the end are the brief's: GC plus crash invariance, and the recovery of a
//! pool driven to 100% of its user capacity. Their case counts follow
//! `PROPTEST_CASES` when it is set (the merge gate), and default to a count
//! that keeps the plain run within minutes.

use std::collections::{HashMap, HashSet};

use otap_cp::{CleanerPolicy, EngineError, Event, FoldPolicy, Phase, Status};
use otap_format::{
    SbSlot, SnapKind, SnapOrigin, SuperblockSlot, Uuid, BLOCK_SIZE, PIN_REPLICATION,
    SUPERBLOCK_SLOT_SIZE,
};
use otap_sim::{Schedule, Sim, SimConfig, SplitMix64, Stop, TearPolicy, TICK_BUDGET};
use proptest::prelude::*;

const DROP: TearPolicy = TearPolicy {
    drop: 1,
    whole: 0,
    torn: 0,
};
const WHOLE: TearPolicy = TearPolicy {
    drop: 0,
    whole: 1,
    torn: 0,
};
const TORN: TearPolicy = TearPolicy {
    drop: 0,
    whole: 0,
    torn: 1,
};

fn tear_of(n: u8) -> TearPolicy {
    match n % 4 {
        0 => DROP,
        1 => WHOLE,
        2 => TORN,
        _ => TearPolicy::default(),
    }
}

/// One completion per poll, in order: every phase of a CP lasts ticks.
fn fifo1() -> Schedule {
    Schedule {
        max_per_poll: 1,
        reorder: false,
        queue_depth: 64,
    }
}

fn no_auto_fold() -> FoldPolicy {
    FoldPolicy {
        log_bytes: 1 << 40,
        interval_ns: 1 << 60,
        max_cps: u64::MAX,
        max_dirty_pages: usize::MAX,
        max_overlay_entries: usize::MAX,
    }
}

/// The small pool with no folds of its own and no background cleaner, so a
/// test decides when each thing happens.
fn quiet_cfg(seed: u64, volumes: &[(u32, u64)]) -> SimConfig {
    let mut cfg = SimConfig::small(seed);
    cfg.volumes = volumes.to_vec();
    cfg.fold = no_auto_fold();
    cfg.cleaner.background = false;
    cfg
}

fn booted(cfg: SimConfig) -> Sim {
    let mut s = Sim::new(cfg).expect("the pool formats");
    s.boot().expect("the pool boots");
    s
}

fn ok(s: &mut Sim, vol: u32, lba: u64, blocks: u32) {
    let c = s.write(vol, lba, blocks).expect("the write completes");
    assert_eq!(c.status, Status::Ok, "write {vol}/{lba}+{blocks}");
}

fn uuid(n: u64) -> Uuid {
    let mut b = [0u8; 16];
    b[..8].copy_from_slice(&n.to_le_bytes());
    b[8..].copy_from_slice(b"qa-step5");
    Uuid::from_bytes(b)
}

/// Every block of `blocks` at `lba` of `vol`, through the engine (arena
/// first, then the committed map), regardless of nvfail.
fn peek(s: &mut Sim, vol: u32, lba: u64, blocks: u64) -> Result<Vec<u8>, EngineError> {
    let mut buf = vec![0u8; blocks as usize * BLOCK_SIZE];
    s.engine().peek(vol, lba, &mut buf)?;
    Ok(buf)
}

fn snapshot_image(s: &mut Sim, snap_id: u64, lba: u64, blocks: u64) -> Vec<u8> {
    let mut buf = vec![0u8; blocks as usize * BLOCK_SIZE];
    s.engine()
        .snapshot_read(snap_id, lba, &mut buf)
        .expect("the snapshot reads");
    buf
}

/// Writes every block of `vol` once (v1, 8 blocks at a time), then
/// overwrites every 8-block chunk except those at `lba % 32 == 0` (v2): the
/// segments v1 went to end up a quarter live, cleaning candidates whose
/// live blocks are the v1 chunks at `lba % 32 == 0`.
fn fragment(s: &mut Sim, vol: u32, size: u64) {
    for lba in (0..size).step_by(8) {
        ok(s, vol, lba, 8);
    }
    s.request_cp().unwrap();
    for lba in (0..size).step_by(8) {
        if lba % 32 != 0 {
            ok(s, vol, lba, 8);
        }
    }
    s.request_cp().unwrap();
}

/// The engine's complaints so far, for a failure message.
fn complaints(s: &Sim) -> Vec<String> {
    s.events
        .iter()
        .filter_map(|e| match e {
            Event::CpFailed { cp_seq, reason } => Some(format!("CP {cp_seq} failed: {reason}")),
            Event::Internal { what } => Some(format!("internal: {what}")),
            Event::CleanSkipped { pseg, reason } => Some(format!("skip {pseg}: {reason}")),
            _ => None,
        })
        .collect()
}

/// Requests a clean under a slow device and ticks until the cleaning CP is
/// staging with relocation stripes in flight.
fn into_cleaning_staging(s: &mut Sim) {
    s.settle().unwrap();
    s.disk().set_schedule(fifo1());
    let before = s.engine().stats().reloc_stripes;
    s.engine().request_clean();
    s.tick_until(
        |e| e.phase() == Phase::Staging && e.stats().reloc_stripes > before,
        "the cleaning CP to stage its relocation stripes",
    )
    .unwrap();
}

// ---------------------------------------------------------------------------
// Durability: rollbacks that race a cleaning CP or a snapshot in flight
// ---------------------------------------------------------------------------

/// A rollback asked for while the cleaning CP that relocates the target
/// snapshot's blocks is still staging rides that very CP: the volume takes
/// the snapshot's root as it was before the relocation (the old addresses),
/// the snapshot is rewritten to the new ones, and the drained segment is
/// counted empty and freed.
#[test]
fn a_rollback_asked_while_a_cleaning_cp_stages_waits_for_the_next_cp() {
    let mut s = booted(quiet_cfg(7001, &[(0, 400)]));
    fragment(&mut s, 0, 400);
    let r = s.snapshot(0).unwrap();
    // v3 over the chunks still live in the v1 segments: those blocks are
    // now the snapshot's alone, and their segments are cleaning candidates.
    for lba in (0..400).step_by(32) {
        ok(&mut s, 0, lba, 8);
    }
    s.request_cp().unwrap();
    s.audit_liveness().unwrap();
    let want = snapshot_image(&mut s, r, 0, 400);
    s.quiesce(0).unwrap();
    into_cleaning_staging(&mut s);
    // The volume is offline with nothing in the arena and nothing of it in
    // the CP's generation: the rollback is accepted now...
    let accepted = s.engine().rollback_volume(0, r);
    s.disk().set_schedule(Schedule::default());
    s.settle().unwrap();
    if accepted.is_err() {
        // ...or, were it refused while the clean is in flight, asked again.
        s.engine().rollback_volume(0, r).unwrap();
    }
    s.tick_until(
        |e| e.volume(0).is_some_and(|v| v.op_pending.is_none()) && e.is_quiet(),
        "the rollback",
    )
    .unwrap();
    assert!(
        s.events
            .iter()
            .any(|e| matches!(e, Event::VolumeRolledBack { vol: 0, .. })),
        "the rollback landed"
    );
    assert!(s.engine().stats().cleaned > 0, "the clean ran");
    // Review defect D1 (Step 5, `08-engineering-log.md`), fixed; the review's
    // expectation stood and the scenario is kept. As reviewed: the rolled-back volume must read as the snapshot did and every
    // block it maps must be counted live. Instead the CP that carried the
    // relocation also carried the rollback, built from the snapshot's root
    // as it was before the relocation rewrote it: the live map names the
    // old addresses in the drained segment, which the CP counted empty; the
    // audit finds the drift, and once the deferred free discards the
    // segment the volume reads garbage (`Corrupt`).
    let drift = s.engine().audit_liveness().unwrap();
    for _ in 0..3 {
        s.request_cp().unwrap();
    }
    let now = peek(&mut s, 0, 0, 400);
    assert!(
        drift.is_empty(),
        "live-block drift right after the rollback: {drift:?} ({:?})",
        complaints(&s)
    );
    assert_eq!(
        now.as_ref().ok(),
        Some(&want),
        "the volume reads as the snapshot after the drained segment was freed: {:?}",
        now.as_ref().err()
    );
}

/// A rollback asked for while a CP in flight is taking a snapshot of the
/// same volume is accepted (the in-flight snapshot is neither in the
/// committed list nor among the waiting requests), so the volume is rolled
/// back behind a snapshot newer than the target. The snapshot history
/// forks, and deleting the target later releases the blocks the live
/// volume still maps.
#[test]
fn a_rollback_is_refused_while_a_snapshot_of_the_volume_is_in_flight() {
    let mut s = booted(quiet_cfg(7002, &[(0, 64), (1, 128)]));
    ok(&mut s, 0, 0, 8); // A
    ok(&mut s, 1, 0, 64); // filler that seals A's segment
    s.request_cp().unwrap();
    let a = peek(&mut s, 0, 0, 8).unwrap();
    let r = s.snapshot(0).unwrap();
    ok(&mut s, 1, 0, 64); // the filler dies (volume 1 has no snapshot)
    ok(&mut s, 0, 0, 8); // B: A is the snapshot's alone
    s.request_cp().unwrap();
    s.quiesce(0).unwrap();
    s.disk().set_schedule(fifo1());
    s.engine()
        .create_snapshot(0, uuid(7002), SnapKind::Crash, SnapOrigin::User)
        .unwrap();
    s.tick_until(
        |e| {
            matches!(
                e.phase(),
                Phase::Metadata | Phase::Flushing | Phase::CommitA
            )
        },
        "the snapshot's CP past its metadata preparation",
    )
    .unwrap();
    let accepted = s.engine().rollback_volume(0, r).is_ok();
    s.disk().set_schedule(Schedule::default());
    s.tick_until(
        |e| {
            e.snapshots().len() == 2
                && e.volume(0).is_some_and(|v| v.op_pending.is_none())
                && e.is_quiet()
        },
        "the snapshot and the rollback",
    )
    .unwrap();
    let newer: Vec<u64> = {
        let r_cp = s
            .engine()
            .snapshots()
            .iter()
            .find(|x| x.snap_id == r)
            .unwrap()
            .cp_seq;
        s.engine()
            .snapshots()
            .iter()
            .filter(|x| x.vol == 0 && x.cp_seq > r_cp)
            .map(|x| x.snap_id)
            .collect()
    };
    let live = peek(&mut s, 0, 0, 8).unwrap();
    if accepted {
        assert_eq!(live, a, "the rollback restored A");
    }
    // Delete the target; the live volume keeps whatever it maps.
    s.engine().delete_snapshot(r).unwrap();
    s.tick_until(
        |e| e.snapshots().iter().all(|x| x.snap_id != r) && e.is_quiet(),
        "the deletion",
    )
    .unwrap();
    for _ in 0..3 {
        s.request_cp().unwrap();
    }
    let drift = s.engine().audit_liveness().unwrap();
    let after = peek(&mut s, 0, 0, 8);
    // Review defect D2 (Step 5, `08-engineering-log.md`), fixed; the review's
    // expectation stood and the scenario is kept. As reviewed: a rollback must be refused while a snapshot of the volume is in
    // flight (it would be newer than the target); with history forked the
    // deletion of the target judges A against the newer snapshot (which
    // holds B), releases A although the live map names it, the segment
    // comes free and is discarded, and the live volume reads `Corrupt`.
    assert!(
        !accepted || newer.is_empty(),
        "the rollback was accepted with snapshot(s) {newer:?} newer than its target"
    );
    assert!(drift.is_empty(), "live-block drift: {drift:?}");
    assert_eq!(after.ok(), Some(live), "the live volume keeps its data");
}

/// A volume deletion asked for while a CP in flight is taking a snapshot of
/// the volume is accepted, and the snapshot outlives its volume: a
/// snapshot of a volume that is not present, which a volume created later
/// at the same index inherits.
#[test]
fn a_volume_deletion_is_refused_while_a_snapshot_of_it_is_in_flight() {
    let mut s = booted(quiet_cfg(7003, &[(0, 64), (1, 64)]));
    ok(&mut s, 1, 0, 8);
    s.request_cp().unwrap();
    s.quiesce(1).unwrap();
    s.disk().set_schedule(fifo1());
    s.engine()
        .create_snapshot(1, uuid(7003), SnapKind::Crash, SnapOrigin::User)
        .unwrap();
    s.tick_until(
        |e| {
            matches!(
                e.phase(),
                Phase::Metadata | Phase::Flushing | Phase::CommitA
            )
        },
        "the snapshot's CP past its metadata preparation",
    )
    .unwrap();
    let accepted = s.engine().delete_volume(1).is_ok();
    s.disk().set_schedule(Schedule::default());
    s.tick_until(
        |e| e.volume(1).is_some_and(|v| v.op_pending.is_none()) && e.is_quiet(),
        "the snapshot and the deletion",
    )
    .unwrap();
    let orphans: Vec<(u64, u32)> = {
        let e = s.engine();
        e.snapshots()
            .iter()
            .filter(|x| !e.volume(x.vol).is_some_and(|v| v.present))
            .map(|x| (x.snap_id, x.vol))
            .collect()
    };
    // Review defect D4 (Step 5, `08-engineering-log.md`), fixed; the review's
    // expectation stood and the scenario is kept. As reviewed: the deletion must be refused while a snapshot of the volume is
    // being taken (`delete_volume` refuses a volume with snapshots or
    // snapshot requests, but not one whose request a CP in flight holds);
    // instead the volume goes and its snapshot stays, naming an index a new
    // volume can take.
    assert!(
        orphans.is_empty(),
        "deletion accepted: {accepted}; snapshots of absent volumes: {orphans:?}"
    );
}

// ---------------------------------------------------------------------------
// Availability: snapshot deletion after a shrink, and requests that race a
// cleaning CP
// ---------------------------------------------------------------------------

/// A snapshot that maps a block beyond the volume's current end (the volume
/// was shrunk after the snapshot) cannot be deleted: the deletion compares
/// each of its entries against the live map, whose lookup refuses an LBA
/// beyond the end, and that refusal fails the CP; the deletion stays
/// pending, so every later CP fails the same way.
#[test]
fn deleting_a_snapshot_after_its_volume_shrank_commits() {
    let mut s = booted(quiet_cfg(7004, &[(0, 64)]));
    ok(&mut s, 0, 0, 4);
    ok(&mut s, 0, 40, 4);
    let snap = s.snapshot(0).unwrap();
    s.resize_volume(0, 32).unwrap();
    s.set_online(0, true).unwrap();
    // Review defect W1 (Step 5, `08-engineering-log.md`), fixed; the review's
    // expectation stood and the scenario is kept. As reviewed: the deletion must commit (the blocks beyond the end are the
    // snapshot's alone and are released); instead `prepare_meta_inner`
    // returns the map's `OutOfRange` for LBA 40, the tick fails, and every
    // CP after it fails the same way while the deletion waits.
    let deleted = s.delete_snapshot(snap);
    assert!(
        deleted.is_ok(),
        "the deletion fails: {deleted:?} ({:?})",
        complaints(&s)
    );
    s.request_cp().unwrap();
    ok(&mut s, 0, 0, 1);
    s.audit_liveness().unwrap();
}

/// The same with the comparison against the next newer snapshot, taken
/// after the shrink and so shorter than the one deleted.
#[test]
fn deleting_a_snapshot_longer_than_its_newer_neighbour_commits() {
    let mut s = booted(quiet_cfg(7005, &[(0, 64)]));
    ok(&mut s, 0, 40, 4);
    let s1 = s.snapshot(0).unwrap();
    s.resize_volume(0, 32).unwrap();
    s.set_online(0, true).unwrap();
    ok(&mut s, 0, 0, 2);
    let _s2 = s.snapshot(0).unwrap();
    // Review defect W1 (Step 5, `08-engineering-log.md`), fixed; the review's
    // expectation stood and the scenario is kept. As reviewed: as above, through `pinned_get` on the newer (shorter) snapshot.
    let deleted = s.delete_snapshot(s1);
    assert!(
        deleted.is_ok(),
        "the deletion fails: {deleted:?} ({:?})",
        complaints(&s)
    );
    s.request_cp().unwrap();
    s.audit_liveness().unwrap();
}

/// A snapshot deletion asked for while a cleaning CP stages relocations of
/// that snapshot's blocks reaches the CP's metadata preparation, which
/// rewrites the relocated blocks into a snapshot it has just removed.
#[test]
fn a_snapshot_deletion_asked_while_a_cleaning_cp_stages_lands_in_the_next_cp() {
    let mut s = booted(quiet_cfg(7006, &[(0, 400)]));
    fragment(&mut s, 0, 400);
    let s1 = s.snapshot(0).unwrap();
    for lba in (0..400).step_by(32) {
        ok(&mut s, 0, lba, 8);
    }
    s.request_cp().unwrap();
    into_cleaning_staging(&mut s);
    s.request_snapshot_deletion(s1).unwrap();
    s.disk().set_schedule(Schedule::default());
    // Review defect W3 (Step 5, `08-engineering-log.md`), fixed; the review's
    // expectation stood and the scenario is kept. As reviewed: the deletion and the clean must both land (or the deletion wait
    // for the next CP); instead `prepare_meta_inner` returns
    // `Internal("relocation for snapshot …, which is gone")` and the tick
    // fails (the CP's snapshot requests, if any, are dropped unanswered).
    let done = s.tick_until(
        |e| e.snapshots().is_empty() && e.is_quiet(),
        "the deletion and the clean",
    );
    assert!(done.is_ok(), "{done:?}");
    s.audit_liveness().unwrap();
    s.read(0, 0, 400).unwrap();
}

/// A volume deletion asked for while a cleaning CP stages relocations of
/// the volume's live blocks (the volume is offline and quiet, so the
/// deletion is accepted) reaches a CP that carries both.
#[test]
fn a_volume_deletion_asked_while_a_cleaning_cp_stages_lands_in_the_next_cp() {
    let mut s = booted(quiet_cfg(7007, &[(0, 400), (1, 64)]));
    fragment(&mut s, 0, 400);
    ok(&mut s, 1, 0, 4);
    s.quiesce(0).unwrap();
    into_cleaning_staging(&mut s);
    let accepted = s.engine().delete_volume(0);
    s.disk().set_schedule(Schedule::default());
    // Review defect W3 (Step 5, `08-engineering-log.md`), fixed; the review's
    // expectation stood and the scenario is kept. As reviewed: the deletion must be refused while the CP relocates the volume's
    // blocks, or wait for the next CP; instead it is accepted and the CP's
    // metadata preparation fails with `Internal("volume 0 has deltas in the
    // CP that carries its delete")`, failing the tick.
    let done = s.tick_until(
        |e| e.is_quiet() && e.volume(0).is_some_and(|v| v.op_pending.is_none()),
        "the clean and the deletion",
    );
    assert!(done.is_ok(), "deletion accepted: {accepted:?}; {done:?}");
    if accepted.is_ok() {
        s.model.remove_volume(0);
        s.cfg.volumes.retain(|(v, _)| *v != 0);
    }
    s.audit_liveness().unwrap();
    s.read(1, 0, 4).unwrap();
}

// ---------------------------------------------------------------------------
// Durability: damaged metadata the cleaner and the boot must contain
// ---------------------------------------------------------------------------

/// A stripe summary that no longer verifies (a media error on the summary
/// page, the data blocks intact) makes the cleaner skip that stripe's
/// blocks as if nothing referenced them; the segment is drained anyway,
/// counted empty and freed, and the live blocks of the stripe are lost.
#[test]
fn a_damaged_stripe_summary_makes_the_cleaner_skip_the_segment() {
    let mut s = booted(quiet_cfg(7008, &[(0, 400)]));
    fragment(&mut s, 0, 400);
    s.read(0, 0, 400).unwrap();
    let e = s.engine().map_entry(0, 0).expect("LBA 0 is mapped");
    let layout = *s.engine().layout();
    let idx = layout.stripe_of_off(e.off());
    let pba = layout.stripe_pba(e.lseg(), idx).unwrap();
    // One byte of the summary's header, not of any data block.
    let at = pba as usize * BLOCK_SIZE + 64;
    let flipped = s.disk().durable()[at] ^ 0xff;
    s.disk().corrupt_durable(at, &[flipped]);
    assert_eq!(
        s.read(0, 0, 400).unwrap().0,
        Status::Ok,
        "the data still reads"
    );
    s.request_clean().unwrap();
    s.request_clean().unwrap();
    for _ in 0..3 {
        s.request_cp().unwrap();
    }
    // Review defect D3 (Step 5, `08-engineering-log.md`), fixed; the review's
    // expectation stood and the scenario is kept. As reviewed: a summary that does not parse proves nothing about the blocks
    // behind it, so the segment must be skipped (`SkipReason::Summary`) or
    // its blocks found through the map; `cleaner::scan` `continue`s past
    // the stripe instead, the CP sets the segment's count to zero (with an
    // `Internal` drift event) and the deferred free discards live data.
    let (status, _) = s.read(0, 0, 400).unwrap();
    assert_eq!(
        status,
        Status::Ok,
        "the live blocks behind the damaged summary ({:?})",
        complaints(&s)
    );
    s.audit_liveness().unwrap();
}

/// The catalogue region is carried forward unchanged by every commit that
/// changes no name, so every candidate of the ring names the same region;
/// one damaged byte in it tears every candidate and the pool no longer
/// boots, although the volumes, the map and the data are intact.
#[test]
fn a_damaged_catalogue_drops_the_names_and_the_pool_boots() {
    let mut s = booted(quiet_cfg(7009, &[(0, 64)]));
    s.engine().set_volume_name(0, "boot").unwrap();
    s.request_cp().unwrap();
    ok(&mut s, 0, 0, 4);
    s.request_cp().unwrap();
    let layout = *s.engine().layout();
    s.shutdown_clean().unwrap();
    let mut regions = HashSet::new();
    for slot in SbSlot::ALL {
        let at = layout.sb_pba(slot) as usize * BLOCK_SIZE;
        let bytes = s.disk().durable()[at..at + SUPERBLOCK_SLOT_SIZE].to_vec();
        if let Ok(sl) = SuperblockSlot::read(&bytes) {
            if !sl.sb.catalog.is_none() {
                regions.insert(sl.sb.catalog.pba);
            }
        }
    }
    assert_eq!(
        regions.len(),
        1,
        "every slot names the one catalogue region"
    );
    for pba in regions {
        // A byte of the volume's name.
        let at = pba as usize * BLOCK_SIZE + 33;
        let flipped = s.disk().durable()[at] ^ 0x01;
        s.disk().corrupt_durable(at, &[flipped]);
    }
    // Review defect W4 (Step 5, `08-engineering-log.md`), fixed; the review's
    // expectation stood and the scenario is kept. As reviewed: a name is not worth the pool: the boot must go on (the name
    // reported lost, or rebuilt), as it does for a damaged snapshot root;
    // instead `walk_cp` turns the catalogue's CRC failure into a torn
    // candidate, every candidate names the same region, and the boot fails
    // with no verifiable CP.
    let boot = s.boot();
    assert!(boot.is_ok(), "{boot:?}");
    s.verify_recovery().unwrap();
}

/// The ring's B slots at the end of the device are whole commits: with
/// both A slots wiped the engine boots from them, clean, with every block
/// (the daemon's probe is another matter; see the daemon's QA suite).
#[test]
fn the_engine_boots_from_the_b_slots_when_both_a_slots_are_wiped() {
    let mut s = booted(quiet_cfg(7011, &[(0, 64)]));
    ok(&mut s, 0, 0, 16);
    s.engine().set_volume_name(0, "kept").unwrap();
    s.request_cp().unwrap();
    let layout = *s.engine().layout();
    s.shutdown_clean().unwrap();
    for slot in [SbSlot::A0, SbSlot::A1] {
        let at = layout.sb_pba(slot) as usize * BLOCK_SIZE;
        s.disk()
            .corrupt_durable(at, &vec![0u8; SUPERBLOCK_SLOT_SIZE]);
    }
    let info = s.boot().unwrap();
    assert!(!info.dirty, "{info:?}");
    s.verify_recovery().unwrap();
    assert_eq!(s.engine().volume_name(0), Some("kept"));
    s.read(0, 0, 16).unwrap();
}

// ---------------------------------------------------------------------------
// Availability: the reserve and the cleaner's metadata
// ---------------------------------------------------------------------------

/// Many snapshots of a quiet volume share one tree. The cleaner rewrites
/// every snapshot that references a relocated block in its own
/// copy-on-write pages, index and root page, so one cleaning CP needs
/// metadata in proportion to the snapshots, while its plan reserves one
/// segment for metadata. On a pool past the background threshold the CPs
/// that carry it fail for space: the pool is marked full and every CP that
/// carries the clients' writes fails the same way.
#[test]
fn cleaning_blocks_that_many_snapshots_share_fits_the_plan_and_keeps_the_reserve() {
    let mut cfg = SimConfig::small(7010);
    cfg.volumes = vec![(0, 400), (1, 2048)];
    cfg.fold.max_cps = 4;
    let mut s = booted(cfg);
    fragment(&mut s, 0, 400);
    // 160 snapshots of volume 0, five folds of 32 requests: one tree.
    let mut n = 0u64;
    for batch in 1..=5usize {
        for _ in 0..32 {
            n += 1;
            s.engine()
                .create_snapshot(0, uuid(10_000 + n), SnapKind::Crash, SnapOrigin::Scheduled)
                .unwrap();
        }
        s.request_cp().unwrap();
        s.tick_until(
            |e| e.snapshots().len() == batch * 32,
            "a batch of snapshots",
        )
        .unwrap();
    }
    // The v1 blocks left in the first segments become the snapshots' alone.
    for lba in (0..400).step_by(32) {
        ok(&mut s, 0, lba, 8);
    }
    s.request_cp().unwrap();
    // Fill volume 1 until the admission rule refuses, with a CP every
    // 64 blocks.
    let mut lba = 0u64;
    let mut refused = 0;
    while lba + 8 <= 2048 && refused < 2 {
        let c = s.write(1, lba, 8).unwrap();
        if c.status == Status::NoSpace {
            refused += 1;
            let _ = s.request_cp();
            continue;
        }
        assert_eq!(c.status, Status::Ok);
        lba += 8;
        if lba.is_multiple_of(64) {
            let _ = s.request_cp();
        }
    }
    let failed: Vec<String> = s
        .events
        .iter()
        .filter_map(|e| match e {
            Event::CpFailed { cp_seq, reason } => Some(format!("{cp_seq}: {reason}")),
            _ => None,
        })
        .collect();
    let status = s.engine().pool_status();
    // Review defect W2 (Step 5, `08-engineering-log.md`), fixed; the review's
    // expectation stood and the scenario is kept. As reviewed: S5-3 keeps a reserve so that front-door writes are refused by
    // the admission rule before any CP runs out of space; instead the
    // background cleaner's CPs (planned with one segment for metadata)
    // need a page, an index and a root page per snapshot, fail for space,
    // and mark the pool full while the admission rule still admits writes.
    assert!(
        failed.is_empty() && !status.pool_full,
        "{} CPs failed for space ({:?}…); pool_full {}, free {} of {}, reserve {}, admissible {} blocks",
        failed.len(),
        failed.first(),
        status.pool_full,
        status.free_segments,
        status.segments,
        status.reserve_segments,
        status.admissible_blocks
    );
}

// ---------------------------------------------------------------------------
// Pins, deletions and operations: behaviour that is right
// ---------------------------------------------------------------------------

/// A pin change still waiting for its CP counts: a snapshot about to become
/// the replication base refuses deletion at once, and one about to lose
/// the base may be deleted in the CP that moves it; the snapshot being
/// deleted refuses new pins.
#[test]
fn pending_pin_changes_govern_deletion() {
    let mut s = booted(quiet_cfg(7020, &[(0, 64)]));
    ok(&mut s, 0, 0, 4);
    let s1 = s.snapshot(0).unwrap();
    ok(&mut s, 0, 0, 4);
    let s2 = s.snapshot(0).unwrap();
    s.engine().set_replication_base(0, Some(s1)).unwrap();
    assert!(matches!(
        s.engine().delete_snapshot(s1),
        Err(EngineError::SnapshotPinned {
            flags: PIN_REPLICATION,
            ..
        })
    ));
    s.request_cp().unwrap();
    // Move the base to S2 and delete S1 before the CP that moves it.
    s.engine().set_replication_base(0, Some(s2)).unwrap();
    s.request_snapshot_deletion(s1).unwrap();
    assert!(matches!(
        s.engine().set_snapshot_pins(s1, PIN_REPLICATION),
        Err(EngineError::NoSuchSnapshot { .. })
    ));
    assert!(matches!(
        s.engine().delete_snapshot(s2),
        Err(EngineError::SnapshotPinned { .. })
    ));
    s.tick_until(
        |e| e.snapshots().len() == 1 && e.is_quiet(),
        "the move and the deletion",
    )
    .unwrap();
    let left = s.engine().snapshots();
    assert_eq!(left[0].snap_id, s2);
    assert_eq!(left[0].flags, PIN_REPLICATION);
    s.crash_daemon().unwrap();
    s.boot().unwrap();
    s.verify_recovery().unwrap();
    assert_eq!(s.engine().snapshots()[0].flags, PIN_REPLICATION);
    assert!(s.engine().delete_snapshot(s2).is_err());
}

/// A rollback refuses a target with a deletion waiting, a deletion refuses
/// a snapshot of a volume with a rollback waiting, and a rollback refuses
/// while a snapshot request of the volume waits.
#[test]
fn rollbacks_and_deletions_refuse_each_other_while_waiting() {
    let mut s = booted(quiet_cfg(7021, &[(0, 64)]));
    ok(&mut s, 0, 0, 4);
    let s1 = s.snapshot(0).unwrap();
    ok(&mut s, 0, 0, 4);
    let s2 = s.snapshot(0).unwrap();
    ok(&mut s, 0, 8, 4);
    s.quiesce(0).unwrap();
    s.engine().delete_snapshot(s2).unwrap();
    assert!(matches!(
        s.engine().rollback_volume(0, s2),
        Err(EngineError::NoSuchSnapshot { .. })
    ));
    // The newer one is going, so the older is a valid target; then nothing
    // else of the volume may be deleted or snapshotted until it lands.
    s.engine().rollback_volume(0, s1).unwrap();
    assert!(matches!(
        s.engine().delete_snapshot(s1),
        Err(EngineError::VolumeBusy { vol: 0 })
    ));
    assert!(matches!(
        s.engine()
            .create_snapshot(0, uuid(7021), SnapKind::Crash, SnapOrigin::User),
        Err(EngineError::VolumeBusy { vol: 0 })
    ));
    assert!(matches!(
        s.engine().set_volume_online(0, true),
        Err(EngineError::VolumeBusy { vol: 0 })
    ));
    s.tick_until(
        |e| e.volume(0).is_some_and(|v| v.op_pending.is_none()) && e.is_quiet(),
        "the deletion and the rollback",
    )
    .unwrap();
    // The harness did not issue the rollback; take the snapshot's image.
    let want = snapshot_image(&mut s, s1, 0, 64);
    assert_eq!(peek(&mut s, 0, 0, 64).unwrap(), want);
    assert!(s.engine().audit_liveness().unwrap().is_empty());
}

/// Deleting a run of snapshots in one CP, in any order of requests,
/// releases each version exactly once: the accounting is exact after the
/// CP and after a crash, and the survivors read as they did.
#[test]
fn several_deletions_in_one_cp_release_each_version_once() {
    for order in [[0usize, 1, 2], [2, 1, 0], [1, 0, 2], [2, 0, 1]] {
        let mut s = booted(quiet_cfg(7022, &[(0, 64)]));
        let mut ids = Vec::new();
        for v in 0..5u64 {
            ok(&mut s, 0, v * 2, 6); // overlapping versions
            ids.push(s.snapshot(0).unwrap());
        }
        ok(&mut s, 0, 0, 16);
        s.request_cp().unwrap();
        // Delete the middle three in one CP.
        let doomed = [ids[1], ids[2], ids[3]];
        for k in order {
            s.request_snapshot_deletion(doomed[k]).unwrap();
        }
        s.engine().request_cp();
        s.tick_until(
            |e| e.snapshots().len() == 2 && e.is_quiet(),
            "the three deletions",
        )
        .unwrap();
        s.audit_liveness().unwrap();
        s.verify_snapshots().unwrap();
        s.crash_power().unwrap();
        s.boot().unwrap();
        s.verify_recovery()
            .unwrap_or_else(|e| panic!("order {order:?}: {e}"));
    }
}

/// A relocation never outruns a write that lands while the cleaning CP
/// stages: overwrites and punches of the very blocks being relocated,
/// submitted between the ticks of the cleaning CP, win, before and after a
/// crash of each kind.
#[test]
fn writes_and_punches_during_a_cleaning_cp_win_over_the_relocation() {
    for (round, crash) in [0u8, 1, 2].into_iter().enumerate() {
        let mut s = booted(quiet_cfg(7023 + round as u64, &[(0, 400)]));
        fragment(&mut s, 0, 400);
        into_cleaning_staging(&mut s);
        // The live v1 chunks are at lba % 32 < 8: hit some of them now.
        let mut ids = Vec::new();
        for lba in (0..400).step_by(64) {
            ids.push(s.submit_write(0, lba + 1, 2));
        }
        ids.push(s.submit_punch(0, 32, 3, false));
        s.disk().set_schedule(Schedule::default());
        for id in ids {
            let c = s.wait(id, TICK_BUDGET).unwrap().expect("completes");
            assert_eq!(c.status, Status::Ok);
        }
        s.settle().unwrap();
        s.request_cp().unwrap();
        assert!(s.engine().stats().relocated > 0);
        s.read(0, 0, 400).unwrap();
        s.audit_liveness().unwrap();
        match crash {
            0 => {
                s.crash_power().unwrap();
            }
            1 => s.crash_daemon().unwrap(),
            _ => s.shutdown_clean().unwrap(),
        }
        s.boot().unwrap();
        s.verify_recovery().unwrap();
        s.clear_nvfail().unwrap();
        s.read(0, 0, 400).unwrap();
    }
}

/// A power cut during each phase of a CP that cleans and deletes the last
/// snapshot that pinned the relocated blocks (the deletion asked for
/// before the CP was planned, so the plan leaves the cleaning out) keeps
/// everything consistent.
#[test]
fn a_power_cut_in_each_phase_of_a_deleting_cp_after_a_clean_is_consistent() {
    let mut seed = 7030;
    for phase in [
        Phase::Staging,
        Phase::Metadata,
        Phase::Flushing,
        Phase::CommitA,
        Phase::CommitB,
    ] {
        for tear in [DROP, TORN, TearPolicy::default()] {
            seed += 1;
            let mut s = booted(quiet_cfg(seed, &[(0, 400)]));
            fragment(&mut s, 0, 400);
            let s1 = s.snapshot(0).unwrap();
            for lba in (0..400).step_by(32) {
                ok(&mut s, 0, lba, 8);
            }
            s.request_cp().unwrap();
            s.request_clean().unwrap();
            assert!(
                s.engine().stats().relocated > 0,
                "the clean moved S1's blocks"
            );
            ok(&mut s, 0, 100, 4); // something to stage
            s.request_snapshot_deletion(s1).unwrap();
            s.disk().set_schedule(fifo1());
            s.engine().request_cp();
            let _ = s.tick_until(|e| e.phase() == phase, "the phase");
            s.disk().set_tear_policy(tear);
            s.crash_power().unwrap();
            s.disk().set_schedule(Schedule::default());
            s.boot().unwrap();
            s.verify_recovery()
                .unwrap_or_else(|e| panic!("seed {seed} {phase:?}: {e}"));
            s.clear_nvfail().unwrap();
            if s.engine().snapshots().iter().any(|x| x.snap_id == s1) {
                s.delete_snapshot(s1).unwrap();
            }
            s.request_clean().unwrap();
            for _ in 0..2 {
                s.request_cp().unwrap();
            }
            s.audit_liveness().unwrap();
            s.read(0, 0, 400).unwrap();
        }
    }
}

// ---------------------------------------------------------------------------
// Property (a): GC + crash invariance
// ---------------------------------------------------------------------------

/// `QA_TRACE=1` prints what each cut interrupted, to see what a run
/// covered.
fn trace() -> bool {
    std::env::var_os("QA_TRACE").is_some()
}

/// The case count: `PROPTEST_CASES` when set (the merge gate), else
/// `default`.
fn cases(default: u32) -> u32 {
    std::env::var("PROPTEST_CASES")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(default)
}

#[derive(Debug, Clone)]
enum GcOp {
    /// `n` small overwrites of one volume, from a seeded generator.
    Burst { vol: u8, seed: u16, n: u8 },
    /// A discard.
    Punch { vol: u8, lba: u16, blocks: u8 },
    /// A snapshot (the harness checks its image after every recovery).
    Snapshot { vol: u8 },
    /// A snapshot deletion that completes.
    Delete { which: u8 },
    /// A cleaning CP that completes.
    Clean,
    /// A clean asked for, a slow device ticked until the cleaning CP is in
    /// phase `phase` (and `extra` ticks more), writes put in flight, then a
    /// stop of kind `kind`.
    CleanCrash {
        phase: u8,
        extra: u8,
        inflight: u8,
        kind: u8,
    },
    /// A snapshot deletion asked for (with a write to stage), likewise cut.
    DeleteCrash {
        which: u8,
        phase: u8,
        extra: u8,
        inflight: u8,
        kind: u8,
    },
    /// Every check the harness offers.
    Check,
}

fn gc_op() -> impl Strategy<Value = GcOp> {
    prop_oneof![
        8 => (any::<u8>(), any::<u16>(), 8..=48u8).prop_map(|(vol, seed, n)| GcOp::Burst { vol, seed, n }),
        2 => (any::<u8>(), any::<u16>(), 1..=32u8).prop_map(|(vol, lba, blocks)| GcOp::Punch { vol, lba, blocks }),
        3 => any::<u8>().prop_map(|vol| GcOp::Snapshot { vol }),
        2 => any::<u8>().prop_map(|which| GcOp::Delete { which }),
        2 => Just(GcOp::Clean),
        4 => (0..5u8, 0..3u8, 0..4u8, any::<u8>()).prop_map(|(phase, extra, inflight, kind)| GcOp::CleanCrash { phase, extra, inflight, kind }),
        4 => (any::<u8>(), 0..5u8, 0..3u8, 0..4u8, any::<u8>()).prop_map(|(which, phase, extra, inflight, kind)| GcOp::DeleteCrash { which, phase, extra, inflight, kind }),
        1 => Just(GcOp::Check),
    ]
}

const CUT_PHASES: [Phase; 5] = [
    Phase::Staging,
    Phase::Metadata,
    Phase::Flushing,
    Phase::CommitA,
    Phase::CommitB,
];

/// Under a slow device, ticks the CP just asked for into `CUT_PHASES[phase]`
/// (or until it is over), then `extra` ticks more.
fn tick_into(s: &mut Sim, phase: u8, extra: u8) -> Result<(), String> {
    s.disk().set_schedule(fifo1());
    s.tick()?;
    let target = CUT_PHASES[usize::from(phase) % CUT_PHASES.len()];
    s.tick_until(
        |e| e.phase() == target || e.phase() == Phase::Idle,
        "the phase to cut",
    )?;
    s.ticks(u32::from(extra))
}

const GC_SIZES: [u64; 2] = [240, 160];

fn gc_cfg(seed: u64) -> SimConfig {
    let mut cfg = SimConfig::small(seed);
    cfg.segments = 40;
    cfg.volumes = vec![(0, GC_SIZES[0]), (1, GC_SIZES[1])];
    cfg.fold.max_cps = 4;
    cfg.cleaner = CleanerPolicy {
        utilization: 0.5,
        max_segments_per_cp: 2,
        background: true,
        margin_segments: 4,
        max_live_fraction: 0.8,
    };
    cfg
}

/// Stops the engine as `kind` says (a power cut under one of the tear
/// policies, a daemon kill, or a clean shutdown), boots it and checks the
/// recovery: the image is a prefix the stop allows, nvfail holds what it
/// must, every snapshot reads as taken, and the live-block accounting
/// matches a walk of every tree (`verify_recovery` audits it).
fn stop_and_recover(s: &mut Sim, kind: u8) -> Result<(), String> {
    match kind % 3 {
        0 => {
            s.disk().set_tear_policy(tear_of(kind / 3));
            s.crash_power()?;
        }
        1 => s.crash_daemon()?,
        _ => {
            s.disk().set_schedule(Schedule::default());
            s.shutdown_clean()?;
        }
    }
    s.disk().set_schedule(Schedule::default());
    s.boot()?;
    let v = s.verify_recovery()?;
    if s.last_stop() == Stop::Clean && v.boot.dirty {
        return Err("a clean stop booted dirty".into());
    }
    s.clear_nvfail()
}

/// Submits up to `n` small writes without waiting for them (they are in
/// flight when the stop comes).
fn submit_inflight(s: &mut Sim, rng: &mut SplitMix64, n: u8) {
    for _ in 0..n {
        let vol = rng.below(2) as u32;
        let size = GC_SIZES[vol as usize];
        let lba = rng.below(size - 2);
        let held = s.engine().nvfail_set().get(vol);
        if !held {
            s.submit_write(vol, lba, 2);
        }
    }
}

/// The uuids of the snapshots whose deletion committed: none may come
/// back.
fn note_deleted(s: &Sim, from: usize, uuids: &HashMap<u64, Uuid>, gone: &mut HashSet<Uuid>) {
    for e in &s.events[from..] {
        if let Event::SnapshotDeleted { snap_id, .. } = e {
            if let Some(u) = uuids.get(snap_id) {
                gone.insert(*u);
            }
        }
    }
}

fn gc_walk(seed: u64, ops: &[GcOp]) -> Result<(), String> {
    let mut s = Sim::new(gc_cfg(seed))?;
    s.boot()?;
    // Every block written once: the overwrites that follow leave segments
    // partly dead, which is what the cleaner compacts.
    for (vol, size) in [(0u32, GC_SIZES[0]), (1, GC_SIZES[1])] {
        for lba in (0..size).step_by(8) {
            let c = s.write(vol, lba, 8)?;
            if c.status != Status::Ok {
                return Err(format!("seed {seed}: the fill: {:?}", c.status));
            }
        }
    }
    s.request_cp()?;
    let mut rng = SplitMix64::new(seed ^ 0x5eed);
    let mut uuids: HashMap<u64, Uuid> = HashMap::new();
    let mut gone: HashSet<Uuid> = HashSet::new();
    let mut seen = 0usize;
    for (i, op) in ops.iter().enumerate() {
        let step = |e: String| format!("seed {seed} op {i} {op:?}: {e}");
        // Requests are made with the engine idle (a deletion or a volume
        // operation asked for while a cleaning CP stages is a separate,
        // known defect; see the `bug_*` tests above).
        s.settle().map_err(step)?;
        for x in s.engine().snapshots() {
            uuids.insert(x.snap_id, x.uuid);
        }
        match *op {
            GcOp::Burst { vol, seed: b, n } => {
                let vol = u32::from(vol % 2);
                let size = GC_SIZES[vol as usize];
                let mut g = SplitMix64::new(u64::from(b));
                for _ in 0..n {
                    let blocks = g.range(1, 4) as u32;
                    let lba = g.below(size - u64::from(blocks));
                    let c = s.write(vol, lba, blocks).map_err(step)?;
                    if !matches!(c.status, Status::Ok | Status::NoSpace | Status::NvFailed) {
                        return Err(step(format!("write status {:?}", c.status)));
                    }
                }
            }
            GcOp::Punch { vol, lba, blocks } => {
                let vol = u32::from(vol % 2);
                let size = GC_SIZES[vol as usize];
                let blocks = u64::from(blocks).min(size);
                let lba = u64::from(lba) % (size - blocks + 1);
                let c = s.punch(vol, lba, blocks as u32, false).map_err(step)?;
                if !matches!(c.status, Status::Ok | Status::NoSpace | Status::NvFailed) {
                    return Err(step(format!("punch status {:?}", c.status)));
                }
            }
            GcOp::Snapshot { vol } => {
                let vol = u32::from(vol % 2);
                if !s.engine().nvfail_set().get(vol) && s.snaps.len() < 6 {
                    s.snapshot(vol).map_err(step)?;
                }
            }
            GcOp::Delete { which } => {
                if !s.snaps.is_empty() {
                    let id = s.snaps[usize::from(which) % s.snaps.len()].snap_id;
                    s.delete_snapshot(id).map_err(step)?;
                }
            }
            GcOp::Clean => {
                s.request_clean().map_err(step)?;
            }
            GcOp::CleanCrash {
                phase,
                extra,
                inflight,
                kind,
            } => {
                let before = s.engine().stats().reloc_stripes;
                s.engine().request_clean();
                tick_into(&mut s, phase, extra).map_err(step)?;
                submit_inflight(&mut s, &mut rng, inflight);
                if trace() {
                    eprintln!(
                        "qa-trace clean-crash kind {} phase {:?} relocating {}",
                        kind % 3,
                        s.engine().phase(),
                        s.engine().stats().reloc_stripes > before
                    );
                }
                stop_and_recover(&mut s, kind).map_err(step)?;
            }
            GcOp::DeleteCrash {
                which,
                phase,
                extra,
                inflight,
                kind,
            } => {
                // Something for the deleting CP to stage, so every phase
                // exists.
                let vol = u32::from(which % 2);
                if !s.engine().nvfail_set().get(vol) {
                    let lba = rng.below(GC_SIZES[vol as usize] - 4);
                    let c = s.write(vol, lba, 4).map_err(step)?;
                    if !matches!(c.status, Status::Ok | Status::NoSpace) {
                        return Err(step(format!("write status {:?}", c.status)));
                    }
                }
                let deleting = !s.snaps.is_empty();
                if deleting {
                    let id = s.snaps[usize::from(which) % s.snaps.len()].snap_id;
                    s.request_snapshot_deletion(id).map_err(step)?;
                }
                s.engine().request_cp();
                tick_into(&mut s, phase, extra).map_err(step)?;
                submit_inflight(&mut s, &mut rng, inflight);
                if trace() {
                    eprintln!(
                        "qa-trace delete-crash kind {} phase {:?} deleting {deleting}",
                        kind % 3,
                        s.engine().phase()
                    );
                }
                stop_and_recover(&mut s, kind).map_err(step)?;
            }
            GcOp::Check => {
                s.check_disk_prefix().map_err(step)?;
                s.verify_snapshots().map_err(step)?;
                s.audit_liveness().map_err(step)?;
            }
        }
        // Nothing deleted comes back.
        note_deleted(&s, seen, &uuids, &mut gone);
        seen = s.events.len();
        let back: Vec<u64> = s
            .engine()
            .snapshots()
            .iter()
            .filter(|x| gone.contains(&x.uuid))
            .map(|x| x.snap_id)
            .collect();
        if !back.is_empty() {
            return Err(step(format!("deleted snapshots came back: {back:?}")));
        }
        // No CP ever fails for space on this pool.
        if let Some(Event::CpFailed { reason, .. }) = s
            .events
            .iter()
            .find(|e| matches!(e, Event::CpFailed { reason, .. } if reason.contains("segment")))
        {
            return Err(step(format!("a CP failed for space: {reason}")));
        }
    }
    s.settle()?;
    s.check_disk_prefix()?;
    s.verify_snapshots()?;
    s.audit_liveness()?;
    s.shutdown_clean()?;
    s.boot()?;
    let v = s.verify_recovery()?;
    if v.k != s.model.last_op() {
        return Err(format!(
            "seed {seed}: the final clean restart kept {} of {} operations",
            v.k,
            s.model.last_op()
        ));
    }
    Ok(())
}

proptest! {
    #![proptest_config(ProptestConfig {
        cases: cases(24),
        failure_persistence: None,
        max_shrink_iters: 256,
        ..ProptestConfig::default()
    })]

    /// The brief's first property: power cuts (every tear policy), daemon
    /// kills and clean stops in any phase of a cleaning CP or of a
    /// snapshot-deleting CP, with writes in flight, never lose live data
    /// (the image is a prefix the stop allows), never bring deleted data
    /// back (overwritten and punched versions stay gone, deleted snapshots
    /// stay deleted), and never leave the live-block accounting off after
    /// a reboot (audited against a walk of every tree).
    #[test]
    fn gc_and_crash_invariance(
        seed in any::<u64>(),
        ops in prop::collection::vec(gc_op(), 6..28),
    ) {
        if let Err(e) = gc_walk(seed, &ops) {
            prop_assert!(false, "{e}");
        }
    }
}

// ---------------------------------------------------------------------------
// Property (b): a pool driven to 100% of its user capacity recovers
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
enum FullOp {
    DeleteSnapshot { which: u8 },
    Punch { lba: u16, blocks: u16 },
    Clean,
    Write { lba: u16 },
    Stop { kind: u8 },
    Snapshot,
}

fn full_op() -> impl Strategy<Value = FullOp> {
    prop_oneof![
        3 => any::<u8>().prop_map(|which| FullOp::DeleteSnapshot { which }),
        3 => (any::<u16>(), 1..=512u16).prop_map(|(lba, blocks)| FullOp::Punch { lba, blocks }),
        3 => Just(FullOp::Clean),
        3 => any::<u16>().prop_map(|lba| FullOp::Write { lba }),
        2 => any::<u8>().prop_map(|kind| FullOp::Stop { kind }),
        1 => Just(FullOp::Snapshot),
    ]
}

const FULL_SIZE: u64 = 2048;

/// No CP may have failed for space, and the pool may not be marked full:
/// S5-3's reserve keeps both for the internal CPs.
fn never_full(s: &mut Sim) -> Result<(), String> {
    if let Some(Event::CpFailed { cp_seq, reason }) = s
        .events
        .iter()
        .find(|e| matches!(e, Event::CpFailed { reason, .. } if reason.contains("segment")))
    {
        return Err(format!("CP {cp_seq} failed for space: {reason}"));
    }
    if s.engine().is_pool_full() {
        return Err(format!("the pool is full: {:?}", s.engine().pool_status()));
    }
    Ok(())
}

fn full_walk(seed: u64, snaps: u8, plan: &[FullOp]) -> Result<(), String> {
    let mut cfg = SimConfig::small(seed);
    cfg.segments = 36;
    cfg.volumes = vec![(0, FULL_SIZE)];
    cfg.fold.max_cps = 4;
    let mut s = Sim::new(cfg)?;
    s.boot()?;
    let mut rng = SplitMix64::new(seed);
    // Fill with fresh blocks, snapshots sprinkled in, until the admission
    // rule refuses even right after a CP.
    let snap_every = if snaps == 0 {
        u64::MAX
    } else {
        FULL_SIZE / 8 / (u64::from(snaps) + 1)
    };
    let mut lba = 0u64;
    let mut chunk = 0u64;
    let mut full = false;
    while lba + 8 <= FULL_SIZE {
        let c = s.write(0, lba, 8)?;
        if c.status == Status::NoSpace {
            s.request_cp()?;
            if s.write(0, lba, 8)?.status == Status::NoSpace {
                full = true;
                break;
            }
        }
        lba += 8;
        chunk += 1;
        if chunk.is_multiple_of(8) {
            s.request_cp()?;
        }
        if chunk.is_multiple_of(snap_every) && rng.chance(3, 4) {
            s.snapshot(0)?;
        }
    }
    if !full {
        return Err(format!("seed {seed}: the pool never filled"));
    }
    never_full(&mut s)?;
    s.audit_liveness()?;
    // Random pressure on the full pool.
    for (i, op) in plan.iter().enumerate() {
        let step = |e: String| format!("seed {seed} op {i} {op:?}: {e}");
        s.settle().map_err(step)?;
        match *op {
            FullOp::DeleteSnapshot { which } => {
                if !s.snaps.is_empty() {
                    let id = s.snaps[usize::from(which) % s.snaps.len()].snap_id;
                    s.delete_snapshot(id).map_err(step)?;
                }
            }
            FullOp::Punch { lba, blocks } => {
                let blocks = u64::from(blocks).min(FULL_SIZE);
                let lba = u64::from(lba) % (FULL_SIZE - blocks + 1);
                let c = s.punch(0, lba, blocks as u32, false).map_err(step)?;
                if !matches!(c.status, Status::Ok | Status::NvFailed) {
                    return Err(step(format!("punch status {:?}", c.status)));
                }
            }
            FullOp::Clean => {
                s.request_clean().map_err(step)?;
            }
            FullOp::Write { lba } => {
                let lba = u64::from(lba) % (FULL_SIZE - 8);
                let c = s.write(0, lba, 8).map_err(step)?;
                if !matches!(c.status, Status::Ok | Status::NoSpace | Status::NvFailed) {
                    return Err(step(format!("write status {:?}", c.status)));
                }
            }
            FullOp::Stop { kind } => {
                stop_and_recover(&mut s, kind).map_err(step)?;
            }
            FullOp::Snapshot => {
                if s.snaps.len() < 6 && !s.engine().nvfail_set().get(0) {
                    s.snapshot(0).map_err(step)?;
                }
            }
        }
        // Never wedged: a requested CP commits.
        s.request_cp().map_err(step)?;
        never_full(&mut s).map_err(step)?;
    }
    // Recovery: every snapshot deleted, half the volume punched, the
    // cleaner run; then writes are admitted again, before and after a
    // reboot.
    while let Some(id) = s.snaps.first().map(|x| x.snap_id) {
        // Asked with the engine idle: on a tight pool the background pump
        // keeps cleaning CPs going, and a deletion asked while one stages
        // fails the tick (`bug_a_snapshot_deletion_asked_while_a_cleaning_cp_stages_fails_the_tick`,
        // which this walk found on its own before it settled first).
        s.settle()?;
        s.delete_snapshot(id)?;
    }
    s.punch(0, 0, (FULL_SIZE / 2) as u32, false)?;
    for _ in 0..8 {
        s.request_clean()?;
    }
    for _ in 0..3 {
        s.request_cp()?;
    }
    never_full(&mut s)?;
    s.audit_liveness()?;
    let status = s.engine().pool_status();
    if status.admissible_blocks < 64 {
        return Err(format!(
            "seed {seed}: half the volume punched and cleaned, yet {} blocks admissible: {status:?}",
            status.admissible_blocks
        ));
    }
    let c = s.write(0, 8, 8)?;
    if c.status != Status::Ok {
        return Err(format!(
            "seed {seed}: the write after the recovery: {:?}",
            c.status
        ));
    }
    s.crash_daemon()?;
    s.boot()?;
    s.verify_recovery()?;
    let c = s.write(0, 16, 8)?;
    if c.status != Status::Ok {
        return Err(format!(
            "seed {seed}: the write after the reboot: {:?}",
            c.status
        ));
    }
    s.shutdown_clean()?;
    s.boot()?;
    s.verify_recovery()?;
    Ok(())
}

proptest! {
    #![proptest_config(ProptestConfig {
        cases: cases(12),
        failure_persistence: None,
        max_shrink_iters: 128,
        ..ProptestConfig::default()
    })]

    /// The brief's second property: a pool driven to 100% of its user
    /// capacity (with up to four snapshots pinning what is overwritten)
    /// still deletes snapshots, punches holes, cleans and reboots under
    /// any order of those, every requested CP commits (no wedge, at boot
    /// or in a CP), no CP fails for space, and once the space is released
    /// writes are admitted again.
    #[test]
    fn a_full_pool_recovers(
        seed in any::<u64>(),
        snaps in 0..=4u8,
        plan in prop::collection::vec(full_op(), 1..10),
    ) {
        if let Err(e) = full_walk(seed, snaps, &plan) {
            prop_assert!(false, "{e}");
        }
    }
}
