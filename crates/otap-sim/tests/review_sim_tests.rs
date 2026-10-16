//! Step 4 QA (Opus-QA-Adversarial): the engine with the persistent map,
//! folds, snapshots, holes, metadata reclamation and the E19 trust policy,
//! attacked through the simulator.
//!
//! The tests that reproduced a defect are regression tests now (their doc
//! comments name the defect in the Step 4 log and say what was wrong and
//! what the engine does now). The other tests pin properties that hold: crashes
//! in every phase of a fold CP, punches at the request bound, page-edge
//! LBAs, a rewind below a snapshot, damaged log regions, and the late
//! discard under a reordering device with tiny segments.

use otap_cp::{Backstore, Event, FoldPolicy, PeekSource, Phase, Status};
use otap_format::{
    SbSlot, SnapKind, SnapOrigin, SuperblockSlot, Uuid, BLOCK_SIZE, MAP_ENTRIES_PER_PAGE,
    SUPERBLOCK_SLOT_SIZE,
};
use otap_sim::{BootInfo, Schedule, Sim, SimConfig, SplitMix64, TearPolicy};

const PAGE: u64 = MAP_ENTRIES_PER_PAGE as u64;

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

fn small(seed: u64) -> SimConfig {
    SimConfig::small(seed)
}

fn sim_with(cfg: SimConfig) -> Sim {
    Sim::new(cfg).expect("the pool formats")
}

fn stalled() -> Schedule {
    Schedule {
        max_per_poll: 0,
        reorder: false,
        queue_depth: 64,
    }
}

fn fifo1() -> Schedule {
    Schedule {
        max_per_poll: 1,
        reorder: false,
        queue_depth: 64,
    }
}

/// A fold policy that never folds on its own.
fn no_auto_fold() -> FoldPolicy {
    FoldPolicy {
        log_bytes: 1 << 40,
        interval_ns: 1 << 60,
        max_cps: u64::MAX,
        max_dirty_pages: usize::MAX,
        max_overlay_entries: usize::MAX,
    }
}

fn ok(s: &mut Sim, vol: u32, lba: u64, blocks: u32) {
    let c = s.write(vol, lba, blocks).expect("the write completes");
    assert_eq!(c.status, Status::Ok, "write {vol}/{lba}+{blocks}");
}

fn punch_ok(s: &mut Sim, vol: u32, lba: u64, blocks: u32, zeroes: bool) {
    let c = s
        .punch(vol, lba, blocks, zeroes)
        .expect("the punch completes");
    assert_eq!(c.status, Status::Ok, "punch {vol}/{lba}+{blocks}");
}

fn source_of(s: &mut Sim, vol: u32, lba: u64) -> PeekSource {
    let mut buf = vec![0u8; BLOCK_SIZE];
    s.engine().peek(vol, lba, &mut buf).expect("peek")[0]
}

/// The byte offset on the device of the stripe holding the committed block
/// `vol`/`lba`.
fn stripe_offset_of(s: &mut Sim, vol: u32, lba: u64) -> usize {
    let entry = s.engine().map_entry(vol, lba).expect("mapped");
    assert!(entry.is_data(), "{vol}/{lba} is data");
    let layout = *s.engine().layout();
    let idx = layout.stripe_of_off(entry.off());
    layout
        .stripe_offset(entry.lseg(), idx)
        .expect("on the device") as usize
}

/// The newest slot of the ring that verifies, read from the medium (the
/// engine may or may not be running).
fn newest_slot(s: &mut Sim) -> SuperblockSlot {
    let layout = *s.disk().layout();
    let mut best: Option<SuperblockSlot> = None;
    let mut buf = vec![0u8; SUPERBLOCK_SLOT_SIZE];
    for slot in SbSlot::ALL {
        s.disk()
            .read(layout.sb_pba(slot), &mut buf)
            .expect("ring read");
        if let Ok(parsed) = SuperblockSlot::read(&buf) {
            if best.is_none_or(|b| parsed.sb.cp_seq > b.sb.cp_seq) {
                best = Some(parsed);
            }
        }
    }
    best.expect("a verified slot")
}

/// Ticks until `cond`, or gives up after `budget` ticks.
fn tick_while(s: &mut Sim, budget: u32, mut cond: impl FnMut(&mut Sim) -> bool) -> bool {
    for _ in 0..budget {
        if !cond(s) {
            return true;
        }
        if s.tick().is_err() {
            return false;
        }
    }
    false
}

/// Asks for a CP and ticks until it commits or the pool reports full.
fn cp_or_full(s: &mut Sim) -> bool {
    let before = s.engine().cp_seq();
    s.engine().request_cp();
    tick_while(s, 20_000, |s| {
        let e = s.engine();
        e.cp_seq() == before && !e.is_pool_full()
    });
    s.engine().cp_seq() > before
}

fn power_cut_and_recover(s: &mut Sim, tear: TearPolicy) -> Result<otap_sim::Verdict, String> {
    s.disk().set_tear_policy(tear);
    s.crash_power()?;
    s.disk().set_tear_policy(TearPolicy::default());
    s.boot()?;
    let v = s.verify_recovery()?;
    s.clear_nvfail()?;
    Ok(v)
}

// ---------------------------------------------------------------------------
// Defects
// ---------------------------------------------------------------------------

/// Found by the Step 4 review (defect 6, fixed): a snapshot asked for while a fold CP is in flight is silently
/// lost.
///
/// `create_snapshot` queues the request and asks for a fold; the snapshot
/// is taken by the next CP's `prepare_meta`. When the request arrives after
/// the CP in flight was prepared and that CP is itself a fold (requested,
/// by policy, for another snapshot, or at shutdown), `finalize` runs
/// `if fold { self.snap_requests.clear(); ... }` and drops the request that
/// CP never saw: `create_snapshot` returned `Ok`, no `SnapshotCreated`
/// follows, and the snapshot never exists. Two snapshots asked for back to
/// back hit this. The right behaviour: `finalize` removes only the requests
/// that CP took (its `new_snapshots`), so the late one is taken by the next
/// fold.
#[test]
fn a_snapshot_asked_for_during_an_in_flight_fold_is_taken_by_the_next() {
    let mut cfg = small(9001);
    cfg.fold = no_auto_fold();
    let mut s = sim_with(cfg);
    s.boot().unwrap();
    ok(&mut s, 0, 0, 4);
    ok(&mut s, 1, 0, 2);
    s.request_cp().unwrap();
    // A fold CP the device holds in its metadata phase.
    s.disk().set_schedule(stalled());
    s.engine().request_fold();
    s.ticks(4).unwrap();
    assert_eq!(s.engine().phase(), Phase::Metadata, "the fold is in flight");
    // Now the operator snapshots volume 1.
    let uuid = Uuid::from_bytes([0x77; 16]);
    s.engine()
        .create_snapshot(1, uuid, SnapKind::Crash, SnapOrigin::User)
        .expect("the request is accepted");
    s.disk().set_schedule(Schedule::default());
    s.settle().unwrap();
    for _ in 0..3 {
        s.request_cp().unwrap();
    }
    let created = s
        .events
        .iter()
        .any(|e| matches!(e, Event::SnapshotCreated { vol: 1, .. }));
    // Before the fix: the request was cleared by the in-flight fold's finalize.
    assert!(
        created && s.engine().snapshots().iter().any(|x| x.uuid == uuid),
        "the snapshot of volume 1 was accepted and never taken: {:?}",
        s.engine().snapshots()
    );
}

/// What a boot does when the medium lost the data stripe of the newest CP
/// (a write-back controller that acknowledged the barrier and never
/// destaged: E19's dead-battery case), the CP being a fold or not.
fn lost_stripe_of_newest_cp(
    fold: bool,
    seed: u64,
) -> (BootInfo, Result<otap_sim::Verdict, String>) {
    let mut cfg = small(seed);
    cfg.fold = no_auto_fold();
    let mut s = sim_with(cfg);
    s.boot().unwrap();
    ok(&mut s, 0, 0, 4);
    s.request_cp().unwrap();
    // The newest CP carries volume 1's blocks 8..11.
    ok(&mut s, 1, 8, 3);
    if fold {
        s.request_fold().unwrap();
    } else {
        s.request_cp().unwrap();
    }
    let off = stripe_offset_of(&mut s, 1, 8);
    let stripe = s.engine().layout().stripe_bytes as usize;
    s.crash_power().unwrap();
    s.disk().corrupt_durable(off, &vec![0u8; stripe]);
    let boot = s.boot().unwrap();
    let verdict = s.verify_recovery();
    (boot, verdict)
}

/// The control for the defect below: when the newest CP is not a fold,
/// the boot checks the stripe that CP wrote, finds it gone, and rewinds to
/// the CP before, with every volume held and a consistent prefix.
#[test]
fn a_lost_stripe_of_a_non_fold_cp_is_caught_and_rewound() {
    let (boot, verdict) = lost_stripe_of_newest_cp(false, 9010);
    assert!(boot.rewound && boot.dirty, "{boot:?}");
    let v = verdict.expect("the recovery verifies");
    assert!(v.lost.get(1) && v.nvfail.get(1));
}

/// Found by the Step 4 review (defect 1, fixed): the boot never checks the data stripes of a fold CP.
///
/// `boot::load_cp` collects the stripes to verify from the selected CP's
/// own delta records (`replayed.cp_seq == sb.cp_seq`), but a fold CP writes
/// no delta region: its records went into the pages it rewrote. So when
/// the newest CP is a fold (on request, for a snapshot, by the fold policy,
/// and always at a clean shutdown), its stripes are never checked: a CP
/// whose data never reached the medium is selected as good, the volumes
/// read `Corrupt` where the rewound CP would have served a consistent
/// prefix, and after a clean shutdown the boot is not even dirty, so
/// nvfail holds nothing. The right behaviour: for a fold CP, take the
/// stripes to check from the fold's pages (every entry born at the CP;
/// the boot already reads and verifies each fold page) and rewind exactly
/// as for a non-fold CP.
#[test]
fn the_boot_checks_the_stripes_a_fold_cp_wrote() {
    let (boot, verdict) = lost_stripe_of_newest_cp(true, 9011);
    // Before the fix: the fold CP is selected as if its stripes had landed.
    assert!(
        boot.rewound && boot.dirty,
        "a fold CP whose stripe is gone was selected: {boot:?}"
    );
    verdict.expect("the recovery verifies (a consistent prefix behind nvfail)");
}

/// The volume used by the damaged-map-page tests: a fold put volume 0's
/// blocks in a map page, a later non-fold CP is the newest, the daemon
/// restarts (a cold page cache), and a bit rots in the page on the medium.
fn damaged_map_page(seed: u64) -> Sim {
    let mut cfg = small(seed);
    cfg.fold = no_auto_fold();
    let mut s = sim_with(cfg);
    s.boot().unwrap();
    ok(&mut s, 0, 0, 4);
    s.request_fold().unwrap();
    ok(&mut s, 1, 0, 1);
    s.request_cp().unwrap();
    let page = s.engine().map().volume(0).unwrap().index()[0];
    assert!(!page.is_none(), "volume 0 has a map page");
    s.crash_daemon().unwrap();
    s.disk()
        .corrupt_durable(page.page_pba as usize * BLOCK_SIZE + 100, &[0xA5; 4]);
    s.boot().unwrap();
    s
}

/// Found by the Step 4 review (defect 13, fixed): a damaged map page turns a read into an engine failure.
///
/// `serve_read` maps only `EngineError::Corrupt` to a status; the
/// `MapError` that `MapSet::get` returns when the page fails its CRC in the
/// index entry (`read_disk_block`, `Err(e) => return Err(e.into())`) is
/// returned from `serve` and `tick` instead. The request is never
/// completed (a ublk tag hangs), and the tick loop (`device::serve`) stops
/// on the first such error: one bad sector in one map page takes every
/// volume down. The right behaviour: the read completes with
/// `Status::Corrupt` like a block whose data fails its CRC, and the engine
/// keeps serving.
#[test]
fn a_damaged_map_page_answers_corrupt_and_the_engine_goes_on() {
    let mut s = damaged_map_page(9020);
    // Before the fix: `read` returns `Err("tick: map: format: ...")`.
    let (status, _) = s
        .read(0, 1, 1)
        .expect("the read completes instead of failing the engine's tick");
    assert_eq!(status, Status::Corrupt);
    // The other volume is unaffected.
    assert_eq!(s.read(1, 0, 1).unwrap().0, Status::Ok);
}

/// Found by the Step 4 review (defect 14, fixed): a damaged map page wedges every consistency point.
///
/// A write over a block of the damaged page is acknowledged from the arena;
/// the CP that carries it computes what each delta replaces
/// (`MapSet::prepare_cp` reads the old entry for the live-byte accounting)
/// and gets the page's CRC error, which `step_staging` returns from `tick`
/// as an internal failure. The job is dropped with its generation still
/// frozen, so the next tick starts the same CP again, rewrites its data
/// stripes into fresh stripes, and fails again: no CP ever commits for any
/// volume, and the data segments fill with copies. The right behaviour: the
/// CP commits (the old entry of a damaged page is unknown to the
/// accounting, the new entry replaces it), or at worst the damage is
/// contained to the volume (nvfail) while the other volumes' CPs commit.
#[test]
fn a_damaged_map_page_does_not_wedge_the_consistency_points() {
    let mut s = damaged_map_page(9021);
    ok(&mut s, 0, 2, 1);
    ok(&mut s, 1, 5, 1);
    let before = s.engine().cp_seq();
    s.engine().request_cp();
    let mut failures = 0;
    for _ in 0..2_000 {
        if s.tick().is_err() {
            failures += 1;
        }
        if s.engine().cp_seq() > before {
            break;
        }
    }
    // Before the fix: every tick that reaches the metadata phase fails; nothing commits.
    assert_eq!(failures, 0, "ticks failed");
    assert!(
        s.engine().cp_seq() > before,
        "no CP committed after the damage (volume 1's write is stuck too)"
    );
}

/// Found by the Step 4 review (defect 15, fixed): the boot CP's segment-table region is never released.
///
/// Every CP queues the previous CP's table regions (`table_regions`: roots,
/// directory, snapshot table, segment table) to be freed. A boot seeds
/// `table_regions` from `BootState::table_regions`, which `walk_cp` fills
/// with the roots, directory and snapshot table only; the selected slot's
/// `seg_table` region is counted in `meta_live` (`load_cp` adds it) but is
/// never queued, so its bytes stay live for the life of the engine and the
/// segment holding it is never recycled: one leaked metadata segment per
/// boot until the next boot recomputes the liveness. The right behaviour:
/// `table_regions` includes the selected slot's `seg_table`, as it does for
/// every CP the engine commits itself.
#[test]
fn the_boot_cps_segment_table_region_is_released() {
    let mut cfg = small(9030);
    cfg.fold.max_cps = 2;
    let mut s = sim_with(cfg);
    s.boot().unwrap();
    for i in 0..4u64 {
        ok(&mut s, 0, i * 3, 2);
        s.request_cp().unwrap();
    }
    s.crash_daemon().unwrap();
    let slot = newest_slot(&mut s);
    let layout = *s.disk().layout();
    let leaked = layout
        .pseg_of_pba(slot.sb.seg_table.pba)
        .expect("the segment table is in a segment");
    s.boot().unwrap();
    let from = s.events.len();
    // Every page, index, delta and table the boot CP referenced is replaced
    // many times over; the stream moves through many segments.
    for i in 0..48u64 {
        ok(&mut s, 0, (i * 5) % 60, 3);
        ok(&mut s, 1, (i * 7) % 60, 3);
        if i % 3 == 2 {
            s.request_fold().unwrap();
        } else {
            s.request_cp().unwrap();
        }
    }
    let freed = s.events[from..]
        .iter()
        .any(|e| matches!(e, Event::SegmentFreed { pseg, .. } if *pseg == leaked));
    // Before the fix: the segment keeps the boot CP's segment table as live bytes.
    assert!(
        freed,
        "segment {leaked} (the boot CP's segment table) was never freed; {} bytes still live",
        s.engine().meta_live_bytes(leaked)
    );
}

/// How taking snapshot after snapshot on a small pool ended.
#[derive(Debug)]
enum Filled {
    /// A snapshot CP failed for want of a segment and the engine reported
    /// the full pool.
    PoolFull { taken: u32 },
    /// A tick failed.
    TickFailed { taken: u32, error: String },
}

/// Snapshots volume 0 of an eight-segment pool (no data written after the
/// first two blocks) until a snapshot's CP no longer fits: every snapshot
/// pins its root page for good (no deletion yet, E25).
fn fill_with_snapshots(seed: u64) -> (Sim, Filled) {
    let mut cfg = small(seed);
    cfg.segments = 8;
    cfg.fold = no_auto_fold();
    let mut s = sim_with(cfg);
    s.boot().unwrap();
    ok(&mut s, 0, 0, 2);
    s.request_cp().unwrap();
    for taken in 0..400u32 {
        let uuid = Uuid::from_bytes({
            let mut b = [0x31u8; 16];
            b[..4].copy_from_slice(&(taken + 1).to_le_bytes());
            b
        });
        s.engine()
            .create_snapshot(0, uuid, SnapKind::Crash, SnapOrigin::Scheduled)
            .unwrap();
        let seen = s.events.len();
        for _ in 0..20_000 {
            if let Err(error) = s.tick() {
                return (s, Filled::TickFailed { taken, error });
            }
            if s.engine().is_pool_full() {
                return (s, Filled::PoolFull { taken });
            }
            if s.events[seen..]
                .iter()
                .any(|e| matches!(e, Event::SnapshotCreated { .. }))
            {
                break;
            }
        }
    }
    panic!("400 snapshots never filled an eight-segment pool");
}

/// Found by the Step 4 review (defect 10, fixed): running out of metadata space fails the tick instead of reporting
/// a full pool.
///
/// `step_staging` turns a CP's `NoSpace` into a full pool only for
/// `EngineError::NoSpace` (a data segment) and `EngineError::Map(MapError::
/// NoSpace)` (a region the map placed). The regions the engine places
/// itself in `prepare_meta_inner` (a snapshot's root page, the snapshot
/// table, the roots table, the directory, the segment table's reservation)
/// fail with `EngineError::Meta(MetaError::NoSpace)`, which falls into
/// `Err((e, _)) => Err(e)`: the tick fails with "metadata: no free metadata
/// segment", `pool_full` stays false, writes keep being acknowledged into
/// the arena, and every later tick retries the same CP and fails again. A
/// boot commit that hits it makes `Engine::boot` itself fail. The right
/// behaviour: every `NoSpace` of a CP, whoever placed the region, is a full
/// pool (`fail_cp(.., no_space = true)`).
#[test]
fn running_out_of_metadata_space_reports_a_full_pool() {
    let (mut s, filled) = fill_with_snapshots(9041);
    // Before the fix: `Filled::TickFailed { error: "tick: metadata: no free metadata segment" }`.
    assert!(
        matches!(filled, Filled::PoolFull { taken } if taken >= 8),
        "{filled:?}"
    );
    // A full pool refuses writes with a status and keeps serving reads.
    assert_eq!(s.write(1, 0, 1).unwrap().status, Status::NoSpace);
    assert_eq!(s.read(0, 0, 2).unwrap().0, Status::Ok);
}

/// Found by the Step 4 review (defect 11, fixed): a pool whose metadata fills up can no longer boot.
///
/// `META_RESERVE_SEGMENTS` keeps two segments free only against *data*
/// allocations (`open_data_segment`); metadata runs (`MetaStream::open_run`,
/// `allocate_run(.., reserve 0)`) take the last free segment, and snapshot
/// root pages are pinned forever (no deletion yet, E25). Snapshots alone
/// therefore fill the pool, and after a restart the boot commit finds no
/// segment either (today `Engine::boot` fails outright with "metadata: no
/// free metadata segment"; with the defect above fixed it would wait as a
/// full pool and never serve): no volume can even be read, which is what
/// the reserve exists to prevent ("so a boot or a shutdown commit always
/// finds room for its metadata", E24). The right behaviour: metadata growth
/// that is optional (a snapshot, a fold) respects the reserve, a snapshot
/// that would breach it is refused, and the boot and shutdown commits
/// always fit.
#[test]
fn a_pool_filled_by_snapshot_metadata_still_boots() {
    let (mut s, filled) = fill_with_snapshots(9040);
    let (taken, how) = match filled {
        Filled::PoolFull { taken } => (taken, "a full pool".to_string()),
        Filled::TickFailed { taken, error } => (taken, format!("a failed tick ({error})")),
    };
    assert!(taken >= 8, "{taken} snapshots before {how}");
    // The pool is full of pinned metadata. A restart:
    s.crash_daemon().unwrap();
    // Before the fix: the boot commit cannot be written; the engine never serves.
    let booted = s.boot();
    assert!(
        booted.is_ok(),
        "a pool full of snapshot metadata ({how} after {taken} snapshots) no longer boots: {booted:?}"
    );
    assert_eq!(s.read(0, 0, 2).unwrap().0, Status::Ok);
}

/// Found by the Step 4 review (defect 24, fixed): dead data segments are never freed, contrary to the status
/// document.
///
/// `docs/06-implementation-status.md` (section 8) says "Metadata segments
/// and dead data segments are freed (and discarded on flash)", and E24 says
/// "A freed data or metadata segment is discarded whole or merely returned,
/// per the DiscardPolicy". `release_freed` skips every segment that is not
/// metadata (`if !is_meta ... continue`), and nothing else frees one, so a
/// data segment whose blocks are all dead stays allocated: rewriting one
/// block over and over fills the pool and ends in `NoSpace` with a single
/// live block. The right behaviour is what the documents say (a data
/// segment whose live count is zero is freed, after the same deferral as a
/// metadata region, since the previous CP may still name its blocks); or,
/// if this is deliberate until the cleaner, the documents must say so.
#[test]
fn dead_data_segments_are_freed() {
    let mut cfg = small(9050);
    cfg.segments = 16;
    let mut s = sim_with(cfg);
    s.boot().unwrap();
    let mut committed = 0;
    for _ in 0..400 {
        let c = s.write(0, 7, 1).unwrap();
        if c.status != Status::Ok {
            break;
        }
        if !cp_or_full(&mut s) {
            break;
        }
        committed += 1;
    }
    let data_live: u64 = (1..=16u64).map(|p| s.engine().live_blocks(p)).sum();
    // Before the fix: the pool filled with dead copies of one block.
    assert_eq!(
        committed, 400,
        "the pool filled after {committed} one-block overwrites ({data_live} live data blocks)"
    );
}

/// Found by the Step 4 review (defect 16, fixed): one damaged sector in an old snapshot's root page makes the whole
/// pool unbootable.
///
/// Every candidate of the ring lists every snapshot, and `walk_cp` turns a
/// snapshot root page (or pinned index) that fails verification into
/// `TornReason::Snapshot` for the candidate; since the damage is in a page
/// both candidates share, every candidate is "torn" and the boot fails
/// with `NoVerifiableCp`. Media damage to a snapshot (secondary data) takes
/// every live volume of the pool offline. The right behaviour: damage that
/// every candidate shares is not a torn CP; the boot takes the newest
/// candidate, reports the snapshot as damaged (or drops it with an
/// event), and serves the live volumes.
#[test]
fn one_damaged_snapshot_root_page_does_not_stop_the_boot() {
    let mut s = sim_with(small(9060));
    s.boot().unwrap();
    ok(&mut s, 0, 0, 4);
    ok(&mut s, 1, 0, 4);
    let snap = s.snapshot(0).unwrap();
    ok(&mut s, 1, 8, 1);
    s.request_cp().unwrap();
    s.request_cp().unwrap();
    // Find the snapshot's root page through the newest slot's table (since
    // format 2.4, Step 6B, the root names the table's head page and the
    // table is its base and deltas replayed: the scenario is unchanged).
    let slot = newest_slot(&mut s);
    let layout = *s.engine().layout();
    let records = otap_cp::load_table::<otap_format::SnapRecord, _>(
        s.disk(),
        &layout,
        &slot.sb.agg_id,
        slot.sb.cp_seq,
        slot.sb.snap_table,
        slot.sb.snap_head_cp,
    )
    .unwrap()
    .records;
    let rec = records.iter().find(|r| r.snap_id == snap).unwrap();
    s.crash_daemon().unwrap();
    s.disk()
        .corrupt_durable(rec.map_root_pba as usize * BLOCK_SIZE + 300, &[0x5A; 2]);
    // Before the fix: every candidate is "torn" by the shared damage.
    let booted = s.boot();
    assert!(
        booted.is_ok(),
        "the pool does not boot because one snapshot page is damaged: {booted:?}"
    );
    assert_eq!(source_of(&mut s, 0, 0), PeekSource::Disk);
    assert_eq!(s.read(1, 0, 4).unwrap().0, Status::Ok);
}

/// Found by the Step 4 review (defect 9, fixed): `snapshot_diff` accepts a base newer than the snapshot and answers
/// that nothing changed.
///
/// The API says the base is "another snapshot of the same volume, older,
/// or none"; `base_cp` checks the volume but not the order, so
/// `snapshot_diff(old, Some(newer))` walks births above the newer
/// snapshot's CP in the older snapshot's pages and returns an empty delta,
/// which a replication sender would ship as "in sync". The right
/// behaviour: refuse a base that is not older than the snapshot (as a
/// base of another volume is refused).
#[test]
fn snapshot_diff_refuses_a_newer_base() {
    let mut s = sim_with(small(9070));
    s.boot().unwrap();
    ok(&mut s, 0, 0, 4);
    let older = s.snapshot(0).unwrap();
    ok(&mut s, 0, 10, 4);
    let newer = s.snapshot(0).unwrap();
    // The honest direction: four blocks changed between them.
    assert_eq!(
        s.engine().snapshot_diff(newer, Some(older)).unwrap().len(),
        4
    );
    // Before the fix: the reversed request is answered with an empty delta.
    let reversed = s.engine().snapshot_diff(older, Some(newer));
    assert!(
        reversed.is_err(),
        "a base newer than the snapshot was accepted: {reversed:?}"
    );
}

/// Found by the Step 4 review (defect 8, fixed): a volume the control plane says is larger than its on-disk root is
/// served at the larger size, and the writes beyond the root wedge every CP.
///
/// `assemble` sets each volume's `size_blocks` from `EngineConfig::volumes`
/// even when the map root on disk says otherwise, and `serve` checks
/// requests against that size. A write past the root's `lba_count` is
/// acknowledged from the arena; the CP that carries it fails in
/// `MapSet::prepare_cp` with `MapError::OutOfRange`, which `step_staging`
/// returns from `tick` as an internal failure, and every later tick retries
/// it: no CP commits for any volume, and the acknowledged write can never
/// become durable. (A grown volume is the natural way to get there: there
/// is no resize yet, and the control plane is the authority on sizes.)
/// The right behaviour: the boot refuses a volume whose configured size
/// differs from its root, or serves the root's size (refusing the write
/// with `OutOfRange`); a resize must go through a CP that rewrites the
/// root.
#[test]
fn a_volume_grown_in_the_configuration_is_refused_at_boot() {
    let mut s = sim_with(small(9080));
    s.boot().unwrap();
    ok(&mut s, 0, 0, 4);
    s.shutdown_clean().unwrap();
    s.cfg.volumes = vec![(0, 128), (1, 64)];
    let Ok(_) = s.boot() else {
        // Refusing the mismatched configuration is a correct answer.
        return;
    };
    let c = s.write(0, 100, 1).unwrap();
    if c.status != Status::Ok {
        // Refused against the on-disk size: also correct.
        return;
    }
    let before = s.engine().cp_seq();
    s.engine().request_cp();
    let mut failures = Vec::new();
    for _ in 0..500 {
        if let Err(e) = s.tick() {
            failures.push(e);
        }
        if s.engine().cp_seq() > before {
            break;
        }
    }
    // Before the fix: "tick: map: volume 0 LBA 100 is out of range", every tick.
    assert!(
        failures.is_empty() && s.engine().cp_seq() > before,
        "an acknowledged write past the on-disk root wedged the CPs: {} failures, first {:?}",
        failures.len(),
        failures.first()
    );
}

/// Found by the Step 4 review (defect 12, fixed): a fold the policy forces on a nearly full pool wedges it, though a
/// plain CP would fit.
///
/// Data allocations keep `META_RESERVE_SEGMENTS` free for metadata, but a
/// fold's cost is every dirty map page plus the indexes, which can exceed
/// what is left; `should_fold` decides from the policy alone, a fold that
/// does not fit fails the CP with `NoSpace`, and every retry (`request_cp`,
/// the boot commit, the shutdown CP) recomputes the same decision and fails
/// the same way: the pool stops committing even an empty CP, and after a
/// restart it cannot write its boot commit. A fold is an optimisation (only
/// a snapshot needs one); the right behaviour is to fall back to a CP that
/// logs its deltas when the fold does not fit, so the pool keeps
/// committing and booting.
#[test]
fn a_policy_fold_that_does_not_fit_gives_way_to_a_plain_cp() {
    let pages = 96u64;
    let mut cfg = small(9090);
    cfg.segments = 14;
    cfg.volumes = vec![(0, pages * PAGE), (1, 64)];
    cfg.fold = no_auto_fold();
    cfg.fold.max_dirty_pages = 32;
    // The Step 4 regime this test pins: no reserve beyond the boot and
    // shutdown commits, and no cleaner.
    cfg.reserve_segments = Some(0);
    cfg.cleaner.background = false;
    let mut s = sim_with(cfg);
    s.boot().unwrap();
    // Fill the data segments to the metadata reserve, all committed.
    let mut lba = 0u64;
    while s.engine().free_segments() > 3 {
        ok(&mut s, 1, lba % 56, 7);
        lba += 7;
        assert!(cp_or_full(&mut s), "a data CP failed while filling");
    }
    // Punch one block in every map page of volume 0 (holes need no data
    // stripe): a small delta, committed as a plain CP.
    for p in 0..pages {
        punch_ok(&mut s, 0, p * PAGE + 1, 1, false);
    }
    assert!(cp_or_full(&mut s), "the CP carrying the punches failed");
    assert!(s.engine().map().dirty_pages() >= 32);
    // The policy now forces a fold of every dirty page at the next CP.
    let mut failures = Vec::new();
    let before = s.engine().cp_seq();
    for _ in 0..3 {
        s.engine().request_cp();
        for _ in 0..2_000 {
            if let Err(e) = s.tick() {
                failures.push(e);
                break;
            }
            if s.engine().cp_seq() > before {
                break;
            }
        }
        if s.engine().cp_seq() > before {
            break;
        }
    }
    // Before the fix: the fold fails for want of a segment, again and again.
    assert!(
        s.engine().cp_seq() > before,
        "no CP commits on a pool with room for one: pool full {}, {} free segments, tick failures {failures:?}",
        s.engine().is_pool_full(),
        s.engine().free_segments()
    );
}

// ---------------------------------------------------------------------------
// Properties that hold (pinned)
// ---------------------------------------------------------------------------

fn kind_of(phase: Phase) -> otap_sim::OpKind {
    match phase {
        Phase::Staging => otap_sim::OpKind::Stripe,
        Phase::Metadata => otap_sim::OpKind::Meta,
        Phase::Flushing => otap_sim::OpKind::Flush,
        Phase::CommitA | Phase::CommitB | Phase::Idle => otap_sim::OpKind::Commit,
    }
}

/// A power cut in each phase of a fold CP that rewrites pages the
/// previous CPs' log and pages referenced, with and without a snapshot
/// riding the fold, under every tear policy: the old pages, indexes and
/// deltas are still there for the rewind (frees are deferred), the image
/// is a prefix behind nvfail, a snapshot exists exactly when its CP
/// landed, and a clean restart afterwards keeps everything.
#[test]
fn a_power_cut_in_every_phase_of_a_fold_recovers_under_every_tear_policy() {
    let mut failures = Vec::new();
    let mut seed = 9100;
    for tear in [DROP, WHOLE, TORN, TearPolicy::default()] {
        for phase in [
            Phase::Staging,
            Phase::Metadata,
            Phase::Flushing,
            Phase::CommitA,
            Phase::CommitB,
        ] {
            for with_snapshot in [false, true] {
                seed += 1;
                if let Err(e) = crash_in_fold(tear, phase, with_snapshot, seed) {
                    failures.push(format!(
                        "{tear:?} {phase:?} snapshot {with_snapshot} seed {seed}: {e}"
                    ));
                }
            }
        }
    }
    assert!(failures.is_empty(), "{failures:#?}");
}

fn crash_in_fold(
    tear: TearPolicy,
    phase: Phase,
    with_snapshot: bool,
    seed: u64,
) -> Result<(), String> {
    let mut cfg = small(seed);
    cfg.volumes = vec![(0, 2 * PAGE + 9), (1, 64)];
    cfg.fold = no_auto_fold();
    cfg.schedule = fifo1();
    let mut s = Sim::new(cfg)?;
    s.boot()?;
    // A fold, then logged CPs on top of it touching both pages of volume 0.
    s.write(0, 0, 4)?;
    s.write(0, PAGE - 2, 4)?;
    s.write(1, 3, 2)?;
    s.request_fold()?;
    s.write(0, 2, 3)?;
    s.punch(0, PAGE, 2, true)?;
    s.request_cp()?;
    s.write(0, 2 * PAGE, 9)?;
    s.punch(1, 0, 4, false)?;
    s.request_cp()?;
    let base = s.engine().cp_seq();
    // The fold that rewrites them, carrying more writes.
    s.write(0, 1, 1)?;
    s.write(1, 60, 4)?;
    if with_snapshot {
        s.engine()
            .create_snapshot(
                0,
                Uuid::from_bytes([9; 16]),
                SnapKind::Crash,
                SnapOrigin::User,
            )
            .map_err(|e| e.to_string())?;
    } else {
        s.engine().request_fold();
    }
    s.tick_until(
        |e| e.phase() == phase && e.backstore().pending_kinds().contains(&kind_of(phase)),
        "the phase",
    )?;
    s.disk().set_tear_policy(tear);
    s.crash_power()?;
    s.disk().set_tear_policy(TearPolicy::default());
    let boot = s.boot()?;
    if !boot.dirty {
        return Err("a power cut booted clean".into());
    }
    if boot.cp_seq < base {
        return Err(format!("rewound below {base}: {boot:?}"));
    }
    s.verify_recovery()?;
    let snaps = s.engine().snapshots();
    if boot.cp_seq == base && !snaps.is_empty() {
        return Err(format!("a snapshot exists without its CP: {snaps:?}"));
    }
    if with_snapshot && boot.cp_seq > base && snaps.len() != 1 {
        return Err(format!(
            "the fold CP landed without its snapshot: {snaps:?}"
        ));
    }
    s.clear_nvfail()?;
    s.write(0, 2 * PAGE + 8, 1)?;
    s.shutdown_clean()?;
    s.boot()?;
    let v = s.verify_recovery()?;
    if v.k != s.model.last_op() {
        return Err(format!(
            "clean restart kept {} of {}",
            v.k,
            s.model.last_op()
        ));
    }
    Ok(())
}

/// Punches at the request bound on a volume larger than it: exactly
/// `MAX_PUNCH_BLOCKS` is served, one more is refused, both the fold and
/// the logged path commit them as holes across hundreds of map pages, and
/// a power cut keeps them (checked at page edges; the full image is too
/// large for the checker's every-block walk).
#[test]
fn punches_at_the_request_bound_commit_as_holes_across_pages() {
    let max = u64::from(otap_cp::MAX_PUNCH_BLOCKS);
    for fold_policy in [false, true] {
        let mut cfg = small(9200 + u64::from(fold_policy));
        cfg.segments = 96;
        cfg.volumes = vec![(0, max + 2 * PAGE), (1, 64)];
        if !fold_policy {
            cfg.fold = no_auto_fold();
        }
        let mut s = sim_with(cfg);
        s.boot().unwrap();
        let edges = [
            0,
            1,
            PAGE - 1,
            PAGE,
            max - 1,
            max,
            max + 1,
            max + 2 * PAGE - 1,
        ];
        for lba in edges {
            ok(&mut s, 0, lba, 1);
        }
        s.request_cp().unwrap();
        let c = s.punch(0, 0, otap_cp::MAX_PUNCH_BLOCKS + 1, false).unwrap();
        assert_eq!(c.status, Status::TooLarge);
        punch_ok(&mut s, 0, 1, otap_cp::MAX_PUNCH_BLOCKS, true);
        assert_eq!(source_of(&mut s, 0, 1), PeekSource::Hole);
        assert_eq!(source_of(&mut s, 0, max), PeekSource::Hole);
        assert_eq!(source_of(&mut s, 0, max + 1), PeekSource::Disk);
        s.request_cp().unwrap();
        for lba in edges {
            let e = s.engine().map_entry(0, lba).expect("mapped");
            let punched = (1..=max).contains(&lba);
            assert_eq!(e.is_hole(), punched, "LBA {lba}: {e:?}");
        }
        s.disk().set_tear_policy(TORN);
        s.crash_power().unwrap();
        s.disk().set_tear_policy(TearPolicy::default());
        let boot = s.boot().unwrap();
        assert!(!boot.rewound, "{boot:?}");
        for lba in edges {
            let want = if (1..=max).contains(&lba) {
                PeekSource::Hole
            } else {
                PeekSource::Disk
            };
            assert_eq!(source_of(&mut s, 0, lba), want, "LBA {lba}");
        }
        let mut buf = vec![0u8; BLOCK_SIZE];
        s.engine().peek(0, max - 1, &mut buf).unwrap();
        assert!(buf.iter().all(|b| *b == 0));
    }
}

/// Writes and punches at every map-page edge of volumes on both sides of a
/// page boundary, snapshots between them, folds, and power cuts under each
/// tear policy: every recovery verifies, every snapshot reads its image,
/// and every diff is exact.
#[test]
fn page_edge_blocks_survive_snapshots_folds_and_power_cuts() {
    let tears = [DROP, WHOLE, TORN, TearPolicy::default()];
    for (round, tear) in tears.into_iter().enumerate() {
        let mut cfg = small(9300 + round as u64);
        cfg.volumes = vec![(0, 2 * PAGE + 1), (1, PAGE - 1)];
        cfg.fold.max_cps = 3;
        let mut s = sim_with(cfg);
        s.boot().unwrap();
        let edges: [(u32, u64); 8] = [
            (0, 0),
            (0, PAGE - 1),
            (0, PAGE),
            (0, 2 * PAGE - 1),
            (0, 2 * PAGE),
            (1, 0),
            (1, PAGE / 2),
            (1, PAGE - 2),
        ];
        let mut rng = SplitMix64::new(9300 + round as u64);
        for step in 0..14 {
            for (vol, lba) in edges {
                if !rng.chance(1, 2) || s.engine().nvfail_set().get(vol) {
                    continue;
                }
                let size = if vol == 0 { 2 * PAGE + 1 } else { PAGE - 1 };
                let blocks = rng.range(1, 3).min(size - lba) as u32;
                if rng.chance(1, 4) {
                    punch_ok(&mut s, vol, lba, blocks, rng.chance(1, 2));
                } else {
                    ok(&mut s, vol, lba, blocks);
                }
            }
            match step % 5 {
                1 => {
                    if !s.engine().nvfail_set().get(0) {
                        s.snapshot(0).unwrap();
                    }
                }
                3 => {
                    power_cut_and_recover(&mut s, tear).unwrap();
                }
                _ => {
                    s.request_cp().unwrap();
                }
            }
            s.check_disk_prefix().unwrap();
            s.verify_snapshots().unwrap();
            let ids: Vec<u64> = s.snaps.iter().map(|x| x.snap_id).collect();
            for id in ids {
                s.check_diff(id).unwrap();
            }
        }
        s.shutdown_clean().unwrap();
        s.boot().unwrap();
        let v = s.verify_recovery().unwrap();
        assert_eq!(v.k, s.model.last_op());
    }
}

/// A boot that rewinds below a snapshot's CP (media damage under the
/// newest CP's roots table) loses the snapshot with the CP: it is not
/// listed, its id answers `NoSuchSnapshot`, nothing dangles, and new
/// snapshots, folds and restarts work afterwards.
#[test]
fn a_rewind_below_a_snapshot_drops_it_without_a_dangling_reference() {
    let mut s = sim_with(small(9400));
    s.boot().unwrap();
    ok(&mut s, 0, 0, 4);
    ok(&mut s, 1, 0, 4);
    s.request_cp().unwrap();
    let kept = s.snapshot(1).unwrap();
    s.request_cp().unwrap();
    let lost = s.snapshot(0).unwrap();
    let snap_cp = s.engine().cp_seq();
    let slot = newest_slot(&mut s);
    assert_eq!(slot.sb.cp_seq, snap_cp, "the snapshot's CP is the newest");
    s.crash_power().unwrap();
    s.disk()
        .corrupt_durable(slot.sb.map_roots.pba as usize * BLOCK_SIZE + 20, &[0xEE; 4]);
    let boot = s.boot().unwrap();
    assert!(boot.rewound && boot.cp_seq < snap_cp, "{boot:?}");
    s.verify_recovery().unwrap();
    let listed: Vec<u64> = s.engine().snapshots().iter().map(|x| x.snap_id).collect();
    assert_eq!(listed, vec![kept]);
    let mut buf = vec![0u8; BLOCK_SIZE];
    assert!(matches!(
        s.engine().snapshot_read(lost, 0, &mut buf),
        Err(otap_cp::EngineError::NoSuchSnapshot { .. })
    ));
    assert!(s.engine().snapshot_diff(lost, None).is_err());
    s.clear_nvfail().unwrap();
    let again = s.snapshot(0).unwrap();
    s.request_fold().unwrap();
    s.verify_snapshots().unwrap();
    s.check_diff(again).unwrap();
    s.shutdown_clean().unwrap();
    s.boot().unwrap();
    s.verify_recovery().unwrap();
    assert_eq!(s.engine().snapshots().len(), 2);
}

/// Damage under the newest CP's own regions (its delta region, its
/// directory) rewinds to the CP before; damage under a region both
/// candidates share (an index no fold has replaced) refuses the boot.
/// Either way nothing damaged is served.
#[test]
fn damaged_log_and_index_regions_rewind_or_refuse_and_never_serve() {
    // The delta region of the newest CP, then its directory.
    for which in ["delta", "directory"] {
        let mut cfg = small(9500);
        cfg.fold = no_auto_fold();
        let mut s = sim_with(cfg);
        s.boot().unwrap();
        ok(&mut s, 0, 0, 4);
        s.request_cp().unwrap();
        ok(&mut s, 0, 10, 2);
        s.request_cp().unwrap();
        let slot = newest_slot(&mut s);
        let target = match which {
            "delta" => *s.engine().map().delta_roots().last().expect("a delta"),
            _ => slot.sb.cp_log_head,
        };
        s.crash_power().unwrap();
        s.disk()
            .corrupt_durable(target.pba as usize * BLOCK_SIZE + 77, &[0x11; 3]);
        let boot = s.boot().unwrap();
        assert!(
            boot.rewound && boot.cp_seq < slot.sb.cp_seq,
            "{which}: {boot:?}"
        );
        let v = s.verify_recovery().unwrap();
        assert!(v.lost.get(0) && v.nvfail.get(0), "{which}");
    }
    // An index both candidates name.
    let mut cfg = small(9501);
    cfg.fold = no_auto_fold();
    let mut s = sim_with(cfg);
    s.boot().unwrap();
    ok(&mut s, 0, 0, 4);
    s.request_fold().unwrap();
    ok(&mut s, 1, 0, 1);
    s.request_cp().unwrap();
    s.request_cp().unwrap();
    let root = *s.engine().map().volume(0).unwrap().root();
    s.crash_power().unwrap();
    s.disk()
        .corrupt_durable(root.index_pba as usize * BLOCK_SIZE + 8, &[0x22; 4]);
    let refused = s.boot();
    assert!(
        refused.is_err(),
        "a damaged shared index was not refused: {refused:?}"
    );
}

/// A random walk with tiny segments (two stripes), a reordering device
/// with shallow queues, whole-segment discards and frequent folds: freed
/// metadata segments are discarded and reused constantly, and every power
/// cut and daemon crash still recovers a verified prefix (no late discard
/// lands on reused metadata).
#[test]
fn tiny_segments_with_reordered_discards_recover_every_crash() {
    for seed in 9600..9606u64 {
        let mut cfg = small(seed);
        cfg.stripes_per_segment = 2;
        cfg.segments = 40;
        cfg.volumes = vec![(0, PAGE + 64), (1, 48)];
        cfg.fold.max_cps = 2;
        cfg.schedule = Schedule {
            max_per_poll: 2,
            reorder: true,
            queue_depth: 6,
        };
        let sizes = [PAGE + 64, 48];
        let mut s = sim_with(cfg);
        s.boot().unwrap();
        let mut rng = SplitMix64::new(seed);
        let mut snapshots = 0;
        for step in 0..40 {
            match rng.below(10) {
                0..=4 => {
                    let vol = rng.below(2) as u32;
                    let blocks = rng.range(1, 6) as u32;
                    let lba = rng.below(sizes[vol as usize] - u64::from(blocks) + 1);
                    if !s.engine().nvfail_set().get(vol) {
                        ok(&mut s, vol, lba, blocks);
                    }
                }
                5 => {
                    let vol = rng.below(2) as u32;
                    let blocks = rng.range(1, 30) as u32;
                    let lba = rng.below(sizes[vol as usize] - u64::from(blocks) + 1);
                    if !s.engine().nvfail_set().get(vol) {
                        punch_ok(&mut s, vol, lba, blocks, rng.chance(1, 2));
                    }
                }
                6 => {
                    if snapshots < 2 && !s.engine().nvfail_set().get(0) {
                        s.snapshot(0).unwrap();
                        snapshots += 1;
                    } else {
                        s.request_fold().unwrap();
                    }
                }
                7 => {
                    let tear = match rng.below(4) {
                        0 => DROP,
                        1 => WHOLE,
                        2 => TORN,
                        _ => TearPolicy::default(),
                    };
                    power_cut_and_recover(&mut s, tear)
                        .unwrap_or_else(|e| panic!("seed {seed} step {step}: {e}"));
                }
                8 => {
                    s.crash_daemon().unwrap();
                    s.boot().unwrap();
                    s.verify_recovery()
                        .unwrap_or_else(|e| panic!("seed {seed} step {step}: {e}"));
                }
                _ => {
                    s.request_cp().unwrap();
                }
            }
            s.check_disk_prefix()
                .unwrap_or_else(|e| panic!("seed {seed} step {step}: {e}"));
            s.verify_snapshots()
                .unwrap_or_else(|e| panic!("seed {seed} step {step}: {e}"));
        }
        let discarded = s.events.iter().any(|e| {
            matches!(
                e,
                Event::SegmentFreed {
                    discarded: true,
                    ..
                }
            )
        });
        assert!(discarded, "seed {seed}: no segment was discarded");
        s.disk().set_schedule(Schedule::default());
        s.shutdown_clean().unwrap();
        s.boot().unwrap();
        let v = s.verify_recovery().unwrap();
        assert_eq!(v.k, s.model.last_op(), "seed {seed}");
    }
}

/// A seeded walk in precise nvfail mode with volumes created at runtime
/// (multi-page ones among them), punches, snapshots of the new volumes,
/// folds, power cuts under every tear policy, daemon crashes and clean
/// restarts: every recovery verifies against the model, every snapshot
/// reads its image and diffs exactly, and a new volume survives once its
/// creation landed.
#[test]
fn a_precise_walk_with_runtime_volumes_punches_snapshots_and_crashes_recovers() {
    for seed in 9700..9705u64 {
        let mut cfg = small(seed);
        cfg.intent = otap_cp::NvfailIntent::Precise;
        cfg.segments = 64;
        cfg.fold.max_cps = 3;
        let mut s = sim_with(cfg);
        s.boot().unwrap();
        let mut sizes: Vec<(u32, u64)> = vec![(0, 64), (1, 64)];
        let mut rng = SplitMix64::new(seed ^ 0xA11);
        for step in 0..48 {
            let pick = sizes[rng.below(sizes.len() as u64) as usize];
            let (vol, size) = pick;
            let held = s.engine().nvfail_set().get(vol);
            match rng.below(14) {
                0..=4 if !held => {
                    let blocks = rng.range(1, 6).min(size) as u32;
                    let lba = rng.below(size - u64::from(blocks) + 1);
                    ok(&mut s, vol, lba, blocks);
                }
                5 | 6 if !held => {
                    let blocks = rng.range(1, 40).min(size) as u32;
                    let lba = rng.below(size - u64::from(blocks) + 1);
                    punch_ok(&mut s, vol, lba, blocks, rng.chance(1, 2));
                }
                7 if sizes.len() < 4 => {
                    let new = sizes.len() as u32 + 3;
                    let size = if rng.chance(1, 2) { PAGE + 40 } else { 24 };
                    s.create_volume(new, size).unwrap();
                    sizes.push((new, size));
                }
                8 if !held => {
                    s.snapshot(vol).unwrap();
                }
                9 => {
                    s.request_fold().unwrap();
                }
                10 => {
                    let tear = match rng.below(4) {
                        0 => DROP,
                        1 => WHOLE,
                        2 => TORN,
                        _ => TearPolicy::default(),
                    };
                    power_cut_and_recover(&mut s, tear)
                        .unwrap_or_else(|e| panic!("seed {seed} step {step}: {e}"));
                }
                11 => {
                    s.crash_daemon().unwrap();
                    s.boot().unwrap();
                    s.verify_recovery()
                        .unwrap_or_else(|e| panic!("seed {seed} step {step}: {e}"));
                }
                12 => {
                    s.shutdown_clean().unwrap();
                    s.boot().unwrap();
                    s.verify_recovery()
                        .unwrap_or_else(|e| panic!("seed {seed} step {step}: {e}"));
                }
                _ => {
                    s.request_cp().unwrap();
                }
            }
            s.check_disk_prefix()
                .unwrap_or_else(|e| panic!("seed {seed} step {step}: {e}"));
            s.verify_snapshots()
                .unwrap_or_else(|e| panic!("seed {seed} step {step}: {e}"));
            let ids: Vec<u64> = s.snaps.iter().map(|x| x.snap_id).collect();
            for id in ids {
                s.check_diff(id)
                    .unwrap_or_else(|e| panic!("seed {seed} step {step} snapshot {id}: {e}"));
            }
        }
        s.shutdown_clean().unwrap();
        s.boot().unwrap();
        let v = s.verify_recovery().unwrap();
        assert_eq!(v.k, s.model.last_op(), "seed {seed}");
    }
}

/// Found by the Step 4 review (defect 4, fixed): under the E19 trust policy, a newest CP whose stripe kept its
/// summary but lost a data block is selected as good.
///
/// E21 promises that after a power loss that loses the controller's cache
/// the engine "either boots dirty with every volume quarantined behind a
/// consistent prefix, or refuses to boot, never serves a torn image". The
/// boot's check of the newest CP's stripes (`boot::load_cp`) reads only
/// each stripe's summary (`StripeSummary::parse_expected`), never the data
/// blocks, whose CRCs the descriptors carry. A controller cache loses whole
/// cache lines, so a stripe's first 4 KiB (the summary) can land while a
/// later data block does not: the CP is selected, and the image is not a
/// prefix (the lost block reads `Corrupt` forever, where a rewind to the CP
/// before would have served a consistent image). The right behaviour on a
/// profile that trusts an opaque barrier: verify the newest CP's data
/// blocks against their descriptors as well (one CP's worth of reads at
/// boot) and rewind when one fails.
#[test]
fn a_trusted_opaque_cp_that_lost_a_data_block_is_rewound() {
    let mut cfg = small(9012);
    cfg.flush_opaque = true;
    cfg.flush_policy = otap_cp::FlushPolicy::TrustBarrierWithNvfail;
    cfg.cache_protected = true;
    cfg.fold = no_auto_fold();
    let mut s = sim_with(cfg);
    s.boot().unwrap();
    ok(&mut s, 0, 0, 4);
    s.request_cp().unwrap();
    ok(&mut s, 1, 8, 3);
    s.request_cp().unwrap();
    // The cache line holding block 9 of volume 1 never reached the medium.
    let e = s.engine().map_entry(1, 9).expect("mapped");
    let layout = *s.engine().layout();
    let pba = layout.block_pba(e.lseg(), e.off()).expect("on the device");
    s.crash_power().unwrap();
    s.disk()
        .corrupt_durable(pba as usize * BLOCK_SIZE, &vec![0u8; BLOCK_SIZE]);
    let boot = s.boot().unwrap();
    let verdict = s.verify_recovery();
    // Before the fix: no rewind; the recovery check meets a `Corrupt` block.
    assert!(boot.rewound && verdict.is_ok(), "{boot:?}: {verdict:?}");
}

/// Found by the Step 4 review (defect 2, fixed): a rewind below the CP that created a volume re-creates it empty and
/// serves it without nvfail, after writes to it were acknowledged.
///
/// A volume created at runtime serves once the CP carrying its root
/// commits. When a boot then rewinds below that CP (the newest CP torn:
/// media damage, or E19's lost controller cache), the volume is absent from
/// the selected CP; the control plane still lists it, so `assemble` marks
/// it `creating` and the boot commit creates it anew, empty. The nvfail
/// hold skips it (`v.present && !v.creating`), and it is in no write-active
/// set of the selected CP, so the writes the client was told were done are
/// gone and the volume serves zeros without an error: a silent loss, which
/// nvfail exists to prevent. The right behaviour: a volume the control
/// plane knows that a rewind made absent is held (cause `TornCp`) like the
/// volumes whose writes the rewind lost, until the operator clears it.
#[test]
fn a_rewind_below_a_volumes_creation_holds_it() {
    let mut s = sim_with(small(9081));
    s.boot().unwrap();
    ok(&mut s, 0, 0, 2);
    s.request_cp().unwrap();
    s.create_volume(2, 64).unwrap();
    let created_at = s.engine().cp_seq();
    // Acknowledged writes to the new volume, still in the arena.
    ok(&mut s, 2, 0, 4);
    ok(&mut s, 2, 10, 2);
    let slot = newest_slot(&mut s);
    assert_eq!(slot.sb.cp_seq, created_at, "the creation CP is the newest");
    s.crash_power().unwrap();
    // The newest CP does not verify any more: the boot must rewind.
    s.disk()
        .corrupt_durable(slot.sb.map_roots.pba as usize * BLOCK_SIZE + 40, &[0x66; 4]);
    let boot = s.boot().unwrap();
    assert!(boot.rewound && boot.cp_seq < created_at, "{boot:?}");
    // Before the fix: "power loss lost writes on {2} but nvfail holds {0, 1}".
    let held = s.engine().nvfail_set();
    let verdict = s.verify_recovery();
    assert!(
        verdict.is_ok() && held.get(2),
        "volume 2 serves empty without nvfail ({held:?}): {verdict:?}"
    );
}

/// A device whose reads of one range fail a set number of times (a
/// transient error: a path failover, a controller reset, a timeout).
struct FlakyReads {
    inner: otap_sim::SimBackstore,
    range: (u64, u64),
    failures_left: u32,
}

impl Backstore for FlakyReads {
    fn profile(&self) -> &otap_cp::MediaProfile {
        self.inner.profile()
    }

    fn write_stripe(
        &mut self,
        pba: u64,
        image: &[u8],
        tag: otap_cp::Tag,
    ) -> Result<(), otap_cp::Busy> {
        self.inner.write_stripe(pba, image, tag)
    }

    fn write_meta(
        &mut self,
        pba: u64,
        image: &[u8],
        tag: otap_cp::Tag,
    ) -> Result<(), otap_cp::Busy> {
        self.inner.write_meta(pba, image, tag)
    }

    fn flush(&mut self, class: otap_cp::DevClass, tag: otap_cp::Tag) -> Result<(), otap_cp::Busy> {
        self.inner.flush(class, tag)
    }

    fn commit(
        &mut self,
        pba: u64,
        slot: &SuperblockSlot,
        tag: otap_cp::Tag,
    ) -> Result<(), otap_cp::Busy> {
        self.inner.commit(pba, slot, tag)
    }

    fn write_intent(
        &mut self,
        pba: u64,
        slot: &otap_format::IntentSlot,
        tag: otap_cp::Tag,
    ) -> Result<(), otap_cp::Busy> {
        self.inner.write_intent(pba, slot, tag)
    }

    fn read(&mut self, pba: u64, dst: &mut [u8]) -> Result<(), otap_cp::IoError> {
        let end = pba + (dst.len() / BLOCK_SIZE) as u64;
        if self.failures_left > 0 && pba < self.range.1 && end > self.range.0 {
            self.failures_left -= 1;
            return Err(otap_cp::IoError::Device { errno: 5 });
        }
        self.inner.read(pba, dst)
    }

    fn discard_segment(
        &mut self,
        pba: u64,
        bytes: u64,
        tag: otap_cp::Tag,
    ) -> Result<(), otap_cp::Busy> {
        self.inner.discard_segment(pba, bytes, tag)
    }

    fn poll(&mut self, out: &mut Vec<otap_cp::Done>) -> usize {
        self.inner.poll(out)
    }
}

/// Found by the Step 4 review (defect 5, fixed): a transient read error during the boot discards the newest CP for
/// good.
///
/// `boot::load_cp` turns a read error of any region the candidate names
/// into `TornReason::Io`, the same verdict as a region that read back
/// wrong. Each slot copy of the newest CP re-reads the region, so an error
/// that recurs once more (a path failover, a controller reset) makes both
/// copies "torn" and the boot rewinds to the CP before; its boot commit
/// then reuses the newest CP's slot pair, so the rewind is final, and every
/// write that CP carried is lost (held by nvfail, but lost) although nothing
/// on the medium was damaged. (With a single failure the boot takes the
/// `B` copy of the same CP, yet still reports `rewound` and holds every
/// volume.) An unreadable ring, by contrast, fails the boot
/// (`BootError::Io`) so it can be retried. The right behaviour: an I/O
/// error while reading a candidate fails the boot like an unreadable ring
/// (or is retried), and only a region that reads back and does not verify
/// makes a candidate torn.
#[test]
fn a_transient_read_error_at_boot_fails_the_boot_instead_of_rewinding() {
    let cfg = small(9082);
    let ecfg = cfg.engine_config();
    let clock = otap_sim::SimClock::new(1_700_000_000_000_000_000);
    let mut disk = otap_sim::SimBackstore::new(cfg.profile(), cfg.seed).unwrap();
    otap_cp::Engine::<otap_sim::SimBackstore, otap_sim::SimFrontDoor, otap_sim::SimClock>::format(
        &mut disk, &clock, &ecfg,
    )
    .unwrap();
    let memory =
        otap_sys::HugeArena::anonymous(cfg.arena_bytes, otap_sys::HugePageSize::TwoMiB).unwrap();
    type Plain =
        otap_cp::Engine<otap_sim::SimBackstore, otap_sim::SimFrontDoor, otap_sim::SimClock>;
    let mut e = Plain::boot(
        ecfg.clone(),
        disk,
        otap_sim::SimFrontDoor::new(),
        clock.clone(),
        memory,
    )
    .map_err(|f| f.error)
    .unwrap();
    for _ in 0..10_000 {
        if e.is_serving() {
            break;
        }
        e.tick().unwrap();
    }
    for (op, lba) in [(1u64, 0u64), (2, 8)] {
        let id = e
            .front_door()
            .submit_write(0, lba, otap_sim::Model::data_of(op, 0, lba, 4));
        for _ in 0..10_000 {
            let done = e.front_door().take_completions();
            if done.iter().any(|c| c.req.id == id) {
                break;
            }
            e.tick().unwrap();
        }
        let before = e.cp_seq();
        e.request_cp();
        while e.cp_seq() == before {
            e.tick().unwrap();
        }
    }
    let newest = e.cp_seq();
    let (mut disk, _, clock, memory) = e.into_parts();
    disk.crash_power();
    // The newest CP's directory: its two slot copies each read it once,
    // and each read fails once.
    let layout = *disk.layout();
    let mut buf = vec![0u8; SUPERBLOCK_SLOT_SIZE];
    let mut dir = None;
    for slot in SbSlot::ALL {
        disk.read(layout.sb_pba(slot), &mut buf).unwrap();
        if let Ok(s) = SuperblockSlot::read(&buf) {
            if s.sb.cp_seq == newest {
                dir = Some(s.sb.cp_log_head);
            }
        }
    }
    let dir = dir.expect("the newest slot");
    let flaky = FlakyReads {
        inner: disk,
        range: (dir.pba, dir.pba + u64::from(dir.pages)),
        failures_left: 2,
    };
    let memory = otap_sys::HugeArena::anonymous(cfg.arena_bytes, otap_sys::HugePageSize::TwoMiB)
        .unwrap_or(memory);
    type Flaky = otap_cp::Engine<FlakyReads, otap_sim::SimFrontDoor, otap_sim::SimClock>;
    let outcome = Flaky::boot(ecfg, flaky, otap_sim::SimFrontDoor::new(), clock, memory);
    if let Ok(mut e) = outcome {
        let booted = e.take_events().into_iter().find_map(|ev| match ev {
            Event::Booted {
                cp_seq,
                rewound,
                torn,
                ..
            } => Some((cp_seq, rewound, torn)),
            _ => None,
        });
        // Before the fix: (newest - 1, true, [(newest, "Io(Device { errno: 5 })")]).
        panic!("a transient read error rewound the boot past an intact CP {newest}: {booted:?}");
    }
}
