//! `otap-stream`: the OpenTAP replication stream, version 1 (round 3,
//! section 7.3; frame header frozen in `otap-format`).
//!
//! The crate has no sockets and no I/O. It turns typed frames into bytes and
//! bytes back into verified frames over memory buffers, tracks what a
//! receiver has landed so a delta resumes after a disconnect, and keeps the
//! credit window that paces a sender against the receiver's staging space.
//! The transport that owns connections, TLS and timers (`TlsTcp`, `FileSink`)
//! is a consumer of this crate.
//!
//! | Module | Owns |
//! |---|---|
//! | [`frame`] | the typed frames (`Hello` to `Pong`), their fixed little-endian bodies and the variable records behind them |
//! | [`codec`] | [`Encoder`] (header + payload with both CRCs) and [`Decoder`] (a byte stream in, CRC-verified frames with zero-copy `Bytes` payloads out, the 8 MiB payload bound enforced before any allocation) |
//! | [`cursor`] | [`Cursor`], the receiver's resume state keyed by `(cp_seq, cp_nonce)`, its [`ResumeBitmap`] of landed chunks, the wire form carried by `HelloAck`, and the 4 KiB [`CursorPage`] persisted per pipe |
//! | [`flow`] | [`ReceiverWindow`] and [`SenderWindow`], credits in 4 KiB units of free receiver staging |
//!
//! # Invariants
//! - A payload reaches an upper layer only after its CRC-32C matched the
//!   header ([`Decoder::next_frame`]); a bad header CRC poisons the decoder
//!   (the connection is dropped), a bad payload CRC is reported with the
//!   frame's identity so the receiver can send `Abort(Corrupt, idx)` and the
//!   sender can resend, and the stream stays in sync.
//! - Landing is idempotent by `(cp_nonce, chunk_idx)`; a `DeltaBegin` with a
//!   known `cp_seq` and a different nonce discards exactly one partial.
//! - A sender never has more bytes in flight than the credits it holds.

#![forbid(unsafe_code)]
#![deny(missing_docs)]
#![warn(rust_2018_idioms, unreachable_pub)]

use core::fmt;

use otap_format::{FormatError, FrameKind};

pub mod codec;
pub mod cursor;
pub mod flow;
pub mod frame;

pub use codec::{Decoder, Encoder, RawFrame};
pub use cursor::{
    BeginOutcome, ChunkRange, Cursor, CursorPage, LandOutcome, Landing, ResumeBitmap,
    MAX_DELTA_CHUNKS, MAX_WIRE_RANGES,
};
pub use flow::{credits_for, ReceiverWindow, SenderWindow, CREDIT_BYTES, MAX_CREDITS};
pub use frame::{
    Abort, AbortReason, AckedBytes, Chunk, Committed, DeltaBegin, DeltaEnd, DeltaMode, Frame,
    FrameMeta, Hello, HelloAck, Holes, Landed, Ping, Pong, SnapTag, VolClass, VolEvent,
    VolEventKind,
};

/// Why a frame could not be built, parsed or acted on.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum StreamError {
    /// The frame header or a fixed structure failed `otap-format`'s checks.
    Format(FormatError),
    /// A payload's CRC did not match its header. The frame was consumed and
    /// the stream is still in sync; the receiver answers `Abort(Corrupt)`.
    PayloadCrc {
        /// The frame's kind byte.
        kind: u8,
        /// The chunk index the header carried.
        chunk_idx: u32,
        /// The CP the header carried.
        cp_seq: u64,
    },
    /// A payload is larger than the decoder or the encoder allows.
    PayloadTooLarge {
        /// The payload length.
        len: usize,
        /// The bound.
        max: u32,
    },
    /// A typed body needs more bytes than the payload holds.
    BodyTooShort {
        /// Bytes needed.
        needed: usize,
        /// Bytes present.
        have: usize,
    },
    /// A typed body leaves bytes unused.
    BodyTooLong {
        /// Bytes the body accounts for.
        expected: usize,
        /// Bytes present.
        found: usize,
    },
    /// A body field holds an undefined or inconsistent value.
    BadBody {
        /// The field.
        field: &'static str,
        /// The value.
        value: u64,
    },
    /// A typed decode was asked for one kind and the header carries another.
    WrongKind {
        /// The kind expected.
        expected: FrameKind,
        /// The kind byte found.
        found: u8,
    },
    /// A send needs more credits than the window holds.
    Window {
        /// Credits needed.
        needed: u32,
        /// Credits available.
        available: u32,
    },
    /// A chunk index is at or beyond the delta's chunk count.
    ChunkOutOfRange {
        /// The index.
        idx: u32,
        /// The delta's chunk count.
        chunks: u32,
    },
    /// The cursor has no delta in flight.
    NoDelta,
    /// The delta's base is not the cursor's applied CP.
    BaseMismatch {
        /// The base the sender named.
        base: u64,
        /// The CP the receiver has applied.
        applied: u64,
    },
    /// A commit was asked for before every chunk landed.
    Incomplete {
        /// Chunks landed.
        landed: u32,
        /// Chunks in the delta.
        chunks: u32,
    },
}

impl fmt::Display for StreamError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match *self {
            Self::Format(e) => write!(f, "frame structure: {e}"),
            Self::PayloadCrc {
                kind,
                chunk_idx,
                cp_seq,
            } => write!(
                f,
                "payload crc mismatch on kind {kind} chunk {chunk_idx} of cp {cp_seq}"
            ),
            Self::PayloadTooLarge { len, max } => {
                write!(f, "payload of {len} bytes exceeds the bound of {max}")
            }
            Self::BodyTooShort { needed, have } => {
                write!(f, "body needs {needed} bytes, payload has {have}")
            }
            Self::BodyTooLong { expected, found } => {
                write!(f, "body accounts for {expected} bytes, payload has {found}")
            }
            Self::BadBody { field, value } => {
                write!(f, "body field `{field}` holds undefined value {value:#x}")
            }
            Self::WrongKind { expected, found } => {
                write!(f, "expected a {expected:?} frame, found kind {found}")
            }
            Self::Window { needed, available } => {
                write!(f, "window: {needed} credits needed, {available} available")
            }
            Self::ChunkOutOfRange { idx, chunks } => {
                write!(f, "chunk {idx} is outside a delta of {chunks} chunks")
            }
            Self::NoDelta => f.write_str("no delta is in flight"),
            Self::BaseMismatch { base, applied } => {
                write!(f, "delta base {base} is not the applied cp {applied}")
            }
            Self::Incomplete { landed, chunks } => {
                write!(f, "only {landed} of {chunks} chunks have landed")
            }
        }
    }
}

impl std::error::Error for StreamError {}

impl From<FormatError> for StreamError {
    fn from(e: FormatError) -> Self {
        Self::Format(e)
    }
}
