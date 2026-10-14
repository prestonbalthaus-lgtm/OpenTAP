//! The Step 3 proofs: crash consistency and the nvfail rules under
//! simulated power cuts, daemon crashes and clean restarts.

use otap_cp::{NvfailIntent, Phase, Status};
use otap_format::NvCause;
use otap_sim::{decode, Decoded, Model, OpKind, Schedule, Sim, SimConfig, Stop, TearPolicy};
use proptest::prelude::*;

fn sim(seed: u64) -> Sim {
    Sim::new(SimConfig::small(seed)).expect("a small pool formats")
}

fn precise(seed: u64) -> Sim {
    let mut cfg = SimConfig::small(seed);
    cfg.intent = NvfailIntent::Precise;
    Sim::new(cfg).expect("a small pool formats")
}

fn ok(sim: &mut Sim, vol: u32, lba: u64, blocks: u32) {
    let c = sim.write(vol, lba, blocks).expect("the write completes");
    assert_eq!(c.status, Status::Ok, "write {vol}/{lba}+{blocks}");
}

#[test]
fn a_formatted_pool_boots_clean_and_reads_zeros() {
    let mut s = sim(1);
    let boot = s.boot().unwrap();
    assert_eq!(boot.cp_seq, 0);
    assert!(!boot.dirty && !boot.rewound && !boot.reattached);
    assert!(boot.nvfail.is_empty());
    assert_eq!(s.engine().cp_seq(), 1, "the boot commit is CP 1");
    let (status, data) = s.read(0, 0, 4).unwrap();
    assert_eq!(status, Status::Ok);
    assert!(data.iter().all(|b| *b == 0));
    let verdict = s.verify_recovery().unwrap();
    assert_eq!(verdict.k, 0);
}

#[test]
fn a_blank_device_does_not_boot() {
    let mut cfg = SimConfig::small(2);
    cfg.segments = 8;
    let mut s = Sim::new(cfg).unwrap();
    // Wipe the ring: no slot verifies.
    let capacity = s.cfg.geometry().capacity_bytes as usize;
    let zeros = vec![0u8; capacity];
    s.disk().corrupt_durable(0, &zeros);
    let err = s.boot().unwrap_err();
    assert!(err.contains("no verified superblock"), "{err}");
}

#[test]
fn writes_are_readable_before_and_after_a_consistency_point() {
    let mut s = sim(3);
    s.boot().unwrap();
    ok(&mut s, 0, 0, 4);
    ok(&mut s, 1, 10, 2);
    assert_eq!(s.read(0, 0, 4).unwrap().0, Status::Ok);
    assert_eq!(s.read(1, 9, 4).unwrap().0, Status::Ok);
    assert_eq!(s.check_disk_prefix().unwrap(), 0, "nothing committed yet");
    let cp = s.request_cp().unwrap();
    assert!(cp >= 2);
    assert_eq!(s.check_disk_prefix().unwrap(), 2);
    assert!(s.engine().stats().stripes > 0);
    ok(&mut s, 0, 1, 1);
    assert_eq!(s.read(0, 0, 4).unwrap().0, Status::Ok);
    assert_eq!(
        s.check_disk_prefix().unwrap(),
        2,
        "the overwrite is still in the arena"
    );
    s.request_cp().unwrap();
    assert_eq!(s.check_disk_prefix().unwrap(), 3);
    assert_eq!(s.read(0, 0, 4).unwrap().0, Status::Ok);
}

/// The test the Step 3 brief asks for: a crash while the superblock commit
/// is pending rolls back to the previous CP with no corruption, whether the
/// slot is dropped, torn or landed whole without being reported.
#[test]
fn a_crash_during_the_commit_rolls_back_to_the_previous_cp() {
    let policies = [
        (
            "dropped",
            TearPolicy {
                drop: 1,
                whole: 0,
                torn: 0,
            },
        ),
        (
            "torn",
            TearPolicy {
                drop: 0,
                whole: 0,
                torn: 1,
            },
        ),
        (
            "landed",
            TearPolicy {
                drop: 0,
                whole: 1,
                torn: 0,
            },
        ),
    ];
    for (name, tear) in policies {
        for phase in [Phase::CommitA, Phase::CommitB] {
            let mut s = sim(7);
            s.boot().unwrap();
            ok(&mut s, 0, 0, 4);
            ok(&mut s, 1, 0, 4);
            let committed_cp = s.request_cp().unwrap();
            ok(&mut s, 0, 4, 4);
            ok(&mut s, 1, 4, 4);
            s.engine().request_cp();
            s.tick_until(
                |e| e.phase() == phase && e.backstore().pending_kinds().contains(&OpKind::Commit),
                "the commit to be pending",
            )
            .unwrap();
            s.disk().set_tear_policy(tear);
            let report = s.crash_power().unwrap();
            assert_eq!(
                report.dropped + report.whole + report.torn,
                1,
                "{name}/{phase:?}: only the commit was unsafe"
            );
            let boot = s.boot().unwrap();
            assert!(boot.dirty, "{name}/{phase:?}");
            let verdict = s.verify_recovery().unwrap();
            let landed = name == "landed" || phase == Phase::CommitB;
            if landed {
                assert_eq!(
                    s.engine().cp_seq(),
                    committed_cp + 2,
                    "{name}/{phase:?}: the new CP plus the boot commit"
                );
                assert_eq!(verdict.k, 4, "{name}/{phase:?}: every write survived");
                assert!(verdict.lost.is_empty());
            } else {
                assert_eq!(
                    s.engine().cp_seq(),
                    committed_cp + 1,
                    "{name}/{phase:?}: back on the previous CP plus the boot commit"
                );
                assert_eq!(
                    verdict.k, 2,
                    "{name}/{phase:?}: exactly the previous CP's writes"
                );
                assert!(verdict.lost.get(0) && verdict.lost.get(1));
            }
            assert!(
                verdict.nvfail.get(0) && verdict.nvfail.get(1),
                "{name}/{phase:?}: conservative mode holds both"
            );
            s.clear_nvfail().unwrap();
            ok(&mut s, 0, 8, 2);
            s.request_cp().unwrap();
            assert_eq!(s.read(0, 0, 10).unwrap().0, Status::Ok);
            assert_eq!(s.read(1, 0, 8).unwrap().0, Status::Ok);
            assert_eq!(s.check_disk_prefix().unwrap(), s.model.last_op());
        }
    }
}

#[test]
fn a_torn_data_stripe_behind_a_complete_superblock_rewinds_one_cp() {
    let mut s = sim(11);
    s.boot().unwrap();
    ok(&mut s, 0, 0, 4);
    let cp_a = s.request_cp().unwrap();
    ok(&mut s, 0, 8, 4);
    let cp_b = s.request_cp().unwrap();
    assert_eq!(cp_b, cp_a + 1);
    // A controller that landed the superblock before the data: damage the
    // summary of the stripe CP B wrote, on the medium.
    let entry = s.engine().map_entry(0, 8).expect("mapped after the CP");
    let layout = *s.engine().layout();
    let idx = layout.stripe_of_off(entry.off());
    let offset = layout.stripe_offset(entry.lseg(), idx).unwrap() as usize;
    s.disk().corrupt_durable(offset + 8, &[0xFF; 8]);
    s.crash_power().unwrap();
    let boot = s.boot().unwrap();
    assert!(boot.rewound, "the newest candidate is torn");
    assert_eq!(boot.cp_seq, cp_a);
    let verdict = s.verify_recovery().unwrap();
    assert_eq!(verdict.k, 1, "only the first write is on disk");
    assert!(verdict.lost.get(0));
    assert_eq!(s.engine().volume(0).unwrap().nvfail, Some(NvCause::TornCp));
    assert_eq!(s.engine().volume(1).unwrap().nvfail, Some(NvCause::TornCp));
}

#[test]
fn a_daemon_crash_re_attaches_the_arena_and_loses_nothing() {
    let mut s = sim(21);
    s.boot().unwrap();
    for i in 0..6u64 {
        ok(&mut s, (i % 2) as u32, i * 4, 3);
    }
    s.request_cp().unwrap();
    for i in 6..12u64 {
        ok(&mut s, (i % 2) as u32, i * 4, 3);
    }
    s.crash_daemon().unwrap();
    let boot = s.boot().unwrap();
    assert!(boot.reattached && boot.dirty, "{boot:?}");
    assert!(boot.nvfail.is_empty());
    let verdict = s.verify_recovery().unwrap();
    assert_eq!(verdict.k, 12);
    assert_eq!(s.read(1, 44, 3).unwrap().0, Status::Ok);
    s.request_cp().unwrap();
    assert_eq!(s.check_disk_prefix().unwrap(), 12);

    // Again, with the crash in the middle of a CP.
    ok(&mut s, 0, 60, 4);
    s.engine().request_cp();
    s.tick_until(|e| e.phase() == Phase::Staging, "staging")
        .unwrap();
    s.crash_daemon().unwrap();
    let boot = s.boot().unwrap();
    assert!(boot.reattached);
    let verdict = s.verify_recovery().unwrap();
    assert_eq!(verdict.k, 13);
    assert!(verdict.nvfail.is_empty());
}

#[test]
fn precise_mode_holds_exactly_the_volumes_with_unflushed_writes() {
    let mut s = precise(31);
    s.boot().unwrap();
    ok(&mut s, 0, 0, 4);
    s.request_cp().unwrap();
    // Two more CPs without a write: volume 0 falls idle.
    s.request_cp().unwrap();
    s.request_cp().unwrap();
    assert_eq!(s.engine().volume(0).unwrap().active_windows, 0);
    ok(&mut s, 1, 0, 4);
    assert!(
        s.engine().stats().intents >= 1,
        "an intent slot preceded the acknowledgement"
    );
    s.crash_power().unwrap();
    let boot = s.boot().unwrap();
    assert!(boot.dirty);
    let verdict = s.verify_recovery().unwrap();
    assert_eq!(verdict.k, 1);
    assert!(verdict.lost.get(1) && !verdict.lost.get(0));
    assert!(
        verdict.nvfail.get(1),
        "the volume that lost a write is held"
    );
    assert!(!verdict.nvfail.get(0), "the idle volume is not");
    assert_eq!(
        s.engine().volume(1).unwrap().nvfail,
        Some(NvCause::DirtyBoot)
    );
}

#[test]
fn conservative_mode_holds_every_writable_volume_on_a_dirty_boot() {
    let mut s = sim(32);
    s.boot().unwrap();
    ok(&mut s, 1, 0, 2);
    s.crash_power().unwrap();
    s.boot().unwrap();
    let verdict = s.verify_recovery().unwrap();
    assert_eq!(verdict.k, 0);
    assert!(verdict.nvfail.get(0) && verdict.nvfail.get(1));
    assert_eq!(
        s.engine().volume(0).unwrap().nvfail,
        Some(NvCause::DirtyBoot)
    );
    assert_eq!(s.read(0, 0, 1).unwrap().0, Status::NvFailed);
    assert_eq!(s.write(1, 0, 1).unwrap().status, Status::NvFailed);
}

#[test]
fn a_clean_shutdown_never_trips_nvfail() {
    let mut s = sim(33);
    s.boot().unwrap();
    ok(&mut s, 0, 0, 4);
    ok(&mut s, 1, 1, 4);
    s.shutdown_clean().unwrap();
    assert_eq!(s.last_stop(), Stop::Clean);
    let boot = s.boot().unwrap();
    assert!(!boot.dirty && boot.nvfail.is_empty());
    let verdict = s.verify_recovery().unwrap();
    assert_eq!(verdict.k, 2);
    // Nothing written: still clean.
    s.shutdown_clean().unwrap();
    let boot = s.boot().unwrap();
    assert!(!boot.dirty);
    assert_eq!(s.verify_recovery().unwrap().k, 2);
}

#[test]
fn nvfail_clear_restores_service_and_bumps_the_rollback_generation() {
    let mut s = sim(34);
    s.boot().unwrap();
    ok(&mut s, 0, 0, 4);
    s.crash_power().unwrap();
    s.boot().unwrap();
    s.verify_recovery().unwrap();
    assert_eq!(s.read(0, 0, 1).unwrap().0, Status::NvFailed);
    assert_eq!(s.engine().volume(0).unwrap().rollback_gen, 0);
    let gen = s.engine().nvfail_clear(0).unwrap();
    assert_eq!(gen, 1);
    assert_eq!(
        s.read(0, 0, 1).unwrap().0,
        Status::NvFailed,
        "held until the clear is durable"
    );
    assert_eq!(s.engine().volume(0).unwrap().clear_pending, Some(1));
    s.request_cp().unwrap();
    assert_eq!(s.read(0, 0, 1).unwrap().0, Status::Ok, "serving again");
    assert_eq!(s.engine().volume(0).unwrap().rollback_gen, 1);
    assert!(s.engine().nvfail_set().get(1), "volume 1 is still held");
    s.crash_daemon().unwrap();
    s.boot().unwrap();
    assert_eq!(
        s.engine().volume(0).unwrap().rollback_gen,
        1,
        "the bump is durable"
    );
    assert_eq!(s.engine().volume(0).unwrap().nvfail, None);
    assert_eq!(
        s.engine().volume(1).unwrap().nvfail,
        Some(NvCause::DirtyBoot)
    );
}

#[test]
fn a_full_arena_defers_writes_until_a_consistency_point_drains_it() {
    let mut cfg = SimConfig::small(41);
    cfg.segments = 96;
    // One large volume, every write to fresh LBAs: nothing is superseded, so
    // the arena really fills.
    cfg.volumes = vec![(0, 4096)];
    let mut s = Sim::new(cfg).unwrap();
    s.boot().unwrap();
    let slots = s.arena_slots().unwrap();
    let per_write = 4u32;
    let n = slots / per_write + 2;
    // Stall the device: no CP can complete, so the arena fills.
    s.disk().set_schedule(Schedule {
        max_per_poll: 0,
        reorder: false,
        queue_depth: 64,
    });
    let base_op = 1_000u64;
    let mut ids = Vec::new();
    for i in 0..n {
        let op = base_op + u64::from(i);
        let lba = u64::from(i) * u64::from(per_write);
        let data = Model::data_of(op, 0, lba, per_write);
        ids.push(s.engine().front_door().submit_write(0, lba, data));
    }
    s.ticks(200).unwrap();
    let completed: Vec<_> = s.engine().front_door().take_completions();
    assert!(
        completed.len() < n as usize,
        "the arena cannot hold every write"
    );
    assert!(completed.iter().all(|c| c.status == Status::Ok));
    assert!(s.engine().stats().deferred > 0, "a write was deferred");
    assert!(s.engine().arena_occupancy() > 0.5);
    assert!(
        s.engine().phase() != Phase::Idle || s.engine().frozen_generations() > 0,
        "a CP was started to drain it"
    );
    // Let the device run again.
    s.disk().set_schedule(Schedule::default());
    let mut all = completed;
    let mut guard = 0;
    while all.len() < n as usize && guard < 20 {
        s.ticks(1_000).unwrap();
        all.extend(s.engine().front_door().take_completions());
        guard += 1;
    }
    assert_eq!(
        all.len(),
        n as usize,
        "every write completed once the arena drained"
    );
    assert!(all.iter().all(|c| c.status == Status::Ok));
    // Every write is readable, from the arena or from disk.
    for i in 0..n {
        let mut buf = vec![0u8; 4096];
        s.engine()
            .peek(0, u64::from(i) * u64::from(per_write), &mut buf)
            .unwrap();
        assert_eq!(
            decode(&buf),
            Decoded::Write {
                op: base_op + u64::from(i),
                k: 0
            },
            "write {i}"
        );
    }
}

#[test]
fn the_cp_timer_and_the_arena_threshold_trigger_consistency_points() {
    let mut cfg = SimConfig::small(42);
    cfg.volumes = vec![(0, 64), (1, 4096)];
    let mut s = Sim::new(cfg).unwrap();
    s.boot().unwrap();
    ok(&mut s, 0, 0, 1);
    let before = s.engine().cp_seq();
    s.advance(6_000_000_000);
    s.tick_until(|e| e.cp_seq() > before, "the timer CP")
        .unwrap();
    assert!(s.events.iter().any(|e| matches!(
        e,
        otap_cp::Event::CpCommitted {
            trigger: otap_cp::CpTrigger::Timer,
            ..
        }
    )));
    // Fill past half the arena with fresh blocks: a CP follows without the
    // timer.
    let slots = s.arena_slots().unwrap();
    let before = s.engine().cp_seq();
    let mut written = 0;
    while written < slots / 2 + 8 {
        ok(&mut s, 1, u64::from(written), 4);
        written += 4;
    }
    s.tick_until(|e| e.cp_seq() > before, "the arena CP")
        .unwrap();
    assert!(s.events.iter().any(|e| matches!(
        e,
        otap_cp::Event::CpCommitted {
            trigger: otap_cp::CpTrigger::ArenaHalf,
            ..
        }
    )));
    assert!(s.check_disk_prefix().unwrap() > 0);
}

#[test]
fn the_loopback_net_delivers_frames_in_order_and_holds_them_while_partitioned() {
    use otap_cp::{EndpointId, Net};
    use otap_stream::{Decoder, Encoder, Frame, FrameMeta, Ping};

    let net = otap_sim::LoopbackNet::new(2);
    let (a, b) = (EndpointId(0), EndpointId(1));
    let mut end_a = net.end(a);
    let mut end_b = net.end(b);
    let meta = FrameMeta::default();
    let frames: Vec<Vec<u8>> = (1..=3u64)
        .map(|n| {
            Encoder::new()
                .encode(
                    &meta,
                    &Frame::Ping(Ping {
                        nonce: n,
                        mono_ns: n * 10,
                    }),
                )
                .unwrap()
                .to_vec()
        })
        .collect();
    for f in &frames {
        end_a.send(b, f).unwrap();
    }
    let mut got = Vec::new();
    assert_eq!(end_b.poll_recv(&mut got), 0, "not before the delay");
    net.tick();
    assert_eq!(end_b.poll_recv(&mut got), 3);
    let mut decoder = Decoder::new();
    for (from, bytes) in &got {
        assert_eq!(*from, a);
        decoder.feed(bytes);
    }
    let decoded = decoder.drain().unwrap();
    let nonces: Vec<u64> = decoded
        .into_iter()
        .map(|raw| match raw.decode().unwrap().1 {
            Frame::Ping(p) => p.nonce,
            other => panic!("{other:?}"),
        })
        .collect();
    assert_eq!(nonces, vec![1, 2, 3]);

    net.set_partitioned(true);
    end_a.send(b, &frames[0]).unwrap();
    net.tick();
    assert_eq!(end_b.poll_recv(&mut got), 0, "held by the partition");
    assert_eq!(net.held(), 1);
    net.set_partitioned(false);
    end_b.send(a, &frames[1]).unwrap();
    net.tick();
    let mut back = Vec::new();
    assert_eq!(end_a.poll_recv(&mut back), 1);
    assert_eq!(back[0].0, b);
    let mut late = Vec::new();
    assert_eq!(end_b.poll_recv(&mut late), 1, "delivered once healed");
    assert_eq!(late[0].1, frames[0]);

    net.set_queue_limit(1);
    end_a.send(b, &frames[0]).unwrap();
    assert!(end_a.send(b, &frames[0]).is_err(), "the inbox is bounded");
}

// ---------------------------------------------------------------------------
// Random I/O with random crashes
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
enum Op {
    Write { vol: u8, lba: u8, blocks: u8 },
    Read { vol: u8, lba: u8, blocks: u8 },
    Ticks(u8),
    Advance(u8),
    RequestCp,
    CrashPower,
    CrashDaemon,
    CleanRestart,
}

fn op() -> impl Strategy<Value = Op> {
    prop_oneof![
        5 => (any::<u8>(), any::<u8>(), 1u8..=4).prop_map(|(vol, lba, blocks)| Op::Write { vol, lba, blocks }),
        3 => (any::<u8>(), any::<u8>(), 1u8..=4).prop_map(|(vol, lba, blocks)| Op::Read { vol, lba, blocks }),
        2 => (1u8..=30).prop_map(Op::Ticks),
        2 => (0u8..=7).prop_map(Op::Advance),
        2 => Just(Op::RequestCp),
        1 => Just(Op::CrashPower),
        1 => Just(Op::CrashDaemon),
        1 => Just(Op::CleanRestart),
    ]
}

#[derive(Debug, Clone)]
struct Scenario {
    seed: u64,
    precise: bool,
    ops: Vec<Op>,
}

fn scenario() -> impl Strategy<Value = Scenario> {
    (
        any::<u64>(),
        any::<bool>(),
        prop::collection::vec(op(), 5..40),
    )
        .prop_map(|(seed, precise, ops)| Scenario { seed, precise, ops })
}

const VOL_BLOCKS: u64 = 48;

fn run(scenario: &Scenario) -> Result<(), String> {
    let mut cfg = SimConfig::small(scenario.seed);
    cfg.volumes = vec![(0, VOL_BLOCKS), (1, VOL_BLOCKS)];
    cfg.intent = if scenario.precise {
        NvfailIntent::Precise
    } else {
        NvfailIntent::Conservative
    };
    let mut s = Sim::new(cfg)?;
    s.boot()?;
    s.verify_recovery()?;
    for (i, op) in scenario.ops.iter().enumerate() {
        let step = |e: String| format!("op {i} {op:?}: {e}");
        match *op {
            Op::Write { vol, lba, blocks } => {
                let vol = u32::from(vol % 2);
                let blocks = u32::from(blocks);
                let lba = u64::from(lba) % (VOL_BLOCKS - u64::from(blocks) + 1);
                let held = s.engine().nvfail_set().get(vol);
                let c = s.write(vol, lba, blocks).map_err(step)?;
                let expected = if held { Status::NvFailed } else { Status::Ok };
                if c.status != expected {
                    return Err(step(format!(
                        "status {:?}, expected {expected:?}",
                        c.status
                    )));
                }
            }
            Op::Read { vol, lba, blocks } => {
                let vol = u32::from(vol % 2);
                let blocks = u32::from(blocks);
                let lba = u64::from(lba) % (VOL_BLOCKS - u64::from(blocks) + 1);
                let held = s.engine().nvfail_set().get(vol);
                let (status, _) = s.read(vol, lba, blocks).map_err(step)?;
                let expected = if held { Status::NvFailed } else { Status::Ok };
                if status != expected {
                    return Err(step(format!("status {status:?}, expected {expected:?}")));
                }
            }
            Op::Ticks(n) => s.ticks(u32::from(n)).map_err(step)?,
            Op::Advance(secs) => s.advance(u64::from(secs) * 1_000_000_000),
            Op::RequestCp => {
                s.request_cp().map_err(step)?;
            }
            Op::CrashPower => {
                s.crash_power().map_err(step)?;
                s.boot().map_err(step)?;
                s.verify_recovery().map_err(step)?;
                s.clear_nvfail().map_err(step)?;
            }
            Op::CrashDaemon => {
                s.crash_daemon().map_err(step)?;
                s.boot().map_err(step)?;
                s.verify_recovery().map_err(step)?;
            }
            Op::CleanRestart => {
                s.shutdown_clean().map_err(step)?;
                s.boot().map_err(step)?;
                s.verify_recovery().map_err(step)?;
            }
        }
        // The committed image is a prefix at every moment.
        s.check_disk_prefix().map_err(step)?;
    }
    s.shutdown_clean()?;
    s.boot()?;
    let verdict = s.verify_recovery()?;
    if verdict.k != s.model.last_op() {
        return Err(format!(
            "final restart: prefix {}, last op {}",
            verdict.k,
            s.model.last_op()
        ));
    }
    Ok(())
}

proptest! {
    #![proptest_config(ProptestConfig {
        failure_persistence: None,
        max_shrink_iters: 2048,
        ..ProptestConfig::default()
    })]

    #[test]
    fn random_io_with_random_crashes_recovers_a_prefix_and_the_right_nvfail_set(scenario in scenario()) {
        if let Err(e) = run(&scenario) {
            prop_assert!(false, "{e}");
        }
    }
}

/// Found by the random-crash property test at 10,000 cases: while a commit
/// that drops a volume from `write_active` was in flight, the engine still
/// treated the volume as durably active and acknowledged a write to it
/// without an intent slot; a power cut then lost the write and the dirty
/// boot did not hold the volume. The engine now narrows its view of what the
/// disk guarantees as soon as the slot is built.
#[test]
fn a_write_during_a_commit_that_drops_its_volume_still_waits_for_an_intent() {
    let mut s = precise(51);
    s.boot().unwrap();
    ok(&mut s, 1, 0, 1);
    s.request_cp().unwrap();
    s.request_cp().unwrap();
    assert_eq!(s.engine().volume(1).unwrap().active_windows, 1);
    ok(&mut s, 0, 0, 1);
    s.engine().request_cp();
    s.tick_until(
        |e| e.phase() == Phase::CommitA && e.backstore().pending_kinds().contains(&OpKind::Commit),
        "the commit that drops volume 1",
    )
    .unwrap();
    let intents_before = s.engine().stats().intents;
    ok(&mut s, 1, 0, 1);
    assert!(
        s.engine().stats().intents > intents_before,
        "the write to the dropped volume was preceded by an intent slot"
    );
    s.crash_power().unwrap();
    s.boot().unwrap();
    let verdict = s.verify_recovery().unwrap();
    assert!(
        verdict.lost.get(1),
        "the write to volume 1 was lost with the arena"
    );
    assert!(verdict.nvfail.get(1), "and the dirty boot holds volume 1");
}
