//! The segment table: which physical segments are in use, by which
//! consistency point, and how much of each is live.
//!
//! Step 4 keeps `lseg == pseg` (no relocation; the cleaner is a later step).
//! Metadata segments and dead data segments are freed after a deferral
//! (E24, E31). The table is written whole at every
//! consistency point into the open metadata segment, and the superblock's
//! `seg_table` root names that image exactly (E14).

use core::fmt;

use otap_format::{
    FormatError, IntoBytes, SegEntry, SegKind, SegState, BLOCK_SIZE, NULL_PSEG, SEG_ENTRY_SIZE,
};

/// Why the table refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum SegmentError {
    /// Every segment is in use.
    NoSpace,
    /// A segment number outside the table.
    BadSegment {
        /// The segment.
        pseg: u64,
    },
    /// The image is not whole entries, or an entry fails its checks.
    BadImage {
        /// The entry index at fault, or `u64::MAX` for the length.
        index: u64,
    },
    /// The format refused an entry.
    Format(FormatError),
}

impl fmt::Display for SegmentError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NoSpace => f.write_str("no free segment"),
            Self::BadSegment { pseg } => write!(f, "segment {pseg} is outside the table"),
            Self::BadImage { index } => write!(f, "segment table image: entry {index}"),
            Self::Format(e) => write!(f, "{e}"),
        }
    }
}

impl std::error::Error for SegmentError {}

impl From<FormatError> for SegmentError {
    fn from(e: FormatError) -> Self {
        Self::Format(e)
    }
}

/// An open segment and the next stripe to write in it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct OpenSegment {
    /// The segment.
    pub pseg: u64,
    /// The next stripe index.
    pub next_idx: u32,
}

/// The table, entry 0 unused.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SegmentTable {
    entries: Vec<SegEntry>,
}

impl SegmentTable {
    /// An empty table for `segments` segments.
    #[must_use]
    pub fn empty(segments: u32) -> Self {
        Self {
            entries: vec![SegEntry::FREE; segments as usize + 1],
        }
    }

    /// Segments the table covers.
    #[must_use]
    pub fn segments(&self) -> u32 {
        (self.entries.len() - 1) as u32
    }

    /// The entry of `pseg`.
    pub fn entry(&self, pseg: u64) -> Result<&SegEntry, SegmentError> {
        if pseg == NULL_PSEG {
            return Err(SegmentError::BadSegment { pseg });
        }
        usize::try_from(pseg)
            .ok()
            .and_then(|i| self.entries.get(i))
            .ok_or(SegmentError::BadSegment { pseg })
    }

    fn entry_mut(&mut self, pseg: u64) -> Result<&mut SegEntry, SegmentError> {
        if pseg == NULL_PSEG {
            return Err(SegmentError::BadSegment { pseg });
        }
        usize::try_from(pseg)
            .ok()
            .and_then(|i| self.entries.get_mut(i))
            .ok_or(SegmentError::BadSegment { pseg })
    }

    /// Every entry, `pseg` first, in segment order; free entries included.
    pub fn iter(&self) -> impl Iterator<Item = (u64, &SegEntry)> + '_ {
        self.entries
            .iter()
            .enumerate()
            .skip(1)
            .map(|(i, e)| (i as u64, e))
    }

    /// Opens the lowest free segment for `kind`, allocated by `birth_cp`.
    pub fn allocate(
        &mut self,
        kind: SegKind,
        birth_cp: u32,
        stripe_bytes: u32,
    ) -> Result<u64, SegmentError> {
        self.allocate_keeping(kind, birth_cp, stripe_bytes, 0)
    }

    /// Like [`allocate`](Self::allocate), but refuses unless more than
    /// `reserve` segments are free: data allocations keep room for the
    /// metadata a boot or a shutdown commit needs.
    pub fn allocate_keeping(
        &mut self,
        kind: SegKind,
        birth_cp: u32,
        stripe_bytes: u32,
        reserve: u32,
    ) -> Result<u64, SegmentError> {
        if self.free_count() <= reserve {
            return Err(SegmentError::NoSpace);
        }
        let pseg = self
            .iter()
            .find(|(_, e)| e.is_free())
            .map(|(p, _)| p)
            .ok_or(SegmentError::NoSpace)?;
        let lap = self.entry(pseg)?.lap.wrapping_add(1).max(1);
        let entry = SegEntry::open(pseg, stripe_bytes, birth_cp, lap, kind)?;
        *self.entry_mut(pseg)? = entry;
        Ok(pseg)
    }

    /// Free segments.
    #[must_use]
    pub fn free_count(&self) -> u32 {
        self.iter().filter(|(_, e)| e.is_free()).count() as u32
    }

    /// Frees a segment in use (a metadata segment no reachable consistency
    /// point names). Its entry becomes all zero, as a never-used one.
    pub fn free(&mut self, pseg: u64) -> Result<(), SegmentError> {
        let entry = self.entry_mut(pseg)?;
        if entry.is_free() {
            return Err(SegmentError::BadSegment { pseg });
        }
        *entry = SegEntry::FREE;
        Ok(())
    }

    /// Marks an open segment full and immutable.
    pub fn seal(&mut self, pseg: u64) -> Result<(), SegmentError> {
        let entry = self.entry_mut(pseg)?;
        if entry.seg_state() != Some(SegState::Open) {
            return Err(SegmentError::BadSegment { pseg });
        }
        entry.state = SegState::Sealed.as_u8();
        entry.seal();
        Ok(())
    }

    /// Records the live bytes of a segment (for the cleaner, and the boot's
    /// view of what the segment holds).
    pub fn set_live_bytes(&mut self, pseg: u64, live_bytes: u32) -> Result<(), SegmentError> {
        let entry = self.entry_mut(pseg)?;
        if entry.is_free() {
            return Err(SegmentError::BadSegment { pseg });
        }
        entry.live_bytes = live_bytes;
        entry.seal();
        Ok(())
    }

    /// The open segments of `kind`, lowest first.
    #[must_use]
    pub fn open_segments(&self, kind: SegKind) -> Vec<u64> {
        self.iter()
            .filter(|(_, e)| e.seg_state() == Some(SegState::Open) && e.seg_kind() == Some(kind))
            .map(|(p, _)| p)
            .collect()
    }

    /// Segments in use (not free), lowest first.
    #[must_use]
    pub fn used_segments(&self) -> Vec<u64> {
        self.iter()
            .filter(|(_, e)| !e.is_free())
            .map(|(p, _)| p)
            .collect()
    }

    /// The on-disk image: every entry in order, padded to whole 4 KiB pages.
    #[must_use]
    pub fn image(&self) -> Vec<u8> {
        let mut bytes = self.entries.as_bytes().to_vec();
        let padded = bytes.len().div_ceil(BLOCK_SIZE) * BLOCK_SIZE;
        bytes.resize(padded, 0);
        bytes
    }

    /// Parses an image for `segments` segments: all-zero entries are free,
    /// every other entry must verify.
    pub fn from_image(bytes: &[u8], segments: u32) -> Result<Self, SegmentError> {
        let count = segments as usize + 1;
        if bytes.len() < count * SEG_ENTRY_SIZE {
            return Err(SegmentError::BadImage { index: u64::MAX });
        }
        let mut entries = Vec::with_capacity(count);
        for i in 0..count {
            let chunk = &bytes[i * SEG_ENTRY_SIZE..(i + 1) * SEG_ENTRY_SIZE];
            if chunk.iter().all(|b| *b == 0) {
                entries.push(SegEntry::FREE);
                continue;
            }
            let entry = <SegEntry as otap_format::FromBytes>::read_from_bytes(chunk)
                .map_err(|_| SegmentError::BadImage { index: i as u64 })?;
            entry
                .verify()
                .map_err(|_| SegmentError::BadImage { index: i as u64 })?;
            if i == 0 || entry.phys != i as u64 {
                return Err(SegmentError::BadImage { index: i as u64 });
            }
            entries.push(entry);
        }
        for extra in bytes[count * SEG_ENTRY_SIZE..].iter() {
            if *extra != 0 {
                return Err(SegmentError::BadImage { index: u64::MAX });
            }
        }
        Ok(Self { entries })
    }
}

impl SegmentTable {
    /// Opens `count` consecutive free segments for `kind` (a region longer
    /// than one segment spans physically adjacent segments, which the
    /// layout keeps contiguous), keeping `reserve` segments free; returns
    /// the first. The lowest run that fits is taken.
    pub fn allocate_run(
        &mut self,
        kind: SegKind,
        birth_cp: u32,
        stripe_bytes: u32,
        count: u32,
        reserve: u32,
    ) -> Result<u64, SegmentError> {
        if count == 0 {
            return Err(SegmentError::NoSpace);
        }
        if count == 1 {
            return self.allocate_keeping(kind, birth_cp, stripe_bytes, reserve);
        }
        if self.free_count() < count.saturating_add(reserve) {
            return Err(SegmentError::NoSpace);
        }
        let segments = self.segments();
        let mut run_start = 0u64;
        let mut run_len = 0u32;
        let mut found = None;
        for (pseg, entry) in self.iter() {
            if entry.is_free() {
                if run_len == 0 {
                    run_start = pseg;
                }
                run_len += 1;
                if run_len == count {
                    found = Some(run_start);
                    break;
                }
            } else {
                run_len = 0;
            }
        }
        let _ = segments;
        let first = found.ok_or(SegmentError::NoSpace)?;
        for pseg in first..first + u64::from(count) {
            let lap = self.entry(pseg)?.lap.wrapping_add(1).max(1);
            let entry = SegEntry::open(pseg, stripe_bytes, birth_cp, lap, kind)?;
            *self.entry_mut(pseg)? = entry;
        }
        Ok(first)
    }

    /// `true` when `pseg` is free.
    #[must_use]
    pub fn is_free(&self, pseg: u64) -> bool {
        self.entry(pseg).is_ok_and(SegEntry::is_free)
    }
}
