//! The engine's half of replication (Step 6C): what a replication source
//! reads from a snapshot and what a replication sink writes into its
//! destination volume. The sessions that drive these calls, the stream and
//! the transports are in [`crate::repl`]; nothing here touches a socket.
//!
//! # The source's stable view
//!
//! A snapshot is immutable: its content never changes while it exists, and
//! its blocks are pinned by it. What can change under a long transfer is
//! where those blocks are (the cleaner relocates them and rewrites the
//! snapshot's tree, keeping every block's birth) and whether the snapshot
//! still exists (an operator may delete it). Every call here therefore
//! takes the snapshot's *current* tree, walks or reads what it was asked
//! for, and returns, all inside one call on the engine thread, where no
//! consistency point can move anything; nothing is held between calls, so
//! no deferred free has to be held either (unlike the verification job,
//! which must keep a whole committed CP readable across its slices). A
//! later call simply sees the relocated tree, with the same content.
//!
//! # The sink's writes
//!
//! [`Engine::apply_write`] and [`Engine::apply_punch`] place blocks and
//! holes into the arena exactly as the front door does (a sequence number,
//! the active generation, the CRCs), for a volume a sink marked as a
//! replication destination ([`Engine::set_replica`]): such a volume is
//! kept offline, so no client can read or write it while it is a mirror,
//! and it refuses to come online until the mark is cleared. A placed block
//! is durable once a committed CP carried its sequence number
//! ([`Engine::durable_seq`]), which is when the sink counts the chunk that
//! carried it as landed.

use otap_arena::ArenaError;
use otap_format::{
    MapEntry, MapPage, SnapKind, SnapOrigin, Uuid, BLOCK_SIZE, MAP_ENTRIES_PER_PAGE,
};
use otap_index::DirtyExtent;
use otap_map::{MapError, MapSet, PageReader, PinnedRoot};

use super::{
    BackstoreReader, Engine, EngineError, PeekSource, MAX_PUNCH_BLOCKS, MAX_REQUEST_BLOCKS,
};
use crate::traits::{Backstore, Clock, FrontDoor, IoError, NvfailIntent};

/// What a delta carries for one block of a snapshot, against its base.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum DeltaKind {
    /// A data block: the snapshot holds a block written at `birth` that the
    /// base does not hold.
    Data {
        /// The CP that wrote it.
        birth: u32,
    },
    /// A block that reads as zeros in the snapshot and not in the base: a
    /// hole punched after the base, or a block the volume lost to a shrink
    /// (and grew back unwritten) since the base.
    Hole,
}

/// One block of a delta.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct DeltaItem {
    /// The LBA.
    pub lba: u64,
    /// What the delta carries for it.
    pub kind: DeltaKind,
}

/// A slice of the walk of a delta ([`Engine::snapshot_delta_scan`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeltaScan {
    /// The blocks found, in LBA order.
    pub items: Vec<DeltaItem>,
    /// Where the next slice starts; `None` once the walk reached the end of
    /// the snapshot.
    pub next_lba: Option<u64>,
    /// The snapshot's size in blocks.
    pub size_blocks: u64,
    /// Map pages read by this slice.
    pub pages: u64,
}

/// How a block read from a snapshot ([`Engine::snapshot_read_blocks`])
/// was found.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct BlockInfo {
    /// Data on disk (CRC checked), a hole, or never written.
    pub source: PeekSource,
    /// The CP that wrote the block or punched the hole; 0 when unmapped.
    pub birth: u32,
}

/// Why a walk of two trees stopped, and in which tree.
enum ScanError {
    Target(MapError),
    Base(MapError),
}

/// `true` when two entries name the same write (relocation changes where a
/// block is, never its birth or its CRC).
fn same_write(a: MapEntry, b: MapEntry) -> bool {
    a.birth_cp == b.birth_cp && a.is_data() == b.is_data() && a.crc32c == b.crc32c
}

/// What the delta carries for one block: `t` in the snapshot, `b` in the
/// base (unmapped when there is no base).
fn classify(t: MapEntry, b: MapEntry, base_cp: u64, has_base: bool) -> Option<DeltaKind> {
    if t.is_unmapped() {
        // Unwritten in the snapshot: zeros there. The destination holds the
        // base's block, which must become zeros too, when it is data (a
        // hole in the base already reads as zeros). Only a shrink and a
        // grow between the base and the snapshot produce this.
        return (has_base && b.is_data()).then_some(DeltaKind::Hole);
    }
    if u64::from(t.birth_cp) <= base_cp && same_write(t, b) {
        return None;
    }
    if t.is_data() {
        Some(DeltaKind::Data { birth: t.birth_cp })
    } else if has_base {
        Some(DeltaKind::Hole)
    } else {
        // A full transfer lands on a fresh volume, where every block reads
        // as zeros already.
        None
    }
}

/// The walk behind [`Engine::snapshot_delta_scan`], over trees taken out of
/// the engine for the call.
#[allow(clippy::too_many_arguments)]
fn scan(
    map: &mut MapSet,
    reader: &mut dyn PageReader,
    target: &PinnedRoot,
    base: Option<&PinnedRoot>,
    base_cp: u64,
    from: u64,
    max_data: usize,
    max_pages: usize,
) -> Result<DeltaScan, ScanError> {
    let size = target.root.lba_count;
    let mut out = DeltaScan {
        items: Vec::new(),
        next_lba: None,
        size_blocks: size,
        pages: 0,
    };
    if from >= size {
        return Ok(out);
    }
    let mut data = 0usize;
    let mut page_no = MapPage::page_of(from);
    let npages = target.index.len() as u64;
    while page_no < npages {
        let first = MapPage::first_lba(page_no);
        let te = target.index[page_no as usize];
        let be = base
            .and_then(|b| b.index.get(page_no as usize).copied())
            .filter(|e| !e.is_none());
        let shared = be.is_some_and(|be| !te.is_none() && be.page_pba == te.page_pba);
        let nothing = match be {
            // A page both trees share holds the same entries in both.
            Some(_) => shared,
            // No base page: only entries born after the base can differ.
            None => te.is_none() || u64::from(te.max_birth) <= base_cp,
        };
        if nothing {
            page_no += 1;
            continue;
        }
        if out.pages >= max_pages as u64 && out.pages > 0 {
            out.next_lba = Some(first.max(from));
            return Ok(out);
        }
        let tp = if te.is_none() {
            None
        } else {
            Some(map.read_page(reader, &te).map_err(ScanError::Target)?)
        };
        let bp = match be {
            Some(be) => Some(map.read_page(reader, &be).map_err(ScanError::Base)?),
            None => None,
        };
        out.pages += u64::from(tp.is_some()) + u64::from(bp.is_some());
        let start = if from > first {
            (from - first) as usize
        } else {
            0
        };
        for slot in start..MAP_ENTRIES_PER_PAGE {
            let lba = first + slot as u64;
            if lba >= size {
                break;
            }
            let t = tp.as_ref().map_or(MapEntry::UNMAPPED, |p| p.entries[slot]);
            let b = bp.as_ref().map_or(MapEntry::UNMAPPED, |p| p.entries[slot]);
            let Some(kind) = classify(t, b, base_cp, base.is_some()) else {
                continue;
            };
            out.items.push(DeltaItem { lba, kind });
            if matches!(kind, DeltaKind::Data { .. }) {
                data += 1;
                if data >= max_data {
                    out.next_lba = (lba + 1 < size).then_some(lba + 1);
                    return Ok(out);
                }
            }
        }
        page_no += 1;
    }
    Ok(out)
}

impl<B: Backstore, F: FrontDoor, C: Clock> Engine<B, F, C> {
    /// The engine clock's monotonic time, for the replication sessions'
    /// timers.
    #[must_use]
    pub fn mono_ns(&self) -> u64 {
        self.clock.mono_ns()
    }

    /// The highest write sequence number a committed consistency point
    /// carried since the boot: every write (front door or replication)
    /// placed with a sequence number at or below it is durable.
    #[must_use]
    pub const fn durable_seq(&self) -> u64 {
        self.durable_seq
    }

    /// `true` when `vol` is marked as a replication destination.
    #[must_use]
    pub fn is_replica(&self, vol: u32) -> bool {
        self.replicas.get(vol)
    }

    /// `true` when `vol` is present, its creation is durable and no
    /// rollback, resize or deletion waits for a CP: a replication sink
    /// waits for this before it writes.
    #[must_use]
    pub fn volume_ready(&self, vol: u32) -> bool {
        self.vols
            .get(vol as usize)
            .is_some_and(|v| v.present && !v.creating && v.op.is_none())
    }

    /// Marks `vol` as a replication destination, or clears the mark. A
    /// destination must be offline (it is a mirror: no client may read a
    /// delta half landed or write under it); while marked it refuses to
    /// come online ([`EngineError::ReplicaVolume`]) and takes writes only
    /// from [`apply_write`](Self::apply_write) and
    /// [`apply_punch`](Self::apply_punch). The mark lives in memory: the
    /// replication sink sets it again when it attaches after a boot.
    pub fn set_replica(&mut self, vol: u32, on: bool) -> Result<(), EngineError> {
        let v = self
            .vols
            .get(vol as usize)
            .filter(|v| v.present)
            .ok_or(EngineError::NoSuchVolume { vol })?;
        if on {
            if !v.offline {
                return Err(EngineError::VolumeOnline { vol });
            }
            self.replicas.set(vol)?;
        } else {
            self.replicas.clear(vol)?;
        }
        Ok(())
    }

    /// The checks every replication write makes.
    fn check_apply(&self, vol: u32, lba: u64, blocks: u32) -> Result<(), EngineError> {
        if !self.serving || self.stopped {
            return Err(EngineError::NotServing);
        }
        let v = self
            .vols
            .get(vol as usize)
            .filter(|v| v.present && !v.creating)
            .ok_or(EngineError::NoSuchVolume { vol })?;
        if !self.replicas.get(vol) {
            return Err(EngineError::NotReplica { vol });
        }
        if v.nvfail.is_some() {
            return Err(EngineError::NvFailed { vol });
        }
        let in_range = blocks > 0
            && lba
                .checked_add(u64::from(blocks))
                .is_some_and(|end| end <= v.size_blocks);
        if !in_range {
            return Err(EngineError::Io(IoError::OutOfRange {
                pba: lba,
                bytes: blocks as usize * BLOCK_SIZE,
            }));
        }
        if self.pool_full {
            return Err(EngineError::NoSpace);
        }
        Ok(())
    }

    /// The arena has no room now: ask for the consistency point that drains
    /// it, when there is something to drain.
    fn want_drain(&mut self) {
        if self.index.unflushed_blocks() > 0 {
            self.cp_requested = true;
        }
    }

    /// Bookkeeping after a replication write was placed: the volume is
    /// write-active (its windows, section 7.6), and in precise mode the
    /// next intent slot carries it if the durable set does not.
    fn note_apply(&mut self, vol: u32, blocks: u32) {
        if let Some(v) = self.vols.get_mut(vol as usize) {
            v.windows = 2;
        }
        if self.backstore.profile().nvfail_intent == NvfailIntent::Precise
            && !self.durable_active.get(vol)
        {
            let _ = self.apply_intent.set(vol);
            self.intent_wanted = true;
        }
        self.stats.applied += u64::from(blocks);
    }

    /// Places `data` (whole 4 KiB blocks) at `lba` of the replication
    /// destination `vol`, as the front door places a write: returns its
    /// sequence number, durable once [`durable_seq`](Self::durable_seq)
    /// reaches it. `Ok(None)` means "not now": the arena is full or at its
    /// stall occupancy, or the pool's admission rule wants the unflushed
    /// writes drained first; a consistency point is asked for, and the
    /// caller tries again after a few ticks.
    ///
    /// # Errors
    /// [`EngineError::NotReplica`] for a volume no sink marked,
    /// [`EngineError::NoSpace`] on a full pool, the range and length
    /// refusals of the front door ([`EngineError::Io`]), and
    /// [`EngineError::NvFailed`], [`EngineError::NoSuchVolume`],
    /// [`EngineError::NotServing`].
    pub fn apply_write(
        &mut self,
        vol: u32,
        lba: u64,
        data: &[u8],
    ) -> Result<Option<u64>, EngineError> {
        if data.is_empty() || !data.len().is_multiple_of(BLOCK_SIZE) {
            return Err(EngineError::Io(IoError::BadLength { bytes: data.len() }));
        }
        let blocks = u32::try_from(data.len() / BLOCK_SIZE)
            .ok()
            .filter(|b| *b <= MAX_REQUEST_BLOCKS && *b <= self.arena.slots())
            .ok_or(EngineError::Io(IoError::BadLength { bytes: data.len() }))?;
        self.check_apply(vol, lba, blocks)?;
        if !self.admits(blocks) {
            if self.index.unflushed_blocks() > 0 {
                self.want_drain();
                return Ok(None);
            }
            return Err(EngineError::NoSpace);
        }
        if self.arena.occupancy() >= self.cfg.pacing.stall_at {
            self.want_drain();
            return Ok(None);
        }
        let ring = match self.arena.reserve(blocks) {
            Ok(ring) => ring,
            Err(ArenaError::Full { .. }) => {
                self.want_drain();
                return Ok(None);
            }
            Err(e) => return Err(e.into()),
        };
        self.arena.run_mut(ring, blocks)?.copy_from_slice(data);
        let crcs: Vec<u32> = data
            .as_chunks::<BLOCK_SIZE>()
            .0
            .iter()
            .map(|block| otap_format::checksum::crc32c(block))
            .collect();
        let seq = self.arena.next_seq();
        self.arena.fill(ring, vol, lba, seq, &crcs)?;
        let superseded = match self
            .index
            .insert(vol, lba, DirtyExtent::data(ring, blocks, seq))
        {
            Ok((_, superseded)) => superseded,
            Err(e) => {
                self.arena.discard(ring, blocks)?;
                return Err(e.into());
            }
        };
        self.arena.place(ring, blocks)?;
        for (old, old_blocks) in superseded {
            self.arena.commit(old, old_blocks)?;
        }
        self.note_apply(vol, blocks);
        Ok(Some(seq))
    }

    /// Punches a hole of `blocks` blocks at `lba` of the replication
    /// destination `vol`, as the front door serves a discard; the same
    /// answers as [`apply_write`](Self::apply_write).
    ///
    /// # Errors
    /// As [`apply_write`](Self::apply_write); a hole above
    /// [`MAX_PUNCH_BLOCKS`] is refused.
    pub fn apply_punch(
        &mut self,
        vol: u32,
        lba: u64,
        blocks: u32,
    ) -> Result<Option<u64>, EngineError> {
        if blocks == 0 || blocks > MAX_PUNCH_BLOCKS {
            return Err(EngineError::Io(IoError::BadLength {
                bytes: blocks as usize * BLOCK_SIZE,
            }));
        }
        self.check_apply(vol, lba, blocks)?;
        let ring = match self.arena.reserve(1) {
            Ok(ring) => ring,
            Err(ArenaError::Full { .. }) => {
                self.want_drain();
                return Ok(None);
            }
            Err(e) => return Err(e.into()),
        };
        let seq = self.arena.next_seq();
        self.arena.fill_hole(ring, vol, lba, blocks, seq)?;
        let (gen, superseded) =
            match self
                .index
                .insert(vol, lba, DirtyExtent::hole(ring, blocks, seq))
            {
                Ok(x) => x,
                Err(e) => {
                    self.arena.discard(ring, 1)?;
                    return Err(e.into());
                }
            };
        self.arena.place(ring, 1)?;
        for (old, old_blocks) in superseded {
            self.arena.commit(old, old_blocks)?;
        }
        self.gen_holes.entry(gen).or_default().push(ring);
        self.note_apply(vol, blocks);
        Ok(Some(seq))
    }

    /// `true` while a snapshot of identity `uuid` was asked for and is not
    /// yet taken (waiting for its fold, or riding the CP in flight).
    #[must_use]
    pub fn snapshot_pending(&self, uuid: Uuid) -> bool {
        self.snap_requests.iter().any(|r| r.uuid == uuid) || self.in_flight_snapshot(uuid)
    }

    /// [`create_snapshot`](Self::create_snapshot) with the identity and the
    /// catalogue name given: what a replication sink takes when a delta
    /// has landed, under the source snapshot's own identity.
    pub fn create_snapshot_as(
        &mut self,
        vol: u32,
        uuid: Uuid,
        kind: SnapKind,
        origin: SnapOrigin,
        name: Option<&str>,
    ) -> Result<(), EngineError> {
        self.create_snapshot_named(vol, uuid, kind, origin, name.map(str::to_string))
    }

    /// The position of snapshot `snap_id` in the table, if it may be read:
    /// present, its tree loaded at boot and not quarantined since.
    fn readable_position(&self, snap_id: u64) -> Result<usize, EngineError> {
        let i = self
            .snapshots
            .iter()
            .position(|s| s.record.snap_id == snap_id)
            .ok_or(EngineError::NoSuchSnapshot { snap_id })?;
        let s = &self.snapshots[i];
        if s.pinned.is_none() || s.damage.is_some() {
            return Err(EngineError::SnapshotDamaged { snap_id });
        }
        Ok(i)
    }

    /// Takes a snapshot's tree out of the table for the length of one call
    /// (so the map and the backstore can be borrowed beside it without a
    /// copy of its index); [`put_tree`](Self::put_tree) puts it back.
    fn take_tree(&mut self, i: usize) -> Result<PinnedRoot, EngineError> {
        self.snapshots[i]
            .pinned
            .take()
            .ok_or_else(|| EngineError::Internal("a readable snapshot has no tree".into()))
    }

    fn put_tree(&mut self, i: usize, tree: PinnedRoot) {
        self.snapshots[i].pinned = Some(tree);
    }

    /// One slice of the walk of the delta of snapshot `snap_id` against
    /// `base` (an older snapshot of the same volume, or none for a full
    /// transfer): from `from` upward, every block the delta must carry,
    /// in LBA order, until `max_data` data blocks were found, `max_pages`
    /// map pages were read, or the end of the snapshot. A data block is
    /// one the snapshot holds and the base does not; a hole is a block
    /// that reads as zeros in the snapshot and not in the base (a punch, or
    /// a block a shrink and a grow left unwritten, which a birth walk alone
    /// cannot see). Pages the two trees share are skipped unread. A full
    /// transfer carries data blocks only. The walk is deterministic, so a
    /// resumed transfer finds every chunk boundary where it was.
    ///
    /// # Errors
    /// [`EngineError::NoSuchSnapshot`], [`EngineError::SnapshotDamaged`]
    /// for a quarantined snapshot or base (and a map page found damaged
    /// here quarantines it), [`EngineError::SnapshotOrder`] for a base not
    /// older than the snapshot or of another volume.
    pub fn snapshot_delta_scan(
        &mut self,
        snap_id: u64,
        base: Option<u64>,
        from: u64,
        max_data: usize,
        max_pages: usize,
    ) -> Result<DeltaScan, EngineError> {
        let ti = self.readable_position(snap_id)?;
        let vol = self.snapshots[ti].record.vol;
        let birth = self.snapshots[ti].birth();
        let (bi, base_cp) = match base {
            None => (None, 0),
            Some(base_id) => {
                let bi = self.readable_position(base_id)?;
                let b = &self.snapshots[bi];
                if b.record.vol != vol || b.birth() >= birth {
                    return Err(EngineError::SnapshotOrder {
                        snap_id,
                        base: base_id,
                    });
                }
                (Some(bi), b.birth())
            }
        };
        let target = self.take_tree(ti)?;
        let base_tree = match bi.map(|i| self.take_tree(i)).transpose() {
            Ok(t) => t,
            Err(e) => {
                self.put_tree(ti, target);
                return Err(e);
            }
        };
        let walked = {
            let Engine { map, backstore, .. } = self;
            scan(
                map,
                &mut BackstoreReader(backstore),
                &target,
                base_tree.as_ref(),
                base_cp,
                from,
                max_data.max(1),
                max_pages.max(1),
            )
        };
        self.put_tree(ti, target);
        if let (Some(i), Some(tree)) = (bi, base_tree) {
            self.put_tree(i, tree);
        }
        match walked {
            Ok(scan) => Ok(scan),
            Err(ScanError::Target(MapError::Format(e))) => {
                self.quarantine_snapshot(snap_id, format!("a map page does not verify: {e}"));
                Err(EngineError::SnapshotDamaged { snap_id })
            }
            Err(ScanError::Base(MapError::Format(e))) => {
                let base_id = base.unwrap_or(snap_id);
                self.quarantine_snapshot(base_id, format!("a map page does not verify: {e}"));
                Err(EngineError::SnapshotDamaged { snap_id: base_id })
            }
            Err(ScanError::Target(e) | ScanError::Base(e)) => Err(e.into()),
        }
    }

    /// Reads the blocks at `lbas` of snapshot `snap_id` into `out` (4 KiB
    /// each, in order): the snapshot's content, CRC checked, zeros for a
    /// hole or an unwritten block. One call reads through the snapshot's
    /// tree as it stands, so relocations between calls do not matter.
    ///
    /// # Errors
    /// [`EngineError::NoSuchSnapshot`], [`EngineError::SnapshotDamaged`]
    /// (a damaged map page found here quarantines the snapshot),
    /// [`EngineError::Corrupt`] for a block that fails its CRC, and the
    /// range and length refusals.
    pub fn snapshot_read_blocks(
        &mut self,
        snap_id: u64,
        lbas: &[u64],
        out: &mut [u8],
    ) -> Result<Vec<BlockInfo>, EngineError> {
        if out.len() != lbas.len() * BLOCK_SIZE {
            return Err(EngineError::Io(IoError::BadLength { bytes: out.len() }));
        }
        let ti = self.readable_position(snap_id)?;
        let tree = self.take_tree(ti)?;
        let vol = tree.vol;
        let mut infos = Vec::with_capacity(lbas.len());
        let mut failed = None;
        for (k, &lba) in lbas.iter().enumerate() {
            if lba >= tree.root.lba_count {
                failed = Some(EngineError::Io(IoError::OutOfRange {
                    pba: lba,
                    bytes: BLOCK_SIZE,
                }));
                break;
            }
            let got = {
                let Engine { map, backstore, .. } = self;
                map.pinned_get(&mut BackstoreReader(backstore), &tree, lba)
            };
            let entry = match got {
                Ok(entry) => entry,
                Err(MapError::Format(e)) => {
                    failed = Some(EngineError::Map(MapError::Format(e)));
                    break;
                }
                Err(e) => {
                    failed = Some(e.into());
                    break;
                }
            };
            let dst = &mut out[k * BLOCK_SIZE..(k + 1) * BLOCK_SIZE];
            match self.read_entry(vol, lba, entry, dst) {
                Ok(source) => infos.push(BlockInfo {
                    source,
                    birth: entry.birth_cp,
                }),
                Err(e) => {
                    failed = Some(e);
                    break;
                }
            }
        }
        self.put_tree(ti, tree);
        match failed {
            None => Ok(infos),
            Some(EngineError::Map(MapError::Format(e))) => {
                self.quarantine_snapshot(snap_id, format!("a map page does not verify: {e}"));
                Err(EngineError::SnapshotDamaged { snap_id })
            }
            Some(e) => Err(e),
        }
    }
}
