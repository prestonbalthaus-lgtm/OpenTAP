//! The engine behind the ublk front door, over the simulator's device: the
//! requests a ublk queue delivers come back with the results the block
//! layer expects, through the arena, the index and the map; nvfail after a
//! power cut answers `EIO` until cleared; a stopped engine answers
//! `ENODEV`; the engine's own range and size checks come back as `EINVAL`
//! and a missing volume as `ENODEV`.

use otap_cp::{Engine, EngineConfig, Event, FoldPolicy, Geometry, MediaProfile, VolumeSpec};
use otap_format::{Uuid, BLOCK_SIZE, INTENT_SLOT_SIZE, SUPERBLOCK_SLOT_SIZE};
use otap_sim::{SimBackstore, SimClock};
use otap_sys::{HugeArena, HugePageSize};
use otap_ublk::{MockTransport, UblkFrontDoor, Watermarks, EINVAL, EIO, ENODEV};

const STRIPE: u32 = 32 * 1024;
const SPS: u64 = 8;
const SEGMENTS: u64 = 40;
const DEPTH: u16 = 16;
const VOL_BLOCKS: u64 = 256;

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

fn profile() -> MediaProfile {
    let mut p = MediaProfile::flash(geometry());
    p.inflight_stripes = 4;
    p
}

fn config() -> EngineConfig {
    config_for(VOL_BLOCKS)
}

fn config_for(size_blocks: u64) -> EngineConfig {
    let mut cfg = EngineConfig::new(
        Uuid::from_bytes([0x51; 16]),
        vec![VolumeSpec {
            vol: 0,
            size_blocks,
        }],
    );
    cfg.fold = FoldPolicy {
        log_bytes: 64 * 1024,
        interval_ns: 60_000_000_000,
        max_cps: 4,
        max_dirty_pages: 8,
        max_overlay_entries: 512,
    };
    cfg.cache_pages = 8;
    cfg
}

fn front_door(vol: u32, size_blocks: u64) -> UblkFrontDoor<MockTransport> {
    UblkFrontDoor::new(
        MockTransport::new(DEPTH),
        0,
        vol,
        size_blocks,
        DEPTH,
        Watermarks::for_depth(DEPTH),
    )
}

fn boot(disk: SimBackstore, front: UblkFrontDoor<MockTransport>) -> UblkEngine {
    boot_with(config(), disk, front)
}

fn boot_with(
    cfg: EngineConfig,
    disk: SimBackstore,
    front: UblkFrontDoor<MockTransport>,
) -> UblkEngine {
    let clock = SimClock::new(1_700_000_000_000_000_000);
    let memory = HugeArena::anonymous(2 << 20, HugePageSize::TwoMiB).unwrap();
    let mut e = UblkEngine::boot(cfg, disk, front, clock, memory)
        .map_err(|f| f.error)
        .unwrap();
    tick_until(&mut e, |e| e.is_serving(), "the boot commit");
    e
}

fn fresh_engine(vol: u32, size_blocks: u64) -> UblkEngine {
    fresh_engine_with(config(), vol, size_blocks)
}

/// A formatted, booted engine under `cfg`, behind a front door for `vol`
/// that believes the volume holds `size_blocks`.
fn fresh_engine_with(cfg: EngineConfig, vol: u32, size_blocks: u64) -> UblkEngine {
    let clock = SimClock::new(1_700_000_000_000_000_000);
    let mut disk = SimBackstore::new(profile(), 7).unwrap();
    UblkEngine::format(&mut disk, &clock, &cfg).unwrap();
    boot_with(cfg, disk, front_door(vol, size_blocks))
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

/// Ticks until `tag` completes; returns its result. Other tags'
/// completions stay in the mock for their own waits.
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

fn block_pattern(lba: u64, blocks: u32, salt: u8) -> Vec<u8> {
    (0..blocks)
        .flat_map(|i| {
            let b = (lba + u64::from(i)) as u8;
            std::iter::repeat_n(b ^ salt, BLOCK_SIZE)
        })
        .collect()
}

fn write(e: &mut UblkEngine, tag: u16, lba: u64, blocks: u32, salt: u8) {
    e.front_door()
        .transport()
        .submit_write(tag, lba, block_pattern(lba, blocks, salt));
    assert_eq!(
        wait(e, tag),
        blocks as i32 * BLOCK_SIZE as i32,
        "write at {lba}"
    );
}

fn read(e: &mut UblkEngine, tag: u16, lba: u64, blocks: u32) -> Vec<u8> {
    e.front_door().transport().submit_read(tag, lba, blocks);
    assert_eq!(
        wait(e, tag),
        blocks as i32 * BLOCK_SIZE as i32,
        "read at {lba}"
    );
    e.front_door().transport().read_payload(tag).unwrap()
}

#[test]
fn reads_and_writes_round_trip_through_the_engine() {
    let mut e = fresh_engine(0, VOL_BLOCKS);
    // Unwritten blocks read as zeros (a map miss).
    assert_eq!(read(&mut e, 0, 10, 2), vec![0u8; 2 * BLOCK_SIZE]);
    write(&mut e, 1, 10, 4, 0xA5);
    // A read served from the arena.
    assert_eq!(read(&mut e, 2, 10, 4), block_pattern(10, 4, 0xA5));
    assert_eq!(read(&mut e, 3, 12, 1), block_pattern(12, 1, 0xA5));
    // After a CP the data comes from the stripe through the map.
    let before = e.cp_seq();
    e.request_cp();
    tick_until(&mut e, |e| e.cp_seq() > before, "the CP");
    assert_eq!(read(&mut e, 4, 10, 4), block_pattern(10, 4, 0xA5));
    // A flush completes with zero.
    e.front_door().transport().submit_flush(5);
    assert_eq!(wait(&mut e, 5), 0);
    // Overwrite part, then a discard and a write of zeros.
    write(&mut e, 6, 11, 2, 0x3C);
    assert_eq!(
        read(&mut e, 7, 10, 4),
        [
            block_pattern(10, 1, 0xA5),
            block_pattern(11, 2, 0x3C),
            block_pattern(13, 1, 0xA5),
        ]
        .concat()
    );
    e.front_door().transport().submit_discard(8, 10, 2);
    assert_eq!(wait(&mut e, 8), 0);
    e.front_door().transport().submit_write_zeroes(9, 13, 1);
    assert_eq!(wait(&mut e, 9), 0);
    assert_eq!(
        read(&mut e, 10, 10, 4),
        [
            vec![0u8; 2 * BLOCK_SIZE],
            block_pattern(12, 1, 0x3C),
            vec![0u8; BLOCK_SIZE]
        ]
        .concat()
    );
    let s = e.front_door().stats();
    assert_eq!(s.errors, 0);
    assert_eq!((s.discards, s.write_zeroes, s.flushes), (1, 1, 1));
    assert_eq!(e.stats().punches, 2);
}

#[test]
fn many_requests_in_flight_respect_the_watermarks() {
    let mut e = fresh_engine(0, VOL_BLOCKS);
    for tag in 0..DEPTH {
        let lba = u64::from(tag) * 4;
        e.front_door()
            .transport()
            .submit_write(tag, lba, block_pattern(lba, 4, 0x77));
    }
    let mut results = std::collections::BTreeMap::new();
    tick_until(
        &mut e,
        |e| {
            for (t, r) in e.front_door().transport().take_commits() {
                results.insert(t, r);
            }
            results.len() == usize::from(DEPTH)
        },
        "every write",
    );
    assert!(results.values().all(|r| *r == 4 * BLOCK_SIZE as i32));
    assert!(e.front_door().in_engine() <= usize::from(DEPTH));
    for tag in 0..DEPTH {
        let lba = u64::from(tag) * 4;
        assert_eq!(read(&mut e, tag, lba, 4), block_pattern(lba, 4, 0x77));
    }
    assert_eq!(e.front_door().stats().errors, 0);
}

#[test]
fn the_engine_s_own_checks_come_back_as_errnos() {
    // A volume large enough that the engine's request bound is the first
    // check to fail, behind a front door that believes the volume is
    // larger still, so the engine's own range check is reached too.
    let vol_blocks = u64::from(otap_cp::MAX_REQUEST_BLOCKS) * 2;
    let mut e = fresh_engine_with(config_for(vol_blocks), 0, vol_blocks * 2);
    // Past the volume: `OutOfRange` → `EINVAL`.
    e.front_door().transport().submit_read(0, vol_blocks, 1);
    assert_eq!(wait(&mut e, 0), -EINVAL);
    e.front_door()
        .transport()
        .submit_write(1, vol_blocks - 1, vec![0u8; 2 * BLOCK_SIZE]);
    assert_eq!(wait(&mut e, 1), -EINVAL);
    e.front_door()
        .transport()
        .submit_discard(2, vol_blocks - 1, 2);
    assert_eq!(wait(&mut e, 2), -EINVAL);
    // Beyond the engine's request bound: `TooLarge` → `EINVAL`.
    let too_many = otap_cp::MAX_REQUEST_BLOCKS + 1;
    e.front_door().transport().submit_read(3, 0, too_many);
    assert_eq!(wait(&mut e, 3), -EINVAL);
    e.front_door()
        .transport()
        .submit_write(4, 0, vec![0u8; too_many as usize * BLOCK_SIZE]);
    assert_eq!(wait(&mut e, 4), -EINVAL);
    // The front door's own check answers before the engine.
    e.front_door().transport().submit_read(5, vol_blocks * 2, 1);
    assert_eq!(wait(&mut e, 5), -EINVAL);
    // A write still works afterwards.
    write(&mut e, 6, 0, 1, 1);
    let s = e.front_door().stats();
    assert_eq!((s.errors, s.refused), (5, 1));
}

#[test]
fn a_missing_volume_is_enodev() {
    let mut e = fresh_engine(5, VOL_BLOCKS);
    e.front_door().transport().submit_read(0, 0, 1);
    assert_eq!(wait(&mut e, 0), -ENODEV);
    e.front_door().transport().submit_flush(1);
    assert_eq!(wait(&mut e, 1), -ENODEV);
}

#[test]
fn nvfail_after_a_power_cut_answers_eio_until_cleared() {
    let mut e = fresh_engine(0, VOL_BLOCKS);
    write(&mut e, 0, 0, 4, 0x11);
    let before = e.cp_seq();
    e.request_cp();
    tick_until(&mut e, |e| e.cp_seq() > before, "the CP");
    // A write that never reaches a CP, then the power goes.
    write(&mut e, 1, 100, 2, 0x22);
    let (mut disk, mut front, _, _) = e.into_parts();
    let _report = disk.crash_power();
    // The driver forgets its requests with the process; so does the mock.
    front = {
        let _ = front.transport().take_commits();
        front_door(0, VOL_BLOCKS)
    };
    let mut e = boot(disk, front);
    let events = e.take_events();
    let booted = events
        .iter()
        .find_map(|ev| match ev {
            Event::Booted { dirty, nvfail, .. } => Some((*dirty, *nvfail)),
            _ => None,
        })
        .expect("a Booted event");
    assert!(booted.0, "the boot was dirty");
    assert!(booted.1.get(0), "volume 0 is held by nvfail");
    // Every request is EIO.
    e.front_door().transport().submit_read(0, 0, 1);
    assert_eq!(wait(&mut e, 0), -EIO);
    e.front_door()
        .transport()
        .submit_write(1, 0, vec![1u8; BLOCK_SIZE]);
    assert_eq!(wait(&mut e, 1), -EIO);
    e.front_door().transport().submit_flush(2);
    assert_eq!(wait(&mut e, 2), -EIO);
    e.front_door().transport().submit_discard(3, 0, 1);
    assert_eq!(wait(&mut e, 3), -EIO);
    assert_eq!(e.front_door().stats().errors, 4);
    // The operator clears nvfail; the committed data is there, the
    // uncommitted write is gone.
    e.nvfail_clear(0).unwrap();
    tick_until(&mut e, |e| e.nvfail_set().is_empty(), "the clear");
    assert_eq!(read(&mut e, 4, 0, 4), block_pattern(0, 4, 0x11));
    assert_eq!(read(&mut e, 5, 100, 2), vec![0u8; 2 * BLOCK_SIZE]);
    write(&mut e, 6, 100, 2, 0x33);
    assert_eq!(read(&mut e, 7, 100, 2), block_pattern(100, 2, 0x33));
}

#[test]
fn a_stopped_engine_answers_enodev() {
    let mut e = fresh_engine(0, VOL_BLOCKS);
    write(&mut e, 0, 0, 1, 1);
    e.shutdown();
    tick_until(&mut e, |e| e.is_stopped(), "the shutdown");
    e.front_door().transport().submit_read(1, 0, 1);
    e.front_door()
        .transport()
        .submit_write(2, 0, vec![0u8; BLOCK_SIZE]);
    assert_eq!(wait(&mut e, 1), -ENODEV);
    assert_eq!(wait(&mut e, 2), -ENODEV);
    // The process would go offline before the device is torn down: from
    // then on the front door answers without the engine.
    e.front_door().set_offline(true);
    e.front_door().transport().submit_flush(3);
    assert_eq!(wait(&mut e, 3), -ENODEV);
    assert_eq!(e.front_door().stats().offline_rejected, 1);
}

#[test]
fn an_abort_mid_flight_drops_the_completion_and_keeps_the_engine_healthy() {
    // Writes beyond what the arena holds at once are fetched and then
    // deferred inside the engine until a consistency point frees room:
    // those are in flight across ticks. The driver aborts one of them.
    let vol_blocks = 4096;
    let mut e = fresh_engine_with(config_for(vol_blocks), 0, vol_blocks);
    const BLOCKS: u32 = 128;
    for tag in 0..DEPTH {
        let lba = u64::from(tag) * u64::from(BLOCKS);
        e.front_door()
            .transport()
            .submit_write(tag, lba, block_pattern(lba, BLOCKS, 0x44));
    }
    e.tick().unwrap();
    let served: Vec<u16> = e
        .front_door()
        .transport()
        .take_commits()
        .into_iter()
        .map(|(t, r)| {
            assert_eq!(r, BLOCKS as i32 * BLOCK_SIZE as i32);
            t
        })
        .collect();
    let deferred = e.front_door().in_engine();
    assert!(
        deferred >= 1 && served.len() + deferred == usize::from(DEPTH),
        "{served:?} served, {deferred} deferred"
    );
    let victim = (0..DEPTH).find(|t| !served.contains(t)).unwrap();
    e.front_door().transport().abort(victim);
    tick_until(&mut e, |e| e.front_door().stats().aborted == 1, "the abort");
    assert_eq!(e.front_door().in_engine(), deferred - 1);
    // The engine serves the deferred writes as room appears; the aborted
    // one finds its data gone and its completion is dropped.
    let mut done = served.clone();
    tick_until(
        &mut e,
        |e| {
            done.extend(
                e.front_door()
                    .transport()
                    .take_commits()
                    .into_iter()
                    .map(|(t, r)| {
                        assert_eq!(r, BLOCKS as i32 * BLOCK_SIZE as i32, "tag {t}");
                        t
                    }),
            );
            done.len() == usize::from(DEPTH) - 1
        },
        "the other writes",
    );
    assert!(!done.contains(&victim));
    assert_eq!(e.front_door().stats().stale, 1);
    assert_eq!(e.front_door().in_engine(), 0);
    assert_eq!(e.front_door().aborted_tags(), vec![victim]);
    // Every other write's data is there; the aborted one never landed.
    for tag in 0..DEPTH {
        let lba = u64::from(tag) * u64::from(BLOCKS);
        let want = if tag == victim {
            vec![0u8; BLOCKS as usize * BLOCK_SIZE]
        } else {
            block_pattern(lba, BLOCKS, 0x44)
        };
        // The victim's tag is retired; read on a neighbour's.
        let on = if tag == victim {
            (tag + 1) % DEPTH
        } else {
            tag
        };
        assert_eq!(read(&mut e, on, lba, BLOCKS), want, "tag {tag}");
    }
    assert_eq!(e.front_door().stats().errors, 0);
    assert!(e.nvfail_set().is_empty());
}
