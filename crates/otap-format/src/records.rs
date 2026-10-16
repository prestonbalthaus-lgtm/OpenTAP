//! The deferred §7.1 records: the segment table entry, the snapshot catalogue
//! record, and the CP log record with its page (F11).
//!
//! # Segment table
//! The superblock's `seg_table` root points at a flat array of [`SegEntry`],
//! one per logical segment, indexed by `lseg` (so the entry carries no
//! `lseg` field of its own). An entry resolves a logical segment to its
//! physical location: `phys` is the physical segment number while the
//! segment holds data, and the PBA of its relocation table once the cleaner
//! has moved the live blocks out (R19). `stripe_data_bytes` is the full
//! stripe size the segment was born with, so a reshaped pool keeps the
//! geometry of older segments (R60). Physical segment 0, like logical
//! segment 0, is never allocated.
//!
//! # Snapshot catalogue
//! The `snap_table` root points at an array of [`SnapRecord`]s. A record
//! pins a volume's map root at a CP (`map_root_pba` is the page that holds
//! the pinned [`MapRoot`]) and names its deadlist. Names are in the
//! catalogue (F16), keyed by `snap_uuid`. An all-zero record is an empty
//! slot. Since format 2.4 the root names a head page and the table is a
//! base region plus delta regions ([`crate::tables`], F19 to F21): a delta
//! carries whole records and [tombstones](SnapRecord::tombstone)
//! ([`SNAP_TOMBSTONE`]), and a record recovered from a damaged table keeps
//! [`SNAP_UNCERTAIN`].
//!
//! # CP log
//! The CP log is the map's journal (section 1.3): every block written or
//! hole punched by a CP is one [`CpLogRec`], which carries exactly what the
//! map entry will hold, so replay is a copy into the map page. Records are
//! packed into [`CpLogPage`]s of 4 KiB, each with a header naming the
//! aggregate, the CP and the page's position in the log. Every record
//! carries its own CRC, so a torn page is cut at the first record that fails.

use core::mem::{align_of, offset_of, size_of};

use zerocopy::{FromBytes, FromZeros, Immutable, IntoBytes, KnownLayout};

use crate::checksum::crc32c;
use crate::constants::{
    BLOCK_SIZE, CPDIR_MAGIC, CPLOG_MAGIC, CP_LOG_DIR_ENTRIES, CP_LOG_DIR_HDR_SIZE,
    CP_LOG_DIR_PAGE_SIZE, CP_LOG_PAGE_HDR_SIZE, CP_LOG_PAGE_SIZE, CP_LOG_RECS_PER_PAGE,
    CP_LOG_REC_SIZE, CP_STRIPE_SIZE, FORMAT_MAJOR, FORMAT_VERSION, MAX_CP_SEQ, MAX_LBA,
    MAX_SEG_BYTES, MAX_STRIPE_BYTES, MAX_VOL, MIN_STRIPE_BYTES, NULL_PSEG, SEG_ENTRY_SIZE,
    SNAPROOT_MAGIC, SNAP_RECORD_SIZE, SNAP_ROOT_CRC_OFFSET, SNAP_ROOT_PAGE_SIZE,
    SNAP_ROOT_RESERVED,
};
use crate::map::{MapEntry, MapRoot};
use crate::types::{
    bad_field, check_crc, check_magic, check_major, copy_out, view, FormatError, RootRef, Uuid,
};

/// What a segment holds.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[repr(u8)]
pub enum SegKind {
    /// Data stripes.
    Data = 0,
    /// Log-structured metadata: CP log pages, map checkpoints, tables.
    Metadata = 1,
}

impl SegKind {
    /// Decodes the on-disk byte.
    #[must_use]
    pub const fn from_u8(value: u8) -> Option<Self> {
        match value {
            0 => Some(Self::Data),
            1 => Some(Self::Metadata),
            _ => None,
        }
    }

    /// The on-disk byte.
    #[must_use]
    pub const fn as_u8(self) -> u8 {
        self as u8
    }
}

/// Where a logical segment is in its life.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[repr(u8)]
pub enum SegState {
    /// Never allocated, or fully retired: no physical segment.
    Free = 0,
    /// Being written; `phys` is the physical segment.
    Open = 1,
    /// Full and immutable; `phys` is the physical segment.
    Sealed = 2,
    /// The cleaner moved every live block out; `phys` is the PBA of the
    /// relocation table and `live_bytes` is zero.
    Relocated = 3,
    /// Freed by a CP; the physical segment is reused only after the next CP
    /// (or the one after on `flush-opaque` backstores). Reserved for a
    /// persisted deferred-free list: the Step 4 engine keeps the deferral in
    /// memory and recomputes it at boot from the two slots' regions, and
    /// never writes this state.
    FreedPending = 4,
}

impl SegState {
    /// Decodes the on-disk byte.
    #[must_use]
    pub const fn from_u8(value: u8) -> Option<Self> {
        match value {
            0 => Some(Self::Free),
            1 => Some(Self::Open),
            2 => Some(Self::Sealed),
            3 => Some(Self::Relocated),
            4 => Some(Self::FreedPending),
            _ => None,
        }
    }

    /// The on-disk byte.
    #[must_use]
    pub const fn as_u8(self) -> u8 {
        self as u8
    }

    /// `true` when `phys` names a physical segment that holds data.
    #[must_use]
    pub const fn holds_data(self) -> bool {
        matches!(self, Self::Open | Self::Sealed | Self::FreedPending)
    }
}

/// The 32-byte segment table entry, indexed by logical segment number.
#[derive(
    FromBytes, IntoBytes, Immutable, KnownLayout, Clone, Copy, PartialEq, Eq, Hash, Default, Debug,
)]
#[repr(C)]
pub struct SegEntry {
    /// The physical segment (states that hold data) or the relocation
    /// table's PBA (`Relocated`); 0 when `Free`.
    pub phys: u64,
    /// The full stripe size at birth, in bytes: the unit of every write to
    /// the segment (R60).
    pub stripe_data_bytes: u32,
    /// Bytes still referenced by a live or pinned root, for the cleaner.
    pub live_bytes: u32,
    /// The CP that allocated the segment, at least 1 unless `Free`.
    pub birth_cp: u32,
    /// How many times the physical segment has been handed out, so a stale
    /// reference from an earlier lap never matches. Not yet advanced: the
    /// Step 4 engine frees a segment with its lap at zero, and recovery
    /// reads exact roots (E14), so nothing relies on it until relocation.
    pub lap: u32,
    /// A [`SegKind`] byte.
    pub kind: u8,
    /// A [`SegState`] byte.
    pub state: u8,
    /// Reserved, zero.
    pub rsvd: u16,
    /// CRC-32C over bytes `0..28`.
    pub crc32c: u32,
}

impl SegEntry {
    /// A free entry that was never allocated.
    pub const FREE: Self = Self {
        phys: 0,
        stripe_data_bytes: 0,
        live_bytes: 0,
        birth_cp: 0,
        lap: 0,
        kind: 0,
        state: 0,
        rsvd: 0,
        crc32c: 0,
    };

    /// A new, open segment on physical segment `pseg`, sealed.
    pub fn open(
        pseg: u64,
        stripe_data_bytes: u32,
        birth_cp: u32,
        lap: u32,
        kind: SegKind,
    ) -> Result<Self, FormatError> {
        let mut entry = Self {
            phys: pseg,
            stripe_data_bytes,
            live_bytes: 0,
            birth_cp,
            lap,
            kind: kind.as_u8(),
            state: SegState::Open.as_u8(),
            rsvd: 0,
            crc32c: 0,
        };
        entry.validate()?;
        entry.seal();
        Ok(entry)
    }

    /// The CRC-32C over bytes `0..28`.
    #[must_use]
    pub fn compute_crc(&self) -> u32 {
        crc32c(&self.as_bytes()[..SEG_ENTRY_SIZE - 4])
    }

    /// Stores the CRC. Call after the last field change.
    pub fn seal(&mut self) {
        self.crc32c = self.compute_crc();
    }

    /// The decoded kind.
    #[must_use]
    pub const fn seg_kind(&self) -> Option<SegKind> {
        SegKind::from_u8(self.kind)
    }

    /// The decoded state.
    #[must_use]
    pub const fn seg_state(&self) -> Option<SegState> {
        SegState::from_u8(self.state)
    }

    /// `true` when the entry is [`FREE`](Self::FREE) up to its `lap`.
    #[must_use]
    pub const fn is_free(&self) -> bool {
        self.state == SegState::Free.as_u8()
    }

    /// Checks every field against its state: a free entry is zero apart
    /// from `lap`; an allocated one names a physical segment or table, a
    /// stripe size in the probe range, a birth and a bounded live count.
    pub const fn validate(&self) -> Result<(), FormatError> {
        let Some(state) = self.seg_state() else {
            return Err(bad_field("state", self.state as u64));
        };
        if self.seg_kind().is_none() {
            return Err(bad_field("kind", self.kind as u64));
        }
        if matches!(state, SegState::Free) {
            if self.phys != 0
                || self.stripe_data_bytes != 0
                || self.live_bytes != 0
                || self.birth_cp != 0
            {
                return Err(bad_field("state", 0));
            }
            return Ok(());
        }
        if self.phys == NULL_PSEG {
            return Err(bad_field("phys", 0));
        }
        let stripe = self.stripe_data_bytes as usize;
        if !stripe.is_multiple_of(BLOCK_SIZE)
            || stripe < MIN_STRIPE_BYTES
            || stripe > MAX_STRIPE_BYTES
        {
            return Err(bad_field(
                "stripe_data_bytes",
                self.stripe_data_bytes as u64,
            ));
        }
        if self.birth_cp == 0 {
            return Err(bad_field("birth_cp", 0));
        }
        if self.live_bytes as u64 > MAX_SEG_BYTES
            || !(self.live_bytes as usize).is_multiple_of(BLOCK_SIZE)
        {
            return Err(bad_field("live_bytes", self.live_bytes as u64));
        }
        if matches!(state, SegState::Relocated) && self.live_bytes != 0 {
            return Err(bad_field("live_bytes", self.live_bytes as u64));
        }
        Ok(())
    }

    /// Validates the fields and checks the CRC. A free entry carries no CRC
    /// and verifies through its fields alone.
    pub fn verify(&self) -> Result<(), FormatError> {
        self.validate()?;
        if self.is_free() {
            return Ok(());
        }
        check_crc("segment entry", self.compute_crc(), self.crc32c)
    }
}

/// How a snapshot was taken.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[repr(u8)]
pub enum SnapKind {
    /// Taken under `FIFREEZE`: mountable read-only without replay.
    Fsfreeze = 1,
    /// Taken without a freeze: exposed only as a writable clone after a
    /// replay mount.
    Crash = 2,
}

impl SnapKind {
    /// Decodes the on-disk byte.
    #[must_use]
    pub const fn from_u8(value: u8) -> Option<Self> {
        match value {
            1 => Some(Self::Fsfreeze),
            2 => Some(Self::Crash),
            _ => None,
        }
    }

    /// The on-disk byte.
    #[must_use]
    pub const fn as_u8(self) -> u8 {
        self as u8
    }
}

/// Why a snapshot exists.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[repr(u8)]
pub enum SnapOrigin {
    /// A schedule.
    Scheduled = 1,
    /// An operator.
    User = 2,
    /// A replication transfer base.
    Transfer = 3,
    /// The divergent head kept after a failover (`divergent-<ts>`).
    Divergent = 4,
}

impl SnapOrigin {
    /// Decodes the on-disk byte.
    #[must_use]
    pub const fn from_u8(value: u8) -> Option<Self> {
        match value {
            1 => Some(Self::Scheduled),
            2 => Some(Self::User),
            3 => Some(Self::Transfer),
            4 => Some(Self::Divergent),
            _ => None,
        }
    }

    /// The on-disk byte.
    #[must_use]
    pub const fn as_u8(self) -> u8 {
        self as u8
    }
}

/// `SnapRecord::flags` bit: pinned by an operator.
pub const PIN_USER: u8 = 1 << 0;
/// `SnapRecord::flags` bit: pinned by a schedule's retention.
pub const PIN_SCHEDULE: u8 = 1 << 1;
/// `SnapRecord::flags` bit: pinned as a replication base.
pub const PIN_REPLICATION: u8 = 1 << 2;
/// `SnapRecord::flags` bit: pinned by a clone volume.
pub const PIN_CLONE: u8 = 1 << 3;
/// Every defined pin bit.
pub const PIN_MASK: u8 = PIN_USER | PIN_SCHEDULE | PIN_REPLICATION | PIN_CLONE;
/// `SnapRecord::flags` bit (format 2.4, F21): the record was recovered
/// from a snapshot table whose delta chain did not verify, so the last
/// change to the snapshot may have been lost (a pin, a relocation of its
/// tree, even its deletion). The snapshot is quarantined: listed, never
/// read, diffed or rolled back to, and its deletion releases nothing (its
/// blocks and pages may already belong to others). The bit is persisted
/// so the next boot keeps the quarantine.
pub const SNAP_UNCERTAIN: u8 = 1 << 6;
/// `SnapRecord::flags` bit (format 2.4, F19): in a delta region only, the
/// record is a tombstone: the snapshot `snap_id` (with its uuid and volume)
/// left the table. Every other field is zero. A base region refuses it.
pub const SNAP_TOMBSTONE: u8 = 1 << 7;
/// Every flag bit a snapshot record in a table base may carry.
pub const SNAP_FLAGS_MASK: u8 = PIN_MASK | SNAP_UNCERTAIN;

/// The 64-byte snapshot catalogue record.
#[derive(
    FromBytes, IntoBytes, Immutable, KnownLayout, Clone, Copy, PartialEq, Eq, Hash, Default, Debug,
)]
#[repr(C)]
pub struct SnapRecord {
    /// Monotonic per aggregate, at least 1; 0 marks an empty slot.
    pub snap_id: u64,
    /// The snapshot's identity, never nil.
    pub snap_uuid: Uuid,
    /// The page holding the pinned map root, at least 1.
    pub map_root_pba: u64,
    /// The deadlist region, or 0 for none.
    pub deadlist_pba: u64,
    /// Wall-clock time of the snapshot, nanoseconds since the Unix epoch.
    pub created_wall_ns: u64,
    /// The CP the snapshot pins, at least 1.
    pub birth_cp: u32,
    /// The volume, below [`MAX_VOLS`](crate::MAX_VOLS).
    pub vol: u32,
    /// A [`SnapKind`] byte.
    pub kind: u8,
    /// A [`SnapOrigin`] byte.
    pub origin: u8,
    /// The `PIN_*` bits; since format 2.4 also [`SNAP_UNCERTAIN`], and
    /// [`SNAP_TOMBSTONE`] in a delta region.
    pub flags: u8,
    /// Reserved, zero.
    pub rsvd: u8,
    /// CRC-32C over bytes `0..60`.
    pub crc32c: u32,
}

impl SnapRecord {
    /// An empty slot.
    pub const EMPTY: Self = Self {
        snap_id: 0,
        snap_uuid: Uuid::NIL,
        map_root_pba: 0,
        deadlist_pba: 0,
        created_wall_ns: 0,
        birth_cp: 0,
        vol: 0,
        kind: 0,
        origin: 0,
        flags: 0,
        rsvd: 0,
        crc32c: 0,
    };

    /// The CRC-32C over bytes `0..60`.
    #[must_use]
    pub fn compute_crc(&self) -> u32 {
        crc32c(&self.as_bytes()[..SNAP_RECORD_SIZE - 4])
    }

    /// Stores the CRC. Call after the last field change.
    pub fn seal(&mut self) {
        self.crc32c = self.compute_crc();
    }

    /// `true` for an empty slot.
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.snap_id == 0
    }

    /// The decoded kind.
    #[must_use]
    pub const fn snap_kind(&self) -> Option<SnapKind> {
        SnapKind::from_u8(self.kind)
    }

    /// The decoded origin.
    #[must_use]
    pub const fn snap_origin(&self) -> Option<SnapOrigin> {
        SnapOrigin::from_u8(self.origin)
    }

    /// The `PIN_*` bits alone.
    #[must_use]
    pub const fn pins(&self) -> u8 {
        self.flags & PIN_MASK
    }

    /// `true` for a record recovered from a damaged table
    /// ([`SNAP_UNCERTAIN`], format 2.4).
    #[must_use]
    pub const fn is_uncertain(&self) -> bool {
        self.flags & SNAP_UNCERTAIN != 0
    }

    /// `true` for a delta region's tombstone ([`SNAP_TOMBSTONE`]).
    #[must_use]
    pub const fn is_tombstone(&self) -> bool {
        self.flags & SNAP_TOMBSTONE != 0
    }

    /// A sealed tombstone (format 2.4, F19): snapshot `snap_id` of `vol`,
    /// identity `snap_uuid`, left the table. Refused for an empty id, a nil
    /// uuid or a volume out of range.
    pub fn tombstone(snap_id: u64, snap_uuid: Uuid, vol: u32) -> Result<Self, FormatError> {
        let mut rec = Self::EMPTY;
        rec.snap_id = snap_id;
        rec.snap_uuid = snap_uuid;
        rec.vol = vol;
        rec.flags = SNAP_TOMBSTONE;
        rec.validate_delta()?;
        rec.seal();
        Ok(rec)
    }

    /// Checks the fields: an empty slot is all zero; a record has a defined
    /// kind and origin, known flag bits ([`SNAP_FLAGS_MASK`]: a tombstone
    /// is refused), a volume in range, a birth, a non-nil uuid and a map
    /// root.
    pub fn validate(&self) -> Result<(), FormatError> {
        if self.is_empty() {
            if *self != Self::EMPTY {
                return Err(bad_field("snap_id", 0));
            }
            return Ok(());
        }
        if self.snap_kind().is_none() {
            return Err(bad_field("kind", u64::from(self.kind)));
        }
        if self.snap_origin().is_none() {
            return Err(bad_field("origin", u64::from(self.origin)));
        }
        if self.flags & !SNAP_FLAGS_MASK != 0 {
            return Err(bad_field("flags", u64::from(self.flags)));
        }
        if self.vol > MAX_VOL {
            return Err(FormatError::OutOfRange {
                field: "vol",
                value: u64::from(self.vol),
                max: u64::from(MAX_VOL),
            });
        }
        if self.birth_cp == 0 {
            return Err(bad_field("birth_cp", 0));
        }
        if self.snap_uuid.is_nil() {
            return Err(bad_field("snap_uuid", 0));
        }
        if self.map_root_pba == 0 {
            return Err(bad_field("map_root_pba", 0));
        }
        Ok(())
    }

    /// Validates the fields and checks the CRC (an empty slot has none).
    pub fn verify(&self) -> Result<(), FormatError> {
        self.validate()?;
        if self.is_empty() {
            return Ok(());
        }
        check_crc("snapshot record", self.compute_crc(), self.crc32c)
    }

    /// Checks a record of a delta region (format 2.4, F19): never empty;
    /// either a record [`validate`](Self::validate) accepts, or a tombstone
    /// whose id, uuid and volume are set and whose every other field
    /// (`rsvd` aside, which readers ignore) is zero.
    pub fn validate_delta(&self) -> Result<(), FormatError> {
        if self.is_empty() {
            return Err(bad_field("snap_id", 0));
        }
        if !self.is_tombstone() {
            return self.validate();
        }
        if self.flags != SNAP_TOMBSTONE {
            return Err(bad_field("flags", u64::from(self.flags)));
        }
        if self.snap_uuid.is_nil() {
            return Err(bad_field("snap_uuid", 0));
        }
        if self.vol > MAX_VOL {
            return Err(FormatError::OutOfRange {
                field: "vol",
                value: u64::from(self.vol),
                max: u64::from(MAX_VOL),
            });
        }
        if self.map_root_pba != 0 {
            return Err(bad_field("map_root_pba", self.map_root_pba));
        }
        if self.deadlist_pba != 0 {
            return Err(bad_field("deadlist_pba", self.deadlist_pba));
        }
        if self.created_wall_ns != 0 {
            return Err(bad_field("created_wall_ns", self.created_wall_ns));
        }
        if self.birth_cp != 0 {
            return Err(bad_field("birth_cp", u64::from(self.birth_cp)));
        }
        if self.kind != 0 {
            return Err(bad_field("kind", u64::from(self.kind)));
        }
        if self.origin != 0 {
            return Err(bad_field("origin", u64::from(self.origin)));
        }
        Ok(())
    }

    /// [`validate_delta`](Self::validate_delta) and the CRC.
    pub fn verify_delta(&self) -> Result<(), FormatError> {
        self.validate_delta()?;
        check_crc("snapshot record", self.compute_crc(), self.crc32c)
    }
}

/// The 32-byte CP log record: one logical change, carrying exactly what the
/// map entry will hold.
#[derive(
    FromBytes, IntoBytes, Immutable, KnownLayout, Clone, Copy, PartialEq, Eq, Hash, Default, Debug,
)]
#[repr(C)]
pub struct CpLogRec {
    /// The logical block address, at most [`MAX_LBA`].
    pub lba: u64,
    /// The packed map address (`lseg:36 | off:16 | plen:12`), or 0 for a hole.
    pub addr: u64,
    /// The volume, below [`MAX_VOLS`](crate::MAX_VOLS).
    pub vol: u32,
    /// The CP that wrote the block or punched the hole, at least 1.
    pub birth_cp: u32,
    /// CRC-32C of the 4 KiB block; 0 for a hole.
    pub block_crc32c: u32,
    /// CRC-32C over bytes `0..28` of this record.
    pub crc32c: u32,
}

impl CpLogRec {
    /// A record from the map entry it will become, sealed.
    pub fn from_entry(vol: u32, lba: u64, entry: MapEntry) -> Result<Self, FormatError> {
        let mut rec = Self {
            lba,
            addr: entry.addr,
            vol,
            birth_cp: entry.birth_cp,
            block_crc32c: entry.crc32c,
            crc32c: 0,
        };
        rec.validate()?;
        rec.seal();
        Ok(rec)
    }

    /// A data record, sealed.
    pub fn data(
        vol: u32,
        lba: u64,
        lseg: u64,
        off: u32,
        plen: u16,
        birth_cp: u32,
        block_crc32c: u32,
    ) -> Result<Self, FormatError> {
        Self::from_entry(
            vol,
            lba,
            MapEntry::data(lseg, off, plen, birth_cp, block_crc32c)?,
        )
    }

    /// A hole record, sealed.
    pub fn hole(vol: u32, lba: u64, birth_cp: u32) -> Result<Self, FormatError> {
        Self::from_entry(vol, lba, MapEntry::hole(birth_cp)?)
    }

    /// The map entry this record replays to.
    #[must_use]
    pub const fn map_entry(&self) -> MapEntry {
        MapEntry {
            addr: self.addr,
            birth_cp: self.birth_cp,
            crc32c: self.block_crc32c,
        }
    }

    /// The CRC-32C over bytes `0..28`.
    #[must_use]
    pub fn compute_crc(&self) -> u32 {
        crc32c(&self.as_bytes()[..CP_LOG_REC_SIZE - 4])
    }

    /// Stores the CRC. Call after the last field change.
    pub fn seal(&mut self) {
        self.crc32c = self.compute_crc();
    }

    /// Checks the fields: volume and LBA in range, and a map entry that is a
    /// data block or a hole (never unmapped: a CP log never records
    /// "nothing happened").
    pub const fn validate(&self) -> Result<(), FormatError> {
        if self.vol > MAX_VOL {
            return Err(FormatError::OutOfRange {
                field: "vol",
                value: self.vol as u64,
                max: MAX_VOL as u64,
            });
        }
        if self.lba > MAX_LBA {
            return Err(FormatError::OutOfRange {
                field: "lba",
                value: self.lba,
                max: MAX_LBA,
            });
        }
        let entry = self.map_entry();
        if entry.is_unmapped() {
            return Err(bad_field("birth_cp", 0));
        }
        entry.validate()
    }

    /// Validates the fields and checks the CRC.
    pub fn verify(&self) -> Result<(), FormatError> {
        self.validate()?;
        check_crc("cp log record", self.compute_crc(), self.crc32c)
    }
}

/// The 64-byte header of a CP log page.
#[derive(FromBytes, IntoBytes, Immutable, KnownLayout, Clone, Copy, PartialEq, Eq, Hash, Debug)]
#[repr(C)]
pub struct CpLogPageHdr {
    /// [`CPLOG_MAGIC`].
    pub magic: u32,
    /// [`FORMAT_VERSION`].
    pub ver: u16,
    /// Records used in this page, at most [`CP_LOG_RECS_PER_PAGE`].
    pub count: u16,
    /// The CP whose changes the page logs, at most [`MAX_CP_SEQ`].
    pub cp_seq: u64,
    /// The page's position in the log, counted from the log head.
    pub page_seq: u64,
    /// The aggregate.
    pub agg_id: Uuid,
    /// The nonce of `cp_seq`.
    pub cp_nonce: u64,
    /// Reserved, zero.
    pub rsvd: [u8; 12],
    /// CRC-32C over bytes `0..60`.
    pub crc32c: u32,
}

impl CpLogPageHdr {
    /// The CRC-32C over bytes `0..60`.
    #[must_use]
    pub fn compute_crc(&self) -> u32 {
        crc32c(&self.as_bytes()[..CP_LOG_PAGE_HDR_SIZE - 4])
    }

    /// Stores the CRC.
    pub fn seal(&mut self) {
        self.crc32c = self.compute_crc();
    }

    /// Checks magic, major version, CRC, the record count and the CP bound.
    pub fn verify(&self) -> Result<(), FormatError> {
        check_magic(self.magic, CPLOG_MAGIC)?;
        check_major(self.ver, FORMAT_MAJOR)?;
        check_crc("cp log page header", self.compute_crc(), self.crc32c)?;
        if self.count as usize > CP_LOG_RECS_PER_PAGE {
            return Err(FormatError::OutOfRange {
                field: "count",
                value: u64::from(self.count),
                max: CP_LOG_RECS_PER_PAGE as u64,
            });
        }
        if self.cp_seq > MAX_CP_SEQ {
            return Err(FormatError::OutOfRange {
                field: "cp_seq",
                value: self.cp_seq,
                max: MAX_CP_SEQ,
            });
        }
        Ok(())
    }
}

/// A 4 KiB CP log page: a header and up to 126 records.
#[derive(FromBytes, IntoBytes, Immutable, KnownLayout, Clone, Copy, PartialEq, Eq, Hash, Debug)]
#[repr(C, align(4096))]
pub struct CpLogPage {
    /// The header.
    pub hdr: CpLogPageHdr,
    /// The records; `hdr.count` of them are used, the rest are zero.
    pub recs: [CpLogRec; CP_LOG_RECS_PER_PAGE],
}

impl CpLogPage {
    /// An empty page for `cp_seq` of `agg_id` at position `page_seq`.
    #[must_use]
    pub fn new(agg_id: Uuid, cp_seq: u64, cp_nonce: u64, page_seq: u64) -> Self {
        let mut page = Self::new_zeroed();
        page.hdr.magic = CPLOG_MAGIC;
        page.hdr.ver = FORMAT_VERSION;
        page.hdr.cp_seq = cp_seq;
        page.hdr.cp_nonce = cp_nonce;
        page.hdr.page_seq = page_seq;
        page.hdr.agg_id = agg_id;
        page
    }

    /// Appends a sealed record; `None` when the page is full.
    pub fn push(&mut self, rec: CpLogRec) -> Option<usize> {
        let index = self.hdr.count as usize;
        if index >= CP_LOG_RECS_PER_PAGE {
            return None;
        }
        self.recs[index] = rec;
        self.hdr.count += 1;
        Some(index)
    }

    /// The records in use.
    #[must_use]
    pub fn records(&self) -> &[CpLogRec] {
        &self.recs[..(self.hdr.count as usize).min(CP_LOG_RECS_PER_PAGE)]
    }

    /// `true` when no record fits any more.
    #[must_use]
    pub const fn is_full(&self) -> bool {
        self.hdr.count as usize >= CP_LOG_RECS_PER_PAGE
    }

    /// Stores the header CRC. Records are sealed as they are built.
    pub fn seal(&mut self) {
        self.hdr.seal();
    }

    /// Verifies the header and every record in use; records beyond the
    /// count are ignored.
    pub fn verify(&self) -> Result<(), FormatError> {
        self.hdr.verify()?;
        self.records().iter().try_for_each(CpLogRec::verify)
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

const _: () = {
    assert!(SegKind::Data.as_u8() == 0 && SegKind::Metadata.as_u8() == 1);
    assert!(matches!(SegKind::from_u8(0), Some(SegKind::Data)));
    assert!(matches!(SegKind::from_u8(1), Some(SegKind::Metadata)));
    assert!(SegKind::from_u8(2).is_none());
    assert!(SegState::Free.as_u8() == 0);
    assert!(SegState::Open.as_u8() == 1);
    assert!(SegState::Sealed.as_u8() == 2);
    assert!(SegState::Relocated.as_u8() == 3);
    assert!(SegState::FreedPending.as_u8() == 4);
    assert!(matches!(SegState::from_u8(0), Some(SegState::Free)));
    assert!(matches!(SegState::from_u8(1), Some(SegState::Open)));
    assert!(matches!(SegState::from_u8(2), Some(SegState::Sealed)));
    assert!(matches!(SegState::from_u8(3), Some(SegState::Relocated)));
    assert!(matches!(SegState::from_u8(4), Some(SegState::FreedPending)));
    assert!(SegState::from_u8(5).is_none());
    assert!(SnapKind::Fsfreeze.as_u8() == 1 && SnapKind::Crash.as_u8() == 2);
    assert!(matches!(SnapKind::from_u8(1), Some(SnapKind::Fsfreeze)));
    assert!(matches!(SnapKind::from_u8(2), Some(SnapKind::Crash)));
    assert!(SnapKind::from_u8(0).is_none() && SnapKind::from_u8(3).is_none());
    assert!(SnapOrigin::Scheduled.as_u8() == 1);
    assert!(SnapOrigin::User.as_u8() == 2);
    assert!(SnapOrigin::Transfer.as_u8() == 3);
    assert!(SnapOrigin::Divergent.as_u8() == 4);
    assert!(matches!(
        SnapOrigin::from_u8(1),
        Some(SnapOrigin::Scheduled)
    ));
    assert!(matches!(
        SnapOrigin::from_u8(4),
        Some(SnapOrigin::Divergent)
    ));
    assert!(SnapOrigin::from_u8(0).is_none() && SnapOrigin::from_u8(5).is_none());

    assert!(size_of::<SegEntry>() == SEG_ENTRY_SIZE);
    assert!(align_of::<SegEntry>() == 8);
    assert!(offset_of!(SegEntry, phys) == 0);
    assert!(offset_of!(SegEntry, stripe_data_bytes) == 8);
    assert!(offset_of!(SegEntry, live_bytes) == 12);
    assert!(offset_of!(SegEntry, birth_cp) == 16);
    assert!(offset_of!(SegEntry, lap) == 20);
    assert!(offset_of!(SegEntry, kind) == 24);
    assert!(offset_of!(SegEntry, state) == 25);
    assert!(offset_of!(SegEntry, rsvd) == 26);
    assert!(offset_of!(SegEntry, crc32c) == 28);

    assert!(size_of::<SnapRecord>() == SNAP_RECORD_SIZE);
    assert!(align_of::<SnapRecord>() == 8);
    assert!(offset_of!(SnapRecord, snap_id) == 0);
    assert!(offset_of!(SnapRecord, snap_uuid) == 8);
    assert!(offset_of!(SnapRecord, map_root_pba) == 24);
    assert!(offset_of!(SnapRecord, deadlist_pba) == 32);
    assert!(offset_of!(SnapRecord, created_wall_ns) == 40);
    assert!(offset_of!(SnapRecord, birth_cp) == 48);
    assert!(offset_of!(SnapRecord, vol) == 52);
    assert!(offset_of!(SnapRecord, kind) == 56);
    assert!(offset_of!(SnapRecord, origin) == 57);
    assert!(offset_of!(SnapRecord, flags) == 58);
    assert!(offset_of!(SnapRecord, rsvd) == 59);
    assert!(offset_of!(SnapRecord, crc32c) == 60);

    assert!(size_of::<CpLogRec>() == CP_LOG_REC_SIZE);
    assert!(align_of::<CpLogRec>() == 8);
    assert!(offset_of!(CpLogRec, lba) == 0);
    assert!(offset_of!(CpLogRec, addr) == 8);
    assert!(offset_of!(CpLogRec, vol) == 16);
    assert!(offset_of!(CpLogRec, birth_cp) == 20);
    assert!(offset_of!(CpLogRec, block_crc32c) == 24);
    assert!(offset_of!(CpLogRec, crc32c) == 28);

    assert!(size_of::<CpLogPageHdr>() == CP_LOG_PAGE_HDR_SIZE);
    assert!(align_of::<CpLogPageHdr>() == 8);
    assert!(offset_of!(CpLogPageHdr, magic) == 0);
    assert!(offset_of!(CpLogPageHdr, ver) == 4);
    assert!(offset_of!(CpLogPageHdr, count) == 6);
    assert!(offset_of!(CpLogPageHdr, cp_seq) == 8);
    assert!(offset_of!(CpLogPageHdr, page_seq) == 16);
    assert!(offset_of!(CpLogPageHdr, agg_id) == 24);
    assert!(offset_of!(CpLogPageHdr, cp_nonce) == 40);
    assert!(offset_of!(CpLogPageHdr, rsvd) == 48);
    assert!(offset_of!(CpLogPageHdr, crc32c) == 60);

    assert!(size_of::<CpLogPage>() == CP_LOG_PAGE_SIZE);
    assert!(align_of::<CpLogPage>() == 4096);
    assert!(offset_of!(CpLogPage, hdr) == 0);
    assert!(offset_of!(CpLogPage, recs) == CP_LOG_PAGE_HDR_SIZE);
    assert!(CP_LOG_PAGE_HDR_SIZE + CP_LOG_RECS_PER_PAGE * CP_LOG_REC_SIZE == CP_LOG_PAGE_SIZE);
};

// ---------------------------------------------------------------------------
// Format 2.1: the CP log directory (F12) and the snapshot root page (F13)
// ---------------------------------------------------------------------------

/// The 64-byte header of a CP log directory page (F12).
///
/// The superblock's `cp_log_head` names a directory region: a run of
/// [`CpLogDirPage`]s listing, as [`RootRef`]s, the delta regions (runs of
/// [`CpLogPage`]s) of every consistency point since the last fold, in CP
/// order, followed by the map pages the writing CP wrote (the pages a fold
/// rewrote, and since format 2.3 the pages a relocation wrote for a
/// snapshot's tree in any CP), so a boot that selects the CP can verify
/// them. Every page of one region carries the same `cp_seq`,
/// `fold_cp_seq`, `agg_id`, `delta_entries` and `page_entries`; `page_seq`
/// numbers the pages from 0.
#[derive(FromBytes, IntoBytes, Immutable, KnownLayout, Clone, Copy, PartialEq, Eq, Hash, Debug)]
#[repr(C)]
pub struct CpLogDirHdr {
    /// [`CPDIR_MAGIC`].
    pub magic: u32,
    /// [`FORMAT_VERSION`].
    pub ver: u16,
    /// Entries used in this page, at most [`CP_LOG_DIR_ENTRIES`].
    pub count: u16,
    /// The CP that wrote the directory, at most [`MAX_CP_SEQ`].
    pub cp_seq: u64,
    /// The CP of the last fold: the map pages hold the state at this CP and
    /// the deltas listed are those of CPs `fold_cp_seq..=cp_seq`.
    pub fold_cp_seq: u64,
    /// The aggregate.
    pub agg_id: Uuid,
    /// Delta regions in the whole directory (the first entries).
    pub delta_entries: u32,
    /// Map page regions in the whole directory (the entries after the
    /// deltas): the pages `cp_seq` wrote. Before format 2.3 these were a
    /// fold's pages only and the field was non-zero only when
    /// `cp_seq == fold_cp_seq`; a 2.3 writer lists the pages a relocation
    /// wrote for a snapshot tree in any CP, and a 2.4 writer also the new
    /// root pages of the snapshots so rewritten (F21), so a boot verifies
    /// them as the commit's own.
    pub page_entries: u32,
    /// The page's position in the directory region, from 0.
    pub page_seq: u32,
    /// Reserved, zero.
    pub rsvd: [u8; 8],
    /// CRC-32C over bytes `0..60`.
    pub crc32c: u32,
}

impl CpLogDirHdr {
    /// The CRC-32C over bytes `0..60`.
    #[must_use]
    pub fn compute_crc(&self) -> u32 {
        crc32c(&self.as_bytes()[..CP_LOG_DIR_HDR_SIZE - 4])
    }

    /// Stores the CRC.
    pub fn seal(&mut self) {
        self.crc32c = self.compute_crc();
    }

    /// Checks magic, major version, CRC, the count and the CP bounds.
    pub fn verify(&self) -> Result<(), FormatError> {
        check_magic(self.magic, CPDIR_MAGIC)?;
        check_major(self.ver, FORMAT_MAJOR)?;
        check_crc("cp log directory header", self.compute_crc(), self.crc32c)?;
        if self.count as usize > CP_LOG_DIR_ENTRIES {
            return Err(FormatError::OutOfRange {
                field: "count",
                value: u64::from(self.count),
                max: CP_LOG_DIR_ENTRIES as u64,
            });
        }
        if self.cp_seq > MAX_CP_SEQ {
            return Err(FormatError::OutOfRange {
                field: "cp_seq",
                value: self.cp_seq,
                max: MAX_CP_SEQ,
            });
        }
        if self.fold_cp_seq > self.cp_seq {
            return Err(FormatError::OutOfRange {
                field: "fold_cp_seq",
                value: self.fold_cp_seq,
                max: self.cp_seq,
            });
        }
        Ok(())
    }
}

/// A 4 KiB CP log directory page: a header and up to 252 root references.
#[derive(FromBytes, IntoBytes, Immutable, KnownLayout, Clone, Copy, PartialEq, Eq, Hash, Debug)]
#[repr(C, align(4096))]
pub struct CpLogDirPage {
    /// The header.
    pub hdr: CpLogDirHdr,
    /// The entries; `hdr.count` of them are used, the rest are
    /// [`RootRef::NONE`].
    pub entries: [RootRef; CP_LOG_DIR_ENTRIES],
}

impl CpLogDirPage {
    /// An empty page for the directory `cp_seq` writes, whose last fold was
    /// `fold_cp_seq`, at position `page_seq`.
    #[must_use]
    pub fn new(agg_id: Uuid, cp_seq: u64, fold_cp_seq: u64, page_seq: u32) -> Self {
        let mut page = Self::new_zeroed();
        page.hdr.magic = CPDIR_MAGIC;
        page.hdr.ver = FORMAT_VERSION;
        page.hdr.cp_seq = cp_seq;
        page.hdr.fold_cp_seq = fold_cp_seq;
        page.hdr.agg_id = agg_id;
        page.hdr.page_seq = page_seq;
        page
    }

    /// Appends a reference, which must name a region at a non-zero block;
    /// `Ok(None)` when the page is full.
    pub fn push(&mut self, root: RootRef) -> Result<Option<usize>, FormatError> {
        if root.is_none() || root.pba == 0 {
            return Err(bad_field("pba", root.pba));
        }
        let index = self.hdr.count as usize;
        if index >= CP_LOG_DIR_ENTRIES {
            return Ok(None);
        }
        self.entries[index] = root;
        self.hdr.count += 1;
        Ok(Some(index))
    }

    /// The entries in use.
    #[must_use]
    pub fn used(&self) -> &[RootRef] {
        &self.entries[..(self.hdr.count as usize).min(CP_LOG_DIR_ENTRIES)]
    }

    /// `true` when no entry fits any more.
    #[must_use]
    pub const fn is_full(&self) -> bool {
        self.hdr.count as usize >= CP_LOG_DIR_ENTRIES
    }

    /// Stores the header CRC.
    pub fn seal(&mut self) {
        self.hdr.seal();
    }

    /// Verifies the header, that every used entry names a region at a
    /// non-zero block, and that the unused entries are absent.
    pub fn verify(&self) -> Result<(), FormatError> {
        self.hdr.verify()?;
        for (i, entry) in self.entries.iter().enumerate() {
            if i < self.hdr.count as usize {
                if entry.is_none() || entry.pba == 0 {
                    return Err(bad_field("pba", entry.pba));
                }
            } else if *entry != RootRef::NONE {
                return Err(bad_field("count", u64::from(self.hdr.count)));
            }
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

/// A CP log directory as a whole: what one region of [`CpLogDirPage`]s
/// encodes.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct CpLogDir {
    /// The CP that wrote it.
    pub cp_seq: u64,
    /// The CP of the last fold.
    pub fold_cp_seq: u64,
    /// The delta regions of CPs `fold_cp_seq..=cp_seq`, in CP order (a CP
    /// without changes has none; a CP may have several).
    pub deltas: Vec<RootRef>,
    /// The map pages `cp_seq` wrote: a fold's rewritten pages, and the
    /// pages a relocation wrote for a snapshot's tree (format 2.3) with the
    /// snapshot root pages it rewrote (format 2.4).
    pub pages: Vec<RootRef>,
}

impl CpLogDir {
    /// The region image: whole 4 KiB pages, at least one.
    pub fn encode(&self, agg_id: Uuid) -> Result<Vec<u8>, FormatError> {
        if self.cp_seq > MAX_CP_SEQ {
            return Err(FormatError::OutOfRange {
                field: "cp_seq",
                value: self.cp_seq,
                max: MAX_CP_SEQ,
            });
        }
        if self.fold_cp_seq > self.cp_seq {
            return Err(FormatError::OutOfRange {
                field: "fold_cp_seq",
                value: self.fold_cp_seq,
                max: self.cp_seq,
            });
        }
        let delta_entries =
            u32::try_from(self.deltas.len()).map_err(|_| bad_field("delta_entries", u64::MAX))?;
        let page_entries =
            u32::try_from(self.pages.len()).map_err(|_| bad_field("page_entries", u64::MAX))?;
        let fresh = |page_seq: u32| {
            let mut page = CpLogDirPage::new(agg_id, self.cp_seq, self.fold_cp_seq, page_seq);
            page.hdr.delta_entries = delta_entries;
            page.hdr.page_entries = page_entries;
            page
        };
        let mut out = Vec::new();
        let mut page_seq = 0u32;
        let mut page = fresh(page_seq);
        for root in self.deltas.iter().chain(self.pages.iter()) {
            if page.push(*root)?.is_none() {
                page.seal();
                out.extend_from_slice(page.as_bytes());
                page_seq += 1;
                page = fresh(page_seq);
                page.push(*root)?.ok_or_else(|| bad_field("count", 0))?;
            }
        }
        page.seal();
        out.extend_from_slice(page.as_bytes());
        Ok(out)
    }

    /// Decodes a region written by [`encode`](Self::encode) for `agg_id`:
    /// every page verifies, agrees with the first on the CP, the fold, the
    /// totals and the aggregate, and is in position; the entries add up to
    /// the totals.
    pub fn decode(bytes: &[u8], agg_id: &Uuid) -> Result<Self, FormatError> {
        if bytes.is_empty() || !bytes.len().is_multiple_of(CP_LOG_DIR_PAGE_SIZE) {
            return Err(FormatError::BadLength {
                expected: bytes.len().div_ceil(CP_LOG_DIR_PAGE_SIZE).max(1) * CP_LOG_DIR_PAGE_SIZE,
                found: bytes.len(),
            });
        }
        let mut all: Vec<RootRef> = Vec::new();
        let mut first: Option<CpLogDirHdr> = None;
        for (i, chunk) in bytes
            .as_chunks::<CP_LOG_DIR_PAGE_SIZE>()
            .0
            .iter()
            .enumerate()
        {
            let page = CpLogDirPage::read(chunk)?;
            if page.hdr.agg_id != *agg_id {
                return Err(FormatError::IdentityMismatch { field: "agg_id" });
            }
            if page.hdr.page_seq as usize != i {
                return Err(FormatError::IdentityMismatch { field: "page_seq" });
            }
            match &first {
                None => first = Some(page.hdr),
                Some(h) => {
                    if page.hdr.cp_seq != h.cp_seq
                        || page.hdr.fold_cp_seq != h.fold_cp_seq
                        || page.hdr.delta_entries != h.delta_entries
                        || page.hdr.page_entries != h.page_entries
                    {
                        return Err(FormatError::IdentityMismatch { field: "cp_seq" });
                    }
                }
            }
            all.extend_from_slice(page.used());
        }
        let hdr = first.ok_or(FormatError::BadLength {
            expected: CP_LOG_DIR_PAGE_SIZE,
            found: 0,
        })?;
        let total = hdr.delta_entries as usize + hdr.page_entries as usize;
        if all.len() != total {
            return Err(bad_field("count", all.len() as u64));
        }
        let pages = all.split_off(hdr.delta_entries as usize);
        Ok(Self {
            cp_seq: hdr.cp_seq,
            fold_cp_seq: hdr.fold_cp_seq,
            deltas: all,
            pages,
        })
    }

    /// Pages the encoded region takes.
    #[must_use]
    pub fn pages_needed(&self) -> usize {
        (self.deltas.len() + self.pages.len())
            .div_ceil(CP_LOG_DIR_ENTRIES)
            .max(1)
    }
}

/// The 4 KiB snapshot root page (F13): the page a [`SnapRecord`]'s
/// `map_root_pba` names, holding the pinned [`MapRoot`] and the deadlist
/// reference, sealed by its own CRC.
#[derive(FromBytes, IntoBytes, Immutable, KnownLayout, Clone, Copy, PartialEq, Eq, Hash, Debug)]
#[repr(C, align(4096))]
pub struct SnapRootPage {
    /// [`SNAPROOT_MAGIC`].
    pub magic: u32,
    /// [`FORMAT_VERSION`].
    pub ver: u16,
    /// Reserved, zero.
    pub rsvd0: u16,
    /// The snapshot, at least 1.
    pub snap_id: u64,
    /// The aggregate.
    pub agg_id: Uuid,
    /// The CP the snapshot pins, at least 1.
    pub birth_cp: u64,
    /// The pinned root: the volume's map as of `birth_cp`.
    pub root: MapRoot,
    /// The deadlist region, or none.
    pub deadlist: RootRef,
    /// Reserved, zero.
    pub reserved: [u8; SNAP_ROOT_RESERVED],
    /// CRC-32C over bytes `0..4092`.
    pub crc32c: u32,
}

impl SnapRootPage {
    /// A page pinning `root` as snapshot `snap_id` of `agg_id` at
    /// `birth_cp`, sealed.
    pub fn new(
        agg_id: Uuid,
        snap_id: u64,
        birth_cp: u64,
        root: MapRoot,
        deadlist: RootRef,
    ) -> Result<Self, FormatError> {
        let mut page = Self::new_zeroed();
        page.magic = SNAPROOT_MAGIC;
        page.ver = FORMAT_VERSION;
        page.snap_id = snap_id;
        page.agg_id = agg_id;
        page.birth_cp = birth_cp;
        page.root = root;
        page.deadlist = deadlist;
        page.validate()?;
        page.seal();
        Ok(page)
    }

    /// The CRC-32C over bytes `0..4092`.
    #[must_use]
    pub fn compute_crc(&self) -> u32 {
        crc32c(&self.as_bytes()[..SNAP_ROOT_CRC_OFFSET])
    }

    /// Stores the CRC.
    pub fn seal(&mut self) {
        self.crc32c = self.compute_crc();
    }

    /// Checks the fields: a snapshot id and a birth, a root that validates,
    /// has a size and was checkpointed at or before the birth, a deadlist
    /// that is absent or names a region at a non-zero block.
    pub fn validate(&self) -> Result<(), FormatError> {
        check_magic(self.magic, SNAPROOT_MAGIC)?;
        check_major(self.ver, FORMAT_MAJOR)?;
        if self.snap_id == 0 {
            return Err(bad_field("snap_id", 0));
        }
        if self.birth_cp == 0 || self.birth_cp > MAX_CP_SEQ {
            return Err(FormatError::OutOfRange {
                field: "birth_cp",
                value: self.birth_cp,
                max: MAX_CP_SEQ,
            });
        }
        self.root.validate()?;
        if self.root.lba_count == 0 {
            return Err(bad_field("lba_count", 0));
        }
        if self.root.checkpoint_cp_seq > self.birth_cp {
            return Err(FormatError::OutOfRange {
                field: "checkpoint_cp_seq",
                value: self.root.checkpoint_cp_seq,
                max: self.birth_cp,
            });
        }
        if self.deadlist.is_none() {
            if self.deadlist != RootRef::NONE {
                return Err(bad_field("deadlist", self.deadlist.pba));
            }
        } else if self.deadlist.pba == 0 {
            return Err(bad_field("deadlist", 0));
        }
        Ok(())
    }

    /// Validates the fields and checks the CRC.
    pub fn verify(&self) -> Result<(), FormatError> {
        self.validate()?;
        check_crc("snapshot root page", self.compute_crc(), self.crc32c)
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

/// The snapshot table region (F14): `records` in ascending `snap_id` order,
/// none empty, padded with zero bytes to whole 4 KiB pages. An empty table
/// is no region at all (`snap_table` is [`RootRef::NONE`]).
pub fn snap_table_image(records: &[SnapRecord]) -> Result<Vec<u8>, FormatError> {
    let mut last = 0u64;
    for rec in records {
        rec.verify()?;
        if rec.snap_id <= last {
            return Err(bad_field("snap_id", rec.snap_id));
        }
        last = rec.snap_id;
    }
    let mut bytes = records.as_bytes().to_vec();
    let padded = bytes.len().div_ceil(BLOCK_SIZE).max(1) * BLOCK_SIZE;
    bytes.resize(padded, 0);
    Ok(bytes)
}

/// Parses a snapshot table region: records until the first empty slot,
/// each verified, ids strictly ascending, and only zero bytes after.
pub fn parse_snap_table(bytes: &[u8]) -> Result<Vec<SnapRecord>, FormatError> {
    if !bytes.len().is_multiple_of(BLOCK_SIZE) {
        return Err(FormatError::BadLength {
            expected: bytes.len().div_ceil(BLOCK_SIZE) * BLOCK_SIZE,
            found: bytes.len(),
        });
    }
    let mut out = Vec::new();
    let mut last = 0u64;
    let mut rest: &[u8] = bytes;
    while rest.len() >= SNAP_RECORD_SIZE {
        let (chunk, tail) = rest.split_at(SNAP_RECORD_SIZE);
        if chunk.iter().all(|b| *b == 0) {
            break;
        }
        let rec: SnapRecord = copy_out(chunk)?;
        rec.verify()?;
        if rec.snap_id <= last {
            return Err(bad_field("snap_id", rec.snap_id));
        }
        last = rec.snap_id;
        out.push(rec);
        rest = tail;
    }
    if rest.iter().any(|b| *b != 0) {
        return Err(bad_field("snap_id", 0));
    }
    Ok(out)
}

const _: () = {
    assert!(size_of::<CpLogDirHdr>() == CP_LOG_DIR_HDR_SIZE);
    assert!(align_of::<CpLogDirHdr>() == 8);
    assert!(offset_of!(CpLogDirHdr, magic) == 0);
    assert!(offset_of!(CpLogDirHdr, ver) == 4);
    assert!(offset_of!(CpLogDirHdr, count) == 6);
    assert!(offset_of!(CpLogDirHdr, cp_seq) == 8);
    assert!(offset_of!(CpLogDirHdr, fold_cp_seq) == 16);
    assert!(offset_of!(CpLogDirHdr, agg_id) == 24);
    assert!(offset_of!(CpLogDirHdr, delta_entries) == 40);
    assert!(offset_of!(CpLogDirHdr, page_entries) == 44);
    assert!(offset_of!(CpLogDirHdr, page_seq) == 48);
    assert!(offset_of!(CpLogDirHdr, rsvd) == 52);
    assert!(offset_of!(CpLogDirHdr, crc32c) == 60);
    assert!(size_of::<CpLogDirPage>() == CP_LOG_DIR_PAGE_SIZE);
    assert!(align_of::<CpLogDirPage>() == 4096);
    assert!(offset_of!(CpLogDirPage, hdr) == 0);
    assert!(offset_of!(CpLogDirPage, entries) == CP_LOG_DIR_HDR_SIZE);

    assert!(size_of::<SnapRootPage>() == SNAP_ROOT_PAGE_SIZE);
    assert!(align_of::<SnapRootPage>() == 4096);
    assert!(offset_of!(SnapRootPage, magic) == 0);
    assert!(offset_of!(SnapRootPage, ver) == 4);
    assert!(offset_of!(SnapRootPage, rsvd0) == 6);
    assert!(offset_of!(SnapRootPage, snap_id) == 8);
    assert!(offset_of!(SnapRootPage, agg_id) == 16);
    assert!(offset_of!(SnapRootPage, birth_cp) == 32);
    assert!(offset_of!(SnapRootPage, root) == 40);
    assert!(offset_of!(SnapRootPage, deadlist) == 104);
    assert!(offset_of!(SnapRootPage, reserved) == 120);
    assert!(offset_of!(SnapRootPage, crc32c) == SNAP_ROOT_CRC_OFFSET);
};

// ---------------------------------------------------------------------------
// Format 2.3: the consistency point's stripe list (F17)
// ---------------------------------------------------------------------------

/// One entry of a consistency point's stripe list (F17): a data stripe the
/// CP wrote, named by segment and index, with the stripe's kind for the
/// record. The superblock's `cp_stripes` root names a region of these,
/// padded to whole pages; a boot verifies every listed stripe's summary
/// against the CP's identity before it trusts the commit, whatever the
/// births of the blocks inside (a relocation stripe carries old births).
#[derive(
    FromBytes, IntoBytes, Immutable, KnownLayout, Clone, Copy, PartialEq, Eq, Hash, Default, Debug,
)]
#[repr(C)]
pub struct CpStripe {
    /// The physical segment, never [`NULL_PSEG`].
    pub pseg: u64,
    /// The stripe's index within the segment.
    pub idx: u32,
    /// A [`crate::StripeKind`] byte.
    pub kind: u8,
    /// Reserved, zero.
    pub rsvd: [u8; 3],
}

impl CpStripe {
    /// An entry for stripe `idx` of segment `pseg` of `kind`.
    pub fn new(pseg: u64, idx: u32, kind: crate::StripeKind) -> Result<Self, FormatError> {
        let entry = Self {
            pseg,
            idx,
            kind: kind.as_u8(),
            rsvd: [0; 3],
        };
        entry.validate()?;
        Ok(entry)
    }

    /// The decoded kind.
    #[must_use]
    pub const fn stripe_kind(&self) -> Option<crate::StripeKind> {
        crate::StripeKind::from_u8(self.kind)
    }

    /// `true` for an all-zero entry (the end of a list).
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.pseg == 0 && self.idx == 0 && self.kind == 0
    }

    /// Checks the fields: a segment and a defined kind.
    pub const fn validate(&self) -> Result<(), FormatError> {
        if self.pseg == NULL_PSEG {
            return Err(bad_field("pseg", 0));
        }
        if self.stripe_kind().is_none() {
            return Err(bad_field("kind", self.kind as u64));
        }
        Ok(())
    }
}

/// The stripe list region (F17): `entries` in strictly ascending
/// `(pseg, idx)` order, at least one, each validated, padded with zero
/// bytes to whole 4 KiB pages. A CP that wrote no data stripe writes no
/// region (`cp_stripes` is [`RootRef::NONE`]).
pub fn cp_stripes_image(entries: &[CpStripe]) -> Result<Vec<u8>, FormatError> {
    if entries.is_empty() {
        return Err(bad_field("count", 0));
    }
    let mut last: Option<(u64, u32)> = None;
    for (i, entry) in entries.iter().enumerate() {
        entry.validate()?;
        let key = (entry.pseg, entry.idx);
        if last.is_some_and(|l| key <= l) {
            return Err(bad_field("idx", i as u64));
        }
        last = Some(key);
    }
    let mut bytes = entries.as_bytes().to_vec();
    let padded = bytes.len().div_ceil(BLOCK_SIZE).max(1) * BLOCK_SIZE;
    bytes.resize(padded, 0);
    Ok(bytes)
}

/// Parses a stripe list region: entries until the first all-zero one, each
/// validated, strictly ascending, at least one, and only zero bytes after.
pub fn parse_cp_stripes(bytes: &[u8]) -> Result<Vec<CpStripe>, FormatError> {
    if !bytes.len().is_multiple_of(BLOCK_SIZE) {
        return Err(FormatError::BadLength {
            expected: bytes.len().div_ceil(BLOCK_SIZE) * BLOCK_SIZE,
            found: bytes.len(),
        });
    }
    let mut out: Vec<CpStripe> = Vec::new();
    let mut rest: &[u8] = bytes;
    while rest.len() >= CP_STRIPE_SIZE {
        let (chunk, tail) = rest.split_at(CP_STRIPE_SIZE);
        if chunk.iter().all(|b| *b == 0) {
            break;
        }
        let entry: CpStripe = copy_out(chunk)?;
        entry.validate()?;
        if entry.rsvd != [0; 3] {
            return Err(bad_field("rsvd", out.len() as u64));
        }
        let key = (entry.pseg, entry.idx);
        if out.last().is_some_and(|l| key <= (l.pseg, l.idx)) {
            return Err(bad_field("idx", out.len() as u64));
        }
        out.push(entry);
        rest = tail;
    }
    if out.is_empty() {
        return Err(bad_field("count", 0));
    }
    if rest.iter().any(|b| *b != 0) {
        return Err(bad_field("pseg", out.len() as u64));
    }
    Ok(out)
}

const _: () = {
    assert!(size_of::<CpStripe>() == CP_STRIPE_SIZE);
    assert!(align_of::<CpStripe>() == 8);
    assert!(offset_of!(CpStripe, pseg) == 0);
    assert!(offset_of!(CpStripe, idx) == 8);
    assert!(offset_of!(CpStripe, kind) == 12);
    assert!(offset_of!(CpStripe, rsvd) == 13);
};
