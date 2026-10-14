//! The in-memory block map: where every mapped block of every volume is on
//! disk, with its birth and CRC.
//!
//! Step 3 persists the whole map at every consistency point as CP log pages
//! (one [`CpLogRec`] per mapped block, in volume and LBA order) and reads it
//! back at boot; the superblock's `cp_log_head` root names the image exactly.
//! The radix map with incremental checkpoints (`otap-map`, week 4) replaces
//! the dump without a format change (E14).

use std::collections::{BTreeMap, HashMap};

use otap_format::{
    CpLogPage, CpLogRec, FormatError, IntoBytes, MapEntry, Uuid, BLOCK_SIZE, CP_LOG_PAGE_SIZE,
};

/// The map.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct BlockMap {
    entries: BTreeMap<(u32, u64), MapEntry>,
    live_per_seg: HashMap<u64, u64>,
}

impl BlockMap {
    /// An empty map.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// The entry of `lba` of `vol`, if mapped.
    #[must_use]
    pub fn get(&self, vol: u32, lba: u64) -> Option<MapEntry> {
        self.entries.get(&(vol, lba)).copied()
    }

    /// Maps `lba` of `vol`; returns the entry replaced.
    pub fn set(&mut self, vol: u32, lba: u64, entry: MapEntry) -> Option<MapEntry> {
        let old = self.entries.insert((vol, lba), entry);
        if let Some(old) = old.filter(MapEntry::is_data) {
            if let Some(n) = self.live_per_seg.get_mut(&old.lseg()) {
                *n = n.saturating_sub(1);
            }
        }
        if entry.is_data() {
            *self.live_per_seg.entry(entry.lseg()).or_insert(0) += 1;
        }
        old
    }

    /// Mapped blocks.
    #[must_use]
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// `true` with nothing mapped.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Live (referenced) data blocks in a segment.
    #[must_use]
    pub fn live_blocks(&self, lseg: u64) -> u64 {
        self.live_per_seg.get(&lseg).copied().unwrap_or(0)
    }

    /// One past the highest block offset a live entry uses in `lseg`, or 0.
    #[must_use]
    pub fn used_end(&self, lseg: u64) -> u32 {
        self.entries
            .values()
            .filter(|e| e.is_data() && e.lseg() == lseg)
            .map(|e| e.off() + 1)
            .max()
            .unwrap_or(0)
    }

    /// Every entry in `(vol, lba)` order.
    pub fn iter(&self) -> impl Iterator<Item = (u32, u64, MapEntry)> + '_ {
        self.entries.iter().map(|((v, l), e)| (*v, *l, *e))
    }

    /// Entries born at `cp_seq`, as `(lseg, off)` pairs in that order.
    #[must_use]
    pub fn blocks_born_at(&self, cp_seq: u32) -> Vec<(u64, u32)> {
        let mut out: Vec<(u64, u32)> = self
            .entries
            .values()
            .filter(|e| e.is_data() && e.birth_cp == cp_seq)
            .map(|e| (e.lseg(), e.off()))
            .collect();
        out.sort_unstable();
        out.dedup();
        out
    }

    /// The whole map as sealed CP log pages for `cp_seq`: a multiple of
    /// 4 KiB, empty when nothing is mapped.
    pub fn dump(&self, agg_id: Uuid, cp_seq: u64, cp_nonce: u64) -> Result<Vec<u8>, FormatError> {
        let mut out = Vec::new();
        let mut page_seq = 0u64;
        let mut page = CpLogPage::new(agg_id, cp_seq, cp_nonce, page_seq);
        for (vol, lba, entry) in self.iter() {
            let rec = CpLogRec::from_entry(vol, lba, entry)?;
            if page.push(rec).is_none() {
                page.seal();
                out.extend_from_slice(page.as_bytes());
                page_seq += 1;
                page = CpLogPage::new(agg_id, cp_seq, cp_nonce, page_seq);
                page.push(rec).expect("an empty page takes a record");
            }
        }
        if page.hdr.count > 0 {
            page.seal();
            out.extend_from_slice(page.as_bytes());
        }
        debug_assert!(out.len().is_multiple_of(BLOCK_SIZE));
        Ok(out)
    }

    /// Rebuilds the map from a dump that must belong to `(agg_id, cp_seq,
    /// cp_nonce)` with pages numbered from 0.
    pub fn load(
        bytes: &[u8],
        agg_id: &Uuid,
        cp_seq: u64,
        cp_nonce: u64,
    ) -> Result<Self, FormatError> {
        if !bytes.len().is_multiple_of(CP_LOG_PAGE_SIZE) {
            return Err(FormatError::BadLength {
                expected: bytes.len().div_ceil(CP_LOG_PAGE_SIZE) * CP_LOG_PAGE_SIZE,
                found: bytes.len(),
            });
        }
        let mut map = Self::new();
        let (pages, rest) = bytes.as_chunks::<CP_LOG_PAGE_SIZE>();
        debug_assert!(rest.is_empty());
        for (i, chunk) in pages.iter().enumerate() {
            let page = CpLogPage::read(&chunk[..])?;
            if page.hdr.agg_id != *agg_id {
                return Err(FormatError::IdentityMismatch { field: "agg_id" });
            }
            if page.hdr.cp_seq != cp_seq {
                return Err(FormatError::IdentityMismatch { field: "cp_seq" });
            }
            if page.hdr.cp_nonce != cp_nonce {
                return Err(FormatError::IdentityMismatch { field: "cp_nonce" });
            }
            if page.hdr.page_seq != i as u64 {
                return Err(FormatError::IdentityMismatch { field: "page_seq" });
            }
            for rec in page.records() {
                rec.verify()?;
                let entry = rec.map_entry();
                entry.validate()?;
                map.set(rec.vol, rec.lba, entry);
            }
        }
        Ok(map)
    }
}
