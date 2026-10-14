//! Shared primitives: identifiers, the volume bitset, root references, aligned
//! buffers and the error type.
//!
//! # Alignment
//! The zero-copy views (`parse`, `ref_from_bytes`) need a buffer aligned to
//! the structure: 4096 for pages, 8 for records. Page-aligned direct-I/O
//! buffers, arena memory, [`Block4K`] and [`Page16K`] qualify; a `Vec<u8>`
//! does not, because its alignment is the allocator's choice (16 under glibc,
//! so a `Superblock` view over it fails with [`FormatError::Misaligned`]). The
//! `read` paths copy and work at any alignment.

use core::fmt;
use core::mem::{align_of, offset_of, size_of};

use zerocopy::{FromBytes, Immutable, IntoBytes, KnownLayout, Unaligned};

use crate::checksum::crc32c;
use crate::constants::{BLOCK_SIZE, MAP_PAGE_SIZE, MAX_VOLS, ROOT_REF_SIZE, VOL_BITSET_WORDS};

/// Why a structure failed to parse or verify.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum FormatError {
    /// The magic number is not the one this structure carries.
    BadMagic {
        /// The magic the structure must carry.
        expected: u32,
        /// The magic that was read.
        found: u32,
    },
    /// The major version is not the one this crate reads.
    BadVersion {
        /// The major version this crate reads.
        expected_major: u8,
        /// The full `ver` field that was read.
        found: u16,
    },
    /// A structure with this crate's magic carries a *higher* major version:
    /// the aggregate was written by a newer release. The reader must stop; it
    /// must never fall back to an older commit (F7).
    NewerFormat {
        /// The full `ver` field that was read.
        found: u16,
    },
    /// The stored CRC-32C does not match the bytes it covers.
    BadCrc {
        /// Which CRC failed: the page, the volume table, the header, the summary, ...
        region: &'static str,
        /// The CRC computed over the covered bytes.
        computed: u32,
        /// The CRC that was stored.
        stored: u32,
    },
    /// The buffer is not the size the structure needs.
    BadLength {
        /// The size needed.
        expected: usize,
        /// The size given.
        found: usize,
    },
    /// The buffer is not aligned for a zero-copy view of this structure.
    Misaligned {
        /// The alignment needed.
        required: usize,
    },
    /// A field holds a value the format does not define, or two fields
    /// contradict each other.
    BadField {
        /// The field.
        field: &'static str,
        /// The value read (or the index of the offending record).
        value: u64,
    },
    /// A value does not fit the bits or the bound the format gives it.
    OutOfRange {
        /// The field.
        field: &'static str,
        /// The value offered.
        value: u64,
        /// The largest value the field can carry.
        max: u64,
    },
    /// A structure verifies but is not the one the caller asked for: a stale
    /// stripe image at a reused location, or a slot of another aggregate.
    IdentityMismatch {
        /// The field that differs.
        field: &'static str,
    },
    /// No slot of the superblock ring verifies for this aggregate.
    NoValidSlot,
}

impl fmt::Display for FormatError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match *self {
            Self::BadMagic { expected, found } => {
                write!(
                    f,
                    "bad magic: expected {expected:#010x}, found {found:#010x}"
                )
            }
            Self::BadVersion {
                expected_major,
                found,
            } => write!(
                f,
                "unsupported version {found:#06x}: this reader handles major {expected_major}"
            ),
            Self::NewerFormat { found } => write!(
                f,
                "format version {found:#06x} is newer than this reader; upgrade before mounting"
            ),
            Self::BadCrc {
                region,
                computed,
                stored,
            } => write!(
                f,
                "crc32c mismatch in {region}: computed {computed:#010x}, stored {stored:#010x}"
            ),
            Self::BadLength { expected, found } => {
                write!(f, "bad length: expected {expected} bytes, found {found}")
            }
            Self::Misaligned { required } => {
                write!(f, "buffer is not {required}-byte aligned")
            }
            Self::BadField { field, value } => {
                write!(f, "field `{field}` holds undefined value {value:#x}")
            }
            Self::OutOfRange { field, value, max } => {
                write!(f, "field `{field}` value {value} exceeds its maximum {max}")
            }
            Self::IdentityMismatch { field } => {
                write!(
                    f,
                    "structure verifies but `{field}` is not the one expected"
                )
            }
            Self::NoValidSlot => f.write_str("no superblock slot verifies for this aggregate"),
        }
    }
}

impl std::error::Error for FormatError {}

/// Maps a zero-copy cast failure to a [`FormatError`].
pub(crate) fn cast_error<S, D: ?Sized>(
    err: zerocopy::CastError<S, D>,
    expected: usize,
    found: usize,
    required_align: usize,
) -> FormatError {
    match err {
        zerocopy::ConvertError::Alignment(_) => FormatError::Misaligned {
            required: required_align,
        },
        zerocopy::ConvertError::Size(_) => FormatError::BadLength { expected, found },
        zerocopy::ConvertError::Validity(never) => match never {},
    }
}

/// Zero-copy view of a fixed-size structure over an exact-length buffer.
pub(crate) fn view<T: FromBytes + KnownLayout + Immutable>(
    bytes: &[u8],
) -> Result<&T, FormatError> {
    T::ref_from_bytes(bytes)
        .map_err(|e| cast_error(e, size_of::<T>(), bytes.len(), align_of::<T>()))
}

/// Copy of a fixed-size structure out of an exact-length buffer of any alignment.
pub(crate) fn copy_out<T: FromBytes>(bytes: &[u8]) -> Result<T, FormatError> {
    T::read_from_bytes(bytes).map_err(|_| FormatError::BadLength {
        expected: size_of::<T>(),
        found: bytes.len(),
    })
}

pub(crate) fn check_magic(found: u32, expected: u32) -> Result<(), FormatError> {
    if found == expected {
        Ok(())
    } else {
        Err(FormatError::BadMagic { expected, found })
    }
}

/// The major half of a `ver` field.
pub(crate) const fn major_of(ver: u16) -> u8 {
    (ver >> 8) as u8
}

/// The minor of a `ver` field.
pub(crate) const fn minor_of(ver: u16) -> u8 {
    ver as u8
}

pub(crate) fn check_major(ver: u16, expected_major: u8) -> Result<(), FormatError> {
    if major_of(ver) == expected_major {
        Ok(())
    } else {
        Err(FormatError::BadVersion {
            expected_major,
            found: ver,
        })
    }
}

pub(crate) fn check_crc(
    region: &'static str,
    computed: u32,
    stored: u32,
) -> Result<(), FormatError> {
    if computed == stored {
        Ok(())
    } else {
        Err(FormatError::BadCrc {
            region,
            computed,
            stored,
        })
    }
}

pub(crate) const fn bad_field(field: &'static str, value: u64) -> FormatError {
    FormatError::BadField { field, value }
}

/// A 128-bit identifier stored as 16 raw bytes (an RFC 4122 UUID in memory
/// order). Used for aggregates, boot ids and device ids. `repr(transparent)`
/// over `[u8; 16]`, which has the same layout as `repr(C)`.
#[derive(
    FromBytes,
    IntoBytes,
    Immutable,
    KnownLayout,
    Unaligned,
    Clone,
    Copy,
    PartialEq,
    Eq,
    Hash,
    Default,
)]
#[repr(transparent)]
pub struct Uuid(pub [u8; 16]);

/// The identity of an aggregate.
pub type AggId = Uuid;

impl Uuid {
    /// The all-zero identifier, meaning "none".
    pub const NIL: Self = Self([0; 16]);

    /// Wraps raw bytes.
    #[must_use]
    pub const fn from_bytes(bytes: [u8; 16]) -> Self {
        Self(bytes)
    }

    /// The raw bytes as an array.
    #[must_use]
    pub const fn as_array(&self) -> &[u8; 16] {
        &self.0
    }

    /// `true` for the all-zero identifier.
    #[must_use]
    pub const fn is_nil(&self) -> bool {
        let mut i = 0;
        while i < 16 {
            if self.0[i] != 0 {
                return false;
            }
            i += 1;
        }
        true
    }
}

impl fmt::Display for Uuid {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for (i, byte) in self.0.iter().enumerate() {
            if matches!(i, 4 | 6 | 8 | 10) {
                f.write_str("-")?;
            }
            write!(f, "{byte:02x}")?;
        }
        Ok(())
    }
}

impl fmt::Debug for Uuid {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Uuid({self})")
    }
}

/// A fixed bitset with one bit per volume index below [`MAX_VOLS`]; `vol` is
/// the index into the volume table (F8). Bit `v` is bit `v % 64` of word
/// `v / 64`.
#[derive(
    FromBytes, IntoBytes, Immutable, KnownLayout, Clone, Copy, PartialEq, Eq, Hash, Default,
)]
#[repr(C)]
pub struct VolBitSet {
    /// The bit words, least significant bit first.
    pub words: [u64; VOL_BITSET_WORDS],
}

impl VolBitSet {
    /// The empty set.
    pub const EMPTY: Self = Self {
        words: [0; VOL_BITSET_WORDS],
    };

    /// The empty set.
    #[must_use]
    pub const fn new() -> Self {
        Self::EMPTY
    }

    const fn locate(vol: u32) -> Result<(usize, u64), FormatError> {
        if (vol as usize) < MAX_VOLS {
            Ok(((vol / 64) as usize, 1u64 << (vol % 64)))
        } else {
            Err(FormatError::OutOfRange {
                field: "vol",
                value: vol as u64,
                max: (MAX_VOLS - 1) as u64,
            })
        }
    }

    /// Whether volume `vol` is in the set. Out-of-range indices are never set.
    #[must_use]
    pub const fn get(&self, vol: u32) -> bool {
        match Self::locate(vol) {
            Ok((word, mask)) => (self.words[word] & mask) != 0,
            Err(_) => false,
        }
    }

    /// Adds volume `vol` to the set.
    pub const fn set(&mut self, vol: u32) -> Result<(), FormatError> {
        match Self::locate(vol) {
            Ok((word, mask)) => {
                self.words[word] |= mask;
                Ok(())
            }
            Err(e) => Err(e),
        }
    }

    /// Removes volume `vol` from the set.
    pub const fn clear(&mut self, vol: u32) -> Result<(), FormatError> {
        match Self::locate(vol) {
            Ok((word, mask)) => {
                self.words[word] &= !mask;
                Ok(())
            }
            Err(e) => Err(e),
        }
    }

    /// Adds every member of `other`.
    pub const fn union_with(&mut self, other: &Self) {
        let mut i = 0;
        while i < VOL_BITSET_WORDS {
            self.words[i] |= other.words[i];
            i += 1;
        }
    }

    /// The union of `self` and `other`.
    #[must_use]
    pub const fn union(&self, other: &Self) -> Self {
        let mut out = *self;
        out.union_with(other);
        out
    }

    /// Keeps only the members `other` also has.
    pub const fn intersect_with(&mut self, other: &Self) {
        let mut i = 0;
        while i < VOL_BITSET_WORDS {
            self.words[i] &= other.words[i];
            i += 1;
        }
    }

    /// The intersection of `self` and `other`.
    #[must_use]
    pub const fn intersection(&self, other: &Self) -> Self {
        let mut out = *self;
        out.intersect_with(other);
        out
    }

    /// `true` when every member of `self` is in `other`.
    #[must_use]
    pub const fn is_subset(&self, other: &Self) -> bool {
        let mut i = 0;
        while i < VOL_BITSET_WORDS {
            if self.words[i] & !other.words[i] != 0 {
                return false;
            }
            i += 1;
        }
        true
    }

    /// `true` when no volume is in the set.
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        let mut i = 0;
        while i < VOL_BITSET_WORDS {
            if self.words[i] != 0 {
                return false;
            }
            i += 1;
        }
        true
    }

    /// The number of volumes in the set.
    #[must_use]
    pub const fn count(&self) -> u32 {
        let mut n = 0;
        let mut i = 0;
        while i < VOL_BITSET_WORDS {
            n += self.words[i].count_ones();
            i += 1;
        }
        n
    }

    /// The highest volume index in the set.
    #[must_use]
    pub const fn highest(&self) -> Option<u32> {
        let mut i = VOL_BITSET_WORDS;
        while i > 0 {
            i -= 1;
            if self.words[i] != 0 {
                return Some((i as u32) * 64 + 63 - self.words[i].leading_zeros());
            }
        }
        None
    }

    /// The volume indices in the set, ascending.
    pub fn iter(&self) -> impl Iterator<Item = u32> + '_ {
        self.words.iter().enumerate().flat_map(|(wi, w)| {
            let mut bits = *w;
            core::iter::from_fn(move || {
                if bits == 0 {
                    None
                } else {
                    let tz = bits.trailing_zeros();
                    bits &= bits - 1;
                    Some((wi as u32) * 64 + tz)
                }
            })
        })
    }
}

impl fmt::Debug for VolBitSet {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_set().entries(self.iter()).finish()
    }
}

/// A reference from the superblock to a metadata region: its first physical
/// block, its length in 4 KiB pages and the CRC-32C over all of its bytes.
/// `pages == 0` means the region does not exist.
#[derive(
    FromBytes, IntoBytes, Immutable, KnownLayout, Clone, Copy, PartialEq, Eq, Hash, Default, Debug,
)]
#[repr(C)]
pub struct RootRef {
    /// First physical block of the region (PBA 0 is the superblock, never a root).
    pub pba: u64,
    /// Length of the region in 4 KiB pages; 0 means "none".
    pub pages: u32,
    /// CRC-32C over the whole region.
    pub crc32c: u32,
}

impl RootRef {
    /// The absent reference.
    pub const NONE: Self = Self {
        pba: 0,
        pages: 0,
        crc32c: 0,
    };

    /// A reference to `pages` pages at `pba` with checksum `crc32c`.
    #[must_use]
    pub const fn new(pba: u64, pages: u32, crc32c: u32) -> Self {
        Self { pba, pages, crc32c }
    }

    /// A reference to `region`, which must be a whole number of pages.
    pub fn for_region(pba: u64, region: &[u8]) -> Result<Self, FormatError> {
        if !region.len().is_multiple_of(BLOCK_SIZE) || region.len() / BLOCK_SIZE > u32::MAX as usize
        {
            return Err(bad_field("pages", region.len() as u64));
        }
        Ok(Self::new(
            pba,
            (region.len() / BLOCK_SIZE) as u32,
            crc32c(region),
        ))
    }

    /// `true` when the reference names no region.
    #[must_use]
    pub const fn is_none(&self) -> bool {
        self.pages == 0
    }

    /// Length of the region in bytes.
    #[must_use]
    pub const fn bytes(&self) -> u64 {
        self.pages as u64 * BLOCK_SIZE as u64
    }

    /// Checks that `region` is the referenced region: the right length and
    /// the stored CRC-32C. An absent reference verifies only an empty region.
    pub fn verify(&self, region: &[u8]) -> Result<(), FormatError> {
        let expected = self.bytes();
        if region.len() as u64 != expected {
            return Err(FormatError::BadLength {
                expected: usize::try_from(expected).unwrap_or(usize::MAX),
                found: region.len(),
            });
        }
        check_crc("root region", crc32c(region), self.crc32c)
    }
}

/// A 4 KiB buffer aligned to 4 KiB: a valid direct-I/O block and a valid
/// zero-copy backing for any 4 KiB structure.
#[derive(FromBytes, IntoBytes, Immutable, KnownLayout, Clone, Copy, PartialEq, Eq, Hash)]
#[repr(C, align(4096))]
pub struct Block4K(pub [u8; BLOCK_SIZE]);

impl Block4K {
    /// An all-zero block.
    #[must_use]
    pub const fn zeroed() -> Self {
        Self([0; BLOCK_SIZE])
    }
}

impl Default for Block4K {
    fn default() -> Self {
        Self::zeroed()
    }
}

impl fmt::Debug for Block4K {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Block4K({BLOCK_SIZE} bytes)")
    }
}

/// A 16 KiB buffer aligned to 4 KiB: backing for a map page, an index page or
/// a volume table.
#[derive(FromBytes, IntoBytes, Immutable, KnownLayout, Clone, Copy, PartialEq, Eq, Hash)]
#[repr(C, align(4096))]
pub struct Page16K(pub [u8; MAP_PAGE_SIZE]);

impl Page16K {
    /// An all-zero page.
    #[must_use]
    pub const fn zeroed() -> Self {
        Self([0; MAP_PAGE_SIZE])
    }
}

impl Default for Page16K {
    fn default() -> Self {
        Self::zeroed()
    }
}

impl fmt::Debug for Page16K {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Page16K({MAP_PAGE_SIZE} bytes)")
    }
}

const _: () = {
    assert!(size_of::<Uuid>() == 16);
    assert!(align_of::<Uuid>() == 1);
    assert!(size_of::<VolBitSet>() == 128);
    assert!(align_of::<VolBitSet>() == 8);
    assert!(offset_of!(VolBitSet, words) == 0);
    assert!(size_of::<RootRef>() == ROOT_REF_SIZE);
    assert!(align_of::<RootRef>() == 8);
    assert!(offset_of!(RootRef, pba) == 0);
    assert!(offset_of!(RootRef, pages) == 8);
    assert!(offset_of!(RootRef, crc32c) == 12);
    assert!(size_of::<Block4K>() == 4096);
    assert!(align_of::<Block4K>() == 4096);
    assert!(size_of::<Page16K>() == 16384);
    assert!(align_of::<Page16K>() == 4096);
};
