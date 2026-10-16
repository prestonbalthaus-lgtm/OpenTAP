//! The sink's durable record (Step 6C): what a replication destination
//! must remember across a daemon restart to resume a delta without
//! re-landing what landed, kept in the [`SinkStore`](crate::SinkStore)
//! (`dbladed`'s `--state-dir`), never in the pool.
//!
//! ```text
//! offset 0     magic           u32   "OTRS"
//! offset 4     version         u16   1
//! offset 6     flags           u16   bit 0: the live volume may hold a partial delta
//! offset 8     dst_agg         16 B  the destination pool
//! offset 24    dst_vol         u32   the destination volume's index
//! offset 28    src_vol         u32   the source volume's index on its pool
//! offset 32    src_agg         16 B  the source pool
//! offset 48    base            16 B  the committed base: the source snapshot the
//!                                    destination holds (nil: none yet)
//! offset 64    landing_target  16 B  the snapshot the delta in flight ends at
//! offset 80    landing_from    16 B  its base (nil for a full transfer)
//! offset 96    pool_cp         u64   the destination pool's CP when this was written
//! offset 104   pool_nonce      u64   that CP's nonce (0 in a record written before
//!                                    the Step 6 review: unknown)
//! offset 112   reserved        16 B  zero
//! offset 128   cursor          4096  the stream's CursorPage: the committed base's
//!                                    CP (source numbering) and the ResumeBitmap of
//!                                    the delta in flight, keyed by (cp_seq, nonce)
//! offset 4224  crc32c          u32   CRC-32C of bytes 0..4224
//! ```
//!
//! A record is written whole after the consistency point that made its
//! landed chunks durable, so it never claims more than the pool holds;
//! `pool_cp` and `pool_nonce` let a boot that rewound below that CP notice
//! that it does (a committed CP lost under a lying cache) and distrust the
//! landing ([`SinkRecord::outlived_by_rewind`]): a rewind of more than one
//! CP leaves the pool's CP below `pool_cp`, and a rewind of one commits the
//! boot under the lost CP's number with another nonce. `pool_nonce` took
//! reserved bytes, which every reader ignores, so the layout stays version
//! 1.

use otap_format::checksum::crc32c;
use otap_format::{IntoBytes, Uuid};
use otap_stream::{Cursor, CursorPage};

/// Record magic: `OTRS`.
pub const RECORD_MAGIC: u32 = u32::from_le_bytes(*b"OTRS");
/// Record layout version.
pub const RECORD_VERSION: u16 = 1;
/// Bytes of a record.
pub const RECORD_BYTES: usize = 4228;

const CURSOR_AT: usize = 128;
const CRC_AT: usize = CURSOR_AT + 4096;
const FLAG_DIRTY: u16 = 1;

/// The sink's resume state for one destination volume.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SinkRecord {
    /// The destination pool.
    pub dst_agg: Uuid,
    /// The destination volume's index.
    pub dst_vol: u32,
    /// The source pool (nil until a source has spoken).
    pub src_agg: Uuid,
    /// The source volume's index on its pool.
    pub src_vol: u32,
    /// The committed base: the source snapshot the destination holds.
    pub base: Uuid,
    /// The live volume may hold writes of a delta that did not commit: a
    /// delta of another identity must roll the volume back to the base
    /// first.
    pub dirty: bool,
    /// The snapshot the delta in flight ends at (nil when none).
    pub landing_target: Uuid,
    /// That delta's base (nil for a full transfer).
    pub landing_from: Uuid,
    /// The destination pool's committed CP when the record was written.
    pub pool_cp: u64,
    /// That CP's nonce; 0 when unknown (a record from before the field).
    pub pool_nonce: u64,
    /// The stream cursor: `applied_cp_seq` is the base's CP in the
    /// source's numbering, and `landing` the delta in flight.
    pub cursor: Cursor,
}

impl SinkRecord {
    /// A record for a destination that holds nothing yet.
    #[must_use]
    pub fn new(dst_agg: Uuid, dst_vol: u32) -> Self {
        Self {
            dst_agg,
            dst_vol,
            src_agg: Uuid::NIL,
            src_vol: 0,
            base: Uuid::NIL,
            dirty: false,
            landing_target: Uuid::NIL,
            landing_from: Uuid::NIL,
            pool_cp: 0,
            pool_nonce: 0,
            cursor: Cursor::new(0, 0, 0),
        }
    }

    /// `true` when the pool, committed at `cp_seq` with `cp_nonce`, no
    /// longer holds the CP this record was written after: a boot rewound
    /// below it (the pool's CP is lower), or rewound by one CP and
    /// committed the boot under the same number (another nonce). What the
    /// record says landed may then be gone, so its landing is not to be
    /// trusted (the Step 6 review's D3).
    #[must_use]
    pub fn outlived_by_rewind(&self, cp_seq: u64, cp_nonce: u64) -> bool {
        self.pool_cp > cp_seq
            || (self.pool_cp == cp_seq && self.pool_nonce != 0 && self.pool_nonce != cp_nonce)
    }

    /// Forgets the delta in flight: nothing of it counts as landed, and the
    /// live volume may hold part of it.
    pub fn forget_landing(&mut self) {
        self.cursor.abort();
        self.landing_target = Uuid::NIL;
        self.landing_from = Uuid::NIL;
        self.dirty = true;
    }

    /// The store key of the record of destination volume `vol`.
    #[must_use]
    pub fn key(vol: u32) -> String {
        format!("sink-{vol}")
    }

    /// The pipe identity the cursor page carries.
    fn pipe(&self) -> Uuid {
        super::pipe_id(self.src_agg, self.src_vol, self.dst_agg, self.dst_vol)
    }

    /// The record's bytes, sealed. Refuses a cursor the page would refuse.
    pub fn encode(&self) -> Result<Vec<u8>, String> {
        let mut cursor = self.cursor.clone();
        // Credits are a connection's, never a record's.
        cursor.credits = 0;
        let page: CursorPage = cursor
            .to_page(self.pipe())
            .map_err(|e| format!("the cursor cannot be written: {e}"))?;
        let mut out = vec![0u8; RECORD_BYTES];
        out[0..4].copy_from_slice(&RECORD_MAGIC.to_le_bytes());
        out[4..6].copy_from_slice(&RECORD_VERSION.to_le_bytes());
        let flags = if self.dirty { FLAG_DIRTY } else { 0 };
        out[6..8].copy_from_slice(&flags.to_le_bytes());
        out[8..24].copy_from_slice(self.dst_agg.as_array());
        out[24..28].copy_from_slice(&self.dst_vol.to_le_bytes());
        out[28..32].copy_from_slice(&self.src_vol.to_le_bytes());
        out[32..48].copy_from_slice(self.src_agg.as_array());
        out[48..64].copy_from_slice(self.base.as_array());
        out[64..80].copy_from_slice(self.landing_target.as_array());
        out[80..96].copy_from_slice(self.landing_from.as_array());
        out[96..104].copy_from_slice(&self.pool_cp.to_le_bytes());
        out[104..112].copy_from_slice(&self.pool_nonce.to_le_bytes());
        out[CURSOR_AT..CRC_AT].copy_from_slice(page.as_bytes());
        let crc = crc32c(&out[..CRC_AT]);
        out[CRC_AT..].copy_from_slice(&crc.to_le_bytes());
        Ok(out)
    }

    /// A record read back, verified: length, magic, version, CRC, the
    /// cursor page (its own checks) and that the page names this record's
    /// pipe. Reserved bytes are ignored, as on every on-disk structure.
    pub fn decode(bytes: &[u8]) -> Result<Self, String> {
        if bytes.len() != RECORD_BYTES {
            return Err(format!(
                "a sink record is {RECORD_BYTES} bytes, this one {}",
                bytes.len()
            ));
        }
        let magic = u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]);
        if magic != RECORD_MAGIC {
            return Err(format!("bad sink record magic {magic:#010x}"));
        }
        let version = u16::from_le_bytes([bytes[4], bytes[5]]);
        if version != RECORD_VERSION {
            return Err(format!(
                "sink record version {version}, expected {RECORD_VERSION}"
            ));
        }
        let stored = u32::from_le_bytes([
            bytes[CRC_AT],
            bytes[CRC_AT + 1],
            bytes[CRC_AT + 2],
            bytes[CRC_AT + 3],
        ]);
        let computed = crc32c(&bytes[..CRC_AT]);
        if stored != computed {
            return Err(format!(
                "sink record crc {computed:#010x}, the record says {stored:#010x}"
            ));
        }
        let uuid = |at: usize| {
            let mut b = [0u8; 16];
            b.copy_from_slice(&bytes[at..at + 16]);
            Uuid::from_bytes(b)
        };
        let u32_at = |at: usize| {
            u32::from_le_bytes([bytes[at], bytes[at + 1], bytes[at + 2], bytes[at + 3]])
        };
        let flags = u16::from_le_bytes([bytes[6], bytes[7]]);
        let mut pool_cp = [0u8; 8];
        pool_cp.copy_from_slice(&bytes[96..104]);
        let mut pool_nonce = [0u8; 8];
        pool_nonce.copy_from_slice(&bytes[104..112]);
        let page = CursorPage::read(&bytes[CURSOR_AT..CRC_AT])
            .map_err(|e| format!("the sink record's cursor page: {e}"))?;
        let cursor =
            Cursor::from_page(&page).map_err(|e| format!("the sink record's cursor: {e}"))?;
        let record = Self {
            dst_agg: uuid(8),
            dst_vol: u32_at(24),
            src_vol: u32_at(28),
            src_agg: uuid(32),
            base: uuid(48),
            dirty: flags & FLAG_DIRTY != 0,
            landing_target: uuid(64),
            landing_from: uuid(80),
            pool_cp: u64::from_le_bytes(pool_cp),
            pool_nonce: u64::from_le_bytes(pool_nonce),
            cursor,
        };
        if page.pipe_id != record.pipe() {
            return Err("the sink record's cursor page names another pipe".to_string());
        }
        Ok(record)
    }
}

const _: () = {
    assert!(CRC_AT + 4 == RECORD_BYTES);
    assert!(size_of::<CursorPage>() == 4096);
};
