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

// ---------------------------------------------------------------------------
// format 2.1: the CP log directory (F12), the snapshot root page (F13) and
// the table regions (F14)
// ---------------------------------------------------------------------------

#[test]
fn format_2_1_sizes_alignments_and_offsets() {
    assert_eq!(size_of::<CpLogDirHdr>(), 64);
    assert_eq!(size_of::<CpLogDirPage>(), 4096);
    assert_eq!(size_of::<SnapRootPage>(), 4096);
    assert_eq!(align_of::<CpLogDirHdr>(), 8);
    assert_eq!(align_of::<CpLogDirPage>(), 4096);
    assert_eq!(align_of::<SnapRootPage>(), 4096);
    assert_eq!(CP_LOG_DIR_ENTRIES, 252);
    assert_eq!(offset_of!(CpLogDirHdr, magic), 0);
    assert_eq!(offset_of!(CpLogDirHdr, ver), 4);
    assert_eq!(offset_of!(CpLogDirHdr, count), 6);
    assert_eq!(offset_of!(CpLogDirHdr, cp_seq), 8);
    assert_eq!(offset_of!(CpLogDirHdr, fold_cp_seq), 16);
    assert_eq!(offset_of!(CpLogDirHdr, agg_id), 24);
    assert_eq!(offset_of!(CpLogDirHdr, delta_entries), 40);
    assert_eq!(offset_of!(CpLogDirHdr, page_entries), 44);
    assert_eq!(offset_of!(CpLogDirHdr, page_seq), 48);
    assert_eq!(offset_of!(CpLogDirHdr, rsvd), 52);
    assert_eq!(offset_of!(CpLogDirHdr, crc32c), 60);
    assert_eq!(offset_of!(CpLogDirPage, entries), 64);
    assert_eq!(offset_of!(SnapRootPage, snap_id), 8);
    assert_eq!(offset_of!(SnapRootPage, agg_id), 16);
    assert_eq!(offset_of!(SnapRootPage, birth_cp), 32);
    assert_eq!(offset_of!(SnapRootPage, root), 40);
    assert_eq!(offset_of!(SnapRootPage, deadlist), 104);
    assert_eq!(offset_of!(SnapRootPage, reserved), 120);
    assert_eq!(offset_of!(SnapRootPage, crc32c), 4092);
    assert_eq!(CPDIR_MAGIC, u32::from_le_bytes(*b"OTCD"));
    assert_eq!(SNAPROOT_MAGIC, u32::from_le_bytes(*b"OTSR"));
    assert_eq!(FORMAT_VERSION, 0x0202);
}

fn root_ref(i: u64) -> RootRef {
    RootRef::new(1000 + i * 8, 1 + (i % 7) as u32, 0x1234_5678 ^ i as u32)
}

#[test]
fn cp_log_dir_pages_round_trip_and_refuse_bad_entries() {
    let mut page = CpLogDirPage::new(uuid(4), 42, 40, 0);
    page.hdr.delta_entries = 3;
    for i in 0..3 {
        assert_eq!(page.push(root_ref(i)).unwrap(), Some(i as usize));
    }
    page.seal();
    assert!(page.verify().is_ok());
    assert_eq!(page.used().len(), 3);
    let mut img = Block4K::zeroed();
    img.0.copy_from_slice(page.as_bytes());
    assert_eq!(*CpLogDirPage::parse(&img.0).unwrap(), page);
    assert_eq!(CpLogDirPage::read(&img.0).unwrap(), page);

    // An absent or zero-block reference is refused by the builder and by
    // the parser alike.
    assert!(page.push(RootRef::NONE).is_err());
    assert!(page.push(RootRef::new(0, 1, 1)).is_err());
    let mut bad = page;
    bad.entries[1] = RootRef::new(0, 1, 1);
    bad.seal();
    assert!(matches!(
        bad.verify(),
        Err(FormatError::BadField { field: "pba", .. })
    ));
    // An entry beyond the count must be absent.
    let mut bad = page;
    bad.entries[3] = root_ref(9);
    bad.seal();
    assert!(matches!(
        bad.verify(),
        Err(FormatError::BadField { field: "count", .. })
    ));
    // The fold never lies ahead of the CP, and fold pages belong to the fold
    // CP only.
    let mut bad = page;
    bad.hdr.fold_cp_seq = 43;
    bad.seal();
    assert!(matches!(
        bad.verify(),
        Err(FormatError::OutOfRange {
            field: "fold_cp_seq",
            ..
        })
    ));
    let mut bad = page;
    bad.hdr.page_entries = 1;
    bad.seal();
    assert!(matches!(
        bad.verify(),
        Err(FormatError::BadField {
            field: "page_entries",
            ..
        })
    ));
    let mut bad = page;
    bad.hdr.count = CP_LOG_DIR_ENTRIES as u16 + 1;
    bad.seal();
    assert!(matches!(
        bad.verify(),
        Err(FormatError::OutOfRange { field: "count", .. })
    ));
    let mut bad = page;
    bad.hdr.magic = CPLOG_MAGIC;
    bad.seal();
    assert!(matches!(bad.verify(), Err(FormatError::BadMagic { .. })));
    let mut bad = page;
    bad.hdr.ver = 0x0300;
    bad.seal();
    assert!(matches!(bad.verify(), Err(FormatError::BadVersion { .. })));
    let mut later = page;
    later.hdr.ver = FORMAT_VERSION + 1;
    later.seal();
    assert!(later.verify().is_ok(), "a later minor is readable");

    // A full page refuses a further entry without failing.
    let mut full = CpLogDirPage::new(uuid(4), 1, 1, 0);
    for i in 0..CP_LOG_DIR_ENTRIES as u64 {
        assert!(full.push(root_ref(i)).unwrap().is_some());
    }
    assert!(full.is_full());
    assert_eq!(full.push(root_ref(1)).unwrap(), None);
}

#[test]
fn a_cp_log_directory_encodes_across_pages_and_decodes_to_the_same() {
    for (deltas, pages) in [
        (0usize, 0usize),
        (1, 0),
        (5, 3),
        (252, 0),
        (253, 0),
        (300, 260),
    ] {
        let dir = CpLogDir {
            cp_seq: 77,
            fold_cp_seq: if pages > 0 { 77 } else { 70 },
            deltas: (0..deltas as u64).map(root_ref).collect(),
            pages: (0..pages as u64).map(|i| root_ref(1000 + i)).collect(),
        };
        let bytes = dir.encode(uuid(5)).unwrap();
        assert_eq!(
            bytes.len(),
            dir.pages_needed() * CP_LOG_DIR_PAGE_SIZE,
            "{deltas}/{pages}"
        );
        let back = CpLogDir::decode(&bytes, &uuid(5)).unwrap();
        assert_eq!(back, dir, "{deltas}/{pages}");
        assert!(matches!(
            CpLogDir::decode(&bytes, &uuid(6)),
            Err(FormatError::IdentityMismatch { field: "agg_id" })
        ));
        if bytes.len() > CP_LOG_DIR_PAGE_SIZE {
            // Pages out of order, or disagreeing on the totals, are refused.
            let mut swapped = bytes.clone();
            let (a, b) = swapped.split_at_mut(CP_LOG_DIR_PAGE_SIZE);
            a.swap_with_slice(&mut b[..CP_LOG_DIR_PAGE_SIZE]);
            assert!(CpLogDir::decode(&swapped, &uuid(5)).is_err());
            let mut short = bytes.clone();
            short.truncate(bytes.len() - CP_LOG_DIR_PAGE_SIZE);
            assert!(matches!(
                CpLogDir::decode(&short, &uuid(5)),
                Err(FormatError::BadField { field: "count", .. })
            ));
        }
    }
    // Pages outside the fold CP, and a fold ahead of the CP, never encode.
    let bad = CpLogDir {
        cp_seq: 10,
        fold_cp_seq: 9,
        deltas: vec![],
        pages: vec![root_ref(1)],
    };
    assert!(bad.encode(uuid(5)).is_err());
    let bad = CpLogDir {
        cp_seq: 10,
        fold_cp_seq: 11,
        deltas: vec![],
        pages: vec![],
    };
    assert!(bad.encode(uuid(5)).is_err());
    assert!(CpLogDir::decode(&[], &uuid(5)).is_err());
    assert!(CpLogDir::decode(&[0u8; 100], &uuid(5)).is_err());
}

fn golden_root() -> MapRoot {
    MapRoot {
        vol: 3,
        flags: 0,
        index_pba: 4096,
        index_pages: 1,
        index_crc: 0xCAFE_F00D,
        lba_count: 1 << 18,
        checkpoint_cp_seq: 30,
        max_birth: 30,
        rsvd0: 0,
        rsvd1: [0; 16],
    }
}

#[test]
fn snap_root_pages_round_trip_and_validate() {
    let page = SnapRootPage::new(uuid(7), 5, 30, golden_root(), RootRef::NONE).unwrap();
    assert!(page.verify().is_ok());
    let mut img = Block4K::zeroed();
    img.0.copy_from_slice(page.as_bytes());
    assert_eq!(*SnapRootPage::parse(&img.0).unwrap(), page);
    assert_eq!(SnapRootPage::read(&img.0).unwrap(), page);
    let with_deadlist =
        SnapRootPage::new(uuid(7), 5, 30, golden_root(), RootRef::new(8192, 2, 1)).unwrap();
    assert!(with_deadlist.verify().is_ok());

    assert!(SnapRootPage::new(uuid(7), 0, 30, golden_root(), RootRef::NONE).is_err());
    assert!(SnapRootPage::new(uuid(7), 5, 0, golden_root(), RootRef::NONE).is_err());
    assert!(SnapRootPage::new(uuid(7), 5, MAX_CP_SEQ + 1, golden_root(), RootRef::NONE).is_err());
    assert!(
        SnapRootPage::new(uuid(7), 5, 29, golden_root(), RootRef::NONE).is_err(),
        "a root checkpointed after the birth"
    );
    assert!(SnapRootPage::new(uuid(7), 5, 30, MapRoot::ABSENT, RootRef::NONE).is_err());
    assert!(SnapRootPage::new(uuid(7), 5, 30, golden_root(), RootRef::new(0, 2, 1)).is_err());
    let mut bad_root = golden_root();
    bad_root.index_pages = 2;
    assert!(SnapRootPage::new(uuid(7), 5, 30, bad_root, RootRef::NONE).is_err());

    // Every bit of the page is under the CRC.
    for bit in [
        0usize,
        40,
        8 * 8,
        16 * 8 + 3,
        32 * 8,
        40 * 8 + 5,
        104 * 8,
        120 * 8,
        4091 * 8 + 7,
    ] {
        let mut flipped = img;
        flip_bit(&mut flipped.0, bit);
        assert!(SnapRootPage::parse(&flipped.0).is_err(), "bit {bit}");
    }
    let mut flipped = img;
    flip_bit(&mut flipped.0, 4092 * 8);
    assert!(matches!(
        SnapRootPage::parse(&flipped.0),
        Err(FormatError::BadCrc { .. })
    ));
}

fn snap(id: u64, vol: u32, birth: u32) -> SnapRecord {
    let mut rec = SnapRecord {
        snap_id: id,
        snap_uuid: uuid(id as u8),
        map_root_pba: 100 + id,
        deadlist_pba: 0,
        created_wall_ns: 1_760_000_000_000_000_000 + id,
        birth_cp: birth,
        vol,
        kind: SnapKind::Crash.as_u8(),
        origin: SnapOrigin::User.as_u8(),
        flags: PIN_USER,
        rsvd: 0,
        crc32c: 0,
    };
    rec.seal();
    rec
}

#[test]
fn the_snapshot_table_region_round_trips_and_refuses_disorder() {
    assert_eq!(snap_table_image(&[]).unwrap().len(), BLOCK_SIZE);
    assert!(parse_snap_table(&snap_table_image(&[]).unwrap())
        .unwrap()
        .is_empty());
    let records: Vec<SnapRecord> = (1..=70)
        .map(|i| snap(i, (i % 4) as u32, 10 + i as u32))
        .collect();
    let image = snap_table_image(&records).unwrap();
    assert_eq!(image.len(), 2 * BLOCK_SIZE, "70 records need two pages");
    assert_eq!(parse_snap_table(&image).unwrap(), records);
    // Ids must ascend; the padding must be zero; a record must verify.
    let mut disorder = records.clone();
    disorder.swap(3, 4);
    assert!(snap_table_image(&disorder).is_err());
    let mut dirty = image.clone();
    dirty[70 * SNAP_RECORD_SIZE + 5] = 1;
    assert!(parse_snap_table(&dirty).is_err());
    let mut flipped = image.clone();
    flipped[64 + 9] ^= 1;
    assert!(parse_snap_table(&flipped).is_err());
    assert!(parse_snap_table(&image[..100]).is_err());
    let mut pages = image.clone();
    let (a, b) = pages.split_at_mut(BLOCK_SIZE);
    a.swap_with_slice(&mut b[..BLOCK_SIZE]);
    assert!(
        parse_snap_table(&pages).is_err(),
        "ids out of order across pages"
    );
}

#[test]
fn the_map_roots_region_round_trips_and_checks_positions() {
    let mut roots = vec![MapRoot::ABSENT; 5];
    roots[1] = MapRoot {
        vol: 1,
        index_pba: 8192,
        index_pages: 1,
        lba_count: 64,
        checkpoint_cp_seq: 3,
        ..MapRoot::ABSENT
    };
    roots[3] = {
        let mut r = golden_root();
        r.vol = 3;
        r
    };
    let image = map_roots_image(&roots).unwrap();
    assert_eq!(image.len(), BLOCK_SIZE);
    assert_eq!(parse_map_roots(&image, 5).unwrap(), roots);
    assert_eq!(map_roots_image(&[]).unwrap().len(), BLOCK_SIZE);
    assert!(parse_map_roots(&map_roots_image(&[]).unwrap(), 0)
        .unwrap()
        .is_empty());
    // A root at the wrong position, an absent slot that is not zero, a
    // count the region cannot hold, and dirty padding are refused.
    let mut wrong = roots.clone();
    wrong[2] = roots[1];
    assert!(map_roots_image(&wrong).is_err());
    let mut wrong = roots.clone();
    wrong[0].max_birth = 1;
    assert!(map_roots_image(&wrong).is_err());
    assert!(parse_map_roots(&image, 65).is_err());
    assert!(parse_map_roots(&image, MAX_VOLS as u32 + 1).is_err());
    let mut dirty = image.clone();
    dirty[5 * MAP_ROOT_SIZE + 1] = 1;
    assert!(parse_map_roots(&dirty, 5).is_err());
    let mut moved = image.clone();
    moved.copy_within(3 * MAP_ROOT_SIZE..4 * MAP_ROOT_SIZE, 2 * MAP_ROOT_SIZE);
    assert!(
        parse_map_roots(&moved, 5).is_err(),
        "a root at another volume's slot"
    );
    // Fewer slots than the region holds is fine as long as the rest is zero.
    assert_eq!(parse_map_roots(&image, 4).unwrap().len(), 4);
    assert!(
        parse_map_roots(&image, 3).is_err(),
        "slot 3 is present and must be counted"
    );
}
