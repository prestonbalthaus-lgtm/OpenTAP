//! The replication source (Step 6C): ships one snapshot of a volume to a
//! sink as the delta against the base the sink holds.
//!
//! ```text
//! handshake ──▶ Hello ──▶ HelloAck ──▶ plan ──▶ DeltaBegin, VolEvents, Holes
//!      │                                             │
//!      │ (the sink already holds the target)         ▼
//!      ▼                                  Chunk … Chunk (credits) ──▶ DeltaEnd
//!     pin ◀──────────────────────────────── Committed ◀── Landed …
//! ```
//!
//! The plan is a walk of the delta in two passes over the snapshot's tree
//! ([`Engine::snapshot_delta_scan`]): the first counts the data blocks and
//! collects the holes, so `DeltaBegin` can say how many chunks and bytes
//! come, and records where each chunk starts; the second builds chunk `i`
//! on demand from its start, so a resend after a reconnect rebuilds the
//! same image byte for byte. A chunk is a stripe image the source packs
//! (F3): a summary whose header names this delta (`cp_seq`, nonce, chunk
//! index) and whose descriptors carry each block's LBA, birth and CRC, then
//! the blocks. A delta with no data block still ships one chunk, the
//! *anchor*: block 0 of the snapshot as it reads, which the sink holds
//! already (the stream refuses a delta of no chunks).
//!
//! After a disconnect a push dials again with a growing delay; the
//! handshake tells it what base the sink holds and the `HelloAck` cursor
//! which chunks landed, and only the missing chunks are sent. The
//! replication base pin moves to the target only once the sink reports the
//! delta committed: by `Committed`, or by naming the target as its base in
//! a later handshake (the `Committed` frame was lost with a connection).

use otap_format::{
    write_summary, HoleRecord, StripeDescriptor, StripeHdr, StripeKind, Uuid, VolBitSet,
    BLOCK_SIZE, PIN_REPLICATION,
};
use otap_stream::{
    credits_for, Abort, AbortReason, Chunk, Cursor, DeltaBegin, DeltaEnd, DeltaMode, Frame,
    FrameMeta, Hello, Holes, Pong, ResumeBitmap, SenderWindow, SnapTag, VolClass, VolEvent,
    VolEventKind,
};

use super::wire::{Handshake, Refusal, RefuseCode, Role, NO_VOLUME};
use super::{
    chunk_image_len, delta_nonce, find_snapshot_by_uuid, pipe_id, volume_uuid, Backoff, Conn,
    Inbound, JobMode, JobReport, JobState, SnapRef, SourceConfig,
};
use crate::engine::{DeltaKind, Engine, PeekSource};
use crate::traits::{Backstore, Clock, Dialer, FrontDoor};

/// The most hole records one `Holes` frame carries.
const HOLES_PER_FRAME: usize = 4096;

/// Where the source's session stands.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Phase {
    /// No connection: a push waits to dial again, a served pull waits for
    /// the sink to come back.
    Wait,
    /// The handshake is sent; waiting for the sink's.
    Handshake,
    /// `Hello` is sent; waiting for `HelloAck`.
    Hello,
    /// `HelloAck` came; the plan is still being walked.
    Plan,
    /// The delta's opening frames go out next.
    Begin,
    /// Chunks go out as credits allow, then `DeltaEnd`.
    Stream,
    /// Everything is sent; waiting for `Committed`.
    Commit,
    /// The sink committed the target; the base pin is moving to it.
    Pin,
    /// The job ended.
    Finished,
}

impl Phase {
    const fn word(self) -> &'static str {
        match self {
            Self::Wait => "waiting to reconnect",
            Self::Handshake => "handshake",
            Self::Hello => "hello",
            Self::Plan => "planning",
            Self::Begin => "delta begin",
            Self::Stream => "streaming",
            Self::Commit => "awaiting commit",
            Self::Pin => "moving the base pin",
            Self::Finished => "finished",
        }
    }
}

/// The delta, walked once.
#[derive(Debug, Clone)]
struct Plan {
    /// The base it is a delta against.
    base: Uuid,
    /// Data blocks per full chunk.
    per_chunk: u32,
    /// Data blocks in the delta.
    data_blocks: u64,
    /// Chunks in the delta (1 for the anchor).
    chunks: u32,
    /// Bytes of every chunk image.
    bytes: u64,
    /// The LBA each chunk's first block is at.
    starts: Vec<u64>,
    /// Hole runs `(lba, blocks)`, ascending.
    holes: Vec<(u64, u32)>,
    /// The snapshot's size in blocks.
    size_blocks: u64,
    /// No data block: chunk 0 is the anchor.
    anchor: bool,
}

impl Plan {
    /// Data blocks in chunk `idx`.
    fn blocks_in(&self, idx: u32) -> u32 {
        if self.anchor {
            return 1;
        }
        let done = u64::from(idx) * u64::from(self.per_chunk);
        (self.data_blocks - done).min(u64::from(self.per_chunk)) as u32
    }
}

/// The first pass, in progress.
#[derive(Debug, Clone)]
struct PlanWalk {
    base: Uuid,
    base_id: Option<u64>,
    next: u64,
    data: u64,
    starts: Vec<u64>,
    holes: Vec<(u64, u32)>,
    size_blocks: u64,
}

/// What the sink holds, as its handshake said.
#[derive(Debug, Clone)]
struct Peer {
    agg: Uuid,
    vol: u32,
    volume: String,
}

/// A replication source job.
pub(crate) struct SourceJob {
    pub(crate) id: u64,
    pub(crate) mode: JobMode,
    addr: String,
    pub(crate) vol: u32,
    vol_name: String,
    dest: String,
    pub(crate) target: SnapRef,
    agg: Uuid,
    epoch: u64,
    cfg: SourceConfig,
    phase: Phase,
    conn: Option<Conn>,
    peer: Option<Peer>,
    base: Option<SnapRef>,
    plan: Option<Plan>,
    walk: Option<PlanWalk>,
    ack: Option<Cursor>,
    hasher: blake3::Hasher,
    hashed_upto: u32,
    window: SenderWindow,
    resume: Option<ResumeBitmap>,
    sent_before: Option<ResumeBitmap>,
    sent_now: Option<ResumeBitmap>,
    next_chunk: u32,
    end_sent: bool,
    backoff: Backoff,
    lost_at: u64,
    pub(crate) state: JobState,
    error: Option<String>,
    connections: u32,
    retries: u32,
    chunks_sent: u32,
    chunks_landed: u32,
    bytes_sent: u64,
    resumed: u32,
    resent: u32,
    frames_sent: u64,
    frames_received: u64,
}

impl SourceJob {
    #[allow(clippy::too_many_arguments)]
    fn new<B: Backstore, F: FrontDoor, C: Clock>(
        id: u64,
        mode: JobMode,
        eng: &Engine<B, F, C>,
        vol: u32,
        vol_name: String,
        target: SnapRef,
        addr: String,
        dest: String,
        cfg: SourceConfig,
        now: u64,
    ) -> Self {
        Self {
            id,
            mode,
            addr,
            vol,
            vol_name,
            dest,
            target,
            agg: eng.config().agg_id,
            epoch: eng.config().owner_epoch,
            backoff: Backoff::new(cfg.retry_initial_ns, cfg.retry_max_ns, now),
            cfg,
            phase: Phase::Wait,
            conn: None,
            peer: None,
            base: None,
            plan: None,
            walk: None,
            ack: None,
            hasher: blake3::Hasher::new(),
            hashed_upto: 0,
            window: SenderWindow::new(0),
            resume: None,
            sent_before: None,
            sent_now: None,
            next_chunk: 0,
            end_sent: false,
            lost_at: now,
            state: JobState::Running,
            error: None,
            connections: 0,
            retries: 0,
            chunks_sent: 0,
            chunks_landed: 0,
            bytes_sent: 0,
            resumed: 0,
            resent: 0,
            frames_sent: 0,
            frames_received: 0,
        }
    }

    /// A push: `link` is dialed to `addr`; the handshake goes out at once.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn push<B: Backstore, F: FrontDoor, C: Clock>(
        id: u64,
        eng: &Engine<B, F, C>,
        vol: u32,
        vol_name: String,
        target: SnapRef,
        addr: String,
        dest: String,
        cfg: SourceConfig,
        link: Box<dyn crate::traits::Link>,
        now: u64,
    ) -> Self {
        let mut job = Self::new(
            id,
            JobMode::Push,
            eng,
            vol,
            vol_name,
            target,
            addr,
            dest,
            cfg,
            now,
        );
        job.open(link, now);
        job
    }

    /// A pull a sink opened to us: the job answers its handshake.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn serve<B: Backstore, F: FrontDoor, C: Clock>(
        id: u64,
        eng: &Engine<B, F, C>,
        vol: u32,
        vol_name: String,
        target: SnapRef,
        peer_addr: String,
        dest: String,
        cfg: SourceConfig,
        now: u64,
    ) -> Self {
        Self::new(
            id,
            JobMode::InboundPull,
            eng,
            vol,
            vol_name,
            target,
            peer_addr,
            dest,
            cfg,
            now,
        )
    }

    /// Takes a new connection for a push: the request handshake goes first.
    fn open(&mut self, link: Box<dyn crate::traits::Link>, now: u64) {
        let mut conn = Conn::new(link, now);
        let mut h = Handshake::new(Role::Source, false, self.agg);
        h.volume_index = self.vol;
        h.snapshot = self.target.uuid;
        h.snapshot_cp = self.target.cp;
        h.volume = self.vol_name.clone();
        h.peer_volume = self.dest.clone();
        h.snapshot_name = self.target.name.clone().unwrap_or_default();
        match conn.queue_handshake(&h) {
            Ok(()) => {
                self.conn = Some(conn);
                self.phase = Phase::Handshake;
                self.connections += 1;
            }
            Err(e) => self.fail(e),
        }
    }

    /// Takes a pull's connection (a new one, or the sink coming back),
    /// answering its handshake: `Err` carries the refusal to send.
    pub(crate) fn attach<B: Backstore, F: FrontDoor, C: Clock>(
        &mut self,
        eng: &mut Engine<B, F, C>,
        mut conn: Conn,
        h: &Handshake,
        now: u64,
    ) -> Result<(), (Conn, Refusal)> {
        if h.role != Role::Sink || h.accept {
            return Err((
                conn,
                Refusal::new(
                    RefuseCode::BadRequest,
                    "a pull must come from a sink's request",
                ),
            ));
        }
        let decision = match self.decide(eng, h) {
            Ok(d) => d,
            Err(r) => return Err((conn, r)),
        };
        // The sink came back on a new connection: the old one goes (only
        // now, so a refused stranger never cuts a running transfer).
        if let Some(old) = &mut self.conn {
            self.frames_sent += old.frames_sent;
            self.frames_received += old.frames_received;
            old.close();
        }
        self.conn = None;
        let mut reply = Handshake::new(Role::Source, true, self.agg);
        reply.volume_index = self.vol;
        reply.snapshot = self.target.uuid;
        reply.snapshot_cp = self.target.cp;
        reply.volume = self.vol_name.clone();
        reply.peer_volume = h.volume.clone();
        reply.snapshot_name = self.target.name.clone().unwrap_or_default();
        if let Err(e) = conn.queue_handshake(&reply) {
            return Err((conn, Refusal::new(RefuseCode::Internal, e)));
        }
        self.connections += 1;
        self.dest = h.volume.clone();
        self.addr = conn.peer();
        self.conn = Some(conn);
        self.after_handshake(eng, decision, now);
        Ok(())
    }

    /// The base the delta is against, from the sink's handshake; the
    /// sink's identity is taken only when the answer is not a refusal.
    fn decide<B: Backstore, F: FrontDoor, C: Clock>(
        &mut self,
        eng: &Engine<B, F, C>,
        h: &Handshake,
    ) -> Result<Option<Option<SnapRef>>, Refusal> {
        let decision = self.choose_base(eng, h)?;
        self.peer = Some(Peer {
            agg: h.agg_id,
            vol: h.volume_index,
            volume: h.volume.clone(),
        });
        Ok(decision)
    }

    /// [`decide`](Self::decide) without its effect.
    fn choose_base<B: Backstore, F: FrontDoor, C: Clock>(
        &self,
        eng: &Engine<B, F, C>,
        h: &Handshake,
    ) -> Result<Option<Option<SnapRef>>, Refusal> {
        if h.agg_id == self.agg {
            return Err(Refusal::new(
                RefuseCode::Conflict,
                "the source and the destination are the same pool: a pool cannot replicate to \
                 itself",
            ));
        }
        let target = find_snapshot_by_uuid(eng, self.vol, self.target.uuid).ok_or_else(|| {
            Refusal::new(
                RefuseCode::NotFound,
                format!("the snapshot {} to ship is gone", self.target.label()),
            )
        })?;
        if target.damaged {
            return Err(Refusal::new(
                RefuseCode::Quarantined,
                format!(
                    "the snapshot {} is quarantined (damaged) and cannot be a replication source",
                    self.target.label()
                ),
            ));
        }
        if h.base.is_nil() {
            return Ok(Some(None));
        }
        if h.base == self.target.uuid {
            // The sink committed the target already: its `Committed` was
            // lost with a connection.
            return Ok(None);
        }
        let base = find_snapshot_by_uuid(eng, self.vol, h.base).ok_or_else(|| {
            Refusal::new(
                RefuseCode::Conflict,
                format!(
                    "the destination's base snapshot {} is not a snapshot of this volume (deleted, \
                     or another source); a full transfer needs a new destination volume",
                    h.base
                ),
            )
        })?;
        if base.damaged {
            return Err(Refusal::new(
                RefuseCode::Quarantined,
                format!(
                    "the destination's base snapshot {} is quarantined (damaged) here and cannot \
                     be a replication base; a full transfer needs a new destination volume",
                    SnapRef::from_info(&base).label()
                ),
            ));
        }
        if base.cp_seq >= self.target.cp {
            return Err(Refusal::new(
                RefuseCode::Conflict,
                format!(
                    "the destination already holds snapshot {}, which is not older than {}",
                    SnapRef::from_info(&base).label(),
                    self.target.label()
                ),
            ));
        }
        Ok(Some(Some(SnapRef::from_info(&base))))
    }

    /// Goes on once the base is decided: `None` means the sink holds the
    /// target already.
    fn after_handshake<B: Backstore, F: FrontDoor, C: Clock>(
        &mut self,
        eng: &mut Engine<B, F, C>,
        decision: Option<Option<SnapRef>>,
        now: u64,
    ) {
        let Some(base) = decision else {
            self.request_pin(eng);
            return;
        };
        let base_uuid = base.as_ref().map_or(Uuid::NIL, |b| b.uuid);
        if self.plan.as_ref().is_some_and(|p| p.base != base_uuid)
            || self.walk.as_ref().is_some_and(|w| w.base != base_uuid)
        {
            self.plan = None;
            self.walk = None;
            self.hasher = blake3::Hasher::new();
            self.hashed_upto = 0;
            self.sent_before = None;
        }
        if self.plan.is_none() && self.walk.is_none() {
            self.walk = Some(PlanWalk {
                base: base_uuid,
                base_id: base.as_ref().map(|b| b.snap_id),
                next: 0,
                data: 0,
                starts: Vec::new(),
                holes: Vec::new(),
                size_blocks: 0,
            });
        }
        self.base = base;
        let hello = Hello {
            node_id: self.agg,
            pipe_id: self.pipe(),
            direction_epoch: 1,
            capabilities: 0,
            max_payload: otap_format::MAX_WIRE_PAYLOAD,
            initial_credits: 0,
        };
        let meta = self.meta(0);
        match self.conn.as_mut() {
            Some(conn) => {
                if let Err(e) = conn.queue_frame(&meta, &Frame::Hello(hello)) {
                    self.fail(e);
                    return;
                }
            }
            None => return,
        }
        self.ack = None;
        self.phase = Phase::Hello;
        self.backoff.progress(now);
    }

    fn pipe(&self) -> Uuid {
        let (agg, vol) = self
            .peer
            .as_ref()
            .map_or((Uuid::NIL, NO_VOLUME), |p| (p.agg, p.vol));
        pipe_id(self.agg, self.vol, agg, vol)
    }

    fn nonce(&self) -> u64 {
        delta_nonce(
            self.target.uuid,
            self.base.as_ref().map_or(Uuid::NIL, |b| b.uuid),
        )
    }

    fn meta(&self, chunk_idx: u32) -> FrameMeta {
        FrameMeta {
            rel: 0,
            chunk_idx,
            cp_seq: self.target.cp,
            base: self.base.as_ref().map_or(0, |b| b.cp),
            cp_nonce: self.nonce(),
            owner_epoch: self.epoch,
        }
    }

    pub(crate) fn fail(&mut self, why: impl Into<String>) {
        if let Some(conn) = &mut self.conn {
            conn.close();
        }
        self.conn = None;
        self.error = Some(why.into());
        self.state = JobState::Failed;
        self.phase = Phase::Finished;
    }

    pub(crate) fn cancel(&mut self) {
        if let Some(conn) = &mut self.conn {
            conn.close();
        }
        self.conn = None;
        self.state = JobState::Cancelled;
        self.phase = Phase::Finished;
        self.error = Some("cancelled by the operator".to_string());
    }

    /// The connection is gone: a push dials again after the delay (up to
    /// the bound of consecutive failures), a served pull waits for the sink.
    fn lose(&mut self, why: String, now: u64) {
        if let Some(conn) = &mut self.conn {
            self.frames_sent += conn.frames_sent;
            self.frames_received += conn.frames_received;
            conn.close();
        }
        self.conn = None;
        if let Some(sent) = self.sent_now.take() {
            self.sent_before = Some(match self.sent_before.take() {
                Some(mut before) => {
                    for idx in 0..sent.chunks() {
                        if sent.is_landed(idx) {
                            let _ = before.mark(idx);
                        }
                    }
                    before
                }
                None => sent,
            });
        }
        self.error = Some(why.clone());
        if matches!(self.phase, Phase::Pin | Phase::Finished) {
            return;
        }
        self.phase = Phase::Wait;
        self.lost_at = now;
        if self.mode == JobMode::Push {
            self.retries += 1;
            if !self.backoff.failed(now, self.cfg.max_retries) {
                self.fail(format!(
                    "gave up after {} consecutive failed connections; last: {why}",
                    self.cfg.max_retries
                ));
            }
        }
    }

    /// One step: connects, reads, plans, sends; `true` when it did work.
    pub(crate) fn step<B: Backstore, F: FrontDoor, C: Clock>(
        &mut self,
        eng: &mut Engine<B, F, C>,
        dialer: &mut dyn Dialer,
        now: u64,
    ) -> bool {
        if self.state != JobState::Running {
            return false;
        }
        if self.phase == Phase::Pin {
            return self.step_pin(eng);
        }
        let mut busy = false;
        if self.conn.is_none() {
            match self.mode {
                JobMode::Push => {
                    if now < self.backoff.next_at {
                        return false;
                    }
                    match dialer.dial(&self.addr) {
                        Ok(link) => self.open(link, now),
                        Err(e) => {
                            self.lose(e.to_string(), now);
                            return true;
                        }
                    }
                    busy = true;
                }
                _ => {
                    if now.saturating_sub(self.lost_at) > self.cfg.linger_ns {
                        self.fail(format!(
                            "the sink did not come back within {} s",
                            self.cfg.linger_ns / 1_000_000_000
                        ));
                    }
                    return false;
                }
            }
        }
        if self.walk.is_some() {
            busy |= self.step_walk(eng);
            if self.state != JobState::Running {
                return true;
            }
        }
        for _ in 0..self.cfg.frames_per_step {
            let Some(conn) = self.conn.as_mut() else {
                return true;
            };
            match conn.recv(now) {
                Ok(Some(msg)) => {
                    busy = true;
                    if let Err(e) = self.on_message(eng, msg, now) {
                        self.lose(e, now);
                        return true;
                    }
                    if self.state != JobState::Running || self.phase == Phase::Pin {
                        return true;
                    }
                }
                Ok(None) => break,
                Err(e) => {
                    self.lose(e, now);
                    return true;
                }
            }
        }
        if self.phase == Phase::Plan && self.plan.is_some() {
            self.setup_resume();
            self.phase = Phase::Begin;
        }
        if self.phase == Phase::Begin {
            busy = true;
            if let Err(e) = self.send_begin() {
                self.fail(e);
                return true;
            }
        }
        if self.phase == Phase::Stream {
            match self.send_chunks(eng) {
                Ok(did) => busy |= did,
                Err(e) => {
                    self.fail(e);
                    return true;
                }
            }
        }
        let Some(conn) = self.conn.as_mut() else {
            return busy;
        };
        if let Err(e) = conn.flush(now) {
            self.lose(e, now);
            return true;
        }
        if now.saturating_sub(conn.last_recv_ns) > self.cfg.idle_timeout_ns {
            let secs = self.cfg.idle_timeout_ns / 1_000_000_000;
            self.lose(format!("nothing from the sink for {secs} s"), now);
            return true;
        }
        if self.phase as u8 >= Phase::Hello as u8
            && now.saturating_sub(conn.last_send_ns) > self.cfg.keepalive_ns
        {
            let meta = self.meta(0);
            if let Some(conn) = self.conn.as_mut() {
                let ping = otap_stream::Ping {
                    nonce: now,
                    mono_ns: now,
                };
                if let Err(e) = conn.queue_frame(&meta, &Frame::Ping(ping)) {
                    self.fail(e);
                }
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
            Inbound::Refuse(r) => {
                if r.code == RefuseCode::Busy {
                    return Err(r.to_string());
                }
                self.fail(r.to_string());
                Ok(())
            }
            Inbound::Version(v) => {
                self.fail(format!(
                    "the sink speaks replication protocol {v}; this side speaks {}",
                    super::wire::REPL_PROTOCOL_VERSION
                ));
                Ok(())
            }
            Inbound::Handshake(h) => {
                if self.phase != Phase::Handshake {
                    return Err("an unexpected handshake from the sink".to_string());
                }
                if h.role != Role::Sink || !h.accept {
                    self.fail("the peer answered the handshake as something other than a sink");
                    return Ok(());
                }
                match self.decide(eng, &h) {
                    Ok(d) => self.after_handshake(eng, d, now),
                    Err(r) => self.fail(r.text),
                }
                Ok(())
            }
            Inbound::Corrupt { kind, idx } => Err(format!(
                "a frame of kind {kind} (chunk {idx}) from the sink failed its payload crc"
            )),
            Inbound::Frame(_meta, frame) => self.on_frame(eng, frame, now),
        }
    }

    fn on_frame<B: Backstore, F: FrontDoor, C: Clock>(
        &mut self,
        eng: &mut Engine<B, F, C>,
        frame: Frame,
        now: u64,
    ) -> Result<(), String> {
        match frame {
            Frame::HelloAck(ack) => {
                if self.phase != Phase::Hello {
                    return Err("an unexpected HelloAck".to_string());
                }
                self.window = SenderWindow::new(ack.cursor.credits);
                self.ack = Some(ack.cursor);
                if self.plan.is_some() {
                    self.setup_resume();
                    self.phase = Phase::Begin;
                } else {
                    self.phase = Phase::Plan;
                }
                Ok(())
            }
            Frame::Landed(l) => {
                self.window.grant(l.credits);
                self.chunks_landed = self.chunks_landed.saturating_add(1);
                self.backoff.progress(now);
                Ok(())
            }
            Frame::Committed(c) => {
                if c.applied_cp_seq != self.target.cp {
                    return Err(format!(
                        "the sink committed cp {}, the delta ends at {}",
                        c.applied_cp_seq, self.target.cp
                    ));
                }
                self.request_pin(eng);
                Ok(())
            }
            Frame::Abort(Abort { reason, idx, .. }) => match reason {
                AbortReason::Corrupt => Err(format!(
                    "the sink found chunk {idx} corrupt; the delta resumes from its cursor"
                )),
                AbortReason::NoSpace => {
                    self.fail("the destination pool has no space for the delta");
                    Ok(())
                }
                AbortReason::BaseMismatch => {
                    self.fail("the destination's base is not the delta's base");
                    Ok(())
                }
                AbortReason::UnknownVol => {
                    self.fail("the destination does not know the delta's volume");
                    Ok(())
                }
                AbortReason::Version | AbortReason::StaleEpoch => {
                    self.fail(format!("the sink aborted the delta ({reason:?})"));
                    Ok(())
                }
            },
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
            other => Err(format!("a {:?} frame from the sink", other.kind())),
        }
    }

    /// The resume point from the `HelloAck` cursor: the chunks of this
    /// very delta (same CP, same nonce) that landed are not sent again.
    fn setup_resume(&mut self) {
        let Some(plan) = &self.plan else {
            return;
        };
        self.resume = None;
        if let Some((cp, nonce, contig, ranges)) = self.ack.as_ref().and_then(|c| c.resume_point())
        {
            if cp == self.target.cp && nonce == self.nonce() {
                if let Ok(bitmap) = ResumeBitmap::from_parts(plan.chunks, contig, &ranges) {
                    self.resume = Some(bitmap);
                }
            }
        }
        self.sent_now = ResumeBitmap::new(plan.chunks).ok();
        self.next_chunk = 0;
        self.end_sent = false;
    }

    /// One slice of the first pass over the delta.
    fn step_walk<B: Backstore, F: FrontDoor, C: Clock>(
        &mut self,
        eng: &mut Engine<B, F, C>,
    ) -> bool {
        let Some(mut walk) = self.walk.take() else {
            return false;
        };
        let per_chunk = self.per_chunk();
        let scan = match eng.snapshot_delta_scan(
            self.target.snap_id,
            walk.base_id,
            walk.next,
            usize::MAX,
            self.cfg.pages_per_step,
        ) {
            Ok(scan) => scan,
            Err(e) => {
                self.fail(format!("walking the delta of {}: {e}", self.target.label()));
                return true;
            }
        };
        walk.size_blocks = scan.size_blocks;
        for item in scan.items {
            match item.kind {
                DeltaKind::Data { .. } => {
                    if walk.data.is_multiple_of(u64::from(per_chunk)) {
                        walk.starts.push(item.lba);
                    }
                    walk.data += 1;
                }
                DeltaKind::Hole => match walk.holes.last_mut() {
                    Some((lba, len)) if *lba + u64::from(*len) == item.lba && *len < u32::MAX => {
                        *len += 1;
                    }
                    _ => walk.holes.push((item.lba, 1)),
                },
            }
        }
        match scan.next_lba {
            Some(next) => {
                walk.next = next;
                self.walk = Some(walk);
            }
            None => {
                let anchor = walk.data == 0;
                let chunks = if anchor {
                    1
                } else {
                    walk.data.div_ceil(u64::from(per_chunk))
                };
                if chunks > u64::from(otap_stream::MAX_DELTA_CHUNKS) {
                    self.fail(format!(
                        "the delta of {} blocks needs more than {} chunks; use a larger chunk",
                        walk.data,
                        otap_stream::MAX_DELTA_CHUNKS
                    ));
                    return true;
                }
                let chunks = chunks as u32;
                let mut plan = Plan {
                    base: walk.base,
                    per_chunk,
                    data_blocks: walk.data,
                    chunks,
                    bytes: 0,
                    starts: walk.starts,
                    holes: walk.holes,
                    size_blocks: walk.size_blocks,
                    anchor,
                };
                plan.bytes = (0..chunks)
                    .map(|i| chunk_image_len(plan.blocks_in(i)) as u64)
                    .sum();
                self.plan = Some(plan);
            }
        }
        true
    }

    fn per_chunk(&self) -> u32 {
        otap_format::SummaryLayout::for_stripe(self.cfg.chunk_bytes as usize)
            .map_or(1, |l| l.nblocks.max(1) as u32)
    }

    /// `DeltaBegin`, the volume events and the holes.
    fn send_begin(&mut self) -> Result<(), String> {
        let plan = self
            .plan
            .clone()
            .ok_or_else(|| "the delta has no plan".to_string())?;
        let meta = self.meta(0);
        let mode = match &self.base {
            None => DeltaMode::Steady,
            Some(b) => DeltaMode::Increment {
                from: b.uuid,
                to: self.target.uuid,
            },
        };
        let begin = DeltaBegin {
            mode,
            chunks: plan.chunks,
            bytes: plan.bytes,
            cut_age_ms: 0,
            write_active: VolBitSet::EMPTY,
            tags: vec![SnapTag {
                uuid: self.target.uuid,
                retention_s: 0,
                rsvd: 0,
            }],
            acked: Vec::new(),
        };
        let mut frames = vec![Frame::DeltaBegin(begin)];
        let name = |s: &str| -> String {
            let mut s = s.to_string();
            while s.len() > otap_stream::frame::MAX_NAME_LEN {
                s.pop();
            }
            s
        };
        if self.base.is_none() {
            frames.push(Frame::VolEvent(VolEvent {
                event: VolEventKind::Create,
                vol: self.vol,
                class: Some(VolClass::DatastoreLun),
                size_blocks: plan.size_blocks,
                stripe_bytes: self.cfg.chunk_bytes,
                retention_s: 0,
                uuid: volume_uuid(self.agg, self.vol),
                name: name(&self.vol_name),
            }));
        } else {
            frames.push(Frame::VolEvent(VolEvent {
                event: VolEventKind::Resize,
                vol: self.vol,
                class: None,
                size_blocks: plan.size_blocks,
                stripe_bytes: 0,
                retention_s: 0,
                uuid: Uuid::NIL,
                name: String::new(),
            }));
        }
        frames.push(Frame::VolEvent(VolEvent {
            event: VolEventKind::SnapTag,
            vol: self.vol,
            class: None,
            size_blocks: 0,
            stripe_bytes: 0,
            retention_s: 0,
            uuid: self.target.uuid,
            name: name(self.target.name.as_deref().unwrap_or("")),
        }));
        for batch in plan.holes.chunks(HOLES_PER_FRAME) {
            let records = batch
                .iter()
                .map(|(lba, len)| HoleRecord::new(self.vol, *lba, *len))
                .collect::<Result<Vec<_>, _>>()
                .map_err(|e| format!("a hole record: {e}"))?;
            frames.push(Frame::Holes(Holes { records }));
        }
        let conn = self
            .conn
            .as_mut()
            .ok_or_else(|| "no connection".to_string())?;
        for frame in &frames {
            conn.queue_frame(&meta, frame)?;
        }
        self.phase = Phase::Stream;
        Ok(())
    }

    /// Sends what the credits allow, then `DeltaEnd`.
    fn send_chunks<B: Backstore, F: FrontDoor, C: Clock>(
        &mut self,
        eng: &mut Engine<B, F, C>,
    ) -> Result<bool, String> {
        let Some(plan) = self.plan.clone() else {
            return Ok(false);
        };
        let mut did = false;
        for _ in 0..self.cfg.chunks_per_step {
            if self.conn.as_ref().is_none_or(|c| c.backlog() > 2) {
                break;
            }
            while self.next_chunk < plan.chunks
                && self
                    .resume
                    .as_ref()
                    .is_some_and(|r| r.is_landed(self.next_chunk))
            {
                self.next_chunk += 1;
                self.resumed += 1;
            }
            let idx = (self.next_chunk < plan.chunks).then_some(self.next_chunk);
            // The digest covers every chunk image in index order: the ones
            // a resume skipped, landed before this process started, are
            // rebuilt to be hashed (never sent), one per unit of the step's
            // budget, so a long catch-up never holds the engine thread.
            if self.hashed_upto < idx.unwrap_or(plan.chunks) && !self.end_sent {
                let image = self.build_image(eng, &plan, self.hashed_upto)?;
                self.hasher.update(&image);
                self.hashed_upto += 1;
                did = true;
                continue;
            }
            let Some(idx) = idx else {
                if self.end_sent {
                    break;
                }
                let end = DeltaEnd {
                    digest: *self.hasher.finalize().as_bytes(),
                    chunks: plan.chunks,
                    bytes: plan.bytes,
                };
                let meta = self.meta(0);
                if let Some(conn) = self.conn.as_mut() {
                    conn.queue_frame(&meta, &Frame::DeltaEnd(end))?;
                }
                self.end_sent = true;
                self.phase = Phase::Commit;
                did = true;
                break;
            };
            let len = chunk_image_len(plan.blocks_in(idx));
            if !self.window.can_send(len) {
                if self.window.available() < credits_for(len)
                    && self.chunks_landed == 0
                    && self
                        .ack
                        .as_ref()
                        .is_some_and(|a| a.credits < credits_for(len))
                {
                    return Err(format!(
                        "the sink grants {} credits and a chunk needs {}; give the sink more \
                         staging or use smaller chunks",
                        self.ack.as_ref().map_or(0, |a| a.credits),
                        credits_for(len)
                    ));
                }
                break;
            }
            let image = self.build_image(eng, &plan, idx)?;
            if self.hashed_upto == idx {
                self.hasher.update(&image);
                self.hashed_upto += 1;
            }
            let chunk =
                Chunk::raw(bytes::Bytes::from(image)).map_err(|e| format!("chunk {idx}: {e}"))?;
            self.window
                .reserve(len)
                .map_err(|e| format!("chunk {idx}: {e}"))?;
            let meta = self.meta(idx);
            if let Some(conn) = self.conn.as_mut() {
                conn.queue_frame(&meta, &Frame::Chunk(chunk))?;
            }
            self.next_chunk += 1;
            if self.sent_before.as_ref().is_some_and(|s| s.is_landed(idx)) {
                self.resent += 1;
            }
            if let Some(sent) = &mut self.sent_now {
                let _ = sent.mark(idx);
            }
            self.chunks_sent += 1;
            self.bytes_sent += len as u64;
            did = true;
        }
        Ok(did)
    }

    /// The image of chunk `idx`, built from the snapshot as it reads now.
    fn build_image<B: Backstore, F: FrontDoor, C: Clock>(
        &self,
        eng: &mut Engine<B, F, C>,
        plan: &Plan,
        idx: u32,
    ) -> Result<Vec<u8>, String> {
        let n = plan.blocks_in(idx) as usize;
        let snap = self.target.snap_id;
        let mut lbas = Vec::with_capacity(n);
        let mut births = Vec::with_capacity(n);
        if plan.anchor {
            lbas.push(0);
        } else {
            let mut from = *plan
                .starts
                .get(idx as usize)
                .ok_or_else(|| format!("chunk {idx} has no start"))?;
            let base_id = self.base.as_ref().map(|b| b.snap_id);
            while lbas.len() < n {
                let scan = eng
                    .snapshot_delta_scan(snap, base_id, from, n - lbas.len(), 4096)
                    .map_err(|e| format!("chunk {idx} of {}: {e}", self.target.label()))?;
                for item in &scan.items {
                    if let DeltaKind::Data { birth } = item.kind {
                        lbas.push(item.lba);
                        births.push(birth);
                    }
                }
                match scan.next_lba {
                    Some(next) => from = next,
                    None => break,
                }
            }
            if lbas.len() != n {
                return Err(format!(
                    "chunk {idx} of {} holds {} blocks, the plan says {n}: the snapshot changed \
                     under the transfer",
                    self.target.label(),
                    lbas.len()
                ));
            }
        }
        let mut data = vec![0u8; n * BLOCK_SIZE];
        let infos = eng
            .snapshot_read_blocks(snap, &lbas, &mut data)
            .map_err(|e| format!("chunk {idx} of {}: {e}", self.target.label()))?;
        let mut descs = Vec::with_capacity(n);
        for (k, lba) in lbas.iter().enumerate() {
            let block = &data[k * BLOCK_SIZE..(k + 1) * BLOCK_SIZE];
            let desc = if plan.anchor && infos[k].source != PeekSource::Disk {
                let birth = if infos[k].birth > 0 {
                    infos[k].birth
                } else {
                    self.target.cp as u32
                };
                StripeDescriptor::hole(self.vol, *lba, birth)
            } else {
                let birth = if plan.anchor {
                    infos[k].birth
                } else {
                    births[k]
                };
                StripeDescriptor::data(self.vol, *lba, birth, otap_format::checksum::crc32c(block))
            }
            .map_err(|e| format!("chunk {idx} block {lba}: {e}"))?;
            descs.push(desc);
        }
        let mut hdr = StripeHdr::new(self.agg, StripeKind::Ingest);
        hdr.cp_seq = self.target.cp;
        hdr.cp_nonce = self.nonce();
        hdr.lseg = u64::from(idx) + 1;
        hdr.stripe_idx = idx;
        hdr.nblocks = n as u16;
        let mut image = vec![0u8; chunk_image_len(n as u32)];
        let summary = write_summary(&mut image, &hdr, &descs)
            .map_err(|e| format!("chunk {idx} summary: {e}"))?;
        image[summary..summary + data.len()].copy_from_slice(&data);
        Ok(image)
    }

    /// The sink committed the target: the base pin moves to it.
    fn request_pin<B: Backstore, F: FrontDoor, C: Clock>(&mut self, eng: &mut Engine<B, F, C>) {
        if let Some(conn) = &mut self.conn {
            self.frames_sent += conn.frames_sent;
            self.frames_received += conn.frames_received;
            conn.close();
        }
        self.conn = None;
        let Some(info) = find_snapshot_by_uuid(eng, self.vol, self.target.uuid) else {
            self.fail(format!(
                "the destination committed {}, which is gone here: the base pin cannot move",
                self.target.label()
            ));
            return;
        };
        match eng.set_replication_base(self.vol, Some(info.snap_id)) {
            Ok(()) => {
                self.phase = Phase::Pin;
                self.error = None;
            }
            Err(e) => self.fail(format!("moving the replication base: {e}")),
        }
    }

    fn step_pin<B: Backstore, F: FrontDoor, C: Clock>(
        &mut self,
        eng: &mut Engine<B, F, C>,
    ) -> bool {
        match find_snapshot_by_uuid(eng, self.vol, self.target.uuid) {
            Some(info) if info.flags & PIN_REPLICATION != 0 => {
                self.state = JobState::Done;
                self.phase = Phase::Finished;
                true
            }
            Some(_) => false,
            None => {
                self.fail(format!(
                    "{} vanished before the base pin moved to it",
                    self.target.label()
                ));
                true
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
        JobReport {
            id: self.id,
            role: Role::Source,
            mode: self.mode,
            state: self.state,
            phase: self.phase.word().to_string(),
            volume: self.vol_name.clone(),
            volume_index: Some(self.vol),
            peer: self.addr.clone(),
            remote_volume: self
                .peer
                .as_ref()
                .map_or_else(|| self.dest.clone(), |p| p.volume.clone()),
            snapshot: self.target.name.clone(),
            snapshot_uuid: Some(self.target.uuid),
            base_uuid: self.base.as_ref().map(|b| b.uuid),
            chunks: self.plan.as_ref().map_or(0, |p| p.chunks),
            chunks_sent: self.chunks_sent,
            chunks_landed: self.chunks_landed,
            bytes: self.plan.as_ref().map_or(0, |p| p.bytes),
            bytes_sent: self.bytes_sent,
            bytes_landed: 0,
            frames_sent,
            frames_received,
            connections: self.connections,
            retries: self.retries,
            resumed_chunks: self.resumed,
            resent_chunks: self.resent,
            duplicate_chunks: 0,
            landed_contig: self.resume.as_ref().map_or(0, ResumeBitmap::landed_contig),
            digest_verified: None,
            last_error: self.error.clone(),
        }
    }
}
