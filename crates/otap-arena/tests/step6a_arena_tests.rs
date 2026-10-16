//! Step 6A: the arena defects the Step 6 audit found, pinned.
//!
//! - A `kill -9` inside a multi-block `fill`, between one slot's field
//!   stores and its word store, left a `Free` slot under the reservation's
//!   stale seal that the re-attach neither released nor reported; behind a
//!   kept `Placed` slot it pinned the tail, and within one lap every write
//!   of the pool was deferred for ever (the arena map's pitfall 21). The
//!   re-attach now checks the slot against its neighbours of the same
//!   write: an interrupted fill is released, a word damaged inside a
//!   placed write is reported as damage.
//! - A kill inside `place` left a placed prefix of a write that was never
//!   acknowledged, and the re-attach replayed it as a torn write; the whole
//!   write is dropped now, so what a boot replays is whole writes only.
//! - The re-attach and the creation of a fresh arena wrote the memory at
//!   once, before the boot had recorded the hold the damage called for
//!   (the arena map's pitfall 22): both are deferred now
//!   ([`Arena::attach_deferred`], [`Arena::create_deferred`]) until the
//!   engine applies them.
//! - An arena made for another size (an arena file a larger size grew)
//!   was taken for a lost arena and recreated, and another pool's arena
//!   with it: the aggregate is checked before the geometry now, and
//!   [`Arena::survey`] reads the old arena under its recorded geometry so a
//!   boot can tell whether a recreation would destroy anything (pitfall
//!   13).

use otap_arena::{
    Arena, ArenaError, Damage, RingRef, SlotDesc, SlotKind, SlotState, ARENA_HEADER_SIZE,
    SLOT_BYTES, SLOT_DESC_SIZE,
};
use otap_format::checksum::crc32c;
use otap_format::{FromBytes, IntoBytes, Uuid};
use otap_sys::{HugeArena, HugePageSize};

const TWO_MIB: usize = 2 * 1024 * 1024;

fn agg() -> Uuid {
    Uuid::from_bytes([0x6A; 16])
}

fn memory() -> HugeArena {
    HugeArena::anonymous(TWO_MIB, HugePageSize::TwoMiB).expect("2 MiB of heap")
}

fn arena() -> Arena {
    Arena::create(memory(), agg(), 42).expect("a fresh arena")
}

fn pattern(tag: u64, k: u32) -> Vec<u8> {
    let mut block = vec![(tag as u8) | 1; SLOT_BYTES];
    block[..8].copy_from_slice(&tag.to_le_bytes());
    block[8..12].copy_from_slice(&k.to_le_bytes());
    block
}

fn write_pattern(a: &mut Arena, ring: RingRef, blocks: u32, tag: u64) -> Vec<u32> {
    let dst = a.run_mut(ring, blocks).expect("a reserved run is writable");
    let mut crcs = Vec::with_capacity(blocks as usize);
    for k in 0..blocks {
        let p = pattern(tag, k);
        dst[k as usize * SLOT_BYTES..(k as usize + 1) * SLOT_BYTES].copy_from_slice(&p);
        crcs.push(crc32c(&p));
    }
    crcs
}

fn placed_run(a: &mut Arena, blocks: u32, vol: u32, lba: u64, tag: u64) -> (RingRef, u64) {
    let ring = a.reserve(blocks).expect("room in the ring");
    let crcs = write_pattern(a, ring, blocks, tag);
    let seq = a.next_seq();
    a.fill(ring, vol, lba, seq, &crcs).expect("fill");
    a.place(ring, blocks).expect("place");
    (ring, seq)
}

fn desc_offset(slot: u32) -> usize {
    ARENA_HEADER_SIZE + slot as usize * SLOT_DESC_SIZE
}

fn read_desc(mem: &HugeArena, slot: u32) -> SlotDesc {
    let at = desc_offset(slot);
    SlotDesc::read_from_bytes(&mem.as_slice()[at..at + SLOT_DESC_SIZE]).expect("32 bytes")
}

fn write_desc(mem: &mut HugeArena, slot: u32, desc: &SlotDesc) {
    let at = desc_offset(slot);
    mem.as_mut_slice()[at..at + SLOT_DESC_SIZE].copy_from_slice(desc.as_bytes());
}

/// What `fill` stores for block `k` of a write before the word: the
/// address, the sequence, the CRC and the kind.
fn store_fields(mem: &mut HugeArena, slot: u32, vol: u32, lba: u64, seq: u64, crc: u32) {
    let mut d = read_desc(mem, slot);
    d.addr = SlotDesc::pack_addr(vol, lba);
    d.seq = seq;
    d.crc32c = crc;
    d.kind = SlotKind::Data.as_u32();
    write_desc(mem, slot, &d);
}

/// The fields and then the word, as `fill` does for one slot.
fn store_filled(mem: &mut HugeArena, slot: u32, vol: u32, lba: u64, seq: u64, crc: u32) {
    store_fields(mem, slot, vol, lba, seq, crc);
    let mut d = read_desc(mem, slot);
    d.word = d.word_for(SlotState::Filled, d.lap());
    write_desc(mem, slot, &d);
}

/// Writes runs of four blocks through the ring, each placed and committed,
/// until `laps` laps have passed; `Err` names the write that found the
/// ring full.
fn turn_the_ring(a: &mut Arena, laps: u32) -> Result<(), String> {
    let writes = laps * a.slots() / 4;
    for i in 0..writes {
        let ring = a
            .reserve(4)
            .map_err(|e| format!("write {i} of {writes}: {e}, window {:?}", a.window()))?;
        let crcs = write_pattern(a, ring, 4, 1000 + u64::from(i));
        let seq = a.next_seq();
        a.fill(ring, 1, u64::from(i) * 4, seq, &crcs)
            .map_err(|e| e.to_string())?;
        a.place(ring, 4).map_err(|e| e.to_string())?;
        a.commit(ring, 4).map_err(|e| e.to_string())?;
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// The interrupted fill (pitfall 21)
// ---------------------------------------------------------------------------

/// A kill inside a four-block fill after slot 1 was filled whole and slot
/// 2's fields were stored, with an acknowledged write placed before it.
/// Before the fix slot 2 stayed `Free` under the reservation's stale seal:
/// once the kept write committed, the tail stopped there for ever, and
/// within a lap every `reserve` answered `Full`.
#[test]
fn a_fill_killed_between_a_slots_fields_and_its_word_does_not_stall_the_ring() {
    let mut a = arena();
    let (kept, _) = placed_run(&mut a, 1, 0, 0, 1);
    let ring = a.reserve(4).unwrap();
    assert_eq!(ring, RingRef { slot: 1, lap: 0 });
    let crcs = write_pattern(&mut a, ring, 4, 2);
    let seq = a.next_seq();
    let mut mem = a.into_memory();
    store_filled(&mut mem, 1, 0, 100, seq, crcs[0]);
    store_fields(&mut mem, 2, 0, 101, seq, crcs[1]);
    let stale = read_desc(&mem, 2);
    assert_eq!(stale.state(), SlotState::Free);
    assert!(
        !stale.verify() && stale.sealed_as().is_none(),
        "a stale seal"
    );

    let (mut a, report) = Arena::attach(mem, &agg()).unwrap();
    assert_eq!(report.placed.len(), 1, "the acknowledged write is kept");
    assert_eq!(report.placed[0].ring, kept);
    assert!(
        report.damaged.is_empty(),
        "a kill is not damage: {report:?}"
    );
    assert_eq!(report.interrupted_fills, 1);
    assert_eq!(
        a.descriptor(2).unwrap().state(),
        SlotState::Committed,
        "the interrupted slot is released"
    );
    a.commit(kept, 1).unwrap();
    assert_eq!(a.occupied(), 0, "nothing is live: the window is empty");
    turn_the_ring(&mut a, 3).expect("the ring keeps turning");
}

/// The same kill on the run's first slot: its successors are still the
/// reservation's.
#[test]
fn a_fill_killed_on_its_first_slot_does_not_stall_the_ring() {
    let mut a = arena();
    let (kept, _) = placed_run(&mut a, 2, 0, 0, 1);
    let ring = a.reserve(3).unwrap();
    let crcs = write_pattern(&mut a, ring, 3, 2);
    let seq = a.next_seq();
    let mut mem = a.into_memory();
    store_fields(&mut mem, ring.slot, 0, 50, seq, crcs[0]);

    let (mut a, report) = Arena::attach(mem, &agg()).unwrap();
    assert_eq!(report.placed.len(), 2);
    assert!(report.damaged.is_empty(), "{report:?}");
    assert_eq!(report.interrupted_fills, 1);
    a.commit(kept, 2).unwrap();
    assert_eq!(a.occupied(), 0);
    turn_the_ring(&mut a, 3).expect("the ring keeps turning");
}

/// Killed on the run's last slot the stale slot lies at the head, outside
/// the window: nothing needs releasing and the next reservation re-seals
/// it.
#[test]
fn a_fill_killed_on_its_last_slot_is_left_to_the_next_reservation() {
    let mut a = arena();
    let (kept, _) = placed_run(&mut a, 1, 0, 0, 1);
    let ring = a.reserve(2).unwrap();
    let crcs = write_pattern(&mut a, ring, 2, 2);
    let seq = a.next_seq();
    let mut mem = a.into_memory();
    store_filled(&mut mem, ring.slot, 0, 7, seq, crcs[0]);
    store_fields(&mut mem, ring.slot + 1, 0, 8, seq, crcs[1]);

    let (mut a, report) = Arena::attach(mem, &agg()).unwrap();
    assert!(report.damaged.is_empty(), "{report:?}");
    assert_eq!(report.dropped_filled, 1);
    assert_eq!(report.interrupted_fills, 0, "outside the window");
    assert_eq!(a.window().1, u64::from(ring.slot + 1), "the head is at it");
    a.commit(kept, 1).unwrap();
    assert_eq!(a.occupied(), 0);
    turn_the_ring(&mut a, 3).expect("the ring keeps turning");
}

/// A word damaged inside a placed write so that it reads as a `Free` slot
/// whose seal matches no state: its neighbours of the same write are
/// placed, so the fill went past it and this is damage, not a kill. Before
/// the fix the slot was neither kept nor reported: an acknowledged block
/// vanished without a hold, and the tail stalled on it.
#[test]
fn a_placed_word_damaged_to_read_free_is_reported_as_damage() {
    let mut a = arena();
    let (_, seq) = placed_run(&mut a, 4, 3, 40, 1);
    let mut mem = a.into_memory();
    let mut d = read_desc(&mem, 2);
    // State bits 00, lap kept, a CRC that belongs to no state.
    d.word = u64::from(d.lap() << 2) | u64::from(!(d.word >> 32) as u32 ^ 0x55) << 32;
    write_desc(&mut mem, 2, &d);
    assert_eq!(read_desc(&mem, 2).state(), SlotState::Free);
    assert!(read_desc(&mem, 2).sealed_as().is_none());

    let (mut a, report) = Arena::attach(mem, &agg()).unwrap();
    assert_eq!(report.interrupted_fills, 0);
    assert_eq!(report.damaged.len(), 1, "{report:?}");
    assert_eq!(report.damaged[0].slot, 2);
    assert_eq!(report.damaged[0].damage, Damage::Descriptor);
    assert!(report.is_damaged(), "the engine holds the volume");
    let kept: Vec<u32> = report.placed.iter().map(|p| p.ring.slot).collect();
    assert_eq!(kept, vec![0, 1, 3]);
    assert!(report.placed.iter().all(|p| p.seq == seq));
    // The other three blocks commit and the window empties.
    a.commit(RingRef { slot: 0, lap: 0 }, 2).unwrap();
    a.commit(RingRef { slot: 3, lap: 0 }, 1).unwrap();
    assert_eq!(a.occupied(), 0);
    turn_the_ring(&mut a, 2).expect("the ring keeps turning");
}

// ---------------------------------------------------------------------------
// The interrupted place
// ---------------------------------------------------------------------------

/// A kill inside `place` after the first two of four slots were placed:
/// the write was in the index but never acknowledged (the acknowledgement
/// follows the whole `place`). Before the fix the two placed slots were
/// kept and replayed, a torn write; now the whole write is dropped.
#[test]
fn a_place_killed_midway_drops_the_whole_write() {
    let mut a = arena();
    let (kept, kept_seq) = placed_run(&mut a, 1, 0, 0, 1);
    let ring = a.reserve(4).unwrap();
    let crcs = write_pattern(&mut a, ring, 4, 2);
    let seq = a.next_seq();
    a.fill(ring, 0, 10, seq, &crcs).unwrap();
    let mut mem = a.into_memory();
    for slot in ring.slot..ring.slot + 2 {
        let mut d = read_desc(&mem, slot);
        d.word = d.word_for(SlotState::Placed, d.lap());
        write_desc(&mut mem, slot, &d);
    }

    let (mut a, report) = Arena::attach(mem, &agg()).unwrap();
    assert_eq!(
        report.placed.len(),
        1,
        "only the acknowledged write: {report:?}"
    );
    assert_eq!(report.placed[0].seq, kept_seq);
    assert_eq!(report.torn_places, 2);
    assert_eq!(report.dropped_filled, 2);
    assert!(report.damaged.is_empty());
    for slot in ring.slot..ring.slot + 4 {
        assert_eq!(a.descriptor(slot).unwrap().state(), SlotState::Committed);
    }
    a.commit(kept, 1).unwrap();
    assert_eq!(a.occupied(), 0);
}

// ---------------------------------------------------------------------------
// Deferred writes (pitfall 22)
// ---------------------------------------------------------------------------

/// An arena with one acknowledged write, one damaged placed slot, one
/// reservation and one filled slot, as a killed process leaves it.
fn killed_arena() -> HugeArena {
    let mut a = arena();
    placed_run(&mut a, 2, 0, 0, 1);
    placed_run(&mut a, 1, 0, 9, 2);
    let ring = a.reserve(1).unwrap();
    let crcs = write_pattern(&mut a, ring, 1, 3);
    let seq = a.next_seq();
    a.fill(ring, 0, 20, seq, &crcs).unwrap();
    a.reserve(2).unwrap();
    let mut mem = a.into_memory();
    // The data of the second write (slot 2) rots.
    let at = hdr_data_off(&mem) + 2 * SLOT_BYTES + 77;
    mem.as_mut_slice()[at] ^= 0xFF;
    mem
}

fn hdr_data_off(mem: &HugeArena) -> usize {
    let hdr = otap_arena::ArenaHeader::read_from_bytes(&mem.as_slice()[..ARENA_HEADER_SIZE])
        .expect("a header");
    hdr.data_off as usize
}

/// `attach_deferred` reports what `attach` reports and writes nothing;
/// applying the writes leaves the memory exactly as `attach` leaves it.
#[test]
fn a_deferred_attach_writes_nothing_until_it_is_applied() {
    let found = killed_arena();
    let before = found.as_slice().to_vec();
    let mut twin = memory();
    twin.as_mut_slice().copy_from_slice(&before);

    let (mut deferred, report) = Arena::attach_deferred(found, &agg()).unwrap();
    assert!(report.is_damaged(), "{report:?}");
    assert!(deferred.has_pending_writes());
    assert_eq!(
        deferred.memory().as_slice(),
        before.as_slice(),
        "nothing written before the apply"
    );
    assert_eq!(
        deferred.reserve(1),
        Err(ArenaError::Pending),
        "nothing is handed out on a table that is not written yet"
    );
    let (immediate, report_now) = Arena::attach(twin, &agg()).unwrap();
    assert_eq!(report, report_now);
    assert_eq!(deferred.window(), immediate.window());
    assert_eq!(deferred.peek_seq(), immediate.peek_seq());

    deferred.apply_pending_writes();
    assert!(!deferred.has_pending_writes());
    assert_eq!(
        deferred.memory().as_slice(),
        immediate.memory().as_slice(),
        "the applied writes are the attach's"
    );
    assert_eq!(deferred.window(), immediate.window());
    assert!(deferred.reserve(1).is_ok());
}

/// A release asked for while the attach's writes are pending joins them:
/// the slot stays `Placed` in the memory until the apply.
#[test]
fn a_release_during_a_deferred_attach_waits_for_the_apply() {
    let mut a = arena();
    let (ring, _) = placed_run(&mut a, 3, 0, 0, 1);
    let (mut a, report) = Arena::attach_deferred(a.into_memory(), &agg()).unwrap();
    assert_eq!(report.placed.len(), 3);
    a.release_after_attach(ring, 3).unwrap();
    assert_eq!(a.descriptor(ring.slot).unwrap().state(), SlotState::Placed);
    assert_eq!(a.occupied(), 3);
    a.apply_pending_writes();
    assert_eq!(
        a.descriptor(ring.slot).unwrap().state(),
        SlotState::Committed
    );
    assert_eq!(a.occupied(), 0);
    // Without pending writes it is an ordinary commit.
    let (ring, _) = placed_run(&mut a, 2, 0, 5, 2);
    a.release_after_attach(ring, 2).unwrap();
    assert_eq!(a.occupied(), 0);
    assert!(matches!(
        a.release_after_attach(ring, 2),
        Err(ArenaError::BadTransition { .. })
    ));
}

/// `create_deferred` leaves a blank or lost memory as it found it until
/// the apply, so a boot stopped before it recorded its hold finds the same
/// blank arena again.
#[test]
fn a_deferred_create_writes_nothing_until_it_is_applied() {
    let blank = memory();
    let mut a = Arena::create_deferred(blank, agg(), 7).unwrap();
    assert!(a.has_pending_writes());
    assert!(Arena::is_blank(a.memory()), "still blank");
    assert_eq!(a.reserve(1), Err(ArenaError::Pending));
    assert_eq!(a.occupied(), 0);
    let a = a.into_memory();
    assert!(Arena::is_blank(&a), "dropped unapplied: still blank");

    let mut lost = memory();
    lost.as_mut_slice()[..16].copy_from_slice(&[0xEE; 16]);
    let mut a = Arena::create_deferred(lost, agg(), 7).unwrap();
    assert_eq!(&a.memory().as_slice()[..16], &[0xEE; 16]);
    a.apply_pending_writes();
    assert!(Arena::can_attach(a.memory(), &agg()).is_ok());
    assert_eq!(a.header().attach_count(), Some(1));
    let fresh = Arena::create(memory(), agg(), 7).unwrap();
    assert_eq!(
        a.memory().as_slice()[..hdr_data_off(fresh.memory())],
        fresh.memory().as_slice()[..hdr_data_off(fresh.memory())],
        "the applied create is the create"
    );
    let (_, report) = Arena::attach(a.into_memory(), &agg()).unwrap();
    assert!(report.placed.is_empty() && report.damaged.is_empty());
}

// ---------------------------------------------------------------------------
// An arena made for another size (pitfall 13)
// ---------------------------------------------------------------------------

fn copy_into(src: &HugeArena, len: usize) -> HugeArena {
    let mut m = HugeArena::anonymous(len, HugePageSize::TwoMiB).expect("memory");
    let n = src.len().min(len);
    m.as_mut_slice()[..n].copy_from_slice(&src.as_slice()[..n]);
    m
}

/// A file a larger size grew keeps the arena whole at its start: the
/// attach refuses its geometry, and the survey reads it under the
/// geometry its header records, counting what a recreation would destroy.
#[test]
fn a_survey_reads_an_arena_under_its_recorded_geometry() {
    let mut a = arena();
    placed_run(&mut a, 3, 0, 0, 1);
    let (done, _) = placed_run(&mut a, 2, 0, 9, 2);
    a.commit(done, 2).unwrap();
    let own = a.into_memory();
    let grown = copy_into(&own, 3 * TWO_MIB);
    assert_eq!(
        Arena::can_attach(&grown, &agg()),
        Err(ArenaError::BadGeometry)
    );
    let survey = Arena::survey(&grown, &agg()).unwrap();
    assert_eq!(survey.recorded_bytes, TWO_MIB as u64);
    assert_eq!(survey.slots, 507);
    assert_eq!(survey.placed, 3);
    assert!(!survey.holds_nothing());
    assert_eq!(
        Arena::survey(&own, &agg()),
        Ok(survey),
        "the same at its own length"
    );

    // Committed, it holds nothing.
    let (mut a, report) = Arena::attach(own, &agg()).unwrap();
    for p in &report.placed {
        a.commit(p.ring, 1).unwrap();
    }
    let survey = Arena::survey(&copy_into(a.memory(), 2 * TWO_MIB), &agg()).unwrap();
    assert_eq!(survey.placed, 0);
    assert!(survey.holds_nothing());
}

/// A placed slot whose state bits were damaged still counts: it may be an
/// acknowledged write.
#[test]
fn a_survey_counts_a_slot_sealed_as_placed_under_damaged_state_bits() {
    let mut a = arena();
    placed_run(&mut a, 1, 0, 0, 1);
    let mut mem = a.into_memory();
    let mut d = read_desc(&mem, 0);
    d.word ^= 0b11; // Placed (0b10) now reads Filled (0b01)
    write_desc(&mut mem, 0, &d);
    assert_ne!(read_desc(&mem, 0).state(), SlotState::Placed);
    assert_eq!(read_desc(&mem, 0).sealed_as(), Some(SlotState::Placed));
    assert_eq!(Arena::survey(&mem, &agg()).unwrap().placed, 1);
}

/// Another pool's arena is that, whatever its size: the aggregate is
/// checked before the geometry, so a boot refuses it instead of taking
/// the size mismatch for its own arena and recreating it.
#[test]
fn another_pools_arena_is_refused_before_its_geometry_is_judged() {
    let mut a = arena();
    placed_run(&mut a, 1, 0, 0, 1);
    let grown = copy_into(a.memory(), 2 * TWO_MIB);
    let other = Uuid::from_bytes([0x6B; 16]);
    assert_eq!(
        Arena::can_attach(&grown, &other),
        Err(ArenaError::WrongAggregate { found: agg() })
    );
    assert_eq!(
        Arena::survey(&grown, &other),
        Err(ArenaError::WrongAggregate { found: agg() })
    );
    assert_eq!(
        Arena::can_attach(a.memory(), &other),
        Err(ArenaError::WrongAggregate { found: agg() })
    );
}

/// A recorded geometry that the memory cannot hold (an arena file cut
/// short) cannot be read; blank memory is no arena at all.
#[test]
fn a_survey_refuses_what_it_cannot_read() {
    let big = Arena::create(
        HugeArena::anonymous(3 * TWO_MIB, HugePageSize::TwoMiB).unwrap(),
        agg(),
        1,
    )
    .unwrap();
    let cut = copy_into(big.memory(), TWO_MIB);
    assert_eq!(Arena::survey(&cut, &agg()), Err(ArenaError::BadGeometry));
    assert!(matches!(
        Arena::survey(&memory(), &agg()),
        Err(ArenaError::BadMagic { found: 0 })
    ));
}
