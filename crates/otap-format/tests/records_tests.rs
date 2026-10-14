//! The Step-2 records: segment table entries, snapshot records, CP log
//! records and pages. Sizes and offsets, round trips over random bits, the
//! validation rules, full CRC coverage and the replay mapping to map entries.

use core::mem::{align_of, offset_of, size_of};

use otap_format::checksum::crc32c;
use otap_format::*;
use proptest::prelude::*;
use proptest::test_runner::TestCaseError;

fn flip_bit(bytes: &mut [u8], bit: usize) {
    bytes[bit / 8] ^= 1 << (bit % 8);
}

fn uuid(seed: u8) -> Uuid {
    let mut b = [0u8; 16];
    for (i, x) in b.iter_mut().enumerate() {
        *x = seed.wrapping_mul(29).wrapping_add(i as u8).wrapping_add(1);
    }
    Uuid::from_bytes(b)
}

fn check_roundtrip<T>(bytes: &[u8], aligned: &mut [u8]) -> Result<(), TestCaseError>
where
    T: FromBytes + IntoBytes + Immutable + KnownLayout + PartialEq,
{
    let copy = T::read_from_bytes(bytes).unwrap();
    prop_assert_eq!(copy.as_bytes(), bytes);
    aligned[..bytes.len()].copy_from_slice(bytes);
    let view = T::ref_from_bytes(&aligned[..bytes.len()]).unwrap();
    prop_assert!(*view == copy);
    Ok(())
}

// ---------------------------------------------------------------------------
// layout
// ---------------------------------------------------------------------------

#[test]
fn record_sizes_alignments_and_offsets() {
    assert_eq!(size_of::<SegEntry>(), 32);
    assert_eq!(size_of::<SnapRecord>(), 64);
    assert_eq!(size_of::<CpLogRec>(), 32);
    assert_eq!(size_of::<CpLogPageHdr>(), 64);
    assert_eq!(size_of::<CpLogPage>(), 4096);
    assert_eq!(align_of::<SegEntry>(), 8);
    assert_eq!(align_of::<SnapRecord>(), 8);
    assert_eq!(align_of::<CpLogRec>(), 8);
    assert_eq!(align_of::<CpLogPageHdr>(), 8);
    assert_eq!(align_of::<CpLogPage>(), 4096);

    assert_eq!(offset_of!(SegEntry, phys), 0);
    assert_eq!(offset_of!(SegEntry, stripe_data_bytes), 8);
    assert_eq!(offset_of!(SegEntry, live_bytes), 12);
    assert_eq!(offset_of!(SegEntry, birth_cp), 16);
    assert_eq!(offset_of!(SegEntry, lap), 20);
    assert_eq!(offset_of!(SegEntry, kind), 24);
    assert_eq!(offset_of!(SegEntry, state), 25);
    assert_eq!(offset_of!(SegEntry, rsvd), 26);
    assert_eq!(offset_of!(SegEntry, crc32c), 28);

    assert_eq!(offset_of!(SnapRecord, snap_id), 0);
    assert_eq!(offset_of!(SnapRecord, snap_uuid), 8);
    assert_eq!(offset_of!(SnapRecord, map_root_pba), 24);
    assert_eq!(offset_of!(SnapRecord, deadlist_pba), 32);
    assert_eq!(offset_of!(SnapRecord, created_wall_ns), 40);
    assert_eq!(offset_of!(SnapRecord, birth_cp), 48);
    assert_eq!(offset_of!(SnapRecord, vol), 52);
    assert_eq!(offset_of!(SnapRecord, kind), 56);
    assert_eq!(offset_of!(SnapRecord, origin), 57);
    assert_eq!(offset_of!(SnapRecord, flags), 58);
    assert_eq!(offset_of!(SnapRecord, rsvd), 59);
    assert_eq!(offset_of!(SnapRecord, crc32c), 60);

    assert_eq!(offset_of!(CpLogRec, lba), 0);
    assert_eq!(offset_of!(CpLogRec, addr), 8);
    assert_eq!(offset_of!(CpLogRec, vol), 16);
    assert_eq!(offset_of!(CpLogRec, birth_cp), 20);
    assert_eq!(offset_of!(CpLogRec, block_crc32c), 24);
    assert_eq!(offset_of!(CpLogRec, crc32c), 28);

    assert_eq!(offset_of!(CpLogPageHdr, magic), 0);
    assert_eq!(offset_of!(CpLogPageHdr, ver), 4);
    assert_eq!(offset_of!(CpLogPageHdr, count), 6);
    assert_eq!(offset_of!(CpLogPageHdr, cp_seq), 8);
    assert_eq!(offset_of!(CpLogPageHdr, page_seq), 16);
    assert_eq!(offset_of!(CpLogPageHdr, agg_id), 24);
    assert_eq!(offset_of!(CpLogPageHdr, cp_nonce), 40);
    assert_eq!(offset_of!(CpLogPageHdr, rsvd), 48);
    assert_eq!(offset_of!(CpLogPageHdr, crc32c), 60);
    assert_eq!(offset_of!(CpLogPage, hdr), 0);
    assert_eq!(offset_of!(CpLogPage, recs), 64);
    assert_eq!(CP_LOG_RECS_PER_PAGE, 126);
    assert_eq!(&CPLOG_MAGIC.to_le_bytes(), b"OTCL");
    assert_eq!(MAX_SEG_BYTES, 256 << 20);
}

// ---------------------------------------------------------------------------
// round trips
// ---------------------------------------------------------------------------

proptest! {
    #[test]
    fn records_roundtrip(bytes in prop::collection::vec(any::<u8>(), 64)) {
        let mut aligned = Block4K::zeroed();
        check_roundtrip::<SegEntry>(&bytes[..32], &mut aligned.0)?;
        check_roundtrip::<SnapRecord>(&bytes, &mut aligned.0)?;
        check_roundtrip::<CpLogRec>(&bytes[..32], &mut aligned.0)?;
        check_roundtrip::<CpLogPageHdr>(&bytes, &mut aligned.0)?;
    }

    #[test]
    fn cp_log_page_roundtrip(bytes in prop::collection::vec(any::<u8>(), 4096)) {
        let mut aligned = Block4K::zeroed();
        check_roundtrip::<CpLogPage>(&bytes, &mut aligned.0)?;
    }
}

// ---------------------------------------------------------------------------
// validation and CRC coverage
// ---------------------------------------------------------------------------

fn stripe_sizes() -> impl Strategy<Value = u32> {
    (2u32..=2048).prop_map(|blocks| blocks * BLOCK_SIZE as u32)
}

proptest! {
    #[test]
    fn seg_entry_rules_and_bit_flips(
        pseg in 1u64..=u64::MAX,
        stripe in stripe_sizes(),
        live_blocks in 0u32..=65536,
        birth in 1u32..=u32::MAX,
        lap in any::<u32>(),
        kind in 0u8..2,
        state in 1u8..5,
        bit in 0usize..(32 * 8),
    ) {
        let mut entry = SegEntry::open(pseg, stripe, birth, lap, SegKind::from_u8(kind).unwrap()).unwrap();
        entry.state = state;
        entry.live_bytes = if state == SegState::Relocated.as_u8() { 0 } else { live_blocks * BLOCK_SIZE as u32 };
        entry.seal();
        prop_assert!(entry.verify().is_ok());
        prop_assert_eq!(entry.seg_state().unwrap().as_u8(), state);
        prop_assert!(!entry.is_free());

        // Every single-bit flip is caught: the CRC covers bytes 0..28 and a
        // flip inside the CRC itself mismatches.
        let mut img = [0u8; 32];
        img.copy_from_slice(entry.as_bytes());
        flip_bit(&mut img, bit);
        prop_assert!(SegEntry::read_from_bytes(&img).unwrap().verify().is_err());

        // Rules.
        let mut bad = entry;
        bad.phys = 0;
        bad.seal();
        prop_assert!(bad.verify().is_err(), "phys 0 is reserved");
        let mut bad = entry;
        bad.stripe_data_bytes = stripe + 1;
        bad.seal();
        prop_assert!(bad.verify().is_err(), "a stripe size must be whole blocks");
        let mut bad = entry;
        bad.stripe_data_bytes = (MAX_STRIPE_BYTES + BLOCK_SIZE) as u32;
        bad.seal();
        prop_assert!(bad.verify().is_err(), "a stripe above 8 MiB");
        let mut bad = entry;
        bad.birth_cp = 0;
        bad.seal();
        prop_assert!(bad.verify().is_err(), "an allocated segment has a birth");
        let mut bad = entry;
        bad.live_bytes = 4095;
        bad.seal();
        prop_assert!(bad.verify().is_err(), "live bytes are whole blocks");
        let mut bad = entry;
        bad.state = SegState::Relocated.as_u8();
        bad.live_bytes = BLOCK_SIZE as u32;
        bad.seal();
        prop_assert!(bad.verify().is_err(), "a relocated segment has no live bytes");
        let mut bad = entry;
        bad.state = 5;
        bad.seal();
        prop_assert!(bad.verify().is_err(), "undefined state");
        let mut bad = entry;
        bad.kind = 2;
        bad.seal();
        prop_assert!(bad.verify().is_err(), "undefined kind");
    }

    #[test]
    fn free_seg_entries_are_zero_apart_from_lap(lap in any::<u32>(), dirty in 1u64..=u64::MAX) {
        let mut free = SegEntry::FREE;
        free.lap = lap;
        prop_assert!(free.verify().is_ok());
        prop_assert!(free.is_free());
        let mut bad = free;
        bad.phys = dirty;
        prop_assert!(bad.verify().is_err());
        let mut bad = free;
        bad.birth_cp = 1;
        prop_assert!(bad.verify().is_err());
    }

    #[test]
    fn snap_record_rules_and_bit_flips(
        snap_id in 1u64..=u64::MAX,
        seed in any::<u8>(),
        map_root_pba in 1u64..=u64::MAX,
        deadlist_pba in any::<u64>(),
        wall in any::<u64>(),
        birth in 1u32..=u32::MAX,
        vol in 0u32..MAX_VOLS as u32,
        kind in 1u8..3,
        origin in 1u8..5,
        flags in 0u8..16,
        bit in 0usize..(64 * 8),
    ) {
        let mut rec = SnapRecord {
            snap_id,
            snap_uuid: uuid(seed),
            map_root_pba,
            deadlist_pba,
            created_wall_ns: wall,
            birth_cp: birth,
            vol,
            kind,
            origin,
            flags,
            rsvd: 0,
            crc32c: 0,
        };
        rec.seal();
        prop_assert!(rec.verify().is_ok());
        prop_assert!(!rec.is_empty());
        prop_assert_eq!(rec.snap_kind().map(SnapKind::as_u8), Some(kind));
        prop_assert_eq!(rec.snap_origin().map(SnapOrigin::as_u8), Some(origin));

        let mut img = [0u8; 64];
        img.copy_from_slice(rec.as_bytes());
        flip_bit(&mut img, bit);
        prop_assert!(SnapRecord::read_from_bytes(&img).unwrap().verify().is_err());

        let mut bad = rec;
        bad.kind = 3;
        bad.seal();
        prop_assert!(bad.verify().is_err());
        let mut bad = rec;
        bad.origin = 0;
        bad.seal();
        prop_assert!(bad.verify().is_err());
        let mut bad = rec;
        bad.flags = 0x10;
        bad.seal();
        prop_assert!(bad.verify().is_err());
        let mut bad = rec;
        bad.vol = MAX_VOLS as u32;
        bad.seal();
        prop_assert!(bad.verify().is_err());
        let mut bad = rec;
        bad.birth_cp = 0;
        bad.seal();
        prop_assert!(bad.verify().is_err());
        let mut bad = rec;
        bad.snap_uuid = Uuid::NIL;
        bad.seal();
        prop_assert!(bad.verify().is_err());
        let mut bad = rec;
        bad.map_root_pba = 0;
        bad.seal();
        prop_assert!(bad.verify().is_err());

        // An empty slot is all zero and nothing else.
        prop_assert!(SnapRecord::EMPTY.verify().is_ok());
        let mut not_empty = SnapRecord::EMPTY;
        not_empty.created_wall_ns = wall | 1;
        prop_assert!(not_empty.verify().is_err());
    }

    #[test]
    fn cp_log_records_replay_to_map_entries(
        vol in 0u32..MAX_VOLS as u32,
        lba in 0u64..=MAX_LBA,
        lseg in 1u64..=MAX_LSEG,
        off in 0u32..=MAX_SEG_OFF,
        plen in 1u16..=SECTORS_PER_BLOCK,
        birth in 1u32..=u32::MAX,
        block_crc in any::<u32>(),
        hole in any::<bool>(),
        bit in 0usize..(32 * 8),
    ) {
        let rec = if hole {
            CpLogRec::hole(vol, lba, birth).unwrap()
        } else {
            CpLogRec::data(vol, lba, lseg, off, plen, birth, block_crc).unwrap()
        };
        prop_assert!(rec.verify().is_ok());
        let entry = rec.map_entry();
        let expected = if hole {
            MapEntry::hole(birth).unwrap()
        } else {
            MapEntry::data(lseg, off, plen, birth, block_crc).unwrap()
        };
        prop_assert_eq!(entry, expected);
        prop_assert_eq!(CpLogRec::from_entry(vol, lba, expected).unwrap(), rec);
        prop_assert_eq!((rec.vol, rec.lba, rec.birth_cp), (vol, lba, birth));

        let mut img = [0u8; 32];
        img.copy_from_slice(rec.as_bytes());
        flip_bit(&mut img, bit);
        prop_assert!(CpLogRec::read_from_bytes(&img).unwrap().verify().is_err());

        prop_assert!(CpLogRec::hole(vol, lba, 0).is_err(), "a journal never records nothing");
        prop_assert!(CpLogRec::data(MAX_VOLS as u32, lba, lseg, off, plen, birth, block_crc).is_err());
        prop_assert!(CpLogRec::data(vol, MAX_LBA + 1, lseg, off, plen, birth, block_crc).is_err());
        prop_assert!(CpLogRec::from_entry(vol, lba, MapEntry::UNMAPPED).is_err());
    }

    #[test]
    fn cp_log_pages_verify_their_used_records(
        count in 0usize..=CP_LOG_RECS_PER_PAGE,
        cp_seq in 1u64..=MAX_CP_SEQ,
        nonce in any::<u64>(),
        page_seq in any::<u64>(),
        bit_seed in any::<usize>(),
    ) {
        let mut page = CpLogPage::new(uuid(3), cp_seq, nonce, page_seq);
        for i in 0..count {
            let rec = CpLogRec::data(i as u32 % 1024, i as u64 * 8, 1 + i as u64, i as u32, 8, cp_seq as u32, i as u32).unwrap();
            prop_assert_eq!(page.push(rec), Some(i));
        }
        prop_assert_eq!(page.is_full(), count == CP_LOG_RECS_PER_PAGE);
        if page.is_full() {
            prop_assert_eq!(page.push(CpLogRec::hole(0, 0, 1).unwrap()), None);
        }
        page.seal();
        prop_assert!(page.verify().is_ok());
        prop_assert_eq!(page.records().len(), count);
        let mut img = Block4K::zeroed();
        img.0.copy_from_slice(page.as_bytes());
        prop_assert!(CpLogPage::parse(&img.0).is_ok());
        prop_assert!(CpLogPage::read(&img.0).is_ok());

        // A flip in the header or in a used record is caught; a flip in an
        // unused record is not part of the page's meaning.
        let used = CP_LOG_PAGE_HDR_SIZE + count * CP_LOG_REC_SIZE;
        if used > 0 {
            let bit = bit_seed % (used * 8);
            flip_bit(&mut img.0, bit);
            prop_assert!(CpLogPage::parse(&img.0).is_err());
        }
        page.hdr.count = CP_LOG_RECS_PER_PAGE as u16 + 1;
        page.seal();
        prop_assert!(
            matches!(
                page.verify(),
                Err(FormatError::OutOfRange { field: "count", .. })
            ),
            "a count above the page capacity is out of range"
        );
        page.hdr.count = 0;
        page.hdr.cp_seq = MAX_CP_SEQ + 1;
        page.seal();
        prop_assert!(page.verify().is_err());
    }
}

#[test]
fn cp_log_page_header_rejects_foreign_pages() {
    let mut page = CpLogPage::new(uuid(1), 5, 9, 0);
    page.seal();
    assert!(page.verify().is_ok());
    let mut other = page;
    other.hdr.magic = SUPERBLOCK_MAGIC;
    other.seal();
    assert!(matches!(other.verify(), Err(FormatError::BadMagic { .. })));
    let mut other = page;
    other.hdr.ver = 0x0300;
    other.seal();
    assert!(matches!(
        other.verify(),
        Err(FormatError::BadVersion { .. })
    ));
    let mut other = page;
    other.hdr.ver = FORMAT_VERSION + 1;
    other.seal();
    assert!(other.verify().is_ok(), "a later minor is readable");
    assert_eq!(page.hdr.compute_crc(), crc32c(&page.as_bytes()[..60]));
}
