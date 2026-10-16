//! The ublk device on Linux: creation, parameters, start, the re-attach
//! after the server's death (Step 6B), the real transport over one queue,
//! the host's control plane ([`LinuxHost`]) and a runner for the engine.

use std::io;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use otap_cp::{Backstore, Clock, Engine, EngineError, IoError};
use otap_format::BLOCK_SIZE;
use otap_sys::ublk::{
    parse_char_name, CtrlBuf, CtrlCommand, UblkChar, UblkControl, UblkControlRing, UblkParamBasic,
    UblkParamDiscard, UblkParams, UblkQueue, UblksrvCtrlCmd, UblksrvCtrlDevInfo, UBLK_ATTR_FUA,
    UBLK_ATTR_ROTATIONAL, UBLK_ATTR_VOLATILE_CACHE, UBLK_CHAR_CLASS_DIR, UBLK_F_CMD_IOCTL_ENCODE,
    UBLK_F_USER_COPY, UBLK_F_USER_RECOVERY, UBLK_F_USER_RECOVERY_REISSUE, UBLK_IO_RES_ABORT,
    UBLK_IO_RES_OK, UBLK_PARAM_TYPE_BASIC, UBLK_PARAM_TYPE_DISCARD,
};
use otap_sys::{FromBytes, IntoBytes, REQUIRED_UBLK_FEATURES};

use crate::frontdoor::{QueueEvent, Transport, TransportError, UblkFrontDoor, Watermarks, ENODEV};
use crate::host::{BoxedTransport, DevInfo, HostDevice, UblkHost};
use crate::start::DeviceControl;

pub use crate::spec::DeviceSpec;

/// The flags every device is created with: the user copy, the recovery
/// with the reissue of a dead server's requests (Step 6B), the ioctl
/// encoding.
const CREATE_FLAGS: u64 = UBLK_F_USER_COPY
    | UBLK_F_USER_RECOVERY
    | UBLK_F_USER_RECOVERY_REISSUE
    | UBLK_F_CMD_IOCTL_ENCODE;

fn transport_error(what: &'static str, e: &io::Error) -> TransportError {
    TransportError {
        what,
        errno: e.raw_os_error().unwrap_or(0),
    }
}

/// A created ublk device and its one queue.
pub struct UblkDevice {
    control: UblkControlRing,
    dev_id: u32,
    info: UblksrvCtrlDevInfo,
    queue: Option<UblkQueue>,
    started: bool,
    /// Re-attached after `START_USER_RECOVERY`: the start is
    /// `END_USER_RECOVERY`, not `START_DEV`.
    recovering: bool,
}

impl UblkDevice {
    /// Creates the device (`ADD_DEV` with `USER_COPY`, `USER_RECOVERY`,
    /// `USER_RECOVERY_REISSUE` and the ioctl encoding, and `ublksrv_flags` =
    /// `stamp`; `SET_PARAMS` with a 4 KiB logical and physical block, a
    /// volatile cache and FUA so the block layer sends flushes and FUA
    /// writes, and the discard limits), opens its character device and
    /// submits a fetch on every tag, on the calling thread, which must be
    /// the one that serves the queue. [`begin_start`](Self::begin_start)
    /// exposes `/dev/ublkbN` afterwards, while that thread serves.
    ///
    /// # Errors
    /// Any control command's error, or a driver that did not grant every
    /// flag of [`REQUIRED_UBLK_FEATURES`] (the device is deleted again).
    pub fn create(spec: &DeviceSpec, stamp: u64) -> io::Result<Self> {
        let mut control = UblkControlRing::new(UblkControl::open()?)?;
        let mut info = UblksrvCtrlDevInfo {
            nr_hw_queues: 1,
            queue_depth: spec.depth,
            max_io_buf_bytes: spec.max_io_bytes,
            dev_id: UblksrvCtrlCmd::ANY_DEV,
            flags: CREATE_FLAGS,
            ublksrv_flags: stamp,
            ..UblksrvCtrlDevInfo::default()
        };
        let mut info_bytes = info.as_bytes().to_vec();
        let buf = CtrlBuf {
            addr: info_bytes.as_ptr().expose_provenance() as u64,
            len: info_bytes.len() as u16,
        };
        control.execute(
            &CtrlCommand::add_dev(UblksrvCtrlCmd::ANY_DEV, buf),
            Some(&mut info_bytes),
        )?;
        info = UblksrvCtrlDevInfo::read_from_bytes(&info_bytes)
            .map_err(|_| io::Error::other("ADD_DEV returned a short device info"))?;
        let dev_id = info.dev_id;
        let mut device = Self {
            control,
            dev_id,
            info,
            queue: None,
            started: false,
            recovering: false,
        };
        // `ADD_DEV` masks the flags the driver does not know and writes the
        // granted set back.
        let granted = info.flags & REQUIRED_UBLK_FEATURES;
        let checked = if granted == REQUIRED_UBLK_FEATURES {
            Ok(())
        } else {
            Err(io::Error::new(
                io::ErrorKind::Unsupported,
                format!(
                    "the ublk driver granted flags {:#x}, without {:#x} of the required {:#x}",
                    info.flags,
                    REQUIRED_UBLK_FEATURES & !granted,
                    REQUIRED_UBLK_FEATURES
                ),
            ))
        };
        if let Err(e) = checked
            .and_then(|()| device.set_params(spec))
            .and_then(|()| device.open_queue(spec.depth))
        {
            let _ = device.delete_inner();
            return Err(e);
        }
        Ok(device)
    }

    /// Re-attaches device `info.dev_id` after a `START_USER_RECOVERY` this
    /// process issued: opens `/dev/ublkcN` (exclusive: the dead server must
    /// have released it, which the recovery's start checked), maps the
    /// queue's descriptors and submits a fetch on every tag, on the calling
    /// thread (the one that serves it). [`begin_start`](Self::begin_start)
    /// then issues `END_USER_RECOVERY`, which waits for those fetches,
    /// records this process's pid and resumes the device: the requests it
    /// held are delivered to this queue.
    ///
    /// # Errors
    /// The open, the mapping or the ring's error.
    pub fn reattach(info: &DevInfo) -> io::Result<Self> {
        let control = UblkControlRing::new(UblkControl::open()?)?;
        let raw = UblksrvCtrlDevInfo {
            nr_hw_queues: info.queues,
            queue_depth: info.depth,
            state: info.state.raw(),
            max_io_buf_bytes: info.max_io_bytes,
            dev_id: info.dev_id,
            ublksrv_pid: info.pid,
            flags: info.flags,
            ublksrv_flags: info.stamp,
            ..UblksrvCtrlDevInfo::default()
        };
        let mut device = Self {
            control,
            dev_id: info.dev_id,
            info: raw,
            queue: None,
            // The block device exists: a teardown stops it before deleting.
            started: true,
            recovering: true,
        };
        device.open_queue(info.depth)?;
        Ok(device)
    }

    fn set_params(&mut self, spec: &DeviceSpec) -> io::Result<()> {
        let mut params = UblkParams::new();
        params.types = UBLK_PARAM_TYPE_BASIC;
        let mut attrs = UBLK_ATTR_VOLATILE_CACHE | UBLK_ATTR_FUA;
        if spec.rotational {
            attrs |= UBLK_ATTR_ROTATIONAL;
        }
        params.basic = UblkParamBasic {
            attrs,
            logical_bs_shift: 12,
            physical_bs_shift: 12,
            io_opt_shift: spec.io_opt_bytes.max(BLOCK_SIZE as u32).trailing_zeros() as u8,
            io_min_shift: 12,
            max_sectors: spec.max_io_bytes / 512,
            chunk_sectors: 0,
            dev_sectors: spec.size_blocks * (BLOCK_SIZE / 512) as u64,
            virt_boundary_mask: 0,
        };
        if let Some(max) = spec.max_discard_bytes {
            params.types |= UBLK_PARAM_TYPE_DISCARD;
            params.discard = UblkParamDiscard {
                discard_alignment: 0,
                discard_granularity: BLOCK_SIZE as u32,
                max_discard_sectors: max / 512,
                max_write_zeroes_sectors: max / 512,
                max_discard_segments: 1,
                reserved0: 0,
            };
        }
        let mut bytes = params.as_bytes().to_vec();
        let buf = CtrlBuf {
            addr: bytes.as_ptr().expose_provenance() as u64,
            len: bytes.len() as u16,
        };
        self.control
            .execute(&CtrlCommand::set_params(self.dev_id, buf), Some(&mut bytes))?;
        Ok(())
    }

    fn open_queue(&mut self, depth: u16) -> io::Result<()> {
        let chr = UblkChar::open(self.dev_id)?;
        let mut queue = UblkQueue::new(chr, 0, depth, u32::from(depth) * 2)?;
        for tag in 0..depth {
            queue
                .push_fetch(tag)
                .map_err(|_| io::Error::other("the queue ring is too small for its fetches"))?;
        }
        queue.submit()?;
        self.queue = Some(queue);
        Ok(())
    }

    /// Brings the device live and returns at once: `START_DEV` for a
    /// created device (Step 6A), `END_USER_RECOVERY` for a re-attached one
    /// (Step 6B), each naming this process's pid. The driver completes
    /// `START_DEV` only after every tag has a processed fetch, which
    /// [`create`](Self::create) submitted, and after `add_disk`, whose
    /// partition scan reads the new disk through this device's queue; it
    /// completes `END_USER_RECOVERY` once the fetches
    /// [`reattach`](Self::reattach) submitted are processed, and the
    /// requests it then releases arrive on the queue. So the queue must be
    /// served, on the thread that submitted its fetches, while the command
    /// is in flight, and the command is polled with
    /// [`poll_start`](Self::poll_start) between turns
    /// ([`crate::start::start_while_serving`]). Waiting for `START_DEV` on
    /// that thread instead never returns for a privileged daemon.
    ///
    /// # Errors
    /// The command could not be submitted.
    pub fn begin_start(&mut self) -> io::Result<()> {
        let pid = std::process::id();
        let cmd = if self.recovering {
            CtrlCommand::end_user_recovery(self.dev_id, pid)
        } else {
            CtrlCommand::start_dev(self.dev_id, pid)
        };
        self.control.begin(&cmd)
    }

    /// `true` once the command [`begin_start`](Self::begin_start) issued
    /// has completed and the device is live; `false` while it runs.
    ///
    /// # Errors
    /// The command's error, or none in flight.
    pub fn poll_start(&mut self) -> io::Result<bool> {
        if self.started && !self.recovering {
            return Ok(true);
        }
        match self.control.poll()? {
            Some(_) => {
                self.started = true;
                self.recovering = false;
                Ok(true)
            }
            None => Ok(false),
        }
    }

    /// `true` while a `START_DEV` or `END_USER_RECOVERY` is in flight.
    #[must_use]
    pub const fn start_in_flight(&self) -> bool {
        self.control.in_flight()
    }

    /// `STOP_DEV`.
    ///
    /// # Errors
    /// The command's error.
    pub fn stop(&mut self) -> io::Result<()> {
        self.control
            .execute(&CtrlCommand::stop_dev(self.dev_id), None)?;
        self.started = false;
        Ok(())
    }

    fn delete_inner(&mut self) -> io::Result<()> {
        self.queue = None;
        self.control
            .execute(&CtrlCommand::del_dev(self.dev_id), None)?;
        Ok(())
    }

    /// Stops (if started) and deletes the device. The transport taken from
    /// it ([`take_transport`](Self::take_transport)), which holds
    /// `/dev/ublkcN` open, must be dropped first: `DEL_DEV` waits until the
    /// character device is released.
    ///
    /// # Errors
    /// The command's error.
    pub fn delete(mut self) -> io::Result<()> {
        if self.started {
            self.stop()?;
        }
        self.delete_inner()
    }

    /// The device id `N` of `/dev/ublkbN`.
    #[must_use]
    pub const fn dev_id(&self) -> u32 {
        self.dev_id
    }

    /// The device as the driver described it after `ADD_DEV` (or as
    /// `GET_DEV_INFO` did before a re-attach).
    #[must_use]
    pub const fn info(&self) -> &UblksrvCtrlDevInfo {
        &self.info
    }

    /// The block device's path.
    #[must_use]
    pub fn block_path(&self) -> String {
        format!("/dev/ublkb{}", self.dev_id)
    }

    /// Takes the queue as a transport (once); the runner's thread must be
    /// the one that created the device, because the driver pins the queue
    /// to the task that issued its last fetch.
    pub fn take_transport(&mut self) -> Option<LinuxTransport> {
        self.queue.take().map(|queue| LinuxTransport {
            queue,
            aborted: Vec::new(),
        })
    }
}

impl DeviceControl for UblkDevice {
    fn begin_start(&mut self) -> Result<(), TransportError> {
        let what = if self.recovering {
            "END_USER_RECOVERY"
        } else {
            "START_DEV"
        };
        Self::begin_start(self).map_err(|e| transport_error(what, &e))
    }

    fn poll_start(&mut self) -> Result<bool, TransportError> {
        let what = if self.recovering {
            "END_USER_RECOVERY"
        } else {
            "START_DEV"
        };
        Self::poll_start(self).map_err(|e| transport_error(what, &e))
    }
}

impl HostDevice for UblkDevice {
    fn dev_id(&self) -> u32 {
        self.dev_id
    }

    fn depth(&self) -> u16 {
        self.info.queue_depth
    }

    fn take_transport(&mut self) -> Option<BoxedTransport> {
        Self::take_transport(self).map(|t| Box::new(t) as BoxedTransport)
    }

    fn start_in_flight(&self) -> bool {
        Self::start_in_flight(self)
    }

    fn delete(self: Box<Self>) -> Result<(), TransportError> {
        (*self)
            .delete()
            .map_err(|e| transport_error("STOP_DEV or DEL_DEV", &e))
    }
}

impl std::fmt::Debug for UblkDevice {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("UblkDevice")
            .field("dev_id", &self.dev_id)
            .field("started", &self.started)
            .field("recovering", &self.recovering)
            .finish()
    }
}

/// The host's ublk control plane over `/dev/ublk-control`: the devices of
/// the `ublk-char` class, `GET_DEV_INFO` and `GET_PARAMS` of each, the
/// creation, `START_USER_RECOVERY` and the re-attach, and the deletion of a
/// device this process does not serve. The control device is opened at the
/// first command, so a host with no ublk driver fails only when asked.
#[derive(Debug, Default)]
pub struct LinuxHost {
    control: Option<UblkControlRing>,
}

impl LinuxHost {
    /// A control plane not yet opened.
    #[must_use]
    pub const fn new() -> Self {
        Self { control: None }
    }

    fn ring(&mut self) -> io::Result<&mut UblkControlRing> {
        if self.control.is_none() {
            self.control = Some(UblkControlRing::new(UblkControl::open()?)?);
        }
        self.control
            .as_mut()
            .ok_or_else(|| io::Error::other("the control ring is not open"))
    }

    fn get_dev_info(&mut self, dev_id: u32) -> io::Result<Option<UblksrvCtrlDevInfo>> {
        let mut bytes = UblksrvCtrlDevInfo::default().as_bytes().to_vec();
        let buf = CtrlBuf {
            addr: bytes.as_ptr().expose_provenance() as u64,
            len: bytes.len() as u16,
        };
        match self
            .ring()?
            .execute(&CtrlCommand::get_dev_info(dev_id, buf), Some(&mut bytes))
        {
            Ok(_) => {}
            Err(e) if e.raw_os_error() == Some(ENODEV) => return Ok(None),
            Err(e) => return Err(e),
        }
        UblksrvCtrlDevInfo::read_from_bytes(&bytes)
            .map(Some)
            .map_err(|_| io::Error::other("GET_DEV_INFO returned a short device info"))
    }

    /// The capacity `GET_PARAMS` reports, in 512-byte sectors, when the
    /// basic parameters were set.
    fn get_sectors(&mut self, dev_id: u32) -> Option<u64> {
        let mut bytes = UblkParams::new().as_bytes().to_vec();
        let buf = CtrlBuf {
            addr: bytes.as_ptr().expose_provenance() as u64,
            len: bytes.len() as u16,
        };
        let ring = self.ring().ok()?;
        ring.execute(&CtrlCommand::get_params(dev_id, buf), Some(&mut bytes))
            .ok()?;
        let params = UblkParams::read_from_bytes(&bytes).ok()?;
        (params.types & UBLK_PARAM_TYPE_BASIC != 0).then_some(params.basic.dev_sectors)
    }

    fn describe(&mut self, dev_id: u32) -> io::Result<Option<DevInfo>> {
        let Some(raw) = self.get_dev_info(dev_id)? else {
            return Ok(None);
        };
        let sectors = self.get_sectors(dev_id);
        Ok(Some(DevInfo::from_raw(&raw, sectors)))
    }
}

/// Whether `/proc/<pid>/stat` names a process that has not exited: a
/// zombie (`Z`) or a dead task (`X`) has released its files already.
fn proc_alive(pid: i32) -> bool {
    if pid <= 0 {
        return false;
    }
    let Ok(stat) = std::fs::read_to_string(format!("/proc/{pid}/stat")) else {
        return false;
    };
    // `pid (comm) state ...`: the command may hold spaces and parentheses,
    // so the state follows the last `)`.
    let state = stat
        .rfind(')')
        .and_then(|at| stat[at + 1..].split_whitespace().next());
    !matches!(state, Some("Z" | "X" | "x") | None)
}

impl UblkHost for LinuxHost {
    fn pid(&self) -> u32 {
        std::process::id()
    }

    fn pid_alive(&mut self, pid: i32) -> bool {
        proc_alive(pid)
    }

    fn list(&mut self) -> Result<Vec<DevInfo>, TransportError> {
        let entries = match std::fs::read_dir(UBLK_CHAR_CLASS_DIR) {
            Ok(entries) => entries,
            // No driver loaded: no devices.
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(e) => return Err(transport_error("reading the ublk-char class", &e)),
        };
        let mut ids: Vec<u32> = entries
            .filter_map(Result::ok)
            .filter_map(|e| e.file_name().to_str().and_then(parse_char_name))
            .collect();
        ids.sort_unstable();
        let mut out = Vec::with_capacity(ids.len());
        for id in ids {
            // A device deleted between the listing and the command is gone.
            if let Some(info) = self
                .describe(id)
                .map_err(|e| transport_error("GET_DEV_INFO", &e))?
            {
                out.push(info);
            }
        }
        Ok(out)
    }

    fn info(&mut self, dev_id: u32) -> Result<Option<DevInfo>, TransportError> {
        self.describe(dev_id)
            .map_err(|e| transport_error("GET_DEV_INFO", &e))
    }

    fn create(
        &mut self,
        spec: &DeviceSpec,
        stamp: u64,
    ) -> Result<Box<dyn HostDevice>, TransportError> {
        UblkDevice::create(spec, stamp)
            .map(|d| Box::new(d) as Box<dyn HostDevice>)
            .map_err(|e| transport_error("creating the ublk device", &e))
    }

    fn start_recovery(&mut self, dev_id: u32) -> Result<(), TransportError> {
        self.ring()
            .and_then(|r| r.execute(&CtrlCommand::start_user_recovery(dev_id), None))
            .map(|_| ())
            .map_err(|e| transport_error("START_USER_RECOVERY", &e))
    }

    fn reattach(&mut self, info: &DevInfo) -> Result<Box<dyn HostDevice>, TransportError> {
        UblkDevice::reattach(info)
            .map(|d| Box::new(d) as Box<dyn HostDevice>)
            .map_err(|e| transport_error("re-opening the ublk queue", &e))
    }

    fn reap(&mut self, dev_id: u32) -> Result<(), TransportError> {
        let ring = self
            .ring()
            .map_err(|e| transport_error("opening /dev/ublk-control", &e))?;
        ring.execute(&CtrlCommand::stop_dev(dev_id), None)
            .map_err(|e| transport_error("STOP_DEV", &e))?;
        ring.execute(&CtrlCommand::del_dev_async(dev_id), None)
            .map_err(|e| transport_error("DEL_DEV_ASYNC", &e))?;
        Ok(())
    }
}

/// The real queue.
#[derive(Debug)]
pub struct LinuxTransport {
    queue: UblkQueue,
    aborted: Vec<u16>,
}

impl LinuxTransport {
    /// The queue.
    #[must_use]
    pub const fn queue(&self) -> &UblkQueue {
        &self.queue
    }

    /// Submits pending commands and waits for at least one completion.
    ///
    /// # Errors
    /// The ring's error.
    pub fn wait(&mut self) -> io::Result<usize> {
        self.queue.submit_and_wait()
    }
}

impl Transport for LinuxTransport {
    fn poll(&mut self, out: &mut Vec<QueueEvent>) -> Result<usize, TransportError> {
        self.queue
            .submit()
            .map_err(|e| transport_error("submit", &e))?;
        let mut reaped = Vec::new();
        self.queue.reap(&mut reaped);
        let mut n = 0;
        for (tag, result) in reaped {
            if result == UBLK_IO_RES_OK {
                if let Some(desc) = self.queue.desc(tag) {
                    out.push(QueueEvent::Request { tag, desc });
                    n += 1;
                }
            } else {
                // UBLK_IO_RES_ABORT, or any error: the tag is done.
                let _ = UBLK_IO_RES_ABORT;
                self.aborted.push(tag);
                out.push(QueueEvent::Abort { tag });
                n += 1;
            }
        }
        Ok(n)
    }

    fn read_data(&mut self, tag: u16, off: u32, dst: &mut [u8]) -> Result<(), IoError> {
        self.queue
            .read_data(tag, off, dst)
            .map_err(|e| IoError::Device {
                errno: e.raw_os_error().unwrap_or(0),
            })
    }

    fn write_data(&mut self, tag: u16, off: u32, src: &[u8]) -> Result<(), IoError> {
        self.queue
            .write_data(tag, off, src)
            .map_err(|e| IoError::Device {
                errno: e.raw_os_error().unwrap_or(0),
            })
    }

    fn commit(&mut self, tag: u16, result: i32) -> Result<(), TransportError> {
        if self.queue.push_commit(tag, result).is_err() {
            // The ring is full of unsubmitted commits: submit and retry
            // once.
            self.queue
                .submit()
                .map_err(|e| transport_error("submit", &e))?;
            self.queue
                .push_commit(tag, result)
                .map_err(|_| TransportError {
                    what: "queue ring full",
                    errno: 11,
                })?;
        }
        Ok(())
    }
}

/// Builds the front door of a started device for volume `vol`.
pub fn front_door(
    device: &mut UblkDevice,
    vol: u32,
    spec: &DeviceSpec,
) -> io::Result<UblkFrontDoor<LinuxTransport>> {
    let transport = device
        .take_transport()
        .ok_or_else(|| io::Error::other("the device's queue was already taken"))?;
    Ok(UblkFrontDoor::new(
        transport,
        0,
        vol,
        spec.size_blocks,
        spec.depth,
        Watermarks::for_depth(spec.depth),
    ))
}

/// Runs the engine's tick loop until `stop` is set or the engine stops:
/// one tick, then, when the tick served nothing, a short sleep so the CP
/// timer still fires (the busy-polled reactor is a later step).
///
/// # Errors
/// The engine's internal failure.
pub fn serve<B: Backstore, C: Clock>(
    engine: &mut Engine<B, UblkFrontDoor<LinuxTransport>, C>,
    stop: &AtomicBool,
) -> Result<(), EngineError> {
    while !stop.load(Ordering::Relaxed) && !engine.is_stopped() {
        let before = engine.stats();
        engine.tick()?;
        let after = engine.stats();
        let idle = before == after && engine.front_door().in_engine() == 0;
        if idle {
            std::thread::sleep(Duration::from_micros(200));
        }
    }
    Ok(())
}
