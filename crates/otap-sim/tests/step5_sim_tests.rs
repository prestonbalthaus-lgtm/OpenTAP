//! Step 5 (Phase C): snapshot deletion and pins, volume rollback, resize
//! and deletion, the catalogue, the cleaner and its reserve, the admission
//! rule, pacing, and offline volumes, over the simulator's device with
//! power cuts and daemon crashes in every phase of the consistency points
//! that carry them. Every recovery check audits the live-block accounting
//! against a walk of every tree.

use std::collections::{HashMap, HashSet};

use otap_cp::{
    CleanerPolicy, EngineError, Event, FoldPolicy, Phase, Status, MAX_RESERVED_SNAPSHOTS,
    MAX_USER_SNAPSHOTS,
};
use otap_format::{SnapKind, SnapOrigin, Uuid, BLOCK_SIZE, PIN_REPLICATION, PIN_USER};
use otap_sim::{Schedule, Sim, SimConfig, SplitMix64, Stop, TearPolicy};
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

fn small(seed: u64) -> SimConfig {
    SimConfig::small(seed)
}

fn sim_with(cfg: SimConfig) -> Sim {
    Sim::new(cfg).expect("the pool formats")
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

/// The pool's state and the engine's complaints, for a failure message.
fn diagnostics(s: &mut Sim) -> String {
    let status = s.engine().pool_status();
    let stats = s.engine().stats();
    let skipped: Vec<String> = s
        .events
        .iter()
        .filter_map(|e| match e {
            Event::CleanSkipped { pseg, reason } => Some(format!("{pseg}: {reason}")),
            _ => None,
        })
        .collect();
    let cleaned = count(s, |e| matches!(e, Event::SegmentCleaned { .. }));
    let freed = count(s, |e| matches!(e, Event::SegmentFreed { .. }));
    let failed: Vec<String> = s
        .events
        .iter()
        .filter_map(|e| match e {
            Event::CpFailed { cp_seq, reason } => Some(format!("{cp_seq}: {reason}")),
            Event::Internal { what } => Some(format!("internal: {what}")),
            _ => None,
        })
        .collect();
    format!(
        "{status:#?}\n{stats:#?}\ncleaned {cleaned}, freed {freed}, skipped {skipped:?}, failed \
         {failed:?}"
    )
}

fn ok(s: &mut Sim, vol: u32, lba: u64, blocks: u32) {
    let c = s.write(vol, lba, blocks).expect("the write completes");
    if c.status != Status::Ok {
        let why = diagnostics(s);
        panic!("write {vol}/{lba}+{blocks}: {:?}\n{why}", c.status);
    }
}

fn punch_ok(s: &mut Sim, vol: u32, lba: u64, blocks: u32) {
    let c = s
        .punch(vol, lba, blocks, false)
        .expect("the punch completes");
    assert_eq!(c.status, Status::Ok, "punch {vol}/{lba}+{blocks}");
}

fn power_cut_and_recover(s: &mut Sim, tear: TearPolicy) -> Result<otap_sim::Verdict, String> {
    s.disk().set_tear_policy(tear);
    s.crash_power()?;
    s.boot()?;
    let v = s.verify_recovery()?;
    s.clear_nvfail()?;
    Ok(v)
}

fn daemon_crash_and_recover(s: &mut Sim) -> Result<otap_sim::Verdict, String> {
    s.crash_daemon()?;
    s.boot()?;
    s.verify_recovery()
}

fn clean_restart(s: &mut Sim) -> Result<otap_sim::Verdict, String> {
    s.shutdown_clean()?;
    s.boot()?;
    let v = s.verify_recovery()?;
    if v.k != s.model.last_op() {
        return Err(format!(
            "a clean restart kept a prefix of {} of {} operations",
            v.k,
            s.model.last_op()
        ));
    }
    Ok(v)
}

/// Every check the harness offers, at once.
fn check_everything(s: &mut Sim) -> Result<(), String> {
    s.check_disk_prefix()?;
    s.verify_snapshots()?;
    let ids: Vec<u64> = s.snaps.iter().map(|x| x.snap_id).collect();
    for id in ids {
        s.check_diff(id)?;
    }
    s.audit_liveness()
}

/// Ticks until the CP state machine is in `phase`, under a slow device.
fn tick_to_phase(s: &mut Sim, phase: Phase) -> Result<(), String> {
    s.disk().set_schedule(fifo1());
    s.tick_until(|e| e.phase() == phase, "the CP phase")
}

fn count<F: Fn(&Event) -> bool>(s: &Sim, f: F) -> usize {
    s.events.iter().filter(|e| f(e)).count()
}

// ---------------------------------------------------------------------------
// Snapshot deletion and pins
// ---------------------------------------------------------------------------

/// Three states of one volume pinned by two snapshots: deleting the older
/// releases exactly the blocks it alone held (the versions overwritten
/// between the two snapshots), deleting the newer releases the versions
/// overwritten since; the live image and the surviving snapshot read on,
/// and the accounting is exact after every step and after a crash.
#[test]
fn deleting_a_snapshot_releases_only_what_it_alone_referenced() {
    let mut cfg = small(5001);
    cfg.volumes = vec![(0, 64)];
    cfg.fold = no_auto_fold();
    cfg.cleaner.background = false;
    let mut s = sim_with(cfg);
    s.boot().unwrap();
    ok(&mut s, 0, 0, 16); // v1 of blocks 0..16
    let s1 = s.snapshot(0).unwrap();
    ok(&mut s, 0, 4, 4); // v2 of 4..8: v1 of 4..8 is now S1's alone
    ok(&mut s, 0, 20, 2); // new blocks
    let s2 = s.snapshot(0).unwrap();
    ok(&mut s, 0, 6, 4); // v3 of 6..10: v2 of 6..8 and v1 of 8..10 are S2's alone
    s.request_cp().unwrap();
    check_everything(&mut s).unwrap();
    let released = s.delete_snapshot(s1).unwrap();
    assert_eq!(released, 4, "v1 of blocks 4..8");
    check_everything(&mut s).unwrap();
    assert!(s.engine().snapshots().iter().all(|x| x.snap_id != s1));
    assert!(matches!(
        s.engine().snapshot_read(s1, 0, &mut vec![0u8; BLOCK_SIZE]),
        Err(EngineError::NoSuchSnapshot { .. })
    ));
    daemon_crash_and_recover(&mut s).unwrap();
    check_everything(&mut s).unwrap();
    let released = s.delete_snapshot(s2).unwrap();
    assert_eq!(released, 4, "v2 of 6..8 and v1 of 8..10");
    check_everything(&mut s).unwrap();
    assert!(s.engine().snapshots().is_empty());
    // A snapshot of nothing in particular: the blocks it shares with the
    // live tree stay.
    let s3 = s.snapshot(0).unwrap();
    assert_eq!(s.delete_snapshot(s3).unwrap(), 0);
    check_everything(&mut s).unwrap();
    let v = clean_restart(&mut s).unwrap();
    assert!(!v.boot.rewound);
    assert!(matches!(
        s.engine().delete_snapshot(s3),
        Err(EngineError::NoSuchSnapshot { .. })
    ));
}

/// A replication base refuses deletion until the base moves on; pins are
/// durable (they survive a daemon crash) and visible in the listing.
#[test]
fn a_replication_base_refuses_deletion_until_superseded_or_unpinned() {
    let mut cfg = small(5002);
    cfg.volumes = vec![(0, 64), (1, 32)];
    let mut s = sim_with(cfg);
    s.boot().unwrap();
    ok(&mut s, 0, 0, 8);
    let s1 = s.snapshot(0).unwrap();
    ok(&mut s, 0, 0, 8);
    let s2 = s.snapshot(0).unwrap();
    s.engine().set_replication_base(0, Some(s1)).unwrap();
    s.request_cp().unwrap();
    let flags = |s: &mut Sim, id: u64| {
        s.engine()
            .snapshots()
            .into_iter()
            .find(|x| x.snap_id == id)
            .map(|x| x.flags)
            .unwrap()
    };
    assert_eq!(flags(&mut s, s1), PIN_REPLICATION);
    assert!(matches!(
        s.engine().delete_snapshot(s1),
        Err(EngineError::SnapshotPinned {
            flags: PIN_REPLICATION,
            ..
        })
    ));
    assert_eq!(
        s.engine().delete_snapshot(s1).unwrap_err().to_string(),
        format!(
            "snapshot {s1} is pinned (flags 0x04); unpin it or supersede the replication base \
             before deleting it"
        )
    );
    daemon_crash_and_recover(&mut s).unwrap();
    assert_eq!(flags(&mut s, s1), PIN_REPLICATION, "the pin survived");
    // Superseded by the newer committed base: S1 is deletable, S2 is not.
    s.engine().set_replication_base(0, Some(s2)).unwrap();
    s.request_cp().unwrap();
    assert_eq!(flags(&mut s, s1), 0);
    assert_eq!(flags(&mut s, s2), PIN_REPLICATION);
    s.delete_snapshot(s1).unwrap();
    assert!(matches!(
        s.engine().delete_snapshot(s2),
        Err(EngineError::SnapshotPinned { .. })
    ));
    // An operator's pin likewise, until unpinned.
    s.engine()
        .set_snapshot_pins(s2, PIN_REPLICATION | PIN_USER)
        .unwrap();
    s.request_cp().unwrap();
    assert_eq!(flags(&mut s, s2), PIN_REPLICATION | PIN_USER);
    s.engine().set_replication_base(0, None).unwrap();
    s.request_cp().unwrap();
    assert_eq!(flags(&mut s, s2), PIN_USER);
    assert!(matches!(
        s.engine().delete_snapshot(s2),
        Err(EngineError::SnapshotPinned {
            flags: PIN_USER,
            ..
        })
    ));
    s.engine().set_snapshot_pins(s2, 0).unwrap();
    s.delete_snapshot(s2).unwrap();
    assert!(matches!(
        s.engine().set_snapshot_pins(99, PIN_USER),
        Err(EngineError::NoSuchSnapshot { snap_id: 99 })
    ));
    assert!(matches!(
        s.engine().set_replication_base(0, Some(s1)),
        Err(EngineError::NoSuchSnapshot { .. })
    ));
    check_everything(&mut s).unwrap();
    clean_restart(&mut s).unwrap();
}

/// A power cut in each phase of the CP that deletes a snapshot, under
/// every tear policy: the snapshot is either still there, whole, or gone
/// for good; nothing live is lost, nothing deleted comes back, and the
/// accounting is exact after every boot.
#[test]
fn snapshot_deletion_survives_a_power_cut_in_every_phase() {
    let mut seed = 5100u64;
    for tear in [DROP, WHOLE, TORN, TearPolicy::default()] {
        for phase in [
            Phase::Staging,
            Phase::Metadata,
            Phase::Flushing,
            Phase::CommitA,
            Phase::CommitB,
        ] {
            seed += 1;
            let mut cfg = small(seed);
            cfg.volumes = vec![(0, 64)];
            cfg.fold = no_auto_fold();
            cfg.cleaner.background = false;
            let mut s = sim_with(cfg);
            s.boot().unwrap();
            ok(&mut s, 0, 0, 24);
            let s1 = s.snapshot(0).unwrap();
            ok(&mut s, 0, 8, 8);
            let s2 = s.snapshot(0).unwrap();
            ok(&mut s, 0, 16, 8);
            s.request_cp().unwrap();
            // Something to stage in the deleting CP, so every phase exists.
            ok(&mut s, 0, 40, 4);
            s.request_snapshot_deletion(s1).unwrap();
            s.engine().request_cp();
            if tick_to_phase(&mut s, phase).is_err() {
                // The CP went through before the slow device bit; the
                // deletion landed and the check below holds anyway.
            }
            s.disk().set_tear_policy(tear);
            s.crash_power().unwrap();
            s.disk().set_schedule(Schedule::default());
            s.boot().unwrap();
            s.verify_recovery()
                .unwrap_or_else(|e| panic!("seed {seed} {phase:?} {tear:?}: {e}"));
            s.clear_nvfail().unwrap();
            check_everything(&mut s).unwrap();
            let listed: Vec<u64> = s.engine().snapshots().iter().map(|x| x.snap_id).collect();
            assert!(listed.contains(&s2), "S2 was never asked to go");
            if listed.contains(&s1) {
                // Ask again: it goes this time.
                s.delete_snapshot(s1).unwrap();
            }
            assert!(!s.engine().snapshots().iter().any(|x| x.snap_id == s1));
            ok(&mut s, 0, 0, 4);
            check_everything(&mut s).unwrap();
            clean_restart(&mut s).unwrap_or_else(|e| panic!("seed {seed}: {e}"));
        }
    }
}

/// 255 user snapshots per volume, 16 baselines besides, counted with the
/// ones still waiting for their CP; another volume has its own budget.
#[test]
fn the_user_snapshot_limit_is_255_and_baselines_have_their_own_slots() {
    let mut cfg = small(5003);
    cfg.volumes = vec![(0, 16), (1, 16)];
    cfg.cleaner.background = false;
    let mut s = sim_with(cfg);
    s.boot().unwrap();
    ok(&mut s, 0, 0, 1);
    let mut uuid = 1000u64;
    let mut make = |s: &mut Sim, vol: u32, origin: SnapOrigin| -> Result<(), EngineError> {
        uuid += 1;
        let mut b = [0u8; 16];
        b[..8].copy_from_slice(&uuid.to_le_bytes());
        s.engine()
            .create_snapshot(vol, Uuid::from_bytes(b), SnapKind::Crash, origin)
    };
    for i in 0..MAX_USER_SNAPSHOTS {
        let origin = if i % 2 == 0 {
            SnapOrigin::User
        } else {
            SnapOrigin::Scheduled
        };
        make(&mut s, 0, origin).unwrap_or_else(|e| panic!("snapshot {i}: {e}"));
        if i % 32 == 31 {
            // The pending requests ride one fold.
            s.request_cp().unwrap();
            s.tick_until(|e| e.snapshots().len() == i + 1, "the batch of snapshots")
                .unwrap();
        }
    }
    assert!(matches!(
        make(&mut s, 0, SnapOrigin::User),
        Err(EngineError::SnapshotLimit {
            vol: 0,
            limit: MAX_USER_SNAPSHOTS
        })
    ));
    // Baselines have their own slots, on this volume and on the other.
    for _ in 0..MAX_RESERVED_SNAPSHOTS {
        make(&mut s, 0, SnapOrigin::Transfer).unwrap();
    }
    assert!(matches!(
        make(&mut s, 0, SnapOrigin::Divergent),
        Err(EngineError::SnapshotLimit {
            vol: 0,
            limit: MAX_RESERVED_SNAPSHOTS
        })
    ));
    make(&mut s, 1, SnapOrigin::User).unwrap();
    s.request_cp().unwrap();
    s.tick_until(
        |e| e.snapshots().len() == MAX_USER_SNAPSHOTS + MAX_RESERVED_SNAPSHOTS + 1,
        "every snapshot",
    )
    .unwrap();
    // Deleting one frees a slot.
    let victim = s.engine().snapshots()[0].snap_id;
    s.engine().delete_snapshot(victim).unwrap();
    s.tick_until(
        |e| e.snapshots().iter().all(|x| x.snap_id != victim),
        "the deletion",
    )
    .unwrap();
    make(&mut s, 0, SnapOrigin::User).unwrap();
    s.request_cp().unwrap();
    s.tick_until(
        |e| e.snapshots().len() == MAX_USER_SNAPSHOTS + MAX_RESERVED_SNAPSHOTS + 1,
        "the replacement snapshot",
    )
    .unwrap();
    s.audit_liveness().unwrap();
    daemon_crash_and_recover(&mut s).unwrap();
    assert_eq!(
        s.engine().snapshots().len(),
        MAX_USER_SNAPSHOTS + MAX_RESERVED_SNAPSHOTS + 1
    );
}

// ---------------------------------------------------------------------------
// Rollback
// ---------------------------------------------------------------------------

/// A rollback needs the volume offline and quiet and refuses with newer
/// snapshots around; done, the volume reads as the snapshot did, every
/// block written since is released, the rollback generation moved on, and
/// the state survives crashes.
#[test]
fn a_rollback_restores_the_snapshot_and_moves_the_generation() {
    let mut cfg = small(5010);
    cfg.volumes = vec![(0, 64), (1, 32)];
    cfg.cleaner.background = false;
    let mut s = sim_with(cfg);
    s.boot().unwrap();
    ok(&mut s, 0, 0, 16);
    ok(&mut s, 1, 0, 4);
    let s1 = s.snapshot(0).unwrap();
    ok(&mut s, 0, 4, 8);
    punch_ok(&mut s, 0, 0, 2);
    ok(&mut s, 0, 30, 6);
    ok(&mut s, 1, 4, 4);
    let s2 = s.snapshot(0).unwrap();
    ok(&mut s, 0, 40, 2);
    let gen_before = s.engine().volume(0).unwrap().rollback_gen;
    // Refusals: online, newer snapshots, another volume's snapshot, a
    // snapshot being deleted.
    assert!(matches!(
        s.engine().rollback_volume(0, s1),
        Err(EngineError::VolumeOnline { vol: 0 })
    ));
    s.quiesce(0).unwrap();
    assert!(matches!(
        s.engine().rollback_volume(0, s1),
        Err(EngineError::NewerSnapshots { vol: 0, count: 1 })
    ));
    assert!(matches!(
        s.engine().rollback_volume(1, s2),
        Err(EngineError::NoSuchSnapshot { .. })
    ));
    // Offline, the volume answers `Offline`; the other serves.
    let c = s.write(0, 0, 1).unwrap();
    assert_eq!(c.status, Status::Offline);
    assert_eq!(s.read(0, 0, 1).unwrap().0, Status::Offline);
    ok(&mut s, 1, 8, 2);
    s.delete_snapshot(s2).unwrap();
    s.rollback(0, s1).unwrap();
    assert_eq!(
        count(&s, |e| matches!(e, Event::VolumeRolledBack { .. })),
        1
    );
    let v = s.engine().volume(0).unwrap();
    assert!(v.offline && v.op_pending.is_none());
    assert_eq!(v.rollback_gen, gen_before + 1);
    s.set_online(0, true).unwrap();
    // The image is the snapshot's: v1 at 0..16 (the punch and the
    // overwrite are gone), nothing at 30..36 and 40..42.
    check_everything(&mut s).unwrap();
    s.read(0, 0, 64).unwrap();
    assert_eq!(s.check_diff(s1).unwrap(), 0, "nothing differs from S1");
    // Writes go on, and the snapshot is untouched by them.
    ok(&mut s, 0, 2, 4);
    s.request_cp().unwrap();
    check_everything(&mut s).unwrap();
    assert_eq!(s.check_diff(s1).unwrap(), 4);
    daemon_crash_and_recover(&mut s).unwrap();
    assert_eq!(s.engine().volume(0).unwrap().rollback_gen, gen_before + 1);
    check_everything(&mut s).unwrap();
    power_cut_and_recover(&mut s, TORN).unwrap();
    check_everything(&mut s).unwrap();
    // Rolling back again releases the writes since; a dirty volume is
    // refused until its writes reach a CP (the harness quiesces).
    ok(&mut s, 0, 50, 3);
    s.set_online(0, false).unwrap();
    assert!(matches!(
        s.engine().rollback_volume(0, s1),
        Err(EngineError::VolumeDirty { vol: 0 })
    ));
    s.set_online(0, true).unwrap();
    s.rollback(0, s1).unwrap();
    s.set_online(0, true).unwrap();
    check_everything(&mut s).unwrap();
    let v = clean_restart(&mut s).unwrap();
    assert!(!v.boot.rewound);
    check_everything(&mut s).unwrap();
}

/// A power cut in each phase of the CP that rolls a volume back, under
/// every tear policy: the volume is either rolled back or exactly as it
/// was, which the model tells from the image; the generation moved iff
/// the rollback landed.
#[test]
fn a_rollback_with_a_power_cut_in_every_phase_lands_whole_or_not_at_all() {
    let mut seed = 5200u64;
    for tear in [DROP, WHOLE, TORN, TearPolicy::default()] {
        for phase in [
            Phase::Staging,
            Phase::Metadata,
            Phase::Flushing,
            Phase::CommitA,
            Phase::CommitB,
        ] {
            seed += 1;
            let mut cfg = small(seed);
            cfg.volumes = vec![(0, 64), (1, 32)];
            cfg.cleaner.background = false;
            let mut s = sim_with(cfg);
            s.boot().unwrap();
            ok(&mut s, 0, 0, 24);
            let s1 = s.snapshot(0).unwrap();
            ok(&mut s, 0, 8, 8);
            punch_ok(&mut s, 0, 20, 4);
            ok(&mut s, 0, 40, 4);
            s.request_cp().unwrap();
            let gen_before = s.engine().volume(0).unwrap().rollback_gen;
            // Another volume's write rides the rollback's CP.
            ok(&mut s, 1, 0, 4);
            let op = s.request_rollback(0, s1).unwrap();
            if tick_to_phase(&mut s, phase).is_err() {
                // Already through.
            }
            s.disk().set_tear_policy(tear);
            s.crash_power().unwrap();
            s.disk().set_schedule(Schedule::default());
            s.boot().unwrap();
            let v = s
                .verify_recovery()
                .unwrap_or_else(|e| panic!("seed {seed} {phase:?} {tear:?}: {e}"));
            let landed = v.k >= op;
            let gen = s.engine().volume(0).unwrap().rollback_gen;
            assert_eq!(
                gen,
                if landed { gen_before + 1 } else { gen_before },
                "seed {seed} {phase:?} {tear:?}: landed {landed}, prefix {}",
                v.k
            );
            assert!(s.engine().volume(0).unwrap().offline, "offline either way");
            s.clear_nvfail().unwrap();
            if !landed {
                // Nothing of it remains: ask again and it lands.
                s.rollback(0, s1).unwrap();
            }
            s.set_online(0, true).unwrap();
            check_everything(&mut s).unwrap();
            assert_eq!(s.check_diff(s1).unwrap(), 0);
            ok(&mut s, 0, 1, 1);
            clean_restart(&mut s).unwrap_or_else(|e| panic!("seed {seed}: {e}"));
            check_everything(&mut s).unwrap();
        }
    }
}

// ---------------------------------------------------------------------------
// The cleaner and the reserve
// ---------------------------------------------------------------------------

/// A pool whose data is a fraction of its segments, rewritten block by
/// block for a long time: the dead blocks it leaves behind are reclaimed
/// by the cleaner, writes never stop, no CP ever fails for space, and the
/// accounting is exact after every CP, after crashes and at the end.
#[test]
fn the_cleaner_reclaims_a_fragmented_pool_so_writes_never_stop() {
    let mut cfg = small(5300);
    cfg.segments = 48; // 8 reserved, 256 KiB each: 40 for 2,240 blocks
    cfg.stripes_per_segment = 8;
    cfg.volumes = vec![(0, 900)]; // ~40% of the data space, once written
    cfg.fold.max_cps = 6;
    let mut s = sim_with(cfg);
    s.boot().unwrap();
    let mut rng = SplitMix64::new(5300);
    for round in 0..60 {
        for _ in 0..40 {
            let lba = rng.below(900);
            let blocks = rng.range(1, 4) as u32;
            let lba = lba.min(900 - u64::from(blocks));
            ok(&mut s, 0, lba, blocks);
        }
        s.request_cp().unwrap();
        s.audit_liveness()
            .unwrap_or_else(|e| panic!("round {round}: {e}"));
        match round % 10 {
            3 => {
                daemon_crash_and_recover(&mut s).unwrap();
            }
            7 => {
                power_cut_and_recover(&mut s, TORN).unwrap();
            }
            _ => {}
        }
    }
    // Counted over the whole run (the engine's counters restart with every
    // instance).
    let cleaned = count(&s, |e| matches!(e, Event::SegmentCleaned { .. }));
    assert!(cleaned > 0, "segments were cleaned: {cleaned}");
    assert!(s
        .events
        .iter()
        .any(|e| matches!(e, Event::SegmentCleaned { relocated, .. } if *relocated > 0)));
    assert!(
        !s.events
            .iter()
            .any(|e| matches!(e, Event::CpFailed { reason, .. } if reason.contains("segment"))),
        "no CP failed for space"
    );
    let status = s.engine().pool_status();
    assert!(status.free_segments >= status.reserve_segments);
    check_everything(&mut s).unwrap();
    clean_restart(&mut s).unwrap();
    check_everything(&mut s).unwrap();
}

/// Blocks a snapshot still references are moved with their pages: after
/// the cleaner drains the segments they were in, the snapshot reads the
/// same image, the live map reads the newer writes, and a power cut in
/// any phase of a cleaning CP leaves everything consistent.
#[test]
fn the_cleaner_moves_blocks_snapshots_reference_and_their_pages_follow() {
    let mut seed = 5400u64;
    for tear in [DROP, WHOLE, TORN, TearPolicy::default()] {
        for phase in [
            Phase::Staging,
            Phase::Metadata,
            Phase::Flushing,
            Phase::CommitA,
            Phase::CommitB,
        ] {
            seed += 1;
            let mut cfg = small(seed);
            cfg.segments = 48;
            cfg.volumes = vec![(0, 400)];
            cfg.fold = no_auto_fold();
            cfg.cleaner.background = false;
            let mut s = sim_with(cfg);
            s.boot().unwrap();
            // Fill: every block written (v1); most overwritten (v2) before
            // any snapshot, so the first segments are three quarters dead;
            // then a snapshot, which pins the v1 blocks left; then those
            // are overwritten too (v3), so the first segments hold a few
            // snapshot-only blocks among the dead.
            for lba in (0..400).step_by(8) {
                ok(&mut s, 0, lba, 8);
            }
            s.request_cp().unwrap();
            for lba in (0..400).step_by(8) {
                if lba % 32 != 0 {
                    ok(&mut s, 0, lba, 8);
                }
            }
            s.request_cp().unwrap();
            let s1 = s.snapshot(0).unwrap();
            for lba in (0..400).step_by(32) {
                ok(&mut s, 0, lba, 8);
            }
            s.request_cp().unwrap();
            check_everything(&mut s).unwrap();
            // The cleaning CP, cut by a power cut in `phase`.
            s.engine().request_clean();
            ok(&mut s, 0, 0, 1);
            if tick_to_phase(&mut s, phase).is_err() {
                // Already through.
            }
            s.disk().set_tear_policy(tear);
            s.crash_power().unwrap();
            s.disk().set_schedule(Schedule::default());
            s.boot().unwrap();
            s.verify_recovery()
                .unwrap_or_else(|e| panic!("seed {seed} {phase:?} {tear:?}: {e}"));
            s.clear_nvfail().unwrap();
            check_everything(&mut s).unwrap();
            // Finish the cleaning, then check that something moved.
            s.request_clean().unwrap();
            s.request_clean().unwrap();
            if count(&s, |e| matches!(e, Event::SegmentCleaned { .. })) == 0 {
                let why = diagnostics(&mut s);
                panic!("seed {seed} {phase:?} {tear:?}: the cleaner drained nothing\n{why}");
            }
            check_everything(&mut s).unwrap();
            assert!(s.check_diff(s1).unwrap() > 0);
            // The drained segments come free after the deferral.
            for _ in 0..3 {
                s.request_cp().unwrap();
            }
            assert!(count(&s, |e| matches!(e, Event::SegmentFreed { .. })) > 0);
            check_everything(&mut s).unwrap();
            daemon_crash_and_recover(&mut s).unwrap();
            check_everything(&mut s).unwrap();
            s.delete_snapshot(s1).unwrap();
            check_everything(&mut s).unwrap();
            clean_restart(&mut s).unwrap_or_else(|e| panic!("seed {seed}: {e}"));
        }
    }
}

/// A pool full for users (the admission rule refuses writes with
/// `NoSpace` while the reserve stays free, and no CP fails for space)
/// still punches holes, deletes a snapshot and cleans; the space comes
/// back and writes resume, before and after a reboot.
#[test]
fn a_pool_full_for_users_still_punches_deletes_and_cleans_then_serves_writes_again() {
    let mut cfg = small(5500);
    cfg.segments = 36; // 8 reserved
    cfg.volumes = vec![(0, 2048)];
    cfg.fold.max_cps = 4;
    let mut s = sim_with(cfg);
    s.boot().unwrap();
    let reserve = s.engine().reserve_segments();
    assert_eq!(reserve, 8);
    // Write fresh blocks until the admission rule says no even right after
    // a consistency point (the rule counts the arena's unflushed blocks, so
    // a refusal before a CP may be followed by room after it).
    let mut lba = 0u64;
    let mut refused = None;
    while lba + 8 <= 2048 {
        let c = s.write(0, lba, 8).unwrap();
        if c.status == Status::NoSpace {
            s.request_cp().unwrap();
            if s.write(0, lba, 8).unwrap().status == Status::NoSpace {
                refused = Some(lba);
                break;
            }
        }
        lba += 8;
        if lba.is_multiple_of(64) {
            s.request_cp().unwrap();
        }
    }
    let first_refused = refused.expect("the pool filled");
    let status = s.engine().pool_status();
    // The reserve is for the metadata of folds and the cleaner: the folds
    // of the fill may have taken some of it, never the floor the boot and
    // shutdown commits need.
    assert!(
        status.free_segments > otap_cp::META_RESERVE_SEGMENTS,
        "{status:?}"
    );
    assert!(!status.pool_full, "no CP failed: {status:?}");
    assert!(
        !s.events.iter().any(|e| matches!(e, Event::CpFailed { .. })),
        "no CP failed for space"
    );
    check_everything(&mut s).unwrap();
    // Still refused, but a snapshot of the full volume, a punch and a
    // deletion all go through.
    assert_eq!(
        s.write(0, first_refused, 8).unwrap().status,
        Status::NoSpace
    );
    let s1 = s.snapshot(0).unwrap();
    punch_ok(&mut s, 0, 0, 256);
    s.request_cp().unwrap();
    check_everything(&mut s).unwrap();
    // Nothing is free yet: the snapshot pins what the punch released.
    assert_eq!(
        s.write(0, first_refused, 8).unwrap().status,
        Status::NoSpace
    );
    s.delete_snapshot(s1).unwrap();
    check_everything(&mut s).unwrap();
    // The cleaner drains the segments the punch emptied; the deferral
    // frees them; writes resume.
    for _ in 0..6 {
        s.request_clean().unwrap();
    }
    assert!(s.engine().pool_status().admissible_blocks > 0);
    ok(&mut s, 0, first_refused, 8);
    check_everything(&mut s).unwrap();
    // The same after a reboot of the full pool.
    for lba in (first_refused..2048).step_by(8) {
        if s.write(0, lba, 8).unwrap().status == Status::NoSpace {
            break;
        }
    }
    daemon_crash_and_recover(&mut s).unwrap();
    check_everything(&mut s).unwrap();
    punch_ok(&mut s, 0, 256, 512);
    for _ in 0..6 {
        s.request_clean().unwrap();
    }
    ok(&mut s, 0, 300, 8);
    check_everything(&mut s).unwrap();
    clean_restart(&mut s).unwrap();
}

/// A block relocated by the cleaner never clobbers a newer write that
/// landed since: whatever the order of the relocation and the write, the
/// newest content wins, before and after crashes.
#[test]
fn relocation_never_clobbers_a_write_that_landed_since() {
    let mut cfg = small(5600);
    cfg.segments = 48;
    cfg.volumes = vec![(0, 300)];
    cfg.fold = no_auto_fold();
    cfg.cleaner.background = false;
    let mut s = sim_with(cfg);
    s.boot().unwrap();
    for lba in (0..300).step_by(10) {
        ok(&mut s, 0, lba, 10);
    }
    s.request_cp().unwrap();
    // Make most of the first segments dead, but keep a few blocks live.
    for lba in (0..300).step_by(10) {
        if lba % 50 != 0 {
            ok(&mut s, 0, lba, 10);
        }
    }
    s.request_cp().unwrap();
    // The live blocks of the candidates are at 0, 50, 100, ...: write new
    // data to some of them right before (same CP) and during the cleaning
    // CP, and punch one.
    ok(&mut s, 0, 0, 2);
    punch_ok(&mut s, 0, 100, 3);
    s.engine().request_clean();
    s.disk().set_schedule(fifo1());
    s.tick_until(|e| e.phase() == Phase::Staging, "staging")
        .unwrap();
    s.ticks(5).unwrap();
    let id = s.submit_write(0, 50, 2);
    s.disk().set_schedule(Schedule::default());
    let c = s.wait(id, 50_000).unwrap().expect("the write completes");
    assert_eq!(c.status, Status::Ok);
    s.settle().unwrap();
    s.request_cp().unwrap();
    check_everything(&mut s).unwrap();
    assert!(s.engine().stats().relocated > 0);
    s.read(0, 0, 300).unwrap();
    daemon_crash_and_recover(&mut s).unwrap();
    check_everything(&mut s).unwrap();
    power_cut_and_recover(&mut s, DROP).unwrap();
    check_everything(&mut s).unwrap();
    clean_restart(&mut s).unwrap();
}

/// The reserve is `max(8, segments / 12)` capped at half the pool, the
/// admission rule counts what is acknowledged and not yet flushed, and a
/// pool with the reserve kept always boots and stops cleanly.
#[test]
fn the_admission_rule_keeps_the_reserve_free() {
    assert_eq!(otap_cp::reserve_for(48), 8);
    assert_eq!(otap_cp::reserve_for(120), 10);
    assert_eq!(otap_cp::reserve_for(10), 5);
    assert_eq!(otap_cp::reserve_for(1200), 100);
    let mut cfg = small(5700);
    cfg.segments = 24;
    cfg.volumes = vec![(0, 2048)];
    cfg.cleaner.background = false;
    let mut s = sim_with(cfg);
    s.boot().unwrap();
    let reserve = s.engine().reserve_segments();
    assert_eq!(reserve, 8);
    let admissible = s.engine().pool_status().admissible_blocks;
    assert!(admissible > 0);
    // Writes are admitted up to what the free segments beyond the reserve
    // hold, counting the arena's unflushed blocks.
    let mut written = 0u64;
    let mut lba = 0u64;
    loop {
        let c = s.write(0, lba, 4).unwrap();
        if c.status == Status::NoSpace {
            break;
        }
        written += 4;
        lba += 4;
        assert!(lba < 2048, "the pool never filled");
    }
    assert!(written <= admissible);
    assert!(written + 4 > admissible / 2);
    let status = s.engine().pool_status();
    assert_eq!(
        status.unflushed_blocks,
        written.min(status.unflushed_blocks)
    );
    assert!(s.engine().stats().admission_refusals >= 1);
    s.request_cp().unwrap();
    let status = s.engine().pool_status();
    assert!(
        status.free_segments >= reserve,
        "{} free, {reserve} reserved",
        status.free_segments
    );
    assert!(!status.pool_full);
    // Every byte acknowledged is kept; the pool boots and stops cleanly
    // with the reserve in hand.
    daemon_crash_and_recover(&mut s).unwrap();
    s.request_cp().unwrap();
    assert_eq!(s.check_disk_prefix().unwrap(), s.model.last_op());
    let v = clean_restart(&mut s).unwrap();
    assert_eq!(s.last_stop(), Stop::Clean);
    assert!(!v.boot.dirty);
}

/// Pacing: past 80% of the arena a write is placed at once but
/// acknowledged after a delay that grows to the maximum at 95%, in order;
/// at 95% writes wait unplaced until a CP drains the arena. The
/// acknowledgements arrive as virtual time passes and the data is there.
#[test]
fn pacing_delays_acknowledgements_and_stalls_at_the_high_watermark() {
    let mut cfg = small(5800);
    cfg.segments = 96;
    cfg.volumes = vec![(0, 4096)];
    cfg.pacing_delay_ns = 50_000_000;
    let mut s = sim_with(cfg);
    s.boot().unwrap();
    let slots = s.arena_slots().unwrap();
    // Stall the device so nothing drains, then fill the arena to 90%.
    s.disk().set_schedule(Schedule {
        max_per_poll: 0,
        reorder: false,
        queue_depth: 64,
    });
    let per = 4u32;
    let target = (f64::from(slots) * 0.90) as u32 / per;
    let mut ids = Vec::new();
    for i in 0..target {
        ids.push(s.submit_write(0, u64::from(i) * u64::from(per), per));
    }
    s.ticks(50).unwrap();
    let done = s.collect();
    let early = done.len();
    assert!(
        early < target as usize,
        "the paced writes are not all acknowledged yet"
    );
    assert!(s.engine().arena_occupancy() >= 0.85);
    assert!(s.engine().stats().paced > 0);
    // Time passes: the delayed acknowledgements arrive, in order.
    s.advance(60_000_000);
    s.ticks(5).unwrap();
    let later = s.collect();
    assert_eq!(
        early + later.len(),
        target as usize,
        "every write acknowledged"
    );
    assert!(later.iter().all(|c| c.status == Status::Ok));
    // Past 95%: held unplaced (deferred), served once the device runs and
    // the CP drains the arena.
    let more = (f64::from(slots) * 0.10) as u32 / per + 2;
    let mut held = Vec::new();
    for i in 0..more {
        held.push(s.submit_write(0, 2048 + u64::from(i) * u64::from(per), per));
    }
    s.ticks(20).unwrap();
    assert!(
        s.engine().stats().deferred > 0,
        "a write was held at the stall"
    );
    s.disk().set_schedule(Schedule::default());
    s.advance(100_000_000);
    for id in held {
        let c = s
            .wait(id, 50_000)
            .unwrap()
            .expect("the held write completes");
        assert_eq!(c.status, Status::Ok);
    }
    s.settle().unwrap();
    s.read(0, 0, 64).unwrap();
    check_everything(&mut s).unwrap();
    clean_restart(&mut s).unwrap();
}

// ---------------------------------------------------------------------------
// Volumes: names, resize, delete, offline
// ---------------------------------------------------------------------------

/// Named volumes and snapshots keep their names across crashes (the
/// catalogue); a volume grows online and shrinks offline; a deleted
/// volume's index is reusable and its name is free again.
#[test]
fn volumes_are_named_resized_deleted_and_recreated_and_the_catalogue_survives_crashes() {
    let mut cfg = small(5900);
    cfg.volumes = vec![(0, 64)];
    cfg.cleaner.background = false;
    let mut s = sim_with(cfg);
    s.boot().unwrap();
    let e = s.engine();
    let v1 = e.create_named_volume("data", 128).unwrap();
    assert_eq!(v1, 1);
    assert!(matches!(
        e.create_named_volume("data", 8),
        Err(EngineError::NameExists { .. })
    ));
    assert!(e.create_named_volume("bad/name", 8).is_err());
    assert!(e.create_named_volume("", 8).is_err());
    e.set_volume_name(0, "boot").unwrap();
    s.tick_until(
        |e| e.volume(1).is_some_and(|v| v.serving),
        "the named volume",
    )
    .unwrap();
    s.model.add_volume(1, 128);
    s.cfg.volumes.push((1, 128));
    ok(&mut s, 1, 0, 8);
    ok(&mut s, 0, 0, 2);
    let snap_uuid = s
        .engine()
        .create_named_snapshot(1, "nightly", SnapKind::Crash, SnapOrigin::User)
        .unwrap();
    assert!(!snap_uuid.is_nil());
    s.tick_until(|e| !e.snapshots().is_empty(), "the named snapshot")
        .unwrap();
    let snap = s.engine().snapshots()[0].clone();
    assert_eq!(snap.name.as_deref(), Some("nightly"));
    assert_eq!(snap.uuid, snap_uuid);
    assert_eq!(s.engine().find_snapshot(1, "nightly"), Some(snap.snap_id));
    assert!(matches!(
        s.engine()
            .create_named_snapshot(1, "nightly", SnapKind::Crash, SnapOrigin::User),
        Err(EngineError::NameExists { .. })
    ));
    s.request_cp().unwrap();
    daemon_crash_and_recover(&mut s).unwrap();
    assert_eq!(s.engine().volume_name(0), Some("boot"));
    assert_eq!(s.engine().volume_name(1), Some("data"));
    assert_eq!(s.engine().find_volume("data"), Some(1));
    assert_eq!(s.engine().snapshot_name(snap.snap_id), Some("nightly"));
    assert_eq!(s.engine().pool_status().catalog_entries, 3);
    // Grow online: the new blocks read as zeros and take writes.
    s.resize_volume(1, 300).unwrap();
    assert_eq!(s.engine().volume(1).unwrap().size_blocks, 300);
    ok(&mut s, 1, 290, 8);
    assert_eq!(s.read(1, 128, 4).unwrap().0, Status::Ok);
    power_cut_and_recover(&mut s, TORN).unwrap();
    assert_eq!(s.engine().volume(1).unwrap().size_blocks, 300);
    s.read(1, 0, 300).unwrap();
    // Shrink needs the volume offline; what lay beyond is gone, and a
    // later grow reads zeros there.
    assert!(matches!(
        s.engine().resize_volume(1, 100),
        Err(EngineError::VolumeOnline { vol: 1 })
    ));
    s.resize_volume(1, 100).unwrap();
    s.set_online(1, true).unwrap();
    assert_eq!(s.read(1, 100, 1).unwrap().0, Status::OutOfRange);
    s.resize_volume(1, 200).unwrap();
    s.read(1, 0, 200).unwrap();
    check_everything(&mut s).unwrap();
    daemon_crash_and_recover(&mut s).unwrap();
    assert_eq!(s.engine().volume(1).unwrap().size_blocks, 200);
    s.read(1, 0, 200).unwrap();
    // Deletion needs no snapshots; afterwards the index and the name are
    // free, and the snapshot's name went with the snapshot.
    assert!(matches!(
        s.engine().delete_volume(1),
        Err(EngineError::VolumeOnline { vol: 1 })
    ));
    s.quiesce(1).unwrap();
    assert!(matches!(
        s.engine().delete_volume(1),
        Err(EngineError::HasSnapshots { vol: 1, count: 1 })
    ));
    s.set_online(1, true).unwrap();
    s.delete_snapshot(snap.snap_id).unwrap();
    assert_eq!(s.engine().pool_status().catalog_entries, 2);
    s.delete_volume(1).unwrap();
    assert!(s.engine().volume(1).is_some_and(|v| !v.present));
    assert_eq!(s.write(1, 0, 1).unwrap().status, Status::NoSuchVolume);
    assert_eq!(s.engine().find_volume("data"), None);
    check_everything(&mut s).unwrap();
    daemon_crash_and_recover(&mut s).unwrap();
    assert!(s.engine().volume(1).is_some_and(|v| !v.present));
    let again = s.engine().create_named_volume("data", 32).unwrap();
    assert_eq!(again, 1);
    s.tick_until(
        |e| e.volume(1).is_some_and(|v| v.serving),
        "the recreated volume",
    )
    .unwrap();
    s.model.add_volume(1, 32);
    s.cfg.volumes.push((1, 32));
    s.read(1, 0, 32).unwrap();
    ok(&mut s, 1, 0, 4);
    check_everything(&mut s).unwrap();
    clean_restart(&mut s).unwrap();
    assert_eq!(s.engine().volume_name(1), Some("data"));
    assert_eq!(s.engine().volumes().len(), 2);
}

/// An offline volume answers `Offline` to everything, keeps its data,
/// stays offline across a reboot (the flag is in the volume table), and
/// serves again when brought online.
#[test]
fn offline_volumes_answer_offline_and_keep_the_state_across_reboots() {
    let mut cfg = small(5950);
    cfg.volumes = vec![(0, 64), (1, 64)];
    let mut s = sim_with(cfg);
    s.boot().unwrap();
    ok(&mut s, 0, 0, 8);
    ok(&mut s, 1, 0, 8);
    s.set_online(0, false).unwrap();
    assert_eq!(s.write(0, 8, 1).unwrap().status, Status::Offline);
    assert_eq!(s.read(0, 0, 1).unwrap().0, Status::Offline);
    assert_eq!(s.punch(0, 0, 1, false).unwrap().status, Status::Offline);
    ok(&mut s, 1, 8, 8);
    let v = s.engine().volume(0).unwrap();
    assert!(v.offline && !v.serving && v.present);
    s.request_cp().unwrap();
    daemon_crash_and_recover(&mut s).unwrap();
    assert!(
        s.engine().volume(0).unwrap().offline,
        "offline after a restart"
    );
    assert_eq!(s.read(0, 0, 1).unwrap().0, Status::Offline);
    power_cut_and_recover(&mut s, DROP).unwrap();
    assert!(s.engine().volume(0).unwrap().offline);
    s.set_online(0, true).unwrap();
    assert!(s.engine().volume(0).unwrap().serving);
    s.read(0, 0, 8).unwrap();
    ok(&mut s, 0, 8, 1);
    // A snapshot of an offline volume is fine; so is a clean restart.
    s.set_online(1, false).unwrap();
    s.snapshot(1).unwrap();
    check_everything(&mut s).unwrap();
    clean_restart(&mut s).unwrap();
    assert!(s.engine().volume(1).unwrap().offline);
    assert!(!s.engine().volume(0).unwrap().offline);
    assert_eq!(s.read(1, 0, 1).unwrap().0, Status::Offline);
    s.set_online(1, true).unwrap();
    s.read(1, 0, 8).unwrap();
}

// ---------------------------------------------------------------------------
// A property walk
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
enum WalkOp {
    Write { vol: u8, lba: u16, blocks: u8 },
    Punch { vol: u8, lba: u16, blocks: u8 },
    Snapshot { vol: u8 },
    DeleteSnapshot { which: u8 },
    Rollback { vol: u8 },
    Grow { vol: u8, by: u8 },
    Offline { vol: u8 },
    Online { vol: u8 },
    Cp,
    Clean,
    Power { tear: u8 },
    Daemon,
    CleanRestart,
    Check,
}

fn walk_op() -> impl Strategy<Value = WalkOp> {
    prop_oneof![
        10 => (any::<u8>(), any::<u16>(), 1..=6u8).prop_map(|(vol, lba, blocks)| WalkOp::Write { vol, lba, blocks }),
        2 => (any::<u8>(), any::<u16>(), 1..=64u8).prop_map(|(vol, lba, blocks)| WalkOp::Punch { vol, lba, blocks }),
        2 => any::<u8>().prop_map(|vol| WalkOp::Snapshot { vol }),
        2 => any::<u8>().prop_map(|which| WalkOp::DeleteSnapshot { which }),
        1 => any::<u8>().prop_map(|vol| WalkOp::Rollback { vol }),
        1 => (any::<u8>(), 1..=8u8).prop_map(|(vol, by)| WalkOp::Grow { vol, by }),
        1 => any::<u8>().prop_map(|vol| WalkOp::Offline { vol }),
        1 => any::<u8>().prop_map(|vol| WalkOp::Online { vol }),
        3 => Just(WalkOp::Cp),
        2 => Just(WalkOp::Clean),
        1 => (0..4u8).prop_map(|tear| WalkOp::Power { tear }),
        1 => Just(WalkOp::Daemon),
        1 => Just(WalkOp::CleanRestart),
        2 => Just(WalkOp::Check),
    ]
}

fn tear_of(n: u8) -> TearPolicy {
    match n {
        0 => DROP,
        1 => WHOLE,
        2 => TORN,
        _ => TearPolicy::default(),
    }
}

/// Two volumes on a small pool with the production reserve and the
/// cleaner on: random writes and punches (overwrite-heavy, so the cleaner
/// runs), snapshots taken and deleted, rollbacks to the volume's newest
/// snapshot (the newer ones deleted first), grows, offline and online
/// toggles, clean requests and crashes of every kind. After every recovery
/// the model checks the image, the snapshots and the live-block
/// accounting; `Check` steps do the same in between; the walk ends in a
/// clean restart that keeps everything.
fn step5_walk(seed: u64, ops: &[WalkOp]) -> Result<(), String> {
    let mut cfg = small(seed);
    cfg.segments = 48;
    cfg.volumes = vec![(0, 200), (1, 120)];
    cfg.fold.max_cps = 4;
    cfg.cleaner = CleanerPolicy {
        utilization: 0.5,
        max_segments_per_cp: 2,
        background: true,
        margin_segments: 4,
        max_live_fraction: 0.8,
    };
    let mut s = Sim::new(cfg)?;
    s.boot()?;
    s.verify_recovery()?;
    let mut sizes: HashMap<u32, u64> = [(0u32, 200u64), (1, 120)].into_iter().collect();
    let mut offline: HashSet<u32> = HashSet::new();
    let pick = |sizes: &HashMap<u32, u64>, vol: u8, lba: u16, blocks: u8| -> (u32, u64, u32) {
        let vol = u32::from(vol % 2);
        let size = sizes[&vol];
        let blocks = u64::from(blocks.max(1)).min(size);
        let lba = (u64::from(lba) * 7) % (size - blocks + 1);
        (vol, lba, blocks as u32)
    };
    for (i, op) in ops.iter().enumerate() {
        let step = |e: String| format!("seed {seed} op {i} {op:?}: {e}");
        match *op {
            WalkOp::Write { vol, lba, blocks } => {
                let (vol, lba, blocks) = pick(&sizes, vol, lba, blocks);
                let held = s.engine().nvfail_set().get(vol);
                let c = s.write(vol, lba, blocks).map_err(step)?;
                let expected = if offline.contains(&vol) {
                    Status::Offline
                } else if held {
                    Status::NvFailed
                } else {
                    Status::Ok
                };
                if c.status != expected && c.status != Status::NoSpace {
                    return Err(step(format!("write status {:?}", c.status)));
                }
            }
            WalkOp::Punch { vol, lba, blocks } => {
                let (vol, lba, blocks) = pick(&sizes, vol, lba, blocks);
                let held = s.engine().nvfail_set().get(vol);
                let c = s.punch(vol, lba, blocks, false).map_err(step)?;
                let expected = if offline.contains(&vol) {
                    Status::Offline
                } else if held {
                    Status::NvFailed
                } else {
                    Status::Ok
                };
                if c.status != expected && c.status != Status::NoSpace {
                    return Err(step(format!("punch status {:?}", c.status)));
                }
            }
            WalkOp::Snapshot { vol } => {
                let vol = u32::from(vol % 2);
                if !s.engine().nvfail_set().get(vol) && s.snaps.len() < 5 {
                    s.snapshot(vol).map_err(step)?;
                }
            }
            WalkOp::DeleteSnapshot { which } => {
                if !s.snaps.is_empty() {
                    let id = s.snaps[usize::from(which) % s.snaps.len()].snap_id;
                    s.delete_snapshot(id).map_err(step)?;
                }
            }
            WalkOp::Rollback { vol } => {
                let vol = u32::from(vol % 2);
                let mut of_vol: Vec<(u64, u64)> = s
                    .snaps
                    .iter()
                    .filter(|x| x.vol == vol)
                    .map(|x| (x.cp_seq, x.snap_id))
                    .collect();
                if of_vol.is_empty() || s.engine().nvfail_set().get(vol) {
                    continue;
                }
                of_vol.sort_unstable();
                // The oldest snapshot is the target; the newer ones go
                // first (a rollback refuses otherwise).
                let target = of_vol[0].1;
                for (_, id) in of_vol.iter().skip(1) {
                    s.delete_snapshot(*id).map_err(step)?;
                }
                s.rollback(vol, target).map_err(step)?;
                offline.insert(vol);
            }
            WalkOp::Grow { vol, by } => {
                let vol = u32::from(vol % 2);
                let size = sizes[&vol] + u64::from(by) * 16;
                if size > 1500 {
                    continue;
                }
                s.resize_volume(vol, size).map_err(step)?;
                sizes.insert(vol, size);
            }
            WalkOp::Offline { vol } => {
                let vol = u32::from(vol % 2);
                s.set_online(vol, false).map_err(step)?;
                offline.insert(vol);
            }
            WalkOp::Online { vol } => {
                let vol = u32::from(vol % 2);
                s.set_online(vol, true).map_err(step)?;
                offline.remove(&vol);
            }
            WalkOp::Cp => {
                s.request_cp().map_err(step)?;
            }
            WalkOp::Clean => {
                s.request_clean().map_err(step)?;
            }
            WalkOp::Power { tear } => {
                s.disk().set_tear_policy(tear_of(tear));
                s.crash_power().map_err(step)?;
                s.boot().map_err(step)?;
                s.verify_recovery().map_err(step)?;
                s.clear_nvfail().map_err(step)?;
            }
            WalkOp::Daemon => {
                s.crash_daemon().map_err(step)?;
                s.boot().map_err(step)?;
                s.verify_recovery().map_err(step)?;
            }
            WalkOp::CleanRestart => {
                s.shutdown_clean().map_err(step)?;
                s.boot().map_err(step)?;
                let v = s.verify_recovery().map_err(step)?;
                if v.k != s.model.last_op() {
                    return Err(step(format!(
                        "clean restart kept {} of {}",
                        v.k,
                        s.model.last_op()
                    )));
                }
            }
            WalkOp::Check => {
                check_everything(&mut s).map_err(step)?;
            }
        }
        // Offline and the size are durable states the engine reports; keep
        // the walk's view honest after a reboot or a rollback.
        for vol in [0u32, 1] {
            let v = s.engine().volume(vol).expect("the volume");
            if v.offline {
                offline.insert(vol);
            } else {
                offline.remove(&vol);
            }
            sizes.insert(vol, v.size_blocks);
        }
    }
    check_everything(&mut s)?;
    s.shutdown_clean()?;
    s.boot()?;
    let v = s.verify_recovery()?;
    if v.k != s.model.last_op() {
        return Err(format!(
            "seed {seed} final restart: prefix {}, last op {}",
            v.k,
            s.model.last_op()
        ));
    }
    check_everything(&mut s)?;
    if s.engine().pool_status().free_segments < otap_cp::META_RESERVE_SEGMENTS {
        return Err(format!("seed {seed}: the floor was breached"));
    }
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
        max_shrink_iters: 512,
        ..ProptestConfig::default()
    })]

    /// The brief's two property subjects: GC and crash invariance (random
    /// power cuts and daemon crashes amid compaction and snapshot deletion
    /// never lose live data, never bring deleted data back and never leak
    /// the live-block accounting across reboots) and the full-pool
    /// recovery (the reserve is never breached and writes resume after
    /// deletions and cleaning), with rollbacks, grows and offline volumes
    /// in the mix.
    #[test]
    fn random_ops_with_deletions_rollbacks_cleaning_and_crashes_keep_the_image_and_the_accounting(
        seed in any::<u64>(),
        ops in prop::collection::vec(walk_op(), 4..32),
    ) {
        if let Err(e) = step5_walk(seed, &ops) {
            prop_assert!(false, "{e}");
        }
    }
}
