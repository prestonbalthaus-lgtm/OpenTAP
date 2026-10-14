//! CRC-32C (Castagnoli, the iSCSI polynomial) and BLAKE3.
//!
//! CRC-32C covers every on-disk and on-wire structure (section 7.7, invariant
//! 3). The parameters are those of the iSCSI CRC (RFC 3720 appendix B.4):
//! polynomial `0x1EDC6F41`, reflected, initial value and final XOR
//! `0xFFFFFFFF`, so `crc32c(b"123456789") == 0xE3069283`. BLAKE3 is used for
//! per-delta digests and the golden-vector manifest.

use core::fmt;

use crc_fast::{CrcAlgorithm, Digest};

use crate::types::{check_crc, FormatError};

/// The CRC-32C generator polynomial (normal representation).
pub const CRC32C_POLY: u32 = 0x1EDC_6F41;
/// `crc32c(b"123456789")`, the standard check value.
pub const CRC32C_CHECK: u32 = 0xE306_9283;

/// CRC-32C of `data`.
#[inline]
#[must_use]
pub fn crc32c(data: &[u8]) -> u32 {
    crc_fast::crc32_iscsi(data)
}

/// The CRC-32C of `a || b` given `crc32c(a)`, `crc32c(b)` and `b.len()`,
/// without touching the data.
#[must_use]
pub fn crc32c_combine(crc_a: u32, crc_b: u32, len_b: u64) -> u32 {
    let combined = crc_fast::checksum_combine(
        CrcAlgorithm::Crc32Iscsi,
        u64::from(crc_a),
        u64::from(crc_b),
        len_b,
    );
    // A 32-bit CRC never sets the upper half of the u64 the library returns.
    combined as u32
}

/// Incremental CRC-32C over data presented in pieces.
pub struct Crc32c {
    digest: Digest,
}

impl Crc32c {
    /// A fresh computation over no bytes.
    #[must_use]
    pub fn new() -> Self {
        Self {
            digest: Digest::new(CrcAlgorithm::Crc32Iscsi),
        }
    }

    /// Feeds `data`.
    pub fn update(&mut self, data: &[u8]) {
        self.digest.update(data);
    }

    /// The CRC-32C of every byte fed so far. The computation may continue.
    #[must_use]
    pub fn finalize(&self) -> u32 {
        // A 32-bit CRC never sets the upper half of the u64 the library returns.
        self.digest.finalize() as u32
    }

    /// Bytes fed so far.
    #[must_use]
    pub fn len(&self) -> u64 {
        self.digest.get_amount()
    }

    /// `true` when no byte has been fed.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

impl Default for Crc32c {
    fn default() -> Self {
        Self::new()
    }
}

impl fmt::Debug for Crc32c {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Crc32c")
            .field("len", &self.len())
            .field("crc", &format_args!("{:#010x}", self.finalize()))
            .finish()
    }
}

/// CRC-32C of a sealed record, `bytes` being the whole record with its CRC
/// field in the last four bytes: the hash of everything before those bytes.
///
/// Returns `None` when `bytes` is shorter than four bytes.
#[must_use]
pub fn crc32c_of_sealed(bytes: &[u8]) -> Option<u32> {
    let body = bytes.len().checked_sub(4)?;
    Some(crc32c(&bytes[..body]))
}

/// The CRC stored in the last four bytes of a sealed record, little-endian.
#[must_use]
pub fn trailing_crc(bytes: &[u8]) -> Option<u32> {
    let body = bytes.len().checked_sub(4)?;
    let mut raw = [0u8; 4];
    raw.copy_from_slice(&bytes[body..]);
    Some(u32::from_le_bytes(raw))
}

/// Verifies a record whose CRC-32C is in its last four bytes and covers every
/// byte before them. A record shorter than four bytes fails with
/// [`FormatError::BadLength`] (`expected` is the four-byte minimum).
pub fn verify_trailing_crc(bytes: &[u8]) -> Result<(), FormatError> {
    match (crc32c_of_sealed(bytes), trailing_crc(bytes)) {
        (Some(computed), Some(stored)) => check_crc("trailing", computed, stored),
        _ => Err(FormatError::BadLength {
            expected: 4,
            found: bytes.len(),
        }),
    }
}

/// Writes the CRC-32C of `bytes[..len - 4]` into `bytes[len - 4..]`.
///
/// Returns the CRC written, or `None` (and writes nothing) when `bytes` is
/// shorter than four bytes.
#[must_use = "a record shorter than four bytes is left unsealed"]
pub fn seal_trailing_crc(bytes: &mut [u8]) -> Option<u32> {
    let body = bytes.len().checked_sub(4)?;
    let crc = crc32c(&bytes[..body]);
    bytes[body..].copy_from_slice(&crc.to_le_bytes());
    Some(crc)
}

/// BLAKE3-256 of `data`.
#[must_use]
pub fn blake3_256(data: &[u8]) -> [u8; 32] {
    *blake3::hash(data).as_bytes()
}

/// Incremental BLAKE3-256.
#[derive(Clone)]
pub struct Blake3 {
    hasher: blake3::Hasher,
}

impl Blake3 {
    /// A fresh hash over no bytes.
    #[must_use]
    pub fn new() -> Self {
        Self {
            hasher: blake3::Hasher::new(),
        }
    }

    /// Feeds `data`.
    pub fn update(&mut self, data: &[u8]) {
        self.hasher.update(data);
    }

    /// The digest of every byte fed so far. The computation may continue.
    #[must_use]
    pub fn finalize(&self) -> [u8; 32] {
        *self.hasher.finalize().as_bytes()
    }

    /// Bytes fed so far.
    #[must_use]
    pub fn len(&self) -> u64 {
        self.hasher.count()
    }

    /// `true` when no byte has been fed.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

impl Default for Blake3 {
    fn default() -> Self {
        Self::new()
    }
}

impl fmt::Debug for Blake3 {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Blake3").field("len", &self.len()).finish()
    }
}

/// Lower-case hex of a digest or any byte string.
#[must_use]
pub fn hex(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        s.push_str(&format!("{b:02x}"));
    }
    s
}
