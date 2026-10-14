//! The ublk device on Linux: creation, parameters, start, the real
//! transport over one queue, and a runner for the engine.

use std::io;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use otap_cp::{Backstore, Clock, Engine, EngineError, IoError};
use otap_format::BLOCK_SIZE;
use otap_sys::ublk::{
    CtrlBuf, CtrlCommand, UblkChar, UblkControl, UblkControlRing, UblkParamBasic, UblkParamDiscard,
    UblkParams, UblkQueue, UblksrvCtrlCmd, UblksrvCtrlDevInfo, UBLK_ATTR_FUA, UBLK_ATTR_ROTATIONAL,
    UBLK_ATTR_VOLATILE_CACHE, UBLK_F_CMD_IOCTL_ENCODE, UBLK_F_USER_COPY, UBLK_F_USER_RECOVERY,
    UBLK_IO_RES_ABORT, UBLK_IO_RES_OK, UBLK_PARAM_TYPE_BASIC, UBLK_PARAM_TYPE_DISCARD,
};
use otap_sys::{FromBytes, IntoBytes};

use crate::frontdoor::{QueueEvent, Transport, TransportError, UblkFrontDoor, Watermarks};

/// How the device is shaped.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DeviceSpec {
    /// The volume's size in 4 KiB blocks.
    pub size_blocks: u64,
    /// Tags per queue (one queue), at most 4096.
    pub depth: u16,
    /// The largest request in bytes (8 MiB, the engine's request bound,
    /// or less when the arena is small: [`DeviceSpec::for_arena`]).
    pub max_io_bytes: u32,
    /// What `optimal_io_size` is derived from: the full stripe. The driver
    /// takes a power of two (a shift), so the device reports the largest
    /// power of two dividing the stripe (the stripe itself when it is one).
    pub io_opt_bytes: u32,
    /// Report discard support, with this bound per request.
    pub max_discard_bytes: Option<u32>,
    /// Report rotational media.
    pub rotational: bool,
}

impl DeviceSpec {
    /// A device for `size_blocks` blocks over a `stripe_bytes` geometry:
    /// 128 tags, 8 MiB requests, discards up to 1 GiB.
    #[must_use]
    pub const fn new(size_blocks: u64, stripe_bytes: u32, rotational: bool) -> Self {
        Self {
            size_blocks,
            depth: 128,
            max_io_bytes: 8 << 20,
            io_opt_bytes: stripe_bytes,
            max_discard_bytes: Some(1 << 30),
            rotational,
        }
    }

    /// Bounds the largest request by the arena: the engine refuses a write
    /// longer than the arena's slots (`TooLarge`, `EINVAL` on the device),
    /// so a device over a small arena must not advertise 8 MiB requests.
    /// Half the arena, rounded down to whole 4 KiB blocks, at least one.
    #[must_use]
    pub fn for_arena(mut self, arena_slots: u32) -> Self {
        let cap = (arena_slots / 2).max(1).saturating_mul(BLOCK_SIZE as u32);
        if cap < self.max_io_bytes {
            self.max_io_bytes = cap;
        }
        self
    }
}

/// A created ublk device and its one queue.
pub struct UblkDevice {
    control: UblkControlRing,
    dev_id: u32,
    info: UblksrvCtrlDevInfo,
    queue: Option<UblkQueue>,
    started: bool,
}

impl UblkDevice {
    /// Creates the device (`ADD_DEV` with `USER_COPY`, `USER_RECOVERY` and
    /// the ioctl encoding; `SET_PARAMS` with a 4 KiB logical and physical
    /// block, a volatile cache and FUA so the block layer sends flushes and
    /// FUA writes, and the discard limits), opens its character device and
    /// pushes a fetch on every tag. [`start`](Self::start) exposes
    /// `/dev/ublkbN` afterwards.
    ///
    /// # Errors
    /// Any control command's error.
    pub fn create(spec: &DeviceSpec) -> io::Result<Self> {
        let mut control = UblkControlRing::new(UblkControl::open()?)?;
        let mut info = UblksrvCtrlDevInfo {
            nr_hw_queues: 1,
            queue_depth: spec.depth,
            max_io_buf_bytes: spec.max_io_bytes,
            dev_id: UblksrvCtrlCmd::ANY_DEV,
            flags: UBLK_F_USER_COPY | UBLK_F_USER_RECOVERY | UBLK_F_CMD_IOCTL_ENCODE,
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
        };
        if let Err(e) = device
            .set_params(spec)
            .and_then(|()| device.open_queue(spec))
        {
            let _ = device.delete_inner();
            return Err(e);
        }
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

    fn open_queue(&mut self, spec: &DeviceSpec) -> io::Result<()> {
        let chr = UblkChar::open(self.dev_id)?;
        let mut queue = UblkQueue::new(chr, 0, spec.depth, u32::from(spec.depth) * 2)?;
        for tag in 0..spec.depth {
            queue
                .push_fetch(tag)
                .map_err(|_| io::Error::other("the queue ring is too small for its fetches"))?;
        }
        queue.submit()?;
        self.queue = Some(queue);
        Ok(())
    }

    /// `START_DEV`: exposes `/dev/ublkbN`. Blocks until the driver sees a
    /// fetch pending on every tag, which [`create`](Self::create) issued.
    ///
    /// # Errors
    /// The command's error.
    pub fn start(&mut self) -> io::Result<()> {
        let pid = std::process::id();
        self.control
            .execute(&CtrlCommand::start_dev(self.dev_id, pid), None)?;
        self.started = true;
        Ok(())
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

    /// Stops (if started) and deletes the device.
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

    /// The device as the driver described it after `ADD_DEV`.
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
    /// to the task that issued its first fetch.
    pub fn take_transport(&mut self) -> Option<LinuxTransport> {
        self.queue.take().map(|queue| LinuxTransport {
            queue,
            aborted: Vec::new(),
        })
    }
}

impl std::fmt::Debug for UblkDevice {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("UblkDevice")
            .field("dev_id", &self.dev_id)
            .field("started", &self.started)
            .finish()
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
        self.queue.submit().map_err(|e| TransportError {
            what: "submit",
            errno: e.raw_os_error().unwrap_or(0),
        })?;
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
            self.queue.submit().map_err(|e| TransportError {
                what: "submit",
                errno: e.raw_os_error().unwrap_or(0),
            })?;
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
