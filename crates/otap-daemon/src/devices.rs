//! `otapctl device list` and `otapctl device reap` (Step 6B): the ublk
//! devices on the host as reports, and the deletion of the ones no daemon
//! will re-attach. The standing of a device comes from
//! [`otap_ublk::classify`]; what this daemon knows (the devices it serves,
//! the ones it recorded stale, the volumes that would take a device) comes
//! from the booted pool, or is empty with no pool booted.

use otap_control::{ControlError, DeviceReport, DeviceState, ErrorCode, Reply, Response};
use otap_format::BLOCK_SIZE;
use otap_ublk::{probe, reap_check, Owner, Probed, Standing, UblkHost};

/// The protocol's word for a standing.
#[must_use]
pub const fn state_of(standing: Standing) -> DeviceState {
    match standing {
        Standing::Serving => DeviceState::Serving,
        Standing::Recoverable => DeviceState::Recoverable,
        Standing::Stale => DeviceState::Stale,
        Standing::Foreign => DeviceState::Foreign,
    }
}

/// The report of `device`. `pool` is the booted pool's fingerprint;
/// `name_of` names a volume of the booted pool, which is asked only for a
/// device served here or stamped for that pool.
#[must_use]
pub fn report(
    device: &Probed,
    pool: Option<u32>,
    name_of: &dyn Fn(u32) -> Option<String>,
) -> DeviceReport {
    let info = &device.info;
    let stamp = info.opentap();
    let of_this_pool =
        device.standing == Standing::Serving || (pool.is_some() && stamp.map(|s| s.pool) == pool);
    DeviceReport {
        dev_id: info.dev_id,
        path: info.block_path(),
        state: state_of(device.standing),
        reason: device.reason.clone(),
        kernel_state: info.state.word(),
        pid: info.pid,
        pid_alive: device.server_alive,
        volume_index: device.vol,
        volume: if of_this_pool {
            device.vol.and_then(name_of)
        } else {
            None
        },
        pool: stamp.map(|s| format!("{:08x}", s.pool)),
        size_bytes: info.size_blocks.map(|b| b * BLOCK_SIZE as u64),
        reissue: info.reissues(),
    }
}

fn unreachable_host(e: &otap_ublk::TransportError) -> ControlError {
    ControlError::new(
        ErrorCode::Io,
        format!("cannot list the host's ublk devices: {e}"),
    )
}

/// Every ublk device on the host, by id.
///
/// # Errors
/// The control plane cannot be reached.
pub fn list(
    host: &mut dyn UblkHost,
    owner: &Owner<'_>,
    name_of: &dyn Fn(u32) -> Option<String>,
) -> Result<Vec<DeviceReport>, ControlError> {
    let probed = probe(host, owner).map_err(|e| unreachable_host(&e))?;
    Ok(probed
        .iter()
        .map(|p| report(p, owner.pool, name_of))
        .collect())
}

/// What a reap did: the devices deleted (as they stood before), and the
/// failure that stopped it, if one did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Reaped {
    /// The deleted devices.
    pub devices: Vec<DeviceReport>,
    /// Why the reap stopped before the end, if it did.
    pub failure: Option<ControlError>,
}

impl Reaped {
    /// The reply: the devices deleted, or the failure that stopped the
    /// reap, naming what was deleted before it.
    ///
    /// # Errors
    /// The failure.
    pub fn into_reply(self) -> Reply {
        match self.failure {
            None => Ok(Response::Reaped {
                devices: self.devices,
            }),
            Some(mut e) => {
                if !self.devices.is_empty() {
                    let ids: Vec<String> =
                        self.devices.iter().map(|d| d.dev_id.to_string()).collect();
                    e.message
                        .push_str(&format!(" (deleted before it: {})", ids.join(", ")));
                }
                Err(e)
            }
        }
    }
}

/// Deletes device `dev_id`, or with `stale` every stale device: the one
/// named must pass [`reap_check`] as named (a stale device, or a
/// recoverable one); `stale` takes every stale device that passes it and
/// skips the rest (a device still quiescing waits for the next reap).
/// Each is deleted with `STOP_DEV` and `DEL_DEV_ASYNC`; the first failure
/// stops the reap and is returned with the devices deleted before it.
///
/// # Errors
/// A request naming neither or both, a device that does not exist or may
/// not be deleted, or a host that cannot be reached.
pub fn reap(
    host: &mut dyn UblkHost,
    owner: &Owner<'_>,
    name_of: &dyn Fn(u32) -> Option<String>,
    dev_id: Option<u32>,
    stale: bool,
) -> Result<Reaped, ControlError> {
    if dev_id.is_some() == stale {
        return Err(ControlError::new(
            ErrorCode::BadRequest,
            "`device reap` takes a device id or --stale, not both and not neither",
        ));
    }
    let probed = probe(host, owner).map_err(|e| unreachable_host(&e))?;
    let targets: Vec<&Probed> = match dev_id {
        Some(id) => {
            let device = probed.iter().find(|p| p.info.dev_id == id).ok_or_else(|| {
                ControlError::new(
                    ErrorCode::NotFound,
                    format!("no ublk device {id} on this host"),
                )
            })?;
            reap_check(device, true).map_err(|m| ControlError::new(ErrorCode::Engine, m))?;
            vec![device]
        }
        None => probed
            .iter()
            .filter(|p| p.standing == Standing::Stale && reap_check(p, false).is_ok())
            .collect(),
    };
    let mut devices = Vec::with_capacity(targets.len());
    for device in targets {
        let id = device.info.dev_id;
        if let Err(e) = host.reap(id) {
            return Ok(Reaped {
                devices,
                failure: Some(ControlError::new(
                    ErrorCode::Io,
                    format!("cannot delete ublk device {id}: {e}"),
                )),
            });
        }
        devices.push(report(device, owner.pool, name_of));
    }
    Ok(Reaped {
        devices,
        failure: None,
    })
}
