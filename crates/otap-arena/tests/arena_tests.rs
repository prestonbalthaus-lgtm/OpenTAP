//! The arena: geometry, the slot lifecycle, the ring, re-attach.

use otap_arena::{slots_for_len, Arena, ArenaError, RingRef, SlotDesc, SlotState, SLOT_BYTES};
use otap_format::checksum::crc32c;
use otap_format::{FromBytes, IntoBytes, Uuid, MAX_LBA, MAX_VOL};
use otap_sys::{HugeArena, HugePageSize};
use proptest::prelude::*;

const TWO_MIB: usize = 2 * 1024 * 1024;

fn memory() -> HugeArena {
    HugeArena::anonymous(TWO_MIB, HugePageSize::TwoMiB).expect("2 MiB of heap")
}

fn agg() -> Uuid {
    Uuid::from_bytes([7; 16])
}

fn arena() -> Arena {
    Arena::create(memory(), agg(), 1_000).expect("a fresh arena")
}

/// Writes `byte` over a reserved run and returns the CRC of each block.
fn paint(a: &mut Arena, ring: RingRef, blocks: u32, byte: u8) -> Vec<u32> {
    let run = a.run_mut(ring, blocks).expect("a reserved run is writable");
    run.fill(byte);
    run.chunks(SLOT_BYTES).map(crc32c).collect()
}

/// Reserves, paints and fills a run.
fn filled(a: &mut Arena, blocks: u32, vol: u32, lba: u64, seq: u64, byte: u8) -> RingRef {
    let ring = a.reserve(blocks).expect("room in the ring");
    let crcs = paint(a, ring, blocks, byte);
    a.fill(ring, vol, lba, seq, &crcs).expect("fill");
    ring
}

#[test]
fn the_geometry_fills_the_memory() {
    assert_eq!(slots_for_len(TWO_MIB).unwrap(), 507);
    let a = arena();
    let hdr = a.header();
    assert_eq!(hdr.slots, 507);
    assert_eq!(hdr.desc_off, 4096);
    assert_eq!(hdr.data_off, 4096 + 16384);
    assert_eq!(a.capacity_bytes(), 507 * SLOT_BYTES as u64);
    assert_eq!(hdr.attach_count(), Some(1));
    assert!(matches!(
        slots_for_len(8192),
        Err(ArenaError::TooSmall { .. })
    ));
}

#[test]
fn a_run_moves_through_its_states() {
    let mut a = arena();
    let ring = a.reserve(3).unwrap();
    assert_eq!(ring, RingRef { slot: 0, lap: 0 });
    assert_eq!(a.occupied(), 3);
    let crcs = paint(&mut a, ring, 3, 0xAB);
    assert_eq!(a.descriptor(0).unwrap().state(), SlotState::Free);
    assert!(a.run(ring, 3).is_err(), "not readable before fill");
    a.fill(ring, 1, 10, 1, &crcs).unwrap();
    let d = a.descriptor(1).unwrap();
    assert_eq!(d.state(), SlotState::Filled);
    assert_eq!(
        (d.vol(), d.lba(), d.seq, d.crc32c, d.lap()),
        (1, 11, 1, crcs[1], 0)
    );
    assert!(d.verify(), "a sealed descriptor verifies");
    assert_eq!(d.addr, SlotDesc::pack_addr(1, 11));
    assert!(a.run(ring, 3).unwrap().iter().all(|b| *b == 0xAB));
    a.check_data(ring, 3).unwrap();
    a.place(ring, 3).unwrap();
    assert_eq!(a.descriptor(2).unwrap().state(), SlotState::Placed);
    assert_eq!(a.occupied(), 3);
    a.commit(ring, 3).unwrap();
    assert_eq!(a.descriptor(0).unwrap().state(), SlotState::Committed);
    assert_eq!(a.occupied(), 0, "the tail passed the committed run");
    assert!(a.run(ring, 3).is_err(), "not readable after commit");
}

#[test]
fn runs_never_wrap_and_the_arena_refuses_when_full() {
    let mut a = arena();
    let big = filled(&mut a, 500, 0, 0, 1, 0x01);
    a.place(big, 500).unwrap();
    // 7 slots remain at the end of the lap: a run of 10 must skip to the
    // next lap, where it would collide with the placed run.
    let err = a.reserve(10).unwrap_err();
    assert_eq!(
        err,
        ArenaError::Full {
            wanted: 10,
            free: 7
        }
    );
    let small = a.reserve(7).unwrap();
    assert_eq!(small, RingRef { slot: 500, lap: 0 });
    assert_eq!(a.occupied(), 507);
    assert!(matches!(a.reserve(1), Err(ArenaError::Full { .. })));
    a.commit(big, 500).unwrap();
    assert_eq!(
        a.occupied(),
        7,
        "the tail stops at the free but reserved run"
    );
    let crcs = paint(&mut a, small, 7, 0x02);
    a.fill(small, 0, 100, 2, &crcs).unwrap();
    a.place(small, 7).unwrap();
    let next = a.reserve(10).unwrap();
    assert_eq!(next, RingRef { slot: 0, lap: 1 }, "the next lap");
    assert_eq!(a.window(), (500, 507 + 10));
    assert!(matches!(
        a.reserve(0),
        Err(ArenaError::BadRun { wanted: 0 })
    ));
    assert!(matches!(
        a.reserve(508),
        Err(ArenaError::BadRun { wanted: 508 })
    ));
}

#[test]
fn a_run_that_fits_an_empty_ring_is_handed_out_on_the_next_lap() {
    // Found by the Step 3 review: the never-used rest of the lap counted
    // against the window, so a run longer than both the lap remainder and
    // the head's offset was refused on an empty ring for ever.
    let mut a = arena();
    let first = filled(&mut a, 250, 0, 0, 1, 0x03);
    a.place(first, 250).unwrap();
    a.commit(first, 250).unwrap();
    assert_eq!(a.occupied(), 0);
    assert_eq!(a.reserve(300), Ok(RingRef { slot: 0, lap: 1 }));
    assert_eq!(a.window(), (507, 507 + 300));
    // A run that does not fit even an empty ring's lap remainder plus the
    // free part of the next lap is still refused.
    let mut b = arena();
    let live = filled(&mut b, 10, 0, 0, 1, 0x04);
    b.place(live, 10).unwrap();
    let rest = b.reserve(490).unwrap();
    b.discard(rest, 490).unwrap();
    assert_eq!(b.window(), (0, 500));
    assert!(matches!(b.reserve(20), Err(ArenaError::Full { .. })));
    assert_eq!(b.reserve(7), Ok(RingRef { slot: 500, lap: 0 }));
}

#[test]
fn a_stale_reference_is_refused() {
    let mut a = arena();
    let first = filled(&mut a, 2, 0, 0, 1, 0x05);
    a.place(first, 2).unwrap();
    a.commit(first, 2).unwrap();
    let rest = filled(&mut a, 505, 0, 2, 2, 0x06);
    a.place(rest, 505).unwrap();
    a.commit(rest, 505).unwrap();
    let again = a.reserve(2).unwrap();
    assert_eq!(again, RingRef { slot: 0, lap: 1 });
    assert_eq!(
        a.run(first, 2).unwrap_err(),
        ArenaError::Stale {
            ring: first,
            lap: 1
        }
    );
    assert!(matches!(a.commit(first, 2), Err(ArenaError::Stale { .. })));
}

#[test]
fn attach_keeps_placed_runs_and_drops_filled_ones() {
    let mut a = arena();
    let placed = filled(&mut a, 2, 3, 40, 1, 0x11);
    a.place(placed, 2).unwrap();
    let _filled = filled(&mut a, 3, 3, 50, 2, 0x12);
    let committed = filled(&mut a, 1, 4, 60, 3, 0x13);
    a.place(committed, 1).unwrap();
    a.commit(committed, 1).unwrap();
    let later = filled(&mut a, 4, 5, 70, 4, 0x22);
    a.place(later, 4).unwrap();

    let memory = a.into_memory();
    Arena::can_attach(&memory, &agg()).unwrap();
    let (a, report) = Arena::attach(memory, &agg()).unwrap();
    assert_eq!(report.attach_count, 2);
    assert_eq!(report.dropped_filled, 3);
    assert_eq!(report.committed, 1);
    assert!(report.damaged.is_empty());
    let seqs: Vec<(u64, u32, u64, u32)> = report
        .placed
        .iter()
        .map(|p| (p.seq, p.vol, p.lba, p.ring.slot))
        .collect();
    assert_eq!(
        seqs,
        vec![
            (1, 3, 40, 0),
            (1, 3, 41, 1),
            (4, 5, 70, 6),
            (4, 5, 71, 7),
            (4, 5, 72, 8),
            (4, 5, 73, 9)
        ]
    );
    assert_eq!(a.peek_seq(), 5);
    assert_eq!(a.window(), (0, 10));
    assert_eq!(
        a.descriptor(2).unwrap().state(),
        SlotState::Committed,
        "dropped"
    );
    assert!(
        a.run(placed, 2).unwrap().iter().all(|b| *b == 0x11),
        "data survived"
    );
    assert!(a.run(later, 4).unwrap().iter().all(|b| *b == 0x22));
    assert_eq!(a.header().attach_count(), Some(2));
}

#[test]
fn attach_refuses_other_memory_and_the_header_is_never_rewritten() {
    let zeroed = memory();
    assert!(Arena::is_blank(&zeroed));
    assert!(matches!(
        Arena::can_attach(&zeroed, &agg()),
        Err(ArenaError::BadMagic { found: 0 })
    ));
    assert!(matches!(
        Arena::attach(zeroed, &agg()),
        Err((ArenaError::BadMagic { found: 0 }, _))
    ));
    let a = arena();
    let other = Uuid::from_bytes([9; 16]);
    let memory = a.into_memory();
    assert!(!Arena::is_blank(&memory));
    assert!(matches!(
        Arena::can_attach(&memory, &other),
        Err(ArenaError::WrongAggregate { .. })
    ));
    // A torn attach counter (the only word an attach writes) is not damage:
    // the count restarts, the arena attaches.
    let mut memory = memory;
    let sealed_before = memory.as_slice()[..64].to_vec();
    memory.as_mut_slice()[64..72].copy_from_slice(&0xDEAD_BEEFu64.to_le_bytes());
    let (a, report) = Arena::attach(memory, &agg()).unwrap();
    assert_eq!(report.attach_count, 1, "an unknown count restarts");
    assert_eq!(a.header().attach_count(), Some(1));
    let memory = a.into_memory();
    assert_eq!(&memory.as_slice()[..64], sealed_before.as_slice());
    // A damaged identity is refused.
    let mut memory = memory;
    memory.as_mut_slice()[50] ^= 1;
    assert!(matches!(
        Arena::can_attach(&memory, &agg()),
        Err(ArenaError::BadCrc)
    ));
}

#[test]
fn fill_checks_addresses_and_transitions() {
    let mut a = arena();
    let ring = a.reserve(2).unwrap();
    let crcs = paint(&mut a, ring, 2, 0x31);
    assert!(matches!(
        a.fill(ring, MAX_VOL + 1, 0, 1, &crcs),
        Err(ArenaError::BadAddress { .. })
    ));
    assert!(matches!(
        a.fill(ring, 0, MAX_LBA, 1, &crcs),
        Err(ArenaError::BadAddress { .. })
    ));
    assert!(matches!(
        a.place(ring, 2),
        Err(ArenaError::BadTransition { .. })
    ));
    a.fill(ring, 0, MAX_LBA - 1, 1, &crcs).unwrap();
    assert_eq!(a.descriptor(1).unwrap().lba(), MAX_LBA);
    assert!(matches!(
        a.fill(ring, 0, 0, 1, &crcs),
        Err(ArenaError::BadTransition { .. })
    ));
    assert!(matches!(
        a.commit(ring, 2),
        Err(ArenaError::BadTransition { .. })
    ));
    a.discard(ring, 2).unwrap();
    assert_eq!(a.occupied(), 0);
    // A reserved run can be discarded before it is filled.
    let orphan = a.reserve(3).unwrap();
    assert_eq!(a.occupied(), 3);
    a.discard(orphan, 3).unwrap();
    assert_eq!(a.occupied(), 0);
    assert!(matches!(
        a.descriptor(600),
        Err(ArenaError::OutOfRange { slot: 600 })
    ));
}

#[test]
fn damaged_data_is_caught_by_check_data_and_by_attach() {
    let mut a = arena();
    let good = filled(&mut a, 2, 1, 0, 1, 0x41);
    a.place(good, 2).unwrap();
    let bad = filled(&mut a, 2, 1, 8, 2, 0x42);
    a.place(bad, 2).unwrap();
    a.check_data(bad, 2).unwrap();
    let mut memory = a.into_memory();
    let data_off = 4096 + 16384;
    memory.as_mut_slice()[data_off + 3 * SLOT_BYTES + 77] ^= 0x80;
    let (a, report) = Arena::attach(memory, &agg()).unwrap();
    assert_eq!(report.placed.len(), 3, "the damaged block is not replayed");
    assert_eq!(report.damaged.len(), 1);
    assert_eq!(report.damaged[0].slot, 3);
    assert_eq!(report.damaged[0].damage, otap_arena::Damage::Data);
    assert!(report.is_damaged());
    assert_eq!(a.descriptor(3).unwrap().state(), SlotState::Committed);
    assert_eq!(a.run(good, 2).unwrap().len(), 2 * SLOT_BYTES);
    assert_eq!(
        a.check_data(bad, 1),
        Ok(()),
        "the first block of the damaged run is intact"
    );
}

#[derive(Debug, Clone, Copy)]
enum Step {
    Reserve(u32),
    PlaceOldest,
    CommitOldest,
}

fn step() -> impl Strategy<Value = Step> {
    prop_oneof![
        4 => (1u32..=40).prop_map(Step::Reserve),
        3 => Just(Step::PlaceOldest),
        3 => Just(Step::CommitOldest),
    ]
}

proptest! {
    #![proptest_config(ProptestConfig { failure_persistence: None, ..ProptestConfig::default() })]

    #[test]
    fn the_ring_never_hands_out_a_live_slot(steps in prop::collection::vec(step(), 1..300)) {
        let mut a = arena();
        // (ring, blocks, state): Filled -> Placed -> gone.
        let mut live: Vec<(RingRef, u32, bool)> = Vec::new();
        let mut seq = 0u64;
        let last = steps.len() - 1;
        for (i, s) in steps.into_iter().enumerate() {
            match s {
                Step::Reserve(n) => match a.reserve(n) {
                    Ok(ring) => {
                        for (other, blocks, _) in &live {
                            let overlap = other.lap == ring.lap
                                && other.slot < ring.slot + n
                                && ring.slot < other.slot + blocks;
                            prop_assert!(!overlap, "a live run was handed out again");
                        }
                        seq += 1;
                        let crcs = paint(&mut a, ring, n, (seq & 0xff) as u8);
                        a.fill(ring, 1, 0, seq, &crcs).unwrap();
                        live.push((ring, n, false));
                    }
                    Err(ArenaError::Full { .. }) => {}
                    Err(e) => prop_assert!(false, "{e}"),
                },
                Step::PlaceOldest => {
                    if let Some((ring, blocks, placed)) = live.iter_mut().find(|(_, _, p)| !*p) {
                        a.place(*ring, *blocks).unwrap();
                        *placed = true;
                    }
                }
                Step::CommitOldest => {
                    if let Some(i) = live.iter().position(|(_, _, p)| *p) {
                        let (ring, blocks, _) = live.remove(i);
                        a.commit(ring, blocks).unwrap();
                    }
                }
            }
            let slots = u64::from(a.slots());
            let (tail, head) = a.window();
            prop_assert!(head - tail <= slots, "window {tail}..{head}");
            for (ring, blocks, _) in &live {
                let pos = u64::from(ring.lap) * slots + u64::from(ring.slot);
                prop_assert!(pos >= tail && pos + u64::from(*blocks) <= head, "live run outside the window");
                // The data check is a full pass over the live runs: every
                // 32 steps and at the end keeps the debug build quick.
                if i % 32 == 0 || i == last {
                    a.check_data(*ring, *blocks).unwrap();
                }
            }
            if live.is_empty() {
                prop_assert_eq!(a.occupied(), 0, "nothing live, nothing occupied");
            }
        }
    }
}

// ---------------------------------------------------------------------------
// hole record slots (Step 4, layout 1.2, E23)
// ---------------------------------------------------------------------------

#[test]
fn a_hole_record_fills_one_slot_and_survives_a_reattach() {
    let mut a = arena();
    assert_eq!(a.header().ver, otap_arena::ARENA_VERSION);
    let data = a.reserve(2).unwrap();
    let crcs = paint(&mut a, data, 2, 0x11);
    a.fill(data, 3, 100, 1, &crcs).unwrap();
    a.place(data, 2).unwrap();
    let hole = a.reserve(1).unwrap();
    a.fill_hole(hole, 3, 1000, 4096, 2).unwrap();
    let desc = a.descriptor(hole.slot).unwrap();
    assert!(desc.is_hole());
    assert_eq!(desc.slot_kind(), Some(otap_arena::SlotKind::Hole));
    assert_eq!((desc.vol(), desc.lba(), desc.seq), (3, 1000, 2));
    assert!(!a.descriptor(data.slot).unwrap().is_hole());
    let rec = a.hole_record(hole).unwrap();
    assert_eq!((rec.vol, rec.lba, rec.blocks), (3, 1000, 4096));
    assert!(a.hole_record(data).is_err(), "a data slot is not a record");
    assert!(
        a.check_data(hole, 1).is_ok(),
        "the record is under the slot CRC"
    );
    a.place(hole, 1).unwrap();

    // Bounds: no zero-length hole, no run past the LBA space, one slot only.
    let spare = a.reserve(1).unwrap();
    assert!(a.fill_hole(spare, 3, 1000, 0, 3).is_err());
    assert!(a.fill_hole(spare, 3, MAX_LBA, 2, 3).is_err());
    assert!(a.fill_hole(spare, MAX_VOL + 1, 0, 1, 3).is_err());
    a.discard(spare, 1).unwrap();

    // A re-attach lists the hole as a placed slot with its block count,
    // in sequence order after the data.
    let mem = a.into_memory();
    let (a, report) = Arena::attach(mem, &agg()).unwrap();
    assert!(!report.is_damaged());
    assert_eq!(report.placed.len(), 3);
    assert!(!report.placed[0].is_hole() && !report.placed[1].is_hole());
    let h = report.placed[2];
    assert!(h.is_hole());
    assert_eq!((h.vol, h.lba, h.seq, h.hole_blocks), (3, 1000, 2, 4096));
    assert_eq!(h.ring, hole);
    assert_eq!(a.hole_record(hole).unwrap().blocks, 4096);
    assert_eq!(a.peek_seq(), 3);
}

#[test]
fn a_damaged_hole_record_or_an_unknown_kind_is_reported_not_trusted() {
    // A hole record whose block was changed: the CRC catches it as data
    // damage; one whose descriptor kind is undefined is kind damage; and a
    // record whose block decodes to another address than its descriptor is
    // kind damage even though the CRC matches.
    for case in 0..3 {
        let mut a = arena();
        let hole = a.reserve(1).unwrap();
        a.fill_hole(hole, 1, 50, 8, 1).unwrap();
        a.place(hole, 1).unwrap();
        let desc_off =
            a.header().desc_off as usize + hole.slot as usize * otap_arena::SLOT_DESC_SIZE;
        let data_off = a.header().data_off as usize + hole.slot as usize * SLOT_BYTES;
        let mut mem = a.into_memory();
        match case {
            0 => mem.as_mut_slice()[data_off + 16] ^= 1,
            1 => {
                // kind = 7, resealed so the descriptor itself verifies.
                let bytes =
                    &mut mem.as_mut_slice()[desc_off..desc_off + otap_arena::SLOT_DESC_SIZE];
                let mut desc = SlotDesc::read_from_bytes(bytes).unwrap();
                desc.kind = 7;
                desc.word = desc.word_for(desc.state(), desc.lap());
                bytes.copy_from_slice(desc.as_bytes());
            }
            _ => {
                // The block says LBA 51 and its CRC is recomputed to match.
                let block = &mut mem.as_mut_slice()[data_off..data_off + SLOT_BYTES];
                block[8..16].copy_from_slice(&51u64.to_le_bytes());
                let crc = crc32c(block);
                let bytes =
                    &mut mem.as_mut_slice()[desc_off..desc_off + otap_arena::SLOT_DESC_SIZE];
                let mut desc = SlotDesc::read_from_bytes(bytes).unwrap();
                desc.crc32c = crc;
                desc.word = desc.word_for(desc.state(), desc.lap());
                bytes.copy_from_slice(desc.as_bytes());
            }
        }
        let (_, report) = Arena::attach(mem, &agg()).unwrap();
        assert!(report.placed.is_empty(), "case {case}");
        assert_eq!(report.damaged.len(), 1, "case {case}");
        let expected = if case == 0 {
            otap_arena::Damage::Data
        } else {
            otap_arena::Damage::Kind
        };
        assert_eq!(report.damaged[0].damage, expected, "case {case}");
    }
}
