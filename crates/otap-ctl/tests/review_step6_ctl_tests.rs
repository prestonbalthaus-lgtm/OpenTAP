//! The Step 6 adversarial review (`docs/reviews/2026-10-16-step6-qa-adversarial.md`):
//! `otapctl`'s findings. Each test reproduced its finding before the fix
//! and pins the fixed behaviour.

use otap_control::{ReplicationReport, ReplicationState, Request, Response};
use otap_ctl::{parse, render, Parsed};

fn parsed(args: &[&str]) -> Result<Parsed, String> {
    let args: Vec<String> = args.iter().map(|s| (*s).to_string()).collect();
    parse(&args)
}

/// R2: a value given to a switch (`--name=value`) was stored and the switch
/// read as present, whatever the value: `device reap --stale=false`
/// deleted every stale device, `pool format --force=no` overwrote a pool,
/// `--delete-newer=false` deleted the newer snapshots. A switch given a
/// value is refused now.
#[test]
fn r2_a_switch_given_a_value_is_refused_not_turned_on() {
    for args in [
        &["device", "reap", "--stale=false"][..],
        &["pool", "format", "--device", "d.img", "--force=no"],
        &[
            "snapshot",
            "rollback",
            "--volume",
            "v",
            "--name",
            "s",
            "--delete-newer=false",
        ],
        &["replicate", "status", "--json=0"],
    ] {
        match parsed(args) {
            Err(e) => assert!(e.contains("takes no value"), "{args:?}: {e}"),
            Ok(Parsed::Run(cmd)) => panic!("{args:?} parsed as {:?}", cmd.request),
            Ok(other) => panic!("{args:?} parsed as {other:?}"),
        }
    }
    // The switches themselves still work.
    match parsed(&["device", "reap", "--stale"]) {
        Ok(Parsed::Run(cmd)) => assert_eq!(
            cmd.request,
            Request::DeviceReap {
                dev_id: None,
                stale: true
            }
        ),
        other => panic!("{other:?}"),
    }
}

/// R6: a replication report carries text from the peer (a refusal's
/// message, the names in its handshake), and `otapctl` printed it as it
/// came: an unauthenticated peer could write terminal escape sequences to
/// the operator's screen. Control characters are escaped now.
#[test]
fn r6_peer_text_in_a_replication_report_reaches_the_terminal_escaped() {
    let report = ReplicationReport {
        id: 3,
        role: "source".into(),
        mode: "push".into(),
        state: ReplicationState::Failed,
        phase: "connecting".into(),
        volume: "data".into(),
        volume_index: Some(0),
        peer: "127.0.0.1:7422".into(),
        remote_volume: "mirror\u{1b}[2J".into(),
        snapshot: Some("s1\u{7}".into()),
        snapshot_uuid: None,
        base_uuid: None,
        chunks: 0,
        chunks_sent: 0,
        chunks_landed: 0,
        bytes: 0,
        bytes_sent: 0,
        bytes_landed: 0,
        frames_sent: 0,
        frames_received: 0,
        connections: 1,
        retries: 0,
        resumed_chunks: 0,
        resent_chunks: 0,
        duplicate_chunks: 0,
        landed_contig: 0,
        digest_verified: None,
        last_error: Some("the peer refused: \u{1b}]0;owned\u{7}\u{1b}[31mred".into()),
    };
    for (response, args) in [
        (
            Response::Replication(report.clone()),
            &["replicate", "status", "3"][..],
        ),
        (
            Response::Replications {
                jobs: vec![report.clone()],
            },
            &["replicate", "status"],
        ),
    ] {
        let Ok(Parsed::Run(cmd)) = parsed(args) else {
            panic!("{args:?} parses");
        };
        let mut out = Vec::new();
        render(&response, &cmd, &mut out).expect("rendered");
        let text = String::from_utf8(out).expect("utf-8");
        assert!(
            !text.chars().any(|c| c.is_control() && c != '\n'),
            "a control character reached the terminal: {text:?}"
        );
        assert!(text.contains("\\u{"), "the escape is shown: {text}");
    }
}
