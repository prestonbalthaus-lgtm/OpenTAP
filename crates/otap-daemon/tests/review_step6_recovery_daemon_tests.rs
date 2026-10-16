//! The Step 6 adversarial review (`docs/reviews/2026-10-16-step6-qa-adversarial.md`):
//! `dbladed`'s ublk device findings, over the in-memory 6.12 driver
//! ([`MockKernel`]), and its command line. Each test reproduced its finding
//! before the fix and pins the fixed behaviour. The helpers are those of
//! `step6b_recovery_daemon_tests.rs`, copied so this file stands alone.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use otap_control::{
    ArenaKind, DeviceReport, DeviceState, Endpoint, PoolReport, Request, Response, VolumeReport,
    BLOCK_SIZE,
};
use otap_daemon::bindings::{self, Bindings};
use otap_daemon::{Daemon, DaemonConfig, UblkBackend};
use otap_format::Uuid;
use otap_ublk::{device_stamp, pool_fingerprint, DevState, ForeignDevice, MockKernel, UblkHost};

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

fn devices(d: &mut Daemon) -> Vec<DeviceReport> {
    match ok(d, &Request::DeviceList {}) {
        Response::Devices { devices } => devices,
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

/// D11: a device's identity is its pool and volume index, and the binding
/// record is what tells two incarnations of an index apart. A `--no-ublk`
/// boot neither loaded nor updated the record, so a volume deleted there
/// and a new volume created at its index left the old binding in place:
/// the next boot with ublk re-attached the dead volume's device to the new
/// volume, delivering the old volume's reissued writes into it and serving
/// it to whatever still had the old device open. The record is now kept on
/// every boot, and a deletion marks the deleted volume's device stale.
#[test]
fn d11_a_device_never_outlives_its_volume_into_the_next_one_at_its_index() {
    let scratch = Scratch::new("review-d11a");
    let kernel = MockKernel::new();
    let old_dev = {
        let mut d = Daemon::new(config(&kernel, &scratch));
        format(&mut d, &scratch);
        boot(&mut d, &scratch);
        let a = create(&mut d, "a");
        sync(&mut d);
        dev_of(&a)
        // Killed: dropped without `PoolStop`.
    };
    {
        let mut cfg = config(&kernel, &scratch);
        cfg.ublk = false;
        let mut d = Daemon::new(cfg);
        boot(&mut d, &scratch);
        clear(&mut d, "a");
        ok(
            &mut d,
            &Request::VolumeOnline {
                name: "a".into(),
                online: false,
            },
        );
        ok(&mut d, &Request::VolumeDelete { name: "a".into() });
        let b = create(&mut d, "b");
        assert_eq!(b.index, 0, "the new volume took the deleted one's index");
        ok(&mut d, &Request::PoolStop {});
    }
    let mut d = Daemon::new(config(&kernel, &scratch));
    boot(&mut d, &scratch);
    let b = show(&mut d, "b");
    assert_ne!(
        dev_of(&b),
        old_dev,
        "the deleted volume's device was re-attached to the new volume"
    );
    let list = devices(&mut d);
    let old = list.iter().find(|r| r.dev_id == old_dev).expect("listed");
    assert_eq!(old.state, DeviceState::Stale, "{old:?}");
    ok(&mut d, &Request::PoolStop {});
}

/// D11, the second case: two devices carrying the same volume's stamp and
/// no record to choose between them (the record lost, or not readable,
/// after a recovery that fell back to a new device and left the old one
/// quiescing with its held writes). The first found, the older device, was
/// re-attached, and its old writes replayed over the newer device's. Two
/// devices claiming one volume with no binding to decide are both left
/// stale; the volume gets a new device.
#[test]
fn d11_two_devices_claiming_one_volume_without_a_record_are_both_left_stale() {
    let scratch = Scratch::new("review-d11b");
    let kernel = MockKernel::new();
    let agg = {
        let mut d = Daemon::new(config(&kernel, &scratch));
        format(&mut d, &scratch);
        let agg = agg_of(&boot(&mut d, &scratch));
        create(&mut d, "a");
        // A clean stop deletes the volume's device and empties the record;
        // the volume stays online.
        ok(&mut d, &Request::PoolStop {});
        agg
    };
    let size_blocks = VOL_BYTES / BLOCK_SIZE as u64;
    let twins: Vec<u32> = (0..2)
        .map(|i| {
            kernel.add_foreign(ForeignDevice {
                stamp: device_stamp(pool_fingerprint(&agg), 0),
                state: DevState::Quiesced,
                pid: 4_000_100 + i,
                recovery: true,
                size_blocks,
                depth: 128,
            })
        })
        .collect();
    assert!(record(&scratch.state(), &agg).entries().is_empty());
    let mut d = Daemon::new(config(&kernel, &scratch));
    boot(&mut d, &scratch);
    let a = show(&mut d, "a");
    assert!(
        !twins.contains(&dev_of(&a)),
        "one of two devices claiming the volume was taken: {twins:?}, {a:?}"
    );
    let list = devices(&mut d);
    for twin in &twins {
        let entry = list.iter().find(|r| r.dev_id == *twin).expect("listed");
        assert_ne!(entry.state, DeviceState::Serving, "{entry:?}");
    }
    ok(&mut d, &Request::PoolStop {});
}

/// R4: `GET_DEV_INFO` sent without a character-device path is refused with
/// `EINVAL` for a device that other software created with
/// `UBLK_F_UNPRIVILEGED_DEV` (`ublk_ctrl_uring_cmd_permission` in the saved
/// 6.12 driver), and one refusal failed the whole listing: one such device
/// on the host refused every pool boot, `device list` and `device reap`.
/// The in-memory driver now answers as the kernel does, and the listing
/// leaves out a device it may not describe (OpenTAP's own devices are
/// privileged, so it is never one of them).
#[test]
fn r4_an_unprivileged_device_of_other_software_never_refuses_a_boot() {
    let scratch = Scratch::new("review-r4");
    let kernel = MockKernel::new();
    {
        let mut d = Daemon::new(config(&kernel, &scratch));
        format(&mut d, &scratch);
        boot(&mut d, &scratch);
        create(&mut d, "a");
        ok(&mut d, &Request::PoolStop {});
    }
    let theirs = kernel.add_foreign(ForeignDevice {
        stamp: 0,
        state: DevState::Live,
        pid: kernel.foreign_process(),
        recovery: false,
        size_blocks: 64,
        depth: 4,
    });
    kernel.set_unprivileged(theirs);
    let mut d = Daemon::new(config(&kernel, &scratch));
    boot(&mut d, &scratch);
    let a = show(&mut d, "a");
    assert!(a.device.is_some(), "{a:?}");
    let list = devices(&mut d);
    assert!(list.iter().all(|r| r.dev_id != theirs), "{list:?}");
    assert!(kernel.info(theirs).is_some(), "never touched");
    ok(&mut d, &Request::PoolStop {});
}

/// R2 (`dbladed`'s side): a switch given a value (`--discard-arena=false`)
/// was read as the switch, whatever the value. It is refused now.
#[test]
fn r2_a_dbladed_switch_given_a_value_is_refused() {
    let run = |args: &[&str]| {
        std::process::Command::new(env!("CARGO_BIN_EXE_dbladed"))
            .args(args)
            .output()
            .expect("dbladed runs")
    };
    for bad in [
        &["--heap-arena=no", "--help"][..],
        &["--no-ublk=false", "--help"],
        &["--allow-remote-replication=0", "--help"],
        &["--discard-arena=false", "--help"],
    ] {
        let out = run(bad);
        assert_eq!(out.status.code(), Some(2), "{bad:?}");
        let text = String::from_utf8_lossy(&out.stderr);
        assert!(text.contains("takes no value"), "{bad:?}: {text}");
    }
    assert!(run(&["--heap-arena", "--help"]).status.success());
}

/// The review's decision on `--recovery-timeout` (E48): the default waits
/// past the block layer's 30 s request timeout, which is what quiesces a
/// device whose daemon died holding every tag, so such a volume keeps its
/// device number; at 5 s it never could. Only that case waits.
#[test]
fn the_recovery_timeout_outlasts_the_block_layers_request_timeout() {
    assert_eq!(
        DaemonConfig::default().recovery_timeout,
        Duration::from_secs(35)
    );
    let help = std::process::Command::new(env!("CARGO_BIN_EXE_dbladed"))
        .arg("--help")
        .output()
        .expect("dbladed runs");
    let text = String::from_utf8_lossy(&help.stdout);
    assert!(text.contains("[default: 35"), "{text}");
}
