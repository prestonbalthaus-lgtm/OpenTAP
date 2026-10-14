//! The fake client: a queue of requests with their data, and the
//! completions the engine delivers.

use std::collections::{HashMap, VecDeque};

use otap_cp::{FrontDoor, IoError, ReqId, Request, RequestKind, Status};
use otap_format::BLOCK_SIZE;

/// A completion the engine delivered.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Completion {
    /// The request.
    pub req: Request,
    /// How it ended.
    pub status: Status,
    /// The write's sequence number (0 otherwise).
    pub seq: u64,
}

/// The fake front door.
#[derive(Debug, Default)]
pub struct SimFrontDoor {
    queue: VecDeque<Request>,
    data: HashMap<ReqId, Vec<u8>>,
    reads: HashMap<ReqId, Vec<u8>>,
    completions: Vec<Completion>,
    next_id: u64,
    /// Requests fetched by the engine and not completed.
    in_flight: usize,
}

impl SimFrontDoor {
    /// An empty front door.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    fn id(&mut self) -> ReqId {
        self.next_id += 1;
        ReqId(self.next_id)
    }

    /// Queues a write of `data` (whole blocks) at `lba` of `vol`.
    pub fn submit_write(&mut self, vol: u32, lba: u64, data: Vec<u8>) -> ReqId {
        debug_assert!(data.len().is_multiple_of(BLOCK_SIZE) && !data.is_empty());
        let id = self.id();
        let blocks = (data.len() / BLOCK_SIZE) as u32;
        self.data.insert(id, data);
        self.queue.push_back(Request {
            id,
            kind: RequestKind::Write { vol, lba, blocks },
        });
        id
    }

    /// Queues a read.
    pub fn submit_read(&mut self, vol: u32, lba: u64, blocks: u32) -> ReqId {
        let id = self.id();
        self.queue.push_back(Request {
            id,
            kind: RequestKind::Read { vol, lba, blocks },
        });
        id
    }

    /// Queues a flush.
    pub fn submit_flush(&mut self, vol: u32) -> ReqId {
        let id = self.id();
        self.queue.push_back(Request {
            id,
            kind: RequestKind::Flush { vol },
        });
        id
    }

    /// Completions since the last call.
    pub fn take_completions(&mut self) -> Vec<Completion> {
        std::mem::take(&mut self.completions)
    }

    /// The data a completed read returned.
    pub fn take_read(&mut self, id: ReqId) -> Option<Vec<u8>> {
        self.reads.remove(&id)
    }

    /// Requests queued and not yet fetched.
    #[must_use]
    pub fn queued(&self) -> usize {
        self.queue.len()
    }

    /// Requests fetched by the engine and not yet completed.
    #[must_use]
    pub const fn in_flight(&self) -> usize {
        self.in_flight
    }

    /// Drops everything queued or in flight (a client that gave up).
    pub fn reset(&mut self) {
        self.queue.clear();
        self.data.clear();
        self.reads.clear();
        self.completions.clear();
        self.in_flight = 0;
    }
}

impl FrontDoor for SimFrontDoor {
    fn fetch(&mut self) -> Option<Request> {
        let req = self.queue.pop_front()?;
        self.in_flight += 1;
        Some(req)
    }

    fn copy_in(&mut self, req: &Request, dst: &mut [u8]) -> Result<(), IoError> {
        let data = self.data.get(&req.id).ok_or(IoError::Corrupt)?;
        if data.len() != dst.len() {
            return Err(IoError::BadLength { bytes: dst.len() });
        }
        dst.copy_from_slice(data);
        Ok(())
    }

    fn copy_out(&mut self, req: &Request, src: &[u8]) -> Result<(), IoError> {
        self.reads.insert(req.id, src.to_vec());
        Ok(())
    }

    fn complete(&mut self, req: Request, status: Status, seq: u64) {
        self.data.remove(&req.id);
        self.in_flight = self.in_flight.saturating_sub(1);
        self.completions.push(Completion { req, status, seq });
    }
}
