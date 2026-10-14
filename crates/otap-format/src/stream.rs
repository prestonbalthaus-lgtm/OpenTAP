//! The 64-byte replication stream frame header (`otap-stream` v1, section
//! 7.3) and the `Holes` record.
//!
//! Every frame is `[StreamHeader][payload; wire_len]`. The header is
//! little-endian, `hdr_crc` covers its first 60 bytes and `payload_crc` covers
//! the `wire_len` payload bytes as sent (after any codec). A bad header CRC
//! drops the connection; a bad payload CRC is `Abort(Corrupt, idx)` and a
//! resend. `Chunk` payloads are stripe images (F3); `cp_nonce` and
//! `chunk_idx` make landing idempotent and let a `DeltaBegin` with a known
//! `cp_seq` and a different nonce discard exactly one partial.
//!
//! # Versioning on the wire
//! The header has no reserved bytes, so the on-disk minor-version rule
//! (unknown reserved content is ignored) does not apply here. A receiver
//! rejects every kind, codec and flag bit it does not define. A later 1.x
//! minor may add kinds, codecs or flag bits, but a sender uses them only
//! after the peer advertised them in `Hello`, so a 1.0 peer never sees them.
//! A different major is refused outright.
//!
//! The same header is written by `dump --since` to a file, so the backup
//! format and the wire format are one. Frames in a contiguous buffer are not
//! 8-aligned after a payload whose length is not a multiple of 8, so the
//! receive path uses [`StreamHeader::read`] (a 64-byte copy), not
//! [`StreamHeader::parse`].

use core::mem::{align_of, offset_of, size_of};

use zerocopy::{FromBytes, FromZeros, Immutable, IntoBytes, KnownLayout};

use crate::checksum::crc32c;
use crate::constants::{
    HOLE_RECORD_SIZE, MAX_CP_SEQ, MAX_LBA, MAX_VOL, MAX_WIRE_PAYLOAD, STREAM_HDR_CRC_OFFSET,
    STREAM_HEADER_SIZE, STREAM_MAGIC, STREAM_MAJOR, STREAM_VERSION,
};
use crate::types::{bad_field, check_crc, check_magic, check_major, copy_out, view, FormatError};

/// The record kinds of the stream. Zero is not a kind, so an all-zero header
/// never parses.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[repr(u8)]
pub enum FrameKind {
    /// Node id, pipe id, direction epoch and capabilities.
    Hello = 1,
    /// The receiver's cursor.
    HelloAck = 2,
    /// Chunk count, bytes, snapshot tags, `cut_age_ms`, the write-active set.
    DeltaBegin = 3,
    /// Create, resize, delete or snapshot-tag a volume, ordered before any
    /// chunk that references it.
    VolEvent = 4,
    /// One stripe image.
    Chunk = 5,
    /// Hole records, ordered before any chunk that references the volume.
    Holes = 6,
    /// The delta's BLAKE3 digest; sent after the sender's superblock commit.
    DeltaEnd = 7,
    /// Landed chunk index and credits.
    Landed = 8,
    /// The receiver committed the CP.
    Committed = 9,
    /// `Corrupt(idx) | NoSpace | BaseMismatch | Version | StaleEpoch | UnknownVol`.
    Abort = 10,
    /// Keepalive request.
    Ping = 11,
    /// Keepalive reply.
    Pong = 12,
}

impl FrameKind {
    /// Decodes the on-wire byte.
    #[must_use]
    pub const fn from_u8(value: u8) -> Option<Self> {
        match value {
            1 => Some(Self::Hello),
            2 => Some(Self::HelloAck),
            3 => Some(Self::DeltaBegin),
            4 => Some(Self::VolEvent),
            5 => Some(Self::Chunk),
            6 => Some(Self::Holes),
            7 => Some(Self::DeltaEnd),
            8 => Some(Self::Landed),
            9 => Some(Self::Committed),
            10 => Some(Self::Abort),
            11 => Some(Self::Ping),
            12 => Some(Self::Pong),
            _ => None,
        }
    }

    /// The on-wire byte.
    #[must_use]
    pub const fn as_u8(self) -> u8 {
        self as u8
    }
}

/// The payload codec, in the low two bits of `flags`. v0.1 sends `Raw` only;
/// compression is Phase B.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[repr(u8)]
pub enum Codec {
    /// `wire_len == raw_len`.
    Raw = 0,
    /// LZ4 block compression.
    Lz4 = 1,
    /// Zstandard compression.
    Zstd = 2,
}

impl Codec {
    /// Decodes the 2-bit field.
    #[must_use]
    pub const fn from_u8(value: u8) -> Option<Self> {
        match value {
            0 => Some(Self::Raw),
            1 => Some(Self::Lz4),
            2 => Some(Self::Zstd),
            _ => None,
        }
    }

    /// The 2-bit field.
    #[must_use]
    pub const fn as_u8(self) -> u8 {
        self as u8
    }
}

/// Mask of the codec bits in `flags`.
pub const FLAGS_CODEC_MASK: u8 = 0b11;

/// The 64-byte frame header.
#[derive(FromBytes, IntoBytes, Immutable, KnownLayout, Clone, Copy, PartialEq, Eq, Hash, Debug)]
#[repr(C)]
pub struct StreamHeader {
    /// [`STREAM_MAGIC`].
    pub magic: u32,
    /// [`STREAM_VERSION`].
    pub ver: u16,
    /// A [`FrameKind`] byte.
    pub kind: u8,
    /// The [`Codec`] in bits 0 to 1; the other bits are zero and refused.
    pub flags: u8,
    /// The relationship the frame belongs to (relationships are multiplexed
    /// on one session per aggregate pair).
    pub rel: u32,
    /// The chunk index within the delta; 0 for frames that are not chunks.
    pub chunk_idx: u32,
    /// The CP the delta ends at, at most [`MAX_CP_SEQ`].
    pub cp_seq: u64,
    /// The CP the delta starts from (the common base), at most `cp_seq`.
    pub base: u64,
    /// The nonce of `cp_seq` (the doc's `cp_id`).
    pub cp_nonce: u64,
    /// The sender's owner epoch.
    pub owner_epoch: u64,
    /// Payload bytes on the wire, at most [`MAX_WIRE_PAYLOAD`].
    pub wire_len: u32,
    /// Payload bytes after decoding, at most [`MAX_WIRE_PAYLOAD`]; equal to
    /// `wire_len` for [`Codec::Raw`].
    pub raw_len: u32,
    /// CRC-32C over the `wire_len` payload bytes.
    pub payload_crc: u32,
    /// CRC-32C over bytes `0..60` of this header.
    pub hdr_crc: u32,
}

impl StreamHeader {
    /// A zeroed header carrying the magic, the version, the kind and the codec.
    #[must_use]
    pub fn new(kind: FrameKind, codec: Codec) -> Self {
        let mut hdr = Self::new_zeroed();
        hdr.magic = STREAM_MAGIC;
        hdr.ver = STREAM_VERSION;
        hdr.kind = kind.as_u8();
        hdr.flags = codec.as_u8();
        hdr
    }

    fn bounded_len(field: &'static str, len: usize) -> Result<u32, FormatError> {
        u32::try_from(len)
            .ok()
            .filter(|len| *len <= MAX_WIRE_PAYLOAD)
            .ok_or(FormatError::OutOfRange {
                field,
                value: len as u64,
                max: u64::from(MAX_WIRE_PAYLOAD),
            })
    }

    /// Records an uncoded payload: its length as both `wire_len` and
    /// `raw_len`, and its CRC. The codec must be [`Codec::Raw`].
    pub fn set_payload(&mut self, payload: &[u8]) -> Result<(), FormatError> {
        if self.codec() != Some(Codec::Raw) {
            return Err(bad_field("codec", u64::from(self.flags)));
        }
        let wire_len = Self::bounded_len("wire_len", payload.len())?;
        self.wire_len = wire_len;
        self.raw_len = wire_len;
        self.payload_crc = crc32c(payload);
        Ok(())
    }

    /// Records an encoded payload: its wire length, the decoded length
    /// `raw_len` and its CRC. The codec must not be [`Codec::Raw`].
    pub fn set_encoded_payload(
        &mut self,
        payload: &[u8],
        raw_len: usize,
    ) -> Result<(), FormatError> {
        match self.codec() {
            None | Some(Codec::Raw) => return Err(bad_field("codec", u64::from(self.flags))),
            Some(_) => {}
        }
        let wire_len = Self::bounded_len("wire_len", payload.len())?;
        let raw_len = Self::bounded_len("raw_len", raw_len)?;
        self.wire_len = wire_len;
        self.raw_len = raw_len;
        self.payload_crc = crc32c(payload);
        Ok(())
    }

    /// The CRC-32C over bytes `0..60`.
    #[must_use]
    pub fn compute_hdr_crc(&self) -> u32 {
        crc32c(&self.as_bytes()[..STREAM_HDR_CRC_OFFSET])
    }

    /// Stores `hdr_crc`. Call after the last field change and before sending.
    pub fn seal(&mut self) {
        self.hdr_crc = self.compute_hdr_crc();
    }

    /// Checks magic, major version, header CRC, kind, codec, reserved flag
    /// bits, both payload bounds, `raw_len == wire_len` for `Raw`, the CP
    /// bound and `base <= cp_seq`.
    pub fn verify(&self) -> Result<(), FormatError> {
        check_magic(self.magic, STREAM_MAGIC)?;
        check_major(self.ver, STREAM_MAJOR)?;
        check_crc("stream header", self.compute_hdr_crc(), self.hdr_crc)?;
        if FrameKind::from_u8(self.kind).is_none() {
            return Err(bad_field("kind", u64::from(self.kind)));
        }
        if self.flags & !FLAGS_CODEC_MASK != 0 {
            return Err(bad_field("flags", u64::from(self.flags)));
        }
        let codec = Codec::from_u8(self.flags & FLAGS_CODEC_MASK)
            .ok_or(bad_field("codec", u64::from(self.flags & FLAGS_CODEC_MASK)))?;
        if self.wire_len > MAX_WIRE_PAYLOAD {
            return Err(FormatError::OutOfRange {
                field: "wire_len",
                value: u64::from(self.wire_len),
                max: u64::from(MAX_WIRE_PAYLOAD),
            });
        }
        if self.raw_len > MAX_WIRE_PAYLOAD {
            return Err(FormatError::OutOfRange {
                field: "raw_len",
                value: u64::from(self.raw_len),
                max: u64::from(MAX_WIRE_PAYLOAD),
            });
        }
        if codec == Codec::Raw && self.raw_len != self.wire_len {
            return Err(bad_field("raw_len", u64::from(self.raw_len)));
        }
        if self.cp_seq > MAX_CP_SEQ {
            return Err(FormatError::OutOfRange {
                field: "cp_seq",
                value: self.cp_seq,
                max: MAX_CP_SEQ,
            });
        }
        if self.base > self.cp_seq {
            return Err(bad_field("base", self.base));
        }
        Ok(())
    }

    /// Checks that `payload` is `wire_len` bytes with the recorded CRC.
    pub fn verify_payload(&self, payload: &[u8]) -> Result<(), FormatError> {
        if payload.len() != self.wire_len as usize {
            return Err(FormatError::BadLength {
                expected: self.wire_len as usize,
                found: payload.len(),
            });
        }
        check_crc("stream payload", crc32c(payload), self.payload_crc)
    }

    /// A verified zero-copy view over a 64-byte, 8-aligned buffer.
    pub fn parse(bytes: &[u8]) -> Result<&Self, FormatError> {
        let hdr: &Self = view(bytes)?;
        hdr.verify()?;
        Ok(hdr)
    }

    /// A verified copy out of a 64-byte buffer of any alignment: the path for
    /// headers in a receive buffer.
    pub fn read(bytes: &[u8]) -> Result<Self, FormatError> {
        let hdr: Self = copy_out(bytes)?;
        hdr.verify()?;
        Ok(hdr)
    }

    /// The decoded kind.
    #[must_use]
    pub const fn frame_kind(&self) -> Option<FrameKind> {
        FrameKind::from_u8(self.kind)
    }

    /// The decoded codec.
    #[must_use]
    pub const fn codec(&self) -> Option<Codec> {
        Codec::from_u8(self.flags & FLAGS_CODEC_MASK)
    }

    /// Header plus payload length.
    #[must_use]
    pub const fn frame_len(&self) -> usize {
        STREAM_HEADER_SIZE + self.wire_len as usize
    }
}

/// One record of a `Holes` payload: `len` blocks from `lba` of volume `vol`
/// read as zero. The payload CRC proves the record arrived as sent, not that
/// it is sane, so a receiver runs [`validate`](Self::validate) on every
/// record before acting on it.
#[derive(
    FromBytes, IntoBytes, Immutable, KnownLayout, Clone, Copy, PartialEq, Eq, Hash, Default, Debug,
)]
#[repr(C)]
pub struct HoleRecord {
    /// The volume index, below [`MAX_VOLS`](crate::MAX_VOLS).
    pub vol: u32,
    /// Length of the hole in 4 KiB blocks, at least 1.
    pub len: u32,
    /// First logical block of the hole; the hole ends at or before `MAX_LBA + 1`.
    pub lba: u64,
}

impl HoleRecord {
    /// A hole of `len` blocks at `lba` in volume `vol`, checked by
    /// [`validate`](Self::validate).
    pub const fn new(vol: u32, lba: u64, len: u32) -> Result<Self, FormatError> {
        let record = Self { vol, len, lba };
        match record.validate() {
            Ok(()) => Ok(record),
            Err(e) => Err(e),
        }
    }

    /// One past the last block of the hole; `None` when the sum overflows.
    #[must_use]
    pub const fn end_lba(&self) -> Option<u64> {
        self.lba.checked_add(self.len as u64)
    }

    /// Rejects a volume index at or above the bound, an empty hole and a hole
    /// that runs past the address space.
    pub const fn validate(&self) -> Result<(), FormatError> {
        if self.vol > MAX_VOL {
            return Err(FormatError::OutOfRange {
                field: "vol",
                value: self.vol as u64,
                max: MAX_VOL as u64,
            });
        }
        if self.len == 0 {
            return Err(bad_field("len", 0));
        }
        match self.end_lba() {
            Some(end) if end <= MAX_LBA + 1 => Ok(()),
            _ => Err(FormatError::OutOfRange {
                field: "lba",
                value: self.lba,
                max: MAX_LBA,
            }),
        }
    }
}

const _: () = {
    assert!(FrameKind::Hello.as_u8() == 1);
    assert!(FrameKind::HelloAck.as_u8() == 2);
    assert!(FrameKind::DeltaBegin.as_u8() == 3);
    assert!(FrameKind::VolEvent.as_u8() == 4);
    assert!(FrameKind::Chunk.as_u8() == 5);
    assert!(FrameKind::Holes.as_u8() == 6);
    assert!(FrameKind::DeltaEnd.as_u8() == 7);
    assert!(FrameKind::Landed.as_u8() == 8);
    assert!(FrameKind::Committed.as_u8() == 9);
    assert!(FrameKind::Abort.as_u8() == 10);
    assert!(FrameKind::Ping.as_u8() == 11);
    assert!(FrameKind::Pong.as_u8() == 12);
    assert!(matches!(FrameKind::from_u8(1), Some(FrameKind::Hello)));
    assert!(matches!(FrameKind::from_u8(5), Some(FrameKind::Chunk)));
    assert!(matches!(FrameKind::from_u8(12), Some(FrameKind::Pong)));
    assert!(FrameKind::from_u8(0).is_none() && FrameKind::from_u8(13).is_none());
    assert!(Codec::Raw.as_u8() == 0);
    assert!(Codec::Lz4.as_u8() == 1);
    assert!(Codec::Zstd.as_u8() == 2);
    assert!(matches!(Codec::from_u8(0), Some(Codec::Raw)));
    assert!(matches!(Codec::from_u8(1), Some(Codec::Lz4)));
    assert!(matches!(Codec::from_u8(2), Some(Codec::Zstd)));
    assert!(Codec::from_u8(3).is_none());

    assert!(size_of::<StreamHeader>() == STREAM_HEADER_SIZE);
    assert!(align_of::<StreamHeader>() == 8);
    assert!(offset_of!(StreamHeader, magic) == 0);
    assert!(offset_of!(StreamHeader, ver) == 4);
    assert!(offset_of!(StreamHeader, kind) == 6);
    assert!(offset_of!(StreamHeader, flags) == 7);
    assert!(offset_of!(StreamHeader, rel) == 8);
    assert!(offset_of!(StreamHeader, chunk_idx) == 12);
    assert!(offset_of!(StreamHeader, cp_seq) == 16);
    assert!(offset_of!(StreamHeader, base) == 24);
    assert!(offset_of!(StreamHeader, cp_nonce) == 32);
    assert!(offset_of!(StreamHeader, owner_epoch) == 40);
    assert!(offset_of!(StreamHeader, wire_len) == 48);
    assert!(offset_of!(StreamHeader, raw_len) == 52);
    assert!(offset_of!(StreamHeader, payload_crc) == 56);
    assert!(offset_of!(StreamHeader, hdr_crc) == STREAM_HDR_CRC_OFFSET);

    assert!(size_of::<HoleRecord>() == HOLE_RECORD_SIZE);
    assert!(align_of::<HoleRecord>() == 8);
    assert!(offset_of!(HoleRecord, vol) == 0);
    assert!(offset_of!(HoleRecord, len) == 4);
    assert!(offset_of!(HoleRecord, lba) == 8);
};
