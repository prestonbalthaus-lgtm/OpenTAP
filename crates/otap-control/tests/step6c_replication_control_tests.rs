//! Step 6C: the replication requests and replies of the control protocol
//! (`ReplicatePush`, `ReplicatePull`, `ReplicateStatus`, `ReplicateCursor`,
//! `ReplicateCancel`, `ReplicateRelease`; `Replication`, `Replications`,
//! `ReplicationCursor`). New variants, so protocol 1 stays; every request,
//! reply and report refuses an unknown field and one that is missing.

use otap_control::{
    read_frame, write_frame, ReplicationCursorReport, ReplicationLandingReport, ReplicationReport,
    ReplicationState, Reply, Request, Response, PROTOCOL_VERSION,
};

fn job() -> ReplicationReport {
    ReplicationReport {
        id: 9,
        role: "sink".into(),
        mode: "inbound push".into(),
        state: ReplicationState::Done,
        phase: "finished".into(),
        volume: "mirror".into(),
        volume_index: Some(1),
        peer: "127.0.0.1:50122".into(),
        remote_volume: "data".into(),
        snapshot: None,
        snapshot_uuid: Some("00000000-0000-4000-8000-0000000000aa".into()),
        base_uuid: Some("00000000-0000-4000-8000-0000000000ab".into()),
        chunks: 3,
        chunks_sent: 0,
        chunks_landed: 3,
        bytes: 3 << 20,
        bytes_sent: 0,
        bytes_landed: 3 << 20,
        frames_sent: 6,
        frames_received: 9,
        connections: 1,
        retries: 0,
        resumed_chunks: 0,
        resent_chunks: 0,
        duplicate_chunks: 0,
        landed_contig: 0,
        digest_verified: Some(true),
        last_error: None,
    }
}

fn cursor() -> ReplicationCursorReport {
    ReplicationCursorReport {
        volume: "mirror".into(),
        volume_index: 1,
        role: "sink".into(),
        replica: true,
        base_uuid: None,
        base_name: None,
        base_cp: None,
        source_pool: None,
        source_volume_index: None,
        landing: Some(ReplicationLandingReport {
            cp_seq: 12,
            nonce: "0000000000000001".into(),
            target: "00000000-0000-4000-8000-0000000000aa".into(),
            chunks: 3,
            landed: 1,
            landed_contig: 1,
            sparse_ranges: 0,
        }),
        dirty: true,
        persisted: true,
    }
}

fn round_trip<T>(value: &T) -> serde_json::Value
where
    T: serde::Serialize + serde::de::DeserializeOwned + PartialEq + std::fmt::Debug,
{
    let json = serde_json::to_string(value).unwrap();
    let back: T = serde_json::from_str(&json).unwrap_or_else(|e| panic!("{json}: {e}"));
    assert_eq!(&back, value);
    serde_json::from_str(&json).unwrap()
}

#[test]
fn the_replication_requests_round_trip_with_their_tags() {
    assert_eq!(PROTOCOL_VERSION, 1, "variants only: the protocol stays 1");
    let v = round_trip(&Request::ReplicatePush {
        volume: "data".into(),
        snapshot: "s1".into(),
        to: "127.0.0.1:7422".into(),
        destination: None,
    });
    assert_eq!(v["op"], "replicate_push");
    assert!(v["destination"].is_null());
    let v = round_trip(&Request::ReplicatePull {
        volume: "copy".into(),
        snapshot: "s1".into(),
        from: "127.0.0.1:7422".into(),
        source: Some("data".into()),
    });
    assert_eq!(v["op"], "replicate_pull");
    assert_eq!(v["source"], "data");
    assert_eq!(
        round_trip(&Request::ReplicateStatus { id: None }),
        serde_json::json!({ "op": "replicate_status", "id": null })
    );
    // An absent optional field reads as none.
    assert_eq!(
        serde_json::from_str::<Request>(r#"{"op":"replicate_status"}"#).unwrap(),
        Request::ReplicateStatus { id: None }
    );
    assert_eq!(
        round_trip(&Request::ReplicateCursor { volume: "m".into() })["op"],
        "replicate_cursor"
    );
    assert_eq!(round_trip(&Request::ReplicateCancel { id: 4 })["id"], 4);
    assert_eq!(
        round_trip(&Request::ReplicateRelease { volume: "m".into() })["op"],
        "replicate_release"
    );
}

#[test]
fn the_replication_requests_refuse_unknown_and_missing_fields() {
    for json in [
        r#"{"op":"replicate_push","volume":"d","snapshot":"s","to":"h:1","destination":null,"tls":true}"#,
        r#"{"op":"replicate_pull","volume":"d","snapshot":"s","from":"h:1","source":null,"x":1}"#,
        r#"{"op":"replicate_status","id":null,"all":true}"#,
        r#"{"op":"replicate_cursor","volume":"m","verbose":true}"#,
        r#"{"op":"replicate_cancel","id":1,"force":true}"#,
        r#"{"op":"replicate_release","volume":"m","keep":true}"#,
        r#"{"op":"replicate_push","volume":"d","snapshot":"s"}"#,
        r#"{"op":"replicate_cancel"}"#,
        r#"{"op":"replicate_cursor"}"#,
    ] {
        assert!(serde_json::from_str::<Request>(json).is_err(), "{json}");
    }
}

#[test]
fn the_replication_replies_round_trip_and_refuse_unknown_fields() {
    let v = round_trip(&Response::Replication(job()));
    assert_eq!(v["kind"], "replication");
    assert_eq!(v["state"], "done");
    let v = round_trip(&Response::Replications {
        jobs: vec![
            job(),
            ReplicationReport {
                state: ReplicationState::Failed,
                last_error: Some("the peer refused".into()),
                ..job()
            },
        ],
    });
    assert_eq!(v["jobs"][1]["state"], "failed");
    let v = round_trip(&Response::ReplicationCursor(cursor()));
    assert_eq!(v["kind"], "replication_cursor");
    assert_eq!(v["landing"]["landed"], 1);
    let mut value = serde_json::to_value(Response::Replication(job())).unwrap();
    value["extra"] = serde_json::json!(1);
    assert!(serde_json::from_value::<Response>(value).is_err());
    let mut value = serde_json::to_value(Response::ReplicationCursor(cursor())).unwrap();
    value["landing"]["extra"] = serde_json::json!(1);
    assert!(serde_json::from_value::<Response>(value).is_err());
    for state in ["running", "done", "failed", "cancelled"] {
        let s: ReplicationState = serde_json::from_value(serde_json::json!(state)).unwrap();
        assert_eq!(serde_json::to_value(s).unwrap(), state);
    }
    assert!(serde_json::from_value::<ReplicationState>(serde_json::json!("paused")).is_err());
}

#[test]
fn a_replication_reply_crosses_the_frame_codec() {
    let reply: Reply = Ok(Response::Replications {
        jobs: vec![job(); 16],
    });
    let mut buf = Vec::new();
    write_frame(&mut buf, &reply).expect("writes");
    let back: Reply = read_frame(&mut buf.as_slice())
        .expect("reads")
        .expect("a frame");
    assert_eq!(back, reply);
}
