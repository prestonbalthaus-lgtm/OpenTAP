//! Step 6B: the `UBLK_F_USER_RECOVERY` re-attach of a dead daemon's device,
//! the device standings and the reaper's checks, over the in-memory 6.12
//! driver ([`MockKernel`]).
//!
//! The kernel rules the mock models are those of `otap_ublk::recovery`
//! (from `ublk_drv.c`: `ublk_ctrl_start_recovery`, `ublk_ctrl_end_recovery`,
//! `ublk_abort_queue`, `__ublk_fail_req`, `ublk_timeout`, `ublk_ch_open`
//! and `ublk_ch_release`). A "daemon" here is a [`MockKernel::process`]
//! that serves its queue by hand, or the real engine behind the front door;
//! dropping it is its `kill -9`.

use std::cell::Cell;

use otap_cp::{Clock, Engine, EngineConfig, Event, Geometry, MediaProfile, VolumeSpec};
use otap_format::{checksum::crc32c, Uuid, BLOCK_SIZE, INTENT_SLOT_SIZE, SUPERBLOCK_SLOT_SIZE};
use otap_sim::{SimBackstore, SimClock};
use otap_sys::ublk::{UblksrvIoDesc, UBLK_IO_OP_READ, UBLK_IO_OP_WRITE};
use otap_sys::{HugeArena, HugePageSize};
use otap_ublk::{
    classify, device_stamp, pool_fingerprint, read_stamp, reap_check, start_while_serving,
    BoxedTransport, DevInfo, DevState, DeviceSpec, Expect, ForeignDevice, HostDevice, MockCmd,
    MockHost, MockKernel, Owner, QueueEvent, Recovery, RecoveryError, Standing, StartError,
    Transport, TransportError, UblkFrontDoor, UblkHost, Watermarks, EBUSY, EINVAL, EIO,
    RECOVERY_RETRY_NS, RECOVERY_TARGET_NS, STAMP_MAGIC,
};

const POOL: u32 = 0x5EED_0001;
const VOL: u32 = 3;
const DEPTH: u16 = 8;
const VOL_BLOCKS: u64 = 256;
const MAX_IO: u32 = 1 << 20;
const BUDGET_NS: u64 = 5_000_000_000;

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

fn pattern(lba: u64, blocks: u32, salt: u8) -> Vec<u8> {
    let mut v = vec![0u8; blocks as usize * BLOCK_SIZE];
    for (i, b) in v.iter_mut().enumerate() {
        *b = (lba as u8)
            .wrapping_mul(31)
            .wrapping_add((i / BLOCK_SIZE) as u8)
            .wrapping_add(salt);
    }
    v
}

/// A daemon process serving one device by hand: every request it is
/// handed is answered (a read with zeros) unless it is told to hold them.
struct Server {
    host: MockHost,
    device: Box<dyn HostDevice>,
    queue: BoxedTransport,
    holding: bool,
    held: Vec<(u16, UblksrvIoDesc)>,
    answered: u32,
}

impl Server {
    fn new(host: MockHost, mut device: Box<dyn HostDevice>) -> Self {
        let queue = device.take_transport().expect("the device's queue");
        Self {
            host,
            device,
            queue,
            holding: false,
            held: Vec::new(),
            answered: 0,
        }
    }

    /// One turn of the serving loop.
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
            self.answered += 1;
        }
        Ok(())
    }

    /// Brings the device live (`START_DEV` or `END_USER_RECOVERY`) while
    /// serving.
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

/// A daemon that created the device of `VOL` and brought it live.
fn created(kernel: &MockKernel) -> Server {
    let mut host = kernel.process();
    let device = host
        .create(&spec(), device_stamp(POOL, VOL))
        .expect("ADD_DEV");
    let mut server = Server::new(host, device);
    server.go_live().expect("START_DEV");
    server
}

/// The recovery of `dev_id` by a new daemon process, stepped until it
/// re-attaches or fails, with `turn` run between attempts.
fn recover_with(
    kernel: &MockKernel,
    dev_id: u32,
    budget_ns: u64,
    mut turn: impl FnMut(u32),
) -> Result<Server, RecoveryError> {
    let mut host = kernel.process();
    let mut recovery = Recovery::new(dev_id, expect(), budget_ns);
    let clock = SimClock::new(0);
    for attempt in 0..10_000u32 {
        if let Some(device) = recovery.poll(&mut host, clock.mono_ns())? {
            return Ok(Server::new(host, device));
        }
        turn(attempt);
        clock.advance(20_000_000);
    }
    panic!("the recovery of device {dev_id} never ended");
}

fn recover(kernel: &MockKernel, dev_id: u32) -> Result<Server, RecoveryError> {
    recover_with(kernel, dev_id, BUDGET_NS, |_| {})
}

#[test]
fn a_stamp_names_its_pool_and_volume_and_nothing_else_is_one() {
    let agg = Uuid::from_bytes([0x42; 16]);
    let fp = pool_fingerprint(&agg);
    assert_eq!(fp, crc32c(agg.as_array()));
    let raw = device_stamp(fp, 1023);
    assert_eq!(raw >> 48, u64::from(STAMP_MAGIC));
    let stamp = read_stamp(raw).expect("an OpenTAP stamp");
    assert_eq!((stamp.pool, stamp.vol), (fp, 1023));
    // A Step 5 device (0) and other software's values are not stamps.
    assert_eq!(read_stamp(0), None);
    assert_eq!(read_stamp(u64::MAX), None);
    assert_eq!(read_stamp(raw ^ (1 << 63)), None);
    // Two pools, two stamps for the same volume.
    let other = pool_fingerprint(&Uuid::from_bytes([0x43; 16]));
    assert_ne!(device_stamp(fp, 7), device_stamp(other, 7));
}

#[test]
fn a_dead_daemons_device_is_re_attached_with_its_number_and_its_requests() {
    let kernel = MockKernel::new();
    let server = created(&kernel);
    let dev_id = server.device.dev_id();
    let first_pid = server.host.pid() as i32;
    assert_eq!(kernel.info(dev_id).unwrap().state, DevState::Live);
    // kill -9: the queue (with its pending fetches) goes with the process.
    drop(server);
    assert!(!kernel.pid_alive(first_pid));
    let info = kernel.info(dev_id).expect("the device outlives its server");
    assert_eq!(info.state, DevState::Quiesced);
    assert_eq!(info.pid, first_pid);
    // The block layer keeps writing; the quiesced device holds it.
    let write = kernel.submit_write(dev_id, 4, pattern(4, 2, 1));
    assert_eq!(kernel.completion(write), None);
    let mut server = recover(&kernel, dev_id).expect("re-attached");
    assert_eq!(server.device.dev_id(), dev_id, "the same /dev/ublkbN");
    server.go_live().expect("END_USER_RECOVERY");
    let info = kernel.info(dev_id).unwrap();
    assert_eq!(info.state, DevState::Live);
    assert_eq!(info.pid, server.host.pid() as i32);
    for _ in 0..4 {
        server.serve().unwrap();
    }
    let done = kernel
        .completion(write)
        .expect("the held write was delivered");
    assert_eq!(done.result, 2 * BLOCK_SIZE as i32);
    assert_eq!(done.deliveries, 1);
    // The order the driver needs: START, then the fetches, then END.
    let log = kernel.log();
    let start = log.iter().position(|l| l.ends_with("StartUserRecovery 0"));
    let fetch = log.iter().position(|l| l.ends_with("Reattach 0"));
    let end = log.iter().position(|l| l.ends_with("EndUserRecovery 0"));
    assert!(start < fetch && fetch < end, "{log:?}");
}

#[test]
fn a_device_that_vanished_ends_its_recovery_as_gone() {
    let kernel = MockKernel::new();
    let server = created(&kernel);
    let dev_id = server.device.dev_id();
    drop(server);
    // Someone deletes it before the new daemon gets to it.
    kernel.process().reap(dev_id).expect("reaped");
    assert_eq!(kernel.info(dev_id), None);
    assert!(matches!(recover(&kernel, dev_id), Err(RecoveryError::Gone)));
}

#[test]
fn a_device_not_yet_quiesced_is_waited_for_and_its_held_requests_reissued() {
    let kernel = MockKernel::new();
    let mut server = created(&kernel);
    let dev_id = server.device.dev_id();
    // The daemon is handed every tag and answers none: no fetch is pending
    // when it dies, so nothing is cancelled and only the request timeout
    // quiesces the device (`ublk_timeout`).
    server.holding = true;
    let writes: Vec<u64> = (0..u64::from(DEPTH))
        .map(|i| kernel.submit_write(dev_id, i * 2, pattern(i * 2, 1, 7)))
        .collect();
    server.serve().unwrap();
    assert_eq!(server.held.len(), usize::from(DEPTH));
    drop(server);
    let info = kernel.info(dev_id).unwrap();
    assert_eq!(
        info.state,
        DevState::Live,
        "not quiesced: every tag was held"
    );
    // The recovery waits (no START is even sent to a LIVE device), and the
    // timeout fires during the wait.
    let mut server = recover_with(&kernel, dev_id, BUDGET_NS, |attempt| {
        if attempt == 10 {
            kernel.request_timeout(dev_id);
        }
    })
    .expect("re-attached once quiesced");
    server.go_live().expect("END_USER_RECOVERY");
    for _ in 0..4 {
        server.serve().unwrap();
    }
    // UBLK_F_USER_RECOVERY_REISSUE: the dead daemon's requests come again.
    for id in writes {
        let done = kernel.completion(id).expect("reissued and answered");
        assert_eq!(done.result, BLOCK_SIZE as i32);
        assert_eq!(done.deliveries, 2, "delivered to each daemon once");
    }
}

#[test]
fn start_user_recovery_answers_ebusy_while_the_dead_process_holds_the_char_device() {
    let kernel = MockKernel::new();
    let server = created(&kernel);
    let dev_id = server.device.dev_id();
    kernel.hold_char_after_exit(dev_id);
    drop(server);
    assert!(kernel.char_open(dev_id), "the dying process still holds it");
    let mut host = kernel.process();
    assert_eq!(
        host.start_recovery(dev_id).unwrap_err().errno,
        EBUSY,
        "ublk_ctrl_start_recovery: UB_STATE_OPEN is still set"
    );
    let mut recovery = Recovery::new(dev_id, expect(), BUDGET_NS);
    let commands = |kernel: &MockKernel| kernel.log().len();
    assert!(recovery.poll(&mut host, 0).unwrap().is_none());
    // Within the retry interval the serving loop's turns issue nothing.
    let issued = commands(&kernel);
    for ms in 1..10 {
        assert!(recovery.poll(&mut host, ms * 1_000_000).unwrap().is_none());
    }
    assert_eq!(commands(&kernel), issued);
    assert_eq!(recovery.attempts(), 1);
    assert!(recovery
        .poll(&mut host, RECOVERY_RETRY_NS)
        .unwrap()
        .is_none());
    kernel.release_char(dev_id);
    let device = recovery
        .poll(&mut host, 2 * RECOVERY_RETRY_NS)
        .unwrap()
        .expect("re-attached once released");
    assert_eq!(recovery.attempts(), 3);
    let mut server = Server::new(host, device);
    server.go_live().expect("END_USER_RECOVERY");
    // A recovery that ended answers no more.
    assert!(matches!(
        recovery.poll(&mut kernel.process(), 3 * RECOVERY_RETRY_NS),
        Err(RecoveryError::NotRecoverable(_))
    ));
}

#[test]
fn the_last_attempt_at_the_end_of_the_budget_is_never_held_back() {
    let kernel = MockKernel::new();
    let server = created(&kernel);
    let dev_id = server.device.dev_id();
    drop(server);
    kernel.busy_for(dev_id, 1);
    let mut host = kernel.process();
    // A long interval and a short budget: the poll at the budget's end
    // attempts (and here succeeds) although the interval has not passed.
    let mut recovery = Recovery::new(dev_id, expect(), 5_000_000).with_retry_ns(1_000_000_000);
    assert!(recovery.poll(&mut host, 0).unwrap().is_none());
    assert!(recovery.poll(&mut host, 4_000_000).unwrap().is_none());
    assert_eq!(recovery.attempts(), 1);
    let device = recovery
        .poll(&mut host, 5_000_000)
        .unwrap()
        .expect("re-attached at the budget's end");
    assert_eq!(recovery.attempts(), 2);
    Server::new(host, device)
        .go_live()
        .expect("END_USER_RECOVERY");
}

#[test]
fn start_and_end_user_recovery_errors_end_the_recovery() {
    let kernel = MockKernel::new();
    let server = created(&kernel);
    let dev_id = server.device.dev_id();
    drop(server);
    kernel.fail_next(MockCmd::StartUserRecovery, EIO);
    match recover(&kernel, dev_id) {
        Err(RecoveryError::Command(e)) => assert_eq!(e.errno, EIO),
        other => panic!("{:?}", other.map(|_| ())),
    }
    kernel.fail_next(MockCmd::StartUserRecovery, EINVAL);
    assert!(matches!(
        recover(&kernel, dev_id),
        Err(RecoveryError::NotRecoverable(_))
    ));
    kernel.fail_next(MockCmd::GetDevInfo, EIO);
    assert!(matches!(
        recover(&kernel, dev_id),
        Err(RecoveryError::Command(_))
    ));
    // END_USER_RECOVERY failing is the start's failure, as START_DEV's is.
    let mut server = recover(&kernel, dev_id).expect("re-attached");
    kernel.fail_next(MockCmd::EndUserRecovery, EIO);
    match server.go_live() {
        Err(StartError::Start(e)) => assert_eq!(e.errno, EIO),
        other => panic!("{other:?}"),
    }
}

#[test]
fn a_daemon_killed_between_start_and_end_is_recovered_by_the_next_one() {
    let kernel = MockKernel::new();
    let server = created(&kernel);
    let dev_id = server.device.dev_id();
    drop(server);
    let read = kernel.submit_read(dev_id, 0, 1);
    // The second daemon starts the recovery, fetches every tag, and is
    // killed before END_USER_RECOVERY.
    let second = recover(&kernel, dev_id).expect("re-attached");
    let second_pid = second.host.pid() as i32;
    drop(second);
    let info = kernel.info(dev_id).unwrap();
    assert_eq!(info.state, DevState::Quiesced, "still waiting for a server");
    assert!(!kernel.pid_alive(second_pid));
    // The third one recovers it: its queue was ready, so the driver takes
    // a new START_USER_RECOVERY.
    let mut third = recover(&kernel, dev_id).expect("recovered again");
    third.go_live().expect("END_USER_RECOVERY");
    for _ in 0..4 {
        third.serve().unwrap();
    }
    let done = kernel.completion(read).expect("answered");
    assert_eq!(done.result, BLOCK_SIZE as i32);
    assert_eq!(done.deliveries, 1, "the second daemon never served it");
}

#[test]
fn a_daemon_killed_during_its_fetches_leaves_the_device_to_the_reaper() {
    let kernel = MockKernel::new();
    let server = created(&kernel);
    let dev_id = server.device.dev_id();
    drop(server);
    kernel.fail_fetch_after(3);
    assert!(matches!(
        recover(&kernel, dev_id),
        Err(RecoveryError::Reattach(_))
    ));
    // No queue of it is ready: START_USER_RECOVERY answers EINVAL for good.
    match recover(&kernel, dev_id) {
        Err(RecoveryError::NotRecoverable(why)) => assert!(why.contains("EINVAL"), "{why}"),
        other => panic!("{:?}", other.map(|_| ())),
    }
    kernel.process().reap(dev_id).expect("reaped");
    assert_eq!(kernel.info(dev_id), None);
}

#[test]
fn recovery_meets_the_two_second_target_and_gives_up_at_its_budget() {
    let kernel = MockKernel::new();
    let first = created(&kernel);
    let fast = first.device.dev_id();
    let mut second_host = kernel.process();
    let slow = second_host
        .create(&spec(), device_stamp(POOL, VOL))
        .expect("ADD_DEV");
    let mut second = Server::new(second_host, slow);
    second.go_live().expect("START_DEV");
    let slow = second.device.dev_id();
    drop(first);
    drop(second);
    // The dying processes' io_uring contexts go late: 40 attempts for one,
    // 200 for the other, at one attempt per 20 ms serving turn.
    kernel.busy_for(fast, 40);
    kernel.busy_for(slow, 200);
    let clock = SimClock::new(0);
    let began = clock.mono_ns();
    let mut host = kernel.process();
    let mut recovery = Recovery::new(fast, expect(), 2 * RECOVERY_TARGET_NS);
    let device = loop {
        if let Some(device) = recovery.poll(&mut host, clock.mono_ns()).unwrap() {
            break device;
        }
        clock.advance(20_000_000);
    };
    let took = clock.mono_ns() - began;
    assert!(took < RECOVERY_TARGET_NS, "re-attached in {took} ns");
    assert_eq!(recovery.attempts(), 41);
    let mut server = Server::new(host, device);
    server.go_live().expect("END_USER_RECOVERY");
    // The slow one: its budget runs out first, and the error says so.
    let mut host = kernel.process();
    let mut recovery = Recovery::new(slow, expect(), RECOVERY_TARGET_NS);
    let err = loop {
        match recovery.poll(&mut host, clock.mono_ns()) {
            Ok(None) => clock.advance(20_000_000),
            Ok(Some(_)) => panic!("re-attached against the model"),
            Err(e) => break e,
        }
    };
    match err {
        RecoveryError::TimedOut { waited_ns, state } => {
            assert!(waited_ns >= RECOVERY_TARGET_NS);
            assert!(waited_ns < RECOVERY_TARGET_NS + 20_000_000);
            assert_eq!(state, DevState::Quiesced);
        }
        other => panic!("{other:?}"),
    }
    assert!(err_text(&kernel, slow).contains("request timeout"));
}

fn err_text(kernel: &MockKernel, dev_id: u32) -> String {
    let mut host = kernel.process();
    let mut recovery = Recovery::new(dev_id, expect(), 0);
    match recovery.poll(&mut host, 0) {
        Err(e) => e.to_string(),
        Ok(_) => String::new(),
    }
}

#[test]
fn a_device_that_does_not_match_its_volume_is_not_re_attached() {
    let kernel = MockKernel::new();
    let server = created(&kernel);
    let dev_id = server.device.dev_id();
    drop(server);
    let mut host = kernel.process();
    let refusal =
        |expect: Expect, host: &mut MockHost| match Recovery::new(dev_id, expect, BUDGET_NS)
            .poll(host, 0)
        {
            Err(RecoveryError::NotRecoverable(why)) => why,
            other => panic!("{:?}", other.map(|d| d.map(|d| d.dev_id()))),
        };
    let why = refusal(
        Expect {
            size_blocks: VOL_BLOCKS * 2,
            ..expect()
        },
        &mut host,
    );
    assert!(why.contains("holds 256 blocks"), "{why}");
    let why = refusal(
        Expect {
            stamp: device_stamp(POOL, VOL + 1),
            ..expect()
        },
        &mut host,
    );
    assert!(why.contains("stamp"), "{why}");
    let why = refusal(
        Expect {
            max_io_bytes: MAX_IO / 2,
            ..expect()
        },
        &mut host,
    );
    assert!(why.contains("arena"), "{why}");
    // Created without UBLK_F_USER_RECOVERY, never started, served by a
    // live process: none is taken.
    let foreign = |state, pid, recovery| {
        kernel.add_foreign(ForeignDevice {
            stamp: device_stamp(POOL, VOL),
            state,
            pid,
            recovery,
            size_blocks: VOL_BLOCKS,
            depth: DEPTH,
        })
    };
    let live_pid = kernel.foreign_process();
    for (id, needle) in [
        (
            foreign(DevState::Quiesced, 9, false),
            "UBLK_F_USER_RECOVERY",
        ),
        (foreign(DevState::Dead, 0, true), "never started"),
        (foreign(DevState::Live, live_pid, true), "serves it"),
    ] {
        match Recovery::new(id, expect(), BUDGET_NS).poll(&mut host, 0) {
            Err(RecoveryError::NotRecoverable(why)) => assert!(why.contains(needle), "{why}"),
            other => panic!("{:?}", other.map(|d| d.map(|d| d.dev_id()))),
        }
    }
    // The original device is still recoverable after all of that.
    let mut server = recover(&kernel, dev_id).expect("re-attached");
    server.go_live().expect("END_USER_RECOVERY");
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

#[test]
fn every_device_on_the_host_has_one_standing() {
    let ours = device_stamp(POOL, VOL);
    let theirs = device_stamp(POOL + 1, VOL);
    let wants_vol = |vol: u16| u32::from(vol) == VOL;
    let wants_none = |_: u16| false;
    let serving = [(5u32, 7u32)];
    let stale = [(
        6u32,
        "volume 1's device; the volume no longer exists".to_string(),
    )];
    let booted = Owner {
        pool: Some(POOL),
        serving: &serving,
        stale: &stale,
        wants: &wants_vol,
    };
    let booted_full = Owner {
        wants: &wants_none,
        ..booted
    };
    let no_pool = Owner {
        pool: None,
        serving: &[],
        stale: &[],
        wants: &wants_none,
    };
    let me = 500u32;
    let check = |info: DevInfo, alive: bool, owner: &Owner<'_>, want: Standing, needle: &str| {
        let p = classify(&info, alive, me, owner);
        assert_eq!(p.standing, want, "{info:?}: {}", p.reason);
        assert!(p.reason.contains(needle), "{}", p.reason);
    };
    check(
        info(5, DevState::Live, 500, ours),
        true,
        &booted,
        Standing::Serving,
        "volume 7",
    );
    check(
        info(6, DevState::Quiesced, 9, ours),
        false,
        &booted,
        Standing::Stale,
        "no longer",
    );
    // A record counts only for a device with this pool's stamp and no live
    // server: the number may have gone to someone else since.
    check(
        info(6, DevState::Quiesced, 9, theirs),
        false,
        &booted,
        Standing::Foreign,
        "another OpenTAP pool",
    );
    check(
        info(6, DevState::Quiesced, 9, 0),
        false,
        &booted,
        Standing::Stale,
        "not an OpenTAP device",
    );
    check(
        info(6, DevState::Live, 77, ours),
        true,
        &booted,
        Standing::Foreign,
        "process 77",
    );
    check(
        info(1, DevState::Live, 77, ours),
        true,
        &booted,
        Standing::Foreign,
        "process 77",
    );
    check(
        info(1, DevState::Live, 500, 0),
        true,
        &booted,
        Standing::Stale,
        "this daemon's pid",
    );
    check(
        info(1, DevState::Quiesced, 9, ours),
        false,
        &booted,
        Standing::Recoverable,
        "no device",
    );
    check(
        info(1, DevState::Live, 9, ours),
        false,
        &booted,
        Standing::Recoverable,
        "no device",
    );
    check(
        info(1, DevState::Quiesced, 9, ours),
        false,
        &booted_full,
        Standing::Stale,
        "another device",
    );
    check(
        info(1, DevState::Dead, -1, ours),
        false,
        &booted,
        Standing::Stale,
        "never started",
    );
    check(
        info(1, DevState::Quiesced, 9, theirs),
        false,
        &booted,
        Standing::Foreign,
        "another OpenTAP pool",
    );
    check(
        info(1, DevState::Quiesced, 9, theirs),
        false,
        &no_pool,
        Standing::Recoverable,
        "booting that pool",
    );
    check(
        info(1, DevState::Dead, -1, theirs),
        false,
        &no_pool,
        Standing::Stale,
        "never started",
    );
    check(
        info(1, DevState::Quiesced, 9, 0),
        false,
        &booted,
        Standing::Stale,
        "not an OpenTAP device",
    );
    check(
        info(1, DevState::Quiesced, 9, 0),
        false,
        &no_pool,
        Standing::Stale,
        "not an OpenTAP device",
    );
    check(
        info(1, DevState::Dead, 0, 0),
        false,
        &no_pool,
        Standing::Foreign,
        "setting it up",
    );
    let mut no_recovery = info(1, DevState::Quiesced, 9, theirs);
    no_recovery.flags &= !otap_sys::ublk::UBLK_F_USER_RECOVERY;
    check(
        no_recovery,
        false,
        &no_pool,
        Standing::Stale,
        "without UBLK_F_USER_RECOVERY",
    );
    // The volume a stamp names is reported.
    let p = classify(&info(1, DevState::Quiesced, 9, ours), false, me, &booted);
    assert_eq!(p.vol, Some(VOL));
}

#[test]
fn only_a_stale_device_or_a_named_recoverable_one_is_reaped() {
    let wants = |_: u16| true;
    let serving = [(5u32, 7u32)];
    let owner = Owner {
        pool: Some(POOL),
        serving: &serving,
        stale: &[],
        wants: &wants,
    };
    let me = 500;
    let probed = |info: DevInfo, alive| classify(&info, alive, me, &owner);
    let serving = probed(info(5, DevState::Live, 500, device_stamp(POOL, 7)), true);
    assert!(reap_check(&serving, true)
        .unwrap_err()
        .contains("volume offline"));
    let foreign = probed(info(1, DevState::Live, 77, 0), true);
    assert!(reap_check(&foreign, true)
        .unwrap_err()
        .contains("not this daemon's"));
    let recoverable = probed(
        info(2, DevState::Quiesced, 9, device_stamp(POOL, VOL)),
        false,
    );
    assert_eq!(recoverable.standing, Standing::Recoverable);
    assert!(reap_check(&recoverable, false)
        .unwrap_err()
        .contains("name it"));
    assert_eq!(reap_check(&recoverable, true), Ok(()));
    let stale = probed(info(3, DevState::Quiesced, 9, 0), false);
    assert_eq!(reap_check(&stale, false), Ok(()));
    assert_eq!(reap_check(&stale, true), Ok(()));
    let quiescing = probed(info(4, DevState::Live, 9, 0), false);
    assert_eq!(quiescing.standing, Standing::Stale);
    assert!(quiescing.quiescing());
    assert!(reap_check(&quiescing, true)
        .unwrap_err()
        .contains("still quiescing"));
}

#[test]
fn the_probe_lists_the_host_and_the_reap_deletes_without_waiting_for_the_dying() {
    let kernel = MockKernel::new();
    let server = created(&kernel);
    let ours = server.device.dev_id();
    let step5 = kernel.add_foreign(ForeignDevice {
        stamp: 0,
        state: DevState::Quiesced,
        pid: 31337,
        recovery: true,
        size_blocks: 64,
        depth: 4,
    });
    let live_pid = kernel.foreign_process();
    let other = kernel.add_foreign(ForeignDevice {
        stamp: 0,
        state: DevState::Live,
        pid: live_pid,
        recovery: true,
        size_blocks: 64,
        depth: 4,
    });
    let mut host = kernel.process();
    let serving = [(ours, VOL)];
    let wants = |_: u16| false;
    let owner = Owner {
        pool: Some(POOL),
        serving: &serving,
        stale: &[],
        wants: &wants,
    };
    let probed = otap_ublk::probe(&mut host, &owner).expect("listed");
    let standing: Vec<(u32, Standing)> =
        probed.iter().map(|p| (p.info.dev_id, p.standing)).collect();
    assert_eq!(
        standing,
        vec![
            (ours, Standing::Serving),
            (step5, Standing::Stale),
            (other, Standing::Foreign)
        ]
    );
    assert!(probed[2].server_alive);
    // A dying process still holds the stale device's character device:
    // DEL_DEV_ASYNC returns at once, and the number goes with the release.
    kernel.hold_char_after_exit(ours);
    drop(server);
    assert!(kernel.char_open(ours));
    host.reap(ours).expect("reaped");
    assert_eq!(kernel.info(ours), None, "no longer listed");
    assert!(
        kernel.device_ids().contains(&ours),
        "the number is still held"
    );
    kernel.release_char(ours);
    assert!(!kernel.device_ids().contains(&ours));
    // DEL_DEV of our own device with its queue still open is refused (the
    // real command waits forever): the queue must go first.
    let mut creator = kernel.process();
    let mut device = creator.create(&spec(), device_stamp(POOL, VOL)).unwrap();
    let queue = device.take_transport().unwrap();
    let mut held = creator
        .create(&spec(), device_stamp(POOL, VOL + 1))
        .unwrap();
    let held_queue = held.take_transport().unwrap();
    assert_eq!(held.delete().unwrap_err().errno, EBUSY);
    drop(held_queue);
    drop(queue);
    device.delete().expect("deleted once its queue closed");
}

/// The engine behind the recovered device: what the arena holds survives
/// the daemon, and what the dead daemon held is delivered again.
mod through_the_engine {
    use super::*;

    const STRIPE: u32 = 32 * 1024;
    const SPS: u64 = 8;
    const SEGMENTS: u64 = 96;

    type BoxEngine = Engine<SimBackstore, UblkFrontDoor<BoxedTransport>, SimClock>;

    fn geometry() -> Geometry {
        let stripe = u64::from(STRIPE);
        let slot_stripes = (SUPERBLOCK_SLOT_SIZE as u64).div_ceil(stripe);
        let intent_stripes = (INTENT_SLOT_SIZE as u64).div_ceil(stripe);
        let front = (2 * slot_stripes + 2 * intent_stripes) * stripe;
        let back = 2 * slot_stripes * stripe;
        Geometry {
            stripe_bytes: STRIPE,
            segment_bytes: SPS * stripe,
            capacity_bytes: front + back + SEGMENTS * SPS * stripe,
            flush_opaque: false,
        }
    }

    fn config() -> EngineConfig {
        let mut cfg = EngineConfig::new(
            Uuid::from_bytes([0x6B; 16]),
            vec![VolumeSpec {
                vol: VOL,
                size_blocks: VOL_BLOCKS,
            }],
        );
        cfg.pacing.max_delay_ns = 0;
        cfg
    }

    fn front(device: &mut dyn HostDevice) -> UblkFrontDoor<BoxedTransport> {
        UblkFrontDoor::new(
            device.take_transport().expect("the queue"),
            0,
            VOL,
            VOL_BLOCKS,
            DEPTH,
            Watermarks::for_depth(DEPTH),
        )
    }

    fn boot(
        disk: SimBackstore,
        front: UblkFrontDoor<BoxedTransport>,
        clock: SimClock,
        memory: HugeArena,
    ) -> BoxEngine {
        let mut engine = BoxEngine::boot(config(), disk, front, clock, memory)
            .map_err(|f| f.error)
            .unwrap();
        for _ in 0..10_000 {
            if engine.is_serving() {
                break;
            }
            engine.tick().unwrap();
        }
        assert!(engine.is_serving());
        engine
    }

    struct Live {
        engine: BoxEngine,
        device: Box<dyn HostDevice>,
    }

    fn go_live(live: &mut Live) {
        let turns = Cell::new(0u32);
        start_while_serving(
            live,
            |l| l.device.begin_start(),
            |l| l.device.poll_start(),
            |l| {
                turns.set(turns.get() + 1);
                l.engine.tick().map_err(|e| e.to_string())
            },
            || turns.get() > 10_000,
        )
        .expect("live");
    }

    fn wait(live: &mut Live, kernel: &MockKernel, id: u64) -> i32 {
        for _ in 0..100_000 {
            if let Some(done) = kernel.completion(id) {
                return done.result;
            }
            live.engine.tick().unwrap();
        }
        panic!("request {id} never completed");
    }

    #[test]
    fn reissued_requests_land_once_and_no_acknowledged_write_is_lost() {
        let kernel = MockKernel::new();
        let clock = SimClock::new(1_700_000_000_000_000_000);
        let mut disk = SimBackstore::new(
            {
                let mut p = MediaProfile::flash(geometry());
                p.inflight_stripes = 4;
                p
            },
            11,
        )
        .unwrap();
        BoxEngine::format(&mut disk, &clock, &config()).unwrap();
        let mut host_a = kernel.process();
        let mut device = host_a
            .create(&spec(), device_stamp(POOL, VOL))
            .expect("ADD_DEV");
        let door = front(&mut *device);
        let memory = HugeArena::anonymous(2 << 20, HugePageSize::TwoMiB).unwrap();
        let mut live = Live {
            engine: boot(disk, door, clock.clone(), memory),
            device,
        };
        go_live(&mut live);
        let dev_id = live.device.dev_id();
        // Acknowledged writes: in the arena, not in any CP.
        let w1 = kernel.submit_write(dev_id, 0, pattern(0, 2, 1));
        let w2 = kernel.submit_write(dev_id, 10, pattern(10, 1, 2));
        assert_eq!(wait(&mut live, &kernel, w1), 2 * BLOCK_SIZE as i32);
        assert_eq!(wait(&mut live, &kernel, w2), BLOCK_SIZE as i32);
        // Handed to the daemon and not answered when it dies: a new write,
        // an overwrite of an acknowledged one, a read.
        let w3 = kernel.submit_write(dev_id, 20, pattern(20, 1, 3));
        let w4 = kernel.submit_write(dev_id, 0, pattern(0, 1, 4));
        let r = kernel.submit_read(dev_id, 10, 1);
        live.engine.front_door().pump().unwrap();
        assert_eq!(kernel.held(dev_id), 3, "the daemon holds three requests");
        // kill -9: the arena's memory survives (a hugetlbfs file), the rest
        // goes with the process.
        let Live { engine, device } = live;
        let (mut disk, door, clock, memory) = engine.into_parts();
        disk.crash_daemon();
        drop(door);
        drop(device);
        drop(host_a);
        assert_eq!(kernel.info(dev_id).unwrap().state, DevState::Quiesced);
        assert_eq!(kernel.unanswered(dev_id), 3, "requeued, not failed");
        // The new daemon: the queue re-attached first, then the engine,
        // then END_USER_RECOVERY while it serves.
        let mut host_b = kernel.process();
        let mut device = Recovery::new(dev_id, expect(), BUDGET_NS)
            .poll(&mut host_b, 0)
            .unwrap()
            .expect("re-attached");
        let door = front(&mut *device);
        let mut live = Live {
            engine: boot(disk, door, clock, memory),
            device,
        };
        let reattached = live
            .engine
            .take_events()
            .into_iter()
            .find_map(|ev| match ev {
                Event::Booted {
                    reattached, dirty, ..
                } => Some((reattached, dirty)),
                _ => None,
            });
        assert_eq!(
            reattached,
            Some((true, true)),
            "a dirty boot over the kept arena"
        );
        assert!(
            live.engine.nvfail_set().is_empty(),
            "the re-attached arena holds nothing back"
        );
        go_live(&mut live);
        for id in [w3, w4, r] {
            let result = wait(&mut live, &kernel, id);
            assert!(result > 0, "request {id}: {result}");
            assert_eq!(kernel.completion(id).unwrap().deliveries, 2);
        }
        assert_eq!(
            kernel.completion(r).unwrap().data,
            Some(pattern(10, 1, 2)),
            "the reissued read saw the acknowledged write"
        );
        // Every write is there, the reissued overwrite last.
        let read = |live: &mut Live, lba, blocks| {
            let id = kernel.submit_read(dev_id, lba, blocks);
            assert!(wait(live, &kernel, id) > 0);
            kernel.completion(id).unwrap().data.unwrap()
        };
        let mut want = pattern(0, 1, 4);
        want.extend_from_slice(&pattern(0, 2, 1)[BLOCK_SIZE..]);
        assert_eq!(read(&mut live, 0, 2), want);
        assert_eq!(read(&mut live, 10, 1), pattern(10, 1, 2));
        assert_eq!(read(&mut live, 20, 1), pattern(20, 1, 3));
        for id in [w1, w2] {
            assert_eq!(kernel.completion(id).unwrap().deliveries, 1);
        }
        assert_eq!(kernel.info(dev_id).unwrap().pid, host_b.pid() as i32);
    }
}
