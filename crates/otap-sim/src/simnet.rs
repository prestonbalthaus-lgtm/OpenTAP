//! A deterministic, connection-oriented network for the replication
//! sessions (Step 6C): [`SimNet`] implements the engine's [`Dialer`],
//! [`LinkListener`] and [`Link`] over in-memory byte pipes, with seeded
//! faults.
//!
//! Every message crosses a pipe as the same link frame the TCP transport
//! writes ([`otap_cp::repl::wire`]: magic, length, CRC, payload), and the
//! receiving end parses it with the same [`FrameReader`], so what a fault
//! does to the bytes is what the transport would see:
//!
//! - **drop**: the frame never arrives, nor does anything sent after it on
//!   that connection; both ends then see the connection reset (TCP loses
//!   a segment only that way: a connection that cannot deliver is reset);
//! - **cut**: a prefix of the frame arrives, then the connection ends, so
//!   the receiver reports a frame truncated mid-way;
//! - **corrupt**: one byte of the frame is flipped, so the receiver's CRC
//!   check refuses it and drops the connection.
//!
//! A fault is chosen by rule (the `n`-th frame of a kind in a direction)
//! or at random from the seed within a budget, so every walk ends with the
//! network healthy and the transfer can converge. Delivery is in order
//! with a delay in ticks; the clock moves only with [`SimNet::tick`]. A
//! connection never duplicates or reorders a frame (TCP does not either);
//! the duplicates a replication session must tolerate are the resends
//! after a reconnect, which the sessions produce themselves.

use std::cell::RefCell;
use std::collections::{BTreeMap, VecDeque};
use std::rc::Rc;

use otap_cp::repl::wire::{encode_frame, peek_kind, FrameReader};
use otap_cp::{Dialer, Link, LinkError, LinkListener};

use crate::rng::SplitMix64;

/// Which way a frame travels on a connection.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Direction {
    /// From the side that dialed to the side that accepted.
    FromDialer,
    /// From the side that accepted to the side that dialed.
    FromAcceptor,
}

/// What a fault does to a frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum FaultKind {
    /// The frame and everything after it are lost; the connection resets.
    Drop,
    /// A prefix of the frame arrives, then the connection ends.
    Cut,
    /// One byte of the frame is flipped; the receiver's CRC refuses it.
    Corrupt,
}

/// A fault aimed at one frame: the `nth` (from 0) frame on the whole
/// network that matches the direction, the message byte and, for a stream
/// frame, the stream kind.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct FaultRule {
    /// What to do.
    pub kind: FaultKind,
    /// The direction, or both.
    pub direction: Option<Direction>,
    /// The message byte (1 handshake, 2 refusal, 3 stream), or any.
    pub message: Option<u8>,
    /// The stream frame's kind byte (`otap_format::FrameKind`), or any.
    pub stream_kind: Option<u8>,
    /// Which matching frame, from 0.
    pub nth: u64,
}

impl FaultRule {
    fn matches(&self, dir: Direction, message: u8, stream_kind: Option<u8>) -> bool {
        self.direction.is_none_or(|d| d == dir)
            && self.message.is_none_or(|m| m == message)
            && self.stream_kind.is_none_or(|k| Some(k) == stream_kind)
    }
}

/// What the network did.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct NetStats {
    /// Connections dialed.
    pub connections: u64,
    /// Dials refused (no listener, or a refusal the plan asked for).
    pub refused: u64,
    /// Frames sent.
    pub frames: u64,
    /// Bytes sent.
    pub bytes: u64,
    /// Frames dropped.
    pub drops: u64,
    /// Frames cut.
    pub cuts: u64,
    /// Frames corrupted.
    pub corruptions: u64,
}

impl NetStats {
    /// Faults injected of every kind.
    #[must_use]
    pub const fn faults(&self) -> u64 {
        self.drops + self.cuts + self.corruptions
    }
}

/// One way of a connection.
#[derive(Debug, Default)]
struct Way {
    /// Bytes in flight: when they arrive, and what.
    queue: VecDeque<(u64, Vec<u8>)>,
    /// The reader of the receiving end.
    reader: FrameReader,
    /// The sender closed this way, or the connection reset: nothing more
    /// is accepted, and the receiver sees the end once the queue drained.
    ended: Option<String>,
}

#[derive(Debug)]
struct Pipe {
    addr: String,
    /// Dialer to acceptor.
    up: Way,
    /// Acceptor to dialer.
    down: Way,
    /// The connection failed: each end reports this once it has read what
    /// arrived before the failure.
    reset: Option<String>,
}

#[derive(Debug)]
struct NetState {
    now: u64,
    delay: u64,
    queue_limit: usize,
    rng: SplitMix64,
    listeners: BTreeMap<String, VecDeque<u64>>,
    pipes: BTreeMap<u64, Pipe>,
    next_conn: u64,
    rules: Vec<(FaultRule, u64)>,
    random_per_mille: u64,
    random_budget: u64,
    refuse_dials: u64,
    stats: NetStats,
}

/// The network a simulation's pools share.
#[derive(Debug, Clone)]
pub struct SimNet {
    shared: Rc<RefCell<NetState>>,
}

impl SimNet {
    /// A healthy network; `seed` drives the random faults.
    #[must_use]
    pub fn new(seed: u64) -> Self {
        Self {
            shared: Rc::new(RefCell::new(NetState {
                now: 0,
                delay: 1,
                queue_limit: 256,
                rng: SplitMix64::new(seed),
                listeners: BTreeMap::new(),
                pipes: BTreeMap::new(),
                next_conn: 1,
                rules: Vec::new(),
                random_per_mille: 0,
                random_budget: 0,
                refuse_dials: 0,
                stats: NetStats::default(),
            })),
        }
    }

    /// Advances delivery time by one tick.
    pub fn tick(&self) {
        self.shared.borrow_mut().now += 1;
    }

    /// The delivery delay in ticks.
    pub fn set_delay(&self, ticks: u64) {
        self.shared.borrow_mut().delay = ticks;
    }

    /// Frames a way may hold before `send` answers `Busy`.
    pub fn set_queue_limit(&self, frames: usize) {
        self.shared.borrow_mut().queue_limit = frames.max(1);
    }

    /// Aims a fault at one frame.
    pub fn add_fault(&self, rule: FaultRule) {
        self.shared.borrow_mut().rules.push((rule, 0));
    }

    /// Random faults: each frame is hit with probability `per_mille`/1000,
    /// at most `budget` times in all (drop, cut and corrupt equally likely).
    pub fn set_random_faults(&self, per_mille: u64, budget: u64) {
        let mut s = self.shared.borrow_mut();
        s.random_per_mille = per_mille.min(1000);
        s.random_budget = budget;
    }

    /// The next `n` dials are refused, as by a host that is down.
    pub fn refuse_dials(&self, n: u64) {
        self.shared.borrow_mut().refuse_dials = n;
    }

    /// Resets every open connection now, as a cable pulled.
    pub fn cut_all(&self) {
        let mut s = self.shared.borrow_mut();
        for pipe in s.pipes.values_mut() {
            if pipe.reset.is_none() {
                pipe.reset = Some("the connection was reset (cable pulled)".to_string());
            }
        }
    }

    /// What the network did so far.
    #[must_use]
    pub fn stats(&self) -> NetStats {
        self.shared.borrow().stats
    }

    /// Connections still open at both ends.
    #[must_use]
    pub fn open_connections(&self) -> usize {
        self.shared
            .borrow()
            .pipes
            .values()
            .filter(|p| p.reset.is_none() && p.up.ended.is_none() && p.down.ended.is_none())
            .count()
    }

    /// A listener at `addr` (a name; no two listeners share one).
    #[must_use]
    pub fn listen(&self, addr: &str) -> SimListener {
        self.shared
            .borrow_mut()
            .listeners
            .entry(addr.to_string())
            .or_default();
        SimListener {
            shared: Rc::clone(&self.shared),
            addr: addr.to_string(),
        }
    }

    /// Stops listening at `addr`: later dials there are refused.
    pub fn unlisten(&self, addr: &str) {
        self.shared.borrow_mut().listeners.remove(addr);
    }

    /// A dialer on this network.
    #[must_use]
    pub fn dialer(&self) -> SimDialer {
        SimDialer {
            shared: Rc::clone(&self.shared),
        }
    }
}

/// Accepts the connections dialed to one address.
#[derive(Debug)]
pub struct SimListener {
    shared: Rc<RefCell<NetState>>,
    addr: String,
}

impl LinkListener for SimListener {
    fn accept(&mut self) -> Option<Box<dyn Link>> {
        let id = self
            .shared
            .borrow_mut()
            .listeners
            .get_mut(&self.addr)?
            .pop_front()?;
        Some(Box::new(SimLink {
            shared: Rc::clone(&self.shared),
            id,
            dialer: false,
            closed: false,
        }))
    }
}

/// Dials addresses of a [`SimNet`].
#[derive(Debug)]
pub struct SimDialer {
    shared: Rc<RefCell<NetState>>,
}

impl Dialer for SimDialer {
    fn dial(&mut self, addr: &str) -> Result<Box<dyn Link>, LinkError> {
        let mut s = self.shared.borrow_mut();
        if s.refuse_dials > 0 {
            s.refuse_dials -= 1;
            s.stats.refused += 1;
            return Err(LinkError::Closed(format!(
                "cannot connect to {addr}: connection refused"
            )));
        }
        let id = s.next_conn;
        let Some(accept) = s.listeners.get_mut(addr) else {
            s.stats.refused += 1;
            return Err(LinkError::Closed(format!(
                "cannot connect to {addr}: nothing listens there"
            )));
        };
        accept.push_back(id);
        s.next_conn += 1;
        s.stats.connections += 1;
        s.pipes.insert(
            id,
            Pipe {
                addr: addr.to_string(),
                up: Way::default(),
                down: Way::default(),
                reset: None,
            },
        );
        Ok(Box::new(SimLink {
            shared: Rc::clone(&self.shared),
            id,
            dialer: true,
            closed: false,
        }))
    }
}

/// One end of a simulated connection.
#[derive(Debug)]
pub struct SimLink {
    shared: Rc<RefCell<NetState>>,
    id: u64,
    dialer: bool,
    closed: bool,
}

impl SimLink {
    fn direction(&self) -> Direction {
        if self.dialer {
            Direction::FromDialer
        } else {
            Direction::FromAcceptor
        }
    }
}

/// Decides the fault, if any, for one frame.
fn choose_fault(s: &mut NetState, dir: Direction, payload: &[u8]) -> Option<FaultKind> {
    let (message, stream_kind) = peek_kind(payload);
    let mut hit = None;
    for (rule, seen) in &mut s.rules {
        if rule.matches(dir, message, stream_kind) {
            if *seen == rule.nth && hit.is_none() {
                hit = Some(rule.kind);
            }
            *seen += 1;
        }
    }
    if hit.is_some() {
        return hit;
    }
    if s.random_budget > 0 && s.random_per_mille > 0 && s.rng.chance(s.random_per_mille, 1000) {
        s.random_budget -= 1;
        return Some(match s.rng.below(3) {
            0 => FaultKind::Drop,
            1 => FaultKind::Cut,
            _ => FaultKind::Corrupt,
        });
    }
    None
}

impl Link for SimLink {
    fn send(&mut self, msg: &[u8]) -> Result<(), LinkError> {
        if self.closed {
            return Err(LinkError::Closed("closed by this side".to_string()));
        }
        let dir = self.direction();
        let mut guard = self.shared.borrow_mut();
        let s = &mut *guard;
        let frame = encode_frame(msg).map_err(|e| LinkError::Closed(e.to_string()))?;
        let Some(pipe) = s.pipes.get(&self.id) else {
            return Err(LinkError::Closed("the connection is gone".to_string()));
        };
        if let Some(why) = &pipe.reset {
            return Err(LinkError::Closed(why.clone()));
        }
        let way = if self.dialer { &pipe.up } else { &pipe.down };
        if let Some(why) = &way.ended {
            return Err(LinkError::Closed(why.clone()));
        }
        if way.queue.len() >= s.queue_limit {
            return Err(LinkError::Busy);
        }
        let fault = choose_fault(s, dir, msg);
        let at = s.now + s.delay;
        s.stats.frames += 1;
        s.stats.bytes += frame.len() as u64;
        let cut_at = if fault == Some(FaultKind::Cut) {
            1 + s.rng.below(frame.len() as u64 - 1) as usize
        } else {
            0
        };
        let flip_at = s.rng.below(frame.len() as u64) as usize;
        let Some(pipe) = s.pipes.get_mut(&self.id) else {
            return Err(LinkError::Closed("the connection is gone".to_string()));
        };
        let way = if self.dialer {
            &mut pipe.up
        } else {
            &mut pipe.down
        };
        match fault {
            None => way.queue.push_back((at, frame)),
            Some(FaultKind::Corrupt) => {
                let mut frame = frame;
                frame[flip_at] ^= 0x5a;
                way.queue.push_back((at, frame));
                s.stats.corruptions += 1;
            }
            Some(FaultKind::Cut) => {
                way.queue.push_back((at, frame[..cut_at].to_vec()));
                way.ended = Some("the connection ended in the middle of a frame".to_string());
                pipe.reset = Some(format!(
                    "the connection to {} was cut in the middle of a frame",
                    pipe.addr
                ));
                s.stats.cuts += 1;
            }
            Some(FaultKind::Drop) => {
                way.ended = Some("a frame was lost and the connection reset".to_string());
                pipe.reset = Some(format!(
                    "the connection to {} reset after a lost frame",
                    pipe.addr
                ));
                s.stats.drops += 1;
            }
        }
        Ok(())
    }

    fn recv(&mut self) -> Result<Option<Vec<u8>>, LinkError> {
        if self.closed {
            return Err(LinkError::Closed("closed by this side".to_string()));
        }
        let mut guard = self.shared.borrow_mut();
        let s = &mut *guard;
        let now = s.now;
        let Some(pipe) = s.pipes.get_mut(&self.id) else {
            return Err(LinkError::Closed("the connection is gone".to_string()));
        };
        let reset = pipe.reset.clone();
        let way = if self.dialer {
            &mut pipe.down
        } else {
            &mut pipe.up
        };
        while way.queue.front().is_some_and(|(at, _)| *at <= now) {
            if let Some((_, bytes)) = way.queue.pop_front() {
                way.reader.feed(&bytes);
            }
        }
        match way.reader.next_payload() {
            Ok(Some(payload)) => return Ok(Some(payload)),
            Ok(None) => {}
            Err(e) => {
                let why = format!("a frame from the peer was refused: {e}");
                pipe.reset = Some(why.clone());
                return Err(LinkError::Closed(why));
            }
        }
        if !way.queue.is_empty() {
            return Ok(None);
        }
        if let Some(why) = reset.or_else(|| way.ended.clone()) {
            return Err(LinkError::Closed(match way.reader.finish() {
                Ok(()) => why,
                Err(e) => format!("{why}: {e}"),
            }));
        }
        Ok(None)
    }

    fn close(&mut self) {
        if self.closed {
            return;
        }
        self.closed = true;
        let mut s = self.shared.borrow_mut();
        if let Some(pipe) = s.pipes.get_mut(&self.id) {
            let way = if self.dialer {
                &mut pipe.up
            } else {
                &mut pipe.down
            };
            if way.ended.is_none() {
                way.ended = Some("closed by the peer".to_string());
            }
            // Reading stops too: the other way ends for this side.
            let other = if self.dialer {
                &mut pipe.down
            } else {
                &mut pipe.up
            };
            if other.ended.is_none() {
                other.ended = Some("closed by the peer".to_string());
            }
        }
    }

    fn peer(&self) -> String {
        let s = self.shared.borrow();
        let addr = s
            .pipes
            .get(&self.id)
            .map_or_else(String::new, |p| p.addr.clone());
        if self.dialer {
            addr
        } else {
            format!("{addr}#{}", self.id)
        }
    }
}

impl Drop for SimLink {
    fn drop(&mut self) {
        self.close();
    }
}
