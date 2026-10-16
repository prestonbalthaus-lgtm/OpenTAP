//! Step 6B: the verification requests and reply of the control protocol
//! (`PoolVerify`, `PoolVerifyStatus`, `PoolVerifyCancel`, `Verify` with its
//! `VerifyReport` and `VerifyFinding`s). They are new variants, so
//! protocol 1 stays: an older daemon answers the requests `BadRequest` (it
//! cannot decode them), and every request, reply and report refuses an
//! unknown field.

use otap_control::{
    read_frame, write_frame, FindingSeverity, Reply, Request, Response, VerifyFinding,
    VerifyReport, VerifyState, VerifyVerdict, PROTOCOL_VERSION,
};

fn finding() -> VerifyFinding {
    VerifyFinding {
        kind: "map_page".into(),
        severity: FindingSeverity::Error,
        pba: Some(4096),
        segment: Some(3),
        volume_index: Some(1),
        lba: Some(1024),
        snapshot: None,
        detail: "map page 1 of volume 1 does not verify".into(),
    }
}

fn report() -> VerifyReport {
    VerifyReport {
        state: VerifyState::Done,
        verdict: Some(VerifyVerdict::Damaged),
        cp_seq: 77,
        phase: "finished".into(),
        progress: 1.0,
        superblock_slots: 4,
        regions: 20,
        map_pages: 9,
        blocks: 400,
        holes: 12,
        stripes: 31,
        segments: 15,
        bytes_read: 3 << 20,
        errors: 1,
        warnings: 0,
        findings: vec![finding()],
        reason: None,
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
fn the_verification_requests_round_trip_and_refuse_unknown_fields() {
    assert_eq!(
        round_trip(&Request::PoolVerify {}),
        serde_json::json!({ "op": "pool_verify" })
    );
    assert_eq!(
        round_trip(&Request::PoolVerifyStatus {}),
        serde_json::json!({ "op": "pool_verify_status" })
    );
    assert_eq!(
        round_trip(&Request::PoolVerifyCancel {}),
        serde_json::json!({ "op": "pool_verify_cancel" })
    );
    // Struct variants with no field, so an unknown field is refused (a
    // unit variant would ignore it).
    for json in [
        r#"{"op":"pool_verify","wait":true}"#,
        r#"{"op":"pool_verify_status","x":1}"#,
        r#"{"op":"pool_verify_cancel","all":true}"#,
    ] {
        assert!(serde_json::from_str::<Request>(json).is_err(), "{json}");
    }
}

#[test]
fn the_verification_report_round_trips_with_every_word() {
    let json = round_trip(&Response::Verify(report()));
    assert_eq!(json["kind"], "verify");
    assert_eq!(json["state"], "done");
    assert_eq!(json["verdict"], "damaged");
    assert_eq!(json["findings"][0]["severity"], "error");
    assert_eq!(json["findings"][0]["kind"], "map_page");
    for (state, word) in [
        (VerifyState::Running, "running"),
        (VerifyState::Done, "done"),
        (VerifyState::Cancelled, "cancelled"),
        (VerifyState::Aborted, "aborted"),
    ] {
        let json = round_trip(&VerifyReport { state, ..report() });
        assert_eq!(json["state"], word);
    }
    for (verdict, word) in [
        (VerifyVerdict::Clean, "clean"),
        (VerifyVerdict::Warnings, "warnings"),
        (VerifyVerdict::Damaged, "damaged"),
        (VerifyVerdict::Incomplete, "incomplete"),
    ] {
        let json = round_trip(&VerifyReport {
            verdict: Some(verdict),
            ..report()
        });
        assert_eq!(json["verdict"], word);
    }
    let json = round_trip(&VerifyReport {
        verdict: None,
        state: VerifyState::Running,
        ..report()
    });
    assert_eq!(json["verdict"], serde_json::Value::Null);
    let json = round_trip(&VerifyFinding {
        severity: FindingSeverity::Warning,
        ..finding()
    });
    assert_eq!(json["severity"], "warning");
}

#[test]
fn the_report_and_its_findings_refuse_unknown_fields() {
    let mut json = serde_json::to_value(report()).unwrap();
    json["extra"] = serde_json::json!(1);
    assert!(serde_json::from_value::<VerifyReport>(json).is_err());
    let mut json = serde_json::to_value(finding()).unwrap();
    json["extra"] = serde_json::json!(1);
    assert!(serde_json::from_value::<VerifyFinding>(json).is_err());
    let mut json = serde_json::to_value(Response::Verify(report())).unwrap();
    json["findings"][0]["extra"] = serde_json::json!(1);
    assert!(serde_json::from_value::<Response>(json).is_err());
    assert_eq!(PROTOCOL_VERSION, 1);
}

#[test]
fn a_verification_reply_crosses_the_wire_in_one_frame() {
    let reply: Reply = Ok(Response::Verify(report()));
    let mut buf = Vec::new();
    write_frame(&mut buf, &reply).unwrap();
    let back: Option<Reply> = read_frame(&mut buf.as_slice()).unwrap();
    assert_eq!(back, Some(reply));
}
