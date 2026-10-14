//! Stream v1 properties: every frame kind round-trips through the codec, the
//! decoder is invariant to how the byte stream is chunked, corruption is
//! reported with the frame's identity and never desynchronises the stream,
//! the landing cursor is invariant to the order chunks arrive in and resumes
//! a delta exactly where a disconnect left it, and the credit windows never
//! let a sender outrun the receiver's staging.

use bytes::{Bytes, BytesMut};
use otap_format::{
    Codec, FormatError, FrameKind, HoleRecord, IntoBytes, StreamHeader, Uuid, VolBitSet,
    BLOCK_SIZE, MAX_CP_SEQ, MAX_LBA, MAX_STRIPE_BYTES, MAX_VOLS, MAX_WIRE_PAYLOAD,
    MIN_STRIPE_BYTES, STREAM_HEADER_SIZE, VOL_BITSET_WORDS,
};
use otap_stream::cursor::{CursorBody, CURSOR_PAGE_RANGES};
use otap_stream::frame::{CAP_CODEC_LZ4, MAX_NAME_LEN};
use otap_stream::*;
use proptest::prelude::*;

// ---------------------------------------------------------------------------
// strategies
// ---------------------------------------------------------------------------

fn uuid() -> impl Strategy<Value = Uuid> {
    any::<[u8; 16]>().prop_map(|mut b| {
        b[0] |= 1;
        Uuid::from_bytes(b)
    })
}

fn bitset() -> impl Strategy<Value = VolBitSet> {
    any::<[u64; VOL_BITSET_WORDS]>().prop_map(|words| VolBitSet { words })
}

/// `chunk_idx` is zero on every frame that is not a chunk (the header rule).
fn meta_for(frame: &Frame, meta: FrameMeta) -> FrameMeta {
    if matches!(frame, Frame::Chunk(_)) {
        meta
    } else {
        FrameMeta {
            chunk_idx: 0,
            ..meta
        }
    }
}

/// Payload bytes a delta of `chunks` stripe images may declare.
fn delta_bytes(chunks: u32, extra: u64) -> u64 {
    let min = u64::from(chunks) * MIN_STRIPE_BYTES as u64;
    let span = u64::from(chunks) * (MAX_STRIPE_BYTES - MIN_STRIPE_BYTES) as u64;
    min + extra % (span + 1)
}

fn meta() -> impl Strategy<Value = FrameMeta> {
    (
        any::<u32>(),
        0u32..MAX_DELTA_CHUNKS,
        0u64..=MAX_CP_SEQ,
        any::<u64>(),
        any::<u64>(),
        any::<u64>(),
    )
        .prop_map(
            |(rel, chunk_idx, cp_seq, base, cp_nonce, owner_epoch)| FrameMeta {
                rel,
                chunk_idx,
                cp_seq,
                base: base % (cp_seq + 1),
                cp_nonce,
                owner_epoch,
            },
        )
}

fn hello() -> impl Strategy<Value = Frame> {
    (
        uuid(),
        uuid(),
        any::<u64>(),
        any::<u64>(),
        (MIN_STRIPE_BYTES as u32)..=MAX_WIRE_PAYLOAD,
        0u32..=MAX_CREDITS,
    )
        .prop_map(
            |(node_id, pipe_id, direction_epoch, capabilities, max_payload, initial_credits)| {
                Frame::Hello(Hello {
                    node_id,
                    pipe_id,
                    direction_epoch,
                    capabilities,
                    max_payload,
                    initial_credits,
                })
            },
        )
}

/// A cursor with a random landing state, built through the public API.
fn cursor() -> impl Strategy<Value = Cursor> {
    (
        0u64..1000,
        any::<u64>(),
        0u32..=MAX_CREDITS,
        proptest::option::of((1u64..100, any::<u64>(), 1u32..=300, any::<u64>())),
    )
        .prop_map(|(applied, epoch, credits, landing)| {
            let mut cursor = Cursor::new(applied, epoch, credits);
            if let Some((ahead, nonce, chunks, pattern)) = landing {
                cursor
                    .begin(applied, applied + ahead, nonce, chunks)
                    .unwrap();
                for idx in 0..chunks {
                    // A pseudo-random subset: bit (idx * 7 + idx / 64) of the pattern.
                    if pattern >> ((idx.wrapping_mul(7) + idx / 64) % 64) & 1 == 1 {
                        cursor.land(applied + ahead, nonce, idx).unwrap();
                    }
                }
            }
            cursor
        })
}

fn hello_ack() -> impl Strategy<Value = Frame> {
    cursor().prop_map(|cursor| Frame::HelloAck(HelloAck { cursor }))
}

fn delta_begin() -> impl Strategy<Value = Frame> {
    (
        proptest::option::of((uuid(), uuid())),
        1u32..=MAX_DELTA_CHUNKS,
        any::<u64>(),
        any::<u32>(),
        bitset(),
        prop::collection::vec((uuid(), any::<u32>()), 0..5),
        prop::collection::vec((0u32..MAX_VOLS as u32, any::<u64>()), 0..5),
    )
        .prop_filter("an increment needs two snapshots", |(inc, ..)| {
            inc.is_none_or(|(from, to)| from != to)
        })
        .prop_map(
            |(inc, chunks, bytes, cut_age_ms, write_active, tags, acked)| {
                Frame::DeltaBegin(DeltaBegin {
                    mode: match inc {
                        Some((from, to)) => DeltaMode::Increment { from, to },
                        None => DeltaMode::Steady,
                    },
                    chunks,
                    bytes: delta_bytes(chunks, bytes),
                    cut_age_ms,
                    write_active,
                    tags: tags
                        .into_iter()
                        .map(|(uuid, retention_s)| SnapTag {
                            uuid,
                            retention_s,
                            rsvd: 0,
                        })
                        .collect(),
                    acked: acked
                        .into_iter()
                        .map(|(vol, bytes)| AckedBytes {
                            vol,
                            rsvd: 0,
                            bytes,
                        })
                        .collect(),
                })
            },
        )
}

fn name() -> impl Strategy<Value = String> {
    prop::collection::vec(any::<char>(), 0..40)
        .prop_map(|chars| chars.into_iter().collect::<String>())
        .prop_filter("names are at most 255 bytes", |s| s.len() <= MAX_NAME_LEN)
}

fn vol_event() -> impl Strategy<Value = Frame> {
    (
        1u8..=4,
        0u32..MAX_VOLS as u32,
        1u8..=5,
        1u64..=MAX_LBA + 1,
        2u32..=2048,
        any::<u32>(),
        uuid(),
        name(),
    )
        .prop_map(
            |(event, vol, class, size_blocks, stripe_bytes, retention_s, uuid, name)| {
                let event = VolEventKind::from_u8(event).unwrap();
                let (class, size_blocks, stripe_bytes, retention_s, uuid, name) = match event {
                    VolEventKind::Create => (
                        Some(VolClass::from_u8(class).unwrap()),
                        size_blocks,
                        stripe_bytes * BLOCK_SIZE as u32,
                        0,
                        uuid,
                        name,
                    ),
                    VolEventKind::Resize => (None, size_blocks, 0, 0, Uuid::NIL, String::new()),
                    VolEventKind::Delete => (None, 0, 0, 0, Uuid::NIL, String::new()),
                    VolEventKind::SnapTag => (None, 0, 0, retention_s, uuid, name),
                };
                Frame::VolEvent(VolEvent {
                    event,
                    vol,
                    class,
                    size_blocks,
                    stripe_bytes,
                    retention_s,
                    uuid,
                    name,
                })
            },
        )
}

fn chunk() -> impl Strategy<Value = Frame> {
    (2usize..=4, any::<u64>()).prop_map(|(blocks, seed)| {
        let image: Vec<u8> = (0..blocks * BLOCK_SIZE)
            .map(|i| (seed.wrapping_mul(i as u64 + 1) >> 13) as u8)
            .collect();
        Frame::Chunk(Chunk::raw(Bytes::from(image)).unwrap())
    })
}

fn holes() -> impl Strategy<Value = Frame> {
    prop::collection::vec((0u32..MAX_VOLS as u32, 0u64..=MAX_LBA, 1u32..=1024), 1..=8).prop_map(
        |records| {
            Frame::Holes(Holes {
                records: records
                    .into_iter()
                    .map(|(vol, lba, len)| {
                        let lba = lba.min(MAX_LBA + 1 - u64::from(len));
                        HoleRecord::new(vol, lba, len).unwrap()
                    })
                    .collect(),
            })
        },
    )
}

fn small_frames() -> impl Strategy<Value = Frame> {
    prop_oneof![
        (any::<[u8; 32]>(), 1u32..=MAX_DELTA_CHUNKS, any::<u64>()).prop_map(
            |(digest, chunks, bytes)| {
                Frame::DeltaEnd(DeltaEnd {
                    digest,
                    chunks,
                    bytes: delta_bytes(chunks, bytes),
                })
            }
        ),
        (
            0u32..MAX_DELTA_CHUNKS,
            0u32..=MAX_CREDITS,
            0u32..=MAX_DELTA_CHUNKS
        )
            .prop_map(|(idx, credits, landed_contig)| {
                Frame::Landed(Landed {
                    idx,
                    credits,
                    landed_contig,
                })
            }),
        (0u64..=MAX_CP_SEQ, any::<u64>()).prop_map(|(applied_cp_seq, applied_epoch)| {
            Frame::Committed(Committed {
                applied_cp_seq,
                applied_epoch,
            })
        }),
        (1u8..=6, 0u32..MAX_DELTA_CHUNKS, any::<u64>()).prop_map(|(reason, idx, detail)| {
            let reason = AbortReason::from_u8(reason).unwrap();
            Frame::Abort(Abort {
                reason,
                idx: if reason == AbortReason::Corrupt {
                    idx
                } else {
                    0
                },
                detail,
            })
        }),
        (any::<u64>(), any::<u64>())
            .prop_map(|(nonce, mono_ns)| Frame::Ping(Ping { nonce, mono_ns })),
        (any::<u64>(), any::<u64>())
            .prop_map(|(nonce, mono_ns)| Frame::Pong(Pong { nonce, mono_ns })),
    ]
}

fn frame() -> impl Strategy<Value = Frame> {
    prop_oneof![
        hello(),
        hello_ack(),
        delta_begin(),
        vol_event(),
        chunk(),
        holes(),
        small_frames(),
    ]
}

fn flip_bit(bytes: &mut [u8], bit: usize) {
    bytes[bit / 8] ^= 1 << (bit % 8);
}

// ---------------------------------------------------------------------------
// codec
// ---------------------------------------------------------------------------

proptest! {
    #[test]
    fn every_frame_round_trips(meta in meta(), frame in frame()) {
        let meta = meta_for(&frame, meta);
        let encoded = Encoder::new().encode(&meta, &frame).unwrap();
        prop_assert_eq!(encoded.len(), STREAM_HEADER_SIZE + frame.payload_len());
        let hdr = StreamHeader::read(&encoded[..STREAM_HEADER_SIZE]).unwrap();
        prop_assert_eq!(hdr.frame_kind(), Some(frame.kind()));
        prop_assert_eq!(hdr.codec(), Some(frame.codec()));
        prop_assert_eq!(hdr.wire_len as usize, frame.payload_len());
        prop_assert_eq!(hdr.raw_len as usize, frame.raw_len());
        prop_assert!(FrameMeta::from_header(&hdr) == meta);
        // The header built for an existing payload is the same header.
        let payload = &encoded[STREAM_HEADER_SIZE..];
        prop_assert_eq!(Encoder::new().header(&meta, &frame, payload).unwrap(), hdr);

        let mut decoder = Decoder::new();
        decoder.feed(&encoded);
        let raw = decoder.next_frame().unwrap().unwrap();
        prop_assert_eq!(decoder.next_frame().unwrap(), None);
        prop_assert_eq!(raw.kind(), Some(frame.kind()));
        let (meta2, frame2) = raw.decode().unwrap();
        prop_assert!(meta2 == meta);
        prop_assert!(frame2 == frame);
        prop_assert_eq!(decoder.frames(), 1);
        prop_assert_eq!(decoder.pending(), 0);
    }

    #[test]
    fn decoder_is_invariant_to_chunking(
        meta in meta(),
        frames in prop::collection::vec(frame(), 1..6),
        cuts in prop::collection::vec(1usize..5000, 0..40),
    ) {
        let meta = FrameMeta { chunk_idx: 0, ..meta };
        let encoder = Encoder::new();
        let mut stream = BytesMut::new();
        for frame in &frames {
            encoder.encode_into(&meta, frame, &mut stream).unwrap();
        }
        let stream = stream.freeze();

        let mut decoder = Decoder::new();
        let mut got = Vec::new();
        let mut pos = 0;
        for cut in cuts {
            if pos >= stream.len() {
                break;
            }
            let end = (pos + cut).min(stream.len());
            decoder.feed(&stream[pos..end]);
            pos = end;
            got.extend(decoder.drain().unwrap());
        }
        decoder.feed(&stream[pos..]);
        got.extend(decoder.drain().unwrap());
        prop_assert_eq!(got.len(), frames.len());
        for (raw, frame) in got.into_iter().zip(&frames) {
            let (m, f) = raw.decode().unwrap();
            prop_assert!(m == meta);
            prop_assert!(f == *frame);
        }
        prop_assert_eq!(decoder.needed(), STREAM_HEADER_SIZE);
        prop_assert_eq!(decoder.pending(), 0);
    }

    #[test]
    fn payload_corruption_is_reported_and_the_stream_stays_in_sync(
        meta in meta(),
        first in frame(),
        second in frame(),
        bit_seed in any::<usize>(),
    ) {
        prop_assume!(first.payload_len() > 0);
        let meta = FrameMeta { chunk_idx: 0, ..meta };
        let encoder = Encoder::new();
        let mut stream = BytesMut::new();
        encoder.encode_into(&meta, &first, &mut stream).unwrap();
        let first_len = stream.len();
        encoder.encode_into(&meta, &second, &mut stream).unwrap();
        let payload_bits = (first_len - STREAM_HEADER_SIZE) * 8;
        flip_bit(&mut stream[STREAM_HEADER_SIZE..first_len], bit_seed % payload_bits);

        let mut decoder = Decoder::new();
        decoder.feed(&stream);
        let err = decoder.next_frame().unwrap_err();
        prop_assert_eq!(
            err,
            StreamError::PayloadCrc {
                kind: first.kind().as_u8(),
                chunk_idx: meta.chunk_idx,
                cp_seq: meta.cp_seq,
            }
        );
        prop_assert!(!decoder.is_poisoned());
        prop_assert_eq!(decoder.corrupt(), 1);
        let raw = decoder.next_frame().unwrap().unwrap();
        let (_, f) = raw.decode().unwrap();
        prop_assert!(f == second);
        prop_assert_eq!(decoder.next_frame().unwrap(), None);
    }

    #[test]
    fn header_corruption_poisons_the_decoder(
        meta in meta(),
        frame in frame(),
        bit in 0usize..STREAM_HEADER_SIZE * 8,
    ) {
        let meta = meta_for(&frame, meta);
        let mut stream = BytesMut::new();
        Encoder::new().encode_into(&meta, &frame, &mut stream).unwrap();
        flip_bit(&mut stream[..STREAM_HEADER_SIZE], bit);
        let mut decoder = Decoder::new();
        decoder.feed(&stream);
        let err = decoder.next_frame().unwrap_err();
        prop_assert!(
            matches!(err, StreamError::Format(_) | StreamError::PayloadTooLarge { .. }),
            "{err:?}"
        );
        prop_assert!(decoder.is_poisoned());
        prop_assert_eq!(decoder.next_frame().unwrap_err(), err);
        prop_assert_eq!(decoder.frames(), 0);
    }

    #[test]
    fn oversized_payloads_are_refused_at_the_header(
        meta in meta(),
        limit in (MIN_STRIPE_BYTES as u32)..(1 << 20),
        over in 1u32..=4096,
    ) {
        let mut hdr = StreamHeader::new(FrameKind::Chunk, Codec::Raw);
        meta.apply(&mut hdr);
        hdr.wire_len = limit + over;
        hdr.raw_len = limit + over;
        hdr.seal();
        let mut decoder = Decoder::with_max_payload(limit);
        decoder.feed(hdr.as_bytes());
        prop_assert_eq!(
            decoder.next_frame().unwrap_err(),
            StreamError::PayloadTooLarge { len: (limit + over) as usize, max: limit }
        );
        prop_assert!(decoder.is_poisoned());
        // The format's own bound is refused by the header check itself.
        let mut hdr = StreamHeader::new(FrameKind::Chunk, Codec::Raw);
        hdr.wire_len = MAX_WIRE_PAYLOAD + over;
        hdr.raw_len = hdr.wire_len;
        hdr.seal();
        let mut decoder = Decoder::new();
        decoder.feed(hdr.as_bytes());
        prop_assert!(
            matches!(
                decoder.next_frame(),
                Err(StreamError::Format(FormatError::OutOfRange { field: "wire_len", .. }))
            ),
            "the format's own payload bound is enforced by the header check"
        );
        prop_assert_eq!(decoder.pending(), STREAM_HEADER_SIZE);
    }
}

#[test]
fn builders_refuse_what_decoders_refuse() {
    let meta = FrameMeta::default();
    let encoder = Encoder::new();
    let too_small = Frame::Hello(Hello {
        node_id: Uuid::from_bytes([1; 16]),
        pipe_id: Uuid::from_bytes([2; 16]),
        direction_epoch: 1,
        capabilities: CAP_CODEC_LZ4,
        max_payload: 4096,
        initial_credits: 0,
    });
    assert!(matches!(
        encoder.encode(&meta, &too_small),
        Err(StreamError::BadBody {
            field: "max_payload",
            ..
        })
    ));
    let no_chunks = Frame::DeltaBegin(DeltaBegin {
        mode: DeltaMode::Steady,
        chunks: 0,
        bytes: 0,
        cut_age_ms: 0,
        write_active: VolBitSet::EMPTY,
        tags: Vec::new(),
        acked: Vec::new(),
    });
    assert!(matches!(
        encoder.encode(&meta, &no_chunks),
        Err(StreamError::BadBody {
            field: "chunks",
            ..
        })
    ));
    let long_name = Frame::VolEvent(VolEvent {
        event: VolEventKind::SnapTag,
        vol: 1,
        class: None,
        size_blocks: 0,
        stripe_bytes: 0,
        retention_s: 0,
        uuid: Uuid::from_bytes([3; 16]),
        name: "x".repeat(MAX_NAME_LEN + 1),
    });
    assert!(matches!(
        encoder.encode(&meta, &long_name),
        Err(StreamError::BadBody {
            field: "name_len",
            ..
        })
    ));
    assert!(matches!(
        encoder.encode(
            &meta,
            &Frame::Holes(Holes {
                records: Vec::new()
            })
        ),
        Err(StreamError::BadBody {
            field: "records",
            ..
        })
    ));
    assert!(matches!(
        Chunk::raw(Bytes::from(vec![0u8; MIN_STRIPE_BYTES - 1])),
        Err(StreamError::BadBody {
            field: "raw_len",
            ..
        })
    ));
    assert!(Chunk::raw(Bytes::from(vec![0u8; MIN_STRIPE_BYTES])).is_ok());

    // Typed decoding checks exact body lengths and the kind asked for.
    let ping = Frame::Ping(Ping {
        nonce: 7,
        mono_ns: 9,
    });
    let encoded = encoder.encode(&meta, &ping).unwrap();
    let hdr = StreamHeader::read(&encoded[..STREAM_HEADER_SIZE]).unwrap();
    let payload = encoded.slice(STREAM_HEADER_SIZE..);
    assert!(matches!(
        Frame::decode_as(&hdr, payload.clone(), FrameKind::Pong),
        Err(StreamError::WrongKind {
            expected: FrameKind::Pong,
            found: 11
        })
    ));
    assert!(Frame::decode_as(&hdr, payload.clone(), FrameKind::Ping).is_ok());
    let mut short = hdr;
    short.wire_len = 8;
    assert!(matches!(
        Frame::decode(&short, payload.slice(..8)),
        Err(StreamError::BodyTooShort {
            needed: 16,
            have: 8
        })
    ));
    let mut long = hdr;
    long.wire_len = 24;
    let mut longer = BytesMut::from(&payload[..]);
    longer.extend_from_slice(&[0; 8]);
    assert!(matches!(
        Frame::decode(&long, longer.freeze()),
        Err(StreamError::BodyTooLong {
            expected: 16,
            found: 24
        })
    ));
    let mut bad_reason = Frame::Abort(Abort {
        reason: AbortReason::NoSpace,
        idx: 0,
        detail: 0,
    });
    let encoded = encoder.encode(&meta, &bad_reason).unwrap();
    let hdr = StreamHeader::read(&encoded[..STREAM_HEADER_SIZE]).unwrap();
    let mut payload = BytesMut::from(&encoded[STREAM_HEADER_SIZE..]);
    payload[0] = 9;
    assert!(matches!(
        Frame::decode(&hdr, payload.freeze()),
        Err(StreamError::BadBody {
            field: "reason",
            ..
        })
    ));
    if let Frame::Abort(a) = &mut bad_reason {
        a.reason = AbortReason::Corrupt;
    }
    assert!(encoder.encode(&meta, &bad_reason).is_ok());
}

// ---------------------------------------------------------------------------
// cursor
// ---------------------------------------------------------------------------

proptest! {
    /// Landing in any order yields the same bitmap, the same contiguous
    /// prefix and completion only when every chunk landed.
    #[test]
    fn landing_order_does_not_matter(chunks in 1u32..=200, order in any::<u64>(), dup_seed in any::<u32>()) {
        let mut indices: Vec<u32> = (0..chunks).collect();
        // A deterministic shuffle from the seed.
        let mut state = order | 1;
        for i in (1..indices.len()).rev() {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            let j = (state % (i as u64 + 1)) as usize;
            indices.swap(i, j);
        }
        let mut cursor = Cursor::new(10, 1, 0);
        prop_assert_eq!(cursor.begin(10, 11, 0xAB, chunks), Ok(BeginOutcome::Started));
        let mut model = vec![false; chunks as usize];
        for (step, &idx) in indices.iter().enumerate() {
            prop_assert_eq!(cursor.land(11, 0xAB, idx), Ok(LandOutcome::New));
            model[idx as usize] = true;
            let landing = cursor.landing.as_ref().unwrap();
            let contig = model.iter().position(|b| !*b).map_or(chunks, |p| p as u32);
            prop_assert_eq!(landing.bitmap.landed_contig(), contig);
            prop_assert_eq!(landing.bitmap.landed_count() as usize, step + 1);
            prop_assert_eq!(landing.bitmap.is_complete(), step + 1 == chunks as usize);
            if step + 1 < chunks as usize {
                prop_assert!(
                    matches!(cursor.commit(1), Err(StreamError::Incomplete { .. })),
                    "a commit before every chunk landed is refused"
                );
            }
            // A resend of any landed chunk is a duplicate, not an error.
            let again = indices[dup_seed as usize % (step + 1)];
            prop_assert_eq!(cursor.land(11, 0xAB, again), Ok(LandOutcome::Duplicate));
        }
        prop_assert_eq!(cursor.land(11, 0xAB, chunks), Err(StreamError::ChunkOutOfRange { idx: chunks, chunks }));
        prop_assert_eq!(cursor.commit(2), Ok(11));
        prop_assert_eq!(cursor.applied_cp_seq, 11);
        prop_assert_eq!(cursor.applied_epoch, 2);
        prop_assert_eq!(cursor.land(11, 0xAB, 0), Err(StreamError::NoDelta));
    }

    /// The resume point describes exactly the missing set, and a sender
    /// that resends only the missing chunks completes the delta.
    #[test]
    fn a_reconnect_resends_only_what_is_missing(cursor in cursor()) {
        match cursor.resume_point() {
            None => prop_assert!(cursor.landing.is_none()),
            Some((cp_seq, nonce, contig, sparse)) => {
                let landing = cursor.landing.as_ref().unwrap();
                let chunks = landing.bitmap.chunks();
                let rebuilt = ResumeBitmap::from_parts(chunks, contig, &sparse).unwrap();
                prop_assert!(rebuilt == landing.bitmap);
                let missing: Vec<u32> = landing.bitmap.missing().collect();
                for idx in 0..chunks {
                    prop_assert_eq!(missing.contains(&idx), !landing.bitmap.is_landed(idx));
                }
                let mut resumed = cursor.clone();
                prop_assert_eq!(resumed.begin(cursor.applied_cp_seq, cp_seq, nonce, chunks), Ok(BeginOutcome::Resumed));
                for idx in &missing {
                    prop_assert_eq!(resumed.land(cp_seq, nonce, *idx), Ok(LandOutcome::New));
                }
                prop_assert!(resumed.landing.as_ref().unwrap().bitmap.is_complete());
                prop_assert_eq!(resumed.commit(cursor.applied_epoch), Ok(cp_seq));
            }
        }
    }

    #[test]
    fn cursors_round_trip_through_hello_ack_and_the_page(cursor in cursor(), pipe in uuid(), bit in 0usize..4096 * 8) {
        let (body, ranges) = cursor.to_wire();
        prop_assert!(Cursor::from_wire(&body, &ranges).unwrap() == cursor);
        let frame = Frame::HelloAck(HelloAck { cursor: cursor.clone() });
        let encoded = Encoder::new().encode(&FrameMeta::default(), &frame).unwrap();
        let mut decoder = Decoder::new();
        decoder.feed(&encoded);
        let (_, decoded) = decoder.next_frame().unwrap().unwrap().decode().unwrap();
        prop_assert!(decoded == frame);

        let page = cursor.to_page(pipe).unwrap();
        prop_assert!(page.verify().is_ok());
        prop_assert_eq!(page.pipe_id, pipe);
        let restored = Cursor::from_page(&page).unwrap();
        if ranges.len() <= CURSOR_PAGE_RANGES {
            prop_assert!(restored == cursor);
        } else {
            // Ranges beyond the page are dropped; what remains is a subset
            // that lands again harmlessly.
            prop_assert_eq!(restored.applied_cp_seq, cursor.applied_cp_seq);
            let a = restored.landing.as_ref().unwrap();
            let b = cursor.landing.as_ref().unwrap();
            for idx in 0..a.bitmap.chunks() {
                prop_assert!(!a.bitmap.is_landed(idx) || b.bitmap.is_landed(idx));
            }
        }
        let mut img = [0u8; 4096];
        img.copy_from_slice(page.as_bytes());
        flip_bit(&mut img, bit);
        prop_assert!(CursorPage::read(&img).is_err());
    }

    #[test]
    fn begin_semantics(applied in 0u64..100, ahead in 1u64..10, nonce in any::<u64>(), chunks in 1u32..50) {
        let mut cursor = Cursor::new(applied, 3, 5);
        let cp = applied + ahead;
        prop_assert_eq!(cursor.begin(applied + 1, cp, nonce, chunks), Err(StreamError::BaseMismatch { base: applied + 1, applied }));
        prop_assert!(
            matches!(
                cursor.begin(applied, applied, nonce, chunks),
                Err(StreamError::BadBody { field: "cp_seq", .. })
            ),
            "a delta must end after its base"
        );
        prop_assert!(
            matches!(
                cursor.begin(applied, cp, nonce, 0),
                Err(StreamError::BadBody { field: "chunks", .. })
            ),
            "a delta has at least one chunk"
        );
        prop_assert_eq!(cursor.begin(applied, cp, nonce, chunks), Ok(BeginOutcome::Started));
        prop_assert_eq!(cursor.land(cp, nonce, 0), Ok(LandOutcome::New));
        prop_assert!(
            matches!(
                cursor.land(cp, nonce ^ 1, 0),
                Err(StreamError::BadBody { field: "cp_nonce", .. })
            ),
            "a chunk of another delta is refused"
        );
        prop_assert_eq!(cursor.begin(applied, cp, nonce, chunks), Ok(BeginOutcome::Resumed));
        prop_assert!(cursor.landing.as_ref().unwrap().bitmap.is_landed(0));
        prop_assert!(
            matches!(
                cursor.begin(applied, cp, nonce, chunks + 1),
                Err(StreamError::BadBody { field: "chunks", .. })
            ),
            "a resumed delta keeps its chunk count"
        );
        prop_assert_eq!(cursor.begin(applied, cp, nonce ^ 1, chunks), Ok(BeginOutcome::Discarded));
        prop_assert!(!cursor.landing.as_ref().unwrap().bitmap.is_landed(0));
        prop_assert_eq!(cursor.begin(applied, cp + 1, nonce, chunks), Ok(BeginOutcome::Replaced));
        prop_assert!(cursor.abort());
        prop_assert!(!cursor.abort());
        prop_assert_eq!(cursor.commit(1), Err(StreamError::NoDelta));
    }
}

#[test]
fn cursor_wire_body_rules() {
    let mut body = CursorBody {
        flags: 2,
        ..CursorBody::default()
    };
    assert!(matches!(
        Cursor::from_wire(&body, &[]),
        Err(StreamError::BadBody { field: "flags", .. })
    ));
    body.flags = 0;
    body.landing_cp_seq = 5;
    assert!(matches!(
        Cursor::from_wire(&body, &[]),
        Err(StreamError::BadBody {
            field: "landing",
            ..
        })
    ));
    body.landing_cp_seq = 0;
    body.sparse_ranges = 1;
    assert!(Cursor::from_wire(&body, &[]).is_err());
    body.sparse_ranges = 0;
    assert!(Cursor::from_wire(&body, &[]).is_ok());
    // Sparse ranges must be canonical: ordered, non-empty, separated by a
    // gap, and strictly above the contiguous prefix.
    assert!(ResumeBitmap::from_parts(10, 3, &[ChunkRange::new(5, 7)]).is_ok());
    assert!(ResumeBitmap::from_parts(10, 3, &[ChunkRange::new(4, 5)]).is_ok());
    assert!(ResumeBitmap::from_parts(10, 3, &[ChunkRange::new(3, 4)]).is_err());
    assert!(
        ResumeBitmap::from_parts(10, 3, &[ChunkRange::new(5, 7), ChunkRange::new(7, 9)]).is_err(),
        "adjacent ranges are one range"
    );
    assert!(
        ResumeBitmap::from_parts(10, 3, &[ChunkRange::new(5, 7), ChunkRange::new(8, 9)]).is_ok()
    );
    assert!(ResumeBitmap::new(0).is_err());
    assert!(ResumeBitmap::new(MAX_DELTA_CHUNKS + 1).is_err());
    assert!(ResumeBitmap::from_parts(10, 3, &[ChunkRange::new(5, 5)]).is_err());
    assert!(
        ResumeBitmap::from_parts(10, 3, &[ChunkRange::new(7, 8), ChunkRange::new(5, 6)]).is_err()
    );
    assert!(ResumeBitmap::from_parts(10, 3, &[ChunkRange::new(9, 11)]).is_err());
    assert!(ResumeBitmap::from_parts(10, 11, &[]).is_err());
    let full = ResumeBitmap::from_parts(10, 10, &[]).unwrap();
    assert!(full.is_complete());
    assert!(full.sparse_ranges().is_empty());
}

// ---------------------------------------------------------------------------
// flow
// ---------------------------------------------------------------------------

proptest! {
    #[test]
    fn windows_never_let_a_sender_outrun_staging(
        staging_blocks in 1u64..=1 << 16,
        steps in prop::collection::vec((0u8..3, 1usize..(4 << 20)), 1..100),
    ) {
        let mut receiver = ReceiverWindow::new(staging_blocks * BLOCK_SIZE as u64).unwrap();
        let mut sender = SenderWindow::new(0);
        let granted = receiver.initial_grant();
        prop_assert!(granted <= credits_for(64 << 20));
        prop_assert!(granted <= receiver.capacity());
        sender.grant(granted);
        prop_assert_eq!(sender.available(), receiver.outstanding());
        let mut in_flight: Vec<usize> = Vec::new();
        for (op, bytes) in steps {
            prop_assert!(receiver.outstanding() + receiver.staged() <= receiver.capacity());
            match op {
                // The sender sends a chunk if it can; the receiver lands it.
                0 => {
                    if sender.can_send(bytes) {
                        let credits = sender.reserve(bytes).unwrap();
                        prop_assert_eq!(credits, credits_for(bytes));
                        receiver.landed(bytes).unwrap();
                        in_flight.push(bytes);
                    } else {
                        prop_assert!(
                            matches!(sender.reserve(bytes), Err(StreamError::Window { .. })),
                            "a send beyond the window is refused"
                        );
                    }
                }
                // The receiver drains the oldest staged chunk and grants.
                1 => {
                    if let Some(done) = in_flight.first().copied() {
                        in_flight.remove(0);
                        receiver.drained(done).unwrap();
                        let grant = receiver.grant_up_to(credits_for(done));
                        sender.grant(grant);
                    }
                }
                // A spurious landing beyond the window is refused.
                _ => {
                    let over = receiver.outstanding() as usize * BLOCK_SIZE + BLOCK_SIZE;
                    prop_assert!(
                        matches!(receiver.landed(over), Err(StreamError::Window { .. })),
                        "a landing beyond the outstanding credits is refused"
                    );
                }
            }
            prop_assert_eq!(sender.available(), receiver.outstanding());
        }
        receiver.reset();
        sender.reset();
        prop_assert_eq!((receiver.outstanding(), receiver.staged(), sender.available()), (0, 0, 0));
        prop_assert_eq!(receiver.free(), receiver.capacity());
    }
}

#[test]
fn window_construction_rules() {
    assert!(ReceiverWindow::new(0).is_err());
    assert!(ReceiverWindow::new(4095).is_err());
    assert!(ReceiverWindow::new(2 << 30).is_ok());
    assert!(ReceiverWindow::new((2 << 30) + 4096).is_err());
    assert_eq!(credits_for(0), 0);
    assert_eq!(credits_for(1), 1);
    assert_eq!(credits_for(4096), 1);
    assert_eq!(credits_for(4097), 2);
    assert_eq!(CREDIT_BYTES, 4096);
    let mut r = ReceiverWindow::new(1 << 20).unwrap();
    assert_eq!(r.initial_grant(), 256);
    assert_eq!(r.grant_up_to(10), 0);
    r.landed(1 << 20).unwrap();
    assert!(r.drained((1 << 20) + 4096).is_err());
    r.drained(1 << 20).unwrap();
    assert_eq!(r.free(), 256);
}
