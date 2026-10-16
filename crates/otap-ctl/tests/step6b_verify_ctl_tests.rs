//! Step 6B: `otapctl pool verify` against an in-process `dbladed` on
//! loopback TCP: it starts the verification job and follows it to its
//! report (exit 0 when clean, 4 otherwise), or only starts it
//! (`--no-wait`), shows the last one (`--status`) or cancels it
//! (`--cancel`); text and JSON.

use std::path::PathBuf;
use std::process::Command;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::Duration;

use otap_control::Endpoint;
use otap_daemon::{serve, Daemon, DaemonConfig, DaemonHandle, Server};

struct Rig {
    endpoint: String,
    dir: PathBuf,
    stop: Arc<AtomicBool>,
    handle: Option<DaemonHandle>,
    engine: Option<JoinHandle<()>>,
    server: Option<JoinHandle<std::io::Result<()>>>,
}

impl Rig {
    fn new(name: &str) -> Self {
        let dir =
            std::env::temp_dir().join(format!("otap-ctl-verify-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let cfg = DaemonConfig {
            control: Endpoint::parse("tcp:127.0.0.1:0").unwrap(),
            ublk: false,
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

    fn booted(name: &str) -> (Self, String) {
        let rig = Self::new(name);
        let device = rig.path("pool.img");
        rig.ok(&["pool", "format", "--device", &device, "--size", "128M"]);
        rig.ok(&[
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
        rig.ok(&["volume", "create", "--name", "data", "--size", "16M"]);
        (rig, device)
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

#[test]
fn pool_verify_follows_the_job_to_its_report_in_text_and_json() {
    let (rig, _) = Rig::booted("healthy");
    let (code, _, err) = rig.ctl(&["pool", "verify", "--status"]);
    assert_eq!(code, 1, "{err}");
    assert!(
        err.contains("not found") && err.contains("no verification"),
        "{err}"
    );
    let (code, _, err) = rig.ctl(&["pool", "verify", "--cancel"]);
    assert_eq!(code, 1, "{err}");
    rig.ok(&[
        "io", "write", "--volume", "data", "--lba", "0", "--fill", "0x5a", "--blocks", "16",
    ]);
    rig.ok(&["snapshot", "create", "--volume", "data", "--name", "s1"]);
    rig.ok(&[
        "io", "write", "--volume", "data", "--lba", "4", "--fill", "0x6b", "--blocks", "4",
    ]);
    rig.ok(&["pool", "sync"]);
    let text = rig.ok(&["pool", "verify"]);
    assert!(text.contains("verification of cp"), "{text}");
    assert!(text.contains("done (finished, 100%), clean"), "{text}");
    assert!(text.contains("4 superblock slots"), "{text}");
    assert!(
        text.contains("findings:  0 error(s), 0 warning(s)"),
        "{text}"
    );
    let (code, out, err) = rig.ctl(&["--json", "pool", "verify"]);
    assert_eq!(code, 0, "{out}{err}");
    let json: serde_json::Value = serde_json::from_str(&out).unwrap();
    assert_eq!(json["kind"], "verify");
    assert_eq!(json["state"], "done");
    assert_eq!(json["verdict"], "clean");
    assert_eq!(json["superblock_slots"], 4);
    assert!(json["blocks"].as_u64().unwrap() > 0);
    // The last one, again.
    let status = rig.ok(&["pool", "verify", "--status"]);
    assert!(status.contains("clean"), "{status}");
    // Started only: answered with the job as it stands.
    let started = rig.ok(&["pool", "verify", "--no-wait"]);
    assert!(started.contains("verification"), "{started}");
}

#[test]
fn pool_verify_exits_4_on_a_damaged_pool_and_lists_the_findings() {
    let (rig, device) = Rig::booted("damaged");
    let pattern = rig.path("pattern.bin");
    let mut block = vec![0u8; 4096];
    for (i, b) in block.iter_mut().enumerate() {
        *b = (i * 11 + 3) as u8;
    }
    block[..16].copy_from_slice(b"step6b-ctl-flip!");
    std::fs::write(&pattern, &block).unwrap();
    rig.ok(&[
        "io", "write", "--volume", "data", "--lba", "12", "--file", &pattern,
    ]);
    rig.ok(&["pool", "sync"]);
    let file = std::fs::read(&device).unwrap();
    let at = file
        .windows(16)
        .position(|w| w == b"step6b-ctl-flip!")
        .expect("the block is in the file");
    {
        use std::io::{Seek, SeekFrom, Write};
        let mut f = std::fs::OpenOptions::new()
            .write(true)
            .open(&device)
            .unwrap();
        f.seek(SeekFrom::Start(at as u64 + 2000)).unwrap();
        f.write_all(&[block[2000] ^ 0x80]).unwrap();
        f.sync_all().unwrap();
    }
    let (code, out, err) = rig.ctl(&["pool", "verify"]);
    assert_eq!(code, 4, "{out}{err}");
    assert!(out.contains("DAMAGED"), "{out}");
    assert!(out.contains("block_crc"), "{out}");
    assert!(out.contains("SEVERITY") && out.contains("error"), "{out}");
    assert!(out.contains("#0"), "{out}");
    let status = rig.ok(&["pool", "status"]);
    assert!(status.contains("verification of cp"), "{status}");
    let (code, out, _) = rig.ctl(&["--json", "pool", "verify"]);
    assert_eq!(code, 4);
    let json: serde_json::Value = serde_json::from_str(&out).unwrap();
    assert_eq!(json["verdict"], "damaged");
    assert_eq!(json["findings"][0]["kind"], "block_crc");
    assert_eq!(json["findings"][0]["lba"], 12);
}
