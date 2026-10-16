//! Step 6B: the pool verification through `dbladed`: a job the daemon
//! advances one slice per engine tick, started, followed and cancelled
//! through the control protocol while the data path keeps answering; a
//! flipped byte in the pool file is found at its volume and LBA, and the
//! pool status carries the finding.

use std::path::PathBuf;
use std::time::Duration;

use otap_control::{
    encode_hex, ArenaKind, ControlError, Endpoint, ErrorCode, FindingSeverity, PoolReport, Request,
    Response, VerifyReport, VerifyState, VerifyVerdict, BLOCK_SIZE,
};
use otap_daemon::{Daemon, DaemonConfig};

/// A 128 MiB file: the small-file geometry.
const POOL_BYTES: u64 = 128 << 20;
/// The smallest arena the daemon accepts.
const ARENA_BYTES: u64 = 64 << 20;

struct Scratch {
    dir: PathBuf,
}

impl Scratch {
    fn new(name: &str) -> Self {
        let dir =
            std::env::temp_dir().join(format!("otap-daemon-verify-{}-{name}", std::process::id()));
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

fn daemon() -> Daemon {
    Daemon::new(DaemonConfig {
        control: Endpoint::parse("tcp:127.0.0.1:0").unwrap(),
        ublk: false,
        op_timeout: Duration::from_secs(60),
        ..DaemonConfig::default()
    })
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

fn booted(scratch: &Scratch) -> (Daemon, String) {
    let mut d = daemon();
    let device = scratch.path("pool.img");
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
    ok(
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
            cp_interval_secs: 1,
        },
    );
    ok(
        &mut d,
        &Request::VolumeCreate {
            name: "data".into(),
            size_bytes: 16 << 20,
        },
    );
    (d, device)
}

fn write(d: &mut Daemon, lba: u64, data: &[u8]) {
    match ok(
        d,
        &Request::IoWrite {
            volume: "data".into(),
            lba,
            data_hex: encode_hex(data),
        },
    ) {
        Response::Io(r) => assert_eq!(r.status, "ok"),
        other => panic!("{other:?}"),
    }
}

fn verify_report(reply: Response) -> VerifyReport {
    match reply {
        Response::Verify(r) => r,
        other => panic!("not a verification report: {other:?}"),
    }
}

/// Starts a verification and follows it to its end, ticking the daemon as
/// its engine thread would between requests.
fn run_verification(d: &mut Daemon) -> VerifyReport {
    let mut report = verify_report(ok(d, &Request::PoolVerify {}));
    for _ in 0..100_000 {
        if report.state != VerifyState::Running {
            return report;
        }
        d.tick();
        report = verify_report(ok(d, &Request::PoolVerifyStatus {}));
    }
    panic!("the verification never ended: {report:?}");
}

fn pool(d: &mut Daemon) -> PoolReport {
    match ok(d, &Request::PoolStatus) {
        Response::Pool(p) => p,
        other => panic!("{other:?}"),
    }
}

#[test]
fn pool_verify_runs_as_a_job_between_requests_and_attests_a_healthy_pool() {
    let scratch = Scratch::new("healthy");
    let (mut d, _) = booted(&scratch);
    // Nothing ran yet.
    assert_eq!(
        err(&mut d, &Request::PoolVerifyStatus {}).code,
        ErrorCode::NotFound
    );
    assert_eq!(
        err(&mut d, &Request::PoolVerifyCancel {}).code,
        ErrorCode::NotFound
    );
    for lba in 0..8u64 {
        write(&mut d, lba * 4, &vec![lba as u8 + 1; 4 * BLOCK_SIZE]);
    }
    ok(
        &mut d,
        &Request::SnapshotCreate {
            volume: "data".into(),
            name: "before".into(),
        },
    );
    write(&mut d, 0, &vec![0xEE; 8 * BLOCK_SIZE]);
    ok(&mut d, &Request::PoolSync);
    // Started: answered at once with the job's report, the view taken.
    let first = verify_report(ok(&mut d, &Request::PoolVerify {}));
    assert_eq!(first.state, VerifyState::Running);
    assert!(first.cp_seq > 0);
    // Asked again while it runs: the same job.
    let again = verify_report(ok(&mut d, &Request::PoolVerify {}));
    assert_eq!(
        (again.state, again.cp_seq),
        (VerifyState::Running, first.cp_seq)
    );
    // Requests are answered while it runs: a write and a sync.
    write(&mut d, 40, &vec![0x11; BLOCK_SIZE]);
    ok(&mut d, &Request::PoolSync);
    let mut report = verify_report(ok(&mut d, &Request::PoolVerifyStatus {}));
    assert_eq!(
        report.cp_seq, first.cp_seq,
        "the job attests the CP it began at"
    );
    for _ in 0..100_000 {
        if report.state != VerifyState::Running {
            break;
        }
        d.tick();
        report = verify_report(ok(&mut d, &Request::PoolVerifyStatus {}));
    }
    assert_eq!(report.state, VerifyState::Done, "{report:?}");
    assert_eq!(report.verdict, Some(VerifyVerdict::Clean), "{report:?}");
    assert!(report.blocks > 0 && report.map_pages > 0 && report.segments > 0);
    assert_eq!(report.superblock_slots, 4);
    assert!(report.bytes_read > 0);
    assert!(report.findings.is_empty());
    assert_eq!(report.phase, "finished");
    // A second verification of a later CP.
    write(&mut d, 44, &vec![0x22; BLOCK_SIZE]);
    ok(&mut d, &Request::PoolSync);
    let later = run_verification(&mut d);
    assert_eq!(later.verdict, Some(VerifyVerdict::Clean), "{later:?}");
    assert!(later.cp_seq > report.cp_seq);
    assert!(!pool(&mut d)
        .warnings
        .iter()
        .any(|w| w.contains("verification")));
}

#[test]
fn pool_verify_finds_a_flipped_byte_and_the_pool_status_carries_it() {
    let scratch = Scratch::new("flipped");
    let (mut d, device) = booted(&scratch);
    // A block no other write repeats: found again in the file.
    let mut marked = vec![0u8; BLOCK_SIZE];
    for (i, b) in marked.iter_mut().enumerate() {
        *b = (i * 7 + 13) as u8;
    }
    marked[..16].copy_from_slice(b"step6b-verify-ok");
    write(&mut d, 9, &marked);
    ok(&mut d, &Request::PoolSync);
    let file = std::fs::read(&device).unwrap();
    let at = file
        .windows(16)
        .position(|w| w == b"step6b-verify-ok")
        .expect("the block is in the file");
    assert_eq!(at % BLOCK_SIZE, 0);
    // One byte of the block, flipped behind the daemon's back.
    {
        use std::io::{Seek, SeekFrom, Write};
        let mut f = std::fs::OpenOptions::new()
            .write(true)
            .open(&device)
            .unwrap();
        f.seek(SeekFrom::Start(at as u64 + 100)).unwrap();
        f.write_all(&[marked[100] ^ 0xFF]).unwrap();
        f.sync_all().unwrap();
    }
    let report = run_verification(&mut d);
    assert_eq!(report.verdict, Some(VerifyVerdict::Damaged), "{report:?}");
    let f = report
        .findings
        .iter()
        .find(|f| f.kind == "block_crc")
        .expect("a block_crc finding");
    assert_eq!(f.severity, FindingSeverity::Error);
    assert_eq!((f.volume_index, f.lba), (Some(0), Some(9)));
    assert_eq!(f.pba, Some((at / BLOCK_SIZE) as u64));
    let status = pool(&mut d);
    assert!(
        status
            .warnings
            .iter()
            .any(|w| w.contains("verification") && w.contains("block_crc")),
        "{:?}",
        status.warnings
    );
}

#[test]
fn pool_verify_can_be_cancelled_and_needs_a_pool() {
    let mut d = daemon();
    for req in [
        Request::PoolVerify {},
        Request::PoolVerifyStatus {},
        Request::PoolVerifyCancel {},
    ] {
        assert_eq!(err(&mut d, &req).code, ErrorCode::NoPool, "{req:?}");
    }
    let scratch = Scratch::new("cancel");
    let (mut d, _) = booted(&scratch);
    for lba in 0..64u64 {
        write(&mut d, lba * 8, &vec![lba as u8; 8 * BLOCK_SIZE]);
    }
    ok(&mut d, &Request::PoolSync);
    let started = verify_report(ok(&mut d, &Request::PoolVerify {}));
    if started.state == VerifyState::Running {
        let cancelled = verify_report(ok(&mut d, &Request::PoolVerifyCancel {}));
        assert_eq!(cancelled.state, VerifyState::Cancelled);
        assert_eq!(cancelled.verdict, Some(VerifyVerdict::Incomplete));
        assert!(cancelled.reason.is_some());
        let status = verify_report(ok(&mut d, &Request::PoolVerifyStatus {}));
        assert_eq!(status.state, VerifyState::Cancelled);
    }
    // A new one runs to its end after a cancel.
    let report = run_verification(&mut d);
    assert_eq!(report.verdict, Some(VerifyVerdict::Clean), "{report:?}");
}
