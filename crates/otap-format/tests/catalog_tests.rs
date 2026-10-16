//! Format 2.3: the name catalogue (F16), the consistency point's stripe
//! list (F17) and the offline flag (F18). Layout, round trips, every
//! validation rule, CRC coverage, canonical order and uniqueness.

use core::mem::{align_of, offset_of, size_of};

use otap_format::*;
use proptest::prelude::*;

fn uuid(seed: u8) -> Uuid {
    let mut b = [0u8; 16];
    for (i, x) in b.iter_mut().enumerate() {
        *x = seed.wrapping_mul(29).wrapping_add(i as u8).wrapping_add(1);
    }
    Uuid::from_bytes(b)
}

fn flip_bit(bytes: &mut [u8], bit: usize) {
    bytes[bit / 8] ^= 1 << (bit % 8);
}

// ---------------------------------------------------------------------------
// layout
// ---------------------------------------------------------------------------

#[test]
fn catalog_and_stripe_list_layouts() {
    assert_eq!(size_of::<CatalogRec>(), CATALOG_RECORD_SIZE);
    assert_eq!(CATALOG_RECORD_SIZE, 512);
    assert_eq!(align_of::<CatalogRec>(), 8);
    assert_eq!(offset_of!(CatalogRec, kind), 0);
    assert_eq!(offset_of!(CatalogRec, name_len), 1);
    assert_eq!(offset_of!(CatalogRec, flags), 2);
    assert_eq!(offset_of!(CatalogRec, rsvd0), 3);
    assert_eq!(offset_of!(CatalogRec, vol), 4);
    assert_eq!(offset_of!(CatalogRec, snap_uuid), 8);
    assert_eq!(offset_of!(CatalogRec, created_wall_ns), 24);
    assert_eq!(offset_of!(CatalogRec, name), 32);
    assert_eq!(offset_of!(CatalogRec, rsvd1), 287);
    assert_eq!(offset_of!(CatalogRec, crc32c), 508);
    assert_eq!(CATALOG_NAME_MAX, 255);
    assert_eq!(size_of::<CpStripe>(), CP_STRIPE_SIZE);
    assert_eq!(CP_STRIPE_SIZE, 16);
    assert_eq!(align_of::<CpStripe>(), 8);
    assert_eq!(offset_of!(CpStripe, pseg), 0);
    assert_eq!(offset_of!(CpStripe, idx), 8);
    assert_eq!(offset_of!(CpStripe, kind), 12);
    assert_eq!(offset_of!(CpStripe, rsvd), 13);
    assert_eq!(VOL_OFFLINE, 2);
    // Format 2.4 took 16 reserved bytes for the table heads' CPs.
    assert_eq!(SUPERBLOCK_RESERVED, 2316);
    assert_eq!(FORMAT_VERSION, 0x0204);
}

// ---------------------------------------------------------------------------
// names
// ---------------------------------------------------------------------------

#[test]
fn names_follow_the_rules() {
    assert!(validate_name(b"data").is_ok());
    assert!(validate_name("vm-images.2026 (prod)".as_bytes()).is_ok());
    assert!(validate_name("volume\u{e9}".as_bytes()).is_ok());
    assert!(validate_name(&[b'a'; 255]).is_ok());
    assert!(matches!(
        validate_name(b""),
        Err(FormatError::BadField {
            field: "name_len",
            value: 0
        })
    ));
    assert!(matches!(
        validate_name(&[b'a'; 256]),
        Err(FormatError::BadField {
            field: "name_len",
            value: 256
        })
    ));
    assert!(matches!(
        validate_name(b"a/b"),
        Err(FormatError::BadField {
            field: "name",
            value: 0x2f
        })
    ));
    assert!(matches!(
        validate_name(b"tab\there"),
        Err(FormatError::BadField {
            field: "name",
            value: 9
        })
    ));
    assert!(matches!(
        validate_name(b"del\x7f"),
        Err(FormatError::BadField {
            field: "name",
            value: 0x7f
        })
    ));
    assert!(matches!(
        validate_name(&[0xff, 0xfe]),
        Err(FormatError::BadField { field: "name", .. })
    ));
}

#[test]
fn catalog_records_round_trip_and_verify() {
    let vol = CatalogRec::volume(5, "data", 7).unwrap();
    assert_eq!(vol.catalog_kind(), Some(CatalogKind::Volume));
    assert_eq!(vol.name(), "data");
    assert_eq!(vol.name_len, 4);
    assert!(vol.snap_uuid.is_nil());
    assert!(vol.verify().is_ok());
    let copy = CatalogRec::read_from_bytes(vol.as_bytes()).unwrap();
    assert_eq!(copy, vol);
    let snap = CatalogRec::snapshot(5, uuid(3), "nightly", 8).unwrap();
    assert_eq!(snap.catalog_kind(), Some(CatalogKind::Snapshot));
    assert_eq!(snap.sort_key(), (2, 5, *uuid(3).as_array()));
    assert!(snap.verify().is_ok());
    assert!(CatalogRec::empty().verify().is_ok());
    assert!(CatalogRec::empty().is_empty());

    // The builders refuse what the parser refuses.
    assert!(CatalogRec::volume(MAX_VOL + 1, "x", 0).is_err());
    assert!(CatalogRec::volume(0, "", 0).is_err());
    assert!(CatalogRec::volume(0, "a/b", 0).is_err());
    assert!(CatalogRec::snapshot(0, Uuid::NIL, "s", 0).is_err());

    // Every rule the verifier applies to a record read from disk.
    let mut bad = vol;
    bad.kind = 3;
    bad.seal();
    assert!(matches!(
        bad.verify(),
        Err(FormatError::BadField { field: "kind", .. })
    ));
    let mut bad = vol;
    bad.snap_uuid = uuid(1);
    bad.seal();
    assert!(matches!(
        bad.verify(),
        Err(FormatError::BadField {
            field: "snap_uuid",
            ..
        })
    ));
    let mut bad = snap;
    bad.snap_uuid = Uuid::NIL;
    bad.seal();
    assert!(matches!(
        bad.verify(),
        Err(FormatError::BadField {
            field: "snap_uuid",
            ..
        })
    ));
    let mut bad = vol;
    bad.name[10] = b'x'; // bytes past name_len must be zero
    bad.seal();
    assert!(matches!(
        bad.verify(),
        Err(FormatError::BadField {
            field: "name_len",
            ..
        })
    ));
    let mut bad = vol;
    bad.name_len = 0;
    bad.seal();
    assert!(matches!(
        bad.verify(),
        Err(FormatError::BadField {
            field: "name_len",
            ..
        })
    ));
    let mut bad = vol;
    bad.name[0] = b'/';
    bad.seal();
    assert!(matches!(
        bad.verify(),
        Err(FormatError::BadField { field: "name", .. })
    ));
    let mut bad = vol;
    bad.vol = MAX_VOL + 1;
    bad.seal();
    assert!(matches!(
        bad.verify(),
        Err(FormatError::OutOfRange { field: "vol", .. })
    ));
    // An empty slot is all zero.
    let mut bad = CatalogRec::empty();
    bad.created_wall_ns = 1;
    assert!(bad.verify().is_err());
    // Reserved bits are ignored by readers, as every reserved field is.
    let mut later = vol;
    later.flags = 0x80;
    later.rsvd1[3] = 9;
    later.seal();
    assert!(later.verify().is_ok());
}

proptest! {
    /// Every single-bit flip of a sealed record fails verification.
    #[test]
    fn every_catalog_bit_is_covered(seed in any::<u8>(), bit in 0usize..(CATALOG_RECORD_SIZE * 8)) {
        let rec = CatalogRec::snapshot(u32::from(seed) % MAX_VOLS as u32, uuid(seed | 1), "name", u64::from(seed)).unwrap();
        let mut bytes = rec.as_bytes().to_vec();
        flip_bit(&mut bytes, bit);
        let flipped = CatalogRec::read_from_bytes(&bytes).unwrap();
        prop_assert!(flipped.verify().is_err(), "bit {bit}");
    }

    /// A name of any length within the bound round-trips through a record.
    #[test]
    fn any_valid_name_round_trips(name in "[a-zA-Z0-9 ._-]{1,255}", vol in 0..MAX_VOLS as u32) {
        let rec = CatalogRec::volume(vol, &name, 1).unwrap();
        prop_assert_eq!(rec.name(), name.as_str());
        let back = CatalogRec::read_from_bytes(rec.as_bytes()).unwrap();
        prop_assert!(back.verify().is_ok());
        prop_assert_eq!(back.name(), name.as_str());
    }
}

// ---------------------------------------------------------------------------
// the catalogue region
// ---------------------------------------------------------------------------

fn sample_catalog() -> Vec<CatalogRec> {
    vec![
        CatalogRec::volume(0, "data", 1).unwrap(),
        CatalogRec::volume(2, "vm", 2).unwrap(),
        CatalogRec::snapshot(0, uuid(12), "s1", 3).unwrap(),
        CatalogRec::snapshot(0, uuid(13), "s2", 4).unwrap(),
        CatalogRec::snapshot(2, uuid(11), "s1", 5).unwrap(),
    ]
}

#[test]
fn catalog_region_round_trips_in_canonical_order() {
    let records = sample_catalog();
    let image = catalog_image(&records).unwrap();
    assert_eq!(image.len(), BLOCK_SIZE);
    assert_eq!(parse_catalog(&image).unwrap(), records);
    // Nine records span pages; an empty catalogue is one zero page.
    let many: Vec<CatalogRec> = (0..9u32)
        .map(|v| CatalogRec::volume(v, &format!("v{v}"), 0).unwrap())
        .collect();
    let image = catalog_image(&many).unwrap();
    assert_eq!(image.len(), 2 * BLOCK_SIZE);
    assert_eq!(parse_catalog(&image).unwrap(), many);
    let empty = catalog_image(&[]).unwrap();
    assert_eq!(empty.len(), BLOCK_SIZE);
    assert!(parse_catalog(&empty).unwrap().is_empty());
}

#[test]
fn catalog_region_refuses_disorder_duplicates_and_damage() {
    let records = sample_catalog();
    // Out of canonical order.
    let mut swapped = records.clone();
    swapped.swap(0, 1);
    assert!(matches!(
        catalog_image(&swapped),
        Err(FormatError::BadField {
            field: "sort_key",
            ..
        })
    ));
    // Two volumes with one name.
    let mut dup = records.clone();
    dup[1] = CatalogRec::volume(2, "data", 2).unwrap();
    assert!(matches!(
        catalog_image(&dup),
        Err(FormatError::BadField { field: "name", .. })
    ));
    // Two snapshots of one volume with one name.
    let mut dup = records.clone();
    dup[3] = CatalogRec::snapshot(0, uuid(13), "s1", 4).unwrap();
    assert!(matches!(
        catalog_image(&dup),
        Err(FormatError::BadField { field: "name", .. })
    ));
    // The same name on snapshots of different volumes is fine (records 2
    // and 4 of the sample), and so is a volume and a snapshot sharing one.
    let mut shared = records.clone();
    shared[2] = CatalogRec::snapshot(0, uuid(12), "data", 3).unwrap();
    assert!(catalog_image(&shared).is_ok());
    // Two records with one key.
    let mut dup = records.clone();
    dup[3] = dup[2];
    assert!(catalog_image(&dup).is_err());
    // An empty record inside the list, and a damaged one.
    let mut holed = records.clone();
    holed[1] = CatalogRec::empty();
    assert!(catalog_image(&holed).is_err());
    let image = catalog_image(&records).unwrap();
    let mut damaged = image.clone();
    damaged[CATALOG_RECORD_SIZE + 26] ^= 1; // created_wall_ns of record 1
    assert!(matches!(
        parse_catalog(&damaged),
        Err(FormatError::BadCrc { .. })
    ));
    let mut damaged = image.clone();
    damaged[CATALOG_RECORD_SIZE + 40] ^= 1; // past the name's length
    assert!(matches!(
        parse_catalog(&damaged),
        Err(FormatError::BadField {
            field: "name_len",
            ..
        })
    ));
    // Bytes after the first empty slot must be zero; the image must be
    // whole pages.
    let mut trailing = image.clone();
    trailing[records.len() * CATALOG_RECORD_SIZE + 600] = 1;
    assert!(parse_catalog(&trailing).is_err());
    assert!(parse_catalog(&image[..image.len() - 1]).is_err());
    // A parsed region is checked for order and uniqueness too.
    let mut disordered = Vec::new();
    for rec in [records[1], records[0]] {
        disordered.extend_from_slice(rec.as_bytes());
    }
    disordered.resize(BLOCK_SIZE, 0);
    assert!(matches!(
        parse_catalog(&disordered),
        Err(FormatError::BadField {
            field: "sort_key",
            ..
        })
    ));
}

// ---------------------------------------------------------------------------
// the stripe list
// ---------------------------------------------------------------------------

#[test]
fn stripe_list_round_trips_and_refuses_bad_entries() {
    let entries = vec![
        CpStripe::new(3, 0, StripeKind::Ingest).unwrap(),
        CpStripe::new(3, 1, StripeKind::Ingest).unwrap(),
        CpStripe::new(9, 7, StripeKind::Relocate).unwrap(),
    ];
    let image = cp_stripes_image(&entries).unwrap();
    assert_eq!(image.len(), BLOCK_SIZE);
    assert_eq!(parse_cp_stripes(&image).unwrap(), entries);
    assert_eq!(entries[2].stripe_kind(), Some(StripeKind::Relocate));
    // 300 entries span two pages.
    let many: Vec<CpStripe> = (0..300u32)
        .map(|i| CpStripe::new(1 + u64::from(i / 64), i % 64, StripeKind::Ingest).unwrap())
        .collect();
    let image = cp_stripes_image(&many).unwrap();
    assert_eq!(image.len(), 2 * BLOCK_SIZE);
    assert_eq!(parse_cp_stripes(&image).unwrap(), many);
    // Empty lists are no region; segment 0 and undefined kinds are refused.
    assert!(cp_stripes_image(&[]).is_err());
    assert!(CpStripe::new(0, 0, StripeKind::Ingest).is_err());
    let mut bad = entries.clone();
    bad[1].kind = 7;
    assert!(matches!(
        cp_stripes_image(&bad),
        Err(FormatError::BadField { field: "kind", .. })
    ));
    // Order is strict.
    let mut bad = entries.clone();
    bad.swap(0, 1);
    assert!(matches!(
        cp_stripes_image(&bad),
        Err(FormatError::BadField { field: "idx", .. })
    ));
    let mut bad = entries.clone();
    bad[1] = bad[0];
    assert!(cp_stripes_image(&bad).is_err());
    // The parser applies the same rules and refuses trailing bytes,
    // reserved bytes and short images.
    let image = cp_stripes_image(&entries).unwrap();
    let mut damaged = image.clone();
    damaged[CP_STRIPE_SIZE + 12] = 9; // kind of entry 1
    assert!(parse_cp_stripes(&damaged).is_err());
    let mut damaged = image.clone();
    damaged[CP_STRIPE_SIZE + 13] = 1; // rsvd of entry 1
    assert!(parse_cp_stripes(&damaged).is_err());
    let mut trailing = image.clone();
    trailing[entries.len() * CP_STRIPE_SIZE + 40] = 1;
    assert!(parse_cp_stripes(&trailing).is_err());
    assert!(parse_cp_stripes(&image[..100]).is_err());
    assert!(parse_cp_stripes(&vec![0u8; BLOCK_SIZE]).is_err());
}

// ---------------------------------------------------------------------------
// the offline flag and the superblock roots
// ---------------------------------------------------------------------------

#[test]
fn offline_volumes_and_the_new_roots_survive_a_commit() {
    let mut slot = SuperblockSlot::new(uuid(0), 42, SbLocation::A);
    slot.sb.owner_epoch = 1;
    slot.sb.vol_count = 2;
    slot.sb.set_geometry(32 * 1024, 256 * 1024, 64 << 20);
    slot.sb.catalog = RootRef::new(900, 2, 0x1234);
    slot.sb.cp_stripes = RootRef::new(910, 1, 0x5678);
    slot.vols.vols[0].flags = VOL_PRESENT;
    slot.vols.vols[1].flags = VOL_PRESENT | VOL_OFFLINE;
    slot.seal().unwrap();
    let back = SuperblockSlot::read(slot.as_bytes()).unwrap();
    assert_eq!(back.sb.catalog, RootRef::new(900, 2, 0x1234));
    assert_eq!(back.sb.cp_stripes, RootRef::new(910, 1, 0x5678));
    assert!(!back.vols.vols[0].is_offline());
    assert!(back.vols.vols[1].is_offline());
    assert!(back.vols.vols[1].is_present());
    // An absent row with the offline bit is not offline (it is nothing).
    let row = VolState {
        flags: VOL_OFFLINE,
        ..VolState::ABSENT
    };
    assert!(!row.is_offline());
}
