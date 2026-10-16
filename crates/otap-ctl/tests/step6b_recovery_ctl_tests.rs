//! Step 6B: `otapctl device list` and `otapctl device reap` against an
//! in-process `dbladed` whose ublk control plane is the in-memory 6.12
//! driver (`otap_ublk::MockKernel`), run as an operator would: the table,
//! the JSON, the reaper's refusals and the usage errors.

use std::path::PathBuf;
use std::process::Command;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::Duration;

use otap_control::Endpoint;
use otap_daemon::{serve, Daemon, DaemonConfig, DaemonHandle, Server, UblkBackend};
use otap_ublk::{DevState, ForeignDevice, MockKernel, UblkHost};

struct Rig {
    endpoint: String,
    dir: PathBuf,
    stop: Arc<AtomicBool>,
    handle: Option<DaemonHandle>,
    engine: Option<JoinHandle<()>>,
    server: Option<JoinHandle<std::io::Result<()>>>,
}

impl Rig {
    /// A daemon over `kernel`, or with ublk off when `None`.
    fn new(name: &str, kernel: Option<&MockKernel>) -> Self {
        let dir = std::env::temp_dir().join(format!("otap-ctl-6b-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let (ublk, ublk_backend) = match kernel {
            Some(k) => {
                let k = k.clone();
                (
                    true,
                    UblkBackend::Custom(Arc::new(move || {
                        Box::new(k.process()) as Box<dyn UblkHost>
                    })),
                )
            }
            None => (false, UblkBackend::Kernel),
        };
        let cfg = DaemonConfig {
            control: Endpoint::parse("tcp:127.0.0.1:0").unwrap(),
            ublk,
            ublk_backend,
            state_dir: dir.join("state"),
            op_timeout: Duration::from_secs(60),
            ..DaemonConfig::default()
        };
        let stop = Arc::new(AtomicBool::new(false));
        let server = Server::bind(&cfg.control).unwrap();
        let endpoint = server.endpoint().to_string();
        let (handle, engine) = Daemon::spawn(cfg, Arc::clone(&stop)).unwrap();
        let server = {
            let handle = handle.clone();
            let stop = Arc::clone(&stop);
            std::thread::spawn(move || serve(server, &handle, &stop))
        };
        Self {
            endpoint,
            dir,
            stop,
            handle: Some(handle),
            engine: Some(engine),
            server: Some(server),
        }
    }

    fn path(&self, file: &str) -> String {
        self.dir.join(file).display().to_string()
    }

    fn ctl(&self, args: &[&str]) -> (i32, String, String) {
        let output = Command::new(env!("CARGO_BIN_EXE_otapctl"))
            .arg("--control")
            .arg(&self.endpoint)
            .args(args)
            .output()
            .expect("otapctl runs");
        (
            output.status.code().unwrap_or(-1),
            String::from_utf8_lossy(&output.stdout).into_owned(),
            String::from_utf8_lossy(&output.stderr).into_owned(),
        )
    }

    fn ok(&self, args: &[&str]) -> String {
        let (code, out, err) = self.ctl(args);
        assert_eq!(code, 0, "otapctl {args:?}\nstdout: {out}\nstderr: {err}");
        out
    }

    fn refused(&self, args: &[&str]) -> String {
        let (code, out, err) = self.ctl(args);
        assert_eq!(code, 1, "otapctl {args:?}\nstdout: {out}\nstderr: {err}");
        err
    }

    fn json(&self, args: &[&str]) -> serde_json::Value {
        let mut full = vec!["--json"];
        full.extend_from_slice(args);
        let out = self.ok(&full);
        serde_json::from_str(&out).unwrap_or_else(|e| panic!("{e}: {out}"))
    }

    fn boot(&self) {
        let device = self.path("pool.img");
        self.ok(&["pool", "format", "--device", &device, "--size", "128M"]);
        self.ok(&[
            "pool",
            "boot",
            "--device",
            &device,
            "--heap-arena",
            "--arena-bytes",
            "64M",
            "--cp-interval",
            "1",
        ]);
    }
}

impl Drop for Rig {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        drop(self.handle.take());
        if let Some(t) = self.engine.take() {
            let _ = t.join();
        }
        if let Some(t) = self.server.take() {
            let _ = t.join();
        }
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

fn leftover(kernel: &MockKernel) -> u32 {
    kernel.add_foreign(ForeignDevice {
        stamp: 0,
        state: DevState::Quiesced,
        pid: 4_000_100,
        recovery: true,
        size_blocks: 64,
        depth: 4,
    })
}

#[test]
fn device_list_shows_every_device_and_its_standing() {
    let kernel = MockKernel::new();
    let rig = Rig::new("list", Some(&kernel));
    let empty = rig.ok(&["device", "list"]);
    assert_eq!(empty.trim(), "no ublk devices");
    rig.boot();
    rig.ok(&["volume", "create", "--name", "data", "--size", "16M"]);
    let stale = leftover(&kernel);
    let live_pid = kernel.foreign_process();
    let foreign = kernel.add_foreign(ForeignDevice {
        stamp: 0,
        state: DevState::Live,
        pid: live_pid,
        recovery: true,
        size_blocks: 64,
        depth: 4,
    });
    let table = rig.ok(&["device", "list"]);
    let lines: Vec<&str> = table.lines().collect();
    assert!(lines[0].starts_with("ID  PATH"), "{table}");
    assert!(
        lines[0].contains("STATE") && lines[0].contains("REASON"),
        "{table}"
    );
    let row = |id: u32| {
        lines
            .iter()
            .find(|l| l.starts_with(&format!("{id} ")))
            .unwrap_or_else(|| panic!("no row for {id}: {table}"))
            .to_string()
    };
    let ours = row(0);
    assert!(
        ours.contains("/dev/ublkb0") && ours.contains("serving"),
        "{ours}"
    );
    assert!(ours.contains("data") && ours.contains("live"), "{ours}");
    let s = row(stale);
    assert!(s.contains("stale") && s.contains("4000100 (dead)"), "{s}");
    assert!(s.contains("not an OpenTAP device"), "{s}");
    let f = row(foreign);
    assert!(
        f.contains("foreign") && f.contains(&live_pid.to_string()),
        "{f}"
    );
    let json = rig.json(&["device", "list"]);
    assert_eq!(json["kind"], "devices");
    let devices = json["devices"].as_array().unwrap();
    assert_eq!(devices.len(), 3);
    assert_eq!(devices[0]["state"], "serving");
    assert_eq!(devices[0]["volume"], "data");
    assert_eq!(devices[0]["reissue"], true);
    assert_eq!(devices[1]["state"], "stale");
    assert_eq!(devices[2]["state"], "foreign");
    assert_eq!(devices[2]["pid_alive"], true);
}

#[test]
fn device_reap_deletes_stale_devices_and_refuses_the_rest() {
    let kernel = MockKernel::new();
    let rig = Rig::new("reap", Some(&kernel));
    rig.boot();
    rig.ok(&["volume", "create", "--name", "data", "--size", "16M"]);
    let first = leftover(&kernel);
    let second = leftover(&kernel);
    let named = rig.ok(&["device", "reap", &first.to_string()]);
    assert!(
        named.starts_with(&format!("reaped /dev/ublkb{first} (stale: ")),
        "{named}"
    );
    let all = rig.ok(&["device", "reap", "--stale"]);
    assert_eq!(all.lines().count(), 1, "{all}");
    assert!(all.contains(&format!("/dev/ublkb{second}")), "{all}");
    assert_eq!(
        rig.ok(&["device", "reap", "--stale"]).trim(),
        "nothing to reap"
    );
    let json = rig.json(&["device", "reap", "--stale"]);
    assert_eq!(json, serde_json::json!({ "kind": "reaped", "devices": [] }));
    // The volume's device is never reaped; a missing one is not found.
    let err = rig.refused(&["device", "reap", "/dev/ublkb0"]);
    assert!(err.contains("refused") && err.contains("serving"), "{err}");
    let err = rig.refused(&["device", "reap", "99"]);
    assert!(
        err.contains("not found") && err.contains("no ublk device 99"),
        "{err}"
    );
    assert_eq!(kernel.device_ids(), vec![0]);
    // The command line refuses what the daemon would.
    for bad in [
        &["device", "reap"][..],
        &["device", "reap", "1", "--stale"],
        &["device", "reap", "sdb"],
        &["device", "list", "--stale"],
    ] {
        let (code, _, err) = rig.ctl(bad);
        assert_eq!(code, 2, "{bad:?}: {err}");
    }
}

#[test]
fn device_verbs_answer_without_a_pool_and_refuse_without_ublk() {
    let kernel = MockKernel::new();
    let stale = leftover(&kernel);
    let rig = Rig::new("nopool", Some(&kernel));
    let table = rig.ok(&["device", "list"]);
    assert!(table.contains("stale"), "{table}");
    let gone = rig.ok(&["device", "reap", &stale.to_string()]);
    assert!(gone.starts_with("reaped"), "{gone}");
    assert!(kernel.device_ids().is_empty());
    let off = Rig::new("noublk", None);
    let err = off.refused(&["device", "list"]);
    assert!(
        err.contains("unsupported") && err.contains("no ublk devices"),
        "{err}"
    );
    let json = {
        let (code, out, _) = off.ctl(&["--json", "device", "reap", "--stale"]);
        assert_eq!(code, 1);
        serde_json::from_str::<serde_json::Value>(&out).unwrap()
    };
    assert_eq!(json["error"]["code"], "unsupported");
}
