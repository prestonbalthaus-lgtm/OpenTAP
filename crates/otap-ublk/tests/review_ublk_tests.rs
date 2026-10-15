//! Step 4 QA (Opus-QA-Adversarial): the ublk front door attacked on its
//! own and with the engine behind it. Offline toggles with requests
//! decoded but not yet handed over, degenerate watermarks, every status's
//! errno, descriptors at the edges of the sector arithmetic, request ids at
//! the edges of their fields, and failing data copies through the engine
//! (no arena slot may leak).
//!
//! The tests that reproduced a defect are regression tests now (their doc
//! comments name the defect in the Step 4 log and say what was wrong and
//! what the engine does now). The others pin properties that hold.

use otap_cp::{
    Engine, EngineConfig, FoldPolicy, FrontDoor, Geometry, MediaProfile, ReqId, RequestKind,
    Status, VolumeSpec,
};
use otap_format::{Uuid, BLOCK_SIZE, INTENT_SLOT_SIZE, SUPERBLOCK_SLOT_SIZE};
use otap_sim::{SimBackstore, SimClock};
use otap_sys::ublk::{
    UblksrvIoDesc, UBLK_IO_OP_DISCARD, UBLK_IO_OP_FLUSH, UBLK_IO_OP_READ, UBLK_IO_OP_WRITE,
    UBLK_IO_OP_WRITE_ZEROES,
};
use otap_sys::{HugeArena, HugePageSize};
use otap_ublk::{
    decode_request, errno_for, request_id, split_id, MockTransport, QueueEvent, UblkFrontDoor,
    Watermarks, EINVAL, EIO, ENODEV, ENOSPC, EOPNOTSUPP,
};
use proptest::prelude::*;

const VOL: u32 = 2;
const SIZE: u64 = 4096;
const SPB: u64 = (BLOCK_SIZE / 512) as u64;

fn desc(op: u8, start_sector: u64, nr_sectors: u32) -> UblksrvIoDesc {
    UblksrvIoDesc {
        op_flags: u32::from(op),
        nr_sectors,
        start_sector,
        addr: 0,
    }
}

fn door(depth: u16, marks: Watermarks) -> UblkFrontDoor<MockTransport> {
    UblkFrontDoor::new(MockTransport::new(depth), 0, VOL, SIZE, depth, marks)
}

fn block(fill: u8, blocks: u32) -> Vec<u8> {
    vec![fill; blocks as usize * BLOCK_SIZE]
}

// ---------------------------------------------------------------------------
// Defects
// ---------------------------------------------------------------------------

/// Found by the Step 4 review (defect 17, fixed): requests decoded before the device went offline still reach the
/// engine.
///
/// `set_offline` documents "while offline every request is answered
/// `ENODEV` without reaching the engine", but `pump` checks the flag only
/// when a request arrives, and `fetch` hands out whatever is already
/// `Ready` without looking at it. A request pumped just before the toggle
/// (several arrive per poll, and the watermark gate keeps them `Ready`
/// across ticks) goes to the engine after the device is offline: a write
/// is placed and acknowledged during what the operator believes is a
/// fence. The right behaviour: going offline answers every `Ready` request
/// `ENODEV` (or `fetch` does, before handing one out); only requests
/// already in the engine finish normally.
#[test]
fn requests_decoded_before_going_offline_are_refused() {
    let mut fd = door(4, Watermarks::for_depth(4));
    fd.transport().submit_write(0, 0, block(1, 1));
    fd.transport().submit_read(1, 0, 1);
    fd.pump().unwrap();
    assert_eq!(fd.ready(), 2);
    fd.set_offline(true);
    // Before the fix: the write decoded before the toggle is handed to the engine.
    let handed = fd.fetch();
    assert!(
        handed.is_none(),
        "an offline front door handed {handed:?} to the engine"
    );
    let mut commits = fd.transport().take_commits();
    commits.sort_unstable();
    assert_eq!(commits, vec![(0, -ENODEV), (1, -ENODEV)]);
}

/// Found by the Step 4 review (defect 18, fixed): a zero high watermark silently starves every request.
///
/// `UblkFrontDoor::new` takes any `Watermarks`; with `high == 0` the gate
/// test `in_engine >= high` holds at `in_engine == 0`, so `fetch` never
/// hands anything to the engine and never answers it either: every tag
/// fills, the block layer's queue stalls, and nothing reports why.
/// `Watermarks::for_depth(0)` already maps a zero high mark to 1, so the
/// hazard is known; the constructor should refuse a high mark of 0 (or
/// clamp it the same way, documented), as builders refuse what parsers
/// refuse.
#[test]
fn a_zero_high_watermark_is_taken_as_one() {
    let mut fd = door(4, Watermarks { high: 0, low: 0 });
    fd.transport().submit_read(0, 0, 1);
    let mut served = false;
    for _ in 0..16 {
        if let Some(req) = fd.fetch() {
            fd.copy_out(&req, &block(0, 1)).unwrap();
            fd.complete(req, Status::Ok, 0);
            served = true;
            break;
        }
    }
    // Before the fix: the request is never handed out and never answered.
    assert!(
        served || !fd.transport().take_commits().is_empty(),
        "a front door with high watermark 0 never serves nor answers (gate closed {} times)",
        fd.stats().gate_closed
    );
}

// ---------------------------------------------------------------------------
// The state machine: properties that hold
// ---------------------------------------------------------------------------

/// Every engine status maps to the result the block layer expects: bytes
/// for a successful read or write, 0 for a successful flush, discard or
/// write of zeros, a negative errno otherwise.
#[test]
fn every_status_maps_to_its_errno_for_every_request_kind() {
    let kinds = [
        RequestKind::Read {
            vol: VOL,
            lba: 0,
            blocks: 3,
        },
        RequestKind::Write {
            vol: VOL,
            lba: 0,
            blocks: 2048,
        },
        RequestKind::Flush { vol: VOL },
        RequestKind::Discard {
            vol: VOL,
            lba: 0,
            blocks: 1 << 18,
        },
        RequestKind::WriteZeroes {
            vol: VOL,
            lba: 0,
            blocks: 7,
        },
    ];
    for kind in &kinds {
        let ok = match kind {
            RequestKind::Read { blocks, .. } | RequestKind::Write { blocks, .. } => {
                *blocks as i32 * BLOCK_SIZE as i32
            }
            _ => 0,
        };
        assert_eq!(errno_for(Status::Ok, kind), ok, "{kind:?}");
        for (status, errno) in [
            (Status::NvFailed, EIO),
            (Status::Corrupt, EIO),
            (Status::Failed, EIO),
            (Status::NoSuchVolume, ENODEV),
            (Status::Stopped, ENODEV),
            (Status::NoSpace, ENOSPC),
            (Status::OutOfRange, EINVAL),
            (Status::TooLarge, EINVAL),
        ] {
            assert_eq!(errno_for(status, kind), -errno, "{status:?} {kind:?}");
        }
    }
    // The engine's largest read or write (8 MiB) fits an i32 byte count.
    let largest = RequestKind::Write {
        vol: VOL,
        lba: 0,
        blocks: otap_cp::MAX_REQUEST_BLOCKS,
    };
    assert_eq!(errno_for(Status::Ok, &largest), 8 << 20);
}

/// Descriptors at the edges of the sector arithmetic: the last block of
/// the volume, one past it, sector counts that are not whole blocks, the
/// largest sector count, start sectors near `u64::MAX`, flushes with any
/// range, and unknown operation codes.
#[test]
fn descriptors_at_the_edges_decode_or_refuse() {
    let last = (SIZE - 1) * SPB;
    assert_eq!(
        decode_request(&desc(UBLK_IO_OP_WRITE, last, SPB as u32), VOL, SIZE),
        Ok(RequestKind::Write {
            vol: VOL,
            lba: SIZE - 1,
            blocks: 1
        })
    );
    for (op, start, sectors) in [
        (UBLK_IO_OP_READ, last, 2 * SPB as u32),
        (UBLK_IO_OP_DISCARD, SIZE * SPB, SPB as u32),
        (UBLK_IO_OP_WRITE_ZEROES, 0, (SIZE * SPB) as u32 + 8),
        (UBLK_IO_OP_READ, 0, 0),
        (UBLK_IO_OP_READ, 1, SPB as u32),
        (UBLK_IO_OP_WRITE, 0, 7),
        (UBLK_IO_OP_WRITE, 0, u32::MAX),
        (UBLK_IO_OP_WRITE, 0, u32::MAX - 7),
        (UBLK_IO_OP_READ, u64::MAX - 7, SPB as u32),
        (UBLK_IO_OP_READ, u64::MAX, SPB as u32),
    ] {
        assert_eq!(
            decode_request(&desc(op, start, sectors), VOL, SIZE),
            Err(EINVAL),
            "op {op} start {start} sectors {sectors}"
        );
    }
    // A flush carries no range: any sectors decode to a flush.
    for (start, sectors) in [(0, 0), (u64::MAX, u32::MAX), (SIZE * SPB, 8)] {
        assert_eq!(
            decode_request(&desc(UBLK_IO_OP_FLUSH, start, sectors), VOL, SIZE),
            Ok(RequestKind::Flush { vol: VOL })
        );
    }
    // Flag bits above the operation byte do not change the operation.
    let mut fua = desc(UBLK_IO_OP_WRITE, 0, SPB as u32);
    fua.op_flags |= 1 << 8;
    assert!(matches!(
        decode_request(&fua, VOL, SIZE),
        Ok(RequestKind::Write { .. })
    ));
    for op in [4u8, 6, 9, 10, 18, 22, 0x7f, 0xff] {
        assert_eq!(
            decode_request(&desc(op, 0, SPB as u32), VOL, SIZE),
            Err(EOPNOTSUPP),
            "op {op}"
        );
    }
}

/// With `high < low` the gate has no hysteresis but never lets more than
/// `high` requests into the engine, and every request is served.
#[test]
fn a_high_mark_below_the_low_mark_still_bounds_the_engine() {
    let mut fd = door(8, Watermarks { high: 2, low: 5 });
    for tag in 0..8u16 {
        fd.transport().submit_read(tag, u64::from(tag), 1);
    }
    let mut held = Vec::new();
    let mut done = 0;
    for _ in 0..64 {
        while let Some(req) = fd.fetch() {
            held.push(req);
            assert!(fd.in_engine() <= 2);
        }
        if let Some(req) = held.pop() {
            fd.copy_out(&req, &block(1, 1)).unwrap();
            fd.complete(req, Status::Ok, 0);
            done += 1;
        }
    }
    assert_eq!(done, 8);
    assert_eq!(fd.transport().take_commits().len(), 8);
}

/// A tag's generation advances with every completion and abort, so after
/// many reuses an id from any earlier use is stale; ids round-trip through
/// their fields at the edges of each.
#[test]
fn generations_keep_every_earlier_use_of_a_tag_stale() {
    let mut fd = door(2, Watermarks::for_depth(2));
    let mut old = Vec::new();
    for i in 0..300u32 {
        fd.transport().submit_write(1, 0, block(i as u8, 1));
        let req = fd.fetch().unwrap();
        let (_, tag, gen) = split_id(req.id);
        assert_eq!((tag, gen), (1, i));
        let mut buf = block(0, 1);
        fd.copy_in(&req, &mut buf).unwrap();
        assert_eq!(buf, block(i as u8, 1));
        fd.complete(req, Status::Ok, 0);
        old.push(req);
    }
    assert_eq!(fd.transport().take_commits().len(), 300);
    for req in old.iter().step_by(37) {
        fd.complete(*req, Status::Ok, 0);
    }
    assert!(fd.transport().take_commits().is_empty());
    for (q, tag, gen) in [
        (0, 0, 0),
        (u16::MAX, u16::MAX, u32::MAX),
        (1, u16::MAX, 0),
        (u16::MAX, 0, 1),
    ] {
        assert_eq!(split_id(request_id(q, tag, gen)), (q, tag, gen));
    }
    assert_eq!(request_id(0, 0, 1), ReqId(1 << 32));
}

/// A request on a tag at `u16::MAX` with the deepest table the type allows
/// is beyond the table and ignored (the driver never sends one); the
/// table's last tag works.
#[test]
fn the_deepest_tag_table_serves_its_last_tag() {
    let depth = u16::MAX;
    let mut fd = door(depth, Watermarks::for_depth(depth));
    fd.transport().submit_read(depth - 1, 3, 2);
    let req = fd.fetch().unwrap();
    assert_eq!(split_id(req.id).1, depth - 1);
    fd.copy_out(&req, &block(9, 2)).unwrap();
    fd.complete(req, Status::Ok, 0);
    assert_eq!(
        fd.transport().take_commits(),
        vec![(depth - 1, 2 * BLOCK_SIZE as i32)]
    );
    fd.transport().inject(QueueEvent::Request {
        tag: u16::MAX,
        desc: desc(UBLK_IO_OP_READ, 0, SPB as u32),
    });
    assert!(fd.fetch().is_none());
    assert!(fd.transport().take_commits().is_empty());
}

proptest! {
    #![proptest_config(ProptestConfig {
        failure_persistence: None,
        ..ProptestConfig::default()
    })]

    /// `decode_request` never panics, and what it accepts is whole blocks
    /// inside the volume (or a flush).
    #[test]
    fn decode_accepts_only_whole_blocks_inside_the_volume(
        op in any::<u8>(),
        flags in any::<u32>(),
        start in prop_oneof![any::<u64>(), 0u64..SIZE * SPB + 64],
        sectors in prop_oneof![any::<u32>(), 0u32..512],
        size in prop_oneof![Just(SIZE), 1u64..1 << 40],
    ) {
        let d = UblksrvIoDesc {
            op_flags: u32::from(op) | (flags & !0xff),
            nr_sectors: sectors,
            start_sector: start,
            addr: 0,
        };
        match decode_request(&d, VOL, size) {
            Ok(RequestKind::Flush { vol }) => prop_assert_eq!(vol, VOL),
            Ok(RequestKind::Read { lba, blocks, .. })
            | Ok(RequestKind::Write { lba, blocks, .. })
            | Ok(RequestKind::Discard { lba, blocks, .. })
            | Ok(RequestKind::WriteZeroes { lba, blocks, .. }) => {
                prop_assert!(blocks > 0);
                prop_assert_eq!(lba * SPB, start);
                prop_assert_eq!(u64::from(blocks) * SPB, u64::from(sectors));
                prop_assert!(lba + u64::from(blocks) <= size);
            }
            Err(e) => prop_assert!(e == EINVAL || e == EOPNOTSUPP),
        }
    }

    /// Ids round-trip for every field value.
    #[test]
    fn request_ids_round_trip(q in any::<u16>(), tag in any::<u16>(), gen in any::<u32>()) {
        prop_assert_eq!(split_id(request_id(q, tag, gen)), (q, tag, gen));
    }
}

// ---------------------------------------------------------------------------
// The engine behind the front door
// ---------------------------------------------------------------------------

const STRIPE: u32 = 32 * 1024;
const SPS: u64 = 8;
const SEGMENTS: u64 = 40;
const DEPTH: u16 = 16;

type UblkEngine = Engine<SimBackstore, UblkFrontDoor<MockTransport>, SimClock>;

fn geometry() -> Geometry {
    let stripe = u64::from(STRIPE);
    let slot_stripes = (SUPERBLOCK_SLOT_SIZE as u64).div_ceil(stripe);
    let intent_stripes = (INTENT_SLOT_SIZE as u64).div_ceil(stripe);
    let front = (2 * slot_stripes + 2 * intent_stripes) * stripe;
    let back = 2 * slot_stripes * stripe;
    Geometry {
        stripe_bytes: STRIPE,
        segment_bytes: SPS * stripe,
        capacity_bytes: front + back + SEGMENTS * SPS * stripe,
        flush_opaque: false,
    }
}

fn engine() -> UblkEngine {
    let mut profile = MediaProfile::flash(geometry());
    profile.inflight_stripes = 4;
    let mut cfg = EngineConfig::new(
        Uuid::from_bytes([0x61; 16]),
        vec![VolumeSpec {
            vol: VOL,
            size_blocks: 1024,
        }],
    );
    cfg.fold = FoldPolicy {
        log_bytes: 64 * 1024,
        interval_ns: 60_000_000_000,
        max_cps: 3,
        max_dirty_pages: 8,
        max_overlay_entries: 512,
    };
    cfg.cache_pages = 8;
    // Virtual time: a paced acknowledgement would wait for an advance
    // nobody makes.
    cfg.pacing.max_delay_ns = 0;
    let clock = SimClock::new(1_700_000_000_000_000_000);
    let mut disk = SimBackstore::new(profile, 17).unwrap();
    UblkEngine::format(&mut disk, &clock, &cfg).unwrap();
    let front = UblkFrontDoor::new(
        MockTransport::new(DEPTH),
        0,
        VOL,
        1024,
        DEPTH,
        Watermarks::for_depth(DEPTH),
    );
    let memory = HugeArena::anonymous(2 << 20, HugePageSize::TwoMiB).unwrap();
    let mut e = UblkEngine::boot(cfg, disk, front, clock, memory)
        .map_err(|f| f.error)
        .unwrap();
    tick_until(&mut e, |e| e.is_serving(), "the boot commit");
    e
}

fn tick_until(e: &mut UblkEngine, mut done: impl FnMut(&mut UblkEngine) -> bool, what: &str) {
    for _ in 0..200_000 {
        if done(e) {
            return;
        }
        e.tick().unwrap();
    }
    panic!("{what} did not happen");
}

fn wait(e: &mut UblkEngine, tag: u16) -> i32 {
    let mut found = None;
    tick_until(
        e,
        |e| {
            found = e.front_door().transport().take_commit(tag);
            found.is_some()
        },
        &format!("tag {tag}"),
    );
    found.unwrap()
}

fn request_cp(e: &mut UblkEngine) {
    let before = e.cp_seq();
    e.request_cp();
    tick_until(e, |e| e.cp_seq() > before, "the CP");
}

/// Copies that fail mid-flight (the client's buffer gone) end in `EIO`
/// for writes and reads alike, release every arena slot they took, leave
/// the volume's data as it was, and the next requests on the same tags
/// work.
#[test]
fn failing_copies_through_the_engine_leak_no_arena_slot() {
    let mut e = engine();
    e.front_door()
        .transport()
        .submit_write(0, 10, block(0x11, 4));
    assert_eq!(wait(&mut e, 0), 4 * BLOCK_SIZE as i32);
    request_cp(&mut e);
    assert_eq!(e.arena_occupancy(), 0.0, "the CP released the slots");
    e.front_door().transport().fail_copies(true);
    for tag in 1..9u16 {
        let lba = u64::from(tag) * 8;
        e.front_door()
            .transport()
            .submit_write(tag, lba, block(tag as u8, 8));
    }
    for tag in 1..9u16 {
        assert_eq!(wait(&mut e, tag), -EIO, "write on tag {tag}");
    }
    e.front_door().transport().submit_read(9, 10, 4);
    assert_eq!(wait(&mut e, 9), -EIO);
    assert_eq!(e.arena_occupancy(), 0.0, "no slot leaked");
    assert_eq!(e.unflushed_blocks(), 0);
    e.front_door().transport().fail_copies(false);
    for tag in 1..10u16 {
        e.front_door().transport().submit_read(tag, 10, 4);
        assert_eq!(wait(&mut e, tag), 4 * BLOCK_SIZE as i32);
        assert_eq!(
            e.front_door().transport().read_payload(tag).unwrap(),
            block(0x11, 4)
        );
    }
    e.front_door()
        .transport()
        .submit_write(3, 100, block(0x33, 2));
    assert_eq!(wait(&mut e, 3), 2 * BLOCK_SIZE as i32);
    request_cp(&mut e);
    assert_eq!(e.arena_occupancy(), 0.0);
    assert_eq!(e.front_door().stats().errors, 9);
}

/// Taking the device offline while requests are inside the engine (some
/// deferred behind a full arena) lets those finish with their real
/// results; everything that arrives while offline is `ENODEV` and never
/// reaches the engine; back online, the device serves again.
#[test]
fn going_offline_with_requests_in_the_engine_finishes_them_normally() {
    let mut e = engine();
    for tag in 0..8u16 {
        let lba = u64::from(tag) * 64;
        e.front_door()
            .transport()
            .submit_write(tag, lba, block(tag as u8 + 1, 64));
    }
    e.tick().unwrap();
    let in_engine = e.front_door().in_engine();
    e.front_door().set_offline(true);
    for tag in 8..12u16 {
        e.front_door().transport().submit_read(tag, 0, 1);
    }
    let mut results = std::collections::BTreeMap::new();
    tick_until(
        &mut e,
        |e| {
            for (t, r) in e.front_door().transport().take_commits() {
                results.insert(t, r);
            }
            results.len() == 12
        },
        "every request",
    );
    for tag in 0..8u16 {
        assert_eq!(results[&tag], 64 * BLOCK_SIZE as i32, "tag {tag}");
    }
    for tag in 8..12u16 {
        assert_eq!(results[&tag], -ENODEV, "tag {tag}");
    }
    assert!(in_engine >= 1);
    assert_eq!(e.front_door().stats().offline_rejected, 4);
    e.front_door().set_offline(false);
    for tag in 0..8u16 {
        let lba = u64::from(tag) * 64;
        e.front_door().transport().submit_read(tag, lba, 64);
        assert_eq!(wait(&mut e, tag), 64 * BLOCK_SIZE as i32);
        assert_eq!(
            e.front_door().transport().read_payload(tag).unwrap(),
            block(tag as u8 + 1, 64)
        );
    }
}

/// A clean shutdown with requests deferred inside the engine answers them
/// (every acknowledged write is in the final CP; the rest are refused with
/// `ENODEV`), and nothing is left owed to the driver.
#[test]
fn a_shutdown_with_requests_in_the_engine_answers_every_tag() {
    let mut e = engine();
    for tag in 0..DEPTH {
        let lba = (u64::from(tag) * 16) % 240;
        e.front_door()
            .transport()
            .submit_write(tag, lba, block(tag as u8, 16));
    }
    e.tick().unwrap();
    e.shutdown();
    let mut results = std::collections::BTreeMap::new();
    tick_until(
        &mut e,
        |e| {
            for (t, r) in e.front_door().transport().take_commits() {
                results.insert(t, r);
            }
            e.is_stopped() && results.len() == usize::from(DEPTH)
        },
        "every tag answered",
    );
    assert_eq!(e.front_door().transport().outstanding(), 0);
    assert_eq!(e.front_door().in_engine(), 0);
    for (tag, r) in &results {
        assert!(
            *r == 16 * BLOCK_SIZE as i32 || *r == -ENODEV,
            "tag {tag}: {r}"
        );
    }
}
