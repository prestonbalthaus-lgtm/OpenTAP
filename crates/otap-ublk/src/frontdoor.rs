//! The front door state machine.

use core::fmt;
use std::collections::VecDeque;

use otap_cp::{FrontDoor, IoError, ReqId, Request, RequestKind, Status};
use otap_format::BLOCK_SIZE;
use otap_sys::ublk::{
    UblksrvIoDesc, UBLK_IO_OP_DISCARD, UBLK_IO_OP_FLUSH, UBLK_IO_OP_READ, UBLK_IO_OP_WRITE,
    UBLK_IO_OP_WRITE_ZEROES,
};

/// `EIO`.
pub const EIO: i32 = 5;
/// `ENODEV`.
pub const ENODEV: i32 = 19;
/// `EINVAL`.
pub const EINVAL: i32 = 22;
/// `ENOSPC`.
pub const ENOSPC: i32 = 28;
/// `EOPNOTSUPP`.
pub const EOPNOTSUPP: i32 = 95;

/// Sectors per 4 KiB block.
const SECTORS: u64 = (BLOCK_SIZE / 512) as u64;

/// Something the queue delivered.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QueueEvent {
    /// A request waits on `tag`, described by `desc`.
    Request {
        /// The tag.
        tag: u16,
        /// The driver's description.
        desc: UblksrvIoDesc,
    },
    /// The driver aborted `tag`: it must not be fetched again.
    Abort {
        /// The tag.
        tag: u16,
    },
}

/// The queue failed: the device is gone or the ring refused.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TransportError {
    /// What failed.
    pub what: &'static str,
    /// The errno, or 0.
    pub errno: i32,
}

impl fmt::Display for TransportError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} (errno {})", self.what, self.errno)
    }
}

impl std::error::Error for TransportError {}

/// The queue as the state machine sees it.
pub trait Transport {
    /// Moves every arrived event into `out`; returns how many.
    fn poll(&mut self, out: &mut Vec<QueueEvent>) -> Result<usize, TransportError>;

    /// Copies `dst.len()` bytes of a write request's data, from byte `off`
    /// of request `tag`, out of the driver.
    fn read_data(&mut self, tag: u16, off: u32, dst: &mut [u8]) -> Result<(), IoError>;

    /// Copies `src` into a read request's data at byte `off` of `tag`.
    fn write_data(&mut self, tag: u16, off: u32, src: &[u8]) -> Result<(), IoError>;

    /// Completes `tag` with `result` (bytes, or a negative errno) and
    /// fetches the tag's next request.
    fn commit(&mut self, tag: u16, result: i32) -> Result<(), TransportError>;
}

/// Back-pressure: how many requests the engine holds at once.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Watermarks {
    /// Requests in the engine at which fetching stops.
    pub high: usize,
    /// Requests in the engine below which fetching resumes.
    pub low: usize,
}

impl Watermarks {
    /// Stop at `depth`, resume at half of it.
    #[must_use]
    pub const fn for_depth(depth: u16) -> Self {
        let depth = depth as usize;
        Self {
            high: if depth == 0 { 1 } else { depth },
            low: depth / 2,
        }
    }
}

/// Counters.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct FrontDoorStats {
    /// Requests the queue delivered.
    pub requests: u64,
    /// Reads handed to the engine.
    pub reads: u64,
    /// Writes handed to the engine.
    pub writes: u64,
    /// Flushes handed to the engine.
    pub flushes: u64,
    /// Discards handed to the engine.
    pub discards: u64,
    /// Writes of zeros handed to the engine.
    pub write_zeroes: u64,
    /// Requests answered `EOPNOTSUPP` or `EINVAL` without the engine.
    pub refused: u64,
    /// Requests answered `ENODEV` while offline.
    pub offline_rejected: u64,
    /// Completions dropped because the tag had moved on.
    pub stale: u64,
    /// Tags the driver aborted.
    pub aborted: u64,
    /// Completions the engine answered with an error status.
    pub errors: u64,
    /// Times the high watermark closed the gate.
    pub gate_closed: u64,
    /// Commits the transport refused.
    pub commit_failures: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TagState {
    /// Fetched by the driver, no request.
    Idle,
    /// A request decoded and waiting for the engine.
    Ready,
    /// Handed to the engine.
    InEngine,
    /// The driver aborted the tag; nothing is fetched on it again.
    Aborted,
}

#[derive(Debug, Clone, Copy)]
struct TagSlot {
    state: TagState,
    kind: Option<RequestKind>,
    gen: u32,
}

/// Decodes a driver descriptor for volume `vol` of `size_blocks` into the
/// engine's request kind, or the errno to answer at once: `EOPNOTSUPP` for
/// an operation the engine does not serve, `EINVAL` for one that is not
/// whole 4 KiB blocks or runs past the volume.
pub fn decode_request(
    desc: &UblksrvIoDesc,
    vol: u32,
    size_blocks: u64,
) -> Result<RequestKind, i32> {
    let op = desc.op();
    let sectors = u64::from(desc.nr_sectors());
    let start = desc.start_sector;
    if op == UBLK_IO_OP_FLUSH {
        return Ok(RequestKind::Flush { vol });
    }
    if !matches!(
        op,
        UBLK_IO_OP_READ | UBLK_IO_OP_WRITE | UBLK_IO_OP_DISCARD | UBLK_IO_OP_WRITE_ZEROES
    ) {
        return Err(EOPNOTSUPP);
    }
    if sectors == 0 || !sectors.is_multiple_of(SECTORS) || !start.is_multiple_of(SECTORS) {
        return Err(EINVAL);
    }
    let lba = start / SECTORS;
    let blocks = u32::try_from(sectors / SECTORS).map_err(|_| EINVAL)?;
    if lba
        .checked_add(u64::from(blocks))
        .is_none_or(|e| e > size_blocks)
    {
        return Err(EINVAL);
    }
    Ok(match op {
        UBLK_IO_OP_READ => RequestKind::Read { vol, lba, blocks },
        UBLK_IO_OP_WRITE => RequestKind::Write { vol, lba, blocks },
        UBLK_IO_OP_DISCARD => RequestKind::Discard { vol, lba, blocks },
        _ => RequestKind::WriteZeroes { vol, lba, blocks },
    })
}

/// The ublk result of an engine status: the bytes a successful read or
/// write moved (0 for a flush or a punch), or a negative errno.
#[must_use]
pub fn errno_for(status: Status, kind: &RequestKind) -> i32 {
    match status {
        Status::Ok => match kind {
            RequestKind::Read { blocks, .. } | RequestKind::Write { blocks, .. } => {
                i32::try_from(u64::from(*blocks) * BLOCK_SIZE as u64).unwrap_or(i32::MAX)
            }
            _ => 0,
        },
        Status::NvFailed | Status::Corrupt | Status::Failed => -EIO,
        Status::NoSuchVolume | Status::Stopped | Status::Offline => -ENODEV,
        Status::NoSpace => -ENOSPC,
        Status::OutOfRange | Status::TooLarge => -EINVAL,
        _ => -EIO,
    }
}

/// The request id of `tag` on queue `q_id` in generation `gen`:
/// `gen << 32 | q_id << 16 | tag`. The generation advances every time the
/// tag completes or is aborted, so a completion for an earlier use of the
/// tag is told apart from the current one.
#[must_use]
pub const fn request_id(q_id: u16, tag: u16, gen: u32) -> ReqId {
    ReqId((gen as u64) << 32 | (q_id as u64) << 16 | tag as u64)
}

/// The queue, tag and generation of a request id.
#[must_use]
pub const fn split_id(id: ReqId) -> (u16, u16, u32) {
    ((id.0 >> 16) as u16, id.0 as u16, (id.0 >> 32) as u32)
}

/// One queue of one device, serving one volume, as the engine's front door.
pub struct UblkFrontDoor<T: Transport> {
    transport: T,
    q_id: u16,
    vol: u32,
    size_blocks: u64,
    tags: Vec<TagSlot>,
    ready: VecDeque<u16>,
    events: Vec<QueueEvent>,
    in_engine: usize,
    gate_closed: bool,
    offline: bool,
    watermarks: Watermarks,
    stats: FrontDoorStats,
    last_error: Option<TransportError>,
}

impl<T: Transport> UblkFrontDoor<T> {
    /// A front door for queue `q_id` of a device exposing volume `vol` of
    /// `size_blocks` blocks, `depth` tags deep, under `watermarks`. A high
    /// watermark of 0 would let nothing through and answer nothing: it is
    /// taken as 1, and a low watermark above the high one as the high one
    /// (as [`Watermarks::for_depth`] builds them).
    #[must_use]
    pub fn new(
        transport: T,
        q_id: u16,
        vol: u32,
        size_blocks: u64,
        depth: u16,
        watermarks: Watermarks,
    ) -> Self {
        let high = watermarks.high.max(1);
        let watermarks = Watermarks {
            high,
            low: watermarks.low.min(high),
        };
        Self {
            transport,
            q_id,
            vol,
            size_blocks,
            tags: vec![
                TagSlot {
                    state: TagState::Idle,
                    kind: None,
                    gen: 0,
                };
                usize::from(depth)
            ],
            ready: VecDeque::new(),
            events: Vec::new(),
            in_engine: 0,
            gate_closed: false,
            offline: false,
            watermarks,
            stats: FrontDoorStats::default(),
            last_error: None,
        }
    }

    /// Takes the device offline (or back): while offline every request is
    /// answered `ENODEV` without reaching the engine, including the ones
    /// already decoded and waiting for it; only requests the engine holds
    /// finish normally.
    pub fn set_offline(&mut self, offline: bool) {
        self.offline = offline;
        if offline {
            while let Some(tag) = self.ready.pop_front() {
                if let Some(slot) = self.tags.get_mut(usize::from(tag)) {
                    if slot.state == TagState::Ready {
                        slot.state = TagState::Idle;
                        slot.kind = None;
                        self.answer(tag, -ENODEV);
                        self.stats.offline_rejected += 1;
                    }
                }
            }
        }
    }

    /// `true` while offline.
    #[must_use]
    pub const fn is_offline(&self) -> bool {
        self.offline
    }

    /// Requests handed to the engine and not completed.
    #[must_use]
    pub const fn in_engine(&self) -> usize {
        self.in_engine
    }

    /// Requests decoded and waiting for the engine.
    #[must_use]
    pub fn ready(&self) -> usize {
        self.ready.len()
    }

    /// Counters.
    #[must_use]
    pub const fn stats(&self) -> FrontDoorStats {
        self.stats
    }

    /// The watermarks.
    #[must_use]
    pub const fn watermarks(&self) -> Watermarks {
        self.watermarks
    }

    /// The transport.
    pub fn transport(&mut self) -> &mut T {
        &mut self.transport
    }

    /// The last transport failure, if any.
    #[must_use]
    pub fn last_error(&self) -> Option<&TransportError> {
        self.last_error.as_ref()
    }

    /// Tags the driver aborted.
    #[must_use]
    pub fn aborted_tags(&self) -> Vec<u16> {
        self.tags
            .iter()
            .enumerate()
            .filter(|(_, t)| t.state == TagState::Aborted)
            .map(|(i, _)| i as u16)
            .collect()
    }

    /// Polls the transport and decodes what arrived: a request becomes
    /// ready for the engine, or is answered at once when it cannot be
    /// served (unsupported, malformed, offline).
    pub fn pump(&mut self) -> Result<usize, TransportError> {
        self.events.clear();
        let mut events = std::mem::take(&mut self.events);
        let polled = self.transport.poll(&mut events);
        let n = match polled {
            Ok(n) => n,
            Err(e) => {
                self.last_error = Some(e.clone());
                self.events = events;
                return Err(e);
            }
        };
        for event in events.drain(..) {
            match event {
                QueueEvent::Abort { tag } => {
                    if let Some(slot) = self.tags.get_mut(usize::from(tag)) {
                        if slot.state == TagState::Ready {
                            self.ready.retain(|t| *t != tag);
                        }
                        if slot.state == TagState::InEngine {
                            // The engine still holds it; its completion is
                            // dropped as stale.
                            slot.gen = slot.gen.wrapping_add(1);
                            self.in_engine = self.in_engine.saturating_sub(1);
                        }
                        slot.state = TagState::Aborted;
                        slot.kind = None;
                        self.stats.aborted += 1;
                    }
                }
                QueueEvent::Request { tag, desc } => {
                    self.stats.requests += 1;
                    let Some(slot) = self.tags.get(usize::from(tag)).copied() else {
                        continue;
                    };
                    if slot.state != TagState::Idle {
                        // A request on a tag that is busy: the driver never
                        // does this; answer it rather than lose it.
                        self.answer(tag, -EINVAL);
                        self.stats.refused += 1;
                        continue;
                    }
                    if self.offline {
                        self.answer(tag, -ENODEV);
                        self.stats.offline_rejected += 1;
                        continue;
                    }
                    match decode_request(&desc, self.vol, self.size_blocks) {
                        Ok(kind) => {
                            let slot = &mut self.tags[usize::from(tag)];
                            slot.state = TagState::Ready;
                            slot.kind = Some(kind);
                            self.ready.push_back(tag);
                        }
                        Err(errno) => {
                            self.answer(tag, -errno);
                            self.stats.refused += 1;
                        }
                    }
                }
            }
        }
        self.events = events;
        Ok(n)
    }

    /// Completes `tag` on the transport without the engine.
    fn answer(&mut self, tag: u16, result: i32) {
        if let Err(e) = self.transport.commit(tag, result) {
            self.stats.commit_failures += 1;
            self.last_error = Some(e);
            if let Some(slot) = self.tags.get_mut(usize::from(tag)) {
                slot.state = TagState::Aborted;
            }
        }
    }
}

impl<T: Transport> FrontDoor for UblkFrontDoor<T> {
    fn fetch(&mut self) -> Option<Request> {
        if self.ready.is_empty() {
            let _ = self.pump();
        }
        if self.in_engine >= self.watermarks.high {
            if !self.gate_closed {
                self.gate_closed = true;
                self.stats.gate_closed += 1;
            }
            return None;
        }
        if self.gate_closed && self.in_engine > self.watermarks.low {
            return None;
        }
        self.gate_closed = false;
        let tag = self.ready.pop_front()?;
        let slot = &mut self.tags[usize::from(tag)];
        let kind = slot.kind?;
        slot.state = TagState::InEngine;
        self.in_engine += 1;
        match kind {
            RequestKind::Read { .. } => self.stats.reads += 1,
            RequestKind::Write { .. } => self.stats.writes += 1,
            RequestKind::Flush { .. } => self.stats.flushes += 1,
            RequestKind::Discard { .. } => self.stats.discards += 1,
            RequestKind::WriteZeroes { .. } => self.stats.write_zeroes += 1,
        }
        Some(Request {
            id: request_id(self.q_id, tag, slot.gen),
            kind,
        })
    }

    fn copy_in(&mut self, req: &Request, dst: &mut [u8]) -> Result<(), IoError> {
        let (q, tag, gen) = split_id(req.id);
        let slot = self.tags.get(usize::from(tag)).copied();
        let live =
            q == self.q_id && slot.is_some_and(|s| s.state == TagState::InEngine && s.gen == gen);
        if !live {
            return Err(IoError::Corrupt);
        }
        let RequestKind::Write { blocks, .. } = req.kind else {
            return Err(IoError::Corrupt);
        };
        if dst.len() != blocks as usize * BLOCK_SIZE {
            return Err(IoError::BadLength { bytes: dst.len() });
        }
        self.transport.read_data(tag, 0, dst)
    }

    fn copy_out(&mut self, req: &Request, src: &[u8]) -> Result<(), IoError> {
        let (q, tag, gen) = split_id(req.id);
        let slot = self.tags.get(usize::from(tag)).copied();
        let live =
            q == self.q_id && slot.is_some_and(|s| s.state == TagState::InEngine && s.gen == gen);
        if !live {
            return Err(IoError::Corrupt);
        }
        self.transport.write_data(tag, 0, src)
    }

    fn complete(&mut self, req: Request, status: Status, _seq: u64) {
        let (q, tag, gen) = split_id(req.id);
        let Some(slot) = self.tags.get_mut(usize::from(tag)) else {
            self.stats.stale += 1;
            return;
        };
        if q != self.q_id || slot.state != TagState::InEngine || slot.gen != gen {
            self.stats.stale += 1;
            return;
        }
        slot.state = TagState::Idle;
        slot.kind = None;
        slot.gen = slot.gen.wrapping_add(1);
        self.in_engine = self.in_engine.saturating_sub(1);
        if status != Status::Ok {
            self.stats.errors += 1;
        }
        let result = errno_for(status, &req.kind);
        self.answer(tag, result);
    }
}

impl<T: Transport> fmt::Debug for UblkFrontDoor<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("UblkFrontDoor")
            .field("q_id", &self.q_id)
            .field("vol", &self.vol)
            .field("depth", &self.tags.len())
            .field("ready", &self.ready.len())
            .field("in_engine", &self.in_engine)
            .field("offline", &self.offline)
            .finish_non_exhaustive()
    }
}
