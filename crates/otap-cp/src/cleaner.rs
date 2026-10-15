//! The segment cleaner (Step 5, E37): which data segments to compact, which
//! of their blocks are still referenced, and the accounting that follows.
//!
//! A data segment fills with stripes once and is never written again; as
//! its blocks are overwritten or their snapshots deleted, `live_bytes`
//! falls. The cleaner picks sealed data segments by lowest live count
//! (ties to the oldest birth), reads every stripe summary of a candidate,
//! and decides per block whether anything still references it: the live
//! map at `(vol, lba)` naming exactly this `(pseg, off)`, or a snapshot of
//! the volume taken at or after the block's birth whose pinned page names
//! it. A referenced block is relocated: written into a `Relocate` stripe
//! of the next consistency point with its birth and CRC unchanged, the
//! live map pointed at the copy through an ordinary delta, and every
//! snapshot that referenced it pointed there too by rewriting its map
//! pages copy on write ([`otap_map::MapSet::rewrite_pinned`]). When the
//! CP commits, nothing references the old segment: its live count is
//! zero and it is freed after the usual deferral, discarded whole on
//! flash.
//!
//! What the cleaner never does: move a block the generation being flushed
//! replaces anyway (it dies with the CP, unless a snapshot holds it), move
//! a block it cannot prove unreferenced or referenced (a damaged snapshot
//! root makes a segment uncleanable until the snapshot is deleted), or
//! run in the same CP as a snapshot deletion or a volume rollback, resize
//! or deletion, whose accounting it would cross.

use std::collections::HashSet;

use otap_format::{BlockKind, MapEntry, SegKind, SegState, StripeSummary, SECTORS_PER_BLOCK};

use crate::layout::DiskLayout;
use crate::segments::SegmentTable;
use crate::traits::IoError;

/// How and when the cleaner runs.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct CleanerPolicy {
    /// Segment utilisation (segments in use over segments) above which the
    /// cleaner runs on its own; 0.70 by default (S5-3).
    pub utilization: f64,
    /// Segments compacted by one consistency point at most; 2 by default.
    /// Doubled while the pool is tight (see `margin_segments`).
    pub max_segments_per_cp: u32,
    /// Run without being asked. `false` leaves only
    /// [`Engine::request_clean`](crate::Engine::request_clean).
    pub background: bool,
    /// The pool is *tight* when the free segments are down to the reserve
    /// plus this margin: the cleaner then runs whatever the utilisation,
    /// compacts twice as much per CP, and the engine runs consistency
    /// points of its own to advance the deferred frees, so a small pool
    /// does not refuse writes for want of a cleaning cycle. 4 by default.
    pub margin_segments: u32,
    /// A segment is a candidate only while its live blocks are below this
    /// fraction of its capacity: compacting one that is fuller moves
    /// nearly a segment to gain almost nothing, and a segment of relocated
    /// blocks (full but for the slack of partial stripes) must never be a
    /// candidate again, or the cleaner would chase its own tail. 0.80 by
    /// default.
    pub max_live_fraction: f64,
}

impl Default for CleanerPolicy {
    fn default() -> Self {
        Self {
            utilization: 0.70,
            max_segments_per_cp: 2,
            background: true,
            margin_segments: 4,
            max_live_fraction: 0.80,
        }
    }
}

/// A block the cleaner moves: where it is, who references it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RelocBlock {
    /// The volume.
    pub vol: u32,
    /// The block.
    pub lba: u64,
    /// The entry every referencer holds now.
    pub old: MapEntry,
    /// The live map references it (a delta moves it).
    pub live: bool,
    /// The snapshots that reference it (their pages are rewritten).
    pub snaps: Vec<u64>,
    /// Where the relocation stripe put it, once written.
    pub new: Option<MapEntry>,
}

/// One segment a consistency point compacts.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CleanTarget {
    /// The segment.
    pub pseg: u64,
    /// Its referenced blocks, in segment order.
    pub blocks: Vec<RelocBlock>,
    /// Blocks the segment table counted live before the scan (for the
    /// drift report).
    pub counted_live: u64,
}

/// Why a candidate was not scanned, or not taken.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SkipReason {
    /// A stripe summary could not be read.
    Read(IoError),
    /// A stripe summary does not parse or names another stripe.
    Summary(String),
    /// A snapshot that may reference a block is damaged: its root cannot
    /// be walked, so nothing in the segment can be proven unreferenced.
    DamagedSnapshot {
        /// The snapshot.
        snap_id: u64,
    },
    /// A map page the check needed does not verify.
    DamagedPage {
        /// The volume.
        vol: u32,
    },
}

impl core::fmt::Display for SkipReason {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Read(e) => write!(f, "a summary read failed: {e}"),
            Self::Summary(s) => write!(f, "a summary is bad: {s}"),
            Self::DamagedSnapshot { snap_id } => {
                write!(f, "snapshot {snap_id} is damaged and may reference a block")
            }
            Self::DamagedPage { vol } => write!(f, "a map page of volume {vol} is damaged"),
        }
    }
}

/// Who references a block, as the engine answers it for the scan, and the
/// device the scan reads summaries from.
pub trait References {
    /// Reads `dst.len()` bytes (whole blocks) at `pba` from the device.
    fn read(&mut self, pba: u64, dst: &mut [u8]) -> Result<(), IoError>;

    /// `true` when the live map of `vol` names `entry` at `lba` (same
    /// address). `Err` when the map page does not verify.
    fn live(&mut self, vol: u32, lba: u64, entry: &MapEntry) -> Result<bool, SkipReason>;

    /// The snapshots of `vol` taken at or after `birth_cp` whose pinned
    /// page names `entry` at `lba`. `Err` when one of them is damaged.
    fn snapshots(&mut self, vol: u32, lba: u64, entry: &MapEntry) -> Result<Vec<u64>, SkipReason>;

    /// `true` when the consistency point being built replaces `(vol, lba)`
    /// with a newer block or a hole: the block dies with the CP.
    fn replaced_by_cp(&self, vol: u32, lba: u64) -> bool;
}

/// Sealed data segments worth cleaning, best first: lowest live count,
/// then oldest birth. Segments the engine must not touch (`excluded`),
/// segments with no live block (the dead-segment path frees them) and
/// segments live beyond `max_live_fraction` of their capacity are left
/// out.
#[must_use]
pub fn candidates(
    table: &SegmentTable,
    layout: &DiskLayout,
    live_blocks: impl Fn(u64) -> u64,
    excluded: &HashSet<u64>,
    max_live_fraction: f64,
) -> Vec<(u64, u64, u32)> {
    let capacity =
        u64::from(layout.data_blocks_per_stripe()) * u64::from(layout.stripes_per_segment);
    let bound = (capacity as f64 * max_live_fraction.clamp(0.0, 1.0)) as u64;
    let mut out: Vec<(u64, u64, u32)> = table
        .iter()
        .filter(|(pseg, e)| {
            e.seg_kind() == Some(SegKind::Data)
                && e.seg_state() == Some(SegState::Sealed)
                && !excluded.contains(pseg)
        })
        .map(|(pseg, e)| (pseg, live_blocks(pseg), e.birth_cp))
        .filter(|(_, live, _)| *live > 0 && *live < bound.min(capacity))
        .collect();
    out.sort_by_key(|(pseg, live, birth)| (*live, *birth, *pseg));
    out
}

/// Scans segment `pseg`: every stripe summary, every data descriptor,
/// and who still references each block. A block nobody references is
/// dead and skipped; one only the CP's own writes replace is skipped too;
/// one a snapshot holds is kept for the snapshot even when the CP
/// replaces its live copy.
///
/// A stripe whose summary bytes are all zero was never written and holds
/// nothing the map can name. A summary that holds anything else and does
/// not parse is damage, not absence: the blocks behind it cannot be proven
/// unreferenced, so the segment is skipped ([`SkipReason::Summary`]); a
/// summary that parses but names another stripe (an older lap, a failed
/// CP's leftovers) is skipped as never written.
pub fn scan(
    layout: &DiskLayout,
    agg_id: &otap_format::Uuid,
    pseg: u64,
    refs: &mut dyn References,
) -> Result<Scan, SkipReason> {
    let summary_len = layout.summary.summary_bytes;
    let mut buf = vec![0u8; summary_len];
    let mut out = Vec::new();
    let mut referenced = 0u64;
    for idx in 0..layout.stripes_per_segment {
        let Some(pba) = layout.stripe_pba(pseg, idx) else {
            return Err(SkipReason::Summary(format!(
                "stripe {idx} is off the device"
            )));
        };
        refs.read(pba, &mut buf).map_err(SkipReason::Read)?;
        let summary = match StripeSummary::parse(&buf) {
            Ok(s) => s,
            Err(_) if buf.iter().all(|b| *b == 0) => continue,
            Err(e) => {
                return Err(SkipReason::Summary(format!(
                    "the summary of stripe {idx} does not verify: {e}"
                )))
            }
        };
        let hdr = summary.hdr();
        if hdr.agg_id != *agg_id || hdr.lseg != pseg || hdr.stripe_idx != idx {
            continue;
        }
        for (i, desc) in summary.descriptors().iter().enumerate() {
            if desc.kind() != Some(BlockKind::Data) {
                continue;
            }
            let off = layout.block_off(idx, i as u32);
            let Ok(old) = MapEntry::data(pseg, off, SECTORS_PER_BLOCK, desc.birth_cp, desc.crc32c)
            else {
                continue;
            };
            let (vol, lba) = (desc.vol(), desc.lba());
            let snaps = refs.snapshots(vol, lba, &old)?;
            let mut live = refs.live(vol, lba, &old)?;
            if live || !snaps.is_empty() {
                referenced += 1;
            }
            if live && refs.replaced_by_cp(vol, lba) {
                // The CP's own write or punch replaces the live copy; only
                // a snapshot can still want the old block.
                live = false;
            }
            if !live && snaps.is_empty() {
                continue;
            }
            out.push(RelocBlock {
                vol,
                lba,
                old,
                live,
                snaps,
                new: None,
            });
        }
    }
    Ok(Scan {
        blocks: out,
        referenced,
    })
}

/// What a scan of a candidate found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Scan {
    /// The blocks to relocate: live, or held by a snapshot (a live block
    /// the CP's own writes replace is left to the CP).
    pub blocks: Vec<RelocBlock>,
    /// Every block the live map or a snapshot references, the ones the
    /// CP replaces included: what the segment table should count live. A
    /// count below the table's means a stripe the scan could not read as
    /// written holds blocks it cannot vouch for, and the segment must not
    /// be drained.
    pub referenced: u64,
}
