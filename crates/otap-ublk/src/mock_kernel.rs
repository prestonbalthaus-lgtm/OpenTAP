//! The 6.12 ublk driver in memory (Step 6B): devices that outlive the
//! process serving them, the control commands a daemon sends, the queues
//! it serves, and the block layer's side, so the recovery re-attach, the
//! reaper and a daemon's restart run on any host.
//!
//! [`MockKernel`] is the driver, shared by every "process" of a test.
//! [`MockKernel::process`] starts one: a [`MockHost`] (its
//! [`UblkHost`]) with its own pid, alive until the host, every device and
//! every queue it made are dropped. Dropping a queue is its server's exit as
//! the driver sees it: the cancel of the pending fetches, the abort of the
//! requests it held (requeued under `UBLK_F_USER_RECOVERY_REISSUE`, failed
//! with `EIO` without it), the quiesce, the release of `/dev/ublkcN`. The
//! test is also the block layer: it submits requests to a device
//! ([`MockKernel::submit_write`] and the others), which the driver hands to
//! whichever server has a fetch pending, and reads their results, each
//! with the number of times it was delivered.
//!
//! The rules modelled, each from the function of `ublk_drv.c` named in
//! [`crate::recovery`]: `START_USER_RECOVERY`'s `EINVAL` and `EBUSY`; a dead
//! server's device staying `LIVE` until the request timeout when it held
//! every tag ([`MockKernel::request_timeout`]); the character device held
//! by a dying process ([`MockKernel::hold_char_after_exit`],
//! [`MockKernel::release_char`], or a number of `EBUSY` answers with
//! [`MockKernel::busy_for`]); `END_USER_RECOVERY`'s `EBUSY`;
//! `START_DEV`'s partition scan through the queue (as [`crate::MockStart`]);
//! `DEL_DEV` refusing to return while `/dev/ublkcN` is open (the real one
//! waits forever); `STOP_DEV` failing the held I/O; `ublks_max`.
//! [`MockKernel::fail_next`] makes a command fail once and
//! [`MockKernel::kill_before`] makes the daemon issuing a command die there
//! (a panic, which unwinds the daemon's state as `kill -9` ends it).

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use otap_cp::IoError;
use otap_format::BLOCK_SIZE;
use otap_sys::ublk::{
    UblksrvIoDesc, UBLK_F_CMD_IOCTL_ENCODE, UBLK_F_UNPRIVILEGED_DEV, UBLK_F_URING_CMD_COMP_IN_TASK,
    UBLK_F_USER_COPY, UBLK_F_USER_RECOVERY, UBLK_F_USER_RECOVERY_REISSUE, UBLK_IO_OP_DISCARD,
    UBLK_IO_OP_FLUSH, UBLK_IO_OP_READ, UBLK_IO_OP_WRITE, UBLK_S_DEV_DEAD, UBLK_S_DEV_LIVE,
    UBLK_S_DEV_QUIESCED,
};

use crate::frontdoor::{QueueEvent, Transport, TransportError, EINVAL, EIO, ENODEV};
use crate::host::{BoxedTransport, DevInfo, DevState, HostDevice, UblkHost, EACCES, EBUSY, EINTR};
use crate::spec::DeviceSpec;
use crate::start::DeviceControl;

/// `EEXIST`.
const EEXIST: i32 = 17;

/// The flags every device the mock creates is granted, as `ADD_DEV` grants
/// them to `device::UblkDevice::create`'s request (the driver ORs in
/// `URING_CMD_COMP_IN_TASK`).
pub const MOCK_DEVICE_FLAGS: u64 = UBLK_F_USER_COPY
    | UBLK_F_USER_RECOVERY
    | UBLK_F_USER_RECOVERY_REISSUE
    | UBLK_F_CMD_IOCTL_ENCODE
    | UBLK_F_URING_CMD_COMP_IN_TASK;

/// A control step a daemon takes, for [`MockKernel::fail_next`] and
/// [`MockKernel::kill_before`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum MockCmd {
    /// `ADD_DEV` (and `SET_PARAMS`).
    AddDev,
    /// `START_DEV`.
    StartDev,
    /// `STOP_DEV`.
    StopDev,
    /// `DEL_DEV`.
    DelDev,
    /// `DEL_DEV_ASYNC`.
    DelDevAsync,
    /// `GET_DEV_INFO`.
    GetDevInfo,
    /// `START_USER_RECOVERY`.
    StartUserRecovery,
    /// Opening `/dev/ublkcN` and fetching every tag after
    /// `START_USER_RECOVERY`.
    Reattach,
    /// `END_USER_RECOVERY`.
    EndUserRecovery,
}

/// A device as other software leaves it ([`MockKernel::add_foreign`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ForeignDevice {
    /// Its `ublksrv_flags`.
    pub stamp: u64,
    /// Its state.
    pub state: DevState,
    /// The pid it names (a [`MockKernel::foreign_process`] for a live
    /// server, any other value for a dead one).
    pub pid: i32,
    /// Created with `UBLK_F_USER_RECOVERY`.
    pub recovery: bool,
    /// Capacity in blocks.
    pub size_blocks: u64,
    /// Tags.
    pub depth: u16,
}

/// What the block layer got back for a request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MockCompletion {
    /// Bytes, 0, or a negative errno.
    pub result: i32,
    /// A read's data.
    pub data: Option<Vec<u8>>,
    /// How many times a server was handed the request.
    pub deliveries: u32,
}

#[derive(Debug, Clone)]
struct AppReq {
    /// 0 for the partition scan of a `START_DEV`.
    id: u64,
    desc: UblksrvIoDesc,
    data: Option<Vec<u8>>,
    delivered: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Ctl {
    Idle,
    /// `START_DEV` in flight: the scan is delivered once the queue is
    /// ready, the start completes once it is answered.
    Start {
        scan_sent: bool,
        scan_done: bool,
    },
    /// `END_USER_RECOVERY` in flight.
    End,
}

#[derive(Debug)]
struct KDev {
    flags: u64,
    stamp: u64,
    depth: u16,
    max_io: u32,
    size_blocks: u64,
    state: u16,
    pid: i32,
    /// The pid holding `/dev/ublkcN` (`UB_STATE_OPEN`).
    open_by: Option<i32>,
    /// `nr_queues_ready == nr_hw_queues` (one queue).
    queue_ready: bool,
    /// The block device exists (`ub_disk`).
    disk: bool,
    /// The queue handle whose fetches the driver holds.
    queue_gen: u64,
    fetched: BTreeSet<u16>,
    owned: BTreeMap<u16, AppReq>,
    read_back: BTreeMap<u16, Vec<u8>>,
    /// Requests waiting for a server: requeued ones first.
    waiting: VecDeque<AppReq>,
    /// Tags whose fetch completes with `UBLK_IO_RES_ABORT` at the next poll.
    aborts: Vec<u16>,
    /// The server died holding every tag; the timeout has not fired.
    dying: bool,
    /// Keep `/dev/ublkcN` held after the server's death until
    /// [`MockKernel::release_char`].
    hold_char: bool,
    /// `START_USER_RECOVERY` attempts still to answer `EBUSY`
    /// ([`MockKernel::busy_for`]).
    busy_attempts: u32,
    /// `DEL_DEV_ASYNC`'d: removed once released.
    deleted: bool,
    ctl: Ctl,
}

impl KDev {
    fn info(&self, dev_id: u32) -> DevInfo {
        DevInfo {
            dev_id,
            state: DevState::from_raw(self.state),
            pid: self.pid,
            flags: self.flags,
            stamp: self.stamp,
            queues: 1,
            depth: self.depth,
            max_io_bytes: self.max_io,
            size_blocks: Some(self.size_blocks),
        }
    }
}

#[derive(Debug, Default)]
struct Kernel {
    devices: BTreeMap<u32, KDev>,
    live_pids: BTreeSet<i32>,
    next_pid: i32,
    next_req: u64,
    next_queue: u64,
    max_devices: u32,
    completions: BTreeMap<u64, MockCompletion>,
    fail_next: Vec<(MockCmd, i32)>,
    kill_before: Option<MockCmd>,
    fetch_limit: Option<u16>,
    log: Vec<String>,
}

impl Kernel {
    /// `GET_DEV_INFO` without a character-device path, as `LinuxHost` sends
    /// it: a device created with `UBLK_F_UNPRIVILEGED_DEV` answers `EINVAL`
    /// (`ublk_ctrl_uring_cmd_permission` wants the path for every command
    /// on such a device); a deleted one is gone.
    fn get_dev_info(&self, dev_id: u32) -> Result<Option<DevInfo>, i32> {
        match self.devices.get(&dev_id).filter(|d| !d.deleted) {
            None => Ok(None),
            Some(d) if d.flags & UBLK_F_UNPRIVILEGED_DEV != 0 => Err(EINVAL),
            Some(d) => Ok(Some(d.info(dev_id))),
        }
    }

    fn fail(&mut self, cmd: MockCmd) -> Option<i32> {
        let pos = self.fail_next.iter().position(|(c, _)| *c == cmd)?;
        Some(self.fail_next.remove(pos).1)
    }

    fn complete(&mut self, req: AppReq, result: i32, data: Option<Vec<u8>>) {
        if req.id == 0 {
            return;
        }
        self.completions.insert(
            req.id,
            MockCompletion {
                result,
                data,
                deliveries: req.delivered,
            },
        );
    }

    /// The abort and quiesce of a dead server's device
    /// (`ublk_abort_queue`, `__ublk_quiesce_dev`).
    fn abort_and_quiesce(&mut self, dev_id: u32) {
        let Some(dev) = self.devices.get_mut(&dev_id) else {
            return;
        };
        let reissue = dev.flags & UBLK_F_USER_RECOVERY_REISSUE != 0;
        let owned: Vec<AppReq> = std::mem::take(&mut dev.owned).into_values().collect();
        dev.read_back.clear();
        let mut failed = Vec::new();
        for req in owned.into_iter().rev() {
            if req.id == 0 {
                continue;
            }
            if reissue {
                dev.waiting.push_front(req);
            } else {
                failed.push(req);
            }
        }
        dev.state = UBLK_S_DEV_QUIESCED;
        dev.dying = false;
        for req in failed {
            self.complete(req, -EIO, None);
        }
    }

    /// `STOP_DEV` (`ublk_stop_dev`): the held I/O fails, the block device
    /// goes, the fetches complete with an abort.
    fn stop(&mut self, dev_id: u32) -> Result<(), TransportError> {
        let Some(dev) = self.devices.get_mut(&dev_id) else {
            return Err(TransportError {
                what: "STOP_DEV of no device",
                errno: ENODEV,
            });
        };
        if dev.dying {
            return Err(TransportError {
                what: "STOP_DEV of a device still quiescing waits for the request timeout",
                errno: EBUSY,
            });
        }
        // `ublk_cancel_dev`: every pending fetch completes with an abort.
        let fetched: Vec<u16> = std::mem::take(&mut dev.fetched).into_iter().collect();
        dev.aborts.extend(fetched);
        if dev.state == UBLK_S_DEV_DEAD && !dev.disk {
            return Ok(());
        }
        let mut failed: Vec<AppReq> = std::mem::take(&mut dev.owned).into_values().collect();
        failed.extend(dev.waiting.drain(..));
        dev.read_back.clear();
        dev.state = UBLK_S_DEV_DEAD;
        dev.pid = -1;
        dev.disk = false;
        dev.ctl = Ctl::Idle;
        for req in failed {
            self.complete(req, -EIO, None);
        }
        Ok(())
    }

    /// The server of `gen`'s queue is gone (`ublk_uring_cmd_cancel_fn`,
    /// then `ublk_ch_release`).
    fn server_exit(&mut self, dev_id: u32, gen: u64) {
        let Some(dev) = self.devices.get_mut(&dev_id) else {
            return;
        };
        if dev.queue_gen != gen {
            return;
        }
        let had_fetch = !dev.fetched.is_empty();
        dev.fetched.clear();
        dev.aborts.clear();
        dev.queue_gen = 0;
        let quiesce = dev.state == UBLK_S_DEV_LIVE && dev.disk;
        if quiesce && !had_fetch {
            // Nothing to cancel: only the request timeout aborts.
            dev.dying = true;
        }
        // A command the dead process had in flight went with it.
        dev.ctl = Ctl::Idle;
        if dev.hold_char {
            dev.hold_char = false;
        } else {
            dev.open_by = None;
        }
        let remove = dev.deleted && dev.open_by.is_none();
        if quiesce && had_fetch {
            self.abort_and_quiesce(dev_id);
        }
        if remove {
            self.devices.remove(&dev_id);
        }
    }

    fn open_queue(
        &mut self,
        dev_id: u32,
        pid: i32,
        kernel: &MockKernel,
        proc: &Arc<Proc>,
    ) -> Result<MockQueue, TransportError> {
        let gen = {
            self.next_queue += 1;
            self.next_queue
        };
        let limit = self.fetch_limit.take();
        let Some(dev) = self.devices.get_mut(&dev_id) else {
            return Err(TransportError {
                what: "open of /dev/ublkcN: no device",
                errno: ENODEV,
            });
        };
        if dev.open_by.is_some() {
            return Err(TransportError {
                what: "open of /dev/ublkcN another process holds",
                errno: EBUSY,
            });
        }
        let fetch = limit.map_or(dev.depth, |l| l.min(dev.depth));
        dev.fetched = (0..fetch).collect();
        dev.queue_ready = fetch == dev.depth;
        if !dev.queue_ready {
            // The process died part-way through its fetches: the driver
            // cancels them, and its queue never became ready.
            dev.fetched.clear();
            return Err(TransportError {
                what: "the process died while fetching its tags",
                errno: EINTR,
            });
        }
        dev.open_by = Some(pid);
        dev.queue_gen = gen;
        Ok(MockQueue {
            kernel: kernel.clone(),
            proc: Arc::clone(proc),
            dev_id,
            gen,
        })
    }
}

/// The driver, shared by every process of a test.
#[derive(Debug, Clone)]
pub struct MockKernel {
    model: Arc<Mutex<Kernel>>,
}

impl Default for MockKernel {
    fn default() -> Self {
        Self::new()
    }
}

/// A process that exists while any of its handles does.
#[derive(Debug)]
struct Proc {
    kernel: MockKernel,
    pid: i32,
}

impl Drop for Proc {
    fn drop(&mut self) {
        self.kernel.lock().live_pids.remove(&self.pid);
    }
}

impl MockKernel {
    /// A driver with no devices and `ublks_max` 64.
    #[must_use]
    pub fn new() -> Self {
        Self {
            model: Arc::new(Mutex::new(Kernel {
                max_devices: 64,
                next_pid: 1000,
                ..Kernel::default()
            })),
        }
    }

    fn lock(&self) -> MutexGuard<'_, Kernel> {
        // A test that panicked while holding the lock has failed already,
        // and a simulated kill panics with the lock released.
        self.model.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Starts a daemon process: its control plane, with a new pid.
    #[must_use]
    pub fn process(&self) -> MockHost {
        let pid = {
            let mut k = self.lock();
            k.next_pid += 1;
            let pid = k.next_pid;
            k.live_pids.insert(pid);
            pid
        };
        MockHost {
            kernel: self.clone(),
            proc: Arc::new(Proc {
                kernel: self.clone(),
                pid,
            }),
        }
    }

    /// Marks device `dev_id` as created by unprivileged software
    /// (`UBLK_F_UNPRIVILEGED_DEV`): `GET_DEV_INFO` without its character
    /// device's path answers `EINVAL` for it.
    pub fn set_unprivileged(&self, dev_id: u32) {
        if let Some(d) = self.lock().devices.get_mut(&dev_id) {
            d.flags |= UBLK_F_UNPRIVILEGED_DEV;
        }
    }

    /// Starts a daemon process that the kernel gave the pid of a dead one
    /// (`pid` reused, as for `dbladed` running as PID 1 of a container).
    ///
    /// # Panics
    /// `pid` is a live process.
    #[must_use]
    pub fn process_with_pid(&self, pid: i32) -> MockHost {
        {
            let mut k = self.lock();
            assert!(!k.live_pids.contains(&pid), "pid {pid} is alive");
            k.live_pids.insert(pid);
        }
        MockHost {
            kernel: self.clone(),
            proc: Arc::new(Proc {
                kernel: self.clone(),
                pid,
            }),
        }
    }

    /// A live process of other software, which serves whatever device
    /// names it; it lives until [`end_foreign`](Self::end_foreign).
    #[must_use]
    pub fn foreign_process(&self) -> i32 {
        let mut k = self.lock();
        k.next_pid += 1;
        let pid = k.next_pid;
        k.live_pids.insert(pid);
        pid
    }

    /// Ends a [`foreign_process`](Self::foreign_process).
    pub fn end_foreign(&self, pid: i32) {
        self.lock().live_pids.remove(&pid);
    }

    /// Whether `pid` is alive.
    #[must_use]
    pub fn pid_alive(&self, pid: i32) -> bool {
        self.lock().live_pids.contains(&pid)
    }

    /// Adds a device as other software (or a Step 5 binary) leaves it.
    ///
    /// # Panics
    /// `ublks_max` devices exist.
    #[must_use]
    pub fn add_foreign(&self, dev: ForeignDevice) -> u32 {
        let mut k = self.lock();
        let id = (0..k.max_devices)
            .find(|i| !k.devices.contains_key(i))
            .expect("room for the device");
        let mut flags = UBLK_F_USER_COPY | UBLK_F_CMD_IOCTL_ENCODE | UBLK_F_URING_CMD_COMP_IN_TASK;
        if dev.recovery {
            flags |= UBLK_F_USER_RECOVERY;
        }
        k.devices.insert(
            id,
            KDev {
                flags,
                stamp: dev.stamp,
                depth: dev.depth,
                max_io: 1 << 20,
                size_blocks: dev.size_blocks,
                state: dev.state.raw(),
                pid: dev.pid,
                open_by: None,
                queue_ready: dev.state != DevState::Dead,
                disk: dev.state != DevState::Dead,
                queue_gen: 0,
                fetched: BTreeSet::new(),
                owned: BTreeMap::new(),
                read_back: BTreeMap::new(),
                waiting: VecDeque::new(),
                aborts: Vec::new(),
                dying: false,
                hold_char: false,
                busy_attempts: 0,
                deleted: false,
                ctl: Ctl::Idle,
            },
        );
        id
    }

    /// The next `attempts` `START_USER_RECOVERY` of `dev_id` answer
    /// `EBUSY`, as they do while a dying process's io_uring context still
    /// holds `/dev/ublkcN` (the kernel releases it asynchronously); the
    /// one after is judged as usual.
    pub fn busy_for(&self, dev_id: u32, attempts: u32) {
        if let Some(dev) = self.lock().devices.get_mut(&dev_id) {
            dev.busy_attempts = attempts;
        }
    }

    /// Sets `ublks_max`.
    pub fn set_max_devices(&self, max: u32) {
        self.lock().max_devices = max;
    }

    /// Makes the next `cmd` fail with `errno` (once).
    pub fn fail_next(&self, cmd: MockCmd, errno: i32) {
        self.lock().fail_next.push((cmd, errno));
    }

    /// Kills the daemon that next issues `cmd`, before the driver sees it:
    /// that call panics, unwinding the daemon as `kill -9` ends it.
    pub fn kill_before(&self, cmd: MockCmd) {
        self.lock().kill_before = Some(cmd);
    }

    /// The next queue opened fetches only `tags` tags before its process
    /// dies: its open fails and the driver has no ready queue for the
    /// device.
    pub fn fail_fetch_after(&self, tags: u16) {
        self.lock().fetch_limit = Some(tags);
    }

    /// Keeps `/dev/ublkcN` of `dev_id` held after its server's death, as
    /// a process whose io_uring context the kernel tears down late does,
    /// until [`release_char`](Self::release_char).
    pub fn hold_char_after_exit(&self, dev_id: u32) {
        if let Some(dev) = self.lock().devices.get_mut(&dev_id) {
            dev.hold_char = true;
        }
    }

    /// The dead process's last reference to `/dev/ublkcN` of `dev_id` goes
    /// (`ublk_ch_release`).
    pub fn release_char(&self, dev_id: u32) {
        let mut k = self.lock();
        let remove = match k.devices.get_mut(&dev_id) {
            Some(dev) if dev.queue_gen == 0 => {
                dev.open_by = None;
                dev.deleted
            }
            _ => false,
        };
        if remove {
            k.devices.remove(&dev_id);
        }
    }

    /// The block layer's request timeout fires on `dev_id` (`ublk_timeout`):
    /// a device whose dead server held every tag is aborted and quiesced.
    pub fn request_timeout(&self, dev_id: u32) {
        let mut k = self.lock();
        if k.devices.get(&dev_id).is_some_and(|d| d.dying) {
            k.abort_and_quiesce(dev_id);
        }
    }

    fn submit(&self, dev_id: u32, op: u8, lba: u64, blocks: u32, data: Option<Vec<u8>>) -> u64 {
        let mut k = self.lock();
        k.next_req += 1;
        let id = k.next_req;
        let dev = k
            .devices
            .get_mut(&dev_id)
            .unwrap_or_else(|| panic!("no device {dev_id} to submit to"));
        assert!(dev.disk, "device {dev_id} exposes no block device");
        dev.waiting.push_back(AppReq {
            id,
            desc: desc(op, lba, blocks),
            data,
            delivered: 0,
        });
        id
    }

    /// The block layer writes `data` (whole blocks) at `lba` of `dev_id`.
    ///
    /// # Panics
    /// No such device, or it exposes no block device.
    #[must_use]
    pub fn submit_write(&self, dev_id: u32, lba: u64, data: Vec<u8>) -> u64 {
        let blocks = (data.len() / BLOCK_SIZE) as u32;
        self.submit(dev_id, UBLK_IO_OP_WRITE, lba, blocks, Some(data))
    }

    /// The block layer reads `blocks` blocks at `lba` of `dev_id`.
    ///
    /// # Panics
    /// As [`submit_write`](Self::submit_write).
    #[must_use]
    pub fn submit_read(&self, dev_id: u32, lba: u64, blocks: u32) -> u64 {
        self.submit(dev_id, UBLK_IO_OP_READ, lba, blocks, None)
    }

    /// The block layer flushes `dev_id`.
    ///
    /// # Panics
    /// As [`submit_write`](Self::submit_write).
    #[must_use]
    pub fn submit_flush(&self, dev_id: u32) -> u64 {
        self.submit(dev_id, UBLK_IO_OP_FLUSH, 0, 0, None)
    }

    /// The block layer discards `blocks` blocks at `lba` of `dev_id`.
    ///
    /// # Panics
    /// As [`submit_write`](Self::submit_write).
    #[must_use]
    pub fn submit_discard(&self, dev_id: u32, lba: u64, blocks: u32) -> u64 {
        self.submit(dev_id, UBLK_IO_OP_DISCARD, lba, blocks, None)
    }

    /// What request `id` completed with, once it has.
    #[must_use]
    pub fn completion(&self, id: u64) -> Option<MockCompletion> {
        self.lock().completions.get(&id).cloned()
    }

    /// Requests of `dev_id` not yet completed (waiting or held by a
    /// server).
    #[must_use]
    pub fn unanswered(&self, dev_id: u32) -> usize {
        self.lock().devices.get(&dev_id).map_or(0, |d| {
            d.waiting.len() + d.owned.values().filter(|r| r.id != 0).count()
        })
    }

    /// Requests of `dev_id` a server holds (handed over, not committed).
    #[must_use]
    pub fn held(&self, dev_id: u32) -> usize {
        self.lock()
            .devices
            .get(&dev_id)
            .map_or(0, |d| d.owned.len())
    }

    /// The device as `GET_DEV_INFO` describes it.
    #[must_use]
    pub fn info(&self, dev_id: u32) -> Option<DevInfo> {
        let k = self.lock();
        k.devices
            .get(&dev_id)
            .filter(|d| !d.deleted)
            .map(|d| d.info(dev_id))
    }

    /// Every device's id, `DEL_DEV_ASYNC`'d ones still held included.
    #[must_use]
    pub fn device_ids(&self) -> Vec<u32> {
        self.lock().devices.keys().copied().collect()
    }

    /// Whether `/dev/ublkcN` of `dev_id` is open.
    #[must_use]
    pub fn char_open(&self, dev_id: u32) -> bool {
        self.lock()
            .devices
            .get(&dev_id)
            .is_some_and(|d| d.open_by.is_some())
    }

    /// The control commands issued so far, as `"<pid> <command> <device>"`.
    #[must_use]
    pub fn log(&self) -> Vec<String> {
        self.lock().log.clone()
    }

    fn record(&self, pid: i32, cmd: MockCmd, dev_id: u32) {
        let mut k = self.lock();
        if k.kill_before == Some(cmd) {
            k.kill_before = None;
            drop(k);
            panic!(
                "mock kernel: the daemon (pid {pid}) was killed before {cmd:?} of device {dev_id}"
            );
        }
        k.log.push(format!("{pid} {cmd:?} {dev_id}"));
    }
}

fn desc(op: u8, lba: u64, blocks: u32) -> UblksrvIoDesc {
    let sectors = (BLOCK_SIZE / 512) as u64;
    UblksrvIoDesc {
        op_flags: u32::from(op),
        nr_sectors: (u64::from(blocks) * sectors) as u32,
        start_sector: lba * sectors,
        addr: 0,
    }
}

fn gone(what: &'static str) -> TransportError {
    TransportError {
        what,
        errno: ENODEV,
    }
}

/// A daemon process's control plane over a [`MockKernel`].
#[derive(Debug)]
pub struct MockHost {
    kernel: MockKernel,
    proc: Arc<Proc>,
}

impl MockHost {
    /// The driver.
    #[must_use]
    pub const fn kernel(&self) -> &MockKernel {
        &self.kernel
    }
}

impl UblkHost for MockHost {
    fn pid(&self) -> u32 {
        self.proc.pid as u32
    }

    fn pid_alive(&mut self, pid: i32) -> bool {
        self.kernel.pid_alive(pid)
    }

    fn list(&mut self) -> Result<Vec<DevInfo>, TransportError> {
        let k = self.kernel.lock();
        let ids: Vec<u32> = k
            .devices
            .iter()
            .filter(|(_, d)| !d.deleted)
            .map(|(id, _)| *id)
            .collect();
        crate::host::listing(&ids, |id| k.get_dev_info(id))
    }

    fn info(&mut self, dev_id: u32) -> Result<Option<DevInfo>, TransportError> {
        self.kernel
            .record(self.proc.pid, MockCmd::GetDevInfo, dev_id);
        let mut k = self.kernel.lock();
        if let Some(errno) = k.fail(MockCmd::GetDevInfo) {
            return Err(TransportError {
                what: "GET_DEV_INFO",
                errno,
            });
        }
        k.get_dev_info(dev_id).map_err(|errno| TransportError {
            what: "GET_DEV_INFO",
            errno,
        })
    }

    fn create(
        &mut self,
        spec: &DeviceSpec,
        stamp: u64,
    ) -> Result<Box<dyn HostDevice>, TransportError> {
        let pid = self.proc.pid;
        let dev_id = {
            let mut k = self.kernel.lock();
            if let Some(errno) = k.fail(MockCmd::AddDev) {
                return Err(TransportError {
                    what: "ADD_DEV",
                    errno,
                });
            }
            if k.devices.len() as u32 >= k.max_devices {
                return Err(TransportError {
                    what: "ADD_DEV beyond ublks_max",
                    errno: EACCES,
                });
            }
            (0..k.max_devices)
                .find(|i| !k.devices.contains_key(i))
                .unwrap_or(0)
        };
        self.kernel.record(pid, MockCmd::AddDev, dev_id);
        {
            let mut k = self.kernel.lock();
            k.devices.insert(
                dev_id,
                KDev {
                    flags: MOCK_DEVICE_FLAGS,
                    stamp,
                    depth: spec.depth,
                    max_io: spec.max_io_bytes,
                    size_blocks: spec.size_blocks,
                    state: UBLK_S_DEV_DEAD,
                    pid: 0,
                    open_by: None,
                    queue_ready: false,
                    disk: false,
                    queue_gen: 0,
                    fetched: BTreeSet::new(),
                    owned: BTreeMap::new(),
                    read_back: BTreeMap::new(),
                    waiting: VecDeque::new(),
                    aborts: Vec::new(),
                    dying: false,
                    hold_char: false,
                    busy_attempts: 0,
                    deleted: false,
                    ctl: Ctl::Idle,
                },
            );
        }
        let opened = self
            .kernel
            .lock()
            .open_queue(dev_id, pid, &self.kernel, &self.proc);
        match opened {
            Ok(queue) => Ok(Box::new(MockDevice {
                kernel: self.kernel.clone(),
                proc: Arc::clone(&self.proc),
                dev_id,
                depth: spec.depth,
                queue: Some(queue),
                recovered: false,
                in_flight: false,
                live: false,
            })),
            Err(e) => {
                // `UblkDevice::create` deletes the device it could not
                // finish.
                self.kernel.lock().devices.remove(&dev_id);
                Err(e)
            }
        }
    }

    fn start_recovery(&mut self, dev_id: u32) -> Result<(), TransportError> {
        self.kernel
            .record(self.proc.pid, MockCmd::StartUserRecovery, dev_id);
        let mut k = self.kernel.lock();
        if let Some(errno) = k.fail(MockCmd::StartUserRecovery) {
            return Err(TransportError {
                what: "START_USER_RECOVERY",
                errno,
            });
        }
        let Some(dev) = k.devices.get_mut(&dev_id).filter(|d| !d.deleted) else {
            return Err(gone("START_USER_RECOVERY of no device"));
        };
        if dev.flags & UBLK_F_USER_RECOVERY == 0 || !dev.queue_ready {
            return Err(TransportError {
                what: "START_USER_RECOVERY",
                errno: EINVAL,
            });
        }
        if dev.busy_attempts > 0 {
            dev.busy_attempts -= 1;
            return Err(TransportError {
                what: "START_USER_RECOVERY",
                errno: EBUSY,
            });
        }
        if dev.open_by.is_some() || dev.state != UBLK_S_DEV_QUIESCED {
            return Err(TransportError {
                what: "START_USER_RECOVERY",
                errno: EBUSY,
            });
        }
        // `ublk_queue_reinit`: every slot forgotten, no queue ready.
        dev.queue_ready = false;
        dev.fetched.clear();
        dev.aborts.clear();
        dev.queue_gen = 0;
        let owned: Vec<AppReq> = std::mem::take(&mut dev.owned).into_values().collect();
        for req in owned.into_iter().rev() {
            if req.id != 0 {
                dev.waiting.push_front(req);
            }
        }
        Ok(())
    }

    fn reattach(&mut self, info: &DevInfo) -> Result<Box<dyn HostDevice>, TransportError> {
        let pid = self.proc.pid;
        self.kernel.record(pid, MockCmd::Reattach, info.dev_id);
        let mut k = self.kernel.lock();
        if let Some(errno) = k.fail(MockCmd::Reattach) {
            return Err(TransportError {
                what: "open of /dev/ublkcN",
                errno,
            });
        }
        let queue = k.open_queue(info.dev_id, pid, &self.kernel, &self.proc)?;
        let depth = k.devices.get(&info.dev_id).map_or(info.depth, |d| d.depth);
        drop(k);
        Ok(Box::new(MockDevice {
            kernel: self.kernel.clone(),
            proc: Arc::clone(&self.proc),
            dev_id: info.dev_id,
            depth,
            queue: Some(queue),
            recovered: true,
            in_flight: false,
            live: false,
        }))
    }

    fn reap(&mut self, dev_id: u32) -> Result<(), TransportError> {
        let pid = self.proc.pid;
        self.kernel.record(pid, MockCmd::StopDev, dev_id);
        {
            let mut k = self.kernel.lock();
            if let Some(errno) = k.fail(MockCmd::StopDev) {
                return Err(TransportError {
                    what: "STOP_DEV",
                    errno,
                });
            }
            if k.devices.get(&dev_id).is_none_or(|d| d.deleted) {
                return Err(gone("STOP_DEV of no device"));
            }
            k.stop(dev_id)?;
        }
        self.kernel.record(pid, MockCmd::DelDevAsync, dev_id);
        let mut k = self.kernel.lock();
        if let Some(errno) = k.fail(MockCmd::DelDevAsync) {
            return Err(TransportError {
                what: "DEL_DEV_ASYNC",
                errno,
            });
        }
        let remove = match k.devices.get_mut(&dev_id) {
            Some(dev) => {
                dev.deleted = true;
                dev.open_by.is_none()
            }
            None => false,
        };
        if remove {
            k.devices.remove(&dev_id);
        }
        Ok(())
    }
}

/// A device a [`MockHost`] created or re-attached.
#[derive(Debug)]
pub struct MockDevice {
    kernel: MockKernel,
    proc: Arc<Proc>,
    dev_id: u32,
    depth: u16,
    queue: Option<MockQueue>,
    recovered: bool,
    in_flight: bool,
    live: bool,
}

impl DeviceControl for MockDevice {
    fn begin_start(&mut self) -> Result<(), TransportError> {
        let cmd = if self.recovered {
            MockCmd::EndUserRecovery
        } else {
            MockCmd::StartDev
        };
        self.kernel.record(self.proc.pid, cmd, self.dev_id);
        let mut k = self.kernel.lock();
        if let Some(errno) = k.fail(cmd) {
            return Err(TransportError {
                what: if self.recovered {
                    "END_USER_RECOVERY"
                } else {
                    "START_DEV"
                },
                errno,
            });
        }
        let Some(dev) = k.devices.get_mut(&self.dev_id) else {
            return Err(gone("the device is gone"));
        };
        if self.live || self.in_flight || dev.ctl != Ctl::Idle {
            return Err(TransportError {
                what: "a start of a started device",
                errno: EEXIST,
            });
        }
        if self.recovered {
            dev.ctl = Ctl::End;
        } else {
            if dev.state == UBLK_S_DEV_LIVE || dev.disk {
                return Err(TransportError {
                    what: "START_DEV of a live device",
                    errno: EEXIST,
                });
            }
            dev.ctl = Ctl::Start {
                scan_sent: false,
                scan_done: false,
            };
        }
        self.in_flight = true;
        Ok(())
    }

    fn poll_start(&mut self) -> Result<bool, TransportError> {
        if self.live {
            return Ok(true);
        }
        if !self.in_flight {
            return Err(TransportError {
                what: "no start in flight",
                errno: EINVAL,
            });
        }
        let pid = self.proc.pid;
        let mut k = self.kernel.lock();
        let Some(dev) = k.devices.get_mut(&self.dev_id) else {
            self.in_flight = false;
            return Err(gone("the device is gone"));
        };
        match dev.ctl {
            Ctl::End => {
                if !dev.queue_ready {
                    return Ok(false);
                }
                dev.ctl = Ctl::Idle;
                self.in_flight = false;
                if dev.state != UBLK_S_DEV_QUIESCED {
                    return Err(TransportError {
                        what: "END_USER_RECOVERY",
                        errno: EBUSY,
                    });
                }
                dev.pid = pid;
                dev.state = UBLK_S_DEV_LIVE;
                self.live = true;
                Ok(true)
            }
            Ctl::Start { scan_done, .. } => {
                if !scan_done {
                    return Ok(false);
                }
                dev.ctl = Ctl::Idle;
                dev.pid = pid;
                dev.state = UBLK_S_DEV_LIVE;
                dev.disk = true;
                self.in_flight = false;
                self.live = true;
                Ok(true)
            }
            Ctl::Idle => {
                // The start was ended under it: the queue went, or a stop.
                self.in_flight = false;
                Err(TransportError {
                    what: "the start was interrupted",
                    errno: EINTR,
                })
            }
        }
    }
}

impl HostDevice for MockDevice {
    fn dev_id(&self) -> u32 {
        self.dev_id
    }

    fn depth(&self) -> u16 {
        self.depth
    }

    fn take_transport(&mut self) -> Option<BoxedTransport> {
        self.queue.take().map(|q| Box::new(q) as BoxedTransport)
    }

    fn start_in_flight(&self) -> bool {
        self.in_flight
    }

    fn delete(self: Box<Self>) -> Result<(), TransportError> {
        let pid = self.proc.pid;
        if self.live || self.recovered {
            self.kernel.record(pid, MockCmd::StopDev, self.dev_id);
            let mut k = self.kernel.lock();
            if let Some(errno) = k.fail(MockCmd::StopDev) {
                return Err(TransportError {
                    what: "STOP_DEV",
                    errno,
                });
            }
            if k.devices.contains_key(&self.dev_id) {
                k.stop(self.dev_id)?;
            }
        }
        self.kernel.record(pid, MockCmd::DelDev, self.dev_id);
        let mut k = self.kernel.lock();
        if let Some(errno) = k.fail(MockCmd::DelDev) {
            return Err(TransportError {
                what: "DEL_DEV",
                errno,
            });
        }
        let Some(dev) = k.devices.get(&self.dev_id) else {
            return Err(gone("DEL_DEV of no device"));
        };
        if dev.open_by.is_some() {
            // The real command waits, uninterruptibly for this caller,
            // until the character device is released.
            return Err(TransportError {
                what: "DEL_DEV with /dev/ublkcN still open would wait forever",
                errno: EBUSY,
            });
        }
        k.devices.remove(&self.dev_id);
        Ok(())
    }
}

/// A queue a [`MockHost`] serves: its fetches are pending in the driver
/// until it is dropped, which is its server's exit.
#[derive(Debug)]
pub struct MockQueue {
    kernel: MockKernel,
    proc: Arc<Proc>,
    dev_id: u32,
    gen: u64,
}

impl MockQueue {
    fn with_dev<T>(
        &self,
        f: impl FnOnce(&mut Kernel, u32) -> Result<T, TransportError>,
    ) -> Result<T, TransportError> {
        let mut k = self.kernel.lock();
        let current = k
            .devices
            .get(&self.dev_id)
            .is_some_and(|d| d.queue_gen == self.gen);
        if !current {
            return Err(gone("the queue's device is gone"));
        }
        f(&mut k, self.dev_id)
    }
}

impl Transport for MockQueue {
    fn poll(&mut self, out: &mut Vec<QueueEvent>) -> Result<usize, TransportError> {
        let pid = self.proc.pid;
        let mut k = self.kernel.lock();
        let Some(dev) = k
            .devices
            .get_mut(&self.dev_id)
            .filter(|d| d.queue_gen == self.gen)
        else {
            return Ok(0);
        };
        let mut n = 0;
        for tag in dev.aborts.drain(..) {
            out.push(QueueEvent::Abort { tag });
            n += 1;
        }
        // The partition scan of a `START_DEV` (as `MockStart`).
        if let Ctl::Start {
            scan_sent: false,
            scan_done,
        } = dev.ctl
        {
            if let Some(&tag) = dev.fetched.iter().next() {
                dev.fetched.remove(&tag);
                dev.owned.insert(
                    tag,
                    AppReq {
                        id: 0,
                        desc: desc(UBLK_IO_OP_READ, 0, 1),
                        data: None,
                        delivered: 1,
                    },
                );
                dev.ctl = Ctl::Start {
                    scan_sent: true,
                    scan_done,
                };
                out.push(QueueEvent::Request {
                    tag,
                    desc: desc(UBLK_IO_OP_READ, 0, 1),
                });
                n += 1;
            }
        }
        // Requests go to a live device's current server only: a quiesced
        // device, or one whose server died holding every tag, holds them.
        let serving = dev.state == UBLK_S_DEV_LIVE && dev.pid == pid && !dev.dying;
        while serving && !dev.waiting.is_empty() {
            let Some(&tag) = dev.fetched.iter().next() else {
                break;
            };
            let Some(mut req) = dev.waiting.pop_front() else {
                break;
            };
            dev.fetched.remove(&tag);
            req.delivered += 1;
            out.push(QueueEvent::Request {
                tag,
                desc: req.desc,
            });
            dev.owned.insert(tag, req);
            n += 1;
        }
        Ok(n)
    }

    fn read_data(&mut self, tag: u16, off: u32, dst: &mut [u8]) -> Result<(), IoError> {
        let k = self.kernel.lock();
        let data = k
            .devices
            .get(&self.dev_id)
            .filter(|d| d.queue_gen == self.gen)
            .and_then(|d| d.owned.get(&tag))
            .and_then(|r| r.data.as_ref())
            .ok_or(IoError::Corrupt)?;
        let start = off as usize;
        if start + dst.len() > data.len() {
            return Err(IoError::BadLength { bytes: dst.len() });
        }
        dst.copy_from_slice(&data[start..start + dst.len()]);
        Ok(())
    }

    fn write_data(&mut self, tag: u16, off: u32, src: &[u8]) -> Result<(), IoError> {
        let mut k = self.kernel.lock();
        let dev = k
            .devices
            .get_mut(&self.dev_id)
            .filter(|d| d.queue_gen == self.gen && d.owned.contains_key(&tag))
            .ok_or(IoError::Corrupt)?;
        let buf = dev.read_back.entry(tag).or_default();
        let start = off as usize;
        if buf.len() < start + src.len() {
            buf.resize(start + src.len(), 0);
        }
        buf[start..start + src.len()].copy_from_slice(src);
        Ok(())
    }

    fn commit(&mut self, tag: u16, result: i32) -> Result<(), TransportError> {
        self.with_dev(|k, dev_id| {
            let Some(dev) = k.devices.get_mut(&dev_id) else {
                return Err(gone("the queue's device is gone"));
            };
            let Some(req) = dev.owned.remove(&tag) else {
                return Err(TransportError {
                    what: "commit of a tag the driver did not hand out",
                    errno: EINVAL,
                });
            };
            let data = dev.read_back.remove(&tag);
            dev.fetched.insert(tag);
            if req.id == 0 {
                if let Ctl::Start { scan_sent, .. } = dev.ctl {
                    dev.ctl = Ctl::Start {
                        scan_sent,
                        scan_done: true,
                    };
                }
                return Ok(());
            }
            let is_read = req.desc.op() == UBLK_IO_OP_READ;
            k.complete(req, result, if is_read && result > 0 { data } else { None });
            Ok(())
        })
    }
}

impl Drop for MockQueue {
    fn drop(&mut self) {
        self.kernel.lock().server_exit(self.dev_id, self.gen);
    }
}
