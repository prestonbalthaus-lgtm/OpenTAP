//! The engine over a real file: format, boot, writes, discards, snapshots,
//! consistency points, a restart from the same file, and the backstore's
//! own contract. This is the whole stack outside the simulator, on any
//! host.

use std::path::PathBuf;

use otap_backstore::FileBackstore;
use otap_cp::{
    Backstore, DevClass, Done, Engine, EngineConfig, Event, FoldPolicy, Geometry, IoError,
    MediaProfile, PeekSource, Status, Tag, VolumeSpec,
};
use otap_format::{
    IntentSlot, SbLocation, SnapKind, SnapOrigin, SuperblockSlot, Uuid, BLOCK_SIZE,
    INTENT_SLOT_SIZE, SUPERBLOCK_SLOT_SIZE,
};
use otap_sim::{Model, SimClock, SimFrontDoor};
use otap_sys::{HugeArena, HugePageSize};

const STRIPE: u32 = 32 * 1024;
const SPS: u64 = 8;
const SEGMENTS: u64 = 40;

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

fn profile() -> MediaProfile {
    let mut p = MediaProfile::flash(geometry());
    p.inflight_stripes = 4;
    p
}

fn config() -> EngineConfig {
    let mut cfg = EngineConfig::new(
        Uuid::from_bytes([0x42; 16]),
        vec![
            VolumeSpec {
                vol: 0,
                size_blocks: 256,
            },
            VolumeSpec {
                vol: 1,
                size_blocks: 64,
            },
        ],
    );
    cfg.fold = FoldPolicy {
        log_bytes: 64 * 1024,
        interval_ns: 60_000_000_000,
        max_cps: 4,
        max_dirty_pages: 8,
        max_overlay_entries: 512,
    };
    cfg.cache_pages = 8;
    // Virtual time: a paced acknowledgement would wait for an advance
    // nobody makes.
    cfg.pacing.max_delay_ns = 0;
    cfg
}

fn temp_path(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("otap-backstore-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    dir.join(name)
}

type FileEngine = Engine<FileBackstore, SimFrontDoor, SimClock>;

fn tick_until(e: &mut FileEngine, mut done: impl FnMut(&mut FileEngine) -> bool, what: &str) {
    for _ in 0..200_000 {
        if done(e) {
            return;
        }
        e.tick().unwrap();
    }
    panic!("{what} did not happen");
}

fn write(e: &mut FileEngine, op: u64, vol: u32, lba: u64, blocks: u32) {
    let id = e
        .front_door()
        .submit_write(vol, lba, Model::data_of(op, vol, lba, blocks));
    tick_until(
        e,
        |e| {
            e.front_door().take_completions().iter().any(|c| {
                assert_eq!(c.status, Status::Ok, "write {op}");
                c.req.id == id
            })
        },
        "the write",
    );
}

fn punch(e: &mut FileEngine, vol: u32, lba: u64, blocks: u32) {
    let id = e.front_door().submit_discard(vol, lba, blocks);
    tick_until(
        e,
        |e| {
            e.front_door().take_completions().iter().any(|c| {
                assert_eq!(c.status, Status::Ok, "discard");
                c.req.id == id
            })
        },
        "the discard",
    );
}

fn request_cp(e: &mut FileEngine) -> u64 {
    let before = e.cp_seq();
    e.request_cp();
    tick_until(e, |e| e.cp_seq() > before, "the requested CP");
    e.cp_seq()
}

fn expect_block(e: &mut FileEngine, vol: u32, lba: u64, want: Option<(u64, u32)>) -> PeekSource {
    let mut buf = vec![0u8; BLOCK_SIZE];
    let source = e.peek(vol, lba, &mut buf).unwrap()[0];
    let expected = match want {
        Some((op, pos)) => otap_sim::block_data(op, vol, lba - u64::from(pos), pos),
        None => vec![0u8; BLOCK_SIZE],
    };
    assert_eq!(buf, expected, "volume {vol} LBA {lba}");
    source
}

#[test]
fn the_engine_runs_over_a_file_and_restarts_from_it() {
    let path = temp_path("pool.img");
    let clock = SimClock::new(1_700_000_000_000_000_000);
    let mut disk = FileBackstore::create(&path, profile()).unwrap();
    FileEngine::format(&mut disk, &clock, &config()).unwrap();
    assert!(disk.stats().commits >= 2);
    let memory = HugeArena::anonymous(2 << 20, HugePageSize::TwoMiB).unwrap();
    let mut e = FileEngine::boot(config(), disk, SimFrontDoor::new(), clock.clone(), memory)
        .map_err(|f| f.error)
        .unwrap();
    tick_until(&mut e, |e| e.is_serving(), "the boot commit");
    let events = e.take_events();
    assert!(events
        .iter()
        .any(|ev| matches!(ev, Event::Booted { dirty: false, .. })));
    // Writes, a CP, more writes, a discard, a snapshot, folds.
    write(&mut e, 1, 0, 0, 8);
    write(&mut e, 2, 1, 10, 4);
    let cp = request_cp(&mut e);
    assert!(cp >= 2);
    assert_eq!(expect_block(&mut e, 0, 3, Some((1, 3))), PeekSource::Disk);
    write(&mut e, 3, 0, 4, 8);
    punch(&mut e, 0, 0, 2);
    e.create_snapshot(
        0,
        Uuid::from_bytes([9; 16]),
        SnapKind::Crash,
        SnapOrigin::User,
    )
    .unwrap();
    tick_until(&mut e, |e| !e.snapshots().is_empty(), "the snapshot");
    let snap = e.snapshots()[0].snap_id;
    assert_eq!(expect_block(&mut e, 0, 0, None), PeekSource::Hole);
    assert_eq!(expect_block(&mut e, 0, 5, Some((3, 1))), PeekSource::Disk);
    for _ in 0..3 {
        write(&mut e, 4, 0, 100, 3);
        request_cp(&mut e);
    }
    assert!(e.stats().folds >= 1, "the fold policy folded");
    // The snapshot still reads the state at its CP.
    let mut buf = vec![0u8; BLOCK_SIZE];
    e.snapshot_read(snap, 100, &mut buf).unwrap();
    assert!(
        buf.iter().all(|b| *b == 0),
        "LBA 100 was unmapped at the snapshot"
    );
    e.snapshot_read(snap, 5, &mut buf).unwrap();
    assert_eq!(buf, otap_sim::block_data(3, 0, 4, 1));
    let diff = e.live_diff(0, Some(snap)).unwrap();
    assert_eq!(
        diff.iter().map(|(l, _)| *l).collect::<Vec<_>>(),
        vec![100, 101, 102]
    );
    // A clean shutdown, then a boot from the same file with a fresh arena:
    // everything committed is there, the snapshot included.
    e.shutdown();
    tick_until(&mut e, |e| e.is_stopped(), "the shutdown");
    let (disk, _, _, _) = e.into_parts();
    drop(disk);
    let disk = FileBackstore::open(&path, profile()).unwrap();
    let memory = HugeArena::anonymous(2 << 20, HugePageSize::TwoMiB).unwrap();
    let mut e = FileEngine::boot(config(), disk, SimFrontDoor::new(), clock, memory)
        .map_err(|f| f.error)
        .unwrap();
    tick_until(&mut e, |e| e.is_serving(), "the second boot commit");
    let events = e.take_events();
    assert!(events.iter().any(|ev| matches!(
        ev,
        Event::Booted {
            dirty: false,
            rewound: false,
            ..
        }
    )));
    assert!(e.nvfail_set().is_empty());
    assert_eq!(expect_block(&mut e, 0, 0, None), PeekSource::Hole);
    assert_eq!(expect_block(&mut e, 0, 3, Some((1, 3))), PeekSource::Disk);
    assert_eq!(expect_block(&mut e, 0, 5, Some((3, 1))), PeekSource::Disk);
    assert_eq!(expect_block(&mut e, 0, 101, Some((4, 1))), PeekSource::Disk);
    assert_eq!(expect_block(&mut e, 1, 12, Some((2, 2))), PeekSource::Disk);
    assert_eq!(e.snapshots().len(), 1);
    e.snapshot_read(snap, 5, &mut buf).unwrap();
    assert_eq!(buf, otap_sim::block_data(3, 0, 4, 1));
    let status = e.pool_status();
    assert!(!status.flush_opaque && status.warnings.is_empty());
    let (disk, _, _, _) = e.into_parts();
    drop(disk);
    std::fs::remove_file(&path).unwrap();
}

#[test]
fn the_file_backstore_keeps_the_contract() {
    let path = temp_path("contract.img");
    let mut disk = FileBackstore::create(&path, profile()).unwrap();
    let mut out = Vec::new();
    // Whole blocks only, inside the device.
    let image = vec![7u8; STRIPE as usize];
    // The middle of the device: clear of the superblock and intent slots at
    // either end, which the commit and the intent below write.
    let data =
        geometry().capacity_bytes / 2 / u64::from(STRIPE) * (u64::from(STRIPE) / BLOCK_SIZE as u64);
    assert!(disk.write_stripe(data, &image, Tag(1)).is_ok());
    assert!(disk.write_meta(data + 16, &image, Tag(2)).is_ok());
    assert!(disk.flush(DevClass::Pool, Tag(3)).is_ok());
    assert_eq!(disk.poll(&mut out), 3);
    assert!(out.iter().all(|d| d.result.is_ok()));
    let mut back = vec![0u8; STRIPE as usize];
    disk.read(data, &mut back).unwrap();
    assert_eq!(back, image);
    let mut short = vec![0u8; 100];
    assert!(matches!(
        disk.read(data, &mut short),
        Err(IoError::BadLength { bytes: 100 })
    ));
    let last = geometry().capacity_bytes / BLOCK_SIZE as u64;
    assert!(matches!(
        disk.read(last, &mut back),
        Err(IoError::OutOfRange { .. })
    ));
    out.clear();
    disk.write_stripe(last, &image, Tag(4)).unwrap();
    disk.poll(&mut out);
    assert!(matches!(
        out[0],
        Done {
            tag: Tag(4),
            result: Err(IoError::OutOfRange { .. })
        }
    ));
    // A commit and an intent are durable on completion (synced), and a
    // discard completes as a no-op that keeps the data.
    let mut slot = SuperblockSlot::new(Uuid::from_bytes([1; 16]), 2, SbLocation::A);
    slot.seal().unwrap();
    let mut intent = IntentSlot::new(Uuid::from_bytes([1; 16]), 1);
    intent.seal();
    out.clear();
    disk.commit(0, &slot, Tag(5)).unwrap();
    disk.write_intent(16, &intent, Tag(6)).unwrap();
    disk.discard_segment(data, u64::from(STRIPE), Tag(7))
        .unwrap();
    assert_eq!(disk.poll(&mut out), 3);
    assert!(out.iter().all(|d| d.result.is_ok()));
    disk.read(data, &mut back).unwrap();
    assert_eq!(back, image);
    let stats = disk.stats();
    assert_eq!((stats.commits, stats.intents, stats.discards), (1, 1, 1));
    drop(disk);
    // Opening a shorter file is refused.
    std::fs::File::create(&path).unwrap().set_len(1024).unwrap();
    assert!(FileBackstore::open(&path, profile()).is_err());
    std::fs::remove_file(&path).unwrap();
}
