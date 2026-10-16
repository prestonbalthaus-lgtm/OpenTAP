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
    CleanerPolicy, DiscardPolicy, Engine, EngineConfig, Event, FlushPolicy, FoldPolicy, Geometry,
    MediaProfile, NvfailIntent, Phase, ReqId, Status, TableFoldPolicy, VerifyOptions, VerifyReport,
    VolumeSpec,
};
use otap_format::{
    SnapKind, SnapOrigin, Uuid, VolBitSet, BLOCK_SIZE, INTENT_SLOT_SIZE, SUPERBLOCK_SLOT_SIZE,
};
use otap_sys::{HugeArena, HugePageSize};

use crate::backstore::{CrashReport, Schedule, SimBackstore, TearPolicy};
use crate::clock::SimClock;
use crate::frontdoor::{Completion, SimFrontDoor};
use crate::model::{expected_data, Expected, Model};

/// Ticks the harness spends waiting for one thing before it gives up.
pub const TICK_BUDGET: u32 = 100_000;

/// What a simulation looks like.
#[derive(Debug, Clone, PartialEq)]
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
    /// A controller that acknowledges flushes it does not perform (see
    /// `Geometry::flush_opaque`); what the engine does with it is
    /// `flush_policy` (E19 / E21).
    pub flush_opaque: bool,
    /// The flush policy.
    pub flush_policy: FlushPolicy,
    /// On a flush-opaque device: the controller cache survives a power cut.
    pub cache_protected: bool,
    /// The discard policy.
    pub discard: DiscardPolicy,
    /// When the map folds.
    pub fold: FoldPolicy,
    /// When the snapshot table and the catalogue fold (format 2.4).
    pub table_fold: TableFoldPolicy,
    /// The device's completion schedule.
    pub schedule: Schedule,
    /// How a power cut tears unsafe writes.
    pub tear: TearPolicy,
    /// Free segments front-door writes leave alone; the engine's rule
    /// (`reserve_for`) when `None`.
    pub reserve_segments: Option<u32>,
    /// When the cleaner runs.
    pub cleaner: CleanerPolicy,
    /// The pacing delay at the stall occupancy; 0 (the default) because the
    /// harness drives virtual time by hand.
    pub pacing_delay_ns: u64,
}

impl SimConfig {
    /// A small pool: 32 KiB stripes, 8 per segment, 48 segments, a 2 MiB
    /// arena, two volumes of 64 blocks, conservative intents, a 5 s timer,
    /// whole-segment discards, and a fold policy that folds every few CPs
    /// so the map's paths run often.
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
            flush_policy: FlushPolicy::TrustBarrierWithNvfail,
            cache_protected: true,
            discard: DiscardPolicy::WholeSegment,
            fold: FoldPolicy {
                log_bytes: 64 * 1024,
                interval_ns: 60_000_000_000,
                max_cps: 5,
                max_dirty_pages: 8,
                max_overlay_entries: 1024,
            },
            table_fold: TableFoldPolicy::default(),
            schedule: Schedule::default(),
            tear: TearPolicy::default(),
            reserve_segments: None,
            cleaner: CleanerPolicy::default(),
            pacing_delay_ns: 0,
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
        profile.flush_policy = self.flush_policy;
        profile.discard = self.discard;
        profile
    }

    /// The engine configuration.
    #[must_use]
    pub fn engine_config(&self) -> EngineConfig {
        let mut cfg = EngineConfig::new(
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
        );
        cfg.fold = self.fold;
        cfg.table_fold = self.table_fold;
        cfg.cache_pages = 16;
        // The harness drives virtual time by hand: a delayed
        // acknowledgement would wait for an advance nobody makes. The
        // stall at the high watermark stays; the delay curve is tested on
        // its own with explicit advances.
        cfg.pacing.max_delay_ns = self.pacing_delay_ns;
        cfg.reserve_segments = self.reserve_segments;
        cfg.cleaner = self.cleaner;
        cfg
    }
}

/// A snapshot the harness took, with the image the model gave it.
#[derive(Debug, Clone)]
pub struct SnapExpect {
    /// The engine's id.
    pub snap_id: u64,
    /// The volume.
    pub vol: u32,
    /// The identity.
    pub uuid: Uuid,
    /// The CP it pins.
    pub cp_seq: u64,
    /// What every block held when it was taken.
    pub image: Vec<Expected>,
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
#[derive(Debug, Clone, PartialEq, Eq)]
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
    /// Why each skipped candidate was torn.
    pub torn: Vec<(u64, String)>,
}

/// What a recovery check established.
#[derive(Debug, Clone, PartialEq, Eq)]
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
    /// Snapshots taken and their expected images.
    pub snaps: Vec<SnapExpect>,
    /// Snapshots whose deletion was asked for: allowed to be gone.
    deleting: std::collections::HashSet<u64>,
    /// Rollbacks issued and not yet reported durable: the model's
    /// operation number to `(vol, snap_id)`.
    rollbacks: HashMap<u64, (u32, u64)>,
    next_uuid: u64,
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
        disk.set_cache_protected(cfg.cache_protected);
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
            snaps: Vec::new(),
            deleting: std::collections::HashSet::new(),
            rollbacks: HashMap::new(),
            next_uuid: 1,
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
    pub fn last_boot(&self) -> Option<BootInfo> {
        self.last_boot.clone()
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
        let mut disk = self.disk.take().ok_or("no device")?;
        let mut front = self.front.take().ok_or("no front door")?;
        let mut memory = self.memory.take().ok_or("no arena memory")?;
        // The configuration is the harness's view of the volumes' sizes;
        // the disk's is the truth after a crash around a rollback or a
        // resize, and a control plane would take it: retry with the size
        // the boot reports.
        let engine = loop {
            match Engine::boot(
                self.cfg.engine_config(),
                disk,
                front,
                self.clock.clone(),
                memory,
            ) {
                Ok(engine) => break engine,
                Err(failure) => {
                    let (error, d, f, _clock, m) = failure.into_parts();
                    disk = d;
                    front = f;
                    memory = m;
                    if let otap_cp::EngineError::VolumeSizeMismatch { vol, on_disk, .. } = error {
                        self.model.set_size(vol, on_disk);
                        let mut found = false;
                        for (v, s) in &mut self.cfg.volumes {
                            if *v == vol {
                                *s = on_disk;
                                found = true;
                            }
                        }
                        if found {
                            continue;
                        }
                    }
                    self.disk = Some(disk);
                    self.front = Some(front);
                    self.memory = Some(memory);
                    return Err(format!("boot: {error}"));
                }
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
                torn,
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
                    torn,
                });
            }
        }
        let info = info.ok_or("no Booted event")?;
        self.last_boot = Some(info.clone());
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
        let from = self.events.len();
        self.events.extend(events.iter().cloned());
        self.confirm_rollbacks(from);
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

    /// Ticks until `done(engine)` holds, within the budget. A timeout
    /// reports the pool's state and the engine's last complaints.
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
        let status = self.engine().pool_status();
        let phase = self.engine().phase();
        let frozen = self.engine().frozen_generations();
        let complaints: Vec<String> = self
            .events
            .iter()
            .rev()
            .filter_map(|e| match e {
                Event::CpFailed { cp_seq, reason } => Some(format!("CP {cp_seq} failed: {reason}")),
                Event::Internal { what } => Some(format!("internal: {what}")),
                Event::CleanSkipped { pseg, reason } => Some(format!("skip {pseg}: {reason}")),
                _ => None,
            })
            .take(6)
            .collect();
        Err(format!(
            "{what} did not happen within {TICK_BUDGET} ticks (phase {phase:?}, {frozen} frozen, \
             {status:?}; last complaints {complaints:?})"
        ))
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

    /// Queues a discard (or a write of zeros) and records it in the model
    /// as issued.
    pub fn submit_punch(&mut self, vol: u32, lba: u64, blocks: u32, zeroes: bool) -> ReqId {
        self.next_op += 1;
        let op = self.next_op;
        self.model.issued_punch(op, vol, lba, blocks);
        let front = self.engine().front_door();
        let id = if zeroes {
            front.submit_write_zeroes(vol, lba, blocks)
        } else {
            front.submit_discard(vol, lba, blocks)
        };
        self.inflight.insert(id, op);
        id
    }

    /// Issues a discard (or a write of zeros) and waits for its completion.
    pub fn punch(
        &mut self,
        vol: u32,
        lba: u64,
        blocks: u32,
        zeroes: bool,
    ) -> Result<Completion, String> {
        let id = self.submit_punch(vol, lba, blocks, zeroes);
        self.wait(id, TICK_BUDGET)?
            .ok_or_else(|| format!("punch {id:?} did not complete within {TICK_BUDGET} ticks"))
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
                let at = lba + u64::from(k);
                let expected = expected_data(vol, at, self.model.expected(vol, at, u64::MAX));
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

    /// Asks for a consistency point and waits until the committed CP moves
    /// and the engine is quiet again (a CP already in flight when the
    /// request was made is not the one that took it).
    pub fn request_cp(&mut self) -> Result<u64, String> {
        let before = self.engine().cp_seq();
        self.engine().request_cp();
        self.tick_until(|e| e.cp_seq() > before && e.is_quiet(), "the requested CP")?;
        Ok(self.engine().cp_seq())
    }

    /// Asks for a fold and waits until the CP that folds commits and the
    /// engine is quiet again.
    pub fn request_fold(&mut self) -> Result<u64, String> {
        let before = self.engine().map().fold_cp();
        self.engine().request_fold();
        self.tick_until(
            |e| e.map().fold_cp() > before && e.is_quiet(),
            "the requested fold",
        )?;
        Ok(self.engine().cp_seq())
    }

    /// Takes a snapshot of `vol` (nothing may be in flight), waits until it
    /// is durable, and records what the model says it holds. Returns its
    /// id.
    pub fn snapshot(&mut self, vol: u32) -> Result<u64, String> {
        if self.unsettled() {
            return Err("a snapshot needs nothing in flight".into());
        }
        let uuid = Uuid::from_bytes({
            let mut b = [0u8; 16];
            b[..8].copy_from_slice(&self.next_uuid.to_le_bytes());
            b[8..].copy_from_slice(&self.cfg.seed.to_le_bytes());
            b
        });
        self.next_uuid += 1;
        self.engine()
            .create_snapshot(vol, uuid, SnapKind::Crash, SnapOrigin::User)
            .map_err(|e| format!("create snapshot of {vol}: {e}"))?;
        let seen = self.events.len();
        for _ in 0..TICK_BUDGET {
            let found = self.events[seen..].iter().find_map(|e| match e {
                Event::SnapshotCreated {
                    snap_id,
                    vol: v,
                    cp_seq,
                } if *v == vol => Some((*snap_id, *cp_seq)),
                _ => None,
            });
            if let Some((snap_id, cp_seq)) = found {
                let image = self.model.image_of(vol);
                self.snaps.push(SnapExpect {
                    snap_id,
                    vol,
                    uuid,
                    cp_seq,
                    image,
                });
                return Ok(snap_id);
            }
            self.tick()?;
        }
        Err(format!(
            "the snapshot of {vol} was not created within {TICK_BUDGET} ticks"
        ))
    }

    /// Reads every snapshot the harness took through the engine and checks
    /// it against the image the model gave it. A snapshot the engine no
    /// longer lists is allowed only after a boot that rewound below its CP;
    /// the harness forgets it then.
    pub fn verify_snapshots(&mut self) -> Result<(), String> {
        let listed: Vec<otap_cp::SnapshotInfo> = self.engine().snapshots();
        let boot = self.last_boot.clone();
        let mut kept = Vec::new();
        for snap in std::mem::take(&mut self.snaps) {
            match listed.iter().find(|s| s.snap_id == snap.snap_id) {
                None => {
                    let rewound_past = boot
                        .as_ref()
                        .is_some_and(|b| b.rewound && b.cp_seq < snap.cp_seq);
                    if self.deleting.remove(&snap.snap_id) {
                        // Its deletion was asked for and landed.
                        continue;
                    }
                    if !rewound_past {
                        return Err(format!(
                            "snapshot {} (CP {}) vanished without a rewind below it",
                            snap.snap_id, snap.cp_seq
                        ));
                    }
                    continue;
                }
                Some(info) => {
                    if info.uuid != snap.uuid || info.vol != snap.vol || info.cp_seq != snap.cp_seq
                    {
                        return Err(format!(
                            "snapshot {} changed identity: {info:?}",
                            snap.snap_id
                        ));
                    }
                }
            }
            let mut buf = vec![0u8; BLOCK_SIZE];
            for (lba, expected) in snap.image.iter().enumerate() {
                let lba = lba as u64;
                self.engine()
                    .snapshot_read(snap.snap_id, lba, &mut buf)
                    .map_err(|e| format!("snapshot {} LBA {lba}: {e}", snap.snap_id))?;
                let want = expected_data(snap.vol, lba, *expected);
                if buf != want {
                    return Err(format!(
                        "snapshot {} of volume {} LBA {lba}: {:?}, expected {:?}",
                        snap.snap_id,
                        snap.vol,
                        crate::model::decode(&buf),
                        crate::model::decode(&want)
                    ));
                }
            }
            kept.push(snap);
        }
        self.snaps = kept;
        Ok(())
    }

    /// Checks the engine's `diff` of `vol` against snapshot `snap_id`: the
    /// blocks born after the snapshot are exactly those whose committed
    /// content differs from the snapshot's, holes for punches.
    pub fn check_diff(&mut self, snap_id: u64) -> Result<usize, String> {
        let snap = self
            .snaps
            .iter()
            .find(|s| s.snap_id == snap_id)
            .cloned()
            .ok_or_else(|| format!("no snapshot {snap_id} in the harness"))?;
        let vol = snap.vol;
        let diff = self
            .engine()
            .live_diff(vol, Some(snap_id))
            .map_err(|e| format!("diff of {vol} since {snap_id}: {e}"))?;
        let mut expected = Vec::new();
        let mut buf = vec![0u8; BLOCK_SIZE];
        // Over the volume as it is now: a block beyond the snapshot's end
        // (the volume grew since) differs when anything was written there.
        let size = self
            .model
            .volumes()
            .find(|(v, _)| *v == vol)
            .map_or(snap.image.len() as u64, |(_, s)| s);
        for lba in 0..size {
            let at_snap = snap.image.get(lba as usize).copied().flatten();
            let at_snap = &at_snap;
            self.engine()
                .peek_disk(vol, lba, &mut buf)
                .map_err(|e| format!("peek_disk {vol}/{lba}: {e}"))?;
            let now = crate::model::decode(&buf);
            let differs = match (now, at_snap) {
                (crate::model::Decoded::Zero, None | Some((_, _, true))) => {
                    // Zeros now, and nothing or a punch then: changed only if
                    // a later punch covered the block (over data, a hole, or
                    // nothing at all), which the engine records with a newer
                    // birth. The committed map tells.
                    self.engine().map_entry(vol, lba).is_some_and(|e| {
                        e.is_hole() && self.engine_hole_is_newer(vol, lba, snap.cp_seq)
                    })
                }
                (crate::model::Decoded::Write { op, .. }, Some((e_op, _, false))) => op != *e_op,
                _ => true,
            };
            if differs {
                expected.push(lba);
            }
        }
        let got: Vec<u64> = diff.iter().map(|(lba, _)| *lba).collect();
        if got != expected {
            return Err(format!(
                "diff of {vol} since snapshot {snap_id}: {got:?}, expected {expected:?}"
            ));
        }
        for (lba, entry) in &diff {
            let now = {
                self.engine()
                    .peek_disk(vol, *lba, &mut buf)
                    .map_err(|e| e.to_string())?;
                crate::model::decode(&buf)
            };
            let hole_ok = entry.is_hole() && now == crate::model::Decoded::Zero;
            let data_ok = entry.is_data() && matches!(now, crate::model::Decoded::Write { .. });
            if !hole_ok && !data_ok {
                return Err(format!(
                    "diff of {vol} LBA {lba}: entry {entry:?} does not match the image {now:?}"
                ));
            }
        }
        Ok(diff.len())
    }

    /// `true` when the committed hole at `lba` was punched after `cp_seq`.
    fn engine_hole_is_newer(&mut self, vol: u32, lba: u64, cp_seq: u64) -> bool {
        self.engine()
            .map_entry(vol, lba)
            .is_some_and(|e| u64::from(e.birth_cp) > cp_seq)
    }

    /// Ticks until no CP is in progress and no generation is frozen.
    pub fn settle(&mut self) -> Result<(), String> {
        self.tick_until(
            |e| e.phase() == Phase::Idle && e.frozen_generations() == 0,
            "the CP pipeline to drain",
        )
    }

    /// Asks for snapshot `snap_id` to be deleted without waiting: the
    /// harness allows it to be gone from now on (a crash may land the
    /// deletion without the event being seen) and keeps checking it while
    /// the engine still lists it.
    pub fn request_snapshot_deletion(&mut self, snap_id: u64) -> Result<(), String> {
        self.engine()
            .delete_snapshot(snap_id)
            .map_err(|e| format!("delete snapshot {snap_id}: {e}"))?;
        self.deleting.insert(snap_id);
        Ok(())
    }

    /// Asks for snapshot `snap_id` to be deleted and waits until it is
    /// gone; returns the blocks the engine released. The harness forgets
    /// the snapshot's image.
    pub fn delete_snapshot(&mut self, snap_id: u64) -> Result<u64, String> {
        self.request_snapshot_deletion(snap_id)?;
        let seen = self.events.len();
        for _ in 0..TICK_BUDGET {
            let found = self.events[seen..].iter().find_map(|e| match e {
                Event::SnapshotDeleted {
                    snap_id: id,
                    released,
                    ..
                } if *id == snap_id => Some(*released),
                _ => None,
            });
            if let Some(released) = found {
                self.snaps.retain(|s| s.snap_id != snap_id);
                self.deleting.remove(&snap_id);
                return Ok(released);
            }
            self.tick()?;
        }
        Err(format!(
            "snapshot {snap_id} was not deleted within {TICK_BUDGET} ticks"
        ))
    }

    /// Asks for `vol` to be rolled back to snapshot `snap_id` without
    /// waiting (the volume is quiesced first and left offline); returns the
    /// model's operation number for the rollback, which a recovery check
    /// settles like a write's.
    pub fn request_rollback(&mut self, vol: u32, snap_id: u64) -> Result<u64, String> {
        let image = self
            .snaps
            .iter()
            .find(|s| s.snap_id == snap_id && s.vol == vol)
            .map(|s| s.image.clone())
            .ok_or_else(|| format!("no snapshot {snap_id} of volume {vol} in the harness"))?;
        self.quiesce(vol)?;
        self.engine()
            .rollback_volume(vol, snap_id)
            .map_err(|e| format!("rollback volume {vol} to {snap_id}: {e}"))?;
        self.next_op += 1;
        let op = self.next_op;
        self.model.rollback(op, vol, image);
        self.rollbacks.insert(op, (vol, snap_id));
        Ok(op)
    }

    /// Marks the rollbacks whose events arrived as confirmed in the model,
    /// and takes the volume's restored size.
    fn confirm_rollbacks(&mut self, from: usize) {
        let mut resized = Vec::new();
        for e in &self.events[from..] {
            if let Event::VolumeRolledBack { vol, snap_id, .. } = e {
                let done: Vec<u64> = self
                    .rollbacks
                    .iter()
                    .filter(|(_, (v, s))| v == vol && s == snap_id)
                    .map(|(op, _)| *op)
                    .collect();
                for op in done {
                    self.model.confirm(op);
                    self.rollbacks.remove(&op);
                }
                resized.push(*vol);
            }
        }
        for vol in resized {
            self.sync_size(vol);
        }
    }

    /// Takes the engine's size of `vol` into the model and the
    /// configuration.
    fn sync_size(&mut self, vol: u32) {
        let Some(size) = self
            .engine
            .as_ref()
            .and_then(|e| e.volume(vol))
            .filter(|v| v.present)
            .map(|v| v.size_blocks)
        else {
            return;
        };
        self.model.set_size(vol, size);
        for (v, s) in &mut self.cfg.volumes {
            if *v == vol {
                *s = size;
            }
        }
    }

    /// Takes every present volume's size from the engine (after a boot:
    /// a rollback or a resize may or may not have landed).
    fn sync_sizes(&mut self) {
        let vols: Vec<u32> = self.model.volumes().map(|(v, _)| v).collect();
        for vol in vols {
            self.sync_size(vol);
        }
    }

    /// Takes `vol` offline (or back online). Offline, its requests answer
    /// `Offline`, which the model takes as refused.
    pub fn set_online(&mut self, vol: u32, online: bool) -> Result<(), String> {
        self.engine()
            .set_volume_online(vol, online)
            .map_err(|e| format!("set volume {vol} online={online}: {e}"))
    }

    /// Takes `vol` offline and flushes everything it has in the arena, so
    /// a rollback, a shrink or a deletion can be asked for.
    pub fn quiesce(&mut self, vol: u32) -> Result<(), String> {
        if self.unsettled() {
            return Err("quiescing needs nothing in flight".into());
        }
        self.set_online(vol, false)?;
        self.engine().request_cp();
        self.tick_until(
            |e| e.pool_status().unflushed_blocks == 0 && e.phase() == Phase::Idle,
            "the volume's writes to reach a CP",
        )
    }

    /// Rolls `vol` back to snapshot `snap_id` (the volume is quiesced
    /// first and left offline) and waits until the rollback is durable;
    /// the model takes the snapshot's image as the volume's content from
    /// now on.
    pub fn rollback(&mut self, vol: u32, snap_id: u64) -> Result<(), String> {
        let op = self.request_rollback(vol, snap_id)?;
        for _ in 0..TICK_BUDGET {
            if !self.rollbacks.contains_key(&op) {
                return Ok(());
            }
            self.tick()?;
        }
        Err(format!(
            "the rollback of volume {vol} did not happen within {TICK_BUDGET} ticks"
        ))
    }

    /// Resizes `vol` to `size` blocks (a shrink quiesces the volume and
    /// leaves it offline) and waits until the new size is durable; the
    /// model follows.
    pub fn resize_volume(&mut self, vol: u32, size: u64) -> Result<(), String> {
        let old = self
            .model
            .volumes()
            .find(|(v, _)| *v == vol)
            .map(|(_, s)| s)
            .ok_or_else(|| format!("no volume {vol}"))?;
        if size < old {
            self.quiesce(vol)?;
        }
        self.engine()
            .resize_volume(vol, size)
            .map_err(|e| format!("resize volume {vol} to {size}: {e}"))?;
        if size == old {
            return Ok(());
        }
        let seen = self.events.len();
        for _ in 0..TICK_BUDGET {
            let done = self.events[seen..].iter().any(
                |e| matches!(e, Event::VolumeResized { vol: v, size_blocks, .. } if *v == vol && *size_blocks == size),
            );
            if done {
                self.next_op += 1;
                let op = self.next_op;
                self.model.resize(op, vol, size);
                for (v, s) in &mut self.cfg.volumes {
                    if *v == vol {
                        *s = size;
                    }
                }
                return Ok(());
            }
            self.tick()?;
        }
        Err(format!(
            "the resize of volume {vol} did not happen within {TICK_BUDGET} ticks"
        ))
    }

    /// Deletes `vol` (quiesced first; its snapshots must be gone) and waits
    /// until the deletion is durable; the model and the configuration
    /// forget it.
    pub fn delete_volume(&mut self, vol: u32) -> Result<(), String> {
        self.quiesce(vol)?;
        self.engine()
            .delete_volume(vol)
            .map_err(|e| format!("delete volume {vol}: {e}"))?;
        let seen = self.events.len();
        for _ in 0..TICK_BUDGET {
            let done = self.events[seen..]
                .iter()
                .any(|e| matches!(e, Event::VolumeDeleted { vol: v, .. } if *v == vol));
            if done {
                self.model.remove_volume(vol);
                self.cfg.volumes.retain(|(v, _)| *v != vol);
                self.snaps.retain(|s| s.vol != vol);
                return Ok(());
            }
            self.tick()?;
        }
        Err(format!(
            "the deletion of volume {vol} did not happen within {TICK_BUDGET} ticks"
        ))
    }

    /// Asks for a clean and waits until the CP that carries it commits and
    /// the engine is quiet again.
    pub fn request_clean(&mut self) -> Result<u64, String> {
        let before = self.engine().cp_seq();
        self.engine().request_clean();
        self.tick_until(
            |e| e.cp_seq() > before && e.is_quiet(),
            "the requested clean",
        )?;
        Ok(self.engine().cp_seq())
    }

    /// Checks that the engine's per-segment live counts agree with a walk
    /// of the live map and every snapshot (the accounting never leaks and
    /// never under-counts).
    pub fn audit_liveness(&mut self) -> Result<(), String> {
        let drift = self
            .engine()
            .audit_liveness()
            .map_err(|e| format!("audit: {e}"))?;
        if drift.is_empty() {
            Ok(())
        } else {
            Err(format!("live-block accounting drifted: {drift:?}"))
        }
    }

    /// Runs a pool verification (`otapctl pool verify`) with `opts` to its
    /// end, ticking the engine (consistency points go on meanwhile), and
    /// returns its report.
    pub fn attest_with(&mut self, opts: VerifyOptions) -> Result<VerifyReport, String> {
        self.engine()
            .start_verify(opts)
            .map_err(|e| format!("start the verification: {e}"))?;
        self.tick_until(|e| !e.verify_running(), "the verification")?;
        self.engine()
            .verify_report()
            .ok_or_else(|| "the verification left no report".to_string())
    }

    /// Runs a pool verification with small slices and checks that it found
    /// nothing at all: the pool's every structure, block and count is what
    /// the engine committed.
    pub fn attest_clean(&mut self) -> Result<VerifyReport, String> {
        let report = self.attest_with(VerifyOptions {
            units_per_tick: 64,
            max_ns_per_tick: 0,
        })?;
        if report.state != otap_cp::VerifyState::Done
            || report.verdict != Some(otap_cp::Verdict::Clean)
        {
            return Err(format!(
                "the verification of cp {} ended {:?} {:?} ({} errors, {} warnings): {:?}",
                report.cp_seq,
                report.state,
                report.verdict,
                report.errors,
                report.warnings,
                report.findings
            ));
        }
        Ok(report)
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
        // model as issued until a recovery check settles them. A rollback
        // in flight likewise: the engine forgets the request, and the
        // recovery check tells whether its CP landed.
        self.inflight.clear();
        self.completed.clear();
        self.rollbacks.clear();
    }

    /// Writes submitted and not yet answered, plus rollbacks not yet
    /// reported durable (nothing may be in flight for a snapshot).
    fn unsettled(&self) -> bool {
        !self.inflight.is_empty() || self.model.unresolved() > 0
    }

    /// The pacing delay the engine runs with (0 by default under the
    /// harness's virtual clock); set before [`Sim::boot`].
    pub fn set_pacing_delay(&mut self, delay_ns: u64) {
        self.cfg.pacing_delay_ns = delay_ns;
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
        let boot = self.last_boot.clone().ok_or("no boot to verify")?;
        self.sync_sizes();
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
                        "{:?} restart lost acknowledged writes on {lost:?}: prefix {k} (boot {boot:?})",
                        self.last_stop
                    ));
                }
                if nvfail != held {
                    return Err(format!(
                        "{:?} restart changed nvfail from {held:?} to {nvfail:?} (boot {boot:?})",
                        self.last_stop
                    ));
                }
                if self.last_stop == Stop::Clean && boot.dirty {
                    return Err(format!("a clean shutdown booted dirty: {boot:?}"));
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
        self.verify_snapshots()?;
        self.audit_liveness()?;
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
