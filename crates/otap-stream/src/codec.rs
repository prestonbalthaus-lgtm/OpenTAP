//! The frame codec: typed frames to bytes, and a byte stream back to
//! CRC-verified frames with zero-copy payload views.
//!
//! [`Encoder`] seals a [`StreamHeader`] over a payload (both CRCs) and
//! either writes header and payload into one buffer or hands back the sealed
//! header for a payload the caller already holds (a stripe image in staging
//! that a transport sends with a vectored write). It refuses every frame
//! and every header the peer's decoder would refuse, so nothing it emits is
//! fatal on the other side.
//!
//! [`Decoder`] is fed arbitrary slices of the incoming byte stream and
//! yields one [`RawFrame`] at a time:
//! 1. once 64 bytes are buffered, the header is copied out and verified
//!    (magic, major, header CRC, kind, codec, flags, both length bounds, the
//!    CP bound); a failure poisons the decoder, because a corrupt header
//!    leaves no way to find the next frame, and the connection is dropped;
//!    a poisoned decoder repeats its error and drops further input;
//! 2. the payload bound is enforced before any space is reserved for it:
//!    `wire_len` above the decoder's limit (8 MiB by default) is refused at
//!    the header;
//! 3. once `wire_len` bytes follow, the frame is split off the buffer
//!    without copying and the payload CRC is checked; a mismatch consumes
//!    the frame, reports its identity ([`StreamError::PayloadCrc`]) and
//!    leaves the decoder in sync for the next frame. For a chunk the
//!    receiver answers `Abort(Corrupt, idx)` and the sender resends it; for
//!    any other kind there is no resend of one frame, and the receiver
//!    aborts the delta (`Abort(Corrupt, 0)`), which the sender restarts from
//!    `DeltaBegin`.
//!
//! A yielded payload shares the receive buffer's allocation with the frames
//! decoded from the same read; the transport reads into fresh space as the
//! buffer grows, so holding a chunk pins at most that read's allocation.

use bytes::{Bytes, BytesMut};
use otap_format::{
    Codec, FrameKind, FromZeros, IntoBytes, StreamHeader, MAX_WIRE_PAYLOAD, STREAM_HEADER_SIZE,
};

use crate::frame::{Frame, FrameMeta};
use crate::StreamError;

/// A frame as it came off the wire: a verified header and a CRC-verified
/// payload view.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RawFrame {
    /// The header.
    pub hdr: StreamHeader,
    /// The payload, `hdr.wire_len` bytes, CRC checked.
    pub payload: Bytes,
}

impl RawFrame {
    /// The kind; `None` only for a header built by hand with an undefined
    /// kind byte (the decoder never yields one).
    #[must_use]
    pub fn kind(&self) -> Option<FrameKind> {
        self.hdr.frame_kind()
    }

    /// The header-level context.
    #[must_use]
    pub const fn meta(&self) -> FrameMeta {
        FrameMeta::from_header(&self.hdr)
    }

    /// The typed frame.
    pub fn decode(self) -> Result<(FrameMeta, Frame), StreamError> {
        let meta = self.meta();
        let frame = Frame::decode(&self.hdr, self.payload)?;
        Ok((meta, frame))
    }
}

/// Builds frames. v0.1 sends raw payloads only; a chunk that already carries
/// a codec is passed through with its own flags.
#[derive(Debug, Clone, Copy, Default)]
pub struct Encoder;

impl Encoder {
    /// A new encoder.
    #[must_use]
    pub const fn new() -> Self {
        Self
    }

    /// The sealed header for `frame` with `payload` as its wire payload,
    /// which must be what [`Frame::encode_payload`] produces for it. The
    /// frame, the context and the sealed header are all verified the way
    /// the peer will verify them.
    pub fn header(
        &self,
        meta: &FrameMeta,
        frame: &Frame,
        payload: &[u8],
    ) -> Result<StreamHeader, StreamError> {
        frame.validate()?;
        meta.check(frame.kind())?;
        let expected = frame.payload_len();
        if payload.len() < expected {
            return Err(StreamError::BodyTooShort {
                needed: expected,
                have: payload.len(),
            });
        }
        if payload.len() > expected {
            return Err(StreamError::BodyTooLong {
                expected,
                found: payload.len(),
            });
        }
        let mut hdr = StreamHeader::new(frame.kind(), frame.codec());
        meta.apply(&mut hdr);
        match frame.codec() {
            Codec::Raw => hdr.set_payload(payload)?,
            _ => hdr.set_encoded_payload(payload, frame.raw_len())?,
        }
        hdr.seal();
        hdr.verify()?;
        Ok(hdr)
    }

    /// Appends the whole frame, header and payload, to `out`. Returns the
    /// bytes appended. Nothing is appended on error.
    pub fn encode_into(
        &self,
        meta: &FrameMeta,
        frame: &Frame,
        out: &mut BytesMut,
    ) -> Result<usize, StreamError> {
        frame.validate()?;
        meta.check(frame.kind())?;
        let start = out.len();
        out.extend_from_slice(&[0u8; STREAM_HEADER_SIZE]);
        if let Err(e) = frame.encode_payload(out) {
            out.truncate(start);
            return Err(e);
        }
        let payload_start = start + STREAM_HEADER_SIZE;
        let hdr = match self.header(meta, frame, &out[payload_start..]) {
            Ok(hdr) => hdr,
            Err(e) => {
                out.truncate(start);
                return Err(e);
            }
        };
        out[start..payload_start].copy_from_slice(hdr.as_bytes());
        Ok(out.len() - start)
    }

    /// The whole frame as one buffer. Nothing is allocated for a frame or a
    /// context that is refused: both are checked, the way the peer checks
    /// them, before room for the frame is reserved.
    pub fn encode(&self, meta: &FrameMeta, frame: &Frame) -> Result<Bytes, StreamError> {
        frame.validate()?;
        meta.check(frame.kind())?;
        let mut out = BytesMut::with_capacity(STREAM_HEADER_SIZE + frame.payload_len());
        self.encode_into(meta, frame, &mut out)?;
        Ok(out.freeze())
    }
}

/// Parses a byte stream into frames.
#[derive(Debug)]
pub struct Decoder {
    buf: BytesMut,
    max_payload: u32,
    pending: Option<StreamHeader>,
    poisoned: Option<StreamError>,
    deferred: Option<StreamError>,
    frames: u64,
    corrupt: u64,
}

impl Default for Decoder {
    fn default() -> Self {
        Self::new()
    }
}

impl Decoder {
    /// A decoder that accepts payloads up to [`MAX_WIRE_PAYLOAD`].
    #[must_use]
    pub fn new() -> Self {
        Self::with_max_payload(MAX_WIRE_PAYLOAD)
    }

    /// A decoder with a tighter payload bound (never above the format's).
    #[must_use]
    pub fn with_max_payload(max_payload: u32) -> Self {
        Self {
            buf: BytesMut::new(),
            max_payload: max_payload.min(MAX_WIRE_PAYLOAD),
            pending: None,
            poisoned: None,
            deferred: None,
            frames: 0,
            corrupt: 0,
        }
    }

    /// Appends received bytes. A poisoned decoder drops them.
    pub fn feed(&mut self, bytes: &[u8]) {
        if self.poisoned.is_none() {
            self.buf.extend_from_slice(bytes);
        }
    }

    /// The receive buffer, for a transport that reads straight into it.
    pub fn buffer(&mut self) -> &mut BytesMut {
        &mut self.buf
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

    /// Frames whose payload CRC failed so far.
    #[must_use]
    pub const fn corrupt(&self) -> u64 {
        self.corrupt
    }

    /// `true` once a bad header was seen; nothing more will be decoded.
    #[must_use]
    pub const fn is_poisoned(&self) -> bool {
        self.poisoned.is_some()
    }

    /// The bytes the decoder is waiting for before it can yield the next
    /// frame: the rest of a header, or the rest of a payload. Zero means a
    /// call to [`next_frame`](Self::next_frame) can make progress now,
    /// which includes reporting an error [`drain`](Self::drain) held back
    /// or the error that poisoned the decoder; a transport that reads
    /// `needed()` bytes before each call therefore never sits on one.
    #[must_use]
    pub fn needed(&self) -> usize {
        if self.poisoned.is_some() || self.deferred.is_some() {
            return 0;
        }
        match &self.pending {
            Some(hdr) => (hdr.wire_len as usize).saturating_sub(self.buf.len()),
            None => STREAM_HEADER_SIZE.saturating_sub(self.buf.len()),
        }
    }

    /// The next complete frame, `Ok(None)` when more bytes are needed.
    pub fn next_frame(&mut self) -> Result<Option<RawFrame>, StreamError> {
        if let Some(e) = self.poisoned {
            return Err(e);
        }
        if let Some(e) = self.deferred.take() {
            return Err(e);
        }
        if self.pending.is_none() {
            if self.buf.len() < STREAM_HEADER_SIZE {
                return Ok(None);
            }
            let hdr = match StreamHeader::read(&self.buf[..STREAM_HEADER_SIZE]) {
                Ok(hdr) => hdr,
                Err(e) => {
                    let e = StreamError::Format(e);
                    self.poisoned = Some(e);
                    return Err(e);
                }
            };
            if hdr.wire_len > self.max_payload {
                let e = StreamError::PayloadTooLarge {
                    len: hdr.wire_len as usize,
                    max: self.max_payload,
                };
                self.poisoned = Some(e);
                return Err(e);
            }
            let _ = self.buf.split_to(STREAM_HEADER_SIZE);
            self.pending = Some(hdr);
        }
        let hdr = self
            .pending
            .as_ref()
            .copied()
            .unwrap_or_else(StreamHeader::new_zeroed);
        let wire_len = hdr.wire_len as usize;
        if self.buf.len() < wire_len {
            self.buf.reserve(wire_len - self.buf.len());
            return Ok(None);
        }
        let payload = self.buf.split_to(wire_len).freeze();
        self.pending = None;
        if hdr.verify_payload(&payload).is_err() {
            self.corrupt += 1;
            return Err(StreamError::PayloadCrc {
                kind: hdr.kind,
                chunk_idx: hdr.chunk_idx,
                cp_seq: hdr.cp_seq,
            });
        }
        self.frames += 1;
        Ok(Some(RawFrame { hdr, payload }))
    }

    /// Every complete frame buffered so far. When an error interrupts the
    /// run after some frames were decoded, those frames are returned and the
    /// error is held for the next call, so nothing decoded is ever lost.
    pub fn drain(&mut self) -> Result<Vec<RawFrame>, StreamError> {
        let mut frames = Vec::new();
        loop {
            match self.next_frame() {
                Ok(Some(frame)) => frames.push(frame),
                Ok(None) => return Ok(frames),
                Err(e) if frames.is_empty() => return Err(e),
                Err(e) => {
                    if self.poisoned.is_none() {
                        self.deferred = Some(e);
                    }
                    return Ok(frames);
                }
            }
        }
    }
}
