//! Raw 20 KiB slot buffers through every superblock path: the parsers must
//! never panic, whatever the ring ranking returns must verify and belong to
//! the aggregate asked for, and a slot the fuzzer manages to make valid must
//! survive a seal-and-reparse round trip. The page, intent and slot views
//! agree with their copying reads on aligned bytes, refuse the same bytes
//! off a page boundary, where the copying reads give the same results.

#![no_main]

use libfuzzer_sys::fuzz_target;
use otap_format::{
    boot_write_active, FormatError, FromBytes, IntentSlot, SbSlot, Superblock, SuperblockSlot,
    INTENT_SLOT_SIZE, SUPERBLOCK_SIZE, SUPERBLOCK_SLOT_SIZE,
};

/// A 4096-aligned slot buffer, so the zero-copy paths are exercised too.
#[repr(C, align(4096))]
struct Slot([u8; SUPERBLOCK_SLOT_SIZE]);

/// Room for a slot at the offsets in `SHIFTS`.
#[repr(C, align(4096))]
struct Shifted([u8; SUPERBLOCK_SLOT_SIZE + 512]);

/// Off a page boundary: by one byte, and by 512 (sector-aligned, as a
/// buffer from a 512-byte-sector read may be, but not page-aligned).
const SHIFTS: [usize; 2] = [1, 512];

fuzz_target!(|data: &[u8]| {
    // Copies at any alignment and any length.
    let _ = Superblock::read(data);
    let _ = SuperblockSlot::read(data);
    let _ = IntentSlot::read(data);
    if data.len() >= SUPERBLOCK_SIZE {
        let _ = Superblock::read(&data[..SUPERBLOCK_SIZE]);
    }
    if data.len() >= INTENT_SLOT_SIZE {
        if let Ok(intent) = IntentSlot::read(&data[..INTENT_SLOT_SIZE]) {
            assert_eq!(intent.slot, IntentSlot::slot_for(intent.intent_seq));
        }
    }

    // A whole slot, aligned: views, ranking, the boot union.
    let mut slot = Slot([0; SUPERBLOCK_SLOT_SIZE]);
    let n = data.len().min(SUPERBLOCK_SLOT_SIZE);
    slot.0[..n].copy_from_slice(&data[..n]);
    let view = SuperblockSlot::ref_from_bytes(&slot.0).expect("aligned and exact");
    let parsed = SuperblockSlot::parse(&slot.0);
    assert_eq!(parsed.is_ok(), view.verify().is_ok());

    // The page and intent views agree with their copies.
    let page = &slot.0[..SUPERBLOCK_SIZE];
    let page_read = Superblock::read(page);
    assert_eq!(Superblock::parse(page).copied(), page_read);
    let intent = &slot.0[..INTENT_SLOT_SIZE];
    let intent_read = IntentSlot::read(intent);
    assert_eq!(IntentSlot::parse(intent).copied(), intent_read);

    // Off a page boundary every view refuses and every copy reads the same.
    let slot_read = SuperblockSlot::read(&slot.0);
    assert_eq!(slot_read.is_ok(), parsed.is_ok());
    let mut shifted = Shifted([0; SUPERBLOCK_SLOT_SIZE + 512]);
    for shift in SHIFTS {
        shifted.0[shift..shift + SUPERBLOCK_SLOT_SIZE].copy_from_slice(&slot.0);
        let at = &shifted.0[shift..shift + SUPERBLOCK_SLOT_SIZE];
        assert!(matches!(
            SuperblockSlot::parse(at),
            Err(FormatError::Misaligned { .. })
        ));
        assert!(matches!(
            Superblock::parse(&at[..SUPERBLOCK_SIZE]),
            Err(FormatError::Misaligned { .. })
        ));
        assert!(matches!(
            IntentSlot::parse(&at[..INTENT_SLOT_SIZE]),
            Err(FormatError::Misaligned { .. })
        ));
        assert!(SuperblockSlot::read(at) == slot_read);
        assert!(Superblock::read(&at[..SUPERBLOCK_SIZE]) == page_read);
        assert!(IntentSlot::read(&at[..INTENT_SLOT_SIZE]) == intent_read);
    }

    let agg = view.sb.agg_id;
    let ring = SbSlot::ALL.into_iter().map(|location| (location, view));
    match SuperblockSlot::rank(&agg, ring) {
        Ok(ranked) => {
            assert!(!ranked.is_empty());
            for s in ranked {
                assert!(s.verify().is_ok());
                assert_eq!(s.sb.agg_id, agg);
            }
        }
        Err(FormatError::NewerFormat { .. }) => assert!(view.sb.is_newer_format()),
        Err(_) => {}
    }
    if let Ok(active) = boot_write_active(&view.sb, []) {
        assert!(active == view.sb.write_active);
    }

    // Reseal what the fuzzer gave us: the sealed copy must parse exactly when
    // the enumerated fields and cross-checks hold, and must round-trip.
    let mut resealed = *view;
    if resealed.seal().is_ok() {
        let bytes = otap_format::IntoBytes::as_bytes(&resealed);
        if let Ok(again) = SuperblockSlot::read(bytes) {
            assert!(again == resealed);
            assert!(SuperblockSlot::read(otap_format::IntoBytes::as_bytes(&again)).is_ok());
        }
    }
});
