//! The device start handshake (Step 6A).
//!
//! `UBLK_CMD_START_DEV` does not return before the device is live, and in
//! 6.12 two waits stand between the command and that point
//! (`ublk_ctrl_start_dev`, `drivers/block/ublk_drv.c`): it waits until
//! every tag of every queue has a processed `FETCH_REQ`
//! (`ub->completion`, completed by `ublk_mark_io_ready` once
//! `nr_queues_ready == nr_hw_queues`), and then it runs `add_disk`, which
//! for a privileged server (the fetching task has `CAP_SYS_ADMIN`, so
//! `GD_SUPPRESS_PART_SCAN` is not set) scans the partition table: it reads
//! the new disk through the very queue it is starting and waits for the
//! read. That read reaches the server as task work on the task that issued
//! the fetches, the one thread that serves the queue. Step 5 issued
//! `START_DEV` on that thread and waited for its completion before going
//! back to serving, so the scan's read was never answered and the start
//! never returned: every `dbladed` running as root wedged at its first
//! device.
//!
//! The order this module fixes, and which [`start_while_serving`] is:
//!
//! 1. the queue's fetches are submitted, on the thread that will serve it
//!    (`device::UblkDevice::create`);
//! 2. the queue is handed to the loop that serves it (the engine's front
//!    door), so a request delivered from now on is answered;
//! 3. `START_DEV` is issued without waiting ([`DeviceControl::begin_start`]:
//!    the driver runs control commands in an io-wq worker, never inline);
//! 4. the serving loop turns, and polls the start between turns
//!    ([`DeviceControl::poll_start`]), until the device is live, the start
//!    fails, or the caller gives up.
//!
//! [`crate::mock::MockStart`] models the driver's two waits over a
//! [`crate::mock::MockTransport`], so the order is tested on any host.

use core::fmt;

use crate::frontdoor::TransportError;

/// The control half of a device's start, as the serving loop drives it.
pub trait DeviceControl {
    /// Issues `START_DEV` and returns at once, before the device is live.
    ///
    /// # Errors
    /// The command could not be issued.
    fn begin_start(&mut self) -> Result<(), TransportError>;

    /// Whether the start issued by
    /// [`begin_start`](DeviceControl::begin_start) has completed: `true`
    /// once the device is live, `false` while it is in flight.
    ///
    /// # Errors
    /// The start failed (the command's errno), or none is in flight.
    fn poll_start(&mut self) -> Result<bool, TransportError>;
}

/// Why a start did not bring the device live.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StartError<E> {
    /// `START_DEV` could not be issued, or failed.
    Start(TransportError),
    /// The serving loop failed while the start was in flight.
    Serve(E),
    /// The caller gave up with the start still in flight.
    TimedOut,
}

impl<E: fmt::Display> fmt::Display for StartError<E> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Start(e) => write!(f, "START_DEV failed: {e}"),
            Self::Serve(e) => write!(f, "serving the queue while it started failed: {e}"),
            Self::TimedOut => f.write_str("START_DEV did not complete in time"),
        }
    }
}

impl<E: fmt::Debug + fmt::Display> std::error::Error for StartError<E> {}

/// Starts a device whose queue `ctx` serves: issues `START_DEV` with
/// `begin` (it must not wait), then alternates `poll` and `serve` (one
/// turn of the loop that serves the queue, on the thread that issued its
/// fetches) until the device is live. `expired` is asked before each turn;
/// once it answers `true` the start is abandoned with
/// [`StartError::TimedOut`] (the caller tears the device down, dropping the
/// queue before `DEL_DEV`, which waits for the character device to close).
///
/// # Errors
/// [`StartError`].
pub fn start_while_serving<X, E>(
    ctx: &mut X,
    mut begin: impl FnMut(&mut X) -> Result<(), TransportError>,
    mut poll: impl FnMut(&mut X) -> Result<bool, TransportError>,
    mut serve: impl FnMut(&mut X) -> Result<(), E>,
    mut expired: impl FnMut() -> bool,
) -> Result<(), StartError<E>> {
    begin(ctx).map_err(StartError::Start)?;
    loop {
        if poll(ctx).map_err(StartError::Start)? {
            return Ok(());
        }
        if expired() {
            return Err(StartError::TimedOut);
        }
        serve(ctx).map_err(StartError::Serve)?;
    }
}
