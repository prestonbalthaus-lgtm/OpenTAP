//! The per-volume two-level radix map: map roots, page-index pages and map
//! pages (section 1.3, "Map, stable block identity, holes, `diff`").
//!
//! ```text
//! MapRoot (64 B, in the map-roots table the superblock points at)
//!   -> page index: index_pages x PageIndexPage (16 KiB, 1024 PageIndexEntry)
//!        -> map pages: MapPage (16 KiB, 1024 MapEntry, one per 4 KiB LBA)
//! ```
//!
//! Checksums follow the "CRC in the parent" rule: a map page's CRC is in its
//! [`PageIndexEntry`] ([`MapPage::parse`] checks it), the page index's CRC is
//! in the [`MapRoot`] ([`MapRoot::verify_index`] checks it), and the
//! map-roots table's CRC is in the superblock's
//! [`RootRef`](crate::RootRef) ([`RootRef::verify`](crate::RootRef::verify)).
//! The pages themselves carry no header, so a page is exactly 1024 entries.
//!
//! An LBA `l` lives in map page `l >> 10` at slot `l & 1023`; map page `p`
//! is described by index entry `p` (index page `p >> 10`, slot `p & 1023`).
//!
//! # Entries (F2)
//! `addr` packs `lseg:36 | off:16 | plen:12`: the logical segment (resolved
//! through the segment table to a physical segment or a relocation table),
//! the 4 KiB block offset within it and the physical length in 512-byte
//! sectors, which format v2 writes as `1..=8`. `lseg == 0` is reserved, so
//! `addr == 0` names no physical block: with a non-zero birth the entry is a
//! hole (the hole-birth lesson), with a zero birth it was never written.
//! `crc32c` is the CRC of the 4 KiB block for data, checked on every read
//! against the block itself (which also catches misdirected writes), and 0
//! for holes and unmapped blocks.

use core::mem::{align_of, offset_of, size_of};

use zerocopy::{FromBytes, FromZeros, Immutable, IntoBytes, KnownLayout};

use crate::checksum::crc32c;
use crate::constants::{
    INDEX_ENTRIES_PER_PAGE, INDEX_ENTRY_SIZE, INDEX_PAGE_SHIFT, INDEX_PAGE_SIZE, LSEG_BITS,
    MAP_ENTRIES_PER_PAGE, MAP_ENTRY_SIZE, MAP_PAGE_SHIFT, MAP_PAGE_SIZE, MAP_ROOT_SIZE, MAX_CP_SEQ,
    MAX_LBA, MAX_LSEG, MAX_PLEN, MAX_SEG_OFF, MAX_VOL, NULL_LSEG, OFF_BITS, PLEN_BITS,
    SECTORS_PER_BLOCK,
};
use crate::types::{bad_field, check_crc, copy_out, view, FormatError};

/// Shift of the logical segment inside `addr`.
pub const MAP_LSEG_SHIFT: u32 = OFF_BITS + PLEN_BITS;
/// Shift of the block offset inside `addr`.
pub const MAP_OFF_SHIFT: u32 = PLEN_BITS;

/// The 16-byte map entry: one per 4 KiB logical block.
#[derive(
    FromBytes, IntoBytes, Immutable, KnownLayout, Clone, Copy, PartialEq, Eq, Hash, Default, Debug,
)]
#[repr(C)]
pub struct MapEntry {
    /// `lseg:36 | off:16 | plen:12`, or 0 for a hole or an unmapped block.
    pub addr: u64,
    /// The CP that wrote the block or punched the hole (F5); 0 only when unmapped.
    pub birth_cp: u32,
    /// CRC-32C of the 4 KiB block; 0 for holes and unmapped blocks.
    pub crc32c: u32,
}

impl MapEntry {
    /// A block that was never written: reads as zero, carries no birth.
    pub const UNMAPPED: Self = Self {
        addr: 0,
        birth_cp: 0,
        crc32c: 0,
    };

    /// Packs an address. `lseg` must be `1..=MAX_LSEG`, `off` at most
    /// [`MAX_SEG_OFF`] and `plen` `1..=SECTORS_PER_BLOCK`.
    pub const fn pack_addr(lseg: u64, off: u32, plen: u16) -> Result<u64, FormatError> {
        if lseg == NULL_LSEG {
            return Err(bad_field("lseg", 0));
        }
        if lseg > MAX_LSEG {
            return Err(FormatError::OutOfRange {
                field: "lseg",
                value: lseg,
                max: MAX_LSEG,
            });
        }
        if off > MAX_SEG_OFF {
            return Err(FormatError::OutOfRange {
                field: "off",
                value: off as u64,
                max: MAX_SEG_OFF as u64,
            });
        }
        if plen == 0 {
            return Err(bad_field("plen", 0));
        }
        if plen > SECTORS_PER_BLOCK {
            return Err(FormatError::OutOfRange {
                field: "plen",
                value: plen as u64,
                max: SECTORS_PER_BLOCK as u64,
            });
        }
        Ok((lseg << MAP_LSEG_SHIFT) | ((off as u64) << MAP_OFF_SHIFT) | plen as u64)
    }

    /// A data entry; `birth_cp` must be at least 1.
    pub const fn data(
        lseg: u64,
        off: u32,
        plen: u16,
        birth_cp: u32,
        crc32c: u32,
    ) -> Result<Self, FormatError> {
        let addr = match Self::pack_addr(lseg, off, plen) {
            Ok(addr) => addr,
            Err(e) => return Err(e),
        };
        if birth_cp == 0 {
            return Err(bad_field("birth_cp", 0));
        }
        Ok(Self {
            addr,
            birth_cp,
            crc32c,
        })
    }

    /// A hole punched at `birth_cp`, which must be at least 1 (an entry with
    /// a zero birth is [`UNMAPPED`](Self::UNMAPPED), not a hole).
    pub const fn hole(birth_cp: u32) -> Result<Self, FormatError> {
        if birth_cp == 0 {
            return Err(bad_field("birth_cp", 0));
        }
        Ok(Self {
            addr: 0,
            birth_cp,
            crc32c: 0,
        })
    }

    /// The logical segment number.
    #[must_use]
    pub const fn lseg(&self) -> u64 {
        self.addr >> MAP_LSEG_SHIFT
    }

    /// The block offset within the segment.
    #[must_use]
    pub const fn off(&self) -> u32 {
        ((self.addr >> MAP_OFF_SHIFT) & MAX_SEG_OFF as u64) as u32
    }

    /// The physical length in sectors.
    #[must_use]
    pub const fn plen(&self) -> u16 {
        (self.addr & MAX_PLEN as u64) as u16
    }

    /// `true` for a block that was never written.
    #[must_use]
    pub const fn is_unmapped(&self) -> bool {
        self.addr == 0 && self.birth_cp == 0
    }

    /// `true` for a punched hole.
    #[must_use]
    pub const fn is_hole(&self) -> bool {
        self.addr == 0 && self.birth_cp != 0
    }

    /// `true` for an entry that names a physical block.
    #[must_use]
    pub const fn is_data(&self) -> bool {
        self.addr != 0
    }

    /// Rejects a data entry with a zero segment, a zero or over-long physical
    /// length or a zero birth, and a hole or unmapped entry with a CRC.
    pub const fn validate(&self) -> Result<(), FormatError> {
        if self.is_data() {
            if self.lseg() == NULL_LSEG {
                return Err(bad_field("lseg", self.addr));
            }
            if self.plen() == 0 || self.plen() > SECTORS_PER_BLOCK {
                return Err(bad_field("plen", self.addr));
            }
            if self.birth_cp == 0 {
                return Err(bad_field("birth_cp", 0));
            }
        } else if self.crc32c != 0 {
            return Err(bad_field("crc32c", self.crc32c as u64));
        }
        Ok(())
    }
}

/// A 16 KiB map page: 1024 entries covering 4 MiB of logical address space.
#[derive(FromBytes, IntoBytes, Immutable, KnownLayout, Clone, Copy, PartialEq, Eq, Hash, Debug)]
#[repr(C, align(4096))]
pub struct MapPage {
    /// Entry `i` describes LBA `page_no * 1024 + i`.
    pub entries: [MapEntry; MAP_ENTRIES_PER_PAGE],
}

impl MapPage {
    /// A page of unmapped entries.
    #[must_use]
    pub fn zeroed() -> Self {
        Self::new_zeroed()
    }

    /// CRC-32C over the whole page, as stored in its [`PageIndexEntry`].
    #[must_use]
    pub fn crc32c(&self) -> u32 {
        crc32c(self.as_bytes())
    }

    /// The highest birth in the page, as stored in its [`PageIndexEntry`].
    #[must_use]
    pub fn max_birth(&self) -> u32 {
        self.entries.iter().map(|e| e.birth_cp).max().unwrap_or(0)
    }

    /// The index entry that describes this page when it is stored at `page_pba`.
    #[must_use]
    pub fn index_entry(&self, page_pba: u64) -> PageIndexEntry {
        PageIndexEntry::new(page_pba, self.max_birth(), self.crc32c())
    }

    /// The map page number of an LBA.
    #[must_use]
    pub const fn page_of(lba: u64) -> u64 {
        lba >> MAP_PAGE_SHIFT
    }

    /// The slot of an LBA within its map page.
    #[must_use]
    pub const fn slot_of(lba: u64) -> usize {
        (lba & (MAP_ENTRIES_PER_PAGE as u64 - 1)) as usize
    }

    /// The first LBA a map page covers.
    #[must_use]
    pub const fn first_lba(page_no: u64) -> u64 {
        page_no << MAP_PAGE_SHIFT
    }

    /// Validates every entry.
    pub fn validate(&self) -> Result<(), FormatError> {
        self.entries.iter().try_for_each(MapEntry::validate)
    }

    /// Checks a page against the index entry that points at it: the entry
    /// must name a page, the CRC and `max_birth` must match, and every entry
    /// must validate.
    pub fn verify(&self, entry: &PageIndexEntry) -> Result<(), FormatError> {
        if entry.is_none() {
            return Err(bad_field("page_pba", 0));
        }
        check_crc("map page", self.crc32c(), entry.page_crc)?;
        self.validate()?;
        if self.max_birth() != entry.max_birth {
            return Err(bad_field("max_birth", u64::from(entry.max_birth)));
        }
        Ok(())
    }

    /// A verified zero-copy view over a 16 KiB, 4096-aligned buffer.
    pub fn parse<'a>(bytes: &'a [u8], entry: &PageIndexEntry) -> Result<&'a Self, FormatError> {
        let page: &Self = view(bytes)?;
        page.verify(entry)?;
        Ok(page)
    }

    /// A verified copy out of a 16 KiB buffer of any alignment.
    pub fn read(bytes: &[u8], entry: &PageIndexEntry) -> Result<Self, FormatError> {
        let page: Self = copy_out(bytes)?;
        page.verify(entry)?;
        Ok(page)
    }
}

/// The 16-byte page-index entry: one per map page, 16 B per 4 MiB of volume.
#[derive(
    FromBytes, IntoBytes, Immutable, KnownLayout, Clone, Copy, PartialEq, Eq, Hash, Default, Debug,
)]
#[repr(C)]
pub struct PageIndexEntry {
    /// Physical block of the map page; 0 when the 4 MiB has no page (every
    /// block unmapped). PBA 0 holds superblock slot `A0`, never a map page.
    pub page_pba: u64,
    /// The highest birth in the page, so `diff(base)` skips pages with
    /// `max_birth <= base`.
    pub max_birth: u32,
    /// CRC-32C of the map page.
    pub page_crc: u32,
}

impl PageIndexEntry {
    /// The entry of a 4 MiB range with no page.
    pub const NONE: Self = Self {
        page_pba: 0,
        max_birth: 0,
        page_crc: 0,
    };

    /// An entry for a page at `page_pba`.
    #[must_use]
    pub const fn new(page_pba: u64, max_birth: u32, page_crc: u32) -> Self {
        Self {
            page_pba,
            max_birth,
            page_crc,
        }
    }

    /// `true` when the range has no page.
    #[must_use]
    pub const fn is_none(&self) -> bool {
        self.page_pba == 0
    }

    /// Rejects an absent entry that carries a birth or a CRC.
    pub const fn validate(&self) -> Result<(), FormatError> {
        if self.is_none() && (self.max_birth != 0 || self.page_crc != 0) {
            return Err(bad_field("page_pba", 0));
        }
        Ok(())
    }
}

/// A 16 KiB page-index page: 1024 entries covering 4 GiB of logical address space.
#[derive(FromBytes, IntoBytes, Immutable, KnownLayout, Clone, Copy, PartialEq, Eq, Hash, Debug)]
#[repr(C, align(4096))]
pub struct PageIndexPage {
    /// Entry `i` describes map page `index_page_no * 1024 + i`.
    pub entries: [PageIndexEntry; INDEX_ENTRIES_PER_PAGE],
}

impl PageIndexPage {
    /// A page of absent entries.
    #[must_use]
    pub fn zeroed() -> Self {
        Self::new_zeroed()
    }

    /// CRC-32C over the whole page. The page index's CRC in the [`MapRoot`]
    /// is over all of its pages in order; an engine folds per-page values
    /// with [`crc32c_combine`](crate::checksum::crc32c_combine).
    #[must_use]
    pub fn crc32c(&self) -> u32 {
        crc32c(self.as_bytes())
    }

    /// The highest `max_birth` in the page.
    #[must_use]
    pub fn max_birth(&self) -> u32 {
        self.entries.iter().map(|e| e.max_birth).max().unwrap_or(0)
    }

    /// The index page number of a map page.
    #[must_use]
    pub const fn index_page_of(map_page_no: u64) -> u64 {
        map_page_no >> INDEX_PAGE_SHIFT
    }

    /// The slot of a map page within its index page.
    #[must_use]
    pub const fn slot_of(map_page_no: u64) -> usize {
        (map_page_no & (INDEX_ENTRIES_PER_PAGE as u64 - 1)) as usize
    }

    /// Validates every entry.
    pub fn validate(&self) -> Result<(), FormatError> {
        self.entries.iter().try_for_each(PageIndexEntry::validate)
    }
}

/// The 64-byte per-volume map root, one per volume index in the map-roots
/// table the superblock points at.
#[derive(
    FromBytes, IntoBytes, Immutable, KnownLayout, Clone, Copy, PartialEq, Eq, Hash, Default, Debug,
)]
#[repr(C)]
pub struct MapRoot {
    /// The volume index, below [`MAX_VOLS`](crate::MAX_VOLS).
    pub vol: u32,
    /// Reserved flag bits, zero; ignored by readers.
    pub flags: u32,
    /// First physical block of the page index.
    pub index_pba: u64,
    /// Length of the page index in 16 KiB index pages.
    pub index_pages: u32,
    /// CRC-32C over the whole page index.
    pub index_crc: u32,
    /// The volume size in 4 KiB blocks, at most `MAX_LBA + 1`.
    pub lba_count: u64,
    /// The CP whose checkpoint wrote this root, at most [`MAX_CP_SEQ`].
    pub checkpoint_cp_seq: u64,
    /// The highest birth anywhere in the volume.
    pub max_birth: u32,
    /// Reserved, zero.
    pub rsvd0: u32,
    /// Reserved, zero.
    pub rsvd1: [u8; 16],
}

impl MapRoot {
    /// Map pages needed for `lba_count` blocks.
    #[must_use]
    pub const fn map_pages_for(lba_count: u64) -> u64 {
        lba_count.div_ceil(MAP_ENTRIES_PER_PAGE as u64)
    }

    /// Index pages needed for `lba_count` blocks.
    #[must_use]
    pub const fn index_pages_for(lba_count: u64) -> u64 {
        Self::map_pages_for(lba_count).div_ceil(INDEX_ENTRIES_PER_PAGE as u64)
    }

    /// Rejects a root whose volume index, size, CP or index length is out of
    /// range or inconsistent.
    pub const fn validate(&self) -> Result<(), FormatError> {
        if self.vol > MAX_VOL {
            return Err(FormatError::OutOfRange {
                field: "vol",
                value: self.vol as u64,
                max: MAX_VOL as u64,
            });
        }
        if self.lba_count > MAX_LBA + 1 {
            return Err(FormatError::OutOfRange {
                field: "lba_count",
                value: self.lba_count,
                max: MAX_LBA + 1,
            });
        }
        if self.checkpoint_cp_seq > MAX_CP_SEQ {
            return Err(FormatError::OutOfRange {
                field: "checkpoint_cp_seq",
                value: self.checkpoint_cp_seq,
                max: MAX_CP_SEQ,
            });
        }
        if self.index_pages as u64 != Self::index_pages_for(self.lba_count) {
            return Err(bad_field("index_pages", self.index_pages as u64));
        }
        Ok(())
    }

    /// Length of the page index in bytes.
    #[must_use]
    pub const fn index_bytes(&self) -> u64 {
        self.index_pages as u64 * INDEX_PAGE_SIZE as u64
    }

    /// Checks `index` against this root: the root validates, the length is
    /// `index_pages` index pages and the CRC matches, and every index entry
    /// validates. `index` must be 8-aligned.
    pub fn verify_index(&self, index: &[u8]) -> Result<(), FormatError> {
        self.validate()?;
        if index.len() as u64 != self.index_bytes() {
            return Err(FormatError::BadLength {
                expected: usize::try_from(self.index_bytes()).unwrap_or(usize::MAX),
                found: index.len(),
            });
        }
        check_crc("page index", crc32c(index), self.index_crc)?;
        let entries = <[PageIndexEntry]>::ref_from_bytes(index).map_err(|e| {
            crate::types::cast_error(e, index.len(), index.len(), align_of::<PageIndexEntry>())
        })?;
        entries.iter().try_for_each(PageIndexEntry::validate)
    }
}

const _: () = {
    assert!(size_of::<MapEntry>() == MAP_ENTRY_SIZE);
    assert!(align_of::<MapEntry>() == 8);
    assert!(offset_of!(MapEntry, addr) == 0);
    assert!(offset_of!(MapEntry, birth_cp) == 8);
    assert!(offset_of!(MapEntry, crc32c) == 12);

    assert!(size_of::<MapPage>() == MAP_PAGE_SIZE);
    assert!(align_of::<MapPage>() == 4096);
    assert!(offset_of!(MapPage, entries) == 0);

    assert!(size_of::<PageIndexEntry>() == INDEX_ENTRY_SIZE);
    assert!(align_of::<PageIndexEntry>() == 8);
    assert!(offset_of!(PageIndexEntry, page_pba) == 0);
    assert!(offset_of!(PageIndexEntry, max_birth) == 8);
    assert!(offset_of!(PageIndexEntry, page_crc) == 12);

    assert!(size_of::<PageIndexPage>() == INDEX_PAGE_SIZE);
    assert!(align_of::<PageIndexPage>() == 4096);
    assert!(offset_of!(PageIndexPage, entries) == 0);

    assert!(size_of::<MapRoot>() == MAP_ROOT_SIZE);
    assert!(align_of::<MapRoot>() == 8);
    assert!(offset_of!(MapRoot, vol) == 0);
    assert!(offset_of!(MapRoot, flags) == 4);
    assert!(offset_of!(MapRoot, index_pba) == 8);
    assert!(offset_of!(MapRoot, index_pages) == 16);
    assert!(offset_of!(MapRoot, index_crc) == 20);
    assert!(offset_of!(MapRoot, lba_count) == 24);
    assert!(offset_of!(MapRoot, checkpoint_cp_seq) == 32);
    assert!(offset_of!(MapRoot, max_birth) == 40);
    assert!(offset_of!(MapRoot, rsvd0) == 44);
    assert!(offset_of!(MapRoot, rsvd1) == 48);

    assert!(MAP_LSEG_SHIFT == 28);
    assert!(MAP_OFF_SHIFT == 12);
    assert!(MAP_LSEG_SHIFT + LSEG_BITS == 64);
};
