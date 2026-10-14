//! The metadata stream of a consistency point: every region the CP writes
//! (deltas, fold pages, index regions, the tables, the directory) placed
//! back to back into whole stripes of the open metadata segment, and into a
//! run of adjacent free segments when a region does not fit what is left
//! (E22).
//!
//! The stream works on a *copy* of the segment table, so a CP that fails
//! leaves the committed table and append point as they were; the engine
//! swaps the copy in when the CP commits. A region never crosses from the
//! open segment into a segment that is not physically next to it: when the
//! open segment cannot hold the whole region, it is sealed and the region
//! starts a new run.

use core::fmt;

use otap_format::{RootRef, SegKind, BLOCK_SIZE};
use otap_map::{MapError, RegionWriter};

use crate::layout::DiskLayout;
use crate::segments::{OpenSegment, SegmentError, SegmentTable};

/// The stripe images a stream finishes with: `(pba, image)` in write order.
pub type MetaStripes = Vec<(u64, Vec<u8>)>;

/// Why a region could not be placed.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum MetaError {
    /// No run of free segments long enough.
    NoSpace,
    /// A region that is not whole pages, or empty.
    BadRegion {
        /// Bytes offered.
        bytes: usize,
    },
    /// A region longer than the whole pool.
    TooLarge {
        /// Pages wanted.
        pages: u64,
    },
    /// The segment table refused.
    Segment(SegmentError),
}

impl fmt::Display for MetaError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NoSpace => f.write_str("no free metadata segment"),
            Self::BadRegion { bytes } => write!(f, "a region of {bytes} bytes"),
            Self::TooLarge { pages } => write!(f, "a region of {pages} pages cannot fit"),
            Self::Segment(e) => write!(f, "{e}"),
        }
    }
}

impl std::error::Error for MetaError {}

impl From<SegmentError> for MetaError {
    fn from(e: SegmentError) -> Self {
        Self::Segment(e)
    }
}

impl From<MetaError> for MapError {
    fn from(e: MetaError) -> Self {
        match e {
            MetaError::NoSpace => MapError::NoSpace,
            other => MapError::Io {
                pba: 0,
                bytes: 0,
                reason: other.to_string(),
            },
        }
    }
}

/// A run of pages the stream has laid out contiguously: it starts at a
/// stripe boundary and is written as whole stripes.
#[derive(Debug, Clone)]
struct Chunk {
    first_pba: u64,
    bytes: Vec<u8>,
    /// One past the last block the run may grow to.
    end_pba: u64,
    /// The segments the run spans (adjacent).
    segments: Vec<u64>,
}

/// The stream.
#[derive(Debug)]
pub struct MetaStream<'a> {
    layout: &'a DiskLayout,
    segments: SegmentTable,
    cp_seq: u32,
    /// Segments every run leaves free (0 for the boot and shutdown commits,
    /// which may take the reserve).
    reserve: u32,
    chunks: Vec<Chunk>,
    regions: Vec<RootRef>,
    /// Where the next region goes: the current chunk's append point.
    cursor: Option<u64>,
}

impl<'a> MetaStream<'a> {
    /// A stream appending after `open` (the committed metadata append
    /// point) into a copy of `segments`, for CP `cp_seq`.
    #[must_use]
    pub fn new(
        layout: &'a DiskLayout,
        segments: &SegmentTable,
        open: Option<OpenSegment>,
        cp_seq: u32,
        reserve: u32,
    ) -> Self {
        let mut stream = Self {
            layout,
            segments: segments.clone(),
            cp_seq: cp_seq.max(1),
            reserve,
            chunks: Vec::new(),
            regions: Vec::new(),
            cursor: None,
        };
        if let Some(open) = open {
            if open.next_idx < layout.stripes_per_segment {
                if let (Some(first), Some(end)) = (
                    layout.stripe_pba(open.pseg, open.next_idx),
                    layout.segment_end_pba(open.pseg),
                ) {
                    stream.chunks.push(Chunk {
                        first_pba: first,
                        bytes: Vec::new(),
                        end_pba: end,
                        segments: vec![open.pseg],
                    });
                    stream.cursor = Some(first);
                }
            }
        }
        stream
    }

    /// Pages the current run can still take.
    fn room(&self) -> u64 {
        match (self.chunks.last(), self.cursor) {
            (Some(chunk), Some(cursor)) => chunk.end_pba.saturating_sub(cursor),
            _ => 0,
        }
    }

    /// Opens a new run of `count` adjacent segments and makes it current.
    fn open_run(&mut self, count: u32) -> Result<(), MetaError> {
        let first = match self.segments.allocate_run(
            SegKind::Metadata,
            self.cp_seq,
            self.layout.stripe_bytes,
            count,
            self.reserve,
        ) {
            Ok(first) => first,
            Err(SegmentError::NoSpace) => return Err(MetaError::NoSpace),
            Err(e) => return Err(e.into()),
        };
        let first_pba = self
            .layout
            .segment_first_pba(first)
            .ok_or(MetaError::NoSpace)?;
        let last = first + u64::from(count) - 1;
        let end_pba = self
            .layout
            .segment_end_pba(last)
            .ok_or(MetaError::NoSpace)?;
        self.chunks.push(Chunk {
            first_pba,
            bytes: Vec::new(),
            end_pba,
            segments: (first..=last).collect(),
        });
        self.cursor = Some(first_pba);
        Ok(())
    }

    /// Reserves `pages` pages for a region to be filled later with
    /// [`place_at`](Self::place_at) (the segment table, whose contents
    /// depend on where everything else went); returns the first block.
    pub fn reserve(&mut self, pages: u32) -> Result<u64, MetaError> {
        if pages == 0 {
            return Err(MetaError::BadRegion { bytes: 0 });
        }
        let needed = u64::from(pages);
        if self.room() < needed {
            // Seal the open segment's run: what is left of it is lost to
            // this region, and nothing else of this CP goes there.
            if let Some(chunk) = self.chunks.last() {
                for pseg in &chunk.segments {
                    if self.segments.entry(*pseg)?.seg_state() == Some(otap_format::SegState::Open)
                    {
                        self.segments.seal(*pseg)?;
                    }
                }
            }
            let per_segment = u64::from(self.layout.blocks_per_segment());
            let count = needed.div_ceil(per_segment);
            let count = u32::try_from(count).map_err(|_| MetaError::TooLarge { pages: needed })?;
            if count > self.segments.segments() {
                return Err(MetaError::TooLarge { pages: needed });
            }
            self.open_run(count)?;
        }
        let pba = self.cursor.ok_or(MetaError::NoSpace)?;
        let chunk = self.chunks.last_mut().ok_or(MetaError::NoSpace)?;
        chunk
            .bytes
            .resize(chunk.bytes.len() + pages as usize * BLOCK_SIZE, 0);
        self.cursor = Some(pba + needed);
        Ok(pba)
    }

    /// Fills a reserved region; `bytes` must be exactly the reserved length.
    pub fn place_at(&mut self, pba: u64, bytes: &[u8]) -> Result<RootRef, MetaError> {
        if bytes.is_empty() || !bytes.len().is_multiple_of(BLOCK_SIZE) {
            return Err(MetaError::BadRegion { bytes: bytes.len() });
        }
        let chunk = self
            .chunks
            .iter_mut()
            .find(|c| pba >= c.first_pba && pba < c.first_pba + (c.bytes.len() / BLOCK_SIZE) as u64)
            .ok_or(MetaError::BadRegion { bytes: bytes.len() })?;
        let start = (pba - chunk.first_pba) as usize * BLOCK_SIZE;
        if start + bytes.len() > chunk.bytes.len() {
            return Err(MetaError::BadRegion { bytes: bytes.len() });
        }
        chunk.bytes[start..start + bytes.len()].copy_from_slice(bytes);
        let root = RootRef::for_region(pba, bytes)
            .map_err(|_| MetaError::BadRegion { bytes: bytes.len() })?;
        self.regions.push(root);
        Ok(root)
    }

    /// Places a region: reserved and filled at once.
    pub fn place_region(&mut self, bytes: &[u8]) -> Result<RootRef, MetaError> {
        if bytes.is_empty() || !bytes.len().is_multiple_of(BLOCK_SIZE) {
            return Err(MetaError::BadRegion { bytes: bytes.len() });
        }
        let pages = u32::try_from(bytes.len() / BLOCK_SIZE).map_err(|_| MetaError::TooLarge {
            pages: (bytes.len() / BLOCK_SIZE) as u64,
        })?;
        let pba = self.reserve(pages)?;
        self.place_at(pba, bytes)
    }

    /// Every region placed so far.
    #[must_use]
    pub fn regions(&self) -> &[RootRef] {
        &self.regions
    }

    /// The table as the stream left it (metadata segments allocated and
    /// sealed), for the CP's table image.
    #[must_use]
    pub const fn segments(&self) -> &SegmentTable {
        &self.segments
    }

    /// The table, mutably (to record live bytes before the image is taken).
    pub fn segments_mut(&mut self) -> &mut SegmentTable {
        &mut self.segments
    }

    /// Bytes the stream placed in each metadata segment, in order.
    #[must_use]
    pub fn bytes_per_segment(&self) -> Vec<(u64, u64)> {
        let mut out: Vec<(u64, u64)> = Vec::new();
        for region in &self.regions {
            for (pseg, bytes) in self.layout.segments_of_region(region.pba, region.pages) {
                match out.iter_mut().find(|(p, _)| *p == pseg) {
                    Some((_, b)) => *b += bytes,
                    None => out.push((pseg, bytes)),
                }
            }
        }
        out
    }

    /// Finishes the stream: the stripe images to write (whole stripes, the
    /// last one of each run padded with zeros), the table, and the append
    /// point after the last region (the open metadata segment), or `None`
    /// when the last run ended exactly on a segment boundary.
    #[must_use]
    pub fn finish(self) -> (MetaStripes, SegmentTable, Option<OpenSegment>) {
        let stripe_bytes = self.layout.stripe_bytes as usize;
        let mut stripes = Vec::new();
        let mut open = None;
        for chunk in &self.chunks {
            if chunk.bytes.is_empty() {
                // The open segment was never written to: it stays open at
                // its old append point.
                if let Some((pseg, idx)) = self.layout.stripe_of_pba(chunk.first_pba) {
                    open = Some(OpenSegment {
                        pseg,
                        next_idx: idx,
                    });
                }
                continue;
            }
            for (k, piece) in chunk.bytes.chunks(stripe_bytes).enumerate() {
                let mut image = vec![0u8; stripe_bytes];
                image[..piece.len()].copy_from_slice(piece);
                let pba = chunk.first_pba + (k * stripe_bytes / BLOCK_SIZE) as u64;
                stripes.push((pba, image));
            }
            let written_stripes = chunk.bytes.len().div_ceil(stripe_bytes) as u64;
            let next_pba = chunk.first_pba + written_stripes * (stripe_bytes / BLOCK_SIZE) as u64;
            open = if next_pba >= chunk.end_pba {
                None
            } else {
                self.layout
                    .stripe_of_pba(next_pba)
                    .map(|(pseg, idx)| OpenSegment {
                        pseg,
                        next_idx: idx,
                    })
            };
        }
        let mut segments = self.segments;
        // Segments of a run that the append point left behind are sealed;
        // the one the append point is in stays open.
        for chunk in &self.chunks {
            for pseg in &chunk.segments {
                let is_open_one = open.is_some_and(|o| o.pseg == *pseg);
                if !is_open_one
                    && segments
                        .entry(*pseg)
                        .is_ok_and(|e| e.seg_state() == Some(otap_format::SegState::Open))
                {
                    let _ = segments.seal(*pseg);
                }
            }
        }
        (stripes, segments, open)
    }
}

impl RegionWriter for MetaStream<'_> {
    fn place(&mut self, bytes: &[u8]) -> Result<RootRef, MapError> {
        self.place_region(bytes).map_err(MapError::from)
    }
}
