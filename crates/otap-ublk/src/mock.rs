//! A ublk queue in memory: the tests' transport, on any host; and the
//! driver's side of `START_DEV` over it ([`MockStart`]).

use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::{Arc, Mutex, MutexGuard};

use otap_cp::IoError;
use otap_format::BLOCK_SIZE;
use otap_sys::ublk::{
    UblksrvIoDesc, UBLK_IO_OP_DISCARD, UBLK_IO_OP_FLUSH, UBLK_IO_OP_READ, UBLK_IO_OP_WRITE,
    UBLK_IO_OP_WRITE_ZEROES,
};

use crate::frontdoor::{QueueEvent, Transport, TransportError};
use crate::start::DeviceControl;

/// `EEXIST`, `START_DEV` on a live device.
const EEXIST: i32 = 17;
/// `EINVAL`.
const EINVAL: i32 = 22;

/// What the driver's `START_DEV` waits for, shared by a [`MockStart`] (the
/// control half) and the [`MockTransport`] linked to it (the queue).
#[derive(Debug, Default)]
struct StartModel {
    /// A queue is linked: its fetches were submitted (the
    /// `nr_queues_ready` wait is met).
    linked: bool,
    /// `START_DEV` is in flight.
    begun: bool,
    /// The partition scan's read is to be delivered at the queue's next
    /// poll (`add_disk` has been reached).
    scan_due: bool,
    /// The tag the scan's read was delivered on, until it is answered.
    scan_tag: Option<u16>,
    /// What the scan's read was answered with.
    scan_result: Option<i32>,
    /// The device is live.
    live: bool,
    /// Fail the start with this errno at its next poll.
    fail: Option<i32>,
}

/// The driver's side of `START_DEV` (6.12, `ublk_ctrl_start_dev`), in
/// memory: begun, it waits until a queue is linked (every tag fetched),
/// then delivers the partition scan's read of LBA 0 through that queue and
/// completes only once the read has been answered, exactly the reads a
/// privileged server's start does through the queue it is starting. A
/// server that waits for the start on the thread that serves the queue
/// never sees it complete; one that serves while it polls does
/// ([`crate::start::start_while_serving`]).
#[derive(Debug, Clone, Default)]
pub struct MockStart {
    model: Arc<Mutex<StartModel>>,
}

impl MockStart {
    /// A device not yet started, with no queue linked.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    fn model(&self) -> MutexGuard<'_, StartModel> {
        // A test that panicked while holding the lock has failed already.
        self.model
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Links the queue the start reads through; its fetches count as
    /// submitted, as `UblkDevice::create` submits them before the start.
    pub fn link(&self, transport: &mut MockTransport) {
        self.model().linked = true;
        transport.start = Some(Arc::clone(&self.model));
    }

    /// Makes the start in flight fail with `errno` at its next poll.
    pub fn fail_with(&self, errno: i32) {
        self.model().fail = Some(errno);
    }

    /// `true` once the device is live.
    #[must_use]
    pub fn is_live(&self) -> bool {
        self.model().live
    }

    /// `true` while `START_DEV` is in flight.
    #[must_use]
    pub fn in_flight(&self) -> bool {
        self.model().begun
    }

    /// What the partition scan's read was answered with, once it was.
    #[must_use]
    pub fn scan_result(&self) -> Option<i32> {
        self.model().scan_result
    }
}

impl DeviceControl for MockStart {
    fn begin_start(&mut self) -> Result<(), TransportError> {
        let mut m = self.model();
        if m.live || m.begun {
            return Err(TransportError {
                what: "START_DEV of a started device",
                errno: EEXIST,
            });
        }
        m.begun = true;
        m.scan_due = true;
        Ok(())
    }

    fn poll_start(&mut self) -> Result<bool, TransportError> {
        let mut m = self.model();
        if m.live {
            return Ok(true);
        }
        if !m.begun {
            return Err(TransportError {
                what: "no START_DEV in flight",
                errno: EINVAL,
            });
        }
        if let Some(errno) = m.fail.take() {
            m.begun = false;
            return Err(TransportError {
                what: "START_DEV",
                errno,
            });
        }
        if m.scan_result.is_some() {
            m.begun = false;
            m.live = true;
            return Ok(true);
        }
        Ok(false)
    }
}

/// A queue whose requests the test writes and whose completions the test
/// reads.
#[derive(Debug, Default)]
pub struct MockTransport {
    depth: u16,
    /// The start this queue serves the partition scan of, if linked.
    start: Option<Arc<Mutex<StartModel>>>,
    events: VecDeque<QueueEvent>,
    /// Write payloads the driver holds, by tag.
    payloads: HashMap<u16, Vec<u8>>,
    /// Read data the server copied in, by tag.
    read_back: HashMap<u16, Vec<u8>>,
    /// Tags with a request outstanding.
    busy: HashSet<u16>,
    /// Tags the driver aborted: no fetch is pending there, so no request
    /// is ever delivered on them again.
    aborted: HashSet<u16>,
    commits: Vec<(u16, i32)>,
    /// Fail the next data copies.
    fail_copies: bool,
    /// Fail every commit.
    fail_commits: bool,
    /// Fail the next poll.
    fail_poll: bool,
    polls: u64,
}

impl MockTransport {
    /// A queue `depth` tags deep.
    #[must_use]
    pub fn new(depth: u16) -> Self {
        Self {
            depth,
            ..Self::default()
        }
    }

    fn desc(op: u8, lba: u64, blocks: u32, flags: u32) -> UblksrvIoDesc {
        UblksrvIoDesc {
            op_flags: u32::from(op) | flags,
            nr_sectors: blocks * (BLOCK_SIZE / 512) as u32,
            start_sector: lba * (BLOCK_SIZE / 512) as u64,
            addr: 0,
        }
    }

    fn deliver(&mut self, tag: u16, desc: UblksrvIoDesc) {
        assert!(tag < self.depth, "tag {tag} beyond the depth");
        assert!(
            !self.aborted.contains(&tag),
            "tag {tag} was aborted; the driver has no fetch to answer there"
        );
        assert!(self.busy.insert(tag), "tag {tag} already has a request");
        self.events.push_back(QueueEvent::Request { tag, desc });
    }

    /// Delivers a write of `data` (whole blocks) at `lba` on `tag`.
    pub fn submit_write(&mut self, tag: u16, lba: u64, data: Vec<u8>) {
        let blocks = (data.len() / BLOCK_SIZE) as u32;
        self.payloads.insert(tag, data);
        self.deliver(tag, Self::desc(UBLK_IO_OP_WRITE, lba, blocks, 0));
    }

    /// Delivers a read of `blocks` blocks at `lba` on `tag`.
    pub fn submit_read(&mut self, tag: u16, lba: u64, blocks: u32) {
        self.deliver(tag, Self::desc(UBLK_IO_OP_READ, lba, blocks, 0));
    }

    /// Delivers a flush on `tag`.
    pub fn submit_flush(&mut self, tag: u16) {
        self.deliver(tag, Self::desc(UBLK_IO_OP_FLUSH, 0, 0, 0));
    }

    /// Delivers a discard on `tag`.
    pub fn submit_discard(&mut self, tag: u16, lba: u64, blocks: u32) {
        self.deliver(tag, Self::desc(UBLK_IO_OP_DISCARD, lba, blocks, 0));
    }

    /// Delivers a write of zeros on `tag`.
    pub fn submit_write_zeroes(&mut self, tag: u16, lba: u64, blocks: u32) {
        self.deliver(tag, Self::desc(UBLK_IO_OP_WRITE_ZEROES, lba, blocks, 0));
    }

    /// Delivers a raw descriptor on `tag` (a malformed or unsupported
    /// request).
    pub fn submit_raw(&mut self, tag: u16, desc: UblksrvIoDesc) {
        self.deliver(tag, desc);
    }

    /// Aborts `tag`: the driver answers the tag's fetch with an abort
    /// instead of a request, so a request delivered on the tag but not
    /// yet pumped is withdrawn.
    pub fn abort(&mut self, tag: u16) {
        self.aborted.insert(tag);
        self.busy.remove(&tag);
        self.payloads.remove(&tag);
        self.events
            .retain(|e| !matches!(e, QueueEvent::Request { tag: t, .. } if *t == tag));
        self.events.push_back(QueueEvent::Abort { tag });
    }

    /// Delivers `event` with no bookkeeping: the driver misbehaving (a
    /// request on a tag that already has one, or on a tag the server
    /// never fetched).
    pub fn inject(&mut self, event: QueueEvent) {
        self.events.push_back(event);
    }

    /// The completions since the last call, as `(tag, result)`.
    pub fn take_commits(&mut self) -> Vec<(u16, i32)> {
        std::mem::take(&mut self.commits)
    }

    /// The completion of `tag`, if it has one, leaving the others.
    pub fn take_commit(&mut self, tag: u16) -> Option<i32> {
        let pos = self.commits.iter().position(|(t, _)| *t == tag)?;
        Some(self.commits.remove(pos).1)
    }

    /// The data a completed read on `tag` returned.
    pub fn read_payload(&mut self, tag: u16) -> Option<Vec<u8>> {
        self.read_back.remove(&tag)
    }

    /// Tags with a request outstanding.
    #[must_use]
    pub fn outstanding(&self) -> usize {
        self.busy.len()
    }

    /// Makes the next data copies fail.
    pub fn fail_copies(&mut self, fail: bool) {
        self.fail_copies = fail;
    }

    /// Makes every commit fail.
    pub fn fail_commits(&mut self, fail: bool) {
        self.fail_commits = fail;
    }

    /// Makes the next poll fail.
    pub fn fail_next_poll(&mut self) {
        self.fail_poll = true;
    }

    /// Polls so far.
    #[must_use]
    pub const fn polls(&self) -> u64 {
        self.polls
    }
}

impl MockTransport {
    /// The linked start's partition scan, when `add_disk` has been reached:
    /// a read of LBA 0 on the lowest tag with a fetch pending.
    fn deliver_scan(&mut self) {
        let Some(model) = self.start.clone() else {
            return;
        };
        let mut m = model
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if !(m.begun && m.scan_due) {
            return;
        }
        let Some(tag) =
            (0..self.depth).find(|t| !self.busy.contains(t) && !self.aborted.contains(t))
        else {
            // Every tag holds a request: the scan waits for one to free.
            return;
        };
        m.scan_due = false;
        m.scan_tag = Some(tag);
        drop(m);
        self.deliver(tag, Self::desc(UBLK_IO_OP_READ, 0, 1, 0));
    }
}

impl Transport for MockTransport {
    fn poll(&mut self, out: &mut Vec<QueueEvent>) -> Result<usize, TransportError> {
        self.polls += 1;
        if self.fail_poll {
            self.fail_poll = false;
            return Err(TransportError {
                what: "poll failed",
                errno: 5,
            });
        }
        self.deliver_scan();
        let n = self.events.len();
        out.extend(self.events.drain(..));
        Ok(n)
    }

    fn read_data(&mut self, tag: u16, off: u32, dst: &mut [u8]) -> Result<(), IoError> {
        if self.fail_copies {
            return Err(IoError::Device { errno: 5 });
        }
        let data = self.payloads.get(&tag).ok_or(IoError::Corrupt)?;
        let start = off as usize;
        if start + dst.len() > data.len() {
            return Err(IoError::BadLength { bytes: dst.len() });
        }
        dst.copy_from_slice(&data[start..start + dst.len()]);
        Ok(())
    }

    fn write_data(&mut self, tag: u16, off: u32, src: &[u8]) -> Result<(), IoError> {
        if self.fail_copies {
            return Err(IoError::Device { errno: 5 });
        }
        if !self.busy.contains(&tag) {
            return Err(IoError::Corrupt);
        }
        let buf = self.read_back.entry(tag).or_default();
        let start = off as usize;
        if buf.len() < start + src.len() {
            buf.resize(start + src.len(), 0);
        }
        buf[start..start + src.len()].copy_from_slice(src);
        Ok(())
    }

    fn commit(&mut self, tag: u16, result: i32) -> Result<(), TransportError> {
        if self.fail_commits {
            return Err(TransportError {
                what: "commit refused",
                errno: 19,
            });
        }
        if !self.busy.remove(&tag) {
            return Err(TransportError {
                what: "commit of an idle tag",
                errno: 22,
            });
        }
        self.payloads.remove(&tag);
        if let Some(model) = &self.start {
            let mut m = model
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if m.scan_tag == Some(tag) {
                // The partition scan's read is answered: `add_disk`, and
                // with it `START_DEV`, can finish.
                m.scan_tag = None;
                m.scan_result = Some(result);
                return Ok(());
            }
        }
        self.commits.push((tag, result));
        Ok(())
    }
}
