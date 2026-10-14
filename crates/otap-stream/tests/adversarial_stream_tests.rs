//! Adversarial tests for stream v1 (`otap-stream`): the QA pass over the
//! codec, the landing cursor and the credit windows.
//!
//! These complement `stream_tests.rs`. They feed the decoder in every
//! fragmentation, push frames to the 8 MiB bound and one byte past it, give
//! every kind empty, truncated, overlong and over-counted bodies behind valid
//! CRCs, walk every enumeration, bound and canonical-form rule of every body
//! and of the `chunk_idx` header rule, cross every kind with every codec,
//! attack the cursor's wire form, its page and its chunk indices, drive the
//! credit windows through random operation sequences with resets, and pin
//! the codec's determinism, injectivity and canonical round trip.
//!
//! A counting global allocator lets a test prove that a hostile record count
//! is refused before anything proportional to it is allocated. Every defect
//! this pass found was fixed in the sources; the tests that found them now
//! pin the fixed behaviour.

use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Mutex, MutexGuard, PoisonError};

use bytes::{BufMut, Bytes, BytesMut};
use otap_format::{
    Codec, FormatError, FrameKind, FromBytes, HoleRecord, IntoBytes, StreamHeader, Uuid, VolBitSet,
    BLOCK_SIZE, HOLE_RECORD_SIZE, MAX_CP_SEQ, MAX_LBA, MAX_STRIPE_BYTES, MAX_VOL, MAX_WIRE_PAYLOAD,
    MIN_STRIPE_BYTES, STREAM_HEADER_SIZE, STREAM_VERSION, VOL_BITSET_WORDS,
};
use otap_stream::cursor::{CursorBody, CURSOR_LANDING, CURSOR_PAGE_RANGES, CURSOR_PAGE_SIZE};
use otap_stream::flow::{INITIAL_GRANT_BYTES, MAX_STAGING_BYTES};
use otap_stream::frame::{
    AbortBody, CommittedBody, DeltaBeginBody, DeltaEndBody, HelloBody, LandedBody, PingBody,
    VolEventBody, CAP_CODEC_LZ4, CAP_CODEC_ZSTD, MAX_NAME_LEN,
};
use otap_stream::*;
use proptest::prelude::*;

// ---------------------------------------------------------------------------
// The allocation probe
// ---------------------------------------------------------------------------

/// The largest single allocation or reallocation target seen since the last
/// reset, process-wide.
static LARGEST_ALLOCATION: AtomicUsize = AtomicUsize::new(0);

/// The system allocator with the size of the largest request recorded.
struct ProbeAlloc;

// SAFETY: every method forwards its arguments unchanged to `System`, which
// upholds the `GlobalAlloc` contract; the probe only reads the sizes.
unsafe impl GlobalAlloc for ProbeAlloc {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        LARGEST_ALLOCATION.fetch_max(layout.size(), Ordering::Relaxed);
        // SAFETY: the caller upholds `alloc`'s contract, which is `System`'s.
        unsafe { System.alloc(layout) }
    }

    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        LARGEST_ALLOCATION.fetch_max(layout.size(), Ordering::Relaxed);
        // SAFETY: as for `alloc`.
        unsafe { System.alloc_zeroed(layout) }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        // SAFETY: `ptr` came from this allocator, that is from `System`, with
        // this `layout`.
        unsafe { System.dealloc(ptr, layout) }
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        LARGEST_ALLOCATION.fetch_max(new_size, Ordering::Relaxed);
        // SAFETY: as for `dealloc`; the caller upholds `realloc`'s contract
        // for `new_size`.
        unsafe { System.realloc(ptr, layout, new_size) }
    }
}

#[global_allocator]
static ALLOCATOR: ProbeAlloc = ProbeAlloc;

/// Every test that allocates a megabyte or more holds this lock, so the probe
/// below only ever sees allocations far smaller than the ones it looks for.
static HEAVY: Mutex<()> = Mutex::new(());

/// An allocation above this size during a probe is a finding: the hostile
/// claims probed below would need 64 MiB or more if anything proportional to
/// them were allocated, and every test outside `HEAVY` stays under 2 MiB.
const PROBE_LIMIT: usize = 4 << 20;

fn heavy() -> MutexGuard<'static, ()> {
    HEAVY.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Runs `f` while `_held` keeps the heavy tests off and returns its result
/// and the largest single allocation made meanwhile.
fn probe<R>(_held: &MutexGuard<'static, ()>, f: impl FnOnce() -> R) -> (R, usize) {
    LARGEST_ALLOCATION.store(0, Ordering::SeqCst);
    let result = f();
    (result, LARGEST_ALLOCATION.load(Ordering::SeqCst))
}

/// [`probe`] with the lock taken here.
fn largest_allocation<R>(f: impl FnOnce() -> R) -> (R, usize) {
    let held = heavy();
    probe(&held, f)
}

// ---------------------------------------------------------------------------
// Builders
// ---------------------------------------------------------------------------

/// Proptest's defaults (so `PROPTEST_CASES` still drives the merge gate),
/// without regression files in the source tree and with bounded shrinking.
fn config() -> ProptestConfig {
    ProptestConfig {
        failure_persistence: None,
        max_shrink_iters: 4096,
        ..ProptestConfig::default()
    }
}

fn id(byte: u8) -> Uuid {
    Uuid::from_bytes([byte; 16])
}

fn encode(meta: &FrameMeta, frame: &Frame) -> Bytes {
    Encoder::new()
        .encode(meta, frame)
        .expect("a valid frame encodes")
}

fn payload_of(frame: &Frame) -> Vec<u8> {
    let mut out = BytesMut::new();
    frame
        .encode_payload(&mut out)
        .expect("a valid frame encodes");
    out.to_vec()
}

/// `meta` fitted to `frame` by the `chunk_idx` rule: zero unless the frame
/// is a chunk, below [`MAX_DELTA_CHUNKS`] for one.
fn meta_for(frame: &Frame, meta: FrameMeta) -> FrameMeta {
    let chunk_idx = if matches!(frame, Frame::Chunk(_)) {
        meta.chunk_idx % MAX_DELTA_CHUNKS
    } else {
        0
    };
    FrameMeta { chunk_idx, ..meta }
}

/// Payload bytes a delta of `chunks` stripe images may declare, picked by
/// `extra` between the smallest and the largest images.
fn delta_bytes(chunks: u32, extra: u64) -> u64 {
    let min = u64::from(chunks) * MIN_STRIPE_BYTES as u64;
    let span = u64::from(chunks) * (MAX_STRIPE_BYTES - MIN_STRIPE_BYTES) as u64;
    min + extra % (span + 1)
}

/// One frame on the wire around `payload` with both CRCs valid, so that
/// whatever is refused is refused for the body.
fn wire(
    kind: FrameKind,
    codec: Codec,
    meta: &FrameMeta,
    payload: &[u8],
    raw_len: usize,
) -> Vec<u8> {
    let mut hdr = StreamHeader::new(kind, codec);
    meta.apply(&mut hdr);
    match codec {
        Codec::Raw => hdr
            .set_payload(payload)
            .expect("a payload within the bound"),
        Codec::Lz4 | Codec::Zstd => hdr
            .set_encoded_payload(payload, raw_len)
            .expect("a payload within the bound"),
    }
    hdr.seal();
    hdr.verify().expect("the header itself is valid");
    [hdr.as_bytes(), payload].concat()
}

fn raw_wire(kind: FrameKind, payload: &[u8]) -> Vec<u8> {
    wire(
        kind,
        Codec::Raw,
        &FrameMeta::default(),
        payload,
        payload.len(),
    )
}

fn ping() -> Frame {
    Frame::Ping(Ping {
        nonce: 0x5EED,
        mono_ns: 42,
    })
}

fn ping_wire() -> Bytes {
    encode(&FrameMeta::default(), &ping())
}

/// Decodes the frame in `wire`, followed on the stream by a good `Ping`. A
/// body problem must surface from the typed decode only: the decoder yields
/// the raw frame (both CRCs are valid), stays unpoisoned and yields the ping.
fn decode_body(wire: &[u8]) -> Result<Frame, StreamError> {
    let mut decoder = Decoder::new();
    decoder.feed(wire);
    decoder.feed(&ping_wire());
    let raw = decoder
        .next_frame()
        .expect("both CRCs are valid")
        .expect("the frame is complete");
    let next = decoder
        .next_frame()
        .expect("the stream is still in sync")
        .expect("the ping follows");
    assert!(
        matches!(next.decode(), Ok((_, Frame::Ping(_)))),
        "the frame after a refused body decodes"
    );
    assert!(!decoder.is_poisoned());
    raw.decode().map(|(_, frame)| frame)
}

/// The typed error for `payload` as a `kind` body behind valid CRCs, `None`
/// when the body is accepted.
fn refusal(kind: FrameKind, payload: &[u8]) -> Option<StreamError> {
    decode_body(&raw_wire(kind, payload)).err()
}

fn bad(field: &'static str, value: u64) -> Option<StreamError> {
    Some(StreamError::BadBody { field, value })
}

fn hello_body() -> HelloBody {
    HelloBody {
        node_id: id(1),
        pipe_id: id(2),
        direction_epoch: 3,
        capabilities: CAP_CODEC_LZ4,
        max_payload: MAX_WIRE_PAYLOAD,
        initial_credits: 16,
        rsvd: [0; 8],
    }
}

/// A landing cursor: applied CP 5, delta 5..6 of 20 chunks with 3 landed
/// contiguously, followed by `sparse_ranges` ranges.
fn cursor_body(sparse_ranges: u32) -> CursorBody {
    CursorBody {
        applied_cp_seq: 5,
        applied_epoch: 1,
        landing_cp_seq: 6,
        landing_cp_nonce: 7,
        landed_contig: 3,
        credits: 9,
        chunks: 20,
        sparse_ranges,
        flags: CURSOR_LANDING,
        rsvd: [0; 12],
    }
}

fn hello_ack_payload(body: &CursorBody, ranges: &[ChunkRange]) -> Vec<u8> {
    [body.as_bytes(), ranges.as_bytes()].concat()
}

fn delta_begin_body() -> DeltaBeginBody {
    DeltaBeginBody {
        mode: 1,
        rsvd0: [0; 3],
        chunks: 4,
        cut_age_ms: 5,
        tags: 0,
        acked: 0,
        rsvd1: 0,
        bytes: 4 * MIN_STRIPE_BYTES as u64,
        from_snap: Uuid::NIL,
        to_snap: Uuid::NIL,
        write_active: VolBitSet::EMPTY,
    }
}

fn snap_tag(byte: u8) -> SnapTag {
    SnapTag {
        uuid: id(byte),
        retention_s: 60,
        rsvd: 0,
    }
}

fn acked_bytes(vol: u32) -> AckedBytes {
    AckedBytes {
        vol,
        rsvd: 0,
        bytes: 4096,
    }
}

/// A `DeltaBegin` payload: the body (whose counts the caller sets) and the
/// records given.
fn delta_begin_payload(body: &DeltaBeginBody, tags: &[SnapTag], acked: &[AckedBytes]) -> Vec<u8> {
    [body.as_bytes(), tags.as_bytes(), acked.as_bytes()].concat()
}

/// A valid fixed `VolEvent` body of each kind, announcing a name of
/// `name_len` bytes (only `Create` and `SnapTag` may carry one).
fn vol_event_body(event: VolEventKind, name_len: u16) -> VolEventBody {
    let create = event == VolEventKind::Create;
    let sized = matches!(event, VolEventKind::Create | VolEventKind::Resize);
    let named = matches!(event, VolEventKind::Create | VolEventKind::SnapTag);
    VolEventBody {
        event: event.as_u8(),
        class: if create { VolClass::File.as_u8() } else { 0 },
        name_len,
        vol: 7,
        size_blocks: if sized { 1 << 20 } else { 0 },
        stripe_bytes: if create { 1 << 20 } else { 0 },
        retention_s: if event == VolEventKind::SnapTag {
            3600
        } else {
            0
        },
        uuid: if named { id(9) } else { Uuid::NIL },
        rsvd: [0; 24],
    }
}

fn vol_event_payload(body: &VolEventBody, name: &[u8]) -> Vec<u8> {
    [body.as_bytes(), name].concat()
}

fn delta_end_body() -> DeltaEndBody {
    DeltaEndBody {
        digest: [0xAB; 32],
        chunks: 4,
        rsvd: 0,
        bytes: 4 * MIN_STRIPE_BYTES as u64,
    }
}

fn landed_body() -> LandedBody {
    LandedBody {
        idx: 3,
        credits: 2,
        landed_contig: 4,
        rsvd: 0,
    }
}

fn committed_body() -> CommittedBody {
    CommittedBody {
        applied_cp_seq: 6,
        applied_epoch: 1,
    }
}

/// An `Abort` body; `idx` names a chunk for `Corrupt` only.
fn abort_body(reason: u8) -> AbortBody {
    AbortBody {
        reason,
        rsvd: [0; 3],
        idx: if reason == AbortReason::Corrupt.as_u8() {
            5
        } else {
            0
        },
        detail: 0,
    }
}

fn ping_body() -> PingBody {
    PingBody {
        nonce: 1,
        mono_ns: 2,
    }
}

/// A hole record as it may arrive, unchecked.
fn hole(vol: u32, lba: u64, len: u32) -> HoleRecord {
    HoleRecord { vol, len, lba }
}

/// Every kind whose body is fixed, with a valid body.
fn fixed_bodies() -> [(FrameKind, Vec<u8>); 10] {
    [
        (FrameKind::Hello, hello_body().as_bytes().to_vec()),
        (FrameKind::HelloAck, cursor_body(0).as_bytes().to_vec()),
        (
            FrameKind::DeltaBegin,
            delta_begin_body().as_bytes().to_vec(),
        ),
        (
            FrameKind::VolEvent,
            vol_event_body(VolEventKind::Delete, 0).as_bytes().to_vec(),
        ),
        (FrameKind::DeltaEnd, delta_end_body().as_bytes().to_vec()),
        (FrameKind::Landed, landed_body().as_bytes().to_vec()),
        (FrameKind::Committed, committed_body().as_bytes().to_vec()),
        (FrameKind::Abort, abort_body(1).as_bytes().to_vec()),
        (FrameKind::Ping, ping_body().as_bytes().to_vec()),
        (FrameKind::Pong, ping_body().as_bytes().to_vec()),
    ]
}

fn sample_hello() -> Hello {
    Hello {
        node_id: id(1),
        pipe_id: id(2),
        direction_epoch: 3,
        capabilities: CAP_CODEC_LZ4 | CAP_CODEC_ZSTD,
        max_payload: MAX_WIRE_PAYLOAD,
        initial_credits: 0,
    }
}

fn sample_delta_begin() -> DeltaBegin {
    let mut write_active = VolBitSet::EMPTY;
    write_active.set(7).expect("a valid volume");
    DeltaBegin {
        mode: DeltaMode::Increment {
            from: id(3),
            to: id(4),
        },
        chunks: 40,
        bytes: 40 * MIN_STRIPE_BYTES as u64,
        cut_age_ms: 12,
        write_active,
        tags: vec![snap_tag(5), snap_tag(6)],
        acked: vec![acked_bytes(0), acked_bytes(MAX_VOL)],
    }
}

fn sample_image(len: usize) -> Bytes {
    Bytes::from((0..len).map(|i| (i % 251) as u8).collect::<Vec<u8>>())
}

/// One valid frame of every kind.
fn sample_frames() -> Vec<Frame> {
    let mut cursor = Cursor::new(5, 1, 64);
    cursor.begin(5, 6, 7, 40).expect("a valid begin");
    for idx in [0, 1, 2, 5, 6, 9, 30] {
        cursor.land(6, 7, idx).expect("in range");
    }
    vec![
        Frame::Hello(sample_hello()),
        Frame::HelloAck(HelloAck { cursor }),
        Frame::DeltaBegin(sample_delta_begin()),
        Frame::VolEvent(VolEvent {
            event: VolEventKind::Create,
            vol: 7,
            class: Some(VolClass::DatastoreLun),
            size_blocks: 1 << 20,
            stripe_bytes: 1 << 20,
            retention_s: 0,
            uuid: id(8),
            name: "lun-7 \u{2713}".to_owned(),
        }),
        Frame::Chunk(Chunk::raw(sample_image(MIN_STRIPE_BYTES)).expect("a stripe-sized image")),
        Frame::Holes(Holes {
            records: vec![
                HoleRecord::new(7, 0, 16).expect("in bounds"),
                HoleRecord::new(MAX_VOL, MAX_LBA, 1).expect("in bounds"),
            ],
        }),
        Frame::DeltaEnd(DeltaEnd {
            digest: [7; 32],
            chunks: 40,
            bytes: 40 * MIN_STRIPE_BYTES as u64,
        }),
        Frame::Landed(Landed {
            idx: 39,
            credits: 2,
            landed_contig: 40,
        }),
        Frame::Committed(Committed {
            applied_cp_seq: 6,
            applied_epoch: 1,
        }),
        Frame::Abort(Abort {
            reason: AbortReason::Corrupt,
            idx: 17,
            detail: 0,
        }),
        ping(),
        Frame::Pong(Pong {
            nonce: 1,
            mono_ns: 2,
        }),
    ]
}

fn sample_meta() -> FrameMeta {
    FrameMeta {
        rel: 3,
        chunk_idx: 0,
        cp_seq: 6,
        base: 5,
        cp_nonce: 7,
        owner_epoch: 1,
    }
}

/// The sample frames back to back, and the offset each starts at.
fn sample_stream() -> (Vec<u8>, Vec<usize>) {
    let mut stream = BytesMut::new();
    let mut starts = Vec::new();
    for frame in sample_frames() {
        starts.push(stream.len());
        Encoder::new()
            .encode_into(&sample_meta(), &frame, &mut stream)
            .expect("a valid frame encodes");
    }
    (stream.to_vec(), starts)
}

/// Every complete frame the decoder holds, decoded; panics on any error.
fn take_all(decoder: &mut Decoder) -> Vec<Frame> {
    let mut out = Vec::new();
    while let Some(raw) = decoder.next_frame().expect("a clean stream") {
        out.push(raw.decode().expect("a valid frame").1);
    }
    out
}

/// An error that contradicts itself: "too short" with enough bytes, or "too
/// long" without too many.
fn contradicts_itself(e: &StreamError) -> bool {
    match *e {
        StreamError::BodyTooShort { needed, have } => have >= needed,
        StreamError::BodyTooLong { expected, found } => found <= expected,
        _ => false,
    }
}

// ---------------------------------------------------------------------------
// Strategies
// ---------------------------------------------------------------------------

fn uuid() -> impl Strategy<Value = Uuid> {
    any::<[u8; 16]>().prop_map(|mut bytes| {
        bytes[0] |= 1;
        Uuid::from_bytes(bytes)
    })
}

/// A valid context for a chunk; [`meta_for`] fits it to other kinds.
fn meta() -> impl Strategy<Value = FrameMeta> {
    (
        any::<u32>(),
        0..MAX_DELTA_CHUNKS,
        0..=MAX_CP_SEQ,
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
        0..=MAX_CREDITS,
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

/// A cursor built through the public API, with a small landing or none.
fn cursor() -> impl Strategy<Value = Cursor> {
    (
        0u64..1000,
        any::<u64>(),
        0..=MAX_CREDITS,
        proptest::option::of((
            1u64..100,
            any::<u64>(),
            1u32..=64,
            prop::collection::vec(any::<u32>(), 0..24),
        )),
    )
        .prop_map(|(applied, epoch, credits, landing)| {
            let mut cursor = Cursor::new(applied, epoch, credits);
            if let Some((ahead, nonce, chunks, landed)) = landing {
                cursor
                    .begin(applied, applied + ahead, nonce, chunks)
                    .expect("a valid begin");
                for idx in landed {
                    cursor
                        .land(applied + ahead, nonce, idx % chunks)
                        .expect("in range");
                }
            }
            cursor
        })
}

fn delta_begin() -> impl Strategy<Value = Frame> {
    (
        proptest::option::of((uuid(), uuid())),
        1u32..=MAX_DELTA_CHUNKS,
        any::<u64>(),
        any::<u32>(),
        any::<[u64; VOL_BITSET_WORDS]>(),
        prop::collection::vec((uuid(), any::<u32>()), 0..4),
        prop::collection::vec((0..=MAX_VOL, any::<u64>()), 0..4),
    )
        .prop_map(|(inc, chunks, extra, cut_age_ms, words, tags, acked)| {
            let mode = match inc {
                Some((from, to)) if from != to => DeltaMode::Increment { from, to },
                _ => DeltaMode::Steady,
            };
            Frame::DeltaBegin(DeltaBegin {
                mode,
                chunks,
                bytes: delta_bytes(chunks, extra),
                cut_age_ms,
                write_active: VolBitSet { words },
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
        })
}

/// A stripe size: whole blocks from 8 KiB to 8 MiB.
fn stripe_bytes() -> impl Strategy<Value = u32> {
    (2u32..=(MAX_STRIPE_BYTES / BLOCK_SIZE) as u32).prop_map(|blocks| blocks * BLOCK_SIZE as u32)
}

fn vol_event() -> impl Strategy<Value = Frame> {
    (
        1u8..=4,
        0..=MAX_VOL,
        1u8..=5,
        1u64..=MAX_LBA + 1,
        stripe_bytes(),
        any::<u32>(),
        uuid(),
        prop::collection::vec(any::<char>(), 0..20),
    )
        .prop_map(
            |(event, vol, class, size, stripe_bytes, retention_s, uuid, name)| {
                let event = VolEventKind::from_u8(event).expect("1..=4");
                let name: String = name.into_iter().collect();
                let bare = VolEvent {
                    event,
                    vol,
                    class: None,
                    size_blocks: 0,
                    stripe_bytes: 0,
                    retention_s: 0,
                    uuid: Uuid::NIL,
                    name: String::new(),
                };
                Frame::VolEvent(match event {
                    VolEventKind::Create => VolEvent {
                        class: VolClass::from_u8(class),
                        size_blocks: size,
                        stripe_bytes,
                        uuid,
                        name,
                        ..bare
                    },
                    VolEventKind::Resize => VolEvent {
                        size_blocks: size,
                        ..bare
                    },
                    VolEventKind::Delete => bare,
                    VolEventKind::SnapTag => VolEvent {
                        retention_s,
                        uuid,
                        name,
                        ..bare
                    },
                })
            },
        )
}

fn chunk() -> impl Strategy<Value = Frame> {
    (2usize..=3, any::<u64>()).prop_map(|(blocks, seed)| {
        let image: Vec<u8> = (0..blocks * BLOCK_SIZE)
            .map(|i| (seed.wrapping_mul(i as u64 | 1) >> 29) as u8)
            .collect();
        Frame::Chunk(Chunk::raw(Bytes::from(image)).expect("a stripe-sized image"))
    })
}

fn holes() -> impl Strategy<Value = Frame> {
    prop::collection::vec((0..=MAX_VOL, 0..=MAX_LBA, 1u32..=4096), 1..6).prop_map(|records| {
        Frame::Holes(Holes {
            records: records
                .into_iter()
                .map(|(vol, lba, len)| {
                    HoleRecord::new(vol, lba.min(MAX_LBA + 1 - u64::from(len)), len)
                        .expect("in bounds")
                })
                .collect(),
        })
    })
}

fn fixed_frames() -> impl Strategy<Value = Frame> {
    prop_oneof![
        (any::<[u8; 32]>(), 1u32..=MAX_DELTA_CHUNKS, any::<u64>()).prop_map(
            |(digest, chunks, extra)| Frame::DeltaEnd(DeltaEnd {
                digest,
                chunks,
                bytes: delta_bytes(chunks, extra),
            })
        ),
        (0..MAX_DELTA_CHUNKS, 0..=MAX_CREDITS, 0..=MAX_DELTA_CHUNKS).prop_map(
            |(idx, credits, landed_contig)| {
                Frame::Landed(Landed {
                    idx,
                    credits,
                    landed_contig,
                })
            }
        ),
        (0..=MAX_CP_SEQ, any::<u64>()).prop_map(|(applied_cp_seq, applied_epoch)| {
            Frame::Committed(Committed {
                applied_cp_seq,
                applied_epoch,
            })
        }),
        (1u8..=6, 0..MAX_DELTA_CHUNKS, any::<u64>()).prop_map(|(reason, idx, detail)| {
            let reason = AbortReason::from_u8(reason).expect("1..=6");
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

/// Every kind but `Chunk`, small.
fn small_frame() -> impl Strategy<Value = Frame> {
    prop_oneof![
        hello(),
        cursor().prop_map(|cursor| Frame::HelloAck(HelloAck { cursor })),
        delta_begin(),
        vol_event(),
        holes(),
        fixed_frames(),
    ]
}

fn frame() -> impl Strategy<Value = Frame> {
    prop_oneof![4 => small_frame(), 1 => chunk()]
}

/// `meta` and `frame` with exactly one field, picked by `field`, set from
/// `value`, kept in range where the field has one. The result may equal the
/// input or be refused; callers skip those cases.
fn mutate(meta: &FrameMeta, frame: &Frame, field: usize, value: u64) -> (FrameMeta, Frame) {
    let mut meta = *meta;
    let mut frame = frame.clone();
    let v32 = value as u32;
    let mut uuid_bytes = [0u8; 16];
    uuid_bytes[..8].copy_from_slice(&value.to_le_bytes());
    uuid_bytes[15] = 1;
    let uuid = Uuid::from_bytes(uuid_bytes);
    let pick = field / 3;
    if field.is_multiple_of(3) {
        match pick % 6 {
            0 => meta.rel = v32,
            1 => meta.chunk_idx = v32 % MAX_DELTA_CHUNKS,
            2 => meta.cp_seq = (value % (MAX_CP_SEQ + 1)).max(meta.base),
            3 => meta.base = value % (meta.cp_seq + 1),
            4 => meta.cp_nonce = value,
            _ => meta.owner_epoch = value,
        }
        return (meta, frame);
    }
    match &mut frame {
        Frame::Hello(h) => match pick % 6 {
            0 => h.node_id = uuid,
            1 => h.pipe_id = uuid,
            2 => h.direction_epoch = value,
            3 => h.capabilities = value,
            4 => {
                h.max_payload = MIN_STRIPE_BYTES as u32
                    + v32 % (MAX_WIRE_PAYLOAD - MIN_STRIPE_BYTES as u32 + 1);
            }
            _ => h.initial_credits = v32 % (MAX_CREDITS + 1),
        },
        Frame::HelloAck(a) => match pick % 3 {
            0 => a.cursor.credits = v32 % (MAX_CREDITS + 1),
            1 => a.cursor.applied_epoch = value,
            _ => match &mut a.cursor.landing {
                Some(landing) => {
                    let idx = v32 % landing.bitmap.chunks();
                    landing.bitmap.mark(idx).expect("in range");
                }
                None => a.cursor.applied_cp_seq = value % (MAX_CP_SEQ + 1),
            },
        },
        Frame::DeltaBegin(d) => match pick % 7 {
            0 => {
                d.mode = match d.mode {
                    DeltaMode::Steady => DeltaMode::Increment {
                        from: uuid,
                        to: id(0xEE),
                    },
                    DeltaMode::Increment { .. } => DeltaMode::Steady,
                };
            }
            1 => d.chunks = 1 + v32 % MAX_DELTA_CHUNKS,
            2 => d.bytes = delta_bytes(d.chunks, value),
            3 => d.cut_age_ms = v32,
            4 => {
                let word = (value % VOL_BITSET_WORDS as u64) as usize;
                d.write_active.words[word] ^= 1 << ((value >> 32) & 63);
            }
            5 => d.tags.push(SnapTag {
                uuid,
                retention_s: v32,
                rsvd: 0,
            }),
            _ => d.acked.push(AckedBytes {
                vol: v32 % (MAX_VOL + 1),
                rsvd: 0,
                bytes: value,
            }),
        },
        Frame::VolEvent(v) => match pick % 7 {
            0 => v.vol = v32 % (MAX_VOL + 1),
            1 => v.size_blocks = 1 + value % (MAX_LBA + 1),
            2 => v.stripe_bytes = (2 + v32 % 2047) * BLOCK_SIZE as u32,
            3 => v.retention_s = v32,
            4 => v.uuid = uuid,
            5 => v.name.push(char::from(b'a' + (value % 26) as u8)),
            _ => v.class = v.class.and(VolClass::from_u8(1 + (value % 5) as u8)),
        },
        Frame::Chunk(c) => {
            let mut image = c.image.to_vec();
            let at = (value % image.len() as u64) as usize;
            image[at] ^= 1 << (pick % 8);
            c.image = Bytes::from(image);
        }
        Frame::Holes(h) => {
            let n = h.records.len();
            let record = &mut h.records[pick % n];
            match value % 3 {
                0 => record.vol = (v32 >> 2) % (MAX_VOL + 1),
                1 => record.len = 1 + (v32 >> 2) % 1024,
                _ => record.lba = (value >> 2) % (MAX_LBA + 1 - 1024),
            }
        }
        Frame::DeltaEnd(e) => match pick % 3 {
            0 => e.digest[(value % 32) as usize] ^= 1 << ((value >> 8) % 8),
            1 => e.chunks = 1 + v32 % MAX_DELTA_CHUNKS,
            _ => e.bytes = delta_bytes(e.chunks, value),
        },
        Frame::Landed(l) => match pick % 3 {
            0 => l.idx = v32 % MAX_DELTA_CHUNKS,
            1 => l.credits = v32 % (MAX_CREDITS + 1),
            _ => l.landed_contig = v32 % (MAX_DELTA_CHUNKS + 1),
        },
        Frame::Committed(c) => match pick % 2 {
            0 => c.applied_cp_seq = value % (MAX_CP_SEQ + 1),
            _ => c.applied_epoch = value,
        },
        Frame::Abort(a) => match pick % 3 {
            0 => {
                if let Some(reason) = AbortReason::from_u8(1 + (value % 6) as u8) {
                    a.reason = reason;
                }
            }
            1 => a.idx = v32 % MAX_DELTA_CHUNKS,
            _ => a.detail = value,
        },
        Frame::Ping(p) => match pick % 2 {
            0 => p.nonce = value,
            _ => p.mono_ns = value,
        },
        Frame::Pong(p) => match pick % 2 {
            0 => p.nonce = value,
            _ => p.mono_ns = value,
        },
    }
    (meta, frame)
}

/// What a decoder makes of a stream, frame by frame.
#[derive(Debug, PartialEq)]
enum Outcome {
    /// A frame whose header and payload verified.
    Frame(StreamHeader, Bytes),
    /// A frame whose payload CRC failed; the stream went on.
    Corrupt(StreamError),
    /// A bad header; nothing after it is decoded.
    Fatal(StreamError),
}

/// Feeds `stream` in pieces of the sizes in `cuts` (the rest in one piece)
/// and records every outcome, calling `next_frame` after every feed.
fn outcomes(stream: &[u8], cuts: &[usize]) -> Vec<Outcome> {
    let mut decoder = Decoder::new();
    let mut seen = Vec::new();
    let mut pos = 0;
    let mut sizes = cuts.iter().copied();
    loop {
        let end = sizes
            .next()
            .map_or(stream.len(), |size| (pos + size).min(stream.len()));
        decoder.feed(&stream[pos..end]);
        pos = end;
        loop {
            match decoder.next_frame() {
                Ok(Some(raw)) => seen.push(Outcome::Frame(raw.hdr, raw.payload)),
                Ok(None) => break,
                Err(e @ StreamError::PayloadCrc { .. }) => seen.push(Outcome::Corrupt(e)),
                Err(e) => {
                    seen.push(Outcome::Fatal(e));
                    return seen;
                }
            }
        }
        if pos == stream.len() {
            return seen;
        }
    }
}

#[test]
fn the_samples_are_valid() {
    for (kind, body) in fixed_bodies() {
        assert_eq!(refusal(kind, &body), None, "{kind:?}");
    }
    for frame in sample_frames() {
        let bytes = encode(&sample_meta(), &frame);
        let mut decoder = Decoder::new();
        decoder.feed(&bytes);
        assert!(take_all(&mut decoder) == [frame]);
    }
}

// ---------------------------------------------------------------------------
// 1. Feeding the decoder
// ---------------------------------------------------------------------------

#[test]
fn empty_feeds_change_nothing_at_any_point_of_a_frame() {
    let (stream, starts) = sample_stream();
    let frames = sample_frames();
    let mut decoder = Decoder::new();
    decoder.feed(&[]);
    assert_eq!(decoder.next_frame(), Ok(None));
    assert_eq!(
        (decoder.needed(), decoder.pending()),
        (STREAM_HEADER_SIZE, 0)
    );

    // Every frame start and end, one byte in, mid-header, the header end and
    // mid-payload.
    let mut cuts = Vec::new();
    for (i, &start) in starts.iter().enumerate() {
        let end = starts.get(i + 1).copied().unwrap_or(stream.len());
        let body = start + STREAM_HEADER_SIZE;
        cuts.extend([start, start + 1, start + 32, body, (body + end) / 2, end]);
    }
    cuts.sort_unstable();
    cuts.dedup();
    let mut got = Vec::new();
    let mut pos = 0;
    for cut in cuts {
        decoder.feed(&stream[pos..cut]);
        pos = cut;
        got.extend(take_all(&mut decoder));
        let state = (decoder.needed(), decoder.pending(), got.len());
        for _ in 0..3 {
            decoder.feed(&[]);
            got.extend(take_all(&mut decoder));
            assert_eq!((decoder.needed(), decoder.pending(), got.len()), state);
        }
    }
    assert!(got == frames);
    assert_eq!(
        (decoder.needed(), decoder.pending(), decoder.frames()),
        (STREAM_HEADER_SIZE, 0, frames.len() as u64)
    );
}

proptest! {
    #![proptest_config(config())]

    /// Fed one byte at a time, the decoder yields each frame on its last
    /// byte, and `needed` and `pending` are exact after every byte.
    #[test]
    fn one_byte_at_a_time_needed_and_pending_are_exact(
        meta in meta(),
        frames in prop::collection::vec(small_frame(), 1..5),
    ) {
        let meta = FrameMeta { chunk_idx: 0, ..meta };
        let mut stream = BytesMut::new();
        let mut bounds = Vec::new();
        for frame in &frames {
            bounds.push((stream.len(), frame.payload_len()));
            Encoder::new().encode_into(&meta, frame, &mut stream).unwrap();
        }
        let mut decoder = Decoder::new();
        let mut got = Vec::new();
        let mut current = 0;
        for (pos, byte) in stream.iter().enumerate() {
            decoder.feed(&[*byte]);
            while let Some(raw) = decoder.next_frame().unwrap() {
                got.push(raw);
            }
            let fed = pos + 1;
            while current < bounds.len()
                && fed >= bounds[current].0 + STREAM_HEADER_SIZE + bounds[current].1
            {
                current += 1;
            }
            prop_assert_eq!(got.len(), current);
            let (needed, pending) = match bounds.get(current) {
                None => (STREAM_HEADER_SIZE, 0),
                Some(&(start, _)) if fed - start < STREAM_HEADER_SIZE => {
                    (STREAM_HEADER_SIZE - (fed - start), fed - start)
                }
                Some(&(start, wire_len)) => (
                    STREAM_HEADER_SIZE + wire_len - (fed - start),
                    fed - start - STREAM_HEADER_SIZE,
                ),
            };
            prop_assert_eq!(decoder.needed(), needed, "after byte {}", fed);
            prop_assert_eq!(decoder.pending(), pending, "after byte {}", fed);
        }
        prop_assert_eq!(got.len(), frames.len());
        for (raw, frame) in got.into_iter().zip(&frames) {
            let (m, f) = raw.decode().unwrap();
            prop_assert!(m == meta);
            prop_assert!(f == *frame);
        }
    }

    /// However a stream is cut, with or without damage, the decoder reports
    /// the same frames, the same CRC failures and the same fatal error; a
    /// frame that decodes re-encodes to the very bytes it came from.
    #[test]
    fn a_damaged_stream_decodes_the_same_however_it_arrives(
        meta in meta(),
        frames in prop::collection::vec(frame(), 1..5),
        flips in prop::collection::vec((any::<usize>(), 1u8..=255), 0..4),
        cuts in prop::collection::vec(1usize..3000, 0..30),
    ) {
        let mut stream = BytesMut::new();
        for frame in &frames {
            Encoder::new()
                .encode_into(&meta_for(frame, meta), frame, &mut stream)
                .unwrap();
        }
        let clean = flips.is_empty();
        for (at, mask) in flips {
            let len = stream.len();
            stream[at % len] ^= mask;
        }
        let whole = outcomes(&stream, &[]);
        let pieces = outcomes(&stream, &cuts);
        prop_assert!(whole == pieces, "the outcomes depend on the cuts");
        if clean {
            prop_assert_eq!(whole.len(), frames.len());
        }
        for (i, outcome) in whole.iter().enumerate() {
            match outcome {
                Outcome::Frame(hdr, payload) => {
                    prop_assert!(hdr.verify().is_ok());
                    prop_assert!(hdr.verify_payload(payload).is_ok());
                    if let Ok(frame) = Frame::decode(hdr, payload.clone()) {
                        prop_assert!(frame.validate().is_ok(), "decoded but invalid");
                        let again = encode(&FrameMeta::from_header(hdr), &frame);
                        prop_assert_eq!(&again[STREAM_HEADER_SIZE..], &payload[..]);
                        let mut decoder = Decoder::new();
                        decoder.feed(&again);
                        let (_, back) = decoder.next_frame().unwrap().unwrap().decode().unwrap();
                        prop_assert!(back == frame);
                    }
                }
                Outcome::Corrupt(_) => {}
                Outcome::Fatal(_) => {
                    prop_assert_eq!(i + 1, whole.len(), "a fatal error ends the stream");
                }
            }
        }
    }
}

#[test]
fn a_frame_split_in_two_at_every_offset_decodes_once() {
    let meta = sample_meta();
    let tail = encode(&meta, &ping());
    for frame in sample_frames() {
        let bytes = encode(&meta, &frame);
        for split in 1..bytes.len() {
            let mut decoder = Decoder::new();
            decoder.feed(&bytes[..split]);
            assert_eq!(
                decoder.next_frame(),
                Ok(None),
                "{:?} at {split}",
                frame.kind()
            );
            let (needed, pending) = if split < STREAM_HEADER_SIZE {
                (STREAM_HEADER_SIZE - split, split)
            } else {
                (bytes.len() - split, split - STREAM_HEADER_SIZE)
            };
            assert_eq!((decoder.needed(), decoder.pending()), (needed, pending));
            // The rest of the frame and a whole next frame in one feed.
            decoder.feed(&[&bytes[split..], &tail[..]].concat());
            let got = take_all(&mut decoder);
            assert!(
                got.len() == 2 && got[0] == frame && got[1] == ping(),
                "{:?} split at {split}",
                frame.kind()
            );
            assert_eq!(
                (decoder.needed(), decoder.pending()),
                (STREAM_HEADER_SIZE, 0)
            );
        }
    }
}

#[test]
fn several_frames_in_one_feed_then_the_rest_of_a_partial_one() {
    let (stream, starts) = sample_stream();
    let frames = sample_frames();
    for k in 1..frames.len() {
        let cut = starts[k] + STREAM_HEADER_SIZE + 3;
        let mut decoder = Decoder::new();
        decoder.feed(&stream[..cut]);
        assert!(take_all(&mut decoder)[..] == frames[..k]);
        assert_eq!(decoder.needed(), frames[k].payload_len() - 3);
        assert_eq!(decoder.pending(), 3);
        decoder.feed(&stream[cut..]);
        assert!(take_all(&mut decoder)[..] == frames[k..]);
    }
}

#[test]
fn drain_returns_what_is_complete_and_keeps_the_rest() {
    let (stream, _) = sample_stream();
    let frames = sample_frames();
    let mut decoder = Decoder::new();
    let mut got = Vec::new();
    for piece in stream.chunks(997) {
        decoder.feed(piece);
        for raw in decoder.drain().unwrap() {
            got.push(raw.decode().unwrap().1);
        }
        // drain stops only where a frame is incomplete.
        assert!(decoder.needed() > 0);
    }
    assert!(got == frames);
    assert!(decoder.drain().unwrap().is_empty());
    assert_eq!(decoder.frames(), frames.len() as u64);
}

#[test]
fn drain_returns_the_frames_before_an_error_and_reports_it_next() {
    let encoder = Encoder::new();
    let meta = sample_meta();
    let holes = Frame::Holes(Holes {
        records: vec![HoleRecord::new(3, 100, 8).unwrap()],
    });
    let mut stream = BytesMut::new();
    encoder.encode_into(&meta, &holes, &mut stream).unwrap();
    let damaged = stream.len() + STREAM_HEADER_SIZE;
    encoder.encode_into(&meta, &ping(), &mut stream).unwrap();
    encoder.encode_into(&meta, &ping(), &mut stream).unwrap();
    stream[damaged] ^= 1;
    let crc = StreamError::PayloadCrc {
        kind: FrameKind::Ping.as_u8(),
        chunk_idx: 0,
        cp_seq: meta.cp_seq,
    };
    let kinds = |frames: Vec<RawFrame>| -> Vec<Option<FrameKind>> {
        frames.iter().map(RawFrame::kind).collect()
    };

    // The holes, then the held CRC failure, then the ping behind it.
    let mut decoder = Decoder::new();
    decoder.feed(&stream);
    assert_eq!(decoder.drain().map(kinds), Ok(vec![Some(FrameKind::Holes)]));
    assert_eq!(decoder.drain().map(kinds), Err(crc));
    assert_eq!(decoder.drain().map(kinds), Ok(vec![Some(FrameKind::Ping)]));
    assert_eq!(decoder.drain().map(kinds), Ok(vec![]));
    assert_eq!((decoder.frames(), decoder.corrupt()), (2, 1));
    assert!(!decoder.is_poisoned());

    // `next_frame` reports a held error first too.
    let mut decoder = Decoder::new();
    decoder.feed(&stream);
    assert_eq!(decoder.drain().map(kinds), Ok(vec![Some(FrameKind::Holes)]));
    assert_eq!(decoder.next_frame(), Err(crc));
    assert!(matches!(
        decoder.next_frame(),
        Ok(Some(raw)) if raw.kind() == Some(FrameKind::Ping)
    ));

    // A bad header after a good frame: the frame, then the poison for good.
    let mut bad = encode(&meta, &ping()).to_vec();
    bad[0] ^= 0xFF;
    let mut decoder = Decoder::new();
    decoder.feed(&encode(&meta, &ping()));
    decoder.feed(&bad);
    assert_eq!(decoder.drain().map(kinds), Ok(vec![Some(FrameKind::Ping)]));
    for _ in 0..2 {
        assert!(matches!(
            decoder.drain(),
            Err(StreamError::Format(FormatError::BadMagic { .. }))
        ));
    }
    assert_eq!(decoder.frames(), 1);
}

/// `needed` is 0 while `drain` holds an error for the next call, and for a
/// poisoned decoder: zero means `next_frame` can make progress now, which
/// includes reporting an error, so a transport that reads `needed()` bytes
/// before each call never sits on a payload CRC failure, and on the
/// `Abort(Corrupt)` it owes the sender, until the peer sends something else.
#[test]
fn needed_is_zero_while_an_error_waits_to_be_reported() {
    let meta = sample_meta();
    let mut stream = BytesMut::new();
    Encoder::new()
        .encode_into(&meta, &ping(), &mut stream)
        .unwrap();
    let damaged = stream.len() + STREAM_HEADER_SIZE;
    Encoder::new()
        .encode_into(&meta, &ping(), &mut stream)
        .unwrap();
    stream[damaged] ^= 1;
    let mut decoder = Decoder::new();
    decoder.feed(&stream);
    assert_eq!(decoder.drain().map(|frames| frames.len()), Ok(1));
    assert_eq!(decoder.needed(), 0, "the held error is ready to report");
    assert!(
        matches!(
            decoder.next_frame(),
            Err(StreamError::PayloadCrc { chunk_idx: 0, .. })
        ),
        "the held error is reported"
    );
    // Both frames are consumed; the decoder waits for the next header.
    assert_eq!(decoder.needed(), STREAM_HEADER_SIZE);
    assert_eq!(decoder.next_frame(), Ok(None));

    // A poisoned decoder asks for nothing: it drops what it is fed, and
    // every call reports the poison.
    let mut bad = [0u8; STREAM_HEADER_SIZE];
    bad[..4].copy_from_slice(b"NOPE");
    let mut poisoned = Decoder::new();
    poisoned.feed(&bad);
    assert!(matches!(
        poisoned.next_frame(),
        Err(StreamError::Format(FormatError::BadMagic { .. }))
    ));
    assert!(poisoned.is_poisoned());
    assert_eq!(poisoned.needed(), 0);
    let held = poisoned.pending();
    poisoned.feed(&bad);
    assert_eq!(poisoned.needed(), 0);
    assert_eq!(
        poisoned.pending(),
        held,
        "a poisoned decoder drops further input and keeps what it refused"
    );
}

#[test]
fn a_transport_may_receive_straight_into_the_decoder_buffer() {
    let (stream, starts) = sample_stream();
    let frames = sample_frames();
    let mut decoder = Decoder::new();
    let mut got = Vec::new();
    let mut pos = 0;
    while pos < stream.len() {
        // Read at most what the decoder says it needs, capped at 300 bytes.
        let end = (pos + decoder.needed().clamp(1, 300)).min(stream.len());
        let buffer = decoder.buffer();
        buffer.reserve(end - pos);
        buffer.put_slice(&stream[pos..end]);
        pos = end;
        got.extend(take_all(&mut decoder));
    }
    assert!(got == frames);

    // Once a chunk's header is parsed, its whole payload fits in place.
    let at = frames
        .iter()
        .position(|f| f.kind() == FrameKind::Chunk)
        .expect("a chunk sample");
    let wire_len = frames[at].payload_len();
    let body = starts[at] + STREAM_HEADER_SIZE;
    let mut decoder = Decoder::new();
    decoder
        .buffer()
        .extend_from_slice(&stream[starts[at]..body]);
    assert_eq!(decoder.next_frame(), Ok(None));
    assert_eq!(decoder.needed(), wire_len);
    assert!(decoder.buffer().capacity() >= wire_len);
    decoder
        .buffer()
        .extend_from_slice(&stream[body..body + wire_len]);
    assert!(take_all(&mut decoder) == [frames[at].clone()]);
}

#[test]
fn a_poisoned_decoder_yields_nothing_more_and_drops_further_input() {
    let (stream, _) = sample_stream();
    let mut garbage = stream.clone();
    garbage[0] ^= 0xFF;
    let mut decoder = Decoder::new();
    decoder.feed(&garbage);
    let first = decoder.next_frame().unwrap_err();
    assert!(matches!(
        first,
        StreamError::Format(FormatError::BadMagic { .. })
    ));
    let held = decoder.pending();
    decoder.feed(&stream);
    assert_eq!(decoder.pending(), held, "a poisoned decoder drops input");
    for _ in 0..3 {
        assert_eq!(decoder.next_frame(), Err(first));
        assert_eq!(decoder.drain(), Err(first));
    }
    assert_eq!((decoder.frames(), decoder.corrupt()), (0, 0));
}

#[test]
fn a_raw_frame_names_its_kind_unless_built_by_hand() {
    let mut decoder = Decoder::new();
    decoder.feed(&ping_wire());
    let raw = decoder.next_frame().unwrap().unwrap();
    assert_eq!(raw.kind(), Some(FrameKind::Ping));
    let hand_built = RawFrame {
        hdr: StreamHeader::read_from_bytes(&[0u8; STREAM_HEADER_SIZE]).unwrap(),
        payload: Bytes::new(),
    };
    assert_eq!(hand_built.kind(), None);
    assert!(matches!(
        hand_built.decode(),
        Err(StreamError::Format(FormatError::BadField {
            field: "kind",
            ..
        }))
    ));
}

#[test]
fn header_versions_kinds_and_cp_bounds_through_the_decoder() {
    let body = ping_body();
    let verdict = |edit: &dyn Fn(&mut StreamHeader)| {
        let mut hdr = StreamHeader::new(FrameKind::Ping, Codec::Raw);
        hdr.set_payload(body.as_bytes()).unwrap();
        edit(&mut hdr);
        hdr.seal();
        let mut decoder = Decoder::new();
        decoder.feed(hdr.as_bytes());
        decoder.feed(body.as_bytes());
        let result = decoder.next_frame().map(|raw| raw.map(RawFrame::decode));
        (result, decoder.is_poisoned())
    };
    // A later minor of major 1 is read (new kinds, codecs and flags are
    // negotiated before a peer may send them).
    for ver in [STREAM_VERSION + 1, STREAM_VERSION | 0xFF] {
        let (result, poisoned) = verdict(&|h: &mut StreamHeader| h.ver = ver);
        assert!(matches!(result, Ok(Some(Ok(_)))) && !poisoned, "{ver:#06x}");
    }
    for ver in [0x0000, 0x00FF, 0x0200, 0xFFFF] {
        let (result, poisoned) = verdict(&|h: &mut StreamHeader| h.ver = ver);
        assert!(
            matches!(
                result,
                Err(StreamError::Format(FormatError::BadVersion { .. }))
            ) && poisoned,
            "{ver:#06x}"
        );
    }
    for kind in [0u8, 13, 0xFF] {
        let (result, poisoned) = verdict(&|h: &mut StreamHeader| h.kind = kind);
        assert!(
            matches!(
                result,
                Err(StreamError::Format(FormatError::BadField {
                    field: "kind",
                    ..
                }))
            ) && poisoned,
            "{kind}"
        );
    }
    let (result, poisoned) = verdict(&|h: &mut StreamHeader| h.cp_seq = MAX_CP_SEQ + 1);
    assert!(
        matches!(
            result,
            Err(StreamError::Format(FormatError::OutOfRange {
                field: "cp_seq",
                ..
            }))
        ) && poisoned
    );
    let (result, poisoned) = verdict(&|h: &mut StreamHeader| {
        h.cp_seq = 5;
        h.base = 6;
    });
    assert!(
        matches!(
            result,
            Err(StreamError::Format(FormatError::BadField {
                field: "base",
                ..
            }))
        ) && poisoned
    );
}

// ---------------------------------------------------------------------------
// 2. The 8 MiB bound
// ---------------------------------------------------------------------------

#[test]
fn a_chunk_of_exactly_the_payload_bound_round_trips() {
    let _guard = heavy();
    let max = MAX_WIRE_PAYLOAD as usize;
    let frame = Frame::Chunk(Chunk::raw(sample_image(max)).expect("8 MiB is a stripe size"));
    let meta = FrameMeta {
        rel: 1,
        chunk_idx: MAX_DELTA_CHUNKS - 1,
        cp_seq: 9,
        base: 8,
        cp_nonce: 10,
        owner_epoch: 11,
    };
    let encoded = encode(&meta, &frame);
    assert_eq!(encoded.len(), STREAM_HEADER_SIZE + max);

    let mut decoder = Decoder::new();
    decoder.feed(&encoded[..STREAM_HEADER_SIZE]);
    assert_eq!(decoder.next_frame(), Ok(None));
    assert_eq!(decoder.needed(), max);
    assert!(
        decoder.buffer().capacity() >= max,
        "the payload is reserved once the header verifies"
    );
    decoder.feed(&encoded[STREAM_HEADER_SIZE..STREAM_HEADER_SIZE + max / 2]);
    assert_eq!(decoder.next_frame(), Ok(None));
    assert_eq!(decoder.needed(), max - max / 2);
    decoder.feed(&encoded[STREAM_HEADER_SIZE + max / 2..]);
    let (m, f) = decoder.next_frame().unwrap().unwrap().decode().unwrap();
    assert_eq!(m, meta);
    assert!(f == frame);
    assert_eq!(decoder.pending(), 0);

    // The vectored path: the header for an image the caller holds.
    let Frame::Chunk(chunk) = &frame else {
        panic!("a chunk")
    };
    let hdr = Encoder::new().header(&meta, &frame, &chunk.image).unwrap();
    assert_eq!(hdr.as_bytes(), &encoded[..STREAM_HEADER_SIZE]);
}

#[test]
fn one_byte_past_the_bound_is_refused_at_the_header_before_any_allocation() {
    let mut over = StreamHeader::new(FrameKind::Chunk, Codec::Raw);
    over.wire_len = MAX_WIRE_PAYLOAD + 1;
    over.raw_len = MAX_WIRE_PAYLOAD + 1;
    over.seal();
    let mut raw_over = StreamHeader::new(FrameKind::Chunk, Codec::Lz4);
    raw_over.wire_len = 100;
    raw_over.raw_len = MAX_WIRE_PAYLOAD + 1;
    raw_over.seal();
    let mut tight = StreamHeader::new(FrameKind::Chunk, Codec::Raw);
    tight.wire_len = MIN_STRIPE_BYTES as u32 + 1;
    tight.raw_len = tight.wire_len;
    tight.seal();

    let (results, largest) = largest_allocation(|| {
        let mut results = Vec::new();
        for (hdr, limit) in [
            (over, MAX_WIRE_PAYLOAD),
            (over, u32::MAX),
            (raw_over, MAX_WIRE_PAYLOAD),
            (tight, MIN_STRIPE_BYTES as u32),
        ] {
            let mut decoder = Decoder::with_max_payload(limit);
            decoder.feed(hdr.as_bytes());
            let first = decoder.next_frame();
            results.push((
                first,
                decoder.next_frame(),
                decoder.pending(),
                decoder.is_poisoned(),
                decoder.buffer().capacity(),
            ));
        }
        results
    });
    assert!(
        largest < 1 << 20,
        "a refused header allocated {largest} bytes"
    );
    for (i, (first, again, pending, poisoned, capacity)) in results.into_iter().enumerate() {
        let expected = match i {
            0 | 1 => StreamError::Format(FormatError::OutOfRange {
                field: "wire_len",
                value: u64::from(MAX_WIRE_PAYLOAD) + 1,
                max: u64::from(MAX_WIRE_PAYLOAD),
            }),
            2 => StreamError::Format(FormatError::OutOfRange {
                field: "raw_len",
                value: u64::from(MAX_WIRE_PAYLOAD) + 1,
                max: u64::from(MAX_WIRE_PAYLOAD),
            }),
            _ => StreamError::PayloadTooLarge {
                len: MIN_STRIPE_BYTES + 1,
                max: MIN_STRIPE_BYTES as u32,
            },
        };
        assert_eq!(first, Err(expected), "case {i}");
        assert_eq!(again, Err(expected), "case {i}");
        assert_eq!(
            pending, STREAM_HEADER_SIZE,
            "case {i}: the header stays unconsumed"
        );
        assert!(poisoned, "case {i}");
        assert!(capacity < 1 << 20, "case {i}: {capacity} bytes reserved");
    }
}

#[test]
fn a_decoder_bound_above_the_format_bound_is_clamped() {
    let _guard = heavy();
    let mut at_bound = StreamHeader::new(FrameKind::Chunk, Codec::Raw);
    at_bound.wire_len = MAX_WIRE_PAYLOAD;
    at_bound.raw_len = MAX_WIRE_PAYLOAD;
    at_bound.seal();
    for limit in [MAX_WIRE_PAYLOAD, MAX_WIRE_PAYLOAD + 1, u32::MAX] {
        let mut decoder = Decoder::with_max_payload(limit);
        decoder.feed(at_bound.as_bytes());
        assert_eq!(decoder.next_frame(), Ok(None), "{limit}");
        assert_eq!(decoder.needed(), MAX_WIRE_PAYLOAD as usize);
    }
    // A bound of zero passes only empty payloads.
    let empty = raw_wire(FrameKind::Ping, &[]);
    let mut decoder = Decoder::with_max_payload(0);
    decoder.feed(&empty);
    let raw = decoder.next_frame().unwrap().unwrap();
    assert_eq!(
        raw.decode(),
        Err(StreamError::BodyTooShort {
            needed: 16,
            have: 0
        })
    );
    decoder.feed(&ping_wire());
    assert_eq!(
        decoder.next_frame(),
        Err(StreamError::PayloadTooLarge { len: 16, max: 0 })
    );
}

// ---------------------------------------------------------------------------
// 3. Empty, truncated, overlong and over-counted bodies
// ---------------------------------------------------------------------------

#[test]
fn no_kind_accepts_an_empty_payload() {
    for (kind, body) in fixed_bodies() {
        assert_eq!(
            refusal(kind, &[]),
            Some(StreamError::BodyTooShort {
                needed: body.len(),
                have: 0
            }),
            "{kind:?}"
        );
    }
    assert_eq!(refusal(FrameKind::Chunk, &[]), bad("raw_len", 0));
    assert_eq!(
        refusal(FrameKind::Holes, &[]),
        Some(StreamError::BodyTooShort {
            needed: HOLE_RECORD_SIZE,
            have: 0
        })
    );
}

#[test]
fn truncated_bodies_are_too_short_at_every_length() {
    for (kind, body) in fixed_bodies() {
        for len in 0..body.len() {
            assert_eq!(
                refusal(kind, &body[..len]),
                Some(StreamError::BodyTooShort {
                    needed: body.len(),
                    have: len
                }),
                "{kind:?} cut to {len}"
            );
        }
    }
    // Record arrays: `needed` runs to the end of the array being read.
    let ack = hello_ack_payload(
        &cursor_body(2),
        &[ChunkRange::new(5, 7), ChunkRange::new(9, 10)],
    );
    for len in size_of::<CursorBody>()..ack.len() {
        assert_eq!(
            refusal(FrameKind::HelloAck, &ack[..len]),
            Some(StreamError::BodyTooShort {
                needed: ack.len(),
                have: len
            })
        );
    }
    let begin = delta_begin_payload(
        &DeltaBeginBody {
            tags: 1,
            acked: 1,
            ..delta_begin_body()
        },
        &[snap_tag(5)],
        &[acked_bytes(1)],
    );
    let fixed = size_of::<DeltaBeginBody>();
    for len in fixed..begin.len() {
        let needed = if len < fixed + size_of::<SnapTag>() {
            fixed + size_of::<SnapTag>()
        } else {
            begin.len()
        };
        assert_eq!(
            refusal(FrameKind::DeltaBegin, &begin[..len]),
            Some(StreamError::BodyTooShort { needed, have: len })
        );
    }
    let name = b"volume-name";
    let event = vol_event_payload(
        &vol_event_body(VolEventKind::SnapTag, name.len() as u16),
        name,
    );
    for len in size_of::<VolEventBody>()..event.len() {
        assert_eq!(
            refusal(FrameKind::VolEvent, &event[..len]),
            Some(StreamError::BodyTooShort {
                needed: event.len(),
                have: len
            })
        );
    }
    // A hole record cut short is too short; a second one cut short leaves
    // bytes the first does not account for.
    let records = [hole(1, 2, 3), hole(4, 5, 6)];
    let bytes = records.as_bytes();
    for len in 0..HOLE_RECORD_SIZE {
        assert_eq!(
            refusal(FrameKind::Holes, &bytes[..len]),
            Some(StreamError::BodyTooShort {
                needed: HOLE_RECORD_SIZE,
                have: len
            })
        );
    }
    for len in HOLE_RECORD_SIZE + 1..2 * HOLE_RECORD_SIZE {
        assert_eq!(
            refusal(FrameKind::Holes, &bytes[..len]),
            Some(StreamError::BodyTooLong {
                expected: HOLE_RECORD_SIZE,
                found: len
            })
        );
    }
}

#[test]
fn bytes_after_every_body_are_too_long() {
    for (kind, body) in fixed_bodies() {
        for extra in [1usize, 7, 8, 16, 24] {
            let mut long = body.clone();
            long.resize(body.len() + extra, 0xA5);
            assert_eq!(
                refusal(kind, &long),
                Some(StreamError::BodyTooLong {
                    expected: body.len(),
                    found: body.len() + extra
                }),
                "{kind:?} + {extra}"
            );
        }
    }
    let too_long = |kind: FrameKind, mut payload: Vec<u8>| {
        let exact = payload.len();
        payload.push(0);
        assert_eq!(
            refusal(kind, &payload),
            Some(StreamError::BodyTooLong {
                expected: exact,
                found: exact + 1
            }),
            "{kind:?}"
        );
    };
    too_long(
        FrameKind::HelloAck,
        hello_ack_payload(&cursor_body(1), &[ChunkRange::new(5, 7)]),
    );
    too_long(
        FrameKind::DeltaBegin,
        delta_begin_payload(
            &DeltaBeginBody {
                tags: 1,
                acked: 1,
                ..delta_begin_body()
            },
            &[snap_tag(5)],
            &[acked_bytes(1)],
        ),
    );
    too_long(
        FrameKind::VolEvent,
        vol_event_payload(&vol_event_body(VolEventKind::Create, 3), b"abc"),
    );
    too_long(FrameKind::Holes, hole(1, 2, 3).as_bytes().to_vec());
    // A raw image off a block multiple, or under two blocks, is not a stripe.
    assert_eq!(
        refusal(FrameKind::Chunk, &vec![0; MIN_STRIPE_BYTES + 1]),
        bad("raw_len", MIN_STRIPE_BYTES as u64 + 1)
    );
    assert_eq!(
        refusal(FrameKind::Chunk, &vec![0; BLOCK_SIZE]),
        bad("raw_len", BLOCK_SIZE as u64)
    );
}

#[test]
fn record_counts_beyond_the_payload_are_refused_before_any_allocation() {
    let ((), largest) = largest_allocation(|| {
        // The wire carries at most MAX_WIRE_RANGES ranges; a count above it
        // is refused before any range is read, one within it once the
        // ranges run out.
        for claim in [2u32, MAX_WIRE_RANGES as u32] {
            let ack = hello_ack_payload(&cursor_body(claim), &[ChunkRange::new(5, 7)]);
            assert_eq!(
                refusal(FrameKind::HelloAck, &ack),
                Some(StreamError::BodyTooShort {
                    needed: 64 + claim as usize * 8,
                    have: 72
                }),
                "sparse_ranges {claim}"
            );
        }
        for claim in [MAX_WIRE_RANGES as u32 + 1, 1 << 16, 1 << 24, u32::MAX] {
            let ack = hello_ack_payload(&cursor_body(claim), &[ChunkRange::new(5, 7)]);
            assert_eq!(
                refusal(FrameKind::HelloAck, &ack),
                bad("sparse_ranges", u64::from(claim)),
                "sparse_ranges {claim}"
            );
        }
        for claim in [2u32, 1 << 16, 1 << 24, u32::MAX] {
            let claimed = claim as usize;
            let tags = delta_begin_payload(
                &DeltaBeginBody {
                    tags: claim,
                    ..delta_begin_body()
                },
                &[snap_tag(5)],
                &[],
            );
            assert_eq!(
                refusal(FrameKind::DeltaBegin, &tags),
                Some(StreamError::BodyTooShort {
                    needed: 192 + claimed * 24,
                    have: 216
                }),
                "tags {claim}"
            );
            let acked = delta_begin_payload(
                &DeltaBeginBody {
                    tags: 1,
                    acked: claim,
                    ..delta_begin_body()
                },
                &[snap_tag(5)],
                &[acked_bytes(1)],
            );
            assert_eq!(
                refusal(FrameKind::DeltaBegin, &acked),
                Some(StreamError::BodyTooShort {
                    needed: 216 + claimed * 16,
                    have: 232
                }),
                "acked {claim}"
            );
        }
        // A name is bounded before its bytes are looked for.
        let short = vol_event_payload(&vol_event_body(VolEventKind::SnapTag, 255), b"abc");
        assert_eq!(
            refusal(FrameKind::VolEvent, &short),
            Some(StreamError::BodyTooShort {
                needed: 64 + 255,
                have: 67
            })
        );
        let over = vol_event_payload(&vol_event_body(VolEventKind::SnapTag, u16::MAX), b"abc");
        assert_eq!(refusal(FrameKind::VolEvent, &over), bad("name_len", 65535));
    });
    assert!(
        largest < PROBE_LIMIT,
        "decoding a hostile record count allocated {largest} bytes at once"
    );
}

// ---------------------------------------------------------------------------
// 4. Every enumeration, bound and canonical form behind valid CRCs
// ---------------------------------------------------------------------------

#[test]
fn delta_begin_enumerations_bounds_and_canonical_form() {
    let begin = |body: DeltaBeginBody, tags: &[SnapTag], acked: &[AckedBytes]| {
        let body = DeltaBeginBody {
            tags: tags.len() as u32,
            acked: acked.len() as u32,
            ..body
        };
        refusal(
            FrameKind::DeltaBegin,
            &delta_begin_payload(&body, tags, acked),
        )
    };
    let base = delta_begin_body();
    assert_eq!(begin(base, &[snap_tag(1)], &[acked_bytes(2)]), None);
    for mode in [0u8, 3, 4, 0x7F, 0xFF] {
        assert_eq!(
            begin(DeltaBeginBody { mode, ..base }, &[], &[]),
            bad("mode", u64::from(mode))
        );
    }
    let increment = |from: Uuid, to: Uuid| DeltaBeginBody {
        mode: 2,
        from_snap: from,
        to_snap: to,
        ..base
    };
    assert_eq!(begin(increment(id(3), id(4)), &[], &[]), None);
    for (from, to) in [
        (Uuid::NIL, id(4)),
        (id(3), Uuid::NIL),
        (id(3), id(3)),
        (Uuid::NIL, Uuid::NIL),
    ] {
        assert_eq!(begin(increment(from, to), &[], &[]), bad("mode", 2));
    }
    // A steady delta carries no snapshots.
    for (from, to) in [(id(3), Uuid::NIL), (Uuid::NIL, id(4)), (id(3), id(4))] {
        let steady = DeltaBeginBody {
            from_snap: from,
            to_snap: to,
            ..base
        };
        assert_eq!(begin(steady, &[], &[]), bad("mode", 1));
    }
    // Chunks, and the bytes they carry: between that many smallest and
    // largest stripe images.
    for chunks in [1, 7, MAX_DELTA_CHUNKS] {
        let min = u64::from(chunks) * MIN_STRIPE_BYTES as u64;
        let max = u64::from(chunks) * MAX_STRIPE_BYTES as u64;
        for bytes in [min, max] {
            let body = DeltaBeginBody {
                chunks,
                bytes,
                ..base
            };
            assert_eq!(
                begin(body, &[], &[]),
                None,
                "{chunks} chunks, {bytes} bytes"
            );
        }
        for bytes in [0, min - 1, max + 1, u64::MAX] {
            let body = DeltaBeginBody {
                chunks,
                bytes,
                ..base
            };
            assert_eq!(begin(body, &[], &[]), bad("bytes", bytes));
        }
    }
    for chunks in [0, MAX_DELTA_CHUNKS + 1, u32::MAX] {
        assert_eq!(
            begin(DeltaBeginBody { chunks, ..base }, &[], &[]),
            bad("chunks", u64::from(chunks))
        );
    }
    // Records: a snapshot is named, a volume is in range, reserved words
    // are zero.
    let nil_tag = SnapTag {
        uuid: Uuid::NIL,
        ..snap_tag(1)
    };
    assert_eq!(
        begin(base, &[snap_tag(1), nil_tag], &[]),
        bad("tag.uuid", 0)
    );
    let reserved_tag = SnapTag {
        rsvd: 9,
        ..snap_tag(1)
    };
    assert_eq!(begin(base, &[reserved_tag], &[]), bad("tag.rsvd", 9));
    assert_eq!(begin(base, &[], &[acked_bytes(MAX_VOL)]), None);
    for vol in [MAX_VOL + 1, u32::MAX] {
        assert_eq!(
            begin(base, &[], &[acked_bytes(0), acked_bytes(vol)]),
            bad("acked.vol", u64::from(vol))
        );
    }
    let reserved_acked = AckedBytes {
        rsvd: 9,
        ..acked_bytes(1)
    };
    assert_eq!(begin(base, &[], &[reserved_acked]), bad("acked.rsvd", 9));
    // Reserved bytes of the body.
    for at in 0..3 {
        let mut rsvd0 = [0u8; 3];
        rsvd0[at] = 0x40;
        assert_eq!(
            begin(DeltaBeginBody { rsvd0, ..base }, &[], &[]),
            bad("rsvd0", 0x40)
        );
    }
    assert_eq!(
        begin(DeltaBeginBody { rsvd1: 2, ..base }, &[], &[]),
        bad("rsvd1", 2)
    );
}

#[test]
fn vol_event_enumerations_bounds_and_per_event_fields() {
    let event = |body: VolEventBody, name: &[u8]| {
        let body = VolEventBody {
            name_len: name.len() as u16,
            ..body
        };
        refusal(FrameKind::VolEvent, &vol_event_payload(&body, name))
    };
    let create = vol_event_body(VolEventKind::Create, 0);
    let resize = vol_event_body(VolEventKind::Resize, 0);
    let delete = vol_event_body(VolEventKind::Delete, 0);
    let snap = vol_event_body(VolEventKind::SnapTag, 0);
    for value in [0u8, 5, 6, 0xFF] {
        assert_eq!(
            event(
                VolEventBody {
                    event: value,
                    ..create
                },
                b""
            ),
            bad("event", u64::from(value))
        );
    }
    for class in 1..=5u8 {
        assert_eq!(event(VolEventBody { class, ..create }, b"x"), None);
    }
    for class in [0u8, 6, 7, 0xFF] {
        assert_eq!(
            event(VolEventBody { class, ..create }, b""),
            bad("class", u64::from(class))
        );
    }
    for body in [resize, delete, snap] {
        assert_eq!(event(body, b""), None, "{}", body.event);
        for class in [1u8, 5, 6, 0xFF] {
            assert_eq!(
                event(VolEventBody { class, ..body }, b""),
                bad("class", u64::from(class)),
                "{}",
                body.event
            );
        }
    }
    for body in [create, resize, delete, snap] {
        assert_eq!(
            event(
                VolEventBody {
                    vol: MAX_VOL,
                    ..body
                },
                b""
            ),
            None
        );
        for vol in [MAX_VOL + 1, u32::MAX] {
            assert_eq!(
                event(VolEventBody { vol, ..body }, b""),
                bad("vol", u64::from(vol)),
                "{}",
                body.event
            );
        }
        for at in [0usize, 23] {
            let mut rsvd = [0u8; 24];
            rsvd[at] = 3;
            assert_eq!(
                event(VolEventBody { rsvd, ..body }, b""),
                bad("rsvd", 3),
                "{}",
                body.event
            );
        }
    }

    // Create: a class, a size, a stripe geometry and the volume's uuid; no
    // retention.
    for size_blocks in [1, MAX_LBA + 1] {
        assert_eq!(
            event(
                VolEventBody {
                    size_blocks,
                    ..create
                },
                b""
            ),
            None
        );
        assert_eq!(
            event(
                VolEventBody {
                    size_blocks,
                    ..resize
                },
                b""
            ),
            None
        );
    }
    for size_blocks in [0, MAX_LBA + 2, u64::MAX] {
        assert_eq!(
            event(
                VolEventBody {
                    size_blocks,
                    ..create
                },
                b""
            ),
            bad("size_blocks", size_blocks)
        );
        assert_eq!(
            event(
                VolEventBody {
                    size_blocks,
                    ..resize
                },
                b""
            ),
            bad("size_blocks", size_blocks)
        );
    }
    for stripe_bytes in [MIN_STRIPE_BYTES as u32, MAX_STRIPE_BYTES as u32] {
        assert_eq!(
            event(
                VolEventBody {
                    stripe_bytes,
                    ..create
                },
                b""
            ),
            None
        );
    }
    for stripe_bytes in [
        0,
        BLOCK_SIZE as u32,
        MIN_STRIPE_BYTES as u32 + 1,
        (MAX_STRIPE_BYTES + BLOCK_SIZE) as u32,
        u32::MAX,
    ] {
        assert_eq!(
            event(
                VolEventBody {
                    stripe_bytes,
                    ..create
                },
                b""
            ),
            bad("stripe_bytes", u64::from(stripe_bytes))
        );
    }
    assert_eq!(
        event(
            VolEventBody {
                retention_s: 1,
                ..create
            },
            b""
        ),
        bad("retention_s", 1)
    );
    assert_eq!(
        event(
            VolEventBody {
                uuid: Uuid::NIL,
                ..create
            },
            b""
        ),
        bad("uuid", 0)
    );

    // Resize and Delete carry nothing they do not use; a SnapTag carries no
    // size or geometry. (Which field these name is pinned by
    // `bug_vol_event_refusals_name_the_wrong_field`.)
    for (what, body, name) in [
        (
            "a stripe geometry on a Resize",
            VolEventBody {
                stripe_bytes: 1 << 20,
                ..resize
            },
            &b""[..],
        ),
        (
            "a retention on a Resize",
            VolEventBody {
                retention_s: 1,
                ..resize
            },
            b"",
        ),
        (
            "a uuid on a Resize",
            VolEventBody {
                uuid: id(9),
                ..resize
            },
            b"",
        ),
        ("a name on a Resize", resize, b"n"),
        (
            "a size on a Delete",
            VolEventBody {
                size_blocks: 1,
                ..delete
            },
            b"",
        ),
        (
            "a stripe geometry on a Delete",
            VolEventBody {
                stripe_bytes: 1 << 20,
                ..delete
            },
            b"",
        ),
        (
            "a retention on a Delete",
            VolEventBody {
                retention_s: 1,
                ..delete
            },
            b"",
        ),
        (
            "a uuid on a Delete",
            VolEventBody {
                uuid: id(9),
                ..delete
            },
            b"",
        ),
        ("a name on a Delete", delete, b"n"),
        (
            "a size on a SnapTag",
            VolEventBody {
                size_blocks: 1,
                ..snap
            },
            b"",
        ),
        (
            "a stripe geometry on a SnapTag",
            VolEventBody {
                stripe_bytes: 1 << 20,
                ..snap
            },
            b"",
        ),
        (
            "a nil uuid on a SnapTag",
            VolEventBody {
                uuid: Uuid::NIL,
                ..snap
            },
            b"",
        ),
    ] {
        assert!(
            matches!(event(body, name), Some(StreamError::BadBody { .. })),
            "{what} was accepted"
        );
    }
    // A SnapTag may carry any retention and a name.
    for retention_s in [0, u32::MAX] {
        assert_eq!(
            event(
                VolEventBody {
                    retention_s,
                    ..snap
                },
                b"daily"
            ),
            None
        );
    }

    // Names: a stray continuation byte, an invalid lead byte, an overlong
    // encoding, a surrogate, a character cut at the end.
    for name in [
        &b"\xFF"[..],
        b"ok\x80",
        b"\xC0\x80",
        b"\xED\xA0\x80",
        b"name\xE2\x82",
    ] {
        assert_eq!(
            event(snap, name),
            bad("name", name.len() as u64),
            "{name:?}"
        );
    }
    let longest = "\u{1F600}".repeat(63) + "abc";
    assert_eq!(longest.len(), MAX_NAME_LEN);
    assert_eq!(event(create, longest.as_bytes()), None);
    let over = vec![b'n'; MAX_NAME_LEN + 1];
    assert_eq!(event(snap, &over), bad("name_len", over.len() as u64));
}

#[test]
fn abort_reasons_and_chunk_indices() {
    for reason in 1..=6u8 {
        assert_eq!(
            refusal(FrameKind::Abort, abort_body(reason).as_bytes()),
            None
        );
    }
    for reason in [0u8, 7, 0x80, 0xFF] {
        assert_eq!(
            refusal(FrameKind::Abort, abort_body(reason).as_bytes()),
            bad("reason", u64::from(reason))
        );
    }
    // `idx` names a chunk of a delta for `Corrupt`, and is 0 otherwise.
    let corrupt = abort_body(AbortReason::Corrupt.as_u8());
    for idx in [0, MAX_DELTA_CHUNKS - 1] {
        let body = AbortBody { idx, ..corrupt };
        assert_eq!(refusal(FrameKind::Abort, body.as_bytes()), None);
    }
    for idx in [MAX_DELTA_CHUNKS, u32::MAX] {
        let body = AbortBody { idx, ..corrupt };
        assert_eq!(
            refusal(FrameKind::Abort, body.as_bytes()),
            bad("idx", u64::from(idx))
        );
    }
    for reason in 2..=6u8 {
        let body = AbortBody {
            idx: 1,
            ..abort_body(reason)
        };
        assert_eq!(refusal(FrameKind::Abort, body.as_bytes()), bad("idx", 1));
    }
    let reserved = AbortBody {
        rsvd: [0, 0, 7],
        ..corrupt
    };
    assert_eq!(
        refusal(FrameKind::Abort, reserved.as_bytes()),
        bad("rsvd", 7)
    );
}

#[test]
fn hello_bounds() {
    let hello = hello_body();
    for max_payload in [
        MIN_STRIPE_BYTES as u32,
        MIN_STRIPE_BYTES as u32 + 1,
        MAX_WIRE_PAYLOAD,
    ] {
        let body = HelloBody {
            max_payload,
            ..hello
        };
        assert_eq!(refusal(FrameKind::Hello, body.as_bytes()), None);
    }
    for max_payload in [
        0,
        BLOCK_SIZE as u32,
        MIN_STRIPE_BYTES as u32 - 1,
        MAX_WIRE_PAYLOAD + 1,
        u32::MAX,
    ] {
        let body = HelloBody {
            max_payload,
            ..hello
        };
        assert_eq!(
            refusal(FrameKind::Hello, body.as_bytes()),
            bad("max_payload", u64::from(max_payload))
        );
    }
    for initial_credits in [0, MAX_CREDITS] {
        let body = HelloBody {
            initial_credits,
            ..hello
        };
        assert_eq!(refusal(FrameKind::Hello, body.as_bytes()), None);
    }
    for initial_credits in [MAX_CREDITS + 1, u32::MAX] {
        let body = HelloBody {
            initial_credits,
            ..hello
        };
        assert_eq!(
            refusal(FrameKind::Hello, body.as_bytes()),
            bad("initial_credits", u64::from(initial_credits))
        );
    }
    let nil_node = HelloBody {
        node_id: Uuid::NIL,
        ..hello
    };
    assert_eq!(
        refusal(FrameKind::Hello, nil_node.as_bytes()),
        bad("node_id", 0)
    );
    let nil_pipe = HelloBody {
        pipe_id: Uuid::NIL,
        ..hello
    };
    assert_eq!(
        refusal(FrameKind::Hello, nil_pipe.as_bytes()),
        bad("pipe_id", 0)
    );
    let reserved = HelloBody {
        rsvd: [0, 0, 0, 5, 0, 0, 0, 0],
        ..hello
    };
    assert_eq!(
        refusal(FrameKind::Hello, reserved.as_bytes()),
        bad("rsvd", 5)
    );
    // Unknown capability bits are the one thing a receiver ignores.
    let every_cap = HelloBody {
        capabilities: u64::MAX,
        ..hello
    };
    match decode_body(&raw_wire(FrameKind::Hello, every_cap.as_bytes())) {
        Ok(Frame::Hello(h)) => {
            assert!(h.supports(CAP_CODEC_LZ4 | CAP_CODEC_ZSTD));
            assert_eq!(h.capabilities, u64::MAX);
        }
        other => panic!("{other:?}"),
    }
    // The encoder refuses what the decoder refuses.
    let over = Frame::Hello(Hello {
        initial_credits: MAX_CREDITS + 1,
        ..sample_hello()
    });
    assert_eq!(
        Encoder::new().encode(&FrameMeta::default(), &over),
        Err(StreamError::BadBody {
            field: "initial_credits",
            value: u64::from(MAX_CREDITS) + 1
        })
    );
}

#[test]
fn committed_delta_end_and_landed_bounds() {
    for applied_cp_seq in [0, MAX_CP_SEQ] {
        let body = CommittedBody {
            applied_cp_seq,
            ..committed_body()
        };
        assert_eq!(refusal(FrameKind::Committed, body.as_bytes()), None);
    }
    for applied_cp_seq in [MAX_CP_SEQ + 1, u64::MAX] {
        let body = CommittedBody {
            applied_cp_seq,
            ..committed_body()
        };
        assert_eq!(
            refusal(FrameKind::Committed, body.as_bytes()),
            bad("applied_cp_seq", applied_cp_seq)
        );
        let frame = Frame::Committed(Committed {
            applied_cp_seq,
            applied_epoch: 0,
        });
        assert_eq!(
            Encoder::new().encode(&FrameMeta::default(), &frame),
            Err(StreamError::BadBody {
                field: "applied_cp_seq",
                value: applied_cp_seq
            })
        );
    }

    for chunks in [1, MAX_DELTA_CHUNKS] {
        let min = u64::from(chunks) * MIN_STRIPE_BYTES as u64;
        let max = u64::from(chunks) * MAX_STRIPE_BYTES as u64;
        for bytes in [min, max] {
            let body = DeltaEndBody {
                chunks,
                bytes,
                ..delta_end_body()
            };
            assert_eq!(refusal(FrameKind::DeltaEnd, body.as_bytes()), None);
        }
        for bytes in [min - 1, max + 1] {
            let body = DeltaEndBody {
                chunks,
                bytes,
                ..delta_end_body()
            };
            assert_eq!(
                refusal(FrameKind::DeltaEnd, body.as_bytes()),
                bad("bytes", bytes)
            );
        }
    }
    for chunks in [0, MAX_DELTA_CHUNKS + 1, u32::MAX] {
        let body = DeltaEndBody {
            chunks,
            ..delta_end_body()
        };
        assert_eq!(
            refusal(FrameKind::DeltaEnd, body.as_bytes()),
            bad("chunks", u64::from(chunks))
        );
    }
    let reserved = DeltaEndBody {
        rsvd: 4,
        ..delta_end_body()
    };
    assert_eq!(
        refusal(FrameKind::DeltaEnd, reserved.as_bytes()),
        bad("rsvd", 4)
    );

    // Landed: a chunk of a delta, a landed prefix no longer than a delta, a
    // grant no larger than any window.
    let landed = landed_body();
    for body in [
        LandedBody {
            idx: MAX_DELTA_CHUNKS - 1,
            ..landed
        },
        LandedBody {
            landed_contig: MAX_DELTA_CHUNKS,
            ..landed
        },
        LandedBody {
            credits: MAX_CREDITS,
            ..landed
        },
        LandedBody {
            idx: 0,
            credits: 0,
            landed_contig: 0,
            rsvd: 0,
        },
    ] {
        assert_eq!(refusal(FrameKind::Landed, body.as_bytes()), None);
    }
    for (body, expected) in [
        (
            LandedBody {
                idx: MAX_DELTA_CHUNKS,
                ..landed
            },
            bad("idx", u64::from(MAX_DELTA_CHUNKS)),
        ),
        (
            LandedBody {
                landed_contig: MAX_DELTA_CHUNKS + 1,
                ..landed
            },
            bad("landed_contig", u64::from(MAX_DELTA_CHUNKS) + 1),
        ),
        (
            LandedBody {
                credits: MAX_CREDITS + 1,
                ..landed
            },
            bad("credits", u64::from(MAX_CREDITS) + 1),
        ),
        (LandedBody { rsvd: 1, ..landed }, bad("rsvd", 1)),
    ] {
        assert_eq!(refusal(FrameKind::Landed, body.as_bytes()), expected);
    }

    // Ping and Pong carry no enumeration: every value decodes.
    for v in [0u64, u64::MAX] {
        let body = PingBody {
            nonce: v,
            mono_ns: v,
        };
        assert_eq!(refusal(FrameKind::Ping, body.as_bytes()), None);
        assert_eq!(refusal(FrameKind::Pong, body.as_bytes()), None);
    }
}

#[test]
fn hole_records_are_validated_one_by_one() {
    let ok = hole(3, 100, 8);
    assert_eq!(refusal(FrameKind::Holes, ok.as_bytes()), None);
    assert_eq!(
        refusal(FrameKind::Holes, hole(MAX_VOL, MAX_LBA, 1).as_bytes()),
        None
    );
    assert_eq!(
        refusal(FrameKind::Holes, hole(0, 0, u32::MAX).as_bytes()),
        None
    );
    for (record, expected) in [
        (
            hole(MAX_VOL + 1, 0, 1),
            FormatError::OutOfRange {
                field: "vol",
                value: u64::from(MAX_VOL) + 1,
                max: u64::from(MAX_VOL),
            },
        ),
        (
            hole(1, 0, 0),
            FormatError::BadField {
                field: "len",
                value: 0,
            },
        ),
        (
            hole(1, MAX_LBA, 2),
            FormatError::OutOfRange {
                field: "lba",
                value: MAX_LBA,
                max: MAX_LBA,
            },
        ),
        (
            hole(1, u64::MAX, 1),
            FormatError::OutOfRange {
                field: "lba",
                value: u64::MAX,
                max: MAX_LBA,
            },
        ),
    ] {
        let cases: [&[HoleRecord]; 3] = [&[record], &[ok, record], &[record, ok]];
        for records in cases {
            assert_eq!(
                refusal(FrameKind::Holes, records.as_bytes()),
                Some(StreamError::Format(expected))
            );
        }
    }
}

// ---------------------------------------------------------------------------
// 5. Codecs, kinds and the header context
// ---------------------------------------------------------------------------

#[test]
fn compressed_chunks_pass_through_with_their_raw_len() {
    let opaque = Bytes::from_static(b"\x04\x22\x4d\x18 compressed bytes are opaque to the codec");
    let meta = sample_meta();
    for codec in [Codec::Lz4, Codec::Zstd] {
        let raw_len = 3 * BLOCK_SIZE as u32;
        let frame = Frame::Chunk(Chunk {
            image: opaque.clone(),
            codec,
            raw_len,
        });
        let bytes = encode(&meta, &frame);
        let hdr = StreamHeader::read(&bytes[..STREAM_HEADER_SIZE]).unwrap();
        assert_eq!(hdr.flags, codec.as_u8());
        assert_eq!(
            (hdr.wire_len as usize, hdr.raw_len),
            (opaque.len(), raw_len)
        );
        assert_eq!(
            &bytes[..],
            &wire(FrameKind::Chunk, codec, &meta, &opaque, raw_len as usize)[..]
        );
        let mut decoder = Decoder::new();
        decoder.feed(&bytes);
        let (m, f) = decoder.next_frame().unwrap().unwrap().decode().unwrap();
        assert_eq!((m, &f), (meta, &frame));
        let Frame::Chunk(chunk) = f else {
            panic!("a chunk")
        };
        assert_eq!(chunk.validate(), Ok(()));
        assert!(matches!(
            chunk.stripe_hdr(),
            Err(StreamError::BadBody { field: "codec", .. })
        ));
        assert!(matches!(
            chunk.summary(),
            Err(StreamError::BadBody { field: "codec", .. })
        ));
    }
}

#[test]
fn every_other_kind_refuses_a_codec() {
    for frame in sample_frames() {
        if frame.kind() == FrameKind::Chunk {
            continue;
        }
        let payload = payload_of(&frame);
        for codec in [Codec::Lz4, Codec::Zstd] {
            for raw_len in [payload.len(), 2 * payload.len(), MAX_WIRE_PAYLOAD as usize] {
                let wire = wire(frame.kind(), codec, &sample_meta(), &payload, raw_len);
                assert_eq!(
                    decode_body(&wire),
                    Err(StreamError::BadBody {
                        field: "codec",
                        value: u64::from(codec.as_u8())
                    }),
                    "{:?} with {codec:?}",
                    frame.kind()
                );
            }
        }
    }
}

#[test]
fn undefined_codecs_and_flag_bits_poison_at_the_header() {
    let body = ping_body();
    for flags in [3u8, 4, 7, 0x80, 0xFF] {
        let mut hdr = StreamHeader::new(FrameKind::Ping, Codec::Raw);
        hdr.set_payload(body.as_bytes()).unwrap();
        hdr.flags = flags;
        hdr.seal();
        let mut decoder = Decoder::new();
        decoder.feed(&[hdr.as_bytes(), body.as_bytes()].concat());
        let field = if flags & !0b11 == 0 { "codec" } else { "flags" };
        let err = decoder.next_frame().unwrap_err();
        assert!(
            matches!(err, StreamError::Format(FormatError::BadField { field: f, .. }) if f == field),
            "{flags:#04x}: {err:?}"
        );
        assert!(decoder.is_poisoned());
    }
    let mut odd = StreamHeader::new(FrameKind::Ping, Codec::Raw);
    odd.set_payload(body.as_bytes()).unwrap();
    odd.raw_len += 1;
    odd.seal();
    let mut decoder = Decoder::new();
    decoder.feed(&[odd.as_bytes(), body.as_bytes()].concat());
    assert!(matches!(
        decoder.next_frame(),
        Err(StreamError::Format(FormatError::BadField {
            field: "raw_len",
            ..
        }))
    ));
}

#[test]
fn the_header_for_a_held_payload_refuses_another_length() {
    let meta = sample_meta();
    for frame in sample_frames() {
        let payload = payload_of(&frame);
        let len = payload.len();
        assert!(Encoder::new().header(&meta, &frame, &payload).is_ok());
        let longer = [&payload[..], &[0]].concat();
        assert_eq!(
            Encoder::new().header(&meta, &frame, &longer),
            Err(StreamError::BodyTooLong {
                expected: len,
                found: len + 1
            }),
            "{:?}",
            frame.kind()
        );
        for short in [len - 1, 0] {
            assert_eq!(
                Encoder::new().header(&meta, &frame, &payload[..short]),
                Err(StreamError::BodyTooShort {
                    needed: len,
                    have: short
                }),
                "{:?}",
                frame.kind()
            );
        }
    }
}

#[test]
fn chunk_idx_is_zero_off_chunks_and_bounded_on_them() {
    for frame in sample_frames() {
        let kind = frame.kind();
        let last_ok = if kind == FrameKind::Chunk {
            MAX_DELTA_CHUNKS - 1
        } else {
            0
        };
        let ok = FrameMeta {
            chunk_idx: last_ok,
            ..sample_meta()
        };
        assert_eq!(ok.check_chunk_idx(kind), Ok(()));
        let payload = payload_of(&frame);
        let encoded = encode(&ok, &frame);
        assert!(decode_body(&encoded) == Ok(frame.clone()), "{kind:?}");
        for chunk_idx in [last_ok + 1, u32::MAX] {
            let over = FrameMeta {
                chunk_idx,
                ..sample_meta()
            };
            let expected = StreamError::BadBody {
                field: "chunk_idx",
                value: u64::from(chunk_idx),
            };
            assert_eq!(over.check_chunk_idx(kind), Err(expected));
            // The encoder refuses it at every entry point...
            assert_eq!(
                Encoder::new().encode(&over, &frame),
                Err(expected),
                "{kind:?}"
            );
            assert_eq!(
                Encoder::new().header(&over, &frame, &payload),
                Err(expected)
            );
            let mut out = BytesMut::from(&b"held"[..]);
            assert_eq!(
                Encoder::new().encode_into(&over, &frame, &mut out),
                Err(expected)
            );
            assert_eq!(&out[..], b"held");
            // ...and the decoder refuses it behind valid CRCs, in sync.
            let on_wire = wire(kind, Codec::Raw, &over, &payload, payload.len());
            assert_eq!(decode_body(&on_wire), Err(expected), "{kind:?}");
        }
    }
}

#[test]
fn the_encoder_refuses_headers_its_decoder_would_refuse() {
    let frame = ping();
    let payload = payload_of(&frame);
    for (meta, expected) in [
        (
            FrameMeta {
                cp_seq: MAX_CP_SEQ + 1,
                ..FrameMeta::default()
            },
            StreamError::Format(FormatError::OutOfRange {
                field: "cp_seq",
                value: MAX_CP_SEQ + 1,
                max: MAX_CP_SEQ,
            }),
        ),
        (
            FrameMeta {
                cp_seq: 5,
                base: 6,
                ..FrameMeta::default()
            },
            StreamError::Format(FormatError::BadField {
                field: "base",
                value: 6,
            }),
        ),
    ] {
        assert_eq!(Encoder::new().encode(&meta, &frame), Err(expected));
        assert_eq!(
            Encoder::new().header(&meta, &frame, &payload),
            Err(expected)
        );
        let mut out = BytesMut::from(&b"held"[..]);
        assert_eq!(
            Encoder::new().encode_into(&meta, &frame, &mut out),
            Err(expected)
        );
        assert_eq!(&out[..], b"held");
    }
    // The bounds themselves are fine.
    let last = FrameMeta {
        cp_seq: MAX_CP_SEQ,
        base: MAX_CP_SEQ,
        ..FrameMeta::default()
    };
    assert!(decode_body(&encode(&last, &frame)) == Ok(frame));
}

#[test]
fn encoding_an_invalid_frame_appends_nothing() {
    let _guard = heavy();
    let over_bound = MAX_WIRE_PAYLOAD as usize / HOLE_RECORD_SIZE + 1;
    let invalid = [
        Frame::Hello(Hello {
            max_payload: 0,
            ..sample_hello()
        }),
        Frame::Hello(Hello {
            pipe_id: Uuid::NIL,
            ..sample_hello()
        }),
        Frame::Hello(Hello {
            initial_credits: MAX_CREDITS + 1,
            ..sample_hello()
        }),
        Frame::HelloAck(HelloAck {
            cursor: Cursor::new(MAX_CP_SEQ + 1, 0, 0),
        }),
        Frame::DeltaBegin(DeltaBegin {
            chunks: 0,
            ..sample_delta_begin()
        }),
        Frame::DeltaBegin(DeltaBegin {
            bytes: 40 * MIN_STRIPE_BYTES as u64 - 1,
            ..sample_delta_begin()
        }),
        Frame::DeltaBegin(DeltaBegin {
            mode: DeltaMode::Increment {
                from: id(3),
                to: id(3),
            },
            ..sample_delta_begin()
        }),
        Frame::DeltaBegin(DeltaBegin {
            acked: vec![acked_bytes(MAX_VOL + 1)],
            ..sample_delta_begin()
        }),
        Frame::DeltaBegin(DeltaBegin {
            tags: vec![SnapTag {
                rsvd: 1,
                ..snap_tag(5)
            }],
            ..sample_delta_begin()
        }),
        Frame::VolEvent(VolEvent {
            event: VolEventKind::SnapTag,
            vol: 1,
            class: None,
            size_blocks: 0,
            stripe_bytes: 0,
            retention_s: 0,
            uuid: id(3),
            name: "n".repeat(MAX_NAME_LEN + 1),
        }),
        Frame::VolEvent(VolEvent {
            event: VolEventKind::Create,
            vol: 1,
            class: None,
            size_blocks: 1,
            stripe_bytes: 1 << 20,
            retention_s: 0,
            uuid: id(3),
            name: String::new(),
        }),
        Frame::VolEvent(VolEvent {
            event: VolEventKind::Create,
            vol: 1,
            class: Some(VolClass::Object),
            size_blocks: 1,
            stripe_bytes: 0,
            retention_s: 0,
            uuid: id(3),
            name: String::new(),
        }),
        Frame::VolEvent(VolEvent {
            event: VolEventKind::Delete,
            vol: 1,
            class: None,
            size_blocks: 0,
            stripe_bytes: 0,
            retention_s: 0,
            uuid: Uuid::NIL,
            name: "gone".to_owned(),
        }),
        Frame::Chunk(Chunk {
            image: Bytes::from_static(&[0; 100]),
            codec: Codec::Raw,
            raw_len: 100,
        }),
        Frame::Chunk(Chunk {
            image: sample_image(MIN_STRIPE_BYTES),
            codec: Codec::Raw,
            raw_len: 0,
        }),
        Frame::Chunk(Chunk {
            image: Bytes::from_static(&[0; 100]),
            codec: Codec::Lz4,
            raw_len: MAX_WIRE_PAYLOAD + 1,
        }),
        Frame::Chunk(Chunk {
            image: Bytes::new(),
            codec: Codec::Zstd,
            raw_len: MIN_STRIPE_BYTES as u32,
        }),
        Frame::Holes(Holes {
            records: Vec::new(),
        }),
        Frame::Holes(Holes {
            records: vec![hole(1, 0, 0)],
        }),
        Frame::Holes(Holes {
            records: vec![hole(1, 0, 1); over_bound],
        }),
        Frame::DeltaEnd(DeltaEnd {
            digest: [0; 32],
            chunks: 0,
            bytes: 0,
        }),
        Frame::DeltaEnd(DeltaEnd {
            digest: [0; 32],
            chunks: 4,
            bytes: 1,
        }),
        Frame::Landed(Landed {
            idx: MAX_DELTA_CHUNKS,
            credits: 0,
            landed_contig: 0,
        }),
        Frame::Landed(Landed {
            idx: 0,
            credits: MAX_CREDITS + 1,
            landed_contig: 0,
        }),
        Frame::Committed(Committed {
            applied_cp_seq: MAX_CP_SEQ + 1,
            applied_epoch: 0,
        }),
        Frame::Abort(Abort {
            reason: AbortReason::NoSpace,
            idx: 1,
            detail: 0,
        }),
    ];
    let prefix = b"bytes already in the buffer";
    for frame in &invalid {
        let mut out = BytesMut::from(&prefix[..]);
        assert!(
            Encoder::new()
                .encode_into(&sample_meta(), frame, &mut out)
                .is_err(),
            "{:?} was encoded",
            frame.kind()
        );
        assert_eq!(
            &out[..],
            &prefix[..],
            "{:?} left bytes behind",
            frame.kind()
        );
        assert!(Encoder::new().encode(&sample_meta(), frame).is_err());
        let mut payload = BytesMut::new();
        assert!(frame.encode_payload(&mut payload).is_err());
        assert!(payload.is_empty());
    }
}

// ---------------------------------------------------------------------------
// 6. The cursor
// ---------------------------------------------------------------------------

#[test]
fn hello_ack_sparse_ranges_must_be_canonical_and_in_bounds() {
    let r = ChunkRange::new;
    let ack = |ranges: &[ChunkRange]| {
        refusal(
            FrameKind::HelloAck,
            &hello_ack_payload(&cursor_body(ranges.len() as u32), ranges),
        )
    };
    // The cursor body: 20 chunks, landed_contig 3.
    assert_eq!(ack(&[r(5, 8), r(10, 12)]), None);
    assert_eq!(ack(&[r(4, 20)]), None);
    assert_eq!(ack(&[r(5, 8), r(9, 10), r(19, 20)]), None);
    assert_eq!(ack(&[r(5, 8), r(7, 9)]), bad("sparse", 7), "overlapping");
    assert_eq!(ack(&[r(5, 7), r(7, 9)]), bad("sparse", 7), "adjacent");
    assert_eq!(ack(&[r(10, 12), r(5, 7)]), bad("sparse", 5), "unordered");
    assert_eq!(ack(&[r(2, 4)]), bad("sparse", 2), "below landed_contig");
    assert_eq!(ack(&[r(0, 2)]), bad("sparse", 0), "below landed_contig");
    assert_eq!(ack(&[r(3, 5)]), bad("sparse", 3), "at landed_contig");
    assert_eq!(ack(&[r(5, 5)]), bad("sparse", 5), "empty");
    assert_eq!(ack(&[r(7, 5)]), bad("sparse", 7), "inverted");
    assert_eq!(
        ack(&[r(18, 21)]),
        Some(StreamError::ChunkOutOfRange {
            idx: 20,
            chunks: 20
        })
    );
    assert_eq!(
        ack(&[r(5, u32::MAX)]),
        Some(StreamError::ChunkOutOfRange {
            idx: u32::MAX - 1,
            chunks: 20
        })
    );
    assert_eq!(
        ack(&[r(u32::MAX - 1, u32::MAX)]),
        Some(StreamError::ChunkOutOfRange {
            idx: u32::MAX - 1,
            chunks: 20
        })
    );
    let past = CursorBody {
        landed_contig: 21,
        ..cursor_body(0)
    };
    assert_eq!(
        refusal(FrameKind::HelloAck, past.as_bytes()),
        Some(StreamError::ChunkOutOfRange {
            idx: 21,
            chunks: 20
        })
    );
    let full = CursorBody {
        landed_contig: 20,
        ..cursor_body(1)
    };
    for range in [r(19, 20), r(20, 21)] {
        assert_eq!(
            refusal(FrameKind::HelloAck, &hello_ack_payload(&full, &[range])),
            bad("sparse", u64::from(range.start))
        );
    }
    // An accepted landing is exactly the one described.
    let payload = hello_ack_payload(&cursor_body(2), &[r(5, 8), r(10, 12)]);
    let Ok(Frame::HelloAck(HelloAck { cursor })) =
        decode_body(&raw_wire(FrameKind::HelloAck, &payload))
    else {
        panic!("a valid HelloAck")
    };
    let landing = cursor.landing.expect("a landing");
    assert_eq!((landing.cp_seq, landing.cp_nonce), (6, 7));
    assert_eq!(landing.bitmap.landed_contig(), 3);
    assert_eq!(landing.bitmap.sparse_ranges(), [r(5, 8), r(10, 12)]);
    let missing: Vec<u32> = landing.bitmap.missing().collect();
    let expected: Vec<u32> = [3, 4].into_iter().chain(8..10).chain(12..20).collect();
    assert_eq!(missing, expected);
}

#[test]
fn landing_fields_need_the_landing_flag_and_the_flag_needs_sane_fields() {
    let idle = CursorBody {
        applied_cp_seq: 5,
        applied_epoch: 1,
        credits: 9,
        ..CursorBody::default()
    };
    assert_eq!(refusal(FrameKind::HelloAck, idle.as_bytes()), None);
    for (what, body) in [
        (
            "landing_cp_seq",
            CursorBody {
                landing_cp_seq: 6,
                ..idle
            },
        ),
        (
            "landing_cp_nonce",
            CursorBody {
                landing_cp_nonce: 7,
                ..idle
            },
        ),
        ("chunks", CursorBody { chunks: 20, ..idle }),
        (
            "landed_contig",
            CursorBody {
                landed_contig: 3,
                ..idle
            },
        ),
    ] {
        assert!(
            matches!(
                refusal(FrameKind::HelloAck, body.as_bytes()),
                Some(StreamError::BadBody {
                    field: "landing",
                    ..
                })
            ),
            "{what} without the landing flag"
        );
    }
    let ranges_without_landing = CursorBody {
        sparse_ranges: 1,
        ..idle
    };
    assert!(matches!(
        refusal(
            FrameKind::HelloAck,
            &hello_ack_payload(&ranges_without_landing, &[ChunkRange::new(5, 7)])
        ),
        Some(StreamError::BadBody {
            field: "landing",
            ..
        })
    ));
    for flags in [2u32, 0x8000_0000, CURSOR_LANDING | 4] {
        let body = CursorBody {
            flags,
            ..cursor_body(0)
        };
        assert_eq!(
            refusal(FrameKind::HelloAck, body.as_bytes()),
            bad("flags", u64::from(flags))
        );
    }
    let rogue = CursorBody {
        applied_cp_seq: MAX_CP_SEQ + 1,
        ..idle
    };
    assert_eq!(
        refusal(FrameKind::HelloAck, rogue.as_bytes()),
        bad("applied_cp_seq", MAX_CP_SEQ + 1)
    );
    let landing = cursor_body(0);
    for cp in [0, 5, MAX_CP_SEQ + 1, u64::MAX] {
        let body = CursorBody {
            landing_cp_seq: cp,
            ..landing
        };
        assert_eq!(
            refusal(FrameKind::HelloAck, body.as_bytes()),
            bad("landing_cp_seq", cp)
        );
    }
    let last = CursorBody {
        landing_cp_seq: MAX_CP_SEQ,
        ..landing
    };
    assert_eq!(refusal(FrameKind::HelloAck, last.as_bytes()), None);
    for chunks in [0, MAX_DELTA_CHUNKS + 1, u32::MAX] {
        let body = CursorBody {
            chunks,
            landed_contig: 0,
            ..landing
        };
        assert_eq!(
            refusal(FrameKind::HelloAck, body.as_bytes()),
            bad("chunks", u64::from(chunks))
        );
    }
    // Credits stay within a window; reserved bytes are zero on the wire.
    let most = CursorBody {
        credits: MAX_CREDITS,
        ..landing
    };
    assert_eq!(refusal(FrameKind::HelloAck, most.as_bytes()), None);
    for credits in [MAX_CREDITS + 1, u32::MAX] {
        let body = CursorBody { credits, ..landing };
        assert_eq!(
            refusal(FrameKind::HelloAck, body.as_bytes()),
            bad("credits", u64::from(credits))
        );
    }
    for at in [0usize, 11] {
        let mut rsvd = [0u8; 12];
        rsvd[at] = 0x11;
        let body = CursorBody { rsvd, ..landing };
        assert_eq!(
            refusal(FrameKind::HelloAck, body.as_bytes()),
            bad("rsvd", 0x11)
        );
    }
}

#[test]
fn the_largest_landing_a_peer_can_describe_decodes_and_lands_at_both_ends() {
    let _guard = heavy();
    let complete = CursorBody {
        chunks: MAX_DELTA_CHUNKS,
        landed_contig: MAX_DELTA_CHUNKS,
        ..cursor_body(0)
    };
    match decode_body(&raw_wire(FrameKind::HelloAck, complete.as_bytes())) {
        Ok(Frame::HelloAck(ack)) => {
            let landing = ack.cursor.landing.expect("a landing");
            assert!(landing.bitmap.is_complete());
            assert_eq!(landing.bitmap.landed_count(), MAX_DELTA_CHUNKS);
        }
        other => panic!("{other:?}"),
    }
    assert!(ResumeBitmap::new(MAX_DELTA_CHUNKS).is_ok());

    let mut cursor = Cursor::new(0, 0, 0);
    cursor.begin(0, 1, 2, MAX_DELTA_CHUNKS).unwrap();
    assert_eq!(
        cursor.land(1, 2, MAX_DELTA_CHUNKS - 1),
        Ok(LandOutcome::New)
    );
    assert_eq!(cursor.land(1, 2, 0), Ok(LandOutcome::New));
    for idx in [MAX_DELTA_CHUNKS, u32::MAX] {
        assert_eq!(
            cursor.land(1, 2, idx),
            Err(StreamError::ChunkOutOfRange {
                idx,
                chunks: MAX_DELTA_CHUNKS
            })
        );
    }
    let bitmap = &cursor.landing.as_ref().unwrap().bitmap;
    assert_eq!((bitmap.landed_contig(), bitmap.landed_count()), (1, 2));
    assert_eq!(
        bitmap.sparse_ranges(),
        [ChunkRange::new(MAX_DELTA_CHUNKS - 1, MAX_DELTA_CHUNKS)]
    );
}

#[test]
fn chunk_indices_at_and_past_the_end() {
    let mut cursor = Cursor::new(0, 0, 0);
    cursor.begin(0, 1, 2, 10).unwrap();
    assert_eq!(cursor.land(1, 2, 9), Ok(LandOutcome::New));
    assert_eq!(cursor.land(1, 2, 9), Ok(LandOutcome::Duplicate));
    for idx in [10, 11, u32::MAX] {
        assert_eq!(
            cursor.land(1, 2, idx),
            Err(StreamError::ChunkOutOfRange { idx, chunks: 10 })
        );
    }
    let bitmap = &cursor.landing.as_ref().unwrap().bitmap;
    assert!(bitmap.is_landed(9) && !bitmap.is_landed(10) && !bitmap.is_landed(u32::MAX));
    assert_eq!(bitmap.landed_count(), 1);
    assert_eq!(bitmap.sparse_ranges(), [ChunkRange::new(9, 10)]);

    // A bitmap has at least one chunk and at most MAX_DELTA_CHUNKS.
    for chunks in [0, MAX_DELTA_CHUNKS + 1, u32::MAX] {
        assert_eq!(
            ResumeBitmap::new(chunks),
            Err(StreamError::BadBody {
                field: "chunks",
                value: u64::from(chunks)
            })
        );
    }
    // Word edges of the bitmap itself, landed from the top down.
    for chunks in [1u32, 63, 64, 65, 128, 129] {
        let mut bitmap = ResumeBitmap::new(chunks).unwrap();
        for idx in (0..chunks).rev() {
            assert_eq!(bitmap.mark(idx), Ok(true));
            assert_eq!(bitmap.mark(idx), Ok(false));
            let contig = if idx == 0 { chunks } else { 0 };
            assert_eq!(bitmap.landed_contig(), contig);
            let expected = if idx == 0 {
                Vec::new()
            } else {
                vec![ChunkRange::new(idx, chunks)]
            };
            assert_eq!(bitmap.sparse_ranges(), expected);
        }
        assert!(bitmap.is_complete());
        assert_eq!(bitmap.missing().count(), 0);
        assert_eq!(
            bitmap.mark(chunks),
            Err(StreamError::ChunkOutOfRange {
                idx: chunks,
                chunks
            })
        );
    }
}

#[test]
fn a_commit_moves_the_base_for_the_next_delta() {
    let mut cursor = Cursor::new(10, 1, 0);
    assert_eq!(cursor.begin(10, 12, 0xA, 2), Ok(BeginOutcome::Started));
    cursor.land(12, 0xA, 0).unwrap();
    cursor.land(12, 0xA, 1).unwrap();
    assert_eq!(cursor.commit(2), Ok(12));
    assert_eq!((cursor.applied_cp_seq, cursor.applied_epoch), (12, 2));
    assert!(cursor.landing.is_none());
    // A replay of the committed delta, or any delta on the old base, is refused.
    for (cp, nonce) in [(12, 0xA), (13, 0xB)] {
        assert_eq!(
            cursor.begin(10, cp, nonce, 2),
            Err(StreamError::BaseMismatch {
                base: 10,
                applied: 12
            })
        );
    }
    assert_eq!(cursor.land(12, 0xA, 0), Err(StreamError::NoDelta));
    assert_eq!(cursor.commit(3), Err(StreamError::NoDelta));
    assert!(matches!(
        cursor.begin(12, 12, 0xB, 2),
        Err(StreamError::BadBody {
            field: "cp_seq",
            ..
        })
    ));
    assert_eq!(cursor.begin(12, 13, 0xB, 2), Ok(BeginOutcome::Started));
    assert_eq!(
        cursor.commit(3),
        Err(StreamError::Incomplete {
            landed: 0,
            chunks: 2
        })
    );
}

#[test]
fn begin_refuses_a_cp_beyond_the_format_bound() {
    let mut cursor = Cursor::new(MAX_CP_SEQ - 1, 1, 0);
    for cp in [MAX_CP_SEQ + 1, u64::MAX] {
        assert_eq!(
            cursor.begin(MAX_CP_SEQ - 1, cp, 1, 1),
            Err(StreamError::BadBody {
                field: "cp_seq",
                value: cp
            })
        );
    }
    assert_eq!(
        cursor.begin(MAX_CP_SEQ - 1, MAX_CP_SEQ, 1, 1),
        Ok(BeginOutcome::Started)
    );
    cursor.land(MAX_CP_SEQ, 1, 0).unwrap();
    assert_eq!(cursor.commit(1), Ok(MAX_CP_SEQ));
    assert!(matches!(
        cursor.begin(MAX_CP_SEQ, MAX_CP_SEQ + 1, 2, 1),
        Err(StreamError::BadBody {
            field: "cp_seq",
            ..
        })
    ));
    // A cursor outside the bound cannot be advertised.
    let rogue = Frame::HelloAck(HelloAck {
        cursor: Cursor::new(MAX_CP_SEQ + 1, 0, 0),
    });
    assert!(matches!(
        Encoder::new().encode(&FrameMeta::default(), &rogue),
        Err(StreamError::BadBody {
            field: "applied_cp_seq",
            ..
        })
    ));
}

#[test]
fn a_refused_begin_leaves_the_partial_alone() {
    let mut cursor = Cursor::new(1, 1, 0);
    cursor.begin(1, 2, 3, 8).unwrap();
    cursor.land(2, 3, 0).unwrap();
    cursor.land(2, 3, 5).unwrap();
    let before = cursor.clone();
    assert_eq!(
        cursor.begin(1, 2, 3, 9),
        Err(StreamError::BadBody {
            field: "chunks",
            value: 9
        })
    );
    assert_eq!(cursor, before);
    // Malformed begins with a new nonce or CP are refused before they can
    // discard or replace anything.
    assert!(cursor.begin(1, 2, 4, 0).is_err());
    assert!(cursor.begin(1, 2, 4, MAX_DELTA_CHUNKS + 1).is_err());
    assert!(cursor.begin(0, 2, 4, 8).is_err());
    assert!(cursor.begin(1, 1, 4, 8).is_err());
    assert!(cursor.begin(1, MAX_CP_SEQ + 1, 4, 8).is_err());
    assert_eq!(cursor, before);
    assert_eq!(cursor.begin(1, 2, 3, 8), Ok(BeginOutcome::Resumed));
    assert_eq!(cursor, before);
}

proptest! {
    #![proptest_config(config())]

    /// The word-wise bitmap (whole-word runs, `first_unlanded`,
    /// `first_landed`) agrees with a plain bit model: the contiguous prefix,
    /// the count, the canonical sparse ranges, the missing chunks, and a
    /// rebuild from its resume form, across word edges.
    #[test]
    fn the_word_wise_bitmap_agrees_with_a_bit_model(
        chunks in prop_oneof![1u32..=130, 1u32..=5000],
        marks in prop::collection::vec(any::<u32>(), 0..300),
        runs in prop::collection::vec((any::<u32>(), 1u32..=300), 0..8),
    ) {
        let mut bitmap = ResumeBitmap::new(chunks).unwrap();
        let mut model = vec![false; chunks as usize];
        let runs = runs.into_iter().flat_map(|(start, len)| {
            let start = start % chunks;
            start..(start + len).min(chunks)
        });
        for idx in marks.into_iter().map(|m| m % chunks).chain(runs) {
            let first = !model[idx as usize];
            prop_assert_eq!(bitmap.mark(idx), Ok(first));
            model[idx as usize] = true;
        }
        let contig = model.iter().position(|b| !*b).map_or(chunks, |p| p as u32);
        prop_assert_eq!(bitmap.landed_contig(), contig);
        prop_assert_eq!(
            bitmap.landed_count() as usize,
            model.iter().filter(|b| **b).count()
        );
        prop_assert_eq!(bitmap.is_complete(), contig == chunks);
        let mut ranges = Vec::new();
        let mut i = contig as usize;
        while i < model.len() {
            if model[i] {
                let start = i;
                while i < model.len() && model[i] {
                    i += 1;
                }
                ranges.push(ChunkRange::new(start as u32, i as u32));
            } else {
                i += 1;
            }
        }
        prop_assert_eq!(bitmap.sparse_ranges(), ranges.clone());
        let missing: Vec<u32> = (contig..chunks).filter(|i| !model[*i as usize]).collect();
        prop_assert_eq!(bitmap.missing().collect::<Vec<_>>(), missing);
        let rebuilt = ResumeBitmap::from_parts(chunks, contig, &ranges).unwrap();
        prop_assert!(rebuilt == bitmap, "the rebuilt bitmap differs");
    }
}

#[test]
fn resume_bitmap_from_parts_edges() {
    let r = ChunkRange::new;
    assert!(ResumeBitmap::from_parts(10, 10, &[]).unwrap().is_complete());
    for range in [r(10, 11), r(9, 10), r(10, 10)] {
        assert_eq!(
            ResumeBitmap::from_parts(10, 10, &[range]),
            Err(StreamError::BadBody {
                field: "sparse",
                value: u64::from(range.start)
            })
        );
    }
    // No range may reach into the contiguous prefix or start right at its
    // end: either would make the prefix longer than described.
    for range in [r(0, 10), r(0, 1), r(3, 6), r(4, 6)] {
        assert_eq!(
            ResumeBitmap::from_parts(10, 4, &[range]),
            Err(StreamError::BadBody {
                field: "sparse",
                value: u64::from(range.start)
            })
        );
    }
    assert!(ResumeBitmap::from_parts(10, 4, &[r(5, 6)]).is_ok());
    assert_eq!(
        ResumeBitmap::from_parts(10, 11, &[]),
        Err(StreamError::ChunkOutOfRange {
            idx: 11,
            chunks: 10
        })
    );
    for chunks in [0, MAX_DELTA_CHUNKS + 1] {
        assert_eq!(
            ResumeBitmap::from_parts(chunks, 0, &[]),
            Err(StreamError::BadBody {
                field: "chunks",
                value: u64::from(chunks)
            })
        );
    }
    let rebuilt = ResumeBitmap::from_parts(130, 64, &[r(65, 66), r(127, 129)]).unwrap();
    assert_eq!(
        (
            rebuilt.landed_contig(),
            rebuilt.landed_count(),
            rebuilt.is_complete()
        ),
        (64, 67, false)
    );
    assert_eq!(rebuilt.sparse_ranges(), [r(65, 66), r(127, 129)]);
    let missing: Vec<u32> = rebuilt.missing().collect();
    let expected: Vec<u32> = [64].into_iter().chain(66..127).chain([129]).collect();
    assert_eq!(missing, expected);
    // A rebuilt bitmap is the bitmap that was described, word edges included.
    let mut direct = ResumeBitmap::new(130).unwrap();
    for idx in (0..64).chain([65, 127, 128]) {
        direct.mark(idx).unwrap();
    }
    assert_eq!(rebuilt, direct);
}

#[test]
fn cursor_page_adversaries() {
    let mut cursor = Cursor::new(5, 1, 9);
    cursor.begin(5, 6, 7, 2000).unwrap();
    for idx in (1..2000).step_by(2) {
        cursor.land(6, 7, idx).unwrap();
    }
    let pipe = id(0x42);
    let page = cursor.to_page(pipe).unwrap();
    assert_eq!(page.cursor.sparse_ranges as usize, CURSOR_PAGE_RANGES);
    let restored = Cursor::from_page(&page).unwrap();
    let kept = &restored.landing.as_ref().unwrap().bitmap;
    let held = &cursor.landing.as_ref().unwrap().bitmap;
    for idx in 0..2000 {
        assert!(!kept.is_landed(idx) || held.is_landed(idx));
    }
    // A receiver restart re-lands what the page dropped and completes.
    let mut resumed = restored.clone();
    assert_eq!(resumed.begin(5, 6, 7, 2000), Ok(BeginOutcome::Resumed));
    for idx in 0..2000 {
        let expected = if kept.is_landed(idx) {
            LandOutcome::Duplicate
        } else {
            LandOutcome::New
        };
        assert_eq!(resumed.land(6, 7, idx), Ok(expected));
    }
    assert_eq!(resumed.commit(2), Ok(6));

    // More ranges than a page holds, behind a valid CRC.
    for n in [CURSOR_PAGE_RANGES as u32 + 1, u32::MAX] {
        let mut over = page;
        over.cursor.sparse_ranges = n;
        over.seal();
        let expected = Err(StreamError::BadBody {
            field: "sparse_ranges",
            value: u64::from(n),
        });
        assert_eq!(over.verify(), expected);
        assert_eq!(
            Cursor::from_page(&over),
            expected.map(|()| restored.clone())
        );
        assert_eq!(CursorPage::read(over.as_bytes()), expected.map(|()| over));
    }
    // Credits beyond any window, behind a valid CRC.
    let mut rich = page;
    rich.cursor.credits = MAX_CREDITS + 1;
    rich.seal();
    assert_eq!(
        Cursor::from_page(&rich),
        Err(StreamError::BadBody {
            field: "credits",
            value: u64::from(MAX_CREDITS) + 1
        })
    );

    // A later minor reads; another major does not.
    let mut later = page;
    later.ver = STREAM_VERSION + 1;
    later.seal();
    assert_eq!(Cursor::from_page(&later), Ok(restored.clone()));
    for ver in [STREAM_VERSION + 0x0100, STREAM_VERSION - 0x0100, u16::MAX] {
        let mut other = page;
        other.ver = ver;
        other.seal();
        assert!(matches!(
            Cursor::from_page(&other),
            Err(StreamError::Format(FormatError::BadVersion { .. }))
        ));
    }

    // On disk, reserved bytes are ignored (otap-format convention 4): a
    // later minor may give them meaning. On the wire they are refused.
    let idle = Cursor::new(5, 1, 9);
    let mut reserved = idle.to_page(pipe).unwrap();
    reserved.ver = STREAM_VERSION + 1;
    reserved.rsvd0 = 0xFFFF;
    reserved.rsvd1 = 1;
    reserved.cursor.rsvd = [0xEE; 12];
    reserved.ranges[7] = ChunkRange::new(1, 2);
    reserved.seal();
    assert_eq!(Cursor::from_page(&reserved), Ok(idle));
    assert_eq!(
        refusal(FrameKind::HelloAck, reserved.cursor.as_bytes()),
        bad("rsvd", 0xEE)
    );

    // The copy path reads at any alignment and only at the exact size.
    let mut shifted = vec![0u8; CURSOR_PAGE_SIZE + 1];
    shifted[1..].copy_from_slice(page.as_bytes());
    assert_eq!(CursorPage::read(&shifted[1..]), Ok(page));
    assert!(CursorPage::read(&shifted[..CURSOR_PAGE_SIZE]).is_err());
    for len in [CURSOR_PAGE_SIZE - 1, CURSOR_PAGE_SIZE + 1] {
        let mut bytes = page.as_bytes().to_vec();
        bytes.resize(len, 0);
        assert_eq!(
            CursorPage::read(&bytes),
            Err(StreamError::Format(FormatError::BadLength {
                expected: CURSOR_PAGE_SIZE,
                found: len
            }))
        );
    }
}

#[test]
fn a_fragmented_landing_still_fits_a_hello_ack() {
    // The repro of the first QA pass: every other chunk of a
    // 2,097,138-chunk delta landed, 1,048,569 sparse ranges.
    let _guard = heavy();
    let fits = (MAX_WIRE_PAYLOAD as usize - size_of::<CursorBody>()) / size_of::<ChunkRange>();
    let chunks = 2 * (fits as u32 + 1);
    let mut cursor = Cursor::new(0, 1, 0);
    cursor.begin(0, 1, 9, chunks).unwrap();
    for idx in (1..chunks).step_by(2) {
        cursor.land(1, 9, idx).unwrap();
    }
    let all = cursor.landing.as_ref().unwrap().bitmap.sparse_ranges();
    assert_eq!(all.len(), fits + 1);
    let (body, ranges) = cursor.to_wire();
    assert_eq!(ranges.len(), MAX_WIRE_RANGES);
    assert_eq!(body.sparse_ranges as usize, MAX_WIRE_RANGES);
    assert_eq!(ranges[..], all[..MAX_WIRE_RANGES]);

    let frame = Frame::HelloAck(HelloAck {
        cursor: cursor.clone(),
    });
    let bytes = encode(&FrameMeta::default(), &frame);
    assert_eq!(
        bytes.len(),
        STREAM_HEADER_SIZE + size_of::<CursorBody>() + MAX_WIRE_RANGES * size_of::<ChunkRange>()
    );
    let Ok(Frame::HelloAck(HelloAck { cursor: sent })) = decode_body(&bytes) else {
        panic!("the HelloAck decodes")
    };
    // What the sender learns is a subset of what landed: the rest is resent
    // and lands again as duplicates.
    let told = &sent.landing.as_ref().unwrap().bitmap;
    let held = &cursor.landing.as_ref().unwrap().bitmap;
    assert_eq!(told.landed_count(), MAX_WIRE_RANGES as u32);
    assert!((0..chunks).all(|idx| !told.is_landed(idx) || held.is_landed(idx)));
    let mut resumed = cursor.clone();
    assert_eq!(resumed.begin(0, 1, 9, chunks), Ok(BeginOutcome::Resumed));
    for idx in told.missing() {
        resumed.land(1, 9, idx).unwrap();
    }
    assert_eq!(resumed.commit(2), Ok(1));
    // The page keeps the first 500 ranges.
    assert!(Cursor::from_page(&cursor.to_page(id(1)).unwrap()).is_ok());
}

#[test]
fn hello_ack_refuses_more_ranges_than_the_wire_carries() {
    let r = ChunkRange::new;
    let body = |n: usize| CursorBody {
        chunks: 10_000,
        landed_contig: 0,
        sparse_ranges: n as u32,
        ..cursor_body(0)
    };
    let ranges =
        |n: usize| -> Vec<ChunkRange> { (0..n as u32).map(|k| r(2 * k + 1, 2 * k + 2)).collect() };
    let most = ranges(MAX_WIRE_RANGES);
    assert!(Cursor::from_wire(&body(MAX_WIRE_RANGES), &most).is_ok());
    assert_eq!(
        refusal(
            FrameKind::HelloAck,
            &hello_ack_payload(&body(MAX_WIRE_RANGES), &most)
        ),
        None
    );
    let over = ranges(MAX_WIRE_RANGES + 1);
    let expected = StreamError::BadBody {
        field: "sparse_ranges",
        value: MAX_WIRE_RANGES as u64 + 1,
    };
    assert_eq!(
        Cursor::from_wire(&body(MAX_WIRE_RANGES + 1), &over),
        Err(expected)
    );
    assert_eq!(
        refusal(
            FrameKind::HelloAck,
            &hello_ack_payload(&body(MAX_WIRE_RANGES + 1), &over)
        ),
        Some(expected)
    );
    // The page's 500-range bound is the page's: a HelloAck carries more.
    assert_eq!(
        Cursor::from_wire(
            &body(CURSOR_PAGE_RANGES + 1),
            &ranges(CURSOR_PAGE_RANGES + 1)
        )
        .map(|c| c.landing.map(|l| l.bitmap.landed_count())),
        Ok(Some(CURSOR_PAGE_RANGES as u32 + 1))
    );
}

// ---------------------------------------------------------------------------
// 7. Credits
// ---------------------------------------------------------------------------

#[test]
fn credit_arithmetic_saturates_and_windows_cap_at_max_credits() {
    let max = u32::MAX as usize;
    assert_eq!(credits_for(usize::MAX), u32::MAX);
    assert_eq!(credits_for(max * BLOCK_SIZE), u32::MAX);
    assert_eq!(credits_for(max * BLOCK_SIZE + 1), u32::MAX);
    assert_eq!(credits_for((max - 1) * BLOCK_SIZE + 1), u32::MAX);
    assert_eq!(credits_for((max - 1) * BLOCK_SIZE), u32::MAX - 1);
    assert_eq!(credits_for(MAX_WIRE_PAYLOAD as usize), 2048);
    assert_eq!(credits_for(MIN_STRIPE_BYTES), 2);
    assert_eq!(
        u64::from(MAX_CREDITS) * u64::from(CREDIT_BYTES),
        MAX_STAGING_BYTES
    );

    let mut sender = SenderWindow::new(u32::MAX);
    assert_eq!(sender.available(), MAX_CREDITS);
    sender.grant(5);
    assert_eq!(sender.available(), MAX_CREDITS);
    sender.grant(u32::MAX);
    assert_eq!(sender.available(), MAX_CREDITS);
    // A saturated credit count is never one a window can hold.
    assert!(!sender.can_send(usize::MAX));
    assert_eq!(
        sender.reserve(usize::MAX),
        Err(StreamError::Window {
            needed: u32::MAX,
            available: MAX_CREDITS
        })
    );
    assert_eq!(sender.reserve(MAX_WIRE_PAYLOAD as usize), Ok(2048));
    assert_eq!(sender.available(), MAX_CREDITS - 2048);
    assert_eq!(
        sender.reserve(MAX_STAGING_BYTES as usize),
        Err(StreamError::Window {
            needed: MAX_CREDITS,
            available: MAX_CREDITS - 2048
        })
    );
    sender.reset();
    assert_eq!(sender.available(), 0);
    assert!(sender.can_send(0));
    assert_eq!(sender.reserve(0), Ok(0));
    assert_eq!(
        sender.reserve(1),
        Err(StreamError::Window {
            needed: 1,
            available: 0
        })
    );
    assert_eq!(sender.available(), 0);
    // A cursor holds no more than a window either.
    assert_eq!(Cursor::new(0, 0, u32::MAX).credits, MAX_CREDITS);
    assert_eq!(Cursor::new(0, 0, MAX_CREDITS - 1).credits, MAX_CREDITS - 1);
}

#[test]
fn a_receiver_window_of_exactly_two_gib() {
    let capacity = (MAX_STAGING_BYTES / BLOCK_SIZE as u64) as u32;
    assert_eq!(capacity, MAX_CREDITS);
    let mut receiver = ReceiverWindow::new(MAX_STAGING_BYTES).unwrap();
    assert_eq!(receiver.capacity(), capacity);
    let initial = credits_for(INITIAL_GRANT_BYTES as usize);
    assert_eq!(receiver.initial_grant(), initial);
    assert_eq!(receiver.grant_up_to(u32::MAX), capacity - initial);
    assert_eq!((receiver.free(), receiver.outstanding()), (0, capacity));
    assert_eq!(receiver.grant_up_to(1), 0);
    assert_eq!(receiver.initial_grant(), 0);
    receiver.landed(MAX_WIRE_PAYLOAD as usize).unwrap();
    assert_eq!(
        (receiver.outstanding(), receiver.staged()),
        (capacity - 2048, 2048)
    );
    receiver.drained(MAX_WIRE_PAYLOAD as usize).unwrap();
    assert_eq!(receiver.free(), 2048);
    for bytes in [
        MAX_STAGING_BYTES + BLOCK_SIZE as u64,
        u64::MAX,
        0,
        1,
        BLOCK_SIZE as u64 + 1,
    ] {
        assert!(ReceiverWindow::new(bytes).is_err(), "{bytes}");
    }
    assert!(ReceiverWindow::new(MAX_STAGING_BYTES - BLOCK_SIZE as u64).is_ok());
    assert_eq!(
        ReceiverWindow::new(BLOCK_SIZE as u64).unwrap().capacity(),
        1
    );
}

#[test]
fn grants_after_a_reset_and_empty_landings() {
    let mut receiver = ReceiverWindow::new(1 << 20).unwrap();
    assert_eq!(receiver.initial_grant(), 256);
    let before = receiver;
    receiver.landed(0).unwrap();
    assert_eq!(receiver, before, "a landing of 0 bytes changes nothing");
    receiver.landed(10 * BLOCK_SIZE).unwrap();
    assert_eq!((receiver.outstanding(), receiver.staged()), (246, 10));
    receiver.reset();
    assert_eq!(
        (receiver.outstanding(), receiver.staged(), receiver.free()),
        (0, 0, 256)
    );
    // A drain reported after the reset is refused.
    assert_eq!(
        receiver.drained(BLOCK_SIZE),
        Err(StreamError::Window {
            needed: 1,
            available: 0
        })
    );
    assert_eq!(receiver.initial_grant(), 256);
    let full = receiver;
    assert_eq!(
        receiver.landed((1 << 20) + 1),
        Err(StreamError::Window {
            needed: 257,
            available: 256
        })
    );
    assert_eq!(receiver, full, "a refused landing changes nothing");
    receiver.landed(1 << 20).unwrap();
    assert_eq!(receiver.grant_up_to(u32::MAX), 0);
    receiver.drained(1 << 20).unwrap();
    assert_eq!(receiver.grant_up_to(u32::MAX), 256);
}

#[derive(Debug, Clone)]
enum WindowOp {
    Grant(u32),
    Initial,
    Send(usize),
    Spurious(usize),
    Drain(usize),
    Reset,
}

fn window_op() -> impl Strategy<Value = WindowOp> {
    prop_oneof![
        any::<u32>().prop_map(WindowOp::Grant),
        (0u32..512).prop_map(WindowOp::Grant),
        Just(WindowOp::Initial),
        (0usize..(9 << 20)).prop_map(WindowOp::Send),
        (0usize..(1 << 16)).prop_map(WindowOp::Send),
        (0usize..(9 << 20)).prop_map(WindowOp::Spurious),
        (0usize..(9 << 20)).prop_map(WindowOp::Drain),
        Just(WindowOp::Reset),
    ]
}

proptest! {
    #![proptest_config(config())]

    /// `outstanding + staged <= capacity` through any mix of grants,
    /// sends, spurious landings, drains and resets; a refused operation
    /// changes nothing; the sender's view tracks the receiver's.
    #[test]
    fn the_windows_hold_their_invariants_through_resets_and_regrants(
        staging_blocks in prop_oneof![1u64..=64, 1u64..=MAX_STAGING_BYTES / BLOCK_SIZE as u64],
        ops in prop::collection::vec(window_op(), 1..200),
    ) {
        let mut receiver = ReceiverWindow::new(staging_blocks * BLOCK_SIZE as u64).unwrap();
        let mut sender = SenderWindow::new(0);
        let capacity = receiver.capacity();
        for op in ops {
            let before = receiver;
            match op {
                WindowOp::Grant(want) => {
                    let granted = receiver.grant_up_to(want);
                    prop_assert_eq!(granted, want.min(before.free()));
                    sender.grant(granted);
                }
                WindowOp::Initial => {
                    let granted = receiver.initial_grant();
                    prop_assert_eq!(
                        granted,
                        credits_for(INITIAL_GRANT_BYTES as usize).min(before.free())
                    );
                    sender.grant(granted);
                }
                WindowOp::Send(bytes) => {
                    let available = sender.available();
                    if sender.can_send(bytes) {
                        prop_assert_eq!(sender.reserve(bytes), Ok(credits_for(bytes)));
                        prop_assert_eq!(receiver.landed(bytes), Ok(()));
                    } else {
                        prop_assert_eq!(
                            sender.reserve(bytes),
                            Err(StreamError::Window { needed: credits_for(bytes), available })
                        );
                        prop_assert_eq!(sender.available(), available);
                    }
                }
                WindowOp::Spurious(bytes) => match receiver.landed(bytes) {
                    Ok(()) => {
                        prop_assert!(credits_for(bytes) <= before.outstanding());
                        // The sender never reserved these: resynchronise it.
                        sender = SenderWindow::new(receiver.outstanding());
                    }
                    Err(e) => {
                        prop_assert_eq!(
                            e,
                            StreamError::Window {
                                needed: credits_for(bytes),
                                available: before.outstanding()
                            }
                        );
                        prop_assert_eq!(receiver, before);
                    }
                },
                WindowOp::Drain(bytes) => match receiver.drained(bytes) {
                    Ok(()) => prop_assert!(credits_for(bytes) <= before.staged()),
                    Err(e) => {
                        prop_assert_eq!(
                            e,
                            StreamError::Window {
                                needed: credits_for(bytes),
                                available: before.staged()
                            }
                        );
                        prop_assert_eq!(receiver, before);
                    }
                },
                WindowOp::Reset => {
                    receiver.reset();
                    sender.reset();
                }
            }
            prop_assert_eq!(receiver.capacity(), capacity);
            prop_assert!(
                u64::from(receiver.outstanding()) + u64::from(receiver.staged())
                    <= u64::from(capacity)
            );
            prop_assert_eq!(
                receiver.free(),
                capacity - receiver.outstanding() - receiver.staged()
            );
            prop_assert_eq!(sender.available(), receiver.outstanding());
            prop_assert!(sender.available() <= MAX_CREDITS);
        }
    }
}

// ---------------------------------------------------------------------------
// 8. Determinism, injectivity and the canonical round trip
// ---------------------------------------------------------------------------

proptest! {
    #![proptest_config(config())]

    /// The same frame encodes to the same bytes, whichever entry point is
    /// used and whatever the buffer already holds.
    #[test]
    fn encoding_is_deterministic(meta in meta(), frame in frame()) {
        let meta = meta_for(&frame, meta);
        let a = encode(&meta, &frame);
        let b = encode(&meta, &frame);
        prop_assert_eq!(&a, &b);
        let mut out = BytesMut::from(&b"prefix"[..]);
        let n = Encoder::new().encode_into(&meta, &frame, &mut out).unwrap();
        prop_assert_eq!(n, a.len());
        prop_assert_eq!(&out[..6], b"prefix");
        prop_assert_eq!(&out[6..], &a[..]);
        let hdr = Encoder::new().header(&meta, &frame, &a[STREAM_HEADER_SIZE..]).unwrap();
        prop_assert_eq!(hdr.as_bytes(), &a[..STREAM_HEADER_SIZE]);
        prop_assert_eq!(&payload_of(&frame)[..], &a[STREAM_HEADER_SIZE..]);
    }

    /// Two frames that differ in one field never encode to the same bytes.
    #[test]
    fn a_change_to_one_field_changes_the_encoding(
        meta in meta(),
        frame in frame(),
        field in any::<usize>(),
        value in any::<u64>(),
    ) {
        let meta = meta_for(&frame, meta);
        let (meta2, frame2) = mutate(&meta, &frame, field, value);
        if meta2 == meta && frame2 == frame {
            return Ok(());
        }
        let (Ok(a), Ok(b)) = (
            Encoder::new().encode(&meta, &frame),
            Encoder::new().encode(&meta2, &frame2),
        ) else {
            return Ok(());
        };
        prop_assert_ne!(a, b);
    }

    /// Two different frames never share an encoding.
    #[test]
    fn distinct_frames_never_share_an_encoding(a in (meta(), frame()), b in (meta(), frame())) {
        let a = (meta_for(&a.1, a.0), a.1);
        let b = (meta_for(&b.1, b.0), b.1);
        prop_assume!(a != b);
        prop_assert_ne!(encode(&a.0, &a.1), encode(&b.0, &b.1));
    }

    /// A body damaged behind valid CRCs is refused or accepted, never a
    /// panic; an accepted one is a valid frame that re-encodes to the very
    /// bytes it came from, and refusals never contradict themselves.
    #[test]
    fn damaged_bodies_behind_valid_crcs_are_refused_or_canonical(
        meta in meta(),
        frame in small_frame(),
        flips in prop::collection::vec((any::<usize>(), 1u8..=255), 1..4),
    ) {
        let meta = FrameMeta { chunk_idx: 0, ..meta };
        let mut payload = payload_of(&frame);
        for (at, mask) in flips {
            let len = payload.len();
            payload[at % len] ^= mask;
        }
        // Keep each case cheap: a HelloAck naming a delta of more than 2^16
        // chunks is skipped (the plain tests cover the 2^24 bound).
        if frame.kind() == FrameKind::HelloAck {
            let chunks = u32::from_le_bytes([payload[40], payload[41], payload[42], payload[43]]);
            if chunks > 1 << 16 {
                return Ok(());
            }
        }
        let wire = wire(frame.kind(), Codec::Raw, &meta, &payload, payload.len());
        let mut decoder = Decoder::new();
        decoder.feed(&wire);
        let raw = decoder.next_frame().unwrap().unwrap();
        let direct = Frame::decode(&raw.hdr, raw.payload.clone());
        match raw.decode() {
            Ok((m, decoded)) => {
                prop_assert!(m == meta);
                prop_assert!(direct == Ok(decoded.clone()));
                prop_assert!(decoded.validate().is_ok(), "decoded but invalid");
                prop_assert_eq!(&payload_of(&decoded)[..], &payload[..]);
                let again = encode(&meta, &decoded);
                prop_assert_eq!(&again[..], &wire[..]);
            }
            Err(e) => {
                prop_assert!(!contradicts_itself(&e), "{:?}", e);
                prop_assert_eq!(direct, Err(e));
            }
        }
    }

    /// `Frame::decode` never panics, whatever the header it is handed says,
    /// and never contradicts itself.
    #[test]
    fn typed_decoding_never_panics_on_an_unverified_header(
        header in any::<[u8; 64]>(),
        kind in 0u8..=13,
        flags in any::<u8>(),
        payload in prop::collection::vec(any::<u8>(), 0..300),
        exact_len in any::<bool>(),
    ) {
        let mut hdr = StreamHeader::read_from_bytes(&header).unwrap();
        hdr.kind = kind;
        hdr.flags = flags;
        if exact_len {
            hdr.wire_len = payload.len() as u32;
        }
        let payload = Bytes::from(payload);
        match Frame::decode(&hdr, payload.clone()) {
            Ok(frame) => prop_assert_eq!(frame.kind().as_u8(), kind),
            Err(e) => prop_assert!(!contradicts_itself(&e), "{:?}", e),
        }
        let other = FrameKind::from_u8(1 + kind % 12).unwrap();
        if other.as_u8() != kind {
            prop_assert_eq!(
                Frame::decode_as(&hdr, payload, other),
                Err(StreamError::WrongKind { expected: other, found: kind })
            );
        }
    }
}

// ---------------------------------------------------------------------------
// 9. The first QA pass's defects, fixed, and what is left
// ---------------------------------------------------------------------------

#[test]
fn decoding_refuses_bodies_the_encoder_never_produces() {
    let steady = delta_begin_body();
    let cases: [(&str, FrameKind, Vec<u8>, StreamError); 13] = [
        (
            "Hello.rsvd",
            FrameKind::Hello,
            HelloBody {
                rsvd: [0, 0, 0, 0, 0, 0, 0, 1],
                ..hello_body()
            }
            .as_bytes()
            .to_vec(),
            StreamError::BadBody {
                field: "rsvd",
                value: 1,
            },
        ),
        (
            "HelloAck body rsvd",
            FrameKind::HelloAck,
            CursorBody {
                rsvd: [1; 12],
                ..cursor_body(0)
            }
            .as_bytes()
            .to_vec(),
            StreamError::BadBody {
                field: "rsvd",
                value: 1,
            },
        ),
        (
            "HelloAck adjacent ranges 5..7 and 7..9",
            FrameKind::HelloAck,
            hello_ack_payload(
                &cursor_body(2),
                &[ChunkRange::new(5, 7), ChunkRange::new(7, 9)],
            ),
            StreamError::BadBody {
                field: "sparse",
                value: 7,
            },
        ),
        (
            "DeltaBegin.rsvd0",
            FrameKind::DeltaBegin,
            delta_begin_payload(
                &DeltaBeginBody {
                    rsvd0: [0, 1, 0],
                    ..steady
                },
                &[],
                &[],
            ),
            StreamError::BadBody {
                field: "rsvd0",
                value: 1,
            },
        ),
        (
            "DeltaBegin.rsvd1",
            FrameKind::DeltaBegin,
            delta_begin_payload(&DeltaBeginBody { rsvd1: 1, ..steady }, &[], &[]),
            StreamError::BadBody {
                field: "rsvd1",
                value: 1,
            },
        ),
        (
            "steady DeltaBegin with a from_snap",
            FrameKind::DeltaBegin,
            delta_begin_payload(
                &DeltaBeginBody {
                    from_snap: id(3),
                    ..steady
                },
                &[],
                &[],
            ),
            StreamError::BadBody {
                field: "mode",
                value: 1,
            },
        ),
        (
            "steady DeltaBegin with a to_snap",
            FrameKind::DeltaBegin,
            delta_begin_payload(
                &DeltaBeginBody {
                    to_snap: id(4),
                    ..steady
                },
                &[],
                &[],
            ),
            StreamError::BadBody {
                field: "mode",
                value: 1,
            },
        ),
        (
            "DeltaBegin SnapTag.rsvd",
            FrameKind::DeltaBegin,
            delta_begin_payload(
                &DeltaBeginBody { tags: 1, ..steady },
                &[SnapTag {
                    rsvd: 1,
                    ..snap_tag(5)
                }],
                &[],
            ),
            StreamError::BadBody {
                field: "tag.rsvd",
                value: 1,
            },
        ),
        (
            "DeltaBegin AckedBytes.rsvd",
            FrameKind::DeltaBegin,
            delta_begin_payload(
                &DeltaBeginBody { acked: 1, ..steady },
                &[],
                &[AckedBytes {
                    rsvd: 1,
                    ..acked_bytes(1)
                }],
            ),
            StreamError::BadBody {
                field: "acked.rsvd",
                value: 1,
            },
        ),
        (
            "VolEvent.rsvd",
            FrameKind::VolEvent,
            vol_event_payload(
                &VolEventBody {
                    rsvd: [1; 24],
                    ..vol_event_body(VolEventKind::Delete, 0)
                },
                b"",
            ),
            StreamError::BadBody {
                field: "rsvd",
                value: 1,
            },
        ),
        (
            "DeltaEnd.rsvd",
            FrameKind::DeltaEnd,
            DeltaEndBody {
                rsvd: 1,
                ..delta_end_body()
            }
            .as_bytes()
            .to_vec(),
            StreamError::BadBody {
                field: "rsvd",
                value: 1,
            },
        ),
        (
            "Landed.rsvd",
            FrameKind::Landed,
            LandedBody {
                rsvd: 1,
                ..landed_body()
            }
            .as_bytes()
            .to_vec(),
            StreamError::BadBody {
                field: "rsvd",
                value: 1,
            },
        ),
        (
            "Abort.rsvd",
            FrameKind::Abort,
            AbortBody {
                rsvd: [0, 0, 1],
                ..abort_body(1)
            }
            .as_bytes()
            .to_vec(),
            StreamError::BadBody {
                field: "rsvd",
                value: 1,
            },
        ),
    ];
    for (what, kind, payload, expected) in cases {
        assert_eq!(refusal(kind, &payload), Some(expected), "{what}");
    }
    // The encoder writes no reserved word but zero.
    let reserved_records = Frame::DeltaBegin(DeltaBegin {
        tags: vec![SnapTag {
            rsvd: 1,
            ..snap_tag(5)
        }],
        acked: vec![AckedBytes {
            rsvd: 1,
            ..acked_bytes(1)
        }],
        ..sample_delta_begin()
    });
    assert_eq!(
        Encoder::new().encode(&sample_meta(), &reserved_records),
        Err(StreamError::BadBody {
            field: "tag.rsvd",
            value: 1
        })
    );
}

#[test]
fn a_raw_chunk_carries_its_image_length() {
    let image = sample_image(MIN_STRIPE_BYTES);
    for raw_len in [0, 1, (MIN_STRIPE_BYTES + BLOCK_SIZE) as u32, u32::MAX] {
        let chunk = Chunk {
            image: image.clone(),
            codec: Codec::Raw,
            raw_len,
        };
        let expected = StreamError::BadBody {
            field: "raw_len",
            value: u64::from(raw_len),
        };
        assert_eq!(chunk.validate(), Err(expected));
        assert_eq!(
            Encoder::new().encode(&sample_meta(), &Frame::Chunk(chunk)),
            Err(expected)
        );
    }
    // A header that disagrees with its raw payload never reaches the typed
    // decode on the wire; handed to it directly, it is refused there too.
    let mut hdr = StreamHeader::new(FrameKind::Chunk, Codec::Raw);
    hdr.set_payload(&image).unwrap();
    hdr.raw_len += BLOCK_SIZE as u32;
    assert_eq!(
        Frame::decode(&hdr, image.clone()),
        Err(StreamError::BadBody {
            field: "raw_len",
            value: u64::from(hdr.raw_len)
        })
    );
    let exact = Chunk::raw(image).unwrap();
    assert_eq!(exact.validate(), Ok(()));
    assert_eq!(exact.raw_len as usize, MIN_STRIPE_BYTES);
}

#[test]
fn a_compressed_chunk_must_decode_to_a_stripe() {
    for (payload, raw_len, field, value) in [
        (&b"abc"[..], 0u32, "raw_len", 0u64),
        (b"abc", 1, "raw_len", 1),
        (
            b"abc",
            MIN_STRIPE_BYTES as u32 - 1,
            "raw_len",
            MIN_STRIPE_BYTES as u64 - 1,
        ),
        (
            b"abc",
            MIN_STRIPE_BYTES as u32 + 1,
            "raw_len",
            MIN_STRIPE_BYTES as u64 + 1,
        ),
        (b"abc", BLOCK_SIZE as u32, "raw_len", BLOCK_SIZE as u64),
        (b"", MIN_STRIPE_BYTES as u32, "wire_len", 0),
    ] {
        let expected = StreamError::BadBody { field, value };
        for codec in [Codec::Lz4, Codec::Zstd] {
            let on_wire = wire(
                FrameKind::Chunk,
                codec,
                &sample_meta(),
                payload,
                raw_len as usize,
            );
            assert_eq!(decode_body(&on_wire), Err(expected), "{codec:?} {raw_len}");
            let chunk = Chunk {
                image: Bytes::copy_from_slice(payload),
                codec,
                raw_len,
            };
            assert_eq!(chunk.validate(), Err(expected));
            assert_eq!(
                Encoder::new().encode(&sample_meta(), &Frame::Chunk(chunk)),
                Err(expected)
            );
        }
    }
    // Any stripe size will do, whatever the wire length.
    for raw_len in [MIN_STRIPE_BYTES as u32, MAX_WIRE_PAYLOAD] {
        let frame = Frame::Chunk(Chunk {
            image: Bytes::from_static(b"x"),
            codec: Codec::Zstd,
            raw_len,
        });
        assert!(decode_body(&encode(&sample_meta(), &frame)) == Ok(frame));
    }
}

#[test]
fn chunk_stripe_hdr_refuses_a_short_raw_image() {
    let chunk = Chunk {
        image: Bytes::from_static(&[0; 10]),
        codec: Codec::Raw,
        raw_len: 10,
    };
    assert_eq!(
        chunk.stripe_hdr(),
        Err(StreamError::BodyTooShort {
            needed: 64,
            have: 10
        })
    );
    assert!(chunk.summary().is_err());
    assert_eq!(
        chunk.validate(),
        Err(StreamError::BadBody {
            field: "raw_len",
            value: 10
        })
    );
}

#[test]
fn refusals_name_the_field_at_fault_and_agree_with_themselves() {
    let nil_pipe = HelloBody {
        pipe_id: Uuid::NIL,
        ..hello_body()
    };
    assert_eq!(
        refusal(FrameKind::Hello, nil_pipe.as_bytes()),
        bad("pipe_id", 0)
    );
    // `Frame::decode` handed more or fewer bytes than the header's wire_len.
    let ping_bytes = encode(&FrameMeta::default(), &ping());
    let hdr = StreamHeader::read(&ping_bytes[..STREAM_HEADER_SIZE]).unwrap();
    let longer = Bytes::from([&ping_bytes[STREAM_HEADER_SIZE..], &[0; 8]].concat());
    assert_eq!(
        Frame::decode(&hdr, longer),
        Err(StreamError::BodyTooLong {
            expected: 16,
            found: 24
        })
    );
    assert_eq!(
        Frame::decode(
            &hdr,
            ping_bytes.slice(STREAM_HEADER_SIZE..STREAM_HEADER_SIZE + 8)
        ),
        Err(StreamError::BodyTooShort {
            needed: 16,
            have: 8
        })
    );
    // `Encoder::header` handed a payload of another length.
    let payload = payload_of(&ping());
    assert_eq!(
        Encoder::new().header(&FrameMeta::default(), &ping(), &payload[..15]),
        Err(StreamError::BodyTooShort {
            needed: 16,
            have: 15
        })
    );
    // Holes: a record cut short is too short (see also the truncation test).
    assert_eq!(
        refusal(FrameKind::Holes, &hole(1, 2, 3).as_bytes()[..9]),
        Some(StreamError::BodyTooShort {
            needed: HOLE_RECORD_SIZE,
            have: 9
        })
    );
}

/// Each refusal of a field the event does not use names that field, with
/// its value, as the other refusals do.
#[test]
fn vol_event_refusals_name_the_unused_field() {
    let resize = vol_event_body(VolEventKind::Resize, 0);
    let delete = vol_event_body(VolEventKind::Delete, 0);
    let snap = vol_event_body(VolEventKind::SnapTag, 0);
    let cases: [(&str, VolEventBody, &[u8], &str, u64); 8] = [
        (
            "a stripe geometry on a Resize",
            VolEventBody {
                stripe_bytes: 1 << 20,
                ..resize
            },
            b"",
            "stripe_bytes",
            1 << 20,
        ),
        (
            "a retention on a Resize",
            VolEventBody {
                retention_s: 7,
                ..resize
            },
            b"",
            "retention_s",
            7,
        ),
        ("a name on a Resize", resize, b"n", "name", 1),
        (
            "a size on a Delete",
            VolEventBody {
                size_blocks: 8,
                ..delete
            },
            b"",
            "size_blocks",
            8,
        ),
        (
            "a stripe geometry on a Delete",
            VolEventBody {
                stripe_bytes: 1 << 20,
                ..delete
            },
            b"",
            "stripe_bytes",
            1 << 20,
        ),
        (
            "a retention on a Delete",
            VolEventBody {
                retention_s: 7,
                ..delete
            },
            b"",
            "retention_s",
            7,
        ),
        ("a name on a Delete", delete, b"nn", "name", 2),
        (
            "a stripe geometry on a SnapTag",
            VolEventBody {
                stripe_bytes: 1 << 20,
                ..snap
            },
            b"",
            "stripe_bytes",
            1 << 20,
        ),
    ];
    for (what, body, name, field, value) in cases {
        let body = VolEventBody {
            name_len: name.len() as u16,
            ..body
        };
        assert_eq!(
            refusal(FrameKind::VolEvent, &vol_event_payload(&body, name)),
            Some(StreamError::BadBody { field, value }),
            "{what}"
        );
    }
}

/// A HelloAck whose cursor holds more than `MAX_CREDITS` is refused by
/// `Frame::validate`, and so by the encoder, as a `Hello` or a `Landed` over
/// the bound is and as the decoder refuses such a body; the page refuses it
/// too, as `from_page` would. The field is public, so `Cursor::new`'s cap
/// does not cover it, and cutting it down on the wire would let two cursors
/// share one encoding.
#[test]
fn a_hello_ack_cursor_over_the_credit_bound_is_refused() {
    let mut over = Cursor::new(5, 1, 0);
    over.credits = MAX_CREDITS + 1;
    let mut further = over.clone();
    further.credits = u32::MAX;
    for cursor in [over, further] {
        let refused = StreamError::BadBody {
            field: "credits",
            value: u64::from(cursor.credits),
        };
        let frame = Frame::HelloAck(HelloAck {
            cursor: cursor.clone(),
        });
        assert_eq!(frame.validate(), Err(refused));
        assert_eq!(
            Encoder::new().encode(&FrameMeta::default(), &frame),
            Err(refused)
        );
        assert_eq!(cursor.to_page(id(1)), Err(refused));
    }
    // At the bound the cursor travels and persists unchanged.
    let mut at = Cursor::new(5, 1, 0);
    at.credits = MAX_CREDITS;
    let ack = Frame::HelloAck(HelloAck { cursor: at.clone() });
    let bytes = Encoder::new().encode(&FrameMeta::default(), &ack).unwrap();
    assert_eq!(decode_body(&bytes), Ok(ack));
    assert_eq!(Cursor::from_page(&at.to_page(id(1)).unwrap()), Ok(at));
}

/// `Encoder::encode` allocates nothing for a context it refuses: the CP
/// bound and `base <= cp_seq` are checked, with the errors the sealed
/// header would be refused with, before room for the frame is reserved.
/// Refusing an 8 MiB chunk under a bad context must not allocate 8 MiB.
#[test]
fn encode_allocates_nothing_for_a_refused_context() {
    let held = heavy();
    let chunk = Frame::Chunk(Chunk::raw(sample_image(MAX_WIRE_PAYLOAD as usize)).unwrap());
    let cases = [
        (
            FrameMeta {
                cp_seq: MAX_CP_SEQ + 1,
                ..FrameMeta::default()
            },
            StreamError::Format(FormatError::OutOfRange {
                field: "cp_seq",
                value: MAX_CP_SEQ + 1,
                max: MAX_CP_SEQ,
            }),
        ),
        (
            FrameMeta {
                cp_seq: 5,
                base: 6,
                ..FrameMeta::default()
            },
            StreamError::Format(FormatError::BadField {
                field: "base",
                value: 6,
            }),
        ),
    ];
    for (meta, expected) in cases {
        assert_eq!(meta.check(FrameKind::Chunk), Err(expected));
        let (result, largest) = probe(&held, || Encoder::new().encode(&meta, &chunk));
        assert_eq!(result, Err(expected));
        assert!(
            largest < 1 << 20,
            "refusing the frame allocated {largest} bytes first"
        );
        // `encode_into` leaves the buffer as it found it.
        let mut out = BytesMut::from(&b"kept"[..]);
        assert_eq!(
            Encoder::new().encode_into(&meta, &chunk, &mut out),
            Err(expected)
        );
        assert_eq!(&out[..], b"kept");
    }
    // The same context passes once `base` fits under `cp_seq`.
    let ok = FrameMeta {
        cp_seq: 6,
        base: 5,
        ..FrameMeta::default()
    };
    assert_eq!(ok.check(FrameKind::Chunk), Ok(()));
    assert!(Encoder::new().encode(&ok, &chunk).is_ok());
}
