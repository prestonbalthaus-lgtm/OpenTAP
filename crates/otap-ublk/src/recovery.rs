//! The re-attach of a device whose server died, and how a daemon tells its
//! devices apart (Step 6B).
//!
//! # The 6.12 contract (`drivers/block/ublk_drv.c`)
//!
//! - When the server of a `UBLK_F_USER_RECOVERY` device dies, io_uring
//!   cancels the fetches it had pending (`ublk_uring_cmd_cancel_fn`), which
//!   aborts the queue (`ublk_abort_requests`, `ublk_abort_queue`) and
//!   schedules the quiesce (`ublk_quiesce_work_fn`, `__ublk_quiesce_dev`):
//!   the block queue is quiesced and the state becomes
//!   `UBLK_S_DEV_QUIESCED`. A request the dead server had been handed and
//!   had not committed is failed with `EIO` (`__ublk_fail_req`) unless the
//!   device also has `UBLK_F_USER_RECOVERY_REISSUE`, in which case it is
//!   requeued; a request dispatched and not yet handed over is requeued
//!   either way (`__ublk_abort_rq`); the rest wait in the quiesced queue.
//! - When the dead server held every tag, no fetch was pending and nothing
//!   is cancelled: only the block layer's request timeout (`ublk_timeout`,
//!   30 s by default, which the driver does not change) aborts and
//!   quiesces, and until then the device reads `UBLK_S_DEV_LIVE` with the
//!   dead pid.
//! - `UBLK_CMD_START_USER_RECOVERY` (`ublk_ctrl_start_recovery`) answers
//!   `EINVAL` without `UBLK_F_USER_RECOVERY` or when no queue is ready
//!   (`nr_queues_ready == 0`: a recovery begun whose fetches never
//!   completed, or a device never started), and `EBUSY` while the dead
//!   process still holds `/dev/ublkcN` (`UB_STATE_OPEN`, cleared by
//!   `ublk_ch_release`) or the state is not `QUIESCED`. On success every
//!   queue is reset (`ublk_queue_reinit`): its slots forgotten, its
//!   daemon task cleared, the descriptor mapping's `mm` unset.
//! - The new server then opens `/dev/ublkcN` (`ublk_ch_open`, exclusive),
//!   maps the descriptors and submits `UBLK_IO_FETCH_REQ` on every tag of
//!   every queue (`ublk_mark_io_ready` completes the device's completion at
//!   the last one).
//! - `UBLK_CMD_END_USER_RECOVERY` (`ublk_ctrl_end_recovery`) waits for that
//!   completion, answers `EBUSY` unless the state is still `QUIESCED`, then
//!   records the new `ublksrv_pid`, unquiesces the block queue and kicks the
//!   requeue list: the held and requeued requests reach the new server as
//!   ordinary fetch completions. The state is `UBLK_S_DEV_LIVE` again and
//!   `/dev/ublkbN` keeps its number.
//! - `STOP_DEV` and `DEL_DEV` (`ublk_stop_dev`, `ublk_ctrl_del_dev`) may be
//!   sent by any privileged process and have no owner check. On a
//!   `QUIESCED` device they unquiesce with `force_abort`, so its held I/O
//!   fails; on a `LIVE` one they first quiesce, which waits for every
//!   started request, so a device still waiting for its timeout makes them
//!   wait too. `DEL_DEV` then waits until the device's number is freed,
//!   which an open `/dev/ublkcN` anywhere prevents; `DEL_DEV_ASYNC` does
//!   not wait.
//! - The driver lists no devices (they are the `ublk-char` class entries
//!   in sysfs) and knows no pools or volumes: `ublksrv_flags` is the only
//!   field it keeps for the server across the crash ([`crate::host`]).
//!
//! # The recovery
//!
//! [`Recovery`] is one device's re-attach, stepped by its caller between
//! turns of the loop that serves the daemon's other queues:
//! `GET_DEV_INFO` and the checks of [`Expect`], `START_USER_RECOVERY`
//! (retried while it answers `EBUSY`, until the budget runs out), then the
//! queue re-opened with a fetch on every tag. The caller hands that queue
//! to its serving loop and issues `END_USER_RECOVERY` through the returned
//! device's [`crate::DeviceControl`] with [`crate::start_while_serving`],
//! exactly as a new device's `START_DEV`.
//!
//! # Who owns a device
//!
//! [`classify`] places every device of the host in one of four
//! [`Standing`]s from what `GET_DEV_INFO` says and what the daemon knows
//! ([`Owner`]), and [`reap_check`] says whether it may be deleted.

use core::fmt;

use crate::frontdoor::{TransportError, EINVAL, ENODEV};
use crate::host::{DevInfo, DevState, HostDevice, UblkHost, EBUSY};

/// The recovery's target: a device serving again within 2 s of the arena
/// re-attach.
pub const RECOVERY_TARGET_NS: u64 = 2_000_000_000;

/// How long a recovery waits, by default, for the driver to quiesce a dead
/// server's device before the daemon gives up on it and creates a new one.
pub const DEFAULT_RECOVERY_BUDGET_NS: u64 = 5_000_000_000;

/// The least time between two attempts of one recovery while the driver
/// answers `EBUSY` or has not quiesced the device: the serving loop turns
/// far more often (every 200 µs when idle), and every attempt is a
/// `GET_DEV_INFO` and a `START_USER_RECOVERY` through the driver's io-wq.
/// The first attempt, and the one at the end of the budget, are never
/// held back.
pub const RECOVERY_RETRY_NS: u64 = 10_000_000;

/// What the device to re-attach must look like.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Expect {
    /// The stamp it was created with ([`crate::host::device_stamp`]).
    pub stamp: u64,
    /// The volume's size in 4 KiB blocks: a re-attached device keeps its
    /// capacity, so it must be the volume's.
    pub size_blocks: u64,
    /// The largest request this daemon's arena takes
    /// ([`crate::DeviceSpec::for_arena`]): a device that advertises more
    /// would hand the engine writes it refuses.
    pub max_io_bytes: u32,
}

/// Why a recovery ended without a re-attached device.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RecoveryError {
    /// The device no longer exists.
    Gone,
    /// The device exists and cannot be re-attached; the text says why.
    NotRecoverable(String),
    /// The driver had not quiesced the device when the budget ran out.
    TimedOut {
        /// How long the recovery waited, in nanoseconds.
        waited_ns: u64,
        /// The device's state at the last attempt.
        state: DevState,
    },
    /// A control command failed.
    Command(TransportError),
    /// `START_USER_RECOVERY` succeeded and the queue could not be opened or
    /// fetched: the driver now has no ready queue for the device, so it
    /// cannot be recovered again.
    Reattach(TransportError),
}

impl fmt::Display for RecoveryError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Gone => f.write_str("the device no longer exists"),
            Self::NotRecoverable(why) => write!(f, "it cannot be re-attached: {why}"),
            Self::TimedOut { waited_ns, state } => write!(
                f,
                "the driver had not quiesced it after {} ms (state {}); a server that \
                 died holding every tag is quiesced only by the block layer's request \
                 timeout, 30 s",
                waited_ns / 1_000_000,
                state.word()
            ),
            Self::Command(e) => write!(f, "a control command failed: {e}"),
            Self::Reattach(e) => write!(f, "its queue could not be re-opened: {e}"),
        }
    }
}

impl std::error::Error for RecoveryError {}

/// One device's re-attach after its server died.
#[derive(Debug, Clone)]
pub struct Recovery {
    dev_id: u32,
    expect: Expect,
    budget_ns: u64,
    retry_ns: u64,
    began_ns: Option<u64>,
    last_ns: Option<u64>,
    attempts: u32,
    done: bool,
}

impl Recovery {
    /// The re-attach of device `dev_id`, which must match `expect`, waiting
    /// at most `budget_ns` for the driver to quiesce it, with attempts at
    /// least [`RECOVERY_RETRY_NS`] apart.
    #[must_use]
    pub const fn new(dev_id: u32, expect: Expect, budget_ns: u64) -> Self {
        Self {
            dev_id,
            expect,
            budget_ns,
            retry_ns: RECOVERY_RETRY_NS,
            began_ns: None,
            last_ns: None,
            attempts: 0,
            done: false,
        }
    }

    /// The same recovery with attempts at least `retry_ns` apart.
    #[must_use]
    pub const fn with_retry_ns(mut self, retry_ns: u64) -> Self {
        self.retry_ns = retry_ns;
        self
    }

    /// The device.
    #[must_use]
    pub const fn dev_id(&self) -> u32 {
        self.dev_id
    }

    /// Attempts so far.
    #[must_use]
    pub const fn attempts(&self) -> u32 {
        self.attempts
    }

    /// Nanoseconds since the first attempt, at `now_ns`.
    #[must_use]
    pub fn elapsed_ns(&self, now_ns: u64) -> u64 {
        self.began_ns.map_or(0, |b| now_ns.saturating_sub(b))
    }

    /// One attempt at `now_ns` (any monotonic clock, the same for every
    /// call): `Ok(Some(device))` once `START_USER_RECOVERY` succeeded and the
    /// queue is re-opened with every tag fetched (the caller serves it and
    /// ends the recovery with the device's
    /// [`crate::DeviceControl::begin_start`]); `Ok(None)` while the driver
    /// has not yet quiesced the device or the dead process still holds
    /// `/dev/ublkcN` (call again after a turn of the serving loop). A call
    /// sooner than the retry interval after the last attempt issues no
    /// command and answers `Ok(None)`, unless the budget has run out.
    ///
    /// # Errors
    /// [`RecoveryError`]; after an error, or after the device was returned,
    /// further calls answer [`RecoveryError::NotRecoverable`].
    pub fn poll(
        &mut self,
        host: &mut dyn UblkHost,
        now_ns: u64,
    ) -> Result<Option<Box<dyn HostDevice>>, RecoveryError> {
        if self.done {
            return Err(RecoveryError::NotRecoverable(
                "its recovery has already ended".to_string(),
            ));
        }
        let result = self.attempt(host, now_ns);
        if !matches!(result, Ok(None)) {
            self.done = true;
        }
        result
    }

    fn attempt(
        &mut self,
        host: &mut dyn UblkHost,
        now_ns: u64,
    ) -> Result<Option<Box<dyn HostDevice>>, RecoveryError> {
        let began = *self.began_ns.get_or_insert(now_ns);
        if let Some(last) = self.last_ns {
            let early = now_ns < last.saturating_add(self.retry_ns);
            if early && now_ns.saturating_sub(began) < self.budget_ns {
                return Ok(None);
            }
        }
        self.last_ns = Some(now_ns);
        self.attempts += 1;
        let info = match host.info(self.dev_id) {
            Ok(Some(info)) => info,
            Ok(None) => return Err(RecoveryError::Gone),
            Err(e) if e.errno == ENODEV => return Err(RecoveryError::Gone),
            Err(e) => return Err(RecoveryError::Command(e)),
        };
        self.check(&info, host)?;
        if info.state == DevState::Quiesced {
            match host.start_recovery(self.dev_id) {
                Ok(()) => {
                    return host
                        .reattach(&info)
                        .map(Some)
                        .map_err(RecoveryError::Reattach)
                }
                Err(e) if e.errno == EBUSY => {}
                Err(e) if e.errno == ENODEV => return Err(RecoveryError::Gone),
                Err(e) if e.errno == EINVAL => {
                    return Err(RecoveryError::NotRecoverable(
                        "START_USER_RECOVERY answered EINVAL: no queue of it is ready (a \
                         recovery was begun and its fetches never completed)"
                            .to_string(),
                    ))
                }
                Err(e) => return Err(RecoveryError::Command(e)),
            }
        }
        // Not quiesced yet (`LIVE` with a dead server), or `EBUSY`: wait.
        let waited_ns = now_ns.saturating_sub(began);
        if waited_ns >= self.budget_ns {
            return Err(RecoveryError::TimedOut {
                waited_ns,
                state: info.state,
            });
        }
        Ok(None)
    }

    fn check(&self, info: &DevInfo, host: &mut dyn UblkHost) -> Result<(), RecoveryError> {
        let refuse = |why: String| Err(RecoveryError::NotRecoverable(why));
        if !info.can_recover() {
            return refuse("it was created without UBLK_F_USER_RECOVERY".to_string());
        }
        if info.stamp != self.expect.stamp {
            return refuse(format!(
                "its ublksrv_flags {:#018x} are not the stamp {:#018x} of this pool's volume",
                info.stamp, self.expect.stamp
            ));
        }
        if info.queues != 1 || info.depth == 0 {
            return refuse(format!(
                "it has {} queue(s) of {} tags; this daemon serves one queue",
                info.queues, info.depth
            ));
        }
        if let Some(size) = info.size_blocks {
            if size != self.expect.size_blocks {
                return refuse(format!(
                    "it holds {size} blocks and the volume {}",
                    self.expect.size_blocks
                ));
            }
        }
        if info.max_io_bytes > self.expect.max_io_bytes {
            return refuse(format!(
                "it takes requests of {} bytes and this daemon's arena at most {}",
                info.max_io_bytes, self.expect.max_io_bytes
            ));
        }
        match info.state {
            DevState::Dead => refuse("it was never started, or was stopped".to_string()),
            DevState::Other(v) => refuse(format!("its state {v} is unknown")),
            DevState::Live if info.pid > 0 && info.pid as u32 == host.pid() => {
                refuse("this process serves it already".to_string())
            }
            DevState::Live if info.pid > 0 && host.pid_alive(info.pid) => {
                refuse(format!("process {} serves it", info.pid))
            }
            DevState::Live | DevState::Quiesced => Ok(()),
        }
    }
}

/// How a device on the host stands with this daemon.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Standing {
    /// This daemon serves it.
    Serving,
    /// Its server is dead and it carries an OpenTAP stamp a daemon booting
    /// its pool re-attaches.
    Recoverable,
    /// Its server is dead and nothing will re-attach it: deleting it loses
    /// only the requests it holds, which fail.
    Stale,
    /// Not this daemon's to touch: another live process serves it, another
    /// pool owns it, or other software may still be setting it up.
    Foreign,
}

impl Standing {
    /// `serving`, `recoverable`, `stale` or `foreign`.
    #[must_use]
    pub const fn word(self) -> &'static str {
        match self {
            Self::Serving => "serving",
            Self::Recoverable => "recoverable",
            Self::Stale => "stale",
            Self::Foreign => "foreign",
        }
    }
}

/// What a daemon knows that `GET_DEV_INFO` does not say.
pub struct Owner<'a> {
    /// The booted pool's fingerprint ([`crate::host::pool_fingerprint`]),
    /// if a pool is booted.
    pub pool: Option<u32>,
    /// The devices this daemon serves, with their volumes.
    pub serving: &'a [(u32, u32)],
    /// The devices the daemon recorded as stale, with why; a record counts
    /// only while the device carries the booted pool's stamp and no live
    /// process serves it.
    pub stale: &'a [(u32, String)],
    /// For the booted pool: whether volume `vol` would take a device (it is
    /// online and has none). A stamped device of the booted pool whose
    /// volume would not is stale.
    pub wants: &'a dyn Fn(u16) -> bool,
}

impl fmt::Debug for Owner<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Owner")
            .field("pool", &self.pool)
            .field("serving", &self.serving)
            .field("stale", &self.stale)
            .finish_non_exhaustive()
    }
}

/// A device and its standing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Probed {
    /// What `GET_DEV_INFO` said.
    pub info: DevInfo,
    /// Whether the pid it names is a live process.
    pub server_alive: bool,
    /// Its standing.
    pub standing: Standing,
    /// Why, for the operator.
    pub reason: String,
    /// The volume it exposes: the one this daemon serves it for, or the
    /// one its stamp names.
    pub vol: Option<u32>,
}

impl Probed {
    /// The server died with every tag in hand and the driver has not yet
    /// quiesced the device: `STOP_DEV` would wait for the block layer's
    /// request timeout.
    #[must_use]
    pub const fn quiescing(&self) -> bool {
        matches!(self.info.state, DevState::Live) && !self.server_alive
    }
}

/// The standing of `info`, whose pid is a live process when
/// `server_alive`, for a daemon of pid `pid` that knows `owner`.
#[must_use]
pub fn classify(info: &DevInfo, server_alive: bool, pid: u32, owner: &Owner<'_>) -> Probed {
    let stamp = info.opentap();
    let probed = |standing: Standing, reason: String, vol: Option<u32>| Probed {
        info: *info,
        server_alive,
        standing,
        reason,
        vol,
    };
    let stamped_vol = stamp.map(|s| u32::from(s.vol));
    if let Some((_, vol)) = owner.serving.iter().find(|(d, _)| *d == info.dev_id) {
        return probed(
            Standing::Serving,
            format!("served by this daemon for volume {vol}"),
            Some(*vol),
        );
    }
    // A live server that is not this daemon wins over any record: what
    // another process serves is never this daemon's to delete.
    let ours = info.pid > 0 && info.pid as u32 == pid;
    if info.state == DevState::Live && server_alive && !ours {
        return probed(
            Standing::Foreign,
            format!("served by process {}", info.pid),
            stamped_vol,
        );
    }
    // A record of the daemon's counts only for a device that still carries
    // the booted pool's stamp: a number the driver gave to someone else
    // since is not that device.
    let of_this_pool = owner.pool.is_some() && stamp.map(|s| s.pool) == owner.pool;
    if let Some((_, why)) = owner.stale.iter().find(|(d, _)| *d == info.dev_id) {
        if of_this_pool {
            return probed(Standing::Stale, why.clone(), stamped_vol);
        }
    }
    if info.state == DevState::Live && ours {
        return probed(
            Standing::Stale,
            "live under this daemon's pid, which does not serve it".to_string(),
            stamped_vol,
        );
    }
    match stamp {
        Some(s) => match owner.pool {
            Some(pool) if pool != s.pool => probed(
                Standing::Foreign,
                format!(
                    "a device of another OpenTAP pool (fingerprint {:08x}), volume {}",
                    s.pool, s.vol
                ),
                stamped_vol,
            ),
            Some(_) => {
                if info.state == DevState::Dead {
                    probed(
                        Standing::Stale,
                        format!(
                            "a device of volume {} of this pool, never started or stopped",
                            s.vol
                        ),
                        stamped_vol,
                    )
                } else if info.can_recover() && (owner.wants)(s.vol) {
                    probed(
                        Standing::Recoverable,
                        format!("volume {} of this pool, which has no device", s.vol),
                        stamped_vol,
                    )
                } else {
                    probed(
                        Standing::Stale,
                        format!(
                            "volume {} of this pool is offline, gone, or has another device",
                            s.vol
                        ),
                        stamped_vol,
                    )
                }
            }
            None => {
                if info.state == DevState::Dead {
                    probed(
                        Standing::Stale,
                        format!(
                            "a device of OpenTAP pool {:08x}, never started or stopped",
                            s.pool
                        ),
                        stamped_vol,
                    )
                } else if info.can_recover() {
                    probed(
                        Standing::Recoverable,
                        format!(
                            "volume {} of OpenTAP pool {:08x}: booting that pool re-attaches it",
                            s.vol, s.pool
                        ),
                        stamped_vol,
                    )
                } else {
                    probed(
                        Standing::Stale,
                        format!(
                            "a device of OpenTAP pool {:08x} created without \
                             UBLK_F_USER_RECOVERY",
                            s.pool
                        ),
                        stamped_vol,
                    )
                }
            }
        },
        None => {
            if info.state == DevState::Dead && info.pid <= 0 {
                probed(
                    Standing::Foreign,
                    "not an OpenTAP device and never started: its owner may still be \
                     setting it up"
                        .to_string(),
                    None,
                )
            } else {
                probed(
                    Standing::Stale,
                    "not an OpenTAP device, and no live process serves it".to_string(),
                    None,
                )
            }
        }
    }
}

/// Every device of the host with its standing, by id.
///
/// # Errors
/// The control plane cannot be reached.
pub fn probe(host: &mut dyn UblkHost, owner: &Owner<'_>) -> Result<Vec<Probed>, TransportError> {
    let pid = host.pid();
    let mut out = Vec::new();
    for info in host.list()? {
        let alive = info.pid > 0 && host.pid_alive(info.pid);
        out.push(classify(&info, alive, pid, owner));
    }
    Ok(out)
}

/// Whether `device` may be deleted: a stale device always, a recoverable
/// one only when the operator named it (`explicit`), never one that is
/// served (by this daemon or another process) or another pool's, and never
/// one still quiescing (the delete would wait for the block layer's
/// request timeout on the engine thread).
///
/// # Errors
/// Why not, for the operator.
pub fn reap_check(device: &Probed, explicit: bool) -> Result<(), String> {
    let id = device.info.dev_id;
    match device.standing {
        Standing::Serving => {
            return Err(format!(
                "device {id} is serving volume {}; take the volume offline (`otapctl volume \
                 offline`) to delete its device",
                device
                    .vol
                    .map_or_else(|| "?".to_string(), |v| v.to_string())
            ))
        }
        Standing::Foreign => {
            return Err(format!(
                "device {id} is not this daemon's to delete: {}",
                device.reason
            ))
        }
        Standing::Recoverable if !explicit => {
            return Err(format!(
                "device {id} is recoverable ({}); name it to delete it",
                device.reason
            ))
        }
        Standing::Recoverable | Standing::Stale => {}
    }
    if device.quiescing() {
        return Err(format!(
            "device {id} is still quiescing: its server died with every request in hand, and \
             the kernel quiesces it within the block layer's request timeout (30 s); try again"
        ));
    }
    Ok(())
}
