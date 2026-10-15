//! `otapctl` against an in-process `dbladed` on loopback TCP: the binary
//! is run as an operator would, and its exit status and output checked.

use std::path::PathBuf;
use std::process::Command;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

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
        let dir = std::env::temp_dir().join(format!("otap-ctl-{}-{name}", std::process::id()));
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

    /// Runs `otapctl --control <endpoint> <args>`; returns the exit status,
    /// stdout and stderr.
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

    /// Runs and expects success; returns stdout.
    fn ok(&self, args: &[&str]) -> String {
        let (code, out, err) = self.ctl(args);
        assert_eq!(code, 0, "otapctl {args:?}\nstdout: {out}\nstderr: {err}");
        out
    }

    /// Runs and expects a daemon refusal; returns stderr.
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
fn the_command_line_runs_the_whole_lifecycle() {
    let rig = Rig::new("lifecycle");
    let device = rig.path("pool.img");

    let ping = rig.ok(&["daemon", "ping"]);
    assert!(ping.contains("protocol 1"), "{ping}");
    assert!(ping.contains("pool booted: no"), "{ping}");
    let err = rig.refused(&["pool", "status"]);
    assert!(err.contains("no pool"), "{err}");

    let formatted = rig.ok(&["pool", "format", "--device", &device, "--size", "128M"]);
    assert!(formatted.starts_with("formatted pool"), "{formatted}");
    assert!(formatted.contains("63 segments"), "{formatted}");
    let err = rig.refused(&["pool", "format", "--device", &device, "--size", "128M"]);
    assert!(err.contains("already holds"), "{err}");

    let booted = rig.ok(&[
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
    assert!(booted.contains("serving:      yes"), "{booted}");
    assert!(booted.contains("arena:        heap, 64 MiB"), "{booted}");
    assert!(booted.contains("last boot:    clean boot"), "{booted}");

    let created = rig.ok(&["volume", "create", "--name", "data", "--size", "16M"]);
    assert!(created.contains("volume data"), "{created}");
    assert!(created.contains("state:      online"), "{created}");
    let list = rig.ok(&["volume", "list"]);
    assert!(list.contains("NAME"), "{list}");
    assert!(list.contains("data"), "{list}");
    assert!(list.contains("16 MiB"), "{list}");
    assert!(list.contains("online"), "{list}");

    let wrote = rig.ok(&[
        "io", "write", "--volume", "data", "--lba", "0", "--fill", "0xab", "--blocks", "2",
    ]);
    assert!(wrote.starts_with("ok, seq "), "{wrote}");
    let read = rig.json(&[
        "io", "read", "--volume", "data", "--lba", "0", "--blocks", "2",
    ]);
    assert_eq!(read["kind"], "io");
    assert_eq!(read["status"], "ok");
    assert_eq!(read["data_hex"], "ab".repeat(2 * 4096));
    let out = rig.path("block1.bin");
    let saved = rig.ok(&[
        "io", "read", "--volume", "data", "--lba", "1", "--out", &out,
    ]);
    assert!(saved.contains("4 KiB written to"), "{saved}");
    assert_eq!(std::fs::read(&out).unwrap(), vec![0xab; 4096]);
    let pattern = rig.path("pattern.bin");
    std::fs::write(&pattern, vec![0x42u8; 3 * 4096]).unwrap();
    rig.ok(&[
        "io", "write", "--volume", "data", "--lba", "8", "--file", &pattern,
    ]);
    let read = rig.json(&["io", "read", "--volume", "data", "--lba", "9"]);
    assert_eq!(read["data_hex"], "42".repeat(4096));
    let dump = rig.ok(&["io", "read", "--volume", "data", "--lba", "9"]);
    assert!(dump.starts_with("ok, 4 KiB"), "{dump}");
    assert!(dump.contains("00000000  42 42 42"), "{dump}");
    let punched = rig.ok(&[
        "io", "punch", "--volume", "data", "--lba", "8", "--blocks", "3",
    ]);
    assert!(punched.starts_with("ok"), "{punched}");
    let read = rig.json(&["io", "read", "--volume", "data", "--lba", "9"]);
    assert_eq!(read["data_hex"], "00".repeat(4096));

    let snap = rig.ok(&["snapshot", "create", "--volume", "data", "--name", "s1"]);
    assert!(snap.contains("snapshot s1"), "{snap}");
    assert!(snap.contains("kind:     crash, origin user"), "{snap}");
    let snaps = rig.ok(&["snapshot", "list"]);
    assert!(snaps.contains("VOLUME") && snaps.contains("s1"), "{snaps}");
    rig.ok(&[
        "io", "write", "--volume", "data", "--lba", "0", "--fill", "0xcd", "--blocks", "1",
    ]);
    let rolled = rig.ok(&["snapshot", "rollback", "--volume", "data", "--name", "s1"]);
    assert!(rolled.contains("generation: 1"), "{rolled}");
    assert!(rolled.contains("state:      online"), "{rolled}");
    let read = rig.json(&["io", "read", "--volume", "data", "--lba", "0"]);
    assert_eq!(read["data_hex"], "ab".repeat(4096));

    let pinned = rig.ok(&["snapshot", "pin", "--volume", "data", "--name", "s1"]);
    assert!(pinned.contains("pins:     user"), "{pinned}");
    let err = rig.refused(&["snapshot", "delete", "--volume", "data", "--name", "s1"]);
    assert!(err.contains("pinned"), "{err}");
    let unpinned = rig.ok(&["snapshot", "unpin", "--volume", "data", "--name", "s1"]);
    assert!(unpinned.contains("pins:     -"), "{unpinned}");
    let base = rig.ok(&[
        "snapshot",
        "replication-base",
        "--volume",
        "data",
        "--name",
        "s1",
    ]);
    assert!(base.contains("pins:     replication"), "{base}");
    rig.refused(&["snapshot", "delete", "--volume", "data", "--name", "s1"]);
    assert_eq!(
        rig.ok(&[
            "snapshot",
            "replication-base",
            "--volume",
            "data",
            "--release"
        ]),
        "ok\n"
    );
    assert_eq!(
        rig.ok(&["snapshot", "delete", "--volume", "data", "--name", "s1"]),
        "ok\n"
    );
    assert_eq!(rig.ok(&["snapshot", "list"]), "no snapshots\n");

    let err = rig.refused(&["volume", "resize", "--name", "data", "--size", "8M"]);
    assert!(err.contains("offline"), "{err}");
    let off = rig.ok(&["volume", "offline", "--name", "data"]);
    assert!(off.contains("state:      offline"), "{off}");
    let shrunk = rig.ok(&["volume", "resize", "--name", "data", "--size", "8M"]);
    assert!(shrunk.contains("size:       8 MiB"), "{shrunk}");
    let on = rig.ok(&["volume", "online", "--name", "data"]);
    assert!(on.contains("state:      online"), "{on}");
    let grown = rig.ok(&["volume", "resize", "--name", "data", "--size", "24M"]);
    assert!(grown.contains("size:       24 MiB"), "{grown}");

    let renamed = rig.ok(&["volume", "rename", "--name", "data", "--new-name", "db"]);
    assert!(renamed.contains("volume db"), "{renamed}");
    rig.ok(&["volume", "show", "--name", "db"]);
    let err = rig.refused(&["volume", "show", "--name", "data"]);
    assert!(err.contains("not found"), "{err}");

    let cleaned = rig.ok(&["pool", "clean"]);
    assert!(cleaned.starts_with("committed cp "), "{cleaned}");
    let synced = rig.ok(&["pool", "sync"]);
    assert!(synced.starts_with("committed cp "), "{synced}");
    let status = rig.json(&["pool", "status"]);
    assert_eq!(status["kind"], "pool");
    assert_eq!(status["volumes"], 1);
    assert_eq!(status["segments"], 63);

    rig.ok(&["volume", "offline", "--name", "db"]);
    assert_eq!(rig.ok(&["volume", "delete", "--name", "db"]), "ok\n");
    assert_eq!(rig.ok(&["volume", "list"]), "no volumes\n");

    let stopped = rig.ok(&["pool", "stop"]);
    assert!(stopped.starts_with("committed cp "), "{stopped}");
    let ping = rig.ok(&["daemon", "ping"]);
    assert!(ping.contains("pool booted: no"), "{ping}");
    let booted = rig.json(&[
        "pool",
        "boot",
        "--device",
        &device,
        "--heap-arena",
        "--arena-bytes",
        "64M",
    ]);
    assert_eq!(booted["cp_interval_secs"], 5);
    assert!(booted["last_boot"]
        .as_str()
        .unwrap()
        .starts_with("clean boot"));
    assert_eq!(rig.ok(&["daemon", "stop"]), "ok\n");
    let handle = rig.handle.as_ref().unwrap();
    let deadline = Instant::now() + Duration::from_secs(30);
    while !handle.is_stopped() {
        assert!(Instant::now() < deadline, "the daemon did not stop");
        std::thread::sleep(Duration::from_millis(10));
    }
    let (code, _, err) = rig.ctl(&["daemon", "ping"]);
    assert_eq!(code, 3, "{err}");
}

#[test]
fn usage_and_connection_errors_have_their_own_exit_codes() {
    let run = |args: &[&str]| {
        let output = Command::new(env!("CARGO_BIN_EXE_otapctl"))
            .args(args)
            .output()
            .unwrap();
        (
            output.status.code().unwrap_or(-1),
            String::from_utf8_lossy(&output.stdout).into_owned(),
            String::from_utf8_lossy(&output.stderr).into_owned(),
        )
    };
    let (code, out, _) = run(&["--help"]);
    assert_eq!(code, 0);
    assert!(out.contains("snapshot rollback"), "{out}");
    let (code, out, _) = run(&["--version"]);
    assert_eq!(code, 0);
    assert!(out.starts_with("otapctl "), "{out}");
    let (code, _, err) = run(&[]);
    assert_eq!(code, 2);
    assert!(err.contains("no command"), "{err}");
    let (code, _, err) = run(&["volume", "create", "--name", "x"]);
    assert_eq!(code, 2);
    assert!(err.contains("--size"), "{err}");
    let (code, _, err) = run(&["--control", "tcp:127.0.0.1:1", "daemon", "ping"]);
    assert_eq!(code, 3, "{err}");
    assert!(err.contains("cannot reach dbladed"), "{err}");
}
