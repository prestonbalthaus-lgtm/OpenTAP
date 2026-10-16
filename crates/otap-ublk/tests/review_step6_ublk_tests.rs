//! The Step 6 adversarial review (`docs/reviews/2026-10-16-step6-qa-adversarial.md`):
//! the ublk recovery findings, over the in-memory driver (`MockKernel`).
//! Each test reproduced its finding before the fix and pins the fixed
//! behaviour. `Server` and the device constants are those of
//! `step6b_recovery_ublk_tests.rs`, trimmed.

use std::cell::Cell;

use otap_cp::Clock;
use otap_format::BLOCK_SIZE;
use otap_sim::SimClock;
use otap_sys::ublk::{UblksrvIoDesc, UBLK_IO_OP_READ, UBLK_IO_OP_WRITE};
use otap_ublk::{
    classify, device_stamp, reap_check, start_while_serving, BoxedTransport, DevInfo, DevState,
    DeviceSpec, Expect, HostDevice, MockHost, MockKernel, Owner, QueueEvent, Recovery, Standing,
    StartError, Transport, TransportError, UblkHost,
};

const POOL: u32 = 0x5EED_0002;
const VOL: u32 = 3;
const DEPTH: u16 = 8;
const VOL_BLOCKS: u64 = 256;
const MAX_IO: u32 = 1 << 20;
const BUDGET_NS: u64 = 40_000_000_000;

fn spec() -> DeviceSpec {
    DeviceSpec {
        size_blocks: VOL_BLOCKS,
        depth: DEPTH,
        max_io_bytes: MAX_IO,
        io_opt_bytes: 32 * 1024,
        max_discard_bytes: Some(1 << 20),
        rotational: false,
    }
}

fn expect() -> Expect {
    Expect {
        stamp: device_stamp(POOL, VOL),
        size_blocks: VOL_BLOCKS,
        max_io_bytes: MAX_IO,
    }
}

/// A daemon process serving one device by hand: every request it is
/// handed is answered unless it is told to hold them.
struct Server {
    _host: MockHost,
    device: Box<dyn HostDevice>,
    queue: BoxedTransport,
    holding: bool,
    held: Vec<(u16, UblksrvIoDesc)>,
}

impl Server {
    fn new(host: MockHost, mut device: Box<dyn HostDevice>) -> Self {
        let queue = device.take_transport().expect("the device's queue");
        Self {
            _host: host,
            device,
            queue,
            holding: false,
            held: Vec::new(),
        }
    }

    fn serve(&mut self) -> Result<(), TransportError> {
        let mut events = Vec::new();
        self.queue.poll(&mut events)?;
        for event in events {
            let QueueEvent::Request { tag, desc } = event else {
                continue;
            };
            if self.holding {
                self.held.push((tag, desc));
                continue;
            }
            let bytes = (desc.nr_sectors() as usize) * 512;
            if desc.op() == UBLK_IO_OP_READ {
                self.queue
                    .write_data(tag, 0, &vec![0u8; bytes])
                    .expect("read data");
            }
            let result = if desc.op() == UBLK_IO_OP_READ || desc.op() == UBLK_IO_OP_WRITE {
                bytes as i32
            } else {
                0
            };
            self.queue.commit(tag, result)?;
        }
        Ok(())
    }

    fn go_live(&mut self) -> Result<(), StartError<TransportError>> {
        let turns = Cell::new(0u32);
        start_while_serving(
            self,
            |s| s.device.begin_start(),
            |s| s.device.poll_start(),
            |s| {
                turns.set(turns.get() + 1);
                s.serve()
            },
            || turns.get() > 1_000,
        )
    }
}

fn info(dev_id: u32, state: DevState, pid: i32, stamp: u64) -> DevInfo {
    DevInfo {
        dev_id,
        state,
        pid,
        flags: otap_ublk::mock_kernel::MOCK_DEVICE_FLAGS,
        stamp,
        queues: 1,
        depth: DEPTH,
        max_io_bytes: MAX_IO,
        size_blocks: Some(VOL_BLOCKS),
    }
}

/// W2: a restarted daemon may get the pid of the one that died (`dbladed`
/// as PID 1 of a container). A device still `LIVE` under that pid, its
/// server dead with every tag in hand, was refused by the recovery at once
/// ("this process serves it already") instead of being waited for until
/// the kernel quiesces it, so the volume lost its device number whatever
/// `--recovery-timeout` said. A device naming this process's pid that this
/// process does not serve is the earlier process's, and its server is dead.
#[test]
fn w2_a_device_live_under_a_reused_pid_is_waited_for_and_re_attached() {
    let kernel = MockKernel::new();
    let mut host = kernel.process();
    let device = host
        .create(&spec(), device_stamp(POOL, VOL))
        .expect("ADD_DEV");
    let mut server = Server::new(host, device);
    server.go_live().expect("START_DEV");
    let dev_id = server.device.dev_id();
    // The daemon is handed every tag and answers none, then dies: only the
    // request timeout will quiesce the device.
    server.holding = true;
    for i in 0..u64::from(DEPTH) {
        let _ = kernel.submit_write(dev_id, i * 2, vec![7u8; BLOCK_SIZE]);
    }
    server.serve().unwrap();
    assert_eq!(server.held.len(), usize::from(DEPTH));
    let old_pid = kernel.info(dev_id).expect("the device").pid;
    drop(server);
    assert_eq!(kernel.info(dev_id).unwrap().state, DevState::Live);
    // The new daemon has the dead one's pid.
    let mut host = kernel.process_with_pid(old_pid);
    assert_eq!(host.pid() as i32, old_pid);
    let mut recovery = Recovery::new(dev_id, expect(), BUDGET_NS);
    let clock = SimClock::new(0);
    let mut device = None;
    for attempt in 0..10_000u32 {
        match recovery.poll(&mut host, clock.mono_ns()) {
            Ok(Some(d)) => {
                device = Some(d);
                break;
            }
            Ok(None) => {}
            Err(e) => panic!("the recovery gave up on the device: {e}"),
        }
        if attempt == 10 {
            kernel.request_timeout(dev_id);
        }
        clock.advance(20_000_000);
    }
    let mut server = Server::new(host, device.expect("re-attached once quiesced"));
    server.go_live().expect("END_USER_RECOVERY");
    assert_eq!(kernel.info(dev_id).unwrap().state, DevState::Live);
}

/// W2, the reaper's side: the same device was classed stale with its
/// server alive (this process), so `device reap` passed it to `STOP_DEV`,
/// which on a `LIVE` device waits for its started requests: on the engine
/// thread, up to the block layer's 30 s (for ever, if the pid's process
/// never quiesces it). It is classed with its server dead now: still
/// quiescing, so never reaped while `LIVE`; a stamped one of this pool is
/// recoverable.
#[test]
fn w2_a_device_live_under_this_pid_that_this_process_does_not_serve_is_never_reaped() {
    let me = 500u32;
    let wants = |vol: u16| u32::from(vol) == VOL;
    let owner = Owner {
        pool: Some(POOL),
        serving: &[],
        stale: &[],
        wants: &wants,
    };
    let unstamped = classify(&info(1, DevState::Live, 500, 0), true, me, &owner);
    assert_eq!(unstamped.standing, Standing::Stale);
    assert!(unstamped.quiescing(), "{}", unstamped.reason);
    assert!(reap_check(&unstamped, true)
        .unwrap_err()
        .contains("still quiescing"));
    let ours = classify(
        &info(2, DevState::Live, 500, device_stamp(POOL, VOL)),
        true,
        me,
        &owner,
    );
    assert_eq!(ours.standing, Standing::Recoverable, "{}", ours.reason);
    assert!(reap_check(&ours, true).is_err());
}
