//! The typed frames of stream v1 and their wire bodies.
//!
//! Every frame is a 64-byte [`StreamHeader`] followed by a payload. The
//! header carries the frame-level context ([`FrameMeta`]: relationship, chunk
//! index, CP, base, nonce, owner epoch); the payload carries the body of the
//! kind. Bodies are fixed little-endian `#[repr(C)]` structures, some
//! followed by arrays of fixed records (sparse ranges, snapshot tags, acked
//! bytes, hole records) or by a short name. A `Chunk` payload is a stripe
//! image (F3) and is handed up as a zero-copy [`Bytes`] view.
//!
//! # Canonical bodies
//! Body fields repeat nothing the header already carries, and every body
//! has exactly one encoding: reserved bytes are zero, fields an event or
//! mode does not use are zero or nil, sparse ranges are in canonical form,
//! and `chunk_idx` is zero on every frame that is not a chunk. Decoding
//! refuses anything else, so a decoded frame re-encodes to the bytes it came
//! from, and the encoder refuses everything the decoder would. Unknown
//! capability bits in `Hello` are the one thing a receiver ignores, because
//! that is how a newer peer advertises more than an older one understands.

use core::mem::{align_of, offset_of, size_of};

use bytes::{Bytes, BytesMut};
use otap_format::{
    Codec, FormatError, FrameKind, FromBytes, HoleRecord, Immutable, IntoBytes, KnownLayout,
    StreamHeader, StripeHdr, StripeSummary, Uuid, VolBitSet, BLOCK_SIZE, HOLE_RECORD_SIZE,
    MAX_CP_SEQ, MAX_LBA, MAX_STRIPE_BYTES, MAX_VOL, MAX_WIRE_PAYLOAD, MIN_STRIPE_BYTES,
    STRIPE_HDR_SIZE,
};

use crate::cursor::{
    ChunkRange, Cursor, CursorBody, ResumeBitmap, MAX_DELTA_CHUNKS, MAX_WIRE_RANGES,
};
use crate::flow::MAX_CREDITS;
use crate::StreamError;

/// The header-level context of a frame: what the 64-byte header carries
/// besides kind, codec, lengths and CRCs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub struct FrameMeta {
    /// The relationship the frame belongs to.
    pub rel: u32,
    /// The chunk index; 0 for frames that are not chunks.
    pub chunk_idx: u32,
    /// The CP the delta ends at.
    pub cp_seq: u64,
    /// The CP the delta starts from.
    pub base: u64,
    /// The nonce of `cp_seq`.
    pub cp_nonce: u64,
    /// The sender's owner epoch.
    pub owner_epoch: u64,
}

impl FrameMeta {
    /// The context a header carries.
    #[must_use]
    pub const fn from_header(hdr: &StreamHeader) -> Self {
        Self {
            rel: hdr.rel,
            chunk_idx: hdr.chunk_idx,
            cp_seq: hdr.cp_seq,
            base: hdr.base,
            cp_nonce: hdr.cp_nonce,
            owner_epoch: hdr.owner_epoch,
        }
    }

    /// Writes the context into a header.
    pub fn apply(&self, hdr: &mut StreamHeader) {
        hdr.rel = self.rel;
        hdr.chunk_idx = self.chunk_idx;
        hdr.cp_seq = self.cp_seq;
        hdr.base = self.base;
        hdr.cp_nonce = self.cp_nonce;
        hdr.owner_epoch = self.owner_epoch;
    }

    /// The `chunk_idx` rule: zero unless the frame is a chunk, and below
    /// [`MAX_DELTA_CHUNKS`] for one.
    pub const fn check_chunk_idx(&self, kind: FrameKind) -> Result<(), StreamError> {
        let bound = if matches!(kind, FrameKind::Chunk) {
            MAX_DELTA_CHUNKS
        } else {
            1
        };
        if self.chunk_idx >= bound {
            return Err(bad("chunk_idx", self.chunk_idx as u64));
        }
        Ok(())
    }

    /// Every rule the peer's decoder enforces on the context: the
    /// `chunk_idx` rule, `cp_seq` at most [`MAX_CP_SEQ`] and
    /// `base <= cp_seq`. The last two are reported with the errors
    /// [`StreamHeader::verify`] gives for the sealed header, so a refusal
    /// reads the same whether it comes before or after sealing. The encoder
    /// checks this before it reserves room for a frame.
    pub const fn check(&self, kind: FrameKind) -> Result<(), StreamError> {
        if let Err(e) = self.check_chunk_idx(kind) {
            return Err(e);
        }
        if self.cp_seq > MAX_CP_SEQ {
            return Err(StreamError::Format(FormatError::OutOfRange {
                field: "cp_seq",
                value: self.cp_seq,
                max: MAX_CP_SEQ,
            }));
        }
        if self.base > self.cp_seq {
            return Err(StreamError::Format(FormatError::BadField {
                field: "base",
                value: self.base,
            }));
        }
        Ok(())
    }
}

/// `Hello::capabilities` bit: the peer accepts LZ4 chunk payloads.
pub const CAP_CODEC_LZ4: u64 = 1 << 0;
/// `Hello::capabilities` bit: the peer accepts Zstandard chunk payloads.
pub const CAP_CODEC_ZSTD: u64 = 1 << 1;

/// The fixed body of `Hello`.
#[derive(
    FromBytes, IntoBytes, Immutable, KnownLayout, Clone, Copy, PartialEq, Eq, Hash, Default, Debug,
)]
#[repr(C)]
pub struct HelloBody {
    /// The sending node.
    pub node_id: Uuid,
    /// The pipe (one per aggregate pair and direction).
    pub pipe_id: Uuid,
    /// The direction epoch; a stale one is refused.
    pub direction_epoch: u64,
    /// `CAP_*` bits; unknown bits are ignored.
    pub capabilities: u64,
    /// The largest payload the sender will emit, at most [`MAX_WIRE_PAYLOAD`].
    pub max_payload: u32,
    /// Credits a receiver proposes as the initial grant (0 from a sender),
    /// at most [`MAX_CREDITS`].
    pub initial_credits: u32,
    /// Reserved, zero.
    pub rsvd: [u8; 8],
}

/// The opening frame of a session.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Hello {
    /// The sending node.
    pub node_id: Uuid,
    /// The pipe.
    pub pipe_id: Uuid,
    /// The direction epoch.
    pub direction_epoch: u64,
    /// `CAP_*` bits.
    pub capabilities: u64,
    /// The largest payload the sender will emit.
    pub max_payload: u32,
    /// Proposed initial credits.
    pub initial_credits: u32,
}

impl Hello {
    /// `true` when `cap` (one or more `CAP_*` bits) is advertised.
    #[must_use]
    pub const fn supports(&self, cap: u64) -> bool {
        self.capabilities & cap == cap
    }

    fn body(&self) -> HelloBody {
        HelloBody {
            node_id: self.node_id,
            pipe_id: self.pipe_id,
            direction_epoch: self.direction_epoch,
            capabilities: self.capabilities,
            max_payload: self.max_payload,
            initial_credits: self.initial_credits,
            rsvd: [0; 8],
        }
    }

    fn from_body(body: &HelloBody) -> Result<Self, StreamError> {
        zero("rsvd", &body.rsvd)?;
        if body.max_payload > MAX_WIRE_PAYLOAD || (body.max_payload as usize) < MIN_STRIPE_BYTES {
            return Err(bad("max_payload", u64::from(body.max_payload)));
        }
        if body.initial_credits > MAX_CREDITS {
            return Err(bad("initial_credits", u64::from(body.initial_credits)));
        }
        if body.node_id.is_nil() {
            return Err(bad("node_id", 0));
        }
        if body.pipe_id.is_nil() {
            return Err(bad("pipe_id", 0));
        }
        Ok(Self {
            node_id: body.node_id,
            pipe_id: body.pipe_id,
            direction_epoch: body.direction_epoch,
            capabilities: body.capabilities,
            max_payload: body.max_payload,
            initial_credits: body.initial_credits,
        })
    }
}

/// The receiver's answer to `Hello`: its cursor.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HelloAck {
    /// The receiver's resume state.
    pub cursor: Cursor,
}

/// How a delta was produced.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum DeltaMode {
    /// The sender's CP write set.
    Steady,
    /// A catch-up increment between two retained snapshots.
    Increment {
        /// The older snapshot.
        from: Uuid,
        /// The newer snapshot, materialized on the receiver.
        to: Uuid,
    },
}

const MODE_STEADY: u8 = 1;
const MODE_INCREMENT: u8 = 2;

/// A snapshot tag carried by `DeltaBegin`.
#[derive(
    FromBytes, IntoBytes, Immutable, KnownLayout, Clone, Copy, PartialEq, Eq, Hash, Default, Debug,
)]
#[repr(C)]
pub struct SnapTag {
    /// The snapshot.
    pub uuid: Uuid,
    /// Retention in seconds; 0 keeps it until the sender deletes it.
    pub retention_s: u32,
    /// Reserved, zero.
    pub rsvd: u32,
}

/// Bytes acknowledged and unflushed per volume at the cut, carried by
/// `DeltaBegin` for the promotion set (R40).
#[derive(
    FromBytes, IntoBytes, Immutable, KnownLayout, Clone, Copy, PartialEq, Eq, Hash, Default, Debug,
)]
#[repr(C)]
pub struct AckedBytes {
    /// The volume.
    pub vol: u32,
    /// Reserved, zero.
    pub rsvd: u32,
    /// The bytes.
    pub bytes: u64,
}

/// The fixed body of `DeltaBegin`, followed by `tags` [`SnapTag`]s and
/// `acked` [`AckedBytes`].
#[derive(
    FromBytes, IntoBytes, Immutable, KnownLayout, Clone, Copy, PartialEq, Eq, Hash, Default, Debug,
)]
#[repr(C)]
pub struct DeltaBeginBody {
    /// `MODE_STEADY` or `MODE_INCREMENT`.
    pub mode: u8,
    /// Reserved, zero.
    pub rsvd0: [u8; 3],
    /// Chunks in the delta, `1..=MAX_DELTA_CHUNKS`.
    pub chunks: u32,
    /// Age of the cut when the delta was described, milliseconds.
    pub cut_age_ms: u32,
    /// Snapshot tags that follow.
    pub tags: u32,
    /// Acked-bytes records that follow.
    pub acked: u32,
    /// Reserved, zero.
    pub rsvd1: u32,
    /// Stripe image bytes over every chunk, the sum of the chunks'
    /// `raw_len`: between `chunks` smallest and `chunks` largest images,
    /// whatever codec the chunks travel under.
    pub bytes: u64,
    /// The older snapshot of an increment; nil for a steady delta.
    pub from_snap: Uuid,
    /// The newer snapshot of an increment; nil for a steady delta.
    pub to_snap: Uuid,
    /// The volumes write-active at the cut.
    pub write_active: VolBitSet,
}

/// The description of a delta.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeltaBegin {
    /// How it was produced.
    pub mode: DeltaMode,
    /// Chunks in the delta.
    pub chunks: u32,
    /// Stripe image bytes over every chunk (the sum of the chunks'
    /// `raw_len`, not what the wire carries under a codec).
    pub bytes: u64,
    /// Age of the cut, milliseconds.
    pub cut_age_ms: u32,
    /// The volumes write-active at the cut.
    pub write_active: VolBitSet,
    /// Snapshot tags.
    pub tags: Vec<SnapTag>,
    /// Acked bytes per volume.
    pub acked: Vec<AckedBytes>,
}

/// What a `VolEvent` announces.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[repr(u8)]
pub enum VolEventKind {
    /// A volume was created: `vol`, `class`, `size_blocks`, `stripe_bytes`,
    /// `uuid` (the volume's) and `name`.
    Create = 1,
    /// A volume was resized: `vol`, `size_blocks`.
    Resize = 2,
    /// A volume was deleted: `vol`.
    Delete = 3,
    /// A snapshot was tagged: `vol`, `uuid` (the snapshot's), `retention_s`,
    /// `name`.
    SnapTag = 4,
}

impl VolEventKind {
    /// Decodes the byte.
    #[must_use]
    pub const fn from_u8(value: u8) -> Option<Self> {
        match value {
            1 => Some(Self::Create),
            2 => Some(Self::Resize),
            3 => Some(Self::Delete),
            4 => Some(Self::SnapTag),
            _ => None,
        }
    }

    /// The byte.
    #[must_use]
    pub const fn as_u8(self) -> u8 {
        self as u8
    }
}

/// The nvfail class of a volume (owner decision D5).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[repr(u8)]
pub enum VolClass {
    /// A datastore LUN.
    DatastoreLun = 1,
    /// An NFS datastore.
    DatastoreNfs = 2,
    /// A file volume.
    File = 3,
    /// An object volume.
    Object = 4,
    /// An SVM root volume.
    Root = 5,
}

impl VolClass {
    /// Decodes the byte.
    #[must_use]
    pub const fn from_u8(value: u8) -> Option<Self> {
        match value {
            1 => Some(Self::DatastoreLun),
            2 => Some(Self::DatastoreNfs),
            3 => Some(Self::File),
            4 => Some(Self::Object),
            5 => Some(Self::Root),
            _ => None,
        }
    }

    /// The byte.
    #[must_use]
    pub const fn as_u8(self) -> u8 {
        self as u8
    }
}

/// Longest `VolEvent` name, in bytes.
pub const MAX_NAME_LEN: usize = 255;

/// The fixed body of `VolEvent`, followed by `name_len` bytes of UTF-8.
#[derive(
    FromBytes, IntoBytes, Immutable, KnownLayout, Clone, Copy, PartialEq, Eq, Hash, Default, Debug,
)]
#[repr(C)]
pub struct VolEventBody {
    /// A [`VolEventKind`] byte.
    pub event: u8,
    /// A [`VolClass`] byte for `Create`; 0 otherwise.
    pub class: u8,
    /// Length of the name that follows.
    pub name_len: u16,
    /// The volume.
    pub vol: u32,
    /// The size in 4 KiB blocks (`Create`, `Resize`); 0 otherwise.
    pub size_blocks: u64,
    /// The stripe geometry at creation (`Create`); 0 otherwise.
    pub stripe_bytes: u32,
    /// Retention in seconds (`SnapTag`); 0 otherwise.
    pub retention_s: u32,
    /// The volume's uuid (`Create`) or the snapshot's (`SnapTag`); nil otherwise.
    pub uuid: Uuid,
    /// Reserved, zero.
    pub rsvd: [u8; 24],
}

/// A volume event, ordered inside a delta before any chunk that references
/// the volume. Fields the event does not use are zero, nil or empty.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VolEvent {
    /// What happened.
    pub event: VolEventKind,
    /// The volume.
    pub vol: u32,
    /// The class, for `Create`.
    pub class: Option<VolClass>,
    /// The size in blocks, for `Create` and `Resize`.
    pub size_blocks: u64,
    /// The stripe geometry, for `Create`.
    pub stripe_bytes: u32,
    /// Retention in seconds, for `SnapTag`.
    pub retention_s: u32,
    /// The volume's or the snapshot's uuid.
    pub uuid: Uuid,
    /// The name, for `Create` and `SnapTag`; at most [`MAX_NAME_LEN`] bytes.
    pub name: String,
}

/// A stripe image: the chunk payload, as a zero-copy view of the receive
/// buffer. The image is verified as a stripe only when the engine lands it
/// into aligned staging ([`Chunk::summary`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Chunk {
    /// The image as it crossed the wire.
    pub image: Bytes,
    /// The codec of `image`.
    pub codec: Codec,
    /// The decoded length, a stripe size; equal to `image.len()` for
    /// [`Codec::Raw`].
    pub raw_len: u32,
}

impl Chunk {
    /// A raw (uncompressed) chunk over an image of a stripe size.
    pub fn raw(image: Bytes) -> Result<Self, StreamError> {
        let raw_len = bounded_len(image.len())?;
        if !stripe_size_ok(u64::from(raw_len)) {
            return Err(bad("raw_len", u64::from(raw_len)));
        }
        Ok(Self {
            image,
            codec: Codec::Raw,
            raw_len,
        })
    }

    /// Checks the codec, the lengths and their agreement.
    pub fn validate(&self) -> Result<(), StreamError> {
        let wire = bounded_len(self.image.len())?;
        if !stripe_size_ok(u64::from(self.raw_len)) {
            return Err(bad("raw_len", u64::from(self.raw_len)));
        }
        match self.codec {
            Codec::Raw => {
                if wire != self.raw_len {
                    return Err(bad("raw_len", u64::from(self.raw_len)));
                }
            }
            Codec::Lz4 | Codec::Zstd => {
                if wire == 0 {
                    return Err(bad("wire_len", 0));
                }
            }
        }
        Ok(())
    }

    /// The stripe header at the front of a raw image (a 64-byte copy that
    /// works at any alignment), verified.
    pub fn stripe_hdr(&self) -> Result<StripeHdr, StreamError> {
        if self.codec != Codec::Raw {
            return Err(bad("codec", u64::from(self.codec.as_u8())));
        }
        if self.image.len() < STRIPE_HDR_SIZE {
            return Err(StreamError::BodyTooShort {
                needed: STRIPE_HDR_SIZE,
                have: self.image.len(),
            });
        }
        Ok(StripeHdr::read(&self.image[..STRIPE_HDR_SIZE])?)
    }

    /// The verified summary of a raw image. Needs an 8-aligned image: a
    /// `Bytes` cut from a receive buffer at an arbitrary offset fails with
    /// `Misaligned`, in which case the engine parses after landing the image
    /// into its aligned staging buffer.
    pub fn summary(&self) -> Result<StripeSummary<'_>, StreamError> {
        if self.codec != Codec::Raw {
            return Err(bad("codec", u64::from(self.codec.as_u8())));
        }
        Ok(StripeSummary::parse(&self.image)?)
    }
}

/// Hole records, ordered before any chunk that references the volume.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Holes {
    /// At least one record, each validated.
    pub records: Vec<HoleRecord>,
}

/// The fixed body of `DeltaEnd`.
#[derive(
    FromBytes, IntoBytes, Immutable, KnownLayout, Clone, Copy, PartialEq, Eq, Hash, Default, Debug,
)]
#[repr(C)]
pub struct DeltaEndBody {
    /// BLAKE3 over the chunk images (summary and data) in index order.
    pub digest: [u8; 32],
    /// Chunks sent.
    pub chunks: u32,
    /// Reserved, zero.
    pub rsvd: u32,
    /// Stripe image bytes sent, the sum of the chunks' `raw_len`.
    pub bytes: u64,
}

/// The end of a delta, sent after the sender's superblock commit.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct DeltaEnd {
    /// BLAKE3 over the chunk images in index order.
    pub digest: [u8; 32],
    /// Chunks sent.
    pub chunks: u32,
    /// Stripe image bytes sent, the sum of the chunks' `raw_len`; the
    /// receiver compares it with what it landed, decoded, so a codec never
    /// changes it.
    pub bytes: u64,
}

/// The body of `Landed`.
#[derive(
    FromBytes, IntoBytes, Immutable, KnownLayout, Clone, Copy, PartialEq, Eq, Hash, Default, Debug,
)]
#[repr(C)]
pub struct LandedBody {
    /// The chunk that landed.
    pub idx: u32,
    /// Credits granted with this acknowledgement.
    pub credits: u32,
    /// The receiver's `landed_contig` after landing.
    pub landed_contig: u32,
    /// Reserved, zero.
    pub rsvd: u32,
}

/// A landed chunk and a credit grant.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Landed {
    /// The chunk that landed, below [`MAX_DELTA_CHUNKS`].
    pub idx: u32,
    /// Credits granted, at most [`MAX_CREDITS`].
    pub credits: u32,
    /// The receiver's `landed_contig` after landing, at most
    /// [`MAX_DELTA_CHUNKS`].
    pub landed_contig: u32,
}

/// The body of `Committed`.
#[derive(
    FromBytes, IntoBytes, Immutable, KnownLayout, Clone, Copy, PartialEq, Eq, Hash, Default, Debug,
)]
#[repr(C)]
pub struct CommittedBody {
    /// The CP the replica is now at.
    pub applied_cp_seq: u64,
    /// The sender epoch of that CP.
    pub applied_epoch: u64,
}

/// The receiver committed the delta's CP.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Committed {
    /// The CP the replica is now at, at most [`MAX_CP_SEQ`].
    pub applied_cp_seq: u64,
    /// The sender epoch of that CP.
    pub applied_epoch: u64,
}

/// Why a side aborts a delta.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[repr(u8)]
pub enum AbortReason {
    /// A chunk's payload CRC failed; `idx` names it and the sender resends.
    /// For a corrupt frame that is not a chunk, `idx` is 0 and the sender
    /// restarts the delta from `DeltaBegin`.
    Corrupt = 1,
    /// The receiver has no staging left.
    NoSpace = 2,
    /// The delta's base is not the receiver's applied CP.
    BaseMismatch = 3,
    /// The peer speaks another version.
    Version = 4,
    /// The direction epoch is stale; `detail` carries the current one.
    StaleEpoch = 5,
    /// A chunk references a volume the receiver does not know; `detail`
    /// carries the volume.
    UnknownVol = 6,
}

impl AbortReason {
    /// Decodes the byte.
    #[must_use]
    pub const fn from_u8(value: u8) -> Option<Self> {
        match value {
            1 => Some(Self::Corrupt),
            2 => Some(Self::NoSpace),
            3 => Some(Self::BaseMismatch),
            4 => Some(Self::Version),
            5 => Some(Self::StaleEpoch),
            6 => Some(Self::UnknownVol),
            _ => None,
        }
    }

    /// The byte.
    #[must_use]
    pub const fn as_u8(self) -> u8 {
        self as u8
    }
}

/// The body of `Abort`.
#[derive(
    FromBytes, IntoBytes, Immutable, KnownLayout, Clone, Copy, PartialEq, Eq, Hash, Default, Debug,
)]
#[repr(C)]
pub struct AbortBody {
    /// An [`AbortReason`] byte.
    pub reason: u8,
    /// Reserved, zero.
    pub rsvd: [u8; 3],
    /// The chunk, for `Corrupt`; 0 otherwise.
    pub idx: u32,
    /// Reason-specific detail.
    pub detail: u64,
}

/// An abort.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Abort {
    /// Why.
    pub reason: AbortReason,
    /// The chunk, for `Corrupt`; 0 otherwise.
    pub idx: u32,
    /// Reason-specific detail.
    pub detail: u64,
}

/// The body of `Ping` and `Pong`.
#[derive(
    FromBytes, IntoBytes, Immutable, KnownLayout, Clone, Copy, PartialEq, Eq, Hash, Default, Debug,
)]
#[repr(C)]
pub struct PingBody {
    /// Matches a `Pong` to its `Ping`.
    pub nonce: u64,
    /// The sender's monotonic clock, nanoseconds.
    pub mono_ns: u64,
}

/// A keepalive request.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Ping {
    /// Matches the `Pong`.
    pub nonce: u64,
    /// The sender's monotonic clock, nanoseconds.
    pub mono_ns: u64,
}

/// A keepalive reply, echoing the `Ping`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Pong {
    /// The `Ping`'s nonce.
    pub nonce: u64,
    /// The `Ping`'s clock reading, echoed.
    pub mono_ns: u64,
}

/// A typed frame.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Frame {
    /// Session open.
    Hello(Hello),
    /// Session open acknowledged with the receiver's cursor.
    HelloAck(HelloAck),
    /// A delta starts.
    DeltaBegin(DeltaBegin),
    /// A volume event inside the delta.
    VolEvent(VolEvent),
    /// One stripe image.
    Chunk(Chunk),
    /// Hole records.
    Holes(Holes),
    /// The delta ends.
    DeltaEnd(DeltaEnd),
    /// A chunk landed.
    Landed(Landed),
    /// The CP committed on the replica.
    Committed(Committed),
    /// An abort.
    Abort(Abort),
    /// Keepalive.
    Ping(Ping),
    /// Keepalive reply.
    Pong(Pong),
}

impl Frame {
    /// The frame's kind.
    #[must_use]
    pub const fn kind(&self) -> FrameKind {
        match self {
            Self::Hello(_) => FrameKind::Hello,
            Self::HelloAck(_) => FrameKind::HelloAck,
            Self::DeltaBegin(_) => FrameKind::DeltaBegin,
            Self::VolEvent(_) => FrameKind::VolEvent,
            Self::Chunk(_) => FrameKind::Chunk,
            Self::Holes(_) => FrameKind::Holes,
            Self::DeltaEnd(_) => FrameKind::DeltaEnd,
            Self::Landed(_) => FrameKind::Landed,
            Self::Committed(_) => FrameKind::Committed,
            Self::Abort(_) => FrameKind::Abort,
            Self::Ping(_) => FrameKind::Ping,
            Self::Pong(_) => FrameKind::Pong,
        }
    }

    /// The codec the payload is in: the chunk's own, `Raw` for every other kind.
    #[must_use]
    pub const fn codec(&self) -> Codec {
        match self {
            Self::Chunk(c) => c.codec,
            _ => Codec::Raw,
        }
    }

    /// The decoded payload length (`raw_len` of the header).
    #[must_use]
    pub fn raw_len(&self) -> usize {
        match self {
            Self::Chunk(c) => c.raw_len as usize,
            _ => self.payload_len(),
        }
    }

    /// The payload length on the wire.
    #[must_use]
    pub fn payload_len(&self) -> usize {
        match self {
            Self::Hello(_) => size_of::<HelloBody>(),
            Self::HelloAck(a) => {
                let (_, ranges) = a.cursor.to_wire();
                size_of::<CursorBody>() + ranges.len() * size_of::<ChunkRange>()
            }
            Self::DeltaBegin(d) => {
                size_of::<DeltaBeginBody>()
                    + d.tags.len() * size_of::<SnapTag>()
                    + d.acked.len() * size_of::<AckedBytes>()
            }
            Self::VolEvent(v) => size_of::<VolEventBody>() + v.name.len(),
            Self::Chunk(c) => c.image.len(),
            Self::Holes(h) => h.records.len() * HOLE_RECORD_SIZE,
            Self::DeltaEnd(_) => size_of::<DeltaEndBody>(),
            Self::Landed(_) => size_of::<LandedBody>(),
            Self::Committed(_) => size_of::<CommittedBody>(),
            Self::Abort(_) => size_of::<AbortBody>(),
            Self::Ping(_) | Self::Pong(_) => size_of::<PingBody>(),
        }
    }

    /// Validates the frame and appends its payload to `out`. Nothing is
    /// appended on error.
    pub fn encode_payload(&self, out: &mut BytesMut) -> Result<(), StreamError> {
        self.validate()?;
        match self {
            Self::Hello(h) => out.extend_from_slice(h.body().as_bytes()),
            Self::HelloAck(a) => {
                let (body, ranges) = a.cursor.to_wire();
                out.extend_from_slice(body.as_bytes());
                out.extend_from_slice(ranges.as_bytes());
            }
            Self::DeltaBegin(d) => {
                let (mode, from_snap, to_snap) = match d.mode {
                    DeltaMode::Steady => (MODE_STEADY, Uuid::NIL, Uuid::NIL),
                    DeltaMode::Increment { from, to } => (MODE_INCREMENT, from, to),
                };
                let body = DeltaBeginBody {
                    mode,
                    rsvd0: [0; 3],
                    chunks: d.chunks,
                    cut_age_ms: d.cut_age_ms,
                    tags: d.tags.len() as u32,
                    acked: d.acked.len() as u32,
                    rsvd1: 0,
                    bytes: d.bytes,
                    from_snap,
                    to_snap,
                    write_active: d.write_active,
                };
                out.extend_from_slice(body.as_bytes());
                out.extend_from_slice(d.tags.as_bytes());
                out.extend_from_slice(d.acked.as_bytes());
            }
            Self::VolEvent(v) => {
                let body = VolEventBody {
                    event: v.event.as_u8(),
                    class: v.class.map_or(0, VolClass::as_u8),
                    name_len: v.name.len() as u16,
                    vol: v.vol,
                    size_blocks: v.size_blocks,
                    stripe_bytes: v.stripe_bytes,
                    retention_s: v.retention_s,
                    uuid: v.uuid,
                    rsvd: [0; 24],
                };
                out.extend_from_slice(body.as_bytes());
                out.extend_from_slice(v.name.as_bytes());
            }
            Self::Chunk(c) => out.extend_from_slice(&c.image),
            Self::Holes(h) => out.extend_from_slice(h.records.as_bytes()),
            Self::DeltaEnd(e) => out.extend_from_slice(
                DeltaEndBody {
                    digest: e.digest,
                    chunks: e.chunks,
                    rsvd: 0,
                    bytes: e.bytes,
                }
                .as_bytes(),
            ),
            Self::Landed(l) => out.extend_from_slice(
                LandedBody {
                    idx: l.idx,
                    credits: l.credits,
                    landed_contig: l.landed_contig,
                    rsvd: 0,
                }
                .as_bytes(),
            ),
            Self::Committed(c) => out.extend_from_slice(
                CommittedBody {
                    applied_cp_seq: c.applied_cp_seq,
                    applied_epoch: c.applied_epoch,
                }
                .as_bytes(),
            ),
            Self::Abort(a) => out.extend_from_slice(
                AbortBody {
                    reason: a.reason.as_u8(),
                    rsvd: [0; 3],
                    idx: a.idx,
                    detail: a.detail,
                }
                .as_bytes(),
            ),
            Self::Ping(p) => out.extend_from_slice(
                PingBody {
                    nonce: p.nonce,
                    mono_ns: p.mono_ns,
                }
                .as_bytes(),
            ),
            Self::Pong(p) => out.extend_from_slice(
                PingBody {
                    nonce: p.nonce,
                    mono_ns: p.mono_ns,
                }
                .as_bytes(),
            ),
        }
        Ok(())
    }

    /// Checks everything the decoder would refuse: bounds, enumerations,
    /// record validity and the canonical form of fields the kind does not
    /// use, so a built frame is one the peer will accept.
    pub fn validate(&self) -> Result<(), StreamError> {
        let len = self.payload_len();
        bounded_len(len)?;
        match self {
            Self::Hello(h) => {
                Hello::from_body(&h.body())?;
            }
            Self::HelloAck(a) => {
                let (body, ranges) = a.cursor.to_wire();
                Cursor::from_wire(&body, &ranges)?;
            }
            Self::DeltaBegin(d) => {
                ResumeBitmap::check_chunks(d.chunks)?;
                check_delta_bytes(d.chunks, d.bytes)?;
                if u32::try_from(d.tags.len()).is_err() || u32::try_from(d.acked.len()).is_err() {
                    return Err(bad("tags", d.tags.len() as u64));
                }
                for tag in &d.tags {
                    if tag.uuid.is_nil() {
                        return Err(bad("tag.uuid", 0));
                    }
                    if tag.rsvd != 0 {
                        return Err(bad("tag.rsvd", u64::from(tag.rsvd)));
                    }
                }
                for acked in &d.acked {
                    if acked.vol > MAX_VOL {
                        return Err(bad("acked.vol", u64::from(acked.vol)));
                    }
                    if acked.rsvd != 0 {
                        return Err(bad("acked.rsvd", u64::from(acked.rsvd)));
                    }
                }
                if let DeltaMode::Increment { from, to } = d.mode {
                    if from.is_nil() || to.is_nil() || from == to {
                        return Err(bad("mode", u64::from(MODE_INCREMENT)));
                    }
                }
            }
            Self::VolEvent(v) => v.validate()?,
            Self::Chunk(c) => c.validate()?,
            Self::Holes(h) => {
                if h.records.is_empty() {
                    return Err(bad("records", 0));
                }
                for record in &h.records {
                    record.validate()?;
                }
            }
            Self::DeltaEnd(e) => {
                ResumeBitmap::check_chunks(e.chunks)?;
                check_delta_bytes(e.chunks, e.bytes)?;
            }
            Self::Landed(l) => {
                if l.idx >= MAX_DELTA_CHUNKS {
                    return Err(bad("idx", u64::from(l.idx)));
                }
                if l.landed_contig > MAX_DELTA_CHUNKS {
                    return Err(bad("landed_contig", u64::from(l.landed_contig)));
                }
                if l.credits > MAX_CREDITS {
                    return Err(bad("credits", u64::from(l.credits)));
                }
            }
            Self::Committed(c) => {
                if c.applied_cp_seq > MAX_CP_SEQ {
                    return Err(bad("applied_cp_seq", c.applied_cp_seq));
                }
            }
            Self::Abort(a) => {
                if a.idx >= MAX_DELTA_CHUNKS {
                    return Err(bad("idx", u64::from(a.idx)));
                }
                if a.reason != AbortReason::Corrupt && a.idx != 0 {
                    return Err(bad("idx", u64::from(a.idx)));
                }
            }
            Self::Ping(_) | Self::Pong(_) => {}
        }
        Ok(())
    }

    /// Decodes a verified frame (header checked, payload CRC checked) into
    /// its typed form. The payload is consumed without copying for chunks
    /// and copied into owned records for everything else.
    pub fn decode(hdr: &StreamHeader, payload: Bytes) -> Result<Self, StreamError> {
        let kind = hdr.frame_kind().ok_or(FormatError::BadField {
            field: "kind",
            value: u64::from(hdr.kind),
        })?;
        let codec = hdr.codec().ok_or(FormatError::BadField {
            field: "codec",
            value: u64::from(hdr.flags),
        })?;
        let wire_len = hdr.wire_len as usize;
        if payload.len() < wire_len {
            return Err(StreamError::BodyTooShort {
                needed: wire_len,
                have: payload.len(),
            });
        }
        if payload.len() > wire_len {
            return Err(StreamError::BodyTooLong {
                expected: wire_len,
                found: payload.len(),
            });
        }
        FrameMeta::from_header(hdr).check_chunk_idx(kind)?;
        if kind == FrameKind::Chunk {
            let chunk = Chunk {
                image: payload,
                codec,
                raw_len: hdr.raw_len,
            };
            chunk.validate()?;
            return Ok(Self::Chunk(chunk));
        }
        if codec != Codec::Raw {
            return Err(bad("codec", u64::from(codec.as_u8())));
        }
        let bytes: &[u8] = &payload;
        let frame = match kind {
            FrameKind::Hello => {
                let (body, rest) = fixed::<HelloBody>(bytes)?;
                finish(rest, bytes.len())?;
                Self::Hello(Hello::from_body(&body)?)
            }
            FrameKind::HelloAck => {
                let (body, rest) = fixed::<CursorBody>(bytes)?;
                zero("rsvd", &body.rsvd)?;
                if body.sparse_ranges as usize > MAX_WIRE_RANGES {
                    return Err(bad("sparse_ranges", u64::from(body.sparse_ranges)));
                }
                let (ranges, rest) =
                    records::<ChunkRange>(rest, body.sparse_ranges as usize, bytes.len())?;
                finish(rest, bytes.len())?;
                Self::HelloAck(HelloAck {
                    cursor: Cursor::from_wire(&body, &ranges)?,
                })
            }
            FrameKind::DeltaBegin => {
                let (body, rest) = fixed::<DeltaBeginBody>(bytes)?;
                zero("rsvd0", &body.rsvd0)?;
                if body.rsvd1 != 0 {
                    return Err(bad("rsvd1", u64::from(body.rsvd1)));
                }
                let (tags, rest) = records::<SnapTag>(rest, body.tags as usize, bytes.len())?;
                let (acked, rest) = records::<AckedBytes>(rest, body.acked as usize, bytes.len())?;
                finish(rest, bytes.len())?;
                let mode = match body.mode {
                    MODE_STEADY => {
                        if !body.from_snap.is_nil() || !body.to_snap.is_nil() {
                            return Err(bad("mode", u64::from(MODE_STEADY)));
                        }
                        DeltaMode::Steady
                    }
                    MODE_INCREMENT => DeltaMode::Increment {
                        from: body.from_snap,
                        to: body.to_snap,
                    },
                    other => return Err(bad("mode", u64::from(other))),
                };
                let frame = Self::DeltaBegin(DeltaBegin {
                    mode,
                    chunks: body.chunks,
                    bytes: body.bytes,
                    cut_age_ms: body.cut_age_ms,
                    write_active: body.write_active,
                    tags,
                    acked,
                });
                frame.validate()?;
                frame
            }
            FrameKind::VolEvent => {
                let (body, rest) = fixed::<VolEventBody>(bytes)?;
                zero("rsvd", &body.rsvd)?;
                let name_len = body.name_len as usize;
                if name_len > MAX_NAME_LEN {
                    return Err(bad("name_len", name_len as u64));
                }
                if rest.len() < name_len {
                    return Err(StreamError::BodyTooShort {
                        needed: size_of::<VolEventBody>() + name_len,
                        have: bytes.len(),
                    });
                }
                let (name, rest) = rest.split_at(name_len);
                finish(rest, bytes.len())?;
                let name = core::str::from_utf8(name)
                    .map_err(|_| bad("name", name_len as u64))?
                    .to_owned();
                let event =
                    VolEventKind::from_u8(body.event).ok_or(bad("event", u64::from(body.event)))?;
                let class = match (event, body.class) {
                    (VolEventKind::Create, c) => {
                        Some(VolClass::from_u8(c).ok_or(bad("class", u64::from(c)))?)
                    }
                    (_, 0) => None,
                    (_, c) => return Err(bad("class", u64::from(c))),
                };
                let event = VolEvent {
                    event,
                    vol: body.vol,
                    class,
                    size_blocks: body.size_blocks,
                    stripe_bytes: body.stripe_bytes,
                    retention_s: body.retention_s,
                    uuid: body.uuid,
                    name,
                };
                event.validate()?;
                Self::VolEvent(event)
            }
            FrameKind::Holes => {
                if bytes.len() < HOLE_RECORD_SIZE {
                    return Err(StreamError::BodyTooShort {
                        needed: HOLE_RECORD_SIZE,
                        have: bytes.len(),
                    });
                }
                if !bytes.len().is_multiple_of(HOLE_RECORD_SIZE) {
                    return Err(StreamError::BodyTooLong {
                        expected: bytes.len() / HOLE_RECORD_SIZE * HOLE_RECORD_SIZE,
                        found: bytes.len(),
                    });
                }
                let (records, rest) =
                    records::<HoleRecord>(bytes, bytes.len() / HOLE_RECORD_SIZE, bytes.len())?;
                finish(rest, bytes.len())?;
                for record in &records {
                    record.validate()?;
                }
                Self::Holes(Holes { records })
            }
            FrameKind::DeltaEnd => {
                let (body, rest) = fixed::<DeltaEndBody>(bytes)?;
                finish(rest, bytes.len())?;
                if body.rsvd != 0 {
                    return Err(bad("rsvd", u64::from(body.rsvd)));
                }
                let frame = Self::DeltaEnd(DeltaEnd {
                    digest: body.digest,
                    chunks: body.chunks,
                    bytes: body.bytes,
                });
                frame.validate()?;
                frame
            }
            FrameKind::Landed => {
                let (body, rest) = fixed::<LandedBody>(bytes)?;
                finish(rest, bytes.len())?;
                if body.rsvd != 0 {
                    return Err(bad("rsvd", u64::from(body.rsvd)));
                }
                let frame = Self::Landed(Landed {
                    idx: body.idx,
                    credits: body.credits,
                    landed_contig: body.landed_contig,
                });
                frame.validate()?;
                frame
            }
            FrameKind::Committed => {
                let (body, rest) = fixed::<CommittedBody>(bytes)?;
                finish(rest, bytes.len())?;
                let frame = Self::Committed(Committed {
                    applied_cp_seq: body.applied_cp_seq,
                    applied_epoch: body.applied_epoch,
                });
                frame.validate()?;
                frame
            }
            FrameKind::Abort => {
                let (body, rest) = fixed::<AbortBody>(bytes)?;
                finish(rest, bytes.len())?;
                zero("rsvd", &body.rsvd)?;
                let reason = AbortReason::from_u8(body.reason)
                    .ok_or(bad("reason", u64::from(body.reason)))?;
                let frame = Self::Abort(Abort {
                    reason,
                    idx: body.idx,
                    detail: body.detail,
                });
                frame.validate()?;
                frame
            }
            FrameKind::Ping => {
                let (body, rest) = fixed::<PingBody>(bytes)?;
                finish(rest, bytes.len())?;
                Self::Ping(Ping {
                    nonce: body.nonce,
                    mono_ns: body.mono_ns,
                })
            }
            FrameKind::Pong => {
                let (body, rest) = fixed::<PingBody>(bytes)?;
                finish(rest, bytes.len())?;
                Self::Pong(Pong {
                    nonce: body.nonce,
                    mono_ns: body.mono_ns,
                })
            }
            FrameKind::Chunk => unreachable!("handled above"),
        };
        Ok(frame)
    }

    /// Decodes and requires a particular kind.
    pub fn decode_as(
        hdr: &StreamHeader,
        payload: Bytes,
        expected: FrameKind,
    ) -> Result<Self, StreamError> {
        if hdr.kind != expected.as_u8() {
            return Err(StreamError::WrongKind {
                expected,
                found: hdr.kind,
            });
        }
        Self::decode(hdr, payload)
    }
}

impl VolEvent {
    /// Checks the volume, the name and the per-event rules: the fields an
    /// event uses are in range, the others are zero, nil or empty.
    pub fn validate(&self) -> Result<(), StreamError> {
        if self.vol > MAX_VOL {
            return Err(bad("vol", u64::from(self.vol)));
        }
        if self.name.len() > MAX_NAME_LEN {
            return Err(bad("name_len", self.name.len() as u64));
        }
        let size_ok = self.size_blocks >= 1 && self.size_blocks <= MAX_LBA + 1;
        match self.event {
            VolEventKind::Create => {
                if self.class.is_none() {
                    return Err(bad("class", 0));
                }
                if !size_ok {
                    return Err(bad("size_blocks", self.size_blocks));
                }
                if !stripe_size_ok(u64::from(self.stripe_bytes)) {
                    return Err(bad("stripe_bytes", u64::from(self.stripe_bytes)));
                }
                if self.uuid.is_nil() {
                    return Err(bad("uuid", 0));
                }
                if self.retention_s != 0 {
                    return Err(bad("retention_s", u64::from(self.retention_s)));
                }
            }
            VolEventKind::Resize => {
                if self.class.is_some() {
                    return Err(bad("class", 1));
                }
                if !size_ok {
                    return Err(bad("size_blocks", self.size_blocks));
                }
                must_be_zero("stripe_bytes", u64::from(self.stripe_bytes))?;
                must_be_zero("retention_s", u64::from(self.retention_s))?;
                self.unused_nil_uuid()?;
                self.unused_empty_name()?;
            }
            VolEventKind::Delete => {
                if self.class.is_some() {
                    return Err(bad("class", 1));
                }
                must_be_zero("size_blocks", self.size_blocks)?;
                must_be_zero("stripe_bytes", u64::from(self.stripe_bytes))?;
                must_be_zero("retention_s", u64::from(self.retention_s))?;
                self.unused_nil_uuid()?;
                self.unused_empty_name()?;
            }
            VolEventKind::SnapTag => {
                if self.class.is_some() {
                    return Err(bad("class", 1));
                }
                must_be_zero("size_blocks", self.size_blocks)?;
                must_be_zero("stripe_bytes", u64::from(self.stripe_bytes))?;
                if self.uuid.is_nil() {
                    return Err(bad("uuid", 0));
                }
            }
        }
        Ok(())
    }

    /// Refuses a UUID on an event that carries none.
    fn unused_nil_uuid(&self) -> Result<(), StreamError> {
        if !self.uuid.is_nil() {
            return Err(bad("uuid", 1));
        }
        Ok(())
    }

    /// Refuses a name on an event that carries none, with its length.
    fn unused_empty_name(&self) -> Result<(), StreamError> {
        if !self.name.is_empty() {
            return Err(bad("name", self.name.len() as u64));
        }
        Ok(())
    }
}

const fn bad(field: &'static str, value: u64) -> StreamError {
    StreamError::BadBody { field, value }
}

/// Refuses a non-zero value in a field the event does not use, naming it.
const fn must_be_zero(field: &'static str, value: u64) -> Result<(), StreamError> {
    if value != 0 {
        return Err(bad(field, value));
    }
    Ok(())
}

fn zero(field: &'static str, bytes: &[u8]) -> Result<(), StreamError> {
    match bytes.iter().position(|b| *b != 0) {
        None => Ok(()),
        Some(i) => Err(bad(field, u64::from(bytes[i]))),
    }
}

/// `true` for a stripe image length: whole blocks within the probe range.
const fn stripe_size_ok(bytes: u64) -> bool {
    bytes >= MIN_STRIPE_BYTES as u64
        && bytes <= MAX_STRIPE_BYTES as u64
        && bytes.is_multiple_of(BLOCK_SIZE as u64)
}

/// A delta of `chunks` stripe images carries between `chunks` smallest and
/// `chunks` largest images.
const fn check_delta_bytes(chunks: u32, bytes: u64) -> Result<(), StreamError> {
    let min = chunks as u64 * MIN_STRIPE_BYTES as u64;
    let max = chunks as u64 * MAX_STRIPE_BYTES as u64;
    if bytes < min || bytes > max {
        return Err(bad("bytes", bytes));
    }
    Ok(())
}

fn bounded_len(len: usize) -> Result<u32, StreamError> {
    u32::try_from(len)
        .ok()
        .filter(|len| *len <= MAX_WIRE_PAYLOAD)
        .ok_or(StreamError::PayloadTooLarge {
            len,
            max: MAX_WIRE_PAYLOAD,
        })
}

/// Copies a fixed body off the front of `bytes`.
fn fixed<T: FromBytes>(bytes: &[u8]) -> Result<(T, &[u8]), StreamError> {
    T::read_from_prefix(bytes).map_err(|_| StreamError::BodyTooShort {
        needed: size_of::<T>(),
        have: bytes.len(),
    })
}

/// Copies `count` fixed records off the front of `rest`. The length is
/// checked before anything is allocated, so a hostile count costs nothing.
fn records<T: FromBytes>(
    rest: &[u8],
    count: usize,
    total: usize,
) -> Result<(Vec<T>, &[u8]), StreamError> {
    let needed = count
        .checked_mul(size_of::<T>())
        .ok_or(StreamError::BodyTooShort {
            needed: usize::MAX,
            have: total,
        })?;
    if rest.len() < needed {
        return Err(StreamError::BodyTooShort {
            needed: total - rest.len() + needed,
            have: total,
        });
    }
    let (raw, rest) = rest.split_at(needed);
    let mut out = Vec::with_capacity(count);
    for chunk in raw.chunks_exact(size_of::<T>()) {
        out.push(
            T::read_from_bytes(chunk).map_err(|_| StreamError::BodyTooShort {
                needed,
                have: raw.len(),
            })?,
        );
    }
    Ok((out, rest))
}

/// Refuses bytes a body does not account for.
fn finish(rest: &[u8], total: usize) -> Result<(), StreamError> {
    if rest.is_empty() {
        Ok(())
    } else {
        Err(StreamError::BodyTooLong {
            expected: total - rest.len(),
            found: total,
        })
    }
}

const _: () = {
    assert!(size_of::<HelloBody>() == 64);
    assert!(align_of::<HelloBody>() == 8);
    assert!(offset_of!(HelloBody, node_id) == 0);
    assert!(offset_of!(HelloBody, pipe_id) == 16);
    assert!(offset_of!(HelloBody, direction_epoch) == 32);
    assert!(offset_of!(HelloBody, capabilities) == 40);
    assert!(offset_of!(HelloBody, max_payload) == 48);
    assert!(offset_of!(HelloBody, initial_credits) == 52);
    assert!(offset_of!(HelloBody, rsvd) == 56);

    assert!(size_of::<SnapTag>() == 24);
    assert!(size_of::<AckedBytes>() == 16);
    assert!(size_of::<DeltaBeginBody>() == 192);
    assert!(align_of::<DeltaBeginBody>() == 8);
    assert!(offset_of!(DeltaBeginBody, mode) == 0);
    assert!(offset_of!(DeltaBeginBody, chunks) == 4);
    assert!(offset_of!(DeltaBeginBody, cut_age_ms) == 8);
    assert!(offset_of!(DeltaBeginBody, tags) == 12);
    assert!(offset_of!(DeltaBeginBody, acked) == 16);
    assert!(offset_of!(DeltaBeginBody, rsvd1) == 20);
    assert!(offset_of!(DeltaBeginBody, bytes) == 24);
    assert!(offset_of!(DeltaBeginBody, from_snap) == 32);
    assert!(offset_of!(DeltaBeginBody, to_snap) == 48);
    assert!(offset_of!(DeltaBeginBody, write_active) == 64);

    assert!(size_of::<VolEventBody>() == 64);
    assert!(offset_of!(VolEventBody, event) == 0);
    assert!(offset_of!(VolEventBody, class) == 1);
    assert!(offset_of!(VolEventBody, name_len) == 2);
    assert!(offset_of!(VolEventBody, vol) == 4);
    assert!(offset_of!(VolEventBody, size_blocks) == 8);
    assert!(offset_of!(VolEventBody, stripe_bytes) == 16);
    assert!(offset_of!(VolEventBody, retention_s) == 20);
    assert!(offset_of!(VolEventBody, uuid) == 24);
    assert!(offset_of!(VolEventBody, rsvd) == 40);

    assert!(size_of::<DeltaEndBody>() == 48);
    assert!(size_of::<LandedBody>() == 16);
    assert!(size_of::<CommittedBody>() == 16);
    assert!(size_of::<AbortBody>() == 16);
    assert!(size_of::<PingBody>() == 16);

    assert!(VolEventKind::Create.as_u8() == 1 && VolEventKind::SnapTag.as_u8() == 4);
    assert!(matches!(
        VolEventKind::from_u8(2),
        Some(VolEventKind::Resize)
    ));
    assert!(VolEventKind::from_u8(0).is_none() && VolEventKind::from_u8(5).is_none());
    assert!(VolClass::DatastoreLun.as_u8() == 1 && VolClass::Root.as_u8() == 5);
    assert!(matches!(VolClass::from_u8(3), Some(VolClass::File)));
    assert!(VolClass::from_u8(0).is_none() && VolClass::from_u8(6).is_none());
    assert!(AbortReason::Corrupt.as_u8() == 1 && AbortReason::UnknownVol.as_u8() == 6);
    assert!(matches!(
        AbortReason::from_u8(5),
        Some(AbortReason::StaleEpoch)
    ));
    assert!(AbortReason::from_u8(0).is_none() && AbortReason::from_u8(7).is_none());
    assert!(stripe_size_ok(MIN_STRIPE_BYTES as u64) && stripe_size_ok(MAX_STRIPE_BYTES as u64));
    assert!(!stripe_size_ok(MIN_STRIPE_BYTES as u64 - 1) && !stripe_size_ok(8193));
};
