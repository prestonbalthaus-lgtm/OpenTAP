//! Step 6C: replication between two simulated pools over the simulated
//! network ([`SimNet`]): full and incremental deltas, resume after drops
//! and cuts at every frame kind and position, sink crashes, quarantined
//! snapshots, abandoned deltas, pulls, and the rule that the replication
//! base pin moves only on `Committed`.
//!
//! The oracle is the source snapshot itself: once a job is done, every
//! block of the destination snapshot (the same identity on the sink pool)
//! must read exactly as the source snapshot reads, and so must the
//! destination's live volume (the mirror stands at the target).

use otap_cp::repl::{JobReport, JobState, ReplConfig, Replicator, SinkConfig, SourceConfig};
use otap_cp::{EngineError, LinkListener, Status};
use otap_format::{FrameKind, SnapOrigin, Uuid, BLOCK_SIZE, PIN_REPLICATION};
use otap_sim::{
    Direction, FaultKind, FaultRule, MemStore, Sim, SimConfig, SimListener, SimNet, SplitMix64,
};
use proptest::prelude::*;

const MS: u64 = 1_000_000;
const STEP_BUDGET: u32 = 100_000;

fn repl_config() -> ReplConfig {
    ReplConfig {
        source: SourceConfig {
            chunk_bytes: 32 * 1024,
            chunks_per_step: 4,
            frames_per_step: 64,
            pages_per_step: 4,
            keepalive_ns: 50 * MS,
            idle_timeout_ns: 400 * MS,
            retry_initial_ns: 5 * MS,
            retry_max_ns: 40 * MS,
            max_retries: 10_000,
            linger_ns: 60_000 * MS,
        },
        sink: SinkConfig {
            staging_bytes: 256 * 1024,
            frames_per_step: 64,
            runs_per_step: 32,
            max_run_blocks: 16,
            inbox_frames: 256,
            keepalive_ns: 50 * MS,
            idle_timeout_ns: 400 * MS,
            retry_initial_ns: 5 * MS,
            retry_max_ns: 40 * MS,
            max_retries: 10_000,
            linger_ns: 60_000 * MS,
            snapshot_retry_ns: 100 * MS,
        },
        handshake_timeout_ns: 400 * MS,
        keep_finished: 64,
    }
}

/// Two pools, the network between them, and a replicator on each.
struct Pair {
    src: Sim,
    dst: Sim,
    net: SimNet,
    src_repl: Replicator,
    dst_repl: Replicator,
    src_listener: SimListener,
    dst_listener: SimListener,
    dst_store: MemStore,
    /// The source's pin was seen on a snapshot the destination did not
    /// hold committed: the pin rule broken.
    pin_violation: Option<String>,
    /// The pin the checker saw last.
    last_pin: Option<Uuid>,
}

impl Pair {
    fn new(seed: u64, src_blocks: u64) -> Self {
        let mut scfg = SimConfig::small(seed);
        scfg.volumes = vec![(0, src_blocks)];
        let mut dcfg = SimConfig::small(seed ^ 0x5eed_d057);
        dcfg.volumes = vec![];
        let mut src = Sim::new(scfg).expect("the source pool formats");
        let mut dst = Sim::new(dcfg).expect("the destination pool formats");
        src.boot().expect("the source boots");
        dst.boot().expect("the destination boots");
        let net = SimNet::new(seed);
        let src_listener = net.listen("src");
        let dst_listener = net.listen("dst");
        let dst_store = MemStore::new();
        let src_repl = Replicator::new(
            repl_config(),
            Box::new(net.dialer()),
            Box::new(MemStore::new()),
        );
        let mut dst_repl = Replicator::new(
            repl_config(),
            Box::new(net.dialer()),
            Box::new(dst_store.clone()),
        );
        dst_repl.attach(dst.engine());
        Self {
            src,
            dst,
            net,
            src_repl,
            dst_repl,
            src_listener,
            dst_listener,
            dst_store,
            pin_violation: None,
            last_pin: None,
        }
    }

    /// One turn of everything: both engines, the network, both
    /// replicators (accepting what the listeners hold first), and the
    /// check of the pin rule.
    fn tick(&mut self) {
        self.src.advance(MS);
        self.dst.advance(MS);
        self.src.tick().expect("the source ticks");
        self.dst.tick().expect("the destination ticks");
        self.net.tick();
        while let Some(link) = self.src_listener.accept() {
            let now = self.src.engine().mono_ns();
            self.src_repl.offer(link, now);
        }
        while let Some(link) = self.dst_listener.accept() {
            let now = self.dst.engine().mono_ns();
            self.dst_repl.offer(link, now);
        }
        self.src_repl.step(self.src.engine());
        self.dst_repl.step(self.dst.engine());
        self.check_pin();
    }

    /// The pin rule: the source's pin moves to a snapshot only once the
    /// destination holds that snapshot durably (the sink may delete its old
    /// base before the source hears `Committed`, so the old pin is not
    /// checked, only each move).
    fn check_pin(&mut self) {
        if self.pin_violation.is_some() {
            return;
        }
        let pinned = self.src_pinned();
        if pinned != self.last_pin {
            if let Some(p) = pinned {
                if self.dst_snapshot(p).is_none() {
                    self.pin_violation = Some(format!(
                        "the source's pin moved to {p}, which the destination does not hold"
                    ));
                }
            }
            self.last_pin = pinned;
        }
    }

    /// Ticks until `done` holds.
    fn run_until(&mut self, what: &str, mut done: impl FnMut(&mut Self) -> bool) {
        for _ in 0..STEP_BUDGET {
            if done(self) {
                return;
            }
            self.tick();
        }
        panic!(
            "{what} did not happen within {STEP_BUDGET} steps; source jobs {:#?}; sink jobs {:#?}; \
             net {:?}",
            self.src_repl.jobs(),
            self.dst_repl.jobs(),
            self.net.stats()
        );
    }

    /// Ticks until source job `id` ends; returns its report and the last
    /// sink job's.
    fn finish(&mut self, id: u64) -> (JobReport, Option<JobReport>) {
        self.run_until("the push to end", |p| {
            p.src_repl
                .job(id)
                .is_some_and(|j| j.state != JobState::Running)
        });
        let sink = self.dst_repl.jobs().into_iter().last();
        (self.src_repl.job(id).expect("the job is kept"), sink)
    }

    /// Pushes snapshot `snap_id` and waits for the job; asserts it is done
    /// and the destination converged.
    fn ship(&mut self, snap_id: u64) -> (JobReport, JobReport) {
        let id = self.push(snap_id);
        let (job, sink) = self.finish(id);
        assert_eq!(job.state, JobState::Done, "{job:#?}");
        let sink = sink.expect("a sink job");
        self.run_until("the sink job to end", |p| {
            p.dst_repl
                .job(sink.id)
                .is_some_and(|j| j.state != JobState::Running)
        });
        let sink = self.dst_repl.job(sink.id).expect("the sink job is kept");
        assert_eq!(sink.state, JobState::Done, "{sink:#?}");
        self.assert_converged(snap_id);
        assert!(self.pin_violation.is_none(), "{:?}", self.pin_violation);
        (job, sink)
    }

    /// Writes `blocks` blocks at `lba` of the source volume.
    fn write(&mut self, lba: u64, blocks: u32) {
        let c = self.src.write(0, lba, blocks).expect("the write completes");
        assert_eq!(c.status, Status::Ok, "write {lba}+{blocks}");
    }

    /// Punches a hole in the source volume.
    fn punch(&mut self, lba: u64, blocks: u32) {
        let c = self
            .src
            .punch(0, lba, blocks, false)
            .expect("the punch completes");
        assert_eq!(c.status, Status::Ok, "punch {lba}+{blocks}");
    }

    /// A snapshot of the source volume; its id.
    fn snapshot(&mut self) -> u64 {
        self.src.snapshot(0).expect("the snapshot is taken")
    }

    /// Starts pushing snapshot `snap_id` to the destination volume `mirror`.
    fn push(&mut self, snap_id: u64) -> u64 {
        let snap = format!("#{snap_id}");
        self.src_repl
            .push(self.src.engine(), "#0", &snap, "dst", Some("mirror"))
            .expect("the push starts")
            .id
    }

    fn src_uuid(&mut self, snap_id: u64) -> Uuid {
        self.src
            .engine()
            .snapshots()
            .iter()
            .find(|s| s.snap_id == snap_id)
            .expect("the source snapshot exists")
            .uuid
    }

    /// The source snapshot that holds the replication pin, if any.
    fn src_pinned(&mut self) -> Option<Uuid> {
        self.src
            .engine()
            .snapshots()
            .iter()
            .find(|s| s.vol == 0 && s.flags & PIN_REPLICATION != 0)
            .map(|s| s.uuid)
    }

    /// The destination volume `mirror`.
    fn mirror(&mut self) -> Option<u32> {
        self.dst.engine().find_volume("mirror")
    }

    /// The destination snapshot of identity `uuid`.
    fn dst_snapshot(&mut self, uuid: Uuid) -> Option<otap_cp::SnapshotInfo> {
        let vol = self.mirror()?;
        self.dst
            .engine()
            .snapshots()
            .into_iter()
            .find(|s| s.vol == vol && s.uuid == uuid)
    }

    /// The destination's snapshots of the mirror.
    fn dst_snapshots(&mut self) -> Vec<otap_cp::SnapshotInfo> {
        let Some(vol) = self.mirror() else {
            return Vec::new();
        };
        self.dst
            .engine()
            .snapshots()
            .into_iter()
            .filter(|s| s.vol == vol && !s.deleting)
            .collect()
    }

    /// Every block of the source snapshot equals the destination's.
    fn assert_converged(&mut self, snap_id: u64) {
        let uuid = self.src_uuid(snap_id);
        let size = snapshot_size(&mut self.src, snap_id);
        let dst = self
            .dst_snapshot(uuid)
            .unwrap_or_else(|| panic!("the destination holds no snapshot {uuid}"));
        assert_eq!(dst.origin, SnapOrigin::Transfer);
        assert!(!dst.damaged);
        let dsize = snapshot_size(&mut self.dst, dst.snap_id);
        assert_eq!(dsize, size, "the destination snapshot's size");
        let vol = self.mirror().expect("the mirror exists");
        let mut a = vec![0u8; BLOCK_SIZE];
        let mut b = vec![0u8; BLOCK_SIZE];
        let mut c = vec![0u8; BLOCK_SIZE];
        for lba in 0..size {
            self.src
                .engine()
                .snapshot_read(snap_id, lba, &mut a)
                .expect("the source snapshot reads");
            self.dst
                .engine()
                .snapshot_read(dst.snap_id, lba, &mut b)
                .expect("the destination snapshot reads");
            assert!(
                a == b,
                "block {lba} of snapshot {uuid} differs on the destination"
            );
            self.dst
                .engine()
                .peek(vol, lba, &mut c)
                .expect("the mirror reads");
            assert!(
                a == c,
                "block {lba} of the mirror differs from the snapshot"
            );
        }
    }

    /// The destination process dies (`kill -9`, or a power cut) and comes
    /// back: a new replicator over the same state store.
    fn crash_dst(&mut self, power: bool) {
        if power {
            self.dst.crash_power().expect("the power cut");
        } else {
            self.dst.crash_daemon().expect("the crash");
        }
        let cfg = repl_config();
        self.dst_repl = Replicator::new(
            cfg,
            Box::new(self.net.dialer()),
            Box::new(self.dst_store.clone()),
        );
        self.net.unlisten("dst");
        self.dst.boot().expect("the destination reboots");
        self.dst
            .audit_liveness()
            .expect("the destination's accounting");
        self.dst_listener = self.net.listen("dst");
        self.dst_repl.attach(self.dst.engine());
    }

    /// The source process dies and comes back (it keeps no replication
    /// state of its own).
    fn crash_src(&mut self) {
        self.src.crash_daemon().expect("the crash");
        self.restart_src();
        self.src.verify_recovery().expect("the source recovers");
    }

    /// Boots the source again after a crash, with a new replicator.
    fn restart_src(&mut self) {
        self.src_repl = Replicator::new(
            repl_config(),
            Box::new(self.net.dialer()),
            Box::new(MemStore::new()),
        );
        self.net.unlisten("src");
        self.src.boot().expect("the source reboots");
        self.src_listener = self.net.listen("src");
    }

    /// Chunks the sink landed, from its last job.
    fn sink_landed(&self) -> u32 {
        self.dst_repl.jobs().last().map_or(0, |j| j.chunks_landed)
    }
}

/// A snapshot's size in blocks.
fn snapshot_size(sim: &mut Sim, snap_id: u64) -> u64 {
    sim.engine()
        .snapshot_delta_scan(snap_id, None, u64::MAX, 1, 1)
        .expect("the snapshot scans")
        .size_blocks
}

/// Fills the source volume with a mix of runs.
fn scatter(p: &mut Pair, rng: &mut SplitMix64, size: u64, writes: u32) {
    for _ in 0..writes {
        let blocks = rng.range(1, 8) as u32;
        let lba = rng.below(size - u64::from(blocks) + 1);
        p.write(lba, blocks);
    }
}

#[test]
fn a_full_transfer_lands_every_block() {
    let mut p = Pair::new(6001, 256);
    p.write(0, 8);
    p.write(40, 3);
    p.write(100, 16);
    p.write(255, 1);
    let snap = p.snapshot();
    let (job, sink) = p.ship(snap);
    assert_eq!(job.chunks, 28_u32.div_ceil(7), "28 blocks in chunks of 7");
    assert_eq!(sink.duplicate_chunks, 0);
    assert_eq!(sink.digest_verified, Some(true));
    assert_eq!(sink.chunks_landed, job.chunks);
    let uuid = p.src_uuid(snap);
    assert_eq!(
        p.src_pinned(),
        Some(uuid),
        "the base pin moved to the target"
    );
    let mirror = p.mirror().expect("the mirror exists");
    let v = p
        .dst
        .engine()
        .volume(mirror)
        .expect("the mirror is present");
    assert!(v.offline, "the mirror stays offline");
    assert!(p.dst.engine().is_replica(mirror));
    let dsnap = p.dst_snapshot(uuid).expect("the destination snapshot");
    assert!(
        dsnap.flags & PIN_REPLICATION != 0,
        "the mirror's base is pinned"
    );
}

/// The second and third deltas ship only what changed since the base the
/// sink holds, holes included, and each moves both pins; the mirror keeps
/// one transfer snapshot, its base.
#[test]
fn incremental_deltas_converge_and_move_both_bases() {
    let mut p = Pair::new(6002, 512);
    let mut rng = SplitMix64::new(6002);
    scatter(&mut p, &mut rng, 512, 40);
    let s1 = p.snapshot();
    let (first, _) = p.ship(s1);
    // Second: overwrites, a punch, a write past what was ever written.
    p.write(3, 4);
    p.punch(100, 20);
    p.write(500, 12);
    let s2 = p.snapshot();
    let (second, sink2) = p.ship(s2);
    assert!(
        second.chunks < first.chunks,
        "the increment ships less: {} vs {}",
        second.chunks,
        first.chunks
    );
    assert_eq!(second.base_uuid, Some(p.src_uuid(s1)));
    assert_eq!(sink2.duplicate_chunks, 0);
    let u2 = p.src_uuid(s2);
    assert_eq!(p.src_pinned(), Some(u2));
    // The old source base is no longer pinned and may be deleted.
    p.src
        .delete_snapshot(s1)
        .expect("the old base is deletable");
    // Third, over the advanced base.
    p.punch(0, 2);
    p.write(200, 30);
    let s3 = p.snapshot();
    p.ship(s3);
    let u3 = p.src_uuid(s3);
    assert_eq!(p.src_pinned(), Some(u3));
    p.run_until("the mirror's old bases to go", |p| {
        p.dst_snapshots().len() == 1
    });
    let left = p.dst_snapshots();
    assert_eq!(left[0].uuid, u3);
    assert!(left[0].flags & PIN_REPLICATION != 0);
}

/// A lost `Committed`: the sink holds the target, the source has not heard
/// it. The pin stays on the old base until the reconnect's handshake
/// reports the target as the sink's base.
#[test]
fn the_base_pin_moves_only_when_the_sink_committed() {
    let mut p = Pair::new(6003, 256);
    p.write(0, 20);
    let s1 = p.snapshot();
    p.ship(s1);
    let u1 = p.src_uuid(s1);
    p.write(30, 9);
    let s2 = p.snapshot();
    let u2 = p.src_uuid(s2);
    p.net.add_fault(FaultRule {
        kind: FaultKind::Drop,
        direction: Some(Direction::FromAcceptor),
        message: Some(3),
        stream_kind: Some(FrameKind::Committed.as_u8()),
        nth: 0,
    });
    let id = p.push(s2);
    let mut saw_held = false;
    p.run_until("the destination to hold the second snapshot", |p| {
        p.dst_snapshot(u2).is_some()
    });
    for _ in 0..20 {
        if p.src_pinned() == Some(u1) {
            saw_held = true;
        }
        assert!(
            matches!(p.src_pinned(), Some(u) if u == u1 || u == u2),
            "the pin is on one of the two bases"
        );
        p.tick();
    }
    let (job, _) = p.finish(id);
    assert_eq!(job.state, JobState::Done, "{job:#?}");
    assert!(saw_held, "the pin waited while the Committed was lost");
    assert!(job.retries >= 1, "a reconnect learned the outcome");
    assert_eq!(p.src_pinned(), Some(u2));
    assert_eq!(p.net.stats().drops, 1);
    assert!(p.pin_violation.is_none(), "{:?}", p.pin_violation);
}

/// A connection reset in the middle of a long delta: the source resumes
/// from the sink's cursor, the sink receives no chunk it had landed, and
/// every chunk lands once.
#[test]
fn a_resume_resends_no_landed_chunk() {
    let mut p = Pair::new(6004, 1024);
    for lba in (0..1024).step_by(8) {
        p.write(lba, 8);
    }
    let s1 = p.snapshot();
    for (nth, kind) in [(30, FaultKind::Drop), (70, FaultKind::Cut)] {
        p.net.add_fault(FaultRule {
            kind,
            direction: Some(Direction::FromDialer),
            message: Some(3),
            stream_kind: Some(FrameKind::Chunk.as_u8()),
            nth,
        });
    }
    let (job, sink) = p.ship(s1);
    let chunks = 1024_u32.div_ceil(7);
    assert_eq!(job.chunks, chunks);
    assert!(job.retries >= 2, "two connections were lost: {job:#?}");
    assert!(job.resumed_chunks > 0, "the resume skipped landed chunks");
    assert_eq!(sink.duplicate_chunks, 0, "no landed chunk came again");
    assert_eq!(sink.chunks_landed, chunks, "every chunk landed once");
    assert!(
        job.chunks_sent < chunks + 70,
        "the source resent only what was lost in flight: {} sent",
        job.chunks_sent
    );
    assert_eq!(p.net.stats().faults(), 2);
}

/// The sink dies in the middle of a delta (a `kill -9`, then a power cut):
/// no partial snapshot is ever exposed, the mirror stays offline, and the
/// delta resumes from the durable record without re-landing what landed.
#[test]
fn a_sink_crash_mid_stream_exposes_no_partial_snapshot() {
    for power in [false, true] {
        let mut p = Pair::new(6005 + u64::from(power), 1024);
        for lba in (0..1024).step_by(8) {
            p.write(lba, 8);
        }
        let s1 = p.snapshot();
        let u1 = p.src_uuid(s1);
        let id = p.push(s1);
        p.run_until("a third of the chunks to land", |p| p.sink_landed() >= 50);
        let landed_before = p.sink_landed();
        p.crash_dst(power);
        assert!(
            p.dst_snapshot(u1).is_none(),
            "power {power}: a partial delta is never a snapshot"
        );
        let mirror = p.mirror().expect("the mirror survived");
        assert!(p.dst.engine().volume(mirror).is_some_and(|v| v.offline));
        assert!(
            p.dst.engine().is_replica(mirror),
            "the record marked it again"
        );
        let (job, sink) = p.finish(id);
        assert_eq!(job.state, JobState::Done, "power {power}: {job:#?}");
        let sink = sink.expect("a sink job after the restart");
        p.run_until("the sink to finish", |p| {
            p.dst_repl
                .job(sink.id)
                .is_some_and(|j| j.state != JobState::Running)
        });
        let sink = p.dst_repl.job(sink.id).expect("kept");
        assert_eq!(sink.duplicate_chunks, 0, "power {power}");
        assert!(
            sink.chunks_landed + landed_before >= job.chunks,
            "power {power}: the restarted sink landed the rest"
        );
        if !power {
            assert!(
                sink.chunks_landed < job.chunks,
                "a kill keeps every landed chunk: the restart landed {} of {}",
                sink.chunks_landed,
                job.chunks
            );
        }
        p.assert_converged(s1);
        assert!(p.pin_violation.is_none());
    }
}

/// A snapshot quarantined on the source (a map page that rots) is refused
/// as the snapshot to ship and as the base of a delta.
#[test]
fn a_quarantined_snapshot_is_refused_as_source_and_as_base() {
    let mut p = Pair::new(6007, 256);
    p.write(0, 4);
    p.write(20, 2);
    let s1 = p.snapshot();
    p.ship(s1);
    // Rot s1's only map page: the live map has its own copy after a fold.
    let page = p.src.engine().map().volume(0).expect("mapped").index()[0];
    p.write(1, 1);
    p.src.request_fold().expect("a fold");
    let live = p.src.engine().map().volume(0).expect("mapped").index()[0];
    assert_ne!(live.page_pba, page.page_pba);
    p.src.request_cp().expect("a CP");
    p.src.request_cp().expect("a CP");
    // The page rots while the daemon is down; the boot checks a
    // snapshot's root and index only, so it boots as sound.
    p.src.crash_daemon().expect("the crash");
    p.src
        .disk()
        .corrupt_durable(page.page_pba as usize * BLOCK_SIZE + 100, &[0xA5; 4]);
    p.restart_src();
    // A cold cache finds it on the next read.
    let mut buf = vec![0u8; BLOCK_SIZE];
    let read = p.src.engine().snapshot_read(s1, 0, &mut buf);
    assert!(
        matches!(read, Err(EngineError::SnapshotDamaged { .. })),
        "{read:?}"
    );
    // As the snapshot to ship.
    let refused = p.src_repl.push(
        p.src.engine(),
        "#0",
        &format!("#{s1}"),
        "dst",
        Some("mirror"),
    );
    assert!(
        matches!(&refused, Err(e) if e.to_string().contains("quarantined")),
        "{refused:?}"
    );
    // As the base the sink holds.
    p.write(40, 3);
    let s2 = p.snapshot();
    let id = p.push(s2);
    let (job, _) = p.finish(id);
    assert_eq!(job.state, JobState::Failed);
    let why = job.last_error.unwrap_or_default();
    assert!(why.contains("quarantined"), "{why}");
    // The destination kept its base and is unharmed.
    let u1 = p.src_uuid(s1);
    assert!(p.dst_snapshot(u1).is_some());
}

/// A shrink and a grow between two snapshots leave blocks unwritten in
/// the target that the base held: a birth walk cannot see them, the delta
/// carries them as holes, and the destination converges.
#[test]
fn a_shrink_and_grow_between_snapshots_converges() {
    let mut p = Pair::new(6008, 256);
    for lba in (0..256).step_by(8) {
        p.write(lba, 8);
    }
    let s1 = p.snapshot();
    p.ship(s1);
    p.src.resize_volume(0, 100).expect("the shrink");
    p.src.set_online(0, true).expect("online");
    p.src.resize_volume(0, 256).expect("the grow");
    p.write(10, 2);
    let s2 = p.snapshot();
    p.ship(s2);
    let mirror = p.mirror().expect("mirror");
    let mut buf = vec![0xffu8; BLOCK_SIZE];
    p.dst.engine().peek(mirror, 200, &mut buf).expect("reads");
    assert!(
        buf.iter().all(|b| *b == 0),
        "a block lost to the shrink reads zeros"
    );
    // And a plain shrink.
    p.src.resize_volume(0, 64).expect("the shrink");
    p.src.set_online(0, true).expect("online");
    let s3 = p.snapshot();
    p.ship(s3);
    assert_eq!(
        p.dst.engine().volume(mirror).map(|v| v.size_blocks),
        Some(64)
    );
}

/// A delta with no change ships the anchor chunk; a delta of holes alone
/// ships the holes and the anchor; both converge.
#[test]
fn an_empty_delta_and_a_holes_only_delta_converge() {
    let mut p = Pair::new(6009, 128);
    p.write(0, 16);
    p.write(64, 16);
    let s1 = p.snapshot();
    p.ship(s1);
    let s2 = p.snapshot();
    let (empty, _) = p.ship(s2);
    assert_eq!(empty.chunks, 1, "the anchor alone");
    p.punch(4, 8);
    p.punch(70, 30);
    let s3 = p.snapshot();
    let (holes, _) = p.ship(s3);
    assert_eq!(holes.chunks, 1, "holes travel as records, plus the anchor");
    // An empty first transfer.
    let mut q = Pair::new(6010, 32);
    let s = q.snapshot();
    let (job, _) = q.ship(s);
    assert_eq!(job.chunks, 1);
}

/// The source gives up a delta half landed and ships a newer target: the
/// sink rolls the mirror back to its base first (or, with no base, deletes
/// the partial first transfer), then lands the new delta.
#[test]
fn a_delta_to_another_target_rolls_the_mirror_back_first() {
    // With a base.
    let mut p = Pair::new(6011, 1024);
    for lba in (0..512).step_by(8) {
        p.write(lba, 8);
    }
    let s1 = p.snapshot();
    p.ship(s1);
    for lba in (0..1024).step_by(4) {
        p.write(lba, 4);
    }
    let s2 = p.snapshot();
    let id = p.push(s2);
    p.run_until("part of the second delta to land", |p| {
        p.sink_landed() >= 20
    });
    p.src_repl.cancel(id).expect("cancelled");
    p.write(7, 3);
    let s3 = p.snapshot();
    let (job, _) = p.ship(s3);
    assert_eq!(job.base_uuid, Some(p.src_uuid(s1)));
    let u2 = p.src_uuid(s2);
    assert!(
        p.dst_snapshot(u2).is_none(),
        "the abandoned target never landed"
    );

    // Without a base.
    let mut q = Pair::new(6012, 1024);
    for lba in (0..1024).step_by(8) {
        q.write(lba, 8);
    }
    let t1 = q.snapshot();
    let id = q.push(t1);
    q.run_until("part of the first transfer to land", |q| {
        q.sink_landed() >= 30
    });
    q.src_repl.cancel(id).expect("cancelled");
    q.write(900, 5);
    let t2 = q.snapshot();
    q.ship(t2);
}

/// A pull: the sink dials and asks for a snapshot by name; drops on both
/// sides of the stream are resumed by the sink dialing again.
#[test]
fn a_pull_converges_through_drops() {
    let mut p = Pair::new(6013, 512);
    let mut rng = SplitMix64::new(6013);
    scatter(&mut p, &mut rng, 512, 50);
    let s1 = p.snapshot();
    for (dir, nth) in [(Direction::FromAcceptor, 5), (Direction::FromDialer, 3)] {
        p.net.add_fault(FaultRule {
            kind: FaultKind::Drop,
            direction: Some(dir),
            message: Some(3),
            stream_kind: None,
            nth,
        });
    }
    let snap = format!("#{s1}");
    let id = p
        .dst_repl
        .pull(p.dst.engine(), "mirror", &snap, "src", Some("#0"))
        .expect("the pull starts")
        .id;
    p.run_until("the pull to end", |p| {
        p.dst_repl
            .job(id)
            .is_some_and(|j| j.state != JobState::Running)
    });
    let job = p.dst_repl.job(id).expect("kept");
    assert_eq!(job.state, JobState::Done, "{job:#?}");
    assert!(job.retries >= 1);
    assert_eq!(job.duplicate_chunks, 0);
    p.run_until("the source to pin the target", |p| p.src_pinned().is_some());
    p.assert_converged(s1);
    // A second pull of the same snapshot finds nothing to do.
    let again = p
        .dst_repl
        .pull(p.dst.engine(), "mirror", &snap, "src", Some("#0"))
        .expect("the pull starts")
        .id;
    p.run_until("the second pull to end", |p| {
        p.dst_repl
            .job(again)
            .is_some_and(|j| j.state != JobState::Running)
    });
    let job = p.dst_repl.job(again).expect("kept");
    assert_eq!(job.state, JobState::Done, "{job:#?}");
    assert_eq!(job.chunks_landed, 0);
}

/// The mirror refuses to come online while it is a mirror; a release
/// frees it, after which it serves the target's content.
#[test]
fn the_mirror_stays_offline_until_released() {
    let mut p = Pair::new(6014, 128);
    p.write(0, 32);
    let s1 = p.snapshot();
    p.ship(s1);
    let mirror = p.mirror().expect("mirror");
    let online = p.dst.engine().set_volume_online(mirror, true);
    assert!(
        matches!(online, Err(EngineError::ReplicaVolume { .. })),
        "{online:?}"
    );
    let cursor = p
        .dst_repl
        .cursor(p.dst.engine(), "mirror")
        .expect("a cursor");
    assert!(cursor.replica && cursor.persisted && !cursor.dirty);
    assert_eq!(cursor.base_uuid, Some(p.src_uuid(s1)));
    let out = p
        .dst_repl
        .release(p.dst.engine(), "mirror")
        .expect("released");
    assert!(!out.rolled_back);
    p.dst
        .engine()
        .set_volume_online(mirror, true)
        .expect("online now");
    p.tick();
    assert!(p.dst_store.get("sink-0").is_none(), "the record is gone");
    let mut data = vec![0u8; 32 * BLOCK_SIZE];
    p.dst
        .engine()
        .peek(mirror, 0, &mut data)
        .expect("the mirror reads");
    let mut want = vec![0u8; 32 * BLOCK_SIZE];
    p.src
        .engine()
        .snapshot_read(s1, 0, &mut want)
        .expect("the source reads");
    assert!(data == want, "the released mirror holds the target");
}

/// Every frame kind of the exchange, at every position the exchange
/// produces it, dropped, cut or corrupted: the transfer converges, the pin
/// rule holds, and no landed chunk is received twice.
#[test]
fn every_frame_kind_and_position_tolerates_a_fault() {
    let kinds: [(Option<u8>, Direction, u8, u64); 14] = [
        (None, Direction::FromDialer, 1, 1),
        (None, Direction::FromAcceptor, 1, 1),
        (Some(FrameKind::Hello.as_u8()), Direction::FromDialer, 3, 1),
        (
            Some(FrameKind::HelloAck.as_u8()),
            Direction::FromAcceptor,
            3,
            1,
        ),
        (
            Some(FrameKind::DeltaBegin.as_u8()),
            Direction::FromDialer,
            3,
            1,
        ),
        (
            Some(FrameKind::VolEvent.as_u8()),
            Direction::FromDialer,
            3,
            2,
        ),
        (Some(FrameKind::Holes.as_u8()), Direction::FromDialer, 3, 1),
        (Some(FrameKind::Chunk.as_u8()), Direction::FromDialer, 3, 3),
        (
            Some(FrameKind::Landed.as_u8()),
            Direction::FromAcceptor,
            3,
            3,
        ),
        (
            Some(FrameKind::DeltaEnd.as_u8()),
            Direction::FromDialer,
            3,
            1,
        ),
        (
            Some(FrameKind::Committed.as_u8()),
            Direction::FromAcceptor,
            3,
            1,
        ),
        (Some(FrameKind::Ping.as_u8()), Direction::FromDialer, 3, 1),
        (Some(FrameKind::Pong.as_u8()), Direction::FromAcceptor, 3, 1),
        (Some(FrameKind::Ping.as_u8()), Direction::FromAcceptor, 3, 1),
    ];
    let mut runs = 0;
    for (k, (stream_kind, dir, message, positions)) in kinds.iter().enumerate() {
        for nth in 0..*positions {
            for (f, fault) in [FaultKind::Drop, FaultKind::Cut, FaultKind::Corrupt]
                .into_iter()
                .enumerate()
            {
                let seed = 7000 + (k as u64) * 100 + nth * 10 + f as u64;
                let mut p = Pair::new(seed, 128);
                p.write(0, 21);
                p.write(64, 21);
                let s1 = p.snapshot();
                p.ship(s1);
                p.write(10, 21);
                p.punch(70, 5);
                let s2 = p.snapshot();
                p.net.add_fault(FaultRule {
                    kind: fault,
                    direction: Some(*dir),
                    message: Some(*message),
                    stream_kind: *stream_kind,
                    nth,
                });
                if *stream_kind == Some(FrameKind::Ping.as_u8())
                    || *stream_kind == Some(FrameKind::Pong.as_u8())
                {
                    // Make the stream wait long enough for a keepalive.
                    p.net.set_delay(60);
                }
                let (job, sink) = p.ship(s2);
                assert_eq!(
                    sink.duplicate_chunks, 0,
                    "kind {stream_kind:?} {dir:?} #{nth} {fault:?}: {job:#?}"
                );
                assert_eq!(
                    p.net.stats().faults(),
                    1,
                    "kind {stream_kind:?} {dir:?} #{nth} {fault:?}: the fault fired"
                );
                assert!(
                    job.retries >= 1 || sink.connections >= 1,
                    "kind {stream_kind:?} {dir:?} #{nth} {fault:?}"
                );
                runs += 1;
            }
        }
    }
    assert_eq!(runs, 57, "every kind at every position, three faults each");
}

/// The frame kinds a fault rule of the walk aims at: every kind the
/// exchange carries, or any message (a handshake, a refusal).
const AIMS: [Option<FrameKind>; 13] = [
    None,
    Some(FrameKind::Hello),
    Some(FrameKind::HelloAck),
    Some(FrameKind::DeltaBegin),
    Some(FrameKind::VolEvent),
    Some(FrameKind::Holes),
    Some(FrameKind::Chunk),
    Some(FrameKind::Landed),
    Some(FrameKind::DeltaEnd),
    Some(FrameKind::Committed),
    Some(FrameKind::Ping),
    Some(FrameKind::Pong),
    Some(FrameKind::Abort),
];

/// One random walk: a source volume written at random, a few snapshots
/// pushed one after another, a fault aimed at a frame of a kind, a
/// direction and a position the seed picks, random faults on the network
/// within a budget, and once in a while a crash of the sink; every delta
/// converges and the pin rule holds throughout.
fn walk(seed: u64, deltas: u32, fault_per_mille: u64, crash: bool) -> Result<(), TestCaseError> {
    let mut rng = SplitMix64::new(seed);
    let size = 64 + rng.below(4) * 64;
    let mut p = Pair::new(seed, size);
    p.net.set_random_faults(fault_per_mille, 4);
    for _ in 0..rng.range(1, 3) {
        let aim = AIMS[rng.below(AIMS.len() as u64) as usize];
        p.net.add_fault(FaultRule {
            kind: [FaultKind::Drop, FaultKind::Cut, FaultKind::Corrupt][rng.below(3) as usize],
            direction: Some(if rng.chance(1, 2) {
                Direction::FromDialer
            } else {
                Direction::FromAcceptor
            }),
            message: aim.map(|_| 3),
            stream_kind: aim.map(FrameKind::as_u8),
            nth: rng.below(6),
        });
    }
    for d in 0..deltas {
        let writes = rng.range(0, 12) as u32;
        scatter(&mut p, &mut rng, size, writes);
        if rng.chance(1, 3) {
            let blocks = rng.range(1, 16) as u32;
            let lba = rng.below(size - u64::from(blocks) + 1);
            p.punch(lba, blocks);
        }
        let snap = p.snapshot();
        let id = p.push(snap);
        if crash && rng.chance(1, 2) {
            let at = rng.range(1, 40);
            for _ in 0..at {
                p.tick();
            }
            p.crash_dst(rng.chance(1, 2));
        }
        let (job, sink) = p.finish(id);
        prop_assert_eq!(job.state, JobState::Done, "delta {}: {:#?}", d, job);
        // A sink restarted after it committed has no job of this delta.
        if let Some(sink) = sink {
            p.run_until("the sink to end", |p| {
                p.dst_repl
                    .job(sink.id)
                    .is_some_and(|j| j.state != JobState::Running)
            });
            let sink = p.dst_repl.job(sink.id).expect("kept");
            prop_assert_eq!(sink.duplicate_chunks, 0, "delta {}: {:#?}", d, sink);
        }
        p.assert_converged(snap);
        prop_assert!(p.pin_violation.is_none(), "{:?}", p.pin_violation);
        let uuid = p.src_uuid(snap);
        prop_assert_eq!(p.src_pinned(), Some(uuid), "the pin moved to the target");
    }
    Ok(())
}

proptest! {
    #![proptest_config(ProptestConfig {
        cases: 64,
        failure_persistence: None,
        max_shrink_iters: 256,
        ..ProptestConfig::default()
    })]

    /// Random workloads, random drops, cuts and corruptions, random sink
    /// crashes: bit-exact convergence of every delta, the pin only on
    /// `Committed`, no landed chunk received twice.
    #[test]
    fn random_transfers_with_faults_and_crashes_converge(
        seed in any::<u64>(),
        deltas in 1u32..4,
        fault in 0u64..60,
        crash in any::<bool>(),
    ) {
        walk(seed, deltas, fault, crash)?;
    }
}

/// The source dies in the middle of a delta (it keeps no replication state
/// of its own): a new push of the same snapshot finds the same delta in the
/// sink's cursor and sends only the chunks that had not landed, rebuilding
/// the landed ones only to complete the digest.
#[test]
fn a_source_crash_mid_stream_resumes_from_the_sinks_cursor() {
    let mut p = Pair::new(6015, 1024);
    for lba in (0..1024).step_by(8) {
        p.write(lba, 8);
    }
    let s1 = p.snapshot();
    p.push(s1);
    p.run_until("part of the delta to land", |p| p.sink_landed() >= 60);
    p.crash_src();
    let id = p.push(s1);
    let (job, sink) = p.finish(id);
    assert_eq!(job.state, JobState::Done, "{job:#?}");
    assert!(
        job.resumed_chunks >= 60,
        "the new source skipped what had landed: {job:#?}"
    );
    assert!(job.chunks_sent < job.chunks, "{job:#?}");
    let sink = sink.expect("the sink job");
    p.run_until("the sink to finish", |p| {
        p.dst_repl
            .job(sink.id)
            .is_some_and(|j| j.state != JobState::Running)
    });
    let sink = p.dst_repl.job(sink.id).expect("kept");
    assert_eq!(sink.duplicate_chunks, 0);
    assert_eq!(sink.digest_verified, Some(true), "the digest was completed");
    p.assert_converged(s1);
}

/// A second source pushing into the same mirror neither takes over the
/// running transfer (it is told the mirror is busy, and retries) nor,
/// once that transfer committed, the mirror itself (it belongs to another
/// source volume): its job fails with the reason and the mirror is intact.
#[test]
fn a_second_source_cannot_take_over_a_mirror() {
    let mut p = Pair::new(6016, 1024);
    for lba in (0..1024).step_by(8) {
        p.write(lba, 8);
    }
    let s1 = p.snapshot();
    let id = p.push(s1);
    p.run_until("part of the delta to land", |p| p.sink_landed() >= 20);
    let mut cfg = SimConfig::small(6016 ^ 0x0bad);
    cfg.volumes = vec![(0, 64)];
    let mut stranger = Sim::new(cfg).expect("formats");
    stranger.boot().expect("boots");
    stranger.write(0, 0, 4).expect("writes");
    let t1 = stranger.snapshot(0).expect("a snapshot");
    let mut repl = Replicator::new(
        repl_config(),
        Box::new(p.net.dialer()),
        Box::new(MemStore::new()),
    );
    let sid = repl
        .push(
            stranger.engine(),
            "#0",
            &format!("#{t1}"),
            "dst",
            Some("mirror"),
        )
        .expect("the push starts")
        .id;
    let mut stranger_done = None;
    for _ in 0..STEP_BUDGET {
        p.tick();
        stranger.advance(MS);
        stranger.tick().expect("ticks");
        repl.step(stranger.engine());
        let job = repl.job(sid).expect("kept");
        if job.state != JobState::Running {
            stranger_done = Some(job);
            break;
        }
    }
    let job = stranger_done.expect("the stranger's job ended");
    assert_eq!(job.state, JobState::Failed, "{job:#?}");
    let why = job.last_error.unwrap_or_default();
    assert!(why.contains("mirror of volume"), "{why}");
    assert!(
        job.retries >= 1,
        "it was told to wait while the transfer ran"
    );
    let (own, _) = p.finish(id);
    assert_eq!(own.state, JobState::Done, "{own:#?}");
    p.assert_converged(s1);
}
