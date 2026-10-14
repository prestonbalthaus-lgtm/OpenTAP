//! The stripe image: summary header, per-stripe volume bitmap and per-block
//! descriptors (F3, F4, F9).
//!
//! A stripe image is `[summary][data]`. The summary is
//!
//! ```text
//! offset 0    StripeHdr          64 B   magic, ver, kind, identity, counts, CRCs
//! offset 64   VolBitSet         128 B   the volumes with blocks in this stripe
//! offset 192  StripeDescriptor  16 B x nblocks
//! ...         zero padding to a 4 KiB multiple
//! ```
//!
//! and the data region holds `nblocks` 4 KiB blocks in descriptor order.
//! `summary_crc` covers everything from offset 64 to the end of the padded
//! summary; `hdr_crc` covers the first 60 bytes of the header. The bitmap is
//! derived from the descriptors by the seal paths and checked against them by
//! the parser, so a consumer may skip stripes by bitmap. Each block's own
//! CRC-32C is in its descriptor.
//!
//! # Integrity versus identity
//! [`StripeSummary::parse`] proves that a summary is intact and
//! self-consistent. It does not prove that it is the summary the caller
//! wanted: a physical location reused across CPs can hold an older stripe
//! whose CRCs pass. Every reader that knows which stripe it expects (the
//! dirty-boot scan of section 1.3, a resend through `read_chunk`, the
//! receiver landing a chunk) therefore uses [`StripeSummary::parse_expected`]
//! with the [`StripeId`] from its metadata, and treats
//! [`FormatError::IdentityMismatch`] as a torn or missing stripe.
//!
//! The stripe image is the chunk payload of the replication stream byte for
//! byte, re-derivable from disk by `(lseg, stripe_idx)` (R25). The 64-byte
//! stream frame is prepended by the transport and never stored.

use core::mem::{align_of, offset_of, size_of};

use zerocopy::{FromBytes, FromZeros, Immutable, IntoBytes, KnownLayout};

use crate::checksum::crc32c;
use crate::constants::{
    BLOCK_KIND_BITS, BLOCK_SIZE, FORMAT_MAJOR, FORMAT_VERSION, LBA_BITS, MAX_CP_SEQ, MAX_LBA,
    MAX_STRIPE_BYTES, MAX_VOL, MAX_VOLS, MAX_VOL_ID, MIN_STRIPE_BYTES, NULL_LSEG,
    STRIPE_DESCRIPTORS_OFFSET, STRIPE_DESCRIPTOR_SIZE, STRIPE_HDR_CRC_OFFSET, STRIPE_HDR_SIZE,
    STRIPE_MAGIC, STRIPE_SUMMARY_FIXED, STRIPE_VOL_BITMAP_OFFSET, VOL_BITS, ZERO_BLOCK_CRC32C,
};
use crate::types::{
    bad_field, cast_error, check_crc, check_magic, check_major, copy_out, view, FormatError, Uuid,
    VolBitSet,
};

/// What a stripe carries. Stripes are homogeneous in kind, and only `Ingest`
/// stripes of outbound consistency groups ship (section 1.3).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[repr(u8)]
pub enum StripeKind {
    /// Client writes drained from a generation.
    Ingest = 0,
    /// Live blocks moved by the cleaner, birth preserved.
    Relocate = 1,
    /// Blocks landed by the replication receiver.
    Applied = 2,
    /// An in-place patch merge.
    Merge = 3,
}

impl StripeKind {
    /// Decodes the on-disk byte.
    #[must_use]
    pub const fn from_u8(value: u8) -> Option<Self> {
        match value {
            0 => Some(Self::Ingest),
            1 => Some(Self::Relocate),
            2 => Some(Self::Applied),
            3 => Some(Self::Merge),
            _ => None,
        }
    }

    /// The on-disk byte.
    #[must_use]
    pub const fn as_u8(self) -> u8 {
        self as u8
    }
}

/// What a descriptor's block is (F4). Values 2 and 3 are reserved.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[repr(u8)]
pub enum BlockKind {
    /// A 4 KiB data block present in the data region.
    Data = 0,
    /// A hole punched at `birth_cp`: the block reads as zero. Its data slot is
    /// present and zero, so the data region stays `nblocks * 4 KiB` and the
    /// descriptor's CRC is [`ZERO_BLOCK_CRC32C`] (F9).
    Hole = 1,
}

impl BlockKind {
    /// Decodes the 2-bit field.
    #[must_use]
    pub const fn from_u8(value: u8) -> Option<Self> {
        match value {
            0 => Some(Self::Data),
            1 => Some(Self::Hole),
            _ => None,
        }
    }

    /// The 2-bit field.
    #[must_use]
    pub const fn as_u8(self) -> u8 {
        self as u8
    }
}

/// The 64-byte stripe summary header.
#[derive(FromBytes, IntoBytes, Immutable, KnownLayout, Clone, Copy, PartialEq, Eq, Hash, Debug)]
#[repr(C)]
pub struct StripeHdr {
    /// [`STRIPE_MAGIC`].
    pub magic: u32,
    /// [`FORMAT_VERSION`].
    pub ver: u16,
    /// A [`StripeKind`] byte.
    pub kind: u8,
    /// Reserved flag bits, zero; ignored by readers (a later minor may define them).
    pub flags: u8,
    /// The aggregate the stripe belongs to.
    pub agg_id: Uuid,
    /// The CP that wrote the stripe, at most [`MAX_CP_SEQ`].
    pub cp_seq: u64,
    /// The nonce of that CP.
    pub cp_nonce: u64,
    /// The logical segment the stripe is in, never [`NULL_LSEG`].
    pub lseg: u64,
    /// The stripe's index within the segment.
    pub stripe_idx: u32,
    /// Descriptors (and data blocks) in the stripe, at least 1.
    pub nblocks: u16,
    /// The consistency group the stripe was packed for.
    pub group: u16,
    /// CRC-32C over the summary from offset 64 to the end of its padding.
    pub summary_crc: u32,
    /// CRC-32C over bytes `0..60` of this header.
    pub hdr_crc: u32,
}

impl StripeHdr {
    /// A zeroed header carrying the magic, the version, the kind and the
    /// aggregate. The caller sets the identity fields and `nblocks`.
    #[must_use]
    pub fn new(agg_id: Uuid, kind: StripeKind) -> Self {
        let mut hdr = Self::new_zeroed();
        hdr.magic = STRIPE_MAGIC;
        hdr.ver = FORMAT_VERSION;
        hdr.kind = kind.as_u8();
        hdr.agg_id = agg_id;
        hdr
    }

    /// The CRC-32C over bytes `0..60`.
    #[must_use]
    pub fn compute_hdr_crc(&self) -> u32 {
        crc32c(&self.as_bytes()[..STRIPE_HDR_CRC_OFFSET])
    }

    /// Stores `hdr_crc`. `summary_crc` must already be set.
    pub fn seal_hdr(&mut self) {
        self.hdr_crc = self.compute_hdr_crc();
    }

    /// Checks everything but the CRCs: magic, major version, kind, a real
    /// segment, the CP bound and `nblocks >= 1`. This is what the seal paths
    /// require before they write.
    pub fn validate_fields(&self) -> Result<(), FormatError> {
        check_magic(self.magic, STRIPE_MAGIC)?;
        check_major(self.ver, FORMAT_MAJOR)?;
        if StripeKind::from_u8(self.kind).is_none() {
            return Err(bad_field("kind", u64::from(self.kind)));
        }
        if self.lseg == NULL_LSEG {
            return Err(bad_field("lseg", self.lseg));
        }
        if self.cp_seq > MAX_CP_SEQ {
            return Err(FormatError::OutOfRange {
                field: "cp_seq",
                value: self.cp_seq,
                max: MAX_CP_SEQ,
            });
        }
        if self.nblocks == 0 {
            return Err(bad_field("nblocks", 0));
        }
        Ok(())
    }

    /// Checks magic, major version, header CRC and every field. The summary
    /// CRC needs the summary bytes: see [`StripeSummary::parse`].
    pub fn verify_hdr(&self) -> Result<(), FormatError> {
        check_magic(self.magic, STRIPE_MAGIC)?;
        check_major(self.ver, FORMAT_MAJOR)?;
        check_crc("stripe header", self.compute_hdr_crc(), self.hdr_crc)?;
        self.validate_fields()
    }

    /// Checks that this header names the stripe `id` describes.
    pub fn check_identity(&self, id: &StripeId) -> Result<(), FormatError> {
        let mismatch = |field| FormatError::IdentityMismatch { field };
        if self.agg_id != id.agg_id {
            return Err(mismatch("agg_id"));
        }
        if self.lseg != id.lseg {
            return Err(mismatch("lseg"));
        }
        if self.stripe_idx != id.stripe_idx {
            return Err(mismatch("stripe_idx"));
        }
        if self.cp_seq != id.cp_seq {
            return Err(mismatch("cp_seq"));
        }
        if self.cp_nonce != id.cp_nonce {
            return Err(mismatch("cp_nonce"));
        }
        Ok(())
    }

    /// A header-verified zero-copy view over a 64-byte, 8-aligned buffer.
    pub fn parse(bytes: &[u8]) -> Result<&Self, FormatError> {
        let hdr: &Self = view(bytes)?;
        hdr.verify_hdr()?;
        Ok(hdr)
    }

    /// A header-verified copy out of a 64-byte buffer of any alignment.
    pub fn read(bytes: &[u8]) -> Result<Self, FormatError> {
        let hdr: Self = copy_out(bytes)?;
        hdr.verify_hdr()?;
        Ok(hdr)
    }

    /// The decoded stripe kind.
    #[must_use]
    pub const fn stripe_kind(&self) -> Option<StripeKind> {
        StripeKind::from_u8(self.kind)
    }

    /// The padded summary length for this header's `nblocks`.
    #[must_use]
    pub const fn summary_bytes(&self) -> usize {
        summary_bytes(self.nblocks as usize)
    }

    /// The data region length for this header's `nblocks`.
    #[must_use]
    pub const fn data_bytes(&self) -> usize {
        self.nblocks as usize * BLOCK_SIZE
    }

    /// The identity this header carries.
    #[must_use]
    pub const fn id(&self) -> StripeId {
        StripeId {
            agg_id: self.agg_id,
            lseg: self.lseg,
            stripe_idx: self.stripe_idx,
            cp_seq: self.cp_seq,
            cp_nonce: self.cp_nonce,
        }
    }
}

/// What a reader expects a stripe to be: the fields that name one stripe of
/// one CP of one aggregate.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct StripeId {
    /// The aggregate.
    pub agg_id: Uuid,
    /// The logical segment.
    pub lseg: u64,
    /// The stripe's index within the segment.
    pub stripe_idx: u32,
    /// The CP that wrote it.
    pub cp_seq: u64,
    /// That CP's nonce.
    pub cp_nonce: u64,
}

/// Shift of the volume index inside `vol_lba_kind`.
pub const DESC_VOL_SHIFT: u32 = 64 - VOL_BITS;
/// Shift of the LBA inside `vol_lba_kind`.
pub const DESC_LBA_SHIFT: u32 = DESC_VOL_SHIFT - LBA_BITS;
/// Shift of the block kind inside `vol_lba_kind`.
pub const DESC_KIND_SHIFT: u32 = DESC_LBA_SHIFT - BLOCK_KIND_BITS;
/// Mask of the reserved low bits of `vol_lba_kind`.
pub const DESC_RSVD_MASK: u64 = (1 << DESC_KIND_SHIFT) - 1;

/// The 16-byte per-block descriptor.
///
/// `vol_lba_kind` packs, from the most significant bit down: `vol` (20 bits,
/// valid below [`MAX_VOLS`]), `lba` (40 bits, in 4 KiB blocks), `kind` (2
/// bits) and 2 reserved bits, which readers ignore.
#[derive(
    FromBytes, IntoBytes, Immutable, KnownLayout, Clone, Copy, PartialEq, Eq, Hash, Default, Debug,
)]
#[repr(C)]
pub struct StripeDescriptor {
    /// `vol:20 | lba:40 | kind:2 | rsvd:2`.
    pub vol_lba_kind: u64,
    /// The CP that wrote the block or punched the hole, at least 1 (F5, F9).
    pub birth_cp: u32,
    /// CRC-32C of the 4 KiB data slot: of the block for `Data`, and
    /// [`ZERO_BLOCK_CRC32C`] for `Hole`.
    pub crc32c: u32,
}

impl StripeDescriptor {
    /// Packs the identity word. `vol` must be below [`MAX_VOLS`] and `lba`
    /// at most [`MAX_LBA`].
    pub const fn pack(vol: u32, lba: u64, kind: BlockKind) -> Result<u64, FormatError> {
        if vol > MAX_VOL {
            return Err(FormatError::OutOfRange {
                field: "vol",
                value: vol as u64,
                max: MAX_VOL as u64,
            });
        }
        if lba > MAX_LBA {
            return Err(FormatError::OutOfRange {
                field: "lba",
                value: lba,
                max: MAX_LBA,
            });
        }
        Ok(((vol as u64) << DESC_VOL_SHIFT)
            | (lba << DESC_LBA_SHIFT)
            | ((kind.as_u8() as u64) << DESC_KIND_SHIFT))
    }

    /// A descriptor for block `lba` of volume `vol`, checked by
    /// [`validate`](Self::validate).
    pub const fn new(
        vol: u32,
        lba: u64,
        kind: BlockKind,
        birth_cp: u32,
        crc32c: u32,
    ) -> Result<Self, FormatError> {
        let vol_lba_kind = match Self::pack(vol, lba, kind) {
            Ok(word) => word,
            Err(e) => return Err(e),
        };
        let desc = Self {
            vol_lba_kind,
            birth_cp,
            crc32c,
        };
        match desc.validate() {
            Ok(()) => Ok(desc),
            Err(e) => Err(e),
        }
    }

    /// A data block descriptor.
    pub const fn data(vol: u32, lba: u64, birth_cp: u32, crc32c: u32) -> Result<Self, FormatError> {
        Self::new(vol, lba, BlockKind::Data, birth_cp, crc32c)
    }

    /// A hole descriptor; its CRC is [`ZERO_BLOCK_CRC32C`].
    pub const fn hole(vol: u32, lba: u64, birth_cp: u32) -> Result<Self, FormatError> {
        Self::new(vol, lba, BlockKind::Hole, birth_cp, ZERO_BLOCK_CRC32C)
    }

    /// The volume index.
    #[must_use]
    pub const fn vol(&self) -> u32 {
        ((self.vol_lba_kind >> DESC_VOL_SHIFT) & MAX_VOL_ID as u64) as u32
    }

    /// The logical block address.
    #[must_use]
    pub const fn lba(&self) -> u64 {
        (self.vol_lba_kind >> DESC_LBA_SHIFT) & MAX_LBA
    }

    /// The raw 2-bit kind field.
    #[must_use]
    pub const fn kind_raw(&self) -> u8 {
        ((self.vol_lba_kind >> DESC_KIND_SHIFT) & ((1 << BLOCK_KIND_BITS) - 1)) as u8
    }

    /// The decoded block kind; `None` for the reserved values.
    #[must_use]
    pub const fn kind(&self) -> Option<BlockKind> {
        BlockKind::from_u8(self.kind_raw())
    }

    /// The reserved low bits.
    #[must_use]
    pub const fn rsvd_bits(&self) -> u8 {
        (self.vol_lba_kind & DESC_RSVD_MASK) as u8
    }

    /// Rejects a reserved kind, a volume index at or above [`MAX_VOLS`], a
    /// zero birth and a hole whose CRC is not that of a zero block.
    pub const fn validate(&self) -> Result<(), FormatError> {
        let kind = match self.kind() {
            Some(kind) => kind,
            None => return Err(bad_field("kind", self.kind_raw() as u64)),
        };
        if self.vol() > MAX_VOL {
            return Err(FormatError::OutOfRange {
                field: "vol",
                value: self.vol() as u64,
                max: MAX_VOL as u64,
            });
        }
        if self.birth_cp == 0 {
            return Err(bad_field("birth_cp", 0));
        }
        if matches!(kind, BlockKind::Hole) && self.crc32c != ZERO_BLOCK_CRC32C {
            return Err(bad_field("crc32c", self.crc32c as u64));
        }
        Ok(())
    }
}

/// The padded summary length for `nblocks` descriptors: header, bitmap and
/// descriptors rounded up to a 4 KiB multiple. Saturates on absurd counts;
/// every caller bounds `nblocks` by `u16::MAX`.
#[must_use]
pub const fn summary_bytes(nblocks: usize) -> usize {
    let raw = STRIPE_SUMMARY_FIXED.saturating_add(nblocks.saturating_mul(STRIPE_DESCRIPTOR_SIZE));
    raw.div_ceil(BLOCK_SIZE).saturating_mul(BLOCK_SIZE)
}

/// Byte offset of descriptor `i` inside a summary.
#[must_use]
pub const fn descriptor_offset(i: usize) -> usize {
    STRIPE_DESCRIPTORS_OFFSET.saturating_add(i.saturating_mul(STRIPE_DESCRIPTOR_SIZE))
}

/// The volumes a set of descriptors touches: the bitmap a summary carries.
#[must_use]
pub fn derive_vol_bitmap(descriptors: &[StripeDescriptor]) -> VolBitSet {
    let mut bitmap = VolBitSet::EMPTY;
    for desc in descriptors {
        // A volume index above the bound is rejected by validate(); here it
        // simply contributes no bit, so the comparison in parse() fails.
        let _ = bitmap.set(desc.vol());
    }
    bitmap
}

/// How a stripe of a given size is split between summary and data. Values are
/// produced by [`for_stripe`](Self::for_stripe); a hand-built layout is not
/// checked.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct SummaryLayout {
    /// The stripe size in bytes, a multiple of 4 KiB.
    pub stripe_bytes: usize,
    /// The most blocks the stripe can carry.
    pub nblocks: usize,
    /// The padded summary length for `nblocks`.
    pub summary_bytes: usize,
}

impl SummaryLayout {
    /// `true` when `nblocks` descriptors and data blocks fit in `stripe_bytes`.
    #[must_use]
    pub const fn fits(stripe_bytes: usize, nblocks: usize) -> bool {
        nblocks <= u16::MAX as usize
            && nblocks
                .saturating_mul(BLOCK_SIZE)
                .saturating_add(summary_bytes(nblocks))
                <= stripe_bytes
    }

    /// The layout of a stripe of `stripe_bytes`, which must be a multiple of
    /// 4 KiB between [`MIN_STRIPE_BYTES`] and [`MAX_STRIPE_BYTES`].
    pub const fn for_stripe(stripe_bytes: usize) -> Result<Self, FormatError> {
        if !stripe_bytes.is_multiple_of(BLOCK_SIZE)
            || stripe_bytes < MIN_STRIPE_BYTES
            || stripe_bytes > MAX_STRIPE_BYTES
        {
            return Err(bad_field("stripe_bytes", stripe_bytes as u64));
        }
        // Each block costs 4096 bytes of data and 16 bytes of descriptor;
        // start from that estimate and settle on the padding boundary.
        let estimate =
            (stripe_bytes - STRIPE_SUMMARY_FIXED) / (BLOCK_SIZE + STRIPE_DESCRIPTOR_SIZE);
        let mut nblocks = if estimate > u16::MAX as usize {
            u16::MAX as usize
        } else {
            estimate
        };
        while nblocks > 0 && !Self::fits(stripe_bytes, nblocks) {
            nblocks -= 1;
        }
        while Self::fits(stripe_bytes, nblocks + 1) {
            nblocks += 1;
        }
        Ok(Self {
            stripe_bytes,
            nblocks,
            summary_bytes: summary_bytes(nblocks),
        })
    }

    /// Byte offset of the data region.
    #[must_use]
    pub const fn data_offset(&self) -> usize {
        self.summary_bytes
    }

    /// Byte offset of data block `i`.
    #[must_use]
    pub const fn block_offset(&self, i: usize) -> usize {
        self.summary_bytes
            .saturating_add(i.saturating_mul(BLOCK_SIZE))
    }

    /// Bytes of the stripe that are neither summary nor data.
    #[must_use]
    pub const fn slack_bytes(&self) -> usize {
        self.stripe_bytes
            .saturating_sub(self.summary_bytes)
            .saturating_sub(self.nblocks.saturating_mul(BLOCK_SIZE))
    }
}

/// A verified, borrowed view of a stripe summary. Only [`parse`](Self::parse)
/// and [`parse_expected`](Self::parse_expected) produce one; the fields are
/// read-only through the view.
#[derive(Debug, Clone, Copy)]
pub struct StripeSummary<'a> {
    hdr: &'a StripeHdr,
    vol_bitmap: &'a VolBitSet,
    descriptors: &'a [StripeDescriptor],
}

impl<'a> StripeSummary<'a> {
    /// Parses and verifies a summary at the start of `region`, which must be
    /// 8-aligned and at least `summary_bytes(nblocks)` long (a whole stripe
    /// image qualifies). Both CRCs are checked, every descriptor is
    /// validated, and the stored bitmap must equal the one the descriptors
    /// imply. This proves integrity, not identity: see the module
    /// documentation and [`parse_expected`](Self::parse_expected).
    pub fn parse(region: &'a [u8]) -> Result<Self, FormatError> {
        let (hdr, rest) = StripeHdr::ref_from_prefix(region)
            .map_err(|e| cast_error(e, STRIPE_HDR_SIZE, region.len(), align_of::<StripeHdr>()))?;
        hdr.verify_hdr()?;
        let nblocks = hdr.nblocks as usize;
        let total = summary_bytes(nblocks);
        if region.len() < total {
            return Err(FormatError::BadLength {
                expected: total,
                found: region.len(),
            });
        }
        check_crc(
            "stripe summary",
            crc32c(&region[STRIPE_VOL_BITMAP_OFFSET..total]),
            hdr.summary_crc,
        )?;
        let (vol_bitmap, rest) = VolBitSet::ref_from_prefix(rest).map_err(|e| {
            cast_error(
                e,
                STRIPE_SUMMARY_FIXED,
                region.len(),
                align_of::<VolBitSet>(),
            )
        })?;
        let (descriptors, _) = <[StripeDescriptor]>::ref_from_prefix_with_elems(rest, nblocks)
            .map_err(|e| cast_error(e, total, region.len(), align_of::<StripeDescriptor>()))?;
        for desc in descriptors {
            desc.validate()?;
        }
        if derive_vol_bitmap(descriptors) != *vol_bitmap {
            return Err(bad_field("vol_bitmap", 0));
        }
        Ok(Self {
            hdr,
            vol_bitmap,
            descriptors,
        })
    }

    /// [`parse`](Self::parse), then a check that the summary is the stripe
    /// `id` names; a stale image at a reused location fails with
    /// [`FormatError::IdentityMismatch`].
    pub fn parse_expected(region: &'a [u8], id: &StripeId) -> Result<Self, FormatError> {
        let summary = Self::parse(region)?;
        summary.hdr.check_identity(id)?;
        Ok(summary)
    }

    /// The header.
    #[must_use]
    pub const fn hdr(&self) -> &'a StripeHdr {
        self.hdr
    }

    /// The volumes with blocks in the stripe.
    #[must_use]
    pub const fn vol_bitmap(&self) -> &'a VolBitSet {
        self.vol_bitmap
    }

    /// The `nblocks` descriptors.
    #[must_use]
    pub const fn descriptors(&self) -> &'a [StripeDescriptor] {
        self.descriptors
    }

    /// The padded summary length.
    #[must_use]
    pub const fn summary_bytes(&self) -> usize {
        self.hdr.summary_bytes()
    }
}

fn check_descriptor_count(hdr: &StripeHdr, count: usize) -> Result<usize, FormatError> {
    if count > u16::MAX as usize {
        return Err(FormatError::OutOfRange {
            field: "nblocks",
            value: count as u64,
            max: u64::from(u16::MAX),
        });
    }
    if count != hdr.nblocks as usize {
        return Err(bad_field("nblocks", count as u64));
    }
    Ok(count)
}

/// Writes a complete, sealed summary into the start of `region`: the header
/// (with both CRCs computed here), the bitmap derived from the descriptors,
/// the descriptors and zero padding. The header's fields and every descriptor
/// are validated first, and `hdr.nblocks` must equal `descriptors.len()`.
/// Nothing is written on error. Returns the padded summary length. No
/// allocation; `region` may be a direct-I/O buffer.
pub fn write_summary(
    region: &mut [u8],
    hdr: &StripeHdr,
    descriptors: &[StripeDescriptor],
) -> Result<usize, FormatError> {
    hdr.validate_fields()?;
    let nblocks = check_descriptor_count(hdr, descriptors.len())?;
    for desc in descriptors {
        desc.validate()?;
    }
    let total = summary_bytes(nblocks);
    if region.len() < total {
        return Err(FormatError::BadLength {
            expected: total,
            found: region.len(),
        });
    }
    let region = &mut region[..total];
    let descs_end = descriptor_offset(nblocks);
    region[STRIPE_VOL_BITMAP_OFFSET..STRIPE_DESCRIPTORS_OFFSET]
        .copy_from_slice(derive_vol_bitmap(descriptors).as_bytes());
    region[STRIPE_DESCRIPTORS_OFFSET..descs_end].copy_from_slice(descriptors.as_bytes());
    region[descs_end..].fill(0);
    let mut sealed = *hdr;
    sealed.summary_crc = crc32c(&region[STRIPE_VOL_BITMAP_OFFSET..]);
    sealed.seal_hdr();
    region[..STRIPE_HDR_SIZE].copy_from_slice(sealed.as_bytes());
    Ok(total)
}

/// Seals a summary that was assembled in place: the header (fields set, CRCs
/// not) at the start of `region` and the descriptors at their offsets. The
/// header's fields and every descriptor are validated, the padding after the
/// descriptors must already be zero (a non-zero byte there means a
/// descriptor count that is off, and is reported rather than erased), the
/// bitmap is derived and written, then `summary_crc` and `hdr_crc`. Returns
/// the padded summary length. Works at any alignment.
pub fn seal_summary_in_place(region: &mut [u8]) -> Result<usize, FormatError> {
    let len = region.len();
    let hdr: StripeHdr = StripeHdr::read_from_prefix(region)
        .map_err(|_| FormatError::BadLength {
            expected: STRIPE_HDR_SIZE,
            found: len,
        })?
        .0;
    hdr.validate_fields()?;
    let nblocks = hdr.nblocks as usize;
    let total = summary_bytes(nblocks);
    if len < total {
        return Err(FormatError::BadLength {
            expected: total,
            found: len,
        });
    }
    let descs_end = descriptor_offset(nblocks);
    let mut bitmap = VolBitSet::EMPTY;
    let (raw_descs, _) =
        region[STRIPE_DESCRIPTORS_OFFSET..descs_end].as_chunks::<STRIPE_DESCRIPTOR_SIZE>();
    for raw in raw_descs {
        let desc: StripeDescriptor = copy_out(raw)?;
        desc.validate()?;
        bitmap.set(desc.vol())?;
    }
    if let Some(dirty) = region[descs_end..total].iter().position(|b| *b != 0) {
        return Err(bad_field("padding", (descs_end + dirty) as u64));
    }
    region[STRIPE_VOL_BITMAP_OFFSET..STRIPE_DESCRIPTORS_OFFSET].copy_from_slice(bitmap.as_bytes());
    let mut sealed = hdr;
    sealed.summary_crc = crc32c(&region[STRIPE_VOL_BITMAP_OFFSET..total]);
    sealed.seal_hdr();
    region[..STRIPE_HDR_SIZE].copy_from_slice(sealed.as_bytes());
    Ok(total)
}

const _: () = {
    assert!(StripeKind::Ingest.as_u8() == 0);
    assert!(StripeKind::Relocate.as_u8() == 1);
    assert!(StripeKind::Applied.as_u8() == 2);
    assert!(StripeKind::Merge.as_u8() == 3);
    assert!(matches!(StripeKind::from_u8(0), Some(StripeKind::Ingest)));
    assert!(matches!(StripeKind::from_u8(1), Some(StripeKind::Relocate)));
    assert!(matches!(StripeKind::from_u8(2), Some(StripeKind::Applied)));
    assert!(matches!(StripeKind::from_u8(3), Some(StripeKind::Merge)));
    assert!(StripeKind::from_u8(4).is_none());
    assert!(BlockKind::Data.as_u8() == 0);
    assert!(BlockKind::Hole.as_u8() == 1);
    assert!(matches!(BlockKind::from_u8(0), Some(BlockKind::Data)));
    assert!(matches!(BlockKind::from_u8(1), Some(BlockKind::Hole)));
    assert!(BlockKind::from_u8(2).is_none() && BlockKind::from_u8(3).is_none());

    assert!(size_of::<StripeHdr>() == STRIPE_HDR_SIZE);
    assert!(align_of::<StripeHdr>() == 8);
    assert!(offset_of!(StripeHdr, magic) == 0);
    assert!(offset_of!(StripeHdr, ver) == 4);
    assert!(offset_of!(StripeHdr, kind) == 6);
    assert!(offset_of!(StripeHdr, flags) == 7);
    assert!(offset_of!(StripeHdr, agg_id) == 8);
    assert!(offset_of!(StripeHdr, cp_seq) == 24);
    assert!(offset_of!(StripeHdr, cp_nonce) == 32);
    assert!(offset_of!(StripeHdr, lseg) == 40);
    assert!(offset_of!(StripeHdr, stripe_idx) == 48);
    assert!(offset_of!(StripeHdr, nblocks) == 52);
    assert!(offset_of!(StripeHdr, group) == 54);
    assert!(offset_of!(StripeHdr, summary_crc) == 56);
    assert!(offset_of!(StripeHdr, hdr_crc) == STRIPE_HDR_CRC_OFFSET);

    assert!(size_of::<StripeDescriptor>() == STRIPE_DESCRIPTOR_SIZE);
    assert!(align_of::<StripeDescriptor>() == 8);
    assert!(offset_of!(StripeDescriptor, vol_lba_kind) == 0);
    assert!(offset_of!(StripeDescriptor, birth_cp) == 8);
    assert!(offset_of!(StripeDescriptor, crc32c) == 12);

    assert!(DESC_VOL_SHIFT == 44);
    assert!(DESC_LBA_SHIFT == 4);
    assert!(DESC_KIND_SHIFT == 2);
    assert!(DESC_RSVD_MASK == 0b11);
    assert!(summary_bytes(0) == 4096);
    assert!(summary_bytes(244) == 4096);
    assert!(summary_bytes(245) == 8192);
    assert!(summary_bytes(u16::MAX as usize) == 1_052_672);
    assert!(MAX_VOLS as u32 - 1 == MAX_VOL);
};
