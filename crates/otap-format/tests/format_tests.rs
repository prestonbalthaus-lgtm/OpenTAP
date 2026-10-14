//! The format freeze suite for `otap-format` (round 3, section 8.4):
//!
//! 1. exact sizes, alignments and field offsets of every structure;
//! 2. byte-identical round trips over random bitstreams, by copy and by
//!    aligned zero-copy view;
//! 3. full CRC coverage: a single flipped bit anywhere in a sealed structure
//!    fails verification, through every parent-CRC path as well;
//! 4. packed-field encode and decode with range checks;
//! 5. the ring ranking, boot union and stripe identity rules;
//! 6. the golden-vector decode lane: stored v2.0 and v2.1 / v1.0 images under
//!    `tests/vectors/v2.<minor>/` with BLAKE3 manifests, decoded by the current
//!    reader and, at the version that produced them, reproduced byte for
//!    byte by the builders.
//!
//! `PROPTEST_CASES=100000 cargo test -p otap-format` runs the merge gate;
//! the default is proptest's 256 cases per property. The vectors are
//! regenerated on purpose only, with
//! `cargo test -p otap-format -- --ignored write_golden_vectors`, together
//! with a minor version bump.

use core::mem::{align_of, offset_of, size_of};
use std::collections::HashMap;
use std::path::PathBuf;

use otap_format::checksum::{
    blake3_256, crc32c, crc32c_combine, hex, seal_trailing_crc, verify_trailing_crc, Crc32c,
    CRC32C_CHECK,
};
use otap_format::*;
use proptest::prelude::*;
use proptest::test_runner::TestCaseError;

// ---------------------------------------------------------------------------
// helpers
// ---------------------------------------------------------------------------

/// A 4096-aligned buffer the size of a whole superblock slot.
#[repr(C, align(4096))]
struct AlignedSlot([u8; SUPERBLOCK_SLOT_SIZE]);

/// Two aligned blocks, so a slice at offset 1 is a guaranteed misaligned view.
#[repr(C, align(4096))]
struct TwoBlocks([u8; 2 * BLOCK_SIZE]);

fn flip_bit(bytes: &mut [u8], bit: usize) {
    bytes[bit / 8] ^= 1 << (bit % 8);
}

fn uuid(seed: u8) -> Uuid {
    let mut b = [0u8; 16];
    for (i, x) in b.iter_mut().enumerate() {
        *x = seed.wrapping_mul(17).wrapping_add(i as u8);
    }
    Uuid::from_bytes(b)
}

fn bits(words: [u64; VOL_BITSET_WORDS]) -> VolBitSet {
    VolBitSet { words }
}

/// Makes a random page verify: fix the magic, the version, the enumerated
/// fields, the slot parity and the bitset bound; keep everything else.
fn fix_page(sb: &mut Superblock) {
    sb.magic = SUPERBLOCK_MAGIC;
    sb.ver = FORMAT_VERSION;
    sb.run_state = 1 + (sb.run_state % 2);
    sb.cp_seq &= MAX_CP_SEQ;
    sb.slot = (sb.slot & 0b10) | (sb.cp_seq & 1) as u8;
    sb.vol_count = MAX_VOLS as u32;
    // A recorded geometry (2.2) built from the random bytes: a whole stripe
    // of at most 8 MiB, a segment of whole stripes, a capacity of at least
    // one segment.
    let stripe = 4096 * (1 + sb.geom_stripe_bytes % 2048);
    let segment = stripe * (1 + sb.geom_segment_bytes % 64);
    let capacity = sb.geom_capacity_bytes.max(u64::from(segment));
    sb.set_geometry(stripe, segment, capacity);
    if sb.open_data_pseg == 0 {
        sb.open_data_idx = 0;
    }
    if sb.durable_log.kind.is_multiple_of(2) {
        sb.durable_log = DurableLogHdr::NONE;
    } else {
        sb.durable_log.kind = DURABLE_LOG_LOCAL_NVME_RING;
    }
}

fn sealed_superblock(bytes: &[u8]) -> Superblock {
    let mut sb = Superblock::read_from_bytes(bytes).unwrap();
    fix_page(&mut sb);
    sb.seal();
    sb
}

/// A random slot that verifies: the page as above, and table rows made
/// consistent with the page's bitsets.
fn sealed_slot(bytes: &[u8]) -> SuperblockSlot {
    let mut slot = SuperblockSlot::read_from_bytes(bytes).unwrap();
    fix_page(&mut slot.sb);
    let SuperblockSlot { sb, vols } = &mut slot;
    for (i, row) in vols.vols.iter_mut().enumerate() {
        let vol = i as u32;
        let failed = sb.nvfail.get(vol);
        let present = sb.write_active.get(vol) || failed || row.flags & VOL_PRESENT != 0;
        row.flags = (row.flags & !VOL_PRESENT) | u8::from(present);
        row.nvfail_cause = if failed { 1 + row.nvfail_cause % 3 } else { 0 };
        row.acked_bytes &= 0xFFFF_FFFF;
        if !present {
            row.acked_bytes = 0;
        }
    }
    slot.seal().unwrap();
    slot
}

fn sealed_intent(bytes: &[u8]) -> IntentSlot {
    let mut intent = IntentSlot::read_from_bytes(bytes).unwrap();
    intent.magic = INTENT_MAGIC;
    intent.ver = FORMAT_VERSION;
    intent.slot = IntentSlot::slot_for(intent.intent_seq);
    intent.cp_seq &= MAX_CP_SEQ;
    intent.seal();
    intent
}

fn sealed_stripe_hdr(bytes: &[u8]) -> StripeHdr {
    let mut hdr = StripeHdr::read_from_bytes(bytes).unwrap();
    hdr.magic = STRIPE_MAGIC;
    hdr.ver = FORMAT_VERSION;
    hdr.kind %= 4;
    hdr.lseg = hdr.lseg.max(1);
    hdr.cp_seq &= MAX_CP_SEQ;
    hdr.nblocks = hdr.nblocks.max(1);
    hdr.seal_hdr();
    hdr
}

fn sealed_stream_hdr(bytes: &[u8], payload: &[u8]) -> StreamHeader {
    let mut hdr = StreamHeader::read_from_bytes(bytes).unwrap();
    hdr.magic = STREAM_MAGIC;
    hdr.ver = STREAM_VERSION;
    hdr.kind = 1 + (hdr.kind % 12);
    hdr.flags %= 3;
    hdr.cp_seq &= MAX_CP_SEQ;
    hdr.base = hdr.base.min(hdr.cp_seq);
    if hdr.flags == Codec::Raw.as_u8() {
        hdr.set_payload(payload).unwrap();
    } else {
        let raw_len = (hdr.raw_len % (MAX_WIRE_PAYLOAD + 1)) as usize;
        hdr.set_encoded_payload(payload, raw_len).unwrap();
    }
    hdr.seal();
    hdr
}

/// Valid descriptors: a volume below the bound, a birth of at least 1, and
/// the zero-block CRC on holes.
fn descriptor() -> impl Strategy<Value = StripeDescriptor> {
    (
        0u32..MAX_VOLS as u32,
        0u64..=MAX_LBA,
        any::<bool>(),
        1u32..=u32::MAX,
        any::<u32>(),
    )
        .prop_map(|(vol, lba, hole, birth, crc)| {
            if hole {
                StripeDescriptor::hole(vol, lba, birth).unwrap()
            } else {
                StripeDescriptor::data(vol, lba, birth, crc).unwrap()
            }
        })
}

/// Copy round trip, then aligned zero-copy round trip, both byte-identical.
fn check_roundtrip<T>(bytes: &[u8], aligned: &mut [u8]) -> Result<(), TestCaseError>
where
    T: FromBytes + IntoBytes + Immutable + KnownLayout + PartialEq,
{
    let copy = T::read_from_bytes(bytes).unwrap();
    prop_assert_eq!(copy.as_bytes(), bytes);
    aligned[..bytes.len()].copy_from_slice(bytes);
    let view = T::ref_from_bytes(&aligned[..bytes.len()]).unwrap();
    prop_assert!(*view == copy);
    prop_assert_eq!(view.as_bytes(), bytes);
    Ok(())
}

// ---------------------------------------------------------------------------
// 1. sizes, alignments, offsets
// ---------------------------------------------------------------------------

#[test]
fn struct_sizes_are_frozen() {
    assert_eq!(size_of::<Superblock>(), 4096);
    assert_eq!(size_of::<StripeDescriptor>(), 16);
    assert_eq!(size_of::<StreamHeader>(), 64);
    assert_eq!(size_of::<StripeHdr>(), 64);
    assert_eq!(size_of::<MapEntry>(), 16);
    assert_eq!(size_of::<MapPage>(), 16384);
    assert_eq!(size_of::<PageIndexEntry>(), 16);
    assert_eq!(size_of::<PageIndexPage>(), 16384);
    assert_eq!(size_of::<MapRoot>(), 64);
    assert_eq!(size_of::<IntentSlot>(), 4096);
    assert_eq!(size_of::<VolState>(), 16);
    assert_eq!(size_of::<VolTable>(), 16384);
    assert_eq!(size_of::<SuperblockSlot>(), 20480);
    assert_eq!(size_of::<InboundState>(), 72);
    assert_eq!(size_of::<DurableLogHdr>(), 64);
    assert_eq!(size_of::<RootRef>(), 16);
    assert_eq!(size_of::<VolBitSet>(), 128);
    assert_eq!(size_of::<Uuid>(), 16);
    assert_eq!(size_of::<HoleRecord>(), 16);
    assert_eq!(size_of::<Block4K>(), 4096);
    assert_eq!(size_of::<Page16K>(), 16384);
}

#[test]
fn struct_alignments_are_frozen() {
    assert_eq!(align_of::<Superblock>(), 4096);
    assert_eq!(align_of::<SuperblockSlot>(), 4096);
    assert_eq!(align_of::<VolTable>(), 4096);
    assert_eq!(align_of::<IntentSlot>(), 4096);
    assert_eq!(align_of::<MapPage>(), 4096);
    assert_eq!(align_of::<PageIndexPage>(), 4096);
    assert_eq!(align_of::<Block4K>(), 4096);
    assert_eq!(align_of::<Page16K>(), 4096);
    assert_eq!(align_of::<StripeHdr>(), 8);
    assert_eq!(align_of::<StripeDescriptor>(), 8);
    assert_eq!(align_of::<StreamHeader>(), 8);
    assert_eq!(align_of::<MapEntry>(), 8);
    assert_eq!(align_of::<PageIndexEntry>(), 8);
    assert_eq!(align_of::<MapRoot>(), 8);
    assert_eq!(align_of::<HoleRecord>(), 8);
    assert_eq!(align_of::<VolState>(), 8);
    assert_eq!(align_of::<InboundState>(), 8);
    assert_eq!(align_of::<DurableLogHdr>(), 8);
    assert_eq!(align_of::<RootRef>(), 8);
    assert_eq!(align_of::<VolBitSet>(), 8);
    assert_eq!(align_of::<Uuid>(), 1);
}

#[test]
fn superblock_field_offsets() {
    assert_eq!(offset_of!(Superblock, magic), 0);
    assert_eq!(offset_of!(Superblock, ver), 4);
    assert_eq!(offset_of!(Superblock, run_state), 6);
    assert_eq!(offset_of!(Superblock, slot), 7);
    assert_eq!(offset_of!(Superblock, agg_id), 8);
    assert_eq!(offset_of!(Superblock, owner_epoch), 24);
    assert_eq!(offset_of!(Superblock, cp_seq), 32);
    assert_eq!(offset_of!(Superblock, cut_seq), 40);
    assert_eq!(offset_of!(Superblock, cp_nonce), 48);
    assert_eq!(offset_of!(Superblock, intent_seq_seen), 56);
    assert_eq!(offset_of!(Superblock, opened_stamp), 64);
    assert_eq!(offset_of!(Superblock, wall_ns), 72);
    assert_eq!(offset_of!(Superblock, acked_bytes), 80);
    assert_eq!(offset_of!(Superblock, vol_count), 88);
    assert_eq!(offset_of!(Superblock, vol_table_crc), 92);
    assert_eq!(offset_of!(Superblock, write_active), 96);
    assert_eq!(offset_of!(Superblock, nvfail), 224);
    assert_eq!(offset_of!(Superblock, freed_pending), 352);
    assert_eq!(offset_of!(Superblock, map_roots), 384);
    assert_eq!(offset_of!(Superblock, seg_table), 400);
    assert_eq!(offset_of!(Superblock, reloc_index), 416);
    assert_eq!(offset_of!(Superblock, cp_log_head), 432);
    assert_eq!(offset_of!(Superblock, snap_table), 448);
    assert_eq!(offset_of!(Superblock, cluster_state), 464);
    assert_eq!(offset_of!(Superblock, durable_log), 480);
    assert_eq!(offset_of!(Superblock, inbound), 544);
    assert_eq!(offset_of!(Superblock, open_data_pseg), 1696);
    assert_eq!(offset_of!(Superblock, open_data_idx), 1704);
    assert_eq!(offset_of!(Superblock, rsvd_open), 1708);
    assert_eq!(offset_of!(Superblock, geom_stripe_bytes), 1712);
    assert_eq!(offset_of!(Superblock, geom_segment_bytes), 1716);
    assert_eq!(offset_of!(Superblock, geom_capacity_bytes), 1720);
    assert_eq!(offset_of!(Superblock, reserved), 1728);
    assert_eq!(offset_of!(Superblock, crc32c), 4092);
    assert_eq!(1728 + SUPERBLOCK_RESERVED + 4, SUPERBLOCK_SIZE);

    assert_eq!(offset_of!(SuperblockSlot, sb), 0);
    assert_eq!(offset_of!(SuperblockSlot, vols), 4096);
}

#[test]
fn superblock_record_offsets() {
    assert_eq!(offset_of!(VolState, acked_bytes), 0);
    assert_eq!(offset_of!(VolState, rollback_gen), 8);
    assert_eq!(offset_of!(VolState, nvfail_cause), 12);
    assert_eq!(offset_of!(VolState, flags), 13);
    assert_eq!(offset_of!(VolState, rsvd), 14);

    assert_eq!(offset_of!(InboundState, applied_cp_seq), 0);
    assert_eq!(offset_of!(InboundState, applied_owner_epoch), 8);
    assert_eq!(offset_of!(InboundState, acked_cp_seq), 16);
    assert_eq!(offset_of!(InboundState, recv_mono_ns), 24);
    assert_eq!(offset_of!(InboundState, sender_cut_wall_ns), 32);
    assert_eq!(offset_of!(InboundState, recv_boot_id), 40);
    assert_eq!(offset_of!(InboundState, cp_age_at_send_ms), 56);
    assert_eq!(offset_of!(InboundState, ship_duration_ms), 60);
    assert_eq!(offset_of!(InboundState, group), 64);
    assert_eq!(offset_of!(InboundState, flags), 66);
    assert_eq!(offset_of!(InboundState, rsvd), 68);

    assert_eq!(offset_of!(DurableLogHdr, kind), 0);
    assert_eq!(offset_of!(DurableLogHdr, flags), 1);
    assert_eq!(offset_of!(DurableLogHdr, rsvd0), 2);
    assert_eq!(offset_of!(DurableLogHdr, dev_id), 8);
    assert_eq!(offset_of!(DurableLogHdr, ring_pba), 24);
    assert_eq!(offset_of!(DurableLogHdr, ring_bytes), 32);
    assert_eq!(offset_of!(DurableLogHdr, head), 40);
    assert_eq!(offset_of!(DurableLogHdr, tail), 48);
    assert_eq!(offset_of!(DurableLogHdr, rsvd1), 56);

    assert_eq!(offset_of!(RootRef, pba), 0);
    assert_eq!(offset_of!(RootRef, pages), 8);
    assert_eq!(offset_of!(RootRef, crc32c), 12);

    assert_eq!(offset_of!(IntentSlot, magic), 0);
    assert_eq!(offset_of!(IntentSlot, ver), 4);
    assert_eq!(offset_of!(IntentSlot, slot), 6);
    assert_eq!(offset_of!(IntentSlot, rsvd0), 7);
    assert_eq!(offset_of!(IntentSlot, agg_id), 8);
    assert_eq!(offset_of!(IntentSlot, intent_seq), 24);
    assert_eq!(offset_of!(IntentSlot, cp_seq), 32);
    assert_eq!(offset_of!(IntentSlot, wall_ns), 40);
    assert_eq!(offset_of!(IntentSlot, write_active), 48);
    assert_eq!(offset_of!(IntentSlot, reserved), 176);
    assert_eq!(offset_of!(IntentSlot, crc32c), 4092);
    assert_eq!(176 + INTENT_RESERVED + 4, INTENT_SLOT_SIZE);
}

#[test]
fn stripe_field_offsets() {
    assert_eq!(offset_of!(StripeHdr, magic), 0);
    assert_eq!(offset_of!(StripeHdr, ver), 4);
    assert_eq!(offset_of!(StripeHdr, kind), 6);
    assert_eq!(offset_of!(StripeHdr, flags), 7);
    assert_eq!(offset_of!(StripeHdr, agg_id), 8);
    assert_eq!(offset_of!(StripeHdr, cp_seq), 24);
    assert_eq!(offset_of!(StripeHdr, cp_nonce), 32);
    assert_eq!(offset_of!(StripeHdr, lseg), 40);
    assert_eq!(offset_of!(StripeHdr, stripe_idx), 48);
    assert_eq!(offset_of!(StripeHdr, nblocks), 52);
    assert_eq!(offset_of!(StripeHdr, group), 54);
    assert_eq!(offset_of!(StripeHdr, summary_crc), 56);
    assert_eq!(offset_of!(StripeHdr, hdr_crc), 60);

    assert_eq!(offset_of!(StripeDescriptor, vol_lba_kind), 0);
    assert_eq!(offset_of!(StripeDescriptor, birth_cp), 8);
    assert_eq!(offset_of!(StripeDescriptor, crc32c), 12);
}

#[test]
fn map_field_offsets() {
    assert_eq!(offset_of!(MapEntry, addr), 0);
    assert_eq!(offset_of!(MapEntry, birth_cp), 8);
    assert_eq!(offset_of!(MapEntry, crc32c), 12);
    assert_eq!(offset_of!(MapPage, entries), 0);

    assert_eq!(offset_of!(PageIndexEntry, page_pba), 0);
    assert_eq!(offset_of!(PageIndexEntry, max_birth), 8);
    assert_eq!(offset_of!(PageIndexEntry, page_crc), 12);
    assert_eq!(offset_of!(PageIndexPage, entries), 0);

    assert_eq!(offset_of!(MapRoot, vol), 0);
    assert_eq!(offset_of!(MapRoot, flags), 4);
    assert_eq!(offset_of!(MapRoot, index_pba), 8);
    assert_eq!(offset_of!(MapRoot, index_pages), 16);
    assert_eq!(offset_of!(MapRoot, index_crc), 20);
    assert_eq!(offset_of!(MapRoot, lba_count), 24);
    assert_eq!(offset_of!(MapRoot, checkpoint_cp_seq), 32);
    assert_eq!(offset_of!(MapRoot, max_birth), 40);
    assert_eq!(offset_of!(MapRoot, rsvd0), 44);
    assert_eq!(offset_of!(MapRoot, rsvd1), 48);
}

#[test]
fn stream_field_offsets() {
    assert_eq!(offset_of!(StreamHeader, magic), 0);
    assert_eq!(offset_of!(StreamHeader, ver), 4);
    assert_eq!(offset_of!(StreamHeader, kind), 6);
    assert_eq!(offset_of!(StreamHeader, flags), 7);
    assert_eq!(offset_of!(StreamHeader, rel), 8);
    assert_eq!(offset_of!(StreamHeader, chunk_idx), 12);
    assert_eq!(offset_of!(StreamHeader, cp_seq), 16);
    assert_eq!(offset_of!(StreamHeader, base), 24);
    assert_eq!(offset_of!(StreamHeader, cp_nonce), 32);
    assert_eq!(offset_of!(StreamHeader, owner_epoch), 40);
    assert_eq!(offset_of!(StreamHeader, wire_len), 48);
    assert_eq!(offset_of!(StreamHeader, raw_len), 52);
    assert_eq!(offset_of!(StreamHeader, payload_crc), 56);
    assert_eq!(offset_of!(StreamHeader, hdr_crc), 60);

    assert_eq!(offset_of!(HoleRecord, vol), 0);
    assert_eq!(offset_of!(HoleRecord, len), 4);
    assert_eq!(offset_of!(HoleRecord, lba), 8);
}

#[test]
fn constants_and_magics() {
    assert_eq!(FORMAT_VERSION, 0x0202);
    assert_eq!(STREAM_VERSION, 0x0100);
    assert_eq!(&SUPERBLOCK_MAGIC.to_le_bytes(), b"OTSB");
    assert_eq!(&INTENT_MAGIC.to_le_bytes(), b"OTIN");
    assert_eq!(&STRIPE_MAGIC.to_le_bytes(), b"OTST");
    assert_eq!(&STREAM_MAGIC.to_le_bytes(), b"OTSF");
    assert_eq!(MAX_VOL, 1023);
    assert_eq!(MAX_VOL_ID, 0xF_FFFF);
    assert_eq!(MAX_LBA, 0xFF_FFFF_FFFF);
    assert_eq!(MAX_LSEG, 0xF_FFFF_FFFF);
    assert_eq!(MAX_SEG_OFF, 0xFFFF);
    assert_eq!(MAX_PLEN, 0xFFF);
    assert_eq!(MAX_CP_SEQ, u64::from(u32::MAX));
    assert_eq!(MAP_LSEG_SHIFT, 28);
    assert_eq!(MAP_OFF_SHIFT, 12);
    assert_eq!(DESC_VOL_SHIFT, 44);
    assert_eq!(DESC_LBA_SHIFT, 4);
    assert_eq!(DESC_KIND_SHIFT, 2);
    assert_eq!(SUPERBLOCK_SLOT_SIZE, 20480);
    assert_eq!(STRIPE_SUMMARY_FIXED, 192);
    assert_eq!(MAX_STRIPE_BYTES, 8 << 20);
    assert_eq!(MAX_WIRE_PAYLOAD as usize, MAX_STRIPE_BYTES);
    assert_eq!(ZERO_BLOCK_CRC32C, crc32c(&[0u8; BLOCK_SIZE]));
}

// ---------------------------------------------------------------------------
// 2. checksums
// ---------------------------------------------------------------------------

#[test]
fn crc32c_known_answers() {
    assert_eq!(crc32c(b"123456789"), CRC32C_CHECK);
    assert_eq!(crc32c(b"123456789"), 0xE306_9283);
    assert_eq!(crc32c(&[]), 0);
    // RFC 3720 appendix B.4 vectors (the on-wire byte order there is
    // little-endian, so "aa 36 91 8a" is the value 0x8A9136AA).
    assert_eq!(crc32c(&[0u8; 32]), 0x8A91_36AA);
    assert_eq!(crc32c(&[0xFFu8; 32]), 0x62A8_AB43);
    let increasing: Vec<u8> = (0u8..32).collect();
    assert_eq!(crc32c(&increasing), 0x46DD_794E);
    let decreasing: Vec<u8> = (0u8..32).rev().collect();
    assert_eq!(crc32c(&decreasing), 0x113F_DB5C);
}

#[test]
fn blake3_known_answers() {
    assert_eq!(
        hex(&blake3_256(b"")),
        "af1349b9f5f9a1a6a0404dea36dcc9499bcb25c9adc112b7cc9a93cae41f3262"
    );
    assert_eq!(
        hex(&blake3_256(b"abc")),
        "6437b3ac38465133ffb63b75273a8db548c558465d79db03fd359c6cd5bd9d85"
    );
}

proptest! {
    #[test]
    fn crc32c_incremental_matches_oneshot(
        data in prop::collection::vec(any::<u8>(), 0..4096),
        split in 0usize..4096,
    ) {
        let split = split.min(data.len());
        let mut inc = Crc32c::new();
        inc.update(&data[..split]);
        inc.update(&data[split..]);
        prop_assert_eq!(inc.finalize(), crc32c(&data));
        prop_assert_eq!(inc.len(), data.len() as u64);
    }

    #[test]
    fn crc32c_combine_matches_oneshot(
        a in prop::collection::vec(any::<u8>(), 0..2048),
        b in prop::collection::vec(any::<u8>(), 0..2048),
    ) {
        let whole = [a.as_slice(), b.as_slice()].concat();
        prop_assert_eq!(crc32c_combine(crc32c(&a), crc32c(&b), b.len() as u64), crc32c(&whole));
    }

    #[test]
    fn trailing_crc_seal_and_verify(
        mut record in prop::collection::vec(any::<u8>(), 4..512),
        bit_seed in any::<usize>(),
    ) {
        let crc = seal_trailing_crc(&mut record).unwrap();
        prop_assert_eq!(crc, crc32c(&record[..record.len() - 4]));
        prop_assert!(verify_trailing_crc(&record).is_ok());
        let bit = bit_seed % (record.len() * 8);
        flip_bit(&mut record, bit);
        prop_assert!(
            matches!(
                verify_trailing_crc(&record),
                Err(FormatError::BadCrc { region: "trailing", .. })
            ),
            "a flipped bit must fail the trailing CRC"
        );
    }
}

// ---------------------------------------------------------------------------
// 3. byte-identical round trips over random bitstreams
// ---------------------------------------------------------------------------

proptest! {
    #[test]
    fn superblock_roundtrip(bytes in prop::collection::vec(any::<u8>(), SUPERBLOCK_SIZE)) {
        let mut aligned = Block4K::zeroed();
        check_roundtrip::<Superblock>(&bytes, &mut aligned.0)?;
    }

    #[test]
    fn intent_slot_roundtrip(bytes in prop::collection::vec(any::<u8>(), INTENT_SLOT_SIZE)) {
        let mut aligned = Block4K::zeroed();
        check_roundtrip::<IntentSlot>(&bytes, &mut aligned.0)?;
    }

    #[test]
    fn superblock_slot_roundtrip(bytes in prop::collection::vec(any::<u8>(), SUPERBLOCK_SLOT_SIZE)) {
        let mut aligned = AlignedSlot([0; SUPERBLOCK_SLOT_SIZE]);
        check_roundtrip::<SuperblockSlot>(&bytes, &mut aligned.0)?;
    }

    #[test]
    fn vol_table_roundtrip(bytes in prop::collection::vec(any::<u8>(), VOL_TABLE_SIZE)) {
        let mut aligned = Page16K::zeroed();
        check_roundtrip::<VolTable>(&bytes, &mut aligned.0)?;
    }

    #[test]
    fn map_page_roundtrip(bytes in prop::collection::vec(any::<u8>(), MAP_PAGE_SIZE)) {
        let mut aligned = Page16K::zeroed();
        check_roundtrip::<MapPage>(&bytes, &mut aligned.0)?;
    }

    #[test]
    fn page_index_page_roundtrip(bytes in prop::collection::vec(any::<u8>(), INDEX_PAGE_SIZE)) {
        let mut aligned = Page16K::zeroed();
        check_roundtrip::<PageIndexPage>(&bytes, &mut aligned.0)?;
    }

    #[test]
    fn small_record_roundtrips(bytes in prop::collection::vec(any::<u8>(), 64)) {
        let mut aligned = Block4K::zeroed();
        check_roundtrip::<StripeHdr>(&bytes, &mut aligned.0)?;
        check_roundtrip::<StreamHeader>(&bytes, &mut aligned.0)?;
        check_roundtrip::<MapRoot>(&bytes, &mut aligned.0)?;
        check_roundtrip::<DurableLogHdr>(&bytes, &mut aligned.0)?;
        check_roundtrip::<StripeDescriptor>(&bytes[..16], &mut aligned.0)?;
        check_roundtrip::<MapEntry>(&bytes[..16], &mut aligned.0)?;
        check_roundtrip::<PageIndexEntry>(&bytes[..16], &mut aligned.0)?;
        check_roundtrip::<HoleRecord>(&bytes[..16], &mut aligned.0)?;
        check_roundtrip::<VolState>(&bytes[..16], &mut aligned.0)?;
        check_roundtrip::<RootRef>(&bytes[..16], &mut aligned.0)?;
        check_roundtrip::<Uuid>(&bytes[..16], &mut aligned.0)?;
    }

    #[test]
    fn inbound_and_bitset_roundtrips(bytes in prop::collection::vec(any::<u8>(), 128)) {
        let mut aligned = Block4K::zeroed();
        check_roundtrip::<VolBitSet>(&bytes, &mut aligned.0)?;
        check_roundtrip::<InboundState>(&bytes[..INBOUND_STATE_SIZE], &mut aligned.0)?;
    }

    /// Sealing, decoding and re-encoding is byte-identical, and a sealed
    /// image decodes to the same fields it was built from, reserved bytes
    /// included.
    #[test]
    fn seal_decode_reencode_is_identity(bytes in prop::collection::vec(any::<u8>(), SUPERBLOCK_SLOT_SIZE)) {
        let slot = sealed_slot(&bytes);
        let decoded = SuperblockSlot::read(slot.as_bytes()).unwrap();
        prop_assert!(decoded == slot);
        prop_assert_eq!(decoded.as_bytes(), slot.as_bytes());
        prop_assert_eq!(decoded.sb.compute_crc(), decoded.sb.crc32c);
        prop_assert_eq!(&decoded.sb.reserved[..], &bytes[1728..4092]);
    }
}

// ---------------------------------------------------------------------------
// 4. full CRC coverage: any single-bit flip fails verification
// ---------------------------------------------------------------------------

proptest! {
    #[test]
    fn superblock_bit_flip_detected(
        bytes in prop::collection::vec(any::<u8>(), SUPERBLOCK_SIZE),
        bit in 0usize..SUPERBLOCK_SIZE * 8,
    ) {
        let sb = sealed_superblock(&bytes);
        prop_assert!(sb.verify().is_ok());
        let mut img = Block4K::zeroed();
        img.0.copy_from_slice(sb.as_bytes());
        prop_assert!(Superblock::parse(&img.0).is_ok());
        flip_bit(&mut img.0, bit);
        prop_assert!(Superblock::parse(&img.0).is_err());
        prop_assert!(Superblock::read(&img.0).is_err());
    }

    #[test]
    fn superblock_slot_bit_flip_detected(
        bytes in prop::collection::vec(any::<u8>(), SUPERBLOCK_SLOT_SIZE),
        bit in 0usize..SUPERBLOCK_SLOT_SIZE * 8,
    ) {
        let slot = sealed_slot(&bytes);
        prop_assert!(slot.verify().is_ok());
        prop_assert_eq!(slot.sb.vol_table_crc, slot.vols.crc32c());
        prop_assert_eq!(Some(slot.sb.acked_bytes), slot.vols.acked_bytes_total());
        let mut img = AlignedSlot([0; SUPERBLOCK_SLOT_SIZE]);
        img.0.copy_from_slice(slot.as_bytes());
        prop_assert!(SuperblockSlot::parse(&img.0).is_ok());
        flip_bit(&mut img.0, bit);
        prop_assert!(SuperblockSlot::parse(&img.0).is_err());
        prop_assert!(SuperblockSlot::read(&img.0).is_err());
    }

    #[test]
    fn intent_slot_bit_flip_detected(
        bytes in prop::collection::vec(any::<u8>(), INTENT_SLOT_SIZE),
        bit in 0usize..INTENT_SLOT_SIZE * 8,
    ) {
        let intent = sealed_intent(&bytes);
        prop_assert!(intent.verify().is_ok());
        let mut img = Block4K::zeroed();
        img.0.copy_from_slice(intent.as_bytes());
        prop_assert!(IntentSlot::parse(&img.0).is_ok());
        flip_bit(&mut img.0, bit);
        prop_assert!(IntentSlot::parse(&img.0).is_err());
    }

    #[test]
    fn stripe_hdr_bit_flip_detected(
        bytes in prop::collection::vec(any::<u8>(), STRIPE_HDR_SIZE),
        bit in 0usize..STRIPE_HDR_SIZE * 8,
    ) {
        let hdr = sealed_stripe_hdr(&bytes);
        prop_assert!(hdr.verify_hdr().is_ok());
        let mut img = [0u8; STRIPE_HDR_SIZE];
        img.copy_from_slice(hdr.as_bytes());
        prop_assert!(StripeHdr::read(&img).is_ok());
        flip_bit(&mut img, bit);
        prop_assert!(StripeHdr::read(&img).is_err());
    }

    #[test]
    fn stripe_summary_bit_flip_detected(
        descriptors in prop::collection::vec(descriptor(), 1..=600),
        hdr_bytes in prop::collection::vec(any::<u8>(), STRIPE_HDR_SIZE),
        bit_seed in any::<usize>(),
    ) {
        let mut hdr = sealed_stripe_hdr(&hdr_bytes);
        hdr.nblocks = descriptors.len() as u16;
        let expected_bitmap = derive_vol_bitmap(&descriptors);

        let mut page = Page16K::zeroed();
        let total = write_summary(&mut page.0, &hdr, &descriptors).unwrap();
        prop_assert_eq!(total, summary_bytes(descriptors.len()));
        prop_assert_eq!(total % BLOCK_SIZE, 0);

        let parsed = StripeSummary::parse(&page.0[..total]).unwrap();
        prop_assert_eq!(parsed.descriptors(), &descriptors[..]);
        prop_assert!(*parsed.vol_bitmap() == expected_bitmap);
        prop_assert_eq!(parsed.hdr().nblocks as usize, descriptors.len());
        prop_assert_eq!(parsed.hdr().cp_seq, hdr.cp_seq);
        prop_assert_eq!(parsed.hdr().lseg, hdr.lseg);
        prop_assert_eq!(parsed.summary_bytes(), total);
        prop_assert!(StripeSummary::parse_expected(&page.0[..total], &hdr.id()).is_ok());
        // A whole stripe image (longer than the summary) parses the same way.
        prop_assert!(StripeSummary::parse(&page.0).is_ok());

        // Hand layout plus in-place sealing reproduces the same bytes: the
        // unsealed header, a bitmap zone it fills, the descriptors, zero padding.
        let mut again = Page16K::zeroed();
        again.0[..STRIPE_HDR_SIZE].copy_from_slice(hdr.as_bytes());
        again.0[descriptor_offset(0)..descriptor_offset(descriptors.len())]
            .copy_from_slice(descriptors.as_bytes());
        prop_assert_eq!(seal_summary_in_place(&mut again.0), Ok(total));
        prop_assert_eq!(&again.0[..], &page.0[..]);

        flip_bit(&mut page.0, bit_seed % (total * 8));
        prop_assert!(StripeSummary::parse(&page.0[..total]).is_err());
        prop_assert!(StripeSummary::parse(&page.0).is_err());
    }

    #[test]
    fn stream_header_bit_flip_detected(
        hdr_bytes in prop::collection::vec(any::<u8>(), STREAM_HEADER_SIZE),
        payload in prop::collection::vec(any::<u8>(), 0..2048),
        bit in 0usize..STREAM_HEADER_SIZE * 8,
    ) {
        let hdr = sealed_stream_hdr(&hdr_bytes, &payload);
        prop_assert!(hdr.verify().is_ok());
        prop_assert!(hdr.verify_payload(&payload).is_ok());
        prop_assert_eq!(hdr.frame_len(), STREAM_HEADER_SIZE + payload.len());
        let mut img = [0u8; STREAM_HEADER_SIZE];
        img.copy_from_slice(hdr.as_bytes());
        prop_assert!(StreamHeader::read(&img).is_ok());
        flip_bit(&mut img, bit);
        prop_assert!(StreamHeader::read(&img).is_err());
    }

    #[test]
    fn stream_payload_bit_flip_detected(
        hdr_bytes in prop::collection::vec(any::<u8>(), STREAM_HEADER_SIZE),
        payload in prop::collection::vec(any::<u8>(), 1..2048),
        bit_seed in any::<usize>(),
    ) {
        let hdr = sealed_stream_hdr(&hdr_bytes, &payload);
        let mut corrupted = payload.clone();
        flip_bit(&mut corrupted, bit_seed % (payload.len() * 8));
        prop_assert!(
            matches!(
                hdr.verify_payload(&corrupted),
                Err(FormatError::BadCrc { region: "stream payload", .. })
            ),
            "a flipped payload bit must fail the payload CRC"
        );
        prop_assert!(hdr.verify_payload(&payload[..payload.len() - 1]).is_err());
        let mut longer = payload.clone();
        longer.push(0);
        prop_assert!(hdr.verify_payload(&longer).is_err());
    }

    /// The parent-CRC paths: a map page against its index entry, and a root
    /// region against its reference.
    #[test]
    fn map_page_and_root_region_bit_flips_detected(
        seeds in prop::collection::vec((1u64..=MAX_LSEG, 0u32..=MAX_SEG_OFF, 1u32..=u32::MAX, any::<u32>(), 0u8..3), 1..64),
        bit_seed in any::<usize>(),
    ) {
        let mut page = MapPage::zeroed();
        for (i, &(lseg, off, birth, crc, kind)) in seeds.iter().enumerate() {
            page.entries[i * 7 % MAP_ENTRIES_PER_PAGE] = match kind {
                0 => MapEntry::data(lseg, off, SECTORS_PER_BLOCK, birth, crc).unwrap(),
                1 => MapEntry::hole(birth).unwrap(),
                _ => MapEntry::UNMAPPED,
            };
        }
        let entry = page.index_entry(4096);
        prop_assert_eq!(entry.max_birth, page.max_birth());
        let mut img = Page16K::zeroed();
        img.0.copy_from_slice(page.as_bytes());
        prop_assert!(MapPage::parse(&img.0, &entry).is_ok());
        prop_assert!(MapPage::read(&img.0, &entry).is_ok());
        let root = RootRef::for_region(4096, &img.0).unwrap();
        prop_assert_eq!(root.pages, 4);
        prop_assert!(root.verify(&img.0).is_ok());

        flip_bit(&mut img.0, bit_seed % (MAP_PAGE_SIZE * 8));
        prop_assert!(
            matches!(
                MapPage::parse(&img.0, &entry),
                Err(FormatError::BadCrc { region: "map page", .. })
            ),
            "a flipped map page bit must fail the index entry CRC"
        );
        prop_assert!(
            matches!(
                root.verify(&img.0),
                Err(FormatError::BadCrc { region: "root region", .. })
            ),
            "a flipped region bit must fail the root CRC"
        );
        prop_assert!(root.verify(&img.0[..MAP_PAGE_SIZE - 1]).is_err());
    }
}

// ---------------------------------------------------------------------------
// 5. packed fields
// ---------------------------------------------------------------------------

proptest! {
    #[test]
    fn descriptor_pack_roundtrip(
        vol in 0u32..MAX_VOLS as u32,
        lba in 0u64..=MAX_LBA,
        hole in any::<bool>(),
        birth in 1u32..=u32::MAX,
        crc in any::<u32>(),
    ) {
        let (kind, crc) = if hole { (BlockKind::Hole, ZERO_BLOCK_CRC32C) } else { (BlockKind::Data, crc) };
        let d = StripeDescriptor::new(vol, lba, kind, birth, crc).unwrap();
        prop_assert_eq!(d.vol(), vol);
        prop_assert_eq!(d.lba(), lba);
        prop_assert_eq!(d.kind(), Some(kind));
        prop_assert_eq!(d.kind_raw(), kind.as_u8());
        prop_assert_eq!(d.rsvd_bits(), 0);
        prop_assert_eq!(d.birth_cp, birth);
        prop_assert_eq!(d.crc32c, crc);
        prop_assert!(d.validate().is_ok());
        prop_assert_eq!(d.vol_lba_kind >> DESC_VOL_SHIFT, u64::from(vol));
        let again = if hole {
            StripeDescriptor::hole(vol, lba, birth).unwrap()
        } else {
            StripeDescriptor::data(vol, lba, birth, crc).unwrap()
        };
        prop_assert_eq!(again, d);
    }

    #[test]
    fn descriptor_rejects_out_of_range(
        vol_field in (MAX_VOLS as u32)..=MAX_VOL_ID,
        vol_wide in (MAX_VOL_ID + 1)..=u32::MAX,
        lba in (MAX_LBA + 1)..=u64::MAX,
        crc in any::<u32>(),
    ) {
        prop_assert!(
            matches!(
                StripeDescriptor::pack(vol_field, 0, BlockKind::Data),
                Err(FormatError::OutOfRange { field: "vol", .. })
            ),
            "a volume index at or above MAX_VOLS is out of range"
        );
        prop_assert!(StripeDescriptor::pack(vol_wide, 0, BlockKind::Data).is_err());
        prop_assert!(StripeDescriptor::pack(0, lba, BlockKind::Data).is_err());
        prop_assert!(StripeDescriptor::new(vol_field, 0, BlockKind::Hole, 1, ZERO_BLOCK_CRC32C).is_err());
        // A zero birth and a hole with a data CRC are rejected.
        prop_assert!(
            matches!(
                StripeDescriptor::data(0, 0, 0, crc),
                Err(FormatError::BadField { field: "birth_cp", .. })
            ),
            "a zero birth is rejected"
        );
        prop_assert_eq!(
            StripeDescriptor::new(0, 0, BlockKind::Hole, 1, crc).is_ok(),
            crc == ZERO_BLOCK_CRC32C
        );
    }

    #[test]
    fn descriptor_words_validate_exactly_when_defined(word in any::<u64>()) {
        let d = StripeDescriptor { vol_lba_kind: word, birth_cp: 1, crc32c: ZERO_BLOCK_CRC32C };
        let defined = d.kind_raw() < 2 && d.vol() <= MAX_VOL;
        prop_assert_eq!(d.validate().is_ok(), defined);
        if defined {
            // Unpack and repack through the constructor reproduces the word
            // when the reserved bits are zero.
            let again = StripeDescriptor::new(d.vol(), d.lba(), d.kind().unwrap(), 1, ZERO_BLOCK_CRC32C).unwrap();
            prop_assert_eq!(again.vol_lba_kind, word & !DESC_RSVD_MASK);
        }
    }

    #[test]
    fn map_addr_roundtrip(
        lseg in 1u64..=MAX_LSEG,
        off in 0u32..=MAX_SEG_OFF,
        plen in 1u16..=SECTORS_PER_BLOCK,
        birth in 1u32..=u32::MAX,
        crc in any::<u32>(),
    ) {
        let e = MapEntry::data(lseg, off, plen, birth, crc).unwrap();
        prop_assert_eq!(e.lseg(), lseg);
        prop_assert_eq!(e.off(), off);
        prop_assert_eq!(e.plen(), plen);
        prop_assert_eq!(e.birth_cp, birth);
        prop_assert_eq!(e.crc32c, crc);
        prop_assert!(e.is_data());
        prop_assert!(!e.is_hole());
        prop_assert!(!e.is_unmapped());
        prop_assert!(e.validate().is_ok());
        prop_assert_eq!(e.addr >> MAP_LSEG_SHIFT, lseg);
    }

    #[test]
    fn map_addr_rejects_out_of_range(
        lseg in (MAX_LSEG + 1)..=u64::MAX,
        off in (MAX_SEG_OFF + 1)..=u32::MAX,
        plen_long in (SECTORS_PER_BLOCK + 1)..=MAX_PLEN,
        plen_wide in (MAX_PLEN + 1)..=u16::MAX,
    ) {
        prop_assert!(
            matches!(
                MapEntry::pack_addr(NULL_LSEG, 0, SECTORS_PER_BLOCK),
                Err(FormatError::BadField { field: "lseg", .. })
            ),
            "segment 0 is reserved"
        );
        prop_assert!(
            matches!(
                MapEntry::pack_addr(lseg, 0, SECTORS_PER_BLOCK),
                Err(FormatError::OutOfRange { field: "lseg", .. })
            ),
            "a segment above 36 bits is out of range"
        );
        prop_assert!(MapEntry::pack_addr(1, off, SECTORS_PER_BLOCK).is_err());
        prop_assert!(
            matches!(
                MapEntry::pack_addr(1, 0, 0),
                Err(FormatError::BadField { field: "plen", .. })
            ),
            "a zero physical length is rejected"
        );
        prop_assert!(
            matches!(
                MapEntry::pack_addr(1, 0, plen_long),
                Err(FormatError::OutOfRange { field: "plen", .. })
            ),
            "a physical length above one block is out of range"
        );
        prop_assert!(MapEntry::pack_addr(1, 0, plen_wide).is_err());
        prop_assert!(MapEntry::pack_addr(1, 0, SECTORS_PER_BLOCK).is_ok());
        prop_assert!(MapEntry::data(1, 0, SECTORS_PER_BLOCK, 0, 0).is_err());
        // Decoding is lenient on the field width; validate() is not.
        let wide = MapEntry { addr: MapEntry::pack_addr(1, 0, 8).unwrap() | u64::from(plen_long), birth_cp: 1, crc32c: 0 };
        prop_assert!(wide.validate().is_err());
    }

    #[test]
    fn map_hole_and_unmapped(birth in 1u32..=u32::MAX, crc in 1u32..=u32::MAX) {
        let hole = MapEntry::hole(birth).unwrap();
        prop_assert!(hole.is_hole());
        prop_assert!(!hole.is_data());
        prop_assert!(!hole.is_unmapped());
        prop_assert!(hole.validate().is_ok());
        prop_assert!(MapEntry::UNMAPPED.is_unmapped());
        prop_assert!(MapEntry::UNMAPPED.validate().is_ok());
        prop_assert!(
            matches!(
                MapEntry::hole(0),
                Err(FormatError::BadField { field: "birth_cp", .. })
            ),
            "a hole needs a birth"
        );
        let hole_with_crc = MapEntry { crc32c: crc, ..hole };
        prop_assert!(hole_with_crc.validate().is_err());
    }

    #[test]
    fn map_page_geometry(lba in 0u64..=MAX_LBA) {
        let page = MapPage::page_of(lba);
        let slot = MapPage::slot_of(lba);
        prop_assert_eq!(MapPage::first_lba(page) + slot as u64, lba);
        prop_assert!(slot < MAP_ENTRIES_PER_PAGE);
        let index_page = PageIndexPage::index_page_of(page);
        let index_slot = PageIndexPage::slot_of(page);
        prop_assert_eq!(index_page * INDEX_ENTRIES_PER_PAGE as u64 + index_slot as u64, page);
    }

    #[test]
    fn map_root_page_counts_and_bounds(lba_count in 0u64..=MAX_LBA + 1, cp in 0u64..=MAX_CP_SEQ) {
        let map_pages = MapRoot::map_pages_for(lba_count);
        let index_pages = MapRoot::index_pages_for(lba_count);
        prop_assert!(map_pages * MAP_ENTRIES_PER_PAGE as u64 >= lba_count);
        prop_assert!(map_pages.saturating_sub(1) * (MAP_ENTRIES_PER_PAGE as u64) < lba_count.max(1));
        prop_assert!(index_pages * INDEX_ENTRIES_PER_PAGE as u64 >= map_pages);
        let root = MapRoot {
            vol: 7,
            lba_count,
            index_pages: index_pages as u32,
            checkpoint_cp_seq: cp,
            ..MapRoot::default()
        };
        prop_assert!(root.validate().is_ok());
        let wrong_index = MapRoot {
            index_pages: index_pages as u32 + 1,
            ..root
        };
        let wrong_vol = MapRoot {
            vol: MAX_VOLS as u32,
            ..root
        };
        let too_big = MapRoot {
            lba_count: MAX_LBA + 2,
            index_pages: MapRoot::index_pages_for(MAX_LBA + 2) as u32,
            ..root
        };
        let late_cp = MapRoot {
            checkpoint_cp_seq: MAX_CP_SEQ + 1,
            ..root
        };
        prop_assert!(wrong_index.validate().is_err());
        prop_assert!(wrong_vol.validate().is_err());
        prop_assert!(too_big.validate().is_err());
        prop_assert!(late_cp.validate().is_err());
    }
}

// ---------------------------------------------------------------------------
// 6. bitsets, ring ranking, boot union, stripe identity
// ---------------------------------------------------------------------------

proptest! {
    #[test]
    fn volbitset_matches_model(
        ops in prop::collection::vec((0u32..(MAX_VOLS as u32 + 100), any::<bool>()), 0..300),
    ) {
        let mut set = VolBitSet::new();
        let mut model = vec![false; MAX_VOLS];
        for &(vol, on) in &ops {
            let result = if on { set.set(vol) } else { set.clear(vol) };
            if (vol as usize) < MAX_VOLS {
                prop_assert!(result.is_ok());
                model[vol as usize] = on;
            } else {
                prop_assert!(result.is_err());
            }
        }
        for vol in 0..MAX_VOLS as u32 {
            prop_assert_eq!(set.get(vol), model[vol as usize]);
        }
        prop_assert!(!set.get(MAX_VOLS as u32));
        prop_assert!(!set.get(u32::MAX));
        let expected: Vec<u32> = model
            .iter()
            .enumerate()
            .filter(|(_, &on)| on)
            .map(|(i, _)| i as u32)
            .collect();
        prop_assert_eq!(set.iter().collect::<Vec<_>>(), expected.clone());
        prop_assert_eq!(set.count() as usize, expected.len());
        prop_assert_eq!(set.is_empty(), expected.is_empty());
        prop_assert_eq!(set.highest(), expected.last().copied());
    }

    #[test]
    fn volbitset_union_is_bitwise_or(
        a in any::<[u64; VOL_BITSET_WORDS]>(),
        b in any::<[u64; VOL_BITSET_WORDS]>(),
    ) {
        let union = bits(a).union(&bits(b));
        for i in 0..VOL_BITSET_WORDS {
            prop_assert_eq!(union.words[i], a[i] | b[i]);
        }
        let mut in_place = bits(a);
        in_place.union_with(&bits(b));
        prop_assert!(in_place == union);
    }

    /// Four slots at their locations with random commits; the ranking picks
    /// the highest (epoch, cp_seq, cut_seq) among the slots that verify, are
    /// ours and sit where they claim, and the A copy on a tie.
    #[test]
    fn rank_picks_newest_verified_slot_a_first(
        cands in prop::collection::vec((0u64..3, 0u64..3, 0u64..3, 0u8..4), SUPERBLOCK_SLOTS),
    ) {
        // (epoch, cp half, cut, fault): fault 0 none, 1 corrupt, 2 other
        // aggregate, 3 slot field claims the other location.
        let ours = uuid(1);
        let slots: Vec<SuperblockSlot> = cands
            .iter()
            .enumerate()
            .map(|(i, &(epoch, half, cut, fault))| {
                let location = SbSlot::from_index(i as u8).unwrap();
                let cp_seq = 2 * half + u64::from(location.parity());
                let agg = if fault == 2 { uuid(9) } else { ours };
                let mut slot = SuperblockSlot::new(agg, cp_seq, location.location());
                slot.sb.owner_epoch = epoch;
                slot.sb.cut_seq = cut;
                if fault == 3 {
                    slot.sb.slot ^= 0b10;
                }
                slot.seal().unwrap();
                if fault == 1 {
                    slot.vols.vols[3].rollback_gen ^= 1;
                }
                slot
            })
            .collect();
        let expected = cands
            .iter()
            .enumerate()
            .filter(|(_, c)| c.3 == 0)
            .map(|(i, c)| {
                let location = SbSlot::from_index(i as u8).unwrap();
                (c.0, 2 * c.1 + u64::from(location.parity()), c.2, location.location() == SbLocation::A, location)
            })
            .max();
        let candidates = slots.iter().enumerate().map(|(i, s)| (SbSlot::from_index(i as u8).unwrap(), s));
        match (SuperblockSlot::select(&ours, candidates), expected) {
            (Ok(got), Some((epoch, cp, cut, _, location))) => {
                prop_assert_eq!((got.sb.owner_epoch, got.sb.cp_seq, got.sb.cut_seq, got.sb.sb_slot()), (epoch, cp, cut, Some(location)));
            }
            (Err(FormatError::NoValidSlot), None) => {}
            (got, expected) => prop_assert!(false, "got {got:?}, expected {expected:?}"),
        }
        let ranked = SuperblockSlot::rank(&ours, slots.iter().enumerate().map(|(i, s)| (SbSlot::from_index(i as u8).unwrap(), s)));
        if let Ok(ranked) = ranked {
            prop_assert_eq!(ranked.len(), cands.iter().filter(|c| c.3 == 0).count());
            for pair in ranked.windows(2) {
                let key = |s: &SuperblockSlot| (s.sb.owner_epoch, s.sb.cp_seq, s.sb.cut_seq);
                prop_assert!(key(pair[0]) >= key(pair[1]));
            }
        }
    }

    #[test]
    fn boot_union_rule(
        seen in 0u64..10,
        base in any::<[u64; VOL_BITSET_WORDS]>(),
        intents in prop::collection::vec(
            (0u64..20, any::<[u64; VOL_BITSET_WORDS]>(), any::<bool>(), any::<bool>()),
            0..6,
        ),
    ) {
        let (ours, theirs) = (uuid(1), uuid(9));
        let mut sb = Superblock::new(ours, 4, SbLocation::A);
        sb.intent_seq_seen = seen;
        sb.vol_count = MAX_VOLS as u32;
        sb.write_active = bits(base);
        sb.seal();
        let slots: Vec<IntentSlot> = intents
            .iter()
            .map(|&(seq, words, same_agg, corrupt)| {
                let mut intent = IntentSlot::new(if same_agg { ours } else { theirs }, seq);
                intent.write_active = bits(words);
                intent.seal();
                if corrupt {
                    intent.reserved[5] ^= 1;
                }
                intent
            })
            .collect();
        let got = boot_write_active(&sb, slots.iter()).unwrap();
        let mut expected = bits(base);
        for &(seq, words, same_agg, corrupt) in &intents {
            if same_agg && !corrupt && seq > seen {
                expected.union_with(&bits(words));
            }
        }
        prop_assert!(got == expected);
    }

    /// Every identity field is checked by parse_expected.
    #[test]
    fn stripe_identity_is_checked(
        descriptors in prop::collection::vec(descriptor(), 1..=8),
        hdr_bytes in prop::collection::vec(any::<u8>(), STRIPE_HDR_SIZE),
        which in 0u8..5,
    ) {
        let mut hdr = sealed_stripe_hdr(&hdr_bytes);
        hdr.nblocks = descriptors.len() as u16;
        let mut page = Block4K::zeroed();
        write_summary(&mut page.0, &hdr, &descriptors).unwrap();
        let id = hdr.id();
        prop_assert!(StripeSummary::parse_expected(&page.0, &id).is_ok());
        let mut other = id;
        let field = match which {
            0 => { other.agg_id = Uuid::from_bytes({ let mut b = *id.agg_id.as_array(); b[0] ^= 1; b }); "agg_id" }
            1 => { other.lseg ^= 1; "lseg" }
            2 => { other.stripe_idx ^= 1; "stripe_idx" }
            3 => { other.cp_seq ^= 1; "cp_seq" }
            _ => { other.cp_nonce ^= 1; "cp_nonce" }
        };
        prop_assert_eq!(
            StripeSummary::parse_expected(&page.0, &other).err(),
            Some(FormatError::IdentityMismatch { field })
        );
    }

    #[test]
    fn summary_layout_is_maximal(blocks in 2usize..=(MAX_STRIPE_BYTES / BLOCK_SIZE)) {
        let stripe = blocks * BLOCK_SIZE;
        let layout = SummaryLayout::for_stripe(stripe).unwrap();
        prop_assert!(layout.nblocks >= 1);
        prop_assert!(SummaryLayout::fits(stripe, layout.nblocks));
        prop_assert!(!SummaryLayout::fits(stripe, layout.nblocks + 1));
        prop_assert_eq!(layout.summary_bytes, summary_bytes(layout.nblocks));
        prop_assert_eq!(layout.summary_bytes % BLOCK_SIZE, 0);
        prop_assert_eq!(layout.data_offset(), layout.summary_bytes);
        prop_assert_eq!(layout.block_offset(layout.nblocks), stripe - layout.slack_bytes());
        prop_assert!(layout.slack_bytes() < 2 * BLOCK_SIZE);
    }
}

#[test]
fn ring_rules_newer_format_and_torn_slots() {
    let ours = uuid(1);
    // Commits 10 (A0, B0) and 11 (A1, B1) of one aggregate.
    let build = |cp_seq: u64, location: SbLocation, epoch: u64| {
        let mut slot = SuperblockSlot::new(ours, cp_seq, location);
        slot.sb.owner_epoch = epoch;
        slot.sb.vol_count = 2;
        slot.vols.vols[0] = VolState {
            acked_bytes: 100 * cp_seq,
            rollback_gen: 1,
            nvfail_cause: 0,
            flags: VOL_PRESENT,
            rsvd: 0,
        };
        slot.vols.vols[1].flags = VOL_PRESENT;
        slot.seal().unwrap();
        slot
    };
    let a0 = build(10, SbLocation::A, 1);
    let b0 = build(10, SbLocation::B, 1);
    let a1 = build(11, SbLocation::A, 1);
    let b1 = build(11, SbLocation::B, 1);
    fn ring(slots: [&SuperblockSlot; 4]) -> impl Iterator<Item = (SbSlot, &SuperblockSlot)> {
        SbSlot::ALL.into_iter().zip(slots)
    }

    // Whole ring: commit 11 wins; both copies are ranked, A first.
    let ranked = SuperblockSlot::rank(&ours, ring([&a0, &a1, &b0, &b1])).unwrap();
    assert_eq!(ranked.len(), 4);
    assert_eq!(ranked[0].sb.cp_seq, 11);
    assert_eq!(ranked[0].sb.sb_slot(), Some(SbSlot::A1));
    assert_eq!(ranked[1].sb.sb_slot(), Some(SbSlot::B1));
    assert_eq!(ranked[2].sb.cp_seq, 10);

    // Commit 11 tore: B1 has the new page over the old table. The slot is
    // refused as a whole, and A1 still carries commit 11.
    let mut torn = b1;
    torn.vols = b0.vols;
    assert!(matches!(
        torn.verify(),
        Err(FormatError::BadCrc {
            region: "volume table",
            ..
        })
    ));
    let selected = SuperblockSlot::select(&ours, ring([&a0, &a1, &b0, &torn])).unwrap();
    assert_eq!(selected.sb.sb_slot(), Some(SbSlot::A1));
    // Both copies of 11 torn: commit 10 is the answer.
    let mut torn_a = a1;
    torn_a.sb.reserved[0] ^= 1;
    let selected = SuperblockSlot::select(&ours, ring([&a0, &torn_a, &b0, &torn])).unwrap();
    assert_eq!(selected.sb.cp_seq, 10);
    assert_eq!(selected.sb.sb_slot(), Some(SbSlot::A0));

    // A slot at the wrong location (a misdirected write) is skipped.
    let selected = SuperblockSlot::select(&ours, ring([&a0, &b1, &b0, &a1])).unwrap();
    assert_eq!(selected.sb.cp_seq, 10);

    // A higher epoch beats a higher cp_seq.
    let promoted = build(10, SbLocation::A, 2);
    let selected = SuperblockSlot::select(&ours, ring([&promoted, &a1, &b0, &b1])).unwrap();
    assert_eq!((selected.sb.owner_epoch, selected.sb.cp_seq), (2, 10));

    // Another aggregate's slots never qualify.
    assert!(matches!(
        SuperblockSlot::select(&uuid(2), ring([&a0, &a1, &b0, &b1])),
        Err(FormatError::NoValidSlot)
    ));

    // A newer major anywhere in the ring is a hard stop, even with a bad CRC
    // and even when older valid commits exist.
    let mut newer = b1;
    newer.sb.ver = (u16::from(FORMAT_MAJOR) + 1) << 8;
    assert!(newer.sb.is_newer_format());
    assert!(matches!(
        SuperblockSlot::select(&ours, ring([&a0, &a1, &b0, &newer])),
        Err(FormatError::NewerFormat { found }) if found == 0x0300
    ));
    // A newer-major intent slot stops the boot union the same way.
    let mut intent = IntentSlot::new(ours, 3);
    intent.ver = 0x0300;
    assert!(matches!(
        boot_write_active(&a1.sb, [&intent]),
        Err(FormatError::NewerFormat { found: 0x0300 })
    ));
    intent.ver = FORMAT_VERSION;
    intent.write_active.set(1).unwrap();
    intent.seal();
    let active = boot_write_active(&a1.sb, [&intent]).unwrap();
    assert!(active.get(1));
}

#[test]
fn ring_slots_and_commit_order() {
    assert_eq!(SbSlot::ALL.map(SbSlot::index), [0, 1, 2, 3]);
    for slot in SbSlot::ALL {
        assert_eq!(SbSlot::from_index(slot.index()), Some(slot));
        assert_eq!(slot.parity(), slot.index() & 1);
    }
    assert_eq!(SbSlot::from_index(4), None);
    assert_eq!(SbSlot::A0.location(), SbLocation::A);
    assert_eq!(SbSlot::B1.location(), SbLocation::B);
    assert_eq!(SbSlot::for_commit(10), [SbSlot::A0, SbSlot::B0]);
    assert_eq!(SbSlot::for_commit(11), [SbSlot::A1, SbSlot::B1]);
    assert_eq!(Superblock::slot_for(11, SbLocation::B), SbSlot::B1);
    assert_eq!(IntentSlot::slot_for(6), 0);
    assert_eq!(IntentSlot::slot_for(7), 1);
    assert_eq!(RunState::from_u8(1), Some(RunState::Running));
    assert_eq!(RunState::from_u8(2), Some(RunState::CleanShutdown));
    assert_eq!(RunState::from_u8(0), None);
    assert_eq!(NvCause::from_u8(3), Some(NvCause::Promote));
    assert_eq!(NvCause::from_u8(4), None);
    assert_eq!(StripeKind::from_u8(3), Some(StripeKind::Merge));
    assert_eq!(StripeKind::from_u8(4), None);
    assert_eq!(FrameKind::from_u8(12), Some(FrameKind::Pong));
    assert_eq!(FrameKind::from_u8(0), None);
    assert_eq!(FrameKind::from_u8(13), None);
    assert_eq!(Codec::from_u8(2), Some(Codec::Zstd));
    assert_eq!(Codec::from_u8(3), None);
}

#[test]
fn summary_layout_known_values() {
    // A 2.5 MiB RAID6 stripe (10 x 256 KiB data) carries exactly 637 blocks:
    // 637 x 4096 data + 12288 summary = 2621440.
    let hdd = SummaryLayout::for_stripe(2_621_440).unwrap();
    assert_eq!(hdd.nblocks, 637);
    assert_eq!(hdd.summary_bytes, 12_288);
    assert_eq!(hdd.slack_bytes(), 0);
    // A 1 MiB NVMe write unit: 254 blocks with two summary pages fill it
    // exactly (254 x 4096 + 8192 = 1048576); 255 would need a third block.
    let nvme = SummaryLayout::for_stripe(1 << 20).unwrap();
    assert_eq!(nvme.nblocks, 254);
    assert_eq!(nvme.summary_bytes, 8192);
    assert_eq!(nvme.slack_bytes(), 0);
    // The largest stripe: 2039 blocks and nine summary pages fill 8 MiB
    // exactly (2039 x 4096 + 36864 = 8388608). And the smallest.
    let big = SummaryLayout::for_stripe(MAX_STRIPE_BYTES).unwrap();
    assert_eq!(big.nblocks, 2039);
    assert_eq!(big.summary_bytes, 36_864);
    assert_eq!(big.slack_bytes(), 0);
    let tiny = SummaryLayout::for_stripe(MIN_STRIPE_BYTES).unwrap();
    assert_eq!(tiny.nblocks, 1);
    assert!(SummaryLayout::for_stripe(BLOCK_SIZE).is_err());
    assert!(SummaryLayout::for_stripe(3 * BLOCK_SIZE + 1).is_err());
    assert!(SummaryLayout::for_stripe(MAX_STRIPE_BYTES + BLOCK_SIZE).is_err());
    assert!(SummaryLayout::for_stripe(1 << 41).is_err());
    assert_eq!(summary_bytes(0), 4096);
    assert_eq!(summary_bytes(244), 4096);
    assert_eq!(summary_bytes(245), 8192);
    assert_eq!(summary_bytes(u16::MAX as usize), 1_052_672);
    assert_eq!(descriptor_offset(0), 192);
    assert_eq!(descriptor_offset(10), 352);
}

// ---------------------------------------------------------------------------
// 7. version policy, enumerated fields, errors
// ---------------------------------------------------------------------------

#[test]
fn minor_versions_are_accepted_and_other_majors_are_not() {
    let mut sb = Superblock::new(uuid(2), 5, SbLocation::A);
    sb.ver = FORMAT_VERSION + 1;
    sb.reserved[..4].copy_from_slice(&[1, 2, 3, 4]);
    sb.seal();
    assert!(sb.verify().is_ok(), "a later minor must be readable");
    assert_eq!(
        Superblock::read(sb.as_bytes()).unwrap().reserved[..4],
        [1, 2, 3, 4]
    );
    sb.ver = (u16::from(FORMAT_MAJOR) + 1) << 8;
    sb.seal();
    assert!(matches!(sb.verify(), Err(FormatError::BadVersion { .. })));
    assert!(sb.is_newer_format());
    sb.ver = (u16::from(FORMAT_MAJOR) - 1) << 8;
    sb.seal();
    assert!(matches!(sb.verify(), Err(FormatError::BadVersion { .. })));
    assert!(!sb.is_newer_format());

    let mut frame = StreamHeader::new(FrameKind::Ping, Codec::Raw);
    frame.ver = STREAM_VERSION + 3;
    frame.seal();
    assert!(frame.verify().is_ok());
    frame.ver = (u16::from(STREAM_MAJOR) + 1) << 8;
    frame.seal();
    assert!(matches!(
        frame.verify(),
        Err(FormatError::BadVersion { .. })
    ));
}

#[test]
fn enumerated_fields_and_cross_checks() {
    let fresh = || {
        let mut sb = Superblock::new(uuid(3), 8, SbLocation::B);
        sb.seal();
        sb
    };
    assert!(fresh().verify().is_ok());
    assert_eq!(fresh().sb_slot(), Some(SbSlot::B0));

    let mut sb = fresh();
    sb.run_state = 0;
    sb.seal();
    assert!(matches!(
        sb.verify(),
        Err(FormatError::BadField {
            field: "run_state",
            ..
        })
    ));
    sb.run_state = 3;
    sb.seal();
    assert!(sb.verify().is_err());

    let mut sb = fresh();
    sb.slot = 4;
    sb.seal();
    assert!(matches!(
        sb.verify(),
        Err(FormatError::BadField { field: "slot", .. })
    ));
    sb.slot = SbSlot::B1.index(); // wrong parity for cp_seq 8
    sb.seal();
    assert!(matches!(
        sb.verify(),
        Err(FormatError::BadField { field: "slot", .. })
    ));

    let mut sb = fresh();
    sb.cp_seq = MAX_CP_SEQ + 2;
    sb.seal();
    assert!(matches!(
        sb.verify(),
        Err(FormatError::OutOfRange {
            field: "cp_seq",
            ..
        })
    ));

    let mut sb = fresh();
    sb.vol_count = MAX_VOLS as u32 + 1;
    sb.seal();
    assert!(matches!(
        sb.verify(),
        Err(FormatError::OutOfRange {
            field: "vol_count",
            ..
        })
    ));

    // Bits at or above vol_count are refused.
    let mut sb = fresh();
    sb.vol_count = 4;
    sb.write_active.set(4).unwrap();
    sb.seal();
    assert!(matches!(
        sb.verify(),
        Err(FormatError::BadField {
            field: "write_active",
            ..
        })
    ));
    let mut sb = fresh();
    sb.vol_count = 4;
    sb.nvfail.set(1000).unwrap();
    sb.seal();
    assert!(matches!(
        sb.verify(),
        Err(FormatError::BadField {
            field: "nvfail",
            ..
        })
    ));

    // The durable-log reservation: undefined kinds, and stray bytes under kind 0.
    for kind in [2u8, 0x80, 0xFF] {
        let mut sb = fresh();
        sb.durable_log.kind = kind;
        sb.seal();
        assert!(matches!(
            sb.verify(),
            Err(FormatError::BadField {
                field: "durable_log.kind",
                ..
            })
        ));
    }
    let mut sb = fresh();
    sb.durable_log.ring_bytes = 1;
    sb.seal();
    assert!(matches!(
        sb.verify(),
        Err(FormatError::BadField {
            field: "durable_log",
            ..
        })
    ));
    let mut sb = fresh();
    sb.durable_log.kind = DURABLE_LOG_LOCAL_NVME_RING;
    sb.durable_log.ring_bytes = 1 << 30;
    sb.seal();
    assert!(sb.verify().is_ok());

    let mut sb = fresh();
    sb.set_run_state(RunState::CleanShutdown);
    sb.seal();
    assert!(sb.verify().is_ok());
    assert!(sb.is_clean_shutdown());
    assert_eq!(sb.commit_key(), (0, 8));

    // The slot's page/table agreement.
    let fresh_slot = || {
        let mut slot = SuperblockSlot::new(uuid(3), 8, SbLocation::A);
        slot.sb.vol_count = 2;
        slot.vols.vols[0].flags = VOL_PRESENT;
        slot.vols.vols[1].flags = VOL_PRESENT;
        slot
    };
    let mut slot = fresh_slot();
    slot.seal().unwrap();
    assert!(slot.verify().is_ok());

    for cause in [4u8, 0x80, 0xFF] {
        let mut slot = fresh_slot();
        slot.vols.vols[0].nvfail_cause = cause;
        slot.seal().unwrap();
        assert!(matches!(
            slot.verify(),
            Err(FormatError::BadField {
                field: "nvfail_cause",
                ..
            })
        ));
    }
    let mut slot = fresh_slot();
    slot.vols.vols[0].nvfail_cause = NvCause::TornCp.as_u8(); // cause without the bit
    slot.seal().unwrap();
    assert!(matches!(
        slot.verify(),
        Err(FormatError::BadField {
            field: "nvfail",
            value: 0
        })
    ));
    let mut slot = fresh_slot();
    slot.sb.nvfail.set(1).unwrap(); // bit without a cause
    slot.seal().unwrap();
    assert!(matches!(
        slot.verify(),
        Err(FormatError::BadField {
            field: "nvfail",
            value: 1
        })
    ));
    let mut slot = fresh_slot();
    slot.sb.nvfail.set(1).unwrap();
    slot.vols.vols[1].nvfail_cause = NvCause::DirtyBoot.as_u8();
    slot.seal().unwrap();
    assert!(slot.verify().is_ok());

    let mut slot = fresh_slot();
    slot.vols.vols[2].flags = VOL_PRESENT; // present at or above vol_count
    slot.seal().unwrap();
    assert!(matches!(
        slot.verify(),
        Err(FormatError::BadField {
            field: "vol_count",
            value: 2
        })
    ));
    let mut slot = fresh_slot();
    slot.vols.vols[1].flags = 0;
    slot.vols.vols[1].acked_bytes = 10; // absent but accounted
    slot.seal().unwrap();
    assert!(matches!(
        slot.verify(),
        Err(FormatError::BadField {
            field: "vol_present",
            value: 1
        })
    ));
    let mut slot = fresh_slot();
    slot.vols.vols[1].flags = 0;
    slot.sb.write_active.set(1).unwrap(); // active but absent
    slot.seal().unwrap();
    assert!(matches!(
        slot.verify(),
        Err(FormatError::BadField {
            field: "write_active",
            value: 1
        })
    ));
    let mut slot = fresh_slot();
    slot.vols.vols[0].acked_bytes = 4096;
    slot.seal().unwrap();
    assert_eq!(slot.sb.acked_bytes, 4096);
    slot.sb.acked_bytes = 4095; // the sum is stale
    slot.sb.seal();
    assert!(matches!(
        slot.verify(),
        Err(FormatError::BadField {
            field: "acked_bytes",
            ..
        })
    ));
    let mut slot = fresh_slot();
    slot.vols.vols[0].acked_bytes = u64::MAX;
    slot.vols.vols[1].acked_bytes = 1;
    assert!(slot.seal().is_err(), "an overflowing sum cannot be sealed");

    // Intent slots: the slot index follows intent_seq.
    let mut intent = IntentSlot::new(uuid(3), 6);
    assert_eq!(intent.slot, 0);
    intent.slot = 1;
    intent.seal();
    assert!(matches!(
        intent.verify(),
        Err(FormatError::BadField { field: "slot", .. })
    ));
    intent.slot = 0;
    intent.cp_seq = MAX_CP_SEQ + 1;
    intent.seal();
    assert!(intent.verify().is_err());

    // Stripe headers.
    let mut hdr = StripeHdr::new(uuid(3), StripeKind::Ingest);
    hdr.lseg = 1;
    hdr.nblocks = 1;
    hdr.seal_hdr();
    assert!(hdr.verify_hdr().is_ok());
    hdr.kind = 4;
    hdr.seal_hdr();
    assert!(matches!(
        hdr.verify_hdr(),
        Err(FormatError::BadField { field: "kind", .. })
    ));
    hdr.kind = StripeKind::Relocate.as_u8();
    hdr.flags = 0xFF; // reserved flag bits are ignored on disk (N/N-1 leniency)
    hdr.seal_hdr();
    assert!(hdr.verify_hdr().is_ok());
    assert_eq!(hdr.stripe_kind(), Some(StripeKind::Relocate));
    hdr.lseg = 0;
    hdr.seal_hdr();
    assert!(matches!(
        hdr.verify_hdr(),
        Err(FormatError::BadField { field: "lseg", .. })
    ));
    hdr.lseg = 1;
    hdr.nblocks = 0;
    hdr.seal_hdr();
    assert!(matches!(
        hdr.verify_hdr(),
        Err(FormatError::BadField {
            field: "nblocks",
            ..
        })
    ));
    hdr.nblocks = 1;
    hdr.cp_seq = MAX_CP_SEQ + 1;
    hdr.seal_hdr();
    assert!(hdr.verify_hdr().is_err());

    // Stream headers.
    let mut frame = StreamHeader::new(FrameKind::Chunk, Codec::Raw);
    frame.kind = 0;
    frame.seal();
    assert!(matches!(
        frame.verify(),
        Err(FormatError::BadField { field: "kind", .. })
    ));
    frame.kind = 13;
    frame.seal();
    assert!(frame.verify().is_err());
    frame.kind = FrameKind::Chunk.as_u8();
    frame.flags = 3;
    frame.seal();
    assert!(matches!(
        frame.verify(),
        Err(FormatError::BadField { field: "codec", .. })
    ));
    frame.flags = 0b100; // reserved wire flag bits are rejected (strict on the wire)
    frame.seal();
    assert!(matches!(
        frame.verify(),
        Err(FormatError::BadField { field: "flags", .. })
    ));
    frame.flags = Codec::Raw.as_u8();
    frame.wire_len = 10;
    frame.raw_len = 11;
    frame.seal();
    assert!(matches!(
        frame.verify(),
        Err(FormatError::BadField {
            field: "raw_len",
            ..
        })
    ));
    frame.flags = Codec::Lz4.as_u8();
    frame.seal();
    assert!(frame.verify().is_ok(), "a codec may change the length");
    assert_eq!(frame.codec(), Some(Codec::Lz4));
    assert_eq!(frame.frame_kind(), Some(FrameKind::Chunk));
    frame.raw_len = MAX_WIRE_PAYLOAD + 1;
    frame.seal();
    assert!(matches!(
        frame.verify(),
        Err(FormatError::OutOfRange {
            field: "raw_len",
            ..
        })
    ));
    frame.raw_len = 11;
    frame.wire_len = MAX_WIRE_PAYLOAD + 1;
    frame.seal();
    assert!(matches!(
        frame.verify(),
        Err(FormatError::OutOfRange {
            field: "wire_len",
            ..
        })
    ));
    frame.wire_len = 10;
    frame.cp_seq = 5;
    frame.base = 6;
    frame.seal();
    assert!(matches!(
        frame.verify(),
        Err(FormatError::BadField { field: "base", .. })
    ));
    frame.base = 5;
    frame.cp_seq = MAX_CP_SEQ + 1;
    frame.seal();
    assert!(frame.verify().is_err());

    // The payload setters follow the codec.
    let payload = [7u8; 16];
    let mut frame = StreamHeader::new(FrameKind::Chunk, Codec::Raw);
    assert!(frame.set_encoded_payload(&payload, 16).is_err());
    assert!(frame.set_payload(&payload).is_ok());
    assert_eq!((frame.wire_len, frame.raw_len), (16, 16));
    assert_eq!(frame.payload_crc, crc32c(&payload));
    let too_big = vec![0u8; MAX_WIRE_PAYLOAD as usize + 1];
    assert!(frame.set_payload(&too_big).is_err());
    let mut frame = StreamHeader::new(FrameKind::Chunk, Codec::Zstd);
    assert!(frame.set_payload(&payload).is_err());
    assert!(frame
        .set_encoded_payload(&payload, MAX_WIRE_PAYLOAD as usize + 1)
        .is_err());
    assert!(frame.set_encoded_payload(&payload, 4096).is_ok());
    assert_eq!((frame.wire_len, frame.raw_len), (16, 4096));
    frame.seal();
    assert!(frame.verify().is_ok());

    // Hole records.
    assert!(HoleRecord::new(0, 4096, 16).is_ok());
    assert!(matches!(
        HoleRecord::new(MAX_VOLS as u32, 0, 1),
        Err(FormatError::OutOfRange { field: "vol", .. })
    ));
    assert!(matches!(
        HoleRecord::new(0, 0, 0),
        Err(FormatError::BadField { field: "len", .. })
    ));
    assert!(HoleRecord::new(0, MAX_LBA - 7, 8).is_ok());
    assert!(HoleRecord::new(0, MAX_LBA - 6, 8).is_err());
    let overflowing = HoleRecord {
        vol: 7,
        len: 16,
        lba: u64::MAX - 1,
    };
    assert_eq!(overflowing.end_lba(), None);
    assert!(overflowing.validate().is_err());
    assert_eq!(HoleRecord::new(1, 10, 5).unwrap().end_lba(), Some(15));
}

#[test]
fn magic_length_and_alignment_errors() {
    let mut sb = Superblock::new(uuid(4), 2, SbLocation::A);
    sb.seal();
    let mut buf = TwoBlocks([0; 2 * BLOCK_SIZE]);
    buf.0[..SUPERBLOCK_SIZE].copy_from_slice(sb.as_bytes());

    assert!(Superblock::parse(&buf.0[..SUPERBLOCK_SIZE]).is_ok());
    assert!(matches!(
        Superblock::parse(&buf.0[..SUPERBLOCK_SIZE - 1]),
        Err(FormatError::BadLength {
            expected: SUPERBLOCK_SIZE,
            found: 4095
        })
    ));
    assert!(matches!(
        Superblock::parse(&buf.0),
        Err(FormatError::BadLength {
            expected: SUPERBLOCK_SIZE,
            ..
        })
    ));
    assert!(matches!(
        Superblock::read(&buf.0[..SUPERBLOCK_SIZE - 1]),
        Err(FormatError::BadLength { .. })
    ));

    // A misaligned slice is refused as a view and accepted as a copy.
    buf.0.copy_within(..SUPERBLOCK_SIZE, 1);
    let shifted = &buf.0[1..SUPERBLOCK_SIZE + 1];
    assert!(matches!(
        Superblock::parse(shifted),
        Err(FormatError::Misaligned { required: 4096 })
    ));
    let copied = Superblock::read(shifted).unwrap();
    assert_eq!(copied, sb);

    let mut wrong_magic = sb;
    wrong_magic.magic ^= 1;
    wrong_magic.seal();
    assert!(matches!(
        wrong_magic.verify(),
        Err(FormatError::BadMagic {
            expected: SUPERBLOCK_MAGIC,
            ..
        })
    ));

    // Errors print something an operator can act on.
    let shown = format!(
        "{}",
        FormatError::BadCrc {
            region: "volume table",
            computed: 1,
            stored: 2
        }
    );
    assert!(shown.contains("volume table"));
    assert!(format!("{}", FormatError::NewerFormat { found: 0x0300 }).contains("newer"));
    assert!(format!("{}", FormatError::NoValidSlot).contains("no superblock slot"));
    assert_eq!(
        format!("{}", uuid(0)),
        "00010203-0405-0607-0809-0a0b0c0d0e0f"
    );
    assert!(Uuid::NIL.is_nil());
    assert!(!uuid(0).is_nil());
}

// ---------------------------------------------------------------------------
// 8. in-place construction without heap allocation
// ---------------------------------------------------------------------------

#[test]
fn builds_in_place_in_aligned_buffers() {
    let mut buf = AlignedSlot([0; SUPERBLOCK_SLOT_SIZE]);
    {
        let slot = SuperblockSlot::mut_from_bytes(&mut buf.0).unwrap();
        *slot = SuperblockSlot::new(uuid(5), 77, SbLocation::B);
        slot.sb.owner_epoch = 3;
        slot.sb.vol_count = 13;
        slot.vols.vols[12].flags = VOL_PRESENT;
        slot.vols.vols[12].acked_bytes = 512;
        slot.sb.write_active.set(12).unwrap();
        slot.seal().unwrap();
    }
    let slot = SuperblockSlot::parse(&buf.0).unwrap();
    assert_eq!(slot.sb.cp_seq, 77);
    assert_eq!(slot.sb.sb_slot(), Some(SbSlot::B1));
    assert!(slot.sb.write_active.get(12));
    assert_eq!(slot.sb.acked_bytes, 512);

    let mut page = Page16K::zeroed();
    let count = 100;
    {
        let descs =
            <[StripeDescriptor]>::mut_from_bytes(&mut page.0[..count * STRIPE_DESCRIPTOR_SIZE])
                .unwrap();
        for (i, d) in descs.iter_mut().enumerate() {
            *d = StripeDescriptor::data(7, i as u64, 9, i as u32).unwrap();
        }
    }
    let descs =
        <[StripeDescriptor]>::ref_from_bytes(&page.0[..count * STRIPE_DESCRIPTOR_SIZE]).unwrap();
    assert_eq!(descs.len(), count);
    assert_eq!(descs[99].lba(), 99);
    assert_eq!(descs[99].vol(), 7);
    assert!(
        <[StripeDescriptor]>::ref_from_bytes(&page.0[..count * STRIPE_DESCRIPTOR_SIZE + 1])
            .is_err()
    );

    let mut page = Page16K::zeroed();
    {
        let map = MapPage::mut_from_bytes(&mut page.0).unwrap();
        map.entries[0] = MapEntry::data(5, 9, SECTORS_PER_BLOCK, 4, 0xDEAD_BEEF).unwrap();
        map.entries[1023] = MapEntry::hole(6).unwrap();
    }
    let map = MapPage::ref_from_bytes(&page.0).unwrap();
    assert_eq!(map.max_birth(), 6);
    assert!(map.entries[0].is_data());
    assert!(map.entries[1023].is_hole());
    assert!(map.entries[512].is_unmapped());
    assert!(map.validate().is_ok());
    assert_eq!(map.crc32c(), crc32c(&page.0));
    let entry = map.index_entry(8192);
    assert!(MapPage::parse(&page.0, &entry).is_ok());
    assert!(MapPage::parse(&page.0, &PageIndexEntry::NONE).is_err());
    assert!(MapPage::parse(&page.0, &PageIndexEntry::new(8192, 5, entry.page_crc)).is_err());

    // A page index over that page, verified through its root.
    let mut index = PageIndexPage::zeroed();
    index.entries[0] = entry;
    assert!(index.validate().is_ok());
    index.entries[1] = PageIndexEntry::new(0, 1, 0);
    assert!(index.validate().is_err());
    index.entries[1] = PageIndexEntry::NONE;
    let root = MapRoot {
        vol: 1,
        index_pba: 4096,
        index_pages: 1,
        index_crc: index.crc32c(),
        lba_count: 1 << 20,
        checkpoint_cp_seq: 6,
        max_birth: 6,
        ..MapRoot::default()
    };
    assert!(root.verify_index(index.as_bytes()).is_ok());
    assert!(root
        .verify_index(&index.as_bytes()[..INDEX_PAGE_SIZE - 16])
        .is_err());
    let mut other = index;
    other.entries[5].max_birth = 1;
    assert!(matches!(
        root.verify_index(other.as_bytes()),
        Err(FormatError::BadCrc {
            region: "page index",
            ..
        })
    ));
}

#[test]
fn summary_builders_reject_bad_inputs() {
    let mut hdr = StripeHdr::new(uuid(6), StripeKind::Ingest);
    hdr.lseg = 4;
    hdr.nblocks = 2;
    let descs = [
        StripeDescriptor::data(1, 1, 1, 1).unwrap(),
        StripeDescriptor::data(1, 2, 1, 2).unwrap(),
        StripeDescriptor::data(3, 3, 1, 3).unwrap(),
    ];
    let mut page = Page16K::zeroed();
    assert!(matches!(
        write_summary(&mut page.0, &hdr, &descs),
        Err(FormatError::BadField {
            field: "nblocks",
            ..
        })
    ));
    assert!(matches!(
        write_summary(&mut page.0[..100], &hdr, &descs[..2]),
        Err(FormatError::BadLength {
            expected: 4096,
            found: 100
        })
    ));
    // A header that parse would refuse is refused before anything is written.
    let zeroed = StripeHdr::new_zeroed();
    assert!(matches!(
        write_summary(&mut page.0, &zeroed, &descs[..2]),
        Err(FormatError::BadMagic { .. })
    ));
    assert!(page.0.iter().all(|b| *b == 0));
    let mut no_segment = hdr;
    no_segment.lseg = 0;
    assert!(matches!(
        write_summary(&mut page.0, &no_segment, &descs[..2]),
        Err(FormatError::BadField { field: "lseg", .. })
    ));
    // An invalid descriptor is refused too.
    let bad_desc = StripeDescriptor {
        vol_lba_kind: StripeDescriptor::pack(1, 1, BlockKind::Hole).unwrap(),
        birth_cp: 1,
        crc32c: 1,
    };
    assert!(matches!(
        write_summary(&mut page.0, &hdr, &[descs[0], bad_desc]),
        Err(FormatError::BadField {
            field: "crc32c",
            ..
        })
    ));

    assert_eq!(write_summary(&mut page.0, &hdr, &descs[..2]), Ok(4096));
    let parsed = StripeSummary::parse(&page.0[..4096]).unwrap();
    assert_eq!(parsed.descriptors().len(), 2);
    assert_eq!(parsed.hdr().data_bytes(), 2 * BLOCK_SIZE);
    assert!(parsed.vol_bitmap().get(1));
    assert!(!parsed.vol_bitmap().get(3));
    assert!(StripeSummary::parse(&page.0[..4095]).is_err());

    // A stored bitmap that disagrees with the descriptors is refused, even
    // though it is covered by the summary CRC.
    let mut lying = page;
    lying.0[STRIPE_VOL_BITMAP_OFFSET] |= 0b1000; // claims volume 3
    let total = {
        let crc = crc32c(&lying.0[STRIPE_VOL_BITMAP_OFFSET..4096]);
        let mut h = StripeHdr::read_from_bytes(&lying.0[..64]).unwrap();
        h.summary_crc = crc;
        h.seal_hdr();
        lying.0[..64].copy_from_slice(h.as_bytes());
        4096
    };
    assert!(matches!(
        StripeSummary::parse(&lying.0[..total]),
        Err(FormatError::BadField {
            field: "vol_bitmap",
            ..
        })
    ));

    // In-place sealing: a reserved descriptor kind and dirty padding are
    // reported, not sealed over.
    let mut bad = page;
    let off = descriptor_offset(1);
    bad.0[off] |= 0b1000; // kind bits 2..4 -> 2 (reserved)
    assert!(matches!(
        seal_summary_in_place(&mut bad.0),
        Err(FormatError::BadField { field: "kind", .. })
    ));
    let mut dirty = page;
    dirty.0[descriptor_offset(2) + 3] = 0xAA;
    assert!(matches!(
        seal_summary_in_place(&mut dirty.0),
        Err(FormatError::BadField {
            field: "padding",
            value
        }) if value as usize == descriptor_offset(2) + 3
    ));
    // Sealing works at any alignment of the region.
    let mut unaligned = vec![0u8; 4096 + 1];
    unaligned[1..].copy_from_slice(&page.0[..4096]);
    assert_eq!(seal_summary_in_place(&mut unaligned[1..]), Ok(4096));
    assert_eq!(&unaligned[1..], &page.0[..4096]);
}

// ---------------------------------------------------------------------------
// 9. golden vectors: the v2.0 and v2.1 / v1.0 decode lanes
// ---------------------------------------------------------------------------

/// A stored vector set: its directory under `tests/vectors/`, the pinned
/// BLAKE3 of its manifest, the format version that wrote it, the manifest's
/// first line and the files it holds. Every set is decoded by the current
/// reader on every run, which is the N / N-1 lane; the newest set is also
/// compared byte for byte with the current builders.
struct VectorSet {
    dir: &'static str,
    manifest_blake3: &'static str,
    format_version: u16,
    header: &'static str,
    names: &'static [&'static str],
}

const V2_0_NAMES: [&str; 12] = [
    "superblock_slot.bin",
    "superblock_slot_minor1.bin",
    "intent_slot.bin",
    "stripe_summary.bin",
    "map_page.bin",
    "page_index_page.bin",
    "map_root.bin",
    "stream_frame.bin",
    "hole_records.bin",
    "seg_entries.bin",
    "snap_records.bin",
    "cp_log_page.bin",
];

const V2_1_NAMES: [&str; 14] = [
    "superblock_slot.bin",
    "superblock_slot_minor1.bin",
    "intent_slot.bin",
    "stripe_summary.bin",
    "map_page.bin",
    "page_index_page.bin",
    "map_root.bin",
    "stream_frame.bin",
    "hole_records.bin",
    "seg_entries.bin",
    "snap_records.bin",
    "cp_log_page.bin",
    "cp_log_dir.bin",
    "snap_root_page.bin",
];

/// Format 2.2 adds fields to the superblock only; the set has the same
/// images, rewritten by the 2.2 builders.
const V2_2_NAMES: [&str; 14] = V2_1_NAMES;

/// The stored sets, oldest first. Regenerating a set is a format change:
/// bump `FORMAT_MINOR` or `STREAM_MINOR`, add a set here with a new
/// directory, run `cargo test -p otap-format -- --ignored
/// write_golden_vectors` and pin the value it prints, keeping every previous
/// directory for the N-1 lane.
const VECTOR_SETS: [VectorSet; 3] = [
    VectorSet {
        dir: "v2.0",
        manifest_blake3: "73b1f4d3a6729fe19b0d13f050aaaed33165d7f07584fd26174c7861332b3af6",
        format_version: 0x0200,
        header: "otap-format golden vectors: format 2.0 stream 1.0",
        names: &V2_0_NAMES,
    },
    VectorSet {
        dir: "v2.1",
        manifest_blake3: "e85ac0be1e18d74d8ce9d2278f5d1ca39bce5660e2e5a8fb835d38f05513ac61",
        format_version: 0x0201,
        header: "otap-format golden vectors: format 2.1 stream 1.0",
        names: &V2_1_NAMES,
    },
    VectorSet {
        dir: "v2.2",
        manifest_blake3: "3f9a9651f60d7d2b1e4189517653d623c128de4ed50bc55b40c8bd953bf8168d",
        format_version: 0x0202,
        header: "otap-format golden vectors: format 2.2 stream 1.0",
        names: &V2_2_NAMES,
    },
];

/// The set the current builders write.
fn current_set() -> &'static VectorSet {
    &VECTOR_SETS[VECTOR_SETS.len() - 1]
}

fn vectors_dir(set: &VectorSet) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("vectors")
        .join(set.dir)
}

const GOLDEN_NONCE: u64 = 0x0123_4567_89AB_CDEF;

fn golden_slot() -> SuperblockSlot {
    let mut slot = SuperblockSlot::new(uuid(0), 42, SbLocation::B);
    let sb = &mut slot.sb;
    sb.owner_epoch = 7;
    sb.cut_seq = 43;
    sb.cp_nonce = GOLDEN_NONCE;
    sb.intent_seq_seen = 9;
    sb.opened_stamp = 1;
    sb.wall_ns = 1_760_000_000_000_000_000;
    sb.vol_count = 3;
    sb.write_active.set(0).unwrap();
    sb.write_active.set(2).unwrap();
    sb.nvfail.set(1).unwrap();
    sb.set_geometry(32 * 1024, 256 * 1024, 64 << 20);
    sb.freed_pending = [
        RootRef::new(100, 1, 0xAAAA_AAAA),
        RootRef::new(101, 2, 0xBBBB_BBBB),
    ];
    sb.map_roots = RootRef::new(200, 4, 1);
    sb.seg_table = RootRef::new(300, 1, 2);
    sb.reloc_index = RootRef::NONE;
    sb.cp_log_head = RootRef::new(400, 16, 3);
    sb.snap_table = RootRef::new(500, 1, 4);
    sb.cluster_state = RootRef::new(600, 8, 5);
    sb.open_data_pseg = 6;
    sb.open_data_idx = 3;
    sb.inbound[0] = InboundState {
        applied_cp_seq: 40,
        applied_owner_epoch: 6,
        acked_cp_seq: 39,
        recv_mono_ns: 123_456_789,
        sender_cut_wall_ns: 1_759_999_999_000_000_000,
        recv_boot_id: uuid(8),
        cp_age_at_send_ms: 1500,
        ship_duration_ms: 2500,
        group: 1,
        flags: INBOUND_VALID,
        rsvd: 0,
    };
    slot.vols.vols[0] = VolState {
        acked_bytes: 4096,
        rollback_gen: 1,
        nvfail_cause: 0,
        flags: VOL_PRESENT,
        rsvd: 0,
    };
    slot.vols.vols[1] = VolState {
        acked_bytes: 0,
        rollback_gen: 2,
        nvfail_cause: NvCause::DirtyBoot.as_u8(),
        flags: VOL_PRESENT,
        rsvd: 0,
    };
    slot.vols.vols[2] = VolState {
        acked_bytes: 4096,
        rollback_gen: 0,
        nvfail_cause: 0,
        flags: VOL_PRESENT,
        rsvd: 0,
    };
    slot.seal().unwrap();
    slot
}

/// The same commit as a later minor would write it: a bumped minor and
/// meaning given to a few reserved bytes.
fn golden_slot_minor1() -> SuperblockSlot {
    let mut slot = golden_slot();
    slot.sb.ver = FORMAT_VERSION + 1;
    slot.sb.reserved[..4].copy_from_slice(&[1, 2, 3, 4]);
    slot.vols.vols[0].rsvd = 0x0102;
    slot.seal().unwrap();
    slot
}

fn golden_intent() -> IntentSlot {
    let mut intent = IntentSlot::new(uuid(0), 11);
    intent.cp_seq = 42;
    intent.wall_ns = 1_760_000_000_500_000_000;
    intent.write_active.set(0).unwrap();
    intent.write_active.set(2).unwrap();
    intent.write_active.set(5).unwrap();
    intent.seal();
    intent
}

fn golden_stripe_id() -> StripeId {
    StripeId {
        agg_id: uuid(0),
        lseg: 17,
        stripe_idx: 3,
        cp_seq: 42,
        cp_nonce: GOLDEN_NONCE,
    }
}

fn golden_descriptors() -> Vec<StripeDescriptor> {
    (0..300u64)
        .map(|i| {
            let vol = (i % 2 * 2) as u32;
            let birth = 40 + (i % 3) as u32;
            if i % 50 == 49 {
                StripeDescriptor::hole(vol, 1000 + i, birth).unwrap()
            } else {
                StripeDescriptor::data(vol, 1000 + i, birth, (i as u32).wrapping_mul(0x0101_0101))
                    .unwrap()
            }
        })
        .collect()
}

fn golden_stripe() -> (Page16K, usize) {
    let id = golden_stripe_id();
    let mut hdr = StripeHdr::new(id.agg_id, StripeKind::Ingest);
    hdr.cp_seq = id.cp_seq;
    hdr.cp_nonce = id.cp_nonce;
    hdr.lseg = id.lseg;
    hdr.stripe_idx = id.stripe_idx;
    hdr.nblocks = 300;
    hdr.group = 1;
    let mut page = Page16K::zeroed();
    let total = write_summary(&mut page.0, &hdr, &golden_descriptors()).unwrap();
    (page, total)
}

fn golden_map_page() -> MapPage {
    let mut page = MapPage::zeroed();
    for i in 0..MAP_ENTRIES_PER_PAGE {
        page.entries[i] = match i % 4 {
            0 => {
                MapEntry::data(17, i as u32, SECTORS_PER_BLOCK, 40, i as u32 ^ 0xFFFF_0000).unwrap()
            }
            1 => MapEntry::hole(41).unwrap(),
            2 => MapEntry::data(18, 100 + i as u32, 3, 42, i as u32).unwrap(),
            _ => MapEntry::UNMAPPED,
        };
    }
    page
}

/// The index entry the golden map page is stored under.
fn golden_map_page_entry() -> PageIndexEntry {
    golden_map_page().index_entry(4096)
}

fn golden_index_page() -> PageIndexPage {
    let mut page = PageIndexPage::zeroed();
    for i in 0..INDEX_ENTRIES_PER_PAGE {
        page.entries[i] = if i % 3 == 2 {
            PageIndexEntry::NONE
        } else {
            PageIndexEntry::new(4096 + i as u64 * 4, 40 + (i % 5) as u32, i as u32 * 7)
        };
    }
    page.entries[0] = golden_map_page_entry();
    page
}

fn golden_map_root() -> MapRoot {
    MapRoot {
        vol: 2,
        flags: 0,
        index_pba: 8192,
        index_pages: 1,
        index_crc: golden_index_page().crc32c(),
        lba_count: 1 << 20,
        checkpoint_cp_seq: 42,
        max_birth: 44,
        rsvd0: 0,
        rsvd1: [0; 16],
    }
}

fn golden_frame() -> (StreamHeader, Vec<u8>) {
    let payload: Vec<u8> = (0..1000u32).map(|i| (i * 31 % 251) as u8).collect();
    let mut hdr = StreamHeader::new(FrameKind::Chunk, Codec::Raw);
    hdr.rel = 2;
    hdr.chunk_idx = 3;
    hdr.cp_seq = 42;
    hdr.base = 41;
    hdr.cp_nonce = GOLDEN_NONCE;
    hdr.owner_epoch = 7;
    hdr.set_payload(&payload).unwrap();
    hdr.seal();
    (hdr, payload)
}

fn golden_holes() -> [HoleRecord; 3] {
    [
        HoleRecord::new(0, 4096, 16).unwrap(),
        HoleRecord::new(2, 1 << 20, 1).unwrap(),
        HoleRecord::new(5, MAX_LBA - 7, 8).unwrap(),
    ]
}

fn golden_seg_entries() -> [SegEntry; 4] {
    let mut sealed = SegEntry::open(5, 2_621_440, 40, 2, SegKind::Data).unwrap();
    sealed.state = SegState::Sealed.as_u8();
    sealed.live_bytes = 640 * BLOCK_SIZE as u32;
    sealed.seal();
    let mut relocated = SegEntry::open(9000, 1 << 20, 7, 1, SegKind::Metadata).unwrap();
    relocated.state = SegState::Relocated.as_u8();
    relocated.seal();
    let mut free = SegEntry::FREE;
    free.lap = 3;
    [
        free,
        SegEntry::open(6, 2_621_440, 42, 1, SegKind::Data).unwrap(),
        sealed,
        relocated,
    ]
}

fn golden_snap_records() -> [SnapRecord; 3] {
    let mut user = SnapRecord {
        snap_id: 1,
        snap_uuid: uuid(11),
        map_root_pba: 700,
        deadlist_pba: 0,
        created_wall_ns: 1_759_000_000_000_000_000,
        birth_cp: 30,
        vol: 2,
        kind: SnapKind::Fsfreeze.as_u8(),
        origin: SnapOrigin::User.as_u8(),
        flags: PIN_USER | PIN_REPLICATION,
        rsvd: 0,
        crc32c: 0,
    };
    user.seal();
    let mut scheduled = SnapRecord {
        snap_id: 2,
        snap_uuid: uuid(12),
        map_root_pba: 701,
        deadlist_pba: 702,
        created_wall_ns: 1_759_500_000_000_000_000,
        birth_cp: 41,
        vol: 0,
        kind: SnapKind::Crash.as_u8(),
        origin: SnapOrigin::Scheduled.as_u8(),
        flags: PIN_SCHEDULE,
        rsvd: 0,
        crc32c: 0,
    };
    scheduled.seal();
    [user, SnapRecord::EMPTY, scheduled]
}

fn golden_cp_log_page() -> CpLogPage {
    let mut page = CpLogPage::new(uuid(0), 42, GOLDEN_NONCE, 17);
    for i in 0..100u32 {
        let rec = if i % 10 == 9 {
            CpLogRec::hole(i % 3, 1000 + u64::from(i), 42).unwrap()
        } else {
            CpLogRec::data(
                i % 3,
                1000 + u64::from(i),
                17,
                i,
                8,
                42,
                i.wrapping_mul(0x0101_0101),
            )
            .unwrap()
        };
        page.push(rec).unwrap();
    }
    page.seal();
    page
}

/// Every golden image, built by the current builders.
fn golden_cp_log_dir() -> CpLogDir {
    CpLogDir {
        cp_seq: 42,
        fold_cp_seq: 42,
        deltas: vec![
            RootRef::new(400, 16, 3),
            RootRef::new(420, 2, 0x0A0B_0C0D),
            RootRef::new(430, 1, 0x1111_2222),
        ],
        pages: vec![
            RootRef::new(4096, 4, 0x3333_4444),
            RootRef::new(4100, 4, 0x5555_6666),
        ],
    }
}

fn golden_snap_root_page() -> SnapRootPage {
    SnapRootPage::new(
        uuid(0),
        1,
        42,
        golden_map_root(),
        RootRef::new(702, 1, 0xDEAD_BEEF),
    )
    .unwrap()
}

/// Every golden image, built by the current builders.
fn golden_images() -> Vec<(&'static str, Vec<u8>)> {
    let (stripe, stripe_len) = golden_stripe();
    let (frame, payload) = golden_frame();
    vec![
        ("seg_entries.bin", golden_seg_entries().as_bytes().to_vec()),
        (
            "snap_records.bin",
            golden_snap_records().as_bytes().to_vec(),
        ),
        ("cp_log_page.bin", golden_cp_log_page().as_bytes().to_vec()),
        ("superblock_slot.bin", golden_slot().as_bytes().to_vec()),
        (
            "superblock_slot_minor1.bin",
            golden_slot_minor1().as_bytes().to_vec(),
        ),
        ("intent_slot.bin", golden_intent().as_bytes().to_vec()),
        ("stripe_summary.bin", stripe.0[..stripe_len].to_vec()),
        ("map_page.bin", golden_map_page().as_bytes().to_vec()),
        (
            "page_index_page.bin",
            golden_index_page().as_bytes().to_vec(),
        ),
        ("map_root.bin", golden_map_root().as_bytes().to_vec()),
        (
            "stream_frame.bin",
            [frame.as_bytes(), payload.as_slice()].concat(),
        ),
        ("hole_records.bin", golden_holes().as_bytes().to_vec()),
        (
            "cp_log_dir.bin",
            golden_cp_log_dir().encode(uuid(0)).unwrap(),
        ),
        (
            "snap_root_page.bin",
            golden_snap_root_page().as_bytes().to_vec(),
        ),
    ]
}

fn manifest_text(images: &[(&str, Vec<u8>)]) -> String {
    let mut text = format!(
        "otap-format golden vectors: format {}.{} stream {}.{}\n",
        FORMAT_MAJOR, FORMAT_MINOR, STREAM_MAJOR, STREAM_MINOR
    );
    for (name, bytes) in images {
        text.push_str(&format!(
            "{}  {}  {}\n",
            hex(&blake3_256(bytes)),
            bytes.len(),
            name
        ));
    }
    text
}

#[test]
#[ignore = "regenerates the current vector set; run on purpose with --ignored, together with a minor bump"]
fn write_golden_vectors() {
    let set = current_set();
    assert_eq!(
        set.format_version, FORMAT_VERSION,
        "add a VectorSet for the new version before regenerating"
    );
    let dir = vectors_dir(set);
    std::fs::create_dir_all(&dir).unwrap();
    let images = golden_images();
    for (name, bytes) in &images {
        std::fs::write(dir.join(name), bytes).unwrap();
    }
    let manifest = manifest_text(&images);
    std::fs::write(dir.join("MANIFEST.txt"), &manifest).unwrap();
    println!(
        "wrote {} vectors to {}\nmanifest_blake3 = \"{}\"",
        images.len(),
        dir.display(),
        hex(&blake3_256(manifest.as_bytes()))
    );
}

/// Reads a set's files, checking the manifest hash, the header line and the
/// per-file hashes and sizes.
fn read_set(set: &VectorSet) -> HashMap<String, Vec<u8>> {
    let dir = vectors_dir(set);
    let manifest = std::fs::read(dir.join("MANIFEST.txt")).unwrap_or_else(|_| {
        panic!(
            "tests/vectors/{}/MANIFEST.txt is missing; run `cargo test -p otap-format -- --ignored write_golden_vectors`",
            set.dir
        )
    });
    assert_eq!(
        hex(&blake3_256(&manifest)),
        set.manifest_blake3,
        "the {} vector manifest changed; a format change needs a minor bump, a new set and a new pinned hash",
        set.dir
    );
    let text = String::from_utf8(manifest).unwrap();
    let mut lines = text.lines();
    assert_eq!(lines.next(), Some(set.header));
    let mut files: HashMap<String, Vec<u8>> = HashMap::new();
    for line in lines {
        let mut parts = line.split_whitespace();
        let (hash, size, name) = (
            parts.next().unwrap(),
            parts.next().unwrap().parse::<usize>().unwrap(),
            parts.next().unwrap(),
        );
        let bytes = std::fs::read(dir.join(name)).unwrap();
        assert_eq!(bytes.len(), size, "{name}");
        assert_eq!(hex(&blake3_256(&bytes)), hash, "{name}");
        files.insert(name.to_string(), bytes);
    }
    for name in set.names {
        assert!(files.contains_key(*name), "{name} is not in the manifest");
    }
    assert_eq!(
        files.len(),
        set.names.len(),
        "{}: unexpected files",
        set.dir
    );
    files
}

#[test]
fn golden_vectors_decode_lane() {
    for set in &VECTOR_SETS {
        let files = read_set(set);
        decode_set(&files, set);
    }
    // At the version that produced the newest set, the builders reproduce
    // it byte for byte; the older sets are what prove N reads N-1.
    let set = current_set();
    assert_eq!(set.format_version, FORMAT_VERSION);
    let files = read_set(set);
    for (name, bytes) in golden_images() {
        assert_eq!(files[name], bytes, "{name} no longer matches its builder");
    }
}

/// The decode lane: the current reader reads every stored image of `set`
/// and sees the values the generator wrote.
fn decode_set(files: &HashMap<String, Vec<u8>>, set: &VectorSet) {
    let slot = SuperblockSlot::read(&files["superblock_slot.bin"]).unwrap();
    assert_eq!(slot.sb.ver, set.format_version);
    assert_eq!(slot.sb.agg_id, uuid(0));
    assert_eq!(slot.sb.commit_key(), (7, 42));
    assert_eq!(slot.sb.cut_seq, 43);
    assert_eq!(slot.sb.cp_nonce, GOLDEN_NONCE);
    assert_eq!(slot.sb.sb_slot(), Some(SbSlot::B0));
    assert_eq!(slot.sb.run_state(), Some(RunState::Running));
    assert_eq!(slot.sb.vol_count, 3);
    assert_eq!(slot.sb.acked_bytes, 8192);
    assert_eq!(slot.sb.write_active.iter().collect::<Vec<_>>(), [0, 2]);
    assert_eq!(slot.sb.nvfail.iter().collect::<Vec<_>>(), [1]);
    assert_eq!(slot.vols.vols[1].nvfail_cause(), Some(NvCause::DirtyBoot));
    assert_eq!(slot.vols.vols[2].acked_bytes, 4096);
    assert_eq!(slot.sb.map_roots, RootRef::new(200, 4, 1));
    if set.format_version >= 0x0201 {
        assert_eq!((slot.sb.open_data_pseg, slot.sb.open_data_idx), (6, 3));
    } else {
        assert_eq!((slot.sb.open_data_pseg, slot.sb.open_data_idx), (0, 0));
    }
    if set.format_version >= 0x0202 {
        assert_eq!(slot.sb.geometry(), Some((32 * 1024, 256 * 1024, 64 << 20)));
    } else {
        assert_eq!(slot.sb.geometry(), None);
    }
    assert_eq!(slot.sb.inbound[0].group, 1);
    assert!(slot.sb.inbound[0].is_valid());
    assert!(slot.sb.durable_log.is_none());

    let minor1 = SuperblockSlot::read(&files["superblock_slot_minor1.bin"]);
    if set.format_version == 0x0201 {
        // The 2.1 writer put its demonstration bytes where 2.2 keeps the
        // geometry: a page that claims 2.2 must carry a valid geometry, so
        // the 2.2 reader refuses this image on that field (not on its CRC,
        // which still verifies). What the N-1 lane proves here is the rule.
        assert!(
            matches!(
                minor1,
                Err(FormatError::BadField {
                    field: "geom_stripe_bytes",
                    ..
                })
            ),
            "{minor1:?}"
        );
    } else {
        let minor1 = minor1.unwrap();
        assert_eq!(minor1.sb.ver, set.format_version + 1);
        if set.format_version >= 0x0202 {
            assert_eq!(minor1.sb.reserved[..4], [1, 2, 3, 4]);
            assert_eq!(minor1.sb.geometry(), slot.sb.geometry());
        } else {
            // The 2.0 writer put its demonstration bytes where 2.1 now keeps
            // the open data segment: a later minor gave meaning to them.
            assert_eq!(minor1.sb.open_data_pseg, 0x0403_0201);
            assert_eq!(minor1.sb.reserved[..4], [0, 0, 0, 0]);
        }
        assert_eq!(minor1.vols.vols[0].rsvd, 0x0102);
        assert_eq!(minor1.sb.commit_key(), slot.sb.commit_key());
    }

    let intent = IntentSlot::read(&files["intent_slot.bin"]).unwrap();
    assert_eq!((intent.intent_seq, intent.slot, intent.cp_seq), (11, 1, 42));
    assert_eq!(intent.write_active.iter().collect::<Vec<_>>(), [0, 2, 5]);
    assert!(intent.supersedes(&slot.sb));

    let mut stripe = Page16K::zeroed();
    let stripe_bytes = &files["stripe_summary.bin"];
    stripe.0[..stripe_bytes.len()].copy_from_slice(stripe_bytes);
    let summary =
        StripeSummary::parse_expected(&stripe.0[..stripe_bytes.len()], &golden_stripe_id())
            .unwrap();
    assert_eq!(summary.descriptors().len(), 300);
    assert_eq!(summary.summary_bytes(), 8192);
    assert_eq!(summary.vol_bitmap().iter().collect::<Vec<_>>(), [0, 2]);
    assert_eq!(summary.hdr().stripe_kind(), Some(StripeKind::Ingest));
    assert_eq!(summary.hdr().group, 1);
    let first = summary.descriptors()[0];
    assert_eq!((first.vol(), first.lba(), first.birth_cp), (0, 1000, 40));
    let hole = summary.descriptors()[49];
    assert_eq!(hole.kind(), Some(BlockKind::Hole));
    assert_eq!(hole.crc32c, ZERO_BLOCK_CRC32C);

    let map_page = MapPage::read(&files["map_page.bin"], &golden_map_page_entry()).unwrap();
    assert!(map_page.entries[0].is_data());
    assert_eq!(map_page.entries[0].lseg(), 17);
    assert!(map_page.entries[1].is_hole());
    assert_eq!(map_page.entries[2].plen(), 3);
    assert!(map_page.entries[3].is_unmapped());
    assert_eq!(map_page.max_birth(), 42);

    let index_page = PageIndexPage::read_from_bytes(&files["page_index_page.bin"]).unwrap();
    assert!(index_page.validate().is_ok());
    assert_eq!(index_page.entries[0], golden_map_page_entry());
    assert!(index_page.entries[2].is_none());
    let root = MapRoot::read_from_bytes(&files["map_root.bin"]).unwrap();
    assert!(root.validate().is_ok());
    assert_eq!(
        (root.vol, root.lba_count, root.checkpoint_cp_seq),
        (2, 1 << 20, 42)
    );
    let mut index = Page16K::zeroed();
    index.0.copy_from_slice(&files["page_index_page.bin"]);
    assert!(root.verify_index(&index.0).is_ok());

    let frame_bytes = &files["stream_frame.bin"];
    let frame = StreamHeader::read(&frame_bytes[..STREAM_HEADER_SIZE]).unwrap();
    assert!(frame
        .verify_payload(&frame_bytes[STREAM_HEADER_SIZE..])
        .is_ok());
    assert_eq!(frame.frame_kind(), Some(FrameKind::Chunk));
    assert_eq!(frame.codec(), Some(Codec::Raw));
    assert_eq!((frame.rel, frame.chunk_idx), (2, 3));
    assert_eq!((frame.cp_seq, frame.base, frame.owner_epoch), (42, 41, 7));
    assert_eq!(frame.cp_nonce, GOLDEN_NONCE);
    assert_eq!(frame.frame_len(), frame_bytes.len());

    let holes: Vec<HoleRecord> = files["hole_records.bin"]
        .as_chunks::<HOLE_RECORD_SIZE>()
        .0
        .iter()
        .map(|raw| HoleRecord::read_from_bytes(raw).unwrap())
        .collect();
    assert_eq!(holes.len(), 3);
    for hole in &holes {
        assert!(hole.validate().is_ok());
    }
    assert_eq!(holes[2].end_lba(), Some(MAX_LBA + 1));

    let segs: Vec<SegEntry> = files["seg_entries.bin"]
        .as_chunks::<SEG_ENTRY_SIZE>()
        .0
        .iter()
        .map(|raw| SegEntry::read_from_bytes(raw).unwrap())
        .collect();
    assert_eq!(segs.len(), 4);
    for seg in &segs {
        assert!(seg.verify().is_ok());
    }
    assert!(segs[0].is_free());
    assert_eq!(segs[0].lap, 3);
    assert_eq!(segs[1].seg_state(), Some(SegState::Open));
    assert_eq!(
        (segs[1].phys, segs[1].stripe_data_bytes, segs[1].birth_cp),
        (6, 2_621_440, 42)
    );
    assert_eq!(segs[2].seg_state(), Some(SegState::Sealed));
    assert_eq!(segs[2].live_bytes, 640 * BLOCK_SIZE as u32);
    assert_eq!(segs[3].seg_state(), Some(SegState::Relocated));
    assert_eq!(segs[3].seg_kind(), Some(SegKind::Metadata));
    assert_eq!(segs[3].phys, 9000);

    let snaps: Vec<SnapRecord> = files["snap_records.bin"]
        .as_chunks::<SNAP_RECORD_SIZE>()
        .0
        .iter()
        .map(|raw| SnapRecord::read_from_bytes(raw).unwrap())
        .collect();
    assert_eq!(snaps.len(), 3);
    for snap in &snaps {
        assert!(snap.verify().is_ok());
    }
    assert_eq!(
        (snaps[0].snap_id, snaps[0].vol, snaps[0].birth_cp),
        (1, 2, 30)
    );
    assert_eq!(snaps[0].snap_kind(), Some(SnapKind::Fsfreeze));
    assert_eq!(snaps[0].snap_origin(), Some(SnapOrigin::User));
    assert_eq!(snaps[0].flags, PIN_USER | PIN_REPLICATION);
    assert!(snaps[1].is_empty());
    assert_eq!(snaps[2].deadlist_pba, 702);
    assert_eq!(snaps[2].snap_kind(), Some(SnapKind::Crash));

    let cp_log = CpLogPage::read(&files["cp_log_page.bin"]).unwrap();
    assert_eq!(cp_log.hdr.ver, set.format_version);
    assert_eq!(cp_log.hdr.agg_id, uuid(0));
    assert_eq!(
        (cp_log.hdr.cp_seq, cp_log.hdr.page_seq, cp_log.hdr.count),
        (42, 17, 100)
    );
    assert_eq!(cp_log.hdr.cp_nonce, GOLDEN_NONCE);
    assert_eq!(cp_log.records().len(), 100);
    assert!(cp_log.records()[9].map_entry().is_hole());
    assert_eq!(cp_log.records()[0].map_entry().lseg(), 17);
    assert_eq!(cp_log.records()[99].lba, 1099);

    // The 2.1 structures exist in the sets written at 2.1 and later.
    if set.format_version >= 0x0201 {
        let dir = CpLogDir::decode(&files["cp_log_dir.bin"], &uuid(0)).unwrap();
        assert_eq!(dir, golden_cp_log_dir());
        assert_eq!(dir.deltas[0].pba, slot.sb.cp_log_head.pba);
        let page = CpLogDirPage::read(&files["cp_log_dir.bin"][..CP_LOG_DIR_PAGE_SIZE]).unwrap();
        assert_eq!(page.hdr.ver, set.format_version);
        assert_eq!((page.hdr.delta_entries, page.hdr.page_entries), (3, 2));

        let snap_root = SnapRootPage::read(&files["snap_root_page.bin"]).unwrap();
        assert_eq!(snap_root.ver, set.format_version);
        assert_eq!((snap_root.snap_id, snap_root.birth_cp), (1, 42));
        assert_eq!(snap_root.root, root);
        assert_eq!(snap_root.deadlist, RootRef::new(702, 1, 0xDEAD_BEEF));
        assert!(snap_root.root.verify_index(&index.0).is_ok());
    }
}
