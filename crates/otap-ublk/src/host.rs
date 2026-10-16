//! The kernel's ublk control plane as a daemon uses it (Step 6B): the
//! devices that exist on the host, the creation of a new one, the
//! re-attach of one whose server died, and the deletion of a stale one.
//!
//! [`UblkHost`] is that control plane; `device::LinuxHost` is the real one
//! over `/dev/ublk-control` (Linux) and [`crate::mock_kernel::MockHost`]
//! models the 6.12 driver in memory, on any host, so the daemon's restart
//! reconciliation and the reaper are tested on the Windows development PC.
//! [`HostDevice`] is one device this process holds: its queue, which
//! becomes the engine's front door, and the control half that brings it
//! live ([`DeviceControl`]): `START_DEV` for a created device,
//! `END_USER_RECOVERY` for a re-attached one, both issued without waiting
//! and polled while the queue is served ([`crate::start_while_serving`]).
//!
//! A device also carries OpenTAP's identity in the one field the driver
//! keeps for its server and never reads, `ublksrv_flags` ([`device_stamp`]):
//! which pool and which volume it exposes. Any process can read it back with
//! `GET_DEV_INFO`, so a restarted daemon (or `otapctl device list` with no
//! pool booted) tells its own devices from another pool's and from other
//! software's without any state of its own.

use core::fmt;

use otap_format::checksum::crc32c;
use otap_format::Uuid;
use otap_sys::ublk::{
    UblksrvCtrlDevInfo, UBLK_F_USER_RECOVERY, UBLK_F_USER_RECOVERY_REISSUE, UBLK_S_DEV_DEAD,
    UBLK_S_DEV_LIVE, UBLK_S_DEV_QUIESCED,
};

use crate::frontdoor::{Transport, TransportError};
use crate::spec::DeviceSpec;
use crate::start::DeviceControl;

/// A queue behind a box: the front door's transport whatever the control
/// plane (`UblkFrontDoor<BoxedTransport>`).
pub type BoxedTransport = Box<dyn Transport>;

/// `EBUSY`: `START_USER_RECOVERY` before the dead server has released the
/// device or before the driver has quiesced it; opening a character device
/// another process holds.
pub const EBUSY: i32 = 16;
/// `EACCES`: `ADD_DEV` once `ublks_max` devices exist.
pub const EACCES: i32 = 13;
/// `EINTR`: a control command whose wait was interrupted.
pub const EINTR: i32 = 4;

/// The kernel's state of a device (`UBLK_S_DEV_*`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum DevState {
    /// Added and never started, or stopped: no block device is exposed.
    Dead,
    /// Started and served, or served by a server that died and whose
    /// requests the driver has not yet quiesced.
    Live,
    /// Its server died (with `UBLK_F_USER_RECOVERY`): the block device
    /// exists, its I/O waits, and a new server may re-attach it.
    Quiesced,
    /// A state this crate does not know.
    Other(u16),
}

impl DevState {
    /// The state of `UblksrvCtrlDevInfo::state`.
    #[must_use]
    pub const fn from_raw(state: u16) -> Self {
        match state {
            UBLK_S_DEV_DEAD => Self::Dead,
            UBLK_S_DEV_LIVE => Self::Live,
            UBLK_S_DEV_QUIESCED => Self::Quiesced,
            other => Self::Other(other),
        }
    }

    /// The raw value.
    #[must_use]
    pub const fn raw(self) -> u16 {
        match self {
            Self::Dead => UBLK_S_DEV_DEAD,
            Self::Live => UBLK_S_DEV_LIVE,
            Self::Quiesced => UBLK_S_DEV_QUIESCED,
            Self::Other(v) => v,
        }
    }

    /// `dead`, `live`, `quiesced`, or `state <n>`.
    #[must_use]
    pub fn word(self) -> String {
        match self {
            Self::Dead => "dead".to_string(),
            Self::Live => "live".to_string(),
            Self::Quiesced => "quiesced".to_string(),
            Self::Other(v) => format!("state {v}"),
        }
    }
}

/// One device as `GET_DEV_INFO` (and `GET_PARAMS`, for the size) describe
/// it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct DevInfo {
    /// `N` of `/dev/ublkbN` and `/dev/ublkcN`.
    pub dev_id: u32,
    /// The kernel's state.
    pub state: DevState,
    /// The pid the last `START_DEV` or `END_USER_RECOVERY` named, `-1` after
    /// `STOP_DEV`, 0 before any start.
    pub pid: i32,
    /// The `UBLK_F_*` bits the device was granted at `ADD_DEV`.
    pub flags: u64,
    /// `ublksrv_flags`: OpenTAP's stamp ([`device_stamp`]), or what other
    /// software put there.
    pub stamp: u64,
    /// Hardware queues.
    pub queues: u16,
    /// Tags per queue.
    pub depth: u16,
    /// The largest request in bytes.
    pub max_io_bytes: u32,
    /// The capacity in 4 KiB blocks, when `GET_PARAMS` could be read.
    pub size_blocks: Option<u64>,
}

impl DevInfo {
    /// The description of `info`, with the capacity in 512-byte sectors
    /// `GET_PARAMS` reported, if read.
    #[must_use]
    pub fn from_raw(info: &UblksrvCtrlDevInfo, dev_sectors: Option<u64>) -> Self {
        Self {
            dev_id: info.dev_id,
            state: DevState::from_raw(info.state),
            pid: info.ublksrv_pid,
            flags: info.flags,
            stamp: info.ublksrv_flags,
            queues: info.nr_hw_queues,
            depth: info.queue_depth,
            max_io_bytes: info.max_io_buf_bytes,
            size_blocks: dev_sectors.map(|s| s / 8),
        }
    }

    /// The device survives its server (`UBLK_F_USER_RECOVERY`).
    #[must_use]
    pub const fn can_recover(&self) -> bool {
        self.flags & UBLK_F_USER_RECOVERY != 0
    }

    /// The requests a dead server held are delivered again after a
    /// re-attach instead of failing (`UBLK_F_USER_RECOVERY_REISSUE`).
    #[must_use]
    pub const fn reissues(&self) -> bool {
        self.flags & UBLK_F_USER_RECOVERY_REISSUE != 0
    }

    /// The block device's path, `/dev/ublkbN`.
    #[must_use]
    pub fn block_path(&self) -> String {
        format!("/dev/ublkb{}", self.dev_id)
    }

    /// OpenTAP's stamp, when the device carries one.
    #[must_use]
    pub const fn opentap(&self) -> Option<Stamp> {
        read_stamp(self.stamp)
    }
}

/// The magic in the top 16 bits of an OpenTAP device's `ublksrv_flags`.
pub const STAMP_MAGIC: u16 = 0x07A9;

/// What an OpenTAP device's `ublksrv_flags` say.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Stamp {
    /// The pool's fingerprint ([`pool_fingerprint`]).
    pub pool: u32,
    /// The volume's index.
    pub vol: u16,
}

/// A pool's fingerprint: the CRC-32C of its aggregate identity. Two pools'
/// fingerprints collide with a chance of one in 2^32; a device stamped
/// with this pool's fingerprint is still checked against the volume it
/// claims (its size, its shape) before it is re-attached.
#[must_use]
pub fn pool_fingerprint(agg_id: &Uuid) -> u32 {
    crc32c(agg_id.as_array())
}

/// The `ublksrv_flags` of the device of volume `vol` of the pool with
/// fingerprint `pool`: `STAMP_MAGIC << 48 | pool << 16 | vol` (`vol` is
/// below `MAX_VOLS`, 1024, so it fits its 16 bits).
#[must_use]
pub const fn device_stamp(pool: u32, vol: u32) -> u64 {
    (STAMP_MAGIC as u64) << 48 | (pool as u64) << 16 | (vol as u64 & 0xFFFF)
}

/// The stamp in `ublksrv_flags`, or `None` when the top 16 bits are not
/// [`STAMP_MAGIC`] (a device of other software, or of a Step 5 binary,
/// which left the field 0).
#[must_use]
pub const fn read_stamp(raw: u64) -> Option<Stamp> {
    if (raw >> 48) as u16 != STAMP_MAGIC {
        return None;
    }
    Some(Stamp {
        pool: (raw >> 16) as u32,
        vol: raw as u16,
    })
}

/// One device this process holds: created or re-attached, with its queue's
/// fetches submitted. Its [`DeviceControl`] brings it live (`START_DEV` for
/// a created device, `END_USER_RECOVERY` for a re-attached one) while the
/// queue, taken with [`take_transport`](HostDevice::take_transport), is
/// served on the thread that submitted the fetches.
pub trait HostDevice: DeviceControl {
    /// The device id `N`.
    fn dev_id(&self) -> u32;

    /// The block device's path, `/dev/ublkbN`.
    fn block_path(&self) -> String {
        format!("/dev/ublkb{}", self.dev_id())
    }

    /// Tags of its one queue.
    fn depth(&self) -> u16;

    /// Takes the queue (once). It holds `/dev/ublkcN` open: dropping it
    /// closes the character device, which `DEL_DEV` waits for.
    fn take_transport(&mut self) -> Option<BoxedTransport>;

    /// `true` while the command [`DeviceControl::begin_start`] issued has
    /// not completed.
    fn start_in_flight(&self) -> bool;

    /// Stops the device (when it was live) and deletes it, waiting for its
    /// number to be freed. The queue must have been dropped first.
    ///
    /// # Errors
    /// The command's error.
    fn delete(self: Box<Self>) -> Result<(), TransportError>;
}

/// The host's devices as a listing reports them: each of `ids` described
/// by `describe` (`GET_DEV_INFO` and `GET_PARAMS`, answering `Ok(None)` for
/// a device deleted since the listing, `Err(errno)` for a refusal). The
/// one policy both [`UblkHost`] implementations follow, so the in-memory
/// driver exercises what the Linux one does.
///
/// A device that answers `EINVAL` is left out: the 6.12 driver refuses
/// `GET_DEV_INFO` without the character device's path for a device made
/// with `UBLK_F_UNPRIVILEGED_DEV` (`ublk_ctrl_uring_cmd_permission`), and
/// OpenTAP makes only privileged devices, so such a device is other
/// software's and none of this daemon's business. One of them used to fail
/// the whole listing, and with it every pool boot (the Step 6 review's R4).
///
/// # Errors
/// Any other refusal, which fails the whole listing.
pub fn listing(
    ids: &[u32],
    mut describe: impl FnMut(u32) -> Result<Option<DevInfo>, i32>,
) -> Result<Vec<DevInfo>, TransportError> {
    let mut out = Vec::with_capacity(ids.len());
    for &id in ids {
        match describe(id) {
            Ok(Some(info)) => out.push(info),
            Ok(None) => {}
            Err(errno) if errno == crate::frontdoor::EINVAL => {}
            Err(errno) => {
                return Err(TransportError {
                    what: "GET_DEV_INFO",
                    errno,
                })
            }
        }
    }
    Ok(out)
}

/// The control plane of the host's ublk driver.
pub trait UblkHost {
    /// This process's pid, which `START_DEV` and `END_USER_RECOVERY` name.
    fn pid(&self) -> u32;

    /// Whether process `pid` exists (and is not a zombie).
    fn pid_alive(&mut self, pid: i32) -> bool;

    /// Every device on the host, by id.
    ///
    /// # Errors
    /// The control device cannot be reached.
    fn list(&mut self) -> Result<Vec<DevInfo>, TransportError>;

    /// One device, `None` when it does not exist.
    ///
    /// # Errors
    /// The control device cannot be reached, or the command failed.
    fn info(&mut self, dev_id: u32) -> Result<Option<DevInfo>, TransportError>;

    /// Creates a device shaped by `spec` with `UBLK_F_USER_COPY`,
    /// `UBLK_F_USER_RECOVERY`, `UBLK_F_USER_RECOVERY_REISSUE` and the ioctl
    /// encoding, `ublksrv_flags` = `stamp`, opens its queue and submits a
    /// fetch on every tag, on the calling thread (the one that will serve
    /// it). The device is not started.
    ///
    /// # Errors
    /// Any command's error (`EACCES` once the driver's `ublks_max` devices
    /// exist); the device is deleted again on a failure after `ADD_DEV`.
    fn create(
        &mut self,
        spec: &DeviceSpec,
        stamp: u64,
    ) -> Result<Box<dyn HostDevice>, TransportError>;

    /// `START_USER_RECOVERY`: the dead server's queue state is forgotten so
    /// a new server can fetch.
    ///
    /// # Errors
    /// `EBUSY` while the dead server still holds `/dev/ublkcN` or the
    /// driver has not quiesced the device; `EINVAL` without
    /// `UBLK_F_USER_RECOVERY`, or when no queue is ready (a recovery begun
    /// and never fetched, a device never started); `ENODEV` for no device.
    fn start_recovery(&mut self, dev_id: u32) -> Result<(), TransportError>;

    /// After [`start_recovery`](UblkHost::start_recovery): opens
    /// `/dev/ublkcN`, maps the queue and submits a fetch on every tag, on
    /// the calling thread. `END_USER_RECOVERY` is the returned device's
    /// [`DeviceControl::begin_start`].
    ///
    /// # Errors
    /// Opening or mapping the queue failed.
    fn reattach(&mut self, info: &DevInfo) -> Result<Box<dyn HostDevice>, TransportError>;

    /// Deletes a device this process does not serve: `STOP_DEV` (which fails
    /// the I/O a quiesced device holds), then `DEL_DEV_ASYNC` (which does
    /// not wait for a dying process to release `/dev/ublkcN`).
    ///
    /// # Errors
    /// The command's error.
    fn reap(&mut self, dev_id: u32) -> Result<(), TransportError>;
}

impl fmt::Debug for dyn HostDevice {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("HostDevice")
            .field("dev_id", &self.dev_id())
            .field("start_in_flight", &self.start_in_flight())
            .finish()
    }
}

impl fmt::Debug for dyn UblkHost {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("UblkHost")
            .field("pid", &self.pid())
            .finish()
    }
}
