//! The harness: one engine over the fakes, driven op by op, crashed and
//! rebooted, and checked against the model after every recovery.
//!
//! Writes can be issued one at a time ([`Sim::write`], which waits) or
//! several in flight ([`Sim::submit_write`], [`Sim::collect`],
//! [`Sim::wait`]); every issued write is in the model from the moment it is
//! submitted, so a crash with requests in flight is checked by the same
//! rules as any other (see the model's documentation).

use std::collections::HashMap;

use otap_arena::ArenaError;
use otap_cp::{
    Engine, EngineConfig, Event, Geometry, MediaProfile, NvfailIntent, Phase, ReqId, Status,
    VolumeSpec,
};
use otap_format::{Uuid, VolBitSet, BLOCK_SIZE, INTENT_SLOT_SIZE, SUPERBLOCK_SLOT_SIZE};
use otap_sys::{HugeArena, HugePageSize};

use crate::backstore::{CrashReport, Schedule, SimBackstore, TearPolicy};
use crate::clock::SimClock;
use crate::frontdoor::{Completion, SimFrontDoor};
use crate::model::Model;

/// Ticks the harness spends waiting for one thing before it gives up.
pub const TICK_BUDGET: u32 = 100_000;

/// What a simulation looks like.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SimConfig {
    /// The seed of every fake.
    pub seed: u64,
    /// The full stripe.
    pub stripe_bytes: u32,
    /// Stripes per segment.
    pub stripes_per_segment: u32,
    /// Segments on the device.
    pub segments: u32,
    /// Arena bytes (a multiple of 2 MiB).
    pub arena_bytes: usize,
    /// Volumes and their sizes in blocks.
    pub volumes: Vec<(u32, u64)>,
    /// The nvfail intent mode.
    pub intent: NvfailIntent,
    /// The CP timer.
    pub cp_timer_ns: u64,
    /// Stripe writes in flight.
    pub inflight_stripes: u16,
    /// A controller that acknowledges flushes it does not perform (the
    /// engine refuses it; see `Geometry::flush_opaque`).
    pub flush_opaque: bool,
    /// The device's completion schedule.
    pub schedule: Schedule,
    /// How a power cut tears unsafe writes.
    pub tear: TearPolicy,
}

impl SimConfig {
    /// A small pool: 32 KiB stripes, 8 per segment, 48 segments, a 2 MiB
    /// arena, two volumes of 64 blocks, conservative intents, a 5 s timer.
    #[must_use]
    pub fn small(seed: u64) -> Self {
        Self {
            seed,
            stripe_bytes: 32 * 1024,
            stripes_per_segment: 8,
            segments: 48,
            arena_bytes: 2 * 1024 * 1024,
            volumes: vec![(0, 64), (1, 64)],
            intent: NvfailIntent::Conservative,
            cp_timer_ns: 5_000_000_000,
            inflight_stripes: 4,
            flush_opaque: false,
            schedule: Schedule::default(),
            tear: TearPolicy::default(),
        }
    }

    /// The device geometry: the fixed regions plus the segments.
    #[must_use]
    pub fn geometry(&self) -> Geometry {
        let stripe = u64::from(self.stripe_bytes);
        let slot_stripes = (SUPERBLOCK_SLOT_SIZE as u64).div_ceil(stripe);
        let intent_stripes = (INTENT_SLOT_SIZE as u64).div_ceil(stripe);
        let front = (2 * slot_stripes + 2 * intent_stripes) * stripe;
        let back = 2 * slot_stripes * stripe;
        let segment_bytes = u64::from(self.stripes_per_segment) * stripe;
        Geometry {
            stripe_bytes: self.stripe_bytes,
            segment_bytes,
            capacity_bytes: front + back + u64::from(self.segments) * segment_bytes,
            flush_opaque: self.flush_opaque,
        }
    }

    /// The media profile.
    #[must_use]
    pub fn profile(&self) -> MediaProfile {
        let mut profile = MediaProfile::flash(self.geometry());
        profile.cp_timer_ns = self.cp_timer_ns;
        profile.inflight_stripes = self.inflight_stripes;
        profile.nvfail_intent = self.intent;
        profile
    }

    /// The engine configuration.
    #[must_use]
    pub fn engine_config(&self) -> EngineConfig {
        EngineConfig::new(
            Uuid::from_bytes(
                self.seed
                    .to_le_bytes()
                    .repeat(2)
                    .try_into()
                    .expect("16 bytes"),
            ),
            self.volumes
                .iter()
                .map(|(vol, size)| VolumeSpec {
                    vol: *vol,
                    size_blocks: *size,
                })
                .collect(),
        )
    }
}

/// The engine the harness drives.
pub type SimEngine = Engine<SimBackstore, SimFrontDoor, SimClock>;

/// How the previous engine instance ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Stop {
    /// Never ran (the first boot after format).
    Fresh,
    /// A power cut.
    Power,
    /// A `kill -9`, or a shutdown that could not write its final CP (the
    /// arena survives either way).
    Daemon,
    /// A clean shutdown.
    Clean,
}

/// What a boot reported.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BootInfo {
    /// The CP selected.
    pub cp_seq: u64,
    /// A dirty boot.
    pub dirty: bool,
    /// The arena re-attached.
    pub reattached: bool,
    /// A torn CP was skipped.
    pub rewound: bool,
    /// The arena memory held a damaged arena and was formatted anew.
    pub arena_lost: bool,
    /// Arena slots the re-attach dropped as damaged.
    pub damaged_slots: u32,
    /// Volumes held after the boot.
    pub nvfail: VolBitSet,
}

/// What a recovery check established.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Verdict {
    /// The prefix bound: the image is the state after the issued writes up
    /// to this operation.
    pub k: u64,
    /// Volumes that lost an acknowledged write.
    pub lost: VolBitSet,
    /// Volumes held by nvfail.
    pub nvfail: VolBitSet,
    /// The boot.
    pub boot: BootInfo,
}

/// The harness.
pub struct Sim {
    /// The configuration (volumes created at runtime are added to it, so a
    /// reboot knows them).
    pub cfg: SimConfig,
    clock: SimClock,
    disk: Option<SimBackstore>,
    front: Option<SimFrontDoor>,
    memory: Option<HugeArena>,
    engine: Option<SimEngine>,
    /// The checker model.
    pub model: Model,
    next_op: u64,
    /// Writes submitted and not yet answered: request to operation number.
    inflight: HashMap<ReqId, u64>,
    /// Completions collected and not yet claimed by [`Sim::wait`].
    completed: HashMap<ReqId, Completion>,
    gen_commits: u64,
    gen_commits_at_stop: u64,
    held_at_stop: VolBitSet,
    last_stop: Stop,
    last_boot: Option<BootInfo>,
    /// Every event the engine reported.
    pub events: Vec<Event>,
}

impl std::fmt::Debug for Sim {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Sim")
            .field("cfg", &self.cfg)
            .field("next_op", &self.next_op)
            .field("gen_commits", &self.gen_commits)
            .field("last_stop", &self.last_stop)
            .field("running", &self.engine.is_some())
            .finish_non_exhaustive()
    }
}

impl Sim {
    /// A formatted pool and an empty arena; nothing is booted yet.
    pub fn new(cfg: SimConfig) -> Result<Self, String> {
        let clock = SimClock::new(1_700_000_000_000_000_000);
        let mut disk = SimBackstore::new(cfg.profile(), cfg.seed).map_err(|e| e.to_string())?;
        disk.set_schedule(cfg.schedule);
        disk.set_tear_policy(cfg.tear);
        Engine::<SimBackstore, SimFrontDoor, SimClock>::format(
            &mut disk,
            &clock,
            &cfg.engine_config(),
        )
        .map_err(|e| e.to_string())?;
        let memory = HugeArena::anonymous(cfg.arena_bytes, HugePageSize::TwoMiB)
            .map_err(|e| e.to_string())?;
        let model = Model::new(&cfg.volumes);
        Ok(Self {
            cfg,
            clock,
            disk: Some(disk),
            front: Some(SimFrontDoor::new()),
            memory: Some(memory),
            engine: None,
            model,
            next_op: 0,
            inflight: HashMap::new(),
            completed: HashMap::new(),
            gen_commits: 0,
            gen_commits_at_stop: 0,
            held_at_stop: VolBitSet::EMPTY,
            last_stop: Stop::Fresh,
            last_boot: None,
            events: Vec::new(),
        })
    }

    /// The engine, which must be running.
    pub fn engine(&mut self) -> &mut SimEngine {
        self.engine.as_mut().expect("the engine is running")
    }

    /// `true` while an engine instance exists.
    #[must_use]
    pub fn is_running(&self) -> bool {
        self.engine.is_some()
    }

    /// The clock.
    #[must_use]
    pub fn clock(&self) -> &SimClock {
        &self.clock
    }

    /// The device, running or not.
    pub fn disk(&mut self) -> &mut SimBackstore {
        match &mut self.engine {
            Some(engine) => engine.backstore(),
            None => self.disk.as_mut().expect("the device exists"),
        }
    }

    /// The arena memory while no engine runs (for damage between two
    /// processes).
    pub fn memory(&mut self) -> Option<&mut HugeArena> {
        self.memory.as_mut()
    }

    /// Generation commits seen so far.
    #[must_use]
    pub const fn gen_commits(&self) -> u64 {
        self.gen_commits
    }

    /// The last boot.
    #[must_use]
    pub const fn last_boot(&self) -> Option<BootInfo> {
        self.last_boot
    }

    /// How the previous instance stopped.
    #[must_use]
    pub const fn last_stop(&self) -> Stop {
        self.last_stop
    }

    /// Writes submitted and not yet answered.
    #[must_use]
    pub fn inflight_writes(&self) -> usize {
        self.inflight.len()
    }

    /// Boots an engine over the device and the arena memory and drives it
    /// until it serves. A refused boot leaves the device and the arena as
    /// they were, so the simulation can go on.
    pub fn boot(&mut self) -> Result<BootInfo, String> {
        if self.engine.is_some() {
            return Err("an engine is already running".into());
        }
        let disk = self.disk.take().ok_or("no device")?;
        let front = self.front.take().ok_or("no front door")?;
        let memory = self.memory.take().ok_or("no arena memory")?;
        let engine = match Engine::boot(
            self.cfg.engine_config(),
            disk,
            front,
            self.clock.clone(),
            memory,
        ) {
            Ok(engine) => engine,
            Err(failure) => {
                let (error, disk, front, _clock, memory) = failure.into_parts();
                self.disk = Some(disk);
                self.front = Some(front);
                self.memory = Some(memory);
                return Err(format!("boot: {error}"));
            }
        };
        self.engine = Some(engine);
        let mut info = None;
        for event in self.pump() {
            if let Event::Booted {
                cp_seq,
                dirty,
                reattached,
                rewound,
                arena_lost,
                damaged_slots,
                nvfail,
                ..
            } = event
            {
                info = Some(BootInfo {
                    cp_seq,
                    dirty,
                    reattached,
                    rewound,
                    arena_lost,
                    damaged_slots,
                    nvfail,
                });
            }
        }
        let info = info.ok_or("no Booted event")?;
        self.last_boot = Some(info);
        self.tick_until(|e| e.is_serving(), "the boot commit")?;
        Ok(info)
    }

    fn pump(&mut self) -> Vec<Event> {
        let events = self.engine().take_events();
        for e in &events {
            if let Event::CpCommitted {
                gen: Some(_),
                max_seq,
                ..
            } = e
            {
                self.gen_commits += 1;
                self.model.mark_committed(*max_seq, self.gen_commits);
            }
        }
        self.events.extend(events.iter().cloned());
        events
    }

    /// One engine tick.
    pub fn tick(&mut self) -> Result<(), String> {
        self.engine().tick().map_err(|e| format!("tick: {e}"))?;
        self.pump();
        Ok(())
    }

    /// `n` ticks.
    pub fn ticks(&mut self, n: u32) -> Result<(), String> {
        for _ in 0..n {
            self.tick()?;
        }
        Ok(())
    }

    /// Ticks until `done(engine)` holds, within the budget.
    pub fn tick_until(
        &mut self,
        mut done: impl FnMut(&mut SimEngine) -> bool,
        what: &str,
    ) -> Result<(), String> {
        for _ in 0..TICK_BUDGET {
            if done(self.engine()) {
                return Ok(());
            }
            self.tick()?;
        }
        Err(format!("{what} did not happen within {TICK_BUDGET} ticks"))
    }

    /// Moves virtual time forward.
    pub fn advance(&self, ns: u64) {
        self.clock.advance(ns);
    }

    /// Queues a write and records it in the model as issued.
    pub fn submit_write(&mut self, vol: u32, lba: u64, blocks: u32) -> ReqId {
        self.next_op += 1;
        let op = self.next_op;
        self.model.issued(op, vol, lba, blocks);
        let data = Model::data_of(op, vol, lba, blocks);
        let id = self.engine().front_door().submit_write(vol, lba, data);
        self.inflight.insert(id, op);
        id
    }

    /// Queues a read.
    pub fn submit_read(&mut self, vol: u32, lba: u64, blocks: u32) -> ReqId {
        self.engine().front_door().submit_read(vol, lba, blocks)
    }

    /// Queues a flush.
    pub fn submit_flush(&mut self, vol: u32) -> ReqId {
        self.engine().front_door().submit_flush(vol)
    }

    /// Takes the engine's completions since the last call, records every
    /// write's outcome in the model, and keeps them for [`Sim::wait`].
    pub fn collect(&mut self) -> Vec<Completion> {
        let done = self.engine().front_door().take_completions();
        for c in &done {
            if let Some(op) = self.inflight.remove(&c.req.id) {
                if c.status == Status::Ok {
                    self.model.acked(op, c.seq);
                } else {
                    self.model.refused(op);
                }
            }
            self.completed.insert(c.req.id, c.clone());
        }
        done
    }

    /// Ticks until request `id` completes (or was already collected), at
    /// most `budget` ticks.
    pub fn wait(&mut self, id: ReqId, budget: u32) -> Result<Option<Completion>, String> {
        for _ in 0..=budget {
            self.collect();
            if let Some(c) = self.completed.remove(&id) {
                return Ok(Some(c));
            }
            self.tick()?;
        }
        Ok(None)
    }

    /// Issues a write and waits for its completion.
    pub fn write(&mut self, vol: u32, lba: u64, blocks: u32) -> Result<Completion, String> {
        let id = self.submit_write(vol, lba, blocks);
        self.wait(id, TICK_BUDGET)?
            .ok_or_else(|| format!("write {id:?} did not complete within {TICK_BUDGET} ticks"))
    }

    /// Issues a read, waits for it, and checks the data against the model
    /// when it succeeds and no write is in flight (with writes in flight the
    /// model cannot say which of them a read should see).
    pub fn read(&mut self, vol: u32, lba: u64, blocks: u32) -> Result<(Status, Vec<u8>), String> {
        let id = self.submit_read(vol, lba, blocks);
        let completion = self
            .wait(id, TICK_BUDGET)?
            .ok_or_else(|| format!("read {id:?} did not complete within {TICK_BUDGET} ticks"))?;
        if completion.status != Status::Ok {
            return Ok((completion.status, Vec::new()));
        }
        let data = self
            .engine()
            .front_door()
            .take_read(id)
            .ok_or("read completed without data")?;
        if self.inflight.is_empty() && self.model.unresolved() == 0 {
            for k in 0..blocks {
                let block = &data[(k as usize) * BLOCK_SIZE..(k as usize + 1) * BLOCK_SIZE];
                let expected = match self.model.expected(vol, lba + u64::from(k), u64::MAX) {
                    Some((op, pos)) => {
                        crate::model::block_data(op, vol, lba + u64::from(k) - u64::from(pos), pos)
                    }
                    None => vec![0u8; BLOCK_SIZE],
                };
                if block != expected.as_slice() {
                    return Err(format!(
                        "read of volume {vol} LBA {}: {:?}, expected {:?}",
                        lba + u64::from(k),
                        crate::model::decode(block),
                        crate::model::decode(&expected)
                    ));
                }
            }
        }
        Ok((Status::Ok, data))
    }

    /// Asks for a consistency point and waits until the committed CP moves.
    pub fn request_cp(&mut self) -> Result<u64, String> {
        let before = self.engine().cp_seq();
        self.engine().request_cp();
        self.tick_until(|e| e.cp_seq() > before, "the requested CP")?;
        Ok(self.engine().cp_seq())
    }

    /// Ticks until no CP is in progress and no generation is frozen.
    pub fn settle(&mut self) -> Result<(), String> {
        self.tick_until(
            |e| e.phase() == Phase::Idle && e.frozen_generations() == 0,
            "the CP pipeline to drain",
        )
    }

    /// Creates a volume and waits until its creation is durable; the model
    /// and the configuration learn it, so a reboot knows it too.
    pub fn create_volume(&mut self, vol: u32, size: u64) -> Result<(), String> {
        self.engine()
            .create_volume(vol, size)
            .map_err(|e| format!("create volume {vol}: {e}"))?;
        let seen = self.events.len();
        for _ in 0..TICK_BUDGET {
            let created = self.events[seen..]
                .iter()
                .any(|e| matches!(e, Event::VolumeCreated { vol: v, .. } if *v == vol));
            if created {
                self.model.add_volume(vol, size);
                self.cfg.volumes.push((vol, size));
                return Ok(());
            }
            self.tick()?;
        }
        Err(format!(
            "volume {vol} was not created within {TICK_BUDGET} ticks"
        ))
    }

    fn record_stop(&mut self, stop: Stop) {
        self.held_at_stop = self.engine().nvfail_set();
        self.gen_commits_at_stop = self.gen_commits;
        self.last_stop = stop;
        // Requests in flight die with the process; their writes stay in the
        // model as issued until a recovery check settles them.
        self.inflight.clear();
        self.completed.clear();
    }

    /// A power cut: the device tears what was unsafe, the arena memory is
    /// lost, the client forgets its requests.
    pub fn crash_power(&mut self) -> Result<CrashReport, String> {
        self.record_stop(Stop::Power);
        let engine = self.engine.take().ok_or("no engine running")?;
        let (mut disk, mut front, _clock, _memory) = engine.into_parts();
        let report = disk.crash_power();
        front.reset();
        self.disk = Some(disk);
        self.front = Some(front);
        self.memory = Some(
            HugeArena::anonymous(self.cfg.arena_bytes, HugePageSize::TwoMiB)
                .map_err(|e| e.to_string())?,
        );
        Ok(report)
    }

    /// A `kill -9`: the device finishes what it had, the arena memory
    /// survives, the client forgets its requests.
    pub fn crash_daemon(&mut self) -> Result<(), String> {
        self.record_stop(Stop::Daemon);
        let engine = self.engine.take().ok_or("no engine running")?;
        let (mut disk, mut front, _clock, memory) = engine.into_parts();
        disk.crash_daemon();
        front.reset();
        self.disk = Some(disk);
        self.front = Some(front);
        self.memory = Some(memory);
        Ok(())
    }

    /// A clean shutdown: the final CP lands, then the process exits with
    /// the arena memory kept. When the pool had no room for the final CP
    /// the engine stops without the clean marker, which the harness records
    /// as a daemon stop (the arena keeps the unflushed writes).
    pub fn shutdown_clean(&mut self) -> Result<(), String> {
        self.engine().shutdown();
        let seen = self.events.len();
        self.tick_until(|e| e.is_stopped(), "the final CP")?;
        let clean = self.events[seen..]
            .iter()
            .any(|e| matches!(e, Event::Stopped { clean: true, .. }));
        self.record_stop(if clean { Stop::Clean } else { Stop::Daemon });
        let engine = self.engine.take().ok_or("no engine running")?;
        let (disk, mut front, _clock, memory) = engine.into_parts();
        front.reset();
        self.disk = Some(disk);
        self.front = Some(front);
        self.memory = Some(memory);
        Ok(())
    }

    /// Reads every block of every volume through the engine, regardless of
    /// nvfail.
    pub fn image(&mut self) -> Result<Vec<(u32, u64, Vec<u8>)>, String> {
        let mut out = Vec::new();
        for (vol, size) in self.model.volumes().collect::<Vec<_>>() {
            for lba in 0..size {
                let mut buf = vec![0u8; BLOCK_SIZE];
                self.engine()
                    .peek(vol, lba, &mut buf)
                    .map_err(|e| format!("peek {vol}/{lba}: {e}"))?;
                out.push((vol, lba, buf));
            }
        }
        Ok(out)
    }

    /// Checks the committed on-disk image (the map only) against the model:
    /// it must be a prefix of the issued writes at any moment.
    pub fn check_disk_prefix(&mut self) -> Result<u64, String> {
        let volumes: Vec<(u32, u64)> = self.model.volumes().collect();
        let mut blocks = HashMap::new();
        for (vol, size) in volumes {
            for lba in 0..size {
                let mut buf = vec![0u8; BLOCK_SIZE];
                self.engine()
                    .peek_disk(vol, lba, &mut buf)
                    .map_err(|e| format!("peek_disk {vol}/{lba}: {e}"))?;
                blocks.insert((vol, lba), buf);
            }
        }
        self.model.check_prefix(|vol, lba| {
            blocks
                .get(&(vol, lba))
                .cloned()
                .ok_or_else(|| format!("no block {vol}/{lba}"))
        })
    }

    /// After a boot: checks the image and the nvfail set against the model
    /// and the way the previous instance stopped, then settles the model
    /// (writes the crash lost are forgotten, writes it kept are part of the
    /// image).
    pub fn verify_recovery(&mut self) -> Result<Verdict, String> {
        let boot = self.last_boot.ok_or("no boot to verify")?;
        let image = self.image()?;
        let mut blocks = HashMap::new();
        for (vol, lba, data) in image {
            blocks.insert((vol, lba), data);
        }
        let k = self.model.check_prefix(|vol, lba| {
            blocks
                .get(&(vol, lba))
                .cloned()
                .ok_or_else(|| format!("no block {vol}/{lba}"))
        })?;
        let lost = self.model.lost_volumes(k);
        let nvfail = self.engine().nvfail_set();
        let held = self.held_at_stop;
        match self.last_stop {
            Stop::Fresh => {
                if k != 0 || !nvfail.is_empty() {
                    return Err(format!("fresh boot: k {k}, nvfail {nvfail:?}"));
                }
            }
            Stop::Daemon | Stop::Clean => {
                if !lost.is_empty() {
                    return Err(format!(
                        "{:?} restart lost acknowledged writes on {lost:?}: prefix {k}",
                        self.last_stop
                    ));
                }
                if nvfail != held {
                    return Err(format!(
                        "{:?} restart changed nvfail from {held:?} to {nvfail:?}",
                        self.last_stop
                    ));
                }
                if self.last_stop == Stop::Clean && boot.dirty {
                    return Err("a clean shutdown booted dirty".into());
                }
            }
            Stop::Power => {
                if !lost.is_subset(&nvfail) {
                    return Err(format!(
                        "power loss lost writes on {lost:?} but nvfail holds {nvfail:?}"
                    ));
                }
                if !held.is_subset(&nvfail) {
                    return Err(format!(
                        "power loss released held volumes: {held:?} -> {nvfail:?}"
                    ));
                }
                if boot.dirty {
                    match self.cfg.intent {
                        NvfailIntent::Conservative => {
                            let all = self.model.all_volumes();
                            if nvfail != all {
                                return Err(format!(
                                    "conservative dirty boot holds {nvfail:?}, expected every volume {all:?}"
                                ));
                            }
                        }
                        NvfailIntent::Precise => {
                            let recent = self
                                .model
                                .recently_active(self.gen_commits_at_stop)
                                .union(&held);
                            if !nvfail.is_subset(&recent) {
                                return Err(format!(
                                    "precise dirty boot holds {nvfail:?} beyond the recently active {recent:?}"
                                ));
                            }
                        }
                    }
                } else if nvfail != held {
                    return Err(format!(
                        "clean boot changed nvfail from {held:?} to {nvfail:?}"
                    ));
                }
            }
        }
        self.model.settle(k);
        Ok(Verdict {
            k,
            lost,
            nvfail,
            boot,
        })
    }

    /// Clears nvfail on every held volume and waits until the clears are
    /// durable and the volumes serve again.
    pub fn clear_nvfail(&mut self) -> Result<(), String> {
        let held = self.engine().nvfail_set();
        for vol in held.iter() {
            self.engine()
                .nvfail_clear(vol)
                .map_err(|e| format!("clear {vol}: {e}"))?;
        }
        if !held.is_empty() {
            self.tick_until(
                |e| held.iter().all(|vol| !e.nvfail_set().get(vol)),
                "the clears to become durable",
            )?;
        }
        Ok(())
    }

    /// Arena slots in the configured memory (what a full arena means).
    pub fn arena_slots(&self) -> Result<u32, ArenaError> {
        otap_arena::slots_for_len(self.cfg.arena_bytes)
    }
}
