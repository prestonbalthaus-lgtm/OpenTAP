//! Adversarial tests for the Step 3 engine through the simulator (the
//! review's pass, ported).
//!
//! Crash consistency at every phase of a CP and of the recovery's own boot
//! commit, power cuts and daemon crashes chained in both orders, intents in
//! flight, every tear policy and completion schedule, small stripes and
//! segments, many volumes, large writes, a full arena and a full pool,
//! runtime volume creation, arena damage between two processes, the engine's
//! edges, the fakes' own promises, and the checker's rules.
//!
//! The first section pins the defects the review found (D1 to D23 in
//! `docs/08-engineering-log.md`): each test is the review's reproduction
//! with the expectation the fix established.
//!
//! Writes go through the harness, one at a time ([`Sim::write`]) or several
//! in flight ([`Sim::submit_write`], [`Sim::collect`], [`Sim::wait`]); every
//! issued write is in the model from the moment it is submitted.

use std::collections::{HashMap, HashSet};
use std::panic::{catch_unwind, AssertUnwindSafe};

use otap_arena::{ArenaHeader, SlotDesc, SlotState, ARENA_HEADER_SIZE, SLOT_DESC_SIZE};
use otap_cp::{
    Backstore, Busy, CpTrigger, DevClass, Done, EndpointId, Engine, EngineConfig, EngineError,
    Event, FrontDoor, IoError, Net, NvfailIntent, PeekSource, Phase, ReqId, Request, Status, Tag,
    VolumeSpec,
};
use otap_format::{
    FromBytes, IntentSlot, IntoBytes, NvCause, SbLocation, SbSlot, SuperblockSlot, Uuid,
    BLOCK_SIZE, INTENT_SLOT_SIZE, MAX_LBA, SUPERBLOCK_SLOT_SIZE,
};
use otap_sim::{
    decode, Completion, Decoded, LoopbackNet, Model, OpKind, Schedule, Sim, SimBackstore, SimClock,
    SimConfig, SimFrontDoor, SplitMix64, Stop, TearPolicy,
};
use otap_sys::{HugeArena, HugePageSize};
use proptest::prelude::*;

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

const WALL_BASE: u64 = 1_700_000_000_000_000_000;

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

/// One completion per poll, first in first out: every phase of the CP state
/// machine lasts at least one tick and the order is known.
fn fifo1() -> Schedule {
    Schedule {
        max_per_poll: 1,
        reorder: false,
        queue_depth: 64,
    }
}

fn stalled() -> Schedule {
    Schedule {
        max_per_poll: 0,
        reorder: false,
        queue_depth: 64,
    }
}

fn sim_with(cfg: SimConfig) -> Sim {
    Sim::new(cfg).expect("the pool formats")
}

fn small(seed: u64) -> SimConfig {
    SimConfig::small(seed)
}

fn precise(seed: u64) -> SimConfig {
    let mut cfg = SimConfig::small(seed);
    cfg.intent = NvfailIntent::Precise;
    cfg
}

/// A harness write that must be acknowledged.
fn ok(s: &mut Sim, vol: u32, lba: u64, blocks: u32) {
    let c = s.write(vol, lba, blocks).expect("the write completes");
    assert_eq!(c.status, Status::Ok, "write {vol}/{lba}+{blocks}");
}

/// The byte offset on the device of the stripe holding the mapped block
/// `vol`/`lba`.
fn stripe_offset_of(s: &mut Sim, vol: u32, lba: u64) -> usize {
    let entry = s.engine().map_entry(vol, lba).expect("mapped");
    let layout = *s.engine().layout();
    let idx = layout.stripe_of_off(entry.off());
    layout
        .stripe_offset(entry.lseg(), idx)
        .expect("on the device") as usize
}

fn kind_of(phase: Phase) -> OpKind {
    match phase {
        Phase::Staging => OpKind::Stripe,
        Phase::Metadata => OpKind::Meta,
        Phase::Flushing => OpKind::Flush,
        Phase::CommitA | Phase::CommitB | Phase::Idle => OpKind::Commit,
    }
}

/// An engine over the fakes with any front door (the harness is fixed to
/// [`SimFrontDoor`]).
type RigEngine<F> = Engine<SimBackstore, F, SimClock>;

fn boot_rig<F: FrontDoor>(
    cfg: &SimConfig,
    disk: SimBackstore,
    front: F,
    clock: SimClock,
    memory: HugeArena,
) -> RigEngine<F> {
    boot_rig_with(cfg.engine_config(), disk, front, clock, memory)
}

/// Boots with an explicit engine configuration (the control plane's view of
/// the volumes) and ticks until the boot commit lands.
fn boot_rig_with<F: FrontDoor>(
    ecfg: EngineConfig,
    disk: SimBackstore,
    front: F,
    clock: SimClock,
    memory: HugeArena,
) -> RigEngine<F> {
    let mut e = Engine::boot(ecfg, disk, front, clock, memory).expect("boot");
    for _ in 0..10_000 {
        if e.is_serving() {
            return e;
        }
        e.tick().expect("tick");
    }
    panic!("the boot commit never landed");
}

fn fresh_rig<F: FrontDoor>(cfg: &SimConfig, front: F) -> RigEngine<F> {
    let clock = SimClock::new(WALL_BASE);
    let mut disk = SimBackstore::new(cfg.profile(), cfg.seed).expect("the layout");
    disk.set_schedule(cfg.schedule);
    disk.set_tear_policy(cfg.tear);
    Engine::<SimBackstore, F, SimClock>::format(&mut disk, &clock, &cfg.engine_config())
        .expect("format");
    let memory = HugeArena::anonymous(cfg.arena_bytes, HugePageSize::TwoMiB).expect("arena memory");
    boot_rig(cfg, disk, front, clock, memory)
}

/// A front door whose client buffers can vanish: `copy_in` fails for the
/// requests named in `fail`.
#[derive(Debug, Default)]
struct FlakyFront {
    inner: SimFrontDoor,
    fail: HashSet<ReqId>,
}

impl FrontDoor for FlakyFront {
    fn fetch(&mut self) -> Option<Request> {
        self.inner.fetch()
    }

    fn copy_in(&mut self, req: &Request, dst: &mut [u8]) -> Result<(), IoError> {
        if self.fail.contains(&req.id) {
            return Err(IoError::Corrupt);
        }
        self.inner.copy_in(req, dst)
    }

    fn copy_out(&mut self, req: &Request, src: &[u8]) -> Result<(), IoError> {
        self.inner.copy_out(req, src)
    }

    fn complete(&mut self, req: Request, status: Status, seq: u64) {
        self.inner.complete(req, status, seq);
    }
}

fn rig_wait<F: FrontDoor>(
    e: &mut RigEngine<F>,
    take: impl Fn(&mut RigEngine<F>) -> Vec<Completion>,
    id: ReqId,
    budget: u32,
) -> Option<Completion> {
    for _ in 0..=budget {
        if let Some(c) = take(e).into_iter().find(|c| c.req.id == id) {
            return Some(c);
        }
        e.tick().expect("tick");
    }
    None
}

/// The byte offset of the data of the placed slot holding `vol`/`lba`.
fn placed_slot_data(memory: &HugeArena, vol: u32, lba: u64) -> Option<usize> {
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

/// The first `Booted` event among `events`, as `(rewound, reattached)`.
fn booted(events: &[Event]) -> Option<(bool, bool)> {
    events.iter().find_map(|ev| match ev {
        Event::Booted {
            rewound,
            reattached,
            ..
        } => Some((*rewound, *reattached)),
        _ => None,
    })
}

// ---------------------------------------------------------------------------
// Review defects, pinned: nvfail in precise mode (D1, D2)
// ---------------------------------------------------------------------------

#[test]
fn precise_a_write_is_not_acknowledged_against_a_superblock_that_has_not_landed() {
    let mut cfg = precise(101);
    cfg.schedule = fifo1();
    let mut s = sim_with(cfg);
    s.boot().unwrap();
    assert_eq!(s.engine().volume(0).unwrap().active_windows, 0, "idle");
    // An empty CP waits in its metadata phase on a stalled device.
    s.disk().set_schedule(stalled());
    s.engine().request_cp();
    s.tick().unwrap();
    assert_eq!(s.engine().phase(), Phase::Metadata);
    // W1 to idle volume 0 is placed and waits for an intent slot.
    let w1 = s.submit_write(0, 0, 1);
    s.tick().unwrap();
    assert!(s.collect().is_empty(), "W1 waits for its intent");
    // The metadata lands: the flush, then W1's intent, are submitted.
    s.disk().set_schedule(fifo1());
    s.tick().unwrap();
    assert_eq!(s.engine().phase(), Phase::Flushing);
    assert_eq!(
        s.disk().pending_kinds(),
        vec![OpKind::Flush, OpKind::Intent]
    );
    // W2 to volume 0 arrives. In the next tick the flush completes and the
    // superblock slot is built (volume 0 has a window, from W1) and
    // submitted. Before the fix W2 was acknowledged against that unlanded
    // slot; now the slot build only narrows the durable set and W2 waits
    // with W1.
    let w2 = s.submit_write(0, 1, 1);
    s.tick().unwrap();
    assert_eq!(s.engine().phase(), Phase::CommitA);
    let done = s.collect();
    assert!(
        !done.iter().any(|x| x.req.id == w1 || x.req.id == w2),
        "neither write is acknowledged before a FUA write lands: {done:?}"
    );
    assert_eq!(s.engine().stats().intents, 0, "no intent slot is durable");
    assert_eq!(
        s.disk().pending_kinds(),
        vec![OpKind::Intent, OpKind::Commit]
    );
    // The power fails before either FUA write lands.
    s.disk().set_tear_policy(DROP);
    s.crash_power().unwrap();
    let boot = s.boot().unwrap();
    assert!(boot.nvfail.is_empty(), "nothing acknowledged, nothing held");
    let v = s.verify_recovery().unwrap();
    assert_eq!(v.k, 0);
    assert!(v.lost.is_empty());
}

#[test]
fn precise_an_intent_issued_before_a_superblock_still_holds_its_volumes_once_the_slot_lands() {
    let mut cfg = precise(102);
    cfg.schedule = fifo1();
    let mut s = sim_with(cfg);
    s.boot().unwrap();
    // Volume 0 is written and carried; one CP passes without a write: one
    // window left.
    ok(&mut s, 0, 0, 1);
    s.request_cp().unwrap();
    s.request_cp().unwrap();
    assert_eq!(s.engine().volume(0).unwrap().active_windows, 1);
    // An empty CP waits in its metadata phase; W1 to idle volume 1 waits
    // for an intent.
    s.disk().set_schedule(stalled());
    s.engine().request_cp();
    s.tick().unwrap();
    let w1 = s.submit_write(1, 0, 1);
    s.tick().unwrap();
    // The metadata lands: the flush is submitted, then the intent, which
    // lists volume 1 and, from the durable set, volume 0.
    s.disk().set_schedule(fifo1());
    s.tick().unwrap();
    assert_eq!(
        s.disk().pending_kinds(),
        vec![OpKind::Flush, OpKind::Intent]
    );
    // The flush completes: the slot is built without volume 0 (its last
    // window is gone). Before the fix it recorded the in-flight intent's
    // sequence as seen, so the landed intent was ignored by the next boot;
    // now a slot records only intents whose completion the engine saw.
    s.tick().unwrap();
    assert_eq!(s.engine().phase(), Phase::CommitA);
    // The intent lands, then A and B.
    s.settle().unwrap();
    let done = s.collect();
    assert!(done
        .iter()
        .any(|x| x.req.id == w1 && x.status == Status::Ok));
    let intents = s.engine().stats().intents;
    // A write to volume 0 is acknowledged without a new intent slot: the
    // landed intent covers it, whichever superblock a boot selects.
    assert_eq!(s.write(0, 1, 1).unwrap().status, Status::Ok);
    assert_eq!(s.engine().stats().intents, intents, "no intent was written");
    s.disk().set_tear_policy(DROP);
    s.crash_power().unwrap();
    s.boot().unwrap();
    let v = s.verify_recovery().unwrap();
    assert!(v.lost.get(0), "the write to volume 0 was lost");
    assert!(
        v.nvfail.get(0) && v.nvfail.get(1),
        "and both volumes are held"
    );
}

// ---------------------------------------------------------------------------
// Review defects, pinned: the arena seen through the engine (D6, D7, D10, D11)
// ---------------------------------------------------------------------------

#[test]
fn a_failed_copy_in_releases_its_reservation() {
    let mut cfg = small(103);
    cfg.volumes = vec![(0, 4096)];
    cfg.segments = 400;
    let mut e = fresh_rig(&cfg, FlakyFront::default());
    let take = |e: &mut RigEngine<FlakyFront>| e.front_door().inner.take_completions();
    // The client's buffer is gone when the engine copies the first write.
    let bad = e
        .front_door()
        .inner
        .submit_write(0, 0, Model::data_of(1, 0, 0, 1));
    e.front_door().fail.insert(bad);
    let c = rig_wait(&mut e, take, bad, 100).expect("the failed write completes");
    assert_eq!(c.status, Status::Failed);
    assert_eq!(
        e.arena_occupancy(),
        0.0,
        "the failed write's slots went back to the ring"
    );
    // 200 writes of 4 fresh blocks, one at a time: 800 blocks through a
    // 507-slot ring, with consistency points draining it. Before the fix
    // the leaked reservation pinned the tail and every write after one lap
    // was deferred for ever.
    for i in 0..200u64 {
        let lba = 1 + i * 4;
        let id = e
            .front_door()
            .inner
            .submit_write(0, lba, Model::data_of(10 + i, 0, lba, 4));
        let done = rig_wait(&mut e, take, id, 3_000);
        assert_eq!(
            done.map(|c| c.status),
            Some(Status::Ok),
            "write {i} never completed: occupancy {:.3}, frozen generations {}, CP {}",
            e.arena_occupancy(),
            e.frozen_generations(),
            e.cp_seq()
        );
    }
}

#[test]
fn a_write_longer_than_the_lap_remainder_completes_on_an_idle_engine() {
    let mut cfg = small(104);
    cfg.volumes = vec![(0, 1024)];
    let mut s = sim_with(cfg);
    s.boot().unwrap();
    ok(&mut s, 0, 0, 250);
    s.request_cp().unwrap();
    assert_eq!(s.engine().arena_occupancy(), 0.0, "the arena is empty");
    let cp_before = s.engine().cp_seq();
    let big = s.submit_write(0, 250, 300);
    let read = s.submit_read(0, 0, 1);
    let mut got = Vec::new();
    for _ in 0..300 {
        s.tick().unwrap();
        got.extend(s.collect());
    }
    let cps = s.engine().cp_seq() - cp_before;
    // The 300-block run skips to the next lap of the empty ring; before the
    // fix it was refused for ever and the read behind it starved while an
    // empty CP followed every retry.
    assert!(
        got.iter()
            .any(|x| x.req.id == big && x.status == Status::Ok),
        "the write never completed; {cps} CPs were committed meanwhile"
    );
    assert!(
        got.iter()
            .any(|x| x.req.id == read && x.status == Status::Ok),
        "the read behind it never completed"
    );
    assert!(cps <= 2, "{cps} CPs for one write");
}

#[test]
fn a_full_arena_on_a_stalled_device_still_serves_reads() {
    let mut cfg = small(105);
    cfg.volumes = vec![(0, 4096)];
    let mut s = sim_with(cfg);
    s.boot().unwrap();
    ok(&mut s, 0, 0, 1);
    // The device stalls; writes fill the arena until one is deferred.
    s.disk().set_schedule(stalled());
    let mut lba = 1u64;
    while s.engine().stats().deferred == 0 {
        s.submit_write(0, lba, 8);
        lba += 8;
        s.tick().unwrap();
        assert!(lba < 4096, "the arena never filled");
    }
    s.collect();
    // A read of a block the arena holds needs neither the device nor a
    // slot; before the fix nothing behind a deferred write was fetched.
    let read = s.submit_read(0, 0, 1);
    let mut served = None;
    for _ in 0..200 {
        s.tick().unwrap();
        if let Some(c) = s.collect().into_iter().find(|x| x.req.id == read) {
            served = Some(c.status);
            break;
        }
    }
    assert_eq!(served, Some(Status::Ok), "the read never completed");
    let data = s.engine().front_door().take_read(read).unwrap();
    assert_eq!(decode(&data), Decoded::Write { op: 1, k: 0 });
    // Writes stay first in, first out: a new write waits behind the
    // deferred one.
    let late = s.submit_write(0, 4000, 1);
    s.ticks(20).unwrap();
    assert!(
        !s.collect().iter().any(|x| x.req.id == late),
        "a write never passes a deferred write"
    );
}

#[test]
fn reattach_drops_a_damaged_slot_and_holds_its_volume() {
    let cfg = small(106);
    let mut e = fresh_rig(&cfg, SimFrontDoor::new());
    let original = Model::data_of(7, 0, 3, 1);
    let id = e.front_door().submit_write(0, 3, original.clone());
    let c = rig_wait(&mut e, |e| e.front_door().take_completions(), id, 100).unwrap();
    assert_eq!(c.status, Status::Ok);
    // kill -9; one byte of the slot's data is damaged before the restart.
    let (mut disk, _front, clock, mut memory) = e.into_parts();
    disk.crash_daemon();
    let at = placed_slot_data(&memory, 0, 3).expect("the write is placed in the arena");
    memory.as_mut_slice()[at + 100] ^= 0xFF;
    let mut e = boot_rig(&cfg, disk, SimFrontDoor::new(), clock, memory);
    let events = e.take_events();
    let damaged = events.iter().find_map(|ev| match ev {
        Event::Booted { damaged_slots, .. } => Some(*damaged_slots),
        _ => None,
    });
    assert_eq!(damaged, Some(1), "the re-attach reported the damage");
    // The acknowledged write is gone and the engine says so: the volume is
    // held (conservative mode holds every volume), the block is unmapped,
    // and nothing damaged ever reaches the disk as good data.
    assert_eq!(e.volume(0).unwrap().nvfail, Some(NvCause::DirtyBoot));
    let read = e.front_door().submit_read(0, 3, 1);
    let c = rig_wait(&mut e, |e| e.front_door().take_completions(), read, 100).unwrap();
    assert_eq!(c.status, Status::NvFailed);
    let mut buf = vec![0u8; BLOCK_SIZE];
    assert_eq!(e.peek(0, 3, &mut buf).unwrap(), vec![PeekSource::Unmapped]);
    e.request_cp();
    for _ in 0..200 {
        e.tick().unwrap();
    }
    assert_eq!(
        e.peek_disk(0, 3, &mut buf).unwrap(),
        vec![PeekSource::Unmapped]
    );
}

#[test]
fn a_block_damaged_in_the_arena_while_the_engine_runs_reads_as_corrupt() {
    // Damage after the write was placed, in the running process: the read
    // path checks the recorded CRC, and the stripe builder reports the block
    // and writes it under the recorded CRC, so the disk answers the same.
    let cfg = small(120);
    let mut s = sim_with(cfg);
    s.boot().unwrap();
    ok(&mut s, 0, 3, 1);
    ok(&mut s, 0, 4, 1);
    let at = placed_slot_data(s.engine().arena_memory_mut(), 0, 3).expect("placed");
    s.engine().arena_memory_mut().as_mut_slice()[at + 100] ^= 0xFF;
    assert_eq!(s.read(0, 3, 1).unwrap().0, Status::Corrupt);
    assert_eq!(
        s.read(0, 4, 1).unwrap().0,
        Status::Ok,
        "the neighbour is fine"
    );
    let mut buf = vec![0u8; BLOCK_SIZE];
    assert!(matches!(
        s.engine().peek(0, 3, &mut buf),
        Err(EngineError::Corrupt { vol: 0, lba: 3 })
    ));
    let seen = s.events.len();
    s.request_cp().unwrap();
    assert!(s.events[seen..]
        .iter()
        .any(|e| matches!(e, Event::ArenaDamage { vol: 0, lba: 3, .. })));
    assert!(matches!(
        s.engine().peek_disk(0, 3, &mut buf),
        Err(EngineError::Corrupt { vol: 0, lba: 3 })
    ));
    assert_eq!(
        s.engine().peek_disk(0, 4, &mut buf).unwrap(),
        vec![PeekSource::Disk]
    );
}

// ---------------------------------------------------------------------------
// Review defects, pinned: volumes and the engine's edges (D5, D16, D17)
// ---------------------------------------------------------------------------

#[test]
fn a_volume_created_at_runtime_serves_only_once_its_creation_is_durable() {
    let mut s = sim_with(small(107));
    s.boot().unwrap();
    s.disk().set_schedule(stalled());
    s.engine().create_volume(2, 16).unwrap();
    let v2 = s.engine().volume(2).unwrap();
    assert!(v2.present && !v2.serving, "{v2:?}");
    // Until the CP that carries the volume lands, it does not exist for
    // the client: no write to it can be acknowledged and then lost with a
    // volume that was never durable (the review's D5).
    let id = s.submit_write(2, 0, 1);
    let done = s.wait(id, 10).unwrap().expect("answered at once");
    assert_eq!(done.status, Status::NoSuchVolume);
    assert_eq!(
        s.engine().create_volume(2, 16),
        Err(EngineError::VolumeExists { vol: 2 })
    );
    // The device runs again: the creation lands.
    s.disk().set_schedule(Schedule::default());
    let seen = s.events.len();
    s.tick_until(|e| e.volume(2).is_some_and(|v| v.serving), "the creation")
        .unwrap();
    assert!(s.events[seen..]
        .iter()
        .any(|e| matches!(e, Event::VolumeCreated { vol: 2, .. })));
    s.model.add_volume(2, 16);
    s.cfg.volumes.push((2, 16));
    ok(&mut s, 2, 0, 1);
    // A daemon crash keeps it, with its write.
    s.crash_daemon().unwrap();
    s.boot().unwrap();
    let v = s.verify_recovery().unwrap();
    assert_eq!(v.k, s.model.last_op());
    let mut buf = vec![0u8; BLOCK_SIZE];
    assert_eq!(
        s.engine().peek(2, 0, &mut buf).unwrap(),
        vec![PeekSource::Arena]
    );
    // A power cut loses the write and holds the (present) volume.
    s.crash_power().unwrap();
    s.boot().unwrap();
    let v2 = s.engine().volume(2).unwrap();
    assert!(
        v2.present && v2.nvfail == Some(NvCause::DirtyBoot),
        "{v2:?}"
    );
    let v = s.verify_recovery().unwrap();
    assert!(v.lost.get(2) && v.nvfail.get(2));
}

#[test]
fn a_volume_the_control_plane_knows_and_the_disk_does_not_is_created_at_boot() {
    let cfg = small(121);
    let mut ecfg = cfg.engine_config();
    let e = fresh_rig(&cfg, SimFrontDoor::new());
    let (disk, _front, clock, memory) = e.into_parts();
    // The control plane's configuration gains volume 3; the disk has never
    // heard of it. The boot creates it with the boot commit.
    ecfg.volumes.push(VolumeSpec {
        vol: 3,
        size_blocks: 8,
    });
    let mut e = boot_rig_with(ecfg, disk, SimFrontDoor::new(), clock, memory);
    let v3 = e.volume(3).unwrap();
    assert!(v3.present && v3.serving, "{v3:?}");
    assert!(e
        .take_events()
        .iter()
        .any(|ev| matches!(ev, Event::VolumeCreated { vol: 3, .. })));
    let id = e
        .front_door()
        .submit_write(3, 0, Model::data_of(1, 3, 0, 1));
    let c = rig_wait(&mut e, |e| e.front_door().take_completions(), id, 100).unwrap();
    assert_eq!(c.status, Status::Ok);
}

#[test]
fn peek_with_an_lba_near_u64_max_answers_out_of_range() {
    let mut s = sim_with(small(109));
    s.boot().unwrap();
    let mut buf = vec![0u8; BLOCK_SIZE];
    let peek = catch_unwind(AssertUnwindSafe(|| {
        s.engine().peek(0, u64::MAX, &mut buf).map(|_| ())
    }));
    let peek_disk = catch_unwind(AssertUnwindSafe(|| {
        s.engine().peek_disk(0, u64::MAX, &mut buf).map(|_| ())
    }));
    assert!(
        matches!(peek, Ok(Err(EngineError::Io(IoError::OutOfRange { .. })))),
        "peek: {:?}",
        peek.map_err(|_| "panicked")
    );
    assert!(
        matches!(
            peek_disk,
            Ok(Err(EngineError::Io(IoError::OutOfRange { .. })))
        ),
        "peek_disk: {:?}",
        peek_disk.map_err(|_| "panicked")
    );
}

#[test]
fn oversized_volumes_and_requests_are_refused_without_failing_the_tick() {
    let mut cfg = small(110);
    cfg.volumes = vec![(0, 4096)];
    let mut s = sim_with(cfg);
    s.boot().unwrap();
    // Sizes are bounded where they are set.
    assert!(matches!(
        s.engine().create_volume(5, MAX_LBA + 10),
        Err(EngineError::BadSize { vol: 5, .. })
    ));
    assert!(matches!(
        s.engine().create_volume(5, 0),
        Err(EngineError::BadSize { vol: 5, .. })
    ));
    // A volume of the whole LBA space is legal in size but its map could
    // never fit a segment of this geometry.
    assert!(matches!(
        s.engine().create_volume(5, MAX_LBA + 1),
        Err(EngineError::MetadataTooLarge { .. })
    ));
    assert!(s.engine().volume(5).is_some_and(|v| !v.present));
    let mut bad = SimConfig::small(1);
    bad.volumes = vec![(0, MAX_LBA + 2)];
    assert!(Sim::new(bad).unwrap_err().contains("out of bounds"));
    // A request longer than the arena is refused with a status, on a
    // healthy tick; before the fix the tick failed and the request was
    // never answered.
    let slots = s.arena_slots().unwrap();
    let id = s.submit_write(0, 0, slots + 1);
    let done = s.wait(id, 20).unwrap().expect("answered");
    assert_eq!(done.status, Status::TooLarge);
    let id = s.submit_read(0, 0, otap_cp::MAX_REQUEST_BLOCKS + 1);
    let done = s.wait(id, 20).unwrap().expect("answered");
    assert_eq!(done.status, Status::TooLarge);
    assert_eq!(s.engine().arena_occupancy(), 0.0, "nothing leaked");
    ok(&mut s, 0, 0, 1);
}

// ---------------------------------------------------------------------------
// Review defects, pinned: small geometries and a full pool (D8, D9)
// ---------------------------------------------------------------------------

#[test]
fn a_geometry_whose_metadata_could_outgrow_a_segment_is_refused() {
    let mut cfg = small(111);
    cfg.stripe_bytes = 8 * 1024;
    cfg.stripes_per_segment = 1;
    cfg.segments = 200;
    cfg.volumes = vec![(0, 16)];
    // One mapped block plus the segment table are two 8 KiB stripes; a
    // segment holds one. In Step 3 the format refused this (its whole-map
    // image had to fit one segment, E14); since Step 4 the metadata stream
    // spans adjacent segments and the pool works (E22).
    let mut s = sim_with(cfg);
    s.boot().unwrap();
    ok(&mut s, 0, 0, 1);
    s.request_cp().unwrap();
    assert_eq!(s.check_disk_prefix().unwrap(), 1);
    ok(&mut s, 0, 5, 3);
    s.request_cp().unwrap();
    s.crash_power().unwrap();
    s.boot().unwrap();
    assert_eq!(s.verify_recovery().unwrap().k, 2);
    // The same device with two-stripe segments is fine too.
    let mut cfg = small(111);
    cfg.stripe_bytes = 8 * 1024;
    cfg.stripes_per_segment = 2;
    cfg.segments = 100;
    cfg.volumes = vec![(0, 16)];
    let mut s = sim_with(cfg);
    s.boot().unwrap();
    ok(&mut s, 0, 0, 1);
    s.request_cp().unwrap();
    assert_eq!(s.check_disk_prefix().unwrap(), 1);
}

/// Writes fresh blocks and asks for a CP after each until the pool is full.
fn fill_pool(s: &mut Sim) -> u64 {
    let mut lba = 0u64;
    for _ in 0..60 {
        ok(s, 0, lba, 7);
        lba += 7;
        s.engine().request_cp();
        s.ticks(400).unwrap();
        if s.engine().is_pool_full() {
            return lba;
        }
    }
    panic!("the pool never filled");
}

#[test]
fn a_full_pool_refuses_writes_serves_reads_and_boots_after_a_daemon_crash() {
    let mut cfg = small(112);
    cfg.segments = 6;
    cfg.stripes_per_segment = 2;
    cfg.volumes = vec![(0, 512)];
    let mut s = sim_with(cfg);
    s.boot().unwrap();
    let lba = fill_pool(&mut s);
    assert!(
        s.events
            .iter()
            .any(|e| matches!(e, Event::CpFailed { reason, .. } if reason.contains("segment"))),
        "the CP reported the full pool"
    );
    // Writes are refused with a status; reads and the committed image are
    // fine; ticks do not fail.
    let c = s.write(0, lba, 1).unwrap();
    assert_eq!(c.status, Status::NoSpace);
    assert_eq!(s.read(0, 0, 7).unwrap().0, Status::Ok);
    let prefix = s.check_disk_prefix().unwrap();
    assert!(prefix >= 1, "the committed image is intact");
    s.ticks(50).unwrap();
    // The last acknowledged write sits in the arena. A daemon crash keeps
    // it; the boot commit finds its metadata room in the reserve the data
    // allocations kept (before the fix the restarted engine could not boot
    // at all, and a failed boot dropped the device and the arena).
    s.crash_daemon().unwrap();
    let boot = s.boot().unwrap();
    assert!(boot.reattached, "{boot:?}");
    let v = s.verify_recovery().unwrap();
    assert!(v.lost.is_empty());
    assert_eq!(s.read(0, 0, 7).unwrap().0, Status::Ok);
    // The boot commit took a segment of the reserve, and the metadata
    // segment the boot CP's own table had leaked until the Step 4 review
    // is recycled: the requested CP finds room for the replayed write, the
    // pool is no longer full, and a clean shutdown lands its marker. (Before
    // the review's fix the pool stayed full here and the shutdown stopped
    // without the marker.)
    s.engine().request_cp();
    s.ticks(400).unwrap();
    assert!(!s.engine().is_pool_full(), "the replayed write was flushed");
    assert_eq!(s.check_disk_prefix().unwrap(), s.model.last_op());
    s.shutdown_clean().unwrap();
    // One segment is left, which the shutdown commit must not take (it is
    // the boot's): the final CP finds no room and the engine stops without
    // the clean marker; the arena keeps nothing unflushed by then.
    assert_eq!(
        s.last_stop(),
        Stop::Daemon,
        "stopped without the clean marker"
    );
    s.boot().unwrap();
    let v = s.verify_recovery().unwrap();
    assert!(v.lost.is_empty(), "every acknowledged write is on disk");
    assert_eq!(v.k, s.model.last_op());
}

#[test]
fn metadata_segments_are_recycled_so_empty_cps_never_exhaust_the_pool() {
    let mut cfg = small(122);
    cfg.segments = 6;
    cfg.stripes_per_segment = 2;
    cfg.volumes = vec![(0, 32)];
    let mut s = sim_with(cfg);
    s.boot().unwrap();
    ok(&mut s, 0, 0, 2);
    for _ in 0..40 {
        s.request_cp().unwrap();
    }
    assert!(
        s.engine().stats().recycled > 0,
        "metadata segments were recycled"
    );
    assert!(
        s.engine().free_segments() >= 2,
        "{} free",
        s.engine().free_segments()
    );
    assert_eq!(s.check_disk_prefix().unwrap(), 1);
    // Every CP on the ring still boots after the recycling.
    ok(&mut s, 0, 8, 2);
    let cp = s.request_cp().unwrap();
    s.crash_daemon().unwrap();
    s.boot().unwrap();
    assert_eq!(s.verify_recovery().unwrap().k, 2);
    assert!(s.engine().cp_seq() > cp);
}

// ---------------------------------------------------------------------------
// Review defects, pinned: flush-opaque backstores (D3, D4, D14, D15 -> E19)
// ---------------------------------------------------------------------------

/// The owner's E19 ruling (E21): strict mode refuses a flush-opaque
/// geometry at format and at boot, with the parts given back; the default
/// mode accepts it, warns in the pool status and runs.
#[test]
fn flush_opaque_geometries_follow_the_flush_policy() {
    let mut cfg = small(114);
    cfg.flush_opaque = true;
    cfg.flush_policy = otap_cp::FlushPolicy::RequireFuaOrFlush;
    let err = Sim::new(cfg).unwrap_err();
    assert!(err.contains("RequireFuaOrFlush"), "{err}");
    // The boot refuses too, with the parts given back.
    let cfg = small(115);
    let clock = SimClock::new(WALL_BASE);
    let mut disk = SimBackstore::new(cfg.profile(), cfg.seed).unwrap();
    Engine::<SimBackstore, SimFrontDoor, SimClock>::format(&mut disk, &clock, &cfg.engine_config())
        .unwrap();
    let mut opaque = cfg.clone();
    opaque.flush_opaque = true;
    opaque.flush_policy = otap_cp::FlushPolicy::RequireFuaOrFlush;
    let mut lying = SimBackstore::new(opaque.profile(), cfg.seed).unwrap();
    lying.corrupt_durable(0, disk.durable());
    let memory = HugeArena::anonymous(cfg.arena_bytes, HugePageSize::TwoMiB).unwrap();
    let failure = Engine::boot(
        cfg.engine_config(),
        lying,
        SimFrontDoor::new(),
        clock,
        memory,
    )
    .expect_err("refused");
    assert!(matches!(failure.error, EngineError::FlushOpaqueRefused));
    let (_, disk_back, _, _, memory_back) = failure.into_parts();
    assert_eq!(disk_back.durable().len(), disk.durable().len());
    assert_eq!(memory_back.len(), cfg.arena_bytes);
    // The default policy trusts the barrier: the pool formats, boots with
    // the structured warning, serves, and survives a power cut whose
    // controller cache held (a healthy battery).
    let mut cfg = small(116);
    cfg.flush_opaque = true;
    cfg.flush_policy = otap_cp::FlushPolicy::TrustBarrierWithNvfail;
    cfg.cache_protected = true;
    let mut s = sim_with(cfg);
    s.boot().unwrap();
    let status = s.engine().pool_status();
    assert!(status.flush_opaque);
    assert_eq!(
        status.flush_policy,
        otap_cp::FlushPolicy::TrustBarrierWithNvfail
    );
    assert!(matches!(
        status.warnings.as_slice(),
        [otap_cp::OperatorWarning::FlushOpaqueTrusted { .. }]
    ));
    assert!(s
        .events
        .iter()
        .any(|e| matches!(e, Event::OperatorWarning(_))));
    ok(&mut s, 0, 0, 4);
    s.request_cp().unwrap();
    ok(&mut s, 1, 0, 2);
    s.crash_power().unwrap();
    s.boot().unwrap();
    let verdict = s.verify_recovery().unwrap();
    assert!(
        verdict.k >= 1,
        "the committed CP survived in the protected cache"
    );
}

#[test]
fn a_rewind_with_a_reattached_arena_holds_the_rewound_volumes() {
    // A daemon crash plus media damage under the newest CP: the boot
    // rewinds and re-attaches at once. Before the fix nvfail engaged only
    // when the arena did not re-attach, so the rewound CP's acknowledged
    // writes were gone without a hold.
    let mut s = sim_with(small(510));
    s.boot().unwrap();
    ok(&mut s, 0, 0, 2);
    let cp = s.request_cp().unwrap();
    ok(&mut s, 1, 0, 1);
    let off = stripe_offset_of(&mut s, 0, 0);
    s.crash_daemon().unwrap();
    s.disk().corrupt_durable(off + 8, &[0xFF; 8]);
    let boot = s.boot().unwrap();
    assert!(boot.rewound && boot.reattached && boot.dirty, "{boot:?}");
    assert_eq!(boot.cp_seq, cp - 1);
    assert_eq!(s.engine().volume(0).unwrap().nvfail, Some(NvCause::TornCp));
    assert_eq!(s.engine().volume(1).unwrap().nvfail, Some(NvCause::TornCp));
    let mut buf = vec![0u8; 2 * BLOCK_SIZE];
    assert_eq!(
        s.engine().peek(0, 0, &mut buf).unwrap(),
        vec![PeekSource::Unmapped, PeekSource::Unmapped],
        "the rewound CP's write is gone"
    );
    let mut one = vec![0u8; BLOCK_SIZE];
    assert_eq!(
        s.engine().peek(1, 0, &mut one).unwrap(),
        vec![PeekSource::Arena],
        "the later write was replayed"
    );
}

#[test]
fn a_rewound_boot_is_dirty_even_onto_a_clean_shutdown_cp() {
    // The boot commit after a clean shutdown is torn by media damage and
    // the power fails: the boot rewinds onto the clean CP. A rewind proves
    // the engine ran past the selected CP, so the boot is dirty.
    let mut s = sim_with(small(118));
    s.boot().unwrap();
    ok(&mut s, 0, 0, 2);
    s.shutdown_clean().unwrap();
    let boot = s.boot().unwrap();
    assert!(!boot.dirty);
    ok(&mut s, 0, 4, 2);
    // Damage the boot commit's metadata region on the medium, then cut the
    // power with the acknowledged write still in the arena.
    let layout = *s.engine().layout();
    let sb_pba = layout.sb_pba(SbSlot::A0);
    let mut slot_bytes = vec![0u8; SUPERBLOCK_SLOT_SIZE];
    s.disk().read(sb_pba, &mut slot_bytes).unwrap();
    let sb_a0 = SuperblockSlot::read_from_bytes(&slot_bytes).unwrap();
    let sb_pba = layout.sb_pba(SbSlot::A1);
    s.disk().read(sb_pba, &mut slot_bytes).unwrap();
    let sb_a1 = SuperblockSlot::read_from_bytes(&slot_bytes).unwrap();
    let newest = if sb_a0.sb.cp_seq > sb_a1.sb.cp_seq {
        sb_a0
    } else {
        sb_a1
    };
    let root = newest.sb.seg_table;
    s.disk()
        .corrupt_durable(root.pba as usize * BLOCK_SIZE + 16, &[0xFF; 8]);
    s.crash_power().unwrap();
    let boot = s.boot().unwrap();
    assert!(boot.rewound && boot.dirty, "{boot:?}");
    let v = s.verify_recovery().unwrap();
    assert!(v.lost.get(0) && v.nvfail.get(0));
    assert_eq!(s.engine().volume(0).unwrap().nvfail, Some(NvCause::TornCp));
}

// ---------------------------------------------------------------------------
// Review defects, pinned: the checker and the fakes (D21, D22, D23)
// ---------------------------------------------------------------------------

#[test]
fn the_checker_accepts_a_hold_kept_by_a_clear_lost_to_a_daemon_crash() {
    let mut s = sim_with(small(116));
    s.boot().unwrap();
    ok(&mut s, 0, 0, 1);
    s.crash_power().unwrap();
    s.boot().unwrap();
    s.verify_recovery().unwrap();
    s.disk().set_schedule(stalled());
    s.engine().nvfail_clear(0).unwrap();
    s.ticks(3).unwrap();
    // The daemon dies before the CP that makes the clear durable: the
    // volume is held again, which is the documented safe side (E17).
    s.crash_daemon().unwrap();
    s.disk().set_schedule(Schedule::default());
    s.boot().unwrap();
    assert_eq!(
        s.engine().volume(0).unwrap().nvfail,
        Some(NvCause::DirtyBoot)
    );
    assert_eq!(s.engine().volume(0).unwrap().rollback_gen, 0);
    s.verify_recovery().unwrap();
}

#[test]
fn the_checker_accepts_a_precise_hold_from_an_intent_whose_write_was_never_acknowledged() {
    let mut s = sim_with(precise(117));
    s.boot().unwrap();
    s.disk().set_schedule(stalled());
    s.submit_write(0, 0, 1);
    s.tick().unwrap();
    s.tick().unwrap();
    assert_eq!(s.disk().pending_kinds(), vec![OpKind::Intent]);
    assert!(s.collect().is_empty(), "not acknowledged");
    // The intent reaches the medium in the power cut; its write was never
    // acknowledged. Holding volume 0 is right: the engine cannot know the
    // acknowledgement never went out, and the model counts the issued
    // write as activity.
    s.disk().set_tear_policy(WHOLE);
    s.crash_power().unwrap();
    s.disk().set_schedule(Schedule::default());
    s.boot().unwrap();
    assert_eq!(
        s.engine().volume(0).unwrap().nvfail,
        Some(NvCause::DirtyBoot)
    );
    let v = s.verify_recovery().unwrap();
    assert_eq!(v.k, 0);
    assert!(v.lost.is_empty());
    assert_eq!(s.model.unresolved(), 0, "the lost write is settled");
}

#[test]
fn the_loopback_net_holds_chunks_across_a_partition_and_delivers_them_in_order() {
    let net = LoopbackNet::new(2);
    let (a, b) = (EndpointId(0), EndpointId(1));
    let mut end_a = net.end(a);
    let mut end_b = net.end(b);
    end_a.send(b, b"one").unwrap();
    net.set_partitioned(true);
    end_a.send(b, b"two").unwrap();
    net.tick();
    let mut got = Vec::new();
    assert_eq!(
        end_b.poll_recv(&mut got),
        1,
        "only the chunk sent before the cut"
    );
    assert_eq!(net.held(), 1);
    net.set_partitioned(false);
    end_a.send(b, b"three").unwrap();
    net.tick();
    end_b.poll_recv(&mut got);
    let chunks: Vec<&[u8]> = got.iter().map(|(_, c)| c.as_slice()).collect();
    assert_eq!(chunks, [b"one".as_slice(), b"two", b"three"]);
    assert_eq!(
        end_a.send(EndpointId(9), b"x"),
        Err(otap_cp::NetError::Unreachable)
    );
}

// ---------------------------------------------------------------------------
// The fakes' own promises
// ---------------------------------------------------------------------------

fn bare_disk(seed: u64) -> SimBackstore {
    let cfg = small(seed);
    SimBackstore::new(cfg.profile(), seed).expect("layout")
}

/// Polls until every tag in `tags` completed; returns the completion order.
fn poll_until_all(disk: &mut SimBackstore, tags: &[Tag]) -> Vec<Tag> {
    let mut order = Vec::new();
    for _ in 0..1_000 {
        let mut out = Vec::new();
        disk.poll(&mut out);
        for d in out {
            assert!(d.result.is_ok());
            order.push(d.tag);
        }
        if tags.iter().all(|t| order.contains(t)) {
            return order;
        }
    }
    panic!("{tags:?} never all completed");
}

fn poll_until(disk: &mut SimBackstore, tag: Tag) -> Vec<Tag> {
    poll_until_all(disk, &[tag])
}

#[test]
fn a_power_cut_never_tears_a_fua_write_that_completed() {
    for seed in 0..20u64 {
        let mut disk = bare_disk(seed);
        disk.set_schedule(Schedule {
            max_per_poll: 4,
            reorder: true,
            queue_depth: 64,
        });
        disk.set_tear_policy(TORN);
        let layout = *disk.layout();
        let agg = Uuid::from_bytes([seed as u8; 16]);
        let mut slot = SuperblockSlot::new(agg, 5, SbLocation::A);
        slot.seal().unwrap();
        let sb_pba = layout.sb_pba(slot.sb.sb_slot().unwrap_or(SbSlot::A0));
        let mut intent = IntentSlot::new(agg, 3);
        intent.seal();
        let intent_pba = layout.intent_pba(intent.slot);
        let stripe_pba = layout.stripe_pba(1, 0).unwrap();
        let stripe = vec![0xA5u8; layout.stripe_bytes as usize];
        disk.commit(sb_pba, &slot, Tag(1)).unwrap();
        disk.write_intent(intent_pba, &intent, Tag(2)).unwrap();
        disk.write_stripe(stripe_pba, &stripe, Tag(3)).unwrap();
        poll_until_all(&mut disk, &[Tag(1), Tag(2), Tag(3)]);
        let report = disk.crash_power();
        assert_eq!(
            report.torn, 1,
            "seed {seed}: only the cached stripe is unsafe"
        );
        let sb_at = sb_pba as usize * BLOCK_SIZE;
        let in_at = intent_pba as usize * BLOCK_SIZE;
        assert_eq!(
            &disk.durable()[sb_at..sb_at + SUPERBLOCK_SLOT_SIZE],
            slot.as_bytes(),
            "seed {seed}"
        );
        assert_eq!(
            &disk.durable()[in_at..in_at + INTENT_SLOT_SIZE],
            intent.as_bytes(),
            "seed {seed}"
        );
    }
}

#[test]
fn a_flush_that_completes_before_an_earlier_write_does_not_make_it_durable() {
    let mut found = false;
    for seed in 0..64u64 {
        let mut disk = bare_disk(seed);
        disk.set_schedule(Schedule {
            max_per_poll: 1,
            reorder: true,
            queue_depth: 64,
        });
        let layout = *disk.layout();
        let pba = layout.stripe_pba(2, 1).unwrap();
        let stripe = vec![0x5Au8; layout.stripe_bytes as usize];
        disk.write_stripe(pba, &stripe, Tag(1)).unwrap();
        disk.flush(DevClass::Pool, Tag(2)).unwrap();
        let order = poll_until(&mut disk, Tag(2));
        if order.contains(&Tag(1)) {
            continue; // the write completed first; try another order
        }
        found = true;
        disk.set_tear_policy(DROP);
        disk.crash_power();
        let at = pba as usize * BLOCK_SIZE;
        assert!(
            disk.durable()[at..at + stripe.len()]
                .iter()
                .all(|b| *b == 0),
            "seed {seed}: a flush made a write it did not cover durable"
        );
    }
    assert!(found, "no seed completed the flush first");
}

#[test]
fn a_daemon_crash_lets_the_device_finish_what_it_had_and_loses_only_the_completions() {
    let mut disk = bare_disk(5);
    disk.set_schedule(stalled());
    let layout = *disk.layout();
    let before = layout.stripe_pba(1, 0).unwrap();
    let after = layout.stripe_pba(1, 1).unwrap();
    let stripe = vec![0x3Cu8; layout.stripe_bytes as usize];
    disk.write_stripe(before, &stripe, Tag(1)).unwrap();
    disk.flush(DevClass::Pool, Tag(2)).unwrap();
    disk.write_stripe(after, &stripe, Tag(3)).unwrap();
    disk.crash_daemon();
    assert_eq!(disk.pending(), 0);
    let mut out = Vec::new();
    disk.set_schedule(Schedule::default());
    assert_eq!(
        disk.poll(&mut out),
        0,
        "the completions died with the process"
    );
    let b = before as usize * BLOCK_SIZE;
    let a = after as usize * BLOCK_SIZE;
    assert_eq!(
        &disk.durable()[b..b + stripe.len()],
        stripe.as_slice(),
        "flushed"
    );
    assert!(
        disk.durable()[a..a + stripe.len()].iter().all(|x| *x == 0),
        "the write after the flush is only cached"
    );
    assert_eq!(disk.cached(), 1);
}

#[test]
fn the_loopback_net_never_reorders_even_when_the_delay_shrinks() {
    let net = LoopbackNet::new(3);
    let (a, b, c) = (EndpointId(0), EndpointId(1), EndpointId(2));
    let mut end_a = net.end(a);
    let mut end_b = net.end(b);
    let mut end_c = net.end(c);
    net.set_delay(5);
    end_a.send(c, b"slow").unwrap();
    net.set_delay(0);
    end_a.send(c, b"fast").unwrap();
    end_b.send(c, b"other").unwrap();
    let mut got = Vec::new();
    assert_eq!(
        end_c.poll_recv(&mut got),
        0,
        "nothing passes the slow chunk"
    );
    for _ in 0..5 {
        net.tick();
    }
    assert_eq!(end_c.poll_recv(&mut got), 3);
    let order: Vec<(EndpointId, &[u8])> = got.iter().map(|(e, x)| (*e, x.as_slice())).collect();
    assert_eq!(
        order,
        vec![(a, b"slow".as_slice()), (a, b"fast"), (b, b"other")]
    );
}

// ---------------------------------------------------------------------------
// Crash consistency at every phase
// ---------------------------------------------------------------------------

const PHASES: [Phase; 5] = [
    Phase::Staging,
    Phase::Metadata,
    Phase::Flushing,
    Phase::CommitA,
    Phase::CommitB,
];

fn crash_in_phase(
    intent: NvfailIntent,
    tear: TearPolicy,
    phase: Phase,
    schedule: Schedule,
    seed: u64,
) -> Result<(), String> {
    let mut cfg = small(seed);
    cfg.intent = intent;
    cfg.schedule = schedule;
    let mut s = Sim::new(cfg)?;
    s.boot()?;
    s.write(0, 0, 4)?;
    s.write(1, 0, 4)?;
    s.request_cp()?;
    s.write(0, 4, 4)?;
    s.write(1, 8, 2)?;
    s.engine().request_cp();
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
    let v = s.verify_recovery()?;
    if phase == Phase::CommitB && v.k != 4 {
        return Err(format!("A had landed, yet the prefix is {}", v.k));
    }
    s.clear_nvfail()?;
    let c = s.write(0, 20, 2)?;
    if c.status != Status::Ok {
        return Err(format!("write after the clear: {:?}", c.status));
    }
    s.shutdown_clean()?;
    s.boot()?;
    let v = s.verify_recovery()?;
    if v.k != s.model.last_op() {
        return Err(format!("clean restart: prefix {}", v.k));
    }
    Ok(())
}

#[test]
fn a_power_cut_in_every_phase_under_every_tear_policy_and_schedule_recovers() {
    let schedules = [
        fifo1(),
        Schedule::default(),
        Schedule {
            max_per_poll: 2,
            reorder: false,
            queue_depth: 2,
        },
    ];
    let mut failures = Vec::new();
    let mut seed = 200;
    for intent in [NvfailIntent::Conservative, NvfailIntent::Precise] {
        for tear in [DROP, WHOLE, TORN, TearPolicy::default()] {
            for phase in PHASES {
                for schedule in schedules {
                    seed += 1;
                    if let Err(e) = crash_in_phase(intent, tear, phase, schedule, seed) {
                        failures.push(format!(
                            "{intent:?} {tear:?} {phase:?} {schedule:?} seed {seed}: {e}"
                        ));
                    }
                }
            }
        }
    }
    assert!(failures.is_empty(), "{failures:#?}");
}

/// Boots with a stalled device (the boot commit waits in its metadata
/// phase; the harness gives up), then steps the device until the commit is
/// in `phase`.
fn stall_boot_into(s: &mut Sim, phase: Phase) -> Result<(), String> {
    s.disk().set_schedule(stalled());
    if s.boot().is_ok() {
        return Err("a stalled device landed the boot commit".into());
    }
    s.disk().set_schedule(fifo1());
    for _ in 0..10 {
        if s.engine().phase() == phase {
            return Ok(());
        }
        s.tick()?;
    }
    Err(format!("the boot commit never reached {phase:?}"))
}

fn second_cut_in_boot_commit(
    intent: NvfailIntent,
    phase: Phase,
    tear: TearPolicy,
    seed: u64,
) -> Result<(), String> {
    let mut cfg = small(seed);
    cfg.intent = intent;
    let mut s = Sim::new(cfg)?;
    s.boot()?;
    s.write(0, 0, 4)?;
    s.write(1, 0, 4)?;
    s.request_cp()?;
    s.write(0, 8, 4)?;
    s.crash_power()?;
    stall_boot_into(&mut s, phase)?;
    s.disk().set_tear_policy(tear);
    s.crash_power()?;
    s.disk().set_schedule(Schedule::default());
    s.disk().set_tear_policy(TearPolicy::default());
    s.boot()?;
    let v = s.verify_recovery()?;
    if v.k < 2 {
        return Err(format!("the committed CP was lost: prefix {}", v.k));
    }
    s.clear_nvfail()?;
    s.write(1, 12, 2)?;
    s.shutdown_clean()?;
    s.boot()?;
    let v = s.verify_recovery()?;
    if v.k != s.model.last_op() {
        return Err(format!("clean restart: prefix {}", v.k));
    }
    Ok(())
}

#[test]
fn a_second_power_cut_inside_the_recoverys_own_boot_commit_recovers() {
    let mut failures = Vec::new();
    let tears = [DROP, WHOLE, TORN];
    let mut seed = 400;
    for intent in [NvfailIntent::Conservative, NvfailIntent::Precise] {
        for (i, phase) in [
            Phase::Metadata,
            Phase::Flushing,
            Phase::CommitA,
            Phase::CommitB,
        ]
        .into_iter()
        .enumerate()
        {
            seed += 1;
            let tear = tears[(i + seed as usize) % tears.len()];
            if let Err(e) = second_cut_in_boot_commit(intent, phase, tear, seed) {
                failures.push(format!("{intent:?} {phase:?} {tear:?}: {e}"));
            }
        }
    }
    assert!(failures.is_empty(), "{failures:#?}");
}

fn chained_crashes(intent: NvfailIntent, seed: u64) -> Result<(), String> {
    let mut cfg = small(seed);
    cfg.intent = intent;
    let mut s = Sim::new(cfg)?;
    s.boot()?;
    s.write(0, 0, 4)?;
    s.request_cp()?;
    s.write(1, 4, 4)?;
    s.write(0, 4, 2)?;
    // A daemon crash, then a power cut before the replayed writes reach a
    // CP: they are lost with the arena, and their volumes are held.
    s.crash_daemon()?;
    s.boot()?;
    s.verify_recovery()?;
    s.crash_power()?;
    s.boot()?;
    let v = s.verify_recovery()?;
    if !v.lost.get(1) {
        return Err(format!("the replayed write survived a power cut? {v:?}"));
    }
    // A power cut, the clear made durable, writes, then a daemon crash.
    s.clear_nvfail()?;
    s.write(1, 8, 3)?;
    s.write(0, 8, 1)?;
    s.crash_daemon()?;
    s.boot()?;
    s.verify_recovery()?;
    // A daemon crash inside the re-attach's boot commit, then another.
    s.write(1, 16, 2)?;
    s.crash_daemon()?;
    stall_boot_into(&mut s, Phase::CommitA)?;
    s.crash_daemon()?;
    s.disk().set_schedule(Schedule::default());
    s.boot()?;
    let v = s.verify_recovery()?;
    if v.k != s.model.last_op() {
        return Err(format!("two daemon crashes lost writes: prefix {}", v.k));
    }
    // A power cut inside the re-attach's boot commit.
    s.write(0, 30, 2)?;
    s.crash_daemon()?;
    stall_boot_into(&mut s, Phase::Flushing)?;
    s.crash_power()?;
    s.disk().set_schedule(Schedule::default());
    s.boot()?;
    s.verify_recovery()?;
    s.clear_nvfail()?;
    s.shutdown_clean()?;
    s.boot()?;
    let v = s.verify_recovery()?;
    if v.k != s.model.last_op() {
        return Err(format!("clean restart: prefix {}", v.k));
    }
    Ok(())
}

#[test]
fn daemon_crashes_and_power_cuts_chained_in_both_orders_recover() {
    for (intent, seed) in [
        (NvfailIntent::Conservative, 501),
        (NvfailIntent::Precise, 502),
    ] {
        chained_crashes(intent, seed).unwrap_or_else(|e| panic!("{intent:?}: {e}"));
    }
}

#[test]
fn a_power_cut_with_an_intent_in_flight_and_dropped_holds_nothing_and_loses_nothing_acknowledged() {
    let mut s = sim_with(precise(503));
    s.boot().unwrap();
    ok(&mut s, 1, 0, 2);
    s.request_cp().unwrap();
    s.disk().set_schedule(stalled());
    s.submit_write(0, 0, 1);
    s.tick().unwrap();
    s.tick().unwrap();
    assert!(s.disk().pending_kinds().contains(&OpKind::Intent));
    assert!(s.collect().is_empty());
    s.disk().set_tear_policy(DROP);
    s.crash_power().unwrap();
    s.disk().set_schedule(Schedule::default());
    s.boot().unwrap();
    let v = s.verify_recovery().unwrap();
    assert_eq!(
        v.k, 1,
        "the committed write survives, the unacknowledged one is gone"
    );
    assert!(
        !v.nvfail.get(0),
        "the intent never landed: volume 0 is not held"
    );
}

#[test]
fn a_clear_is_durable_before_the_volume_serves_so_a_generation_is_never_reissued() {
    let mut s = sim_with(small(504));
    s.boot().unwrap();
    ok(&mut s, 0, 0, 1);
    s.crash_power().unwrap();
    s.boot().unwrap();
    s.verify_recovery().unwrap();
    // The operator clears volume 0. Until the CP that records the bumped
    // generation lands, the volume keeps answering NvFailed (before the fix
    // it served at once under a generation that was not durable).
    let first = s.engine().nvfail_clear(0).unwrap();
    assert_eq!(first, 1);
    assert_eq!(s.engine().volume(0).unwrap().clear_pending, Some(1));
    s.disk().set_schedule(stalled());
    let id = s.submit_write(0, 2, 1);
    let done = s.wait(id, 5).unwrap().expect("answered at once");
    assert_eq!(done.status, Status::NvFailed, "held until the clear lands");
    assert_eq!(s.engine().nvfail_clear(0).unwrap(), 1, "the same clear");
    // A power cut before that CP: held again, generation untouched.
    s.crash_power().unwrap();
    s.disk().set_schedule(Schedule::default());
    s.boot().unwrap();
    assert_eq!(
        s.engine().volume(0).unwrap().nvfail,
        Some(NvCause::DirtyBoot)
    );
    assert_eq!(s.engine().volume(0).unwrap().rollback_gen, 0);
    s.verify_recovery().unwrap();
    // The clear lands this time; a write is acknowledged under generation
    // 1 (the identity the client sees, section 7.6) and lost to a power
    // cut; the next clear hands out generation 2, never 1 again.
    assert_eq!(s.engine().nvfail_clear(0).unwrap(), 1);
    s.tick_until(|e| !e.nvfail_set().get(0), "the clear")
        .unwrap();
    assert_eq!(s.engine().volume(0).unwrap().rollback_gen, 1);
    s.disk().set_schedule(stalled());
    let id = s.submit_write(0, 2, 1);
    assert_eq!(s.wait(id, 5).unwrap().map(|x| x.status), Some(Status::Ok));
    s.crash_power().unwrap();
    s.disk().set_schedule(Schedule::default());
    s.boot().unwrap();
    let v = s.verify_recovery().unwrap();
    assert!(v.lost.get(0));
    let second = s.engine().nvfail_clear(0).unwrap();
    assert!(
        second > first,
        "generation {second} reissued after generation {first} was served"
    );
}

#[test]
fn a_volume_created_at_runtime_survives_crashes_once_its_cp_landed() {
    // The harness's configuration gives a runtime volume to every reboot
    // (`Sim::create_volume` records it), but this drives the engine
    // directly with a boot configuration that remembers volume 2, as a
    // control plane would.
    let cfg = small(505);
    let mut ecfg = cfg.engine_config();
    ecfg.volumes.push(VolumeSpec {
        vol: 2,
        size_blocks: 16,
    });
    let mut e = fresh_rig(&cfg, SimFrontDoor::new());
    let take = |e: &mut RigEngine<SimFrontDoor>| e.front_door().take_completions();
    e.create_volume(2, 16).unwrap();
    let before = e.cp_seq();
    for _ in 0..1_000 {
        if e.cp_seq() > before {
            break;
        }
        e.tick().unwrap();
    }
    assert!(e.cp_seq() > before, "the creation is durable");
    assert!(e.volume(2).unwrap().serving);
    let data = Model::data_of(9, 2, 3, 2);
    let id = e.front_door().submit_write(2, 3, data.clone());
    assert_eq!(rig_wait(&mut e, take, id, 100).unwrap().status, Status::Ok);
    // kill -9 and restart.
    let (mut disk, _front, clock, memory) = e.into_parts();
    disk.crash_daemon();
    let mut e = boot_rig_with(ecfg.clone(), disk, SimFrontDoor::new(), clock, memory);
    assert!(e.volume(2).unwrap().present);
    let mut buf = vec![0u8; 2 * BLOCK_SIZE];
    assert_eq!(
        e.peek(2, 3, &mut buf).unwrap(),
        vec![PeekSource::Arena, PeekSource::Arena]
    );
    assert_eq!(buf, data, "the acknowledged write was replayed");
    // A power cut before the replayed write reaches a CP: the volume is
    // present and held.
    let (mut disk, _front, clock, _memory) = e.into_parts();
    disk.crash_power();
    let memory = HugeArena::anonymous(cfg.arena_bytes, HugePageSize::TwoMiB).expect("arena memory");
    let e = boot_rig_with(ecfg, disk, SimFrontDoor::new(), clock, memory);
    let v2 = e.volume(2).unwrap();
    assert!(
        v2.present && v2.nvfail == Some(NvCause::DirtyBoot),
        "{v2:?}"
    );
}

#[test]
fn a_torn_attach_counter_cannot_destroy_the_arena() {
    let cfg = small(506);
    let mut e = fresh_rig(&cfg, SimFrontDoor::new());
    let data = Model::data_of(3, 0, 5, 2);
    let id = e.front_door().submit_write(0, 5, data.clone());
    let take = |e: &mut RigEngine<SimFrontDoor>| e.front_door().take_completions();
    assert_eq!(rig_wait(&mut e, take, id, 100).unwrap().status, Status::Ok);
    let (mut disk, _front, clock, mut memory) = e.into_parts();
    disk.crash_daemon();
    // The restarted daemon is killed in the middle of the only store an
    // attach makes: the attach counter word, half written. Before the fix
    // the whole 4 KiB header was rewritten on every attach, and a kill
    // inside that copy left a header whose CRC failed; the next boot
    // formatted the arena anew and every acknowledged write was gone.
    memory.as_mut_slice()[64..72].copy_from_slice(&0x0000_0003_0000_0002u64.to_le_bytes());
    let mut e = boot_rig(&cfg, disk, SimFrontDoor::new(), clock, memory);
    let (rewound, reattached) = booted(&e.take_events()).unwrap();
    assert!(reattached && !rewound);
    let mut buf = vec![0u8; 2 * BLOCK_SIZE];
    let sources = e.peek(0, 5, &mut buf).unwrap();
    assert_eq!(
        (sources, buf == data),
        (vec![PeekSource::Arena, PeekSource::Arena], true),
        "the acknowledged write survived the restart"
    );
    assert!(e.volume(0).unwrap().nvfail.is_none());
}

#[test]
fn a_damaged_arena_header_is_a_lost_arena_not_a_blank_one() {
    let cfg = small(511);
    let mut e = fresh_rig(&cfg, SimFrontDoor::new());
    let id = e
        .front_door()
        .submit_write(0, 5, Model::data_of(3, 0, 5, 1));
    let take = |e: &mut RigEngine<SimFrontDoor>| e.front_door().take_completions();
    assert_eq!(rig_wait(&mut e, take, id, 100).unwrap().status, Status::Ok);
    let (mut disk, _front, clock, mut memory) = e.into_parts();
    disk.crash_daemon();
    // The sealed identity is damaged: the arena's contents cannot be
    // trusted, so the boot treats it like a power loss and says so.
    memory.as_mut_slice()[50] ^= 0x10;
    let mut e = boot_rig(&cfg, disk, SimFrontDoor::new(), clock, memory);
    let lost = e.take_events().iter().find_map(|ev| match ev {
        Event::Booted {
            arena_lost,
            reattached,
            ..
        } => Some((*arena_lost, *reattached)),
        _ => None,
    });
    assert_eq!(lost, Some((true, false)));
    assert_eq!(e.volume(0).unwrap().nvfail, Some(NvCause::DirtyBoot));
}

// ---------------------------------------------------------------------------
// I/O errors (the fake device never fails, so this wraps it)
// ---------------------------------------------------------------------------

/// One planned failure: `(kind, nth submission of that kind, landed)`.
type Fault = (OpKind, usize, bool);

/// A device that fails chosen submissions: the `nth` (from 0) submission of
/// `kind` completes with a device error, after reaching the medium when
/// `landed` is set (a controller that reports an error for a write it
/// performed) or without touching it otherwise.
struct ErrDisk {
    inner: SimBackstore,
    plan: Vec<Fault>,
    seen: HashMap<OpKind, usize>,
    injected: Vec<Done>,
    /// Planned failures that fired.
    fired: usize,
    flip: HashSet<Tag>,
}

impl ErrDisk {
    fn new(inner: SimBackstore, plan: Vec<Fault>) -> Self {
        Self {
            inner,
            plan,
            seen: HashMap::new(),
            injected: Vec::new(),
            fired: 0,
            flip: HashSet::new(),
        }
    }

    fn submit(
        &mut self,
        kind: OpKind,
        tag: Tag,
        go: impl FnOnce(&mut SimBackstore) -> Result<(), Busy>,
    ) -> Result<(), Busy> {
        let nth = *self.seen.get(&kind).unwrap_or(&0);
        let planned = self
            .plan
            .iter()
            .find(|(k, i, _)| *k == kind && *i == nth)
            .map(|(_, _, landed)| *landed);
        match planned {
            None => {
                go(&mut self.inner)?;
            }
            Some(false) => self.injected.push(Done {
                tag,
                result: Err(IoError::Device { errno: 5 }),
            }),
            Some(true) => {
                go(&mut self.inner)?;
                self.flip.insert(tag);
            }
        }
        *self.seen.entry(kind).or_insert(0) += 1;
        self.fired += usize::from(planned.is_some());
        Ok(())
    }
}

impl Backstore for ErrDisk {
    fn profile(&self) -> &otap_cp::MediaProfile {
        self.inner.profile()
    }

    fn write_stripe(&mut self, pba: u64, image: &[u8], tag: Tag) -> Result<(), Busy> {
        self.submit(OpKind::Stripe, tag, |d| d.write_stripe(pba, image, tag))
    }

    fn write_meta(&mut self, pba: u64, image: &[u8], tag: Tag) -> Result<(), Busy> {
        self.submit(OpKind::Meta, tag, |d| d.write_meta(pba, image, tag))
    }

    fn flush(&mut self, class: DevClass, tag: Tag) -> Result<(), Busy> {
        self.submit(OpKind::Flush, tag, |d| d.flush(class, tag))
    }

    fn commit(&mut self, pba: u64, slot: &SuperblockSlot, tag: Tag) -> Result<(), Busy> {
        self.submit(OpKind::Commit, tag, |d| d.commit(pba, slot, tag))
    }

    fn write_intent(&mut self, pba: u64, slot: &IntentSlot, tag: Tag) -> Result<(), Busy> {
        self.submit(OpKind::Intent, tag, |d| d.write_intent(pba, slot, tag))
    }

    fn read(&mut self, pba: u64, dst: &mut [u8]) -> Result<(), IoError> {
        self.inner.read(pba, dst)
    }

    fn discard_segment(&mut self, pba: u64, bytes: u64, tag: Tag) -> Result<(), Busy> {
        self.inner.discard_segment(pba, bytes, tag)
    }

    fn poll(&mut self, out: &mut Vec<Done>) -> usize {
        let mut done = Vec::new();
        self.inner.poll(&mut done);
        for mut d in done {
            if self.flip.remove(&d.tag) {
                d.result = Err(IoError::Device { errno: 5 });
            }
            out.push(d);
        }
        out.append(&mut self.injected);
        out.len()
    }
}

/// Runs a short workload over a device that fails per `plan`, optionally
/// cutting the power right after the first failed CP attempt, and checks
/// the image after a power cut against the model.
fn with_io_errors(
    intent: NvfailIntent,
    plan: Vec<Fault>,
    cut_after_failure: bool,
    seed: u64,
) -> Result<(), String> {
    let mut cfg = small(seed);
    cfg.intent = intent;
    cfg.schedule = fifo1();
    let ecfg = cfg.engine_config();
    let clock = SimClock::new(WALL_BASE);
    let mut inner = SimBackstore::new(cfg.profile(), seed).map_err(|e| e.to_string())?;
    inner.set_schedule(cfg.schedule);
    Engine::<SimBackstore, SimFrontDoor, SimClock>::format(&mut inner, &clock, &ecfg)
        .map_err(|e| e.to_string())?;
    let memory =
        HugeArena::anonymous(cfg.arena_bytes, HugePageSize::TwoMiB).map_err(|e| e.to_string())?;
    let mut e = Engine::boot(
        ecfg.clone(),
        ErrDisk::new(inner, plan),
        SimFrontDoor::new(),
        clock,
        memory,
    )
    .map_err(|e| format!("boot: {e}"))?;
    let mut model = Model::new(&cfg.volumes);
    let mut failed_cps = 0;
    let tick =
        |e: &mut Engine<ErrDisk, SimFrontDoor, SimClock>, failed: &mut u32| -> Result<(), String> {
            e.tick().map_err(|err| format!("tick: {err}"))?;
            for ev in e.take_events() {
                if matches!(ev, Event::CpFailed { .. }) {
                    *failed += 1;
                }
            }
            Ok(())
        };
    for _ in 0..2_000 {
        if e.is_serving() {
            break;
        }
        tick(&mut e, &mut failed_cps)?;
    }
    if !e.is_serving() {
        return Err("never served".into());
    }
    let mut op = 0u64;
    'work: for round in 0..3u64 {
        for (vol, lba) in [(0u32, round * 8), (1, round * 8 + 2)] {
            op += 1;
            let id = e
                .front_door()
                .submit_write(vol, lba, Model::data_of(op, vol, lba, 3));
            let mut status = None;
            for _ in 0..2_000 {
                if let Some(c) = e
                    .front_door()
                    .take_completions()
                    .into_iter()
                    .find(|c| c.req.id == id)
                {
                    status = Some((c.status, c.seq));
                    break;
                }
                tick(&mut e, &mut failed_cps)?;
            }
            match status {
                Some((Status::Ok, seq)) => model.acked_now(op, vol, lba, 3, seq),
                other => return Err(format!("write {op}: {other:?}")),
            }
        }
        let before = e.cp_seq();
        e.request_cp();
        for _ in 0..2_000 {
            if e.cp_seq() > before {
                break;
            }
            tick(&mut e, &mut failed_cps)?;
            if cut_after_failure && failed_cps > 0 {
                break 'work;
            }
        }
        if !cut_after_failure && e.cp_seq() <= before {
            return Err(format!("round {round}: the CP never committed"));
        }
    }
    let fired = e.backstore().fired;
    let (err_disk, _front, clock, _memory) = e.into_parts();
    let mut inner = err_disk.inner;
    inner.crash_power();
    let memory =
        HugeArena::anonymous(cfg.arena_bytes, HugePageSize::TwoMiB).map_err(|e| e.to_string())?;
    let mut e = Engine::boot(ecfg, inner, SimFrontDoor::new(), clock, memory)
        .map_err(|e| format!("reboot: {e}"))?;
    for _ in 0..2_000 {
        if e.is_serving() {
            break;
        }
        e.tick().map_err(|err| format!("tick: {err}"))?;
    }
    let mut image = HashMap::new();
    for (vol, size) in &cfg.volumes {
        for lba in 0..*size {
            let mut buf = vec![0u8; BLOCK_SIZE];
            e.peek(*vol, lba, &mut buf)
                .map_err(|err| format!("peek {vol}/{lba}: {err}"))?;
            image.insert((*vol, lba), buf);
        }
    }
    let k = model.check_prefix(|vol, lba| {
        image
            .get(&(vol, lba))
            .cloned()
            .ok_or_else(|| "missing".to_string())
    })?;
    let lost = model.lost_volumes(k);
    let nvfail = e.nvfail_set();
    if !lost.is_subset(&nvfail) {
        return Err(format!("lost writes on {lost:?}, nvfail holds {nvfail:?}"));
    }
    if fired == 0 {
        return Err("the plan never fired".into());
    }
    Ok(())
}

#[test]
fn failed_stripes_metadata_flushes_commits_and_intents_are_retried_and_recover() {
    use OpKind::{Commit, Flush, Intent, Meta, Stripe};
    // Submission counts: the format wipes with metadata writes (not seen by
    // this wrapper); the boot commit writes meta 0, flush 0, commits 0 (A)
    // and 1 (B); the first data CP writes stripes from 0, then the next
    // meta, flush and commits 2 (A) and 3 (B).
    let cases: Vec<(NvfailIntent, Vec<Fault>, bool)> = vec![
        (NvfailIntent::Conservative, vec![(Stripe, 0, false)], false),
        (NvfailIntent::Conservative, vec![(Stripe, 1, true)], false),
        (NvfailIntent::Conservative, vec![(Meta, 1, false)], false),
        (NvfailIntent::Conservative, vec![(Flush, 1, false)], false),
        (NvfailIntent::Conservative, vec![(Commit, 2, false)], false),
        (NvfailIntent::Conservative, vec![(Commit, 3, false)], false),
        (NvfailIntent::Conservative, vec![(Commit, 2, true)], false),
        (NvfailIntent::Conservative, vec![(Commit, 3, true)], false),
        (NvfailIntent::Conservative, vec![(Commit, 2, true)], true),
        (NvfailIntent::Conservative, vec![(Commit, 3, false)], true),
        (
            NvfailIntent::Conservative,
            vec![(Stripe, 0, false), (Meta, 2, false), (Commit, 4, true)],
            false,
        ),
        (NvfailIntent::Precise, vec![(Intent, 0, false)], false),
        (NvfailIntent::Precise, vec![(Intent, 1, true)], false),
        (NvfailIntent::Precise, vec![(Commit, 2, true)], true),
        (NvfailIntent::Precise, vec![(Commit, 3, false)], true),
    ];
    let mut failures = Vec::new();
    for (i, (intent, plan, cut)) in cases.into_iter().enumerate() {
        if let Err(e) = with_io_errors(intent, plan.clone(), cut, 800 + i as u64) {
            failures.push(format!("{intent:?} {plan:?} cut {cut}: {e}"));
        }
    }
    assert!(failures.is_empty(), "{failures:#?}");
}

#[test]
fn shutdown_during_the_reattach_boot_commit_flushes_the_replayed_writes() {
    let mut s = sim_with(small(508));
    s.boot().unwrap();
    ok(&mut s, 0, 0, 3);
    ok(&mut s, 1, 4, 3);
    s.crash_daemon().unwrap();
    stall_boot_into(&mut s, Phase::Metadata).unwrap();
    s.engine().shutdown();
    s.disk().set_schedule(Schedule::default());
    s.tick_until(|e| e.is_stopped(), "the final CP").unwrap();
    s.shutdown_clean().unwrap();
    let boot = s.boot().unwrap();
    assert!(!boot.dirty, "{boot:?}");
    let v = s.verify_recovery().unwrap();
    assert_eq!(v.k, 2, "the replayed writes reached the final CP");
    assert_eq!(s.check_disk_prefix().unwrap(), 2);
}

// ---------------------------------------------------------------------------
// Shutdown, requests and triggers around a CP in flight
// ---------------------------------------------------------------------------

#[test]
fn shutdown_and_requests_during_a_cp_in_flight() {
    for (i, phase) in PHASES.into_iter().enumerate() {
        let mut cfg = small(600 + i as u64);
        cfg.schedule = fifo1();
        let mut s = sim_with(cfg);
        s.boot().unwrap();
        ok(&mut s, 0, 0, 3);
        ok(&mut s, 1, 5, 3);
        let before = s.engine().cp_seq();
        s.engine().request_cp();
        s.tick_until(|e| e.phase() == phase, "the phase").unwrap();
        // A request during the CP: one more CP follows, no more.
        s.engine().request_cp();
        s.engine().request_cp();
        s.settle().unwrap();
        s.ticks(20).unwrap();
        assert_eq!(s.engine().cp_seq(), before + 2, "{phase:?}");
        // Shutdown during the next CP.
        ok(&mut s, 0, 10, 2);
        s.engine().request_cp();
        s.tick_until(|e| e.phase() == phase, "the phase").unwrap();
        s.shutdown_clean().unwrap();
        let boot = s.boot().unwrap();
        assert!(!boot.dirty && boot.nvfail.is_empty(), "{phase:?}: {boot:?}");
        let v = s.verify_recovery().unwrap();
        assert_eq!(v.k, 3, "{phase:?}");
    }
}

#[test]
fn shutdown_with_writes_deferred_by_a_full_arena_on_a_stalled_device() {
    let mut cfg = small(610);
    cfg.volumes = vec![(0, 4096)];
    let mut s = sim_with(cfg);
    s.boot().unwrap();
    s.disk().set_schedule(stalled());
    let mut lba = 0u64;
    while s.engine().stats().deferred == 0 {
        s.submit_write(0, lba, 8);
        lba += 8;
        s.tick().unwrap();
    }
    for _ in 0..4 {
        s.submit_write(0, lba, 8);
        lba += 8;
    }
    s.ticks(3).unwrap();
    s.collect();
    s.engine().shutdown();
    s.disk().set_schedule(Schedule::default());
    s.tick_until(|e| e.is_stopped(), "the final CP").unwrap();
    s.ticks(2).unwrap();
    let done = s.collect();
    let stopped = done.iter().filter(|x| x.status == Status::Stopped).count();
    assert!(stopped >= 1, "the deferred writes are refused: {done:?}");
    assert_eq!(s.inflight_writes(), 0, "every request was answered");
    s.shutdown_clean().unwrap();
    let boot = s.boot().unwrap();
    assert!(!boot.dirty);
    let v = s.verify_recovery().unwrap();
    assert_eq!(
        v.k,
        s.model.last_op(),
        "every acknowledged write is on disk"
    );
}

#[test]
fn precise_writes_waiting_for_an_intent_are_acknowledged_by_the_cp_that_carries_them() {
    // The review's hazard: at shutdown, writes still waiting for their
    // intent when the final CP landed were completed `Stopped` although
    // that CP made them durable. A CP now acknowledges what it carried.
    let mut cfg = precise(613);
    cfg.schedule = fifo1();
    let mut s = sim_with(cfg);
    s.boot().unwrap();
    s.disk().set_schedule(stalled());
    let id = s.submit_write(0, 0, 2);
    s.ticks(2).unwrap();
    assert!(s.collect().is_empty(), "waiting for its intent");
    // The intent fails for good: the device answers every intent write with
    // an error from now on, by never completing it; the shutdown's final CP
    // still carries the write.
    s.engine().shutdown();
    s.disk().set_schedule(Schedule {
        max_per_poll: 4,
        reorder: false,
        queue_depth: 64,
    });
    s.tick_until(|e| e.is_stopped(), "the final CP").unwrap();
    s.ticks(2).unwrap();
    let done = s.collect();
    let mine = done.iter().find(|x| x.req.id == id).expect("answered");
    assert_eq!(mine.status, Status::Ok, "{done:?}");
    s.shutdown_clean().unwrap();
    s.boot().unwrap();
    let v = s.verify_recovery().unwrap();
    assert_eq!(v.k, s.model.last_op());
    assert_eq!(s.read(0, 0, 2).unwrap().0, Status::Ok);
}

#[test]
fn the_cp_timer_survives_time_jumping_far_ahead() {
    let mut s = sim_with(small(611));
    s.boot().unwrap();
    for i in 0..3u64 {
        ok(&mut s, 0, i, 1);
        let before = s.engine().cp_seq();
        s.advance(1_000_000_000_000_000_000);
        s.tick_until(|e| e.cp_seq() > before, "the timer CP")
            .unwrap();
        s.settle().unwrap();
        s.ticks(10).unwrap();
        assert_eq!(
            s.engine().cp_seq(),
            before + 1,
            "one timer CP per jump, not one per tick"
        );
    }
    assert!(s.events.iter().any(|e| matches!(
        e,
        Event::CpCommitted {
            trigger: CpTrigger::Timer,
            ..
        }
    )));
    s.crash_daemon().unwrap();
    s.boot().unwrap();
    assert_eq!(s.verify_recovery().unwrap().k, 3);
}

#[test]
fn the_edges_of_a_volume_and_of_the_request_space() {
    let mut s = sim_with(small(612));
    s.boot().unwrap();
    let status =
        |s: &mut Sim, vol: u32, lba: u64, blocks: u32| s.write(vol, lba, blocks).unwrap().status;
    assert_eq!(status(&mut s, 0, 63, 1), Status::Ok, "the last block");
    assert_eq!(status(&mut s, 0, 63, 2), Status::OutOfRange);
    assert_eq!(status(&mut s, 0, 64, 1), Status::OutOfRange);
    assert_eq!(status(&mut s, 0, u64::MAX, 1), Status::OutOfRange);
    assert_eq!(status(&mut s, 5, 0, 1), Status::NoSuchVolume);
    assert_eq!(status(&mut s, u32::MAX, 0, 1), Status::NoSuchVolume);
    let read = |s: &mut Sim, vol: u32, lba: u64, blocks: u32| {
        let id = s.submit_read(vol, lba, blocks);
        s.wait(id, 50).unwrap().expect("the read completes").status
    };
    assert_eq!(read(&mut s, 0, 63, 1), Status::Ok);
    assert_eq!(read(&mut s, 0, 63, 2), Status::OutOfRange);
    assert_eq!(read(&mut s, 0, u64::MAX, 1), Status::OutOfRange);
    assert_eq!(read(&mut s, 0, 0, 0), Status::OutOfRange);
    assert_eq!(read(&mut s, 9, 0, 1), Status::NoSuchVolume);
    let mut buf = vec![0u8; 2 * BLOCK_SIZE];
    assert!(
        s.engine().peek(0, 63, &mut buf).is_err(),
        "peek past the end"
    );
    assert!(s.engine().peek(0, 62, &mut buf).is_ok());
    // Held volumes: every request answers NvFailed, peek still works.
    s.crash_power().unwrap();
    s.boot().unwrap();
    s.verify_recovery().unwrap();
    assert_eq!(read(&mut s, 0, 0, 1), Status::NvFailed);
    let flush = s.submit_flush(0);
    s.ticks(2).unwrap();
    let done = s.collect();
    assert!(done
        .iter()
        .any(|x| x.req.id == flush && x.status == Status::NvFailed));
    let mut one = vec![0u8; BLOCK_SIZE];
    assert_eq!(
        s.engine().peek(0, 63, &mut one).unwrap(),
        vec![PeekSource::Unmapped]
    );
    assert_eq!(
        s.engine().peek_disk(0, 63, &mut one).unwrap(),
        vec![PeekSource::Unmapped]
    );
}

// ---------------------------------------------------------------------------
// Geometry, volumes and write sizes
// ---------------------------------------------------------------------------

/// A seeded walk of writes of up to 64 blocks, CPs and crashes over a small
/// stripe geometry; the arena laps several times.
fn geometry_walk(
    stripe_kib: u32,
    stripes_per_segment: u32,
    segments: u32,
    vol_blocks: u64,
    seed: u64,
) -> Result<(), String> {
    let mut cfg = small(seed);
    cfg.stripe_bytes = stripe_kib * 1024;
    cfg.stripes_per_segment = stripes_per_segment;
    cfg.segments = segments;
    cfg.volumes = vec![(0, vol_blocks), (1, vol_blocks)];
    cfg.inflight_stripes = 2;
    let mut s = Sim::new(cfg)?;
    s.boot()?;
    let mut rng = SplitMix64::new(seed);
    for step in 0..40 {
        let what = rng.below(10);
        let r = match what {
            0..=5 => {
                let vol = rng.below(2) as u32;
                let blocks = rng.range(1, 64.min(vol_blocks)) as u32;
                let lba = rng.below(vol_blocks - u64::from(blocks) + 1);
                let held = s.engine().nvfail_set().get(vol);
                let c = s.write(vol, lba, blocks)?;
                let expected = if held { Status::NvFailed } else { Status::Ok };
                if c.status == expected {
                    Ok(())
                } else {
                    Err(format!("write status {:?}", c.status))
                }
            }
            6 => s.request_cp().map(|_| ()),
            7 => s
                .crash_power()
                .map(|_| ())
                .and_then(|()| s.boot().map(|_| ()))
                .and_then(|()| s.verify_recovery().map(|_| ()))
                .and_then(|()| s.clear_nvfail()),
            8 => s
                .crash_daemon()
                .and_then(|()| s.boot().map(|_| ()))
                .and_then(|()| s.verify_recovery().map(|_| ())),
            _ => s
                .shutdown_clean()
                .and_then(|()| s.boot().map(|_| ()))
                .and_then(|()| s.verify_recovery().map(|_| ())),
        };
        r.map_err(|e| format!("step {step} ({what}): {e}"))?;
        s.check_disk_prefix()
            .map_err(|e| format!("step {step}: disk prefix: {e}"))?;
    }
    s.shutdown_clean()?;
    s.boot()?;
    let v = s.verify_recovery()?;
    if v.k != s.model.last_op() {
        return Err(format!("final restart: prefix {}", v.k));
    }
    Ok(())
}

#[test]
fn small_stripes_large_writes_and_arena_laps_recover() {
    for (stripe_kib, sps, segs, vol_blocks, seed) in [
        (8, 4, 400, 128, 701),
        (16, 4, 300, 128, 702),
        (16, 2, 400, 96, 703),
        (64, 2, 100, 256, 704),
    ] {
        geometry_walk(stripe_kib, sps, segs, vol_blocks, seed)
            .unwrap_or_else(|e| panic!("{stripe_kib} KiB x {sps}: {e}"));
    }
}

#[test]
fn three_hundred_volumes_on_a_small_arena() {
    for intent in [NvfailIntent::Conservative, NvfailIntent::Precise] {
        let mut cfg = small(710);
        cfg.intent = intent;
        cfg.volumes = (0..300).map(|v| (v, 4)).collect();
        cfg.segments = 120;
        let mut s = sim_with(cfg);
        s.boot().unwrap();
        for vol in (0..300).step_by(3) {
            ok(&mut s, vol, u64::from(vol % 4), 1);
        }
        s.request_cp().unwrap();
        for vol in (0..300).step_by(7) {
            ok(&mut s, vol, 0, 1);
        }
        s.crash_power().unwrap();
        s.boot().unwrap();
        let v = s
            .verify_recovery()
            .unwrap_or_else(|e| panic!("{intent:?}: {e}"));
        assert!(!v.lost.is_empty());
        s.clear_nvfail().unwrap();
        for vol in (1..300).step_by(11) {
            ok(&mut s, vol, 3, 1);
        }
        s.crash_daemon().unwrap();
        s.boot().unwrap();
        s.verify_recovery().unwrap();
        s.shutdown_clean().unwrap();
        s.boot().unwrap();
        assert_eq!(s.verify_recovery().unwrap().k, s.model.last_op());
    }
}

// ---------------------------------------------------------------------------
// Random adversarial walks
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
enum Op {
    Write {
        vol: u8,
        lba: u16,
        blocks: u8,
    },
    Burst {
        n: u8,
        seed: u16,
    },
    InflightCrash {
        n: u8,
        ticks: u8,
        power: bool,
        seed: u16,
    },
    Read {
        vol: u8,
        lba: u16,
        blocks: u8,
    },
    Ticks(u8),
    Advance(u8),
    RequestCp,
    CrashPower {
        tear: u8,
    },
    CrashDaemon,
    CleanRestart,
    Punch {
        vol: u8,
        lba: u16,
        blocks: u8,
        zeroes: bool,
    },
    Snapshot {
        vol: u8,
    },
    Fold,
    CheckSnapshots,
    Device {
        max: u8,
        reorder: bool,
        depth: u8,
    },
}

fn op() -> impl Strategy<Value = Op> {
    prop_oneof![
        6 => (any::<u8>(), any::<u16>(), 1u8..=64).prop_map(|(vol, lba, blocks)| Op::Write { vol, lba, blocks }),
        2 => (2u8..=6, any::<u16>()).prop_map(|(n, seed)| Op::Burst { n, seed }),
        1 => (1u8..=6, 0u8..=4, any::<bool>(), any::<u16>())
            .prop_map(|(n, ticks, power, seed)| Op::InflightCrash { n, ticks, power, seed }),
        2 => (any::<u8>(), any::<u16>(), 1u8..=16).prop_map(|(vol, lba, blocks)| Op::Read { vol, lba, blocks }),
        2 => (1u8..=20).prop_map(Op::Ticks),
        1 => (0u8..=7).prop_map(Op::Advance),
        2 => Just(Op::RequestCp),
        2 => (any::<u8>(), any::<u16>(), 1u8..=32, any::<bool>())
            .prop_map(|(vol, lba, blocks, zeroes)| Op::Punch { vol, lba, blocks, zeroes }),
        1 => any::<u8>().prop_map(|vol| Op::Snapshot { vol }),
        1 => Just(Op::Fold),
        1 => Just(Op::CheckSnapshots),
        1 => (0u8..4).prop_map(|tear| Op::CrashPower { tear }),
        1 => Just(Op::CrashDaemon),
        1 => Just(Op::CleanRestart),
        1 => (1u8..=4, any::<bool>(), 1u8..=64).prop_map(|(max, reorder, depth)| Op::Device { max, reorder, depth }),
    ]
}

#[derive(Debug, Clone)]
struct Scenario {
    seed: u64,
    precise: bool,
    geometry: u8,
    volumes: u8,
    ops: Vec<Op>,
}

fn scenario() -> impl Strategy<Value = Scenario> {
    (
        any::<u64>(),
        any::<bool>(),
        0u8..4,
        2u8..=4,
        prop::collection::vec(op(), 5..32),
    )
        .prop_map(|(seed, precise, geometry, volumes, ops)| Scenario {
            seed,
            precise,
            geometry,
            volumes,
            ops,
        })
}

fn tear_of(n: u8) -> TearPolicy {
    match n {
        0 => DROP,
        1 => WHOLE,
        2 => TORN,
        _ => TearPolicy::default(),
    }
}

struct Walk {
    s: Sim,
    sizes: Vec<u64>,
    /// The committed CP the disk image was last checked at.
    checked: Option<(u64, u64)>,
}

impl Walk {
    fn held(&mut self, vol: u32) -> bool {
        self.s.engine().nvfail_set().get(vol)
    }

    fn after_crash(&mut self, power: bool) -> Result<(), String> {
        self.s.boot()?;
        self.s.verify_recovery()?;
        if power {
            self.s.clear_nvfail()?;
        }
        Ok(())
    }

    /// Every snapshot still reads its image, and the live diff against
    /// each is exactly what changed since.
    fn check_snapshots(&mut self) -> Result<(), String> {
        self.s.verify_snapshots()?;
        let ids: Vec<u64> = self.s.snaps.iter().map(|x| x.snap_id).collect();
        for id in ids {
            self.s.check_diff(id)?;
        }
        Ok(())
    }

    fn pick(&self, vol: u8, lba: u16, blocks: u8) -> (u32, u64, u32) {
        let vol = u32::from(vol) % self.sizes.len() as u32;
        let size = self.sizes[vol as usize];
        let blocks = u64::from(blocks).min(size) as u32;
        let lba = u64::from(lba) % (size - u64::from(blocks) + 1);
        (vol, lba, blocks)
    }

    fn step(&mut self, op: &Op) -> Result<(), String> {
        match *op {
            Op::Write { vol, lba, blocks } => {
                let (vol, lba, blocks) = self.pick(vol, lba, blocks);
                let held = self.held(vol);
                let done = self.s.write(vol, lba, blocks)?;
                let expected = if held { Status::NvFailed } else { Status::Ok };
                if done.status != expected {
                    return Err(format!("write {vol}/{lba}+{blocks}: {:?}", done.status));
                }
            }
            Op::Burst { n, seed } => {
                // Several writes in flight at once, in both nvfail modes
                // (the model knows every issued write).
                let mut rng = SplitMix64::new(u64::from(seed));
                let mut ids = Vec::new();
                for _ in 0..n {
                    let (vol, lba, blocks) = self.pick(
                        rng.below(8) as u8,
                        rng.below(u64::from(u16::MAX)) as u16,
                        rng.range(1, 16) as u8,
                    );
                    ids.push(self.s.submit_write(vol, lba, blocks));
                }
                for id in ids {
                    self.s
                        .wait(id, 20_000)?
                        .ok_or("burst write never completed")?;
                }
            }
            Op::InflightCrash {
                n,
                ticks,
                power,
                seed,
            } => {
                let mut rng = SplitMix64::new(u64::from(seed));
                for _ in 0..n {
                    let (vol, lba, blocks) = self.pick(
                        rng.below(8) as u8,
                        rng.below(u64::from(u16::MAX)) as u16,
                        rng.range(1, 32) as u8,
                    );
                    self.s.submit_write(vol, lba, blocks);
                }
                for _ in 0..ticks {
                    self.s.tick()?;
                }
                self.s.collect();
                if power {
                    self.s.crash_power()?;
                } else {
                    self.s.crash_daemon()?;
                }
                self.after_crash(power)?;
            }
            Op::Read { vol, lba, blocks } => {
                let (vol, lba, blocks) = self.pick(vol, lba, blocks);
                let held = self.held(vol);
                let (status, _) = self.s.read(vol, lba, blocks)?;
                let expected = if held { Status::NvFailed } else { Status::Ok };
                if status != expected {
                    return Err(format!("read {vol}/{lba}+{blocks}: {status:?}"));
                }
            }
            Op::Punch {
                vol,
                lba,
                blocks,
                zeroes,
            } => {
                let (vol, lba, blocks) = self.pick(vol, lba, blocks);
                let held = self.held(vol);
                let done = self.s.punch(vol, lba, blocks, zeroes)?;
                let expected = if held { Status::NvFailed } else { Status::Ok };
                if done.status != expected {
                    return Err(format!("punch {vol}/{lba}+{blocks}: {:?}", done.status));
                }
            }
            Op::Snapshot { vol } => {
                // A few per scenario: every snapshot pins data the pool
                // cannot reclaim, and these pools are small.
                let vol = u32::from(vol) % self.sizes.len() as u32;
                if !self.held(vol) && self.s.snaps.len() < 3 {
                    self.s.snapshot(vol)?;
                }
            }
            Op::Fold => {
                self.s.request_fold()?;
            }
            Op::CheckSnapshots => self.check_snapshots()?,
            Op::Ticks(n) => self.s.ticks(u32::from(n))?,
            Op::Advance(secs) => self.s.advance(u64::from(secs) * 1_000_000_000),
            Op::RequestCp => {
                self.s.request_cp()?;
            }
            Op::CrashPower { tear } => {
                self.s.disk().set_tear_policy(tear_of(tear));
                self.s.crash_power()?;
                self.after_crash(true)?;
            }
            Op::CrashDaemon => {
                self.s.crash_daemon()?;
                self.after_crash(false)?;
            }
            Op::CleanRestart => {
                self.s.shutdown_clean()?;
                self.s.boot()?;
                self.s.verify_recovery()?;
            }
            Op::Device {
                max,
                reorder,
                depth,
            } => self.s.disk().set_schedule(Schedule {
                max_per_poll: usize::from(max),
                reorder,
                queue_depth: usize::from(depth),
            }),
        }
        // The committed image changes only with the committed CP (a boot
        // resets the memo): check it whenever that moved.
        let cp = (self.s.engine().cp_seq(), self.s.engine().cp_nonce());
        if self.checked != Some(cp) {
            self.s.check_disk_prefix()?;
            self.checked = Some(cp);
        }
        Ok(())
    }
}

fn walk(sc: &Scenario) -> Result<(), String> {
    let (stripe_kib, sps, segments) = match sc.geometry {
        0 => (8, 8, 160),
        1 => (16, 4, 160),
        2 => (32, 8, 48),
        _ => (64, 2, 80),
    };
    let mut cfg = SimConfig::small(sc.seed);
    cfg.stripe_bytes = stripe_kib * 1024;
    cfg.stripes_per_segment = sps;
    cfg.segments = segments;
    cfg.intent = if sc.precise {
        NvfailIntent::Precise
    } else {
        NvfailIntent::Conservative
    };
    let sizes: Vec<u64> = (0..u64::from(sc.volumes)).map(|v| 32 + 32 * v).collect();
    cfg.volumes = sizes
        .iter()
        .enumerate()
        .map(|(v, s)| (v as u32, *s))
        .collect();
    cfg.inflight_stripes = 1 + (sc.seed % 6) as u16;
    let mut w = Walk {
        s: Sim::new(cfg)?,
        sizes,
        checked: None,
    };
    w.s.boot()?;
    w.s.verify_recovery()?;
    for (i, op) in sc.ops.iter().enumerate() {
        w.step(op).map_err(|e| format!("op {i} {op:?}: {e}"))?;
    }
    w.s.disk().set_schedule(Schedule::default());
    w.s.shutdown_clean()?;
    w.s.boot()?;
    let v = w.s.verify_recovery()?;
    if v.k != w.s.model.last_op() {
        return Err(format!(
            "final restart: prefix {}, last op {}",
            v.k,
            w.s.model.last_op()
        ));
    }
    w.check_snapshots()?;
    if w.s.last_stop() != Stop::Clean {
        return Err("the last stop was not clean".into());
    }
    Ok(())
}

proptest! {
    #![proptest_config(ProptestConfig {
        failure_persistence: None,
        max_shrink_iters: 1024,
        ..ProptestConfig::default()
    })]

    /// Random stripe geometries, volume counts, completion schedules, queue
    /// depths, tear policies, write sizes up to 64 blocks, punches, snapshots
    /// and folds, bursts of writes in flight together and crashes with
    /// requests in flight (in both nvfail modes), power cuts, daemon crashes
    /// and clean restarts, checked by the harness's model after every
    /// recovery, and the snapshot images and diffs on demand and at the end.
    #[test]
    fn random_geometries_schedules_bursts_and_crashes_recover(sc in scenario()) {
        if let Err(e) = walk(&sc) {
            prop_assert!(false, "{e}");
        }
    }
}

/// Found by this walk right after the Step 4 review's fixes: a discard that
/// completed while a CP was still staging its stripes was recorded as a
/// free to re-apply when the CP's table copy landed, but that CP copied the
/// table *after* the free, allocated the freed segment for its own
/// metadata, and the re-applied free then handed the segment, with the
/// snapshot's pinned page in it, to the next CP. A free is re-applied only
/// for a CP that copied the table before it (one past its metadata
/// preparation).
#[test]
fn a_free_that_lands_while_a_cp_is_staging_is_not_applied_twice() {
    let sc = Scenario {
        seed: 2542330785522,
        precise: false,
        geometry: 0,
        volumes: 2,
        ops: vec![
            Op::Write {
                vol: 0,
                lba: 0,
                blocks: 1,
            },
            Op::Write {
                vol: 0,
                lba: 0,
                blocks: 8,
            },
            Op::RequestCp,
            Op::Punch {
                vol: 0,
                lba: 0,
                blocks: 1,
                zeroes: false,
            },
            Op::Write {
                vol: 0,
                lba: 0,
                blocks: 32,
            },
            Op::InflightCrash {
                n: 2,
                ticks: 1,
                power: false,
                seed: 9438,
            },
            Op::Write {
                vol: 0,
                lba: 0,
                blocks: 32,
            },
            Op::Write {
                vol: 0,
                lba: 0,
                blocks: 32,
            },
            Op::Write {
                vol: 19,
                lba: 0,
                blocks: 38,
            },
            Op::Write {
                vol: 89,
                lba: 0,
                blocks: 33,
            },
            Op::Write {
                vol: 0,
                lba: 0,
                blocks: 32,
            },
            Op::RequestCp,
            Op::Write {
                vol: 0,
                lba: 0,
                blocks: 1,
            },
            Op::Snapshot { vol: 0 },
            Op::Write {
                vol: 0,
                lba: 0,
                blocks: 1,
            },
        ],
    };
    if let Err(e) = walk(&sc) {
        panic!("{e}");
    }
}
