//! The fake block device.
//!
//! Three layers model what a real device does with a write:
//!
//! - **pending**: submitted, not yet completed; a crash may leave any part
//!   of it on the medium;
//! - **cached**: completed, not yet flushed (the device's volatile cache);
//!   reads see it, a power cut may lose or tear it;
//! - **durable**: on the medium; survives a power cut.
//!
//! A flush moves everything cached to durable, unless the device is
//! `flush_opaque` (a controller that acknowledges flushes it does not
//! perform, C07): then nothing moves and only FUA writes (superblock and
//! intent slots) are durable on completion. Completions are delivered in a
//! seeded order, a few per poll, so the engine's protocol is exercised under
//! reordering; a crash tears each unsafe write at 512-byte sector
//! granularity: dropped, applied whole, or a random subset of its sectors.

use std::collections::VecDeque;

use otap_cp::{Backstore, Busy, DevClass, DiskLayout, Done, IoError, MediaProfile, Tag};
use otap_format::{IntentSlot, IntoBytes, SuperblockSlot, BLOCK_SIZE, SECTOR_SIZE};

use crate::rng::SplitMix64;

/// What kind of write a pending operation is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum OpKind {
    /// A data stripe.
    Stripe,
    /// A metadata stripe.
    Meta,
    /// A flush.
    Flush,
    /// A superblock slot (FUA).
    Commit,
    /// An intent slot (FUA).
    Intent,
}

#[derive(Debug, Clone)]
struct Pending {
    tag: Tag,
    kind: OpKind,
    offset: usize,
    data: Vec<u8>,
}

/// How the scheduler completes pending operations.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Schedule {
    /// At most this many completions per poll (0 stalls the device).
    pub max_per_poll: usize,
    /// Complete in a random order rather than first in, first out.
    pub reorder: bool,
    /// Pending operations above which submissions answer `Busy`.
    pub queue_depth: usize,
}

impl Default for Schedule {
    fn default() -> Self {
        Self {
            max_per_poll: 4,
            reorder: true,
            queue_depth: 64,
        }
    }
}

/// How a power cut treats each unsafe write (weights).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TearPolicy {
    /// Nothing reaches the medium.
    pub drop: u32,
    /// All of it does.
    pub whole: u32,
    /// A random subset of its sectors does.
    pub torn: u32,
}

impl Default for TearPolicy {
    fn default() -> Self {
        Self {
            drop: 1,
            whole: 1,
            torn: 2,
        }
    }
}

/// What a power cut did.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct CrashReport {
    /// Writes dropped entirely.
    pub dropped: u32,
    /// Writes applied whole.
    pub whole: u32,
    /// Writes torn.
    pub torn: u32,
}

/// Counters.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct BackstoreStats {
    /// Stripe writes submitted.
    pub stripes: u64,
    /// Metadata writes submitted.
    pub metas: u64,
    /// Flushes submitted.
    pub flushes: u64,
    /// Superblock commits submitted.
    pub commits: u64,
    /// Intent writes submitted.
    pub intents: u64,
    /// Reads served.
    pub reads: u64,
    /// Submissions refused with `Busy`.
    pub busy: u64,
}

/// The fake device.
#[derive(Debug, Clone)]
pub struct SimBackstore {
    profile: MediaProfile,
    layout: DiskLayout,
    durable: Vec<u8>,
    current: Vec<u8>,
    cached: Vec<(usize, usize)>,
    pending: VecDeque<Pending>,
    completions: Vec<Done>,
    rng: SplitMix64,
    schedule: Schedule,
    tear: TearPolicy,
    stats: BackstoreStats,
}

impl SimBackstore {
    /// A zeroed device for `profile`, with a scheduler seeded by `seed`.
    pub fn new(profile: MediaProfile, seed: u64) -> Result<Self, otap_cp::LayoutError> {
        let layout = DiskLayout::new(&profile.geom)?;
        let capacity = profile.geom.capacity_bytes as usize;
        Ok(Self {
            profile,
            layout,
            durable: vec![0; capacity],
            current: vec![0; capacity],
            cached: Vec::new(),
            pending: VecDeque::new(),
            completions: Vec::new(),
            rng: SplitMix64::new(seed),
            schedule: Schedule::default(),
            tear: TearPolicy::default(),
            stats: BackstoreStats::default(),
        })
    }

    /// The layout.
    #[must_use]
    pub const fn layout(&self) -> &DiskLayout {
        &self.layout
    }

    /// Changes the scheduler.
    pub fn set_schedule(&mut self, schedule: Schedule) {
        self.schedule = schedule;
    }

    /// The scheduler.
    #[must_use]
    pub const fn schedule(&self) -> Schedule {
        self.schedule
    }

    /// Changes the tear policy.
    pub fn set_tear_policy(&mut self, tear: TearPolicy) {
        self.tear = tear;
    }

    /// Counters.
    #[must_use]
    pub const fn stats(&self) -> BackstoreStats {
        self.stats
    }

    /// Operations submitted and not completed.
    #[must_use]
    pub fn pending(&self) -> usize {
        self.pending.len()
    }

    /// The kinds of the pending operations, oldest first.
    #[must_use]
    pub fn pending_kinds(&self) -> Vec<OpKind> {
        self.pending.iter().map(|p| p.kind).collect()
    }

    /// Completed writes not yet flushed.
    #[must_use]
    pub fn cached(&self) -> usize {
        self.cached.len()
    }

    /// The bytes on the medium (what a power cut keeps).
    #[must_use]
    pub fn durable(&self) -> &[u8] {
        &self.durable
    }

    /// Overwrites bytes on the medium directly: a fault injection.
    pub fn corrupt_durable(&mut self, offset: usize, bytes: &[u8]) {
        let end = (offset + bytes.len()).min(self.durable.len());
        if offset < end {
            let n = end - offset;
            self.durable[offset..end].copy_from_slice(&bytes[..n]);
            self.current[offset..end].copy_from_slice(&bytes[..n]);
        }
    }

    /// A power cut: pending and cached writes are dropped, applied whole or
    /// torn; the device forgets everything else.
    pub fn crash_power(&mut self) -> CrashReport {
        let mut report = CrashReport::default();
        let pending: Vec<Pending> = self.pending.drain(..).collect();
        for p in pending {
            if p.kind == OpKind::Flush {
                continue;
            }
            self.land_unsafely(p.offset, &p.data, &mut report);
        }
        let cached = std::mem::take(&mut self.cached);
        for (offset, len) in cached {
            let data = self.current[offset..offset + len].to_vec();
            self.land_unsafely(offset, &data, &mut report);
        }
        self.current.copy_from_slice(&self.durable);
        self.completions.clear();
        report
    }

    /// A daemon crash: the device keeps working. Every pending write
    /// completes (into the cache) and the completions are lost with the
    /// process.
    pub fn crash_daemon(&mut self) {
        let pending: Vec<Pending> = self.pending.drain(..).collect();
        for p in pending {
            self.complete_now(p);
        }
        self.completions.clear();
    }

    fn land_unsafely(&mut self, offset: usize, data: &[u8], report: &mut CrashReport) {
        let total = self.tear.drop + self.tear.whole + self.tear.torn;
        let pick = self.rng.below(u64::from(total.max(1))) as u32;
        if pick < self.tear.drop {
            report.dropped += 1;
            return;
        }
        if pick < self.tear.drop + self.tear.whole {
            report.whole += 1;
            self.durable[offset..offset + data.len()].copy_from_slice(data);
            return;
        }
        report.torn += 1;
        let sectors = data.len().div_ceil(SECTOR_SIZE);
        for s in 0..sectors {
            if self.rng.chance(1, 2) {
                let start = s * SECTOR_SIZE;
                let end = ((s + 1) * SECTOR_SIZE).min(data.len());
                self.durable[offset + start..offset + end].copy_from_slice(&data[start..end]);
            }
        }
    }

    fn complete_now(&mut self, p: Pending) {
        match p.kind {
            OpKind::Flush => {
                if !self.profile.geom.flush_opaque {
                    let cached = std::mem::take(&mut self.cached);
                    for (offset, len) in cached {
                        self.durable[offset..offset + len]
                            .copy_from_slice(&self.current[offset..offset + len]);
                    }
                }
            }
            OpKind::Commit | OpKind::Intent => {
                let end = p.offset + p.data.len();
                self.current[p.offset..end].copy_from_slice(&p.data);
                self.durable[p.offset..end].copy_from_slice(&p.data);
            }
            OpKind::Stripe | OpKind::Meta => {
                let end = p.offset + p.data.len();
                self.current[p.offset..end].copy_from_slice(&p.data);
                self.cached.push((p.offset, p.data.len()));
            }
        }
        self.completions.push(Done {
            tag: p.tag,
            result: Ok(()),
        });
    }

    fn submit(&mut self, kind: OpKind, pba: u64, data: Vec<u8>, tag: Tag) -> Result<(), Busy> {
        if self.pending.len() >= self.schedule.queue_depth {
            self.stats.busy += 1;
            return Err(Busy);
        }
        let offset = pba as usize * BLOCK_SIZE;
        if offset + data.len() > self.durable.len() {
            self.completions.push(Done {
                tag,
                result: Err(IoError::OutOfRange {
                    pba,
                    bytes: data.len(),
                }),
            });
            return Ok(());
        }
        self.pending.push_back(Pending {
            tag,
            kind,
            offset,
            data,
        });
        Ok(())
    }
}

impl Backstore for SimBackstore {
    fn profile(&self) -> &MediaProfile {
        &self.profile
    }

    fn write_stripe(&mut self, pba: u64, image: &[u8], tag: Tag) -> Result<(), Busy> {
        self.stats.stripes += 1;
        self.submit(OpKind::Stripe, pba, image.to_vec(), tag)
    }

    fn write_meta(&mut self, pba: u64, image: &[u8], tag: Tag) -> Result<(), Busy> {
        self.stats.metas += 1;
        self.submit(OpKind::Meta, pba, image.to_vec(), tag)
    }

    fn flush(&mut self, _class: DevClass, tag: Tag) -> Result<(), Busy> {
        self.stats.flushes += 1;
        self.submit(OpKind::Flush, 0, Vec::new(), tag)
    }

    fn commit(&mut self, pba: u64, slot: &SuperblockSlot, tag: Tag) -> Result<(), Busy> {
        self.stats.commits += 1;
        self.submit(OpKind::Commit, pba, slot.as_bytes().to_vec(), tag)
    }

    fn write_intent(&mut self, pba: u64, slot: &IntentSlot, tag: Tag) -> Result<(), Busy> {
        self.stats.intents += 1;
        self.submit(OpKind::Intent, pba, slot.as_bytes().to_vec(), tag)
    }

    fn read(&mut self, pba: u64, dst: &mut [u8]) -> Result<(), IoError> {
        self.stats.reads += 1;
        if !dst.len().is_multiple_of(BLOCK_SIZE) {
            return Err(IoError::BadLength { bytes: dst.len() });
        }
        let offset = pba as usize * BLOCK_SIZE;
        let end = offset + dst.len();
        if end > self.current.len() {
            return Err(IoError::OutOfRange {
                pba,
                bytes: dst.len(),
            });
        }
        dst.copy_from_slice(&self.current[offset..end]);
        Ok(())
    }

    fn poll(&mut self, out: &mut Vec<Done>) -> usize {
        let budget = if self.schedule.max_per_poll == 0 {
            0
        } else {
            self.rng.range(1, self.schedule.max_per_poll as u64) as usize
        };
        for _ in 0..budget {
            if self.pending.is_empty() {
                break;
            }
            let at = if self.schedule.reorder {
                self.rng.below(self.pending.len() as u64) as usize
            } else {
                0
            };
            let p = self.pending.remove(at).expect("index within pending");
            self.complete_now(p);
        }
        let n = self.completions.len();
        out.append(&mut self.completions);
        n
    }
}
