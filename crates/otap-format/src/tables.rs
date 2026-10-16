//! The incremental tables (format 2.4, F19 to F21): the snapshot table and
//! the name catalogue kept as a *base* region and a chain of *delta*
//! regions, named by a *head* page, so a consistency point writes only the
//! records it changed.
//!
//! # The pieces
//!
//! - A **base** region is the whole table as of the table's last fold, in
//!   the format 2.3 image: [`snap_table_image`] for the snapshot table,
//!   [`catalog_image`] for the catalogue, records only, no tombstone.
//! - A **delta** region is the changes one consistency point made: a run of
//!   [`TableLogPage`]s, each a 64-byte [`TableLogHdr`] naming the aggregate,
//!   the table, the CP, its nonce and the page's position, then up to 63
//!   snapshot records or 7 catalogue records. A record is a whole record
//!   (an upsert by key) or a tombstone ([`crate::SNAP_TOMBSTONE`],
//!   [`crate::CATALOG_TOMBSTONE`]); every record keeps its own CRC, so a
//!   damaged byte costs that record, not the region. The keys of one
//!   region strictly ascend (snapshot id; the catalogue's canonical order).
//! - The **head** is one 4 KiB [`TableHeadPage`]: the base region and the
//!   delta regions since the fold, oldest first, each as a
//!   [`TableChainEntry`] carrying the region's reference, the CP that wrote
//!   it and its record count, sealed by a trailing CRC. At most
//!   [`TABLE_HEAD_DELTAS`] deltas: a table whose chain is full is folded by
//!   the next CP that changes it, whatever the policy.
//!
//! The superblock's `snap_table` and `catalog` roots name the heads, and
//! `snap_head_cp` and `catalog_head_cp` the CPs that wrote them (2.4).
//!
//! # Replay (F21)
//!
//! The table at a CP is the base's records, then every delta's records in
//! chain order: an upsert replaces the record of its key, a tombstone
//! removes it. A region (head, base or delta) written by the commit being
//! booted that does not verify tears that commit, like every region the
//! commit wrote; one written by an older commit is damage: its readable
//! records are taken ([`scan_table_delta`], [`scan_table_base`]), and
//! every key whose last readable record precedes the damage is uncertain
//! (the engine drops such a name and quarantines such a snapshot with
//! [`crate::SNAP_UNCERTAIN`]), until the next fold rewrites the table.
//!
//! # A 2.3 reader meets a 2.4 pool
//!
//! A 2.3 reader takes the head behind `snap_table` for a snapshot table
//! image. Byte 56 of the head (a snapshot record's `kind`) is always zero,
//! so that reader refuses the table, finds every candidate of the ring torn
//! and refuses the pool before it writes anything, instead of rewriting
//! the superblock without the 2.4 fields. The engine keeps the snapshot
//! table's head (empty or not) whenever the pool holds a name or a
//! snapshot; a pool with neither has no head at all, and nothing a 2.3
//! reader could misread.

use core::fmt::Debug;
use core::hash::Hash;
use core::mem::{align_of, offset_of, size_of};

use zerocopy::{FromBytes, FromZeros, Immutable, IntoBytes, KnownLayout};

use crate::catalog::{catalog_image, parse_catalog, CatalogKind, CatalogRec};
use crate::checksum::crc32c;
use crate::constants::{
    BLOCK_SIZE, CATALOG_RECORD_SIZE, FORMAT_MAJOR, FORMAT_VERSION, MAX_CP_SEQ, SNAP_RECORD_SIZE,
    TABLE_CHAIN_ENTRY_SIZE, TABLE_HEAD_CRC_OFFSET, TABLE_HEAD_DELTAS, TABLE_HEAD_MAGIC,
    TABLE_HEAD_PAGE_SIZE, TABLE_HEAD_RESERVED, TABLE_LOG_HDR_SIZE, TABLE_LOG_MAGIC,
    TABLE_LOG_PAGE_SIZE, TABLE_LOG_PAYLOAD,
};
use crate::records::{parse_snap_table, snap_table_image, SnapRecord};
use crate::types::{
    bad_field, check_crc, check_magic, check_major, copy_out, view, FormatError, RootRef, Uuid,
};

/// Which table a head page or a delta region belongs to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[repr(u8)]
pub enum TableKind {
    /// The name catalogue (F16).
    Catalog = 1,
    /// The snapshot table (F14).
    Snapshots = 2,
}

impl TableKind {
    /// Decodes the on-disk byte.
    #[must_use]
    pub const fn from_u8(value: u8) -> Option<Self> {
        match value {
            1 => Some(Self::Catalog),
            2 => Some(Self::Snapshots),
            _ => None,
        }
    }

    /// The on-disk byte.
    #[must_use]
    pub const fn as_u8(self) -> u8 {
        self as u8
    }

    /// Records of this table one [`TableLogPage`] carries.
    #[must_use]
    pub const fn records_per_page(self) -> usize {
        match self {
            Self::Catalog => TABLE_LOG_PAYLOAD / CATALOG_RECORD_SIZE,
            Self::Snapshots => TABLE_LOG_PAYLOAD / SNAP_RECORD_SIZE,
        }
    }
}

/// A record one of the incremental tables holds: what a base image, a
/// delta region and a replay need to know about it.
pub trait TableRecord: Copy + Eq + Debug + FromBytes + IntoBytes + Immutable + KnownLayout {
    /// The table.
    const TABLE: TableKind;
    /// The record's size in bytes.
    const SIZE: usize;
    /// What a record is replaced and removed by.
    type Key: Ord + Copy + Debug + Hash;

    /// The record's key.
    fn key(&self) -> Self::Key;

    /// `true` for a tombstone.
    fn is_tombstone(&self) -> bool;

    /// A sealed tombstone for this record's key.
    fn tombstone_of(&self) -> Result<Self, FormatError>;

    /// Checks a record of a base region: not empty, not a tombstone, every
    /// field and the CRC.
    fn verify_base(&self) -> Result<(), FormatError>;

    /// Checks a record of a delta region: a record or a tombstone, and the
    /// CRC.
    fn verify_delta(&self) -> Result<(), FormatError>;

    /// The base image of `records` (canonical order, verified).
    fn base_image(records: &[Self]) -> Result<Vec<u8>, FormatError>;

    /// Parses a base image.
    fn parse_base(bytes: &[u8]) -> Result<Vec<Self>, FormatError>;
}

impl TableRecord for SnapRecord {
    const TABLE: TableKind = TableKind::Snapshots;
    const SIZE: usize = SNAP_RECORD_SIZE;
    type Key = u64;

    fn key(&self) -> u64 {
        self.snap_id
    }

    fn is_tombstone(&self) -> bool {
        SnapRecord::is_tombstone(self)
    }

    fn tombstone_of(&self) -> Result<Self, FormatError> {
        SnapRecord::tombstone(self.snap_id, self.snap_uuid, self.vol)
    }

    fn verify_base(&self) -> Result<(), FormatError> {
        if self.is_empty() {
            return Err(bad_field("snap_id", 0));
        }
        self.verify()
    }

    fn verify_delta(&self) -> Result<(), FormatError> {
        SnapRecord::verify_delta(self)
    }

    fn base_image(records: &[Self]) -> Result<Vec<u8>, FormatError> {
        snap_table_image(records)
    }

    fn parse_base(bytes: &[u8]) -> Result<Vec<Self>, FormatError> {
        parse_snap_table(bytes)
    }
}

impl TableRecord for CatalogRec {
    const TABLE: TableKind = TableKind::Catalog;
    const SIZE: usize = CATALOG_RECORD_SIZE;
    type Key = (u8, u32, [u8; 16]);

    fn key(&self) -> (u8, u32, [u8; 16]) {
        self.sort_key()
    }

    fn is_tombstone(&self) -> bool {
        CatalogRec::is_tombstone(self)
    }

    fn tombstone_of(&self) -> Result<Self, FormatError> {
        let kind = self
            .catalog_kind()
            .ok_or(bad_field("kind", u64::from(self.kind)))?;
        CatalogRec::tombstone(kind, self.vol, self.snap_uuid)
    }

    fn verify_base(&self) -> Result<(), FormatError> {
        if self.is_empty() {
            return Err(bad_field("kind", 0));
        }
        self.verify()
    }

    fn verify_delta(&self) -> Result<(), FormatError> {
        CatalogRec::verify_delta(self)
    }

    fn base_image(records: &[Self]) -> Result<Vec<u8>, FormatError> {
        catalog_image(records)
    }

    fn parse_base(bytes: &[u8]) -> Result<Vec<Self>, FormatError> {
        parse_catalog(bytes)
    }
}

/// The 64-byte header of a table log page (F19).
#[derive(FromBytes, IntoBytes, Immutable, KnownLayout, Clone, Copy, PartialEq, Eq, Hash, Debug)]
#[repr(C)]
pub struct TableLogHdr {
    /// [`TABLE_LOG_MAGIC`].
    pub magic: u32,
    /// [`FORMAT_VERSION`].
    pub ver: u16,
    /// Records used in this page, 1 to the table's records per page.
    pub count: u16,
    /// The CP that wrote the region, at most [`MAX_CP_SEQ`].
    pub cp_seq: u64,
    /// That CP's nonce.
    pub cp_nonce: u64,
    /// The aggregate.
    pub agg_id: Uuid,
    /// The page's position in its region, from 0.
    pub page_seq: u32,
    /// A [`TableKind`] byte.
    pub table: u8,
    /// Reserved, zero.
    pub rsvd0: [u8; 3],
    /// Reserved, zero.
    pub rsvd1: [u8; 12],
    /// CRC-32C over bytes `0..60`.
    pub crc32c: u32,
}

impl TableLogHdr {
    /// The CRC-32C over bytes `0..60`.
    #[must_use]
    pub fn compute_crc(&self) -> u32 {
        crc32c(&self.as_bytes()[..TABLE_LOG_HDR_SIZE - 4])
    }

    /// Stores the CRC.
    pub fn seal(&mut self) {
        self.crc32c = self.compute_crc();
    }

    /// The decoded table.
    #[must_use]
    pub const fn table_kind(&self) -> Option<TableKind> {
        TableKind::from_u8(self.table)
    }

    /// Checks magic, major version, CRC, the table, the CP bound and the
    /// count against the table's page capacity.
    pub fn verify(&self) -> Result<(), FormatError> {
        check_magic(self.magic, TABLE_LOG_MAGIC)?;
        check_major(self.ver, FORMAT_MAJOR)?;
        check_crc("table log header", self.compute_crc(), self.crc32c)?;
        let Some(table) = self.table_kind() else {
            return Err(bad_field("table", u64::from(self.table)));
        };
        if self.cp_seq > MAX_CP_SEQ {
            return Err(FormatError::OutOfRange {
                field: "cp_seq",
                value: self.cp_seq,
                max: MAX_CP_SEQ,
            });
        }
        if self.count == 0 || usize::from(self.count) > table.records_per_page() {
            return Err(FormatError::OutOfRange {
                field: "count",
                value: u64::from(self.count),
                max: table.records_per_page() as u64,
            });
        }
        Ok(())
    }
}

/// A 4 KiB table log page: a header and up to 63 snapshot records or 7
/// catalogue records, the rest zero.
#[derive(FromBytes, IntoBytes, Immutable, KnownLayout, Clone, Copy, PartialEq, Eq, Hash, Debug)]
#[repr(C, align(4096))]
pub struct TableLogPage {
    /// The header.
    pub hdr: TableLogHdr,
    /// The records, back to back from the start, then zeros.
    pub payload: [u8; TABLE_LOG_PAYLOAD],
}

impl TableLogPage {
    /// An empty page of `table` for the region CP `cp_seq` writes, at
    /// position `page_seq`.
    #[must_use]
    pub fn new(agg_id: Uuid, cp_seq: u64, cp_nonce: u64, page_seq: u32, table: TableKind) -> Self {
        let mut page = Self::new_zeroed();
        page.hdr.magic = TABLE_LOG_MAGIC;
        page.hdr.ver = FORMAT_VERSION;
        page.hdr.cp_seq = cp_seq;
        page.hdr.cp_nonce = cp_nonce;
        page.hdr.agg_id = agg_id;
        page.hdr.page_seq = page_seq;
        page.hdr.table = table.as_u8();
        page
    }

    /// Appends a record of the page's table; `false` when the page is full
    /// or of another table.
    pub fn push<R: TableRecord>(&mut self, rec: &R) -> bool {
        if self.hdr.table != R::TABLE.as_u8() {
            return false;
        }
        let at = usize::from(self.hdr.count);
        if at >= R::TABLE.records_per_page() {
            return false;
        }
        self.payload[at * R::SIZE..(at + 1) * R::SIZE].copy_from_slice(rec.as_bytes());
        self.hdr.count += 1;
        true
    }

    /// The record slot `i` of a page of `R`'s table, copied out unverified
    /// (`None` beyond the page).
    #[must_use]
    pub fn slot<R: TableRecord>(&self, i: usize) -> Option<R> {
        if i >= R::TABLE.records_per_page() {
            return None;
        }
        R::read_from_bytes(&self.payload[i * R::SIZE..(i + 1) * R::SIZE]).ok()
    }

    /// Stores the header CRC.
    pub fn seal(&mut self) {
        self.hdr.seal();
    }

    /// Verifies the header and that the bytes after the used records are
    /// zero (the records verify on their own).
    pub fn verify(&self) -> Result<(), FormatError> {
        self.hdr.verify()?;
        let table = self.hdr.table_kind().ok_or(bad_field("table", 0))?;
        let size = TABLE_LOG_PAYLOAD / table.records_per_page();
        let used = usize::from(self.hdr.count) * size;
        if self.payload[used..].iter().any(|b| *b != 0) {
            return Err(bad_field("count", u64::from(self.hdr.count)));
        }
        Ok(())
    }

    /// A verified zero-copy view over a 4 KiB, 4096-aligned buffer.
    pub fn parse(bytes: &[u8]) -> Result<&Self, FormatError> {
        let page: &Self = view(bytes)?;
        page.verify()?;
        Ok(page)
    }

    /// A verified copy out of a 4 KiB buffer of any alignment.
    pub fn read(bytes: &[u8]) -> Result<Self, FormatError> {
        let page: Self = copy_out(bytes)?;
        page.verify()?;
        Ok(page)
    }
}

/// A delta region as written: the CP, its nonce and the records in key
/// order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TableDelta<R> {
    /// The CP that wrote it.
    pub cp_seq: u64,
    /// Its nonce.
    pub cp_nonce: u64,
    /// The records and tombstones, keys strictly ascending.
    pub records: Vec<R>,
}

/// The delta region CP `cp_seq` writes for `records` (format 2.4, F19):
/// at least one record, each a valid delta record, keys strictly
/// ascending, packed into whole [`TableLogPage`]s.
pub fn table_delta_image<R: TableRecord>(
    agg_id: Uuid,
    cp_seq: u64,
    cp_nonce: u64,
    records: &[R],
) -> Result<Vec<u8>, FormatError> {
    if records.is_empty() {
        return Err(bad_field("count", 0));
    }
    if cp_seq > MAX_CP_SEQ {
        return Err(FormatError::OutOfRange {
            field: "cp_seq",
            value: cp_seq,
            max: MAX_CP_SEQ,
        });
    }
    let mut last: Option<R::Key> = None;
    for (i, rec) in records.iter().enumerate() {
        rec.verify_delta()?;
        if last.is_some_and(|l| rec.key() <= l) {
            return Err(bad_field("key", i as u64));
        }
        last = Some(rec.key());
    }
    let per_page = R::TABLE.records_per_page();
    let mut out = Vec::with_capacity(records.len().div_ceil(per_page) * TABLE_LOG_PAGE_SIZE);
    for (page_seq, chunk) in records.chunks(per_page).enumerate() {
        let page_seq = u32::try_from(page_seq).map_err(|_| bad_field("page_seq", u64::MAX))?;
        let mut page = TableLogPage::new(agg_id, cp_seq, cp_nonce, page_seq, R::TABLE);
        for rec in chunk {
            if !page.push(rec) {
                return Err(bad_field("count", u64::from(page.hdr.count)));
            }
        }
        page.seal();
        out.extend_from_slice(page.as_bytes());
    }
    Ok(out)
}

/// Parses a delta region of `R`'s table written for `agg_id`: every page
/// verifies, names the table, agrees with the first on the CP and its
/// nonce, is in position, and every page but the last is full; every
/// record is a valid delta record and the keys strictly ascend.
pub fn parse_table_delta<R: TableRecord>(
    bytes: &[u8],
    agg_id: &Uuid,
) -> Result<TableDelta<R>, FormatError> {
    if bytes.is_empty() || !bytes.len().is_multiple_of(TABLE_LOG_PAGE_SIZE) {
        return Err(FormatError::BadLength {
            expected: bytes.len().div_ceil(TABLE_LOG_PAGE_SIZE).max(1) * TABLE_LOG_PAGE_SIZE,
            found: bytes.len(),
        });
    }
    let pages = bytes.len() / TABLE_LOG_PAGE_SIZE;
    let per_page = R::TABLE.records_per_page();
    let mut first: Option<(u64, u64)> = None;
    let mut records = Vec::new();
    let mut last: Option<R::Key> = None;
    for (i, chunk) in bytes
        .as_chunks::<TABLE_LOG_PAGE_SIZE>()
        .0
        .iter()
        .enumerate()
    {
        let page = TableLogPage::read(chunk)?;
        if page.hdr.agg_id != *agg_id {
            return Err(FormatError::IdentityMismatch { field: "agg_id" });
        }
        if page.hdr.table != R::TABLE.as_u8() {
            return Err(FormatError::IdentityMismatch { field: "table" });
        }
        if page.hdr.page_seq as usize != i {
            return Err(FormatError::IdentityMismatch { field: "page_seq" });
        }
        match first {
            None => first = Some((page.hdr.cp_seq, page.hdr.cp_nonce)),
            Some(id) if id != (page.hdr.cp_seq, page.hdr.cp_nonce) => {
                return Err(FormatError::IdentityMismatch { field: "cp_seq" })
            }
            Some(_) => {}
        }
        if i + 1 < pages && usize::from(page.hdr.count) != per_page {
            return Err(bad_field("count", u64::from(page.hdr.count)));
        }
        for k in 0..usize::from(page.hdr.count) {
            let rec: R = page.slot(k).ok_or(bad_field("count", k as u64))?;
            rec.verify_delta()?;
            if last.is_some_and(|l| rec.key() <= l) {
                return Err(bad_field("key", (i * per_page + k) as u64));
            }
            last = Some(rec.key());
            records.push(rec);
        }
    }
    let (cp_seq, cp_nonce) = first.ok_or(FormatError::BadLength {
        expected: TABLE_LOG_PAGE_SIZE,
        found: 0,
    })?;
    Ok(TableDelta {
        cp_seq,
        cp_nonce,
        records,
    })
}

/// What a tolerant scan of a damaged region recovered.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TableScan<R> {
    /// The records that verify on their own, in the order found.
    pub records: Vec<R>,
    /// Page headers that do not verify, and record slots that hold
    /// something and do not verify or sit on such a page (their records
    /// are not believed). Zero for a region that is whole.
    pub damaged: usize,
    /// The first failure, for the report.
    pub problem: Option<FormatError>,
}

impl<R> TableScan<R> {
    fn note(&mut self, e: FormatError) {
        self.damaged += 1;
        if self.problem.is_none() {
            self.problem = Some(e);
        }
    }
}

/// Reads what can be believed of a delta region of `R`'s table, written by
/// CP `cp_seq`, that did not parse whole (F21): a page whose header
/// verifies, names the aggregate, the table and the CP and sits at its
/// position gives every record that verifies on its own; a page whose
/// header does not gives nothing, and each of its non-zero record slots
/// counts as damaged.
#[must_use]
pub fn scan_table_delta<R: TableRecord>(bytes: &[u8], agg_id: &Uuid, cp_seq: u64) -> TableScan<R> {
    let mut scan = TableScan {
        records: Vec::new(),
        damaged: 0,
        problem: None,
    };
    let per_page = R::TABLE.records_per_page();
    let (pages, tail) = bytes.as_chunks::<TABLE_LOG_PAGE_SIZE>();
    if !tail.is_empty() {
        scan.note(FormatError::BadLength {
            expected: bytes.len().div_ceil(TABLE_LOG_PAGE_SIZE) * TABLE_LOG_PAGE_SIZE,
            found: bytes.len(),
        });
    }
    for (i, chunk) in pages.iter().enumerate() {
        let page: TableLogPage = match copy_out(chunk) {
            Ok(page) => page,
            Err(e) => {
                scan.note(e);
                continue;
            }
        };
        let header = page.hdr.verify().and_then(|()| {
            if page.hdr.agg_id != *agg_id {
                Err(FormatError::IdentityMismatch { field: "agg_id" })
            } else if page.hdr.table != R::TABLE.as_u8() {
                Err(FormatError::IdentityMismatch { field: "table" })
            } else if page.hdr.page_seq as usize != i {
                Err(FormatError::IdentityMismatch { field: "page_seq" })
            } else if page.hdr.cp_seq != cp_seq {
                Err(FormatError::IdentityMismatch { field: "cp_seq" })
            } else {
                Ok(())
            }
        });
        let used = match header {
            Ok(()) => usize::from(page.hdr.count),
            Err(e) => {
                scan.note(e);
                0
            }
        };
        for k in 0..per_page {
            let raw = &page.payload[k * R::SIZE..(k + 1) * R::SIZE];
            if k >= used {
                if raw.iter().any(|b| *b != 0) {
                    scan.note(bad_field("count", k as u64));
                }
                continue;
            }
            match R::read_from_bytes(raw).map_err(|_| bad_field("count", k as u64)) {
                Ok(rec) => match rec.verify_delta() {
                    Ok(()) => scan.records.push(rec),
                    Err(e) => scan.note(e),
                },
                Err(e) => scan.note(e),
            }
        }
    }
    scan
}

/// Reads what can be believed of a base region that did not parse whole:
/// every record slot that holds something and verifies as a base record;
/// the others count as damaged.
#[must_use]
pub fn scan_table_base<R: TableRecord>(bytes: &[u8]) -> TableScan<R> {
    let mut scan = TableScan {
        records: Vec::new(),
        damaged: 0,
        problem: None,
    };
    let whole = bytes.len() / R::SIZE * R::SIZE;
    if whole != bytes.len() || !bytes.len().is_multiple_of(BLOCK_SIZE) {
        scan.note(FormatError::BadLength {
            expected: bytes.len().div_ceil(BLOCK_SIZE) * BLOCK_SIZE,
            found: bytes.len(),
        });
    }
    for raw in bytes[..whole].chunks(R::SIZE) {
        if raw.iter().all(|b| *b == 0) {
            continue;
        }
        match R::read_from_bytes(raw).map_err(|_| bad_field("record", 0)) {
            Ok(rec) => match rec.verify_base() {
                Ok(()) => scan.records.push(rec),
                Err(e) => scan.note(e),
            },
            Err(e) => scan.note(e),
        }
    }
    scan
}

/// One region of a table's chain as its head lists it (F20).
#[derive(
    FromBytes, IntoBytes, Immutable, KnownLayout, Clone, Copy, PartialEq, Eq, Hash, Default, Debug,
)]
#[repr(C)]
pub struct TableChainEntry {
    /// The region; [`RootRef::NONE`] for an empty base.
    pub region: RootRef,
    /// The CP that wrote the region.
    pub cp_seq: u64,
    /// Records in the region (tombstones included).
    pub records: u32,
    /// Reserved, zero.
    pub rsvd: u32,
}

impl TableChainEntry {
    /// The unused entry.
    pub const NONE: Self = Self {
        region: RootRef::NONE,
        cp_seq: 0,
        records: 0,
        rsvd: 0,
    };

    /// An entry for `region`, written by `cp_seq`, of `records` records.
    #[must_use]
    pub const fn new(region: RootRef, cp_seq: u64, records: u32) -> Self {
        Self {
            region,
            cp_seq,
            records,
            rsvd: 0,
        }
    }
}

/// The 4 KiB head page of a table (F20): the base region and the delta
/// regions since the table's last fold, sealed by a trailing CRC.
#[derive(FromBytes, IntoBytes, Immutable, KnownLayout, Clone, Copy, PartialEq, Eq, Hash, Debug)]
#[repr(C, align(4096))]
pub struct TableHeadPage {
    /// [`TABLE_HEAD_MAGIC`].
    pub magic: u32,
    /// [`FORMAT_VERSION`].
    pub ver: u16,
    /// A [`TableKind`] byte.
    pub table: u8,
    /// Reserved, zero.
    pub rsvd0: u8,
    /// The CP that wrote the head, at most [`MAX_CP_SEQ`].
    pub cp_seq: u64,
    /// The aggregate.
    pub agg_id: Uuid,
    /// Delta entries in use, at most [`TABLE_HEAD_DELTAS`].
    pub delta_count: u32,
    /// Reserved, zero. Byte 56 of the page, inside this field, is what a
    /// format 2.3 reader takes for a snapshot record's `kind`: zero, so
    /// that reader refuses the page.
    pub rsvd1: [u8; 28],
    /// The base region: the table as of `base.cp_seq`, its last fold.
    pub base: TableChainEntry,
    /// The delta regions since the fold, oldest first; unused entries are
    /// [`TableChainEntry::NONE`].
    pub deltas: [TableChainEntry; TABLE_HEAD_DELTAS],
    /// Reserved, zero.
    pub reserved: [u8; TABLE_HEAD_RESERVED],
    /// CRC-32C over bytes `0..4092`.
    pub crc32c: u32,
}

impl TableHeadPage {
    /// The CRC-32C over bytes `0..4092`.
    #[must_use]
    pub fn compute_crc(&self) -> u32 {
        crc32c(&self.as_bytes()[..TABLE_HEAD_CRC_OFFSET])
    }

    /// Stores the CRC.
    pub fn seal(&mut self) {
        self.crc32c = self.compute_crc();
    }

    /// The decoded table.
    #[must_use]
    pub const fn table_kind(&self) -> Option<TableKind> {
        TableKind::from_u8(self.table)
    }

    /// The head as a value (unverified).
    fn head(&self) -> Result<TableHead, FormatError> {
        let table = self
            .table_kind()
            .ok_or(bad_field("table", u64::from(self.table)))?;
        let count = self.delta_count as usize;
        if count > TABLE_HEAD_DELTAS {
            return Err(FormatError::OutOfRange {
                field: "delta_count",
                value: u64::from(self.delta_count),
                max: TABLE_HEAD_DELTAS as u64,
            });
        }
        Ok(TableHead {
            table,
            cp_seq: self.cp_seq,
            base: self.base,
            deltas: self.deltas[..count].to_vec(),
        })
    }

    /// Checks magic, major version, CRC, the table, the chain rules
    /// ([`TableHead::validate`]) and that the unused entries are zero.
    pub fn verify(&self) -> Result<(), FormatError> {
        check_magic(self.magic, TABLE_HEAD_MAGIC)?;
        check_major(self.ver, FORMAT_MAJOR)?;
        check_crc("table head", self.compute_crc(), self.crc32c)?;
        self.head()?.validate()?;
        if self.deltas[self.delta_count as usize..]
            .iter()
            .any(|e| *e != TableChainEntry::NONE)
        {
            return Err(bad_field("delta_count", u64::from(self.delta_count)));
        }
        Ok(())
    }

    /// A verified copy out of a 4 KiB buffer of any alignment.
    pub fn read(bytes: &[u8]) -> Result<Self, FormatError> {
        let page: Self = copy_out(bytes)?;
        page.verify()?;
        Ok(page)
    }
}

/// A table's head as a value: what [`TableHeadPage`] encodes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TableHead {
    /// The table.
    pub table: TableKind,
    /// The CP that writes (or wrote) the head.
    pub cp_seq: u64,
    /// The base region.
    pub base: TableChainEntry,
    /// The delta regions since the fold, oldest first.
    pub deltas: Vec<TableChainEntry>,
}

impl TableHead {
    /// Checks the chain (F20): the CP within bounds; a base written at or
    /// before the head, naming a region exactly when it holds records; at
    /// most [`TABLE_HEAD_DELTAS`] deltas, each naming a region at a
    /// non-zero block with at least one record, written by strictly
    /// ascending CPs after the base and at or before the head; reserved
    /// words zero.
    pub fn validate(&self) -> Result<(), FormatError> {
        if self.cp_seq > MAX_CP_SEQ {
            return Err(FormatError::OutOfRange {
                field: "cp_seq",
                value: self.cp_seq,
                max: MAX_CP_SEQ,
            });
        }
        if self.base.cp_seq > self.cp_seq {
            return Err(FormatError::OutOfRange {
                field: "base.cp_seq",
                value: self.base.cp_seq,
                max: self.cp_seq,
            });
        }
        if self.base.region.is_none() {
            if self.base.region != RootRef::NONE || self.base.records != 0 {
                return Err(bad_field("base.records", u64::from(self.base.records)));
            }
        } else if self.base.region.pba == 0 || self.base.records == 0 {
            return Err(bad_field("base.region", self.base.region.pba));
        }
        if self.base.rsvd != 0 {
            return Err(bad_field("base.rsvd", u64::from(self.base.rsvd)));
        }
        if self.deltas.len() > TABLE_HEAD_DELTAS {
            return Err(FormatError::OutOfRange {
                field: "delta_count",
                value: self.deltas.len() as u64,
                max: TABLE_HEAD_DELTAS as u64,
            });
        }
        let mut after = self.base.cp_seq;
        for (i, delta) in self.deltas.iter().enumerate() {
            if delta.region.is_none() || delta.region.pba == 0 || delta.records == 0 {
                return Err(bad_field("deltas", i as u64));
            }
            if delta.cp_seq <= after || delta.cp_seq > self.cp_seq {
                return Err(FormatError::OutOfRange {
                    field: "deltas.cp_seq",
                    value: delta.cp_seq,
                    max: self.cp_seq,
                });
            }
            if delta.rsvd != 0 {
                return Err(bad_field("deltas.rsvd", i as u64));
            }
            after = delta.cp_seq;
        }
        Ok(())
    }

    /// The sealed page for `agg_id`; refuses what
    /// [`validate`](Self::validate) refuses.
    pub fn page(&self, agg_id: Uuid) -> Result<TableHeadPage, FormatError> {
        self.validate()?;
        let mut page = TableHeadPage::new_zeroed();
        page.magic = TABLE_HEAD_MAGIC;
        page.ver = FORMAT_VERSION;
        page.table = self.table.as_u8();
        page.cp_seq = self.cp_seq;
        page.agg_id = agg_id;
        page.delta_count = self.deltas.len() as u32;
        page.base = self.base;
        page.deltas[..self.deltas.len()].copy_from_slice(&self.deltas);
        page.seal();
        Ok(page)
    }

    /// Decodes a head page of `table` written for `agg_id`: the page
    /// verifies and names both.
    pub fn decode(bytes: &[u8], agg_id: &Uuid, table: TableKind) -> Result<Self, FormatError> {
        let page = TableHeadPage::read(bytes)?;
        if page.agg_id != *agg_id {
            return Err(FormatError::IdentityMismatch { field: "agg_id" });
        }
        if page.table != table.as_u8() {
            return Err(FormatError::IdentityMismatch { field: "table" });
        }
        page.head()
    }

    /// Records in the base.
    #[must_use]
    pub fn base_records(&self) -> u64 {
        u64::from(self.base.records)
    }

    /// Records in the deltas.
    #[must_use]
    pub fn delta_records(&self) -> u64 {
        self.deltas.iter().map(|d| u64::from(d.records)).sum()
    }

    /// The regions the chain names: the base (when it has one), then the
    /// deltas.
    pub fn regions(&self) -> impl Iterator<Item = RootRef> + '_ {
        core::iter::once(self.base.region)
            .chain(self.deltas.iter().map(|d| d.region))
            .filter(|r| !r.is_none())
    }
}

/// The catalogue key of a snapshot's name: what the catalogue's records of
/// `vol`'s snapshot `snap_uuid` are replaced by.
#[must_use]
pub fn snapshot_name_key(vol: u32, snap_uuid: Uuid) -> (u8, u32, [u8; 16]) {
    (CatalogKind::Snapshot.as_u8(), vol, *snap_uuid.as_array())
}

const _: () = {
    assert!(TableKind::Catalog.as_u8() == 1 && TableKind::Snapshots.as_u8() == 2);
    assert!(matches!(TableKind::from_u8(1), Some(TableKind::Catalog)));
    assert!(matches!(TableKind::from_u8(2), Some(TableKind::Snapshots)));
    assert!(TableKind::from_u8(0).is_none() && TableKind::from_u8(3).is_none());
    assert!(TableKind::Snapshots.records_per_page() == 63);
    assert!(TableKind::Catalog.records_per_page() == 7);

    assert!(size_of::<TableLogHdr>() == TABLE_LOG_HDR_SIZE);
    assert!(align_of::<TableLogHdr>() == 8);
    assert!(offset_of!(TableLogHdr, magic) == 0);
    assert!(offset_of!(TableLogHdr, ver) == 4);
    assert!(offset_of!(TableLogHdr, count) == 6);
    assert!(offset_of!(TableLogHdr, cp_seq) == 8);
    assert!(offset_of!(TableLogHdr, cp_nonce) == 16);
    assert!(offset_of!(TableLogHdr, agg_id) == 24);
    assert!(offset_of!(TableLogHdr, page_seq) == 40);
    assert!(offset_of!(TableLogHdr, table) == 44);
    assert!(offset_of!(TableLogHdr, rsvd0) == 45);
    assert!(offset_of!(TableLogHdr, rsvd1) == 48);
    assert!(offset_of!(TableLogHdr, crc32c) == 60);
    assert!(size_of::<TableLogPage>() == TABLE_LOG_PAGE_SIZE);
    assert!(align_of::<TableLogPage>() == 4096);
    assert!(offset_of!(TableLogPage, payload) == TABLE_LOG_HDR_SIZE);

    assert!(size_of::<TableChainEntry>() == TABLE_CHAIN_ENTRY_SIZE);
    assert!(align_of::<TableChainEntry>() == 8);
    assert!(offset_of!(TableChainEntry, region) == 0);
    assert!(offset_of!(TableChainEntry, cp_seq) == 16);
    assert!(offset_of!(TableChainEntry, records) == 24);
    assert!(offset_of!(TableChainEntry, rsvd) == 28);

    assert!(size_of::<TableHeadPage>() == TABLE_HEAD_PAGE_SIZE);
    assert!(align_of::<TableHeadPage>() == 4096);
    assert!(offset_of!(TableHeadPage, magic) == 0);
    assert!(offset_of!(TableHeadPage, ver) == 4);
    assert!(offset_of!(TableHeadPage, table) == 6);
    assert!(offset_of!(TableHeadPage, rsvd0) == 7);
    assert!(offset_of!(TableHeadPage, cp_seq) == 8);
    assert!(offset_of!(TableHeadPage, agg_id) == 16);
    assert!(offset_of!(TableHeadPage, delta_count) == 32);
    assert!(offset_of!(TableHeadPage, rsvd1) == 36);
    // The byte a 2.3 reader takes for a snapshot record's `kind` is a
    // reserved byte of the head, always zero.
    assert!(offset_of!(TableHeadPage, rsvd1) < 56 && 56 < offset_of!(TableHeadPage, base));
    assert!(offset_of!(TableHeadPage, base) == 64);
    assert!(offset_of!(TableHeadPage, deltas) == 96);
    assert!(offset_of!(TableHeadPage, reserved) == 96 + TABLE_HEAD_DELTAS * TABLE_CHAIN_ENTRY_SIZE);
    assert!(offset_of!(TableHeadPage, crc32c) == TABLE_HEAD_CRC_OFFSET);
    assert!(SNAP_RECORD_SIZE == size_of::<SnapRecord>());
    assert!(CATALOG_RECORD_SIZE == size_of::<CatalogRec>());
};
