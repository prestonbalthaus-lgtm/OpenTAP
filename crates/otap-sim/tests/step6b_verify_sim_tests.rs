//! Step 6B: the pool attestation (`otapctl pool verify`) over the
//! simulator's device. A healthy pool attests clean after every kind of
//! workload the engine runs (writes, punches, snapshots and their
//! deletion, rollbacks, cleaning, crashes of every kind); every kind of
//! damage injected into the device is found, with its kind and address;
//! the verification runs in slices while writes and consistency points go
//! on, holds the deferred frees until it ends, gives way to a pool that
//! needs the space, and can be cancelled.

use otap_cp::{
    CleanerPolicy, Event, FindingKind, OperatorWarning, Severity, Status, Verdict, VerifyOptions,
    VerifyReport, VerifyState,
};
use otap_format::{SbSlot, BLOCK_SIZE};
use otap_sim::{Sim, SimConfig, TearPolicy};
use proptest::prelude::*;

fn small(seed: u64) -> SimConfig {
    let mut cfg = SimConfig::small(seed);
    cfg.volumes = vec![(0, 160), (1, 96)];
    cfg
}

fn sim_with(cfg: SimConfig) -> Sim {
    Sim::new(cfg).expect("the pool formats")
}

fn ok(s: &mut Sim, vol: u32, lba: u64, blocks: u32) {
    let c = s.write(vol, lba, blocks).expect("the write completes");
    assert_eq!(c.status, Status::Ok, "write {vol}/{lba}+{blocks}");
}

fn punch_ok(s: &mut Sim, vol: u32, lba: u64, blocks: u32) {
    let c = s
        .punch(vol, lba, blocks, false)
        .expect("the punch completes");
    assert_eq!(c.status, Status::Ok, "punch {vol}/{lba}+{blocks}");
}

/// A verification with slices small enough that many CP ticks interleave.
fn attest(s: &mut Sim) -> VerifyReport {
    s.attest_with(VerifyOptions {
        units_per_tick: 32,
        max_ns_per_tick: 0,
    })
    .expect("the verification runs")
}

fn kinds(report: &VerifyReport) -> Vec<FindingKind> {
    report.findings.iter().map(|f| f.kind).collect()
}

/// The healthy workload most tests start from: two volumes written and
/// overwritten, a punch, two snapshots of volume 0, one of volume 1, and
/// enough consistency points for deltas, folds and frees.
fn workload(s: &mut Sim) -> (u64, u64) {
    ok(s, 0, 0, 24);
    ok(s, 1, 0, 16);
    let first = s.snapshot(0).expect("snapshot");
    ok(s, 0, 4, 8);
    punch_ok(s, 0, 20, 4);
    ok(s, 1, 8, 8);
    s.request_cp().expect("cp");
    let second = s.snapshot(0).expect("snapshot");
    ok(s, 0, 40, 30);
    s.snapshot(1).expect("snapshot");
    ok(s, 1, 20, 12);
    s.request_cp().expect("cp");
    (first, second)
}

#[test]
fn a_healthy_pool_attests_clean_through_every_kind_of_stop() {
    let mut s = sim_with(small(6100));
    s.boot().unwrap();
    s.attest_clean().unwrap();
    let (first, _) = workload(&mut s);
    let report = s.attest_clean().unwrap();
    assert_eq!(report.counts.superblock_slots, 4);
    assert!(report.counts.blocks > 0 && report.counts.map_pages > 0);
    assert!(report.counts.holes >= 4, "{report:?}");
    assert!(report.counts.segments > 0 && report.counts.stripes > 0);
    s.delete_snapshot(first).unwrap();
    s.request_fold().unwrap();
    s.attest_clean().unwrap();
    s.crash_daemon().unwrap();
    s.boot().unwrap();
    s.verify_recovery().unwrap();
    s.attest_clean().unwrap();
    ok(&mut s, 0, 100, 20);
    s.disk().set_tear_policy(TearPolicy::default());
    s.crash_power().unwrap();
    s.boot().unwrap();
    s.verify_recovery().unwrap();
    s.clear_nvfail().unwrap();
    s.attest_clean().unwrap();
    s.shutdown_clean().unwrap();
    s.boot().unwrap();
    s.verify_recovery().unwrap();
    s.attest_clean().unwrap();
}

#[test]
fn relocation_by_the_cleaner_raises_no_false_finding() {
    let mut cfg = small(6110);
    cfg.cleaner = CleanerPolicy {
        utilization: 0.0,
        max_segments_per_cp: 4,
        background: true,
        margin_segments: 4,
        max_live_fraction: 0.9,
    };
    let mut s = sim_with(cfg);
    s.boot().unwrap();
    workload(&mut s);
    // Overwrites leave the early segments mostly dead: the cleaner moves
    // what the live map and the snapshots still reference.
    for round in 0..6u64 {
        ok(&mut s, 0, round * 3, 6);
        ok(&mut s, 1, round, 4);
        s.request_cp().unwrap();
    }
    s.request_clean().unwrap();
    s.request_clean().unwrap();
    let relocated = s.engine().stats().relocated;
    assert!(relocated > 0, "the cleaner relocated blocks");
    let report = s.attest_clean().unwrap();
    assert!(report.counts.blocks > 0);
    s.crash_daemon().unwrap();
    s.boot().unwrap();
    s.verify_recovery().unwrap();
    s.attest_clean().unwrap();
}

#[test]
fn a_flipped_payload_byte_is_found_at_its_address() {
    let mut s = sim_with(small(6120));
    s.boot().unwrap();
    workload(&mut s);
    let entry = s.engine().map_entry(1, 9).expect("mapped");
    let layout = *s.engine().layout();
    let pba = layout.block_pba(entry.lseg(), entry.off()).unwrap();
    let at = pba as usize * BLOCK_SIZE + 100;
    let flipped = s.disk().durable()[at] ^ 0x40;
    s.disk().corrupt_durable(at, &[flipped]);
    let report = attest(&mut s);
    assert_eq!(report.verdict, Some(Verdict::Damaged), "{report:?}");
    let f = report
        .findings
        .iter()
        .find(|f| f.kind == FindingKind::BlockCrc)
        .expect("a block_crc finding");
    assert_eq!(f.severity, Severity::Error);
    assert_eq!(
        (f.pba, f.pseg, f.vol, f.lba),
        (Some(pba), Some(entry.lseg()), Some(1), Some(9))
    );
    assert_eq!(report.errors, 1, "{:?}", report.findings);
    // The pool status carries the finding.
    let warning = s
        .engine()
        .pool_status()
        .warnings
        .into_iter()
        .find(|w| matches!(w, OperatorWarning::AttestationFindings { .. }));
    assert!(
        matches!(
            warning,
            Some(OperatorWarning::AttestationFindings { errors: 1, .. })
        ),
        "{warning:?}"
    );
}

#[test]
fn a_damaged_map_page_is_found() {
    let mut s = sim_with(small(6130));
    s.boot().unwrap();
    workload(&mut s);
    s.request_fold().unwrap();
    let entry = s.engine().map().volume(0).unwrap().index()[0];
    assert!(!entry.is_none());
    s.disk()
        .corrupt_durable(entry.page_pba as usize * BLOCK_SIZE + 4000, &[0xEE; 3]);
    let report = attest(&mut s);
    assert_eq!(report.verdict, Some(Verdict::Damaged), "{report:?}");
    let f = report
        .findings
        .iter()
        .find(|f| f.kind == FindingKind::MapPage)
        .expect("a map_page finding");
    assert_eq!((f.pba, f.vol), (Some(entry.page_pba), Some(0)));
}

#[test]
fn a_damaged_stripe_summary_is_found() {
    let mut s = sim_with(small(6140));
    s.boot().unwrap();
    workload(&mut s);
    let entry = s.engine().map_entry(0, 50).expect("mapped");
    let layout = *s.engine().layout();
    let idx = entry.off() / layout.blocks_per_stripe();
    let pba = layout.stripe_pba(entry.lseg(), idx).unwrap();
    // A descriptor byte inside the summary: its CRC no longer holds.
    s.disk()
        .corrupt_durable(pba as usize * BLOCK_SIZE + 200, &[0x77]);
    let report = attest(&mut s);
    assert_eq!(report.verdict, Some(Verdict::Damaged), "{report:?}");
    let f = report
        .findings
        .iter()
        .find(|f| f.kind == FindingKind::StripeSummary)
        .expect("a stripe_summary finding");
    assert_eq!((f.pba, f.pseg), (Some(pba), Some(entry.lseg())));
}

#[test]
fn a_damaged_superblock_slot_is_found() {
    let mut s = sim_with(small(6150));
    s.boot().unwrap();
    workload(&mut s);
    let layout = *s.engine().layout();
    // The slot of the commit before the last: the boot would rank past it,
    // the verification must still say it is gone.
    let cp = s.engine().cp_seq();
    let [older_a, _] = SbSlot::for_commit(cp - 1);
    let pba = layout.sb_pba(older_a);
    let at = pba as usize * BLOCK_SIZE + 64;
    let flipped = s.disk().durable()[at] ^ 0x01;
    s.disk().corrupt_durable(at, &[flipped]);
    let report = attest(&mut s);
    assert_eq!(report.verdict, Some(Verdict::Damaged), "{report:?}");
    let f = report
        .findings
        .iter()
        .find(|f| f.kind == FindingKind::SuperblockSlot)
        .expect("a superblock_slot finding");
    assert_eq!(f.pba, Some(pba));
}

#[test]
fn a_live_bytes_drift_is_found_on_its_segment() {
    let mut s = sim_with(small(6160));
    s.boot().unwrap();
    workload(&mut s);
    let entry = s.engine().map_entry(0, 2).expect("mapped");
    let pseg = entry.lseg();
    let before = s.engine().live_blocks(pseg);
    s.engine().skew_live_blocks(pseg, 3);
    s.request_cp().unwrap();
    assert_eq!(s.engine().live_blocks(pseg), before + 3);
    let report = attest(&mut s);
    let f = report
        .findings
        .iter()
        .find(|f| f.kind == FindingKind::LiveBlocks)
        .expect("a live_blocks finding");
    assert_eq!(f.pseg, Some(pseg));
    // Counted above what the trees reach: a leak, not data at risk.
    assert_eq!(f.severity, Severity::Warning);
    assert_eq!(report.verdict, Some(Verdict::Warnings), "{report:?}");
    // Counted below: the pool could free referenced blocks.
    s.engine().skew_live_blocks(pseg, -5);
    s.request_cp().unwrap();
    let report = attest(&mut s);
    let f = report
        .findings
        .iter()
        .find(|f| f.kind == FindingKind::LiveBlocks)
        .expect("a live_blocks finding");
    assert_eq!((f.pseg, f.severity), (Some(pseg), Severity::Error));
    assert_eq!(report.verdict, Some(Verdict::Damaged));
}

#[test]
fn a_quarantined_snapshot_is_reported() {
    let mut s = sim_with(small(6170));
    s.boot().unwrap();
    let (first, _) = workload(&mut s);
    // Damage a page only the first snapshot holds, then read the snapshot:
    // the read quarantines it (Step 6A).
    let pinned_page = {
        let records = s.engine().snapshot_records();
        let rec = records.iter().find(|r| r.snap_id == first).unwrap();
        let at = rec.map_root_pba as usize * BLOCK_SIZE;
        let page = s.disk().durable()[at..at + BLOCK_SIZE].to_vec();
        let root = otap_format::SnapRootPage::read(&page).unwrap();
        let at = root.root.index_pba as usize * BLOCK_SIZE;
        let index = s.disk().durable()[at..at + root.root.index_bytes() as usize].to_vec();
        root.root.verify_index(&index).unwrap();
        <otap_format::PageIndexEntry as otap_format::FromBytes>::read_from_bytes(&index[..16])
            .unwrap()
    };
    let live = s.engine().map().volume(0).unwrap().index()[0];
    assert_ne!(
        pinned_page.page_pba, live.page_pba,
        "the snapshot's page is its own"
    );
    s.disk()
        .corrupt_durable(pinned_page.page_pba as usize * BLOCK_SIZE + 64, &[0xAB; 2]);
    // A restart empties the page cache; the boot checks the snapshot's root
    // and index only, so the first read of the page finds the damage.
    s.crash_daemon().unwrap();
    s.boot().unwrap();
    let mut buf = vec![0u8; BLOCK_SIZE];
    assert!(s.engine().snapshot_read(first, 0, &mut buf).is_err());
    let report = attest(&mut s);
    let quarantined = report
        .findings
        .iter()
        .find(|f| f.kind == FindingKind::SnapshotQuarantined)
        .expect("the quarantine is reported");
    assert_eq!(quarantined.snap_id, Some(first));
    assert!(kinds(&report).contains(&FindingKind::MapPage), "{report:?}");
}

#[test]
fn verification_serves_writes_meanwhile_and_holds_the_frees_it_walks() {
    let mut cfg = small(6180);
    cfg.cleaner.background = false;
    let mut s = sim_with(cfg);
    s.boot().unwrap();
    // Both volumes written whole, no snapshot: rewriting volume 1 later
    // kills its segments outright.
    for lba in (0..160u64).step_by(8) {
        ok(&mut s, 0, lba, 8);
    }
    for lba in (0..96u64).step_by(8) {
        ok(&mut s, 1, lba, 8);
    }
    s.request_cp().unwrap();
    s.engine()
        .start_verify(VerifyOptions {
            units_per_tick: 1,
            max_ns_per_tick: 0,
        })
        .unwrap();
    let attested = loop {
        s.tick().unwrap();
        if let Some(r) = s.engine().verify_report() {
            if r.cp_seq > 0 {
                break r.cp_seq;
            }
        }
    };
    // The data segments CP N references.
    let referenced: Vec<u64> = (1..=s.cfg.segments as u64)
        .filter(|p| s.engine().live_blocks(*p) > 0)
        .collect();
    assert!(!referenced.is_empty());
    let seen = s.events.len();
    // Volume 1 rewritten whole twice, with consistency points, while the
    // verification walks CP N: its old segments die, and stay where they
    // are until the verification ends.
    let mut rounds = 0;
    while s.engine().verify_running() && rounds < 2 {
        for lba in (0..96u64).step_by(8) {
            ok(&mut s, 1, lba, 8);
        }
        s.request_cp().unwrap();
        rounds += 1;
    }
    assert_eq!(rounds, 2, "the verification ran through both rewrites");
    assert!(s.engine().verify_running());
    let freed_during: Vec<u64> = s.events[seen..]
        .iter()
        .filter_map(|e| match e {
            Event::SegmentFreed { pseg, .. } => Some(*pseg),
            _ => None,
        })
        .collect();
    assert!(
        freed_during.iter().all(|p| !referenced.contains(p)),
        "segments {freed_during:?} that CP {attested} references were freed under the \
         verification"
    );
    s.tick_until(|e| !e.verify_running(), "the verification")
        .unwrap();
    let report = s.engine().verify_report().unwrap();
    assert_eq!(report.cp_seq, attested);
    assert_eq!(report.verdict, Some(Verdict::Clean), "{report:?}");
    // Once it ended, the held frees go: volume 1's old segments are freed.
    let seen = s.events.len();
    for _ in 0..3 {
        s.request_cp().unwrap();
    }
    let freed_after: Vec<u64> = s.events[seen..]
        .iter()
        .filter_map(|e| match e {
            Event::SegmentFreed { pseg, .. } => Some(*pseg),
            _ => None,
        })
        .collect();
    let released_since: Vec<u64> = s
        .events
        .iter()
        .filter_map(|e| match e {
            Event::SegmentFreed { pseg, .. } if referenced.contains(pseg) => Some(*pseg),
            _ => None,
        })
        .collect();
    assert!(
        !released_since.is_empty(),
        "the segments the verification held were released (freed after: {freed_after:?})"
    );
    s.attest_clean().unwrap();
    s.check_disk_prefix().unwrap();
}

#[test]
fn a_verification_can_be_cancelled_and_gives_way_to_a_pool_that_needs_space() {
    let mut s = sim_with(small(6190));
    s.boot().unwrap();
    workload(&mut s);
    s.engine()
        .start_verify(VerifyOptions {
            units_per_tick: 1,
            max_ns_per_tick: 0,
        })
        .unwrap();
    s.ticks(3).unwrap();
    let report = s.engine().cancel_verify().expect("a verification ran");
    assert_eq!(report.state, VerifyState::Cancelled);
    assert_eq!(report.verdict, Some(Verdict::Incomplete));
    assert!(!s.engine().verify_running());
    assert!(s.engine().cancel_verify().is_none());
    s.tick().unwrap();
    assert!(s.events.iter().any(|e| matches!(
        e,
        Event::VerifyFinished {
            state: VerifyState::Cancelled,
            ..
        }
    )));
    // A pool down to its reserve and margin needs the frees a verification
    // holds: the verification gives way, and the pool goes on.
    let mut cfg = small(6191);
    cfg.segments = 24;
    cfg.volumes = vec![(0, 400), (1, 96)];
    cfg.cleaner.background = false;
    let mut s = sim_with(cfg);
    s.boot().unwrap();
    for lba in (0..400u64).step_by(8) {
        ok(&mut s, 0, lba, 8);
    }
    s.request_cp().unwrap();
    s.engine()
        .start_verify(VerifyOptions {
            units_per_tick: 1,
            max_ns_per_tick: 0,
        })
        .unwrap();
    let mut aborted = None;
    for round in 0..2000u64 {
        let c = s.write(1, (round * 8) % 96, 8).unwrap();
        if c.status != Status::Ok {
            break;
        }
        s.request_cp().unwrap();
        if let Some(r) = s.engine().verify_report() {
            if r.state != VerifyState::Running {
                aborted = Some(r);
                break;
            }
        }
    }
    let report = aborted.expect("the verification ended");
    assert_eq!(report.state, VerifyState::Aborted, "{report:?}");
    assert!(
        report
            .reason
            .as_deref()
            .is_some_and(|r| r.contains("tight")),
        "{report:?}"
    );
    assert!(!s.engine().verify_running());
    // The held frees go and writes are admitted again.
    for _ in 0..4 {
        s.request_cp().unwrap();
    }
    ok(&mut s, 1, 0, 8);
    s.attest_clean().unwrap();
}

/// A record of the catalogue's chain damaged while the pool runs: the
/// verification finds it, the engine folds the table at its next CP (the
/// repair), and the next verification is clean.
#[test]
fn a_damaged_table_chain_is_found_and_the_next_fold_repairs_it() {
    let mut s = sim_with(small(6197));
    s.boot().unwrap();
    s.engine().set_volume_name(0, "zero").unwrap();
    s.request_cp().unwrap();
    s.engine().set_volume_name(1, "one").unwrap();
    s.request_cp().unwrap();
    assert_eq!(s.engine().pool_status().catalog_deltas, 1);
    let delta = s.engine().table_chains().1.deltas[0];
    let at = delta.region.pba as usize * BLOCK_SIZE + 64 + 40;
    let flipped = s.disk().durable()[at] ^ 0x22;
    s.disk().corrupt_durable(at, &[flipped]);
    let report = attest(&mut s);
    assert_eq!(report.verdict, Some(Verdict::Damaged), "{report:?}");
    assert!(
        kinds(&report).contains(&FindingKind::TableChain),
        "{report:?}"
    );
    // The repair rides the CP the verification asked for.
    s.tick_until(|e| e.is_quiet(), "the repair").unwrap();
    s.request_cp().unwrap();
    assert_eq!(s.engine().pool_status().catalog_deltas, 0);
    s.attest_clean().unwrap();
    // The names were the engine's all along.
    s.crash_daemon().unwrap();
    s.boot().unwrap();
    assert_eq!(s.engine().volume_name(1), Some("one"));
    assert!(!s
        .engine()
        .pool_status()
        .warnings
        .iter()
        .any(|w| matches!(w, OperatorWarning::CatalogDamaged { .. })));
}

#[test]
fn a_verification_is_refused_while_the_engine_does_not_serve() {
    let mut s = sim_with(small(6195));
    s.boot().unwrap();
    s.engine().shutdown();
    assert!(s.engine().start_verify(VerifyOptions::default()).is_err());
}

// ---------------------------------------------------------------------------
// The random walk
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
enum Op {
    Write { vol: u8, lba: u8, blocks: u8 },
    Punch { vol: u8, lba: u8, blocks: u8 },
    Snapshot { vol: u8 },
    DeleteSnapshot { which: u8 },
    Cp,
    Fold,
    Clean,
    Daemon,
    Power,
    Restart,
    Attest,
}

fn op() -> impl Strategy<Value = Op> {
    prop_oneof![
        8 => (any::<u8>(), any::<u8>(), 1..=8u8).prop_map(|(vol, lba, blocks)| Op::Write { vol, lba, blocks }),
        2 => (any::<u8>(), any::<u8>(), 1..=16u8).prop_map(|(vol, lba, blocks)| Op::Punch { vol, lba, blocks }),
        2 => any::<u8>().prop_map(|vol| Op::Snapshot { vol }),
        2 => any::<u8>().prop_map(|which| Op::DeleteSnapshot { which }),
        3 => Just(Op::Cp),
        1 => Just(Op::Fold),
        2 => Just(Op::Clean),
        1 => Just(Op::Daemon),
        1 => Just(Op::Power),
        1 => Just(Op::Restart),
        2 => Just(Op::Attest),
    ]
}

/// Random writes, punches, snapshots and deletions, folds, cleaning and
/// crashes on a small pool with the cleaner on; after every recovery and
/// at random points the pool must attest clean (no finding at all), and
/// at the end too.
fn walk(seed: u64, ops: &[Op]) -> Result<(), String> {
    let mut cfg = small(seed);
    cfg.cleaner = CleanerPolicy {
        utilization: 0.3,
        max_segments_per_cp: 2,
        background: true,
        margin_segments: 4,
        max_live_fraction: 0.8,
    };
    let mut s = Sim::new(cfg)?;
    s.boot()?;
    let sizes = [160u64, 96];
    for (i, op) in ops.iter().enumerate() {
        let step = |e: String| format!("seed {seed} op {i} {op:?}: {e}");
        match *op {
            Op::Write { vol, lba, blocks } | Op::Punch { vol, lba, blocks } => {
                let vol = u32::from(vol % 2);
                let size = sizes[vol as usize];
                let blocks = u64::from(blocks).min(size);
                let lba = u64::from(lba) % (size - blocks + 1);
                let held = s.engine().nvfail_set().get(vol);
                let c = if matches!(op, Op::Write { .. }) {
                    s.write(vol, lba, blocks as u32).map_err(step)?
                } else {
                    s.punch(vol, lba, blocks as u32, false).map_err(step)?
                };
                let fine = c.status == Status::Ok
                    || c.status == Status::NoSpace
                    || (held && c.status == Status::NvFailed);
                if !fine {
                    return Err(step(format!("status {:?}", c.status)));
                }
            }
            Op::Snapshot { vol } => {
                let vol = u32::from(vol % 2);
                if !s.engine().nvfail_set().get(vol) && s.snaps.len() < 4 {
                    s.snapshot(vol).map_err(step)?;
                }
            }
            Op::DeleteSnapshot { which } => {
                if !s.snaps.is_empty() {
                    let id = s.snaps[usize::from(which) % s.snaps.len()].snap_id;
                    s.delete_snapshot(id).map_err(step)?;
                }
            }
            Op::Cp => {
                s.request_cp().map_err(step)?;
            }
            Op::Fold => {
                s.request_fold().map_err(step)?;
            }
            Op::Clean => {
                s.request_clean().map_err(step)?;
            }
            Op::Daemon => {
                s.crash_daemon().map_err(step)?;
                s.boot().map_err(step)?;
                s.verify_recovery().map_err(step)?;
                s.attest_clean().map_err(step)?;
            }
            Op::Power => {
                s.crash_power().map_err(step)?;
                s.boot().map_err(step)?;
                s.verify_recovery().map_err(step)?;
                s.clear_nvfail().map_err(step)?;
                s.attest_clean().map_err(step)?;
            }
            Op::Restart => {
                s.shutdown_clean().map_err(step)?;
                s.boot().map_err(step)?;
                s.verify_recovery().map_err(step)?;
                s.attest_clean().map_err(step)?;
            }
            Op::Attest => {
                s.attest_clean().map_err(step)?;
            }
        }
    }
    s.attest_clean()
        .map_err(|e| format!("seed {seed} at the end: {e}"))?;
    Ok(())
}

/// The case count: `PROPTEST_CASES` when set (the 10,000-case gate), else
/// `default` for the plain run.
fn proptest_cases(default: u32) -> u32 {
    std::env::var("PROPTEST_CASES")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(default)
}

proptest! {
    #![proptest_config(ProptestConfig {
        failure_persistence: None,
        cases: proptest_cases(48),
        max_shrink_iters: 256,
        ..ProptestConfig::default()
    })]

    /// A healthy pool never raises a finding, whatever ran on it.
    #[test]
    fn random_workloads_with_crashes_always_attest_clean(
        seed in any::<u64>(),
        ops in prop::collection::vec(op(), 4..24),
    ) {
        if let Err(e) = walk(seed, &ops) {
            prop_assert!(false, "{e}");
        }
    }
}
