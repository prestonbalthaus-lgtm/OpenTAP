//! Frame demuxing against arbitrary input, in four parts.
//!
//! 1. The input as a byte stream, fed in sizes the input itself chooses and
//!    again all at once: the decoder never panics and reports the same
//!    frames, CRC failures and fatal error however the bytes arrive; every
//!    frame it yields decodes or fails cleanly, and a decoded frame is valid,
//!    re-encodes and reads back unchanged.
//! 2. The input sealed as the body of a frame of a kind and codec its first
//!    byte picks, both CRCs valid, so the typed decoders see arbitrary bodies
//!    without the fuzzer having to solve two CRCs. When the input says so the
//!    payload sits at an odd offset of the receive buffer.
//! 3. A frame of every kind built from the input: it round-trips through
//!    every encoder entry point, a refused one leaves nothing behind, and one
//!    flipped bit is a payload CRC failure (the stream stays in sync) or a
//!    poisoned decoder, never a frame.
//! 4. The frame header and cursor page parsers at aligned and unaligned
//!    offsets.

#![no_main]

use bytes::{Bytes, BytesMut};
use libfuzzer_sys::fuzz_target;
use otap_format::{
    Codec, FormatError, FrameKind, HoleRecord, IntoBytes, StreamHeader, Uuid, VolBitSet,
    BLOCK_SIZE, MAX_CP_SEQ, MAX_LBA, MAX_STRIPE_BYTES, MAX_VOL, MAX_VOLS, MAX_WIRE_PAYLOAD,
    MIN_STRIPE_BYTES, STREAM_HEADER_SIZE,
};
use otap_stream::cursor::CURSOR_PAGE_SIZE;
use otap_stream::{
    Abort, AbortReason, AckedBytes, Chunk, Committed, Cursor, CursorPage, Decoder, DeltaBegin,
    DeltaEnd, DeltaMode, Encoder, Frame, FrameMeta, Hello, HelloAck, Holes, Landed, Ping, Pong,
    SnapTag, StreamError, VolClass, VolEvent, VolEventKind, MAX_CREDITS, MAX_DELTA_CHUNKS,
};

/// A decoded frame must re-encode to the bytes it came from: decoding
/// accepts only what the encoder writes (`otap_stream::frame`, "Canonical
/// bodies"). Set to `false` only to keep fuzzing the other properties while
/// a canonical-form regression is being fixed.
const CANONICAL: bool = true;

/// After this many decoder steps the rest of the input is fed at once.
const MAX_STEPS: usize = 1 << 16;

/// Rebuilding and re-encoding a landing walks its bitmap a word at a time,
/// a few dozen times per iteration, so part 2 cuts a sealed `HelloAck` that
/// names more chunks than this (but no more than `MAX_DELTA_CHUNKS`, which
/// is refused cheaply) down to keep an iteration cheap, and part 4 does not
/// round-trip a larger restored landing. A frame that arrives in part 1 is
/// decoded as it is.
const MAX_FUZZ_CHUNKS: u32 = 1 << 20;

fuzz_target!(|data: &[u8]| {
    demux(data);
    sealed_body(data);
    built_frames(data);
    parsers_at_offsets(data);
});

// ---------------------------------------------------------------------------
// 1. The input as a byte stream
// ---------------------------------------------------------------------------

/// What a decoder makes of a byte stream, frame by frame.
#[derive(Debug, PartialEq)]
enum Outcome {
    /// A frame whose header and payload verified.
    Frame(StreamHeader, Bytes),
    /// A payload CRC failure; the stream goes on.
    Corrupt(StreamError),
    /// A bad header; nothing after it is decoded.
    Fatal(StreamError),
}

/// Decodes `data` fed in pieces of `step(position)` bytes (at least one).
fn outcomes(data: &[u8], step: impl Fn(usize) -> usize) -> Vec<Outcome> {
    let mut decoder = Decoder::new();
    let mut seen = Vec::new();
    let mut pos = 0;
    let mut steps = 0;
    while pos < data.len() {
        steps += 1;
        let size = if steps > MAX_STEPS {
            data.len()
        } else {
            step(pos).max(1)
        };
        let end = pos.saturating_add(size).min(data.len());
        decoder.feed(&data[pos..end]);
        pos = end;
        loop {
            match decoder.next_frame() {
                Ok(Some(raw)) => seen.push(Outcome::Frame(raw.hdr, raw.payload)),
                Ok(None) => {
                    assert!(decoder.needed() > 0, "an incomplete frame needs bytes");
                    break;
                }
                Err(e @ StreamError::PayloadCrc { .. }) => {
                    assert!(!decoder.is_poisoned());
                    seen.push(Outcome::Corrupt(e));
                }
                Err(e) => {
                    assert!(decoder.is_poisoned());
                    assert_eq!(decoder.next_frame(), Err(e), "a poisoned decoder repeats");
                    seen.push(Outcome::Fatal(e));
                    return seen;
                }
            }
        }
    }
    seen
}

fn demux(data: &[u8]) {
    let stepwise = outcomes(data, |pos| 1 + usize::from(data[pos] % 97));
    let whole = outcomes(data, |_| data.len());
    assert!(
        stepwise == whole,
        "the outcome depends on how the bytes arrive"
    );
    for outcome in &whole {
        if let Outcome::Frame(hdr, payload) = outcome {
            check_decoded(hdr, payload);
        }
    }
}

/// A verified frame (header and payload CRC checked): its body decodes or is
/// refused cleanly. A decoded frame is valid, re-encodes and reads back
/// unchanged, and with `CANONICAL` re-encodes to the very same bytes.
fn check_decoded(hdr: &StreamHeader, payload: &Bytes) {
    let Ok(frame) = Frame::decode(hdr, payload.clone()) else {
        return;
    };
    assert_eq!(frame.kind().as_u8(), hdr.kind);
    assert_eq!(frame.codec().as_u8(), hdr.flags);
    frame.validate().expect("whatever decodes is valid");
    if let Frame::Chunk(chunk) = &frame {
        // At whatever alignment the receive buffer left the image.
        let _ = chunk.stripe_hdr();
        let _ = chunk.summary();
    }
    let meta = FrameMeta::from_header(hdr);
    let again = Encoder::new()
        .encode(&meta, &frame)
        .expect("a decoded frame re-encodes");
    let mut decoder = Decoder::new();
    decoder.feed(&again);
    let raw = decoder
        .next_frame()
        .expect("a re-encoded frame verifies")
        .expect("and is complete");
    let reread = raw.hdr;
    let (meta2, frame2) = raw.decode().expect("a re-encoded frame decodes");
    assert!(
        meta2 == meta && frame2 == frame,
        "a re-encoded frame reads back changed"
    );
    if CANONICAL {
        assert_eq!(
            &again[STREAM_HEADER_SIZE..],
            &payload[..],
            "decoding accepted a body the encoder never writes"
        );
        let expected = StreamHeader {
            ver: reread.ver,
            hdr_crc: reread.hdr_crc,
            ..*hdr
        };
        assert_eq!(reread, expected, "re-encoding changed the header");
    }
}

// ---------------------------------------------------------------------------
// 2. The input as a sealed body
// ---------------------------------------------------------------------------

/// The input after its first byte as the body of a frame whose kind and
/// codec the first byte picks, behind valid CRCs.
fn sealed_body(data: &[u8]) {
    let Some((&control, body)) = data.split_first() else {
        return;
    };
    let kind = FrameKind::from_u8(1 + control % 12).expect("1 to 12 are kinds");
    // A codec on one input in four: every kind but a chunk must refuse it.
    let codec = match (control >> 4) & 7 {
        1 => Codec::Lz4,
        2 => Codec::Zstd,
        _ => Codec::Raw,
    };
    let mut payload = body.to_vec();
    if kind == FrameKind::HelloAck && payload.len() >= 44 {
        let chunks = u32::from_le_bytes([payload[40], payload[41], payload[42], payload[43]]);
        if chunks > MAX_FUZZ_CHUNKS && chunks <= MAX_DELTA_CHUNKS {
            payload[40..44].copy_from_slice(&(chunks % MAX_FUZZ_CHUNKS).to_le_bytes());
        }
    }
    let mut hdr = StreamHeader::new(kind, codec);
    let sealed = match codec {
        Codec::Raw => hdr.set_payload(&payload),
        Codec::Lz4 | Codec::Zstd => {
            hdr.set_encoded_payload(&payload, usize::from(control) * BLOCK_SIZE)
        }
    };
    if sealed.is_err() {
        return;
    }
    hdr.seal();
    let direct = Frame::decode(&hdr, Bytes::copy_from_slice(&payload));

    // Through the receive path. A 65-byte frame ahead of it leaves its
    // payload at an odd offset of the receive buffer.
    let mut decoder = Decoder::new();
    if control & 0x80 != 0 {
        decoder.feed(&odd_frame());
        let first = decoder.next_frame().expect("valid").expect("complete");
        assert_eq!(first.payload.len() % 2, 1);
    }
    decoder.feed(hdr.as_bytes());
    decoder.feed(&payload);
    let raw = decoder
        .next_frame()
        .expect("both CRCs are valid")
        .expect("and the frame is complete");
    assert_eq!(decoder.next_frame(), Ok(None));
    let through = Frame::decode(&raw.hdr, raw.payload.clone());
    assert!(
        through == direct,
        "the receive path and a direct decode disagree"
    );
    check_decoded(&raw.hdr, &raw.payload);
}

/// A valid frame with a 65-byte payload: a `VolEvent` with a 1-byte name.
fn odd_frame() -> Bytes {
    let event = Frame::VolEvent(VolEvent {
        event: VolEventKind::SnapTag,
        vol: 0,
        class: None,
        size_blocks: 0,
        stripe_bytes: 0,
        retention_s: 0,
        uuid: Uuid::from_bytes([1; 16]),
        name: "x".to_owned(),
    });
    Encoder::new()
        .encode(&FrameMeta::default(), &event)
        .expect("a valid frame")
}

// ---------------------------------------------------------------------------
// 3. Frames built from the input
// ---------------------------------------------------------------------------

/// Integers read off the input, little-endian; zeros once it runs out.
struct Input<'a> {
    data: &'a [u8],
    pos: usize,
}

impl Input<'_> {
    fn bytes<const N: usize>(&mut self) -> [u8; N] {
        let mut out = [0; N];
        let rest = self.data.get(self.pos..).unwrap_or_default();
        let n = rest.len().min(N);
        out[..n].copy_from_slice(&rest[..n]);
        self.pos += n;
        out
    }

    fn u8(&mut self) -> u8 {
        self.bytes::<1>()[0]
    }

    fn u32(&mut self) -> u32 {
        u32::from_le_bytes(self.bytes())
    }

    fn u64(&mut self) -> u64 {
        u64::from_le_bytes(self.bytes())
    }

    fn uuid(&mut self) -> Uuid {
        Uuid::from_bytes(self.bytes())
    }

    /// `true` on about one input in eight: take a value as it comes rather
    /// than fit it to its rule, so the refusals are fuzzed too.
    fn raw(&mut self) -> bool {
        self.u8().is_multiple_of(8)
    }

    /// A stripe size (whole blocks, 8 KiB to 8 MiB), or now and then any
    /// value.
    fn stripe_size(&mut self) -> u32 {
        let value = self.u32();
        if self.raw() {
            value
        } else {
            (2 + value % (MAX_STRIPE_BYTES / BLOCK_SIZE - 1) as u32) * BLOCK_SIZE as u32
        }
    }

    /// The payload bytes a delta of `chunks` stripe images may declare, or
    /// now and then any value.
    fn delta_bytes(&mut self, chunks: u32) -> u64 {
        let value = self.u64();
        if self.raw() {
            return value;
        }
        let min = u64::from(chunks) * MIN_STRIPE_BYTES as u64;
        let span = u64::from(chunks) * (MAX_STRIPE_BYTES - MIN_STRIPE_BYTES) as u64;
        min + value % (span + 1)
    }

    /// A chunk count, `1..=MAX_DELTA_CHUNKS`, or now and then any value.
    fn chunks(&mut self) -> u32 {
        let value = self.u32();
        if self.raw() {
            value
        } else {
            1 + value % MAX_DELTA_CHUNKS
        }
    }

    /// `value % bound`, or now and then `value` itself.
    fn below(&mut self, bound: u32) -> u32 {
        let value = self.u32();
        if self.raw() {
            value
        } else {
            value % bound
        }
    }
}

fn built_frames(data: &[u8]) {
    let mut input = Input { data, pos: 0 };
    let cp_seq = input.u64() % (MAX_CP_SEQ + 1);
    let meta = FrameMeta {
        rel: input.u32(),
        chunk_idx: input.u32(),
        cp_seq,
        base: input.u64() % (cp_seq + 1),
        cp_nonce: input.u64(),
        owner_epoch: input.u64(),
    };
    // The `chunk_idx` rule: zero off chunks, below MAX_DELTA_CHUNKS on one;
    // now and then the context is kept as it came, to be refused.
    let keep_chunk_idx = input.raw();
    let flip = input.u64();
    let image: Vec<u8> = (0..MIN_STRIPE_BYTES)
        .map(|i| data.get(i % data.len().max(1)).copied().unwrap_or(0))
        .collect();
    let compressed = Bytes::copy_from_slice(&data[..data.len().min(256)]);
    let frames = [
        Frame::Hello(Hello {
            node_id: input.uuid(),
            pipe_id: input.uuid(),
            direction_epoch: input.u64(),
            capabilities: input.u64(),
            max_payload: MIN_STRIPE_BYTES as u32
                + input.u32() % (MAX_WIRE_PAYLOAD - MIN_STRIPE_BYTES as u32 + 1),
            initial_credits: input.below(MAX_CREDITS + 1),
        }),
        Frame::HelloAck(HelloAck {
            cursor: cursor_from(&mut input),
        }),
        Frame::DeltaBegin(delta_begin_from(&mut input)),
        Frame::VolEvent(vol_event_from(&mut input)),
        Frame::Chunk(Chunk::raw(Bytes::from(image)).expect("a stripe-sized image")),
        Frame::Chunk(Chunk {
            image: compressed,
            codec: if input.u8() & 1 == 0 {
                Codec::Lz4
            } else {
                Codec::Zstd
            },
            raw_len: input.stripe_size(),
        }),
        Frame::Holes(Holes {
            records: holes_from(data),
        }),
        {
            let digest = input.bytes();
            let chunks = input.chunks();
            Frame::DeltaEnd(DeltaEnd {
                digest,
                chunks,
                bytes: input.delta_bytes(chunks),
            })
        },
        Frame::Landed(Landed {
            idx: input.below(MAX_DELTA_CHUNKS),
            credits: input.below(MAX_CREDITS + 1),
            landed_contig: input.below(MAX_DELTA_CHUNKS + 1),
        }),
        Frame::Committed(Committed {
            applied_cp_seq: input.u64() % (MAX_CP_SEQ + 1),
            applied_epoch: input.u64(),
        }),
        {
            let reason = AbortReason::from_u8(1 + input.u8() % 6).expect("1 to 6 are reasons");
            let bound = if reason == AbortReason::Corrupt {
                MAX_DELTA_CHUNKS
            } else {
                1
            };
            Frame::Abort(Abort {
                reason,
                idx: input.below(bound),
                detail: input.u64(),
            })
        },
        Frame::Ping(Ping {
            nonce: input.u64(),
            mono_ns: input.u64(),
        }),
        Frame::Pong(Pong {
            nonce: input.u64(),
            mono_ns: input.u64(),
        }),
    ];
    for (i, frame) in frames.iter().enumerate() {
        let meta = if keep_chunk_idx {
            meta
        } else {
            meta_for(frame, meta)
        };
        round_trip(&meta, frame, flip.rotate_left(5 * i as u32));
    }
}

/// `meta` fitted to `frame` by the `chunk_idx` rule.
fn meta_for(frame: &Frame, meta: FrameMeta) -> FrameMeta {
    let chunk_idx = if matches!(frame, Frame::Chunk(_)) {
        meta.chunk_idx % MAX_DELTA_CHUNKS
    } else {
        0
    };
    FrameMeta { chunk_idx, ..meta }
}

/// A receiver cursor built through the public API: a landing of at most
/// 4096 chunks with up to 255 of them landed.
fn cursor_from(input: &mut Input<'_>) -> Cursor {
    let applied = input.u64() % (MAX_CP_SEQ + 1);
    let mut cursor = Cursor::new(applied, input.u64(), input.u32());
    let cp_seq = applied.saturating_add(1 + u64::from(input.u8()));
    let nonce = input.u64();
    let chunks = 1 + input.u32() % 4096;
    if cursor.begin(applied, cp_seq, nonce, chunks).is_ok() {
        for _ in 0..input.u8() {
            let _ = cursor.land(cp_seq, nonce, input.u32() % chunks);
        }
    }
    cursor
}

/// A delta description, mostly valid: the snapshots and the volumes are
/// taken as they come, the counts and sizes fitted to their rules except
/// now and then.
fn delta_begin_from(input: &mut Input<'_>) -> DeltaBegin {
    let mode = match input.u8() % 3 {
        0 => DeltaMode::Steady,
        _ => DeltaMode::Increment {
            from: input.uuid(),
            to: input.uuid(),
        },
    };
    let chunks = input.chunks();
    let bytes = input.delta_bytes(chunks);
    let cut_age_ms = input.u32();
    let mut write_active = VolBitSet::EMPTY;
    for word in &mut write_active.words {
        *word = input.u64();
    }
    let tags = (0..input.u8() % 8)
        .map(|_| SnapTag {
            uuid: input.uuid(),
            retention_s: input.u32(),
            rsvd: 0,
        })
        .collect();
    let acked = (0..input.u8() % 8)
        .map(|_| AckedBytes {
            vol: input.u32() % (MAX_VOL + 2),
            rsvd: 0,
            bytes: input.u64(),
        })
        .collect();
    DeltaBegin {
        mode,
        chunks,
        bytes,
        cut_age_ms,
        write_active,
        tags,
        acked,
    }
}

/// A volume event with the fields its kind uses, mostly valid; now and then
/// one field it does not use is set too. The name is the input made UTF-8,
/// which may push it past the length bound.
fn vol_event_from(input: &mut Input<'_>) -> VolEvent {
    let event = VolEventKind::from_u8(1 + input.u8() % 4).expect("1 to 4 are events");
    let mut v = VolEvent {
        event,
        vol: input.u32() % (MAX_VOL + 2),
        class: None,
        size_blocks: 0,
        stripe_bytes: 0,
        retention_s: 0,
        uuid: Uuid::NIL,
        name: String::new(),
    };
    let size_blocks = 1 + input.u64() % (MAX_LBA + 1);
    match event {
        VolEventKind::Create => {
            v.class = VolClass::from_u8(input.u8() % 6);
            v.size_blocks = size_blocks;
            v.stripe_bytes = input.stripe_size();
            v.uuid = input.uuid();
        }
        VolEventKind::Resize => v.size_blocks = size_blocks,
        VolEventKind::Delete => {}
        VolEventKind::SnapTag => {
            v.uuid = input.uuid();
            v.retention_s = input.u32();
        }
    }
    if matches!(event, VolEventKind::Create | VolEventKind::SnapTag) || input.raw() {
        let raw_name: Vec<u8> = (0..input.u8()).map(|_| input.u8()).collect();
        v.name = String::from_utf8_lossy(&raw_name).into_owned();
    }
    if input.raw() {
        match input.u8() % 4 {
            0 => v.size_blocks = input.u64(),
            1 => v.stripe_bytes = input.u32(),
            2 => v.retention_s = input.u32(),
            _ => v.uuid = input.uuid(),
        }
    }
    v
}

/// Up to 64 hole records, one per 16-byte window of the input that makes a
/// valid one.
fn holes_from(data: &[u8]) -> Vec<HoleRecord> {
    data.as_chunks::<16>()
        .0
        .iter()
        .take(64)
        .filter_map(|c| {
            let vol = u32::from_le_bytes(c[0..4].try_into().ok()?) % MAX_VOLS as u32;
            let len = 1 + u32::from_le_bytes(c[4..8].try_into().ok()?) % 4096;
            let lba =
                u64::from_le_bytes(c[8..16].try_into().ok()?) % (MAX_LBA + 1 - u64::from(len));
            HoleRecord::new(vol, lba, len).ok()
        })
        .collect()
}

/// `frame` through every encoder entry point and back, then with one bit
/// flipped.
fn round_trip(meta: &FrameMeta, frame: &Frame, flip: u64) {
    let encoder = Encoder::new();
    let mut prefixed = BytesMut::from(&b"prefix"[..]);
    let Ok(encoded) = encoder.encode(meta, frame) else {
        // A refused frame leaves nothing behind.
        assert!(encoder.encode_into(meta, frame, &mut prefixed).is_err());
        assert_eq!(&prefixed[..], b"prefix");
        return;
    };
    let appended = encoder
        .encode_into(meta, frame, &mut prefixed)
        .expect("a frame that encodes once encodes twice");
    assert_eq!(appended, encoded.len());
    assert_eq!(&prefixed[6..], &encoded[..]);
    let hdr = encoder
        .header(meta, frame, &encoded[STREAM_HEADER_SIZE..])
        .expect("the header for the payload just encoded");
    assert_eq!(hdr.as_bytes(), &encoded[..STREAM_HEADER_SIZE]);

    let mut decoder = Decoder::new();
    decoder.feed(&encoded);
    let raw = decoder.next_frame().expect("valid").expect("complete");
    let (meta2, frame2) = raw.decode().expect("an encoded frame decodes");
    assert!(meta2 == *meta && frame2 == *frame);

    // One flipped bit, then the intact frame: a header bit poisons the
    // decoder, a payload bit is a CRC failure and the copy behind it decodes.
    let mut damaged = encoded.to_vec();
    let bit = (flip % (damaged.len() as u64 * 8)) as usize;
    damaged[bit / 8] ^= 1 << (bit % 8);
    let mut decoder = Decoder::new();
    decoder.feed(&damaged);
    decoder.feed(&encoded);
    match decoder.next_frame() {
        Err(StreamError::PayloadCrc { .. }) => {
            assert!(bit >= STREAM_HEADER_SIZE * 8);
            let raw = decoder.next_frame().expect("in sync").expect("complete");
            assert!(raw.decode().expect("intact").1 == *frame);
        }
        Err(_) => {
            assert!(bit < STREAM_HEADER_SIZE * 8);
            assert!(decoder.is_poisoned());
        }
        Ok(_) => panic!("a flipped bit went unnoticed"),
    }
}

// ---------------------------------------------------------------------------
// 4. Parsers at aligned and unaligned offsets
// ---------------------------------------------------------------------------

/// A buffer whose offset `k` is `k` bytes past an 8-byte boundary.
#[repr(C, align(8))]
struct Scratch<const N: usize>([u8; N]);

fn parsers_at_offsets(data: &[u8]) {
    // The first 64 bytes as a frame header: the copy reads the same at
    // every offset; the view agrees with it when aligned and refuses
    // otherwise.
    let mut head = [0u8; STREAM_HEADER_SIZE];
    let n = data.len().min(STREAM_HEADER_SIZE);
    head[..n].copy_from_slice(&data[..n]);
    let reference = StreamHeader::read(&head);
    let mut scratch = Scratch([0u8; STREAM_HEADER_SIZE + 8]);
    for offset in 0..8 {
        scratch.0[offset..offset + STREAM_HEADER_SIZE].copy_from_slice(&head);
        let at = &scratch.0[offset..offset + STREAM_HEADER_SIZE];
        assert_eq!(StreamHeader::read(at), reference);
        let viewed = StreamHeader::parse(at).copied();
        if offset == 0 {
            assert_eq!(viewed, reference);
        } else {
            assert_eq!(viewed, Err(FormatError::Misaligned { required: 8 }));
        }
    }

    // The first 4 KiB as a cursor page, aligned and three bytes on.
    let mut page = Scratch([0u8; CURSOR_PAGE_SIZE + 8]);
    let m = data.len().min(CURSOR_PAGE_SIZE);
    page.0[..m].copy_from_slice(&data[..m]);
    let reference = CursorPage::read(&page.0[..CURSOR_PAGE_SIZE]);
    page.0.copy_within(..CURSOR_PAGE_SIZE, 3);
    assert_eq!(
        CursorPage::read(&page.0[3..3 + CURSOR_PAGE_SIZE]),
        reference
    );
    let Ok(page) = reference else {
        return;
    };
    let Ok(cursor) = Cursor::from_page(&page) else {
        return;
    };
    if cursor
        .landing
        .as_ref()
        .is_some_and(|landing| landing.bitmap.chunks() > MAX_FUZZ_CHUNKS)
    {
        return;
    }
    // A restored cursor persists to itself and can be advertised.
    assert_eq!(
        Cursor::from_page(
            &cursor
                .to_page(page.pipe_id)
                .expect("a restored cursor persists")
        ),
        Ok(cursor.clone())
    );
    let ack = Frame::HelloAck(HelloAck { cursor });
    let bytes = Encoder::new()
        .encode(&FrameMeta::default(), &ack)
        .expect("a restored cursor can be sent");
    let mut decoder = Decoder::new();
    decoder.feed(&bytes);
    let (_, back) = decoder
        .next_frame()
        .expect("valid")
        .expect("complete")
        .decode()
        .expect("decodes");
    assert!(back == ack);
}
