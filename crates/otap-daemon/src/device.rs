//! The ublk devices of online volumes (Linux).
//!
//! A device is made in two halves (Step 6A). [`create_device`] does what
//! needs no server: `ADD_DEV`, `SET_PARAMS`, the queue and a fetch on every
//! tag, on the calling thread, which is the engine thread that will serve
//! it (the driver pins the queue to the task that issued the first fetch).
//! The pool then hands the device's front door to the engine and starts the
//! device while the engine serves it ([`otap_ublk::start_while_serving`]):
//! `START_DEV` completes only after the driver's partition scan has read
//! the new disk through that queue. Every teardown, a failed start
//! included, goes through [`destroy_device`], which closes the queue before
//! `DEL_DEV` (that command waits until `/dev/ublkcN` is released).

use otap_control::{ControlError, ErrorCode};
use otap_ublk::device::{front_door, DeviceSpec, UblkDevice};

use crate::frontdoor::VolumeDevice;

/// Creates the device of volume `vol` of `size_blocks` blocks over a
/// `stripe_bytes` geometry, on the calling thread (the one that serves it),
/// with its queue's fetches submitted and its front door open, but not
/// started: the caller registers it with the engine and starts it while the
/// engine serves. Returns the device and its block path.
pub fn create_device(
    vol: u32,
    size_blocks: u64,
    stripe_bytes: u32,
    rotational: bool,
    arena_slots: u32,
) -> Result<(VolumeDevice, String), ControlError> {
    let spec = DeviceSpec::new(size_blocks, stripe_bytes, rotational).for_arena(arena_slots);
    let mut device = UblkDevice::create(&spec).map_err(|e| {
        ControlError::new(
            ErrorCode::Io,
            format!("cannot create the ublk device of volume {vol}: {e}"),
        )
    })?;
    let front = match front_door(&mut device, vol, &spec) {
        Ok(f) => f,
        Err(e) => {
            // `front_door` fails only when the queue was already taken, so
            // no `/dev/ublkcN` is held open here.
            let _ = device.delete();
            return Err(ControlError::new(
                ErrorCode::Io,
                format!("cannot open the queue of volume {vol}: {e}"),
            ));
        }
    };
    let path = device.block_path();
    Ok((VolumeDevice { device, front }, path))
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
