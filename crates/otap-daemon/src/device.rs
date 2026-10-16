//! The ublk devices of online volumes, over a control plane
//! ([`otap_ublk::UblkHost`]: the kernel's on Linux, the tests' in-memory
//! driver anywhere).
//!
//! A device is made in two halves (Step 6A). The control plane's `create`
//! does what needs no server: `ADD_DEV`, `SET_PARAMS`, the queue and a
//! fetch on every tag, on the calling thread, which is the engine thread
//! that will serve it (the driver pins the queue to the task that issued
//! the fetches). A device whose server died is re-attached the same way
//! (Step 6B): `START_USER_RECOVERY`, the queue re-opened and fetched. The
//! pool then hands the device's front door ([`volume_device`]) to the
//! engine and brings the device live while the engine serves it
//! ([`otap_ublk::start_while_serving`]): `START_DEV` completes only after
//! the driver's partition scan has read the new disk through that queue,
//! `END_USER_RECOVERY` once the queue's fetches are in. Every teardown, a
//! failed start included, goes through [`destroy_device`], which closes
//! the queue before `DEL_DEV` (that command waits until `/dev/ublkcN` is
//! released).

use std::cell::RefCell;
use std::rc::Rc;

use otap_control::{ControlError, ErrorCode};
use otap_ublk::{BoxedTransport, HostDevice, UblkFrontDoor, UblkHost, Watermarks};

/// The control plane a daemon and its pool share, on the engine thread.
pub type HostRef = Rc<RefCell<Box<dyn UblkHost>>>;

/// One volume's ublk device: the device and its queue as the engine's
/// front door.
pub struct VolumeDevice {
    /// The device.
    pub device: Box<dyn HostDevice>,
    /// Its queue as the engine's front door.
    pub front: UblkFrontDoor<BoxedTransport>,
}

impl std::fmt::Debug for VolumeDevice {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("VolumeDevice")
            .field("dev_id", &self.device.dev_id())
            .field("front", &self.front)
            .finish()
    }
}

/// The front door of `device` for volume `vol` of `size_blocks` blocks: its
/// queue, `depth` tags deep, under the depth's watermarks. A device whose
/// queue was already taken is deleted and refused.
pub fn volume_device(
    mut device: Box<dyn HostDevice>,
    vol: u32,
    size_blocks: u64,
) -> Result<VolumeDevice, ControlError> {
    let depth = device.depth();
    let Some(transport) = device.take_transport() else {
        // No queue held: nothing keeps `/dev/ublkcN` open, so the delete
        // does not wait.
        let id = device.dev_id();
        let _ = device.delete();
        return Err(ControlError::new(
            ErrorCode::Io,
            format!("the queue of ublk device {id} (volume {vol}) was already taken"),
        ));
    };
    let front = UblkFrontDoor::new(
        transport,
        0,
        vol,
        size_blocks,
        depth,
        Watermarks::for_depth(depth),
    );
    Ok(VolumeDevice { device, front })
}

/// Stops and deletes a volume's device. The front door is dropped first,
/// which closes `/dev/ublkcN`: `DEL_DEV` waits for that. Requests the
/// engine still holds finish normally; new ones were answered `ENODEV`
/// since the front door went offline.
pub fn destroy_device(dev: VolumeDevice) -> Result<(), ControlError> {
    let VolumeDevice { device, front } = dev;
    drop(front);
    device.delete().map_err(|e| {
        ControlError::new(ErrorCode::Io, format!("cannot delete the ublk device: {e}"))
    })
}

/// The reply of a device request when the daemon drives no ublk control
/// plane.
#[must_use]
pub fn no_ublk() -> ControlError {
    ControlError::new(
        ErrorCode::Unsupported,
        "this daemon exposes no ublk devices (started with --no-ublk, or the host has no ublk \
         driver)",
    )
}
