//! `otap-ublk`: the ublk front door (round 3, section 1.3, "Write path";
//! Step 4 brief, "Opus-Protocols-Ublk").
//!
//! A ublk device is a kernel block device whose requests a user process
//! serves. Under `UBLK_F_USER_COPY` the driver describes each request in a
//! per-tag descriptor and the server moves the data with reads and writes
//! on the character device at the user-copy position. This crate turns
//! that into the engine's [`otap_cp::FrontDoor`]:
//!
//! - [`frontdoor::UblkFrontDoor`] is the state machine: it decodes
//!   descriptors into [`otap_cp::Request`]s (read, write, flush, discard,
//!   write zeroes; anything else is answered `EOPNOTSUPP`), hands them to
//!   the engine under a back-pressure watermark, copies a write's data into
//!   the arena run the engine gives it and a read's data back to the
//!   driver, and turns the engine's [`otap_cp::Status`] into the request's
//!   result (bytes, or `-errno`: `EIO` under nvfail, `ENODEV` offline,
//!   `ENOSPC` on a full pool, `EINVAL` for a request the engine cannot
//!   take). A completion for a tag the driver has since reissued is
//!   recognised by its generation and dropped.
//! - [`frontdoor::Transport`] is the queue as the state machine sees it:
//!   events (a request on a tag, an abort), the two copies, the commit.
//!   [`mock::MockTransport`] drives it in the tests on any host, which is
//!   how every path of the state machine is unit- and property-tested on
//!   the Windows development PC; `device::LinuxTransport` is the real
//!   queue over [`otap_sys::ublk`].
//! - `device` (Linux) creates and starts the device (`ADD_DEV` with the
//!   recovery flags and OpenTAP's stamp, `SET_PARAMS` with a 4 KiB block, a
//!   volatile cache and FUA, discard limits; `START_DEV` once every tag has
//!   a fetch pending), re-attaches one after its server died, is the host's
//!   control plane (`device::LinuxHost`), and runs the engine's tick loop
//!   against a device.
//! - [`start`] is the start handshake (Step 6A): `START_DEV` is issued
//!   without waiting and polled while the queue is served, because the
//!   driver's partition scan reads the new disk through that queue before
//!   the command returns; [`mock::MockStart`] models it on any host.
//! - [`host`] is the control plane as a daemon uses it (Step 6B): the
//!   devices on the host ([`host::UblkHost`]), one this process holds
//!   ([`host::HostDevice`]), and OpenTAP's stamp in a device's
//!   `ublksrv_flags`. [`spec::DeviceSpec`] is what a device is created
//!   with.
//! - [`recovery`] is the `UBLK_F_USER_RECOVERY` re-attach after a daemon's
//!   death ([`recovery::Recovery`]: `START_USER_RECOVERY`, the queue
//!   re-opened and fetched, then `END_USER_RECOVERY` through the device's
//!   [`DeviceControl`]), with the 6.12 contract it is written against, and
//!   the standing of every device on the host ([`recovery::classify`],
//!   [`recovery::probe`], [`recovery::reap_check`]).
//! - [`mock_kernel`] is the driver in memory: devices that outlive their
//!   server, the death of a server, `START_USER_RECOVERY`'s refusals, the
//!   reissue of the requests a dead server held, so every step of the
//!   recovery and of a daemon's restart runs on any host.
//!
//! One thing the front door does not do, on purpose: it drops the
//! `UBLK_IO_F_FUA` flag (a write is acknowledged when it is placed in the
//! arena, D2; FUA semantics on the medium are a later policy). A device is
//! created with `UBLK_F_USER_RECOVERY_REISSUE`: a request the dead server
//! held is delivered again after the re-attach, which is safe because
//! every request the front door serves is idempotent (see [`recovery`]).
//!
//! The crate contains no `unsafe`; the rings and the descriptor mapping
//! live in `otap-sys`.

#![forbid(unsafe_code)]
#![deny(missing_docs)]
#![warn(rust_2018_idioms, unreachable_pub)]

#[cfg(target_os = "linux")]
pub mod device;
pub mod frontdoor;
pub mod host;
pub mod mock;
pub mod mock_kernel;
pub mod recovery;
pub mod spec;
pub mod start;

pub use frontdoor::{
    decode_request, errno_for, request_id, split_id, FrontDoorStats, QueueEvent, Transport,
    TransportError, UblkFrontDoor, Watermarks, EINVAL, EIO, ENODEV, ENOSPC, EOPNOTSUPP,
};
pub use host::{
    device_stamp, pool_fingerprint, read_stamp, BoxedTransport, DevInfo, DevState, HostDevice,
    Stamp, UblkHost, EBUSY, STAMP_MAGIC,
};
pub use mock::{MockStart, MockTransport};
pub use mock_kernel::{ForeignDevice, MockCmd, MockCompletion, MockDevice, MockHost, MockKernel};
pub use recovery::{
    classify, probe, reap_check, Expect, Owner, Probed, Recovery, RecoveryError, Standing,
    DEFAULT_RECOVERY_BUDGET_NS, RECOVERY_RETRY_NS, RECOVERY_TARGET_NS,
};
pub use spec::DeviceSpec;
pub use start::{start_while_serving, DeviceControl, StartError};
