//! Step 6A: the engine defects the Step 6 audit found, pinned through the
//! simulator's device and the engine-level rig.
//!
//! - **The hold a stopped boot never recorded** (the arena map's pitfall
//!   22). A dirty boot decided its nvfail hold in memory and recorded it
//!   only in its boot commit's slot, but its re-attach had already
//!   rewritten the damaged arena slots and its `Arena::create` had already
//!   formatted a blank or lost arena. A second `kill -9` (or a timeout, or
//!   a failed boot commit) before that slot landed left the next boot a
//!   whole arena and no cause to hold: volumes that lost acknowledged
//!   writes were served without nvfail. The arena's writes now wait until
//!   the boot commit's `A` slot, which carries the hold in its volume
//!   table, has landed.
//! - **A changed arena size** (the arena map's pitfall 13). An arena file
//!   a larger size grew failed its geometry check and the boot recreated
//!   it, destroying every placed write, and another pool's arena under
//!   another size was formatted over. Now the boot refuses unless the old
//!   arena holds nothing and the pool stopped cleanly, or the operator
//!   discards it; another pool's arena is refused as such.
//! - **A snapshot's damaged map page** (the engine map's trap 7.22).
//!   Deleting the snapshot failed every consistency point of the pool from
//!   then on. Now the deletion walks past the page and commits, and a
//!   snapshot found damaged is quarantined while the pool goes on.

use otap_cp::{Engine, EngineError, Event, PeekSource, Phase, Status};
use otap_format::{NvCause, BLOCK_SIZE};
use otap_sim::{Model, Schedule, SimBackstore, SimClock, SimConfig, SimFrontDoor, TearPolicy};
use otap_sys::{HugeArena, HugePageSize};

const WALL_BASE: u64 = 1_700_000_000_000_000_000;

const DROP: TearPolicy = TearPolicy {
    drop: 1,
    whole: 0,
    torn: 0,
};

fn small(seed: u64) -> SimConfig {
    SimConfig::small(seed)
}

/// No completion ever: every phase of a CP waits on the device.
fn stalled() -> Schedule {
    Schedule {
        max_per_poll: 0,
        reorder: false,
        queue_depth: 64,
    }
}

/// One completion per poll, in order.
fn fifo1() -> Schedule {
    Schedule {
        max_per_poll: 1,
        reorder: false,
        queue_depth: 64,
    }
}

type RigEngine = Engine<SimBackstore, SimFrontDoor, SimClock>;

/// Boots and ticks until the boot commit lands.
fn boot_rig(cfg: &SimConfig, disk: SimBackstore, clock: SimClock, memory: HugeArena) -> RigEngine {
    let mut e = Engine::boot(
        cfg.engine_config(),
        disk,
        SimFrontDoor::new(),
        clock,
        memory,
    )
    .expect("boot");
    for _ in 0..10_000 {
        if e.is_serving() {
            return e;
        }
        e.tick().expect("tick");
    }
    panic!("the boot commit never landed");
}

fn fresh_rig(cfg: &SimConfig) -> RigEngine {
    let clock = SimClock::new(WALL_BASE);
    let mut disk = SimBackstore::new(cfg.profile(), cfg.seed).expect("the layout");
    disk.set_schedule(cfg.schedule);
    disk.set_tear_policy(cfg.tear);
    RigEngine::format(&mut disk, &clock, &cfg.engine_config()).expect("format");
    boot_rig(cfg, disk, clock, blank_arena(cfg))
}

fn blank_arena(cfg: &SimConfig) -> HugeArena {
    HugeArena::anonymous(cfg.arena_bytes, HugePageSize::TwoMiB).expect("arena memory")
}

/// Ticks `e` until `done` or a budget runs out.
fn tick_until(e: &mut RigEngine, what: &str, mut done: impl FnMut(&mut RigEngine) -> bool) {
    for _ in 0..10_000 {
        if done(e) {
            return;
        }
        e.tick().expect("tick");
    }
    panic!("never reached: {what}");
}

/// Writes `blocks` blocks of op `op` at `vol`/`lba` and waits for the
/// acknowledgement.
fn acked_write(e: &mut RigEngine, op: u64, vol: u32, lba: u64, blocks: u32) {
    let id = e
        .front_door()
        .submit_write(vol, lba, Model::data_of(op, vol, lba, blocks));
    let mut status = None;
    tick_until(e, "the write's completion", |e| {
        if let Some(c) = e
            .front_door()
            .take_completions()
            .into_iter()
            .find(|c| c.req.id == id)
        {
            status = Some(c.status);
        }
        status.is_some()
    });
    assert_eq!(status, Some(Status::Ok), "write {vol}/{lba}+{blocks}");
}

/// The byte offset of the data of the placed slot holding `vol`/`lba`.
fn placed_slot_data(memory: &HugeArena, vol: u32, lba: u64) -> Option<usize> {
    use otap_arena::{ArenaHeader, SlotDesc, SlotState, ARENA_HEADER_SIZE, SLOT_DESC_SIZE};
    use otap_format::FromBytes;
    let hdr = ArenaHeader::read_from_bytes(&memory.as_slice()[..ARENA_HEADER_SIZE]).ok()?;
    for slot in 0..hdr.slots {
        let at = hdr.desc_off as usize + slot as usize * SLOT_DESC_SIZE;
        let d = SlotDesc::read_from_bytes(&memory.as_slice()[at..at + SLOT_DESC_SIZE]).ok()?;
        if d.state() == SlotState::Placed && d.vol() == vol && d.lba() == lba {
            return Some(hdr.data_off as usize + slot as usize * BLOCK_SIZE);
        }
    }
    None
}

/// The `Booted` event's `(damaged_slots, arena_lost, reattached)`.
fn booted(events: &[Event]) -> Option<(u32, bool, bool)> {
    events.iter().find_map(|ev| match ev {
        Event::Booted {
            damaged_slots,
            arena_lost,
            reattached,
            ..
        } => Some((*damaged_slots, *arena_lost, *reattached)),
        _ => None,
    })
}

/// Boots on a stalled device and ticks until the boot commit waits in
/// `phase` (its `A` slot not written, or written and not landed); returns
/// the engine with the hold decided in memory.
fn boot_into(
    cfg: &SimConfig,
    mut disk: SimBackstore,
    clock: SimClock,
    memory: HugeArena,
    schedule: Schedule,
    phase: Phase,
) -> RigEngine {
    disk.set_schedule(schedule);
    let mut e = Engine::boot(
        cfg.engine_config(),
        disk,
        SimFrontDoor::new(),
        clock,
        memory,
    )
    .expect("boot");
    tick_until(&mut e, "the boot commit's phase", |e| e.phase() == phase);
    assert!(!e.is_serving(), "the boot commit has not landed");
    e
}

/// `kill -9` of the daemon: the device completes what it was given.
fn kill(e: RigEngine) -> (SimBackstore, SimClock, HugeArena) {
    let (mut disk, _front, clock, memory) = e.into_parts();
    disk.crash_daemon();
    (disk, clock, memory)
}

/// An acknowledged write of volume 0 whose arena slot is damaged across a
/// `kill -9`: the first restart must hold volume 0.
fn damaged_slot_after_a_kill(seed: u64) -> (SimConfig, SimBackstore, SimClock, HugeArena) {
    let cfg = small(seed);
    let mut e = fresh_rig(&cfg);
    acked_write(&mut e, 7, 0, 3, 1);
    let (disk, clock, mut memory) = kill(e);
    let at = placed_slot_data(&memory, 0, 3).expect("the write is placed in the arena");
    memory.as_mut_slice()[at + 100] ^= 0xFF;
    (cfg, disk, clock, memory)
}

fn assert_held(e: &mut RigEngine, what: &str) {
    assert_eq!(
        e.volume(0).unwrap().nvfail,
        Some(NvCause::DirtyBoot),
        "volume 0 is held: {what}"
    );
    let read = e.front_door().submit_read(0, 3, 1);
    let mut status = None;
    tick_until(e, "the read", |e| {
        if let Some(c) = e
            .front_door()
            .take_completions()
            .into_iter()
            .find(|c| c.req.id == read)
        {
            status = Some(c.status);
        }
        status.is_some()
    });
    assert_eq!(status, Some(Status::NvFailed), "{what}");
}

// ---------------------------------------------------------------------------
// The hold a stopped boot never recorded (pitfall 22)
// ---------------------------------------------------------------------------

/// The damaged-slot boot is killed while its boot commit writes its
/// metadata. Before the fix its re-attach had already rewritten the
/// damaged slot `Committed`, so the next boot found a whole arena and
/// served volume 0, which had lost an acknowledged write, without nvfail.
#[test]
fn a_boot_killed_before_its_commit_keeps_the_hold_a_damaged_slot_calls_for() {
    let (cfg, disk, clock, memory) = damaged_slot_after_a_kill(6101);
    let mut e = boot_into(&cfg, disk, clock, memory, stalled(), Phase::Metadata);
    assert_eq!(booted(&e.take_events()).map(|b| b.0), Some(1));
    assert_eq!(e.volume(0).unwrap().nvfail, Some(NvCause::DirtyBoot));
    let (mut disk, clock, memory) = kill(e);
    disk.set_schedule(cfg.schedule);
    let mut e = boot_rig(&cfg, disk, clock, memory);
    let events = e.take_events();
    assert_held(&mut e, "after a second kill before the boot commit");
    assert_eq!(
        booted(&events).map(|b| b.0),
        Some(1),
        "the second boot sees the damage again"
    );
}

/// A power cut empties the arena; the restarted daemon's boot is killed
/// before its commit. Before the fix that boot had already formatted the
/// blank arena, so the next boot re-attached an empty, whole arena and
/// served volume 0 without the write the power cut lost.
#[test]
fn a_boot_killed_before_its_commit_keeps_the_hold_a_blank_arena_calls_for() {
    let cfg = small(6102);
    let mut e = fresh_rig(&cfg);
    acked_write(&mut e, 7, 0, 3, 1);
    let (mut disk, _front, clock, _memory) = e.into_parts();
    disk.set_tear_policy(DROP);
    disk.crash_power();
    let e = boot_into(
        &cfg,
        disk,
        clock,
        blank_arena(&cfg),
        stalled(),
        Phase::Metadata,
    );
    assert_eq!(e.volume(0).unwrap().nvfail, Some(NvCause::DirtyBoot));
    let (mut disk, clock, memory) = kill(e);
    disk.set_schedule(cfg.schedule);
    let mut e = boot_rig(&cfg, disk, clock, memory);
    let events = e.take_events();
    assert_held(
        &mut e,
        "after a power cut and a kill before the boot commit",
    );
    assert_eq!(
        booted(&events).map(|b| b.2),
        Some(false),
        "the second boot finds the arena blank again"
    );
}

/// The same with an arena whose sealed header is damaged (a lost arena).
#[test]
fn a_boot_killed_before_its_commit_keeps_the_hold_a_lost_arena_calls_for() {
    let cfg = small(6103);
    let mut e = fresh_rig(&cfg);
    acked_write(&mut e, 7, 0, 3, 1);
    let (disk, clock, mut memory) = kill(e);
    memory.as_mut_slice()[50] ^= 0x10;
    let e = boot_into(&cfg, disk, clock, memory, stalled(), Phase::Metadata);
    let (mut disk, clock, memory) = kill(e);
    disk.set_schedule(cfg.schedule);
    let mut e = boot_rig(&cfg, disk, clock, memory);
    let events = e.take_events();
    assert_held(
        &mut e,
        "after a lost arena and a kill before the boot commit",
    );
    assert_eq!(
        booted(&events).map(|b| (b.1, b.2)),
        Some((true, false)),
        "the second boot finds the arena lost again"
    );
}

/// Any number of kills during the boot: in the metadata phase, in the
/// flush, with the `A` slot in flight (the kill lands it), then a boot
/// that completes, then a later kill. The hold is never lost, and once a
/// boot commit's `A` slot has carried it the arena is written and found
/// whole.
#[test]
fn consecutive_kills_during_the_boot_never_lose_the_hold() {
    let (cfg, mut disk, mut clock, mut memory) = damaged_slot_after_a_kill(6104);
    for (schedule, phase) in [
        (stalled(), Phase::Metadata),
        (fifo1(), Phase::Flushing),
        (stalled(), Phase::Metadata),
    ] {
        let mut e = boot_into(&cfg, disk, clock, memory, schedule, phase);
        let events = e.take_events();
        assert_eq!(e.volume(0).unwrap().nvfail, Some(NvCause::DirtyBoot));
        assert_eq!(booted(&events).map(|b| b.0), Some(1));
        (disk, clock, memory) = kill(e);
    }
    // The A slot is in flight when the daemon dies: the device lands it,
    // the engine never sees it complete.
    let e = boot_into(&cfg, disk, clock, memory, fifo1(), Phase::CommitA);
    let (d, c, m) = kill(e);
    // The next boot selects that commit: its volume table holds volume 0.
    let e = boot_into(&cfg, d, c, m, fifo1(), Phase::Metadata);
    assert_eq!(e.volume(0).unwrap().nvfail, Some(NvCause::DirtyBoot));
    (disk, clock, memory) = kill(e);
    disk.set_schedule(cfg.schedule);
    let mut e = boot_rig(&cfg, disk, clock, memory);
    assert_held(&mut e, "after four kills during the boot");
    // That boot's commit landed and carried the hold: the arena is written
    // and the next boot finds it whole, and the hold in the volume table.
    let (mut disk, clock, memory) = kill(e);
    disk.set_schedule(cfg.schedule);
    let mut e = boot_rig(&cfg, disk, clock, memory);
    assert_eq!(
        booted(&e.take_events()).map(|b| (b.0, b.2)),
        Some((0, true)),
        "the arena re-attached whole"
    );
    assert_held(&mut e, "after the arena was written");
    // The operator clears it; the block is gone, never served as good.
    e.nvfail_clear(0).expect("the clear is accepted");
    e.request_cp();
    tick_until(&mut e, "the clear", |e| {
        e.volume(0).is_some_and(|v| v.nvfail.is_none())
    });
    let mut buf = vec![0u8; BLOCK_SIZE];
    assert_eq!(e.peek(0, 3, &mut buf).unwrap(), vec![PeekSource::Unmapped]);
}

/// A clean daemon crash with nothing damaged still re-attaches and replays
/// every acknowledged write, whatever kills hit the boots in between: the
/// deferral holds nothing back that a boot needs.
#[test]
fn kills_during_the_boot_of_a_whole_arena_lose_nothing_and_hold_nothing() {
    let cfg = small(6105);
    let mut e = fresh_rig(&cfg);
    acked_write(&mut e, 7, 0, 3, 2);
    acked_write(&mut e, 8, 1, 9, 1);
    let (mut disk, mut clock, mut memory) = kill(e);
    for phase in [Phase::Metadata, Phase::Flushing, Phase::CommitA] {
        let e = boot_into(&cfg, disk, clock, memory, fifo1(), phase);
        assert!(e.volume(0).unwrap().nvfail.is_none());
        (disk, clock, memory) = kill(e);
    }
    disk.set_schedule(cfg.schedule);
    let mut e = boot_rig(&cfg, disk, clock, memory);
    let (damaged, lost, reattached) = booted(&e.take_events()).unwrap();
    assert_eq!((damaged, lost, reattached), (0, false, true));
    for vol in [0, 1] {
        assert!(e.volume(vol).unwrap().nvfail.is_none());
    }
    let mut buf = vec![0u8; 2 * BLOCK_SIZE];
    e.peek(0, 3, &mut buf).unwrap();
    assert_eq!(buf, Model::data_of(7, 0, 3, 2), "the write was replayed");
    let mut one = vec![0u8; BLOCK_SIZE];
    e.peek(1, 9, &mut one).unwrap();
    assert_eq!(one, Model::data_of(8, 1, 9, 1));
}

// ---------------------------------------------------------------------------
// A changed arena size (the arena map's pitfall 13)
// ---------------------------------------------------------------------------

/// The memory a hugetlbfs arena file becomes when a larger size grows it:
/// the old arena whole at its start, zeros after.
fn grown(memory: &HugeArena, bytes: usize) -> HugeArena {
    let mut m = HugeArena::anonymous(bytes, HugePageSize::TwoMiB).expect("arena memory");
    m.as_mut_slice()[..memory.len()].copy_from_slice(memory.as_slice());
    m
}

/// The memory cut back to `bytes`, as an arena file mapped at its own
/// length again.
fn cut(memory: &HugeArena, bytes: usize) -> HugeArena {
    let mut m = HugeArena::anonymous(bytes, HugePageSize::TwoMiB).expect("arena memory");
    m.as_mut_slice()
        .copy_from_slice(&memory.as_slice()[..bytes]);
    m
}

fn boot_refused(
    cfg: otap_cp::EngineConfig,
    disk: SimBackstore,
    clock: SimClock,
    memory: HugeArena,
) -> (EngineError, SimBackstore, SimClock, HugeArena) {
    match Engine::boot(cfg, disk, SimFrontDoor::new(), clock, memory) {
        Ok(mut e) => {
            let lost = booted(&e.take_events()).map(|b| b.1);
            panic!("the boot was accepted (arena lost: {lost:?}); it must be refused")
        }
        Err(failure) => {
            let (error, disk, _front, clock, memory) = failure.into_parts();
            (error, disk, clock, memory)
        }
    }
}

/// A larger arena size over an arena file that holds an acknowledged,
/// uncommitted write, after a `kill -9`. Before the fix the file grew, the
/// header's geometry no longer matched, and the boot recreated the arena:
/// the write was destroyed (and only nvfail said so). Now the boot is
/// refused, the memory is untouched, and the arena's own size boots and
/// replays the write.
#[test]
fn a_larger_arena_over_placed_writes_is_refused_not_recreated() {
    let cfg = small(6201);
    let mut e = fresh_rig(&cfg);
    acked_write(&mut e, 7, 0, 3, 1);
    let (disk, clock, memory) = kill(e);
    let bigger = grown(&memory, 2 * cfg.arena_bytes);
    let before = bigger.as_slice().to_vec();
    let (error, disk, clock, bigger) = boot_refused(cfg.engine_config(), disk, clock, bigger);
    assert_eq!(
        error,
        EngineError::ArenaResize {
            recorded_bytes: Some(cfg.arena_bytes as u64),
            offered_bytes: 2 * cfg.arena_bytes as u64,
            placed: 1,
            clean: false,
        }
    );
    assert!(
        error.to_string().contains("not yet in a consistency point"),
        "{error}"
    );
    assert_eq!(bigger.as_slice(), before.as_slice(), "nothing was written");
    // Back to the arena's own size: the write is replayed, nothing held.
    let mut e = boot_rig(&cfg, disk, clock, cut(&bigger, cfg.arena_bytes));
    assert_eq!(booted(&e.take_events()).map(|b| b.2), Some(true));
    assert!(e.volume(0).unwrap().nvfail.is_none());
    let mut buf = vec![0u8; BLOCK_SIZE];
    assert_eq!(e.peek(0, 3, &mut buf).unwrap(), vec![PeekSource::Arena]);
    assert_eq!(buf, Model::data_of(7, 0, 3, 1));
}

/// A dirty pool with nothing in the arena (killed idle) is refused too:
/// only a clean shutdown proves the arena holds nothing the pool lacks.
#[test]
fn a_larger_arena_after_an_unclean_stop_is_refused() {
    let cfg = small(6202);
    let mut e = fresh_rig(&cfg);
    acked_write(&mut e, 7, 0, 3, 1);
    e.request_cp();
    tick_until(&mut e, "the CP", |e| {
        e.unflushed_blocks() == 0 && e.is_quiet()
    });
    let (disk, clock, memory) = kill(e);
    let (error, ..) = boot_refused(
        cfg.engine_config(),
        disk,
        clock,
        grown(&memory, 3 * cfg.arena_bytes),
    );
    assert!(
        matches!(
            error,
            EngineError::ArenaResize {
                placed: 0,
                clean: false,
                ..
            }
        ),
        "{error:?}"
    );
    assert!(error.to_string().contains("did not shut down cleanly"));
}

/// After a clean shutdown the arena holds nothing: a larger size recreates
/// it, nothing is lost and nothing is held.
#[test]
fn a_larger_arena_after_a_clean_shutdown_is_recreated() {
    let cfg = small(6203);
    let mut e = fresh_rig(&cfg);
    acked_write(&mut e, 7, 0, 3, 1);
    e.shutdown();
    tick_until(&mut e, "the clean stop", |e| e.is_stopped());
    let (disk, _front, clock, memory) = e.into_parts();
    let mut e = boot_rig(&cfg, disk, clock, grown(&memory, 2 * cfg.arena_bytes));
    let (damaged, lost, reattached) = booted(&e.take_events()).unwrap();
    assert_eq!((damaged, lost, reattached), (0, false, false));
    assert!(e.volume(0).unwrap().nvfail.is_none());
    let mut buf = vec![0u8; BLOCK_SIZE];
    assert_eq!(e.peek(0, 3, &mut buf).unwrap(), vec![PeekSource::Disk]);
    assert_eq!(buf, Model::data_of(7, 0, 3, 1));
    // The new arena has the new size's ring.
    assert_eq!(
        e.arena_memory_mut().len(),
        2 * cfg.arena_bytes,
        "the arena spans the new memory"
    );
    acked_write(&mut e, 9, 0, 4, 1);
}

/// The operator's explicit discard recreates the arena anyway: the write
/// is gone, and the dirty boot holds the volume.
#[test]
fn a_discarded_arena_is_recreated_and_its_volumes_held() {
    let cfg = small(6204);
    let mut e = fresh_rig(&cfg);
    acked_write(&mut e, 7, 0, 3, 1);
    let (disk, clock, memory) = kill(e);
    let mut ecfg = cfg.engine_config();
    ecfg.discard_arena = true;
    let mut e = Engine::boot(
        ecfg,
        disk,
        SimFrontDoor::new(),
        clock,
        grown(&memory, 2 * cfg.arena_bytes),
    )
    .expect("the discard is accepted");
    tick_until(&mut e, "the boot commit", |e| e.is_serving());
    assert_eq!(booted(&e.take_events()).map(|b| b.1), Some(true), "lost");
    assert_held(&mut e, "after the arena was discarded");
}

/// Another pool's arena under another size is refused as another pool's,
/// never recreated (the aggregate is checked before the geometry). Before
/// the fix the geometry was checked first and the foreign arena was
/// formatted over.
#[test]
fn another_pools_arena_under_another_size_is_refused() {
    let other = small(6205);
    let mut e = fresh_rig(&other);
    acked_write(&mut e, 7, 0, 3, 1);
    let (_disk, _clock, foreign) = kill(e);
    let cfg = small(6206);
    assert_ne!(cfg.engine_config().agg_id, other.engine_config().agg_id);
    let clock = SimClock::new(WALL_BASE);
    let mut disk = SimBackstore::new(cfg.profile(), cfg.seed).expect("the layout");
    RigEngine::format(&mut disk, &clock, &cfg.engine_config()).expect("format");
    let bigger = grown(&foreign, 2 * cfg.arena_bytes);
    let before = bigger.as_slice().to_vec();
    let (error, _, _, bigger) = boot_refused(cfg.engine_config(), disk, clock, bigger);
    assert!(
        matches!(
            error,
            EngineError::Arena(otap_arena::ArenaError::WrongAggregate { .. })
        ),
        "{error:?}"
    );
    assert_eq!(
        bigger.as_slice(),
        before.as_slice(),
        "the foreign arena is untouched"
    );
}

// ---------------------------------------------------------------------------
// A snapshot whose map page is damaged (the engine map's trap 7.22)
// ---------------------------------------------------------------------------

/// A fold policy that never folds on its own.
fn no_auto_fold() -> otap_cp::FoldPolicy {
    otap_cp::FoldPolicy {
        log_bytes: 1 << 40,
        interval_ns: 1 << 60,
        max_cps: u64::MAX,
        max_dirty_pages: usize::MAX,
        max_overlay_entries: usize::MAX,
    }
}

fn ok(s: &mut otap_sim::Sim, vol: u32, lba: u64, blocks: u32) {
    let c = s.write(vol, lba, blocks).expect("the write completes");
    assert_eq!(c.status, Status::Ok, "write {vol}/{lba}+{blocks}");
}

fn cp_failures(s: &otap_sim::Sim, from: usize) -> Vec<String> {
    s.events[from..]
        .iter()
        .filter_map(|e| match e {
            Event::CpFailed { cp_seq, reason } => Some(format!("CP {cp_seq}: {reason}")),
            _ => None,
        })
        .collect()
}

/// A snapshot of volume 0 whose map page 0 rots on the medium after the
/// daemon restarted (a cold page cache). With `shared`, the page is also
/// the live map's (no fold since the snapshot); otherwise a later fold
/// gave the live map its own copy and only the snapshot names the old
/// page. The boot checks a snapshot's root page and index only, so the
/// snapshot boots as sound.
fn damaged_snapshot_page(seed: u64, shared: bool) -> (otap_sim::Sim, u64) {
    let mut cfg = small(seed);
    cfg.fold = no_auto_fold();
    let mut s = otap_sim::Sim::new(cfg).expect("the pool formats");
    s.boot().unwrap();
    ok(&mut s, 0, 0, 4);
    ok(&mut s, 1, 0, 2);
    let snap = s.snapshot(0).unwrap();
    let pinned = s.engine().map().volume(0).unwrap().index()[0];
    assert!(!pinned.is_none());
    if !shared {
        ok(&mut s, 0, 1, 1);
        s.request_fold().unwrap();
        let live = s.engine().map().volume(0).unwrap().index()[0];
        assert_ne!(
            live.page_pba, pinned.page_pba,
            "the live map has its own page"
        );
    }
    // The newest CP is a plain one: the boot verifies the pages a fold
    // wrote, and this damage is older than that.
    ok(&mut s, 1, 4, 1);
    s.request_cp().unwrap();
    s.request_cp().unwrap();
    s.crash_daemon().unwrap();
    s.disk()
        .corrupt_durable(pinned.page_pba as usize * BLOCK_SIZE + 100, &[0xA5; 4]);
    s.boot().unwrap();
    let info = s.engine().snapshots();
    assert!(
        info.iter().any(|i| i.snap_id == snap && !i.damaged),
        "the boot finds nothing wrong with it"
    );
    (s, snap)
}

/// Before the fix the deletion walked the snapshot's pages with a `?`:
/// the damaged page failed `prepare_meta`, the CP failed, the deletion
/// stayed queued, and every later CP of the pool failed the same way, so
/// no write to any volume was ever made durable again. Now the deletion
/// commits, the blocks the damaged page named stay counted (the safe
/// direction, as for a snapshot damaged at boot, E36), and the pool says
/// so.
#[test]
fn deleting_a_snapshot_with_a_damaged_map_page_does_not_wedge_the_pool() {
    for shared in [false, true] {
        let (mut s, snap) = damaged_snapshot_page(6301 + u64::from(shared), shared);
        let from = s.events.len();
        let deleted = s.delete_snapshot(snap);
        assert!(
            deleted.is_ok(),
            "shared {shared}: {deleted:?}; CP failures {:?}",
            cp_failures(&s, from)
        );
        assert!(
            cp_failures(&s, from).is_empty(),
            "{:?}",
            cp_failures(&s, from)
        );
        assert!(s.engine().snapshots().iter().all(|i| i.snap_id != snap));
        // The pool goes on: a healthy volume's write is made durable.
        let before = s.engine().cp_seq();
        ok(&mut s, 1, 7, 1);
        s.request_cp().expect("a CP commits");
        assert!(s.engine().cp_seq() > before);
        assert_eq!(s.engine().unflushed_blocks(), 0);
        // The damage was reported while it mattered.
        assert!(
            s.events[from..].iter().any(|e| matches!(
                e,
                Event::OperatorWarning(otap_cp::OperatorWarning::SnapshotDamaged { snap_id, .. })
                    if *snap_id == snap
            )),
            "the damaged snapshot is reported"
        );
        // A clean restart boots, and the live volume reads as before: the
        // shared page's blocks read corrupt, the others are untouched.
        s.shutdown_clean().unwrap();
        s.boot().unwrap();
        assert_eq!(s.read(1, 0, 2).unwrap().0, Status::Ok);
        let (status, _) = s.read(0, 0, 4).unwrap();
        let expected = if shared { Status::Corrupt } else { Status::Ok };
        assert_eq!(status, expected, "shared {shared}");
    }
}

/// A walk that finds the damaged page quarantines the snapshot: it is
/// listed damaged, the pool status warns, a diff of it is refused, and
/// the consistency points of every volume go on.
#[test]
fn a_snapshot_found_damaged_is_quarantined_and_the_pool_goes_on() {
    let (mut s, snap) = damaged_snapshot_page(6303, false);
    assert!(s.engine().snapshot_diff(snap, None).is_err());
    let info = s.engine().snapshots();
    assert!(
        info.iter().any(|i| i.snap_id == snap && i.damaged),
        "listed damaged: {info:?}"
    );
    let warned = s.engine().pool_status().warnings.iter().any(|w| {
        matches!(w, otap_cp::OperatorWarning::SnapshotDamaged { snap_id, .. } if *snap_id == snap)
    });
    assert!(warned, "the pool status warns");
    assert_eq!(
        s.engine().snapshot_diff(snap, None),
        Err(EngineError::SnapshotDamaged { snap_id: snap })
    );
    // A rollback to it is refused: it would make its damage the volume's.
    s.engine().set_volume_online(0, false).unwrap();
    assert_eq!(
        s.engine().rollback_volume(0, snap),
        Err(EngineError::SnapshotDamaged { snap_id: snap })
    );
    s.engine().set_volume_online(0, true).unwrap();
    let from = s.events.len();
    ok(&mut s, 0, 9, 1);
    ok(&mut s, 1, 9, 1);
    s.request_fold().expect("a fold commits");
    s.request_clean().expect("a cleaning CP commits");
    assert!(
        cp_failures(&s, from).is_empty(),
        "{:?}",
        cp_failures(&s, from)
    );
    // And it can still be deleted.
    s.delete_snapshot(snap).expect("the deletion commits");
    assert!(
        cp_failures(&s, from).is_empty(),
        "{:?}",
        cp_failures(&s, from)
    );
    assert!(
        !s.engine().pool_status().warnings.iter().any(|w| matches!(
            w,
            otap_cp::OperatorWarning::SnapshotDamaged { snap_id, .. } if *snap_id == snap
        )),
        "the warning goes with the snapshot"
    );
}
