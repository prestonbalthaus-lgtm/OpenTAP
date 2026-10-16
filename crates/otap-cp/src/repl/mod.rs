//! Replication (Step 6C): incremental snapshot deltas shipped from a
//! source pool to a sink pool over a [`Link`], with resume.
//!
//! # Frame flow
//!
//! ```text
//! source                                   sink
//!   Handshake ─────────────────────────────▶      (push: the source dials)
//!             ◀──────────────────────────── Handshake (its committed base)
//!   Hello ─────────────────────────────────▶
//!             ◀──────────────────────────── HelloAck (cursor: landed chunks, credits)
//!   DeltaBegin, VolEvent…, Holes… ─────────▶
//!   Chunk(i) … (as credits allow) ─────────▶
//!             ◀──────────────────────────── Landed(i, credits) once chunk i is durable
//!   DeltaEnd ──────────────────────────────▶
//!             ◀──────────────────────────── Committed (the destination snapshot is durable)
//!   (the replication base pin moves to the target)
//! ```
//!
//! A pull is the same exchange with the sink dialing and speaking first.
//! The source half is [`source`], the sink half [`sink`], the link framing
//! and the handshake [`wire`], the sink's durable record [`record`]. The
//! [`Replicator`] owns the jobs of one pool, routes the connections a
//! listener accepted, and is stepped between engine ticks by its host
//! (`dbladed`'s engine thread, or a simulation), so a transfer of any size
//! never holds the data path for longer than one bounded step.
//!
//! # The rules
//!
//! - A delta carries the blocks of the target snapshot that its base does
//!   not hold ([`Engine::snapshot_delta_scan`]); a full transfer has no
//!   base. A quarantined snapshot (damaged, or `SNAP_UNCERTAIN`) is
//!   refused as a source and as a base.
//! - A chunk lands when the destination's consistency point that carries
//!   its blocks commits; the sink's cursor (`ResumeBitmap`) and its record
//!   say so only then, so a resumed delta never re-sends a landed chunk and
//!   never skips one that was lost.
//! - The destination volume is offline and marked as a replica while it
//!   is a mirror; its snapshot of the target, under the source snapshot's
//!   identity, is taken only once every chunk landed, so no partial delta
//!   is ever visible as a snapshot.
//! - The source moves its replication base pin (`PIN_REPLICATION`) to the
//!   target only when the sink reports the delta committed.

pub mod record;
pub mod sink;
pub mod source;
pub mod wire;

use std::collections::VecDeque;
use std::fmt;

use bytes::Bytes;
use otap_format::{summary_bytes, validate_name, SnapOrigin, Uuid, BLOCK_SIZE, PIN_REPLICATION};
use otap_stream::{Decoder, Encoder, Frame, FrameMeta, StreamError};

pub use record::SinkRecord;
pub use wire::{
    encode_frame, FrameReader, Handshake, Message, Refusal, RefuseCode, Role, WireError,
    LINK_HEADER_BYTES, LINK_MAGIC, MAX_LINK_PAYLOAD, NO_VOLUME, REPL_PROTOCOL_VERSION,
};

use crate::engine::{Engine, EngineError, SnapshotInfo};
use crate::traits::{Backstore, Clock, Dialer, FrontDoor, Link, LinkError, SinkStore};
use sink::SinkJob;
use source::SourceJob;

const SECOND: u64 = 1_000_000_000;

/// How a source job runs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SourceConfig {
    /// Bytes of a full chunk image: a stripe size, 8 KiB to 8 MiB.
    pub chunk_bytes: u32,
    /// Chunks built and queued per step at most.
    pub chunks_per_step: u32,
    /// Messages read per step at most.
    pub frames_per_step: u32,
    /// Map pages the delta's first pass reads per step at most.
    pub pages_per_step: usize,
    /// A `Ping` after this long without sending.
    pub keepalive_ns: u64,
    /// The connection is dropped after this long without a message.
    pub idle_timeout_ns: u64,
    /// The first delay before dialing again.
    pub retry_initial_ns: u64,
    /// The longest delay between dials.
    pub retry_max_ns: u64,
    /// Consecutive failed connections (without progress) before a push
    /// gives up.
    pub max_retries: u32,
    /// How long a served pull waits for its sink to come back.
    pub linger_ns: u64,
}

impl Default for SourceConfig {
    fn default() -> Self {
        Self {
            chunk_bytes: 1024 * 1024,
            chunks_per_step: 8,
            frames_per_step: 256,
            pages_per_step: 256,
            keepalive_ns: 5 * SECOND,
            idle_timeout_ns: 30 * SECOND,
            retry_initial_ns: SECOND / 10,
            retry_max_ns: 5 * SECOND,
            max_retries: 20,
            linger_ns: 600 * SECOND,
        }
    }
}

/// How a sink job runs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SinkConfig {
    /// Staging the sink offers in credits: received chunks waiting to be
    /// placed or to become durable. At least one chunk of the source.
    pub staging_bytes: u64,
    /// Messages read per step at most.
    pub frames_per_step: u32,
    /// Writes placed per step at most.
    pub runs_per_step: u32,
    /// The longest write one replication write places, in blocks.
    pub max_run_blocks: u32,
    /// Frames held while the destination is being prepared (created,
    /// resized, rolled back) at most.
    pub inbox_frames: usize,
    /// A `Ping` after this long without sending.
    pub keepalive_ns: u64,
    /// The connection is dropped after this long without a message.
    pub idle_timeout_ns: u64,
    /// The first delay before a pull dials again.
    pub retry_initial_ns: u64,
    /// The longest delay between dials.
    pub retry_max_ns: u64,
    /// Consecutive failed connections before a pull gives up.
    pub max_retries: u32,
    /// How long an inbound transfer waits for its source to come back.
    pub linger_ns: u64,
    /// How long between two requests for the destination snapshot when the
    /// first one was dropped (a fold that did not fit).
    pub snapshot_retry_ns: u64,
}

impl Default for SinkConfig {
    fn default() -> Self {
        Self {
            staging_bytes: 64 * 1024 * 1024,
            frames_per_step: 256,
            runs_per_step: 256,
            max_run_blocks: 64,
            inbox_frames: 1024,
            keepalive_ns: 5 * SECOND,
            idle_timeout_ns: 30 * SECOND,
            retry_initial_ns: SECOND / 10,
            retry_max_ns: 5 * SECOND,
            max_retries: 20,
            linger_ns: 600 * SECOND,
            snapshot_retry_ns: SECOND,
        }
    }
}

/// How a pool's replication runs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReplConfig {
    /// The source jobs.
    pub source: SourceConfig,
    /// The sink jobs.
    pub sink: SinkConfig,
    /// An accepted connection that says nothing for this long is dropped.
    pub handshake_timeout_ns: u64,
    /// Finished jobs kept for `status`.
    pub keep_finished: usize,
}

impl Default for ReplConfig {
    fn default() -> Self {
        Self {
            source: SourceConfig::default(),
            sink: SinkConfig::default(),
            handshake_timeout_ns: 30 * SECOND,
            keep_finished: 16,
        }
    }
}

/// Where a job stands.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum JobState {
    /// In progress.
    Running,
    /// The delta committed (the source's base pin moved, or the sink's
    /// snapshot is durable).
    Done,
    /// Gave up; the report's error says why.
    Failed,
    /// Cancelled by the operator.
    Cancelled,
}

/// How a job came to be.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum JobMode {
    /// This pool pushes a snapshot to a sink it dialed.
    Push,
    /// This pool pulls a snapshot from a source it dialed.
    Pull,
    /// A source pushes to this pool.
    InboundPush,
    /// A sink pulls from this pool.
    InboundPull,
}

impl JobMode {
    /// The mode as a word.
    #[must_use]
    pub const fn word(self) -> &'static str {
        match self {
            Self::Push => "push",
            Self::Pull => "pull",
            Self::InboundPush => "inbound push",
            Self::InboundPull => "inbound pull",
        }
    }
}

/// A job as `status` reports it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JobReport {
    /// The job's number.
    pub id: u64,
    /// This pool's end.
    pub role: Role,
    /// How it came to be.
    pub mode: JobMode,
    /// Where it stands.
    pub state: JobState,
    /// What it is doing, as words.
    pub phase: String,
    /// This pool's volume.
    pub volume: String,
    /// Its index, once it exists.
    pub volume_index: Option<u32>,
    /// The peer's address.
    pub peer: String,
    /// The peer's volume.
    pub remote_volume: String,
    /// The snapshot shipped, by name.
    pub snapshot: Option<String>,
    /// The snapshot shipped.
    pub snapshot_uuid: Option<Uuid>,
    /// The base of the delta.
    pub base_uuid: Option<Uuid>,
    /// Chunks in the delta (0 until known).
    pub chunks: u32,
    /// Chunks sent (source).
    pub chunks_sent: u32,
    /// Chunks landed: acknowledged (source) or durable (sink).
    pub chunks_landed: u32,
    /// Bytes of every chunk image.
    pub bytes: u64,
    /// Chunk bytes sent (source).
    pub bytes_sent: u64,
    /// Chunk bytes landed (sink).
    pub bytes_landed: u64,
    /// Messages sent over every connection.
    pub frames_sent: u64,
    /// Messages received over every connection.
    pub frames_received: u64,
    /// Connections made or taken.
    pub connections: u32,
    /// Connections lost and made again.
    pub retries: u32,
    /// Chunks a resume skipped because the sink had landed them.
    pub resumed_chunks: u32,
    /// Chunks sent again after a reconnect (they were in flight and lost).
    pub resent_chunks: u32,
    /// Chunks the sink received although they had landed (0 when the
    /// resume works).
    pub duplicate_chunks: u32,
    /// The sink's first chunk not landed.
    pub landed_contig: u32,
    /// Whether the sink verified the delta's digest: `None` when it could
    /// not (it restarted in the middle of the delta) or has not yet.
    pub digest_verified: Option<bool>,
    /// The last error met (the reason of a failure, or of the last lost
    /// connection).
    pub last_error: Option<String>,
}

/// The delta in flight on a sink, as its cursor says.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LandingReport {
    /// The CP (source numbering) it ends at.
    pub cp_seq: u64,
    /// Its nonce.
    pub nonce: u64,
    /// The snapshot it ends at.
    pub target: Uuid,
    /// Chunks in it.
    pub chunks: u32,
    /// Chunks landed.
    pub landed: u32,
    /// The first chunk not landed.
    pub landed_contig: u32,
    /// Landed ranges above `landed_contig`.
    pub sparse_ranges: u32,
}

/// The durable replication position of a volume.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CursorReport {
    /// The volume.
    pub volume: String,
    /// Its index.
    pub volume_index: u32,
    /// Its end of the relationship: a sink (a mirror) or a source.
    pub role: Role,
    /// Marked as a replica (offline to clients).
    pub replica: bool,
    /// The committed base: the snapshot both ends hold.
    pub base_uuid: Option<Uuid>,
    /// Its name.
    pub base_name: Option<String>,
    /// Its CP in the source's numbering, when known.
    pub base_cp: Option<u64>,
    /// The source pool (a sink's view).
    pub source_pool: Option<Uuid>,
    /// The source volume's index (a sink's view).
    pub source_volume_index: Option<u32>,
    /// The delta in flight.
    pub landing: Option<LandingReport>,
    /// The live volume may hold a partial delta (rolled back to the base
    /// before the next delta of another identity, or at release).
    pub dirty: bool,
    /// A record is kept in the state store.
    pub persisted: bool,
}

/// Why a replication request was refused.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum ReplError {
    /// The volume, the snapshot or the job does not exist.
    NotFound(String),
    /// The request is malformed.
    BadRequest(String),
    /// The request contradicts the pool's state.
    Conflict(String),
    /// Another transfer of the volume runs.
    Busy(String),
    /// The engine refused.
    Engine(EngineError),
}

impl fmt::Display for ReplError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotFound(s) | Self::BadRequest(s) | Self::Conflict(s) | Self::Busy(s) => {
                f.write_str(s)
            }
            Self::Engine(e) => write!(f, "{e}"),
        }
    }
}

impl std::error::Error for ReplError {}

impl From<EngineError> for ReplError {
    fn from(e: EngineError) -> Self {
        Self::Engine(e)
    }
}

/// What a release did ([`Replicator::release`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ReleaseOutcome {
    /// The volume released.
    pub vol: u32,
    /// The live volume held a partial delta and is being rolled back to
    /// its base by the next consistency point.
    pub rolled_back: bool,
}

fn uuid_from_hash(hash: &blake3::Hash) -> Uuid {
    let mut bytes = [0u8; 16];
    bytes.copy_from_slice(&hash.as_bytes()[..16]);
    bytes[6] = (bytes[6] & 0x0f) | 0x40;
    bytes[8] = (bytes[8] & 0x3f) | 0x80;
    Uuid::from_bytes(bytes)
}

/// The pipe a relationship's cursor belongs to: one per source volume and
/// destination volume pair.
#[must_use]
pub fn pipe_id(src_agg: Uuid, src_vol: u32, dst_agg: Uuid, dst_vol: u32) -> Uuid {
    let mut h = blake3::Hasher::new();
    h.update(b"opentap replication pipe");
    h.update(src_agg.as_array());
    h.update(&src_vol.to_le_bytes());
    h.update(dst_agg.as_array());
    h.update(&dst_vol.to_le_bytes());
    uuid_from_hash(&h.finalize())
}

/// The nonce of the delta from `base` to `target`: the same delta has the
/// same nonce in every connection and every process, so a resume finds it.
#[must_use]
pub fn delta_nonce(target: Uuid, base: Uuid) -> u64 {
    let mut h = blake3::Hasher::new();
    h.update(b"opentap delta nonce");
    h.update(target.as_array());
    h.update(base.as_array());
    let mut word = [0u8; 8];
    word.copy_from_slice(&h.finalize().as_bytes()[..8]);
    u64::from_le_bytes(word).max(1)
}

/// The identity a full transfer gives a volume in `VolEvent::Create`.
#[must_use]
pub fn volume_uuid(agg: Uuid, vol: u32) -> Uuid {
    let mut h = blake3::Hasher::new();
    h.update(b"opentap volume identity");
    h.update(agg.as_array());
    h.update(&vol.to_le_bytes());
    uuid_from_hash(&h.finalize())
}

/// The bytes of a chunk image of `blocks` data blocks: its summary, then
/// the blocks.
#[must_use]
pub fn chunk_image_len(blocks: u32) -> usize {
    summary_bytes(blocks as usize) + blocks as usize * BLOCK_SIZE
}

/// The volume `name` names, or, as `#<index>`, the volume at that index.
pub fn resolve_volume<B: Backstore, F: FrontDoor, C: Clock>(
    eng: &Engine<B, F, C>,
    name: &str,
) -> Option<u32> {
    if let Some(vol) = eng.find_volume(name) {
        return Some(vol);
    }
    let index = name.strip_prefix('#')?.parse::<u32>().ok()?;
    eng.volume(index).filter(|v| v.present).map(|_| index)
}

/// The snapshot of `vol` that `name` names, or, as `#<id>`, the snapshot
/// with that id (unnamed snapshots are addressed so).
pub fn resolve_snapshot<B: Backstore, F: FrontDoor, C: Clock>(
    eng: &Engine<B, F, C>,
    vol: u32,
    name: &str,
) -> Option<u64> {
    if let Some(id) = eng.find_snapshot(vol, name) {
        return Some(id);
    }
    let id = name.strip_prefix('#')?.parse::<u64>().ok()?;
    eng.snapshots()
        .iter()
        .any(|s| s.snap_id == id && s.vol == vol)
        .then_some(id)
}

/// A volume's name, or `#<index>` when it has none.
pub fn volume_label<B: Backstore, F: FrontDoor, C: Clock>(
    eng: &Engine<B, F, C>,
    vol: u32,
) -> String {
    eng.volume_name(vol)
        .map_or_else(|| format!("#{vol}"), str::to_string)
}

/// The snapshot of `vol` with identity `uuid`, unless it is being deleted.
pub fn find_snapshot_by_uuid<B: Backstore, F: FrontDoor, C: Clock>(
    eng: &Engine<B, F, C>,
    vol: u32,
    uuid: Uuid,
) -> Option<SnapshotInfo> {
    eng.snapshots()
        .into_iter()
        .find(|s| s.vol == vol && s.uuid == uuid && !s.deleting)
}

/// The newest snapshot of `vol` a replication sink took (origin
/// `Transfer`), unless it is being deleted: a destination's committed base.
pub fn newest_transfer<B: Backstore, F: FrontDoor, C: Clock>(
    eng: &Engine<B, F, C>,
    vol: u32,
) -> Option<SnapshotInfo> {
    eng.snapshots()
        .into_iter()
        .filter(|s| s.vol == vol && s.origin == SnapOrigin::Transfer && !s.deleting)
        .max_by_key(|s| s.cp_seq)
}

/// `true` when the pool itself says volume `vol` is a replication
/// destination: its newest transfer snapshot carries the replication pin,
/// which a sink sets on the base it committed and `replicate release`
/// clears. With the sink record lost (a state directory on a tmpfs after a
/// host reboot) this is what keeps the mirror marked, so a partial delta is
/// never brought online (the Step 6 review's D6); a volume without it is
/// not a mirror and is refused as a destination unless a record names it
/// (D5).
#[must_use]
pub fn is_mirror_by_pool<B: Backstore, F: FrontDoor, C: Clock>(
    eng: &Engine<B, F, C>,
    vol: u32,
) -> bool {
    newest_transfer(eng, vol).is_some_and(|b| b.flags & PIN_REPLICATION != 0)
}

/// A snapshot of identity `uuid` held by a volume of this pool other than
/// `vol`: a destination snapshot cannot take that identity, since uuids are
/// unique per pool (the Step 6 review's W1).
pub(crate) fn snapshot_elsewhere<B: Backstore, F: FrontDoor, C: Clock>(
    eng: &Engine<B, F, C>,
    uuid: Uuid,
    vol: Option<u32>,
) -> Option<SnapshotInfo> {
    eng.snapshots()
        .into_iter()
        .find(|s| s.uuid == uuid && Some(s.vol) != vol)
}

/// A snapshot as a job remembers it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SnapRef {
    pub(crate) snap_id: u64,
    pub(crate) uuid: Uuid,
    pub(crate) cp: u64,
    pub(crate) name: Option<String>,
}

impl SnapRef {
    pub(crate) fn from_info(info: &SnapshotInfo) -> Self {
        Self {
            snap_id: info.snap_id,
            uuid: info.uuid,
            cp: info.cp_seq,
            name: info.name.clone(),
        }
    }

    pub(crate) fn label(&self) -> String {
        self.name.clone().unwrap_or_else(|| self.uuid.to_string())
    }
}

/// Dial delays that grow while connections fail and reset on progress.
#[derive(Debug, Clone)]
pub(crate) struct Backoff {
    initial: u64,
    max: u64,
    delay: u64,
    pub(crate) next_at: u64,
    failures: u32,
}

impl Backoff {
    pub(crate) fn new(initial: u64, max: u64, now: u64) -> Self {
        Self {
            initial,
            max,
            delay: initial,
            next_at: now,
            failures: 0,
        }
    }

    /// A connection failed: the next dial waits. `false` once more than
    /// `limit` failed in a row.
    pub(crate) fn failed(&mut self, now: u64, limit: u32) -> bool {
        self.failures += 1;
        if self.failures > limit {
            return false;
        }
        self.next_at = now.saturating_add(self.delay);
        self.delay = self.delay.saturating_mul(2).min(self.max).max(1);
        true
    }

    /// The transfer moved on: the next failure starts from the first delay.
    pub(crate) fn progress(&mut self, now: u64) {
        self.failures = 0;
        self.delay = self.initial;
        self.next_at = now;
    }
}

/// A message off a link, decoded.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Inbound {
    Handshake(Handshake),
    Refuse(Refusal),
    /// A handshake of another protocol version.
    Version(u16),
    /// A stream frame, verified.
    Frame(FrameMeta, Frame),
    /// A stream frame whose payload failed its CRC (the stream is in sync).
    Corrupt {
        kind: u8,
        idx: u32,
    },
}

/// A link with its send queue and counters.
pub(crate) struct Conn {
    link: Box<dyn Link>,
    out: VecDeque<Vec<u8>>,
    encoder: Encoder,
    pub(crate) frames_sent: u64,
    pub(crate) frames_received: u64,
    pub(crate) last_recv_ns: u64,
    pub(crate) last_send_ns: u64,
}

impl Conn {
    pub(crate) fn new(link: Box<dyn Link>, now: u64) -> Self {
        Self {
            link,
            out: VecDeque::new(),
            encoder: Encoder::new(),
            frames_sent: 0,
            frames_received: 0,
            last_recv_ns: now,
            last_send_ns: now,
        }
    }

    pub(crate) fn peer(&self) -> String {
        self.link.peer()
    }

    /// Messages queued and not yet taken by the link.
    pub(crate) fn backlog(&self) -> usize {
        self.out.len()
    }

    pub(crate) fn queue(&mut self, msg: &Message) -> Result<(), String> {
        let payload = msg
            .encode()
            .map_err(|e| format!("encoding a message: {e}"))?;
        self.out.push_back(payload);
        Ok(())
    }

    pub(crate) fn queue_handshake(&mut self, h: &Handshake) -> Result<(), String> {
        self.queue(&Message::Handshake(h.clone()))
    }

    pub(crate) fn queue_frame(&mut self, meta: &FrameMeta, frame: &Frame) -> Result<(), String> {
        let bytes = self
            .encoder
            .encode(meta, frame)
            .map_err(|e| format!("encoding a {:?} frame: {e}", frame.kind()))?;
        self.queue(&Message::Stream(bytes))
    }

    /// Hands queued messages to the link until it is busy.
    pub(crate) fn flush(&mut self, now: u64) -> Result<(), String> {
        while let Some(front) = self.out.front() {
            match self.link.send(front) {
                Ok(()) => {
                    self.out.pop_front();
                    self.frames_sent += 1;
                    self.last_send_ns = now;
                }
                Err(LinkError::Busy) => break,
                Err(e) => return Err(e.to_string()),
            }
        }
        Ok(())
    }

    /// The next message, decoded; `Err` ends the connection.
    pub(crate) fn recv(&mut self, now: u64) -> Result<Option<Inbound>, String> {
        let payload = match self.link.recv() {
            Ok(Some(p)) => p,
            Ok(None) | Err(LinkError::Busy) => return Ok(None),
            Err(e) => return Err(e.to_string()),
        };
        self.frames_received += 1;
        self.last_recv_ns = now;
        if payload.first() == Some(&1) {
            if let Ok(v) = Handshake::peek_version(&payload[1..]) {
                if v != REPL_PROTOCOL_VERSION {
                    return Ok(Some(Inbound::Version(v)));
                }
            }
        }
        match Message::decode(&payload) {
            Ok(Message::Handshake(h)) => Ok(Some(Inbound::Handshake(h))),
            Ok(Message::Refuse(r)) => Ok(Some(Inbound::Refuse(r))),
            Ok(Message::Stream(bytes)) => decode_stream(&bytes).map(Some),
            Err(e) => Err(format!("a malformed message from the peer: {e}")),
        }
    }

    /// Sends what is queued (as far as the link takes it) and closes.
    pub(crate) fn close(&mut self) {
        let now = self.last_send_ns;
        let _ = self.flush(now);
        self.link.close();
    }
}

/// One stream frame off a message: verified by the stream's own decoder.
fn decode_stream(bytes: &Bytes) -> Result<Inbound, String> {
    let mut decoder = Decoder::new();
    decoder.feed(bytes);
    match decoder.next_frame() {
        Ok(Some(raw)) => {
            if decoder.pending() != 0 {
                return Err(format!(
                    "a stream message carries {} bytes beyond its frame",
                    decoder.pending()
                ));
            }
            raw.decode()
                .map(|(meta, frame)| Inbound::Frame(meta, frame))
                .map_err(|e| format!("a stream frame from the peer: {e}"))
        }
        Ok(None) => Err("a stream message holds a truncated frame".to_string()),
        Err(StreamError::PayloadCrc {
            kind, chunk_idx, ..
        }) => Ok(Inbound::Corrupt {
            kind,
            idx: chunk_idx,
        }),
        Err(e) => Err(format!("a stream frame from the peer: {e}")),
    }
}

/// A connection accepted and not yet routed: waiting for its handshake.
struct Pending {
    conn: Conn,
    since: u64,
}

enum Job {
    Source(Box<SourceJob>),
    Sink(Box<SinkJob>),
}

impl Job {
    fn id(&self) -> u64 {
        match self {
            Self::Source(j) => j.id,
            Self::Sink(j) => j.id,
        }
    }

    fn state(&self) -> JobState {
        match self {
            Self::Source(j) => j.state,
            Self::Sink(j) => j.state,
        }
    }

    fn report(&self) -> JobReport {
        match self {
            Self::Source(j) => j.report(),
            Self::Sink(j) => j.report(),
        }
    }
}

/// The replication jobs of one pool, stepped between engine ticks.
pub struct Replicator {
    cfg: ReplConfig,
    dialer: Box<dyn Dialer>,
    store: Box<dyn SinkStore>,
    pending: Vec<Pending>,
    jobs: Vec<Job>,
    next_id: u64,
    log: Vec<String>,
}

impl fmt::Debug for Replicator {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Replicator")
            .field("jobs", &self.jobs.len())
            .field("pending", &self.pending.len())
            .finish_non_exhaustive()
    }
}

impl Replicator {
    /// A replicator that dials with `dialer` and keeps sink records in
    /// `store`.
    #[must_use]
    pub fn new(cfg: ReplConfig, dialer: Box<dyn Dialer>, store: Box<dyn SinkStore>) -> Self {
        Self {
            cfg,
            dialer,
            store,
            pending: Vec::new(),
            jobs: Vec::new(),
            next_id: 1,
            log: Vec::new(),
        }
    }

    /// The configuration.
    #[must_use]
    pub const fn config(&self) -> &ReplConfig {
        &self.cfg
    }

    /// Lines worth a journal entry since the last call.
    pub fn take_log(&mut self) -> Vec<String> {
        std::mem::take(&mut self.log)
    }

    /// After a boot: marks every destination this pool's sink records name
    /// as a replica again (the mark lives in memory), so no client can
    /// bring a mirror online before its source resumes. Returns what it
    /// found, for the journal.
    pub fn attach<B: Backstore, F: FrontDoor, C: Clock>(
        &mut self,
        eng: &mut Engine<B, F, C>,
    ) -> Vec<String> {
        let mut lines = Vec::new();
        let keys = match self.store.keys() {
            Ok(keys) => keys,
            Err(e) => {
                lines.push(format!(
                    "replication: the sink records cannot be listed: {e}"
                ));
                return lines;
            }
        };
        let agg = eng.config().agg_id;
        for key in keys.iter().filter(|k| k.starts_with("sink-")) {
            let rec = match self
                .store
                .load(key)
                .map(|b| b.map(|b| SinkRecord::decode(&b)))
            {
                Ok(Some(Ok(rec))) => rec,
                Ok(Some(Err(e))) | Err(e) => {
                    lines.push(format!(
                        "replication: the sink record {key} is unreadable ({e}); the destination \
                         is reconciled from the pool at its next transfer"
                    ));
                    continue;
                }
                Ok(None) => continue,
            };
            if rec.dst_agg != agg {
                continue;
            }
            let mut rec = rec;
            let vol = rec.dst_vol;
            if (rec.cursor.landing.is_some() || !rec.landing_target.is_nil())
                && rec.outlived_by_rewind(eng.cp_seq(), eng.cp_nonce())
            {
                // The boot rewound below the CP the record was written after:
                // chunks it calls landed may be gone with that CP. Right
                // after the boot commit is the one moment a rewind of a
                // single CP is visible (the boot commits under the lost CP's
                // number), so the landing is forgotten here, durably, before
                // any source can resume from it (the Step 6 review's D3).
                rec.forget_landing();
                match rec.encode() {
                    Ok(bytes) => {
                        if let Err(e) = self.store.save(key, &bytes) {
                            lines.push(format!(
                                "replication: the sink record {key} cannot be saved ({e}); it is \
                                 removed, and the destination is reconciled from the pool"
                            ));
                            let _ = self.store.remove(key);
                        }
                    }
                    Err(e) => {
                        lines.push(format!(
                            "replication: the sink record {key} cannot be rewritten ({e}); it is \
                             removed, and the destination is reconciled from the pool"
                        ));
                        let _ = self.store.remove(key);
                    }
                }
                lines.push(format!(
                    "replication: the pool rewound below the sink record of volume {}; the delta \
                     in flight starts over from the base",
                    volume_label(eng, vol)
                ));
            }
            match eng.volume(vol) {
                Some(v) if v.present && v.offline => match eng.set_replica(vol, true) {
                    Ok(()) => lines.push(format!(
                        "replication: volume {} is the mirror of volume {} of pool {}{}",
                        volume_label(eng, vol),
                        rec.src_vol,
                        rec.src_agg,
                        if rec.cursor.landing.is_some() {
                            "; a delta resumes when its source reconnects"
                        } else {
                            ""
                        }
                    )),
                    Err(e) => lines.push(format!(
                        "replication: volume {vol} cannot be marked as a mirror: {e}"
                    )),
                },
                Some(v) if v.present => lines.push(format!(
                    "replication: volume {} has a sink record but is online; it is no longer \
                     treated as a mirror (`replicate release` forgets the record)",
                    volume_label(eng, vol)
                )),
                _ => {
                    let _ = self.store.remove(key);
                    lines.push(format!(
                        "replication: the sink record of the gone volume {vol} was removed"
                    ));
                }
            }
        }
        // A mirror whose record is gone (the state directory emptied, as a
        // tmpfs is at a host reboot) is still a mirror: the pool says so by
        // the replication pin on its newest transfer snapshot, and its live
        // volume may hold part of a delta (the Step 6 review's D6).
        for (vol, info) in eng.volumes() {
            if info.present && info.offline && !eng.is_replica(vol) && is_mirror_by_pool(eng, vol) {
                match eng.set_replica(vol, true) {
                    Ok(()) => lines.push(format!(
                        "replication: volume {} holds a pinned transfer snapshot and no sink \
                         record; it stays a mirror (its next transfer starts from that base; \
                         `replicate release` ends the relationship)",
                        volume_label(eng, vol)
                    )),
                    Err(e) => lines.push(format!(
                        "replication: volume {vol} cannot be marked as a mirror: {e}"
                    )),
                }
            }
        }
        lines
    }

    /// Forgets the sink record of destination volume `vol`: the daemon
    /// calls it when the volume is deleted, so a record never outlives its
    /// volume and attaches to the next volume at that index (the Step 6
    /// review's D7).
    ///
    /// # Errors
    /// The store's, as text.
    pub fn forget(&mut self, vol: u32) -> Result<(), String> {
        self.store
            .remove(&SinkRecord::key(vol))
            .map_err(|e| format!("the sink record of volume {vol} cannot be removed: {e}"))
    }

    /// Hands over a connection a listener accepted; it is routed once its
    /// handshake arrives.
    pub fn offer(&mut self, link: Box<dyn Link>, now: u64) {
        self.pending.push(Pending {
            conn: Conn::new(link, now),
            since: now,
        });
    }

    /// Starts pushing snapshot `snapshot` of `volume` to the sink at
    /// `addr`, into its volume `destination` (by default the same name).
    pub fn push<B: Backstore, F: FrontDoor, C: Clock>(
        &mut self,
        eng: &mut Engine<B, F, C>,
        volume: &str,
        snapshot: &str,
        addr: &str,
        destination: Option<&str>,
    ) -> Result<JobReport, ReplError> {
        let now = eng.mono_ns();
        let vol = resolve_volume(eng, volume)
            .ok_or_else(|| ReplError::NotFound(format!("no volume named {volume:?}")))?;
        let label = volume_label(eng, vol);
        let snap_id = resolve_snapshot(eng, vol, snapshot).ok_or_else(|| {
            ReplError::NotFound(format!(
                "no snapshot named {snapshot:?} on volume {label:?}"
            ))
        })?;
        let info = eng
            .snapshots()
            .into_iter()
            .find(|s| s.snap_id == snap_id)
            .ok_or_else(|| ReplError::NotFound(format!("snapshot {snapshot:?} is gone")))?;
        if info.damaged {
            return Err(ReplError::Conflict(format!(
                "snapshot {snapshot:?} is quarantined (damaged) and cannot be a replication source"
            )));
        }
        if info.deleting {
            return Err(ReplError::Conflict(format!(
                "snapshot {snapshot:?} is being deleted"
            )));
        }
        if self.running_source(vol).is_some() || self.running_sink(vol).is_some() {
            return Err(ReplError::Busy(format!(
                "a transfer of volume {label:?} is running; wait for it or cancel it"
            )));
        }
        let dest = destination.unwrap_or(&label).to_string();
        validate_name(dest.as_bytes()).map_err(|e| {
            ReplError::BadRequest(format!("the destination volume name {dest:?}: {e}"))
        })?;
        let link = self
            .dialer
            .dial(addr)
            .map_err(|e| ReplError::BadRequest(e.to_string()))?;
        let id = self.take_id();
        let job = SourceJob::push(
            id,
            eng,
            vol,
            label.clone(),
            SnapRef::from_info(&info),
            addr.to_string(),
            dest.clone(),
            self.cfg.source.clone(),
            link,
            now,
        );
        let report = job.report();
        self.log.push(format!(
            "replication job {id}: pushing snapshot {snapshot:?} of volume {label:?} to {addr} \
             as {dest:?}"
        ));
        self.jobs.push(Job::Source(Box::new(job)));
        Ok(report)
    }

    /// Starts pulling snapshot `snapshot` of the source volume
    /// `remote_volume` (by default the same name) from the source at
    /// `addr` into the local volume `volume`.
    pub fn pull<B: Backstore, F: FrontDoor, C: Clock>(
        &mut self,
        eng: &mut Engine<B, F, C>,
        volume: &str,
        snapshot: &str,
        addr: &str,
        remote_volume: Option<&str>,
    ) -> Result<JobReport, ReplError> {
        let now = eng.mono_ns();
        validate_name(volume.as_bytes())
            .map_err(|e| ReplError::BadRequest(format!("the volume name {volume:?}: {e}")))?;
        validate_name(snapshot.as_bytes())
            .map_err(|e| ReplError::BadRequest(format!("the snapshot name {snapshot:?}: {e}")))?;
        let src_volume = remote_volume.unwrap_or(volume).to_string();
        validate_name(src_volume.as_bytes()).map_err(|e| {
            ReplError::BadRequest(format!("the source volume name {src_volume:?}: {e}"))
        })?;
        let dst = resolve_volume(eng, volume);
        if let Some(vol) = dst {
            if self.running_sink(vol).is_some() || self.running_source(vol).is_some() {
                return Err(ReplError::Busy(format!(
                    "a transfer of volume {volume:?} is running; wait for it or cancel it"
                )));
            }
            if eng.volume(vol).is_some_and(|v| !v.offline) {
                return Err(ReplError::Conflict(format!(
                    "volume {volume:?} is online; a replication destination is a mirror and must \
                     be offline (or name a new volume)"
                )));
            }
        } else if self.running_sink_named(volume).is_some() {
            return Err(ReplError::Busy(format!(
                "a transfer into volume {volume:?} is running"
            )));
        }
        let link = self
            .dialer
            .dial(addr)
            .map_err(|e| ReplError::BadRequest(e.to_string()))?;
        let id = self.take_id();
        let mut job = SinkJob::new(
            id,
            JobMode::Pull,
            eng,
            &mut *self.store,
            volume.to_string(),
            addr.to_string(),
            src_volume.clone(),
            snapshot.to_string(),
            self.cfg.sink.clone(),
            now,
        );
        job.open(link, now);
        let report = job.report();
        self.log.push(format!(
            "replication job {id}: pulling snapshot {snapshot:?} of volume {src_volume:?} from \
             {addr} into {volume:?}"
        ));
        self.jobs.push(Job::Sink(Box::new(job)));
        Ok(report)
    }

    fn take_id(&mut self) -> u64 {
        let id = self.next_id;
        self.next_id += 1;
        id
    }

    fn running_source(&self, vol: u32) -> Option<usize> {
        self.jobs.iter().position(|j| match j {
            Job::Source(s) => s.vol == vol && s.state == JobState::Running,
            Job::Sink(_) => false,
        })
    }

    fn running_sink(&self, vol: u32) -> Option<usize> {
        self.jobs.iter().position(|j| match j {
            Job::Sink(s) => s.dst_vol == Some(vol) && s.state == JobState::Running,
            Job::Source(_) => false,
        })
    }

    fn running_sink_named(&self, name: &str) -> Option<usize> {
        self.jobs.iter().position(|j| match j {
            Job::Sink(s) => s.dst_name == name && s.state == JobState::Running,
            Job::Source(_) => false,
        })
    }

    /// One step of every job and of the connections waiting for their
    /// handshake; `true` when something was done (the host steps again at
    /// once rather than idling).
    pub fn step<B: Backstore, F: FrontDoor, C: Clock>(
        &mut self,
        eng: &mut Engine<B, F, C>,
    ) -> bool {
        let now = eng.mono_ns();
        let mut busy = self.route(eng, now);
        for job in &mut self.jobs {
            let before = job.state();
            let did = match job {
                Job::Source(j) => j.step(eng, &mut *self.dialer, now),
                Job::Sink(j) => j.step(eng, &mut *self.dialer, &mut *self.store, now),
            };
            busy |= did;
            let after = job.state();
            if before == JobState::Running && after != JobState::Running {
                let r = job.report();
                self.log.push(match after {
                    JobState::Done => format!(
                        "replication job {} ({} of {}) done: {} chunks, {} retries",
                        r.id,
                        r.mode.word(),
                        r.volume,
                        r.chunks,
                        r.retries
                    ),
                    _ => format!(
                        "replication job {} ({} of {}) {:?}: {}",
                        r.id,
                        r.mode.word(),
                        r.volume,
                        after,
                        r.last_error.unwrap_or_default()
                    ),
                });
            }
        }
        // Keep the running jobs and the newest finished ones.
        let finished: Vec<u64> = self
            .jobs
            .iter()
            .filter(|j| j.state() != JobState::Running)
            .map(Job::id)
            .collect();
        if finished.len() > self.cfg.keep_finished {
            let drop: Vec<u64> = finished[..finished.len() - self.cfg.keep_finished].to_vec();
            self.jobs.retain(|j| !drop.contains(&j.id()));
        }
        busy
    }

    /// Routes the accepted connections whose handshake arrived.
    fn route<B: Backstore, F: FrontDoor, C: Clock>(
        &mut self,
        eng: &mut Engine<B, F, C>,
        now: u64,
    ) -> bool {
        let mut busy = false;
        let pending = std::mem::take(&mut self.pending);
        for mut p in pending {
            match p.conn.recv(now) {
                Ok(None) => {
                    if now.saturating_sub(p.since) > self.cfg.handshake_timeout_ns {
                        p.conn.close();
                    } else {
                        self.pending.push(p);
                    }
                }
                Ok(Some(Inbound::Handshake(h))) => {
                    busy = true;
                    self.route_handshake(eng, p.conn, &h, now);
                }
                Ok(Some(Inbound::Version(v))) => {
                    busy = true;
                    refuse(
                        p.conn,
                        Refusal::new(
                            RefuseCode::Version,
                            format!(
                                "this pool speaks replication protocol {REPL_PROTOCOL_VERSION}, \
                                 the peer {v}"
                            ),
                        ),
                    );
                }
                Ok(Some(_)) => {
                    busy = true;
                    refuse(
                        p.conn,
                        Refusal::new(
                            RefuseCode::BadRequest,
                            "the first message of a replication connection must be a handshake",
                        ),
                    );
                }
                Err(_) => {
                    busy = true;
                    p.conn.close();
                }
            }
        }
        busy
    }

    fn route_handshake<B: Backstore, F: FrontDoor, C: Clock>(
        &mut self,
        eng: &mut Engine<B, F, C>,
        conn: Conn,
        h: &Handshake,
        now: u64,
    ) {
        if h.accept {
            refuse(
                conn,
                Refusal::new(
                    RefuseCode::BadRequest,
                    "an answer where a request was expected",
                ),
            );
            return;
        }
        if h.agg_id == eng.config().agg_id {
            refuse(
                conn,
                Refusal::new(
                    RefuseCode::Conflict,
                    "the source and the destination are the same pool: a pool cannot replicate \
                     to itself",
                ),
            );
            return;
        }
        if let Err(e) = validate_name(h.peer_volume.as_bytes()) {
            refuse(
                conn,
                Refusal::new(
                    RefuseCode::BadRequest,
                    format!("the volume name {:?}: {e}", h.peer_volume),
                ),
            );
            return;
        }
        match h.role {
            Role::Source => self.route_push(eng, conn, h, now),
            Role::Sink => self.route_pull(eng, conn, h, now),
        }
    }

    /// A source pushes to this pool: the sink job of the destination takes
    /// the connection (a new one, or the running one when the source comes
    /// back).
    fn route_push<B: Backstore, F: FrontDoor, C: Clock>(
        &mut self,
        eng: &mut Engine<B, F, C>,
        conn: Conn,
        h: &Handshake,
        now: u64,
    ) {
        let dst = resolve_volume(eng, &h.peer_volume);
        if let Some(vol) = dst {
            if self.running_source(vol).is_some() {
                refuse(
                    conn,
                    Refusal::new(
                        RefuseCode::Busy,
                        format!("volume {:?} is being pushed from this pool", h.peer_volume),
                    ),
                );
                return;
            }
        }
        let existing = match dst {
            Some(vol) => self.running_sink(vol),
            None => self.running_sink_named(&h.peer_volume),
        };
        let (index, fresh) = match existing {
            Some(i) => (i, false),
            None => {
                let id = self.take_id();
                let job = SinkJob::new(
                    id,
                    JobMode::InboundPush,
                    eng,
                    &mut *self.store,
                    h.peer_volume.clone(),
                    conn.peer(),
                    h.volume.clone(),
                    h.snapshot_name.clone(),
                    self.cfg.sink.clone(),
                    now,
                );
                self.jobs.push(Job::Sink(Box::new(job)));
                (self.jobs.len() - 1, true)
            }
        };
        let Job::Sink(job) = &mut self.jobs[index] else {
            refuse(
                conn,
                Refusal::new(RefuseCode::Internal, "the job table is confused"),
            );
            return;
        };
        match job.attach(eng, conn, h, now) {
            Ok(()) => {
                if fresh {
                    let id = job.id;
                    self.log.push(format!(
                        "replication job {id}: receiving snapshot {:?} of volume {:?} from {} \
                         into {:?}",
                        h.snapshot_name,
                        h.volume,
                        job.report().peer,
                        h.peer_volume
                    ));
                }
            }
            Err((conn, refusal)) => {
                refuse(conn, refusal);
                if fresh {
                    self.jobs.remove(index);
                }
            }
        }
    }

    /// A sink pulls from this pool: a source job of the volume takes the
    /// connection.
    fn route_pull<B: Backstore, F: FrontDoor, C: Clock>(
        &mut self,
        eng: &mut Engine<B, F, C>,
        conn: Conn,
        h: &Handshake,
        now: u64,
    ) {
        let Some(vol) = resolve_volume(eng, &h.peer_volume) else {
            refuse(
                conn,
                Refusal::new(
                    RefuseCode::NotFound,
                    format!("no volume named {:?} on the source", h.peer_volume),
                ),
            );
            return;
        };
        let label = volume_label(eng, vol);
        let info = resolve_snapshot(eng, vol, &h.snapshot_name)
            .and_then(|id| eng.snapshots().into_iter().find(|s| s.snap_id == id));
        let Some(info) = info else {
            refuse(
                conn,
                Refusal::new(
                    RefuseCode::NotFound,
                    format!(
                        "no snapshot named {:?} on volume {label:?} of the source",
                        h.snapshot_name
                    ),
                ),
            );
            return;
        };
        if info.damaged {
            refuse(
                conn,
                Refusal::new(
                    RefuseCode::Quarantined,
                    format!(
                        "snapshot {:?} is quarantined (damaged) and cannot be a replication \
                         source",
                        h.snapshot_name
                    ),
                ),
            );
            return;
        }
        if self.running_sink(vol).is_some() {
            refuse(
                conn,
                Refusal::new(
                    RefuseCode::Busy,
                    format!("volume {label:?} is itself receiving a transfer"),
                ),
            );
            return;
        }
        let existing = self.jobs.iter().position(|j| match j {
            Job::Source(s) => s.vol == vol && s.state == JobState::Running,
            Job::Sink(_) => false,
        });
        let (index, fresh) = match existing {
            Some(i) => {
                let same = matches!(&self.jobs[i], Job::Source(s)
                    if s.mode == JobMode::InboundPull && s.target.uuid == info.uuid);
                if !same {
                    refuse(
                        conn,
                        Refusal::new(
                            RefuseCode::Busy,
                            format!("another transfer of volume {label:?} is running"),
                        ),
                    );
                    return;
                }
                (i, false)
            }
            None => {
                let id = self.take_id();
                let job = SourceJob::serve(
                    id,
                    eng,
                    vol,
                    label.clone(),
                    SnapRef::from_info(&info),
                    conn.peer(),
                    h.volume.clone(),
                    self.cfg.source.clone(),
                    now,
                );
                self.jobs.push(Job::Source(Box::new(job)));
                (self.jobs.len() - 1, true)
            }
        };
        let Job::Source(job) = &mut self.jobs[index] else {
            refuse(
                conn,
                Refusal::new(RefuseCode::Internal, "the job table is confused"),
            );
            return;
        };
        match job.attach(eng, conn, h, now) {
            Ok(()) => {
                if fresh {
                    let id = job.id;
                    self.log.push(format!(
                        "replication job {id}: serving snapshot {:?} of volume {label:?} to {}",
                        h.snapshot_name,
                        job.report().peer
                    ));
                }
            }
            Err((conn, refusal)) => {
                refuse(conn, refusal);
                if fresh {
                    self.jobs.remove(index);
                }
            }
        }
    }

    /// `true` while a job runs or a connection waits for its handshake.
    #[must_use]
    pub fn active(&self) -> bool {
        !self.pending.is_empty() || self.jobs.iter().any(|j| j.state() == JobState::Running)
    }

    /// Every job kept, oldest first.
    #[must_use]
    pub fn jobs(&self) -> Vec<JobReport> {
        self.jobs.iter().map(Job::report).collect()
    }

    /// One job.
    #[must_use]
    pub fn job(&self, id: u64) -> Option<JobReport> {
        self.jobs.iter().find(|j| j.id() == id).map(Job::report)
    }

    /// Cancels a running job. A sink keeps its record, so a later transfer
    /// resumes or rolls the destination back to its base.
    pub fn cancel(&mut self, id: u64) -> Result<JobReport, ReplError> {
        let job = self
            .jobs
            .iter_mut()
            .find(|j| j.id() == id)
            .ok_or_else(|| ReplError::NotFound(format!("no replication job {id}")))?;
        if job.state() != JobState::Running {
            return Err(ReplError::Conflict(format!(
                "replication job {id} is not running ({:?})",
                job.state()
            )));
        }
        match job {
            Job::Source(j) => j.cancel(),
            Job::Sink(j) => j.cancel(),
        }
        self.log.push(format!("replication job {id} cancelled"));
        Ok(job.report())
    }

    /// The durable replication position of `volume`: a mirror's sink
    /// record (or what the pool says when it has none), or a source
    /// volume's replication base.
    pub fn cursor<B: Backstore, F: FrontDoor, C: Clock>(
        &mut self,
        eng: &mut Engine<B, F, C>,
        volume: &str,
    ) -> Result<CursorReport, ReplError> {
        let vol = resolve_volume(eng, volume)
            .ok_or_else(|| ReplError::NotFound(format!("no volume named {volume:?}")))?;
        let label = volume_label(eng, vol);
        let key = SinkRecord::key(vol);
        let rec = match self.store.load(&key) {
            Ok(Some(bytes)) => SinkRecord::decode(&bytes).ok(),
            _ => None,
        };
        let replica = eng.is_replica(vol);
        if rec.is_some() || replica || newest_transfer(eng, vol).is_some() {
            let base = newest_transfer(eng, vol);
            let landing = rec.as_ref().and_then(|r| {
                r.cursor.landing.as_ref().map(|l| LandingReport {
                    cp_seq: l.cp_seq,
                    nonce: l.cp_nonce,
                    target: r.landing_target,
                    chunks: l.bitmap.chunks(),
                    landed: l.bitmap.landed_count(),
                    landed_contig: l.bitmap.landed_contig(),
                    sparse_ranges: l.bitmap.sparse_ranges().len() as u32,
                })
            });
            return Ok(CursorReport {
                volume: label,
                volume_index: vol,
                role: Role::Sink,
                replica,
                base_uuid: base.as_ref().map(|b| b.uuid),
                base_name: base.as_ref().and_then(|b| b.name.clone()),
                base_cp: rec
                    .as_ref()
                    .filter(|r| base.as_ref().is_some_and(|b| b.uuid == r.base))
                    .map(|r| r.cursor.applied_cp_seq),
                source_pool: rec.as_ref().map(|r| r.src_agg).filter(|a| !a.is_nil()),
                source_volume_index: rec
                    .as_ref()
                    .filter(|r| !r.src_agg.is_nil())
                    .map(|r| r.src_vol),
                landing,
                dirty: rec.as_ref().is_none_or(|r| r.dirty),
                persisted: rec.is_some(),
            });
        }
        let base = eng
            .snapshots()
            .into_iter()
            .find(|s| s.vol == vol && s.flags & PIN_REPLICATION != 0 && !s.deleting);
        Ok(CursorReport {
            volume: label,
            volume_index: vol,
            role: Role::Source,
            replica: false,
            base_uuid: base.as_ref().map(|b| b.uuid),
            base_name: base.as_ref().and_then(|b| b.name.clone()),
            base_cp: base.as_ref().map(|b| b.cp_seq),
            source_pool: None,
            source_volume_index: None,
            landing: None,
            dirty: false,
            persisted: false,
        })
    }

    /// Ends `volume`'s life as a mirror: the replica mark and the base pin
    /// go, the sink record is forgotten, and a live volume that may hold a
    /// partial delta is rolled back to its base by the next consistency
    /// point (the caller makes sure nothing of the volume is unflushed
    /// first). The volume stays offline; `volume online` serves it.
    pub fn release<B: Backstore, F: FrontDoor, C: Clock>(
        &mut self,
        eng: &mut Engine<B, F, C>,
        volume: &str,
    ) -> Result<ReleaseOutcome, ReplError> {
        let vol = resolve_volume(eng, volume)
            .ok_or_else(|| ReplError::NotFound(format!("no volume named {volume:?}")))?;
        if self.running_sink(vol).is_some() {
            return Err(ReplError::Busy(format!(
                "a transfer into volume {volume:?} is running; cancel it first"
            )));
        }
        let key = SinkRecord::key(vol);
        let rec = match self.store.load(&key) {
            Ok(Some(bytes)) => SinkRecord::decode(&bytes).ok(),
            _ => None,
        };
        let base = newest_transfer(eng, vol);
        if rec.is_none() && !eng.is_replica(vol) && base.is_none() {
            return Err(ReplError::Conflict(format!(
                "volume {volume:?} is not a replication destination"
            )));
        }
        let dirty = rec.as_ref().is_none_or(|r| r.dirty);
        let mut rolled_back = false;
        if dirty {
            match &base {
                Some(b) => {
                    eng.rollback_volume(vol, b.snap_id)?;
                    rolled_back = true;
                }
                None => {
                    return Err(ReplError::Conflict(format!(
                        "volume {volume:?} holds only part of a first transfer and no snapshot; \
                         delete it instead"
                    )));
                }
            }
        }
        eng.set_replica(vol, false)?;
        eng.set_replication_base(vol, None)?;
        self.store
            .remove(&key)
            .map_err(|e| ReplError::Conflict(format!("the sink record cannot be removed: {e}")))?;
        self.log.push(format!(
            "replication: volume {volume:?} released from its mirror relationship{}",
            if rolled_back {
                ", rolled back to its last complete snapshot"
            } else {
                ""
            }
        ));
        Ok(ReleaseOutcome { vol, rolled_back })
    }
}

/// Sends a refusal and closes.
fn refuse(mut conn: Conn, refusal: Refusal) {
    if conn.queue(&Message::Refuse(refusal)).is_ok() {
        let now = conn.last_send_ns;
        let _ = conn.flush(now);
    }
    conn.close();
}

/// Answers a connection nobody will serve (no pool is booted) with
/// `refusal`, then closes it.
pub fn refuse_link(link: Box<dyn Link>, refusal: Refusal) {
    refuse(Conn::new(link, 0), refusal);
}
