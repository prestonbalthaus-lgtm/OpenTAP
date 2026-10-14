//! Adversarial and property tests for `otap-format`: the QA pass over the v2
//! format freeze and the v1 stream frame.
//!
//! These complement `format_tests.rs`. They tear slot, page and sector writes
//! against the F7 ring, probe alignment and length edges, feed degenerate and
//! cross-structure images to every parser, fuzz every parser for panics
//! against an independent oracle (the bitwise CRC-32C and the rule checks
//! below are written from the lib.rs rulings, not taken from the crate),
//! attack the packed fields, and pin determinism and the version policy.

use std::sync::{mpsc, OnceLock};
use std::thread;
use std::time::Duration;

use otap_format::checksum::{
    blake3_256, crc32c, crc32c_combine, crc32c_of_sealed, seal_trailing_crc, trailing_crc,
    verify_trailing_crc, Crc32c, CRC32C_CHECK, CRC32C_POLY,
};
use otap_format::*;
use proptest::prelude::*;

// ---------------------------------------------------------------------------
// Reference model. Layout facts and rules are copied from the lib.rs tables
// and rulings rather than read from the crate, so a silent change shows up
// here as a disagreement (`the_spec_constants_this_file_assumes` pins them).
// ---------------------------------------------------------------------------

/// The CRC-32C generator 0x1EDC6F41, bit-reflected.
const POLY_REFLECTED: u32 = 0x82F6_3B78;

const MAGIC_SB: u32 = u32::from_le_bytes(*b"OTSB");
const MAGIC_INTENT: u32 = u32::from_le_bytes(*b"OTIN");
const MAGIC_STRIPE: u32 = u32::from_le_bytes(*b"OTST");
const MAGIC_STREAM: u32 = u32::from_le_bytes(*b"OTSF");
/// Major version of the disk structures.
const DISK_MAJOR: u8 = 2;
/// Major version of the stream frame.
const WIRE_MAJOR: u8 = 1;
/// F5: the largest CP a header may carry.
const MAX_CP: u64 = 0xFFFF_FFFF;
/// F8: every volume index is below this bound.
const VOLS: u32 = 1024;
/// F3: the largest stripe, which is also the frame payload bound.
const WIRE_MAX: u32 = 8 << 20;

const SB_CP_SEQ_AT: usize = 32;
const SB_ACKED_AT: usize = 80;
const SB_VOL_COUNT_AT: usize = 88;
const SB_VOL_TABLE_CRC_AT: usize = 92;
const SB_WRITE_ACTIVE_AT: usize = 96;
const SB_NVFAIL_AT: usize = 224;
const SB_DURABLE_LOG_AT: usize = 480;
const INTENT_SEQ_AT: usize = 24;
const INTENT_CP_SEQ_AT: usize = 32;
const STRIPE_CP_SEQ_AT: usize = 24;
const STRIPE_LSEG_AT: usize = 40;
const STRIPE_NBLOCKS_AT: usize = 52;
const STRIPE_SUMMARY_CRC_AT: usize = 56;
const STREAM_CP_SEQ_AT: usize = 16;
const STREAM_BASE_AT: usize = 24;
const STREAM_WIRE_LEN_AT: usize = 48;
const STREAM_RAW_LEN_AT: usize = 52;

/// Bitwise reflected CRC-32C: initial value and final XOR 0xFFFFFFFF.
fn crc32c_bitwise(data: &[u8]) -> u32 {
    let mut crc = u32::MAX;
    for &byte in data {
        crc ^= u32::from(byte);
        for _ in 0..8 {
            crc = if crc & 1 == 1 {
                (crc >> 1) ^ POLY_REFLECTED
            } else {
                crc >> 1
            };
        }
    }
    !crc
}

/// The byte-at-a-time table of the same CRC, derived from the bitwise rule.
static CRC_TABLE: [u32; 256] = {
    let mut table = [0u32; 256];
    let mut i = 0;
    while i < 256 {
        let mut crc = i as u32;
        let mut bit = 0;
        while bit < 8 {
            crc = if crc & 1 == 1 {
                (crc >> 1) ^ POLY_REFLECTED
            } else {
                crc >> 1
            };
            bit += 1;
        }
        table[i] = crc;
        i += 1;
    }
    table
};

/// Table-driven form of `crc32c_bitwise`, fast enough for the fuzz oracle.
fn ref_crc(data: &[u8]) -> u32 {
    let mut crc = u32::MAX;
    for &byte in data {
        crc = CRC_TABLE[usize::from((crc as u8) ^ byte)] ^ (crc >> 8);
    }
    !crc
}

/// F9: the CRC of an all-zero 4 KiB data slot, which a hole descriptor carries.
fn zero_block_crc() -> u32 {
    static CRC: OnceLock<u32> = OnceLock::new();
    *CRC.get_or_init(|| ref_crc(&[0u8; 4096]))
}

/// The padded summary length: header and bitmap (192 bytes) plus 16 bytes
/// per descriptor, rounded up to 4 KiB.
fn ref_summary_bytes(nblocks: usize) -> usize {
    (192 + 16 * nblocks).div_ceil(4096) * 4096
}

/// The largest block count up to `u16::MAX` whose data and padded summary fit
/// in `stripe_bytes`, by binary search (the cost is monotonic in the count).
fn ref_max_blocks(stripe_bytes: usize) -> usize {
    let cost = |n: usize| n * 4096 + ref_summary_bytes(n);
    let cap = usize::from(u16::MAX);
    if cost(cap) <= stripe_bytes {
        return cap;
    }
    let (mut fits, mut too_big) = (0, cap);
    while too_big - fits > 1 {
        let mid = fits + (too_big - fits) / 2;
        if cost(mid) <= stripe_bytes {
            fits = mid;
        } else {
            too_big = mid;
        }
    }
    fits
}

fn le_u16(bytes: &[u8], at: usize) -> u16 {
    u16::from_le_bytes([bytes[at], bytes[at + 1]])
}

fn le_u32(bytes: &[u8], at: usize) -> u32 {
    u32::from_le_bytes([bytes[at], bytes[at + 1], bytes[at + 2], bytes[at + 3]])
}

fn le_u64(bytes: &[u8], at: usize) -> u64 {
    let mut word = [0u8; 8];
    word.copy_from_slice(&bytes[at..at + 8]);
    u64::from_le_bytes(word)
}

/// Bit `v` of a 128-byte volume bitset (bit `v % 64` of little-endian word `v / 64`).
fn bit(set: &[u8], v: u32) -> bool {
    (set[(v / 8) as usize] & (1 << (v % 8))) != 0
}

fn highest_bit(set: &[u8]) -> Option<u32> {
    (0..VOLS).rev().find(|&v| bit(set, v))
}

/// The refusal a record with `magic` at 0, `ver` at 4 and a trailing CRC must
/// get from the length, magic, major and CRC checks, in that order; `None`
/// when all four pass.
fn record_verdict(
    rec: &[u8],
    size: usize,
    magic: u32,
    major: u8,
    region: &'static str,
) -> Option<FormatError> {
    if rec.len() != size {
        return Some(FormatError::BadLength {
            expected: size,
            found: rec.len(),
        });
    }
    let found = le_u32(rec, 0);
    if found != magic {
        return Some(FormatError::BadMagic {
            expected: magic,
            found,
        });
    }
    if rec[5] != major {
        return Some(FormatError::BadVersion {
            expected_major: major,
            found: le_u16(rec, 4),
        });
    }
    let computed = ref_crc(&rec[..size - 4]);
    let stored = le_u32(rec, size - 4);
    if computed != stored {
        return Some(FormatError::BadCrc {
            region,
            computed,
            stored,
        });
    }
    None
}

/// The superblock page rules behind its CRC: run state, slot index, the CP
/// bound, slot parity (F7), `vol_count`, the bitsets below it, and the
/// durable-log reservation (all zero unless the kind is 1).
fn page_rules_ok(p: &[u8]) -> bool {
    let cp_seq = le_u64(p, SB_CP_SEQ_AT);
    let vol_count = le_u32(p, SB_VOL_COUNT_AT);
    let below = |at: usize| highest_bit(&p[at..at + 128]).is_none_or(|v| v < vol_count);
    let log = &p[SB_DURABLE_LOG_AT..SB_DURABLE_LOG_AT + 64];
    (1..=2).contains(&p[6])
        && p[7] < 4
        && cp_seq <= MAX_CP
        && u64::from(p[7] & 1) == (cp_seq & 1)
        && vol_count <= VOLS
        && below(SB_WRITE_ACTIVE_AT)
        && below(SB_NVFAIL_AT)
        && (log[0] == 1 || log.iter().all(|&b| b == 0))
}

/// The volume-table rules of a slot (F1): defined causes, absent rows at and
/// above `vol_count`, no bytes or cause on an absent volume, nvfail exactly
/// where a cause is, write-active only on present volumes, and the page's
/// `acked_bytes` equal to the sum of the rows.
fn table_rules_ok(x: &[u8]) -> bool {
    let page = &x[..4096];
    let vol_count = le_u32(page, SB_VOL_COUNT_AT);
    let mut sum: u64 = 0;
    for (row, vol) in x[4096..].as_chunks::<16>().0.iter().zip(0u32..) {
        let acked = le_u64(row, 0);
        let cause = row[12];
        let present = (row[13] & 1) == 1;
        if cause > 3
            || (vol >= vol_count && row.iter().any(|&b| b != 0))
            || (!present && (acked != 0 || cause != 0))
            || bit(&page[SB_NVFAIL_AT..], vol) != (cause != 0)
            || (bit(&page[SB_WRITE_ACTIVE_AT..], vol) && !present)
        {
            return false;
        }
        let Some(next) = sum.checked_add(acked) else {
            return false;
        };
        sum = next;
    }
    sum == le_u64(page, SB_ACKED_AT)
}

fn intent_rules_ok(p: &[u8]) -> bool {
    u64::from(p[6]) == (le_u64(p, INTENT_SEQ_AT) & 1) && le_u64(p, INTENT_CP_SEQ_AT) <= MAX_CP
}

fn stripe_hdr_rules_ok(p: &[u8]) -> bool {
    p[6] < 4
        && le_u64(p, STRIPE_LSEG_AT) != 0
        && le_u64(p, STRIPE_CP_SEQ_AT) <= MAX_CP
        && le_u16(p, STRIPE_NBLOCKS_AT) >= 1
}

fn stream_rules_ok(p: &[u8]) -> bool {
    let (wire, raw) = (le_u32(p, STREAM_WIRE_LEN_AT), le_u32(p, STREAM_RAW_LEN_AT));
    let (cp_seq, base) = (le_u64(p, STREAM_CP_SEQ_AT), le_u64(p, STREAM_BASE_AT));
    (1..=12).contains(&p[6])
        && p[7] < 3
        && wire <= WIRE_MAX
        && raw <= WIRE_MAX
        && (p[7] != 0 || raw == wire)
        && cp_seq <= MAX_CP
        && base <= cp_seq
}

/// F4, F8, F9: a defined kind, a volume below the bound, a birth, and the
/// zero-block CRC on a hole. The two reserved low bits are free.
fn descriptor_ok(d: &[u8; 16]) -> bool {
    let word = le_u64(d, 0);
    let kind = (word >> 2) & 0b11;
    kind < 2
        && (word >> 44) < u64::from(VOLS)
        && le_u32(d, 8) != 0
        && (kind == 0 || le_u32(d, 12) == zero_block_crc())
}

/// The bitmap a run of descriptors implies (F3).
fn ref_bitmap(descs: &[u8]) -> [u8; 128] {
    let mut map = [0u8; 128];
    for d in descs.as_chunks::<16>().0 {
        let vol = le_u64(d, 0) >> 44;
        if vol < u64::from(VOLS) {
            map[(vol / 8) as usize] |= 1 << (vol % 8);
        }
    }
    map
}

/// What `StripeSummary::parse` must answer for an 8-aligned `x`: `Ok` with the
/// padded length, an exact refusal for the length, magic, version and CRC
/// checks, or `Err(None)` for "a field rule fails" (any `BadField` or
/// `OutOfRange`).
fn summary_verdict(x: &[u8]) -> Result<usize, Option<FormatError>> {
    if x.len() < 64 {
        return Err(Some(FormatError::BadLength {
            expected: 64,
            found: x.len(),
        }));
    }
    let head = &x[..64];
    if let Some(refusal) = record_verdict(head, 64, MAGIC_STRIPE, DISK_MAJOR, "stripe header") {
        return Err(Some(refusal));
    }
    if !stripe_hdr_rules_ok(head) {
        return Err(None);
    }
    let n = usize::from(le_u16(x, STRIPE_NBLOCKS_AT));
    let total = ref_summary_bytes(n);
    if x.len() < total {
        return Err(Some(FormatError::BadLength {
            expected: total,
            found: x.len(),
        }));
    }
    let computed = ref_crc(&x[64..total]);
    let stored = le_u32(x, STRIPE_SUMMARY_CRC_AT);
    if computed != stored {
        return Err(Some(FormatError::BadCrc {
            region: "stripe summary",
            computed,
            stored,
        }));
    }
    let descs = &x[192..192 + 16 * n];
    if !descs.as_chunks::<16>().0.iter().all(descriptor_ok) || ref_bitmap(descs) != x[64..192] {
        return Err(None);
    }
    Ok(total)
}

// ---------------------------------------------------------------------------
// Buffers, samples and builders
// ---------------------------------------------------------------------------

/// Proptest's defaults (so `PROPTEST_CASES` still drives the merge gate),
/// without regression files and with bounded shrinking.
fn config() -> ProptestConfig {
    ProptestConfig {
        failure_persistence: None,
        max_shrink_iters: 4096,
        ..ProptestConfig::default()
    }
}

/// A zeroed heap buffer of `blocks` 4 KiB blocks, 4096-aligned.
fn aligned_blocks(blocks: usize) -> Vec<Block4K> {
    vec![Block4K::zeroed(); blocks]
}

/// Two 4096-aligned blocks on the stack.
#[repr(C, align(4096))]
struct Aligned8K([u8; 2 * BLOCK_SIZE]);

/// `input` copied into a zeroed aligned arena at byte `offset`; the copy is
/// `arena.as_bytes()[offset..offset + input.len()]`.
fn place(input: &[u8], offset: usize) -> Vec<Block4K> {
    let blocks = (offset + input.len()).div_ceil(BLOCK_SIZE).max(1);
    let mut arena = aligned_blocks(blocks);
    arena.as_mut_slice().as_mut_bytes()[offset..offset + input.len()].copy_from_slice(input);
    arena
}

fn agg(seed: u8) -> Uuid {
    Uuid::from_bytes([seed; 16])
}

/// Fills `bytes` with a pattern that has no zero byte.
fn fill_nonzero(bytes: &mut [u8], seed: u8) {
    for (i, b) in bytes.iter_mut().enumerate() {
        *b = ((i as u8).wrapping_mul(31) ^ seed) | 1;
    }
}

/// `old` with the `unit`-byte chunks selected by `mask` (bit `i` for chunk
/// `i`) taken from `new`: a write torn at `unit` granularity.
fn mix(old: &[u8], new: &[u8], unit: usize, mask: u64) -> Vec<u8> {
    assert_eq!(old.len(), new.len());
    let mut out = old.to_vec();
    for (i, (dst, src)) in out.chunks_mut(unit).zip(new.chunks(unit)).enumerate() {
        if (mask >> i) & 1 == 1 {
            dst.copy_from_slice(src);
        }
    }
    out
}

/// A slot torn at 4 KiB granularity: bit `i` of `mask` takes page `i` from `new`.
fn mix_slots(old: &SuperblockSlot, new: &SuperblockSlot, mask: u64) -> SuperblockSlot {
    SuperblockSlot::read_from_bytes(&mix(old.as_bytes(), new.as_bytes(), BLOCK_SIZE, mask)).unwrap()
}

/// The owner epoch of every commit in the ring tests.
const RING_EPOCH: u64 = 3;

/// A sealed slot holding commit `cp_seq`, written to `slot`. Every volume is
/// present and its record depends on `cp_seq`, so two commits differ in
/// every 4 KiB page.
fn commit_slot(agg_id: Uuid, cp_seq: u64, slot: SbSlot) -> SuperblockSlot {
    let mut s = SuperblockSlot::new(agg_id, cp_seq, slot.location());
    assert_eq!(
        s.sb.sb_slot(),
        Some(slot),
        "commit {cp_seq} is never written to {slot:?}"
    );
    s.sb.owner_epoch = RING_EPOCH;
    s.sb.cut_seq = cp_seq + 100;
    s.sb.cp_nonce = cp_seq.wrapping_mul(0x9E37_79B9_7F4A_7C15);
    s.sb.intent_seq_seen = cp_seq / 2;
    s.sb.wall_ns = 1_760_000_000_000_000_000 + cp_seq;
    s.sb.vol_count = MAX_VOLS as u32;
    for (vol, row) in (0u64..).zip(s.vols.vols.iter_mut()) {
        *row = VolState {
            acked_bytes: cp_seq * 4096 + vol,
            rollback_gen: cp_seq as u32,
            nvfail_cause: NvCause::None.as_u8(),
            flags: VOL_PRESENT,
            rsvd: 0,
        };
    }
    s.sb.write_active.set((cp_seq % 1024) as u32).unwrap();
    s.seal().unwrap();
    s
}

/// The ring as F7 leaves it just before commit `n` lands: the pair commit `n`
/// writes (`SbSlot::for_commit(n)`) still holds `n - 2`, the other pair `n - 1`.
fn ring_before_commit(agg_id: Uuid, n: u64) -> Vec<SuperblockSlot> {
    SbSlot::ALL
        .iter()
        .map(|&slot| {
            let cp = if SbSlot::for_commit(n).contains(&slot) {
                n - 2
            } else {
                n - 1
            };
            commit_slot(agg_id, cp, slot)
        })
        .collect()
}

/// The commit key `SuperblockSlot::select` picks from a ring laid out in
/// slot-index order, each slot read from its own location.
fn ring_select(agg_id: &Uuid, ring: &[SuperblockSlot]) -> Result<(u64, u64), FormatError> {
    SuperblockSlot::select(agg_id, SbSlot::ALL.into_iter().zip(ring))
        .map(|slot| slot.sb.commit_key())
}

/// Header (unsealed), bitmap and `n` descriptors of a stripe written by CP
/// `cp`. Stripes of different CPs differ in every 4 KiB page of the summary.
fn sample_stripe(n: usize, cp: u32) -> (StripeHdr, VolBitSet, Vec<StripeDescriptor>) {
    let mut hdr = StripeHdr::new(agg(7), StripeKind::Ingest);
    hdr.cp_seq = u64::from(cp);
    hdr.cp_nonce = u64::from(cp).wrapping_mul(0x9E37_79B9_7F4A_7C15);
    hdr.lseg = 17;
    hdr.stripe_idx = 3;
    hdr.nblocks = u16::try_from(n).unwrap();
    hdr.group = 1;
    let descs: Vec<StripeDescriptor> = (0..n)
        .map(|i| {
            let (vol, lba) = ((i % 5) as u32, 1000 + i as u64);
            let desc = if i % 7 == 6 {
                StripeDescriptor::hole(vol, lba, cp)
            } else {
                StripeDescriptor::data(vol, lba, cp, (i as u32) ^ cp.rotate_left(7))
            };
            desc.unwrap()
        })
        .collect();
    let mut bitmap = VolBitSet::new();
    for d in &descs {
        bitmap.set(d.vol()).unwrap();
    }
    (hdr, bitmap, descs)
}

/// A sealed `Chunk` frame header with a `payload_len`-byte raw payload.
fn sample_frame(payload_len: usize) -> (StreamHeader, Vec<u8>) {
    let payload: Vec<u8> = (0..payload_len).map(|i| (i * 7 % 251) as u8).collect();
    let mut hdr = StreamHeader::new(FrameKind::Chunk, Codec::Raw);
    hdr.rel = 2;
    hdr.chunk_idx = 9;
    hdr.cp_seq = 42;
    hdr.base = 40;
    hdr.cp_nonce = 0x0123_4567_89AB_CDEF;
    hdr.owner_epoch = RING_EPOCH;
    hdr.set_payload(&payload).unwrap();
    hdr.seal();
    (hdr, payload)
}

/// The slot of commit 42, built once.
fn sample_slot() -> &'static SuperblockSlot {
    static SLOT: OnceLock<SuperblockSlot> = OnceLock::new();
    SLOT.get_or_init(|| commit_slot(agg(8), 42, SbSlot::A0))
}

/// A sealed two-page summary of 300 blocks, built once.
fn sample_summary() -> &'static [u8] {
    static SUMMARY: OnceLock<Vec<u8>> = OnceLock::new();
    SUMMARY.get_or_init(|| {
        let (hdr, _, descs) = sample_stripe(300, 4);
        let mut image = vec![0u8; summary_bytes(300)];
        write_summary(&mut image, &hdr, &descs).unwrap();
        image
    })
}

/// Lays a summary out by hand (unsealed header, descriptors, zero padding;
/// the bitmap zone is left to the seal) and seals it with
/// `seal_summary_in_place`.
fn assemble_in_place(
    region: &mut [u8],
    hdr: &StripeHdr,
    descs: &[StripeDescriptor],
) -> Result<usize, FormatError> {
    let end = 192 + 16 * descs.len();
    let total = ref_summary_bytes(descs.len());
    region[..64].copy_from_slice(hdr.as_bytes());
    region[192..end].copy_from_slice(descs.as_bytes());
    region[end..total].fill(0);
    seal_summary_in_place(region)
}

/// Writes the reference CRC of everything before the last four bytes into them.
fn seal_tail(rec: &mut [u8]) {
    let body = rec.len() - 4;
    let crc = ref_crc(&rec[..body]);
    rec[body..].copy_from_slice(&crc.to_le_bytes());
}

/// Re-seals an edited image with the reference CRC, by the magic it carries,
/// so that edits of guarded fields reach the rules behind the CRCs.
fn reseal(img: &mut [u8]) {
    if img.len() < STRIPE_HDR_SIZE {
        return;
    }
    let magic = le_u32(img, 0);
    if magic == MAGIC_SB && img.len() == SUPERBLOCK_SLOT_SIZE {
        let table_crc = ref_crc(&img[SUPERBLOCK_SIZE..]);
        img[SB_VOL_TABLE_CRC_AT..SB_VOL_TABLE_CRC_AT + 4].copy_from_slice(&table_crc.to_le_bytes());
        seal_tail(&mut img[..SUPERBLOCK_SIZE]);
    } else if magic == MAGIC_STRIPE && img.len() >= BLOCK_SIZE {
        let total = ref_summary_bytes(usize::from(le_u16(img, STRIPE_NBLOCKS_AT)));
        if total <= img.len() {
            let summary_crc = ref_crc(&img[64..total]);
            img[STRIPE_SUMMARY_CRC_AT..STRIPE_SUMMARY_CRC_AT + 4]
                .copy_from_slice(&summary_crc.to_le_bytes());
        }
        seal_tail(&mut img[..STRIPE_HDR_SIZE]);
    } else {
        // A bare page or a 64-byte header: the CRC is in the last four bytes.
        seal_tail(img);
    }
}

// ---------------------------------------------------------------------------
// 1. Torn writes and the ring (F7)
// ---------------------------------------------------------------------------

#[test]
fn torn_slot_mixes_at_every_page_boundary_never_verify() {
    let agg_id = agg(1);
    let old = commit_slot(agg_id, 41, SbSlot::A1);
    let new = commit_slot(agg_id, 42, SbSlot::A0);
    for (a, b) in old
        .as_bytes()
        .chunks(BLOCK_SIZE)
        .zip(new.as_bytes().chunks(BLOCK_SIZE))
    {
        assert_ne!(a, b, "the commits must differ in every 4 KiB page");
    }
    let all = (1u64 << (SUPERBLOCK_SLOT_SIZE / BLOCK_SIZE)) - 1;
    for mask in 0..=all {
        let mixed = mix_slots(&old, &new, mask);
        let whole = mask == 0 || mask == all;
        assert_eq!(
            SuperblockSlot::parse(mixed.as_bytes()).is_ok(),
            whole,
            "parse, pages from the new commit {mask:#07b}"
        );
        let err = SuperblockSlot::read(mixed.as_bytes()).err();
        if whole {
            assert_eq!(err, None);
        } else {
            // The page is always one commit's whole page: the table CRC fails.
            assert!(
                matches!(
                    err,
                    Some(FormatError::BadCrc {
                        region: "volume table",
                        ..
                    })
                ),
                "pages {mask:#07b}: {err:?}"
            );
        }
    }
    // The two splices of page and table, with the CRCs the error must name.
    assert_eq!(
        SuperblockSlot::read(mix_slots(&old, &new, 0b00001).as_bytes()).err(),
        Some(FormatError::BadCrc {
            region: "volume table",
            computed: old.vols.crc32c(),
            stored: new.sb.vol_table_crc
        })
    );
    assert_eq!(
        SuperblockSlot::read(mix_slots(&old, &new, 0b11110).as_bytes()).err(),
        Some(FormatError::BadCrc {
            region: "volume table",
            computed: new.vols.crc32c(),
            stored: old.sb.vol_table_crc
        })
    );
}

#[test]
fn torn_sectors_inside_the_superblock_page_never_verify() {
    let agg_id = agg(1);
    let old = commit_slot(agg_id, 40, SbSlot::A0);
    let new = commit_slot(agg_id, 42, SbSlot::A0);
    // The first 512 bytes of the write landed, the rest is the old commit.
    let first_sector_new = mix(old.as_bytes(), new.as_bytes(), 512, 1);
    assert!(SuperblockSlot::read(&first_sector_new).is_err());
    assert!(Superblock::read(&first_sector_new[..SUPERBLOCK_SIZE]).is_err());
    // Every mix of the eight sectors of the page, under either table.
    for mask in 0..256u64 {
        let page = mix(old.sb.as_bytes(), new.sb.as_bytes(), 512, mask);
        let page_whole = page == old.sb.as_bytes() || page == new.sb.as_bytes();
        assert_eq!(
            Superblock::read(&page).is_ok(),
            page_whole,
            "sectors from the new page {mask:#010b}"
        );
        for table in [&old.vols, &new.vols] {
            let mut slot = SuperblockSlot::new_zeroed();
            slot.sb = Superblock::read_from_bytes(&page).unwrap();
            slot.vols = *table;
            let whole = slot == old || slot == new;
            assert_eq!(
                SuperblockSlot::parse(slot.as_bytes()).is_ok(),
                whole,
                "{mask:#010b}"
            );
            assert_eq!(
                SuperblockSlot::read(slot.as_bytes()).is_ok(),
                whole,
                "{mask:#010b}"
            );
        }
    }
}

#[test]
fn ring_falls_back_to_the_previous_commit_through_any_torn_write() {
    let agg_id = agg(4);
    for n in [10u64, 11] {
        let [first, second] = SbSlot::for_commit(n);
        let (i, j) = (usize::from(first.index()), usize::from(second.index()));
        let before = ring_before_commit(agg_id, n);
        assert_eq!(ring_select(&agg_id, &before), Ok((RING_EPOCH, n - 1)));
        let new_first = commit_slot(agg_id, n, first);
        let new_second = commit_slot(agg_id, n, second);
        let all = (1u64 << (SUPERBLOCK_SLOT_SIZE / BLOCK_SIZE)) - 1;
        for mask in 0..=all {
            let expect = Ok((RING_EPOCH, if mask == all { n } else { n - 1 }));
            // The first write tears; the second never starts.
            let mut ring = before.clone();
            ring[i] = mix_slots(&before[i], &new_first, mask);
            assert_eq!(
                ring_select(&agg_id, &ring),
                expect,
                "commit {n}, first write, pages {mask:#07b}"
            );
            // ... and a whole location is lost on top of it.
            for lost in [SbLocation::A, SbLocation::B] {
                let mut damaged = ring.clone();
                for slot in SbSlot::ALL.into_iter().filter(|s| s.location() == lost) {
                    damaged[usize::from(slot.index())] = SuperblockSlot::new_zeroed();
                }
                let survivor = if lost == first.location() {
                    Ok((RING_EPOCH, n - 1))
                } else {
                    expect
                };
                assert_eq!(
                    ring_select(&agg_id, &damaged),
                    survivor,
                    "commit {n}, location {lost:?} lost, pages {mask:#07b}"
                );
            }
            // The first write landed and the second tears: commit n is whole.
            let mut ring = before.clone();
            ring[i] = new_first;
            ring[j] = mix_slots(&before[j], &new_second, mask);
            assert_eq!(
                ring_select(&agg_id, &ring),
                Ok((RING_EPOCH, n)),
                "commit {n}, second write, pages {mask:#07b}"
            );
        }
        // A tear inside the first 4 KiB of the first write: the table is old.
        for sectors in 0..256u64 {
            let mut ring = before.clone();
            let page = mix(
                before[i].sb.as_bytes(),
                new_first.sb.as_bytes(),
                512,
                sectors,
            );
            ring[i].sb = Superblock::read_from_bytes(&page).unwrap();
            assert_eq!(
                ring_select(&agg_id, &ring),
                Ok((RING_EPOCH, n - 1)),
                "commit {n}, sectors {sectors:#010b}"
            );
        }
    }
}

/// The ring rule of F7 as `SuperblockSlot::rank` states it: only whole slots
/// count (a torn slot whose page verifies on its own does not), only slots of
/// the aggregate asked for, only at the location their page names; a newer
/// major anywhere is a hard stop; the `A` copy wins a tie.
#[test]
fn ring_selection_trusts_only_whole_slots_of_this_aggregate_at_their_location() {
    let agg_id = agg(5);
    let n = 12;
    let [first, _] = SbSlot::for_commit(n);
    let i = usize::from(first.index());
    let before = ring_before_commit(agg_id, n);

    // Commit n tore after its page: page n over table n - 2.
    let mut torn = before.clone();
    torn[i] = mix_slots(&before[i], &commit_slot(agg_id, n, first), 0b00001);
    assert!(Superblock::parse(torn[i].sb.as_bytes()).is_ok());
    assert!(matches!(
        SuperblockSlot::parse(torn[i].as_bytes()),
        Err(FormatError::BadCrc {
            region: "volume table",
            ..
        })
    ));
    assert_eq!(ring_select(&agg_id, &torn), Ok((RING_EPOCH, n - 1)));
    let ranked = SuperblockSlot::rank(&agg_id, SbSlot::ALL.into_iter().zip(&torn)).unwrap();
    let order: Vec<(u64, u8)> = ranked.iter().map(|s| (s.sb.cp_seq, s.sb.slot)).collect();
    assert_eq!(
        order,
        vec![
            (n - 1, SbSlot::A1.index()),
            (n - 1, SbSlot::B1.index()),
            (n - 2, SbSlot::B0.index())
        ]
    );

    // A tie between the A and B copies of one commit goes to A.
    let winner = SuperblockSlot::select(&agg_id, SbSlot::ALL.into_iter().zip(&before)).unwrap();
    assert!(std::ptr::eq(
        winner,
        &before[usize::from(SbSlot::A1.index())]
    ));

    // A newer commit of another aggregate never wins.
    let mut foreign = before.clone();
    foreign[i] = commit_slot(agg(6), n + 100, first);
    assert_eq!(ring_select(&agg_id, &foreign), Ok((RING_EPOCH, n - 1)));

    // A newer commit read from a location its page does not name is skipped.
    let mut misplaced = before.clone();
    misplaced[i] = commit_slot(agg_id, n + 2, SbSlot::B0);
    assert_eq!(ring_select(&agg_id, &misplaced), Ok((RING_EPOCH, n - 1)));

    // Nothing that verifies: no valid slot.
    let blank = vec![SuperblockSlot::new_zeroed(); SUPERBLOCK_SLOTS];
    assert_eq!(ring_select(&agg_id, &blank), Err(FormatError::NoValidSlot));
    assert_eq!(ring_select(&agg(9), &before), Err(FormatError::NoValidSlot));
}

#[test]
fn torn_stripe_summary_pages_never_verify() {
    let n = 600;
    let total = summary_bytes(n);
    assert_eq!(total, 3 * BLOCK_SIZE);
    let image = |cp| {
        let (hdr, _, descs) = sample_stripe(n, cp);
        let mut bytes = vec![0u8; total];
        write_summary(&mut bytes, &hdr, &descs).unwrap();
        bytes
    };
    let (old, new) = (image(41), image(42));
    for (a, b) in old.chunks(BLOCK_SIZE).zip(new.chunks(BLOCK_SIZE)) {
        assert_ne!(a, b, "the summaries must differ in every page");
    }
    for mask in 0..8u64 {
        let arena = place(&mix(&old, &new, BLOCK_SIZE, mask), 0);
        let whole = mask == 0 || mask == 0b111;
        assert_eq!(
            StripeSummary::parse(arena.as_slice().as_bytes()).is_ok(),
            whole,
            "pages from the new summary {mask:#05b}"
        );
    }
}

#[test]
fn torn_intent_writes_never_verify_and_never_reach_the_boot_union() {
    let agg_id = agg(6);
    let mut sb = Superblock::new(agg_id, 30, SbLocation::A);
    sb.vol_count = 8;
    sb.intent_seq_seen = 5;
    sb.write_active.set(1).unwrap();
    sb.seal();
    assert_eq!(sb.verify(), Ok(()));
    let intent = |seq: u64, vol: u32| {
        let mut slot = IntentSlot::new(agg_id, seq);
        slot.cp_seq = 30 + seq;
        slot.wall_ns = 1_760_000_000_000_000_000 + seq;
        slot.write_active.set(vol).unwrap();
        slot.seal();
        slot
    };
    let (old, other, new) = (intent(6, 2), intent(7, 3), intent(8, 4));
    for mask in 0..256u64 {
        let torn =
            IntentSlot::read_from_bytes(&mix(old.as_bytes(), new.as_bytes(), 512, mask)).unwrap();
        let (is_old, is_new) = (torn == old, torn == new);
        assert_eq!(
            torn.verify().is_ok(),
            is_old || is_new,
            "sectors from the new intent {mask:#010b}"
        );
        let mut expected = sb.write_active;
        expected.set(3).unwrap();
        if is_old {
            expected.set(2).unwrap();
        }
        if is_new {
            expected.set(4).unwrap();
        }
        assert_eq!(
            boot_write_active(&sb, [&torn, &other]),
            Ok(expected),
            "{mask:#010b}"
        );
    }
}

// ---------------------------------------------------------------------------
// 2. Alignment
// ---------------------------------------------------------------------------

#[test]
fn page_views_need_4096_alignment_and_copies_do_not() {
    let mut sb = Superblock::new(agg(2), 7, SbLocation::B);
    sb.seal();
    let mut intent = IntentSlot::new(agg(2), 3);
    intent.seal();
    for off in [1usize, 8, 512, 4095] {
        let mut buf = Aligned8K([0xA5; 2 * BLOCK_SIZE]);
        buf.0[off..off + SUPERBLOCK_SIZE].copy_from_slice(sb.as_bytes());
        let x = &buf.0[off..off + SUPERBLOCK_SIZE];
        assert_eq!(
            Superblock::parse(x).unwrap_err(),
            FormatError::Misaligned { required: 4096 },
            "offset {off}"
        );
        assert_eq!(Superblock::read(x).unwrap(), sb, "offset {off}");
        buf.0[off..off + INTENT_SLOT_SIZE].copy_from_slice(intent.as_bytes());
        let x = &buf.0[off..off + INTENT_SLOT_SIZE];
        assert_eq!(
            IntentSlot::parse(x).unwrap_err(),
            FormatError::Misaligned { required: 4096 },
            "offset {off}"
        );
        assert_eq!(IntentSlot::read(x).unwrap(), intent, "offset {off}");
    }
    let mut buf = Aligned8K([0xA5; 2 * BLOCK_SIZE]);
    for off in [0usize, BLOCK_SIZE] {
        buf.0[off..off + SUPERBLOCK_SIZE].copy_from_slice(sb.as_bytes());
        assert_eq!(
            Superblock::parse(&buf.0[off..off + SUPERBLOCK_SIZE]).unwrap(),
            &sb
        );
    }
    // A whole slot at the same offsets inside a 24 KiB aligned buffer.
    let slot = commit_slot(agg(2), 8, SbSlot::A0);
    for off in [0usize, 1, 8, 512, 4095, 4096] {
        let mut arena = aligned_blocks(6);
        let bytes = arena.as_mut_slice().as_mut_bytes();
        bytes[off..off + SUPERBLOCK_SLOT_SIZE].copy_from_slice(slot.as_bytes());
        let x = &bytes[off..off + SUPERBLOCK_SLOT_SIZE];
        assert_eq!(SuperblockSlot::read(x).unwrap(), slot, "offset {off}");
        let view = SuperblockSlot::parse(x);
        if off.is_multiple_of(BLOCK_SIZE) {
            assert_eq!(view.unwrap(), &slot);
        } else {
            assert_eq!(
                view.unwrap_err(),
                FormatError::Misaligned { required: 4096 },
                "offset {off}"
            );
        }
    }
}

#[test]
fn headers_view_at_eight_byte_alignment_and_copy_at_any_offset() {
    let (frame, _) = sample_frame(100);
    let stripe_hdr = StripeHdr::read(&sample_summary()[..STRIPE_HDR_SIZE]).unwrap();
    let mut buf = Block4K::zeroed();
    for off in 0..=64usize {
        buf.0.fill(0x5A);
        buf.0[off..off + STREAM_HEADER_SIZE].copy_from_slice(frame.as_bytes());
        let x = &buf.0[off..off + STREAM_HEADER_SIZE];
        assert_eq!(StreamHeader::read(x).unwrap(), frame, "offset {off}");
        if off.is_multiple_of(8) {
            assert_eq!(StreamHeader::parse(x).unwrap(), &frame, "offset {off}");
        } else {
            assert_eq!(
                StreamHeader::parse(x).unwrap_err(),
                FormatError::Misaligned { required: 8 },
                "offset {off}"
            );
        }
        buf.0[off..off + STRIPE_HDR_SIZE].copy_from_slice(stripe_hdr.as_bytes());
        let x = &buf.0[off..off + STRIPE_HDR_SIZE];
        assert_eq!(StripeHdr::read(x).unwrap(), stripe_hdr, "offset {off}");
        if off.is_multiple_of(8) {
            assert_eq!(StripeHdr::parse(x).unwrap(), &stripe_hdr, "offset {off}");
        } else {
            assert_eq!(
                StripeHdr::parse(x).unwrap_err(),
                FormatError::Misaligned { required: 8 },
                "offset {off}"
            );
        }
    }
}

#[test]
fn stripe_summary_needs_eight_byte_alignment_only() {
    let image = sample_summary();
    let total = image.len();
    let (_, bitmap, descs) = sample_stripe(300, 4);
    for off in [
        0usize, 1, 2, 3, 4, 5, 6, 7, 8, 9, 12, 16, 20, 24, 40, 512, 4095, 4096, 4100, 4104,
    ] {
        let arena = place(image, off);
        let x = &arena.as_slice().as_bytes()[off..off + total];
        let parsed = StripeSummary::parse(x);
        if off.is_multiple_of(8) {
            let summary = parsed.unwrap_or_else(|e| panic!("offset {off}: {e}"));
            assert_eq!(summary.descriptors(), &descs[..]);
            assert_eq!(*summary.vol_bitmap(), bitmap);
        } else {
            assert_eq!(
                parsed.unwrap_err(),
                FormatError::Misaligned { required: 8 },
                "offset {off}"
            );
        }
    }
}

// ---------------------------------------------------------------------------
// 3. Extremes
// ---------------------------------------------------------------------------

#[test]
fn summary_with_u16_max_descriptors_round_trips() {
    let n = usize::from(u16::MAX);
    let total = summary_bytes(n);
    assert_eq!(total, 257 * BLOCK_SIZE);
    let descs: Vec<StripeDescriptor> = (0..n)
        .map(|i| {
            let (vol, lba) = ((i % MAX_VOLS) as u32, MAX_LBA - i as u64);
            let desc = if i.is_multiple_of(3) {
                StripeDescriptor::hole(vol, lba, u32::MAX)
            } else {
                StripeDescriptor::data(vol, lba, u32::MAX, i as u32)
            };
            desc.unwrap()
        })
        .collect();
    let mut hdr = StripeHdr::new(agg(3), StripeKind::Relocate);
    hdr.nblocks = u16::MAX;
    hdr.cp_seq = MAX_CP_SEQ;
    hdr.lseg = MAX_LSEG;
    hdr.stripe_idx = u32::MAX;
    let mut arena = aligned_blocks(total / BLOCK_SIZE);
    let region = arena.as_mut_slice().as_mut_bytes();
    assert_eq!(region.len(), total);
    assert_eq!(write_summary(region, &hdr, &descs), Ok(total));
    {
        let summary = StripeSummary::parse(region).unwrap();
        assert_eq!(summary.hdr().nblocks, u16::MAX);
        assert_eq!(summary.descriptors(), &descs[..]);
        assert_eq!(summary.summary_bytes(), total);
        assert_eq!(summary.hdr().data_bytes(), n * BLOCK_SIZE);
        // Every volume index occurs, so the derived bitmap is full.
        assert_eq!(
            *summary.vol_bitmap(),
            VolBitSet {
                words: [u64::MAX; VOL_BITSET_WORDS]
            }
        );
    }
    assert_eq!(
        StripeSummary::parse(&region[..total - 1]).unwrap_err(),
        FormatError::BadLength {
            expected: total,
            found: total - 1
        }
    );
    // One descriptor more than `nblocks` can count is refused before any write.
    let too_many = vec![StripeDescriptor::default(); n + 1];
    let mut bigger = aligned_blocks(258);
    let region = bigger.as_mut_slice().as_mut_bytes();
    assert_eq!(
        write_summary(region, &hdr, &too_many),
        Err(FormatError::OutOfRange {
            field: "nblocks",
            value: (n + 1) as u64,
            max: u64::from(u16::MAX)
        })
    );
    assert!(region.iter().all(|&b| b == 0));
}

#[test]
fn summary_bytes_matches_the_reference_for_every_block_count() {
    assert_eq!(summary_bytes(0), BLOCK_SIZE);
    assert_eq!(summary_bytes(1), BLOCK_SIZE);
    assert_eq!(summary_bytes(244), BLOCK_SIZE);
    assert_eq!(summary_bytes(245), 2 * BLOCK_SIZE);
    for n in 0..=usize::from(u16::MAX) {
        assert_eq!(summary_bytes(n), ref_summary_bytes(n), "nblocks {n}");
        assert_eq!(descriptor_offset(n), 192 + 16 * n, "nblocks {n}");
    }
    // The descriptors end exactly on a page boundary at 256 k - 12 blocks.
    for k in 1..=256usize {
        let exact = 256 * k - 12;
        assert_eq!(summary_bytes(exact), k * BLOCK_SIZE, "nblocks {exact}");
        assert_eq!(
            summary_bytes(exact + 1),
            (k + 1) * BLOCK_SIZE,
            "nblocks {}",
            exact + 1
        );
    }
    // Absurd counts saturate instead of overflowing.
    assert_eq!(summary_bytes(usize::MAX), usize::MAX);
    assert_eq!(descriptor_offset(usize::MAX), usize::MAX);
    let wild = SummaryLayout {
        stripe_bytes: 0,
        nblocks: usize::MAX,
        summary_bytes: usize::MAX,
    };
    assert_eq!(wild.block_offset(usize::MAX), usize::MAX);
    assert_eq!(wild.slack_bytes(), 0);
}

#[test]
fn summary_layout_matches_an_independent_search_over_its_whole_range() {
    // F3: a stripe is a multiple of 4 KiB from 8 KiB (one summary page and one
    // data block) to 8 MiB.
    for blocks in 0..=2048 + 64 {
        let stripe = blocks * BLOCK_SIZE;
        let layout = SummaryLayout::for_stripe(stripe);
        if !(2..=2048).contains(&blocks) {
            assert_eq!(
                layout,
                Err(FormatError::BadField {
                    field: "stripe_bytes",
                    value: stripe as u64
                }),
                "stripe of {blocks} blocks"
            );
            continue;
        }
        let layout = layout.unwrap();
        let n = ref_max_blocks(stripe);
        assert_eq!(layout.nblocks, n, "stripe of {blocks} blocks");
        assert_eq!(layout.stripe_bytes, stripe);
        assert_eq!(layout.summary_bytes, ref_summary_bytes(n));
        assert_eq!(
            layout.slack_bytes(),
            stripe - n * BLOCK_SIZE - ref_summary_bytes(n)
        );
        assert!(
            layout.slack_bytes() < 2 * BLOCK_SIZE,
            "stripe of {blocks} blocks"
        );
        assert!(SummaryLayout::fits(stripe, n));
        assert!(!SummaryLayout::fits(stripe, n + 1));
    }
    for stripe in [8193usize, 12_345, (8 << 20) - 1] {
        assert_eq!(
            SummaryLayout::for_stripe(stripe),
            Err(FormatError::BadField {
                field: "stripe_bytes",
                value: stripe as u64
            })
        );
    }
}

#[test]
fn summary_layout_refuses_stripes_outside_8_kib_to_8_mib() {
    let largest = SummaryLayout::for_stripe(8 << 20).unwrap();
    assert_eq!(
        (
            largest.nblocks,
            largest.summary_bytes,
            largest.slack_bytes()
        ),
        (2039, 9 * BLOCK_SIZE, 0)
    );
    let smallest = SummaryLayout::for_stripe(8192).unwrap();
    assert_eq!(
        (
            smallest.nblocks,
            smallest.summary_bytes,
            smallest.slack_bytes()
        ),
        (1, BLOCK_SIZE, 0)
    );
    for stripe in [
        0usize,
        BLOCK_SIZE,
        (8 << 20) + BLOCK_SIZE,
        256 << 20,
        1 << 30,
        usize::MAX & !4095,
        usize::MAX,
    ] {
        assert_eq!(
            SummaryLayout::for_stripe(stripe),
            Err(FormatError::BadField {
                field: "stripe_bytes",
                value: stripe as u64
            }),
            "stripe {stripe}"
        );
    }
    // `fits` never overflows, whatever it is asked.
    assert!(!SummaryLayout::fits(usize::MAX, usize::MAX));
    assert!(!SummaryLayout::fits(usize::MAX, usize::from(u16::MAX) + 1));
    assert!(SummaryLayout::fits(usize::MAX, usize::from(u16::MAX)));
}

#[test]
fn summary_layout_answers_any_size_immediately() {
    // The downward search used to step one block at a time from an estimate
    // that ignored the u16 cap (2 TiB took seconds); the size bound and the
    // clamp make every call O(1), and the function is usable in a const.
    const LARGEST: Result<SummaryLayout, FormatError> = SummaryLayout::for_stripe(8 << 20);
    let sizes = [1usize << 41, usize::MAX & !4095, 8 << 20];
    let (tx, rx) = mpsc::channel();
    let _worker = thread::spawn(move || {
        // The send fails only when the test has already given up.
        let _ = tx.send(sizes.map(SummaryLayout::for_stripe));
    });
    let Ok(results) = rx.recv_timeout(Duration::from_secs(2)) else {
        panic!("SummaryLayout::for_stripe did not return within 2 s");
    };
    for (stripe, result) in sizes.iter().zip(results).take(2) {
        assert_eq!(
            result,
            Err(FormatError::BadField {
                field: "stripe_bytes",
                value: *stripe as u64
            })
        );
    }
    assert_eq!(results[2], LARGEST);
    assert_eq!(results[2].map(|layout| layout.nblocks), Ok(2039));
}

// ---------------------------------------------------------------------------
// 4. Degenerate and confused images
// ---------------------------------------------------------------------------

#[test]
fn all_zero_and_all_ones_images_are_refused() {
    for fill in [0x00u8, 0xFF] {
        let found = u32::from_le_bytes([fill; 4]);
        let bad = |expected| FormatError::BadMagic { expected, found };
        let page = Block4K([fill; BLOCK_SIZE]);

        let sb = Superblock::read_from_bytes(&page.0).unwrap();
        assert!(!sb.is_newer_format());
        assert_eq!(sb.verify(), Err(bad(SUPERBLOCK_MAGIC)), "fill {fill:#04x}");
        assert_eq!(
            Superblock::parse(&page.0).unwrap_err(),
            bad(SUPERBLOCK_MAGIC)
        );
        assert_eq!(
            Superblock::read(&page.0).unwrap_err(),
            bad(SUPERBLOCK_MAGIC)
        );

        let intent = IntentSlot::read_from_bytes(&page.0).unwrap();
        assert_eq!(intent.verify(), Err(bad(INTENT_MAGIC)));
        assert_eq!(IntentSlot::parse(&page.0).unwrap_err(), bad(INTENT_MAGIC));
        assert_eq!(IntentSlot::read(&page.0).unwrap_err(), bad(INTENT_MAGIC));
        let mut live = Superblock::new(agg(1), 0, SbLocation::A);
        live.vol_count = 16;
        live.write_active.set(9).unwrap();
        live.seal();
        assert_eq!(
            boot_write_active(&live, [&intent, &intent]),
            Ok(live.write_active)
        );

        let mut arena = aligned_blocks(SUPERBLOCK_SLOT_SIZE / BLOCK_SIZE);
        arena.as_mut_slice().as_mut_bytes().fill(fill);
        let slot_bytes = arena.as_slice().as_bytes();
        assert_eq!(
            SuperblockSlot::parse(slot_bytes).unwrap_err(),
            bad(SUPERBLOCK_MAGIC)
        );
        assert_eq!(
            SuperblockSlot::read(slot_bytes).unwrap_err(),
            bad(SUPERBLOCK_MAGIC)
        );
        let slot = SuperblockSlot::ref_from_bytes(slot_bytes).unwrap();
        assert_eq!(
            SuperblockSlot::select(&agg(1), SbSlot::ALL.map(|s| (s, slot))),
            Err(FormatError::NoValidSlot)
        );

        let head = &page.0[..STRIPE_HDR_SIZE];
        assert_eq!(StripeHdr::parse(head).unwrap_err(), bad(STRIPE_MAGIC));
        assert_eq!(StripeHdr::read(head).unwrap_err(), bad(STRIPE_MAGIC));
        assert_eq!(
            StripeHdr::read_from_bytes(head).unwrap().verify_hdr(),
            Err(bad(STRIPE_MAGIC))
        );
        assert_eq!(StreamHeader::parse(head).unwrap_err(), bad(STREAM_MAGIC));
        assert_eq!(StreamHeader::read(head).unwrap_err(), bad(STREAM_MAGIC));
        assert_eq!(
            StreamHeader::read_from_bytes(head).unwrap().verify(),
            Err(bad(STREAM_MAGIC))
        );

        let region = Page16K([fill; MAP_PAGE_SIZE]);
        assert_eq!(
            StripeSummary::parse(&region.0).unwrap_err(),
            bad(STRIPE_MAGIC)
        );
        let mut scratch = region;
        assert_eq!(
            seal_summary_in_place(&mut scratch.0),
            Err(bad(STRIPE_MAGIC))
        );
        assert_eq!(scratch, region, "a refused seal writes nothing");
    }

    // The headerless records and pages: all-zero means "absent" and is valid,
    // all-ones is refused by a field rule.
    let zeros = Page16K::zeroed();
    let ones = Page16K([0xFF; MAP_PAGE_SIZE]);
    let field = |field, value| FormatError::BadField { field, value };
    let vol_range = FormatError::OutOfRange {
        field: "vol",
        value: u64::from(u32::MAX),
        max: 1023,
    };
    let map = |p: &Page16K| *MapPage::ref_from_bytes(&p.0).unwrap();
    assert_eq!(map(&zeros).validate(), Ok(()));
    assert_eq!(map(&ones).validate(), Err(field("plen", u64::MAX)));
    assert_eq!(
        MapPage::parse(&ones.0, &map(&ones).index_entry(77)),
        Err(field("plen", u64::MAX))
    );
    assert_eq!(
        MapPage::parse(&zeros.0, &map(&zeros).index_entry(77)).map(|p| p.max_birth()),
        Ok(0)
    );
    let index = |p: &Page16K| *PageIndexPage::ref_from_bytes(&p.0).unwrap();
    assert_eq!(index(&zeros).validate(), Ok(()));
    assert_eq!(index(&ones).validate(), Ok(()));
    let root = |fill: u8| MapRoot::read_from_bytes(&[fill; MAP_ROOT_SIZE]).unwrap();
    assert_eq!(root(0).validate(), Ok(()));
    assert_eq!(root(0xFF).validate(), Err(vol_range));
    let hole = |fill: u8| HoleRecord::read_from_bytes(&[fill; HOLE_RECORD_SIZE]).unwrap();
    assert_eq!(hole(0).validate(), Err(field("len", 0)));
    assert_eq!(hole(0xFF).validate(), Err(vol_range));
    let log = |fill: u8| DurableLogHdr::read_from_bytes(&[fill; DURABLE_LOG_HDR_SIZE]).unwrap();
    assert_eq!(log(0).validate(), Ok(()));
    assert_eq!(log(0xFF).validate(), Err(field("durable_log.kind", 0xFF)));
    let desc =
        |fill: u8| StripeDescriptor::read_from_bytes(&[fill; STRIPE_DESCRIPTOR_SIZE]).unwrap();
    assert_eq!(desc(0).validate(), Err(field("birth_cp", 0)));
    assert_eq!(desc(0xFF).validate(), Err(field("kind", 3)));
}

#[test]
fn checks_run_in_the_order_magic_version_crc_fields() {
    let mut sb = Superblock::new_zeroed();
    sb.ver = 0x0300;
    sb.run_state = 9;
    sb.crc32c = 0xDEAD_BEEF;
    assert_eq!(
        sb.verify(),
        Err(FormatError::BadMagic {
            expected: SUPERBLOCK_MAGIC,
            found: 0
        })
    );
    sb.magic = SUPERBLOCK_MAGIC;
    assert_eq!(
        sb.verify(),
        Err(FormatError::BadVersion {
            expected_major: FORMAT_MAJOR,
            found: 0x0300
        })
    );
    sb.ver = FORMAT_VERSION;
    assert_eq!(
        sb.verify(),
        Err(FormatError::BadCrc {
            region: "superblock page",
            computed: ref_crc(&sb.as_bytes()[..4092]),
            stored: 0xDEAD_BEEF
        })
    );
    let sb_after = |sb: &mut Superblock| {
        sb.seal();
        sb.verify()
    };
    let field = |field, value| Err(FormatError::BadField { field, value });
    let range = |field, value, max| Err(FormatError::OutOfRange { field, value, max });
    assert_eq!(sb_after(&mut sb), field("run_state", 9));
    sb.run_state = RunState::CleanShutdown.as_u8();
    sb.slot = 7;
    assert_eq!(sb_after(&mut sb), field("slot", 7));
    sb.slot = SbSlot::B1.index();
    assert_eq!(sb_after(&mut sb), field("slot", 3), "slot parity");
    sb.cp_seq = MAX_CP_SEQ + 2;
    assert_eq!(
        sb_after(&mut sb),
        range("cp_seq", MAX_CP_SEQ + 2, MAX_CP_SEQ)
    );
    sb.cp_seq = 1;
    sb.vol_count = u32::MAX;
    assert_eq!(
        sb_after(&mut sb),
        range("vol_count", u64::from(u32::MAX), MAX_VOLS as u64)
    );
    sb.vol_count = 4;
    sb.write_active.set(4).unwrap();
    assert_eq!(sb_after(&mut sb), field("write_active", 4));
    sb.write_active.clear(4).unwrap();
    sb.write_active.set(3).unwrap();
    sb.nvfail.set(9).unwrap();
    assert_eq!(sb_after(&mut sb), field("nvfail", 9));
    sb.nvfail.clear(9).unwrap();
    sb.durable_log.kind = 5;
    assert_eq!(sb_after(&mut sb), field("durable_log.kind", 5));
    sb.durable_log.kind = DURABLE_LOG_NONE;
    sb.durable_log.ring_pba = 1;
    assert_eq!(sb_after(&mut sb), field("durable_log", 0));
    sb.durable_log = DurableLogHdr::NONE;
    assert_eq!(sb_after(&mut sb), Ok(()));

    let mut intent = IntentSlot::new_zeroed();
    intent.ver = 0x0100;
    intent.slot = 2;
    intent.crc32c = 3;
    assert_eq!(
        intent.verify(),
        Err(FormatError::BadMagic {
            expected: INTENT_MAGIC,
            found: 0
        })
    );
    intent.magic = INTENT_MAGIC;
    assert_eq!(
        intent.verify(),
        Err(FormatError::BadVersion {
            expected_major: FORMAT_MAJOR,
            found: 0x0100
        })
    );
    intent.ver = FORMAT_VERSION;
    assert_eq!(
        intent.verify(),
        Err(FormatError::BadCrc {
            region: "intent slot",
            computed: ref_crc(&intent.as_bytes()[..4092]),
            stored: 3
        })
    );
    let intent_after = |intent: &mut IntentSlot| {
        intent.seal();
        intent.verify()
    };
    assert_eq!(intent_after(&mut intent), field("slot", 2));
    intent.slot = 1;
    assert_eq!(intent_after(&mut intent), field("slot", 1), "slot parity");
    intent.intent_seq = 1;
    intent.cp_seq = MAX_CP_SEQ + 1;
    assert_eq!(
        intent_after(&mut intent),
        range("cp_seq", MAX_CP_SEQ + 1, MAX_CP_SEQ)
    );
    intent.cp_seq = 0;
    assert_eq!(intent_after(&mut intent), Ok(()));

    let mut hdr = StripeHdr::new_zeroed();
    hdr.ver = 0x0100;
    hdr.kind = 4;
    hdr.hdr_crc = 5;
    assert_eq!(
        hdr.verify_hdr(),
        Err(FormatError::BadMagic {
            expected: STRIPE_MAGIC,
            found: 0
        })
    );
    hdr.magic = STRIPE_MAGIC;
    assert_eq!(
        hdr.verify_hdr(),
        Err(FormatError::BadVersion {
            expected_major: FORMAT_MAJOR,
            found: 0x0100
        })
    );
    hdr.ver = FORMAT_VERSION;
    assert_eq!(
        hdr.verify_hdr(),
        Err(FormatError::BadCrc {
            region: "stripe header",
            computed: ref_crc(&hdr.as_bytes()[..60]),
            stored: 5
        })
    );
    let hdr_after = |hdr: &mut StripeHdr| {
        hdr.seal_hdr();
        hdr.verify_hdr()
    };
    assert_eq!(hdr_after(&mut hdr), field("kind", 4));
    hdr.kind = StripeKind::Merge.as_u8();
    assert_eq!(hdr_after(&mut hdr), field("lseg", 0));
    hdr.lseg = 1;
    hdr.cp_seq = MAX_CP_SEQ + 1;
    assert_eq!(
        hdr_after(&mut hdr),
        range("cp_seq", MAX_CP_SEQ + 1, MAX_CP_SEQ)
    );
    hdr.cp_seq = 0;
    assert_eq!(hdr_after(&mut hdr), field("nblocks", 0));
    hdr.nblocks = 1;
    assert_eq!(hdr_after(&mut hdr), Ok(()));

    let mut frame = StreamHeader::new_zeroed();
    frame.ver = 0x0200;
    frame.flags = 0xFF;
    frame.wire_len = u32::MAX;
    frame.hdr_crc = 1;
    assert_eq!(
        frame.verify(),
        Err(FormatError::BadMagic {
            expected: STREAM_MAGIC,
            found: 0
        })
    );
    frame.magic = STREAM_MAGIC;
    assert_eq!(
        frame.verify(),
        Err(FormatError::BadVersion {
            expected_major: STREAM_MAJOR,
            found: 0x0200
        })
    );
    frame.ver = STREAM_VERSION;
    assert_eq!(
        frame.verify(),
        Err(FormatError::BadCrc {
            region: "stream header",
            computed: ref_crc(&frame.as_bytes()[..60]),
            stored: 1
        })
    );
    let frame_after = |frame: &mut StreamHeader| {
        frame.seal();
        frame.verify()
    };
    let wire = u64::from(MAX_WIRE_PAYLOAD);
    assert_eq!(frame_after(&mut frame), field("kind", 0));
    frame.kind = FrameKind::Chunk.as_u8();
    assert_eq!(frame_after(&mut frame), field("flags", 0xFF));
    frame.flags = 3;
    assert_eq!(frame_after(&mut frame), field("codec", 3));
    frame.flags = Codec::Zstd.as_u8();
    assert_eq!(
        frame_after(&mut frame),
        range("wire_len", u64::from(u32::MAX), wire)
    );
    frame.wire_len = 7;
    frame.raw_len = u32::MAX;
    assert_eq!(
        frame_after(&mut frame),
        range("raw_len", u64::from(u32::MAX), wire)
    );
    frame.raw_len = 9;
    frame.flags = Codec::Raw.as_u8();
    assert_eq!(frame_after(&mut frame), field("raw_len", 9));
    frame.raw_len = 7;
    frame.cp_seq = MAX_CP_SEQ + 1;
    assert_eq!(
        frame_after(&mut frame),
        range("cp_seq", MAX_CP_SEQ + 1, MAX_CP_SEQ)
    );
    frame.cp_seq = 5;
    frame.base = 6;
    assert_eq!(frame_after(&mut frame), field("base", 6));
    frame.base = 5;
    assert_eq!(frame_after(&mut frame), Ok(()));
}

#[test]
fn every_parser_refuses_every_other_structure_by_magic() {
    let slot = commit_slot(agg(3), 20, SbSlot::A0);
    let mut intent = IntentSlot::new(agg(3), 2);
    intent.seal();
    let mut stripe = Page16K::zeroed();
    stripe.0[..sample_summary().len()].copy_from_slice(sample_summary());
    let (frame, _) = sample_frame(10);
    let bad = |expected, found| FormatError::BadMagic { expected, found };

    let sb_page = slot.sb.as_bytes();
    let intent_page = intent.as_bytes();
    let stripe_page = &stripe.0[..BLOCK_SIZE];
    assert_eq!(
        Superblock::parse(intent_page).unwrap_err(),
        bad(SUPERBLOCK_MAGIC, INTENT_MAGIC)
    );
    assert_eq!(
        Superblock::parse(stripe_page).unwrap_err(),
        bad(SUPERBLOCK_MAGIC, STRIPE_MAGIC)
    );
    assert_eq!(
        IntentSlot::parse(sb_page).unwrap_err(),
        bad(INTENT_MAGIC, SUPERBLOCK_MAGIC)
    );
    assert_eq!(
        IntentSlot::parse(stripe_page).unwrap_err(),
        bad(INTENT_MAGIC, STRIPE_MAGIC)
    );
    assert_eq!(
        StripeSummary::parse(sb_page).unwrap_err(),
        bad(STRIPE_MAGIC, SUPERBLOCK_MAGIC)
    );
    assert_eq!(
        StripeSummary::parse(intent_page).unwrap_err(),
        bad(STRIPE_MAGIC, INTENT_MAGIC)
    );
    assert_eq!(
        StripeSummary::parse(slot.as_bytes()).unwrap_err(),
        bad(STRIPE_MAGIC, SUPERBLOCK_MAGIC)
    );
    assert_eq!(
        StripeHdr::read(frame.as_bytes()).unwrap_err(),
        bad(STRIPE_MAGIC, STREAM_MAGIC)
    );
    assert_eq!(
        StripeHdr::read(&intent_page[..STRIPE_HDR_SIZE]).unwrap_err(),
        bad(STRIPE_MAGIC, INTENT_MAGIC)
    );
    assert_eq!(
        StreamHeader::read(&stripe.0[..STREAM_HEADER_SIZE]).unwrap_err(),
        bad(STREAM_MAGIC, STRIPE_MAGIC)
    );
    assert_eq!(
        StreamHeader::read(&sb_page[..STREAM_HEADER_SIZE]).unwrap_err(),
        bad(STREAM_MAGIC, SUPERBLOCK_MAGIC)
    );
    // A slot whose page slot holds an intent page.
    let mut confused = slot;
    confused.sb = Superblock::read_from_bytes(intent_page).unwrap();
    assert_eq!(
        SuperblockSlot::parse(confused.as_bytes()).unwrap_err(),
        bad(SUPERBLOCK_MAGIC, INTENT_MAGIC)
    );
}

// ---------------------------------------------------------------------------
// 5. Short and oversized regions, and the two summary builders
// ---------------------------------------------------------------------------

#[test]
fn summary_parse_refuses_short_regions_with_the_length_needed() {
    let n = 600;
    let total = summary_bytes(n);
    let (hdr, _, descs) = sample_stripe(n, 9);
    let mut arena = aligned_blocks(4);
    let bytes = arena.as_mut_slice().as_mut_bytes();
    assert_eq!(write_summary(bytes, &hdr, &descs), Ok(total));
    for len in [0usize, 1, 8, 63] {
        assert_eq!(
            StripeSummary::parse(&bytes[..len]).unwrap_err(),
            FormatError::BadLength {
                expected: STRIPE_HDR_SIZE,
                found: len
            }
        );
    }
    // Exactly a header: it verifies, then the summary length is refused.
    assert_eq!(
        StripeSummary::parse(&bytes[..STRIPE_HDR_SIZE]).unwrap_err(),
        FormatError::BadLength {
            expected: total,
            found: STRIPE_HDR_SIZE
        }
    );
    assert_eq!(
        StripeSummary::parse(&bytes[..total - 1]).unwrap_err(),
        FormatError::BadLength {
            expected: total,
            found: total - 1
        }
    );
    assert!(StripeSummary::parse(&bytes[..total]).is_ok());
    assert!(StripeSummary::parse(bytes).is_ok());
    assert_eq!(
        seal_summary_in_place(&mut bytes[..63]),
        Err(FormatError::BadLength {
            expected: STRIPE_HDR_SIZE,
            found: 63
        })
    );
    // A header claiming more blocks than the region holds is refused by
    // length before the summary CRC is read.
    let mut claim = StripeHdr::read(&bytes[..STRIPE_HDR_SIZE]).unwrap();
    claim.nblocks = u16::MAX;
    claim.seal_hdr();
    bytes[..STRIPE_HDR_SIZE].copy_from_slice(claim.as_bytes());
    assert_eq!(
        StripeSummary::parse(bytes).unwrap_err(),
        FormatError::BadLength {
            expected: summary_bytes(65_535),
            found: 4 * BLOCK_SIZE
        }
    );
    // One claiming fewer blocks reaches the summary CRC over its own length.
    claim.nblocks = 10;
    claim.seal_hdr();
    bytes[..STRIPE_HDR_SIZE].copy_from_slice(claim.as_bytes());
    assert_eq!(
        StripeSummary::parse(bytes).unwrap_err(),
        FormatError::BadCrc {
            region: "stripe summary",
            computed: ref_crc(&bytes[64..BLOCK_SIZE]),
            stored: claim.summary_crc
        }
    );
}

#[test]
fn write_summary_fills_exact_and_offset_regions_like_in_place_sealing() {
    for n in [1usize, 244, 245, 600] {
        let total = summary_bytes(n);
        let (hdr, _, descs) = sample_stripe(n, 11);
        // A region of exactly the padded length.
        let mut exact_buf = aligned_blocks(total / BLOCK_SIZE);
        let exact = exact_buf.as_mut_slice().as_mut_bytes();
        assert_eq!(write_summary(exact, &hdr, &descs), Ok(total), "nblocks {n}");
        // The same summary 4 KiB into a bigger aligned buffer with a dirty tail.
        let mut big_buf = aligned_blocks(total / BLOCK_SIZE + 2);
        let big = big_buf.as_mut_slice().as_mut_bytes();
        big.fill(0xC3);
        let region = &mut big[BLOCK_SIZE..];
        assert_eq!(write_summary(region, &hdr, &descs), Ok(total));
        assert_eq!(&region[..total], exact, "nblocks {n}");
        assert!(region[total..].iter().all(|&b| b == 0xC3), "nblocks {n}");
        let parsed = StripeSummary::parse(region).unwrap();
        assert_eq!(parsed.descriptors(), &descs[..]);
        assert_eq!(parsed.summary_bytes(), total);
        // Laid out by hand and sealed in place: the same bytes.
        let mut manual_buf = aligned_blocks(total / BLOCK_SIZE);
        let manual = manual_buf.as_mut_slice().as_mut_bytes();
        assert_eq!(assemble_in_place(manual, &hdr, &descs), Ok(total));
        assert_eq!(manual, exact, "nblocks {n}");
    }
    // An empty stripe is refused by both builders and by the parser.
    let (mut empty, _, _) = sample_stripe(1, 11);
    empty.nblocks = 0;
    let none = FormatError::BadField {
        field: "nblocks",
        value: 0,
    };
    let mut page = Page16K::zeroed();
    assert_eq!(write_summary(&mut page.0, &empty, &[]), Err(none));
    assert_eq!(assemble_in_place(&mut page.0, &empty, &[]), Err(none));
    let mut img = sample_summary().to_vec();
    img[STRIPE_NBLOCKS_AT..STRIPE_NBLOCKS_AT + 2].fill(0);
    reseal(&mut img);
    let arena = place(&img, 0);
    assert_eq!(
        StripeSummary::parse(arena.as_slice().as_bytes()).unwrap_err(),
        none
    );
    // A short region is refused and left untouched.
    let (hdr, _, descs) = sample_stripe(300, 11);
    let total = summary_bytes(300);
    let mut short = Page16K([0x5A; MAP_PAGE_SIZE]);
    assert_eq!(
        write_summary(&mut short.0[..total - 1], &hdr, &descs),
        Err(FormatError::BadLength {
            expected: total,
            found: total - 1
        })
    );
    assert!(short.0.iter().all(|&b| b == 0x5A));
}

#[test]
fn in_place_sealing_refuses_dirty_padding_and_writes_the_bitmap() {
    let (hdr, bitmap, descs) = sample_stripe(10, 12);
    let total = summary_bytes(10);
    let mut written = Page16K::zeroed();
    write_summary(&mut written.0, &hdr, &descs).unwrap();
    assert_eq!(derive_vol_bitmap(&descs), bitmap);
    // Dirty padding is reported (not erased) and nothing is written.
    let mut dirty = Page16K([0xEE; MAP_PAGE_SIZE]);
    dirty.0[..64].copy_from_slice(hdr.as_bytes());
    dirty.0[192..352].copy_from_slice(descs.as_bytes());
    let before = dirty;
    assert_eq!(
        seal_summary_in_place(&mut dirty.0),
        Err(FormatError::BadField {
            field: "padding",
            value: 352
        })
    );
    assert_eq!(dirty, before);
    // With the padding zeroed, the dirty bitmap zone is overwritten by the
    // derived bitmap and the result is byte for byte write_summary's.
    dirty.0[352..total].fill(0);
    assert_eq!(seal_summary_in_place(&mut dirty.0), Ok(total));
    assert_eq!(&dirty.0[64..192], bitmap.as_bytes());
    assert_eq!(&dirty.0[..total], &written.0[..total]);
}

#[test]
fn both_summary_builders_refuse_what_parse_refuses() {
    let (hdr, _, descs) = sample_stripe(2, 13);
    let mut zeroed = StripeHdr::new_zeroed();
    zeroed.nblocks = 2;
    let headers = [
        (
            "a zeroed header",
            zeroed,
            FormatError::BadMagic {
                expected: STRIPE_MAGIC,
                found: 0,
            },
        ),
        (
            "a foreign major",
            StripeHdr { ver: 0x0300, ..hdr },
            FormatError::BadVersion {
                expected_major: FORMAT_MAJOR,
                found: 0x0300,
            },
        ),
        (
            "an undefined kind",
            StripeHdr { kind: 4, ..hdr },
            FormatError::BadField {
                field: "kind",
                value: 4,
            },
        ),
        (
            "segment 0",
            StripeHdr { lseg: 0, ..hdr },
            FormatError::BadField {
                field: "lseg",
                value: 0,
            },
        ),
        (
            "a CP beyond 32 bits",
            StripeHdr {
                cp_seq: MAX_CP_SEQ + 1,
                ..hdr
            },
            FormatError::OutOfRange {
                field: "cp_seq",
                value: MAX_CP_SEQ + 1,
                max: MAX_CP_SEQ,
            },
        ),
        (
            "a count that disagrees",
            StripeHdr { nblocks: 3, ..hdr },
            FormatError::BadField {
                field: "nblocks",
                value: 2,
            },
        ),
    ];
    let hole = StripeDescriptor::hole(1, 7, 13).unwrap();
    let descriptors = [
        (
            "a reserved kind",
            StripeDescriptor {
                vol_lba_kind: descs[1].vol_lba_kind | 0b1000,
                ..descs[1]
            },
            FormatError::BadField {
                field: "kind",
                value: 2,
            },
        ),
        (
            "volume 1024",
            StripeDescriptor {
                vol_lba_kind: (1024 << 44) | (descs[1].vol_lba_kind & ((1 << 44) - 1)),
                ..descs[1]
            },
            FormatError::OutOfRange {
                field: "vol",
                value: 1024,
                max: 1023,
            },
        ),
        (
            "birth 0",
            StripeDescriptor {
                birth_cp: 0,
                ..descs[1]
            },
            FormatError::BadField {
                field: "birth_cp",
                value: 0,
            },
        ),
        (
            "a hole with a data CRC",
            StripeDescriptor {
                crc32c: 0x1234,
                ..hole
            },
            FormatError::BadField {
                field: "crc32c",
                value: 0x1234,
            },
        ),
    ];
    let cases = headers
        .into_iter()
        .map(|(label, h, e)| (label, h, descs.clone(), e));
    let cases = cases.chain(
        descriptors
            .into_iter()
            .map(|(label, d, e)| (label, hdr, vec![descs[0], d], e)),
    );
    for (label, h, ds, expected) in cases {
        if h.nblocks as usize != ds.len() {
            // assemble_in_place lays out as many descriptors as it is given;
            // the count disagreement is write_summary's to catch.
            let mut written = Page16K::zeroed();
            assert_eq!(
                write_summary(&mut written.0, &h, &ds),
                Err(expected),
                "{label}"
            );
            assert!(
                written.0.iter().all(|&b| b == 0),
                "write_summary wrote {label}"
            );
            continue;
        }
        let mut written = Page16K::zeroed();
        assert_eq!(
            write_summary(&mut written.0, &h, &ds),
            Err(expected),
            "write_summary, {label}"
        );
        assert!(
            written.0.iter().all(|&b| b == 0),
            "write_summary wrote {label} before refusing it"
        );
        let mut by_hand = Page16K::zeroed();
        assert_eq!(
            assemble_in_place(&mut by_hand.0, &h, &ds),
            Err(expected),
            "seal_summary_in_place, {label}"
        );
    }
}

#[test]
fn parse_checks_descriptors_and_the_bitmap_behind_valid_crcs() {
    let parse_after = |edit: &dyn Fn(&mut [u8])| {
        let mut img = sample_summary().to_vec();
        edit(&mut img);
        reseal(&mut img);
        let arena = place(&img, 0);
        StripeSummary::parse(arena.as_slice().as_bytes()).map(|s| s.summary_bytes())
    };
    let field = |field, value| Err(FormatError::BadField { field, value });
    let first_desc = 192;
    let set_word = |img: &mut [u8], at: usize, word: u64| {
        img[at..at + 8].copy_from_slice(&word.to_le_bytes());
    };
    assert_eq!(parse_after(&|_| {}), Ok(2 * BLOCK_SIZE));
    // The bitmap must be exactly the one the descriptors imply.
    assert_eq!(
        parse_after(&|img| img[64 + 100] ^= 1),
        field("vol_bitmap", 0)
    );
    assert_eq!(parse_after(&|img| img[64] &= !1), field("vol_bitmap", 0));
    // Each descriptor rule, behind a valid summary CRC.
    assert_eq!(
        parse_after(&|img| img[first_desc] |= 0b1000),
        field("kind", 2)
    );
    assert_eq!(
        parse_after(&|img| {
            let word = le_u64(img, first_desc);
            set_word(img, first_desc, (word & ((1 << 44) - 1)) | (1024 << 44));
        }),
        Err(FormatError::OutOfRange {
            field: "vol",
            value: 1024,
            max: 1023
        })
    );
    assert_eq!(
        parse_after(&|img| img[first_desc + 8..first_desc + 12].fill(0)),
        field("birth_cp", 0)
    );
    // Descriptor 6 is a hole; its CRC must be that of a zero block.
    let hole_crc = first_desc + 6 * 16 + 12;
    assert_eq!(
        parse_after(&|img| img[hole_crc..hole_crc + 4].copy_from_slice(&1u32.to_le_bytes())),
        field("crc32c", 1)
    );
    // The two reserved low bits are free.
    assert_eq!(
        parse_after(&|img| img[first_desc] |= 0b11),
        Ok(2 * BLOCK_SIZE)
    );
    // Header field rules, behind a valid header CRC.
    assert_eq!(parse_after(&|img| img[6] = 4), field("kind", 4));
    assert_eq!(
        parse_after(&|img| img[STRIPE_LSEG_AT..STRIPE_LSEG_AT + 8].fill(0)),
        field("lseg", 0)
    );
    assert_eq!(
        parse_after(&|img| set_word(img, STRIPE_CP_SEQ_AT, MAX_CP_SEQ + 1)),
        Err(FormatError::OutOfRange {
            field: "cp_seq",
            value: MAX_CP_SEQ + 1,
            max: MAX_CP_SEQ
        })
    );
}

#[test]
fn parse_expected_refuses_a_stale_stripe_at_a_reused_location() {
    let arena = place(sample_summary(), 0);
    let x = arena.as_slice().as_bytes();
    let summary = StripeSummary::parse(x).unwrap();
    let id = summary.hdr().id();
    assert_eq!(
        id,
        StripeId {
            agg_id: agg(7),
            lseg: 17,
            stripe_idx: 3,
            cp_seq: 4,
            cp_nonce: 4u64.wrapping_mul(0x9E37_79B9_7F4A_7C15)
        }
    );
    assert!(StripeSummary::parse_expected(x, &id).is_ok());
    for (field, other) in [
        (
            "agg_id",
            StripeId {
                agg_id: agg(8),
                ..id
            },
        ),
        ("lseg", StripeId { lseg: 18, ..id }),
        (
            "stripe_idx",
            StripeId {
                stripe_idx: 4,
                ..id
            },
        ),
        ("cp_seq", StripeId { cp_seq: 5, ..id }),
        (
            "cp_nonce",
            StripeId {
                cp_nonce: id.cp_nonce ^ 1,
                ..id
            },
        ),
    ] {
        let mismatch = FormatError::IdentityMismatch { field };
        assert_eq!(summary.hdr().check_identity(&other), Err(mismatch));
        assert_eq!(
            StripeSummary::parse_expected(x, &other).map(|s| s.summary_bytes()),
            Err(mismatch)
        );
    }
    // An older CP's stripe at the same segment and index verifies by CRC but
    // is not the stripe the metadata names.
    let (mut stale_hdr, _, descs) = sample_stripe(300, 3);
    stale_hdr.cp_nonce = id.cp_nonce;
    let mut stale = vec![0u8; summary_bytes(300)];
    write_summary(&mut stale, &stale_hdr, &descs).unwrap();
    let stale_arena = place(&stale, 0);
    let stale_x = stale_arena.as_slice().as_bytes();
    assert!(StripeSummary::parse(stale_x).is_ok());
    assert_eq!(
        StripeSummary::parse_expected(stale_x, &id).map(|s| s.summary_bytes()),
        Err(FormatError::IdentityMismatch { field: "cp_seq" })
    );
    // Integrity is checked before identity.
    let mut torn = sample_summary().to_vec();
    torn[4000] ^= 1;
    let torn_arena = place(&torn, 0);
    assert!(matches!(
        StripeSummary::parse_expected(torn_arena.as_slice().as_bytes(), &id),
        Err(FormatError::BadCrc {
            region: "stripe summary",
            ..
        })
    ));
}

proptest! {
    #![proptest_config(config())]

    /// Both summary builders give the same answer for every input, valid or
    /// not: the same bytes when they accept, the same refusal when they do
    /// not, and `write_summary` writes nothing before refusing. What they
    /// accept, the parser accepts.
    #[test]
    fn summary_builders_agree_on_every_input(
        hdr_seed in prop::collection::vec(any::<u8>(), STRIPE_HDR_SIZE),
        header_mode in 0u8..3,
        words in prop::collection::vec((any::<u64>(), any::<u32>(), any::<u32>(), 0u8..8), 0..=300),
        junk in any::<u8>(),
    ) {
        let descs: Vec<StripeDescriptor> = words
            .iter()
            .map(|&(w, birth, crc, mode)| {
                if mode == 0 {
                    StripeDescriptor { vol_lba_kind: w, birth_cp: birth, crc32c: crc }
                } else {
                    valid_descriptor(w)
                }
            })
            .collect();
        let n = descs.len();
        let hdr = match header_mode {
            0 => stripe_hdr_from(&hdr_seed, n as u16),
            1 => {
                let mut hdr = StripeHdr::read_from_bytes(&hdr_seed).unwrap();
                hdr.magic = STRIPE_MAGIC;
                hdr.ver = FORMAT_VERSION;
                hdr.nblocks = n as u16;
                hdr
            }
            _ => {
                let mut hdr = StripeHdr::read_from_bytes(&hdr_seed).unwrap();
                hdr.nblocks = n as u16;
                hdr
            }
        };
        let total = summary_bytes(n);
        let mut written = vec![junk; total + 64];
        let mut sealed = vec![!junk; total + 64];
        let a = write_summary(&mut written, &hdr, &descs);
        let b = assemble_in_place(&mut sealed, &hdr, &descs);
        prop_assert_eq!(a, b);
        if a.is_ok() {
            prop_assert_eq!(a, Ok(total));
            prop_assert_eq!(&written[..total], &sealed[..total]);
            prop_assert!(written[total..].iter().all(|&x| x == junk));
            let arena = place(&written, 0);
            prop_assert!(StripeSummary::parse(arena.as_slice().as_bytes()).is_ok());
        } else {
            prop_assert!(written.iter().all(|&x| x == junk), "write_summary wrote before refusing");
        }
    }
}

// ---------------------------------------------------------------------------
// 6. No-panic fuzz with an independent oracle
// ---------------------------------------------------------------------------

/// Clears every bit at or above `count` (the page rule for both bitsets).
fn keep_below(set: &mut VolBitSet, count: u32) {
    for v in count..VOLS {
        set.clear(v).unwrap();
    }
}

/// A page that verifies, built from random bytes: the guarded fields are
/// forced into range; the minor version, the reserved bytes and every other
/// field stay random.
fn superblock_page(seed: &[u8]) -> Superblock {
    let mut sb = Superblock::read_from_bytes(seed).unwrap();
    sb.magic = SUPERBLOCK_MAGIC;
    sb.ver = (u16::from(FORMAT_MAJOR) << 8) | (sb.ver & 0xFF);
    sb.run_state = 1 + (sb.run_state & 1);
    sb.cp_seq &= MAX_CP_SEQ;
    let location = if sb.slot & 1 == 0 {
        SbLocation::A
    } else {
        SbLocation::B
    };
    sb.slot = Superblock::slot_for(sb.cp_seq, location).index();
    sb.vol_count %= MAX_VOLS as u32 + 1;
    keep_below(&mut sb.write_active, sb.vol_count);
    keep_below(&mut sb.nvfail, sb.vol_count);
    if sb.durable_log.kind & 1 == 0 {
        sb.durable_log = DurableLogHdr::NONE;
    } else {
        sb.durable_log.kind = DURABLE_LOG_LOCAL_NVME_RING;
    }
    sb
}

fn superblock_image(seed: &[u8]) -> Vec<u8> {
    let mut sb = superblock_page(seed);
    sb.seal();
    sb.as_bytes().to_vec()
}

/// A slot that verifies: the page as above, and a table whose rows agree
/// with it (random presence, causes, counters and reserved bytes).
fn slot_image(seed: &[u8]) -> Vec<u8> {
    let mut slot = SuperblockSlot::read_from_bytes(seed).unwrap();
    slot.sb = superblock_page(&seed[..SUPERBLOCK_SIZE]);
    let vol_count = slot.sb.vol_count;
    let mut nvfail = VolBitSet::EMPTY;
    for (row, vol) in slot.vols.vols.iter_mut().zip(0u32..) {
        if vol >= vol_count {
            *row = VolState::ABSENT;
            continue;
        }
        if row.is_present() {
            row.acked_bytes &= 0xFFFF_FFFF;
            row.nvfail_cause %= 4;
        } else {
            row.acked_bytes = 0;
            row.nvfail_cause = 0;
            slot.sb.write_active.clear(vol).unwrap();
        }
        if row.nvfail_cause != 0 {
            nvfail.set(vol).unwrap();
        }
    }
    slot.sb.nvfail = nvfail;
    slot.seal().unwrap();
    slot.as_bytes().to_vec()
}

fn intent_image(seed: &[u8]) -> Vec<u8> {
    let mut intent = IntentSlot::read_from_bytes(seed).unwrap();
    intent.magic = INTENT_MAGIC;
    intent.ver = (u16::from(FORMAT_MAJOR) << 8) | (intent.ver & 0xFF);
    intent.slot = IntentSlot::slot_for(intent.intent_seq);
    intent.cp_seq &= MAX_CP_SEQ;
    intent.seal();
    intent.as_bytes().to_vec()
}

fn stripe_hdr_from(seed: &[u8], nblocks: u16) -> StripeHdr {
    let mut hdr = StripeHdr::read_from_bytes(seed).unwrap();
    hdr.magic = STRIPE_MAGIC;
    hdr.ver = (u16::from(FORMAT_MAJOR) << 8) | (hdr.ver & 0xFF);
    hdr.kind %= 4;
    hdr.lseg = hdr.lseg.max(1);
    hdr.cp_seq &= MAX_CP_SEQ;
    hdr.nblocks = nblocks;
    hdr.seal_hdr();
    hdr
}

fn stream_image(seed: &[u8], payload_len: usize) -> Vec<u8> {
    let mut hdr = StreamHeader::read_from_bytes(seed).unwrap();
    hdr.magic = STREAM_MAGIC;
    hdr.ver = (u16::from(STREAM_MAJOR) << 8) | (hdr.ver & 0xFF);
    hdr.kind = 1 + hdr.kind % 12;
    hdr.flags %= 3;
    hdr.cp_seq &= MAX_CP_SEQ;
    hdr.base %= hdr.cp_seq + 1;
    let payload = vec![0x5Au8; payload_len];
    if hdr.flags == Codec::Raw.as_u8() {
        hdr.set_payload(&payload).unwrap();
    } else {
        let raw_len = hdr.raw_len as usize % (MAX_WIRE_PAYLOAD as usize + 1);
        hdr.set_encoded_payload(&payload, raw_len).unwrap();
    }
    hdr.seal();
    hdr.as_bytes().to_vec()
}

/// A descriptor that validates, built from random bits (reserved bits kept).
fn valid_descriptor(w: u64) -> StripeDescriptor {
    let vol = (w >> 44) % u64::from(VOLS);
    let hole = (w >> 2) & 1 == 1;
    StripeDescriptor {
        vol_lba_kind: (vol << 44) | (w & ((1 << 44) - 1) & !0b1000),
        birth_cp: ((w >> 8) as u32).max(1),
        crc32c: if hole {
            zero_block_crc()
        } else {
            (w >> 16) as u32
        },
    }
}

fn summary_image(hdr_seed: &[u8], raw: &[u64], extra: usize) -> Vec<u8> {
    let descs: Vec<StripeDescriptor> = raw.iter().map(|&w| valid_descriptor(w)).collect();
    let hdr = stripe_hdr_from(hdr_seed, descs.len() as u16);
    let mut image = vec![0u8; summary_bytes(descs.len()) + extra];
    write_summary(&mut image, &hdr, &descs).unwrap();
    image
}

fn valid_image() -> impl Strategy<Value = Vec<u8>> {
    prop_oneof![
        prop::collection::vec(any::<u8>(), SUPERBLOCK_SIZE).prop_map(|s| superblock_image(&s)),
        prop::collection::vec(any::<u8>(), SUPERBLOCK_SLOT_SIZE).prop_map(|s| slot_image(&s)),
        prop::collection::vec(any::<u8>(), INTENT_SLOT_SIZE).prop_map(|s| intent_image(&s)),
        (
            prop::collection::vec(any::<u8>(), STRIPE_HDR_SIZE),
            1u16..=u16::MAX
        )
            .prop_map(|(s, n)| stripe_hdr_from(&s, n).as_bytes().to_vec()),
        (
            prop::collection::vec(any::<u8>(), STREAM_HEADER_SIZE),
            0usize..512
        )
            .prop_map(|(s, n)| stream_image(&s, n)),
        (
            prop::collection::vec(any::<u8>(), STRIPE_HDR_SIZE),
            prop::collection::vec(any::<u64>(), 1..=600),
            0usize..=4096,
        )
            .prop_map(|(h, d, extra)| summary_image(&h, &d, extra)),
    ]
}

/// Random bytes, or a valid image with a few bytes changed, optionally
/// re-sealed, and in a third of the cases cut short or extended (the rest
/// keep their length, so the full-size checks see the edited image).
fn fuzz_input() -> impl Strategy<Value = Vec<u8>> {
    prop_oneof![
        1 => prop::collection::vec(any::<u8>(), 0..=32 * 1024),
        3 => (
            valid_image(),
            prop::collection::vec((any::<usize>(), 1u8..=255), 0..4),
            any::<bool>(),
            prop_oneof![2 => Just(0isize), 1 => -8isize..=8],
        )
            .prop_map(|(mut img, flips, reseal_after, delta)| {
                for (pos, x) in flips {
                    let at = pos % img.len();
                    img[at] ^= x;
                }
                if reseal_after {
                    reseal(&mut img);
                }
                let len = img.len().saturating_add_signed(delta);
                img.resize(len, 0);
                img
            }),
    ]
}

fn alignment_offset() -> impl Strategy<Value = usize> {
    prop_oneof![3 => Just(0usize), 1 => Just(8usize), 1 => 1usize..64]
}

/// A copy's verdict against the reference: exact for the length, magic,
/// version and CRC checks; then `Ok` with the bytes verbatim exactly when
/// every documented field rule holds, and a field refusal otherwise.
fn judge(
    x: &[u8],
    copy: Result<Vec<u8>, FormatError>,
    expected: Option<FormatError>,
    rules_ok: bool,
) -> Result<(), TestCaseError> {
    match expected {
        Some(refusal) => prop_assert_eq!(copy, Err(refusal)),
        None if rules_ok => prop_assert_eq!(copy, Ok(x.to_vec())),
        None => {
            let refused = copy.err();
            prop_assert!(
                matches!(
                    refused,
                    Some(FormatError::BadField { .. } | FormatError::OutOfRange { .. })
                ),
                "a field rule fails, so the refusal must name a field: {refused:?}"
            );
        }
    }
    Ok(())
}

/// A zero-copy view answers exactly as a copy on an aligned buffer, and
/// refuses a misaligned one with `Misaligned`.
fn view_agrees(
    x: &[u8],
    view: Result<Vec<u8>, FormatError>,
    copy: Result<Vec<u8>, FormatError>,
    align: usize,
) -> Result<(), TestCaseError> {
    if x.as_ptr().addr().is_multiple_of(align) {
        prop_assert_eq!(view, copy);
    } else {
        prop_assert_eq!(view, Err(FormatError::Misaligned { required: align }));
    }
    Ok(())
}

fn check_superblock(x: &[u8]) -> Result<(), TestCaseError> {
    let copy = Superblock::read(x).map(|v| v.as_bytes().to_vec());
    let expected = record_verdict(x, SUPERBLOCK_SIZE, MAGIC_SB, DISK_MAJOR, "superblock page");
    let rules_ok = expected.is_none() && page_rules_ok(x);
    judge(x, copy.clone(), expected, rules_ok)?;
    view_agrees(
        x,
        Superblock::parse(x).map(|v| v.as_bytes().to_vec()),
        copy,
        BLOCK_SIZE,
    )
}

fn check_slot(x: &[u8]) -> Result<(), TestCaseError> {
    let copy = SuperblockSlot::read(x).map(|v| v.as_bytes().to_vec());
    let header = if x.len() == SUPERBLOCK_SLOT_SIZE {
        record_verdict(
            &x[..SUPERBLOCK_SIZE],
            SUPERBLOCK_SIZE,
            MAGIC_SB,
            DISK_MAJOR,
            "superblock page",
        )
    } else {
        Some(FormatError::BadLength {
            expected: SUPERBLOCK_SLOT_SIZE,
            found: x.len(),
        })
    };
    // The page and its rules, then the table CRC, then the table rules.
    let (expected, rules_ok) = match header {
        Some(refusal) => (Some(refusal), false),
        None if !page_rules_ok(&x[..SUPERBLOCK_SIZE]) => (None, false),
        None => {
            let computed = ref_crc(&x[SUPERBLOCK_SIZE..]);
            let stored = le_u32(x, SB_VOL_TABLE_CRC_AT);
            if computed == stored {
                (None, table_rules_ok(x))
            } else {
                let refusal = FormatError::BadCrc {
                    region: "volume table",
                    computed,
                    stored,
                };
                (Some(refusal), false)
            }
        }
    };
    judge(x, copy.clone(), expected, rules_ok)?;
    view_agrees(
        x,
        SuperblockSlot::parse(x).map(|v| v.as_bytes().to_vec()),
        copy,
        BLOCK_SIZE,
    )
}

fn check_intent(x: &[u8]) -> Result<(), TestCaseError> {
    let copy = IntentSlot::read(x).map(|v| v.as_bytes().to_vec());
    let expected = record_verdict(x, INTENT_SLOT_SIZE, MAGIC_INTENT, DISK_MAJOR, "intent slot");
    let rules_ok = expected.is_none() && intent_rules_ok(x);
    judge(x, copy.clone(), expected, rules_ok)?;
    view_agrees(
        x,
        IntentSlot::parse(x).map(|v| v.as_bytes().to_vec()),
        copy,
        BLOCK_SIZE,
    )
}

fn check_stripe_hdr(x: &[u8]) -> Result<(), TestCaseError> {
    let copy = StripeHdr::read(x).map(|v| v.as_bytes().to_vec());
    let expected = record_verdict(
        x,
        STRIPE_HDR_SIZE,
        MAGIC_STRIPE,
        DISK_MAJOR,
        "stripe header",
    );
    let rules_ok = expected.is_none() && stripe_hdr_rules_ok(x);
    judge(x, copy.clone(), expected, rules_ok)?;
    view_agrees(
        x,
        StripeHdr::parse(x).map(|v| v.as_bytes().to_vec()),
        copy,
        8,
    )
}

fn check_stream_hdr(x: &[u8]) -> Result<(), TestCaseError> {
    let copy = StreamHeader::read(x).map(|v| v.as_bytes().to_vec());
    let expected = record_verdict(
        x,
        STREAM_HEADER_SIZE,
        MAGIC_STREAM,
        WIRE_MAJOR,
        "stream header",
    );
    let rules_ok = expected.is_none() && stream_rules_ok(x);
    judge(x, copy.clone(), expected, rules_ok)?;
    view_agrees(
        x,
        StreamHeader::parse(x).map(|v| v.as_bytes().to_vec()),
        copy,
        8,
    )
}

fn check_summary(x: &[u8]) -> Result<(), TestCaseError> {
    let parsed = StripeSummary::parse(x);
    if !x.as_ptr().addr().is_multiple_of(8) {
        prop_assert_eq!(
            parsed.map(|s| s.summary_bytes()),
            Err(FormatError::Misaligned { required: 8 })
        );
        return Ok(());
    }
    match (summary_verdict(x), parsed) {
        (Ok(total), Ok(summary)) => {
            let n = usize::from(le_u16(x, STRIPE_NBLOCKS_AT));
            prop_assert_eq!(summary.summary_bytes(), total);
            prop_assert_eq!(summary.descriptors().len(), n);
            prop_assert_eq!(summary.hdr().as_bytes(), &x[..64]);
            prop_assert_eq!(summary.vol_bitmap().as_bytes(), &x[64..192]);
            prop_assert_eq!(summary.descriptors().as_bytes(), &x[192..192 + 16 * n]);
        }
        (Err(Some(expected)), Err(got)) => prop_assert_eq!(got, expected),
        (Err(None), Err(got)) => prop_assert!(
            matches!(
                got,
                FormatError::BadField { .. } | FormatError::OutOfRange { .. }
            ),
            "a field rule fails, so the refusal must name a field: {got:?}"
        ),
        (verdict, got) => {
            return Err(TestCaseError::fail(format!(
                "reference {verdict:?}, parse {:?}",
                got.map(|s| s.summary_bytes())
            )));
        }
    }
    Ok(())
}

proptest! {
    #![proptest_config(config())]

    /// No input of up to 32 KiB, at any alignment, makes a parser panic, and
    /// every parser answers exactly as the reference model of the lib.rs
    /// rulings: accepted inputs pass every documented rule and come back
    /// verbatim, refusals name the first failing check with the values read.
    #[test]
    fn parsers_never_panic_and_never_lie(input in fuzz_input(), offset in alignment_offset()) {
        let arena = place(&input, offset);
        let x = &arena.as_slice().as_bytes()[offset..offset + input.len()];
        check_summary(x)?;
        for len in [x.len(), STRIPE_HDR_SIZE, SUPERBLOCK_SIZE, SUPERBLOCK_SLOT_SIZE] {
            if len <= x.len() {
                let rec = &x[..len];
                check_superblock(rec)?;
                check_slot(rec)?;
                check_intent(rec)?;
                check_stripe_hdr(rec)?;
                check_stream_hdr(rec)?;
            }
        }
    }
}

// ---------------------------------------------------------------------------
// 7. Independent CRC-32C and the constants this file assumes
// ---------------------------------------------------------------------------

#[test]
fn reference_crc_and_crate_crc_agree_on_the_rfc_3720_vectors() {
    let increasing: Vec<u8> = (0u8..32).collect();
    let decreasing: Vec<u8> = (0u8..32).rev().collect();
    // RFC 3720 appendix B.4: an iSCSI SCSI Read (10) command PDU.
    let read10: [u8; 48] = [
        0x01, 0xC0, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
        0x00, 0x14, 0x00, 0x00, 0x00, 0x00, 0x00, 0x04, 0x00, 0x00, 0x00, 0x00, 0x14, 0x00, 0x00,
        0x00, 0x18, 0x28, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x02, 0x00, 0x00, 0x00, 0x00,
        0x00, 0x00, 0x00,
    ];
    let vectors: [(&[u8], u32); 7] = [
        (&[], 0),
        (b"123456789", 0xE306_9283),
        (&[0u8; 32], 0x8A91_36AA),
        (&[0xFFu8; 32], 0x62A8_AB43),
        (increasing.as_slice(), 0x46DD_794E),
        (decreasing.as_slice(), 0x113F_DB5C),
        (&read10, 0xD996_3A56),
    ];
    for (data, expected) in vectors {
        let len = data.len();
        assert_eq!(
            crc32c_bitwise(data),
            expected,
            "bitwise reference, {len} bytes"
        );
        assert_eq!(ref_crc(data), expected, "table reference, {len} bytes");
        assert_eq!(crc32c(data), expected, "crate, {len} bytes");
        let mut digest = Crc32c::new();
        for byte in data {
            digest.update(std::slice::from_ref(byte));
        }
        assert_eq!(
            digest.finalize(),
            expected,
            "crate digest byte by byte, {len} bytes"
        );
    }
    assert_eq!(CRC32C_POLY.reverse_bits(), POLY_REFLECTED);
    assert_eq!(CRC32C_CHECK, crc32c_bitwise(b"123456789"));
}

#[test]
fn the_spec_constants_this_file_assumes() {
    // F9: the hole CRC is the CRC of a zero block, by the bitwise reference.
    assert_eq!(ZERO_BLOCK_CRC32C, crc32c_bitwise(&[0u8; BLOCK_SIZE]));
    assert_eq!(ZERO_BLOCK_CRC32C, zero_block_crc());
    // F5, F8, F3.
    assert_eq!(MAX_CP_SEQ, MAX_CP);
    assert_eq!(MAX_VOL, VOLS - 1);
    assert_eq!(MAX_VOLS, VOLS as usize);
    assert_eq!(MAX_WIRE_PAYLOAD, WIRE_MAX);
    assert_eq!(MAX_STRIPE_BYTES, WIRE_MAX as usize);
    assert_eq!(MIN_STRIPE_BYTES, 2 * BLOCK_SIZE);
}

#[test]
fn trailing_crc_helpers_refuse_records_shorter_than_their_crc() {
    for len in 0..4 {
        let mut rec = vec![0xABu8; len];
        assert_eq!(crc32c_of_sealed(&rec), None);
        assert_eq!(trailing_crc(&rec), None);
        assert_eq!(seal_trailing_crc(&mut rec), None);
        assert!(rec.iter().all(|&b| b == 0xAB));
        assert_eq!(
            verify_trailing_crc(&rec),
            Err(FormatError::BadLength {
                expected: 4,
                found: len
            })
        );
    }
    // Four bytes are a record with an empty body, whose CRC is zero.
    let mut four = [9u8; 4];
    assert_eq!(seal_trailing_crc(&mut four), Some(0));
    assert_eq!(four, [0; 4]);
    assert_eq!(verify_trailing_crc(&four), Ok(()));
    assert_eq!(trailing_crc(&four), Some(0));
    four[0] = 1;
    assert_eq!(
        verify_trailing_crc(&four),
        Err(FormatError::BadCrc {
            region: "trailing",
            computed: 0,
            stored: 1
        })
    );
}

proptest! {
    #![proptest_config(config())]

    #[test]
    fn crate_crc_matches_the_bitwise_reference(
        data in prop::collection::vec(any::<u8>(), 0..=4096),
        offset in 0usize..64,
    ) {
        let arena = place(&data, offset);
        let x = &arena.as_slice().as_bytes()[offset..offset + data.len()];
        let expected = crc32c_bitwise(&data);
        prop_assert_eq!(crc32c(x), expected);
        prop_assert_eq!(ref_crc(&data), expected);
        prop_assert_eq!(
            crc32c_of_sealed(x),
            x.len().checked_sub(4).map(|body| crc32c_bitwise(&x[..body]))
        );
    }

    #[test]
    fn crc_digest_matches_the_reference_under_any_chunking(
        data in prop::collection::vec(any::<u8>(), 0..=2048),
        cuts in prop::collection::vec(any::<usize>(), 0..16),
    ) {
        let mut cuts: Vec<usize> = cuts.iter().map(|c| c % (data.len() + 1)).collect();
        cuts.sort_unstable();
        let mut digest = Crc32c::new();
        let mut start = 0;
        for cut in cuts {
            digest.update(&data[start..cut]);
            start = cut;
        }
        digest.update(&data[start..]);
        prop_assert_eq!(digest.finalize(), crc32c_bitwise(&data));
        prop_assert_eq!(digest.len(), data.len() as u64);
        // finalize does not end the computation.
        digest.update(b"tail");
        let mut longer = data.clone();
        longer.extend_from_slice(b"tail");
        prop_assert_eq!(digest.finalize(), crc32c_bitwise(&longer));
    }

    #[test]
    fn crc_combine_matches_the_reference(
        a in prop::collection::vec(any::<u8>(), 0..=2048),
        b in prop::collection::vec(any::<u8>(), 0..=2048),
    ) {
        let mut whole = a.clone();
        whole.extend_from_slice(&b);
        prop_assert_eq!(
            crc32c_combine(crc32c_bitwise(&a), crc32c_bitwise(&b), b.len() as u64),
            crc32c_bitwise(&whole)
        );
    }

    /// Appending zeros is linear in the CRC and composes over lengths no
    /// buffer could hold, which exercises every bit of the length operator.
    #[test]
    fn crc_combine_zero_extension_is_linear_and_composes(
        c1 in any::<u32>(),
        c2 in any::<u32>(),
        l1 in 0u64..(1 << 62),
        l2 in 0u64..(1 << 62),
    ) {
        let zeros = |crc: u32, len: u64| crc32c_combine(crc, 0, len);
        prop_assert_eq!(zeros(c1, 0), c1);
        prop_assert_eq!(zeros(c1 ^ c2, l1), zeros(c1, l1) ^ zeros(c2, l1));
        prop_assert_eq!(zeros(zeros(c1, l1), l2), zeros(c1, l1 + l2));
    }
}

// ---------------------------------------------------------------------------
// 8. Packed fields and headerless records
// ---------------------------------------------------------------------------

/// The parts of a descriptor word: volume (valid or not), LBA, kind (defined
/// or reserved) and the reserved low bits.
fn desc_parts() -> impl Strategy<Value = (u32, u64, u8, u8)> {
    (
        prop_oneof![3 => 0u32..1024, 1 => 1024u32..(1 << 20)],
        0u64..(1 << 40),
        prop_oneof![3 => 0u8..2, 1 => 2u8..4],
        0u8..4,
    )
}

proptest! {
    #![proptest_config(config())]

    /// `vol:20 | lba:40 | kind:2 | rsvd:2`, unpacked here by the documented
    /// shifts, repacked through the public constructors, and validated by
    /// F4, F8 and F9 in the documented order; reserved bits are ignored.
    #[test]
    fn descriptor_words_unpack_repack_and_validate(
        (vol, lba, kind_raw, rsvd) in desc_parts(),
        noise in any::<u64>(),
        birth in prop_oneof![1 => Just(0u32), 4 => any::<u32>()],
        crc_mode in 0u8..3,
    ) {
        let word = (u64::from(vol) << 44) | (lba << 4) | (u64::from(kind_raw) << 2) | u64::from(rsvd);
        let crc = match crc_mode {
            0 => zero_block_crc(),
            1 => 0,
            _ => noise as u32,
        };
        let d = StripeDescriptor { vol_lba_kind: word, birth_cp: birth, crc32c: crc };
        prop_assert_eq!((d.vol(), d.lba(), d.kind_raw(), d.rsvd_bits()), (vol, lba, kind_raw, rsvd));
        let vol_range = FormatError::OutOfRange { field: "vol", value: u64::from(vol), max: 1023 };
        let no_birth = FormatError::BadField { field: "birth_cp", value: 0 };
        let expected = if kind_raw >= 2 {
            Err(FormatError::BadField { field: "kind", value: u64::from(kind_raw) })
        } else if vol >= VOLS {
            Err(vol_range)
        } else if birth == 0 {
            Err(no_birth)
        } else if kind_raw == 1 && crc != zero_block_crc() {
            Err(FormatError::BadField { field: "crc32c", value: u64::from(crc) })
        } else {
            Ok(())
        };
        prop_assert_eq!(d.validate(), expected);
        if let Some(kind) = d.kind() {
            let packed = if vol >= VOLS { Err(vol_range) } else { Ok(word & !0b11) };
            prop_assert_eq!(StripeDescriptor::pack(vol, lba, kind), packed);
            let bare = StripeDescriptor { vol_lba_kind: word & !0b11, ..d };
            prop_assert_eq!(StripeDescriptor::new(vol, lba, kind, birth, crc), expected.map(|()| bare));
        }
        let base = (u64::from(vol) << 44) | (lba << 4);
        let built = |kind_bits: u64, crc32c: u32| {
            if vol >= VOLS {
                Err(vol_range)
            } else if birth == 0 {
                Err(no_birth)
            } else {
                Ok(StripeDescriptor { vol_lba_kind: base | kind_bits, birth_cp: birth, crc32c })
            }
        };
        prop_assert_eq!(StripeDescriptor::data(vol, lba, birth, crc), built(0, crc));
        prop_assert_eq!(StripeDescriptor::hole(vol, lba, birth), built(0b100, zero_block_crc()));
        // Any word at all unpacks by the same shifts.
        let any_word = StripeDescriptor { vol_lba_kind: noise, ..d };
        prop_assert_eq!(u64::from(any_word.vol()), noise >> 44);
        prop_assert_eq!(any_word.lba(), (noise >> 4) & ((1 << 40) - 1));
        prop_assert_eq!(u64::from(any_word.kind_raw()), (noise >> 2) & 0b11);
        prop_assert_eq!(u64::from(any_word.rsvd_bits()), noise & 0b11);
    }

    /// `lseg:36 | off:16 | plen:12` with v2's `plen` of 1 to 8 sectors (F2),
    /// births from 1 (F5), and no CRC on holes or unmapped blocks (F9).
    #[test]
    fn map_addresses_unpack_repack_and_validate(
        lseg in prop_oneof![1 => Just(0u64), 4 => 1u64..(1 << 36)],
        off in prop_oneof![1 => Just(0u32), 4 => 0u32..=0xFFFF],
        plen in prop_oneof![1 => Just(0u16), 3 => 1u16..=8, 1 => 9u16..=0xFFF],
        birth in prop_oneof![1 => Just(0u32), 4 => any::<u32>()],
        crc in prop_oneof![1 => Just(0u32), 2 => any::<u32>()],
        slot in 0..MAP_ENTRIES_PER_PAGE,
    ) {
        let addr = (lseg << 28) | (u64::from(off) << 12) | u64::from(plen);
        let e = MapEntry { addr, birth_cp: birth, crc32c: crc };
        prop_assert_eq!((e.lseg(), e.off(), e.plen()), (lseg, off, plen));
        let field = |field, value| FormatError::BadField { field, value };
        let packed = if lseg == 0 {
            Err(field("lseg", 0))
        } else if plen == 0 {
            Err(field("plen", 0))
        } else if plen > 8 {
            Err(FormatError::OutOfRange { field: "plen", value: u64::from(plen), max: 8 })
        } else {
            Ok(addr)
        };
        prop_assert_eq!(MapEntry::pack_addr(lseg, off, plen), packed);
        let data = match packed {
            Err(refusal) => Err(refusal),
            Ok(_) if birth == 0 => Err(field("birth_cp", 0)),
            Ok(_) => Ok(e),
        };
        prop_assert_eq!(MapEntry::data(lseg, off, plen, birth, crc), data);
        let valid = if addr != 0 {
            if lseg == 0 {
                Err(field("lseg", addr))
            } else if plen == 0 || plen > 8 {
                Err(field("plen", addr))
            } else if birth == 0 {
                Err(field("birth_cp", 0))
            } else {
                Ok(())
            }
        } else if crc != 0 {
            Err(field("crc32c", u64::from(crc)))
        } else {
            Ok(())
        };
        prop_assert_eq!(e.validate(), valid);
        let hole = if birth == 0 {
            Err(field("birth_cp", 0))
        } else {
            Ok(MapEntry { addr: 0, birth_cp: birth, crc32c: 0 })
        };
        prop_assert_eq!(MapEntry::hole(birth), hole);
        prop_assert_eq!(
            (e.is_data(), e.is_hole(), e.is_unmapped()),
            (addr != 0, addr == 0 && birth != 0, addr == 0 && birth == 0)
        );
        // One entry decides a page whose other entries are unmapped.
        let mut page = MapPage::zeroed();
        page.entries[slot] = e;
        prop_assert_eq!(page.validate(), valid);
    }

    #[test]
    fn map_root_validate_matches_the_documented_bounds(
        vol in prop_oneof![3 => 0u32..1024, 1 => any::<u32>()],
        lba_count in prop_oneof![3 => 0u64..=(1 << 40), 1 => any::<u64>()],
        checkpoint in prop_oneof![3 => 0u64..=MAX_CP, 1 => any::<u64>()],
        index_noise in any::<u32>(),
        matching in any::<bool>(),
    ) {
        let needed = lba_count.div_ceil(1024).div_ceil(1024);
        let index_pages = if matching {
            u32::try_from(needed).unwrap_or(index_noise)
        } else {
            index_noise
        };
        let root = MapRoot {
            vol,
            lba_count,
            checkpoint_cp_seq: checkpoint,
            index_pages,
            ..MapRoot::default()
        };
        let range = |field, value, max| Err(FormatError::OutOfRange { field, value, max });
        let expected = if vol >= VOLS {
            range("vol", u64::from(vol), 1023)
        } else if lba_count > (1 << 40) {
            range("lba_count", lba_count, 1 << 40)
        } else if checkpoint > MAX_CP {
            range("checkpoint_cp_seq", checkpoint, MAX_CP)
        } else if u64::from(index_pages) != needed {
            Err(FormatError::BadField { field: "index_pages", value: u64::from(index_pages) })
        } else {
            Ok(())
        };
        prop_assert_eq!(root.validate(), expected);
        prop_assert_eq!(root.index_bytes(), u64::from(index_pages) * 16384);
    }

    /// F8 and the 40-bit LBA space on peer-supplied hole records: checked
    /// arithmetic, no panic for any record.
    #[test]
    fn hole_records_validate_exactly_the_documented_range(
        vol in prop_oneof![3 => 0u32..1024, 1 => any::<u32>()],
        lba in prop_oneof![
            2 => 0u64..(1 << 40),
            1 => ((1u64 << 40) - 64)..(1 << 40),
            1 => any::<u64>(),
            1 => Just(u64::MAX),
        ],
        len in prop_oneof![1 => Just(0u32), 3 => 1u32..4096, 1 => any::<u32>()],
    ) {
        let record = HoleRecord { vol, len, lba };
        prop_assert_eq!(record.end_lba(), lba.checked_add(u64::from(len)));
        let end = u128::from(lba) + u128::from(len);
        let expected = if vol >= VOLS {
            Err(FormatError::OutOfRange { field: "vol", value: u64::from(vol), max: 1023 })
        } else if len == 0 {
            Err(FormatError::BadField { field: "len", value: 0 })
        } else if end > (1 << 40) {
            Err(FormatError::OutOfRange { field: "lba", value: lba, max: (1 << 40) - 1 })
        } else {
            Ok(())
        };
        prop_assert_eq!(record.validate(), expected);
        prop_assert_eq!(HoleRecord::new(vol, lba, len), expected.map(|()| record));
    }
}

#[test]
fn map_pages_and_indexes_verify_against_their_parents() {
    let field = |field, value| FormatError::BadField { field, value };
    let mut page = MapPage::zeroed();
    page.entries[0] = MapEntry::data(5, 9, SECTORS_PER_BLOCK, 4, 0xDEAD_BEEF).unwrap();
    page.entries[1] = MapEntry::hole(6).unwrap();
    let entry = page.index_entry(4096);
    assert_eq!(
        entry,
        PageIndexEntry::new(4096, 6, crc32c_bitwise(page.as_bytes()))
    );
    let mut buf = Page16K::zeroed();
    buf.0.copy_from_slice(page.as_bytes());
    assert_eq!(MapPage::parse(&buf.0, &entry), Ok(&page));
    assert_eq!(MapPage::read(&buf.0, &entry), Ok(page));
    let stale = PageIndexEntry {
        page_crc: entry.page_crc ^ 1,
        ..entry
    };
    assert_eq!(
        MapPage::read(&buf.0, &stale),
        Err(FormatError::BadCrc {
            region: "map page",
            computed: entry.page_crc,
            stored: entry.page_crc ^ 1
        })
    );
    let lying = PageIndexEntry {
        max_birth: 7,
        ..entry
    };
    assert_eq!(MapPage::read(&buf.0, &lying), Err(field("max_birth", 7)));
    assert_eq!(
        MapPage::read(&buf.0, &PageIndexEntry::NONE),
        Err(field("page_pba", 0))
    );
    assert_eq!(
        MapPage::parse(&buf.0[8..], &entry),
        Err(FormatError::Misaligned { required: 4096 })
    );
    // A page that matches its CRC but holds an invalid entry.
    let mut bad = page;
    bad.entries[2] = MapEntry {
        addr: 0,
        birth_cp: 0,
        crc32c: 1,
    };
    assert_eq!(
        MapPage::read(bad.as_bytes(), &bad.index_entry(4096)),
        Err(field("crc32c", 1))
    );
    // Absent index entries carry nothing.
    assert_eq!(PageIndexEntry::NONE.validate(), Ok(()));
    assert_eq!(
        PageIndexEntry::new(0, 3, 0).validate(),
        Err(field("page_pba", 0))
    );
    assert_eq!(
        PageIndexEntry::new(0, 0, 9).validate(),
        Err(field("page_pba", 0))
    );
    // The page index against its root.
    let mut index = PageIndexPage::zeroed();
    index.entries[0] = entry;
    let index_crc = crc32c_bitwise(index.as_bytes());
    let root = MapRoot {
        vol: 3,
        index_pba: 9000,
        index_pages: 1,
        index_crc,
        lba_count: 1 << 20,
        checkpoint_cp_seq: 6,
        max_birth: 6,
        ..MapRoot::default()
    };
    let mut ibuf = Page16K::zeroed();
    ibuf.0.copy_from_slice(index.as_bytes());
    assert_eq!(root.index_bytes(), 16384);
    assert_eq!(root.verify_index(&ibuf.0), Ok(()));
    assert_eq!(
        root.verify_index(&ibuf.0[..16383]),
        Err(FormatError::BadLength {
            expected: 16384,
            found: 16383
        })
    );
    let wrong = MapRoot {
        index_crc: index_crc ^ 1,
        ..root
    };
    assert_eq!(
        wrong.verify_index(&ibuf.0),
        Err(FormatError::BadCrc {
            region: "page index",
            computed: index_crc,
            stored: index_crc ^ 1
        })
    );
    let mut dirty = index;
    dirty.entries[5] = PageIndexEntry::new(0, 0, 7);
    ibuf.0.copy_from_slice(dirty.as_bytes());
    let dirty_root = MapRoot {
        index_crc: crc32c_bitwise(dirty.as_bytes()),
        ..root
    };
    assert_eq!(dirty_root.verify_index(&ibuf.0), Err(field("page_pba", 0)));
    // A root reference over a region.
    let region = &ibuf.0[..];
    let reference = RootRef::for_region(77, region).unwrap();
    assert_eq!(reference, RootRef::new(77, 4, crc32c_bitwise(region)));
    assert_eq!(reference.verify(region), Ok(()));
    assert_eq!(
        reference.verify(&region[..BLOCK_SIZE]),
        Err(FormatError::BadLength {
            expected: 16384,
            found: BLOCK_SIZE
        })
    );
    assert_eq!(
        RootRef::for_region(77, &region[..100]),
        Err(field("pages", 100))
    );
    let mut changed = ibuf;
    changed.0[100] ^= 1;
    assert!(matches!(
        reference.verify(&changed.0),
        Err(FormatError::BadCrc {
            region: "root region",
            ..
        })
    ));
    assert_eq!(RootRef::NONE.verify(&[]), Ok(()));
}

#[test]
fn hole_records_from_the_wire_are_checked_without_panicking() {
    // The repro of the overflow: a peer-supplied record near the top of u64.
    let mut wire = [0u8; HOLE_RECORD_SIZE];
    wire[0..4].copy_from_slice(&7u32.to_le_bytes());
    wire[4..8].copy_from_slice(&16u32.to_le_bytes());
    wire[8..16].copy_from_slice(&(u64::MAX - 1).to_le_bytes());
    let record = HoleRecord::read_from_bytes(&wire).unwrap();
    assert_eq!(record.end_lba(), None);
    let past_the_end = |lba| FormatError::OutOfRange {
        field: "lba",
        value: lba,
        max: MAX_LBA,
    };
    assert_eq!(record.validate(), Err(past_the_end(u64::MAX - 1)));
    // The edges of the 40-bit address space and of the volume bound.
    assert_eq!(
        HoleRecord::new(0, MAX_LBA, 1).map(|r| r.end_lba()),
        Ok(Some(MAX_LBA + 1))
    );
    assert_eq!(HoleRecord::new(0, MAX_LBA, 2), Err(past_the_end(MAX_LBA)));
    assert_eq!(
        HoleRecord::new(MAX_VOL, 0, u32::MAX).map(|r| r.end_lba()),
        Ok(Some(u64::from(u32::MAX)))
    );
    assert_eq!(
        HoleRecord::new(MAX_VOL + 1, 0, 1),
        Err(FormatError::OutOfRange {
            field: "vol",
            value: u64::from(MAX_VOL + 1),
            max: u64::from(MAX_VOL)
        })
    );
    assert_eq!(
        HoleRecord::new(0, 0, 0),
        Err(FormatError::BadField {
            field: "len",
            value: 0
        })
    );
}

// ---------------------------------------------------------------------------
// 9. Determinism and coverage
// ---------------------------------------------------------------------------

#[test]
fn identical_inputs_build_identical_images() {
    let slot = commit_slot(agg(8), 42, SbSlot::A0);
    assert_eq!(slot.as_bytes(), sample_slot().as_bytes());
    assert_eq!(
        blake3_256(slot.as_bytes()),
        blake3_256(sample_slot().as_bytes())
    );
    // Built in place over a clean and over a dirty direct-I/O buffer.
    let build = |buf: &mut Block4K| {
        let sb = Superblock::mut_from_bytes(&mut buf.0).unwrap();
        *sb = Superblock::new(agg(8), 77, SbLocation::B);
        sb.owner_epoch = 9;
        sb.vol_count = 8;
        sb.write_active.set(5).unwrap();
        sb.seal();
    };
    let mut clean = Block4K::zeroed();
    let mut dirty = Block4K([0xA5; BLOCK_SIZE]);
    build(&mut clean);
    build(&mut dirty);
    assert_eq!(clean.0, dirty.0);
    assert!(Superblock::parse(&clean.0).is_ok());
    // A summary written into a dirty region.
    let (hdr, _, descs) = sample_stripe(300, 4);
    let mut dirty_page = Page16K([0xA5; MAP_PAGE_SIZE]);
    let total = write_summary(&mut dirty_page.0, &hdr, &descs).unwrap();
    assert_eq!(&dirty_page.0[..total], sample_summary());
    // Two frames from the same inputs.
    let (a, _) = sample_frame(100);
    let (b, _) = sample_frame(100);
    assert_eq!(a.as_bytes(), b.as_bytes());
}

proptest! {
    #![proptest_config(config())]

    #[test]
    fn any_byte_change_in_a_slot_moves_the_blake3_and_fails_the_crc(
        index in 0..SUPERBLOCK_SLOT_SIZE,
        xor in 1u8..=255,
    ) {
        let original = sample_slot().as_bytes();
        let mut changed = original.to_vec();
        changed[index] ^= xor;
        prop_assert_ne!(blake3_256(&changed), blake3_256(original));
        prop_assert!(SuperblockSlot::read(&changed).is_err());
    }

    #[test]
    fn any_byte_change_in_a_summary_or_frame_is_caught(index in any::<usize>(), xor in 1u8..=255) {
        let original = sample_summary();
        let mut changed = original.to_vec();
        changed[index % original.len()] ^= xor;
        prop_assert_ne!(blake3_256(&changed), blake3_256(original));
        let arena = place(&changed, 0);
        prop_assert!(StripeSummary::parse(arena.as_slice().as_bytes()).is_err());
        let (frame, _) = sample_frame(100);
        let mut header = frame.as_bytes().to_vec();
        header[index % STREAM_HEADER_SIZE] ^= xor;
        prop_assert!(StreamHeader::read(&header).is_err());
    }

    /// CRC-32C has an (x + 1) factor and a period far beyond these lengths, so
    /// every 2- and 3-bit error is detected, not just most of them.
    #[test]
    fn two_and_three_bit_errors_in_a_slot_are_always_caught(
        bits in prop::collection::vec(0..SUPERBLOCK_SLOT_SIZE * 8, 2..=3),
    ) {
        let mut distinct = bits.clone();
        distinct.sort_unstable();
        distinct.dedup();
        prop_assume!(distinct.len() == bits.len());
        let mut img = sample_slot().as_bytes().to_vec();
        for &bit in &bits {
            img[bit / 8] ^= 1 << (bit % 8);
        }
        prop_assert!(SuperblockSlot::read(&img).is_err());
    }

    #[test]
    fn two_and_three_bit_errors_in_headers_are_always_caught(
        bits in prop::collection::vec(0..STREAM_HEADER_SIZE * 8, 2..=3),
    ) {
        let mut distinct = bits.clone();
        distinct.sort_unstable();
        distinct.dedup();
        prop_assume!(distinct.len() == bits.len());
        let (frame, _) = sample_frame(10);
        let mut stream = frame.as_bytes().to_vec();
        let mut stripe = sample_summary()[..STRIPE_HDR_SIZE].to_vec();
        for &bit in &bits {
            stream[bit / 8] ^= 1 << (bit % 8);
            stripe[bit / 8] ^= 1 << (bit % 8);
        }
        prop_assert!(StreamHeader::read(&stream).is_err());
        prop_assert!(StripeHdr::read(&stripe).is_err());
    }
}

// ---------------------------------------------------------------------------
// 10. Version policy
// ---------------------------------------------------------------------------

#[test]
fn later_minors_round_trip_with_reserved_bytes_verbatim() {
    let mut slot = SuperblockSlot::new(agg(6), 5, SbLocation::A);
    slot.sb.ver = FORMAT_VERSION + 1;
    slot.sb.vol_count = 8;
    fill_nonzero(&mut slot.sb.reserved, 0x11);
    slot.sb.inbound[3].rsvd = 0xDEAD_BEEF;
    // A configured durable log may carry its reserved bytes (kind 0 may not).
    slot.sb.durable_log.kind = DURABLE_LOG_LOCAL_NVME_RING;
    slot.sb.durable_log.rsvd0 = [1, 2, 3, 4, 5, 6];
    slot.sb.durable_log.rsvd1 = u64::MAX;
    // An absent row inside vol_count may carry reserved bits.
    slot.vols.vols[7].rsvd = 0xFFFF;
    slot.vols.vols[7].flags = 0xF0;
    slot.seal().unwrap();
    let viewed = SuperblockSlot::parse(slot.as_bytes()).unwrap();
    assert_eq!(viewed.as_bytes(), slot.as_bytes());
    assert_eq!(viewed.sb.reserved, slot.sb.reserved);
    assert_eq!(SuperblockSlot::read(slot.as_bytes()).unwrap(), slot);
    let page = Superblock::read(slot.sb.as_bytes()).unwrap();
    assert_eq!(page.ver, FORMAT_VERSION + 1);
    assert_eq!(page.reserved, slot.sb.reserved);
    assert!(page.reserved.iter().all(|&b| b != 0));
    assert!(!page.is_newer_format());
    // The highest minor of the major is accepted too.
    let mut top = slot.sb;
    top.ver = (u16::from(FORMAT_MAJOR) << 8) | 0xFF;
    top.seal();
    assert_eq!(Superblock::read(top.as_bytes()).unwrap(), top);

    let mut intent = IntentSlot::new(agg(6), 9);
    intent.ver = FORMAT_VERSION + 1;
    intent.rsvd0 = 0x7F;
    fill_nonzero(&mut intent.reserved, 0x22);
    intent.seal();
    let viewed = IntentSlot::parse(intent.as_bytes()).unwrap();
    assert_eq!(viewed.as_bytes(), intent.as_bytes());
    assert_eq!(IntentSlot::read(intent.as_bytes()).unwrap(), intent);

    // A summary whose header flags and descriptor reserved bits are set.
    let (mut hdr, _, mut descs) = sample_stripe(5, 6);
    hdr.ver = FORMAT_VERSION + 1;
    hdr.flags = 0xFF;
    for d in &mut descs {
        d.vol_lba_kind |= DESC_RSVD_MASK;
    }
    let mut page = Page16K::zeroed();
    let total = write_summary(&mut page.0, &hdr, &descs).unwrap();
    let summary = StripeSummary::parse(&page.0[..total]).unwrap();
    assert_eq!(summary.hdr().ver, FORMAT_VERSION + 1);
    assert_eq!(summary.hdr().flags, 0xFF);
    assert_eq!(summary.descriptors(), &descs[..]);
    assert!(summary.descriptors().iter().all(|d| d.rsvd_bits() == 0b11));

    let (mut frame, payload) = sample_frame(33);
    frame.ver = STREAM_VERSION + 1;
    frame.seal();
    let read = StreamHeader::read(frame.as_bytes()).unwrap();
    assert_eq!(read, frame);
    assert!(read.verify_payload(&payload).is_ok());
}

#[test]
fn other_majors_are_refused_and_newer_ones_stop_the_boot() {
    for ver in [
        0x0000u16, 0x0002, 0x0100, 0x01FF, 0x0300, 0x0302, 0x8000, 0xFF00, 0xFFFF,
    ] {
        let refused = FormatError::BadVersion {
            expected_major: FORMAT_MAJOR,
            found: ver,
        };
        let mut slot = SuperblockSlot::new(agg(6), 1, SbLocation::A);
        slot.sb.ver = ver;
        slot.seal().unwrap();
        assert_eq!(slot.sb.verify(), Err(refused), "ver {ver:#06x}");
        assert_eq!(slot.sb.is_newer_format(), (ver >> 8) > 2, "ver {ver:#06x}");
        assert_eq!(SuperblockSlot::parse(slot.as_bytes()).unwrap_err(), refused);
        assert_eq!(Superblock::read(slot.sb.as_bytes()).unwrap_err(), refused);

        let mut intent = IntentSlot::new(agg(6), 0);
        intent.ver = ver;
        intent.seal();
        assert_eq!(IntentSlot::read(intent.as_bytes()).unwrap_err(), refused);
        assert_eq!(intent.is_newer_format(), (ver >> 8) > 2);

        // A sealed summary whose header is re-sealed under the foreign major.
        let mut page = Page16K::zeroed();
        page.0[..sample_summary().len()].copy_from_slice(sample_summary());
        let mut foreign = StripeHdr::read(&page.0[..STRIPE_HDR_SIZE]).unwrap();
        foreign.ver = ver;
        foreign.seal_hdr();
        page.0[..STRIPE_HDR_SIZE].copy_from_slice(foreign.as_bytes());
        assert_eq!(StripeSummary::parse(&page.0).unwrap_err(), refused);
        assert_eq!(
            StripeHdr::read(&page.0[..STRIPE_HDR_SIZE]).unwrap_err(),
            refused
        );
    }
    for ver in [
        0x0000u16, 0x0001, 0x00FF, 0x0200, 0x0201, 0x8001, 0xFF01, 0xFFFF,
    ] {
        let (mut frame, _) = sample_frame(10);
        frame.ver = ver;
        frame.seal();
        assert_eq!(
            StreamHeader::read(frame.as_bytes()).unwrap_err(),
            FormatError::BadVersion {
                expected_major: STREAM_MAJOR,
                found: ver
            },
            "ver {ver:#06x}"
        );
    }

    // F7: a slot of a newer major anywhere in the ring stops the boot, even
    // with a broken CRC or another aggregate's id; an older one is skipped.
    let agg_id = agg(6);
    let base = ring_before_commit(agg_id, 20);
    for victim in 0..SUPERBLOCK_SLOTS {
        for ver in [0x0300u16, 0xFF00] {
            let mut ring = base.clone();
            ring[victim].sb.ver = ver;
            ring[victim].seal().unwrap();
            let stop = Err(FormatError::NewerFormat { found: ver });
            assert_eq!(ring_select(&agg_id, &ring), stop, "slot {victim}");
            ring[victim].sb.crc32c ^= 1;
            assert_eq!(ring_select(&agg_id, &ring), stop, "slot {victim}, bad CRC");
            ring[victim].sb.agg_id = agg(7);
            assert_eq!(
                ring_select(&agg_id, &ring),
                stop,
                "slot {victim}, other aggregate"
            );
        }
        let mut ring = base.clone();
        ring[victim].sb.ver = 0x0100;
        ring[victim].seal().unwrap();
        assert_eq!(
            ring_select(&agg_id, &ring),
            Ok((RING_EPOCH, 19)),
            "slot {victim}"
        );
    }

    // The same rule for intent slots in the boot union.
    let mut sb = Superblock::new(agg_id, 20, SbLocation::A);
    sb.vol_count = 4;
    sb.write_active.set(1).unwrap();
    sb.seal();
    let intent_of = |ver: u16| {
        let mut intent = IntentSlot::new(agg_id, 9);
        intent.ver = ver;
        intent.write_active.set(2).unwrap();
        intent.seal();
        intent
    };
    assert_eq!(
        boot_write_active(&sb, [&intent_of(0x0300)]),
        Err(FormatError::NewerFormat { found: 0x0300 })
    );
    assert_eq!(
        boot_write_active(&sb, [&intent_of(0x0100)]),
        Ok(sb.write_active)
    );
    let mut both = sb.write_active;
    both.set(2).unwrap();
    assert_eq!(
        boot_write_active(&sb, [&intent_of(FORMAT_VERSION + 1)]),
        Ok(both)
    );
}

/// On the wire the header has no reserved bytes (lib.rs convention 4 and the
/// stream module's "Versioning on the wire"): a flag bit this release does not
/// define is refused under any minor; new ones are negotiated in `Hello`.
#[test]
fn stream_reserved_flag_bits_are_refused_even_from_a_later_minor() {
    let (mut frame, _) = sample_frame(10);
    frame.ver = STREAM_VERSION + 1;
    for bit in 2..8 {
        frame.flags = Codec::Raw.as_u8() | (1 << bit);
        frame.seal();
        assert_eq!(
            frame.verify(),
            Err(FormatError::BadField {
                field: "flags",
                value: u64::from(frame.flags)
            }),
            "flag bit {bit}"
        );
    }
}

#[test]
fn frame_builders_refuse_what_verify_would_refuse() {
    let payload = vec![7u8; 100];
    let codec_of = |flags: u8| FormatError::BadField {
        field: "codec",
        value: u64::from(flags),
    };
    let mut raw = StreamHeader::new(FrameKind::Chunk, Codec::Raw);
    assert_eq!(raw.set_encoded_payload(&payload, 400), Err(codec_of(0)));
    raw.set_payload(&payload).unwrap();
    assert_eq!(
        (raw.wire_len, raw.raw_len, raw.payload_crc),
        (100, 100, crc32c_bitwise(&payload))
    );
    let mut lz4 = StreamHeader::new(FrameKind::Chunk, Codec::Lz4);
    assert_eq!(lz4.set_payload(&payload), Err(codec_of(1)));
    assert_eq!(
        lz4.set_encoded_payload(&payload, WIRE_MAX as usize + 1),
        Err(FormatError::OutOfRange {
            field: "raw_len",
            value: u64::from(WIRE_MAX) + 1,
            max: u64::from(WIRE_MAX)
        })
    );
    lz4.set_encoded_payload(&payload, WIRE_MAX as usize)
        .unwrap();
    lz4.seal();
    assert_eq!(lz4.verify(), Ok(()));
    let mut odd = StreamHeader::new(FrameKind::Chunk, Codec::Raw);
    odd.flags = 3;
    assert_eq!(odd.set_payload(&payload), Err(codec_of(3)));
    assert_eq!(odd.set_encoded_payload(&payload, 1), Err(codec_of(3)));
    let big = vec![0u8; WIRE_MAX as usize + 1];
    assert_eq!(
        raw.set_payload(&big),
        Err(FormatError::OutOfRange {
            field: "wire_len",
            value: u64::from(WIRE_MAX) + 1,
            max: u64::from(WIRE_MAX)
        })
    );
    raw.set_payload(&big[..WIRE_MAX as usize]).unwrap();
    raw.seal();
    assert_eq!(raw.verify(), Ok(()));
    assert_eq!(raw.verify_payload(&big[..WIRE_MAX as usize]), Ok(()));
}

// ---------------------------------------------------------------------------
// 11. The rules added by the audit
// ---------------------------------------------------------------------------

#[test]
fn superblock_verify_refuses_an_undefined_or_dirty_durable_log() {
    let mut sb = Superblock::new(agg(9), 0, SbLocation::A);
    let verdict = |sb: &mut Superblock| {
        sb.seal();
        let mut page = Block4K::zeroed();
        page.0.copy_from_slice(sb.as_bytes());
        assert_eq!(Superblock::parse(&page.0).map(|_| ()), sb.verify());
        sb.verify()
    };
    assert_eq!(verdict(&mut sb), Ok(()));
    for kind in [2u8, 0x80, 0xFF] {
        sb.durable_log = DurableLogHdr::NONE;
        sb.durable_log.kind = kind;
        assert_eq!(
            verdict(&mut sb),
            Err(FormatError::BadField {
                field: "durable_log.kind",
                value: u64::from(kind)
            })
        );
    }
    // No durable log means every other byte of the reservation is zero.
    for at in 1..DURABLE_LOG_HDR_SIZE {
        let bytes: Vec<u8> = (0..DURABLE_LOG_HDR_SIZE)
            .map(|i| u8::from(i == at))
            .collect();
        sb.durable_log = DurableLogHdr::read_from_bytes(&bytes).unwrap();
        assert_eq!(
            verdict(&mut sb),
            Err(FormatError::BadField {
                field: "durable_log",
                value: 0
            }),
            "byte {at}"
        );
    }
    // A configured log may carry anything in its fields.
    let mut bytes = [0xA5u8; DURABLE_LOG_HDR_SIZE];
    bytes[0] = DURABLE_LOG_LOCAL_NVME_RING;
    sb.durable_log = DurableLogHdr::read_from_bytes(&bytes).unwrap();
    assert_eq!(verdict(&mut sb), Ok(()));
    assert!(DurableLogHdr::NONE.is_none());
    assert_eq!(DurableLogHdr::NONE.validate(), Ok(()));
}

#[test]
fn slot_verify_enforces_the_volume_table_rules() {
    let base = || {
        let mut slot = SuperblockSlot::new(agg(10), 4, SbLocation::A);
        slot.sb.vol_count = 4;
        slot.vols.vols[0] = VolState {
            acked_bytes: 4096,
            rollback_gen: 1,
            nvfail_cause: NvCause::None.as_u8(),
            flags: VOL_PRESENT,
            rsvd: 0,
        };
        slot.vols.vols[1] = VolState {
            acked_bytes: 0,
            rollback_gen: 2,
            nvfail_cause: NvCause::Promote.as_u8(),
            flags: VOL_PRESENT,
            rsvd: 0,
        };
        slot.sb.nvfail.set(1).unwrap();
        slot.sb.write_active.set(0).unwrap();
        slot.seal().unwrap();
        slot
    };
    let refusal = |slot: SuperblockSlot| SuperblockSlot::read(slot.as_bytes()).err();
    let field = |field, value| Some(FormatError::BadField { field, value });
    let good = base();
    assert_eq!(
        SuperblockSlot::parse(good.as_bytes()).map(|s| s.sb.acked_bytes),
        Ok(4096)
    );
    let edited = |edit: &dyn Fn(&mut SuperblockSlot)| {
        let mut slot = base();
        edit(&mut slot);
        slot.seal().unwrap();
        refusal(slot)
    };
    // Every cause must decode.
    for cause in [4u8, 0x80, 0xFF] {
        assert_eq!(
            edited(&|s| s.vols.vols[1].nvfail_cause = cause),
            field("nvfail_cause", u64::from(cause))
        );
    }
    // Rows at and above vol_count are absent, to the last one.
    assert_eq!(
        edited(&|s| s.vols.vols[4].rollback_gen = 1),
        field("vol_count", 4)
    );
    assert_eq!(
        edited(&|s| s.vols.vols[1023].rsvd = 1),
        field("vol_count", 1023)
    );
    // An absent volume carries no acknowledged bytes and no cause.
    assert_eq!(
        edited(&|s| s.vols.vols[2].acked_bytes = 1),
        field("vol_present", 2)
    );
    assert_eq!(
        edited(&|s| {
            s.vols.vols[2].nvfail_cause = NvCause::TornCp.as_u8();
            s.sb.nvfail.set(2).unwrap();
        }),
        field("vol_present", 2)
    );
    // An nvfail bit exactly where a cause is.
    assert_eq!(
        edited(&|s| s.sb.nvfail.clear(1).unwrap()),
        field("nvfail", 1)
    );
    assert_eq!(edited(&|s| s.sb.nvfail.set(0).unwrap()), field("nvfail", 0));
    // Write-active only on present volumes.
    assert_eq!(
        edited(&|s| s.sb.write_active.set(3).unwrap()),
        field("write_active", 3)
    );
    // The page's acked_bytes is the sum of the rows.
    let mut off_by_one = base();
    off_by_one.sb.acked_bytes += 1;
    off_by_one.sb.seal();
    assert_eq!(refusal(off_by_one), field("acked_bytes", 4097));
    let mut overflow = base();
    overflow.vols.vols[0].acked_bytes = u64::MAX;
    overflow.vols.vols[1].acked_bytes = 1;
    assert_eq!(
        overflow.seal(),
        Err(FormatError::BadField {
            field: "acked_bytes",
            value: u64::MAX
        })
    );
    // The reserved bytes of a present row are free.
    assert_eq!(
        edited(&|s| {
            s.vols.vols[0].rsvd = 0xFFFF;
            s.vols.vols[0].flags = 0xFF;
        }),
        None
    );
}
