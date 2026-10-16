//! The ublk UAPI of Linux 6.12 (`include/uapi/linux/ublk_cmd.h`) as pure
//! data: command numbers and their ioctl encodings, feature, state,
//! operation and attribute bits, the user-copy offset arithmetic, the
//! `#[repr(C)]` structures as zerocopy views, and builders for the commands
//! OpenTAP issues. On Linux the module also opens the two ublk character
//! devices and turns a command into an io_uring submission entry; apart from
//! those `open` calls it performs no I/O on any host. Pushing the entry is the
//! caller's step (and the `unsafe` one, `SubmissionQueue::push`).
//!
//! References of the form `ublk_cmd.h:NN` are lines of the v6.12 header every
//! value here was checked against. The ioctl encoding is the generic one of
//! `include/uapi/asm-generic/ioctl.h`, which x86-64 and arm64 use unchanged.
//!
//! # The write path these items serve (round 3, section 1.3)
//! A device is created with `UBLK_F_USER_COPY`, so every queue starts with
//! one `FETCH_REQ` per tag carrying `addr = 0` ([`IoCommand::fetch`]). A WRITE
//! completes its fetch; the reactor copies the data out of the driver with a
//! plain `IORING_OP_READ` on the `O_NONBLOCK` `/dev/ublkcN` descriptor at
//! [`user_copy_offset`]`(q_id, tag, 0)` (`uring::user_copy_read_sqe`, Linux
//! only) and acknowledges with `COMMIT_AND_FETCH_REQ`
//! ([`IoCommand::commit_and_fetch`]), again with `addr = 0`.

use core::mem::{align_of, offset_of, size_of};

use zerocopy::{FromBytes, FromZeros, Immutable, IntoBytes, KnownLayout};

// ---------------------------------------------------------------------------
// ioctl encoding (include/uapi/asm-generic/ioctl.h)
// ---------------------------------------------------------------------------

/// `_IOC_NONE`: the command transfers no argument.
pub const IOC_NONE: u32 = 0;
/// `_IOC_WRITE`: the caller writes the argument and the kernel reads it.
pub const IOC_WRITE: u32 = 1;
/// `_IOC_READ`: the kernel writes the argument and the caller reads it.
pub const IOC_READ: u32 = 2;

/// `_IOC_TYPESHIFT`; `_IOC_NR` occupies bits 0 to 7 below it.
const IOC_TYPESHIFT: u32 = 8;
/// `_IOC_SIZESHIFT`; the argument size is 14 bits wide.
const IOC_SIZESHIFT: u32 = 16;
/// `_IOC_DIRSHIFT`; the direction is 2 bits wide.
const IOC_DIRSHIFT: u32 = 30;

/// The kernel's `_IOC(dir, type, nr, size)`:
/// `(dir << 30) | (size << 16) | (type << 8) | nr`.
///
/// # Panics
/// If a field does not fit its width: `dir` 2 bits, `ty` and `nr` 8 bits,
/// `size` 14 bits. Every use in this crate is a `const` item, so a bad
/// encoding fails to compile rather than panicking.
#[must_use]
pub const fn ioc(dir: u32, ty: u32, nr: u32, size: u32) -> u32 {
    assert!(dir < 1 << 2, "_IOC: dir is 2 bits wide");
    assert!(ty < 1 << 8, "_IOC: type is 8 bits wide");
    assert!(nr < 1 << 8, "_IOC: nr is 8 bits wide");
    assert!(size < 1 << 14, "_IOC: size is 14 bits wide");
    (dir << IOC_DIRSHIFT) | (size << IOC_SIZESHIFT) | (ty << IOC_TYPESHIFT) | nr
}

/// The ioctl type of every ublk command: `'u'` (ublk_cmd.h:28 to 53, 91 to 96).
const UBLK_IOC_TYPE: u32 = b'u' as u32;
/// `sizeof(struct ublksrv_ctrl_cmd)`, the size field of every `UBLK_U_CMD_*`.
const CTRL_CMD_SIZE: u32 = size_of::<UblksrvCtrlCmd>() as u32;
/// `sizeof(struct ublksrv_io_cmd)`, the size field of every `UBLK_U_IO_*`.
const IO_CMD_SIZE: u32 = size_of::<UblksrvIoCmd>() as u32;

/// `_IOR('u', nr, struct ublksrv_ctrl_cmd)`.
const fn ctrl_ior(nr: u32) -> u32 {
    ioc(IOC_READ, UBLK_IOC_TYPE, nr, CTRL_CMD_SIZE)
}

/// `_IOWR('u', nr, struct ublksrv_ctrl_cmd)`.
const fn ctrl_iowr(nr: u32) -> u32 {
    ioc(IOC_READ | IOC_WRITE, UBLK_IOC_TYPE, nr, CTRL_CMD_SIZE)
}

/// `_IOWR('u', nr, struct ublksrv_io_cmd)`.
const fn io_iowr(nr: u32) -> u32 {
    ioc(IOC_READ | IOC_WRITE, UBLK_IOC_TYPE, nr, IO_CMD_SIZE)
}

// ---------------------------------------------------------------------------
// Control commands (ublk_cmd.h:15 to 59)
// ---------------------------------------------------------------------------

/// `UBLK_CMD_GET_QUEUE_AFFINITY` (ublk_cmd.h:15), the legacy number.
pub const UBLK_CMD_GET_QUEUE_AFFINITY: u32 = 0x01;
/// `UBLK_CMD_GET_DEV_INFO` (ublk_cmd.h:16).
pub const UBLK_CMD_GET_DEV_INFO: u32 = 0x02;
/// `UBLK_CMD_ADD_DEV` (ublk_cmd.h:17).
pub const UBLK_CMD_ADD_DEV: u32 = 0x04;
/// `UBLK_CMD_DEL_DEV` (ublk_cmd.h:18).
pub const UBLK_CMD_DEL_DEV: u32 = 0x05;
/// `UBLK_CMD_START_DEV` (ublk_cmd.h:19).
pub const UBLK_CMD_START_DEV: u32 = 0x06;
/// `UBLK_CMD_STOP_DEV` (ublk_cmd.h:20).
pub const UBLK_CMD_STOP_DEV: u32 = 0x07;
/// `UBLK_CMD_SET_PARAMS` (ublk_cmd.h:21).
pub const UBLK_CMD_SET_PARAMS: u32 = 0x08;
/// `UBLK_CMD_GET_PARAMS` (ublk_cmd.h:22).
pub const UBLK_CMD_GET_PARAMS: u32 = 0x09;
/// `UBLK_CMD_START_USER_RECOVERY` (ublk_cmd.h:23).
pub const UBLK_CMD_START_USER_RECOVERY: u32 = 0x10;
/// `UBLK_CMD_END_USER_RECOVERY` (ublk_cmd.h:24).
pub const UBLK_CMD_END_USER_RECOVERY: u32 = 0x11;
/// `UBLK_CMD_GET_DEV_INFO2` (ublk_cmd.h:25).
pub const UBLK_CMD_GET_DEV_INFO2: u32 = 0x12;

/// `UBLK_U_CMD_GET_QUEUE_AFFINITY`, `_IOR` (ublk_cmd.h:28): 0x8020_7501.
pub const UBLK_U_CMD_GET_QUEUE_AFFINITY: u32 = ctrl_ior(UBLK_CMD_GET_QUEUE_AFFINITY);
/// `UBLK_U_CMD_GET_DEV_INFO`, `_IOR` (ublk_cmd.h:30): 0x8020_7502.
pub const UBLK_U_CMD_GET_DEV_INFO: u32 = ctrl_ior(UBLK_CMD_GET_DEV_INFO);
/// `UBLK_U_CMD_ADD_DEV`, `_IOWR` (ublk_cmd.h:32): 0xC020_7504.
pub const UBLK_U_CMD_ADD_DEV: u32 = ctrl_iowr(UBLK_CMD_ADD_DEV);
/// `UBLK_U_CMD_DEL_DEV`, `_IOWR` (ublk_cmd.h:34): 0xC020_7505.
pub const UBLK_U_CMD_DEL_DEV: u32 = ctrl_iowr(UBLK_CMD_DEL_DEV);
/// `UBLK_U_CMD_START_DEV`, `_IOWR` (ublk_cmd.h:36): 0xC020_7506.
pub const UBLK_U_CMD_START_DEV: u32 = ctrl_iowr(UBLK_CMD_START_DEV);
/// `UBLK_U_CMD_STOP_DEV`, `_IOWR` (ublk_cmd.h:38): 0xC020_7507.
pub const UBLK_U_CMD_STOP_DEV: u32 = ctrl_iowr(UBLK_CMD_STOP_DEV);
/// `UBLK_U_CMD_SET_PARAMS`, `_IOWR` (ublk_cmd.h:40): 0xC020_7508.
pub const UBLK_U_CMD_SET_PARAMS: u32 = ctrl_iowr(UBLK_CMD_SET_PARAMS);
/// `UBLK_U_CMD_GET_PARAMS`, `_IOR` (ublk_cmd.h:42): 0x8020_7509.
pub const UBLK_U_CMD_GET_PARAMS: u32 = ctrl_ior(UBLK_CMD_GET_PARAMS);
/// `UBLK_U_CMD_START_USER_RECOVERY`, `_IOWR` (ublk_cmd.h:44): 0xC020_7510.
pub const UBLK_U_CMD_START_USER_RECOVERY: u32 = ctrl_iowr(UBLK_CMD_START_USER_RECOVERY);
/// `UBLK_U_CMD_END_USER_RECOVERY`, `_IOWR` (ublk_cmd.h:46): 0xC020_7511.
pub const UBLK_U_CMD_END_USER_RECOVERY: u32 = ctrl_iowr(UBLK_CMD_END_USER_RECOVERY);
/// `UBLK_U_CMD_GET_DEV_INFO2`, `_IOR` (ublk_cmd.h:48): 0x8020_7512.
pub const UBLK_U_CMD_GET_DEV_INFO2: u32 = ctrl_ior(UBLK_CMD_GET_DEV_INFO2);
/// `UBLK_U_CMD_GET_FEATURES`, `_IOR('u', 0x13, ...)` (ublk_cmd.h:50):
/// 0x8020_7513. It has no legacy number.
pub const UBLK_U_CMD_GET_FEATURES: u32 = ctrl_ior(0x13);
/// `UBLK_U_CMD_DEL_DEV_ASYNC`, `_IOR('u', 0x14, ...)` (ublk_cmd.h:52):
/// 0x8020_7514. It has no legacy number.
pub const UBLK_U_CMD_DEL_DEV_ASYNC: u32 = ctrl_ior(0x14);

/// `UBLK_FEATURES_LEN` (ublk_cmd.h:59): the size of the `GET_FEATURES` reply,
/// one native-endian `u64` of `UBLK_F_*` bits. The driver refuses any other
/// `len`.
pub const UBLK_FEATURES_LEN: u16 = 8;

/// The bound on device numbers: `UBLK_MAX_UBLKS`, which is `UBLK_MINORS`,
/// `1 << MINORBITS` (a driver constant of `ublk_drv.c`, not of the UAPI
/// header). `ADD_DEV` refuses a requested id at or above it and the
/// driver's id allocator never hands one out; a device number read from
/// anywhere else (a directory listing, a state file) is checked against it.
pub const UBLK_MAX_UBLKS: u32 = 1 << 20;

/// Where the driver's character devices appear in sysfs: the `ublk-char`
/// class (`ublk_chr_class` in `ublk_drv.c`), one entry `ublkc<N>` per
/// device from `ADD_DEV` to its release (`ublk_add_chdev`). The driver has
/// no command that lists its devices, so this directory is the list; each
/// entry is then asked about with `GET_DEV_INFO`.
pub const UBLK_CHAR_CLASS_DIR: &str = "/sys/class/ublk-char";

/// The device number of a `ublk-char` class entry or a `/dev` node name,
/// `ublkc<N>` (decimal, no sign, no leading zero unless it is `0`, below
/// [`UBLK_MAX_UBLKS`]); `None` for any other name.
#[must_use]
pub fn parse_char_name(name: &str) -> Option<u32> {
    let digits = name.strip_prefix("ublkc")?;
    if digits.is_empty()
        || !digits.bytes().all(|b| b.is_ascii_digit())
        || (digits.len() > 1 && digits.starts_with('0'))
    {
        return None;
    }
    let id: u32 = digits.parse().ok()?;
    (id < UBLK_MAX_UBLKS).then_some(id)
}

// ---------------------------------------------------------------------------
// I/O commands, results and the user-copy window (ublk_cmd.h:86 to 127)
// ---------------------------------------------------------------------------

/// `UBLK_IO_FETCH_REQ` (ublk_cmd.h:86), the legacy number.
pub const UBLK_IO_FETCH_REQ: u32 = 0x20;
/// `UBLK_IO_COMMIT_AND_FETCH_REQ` (ublk_cmd.h:87).
pub const UBLK_IO_COMMIT_AND_FETCH_REQ: u32 = 0x21;
/// `UBLK_IO_NEED_GET_DATA` (ublk_cmd.h:88).
pub const UBLK_IO_NEED_GET_DATA: u32 = 0x22;

/// `UBLK_U_IO_FETCH_REQ`, `_IOWR` (ublk_cmd.h:91): 0xC010_7520.
pub const UBLK_U_IO_FETCH_REQ: u32 = io_iowr(UBLK_IO_FETCH_REQ);
/// `UBLK_U_IO_COMMIT_AND_FETCH_REQ`, `_IOWR` (ublk_cmd.h:93): 0xC010_7521.
pub const UBLK_U_IO_COMMIT_AND_FETCH_REQ: u32 = io_iowr(UBLK_IO_COMMIT_AND_FETCH_REQ);
/// `UBLK_U_IO_NEED_GET_DATA`, `_IOWR` (ublk_cmd.h:95): 0xC010_7522.
pub const UBLK_U_IO_NEED_GET_DATA: u32 = io_iowr(UBLK_IO_NEED_GET_DATA);

/// `UBLK_IO_RES_OK` (ublk_cmd.h:99): the fetch completed with a request.
pub const UBLK_IO_RES_OK: i32 = 0;
/// `UBLK_IO_RES_NEED_GET_DATA` (ublk_cmd.h:100): only with
/// `UBLK_F_NEED_GET_DATA`, which OpenTAP does not use.
pub const UBLK_IO_RES_NEED_GET_DATA: i32 = 1;
/// `UBLK_IO_RES_ABORT` (ublk_cmd.h:101): `-ENODEV`, ENODEV being 19 in
/// `include/uapi/asm-generic/errno-base.h`. The only result after which the
/// tag must not be fetched again.
pub const UBLK_IO_RES_ABORT: i32 = -19;

/// `UBLKSRV_CMD_BUF_OFFSET` (ublk_cmd.h:103): the `mmap` offset of a queue's
/// array of [`UblksrvIoDesc`] on `/dev/ublkcN`.
pub const UBLKSRV_CMD_BUF_OFFSET: u64 = 0;
/// `UBLKSRV_IO_BUF_OFFSET` (ublk_cmd.h:104): the base of the user-copy window
/// on `/dev/ublkcN`.
pub const UBLKSRV_IO_BUF_OFFSET: u64 = 0x8000_0000;

/// `UBLK_MAX_QUEUE_DEPTH` (ublk_cmd.h:107): tags per queue the driver
/// accepts, although the tag field is 16 bits wide.
pub const UBLK_MAX_QUEUE_DEPTH: u32 = 4096;

/// `UBLK_IO_BUF_OFF` (ublk_cmd.h:110): the byte offset within a request
/// starts at bit 0 of a user-copy position.
pub const UBLK_IO_BUF_OFF: u32 = 0;
/// `UBLK_IO_BUF_BITS` (ublk_cmd.h:111): 25 bits, so one request carries at
/// most 32 MiB.
pub const UBLK_IO_BUF_BITS: u32 = 25;
/// `UBLK_IO_BUF_BITS_MASK` (ublk_cmd.h:112).
pub const UBLK_IO_BUF_BITS_MASK: u64 = (1 << UBLK_IO_BUF_BITS) - 1;
/// `UBLK_TAG_OFF` (ublk_cmd.h:115): the tag starts at bit 25.
pub const UBLK_TAG_OFF: u32 = UBLK_IO_BUF_BITS;
/// `UBLK_TAG_BITS` (ublk_cmd.h:116).
pub const UBLK_TAG_BITS: u32 = 16;
/// `UBLK_TAG_BITS_MASK` (ublk_cmd.h:117).
pub const UBLK_TAG_BITS_MASK: u64 = (1 << UBLK_TAG_BITS) - 1;
/// `UBLK_QID_OFF` (ublk_cmd.h:120): the queue id starts at bit 41.
pub const UBLK_QID_OFF: u32 = UBLK_TAG_OFF + UBLK_TAG_BITS;
/// `UBLK_QID_BITS` (ublk_cmd.h:121).
pub const UBLK_QID_BITS: u32 = 12;
/// `UBLK_QID_BITS_MASK` (ublk_cmd.h:122).
pub const UBLK_QID_BITS_MASK: u64 = (1 << UBLK_QID_BITS) - 1;
/// `UBLK_MAX_NR_QUEUES` (ublk_cmd.h:124): 4096.
pub const UBLK_MAX_NR_QUEUES: u32 = 1 << UBLK_QID_BITS;
/// `UBLKSRV_IO_BUF_TOTAL_BITS` (ublk_cmd.h:126): 53.
pub const UBLKSRV_IO_BUF_TOTAL_BITS: u32 = UBLK_QID_OFF + UBLK_QID_BITS;
/// `UBLKSRV_IO_BUF_TOTAL_SIZE` (ublk_cmd.h:127): the size of the user-copy
/// window above [`UBLKSRV_IO_BUF_OFFSET`].
pub const UBLKSRV_IO_BUF_TOTAL_SIZE: u64 = 1 << UBLKSRV_IO_BUF_TOTAL_BITS;

/// The file position of byte `buf_off` of request `(q_id, tag)` in the
/// `UBLK_F_USER_COPY` window of `/dev/ublkcN`: `UBLKSRV_IO_BUF_OFFSET +
/// (q_id << 41) + (tag << 25) + buf_off`, the inverse of the driver's
/// `ublk_pos_to_hwq`, `ublk_pos_to_tag` and `ublk_pos_to_buf_off`
/// (`drivers/block/ublk_drv.c`).
///
/// `None` when a field does not fit its width: `q_id` must be below
/// [`UBLK_MAX_NR_QUEUES`] and `buf_off` below 32 MiB ([`UBLK_IO_BUF_BITS`]);
/// every `u16` tag is encodable. The driver further refuses a queue at or
/// above the device's `nr_hw_queues` and a tag at or above its
/// `queue_depth` (at most [`UBLK_MAX_QUEUE_DEPTH`]).
#[must_use]
pub const fn checked_user_copy_offset(q_id: u16, tag: u16, buf_off: u32) -> Option<u64> {
    if q_id as u32 >= UBLK_MAX_NR_QUEUES || buf_off as u64 > UBLK_IO_BUF_BITS_MASK {
        return None;
    }
    Some(
        UBLKSRV_IO_BUF_OFFSET
            + ((q_id as u64) << UBLK_QID_OFF)
            + ((tag as u64) << UBLK_TAG_OFF)
            + buf_off as u64,
    )
}

/// [`checked_user_copy_offset`] for arguments the caller knows are in range:
/// the position an `IORING_OP_READ` (WRITE data) or `IORING_OP_WRITE` (READ
/// data) uses on `/dev/ublkcN`.
///
/// # Panics
/// If `q_id >= UBLK_MAX_NR_QUEUES` or `buf_off > UBLK_IO_BUF_BITS_MASK`. A
/// queue id comes from the device's own `nr_hw_queues` and a byte offset from
/// a request of at most 32 MiB, so either is a bug in the caller.
#[must_use]
pub const fn user_copy_offset(q_id: u16, tag: u16, buf_off: u32) -> u64 {
    match checked_user_copy_offset(q_id, tag, buf_off) {
        Some(pos) => pos,
        None => panic!("user_copy_offset: q_id must be below 4096 and buf_off below 32 MiB"),
    }
}

/// Splits a user-copy position into `(q_id, tag, buf_off)` the way the driver
/// does (`ublk_pos_to_hwq`, `ublk_pos_to_tag`, `ublk_pos_to_buf_off`).
/// `None` for a position outside the window `[UBLKSRV_IO_BUF_OFFSET,
/// UBLKSRV_IO_BUF_OFFSET + UBLKSRV_IO_BUF_TOTAL_SIZE)`.
#[must_use]
pub const fn split_user_copy_offset(pos: u64) -> Option<(u16, u16, u32)> {
    if pos < UBLKSRV_IO_BUF_OFFSET || pos - UBLKSRV_IO_BUF_OFFSET >= UBLKSRV_IO_BUF_TOTAL_SIZE {
        return None;
    }
    let rel = pos - UBLKSRV_IO_BUF_OFFSET;
    Some((
        ((rel >> UBLK_QID_OFF) & UBLK_QID_BITS_MASK) as u16,
        ((rel >> UBLK_TAG_OFF) & UBLK_TAG_BITS_MASK) as u16,
        (rel & UBLK_IO_BUF_BITS_MASK) as u32,
    ))
}

// ---------------------------------------------------------------------------
// Feature flags and device states (ublk_cmd.h:133 to 196)
// ---------------------------------------------------------------------------

/// `UBLK_F_SUPPORT_ZERO_COPY` (ublk_cmd.h:133). Unsupported by 6.12: the
/// driver does not report it in `GET_FEATURES` and does not grant it to a new
/// device.
pub const UBLK_F_SUPPORT_ZERO_COPY: u64 = 1 << 0;
/// `UBLK_F_URING_CMD_COMP_IN_TASK` (ublk_cmd.h:139).
pub const UBLK_F_URING_CMD_COMP_IN_TASK: u64 = 1 << 1;
/// `UBLK_F_NEED_GET_DATA` (ublk_cmd.h:148).
pub const UBLK_F_NEED_GET_DATA: u64 = 1 << 2;
/// `UBLK_F_USER_RECOVERY` (ublk_cmd.h:150): the device survives its server's
/// death and a new server re-attaches it (`START_USER_RECOVERY`,
/// `END_USER_RECOVERY`).
pub const UBLK_F_USER_RECOVERY: u64 = 1 << 3;
/// `UBLK_F_USER_RECOVERY_REISSUE` (ublk_cmd.h:152): requests in flight at the
/// server's death are reissued to the new server. With `USER_RECOVERY`
/// alone a request the dead server had fetched and not committed is failed
/// with `EIO` (`__ublk_fail_req`); with this flag too it is requeued and
/// delivered to the new server after `END_USER_RECOVERY`. 6.12 has no
/// `UBLK_F_USER_RECOVERY_FAIL_IO` (`UBLK_F_ALL` in `ublk_drv.c`).
pub const UBLK_F_USER_RECOVERY_REISSUE: u64 = 1 << 4;
/// `UBLK_F_UNPRIVILEGED_DEV` (ublk_cmd.h:173). Excluded by design (C04): the
/// driver refuses it together with `UBLK_F_USER_COPY`.
pub const UBLK_F_UNPRIVILEGED_DEV: u64 = 1 << 5;
/// `UBLK_F_CMD_IOCTL_ENCODE` (ublk_cmd.h:176): commands use the `UBLK_U_*`
/// ioctl encoding.
pub const UBLK_F_CMD_IOCTL_ENCODE: u64 = 1 << 6;
/// `UBLK_F_USER_COPY` (ublk_cmd.h:185): request data moves by `pread`/`pwrite`
/// (here `IORING_OP_READ`/`IORING_OP_WRITE`) on `/dev/ublkcN` at
/// [`user_copy_offset`].
pub const UBLK_F_USER_COPY: u64 = 1 << 7;
/// `UBLK_F_ZONED` (ublk_cmd.h:191).
pub const UBLK_F_ZONED: u64 = 1 << 8;

/// The name of a single `UBLK_F_*` bit, for diagnostics; `None` for any other
/// value, including combinations.
#[must_use]
pub const fn feature_name(feature: u64) -> Option<&'static str> {
    Some(match feature {
        UBLK_F_SUPPORT_ZERO_COPY => "UBLK_F_SUPPORT_ZERO_COPY",
        UBLK_F_URING_CMD_COMP_IN_TASK => "UBLK_F_URING_CMD_COMP_IN_TASK",
        UBLK_F_NEED_GET_DATA => "UBLK_F_NEED_GET_DATA",
        UBLK_F_USER_RECOVERY => "UBLK_F_USER_RECOVERY",
        UBLK_F_USER_RECOVERY_REISSUE => "UBLK_F_USER_RECOVERY_REISSUE",
        UBLK_F_UNPRIVILEGED_DEV => "UBLK_F_UNPRIVILEGED_DEV",
        UBLK_F_CMD_IOCTL_ENCODE => "UBLK_F_CMD_IOCTL_ENCODE",
        UBLK_F_USER_COPY => "UBLK_F_USER_COPY",
        UBLK_F_ZONED => "UBLK_F_ZONED",
        _ => return None,
    })
}

/// `UBLK_S_DEV_DEAD` (ublk_cmd.h:194).
pub const UBLK_S_DEV_DEAD: u16 = 0;
/// `UBLK_S_DEV_LIVE` (ublk_cmd.h:195).
pub const UBLK_S_DEV_LIVE: u16 = 1;
/// `UBLK_S_DEV_QUIESCED` (ublk_cmd.h:196): the server died with
/// `UBLK_F_USER_RECOVERY` set and the device waits for a new one.
pub const UBLK_S_DEV_QUIESCED: u16 = 2;

// ---------------------------------------------------------------------------
// Request operations and flags (ublk_cmd.h:246 to 277)
// ---------------------------------------------------------------------------

/// `UBLK_IO_OP_READ` (ublk_cmd.h:246).
pub const UBLK_IO_OP_READ: u8 = 0;
/// `UBLK_IO_OP_WRITE` (ublk_cmd.h:247).
pub const UBLK_IO_OP_WRITE: u8 = 1;
/// `UBLK_IO_OP_FLUSH` (ublk_cmd.h:248).
pub const UBLK_IO_OP_FLUSH: u8 = 2;
/// `UBLK_IO_OP_DISCARD` (ublk_cmd.h:249).
pub const UBLK_IO_OP_DISCARD: u8 = 3;
/// `UBLK_IO_OP_WRITE_SAME` (ublk_cmd.h:250).
pub const UBLK_IO_OP_WRITE_SAME: u8 = 4;
/// `UBLK_IO_OP_WRITE_ZEROES` (ublk_cmd.h:251).
pub const UBLK_IO_OP_WRITE_ZEROES: u8 = 5;
/// `UBLK_IO_OP_ZONE_OPEN` (ublk_cmd.h:252).
pub const UBLK_IO_OP_ZONE_OPEN: u8 = 10;
/// `UBLK_IO_OP_ZONE_CLOSE` (ublk_cmd.h:253).
pub const UBLK_IO_OP_ZONE_CLOSE: u8 = 11;
/// `UBLK_IO_OP_ZONE_FINISH` (ublk_cmd.h:254).
pub const UBLK_IO_OP_ZONE_FINISH: u8 = 12;
/// `UBLK_IO_OP_ZONE_APPEND` (ublk_cmd.h:255).
pub const UBLK_IO_OP_ZONE_APPEND: u8 = 13;
/// `UBLK_IO_OP_ZONE_RESET_ALL` (ublk_cmd.h:256).
pub const UBLK_IO_OP_ZONE_RESET_ALL: u8 = 14;
/// `UBLK_IO_OP_ZONE_RESET` (ublk_cmd.h:257).
pub const UBLK_IO_OP_ZONE_RESET: u8 = 15;
/// `UBLK_IO_OP_REPORT_ZONES` (ublk_cmd.h:269).
pub const UBLK_IO_OP_REPORT_ZONES: u8 = 18;

/// `UBLK_IO_F_FAILFAST_DEV` (ublk_cmd.h:271). Like every `UBLK_IO_F_*` value
/// it is a mask over the whole `op_flags` word, not over `op_flags >> 8`.
pub const UBLK_IO_F_FAILFAST_DEV: u32 = 1 << 8;
/// `UBLK_IO_F_FAILFAST_TRANSPORT` (ublk_cmd.h:272).
pub const UBLK_IO_F_FAILFAST_TRANSPORT: u32 = 1 << 9;
/// `UBLK_IO_F_FAILFAST_DRIVER` (ublk_cmd.h:273).
pub const UBLK_IO_F_FAILFAST_DRIVER: u32 = 1 << 10;
/// `UBLK_IO_F_META` (ublk_cmd.h:274).
pub const UBLK_IO_F_META: u32 = 1 << 11;
/// `UBLK_IO_F_FUA` (ublk_cmd.h:275).
pub const UBLK_IO_F_FUA: u32 = 1 << 13;
/// `UBLK_IO_F_NOUNMAP` (ublk_cmd.h:276).
pub const UBLK_IO_F_NOUNMAP: u32 = 1 << 15;
/// `UBLK_IO_F_SWAP` (ublk_cmd.h:277).
pub const UBLK_IO_F_SWAP: u32 = 1 << 16;

// ---------------------------------------------------------------------------
// Parameter attributes and types (ublk_cmd.h:340 to 397)
// ---------------------------------------------------------------------------

/// `UBLK_ATTR_READ_ONLY` (ublk_cmd.h:340), in [`UblkParamBasic::attrs`].
pub const UBLK_ATTR_READ_ONLY: u32 = 1 << 0;
/// `UBLK_ATTR_ROTATIONAL` (ublk_cmd.h:341).
pub const UBLK_ATTR_ROTATIONAL: u32 = 1 << 1;
/// `UBLK_ATTR_VOLATILE_CACHE` (ublk_cmd.h:342): the device has a write cache,
/// so the block layer sends `FLUSH`.
pub const UBLK_ATTR_VOLATILE_CACHE: u32 = 1 << 2;
/// `UBLK_ATTR_FUA` (ublk_cmd.h:343): the device honours `UBLK_IO_F_FUA`.
pub const UBLK_ATTR_FUA: u32 = 1 << 3;

/// `UBLK_PARAM_TYPE_BASIC` (ublk_cmd.h:394), in [`UblkParams::types`].
pub const UBLK_PARAM_TYPE_BASIC: u32 = 1 << 0;
/// `UBLK_PARAM_TYPE_DISCARD` (ublk_cmd.h:395).
pub const UBLK_PARAM_TYPE_DISCARD: u32 = 1 << 1;
/// `UBLK_PARAM_TYPE_DEVT` (ublk_cmd.h:396): read-only, filled by `GET_PARAMS`.
pub const UBLK_PARAM_TYPE_DEVT: u32 = 1 << 2;
/// `UBLK_PARAM_TYPE_ZONED` (ublk_cmd.h:397).
pub const UBLK_PARAM_TYPE_ZONED: u32 = 1 << 3;

// ---------------------------------------------------------------------------
// Structures
// ---------------------------------------------------------------------------

/// `struct ublksrv_ctrl_cmd` (ublk_cmd.h:199): the 32-byte header of every
/// control command, carried in the command area of a 128-byte SQE.
#[repr(C)]
#[derive(
    Clone, Copy, Debug, Default, PartialEq, Eq, Hash, FromBytes, IntoBytes, Immutable, KnownLayout,
)]
pub struct UblksrvCtrlCmd {
    /// The device the command is for.
    pub dev_id: u32,
    /// The queue, or [`UblksrvCtrlCmd::NO_QUEUE`] for a device command.
    pub queue_id: u16,
    /// Length of the command's buffer at `addr`.
    pub len: u16,
    /// Address of the command's IN or OUT buffer in this process.
    pub addr: u64,
    /// Inline data: `START_DEV` and `END_USER_RECOVERY` carry the server's pid
    /// in `data[0]`.
    pub data: [u64; 1],
    /// Length of the device path including its NUL, for
    /// `UBLK_F_UNPRIVILEGED_DEV` and `GET_DEV_INFO2` only.
    pub dev_path_len: u16,
    /// Padding, zero.
    pub pad: u16,
    /// Reserved, zero.
    pub reserved: u32,
}

impl UblksrvCtrlCmd {
    /// `queue_id` of a command that is not for a queue: C's `-1`
    /// (ublk_cmd.h:203). `ADD_DEV` refuses anything else.
    pub const NO_QUEUE: u16 = u16::MAX;
    /// `dev_id` with which `ADD_DEV` lets the driver choose the id (`U32_MAX`
    /// in `ublk_ctrl_add_dev`); also carried by `GET_FEATURES`, which names no
    /// device.
    pub const ANY_DEV: u32 = u32::MAX;
}

/// `struct ublksrv_ctrl_dev_info` (ublk_cmd.h:223): the 64-byte device
/// description `ADD_DEV` takes and returns and `GET_DEV_INFO` returns.
#[repr(C)]
#[derive(
    Clone, Copy, Debug, Default, PartialEq, Eq, Hash, FromBytes, IntoBytes, Immutable, KnownLayout,
)]
pub struct UblksrvCtrlDevInfo {
    /// Number of hardware queues, at most [`UBLK_MAX_NR_QUEUES`].
    pub nr_hw_queues: u16,
    /// Tags per queue, at most [`UBLK_MAX_QUEUE_DEPTH`].
    pub queue_depth: u16,
    /// One of `UBLK_S_DEV_*`.
    pub state: u16,
    /// Padding, zero.
    pub pad0: u16,
    /// Largest request in bytes.
    pub max_io_buf_bytes: u32,
    /// The device id.
    pub dev_id: u32,
    /// The pid of the serving process.
    pub ublksrv_pid: i32,
    /// Padding, zero.
    pub pad1: u32,
    /// `UBLK_F_*` bits.
    pub flags: u64,
    /// For the server's own use; the driver does not interpret it.
    pub ublksrv_flags: u64,
    /// The owner's uid, stored by the driver.
    pub owner_uid: u32,
    /// The owner's gid, stored by the driver.
    pub owner_gid: u32,
    /// Reserved, zero.
    pub reserved1: u64,
    /// Reserved, zero.
    pub reserved2: u64,
}

/// `struct ublksrv_io_desc` (ublk_cmd.h:286): the driver's description of one
/// request, in the array a queue maps at [`UBLKSRV_CMD_BUF_OFFSET`], indexed
/// by tag.
#[repr(C)]
#[derive(
    Clone, Copy, Debug, Default, PartialEq, Eq, Hash, FromBytes, IntoBytes, Immutable, KnownLayout,
)]
pub struct UblksrvIoDesc {
    /// The operation in bits 0 to 7 (`UBLK_IO_OP_*`) and the `UBLK_IO_F_*`
    /// flags in bits 8 to 31.
    pub op_flags: u32,
    /// The C union `{ nr_sectors; nr_zones; }`: read it through
    /// [`nr_sectors`](Self::nr_sectors()) or, for `UBLK_IO_OP_REPORT_ZONES`,
    /// [`nr_zones`](Self::nr_zones()).
    pub nr_sectors: u32,
    /// First 512-byte sector of the request.
    pub start_sector: u64,
    /// The buffer address the server supplied in its fetch; unused with
    /// `UBLK_F_USER_COPY`.
    pub addr: u64,
}

impl UblksrvIoDesc {
    /// The operation, one of `UBLK_IO_OP_*` (`ublksrv_get_op`,
    /// ublk_cmd.h:302).
    #[must_use]
    pub const fn op(&self) -> u8 {
        (self.op_flags & 0xff) as u8
    }

    /// The flag bits in place, so `desc.flags() & UBLK_IO_F_FUA != 0` tests
    /// FUA. The C helper `ublksrv_get_flags` (ublk_cmd.h:307) returns
    /// `op_flags >> 8` instead, which cannot be tested against the
    /// `UBLK_IO_F_*` masks: they are defined on the unshifted word (1 << 8 and
    /// up), which is where the driver's `ublk_setup_iod` puts them.
    #[must_use]
    pub const fn flags(&self) -> u32 {
        self.op_flags & !0xff
    }

    /// Whether every bit of `flag` (`UBLK_IO_F_*`) is set.
    #[must_use]
    pub const fn has_flag(&self, flag: u32) -> bool {
        self.op_flags & flag == flag
    }

    /// The union read as `nr_sectors`: the request's length in 512-byte
    /// sectors, for every operation but `UBLK_IO_OP_REPORT_ZONES`.
    #[must_use]
    pub const fn nr_sectors(&self) -> u32 {
        self.nr_sectors
    }

    /// The union read as `nr_zones`, for `UBLK_IO_OP_REPORT_ZONES`.
    #[must_use]
    pub const fn nr_zones(&self) -> u32 {
        self.nr_sectors
    }
}

/// `struct ublksrv_io_cmd` (ublk_cmd.h:313): the 16-byte payload of a
/// `FETCH_REQ`, `COMMIT_AND_FETCH_REQ` or `NEED_GET_DATA` command on
/// `/dev/ublkcN`, which fits the command area of a 64-byte SQE.
#[repr(C)]
#[derive(
    Clone, Copy, Debug, Default, PartialEq, Eq, Hash, FromBytes, IntoBytes, Immutable, KnownLayout,
)]
pub struct UblksrvIoCmd {
    /// The queue.
    pub q_id: u16,
    /// The request's tag within the queue.
    pub tag: u16,
    /// The request's result, for `COMMIT_AND_FETCH_REQ`: bytes transferred or
    /// a negative errno.
    pub result: i32,
    /// The C union `{ addr; zone_append_lba; }`. With `UBLK_F_USER_COPY` the
    /// driver requires `addr == 0` in every fetch and commit except a zone
    /// append's commit, where it is [`zone_append_lba`](Self::zone_append_lba()).
    pub addr: u64,
}

impl UblksrvIoCmd {
    /// The union read as `zone_append_lba`.
    #[must_use]
    pub const fn zone_append_lba(&self) -> u64 {
        self.addr
    }
}

/// `struct ublk_param_basic` (ublk_cmd.h:339).
#[repr(C)]
#[derive(
    Clone, Copy, Debug, Default, PartialEq, Eq, Hash, FromBytes, IntoBytes, Immutable, KnownLayout,
)]
pub struct UblkParamBasic {
    /// `UBLK_ATTR_*` bits.
    pub attrs: u32,
    /// log2 of the logical block size.
    pub logical_bs_shift: u8,
    /// log2 of the physical block size.
    pub physical_bs_shift: u8,
    /// log2 of the optimal I/O size.
    pub io_opt_shift: u8,
    /// log2 of the minimum I/O size.
    pub io_min_shift: u8,
    /// Largest request in 512-byte sectors.
    pub max_sectors: u32,
    /// Chunk size in sectors (requests do not cross it), or 0.
    pub chunk_sectors: u32,
    /// Device size in sectors.
    pub dev_sectors: u64,
    /// Virtual boundary mask for request segments, or 0.
    pub virt_boundary_mask: u64,
}

/// `struct ublk_param_discard` (ublk_cmd.h:357).
#[repr(C)]
#[derive(
    Clone, Copy, Debug, Default, PartialEq, Eq, Hash, FromBytes, IntoBytes, Immutable, KnownLayout,
)]
pub struct UblkParamDiscard {
    /// Discard alignment in bytes.
    pub discard_alignment: u32,
    /// Discard granularity in bytes.
    pub discard_granularity: u32,
    /// Largest discard in sectors.
    pub max_discard_sectors: u32,
    /// Largest write-zeroes in sectors.
    pub max_write_zeroes_sectors: u32,
    /// Most segments in one discard.
    pub max_discard_segments: u16,
    /// Reserved, zero.
    pub reserved0: u16,
}

/// `struct ublk_param_devt` (ublk_cmd.h:372): read-only, filled by
/// `GET_PARAMS`; the disk numbers are valid once the device is started.
#[repr(C)]
#[derive(
    Clone, Copy, Debug, Default, PartialEq, Eq, Hash, FromBytes, IntoBytes, Immutable, KnownLayout,
)]
pub struct UblkParamDevt {
    /// Major of `/dev/ublkcN`.
    pub char_major: u32,
    /// Minor of `/dev/ublkcN`.
    pub char_minor: u32,
    /// Major of `/dev/ublkbN`.
    pub disk_major: u32,
    /// Minor of `/dev/ublkbN`.
    pub disk_minor: u32,
}

/// `struct ublk_param_zoned` (ublk_cmd.h:379).
#[repr(C)]
#[derive(
    Clone, Copy, Debug, Default, PartialEq, Eq, Hash, FromBytes, IntoBytes, Immutable, KnownLayout,
)]
pub struct UblkParamZoned {
    /// Most open zones.
    pub max_open_zones: u32,
    /// Most active zones.
    pub max_active_zones: u32,
    /// Largest zone append in sectors.
    pub max_zone_append_sectors: u32,
    /// Reserved, zero.
    pub reserved: [u8; 20],
}

/// `struct ublk_params` (ublk_cmd.h:386), the buffer of `SET_PARAMS` and
/// `GET_PARAMS`.
///
/// C lays it out as 108 bytes of fields followed by 4 bytes of tail padding:
/// `basic` holds `u64`s, so the structure is 8-aligned and `sizeof(struct
/// ublk_params)` on x86-64 and arm64 is 112. zerocopy's `IntoBytes` refuses
/// implicit padding, so the 4 bytes are the named field `tail_pad`, which
/// makes `size_of::<UblkParams>()` equal the C size (asserted against a
/// field-for-field `#[repr(C)]` mirror without the explicit padding).
#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, FromBytes, IntoBytes, Immutable, KnownLayout)]
pub struct UblkParams {
    /// Total length of the parameters: `size_of::<UblkParams>()` (112), as
    /// [`UblkParams::new`] sets it. The driver needs `len` both ways and
    /// copies at most its own `sizeof(struct ublk_params)`.
    pub len: u32,
    /// `UBLK_PARAM_TYPE_*` bits naming the members that are meaningful.
    pub types: u32,
    /// Basic limits and attributes.
    pub basic: UblkParamBasic,
    /// Discard limits.
    pub discard: UblkParamDiscard,
    /// Device numbers (read-only).
    pub devt: UblkParamDevt,
    /// Zoned limits.
    pub zoned: UblkParamZoned,
    /// C's tail padding, named; zero.
    pub tail_pad: [u8; 4],
}

impl UblkParams {
    /// All-zero parameters with `len` set to the structure's size (112).
    #[must_use]
    pub fn new() -> Self {
        let mut params = Self::new_zeroed();
        params.len = size_of::<Self>() as u32;
        params
    }
}

impl Default for UblkParams {
    fn default() -> Self {
        Self::new()
    }
}

// ---------------------------------------------------------------------------
// Command builders
// ---------------------------------------------------------------------------

/// A buffer in this process named by a control command: its address and its
/// length in bytes.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct CtrlBuf {
    /// The address, as the driver will dereference it.
    pub addr: u64,
    /// The length in bytes.
    pub len: u16,
}

/// A ublk control command ready for `IORING_OP_URING_CMD` on a ring of
/// 128-byte SQEs (the driver refuses control commands on 64-byte SQEs): the
/// ioctl-encoded `cmd_op` and the header that fills the first 32 of the
/// SQE's 80 command bytes.
///
/// The constructors fill `queue_id` with [`UblksrvCtrlCmd::NO_QUEUE`] and
/// leave every other field zero unless the command needs it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct CtrlCommand {
    /// The ioctl-encoded command (`UBLK_U_CMD_*`), the SQE's `cmd_op`.
    pub cmd_op: u32,
    /// The header the driver reads from the SQE's command area.
    pub header: UblksrvCtrlCmd,
}

impl CtrlCommand {
    /// Bytes in the command area of a 128-byte SQE, the `cmd` of io-uring's
    /// `UringCmd80` builder.
    pub const CMD_LEN: usize = 80;

    /// Any control command for `dev_id`, with an optional buffer.
    #[must_use]
    pub const fn new(cmd_op: u32, dev_id: u32, buf: Option<CtrlBuf>) -> Self {
        let (addr, len) = match buf {
            Some(buf) => (buf.addr, buf.len),
            None => (0, 0),
        };
        Self {
            cmd_op,
            header: UblksrvCtrlCmd {
                dev_id,
                queue_id: UblksrvCtrlCmd::NO_QUEUE,
                len,
                addr,
                data: [0],
                dev_path_len: 0,
                pad: 0,
                reserved: 0,
            },
        }
    }

    /// `ADD_DEV`: `info` is a [`UblksrvCtrlDevInfo`] (64 bytes) whose `dev_id`
    /// equals `dev_id`; the driver completes it and writes it back.
    /// [`UblksrvCtrlCmd::ANY_DEV`] lets the driver choose the id.
    #[must_use]
    pub const fn add_dev(dev_id: u32, info: CtrlBuf) -> Self {
        Self::new(UBLK_U_CMD_ADD_DEV, dev_id, Some(info))
    }

    /// `DEL_DEV`: removes the device and waits until its number is freed,
    /// which is only once every reference is dropped, including an open
    /// `/dev/ublkcN` anywhere (`ublk_ctrl_del_dev` with `wait`).
    #[must_use]
    pub const fn del_dev(dev_id: u32) -> Self {
        Self::new(UBLK_U_CMD_DEL_DEV, dev_id, None)
    }

    /// `DEL_DEV_ASYNC` (ublk_cmd.h:52): removes the device like `DEL_DEV`
    /// and returns without waiting for its number to be freed
    /// (`ublk_ctrl_del_dev` without `wait`), so deleting a device whose
    /// character device a dying process still holds does not block; the
    /// number is freed when the last reference goes.
    #[must_use]
    pub const fn del_dev_async(dev_id: u32) -> Self {
        Self::new(UBLK_U_CMD_DEL_DEV_ASYNC, dev_id, None)
    }

    /// `START_DEV`: completes once every tag of every queue has a fetch
    /// pending, then exposes `/dev/ublkbN`; the driver requires the server's
    /// (positive) pid in `data[0]`.
    #[must_use]
    pub const fn start_dev(dev_id: u32, ublksrv_pid: u32) -> Self {
        let mut cmd = Self::new(UBLK_U_CMD_START_DEV, dev_id, None);
        cmd.header.data = [ublksrv_pid as u64];
        cmd
    }

    /// `STOP_DEV`.
    #[must_use]
    pub const fn stop_dev(dev_id: u32) -> Self {
        Self::new(UBLK_U_CMD_STOP_DEV, dev_id, None)
    }

    /// `SET_PARAMS`: `params` is a [`UblkParams`] with `len` set; refused
    /// while the device is live.
    #[must_use]
    pub const fn set_params(dev_id: u32, params: CtrlBuf) -> Self {
        Self::new(UBLK_U_CMD_SET_PARAMS, dev_id, Some(params))
    }

    /// `GET_PARAMS`: `params` is a [`UblkParams`] with `len` set.
    #[must_use]
    pub const fn get_params(dev_id: u32, params: CtrlBuf) -> Self {
        Self::new(UBLK_U_CMD_GET_PARAMS, dev_id, Some(params))
    }

    /// `GET_DEV_INFO`: `info` receives a [`UblksrvCtrlDevInfo`].
    #[must_use]
    pub const fn get_dev_info(dev_id: u32, info: CtrlBuf) -> Self {
        Self::new(UBLK_U_CMD_GET_DEV_INFO, dev_id, Some(info))
    }

    /// `GET_FEATURES`: the driver writes [`UBLK_FEATURES_LEN`] bytes (one
    /// `u64` of `UBLK_F_*` bits) at `addr`. It names no device.
    #[must_use]
    pub const fn get_features(addr: u64) -> Self {
        let buf = CtrlBuf {
            addr,
            len: UBLK_FEATURES_LEN,
        };
        Self::new(UBLK_U_CMD_GET_FEATURES, UblksrvCtrlCmd::ANY_DEV, Some(buf))
    }

    /// `START_USER_RECOVERY`: begins re-attaching a quiesced
    /// `UBLK_F_USER_RECOVERY` device to a new server.
    #[must_use]
    pub const fn start_user_recovery(dev_id: u32) -> Self {
        Self::new(UBLK_U_CMD_START_USER_RECOVERY, dev_id, None)
    }

    /// `END_USER_RECOVERY`: completes once the new server has a fetch pending
    /// on every tag, then resumes the device; like `START_DEV` it carries the
    /// server's pid in `data[0]`.
    #[must_use]
    pub const fn end_user_recovery(dev_id: u32, ublksrv_pid: u32) -> Self {
        let mut cmd = Self::new(UBLK_U_CMD_END_USER_RECOVERY, dev_id, None);
        cmd.header.data = [ublksrv_pid as u64];
        cmd
    }

    /// The 80 command bytes: the header, then zeros.
    #[must_use]
    pub fn cmd_bytes(&self) -> [u8; Self::CMD_LEN] {
        let mut cmd = [0; Self::CMD_LEN];
        cmd[..size_of::<UblksrvCtrlCmd>()].copy_from_slice(self.header.as_bytes());
        cmd
    }
}

/// A ublk I/O command for `IORING_OP_URING_CMD` on `/dev/ublkcN`; its 16-byte
/// payload fits a 64-byte SQE. The constructors keep `addr = 0`, which
/// `UBLK_F_USER_COPY` requires of every fetch and commit (the driver returns
/// `-EINVAL` otherwise), so a reactor cannot get it wrong.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct IoCommand {
    /// The ioctl-encoded command (`UBLK_U_IO_*`), the SQE's `cmd_op`.
    pub cmd_op: u32,
    /// The payload.
    pub cmd: UblksrvIoCmd,
}

impl IoCommand {
    /// Bytes in the command area of a 64-byte SQE, the `cmd` of io-uring's
    /// `UringCmd16` builder.
    pub const CMD_LEN: usize = 16;

    /// `FETCH_REQ` for `(q_id, tag)`: issued once per tag when a queue starts
    /// (and again after `START_USER_RECOVERY`).
    #[must_use]
    pub const fn fetch(q_id: u16, tag: u16) -> Self {
        Self {
            cmd_op: UBLK_U_IO_FETCH_REQ,
            cmd: UblksrvIoCmd {
                q_id,
                tag,
                result: 0,
                addr: 0,
            },
        }
    }

    /// `COMMIT_AND_FETCH_REQ`: completes request `(q_id, tag)` with `result`
    /// (bytes, or a negative errno) and fetches the tag's next request. For a
    /// WRITE this is the acknowledgement.
    #[must_use]
    pub const fn commit_and_fetch(q_id: u16, tag: u16, result: i32) -> Self {
        Self {
            cmd_op: UBLK_U_IO_COMMIT_AND_FETCH_REQ,
            cmd: UblksrvIoCmd {
                q_id,
                tag,
                result,
                addr: 0,
            },
        }
    }

    /// The 16 command bytes.
    #[must_use]
    pub fn cmd_bytes(&self) -> [u8; Self::CMD_LEN] {
        let mut cmd = [0; Self::CMD_LEN];
        cmd.copy_from_slice(self.cmd.as_bytes());
        cmd
    }
}

// ---------------------------------------------------------------------------
// Layout and encoding invariants (section 7.7, invariant 3)
// ---------------------------------------------------------------------------

const _: () = {
    // The two encodings the Step-2 brief pins, then the rest of the table.
    assert!(UBLK_U_CMD_START_DEV == 0xC020_7506);
    assert!(UBLK_U_IO_FETCH_REQ == 0xC010_7520);
    assert!(UBLK_U_CMD_GET_QUEUE_AFFINITY == 0x8020_7501);
    assert!(UBLK_U_CMD_GET_DEV_INFO == 0x8020_7502);
    assert!(UBLK_U_CMD_ADD_DEV == 0xC020_7504);
    assert!(UBLK_U_CMD_DEL_DEV == 0xC020_7505);
    assert!(UBLK_U_CMD_STOP_DEV == 0xC020_7507);
    assert!(UBLK_U_CMD_SET_PARAMS == 0xC020_7508);
    assert!(UBLK_U_CMD_GET_PARAMS == 0x8020_7509);
    assert!(UBLK_U_CMD_START_USER_RECOVERY == 0xC020_7510);
    assert!(UBLK_U_CMD_END_USER_RECOVERY == 0xC020_7511);
    assert!(UBLK_U_CMD_GET_DEV_INFO2 == 0x8020_7512);
    assert!(UBLK_U_CMD_GET_FEATURES == 0x8020_7513);
    assert!(UBLK_U_CMD_DEL_DEV_ASYNC == 0x8020_7514);
    assert!(UBLK_U_IO_COMMIT_AND_FETCH_REQ == 0xC010_7521);
    assert!(UBLK_U_IO_NEED_GET_DATA == 0xC010_7522);

    // The user-copy window: 25 + 16 + 12 = 53 bits above 0x8000_0000.
    assert!(UBLK_TAG_OFF == 25 && UBLK_QID_OFF == 41 && UBLKSRV_IO_BUF_TOTAL_BITS == 53);
    assert!(UBLK_MAX_NR_QUEUES == 4096);
    assert!(user_copy_offset(0, 0, 0) == UBLKSRV_IO_BUF_OFFSET);
    assert!(
        user_copy_offset(4095, u16::MAX, (1 << 25) - 1)
            == UBLKSRV_IO_BUF_OFFSET + UBLKSRV_IO_BUF_TOTAL_SIZE - 1
    );
    assert!(checked_user_copy_offset(4096, 0, 0).is_none());
    assert!(checked_user_copy_offset(0, 0, 1 << 25).is_none());

    assert!(size_of::<UblksrvCtrlCmd>() == 32);
    assert!(align_of::<UblksrvCtrlCmd>() == 8);
    assert!(offset_of!(UblksrvCtrlCmd, dev_id) == 0);
    assert!(offset_of!(UblksrvCtrlCmd, queue_id) == 4);
    assert!(offset_of!(UblksrvCtrlCmd, len) == 6);
    assert!(offset_of!(UblksrvCtrlCmd, addr) == 8);
    assert!(offset_of!(UblksrvCtrlCmd, data) == 16);
    assert!(offset_of!(UblksrvCtrlCmd, dev_path_len) == 24);
    assert!(offset_of!(UblksrvCtrlCmd, pad) == 26);
    assert!(offset_of!(UblksrvCtrlCmd, reserved) == 28);
    assert!(size_of::<UblksrvCtrlCmd>() <= CtrlCommand::CMD_LEN);

    assert!(size_of::<UblksrvCtrlDevInfo>() == 64);
    assert!(align_of::<UblksrvCtrlDevInfo>() == 8);
    assert!(offset_of!(UblksrvCtrlDevInfo, nr_hw_queues) == 0);
    assert!(offset_of!(UblksrvCtrlDevInfo, queue_depth) == 2);
    assert!(offset_of!(UblksrvCtrlDevInfo, state) == 4);
    assert!(offset_of!(UblksrvCtrlDevInfo, pad0) == 6);
    assert!(offset_of!(UblksrvCtrlDevInfo, max_io_buf_bytes) == 8);
    assert!(offset_of!(UblksrvCtrlDevInfo, dev_id) == 12);
    assert!(offset_of!(UblksrvCtrlDevInfo, ublksrv_pid) == 16);
    assert!(offset_of!(UblksrvCtrlDevInfo, pad1) == 20);
    assert!(offset_of!(UblksrvCtrlDevInfo, flags) == 24);
    assert!(offset_of!(UblksrvCtrlDevInfo, ublksrv_flags) == 32);
    assert!(offset_of!(UblksrvCtrlDevInfo, owner_uid) == 40);
    assert!(offset_of!(UblksrvCtrlDevInfo, owner_gid) == 44);
    assert!(offset_of!(UblksrvCtrlDevInfo, reserved1) == 48);
    assert!(offset_of!(UblksrvCtrlDevInfo, reserved2) == 56);

    assert!(size_of::<UblksrvIoDesc>() == 24);
    assert!(align_of::<UblksrvIoDesc>() == 8);
    assert!(offset_of!(UblksrvIoDesc, op_flags) == 0);
    assert!(offset_of!(UblksrvIoDesc, nr_sectors) == 4);
    assert!(offset_of!(UblksrvIoDesc, start_sector) == 8);
    assert!(offset_of!(UblksrvIoDesc, addr) == 16);

    assert!(size_of::<UblksrvIoCmd>() == 16);
    assert!(align_of::<UblksrvIoCmd>() == 8);
    assert!(offset_of!(UblksrvIoCmd, q_id) == 0);
    assert!(offset_of!(UblksrvIoCmd, tag) == 2);
    assert!(offset_of!(UblksrvIoCmd, result) == 4);
    assert!(offset_of!(UblksrvIoCmd, addr) == 8);
    assert!(size_of::<UblksrvIoCmd>() == IoCommand::CMD_LEN);

    assert!(size_of::<UblkParamBasic>() == 32);
    assert!(align_of::<UblkParamBasic>() == 8);
    assert!(offset_of!(UblkParamBasic, attrs) == 0);
    assert!(offset_of!(UblkParamBasic, logical_bs_shift) == 4);
    assert!(offset_of!(UblkParamBasic, physical_bs_shift) == 5);
    assert!(offset_of!(UblkParamBasic, io_opt_shift) == 6);
    assert!(offset_of!(UblkParamBasic, io_min_shift) == 7);
    assert!(offset_of!(UblkParamBasic, max_sectors) == 8);
    assert!(offset_of!(UblkParamBasic, chunk_sectors) == 12);
    assert!(offset_of!(UblkParamBasic, dev_sectors) == 16);
    assert!(offset_of!(UblkParamBasic, virt_boundary_mask) == 24);

    assert!(size_of::<UblkParamDiscard>() == 20);
    assert!(align_of::<UblkParamDiscard>() == 4);
    assert!(offset_of!(UblkParamDiscard, discard_alignment) == 0);
    assert!(offset_of!(UblkParamDiscard, discard_granularity) == 4);
    assert!(offset_of!(UblkParamDiscard, max_discard_sectors) == 8);
    assert!(offset_of!(UblkParamDiscard, max_write_zeroes_sectors) == 12);
    assert!(offset_of!(UblkParamDiscard, max_discard_segments) == 16);
    assert!(offset_of!(UblkParamDiscard, reserved0) == 18);

    assert!(size_of::<UblkParamDevt>() == 16);
    assert!(align_of::<UblkParamDevt>() == 4);
    assert!(offset_of!(UblkParamDevt, char_major) == 0);
    assert!(offset_of!(UblkParamDevt, char_minor) == 4);
    assert!(offset_of!(UblkParamDevt, disk_major) == 8);
    assert!(offset_of!(UblkParamDevt, disk_minor) == 12);

    assert!(size_of::<UblkParamZoned>() == 32);
    assert!(align_of::<UblkParamZoned>() == 4);
    assert!(offset_of!(UblkParamZoned, max_open_zones) == 0);
    assert!(offset_of!(UblkParamZoned, max_active_zones) == 4);
    assert!(offset_of!(UblkParamZoned, max_zone_append_sectors) == 8);
    assert!(offset_of!(UblkParamZoned, reserved) == 12);

    // struct ublk_params: 4 + 4 + 32 + 20 + 16 + 32 = 108 bytes of fields at
    // offsets 0, 4, 8, 40, 60 and 76, rounded up to the 8-byte alignment that
    // `basic` imposes: 112.
    assert!(size_of::<UblkParams>() == 112);
    assert!(align_of::<UblkParams>() == 8);
    assert!(offset_of!(UblkParams, len) == 0);
    assert!(offset_of!(UblkParams, types) == 4);
    assert!(offset_of!(UblkParams, basic) == 8);
    assert!(offset_of!(UblkParams, discard) == 40);
    assert!(offset_of!(UblkParams, devt) == 60);
    assert!(offset_of!(UblkParams, zoned) == 76);
    assert!(offset_of!(UblkParams, tail_pad) == 108);

    // The same fields with C's implicit tail padding, laid out by the
    // target's C rules (`#[repr(C)]`): this is `sizeof(struct ublk_params)`.
    #[allow(dead_code)]
    #[repr(C)]
    struct UblkParamsAsC {
        len: u32,
        types: u32,
        basic: UblkParamBasic,
        discard: UblkParamDiscard,
        devt: UblkParamDevt,
        zoned: UblkParamZoned,
    }
    assert!(size_of::<UblkParamsAsC>() == size_of::<UblkParams>());
    assert!(align_of::<UblkParamsAsC>() == align_of::<UblkParams>());
};

// ---------------------------------------------------------------------------
// Devices and SQEs (Linux)
// ---------------------------------------------------------------------------

#[cfg(target_os = "linux")]
mod linux {
    use std::io;
    use std::os::fd::{AsFd, AsRawFd, BorrowedFd, OwnedFd, RawFd};

    use io_uring::{opcode, squeue, types};
    use rustix::fs::{fcntl_getfl, fcntl_setfl, open, Mode, OFlags};

    use super::{CtrlCommand, IoCommand};

    /// The ublk control device, `/dev/ublk-control`, opened
    /// `O_RDWR | O_CLOEXEC`. Linux only.
    ///
    /// Control commands are `IORING_OP_URING_CMD`s on this descriptor, on a
    /// ring of 128-byte SQEs (`IoUring<squeue::Entry128, _>`): the driver
    /// rejects them on 64-byte SQEs.
    #[derive(Debug)]
    pub struct UblkControl {
        fd: OwnedFd,
    }

    impl UblkControl {
        /// Where udev creates the control device.
        pub const PATH: &'static str = "/dev/ublk-control";

        /// Opens [`UblkControl::PATH`] `O_RDWR | O_CLOEXEC`.
        ///
        /// # Errors
        /// The `open(2)` error: `ENOENT` when `ublk_drv` is not loaded,
        /// `EACCES` without permission on the node.
        pub fn open() -> io::Result<Self> {
            let fd = open(Self::PATH, OFlags::RDWR | OFlags::CLOEXEC, Mode::empty())?;
            Ok(Self { fd })
        }

        /// Wraps a descriptor of the control device opened elsewhere (for
        /// example handed over through the service manager's descriptor
        /// store).
        #[must_use]
        pub fn from_fd(fd: OwnedFd) -> Self {
            Self { fd }
        }

        /// The SQE for `cmd`, built with io-uring's `UringCmd80`: opcode
        /// `IORING_OP_URING_CMD`, `cmd_op` and the 80 command bytes, no flags
        /// and `user_data` 0 (set it with `Entry128::user_data`). The header
        /// names buffers by address: they must stay valid until the
        /// command's CQE is reaped.
        #[must_use]
        pub fn command_sqe(&self, cmd: &CtrlCommand) -> squeue::Entry128 {
            opcode::UringCmd80::new(types::Fd(self.fd.as_raw_fd()), cmd.cmd_op)
                .cmd(cmd.cmd_bytes())
                .build()
        }
    }

    impl AsFd for UblkControl {
        fn as_fd(&self) -> BorrowedFd<'_> {
            self.fd.as_fd()
        }
    }

    impl AsRawFd for UblkControl {
        fn as_raw_fd(&self) -> RawFd {
            self.fd.as_raw_fd()
        }
    }

    /// A ublk character device, `/dev/ublkcN`, opened
    /// `O_RDWR | O_NONBLOCK | O_CLOEXEC`. Linux only.
    ///
    /// `O_NONBLOCK` is a requirement, not a preference (C03): `ublk_ch_fops`
    /// sets no `FMODE_NOWAIT`, so without it io_uring punts every user-copy
    /// read to an io-wq worker. The flag is taken from the open file, so it
    /// must be set before the descriptor is registered as a fixed file;
    /// every constructor here guarantees it.
    #[derive(Debug)]
    pub struct UblkChar {
        fd: OwnedFd,
        dev_id: u32,
    }

    impl UblkChar {
        /// Opens `/dev/ublkc{dev_id}` `O_RDWR | O_NONBLOCK | O_CLOEXEC`.
        ///
        /// # Errors
        /// The `open(2)` error, `ENOENT` when the device does not exist.
        pub fn open(dev_id: u32) -> io::Result<Self> {
            let path = format!("/dev/ublkc{dev_id}");
            let flags = OFlags::RDWR | OFlags::NONBLOCK | OFlags::CLOEXEC;
            let fd = open(path.as_str(), flags, Mode::empty())?;
            Ok(Self { fd, dev_id })
        }

        /// Wraps a descriptor of `/dev/ublkc{dev_id}` opened elsewhere (for
        /// example kept in the service manager's descriptor store across a
        /// restart), setting `O_NONBLOCK` on it if it is missing.
        ///
        /// # Errors
        /// The `fcntl(2)` error.
        pub fn from_fd(fd: OwnedFd, dev_id: u32) -> io::Result<Self> {
            let flags = fcntl_getfl(&fd)?;
            if !flags.contains(OFlags::NONBLOCK) {
                fcntl_setfl(&fd, flags | OFlags::NONBLOCK)?;
            }
            Ok(Self { fd, dev_id })
        }

        /// The device id `N` of `/dev/ublkcN`.
        #[must_use]
        pub const fn dev_id(&self) -> u32 {
            self.dev_id
        }

        /// The SQE for `cmd`, built with io-uring's `UringCmd16`: opcode
        /// `IORING_OP_URING_CMD`, `cmd_op` and the 16 command bytes, no flags
        /// and `user_data` 0 (set it with `Entry::user_data`; the tag is the
        /// usual choice).
        #[must_use]
        pub fn command_sqe(&self, cmd: &IoCommand) -> squeue::Entry {
            opcode::UringCmd16::new(types::Fd(self.fd.as_raw_fd()), cmd.cmd_op)
                .cmd(cmd.cmd_bytes())
                .build()
        }
    }

    impl AsFd for UblkChar {
        fn as_fd(&self) -> BorrowedFd<'_> {
            self.fd.as_fd()
        }
    }

    impl AsRawFd for UblkChar {
        fn as_raw_fd(&self) -> RawFd {
            self.fd.as_raw_fd()
        }
    }

    use io_uring::{cqueue, IoUring};
    use memmap2::{Mmap, MmapOptions};
    use rustix::io::{pread, pwrite};
    use zerocopy::FromBytes;

    use super::{
        checked_user_copy_offset, UblksrvIoDesc, UBLKSRV_CMD_BUF_OFFSET, UBLK_MAX_QUEUE_DEPTH,
    };

    /// The control device with a private ring of 128-byte SQEs, running
    /// one control command at a time: synchronously what device creation
    /// and teardown need (`ADD_DEV`, `SET_PARAMS`, `STOP_DEV`, `DEL_DEV`,
    /// the recovery pair), and `START_DEV` started and polled
    /// ([`UblkControlRing::begin`], [`UblkControlRing::poll`]), because it
    /// completes only once the queue has served the reads of the new
    /// disk's partition scan, which the thread that waits here serves.
    pub struct UblkControlRing {
        control: UblkControl,
        ring: IoUring<squeue::Entry128, cqueue::Entry>,
        /// The command [`UblkControlRing::begin`] submitted, until its CQE
        /// is reaped.
        in_flight: Option<u32>,
    }

    impl UblkControlRing {
        /// A private two-entry SQE128 ring over `control`.
        ///
        /// # Errors
        /// The `io_uring_setup(2)` error.
        pub fn new(control: UblkControl) -> io::Result<Self> {
            let ring = IoUring::<squeue::Entry128, cqueue::Entry>::builder().build(2)?;
            Ok(Self {
                control,
                ring,
                in_flight: None,
            })
        }

        /// The control device.
        #[must_use]
        pub const fn control(&self) -> &UblkControl {
            &self.control
        }

        /// `true` while a command [`begin`](Self::begin) submitted has not
        /// been reaped by [`poll`](Self::poll).
        #[must_use]
        pub const fn in_flight(&self) -> bool {
            self.in_flight.is_some()
        }

        /// Submits `cmd`, which must name no buffer, and returns without
        /// waiting: the driver runs control commands in an io-wq worker
        /// (`ublk_ctrl_uring_cmd` answers `-EAGAIN` to the inline issue),
        /// so the caller goes on serving while it runs, and reaps its
        /// result with [`poll`](Self::poll).
        ///
        /// # Errors
        /// A command that names a buffer or one already in flight
        /// ([`io::ErrorKind::InvalidInput`]), or the ring's error.
        pub fn begin(&mut self, cmd: &CtrlCommand) -> io::Result<()> {
            if cmd.header.addr != 0 || cmd.header.len != 0 {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "a command started without waiting may name no buffer",
                ));
            }
            if self.in_flight.is_some() {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "a control command is already in flight",
                ));
            }
            let sqe = self
                .control
                .command_sqe(cmd)
                .user_data(u64::from(cmd.cmd_op));
            // SAFETY: `push` requires every address the entry names to stay
            // valid until the operation completes. The entry names no
            // memory (checked above: no buffer address, no length); the
            // command block is copied into the SQE, and the ring that holds
            // it is owned by `self`, which reaps its CQE in `poll` (or, if
            // dropped first, io_uring's own teardown waits for the
            // operation, which touches no caller memory).
            let pushed = unsafe { self.ring.submission().push(&sqe) };
            if pushed.is_err() {
                return Err(io::Error::other("the control ring refused its entry"));
            }
            // From here the entry is the ring's: its CQE is the one `poll`
            // reaps (a refused submission leaves it queued, and `poll`'s
            // own submission sends it), and `execute` waits its turn.
            self.in_flight = Some(cmd.cmd_op);
            loop {
                match self.ring.submit() {
                    Ok(_) => return Ok(()),
                    Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                    Err(e) => return Err(e),
                }
            }
        }

        /// The result of the command [`begin`](Self::begin) submitted:
        /// `Ok(None)` while it runs, `Ok(Some(result))` once its CQE is
        /// reaped (the call that reaps it is the last for that command).
        /// Enters the kernel without waiting, so the completion's task work
        /// runs.
        ///
        /// # Errors
        /// No command in flight ([`io::ErrorKind::InvalidInput`]), the
        /// ring's error, or the command's negative result as an OS error.
        pub fn poll(&mut self) -> io::Result<Option<i32>> {
            if self.in_flight.is_none() {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "no control command is in flight",
                ));
            }
            match self.ring.submit() {
                Ok(_) => {}
                Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                Err(e) => return Err(e),
            }
            let Some(cqe) = self.ring.completion().next() else {
                return Ok(None);
            };
            self.in_flight = None;
            let result = cqe.result();
            if result < 0 {
                return Err(io::Error::from_raw_os_error(-result));
            }
            Ok(Some(result))
        }

        /// Runs `cmd` and returns its result (0, or a negative errno which
        /// is also returned as an error). `buf` is the command's IN or OUT
        /// buffer and must be the one `cmd.header.addr` names with
        /// `cmd.header.len` bytes (checked), so it is alive for the call.
        ///
        /// # Errors
        /// A buffer that does not match the command, a command started with
        /// [`begin`](Self::begin) still in flight, the ring's error, or the
        /// command's negative result as an OS error.
        pub fn execute(&mut self, cmd: &CtrlCommand, buf: Option<&mut [u8]>) -> io::Result<i32> {
            if self.in_flight.is_some() {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "a control command started without waiting is still in flight",
                ));
            }
            match &buf {
                Some(b) => {
                    if b.as_ptr().expose_provenance() as u64 != cmd.header.addr
                        || b.len() != usize::from(cmd.header.len)
                    {
                        return Err(io::Error::new(
                            io::ErrorKind::InvalidInput,
                            "the buffer is not the one the command names",
                        ));
                    }
                }
                None => {
                    if cmd.header.addr != 0 || cmd.header.len != 0 {
                        return Err(io::Error::new(
                            io::ErrorKind::InvalidInput,
                            "the command names a buffer that was not given",
                        ));
                    }
                }
            }
            let sqe = self
                .control
                .command_sqe(cmd)
                .user_data(u64::from(cmd.cmd_op));
            // SAFETY: `push` requires the entry's addresses to stay valid
            // for the operation. The only memory the entry names is `buf`,
            // which the caller lends for the whole call (checked above to
            // be exactly the command's address and length) and which this
            // function does not return from until the CQE is reaped or
            // the ring is closed with the entry never submitted.
            let pushed = unsafe { self.ring.submission().push(&sqe) };
            if pushed.is_err() {
                return Err(io::Error::other("the control ring refused its entry"));
            }
            let result = loop {
                match self.ring.submit_and_wait(1) {
                    Ok(_) => {}
                    Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                    Err(e) => {
                        // Keep `buf` alive until the kernel is known to be
                        // done: a submitted entry with no CQE is waited
                        // for, a never-submitted one is dropped with the
                        // ring.
                        if self.ring.submission().is_empty() {
                            let mut sink = 0;
                            while self.ring.completion().next().is_none() && sink < 1_000_000 {
                                let _ = self.ring.submit_and_wait(1);
                                sink += 1;
                            }
                        }
                        return Err(e);
                    }
                }
                if let Some(cqe) = self.ring.completion().next() {
                    break cqe.result();
                }
            };
            let _ = buf;
            if result < 0 {
                return Err(io::Error::from_raw_os_error(-result));
            }
            Ok(result)
        }
    }

    impl std::fmt::Debug for UblkControlRing {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.debug_struct("UblkControlRing")
                .field("control", &self.control)
                .finish()
        }
    }

    /// `sizeof(struct ublksrv_io_desc) * UBLK_MAX_QUEUE_DEPTH`: the stride
    /// of one queue's descriptor array in the `mmap` offset space of
    /// `/dev/ublkcN` (`ublk_ch_mmap`, `drivers/block/ublk_drv.c`).
    pub const UBLK_QUEUE_CMD_BUF_STRIDE: u64 =
        size_of::<UblksrvIoDesc>() as u64 * UBLK_MAX_QUEUE_DEPTH as u64;

    /// One hardware queue of a ublk device served from this process: the
    /// character device, a ring of 64-byte SQEs for its `FETCH_REQ` and
    /// `COMMIT_AND_FETCH_REQ` commands, and the read-only mapping of the
    /// queue's request descriptors. Every command of the queue must be
    /// issued from the thread that issued its first fetch (the driver pins
    /// the queue to that task).
    pub struct UblkQueue {
        chr: UblkChar,
        ring: IoUring<squeue::Entry, cqueue::Entry>,
        descs: Mmap,
        q_id: u16,
        depth: u16,
        pushed: usize,
    }

    impl UblkQueue {
        /// Queue `q_id` of `chr`, `depth` tags deep (at most
        /// [`UBLK_MAX_QUEUE_DEPTH`]), with a ring of `entries` entries (at
        /// least `depth`).
        ///
        /// # Errors
        /// The `mmap(2)` or `io_uring_setup(2)` error, or a depth over the
        /// bound.
        pub fn new(chr: UblkChar, q_id: u16, depth: u16, entries: u32) -> io::Result<Self> {
            if depth == 0 || u32::from(depth) > UBLK_MAX_QUEUE_DEPTH {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "queue depth out of range",
                ));
            }
            let bytes = usize::from(depth) * size_of::<UblksrvIoDesc>();
            let len = bytes.next_multiple_of(4096);
            let offset = UBLKSRV_CMD_BUF_OFFSET + u64::from(q_id) * UBLK_QUEUE_CMD_BUF_STRIDE;
            // SAFETY: memmap2 requires that the mapped file not be changed
            // underneath the mapping in ways the mapping does not expect.
            // The descriptor array of a ublk queue is driver memory that
            // the driver writes exactly once per request, before it
            // completes that request's fetch, and this value reads a
            // descriptor only after reaping that completion (`desc` takes
            // the tag of a reaped fetch), so no read overlaps a write. The
            // mapping is read-only (`map`, not `map_mut`) and the driver
            // enforces `offset` and `len` (`ublk_ch_mmap` refuses any other
            // length than the page-rounded array of this queue).
            let descs = unsafe { MmapOptions::new().offset(offset).len(len).map(&chr) }?;
            let ring = IoUring::builder().build(entries.max(u32::from(depth)))?;
            Ok(Self {
                chr,
                ring,
                descs,
                q_id,
                depth,
                pushed: 0,
            })
        }

        /// The character device.
        #[must_use]
        pub const fn chr(&self) -> &UblkChar {
            &self.chr
        }

        /// The queue id.
        #[must_use]
        pub const fn q_id(&self) -> u16 {
            self.q_id
        }

        /// Tags in the queue.
        #[must_use]
        pub const fn depth(&self) -> u16 {
            self.depth
        }

        /// The descriptor of `tag`, as the driver wrote it before
        /// completing the tag's fetch; `None` for a tag beyond the depth.
        #[must_use]
        pub fn desc(&self, tag: u16) -> Option<UblksrvIoDesc> {
            if tag >= self.depth {
                return None;
            }
            let size = size_of::<UblksrvIoDesc>();
            let at = usize::from(tag) * size;
            UblksrvIoDesc::read_from_bytes(&self.descs[at..at + size]).ok()
        }

        /// Pushes `FETCH_REQ` for `tag`.
        ///
        /// # Errors
        /// [`crate::block::Full`] when the ring has no room.
        pub fn push_fetch(&mut self, tag: u16) -> Result<(), crate::block::Full> {
            self.push_command(&IoCommand::fetch(self.q_id, tag), tag)
        }

        /// Pushes `COMMIT_AND_FETCH_REQ` for `tag` with `result` (bytes
        /// transferred, or a negative errno).
        ///
        /// # Errors
        /// [`crate::block::Full`] when the ring has no room.
        pub fn push_commit(&mut self, tag: u16, result: i32) -> Result<(), crate::block::Full> {
            self.push_command(&IoCommand::commit_and_fetch(self.q_id, tag, result), tag)
        }

        fn push_command(&mut self, cmd: &IoCommand, tag: u16) -> Result<(), crate::block::Full> {
            let sqe = self.chr.command_sqe(cmd).user_data(u64::from(tag));
            // SAFETY: `push` requires the entry's addresses to stay valid
            // for the operation. An I/O command under `UBLK_F_USER_COPY`
            // names no memory of this process (`addr` is 0 and the 16
            // command bytes travel inline in the SQE), and the descriptor
            // it names is owned by `self.chr` for the queue's life.
            let pushed = unsafe { self.ring.submission().push(&sqe) };
            if pushed.is_err() {
                return Err(crate::block::Full);
            }
            self.pushed += 1;
            Ok(())
        }

        /// Submits every pushed command.
        ///
        /// # Errors
        /// The `io_uring_enter(2)` error (`EINTR` is retried).
        pub fn submit(&mut self) -> io::Result<usize> {
            loop {
                match self.ring.submit() {
                    Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                    other => return other,
                }
            }
        }

        /// Submits and waits for at least one completion (a request
        /// arriving, or a commit's own completion).
        ///
        /// # Errors
        /// The `io_uring_enter(2)` error (`EINTR` is retried).
        pub fn submit_and_wait(&mut self) -> io::Result<usize> {
            loop {
                match self.ring.submit_and_wait(1) {
                    Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                    other => return other,
                }
            }
        }

        /// Moves every available completion into `out` as `(tag, result)`:
        /// `UBLK_IO_RES_OK` means a request waits in the tag's descriptor,
        /// [`super::UBLK_IO_RES_ABORT`] that the tag must not be fetched
        /// again.
        pub fn reap(&mut self, out: &mut Vec<(u16, i32)>) -> usize {
            let mut n = 0;
            let mut cq = self.ring.completion();
            cq.sync();
            for cqe in cq.by_ref() {
                let tag = u16::try_from(cqe.user_data()).unwrap_or(u16::MAX);
                out.push((tag, cqe.result()));
                n += 1;
            }
            n
        }

        /// Copies `dst.len()` bytes of a WRITE request's data out of the
        /// driver, from byte `off` of request `tag` (`pread` at the
        /// user-copy position).
        ///
        /// # Errors
        /// The `pread(2)` error, or a short copy as `UnexpectedEof`.
        pub fn read_data(&self, tag: u16, off: u32, dst: &mut [u8]) -> io::Result<()> {
            let pos = checked_user_copy_offset(self.q_id, tag, off).ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "user-copy position out of range",
                )
            })?;
            let mut done = 0usize;
            while done < dst.len() {
                let n = pread(&self.chr, &mut dst[done..], pos + done as u64)?;
                if n == 0 {
                    return Err(io::Error::new(
                        io::ErrorKind::UnexpectedEof,
                        "short user copy",
                    ));
                }
                done += n;
            }
            Ok(())
        }

        /// Copies `src` into a READ request's data, at byte `off` of
        /// request `tag` (`pwrite` at the user-copy position).
        ///
        /// # Errors
        /// The `pwrite(2)` error, or a short copy as `WriteZero`.
        pub fn write_data(&self, tag: u16, off: u32, src: &[u8]) -> io::Result<()> {
            let pos = checked_user_copy_offset(self.q_id, tag, off).ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "user-copy position out of range",
                )
            })?;
            let mut done = 0usize;
            while done < src.len() {
                let n = pwrite(&self.chr, &src[done..], pos + done as u64)?;
                if n == 0 {
                    return Err(io::Error::new(io::ErrorKind::WriteZero, "short user copy"));
                }
                done += n;
            }
            Ok(())
        }
    }

    impl std::fmt::Debug for UblkQueue {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.debug_struct("UblkQueue")
                .field("dev_id", &self.chr.dev_id())
                .field("q_id", &self.q_id)
                .field("depth", &self.depth)
                .field("pushed", &self.pushed)
                .finish()
        }
    }
}

#[cfg(target_os = "linux")]
pub use linux::{UblkChar, UblkControl, UblkControlRing, UblkQueue, UBLK_QUEUE_CMD_BUF_STRIDE};
