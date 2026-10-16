//! The Step 6 adversarial review (`docs/reviews/2026-10-16-step6-qa-adversarial.md`):
//! the arena's findings. Each test reproduced its finding before the fix
//! and pins the fixed behaviour.

use otap_arena::{
    Arena, Damage, Fills, RingRef, SlotDesc, SlotState, ARENA_HEADER_SIZE, SLOT_BYTES,
    SLOT_DESC_SIZE,
};
use otap_format::checksum::crc32c;
use otap_format::{FromBytes, IntoBytes, Uuid};
use otap_sys::{HugeArena, HugePageSize};

const TWO_MIB: usize = 2 * 1024 * 1024;

fn agg() -> Uuid {
    Uuid::from_bytes([0x6B; 16])
}

fn arena() -> Arena {
    let mem = HugeArena::anonymous(TWO_MIB, HugePageSize::TwoMiB).expect("2 MiB of heap");
    Arena::create(mem, agg(), 43).expect("a fresh arena")
}

/// A placed write of `blocks` blocks at `lba` of `vol`; its ring and seq.
fn placed_run(a: &mut Arena, blocks: u32, vol: u32, lba: u64, tag: u8) -> (RingRef, u64) {
    let ring = a.reserve(blocks).expect("room in the ring");
    let dst = a.run_mut(ring, blocks).expect("a reserved run is writable");
    let mut crcs = Vec::with_capacity(blocks as usize);
    for k in 0..blocks as usize {
        let block = &mut dst[k * SLOT_BYTES..(k + 1) * SLOT_BYTES];
        block.fill(tag);
        block[0] = k as u8;
        crcs.push(crc32c(block));
    }
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

/// Damages slot `slot`'s word so it reads `Free` under a seal of no state
/// (the damage of `step6a_arena_tests::a_placed_word_damaged_to_read_free_is_reported_as_damage`),
/// with its lap moved by `lap_delta`.
fn damage_to_free(mem: &mut HugeArena, slot: u32, lap_delta: u32) {
    let mut d = read_desc(mem, slot);
    let lap = d.lap() + lap_delta;
    d.word = u64::from(lap << 2) | u64::from(!(d.word >> 32) as u32 ^ 0x55) << 32;
    write_desc(mem, slot, &d);
    let back = read_desc(mem, slot);
    assert_eq!(back.state(), SlotState::Free);
    assert!(back.sealed_as().is_none());
}

/// D10: a placed word damaged to read `Free` was judged only by its
/// neighbours of the same write, compared under the damaged word's own
/// lap. A write of one block (every 4 KiB write and every hole record) has
/// no such neighbour, so the slot was taken for an interrupted fill and
/// released: an acknowledged block silently lost, the volume not held.
/// For a serial writer (the engine fills one reservation at a time, whole,
/// on its thread) only the newest reservation can hold an interrupted
/// fill, so a stale slot with anything written after it is damage
/// (`Fills::Serial`, what the engine's boot attaches with).
#[test]
fn d10_a_damaged_placed_word_of_a_one_block_write_is_damage_not_a_fill() {
    let mut a = arena();
    let (_, first) = placed_run(&mut a, 1, 3, 40, 1);
    let (_, second) = placed_run(&mut a, 2, 3, 41, 2);
    let mut mem = a.into_memory();
    damage_to_free(&mut mem, 0, 0);
    let (_, report) = Arena::attach_with(mem, &agg(), Fills::Serial).unwrap();
    assert_eq!(report.interrupted_fills, 0, "{report:?}");
    assert_eq!(report.damaged.len(), 1, "{report:?}");
    assert_eq!(report.damaged[0].slot, 0);
    assert_eq!(report.damaged[0].damage, Damage::Descriptor);
    assert!(report.is_damaged(), "the engine holds the volume");
    assert!(report
        .placed
        .iter()
        .all(|p| p.seq == second && p.seq != first));
}

/// D10, the second case: the same damage with a lap bit flipped too. The
/// neighbours' laps no longer matched the damaged one, so even a slot in
/// the middle of a placed write was taken for an interrupted fill, and the
/// rest of the write was replayed torn. The lap a stale slot is judged
/// under is now the one its position in the window implies.
#[test]
fn d10_a_damaged_placed_word_with_a_flipped_lap_is_damage_not_a_fill() {
    let mut a = arena();
    let (_, seq) = placed_run(&mut a, 4, 3, 40, 1);
    let mut mem = a.into_memory();
    damage_to_free(&mut mem, 2, 1);
    let (_, report) = Arena::attach(mem, &agg()).unwrap();
    assert_eq!(report.interrupted_fills, 0, "{report:?}");
    assert_eq!(report.damaged.len(), 1, "{report:?}");
    assert_eq!(report.damaged[0].slot, 2);
    assert!(report.is_damaged(), "the engine holds the volume");
    let kept: Vec<u32> = report.placed.iter().map(|p| p.ring.slot).collect();
    assert_eq!(kept, vec![0, 1, 3]);
    assert!(report.placed.iter().all(|p| p.seq == seq));
}

/// R5: a damaged slot was rewritten `Committed` under the lap its damaged
/// word claimed, which the rewrite then sealed as verified: one flipped
/// lap bit moved the next attach's head by laps, the live window came out
/// wider than the ring, and the arena was refused as inconsistent (the
/// engine then recreates it, losing every unflushed write of every
/// volume). A damaged slot is rewritten under the lap its position
/// implies.
#[test]
fn r5_a_rewritten_damaged_slot_never_moves_the_next_attachs_head() {
    let mut a = arena();
    for i in 0..4u8 {
        placed_run(&mut a, 1, 0, u64::from(i), 10 + i);
    }
    let mut mem = a.into_memory();
    let mut d = read_desc(&mem, 1);
    d.word ^= 1 << 22;
    write_desc(&mut mem, 1, &d);
    let (a, report) = Arena::attach(mem, &agg()).unwrap();
    assert_eq!(report.damaged.len(), 1, "{report:?}");
    // The re-attach's writes land; the process dies again before any CP.
    let mem = a.into_memory();
    let (_, again) = Arena::attach(mem, &agg()).expect("the second attach accepts the arena");
    let kept: Vec<u32> = again.placed.iter().map(|p| p.ring.slot).collect();
    assert_eq!(kept, vec![0, 2, 3]);
}
