//! The Step 6 adversarial review (`docs/reviews/2026-10-16-step6-qa-adversarial.md`):
//! the replication findings that need the daemon, between two in-process
//! `dbladed` daemons over 127.0.0.1. Each test reproduced its finding
//! before the fix and pins the fixed behaviour. The helpers are those of
//! `step6c_replication_daemon_tests.rs`, copied so this file stands alone.

use std::path::PathBuf;
use std::time::{Duration, Instant};

use otap_control::{
    decode_hex, encode_hex, ArenaKind, ControlError, Endpoint, ReplicationReport, ReplicationState,
    Request, Response, BLOCK_SIZE,
};
use otap_daemon::{Daemon, DaemonConfig, TcpConfig, TcpReplicationTransport};

const POOL_BYTES: u64 = 128 << 20;
const ARENA_BYTES: u64 = 64 << 20;
const DEADLINE: Duration = Duration::from_secs(120);

struct Scratch {
    dir: PathBuf,
}

impl Scratch {
    fn new(name: &str) -> Self {
        let dir =
            std::env::temp_dir().join(format!("otap-daemon-repl-{}-{name}", std::process::id()));
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

/// A daemon with its own state directory and a booted pool of its own;
/// `listen` binds a replication port on 127.0.0.1.
fn daemon(scratch: &Scratch, name: &str, listen: bool) -> (Daemon, String, Option<String>) {
    let state = scratch.dir.join(format!("{name}-state"));
    let mut d = Daemon::new(DaemonConfig {
        control: Endpoint::parse("tcp:127.0.0.1:0").unwrap(),
        ublk: false,
        op_timeout: Duration::from_secs(60),
        state_dir: state,
        ..DaemonConfig::default()
    });
    let addr = listen.then(|| {
        let l = TcpReplicationTransport::new(TcpConfig::default())
            .listen("127.0.0.1:0")
            .expect("the replication port binds");
        let addr = l.local_addr().to_string();
        d.set_replication_listener(l);
        addr
    });
    let device = scratch.path(&format!("{name}.img"));
    ok(
        &mut d,
        &Request::PoolFormat {
            device: device.clone(),
            size_bytes: Some(POOL_BYTES),
            media: None,
            stripe_bytes: None,
            segment_bytes: None,
            flush_opaque: None,
            strict_flush: false,
            force: false,
        },
    );
    boot(&mut d, &device);
    (d, device, addr)
}

fn boot(d: &mut Daemon, device: &str) {
    ok(
        d,
        &Request::PoolBoot {
            device: device.to_string(),
            media: None,
            stripe_bytes: None,
            segment_bytes: None,
            flush_opaque: None,
            strict_flush: false,
            arena_bytes: ARENA_BYTES,
            arena: ArenaKind::Heap,
            cp_interval_secs: 1,
        },
    );
}

/// The block pattern written at `lba` in round `round`.
fn pattern(lba: u64, round: u8) -> Vec<u8> {
    let mut b = vec![0u8; BLOCK_SIZE];
    for (i, byte) in b.iter_mut().enumerate() {
        *byte = (lba as u8) ^ (i as u8).wrapping_mul(31) ^ round;
    }
    b
}

fn write_run(d: &mut Daemon, volume: &str, lba: u64, blocks: u64, round: u8) {
    let mut data = Vec::with_capacity(blocks as usize * BLOCK_SIZE);
    for k in 0..blocks {
        data.extend_from_slice(&pattern(lba + k, round));
    }
    match ok(
        d,
        &Request::IoWrite {
            volume: volume.into(),
            lba,
            data_hex: encode_hex(&data),
        },
    ) {
        Response::Io(r) => assert_eq!(r.status, "ok"),
        other => panic!("{other:?}"),
    }
}

fn read_run(d: &mut Daemon, volume: &str, lba: u64, blocks: u32) -> Vec<u8> {
    match ok(
        d,
        &Request::IoRead {
            volume: volume.into(),
            lba,
            blocks,
        },
    ) {
        Response::Io(r) => {
            assert_eq!(r.status, "ok", "read {volume} {lba}+{blocks}");
            decode_hex(&r.data_hex).expect("hex")
        }
        other => panic!("{other:?}"),
    }
}

fn job(reply: Response) -> ReplicationReport {
    match reply {
        Response::Replication(r) => r,
        other => panic!("not a job: {other:?}"),
    }
}

/// Ticks both daemons, as their engine threads would, until `done`.
fn run(a: &mut Daemon, b: &mut Daemon, mut done: impl FnMut(&mut Daemon, &mut Daemon) -> bool) {
    let start = Instant::now();
    while start.elapsed() < DEADLINE {
        if done(a, b) {
            return;
        }
        let busy = a.tick() | b.tick();
        if !busy {
            std::thread::sleep(Duration::from_micros(300));
        }
    }
    panic!(
        "timed out: source {:?}; sink {:?}",
        a.handle(&Request::ReplicateStatus { id: None }),
        b.handle(&Request::ReplicateStatus { id: None })
    );
}

fn status(d: &mut Daemon, id: u64) -> ReplicationReport {
    job(ok(d, &Request::ReplicateStatus { id: Some(id) }))
}

fn push(d: &mut Daemon, snapshot: &str, to: &str) -> ReplicationReport {
    job(ok(
        d,
        &Request::ReplicatePush {
            volume: "data".into(),
            snapshot: snapshot.into(),
            to: to.into(),
            destination: Some("mirror".into()),
        },
    ))
}

fn wait_done(src: &mut Daemon, dst: &mut Daemon, id: u64) -> ReplicationReport {
    run(src, dst, |s, _| {
        status(s, id).state != ReplicationState::Running
    });
    let report = status(src, id);
    assert_eq!(report.state, ReplicationState::Done, "{report:#?}");
    report
}

/// A source volume "data" with one round of writes and snapshot `s1`.
fn source_with_s1(src: &mut Daemon) {
    ok(
        src,
        &Request::VolumeCreate {
            name: "data".into(),
            size_bytes: 16 << 20,
        },
    );
    for lba in (0..2048u64).step_by(256) {
        write_run(src, "data", lba, 256, 1);
    }
    ok(
        src,
        &Request::SnapshotCreate {
            volume: "data".into(),
            name: "s1".into(),
        },
    );
}

/// D7: a sink record outlived the deletion of its mirror (records are keyed
/// by volume index), and the next volume created at that index was taken
/// for the mirror at the next boot: marked a replica, refused online, and
/// `replicate release` advised deleting it. The daemon now forgets the
/// record when it deletes the volume.
#[test]
fn d7_a_deleted_mirrors_record_never_attaches_to_the_next_volume() {
    let scratch = Scratch::new("review-d7");
    let (mut src, _, _) = daemon(&scratch, "src", false);
    let (mut dst, dst_device, addr) = daemon(&scratch, "dst", true);
    let addr = addr.expect("the sink listens");
    // Enough data that cancelling after the first landing leaves part of
    // the first transfer, and no destination snapshot, on the sink.
    ok(
        &mut src,
        &Request::VolumeCreate {
            name: "data".into(),
            size_bytes: 64 << 20,
        },
    );
    for lba in (0..16384u64).step_by(256) {
        write_run(&mut src, "data", lba, 256, 1);
    }
    ok(
        &mut src,
        &Request::SnapshotCreate {
            volume: "data".into(),
            name: "s1".into(),
        },
    );
    let started = push(&mut src, "s1", &addr);
    run(&mut src, &mut dst, |s, _| {
        status(s, started.id).chunks_landed >= 1
    });
    ok(&mut src, &Request::ReplicateCancel { id: started.id });
    let jobs = match ok(&mut dst, &Request::ReplicateStatus { id: None }) {
        Response::Replications { jobs } => jobs,
        other => panic!("{other:?}"),
    };
    for j in jobs.iter().filter(|j| j.state == ReplicationState::Running) {
        ok(&mut dst, &Request::ReplicateCancel { id: j.id });
    }
    // The operator deletes the partial mirror (after a sync, when chunks
    // were still in the arena).
    let mut deleted = false;
    for _ in 0..50 {
        match dst.handle(&Request::VolumeDelete {
            name: "mirror".into(),
        }) {
            Ok(_) => {
                deleted = true;
                break;
            }
            Err(e) => {
                assert!(
                    e.message.contains("waiting") || e.message.contains("not yet in a"),
                    "{e}"
                );
                ok(&mut dst, &Request::PoolSync {});
            }
        }
    }
    assert!(deleted, "the partial mirror is deleted");
    ok(
        &mut dst,
        &Request::VolumeCreate {
            name: "db".into(),
            size_bytes: 16 << 20,
        },
    );
    write_run(&mut dst, "db", 0, 8, 9);
    ok(
        &mut dst,
        &Request::VolumeOnline {
            name: "db".into(),
            online: false,
        },
    );
    ok(&mut dst, &Request::PoolStop {});
    boot(&mut dst, &dst_device);
    match dst.handle(&Request::VolumeOnline {
        name: "db".into(),
        online: true,
    }) {
        Ok(_) => {}
        Err(e) => panic!("the operator's new volume was taken for the old mirror: {e}"),
    }
    let back = read_run(&mut dst, "db", 0, 8);
    let mut want = Vec::new();
    for lba in 0..8 {
        want.extend_from_slice(&pattern(lba, 9));
    }
    assert!(back == want, "the operator's data reads back");
}

/// D4 (its second trigger): `snapshot rollback` and `volume resize` of a
/// mirror went straight to the engine, under the sink: a rollback in the
/// middle of a delta wiped chunks the sink's record still called landed.
/// Both are refused while the volume is a mirror.
#[test]
fn d4_an_operator_cannot_roll_back_or_resize_a_mirror() {
    let scratch = Scratch::new("review-d4");
    let (mut src, _, _) = daemon(&scratch, "src", false);
    let (mut dst, _, addr) = daemon(&scratch, "dst", true);
    let addr = addr.expect("the sink listens");
    source_with_s1(&mut src);
    let started = push(&mut src, "s1", &addr);
    wait_done(&mut src, &mut dst, started.id);
    let e = err(
        &mut dst,
        &Request::SnapshotRollback {
            volume: "mirror".into(),
            name: "s1".into(),
            delete_newer: false,
        },
    );
    assert!(e.message.contains("replicate release"), "{e}");
    let e = err(
        &mut dst,
        &Request::VolumeResize {
            name: "mirror".into(),
            size_bytes: 32 << 20,
        },
    );
    assert!(e.message.contains("replicate release"), "{e}");
}
