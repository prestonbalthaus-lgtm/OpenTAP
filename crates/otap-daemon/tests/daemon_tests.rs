//! The daemon end to end on a file-backed pool, in process: format, boot,
//! the volume and snapshot lifecycle through the control protocol's
//! requests, the data path through the channel front door, a clean
//! restart, a dirty one, the refusals, and the server over TCP.

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use otap_control::{
    decode_hex, encode_hex, ArenaKind, Client, ControlError, Endpoint, ErrorCode, IoResultReport,
    PoolReport, Request, Response, SnapshotReport, VolumeReport, BLOCK_SIZE, PIN_REPLICATION,
    PIN_USER,
};
use otap_daemon::{serve, Daemon, DaemonConfig, Server};

/// A 128 MiB file: 64 KiB stripes and 2 MiB segments (the small-file
/// geometry), 64 segments, a reserve of 8.
const POOL_BYTES: u64 = 128 << 20;
/// The smallest arena the daemon accepts.
const ARENA_BYTES: u64 = 64 << 20;

struct Scratch {
    dir: PathBuf,
}

impl Scratch {
    fn new(name: &str) -> Self {
        let dir = std::env::temp_dir().join(format!("otap-daemon-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("a scratch directory");
        Self { dir }
    }

    fn path(&self, file: &str) -> String {
        self.dir.join(file).display().to_string()
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

fn config() -> DaemonConfig {
    DaemonConfig {
        control: Endpoint::parse("tcp:127.0.0.1:0").unwrap(),
        ublk: false,
        op_timeout: Duration::from_secs(60),
        ..DaemonConfig::default()
    }
}

fn daemon() -> Daemon {
    Daemon::new(config())
}

fn format_req(device: &str, force: bool) -> Request {
    Request::PoolFormat {
        device: device.to_string(),
        size_bytes: Some(POOL_BYTES),
        media: None,
        stripe_bytes: None,
        segment_bytes: None,
        flush_opaque: None,
        strict_flush: false,
        force,
    }
}

fn boot_req(device: &str) -> Request {
    Request::PoolBoot {
        device: device.to_string(),
        media: None,
        stripe_bytes: None,
        segment_bytes: None,
        flush_opaque: None,
        strict_flush: false,
        arena_bytes: ARENA_BYTES,
        arena: ArenaKind::Heap,
        cp_interval_secs: 1,
    }
}

fn ok(d: &mut Daemon, req: &Request) -> Response {
    match d.handle(req) {
        Ok(r) => r,
        Err(e) => panic!("{req:?} failed: {e}"),
    }
}

fn err(d: &mut Daemon, req: &Request) -> ControlError {
    match d.handle(req) {
        Ok(r) => panic!("{req:?} succeeded with {r:?}"),
        Err(e) => e,
    }
}

fn volume(d: &mut Daemon, req: &Request) -> VolumeReport {
    match ok(d, req) {
        Response::Volume(v) => v,
        other => panic!("{req:?} answered {other:?}"),
    }
}

fn snapshot(d: &mut Daemon, req: &Request) -> SnapshotReport {
    match ok(d, req) {
        Response::Snapshot(s) => s,
        other => panic!("{req:?} answered {other:?}"),
    }
}

fn pool(d: &mut Daemon, req: &Request) -> PoolReport {
    match ok(d, req) {
        Response::Pool(p) => p,
        other => panic!("{req:?} answered {other:?}"),
    }
}

fn io(d: &mut Daemon, req: &Request) -> IoResultReport {
    match ok(d, req) {
        Response::Io(r) => r,
        other => panic!("{req:?} answered {other:?}"),
    }
}

fn write_fill(d: &mut Daemon, volume: &str, lba: u64, fill: u8, blocks: usize) -> IoResultReport {
    io(
        d,
        &Request::IoWrite {
            volume: volume.to_string(),
            lba,
            data_hex: encode_hex(&vec![fill; blocks * BLOCK_SIZE]),
        },
    )
}

fn read(d: &mut Daemon, volume: &str, lba: u64, blocks: u32) -> IoResultReport {
    io(
        d,
        &Request::IoRead {
            volume: volume.to_string(),
            lba,
            blocks,
        },
    )
}

fn snapshots(d: &mut Daemon, volume: Option<&str>) -> Vec<SnapshotReport> {
    match ok(
        d,
        &Request::SnapshotList {
            volume: volume.map(str::to_string),
        },
    ) {
        Response::Snapshots { snapshots } => snapshots,
        other => panic!("snapshot list answered {other:?}"),
    }
}

fn volumes(d: &mut Daemon) -> Vec<VolumeReport> {
    match ok(d, &Request::VolumeList) {
        Response::Volumes { volumes } => volumes,
        other => panic!("volume list answered {other:?}"),
    }
}

#[test]
fn a_daemon_without_a_pool_answers_ping_and_refuses_the_rest() {
    let mut d = daemon();
    match ok(&mut d, &Request::Ping) {
        Response::Pong {
            protocol,
            pool_booted,
            version,
        } => {
            assert_eq!(protocol, otap_control::PROTOCOL_VERSION);
            assert!(!pool_booted);
            assert_eq!(version, otap_daemon::VERSION);
        }
        other => panic!("{other:?}"),
    }
    for req in [
        Request::PoolStatus,
        Request::PoolSync,
        Request::PoolClean,
        Request::PoolStop,
        Request::VolumeList,
        Request::SnapshotList { volume: None },
        Request::IoRead {
            volume: "x".into(),
            lba: 0,
            blocks: 1,
        },
    ] {
        assert_eq!(err(&mut d, &req).code, ErrorCode::NoPool, "{req:?}");
    }
    assert!(!d.exit_requested());
    assert_eq!(ok(&mut d, &Request::DaemonStop), Response::Ok);
    assert!(d.exit_requested());
}

#[test]
fn format_boot_and_the_whole_lifecycle_then_a_clean_restart() {
    let scratch = Scratch::new("lifecycle");
    let device = scratch.path("pool.img");
    let mut d = daemon();

    // Format: the small-file geometry, and a refusal to format a pool
    // without force.
    match ok(&mut d, &format_req(&device, false)) {
        Response::Formatted {
            stripe_bytes,
            segment_bytes,
            capacity_bytes,
            segments,
            ..
        } => {
            assert_eq!(stripe_bytes, 64 << 10);
            assert_eq!(segment_bytes, 2 << 20);
            assert_eq!(capacity_bytes, POOL_BYTES);
            assert_eq!(segments, 63);
        }
        other => panic!("{other:?}"),
    }
    let refused = err(&mut d, &format_req(&device, false));
    assert_eq!(refused.code, ErrorCode::BadRequest);
    assert!(refused.message.contains("already holds"), "{refused}");
    assert!(matches!(
        ok(&mut d, &format_req(&device, true)),
        Response::Formatted { .. }
    ));

    // Boot.
    let report = pool(&mut d, &boot_req(&device));
    assert!(report.serving);
    assert_eq!(report.volumes, 0);
    assert_eq!(report.segments, 63);
    assert_eq!(report.reserve_segments, 8);
    assert_eq!(report.arena, "heap");
    assert_eq!(report.arena_bytes, ARENA_BYTES);
    assert_eq!(report.cp_interval_secs, 1);
    assert!(
        report.last_boot.starts_with("clean boot"),
        "{}",
        report.last_boot
    );
    assert_eq!(err(&mut d, &boot_req(&device)).code, ErrorCode::PoolBooted);
    assert_eq!(
        err(&mut d, &format_req(&device, true)).code,
        ErrorCode::PoolBooted
    );

    // Volumes.
    let create = |name: &str, size: u64| Request::VolumeCreate {
        name: name.to_string(),
        size_bytes: size,
    };
    let data = volume(&mut d, &create("data", 16 << 20));
    assert_eq!(data.name.as_deref(), Some("data"));
    assert_eq!(data.size_bytes, 16 << 20);
    assert!(data.serving && !data.offline);
    assert_eq!(data.snapshots, 0);
    assert_eq!(data.device, None);
    assert_eq!(
        err(&mut d, &create("data", 1 << 20)).code,
        ErrorCode::BadRequest,
        "a second volume of the same name"
    );
    assert_eq!(err(&mut d, &create("tiny", 0)).code, ErrorCode::BadRequest);
    assert_eq!(
        err(&mut d, &create("bad/name", 1 << 20)).code,
        ErrorCode::BadRequest
    );
    assert_eq!(volumes(&mut d).len(), 1);
    assert_eq!(
        err(
            &mut d,
            &Request::VolumeShow {
                name: "nope".into()
            }
        )
        .code,
        ErrorCode::NotFound
    );

    // The data path through the channel front door.
    let w = write_fill(&mut d, "data", 0, 0xab, 2);
    assert_eq!(w.status, "ok");
    assert!(w.seq > 0);
    let r = read(&mut d, "data", 0, 2);
    assert_eq!(r.status, "ok");
    assert_eq!(decode_hex(&r.data_hex).unwrap(), vec![0xab; 2 * BLOCK_SIZE]);
    let beyond = read(&mut d, "data", (16 << 20) / BLOCK_SIZE as u64 - 1, 2);
    assert_eq!(beyond.status, "out_of_range");
    assert!(beyond.data_hex.is_empty());
    let punch = io(
        &mut d,
        &Request::IoPunch {
            volume: "data".into(),
            lba: 0,
            blocks: 1,
        },
    );
    assert_eq!(punch.status, "ok");
    assert_eq!(
        decode_hex(&read(&mut d, "data", 0, 1).data_hex).unwrap(),
        vec![0u8; BLOCK_SIZE]
    );
    assert_eq!(
        decode_hex(&read(&mut d, "data", 1, 1).data_hex).unwrap(),
        vec![0xab; BLOCK_SIZE]
    );
    let odd = err(
        &mut d,
        &Request::IoWrite {
            volume: "data".into(),
            lba: 0,
            data_hex: "abc".into(),
        },
    );
    assert_eq!(odd.code, ErrorCode::BadRequest);
    let partial = err(
        &mut d,
        &Request::IoWrite {
            volume: "data".into(),
            lba: 0,
            data_hex: "ab".repeat(100),
        },
    );
    assert_eq!(partial.code, ErrorCode::BadRequest);
    let long = err(
        &mut d,
        &Request::IoRead {
            volume: "data".into(),
            lba: 0,
            blocks: otap_control::MAX_IO_BLOCKS + 1,
        },
    );
    assert_eq!(long.code, ErrorCode::BadRequest);
    assert_eq!(
        err(
            &mut d,
            &Request::IoRead {
                volume: "ghost".into(),
                lba: 0,
                blocks: 1,
            }
        )
        .code,
        ErrorCode::NotFound
    );

    // Snapshots: taken, listed, named uniquely per volume.
    let snap = |volume: &str, name: &str| Request::SnapshotCreate {
        volume: volume.to_string(),
        name: name.to_string(),
    };
    let s1 = snapshot(&mut d, &snap("data", "s1"));
    assert_eq!(s1.name.as_deref(), Some("s1"));
    assert_eq!(s1.volume.as_deref(), Some("data"));
    assert_eq!(s1.consistency, "crash");
    assert_eq!(s1.origin, "user");
    assert_eq!(s1.flags, 0);
    assert!(!s1.damaged && !s1.deleting);
    assert_eq!(err(&mut d, &snap("data", "s1")).code, ErrorCode::BadRequest);
    assert_eq!(err(&mut d, &snap("ghost", "s9")).code, ErrorCode::NotFound);
    assert_eq!(snapshots(&mut d, Some("data")).len(), 1);
    assert_eq!(snapshots(&mut d, None).len(), 1);
    assert_eq!(
        volume(
            &mut d,
            &Request::VolumeShow {
                name: "data".into()
            }
        )
        .snapshots,
        1
    );

    // Rollback of an online volume: offline, flushed, rolled back, online.
    assert_eq!(write_fill(&mut d, "data", 1, 0xcd, 1).status, "ok");
    assert!(matches!(
        ok(&mut d, &Request::PoolSync),
        Response::Committed { .. }
    ));
    assert_eq!(
        decode_hex(&read(&mut d, "data", 1, 1).data_hex).unwrap(),
        vec![0xcd; BLOCK_SIZE]
    );
    let rolled = volume(
        &mut d,
        &Request::SnapshotRollback {
            volume: "data".into(),
            name: "s1".into(),
            delete_newer: false,
        },
    );
    assert_eq!(rolled.rollback_gen, 1);
    assert!(rolled.serving && !rolled.offline);
    assert_eq!(
        decode_hex(&read(&mut d, "data", 1, 1).data_hex).unwrap(),
        vec![0xab; BLOCK_SIZE],
        "the snapshot's content is back"
    );
    assert_eq!(
        decode_hex(&read(&mut d, "data", 0, 1).data_hex).unwrap(),
        vec![0u8; BLOCK_SIZE],
        "the hole was in the snapshot too"
    );

    // Newer snapshots block a rollback unless deleted first.
    snapshot(&mut d, &snap("data", "s2"));
    let blocked = err(
        &mut d,
        &Request::SnapshotRollback {
            volume: "data".into(),
            name: "s1".into(),
            delete_newer: false,
        },
    );
    assert_eq!(blocked.code, ErrorCode::Engine);
    assert!(blocked.message.contains("newer"), "{blocked}");
    let online_again = volume(
        &mut d,
        &Request::VolumeShow {
            name: "data".into(),
        },
    );
    assert!(
        online_again.serving && !online_again.offline,
        "a refused rollback leaves the volume as it was"
    );
    let rolled = volume(
        &mut d,
        &Request::SnapshotRollback {
            volume: "data".into(),
            name: "s1".into(),
            delete_newer: true,
        },
    );
    assert_eq!(rolled.rollback_gen, 2);
    let listed = snapshots(&mut d, Some("data"));
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].name.as_deref(), Some("s1"));

    // Pins refuse deletion; the replication base is a pin of its own.
    let pinned = snapshot(
        &mut d,
        &Request::SnapshotPin {
            volume: "data".into(),
            name: "s1".into(),
            flags: PIN_USER,
        },
    );
    assert_eq!(pinned.flags, PIN_USER);
    let delete = Request::SnapshotDelete {
        volume: "data".into(),
        name: "s1".into(),
    };
    let held = err(&mut d, &delete);
    assert_eq!(held.code, ErrorCode::Engine);
    assert!(held.message.contains("pinned"), "{held}");
    assert_eq!(
        err(
            &mut d,
            &Request::SnapshotPin {
                volume: "data".into(),
                name: "s1".into(),
                flags: PIN_REPLICATION,
            }
        )
        .code,
        ErrorCode::BadRequest,
        "the replication pin is not set by hand"
    );
    let unpinned = snapshot(
        &mut d,
        &Request::SnapshotPin {
            volume: "data".into(),
            name: "s1".into(),
            flags: 0,
        },
    );
    assert_eq!(unpinned.flags, 0);
    let base = snapshot(
        &mut d,
        &Request::SnapshotReplicationBase {
            volume: "data".into(),
            name: Some("s1".into()),
        },
    );
    assert_eq!(base.flags, PIN_REPLICATION);
    assert_eq!(err(&mut d, &delete).code, ErrorCode::Engine);
    let s3 = snapshot(&mut d, &snap("data", "s3"));
    let superseded = snapshot(
        &mut d,
        &Request::SnapshotReplicationBase {
            volume: "data".into(),
            name: Some("s3".into()),
        },
    );
    assert_eq!(superseded.id, s3.id);
    assert_eq!(superseded.flags, PIN_REPLICATION);
    let s1_now = snapshots(&mut d, Some("data"))
        .into_iter()
        .find(|s| s.name.as_deref() == Some("s1"))
        .unwrap();
    assert_eq!(s1_now.flags, 0, "a newer base supersedes the old one");
    assert_eq!(ok(&mut d, &delete), Response::Ok);
    assert_eq!(
        ok(
            &mut d,
            &Request::SnapshotReplicationBase {
                volume: "data".into(),
                name: None,
            }
        ),
        Response::Ok
    );
    assert_eq!(
        ok(
            &mut d,
            &Request::SnapshotDelete {
                volume: "data".into(),
                name: "s3".into(),
            }
        ),
        Response::Ok
    );
    assert!(snapshots(&mut d, None).is_empty());
    assert_eq!(err(&mut d, &delete).code, ErrorCode::NotFound);

    // Resize: a grow online, a shrink offline only.
    let grown = volume(
        &mut d,
        &Request::VolumeResize {
            name: "data".into(),
            size_bytes: 32 << 20,
        },
    );
    assert_eq!(grown.size_bytes, 32 << 20);
    assert_eq!(
        write_fill(&mut d, "data", (32 << 20) / BLOCK_SIZE as u64 - 1, 0x11, 1).status,
        "ok"
    );
    let shrink = Request::VolumeResize {
        name: "data".into(),
        size_bytes: 8 << 20,
    };
    let online = err(&mut d, &shrink);
    assert_eq!(online.code, ErrorCode::Engine);
    assert!(online.message.contains("offline"), "{online}");
    let offline = volume(
        &mut d,
        &Request::VolumeOnline {
            name: "data".into(),
            online: false,
        },
    );
    assert!(offline.offline && !offline.serving);
    assert_eq!(read(&mut d, "data", 0, 1).status, "offline");
    let shrunk = volume(&mut d, &shrink);
    assert_eq!(shrunk.size_bytes, 8 << 20);
    let back = volume(
        &mut d,
        &Request::VolumeOnline {
            name: "data".into(),
            online: true,
        },
    );
    assert!(back.serving && back.size_bytes == 8 << 20);
    assert_eq!(
        decode_hex(&read(&mut d, "data", 1, 1).data_hex).unwrap(),
        vec![0xab; BLOCK_SIZE],
        "what lay below the new end is still there"
    );

    // Rename and delete.
    let renamed = volume(
        &mut d,
        &Request::VolumeRename {
            name: "data".into(),
            new_name: "db".into(),
        },
    );
    assert_eq!(renamed.name.as_deref(), Some("db"));
    assert_eq!(
        err(
            &mut d,
            &Request::VolumeShow {
                name: "data".into()
            }
        )
        .code,
        ErrorCode::NotFound
    );
    let delete_db = Request::VolumeDelete { name: "db".into() };
    assert_eq!(err(&mut d, &delete_db).code, ErrorCode::Engine, "online");
    volume(
        &mut d,
        &Request::VolumeOnline {
            name: "db".into(),
            online: false,
        },
    );
    assert_eq!(ok(&mut d, &delete_db), Response::Ok);
    assert!(volumes(&mut d).is_empty());

    // A cleaning CP and a sync answer with their commits.
    let Response::Committed { cp_seq: cleaned } = ok(&mut d, &Request::PoolClean) else {
        panic!("clean")
    };
    let Response::Committed { cp_seq: synced } = ok(&mut d, &Request::PoolSync) else {
        panic!("sync")
    };
    assert!(synced > cleaned);

    // A volume that survives a clean restart with its name and content.
    volume(&mut d, &create("keep", 16 << 20));
    assert_eq!(write_fill(&mut d, "keep", 7, 0x77, 3).status, "ok");
    let Response::Committed { .. } = ok(&mut d, &Request::PoolStop) else {
        panic!("stop")
    };
    assert!(matches!(
        ok(&mut d, &Request::Ping),
        Response::Pong {
            pool_booted: false,
            ..
        }
    ));
    assert_eq!(err(&mut d, &Request::PoolStatus).code, ErrorCode::NoPool);
    let report = pool(&mut d, &boot_req(&device));
    assert!(
        report.last_boot.starts_with("clean boot"),
        "{}",
        report.last_boot
    );
    assert_eq!(report.volumes, 1);
    let kept = volumes(&mut d);
    assert_eq!(kept.len(), 1);
    assert_eq!(kept[0].name.as_deref(), Some("keep"));
    assert_eq!(kept[0].size_bytes, 16 << 20);
    assert!(kept[0].serving);
    assert_eq!(
        decode_hex(&read(&mut d, "keep", 7, 3).data_hex).unwrap(),
        vec![0x77; 3 * BLOCK_SIZE]
    );
    assert_eq!(ok(&mut d, &Request::DaemonStop), Response::Ok);
    assert!(d.exit_requested());
    assert!(matches!(
        ok(&mut d, &Request::Ping),
        Response::Pong {
            pool_booted: false,
            ..
        }
    ));
}

#[test]
fn a_dirty_restart_holds_the_volume_under_nvfail_until_it_is_cleared() {
    let scratch = Scratch::new("dirty");
    let device = scratch.path("pool.img");
    {
        let mut d = daemon();
        ok(&mut d, &format_req(&device, false));
        pool(&mut d, &boot_req(&device));
        volume(
            &mut d,
            &Request::VolumeCreate {
                name: "data".into(),
                size_bytes: 16 << 20,
            },
        );
        assert_eq!(write_fill(&mut d, "data", 0, 0x5a, 1).status, "ok");
        // No stop: the daemon dies with its heap arena.
    }
    let mut d = daemon();
    let report = pool(&mut d, &boot_req(&device));
    assert!(
        report.last_boot.starts_with("dirty boot"),
        "{}",
        report.last_boot
    );
    let held = volume(
        &mut d,
        &Request::VolumeShow {
            name: "data".into(),
        },
    );
    assert!(held.nvfail.is_some(), "{held:?}");
    assert!(!held.serving);
    assert_eq!(read(&mut d, "data", 0, 1).status, "nvfailed");
    assert_eq!(write_fill(&mut d, "data", 0, 0x5a, 1).status, "nvfailed");
    let cleared = volume(
        &mut d,
        &Request::VolumeNvfailClear {
            name: "data".into(),
        },
    );
    assert_eq!(cleared.nvfail, None);
    assert!(cleared.serving);
    assert_eq!(cleared.rollback_gen, 1);
    assert_eq!(read(&mut d, "data", 0, 1).status, "ok");
    let again = volume(
        &mut d,
        &Request::VolumeNvfailClear {
            name: "data".into(),
        },
    );
    assert_eq!(
        again.rollback_gen, 1,
        "a clear of a clear volume is a no-op"
    );
    assert!(matches!(
        ok(&mut d, &Request::PoolStop),
        Response::Committed { .. }
    ));
}

#[test]
fn boot_refuses_a_missing_blank_or_mismatched_device() {
    let scratch = Scratch::new("refusals");
    let mut d = daemon();
    let missing = err(&mut d, &boot_req(&scratch.path("missing.img")));
    assert_eq!(missing.code, ErrorCode::NotFound);
    let blank = scratch.path("blank.img");
    std::fs::write(&blank, vec![0u8; 1 << 20]).unwrap();
    let no_pool = err(&mut d, &boot_req(&blank));
    assert_eq!(no_pool.code, ErrorCode::NotFound);
    assert!(no_pool.message.contains("superblock"), "{no_pool}");
    let device = scratch.path("pool.img");
    ok(&mut d, &format_req(&device, false));
    let wrong = err(
        &mut d,
        &Request::PoolBoot {
            device: device.clone(),
            media: None,
            stripe_bytes: Some(128 << 10),
            segment_bytes: None,
            flush_opaque: None,
            strict_flush: false,
            arena_bytes: ARENA_BYTES,
            arena: ArenaKind::Heap,
            cp_interval_secs: 1,
        },
    );
    assert_eq!(wrong.code, ErrorCode::BadRequest);
    assert!(wrong.message.contains("65536-byte stripe"), "{wrong}");
    let small = err(
        &mut d,
        &Request::PoolBoot {
            device: device.clone(),
            media: None,
            stripe_bytes: None,
            segment_bytes: None,
            flush_opaque: None,
            strict_flush: false,
            arena_bytes: ARENA_BYTES - (2 << 20),
            arena: ArenaKind::Heap,
            cp_interval_secs: 1,
        },
    );
    assert_eq!(small.code, ErrorCode::BadRequest);
    let slow = err(
        &mut d,
        &Request::PoolBoot {
            device: device.clone(),
            media: None,
            stripe_bytes: None,
            segment_bytes: None,
            flush_opaque: None,
            strict_flush: false,
            arena_bytes: ARENA_BYTES,
            arena: ArenaKind::Heap,
            cp_interval_secs: 16,
        },
    );
    assert_eq!(slow.code, ErrorCode::BadRequest);
    let nowhere = err(
        &mut d,
        &Request::PoolFormat {
            device: scratch.path("sub/dir/pool.img"),
            size_bytes: None,
            media: None,
            stripe_bytes: None,
            segment_bytes: None,
            flush_opaque: None,
            strict_flush: false,
            force: false,
        },
    );
    assert_eq!(nowhere.code, ErrorCode::NotFound, "no size, no file");
    assert!(matches!(ok(&mut d, &boot_req(&device)), Response::Pool(_)));
    assert!(matches!(
        ok(&mut d, &Request::PoolStop),
        Response::Committed { .. }
    ));
}

#[test]
fn a_configured_device_boots_at_start_and_a_bad_one_fails_the_spawn() {
    let scratch = Scratch::new("spawn");
    let device = scratch.path("pool.img");
    {
        let mut d = daemon();
        ok(&mut d, &format_req(&device, false));
    }
    let stop = Arc::new(AtomicBool::new(false));
    let mut cfg = config();
    cfg.device = Some(PathBuf::from(&device));
    cfg.arena = ArenaKind::Heap;
    cfg.arena_bytes = ARENA_BYTES;
    cfg.cp_interval_secs = 1;
    let (handle, thread) = Daemon::spawn(cfg.clone(), Arc::clone(&stop)).unwrap();
    assert!(matches!(
        handle.call(&Request::Ping).unwrap(),
        Response::Pong {
            pool_booted: true,
            ..
        }
    ));
    assert!(matches!(
        handle.call(&Request::PoolStatus).unwrap(),
        Response::Pool(_)
    ));
    // A signal: the flag stops the engine thread with a clean shutdown.
    stop.store(true, Ordering::SeqCst);
    thread.join().unwrap();
    assert!(handle.is_stopped());
    assert_eq!(
        handle.call(&Request::Ping).unwrap_err().code,
        ErrorCode::Io,
        "a stopped daemon answers nothing"
    );
    // The clean shutdown shows at the next boot.
    let mut d = daemon();
    let report = pool(&mut d, &boot_req(&device));
    assert!(
        report.last_boot.starts_with("clean boot"),
        "{}",
        report.last_boot
    );
    ok(&mut d, &Request::PoolStop);

    let mut bad = cfg;
    bad.device = Some(PathBuf::from(scratch.path("missing.img")));
    let failed = Daemon::spawn(bad, Arc::new(AtomicBool::new(false)));
    assert_eq!(failed.err().map(|e| e.code), Some(ErrorCode::NotFound));
}

#[test]
fn the_server_answers_over_tcp_and_survives_a_bad_frame() {
    let scratch = Scratch::new("tcp");
    let device = scratch.path("pool.img");
    let stop = Arc::new(AtomicBool::new(false));
    let server = Server::bind(&Endpoint::parse("tcp:127.0.0.1:0").unwrap()).unwrap();
    let endpoint = server.endpoint().clone();
    assert!(matches!(endpoint, Endpoint::Tcp(addr) if addr.port() != 0));
    let (handle, engine) = Daemon::spawn(config(), Arc::clone(&stop)).unwrap();
    let serving = {
        let handle = handle.clone();
        let stop = Arc::clone(&stop);
        std::thread::spawn(move || serve(server, &handle, &stop))
    };

    let mut client = Client::connect(&endpoint, Some(Duration::from_secs(60))).unwrap();
    assert!(matches!(
        client.hello().unwrap(),
        Response::Pong {
            pool_booted: false,
            ..
        }
    ));
    assert!(matches!(
        client.call(&format_req(&device, false)).unwrap(),
        Response::Formatted { .. }
    ));
    assert!(matches!(
        client.call(&boot_req(&device)).unwrap(),
        Response::Pool(_)
    ));
    let Response::Volume(v) = client
        .call(&Request::VolumeCreate {
            name: "data".into(),
            size_bytes: 16 << 20,
        })
        .unwrap()
    else {
        panic!("a volume")
    };
    assert!(v.serving);
    let Response::Io(w) = client
        .call(&Request::IoWrite {
            volume: "data".into(),
            lba: 3,
            data_hex: encode_hex(&vec![0x3c; BLOCK_SIZE]),
        })
        .unwrap()
    else {
        panic!("a write")
    };
    assert_eq!(w.status, "ok");

    // A second client at the same time, and a frame that is not a request.
    let mut other = Client::connect(&endpoint, Some(Duration::from_secs(60))).unwrap();
    let Response::Io(r) = other
        .call(&Request::IoRead {
            volume: "data".into(),
            lba: 3,
            blocks: 1,
        })
        .unwrap()
    else {
        panic!("a read")
    };
    assert_eq!(decode_hex(&r.data_hex).unwrap(), vec![0x3c; BLOCK_SIZE]);
    {
        use std::io::Write;
        use std::net::TcpStream;
        let Endpoint::Tcp(addr) = endpoint else {
            panic!()
        };
        let mut raw = TcpStream::connect(addr).unwrap();
        raw.set_read_timeout(Some(Duration::from_secs(30))).unwrap();
        let body = br#"{"op":"nonsense"}"#;
        raw.write_all(&(body.len() as u32).to_le_bytes()).unwrap();
        raw.write_all(body).unwrap();
        let reply: Option<otap_control::Reply> = otap_control::read_frame(&mut raw).unwrap();
        let e = reply.unwrap().unwrap_err();
        assert_eq!(e.code, ErrorCode::BadRequest);
        // The connection goes on.
        otap_control::write_frame(&mut raw, &Request::Ping).unwrap();
        let reply: Option<otap_control::Reply> = otap_control::read_frame(&mut raw).unwrap();
        assert!(matches!(reply.unwrap().unwrap(), Response::Pong { .. }));
    }

    assert_eq!(client.call(&Request::DaemonStop).unwrap(), Response::Ok);
    let deadline = Instant::now() + Duration::from_secs(30);
    while !handle.is_stopped() {
        assert!(Instant::now() < deadline, "the engine thread did not stop");
        std::thread::sleep(Duration::from_millis(10));
    }
    engine.join().unwrap();
    serving.join().unwrap().unwrap();
    assert!(Client::connect(&endpoint, Some(Duration::from_secs(5))).is_err());
}
