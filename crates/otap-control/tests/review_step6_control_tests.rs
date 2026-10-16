//! The Step 6 adversarial review (`docs/reviews/2026-10-16-step6-qa-adversarial.md`):
//! the control protocol's findings. Each test reproduced its finding before
//! the fix and pins the fixed behaviour.

use otap_control::{Reply, Request, Response};

/// R1: every request without fields decoded with any unknown field, because
/// serde lets an internally tagged *unit* variant ignore the rest of the
/// map, whatever `deny_unknown_fields` says (the rule R3 of the Step 5
/// review set for every request). The variants are empty struct variants
/// now, as `DeviceList {}` already was; the JSON a client sends is the same.
#[test]
fn r1_a_request_without_fields_refuses_an_unknown_field() {
    for op in [
        "ping",
        "pool_status",
        "pool_sync",
        "pool_clean",
        "pool_stop",
        "daemon_stop",
        "volume_list",
        "device_list",
        "pool_verify",
        "pool_verify_status",
        "pool_verify_cancel",
    ] {
        let bare = format!(r#"{{"op":"{op}"}}"#);
        let request: Request =
            serde_json::from_str(&bare).unwrap_or_else(|e| panic!("{bare} must still decode: {e}"));
        assert_eq!(
            serde_json::to_string(&request).unwrap(),
            bare,
            "the wire form of {op} must not change"
        );
        let extra = format!(r#"{{"op":"{op}","force":true}}"#);
        assert!(
            serde_json::from_str::<Request>(&extra).is_err(),
            "{extra} decoded: an unknown field was ignored"
        );
    }
}

/// R1, the reply side: `Response::Ok` was a unit variant too.
#[test]
fn r1_the_ok_reply_refuses_an_unknown_field() {
    let bare = r#"{"kind":"ok"}"#;
    let response: Response = serde_json::from_str(bare).expect("ok decodes");
    assert_eq!(serde_json::to_string(&response).unwrap(), bare);
    assert!(serde_json::from_str::<Response>(r#"{"kind":"ok","extra":1}"#).is_err());
    assert!(serde_json::from_str::<Reply>(r#"{"Ok":{"kind":"ok","cp_seq":4}}"#).is_err());
}
