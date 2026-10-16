//! Opus-QA-Adversarial, Step 5: `dbladed` in process, attacked through
//! the control protocol's requests (the [`Daemon`] directly, the engine
//! thread through its handle, and the TCP server on loopback).
//!
//! The review's `bug_*` tests reproduced four defects (W1, W5, R1, R2 in the
//! Step 5 entry of `08-engineering-log.md`); they are ported in place with
//! the fixed behaviour as their expectation, each with a note naming its
//! defect. The others pin behaviour that was right as reviewed.

use std::io::Write;
use std::net::TcpStream;
use std::path::PathBuf;
use std::sync::atomic::AtomicBool;
use std::sync::Arc;
use std::time::Duration;

use otap_control::{
    decode_hex, encode_hex, read_frame, ArenaKind, Client, ControlError, Endpoint, ErrorCode,
    IoResultReport, Reply, Request, Response, SnapshotReport, VolumeReport, BLOCK_SIZE,
    MAX_FRAME_BYTES,
};
use otap_daemon::{serve, Daemon, DaemonConfig, Server};

/// The lead's harness geometry: a 128 MiB file of 64 KiB stripes and
/// 2 MiB segments, a heap arena of 64 MiB, a 1 s CP timer.
const POOL_BYTES: u64 = 128 << 20;
const ARENA_BYTES: u64 = 64 << 20;

struct Scratch {
    dir: PathBuf,
}

impl Scratch {
    fn new(name: &str) -> Self {
        let dir =
            std::env::temp_dir().join(format!("otap-qa-daemon-{}-{name}", std::process::id()));
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
        op_timeout: Duration::from_secs(30),
        ..DaemonConfig::default()
    }
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

fn io(d: &mut Daemon, req: &Request) -> IoResultReport {
    match ok(d, req) {
        Response::Io(r) => r,
        other => panic!("{req:?} answered {other:?}"),
    }
}

fn create(name: &str, size: u64) -> Request {
    Request::VolumeCreate {
        name: name.to_string(),
        size_bytes: size,
    }
}

fn snap(volume: &str, name: &str) -> Request {
    Request::SnapshotCreate {
        volume: volume.to_string(),
        name: name.to_string(),
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

fn snapshot_names(d: &mut Daemon, volume: &str) -> Vec<String> {
    match ok(
        d,
        &Request::SnapshotList {
            volume: Some(volume.to_string()),
        },
    ) {
        Response::Snapshots { snapshots } => snapshots.into_iter().filter_map(|s| s.name).collect(),
        other => panic!("snapshot list answered {other:?}"),
    }
}

/// A booted daemon over a fresh pool file.
fn booted(scratch: &Scratch) -> (Daemon, String) {
    let device = scratch.path("pool.img");
    let mut d = Daemon::new(config());
    ok(&mut d, &format_req(&device, false));
    ok(&mut d, &boot_req(&device));
    (d, device)
}

// ---------------------------------------------------------------------------
// Defects
// ---------------------------------------------------------------------------

/// `snapshot rollback --delete-newer` deletes the newer snapshots one by
/// one and only then meets one it may not delete (a replication base): the
/// rollback is refused, but the snapshots deleted before the refusal are
/// gone.
#[test]
fn a_rollback_with_delete_newer_refuses_up_front_when_a_newer_snapshot_is_pinned() {
    let scratch = Scratch::new("rollback-newer");
    let (mut d, _) = booted(&scratch);
    volume(&mut d, &create("data", 16 << 20));
    assert_eq!(write_fill(&mut d, "data", 0, 0x11, 1).status, "ok");
    snapshot(&mut d, &snap("data", "s1"));
    assert_eq!(write_fill(&mut d, "data", 0, 0x22, 1).status, "ok");
    snapshot(&mut d, &snap("data", "s2"));
    assert_eq!(write_fill(&mut d, "data", 0, 0x33, 1).status, "ok");
    snapshot(&mut d, &snap("data", "s3"));
    snapshot(
        &mut d,
        &Request::SnapshotReplicationBase {
            volume: "data".into(),
            name: Some("s3".into()),
        },
    );
    let refused = d.handle(&Request::SnapshotRollback {
        volume: "data".into(),
        name: "s1".into(),
        delete_newer: true,
    });
    assert!(
        refused.is_err(),
        "the rollback past a replication base is refused"
    );
    // Review defect R1 (Step 5, `08-engineering-log.md`), fixed; the review's
    // expectation stood and the scenario is kept. As reviewed: a refused rollback must leave the snapshots as they were (check
    // every newer snapshot's pins before deleting any); instead
    // `Pool::snapshot_rollback` deleted s2, then failed on s3.
    let left = snapshot_names(&mut d, "data");
    assert_eq!(
        left,
        vec!["s1", "s2", "s3"],
        "the snapshots after the refusal"
    );
}

/// `pool format` without `--force` refuses a device that holds an OpenTAP
/// pool, but formats over any other existing file or device: a file of
/// someone's data (or, on Linux, a disk with a filesystem) is overwritten
/// without the flag that is meant to guard exactly that.
#[test]
fn pool_format_without_force_refuses_an_existing_file_that_holds_data() {
    let scratch = Scratch::new("format-overwrite");
    let device = scratch.path("database.img");
    let precious = vec![0xa5u8; POOL_BYTES as usize];
    std::fs::write(&device, &precious).unwrap();
    let mut d = Daemon::new(config());
    let reply = d.handle(&format_req(&device, false));
    let after = std::fs::read(&device).unwrap();
    let changed = after.iter().zip(&precious).filter(|(a, b)| a != b).count();
    // Review defect R2 (Step 5, `08-engineering-log.md`), fixed; the review's
    // expectation stood and the scenario is kept. As reviewed: without `--force` a device that holds anything (a non-zero
    // first or last megabyte) must be refused; instead the probe only
    // looks for an OpenTAP superblock, and the format rewrites the head
    // and the tail of the file.
    assert!(
        reply.is_err() && changed == 0,
        "the format answered {reply:?} and changed {changed} bytes of the file"
    );
}

/// The superblock ring keeps two slots at the start of the device and two
/// at the end (F7), so a pool whose A slots were wiped (the first 128 KiB of
/// this 64 KiB-stripe pool: A0 and A1, nothing else) still has two whole
/// commits, and the engine boots from them. The daemon's probe looks only
/// at the start and refuses the device.
#[test]
fn a_pool_whose_a_slots_were_wiped_boots_from_its_b_slots() {
    let scratch = Scratch::new("wiped-head");
    let (mut d, device) = booted(&scratch);
    volume(&mut d, &create("data", 16 << 20));
    assert_eq!(write_fill(&mut d, "data", 5, 0x5e, 2).status, "ok");
    assert!(matches!(
        ok(&mut d, &Request::PoolStop {}),
        Response::Committed { .. }
    ));
    {
        use std::io::{Seek, SeekFrom};
        let mut f = std::fs::OpenOptions::new()
            .write(true)
            .open(&device)
            .unwrap();
        f.seek(SeekFrom::Start(0)).unwrap();
        f.write_all(&vec![0u8; 128 << 10]).unwrap();
        f.sync_all().unwrap();
    }
    // Review defect W5 (Step 5, `08-engineering-log.md`), fixed; the review's
    // expectation stood and the scenario is kept. As reviewed: the boot must find the B slots at the end of the device (the
    // engine ranks all four slots and boots from them; see
    // `the_engine_boots_from_the_b_slots_when_both_a_slots_are_wiped` in
    // the simulator's QA suite); instead `probe_superblock` reads only A0
    // and A1 and answers that the device holds no pool (and a `pool
    // format` without `--force` would then overwrite it).
    let reply = d.handle(&boot_req(&device));
    assert!(reply.is_ok(), "{reply:?}");
    assert_eq!(
        decode_hex(&read(&mut d, "data", 5, 2).data_hex).unwrap(),
        vec![0x5e; 2 * BLOCK_SIZE]
    );
}

/// A snapshot that maps a block beyond its volume's current end (the
/// volume shrank after it was taken) cannot be deleted: the engine's tick
/// fails on every CP from then on, the request answers an engine error, a
/// sync never commits, and a planned stop cannot write its clean record.
#[test]
fn deleting_a_snapshot_after_a_shrink_commits_and_the_pool_stops_cleanly() {
    let scratch = Scratch::new("shrink-delete");
    let (mut d, device) = booted(&scratch);
    volume(&mut d, &create("data", 32 << 20));
    assert_eq!(write_fill(&mut d, "data", 6000, 0x6d, 1).status, "ok");
    assert_eq!(write_fill(&mut d, "data", 10, 0x10, 1).status, "ok");
    snapshot(&mut d, &snap("data", "s1"));
    volume(
        &mut d,
        &Request::VolumeOnline {
            name: "data".into(),
            online: false,
        },
    );
    let shrunk = volume(
        &mut d,
        &Request::VolumeResize {
            name: "data".into(),
            size_bytes: 8 << 20,
        },
    );
    assert_eq!(shrunk.size_bytes, 8 << 20);
    // Review defect W1 (Step 5, `08-engineering-log.md`), fixed; the review's
    // expectation stood and the scenario is kept. As reviewed: the deletion must commit; instead the CP's metadata preparation
    // asks the live map for LBA 6000 of a 2048-block volume, the map
    // answers `OutOfRange`, the tick fails, and it fails again on every CP
    // after (the deletion stays pending).
    let deleted = d.handle(&Request::SnapshotDelete {
        volume: "data".into(),
        name: "s1".into(),
    });
    let stopped = d.handle(&Request::PoolStop {});
    assert!(
        deleted.is_ok() && matches!(stopped, Ok(Response::Committed { .. })),
        "the deletion: {deleted:?}; the clean stop: {stopped:?}"
    );
    // And the next boot is a clean one.
    match ok(&mut d, &boot_req(&device)) {
        Response::Pool(p) => assert!(p.last_boot.starts_with("clean boot"), "{}", p.last_boot),
        other => panic!("{other:?}"),
    }
}

// ---------------------------------------------------------------------------
// Behaviour that is right
// ---------------------------------------------------------------------------

/// Requests from several clients at once are answered in turn by the one
/// engine thread: a client waiting for a consistency point delays the
/// others, never loses them, and every answer is the right one.
#[test]
fn concurrent_clients_are_served_in_turn_and_every_answer_is_right() {
    let scratch = Scratch::new("concurrent");
    let device = scratch.path("pool.img");
    {
        let mut d = Daemon::new(config());
        ok(&mut d, &format_req(&device, false));
    }
    let mut cfg = config();
    cfg.device = Some(PathBuf::from(&device));
    cfg.arena = ArenaKind::Heap;
    cfg.arena_bytes = ARENA_BYTES;
    cfg.cp_interval_secs = 1;
    let stop = Arc::new(AtomicBool::new(false));
    let (handle, thread) = Daemon::spawn(cfg, Arc::clone(&stop)).unwrap();
    for v in 0..3 {
        assert!(matches!(
            handle.call(&create(&format!("v{v}"), 4 << 20)).unwrap(),
            Response::Volume(_)
        ));
    }
    let workers: Vec<_> = (0..3)
        .map(|v| {
            let h = handle.clone();
            std::thread::spawn(move || {
                let vol = format!("v{v}");
                for k in 0..6u8 {
                    let fill = 16 * v as u8 + k;
                    let w = h
                        .call(&Request::IoWrite {
                            volume: vol.clone(),
                            lba: u64::from(k),
                            data_hex: encode_hex(&vec![fill; BLOCK_SIZE]),
                        })
                        .unwrap();
                    assert!(
                        matches!(w, Response::Io(ref r) if r.status == "ok"),
                        "{w:?}"
                    );
                    if k % 2 == 0 {
                        let s = h.call(&snap(&vol, &format!("s{k}"))).unwrap();
                        assert!(matches!(s, Response::Snapshot(_)), "{s:?}");
                    } else {
                        assert!(matches!(
                            h.call(&Request::PoolSync {}).unwrap(),
                            Response::Committed { .. }
                        ));
                    }
                }
                for k in 0..6u8 {
                    let Response::Io(r) = h
                        .call(&Request::IoRead {
                            volume: vol.clone(),
                            lba: u64::from(k),
                            blocks: 1,
                        })
                        .unwrap()
                    else {
                        panic!("a read")
                    };
                    assert_eq!(
                        decode_hex(&r.data_hex).unwrap(),
                        vec![16 * v as u8 + k; BLOCK_SIZE]
                    );
                }
            })
        })
        .collect();
    for _ in 0..20 {
        assert!(matches!(
            handle.call(&Request::Ping {}).unwrap(),
            Response::Pong { .. }
        ));
    }
    for w in workers {
        w.join().unwrap();
    }
    let Response::Snapshots { snapshots } = handle
        .call(&Request::SnapshotList { volume: None })
        .unwrap()
    else {
        panic!("a list")
    };
    assert_eq!(snapshots.len(), 9);
    assert_eq!(
        handle.call(&Request::DaemonStop {}).unwrap(),
        Response::Ok {}
    );
    thread.join().unwrap();
    // The stop was clean.
    let mut d = Daemon::new(config());
    match ok(&mut d, &boot_req(&device)) {
        Response::Pool(p) => assert!(p.last_boot.starts_with("clean boot"), "{}", p.last_boot),
        other => panic!("{other:?}"),
    }
    ok(&mut d, &Request::PoolStop {});
}

/// When the last handle goes (the control server ended), the engine thread
/// stops the pool cleanly.
#[test]
fn dropping_every_handle_stops_the_pool_cleanly() {
    let scratch = Scratch::new("drop-handle");
    let device = scratch.path("pool.img");
    {
        let mut d = Daemon::new(config());
        ok(&mut d, &format_req(&device, false));
    }
    let mut cfg = config();
    cfg.device = Some(PathBuf::from(&device));
    cfg.arena = ArenaKind::Heap;
    cfg.arena_bytes = ARENA_BYTES;
    cfg.cp_interval_secs = 1;
    let (handle, thread) = Daemon::spawn(cfg, Arc::new(AtomicBool::new(false))).unwrap();
    handle.call(&create("data", 4 << 20)).unwrap();
    let w = handle
        .call(&Request::IoWrite {
            volume: "data".into(),
            lba: 3,
            data_hex: encode_hex(&vec![0x77; BLOCK_SIZE]),
        })
        .unwrap();
    assert!(matches!(w, Response::Io(ref r) if r.status == "ok"));
    drop(handle);
    thread.join().unwrap();
    let mut d = Daemon::new(config());
    match ok(&mut d, &boot_req(&device)) {
        Response::Pool(p) => assert!(p.last_boot.starts_with("clean boot"), "{}", p.last_boot),
        other => panic!("{other:?}"),
    }
    assert_eq!(
        decode_hex(&read(&mut d, "data", 3, 1).data_hex).unwrap(),
        vec![0x77; BLOCK_SIZE],
        "the acknowledged write survived the stop"
    );
    ok(&mut d, &Request::PoolStop {});
}

/// Malformed input over TCP: an empty frame and a frame of the wrong shape
/// are answered with `BadRequest` and the connection goes on; a length
/// beyond the bound or a stream cut inside a frame ends the connection;
/// the daemon keeps serving new connections either way.
#[test]
fn malformed_frames_are_answered_or_end_the_connection_and_the_daemon_serves_on() {
    let stop = Arc::new(AtomicBool::new(false));
    let server = Server::bind(&Endpoint::parse("tcp:127.0.0.1:0").unwrap()).unwrap();
    let endpoint = server.endpoint().clone();
    let Endpoint::Tcp(addr) = endpoint.clone() else {
        panic!("a TCP endpoint")
    };
    let (handle, engine) = Daemon::spawn(config(), Arc::clone(&stop)).unwrap();
    let serving = {
        let handle = handle.clone();
        let stop = Arc::clone(&stop);
        std::thread::spawn(move || serve(server, &handle, &stop))
    };
    let answer = |raw: &mut TcpStream, body: &[u8]| -> Reply {
        raw.write_all(&(body.len() as u32).to_le_bytes()).unwrap();
        raw.write_all(body).unwrap();
        read_frame::<Reply, _>(raw).unwrap().expect("a reply")
    };
    let mut raw = TcpStream::connect(addr).unwrap();
    raw.set_read_timeout(Some(Duration::from_secs(30))).unwrap();
    for body in [
        &b""[..],
        &b"null"[..],
        &br#"{"op":"volume_create","name":5,"size_bytes":1}"#[..],
        &br#"{"op":"volume_create","name":"x"}"#[..],
        &br#"{"op":"io_read","volume":"x","lba":-1,"blocks":1}"#[..],
        &br#"{"op":"pool_boot","device":"x"}"#[..],
        &[0xff, 0xfe, 0x00][..],
    ] {
        let e = answer(&mut raw, body).unwrap_err();
        assert_eq!(
            e.code,
            ErrorCode::BadRequest,
            "{:?}",
            String::from_utf8_lossy(body)
        );
    }
    assert!(matches!(
        answer(&mut raw, br#"{"op":"ping"}"#).unwrap(),
        Response::Pong { .. }
    ));
    // A length beyond the bound: the daemon closes the connection.
    raw.write_all(&(MAX_FRAME_BYTES + 1).to_le_bytes()).unwrap();
    let closed = read_frame::<Reply, _>(&mut raw);
    assert!(
        matches!(closed, Ok(None) | Err(_)),
        "the connection ends: {closed:?}"
    );
    // A frame cut short: the connection ends, nothing else.
    let mut cut = TcpStream::connect(addr).unwrap();
    cut.write_all(&100u32.to_le_bytes()).unwrap();
    cut.write_all(b"{\"op\":").unwrap();
    drop(cut);
    // New clients are served.
    let mut client = Client::connect(&endpoint, Some(Duration::from_secs(30))).unwrap();
    assert!(matches!(client.hello().unwrap(), Response::Pong { .. }));
    assert_eq!(
        client.call(&Request::DaemonStop {}).unwrap(),
        Response::Ok {}
    );
    engine.join().unwrap();
    serving.join().unwrap().unwrap();
}

/// Names at their bounds: 255 bytes of UTF-8 are a name, 256 are not;
/// control characters and `/` are refused; the same snapshot name on two
/// volumes is fine; every name survives a clean restart.
#[test]
fn names_at_their_bounds_are_kept_or_refused_and_survive_a_restart() {
    let scratch = Scratch::new("names");
    let (mut d, device) = booted(&scratch);
    let long = "n".repeat(255);
    let unicode = "données-é-日本";
    volume(&mut d, &create(&long, 1 << 20));
    volume(&mut d, &create(unicode, 1 << 20));
    for bad in [
        "n".repeat(256),
        "a/b".to_string(),
        "tab\there".to_string(),
        String::new(),
    ] {
        let e = d.handle(&create(&bad, 1 << 20)).unwrap_err();
        assert_eq!(e.code, ErrorCode::BadRequest, "{bad:?}");
    }
    let e = d.handle(&create(unicode, 1 << 20)).unwrap_err();
    assert_eq!(e.code, ErrorCode::BadRequest, "a second volume of one name");
    snapshot(&mut d, &snap(&long, "nightly"));
    snapshot(&mut d, &snap(unicode, "nightly"));
    snapshot(&mut d, &snap(unicode, &"s".repeat(255)));
    assert!(d.handle(&snap(unicode, "nightly")).is_err());
    assert!(matches!(
        ok(&mut d, &Request::PoolStop {}),
        Response::Committed { .. }
    ));
    ok(&mut d, &boot_req(&device));
    let mut names = snapshot_names(&mut d, unicode);
    names.sort();
    assert_eq!(names, vec!["nightly".to_string(), "s".repeat(255)]);
    assert_eq!(snapshot_names(&mut d, &long), vec!["nightly".to_string()]);
    ok(&mut d, &Request::PoolStop {});
}

/// I/O requests at the edges of a volume and of the protocol: a read at
/// the last block works and one past it answers `out_of_range` (an LBA
/// near `u64::MAX` included), a zero-block request is refused, data that
/// is not whole blocks is refused.
#[test]
fn io_requests_at_the_edges_are_answered_not_crashed() {
    let scratch = Scratch::new("io-edges");
    let (mut d, _) = booted(&scratch);
    volume(&mut d, &create("data", 1 << 20)); // 256 blocks
    assert_eq!(write_fill(&mut d, "data", 255, 0x0f, 1).status, "ok");
    assert_eq!(read(&mut d, "data", 255, 1).status, "ok");
    assert_eq!(read(&mut d, "data", 256, 1).status, "out_of_range");
    assert_eq!(read(&mut d, "data", u64::MAX, 1).status, "out_of_range");
    assert_eq!(read(&mut d, "data", u64::MAX - 1, 2).status, "out_of_range");
    assert_eq!(
        write_fill(&mut d, "data", 255, 0x0f, 2).status,
        "out_of_range"
    );
    let punch = io(
        &mut d,
        &Request::IoPunch {
            volume: "data".into(),
            lba: u64::MAX,
            blocks: 1,
        },
    );
    assert_eq!(punch.status, "out_of_range");
    for req in [
        Request::IoRead {
            volume: "data".into(),
            lba: 0,
            blocks: 0,
        },
        Request::IoPunch {
            volume: "data".into(),
            lba: 0,
            blocks: 0,
        },
        Request::IoWrite {
            volume: "data".into(),
            lba: 0,
            data_hex: String::new(),
        },
        Request::IoWrite {
            volume: "data".into(),
            lba: 0,
            data_hex: "zz".repeat(BLOCK_SIZE),
        },
    ] {
        let e: ControlError = d.handle(&req).unwrap_err();
        assert_eq!(e.code, ErrorCode::BadRequest, "{req:?}");
    }
    let e = d
        .handle(&Request::VolumeResize {
            name: "data".into(),
            size_bytes: 0,
        })
        .unwrap_err();
    assert_eq!(e.code, ErrorCode::BadRequest);
    ok(&mut d, &Request::PoolStop {});
}
