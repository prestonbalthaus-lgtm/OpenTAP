//! Step 4 QA (Opus-QA-Adversarial): the portable file backstore at the
//! edges of its contract (the last block, a capacity that is not whole
//! blocks, zero-length and misaligned buffers, addresses that overflow,
//! discards past the end, completion order), and the engine over a file in
//! the situations a real file meets: an odd capacity, a reformat over an
//! older pool, a file opened with the wrong geometry.

use std::path::PathBuf;

use otap_backstore::FileBackstore;
use otap_cp::{
    Backstore, DevClass, Done, Engine, EngineConfig, EngineError, Geometry, IoError, MediaProfile,
    PeekSource, Status, Tag, VolumeSpec,
};
use otap_format::{Uuid, BLOCK_SIZE, INTENT_SLOT_SIZE, SUPERBLOCK_SLOT_SIZE};
use otap_sim::{Model, SimClock, SimFrontDoor};
use otap_sys::{HugeArena, HugePageSize};

const STRIPE: u32 = 32 * 1024;

fn geometry(segments: u64, tail: u64) -> Geometry {
    let stripe = u64::from(STRIPE);
    let slot_stripes = (SUPERBLOCK_SLOT_SIZE as u64).div_ceil(stripe);
    let intent_stripes = (INTENT_SLOT_SIZE as u64).div_ceil(stripe);
    let front = (2 * slot_stripes + 2 * intent_stripes) * stripe;
    let back = 2 * slot_stripes * stripe;
    Geometry {
        stripe_bytes: STRIPE,
        segment_bytes: 8 * stripe,
        capacity_bytes: front + back + segments * 8 * stripe + tail,
        flush_opaque: false,
    }
}

fn profile(geom: Geometry) -> MediaProfile {
    let mut p = MediaProfile::flash(geom);
    p.inflight_stripes = 4;
    p
}

fn config(agg: u8) -> EngineConfig {
    let mut cfg = EngineConfig::new(
        Uuid::from_bytes([agg; 16]),
        vec![VolumeSpec {
            vol: 0,
            size_blocks: 128,
        }],
    );
    cfg.cache_pages = 8;
    cfg
}

fn temp_path(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("otap-qa-backstore-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    dir.join(name)
}

type FileEngine = Engine<FileBackstore, SimFrontDoor, SimClock>;

fn clock() -> SimClock {
    SimClock::new(1_700_000_000_000_000_000)
}

fn boot(disk: FileBackstore, cfg: EngineConfig) -> Result<FileEngine, EngineError> {
    let memory = HugeArena::anonymous(2 << 20, HugePageSize::TwoMiB).unwrap();
    let mut e =
        FileEngine::boot(cfg, disk, SimFrontDoor::new(), clock(), memory).map_err(|f| f.error)?;
    for _ in 0..100_000 {
        if e.is_serving() {
            return Ok(e);
        }
        e.tick()?;
    }
    panic!("the boot commit never landed");
}

fn tick_until(e: &mut FileEngine, mut done: impl FnMut(&mut FileEngine) -> bool) {
    for _ in 0..100_000 {
        if done(e) {
            return;
        }
        e.tick().unwrap();
    }
    panic!("the condition never held");
}

fn write(e: &mut FileEngine, op: u64, lba: u64, blocks: u32) {
    let id = e
        .front_door()
        .submit_write(0, lba, Model::data_of(op, 0, lba, blocks));
    tick_until(e, |e| {
        e.front_door().take_completions().iter().any(|c| {
            assert_eq!(c.status, Status::Ok);
            c.req.id == id
        })
    });
}

fn cp(e: &mut FileEngine) {
    let before = e.cp_seq();
    e.request_cp();
    tick_until(e, |e| e.cp_seq() > before);
}

/// The contract at its edges: the last whole block reads and writes; a
/// block that runs past the end, a partial last block, an address whose
/// byte offset overflows, a misaligned length and a discard past the end
/// are refused by name; a zero-length buffer is a no-op; completions come
/// back in submission order with their tags.
#[test]
fn the_contract_holds_at_its_edges() {
    let geom = geometry(8, 512);
    let path = temp_path("edges.img");
    let mut disk = FileBackstore::create(&path, profile(geom)).unwrap();
    let whole = geom.capacity_bytes / BLOCK_SIZE as u64;
    let last = whole - 1;
    let mut out: Vec<Done> = Vec::new();
    let one = vec![0x5Au8; BLOCK_SIZE];
    disk.write_meta(last, &one, Tag(1)).unwrap();
    disk.write_stripe(whole, &one, Tag(2)).unwrap();
    disk.write_meta(last, &vec![1u8; 2 * BLOCK_SIZE], Tag(3))
        .unwrap();
    disk.write_meta(u64::MAX / 2, &one, Tag(4)).unwrap();
    disk.write_meta(0, &[0u8; 100], Tag(5)).unwrap();
    disk.write_meta(3, &[], Tag(6)).unwrap();
    disk.discard_segment(last, BLOCK_SIZE as u64, Tag(7))
        .unwrap();
    disk.discard_segment(last, 2 * BLOCK_SIZE as u64, Tag(8))
        .unwrap();
    disk.discard_segment(0, 1000, Tag(9)).unwrap();
    disk.flush(DevClass::Pool, Tag(10)).unwrap();
    assert_eq!(disk.poll(&mut out), 10);
    let tags: Vec<u64> = out.iter().map(|d| d.tag.0).collect();
    assert_eq!(tags, (1..=10).collect::<Vec<_>>());
    assert!(out[0].result.is_ok());
    assert!(matches!(out[1].result, Err(IoError::OutOfRange { .. })));
    assert!(matches!(out[2].result, Err(IoError::OutOfRange { .. })));
    assert!(matches!(out[3].result, Err(IoError::OutOfRange { .. })));
    assert!(matches!(
        out[4].result,
        Err(IoError::BadLength { bytes: 100 })
    ));
    assert!(
        matches!(out[5].result, Err(IoError::BadLength { bytes: 0 })),
        "a zero-length write is refused (the Step 4 review's hazard 13)"
    );
    assert!(out[6].result.is_ok());
    assert!(matches!(out[7].result, Err(IoError::OutOfRange { .. })));
    assert!(matches!(out[8].result, Err(IoError::BadLength { .. })));
    assert!(out[9].result.is_ok());
    let mut back = vec![0u8; BLOCK_SIZE];
    disk.read(last, &mut back).unwrap();
    assert_eq!(back, one, "the discard kept the data");
    assert!(matches!(
        disk.read(whole, &mut back),
        Err(IoError::OutOfRange { .. })
    ));
    assert!(matches!(
        disk.read(u64::MAX, &mut back),
        Err(IoError::OutOfRange { .. })
    ));
    assert!(matches!(
        disk.read(0, &mut [0u8; 3]),
        Err(IoError::BadLength { bytes: 3 })
    ));
    assert!(matches!(
        disk.read(0, &mut []),
        Err(IoError::BadLength { bytes: 0 })
    ));
    drop(disk);
    std::fs::remove_file(&path).unwrap();
}

/// A file whose capacity is not a whole number of blocks: the layout keeps
/// clear of the partial tail, so the engine formats, writes, commits and
/// restarts from the file with everything there.
#[test]
fn an_odd_capacity_file_formats_runs_and_restarts() {
    for tail in [512u64, 4096 + 1024, 3 * 32 * 1024 + 3584] {
        let geom = geometry(12, tail);
        let path = temp_path(&format!("odd-{tail}.img"));
        let mut disk = FileBackstore::create(&path, profile(geom)).unwrap();
        FileEngine::format(&mut disk, &clock(), &config(0x71)).unwrap();
        let mut e = boot(disk, config(0x71)).unwrap();
        write(&mut e, 1, 0, 8);
        write(&mut e, 2, 120, 8);
        cp(&mut e);
        e.request_fold();
        cp(&mut e);
        write(&mut e, 3, 60, 2);
        e.shutdown();
        tick_until(&mut e, |e| e.is_stopped());
        let (disk, _, _, _) = e.into_parts();
        drop(disk);
        let disk = FileBackstore::open(&path, profile(geom)).unwrap();
        let mut e = boot(disk, config(0x71)).unwrap();
        let mut buf = vec![0u8; BLOCK_SIZE];
        for (op, lba, pos) in [(1u64, 3u64, 3u32), (2, 127, 7), (3, 61, 1)] {
            let src = e.peek(0, lba, &mut buf).unwrap()[0];
            assert_eq!(src, PeekSource::Disk, "tail {tail} LBA {lba}");
            assert_eq!(
                buf,
                otap_sim::block_data(op, 0, lba - u64::from(pos), pos),
                "tail {tail} LBA {lba}"
            );
        }
        let (disk, _, _, _) = e.into_parts();
        drop(disk);
        std::fs::remove_file(&path).unwrap();
    }
}

/// A reformat over a file that holds an older pool: the old aggregate can
/// no longer boot from it (its ring was wiped), the new one boots empty,
/// and a file shorter than the profile is refused at open (a longer one is
/// fine).
#[test]
fn a_reformat_and_a_short_file_are_refused_cleanly() {
    let geom = geometry(10, 0);
    let path = temp_path("reformat.img");
    let mut disk = FileBackstore::create(&path, profile(geom)).unwrap();
    FileEngine::format(&mut disk, &clock(), &config(0x11)).unwrap();
    let mut e = boot(disk, config(0x11)).unwrap();
    write(&mut e, 1, 5, 4);
    cp(&mut e);
    let (disk, _, _, _) = e.into_parts();
    drop(disk);
    let mut disk = FileBackstore::open(&path, profile(geom)).unwrap();
    FileEngine::format(&mut disk, &clock(), &config(0x22)).unwrap();
    drop(disk);
    let disk = FileBackstore::open(&path, profile(geom)).unwrap();
    assert!(
        boot(disk, config(0x11)).is_err(),
        "the old aggregate booted after a reformat"
    );
    let disk = FileBackstore::open(&path, profile(geom)).unwrap();
    let mut e = boot(disk, config(0x22)).unwrap();
    let mut buf = vec![0u8; BLOCK_SIZE];
    assert_eq!(e.peek(0, 5, &mut buf).unwrap()[0], PeekSource::Unmapped);
    let (disk, _, _, _) = e.into_parts();
    drop(disk);
    // A file shorter than the profile is refused at open; a longer one is
    // fine.
    let mut longer = geom;
    longer.capacity_bytes -= u64::from(STRIPE);
    assert!(FileBackstore::open(&path, profile(longer)).is_ok());
    let mut shorter = geom;
    shorter.capacity_bytes += 1;
    assert!(FileBackstore::open(&path, profile(shorter)).is_err());
    std::fs::remove_file(&path).unwrap();
}

/// A capacity too small for the layout is refused by the format, by name,
/// before anything is written.
#[test]
fn a_file_too_small_for_the_layout_is_refused_by_the_format() {
    let mut geom = geometry(1, 0);
    geom.capacity_bytes -= 1;
    let path = temp_path("tiny.img");
    let mut disk = FileBackstore::create(&path, profile(geom)).unwrap();
    let err = FileEngine::format(&mut disk, &clock(), &config(0x33)).unwrap_err();
    assert!(matches!(err, EngineError::Layout(_)), "{err}");
    assert_eq!(disk.stats().bytes_written, 0);
    drop(disk);
    std::fs::remove_file(&path).unwrap();
}

/// Found by the Step 4 review (defect 3, fixed): a pool boots under a stripe geometry other than the one it was
/// formatted with, and its boot commit rewrites the device.
///
/// The superblock records no geometry (no stripe, segment or segment-base
/// field), and slot `A0` sits at block 0 for every stripe size, so a boot
/// whose profile carries another stripe (an operator override, or the
/// `otap-geom` planner deciding differently after a kernel, driver or md
/// change: E26 expects the planner to learn) finds a verified slot and
/// lays the pool out anew: the segments start elsewhere (the front region
/// is sized in stripes), the slots `A1`, `B0` and `B1` move (so the newest
/// CP may not even be seen, a silent rewind), and the boot commit writes
/// its metadata and slots at the new positions, over the old layout's
/// segments and slots. The right behaviour: the format records the
/// layout's geometry and the boot refuses a profile that differs (or adopts
/// the recorded geometry); it never writes under a layout it did not
/// format. (A format change: a 2.2 minor with the fields in the reserved
/// area.)
#[test]
fn a_pool_refuses_to_boot_under_another_stripe_geometry() {
    let geom = geometry(10, 0);
    let path = temp_path("regeom.img");
    let mut disk = FileBackstore::create(&path, profile(geom)).unwrap();
    FileEngine::format(&mut disk, &clock(), &config(0x44)).unwrap();
    let mut e = boot(disk, config(0x44)).unwrap();
    write(&mut e, 1, 0, 8);
    cp(&mut e);
    // Two empty CPs: the newest slot at block 0 names no stripe of its own.
    cp(&mut e);
    cp(&mut e);
    let (disk, _, _, _) = e.into_parts();
    drop(disk);
    let before = std::fs::read(&path).unwrap();
    let mut other = geom;
    other.stripe_bytes = 64 * 1024;
    other.segment_bytes = 4 * 64 * 1024;
    let disk = FileBackstore::open(&path, profile(other)).unwrap();
    let outcome = boot(disk, config(0x44));
    let booted = outcome.is_ok();
    drop(outcome);
    let after = std::fs::read(&path).unwrap();
    let rewritten = before
        .chunks(BLOCK_SIZE)
        .zip(after.chunks(BLOCK_SIZE))
        .filter(|(a, b)| a != b)
        .count();
    std::fs::remove_file(&path).unwrap();
    // Before the fix: it boots, and its boot commit rewrites blocks of the old layout.
    assert!(
        !booted && rewritten == 0,
        "booted under 64 KiB stripes: {booted}; {rewritten} blocks of the 32 KiB pool rewritten"
    );
}
