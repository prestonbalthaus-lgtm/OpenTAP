//! The checker model: every write the client issued, what the engine
//! answered, and the rules a recovered image must satisfy.
//!
//! Every block a write carries is self-describing: its first bytes name the
//! write (the harness's operation number), the volume, the LBA and the
//! block's position in the write, and the rest is a fill derived from them.
//! A recovered block therefore says which write it came from, or that it was
//! never written (all zero), or that it is garbage (which the checker treats
//! as a failure).
//!
//! # The rules (section 7.7, invariants 1 and 6)
//!
//! The engine places writes in the order the client issued them (reads may
//! pass a deferred write; writes never pass each other), and a consistency
//! point carries whole generations, which are contiguous ranges of that
//! order. So whatever a crash leaves behind is the state after applying the
//! issued writes up to some operation number `j`, acknowledged or not:
//!
//! - every block holds the newest issued write with `op <= j` that covered
//!   it, or zeros if none did ([`Model::check_prefix`] finds `j` as the
//!   highest operation number any block holds, then checks every block);
//! - an *acknowledged* write with `op > j` is lost; after a daemon crash or
//!   a clean restart there is none; after a power cut its volume must be
//!   held by nvfail;
//! - an unacknowledged write with `op > j` simply never happened (the client
//!   never heard of it), and one with `op <= j` was placed before the crash
//!   and lives on like an acknowledged one: [`Model::settle`] records both
//!   outcomes once a recovery was checked;
//! - nvfail after a dirty boot holds every volume that lost an acknowledged
//!   write, holds no volume after a clean shutdown, and in precise mode
//!   holds no volume idle for two consistency points (a write issued but
//!   never acknowledged still counts as activity: its intent may have
//!   landed).

use std::collections::{BTreeMap, HashMap};

use otap_format::{VolBitSet, BLOCK_SIZE};

/// A write the client issued.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WriteRecord {
    /// The operation number (1 and up); embedded in the data.
    pub op: u64,
    /// The volume.
    pub vol: u32,
    /// The first LBA.
    pub lba: u64,
    /// Blocks.
    pub blocks: u32,
    /// The engine's sequence number, once acknowledged (0 before).
    pub seq: u64,
    /// The engine acknowledged it.
    pub acked: bool,
    /// A recovery found it applied (placed before a crash, acknowledged or
    /// not); from then on it is part of the image like an acknowledged one.
    pub landed: bool,
    /// The index (among generation commits the harness saw) of the
    /// consistency point that carried the write, once one did.
    pub committed_at: Option<u64>,
}

/// What a recovered block decoded to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Decoded {
    /// Never written.
    Zero,
    /// Block `k` of write `op`.
    Write {
        /// The operation.
        op: u64,
        /// The block's position in it.
        k: u32,
    },
    /// Neither.
    Garbage,
}

/// The model.
#[derive(Debug, Clone, Default)]
pub struct Model {
    writes: Vec<WriteRecord>,
    by_block: HashMap<(u32, u64), Vec<usize>>,
    sizes: BTreeMap<u32, u64>,
}

/// The data of block `k` of write `op` to `lba` of `vol`.
#[must_use]
pub fn block_data(op: u64, vol: u32, lba: u64, k: u32) -> Vec<u8> {
    let mut block = vec![0u8; BLOCK_SIZE];
    block[..8].copy_from_slice(&op.to_le_bytes());
    block[8..12].copy_from_slice(&vol.to_le_bytes());
    block[12..20].copy_from_slice(&lba.to_le_bytes());
    block[20..24].copy_from_slice(&k.to_le_bytes());
    let mut x = op
        .wrapping_mul(0x9E37_79B9_7F4A_7C15)
        .wrapping_add(lba)
        .wrapping_add(u64::from(vol) << 40)
        .wrapping_add(u64::from(k) << 20)
        | 1;
    for byte in &mut block[24..] {
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        *byte = (x & 0xff) as u8;
    }
    block
}

/// Decodes a recovered block.
#[must_use]
pub fn decode(block: &[u8]) -> Decoded {
    if block.len() != BLOCK_SIZE {
        return Decoded::Garbage;
    }
    if block.iter().all(|b| *b == 0) {
        return Decoded::Zero;
    }
    let op = u64::from_le_bytes(block[..8].try_into().expect("8 bytes"));
    let vol = u32::from_le_bytes(block[8..12].try_into().expect("4 bytes"));
    let lba = u64::from_le_bytes(block[12..20].try_into().expect("8 bytes"));
    let k = u32::from_le_bytes(block[20..24].try_into().expect("4 bytes"));
    if block_data(op, vol, lba, k) == block {
        Decoded::Write { op, k }
    } else {
        Decoded::Garbage
    }
}

impl Model {
    /// A model of the given volumes.
    #[must_use]
    pub fn new(volumes: &[(u32, u64)]) -> Self {
        Self {
            writes: Vec::new(),
            by_block: HashMap::new(),
            sizes: volumes.iter().copied().collect(),
        }
    }

    /// The volumes and their sizes.
    pub fn volumes(&self) -> impl Iterator<Item = (u32, u64)> + '_ {
        self.sizes.iter().map(|(v, s)| (*v, *s))
    }

    /// Adds a volume the engine created (durably).
    pub fn add_volume(&mut self, vol: u32, size: u64) {
        self.sizes.insert(vol, size);
    }

    /// The data of a write.
    #[must_use]
    pub fn data_of(op: u64, vol: u32, lba: u64, blocks: u32) -> Vec<u8> {
        let mut out = Vec::with_capacity(blocks as usize * BLOCK_SIZE);
        for k in 0..blocks {
            out.extend_from_slice(&block_data(op, vol, lba, k));
        }
        out
    }

    /// Records a write the client issued (not yet answered). Operation
    /// numbers must increase.
    pub fn issued(&mut self, op: u64, vol: u32, lba: u64, blocks: u32) {
        debug_assert!(self.writes.last().is_none_or(|w| w.op < op));
        let index = self.writes.len();
        for k in 0..blocks {
            self.by_block
                .entry((vol, lba + u64::from(k)))
                .or_default()
                .push(index);
        }
        self.writes.push(WriteRecord {
            op,
            vol,
            lba,
            blocks,
            seq: 0,
            acked: false,
            landed: false,
            committed_at: None,
        });
    }

    /// Records that the engine acknowledged write `op` with `seq`.
    pub fn acked(&mut self, op: u64, seq: u64) {
        if let Some(w) = self.writes.iter_mut().find(|w| w.op == op) {
            w.acked = true;
            w.seq = seq;
        }
    }

    /// Records that the engine refused write `op`: it never happened.
    pub fn refused(&mut self, op: u64) {
        self.writes.retain(|w| w.op != op);
        self.reindex();
    }

    /// Records an acknowledged write in one step (the synchronous harness
    /// path).
    pub fn acked_now(&mut self, op: u64, vol: u32, lba: u64, blocks: u32, seq: u64) {
        self.issued(op, vol, lba, blocks);
        self.acked(op, seq);
    }

    /// Every write issued and not refused, in operation order.
    #[must_use]
    pub fn writes(&self) -> &[WriteRecord] {
        &self.writes
    }

    /// Writes issued and neither acknowledged nor found landed.
    #[must_use]
    pub fn unresolved(&self) -> usize {
        self.writes.iter().filter(|w| !w.acked && !w.landed).count()
    }

    /// Settles a checked recovery at prefix `j`: every write above `j` is
    /// gone (the acknowledged ones were lost, the others never happened),
    /// every write at or below it is part of the image. Returns how many
    /// acknowledged writes were lost.
    pub fn settle(&mut self, j: u64) -> usize {
        let lost = self.writes.iter().filter(|w| w.acked && w.op > j).count();
        self.writes.retain(|w| w.op <= j);
        for w in &mut self.writes {
            w.landed = true;
        }
        self.reindex();
        lost
    }

    fn reindex(&mut self) {
        self.by_block.clear();
        for (index, rec) in self.writes.iter().enumerate() {
            for b in 0..rec.blocks {
                self.by_block
                    .entry((rec.vol, rec.lba + u64::from(b)))
                    .or_default()
                    .push(index);
            }
        }
    }

    /// The newest write with `op <= limit` covering the block, and the
    /// block's position in it.
    #[must_use]
    pub fn expected(&self, vol: u32, lba: u64, limit: u64) -> Option<(u64, u32)> {
        let list = self.by_block.get(&(vol, lba))?;
        list.iter()
            .rev()
            .map(|i| &self.writes[*i])
            .find(|w| w.op <= limit)
            .map(|w| (w.op, (lba - w.lba) as u32))
    }

    /// The newest acknowledged (or landed) write's operation number.
    #[must_use]
    pub fn last_op(&self) -> u64 {
        self.writes
            .iter()
            .rev()
            .find(|w| w.acked || w.landed)
            .map_or(0, |w| w.op)
    }

    /// Checks that `image` (every block of every volume, as `read` returns
    /// them) is the state after the issued writes up to some operation
    /// number `j`, and returns `j` (0 for an empty image).
    pub fn check_prefix<R>(&self, mut read: R) -> Result<u64, String>
    where
        R: FnMut(u32, u64) -> Result<Vec<u8>, String>,
    {
        let mut decoded: Vec<(u32, u64, Decoded)> = Vec::new();
        let mut j = 0u64;
        for (vol, size) in self.volumes() {
            for lba in 0..size {
                let block = read(vol, lba)?;
                let d = decode(&block);
                match d {
                    Decoded::Garbage => {
                        return Err(format!("volume {vol} LBA {lba}: garbage block"));
                    }
                    Decoded::Write { op, .. } => {
                        if !self.writes.iter().any(|w| {
                            w.op == op
                                && w.vol == vol
                                && lba >= w.lba
                                && lba < w.lba + u64::from(w.blocks)
                        }) {
                            return Err(format!(
                                "volume {vol} LBA {lba}: holds write {op}, which never covered it"
                            ));
                        }
                        j = j.max(op);
                    }
                    Decoded::Zero => {}
                }
                decoded.push((vol, lba, d));
            }
        }
        for (vol, lba, d) in decoded {
            let expected = self.expected(vol, lba, j);
            let ok = match (d, expected) {
                (Decoded::Zero, None) => true,
                (Decoded::Write { op, k: pos }, Some((e_op, e_pos))) => op == e_op && pos == e_pos,
                _ => false,
            };
            if !ok {
                return Err(format!(
                    "volume {vol} LBA {lba}: holds {d:?}, expected {expected:?} under prefix bound {j}"
                ));
            }
        }
        Ok(j)
    }

    /// Volumes with an acknowledged write above `j`: the writes a crash at
    /// prefix `j` lost.
    #[must_use]
    pub fn lost_volumes(&self, j: u64) -> VolBitSet {
        let mut set = VolBitSet::EMPTY;
        for w in &self.writes {
            if w.acked && w.op > j {
                let _ = set.set(w.vol);
            }
        }
        set
    }

    /// Records that generation commit number `cp_index` carried every
    /// acknowledged write with a sequence number up to `max_seq`, and every
    /// write issued before one of those.
    pub fn mark_committed(&mut self, max_seq: u64, cp_index: u64) {
        let mut last_op = 0u64;
        for w in &self.writes {
            if w.acked && w.seq != 0 && w.seq <= max_seq {
                last_op = last_op.max(w.op);
            }
        }
        for w in &mut self.writes {
            if w.committed_at.is_none()
                && ((w.acked && w.seq != 0 && w.seq <= max_seq) || (!w.acked && w.op < last_op))
            {
                w.committed_at = Some(cp_index);
            }
        }
    }

    /// Volumes precise mode may still hold after `cps_now` generation
    /// commits: those with a write (issued, acknowledged or not) not yet
    /// carried by a consistency point, and those whose last carried write
    /// is at most one commit old (the bit clears at the second consistency
    /// point without a write, section 7.6).
    #[must_use]
    pub fn recently_active(&self, cps_now: u64) -> VolBitSet {
        let mut set = VolBitSet::EMPTY;
        for w in &self.writes {
            let recent = match w.committed_at {
                None => true,
                Some(c) => cps_now.saturating_sub(c) <= 1,
            };
            if recent {
                let _ = set.set(w.vol);
            }
        }
        set
    }

    /// Every present volume.
    #[must_use]
    pub fn all_volumes(&self) -> VolBitSet {
        let mut set = VolBitSet::EMPTY;
        for (vol, _) in self.volumes() {
            let _ = set.set(vol);
        }
        set
    }
}
