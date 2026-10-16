//! Step 6B: the format 2.4 structures of the incremental tables (F19 to
//! F21): tombstones in both record types, the table log page and its
//! region, the table head, the tolerant scans a damaged region gets, the
//! superblock's head CPs, and the byte that makes a 2.3 reader refuse a
//! 2.4 pool.

use core::mem::{align_of, offset_of, size_of};

use otap_format::*;
use proptest::prelude::*;

fn uuid(n: u8) -> Uuid {
    let mut b = [0u8; 16];
    b[0] = n;
    b[15] = 0xA5;
    Uuid::from_bytes(b)
}

fn snap(id: u64, vol: u32, birth: u32) -> SnapRecord {
    let mut rec = SnapRecord {
        snap_id: id,
        snap_uuid: uuid(id as u8 | 1),
        map_root_pba: 1000 + id,
        deadlist_pba: 0,
        created_wall_ns: 1_760_000_000_000_000_000 + id,
        birth_cp: birth,
        vol,
        kind: SnapKind::Crash.as_u8(),
        origin: SnapOrigin::User.as_u8(),
        flags: 0,
        rsvd: 0,
        crc32c: 0,
    };
    rec.seal();
    rec
}

fn agg() -> Uuid {
    uuid(200)
}

#[test]
fn layouts_and_constants() {
    assert_eq!(size_of::<TableLogHdr>(), TABLE_LOG_HDR_SIZE);
    assert_eq!(size_of::<TableLogPage>(), TABLE_LOG_PAGE_SIZE);
    assert_eq!(align_of::<TableLogPage>(), 4096);
    assert_eq!(offset_of!(TableLogPage, payload), 64);
    assert_eq!(size_of::<TableHeadPage>(), TABLE_HEAD_PAGE_SIZE);
    assert_eq!(offset_of!(TableHeadPage, base), 64);
    assert_eq!(offset_of!(TableHeadPage, deltas), 96);
    assert_eq!(offset_of!(TableHeadPage, crc32c), 4092);
    assert_eq!(size_of::<TableChainEntry>(), TABLE_CHAIN_ENTRY_SIZE);
    assert_eq!(TABLE_HEAD_DELTAS, 124);
    assert_eq!(TableKind::Snapshots.records_per_page(), 63);
    assert_eq!(TableKind::Catalog.records_per_page(), 7);
    assert_eq!(offset_of!(Superblock, snap_head_cp), 1760);
    assert_eq!(offset_of!(Superblock, catalog_head_cp), 1768);
    assert_eq!(SNAP_UNCERTAIN, 0x40);
    assert_eq!(SNAP_TOMBSTONE, 0x80);
    assert_eq!(CATALOG_TOMBSTONE, 0x01);
    assert_eq!(FORMAT_VERSION, 0x0204);
}

#[test]
fn snapshot_tombstones_and_the_uncertain_mark() {
    let tomb = SnapRecord::tombstone(7, uuid(7), 3).unwrap();
    assert!(tomb.is_tombstone());
    assert!(tomb.verify_delta().is_ok());
    // A base refuses a tombstone (and a base image too).
    assert!(tomb.verify().is_err());
    assert!(snap_table_image(&[tomb]).is_err());
    assert!(SnapRecord::tombstone(0, uuid(7), 3).is_err());
    assert!(SnapRecord::tombstone(7, Uuid::NIL, 3).is_err());
    assert!(SnapRecord::tombstone(7, uuid(7), MAX_VOL + 1).is_err());
    // A tombstone carries its key and nothing else.
    let mut fat = tomb;
    fat.map_root_pba = 9;
    fat.seal();
    assert!(matches!(
        fat.verify_delta(),
        Err(FormatError::BadField {
            field: "map_root_pba",
            ..
        })
    ));
    let mut pinned = tomb;
    pinned.flags |= PIN_USER;
    pinned.seal();
    assert!(pinned.verify_delta().is_err());
    // The uncertain mark is valid in a base and in a delta, and is not a
    // pin.
    let mut rec = snap(4, 1, 30);
    rec.flags = PIN_USER | SNAP_UNCERTAIN;
    rec.seal();
    assert!(rec.verify().is_ok() && rec.verify_delta().is_ok());
    assert!(rec.is_uncertain());
    assert_eq!(rec.pins(), PIN_USER);
    // An empty record is no delta record.
    assert!(SnapRecord::EMPTY.verify_delta().is_err());
    // An unknown flag stays refused.
    let mut odd = snap(5, 1, 30);
    odd.flags = 0x20;
    odd.seal();
    assert!(odd.verify().is_err() && odd.verify_delta().is_err());
}

#[test]
fn catalogue_tombstones() {
    let vol = CatalogRec::tombstone(CatalogKind::Volume, 3, Uuid::NIL).unwrap();
    let snapshot = CatalogRec::tombstone(CatalogKind::Snapshot, 3, uuid(9)).unwrap();
    for t in [vol, snapshot] {
        assert!(t.is_tombstone());
        assert!(t.verify_delta().is_ok());
        assert!(matches!(
            t.verify(),
            Err(FormatError::BadField { field: "flags", .. })
        ));
    }
    assert_eq!(
        vol.sort_key(),
        CatalogRec::volume(3, "x", 1).unwrap().sort_key()
    );
    assert!(CatalogRec::tombstone(CatalogKind::Volume, 3, uuid(1)).is_err());
    assert!(CatalogRec::tombstone(CatalogKind::Snapshot, 3, Uuid::NIL).is_err());
    let mut named = vol;
    named.name_len = 1;
    named.name[0] = b'a';
    named.seal();
    assert!(named.verify_delta().is_err());
    let mut dated = vol;
    dated.created_wall_ns = 5;
    dated.seal();
    assert!(dated.verify_delta().is_err());
    // A record is a delta record too.
    let rec = CatalogRec::volume(1, "data", 7).unwrap();
    assert!(rec.verify_delta().is_ok());
    assert!(CatalogRec::empty().verify_delta().is_err());
    // A base image refuses a tombstone.
    assert!(catalog_image(&[vol]).is_err());
    assert_eq!(
        snapshot_name_key(3, uuid(9)),
        CatalogRec::snapshot(3, uuid(9), "s", 1).unwrap().sort_key()
    );
}

#[test]
fn a_delta_region_round_trips_and_refuses_what_its_reader_refuses() {
    let records: Vec<SnapRecord> = (1..=70u64)
        .map(|id| {
            if id % 9 == 0 {
                SnapRecord::tombstone(id, uuid(id as u8 | 1), 1).unwrap()
            } else {
                snap(id, (id % 3) as u32, 40)
            }
        })
        .collect();
    let image = table_delta_image(agg(), 42, 0x1234, &records).unwrap();
    assert_eq!(image.len(), 2 * TABLE_LOG_PAGE_SIZE);
    let back: TableDelta<SnapRecord> = parse_table_delta(&image, &agg()).unwrap();
    assert_eq!((back.cp_seq, back.cp_nonce), (42, 0x1234));
    assert_eq!(back.records, records);
    let page = TableLogPage::read(&image[..TABLE_LOG_PAGE_SIZE]).unwrap();
    assert_eq!(page.hdr.count, 63);
    assert_eq!(page.hdr.table_kind(), Some(TableKind::Snapshots));
    // The builder refuses an empty region, keys out of order or repeated,
    // and a record that is not a valid delta record.
    assert!(table_delta_image::<SnapRecord>(agg(), 42, 1, &[]).is_err());
    let mut swapped = records.clone();
    swapped.swap(0, 1);
    assert!(table_delta_image(agg(), 42, 1, &swapped).is_err());
    let doubled = vec![records[0], records[0]];
    assert!(table_delta_image(agg(), 42, 1, &doubled).is_err());
    let mut bad = records[0];
    bad.kind = 9;
    bad.seal();
    assert!(table_delta_image(agg(), 42, 1, &[bad]).is_err());
    // The parser refuses another aggregate, another table, a page out of
    // place, a short page before the last, and a record that fails.
    assert!(matches!(
        parse_table_delta::<SnapRecord>(&image, &uuid(1)),
        Err(FormatError::IdentityMismatch { field: "agg_id" })
    ));
    assert!(matches!(
        parse_table_delta::<CatalogRec>(&image, &agg()),
        Err(FormatError::IdentityMismatch { field: "table" })
    ));
    let mut reordered = image[TABLE_LOG_PAGE_SIZE..].to_vec();
    reordered.extend_from_slice(&image[..TABLE_LOG_PAGE_SIZE]);
    assert!(parse_table_delta::<SnapRecord>(&reordered, &agg()).is_err());
    let mut flipped = image.clone();
    flipped[64 + 5] ^= 1;
    assert!(parse_table_delta::<SnapRecord>(&flipped, &agg()).is_err());
    assert!(parse_table_delta::<SnapRecord>(&image[..100], &agg()).is_err());
    let names = vec![
        CatalogRec::volume(0, "a", 1).unwrap(),
        CatalogRec::tombstone(CatalogKind::Volume, 1, Uuid::NIL).unwrap(),
        CatalogRec::snapshot(0, uuid(3), "s", 2).unwrap(),
    ];
    let image = table_delta_image(agg(), 7, 9, &names).unwrap();
    assert_eq!(image.len(), TABLE_LOG_PAGE_SIZE);
    let back: TableDelta<CatalogRec> = parse_table_delta(&image, &agg()).unwrap();
    assert_eq!(back.records, names);
}

#[test]
fn a_damaged_region_gives_its_readable_records_and_counts_the_rest() {
    let records: Vec<SnapRecord> = (1..=70u64).map(|id| snap(id, 1, 40)).collect();
    let image = table_delta_image(agg(), 42, 1, &records).unwrap();
    // A whole region scans to every record and no damage.
    let whole: TableScan<SnapRecord> = scan_table_delta(&image, &agg(), 42);
    assert_eq!((whole.records.len(), whole.damaged), (70, 0));
    // One record's byte: that record alone is lost.
    let mut one = image.clone();
    one[64 + 64 * 5 + 10] ^= 0x40;
    let scan: TableScan<SnapRecord> = scan_table_delta(&one, &agg(), 42);
    assert_eq!((scan.records.len(), scan.damaged), (69, 1));
    assert!(!scan.records.iter().any(|r| r.snap_id == 6));
    // The first page's header: none of its records is believed, the
    // second page's are.
    let mut header = image.clone();
    header[9] ^= 1;
    let scan: TableScan<SnapRecord> = scan_table_delta(&header, &agg(), 42);
    assert_eq!(scan.records.len(), 7);
    assert_eq!(scan.damaged, 1 + 63);
    assert!(scan.problem.is_some());
    // Another CP's page is not believed either.
    let scan: TableScan<SnapRecord> = scan_table_delta(&image, &agg(), 41);
    assert!(scan.records.is_empty() && scan.damaged > 0);
    // A base image scans the same way, record by record.
    let base = snap_table_image(&records[..10]).unwrap();
    let mut bad = base.clone();
    bad[64 * 3 + 2] ^= 0x10;
    let scan: TableScan<SnapRecord> = scan_table_base(&bad);
    assert_eq!((scan.records.len(), scan.damaged), (9, 1));
    let names: Vec<CatalogRec> = (0..5u32)
        .map(|v| CatalogRec::volume(v, &format!("v{v}"), 1).unwrap())
        .collect();
    let base = catalog_image(&names).unwrap();
    let mut bad = base.clone();
    bad[512 + 40] ^= 0x01;
    let scan: TableScan<CatalogRec> = scan_table_base(&bad);
    assert_eq!((scan.records.len(), scan.damaged), (4, 1));
}

fn head() -> TableHead {
    TableHead {
        table: TableKind::Catalog,
        cp_seq: 50,
        base: TableChainEntry::new(RootRef::new(300, 2, 0xAA), 20, 9),
        deltas: vec![
            TableChainEntry::new(RootRef::new(310, 1, 0xBB), 30, 1),
            TableChainEntry::new(RootRef::new(311, 1, 0xCC), 50, 2),
        ],
    }
}

#[test]
fn the_head_round_trips_and_its_rules_hold() {
    let page = head().page(agg()).unwrap();
    let bytes = IntoBytes::as_bytes(&page).to_vec();
    assert_eq!(
        TableHead::decode(&bytes, &agg(), TableKind::Catalog).unwrap(),
        head()
    );
    assert!(matches!(
        TableHead::decode(&bytes, &uuid(1), TableKind::Catalog),
        Err(FormatError::IdentityMismatch { field: "agg_id" })
    ));
    assert!(matches!(
        TableHead::decode(&bytes, &agg(), TableKind::Snapshots),
        Err(FormatError::IdentityMismatch { field: "table" })
    ));
    assert_eq!(head().regions().count(), 3);
    assert_eq!((head().base_records(), head().delta_records()), (9, 3));
    // The chain rules: births in order, after the base, at or before the
    // head; regions named; at most 124 deltas.
    let mut late = head();
    late.deltas[1].cp_seq = 51;
    assert!(late.page(agg()).is_err());
    let mut disorder = head();
    disorder.deltas.swap(0, 1);
    assert!(disorder.page(agg()).is_err());
    let mut before_base = head();
    before_base.deltas[0].cp_seq = 20;
    assert!(before_base.page(agg()).is_err());
    let mut empty = head();
    empty.deltas[0].records = 0;
    assert!(empty.page(agg()).is_err());
    let mut unnamed = head();
    unnamed.deltas[0].region = RootRef::NONE;
    assert!(unnamed.page(agg()).is_err());
    let mut hollow = head();
    hollow.base.records = 0;
    assert!(hollow.page(agg()).is_err());
    let mut no_base = head();
    no_base.base = TableChainEntry::new(RootRef::NONE, 20, 0);
    assert!(no_base.page(agg()).is_ok());
    let mut long = head();
    long.deltas = (0..=TABLE_HEAD_DELTAS as u64)
        .map(|i| TableChainEntry::new(RootRef::new(400 + i, 1, 1), 21 + i, 1))
        .collect();
    long.cp_seq = 21 + TABLE_HEAD_DELTAS as u64;
    assert!(long.page(agg()).is_err());
    long.deltas.pop();
    assert!(long.page(agg()).is_ok());
    // An unused entry that is not zero is refused by the reader.
    let mut stray = page;
    stray.deltas[5] = TableChainEntry::new(RootRef::new(9, 1, 1), 49, 1);
    stray.seal();
    assert!(TableHeadPage::read(IntoBytes::as_bytes(&stray)).is_err());
}

/// A 2.3 reader takes the snapshot table's head for a table image: it must
/// refuse it, whatever the head holds, so a Step 5 binary refuses a 2.4
/// pool instead of rewriting its superblock (F20).
#[test]
fn a_format_2_3_reader_refuses_every_snapshot_head() {
    for deltas in [0usize, 1, 5] {
        let head = TableHead {
            table: TableKind::Snapshots,
            cp_seq: 900,
            base: if deltas == 0 {
                TableChainEntry::new(RootRef::NONE, 3, 0)
            } else {
                TableChainEntry::new(RootRef::new(77, 1, 0x1111), 3, 1)
            },
            deltas: (0..deltas as u64)
                .map(|i| TableChainEntry::new(RootRef::new(80 + i, 1, 0x2222), 4 + i, 1))
                .collect(),
        };
        let page = head.page(uuid(3)).unwrap();
        let bytes = IntoBytes::as_bytes(&page);
        assert_eq!(bytes[56], 0, "a snapshot record's kind byte");
        assert!(parse_snap_table(bytes).is_err());
    }
}

#[test]
fn the_superblock_names_the_heads_cps() {
    let mut slot = SuperblockSlot::new(agg(), 42, SbLocation::A);
    slot.sb.snap_table = RootRef::new(500, 1, 1);
    slot.sb.snap_head_cp = 42;
    slot.sb.catalog = RootRef::new(501, 1, 2);
    slot.sb.catalog_head_cp = 17;
    slot.seal().unwrap();
    let back = SuperblockSlot::read(IntoBytes::as_bytes(&slot)).unwrap();
    assert_eq!((back.sb.snap_head_cp, back.sb.catalog_head_cp), (42, 17));
    // A head written after the commit is refused.
    let mut ahead = slot;
    ahead.sb.catalog_head_cp = 43;
    ahead.seal().unwrap();
    assert!(matches!(
        SuperblockSlot::read(IntoBytes::as_bytes(&ahead)),
        Err(FormatError::OutOfRange {
            field: "catalog_head_cp",
            ..
        })
    ));
    // A table without a head names no CP.
    let mut orphan = slot;
    orphan.sb.snap_table = RootRef::NONE;
    orphan.seal().unwrap();
    assert!(matches!(
        SuperblockSlot::read(IntoBytes::as_bytes(&orphan)),
        Err(FormatError::BadField {
            field: "snap_head_cp",
            ..
        })
    ));
    // A 2.3 page gives those bytes no meaning.
    let mut old = orphan;
    old.sb.ver = 0x0203;
    old.seal().unwrap();
    assert!(SuperblockSlot::read(IntoBytes::as_bytes(&old)).is_ok());
}

proptest! {
    /// Every single-bit flip of a sealed head fails its verification.
    #[test]
    fn every_head_bit_is_covered(bit in 0usize..(TABLE_HEAD_PAGE_SIZE * 8)) {
        let page = head().page(agg()).unwrap();
        let mut bytes = IntoBytes::as_bytes(&page).to_vec();
        bytes[bit / 8] ^= 1 << (bit % 8);
        prop_assert!(TableHead::decode(&bytes, &agg(), TableKind::Catalog).is_err(), "bit {bit}");
    }

    /// Every single-bit flip of a delta region's header or records fails
    /// its strict parse, and costs at most one page in a tolerant scan.
    #[test]
    fn every_delta_bit_is_covered(bit in 0usize..(2 * TABLE_LOG_PAGE_SIZE * 8)) {
        let records: Vec<SnapRecord> = (1..=64u64).map(|id| snap(id, 2, 41)).collect();
        let image = table_delta_image(agg(), 41, 5, &records).unwrap();
        let mut bytes = image.clone();
        bytes[bit / 8] ^= 1 << (bit % 8);
        prop_assert!(parse_table_delta::<SnapRecord>(&bytes, &agg()).is_err(), "bit {bit}");
        let scan: TableScan<SnapRecord> = scan_table_delta(&bytes, &agg(), 41);
        prop_assert!(scan.records.len() >= records.len() - 63, "bit {bit}");
        prop_assert!(scan.damaged >= 1, "bit {bit}");
        for rec in &scan.records {
            prop_assert!(records.contains(rec), "bit {bit}: a record that was never written");
        }
    }
}
