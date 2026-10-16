//! Step 6B: `dbladed`'s ublk devices across a `kill -9`, over the
//! in-memory 6.12 driver ([`MockKernel`]): the stamp and the binding record
//! of every device, the re-attach of the same `/dev/ublkbN` by a restarted
//! daemon before it serves (with the requests the dead one held delivered
//! again), a daemon killed between `START_USER_RECOVERY` and
//! `END_USER_RECOVERY` and restarted again, a device gone or not
//! re-attachable giving its volume a new one, the stale devices listed and
//! reaped only on request, foreign ones never touched, and the record's own
//! format.
//!
//! A daemon here is an in-process [`Daemon`] whose ublk control plane is a
//! [`MockKernel::process`]; dropping it without `PoolStop` is its kill (its
//! heap arena dies with it, so the boot after it is dirty, as
//! `daemon_tests::a_dirty_restart_holds_the_volume_under_nvfail_until_it_is_cleared`
//! shows; the writes these tests check are committed first).

use std::panic::{catch_unwind, AssertUnwindSafe};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use otap_control::{
    decode_hex, ArenaKind, ControlError, DeviceReport, DeviceState, Endpoint, ErrorCode,
    IoResultReport, PoolReport, Request, Response, VolumeReport, BLOCK_SIZE,
};
use otap_daemon::bindings::{self, Binding, Bindings, BindingsError, StaleReason};
use otap_daemon::{Daemon, DaemonConfig, UblkBackend};
use otap_format::Uuid;
use otap_ublk::{
    device_stamp, pool_fingerprint, DevState, ForeignDevice, MockCmd, MockKernel, UblkHost, EIO,
};

const POOL_BYTES: u64 = 128 << 20;
const ARENA_BYTES: u64 = 64 << 20;
const VOL_BYTES: u64 = 16 << 20;

struct Scratch {
    dir: PathBuf,
}

impl Scratch {
    fn new(name: &str) -> Self {
        let dir = std::env::temp_dir().join(format!("otap-6b-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("a scratch directory");
        Self { dir }
    }

    fn pool(&self) -> String {
        self.dir.join("pool.img").display().to_string()
    }

    fn state(&self) -> PathBuf {
        self.dir.join("state")
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

fn config(kernel: &MockKernel, scratch: &Scratch) -> DaemonConfig {
    let k = kernel.clone();
    DaemonConfig {
        control: Endpoint::parse("tcp:127.0.0.1:0").unwrap(),
        ublk: true,
        ublk_backend: UblkBackend::Custom(Arc::new(move || {
            Box::new(k.process()) as Box<dyn UblkHost>
        })),
        state_dir: scratch.state(),
        recovery_timeout: Duration::from_secs(1),
        op_timeout: Duration::from_secs(60),
        ..DaemonConfig::default()
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

fn format(d: &mut Daemon, scratch: &Scratch) {
    ok(
        d,
        &Request::PoolFormat {
            device: scratch.pool(),
            size_bytes: Some(POOL_BYTES),
            media: None,
            stripe_bytes: None,
            segment_bytes: None,
            flush_opaque: None,
            strict_flush: false,
            force: false,
        },
    );
}

fn boot(d: &mut Daemon, scratch: &Scratch) -> PoolReport {
    match ok(
        d,
        &Request::PoolBoot {
            device: scratch.pool(),
            media: None,
            stripe_bytes: None,
            segment_bytes: None,
            flush_opaque: None,
            strict_flush: false,
            arena_bytes: ARENA_BYTES,
            arena: ArenaKind::Heap,
            cp_interval_secs: 1,
        },
    ) {
        Response::Pool(p) => p,
        other => panic!("{other:?}"),
    }
}

fn create(d: &mut Daemon, name: &str) -> VolumeReport {
    match ok(
        d,
        &Request::VolumeCreate {
            name: name.into(),
            size_bytes: VOL_BYTES,
        },
    ) {
        Response::Volume(v) => v,
        other => panic!("{other:?}"),
    }
}

fn show(d: &mut Daemon, name: &str) -> VolumeReport {
    match ok(d, &Request::VolumeShow { name: name.into() }) {
        Response::Volume(v) => v,
        other => panic!("{other:?}"),
    }
}

fn sync(d: &mut Daemon) {
    ok(d, &Request::PoolSync {});
}

/// Clears the hold a dirty boot over a lost heap arena put on `name`.
fn clear(d: &mut Daemon, name: &str) {
    let v = match ok(d, &Request::VolumeNvfailClear { name: name.into() }) {
        Response::Volume(v) => v,
        other => panic!("{other:?}"),
    };
    assert_eq!(v.nvfail, None);
}

fn io_read(d: &mut Daemon, volume: &str, lba: u64, blocks: u32) -> IoResultReport {
    match ok(
        d,
        &Request::IoRead {
            volume: volume.into(),
            lba,
            blocks,
        },
    ) {
        Response::Io(r) => r,
        other => panic!("{other:?}"),
    }
}

fn devices(d: &mut Daemon) -> Vec<DeviceReport> {
    match ok(d, &Request::DeviceList {}) {
        Response::Devices { devices } => devices,
        other => panic!("{other:?}"),
    }
}

fn reaped(d: &mut Daemon, dev_id: Option<u32>, stale: bool) -> Vec<DeviceReport> {
    match ok(d, &Request::DeviceReap { dev_id, stale }) {
        Response::Reaped { devices } => devices,
        other => panic!("{other:?}"),
    }
}

/// The device id of a volume from its report's `/dev/ublkbN`.
fn dev_of(v: &VolumeReport) -> u32 {
    v.device
        .as_deref()
        .and_then(|p| p.strip_prefix("/dev/ublkb"))
        .and_then(|n| n.parse().ok())
        .unwrap_or_else(|| panic!("volume {:?} has no device", v.name))
}

fn block(fill: u8) -> Vec<u8> {
    vec![fill; BLOCK_SIZE]
}

/// Ticks the daemon until the block layer's request `id` completes.
fn wait(d: &mut Daemon, kernel: &MockKernel, id: u64) -> i32 {
    let deadline = Instant::now() + Duration::from_secs(30);
    while Instant::now() < deadline {
        if let Some(done) = kernel.completion(id) {
            return done.result;
        }
        d.tick();
    }
    panic!("request {id} never completed");
}

fn agg_of(report: &PoolReport) -> Uuid {
    let hex: String = report.agg_id.chars().filter(|c| *c != '-').collect();
    let mut bytes = [0u8; 16];
    for (i, b) in bytes.iter_mut().enumerate() {
        *b = u8::from_str_radix(&hex[2 * i..2 * i + 2], 16).unwrap();
    }
    Uuid::from_bytes(bytes)
}

fn record(state: &Path, agg: &Uuid) -> Bindings {
    bindings::load(state, agg)
        .expect("a readable record")
        .unwrap_or_else(|| Bindings::new(*agg))
}

#[test]
fn a_volume_device_is_stamped_recorded_and_served() {
    let scratch = Scratch::new("stamp");
    let kernel = MockKernel::new();
    let mut d = Daemon::new(config(&kernel, &scratch));
    format(&mut d, &scratch);
    let report = boot(&mut d, &scratch);
    let agg = agg_of(&report);
    let v = create(&mut d, "a");
    let dev = dev_of(&v);
    let info = kernel.info(dev).expect("the device exists");
    assert_eq!(info.state, DevState::Live);
    assert_eq!(info.stamp, device_stamp(pool_fingerprint(&agg), v.index));
    assert!(info.reissues(), "created with UBLK_F_USER_RECOVERY_REISSUE");
    assert_eq!(info.size_blocks, Some(VOL_BYTES / BLOCK_SIZE as u64));
    assert_eq!(
        record(&scratch.state(), &agg).entries(),
        &[Binding::Bound {
            dev_id: dev,
            vol: v.index,
            size_blocks: VOL_BYTES / BLOCK_SIZE as u64
        }]
    );
    // The block layer's write lands where the control plane reads it.
    let id = kernel.submit_write(dev, 5, block(0x3C));
    assert_eq!(wait(&mut d, &kernel, id), BLOCK_SIZE as i32);
    assert_eq!(
        decode_hex(&io_read(&mut d, "a", 5, 1).data_hex).unwrap(),
        block(0x3C)
    );
    let list = devices(&mut d);
    assert_eq!(list.len(), 1);
    assert_eq!(list[0].state, DeviceState::Serving);
    assert_eq!(list[0].volume.as_deref(), Some("a"));
    // Offline takes the device down and forgets it; the empty record goes.
    ok(
        &mut d,
        &Request::VolumeOnline {
            name: "a".into(),
            online: false,
        },
    );
    assert_eq!(kernel.info(dev), None);
    assert!(!bindings::file_path(&scratch.state(), &agg).exists());
    ok(
        &mut d,
        &Request::VolumeOnline {
            name: "a".into(),
            online: true,
        },
    );
    assert!(show(&mut d, "a").device.is_some());
    // A clean stop deletes every device and leaves no record.
    ok(&mut d, &Request::PoolStop {});
    assert!(kernel.device_ids().is_empty());
    assert!(!bindings::file_path(&scratch.state(), &agg).exists());
}

#[test]
fn a_restart_after_kill_re_attaches_the_same_devices_before_serving() {
    let scratch = Scratch::new("restart");
    let kernel = MockKernel::new();
    let (agg, dev_a, dev_b, first_pid) = {
        let mut d = Daemon::new(config(&kernel, &scratch));
        format(&mut d, &scratch);
        let agg = agg_of(&boot(&mut d, &scratch));
        let a = create(&mut d, "a");
        let b = create(&mut d, "b");
        let (dev_a, dev_b) = (dev_of(&a), dev_of(&b));
        for (dev, fill) in [(dev_a, 0xA1), (dev_b, 0xB2)] {
            let id = kernel.submit_write(dev, 7, block(fill));
            assert_eq!(wait(&mut d, &kernel, id), BLOCK_SIZE as i32);
        }
        sync(&mut d);
        sync(&mut d);
        let pid = kernel.info(dev_a).unwrap().pid;
        (agg, dev_a, dev_b, pid)
        // kill -9.
    };
    assert!(!kernel.pid_alive(first_pid));
    for dev in [dev_a, dev_b] {
        assert_eq!(kernel.info(dev).unwrap().state, DevState::Quiesced);
    }
    // The block layer goes on reading: the quiesced device holds it.
    let held = kernel.submit_read(dev_a, 7, 1);
    let mut d = Daemon::new(config(&kernel, &scratch));
    let report = boot(&mut d, &scratch);
    // The heap arena died with the daemon (S5-1): the boot is dirty and
    // holds the volumes, which the re-attach does not change.
    assert!(
        report.last_boot.starts_with("dirty boot") && report.last_boot.contains("held by nvfail"),
        "{}",
        report.last_boot
    );
    // The same /dev/ublkbN for each volume, live under the new daemon.
    assert_eq!(dev_of(&show(&mut d, "a")), dev_a);
    assert_eq!(dev_of(&show(&mut d, "b")), dev_b);
    let new_pid = kernel.info(dev_a).unwrap().pid;
    assert_ne!(new_pid, first_pid);
    assert!(kernel.pid_alive(new_pid));
    for dev in [dev_a, dev_b] {
        assert_eq!(kernel.info(dev).unwrap().state, DevState::Live);
    }
    // No device was created: every ADD_DEV was the first daemon's.
    let adds: Vec<String> = kernel
        .log()
        .into_iter()
        .filter(|l| l.contains("AddDev"))
        .collect();
    assert_eq!(adds.len(), 2, "{adds:?}");
    assert!(adds.iter().all(|l| l.starts_with(&first_pid.to_string())));
    // Each device served again within the 2 s target of the arena
    // re-attach, as the daemon measured it.
    let events = d.pool().unwrap().recent_events();
    for dev in [dev_a, dev_b] {
        let line = events
            .iter()
            .find(|e| e.contains(&format!("re-attached /dev/ublkb{dev},")))
            .unwrap_or_else(|| panic!("{events:?}"));
        let ms: u64 = line
            .split("serving ")
            .nth(1)
            .and_then(|rest| rest.split(' ').next())
            .and_then(|n| n.parse().ok())
            .unwrap_or_else(|| panic!("{line}"));
        assert!(ms < 2_000, "{line}");
        assert!(!line.contains("beyond the 2 s target"), "{line}");
    }
    // The held read reaches the new daemon, once, and is answered as the
    // held volume answers: EIO, not lost and not left hanging.
    assert_eq!(wait(&mut d, &kernel, held), -EIO);
    assert_eq!(kernel.completion(held).unwrap().deliveries, 1);
    // Cleared, the volumes serve the committed data through the same
    // devices.
    for name in ["a", "b"] {
        clear(&mut d, name);
    }
    for (dev, fill) in [(dev_a, 0xA1), (dev_b, 0xB2)] {
        let id = kernel.submit_read(dev, 7, 1);
        assert_eq!(wait(&mut d, &kernel, id), BLOCK_SIZE as i32);
        assert_eq!(kernel.completion(id).unwrap().data, Some(block(fill)));
    }
    assert_eq!(
        decode_hex(&io_read(&mut d, "b", 7, 1).data_hex).unwrap(),
        block(0xB2)
    );
    let record = record(&scratch.state(), &agg);
    assert_eq!(
        record.bound(show(&mut d, "a").index).map(|b| b.0),
        Some(dev_a)
    );
    assert_eq!(
        record.bound(show(&mut d, "b").index).map(|b| b.0),
        Some(dev_b)
    );
    assert!(devices(&mut d)
        .iter()
        .all(|r| r.state == DeviceState::Serving));
    ok(&mut d, &Request::PoolStop {});
}

#[test]
fn the_requests_queued_at_a_dead_daemons_device_are_answered_once_by_the_next() {
    let scratch = Scratch::new("reissue");
    let kernel = MockKernel::new();
    let dev = {
        let mut d = Daemon::new(config(&kernel, &scratch));
        format(&mut d, &scratch);
        boot(&mut d, &scratch);
        let dev = dev_of(&create(&mut d, "a"));
        sync(&mut d);
        dev
    };
    // Requests the dead daemon never saw, queued at the quiesced device.
    let ids: Vec<u64> = (0..4).map(|i| kernel.submit_read(dev, i, 1)).collect();
    let mut d = Daemon::new(config(&kernel, &scratch));
    boot(&mut d, &scratch);
    for id in ids {
        // The volume is held (the heap arena died): every one is answered,
        // once, with the hold's EIO.
        assert_eq!(wait(&mut d, &kernel, id), -EIO, "request {id}");
        assert_eq!(kernel.completion(id).unwrap().deliveries, 1);
    }
    assert_eq!(kernel.unanswered(dev), 0);
    clear(&mut d, "a");
    let id = kernel.submit_read(dev, 0, 1);
    assert_eq!(wait(&mut d, &kernel, id), BLOCK_SIZE as i32);
    ok(&mut d, &Request::PoolStop {});
}

#[test]
fn a_daemon_killed_between_start_and_end_of_the_recovery_is_restarted_again() {
    let scratch = Scratch::new("startend");
    let kernel = MockKernel::new();
    let (dev, vol) = {
        let mut d = Daemon::new(config(&kernel, &scratch));
        format(&mut d, &scratch);
        boot(&mut d, &scratch);
        let v = create(&mut d, "a");
        let id = kernel.submit_write(dev_of(&v), 3, block(0x77));
        assert_eq!(wait(&mut d, &kernel, id), BLOCK_SIZE as i32);
        sync(&mut d);
        sync(&mut d);
        (dev_of(&v), v.index)
    };
    // The second daemon issues START_USER_RECOVERY, fetches every tag, and
    // is killed before END_USER_RECOVERY.
    kernel.kill_before(MockCmd::EndUserRecovery);
    let second = catch_unwind(AssertUnwindSafe(|| {
        let mut d = Daemon::new(config(&kernel, &scratch));
        boot(&mut d, &scratch);
    }));
    assert!(second.is_err(), "the second daemon was killed");
    let log = kernel.log();
    assert!(
        log.iter()
            .any(|l| l.ends_with(&format!("StartUserRecovery {dev}"))),
        "{log:?}"
    );
    assert!(!log
        .iter()
        .any(|l| l.ends_with(&format!("EndUserRecovery {dev}"))));
    assert_eq!(kernel.info(dev).unwrap().state, DevState::Quiesced);
    let pending = kernel.submit_read(dev, 3, 1);
    // The third one recovers the same device.
    let mut d = Daemon::new(config(&kernel, &scratch));
    boot(&mut d, &scratch);
    let v = show(&mut d, "a");
    assert_eq!((dev_of(&v), v.index), (dev, vol));
    assert_eq!(kernel.info(dev).unwrap().state, DevState::Live);
    // Answered once, by the third daemon, as the held volume answers.
    assert_eq!(wait(&mut d, &kernel, pending), -EIO);
    assert_eq!(kernel.completion(pending).unwrap().deliveries, 1);
    clear(&mut d, "a");
    let read = kernel.submit_read(dev, 3, 1);
    assert_eq!(wait(&mut d, &kernel, read), BLOCK_SIZE as i32);
    assert_eq!(kernel.completion(read).unwrap().data, Some(block(0x77)));
    ok(&mut d, &Request::PoolStop {});
}

#[test]
fn a_device_that_vanished_gives_its_volume_a_new_one() {
    let scratch = Scratch::new("vanished");
    let kernel = MockKernel::new();
    let (agg, dev) = {
        let mut d = Daemon::new(config(&kernel, &scratch));
        format(&mut d, &scratch);
        let agg = agg_of(&boot(&mut d, &scratch));
        (agg, dev_of(&create(&mut d, "a")))
    };
    // A tool deletes the dead daemon's device.
    kernel.process().reap(dev).unwrap();
    let mut d = Daemon::new(config(&kernel, &scratch));
    boot(&mut d, &scratch);
    let new_dev = dev_of(&show(&mut d, "a"));
    let new_pid = kernel.info(new_dev).unwrap().pid;
    let log = kernel.log();
    assert!(
        log.contains(&format!("{new_pid} AddDev {new_dev}")),
        "{log:?}"
    );
    assert!(!log.iter().any(|l| l.contains("StartUserRecovery")));
    let record = record(&scratch.state(), &agg);
    assert_eq!(record.entries().len(), 1);
    assert_eq!(record.entries()[0].dev_id(), new_dev);
    let events = d.pool().unwrap().recent_events();
    assert!(
        events.iter().any(|e| e.contains("no longer exists")),
        "{events:?}"
    );
}

#[test]
fn a_device_not_quiesced_in_time_is_listed_stale_and_reaped_on_request() {
    let scratch = Scratch::new("timeout");
    let kernel = MockKernel::new();
    let (agg, old) = {
        let mut d = Daemon::new(config(&kernel, &scratch));
        format(&mut d, &scratch);
        let agg = agg_of(&boot(&mut d, &scratch));
        let old = dev_of(&create(&mut d, "a"));
        // The dying process's io_uring context outlives the restart.
        kernel.hold_char_after_exit(old);
        (agg, old)
    };
    assert!(kernel.char_open(old));
    let began = Instant::now();
    let mut d = Daemon::new(config(&kernel, &scratch));
    boot(&mut d, &scratch);
    let waited = began.elapsed();
    assert!(waited >= Duration::from_secs(1), "{waited:?}");
    assert!(waited < Duration::from_secs(20), "{waited:?}");
    // The volume serves on a new device; the old one is stale, not deleted.
    let new_dev = dev_of(&show(&mut d, "a"));
    assert_ne!(new_dev, old);
    assert_eq!(kernel.info(old).unwrap().state, DevState::Quiesced);
    let list = devices(&mut d);
    let stale = list.iter().find(|r| r.dev_id == old).unwrap();
    assert_eq!(stale.state, DeviceState::Stale);
    assert!(
        stale.reason.contains("recovery timeout"),
        "{}",
        stale.reason
    );
    assert_eq!(
        list.iter().find(|r| r.dev_id == new_dev).unwrap().state,
        DeviceState::Serving
    );
    assert!(record(&scratch.state(), &agg).is_stale(old));
    // A serving device is never reaped; the stale one is, on request.
    let refused = err(
        &mut d,
        &Request::DeviceReap {
            dev_id: Some(new_dev),
            stale: false,
        },
    );
    assert_eq!(refused.code, ErrorCode::Engine);
    assert!(refused.message.contains("serving"), "{}", refused.message);
    let gone = reaped(&mut d, None, true);
    assert_eq!(gone.len(), 1);
    assert_eq!(gone[0].dev_id, old);
    assert_eq!(kernel.info(old), None);
    assert!(!record(&scratch.state(), &agg).is_stale(old));
    assert!(
        reaped(&mut d, None, true).is_empty(),
        "nothing left to reap"
    );
    // The dying process's last reference frees the number.
    kernel.release_char(old);
    assert!(!kernel.device_ids().contains(&old));
    ok(&mut d, &Request::PoolStop {});
}

#[test]
fn stale_and_foreign_devices_are_listed_and_only_stale_ones_are_reaped() {
    let scratch = Scratch::new("foreign");
    let kernel = MockKernel::new();
    let mut d = Daemon::new(config(&kernel, &scratch));
    format(&mut d, &scratch);
    let agg = agg_of(&boot(&mut d, &scratch));
    let ours = dev_of(&create(&mut d, "a"));
    let live_pid = kernel.foreign_process();
    let base = ForeignDevice {
        stamp: 0,
        state: DevState::Quiesced,
        pid: 4_000_000,
        recovery: true,
        size_blocks: 512,
        depth: 4,
    };
    let served_elsewhere = kernel.add_foreign(ForeignDevice {
        state: DevState::Live,
        pid: live_pid,
        ..base
    });
    let step5_leftover = kernel.add_foreign(base);
    let other_pool = kernel.add_foreign(ForeignDevice {
        stamp: device_stamp(pool_fingerprint(&agg) ^ 1, 0),
        ..base
    });
    let setting_up = kernel.add_foreign(ForeignDevice {
        state: DevState::Dead,
        pid: 0,
        ..base
    });
    let list = devices(&mut d);
    let state_of = |id: u32| list.iter().find(|r| r.dev_id == id).unwrap().state;
    assert_eq!(state_of(ours), DeviceState::Serving);
    assert_eq!(state_of(served_elsewhere), DeviceState::Foreign);
    assert_eq!(state_of(step5_leftover), DeviceState::Stale);
    assert_eq!(state_of(other_pool), DeviceState::Foreign);
    assert_eq!(state_of(setting_up), DeviceState::Foreign);
    let other = list.iter().find(|r| r.dev_id == other_pool).unwrap();
    assert_eq!(
        other.pool.as_deref(),
        Some(&*format!("{:08x}", pool_fingerprint(&agg) ^ 1))
    );
    assert_eq!(other.volume, None, "another pool's volume is not named");
    // Named, a foreign device is refused; an unknown one is not found.
    for id in [served_elsewhere, other_pool, setting_up] {
        let e = err(
            &mut d,
            &Request::DeviceReap {
                dev_id: Some(id),
                stale: false,
            },
        );
        assert_eq!(e.code, ErrorCode::Engine, "{}", e.message);
    }
    let e = err(
        &mut d,
        &Request::DeviceReap {
            dev_id: Some(77),
            stale: false,
        },
    );
    assert_eq!(e.code, ErrorCode::NotFound);
    let e = err(
        &mut d,
        &Request::DeviceReap {
            dev_id: Some(1),
            stale: true,
        },
    );
    assert_eq!(e.code, ErrorCode::BadRequest);
    // `--stale` takes the Step 5 leftover only.
    let gone = reaped(&mut d, None, true);
    assert_eq!(
        gone.iter().map(|r| r.dev_id).collect::<Vec<_>>(),
        vec![step5_leftover]
    );
    let mut left = kernel.device_ids();
    left.sort_unstable();
    let mut want = vec![ours, served_elsewhere, other_pool, setting_up];
    want.sort_unstable();
    assert_eq!(left, want);
    // A failure stops the reap and says what it was.
    let another = kernel.add_foreign(base);
    kernel.fail_next(MockCmd::StopDev, 5);
    let e = err(
        &mut d,
        &Request::DeviceReap {
            dev_id: Some(another),
            stale: false,
        },
    );
    assert_eq!(e.code, ErrorCode::Io);
    assert_eq!(reaped(&mut d, Some(another), false).len(), 1);
    ok(&mut d, &Request::PoolStop {});
}

#[test]
fn the_device_verbs_work_with_no_pool_booted_and_refuse_without_ublk() {
    let scratch = Scratch::new("nopool");
    let kernel = MockKernel::new();
    let (agg, dev) = {
        let mut d = Daemon::new(config(&kernel, &scratch));
        format(&mut d, &scratch);
        let agg = agg_of(&boot(&mut d, &scratch));
        (agg, dev_of(&create(&mut d, "a")))
    };
    let leftover = kernel.add_foreign(ForeignDevice {
        stamp: 0,
        state: DevState::Quiesced,
        pid: 4_000_001,
        recovery: true,
        size_blocks: 64,
        depth: 4,
    });
    let mut d = Daemon::new(config(&kernel, &scratch));
    let list = devices(&mut d);
    let ours = list.iter().find(|r| r.dev_id == dev).unwrap();
    assert_eq!(ours.state, DeviceState::Recoverable);
    assert_eq!(
        ours.pool.as_deref(),
        Some(&*format!("{:08x}", pool_fingerprint(&agg)))
    );
    assert_eq!(ours.volume_index, Some(0));
    assert!(!ours.pid_alive);
    assert_eq!(
        list.iter().find(|r| r.dev_id == leftover).unwrap().state,
        DeviceState::Stale
    );
    // `--stale` leaves the recoverable device; naming it deletes it.
    let gone = reaped(&mut d, None, true);
    assert_eq!(
        gone.iter().map(|r| r.dev_id).collect::<Vec<_>>(),
        vec![leftover]
    );
    assert!(kernel.info(dev).is_some());
    assert_eq!(
        reaped(&mut d, Some(dev), false)[0].state,
        DeviceState::Recoverable
    );
    assert!(kernel.device_ids().is_empty());
    // The pool then boots and gives its volume a new device.
    boot(&mut d, &scratch);
    assert!(show(&mut d, "a").device.is_some());
    ok(&mut d, &Request::PoolStop {});
    // Without ublk the verbs are refused, not answered with an empty list.
    let mut off = Daemon::new(DaemonConfig {
        ublk: false,
        ..config(&kernel, &scratch)
    });
    assert_eq!(
        err(&mut off, &Request::DeviceList {}).code,
        ErrorCode::Unsupported
    );
    assert_eq!(
        err(
            &mut off,
            &Request::DeviceReap {
                dev_id: None,
                stale: true
            }
        )
        .code,
        ErrorCode::Unsupported
    );
}

#[test]
fn a_device_left_without_a_record_is_found_by_its_stamp() {
    let scratch = Scratch::new("stampfound");
    let kernel = MockKernel::new();
    let (agg, dev) = {
        let mut d = Daemon::new(config(&kernel, &scratch));
        format(&mut d, &scratch);
        let agg = agg_of(&boot(&mut d, &scratch));
        (agg, dev_of(&create(&mut d, "a")))
    };
    // The record is lost (a crash before its rename, or a damaged file):
    // the stamp in ublksrv_flags still names the pool and the volume.
    let path = bindings::file_path(&scratch.state(), &agg);
    std::fs::write(&path, b"not a record").unwrap();
    let mut d = Daemon::new(config(&kernel, &scratch));
    boot(&mut d, &scratch);
    assert_eq!(dev_of(&show(&mut d, "a")), dev);
    let events = d.pool().unwrap().recent_events();
    assert!(
        events.iter().any(|e| e.contains("not usable")),
        "{events:?}"
    );
    assert!(
        events.iter().any(|e| e.contains("carries its stamp")),
        "{events:?}"
    );
    assert_eq!(
        record(&scratch.state(), &agg).bound(0).map(|b| b.0),
        Some(dev)
    );
    ok(&mut d, &Request::PoolStop {});
}

#[test]
fn a_recorded_device_whose_volume_is_gone_is_listed_stale() {
    let scratch = Scratch::new("volgone");
    let kernel = MockKernel::new();
    let agg = {
        let mut d = Daemon::new(config(&kernel, &scratch));
        format(&mut d, &scratch);
        let agg = agg_of(&boot(&mut d, &scratch));
        create(&mut d, "a");
        agg
    };
    // A device of volume 9 of this pool, recorded but with no volume 9.
    let orphan = kernel.add_foreign(ForeignDevice {
        stamp: device_stamp(pool_fingerprint(&agg), 9),
        state: DevState::Quiesced,
        pid: 4_000_002,
        recovery: true,
        size_blocks: 64,
        depth: 4,
    });
    let mut rec = record(&scratch.state(), &agg);
    rec.bind(9, orphan, 64);
    bindings::save(&scratch.state(), &rec, 1).unwrap();
    let mut d = Daemon::new(config(&kernel, &scratch));
    boot(&mut d, &scratch);
    let list = devices(&mut d);
    let entry = list.iter().find(|r| r.dev_id == orphan).unwrap();
    assert_eq!(entry.state, DeviceState::Stale);
    assert!(
        entry.reason.contains("no longer exists"),
        "{}",
        entry.reason
    );
    assert!(record(&scratch.state(), &agg).is_stale(orphan));
    assert!(kernel.info(orphan).is_some(), "listed, not deleted");
    assert_eq!(reaped(&mut d, Some(orphan), false).len(), 1);
    ok(&mut d, &Request::PoolStop {});
}

#[test]
fn a_second_daemon_on_a_served_pool_is_refused_before_it_opens_it() {
    let scratch = Scratch::new("second");
    let kernel = MockKernel::new();
    let mut first = Daemon::new(config(&kernel, &scratch));
    format(&mut first, &scratch);
    boot(&mut first, &scratch);
    let dev = dev_of(&create(&mut first, "a"));
    let id = kernel.submit_write(dev, 1, block(0x11));
    assert_eq!(wait(&mut first, &kernel, id), BLOCK_SIZE as i32);
    let mut second = Daemon::new(config(&kernel, &scratch));
    let e = err(
        &mut second,
        &Request::PoolBoot {
            device: scratch.pool(),
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
    assert_eq!(e.code, ErrorCode::Engine);
    assert!(
        e.message.contains("another dbladed runs this pool"),
        "{}",
        e.message
    );
    assert!(second.pool().is_none());
    // The first daemon never noticed: its device serves on.
    let log = kernel.log();
    assert!(
        !log.iter().any(|l| l.contains("StartUserRecovery")),
        "{log:?}"
    );
    let id = kernel.submit_read(dev, 1, 1);
    assert_eq!(wait(&mut first, &kernel, id), BLOCK_SIZE as i32);
    assert_eq!(kernel.completion(id).unwrap().data, Some(block(0x11)));
    ok(&mut first, &Request::PoolStop {});
}

#[test]
fn a_recorded_number_the_driver_gave_to_another_device_is_left_alone() {
    let scratch = Scratch::new("reused");
    let kernel = MockKernel::new();
    let (agg, dev) = {
        let mut d = Daemon::new(config(&kernel, &scratch));
        format(&mut d, &scratch);
        let agg = agg_of(&boot(&mut d, &scratch));
        (agg, dev_of(&create(&mut d, "a")))
    };
    // The dead daemon's device is deleted and its number goes to other
    // software's device.
    kernel.process().reap(dev).unwrap();
    let theirs = kernel.add_foreign(ForeignDevice {
        stamp: 0,
        state: DevState::Quiesced,
        pid: 4_000_003,
        recovery: true,
        size_blocks: VOL_BYTES / BLOCK_SIZE as u64,
        depth: 128,
    });
    assert_eq!(theirs, dev, "the number is reused");
    let mut d = Daemon::new(config(&kernel, &scratch));
    boot(&mut d, &scratch);
    let new_dev = dev_of(&show(&mut d, "a"));
    assert_ne!(new_dev, theirs);
    assert!(!kernel.log().iter().any(|l| l.contains("StartUserRecovery")));
    assert_eq!(kernel.info(theirs).unwrap().state, DevState::Quiesced);
    let rec = record(&scratch.state(), &agg);
    assert_eq!(rec.bound(0).map(|b| b.0), Some(new_dev));
    assert!(!rec.is_stale(theirs), "not recorded as ours");
    let events = d.pool().unwrap().recent_events();
    assert!(
        events
            .iter()
            .any(|e| e.contains("no longer carries the stamp")),
        "{events:?}"
    );
    ok(&mut d, &Request::PoolStop {});
}

#[test]
fn the_record_round_trips_and_refuses_what_does_not_verify() {
    let agg = Uuid::from_bytes([0x5A; 16]);
    let mut rec = Bindings::new(agg);
    rec.bind(2, 7, 4096);
    rec.bind(0, 3, 1024);
    rec.mark_stale(5, 1, StaleReason::RecoveryTimedOut);
    // Rebinding a volume replaces its device.
    rec.bind(2, 8, 4096);
    assert_eq!(rec.bound(2), Some((8, 4096)));
    assert_eq!(
        rec.entries()
            .iter()
            .map(Binding::dev_id)
            .collect::<Vec<_>>(),
        vec![3, 5, 8]
    );
    let bytes = rec.encode(99);
    assert_eq!(bytes.len(), 40 + 3 * 32 + 4);
    assert_eq!(&bytes[..8], b"OTAPBIND");
    assert_eq!(Bindings::decode(&bytes, &agg).unwrap(), rec);
    let refuse = |bytes: &[u8]| Bindings::decode(bytes, &agg).unwrap_err();
    assert_eq!(refuse(&bytes[..20]), BindingsError::TooShort { len: 20 });
    let mut bad = bytes.clone();
    bad[0] = b'X';
    assert_eq!(refuse(&bad), BindingsError::BadMagic);
    let mut bad = bytes.clone();
    bad[60] ^= 1;
    assert!(matches!(refuse(&bad), BindingsError::BadCrc { .. }));
    let mut bad = bytes.clone();
    bad[8] = 2;
    assert_eq!(refuse(&bad), BindingsError::BadVersion { found: 2 });
    assert!(matches!(
        refuse(&bytes[..bytes.len() - 1]),
        BindingsError::BadLength { .. }
    ));
    let other = Uuid::from_bytes([0x5B; 16]);
    assert_eq!(
        Bindings::decode(&bytes, &other).unwrap_err(),
        BindingsError::OtherPool { found: agg }
    );
    // Each entry field is checked, under a valid CRC.
    let reseal = |mut b: Vec<u8>| {
        let body = b.len() - 4;
        let crc = otap_format::checksum::crc32c(&b[..body]);
        b[body..].copy_from_slice(&crc.to_le_bytes());
        b
    };
    let entry = |i: usize| 40 + 32 * i;
    let mut b = bytes.clone();
    b[entry(0) + 16] = 9;
    assert!(matches!(
        refuse(&reseal(b)),
        BindingsError::BadEntry { field: "kind", .. }
    ));
    let mut b = bytes.clone();
    b[entry(1) + 17] = 0;
    assert!(matches!(
        refuse(&reseal(b)),
        BindingsError::BadEntry {
            field: "stale reason",
            ..
        }
    ));
    let mut b = bytes.clone();
    b[entry(0) + 4..entry(0) + 8].copy_from_slice(&5000u32.to_le_bytes());
    assert!(matches!(
        refuse(&reseal(b)),
        BindingsError::BadEntry {
            field: "volume",
            ..
        }
    ));
    let mut b = bytes.clone();
    b[entry(2) + 20] = 1;
    assert!(matches!(
        refuse(&reseal(b)),
        BindingsError::BadEntry {
            field: "reserved byte",
            ..
        }
    ));
    let mut b = bytes.clone();
    b[entry(1)..entry(1) + 4].copy_from_slice(&3u32.to_le_bytes());
    assert_eq!(
        refuse(&reseal(b)),
        BindingsError::DuplicateDevice { dev_id: 3 }
    );
    let mut b = bytes.clone();
    b[entry(2) + 4..entry(2) + 8].copy_from_slice(&0u32.to_le_bytes());
    assert_eq!(
        refuse(&reseal(b)),
        BindingsError::DuplicateVolume { vol: 0 }
    );
    let mut b = bytes;
    b[12..16].copy_from_slice(&5000u32.to_le_bytes());
    assert_eq!(refuse(&b), BindingsError::TooMany { count: 5000 });
}

#[test]
fn the_record_is_replaced_whole_and_removed_when_empty() {
    let scratch = Scratch::new("record");
    let dir = scratch.state().join("nested");
    let agg = Uuid::from_bytes([0x5C; 16]);
    let mut rec = Bindings::new(agg);
    assert_eq!(bindings::load(&dir, &agg), Ok(None), "no record yet");
    rec.bind(1, 4, 64);
    bindings::save(&dir, &rec, 7).unwrap();
    assert_eq!(bindings::load(&dir, &agg), Ok(Some(rec.clone())));
    rec.mark_stale(4, 1, StaleReason::VolumeOffline);
    bindings::save(&dir, &rec, 7).unwrap();
    assert_eq!(bindings::load(&dir, &agg), Ok(Some(rec.clone())));
    let names: Vec<String> = std::fs::read_dir(&dir)
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    assert_eq!(names, vec![bindings::file_name(&agg)], "no temporary left");
    // Another pool's record sits beside it untouched.
    let other = Uuid::from_bytes([0x5D; 16]);
    let mut theirs = Bindings::new(other);
    theirs.bind(0, 9, 64);
    bindings::save(&dir, &theirs, 8).unwrap();
    assert!(rec.forget(4));
    assert!(!rec.forget(4));
    bindings::save(&dir, &rec, 7).unwrap();
    assert_eq!(
        bindings::load(&dir, &agg),
        Ok(None),
        "the empty record is gone"
    );
    assert_eq!(bindings::load(&dir, &other), Ok(Some(theirs)));
    assert!(StaleReason::RecoveryFailed.describe(3).contains("volume 3"));
}

#[test]
fn dbladed_takes_a_state_dir_and_a_recovery_timeout() {
    let run = |args: &[&str]| {
        std::process::Command::new(env!("CARGO_BIN_EXE_dbladed"))
            .args(args)
            .output()
            .expect("dbladed runs")
    };
    let help = run(&["--help"]);
    assert!(help.status.success());
    let text = String::from_utf8_lossy(&help.stdout);
    assert!(text.contains("--state-dir <path>"), "{text}");
    assert!(text.contains("--recovery-timeout <secs>"), "{text}");
    for bad in [
        &["--recovery-timeout", "0"][..],
        &["--recovery-timeout", "301"],
        &["--recovery-timeout", "soon"],
        &["--state-dir="],
    ] {
        let out = run(bad);
        assert_eq!(out.status.code(), Some(2), "{bad:?}");
    }
}
