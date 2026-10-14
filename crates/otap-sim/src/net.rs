//! An in-memory network: ordered, reliable delivery between endpoints with
//! a delay in ticks, a partition switch and bounded queues. A partition
//! holds what is sent and delivers it, in order, when the network heals;
//! nothing accepted is ever dropped (the `Net` contract).

use std::cell::RefCell;
use std::collections::VecDeque;
use std::rc::Rc;

use otap_cp::{EndpointId, Net, NetError};

/// A chunk waiting in an inbox: delivery tick (`None` while held by a
/// partition), sender, bytes.
type Queued = (Option<u64>, EndpointId, Vec<u8>);

#[derive(Debug)]
struct Shared {
    now: u64,
    delay: u64,
    partitioned: bool,
    queue_limit: usize,
    inboxes: Vec<VecDeque<Queued>>,
    held: u64,
}

/// The network every [`NetEnd`] of one simulation shares.
#[derive(Debug, Clone)]
pub struct LoopbackNet {
    shared: Rc<RefCell<Shared>>,
}

impl LoopbackNet {
    /// A network of `endpoints` endpoints, delivering one tick after a send.
    #[must_use]
    pub fn new(endpoints: usize) -> Self {
        Self {
            shared: Rc::new(RefCell::new(Shared {
                now: 0,
                delay: 1,
                partitioned: false,
                queue_limit: 1024,
                inboxes: (0..endpoints).map(|_| VecDeque::new()).collect(),
                held: 0,
            })),
        }
    }

    /// The handle endpoint `id` sends and receives with.
    #[must_use]
    pub fn end(&self, id: EndpointId) -> NetEnd {
        NetEnd {
            me: id,
            shared: Rc::clone(&self.shared),
        }
    }

    /// Advances delivery time by one tick.
    pub fn tick(&self) {
        self.shared.borrow_mut().now += 1;
    }

    /// Sets the delivery delay in ticks.
    pub fn set_delay(&self, ticks: u64) {
        self.shared.borrow_mut().delay = ticks;
    }

    /// Cuts or heals the network: while cut, sends are accepted and held;
    /// healing schedules every held chunk, in order, `delay` ticks out.
    pub fn set_partitioned(&self, cut: bool) {
        let mut shared = self.shared.borrow_mut();
        shared.partitioned = cut;
        if !cut {
            let at = shared.now + shared.delay;
            for inbox in &mut shared.inboxes {
                for (deliver_at, _, _) in inbox.iter_mut() {
                    if deliver_at.is_none() {
                        *deliver_at = Some(at);
                    }
                }
            }
        }
    }

    /// Chunks a partition has held so far.
    #[must_use]
    pub fn held(&self) -> u64 {
        self.shared.borrow().held
    }

    /// Sets the inbox bound above which a send answers `Busy`.
    pub fn set_queue_limit(&self, limit: usize) {
        self.shared.borrow_mut().queue_limit = limit;
    }
}

/// One endpoint's view of the network.
#[derive(Debug, Clone)]
pub struct NetEnd {
    me: EndpointId,
    shared: Rc<RefCell<Shared>>,
}

impl NetEnd {
    /// This endpoint.
    #[must_use]
    pub const fn id(&self) -> EndpointId {
        self.me
    }
}

impl Net for NetEnd {
    fn send(&mut self, to: EndpointId, bytes: &[u8]) -> Result<(), NetError> {
        let mut shared = self.shared.borrow_mut();
        let limit = shared.queue_limit;
        let partitioned = shared.partitioned;
        let deliver_at = if partitioned {
            None
        } else {
            Some(shared.now + shared.delay)
        };
        let from = self.me;
        let inbox = shared
            .inboxes
            .get_mut(to.0 as usize)
            .ok_or(NetError::Unreachable)?;
        if inbox.len() >= limit {
            return Err(NetError::Busy);
        }
        inbox.push_back((deliver_at, from, bytes.to_vec()));
        if partitioned {
            shared.held += 1;
        }
        Ok(())
    }

    fn poll_recv(&mut self, out: &mut Vec<(EndpointId, Vec<u8>)>) -> usize {
        let mut shared = self.shared.borrow_mut();
        let now = shared.now;
        let Some(inbox) = shared.inboxes.get_mut(self.me.0 as usize) else {
            return 0;
        };
        let mut n = 0;
        while inbox
            .front()
            .is_some_and(|(at, _, _)| at.is_some_and(|at| at <= now))
        {
            let (_, from, bytes) = inbox.pop_front().expect("front exists");
            out.push((from, bytes));
            n += 1;
        }
        n
    }
}
