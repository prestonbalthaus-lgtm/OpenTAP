//! The name catalogue (format 2.3, F16): the human-readable names of
//! volumes and snapshots, kept in the pool so they survive reboots and
//! crashes with the objects they name.
//!
//! The superblock's `catalog` root names a region of [`CatalogRec`]s, one
//! per named volume or snapshot, padded with zero bytes to whole 4 KiB
//! pages. A record is keyed by what it names: a volume by its index, a
//! snapshot by its uuid (the snapshot catalogue's identity, F11). Names are
//! UTF-8 of 1 to 255 bytes without control characters or `/`; a volume name
//! is unique among volumes and a snapshot name among the snapshots of its
//! volume. The region is written in a canonical order (volumes by index,
//! then snapshots by volume and uuid) so two engines writing the same names
//! write the same bytes, and a commit that changed no name carries the
//! previous region's reference forward unchanged.
//!
//! Every record carries its own CRC, and the region as a whole is sealed by
//! the superblock's root reference like every other table region (F14).

use core::mem::{align_of, offset_of, size_of};

use zerocopy::{FromBytes, FromZeros, Immutable, IntoBytes, KnownLayout};

use crate::checksum::crc32c;
use crate::constants::{
    BLOCK_SIZE, CATALOG_NAME_MAX, CATALOG_RECORD_SIZE, CATALOG_RESERVED, MAX_VOL,
};
use crate::types::{bad_field, check_crc, copy_out, FormatError, Uuid};

/// What a catalogue record names.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[repr(u8)]
pub enum CatalogKind {
    /// A volume, keyed by its index.
    Volume = 1,
    /// A snapshot, keyed by its uuid.
    Snapshot = 2,
}

impl CatalogKind {
    /// Decodes the on-disk byte.
    #[must_use]
    pub const fn from_u8(value: u8) -> Option<Self> {
        match value {
            1 => Some(Self::Volume),
            2 => Some(Self::Snapshot),
            _ => None,
        }
    }

    /// The on-disk byte.
    #[must_use]
    pub const fn as_u8(self) -> u8 {
        self as u8
    }
}

/// Checks a name: 1 to [`CATALOG_NAME_MAX`] bytes of UTF-8, no control
/// character (below 0x20, or 0x7f) and no `/`. Refusals name the length
/// (`name_len`) or the offending byte (`name`).
pub fn validate_name(name: &[u8]) -> Result<(), FormatError> {
    if name.is_empty() || name.len() > CATALOG_NAME_MAX {
        return Err(bad_field("name_len", name.len() as u64));
    }
    if core::str::from_utf8(name).is_err() {
        return Err(bad_field("name", u64::MAX));
    }
    for byte in name {
        if *byte < 0x20 || *byte == 0x7f || *byte == b'/' {
            return Err(bad_field("name", u64::from(*byte)));
        }
    }
    Ok(())
}

/// The 512-byte catalogue record.
#[derive(FromBytes, IntoBytes, Immutable, KnownLayout, Clone, Copy, PartialEq, Eq, Hash, Debug)]
#[repr(C)]
pub struct CatalogRec {
    /// A [`CatalogKind`] byte; 0 marks an empty slot.
    pub kind: u8,
    /// Bytes of `name` in use, 1 to [`CATALOG_NAME_MAX`].
    pub name_len: u8,
    /// Reserved flag bits, zero; ignored by readers.
    pub flags: u8,
    /// Reserved, zero.
    pub rsvd0: u8,
    /// The volume the record names, or the snapshot's volume.
    pub vol: u32,
    /// The snapshot's uuid; nil for a volume.
    pub snap_uuid: Uuid,
    /// When the name was given, nanoseconds since the Unix epoch.
    pub created_wall_ns: u64,
    /// The name, `name_len` bytes of UTF-8 followed by zeros.
    pub name: [u8; CATALOG_NAME_MAX],
    /// Reserved, zero.
    pub rsvd1: [u8; CATALOG_RESERVED],
    /// CRC-32C over bytes `0..508`.
    pub crc32c: u32,
}

impl CatalogRec {
    /// An empty slot.
    #[must_use]
    pub fn empty() -> Self {
        Self::new_zeroed()
    }

    fn build(
        kind: CatalogKind,
        vol: u32,
        snap_uuid: Uuid,
        name: &str,
        created_wall_ns: u64,
    ) -> Result<Self, FormatError> {
        validate_name(name.as_bytes())?;
        let mut rec = Self::new_zeroed();
        rec.kind = kind.as_u8();
        rec.name_len = name.len() as u8;
        rec.vol = vol;
        rec.snap_uuid = snap_uuid;
        rec.created_wall_ns = created_wall_ns;
        rec.name[..name.len()].copy_from_slice(name.as_bytes());
        rec.validate()?;
        rec.seal();
        Ok(rec)
    }

    /// A sealed record naming volume `vol`.
    pub fn volume(vol: u32, name: &str, created_wall_ns: u64) -> Result<Self, FormatError> {
        Self::build(CatalogKind::Volume, vol, Uuid::NIL, name, created_wall_ns)
    }

    /// A sealed record naming the snapshot `snap_uuid` of volume `vol`.
    pub fn snapshot(
        vol: u32,
        snap_uuid: Uuid,
        name: &str,
        created_wall_ns: u64,
    ) -> Result<Self, FormatError> {
        Self::build(CatalogKind::Snapshot, vol, snap_uuid, name, created_wall_ns)
    }

    /// The CRC-32C over bytes `0..508`.
    #[must_use]
    pub fn compute_crc(&self) -> u32 {
        crc32c(&self.as_bytes()[..CATALOG_RECORD_SIZE - 4])
    }

    /// Stores the CRC. Call after the last field change.
    pub fn seal(&mut self) {
        self.crc32c = self.compute_crc();
    }

    /// `true` for an empty slot.
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.kind == 0
    }

    /// The decoded kind.
    #[must_use]
    pub const fn catalog_kind(&self) -> Option<CatalogKind> {
        CatalogKind::from_u8(self.kind)
    }

    /// The name bytes in use.
    #[must_use]
    pub fn name_bytes(&self) -> &[u8] {
        &self.name[..(self.name_len as usize).min(CATALOG_NAME_MAX)]
    }

    /// The name, which a validated record holds as UTF-8.
    #[must_use]
    pub fn name(&self) -> &str {
        core::str::from_utf8(self.name_bytes()).unwrap_or("")
    }

    /// The canonical order of the region: volumes by index, then snapshots
    /// by volume and uuid.
    #[must_use]
    pub fn sort_key(&self) -> (u8, u32, [u8; 16]) {
        (self.kind, self.vol, *self.snap_uuid.as_array())
    }

    /// Checks the fields: an empty slot is all zero; a record has a defined
    /// kind, a volume in range, a valid name with zeros after it, and a nil
    /// uuid for a volume or a non-nil one for a snapshot.
    pub fn validate(&self) -> Result<(), FormatError> {
        if self.is_empty() {
            if self.as_bytes().iter().any(|b| *b != 0) {
                return Err(bad_field("kind", 0));
            }
            return Ok(());
        }
        let Some(kind) = self.catalog_kind() else {
            return Err(bad_field("kind", u64::from(self.kind)));
        };
        if self.vol > MAX_VOL {
            return Err(FormatError::OutOfRange {
                field: "vol",
                value: u64::from(self.vol),
                max: u64::from(MAX_VOL),
            });
        }
        validate_name(self.name_bytes())?;
        if self.name[self.name_len as usize..].iter().any(|b| *b != 0) {
            return Err(bad_field("name_len", u64::from(self.name_len)));
        }
        match kind {
            CatalogKind::Volume if !self.snap_uuid.is_nil() => Err(bad_field("snap_uuid", 1)),
            CatalogKind::Snapshot if self.snap_uuid.is_nil() => Err(bad_field("snap_uuid", 0)),
            _ => Ok(()),
        }
    }

    /// Validates the fields and checks the CRC (an empty slot has none).
    pub fn verify(&self) -> Result<(), FormatError> {
        self.validate()?;
        if self.is_empty() {
            return Ok(());
        }
        check_crc("catalogue record", self.compute_crc(), self.crc32c)
    }
}

/// Checks that `records` is a valid catalogue: every record verifies, none
/// is empty, keys strictly ascend in [`CatalogRec::sort_key`] order (so
/// every key is unique), volume names are unique and snapshot names are
/// unique within a volume.
fn check_catalog(records: &[CatalogRec]) -> Result<(), FormatError> {
    let mut last: Option<(u8, u32, [u8; 16])> = None;
    // A volume name is unique among every volume (one scope); a snapshot
    // name among the snapshots of its volume (a scope per volume). One
    // pass with a set, so a catalogue of every volume and snapshot the
    // format allows checks in linear time.
    let mut names: std::collections::HashSet<(u8, u32, &[u8])> = std::collections::HashSet::new();
    for (i, rec) in records.iter().enumerate() {
        rec.verify()?;
        if rec.is_empty() {
            return Err(bad_field("kind", i as u64));
        }
        let key = rec.sort_key();
        if last.is_some_and(|l| key <= l) {
            return Err(bad_field("sort_key", i as u64));
        }
        last = Some(key);
        let scope = if rec.catalog_kind() == Some(CatalogKind::Volume) {
            0
        } else {
            rec.vol
        };
        if !names.insert((rec.kind, scope, rec.name_bytes())) {
            return Err(bad_field("name", i as u64));
        }
    }
    Ok(())
}

/// The catalogue region: `records` in canonical order, verified and
/// unique, padded with zero bytes to whole 4 KiB pages (at least one).
pub fn catalog_image(records: &[CatalogRec]) -> Result<Vec<u8>, FormatError> {
    check_catalog(records)?;
    let mut bytes = records.as_bytes().to_vec();
    let padded = bytes.len().div_ceil(BLOCK_SIZE).max(1) * BLOCK_SIZE;
    bytes.resize(padded, 0);
    Ok(bytes)
}

/// Parses a catalogue region: records until the first empty slot, each
/// verified, keys strictly ascending and names unique, and only zero bytes
/// after.
pub fn parse_catalog(bytes: &[u8]) -> Result<Vec<CatalogRec>, FormatError> {
    if !bytes.len().is_multiple_of(BLOCK_SIZE) {
        return Err(FormatError::BadLength {
            expected: bytes.len().div_ceil(BLOCK_SIZE) * BLOCK_SIZE,
            found: bytes.len(),
        });
    }
    let mut out = Vec::new();
    let mut rest: &[u8] = bytes;
    while rest.len() >= CATALOG_RECORD_SIZE {
        let (chunk, tail) = rest.split_at(CATALOG_RECORD_SIZE);
        if chunk.iter().all(|b| *b == 0) {
            break;
        }
        let rec: CatalogRec = copy_out(chunk)?;
        out.push(rec);
        rest = tail;
    }
    if rest.iter().any(|b| *b != 0) {
        return Err(bad_field("kind", out.len() as u64));
    }
    check_catalog(&out)?;
    Ok(out)
}

const _: () = {
    assert!(CatalogKind::Volume.as_u8() == 1 && CatalogKind::Snapshot.as_u8() == 2);
    assert!(matches!(CatalogKind::from_u8(1), Some(CatalogKind::Volume)));
    assert!(matches!(
        CatalogKind::from_u8(2),
        Some(CatalogKind::Snapshot)
    ));
    assert!(CatalogKind::from_u8(0).is_none() && CatalogKind::from_u8(3).is_none());
    assert!(size_of::<CatalogRec>() == CATALOG_RECORD_SIZE);
    assert!(align_of::<CatalogRec>() == 8);
    assert!(offset_of!(CatalogRec, kind) == 0);
    assert!(offset_of!(CatalogRec, name_len) == 1);
    assert!(offset_of!(CatalogRec, flags) == 2);
    assert!(offset_of!(CatalogRec, rsvd0) == 3);
    assert!(offset_of!(CatalogRec, vol) == 4);
    assert!(offset_of!(CatalogRec, snap_uuid) == 8);
    assert!(offset_of!(CatalogRec, created_wall_ns) == 24);
    assert!(offset_of!(CatalogRec, name) == 32);
    assert!(offset_of!(CatalogRec, rsvd1) == 32 + CATALOG_NAME_MAX);
    assert!(offset_of!(CatalogRec, crc32c) == CATALOG_RECORD_SIZE - 4);
};
