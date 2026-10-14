//! The receiver's resume state: which chunks of the delta in flight have
//! landed, keyed by `(cp_seq, cp_nonce)` (section 7.3, "Cursor, credits,
//! integrity").
//!
//! A sender restart resumes at `landed_contig`, the first chunk the receiver
//! has not landed, and skips the sparse ranges above it; a `DeltaBegin` with
//! the same `cp_seq` and a different `cp_nonce` discards the partial; a
//! receiver restart re-lands whatever its last persisted [`CursorPage`] did
//! not record, which is safe because landing is idempotent by
//! `(cp_nonce, chunk_idx)`.
//!
//! The sparse ranges are always in canonical form: ascending, non-empty,
//! separated by at least one unlanded chunk, and the first one strictly
//! above `landed_contig`. On the wire at most [`MAX_WIRE_RANGES`] of them
//! are carried and in the page at most [`CURSOR_PAGE_RANGES`]; anything
//! beyond is dropped, and the chunks it described land again as duplicates.
//! A sender that sends in order never produces more than a handful.

use core::mem::{align_of, offset_of, size_of};

use otap_format::checksum::crc32c;
use otap_format::{
    FormatError, FromBytes, FromZeros, Immutable, IntoBytes, KnownLayout, Uuid, MAX_CP_SEQ,
};

use crate::flow::MAX_CREDITS;
use crate::StreamError;

/// The most chunks one delta may carry: 2^24. With 8 KiB stripes that is
/// 128 GiB of payload, more than a CP or a snapshot increment is allowed to
/// hold, and it bounds the landed bitmap a peer can make a receiver allocate
/// at 2 MiB.
pub const MAX_DELTA_CHUNKS: u32 = 1 << 24;

/// Sparse ranges carried on the wire by `HelloAck`: 32 KiB of ranges, more
/// than a 64 MiB credit window can fragment into.
pub const MAX_WIRE_RANGES: usize = 4096;

/// A half-open range of chunk indices, `start..end`.
#[derive(
    FromBytes, IntoBytes, Immutable, KnownLayout, Clone, Copy, PartialEq, Eq, Hash, Default, Debug,
)]
#[repr(C)]
pub struct ChunkRange {
    /// First chunk of the range.
    pub start: u32,
    /// One past the last chunk of the range.
    pub end: u32,
}

impl ChunkRange {
    /// The range `start..end`.
    #[must_use]
    pub const fn new(start: u32, end: u32) -> Self {
        Self { start, end }
    }

    /// Chunks in the range.
    #[must_use]
    pub const fn len(&self) -> u32 {
        self.end.saturating_sub(self.start)
    }

    /// `true` when the range holds no chunk.
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.end <= self.start
    }
}

/// The landed set of one delta: one bit per chunk.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct ResumeBitmap {
    chunks: u32,
    words: Vec<u64>,
    landed: u32,
    contig: u32,
}

impl ResumeBitmap {
    /// An empty bitmap for a delta of `chunks` chunks (`1..=MAX_DELTA_CHUNKS`).
    pub fn new(chunks: u32) -> Result<Self, StreamError> {
        Self::check_chunks(chunks)?;
        Ok(Self {
            chunks,
            words: vec![0; (chunks as usize).div_ceil(64)],
            landed: 0,
            contig: 0,
        })
    }

    /// Checks a chunk count against zero and [`MAX_DELTA_CHUNKS`].
    pub const fn check_chunks(chunks: u32) -> Result<(), StreamError> {
        if chunks == 0 || chunks > MAX_DELTA_CHUNKS {
            return Err(StreamError::BadBody {
                field: "chunks",
                value: chunks as u64,
            });
        }
        Ok(())
    }

    /// Rebuilds a bitmap from its resume form: everything below `contig`
    /// landed, plus the `sparse` ranges, which must be canonical (see the
    /// module documentation).
    pub fn from_parts(
        chunks: u32,
        contig: u32,
        sparse: &[ChunkRange],
    ) -> Result<Self, StreamError> {
        let mut bitmap = Self::new(chunks)?;
        if contig > chunks {
            return Err(StreamError::ChunkOutOfRange {
                idx: contig,
                chunks,
            });
        }
        bitmap.set_range(0, contig);
        let mut floor = contig;
        for range in sparse {
            if range.is_empty() || range.start <= floor {
                return Err(StreamError::BadBody {
                    field: "sparse",
                    value: u64::from(range.start),
                });
            }
            if range.end > chunks {
                return Err(StreamError::ChunkOutOfRange {
                    idx: range.end - 1,
                    chunks,
                });
            }
            bitmap.set_range(range.start, range.end);
            floor = range.end;
        }
        bitmap.contig = bitmap.first_unlanded(0);
        if bitmap.contig != contig {
            return Err(StreamError::BadBody {
                field: "landed_contig",
                value: u64::from(contig),
            });
        }
        Ok(bitmap)
    }

    /// Sets bits `start..end` (in bounds, not yet set) a word at a time.
    fn set_range(&mut self, start: u32, end: u32) {
        let mut idx = start;
        while idx < end {
            let word = (idx / 64) as usize;
            let bit = idx % 64;
            let take = (end - idx).min(64 - bit);
            let mask = if take == 64 {
                u64::MAX
            } else {
                ((1u64 << take) - 1) << bit
            };
            let before = self.words[word].count_ones();
            self.words[word] |= mask;
            self.landed += self.words[word].count_ones() - before;
            idx += take;
        }
    }

    /// The first chunk at or after `from` that has not landed, or `chunks`.
    fn first_unlanded(&self, from: u32) -> u32 {
        let mut idx = from;
        while idx < self.chunks {
            let word_idx = (idx / 64) as usize;
            let bit = idx % 64;
            // The bits from `idx` upward, in the low positions; the shift
            // brings in zeros above, so the run never exceeds the word.
            let run = (self.words[word_idx] >> bit).trailing_ones();
            if run < 64 - bit {
                return (idx + run).min(self.chunks);
            }
            idx = (word_idx as u32 + 1) * 64;
        }
        self.chunks
    }

    /// The first chunk at or after `from` that has landed, or `chunks`.
    fn first_landed(&self, from: u32) -> u32 {
        let mut idx = from;
        while idx < self.chunks {
            let word = self.words[(idx / 64) as usize] >> (idx % 64);
            if word == 0 {
                idx = (idx / 64 + 1) * 64;
                continue;
            }
            return (idx + word.trailing_zeros()).min(self.chunks);
        }
        self.chunks
    }

    /// Chunks in the delta.
    #[must_use]
    pub const fn chunks(&self) -> u32 {
        self.chunks
    }

    /// Chunks landed so far.
    #[must_use]
    pub const fn landed_count(&self) -> u32 {
        self.landed
    }

    /// The first chunk that has not landed; `chunks` when all have.
    #[must_use]
    pub const fn landed_contig(&self) -> u32 {
        self.contig
    }

    /// `true` when every chunk has landed.
    #[must_use]
    pub const fn is_complete(&self) -> bool {
        self.landed == self.chunks
    }

    /// Whether chunk `idx` has landed; out-of-range indices never have.
    #[must_use]
    pub fn is_landed(&self, idx: u32) -> bool {
        idx < self.chunks && self.words[(idx / 64) as usize] & (1u64 << (idx % 64)) != 0
    }

    /// Marks chunk `idx` landed. Returns `true` the first time, `false` for a
    /// duplicate.
    pub fn mark(&mut self, idx: u32) -> Result<bool, StreamError> {
        if idx >= self.chunks {
            return Err(StreamError::ChunkOutOfRange {
                idx,
                chunks: self.chunks,
            });
        }
        let word = &mut self.words[(idx / 64) as usize];
        let mask = 1u64 << (idx % 64);
        if *word & mask != 0 {
            return Ok(false);
        }
        *word |= mask;
        self.landed += 1;
        if idx == self.contig {
            self.contig = self.first_unlanded(idx);
        }
        Ok(true)
    }

    /// The landed ranges above `landed_contig`, in canonical form.
    #[must_use]
    pub fn sparse_ranges(&self) -> Vec<ChunkRange> {
        let mut ranges = Vec::new();
        let mut idx = self.first_landed(self.contig);
        while idx < self.chunks {
            let end = self.first_unlanded(idx);
            ranges.push(ChunkRange::new(idx, end));
            idx = self.first_landed(end);
        }
        ranges
    }

    /// The chunks that have not landed, ascending: what a sender resends.
    pub fn missing(&self) -> impl Iterator<Item = u32> + '_ {
        (self.contig..self.chunks).filter(|idx| !self.is_landed(*idx))
    }
}

/// The delta in flight on a receiver.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Landing {
    /// The CP the delta ends at.
    pub cp_seq: u64,
    /// The nonce of that CP: the partial's identity.
    pub cp_nonce: u64,
    /// What has landed.
    pub bitmap: ResumeBitmap,
}

/// What [`Cursor::begin`] did with a `DeltaBegin`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum BeginOutcome {
    /// A new delta; nothing was in flight.
    Started,
    /// The same delta, by `(cp_seq, cp_nonce)`: landing continues from the
    /// bitmap, and the sender should resume from `landed_contig`.
    Resumed,
    /// A known `cp_seq` with a different nonce: the partial was discarded
    /// and a new landing started.
    Discarded,
    /// A different `cp_seq` while another delta was in flight: the old
    /// partial was discarded (its sender gave up on it) and the new one
    /// started.
    Replaced,
}

/// What [`Cursor::land`] did with a chunk.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum LandOutcome {
    /// First landing of this chunk.
    New,
    /// The chunk had landed before: a resend after a reconnect, counted and
    /// otherwise ignored.
    Duplicate,
}

/// The receiver's resume state for one pipe.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Cursor {
    /// The CP the replica is at.
    pub applied_cp_seq: u64,
    /// The sender's owner epoch of that CP.
    pub applied_epoch: u64,
    /// The delta in flight, if any.
    pub landing: Option<Landing>,
    /// Credits the sender holds, in 4 KiB units, at most [`MAX_CREDITS`].
    pub credits: u32,
}

impl Cursor {
    /// A cursor at `applied_cp_seq` with no delta in flight. Credits above
    /// [`MAX_CREDITS`] are capped here; a field set over a bound later is
    /// refused where the cursor leaves the process, by
    /// [`Frame::validate`](crate::frame::Frame::validate) (and so the
    /// encoder) and by [`to_page`](Self::to_page).
    #[must_use]
    pub const fn new(applied_cp_seq: u64, applied_epoch: u64, credits: u32) -> Self {
        Self {
            applied_cp_seq,
            applied_epoch,
            landing: None,
            credits: if credits > MAX_CREDITS {
                MAX_CREDITS
            } else {
                credits
            },
        }
    }

    /// Handles a `DeltaBegin` for `base..cp_seq` with `chunks` chunks. The
    /// base must be the applied CP; the outcome says whether landing resumes
    /// or starts over. A refused begin leaves any partial alone, and a
    /// resumed one allocates nothing: the bitmap (2 MiB for the largest
    /// delta) is built only once the outcome is known to need it.
    pub fn begin(
        &mut self,
        base: u64,
        cp_seq: u64,
        cp_nonce: u64,
        chunks: u32,
    ) -> Result<BeginOutcome, StreamError> {
        if base != self.applied_cp_seq {
            return Err(StreamError::BaseMismatch {
                base,
                applied: self.applied_cp_seq,
            });
        }
        if cp_seq <= base || cp_seq > MAX_CP_SEQ {
            return Err(StreamError::BadBody {
                field: "cp_seq",
                value: cp_seq,
            });
        }
        ResumeBitmap::check_chunks(chunks)?;
        let outcome = match &self.landing {
            Some(l) if l.cp_seq == cp_seq && l.cp_nonce == cp_nonce => {
                if l.bitmap.chunks() != chunks {
                    return Err(StreamError::BadBody {
                        field: "chunks",
                        value: u64::from(chunks),
                    });
                }
                return Ok(BeginOutcome::Resumed);
            }
            Some(l) if l.cp_seq == cp_seq => BeginOutcome::Discarded,
            Some(_) => BeginOutcome::Replaced,
            None => BeginOutcome::Started,
        };
        self.landing = Some(Landing {
            cp_seq,
            cp_nonce,
            bitmap: ResumeBitmap::new(chunks)?,
        });
        Ok(outcome)
    }

    /// Lands chunk `idx` of the delta identified by `(cp_seq, cp_nonce)`.
    /// A chunk of another delta is refused; a duplicate is reported.
    pub fn land(
        &mut self,
        cp_seq: u64,
        cp_nonce: u64,
        idx: u32,
    ) -> Result<LandOutcome, StreamError> {
        let landing = self.landing.as_mut().ok_or(StreamError::NoDelta)?;
        if landing.cp_seq != cp_seq || landing.cp_nonce != cp_nonce {
            return Err(StreamError::BadBody {
                field: "cp_nonce",
                value: cp_nonce,
            });
        }
        Ok(if landing.bitmap.mark(idx)? {
            LandOutcome::New
        } else {
            LandOutcome::Duplicate
        })
    }

    /// Commits the delta: every chunk must have landed. The replica moves to
    /// the delta's CP and the landing is cleared.
    pub fn commit(&mut self, epoch: u64) -> Result<u64, StreamError> {
        let landing = self.landing.as_ref().ok_or(StreamError::NoDelta)?;
        if !landing.bitmap.is_complete() {
            return Err(StreamError::Incomplete {
                landed: landing.bitmap.landed_count(),
                chunks: landing.bitmap.chunks(),
            });
        }
        self.applied_cp_seq = landing.cp_seq;
        self.applied_epoch = epoch;
        self.landing = None;
        Ok(self.applied_cp_seq)
    }

    /// Drops the delta in flight, if any.
    pub fn abort(&mut self) -> bool {
        self.landing.take().is_some()
    }

    /// The point a reconnecting sender resumes from: `(cp_seq, cp_nonce,
    /// landed_contig, sparse ranges)`, or `None` without a delta in flight.
    #[must_use]
    pub fn resume_point(&self) -> Option<(u64, u64, u32, Vec<ChunkRange>)> {
        self.landing.as_ref().map(|l| {
            (
                l.cp_seq,
                l.cp_nonce,
                l.bitmap.landed_contig(),
                l.bitmap.sparse_ranges(),
            )
        })
    }

    /// The fixed wire body (carried by `HelloAck`) and the sparse ranges that
    /// follow it, at most [`MAX_WIRE_RANGES`] of them. The body carries the
    /// cursor's fields as they are; whether they are in bounds (the credits
    /// under [`MAX_CREDITS`], the CPs under `MAX_CP_SEQ`) is for
    /// [`CursorBody::validate`], which the frame builder, the encoder and
    /// [`to_page`](Self::to_page) all apply, so a cursor the peer would
    /// refuse is never sent or persisted with other values.
    #[must_use]
    pub fn to_wire(&self) -> (CursorBody, Vec<ChunkRange>) {
        let mut body = CursorBody {
            applied_cp_seq: self.applied_cp_seq,
            applied_epoch: self.applied_epoch,
            credits: self.credits,
            ..CursorBody::new_zeroed()
        };
        let ranges = match &self.landing {
            Some(l) => {
                let mut ranges = l.bitmap.sparse_ranges();
                ranges.truncate(MAX_WIRE_RANGES);
                body.flags = CURSOR_LANDING;
                body.landing_cp_seq = l.cp_seq;
                body.landing_cp_nonce = l.cp_nonce;
                body.chunks = l.bitmap.chunks();
                body.landed_contig = l.bitmap.landed_contig();
                body.sparse_ranges = ranges.len() as u32;
                ranges
            }
            None => Vec::new(),
        };
        (body, ranges)
    }

    /// Rebuilds a cursor from its wire form.
    pub fn from_wire(body: &CursorBody, ranges: &[ChunkRange]) -> Result<Self, StreamError> {
        body.validate()?;
        if ranges.len() != body.sparse_ranges as usize || ranges.len() > MAX_WIRE_RANGES {
            return Err(StreamError::BadBody {
                field: "sparse_ranges",
                value: ranges.len() as u64,
            });
        }
        let landing = if body.flags & CURSOR_LANDING != 0 {
            Some(Landing {
                cp_seq: body.landing_cp_seq,
                cp_nonce: body.landing_cp_nonce,
                bitmap: ResumeBitmap::from_parts(body.chunks, body.landed_contig, ranges)?,
            })
        } else {
            None
        };
        Ok(Self {
            applied_cp_seq: body.applied_cp_seq,
            applied_epoch: body.applied_epoch,
            landing,
            credits: body.credits,
        })
    }

    /// Writes the cursor into a page for `pipe_id`, sealed. Refuses what
    /// [`from_page`](Self::from_page) would refuse, so a page written always
    /// reads back. Sparse ranges beyond the page's capacity are dropped:
    /// those chunks will be landed again after a receiver restart, which is
    /// harmless.
    pub fn to_page(&self, pipe_id: Uuid) -> Result<CursorPage, StreamError> {
        let (body, ranges) = self.to_wire();
        body.validate()?;
        let mut page = CursorPage::new_zeroed();
        page.magic = CURSOR_MAGIC;
        page.ver = otap_format::STREAM_VERSION;
        page.pipe_id = pipe_id;
        page.cursor = body;
        let kept = ranges.len().min(CURSOR_PAGE_RANGES);
        page.cursor.sparse_ranges = kept as u32;
        page.ranges[..kept].copy_from_slice(&ranges[..kept]);
        page.seal();
        Ok(page)
    }

    /// Rebuilds a cursor from a verified page. Reserved bytes of the page are
    /// ignored, as on every on-disk structure.
    pub fn from_page(page: &CursorPage) -> Result<Self, StreamError> {
        page.verify()?;
        let n = page.cursor.sparse_ranges as usize;
        Self::from_wire(&page.cursor, &page.ranges[..n])
    }
}

/// `CursorBody::flags` bit: a delta is in flight and the landing fields apply.
pub const CURSOR_LANDING: u32 = 1 << 0;

/// The fixed wire form of a cursor: the `HelloAck` body and the core of the
/// persisted page. Followed on the wire by `sparse_ranges` [`ChunkRange`]s.
#[derive(
    FromBytes, IntoBytes, Immutable, KnownLayout, Clone, Copy, PartialEq, Eq, Hash, Default, Debug,
)]
#[repr(C)]
pub struct CursorBody {
    /// The CP the replica is at.
    pub applied_cp_seq: u64,
    /// The sender's owner epoch of that CP.
    pub applied_epoch: u64,
    /// The CP of the delta in flight (valid under [`CURSOR_LANDING`]).
    pub landing_cp_seq: u64,
    /// The nonce of the delta in flight.
    pub landing_cp_nonce: u64,
    /// The first chunk not landed.
    pub landed_contig: u32,
    /// Credits the sender holds, at most [`MAX_CREDITS`].
    pub credits: u32,
    /// Chunks in the delta in flight.
    pub chunks: u32,
    /// [`ChunkRange`]s that follow the body.
    pub sparse_ranges: u32,
    /// [`CURSOR_LANDING`] and reserved bits.
    pub flags: u32,
    /// Reserved, zero; ignored on disk, refused on the wire.
    pub rsvd: [u8; 12],
}

impl CursorBody {
    /// Checks the flag bits, the CP bounds, the credit bound and the landing
    /// fields.
    pub fn validate(&self) -> Result<(), StreamError> {
        if self.flags & !CURSOR_LANDING != 0 {
            return Err(StreamError::BadBody {
                field: "flags",
                value: u64::from(self.flags),
            });
        }
        if self.applied_cp_seq > MAX_CP_SEQ {
            return Err(StreamError::BadBody {
                field: "applied_cp_seq",
                value: self.applied_cp_seq,
            });
        }
        if self.credits > MAX_CREDITS {
            return Err(StreamError::BadBody {
                field: "credits",
                value: u64::from(self.credits),
            });
        }
        if self.flags & CURSOR_LANDING != 0 {
            if self.landing_cp_seq <= self.applied_cp_seq || self.landing_cp_seq > MAX_CP_SEQ {
                return Err(StreamError::BadBody {
                    field: "landing_cp_seq",
                    value: self.landing_cp_seq,
                });
            }
            ResumeBitmap::check_chunks(self.chunks)?;
            if self.landed_contig > self.chunks {
                return Err(StreamError::ChunkOutOfRange {
                    idx: self.landed_contig,
                    chunks: self.chunks,
                });
            }
        } else if self.landing_cp_seq != 0
            || self.landing_cp_nonce != 0
            || self.chunks != 0
            || self.landed_contig != 0
            || self.sparse_ranges != 0
        {
            return Err(StreamError::BadBody {
                field: "landing",
                value: self.landing_cp_seq,
            });
        }
        Ok(())
    }
}

/// Cursor page magic: `OTCU`.
pub const CURSOR_MAGIC: u32 = u32::from_le_bytes(*b"OTCU");
/// Size of the persisted cursor page.
pub const CURSOR_PAGE_SIZE: usize = 4096;
/// Sparse ranges a page can hold: `(4096 - 88 - 8) / 8`.
pub const CURSOR_PAGE_RANGES: usize = 500;

/// The 4 KiB cursor page a receiver persists per pipe at every receiver CP.
#[derive(FromBytes, IntoBytes, Immutable, KnownLayout, Clone, Copy, PartialEq, Eq, Hash, Debug)]
#[repr(C, align(4096))]
pub struct CursorPage {
    /// [`CURSOR_MAGIC`].
    pub magic: u32,
    /// The stream version.
    pub ver: u16,
    /// Reserved, zero.
    pub rsvd0: u16,
    /// The pipe this cursor belongs to.
    pub pipe_id: Uuid,
    /// The cursor.
    pub cursor: CursorBody,
    /// The sparse ranges; `cursor.sparse_ranges` of them are used.
    pub ranges: [ChunkRange; CURSOR_PAGE_RANGES],
    /// Reserved, zero.
    pub rsvd1: u32,
    /// CRC-32C over bytes `0..4092`.
    pub crc32c: u32,
}

impl CursorPage {
    /// The CRC-32C over every byte before `crc32c`.
    #[must_use]
    pub fn compute_crc(&self) -> u32 {
        crc32c(&self.as_bytes()[..CURSOR_PAGE_SIZE - 4])
    }

    /// Stores the CRC.
    pub fn seal(&mut self) {
        self.crc32c = self.compute_crc();
    }

    /// Checks magic, major version, CRC and the cursor fields.
    pub fn verify(&self) -> Result<(), StreamError> {
        if self.magic != CURSOR_MAGIC {
            return Err(FormatError::BadMagic {
                expected: CURSOR_MAGIC,
                found: self.magic,
            }
            .into());
        }
        if (self.ver >> 8) as u8 != otap_format::STREAM_MAJOR {
            return Err(FormatError::BadVersion {
                expected_major: otap_format::STREAM_MAJOR,
                found: self.ver,
            }
            .into());
        }
        let computed = self.compute_crc();
        if computed != self.crc32c {
            return Err(FormatError::BadCrc {
                region: "cursor page",
                computed,
                stored: self.crc32c,
            }
            .into());
        }
        self.cursor.validate()?;
        if self.cursor.sparse_ranges as usize > CURSOR_PAGE_RANGES {
            return Err(StreamError::BadBody {
                field: "sparse_ranges",
                value: u64::from(self.cursor.sparse_ranges),
            });
        }
        Ok(())
    }

    /// A verified copy out of a 4 KiB buffer of any alignment.
    pub fn read(bytes: &[u8]) -> Result<Self, StreamError> {
        let page = Self::read_from_bytes(bytes).map_err(|_| {
            StreamError::Format(FormatError::BadLength {
                expected: CURSOR_PAGE_SIZE,
                found: bytes.len(),
            })
        })?;
        page.verify()?;
        Ok(page)
    }
}

const _: () = {
    assert!(size_of::<ChunkRange>() == 8);
    assert!(align_of::<ChunkRange>() == 4);
    assert!(size_of::<CursorBody>() == 64);
    assert!(align_of::<CursorBody>() == 8);
    assert!(offset_of!(CursorBody, applied_cp_seq) == 0);
    assert!(offset_of!(CursorBody, applied_epoch) == 8);
    assert!(offset_of!(CursorBody, landing_cp_seq) == 16);
    assert!(offset_of!(CursorBody, landing_cp_nonce) == 24);
    assert!(offset_of!(CursorBody, landed_contig) == 32);
    assert!(offset_of!(CursorBody, credits) == 36);
    assert!(offset_of!(CursorBody, chunks) == 40);
    assert!(offset_of!(CursorBody, sparse_ranges) == 44);
    assert!(offset_of!(CursorBody, flags) == 48);
    assert!(offset_of!(CursorBody, rsvd) == 52);
    assert!(size_of::<CursorPage>() == CURSOR_PAGE_SIZE);
    assert!(align_of::<CursorPage>() == 4096);
    assert!(offset_of!(CursorPage, magic) == 0);
    assert!(offset_of!(CursorPage, ver) == 4);
    assert!(offset_of!(CursorPage, pipe_id) == 8);
    assert!(offset_of!(CursorPage, cursor) == 24);
    assert!(offset_of!(CursorPage, ranges) == 88);
    assert!(offset_of!(CursorPage, rsvd1) == 88 + CURSOR_PAGE_RANGES * 8);
    assert!(offset_of!(CursorPage, crc32c) == CURSOR_PAGE_SIZE - 4);
    assert!(MAX_WIRE_RANGES * 8 + 64 < otap_format::MAX_WIRE_PAYLOAD as usize);
};
