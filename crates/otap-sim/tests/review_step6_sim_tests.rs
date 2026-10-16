//! The Step 6 adversarial review (`docs/reviews/2026-10-16-step6-qa-adversarial.md`):
//! the engine's findings over the simulator. Each test reproduced its
//! finding before the fix and pins the fixed behaviour.

use otap_cp::{
    Engine, EngineError, Event, FlushPolicy, FoldPolicy, Phase, Status, VerifyOptions, VerifyState,
};
use otap_format::{
    SbSlot, SuperblockSlot, TableHead, TableKind, BLOCK_SIZE, PIN_USER, SUPERBLOCK_SLOT_SIZE,
};
use otap_sim::{Schedule, Sim, SimBackstore, SimClock, SimConfig, SimFrontDoor};
use otap_sys::{HugeArena, HugePageSize};

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

/// The snapshot table's head as the newest slot names it.
fn snapshot_head(s: &mut Sim) -> TableHead {
    let slot = newest_slot(s);
    let root = slot.sb.snap_table;
    assert!(!root.is_none(), "the snapshot head exists");
    let at = root.pba as usize * BLOCK_SIZE;
    let bytes = s.disk().durable()[at..at + root.bytes() as usize].to_vec();
    TableHead::decode(&bytes, &slot.sb.agg_id, TableKind::Snapshots).expect("the head decodes")
}

/// Flips one byte on the medium.
fn flip(s: &mut Sim, at: usize) {
    let byte = s.disk().durable()[at] ^ 0x5A;
    s.disk().corrupt_durable(at, &[byte]);
}

/// Whether snapshot `snap_id` is listed as damaged (quarantined).
fn quarantined(s: &mut Sim, snap_id: u64) -> bool {
    s.engine()
        .snapshots()
        .iter()
        .find(|x| x.snap_id == snap_id)
        .is_some_and(|x| x.damaged)
}

/// D8: the cleaner's rewrite of a snapshot's tree built the snapshot's new
/// state with no damage, so a quarantined snapshot (Step 6A's damaged map
/// page, or a record marked `SNAP_UNCERTAIN` from a damaged table chain)
/// came out of the quarantine the moment a cleaning CP relocated one of
/// its blocks: it could be read, diffed, rolled back to and replicated
/// until the next reboot, though it may be a deleted snapshot brought back
/// or a stale tree. A rewrite keeps the quarantine.
#[test]
fn d8_a_cleaner_pass_never_lifts_a_snapshots_quarantine() {
    let mut cfg = SimConfig::small(6840);
    cfg.volumes = vec![(0, 64)];
    cfg.cleaner.background = false;
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
    let _s3 = s.snapshot(0).unwrap();
    ok(&mut s, 0, 30, 2);
    s.request_cp().unwrap();
    // Damage the pin's delta region outside its record (no record lost,
    // D9): s2, last changed before it, is uncertain at the next boot.
    let head = snapshot_head(&mut s);
    assert_eq!(head.deltas.len(), 3, "{head:?}");
    flip(
        &mut s,
        head.deltas[1].region.pba as usize * BLOCK_SIZE + 64 + 64 + 10,
    );
    s.crash_daemon().unwrap();
    s.boot().unwrap();
    assert!(quarantined(&mut s, s2), "s2 is quarantined after the boot");
    let root_of = |s: &mut Sim| {
        s.engine()
            .snapshot_records()
            .iter()
            .find(|r| r.snap_id == s2)
            .map(|r| r.map_root_pba)
    };
    let before = root_of(&mut s);
    // Garbage next to s2's blocks, and cleaning until a CP rewrites s2.
    let mut rewritten = false;
    for round in 0..40u64 {
        ok(&mut s, 0, 30 + (round % 4) * 8, 8);
        s.request_cp().unwrap();
        s.request_clean().unwrap();
        if root_of(&mut s) != before {
            rewritten = true;
            break;
        }
    }
    assert!(rewritten, "the cleaner relocated a block of s2");
    assert!(
        quarantined(&mut s, s2),
        "a cleaning CP lifted the quarantine of s2"
    );
    let mut buf = vec![0u8; BLOCK_SIZE];
    assert!(matches!(
        s.engine().snapshot_read(s2, 8, &mut buf),
        Err(EngineError::SnapshotDamaged { .. })
    ));
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

/// D1 (the Step 5 review's hazard 8, carried into Step 6 and closed here):
/// the cleaner relocated a block that already failed its CRC as it was,
/// under its recorded CRC. Under a trusted opaque barrier an unclean boot
/// checks every data block of the newest CP's stripes against its
/// descriptor, relocation stripes included, so the relocating CP read as
/// torn and the boot rewound past a sound CP, losing the acknowledged
/// writes that CP carried (held under nvfail, but gone). The fix verifies
/// each block against its CRC when the relocation reads it; a block that
/// fails leaves its segment to the cleaner's skip list (`CleanSkipped`),
/// where it keeps answering `Corrupt` from its old address, and the CP is
/// retried without it.
#[test]
fn d1_a_block_that_fails_its_crc_is_never_relocated() {
    let mut cfg = SimConfig::small(6801);
    cfg.flush_opaque = true;
    cfg.flush_policy = FlushPolicy::TrustBarrierWithNvfail;
    cfg.cache_protected = true;
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
    // Most of the first segments die; blocks at 0, 50, 100, ... stay live
    // there, so those segments are the cleaner's candidates.
    for lba in (0..300).step_by(10) {
        if lba % 50 != 0 {
            ok(&mut s, 0, lba, 10);
        }
    }
    s.request_cp().unwrap();
    // Bit rot in one live block of a candidate.
    let e = s.engine().map_entry(0, 52).expect("mapped");
    let damaged_seg = e.lseg();
    let layout = *s.engine().layout();
    let pba = layout.block_pba(e.lseg(), e.off()).expect("on the device");
    s.disk()
        .corrupt_durable(pba as usize * BLOCK_SIZE + 100, &[0xA5; 8]);
    // The CP that cleans also carries acknowledged writes.
    ok(&mut s, 0, 11, 2);
    s.events.clear();
    for _ in 0..3 {
        s.request_clean().unwrap();
    }
    let skipped = s
        .events
        .iter()
        .any(|e| matches!(e, Event::CleanSkipped { pseg, .. } if *pseg == damaged_seg));
    let relocated = s.engine().stats().relocated;
    // An unclean stop with the cleaning CP the newest.
    s.crash_power().unwrap();
    let boot = s.boot().unwrap();
    assert!(
        !boot.rewound,
        "the boot rewound past a sound CP (relocated {relocated}, skipped {skipped}): {:?}",
        boot.torn
    );
    assert!(
        skipped,
        "the segment with the damaged block was not skipped"
    );
    // A power cut loses the arena, so the write-active volume is held; the
    // operator clears it and the committed image serves.
    s.clear_nvfail().unwrap();
    let (status, _) = s.read(0, 52, 1).unwrap();
    assert_eq!(
        status,
        Status::Corrupt,
        "the damaged block still reads Corrupt"
    );
    let (status, _) = s.read(0, 11, 2).unwrap();
    assert_eq!(status, Status::Ok);
}

/// D9: the snapshot table of format 2.4 tolerated an unreadable head or
/// region written by an older CP by booting without what it could not
/// read: one bad 4 KiB head dropped every snapshot (user-pinned ones and
/// replication bases included), and a record lost from a delta dropped the
/// snapshot it created. The boot went on with a warning, the next commit
/// folded the loss into a clean chain, and the blocks only those snapshots
/// held were counted dead, so the cleaner freed them: the snapshots' data
/// gone for good. Format 2.3 refused the boot on any damage of the table.
/// Now a snapshot table that lost records (an unreadable head, or a region
/// whose readable records fall short of its head's count) tears every
/// candidate that names it, so the boot is refused rather than freeing
/// what the lost snapshots pin; damage that loses no record (a page
/// header, padding) keeps the tolerant path, and the catalogue keeps it
/// always (names only, W4 of the Step 5 review).
#[test]
fn d9_a_snapshot_table_that_lost_records_refuses_the_boot() {
    // An unreadable head written by an older CP.
    let mut cfg = SimConfig::small(6900);
    cfg.volumes = vec![(0, 64)];
    cfg.cleaner.background = false;
    let mut s = sim_with(cfg);
    s.boot().unwrap();
    ok(&mut s, 0, 0, 8);
    s.snapshot(0).unwrap();
    // Only the snapshot holds the first blocks now.
    ok(&mut s, 0, 0, 8);
    s.request_cp().unwrap();
    s.request_cp().unwrap();
    let slot = newest_slot(&mut s);
    assert!(slot.sb.snap_head_cp < slot.sb.cp_seq, "an older CP's head");
    flip(&mut s, slot.sb.snap_table.pba as usize * BLOCK_SIZE + 100);
    s.crash_daemon().unwrap();
    match s.boot() {
        Err(e) => assert!(e.contains("snapshot"), "{e}"),
        Ok(info) => panic!(
            "the pool booted without its snapshot table ({info:?}): snapshots {:?}",
            s.engine().snapshots()
        ),
    }

    // A record lost from a delta: the snapshot it created.
    let mut cfg = SimConfig::small(6901);
    cfg.volumes = vec![(0, 64)];
    cfg.cleaner.background = false;
    let mut s = sim_with(cfg);
    s.boot().unwrap();
    ok(&mut s, 0, 0, 8);
    s.snapshot(0).unwrap();
    ok(&mut s, 0, 8, 8);
    let s2 = s.snapshot(0).unwrap();
    ok(&mut s, 0, 8, 8);
    s.request_cp().unwrap();
    s.request_cp().unwrap();
    let head = snapshot_head(&mut s);
    let created = head.deltas.last().copied().expect("s2's delta");
    assert_eq!(created.records, 1, "{head:?}");
    // Inside s2's record: it fails its own CRC.
    flip(&mut s, created.region.pba as usize * BLOCK_SIZE + 64 + 20);
    s.crash_daemon().unwrap();
    match s.boot() {
        Err(e) => assert!(e.contains("snapshot"), "{e}"),
        Ok(info) => panic!(
            "the pool booted without snapshot {s2} ({info:?}): snapshots {:?}",
            s.engine().snapshots()
        ),
    }
}

/// Writes fresh blocks and asks for a CP after each until the pool is full.
fn fill_pool(s: &mut Sim) {
    let mut lba = 0u64;
    for _ in 0..60 {
        ok(s, 0, lba, 7);
        lba += 7;
        s.engine().request_cp();
        s.ticks(400).unwrap();
        if s.engine().is_pool_full() {
            return;
        }
    }
    panic!("the pool never filled");
}

/// R3: a verification aborted on a full pool at its first slice, saying it
/// held deferred frees, although no CP can commit on a full pool and it
/// held nothing: the pool could not be attested exactly when the operator
/// most needs to know what is on it. The full pool aborts a verification
/// only when it holds something, as the tight pool already did.
#[test]
fn r3_a_full_pool_can_still_be_verified() {
    let mut cfg = SimConfig::small(6930);
    cfg.segments = 6;
    cfg.stripes_per_segment = 2;
    cfg.volumes = vec![(0, 512)];
    cfg.reserve_segments = Some(0);
    cfg.cleaner.background = false;
    let mut s = sim_with(cfg);
    s.boot().unwrap();
    fill_pool(&mut s);
    let report = s.attest_with(VerifyOptions::default()).unwrap();
    assert_eq!(report.state, VerifyState::Done, "{report:?}");
}

/// C1 (cited, not reproduced): the reviewer read that a verification which
/// gives way to a pool gone full releases its held frees without asking
/// for a consistency point, so a pool whose failed CP kept its generation
/// frozen would refuse writes until an operator's request. Driven here to
/// exactly that abort ("the pool is full"), the pool went on by itself:
/// no defect was shown, no code was changed, and this test pins the
/// behaviour seen.
#[test]
fn c1_a_verification_that_gives_way_to_a_full_pool_lets_it_recover() {
    let mut cfg = SimConfig::small(6931);
    cfg.segments = 24;
    cfg.volumes = vec![(0, 640)];
    cfg.reserve_segments = Some(0);
    cfg.cleaner.background = false;
    cfg.cleaner.margin_segments = 0;
    let mut s = sim_with(cfg);
    s.boot().unwrap();
    for lba in (0..640u64).step_by(8) {
        ok(&mut s, 0, lba, 8);
    }
    s.request_cp().unwrap();
    s.engine()
        .start_verify(VerifyOptions {
            units_per_tick: 1,
            max_ns_per_tick: 0,
        })
        .unwrap();
    // Overwrites whose dead blocks the verification holds, until it gives
    // way.
    let mut ended = None;
    for round in 0..4000u64 {
        let c = s.write(0, (round * 8) % 640, 8).unwrap();
        if c.status != Status::Ok {
            // Refused: the pool is full; the verification gives way at the
            // next tick.
            s.ticks(5).unwrap();
        } else {
            s.engine().request_cp();
            s.ticks(3).unwrap();
        }
        if let Some(r) = s.engine().verify_report() {
            if r.state != VerifyState::Running {
                ended = Some(r);
                break;
            }
        }
    }
    let report = ended.expect("the verification gave way");
    assert_eq!(report.state, VerifyState::Aborted, "{report:?}");
    assert!(
        report.reason.as_deref().is_some_and(|r| r.contains("full")),
        "the pool went full first: {report:?}"
    );
    assert!(s.engine().is_pool_full(), "the abort met a full pool");
    // No request: the released space alone must let the pool go on.
    s.ticks(2000).unwrap();
    assert!(
        !s.engine().is_pool_full(),
        "the pool stayed full with {} free segments",
        s.engine().free_segments()
    );
    ok(&mut s, 0, 0, 8);
}

type RigEngine = Engine<SimBackstore, SimFrontDoor, SimClock>;

const WALL_BASE: u64 = 1_700_000_000_000_000_000;

/// Ticks `e` until `done` or a budget runs out.
fn rig_until(e: &mut RigEngine, what: &str, mut done: impl FnMut(&mut RigEngine) -> bool) {
    for _ in 0..10_000 {
        if done(e) {
            return;
        }
        e.tick().expect("tick");
    }
    panic!("never reached: {what}");
}

/// A formatted pool booted over a blank arena, serving.
fn fresh_rig(cfg: &SimConfig) -> RigEngine {
    let clock = SimClock::new(WALL_BASE);
    let mut disk = SimBackstore::new(cfg.profile(), cfg.seed).expect("the layout");
    disk.set_schedule(cfg.schedule);
    RigEngine::format(&mut disk, &clock, &cfg.engine_config()).expect("format");
    let memory = HugeArena::anonymous(cfg.arena_bytes, HugePageSize::TwoMiB).expect("memory");
    let mut e = Engine::boot(
        cfg.engine_config(),
        disk,
        SimFrontDoor::new(),
        clock,
        memory,
    )
    .expect("boot");
    rig_until(&mut e, "the boot commit", |e| e.is_serving());
    e
}

/// The memory grown to `bytes`, the old content at its start, as an arena
/// file a larger `--arena-bytes` grew.
fn grown(memory: &HugeArena, bytes: usize) -> HugeArena {
    let mut m = HugeArena::anonymous(bytes, HugePageSize::TwoMiB).expect("arena memory");
    m.as_mut_slice()[..memory.len()].copy_from_slice(memory.as_slice());
    m
}

/// W3: a clean pool's arena resized at boot (Step 6A allows it when nothing
/// is placed and the pool stopped cleanly) was recreated only in memory
/// deferred to the boot commit's `A` slot, though there was nothing to
/// hold. A kill once `A` had landed but before the deferred write left the
/// old header at the new size, and `A` itself made the pool's newest CP a
/// running one: the next boot refused the resize as unclean, and the
/// daemon (whose file was already resized) refused the old size too, so
/// only `--discard-arena` booted it, holding every volume for nothing. A
/// resize with nothing to hold is written at once now.
#[test]
fn w3_a_clean_resize_killed_after_the_boot_commit_landed_still_boots() {
    let cfg = SimConfig::small(6950);
    let mut e = fresh_rig(&cfg);
    let id = e.front_door().submit_write(0, 3, vec![0x5Au8; BLOCK_SIZE]);
    rig_until(&mut e, "the write", |e| {
        e.front_door()
            .take_completions()
            .iter()
            .any(|c| c.req.id == id)
    });
    e.shutdown();
    rig_until(&mut e, "the clean stop", |e| e.is_stopped());
    let (mut disk, _front, clock, memory) = e.into_parts();
    // The boot of the larger arena, stopped with its commit's `A` slot
    // written: the device completes it at the kill.
    disk.set_schedule(Schedule {
        max_per_poll: 1,
        reorder: false,
        queue_depth: 64,
    });
    let bigger = 2 * cfg.arena_bytes;
    let mut e = Engine::boot(
        cfg.engine_config(),
        disk,
        SimFrontDoor::new(),
        clock,
        grown(&memory, bigger),
    )
    .expect("the resize of a clean pool's empty arena is accepted");
    rig_until(&mut e, "the boot commit's A slot", |e| {
        e.phase() == Phase::CommitA
    });
    let (mut disk, _front, clock, memory) = e.into_parts();
    disk.crash_daemon();
    disk.set_schedule(Schedule::default());
    // The next boot at the new size.
    let mut e = match Engine::boot(
        cfg.engine_config(),
        disk,
        SimFrontDoor::new(),
        clock,
        memory,
    ) {
        Ok(e) => e,
        Err(failure) => panic!(
            "the boot after the interrupted resize was refused: {}",
            failure.into_parts().0
        ),
    };
    rig_until(&mut e, "the boot commit", |e| e.is_serving());
    assert!(e.volume(0).unwrap().nvfail.is_none(), "nothing is held");
    assert_eq!(e.arena_memory_mut().len(), bigger);
}
