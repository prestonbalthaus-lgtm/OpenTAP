//! Step 6A: the ublk device start handshake, over the mock queue.
//!
//! In 6.12 `UBLK_CMD_START_DEV` returns only after every tag has a
//! processed fetch and after `add_disk`, whose partition scan (for a
//! privileged server, which `dbladed` as root is) reads the new disk
//! through the queue being started and waits for the read
//! (`ublk_ctrl_start_dev`, `ublk_mark_io_ready` and the
//! `GD_SUPPRESS_PART_SCAN` test in the saved `ublk_drv.c`). Step 5 issued
//! the command on the thread that serves the queue and waited for it there,
//! so that read was never answered and device creation never returned. The
//! fix issues `START_DEV` without waiting and polls it between turns of the
//! serving loop ([`start_while_serving`]). [`MockStart`] models the two
//! waits over a [`MockTransport`]; the Linux device type-checks the same
//! trait and the daemon drives it with the same function.

use std::cell::Cell;

use otap_cp::{Engine, EngineConfig, Geometry, MediaProfile, VolumeSpec};
use otap_format::{Uuid, BLOCK_SIZE, INTENT_SLOT_SIZE, SUPERBLOCK_SLOT_SIZE};
use otap_sim::{SimBackstore, SimClock};
use otap_sys::{HugeArena, HugePageSize};
use otap_ublk::{
    start_while_serving, DeviceControl, MockStart, MockTransport, StartError, TransportError,
    UblkFrontDoor, Watermarks, EINVAL,
};

const STRIPE: u32 = 32 * 1024;
const SPS: u64 = 8;
const SEGMENTS: u64 = 96;
const DEPTH: u16 = 8;
const VOL_BLOCKS: u64 = 256;

type UblkEngine = Engine<SimBackstore, UblkFrontDoor<MockTransport>, SimClock>;

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

/// The device as `create` leaves it: the queue's fetches submitted and its
/// front door in the engine, the start not yet issued.
struct Rig {
    engine: UblkEngine,
    start: MockStart,
}

impl Rig {
    fn new(link: bool) -> Self {
        let start = MockStart::new();
        let mut transport = MockTransport::new(DEPTH);
        if link {
            start.link(&mut transport);
        }
        let mut cfg = EngineConfig::new(
            Uuid::from_bytes([0x6C; 16]),
            vec![VolumeSpec {
                vol: 0,
                size_blocks: VOL_BLOCKS,
            }],
        );
        cfg.pacing.max_delay_ns = 0;
        let clock = SimClock::new(1_700_000_000_000_000_000);
        let mut profile = MediaProfile::flash(geometry());
        profile.inflight_stripes = 4;
        let mut disk = SimBackstore::new(profile, 9).unwrap();
        UblkEngine::format(&mut disk, &clock, &cfg).unwrap();
        let front = UblkFrontDoor::new(
            transport,
            0,
            0,
            VOL_BLOCKS,
            DEPTH,
            Watermarks::for_depth(DEPTH),
        );
        let memory = HugeArena::anonymous(2 << 20, HugePageSize::TwoMiB).unwrap();
        let mut engine = UblkEngine::boot(cfg, disk, front, clock, memory)
            .map_err(|f| f.error)
            .unwrap();
        for _ in 0..10_000 {
            if engine.is_serving() {
                break;
            }
            engine.tick().unwrap();
        }
        assert!(engine.is_serving());
        Self { engine, start }
    }

    /// The fixed order: `START_DEV` in flight while the engine ticks.
    fn start_serving(&mut self, turns: u32) -> Result<u32, StartError<String>> {
        let served = Cell::new(0u32);
        start_while_serving(
            self,
            |r| r.start.begin_start(),
            |r| r.start.poll_start(),
            |r| {
                served.set(served.get() + 1);
                r.engine.tick().map_err(|e| e.to_string())
            },
            || served.get() >= turns,
        )
        .map(|()| served.get())
    }
}

#[test]
fn start_dev_completes_while_its_queue_is_served() {
    let mut rig = Rig::new(true);
    let turns = rig.start_serving(1_000).expect("the device goes live");
    assert!(rig.start.is_live());
    assert_eq!(
        rig.start.scan_result(),
        Some(BLOCK_SIZE as i32),
        "the partition scan's read was answered with a block"
    );
    assert!(turns >= 1, "it took serving: {turns} turns");
    // The device serves clients from here on.
    let t = rig.engine.front_door().transport();
    t.submit_read(1, 3, 1);
    let mut got = None;
    for _ in 0..1_000 {
        rig.engine.tick().unwrap();
        got = rig.engine.front_door().transport().take_commit(1);
        if got.is_some() {
            break;
        }
    }
    assert_eq!(got, Some(BLOCK_SIZE as i32));
}

/// The Step 5 order, reproduced: `START_DEV` issued, then waited for on
/// the serving thread before the queue is served again. The start never
/// completes, because the read it waits for is never answered; serving
/// releases it at once.
#[test]
fn start_dev_waited_for_on_the_serving_thread_never_completes() {
    let mut rig = Rig::new(true);
    rig.start.begin_start().unwrap();
    for _ in 0..100_000 {
        assert_eq!(rig.start.poll_start(), Ok(false), "a wait that cannot end");
    }
    assert!(rig.start.in_flight());
    assert_eq!(
        rig.start.scan_result(),
        None,
        "the scan's read was never served"
    );
    // The fix's order from here: serve while polling.
    let served = Cell::new(0u32);
    start_while_serving(
        &mut rig,
        |_| Ok(()),
        |r| r.start.poll_start(),
        |r| {
            served.set(served.get() + 1);
            r.engine.tick().map_err(|e| e.to_string())
        },
        || served.get() >= 1_000,
    )
    .expect("serving the queue lets the start finish");
    assert!(rig.start.is_live());
}

/// The driver's first wait: with no queue fetched, the start does not
/// complete however long the loop serves, and the caller gives up.
#[test]
fn a_start_with_no_queue_fetched_times_out() {
    let mut rig = Rig::new(false);
    assert_eq!(rig.start_serving(200), Err(StartError::TimedOut));
    assert!(rig.start.in_flight());
}

/// A start the driver refuses is reported with its errno and leaves
/// nothing in flight.
#[test]
fn a_failed_start_reports_its_errno() {
    let mut rig = Rig::new(true);
    rig.start.fail_with(EINVAL);
    assert_eq!(
        rig.start_serving(1_000),
        Err(StartError::Start(TransportError {
            what: "START_DEV",
            errno: EINVAL
        }))
    );
    assert!(!rig.start.is_live());
    assert!(!rig.start.in_flight());
}

/// A serving loop that fails stops the start with its error.
#[test]
fn a_serving_failure_ends_the_start() {
    let mut rig = Rig::new(true);
    let result = start_while_serving(
        &mut rig,
        |r| r.start.begin_start(),
        |r| r.start.poll_start(),
        |_| Err("the engine stopped"),
        || false,
    );
    assert_eq!(result, Err(StartError::Serve("the engine stopped")));
}

/// A start issued while every tag holds a client request: the scan's read
/// waits for a tag, which serving frees.
#[test]
fn the_scan_waits_for_a_free_tag() {
    let mut rig = Rig::new(true);
    for tag in 0..DEPTH {
        rig.engine
            .front_door()
            .transport()
            .submit_read(tag, u64::from(tag), 1);
    }
    let turns = rig.start_serving(1_000).expect("the device goes live");
    assert!(rig.start.is_live(), "{turns} turns");
    assert_eq!(rig.start.scan_result(), Some(BLOCK_SIZE as i32));
    let commits = rig.engine.front_door().transport().take_commits();
    assert_eq!(
        commits.len(),
        usize::from(DEPTH),
        "every client read answered"
    );
}

/// A second `START_DEV` on a live device is refused (`EEXIST`, as the
/// driver answers).
#[test]
fn a_live_device_is_not_started_twice() {
    let mut rig = Rig::new(true);
    rig.start_serving(1_000).unwrap();
    let again = rig.start.begin_start();
    assert_eq!(again.map_err(|e| e.errno), Err(17));
}
