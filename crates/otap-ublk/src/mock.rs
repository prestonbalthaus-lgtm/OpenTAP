//! A ublk queue in memory: the tests' transport, on any host.

use std::collections::{HashMap, HashSet, VecDeque};

use otap_cp::IoError;
use otap_format::BLOCK_SIZE;
use otap_sys::ublk::{
    UblksrvIoDesc, UBLK_IO_OP_DISCARD, UBLK_IO_OP_FLUSH, UBLK_IO_OP_READ, UBLK_IO_OP_WRITE,
    UBLK_IO_OP_WRITE_ZEROES,
};

use crate::frontdoor::{QueueEvent, Transport, TransportError};

/// A queue whose requests the test writes and whose completions the test
/// reads.
#[derive(Debug, Default)]
pub struct MockTransport {
    depth: u16,
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
        self.commits.push((tag, result));
        Ok(())
    }
}
