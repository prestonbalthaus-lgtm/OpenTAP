//! Format constants: sizes, counts, magics, versions, bounds and packed-field
//! widths.
//!
//! Every number here is part of the frozen v2 layout. Changing one is a format
//! change and needs a version bump, a new golden vector set and an N/N-1
//! decode lane entry in the same commit (round 3, section 8.4).

/// Major version of the on-disk format. A reader rejects any other major, and
/// a reader that meets a *higher* major in the superblock ring stops instead
/// of mounting an older commit (F7).
pub const FORMAT_MAJOR: u8 = 2;
/// Minor version of the on-disk format. A later minor may only give meaning to
/// bytes that this minor writes as zero; readers accept any minor of their own
/// major.
pub const FORMAT_MINOR: u8 = 3;
/// The `ver` field written into every on-disk structure: `major << 8 | minor`.
pub const FORMAT_VERSION: u16 = ((FORMAT_MAJOR as u16) << 8) | FORMAT_MINOR as u16;

/// Major version of the replication stream frame.
pub const STREAM_MAJOR: u8 = 1;
/// Minor version of the replication stream frame.
pub const STREAM_MINOR: u8 = 0;
/// The `ver` field written into every stream frame header.
pub const STREAM_VERSION: u16 = ((STREAM_MAJOR as u16) << 8) | STREAM_MINOR as u16;

/// The unit of every logical and physical block address: 4 KiB.
pub const BLOCK_SIZE: usize = 4096;
/// `log2(BLOCK_SIZE)`.
pub const BLOCK_SHIFT: u32 = 12;
/// The unit of `MapEntry::plen`: a 512-byte sector.
pub const SECTOR_SIZE: usize = 512;
/// Sectors in one block, and the largest physical length format v2 writes.
pub const SECTORS_PER_BLOCK: u16 = (BLOCK_SIZE / SECTOR_SIZE) as u16;
/// CRC-32C of an all-zero 4 KiB block: the CRC a hole descriptor carries,
/// because a hole's data slot is present and zero (F9).
pub const ZERO_BLOCK_CRC32C: u32 = 0x98f9_4189;

/// The highest CP sequence format v2 can record: `birth_cp` is 32 bits (F5).
/// CP 0 is the mkfs commit, so every written block has `birth_cp >= 1`.
pub const MAX_CP_SEQ: u64 = u32::MAX as u64;

/// Size of the superblock page.
pub const SUPERBLOCK_SIZE: usize = 4096;
/// Slots in the superblock ring: `A0`, `A1`, `B0`, `B1`.
pub const SUPERBLOCK_SLOTS: usize = 4;
/// Bytes of the superblock page that precede the trailing CRC.
pub const SUPERBLOCK_CRC_OFFSET: usize = SUPERBLOCK_SIZE - 4;
/// Reserved bytes at the end of the superblock page, written as zero and
/// covered by the CRC.
pub const SUPERBLOCK_RESERVED: usize = 2332;

/// Volumes per aggregate. A volume index is below this bound everywhere: in
/// the bitsets, the volume table, descriptors and hole records (F8).
pub const MAX_VOLS: usize = 1024;
/// The largest volume index.
pub const MAX_VOL: u32 = MAX_VOLS as u32 - 1;
/// 64-bit words in a [`crate::VolBitSet`].
pub const VOL_BITSET_WORDS: usize = MAX_VOLS / 64;
/// Bytes in a [`crate::VolBitSet`].
pub const VOL_BITSET_BYTES: usize = VOL_BITSET_WORDS * 8;
/// Inbound consistency-group slots reserved in the superblock.
pub const MAX_GROUPS: usize = 16;

/// Size of one [`crate::VolState`] record.
pub const VOL_STATE_SIZE: usize = 16;
/// Size of the volume table that follows the superblock page in a slot.
pub const VOL_TABLE_SIZE: usize = VOL_STATE_SIZE * MAX_VOLS;
/// Size of one whole superblock slot: the page plus the volume table. This is
/// the unit written with FUA (one full stripe on parity pools, R26).
pub const SUPERBLOCK_SLOT_SIZE: usize = SUPERBLOCK_SIZE + VOL_TABLE_SIZE;

/// Intent slots (two, alternating by `intent_seq` parity).
pub const INTENT_SLOTS: usize = 2;
/// Size of one intent slot page.
pub const INTENT_SLOT_SIZE: usize = 4096;
/// Bytes of the intent slot that precede the trailing CRC.
pub const INTENT_CRC_OFFSET: usize = INTENT_SLOT_SIZE - 4;
/// Reserved bytes at the end of the intent slot page.
pub const INTENT_RESERVED: usize = 3916;

/// Size of one [`crate::InboundState`] record.
pub const INBOUND_STATE_SIZE: usize = 72;
/// Size of the [`crate::DurableLogHdr`] reservation.
pub const DURABLE_LOG_HDR_SIZE: usize = 64;
/// Size of one [`crate::RootRef`].
pub const ROOT_REF_SIZE: usize = 16;

/// Size of the stripe summary header.
pub const STRIPE_HDR_SIZE: usize = 64;
/// Bytes of the stripe header that precede `hdr_crc`.
pub const STRIPE_HDR_CRC_OFFSET: usize = STRIPE_HDR_SIZE - 4;
/// Size of one per-block stripe descriptor.
pub const STRIPE_DESCRIPTOR_SIZE: usize = 16;
/// Fixed part of a stripe summary: the header and the per-stripe volume bitmap.
pub const STRIPE_SUMMARY_FIXED: usize = STRIPE_HDR_SIZE + VOL_BITSET_BYTES;
/// Byte offset of the volume bitmap inside a stripe summary.
pub const STRIPE_VOL_BITMAP_OFFSET: usize = STRIPE_HDR_SIZE;
/// Byte offset of the first descriptor inside a stripe summary.
pub const STRIPE_DESCRIPTORS_OFFSET: usize = STRIPE_SUMMARY_FIXED;
/// The smallest stripe: one summary page and one data block.
pub const MIN_STRIPE_BYTES: usize = 2 * BLOCK_SIZE;
/// The largest stripe: the top of the section 1.3 probe range, and the size
/// of the stream's receive and bounce buffers.
pub const MAX_STRIPE_BYTES: usize = 8 * 1024 * 1024;

/// Size of one map entry.
pub const MAP_ENTRY_SIZE: usize = 16;
/// Entries in one map page: one per 4 KiB block, so one page covers 4 MiB.
pub const MAP_ENTRIES_PER_PAGE: usize = 1024;
/// Size of one map page.
pub const MAP_PAGE_SIZE: usize = MAP_ENTRY_SIZE * MAP_ENTRIES_PER_PAGE;
/// `log2(MAP_ENTRIES_PER_PAGE)`: shift from an LBA to its map page number.
pub const MAP_PAGE_SHIFT: u32 = 10;
/// Size of one page-index entry.
pub const INDEX_ENTRY_SIZE: usize = 16;
/// Entries in one page-index page: one per map page, so one covers 4 GiB.
pub const INDEX_ENTRIES_PER_PAGE: usize = 1024;
/// Size of one page-index page.
pub const INDEX_PAGE_SIZE: usize = INDEX_ENTRY_SIZE * INDEX_ENTRIES_PER_PAGE;
/// `log2(INDEX_ENTRIES_PER_PAGE)`: shift from a map page number to its index page.
pub const INDEX_PAGE_SHIFT: u32 = 10;
/// Size of one per-volume map root record.
pub const MAP_ROOT_SIZE: usize = 64;

/// Size of one segment table entry.
pub const SEG_ENTRY_SIZE: usize = 32;
/// Physical segment number 0 is never allocated: it marks "no segment".
pub const NULL_PSEG: u64 = 0;
/// The largest segment: `MAX_SEG_OFF + 1` blocks, 256 MiB.
pub const MAX_SEG_BYTES: u64 = (MAX_SEG_OFF as u64 + 1) * BLOCK_SIZE as u64;
/// Size of one snapshot catalogue record.
pub const SNAP_RECORD_SIZE: usize = 64;
/// Size of one name catalogue record (format 2.3, F16).
pub const CATALOG_RECORD_SIZE: usize = 512;
/// The longest name a catalogue record holds, in bytes of UTF-8: the bound
/// the stream's `VolEvent` already carries.
pub const CATALOG_NAME_MAX: usize = 255;
/// Reserved bytes at the end of a catalogue record.
pub const CATALOG_RESERVED: usize = 221;
/// Size of one entry of a consistency point's stripe list (format 2.3, F17).
pub const CP_STRIPE_SIZE: usize = 16;
/// Size of one CP log record.
pub const CP_LOG_REC_SIZE: usize = 32;
/// Size of the CP log page header.
pub const CP_LOG_PAGE_HDR_SIZE: usize = 64;
/// Size of one CP log page.
pub const CP_LOG_PAGE_SIZE: usize = 4096;
/// Records in one CP log page.
pub const CP_LOG_RECS_PER_PAGE: usize = (CP_LOG_PAGE_SIZE - CP_LOG_PAGE_HDR_SIZE) / CP_LOG_REC_SIZE;
/// Size of one CP log directory page (format 2.1, F12).
pub const CP_LOG_DIR_PAGE_SIZE: usize = 4096;
/// Size of the CP log directory page header.
pub const CP_LOG_DIR_HDR_SIZE: usize = 64;
/// Root references in one CP log directory page.
pub const CP_LOG_DIR_ENTRIES: usize = (CP_LOG_DIR_PAGE_SIZE - CP_LOG_DIR_HDR_SIZE) / ROOT_REF_SIZE;
/// Size of one snapshot root page (format 2.1, F13).
pub const SNAP_ROOT_PAGE_SIZE: usize = 4096;
/// Bytes of the snapshot root page that precede the trailing CRC.
pub const SNAP_ROOT_CRC_OFFSET: usize = SNAP_ROOT_PAGE_SIZE - 4;
/// Reserved bytes at the end of the snapshot root page.
pub const SNAP_ROOT_RESERVED: usize = 3972;

/// Size of the stream frame header.
pub const STREAM_HEADER_SIZE: usize = 64;
/// Bytes of the stream header that precede `hdr_crc`.
pub const STREAM_HDR_CRC_OFFSET: usize = STREAM_HEADER_SIZE - 4;
/// Largest payload a single frame may carry, on the wire and after decoding:
/// one stripe image, which is at most [`MAX_STRIPE_BYTES`] and includes its
/// summary. `Holes` and `VolEvent` payloads are split below it.
pub const MAX_WIRE_PAYLOAD: u32 = MAX_STRIPE_BYTES as u32;
/// Size of one hole record in a `Holes` frame.
pub const HOLE_RECORD_SIZE: usize = 16;

/// Superblock page magic, the ASCII bytes `OTSB` read as a little-endian `u32`.
/// The magic at offset 0 and `ver` at offset 4 never move across majors.
pub const SUPERBLOCK_MAGIC: u32 = u32::from_le_bytes(*b"OTSB");
/// Intent slot magic: `OTIN`.
pub const INTENT_MAGIC: u32 = u32::from_le_bytes(*b"OTIN");
/// Stripe summary header magic: `OTST`.
pub const STRIPE_MAGIC: u32 = u32::from_le_bytes(*b"OTST");
/// Stream frame header magic: `OTSF`.
pub const STREAM_MAGIC: u32 = u32::from_le_bytes(*b"OTSF");
/// CP log page header magic: `OTCL`.
pub const CPLOG_MAGIC: u32 = u32::from_le_bytes(*b"OTCL");
/// CP log directory page magic: `OTCD` (format 2.1).
pub const CPDIR_MAGIC: u32 = u32::from_le_bytes(*b"OTCD");
/// Snapshot root page magic: `OTSR` (format 2.1).
pub const SNAPROOT_MAGIC: u32 = u32::from_le_bytes(*b"OTSR");

/// Bits of the volume index field in a stripe descriptor. The field is wider
/// than [`MAX_VOL`] needs; only values below [`MAX_VOLS`] are valid.
pub const VOL_BITS: u32 = 20;
/// Bits of the logical block address in a stripe descriptor.
pub const LBA_BITS: u32 = 40;
/// Bits of the block kind in a stripe descriptor.
pub const BLOCK_KIND_BITS: u32 = 2;
/// The largest value the 20-bit volume field can hold (not a valid index).
pub const MAX_VOL_ID: u32 = (1 << VOL_BITS) - 1;
/// Largest logical block address a packed descriptor can carry.
pub const MAX_LBA: u64 = (1 << LBA_BITS) - 1;

/// Bits of the logical segment number in a map address (F2).
pub const LSEG_BITS: u32 = 36;
/// Bits of the block offset within a segment in a map address.
pub const OFF_BITS: u32 = 16;
/// Bits of the physical length in a map address.
pub const PLEN_BITS: u32 = 12;
/// Largest logical segment number.
pub const MAX_LSEG: u64 = (1 << LSEG_BITS) - 1;
/// Largest block offset within a segment (segments are at most 256 MiB).
pub const MAX_SEG_OFF: u32 = (1 << OFF_BITS) - 1;
/// The largest value the 12-bit physical-length field can hold. Format v2
/// writes `1..=SECTORS_PER_BLOCK`; the rest of the range is the compression
/// reservation.
pub const MAX_PLEN: u16 = (1 << PLEN_BITS) - 1;
/// Logical segment number 0 is never allocated: it marks "no physical block".
pub const NULL_LSEG: u64 = 0;

const _: () = {
    assert!(FORMAT_VERSION == 0x0203);
    assert!(STREAM_VERSION == 0x0100);
    assert!(VOL_BITSET_BYTES == 128);
    assert!(VOL_TABLE_SIZE == 16384);
    assert!(SUPERBLOCK_SLOT_SIZE == 20480);
    assert!(MAP_PAGE_SIZE == 16384);
    assert!(INDEX_PAGE_SIZE == 16384);
    assert!(STRIPE_SUMMARY_FIXED == 192);
    assert!(VOL_BITS + LBA_BITS + BLOCK_KIND_BITS + 2 == 64);
    assert!(LSEG_BITS + OFF_BITS + PLEN_BITS == 64);
    assert!(SECTORS_PER_BLOCK == 8);
    assert!((MAX_VOLS as u32) - 1 == MAX_VOL);
    assert!(MAX_VOL < MAX_VOL_ID);
    assert!(MAX_WIRE_PAYLOAD as usize == MAX_STRIPE_BYTES);
    assert!(MIN_STRIPE_BYTES < MAX_STRIPE_BYTES);
    assert!(CP_LOG_RECS_PER_PAGE == 126);
    assert!(CP_LOG_DIR_ENTRIES == 252);
    assert!(CP_LOG_DIR_HDR_SIZE + CP_LOG_DIR_ENTRIES * ROOT_REF_SIZE == CP_LOG_DIR_PAGE_SIZE);
    assert!(SNAP_ROOT_RESERVED + 120 + 4 == SNAP_ROOT_PAGE_SIZE);
    assert!(MAX_SEG_BYTES == 256 * 1024 * 1024);
    assert!(32 + CATALOG_NAME_MAX + CATALOG_RESERVED + 4 == CATALOG_RECORD_SIZE);
};
