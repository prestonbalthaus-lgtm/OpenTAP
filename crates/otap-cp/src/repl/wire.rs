//! The replication link's framing and its session messages (Step 6C).
//!
//! # Link frames
//!
//! Every message crosses a link inside one frame:
//!
//! ```text
//! offset 0   magic    u32  "OTRL"
//! offset 4   len      u32  payload bytes, at most MAX_LINK_PAYLOAD
//! offset 8   crc32c   u32  CRC-32C of the payload
//! offset 12  payload  len bytes
//! ```
//!
//! [`FrameReader`] is fed the byte stream as it arrives and yields whole,
//! verified payloads. A bad magic (garbage, or a peer that is not an
//! OpenTAP replication peer), a length above the bound (refused before
//! anything is allocated for it) or a CRC mismatch poisons the reader: the
//! stream cannot be resynchronised, and the connection is dropped.
//! Bytes left over when the connection ends are a truncated frame, which
//! [`FrameReader::finish`] reports.
//!
//! # Messages
//!
//! The first payload byte says what the message is:
//!
//! | Byte | Message | Body |
//! |---|---|---|
//! | 1 | [`Handshake`] | the protocol version, the role, the pool, the volumes, the snapshots and the receiver's base |
//! | 2 | [`Refusal`] | why the peer will not go on, for the operator; the connection closes after it |
//! | 3 | a stream frame | one `otap-stream` frame, header and payload, exactly as the stream's encoder made it |
//!
//! The handshake is the first message each way: the side that dialed
//! speaks first, the side that accepted answers with its own handshake or
//! a refusal. A peer of another protocol version is refused with
//! [`RefuseCode::Version`] before anything else of its handshake is read.

use bytes::Bytes;
use otap_format::checksum::crc32c;
use otap_format::{Uuid, MAX_WIRE_PAYLOAD, STREAM_HEADER_SIZE};

use core::fmt;

/// The replication session protocol version, carried by every handshake.
/// A link frame or message change bumps it.
pub const REPL_PROTOCOL_VERSION: u16 = 1;

/// Link frame magic: `OTRL`.
pub const LINK_MAGIC: u32 = u32::from_le_bytes(*b"OTRL");

/// Bytes of a link frame's header.
pub const LINK_HEADER_BYTES: usize = 12;

/// The largest payload a link frame may carry: one stream frame of the
/// largest stream payload, with its header and the message byte, plus room
/// for a handshake's names.
pub const MAX_LINK_PAYLOAD: usize = MAX_WIRE_PAYLOAD as usize + STREAM_HEADER_SIZE + 1024;

/// Handshake magic: `OTRH`.
pub const HANDSHAKE_MAGIC: u32 = u32::from_le_bytes(*b"OTRH");

/// The volume index a handshake carries when the sender has none (a
/// destination that does not exist yet).
pub const NO_VOLUME: u32 = u32::MAX;

/// The longest name a handshake carries.
pub const MAX_HANDSHAKE_NAME: usize = 255;

/// The longest text a refusal carries.
pub const MAX_REFUSAL_TEXT: usize = 1024;

const MSG_HANDSHAKE: u8 = 1;
const MSG_REFUSE: u8 = 2;
const MSG_STREAM: u8 = 3;

/// Bytes of a handshake before its names.
const HANDSHAKE_FIXED: usize = 96;

/// Why bytes off a link were refused.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum WireError {
    /// A frame did not start with [`LINK_MAGIC`]: garbage, or not an
    /// OpenTAP replication peer.
    BadMagic {
        /// What was found.
        found: u32,
    },
    /// A frame announced a payload above [`MAX_LINK_PAYLOAD`].
    TooLarge {
        /// The length announced.
        len: usize,
        /// The bound.
        max: usize,
    },
    /// A frame's payload did not match its CRC.
    BadCrc {
        /// The CRC computed.
        computed: u32,
        /// The CRC the frame carried.
        stored: u32,
    },
    /// The connection ended inside a frame.
    Truncated {
        /// Bytes of the frame received.
        have: usize,
        /// Bytes the frame needed.
        need: usize,
    },
    /// A message's body is malformed: the field and its value.
    BadMessage {
        /// The field.
        field: &'static str,
        /// Its value.
        value: u64,
    },
}

impl fmt::Display for WireError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::BadMagic { found } => write!(
                f,
                "a frame starts with {found:#010x}, not the replication magic: garbage, or not an \
                 OpenTAP replication peer"
            ),
            Self::TooLarge { len, max } => {
                write!(f, "a frame announces {len} bytes, above the bound of {max}")
            }
            Self::BadCrc { computed, stored } => write!(
                f,
                "a frame's payload crc is {computed:#010x}, the frame says {stored:#010x}"
            ),
            Self::Truncated { have, need } => write!(
                f,
                "the connection ended inside a frame ({have} of {need} bytes received)"
            ),
            Self::BadMessage { field, value } => {
                write!(f, "a message's `{field}` holds {value:#x}")
            }
        }
    }
}

impl std::error::Error for WireError {}

const fn bad(field: &'static str, value: u64) -> WireError {
    WireError::BadMessage { field, value }
}

/// One link frame around `payload`, refused above [`MAX_LINK_PAYLOAD`].
pub fn encode_frame(payload: &[u8]) -> Result<Vec<u8>, WireError> {
    if payload.len() > MAX_LINK_PAYLOAD {
        return Err(WireError::TooLarge {
            len: payload.len(),
            max: MAX_LINK_PAYLOAD,
        });
    }
    let mut out = Vec::with_capacity(LINK_HEADER_BYTES + payload.len());
    out.extend_from_slice(&LINK_MAGIC.to_le_bytes());
    out.extend_from_slice(&(payload.len() as u32).to_le_bytes());
    out.extend_from_slice(&crc32c(payload).to_le_bytes());
    out.extend_from_slice(payload);
    Ok(out)
}

/// Turns the bytes of a link into verified payloads.
#[derive(Debug, Default)]
pub struct FrameReader {
    buf: Vec<u8>,
    poisoned: Option<WireError>,
    frames: u64,
}

impl FrameReader {
    /// An empty reader.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Appends received bytes; a poisoned reader drops them.
    pub fn feed(&mut self, bytes: &[u8]) {
        if self.poisoned.is_none() {
            self.buf.extend_from_slice(bytes);
        }
    }

    /// Bytes buffered and not yet yielded.
    #[must_use]
    pub fn pending(&self) -> usize {
        self.buf.len()
    }

    /// Frames yielded so far.
    #[must_use]
    pub const fn frames(&self) -> u64 {
        self.frames
    }

    /// The next whole payload, `Ok(None)` while more bytes are needed. A
    /// bad frame poisons the reader: the same error comes back on every
    /// later call.
    pub fn next_payload(&mut self) -> Result<Option<Vec<u8>>, WireError> {
        if let Some(e) = &self.poisoned {
            return Err(e.clone());
        }
        if self.buf.len() < LINK_HEADER_BYTES {
            return Ok(None);
        }
        let word = |at: usize| {
            u32::from_le_bytes([
                self.buf[at],
                self.buf[at + 1],
                self.buf[at + 2],
                self.buf[at + 3],
            ])
        };
        let magic = word(0);
        if magic != LINK_MAGIC {
            return Err(self.poison(WireError::BadMagic { found: magic }));
        }
        let len = word(4) as usize;
        if len > MAX_LINK_PAYLOAD {
            return Err(self.poison(WireError::TooLarge {
                len,
                max: MAX_LINK_PAYLOAD,
            }));
        }
        let stored = word(8);
        if self.buf.len() < LINK_HEADER_BYTES + len {
            return Ok(None);
        }
        let payload = self.buf[LINK_HEADER_BYTES..LINK_HEADER_BYTES + len].to_vec();
        let computed = crc32c(&payload);
        if computed != stored {
            return Err(self.poison(WireError::BadCrc { computed, stored }));
        }
        self.buf.drain(..LINK_HEADER_BYTES + len);
        self.frames += 1;
        Ok(Some(payload))
    }

    /// What the end of the connection means: `Ok` when it fell between
    /// frames, [`WireError::Truncated`] when it fell inside one (the error
    /// that poisoned the reader, if any, comes first).
    pub fn finish(&self) -> Result<(), WireError> {
        if let Some(e) = &self.poisoned {
            return Err(e.clone());
        }
        if self.buf.is_empty() {
            return Ok(());
        }
        let need = if self.buf.len() >= LINK_HEADER_BYTES {
            let len = u32::from_le_bytes([self.buf[4], self.buf[5], self.buf[6], self.buf[7]]);
            LINK_HEADER_BYTES + len as usize
        } else {
            LINK_HEADER_BYTES
        };
        Err(WireError::Truncated {
            have: self.buf.len(),
            need,
        })
    }

    fn poison(&mut self, e: WireError) -> WireError {
        self.buf.clear();
        self.poisoned = Some(e.clone());
        e
    }
}

/// Which end of the replication stream a handshake speaks for.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Role {
    /// The side that sends the delta (a push dials as the source).
    Source,
    /// The side that lands it (a pull dials as the sink).
    Sink,
}

impl Role {
    const fn as_u8(self) -> u8 {
        match self {
            Self::Source => 1,
            Self::Sink => 2,
        }
    }

    const fn from_u8(v: u8) -> Option<Self> {
        match v {
            1 => Some(Self::Source),
            2 => Some(Self::Sink),
            _ => None,
        }
    }

    /// The role as a word.
    #[must_use]
    pub const fn word(self) -> &'static str {
        match self {
            Self::Source => "source",
            Self::Sink => "sink",
        }
    }
}

/// The opening message each way.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Handshake {
    /// The sender's protocol version.
    pub version: u16,
    /// The sender's role.
    pub role: Role,
    /// `false` for the dialer's request, `true` for the acceptor's answer.
    pub accept: bool,
    /// The sender's pool.
    pub agg_id: Uuid,
    /// The sender's volume index, or [`NO_VOLUME`].
    pub volume_index: u32,
    /// From a source: the snapshot the delta ends at.
    pub snapshot: Uuid,
    /// From a source: that snapshot's CP.
    pub snapshot_cp: u64,
    /// From a sink: the snapshot the destination holds as its committed
    /// base (the source's identity), nil when it holds none.
    pub base: Uuid,
    /// From a sink: that base's CP in the source's numbering, 0 when
    /// unknown.
    pub base_cp: u64,
    /// The sender's own volume name (may be empty).
    pub volume: String,
    /// The volume the sender means on the peer: a push names the
    /// destination, a pull names the source volume.
    pub peer_volume: String,
    /// A pull names the snapshot it asks for; a source names its target.
    pub snapshot_name: String,
}

impl Handshake {
    /// A handshake of this protocol version with every identity nil.
    #[must_use]
    pub fn new(role: Role, accept: bool, agg_id: Uuid) -> Self {
        Self {
            version: REPL_PROTOCOL_VERSION,
            role,
            accept,
            agg_id,
            volume_index: NO_VOLUME,
            snapshot: Uuid::NIL,
            snapshot_cp: 0,
            base: Uuid::NIL,
            base_cp: 0,
            volume: String::new(),
            peer_volume: String::new(),
            snapshot_name: String::new(),
        }
    }

    fn encode_body(&self, out: &mut Vec<u8>) -> Result<(), WireError> {
        for (field, name) in [
            ("volume", &self.volume),
            ("peer_volume", &self.peer_volume),
            ("snapshot_name", &self.snapshot_name),
        ] {
            if name.len() > MAX_HANDSHAKE_NAME {
                return Err(bad(field, name.len() as u64));
            }
        }
        out.extend_from_slice(&HANDSHAKE_MAGIC.to_le_bytes());
        out.extend_from_slice(&self.version.to_le_bytes());
        out.push(self.role.as_u8());
        out.push(u8::from(self.accept));
        out.extend_from_slice(self.agg_id.as_array());
        out.extend_from_slice(&self.volume_index.to_le_bytes());
        out.extend_from_slice(&[0u8; 4]);
        out.extend_from_slice(self.snapshot.as_array());
        out.extend_from_slice(&self.snapshot_cp.to_le_bytes());
        out.extend_from_slice(self.base.as_array());
        out.extend_from_slice(&self.base_cp.to_le_bytes());
        out.extend_from_slice(&(self.volume.len() as u16).to_le_bytes());
        out.extend_from_slice(&(self.peer_volume.len() as u16).to_le_bytes());
        out.extend_from_slice(&(self.snapshot_name.len() as u16).to_le_bytes());
        out.extend_from_slice(&[0u8; 10]);
        out.extend_from_slice(self.volume.as_bytes());
        out.extend_from_slice(self.peer_volume.as_bytes());
        out.extend_from_slice(self.snapshot_name.as_bytes());
        Ok(())
    }

    /// The protocol version of a handshake body, read before anything
    /// else so a peer of another version is refused by version, whatever
    /// the rest of its body looks like.
    pub fn peek_version(body: &[u8]) -> Result<u16, WireError> {
        if body.len() < 6 {
            return Err(bad("handshake_len", body.len() as u64));
        }
        let magic = u32::from_le_bytes([body[0], body[1], body[2], body[3]]);
        if magic != HANDSHAKE_MAGIC {
            return Err(bad("handshake_magic", u64::from(magic)));
        }
        Ok(u16::from_le_bytes([body[4], body[5]]))
    }

    fn decode_body(body: &[u8]) -> Result<Self, WireError> {
        let version = Self::peek_version(body)?;
        if version != REPL_PROTOCOL_VERSION {
            return Err(bad("version", u64::from(version)));
        }
        if body.len() < HANDSHAKE_FIXED {
            return Err(bad("handshake_len", body.len() as u64));
        }
        let u16_at = |at: usize| u16::from_le_bytes([body[at], body[at + 1]]);
        let u32_at =
            |at: usize| u32::from_le_bytes([body[at], body[at + 1], body[at + 2], body[at + 3]]);
        let u64_at = |at: usize| {
            let mut b = [0u8; 8];
            b.copy_from_slice(&body[at..at + 8]);
            u64::from_le_bytes(b)
        };
        let uuid_at = |at: usize| {
            let mut b = [0u8; 16];
            b.copy_from_slice(&body[at..at + 16]);
            Uuid::from_bytes(b)
        };
        let role = Role::from_u8(body[6]).ok_or(bad("role", u64::from(body[6])))?;
        let accept = match body[7] {
            0 => false,
            1 => true,
            v => return Err(bad("accept", u64::from(v))),
        };
        if body[28..32] != [0u8; 4] {
            return Err(bad("rsvd0", u64::from(u32_at(28))));
        }
        if body[86..96].iter().any(|b| *b != 0) {
            return Err(bad("rsvd1", 1));
        }
        let lens = [
            u16_at(80) as usize,
            u16_at(82) as usize,
            u16_at(84) as usize,
        ];
        let mut at = HANDSHAKE_FIXED;
        let mut names = Vec::with_capacity(3);
        for (field, len) in ["volume", "peer_volume", "snapshot_name"].iter().zip(lens) {
            if len > MAX_HANDSHAKE_NAME {
                return Err(bad(field, len as u64));
            }
            let end = at
                .checked_add(len)
                .filter(|end| *end <= body.len())
                .ok_or(bad("handshake_len", body.len() as u64))?;
            let name = core::str::from_utf8(&body[at..end]).map_err(|_| bad(field, len as u64))?;
            names.push(name.to_string());
            at = end;
        }
        if at != body.len() {
            return Err(bad("handshake_len", body.len() as u64));
        }
        let snapshot_name = names.pop().unwrap_or_default();
        let peer_volume = names.pop().unwrap_or_default();
        let volume = names.pop().unwrap_or_default();
        Ok(Self {
            version,
            role,
            accept,
            agg_id: uuid_at(8),
            volume_index: u32_at(24),
            snapshot: uuid_at(32),
            snapshot_cp: u64_at(48),
            base: uuid_at(56),
            base_cp: u64_at(72),
            volume,
            peer_volume,
            snapshot_name,
        })
    }
}

/// Why a peer refused to go on.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum RefuseCode {
    /// Another protocol version.
    Version,
    /// Busy: another transfer of the volume runs; try again later.
    Busy,
    /// No pool is booted on the peer.
    NoPool,
    /// The volume or the snapshot named does not exist.
    NotFound,
    /// The request is malformed or contradicts itself.
    BadRequest,
    /// The request contradicts the peer's state: a destination that is
    /// online or not a replica, a base the source does not hold, the same
    /// pool on both ends.
    Conflict,
    /// The snapshot or the base is quarantined (damaged).
    Quarantined,
    /// The peer failed.
    Internal,
}

impl RefuseCode {
    const fn as_u16(self) -> u16 {
        match self {
            Self::Version => 1,
            Self::Busy => 2,
            Self::NoPool => 3,
            Self::NotFound => 4,
            Self::BadRequest => 5,
            Self::Conflict => 6,
            Self::Quarantined => 7,
            Self::Internal => 8,
        }
    }

    const fn from_u16(v: u16) -> Option<Self> {
        match v {
            1 => Some(Self::Version),
            2 => Some(Self::Busy),
            3 => Some(Self::NoPool),
            4 => Some(Self::NotFound),
            5 => Some(Self::BadRequest),
            6 => Some(Self::Conflict),
            7 => Some(Self::Quarantined),
            8 => Some(Self::Internal),
            _ => None,
        }
    }

    /// The code as a word.
    #[must_use]
    pub const fn word(self) -> &'static str {
        match self {
            Self::Version => "version",
            Self::Busy => "busy",
            Self::NoPool => "no pool",
            Self::NotFound => "not found",
            Self::BadRequest => "bad request",
            Self::Conflict => "conflict",
            Self::Quarantined => "quarantined",
            Self::Internal => "internal",
        }
    }
}

/// A refusal: the code and the text for the operator.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Refusal {
    /// Why, as a class.
    pub code: RefuseCode,
    /// Why, for the operator (at most [`MAX_REFUSAL_TEXT`] bytes; longer
    /// text is cut at a character boundary).
    pub text: String,
}

impl Refusal {
    /// A refusal; the text is cut to the bound.
    #[must_use]
    pub fn new(code: RefuseCode, text: impl Into<String>) -> Self {
        let mut text = text.into();
        if text.len() > MAX_REFUSAL_TEXT {
            let mut cut = MAX_REFUSAL_TEXT;
            while !text.is_char_boundary(cut) {
                cut -= 1;
            }
            text.truncate(cut);
        }
        Self { code, text }
    }
}

impl fmt::Display for Refusal {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "the peer refused ({}): {}", self.code.word(), self.text)
    }
}

/// What one link frame carries.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Message {
    /// The opening message.
    Handshake(Handshake),
    /// A refusal; the connection closes after it.
    Refuse(Refusal),
    /// One `otap-stream` frame, header and payload.
    Stream(Bytes),
}

impl Message {
    /// The payload of the link frame that carries the message.
    pub fn encode(&self) -> Result<Vec<u8>, WireError> {
        let mut out = Vec::new();
        match self {
            Self::Handshake(h) => {
                out.push(MSG_HANDSHAKE);
                h.encode_body(&mut out)?;
            }
            Self::Refuse(r) => {
                if r.text.len() > MAX_REFUSAL_TEXT {
                    return Err(bad("text_len", r.text.len() as u64));
                }
                out.push(MSG_REFUSE);
                out.extend_from_slice(&r.code.as_u16().to_le_bytes());
                out.extend_from_slice(&(r.text.len() as u16).to_le_bytes());
                out.extend_from_slice(r.text.as_bytes());
            }
            Self::Stream(frame) => {
                if frame.len() < STREAM_HEADER_SIZE {
                    return Err(bad("stream_len", frame.len() as u64));
                }
                out.reserve(1 + frame.len());
                out.push(MSG_STREAM);
                out.extend_from_slice(frame);
            }
        }
        if out.len() > MAX_LINK_PAYLOAD {
            return Err(WireError::TooLarge {
                len: out.len(),
                max: MAX_LINK_PAYLOAD,
            });
        }
        Ok(out)
    }

    /// The message a link frame's payload carries. A stream frame is only
    /// split off here; the stream's decoder verifies it.
    pub fn decode(payload: &[u8]) -> Result<Self, WireError> {
        let (&kind, body) = payload.split_first().ok_or(bad("message_len", 0))?;
        match kind {
            MSG_HANDSHAKE => Ok(Self::Handshake(Handshake::decode_body(body)?)),
            MSG_REFUSE => {
                if body.len() < 4 {
                    return Err(bad("refusal_len", body.len() as u64));
                }
                let code = u16::from_le_bytes([body[0], body[1]]);
                let code = RefuseCode::from_u16(code).ok_or(bad("code", u64::from(code)))?;
                let len = u16::from_le_bytes([body[2], body[3]]) as usize;
                if len > MAX_REFUSAL_TEXT || body.len() != 4 + len {
                    return Err(bad("text_len", len as u64));
                }
                let text = core::str::from_utf8(&body[4..]).map_err(|_| bad("text", len as u64))?;
                Ok(Self::Refuse(Refusal {
                    code,
                    text: text.to_string(),
                }))
            }
            MSG_STREAM => {
                if body.len() < STREAM_HEADER_SIZE {
                    return Err(bad("stream_len", body.len() as u64));
                }
                Ok(Self::Stream(Bytes::copy_from_slice(body)))
            }
            other => Err(bad("message", u64::from(other))),
        }
    }

    /// What the message is, as a word, for status lines and fault plans.
    #[must_use]
    pub fn word(&self) -> &'static str {
        match self {
            Self::Handshake(_) => "handshake",
            Self::Refuse(_) => "refuse",
            Self::Stream(_) => "stream",
        }
    }
}

/// The message kind of a link payload without decoding it: 1 handshake,
/// 2 refusal, 3 stream; and for a stream frame, the stream frame's kind
/// byte. What a fault plan keys on.
#[must_use]
pub fn peek_kind(payload: &[u8]) -> (u8, Option<u8>) {
    match payload.first() {
        Some(&MSG_STREAM) if payload.len() > 7 => (MSG_STREAM, Some(payload[1 + 6])),
        Some(&k) => (k, None),
        None => (0, None),
    }
}

const _: () = {
    assert!(HANDSHAKE_FIXED == 4 + 2 + 1 + 1 + 16 + 4 + 4 + 16 + 8 + 16 + 8 + 2 + 2 + 2 + 10);
    assert!(MAX_LINK_PAYLOAD < u32::MAX as usize);
};
