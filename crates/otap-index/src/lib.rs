//! `otap-index`: the volatile dirty index and the cut (round 3, sections 1.3
//! and 7.2; Step 3 brief, "Opus-Storage-Index").
//!
//! Every acknowledged write sits in the arena until a consistency point
//! carries it to disk. The index says where: per volume, a `BTreeMap` from
//! LBA to a [`DirtyExtent`], a run of consecutive blocks held in consecutive
//! arena slots ([`otap_arena::RingRef`]). Inserting a write splits or trims
//! whatever it overlaps and merges with neighbours whose slots continue its
//! run, so a stream of 4 KiB writes becomes one extent the flusher copies in
//! one pass.
//!
//! # Generations and the cut
//!
//! Writes go into the *active* [`Generation`]. A *cut* freezes it: the active
//! pointer is swapped to a fresh generation (lock-free for the writers, who
//! load the pointer without a lock), then the cutter waits until every
//! holder of the old pointer has let go: inserts in progress, and readers
//! inside [`Index::with_active`] or [`Index::lookup`]. That wait is the grace
//! period; it never waits for a flush. Keep what runs under `with_active`
//! short, and never cut from inside it (the cut would wait for itself). The
//! frozen generation joins a queue of at most `ring - 1` generations (eight
//! in the ring, section 1.3) that the flusher drains in order, one
//! consistency point each.
//!
//! # Ordering
//!
//! Within a generation the later insert wins its whole range: the index is a
//! record of what the client wrote, in the order the engine placed it. Each
//! insert carries the write's sequence number, and [`Index::insert`] refuses
//! a sequence below the newest one the volume has seen
//! ([`IndexError::StaleSeq`]; equal is another piece of the same write), so
//! insertion order and sequence order are one order and a merged extent's
//! sequence (the highest of its parts) is the newest write it holds. Across
//! generations the newer generation wins regardless, because that is the
//! order consistency points reach the disk.
//!
//! A [`lookup`](Index::lookup) answers from the active generation first, then
//! the frozen ones newest first.

#![forbid(unsafe_code)]
#![deny(missing_docs)]
#![warn(rust_2018_idioms, unreachable_pub)]

use core::fmt;
use std::collections::{BTreeMap, VecDeque};
use std::sync::atomic::{fence, AtomicU64, Ordering};
use std::sync::Arc;

use arc_swap::ArcSwap;
use parking_lot::Mutex;

use otap_arena::RingRef;
use otap_format::{MAX_VOL, MAX_VOLS};

/// A generation's identity: 1 for the first, counting up.
pub type GenId = u64;

/// Arena runs an insert superseded: `(first slot, blocks)`. Their data is
/// dead, and the engine releases them.
pub type Superseded = Vec<(RingRef, u32)>;

/// Blocks in a list of superseded runs.
#[must_use]
pub fn superseded_blocks(runs: &[(RingRef, u32)]) -> u64 {
    runs.iter().map(|(_, b)| u64::from(*b)).sum()
}

/// Generations in the ring: the active one plus at most seven frozen.
pub const DEFAULT_RING: usize = 8;

/// A run of dirty blocks: LBAs `lba..lba + blocks` (the map key is `lba`)
/// held in arena slots `ring.slot..ring.slot + blocks` on lap `ring.lap`;
/// or, with `hole` set, a run of blocks a discard punched, which holds no
/// data and whose `ring` names the one arena slot carrying the discard's
/// record (Step 4, E23). A hole reads as zeros and reaches the disk as map
/// holes, never as stripe blocks.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct DirtyExtent {
    /// The first slot, or the record slot of a hole.
    pub ring: RingRef,
    /// Blocks in the run, at least 1.
    pub blocks: u32,
    /// The sequence number of the write (the highest, once merged).
    pub seq: u64,
    /// `true` for a punched hole.
    pub hole: bool,
}

impl DirtyExtent {
    /// A data run.
    #[must_use]
    pub const fn data(ring: RingRef, blocks: u32, seq: u64) -> Self {
        Self {
            ring,
            blocks,
            seq,
            hole: false,
        }
    }

    /// A punched hole whose record lives in slot `record`.
    #[must_use]
    pub const fn hole(record: RingRef, blocks: u32, seq: u64) -> Self {
        Self {
            ring: record,
            blocks,
            seq,
            hole: true,
        }
    }

    /// The part of the extent from block `from` to block `to` (exclusive),
    /// counted from its first block. A hole's record slot stays the same.
    #[must_use]
    pub const fn slice(&self, from: u32, to: u32) -> Self {
        Self {
            ring: if self.hole {
                self.ring
            } else {
                self.ring.offset(from)
            },
            blocks: to - from,
            seq: self.seq,
            hole: self.hole,
        }
    }

    /// The slot of block `k` of the extent (the record slot for a hole).
    #[must_use]
    pub const fn slot_of(&self, k: u32) -> RingRef {
        if self.hole {
            self.ring
        } else {
            self.ring.offset(k)
        }
    }
}

/// Where a block is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Hit {
    /// The block's own slot, or the record slot of the hole covering it.
    pub ring: RingRef,
    /// The sequence number of the extent holding it.
    pub seq: u64,
    /// The generation holding it.
    pub gen: GenId,
    /// `true` when the block is a punched hole (zeros, no slot data).
    pub hole: bool,
}

/// Why an operation was refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum IndexError {
    /// A volume index at or above [`MAX_VOLS`].
    BadVol {
        /// The volume.
        vol: u32,
    },
    /// An extent of zero blocks or one that runs past the LBA space.
    BadExtent {
        /// The LBA.
        lba: u64,
        /// The blocks.
        blocks: u32,
    },
    /// The sequence number is below the newest one the volume has seen.
    StaleSeq {
        /// The volume.
        vol: u32,
        /// The sequence offered.
        seq: u64,
        /// The highest sequence the volume has seen.
        newest: u64,
    },
    /// Every ring slot but the active one holds a frozen generation; a
    /// consistency point must retire one before the next cut.
    RingFull {
        /// Frozen generations.
        frozen: usize,
    },
    /// The generation named is not the oldest frozen one.
    NotOldest {
        /// The generation named.
        gen: GenId,
    },
}

impl fmt::Display for IndexError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::BadVol { vol } => write!(f, "volume {vol} is beyond {MAX_VOL}"),
            Self::BadExtent { lba, blocks } => {
                write!(
                    f,
                    "an extent of {blocks} blocks at LBA {lba} is out of bounds"
                )
            }
            Self::StaleSeq { vol, seq, newest } => write!(
                f,
                "sequence {seq} for volume {vol} is not above the newest ({newest})"
            ),
            Self::RingFull { frozen } => {
                write!(f, "generation ring full: {frozen} frozen generations")
            }
            Self::NotOldest { gen } => write!(f, "generation {gen} is not the oldest frozen one"),
        }
    }
}

impl std::error::Error for IndexError {}

/// One volume's dirty extents in one generation.
#[derive(Debug, Default, Clone)]
pub struct VolumeDirty {
    map: BTreeMap<u64, DirtyExtent>,
    blocks: u64,
}

impl VolumeDirty {
    /// An empty map.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            map: BTreeMap::new(),
            blocks: 0,
        }
    }

    /// Inserts `ext` at `lba`: it wins its whole range, so the overlapped
    /// blocks of earlier extents are trimmed or split away; what remains is
    /// merged with neighbours whose slot runs continue it. Returns the arena
    /// runs of the data superseded, whose slots the caller releases (a hole
    /// superseded releases nothing: its record slot is released when its
    /// generation's consistency point commits). (The index above refuses
    /// out-of-order sequences; this map trusts the order it is called in.)
    pub fn insert(&mut self, lba: u64, ext: DirtyExtent) -> Superseded {
        debug_assert!(ext.blocks > 0);
        let end = lba + u64::from(ext.blocks);
        let mut touched: Vec<(u64, DirtyExtent)> = Vec::new();
        if let Some((&k, &e)) = self.map.range(..lba).next_back() {
            if k + u64::from(e.blocks) > lba {
                touched.push((k, e));
            }
        }
        for (&k, &e) in self.map.range(lba..end) {
            touched.push((k, e));
        }
        let mut superseded: Superseded = Vec::new();
        for (k, e) in touched {
            self.remove(k, &e);
            let e_end = k + u64::from(e.blocks);
            let ov_start = k.max(lba);
            let ov_end = e_end.min(end);
            if k < ov_start {
                self.put(k, e.slice(0, (ov_start - k) as u32));
            }
            if ov_end < e_end {
                self.put(ov_end, e.slice((ov_end - k) as u32, e.blocks));
            }
            if !e.hole {
                superseded.push((
                    e.ring.offset((ov_start - k) as u32),
                    (ov_end - ov_start) as u32,
                ));
            }
        }
        self.put(lba, ext);
        self.coalesce_around(lba, end);
        superseded
    }

    /// The slot holding `lba`, with the extent's sequence number (for a
    /// hole, its record slot; see [`find`](Self::find)).
    #[must_use]
    pub fn lookup(&self, lba: u64) -> Option<(RingRef, u64)> {
        self.find(lba)
            .map(|(k, e)| (e.slot_of((lba - k) as u32), e.seq))
    }

    /// The extent holding `lba`, with its first LBA.
    #[must_use]
    pub fn find(&self, lba: u64) -> Option<(u64, &DirtyExtent)> {
        let (&k, e) = self.map.range(..=lba).next_back()?;
        if k + u64::from(e.blocks) > lba {
            Some((k, e))
        } else {
            None
        }
    }

    /// Dirty blocks that hold data (holes excluded).
    #[must_use]
    pub fn data_blocks(&self) -> u64 {
        self.map
            .values()
            .filter(|e| !e.hole)
            .map(|e| u64::from(e.blocks))
            .sum()
    }

    /// The extents in LBA order.
    pub fn iter(&self) -> impl Iterator<Item = (u64, &DirtyExtent)> + '_ {
        self.map.iter().map(|(k, e)| (*k, e))
    }

    /// Dirty blocks.
    #[must_use]
    pub const fn blocks(&self) -> u64 {
        self.blocks
    }

    /// Extents.
    #[must_use]
    pub fn extents(&self) -> usize {
        self.map.len()
    }

    /// `true` with no dirty block.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.map.is_empty()
    }

    fn put(&mut self, lba: u64, ext: DirtyExtent) {
        if ext.blocks == 0 {
            return;
        }
        self.blocks += u64::from(ext.blocks);
        let old = self.map.insert(lba, ext);
        debug_assert!(old.is_none(), "extents never overlap");
    }

    fn remove(&mut self, lba: u64, ext: &DirtyExtent) {
        if self.map.remove(&lba).is_some() {
            self.blocks -= u64::from(ext.blocks);
        }
    }

    /// Merges extents that touch `[lba, end)` with neighbours whose slot run
    /// continues theirs.
    fn coalesce_around(&mut self, lba: u64, end: u64) {
        let start = self.map.range(..lba).next_back().map_or(lba, |(k, _)| *k);
        let keys: Vec<u64> = self.map.range(start..=end).map(|(k, _)| *k).collect();
        self.coalesce_keys(keys);
    }

    fn coalesce_keys(&mut self, mut keys: Vec<u64>) {
        let mut i = 0;
        while i + 1 < keys.len() {
            let (a_key, b_key) = (keys[i], keys[i + 1]);
            let (Some(a), Some(b)) = (self.map.get(&a_key).copied(), self.map.get(&b_key).copied())
            else {
                i += 1;
                continue;
            };
            // Data runs merge when their slots continue each other; holes
            // merge when they share one record slot (two pieces of a hole a
            // later write split, or a re-attach produced).
            let adjacent = a_key + u64::from(a.blocks) == b_key
                && a.hole == b.hole
                && u64::from(a.blocks) + u64::from(b.blocks) <= u64::from(u32::MAX);
            let contiguous = adjacent
                && if a.hole {
                    a.ring == b.ring
                } else {
                    a.ring.lap == b.ring.lap && a.ring.slot + a.blocks == b.ring.slot
                };
            if contiguous {
                self.remove(b_key, &b);
                let merged = DirtyExtent {
                    ring: a.ring,
                    blocks: a.blocks + b.blocks,
                    seq: a.seq.max(b.seq),
                    hole: a.hole,
                };
                self.blocks -= u64::from(a.blocks);
                self.map.insert(a_key, merged);
                self.blocks += u64::from(merged.blocks);
                keys.remove(i + 1);
            } else {
                i += 1;
            }
        }
    }
}

/// A generation: every volume's dirty extents between two cuts.
#[derive(Debug)]
pub struct Generation {
    id: GenId,
    vols: Vec<Mutex<VolumeDirty>>,
    blocks: AtomicU64,
    inserts: AtomicU64,
    opened_ns: u64,
    cut_ns: AtomicU64,
}

impl Generation {
    fn new(id: GenId, now_ns: u64) -> Self {
        Self {
            id,
            vols: (0..MAX_VOLS)
                .map(|_| Mutex::new(VolumeDirty::new()))
                .collect(),
            blocks: AtomicU64::new(0),
            inserts: AtomicU64::new(0),
            opened_ns: now_ns,
            cut_ns: AtomicU64::new(0),
        }
    }

    /// The identity.
    #[must_use]
    pub const fn id(&self) -> GenId {
        self.id
    }

    /// Dirty blocks over every volume (net of superseded blocks).
    #[must_use]
    pub fn blocks(&self) -> u64 {
        self.blocks.load(Ordering::Relaxed)
    }

    /// Dirty bytes.
    #[must_use]
    pub fn bytes(&self) -> u64 {
        self.blocks() * otap_format::BLOCK_SIZE as u64
    }

    /// Inserts so far.
    #[must_use]
    pub fn inserts(&self) -> u64 {
        self.inserts.load(Ordering::Relaxed)
    }

    /// When the generation was opened (the clock's `mono_ns`).
    #[must_use]
    pub const fn opened_ns(&self) -> u64 {
        self.opened_ns
    }

    /// When it was cut, or 0 while active.
    #[must_use]
    pub fn cut_ns(&self) -> u64 {
        self.cut_ns.load(Ordering::Relaxed)
    }

    /// `true` with no dirty block.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.blocks() == 0
    }

    /// Runs `f` on one volume's map.
    pub fn with_volume<R>(&self, vol: u32, f: impl FnOnce(&VolumeDirty) -> R) -> Option<R> {
        let guard = self.vols.get(vol as usize)?.lock();
        Some(f(&guard))
    }

    /// Runs `f` on every volume that has dirty blocks, in volume order.
    pub fn for_each_dirty_volume(&self, mut f: impl FnMut(u32, &VolumeDirty)) {
        for (vol, cell) in self.vols.iter().enumerate() {
            let guard = cell.lock();
            if !guard.is_empty() {
                f(vol as u32, &guard);
            }
        }
    }

    /// The volumes with dirty blocks, in order.
    #[must_use]
    pub fn dirty_volumes(&self) -> Vec<u32> {
        let mut out = Vec::new();
        self.for_each_dirty_volume(|vol, _| out.push(vol));
        out
    }

    fn insert(&self, vol: u32, lba: u64, ext: DirtyExtent) -> Result<Superseded, IndexError> {
        let cell = self
            .vols
            .get(vol as usize)
            .ok_or(IndexError::BadVol { vol })?;
        let mut guard = cell.lock();
        let before = guard.blocks();
        let superseded = guard.insert(lba, ext);
        let after = guard.blocks();
        drop(guard);
        if after >= before {
            self.blocks.fetch_add(after - before, Ordering::Relaxed);
        } else {
            self.blocks.fetch_sub(before - after, Ordering::Relaxed);
        }
        self.inserts.fetch_add(1, Ordering::Relaxed);
        Ok(superseded)
    }

    fn lookup(&self, vol: u32, lba: u64) -> Option<Hit> {
        let cell = self.vols.get(vol as usize)?;
        let guard = cell.lock();
        guard.find(lba).map(|(k, e)| Hit {
            ring: e.slot_of((lba - k) as u32),
            seq: e.seq,
            gen: self.id,
            hole: e.hole,
        })
    }
}

/// The dirty index: the active generation and the frozen queue.
#[derive(Debug)]
pub struct Index {
    active: ArcSwap<Generation>,
    frozen: Mutex<VecDeque<Arc<Generation>>>,
    ring: usize,
    next_id: AtomicU64,
    cut_lock: Mutex<()>,
    grace_spins: AtomicU64,
    /// The highest sequence inserted per volume, across generations.
    newest_seq: Vec<AtomicU64>,
}

impl Index {
    /// An index whose ring holds `ring` generations (at least 2); generation
    /// 1 is active.
    #[must_use]
    pub fn new(ring: usize, now_ns: u64) -> Self {
        let ring = ring.max(2);
        Self {
            active: ArcSwap::from_pointee(Generation::new(1, now_ns)),
            frozen: Mutex::new(VecDeque::with_capacity(ring)),
            ring,
            next_id: AtomicU64::new(2),
            cut_lock: Mutex::new(()),
            grace_spins: AtomicU64::new(0),
            newest_seq: (0..MAX_VOLS).map(|_| AtomicU64::new(0)).collect(),
        }
    }

    /// Generations the ring holds.
    #[must_use]
    pub const fn ring(&self) -> usize {
        self.ring
    }

    /// Inserts a write into the active generation and returns which
    /// generation took it and the arena runs it superseded (dead data the
    /// caller releases). The sequence must not be below the volume's newest
    /// (equal: another piece of the same write). Lock-free with respect to
    /// the cut: the active pointer is loaded without a lock, and a concurrent
    /// cut waits for this insert to finish.
    pub fn insert(
        &self,
        vol: u32,
        lba: u64,
        ext: DirtyExtent,
    ) -> Result<(GenId, Superseded), IndexError> {
        if ext.blocks == 0
            || lba
                .checked_add(u64::from(ext.blocks))
                .is_none_or(|e| e > otap_format::MAX_LBA + 1)
        {
            return Err(IndexError::BadExtent {
                lba,
                blocks: ext.blocks,
            });
        }
        let newest = self
            .newest_seq
            .get(vol as usize)
            .ok_or(IndexError::BadVol { vol })?;
        // Equal is fine: a write whose run was split by a later overwrite
        // comes back from a re-attach as several pieces with one sequence.
        let previous = newest.fetch_max(ext.seq, Ordering::AcqRel);
        if previous > ext.seq {
            return Err(IndexError::StaleSeq {
                vol,
                seq: ext.seq,
                newest: previous,
            });
        }
        let active = self.active.load();
        let superseded = active.insert(vol, lba, ext)?;
        Ok((active.id(), superseded))
    }

    /// The highest sequence inserted for `vol` so far (0 for none).
    #[must_use]
    pub fn newest_seq(&self, vol: u32) -> u64 {
        self.newest_seq
            .get(vol as usize)
            .map_or(0, |n| n.load(Ordering::Acquire))
    }

    /// Where `lba` of `vol` is, if dirty: the active generation first, then
    /// the frozen ones newest first.
    #[must_use]
    pub fn lookup(&self, vol: u32, lba: u64) -> Option<Hit> {
        if vol > MAX_VOL {
            return None;
        }
        if let Some(hit) = self.active.load().lookup(vol, lba) {
            return Some(hit);
        }
        let frozen = self.frozen.lock();
        for gen in frozen.iter().rev() {
            if let Some(hit) = gen.lookup(vol, lba) {
                return Some(hit);
            }
        }
        None
    }

    /// Runs `f` on the active generation. A cut waits for `f` to return;
    /// never cut from inside it.
    pub fn with_active<R>(&self, f: impl FnOnce(&Generation) -> R) -> R {
        let active = self.active.load();
        f(&active)
    }

    /// The active generation's identity.
    #[must_use]
    pub fn active_id(&self) -> GenId {
        self.active.load().id()
    }

    /// Dirty blocks in the active generation.
    #[must_use]
    pub fn active_blocks(&self) -> u64 {
        self.active.load().blocks()
    }

    /// Dirty blocks in every generation, active and frozen.
    #[must_use]
    pub fn unflushed_blocks(&self) -> u64 {
        let frozen: u64 = self.frozen.lock().iter().map(|g| g.blocks()).sum();
        frozen + self.active_blocks()
    }

    /// `Ok` when a cut can happen now (a ring slot is free).
    pub fn cut_within(&self) -> Result<(), IndexError> {
        let frozen = self.frozen.lock().len();
        if frozen + 1 >= self.ring {
            return Err(IndexError::RingFull { frozen });
        }
        Ok(())
    }

    /// Freezes the active generation and opens a new one. Returns the frozen
    /// generation, which is also queued for the flusher. Refused when the
    /// ring is full; never waits for a flush, only for the holders of the
    /// old pointer (see the crate documentation).
    pub fn cut(&self, now_ns: u64) -> Result<Arc<Generation>, IndexError> {
        let _cut = self.cut_lock.lock();
        {
            let frozen = self.frozen.lock();
            if frozen.len() + 1 >= self.ring {
                return Err(IndexError::RingFull {
                    frozen: frozen.len(),
                });
            }
        }
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let old = self.active.swap(Arc::new(Generation::new(id, now_ns)));
        // The grace period: the swap turned every guard of the old pointer
        // into a strong reference, so the count reaches 1 exactly when the
        // last holder let go, and no new holder can appear. The acquire
        // fence orders their releases before the reads below; the maps
        // themselves are read under their own locks.
        let mut spins = 0u64;
        while Arc::strong_count(&old) > 1 {
            spins += 1;
            if spins > 64 {
                std::thread::yield_now();
            } else {
                std::hint::spin_loop();
            }
        }
        fence(Ordering::Acquire);
        self.grace_spins.fetch_add(spins, Ordering::Relaxed);
        old.cut_ns.store(now_ns, Ordering::Relaxed);
        self.frozen.lock().push_back(Arc::clone(&old));
        Ok(old)
    }

    /// The oldest frozen generation, which is the next consistency point.
    #[must_use]
    pub fn oldest_frozen(&self) -> Option<Arc<Generation>> {
        self.frozen.lock().front().cloned()
    }

    /// Every frozen generation, oldest first.
    #[must_use]
    pub fn frozen(&self) -> Vec<Arc<Generation>> {
        self.frozen.lock().iter().cloned().collect()
    }

    /// Frozen generations.
    #[must_use]
    pub fn frozen_len(&self) -> usize {
        self.frozen.lock().len()
    }

    /// Drops the oldest frozen generation, which must be `gen`: its
    /// consistency point committed.
    pub fn retire(&self, gen: GenId) -> Result<(), IndexError> {
        let mut frozen = self.frozen.lock();
        match frozen.front() {
            Some(front) if front.id() == gen => {
                frozen.pop_front();
                Ok(())
            }
            _ => Err(IndexError::NotOldest { gen }),
        }
    }

    /// Spins the cuts so far spent waiting for holders of the old pointer (a
    /// measurement, never a gate).
    #[must_use]
    pub fn grace_spins(&self) -> u64 {
        self.grace_spins.load(Ordering::Relaxed)
    }
}
