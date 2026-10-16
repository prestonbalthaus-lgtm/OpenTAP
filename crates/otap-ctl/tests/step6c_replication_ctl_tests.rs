//! Step 6C: `otapctl replicate` against two in-process `dbladed` daemons on
//! loopback TCP: a push that waits for its job (exit 0), the status table
//! and one job, both cursors, JSON, `--no-wait`, a push the sink refuses
//! (exit 4), cancel and release; and the command line's own refusals.

use std::path::PathBuf;
use std::process::Command;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::Duration;

use otap_control::{Endpoint, Request};
use otap_ctl::{parse, Parsed};
use otap_daemon::{serve, Daemon, DaemonConfig, DaemonHandle, Server};

struct Rig {
    endpoint: String,
    replication: Option<String>,
    dir: PathBuf,
    stop: Arc<AtomicBool>,
    handle: Option<DaemonHandle>,
    engine: Option<JoinHandle<()>>,
    server: Option<JoinHandle<std::io::Result<()>>>,
}

impl Rig {
    fn new(name: &str, listen: bool) -> Self {
        let dir = std::env::temp_dir().join(format!("otap-ctl-repl-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let cfg = DaemonConfig {
            control: Endpoint::parse("tcp:127.0.0.1:0").unwrap(),
            ublk: false,
            op_timeout: Duration::from_secs(60),
            state_dir: dir.join("state"),
            replication_listen: listen.then(|| "127.0.0.1:0".to_string()),
            ..DaemonConfig::default()
        };
        let stop = Arc::new(AtomicBool::new(false));
        let server = Server::bind(&cfg.control).unwrap();
        let endpoint = server.endpoint().to_string();
        let (handle, engine) = Daemon::spawn(cfg, Arc::clone(&stop)).unwrap();
        let replication = handle.replication_addr().map(|a| a.to_string());
        let server = {
            let handle = handle.clone();
            let stop = Arc::clone(&stop);
            std::thread::spawn(move || serve(server, &handle, &stop))
        };
        Self {
            endpoint,
            replication,
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

    fn booted(name: &str, listen: bool) -> Self {
        let rig = Self::new(name, listen);
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
        rig
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

fn request(args: &[&str]) -> Result<Request, String> {
    let args: Vec<String> = args.iter().map(|s| (*s).to_string()).collect();
    match parse(&args)? {
        Parsed::Run(cmd) => Ok(cmd.request),
        other => panic!("{other:?}"),
    }
}

#[test]
fn the_replicate_verbs_parse() {
    assert_eq!(
        request(&[
            "replicate",
            "push",
            "data",
            "s1",
            "--to",
            "127.0.0.1:7422/backup"
        ]),
        Ok(Request::ReplicatePush {
            volume: "data".into(),
            snapshot: "s1".into(),
            to: "127.0.0.1:7422".into(),
            destination: Some("backup".into()),
        })
    );
    assert_eq!(
        request(&["replicate", "push", "data", "#4", "--to", "[::1]:7422"]),
        Ok(Request::ReplicatePush {
            volume: "data".into(),
            snapshot: "#4".into(),
            to: "[::1]:7422".into(),
            destination: None,
        })
    );
    assert_eq!(
        request(&[
            "replicate",
            "pull",
            "copy",
            "s1",
            "--from",
            "host:7422/data"
        ]),
        Ok(Request::ReplicatePull {
            volume: "copy".into(),
            snapshot: "s1".into(),
            from: "host:7422".into(),
            source: Some("data".into()),
        })
    );
    assert_eq!(
        request(&["replicate", "status"]),
        Ok(Request::ReplicateStatus { id: None })
    );
    assert_eq!(
        request(&["replicate", "status", "7"]),
        Ok(Request::ReplicateStatus { id: Some(7) })
    );
    assert_eq!(
        request(&["replicate", "cursor", "copy"]),
        Ok(Request::ReplicateCursor {
            volume: "copy".into()
        })
    );
    assert_eq!(
        request(&["replicate", "cancel", "7"]),
        Ok(Request::ReplicateCancel { id: 7 })
    );
    assert_eq!(
        request(&["replicate", "release", "copy"]),
        Ok(Request::ReplicateRelease {
            volume: "copy".into()
        })
    );
    let args: Vec<String> = [
        "replicate",
        "push",
        "data",
        "s1",
        "--to",
        "h:1",
        "--no-wait",
    ]
    .iter()
    .map(|s| (*s).to_string())
    .collect();
    match parse(&args) {
        Ok(Parsed::Run(cmd)) => assert!(!cmd.wait_job),
        other => panic!("{other:?}"),
    }
    let args: Vec<String> = ["replicate", "push", "data", "s1", "--to", "h:1"]
        .iter()
        .map(|s| (*s).to_string())
        .collect();
    match parse(&args) {
        Ok(Parsed::Run(cmd)) => assert!(cmd.wait_job),
        other => panic!("{other:?}"),
    }
    for (bad, why) in [
        (&["replicate", "push", "data", "--to", "h:1"][..], "needs 2"),
        (&["replicate", "push", "data", "s1"][..], "--to"),
        (
            &["replicate", "push", "data", "s1", "--to", "nohost"][..],
            "host:port",
        ),
        (
            &["replicate", "push", "data", "s1", "--to", "h:1/"][..],
            "empty volume",
        ),
        (
            &["replicate", "push", "data", "s1", "--to", "h:99999"][..],
            "host:port",
        ),
        (
            &["replicate", "pull", "copy", "s1", "--to", "h:1"][..],
            "--from",
        ),
        (&["replicate", "status", "x"][..], "job number"),
        (&["replicate", "cancel"][..], "needs 1"),
        (&["replicate", "cursor", "a", "b"][..], "too many"),
        (&["replicate", "start"][..], "no verb"),
    ] {
        let e = request(bad).expect_err("refused");
        assert!(e.contains(why), "{bad:?}: {e}");
    }
}

#[test]
fn replicate_push_status_cursor_and_release_through_otapctl() {
    let src = Rig::booted("src", false);
    let dst = Rig::booted("dst", true);
    let to = dst.replication.clone().expect("the sink listens");
    src.ok(&["volume", "create", "--name", "data", "--size", "16M"]);
    src.ok(&[
        "io", "write", "--volume", "data", "--lba", "0", "--fill", "0x5a", "--blocks", "64",
    ]);
    src.ok(&["snapshot", "create", "--volume", "data", "--name", "s1"]);
    let target = format!("{to}/mirror");
    let out = src.ok(&["replicate", "push", "data", "s1", "--to", &target]);
    assert!(
        out.contains("replication job 1: push of data (source), done"),
        "{out}"
    );
    assert!(out.contains("s1 ("), "{out}");
    let table = src.ok(&["replicate", "status"]);
    assert!(
        table.contains("ID") && table.contains("push") && table.contains("done"),
        "{table}"
    );
    let one = src.ok(&["replicate", "status", "1"]);
    assert!(one.contains("replication job 1"), "{one}");
    let sink_table = dst.ok(&["replicate", "status"]);
    assert!(sink_table.contains("inbound push"), "{sink_table}");
    let c = dst.ok(&["replicate", "cursor", "mirror"]);
    assert!(
        c.contains("a mirror") && c.contains("s1") && c.contains("replica:    yes"),
        "{c}"
    );
    let c = src.ok(&["replicate", "cursor", "data"]);
    assert!(c.contains("a source") && c.contains("s1"), "{c}");
    let json = dst.ok(&["--json", "replicate", "cursor", "mirror"]);
    let v: serde_json::Value = serde_json::from_str(&json).expect("json");
    assert_eq!(v["kind"], "replication_cursor");
    assert_eq!(v["role"], "sink");
    assert_eq!(v["base_name"], "s1");
    // The mirror is offline and stays so.
    let (code, _, err) = dst.ctl(&["volume", "online", "--name", "mirror"]);
    assert_eq!(code, 1, "{err}");
    assert!(err.contains("replication destination"), "{err}");
    // A push the sink refuses ends failed: exit 4.
    dst.ok(&["volume", "create", "--name", "live", "--size", "16M"]);
    let refused = format!("{to}/live");
    let (code, out, err) = src.ctl(&["replicate", "push", "data", "s1", "--to", &refused]);
    assert_eq!(code, 4, "stdout: {out}\nstderr: {err}");
    assert!(out.contains("FAILED") && out.contains("online"), "{out}");
    // --no-wait answers at once with the running job.
    src.ok(&["snapshot", "create", "--volume", "data", "--name", "s2"]);
    let out = src.ok(&[
        "--json",
        "replicate",
        "push",
        "data",
        "s2",
        "--to",
        &target,
        "--no-wait",
    ]);
    let v: serde_json::Value = serde_json::from_str(&out).expect("json");
    assert_eq!(v["kind"], "replication");
    let id = v["id"].as_u64().expect("an id");
    let mut state = v["state"].as_str().unwrap_or("").to_string();
    for _ in 0..600 {
        if state != "running" {
            break;
        }
        std::thread::sleep(Duration::from_millis(100));
        let out = src.ok(&["--json", "replicate", "status", &id.to_string()]);
        let v: serde_json::Value = serde_json::from_str(&out).expect("json");
        state = v["state"].as_str().unwrap_or("").to_string();
    }
    assert_eq!(state, "done");
    // Release, then the mirror serves.
    let out = dst.ok(&["replicate", "release", "mirror"]);
    assert!(out.contains("volume mirror"), "{out}");
    dst.ok(&["volume", "online", "--name", "mirror"]);
    let c = dst.ok(&["replicate", "cursor", "mirror"]);
    assert!(c.contains("replica:    no"), "{c}");
    // Cancel of a finished job is refused.
    let (code, _, err) = src.ctl(&["replicate", "cancel", "1"]);
    assert_eq!(code, 1, "{err}");
    assert!(err.contains("not running"), "{err}");
}
