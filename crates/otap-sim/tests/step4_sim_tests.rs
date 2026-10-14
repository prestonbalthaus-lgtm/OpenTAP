//! Step 4 (Phase C): the persistent map, the CP log, snapshots, holes, the
//! hybrid read path, the fold policy, segment reclamation, the E19 trust
//! policy under a cached controller, and the late-discard regression, all
//! over the simulator's device with random power cuts and daemon crashes.

use std::collections::HashMap;

use otap_cp::{
    Backstore, Busy, DevClass, DiscardPolicy, Done, Engine, EngineConfig, Event, FlushPolicy,
    FoldPolicy, IoError, OperatorWarning, PeekSource, Phase, Status, Tag,
};
use otap_format::{IntentSlot, SuperblockSlot, BLOCK_SIZE, MAP_ENTRIES_PER_PAGE};
use otap_sim::{
    Model, Schedule, Sim, SimBackstore, SimClock, SimConfig, SimFrontDoor, SplitMix64, Stop,
    TearPolicy,
};
use otap_sys::{HugeArena, HugePageSize};
use proptest::prelude::*;

const WALL_BASE: u64 = 1_700_000_000_000_000_000;

const DROP: TearPolicy = TearPolicy {
    drop: 1,
    whole: 0,
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

/// A write that must be acknowledged.
fn ok(s: &mut Sim, vol: u32, lba: u64, blocks: u32) {
    let c = s.write(vol, lba, blocks).expect("the write completes");
    assert_eq!(c.status, Status::Ok, "write {vol}/{lba}+{blocks}");
}

/// A discard (or a write of zeros) that must be acknowledged.
fn punch_ok(s: &mut Sim, vol: u32, lba: u64, blocks: u32, zeroes: bool) {
    let c = s
        .punch(vol, lba, blocks, zeroes)
        .expect("the punch completes");
    assert_eq!(c.status, Status::Ok, "punch {vol}/{lba}+{blocks}");
}

/// A write or punch that is acknowledged unless nvfail holds the volume.
fn io_respecting_nvfail(s: &mut Sim, vol: u32, lba: u64, blocks: u32, punch: Option<bool>) {
    let held = s.engine().nvfail_set().get(vol);
    let c = match punch {
        None => s.write(vol, lba, blocks),
        Some(zeroes) => s.punch(vol, lba, blocks, zeroes),
    }
    .expect("the request completes");
    let expected = if held { Status::NvFailed } else { Status::Ok };
    assert_eq!(c.status, expected, "{vol}/{lba}+{blocks} punch {punch:?}");
}

fn source_of(s: &mut Sim, vol: u32, lba: u64) -> PeekSource {
    let mut buf = vec![0u8; BLOCK_SIZE];
    s.engine().peek(vol, lba, &mut buf).expect("peek")[0]
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

fn folds_in(events: &[Event]) -> usize {
    events
        .iter()
        .filter(|e| matches!(e, Event::Folded { .. }))
        .count()
}

// ---------------------------------------------------------------------------
// The persistent map
// ---------------------------------------------------------------------------

/// Volumes larger than a map page, written across their pages for many
/// CPs and folds, with power cuts and daemon crashes in between: the map's
/// pages, indexes and delta regions land in several metadata segments and
/// every boot reads the right image back.
#[test]
fn a_map_spanning_many_pages_and_segments_persists_across_crashes() {
    let page = MAP_ENTRIES_PER_PAGE as u64;
    let sizes = [3 * page - 72, 2 * page + 400];
    let mut cfg = small(401);
    cfg.volumes = vec![(0, sizes[0]), (1, sizes[1])];
    cfg.fold.max_cps = 3;
    let segments = cfg.segments;
    let mut s = sim_with(cfg);
    s.boot().unwrap();
    let mut rng = SplitMix64::new(401);
    for round in 0..16 {
        for _ in 0..6 {
            let vol = rng.below(2) as u32;
            let size = sizes[vol as usize];
            let blocks = rng.range(1, 8) as u32;
            let lba = rng.below(size - u64::from(blocks) + 1);
            io_respecting_nvfail(&mut s, vol, lba, blocks, None);
        }
        s.request_cp().unwrap();
        match round % 4 {
            1 => {
                daemon_crash_and_recover(&mut s).unwrap();
            }
            3 => {
                let tear = if round % 8 == 3 { TORN } else { DROP };
                power_cut_and_recover(&mut s, tear).unwrap();
            }
            _ => {}
        }
        s.check_disk_prefix().unwrap();
    }
    s.request_fold().unwrap();
    for vol in 0..2 {
        let pages = s
            .engine()
            .map()
            .volume(vol)
            .expect("the volume's map")
            .index()
            .iter()
            .filter(|e| !e.is_none())
            .count();
        assert!(pages >= 2, "volume {vol} has {pages} map pages");
    }
    let meta_segments = (0..u64::from(segments))
        .filter(|p| s.engine().meta_live_bytes(*p) > 0)
        .count();
    assert!(
        meta_segments >= 2,
        "metadata lives in {meta_segments} segment(s)"
    );
    assert!(folds_in(&s.events) >= 4);
    let v = clean_restart(&mut s).unwrap();
    assert!(!v.boot.rewound);
}

/// Between folds the CP log carries the map's changes: after a crash the
/// boot replays every delta region above the fold, and nothing else.
#[test]
fn cp_log_records_replay_from_the_last_fold_after_a_crash() {
    let mut cfg = small(402);
    cfg.fold = no_auto_fold();
    let mut s = sim_with(cfg);
    s.boot().unwrap();
    let fold0 = s.engine().map().fold_cp();
    for i in 0..6u64 {
        ok(&mut s, 0, i * 4, 3);
        ok(&mut s, 1, 60 - i * 2, 2);
        s.request_cp().unwrap();
    }
    let map = s.engine().map();
    assert_eq!(map.fold_cp(), fold0, "no fold happened on its own");
    assert_eq!(map.cps_since_fold(), 6);
    assert_eq!(map.delta_roots().len(), 6, "one delta region per CP");
    let bytes = map.log_bytes_since_fold();
    assert!(bytes > 0);
    // A power cut: the boot replays the six regions.
    let v = power_cut_and_recover(&mut s, TearPolicy::default()).unwrap();
    assert!(!v.boot.rewound);
    assert_eq!(v.k, s.model.last_op());
    let map = s.engine().map();
    assert_eq!(map.fold_cp(), fold0);
    assert_eq!(map.cps_since_fold(), 6);
    assert_eq!(map.delta_roots().len(), 6);
    assert_eq!(map.log_bytes_since_fold(), bytes);
    let replayed = s
        .events
        .iter()
        .rev()
        .find_map(|e| match e {
            Event::Booted {
                replayed_blocks, ..
            } => Some(*replayed_blocks),
            _ => None,
        })
        .expect("a Booted event");
    assert_eq!(
        replayed, 0,
        "nothing in the arena to replay after a power cut"
    );
    // Partial progress since the fold, then a daemon crash: replayed too.
    ok(&mut s, 0, 40, 4);
    s.request_cp().unwrap();
    daemon_crash_and_recover(&mut s).unwrap();
    assert_eq!(s.engine().map().cps_since_fold(), 7);
    // A requested fold empties the log; a crash then boots from the fold.
    let folds = folds_in(&s.events);
    s.request_fold().unwrap();
    assert_eq!(folds_in(&s.events), folds + 1);
    let map = s.engine().map();
    assert!(map.fold_cp() > fold0);
    assert_eq!(map.cps_since_fold(), 0);
    assert!(map.delta_roots().is_empty());
    assert_eq!(map.log_bytes_since_fold(), 0);
    power_cut_and_recover(&mut s, TearPolicy::default()).unwrap();
    assert_eq!(s.engine().map().cps_since_fold(), 0);
    assert!(s.engine().map().delta_roots().is_empty());
    // A clean shutdown folds what is dirty.
    ok(&mut s, 1, 0, 1);
    s.request_cp().unwrap();
    assert_eq!(s.engine().map().cps_since_fold(), 1);
    clean_restart(&mut s).unwrap();
    assert_eq!(s.engine().map().cps_since_fold(), 0);
    assert!(s.engine().map().delta_roots().is_empty());
}

// ---------------------------------------------------------------------------
// Snapshots
// ---------------------------------------------------------------------------

/// Snapshots taken between writes, punches, CPs, folds, power cuts (every
/// tear policy) and daemon crashes keep reading the image the model gave
/// them, and the live diff against each is exactly what changed since.
#[test]
fn snapshots_keep_their_image_and_diff_across_random_crashes() {
    let mut verified = 0;
    let mut diffs = 0;
    for seed in 410..418u64 {
        let mut cfg = small(seed);
        cfg.volumes = vec![(0, 96), (1, 64)];
        cfg.fold.max_cps = 4;
        let sizes = [96u64, 64];
        let mut s = sim_with(cfg);
        s.boot().unwrap();
        let mut rng = SplitMix64::new(seed);
        for _ in 0..36 {
            match rng.below(12) {
                0..=4 => {
                    let vol = rng.below(2) as u32;
                    let blocks = rng.range(1, 6) as u32;
                    let lba = rng.below(sizes[vol as usize] - u64::from(blocks) + 1);
                    io_respecting_nvfail(&mut s, vol, lba, blocks, None);
                }
                5 => {
                    let vol = rng.below(2) as u32;
                    let blocks = rng.range(1, 12) as u32;
                    let lba = rng.below(sizes[vol as usize] - u64::from(blocks) + 1);
                    let zeroes = rng.chance(1, 2);
                    io_respecting_nvfail(&mut s, vol, lba, blocks, Some(zeroes));
                }
                6 | 7 => {
                    let vol = rng.below(2) as u32;
                    if !s.engine().nvfail_set().get(vol) {
                        s.snapshot(vol).unwrap();
                    }
                }
                8 => {
                    s.request_cp().unwrap();
                }
                9 => {
                    let tear = match rng.below(3) {
                        0 => DROP,
                        1 => TORN,
                        _ => TearPolicy::default(),
                    };
                    power_cut_and_recover(&mut s, tear).unwrap();
                }
                10 => {
                    daemon_crash_and_recover(&mut s).unwrap();
                }
                _ => {
                    s.request_fold().unwrap();
                }
            }
            s.verify_snapshots().unwrap();
            let ids: Vec<u64> = s.snaps.iter().map(|x| x.snap_id).collect();
            for id in ids {
                s.check_diff(id).unwrap();
                diffs += 1;
            }
        }
        clean_restart(&mut s).unwrap();
        verified += s.snaps.len();
    }
    assert!(verified >= 8, "{verified} snapshots survived to the end");
    assert!(diffs >= 100, "{diffs} diffs checked");
}

/// A snapshot pins its blocks: overwriting and punching the live volume
/// after it changes what the volume reads, not what the snapshot reads,
/// across CPs, folds and a restart from disk.
#[test]
fn a_snapshot_is_isolated_from_later_writes_punches_and_folds() {
    let mut cfg = small(419);
    cfg.fold.max_cps = 2;
    let mut s = sim_with(cfg);
    s.boot().unwrap();
    ok(&mut s, 0, 0, 8);
    ok(&mut s, 0, 20, 4);
    punch_ok(&mut s, 0, 2, 2, false);
    let snap = s.snapshot(0).unwrap();
    assert_eq!(s.engine().stats().snapshots, 1);
    assert_eq!(s.check_diff(snap).unwrap(), 0, "nothing changed yet");
    // Overwrite, punch, extend.
    ok(&mut s, 0, 0, 1);
    punch_ok(&mut s, 0, 20, 2, true);
    ok(&mut s, 0, 40, 3);
    ok(&mut s, 0, 3, 1);
    s.request_cp().unwrap();
    s.verify_snapshots().unwrap();
    assert_eq!(s.check_diff(snap).unwrap(), 7, "0, 3, 20, 21, 40..42");
    // Folds and more CPs do not disturb it; neither does a restart.
    for i in 0..4u64 {
        ok(&mut s, 0, 50 + i, 1);
        s.request_cp().unwrap();
    }
    assert!(s.engine().stats().folds >= 2);
    s.verify_snapshots().unwrap();
    assert_eq!(s.check_diff(snap).unwrap(), 11);
    clean_restart(&mut s).unwrap();
    assert_eq!(s.check_diff(snap).unwrap(), 11);
    power_cut_and_recover(&mut s, TORN).unwrap();
    s.verify_snapshots().unwrap();
    assert_eq!(s.check_diff(snap).unwrap(), 11);
    // A second snapshot of the same volume diffs from the first state.
    let later = s.snapshot(0).unwrap();
    assert_eq!(s.check_diff(later).unwrap(), 0);
    assert_eq!(s.engine().snapshots().len(), 2);
    ok(&mut s, 0, 60, 1);
    s.request_cp().unwrap();
    assert_eq!(s.check_diff(later).unwrap(), 1);
    assert_eq!(s.check_diff(snap).unwrap(), 12);
}

// ---------------------------------------------------------------------------
// Holes
// ---------------------------------------------------------------------------

/// Discards and writes of zeros read as zeros at once (from the arena's
/// hole record), survive a daemon crash through the re-attached arena,
/// become holes in the committed map at the next CP, and survive a power
/// cut from there.
#[test]
fn punches_persist_through_daemon_crashes_and_power_cuts() {
    let mut s = sim_with(small(420));
    s.boot().unwrap();
    ok(&mut s, 0, 0, 16);
    ok(&mut s, 1, 10, 6);
    punch_ok(&mut s, 0, 4, 4, false);
    punch_ok(&mut s, 1, 12, 2, true);
    assert_eq!(s.engine().stats().punches, 2);
    assert_eq!(source_of(&mut s, 0, 5), PeekSource::Hole);
    assert_eq!(source_of(&mut s, 0, 3), PeekSource::Arena);
    assert_eq!(source_of(&mut s, 1, 13), PeekSource::Hole);
    s.read(0, 0, 16).unwrap();
    s.read(1, 10, 6).unwrap();
    // The daemon dies: the hole records come back with the arena.
    let v = daemon_crash_and_recover(&mut s).unwrap();
    assert!(v.boot.reattached);
    assert_eq!(v.k, s.model.last_op());
    assert_eq!(source_of(&mut s, 0, 5), PeekSource::Hole);
    assert_eq!(source_of(&mut s, 1, 13), PeekSource::Hole);
    s.read(0, 0, 16).unwrap();
    // A CP commits them as holes in the map.
    s.request_cp().unwrap();
    assert!(s.engine().map_entry(0, 5).is_some_and(|e| e.is_hole()));
    assert!(s.engine().map_entry(1, 13).is_some_and(|e| e.is_hole()));
    assert!(s.engine().map_entry(0, 3).is_some_and(|e| e.is_data()));
    assert_eq!(source_of(&mut s, 0, 5), PeekSource::Hole);
    power_cut_and_recover(&mut s, TORN).unwrap();
    assert!(s.engine().map_entry(0, 5).is_some_and(|e| e.is_hole()));
    assert_eq!(source_of(&mut s, 0, 5), PeekSource::Hole);
    s.read(0, 0, 16).unwrap();
    s.read(1, 10, 6).unwrap();
    // Writes over holes, holes over writes, a hole over a hole, and a
    // punch of the whole volume.
    ok(&mut s, 0, 5, 2);
    punch_ok(&mut s, 0, 0, 2, true);
    punch_ok(&mut s, 0, 6, 4, false);
    s.read(0, 0, 16).unwrap();
    s.request_cp().unwrap();
    s.read(0, 0, 16).unwrap();
    punch_ok(&mut s, 1, 0, 64, false);
    for lba in [0, 13, 63] {
        assert_eq!(source_of(&mut s, 1, lba), PeekSource::Hole);
    }
    s.read(1, 0, 64).unwrap();
    s.request_fold().unwrap();
    assert!(s.engine().map_entry(1, 63).is_some_and(|e| e.is_hole()));
    clean_restart(&mut s).unwrap();
    s.read(1, 0, 64).unwrap();
    s.read(0, 0, 16).unwrap();
}

/// Punches are refused where writes are: a held volume, a range past the
/// end, a missing volume; and a punch larger than the bound.
#[test]
fn punches_are_refused_like_writes() {
    let mut s = sim_with(small(421));
    s.boot().unwrap();
    let c = s.punch(0, 60, 8, false).unwrap();
    assert_eq!(c.status, Status::OutOfRange);
    let c = s.punch(7, 0, 1, true).unwrap();
    assert_eq!(c.status, Status::NoSuchVolume);
    let c = s.punch(0, 0, 0, true).unwrap();
    assert_eq!(c.status, Status::OutOfRange);
    ok(&mut s, 0, 0, 2);
    power_cut_and_recover_without_clear(&mut s);
    assert!(s.engine().nvfail_set().get(0));
    let c = s.punch(0, 0, 1, false).unwrap();
    assert_eq!(c.status, Status::NvFailed);
    s.clear_nvfail().unwrap();
    punch_ok(&mut s, 0, 0, 1, false);
}

fn power_cut_and_recover_without_clear(s: &mut Sim) {
    s.crash_power().expect("the power cut");
    s.boot().expect("the boot");
    s.verify_recovery().expect("recovery");
}

// ---------------------------------------------------------------------------
// The hybrid read path
// ---------------------------------------------------------------------------

/// A read is answered from the active generation first, then from a
/// frozen generation whose CP is still in flight, then from the committed
/// map on disk; the newest wins at every step, holes included.
#[test]
fn reads_come_from_the_active_index_the_frozen_generation_and_the_map() {
    let mut s = sim_with(small(430));
    s.boot().unwrap();
    ok(&mut s, 0, 0, 4);
    ok(&mut s, 0, 8, 2);
    assert_eq!(source_of(&mut s, 0, 1), PeekSource::Arena);
    assert_eq!(source_of(&mut s, 0, 20), PeekSource::Unmapped);
    // Freeze the generation in a CP the device never completes.
    s.disk().set_schedule(stalled());
    s.engine().request_cp();
    s.ticks(8).unwrap();
    assert_eq!(s.engine().frozen_generations(), 1);
    assert_ne!(s.engine().phase(), Phase::Idle);
    // New writes and a punch go to the next generation; reads see them
    // over the frozen blocks, and the frozen ones over the (empty) map.
    ok(&mut s, 0, 0, 1);
    punch_ok(&mut s, 0, 8, 1, false);
    assert!(
        s.engine().map_entry(0, 1).is_none(),
        "nothing committed yet"
    );
    s.read(0, 0, 4).unwrap();
    s.read(0, 8, 2).unwrap();
    assert_eq!(source_of(&mut s, 0, 0), PeekSource::Arena);
    assert_eq!(source_of(&mut s, 0, 1), PeekSource::Arena);
    assert_eq!(source_of(&mut s, 0, 8), PeekSource::Hole);
    assert_eq!(source_of(&mut s, 0, 9), PeekSource::Arena);
    // The device wakes: the frozen generation commits; the newer blocks
    // still shadow the map's.
    s.disk().set_schedule(Schedule::default());
    s.settle().unwrap();
    assert_eq!(s.engine().frozen_generations(), 0);
    assert!(s.engine().map_entry(0, 0).is_some_and(|e| e.is_data()));
    assert_eq!(
        source_of(&mut s, 0, 0),
        PeekSource::Arena,
        "the newer write"
    );
    assert_eq!(source_of(&mut s, 0, 1), PeekSource::Disk);
    assert_eq!(source_of(&mut s, 0, 8), PeekSource::Hole, "the newer punch");
    assert_eq!(source_of(&mut s, 0, 9), PeekSource::Disk);
    s.read(0, 0, 4).unwrap();
    s.read(0, 8, 2).unwrap();
    // The next CP moves everything to the map.
    s.request_cp().unwrap();
    assert_eq!(source_of(&mut s, 0, 0), PeekSource::Disk);
    assert_eq!(source_of(&mut s, 0, 8), PeekSource::Hole);
    assert!(s.engine().map_entry(0, 8).is_some_and(|e| e.is_hole()));
    s.read(0, 0, 10).unwrap();
    // A fold rewrites the map's pages; reads are unchanged.
    s.request_fold().unwrap();
    s.read(0, 0, 10).unwrap();
    assert_eq!(source_of(&mut s, 0, 0), PeekSource::Disk);
    assert_eq!(source_of(&mut s, 0, 20), PeekSource::Unmapped);
}

// ---------------------------------------------------------------------------
// The fold policy and segment reclamation
// ---------------------------------------------------------------------------

/// Each trigger of the fold policy folds at the CP it names: the CP count,
/// the log's bytes, the dirty pages, the overlay size, and time.
#[test]
fn the_fold_policy_folds_on_each_of_its_triggers() {
    let page = MAP_ENTRIES_PER_PAGE as u64;
    // CP count: folds at the third CP that carries changes.
    let mut cfg = small(440);
    cfg.fold = no_auto_fold();
    cfg.fold.max_cps = 3;
    let mut s = sim_with(cfg);
    s.boot().unwrap();
    let base = folds_in(&s.events);
    for cp in 1..=3u64 {
        ok(&mut s, 0, cp, 1);
        s.request_cp().unwrap();
        let folded = folds_in(&s.events) - base;
        assert_eq!(folded, usize::from(cp == 3), "after CP {cp}");
    }
    assert_eq!(s.engine().map().cps_since_fold(), 0);
    // An empty CP does not count.
    s.request_cp().unwrap();
    assert_eq!(s.engine().map().cps_since_fold(), 0);
    // Log bytes: enough records in one CP fold it at once.
    let mut cfg = small(441);
    cfg.fold = no_auto_fold();
    cfg.fold.log_bytes = 2048;
    let mut s = sim_with(cfg);
    s.boot().unwrap();
    let base = folds_in(&s.events);
    ok(&mut s, 0, 0, 2);
    s.request_cp().unwrap();
    assert_eq!(folds_in(&s.events) - base, 0, "two records are not 2 KiB");
    for lba in 0..64u64 {
        ok(&mut s, 1, lba, 1);
    }
    for lba in (0..64u64).rev() {
        ok(&mut s, 0, lba, 1);
    }
    s.request_cp().unwrap();
    assert_eq!(folds_in(&s.events) - base, 1, "128 records are over 2 KiB");
    assert_eq!(s.engine().map().log_bytes_since_fold(), 0);
    // Dirty pages: a volume spanning pages, touched on three of them.
    let mut cfg = small(442);
    cfg.volumes = vec![(0, 3 * page), (1, 64)];
    cfg.fold = no_auto_fold();
    cfg.fold.max_dirty_pages = 3;
    let mut s = sim_with(cfg);
    s.boot().unwrap();
    let base = folds_in(&s.events);
    ok(&mut s, 0, 0, 1);
    ok(&mut s, 0, page + 1, 1);
    s.request_cp().unwrap();
    assert_eq!(folds_in(&s.events) - base, 0, "two dirty pages");
    assert_eq!(s.engine().map().dirty_pages(), 2);
    ok(&mut s, 0, 2 * page + 2, 1);
    s.request_cp().unwrap();
    assert_eq!(
        folds_in(&s.events) - base,
        0,
        "the policy reads the committed dirty pages: not yet"
    );
    assert_eq!(s.engine().map().dirty_pages(), 3);
    ok(&mut s, 0, 7, 1);
    s.request_cp().unwrap();
    assert_eq!(folds_in(&s.events) - base, 1, "three dirty pages");
    assert_eq!(s.engine().map().dirty_pages(), 0);
    // Overlay entries.
    let mut cfg = small(443);
    cfg.fold = no_auto_fold();
    cfg.fold.max_overlay_entries = 4;
    let mut s = sim_with(cfg);
    s.boot().unwrap();
    let base = folds_in(&s.events);
    ok(&mut s, 0, 0, 3);
    s.request_cp().unwrap();
    assert_eq!(folds_in(&s.events) - base, 0);
    assert_eq!(s.engine().map().overlay_entries(), 3);
    ok(&mut s, 0, 10, 2);
    s.request_cp().unwrap();
    assert_eq!(s.engine().map().overlay_entries(), 5);
    ok(&mut s, 0, 20, 1);
    s.request_cp().unwrap();
    assert_eq!(folds_in(&s.events) - base, 1, "five entries are over four");
    assert_eq!(s.engine().map().overlay_entries(), 0);
    // Time.
    let mut cfg = small(444);
    cfg.fold = no_auto_fold();
    cfg.fold.interval_ns = 10_000_000_000;
    let mut s = sim_with(cfg);
    s.boot().unwrap();
    let base = folds_in(&s.events);
    ok(&mut s, 0, 0, 1);
    s.request_cp().unwrap();
    assert_eq!(folds_in(&s.events) - base, 0);
    s.advance(11_000_000_000);
    s.request_cp().unwrap();
    assert_eq!(
        folds_in(&s.events) - base,
        1,
        "the interval passed with the overlay dirty: the CP folds"
    );
    ok(&mut s, 0, 1, 1);
    s.request_cp().unwrap();
    assert_eq!(
        folds_in(&s.events) - base,
        1,
        "the clock restarted at the fold"
    );
    s.advance(11_000_000_000);
    ok(&mut s, 0, 2, 1);
    s.request_cp().unwrap();
    assert_eq!(folds_in(&s.events) - base, 2, "the interval passed again");
    // Every fold leaves the map readable and the pool consistent.
    clean_restart(&mut s).unwrap();
}

/// Segment reclamation follows the discard policy: whole-segment discards
/// on flash, plain reuse without discards otherwise; and a reclaimed
/// segment reads back fine after a reuse either way.
#[test]
fn segment_reclamation_follows_the_discard_policy() {
    for (policy, discards_expected) in [
        (DiscardPolicy::WholeSegment, true),
        (DiscardPolicy::Never, false),
    ] {
        let mut cfg = small(450);
        cfg.discard = policy;
        cfg.fold.max_cps = 2;
        let mut s = sim_with(cfg);
        s.boot().unwrap();
        for round in 0..24u64 {
            ok(&mut s, 0, (round * 3) % 60, 3);
            ok(&mut s, 1, (round * 5) % 60, 2);
            s.request_cp().unwrap();
        }
        let freed: Vec<bool> = s
            .events
            .iter()
            .filter_map(|e| match e {
                Event::SegmentFreed { discarded, .. } => Some(*discarded),
                _ => None,
            })
            .collect();
        assert!(
            freed.len() >= 4,
            "{policy:?}: {} segments freed",
            freed.len()
        );
        assert!(
            freed.iter().all(|d| *d == discards_expected),
            "{policy:?}: discarded flags {freed:?}"
        );
        let discards = s.disk().stats().discards;
        assert_eq!(
            discards > 0,
            discards_expected,
            "{policy:?}: {discards} discards"
        );
        assert_eq!(s.engine().stats().discards > 0, discards_expected);
        s.check_disk_prefix().unwrap();
        power_cut_and_recover(&mut s, TORN).unwrap();
        clean_restart(&mut s).unwrap();
    }
}

// ---------------------------------------------------------------------------
// E19: the trust policy under a cached controller
// ---------------------------------------------------------------------------

/// A flush-opaque controller whose cache holds through a power cut (a
/// healthy battery): under the trust policy every power cut boots without
/// a rewind, every committed write is there, and nvfail quarantines the
/// volumes until the operator clears them.
#[test]
fn a_protected_opaque_cache_recovers_every_committed_cp_under_the_trust_policy() {
    for seed in 460..466u64 {
        let mut cfg = small(seed);
        cfg.flush_opaque = true;
        cfg.flush_policy = FlushPolicy::TrustBarrierWithNvfail;
        cfg.cache_protected = true;
        cfg.fold.max_cps = 3;
        let mut s = sim_with(cfg);
        s.boot().unwrap();
        let status = s.engine().pool_status();
        assert!(status.flush_opaque);
        assert!(matches!(
            status.warnings.as_slice(),
            [OperatorWarning::FlushOpaqueTrusted {
                policy: FlushPolicy::TrustBarrierWithNvfail
            }]
        ));
        let mut rng = SplitMix64::new(seed);
        for _ in 0..24 {
            match rng.below(8) {
                0..=3 => {
                    let vol = rng.below(2) as u32;
                    let blocks = rng.range(1, 6) as u32;
                    let lba = rng.below(64 - u64::from(blocks) + 1);
                    io_respecting_nvfail(&mut s, vol, lba, blocks, None);
                }
                4 => {
                    let vol = rng.below(2) as u32;
                    io_respecting_nvfail(&mut s, vol, rng.below(60), 4, Some(rng.chance(1, 2)));
                }
                5 => {
                    s.request_cp().unwrap();
                }
                6 => {
                    let tear = if rng.chance(1, 2) { TORN } else { DROP };
                    s.disk().set_tear_policy(tear);
                    s.crash_power().unwrap();
                    s.boot().unwrap();
                    let v = s.verify_recovery().unwrap();
                    assert!(!v.boot.rewound, "seed {seed}: {:?}", v.boot);
                    assert!(
                        v.lost.is_subset(&v.nvfail),
                        "seed {seed}: lost {:?}",
                        v.lost
                    );
                    if v.boot.dirty {
                        assert_eq!(v.nvfail, s.model.all_volumes(), "seed {seed}");
                    }
                    s.clear_nvfail().unwrap();
                }
                _ => {
                    daemon_crash_and_recover(&mut s).unwrap();
                }
            }
            s.check_disk_prefix().unwrap();
        }
        clean_restart(&mut s).unwrap();
    }
}

/// The same controller with a dead battery: writes the barrier was trusted
/// for tear or vanish in a power cut. The engine never serves a torn
/// image: either a verifiable CP boots, dirty, with every volume held by
/// nvfail and a consistent prefix behind it, or the boot is refused. The
/// simulated controller never persists its cache (not even the format's
/// pages), so refusal is the common outcome here; what the test guards is
/// that nothing torn is ever served.
#[test]
fn an_unprotected_opaque_cache_is_quarantined_or_refused_never_served_torn() {
    let mut booted = 0;
    let mut refused = 0;
    let mut rewound = 0;
    for seed in 470..486u64 {
        let mut cfg = small(seed);
        cfg.flush_opaque = true;
        cfg.flush_policy = FlushPolicy::TrustBarrierWithNvfail;
        cfg.cache_protected = false;
        cfg.tear = match seed % 3 {
            0 => DROP,
            1 => TORN,
            _ => TearPolicy::default(),
        };
        let mut s = sim_with(cfg);
        s.boot().unwrap();
        let mut rng = SplitMix64::new(seed);
        for _ in 0..rng.range(2, 6) {
            for _ in 0..rng.range(1, 5) {
                let vol = rng.below(2) as u32;
                let blocks = rng.range(1, 6) as u32;
                let lba = rng.below(64 - u64::from(blocks) + 1);
                ok(&mut s, vol, lba, blocks);
            }
            s.request_cp().unwrap();
        }
        ok(&mut s, 0, 0, 1);
        s.crash_power().unwrap();
        match s.boot() {
            Ok(info) => {
                booted += 1;
                assert!(info.dirty, "seed {seed}: a power cut is a dirty boot");
                let v = s.verify_recovery().unwrap();
                assert_eq!(v.nvfail, s.model.all_volumes(), "seed {seed}: quarantine");
                rewound += usize::from(v.boot.rewound);
                s.clear_nvfail().unwrap();
                ok(&mut s, 1, 0, 1);
                s.request_cp().unwrap();
                s.check_disk_prefix().unwrap();
            }
            Err(e) => {
                refused += 1;
                assert!(
                    e.contains("boot") || e.contains("torn") || e.contains("verif"),
                    "seed {seed}: {e}"
                );
            }
        }
    }
    assert!(booted >= 1, "{booted} booted, {refused} refused");
    assert!(
        rewound + refused >= 1,
        "no seed tore anything the boot noticed ({booted} booted)"
    );
}

// ---------------------------------------------------------------------------
// Regression: a late discard over a reused segment
// ---------------------------------------------------------------------------

/// A device that holds every discard until released, and reports a write
/// into a segment whose discard it still holds.
struct LateDiscardDisk {
    inner: SimBackstore,
    held: Vec<(u64, u64, Tag)>,
    release: bool,
    violations: Vec<String>,
    held_total: usize,
}

impl LateDiscardDisk {
    fn check(&mut self, what: &str, pba: u64, bytes: usize) {
        let end = pba + (bytes as u64).div_ceil(BLOCK_SIZE as u64);
        for (start, len, _) in &self.held {
            let held_end = start + len / BLOCK_SIZE as u64;
            if pba < held_end && end > *start {
                self.violations.push(format!(
                    "{what} at PBA {pba} ({bytes} bytes) inside the held discard of PBA {start}"
                ));
            }
        }
    }
}

impl Backstore for LateDiscardDisk {
    fn profile(&self) -> &otap_cp::MediaProfile {
        self.inner.profile()
    }

    fn write_stripe(&mut self, pba: u64, image: &[u8], tag: Tag) -> Result<(), Busy> {
        self.check("stripe", pba, image.len());
        self.inner.write_stripe(pba, image, tag)
    }

    fn write_meta(&mut self, pba: u64, image: &[u8], tag: Tag) -> Result<(), Busy> {
        self.check("meta", pba, image.len());
        self.inner.write_meta(pba, image, tag)
    }

    fn flush(&mut self, class: DevClass, tag: Tag) -> Result<(), Busy> {
        self.inner.flush(class, tag)
    }

    fn commit(&mut self, pba: u64, slot: &SuperblockSlot, tag: Tag) -> Result<(), Busy> {
        self.inner.commit(pba, slot, tag)
    }

    fn write_intent(&mut self, pba: u64, slot: &IntentSlot, tag: Tag) -> Result<(), Busy> {
        self.inner.write_intent(pba, slot, tag)
    }

    fn read(&mut self, pba: u64, dst: &mut [u8]) -> Result<(), IoError> {
        self.inner.read(pba, dst)
    }

    fn discard_segment(&mut self, pba: u64, bytes: u64, tag: Tag) -> Result<(), Busy> {
        if self.release {
            self.inner.discard_segment(pba, bytes, tag)
        } else {
            self.held.push((pba, bytes, tag));
            self.held_total += 1;
            Ok(())
        }
    }

    fn poll(&mut self, out: &mut Vec<Done>) -> usize {
        if self.release && !self.held.is_empty() {
            for (pba, bytes, tag) in std::mem::take(&mut self.held) {
                // A discard issued long ago lands now.
                self.inner
                    .discard_segment(pba, bytes, tag)
                    .expect("the device takes the discard");
            }
        }
        self.inner.poll(out)
    }
}

type LateEngine = Engine<LateDiscardDisk, SimFrontDoor, SimClock>;

fn drive(e: &mut LateEngine, mut done: impl FnMut(&mut LateEngine) -> bool, what: &str) {
    for _ in 0..200_000 {
        if done(e) {
            return;
        }
        e.tick().unwrap();
    }
    panic!("{what} did not happen");
}

fn late_write(e: &mut LateEngine, op: u64, vol: u32, lba: u64, blocks: u32) {
    let id = e
        .front_door()
        .submit_write(vol, lba, Model::data_of(op, vol, lba, blocks));
    drive(
        e,
        |e| {
            e.front_door().take_completions().iter().any(|c| {
                assert_eq!(c.status, Status::Ok, "write {op}");
                c.req.id == id
            })
        },
        "the write",
    );
}

/// Found by the random-geometry walk (a reordering device): the engine
/// freed a metadata segment when it issued the discard, reused it for the
/// next CP's pages, and the discard then landed on the new pages ("index:
/// crc32c mismatch" at the next boot). A freed segment now stays allocated
/// until its discard completes. Here every discard is held back while the
/// engine runs many CPs and folds: no write may land inside a held
/// segment, the pool must still make progress with the segments it has
/// left, and once the discards land the segments are free again.
#[test]
fn a_segment_stays_allocated_until_its_discard_completes() {
    let mut cfg = small(490);
    cfg.fold.max_cps = 2;
    let ecfg: EngineConfig = cfg.engine_config();
    let clock = SimClock::new(WALL_BASE);
    let mut inner = SimBackstore::new(cfg.profile(), cfg.seed).unwrap();
    inner.set_schedule(Schedule {
        max_per_poll: 2,
        reorder: true,
        queue_depth: 64,
    });
    Engine::<SimBackstore, SimFrontDoor, SimClock>::format(&mut inner, &clock, &ecfg).unwrap();
    let disk = LateDiscardDisk {
        inner,
        held: Vec::new(),
        release: false,
        violations: Vec::new(),
        held_total: 0,
    };
    let memory = HugeArena::anonymous(cfg.arena_bytes, HugePageSize::TwoMiB).unwrap();
    let mut e = LateEngine::boot(
        ecfg.clone(),
        disk,
        SimFrontDoor::new(),
        clock.clone(),
        memory,
    )
    .map_err(|f| f.error)
    .unwrap();
    drive(&mut e, |e| e.is_serving(), "the boot commit");
    let free_at_start = e.free_segments();
    let mut op = 0;
    let mut rng = SplitMix64::new(490);
    for round in 0..40u64 {
        for _ in 0..3 {
            op += 1;
            let vol = rng.below(2) as u32;
            let blocks = rng.range(1, 4) as u32;
            let lba = rng.below(64 - u64::from(blocks) + 1);
            late_write(&mut e, op, vol, lba, blocks);
        }
        let before = e.cp_seq();
        if round % 3 == 2 {
            e.request_fold();
        } else {
            e.request_cp();
        }
        drive(&mut e, |e| e.cp_seq() > before, "the CP");
        assert!(
            e.backstore().violations.is_empty(),
            "round {round}: {:?}",
            e.backstore().violations
        );
    }
    let held = e.backstore().held.len();
    assert!(held >= 2, "{held} discards held");
    assert!(
        e.free_segments() < free_at_start,
        "held segments are not free: {} of {free_at_start}",
        e.free_segments()
    );
    let freed_before = e
        .take_events()
        .iter()
        .filter(|ev| matches!(ev, Event::SegmentFreed { .. }))
        .count();
    assert_eq!(freed_before, 0, "nothing is freed while its discard pends");
    // Release: the discards land, the segments become free.
    let free_before_release = e.free_segments();
    e.backstore().release = true;
    drive(
        &mut e,
        |e| e.backstore().held.is_empty(),
        "the held discards to be issued",
    );
    let mut freed = 0;
    for _ in 0..200_000 {
        freed += e
            .take_events()
            .iter()
            .filter(|ev| {
                matches!(
                    ev,
                    Event::SegmentFreed {
                        discarded: true,
                        ..
                    }
                )
            })
            .count();
        if freed >= held {
            break;
        }
        e.tick().unwrap();
    }
    assert_eq!(freed, held, "every held discard freed its segment");
    assert!(
        e.free_segments() >= free_before_release + held as u32,
        "{} free before the release, {} after",
        free_before_release,
        e.free_segments()
    );
    assert!(e.backstore().violations.is_empty());
    // More CPs reuse them; the boot afterwards sees no torn CP.
    for round in 0..6u64 {
        op += 1;
        late_write(&mut e, op, 0, round * 8, 4);
        let before = e.cp_seq();
        e.request_cp();
        drive(&mut e, |e| e.cp_seq() > before, "the CP");
    }
    e.shutdown();
    drive(&mut e, |e| e.is_stopped(), "the shutdown");
    let (mut disk, _, _, memory) = e.into_parts();
    disk.release = true;
    let mut e = LateEngine::boot(ecfg, disk, SimFrontDoor::new(), clock, memory)
        .map_err(|f| f.error)
        .unwrap();
    drive(&mut e, |e| e.is_serving(), "the second boot");
    let booted = e
        .take_events()
        .into_iter()
        .find_map(|ev| match ev {
            Event::Booted { dirty, torn, .. } => Some((dirty, torn)),
            _ => None,
        })
        .expect("a Booted event");
    assert_eq!(booted, (false, Vec::new()));
    let mut buf = vec![0u8; BLOCK_SIZE];
    for lba in (0..48).step_by(8) {
        assert_eq!(
            e.peek(0, lba, &mut buf).unwrap()[0],
            PeekSource::Disk,
            "LBA {lba}"
        );
    }
}

// ---------------------------------------------------------------------------
// A mixed walk: everything above, seeded, with crashes at random points
// ---------------------------------------------------------------------------

/// Writes, punches, snapshots, CPs, folds, device schedules and crashes
/// of every kind, with the model and the snapshot images checked after
/// each step; each seed runs to a clean restart that keeps everything.
#[test]
fn a_seeded_walk_of_writes_punches_snapshots_folds_and_crashes_recovers() {
    let mut snapshots_checked = 0usize;
    for seed in 500..508u64 {
        let mut cfg = small(seed);
        cfg.volumes = vec![(0, 96), (1, 64), (2, 40)];
        cfg.fold.max_cps = 3;
        let sizes = [96u64, 64, 40];
        let mut s = sim_with(cfg);
        s.boot().unwrap();
        let mut rng = SplitMix64::new(seed ^ 0x5eed);
        let mut diff_calls: HashMap<u64, usize> = HashMap::new();
        for step in 0..48 {
            match rng.below(16) {
                0..=5 => {
                    let vol = rng.below(3) as u32;
                    let blocks = rng.range(1, 8) as u32;
                    let lba = rng.below(sizes[vol as usize] - u64::from(blocks) + 1);
                    io_respecting_nvfail(&mut s, vol, lba, blocks, None);
                }
                6 | 7 => {
                    let vol = rng.below(3) as u32;
                    let blocks = rng.range(1, 16) as u32;
                    let lba = rng.below(sizes[vol as usize] - u64::from(blocks) + 1);
                    io_respecting_nvfail(&mut s, vol, lba, blocks, Some(rng.chance(1, 3)));
                }
                8 => {
                    let vol = rng.below(3) as u32;
                    if !s.engine().nvfail_set().get(vol) {
                        s.snapshot(vol).unwrap();
                    }
                }
                9 | 10 => {
                    s.request_cp().unwrap();
                }
                11 => {
                    s.request_fold().unwrap();
                }
                12 => {
                    let tear = match rng.below(3) {
                        0 => DROP,
                        1 => TORN,
                        _ => TearPolicy::default(),
                    };
                    power_cut_and_recover(&mut s, tear)
                        .unwrap_or_else(|e| panic!("seed {seed} step {step}: {e}"));
                }
                13 => {
                    daemon_crash_and_recover(&mut s)
                        .unwrap_or_else(|e| panic!("seed {seed} step {step}: {e}"));
                }
                14 => {
                    s.disk().set_schedule(Schedule {
                        max_per_poll: rng.range(1, 3) as usize,
                        reorder: rng.chance(1, 2),
                        queue_depth: rng.range(2, 32) as usize,
                    });
                }
                _ => {
                    s.advance(rng.range(0, 7) * 1_000_000_000);
                    s.ticks(rng.range(1, 20) as u32).unwrap();
                }
            }
            s.check_disk_prefix()
                .unwrap_or_else(|e| panic!("seed {seed} step {step}: {e}"));
            s.verify_snapshots()
                .unwrap_or_else(|e| panic!("seed {seed} step {step}: {e}"));
            let ids: Vec<u64> = s.snaps.iter().map(|x| x.snap_id).collect();
            for id in ids {
                s.check_diff(id)
                    .unwrap_or_else(|e| panic!("seed {seed} step {step}: {e}"));
                *diff_calls.entry(id).or_default() += 1;
            }
        }
        s.disk().set_schedule(Schedule::default());
        clean_restart(&mut s).unwrap_or_else(|e| panic!("seed {seed}: {e}"));
        assert_eq!(s.last_stop(), Stop::Clean);
        snapshots_checked += diff_calls.len();
    }
    assert!(snapshots_checked >= 8, "{snapshots_checked} snapshots seen");
}

// ---------------------------------------------------------------------------
// Regression: the checker and ambiguous zeros
// ---------------------------------------------------------------------------

/// Found by the mixed walk: a punch over an acknowledged write leaves the
/// image as nothing at all would, and the harness's checker took the
/// smallest operation number the image agreed with, then reported the
/// write and the punch as lost after a daemon restart that had kept both
/// (a hole record at LBA 0, re-attached whole). The checker now takes the
/// largest consistent operation number; the engine was right throughout.
#[test]
fn a_punch_over_a_write_is_not_a_loss_after_a_daemon_restart() {
    for (seed, zeroes) in [(495u64, false), (496, true)] {
        let mut s = sim_with(small(seed));
        s.boot().unwrap();
        ok(&mut s, 0, 0, 4);
        punch_ok(&mut s, 0, 0, 64, zeroes);
        punch_ok(&mut s, 1, 0, 8, !zeroes);
        ok(&mut s, 1, 2, 2);
        punch_ok(&mut s, 1, 1, 4, zeroes);
        let v = daemon_crash_and_recover(&mut s).unwrap();
        assert_eq!(v.k, s.model.last_op());
        assert!(v.lost.is_empty());
        assert_eq!(source_of(&mut s, 0, 0), PeekSource::Hole);
        assert_eq!(source_of(&mut s, 1, 3), PeekSource::Hole);
        s.read(0, 0, 64).unwrap();
        s.read(1, 0, 8).unwrap();
        clean_restart(&mut s).unwrap();
    }
}

// ---------------------------------------------------------------------------
// A property walk over volumes that span map pages
// ---------------------------------------------------------------------------

const WHOLE: TearPolicy = TearPolicy {
    drop: 0,
    whole: 1,
    torn: 0,
};

#[derive(Debug, Clone)]
enum WalkOp {
    Write {
        vol: u8,
        lba: u16,
        blocks: u8,
    },
    Punch {
        vol: u8,
        lba: u16,
        blocks: u8,
        zeroes: bool,
    },
    Snapshot {
        vol: u8,
    },
    Cp,
    Fold,
    Power {
        tear: u8,
    },
    Daemon,
    Clean,
    Check,
}

fn walk_op() -> impl Strategy<Value = WalkOp> {
    prop_oneof![
        6 => (any::<u8>(), any::<u16>(), 1..=8u8).prop_map(|(vol, lba, blocks)| WalkOp::Write { vol, lba, blocks }),
        2 => (any::<u8>(), any::<u16>(), 1..=255u8, any::<bool>())
            .prop_map(|(vol, lba, blocks, zeroes)| WalkOp::Punch { vol, lba, blocks, zeroes }),
        1 => any::<u8>().prop_map(|vol| WalkOp::Snapshot { vol }),
        2 => Just(WalkOp::Cp),
        1 => Just(WalkOp::Fold),
        1 => (0..4u8).prop_map(|tear| WalkOp::Power { tear }),
        1 => Just(WalkOp::Daemon),
        1 => Just(WalkOp::Clean),
        1 => Just(WalkOp::Check),
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

/// Two volumes wider than a map page, so writes and punches land on
/// several pages and the folds rewrite more than one; snapshots pin them;
/// crashes of every kind in between. After every recovery the model
/// checks the image and the snapshots; `Check` steps verify the committed
/// prefix, every snapshot image and every live diff; the walk ends in a
/// clean restart that keeps everything.
fn multi_page_walk(seed: u64, ops: &[WalkOp]) -> Result<(), String> {
    let page = MAP_ENTRIES_PER_PAGE as u64;
    let sizes = [page + 300, page + 60];
    let mut cfg = small(seed);
    cfg.volumes = vec![(0, sizes[0]), (1, sizes[1])];
    cfg.fold.max_cps = 3;
    let mut s = Sim::new(cfg)?;
    s.boot()?;
    s.verify_recovery()?;
    let pick = |vol: u8, lba: u16, blocks: u8| -> (u32, u64, u32) {
        let vol = u32::from(vol % 2);
        let size = sizes[vol as usize];
        let blocks = u64::from(blocks.max(1)).min(size);
        // Spread the 16-bit LBA over the whole volume.
        let lba = (u64::from(lba) * 7) % (size - blocks + 1);
        (vol, lba, blocks as u32)
    };
    let check_snapshots = |s: &mut Sim| -> Result<(), String> {
        s.verify_snapshots()?;
        let ids: Vec<u64> = s.snaps.iter().map(|x| x.snap_id).collect();
        for id in ids {
            s.check_diff(id)?;
        }
        Ok(())
    };
    for (i, op) in ops.iter().enumerate() {
        let step = |e: String| format!("seed {seed} op {i} {op:?}: {e}");
        match *op {
            WalkOp::Write { vol, lba, blocks } => {
                let (vol, lba, blocks) = pick(vol, lba, blocks);
                let held = s.engine().nvfail_set().get(vol);
                let c = s.write(vol, lba, blocks).map_err(step)?;
                let expected = if held { Status::NvFailed } else { Status::Ok };
                if c.status != expected {
                    return Err(step(format!("write status {:?}", c.status)));
                }
            }
            WalkOp::Punch {
                vol,
                lba,
                blocks,
                zeroes,
            } => {
                let (vol, lba, blocks) = pick(vol, lba, blocks);
                let held = s.engine().nvfail_set().get(vol);
                let c = s.punch(vol, lba, blocks, zeroes).map_err(step)?;
                let expected = if held { Status::NvFailed } else { Status::Ok };
                if c.status != expected {
                    return Err(step(format!("punch status {:?}", c.status)));
                }
            }
            WalkOp::Snapshot { vol } => {
                let vol = u32::from(vol % 2);
                if !s.engine().nvfail_set().get(vol) && s.snaps.len() < 4 {
                    s.snapshot(vol).map_err(step)?;
                }
            }
            WalkOp::Cp => {
                s.request_cp().map_err(step)?;
            }
            WalkOp::Fold => {
                s.request_fold().map_err(step)?;
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
            WalkOp::Clean => {
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
                s.check_disk_prefix().map_err(step)?;
                check_snapshots(&mut s).map_err(step)?;
            }
        }
    }
    s.check_disk_prefix()?;
    check_snapshots(&mut s)?;
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
    check_snapshots(&mut s)?;
    Ok(())
}

proptest! {
    #![proptest_config(ProptestConfig {
        failure_persistence: None,
        cases: 64,
        max_shrink_iters: 512,
        ..ProptestConfig::default()
    })]

    /// The brief's three property subjects over volumes wider than a map
    /// page: multi-segment map persistence, CP log replay, and snapshot
    /// read isolation across random power cuts (every tear policy), daemon
    /// crashes and clean restarts, with punches and folds in the mix.
    #[test]
    fn random_ops_over_multi_page_volumes_keep_the_map_and_the_snapshots(
        seed in any::<u64>(),
        ops in prop::collection::vec(walk_op(), 4..28),
    ) {
        if let Err(e) = multi_page_walk(seed, &ops) {
            prop_assert!(false, "{e}");
        }
    }
}
