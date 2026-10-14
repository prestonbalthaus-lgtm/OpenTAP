//! Boot: select a verified consistency point, rewind a torn one.
//!
//! The ring is read raw (a slot written by a newer format must reach
//! [`SuperblockSlot::rank`] so it can stop the boot). The ranked candidates
//! are tried newest first: the candidate's segment table and map image are
//! read and verified against the roots, and every stripe the map says that
//! CP wrote is checked by its summary (CRC and identity, including the CP's
//! nonce). The first candidate that passes is the boot's consistency point;
//! each one that fails is a torn CP, recorded with its reason.

use core::fmt;

use otap_format::{
    FormatError, FromBytes, FromZeros, IntentSlot, RootRef, SbSlot, SegKind, SegState, StripeId,
    StripeSummary, SuperblockSlot, Uuid, BLOCK_SIZE, INTENT_SLOT_SIZE, SUPERBLOCK_SLOTS,
    SUPERBLOCK_SLOT_SIZE,
};

use crate::blockmap::BlockMap;
use crate::layout::DiskLayout;
use crate::segments::{OpenSegment, SegmentError, SegmentTable};
use crate::traits::{Backstore, IoError};

/// Why a boot failed.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum BootError {
    /// No slot of the ring verifies for this aggregate.
    NoSuperblock(FormatError),
    /// A slot was written by a newer format: a hard stop (F7).
    NewerFormat(FormatError),
    /// Every ranked candidate was torn.
    NoVerifiableCp {
        /// `(cp_seq, reason)` for each candidate tried.
        tried: Vec<(u64, TornReason)>,
    },
    /// Reading the device failed.
    Io(IoError),
}

impl fmt::Display for BootError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NoSuperblock(e) => write!(f, "no verified superblock: {e}"),
            Self::NewerFormat(e) => write!(f, "{e}"),
            Self::NoVerifiableCp { tried } => {
                write!(
                    f,
                    "no verifiable consistency point among {} candidates",
                    tried.len()
                )
            }
            Self::Io(e) => write!(f, "{e}"),
        }
    }
}

impl std::error::Error for BootError {}

impl From<IoError> for BootError {
    fn from(e: IoError) -> Self {
        Self::Io(e)
    }
}

/// Why a candidate was torn.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum TornReason {
    /// The segment table root does not verify or the image is bad.
    SegmentTable(String),
    /// The map image does not verify.
    Map(FormatError),
    /// A stripe the CP wrote fails its summary check.
    Stripe {
        /// The segment.
        lseg: u64,
        /// The stripe.
        idx: u32,
        /// The reason.
        reason: FormatError,
    },
    /// A root names a region outside the device.
    RootOutOfRange {
        /// The root's field name.
        field: &'static str,
    },
    /// Reading failed.
    Io(IoError),
}

/// The ring as read: every image, verified or not.
#[derive(Debug, Clone)]
pub struct RingImages {
    /// The four superblock slots, in [`SbSlot::ALL`] order.
    pub slots: [SuperblockSlot; SUPERBLOCK_SLOTS],
    /// The two intent slots.
    pub intents: [IntentSlot; 2],
}

/// Reads the ring and the intent slots raw.
pub fn read_ring<B: Backstore>(b: &mut B, layout: &DiskLayout) -> Result<RingImages, IoError> {
    let mut slots = [SuperblockSlot::new_zeroed(); SUPERBLOCK_SLOTS];
    let mut buf = vec![0u8; SUPERBLOCK_SLOT_SIZE];
    for (i, slot) in SbSlot::ALL.iter().enumerate() {
        b.read(layout.sb_pba(*slot), &mut buf)?;
        slots[i] = SuperblockSlot::read_from_bytes(&buf).expect("slot-sized buffer");
    }
    let mut intents = [IntentSlot::new_zeroed(); 2];
    let mut ibuf = vec![0u8; INTENT_SLOT_SIZE];
    for (which, intent) in intents.iter_mut().enumerate() {
        b.read(layout.intent_pba(which as u8), &mut ibuf)?;
        *intent = IntentSlot::read_from_bytes(&ibuf).expect("intent-sized buffer");
    }
    Ok(RingImages { slots, intents })
}

/// What a boot selected and rebuilt.
#[derive(Debug, Clone)]
pub struct BootState {
    /// The selected slot.
    pub slot: SuperblockSlot,
    /// The intent slots, raw; [`otap_format::boot_write_active`] filters
    /// them.
    pub intents: [IntentSlot; 2],
    /// The map at that CP.
    pub map: BlockMap,
    /// The segment table at that CP.
    pub segments: SegmentTable,
    /// Where the next data stripe goes, if a data segment is open.
    pub open_data: Option<OpenSegment>,
    /// Where the next metadata stripe goes, if a metadata segment is open.
    pub open_meta: Option<OpenSegment>,
    /// Candidates skipped as torn, newest first.
    pub torn: Vec<(u64, TornReason)>,
    /// The metadata roots of every verified slot on the ring, selected or
    /// not: the metadata segments a boot may still need.
    pub ring_roots: Vec<RootRef>,
}

impl BootState {
    /// `true` when the newest candidate was torn and an older CP was taken.
    #[must_use]
    pub fn rewound(&self) -> bool {
        !self.torn.is_empty()
    }
}

/// Selects and loads the boot's consistency point.
pub fn evaluate<B: Backstore>(
    b: &mut B,
    layout: &DiskLayout,
    agg_id: &Uuid,
) -> Result<BootState, BootError> {
    let images = read_ring(b, layout)?;
    let ranked = SuperblockSlot::rank(agg_id, SbSlot::ALL.iter().copied().zip(images.slots.iter()))
        .map_err(|e| match e {
            FormatError::NewerFormat { .. } => BootError::NewerFormat(e),
            other => BootError::NoSuperblock(other),
        })?;
    let ring_roots: Vec<RootRef> = ranked
        .iter()
        .flat_map(|c| [c.sb.seg_table, c.sb.cp_log_head])
        .filter(|r| !r.is_none())
        .collect();
    let mut torn = Vec::new();
    for candidate in ranked {
        match load_cp(b, layout, agg_id, candidate) {
            Ok((map, segments, open_data, open_meta)) => {
                return Ok(BootState {
                    slot: *candidate,
                    intents: images.intents,
                    map,
                    segments,
                    open_data,
                    open_meta,
                    torn,
                    ring_roots,
                });
            }
            Err(reason) => torn.push((candidate.sb.cp_seq, reason)),
        }
    }
    Err(BootError::NoVerifiableCp { tried: torn })
}

type Loaded = (
    BlockMap,
    SegmentTable,
    Option<OpenSegment>,
    Option<OpenSegment>,
);

fn load_cp<B: Backstore>(
    b: &mut B,
    layout: &DiskLayout,
    agg_id: &Uuid,
    slot: &SuperblockSlot,
) -> Result<Loaded, TornReason> {
    let sb = &slot.sb;
    // A freshly formatted pool (CP 0) has no table image: every segment is
    // free.
    let segments = if sb.seg_table.is_none() {
        SegmentTable::empty(layout.segments)
    } else {
        let table_bytes = read_root(b, layout, &sb.seg_table, "seg_table")?;
        SegmentTable::from_image(&table_bytes, layout.segments)
            .map_err(|e: SegmentError| TornReason::SegmentTable(e.to_string()))?
    };
    let map_bytes = read_root(b, layout, &sb.cp_log_head, "cp_log_head")?;
    let map =
        BlockMap::load(&map_bytes, agg_id, sb.cp_seq, sb.cp_nonce).map_err(TornReason::Map)?;
    // Every stripe this CP wrote must still read as this CP's.
    let born = map.blocks_born_at(sb.cp_seq as u32);
    let mut last: Option<(u64, u32)> = None;
    let summary_len = layout.summary.summary_bytes;
    let mut buf = vec![0u8; summary_len];
    for (lseg, off) in born {
        let idx = layout.stripe_of_off(off);
        if last == Some((lseg, idx)) {
            continue;
        }
        last = Some((lseg, idx));
        let pba = layout.stripe_pba(lseg, idx).ok_or(TornReason::Stripe {
            lseg,
            idx,
            reason: FormatError::OutOfRange {
                field: "lseg",
                value: lseg,
                max: u64::from(layout.segments),
            },
        })?;
        b.read(pba, &mut buf).map_err(TornReason::Io)?;
        let id = StripeId {
            agg_id: *agg_id,
            lseg,
            stripe_idx: idx,
            cp_seq: sb.cp_seq,
            cp_nonce: sb.cp_nonce,
        };
        StripeSummary::parse_expected(&buf, &id).map_err(|reason| TornReason::Stripe {
            lseg,
            idx,
            reason,
        })?;
    }
    // Append points: data from the map's live blocks, metadata from the
    // roots' regions.
    let open_data = segments
        .open_segments(SegKind::Data)
        .into_iter()
        .max()
        .map(|pseg| {
            let end = map.used_end(pseg);
            OpenSegment {
                pseg,
                next_idx: if end == 0 {
                    0
                } else {
                    layout.stripe_of_off(end - 1) + 1
                },
            }
        });
    let open_meta = segments
        .open_segments(SegKind::Metadata)
        .into_iter()
        .max()
        .map(|pseg| {
            let mut next_idx = 0u32;
            for root in [&sb.seg_table, &sb.cp_log_head] {
                if let Some(end_idx) = region_end_stripe(layout, pseg, root) {
                    next_idx = next_idx.max(end_idx);
                }
            }
            OpenSegment { pseg, next_idx }
        });
    for (pseg, entry) in segments.iter() {
        if entry.seg_state() == Some(SegState::Open) && entry.seg_kind().is_none() {
            return Err(TornReason::SegmentTable(format!(
                "segment {pseg} has no kind"
            )));
        }
    }
    Ok((map, segments, open_data, open_meta))
}

/// One past the last stripe of `pseg` that `root`'s region touches, if the
/// region is in that segment.
fn region_end_stripe(layout: &DiskLayout, pseg: u64, root: &RootRef) -> Option<u32> {
    if root.is_none() {
        return None;
    }
    let start = layout.stripe_pba(pseg, 0)?;
    let seg_blocks = u64::from(layout.blocks_per_segment());
    if root.pba < start || root.pba >= start + seg_blocks {
        return None;
    }
    let end_block = root.pba - start + u64::from(root.pages);
    Some(end_block.div_ceil(u64::from(layout.blocks_per_stripe())) as u32)
}

fn read_root<B: Backstore>(
    b: &mut B,
    layout: &DiskLayout,
    root: &RootRef,
    field: &'static str,
) -> Result<Vec<u8>, TornReason> {
    if root.is_none() {
        return Ok(Vec::new());
    }
    let bytes = root.bytes();
    let end = root
        .pba
        .checked_add(u64::from(root.pages))
        .map(|blocks| blocks.saturating_mul(BLOCK_SIZE as u64));
    if end.is_none_or(|e| e > layout.capacity_bytes) || bytes > (1u64 << 32) {
        return Err(TornReason::RootOutOfRange { field });
    }
    let mut buf = vec![0u8; bytes as usize];
    b.read(root.pba, &mut buf).map_err(TornReason::Io)?;
    root.verify(&buf)
        .map_err(|e| TornReason::SegmentTable(format!("{field}: {e}")))?;
    Ok(buf)
}
