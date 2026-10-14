//! Where things are on the device.
//!
//! ```text
//! | A0 | A1 | I0 | I1 | segment 1 | segment 2 | ... | segment n | gap | B0 | B1 |
//! ```
//!
//! The superblock slots `A0`, `A1` are at the start and `B0`, `B1` at the
//! end (F7); each slot and each intent slot (`I0`, `I1`) occupies whole
//! stripes, so on a parity pool every one of them is a full-stripe write
//! (R26). Physical segments are numbered from 1 (0 is [`NULL_PSEG`]) and are
//! whole stripes; a stripe is addressed by `(pseg, idx)` and every address
//! handed to the backstore is a 4 KiB block number.

use core::fmt;

use otap_format::{
    FormatError, SbSlot, SummaryLayout, BLOCK_SIZE, INTENT_SLOT_SIZE, MAX_SEG_BYTES,
    MAX_STRIPE_BYTES, MIN_STRIPE_BYTES, NULL_PSEG, SUPERBLOCK_SLOT_SIZE,
};

use crate::traits::Geometry;

/// Why a geometry cannot be laid out.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum LayoutError {
    /// The stripe is not a 4 KiB multiple inside the format's bounds.
    BadStripe {
        /// Bytes.
        stripe_bytes: u32,
    },
    /// The segment is not a whole number of stripes, or above 256 MiB.
    BadSegment {
        /// Bytes.
        segment_bytes: u64,
    },
    /// The device cannot hold the fixed regions and two segments.
    TooSmall {
        /// Bytes.
        capacity_bytes: u64,
        /// Bytes needed.
        needed: u64,
    },
    /// The device holds more segments than the table can count (a `u32`):
    /// a larger segment is needed, never a shortened layout.
    TooManySegments {
        /// Segments the device would hold.
        segments: u64,
    },
    /// The format refused the stripe size.
    Format(FormatError),
}

impl fmt::Display for LayoutError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::BadStripe { stripe_bytes } => write!(f, "stripe of {stripe_bytes} bytes"),
            Self::BadSegment { segment_bytes } => write!(f, "segment of {segment_bytes} bytes"),
            Self::TooManySegments { segments } => write!(
                f,
                "{segments} segments are more than the segment table can count"
            ),
            Self::TooSmall {
                capacity_bytes,
                needed,
            } => write!(f, "{capacity_bytes} bytes is too small; {needed} needed"),
            Self::Format(e) => write!(f, "{e}"),
        }
    }
}

impl std::error::Error for LayoutError {}

impl From<FormatError> for LayoutError {
    fn from(e: FormatError) -> Self {
        Self::Format(e)
    }
}

/// The layout of one aggregate on one device.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DiskLayout {
    /// The full stripe, in bytes.
    pub stripe_bytes: u32,
    /// Stripes per segment.
    pub stripes_per_segment: u32,
    /// The segment, in bytes.
    pub segment_bytes: u64,
    /// Segments, numbered `1..=segments`.
    pub segments: u32,
    /// How a data stripe is split between summary and blocks.
    pub summary: SummaryLayout,
    /// Stripes a superblock slot occupies.
    pub slot_stripes: u32,
    /// Stripes an intent slot occupies.
    pub intent_stripes: u32,
    /// Byte offset of segment 1.
    pub seg_base: u64,
    /// Byte offset of `B0`.
    pub b_base: u64,
    /// The device, in bytes.
    pub capacity_bytes: u64,
}

impl DiskLayout {
    /// Lays `geom` out.
    pub fn new(geom: &Geometry) -> Result<Self, LayoutError> {
        let stripe = geom.stripe_bytes;
        if !(stripe as usize).is_multiple_of(BLOCK_SIZE)
            || (stripe as usize) < MIN_STRIPE_BYTES
            || stripe as usize > MAX_STRIPE_BYTES
        {
            return Err(LayoutError::BadStripe {
                stripe_bytes: stripe,
            });
        }
        let summary = SummaryLayout::for_stripe(stripe as usize)?;
        let segment = geom.segment_bytes;
        if segment == 0 || !segment.is_multiple_of(u64::from(stripe)) || segment > MAX_SEG_BYTES {
            return Err(LayoutError::BadSegment {
                segment_bytes: segment,
            });
        }
        let stripes_per_segment = (segment / u64::from(stripe)) as u32;
        let slot_stripes = (SUPERBLOCK_SLOT_SIZE as u32).div_ceil(stripe);
        let intent_stripes = (INTENT_SLOT_SIZE as u32).div_ceil(stripe);
        let front = u64::from(2 * slot_stripes + 2 * intent_stripes) * u64::from(stripe);
        let back = u64::from(2 * slot_stripes) * u64::from(stripe);
        let needed = front + back + 2 * segment;
        if geom.capacity_bytes < needed {
            return Err(LayoutError::TooSmall {
                capacity_bytes: geom.capacity_bytes,
                needed,
            });
        }
        let segments = (geom.capacity_bytes - front - back) / segment;
        if segments >= u64::from(u32::MAX) {
            return Err(LayoutError::TooManySegments { segments });
        }
        let segments = segments as u32;
        let b_base = geom.capacity_bytes - back;
        Ok(Self {
            stripe_bytes: stripe,
            stripes_per_segment,
            segment_bytes: segment,
            segments,
            summary,
            slot_stripes,
            intent_stripes,
            seg_base: front,
            b_base,
            capacity_bytes: geom.capacity_bytes,
        })
    }

    /// Blocks per stripe.
    #[must_use]
    pub const fn blocks_per_stripe(&self) -> u32 {
        self.stripe_bytes / BLOCK_SIZE as u32
    }

    /// Blocks per segment.
    #[must_use]
    pub const fn blocks_per_segment(&self) -> u32 {
        self.stripes_per_segment * self.blocks_per_stripe()
    }

    /// Data blocks a stripe carries at most.
    #[must_use]
    pub const fn data_blocks_per_stripe(&self) -> u32 {
        self.summary.nblocks as u32
    }

    /// The byte offset of stripe `idx` of segment `pseg`.
    #[must_use]
    pub fn stripe_offset(&self, pseg: u64, idx: u32) -> Option<u64> {
        if pseg == NULL_PSEG || pseg > u64::from(self.segments) || idx >= self.stripes_per_segment {
            return None;
        }
        Some(
            self.seg_base
                + (pseg - 1) * self.segment_bytes
                + u64::from(idx) * u64::from(self.stripe_bytes),
        )
    }

    /// The block number of stripe `idx` of segment `pseg`.
    #[must_use]
    pub fn stripe_pba(&self, pseg: u64, idx: u32) -> Option<u64> {
        self.stripe_offset(pseg, idx)
            .map(|off| off / BLOCK_SIZE as u64)
    }

    /// The block offset, within its segment, of data block `i` of stripe
    /// `idx`: what a map entry's `off` holds.
    #[must_use]
    pub const fn block_off(&self, idx: u32, i: u32) -> u32 {
        idx * self.blocks_per_stripe() + (self.summary.block_offset(i as usize) / BLOCK_SIZE) as u32
    }

    /// The block number of block offset `off` of segment `pseg`.
    #[must_use]
    pub fn block_pba(&self, pseg: u64, off: u32) -> Option<u64> {
        if pseg == NULL_PSEG || pseg > u64::from(self.segments) || off >= self.blocks_per_segment()
        {
            return None;
        }
        let bytes =
            self.seg_base + (pseg - 1) * self.segment_bytes + u64::from(off) * BLOCK_SIZE as u64;
        Some(bytes / BLOCK_SIZE as u64)
    }

    /// The physical segment holding block number `pba`, if any.
    #[must_use]
    pub fn pseg_of_pba(&self, pba: u64) -> Option<u64> {
        let bytes = pba.checked_mul(BLOCK_SIZE as u64)?;
        if bytes < self.seg_base || bytes >= self.b_base {
            return None;
        }
        let pseg = (bytes - self.seg_base) / self.segment_bytes + 1;
        (pseg <= u64::from(self.segments)).then_some(pseg)
    }

    /// The segment and stripe a block offset falls in.
    #[must_use]
    pub const fn stripe_of_off(&self, off: u32) -> u32 {
        off / self.blocks_per_stripe()
    }

    /// The block number of a superblock slot.
    #[must_use]
    pub fn sb_pba(&self, slot: SbSlot) -> u64 {
        let stripe = u64::from(self.stripe_bytes);
        let offset = match slot {
            SbSlot::A0 => 0,
            SbSlot::A1 => u64::from(self.slot_stripes) * stripe,
            SbSlot::B0 => self.b_base,
            SbSlot::B1 => self.b_base + u64::from(self.slot_stripes) * stripe,
        };
        offset / BLOCK_SIZE as u64
    }

    /// The block number of intent slot `which` (0 or 1).
    #[must_use]
    pub fn intent_pba(&self, which: u8) -> u64 {
        let stripe = u64::from(self.stripe_bytes);
        let offset = u64::from(2 * self.slot_stripes) * stripe
            + u64::from(which & 1) * u64::from(self.intent_stripes) * stripe;
        offset / BLOCK_SIZE as u64
    }

    /// The stripes a region of `pages` pages occupies when written as whole
    /// stripes (saturating at `u32::MAX`; unused by the engine, which packs
    /// regions through the metadata stream).
    #[must_use]
    pub const fn stripes_for_pages(&self, pages: u32) -> u32 {
        let bytes = pages as u64 * BLOCK_SIZE as u64;
        let stripes = bytes.div_ceil(self.stripe_bytes as u64);
        if stripes > u32::MAX as u64 {
            u32::MAX
        } else {
            stripes as u32
        }
    }

    /// Bytes every segment holds.
    #[must_use]
    pub const fn segment_data_bytes(&self) -> u64 {
        self.segment_bytes
    }
}

impl DiskLayout {
    /// Pages (4 KiB blocks) per stripe.
    #[must_use]
    pub const fn pages_per_stripe(&self) -> u32 {
        self.blocks_per_stripe()
    }

    /// The first block number of segment `pseg`.
    #[must_use]
    pub fn segment_first_pba(&self, pseg: u64) -> Option<u64> {
        self.stripe_pba(pseg, 0)
    }

    /// One past the last block number of segment `pseg`.
    #[must_use]
    pub fn segment_end_pba(&self, pseg: u64) -> Option<u64> {
        self.segment_first_pba(pseg)
            .map(|first| first + u64::from(self.blocks_per_segment()))
    }

    /// How a region of `pages` pages at `pba` falls over physical segments:
    /// `(pseg, bytes)` per segment touched, in order. Blocks outside every
    /// segment (the fixed regions) are not reported.
    #[must_use]
    pub fn segments_of_region(&self, pba: u64, pages: u32) -> Vec<(u64, u64)> {
        let mut out = Vec::new();
        let mut at = pba;
        let end = pba.saturating_add(u64::from(pages));
        while at < end {
            let Some(pseg) = self.pseg_of_pba(at) else {
                break;
            };
            let seg_end = match self.segment_end_pba(pseg) {
                Some(e) => e,
                None => break,
            };
            let take = seg_end.min(end) - at;
            out.push((pseg, take * BLOCK_SIZE as u64));
            at = seg_end;
        }
        out
    }

    /// The stripe index within its segment of block number `pba`, if the
    /// block is in a segment.
    #[must_use]
    pub fn stripe_of_pba(&self, pba: u64) -> Option<(u64, u32)> {
        let pseg = self.pseg_of_pba(pba)?;
        let first = self.segment_first_pba(pseg)?;
        Some((
            pseg,
            ((pba - first) / u64::from(self.blocks_per_stripe())) as u32,
        ))
    }
}
