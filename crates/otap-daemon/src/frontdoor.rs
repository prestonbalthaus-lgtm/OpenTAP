//! The daemon's front doors: a channel the control plane feeds (the `Io*`
//! requests, and every host without a block device), and the ublk device
//! of each online volume (on Linux, or over the tests' in-memory driver),
//! behind one composite the engine sees as its single [`FrontDoor`].

use std::collections::{BTreeMap, HashMap, VecDeque};
use std::sync::mpsc::{self, Receiver, Sender};

use otap_cp::{FrontDoor, IoError, ReqId, Request, RequestKind, Status};
use otap_format::BLOCK_SIZE;

pub use crate::device::VolumeDevice;

/// How a channel request ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IoCompletion {
    /// The engine's status.
    pub status: Status,
    /// The write's sequence number, 0 otherwise.
    pub seq: u64,
    /// A read's data, when the read succeeded.
    pub data: Option<Vec<u8>>,
}

struct Submitted {
    kind: RequestKind,
    data: Option<Vec<u8>>,
    reply: Sender<IoCompletion>,
}

struct InFlight {
    data: Option<Vec<u8>>,
    read: Option<Vec<u8>>,
    reply: Sender<IoCompletion>,
}

/// A handle that submits requests into a [`ChannelFrontDoor`] from any
/// thread.
#[derive(Debug, Clone)]
pub struct IoHandle {
    tx: Sender<Submitted>,
}

impl std::fmt::Debug for Submitted {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Submitted")
            .field("kind", &self.kind)
            .finish()
    }
}

impl IoHandle {
    /// Submits `kind` (with a write's `data`); the completion arrives on
    /// the returned receiver once the engine answers.
    pub fn submit(&self, kind: RequestKind, data: Option<Vec<u8>>) -> Receiver<IoCompletion> {
        let (reply, rx) = mpsc::channel();
        let _ = self.tx.send(Submitted { kind, data, reply });
        rx
    }
}

/// The ids of channel requests carry this bit; a ublk request id carries
/// the tag's generation in its upper half, which reaches this bit only
/// after 2^31 completions of one tag.
const CHANNEL_ID_BIT: u64 = 1 << 63;

/// A front door fed through a channel.
pub struct ChannelFrontDoor {
    rx: Receiver<Submitted>,
    handle: IoHandle,
    queue: VecDeque<Request>,
    inflight: HashMap<ReqId, InFlight>,
    next_id: u64,
}

impl std::fmt::Debug for ChannelFrontDoor {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ChannelFrontDoor")
            .field("queued", &self.queue.len())
            .field("inflight", &self.inflight.len())
            .finish()
    }
}

impl Default for ChannelFrontDoor {
    fn default() -> Self {
        Self::new()
    }
}

impl ChannelFrontDoor {
    /// An empty front door.
    #[must_use]
    pub fn new() -> Self {
        let (tx, rx) = mpsc::channel();
        Self {
            rx,
            handle: IoHandle { tx },
            queue: VecDeque::new(),
            inflight: HashMap::new(),
            next_id: 1,
        }
    }

    /// A handle to submit through.
    #[must_use]
    pub fn handle(&self) -> IoHandle {
        self.handle.clone()
    }

    /// Requests handed to the engine and not completed.
    #[must_use]
    pub fn in_engine(&self) -> usize {
        self.inflight.len()
    }

    /// `true` when `id` is one of this front door's.
    #[must_use]
    pub fn owns(&self, id: ReqId) -> bool {
        id.0 & CHANNEL_ID_BIT != 0 && self.inflight.contains_key(&id)
    }

    /// Moves every submitted request into the queue (an empty or closed
    /// channel ends the pull).
    fn pull(&mut self) {
        while let Ok(Submitted { kind, data, reply }) = self.rx.try_recv() {
            let id = ReqId(CHANNEL_ID_BIT | self.next_id);
            self.next_id += 1;
            self.inflight.insert(
                id,
                InFlight {
                    data,
                    read: None,
                    reply,
                },
            );
            self.queue.push_back(Request { id, kind });
        }
    }
}

impl FrontDoor for ChannelFrontDoor {
    fn fetch(&mut self) -> Option<Request> {
        if self.queue.is_empty() {
            self.pull();
        }
        self.queue.pop_front()
    }

    fn copy_in(&mut self, req: &Request, dst: &mut [u8]) -> Result<(), IoError> {
        let entry = self.inflight.get(&req.id).ok_or(IoError::Corrupt)?;
        let data = entry.data.as_ref().ok_or(IoError::Corrupt)?;
        if data.len() != dst.len() {
            return Err(IoError::BadLength { bytes: dst.len() });
        }
        dst.copy_from_slice(data);
        Ok(())
    }

    fn copy_out(&mut self, req: &Request, src: &[u8]) -> Result<(), IoError> {
        let entry = self.inflight.get_mut(&req.id).ok_or(IoError::Corrupt)?;
        entry.read = Some(src.to_vec());
        Ok(())
    }

    fn complete(&mut self, req: Request, status: Status, seq: u64) {
        if let Some(entry) = self.inflight.remove(&req.id) {
            let data = if status == Status::Ok {
                entry.read
            } else {
                None
            };
            let _ = entry.reply.send(IoCompletion { status, seq, data });
        }
    }
}

/// The composite the engine runs on: the channel, and the devices.
pub struct DaemonFrontDoor {
    channel: ChannelFrontDoor,
    devices: BTreeMap<u32, VolumeDevice>,
    /// Which device is polled first next time, so none starves.
    turn: usize,
}

impl std::fmt::Debug for DaemonFrontDoor {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DaemonFrontDoor")
            .field("channel", &self.channel)
            .field("devices", &self.devices.len())
            .finish()
    }
}

impl Default for DaemonFrontDoor {
    fn default() -> Self {
        Self::new()
    }
}

impl DaemonFrontDoor {
    /// A composite with the channel and no devices.
    #[must_use]
    pub fn new() -> Self {
        Self {
            channel: ChannelFrontDoor::new(),
            devices: BTreeMap::new(),
            turn: 0,
        }
    }

    /// A handle into the channel.
    #[must_use]
    pub fn io_handle(&self) -> IoHandle {
        self.channel.handle()
    }

    /// Requests the engine holds, over every door.
    #[must_use]
    pub fn in_engine(&self) -> usize {
        let devices: usize = self.devices.values().map(|d| d.front.in_engine()).sum();
        self.channel.in_engine() + devices
    }

    /// The volumes with a device.
    #[must_use]
    pub fn device_volumes(&self) -> Vec<u32> {
        self.devices.keys().copied().collect()
    }

    /// Adds the device of `vol`.
    pub fn add_device(&mut self, vol: u32, device: VolumeDevice) {
        self.devices.insert(vol, device);
    }

    /// Takes the device of `vol` out, for its teardown.
    pub fn remove_device(&mut self, vol: u32) -> Option<VolumeDevice> {
        self.devices.remove(&vol)
    }

    /// The device of `vol`.
    #[must_use]
    pub fn device(&self, vol: u32) -> Option<&VolumeDevice> {
        self.devices.get(&vol)
    }

    /// The device of `vol`, mutably.
    pub fn device_mut(&mut self, vol: u32) -> Option<&mut VolumeDevice> {
        self.devices.get_mut(&vol)
    }
}

impl FrontDoor for DaemonFrontDoor {
    fn fetch(&mut self) -> Option<Request> {
        let n = self.devices.len();
        if n > 0 {
            let keys: Vec<u32> = self.devices.keys().copied().collect();
            for k in 0..n {
                let vol = keys[(self.turn + k) % n];
                if let Some(dev) = self.devices.get_mut(&vol) {
                    if let Some(req) = dev.front.fetch() {
                        self.turn = (self.turn + k + 1) % n;
                        return Some(req);
                    }
                }
            }
        }
        self.channel.fetch()
    }

    fn copy_in(&mut self, req: &Request, dst: &mut [u8]) -> Result<(), IoError> {
        if self.channel.owns(req.id) {
            return self.channel.copy_in(req, dst);
        }
        if let Some(dev) = self.devices.get_mut(&req.vol()) {
            return dev.front.copy_in(req, dst);
        }
        Err(IoError::Corrupt)
    }

    fn copy_out(&mut self, req: &Request, src: &[u8]) -> Result<(), IoError> {
        if self.channel.owns(req.id) {
            return self.channel.copy_out(req, src);
        }
        if let Some(dev) = self.devices.get_mut(&req.vol()) {
            return dev.front.copy_out(req, src);
        }
        Err(IoError::Corrupt)
    }

    fn complete(&mut self, req: Request, status: Status, seq: u64) {
        if self.channel.owns(req.id) {
            self.channel.complete(req, status, seq);
        } else if let Some(dev) = self.devices.get_mut(&req.vol()) {
            dev.front.complete(req, status, seq);
        }
    }
}

/// Blocks of a byte length, when it is whole blocks and not empty.
#[must_use]
pub fn blocks_of(bytes: usize) -> Option<u32> {
    if bytes == 0 || !bytes.is_multiple_of(BLOCK_SIZE) {
        return None;
    }
    u32::try_from(bytes / BLOCK_SIZE).ok()
}
