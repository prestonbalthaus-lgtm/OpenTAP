//! The ublk devices of online volumes (Linux).

use otap_control::{ControlError, ErrorCode};
use otap_ublk::device::{front_door, DeviceSpec, UblkDevice};

use crate::frontdoor::VolumeDevice;

/// Creates and starts the device of volume `vol` of `size_blocks` blocks
/// over a `stripe_bytes` geometry, on the calling thread (the one that
/// serves it: the driver pins the queue to the task that issued the first
/// fetch).
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
            let _ = device.delete();
            return Err(ControlError::new(
                ErrorCode::Io,
                format!("cannot open the queue of volume {vol}: {e}"),
            ));
        }
    };
    if let Err(e) = device.start() {
        let _ = device.delete();
        return Err(ControlError::new(
            ErrorCode::Io,
            format!("cannot start the ublk device of volume {vol}: {e}"),
        ));
    }
    let path = device.block_path();
    Ok((VolumeDevice { device, front }, path))
}

/// Stops and deletes a volume's device. Requests the engine still holds
/// finish normally; new ones were answered `ENODEV` since the front door
/// went offline.
pub fn destroy_device(dev: VolumeDevice) -> Result<(), ControlError> {
    let VolumeDevice { device, front } = dev;
    drop(front);
    device.delete().map_err(|e| {
        ControlError::new(ErrorCode::Io, format!("cannot delete the ublk device: {e}"))
    })
}
