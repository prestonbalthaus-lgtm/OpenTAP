//! The Step 6 adversarial review (`docs/reviews/2026-10-16-step6-qa-adversarial.md`):
//! the replication findings, between two simulated pools over the
//! simulated network. Each test reproduced its finding before the fix and
//! pins the fixed behaviour. The two-pool harness (`Pair`) is the one of
//! `step6c_replication_sim_tests.rs`, copied so this file stands alone.

use otap_cp::repl::{JobReport, JobState, ReplConfig, Replicator, SinkConfig, SourceConfig};
use otap_cp::{Backstore, EngineError, LinkListener, Status};
use otap_format::{
    SbSlot, SnapOrigin, SuperblockSlot, Uuid, BLOCK_SIZE, PIN_REPLICATION, SUPERBLOCK_SLOT_SIZE,
};
use otap_sim::{MemStore, Schedule, Sim, SimConfig, SimListener, SimNet};

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

/// The superblock slot of the newest CP on the destination's disk.
fn newest_slot(s: &mut Sim) -> SuperblockSlot {
    let layout = *s.disk().layout();
    let mut best: Option<SuperblockSlot> = None;
    let mut buf = vec![0u8; SUPERBLOCK_SLOT_SIZE];
    for slot in SbSlot::ALL {
        s.disk()
            .read(layout.sb_pba(slot), &mut buf)
            .expect("ring read");
        if let Ok(parsed) = SuperblockSlot::read(&buf) {
            if best.is_none_or(|b| parsed.sb.cp_seq > b.sb.cp_seq) {
                best = Some(parsed);
            }
        }
    }
    best.expect("a verified slot")
}

/// A device that completes nothing: no consistency point can commit.
fn frozen() -> Schedule {
    Schedule {
        max_per_poll: 0,
        reorder: false,
        queue_depth: 4096,
    }
}

impl Pair {
    /// The last sink job's report.
    fn sink_job(&self) -> Option<JobReport> {
        self.dst_repl.jobs().into_iter().last()
    }

    /// Pushes snapshot `snap_id` into the destination volume `dest`.
    fn push_to(&mut self, snap_id: u64, dest: &str) -> u64 {
        let snap = format!("#{snap_id}");
        self.src_repl
            .push(self.src.engine(), "#0", &snap, "dst", Some(dest))
            .expect("the push starts")
            .id
    }
}

/// D2: a sink job in the middle of taking its destination snapshot took
/// the target of a later handshake (the source cancelled and pushed a newer
/// snapshot on a new connection) and then created the destination snapshot
/// under the new target's identity over the old delta's content: a mirror
/// silently holding the wrong data under the right name, which every later
/// increment builds on (the source accepts a sink that reports the target
/// as its base). The snapshot now takes the identity of the delta that
/// landed (the record's landing target), never the latest handshake's.
#[test]
fn d2_a_later_handshake_never_renames_the_delta_that_landed() {
    let mut p = Pair::new(7001, 256);
    p.write(0, 32);
    let s1 = p.snapshot();
    let id1 = p.push(s1);
    p.run_until("the sink to take its snapshot", |p| {
        p.sink_job()
            .is_some_and(|j| j.phase == "taking the snapshot")
    });
    // Hold the destination's consistency points so the snapshot cannot be
    // listed while the source changes its mind.
    p.dst.disk().set_schedule(frozen());
    p.src_repl.cancel(id1).expect("cancelled");
    p.write(100, 16);
    let s2 = p.snapshot();
    let u2 = p.src_uuid(s2);
    let id2 = p.push(s2);
    p.run_until("the new handshake to reach the sink", |p| {
        p.sink_job().is_some_and(|j| j.snapshot_uuid == Some(u2))
    });
    assert_eq!(
        p.sink_job().map(|j| j.state),
        Some(JobState::Running),
        "the sink job was still taking its snapshot"
    );
    p.dst.disk().set_schedule(Schedule::default());
    let (job, _) = p.finish(id2);
    if job.state != JobState::Done {
        let again = p.push(s2);
        let (job, _) = p.finish(again);
        assert_eq!(job.state, JobState::Done, "{job:#?}");
    }
    p.assert_converged(s2);
}

/// D3: the sink record's rewind guard compared `pool_cp` with the pool's CP
/// only, and only when a job was created. A boot that rewinds by one CP
/// commits the boot under the lost CP's number, so `pool_cp > cp_seq` never
/// fired; the sink then cleared the `TornCp` hold that reported the loss and
/// resumed from a bitmap claiming chunks the lost CP had carried, and the
/// source skipped them. The record now carries the CP's nonce too, the
/// replicator checks it at attach, and a landing the pool no longer holds
/// starts over.
#[test]
fn d3_a_rewound_sink_never_trusts_the_chunks_the_lost_cp_carried() {
    let mut p = Pair::new(7002, 1024);
    for lba in (0..1024).step_by(8) {
        p.write(lba, 4);
    }
    let s1 = p.snapshot();
    let id = p.push(s1);
    p.run_until("fifty chunks to land", |p| p.sink_landed() >= 50);
    let before = p.sink_landed();
    p.run_until("one more landing", |p| p.sink_landed() > before);
    // A power cut, and the newest CP lost (a lying controller cache): the
    // boot rewinds by one CP.
    p.dst.crash_power().expect("the power cut");
    let slot = newest_slot(&mut p.dst);
    p.dst
        .disk()
        .corrupt_durable(slot.sb.map_roots.pba as usize * BLOCK_SIZE + 40, &[0x66; 4]);
    p.dst_repl = Replicator::new(
        repl_config(),
        Box::new(p.net.dialer()),
        Box::new(p.dst_store.clone()),
    );
    p.net.unlisten("dst");
    let boot = p.dst.boot().expect("the destination reboots");
    assert!(boot.rewound, "{boot:?}");
    p.dst_listener = p.net.listen("dst");
    p.dst_repl.attach(p.dst.engine());
    let (job, _) = p.finish(id);
    assert_eq!(job.state, JobState::Done, "{job:#?}");
    p.assert_converged(s1);
}

/// D4: a delta of another identity on a dirty mirror rolled the mirror back
/// to its base, but the record kept the abandoned delta's bitmap until the
/// rollback was durable. A sink job that ended in between (cancelled, or
/// the daemon killed) left a record that still claimed those chunks, so a
/// later push of the abandoned snapshot resumed and the source skipped
/// chunks the rollback had wiped. The landing is now aborted and saved
/// before the rollback (or deletion) is asked for.
#[test]
fn d4_a_rollback_forgets_the_landing_before_it_wipes_it() {
    let mut p = Pair::new(7004, 1024);
    for lba in (0..512).step_by(8) {
        p.write(lba, 8);
    }
    let s1 = p.snapshot();
    p.ship(s1);
    for lba in (0..1024).step_by(4) {
        p.write(lba, 4);
    }
    let s2 = p.snapshot();
    let shipped = p.sink_job().expect("the first sink job").id;
    let id_a = p.push(s2);
    // The new sink job's landings, not the finished one's.
    p.run_until("part of the second delta to land", |p| {
        p.sink_job()
            .is_some_and(|j| j.id != shipped && j.chunks_landed >= 20)
    });
    p.src_repl.cancel(id_a).expect("cancelled");
    p.write(7, 3);
    let s3 = p.snapshot();
    let id_b = p.push(s3);
    p.run_until("the sink to roll the mirror back", |p| {
        p.sink_job()
            .is_some_and(|j| j.phase == "rolling the mirror back to its base")
    });
    p.tick();
    let mirror = p.mirror().expect("the mirror");
    let sink_id = p.sink_job().expect("a sink job").id;
    p.dst_repl
        .cancel(sink_id)
        .expect("the sink job is cancelled");
    p.src_repl.cancel(id_b).expect("cancelled");
    p.run_until("the rollback to land", |p| {
        p.dst.engine().volume_ready(mirror)
    });
    // The abandoned target pushed again lands whole.
    let again = p.push(s2);
    let (job, _) = p.finish(again);
    assert_eq!(job.state, JobState::Done, "{job:#?}");
    p.assert_converged(s2);
}

/// D5: an existing offline volume named as a destination was marked as a
/// replica as soon as a sink job looked at it, which made the refusal of a
/// volume that is not a replication destination dead code; with no record
/// it read as a dirty mirror without a base, so the full transfer deleted
/// it and created it anew: the operator's volume and its data gone. It is
/// refused now, untouched.
#[test]
fn d5_an_existing_volume_is_refused_as_a_destination_not_deleted() {
    let mut p = Pair::new(7005, 256);
    p.dst.create_volume(0, 256).expect("the volume is created");
    p.dst.engine().set_volume_name(0, "mirror").expect("named");
    let c = p.dst.write(0, 0, 8).expect("the write completes");
    assert_eq!(c.status, Status::Ok);
    p.dst.set_online(0, false).expect("offline");
    let mut before = vec![0u8; BLOCK_SIZE];
    p.dst.engine().peek(0, 3, &mut before).expect("reads");
    p.write(0, 16);
    let s1 = p.snapshot();
    let id = p.push(s1);
    let (job, _) = p.finish(id);
    assert_ne!(job.state, JobState::Done, "{job:#?}");
    assert!(
        p.dst.engine().volume(0).is_some_and(|v| v.present),
        "the operator's volume is still there"
    );
    let mut after = vec![0u8; BLOCK_SIZE];
    p.dst.engine().peek(0, 3, &mut after).expect("reads");
    assert!(before == after, "the operator's data is untouched");
    assert!(
        !p.dst.engine().is_replica(0),
        "it is not marked as a mirror"
    );
}

/// D6: the replica mark lives in memory and was set again at boot only from
/// the sink records in the state directory, which defaults to a tmpfs. A
/// host reboot in the middle of a delta left the mirror an ordinary offline
/// volume, and `volume online` served a torn mix of the base and the next
/// snapshot. A volume whose newest transfer snapshot carries its own
/// replication pin (the mirror pins its base; `replicate release` clears
/// it) is a mirror, record or not.
#[test]
fn d6_a_mirror_whose_record_was_lost_stays_a_mirror() {
    let mut p = Pair::new(7006, 512);
    for lba in (0..512).step_by(8) {
        p.write(lba, 8);
    }
    let s1 = p.snapshot();
    p.ship(s1);
    for lba in (0..512).step_by(4) {
        p.write(lba, 4);
    }
    let s2 = p.snapshot();
    let id = p.push(s2);
    p.run_until("part of the delta to land", |p| p.sink_landed() >= 5);
    // The state directory is a tmpfs: a host reboot empties it.
    p.dst_store = MemStore::new();
    p.crash_dst(false);
    let mirror = p.mirror().expect("the mirror");
    assert!(
        matches!(
            p.dst.engine().set_volume_online(mirror, true),
            Err(EngineError::ReplicaVolume { .. })
        ),
        "a mirror holding part of a delta came online"
    );
    // And the transfer still completes, from the base the pool holds.
    let (job, _) = p.finish(id);
    if job.state != JobState::Done {
        let again = p.push(s2);
        let (job, _) = p.finish(again);
        assert_eq!(job.state, JobState::Done, "{job:#?}");
    }
    p.assert_converged(s2);
}

/// W1: a target snapshot that already existed on the sink pool, in another
/// volume (a second destination of the same source, or a failback), made
/// `create_snapshot_as` answer `SnapshotExists` forever; the sink read it
/// as "asked" and retried without end, the source waited for `Committed`
/// kept alive by the sink's pings, and neither job ever ended. Such a
/// target is now refused at the handshake.
#[test]
fn w1_a_target_held_by_another_volume_of_the_sink_pool_is_refused() {
    let mut p = Pair::new(7007, 256);
    p.write(0, 16);
    let s1 = p.snapshot();
    let first = p.push_to(s1, "other");
    let (job, _) = p.finish(first);
    assert_eq!(job.state, JobState::Done, "{job:#?}");
    let second = p.push(s1);
    let (job, sink) = p.finish(second);
    assert_eq!(job.state, JobState::Failed, "{job:#?}");
    let why = job
        .last_error
        .clone()
        .or(sink.and_then(|s| s.last_error))
        .unwrap_or_default();
    assert!(why.contains("already"), "{why}");
}
