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
//! - `device` (Linux) creates and starts the device (`ADD_DEV`,
//!   `SET_PARAMS` with a 4 KiB block, a volatile cache and FUA, discard
//!   limits; `START_DEV` once every tag has a fetch pending) and runs the
//!   engine's tick loop against it.
//! - [`start`] is the start handshake (Step 6A): `START_DEV` is issued
//!   without waiting and polled while the queue is served, because the
//!   driver's partition scan reads the new disk through that queue before
//!   the command returns; [`mock::MockStart`] models it on any host.
//!
//! Two things the front door does not do, on purpose: it drops the
//! `UBLK_IO_F_FUA` flag (a write is acknowledged when it is placed in the
//! arena, D2; FUA semantics on the medium are a Step 5 policy), and it has
//! no recovery path for `UBLK_F_USER_RECOVERY` yet (the device is created
//! with the flag; a tag whose fetch fails is retired; a new daemon
//! re-attaching the queue is `dbladed`'s, Step 5).
//!
//! The crate contains no `unsafe`; the rings and the descriptor mapping
//! live in `otap-sys`.

#![forbid(unsafe_code)]
#![deny(missing_docs)]
#![warn(rust_2018_idioms, unreachable_pub)]

#[cfg(target_os = "linux")]
pub mod device;
pub mod frontdoor;
pub mod mock;
pub mod start;

pub use frontdoor::{
    decode_request, errno_for, request_id, split_id, FrontDoorStats, QueueEvent, Transport,
    TransportError, UblkFrontDoor, Watermarks, EINVAL, EIO, ENODEV, ENOSPC, EOPNOTSUPP,
};
pub use mock::{MockStart, MockTransport};
pub use start::{start_while_serving, DeviceControl, StartError};
