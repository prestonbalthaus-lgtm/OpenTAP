//! Step 6B: the device requests and replies of the control protocol
//! (`DeviceList`, `DeviceReap`, `Devices`, `Reaped`, `DeviceReport`). They
//! are new variants, so protocol 1 stays: an older daemon answers the
//! requests `BadRequest` (it cannot decode them), and every reply, report
//! and request still refuses an unknown field.

use otap_control::{
    read_frame, write_frame, ControlError, DeviceReport, DeviceState, ErrorCode, Reply, Request,
    Response, PROTOCOL_VERSION,
};

fn report(state: DeviceState) -> DeviceReport {
    DeviceReport {
        dev_id: 2,
        path: "/dev/ublkb2".into(),
        state,
        reason: "volume 4 of this pool, which has no device".into(),
        kernel_state: "quiesced".into(),
        pid: 812,
        pid_alive: false,
        volume_index: Some(4),
        volume: Some("db".into()),
        pool: Some("5eed0001".into()),
        size_bytes: Some(1 << 30),
        reissue: true,
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
fn the_device_requests_round_trip_under_their_op_tags() {
    let json = round_trip(&Request::DeviceList {});
    assert_eq!(json, serde_json::json!({ "op": "device_list" }));
    let json = round_trip(&Request::DeviceReap {
        dev_id: Some(7),
        stale: false,
    });
    assert_eq!(
        json,
        serde_json::json!({ "op": "device_reap", "dev_id": 7, "stale": false })
    );
    let json = round_trip(&Request::DeviceReap {
        dev_id: None,
        stale: true,
    });
    assert_eq!(json["dev_id"], serde_json::Value::Null);
    // An absent optional decodes as None, as everywhere in the protocol.
    let reap: Request = serde_json::from_str(r#"{"op":"device_reap","stale":true}"#).unwrap();
    assert_eq!(
        reap,
        Request::DeviceReap {
            dev_id: None,
            stale: true
        }
    );
}

#[test]
fn the_device_replies_round_trip_with_every_state() {
    for state in [
        DeviceState::Serving,
        DeviceState::Recoverable,
        DeviceState::Stale,
        DeviceState::Foreign,
    ] {
        let json = round_trip(&report(state));
        let word = json["state"].as_str().unwrap().to_string();
        assert_eq!(
            word,
            match state {
                DeviceState::Serving => "serving",
                DeviceState::Recoverable => "recoverable",
                DeviceState::Stale => "stale",
                DeviceState::Foreign => "foreign",
            }
        );
    }
    let devices: Reply = Ok(Response::Devices {
        devices: vec![report(DeviceState::Serving), report(DeviceState::Foreign)],
    });
    let json = round_trip(&devices);
    assert_eq!(json["Ok"]["kind"], "devices");
    assert_eq!(json["Ok"]["devices"][1]["state"], "foreign");
    let reaped: Reply = Ok(Response::Reaped {
        devices: Vec::new(),
    });
    let json = round_trip(&reaped);
    assert_eq!(
        json["Ok"],
        serde_json::json!({ "kind": "reaped", "devices": [] })
    );
    let none = report(DeviceState::Stale);
    let bare = DeviceReport {
        volume_index: None,
        volume: None,
        pool: None,
        size_bytes: None,
        ..none
    };
    round_trip(&bare);
}

#[test]
fn an_unknown_field_is_refused_in_every_new_shape() {
    for text in [
        r#"{"op":"device_list","all":true}"#,
        r#"{"op":"device_reap","dev_id":1,"stale":false,"force":true}"#,
        r#"{"op":"device_reap","dev":1,"stale":false}"#,
    ] {
        assert!(
            serde_json::from_str::<Request>(text).is_err(),
            "{text} decoded"
        );
    }
    let mut report = serde_json::to_value(report(DeviceState::Stale)).unwrap();
    report["kind"] = serde_json::json!("x");
    assert!(serde_json::from_value::<DeviceReport>(report).is_err());
    let reply = r#"{"Ok":{"kind":"devices","devices":[],"extra":1}}"#;
    assert!(serde_json::from_str::<Reply>(reply).is_err());
    let state = r#""dead""#;
    assert!(serde_json::from_str::<DeviceState>(state).is_err());
}

#[test]
fn the_new_variants_keep_protocol_1_and_frame_like_the_rest() {
    assert_eq!(PROTOCOL_VERSION, 1);
    let mut wire = Vec::new();
    write_frame(
        &mut wire,
        &Request::DeviceReap {
            dev_id: Some(3),
            stale: false,
        },
    )
    .unwrap();
    let back: Option<Request> = read_frame(&mut wire.as_slice()).unwrap();
    assert_eq!(
        back,
        Some(Request::DeviceReap {
            dev_id: Some(3),
            stale: false
        })
    );
    let refused: Reply = Err(ControlError::new(
        ErrorCode::Unsupported,
        "this daemon exposes no ublk devices",
    ));
    round_trip(&refused);
}
