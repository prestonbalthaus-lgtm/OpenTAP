//! The pool's ublk devices across a daemon's death (Step 6B): the re-attach
//! of the devices a killed daemon left, at boot and before any volume is
//! served, the record of them in the state directory, and the device list
//! and the reaper with what the pool knows.
//!
//! At boot ([`Pool::reconcile_devices`]), after the boot commit:
//!
//! 1. The host's devices are listed. A recorded device that no longer
//!    exists is forgotten; a device bound to a volume that is no longer
//!    online is recorded stale.
//! 2. Every online volume with a device (the one the record binds to it,
//!    or else one stamped with this pool and the volume, which covers a
//!    crash before the record was written) gets a [`Recovery`], and every
//!    recovery is stepped in turn, the engine ticking between rounds so
//!    the devices already re-attached are served: `START_USER_RECOVERY`
//!    (retried while the driver answers `EBUSY`, within
//!    `--recovery-timeout`), the queue re-opened with every tag fetched,
//!    the front door handed to the engine, then `END_USER_RECOVERY` while
//!    the engine serves it ([`Pool::start_device`]). The volume keeps its
//!    `/dev/ublkbN`, and with `UBLK_F_USER_RECOVERY_REISSUE` the requests
//!    the dead daemon held are delivered again.
//! 3. A device that is gone gives its volume a new one; a device that
//!    cannot be re-attached (or whose quiesce did not come in time) is
//!    recorded stale and its volume gets a new one. Nothing is deleted
//!    here: stale devices are listed (`otapctl device list`) and deleted
//!    on request (`otapctl device reap`).

use std::time::Instant;

use otap_control::{ControlError, ErrorCode, Reply, Response};
use otap_ublk::{
    device_stamp, DevState, Expect, HostDevice, Owner, Recovery, RecoveryError, RECOVERY_TARGET_NS,
};

use super::{Pool, Served, IDLE_TICK};
use crate::bindings::{self, Binding, StaleReason};
use crate::device::{no_ublk, volume_device, HostRef};

/// Refuses a boot of the pool with fingerprint `pool_fp` while a live
/// process other than this one serves one of its devices: another daemon
/// runs the pool, and this one must not touch it. Checked before the pool
/// is opened. (A dead daemon's device still `LIVE` names a dead pid, which
/// does not count; a pid the system has since reused for another process
/// does, until the driver quiesces the device, within its request
/// timeout.)
pub(super) fn refuse_a_second_server(host: &HostRef, pool_fp: u32) -> Result<(), ControlError> {
    let listed = host.borrow_mut().list();
    let found = listed.map_err(|e| {
        ControlError::new(
            ErrorCode::Io,
            format!("cannot list the host's ublk devices: {e}"),
        )
    })?;
    let me = host.borrow().pid();
    for d in &found {
        let Some(stamp) = d.opentap().filter(|s| s.pool == pool_fp) else {
            continue;
        };
        let served = d.state == DevState::Live
            && d.pid > 0
            && d.pid as u32 != me
            && host.borrow_mut().pid_alive(d.pid);
        if served {
            return Err(ControlError::new(
                ErrorCode::Engine,
                format!(
                    "ublk device {} of this pool's volume {} is served by process {}: another \
                     dbladed runs this pool, so it is not booted here (`otapctl device list` \
                     shows the devices)",
                    d.dev_id, stamp.vol, d.pid
                ),
            ));
        }
    }
    Ok(())
}

impl Pool {
    /// Reads the record of the pool's devices from the state directory. A
    /// record that does not verify is set aside with a line in the log:
    /// the devices' stamps rebuild it.
    pub(super) fn load_bindings(&mut self) {
        let agg_id = *self.bindings.agg_id();
        match bindings::load(&self.state_dir, &agg_id) {
            Ok(Some(found)) => self.bindings = found,
            Ok(None) => {}
            Err(e) => self.log(format!(
                "the ublk device record {} is not usable ({e}); the devices' stamps stand in \
                 for it",
                bindings::file_path(&self.state_dir, &agg_id).display()
            )),
        }
    }

    /// Writes the record of the pool's devices, with or without ublk (a
    /// `--no-ublk` daemon keeps the record of the devices a killed daemon
    /// left, D11). A failure is logged, never fatal: the record is a hint
    /// the stamps back up.
    pub(super) fn save_bindings(&mut self) {
        if let Err(e) = bindings::save(&self.state_dir, &self.bindings, std::process::id()) {
            let path = bindings::file_path(&self.state_dir, self.bindings.agg_id());
            self.log(format!(
                "cannot record the ublk devices in {}: {e}",
                path.display()
            ));
        }
    }

    /// Re-attaches the devices a killed daemon left to the online volumes
    /// they exposed (the module's steps 1 to 3). `reattach_began` is when
    /// the engine's boot (and with it the arena's re-attach) began: each
    /// device's time to serving is logged from there, against the 2 s
    /// target.
    pub(super) fn reconcile_devices(
        &mut self,
        reattach_began: Instant,
    ) -> Result<(), ControlError> {
        let Some(host) = self.host.clone() else {
            return Ok(());
        };
        let listed = host.borrow_mut().list();
        let found = listed.map_err(|e| {
            ControlError::new(
                ErrorCode::Io,
                format!("cannot list the host's ublk devices: {e}"),
            )
        })?;
        // Step 1: the record against the host. A recorded number that is gone,
        // or that the driver has since given to a device without this
        // volume's stamp, is forgotten: that device is not ours.
        for entry in self.bindings.entries().to_vec() {
            let id = entry.dev_id();
            let stamp = device_stamp(self.pool_fp, entry.vol());
            match found.iter().find(|d| d.dev_id == id) {
                Some(d) if d.stamp == stamp => {}
                Some(_) => {
                    self.bindings.forget(id);
                    self.log(format!(
                        "ublk device {id} no longer carries the stamp of volume {}; it is \
                         left alone",
                        entry.vol()
                    ));
                }
                None => {
                    self.bindings.forget(id);
                    if let Binding::Bound { vol, .. } = entry {
                        self.log(format!(
                            "volume {vol}: its ublk device {id} no longer exists; it gets a new \
                             one"
                        ));
                    }
                }
            }
        }
        let online: Vec<(u32, u64)> = self
            .engine
            .volumes()
            .into_iter()
            .filter(|(_, v)| v.present && !v.offline)
            .map(|(vol, v)| (vol, v.size_blocks))
            .collect();
        for entry in self.bindings.entries().to_vec() {
            if let Binding::Bound { dev_id, vol, .. } = entry {
                if online.iter().all(|(v, _)| *v != vol) {
                    let present = self.engine.volume(vol).is_some_and(|v| v.present);
                    let reason = if present {
                        StaleReason::VolumeOffline
                    } else {
                        StaleReason::VolumeGone
                    };
                    self.bindings.mark_stale(dev_id, vol, reason);
                    self.log(format!(
                        "ublk device {dev_id} of volume {vol} is stale: {}",
                        reason.describe(vol)
                    ));
                }
            }
        }
        // Step 2: a recovery per online volume with a device.
        let budget_ns = u64::try_from(self.recovery_timeout.as_nanos()).unwrap_or(u64::MAX);
        let mut pending: Vec<(u32, Recovery)> = Vec::new();
        for &(vol, size_blocks) in &online {
            let stamp = device_stamp(self.pool_fp, vol);
            let bound = self.bindings.bound(vol).map(|(dev_id, _)| dev_id);
            let claims: Vec<u32> = found
                .iter()
                .filter(|d| d.stamp == stamp && !self.bindings.is_stale(d.dev_id))
                .map(|d| d.dev_id)
                .collect();
            if bound.is_none() && claims.len() > 1 {
                // Two devices carry the volume's stamp and no record says
                // which one it was serving (an older one may hold writes the
                // newer one's overtook): neither is re-attached, both are
                // left stale for the operator, and the volume gets a new
                // device (the Step 6 review's D11).
                for &dev_id in &claims {
                    self.bindings
                        .mark_stale(dev_id, vol, StaleReason::RecoveryFailed);
                }
                self.log(format!(
                    "volume {vol}: ublk devices {claims:?} all carry its stamp and none is \
                     recorded; none is re-attached, all are stale, and the volume gets a new \
                     device"
                ));
                continue;
            }
            let stamped = claims.first().copied();
            let Some(dev_id) = bound.or(stamped) else {
                continue;
            };
            if bound.is_none() {
                self.log(format!(
                    "volume {vol}: ublk device {dev_id} carries its stamp and was not recorded; \
                     it is re-attached"
                ));
            }
            let expect = Expect {
                stamp,
                size_blocks,
                max_io_bytes: self.device_spec(size_blocks).max_io_bytes,
            };
            pending.push((vol, Recovery::new(dev_id, expect, budget_ns)));
        }
        let began = Instant::now();
        while !pending.is_empty() {
            let now_ns = u64::try_from(began.elapsed().as_nanos()).unwrap_or(u64::MAX);
            let mut i = 0;
            while i < pending.len() {
                let polled = pending[i].1.poll(&mut **host.borrow_mut(), now_ns);
                match polled {
                    Ok(None) => i += 1,
                    Ok(Some(device)) => {
                        let (vol, recovery) = pending.swap_remove(i);
                        self.finish_recovery(vol, device, &recovery, reattach_began);
                    }
                    Err(e) => {
                        let (vol, recovery) = pending.swap_remove(i);
                        self.recovery_failed(vol, recovery.dev_id(), &e);
                    }
                }
            }
            if pending.is_empty() {
                break;
            }
            if self.interrupted() {
                return Err(Self::interrupted_error("the re-attach of the ublk devices"));
            }
            // A turn of the engine: the devices already re-attached are
            // served, and time passes for the driver's quiesce.
            if !self.tick()? {
                std::thread::sleep(IDLE_TICK);
            }
        }
        self.save_bindings();
        let stale = self.bindings.stale().len();
        if stale > 0 {
            self.log(format!(
                "{stale} stale ublk device(s) recorded; `otapctl device list` shows them and \
                 `otapctl device reap --stale` deletes them"
            ));
        }
        Ok(())
    }

    /// Hands a re-attached device's queue to the engine and ends its
    /// recovery while the engine serves it; on failure the device is torn
    /// down (or recorded stale) and the volume gets a new one. `began` is
    /// when the arena's re-attach began.
    fn finish_recovery(
        &mut self,
        vol: u32,
        device: Box<dyn HostDevice>,
        recovery: &Recovery,
        began: Instant,
    ) {
        let dev_id = device.dev_id();
        let size_blocks = self.engine.volume(vol).map_or(0, |v| v.size_blocks);
        let dev = match volume_device(device, vol, size_blocks) {
            Ok(dev) => dev,
            Err(e) => {
                self.bindings
                    .mark_stale(dev_id, vol, StaleReason::RecoveryFailed);
                self.log(format!("volume {vol}: {e}"));
                return;
            }
        };
        self.engine.front_door().add_device(vol, dev);
        match self.start_device(vol) {
            Ok(()) => {
                let path = format!("/dev/ublkb{dev_id}");
                self.bindings.bind(vol, dev_id, size_blocks);
                self.devices.insert(
                    vol,
                    Served {
                        dev_id,
                        path: path.clone(),
                    },
                );
                let elapsed = began.elapsed();
                let mut line = format!(
                    "volume {vol} re-attached {path}, serving {} ms after the arena re-attach \
                     began ({} attempt(s))",
                    elapsed.as_millis(),
                    recovery.attempts()
                );
                if elapsed.as_nanos() > u128::from(RECOVERY_TARGET_NS) {
                    line.push_str(", beyond the 2 s target");
                }
                self.log(line);
            }
            Err(e) => {
                self.log(format!(
                    "volume {vol}: END_USER_RECOVERY of ublk device {dev_id} failed ({}); it \
                     gets a new device",
                    e.message
                ));
                self.abandon_device(vol, StaleReason::RecoveryFailed);
            }
        }
    }

    /// Records why a device was not re-attached; its volume gets a new one.
    fn recovery_failed(&mut self, vol: u32, dev_id: u32, e: &RecoveryError) {
        match e {
            RecoveryError::Gone => {
                self.bindings.forget(dev_id);
                self.log(format!(
                    "volume {vol}: its ublk device {dev_id} is gone; it gets a new one"
                ));
            }
            other => {
                let reason = if matches!(other, RecoveryError::TimedOut { .. }) {
                    StaleReason::RecoveryTimedOut
                } else {
                    StaleReason::RecoveryFailed
                };
                self.bindings.mark_stale(dev_id, vol, reason);
                self.log(format!(
                    "volume {vol}: ublk device {dev_id} is not re-attached ({other}); it is \
                     listed stale and the volume gets a new device"
                ));
            }
        }
    }

    /// The devices this pool serves, as `(device, volume)`.
    fn serving(&self) -> Vec<(u32, u32)> {
        self.devices
            .iter()
            .map(|(vol, s)| (s.dev_id, *vol))
            .collect()
    }

    /// Whether volume `vol` would take a device: online and without one.
    fn wants_device(&self, vol: u16) -> bool {
        let vol = u32::from(vol);
        !self.devices.contains_key(&vol)
            && self
                .engine
                .volume(vol)
                .is_some_and(|v| v.present && !v.offline)
    }

    fn volume_name_of(&self, vol: u32) -> Option<String> {
        self.engine.volume_name(vol).map(str::to_string)
    }

    /// `otapctl device list` with this pool booted.
    pub(super) fn device_list(&mut self) -> Reply {
        let host = self.host.clone().ok_or_else(no_ublk)?;
        let serving = self.serving();
        let stale = self.bindings.stale();
        let wants = |vol: u16| self.wants_device(vol);
        let name_of = |vol: u32| self.volume_name_of(vol);
        let owner = Owner {
            pool: Some(self.pool_fp),
            serving: &serving,
            stale: &stale,
            wants: &wants,
        };
        let devices = crate::devices::list(&mut **host.borrow_mut(), &owner, &name_of)?;
        Ok(Response::Devices { devices })
    }

    /// `otapctl device reap` with this pool booted: the deleted devices
    /// are forgotten from the record.
    pub(super) fn device_reap(&mut self, dev_id: Option<u32>, stale: bool) -> Reply {
        let host = self.host.clone().ok_or_else(no_ublk)?;
        let serving = self.serving();
        let stale_list = self.bindings.stale();
        let reaped = {
            let wants = |vol: u16| self.wants_device(vol);
            let name_of = |vol: u32| self.volume_name_of(vol);
            let owner = Owner {
                pool: Some(self.pool_fp),
                serving: &serving,
                stale: &stale_list,
                wants: &wants,
            };
            crate::devices::reap(&mut **host.borrow_mut(), &owner, &name_of, dev_id, stale)?
        };
        for device in &reaped.devices {
            self.bindings.forget(device.dev_id);
            self.log(format!("ublk device {} reaped", device.dev_id));
        }
        self.save_bindings();
        reaped.into_reply()
    }
}
