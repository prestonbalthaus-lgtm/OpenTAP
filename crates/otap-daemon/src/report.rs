//! The engine's state as the protocol reports it.

use otap_control::{
    FindingSeverity, PoolReport, SnapshotReport, VerifyFinding, VerifyState as WireState,
    VerifyVerdict, VolumeReport,
};
use otap_cp::{
    Backstore, Clock, Engine, FrontDoor, Severity, SnapshotInfo, Verdict, VerifyReport,
    VerifyState, VolumeStatus,
};
use otap_format::{SnapKind, SnapOrigin, BLOCK_SIZE};

/// A pool verification's report as the protocol carries it (Step 6B).
#[must_use]
pub fn verify_report(r: &VerifyReport) -> otap_control::VerifyReport {
    otap_control::VerifyReport {
        state: match r.state {
            VerifyState::Running => WireState::Running,
            VerifyState::Done => WireState::Done,
            VerifyState::Cancelled => WireState::Cancelled,
            VerifyState::Aborted => WireState::Aborted,
        },
        verdict: r.verdict.map(|v| match v {
            Verdict::Clean => VerifyVerdict::Clean,
            Verdict::Warnings => VerifyVerdict::Warnings,
            Verdict::Damaged => VerifyVerdict::Damaged,
            Verdict::Incomplete => VerifyVerdict::Incomplete,
        }),
        cp_seq: r.cp_seq,
        phase: r.phase.as_str().to_string(),
        progress: r.progress,
        superblock_slots: r.counts.superblock_slots,
        regions: r.counts.regions,
        map_pages: r.counts.map_pages,
        blocks: r.counts.blocks,
        holes: r.counts.holes,
        stripes: r.counts.stripes,
        segments: r.counts.segments,
        bytes_read: r.counts.bytes_read,
        errors: r.errors,
        warnings: r.warnings,
        findings: r
            .findings
            .iter()
            .map(|f| VerifyFinding {
                kind: f.kind.as_str().to_string(),
                severity: match f.severity {
                    Severity::Error => FindingSeverity::Error,
                    Severity::Warning => FindingSeverity::Warning,
                },
                pba: f.pba,
                segment: f.pseg,
                volume_index: f.vol,
                lba: f.lba,
                snapshot: f.snap_id,
                detail: f.detail.clone(),
            })
            .collect(),
        reason: r.reason.clone(),
    }
}

/// A volume report from the engine's view and the device path, if any.
#[must_use]
pub fn volume_report(
    index: u32,
    v: &VolumeStatus,
    device: Option<String>,
    snapshots: usize,
) -> VolumeReport {
    VolumeReport {
        index,
        name: v.name.clone(),
        size_bytes: v.size_blocks * BLOCK_SIZE as u64,
        serving: v.serving,
        offline: v.offline,
        nvfail: v.nvfail.map(|c| format!("{c:?}").to_lowercase()),
        rollback_gen: v.rollback_gen,
        op_pending: v.op_pending.clone(),
        device,
        snapshots,
    }
}

/// A snapshot report.
#[must_use]
pub fn snapshot_report(s: &SnapshotInfo, volume: Option<String>) -> SnapshotReport {
    SnapshotReport {
        id: s.snap_id,
        uuid: s.uuid.to_string(),
        name: s.name.clone(),
        volume_index: s.vol,
        volume,
        cp_seq: s.cp_seq,
        created_wall_ns: s.created_wall_ns,
        consistency: match s.kind {
            SnapKind::Fsfreeze => "fsfreeze",
            SnapKind::Crash => "crash",
        }
        .to_string(),
        origin: match s.origin {
            SnapOrigin::Scheduled => "scheduled",
            SnapOrigin::User => "user",
            SnapOrigin::Transfer => "transfer",
            SnapOrigin::Divergent => "divergent",
        }
        .to_string(),
        flags: s.flags,
        damaged: s.damaged,
        deleting: s.deleting,
    }
}

/// The pool report.
#[allow(clippy::too_many_arguments)]
#[must_use]
pub fn pool_report<B: Backstore, F: FrontDoor, C: Clock>(
    engine: &Engine<B, F, C>,
    device: &str,
    arena_bytes: u64,
    arena: &str,
    cp_interval_secs: u32,
    last_boot: &str,
) -> PoolReport {
    let status = engine.pool_status();
    let stats = engine.stats();
    let layout = engine.layout();
    PoolReport {
        device: device.to_string(),
        agg_id: engine.config().agg_id.to_string(),
        cp_seq: status.cp_seq,
        fold_cp: status.fold_cp,
        serving: status.serving,
        free_segments: status.free_segments,
        segments: status.segments,
        reserve_segments: status.reserve_segments,
        segment_bytes: layout.segment_bytes,
        stripe_bytes: layout.stripe_bytes,
        utilization: status.utilization,
        admissible_bytes: status.admissible_blocks * BLOCK_SIZE as u64,
        unflushed_bytes: status.unflushed_blocks * BLOCK_SIZE as u64,
        arena_occupancy: status.arena_occupancy,
        arena_bytes,
        arena: arena.to_string(),
        cp_interval_secs,
        pool_full: status.pool_full,
        flush_policy: format!("{:?}", status.flush_policy),
        flush_opaque: status.flush_opaque,
        discard: format!("{:?}", status.discard),
        warnings: status.warnings.iter().map(ToString::to_string).collect(),
        volumes: status.volumes,
        snapshots: status.snapshots,
        cleaned_segments: status.cleaned_segments,
        relocated_blocks: status.relocated_blocks,
        cps: stats.cps,
        writes: stats.writes,
        reads: stats.reads,
        last_boot: last_boot.to_string(),
    }
}
