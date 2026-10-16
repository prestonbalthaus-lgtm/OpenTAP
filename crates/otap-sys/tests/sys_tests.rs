//! Host-independent tests of `otap-sys`: every constant against the v6.12
//! headers, the ioctl encodings, the user-copy offset (hand-computed values
//! and a property test against the driver's decoding), the command builders'
//! bytes, the structures' C layout, the allowlist checks, the fixed-buffer
//! layout and the arena's validation; the portable arena's alignment and
//! read/write behaviour where it is the implementation (off Linux); and, on
//! Linux, SQE construction that needs no ublk device.

use std::mem::{align_of, size_of};

use otap_sys::probe::{Allowlist, MissingCapability, OpcodeSet};
use otap_sys::ublk::{
    self, CtrlBuf, CtrlCommand, IoCommand, UblkParams, UblksrvCtrlCmd, UblksrvCtrlDevInfo,
    UblksrvIoCmd, UblksrvIoDesc,
};
use otap_sys::uring::{self, FixedBufferLayout, MAX_FIXED_BUFFERS, MAX_FIXED_BUFFER_LEN};
use otap_sys::{HugeArena, HugePageSize, IntoBytes, REQUIRED_OPCODES, REQUIRED_UBLK_FEATURES};
use proptest::prelude::*;

const MIB: usize = 1 << 20;
const GIB: usize = 1 << 30;

// ---------------------------------------------------------------------------
// ublk_cmd.h
// ---------------------------------------------------------------------------

#[test]
fn ioc_matches_the_kernel_macro() {
    // _IOWR('u', 6, 32) and _IOR('u', 0x13, 32), worked by hand.
    assert_eq!(ublk::ioc(3, u32::from(b'u'), 0x06, 32), 0xC020_7506);
    assert_eq!(ublk::ioc(ublk::IOC_READ, 0x75, 0x13, 32), 0x8020_7513);
    assert_eq!(ublk::ioc(ublk::IOC_NONE, 0, 0, 0), 0);
    assert_eq!(
        ublk::ioc(ublk::IOC_WRITE, 0xff, 0xff, (1 << 14) - 1),
        0x7fff_ffff
    );
    assert_eq!((ublk::IOC_NONE, ublk::IOC_WRITE, ublk::IOC_READ), (0, 1, 2));
}

#[test]
#[should_panic(expected = "size is 14 bits")]
fn ioc_rejects_an_oversized_argument() {
    let _ = ublk::ioc(ublk::IOC_READ, 0x75, 1, 1 << 14);
}

#[test]
fn control_commands_match_the_header() {
    let legacy = [
        (ublk::UBLK_CMD_GET_QUEUE_AFFINITY, 0x01),
        (ublk::UBLK_CMD_GET_DEV_INFO, 0x02),
        (ublk::UBLK_CMD_ADD_DEV, 0x04),
        (ublk::UBLK_CMD_DEL_DEV, 0x05),
        (ublk::UBLK_CMD_START_DEV, 0x06),
        (ublk::UBLK_CMD_STOP_DEV, 0x07),
        (ublk::UBLK_CMD_SET_PARAMS, 0x08),
        (ublk::UBLK_CMD_GET_PARAMS, 0x09),
        (ublk::UBLK_CMD_START_USER_RECOVERY, 0x10),
        (ublk::UBLK_CMD_END_USER_RECOVERY, 0x11),
        (ublk::UBLK_CMD_GET_DEV_INFO2, 0x12),
    ];
    for (value, expected) in legacy {
        assert_eq!(value, expected);
    }
    // (encoded, direction, nr): _IOR is 2, _IOWR is 3, size 32, type 'u'.
    let encoded = [
        (ublk::UBLK_U_CMD_GET_QUEUE_AFFINITY, 2, 0x01, 0x8020_7501),
        (ublk::UBLK_U_CMD_GET_DEV_INFO, 2, 0x02, 0x8020_7502),
        (ublk::UBLK_U_CMD_ADD_DEV, 3, 0x04, 0xC020_7504),
        (ublk::UBLK_U_CMD_DEL_DEV, 3, 0x05, 0xC020_7505),
        (ublk::UBLK_U_CMD_START_DEV, 3, 0x06, 0xC020_7506),
        (ublk::UBLK_U_CMD_STOP_DEV, 3, 0x07, 0xC020_7507),
        (ublk::UBLK_U_CMD_SET_PARAMS, 3, 0x08, 0xC020_7508),
        (ublk::UBLK_U_CMD_GET_PARAMS, 2, 0x09, 0x8020_7509),
        (ublk::UBLK_U_CMD_START_USER_RECOVERY, 3, 0x10, 0xC020_7510),
        (ublk::UBLK_U_CMD_END_USER_RECOVERY, 3, 0x11, 0xC020_7511),
        (ublk::UBLK_U_CMD_GET_DEV_INFO2, 2, 0x12, 0x8020_7512),
        (ublk::UBLK_U_CMD_GET_FEATURES, 2, 0x13, 0x8020_7513),
        (ublk::UBLK_U_CMD_DEL_DEV_ASYNC, 2, 0x14, 0x8020_7514),
    ];
    for (value, dir, nr, expected) in encoded {
        assert_eq!(value, expected, "nr {nr:#x}");
        assert_eq!(
            value,
            (dir << 30) | (32 << 16) | (u32::from(b'u') << 8) | nr
        );
        assert_eq!(value & 0xff, nr, "_IOC_NR");
        assert_eq!((value >> 8) & 0xff, u32::from(b'u'), "_IOC_TYPE");
        assert_eq!((value >> 16) & 0x3fff, 32, "_IOC_SIZE");
    }
    assert_eq!(ublk::UBLK_FEATURES_LEN, 8);
}

#[test]
fn io_commands_and_window_match_the_header() {
    assert_eq!(ublk::UBLK_IO_FETCH_REQ, 0x20);
    assert_eq!(ublk::UBLK_IO_COMMIT_AND_FETCH_REQ, 0x21);
    assert_eq!(ublk::UBLK_IO_NEED_GET_DATA, 0x22);
    assert_eq!(ublk::UBLK_U_IO_FETCH_REQ, 0xC010_7520);
    assert_eq!(ublk::UBLK_U_IO_COMMIT_AND_FETCH_REQ, 0xC010_7521);
    assert_eq!(ublk::UBLK_U_IO_NEED_GET_DATA, 0xC010_7522);
    assert_eq!(ublk::UBLK_IO_RES_OK, 0);
    assert_eq!(ublk::UBLK_IO_RES_NEED_GET_DATA, 1);
    assert_eq!(ublk::UBLK_IO_RES_ABORT, -19);

    assert_eq!(ublk::UBLKSRV_CMD_BUF_OFFSET, 0);
    assert_eq!(ublk::UBLKSRV_IO_BUF_OFFSET, 0x8000_0000);
    assert_eq!(ublk::UBLK_MAX_QUEUE_DEPTH, 4096);
    assert_eq!(ublk::UBLK_IO_BUF_OFF, 0);
    assert_eq!(ublk::UBLK_IO_BUF_BITS, 25);
    assert_eq!(ublk::UBLK_IO_BUF_BITS_MASK, 0x01ff_ffff);
    assert_eq!(ublk::UBLK_TAG_OFF, 25);
    assert_eq!(ublk::UBLK_TAG_BITS, 16);
    assert_eq!(ublk::UBLK_TAG_BITS_MASK, 0xffff);
    assert_eq!(ublk::UBLK_QID_OFF, 41);
    assert_eq!(ublk::UBLK_QID_BITS, 12);
    assert_eq!(ublk::UBLK_QID_BITS_MASK, 0xfff);
    assert_eq!(ublk::UBLK_MAX_NR_QUEUES, 4096);
    assert_eq!(ublk::UBLKSRV_IO_BUF_TOTAL_BITS, 53);
    assert_eq!(ublk::UBLKSRV_IO_BUF_TOTAL_SIZE, 1 << 53);
}

#[test]
fn feature_state_op_and_attribute_bits_match_the_header() {
    let features = [
        (
            ublk::UBLK_F_SUPPORT_ZERO_COPY,
            0,
            "UBLK_F_SUPPORT_ZERO_COPY",
        ),
        (
            ublk::UBLK_F_URING_CMD_COMP_IN_TASK,
            1,
            "UBLK_F_URING_CMD_COMP_IN_TASK",
        ),
        (ublk::UBLK_F_NEED_GET_DATA, 2, "UBLK_F_NEED_GET_DATA"),
        (ublk::UBLK_F_USER_RECOVERY, 3, "UBLK_F_USER_RECOVERY"),
        (
            ublk::UBLK_F_USER_RECOVERY_REISSUE,
            4,
            "UBLK_F_USER_RECOVERY_REISSUE",
        ),
        (ublk::UBLK_F_UNPRIVILEGED_DEV, 5, "UBLK_F_UNPRIVILEGED_DEV"),
        (ublk::UBLK_F_CMD_IOCTL_ENCODE, 6, "UBLK_F_CMD_IOCTL_ENCODE"),
        (ublk::UBLK_F_USER_COPY, 7, "UBLK_F_USER_COPY"),
        (ublk::UBLK_F_ZONED, 8, "UBLK_F_ZONED"),
    ];
    for (value, bit, name) in features {
        assert_eq!(value, 1 << bit, "{name}");
        assert_eq!(ublk::feature_name(value), Some(name));
    }
    assert_eq!(ublk::feature_name(1 << 9), None);
    assert_eq!(
        ublk::feature_name(ublk::UBLK_F_USER_COPY | ublk::UBLK_F_ZONED),
        None
    );

    assert_eq!(
        (
            ublk::UBLK_S_DEV_DEAD,
            ublk::UBLK_S_DEV_LIVE,
            ublk::UBLK_S_DEV_QUIESCED
        ),
        (0, 1, 2)
    );

    let ops = [
        ublk::UBLK_IO_OP_READ,
        ublk::UBLK_IO_OP_WRITE,
        ublk::UBLK_IO_OP_FLUSH,
        ublk::UBLK_IO_OP_DISCARD,
        ublk::UBLK_IO_OP_WRITE_SAME,
        ublk::UBLK_IO_OP_WRITE_ZEROES,
        ublk::UBLK_IO_OP_ZONE_OPEN,
        ublk::UBLK_IO_OP_ZONE_CLOSE,
        ublk::UBLK_IO_OP_ZONE_FINISH,
        ublk::UBLK_IO_OP_ZONE_APPEND,
        ublk::UBLK_IO_OP_ZONE_RESET_ALL,
        ublk::UBLK_IO_OP_ZONE_RESET,
        ublk::UBLK_IO_OP_REPORT_ZONES,
    ];
    assert_eq!(ops, [0, 1, 2, 3, 4, 5, 10, 11, 12, 13, 14, 15, 18]);

    let io_flags = [
        ublk::UBLK_IO_F_FAILFAST_DEV,
        ublk::UBLK_IO_F_FAILFAST_TRANSPORT,
        ublk::UBLK_IO_F_FAILFAST_DRIVER,
        ublk::UBLK_IO_F_META,
        ublk::UBLK_IO_F_FUA,
        ublk::UBLK_IO_F_NOUNMAP,
        ublk::UBLK_IO_F_SWAP,
    ];
    assert_eq!(
        io_flags,
        [1 << 8, 1 << 9, 1 << 10, 1 << 11, 1 << 13, 1 << 15, 1 << 16]
    );

    assert_eq!(
        [
            ublk::UBLK_ATTR_READ_ONLY,
            ublk::UBLK_ATTR_ROTATIONAL,
            ublk::UBLK_ATTR_VOLATILE_CACHE,
            ublk::UBLK_ATTR_FUA,
        ],
        [1, 2, 4, 8]
    );
    assert_eq!(
        [
            ublk::UBLK_PARAM_TYPE_BASIC,
            ublk::UBLK_PARAM_TYPE_DISCARD,
            ublk::UBLK_PARAM_TYPE_DEVT,
            ublk::UBLK_PARAM_TYPE_ZONED,
        ],
        [1, 2, 4, 8]
    );
}

#[test]
fn structures_have_the_c_sizes() {
    assert_eq!(size_of::<UblksrvCtrlCmd>(), 32);
    assert_eq!(size_of::<UblksrvCtrlDevInfo>(), 64);
    assert_eq!(size_of::<UblksrvIoDesc>(), 24);
    assert_eq!(size_of::<UblksrvIoCmd>(), 16);
    assert_eq!(size_of::<ublk::UblkParamBasic>(), 32);
    assert_eq!(size_of::<ublk::UblkParamDiscard>(), 20);
    assert_eq!(size_of::<ublk::UblkParamDevt>(), 16);
    assert_eq!(size_of::<ublk::UblkParamZoned>(), 32);
    assert_eq!(size_of::<UblkParams>(), 112);
    assert_eq!(align_of::<UblkParams>(), 8);
}

#[test]
fn ublk_params_new_sets_len_and_nothing_else() {
    let params = UblkParams::new();
    assert_eq!(params.len, 112);
    assert_eq!(params, UblkParams::default());
    let bytes = params.as_bytes();
    assert_eq!(bytes.len(), 112);
    assert_eq!(bytes[..4], 112u32.to_ne_bytes());
    assert!(bytes[4..].iter().all(|&b| b == 0));
}

#[test]
fn io_desc_accessors_follow_the_driver() {
    let desc = UblksrvIoDesc {
        op_flags: u32::from(ublk::UBLK_IO_OP_WRITE) | ublk::UBLK_IO_F_FUA | ublk::UBLK_IO_F_META,
        nr_sectors: 8,
        start_sector: 4096,
        addr: 0,
    };
    assert_eq!(desc.op(), ublk::UBLK_IO_OP_WRITE);
    assert_eq!(desc.flags(), ublk::UBLK_IO_F_FUA | ublk::UBLK_IO_F_META);
    assert!(desc.flags() & ublk::UBLK_IO_F_FUA != 0);
    assert!(desc.has_flag(ublk::UBLK_IO_F_FUA));
    assert!(desc.has_flag(ublk::UBLK_IO_F_FUA | ublk::UBLK_IO_F_META));
    assert!(!desc.has_flag(ublk::UBLK_IO_F_NOUNMAP));
    assert_eq!(desc.nr_sectors(), 8);
    assert_eq!(desc.nr_zones(), 8);

    let report = UblksrvIoDesc {
        op_flags: u32::from(ublk::UBLK_IO_OP_REPORT_ZONES),
        nr_sectors: 3,
        ..UblksrvIoDesc::default()
    };
    assert_eq!(report.op(), ublk::UBLK_IO_OP_REPORT_ZONES);
    assert_eq!(report.flags(), 0);
    assert_eq!(report.nr_zones(), 3);

    let commit = UblksrvIoCmd {
        q_id: 0,
        tag: 0,
        result: 4096,
        addr: 0x1234,
    };
    assert_eq!(commit.zone_append_lba(), 0x1234);
}

// ---------------------------------------------------------------------------
// The user-copy offset
// ---------------------------------------------------------------------------

#[test]
fn user_copy_offset_hand_computed() {
    let cases: [((u16, u16, u32), u64); 7] = [
        ((0, 0, 0), 0x0000_0000_8000_0000),
        ((0, 0, 4096), 0x0000_0000_8000_1000),
        ((0, 1, 0), 0x0000_0000_8200_0000),
        ((1, 0, 0), 0x0000_0200_8000_0000),
        ((2, 5, 512), 0x0000_0400_8a00_0200),
        ((0, 4095, 0x01ff_ffff), 0x0000_0020_7fff_ffff),
        ((4095, 0xffff, 0x01ff_ffff), 0x0020_0000_7fff_ffff),
    ];
    for ((q_id, tag, buf_off), expected) in cases {
        assert_eq!(
            ublk::user_copy_offset(q_id, tag, buf_off),
            expected,
            "({q_id}, {tag}, {buf_off})"
        );
        assert_eq!(
            ublk::checked_user_copy_offset(q_id, tag, buf_off),
            Some(expected)
        );
        assert_eq!(
            ublk::split_user_copy_offset(expected),
            Some((q_id, tag, buf_off))
        );
    }
    // The last byte of the window.
    assert_eq!(
        ublk::user_copy_offset(4095, 0xffff, 0x01ff_ffff),
        ublk::UBLKSRV_IO_BUF_OFFSET + ublk::UBLKSRV_IO_BUF_TOTAL_SIZE - 1
    );
}

#[test]
fn user_copy_offset_rejects_out_of_range_fields() {
    assert_eq!(ublk::checked_user_copy_offset(4096, 0, 0), None);
    assert_eq!(ublk::checked_user_copy_offset(u16::MAX, 0, 0), None);
    assert_eq!(ublk::checked_user_copy_offset(0, 0, 1 << 25), None);
    assert_eq!(ublk::checked_user_copy_offset(0, 0, u32::MAX), None);
    assert_eq!(ublk::split_user_copy_offset(0), None);
    assert_eq!(ublk::split_user_copy_offset(0x7fff_ffff), None);
    assert_eq!(
        ublk::split_user_copy_offset(ublk::UBLKSRV_IO_BUF_OFFSET + ublk::UBLKSRV_IO_BUF_TOTAL_SIZE),
        None
    );
    assert_eq!(ublk::split_user_copy_offset(u64::MAX), None);
}

#[test]
#[should_panic(expected = "q_id must be below 4096")]
fn user_copy_offset_panics_on_a_queue_out_of_range() {
    let _ = ublk::user_copy_offset(4096, 0, 0);
}

#[test]
#[should_panic(expected = "buf_off below 32 MiB")]
fn user_copy_offset_panics_on_a_byte_offset_out_of_range() {
    let _ = ublk::user_copy_offset(0, 0, 1 << 25);
}

/// The driver's decoding, written from `ublk_pos_to_hwq`, `ublk_pos_to_tag`
/// and `ublk_pos_to_buf_off` independently of the crate.
fn driver_decode(pos: u64) -> (u64, u64, u64) {
    let rel = pos - 0x8000_0000;
    ((rel >> 41) & 0xfff, (rel >> 25) & 0xffff, rel & 0x01ff_ffff)
}

proptest! {
    #[test]
    fn user_copy_offset_round_trips_through_the_driver_decoding(
        q_id in 0u16..4096,
        tag in any::<u16>(),
        buf_off in 0u32..(1 << 25),
    ) {
        let pos = ublk::user_copy_offset(q_id, tag, buf_off);
        prop_assert!(pos >= ublk::UBLKSRV_IO_BUF_OFFSET);
        prop_assert!(pos < ublk::UBLKSRV_IO_BUF_OFFSET + ublk::UBLKSRV_IO_BUF_TOTAL_SIZE);
        prop_assert_eq!(
            driver_decode(pos),
            (u64::from(q_id), u64::from(tag), u64::from(buf_off))
        );
        prop_assert_eq!(ublk::split_user_copy_offset(pos), Some((q_id, tag, buf_off)));
        prop_assert_eq!(ublk::checked_user_copy_offset(q_id, tag, buf_off), Some(pos));
    }

    #[test]
    fn user_copy_offset_is_injective_and_ordered(
        a in (0u16..4096, any::<u16>(), 0u32..(1 << 25)),
        b in (0u16..4096, any::<u16>(), 0u32..(1 << 25)),
    ) {
        let pa = ublk::user_copy_offset(a.0, a.1, a.2);
        let pb = ublk::user_copy_offset(b.0, b.1, b.2);
        // Field order (q_id, tag, buf_off) is the numeric order of positions.
        prop_assert_eq!(a.cmp(&b), pa.cmp(&pb));
    }

    #[test]
    fn checked_user_copy_offset_refuses_exactly_the_out_of_range_fields(
        q_id in any::<u16>(),
        tag in any::<u16>(),
        buf_off in any::<u32>(),
    ) {
        let in_range = q_id < 4096 && buf_off < (1 << 25);
        prop_assert_eq!(
            ublk::checked_user_copy_offset(q_id, tag, buf_off).is_some(),
            in_range
        );
    }

    #[test]
    fn split_user_copy_offset_inverts_every_window_position(rel in 0u64..(1 << 53)) {
        let pos = ublk::UBLKSRV_IO_BUF_OFFSET + rel;
        let (q_id, tag, buf_off) = ublk::split_user_copy_offset(pos).unwrap();
        prop_assert_eq!(ublk::user_copy_offset(q_id, tag, buf_off), pos);
    }
}

// ---------------------------------------------------------------------------
// Command builders
// ---------------------------------------------------------------------------

/// The 80 command bytes a control command must produce, assembled field by
/// field from the C layout of `struct ublksrv_ctrl_cmd`.
fn expected_ctrl_bytes(dev_id: u32, queue_id: u16, len: u16, addr: u64, data0: u64) -> [u8; 80] {
    let mut cmd = [0u8; 80];
    cmd[0..4].copy_from_slice(&dev_id.to_ne_bytes());
    cmd[4..6].copy_from_slice(&queue_id.to_ne_bytes());
    cmd[6..8].copy_from_slice(&len.to_ne_bytes());
    cmd[8..16].copy_from_slice(&addr.to_ne_bytes());
    cmd[16..24].copy_from_slice(&data0.to_ne_bytes());
    // dev_path_len, pad, reserved and the 48 bytes after the header are 0.
    cmd
}

#[test]
fn control_builders_fill_the_header_and_op() {
    let info = CtrlBuf {
        addr: 0x7f00_1234_5000,
        len: 64,
    };
    let params = CtrlBuf {
        addr: 0x7f00_1234_6000,
        len: 112,
    };
    let no_queue = UblksrvCtrlCmd::NO_QUEUE;
    let cases = [
        (
            CtrlCommand::add_dev(UblksrvCtrlCmd::ANY_DEV, info),
            ublk::UBLK_U_CMD_ADD_DEV,
            expected_ctrl_bytes(u32::MAX, no_queue, 64, info.addr, 0),
        ),
        (
            CtrlCommand::del_dev(7),
            ublk::UBLK_U_CMD_DEL_DEV,
            expected_ctrl_bytes(7, no_queue, 0, 0, 0),
        ),
        (
            CtrlCommand::start_dev(7, 4321),
            ublk::UBLK_U_CMD_START_DEV,
            expected_ctrl_bytes(7, no_queue, 0, 0, 4321),
        ),
        (
            CtrlCommand::stop_dev(7),
            ublk::UBLK_U_CMD_STOP_DEV,
            expected_ctrl_bytes(7, no_queue, 0, 0, 0),
        ),
        (
            CtrlCommand::set_params(7, params),
            ublk::UBLK_U_CMD_SET_PARAMS,
            expected_ctrl_bytes(7, no_queue, 112, params.addr, 0),
        ),
        (
            CtrlCommand::get_params(7, params),
            ublk::UBLK_U_CMD_GET_PARAMS,
            expected_ctrl_bytes(7, no_queue, 112, params.addr, 0),
        ),
        (
            CtrlCommand::get_dev_info(7, info),
            ublk::UBLK_U_CMD_GET_DEV_INFO,
            expected_ctrl_bytes(7, no_queue, 64, info.addr, 0),
        ),
        (
            CtrlCommand::get_features(0x7f00_1234_7000),
            ublk::UBLK_U_CMD_GET_FEATURES,
            expected_ctrl_bytes(u32::MAX, no_queue, 8, 0x7f00_1234_7000, 0),
        ),
        (
            CtrlCommand::start_user_recovery(7),
            ublk::UBLK_U_CMD_START_USER_RECOVERY,
            expected_ctrl_bytes(7, no_queue, 0, 0, 0),
        ),
        (
            CtrlCommand::end_user_recovery(7, 4321),
            ublk::UBLK_U_CMD_END_USER_RECOVERY,
            expected_ctrl_bytes(7, no_queue, 0, 0, 4321),
        ),
    ];
    for (cmd, op, bytes) in cases {
        assert_eq!(cmd.cmd_op, op, "{cmd:?}");
        assert_eq!(cmd.cmd_bytes(), bytes, "{cmd:?}");
        assert_eq!(cmd.header.as_bytes(), &bytes[..32]);
        assert_eq!(cmd.header.dev_path_len, 0);
        assert_eq!(cmd.header.pad, 0);
        assert_eq!(cmd.header.reserved, 0);
    }
}

#[test]
fn generic_control_builder_takes_an_optional_buffer() {
    let with = CtrlCommand::new(
        ublk::UBLK_U_CMD_GET_DEV_INFO2,
        3,
        Some(CtrlBuf {
            addr: 0x1000,
            len: 80,
        }),
    );
    assert_eq!(with.cmd_op, ublk::UBLK_U_CMD_GET_DEV_INFO2);
    assert_eq!(
        with.cmd_bytes(),
        expected_ctrl_bytes(3, UblksrvCtrlCmd::NO_QUEUE, 80, 0x1000, 0)
    );
    let without = CtrlCommand::new(ublk::UBLK_U_CMD_DEL_DEV_ASYNC, 3, None);
    assert_eq!(
        without.cmd_bytes(),
        expected_ctrl_bytes(3, UblksrvCtrlCmd::NO_QUEUE, 0, 0, 0)
    );
    assert_eq!(CtrlCommand::CMD_LEN, 80);
}

#[test]
fn io_builders_keep_addr_zero() {
    let fetch = IoCommand::fetch(2, 17);
    assert_eq!(fetch.cmd_op, ublk::UBLK_U_IO_FETCH_REQ);
    assert_eq!(
        fetch.cmd,
        UblksrvIoCmd {
            q_id: 2,
            tag: 17,
            result: 0,
            addr: 0
        }
    );
    let mut bytes = [0u8; 16];
    bytes[0..2].copy_from_slice(&2u16.to_ne_bytes());
    bytes[2..4].copy_from_slice(&17u16.to_ne_bytes());
    assert_eq!(fetch.cmd_bytes(), bytes);

    let commit = IoCommand::commit_and_fetch(2, 17, -5);
    assert_eq!(commit.cmd_op, ublk::UBLK_U_IO_COMMIT_AND_FETCH_REQ);
    assert_eq!(commit.cmd.addr, 0);
    bytes[4..8].copy_from_slice(&(-5i32).to_ne_bytes());
    assert_eq!(commit.cmd_bytes(), bytes);
    assert_eq!(IoCommand::CMD_LEN, 16);
}

// ---------------------------------------------------------------------------
// io_uring.h opcodes and the allowlist
// ---------------------------------------------------------------------------

#[test]
fn opcodes_match_the_header_enum() {
    // enum io_uring_op, io_uring.h:203 to 265, in order.
    let names = [
        "NOP",
        "READV",
        "WRITEV",
        "FSYNC",
        "READ_FIXED",
        "WRITE_FIXED",
        "POLL_ADD",
        "POLL_REMOVE",
        "SYNC_FILE_RANGE",
        "SENDMSG",
        "RECVMSG",
        "TIMEOUT",
        "TIMEOUT_REMOVE",
        "ACCEPT",
        "ASYNC_CANCEL",
        "LINK_TIMEOUT",
        "CONNECT",
        "FALLOCATE",
        "OPENAT",
        "CLOSE",
        "FILES_UPDATE",
        "STATX",
        "READ",
        "WRITE",
        "FADVISE",
        "MADVISE",
        "SEND",
        "RECV",
        "OPENAT2",
        "EPOLL_CTL",
        "SPLICE",
        "PROVIDE_BUFFERS",
        "REMOVE_BUFFERS",
        "TEE",
        "SHUTDOWN",
        "RENAMEAT",
        "UNLINKAT",
        "MKDIRAT",
        "SYMLINKAT",
        "LINKAT",
        "MSG_RING",
        "FSETXATTR",
        "SETXATTR",
        "FGETXATTR",
        "GETXATTR",
        "SOCKET",
        "URING_CMD",
        "SEND_ZC",
        "SENDMSG_ZC",
        "READ_MULTISHOT",
        "WAITID",
        "FUTEX_WAIT",
        "FUTEX_WAKE",
        "FUTEX_WAITV",
        "FIXED_FD_INSTALL",
        "FTRUNCATE",
        "BIND",
        "LISTEN",
    ];
    assert_eq!(names.len(), usize::from(uring::IORING_OP_LAST));
    for (opcode, name) in (0u8..).zip(names) {
        assert_eq!(
            uring::opcode_name(opcode),
            Some(format!("IORING_OP_{name}").as_str())
        );
    }
    assert_eq!(uring::opcode_name(uring::IORING_OP_LAST), None);
    assert_eq!(uring::opcode_name(u8::MAX), None);

    let named = [
        (uring::IORING_OP_FSYNC, 3),
        (uring::IORING_OP_READ_FIXED, 4),
        (uring::IORING_OP_WRITE_FIXED, 5),
        (uring::IORING_OP_READ, 22),
        (uring::IORING_OP_WRITE, 23),
        (uring::IORING_OP_SEND, 26),
        (uring::IORING_OP_RECV, 27),
        (uring::IORING_OP_URING_CMD, 46),
        (uring::IORING_OP_SEND_ZC, 47),
        (uring::IORING_OP_SENDMSG_ZC, 48),
        (uring::IORING_OP_LISTEN, 57),
    ];
    for (value, expected) in named {
        assert_eq!(value, expected);
    }
}

#[test]
fn required_set_is_the_v0_1_set() {
    assert_eq!(REQUIRED_OPCODES, [22, 23, 5, 3, 46, 47, 27]);
    // Step 6B added UBLK_F_USER_RECOVERY_REISSUE (bit 4) to the set.
    assert_eq!(
        REQUIRED_UBLK_FEATURES,
        (1 << 7) | (1 << 3) | (1 << 4) | (1 << 6)
    );
}

#[test]
fn opcode_set_basics() {
    let empty = OpcodeSet::new();
    assert!(empty.is_empty());
    assert_eq!(empty.len(), 0);
    assert_eq!(empty, OpcodeSet::default());

    let set = OpcodeSet::from_opcodes(&[0, 63, 64, 127, 128, 255, 46, 46]);
    assert_eq!(set.len(), 7);
    for opcode in [0, 46, 63, 64, 127, 128, 255] {
        assert!(set.contains(opcode), "{opcode}");
    }
    assert!(!set.contains(1) && !set.contains(65) && !set.contains(254));
    assert_eq!(
        set.iter().collect::<Vec<_>>(),
        [0, 46, 63, 64, 127, 128, 255]
    );
    assert_eq!(set.without(46).len(), 6);
    assert_eq!(set.with(1).len(), 8);
    assert_eq!(set.iter().collect::<OpcodeSet>(), set);

    let mut grown = OpcodeSet::new();
    grown.insert(200);
    assert!(grown.contains(200));
    assert_eq!(format!("{grown:?}"), "{200}");

    let all: OpcodeSet = (0..=u8::MAX).collect();
    assert_eq!(all.len(), 256);
}

/// An allowlist of everything 6.12 defines and the features its driver
/// reports (`UBLK_F_ALL` without zero copy).
fn kernel_6_12() -> Allowlist {
    let opcodes: OpcodeSet = (0..uring::IORING_OP_LAST).collect();
    let features = (1 << 9) - 1 - ublk::UBLK_F_SUPPORT_ZERO_COPY;
    Allowlist::from_parts(opcodes, features)
}

#[test]
fn a_6_12_kernel_passes_the_v0_1_gate() {
    let allow = kernel_6_12();
    assert_eq!(allow.check_required(), Ok(()));
    assert_eq!(allow.requires(&REQUIRED_OPCODES), Ok(()));
    assert_eq!(allow.requires_ublk_features(REQUIRED_UBLK_FEATURES), Ok(()));
    assert!(allow.supports_opcode(uring::IORING_OP_URING_CMD));
    assert!(!allow.supports_opcode(uring::IORING_OP_LAST));
    assert!(allow.has_ublk_feature(ublk::UBLK_F_USER_COPY));
    assert!(allow.has_ublk_feature(REQUIRED_UBLK_FEATURES));
    assert!(!allow.has_ublk_feature(ublk::UBLK_F_SUPPORT_ZERO_COPY));
    assert!(allow.has_ublk_feature(0));
    assert_eq!(allow.opcodes().len(), 58);
}

#[test]
fn missing_capabilities_are_listed() {
    let full = kernel_6_12();
    let allow = Allowlist::from_parts(
        full.opcodes()
            .without(uring::IORING_OP_SEND_ZC)
            .without(uring::IORING_OP_URING_CMD),
        full.ublk_features() & !ublk::UBLK_F_USER_COPY,
    );
    let missing = allow.check_required().unwrap_err();
    assert_eq!(
        missing,
        MissingCapability {
            opcodes: vec![uring::IORING_OP_URING_CMD, uring::IORING_OP_SEND_ZC],
            ublk_features: ublk::UBLK_F_USER_COPY,
        }
    );
    assert!(!missing.is_empty());
    assert_eq!(
        missing.to_string(),
        "the kernel lacks IORING_OP_URING_CMD (46), IORING_OP_SEND_ZC (47), UBLK_F_USER_COPY"
    );

    let opcodes_only = allow.requires(&[uring::IORING_OP_READ, uring::IORING_OP_SEND_ZC]);
    assert_eq!(
        opcodes_only,
        Err(MissingCapability {
            opcodes: vec![uring::IORING_OP_SEND_ZC],
            ublk_features: 0,
        })
    );
    let features_only = allow.requires_ublk_features(ublk::UBLK_F_USER_COPY | ublk::UBLK_F_ZONED);
    assert_eq!(
        features_only,
        Err(MissingCapability {
            opcodes: vec![],
            ublk_features: ublk::UBLK_F_USER_COPY,
        })
    );

    let nothing = Allowlist::from_parts(OpcodeSet::new(), 0);
    let missing = nothing.requires(&[200]).unwrap_err();
    assert_eq!(missing.to_string(), "the kernel lacks io_uring opcode 200");
    let missing = nothing.requires_ublk_features(1 << 40).unwrap_err();
    assert_eq!(missing.to_string(), "the kernel lacks ublk feature bit 40");
    assert_eq!(nothing.requires(&[]), Ok(()));
    assert_eq!(
        MissingCapability::default().to_string(),
        "the kernel lacks nothing"
    );

    let io_err = std::io::Error::from(nothing.check_required().unwrap_err());
    assert_eq!(io_err.kind(), std::io::ErrorKind::Unsupported);
}

// ---------------------------------------------------------------------------
// Fixed buffers
// ---------------------------------------------------------------------------

#[test]
fn fixed_buffer_layout_cuts_at_one_gib() {
    assert_eq!(MAX_FIXED_BUFFER_LEN, GIB);
    assert_eq!(MAX_FIXED_BUFFERS, 16384);
    assert_eq!(FixedBufferLayout::new(0), None);

    let small = FixedBufferLayout::new(64 * MIB).unwrap();
    assert_eq!(
        (small.len(), small.chunk_len(), small.count()),
        (64 * MIB, 64 * MIB, 1)
    );
    assert!(!small.is_empty());
    assert_eq!(small.buffer(0), Some((0, 64 * MIB)));
    assert_eq!(small.buffer(1), None);

    // The 8 GiB production arena: eight 1 GiB buffers.
    let arena = FixedBufferLayout::new(8 * GIB).unwrap();
    assert_eq!((arena.chunk_len(), arena.count()), (GIB, 8));
    assert_eq!(arena.buffer(7), Some((7 * GIB, GIB)));
    assert_eq!(arena.buffer(8), None);

    // A length that is not a multiple of 1 GiB ends with a short buffer.
    let ragged = FixedBufferLayout::new(GIB + 2 * MIB).unwrap();
    assert_eq!(ragged.count(), 2);
    assert_eq!(ragged.buffer(1), Some((GIB, 2 * MIB)));

    // The table limit.
    assert!(FixedBufferLayout::new(MAX_FIXED_BUFFERS * GIB).is_some());
    assert_eq!(FixedBufferLayout::new(MAX_FIXED_BUFFERS * GIB + 1), None);
}

#[test]
fn fixed_buffer_index_of_refuses_straddles() {
    let arena = FixedBufferLayout::new(8 * GIB).unwrap();
    assert_eq!(arena.index_of(0, 4096), Some(0));
    assert_eq!(arena.index_of(GIB - 4096, 4096), Some(0));
    assert_eq!(arena.index_of(GIB - 4096, 8192), None);
    assert_eq!(arena.index_of(GIB, 8 * MIB), Some(1));
    assert_eq!(arena.index_of(3 * GIB + 5, 1), Some(3));
    assert_eq!(arena.index_of(8 * GIB - 1, 1), Some(7));
    assert_eq!(arena.index_of(8 * GIB - 1, 2), None);
    assert_eq!(arena.index_of(8 * GIB, 1), None);
    assert_eq!(arena.index_of(0, 0), None);
    assert_eq!(arena.index_of(usize::MAX, 2), None);
}

proptest! {
    #[test]
    fn fixed_buffers_tile_the_region(len in 1usize..=(64 * GIB)) {
        let layout = FixedBufferLayout::new(len).unwrap();
        let mut next = 0;
        for index in 0..layout.count() {
            let (start, buf_len) = layout.buffer(index).unwrap();
            prop_assert_eq!(start, next);
            prop_assert!(buf_len > 0 && buf_len <= MAX_FIXED_BUFFER_LEN);
            prop_assert_eq!(layout.index_of(start, buf_len), u16::try_from(index).ok());
            next = start + buf_len;
        }
        prop_assert_eq!(next, len);
    }
}

// ---------------------------------------------------------------------------
// Arenas
// ---------------------------------------------------------------------------

#[test]
fn huge_page_sizes() {
    assert_eq!(HugePageSize::TwoMiB.bytes(), 2 * MIB);
    assert_eq!(HugePageSize::TwoMiB.page_bits(), 21);
    assert_eq!(HugePageSize::OneGiB.bytes(), GIB);
    assert_eq!(HugePageSize::OneGiB.page_bits(), 30);
    assert_eq!(HugePageSize::TwoMiB.to_string(), "2 MiB");
    assert_eq!(HugePageSize::OneGiB.to_string(), "1 GiB");
}

#[test]
fn arena_lengths_are_validated_before_mapping() {
    for (len, size) in [
        (0, HugePageSize::TwoMiB),
        (MIB, HugePageSize::TwoMiB),
        (3 * MIB, HugePageSize::TwoMiB),
        (2 * MIB, HugePageSize::OneGiB),
    ] {
        let err = HugeArena::anonymous(len, size).unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::InvalidInput, "{len} {size}");
        let path = std::path::Path::new("arena-that-must-not-be-created");
        let err = HugeArena::map_file(path, len, size).unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::InvalidInput, "{len} {size}");
        assert!(!path.exists());
    }
}

#[cfg(not(target_os = "linux"))]
mod portable_arena {
    use super::*;

    #[test]
    fn anonymous_arena_is_aligned_zeroed_and_writable() {
        for pages in [1, 2, 3] {
            let len = pages * 2 * MIB;
            let mut arena = HugeArena::anonymous(len, HugePageSize::TwoMiB).unwrap();
            assert_eq!(arena.len(), len);
            assert!(!arena.is_empty());
            assert_eq!(arena.page_size(), HugePageSize::TwoMiB);
            assert_eq!(arena.as_ptr().addr() % (2 * MIB), 0);
            assert_eq!(arena.as_mut_ptr().cast_const(), arena.as_ptr());
            assert_eq!(arena.as_slice().as_ptr(), arena.as_ptr());
            assert_eq!(arena.as_slice().len(), len);
            assert!(arena.as_slice().iter().all(|&b| b == 0));

            for (i, byte) in arena.as_mut_slice().iter_mut().enumerate() {
                *byte = (i % 251) as u8;
            }
            let bytes = arena.as_slice();
            assert!(bytes.iter().enumerate().all(|(i, &b)| b == (i % 251) as u8));
            assert_eq!(bytes[len - 1], ((len - 1) % 251) as u8);

            arena.lock().unwrap();
            arena.exclude_from_core_dumps().unwrap();
            let debug = format!("{arena:?}");
            assert!(debug.starts_with("HugeArena") && debug.contains("TwoMiB"));
        }
    }

    #[test]
    fn the_arena_does_not_move_with_its_owner() {
        let mut arena = HugeArena::anonymous(2 * MIB, HugePageSize::TwoMiB).unwrap();
        arena.as_mut_slice()[123] = 0xa5;
        let ptr = arena.as_ptr();
        let moved = Box::new(arena);
        assert_eq!(moved.as_ptr(), ptr);
        assert_eq!(moved.as_slice()[123], 0xa5);
    }

    #[test]
    fn map_file_is_unsupported_off_linux() {
        let path = std::path::Path::new("arena-that-must-not-be-created");
        let err = HugeArena::map_file(path, 2 * MIB, HugePageSize::TwoMiB).unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::Unsupported);
        assert!(!path.exists());
    }

    #[test]
    fn registered_buffers_are_unsupported_off_linux() {
        let arena = HugeArena::anonymous(2 * MIB, HugePageSize::TwoMiB).unwrap();
        let err = otap_sys::RegisteredBuffers::new(&arena).unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::Unsupported);
    }
}

// ---------------------------------------------------------------------------
// Linux: SQE construction without a ublk device
// ---------------------------------------------------------------------------

#[cfg(target_os = "linux")]
mod linux_sqes {
    use std::fs::File;
    use std::os::fd::{AsFd, OwnedFd};

    use otap_sys::ublk::{UblkChar, UblkControl};

    use super::*;

    /// A descriptor that stands in for a ublk device: SQE construction only
    /// copies its number.
    fn stand_in() -> OwnedFd {
        OwnedFd::from(File::open("/dev/null").unwrap())
    }

    #[test]
    fn control_sqe_is_a_uring_cmd_without_flags() {
        let control = UblkControl::from_fd(stand_in());
        let sqe = control
            .command_sqe(&CtrlCommand::start_dev(1, 1))
            .user_data(9);
        assert_eq!(sqe.get_opcode(), u32::from(uring::IORING_OP_URING_CMD));
        assert_eq!(sqe.get_user_data(), 9);
        assert!(format!("{sqe:?}").contains("flags: 0"));
        assert!(control.as_fd().try_clone_to_owned().is_ok());
    }

    #[test]
    fn io_sqe_is_a_uring_cmd_and_the_char_fd_is_nonblocking() {
        let chr = UblkChar::from_fd(stand_in(), 4).unwrap();
        assert_eq!(chr.dev_id(), 4);
        let nonblocking = is_nonblocking(&chr);
        assert!(nonblocking);
        let sqe = chr.command_sqe(&IoCommand::fetch(0, 0));
        assert_eq!(sqe.get_opcode(), u32::from(uring::IORING_OP_URING_CMD));
    }

    #[test]
    fn user_copy_read_is_a_plain_read_without_async() {
        let chr = UblkChar::from_fd(stand_in(), 4).unwrap();
        let mut slot = vec![0u8; 4096];
        let pos = ublk::user_copy_offset(0, 3, 0);
        let sqe = uring::user_copy_read_sqe(&chr, slot.as_mut_ptr(), 4096, pos);
        assert_eq!(sqe.get_opcode(), u32::from(uring::IORING_OP_READ));
        assert!(format!("{sqe:?}").contains("flags: 0"));
    }

    /// Reads the descriptor's status flags through `/proc/self/fdinfo`, so
    /// the test needs no FFI: `flags:` is octal and `O_NONBLOCK` is 04000.
    fn is_nonblocking(chr: &UblkChar) -> bool {
        use std::os::fd::AsRawFd;
        let info =
            std::fs::read_to_string(format!("/proc/self/fdinfo/{}", chr.as_raw_fd())).unwrap();
        let flags = info
            .lines()
            .find_map(|line| line.strip_prefix("flags:"))
            .map(|value| u32::from_str_radix(value.trim(), 8).unwrap())
            .unwrap();
        flags & 0o4000 != 0
    }
}
