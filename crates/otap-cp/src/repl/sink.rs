//! The replication sink (Step 6C): lands a delta into a destination volume
//! and commits it as a snapshot under the source snapshot's identity.
//!
//! ```text
//! handshake ─▶ Hello/HelloAck ─▶ DeltaBegin ─┬─▶ (identity differs and the volume is dirty)
//!                                            │        roll back to the base / delete
//!                                            ▼
//!                                VolEvent: create / resize, then Holes and Chunks:
//!                                placed in the arena, landed when durable (cursor,
//!                                record, Landed + credits)
//!                                            ▼
//!                  DeltaEnd + every chunk landed ─▶ snapshot (Transfer origin, the
//!                  source's uuid) durable ─▶ record: base = target ─▶ Committed
//! ```
//!
//! # What survives a sink restart
//!
//! The record in the [`SinkStore`] (written after each consistency point
//! that made chunks durable, before their `Landed` frames are sent) keeps
//! the delta's identity and its `ResumeBitmap`; the pool keeps the landed
//! blocks and the destination's snapshots. A restarted sink resumes the
//! same delta from its record: the source's next `HelloAck` carries the
//! bitmap and only the chunks missing from it come again. The live volume
//! is offline throughout, and the destination snapshot is taken only once
//! every chunk landed, so a crash never exposes a partial snapshot; the
//! snapshot's existence is itself the commit record (a sink that finds
//! the delta's target among its snapshots at restart finishes the commit).
//! A delta of another identity (the source moved to another target) first
//! rolls the dirty live volume back to the base, or deletes a destination
//! that holds only part of a first transfer. A lost record is rebuilt from
//! the pool: the newest transfer snapshot is the base, and the live volume
//! is treated as dirty.

use std::collections::VecDeque;

use otap_format::{
    BlockKind, IntoBytes, SnapKind, SnapOrigin, StripeDescriptor, StripeId, StripeSummary, Uuid,
    BLOCK_SIZE,
};
use otap_stream::{
    credits_for, Abort, AbortReason, Committed, DeltaEnd, DeltaMode, Frame, FrameMeta, HelloAck,
    LandOutcome, Landed, Pong, ReceiverWindow, VolEventKind,
};

use super::record::SinkRecord;
use super::wire::{Handshake, Refusal, RefuseCode, Role, NO_VOLUME};
use super::{
    chunk_image_len, find_snapshot_by_uuid, newest_transfer, resolve_volume, Backoff, Conn,
    Inbound, JobMode, JobReport, JobState, SinkConfig,
};
use crate::engine::{Engine, EngineError, MAX_PUNCH_BLOCKS, MAX_REQUEST_BLOCKS};
use crate::traits::{Backstore, Clock, Dialer, FrontDoor, Link, SinkStore};

/// What the sink is doing with the destination volume.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Stage {
    /// Waiting for a delta to begin.
    Idle,
    /// Clearing nvfail on the destination (a dirty boot held it).
    Nvfail,
    /// Rolling the live volume back to the base (`Some(snap_id)`), or
    /// deleting a destination that holds part of a first transfer.
    Reset(Option<u64>, bool),
    /// Creating the destination; waiting for the creation to be durable.
    Create,
    /// Resizing the destination to the target's size.
    Resize(u64, bool),
    /// Landing holes and chunks.
    Apply,
    /// Every chunk landed: taking the destination snapshot.
    Snapshot,
    /// The job ended.
    Done,
}

impl Stage {
    const fn word(self) -> &'static str {
        match self {
            Self::Idle => "waiting for a delta",
            Self::Nvfail => "clearing nvfail on the mirror",
            Self::Reset(Some(_), _) => "rolling the mirror back to its base",
            Self::Reset(None, _) => "deleting a partial first transfer",
            Self::Create => "creating the mirror",
            Self::Resize(..) => "resizing the mirror",
            Self::Apply => "landing",
            Self::Snapshot => "taking the snapshot",
            Self::Done => "finished",
        }
    }

    /// The stages in which delta frames are taken.
    const fn takes_frames(self) -> bool {
        matches!(self, Self::Idle | Self::Apply | Self::Snapshot)
    }
}

/// The snapshot the source ships, as its handshake named it.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Target {
    uuid: Uuid,
    cp: u64,
    name: String,
}

/// The delta in flight, from its `DeltaBegin`.
#[derive(Debug, Clone)]
struct Delta {
    cp: u64,
    nonce: u64,
    epoch: u64,
    chunks: u32,
    bytes: u64,
    full: bool,
    snap_name: Option<String>,
    end: Option<DeltaEnd>,
}

/// One write a chunk carries.
#[derive(Debug, Clone, Copy)]
enum Run {
    Write { lba: u64, at: usize, blocks: u32 },
    Punch { lba: u64, blocks: u32 },
}

/// A chunk received and not yet landed.
#[derive(Debug)]
struct PendingChunk {
    idx: u32,
    cp: u64,
    nonce: u64,
    /// The image, in 8-aligned words (the summary parser needs them).
    image: Vec<u64>,
    len: usize,
    runs: Vec<Run>,
    next: usize,
    max_seq: u64,
    /// Credits of copies of this chunk received while it was in flight,
    /// returned with its `Landed`.
    extra: u32,
}

impl PendingChunk {
    fn bytes(&self) -> &[u8] {
        &self.image.as_bytes()[..self.len]
    }

    fn placed(&self) -> bool {
        self.next == self.runs.len()
    }
}

/// What waits to be placed, in arrival order.
#[derive(Debug)]
enum Op {
    Punch {
        lba: u64,
        blocks: u32,
        seq: Option<u64>,
    },
    Chunk(PendingChunk),
}

/// A replication sink job: one destination volume.
pub(crate) struct SinkJob {
    pub(crate) id: u64,
    pub(crate) mode: JobMode,
    addr: String,
    pub(crate) dst_name: String,
    pub(crate) dst_vol: Option<u32>,
    src_volume: String,
    snapshot_name: String,
    agg: Uuid,
    cfg: SinkConfig,
    rec: SinkRecord,
    stored: bool,
    /// A commit found done at the job's start (the destination snapshot
    /// exists, the record had not caught up): its pin and the old base
    /// still need the engine.
    fixup: Option<Uuid>,
    src_agg: Uuid,
    src_vol: u32,
    target: Option<Target>,
    delta: Option<Delta>,
    stage: Stage,
    conn: Option<Conn>,
    hello_done: bool,
    /// A pull sent its request on this connection and waits for the
    /// source's answer.
    awaiting_reply: bool,
    /// `Hello` came while chunks of the delta were still in flight: the
    /// `HelloAck` waits until they landed, so the cursor it carries holds
    /// every chunk received and a resume re-sends none of them.
    hello_pending: bool,
    inbox: VecDeque<(FrameMeta, Frame)>,
    ops: VecDeque<Op>,
    window: ReceiverWindow,
    hasher: blake3::Hasher,
    hashed_upto: u32,
    hash_ok: bool,
    snap_asked: Option<u64>,
    snap_unnamed: bool,
    backoff: Backoff,
    lost_at: u64,
    pub(crate) state: JobState,
    error: Option<String>,
    connections: u32,
    retries: u32,
    chunks_received: u32,
    chunks_landed: u32,
    bytes_landed: u64,
    duplicates: u32,
    digest_verified: Option<bool>,
    frames_sent: u64,
    frames_received: u64,
}

/// The smallest staging a sink offers: 16 credits.
const MIN_STAGING_BYTES: u64 = 64 * 1024;

/// The receive window over the configured staging, bounded to what the
/// stream allows and aligned to whole credits.
fn window_for(cfg: &SinkConfig) -> ReceiverWindow {
    let bytes = cfg
        .staging_bytes
        .clamp(MIN_STAGING_BYTES, otap_stream::flow::MAX_STAGING_BYTES)
        / BLOCK_SIZE as u64
        * BLOCK_SIZE as u64;
    ReceiverWindow::new(bytes)
        .expect("a staging size clamped to the stream's bounds and aligned to credits is valid")
}

impl SinkJob {
    /// A sink job for the destination `dst_name`, with its record loaded
    /// and reconciled with the pool.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn new<B: Backstore, F: FrontDoor, C: Clock>(
        id: u64,
        mode: JobMode,
        eng: &mut Engine<B, F, C>,
        store: &mut dyn SinkStore,
        dst_name: String,
        addr: String,
        src_volume: String,
        snapshot_name: String,
        cfg: SinkConfig,
        now: u64,
    ) -> Self {
        let agg = eng.config().agg_id;
        let dst_vol = resolve_volume(eng, &dst_name);
        let mut job = Self {
            id,
            mode,
            addr,
            dst_name,
            dst_vol,
            src_volume,
            snapshot_name,
            agg,
            window: window_for(&cfg),
            backoff: Backoff::new(cfg.retry_initial_ns, cfg.retry_max_ns, now),
            cfg,
            rec: SinkRecord::new(agg, dst_vol.unwrap_or(NO_VOLUME)),
            stored: false,
            fixup: None,
            src_agg: Uuid::NIL,
            src_vol: NO_VOLUME,
            target: None,
            delta: None,
            stage: Stage::Idle,
            conn: None,
            hello_done: false,
            awaiting_reply: false,
            hello_pending: false,
            inbox: VecDeque::new(),
            ops: VecDeque::new(),
            hasher: blake3::Hasher::new(),
            hashed_upto: 0,
            hash_ok: true,
            snap_asked: None,
            snap_unnamed: false,
            lost_at: now,
            state: JobState::Running,
            error: None,
            connections: 0,
            retries: 0,
            chunks_received: 0,
            chunks_landed: 0,
            bytes_landed: 0,
            duplicates: 0,
            digest_verified: None,
            frames_sent: 0,
            frames_received: 0,
        };
        job.load(eng, store);
        job
    }

    /// Loads the destination's record and reconciles it with the pool.
    fn load<B: Backstore, F: FrontDoor, C: Clock>(
        &mut self,
        eng: &mut Engine<B, F, C>,
        store: &mut dyn SinkStore,
    ) {
        let Some(vol) = self.dst_vol else {
            return;
        };
        let loaded = match store.load(&SinkRecord::key(vol)) {
            Ok(Some(bytes)) => match SinkRecord::decode(&bytes) {
                Ok(rec) if rec.dst_agg == self.agg && rec.dst_vol == vol => Some(rec),
                Ok(_) => None,
                Err(e) => {
                    self.error = Some(format!("the sink record was unreadable: {e}"));
                    None
                }
            },
            Ok(None) => None,
            Err(e) => {
                self.error = Some(format!("the sink record could not be read: {e}"));
                None
            }
        };
        let pool_base = newest_transfer(eng, vol);
        let pool_base_uuid = pool_base.as_ref().map_or(Uuid::NIL, |b| b.uuid);
        match loaded {
            None => {
                // No record: the pool says what is committed; whether the
                // live volume holds part of a delta is unknown.
                self.rec = SinkRecord::new(self.agg, vol);
                self.rec.base = pool_base_uuid;
                self.rec.dirty = true;
            }
            Some(mut rec) => {
                self.stored = true;
                if rec.pool_cp > eng.cp_seq() {
                    // The pool is behind the record (a committed CP lost
                    // under a lying cache): the landing is not to be
                    // trusted.
                    rec.cursor.abort();
                    rec.landing_target = Uuid::NIL;
                    rec.landing_from = Uuid::NIL;
                    rec.dirty = true;
                }
                if !rec.landing_target.is_nil() && pool_base_uuid == rec.landing_target {
                    // The destination snapshot was taken and the record did
                    // not catch up: the commit is done.
                    let old = rec.base;
                    let cp = rec.cursor.landing.as_ref().map_or(0, |l| l.cp_seq);
                    rec.cursor.landing = None;
                    rec.cursor.applied_cp_seq = cp;
                    rec.base = rec.landing_target;
                    rec.landing_target = Uuid::NIL;
                    rec.landing_from = Uuid::NIL;
                    rec.dirty = false;
                    self.fixup = Some(old);
                } else if pool_base_uuid != rec.base {
                    rec.base = pool_base_uuid;
                    rec.cursor.abort();
                    rec.cursor.applied_cp_seq = 0;
                    rec.landing_target = Uuid::NIL;
                    rec.landing_from = Uuid::NIL;
                    rec.dirty = true;
                } else if pool_base.is_some()
                    && pool_base
                        .as_ref()
                        .is_some_and(|b| b.flags & otap_format::PIN_REPLICATION == 0)
                {
                    self.fixup = Some(Uuid::NIL);
                }
                self.rec = rec;
            }
        }
        if eng.volume(vol).is_some_and(|v| v.present && v.offline) {
            let _ = eng.set_replica(vol, true);
        }
    }

    /// Takes the connection of a pull this job dialed: its request goes
    /// first.
    pub(crate) fn open(&mut self, link: Box<dyn Link>, now: u64) {
        let mut conn = Conn::new(link, now);
        let mut h = Handshake::new(Role::Sink, false, self.agg);
        h.volume_index = self.dst_vol.unwrap_or(NO_VOLUME);
        h.base = self.rec.base;
        h.base_cp = self.rec.cursor.applied_cp_seq;
        h.volume = self.dst_name.clone();
        h.peer_volume = self.src_volume.clone();
        h.snapshot_name = self.snapshot_name.clone();
        match conn.queue_handshake(&h) {
            Ok(()) => {
                self.conn = Some(conn);
                self.hello_done = false;
                self.awaiting_reply = true;
                self.connections += 1;
            }
            Err(e) => self.fail(e),
        }
    }

    /// Checks a source's handshake against the destination; the answer is
    /// the handshake to send.
    fn check_source<B: Backstore, F: FrontDoor, C: Clock>(
        &mut self,
        eng: &mut Engine<B, F, C>,
        h: &Handshake,
    ) -> Result<(), Refusal> {
        if h.agg_id == self.agg {
            return Err(Refusal::new(
                RefuseCode::Conflict,
                "the source and the destination are the same pool: a pool cannot replicate to \
                 itself",
            ));
        }
        if h.snapshot.is_nil() || h.snapshot_cp == 0 {
            return Err(Refusal::new(
                RefuseCode::BadRequest,
                "the source named no snapshot to ship",
            ));
        }
        if !self.src_agg.is_nil() && (self.src_agg != h.agg_id || self.src_vol != h.volume_index) {
            return Err(Refusal::new(
                RefuseCode::Busy,
                format!(
                    "a transfer from volume {} of pool {} into {:?} is running",
                    self.src_vol, self.src_agg, self.dst_name
                ),
            ));
        }
        self.dst_vol = resolve_volume(eng, &self.dst_name).or(self.dst_vol);
        if let Some(vol) = self.dst_vol {
            let Some(v) = eng.volume(vol).filter(|v| v.present) else {
                self.dst_vol = None;
                return Ok(());
            };
            if !v.offline {
                return Err(Refusal::new(
                    RefuseCode::Conflict,
                    format!(
                        "the destination volume {:?} is online; a replication destination is a \
                         mirror and must be offline (or name a new volume)",
                        self.dst_name
                    ),
                ));
            }
            let base = newest_transfer(eng, vol);
            if !self.stored && base.is_none() && !eng.is_replica(vol) {
                return Err(Refusal::new(
                    RefuseCode::Conflict,
                    format!(
                        "the destination volume {:?} exists and is not a replication destination; \
                         name a new volume or delete it",
                        self.dst_name
                    ),
                ));
            }
            if !self.rec.src_agg.is_nil()
                && (self.rec.src_agg != h.agg_id || self.rec.src_vol != h.volume_index)
            {
                return Err(Refusal::new(
                    RefuseCode::Conflict,
                    format!(
                        "the destination volume {:?} is the mirror of volume {} of pool {}, not of \
                         volume {} of pool {}",
                        self.dst_name,
                        self.rec.src_vol,
                        self.rec.src_agg,
                        h.volume_index,
                        h.agg_id
                    ),
                ));
            }
            if base.as_ref().is_some_and(|b| b.damaged) {
                return Err(Refusal::new(
                    RefuseCode::Quarantined,
                    "the destination's base snapshot is quarantined (damaged): it cannot be a \
                     replication base; a full transfer needs a new destination volume",
                ));
            }
            if !eng.is_replica(vol) {
                eng.set_replica(vol, true).map_err(|e| {
                    Refusal::new(RefuseCode::Internal, format!("marking the mirror: {e}"))
                })?;
            }
        }
        self.src_agg = h.agg_id;
        self.src_vol = h.volume_index;
        self.target = Some(Target {
            uuid: h.snapshot,
            cp: h.snapshot_cp,
            name: h.snapshot_name.clone(),
        });
        Ok(())
    }

    /// Takes a push's connection (a new one, or the source coming back),
    /// answering its handshake: `Err` carries the refusal to send.
    pub(crate) fn attach<B: Backstore, F: FrontDoor, C: Clock>(
        &mut self,
        eng: &mut Engine<B, F, C>,
        mut conn: Conn,
        h: &Handshake,
        now: u64,
    ) -> Result<(), (Conn, Refusal)> {
        if h.role != Role::Source || h.accept {
            return Err((
                conn,
                Refusal::new(
                    RefuseCode::BadRequest,
                    "a push must come from a source's request",
                ),
            ));
        }
        if let Err(r) = self.check_source(eng, h) {
            return Err((conn, r));
        }
        // The source came back on a new connection: the old one goes (only
        // now, so a refused stranger never cuts a running transfer).
        if let Some(old) = &mut self.conn {
            self.frames_sent += old.frames_sent;
            self.frames_received += old.frames_received;
            old.close();
        }
        self.conn = None;
        self.inbox.clear();
        let mut reply = Handshake::new(Role::Sink, true, self.agg);
        reply.volume_index = self.dst_vol.unwrap_or(NO_VOLUME);
        reply.base = self.rec.base;
        reply.base_cp = self.rec.cursor.applied_cp_seq;
        reply.volume = self.dst_name.clone();
        reply.peer_volume = h.volume.clone();
        if let Err(e) = conn.queue_handshake(&reply) {
            return Err((conn, Refusal::new(RefuseCode::Internal, e)));
        }
        self.src_volume = h.volume.clone();
        self.snapshot_name = h.snapshot_name.clone();
        self.addr = conn.peer();
        self.connections += 1;
        self.hello_done = false;
        self.hello_pending = false;
        self.conn = Some(conn);
        self.backoff.progress(now);
        if h.snapshot == self.rec.base {
            // The destination holds the target already: nothing to land.
            self.finish_done();
        }
        Ok(())
    }

    pub(crate) fn fail(&mut self, why: impl Into<String>) {
        if let Some(conn) = &mut self.conn {
            conn.close();
        }
        self.conn = None;
        self.ops.clear();
        self.inbox.clear();
        self.error = Some(why.into());
        self.state = JobState::Failed;
        self.stage = Stage::Done;
    }

    pub(crate) fn cancel(&mut self) {
        self.fail("cancelled by the operator");
        self.state = JobState::Cancelled;
    }

    /// Ends the job with the target committed (or held already).
    fn finish_done(&mut self) {
        if let Some(conn) = &mut self.conn {
            self.frames_sent += conn.frames_sent;
            self.frames_received += conn.frames_received;
            conn.close();
        }
        self.conn = None;
        self.state = JobState::Done;
        self.stage = Stage::Done;
    }

    /// The connection is gone: a pull dials again; a push waits for its
    /// source. Landed chunks stay landed; chunks in flight keep landing.
    fn lose(&mut self, why: String, now: u64) {
        if let Some(conn) = &mut self.conn {
            self.frames_sent += conn.frames_sent;
            self.frames_received += conn.frames_received;
            conn.close();
        }
        self.conn = None;
        self.hello_done = false;
        self.hello_pending = false;
        self.inbox.clear();
        self.lost_at = now;
        self.error = Some(why.clone());
        if self.stage == Stage::Done {
            return;
        }
        if self.mode == JobMode::Pull {
            self.retries += 1;
            if !self.backoff.failed(now, self.cfg.max_retries) {
                self.fail(format!(
                    "gave up after {} consecutive failed connections; last: {why}",
                    self.cfg.max_retries
                ));
            }
        }
    }

    /// Writes the record (when the destination exists).
    fn persist<B: Backstore, F: FrontDoor, C: Clock>(
        &mut self,
        eng: &Engine<B, F, C>,
        store: &mut dyn SinkStore,
    ) -> Result<(), String> {
        let Some(vol) = self.dst_vol else {
            return Ok(());
        };
        self.rec.dst_agg = self.agg;
        self.rec.dst_vol = vol;
        if !self.src_agg.is_nil() {
            self.rec.src_agg = self.src_agg;
            self.rec.src_vol = self.src_vol;
        }
        self.rec.pool_cp = eng.cp_seq();
        let bytes = self.rec.encode()?;
        store
            .save(&SinkRecord::key(vol), &bytes)
            .map_err(|e| format!("the sink record cannot be saved: {e}"))?;
        self.stored = true;
        Ok(())
    }

    /// One step; `true` when it did work.
    pub(crate) fn step<B: Backstore, F: FrontDoor, C: Clock>(
        &mut self,
        eng: &mut Engine<B, F, C>,
        dialer: &mut dyn Dialer,
        store: &mut dyn SinkStore,
        now: u64,
    ) -> bool {
        if self.state != JobState::Running {
            return false;
        }
        let mut busy = false;
        if let Some(old) = self.fixup.take() {
            busy = true;
            self.fix_pins(eng, old);
        }
        if self.conn.is_none() {
            if self.mode == JobMode::Pull {
                if now >= self.backoff.next_at {
                    match dialer.dial(&self.addr) {
                        Ok(link) => self.open(link, now),
                        Err(e) => self.lose(e.to_string(), now),
                    }
                    busy = true;
                }
            } else if now.saturating_sub(self.lost_at) > self.cfg.linger_ns && self.ops.is_empty() {
                self.fail(format!(
                    "the source did not come back within {} s; the next transfer resumes from the \
                     record",
                    self.cfg.linger_ns / 1_000_000_000
                ));
                return true;
            }
        }
        busy |= self.read(eng, now);
        if self.state != JobState::Running {
            return true;
        }
        busy |= self.step_stage(eng, store);
        busy |= self.take_inbox(eng, store, now);
        if self.state != JobState::Running {
            return true;
        }
        busy |= self.apply(eng);
        busy |= self.land(eng, store, now);
        if self.hello_pending && !self.ops.iter().any(|op| matches!(op, Op::Chunk(_))) {
            self.hello_pending = false;
            if let Err(e) = self.hello(now) {
                self.lose(e, now);
                return true;
            }
            busy = true;
        }
        if self.state != JobState::Running {
            return true;
        }
        busy |= self.finish(eng, store, now);
        if let Some(conn) = self.conn.as_mut() {
            if let Err(e) = conn.flush(now) {
                self.lose(e, now);
                return true;
            }
            if now.saturating_sub(conn.last_recv_ns) > self.cfg.idle_timeout_ns {
                let secs = self.cfg.idle_timeout_ns / 1_000_000_000;
                self.lose(format!("nothing from the source for {secs} s"), now);
                return true;
            }
            if self.hello_done && now.saturating_sub(conn.last_send_ns) > self.cfg.keepalive_ns {
                let ping = otap_stream::Ping {
                    nonce: now,
                    mono_ns: now,
                };
                let meta = self.meta(0);
                if let Some(conn) = self.conn.as_mut() {
                    if let Err(e) = conn.queue_frame(&meta, &Frame::Ping(ping)) {
                        self.fail(e);
                    }
                }
            }
        }
        busy
    }

    fn meta(&self, chunk_idx: u32) -> FrameMeta {
        match &self.delta {
            Some(d) => FrameMeta {
                rel: 0,
                chunk_idx,
                cp_seq: d.cp,
                base: self.rec.cursor.applied_cp_seq.min(d.cp),
                cp_nonce: d.nonce,
                owner_epoch: d.epoch,
            },
            None => FrameMeta {
                chunk_idx,
                ..FrameMeta::default()
            },
        }
    }

    /// Reads what the link holds: answers handshakes, `Hello` and `Ping`
    /// at once, and queues the delta's frames in order.
    fn read<B: Backstore, F: FrontDoor, C: Clock>(
        &mut self,
        eng: &mut Engine<B, F, C>,
        now: u64,
    ) -> bool {
        let mut busy = false;
        for _ in 0..self.cfg.frames_per_step {
            if self.inbox.len() >= self.cfg.inbox_frames {
                break;
            }
            let Some(conn) = self.conn.as_mut() else {
                break;
            };
            let msg = match conn.recv(now) {
                Ok(Some(msg)) => msg,
                Ok(None) => break,
                Err(e) => {
                    self.lose(e, now);
                    return true;
                }
            };
            busy = true;
            if let Err(e) = self.on_message(eng, msg, now) {
                self.lose(e, now);
                return true;
            }
            if self.state != JobState::Running || self.conn.is_none() {
                return true;
            }
        }
        busy
    }

    fn on_message<B: Backstore, F: FrontDoor, C: Clock>(
        &mut self,
        eng: &mut Engine<B, F, C>,
        msg: Inbound,
        now: u64,
    ) -> Result<(), String> {
        match msg {
            Inbound::Handshake(h) => {
                if self.mode != JobMode::Pull || !self.awaiting_reply || h.role != Role::Source {
                    return Err("an unexpected handshake from the source".to_string());
                }
                if !h.accept {
                    return Err("the source answered with a request".to_string());
                }
                self.awaiting_reply = false;
                if let Err(r) = self.check_source(eng, &h) {
                    self.fail(r.text);
                    return Ok(());
                }
                self.backoff.progress(now);
                if h.snapshot == self.rec.base {
                    self.finish_done();
                }
                Ok(())
            }
            Inbound::Refuse(r) => {
                if r.code == RefuseCode::Busy {
                    return Err(r.to_string());
                }
                self.fail(r.to_string());
                Ok(())
            }
            Inbound::Version(v) => {
                self.fail(format!(
                    "the source speaks replication protocol {v}; this side speaks {}",
                    super::wire::REPL_PROTOCOL_VERSION
                ));
                Ok(())
            }
            Inbound::Corrupt { kind, idx } => {
                let abort = Abort {
                    reason: AbortReason::Corrupt,
                    idx: if kind == otap_format::FrameKind::Chunk.as_u8() {
                        idx
                    } else {
                        0
                    },
                    detail: u64::from(kind),
                };
                let meta = self.meta(0);
                if let Some(conn) = self.conn.as_mut() {
                    let _ = conn.queue_frame(&meta, &Frame::Abort(abort));
                }
                Err(format!(
                    "a frame of kind {kind} (chunk {idx}) failed its payload crc; the source \
                     resends from the cursor"
                ))
            }
            Inbound::Frame(meta, frame) => self.on_frame(eng, meta, frame),
        }
    }

    fn on_frame<B: Backstore, F: FrontDoor, C: Clock>(
        &mut self,
        eng: &mut Engine<B, F, C>,
        meta: FrameMeta,
        frame: Frame,
    ) -> Result<(), String> {
        match frame {
            Frame::Hello(hello) => {
                if self.target.is_none() {
                    return Err("a Hello before the handshake".to_string());
                }
                if hello.node_id != self.src_agg {
                    return Err(format!(
                        "the Hello names node {}, the handshake pool {}",
                        hello.node_id, self.src_agg
                    ));
                }
                self.hello_pending = true;
                Ok(())
            }
            Frame::Ping(p) => {
                let meta = self.meta(0);
                if let Some(conn) = self.conn.as_mut() {
                    conn.queue_frame(
                        &meta,
                        &Frame::Pong(Pong {
                            nonce: p.nonce,
                            mono_ns: p.mono_ns,
                        }),
                    )?;
                }
                Ok(())
            }
            Frame::Pong(_) => Ok(()),
            Frame::Chunk(_)
            | Frame::DeltaBegin(_)
            | Frame::VolEvent(_)
            | Frame::Holes(_)
            | Frame::DeltaEnd(_)
            | Frame::Abort(_) => {
                if !self.hello_done {
                    return Err(format!("a {:?} frame before Hello", frame.kind()));
                }
                if matches!(frame, Frame::Chunk(_))
                    && self.inbox.is_empty()
                    && self.stage == Stage::Apply
                {
                    return self.chunk(eng, meta, frame);
                }
                self.inbox.push_back((meta, frame));
                Ok(())
            }
            other => Err(format!("a {:?} frame from the source", other.kind())),
        }
    }

    /// Answers `Hello` with the cursor: the landed chunks and the credits
    /// the staging allows (chunks still in flight keep theirs).
    fn hello(&mut self, now: u64) -> Result<(), String> {
        let mut window = window_for(&self.cfg);
        for op in &self.ops {
            if let Op::Chunk(c) = op {
                let credits = credits_for(c.len);
                window.grant_up_to(credits);
                window
                    .landed(c.len)
                    .map_err(|e| format!("re-accounting the staging: {e}"))?;
            }
        }
        let mut cursor = self.rec.cursor.clone();
        cursor.credits = window.initial_grant();
        self.window = window;
        let meta = self.meta(0);
        let conn = self
            .conn
            .as_mut()
            .ok_or_else(|| "no connection".to_string())?;
        conn.queue_frame(&meta, &Frame::HelloAck(HelloAck { cursor }))?;
        conn.flush(now)?;
        self.hello_done = true;
        Ok(())
    }

    /// Progresses a stage that waits for the engine.
    fn step_stage<B: Backstore, F: FrontDoor, C: Clock>(
        &mut self,
        eng: &mut Engine<B, F, C>,
        store: &mut dyn SinkStore,
    ) -> bool {
        // A dirty boot held the mirror: what landed is in committed CPs
        // (the record never says more), so the hold can go.
        if let Some(vol) = self.dst_vol {
            if self.stage.takes_frames()
                && eng
                    .volume(vol)
                    .is_some_and(|v| v.present && v.nvfail.is_some())
            {
                match eng.nvfail_clear(vol) {
                    Ok(_) => self.stage = Stage::Nvfail,
                    Err(e) => {
                        self.fail(format!("clearing nvfail on the mirror: {e}"));
                        return true;
                    }
                }
            }
        }
        match self.stage {
            Stage::Nvfail => {
                let Some(vol) = self.dst_vol else {
                    self.stage = Stage::Idle;
                    return true;
                };
                if eng.volume(vol).is_some_and(|v| v.nvfail.is_none()) {
                    self.stage = if self.delta.is_some() {
                        Stage::Apply
                    } else {
                        Stage::Idle
                    };
                    return true;
                }
                false
            }
            Stage::Reset(base, started) => self.step_reset(eng, store, base, started),
            Stage::Create => {
                let Some(vol) = self.dst_vol else {
                    self.fail("the mirror vanished while it was being created");
                    return true;
                };
                if eng.volume_ready(vol) {
                    self.stage = Stage::Apply;
                    if let Err(e) = self.persist(eng, store) {
                        self.fail(e);
                    }
                    return true;
                }
                false
            }
            Stage::Resize(size, started) => {
                let Some(vol) = self.dst_vol else {
                    self.fail("the mirror vanished while it was being resized");
                    return true;
                };
                if !started {
                    match eng.resize_volume(vol, size) {
                        Ok(()) => self.stage = Stage::Resize(size, true),
                        Err(EngineError::VolumeDirty { .. })
                        | Err(EngineError::VolumeBusy { .. }) => {
                            eng.request_cp();
                        }
                        Err(e) => {
                            self.fail(format!("resizing the mirror to {size} blocks: {e}"));
                        }
                    }
                    return true;
                }
                if eng.volume_ready(vol) && eng.volume(vol).is_some_and(|v| v.size_blocks == size) {
                    self.stage = Stage::Apply;
                    return true;
                }
                false
            }
            _ => false,
        }
    }

    fn step_reset<B: Backstore, F: FrontDoor, C: Clock>(
        &mut self,
        eng: &mut Engine<B, F, C>,
        store: &mut dyn SinkStore,
        base: Option<u64>,
        started: bool,
    ) -> bool {
        let Some(vol) = self.dst_vol else {
            self.reset_done(eng, store);
            return true;
        };
        if !started {
            let asked = match base {
                Some(snap_id) => eng.rollback_volume(vol, snap_id),
                None => eng.delete_volume(vol),
            };
            match asked {
                Ok(()) => self.stage = Stage::Reset(base, true),
                Err(EngineError::VolumeDirty { .. }) | Err(EngineError::VolumeBusy { .. }) => {
                    eng.request_cp();
                }
                Err(e) => self.fail(format!(
                    "{} the mirror before the new delta: {e}",
                    if base.is_some() {
                        "rolling back"
                    } else {
                        "deleting"
                    }
                )),
            }
            return true;
        }
        let gone = match base {
            Some(_) => eng.volume_ready(vol),
            None => !eng.volume(vol).is_some_and(|v| v.present),
        };
        if !gone {
            return false;
        }
        if base.is_none() {
            let _ = store.remove(&SinkRecord::key(vol));
            self.stored = false;
            self.dst_vol = None;
        }
        self.reset_done(eng, store);
        true
    }

    fn reset_done<B: Backstore, F: FrontDoor, C: Clock>(
        &mut self,
        eng: &Engine<B, F, C>,
        store: &mut dyn SinkStore,
    ) {
        self.rec.cursor.abort();
        self.rec.landing_target = Uuid::NIL;
        self.rec.landing_from = Uuid::NIL;
        self.rec.dirty = false;
        self.delta = None;
        self.stage = Stage::Idle;
        if let Err(e) = self.persist(eng, store) {
            self.fail(e);
        }
    }

    /// Takes the queued delta frames while the stage allows.
    fn take_inbox<B: Backstore, F: FrontDoor, C: Clock>(
        &mut self,
        eng: &mut Engine<B, F, C>,
        store: &mut dyn SinkStore,
        now: u64,
    ) -> bool {
        let mut busy = false;
        while self.stage.takes_frames() && self.state == JobState::Running {
            let Some((meta, frame)) = self.inbox.pop_front() else {
                break;
            };
            busy = true;
            match self.delta_frame(eng, store, meta, frame.clone(), now) {
                Ok(true) => {}
                Ok(false) => {
                    self.inbox.push_front((meta, frame));
                    break;
                }
                Err(e) => {
                    self.lose(e, now);
                    break;
                }
            }
        }
        busy
    }

    /// Sends an abort, then fails or loses the connection.
    fn abort(&mut self, reason: AbortReason, detail: u64, why: String, fatal: bool, now: u64) {
        let meta = self.meta(0);
        if let Some(conn) = self.conn.as_mut() {
            let _ = conn.queue_frame(
                &meta,
                &Frame::Abort(Abort {
                    reason,
                    idx: 0,
                    detail,
                }),
            );
        }
        if fatal {
            self.fail(why);
        } else {
            self.lose(why, now);
        }
    }

    /// One delta frame: `Ok(false)` when the stage must change first (the
    /// frame is taken again later).
    fn delta_frame<B: Backstore, F: FrontDoor, C: Clock>(
        &mut self,
        eng: &mut Engine<B, F, C>,
        store: &mut dyn SinkStore,
        meta: FrameMeta,
        frame: Frame,
        now: u64,
    ) -> Result<bool, String> {
        let target = self
            .target
            .clone()
            .ok_or_else(|| "a delta frame before the handshake".to_string())?;
        if let Frame::DeltaBegin(begin) = &frame {
            return self.begin(eng, store, &target, meta, begin, now);
        }
        let delta = self
            .delta
            .clone()
            .ok_or_else(|| format!("a {:?} frame outside a delta", frame.kind()))?;
        if meta.cp_seq != delta.cp || meta.cp_nonce != delta.nonce {
            return Err(format!(
                "a {:?} frame of delta (cp {}, nonce {:#x}) inside delta (cp {}, nonce {:#x})",
                frame.kind(),
                meta.cp_seq,
                meta.cp_nonce,
                delta.cp,
                delta.nonce
            ));
        }
        match frame {
            Frame::VolEvent(ev) => {
                if ev.vol != self.src_vol {
                    self.abort(
                        AbortReason::UnknownVol,
                        u64::from(ev.vol),
                        format!(
                            "a volume event names volume {}, the source volume is {}",
                            ev.vol, self.src_vol
                        ),
                        true,
                        now,
                    );
                    return Ok(true);
                }
                self.vol_event(eng, &delta, &ev)
            }
            Frame::Holes(holes) => {
                let Some(vol) = self.dst_vol else {
                    return Err("holes before the mirror exists".to_string());
                };
                let size = eng.volume(vol).map_or(0, |v| v.size_blocks);
                for rec in &holes.records {
                    if rec.vol != self.src_vol {
                        self.abort(
                            AbortReason::UnknownVol,
                            u64::from(rec.vol),
                            format!("a hole names volume {}", rec.vol),
                            true,
                            now,
                        );
                        return Ok(true);
                    }
                    let end = rec.lba.saturating_add(u64::from(rec.len));
                    if end > size {
                        return Err(format!(
                            "a hole at {}+{} runs past the mirror's {size} blocks",
                            rec.lba, rec.len
                        ));
                    }
                    let mut lba = rec.lba;
                    while lba < end {
                        let blocks = (end - lba).min(u64::from(MAX_PUNCH_BLOCKS)) as u32;
                        self.ops.push_back(Op::Punch {
                            lba,
                            blocks,
                            seq: None,
                        });
                        lba += u64::from(blocks);
                    }
                }
                Ok(true)
            }
            Frame::Chunk(_) => {
                if self.stage != Stage::Apply {
                    return Ok(false);
                }
                self.chunk(eng, meta, frame)?;
                Ok(true)
            }
            Frame::DeltaEnd(end) => {
                if end.chunks != delta.chunks || end.bytes != delta.bytes {
                    return Err(format!(
                        "DeltaEnd says {} chunks and {} bytes, DeltaBegin {} and {}",
                        end.chunks, end.bytes, delta.chunks, delta.bytes
                    ));
                }
                if let Some(d) = self.delta.as_mut() {
                    d.end = Some(end);
                }
                Ok(true)
            }
            Frame::Abort(a) => {
                // The source gave the delta up; what landed stays landed.
                self.error = Some(format!("the source aborted the delta ({:?})", a.reason));
                Ok(true)
            }
            other => Err(format!("a {:?} frame inside a delta", other.kind())),
        }
    }

    fn begin<B: Backstore, F: FrontDoor, C: Clock>(
        &mut self,
        eng: &mut Engine<B, F, C>,
        store: &mut dyn SinkStore,
        target: &Target,
        meta: FrameMeta,
        begin: &otap_stream::DeltaBegin,
        now: u64,
    ) -> Result<bool, String> {
        if self.stage == Stage::Snapshot {
            // The same delta begun again while its snapshot is being taken
            // (the source reconnected): nothing new to land.
            if self
                .delta
                .as_ref()
                .is_some_and(|d| d.cp == meta.cp_seq && d.nonce == meta.cp_nonce)
            {
                return Ok(true);
            }
            return Ok(false);
        }
        if meta.cp_seq != target.cp {
            return Err(format!(
                "the delta ends at cp {}, the handshake named cp {}",
                meta.cp_seq, target.cp
            ));
        }
        if !begin.tags.iter().any(|t| t.uuid == target.uuid) {
            return Err("the delta does not tag the handshake's snapshot".to_string());
        }
        let (full, from) = match begin.mode {
            DeltaMode::Steady => (true, Uuid::NIL),
            DeltaMode::Increment { from, to } => {
                if to != target.uuid {
                    return Err("the delta ends at another snapshot than the handshake's".into());
                }
                (false, from)
            }
        };
        let base_ok = if full {
            self.rec.base.is_nil() && meta.base == 0
        } else {
            from == self.rec.base
        };
        if !base_ok {
            self.abort(
                AbortReason::BaseMismatch,
                meta.base,
                format!(
                    "the delta's base {} is not the destination's base {}",
                    if full {
                        "(none)".to_string()
                    } else {
                        from.to_string()
                    },
                    if self.rec.base.is_nil() {
                        "(none)".to_string()
                    } else {
                        self.rec.base.to_string()
                    }
                ),
                true,
                now,
            );
            return Ok(true);
        }
        let same = self
            .rec
            .cursor
            .landing
            .as_ref()
            .is_some_and(|l| l.cp_seq == meta.cp_seq && l.cp_nonce == meta.cp_nonce)
            && self.rec.landing_target == target.uuid;
        let foreign = self.ops.iter().any(|op| match op {
            Op::Chunk(c) => c.cp != meta.cp_seq || c.nonce != meta.cp_nonce,
            Op::Punch { .. } => !same,
        });
        if !same && (self.rec.dirty || foreign) {
            for op in std::mem::take(&mut self.ops) {
                if let Op::Chunk(c) = op {
                    let _ = self.window.drained(c.len);
                }
            }
            let base = if self.rec.base.is_nil() {
                None
            } else {
                match self
                    .dst_vol
                    .and_then(|vol| find_snapshot_by_uuid(eng, vol, self.rec.base))
                {
                    Some(info) => Some(info.snap_id),
                    None => {
                        self.fail(
                            "the destination's base snapshot is gone; it cannot be rolled back to",
                        );
                        return Ok(true);
                    }
                }
            };
            self.stage = Stage::Reset(base, false);
            return Ok(false);
        }
        self.rec.cursor.applied_cp_seq = meta.base;
        let outcome = self
            .rec
            .cursor
            .begin(meta.base, meta.cp_seq, meta.cp_nonce, begin.chunks)
            .map_err(|e| format!("DeltaBegin: {e}"))?;
        if outcome != otap_stream::BeginOutcome::Resumed {
            self.rec.landing_target = target.uuid;
            self.rec.landing_from = from;
            self.hasher = blake3::Hasher::new();
            self.hashed_upto = 0;
            self.hash_ok = true;
        } else if let Some(l) = &self.rec.cursor.landing {
            // The digest is verified only over an unbroken run of landings
            // seen by this process.
            if self.hashed_upto != l.bitmap.landed_contig()
                || l.bitmap.landed_count() != l.bitmap.landed_contig()
            {
                self.hash_ok = false;
            }
        }
        self.rec.dirty = true;
        self.delta = Some(Delta {
            cp: meta.cp_seq,
            nonce: meta.cp_nonce,
            epoch: meta.owner_epoch,
            chunks: begin.chunks,
            bytes: begin.bytes,
            full,
            snap_name: (!target.name.is_empty()).then(|| target.name.clone()),
            end: None,
        });
        self.digest_verified = None;
        self.stage = Stage::Apply;
        self.persist(eng, store)?;
        Ok(true)
    }

    fn vol_event<B: Backstore, F: FrontDoor, C: Clock>(
        &mut self,
        eng: &mut Engine<B, F, C>,
        delta: &Delta,
        ev: &otap_stream::VolEvent,
    ) -> Result<bool, String> {
        match ev.event {
            VolEventKind::Create => {
                if !delta.full {
                    return Err("a volume creation inside an incremental delta".to_string());
                }
                match self
                    .dst_vol
                    .filter(|v| eng.volume(*v).is_some_and(|s| s.present))
                {
                    Some(vol) => {
                        if eng
                            .volume(vol)
                            .is_some_and(|v| v.size_blocks != ev.size_blocks)
                        {
                            self.stage = Stage::Resize(ev.size_blocks, false);
                        }
                    }
                    None => {
                        let vol = eng
                            .create_named_volume(&self.dst_name, ev.size_blocks)
                            .map_err(|e| format!("creating the mirror {:?}: {e}", self.dst_name))?;
                        eng.set_volume_online(vol, false)
                            .map_err(|e| format!("taking the mirror offline: {e}"))?;
                        eng.set_replica(vol, true)
                            .map_err(|e| format!("marking the mirror: {e}"))?;
                        self.dst_vol = Some(vol);
                        self.stage = Stage::Create;
                    }
                }
                Ok(true)
            }
            VolEventKind::Resize => {
                let Some(vol) = self.dst_vol else {
                    let now = eng.mono_ns();
                    self.abort(
                        AbortReason::UnknownVol,
                        u64::from(ev.vol),
                        "an incremental delta for a mirror that does not exist".to_string(),
                        true,
                        now,
                    );
                    return Ok(true);
                };
                if eng
                    .volume(vol)
                    .is_some_and(|v| v.size_blocks != ev.size_blocks)
                {
                    self.stage = Stage::Resize(ev.size_blocks, false);
                }
                Ok(true)
            }
            VolEventKind::SnapTag => {
                if ev.uuid != self.target.as_ref().map_or(Uuid::NIL, |t| t.uuid) {
                    return Err("the delta tags a snapshot other than its target".to_string());
                }
                if let Some(d) = self.delta.as_mut() {
                    d.snap_name = (!ev.name.is_empty()).then(|| ev.name.clone());
                }
                Ok(true)
            }
            VolEventKind::Delete => Err("a delta never deletes the destination volume".to_string()),
        }
    }

    /// A chunk: a duplicate is answered at once; a new one is verified
    /// (identity, summary, every block's CRC) and queued for placing.
    fn chunk<B: Backstore, F: FrontDoor, C: Clock>(
        &mut self,
        eng: &mut Engine<B, F, C>,
        meta: FrameMeta,
        frame: Frame,
    ) -> Result<(), String> {
        let Frame::Chunk(chunk) = frame else {
            return Err("not a chunk".to_string());
        };
        let delta = self
            .delta
            .clone()
            .ok_or_else(|| "a chunk outside a delta".to_string())?;
        let idx = meta.chunk_idx;
        if idx >= delta.chunks {
            return Err(format!("chunk {idx} of a delta of {} chunks", delta.chunks));
        }
        if chunk.codec != otap_format::Codec::Raw {
            return Err(format!("chunk {idx} carries codec {:?}", chunk.codec));
        }
        let len = chunk.image.len();
        self.window
            .landed(len)
            .map_err(|e| format!("chunk {idx} exceeds the credits granted: {e}"))?;
        self.chunks_received += 1;
        let landed = self
            .rec
            .cursor
            .landing
            .as_ref()
            .is_some_and(|l| l.bitmap.is_landed(idx));
        if landed {
            self.duplicates += 1;
            self.window
                .drained(len)
                .map_err(|e| format!("re-accounting a duplicate: {e}"))?;
            let credits = self.window.grant_up_to(credits_for(len));
            let contig = self
                .rec
                .cursor
                .landing
                .as_ref()
                .map_or(0, |l| l.bitmap.landed_contig());
            let meta = self.meta(0);
            if let Some(conn) = self.conn.as_mut() {
                conn.queue_frame(
                    &meta,
                    &Frame::Landed(Landed {
                        idx,
                        credits,
                        landed_contig: contig,
                    }),
                )?;
            }
            return Ok(());
        }
        if let Some(Op::Chunk(pending)) = self
            .ops
            .iter_mut()
            .find(|op| matches!(op, Op::Chunk(c) if c.idx == idx))
        {
            // In flight already (a resend after a reconnect): its credits
            // come back with the first copy's `Landed`.
            pending.extra += credits_for(len);
            self.window
                .drained(len)
                .map_err(|e| format!("re-accounting a resend: {e}"))?;
            return Ok(());
        }
        let vol_size = self
            .dst_vol
            .and_then(|v| eng.volume(v))
            .map_or(0, |v| v.size_blocks);
        match self.parse(&delta, idx, &chunk.image, vol_size) {
            Ok(pending) => {
                self.ops.push_back(Op::Chunk(pending));
                Ok(())
            }
            Err(why) => {
                let _ = self.window.drained(len);
                let meta = self.meta(0);
                if let Some(conn) = self.conn.as_mut() {
                    let _ = conn.queue_frame(
                        &meta,
                        &Frame::Abort(Abort {
                            reason: AbortReason::Corrupt,
                            idx,
                            detail: 0,
                        }),
                    );
                }
                Err(format!("chunk {idx} is corrupt: {why}"))
            }
        }
    }

    /// Verifies a chunk image and turns it into writes.
    fn parse(
        &self,
        delta: &Delta,
        idx: u32,
        image: &[u8],
        vol_size: u64,
    ) -> Result<PendingChunk, String> {
        let len = image.len();
        let mut words: Vec<u64> = vec![0u64; len.div_ceil(8)];
        words.as_mut_bytes()[..len].copy_from_slice(image);
        let id = StripeId {
            agg_id: self.src_agg,
            lseg: u64::from(idx) + 1,
            stripe_idx: idx,
            cp_seq: delta.cp,
            cp_nonce: delta.nonce,
        };
        let bytes = &words.as_bytes()[..len];
        let summary = StripeSummary::parse_expected(bytes, &id).map_err(|e| e.to_string())?;
        let descs: Vec<StripeDescriptor> = summary.descriptors().to_vec();
        let n = descs.len();
        if len != chunk_image_len(n as u32) {
            return Err(format!(
                "{len} bytes for {n} blocks, expected {}",
                chunk_image_len(n as u32)
            ));
        }
        let data_at = summary.summary_bytes();
        let mut runs: Vec<Run> = Vec::new();
        let max_run = self.cfg.max_run_blocks.clamp(1, MAX_REQUEST_BLOCKS);
        for (k, d) in descs.iter().enumerate() {
            if d.vol() != self.src_vol {
                return Err(format!("block {k} names volume {}", d.vol()));
            }
            let lba = d.lba();
            if lba >= vol_size {
                return Err(format!(
                    "block {k} at {lba} is past the mirror's {vol_size} blocks"
                ));
            }
            let at = data_at + k * BLOCK_SIZE;
            let block = &bytes[at..at + BLOCK_SIZE];
            if otap_format::checksum::crc32c(block) != d.crc32c {
                return Err(format!("block {k} at {lba} fails its crc"));
            }
            match d.kind() {
                Some(BlockKind::Data) => match runs.last_mut() {
                    Some(Run::Write {
                        lba: start, blocks, ..
                    }) if *start + u64::from(*blocks) == lba && *blocks < max_run => {
                        *blocks += 1;
                    }
                    _ => runs.push(Run::Write { lba, at, blocks: 1 }),
                },
                Some(BlockKind::Hole) => match runs.last_mut() {
                    Some(Run::Punch { lba: start, blocks })
                        if *start + u64::from(*blocks) == lba =>
                    {
                        *blocks += 1;
                    }
                    _ => runs.push(Run::Punch { lba, blocks: 1 }),
                },
                None => return Err(format!("block {k} has a reserved kind")),
            }
        }
        Ok(PendingChunk {
            idx,
            cp: delta.cp,
            nonce: delta.nonce,
            image: words,
            len,
            runs,
            next: 0,
            max_seq: 0,
            extra: 0,
        })
    }

    /// Places queued holes and chunk writes, in order, until the arena
    /// says not now.
    fn apply<B: Backstore, F: FrontDoor, C: Clock>(&mut self, eng: &mut Engine<B, F, C>) -> bool {
        let Some(vol) = self.dst_vol else {
            return false;
        };
        if !matches!(self.stage, Stage::Apply | Stage::Snapshot) || !eng.volume_ready(vol) {
            return false;
        }
        let mut budget = self.cfg.runs_per_step;
        let mut placed_any = false;
        let mut failure: Option<EngineError> = None;
        'ops: for op in &mut self.ops {
            match op {
                Op::Punch { lba, blocks, seq } => {
                    if seq.is_some() {
                        continue;
                    }
                    if budget == 0 {
                        break;
                    }
                    budget -= 1;
                    match eng.apply_punch(vol, *lba, *blocks) {
                        Ok(Some(s)) => {
                            *seq = Some(s);
                            placed_any = true;
                        }
                        Ok(None) => break,
                        Err(e) => {
                            failure = Some(e);
                            break;
                        }
                    }
                }
                Op::Chunk(c) => {
                    while c.next < c.runs.len() {
                        if budget == 0 {
                            break 'ops;
                        }
                        budget -= 1;
                        let got = match c.runs[c.next] {
                            Run::Write { lba, at, blocks } => {
                                let end = at + blocks as usize * BLOCK_SIZE;
                                let data = &c.image.as_bytes()[at..end];
                                eng.apply_write(vol, lba, data)
                            }
                            Run::Punch { lba, blocks } => eng.apply_punch(vol, lba, blocks),
                        };
                        match got {
                            Ok(Some(s)) => {
                                c.max_seq = c.max_seq.max(s);
                                c.next += 1;
                                placed_any = true;
                            }
                            Ok(None) => break 'ops,
                            Err(e) => {
                                failure = Some(e);
                                break 'ops;
                            }
                        }
                    }
                }
            }
        }
        if let Some(e) = failure {
            let now = eng.mono_ns();
            match e {
                EngineError::NoSpace => self.abort(
                    AbortReason::NoSpace,
                    0,
                    "the destination pool is full".to_string(),
                    true,
                    now,
                ),
                EngineError::NvFailed { .. } => {}
                e => self.fail(format!("landing into the mirror: {e}")),
            }
            return true;
        }
        let unflushed = self.ops.iter().any(|op| match op {
            Op::Punch { seq, .. } => seq.is_some_and(|s| s > eng.durable_seq()),
            Op::Chunk(c) => c.next > 0 && c.max_seq > eng.durable_seq(),
        });
        if unflushed {
            eng.request_cp();
        }
        placed_any
    }

    /// Lands, in order, what a committed CP made durable: the cursor and
    /// the record first, then `Landed` with the credits back.
    fn land<B: Backstore, F: FrontDoor, C: Clock>(
        &mut self,
        eng: &mut Engine<B, F, C>,
        store: &mut dyn SinkStore,
        now: u64,
    ) -> bool {
        let durable = eng.durable_seq();
        let mut out: Vec<(u32, u32)> = Vec::new();
        while let Some(front) = self.ops.front() {
            let ready = match front {
                Op::Punch { seq, .. } => seq.is_some_and(|s| s <= durable),
                Op::Chunk(c) => c.placed() && c.max_seq <= durable,
            };
            if !ready {
                break;
            }
            let Some(op) = self.ops.pop_front() else {
                break;
            };
            let Op::Chunk(c) = op else {
                continue;
            };
            let landed = match self.rec.cursor.land(c.cp, c.nonce, c.idx) {
                Ok(outcome) => outcome,
                Err(e) => {
                    self.fail(format!("landing chunk {}: {e}", c.idx));
                    return true;
                }
            };
            if landed == LandOutcome::New {
                if c.idx == self.hashed_upto {
                    self.hasher.update(c.bytes());
                    self.hashed_upto += 1;
                } else {
                    self.hash_ok = false;
                }
                self.chunks_landed += 1;
                self.bytes_landed += c.len as u64;
            } else {
                self.duplicates += 1;
            }
            let _ = self.window.drained(c.len);
            let credits = self.window.grant_up_to(credits_for(c.len) + c.extra);
            out.push((c.idx, credits));
        }
        if out.is_empty() {
            return false;
        }
        if let Err(e) = self.persist(eng, store) {
            self.fail(e);
            return true;
        }
        let contig = self
            .rec
            .cursor
            .landing
            .as_ref()
            .map_or(0, |l| l.bitmap.landed_contig());
        if self.hello_done {
            let meta = self.meta(0);
            if let Some(conn) = self.conn.as_mut() {
                for (idx, credits) in out {
                    if let Err(e) = conn.queue_frame(
                        &meta,
                        &Frame::Landed(Landed {
                            idx,
                            credits,
                            landed_contig: contig,
                        }),
                    ) {
                        self.lose(e, now);
                        break;
                    }
                }
            }
        }
        true
    }

    /// Once every chunk landed and `DeltaEnd` came: the digest, the
    /// snapshot, the commit.
    fn finish<B: Backstore, F: FrontDoor, C: Clock>(
        &mut self,
        eng: &mut Engine<B, F, C>,
        store: &mut dyn SinkStore,
        now: u64,
    ) -> bool {
        let Some(delta) = self.delta.clone() else {
            return false;
        };
        let Some(vol) = self.dst_vol else {
            return false;
        };
        let target = match &self.target {
            Some(t) => t.clone(),
            None => return false,
        };
        if self.stage == Stage::Apply {
            let Some(end) = delta.end else {
                return false;
            };
            let complete = self
                .rec
                .cursor
                .landing
                .as_ref()
                .is_some_and(|l| l.bitmap.is_complete());
            if !complete || !self.ops.is_empty() {
                return false;
            }
            if self.hash_ok && self.hashed_upto == delta.chunks {
                let ok = *self.hasher.finalize().as_bytes() == end.digest;
                self.digest_verified = Some(ok);
                if !ok {
                    // Every chunk passed its own checks, yet the whole
                    // differs: start the delta over, from a clean base.
                    self.rec.cursor.abort();
                    self.rec.landing_target = Uuid::NIL;
                    self.rec.landing_from = Uuid::NIL;
                    self.delta = None;
                    self.stage = Stage::Idle;
                    if let Err(e) = self.persist(eng, store) {
                        self.fail(e);
                        return true;
                    }
                    self.abort(
                        AbortReason::Corrupt,
                        0,
                        "the delta's digest does not match its chunks; it starts over".to_string(),
                        false,
                        now,
                    );
                    return true;
                }
            }
            self.stage = Stage::Snapshot;
            self.snap_asked = None;
        }
        if self.stage != Stage::Snapshot {
            return false;
        }
        if find_snapshot_by_uuid(eng, vol, target.uuid).is_some() {
            self.commit(eng, store, &delta, &target, now);
            return true;
        }
        let due = self
            .snap_asked
            .is_none_or(|at| now.saturating_sub(at) > self.cfg.snapshot_retry_ns);
        if eng.snapshot_pending(target.uuid) || !due {
            return false;
        }
        let name = if self.snap_unnamed {
            None
        } else {
            delta.snap_name.as_deref()
        };
        match eng.create_snapshot_as(
            vol,
            target.uuid,
            SnapKind::Crash,
            SnapOrigin::Transfer,
            name,
        ) {
            Ok(()) => self.snap_asked = Some(now),
            Err(EngineError::NameExists { .. }) => self.snap_unnamed = true,
            Err(EngineError::SnapshotExists { .. })
            | Err(EngineError::VolumeBusy { .. })
            | Err(EngineError::NvFailed { .. }) => self.snap_asked = Some(now),
            Err(e) => self.fail(format!("taking the destination snapshot: {e}")),
        }
        true
    }

    /// The destination snapshot is durable: the record moves to it, the
    /// pin follows, the old base goes, and the source hears `Committed`.
    fn commit<B: Backstore, F: FrontDoor, C: Clock>(
        &mut self,
        eng: &mut Engine<B, F, C>,
        store: &mut dyn SinkStore,
        delta: &Delta,
        target: &Target,
        now: u64,
    ) {
        let old = self.rec.base;
        if let Err(e) = self.rec.cursor.commit(delta.epoch) {
            self.fail(format!("committing the cursor: {e}"));
            return;
        }
        self.rec.base = target.uuid;
        self.rec.landing_target = Uuid::NIL;
        self.rec.landing_from = Uuid::NIL;
        self.rec.dirty = false;
        if let Err(e) = self.persist(eng, store) {
            self.fail(e);
            return;
        }
        self.fix_pins(eng, old);
        let committed = Committed {
            applied_cp_seq: delta.cp,
            applied_epoch: delta.epoch,
        };
        let meta = self.meta(0);
        if let Some(conn) = self.conn.as_mut() {
            if conn
                .queue_frame(&meta, &Frame::Committed(committed))
                .is_ok()
            {
                let _ = conn.flush(now);
            }
        }
        self.delta = None;
        self.finish_done();
    }

    /// The destination's newest transfer snapshot takes the replication
    /// pin; an older transfer snapshot that only the pin kept (`old`, or
    /// every unpinned one when `old` is nil) is deleted.
    fn fix_pins<B: Backstore, F: FrontDoor, C: Clock>(
        &mut self,
        eng: &mut Engine<B, F, C>,
        old: Uuid,
    ) {
        let Some(vol) = self.dst_vol else {
            return;
        };
        let Some(base) = newest_transfer(eng, vol) else {
            return;
        };
        if let Err(e) = eng.set_replication_base(vol, Some(base.snap_id)) {
            self.error = Some(format!("pinning the mirror's base: {e}"));
            return;
        }
        let stale: Vec<u64> = eng
            .snapshots()
            .into_iter()
            .filter(|s| {
                s.vol == vol
                    && s.origin == SnapOrigin::Transfer
                    && s.snap_id != base.snap_id
                    && !s.deleting
                    && (s.uuid == old || old.is_nil())
                    && s.flags & !otap_format::PIN_REPLICATION == 0
            })
            .map(|s| s.snap_id)
            .collect();
        for snap_id in stale {
            if let Err(e) = eng.delete_snapshot(snap_id) {
                self.error = Some(format!("deleting the mirror's old base: {e}"));
            }
        }
    }

    pub(crate) fn report(&self) -> JobReport {
        let (frames_sent, frames_received) =
            self.conn
                .as_ref()
                .map_or((self.frames_sent, self.frames_received), |c| {
                    (
                        self.frames_sent + c.frames_sent,
                        self.frames_received + c.frames_received,
                    )
                });
        let landing = self.rec.cursor.landing.as_ref();
        JobReport {
            id: self.id,
            role: Role::Sink,
            mode: self.mode,
            state: self.state,
            phase: self.stage.word().to_string(),
            volume: self.dst_name.clone(),
            volume_index: self.dst_vol,
            peer: self.addr.clone(),
            remote_volume: self.src_volume.clone(),
            snapshot: self
                .target
                .as_ref()
                .map(|t| t.name.clone())
                .filter(|n| !n.is_empty())
                .or_else(|| (!self.snapshot_name.is_empty()).then(|| self.snapshot_name.clone())),
            snapshot_uuid: self.target.as_ref().map(|t| t.uuid),
            base_uuid: (!self.rec.base.is_nil()).then_some(self.rec.base),
            chunks: self
                .delta
                .as_ref()
                .map_or_else(|| landing.map_or(0, |l| l.bitmap.chunks()), |d| d.chunks),
            chunks_sent: 0,
            chunks_landed: self.chunks_landed,
            bytes: self.delta.as_ref().map_or(0, |d| d.bytes),
            bytes_sent: 0,
            bytes_landed: self.bytes_landed,
            frames_sent,
            frames_received,
            connections: self.connections,
            retries: self.retries,
            resumed_chunks: 0,
            resent_chunks: 0,
            duplicate_chunks: self.duplicates,
            landed_contig: landing.map_or(0, |l| l.bitmap.landed_contig()),
            digest_verified: self.digest_verified,
            last_error: self.error.clone(),
        }
    }
}

const _: () = {
    assert!(MIN_STAGING_BYTES <= otap_stream::flow::MAX_STAGING_BYTES);
    assert!(MIN_STAGING_BYTES.is_multiple_of(BLOCK_SIZE as u64));
};
