//! Step 6B: the UAPI pieces the recovery re-attach and the reaper add to
//! `otap-sys`, checked against the saved v6.12 `ublk_cmd.h` and
//! `ublk_drv.c`: `DEL_DEV_ASYNC`'s builder, the recovery flags and states,
//! the reissue flag in the v0.1 requirement, the device-number bound and the
//! `ublk-char` class names the device list is read from.

use otap_sys::probe::Allowlist;
use otap_sys::ublk::{
    self, parse_char_name, CtrlCommand, UblksrvCtrlCmd, UblksrvCtrlDevInfo, UBLK_CHAR_CLASS_DIR,
    UBLK_MAX_UBLKS,
};
use otap_sys::{IntoBytes, REQUIRED_UBLK_FEATURES};

#[test]
fn del_dev_async_is_the_header_with_no_buffer_and_op_0x14() {
    // `UBLK_U_CMD_DEL_DEV_ASYNC` is `_IOR('u', 0x14, struct ublksrv_ctrl_cmd)`
    // (ublk_cmd.h:52).
    assert_eq!(ublk::UBLK_U_CMD_DEL_DEV_ASYNC, 0x8020_7514);
    let cmd = CtrlCommand::del_dev_async(9);
    assert_eq!(cmd.cmd_op, ublk::UBLK_U_CMD_DEL_DEV_ASYNC);
    assert_eq!(cmd.header.dev_id, 9);
    assert_eq!(cmd.header.queue_id, UblksrvCtrlCmd::NO_QUEUE);
    assert_eq!((cmd.header.addr, cmd.header.len), (0, 0));
    assert_eq!(cmd.header.data, [0]);
    let bytes = cmd.cmd_bytes();
    assert_eq!(&bytes[..32], cmd.header.as_bytes());
    assert!(bytes[32..].iter().all(|b| *b == 0));
    // The synchronous delete differs only in its op.
    let sync = CtrlCommand::del_dev(9);
    assert_eq!(sync.header, cmd.header);
    assert_ne!(sync.cmd_op, cmd.cmd_op);
}

#[test]
fn the_recovery_pair_carries_the_pid_where_start_dev_does() {
    // END_USER_RECOVERY names the new server's pid in data[0], as START_DEV
    // does (`ublk_ctrl_end_recovery`, `ublk_ctrl_start_dev`);
    // START_USER_RECOVERY names nothing.
    let start = CtrlCommand::start_user_recovery(3);
    let end = CtrlCommand::end_user_recovery(3, 4242);
    let dev = CtrlCommand::start_dev(3, 4242);
    assert_eq!(start.header.data, [0]);
    assert_eq!(end.header.data, dev.header.data);
    assert_eq!(start.cmd_op, 0xC020_7510);
    assert_eq!(end.cmd_op, 0xC020_7511);
}

#[test]
fn recovery_flags_and_states_are_the_headers() {
    // ublk_cmd.h:150-152 and 194-196.
    assert_eq!(ublk::UBLK_F_USER_RECOVERY, 1 << 3);
    assert_eq!(ublk::UBLK_F_USER_RECOVERY_REISSUE, 1 << 4);
    assert_eq!(
        (
            ublk::UBLK_S_DEV_DEAD,
            ublk::UBLK_S_DEV_LIVE,
            ublk::UBLK_S_DEV_QUIESCED
        ),
        (0, 1, 2)
    );
    assert_eq!(
        ublk::feature_name(ublk::UBLK_F_USER_RECOVERY_REISSUE),
        Some("UBLK_F_USER_RECOVERY_REISSUE")
    );
    // The stamp's field: 64 bits at offset 32 of the 64-byte device info,
    // which `ADD_DEV` copies through and `GET_DEV_INFO` returns.
    let info = UblksrvCtrlDevInfo {
        ublksrv_flags: 0x07A9_0123_4567_0003,
        ..UblksrvCtrlDevInfo::default()
    };
    let bytes = info.as_bytes();
    assert_eq!(bytes.len(), 64);
    assert_eq!(
        u64::from_ne_bytes(bytes[32..40].try_into().unwrap()),
        0x07A9_0123_4567_0003
    );
}

#[test]
fn the_v0_1_requirement_includes_the_reissue() {
    assert_ne!(
        REQUIRED_UBLK_FEATURES & ublk::UBLK_F_USER_RECOVERY_REISSUE,
        0
    );
    // A driver without it fails the check, naming the flag.
    let without = REQUIRED_UBLK_FEATURES & !ublk::UBLK_F_USER_RECOVERY_REISSUE;
    let allow = Allowlist::from_parts(Default::default(), without);
    let missing = allow
        .requires_ublk_features(REQUIRED_UBLK_FEATURES)
        .unwrap_err();
    assert!(
        missing.to_string().contains("UBLK_F_USER_RECOVERY_REISSUE"),
        "{missing}"
    );
    let with = Allowlist::from_parts(Default::default(), REQUIRED_UBLK_FEATURES);
    assert_eq!(with.requires_ublk_features(REQUIRED_UBLK_FEATURES), Ok(()));
}

#[test]
fn char_class_names_parse_to_device_numbers_below_the_bound() {
    assert_eq!(UBLK_CHAR_CLASS_DIR, "/sys/class/ublk-char");
    assert_eq!(UBLK_MAX_UBLKS, 1 << 20);
    assert_eq!(parse_char_name("ublkc0"), Some(0));
    assert_eq!(parse_char_name("ublkc17"), Some(17));
    assert_eq!(parse_char_name("ublkc1048575"), Some(UBLK_MAX_UBLKS - 1));
    for not in [
        "ublkc",
        "ublkc01",
        "ublkc-1",
        "ublkc+1",
        "ublkc1a",
        "ublkb1",
        "ublk-control",
        "ublkc1048576",
        "ublkc99999999999",
        " ublkc1",
    ] {
        assert_eq!(parse_char_name(not), None, "{not:?}");
    }
}
