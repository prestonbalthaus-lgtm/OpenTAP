//! The state machine under random interleavings of the driver and the
//! engine, against a model that mirrors the queue's timing: driver events
//! land when the front door pumps (a fetch with nothing ready, or an
//! explicit pump), engine completions land at once. After every step the
//! model and the front door agree on what the engine holds, what is ready,
//! what the mock still owes, every counter, and every commit in order;
//! every request ends in exactly one completion with the result its status
//! maps to, or in none when the driver aborted it; the engine never holds
//! more than the high watermark, and a closed gate stays closed until the
//! low watermark.

use std::collections::{HashMap, HashSet, VecDeque};

use otap_cp::{FrontDoor, Request, RequestKind, Status};
use otap_format::BLOCK_SIZE;
use otap_sys::ublk::{UblksrvIoDesc, UBLK_IO_OP_READ, UBLK_IO_OP_WRITE_SAME};
use otap_ublk::{
    decode_request, errno_for, split_id, FrontDoorStats, MockTransport, UblkFrontDoor, Watermarks,
    EINVAL, ENODEV,
};
use proptest::prelude::*;

const VOL: u32 = 0;
const SIZE: u64 = 64;
const SPB: u64 = (BLOCK_SIZE / 512) as u64;
const Q: u16 = 1;

#[derive(Debug, Clone)]
enum Op {
    /// The driver delivers a request on a tag (if it owes nothing on it).
    Submit {
        tag: u16,
        kind: u8,
        lba: u64,
        blocks: u32,
    },
    /// The driver delivers something unservable.
    SubmitBad { tag: u16, unsupported: bool },
    /// The driver aborts a tag.
    Abort { tag: u16 },
    /// The front door pumps.
    Pump,
    /// The engine fetches.
    Fetch,
    /// The engine completes the `n`th request it holds with a status.
    Complete { n: u8, status: u8 },
    /// The engine completes a request it already completed or that was
    /// aborted under it.
    CompleteStale { n: u8 },
    /// The device goes offline or online.
    Offline(bool),
}

fn status_of(i: u8) -> Status {
    match i % 9 {
        0..=3 => Status::Ok,
        4 => Status::NvFailed,
        5 => Status::NoSuchVolume,
        6 => Status::NoSpace,
        7 => Status::OutOfRange,
        _ => Status::Failed,
    }
}

fn op_strategy() -> impl Strategy<Value = Op> {
    prop_oneof![
        6 => (any::<u16>(), 0..5u8, 0..SIZE, 1..4u32).prop_map(|(tag, kind, lba, blocks)| Op::Submit {
            tag,
            kind,
            lba,
            blocks
        }),
        1 => (any::<u16>(), any::<bool>()).prop_map(|(tag, unsupported)| Op::SubmitBad { tag, unsupported }),
        1 => any::<u16>().prop_map(|tag| Op::Abort { tag }),
        2 => Just(Op::Pump),
        6 => Just(Op::Fetch),
        5 => (any::<u8>(), any::<u8>()).prop_map(|(n, status)| Op::Complete { n, status }),
        1 => any::<u8>().prop_map(|n| Op::CompleteStale { n }),
        1 => any::<bool>().prop_map(Op::Offline),
    ]
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TagState {
    Idle,
    Ready,
    InEngine,
    Aborted,
}

#[derive(Debug, Clone, Copy)]
enum Delivered {
    Request { tag: u16, desc: UblksrvIoDesc },
    Abort { tag: u16 },
}

/// The model: the front door's state as the test understands it.
struct Model {
    depth: u16,
    marks: Watermarks,
    states: Vec<TagState>,
    kinds: Vec<Option<RequestKind>>,
    gens: Vec<u32>,
    ready: VecDeque<u16>,
    /// Events the mock holds, not yet pumped.
    queued: VecDeque<Delivered>,
    /// What the engine holds, in fetch order.
    held: Vec<Request>,
    /// Requests whose completion must be dropped: done, or aborted under
    /// the engine.
    stale: Vec<Request>,
    /// Tags the mock owes a commit on (its `busy` set).
    busy: HashSet<u16>,
    /// Tags the driver aborted: it delivers nothing on them again.
    retired: HashSet<u16>,
    /// Commits expected since the last check, in order.
    commits: Vec<(u16, i32)>,
    stats: FrontDoorStats,
    gate_closed: bool,
    offline: bool,
}

impl Model {
    fn commit(&mut self, tag: u16, result: i32) {
        // The mock refuses a commit on a tag it does not owe; the front
        // door then retires the tag.
        if self.busy.remove(&tag) {
            self.commits.push((tag, result));
        } else {
            self.stats.commit_failures += 1;
            self.states[usize::from(tag)] = TagState::Aborted;
        }
    }

    /// Mirrors `UblkFrontDoor::pump`.
    fn pump(&mut self) {
        while let Some(event) = self.queued.pop_front() {
            match event {
                Delivered::Abort { tag } => {
                    if tag >= self.depth {
                        continue;
                    }
                    let i = usize::from(tag);
                    match self.states[i] {
                        TagState::Ready => self.ready.retain(|t| *t != tag),
                        TagState::InEngine => {
                            self.gens[i] = self.gens[i].wrapping_add(1);
                            if let Some(pos) =
                                self.held.iter().position(|r| split_id(r.id).1 == tag)
                            {
                                let req = self.held.remove(pos);
                                self.stale.push(req);
                            }
                        }
                        TagState::Idle | TagState::Aborted => {}
                    }
                    self.states[i] = TagState::Aborted;
                    self.kinds[i] = None;
                    self.stats.aborted += 1;
                }
                Delivered::Request { tag, desc } => {
                    self.stats.requests += 1;
                    if tag >= self.depth {
                        continue;
                    }
                    let i = usize::from(tag);
                    if self.states[i] != TagState::Idle {
                        self.commit(tag, -EINVAL);
                        self.stats.refused += 1;
                        continue;
                    }
                    if self.offline {
                        self.commit(tag, -ENODEV);
                        self.stats.offline_rejected += 1;
                        continue;
                    }
                    match decode_request(&desc, VOL, SIZE) {
                        Ok(kind) => {
                            self.states[i] = TagState::Ready;
                            self.kinds[i] = Some(kind);
                            self.ready.push_back(tag);
                        }
                        Err(errno) => {
                            self.commit(tag, -errno);
                            self.stats.refused += 1;
                        }
                    }
                }
            }
        }
    }

    /// Mirrors `UblkFrontDoor::fetch`: what the engine gets.
    fn fetch(&mut self) -> Option<Request> {
        if self.ready.is_empty() {
            self.pump();
        }
        if self.held.len() >= self.marks.high {
            if !self.gate_closed {
                self.gate_closed = true;
                self.stats.gate_closed += 1;
            }
            return None;
        }
        if self.gate_closed && self.held.len() > self.marks.low {
            return None;
        }
        self.gate_closed = false;
        let tag = self.ready.pop_front()?;
        let i = usize::from(tag);
        let kind = self.kinds[i]?;
        self.states[i] = TagState::InEngine;
        match kind {
            RequestKind::Read { .. } => self.stats.reads += 1,
            RequestKind::Write { .. } => self.stats.writes += 1,
            RequestKind::Flush { .. } => self.stats.flushes += 1,
            RequestKind::Discard { .. } => self.stats.discards += 1,
            RequestKind::WriteZeroes { .. } => self.stats.write_zeroes += 1,
        }
        let req = Request {
            id: otap_ublk::request_id(Q, tag, self.gens[i]),
            kind,
        };
        self.held.push(req);
        Some(req)
    }

    /// Mirrors `UblkFrontDoor::complete` for a request the engine holds.
    fn complete(&mut self, req: Request, status: Status) {
        let (_, tag, _) = split_id(req.id);
        let i = usize::from(tag);
        self.held.retain(|r| r.id != req.id);
        self.states[i] = TagState::Idle;
        self.kinds[i] = None;
        self.gens[i] = self.gens[i].wrapping_add(1);
        if status != Status::Ok {
            self.stats.errors += 1;
        }
        self.commit(tag, errno_for(status, &req.kind));
        self.stale.push(req);
    }
}

fn run(depth: u16, marks: Watermarks, ops: Vec<Op>) -> Result<(), TestCaseError> {
    let mut fd = UblkFrontDoor::new(MockTransport::new(depth), Q, VOL, SIZE, depth, marks);
    let mut m = Model {
        depth,
        marks,
        states: vec![TagState::Idle; usize::from(depth)],
        kinds: vec![None; usize::from(depth)],
        gens: vec![0; usize::from(depth)],
        ready: VecDeque::new(),
        queued: VecDeque::new(),
        held: Vec::new(),
        stale: Vec::new(),
        busy: HashSet::new(),
        retired: HashSet::new(),
        commits: Vec::new(),
        stats: FrontDoorStats::default(),
        gate_closed: false,
        offline: false,
    };
    let mut payloads: HashMap<u16, Vec<u8>> = HashMap::new();
    for op in ops {
        match op {
            Op::Submit {
                tag,
                kind,
                lba,
                blocks,
            } => {
                let tag = tag % depth;
                if m.retired.contains(&tag) || !m.busy.insert(tag) {
                    continue;
                }
                let lba = lba.min(SIZE - u64::from(blocks));
                let spb = SPB as u32;
                let (op, start, n) = match kind {
                    0 => (UBLK_IO_OP_READ, lba * SPB, blocks * spb),
                    1 => (otap_sys::ublk::UBLK_IO_OP_WRITE, lba * SPB, blocks * spb),
                    2 => (otap_sys::ublk::UBLK_IO_OP_FLUSH, 0, 0),
                    3 => (otap_sys::ublk::UBLK_IO_OP_DISCARD, lba * SPB, blocks * spb),
                    _ => (
                        otap_sys::ublk::UBLK_IO_OP_WRITE_ZEROES,
                        lba * SPB,
                        blocks * spb,
                    ),
                };
                let desc = UblksrvIoDesc {
                    op_flags: u32::from(op),
                    nr_sectors: n,
                    start_sector: start,
                    addr: 0,
                };
                let t = fd.transport();
                match kind {
                    0 => t.submit_read(tag, lba, blocks),
                    1 => {
                        let data = vec![tag as u8 ^ 0x5A; blocks as usize * BLOCK_SIZE];
                        payloads.insert(tag, data.clone());
                        t.submit_write(tag, lba, data);
                    }
                    2 => t.submit_flush(tag),
                    3 => t.submit_discard(tag, lba, blocks),
                    _ => t.submit_write_zeroes(tag, lba, blocks),
                }
                m.queued.push_back(Delivered::Request { tag, desc });
            }
            Op::SubmitBad { tag, unsupported } => {
                let tag = tag % depth;
                if m.retired.contains(&tag) || !m.busy.insert(tag) {
                    continue;
                }
                let desc = if unsupported {
                    UblksrvIoDesc {
                        op_flags: u32::from(UBLK_IO_OP_WRITE_SAME),
                        nr_sectors: SPB as u32,
                        start_sector: 0,
                        addr: 0,
                    }
                } else {
                    UblksrvIoDesc {
                        op_flags: u32::from(UBLK_IO_OP_READ),
                        nr_sectors: 3,
                        start_sector: 0,
                        addr: 0,
                    }
                };
                fd.transport().submit_raw(tag, desc);
                m.queued.push_back(Delivered::Request { tag, desc });
            }
            Op::Abort { tag } => {
                let tag = tag % depth;
                // The driver's abort replaces the tag's fetch: a request it
                // delivered but the server has not pumped is withdrawn.
                fd.transport().abort(tag);
                m.busy.remove(&tag);
                m.retired.insert(tag);
                m.queued
                    .retain(|d| !matches!(d, Delivered::Request { tag: t, .. } if *t == tag));
                m.queued.push_back(Delivered::Abort { tag });
                payloads.remove(&tag);
            }
            Op::Pump => {
                let expected = m.queued.len();
                let n = fd.pump().unwrap();
                m.pump();
                prop_assert_eq!(n, expected);
            }
            Op::Fetch => {
                let got = fd.fetch();
                let want = m.fetch();
                prop_assert_eq!(got.map(|r| (r.id, r.kind)), want.map(|r| (r.id, r.kind)));
                if let Some(req) = got {
                    let (q, tag, _) = split_id(req.id);
                    prop_assert_eq!(q, Q);
                    prop_assert_eq!(req.vol(), VOL);
                    if let RequestKind::Write { blocks, .. } = req.kind {
                        let mut buf = vec![0u8; blocks as usize * BLOCK_SIZE];
                        match fd.copy_in(&req, &mut buf) {
                            Ok(()) => prop_assert_eq!(&buf, &payloads[&tag]),
                            Err(e) => {
                                // The driver withdrew the write after the
                                // front door saw it (an abort not yet
                                // pumped): the engine fails the request at
                                // once, and the queue refuses the commit.
                                prop_assert_eq!(e, otap_cp::IoError::Corrupt);
                                prop_assert!(!m.busy.contains(&tag), "tag {} is owed", tag);
                                fd.complete(req, Status::Failed, 0);
                                m.complete(req, Status::Failed);
                            }
                        }
                    }
                }
            }
            Op::Complete { n, status } => {
                if m.held.is_empty() {
                    continue;
                }
                let req = m.held[usize::from(n) % m.held.len()];
                let tag = split_id(req.id).1;
                let mut status = status_of(status);
                if let RequestKind::Read { blocks, .. } = req.kind {
                    let data = vec![0x11u8; blocks as usize * BLOCK_SIZE];
                    match fd.copy_out(&req, &data) {
                        Ok(()) => {}
                        Err(e) => {
                            // The driver withdrew the request (an abort
                            // the front door has not pumped yet): the copy
                            // fails and the engine completes with a
                            // failure, whose commit the queue then refuses.
                            prop_assert_eq!(e, otap_cp::IoError::Corrupt);
                            prop_assert!(!m.busy.contains(&tag), "tag {} is owed", tag);
                            status = Status::Failed;
                        }
                    }
                }
                fd.complete(req, status, 1);
                m.complete(req, status);
                payloads.remove(&tag);
            }
            Op::CompleteStale { n } => {
                if m.stale.is_empty() {
                    continue;
                }
                let req = m.stale[usize::from(n) % m.stale.len()];
                let mut buf = vec![0u8; BLOCK_SIZE];
                prop_assert_eq!(fd.copy_in(&req, &mut buf), Err(otap_cp::IoError::Corrupt));
                prop_assert_eq!(fd.copy_out(&req, &buf), Err(otap_cp::IoError::Corrupt));
                fd.complete(req, Status::Ok, 0);
                m.stats.stale += 1;
            }
            Op::Offline(off) => {
                // Going offline answers every request still waiting, in
                // order; requests the engine holds finish normally.
                if off {
                    while let Some(tag) = m.ready.pop_front() {
                        let i = usize::from(tag);
                        m.states[i] = TagState::Idle;
                        m.kinds[i] = None;
                        m.commit(tag, -ENODEV);
                        m.stats.offline_rejected += 1;
                    }
                }
                fd.set_offline(off);
                m.offline = off;
            }
        }
        // Invariants after every step.
        prop_assert_eq!(fd.in_engine(), m.held.len());
        prop_assert!(fd.in_engine() <= marks.high);
        prop_assert_eq!(fd.ready(), m.ready.len());
        prop_assert_eq!(fd.is_offline(), m.offline);
        prop_assert_eq!(fd.stats(), m.stats);
        prop_assert_eq!(fd.transport().outstanding(), m.busy.len());
        let committed = fd.transport().take_commits();
        prop_assert_eq!(&committed, &m.commits);
        m.commits.clear();
        let aborted: Vec<u16> = m
            .states
            .iter()
            .enumerate()
            .filter(|(_, s)| **s == TagState::Aborted)
            .map(|(i, _)| i as u16)
            .collect();
        prop_assert_eq!(fd.aborted_tags(), aborted);
    }
    // Drain: pump, then complete everything the engine holds; the mock
    // then owes exactly the ready requests.
    let _ = fd.pump();
    m.pump();
    for req in m.held.clone() {
        if let RequestKind::Read { blocks, .. } = req.kind {
            fd.copy_out(&req, &vec![0u8; blocks as usize * BLOCK_SIZE])
                .unwrap();
        }
        fd.complete(req, Status::Ok, 1);
        m.complete(req, Status::Ok);
    }
    prop_assert_eq!(fd.transport().take_commits(), m.commits);
    prop_assert_eq!(fd.in_engine(), 0);
    prop_assert_eq!(fd.ready(), m.ready.len());
    prop_assert_eq!(fd.transport().outstanding(), m.ready.len());
    prop_assert_eq!(fd.stats(), m.stats);
    Ok(())
}

proptest! {
    #![proptest_config(ProptestConfig {
        cases: 256,
        .. ProptestConfig::default()
    })]

    #[test]
    fn the_state_machine_keeps_its_contract(
        depth in 1..12u16,
        high in 1..8usize,
        ops in prop::collection::vec(op_strategy(), 1..200),
    ) {
        run(depth, Watermarks { high, low: high / 2 }, ops)?;
    }
}
