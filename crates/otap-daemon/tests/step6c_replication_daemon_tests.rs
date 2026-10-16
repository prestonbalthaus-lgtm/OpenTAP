//! Step 6C: replication between two in-process `dbladed` daemons over
//! 127.0.0.1, driven through the control protocol: a push, its status on
//! both ends, the cursors, the mirror kept offline, an incremental delta,
//! a sink pool restarted in the middle of a delta (its cursor kept in the
//! state directory), cancel, release, and a pull.

use std::path::PathBuf;
use std::time::{Duration, Instant};

use otap_control::{
    decode_hex, encode_hex, ArenaKind, ControlError, Endpoint, ErrorCode, ReplicationCursorReport,
    ReplicationReport, ReplicationState, Request, Response, BLOCK_SIZE,
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

fn cursor(reply: Response) -> ReplicationCursorReport {
    match reply {
        Response::ReplicationCursor(c) => c,
        other => panic!("not a cursor: {other:?}"),
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

#[test]
fn a_push_between_two_daemons_lands_a_mirror_and_a_delta_follows() {
    let scratch = Scratch::new("push");
    let (mut src, _, _) = daemon(&scratch, "src", false);
    let (mut dst, dst_device, addr) = daemon(&scratch, "dst", true);
    let addr = addr.expect("the sink listens");
    ok(
        &mut src,
        &Request::VolumeCreate {
            name: "data".into(),
            size_bytes: 16 << 20,
        },
    );
    for lba in (0..2048u64).step_by(256) {
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
    assert_eq!(started.role, "source");
    assert_eq!(started.mode, "push");
    let done = wait_done(&mut src, &mut dst, started.id);
    assert!(done.chunks >= 8, "{done:#?}");
    assert_eq!(done.chunks_landed, done.chunks);

    // The sink's view: an inbound job done, a mirror offline, a cursor.
    let jobs = match ok(&mut dst, &Request::ReplicateStatus { id: None }) {
        Response::Replications { jobs } => jobs,
        other => panic!("{other:?}"),
    };
    let inbound = jobs.last().expect("the sink's job");
    assert_eq!(inbound.mode, "inbound push");
    assert_eq!(inbound.state, ReplicationState::Done);
    assert_eq!(inbound.duplicate_chunks, 0);
    assert_eq!(inbound.digest_verified, Some(true));
    match ok(
        &mut dst,
        &Request::VolumeShow {
            name: "mirror".into(),
        },
    ) {
        Response::Volume(v) => assert!(v.offline && !v.serving, "{v:?}"),
        other => panic!("{other:?}"),
    }
    let c = cursor(ok(
        &mut dst,
        &Request::ReplicateCursor {
            volume: "mirror".into(),
        },
    ));
    assert_eq!(c.role, "sink");
    assert!(
        c.replica && c.persisted && !c.dirty && c.landing.is_none(),
        "{c:?}"
    );
    assert_eq!(c.base_name.as_deref(), Some("s1"));
    let src_cursor = cursor(ok(
        &mut src,
        &Request::ReplicateCursor {
            volume: "data".into(),
        },
    ));
    assert_eq!(src_cursor.role, "source");
    assert_eq!(src_cursor.base_name.as_deref(), Some("s1"));
    assert_eq!(src_cursor.base_uuid, c.base_uuid);
    // The mirror refuses to come online while it is a mirror.
    let e = err(
        &mut dst,
        &Request::VolumeOnline {
            name: "mirror".into(),
            online: true,
        },
    );
    assert!(e.message.contains("replication destination"), "{e}");
    // The source's old base cannot be deleted: it is the replication base.
    let e = err(
        &mut src,
        &Request::SnapshotDelete {
            volume: "data".into(),
            name: "s1".into(),
        },
    );
    assert!(e.message.contains("pinned"), "{e}");

    // An incremental delta, with the sink pool restarted in its middle:
    // the cursor in the state directory resumes it.
    for lba in (0..4096u64).step_by(256) {
        write_run(&mut src, "data", lba, 256, 2);
    }
    ok(
        &mut src,
        &Request::SnapshotCreate {
            volume: "data".into(),
            name: "s2".into(),
        },
    );
    let second = push(&mut src, "s2", &addr);
    run(&mut src, &mut dst, |_, d| {
        match d.handle(&Request::ReplicateCursor {
            volume: "mirror".into(),
        }) {
            Ok(Response::ReplicationCursor(c)) => c.landing.is_some_and(|l| l.landed >= 3),
            _ => false,
        }
    });
    let records: Vec<_> = std::fs::read_dir(scratch.dir.join("dst-state"))
        .expect("the state directory")
        .flatten()
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .filter(|n| n.starts_with("repl-"))
        .collect();
    assert_eq!(records.len(), 1, "one cursor record: {records:?}");
    ok(&mut dst, &Request::PoolStop);
    boot(&mut dst, &dst_device);
    let second = wait_done(&mut src, &mut dst, second.id);
    assert!(second.retries >= 1, "the restart was met: {second:#?}");
    assert!(second.resumed_chunks >= 3, "{second:#?}");
    let jobs = match ok(&mut dst, &Request::ReplicateStatus { id: None }) {
        Response::Replications { jobs } => jobs,
        other => panic!("{other:?}"),
    };
    let resumed = jobs.last().expect("the restarted sink's job");
    assert_eq!(resumed.duplicate_chunks, 0, "{resumed:#?}");
    assert!(resumed.chunks_landed < second.chunks, "{resumed:#?}");

    // Release: the mirror serves the second snapshot's content.
    let released = ok(
        &mut dst,
        &Request::ReplicateRelease {
            volume: "mirror".into(),
        },
    );
    assert!(matches!(released, Response::Volume(_)), "{released:?}");
    ok(
        &mut dst,
        &Request::VolumeOnline {
            name: "mirror".into(),
            online: true,
        },
    );
    for lba in (0..4096u64).step_by(256) {
        let want = read_run(&mut src, "data", lba, 256);
        let got = read_run(&mut dst, "mirror", lba, 256);
        assert!(want == got, "blocks {lba}..+256 differ after the release");
    }
    let c = cursor(ok(
        &mut dst,
        &Request::ReplicateCursor {
            volume: "mirror".into(),
        },
    ));
    assert!(!c.replica && !c.persisted, "{c:?}");
}

#[test]
fn a_pull_cancel_and_the_refusals() {
    let scratch = Scratch::new("pull");
    let (mut src, _, src_addr) = daemon(&scratch, "src", true);
    let (mut dst, _, _) = daemon(&scratch, "dst", false);
    let src_addr = src_addr.expect("the source listens");
    ok(
        &mut src,
        &Request::VolumeCreate {
            name: "data".into(),
            size_bytes: 8 << 20,
        },
    );
    write_run(&mut src, "data", 0, 64, 3);
    ok(
        &mut src,
        &Request::SnapshotCreate {
            volume: "data".into(),
            name: "p1".into(),
        },
    );
    // Unknown names are refused by the source, and the job says why.
    let bad = job(ok(
        &mut dst,
        &Request::ReplicatePull {
            volume: "copy".into(),
            snapshot: "nope".into(),
            from: src_addr.clone(),
            source: Some("data".into()),
        },
    ));
    run(&mut src, &mut dst, |_, d| {
        status(d, bad.id).state != ReplicationState::Running
    });
    let bad = status(&mut dst, bad.id);
    assert_eq!(bad.state, ReplicationState::Failed);
    assert!(
        bad.last_error
            .as_deref()
            .unwrap_or("")
            .contains("no snapshot named"),
        "{bad:#?}"
    );
    // The pull itself.
    let pull = job(ok(
        &mut dst,
        &Request::ReplicatePull {
            volume: "copy".into(),
            snapshot: "p1".into(),
            from: src_addr.clone(),
            source: Some("data".into()),
        },
    ));
    assert_eq!(pull.role, "sink");
    run(&mut src, &mut dst, |_, d| {
        status(d, pull.id).state != ReplicationState::Running
    });
    let pull = status(&mut dst, pull.id);
    assert_eq!(pull.state, ReplicationState::Done, "{pull:#?}");
    // The source pins the pulled snapshot once the sink committed it.
    run(&mut src, &mut dst, |s, _| {
        match s.handle(&Request::ReplicateCursor {
            volume: "data".into(),
        }) {
            Ok(Response::ReplicationCursor(c)) => c.base_name.as_deref() == Some("p1"),
            _ => false,
        }
    });
    // Requests that cannot be served.
    let e = err(
        &mut src,
        &Request::ReplicatePush {
            volume: "data".into(),
            snapshot: "p1".into(),
            to: "192.0.2.1:7422".into(),
            destination: None,
        },
    );
    assert_eq!(e.code, ErrorCode::BadRequest);
    assert!(e.message.contains("--allow-remote-replication"), "{e}");
    let e = err(
        &mut src,
        &Request::ReplicatePush {
            volume: "nothing".into(),
            snapshot: "p1".into(),
            to: src_addr.clone(),
            destination: None,
        },
    );
    assert_eq!(e.code, ErrorCode::NotFound);
    let e = err(&mut src, &Request::ReplicateStatus { id: Some(999) });
    assert_eq!(e.code, ErrorCode::NotFound);
    let e = err(&mut src, &Request::ReplicateCancel { id: 999 });
    assert_eq!(e.code, ErrorCode::NotFound);
    // A push to a port nobody listens on keeps retrying until cancelled.
    let closed = {
        let l = std::net::TcpListener::bind("127.0.0.1:0").expect("binds");
        l.local_addr().expect("addr").to_string()
    };
    let doomed = push(&mut src, "p1", &closed);
    run(&mut src, &mut dst, |s, _| status(s, doomed.id).retries >= 2);
    let cancelled = job(ok(&mut src, &Request::ReplicateCancel { id: doomed.id }));
    assert_eq!(cancelled.state, ReplicationState::Cancelled);
    // A daemon with no pool refuses inbound transfers.
    let mut lonely = Daemon::new(DaemonConfig {
        control: Endpoint::parse("tcp:127.0.0.1:0").unwrap(),
        ublk: false,
        state_dir: scratch.dir.join("lonely-state"),
        ..DaemonConfig::default()
    });
    let l = TcpReplicationTransport::new(TcpConfig::default())
        .listen("127.0.0.1:0")
        .expect("binds");
    let lonely_addr = l.local_addr().to_string();
    lonely.set_replication_listener(l);
    let refused = push(&mut src, "p1", &lonely_addr);
    run(&mut src, &mut lonely, |s, _| {
        status(s, refused.id).state != ReplicationState::Running
    });
    let refused = status(&mut src, refused.id);
    assert_eq!(refused.state, ReplicationState::Failed);
    assert!(
        refused
            .last_error
            .as_deref()
            .unwrap_or("")
            .contains("no pool"),
        "{refused:#?}"
    );
}
