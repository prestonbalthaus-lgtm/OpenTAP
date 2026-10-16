//! Adversarial tests for `otap-arena` (the Step 3 review, ported).
//!
//! Re-attach after every interleaving of reserve, fill, place, commit,
//! discard and a single-slot release, including a `kill -9` in the middle of
//! a fill, a place or a commit (modelled by rewinding the words the
//! transition had not reached yet); wrap-around with reservations never
//! filled; two laps of reuse and stale references; `Full` accounting; and a
//! descriptor table damaged between two processes.
//!
//! The review found five defects here (a run that fit an empty ring was
//! refused, descriptors had no CRC so damage was replayed or built an
//! absurd window, and a re-attach reissued acknowledged sequence numbers);
//! the first five tests pin the fixes.

use otap_arena::{
    Arena, ArenaError, ArenaHeader, Damage, RingRef, SlotDesc, SlotState, ARENA_HEADER_SIZE,
    SLOT_BYTES, SLOT_DESC_SIZE, SLOT_WORD_OFFSET,
};
use otap_format::checksum::crc32c;
use otap_format::{FromBytes, IntoBytes, Uuid};
use otap_sys::{HugeArena, HugePageSize};
use proptest::prelude::*;

const TWO_MIB: usize = 2 * 1024 * 1024;

fn agg() -> Uuid {
    Uuid::from_bytes([0x5A; 16])
}

fn memory() -> HugeArena {
    HugeArena::anonymous(TWO_MIB, HugePageSize::TwoMiB).expect("2 MiB of heap")
}

fn arena() -> Arena {
    Arena::create(memory(), agg(), 42).expect("a fresh arena")
}

/// The bytes of block `k` of the write tagged `tag`: self-describing, so a
/// slot that holds the wrong data is caught.
fn pattern(tag: u64, k: u32) -> Vec<u8> {
    let fill = (tag.wrapping_mul(0x9E37_79B9_7F4A_7C15) ^ u64::from(k)).to_le_bytes()[3] | 1;
    let mut block = vec![fill; SLOT_BYTES];
    block[..8].copy_from_slice(&tag.to_le_bytes());
    block[8..12].copy_from_slice(&k.to_le_bytes());
    block
}

/// Copies the pattern of blocks `k0..k0 + blocks` of `tag` into the run and
/// returns the block CRCs.
fn write_pattern(a: &mut Arena, ring: RingRef, blocks: u32, tag: u64, k0: u32) -> Vec<u32> {
    let dst = a.run_mut(ring, blocks).expect("a reserved run is writable");
    let mut crcs = Vec::with_capacity(blocks as usize);
    for k in 0..blocks {
        let p = pattern(tag, k0 + k);
        dst[k as usize * SLOT_BYTES..(k as usize + 1) * SLOT_BYTES].copy_from_slice(&p);
        crcs.push(crc32c(&p));
    }
    crcs
}

/// Reserves, fills and places a run: from here on its write may have been
/// acknowledged.
fn placed_run(a: &mut Arena, blocks: u32, vol: u32, lba: u64, tag: u64) -> (RingRef, u64) {
    let ring = a.reserve(blocks).expect("room in the ring");
    let crcs = write_pattern(a, ring, blocks, tag, 0);
    let seq = a.next_seq();
    a.fill(ring, vol, lba, seq, &crcs).expect("fill");
    a.place(ring, blocks).expect("place");
    (ring, seq)
}

fn desc_offset(mem: &HugeArena, slot: u32) -> usize {
    let hdr = ArenaHeader::read_from_bytes(&mem.as_slice()[..ARENA_HEADER_SIZE])
        .expect("a header-sized prefix");
    hdr.desc_off as usize + slot as usize * SLOT_DESC_SIZE
}

fn read_desc(mem: &HugeArena, slot: u32) -> SlotDesc {
    let at = desc_offset(mem, slot);
    SlotDesc::read_from_bytes(&mem.as_slice()[at..at + SLOT_DESC_SIZE]).expect("32 bytes")
}

fn write_desc(mem: &mut HugeArena, slot: u32, desc: &SlotDesc) {
    let at = desc_offset(mem, slot);
    mem.as_mut_slice()[at..at + SLOT_DESC_SIZE].copy_from_slice(desc.as_bytes());
}

/// Rewinds a slot to `state` with its lap kept and its word sealed, as a
/// process killed before the store that would have changed it leaves the
/// table (every transition is one word store, so a slot is either before
/// or after it, never between).
fn set_state(mem: &mut HugeArena, slot: u32, state: SlotState) {
    let mut desc = read_desc(mem, slot);
    desc.word = desc.word_for(state, desc.lap());
    write_desc(mem, slot, &desc);
}

// ---------------------------------------------------------------------------
// Defects the review found, pinned
// ---------------------------------------------------------------------------

#[test]
fn reserve_hands_out_a_run_that_fits_an_empty_ring() {
    let mut a = arena();
    assert_eq!(a.slots(), 507);
    let (first, _) = placed_run(&mut a, 250, 0, 0, 1);
    a.commit(first, 250).unwrap();
    assert_eq!(a.occupied(), 0, "nothing is live: the window is empty");
    // The never-used rest of the lap used to count against the window, so
    // a run longer than both the lap remainder and the head's offset was
    // refused for ever on an idle engine.
    assert_eq!(
        a.reserve(300),
        Ok(RingRef { slot: 0, lap: 1 }),
        "the ring is empty"
    );
}

#[test]
fn attach_reports_a_placed_slot_whose_word_is_damaged() {
    let mut a = arena();
    let (run, _) = placed_run(&mut a, 3, 2, 100, 7);
    let mut mem = a.into_memory();
    // Flipped bits in the sealed word (the state and the lap live there).
    let at = desc_offset(&mem, run.slot + 1) + SLOT_WORD_OFFSET;
    mem.as_mut_slice()[at] ^= 0x7F;
    let (_, report) = Arena::attach(mem, &agg()).unwrap();
    let lbas: Vec<u64> = report.placed.iter().map(|p| p.lba).collect();
    assert_eq!(lbas, vec![100, 102], "the damaged slot is not replayed");
    assert_eq!(report.damaged.len(), 1);
    assert_eq!(report.damaged[0].slot, run.slot + 1);
    assert_eq!(report.damaged[0].damage, Damage::Descriptor);
    assert!(report.is_damaged(), "the engine holds the volumes");
}

#[test]
fn attach_reports_a_descriptor_whose_address_was_damaged() {
    let mut a = arena();
    let (run, _) = placed_run(&mut a, 1, 2, 100, 8);
    let mut mem = a.into_memory();
    let mut desc = read_desc(&mem, run.slot);
    desc.addr ^= 1 << 20;
    write_desc(&mut mem, run.slot, &desc);
    // A single flipped bit in the address used to be replayed to the wrong
    // block; the descriptor CRC catches it now.
    let (_, report) = Arena::attach(mem, &agg()).unwrap();
    assert!(
        report.placed.is_empty(),
        "nothing is replayed from a damaged descriptor"
    );
    assert_eq!(report.damaged.len(), 1);
    assert_eq!(report.damaged[0].slot, run.slot);
    assert_eq!(report.damaged[0].damage, Damage::Descriptor);
}

#[test]
fn a_damaged_lap_is_reported_and_the_window_stays_within_the_ring() {
    let mut a = arena();
    placed_run(&mut a, 2, 1, 0, 9);
    let (second, _) = placed_run(&mut a, 2, 1, 2, 10);
    let mut mem = a.into_memory();
    let mut desc = read_desc(&mem, second.slot);
    // Lap bit 24 is word bit 26.
    desc.word ^= 1 << 26;
    write_desc(&mut mem, second.slot, &desc);
    // One flipped lap bit used to move the head 2^24 laps ahead: the window
    // spanned billions of positions and the engine hung.
    let (mut a, report) = Arena::attach(mem, &agg()).unwrap();
    let (tail, head) = a.window();
    assert!(
        head - tail <= u64::from(a.slots()),
        "the window {tail}..{head} spans more than the ring of {} slots",
        a.slots()
    );
    assert_eq!(report.damaged.len(), 1);
    assert_eq!(
        report.placed.len(),
        3,
        "the other three placed slots are kept"
    );
    assert!(a.reserve(4).is_ok(), "the ring still hands out runs");
}

#[test]
fn reattach_never_reissues_a_sequence_number() {
    let mut a = arena();
    let mut last = 0;
    for i in 0..a.slots() {
        let (ring, seq) = placed_run(&mut a, 1, 0, u64::from(i), u64::from(i));
        a.commit(ring, 1).unwrap();
        last = seq;
    }
    assert_eq!(
        last, 507,
        "507 acknowledged writes, sequence numbers 1 to 507"
    );
    // A write of the whole ring reserves every slot (they become `Free` on
    // lap 1, their old sequence numbers still in the descriptors) and the
    // daemon dies before the copy finishes.
    assert_eq!(a.reserve(a.slots()), Ok(RingRef { slot: 0, lap: 1 }));
    let (a, report) = Arena::attach(a.into_memory(), &agg()).unwrap();
    assert!(report.placed.is_empty());
    // The next sequence number is the maximum over every descriptor,
    // whatever its state: the reserved slots still carry their previous
    // occupants' numbers.
    assert!(
        a.peek_seq() > last,
        "next sequence number {} after re-attach; {last} was already acknowledged",
        a.peek_seq()
    );
}

// ---------------------------------------------------------------------------
// Behaviour that holds
// ---------------------------------------------------------------------------

#[test]
fn a_kill_between_fill_and_place_drops_the_write_and_frees_its_slots() {
    let mut a = arena();
    let (kept, kept_seq) = placed_run(&mut a, 2, 1, 10, 1);
    let ring = a.reserve(3).unwrap();
    let crcs = write_pattern(&mut a, ring, 3, 2, 0);
    let seq = a.next_seq();
    a.fill(ring, 1, 20, seq, &crcs).unwrap();
    let (mut a, report) = Arena::attach(a.into_memory(), &agg()).unwrap();
    assert_eq!(report.dropped_filled, 3);
    assert_eq!(report.placed.len(), 2);
    assert!(report
        .placed
        .iter()
        .all(|p| p.seq == kept_seq && p.ring.lap == kept.lap));
    assert_eq!(
        a.descriptor(ring.slot).unwrap().state(),
        SlotState::Committed
    );
    assert!(a.run(ring, 3).is_err(), "the dropped run is not readable");
    assert!(a.peek_seq() > kept_seq);
    // The kept run is still exactly what was written.
    let data = a.run(kept, 2).unwrap().to_vec();
    assert_eq!(&data[..SLOT_BYTES], pattern(1, 0).as_slice());
    assert_eq!(&data[SLOT_BYTES..], pattern(1, 1).as_slice());
    // Committing the kept run empties the window: nothing leaked.
    a.commit(kept, 2).unwrap();
    assert_eq!(a.occupied(), 0);
}

#[test]
fn a_reservation_never_filled_pins_the_window_until_a_reattach() {
    let mut a = arena();
    // A write whose copy failed: reserved, never filled, never discarded
    // (the engine discards such a run; this is what happens when a caller
    // does not).
    let orphan = a.reserve(1).unwrap();
    let mut committed = 0u32;
    loop {
        match a.reserve(4) {
            Ok(ring) => {
                let crcs = write_pattern(&mut a, ring, 4, u64::from(committed), 0);
                let seq = a.next_seq();
                a.fill(ring, 0, u64::from(committed) * 4, seq, &crcs)
                    .unwrap();
                a.place(ring, 4).unwrap();
                a.commit(ring, 4).unwrap();
                committed += 1;
            }
            Err(ArenaError::Full { .. }) => break,
            Err(e) => panic!("{e}"),
        }
        assert!(committed < 1_000, "the ring never filled");
    }
    // Nothing is live but the orphan, yet the window covers the whole lap:
    // the tail stops at a `Free` slot of its own lap.
    assert_eq!(a.window().0, 0, "the tail is stuck at the orphan");
    assert!(a.occupied() > 500, "occupied {}", a.occupied());
    assert_eq!(a.descriptor(orphan.slot).unwrap().state(), SlotState::Free);
    // A discard releases it; so does a re-attach.
    a.discard(orphan, 1).unwrap();
    assert_eq!(a.occupied(), 0, "the discard released the orphan");
    let orphan = a.reserve(1).unwrap();
    assert_eq!(a.occupied(), 1);
    let (mut a, report) = Arena::attach(a.into_memory(), &agg()).unwrap();
    assert!(report.placed.is_empty());
    assert_eq!(a.occupied(), 0, "re-attach turned the orphan reusable");
    assert!(a.reserve(4).is_ok());
    assert!(matches!(
        a.discard(orphan, 1),
        Err(ArenaError::BadTransition { .. }) | Err(ArenaError::Stale { .. })
    ));
}

#[test]
fn one_placed_slot_at_the_tail_pins_the_whole_window() {
    let mut a = arena();
    let (pinned, _) = placed_run(&mut a, 1, 0, 0, 1);
    for i in 0..100u64 {
        let (ring, _) = placed_run(&mut a, 4, 0, 1 + i * 4, 100 + i);
        a.commit(ring, 4).unwrap();
    }
    // One live block, 401 slots in the window: occupancy is the window, not
    // what is live (E16), so a single generation that cannot flush holds
    // the whole arena.
    assert_eq!(a.occupied(), 401);
    let free = a.slots() - a.occupied();
    assert!(matches!(a.reserve(free + 1), Err(ArenaError::Full { .. })));
    a.commit(pinned, 1).unwrap();
    assert_eq!(a.occupied(), 0);
}

#[test]
fn a_superseded_slot_released_inside_a_run_is_not_replayed() {
    let mut a = arena();
    let (run, seq) = placed_run(&mut a, 5, 3, 40, 1);
    // A newer write replaced LBA 42: the engine releases that one slot.
    a.commit(run.offset(2), 1).unwrap();
    let (a, report) = Arena::attach(a.into_memory(), &agg()).unwrap();
    let got: Vec<(u32, u64, u64)> = report
        .placed
        .iter()
        .map(|p| (p.ring.slot, p.lba, p.seq))
        .collect();
    assert_eq!(
        got,
        vec![(0, 40, seq), (1, 41, seq), (3, 43, seq), (4, 44, seq)]
    );
    assert_eq!(a.window(), (0, 5));
}

#[test]
fn two_laps_of_reuse_refuse_every_stale_reference() {
    let mut a = arena();
    let (old, _) = placed_run(&mut a, 4, 0, 0, 1);
    a.commit(old, 4).unwrap();
    // Go around twice with runs that straddle nothing (61 * 7 = 427 per
    // batch, so the lap ends are skipped at different offsets).
    let mut tag = 10u64;
    while a.window().1 < 2 * u64::from(a.slots()) + 4 {
        let (ring, _) = placed_run(&mut a, 7, 0, tag * 7, tag);
        a.commit(ring, 7).unwrap();
        tag += 1;
    }
    let lap_now = (a.window().1 / u64::from(a.slots())) as u32;
    assert!(lap_now >= 2);
    let stale = |a: &mut Arena| {
        assert!(matches!(a.run(old, 4), Err(ArenaError::Stale { .. })));
        assert!(matches!(a.run_mut(old, 4), Err(ArenaError::Stale { .. })));
        assert!(matches!(
            a.fill(old, 0, 0, 1, &[0; 4]),
            Err(ArenaError::Stale { .. })
        ));
        assert!(matches!(a.place(old, 4), Err(ArenaError::Stale { .. })));
        assert!(matches!(a.commit(old, 4), Err(ArenaError::Stale { .. })));
        assert!(matches!(a.discard(old, 4), Err(ArenaError::Stale { .. })));
    };
    stale(&mut a);
    let (mut a, _) = Arena::attach(a.into_memory(), &agg()).unwrap();
    stale(&mut a);
    // A reference from the future is refused too.
    let future = RingRef {
        slot: 0,
        lap: lap_now + 5,
    };
    assert!(matches!(a.run(future, 1), Err(ArenaError::Stale { .. })));
}

#[test]
fn a_write_that_does_not_fit_the_lap_end_skips_it_and_survives_a_kill() {
    let mut a = arena();
    let (first, _) = placed_run(&mut a, 480, 0, 0, 1);
    a.commit(first, 480).unwrap();
    // 27 slots remain on lap 0; a run of 64 skips them. Head at 480, so
    // 64 <= 480 and the skip is allowed.
    let (straddle, seq) = placed_run(&mut a, 64, 1, 1000, 2);
    assert_eq!(straddle, RingRef { slot: 0, lap: 1 });
    for slot in 480..507 {
        let d = a.descriptor(slot).unwrap();
        assert_eq!(d.state(), SlotState::Committed, "skipped {slot}");
        assert_eq!(d.lap(), 0);
    }
    let (mut a, report) = Arena::attach(a.into_memory(), &agg()).unwrap();
    assert_eq!(report.placed.len(), 64);
    assert!(report
        .placed
        .iter()
        .enumerate()
        .all(|(k, p)| p.seq == seq && p.lba == 1000 + k as u64 && p.ring.lap == 1));
    assert_eq!(a.window(), (507, 507 + 64));
    let data = a.run(straddle.offset(63), 1).unwrap().to_vec();
    assert_eq!(data, pattern(2, 63));
    a.commit(straddle, 64).unwrap();
    assert_eq!(a.occupied(), 0);
}

// ---------------------------------------------------------------------------
// Re-attach after every interleaving, kills mid-transition included
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum St {
    Reserved,
    Filled,
    Placed,
    /// The placed prefix of a write whose `place` the kill interrupted:
    /// never acknowledged, dropped with the rest of the write (Step 6A).
    Torn,
}

#[derive(Debug, Clone)]
struct Run {
    ring: RingRef,
    blocks: u32,
    st: St,
    tag: u64,
    /// Index, in the original write, of this run's first block.
    k0: u32,
    vol: u32,
    lba: u64,
    seq: u64,
    crcs: Vec<u32>,
}

impl Run {
    fn slice(&self, from: u32, to: u32) -> Self {
        Self {
            ring: self.ring.offset(from),
            blocks: to - from,
            st: self.st,
            tag: self.tag,
            k0: self.k0 + from,
            vol: self.vol,
            lba: self.lba + u64::from(from),
            seq: self.seq,
            crcs: self.crcs[from as usize..to as usize].to_vec(),
        }
    }

    fn pos(&self, slots: u32) -> u64 {
        u64::from(self.ring.lap) * u64::from(slots) + u64::from(self.ring.slot)
    }
}

#[derive(Debug, Clone, Copy)]
enum Op {
    Reserve(u32),
    Fill(usize),
    Place(usize),
    Commit(usize),
    Release(usize, u32),
    Discard(usize),
    Kill,
    KillMidFill(usize, u32),
    /// A kill between one slot's field stores and its word store (Step 6A).
    KillInsideFill(usize, u32),
    KillMidPlace(usize, u32),
    KillMidCommit(usize, u32),
}

fn op() -> impl Strategy<Value = Op> {
    prop_oneof![
        6 => (1u32..=64).prop_map(Op::Reserve),
        4 => any::<usize>().prop_map(Op::Fill),
        4 => any::<usize>().prop_map(Op::Place),
        3 => any::<usize>().prop_map(Op::Commit),
        1 => (any::<usize>(), any::<u32>()).prop_map(|(i, k)| Op::Release(i, k)),
        1 => any::<usize>().prop_map(Op::Discard),
        1 => Just(Op::Kill),
        1 => (any::<usize>(), any::<u32>()).prop_map(|(i, k)| Op::KillMidFill(i, k)),
        1 => (any::<usize>(), any::<u32>()).prop_map(|(i, k)| Op::KillInsideFill(i, k)),
        1 => (any::<usize>(), any::<u32>()).prop_map(|(i, k)| Op::KillMidPlace(i, k)),
        1 => (any::<usize>(), any::<u32>()).prop_map(|(i, k)| Op::KillMidCommit(i, k)),
    ]
}

/// The index in `runs` of the `pick`-th run in state `st`, if any.
fn pick(runs: &[Run], st: St, pick: usize) -> Option<usize> {
    let candidates: Vec<usize> = runs
        .iter()
        .enumerate()
        .filter(|(_, r)| r.st == st)
        .map(|(i, _)| i)
        .collect();
    if candidates.is_empty() {
        None
    } else {
        Some(candidates[pick % candidates.len()])
    }
}

fn fill_run(a: &mut Arena, run: &mut Run) -> Result<(), TestCaseError> {
    run.seq = a.next_seq();
    run.vol = (run.tag % 1024) as u32;
    run.lba = run.tag * 64;
    a.fill(run.ring, run.vol, run.lba, run.seq, &run.crcs)
        .map_err(|e| TestCaseError::fail(format!("fill: {e}")))?;
    run.st = St::Filled;
    Ok(())
}

/// Re-attaches and checks the report against the model, which has already
/// been updated with whatever the kill left in each state; drops every run
/// that is not `Placed`.
fn reattach(
    mem: HugeArena,
    runs: &mut Vec<Run>,
    attaches: &mut u32,
) -> Result<Arena, TestCaseError> {
    let (a, report) =
        Arena::attach(mem, &agg()).map_err(|(e, _)| TestCaseError::fail(format!("attach: {e}")))?;
    *attaches += 1;
    prop_assert_eq!(report.attach_count, *attaches);
    let slots = a.slots();
    let mut expected: Vec<(u32, u32, u32, u64, u64, u32)> = Vec::new();
    let mut filled = 0u32;
    let mut torn = 0u32;
    for r in runs.iter() {
        match r.st {
            St::Torn => torn += r.blocks,
            St::Placed => {
                for k in 0..r.blocks {
                    expected.push((
                        r.ring.slot + k,
                        r.ring.lap,
                        r.vol,
                        r.lba + u64::from(k),
                        r.seq,
                        r.crcs[k as usize],
                    ));
                }
            }
            St::Filled => filled += r.blocks,
            St::Reserved => {}
        }
    }
    let mut got: Vec<(u32, u32, u32, u64, u64, u32)> = report
        .placed
        .iter()
        .map(|p| (p.ring.slot, p.ring.lap, p.vol, p.lba, p.seq, p.crc32c))
        .collect();
    // The report is in (seq, position) order.
    let keys: Vec<(u64, u64)> = report
        .placed
        .iter()
        .map(|p| {
            (
                p.seq,
                u64::from(p.ring.lap) * u64::from(slots) + u64::from(p.ring.slot),
            )
        })
        .collect();
    let mut sorted = keys.clone();
    sorted.sort_unstable();
    prop_assert_eq!(
        &keys,
        &sorted,
        "placed slots are reported in (seq, position) order"
    );
    expected.sort_unstable();
    got.sort_unstable();
    prop_assert_eq!(&got, &expected, "exactly the placed slots are kept");
    prop_assert_eq!(
        report.dropped_filled,
        filled,
        "every filled slot is dropped"
    );
    prop_assert_eq!(
        report.torn_places,
        torn,
        "the placed prefix of an interrupted place is dropped"
    );
    prop_assert!(report.damaged.is_empty(), "a kill is not damage");
    runs.retain(|r| r.st == St::Placed);
    let (tail, head) = a.window();
    prop_assert!(head - tail <= u64::from(slots), "window {tail}..{head}");
    let max_seq = runs.iter().map(|r| r.seq).max().unwrap_or(0);
    prop_assert!(
        a.peek_seq() > max_seq,
        "next seq {} <= {max_seq}",
        a.peek_seq()
    );
    for r in runs.iter() {
        let pos = r.pos(slots);
        prop_assert!(
            pos >= tail && pos + u64::from(r.blocks) <= head,
            "placed run at {pos} outside the window {tail}..{head}"
        );
    }
    Ok(a)
}

/// The model's invariants; `data` also reads every live run back (done
/// after each re-attach and at the end, to keep the debug build fast).
fn check(a: &Arena, runs: &[Run], data: bool) -> Result<(), TestCaseError> {
    let slots = a.slots();
    prop_assert!(a.occupied() <= slots);
    let (tail, head) = a.window();
    for (i, r) in runs.iter().enumerate() {
        let pos = r.pos(slots);
        prop_assert!(
            pos >= tail && pos + u64::from(r.blocks) <= head,
            "live run {r:?} outside the window {tail}..{head}"
        );
        for other in &runs[i + 1..] {
            let overlap = r.ring.slot < other.ring.slot + other.blocks
                && other.ring.slot < r.ring.slot + r.blocks;
            prop_assert!(!overlap, "two live runs share a slot: {r:?} and {other:?}");
        }
        if data && r.st != St::Reserved {
            let data = a
                .run(r.ring, r.blocks)
                .map_err(|e| TestCaseError::fail(format!("live run unreadable: {e}")))?;
            for k in 0..r.blocks {
                let block = &data[k as usize * SLOT_BYTES..(k as usize + 1) * SLOT_BYTES];
                // The CRC of the pattern written at reserve time: cheap in
                // a debug build, and any other data fails it.
                prop_assert!(
                    crc32c(block) == r.crcs[k as usize],
                    "run {r:?} block {k} holds other data"
                );
            }
        }
    }
    if runs.is_empty() {
        prop_assert_eq!(
            a.occupied(),
            0,
            "nothing is live but the window is not empty"
        );
    }
    Ok(())
}

fn run_ops(ops: &[Op]) -> Result<(), TestCaseError> {
    let mut a = Some(arena());
    let mut runs: Vec<Run> = Vec::new();
    let mut next_tag = 1u64;
    let mut attaches = 1u32;
    for op in ops {
        let arena = a.as_mut().expect("an arena");
        match *op {
            Op::Reserve(n) => match arena.reserve(n) {
                Ok(ring) => {
                    for r in &runs {
                        let overlap =
                            r.ring.slot < ring.slot + n && ring.slot < r.ring.slot + r.blocks;
                        prop_assert!(!overlap, "reserve({n}) = {ring:?} overlaps live {r:?}");
                    }
                    let tag = next_tag;
                    next_tag += 1;
                    let crcs = write_pattern(arena, ring, n, tag, 0);
                    runs.push(Run {
                        ring,
                        blocks: n,
                        st: St::Reserved,
                        tag,
                        k0: 0,
                        vol: 0,
                        lba: 0,
                        seq: 0,
                        crcs,
                    });
                }
                Err(ArenaError::Full { .. }) => {}
                Err(e) => prop_assert!(false, "reserve({n}): {e}"),
            },
            Op::Fill(i) => {
                if let Some(i) = pick(&runs, St::Reserved, i) {
                    fill_run(arena, &mut runs[i])?;
                }
            }
            Op::Place(i) => {
                if let Some(i) = pick(&runs, St::Filled, i) {
                    arena
                        .place(runs[i].ring, runs[i].blocks)
                        .map_err(|e| TestCaseError::fail(format!("place: {e}")))?;
                    runs[i].st = St::Placed;
                }
            }
            Op::Commit(i) => {
                if let Some(i) = pick(&runs, St::Placed, i) {
                    let r = runs.remove(i);
                    arena
                        .commit(r.ring, r.blocks)
                        .map_err(|e| TestCaseError::fail(format!("commit: {e}")))?;
                }
            }
            Op::Release(i, k) => {
                if let Some(i) = pick(&runs, St::Placed, i) {
                    let r = runs.remove(i);
                    let k = k % r.blocks;
                    arena
                        .commit(r.ring.offset(k), 1)
                        .map_err(|e| TestCaseError::fail(format!("release: {e}")))?;
                    if k > 0 {
                        runs.push(r.slice(0, k));
                    }
                    if k + 1 < r.blocks {
                        runs.push(r.slice(k + 1, r.blocks));
                    }
                }
            }
            Op::Discard(i) => {
                if let Some(i) = pick(&runs, St::Filled, i) {
                    let r = runs.remove(i);
                    arena
                        .discard(r.ring, r.blocks)
                        .map_err(|e| TestCaseError::fail(format!("discard: {e}")))?;
                }
            }
            Op::Kill => {
                let mem = a.take().expect("an arena").into_memory();
                a = Some(reattach(mem, &mut runs, &mut attaches)?);
            }
            Op::KillMidFill(i, j) => {
                let Some(i) = pick(&runs, St::Reserved, i) else {
                    continue;
                };
                fill_run(arena, &mut runs[i])?;
                let r = runs[i].clone();
                let j = j % (r.blocks + 1);
                let mut mem = a.take().expect("an arena").into_memory();
                for k in j..r.blocks {
                    set_state(&mut mem, r.ring.slot + k, SlotState::Free);
                }
                // Slots 0..j are filled, the rest still reserved: all of it
                // goes at the re-attach.
                runs.remove(i);
                if j > 0 {
                    runs.push(r.slice(0, j));
                }
                a = Some(reattach(mem, &mut runs, &mut attaches)?);
            }
            Op::KillInsideFill(i, j) => {
                let Some(i) = pick(&runs, St::Reserved, i) else {
                    continue;
                };
                let r0 = runs[i].clone();
                let j = j % r0.blocks;
                // The reservation's descriptors, before the fill changes
                // their fields.
                let mut before = Vec::with_capacity(r0.blocks as usize);
                for k in 0..r0.blocks {
                    before.push(
                        arena
                            .descriptor(r0.ring.slot + k)
                            .map_err(|e| TestCaseError::fail(format!("descriptor: {e}")))?,
                    );
                }
                fill_run(arena, &mut runs[i])?;
                let r = runs.remove(i);
                let mut mem = a.take().expect("an arena").into_memory();
                // Slots 0..j were filled whole; slot j has the fill's fields
                // under the reservation's word; the slots after it are still
                // the reservation's. The write was never acknowledged and
                // goes whole; the stale slot is released when it lies inside
                // the window, so nothing stalls (Step 6A).
                let mut stale = read_desc(&mem, r.ring.slot + j);
                stale.word = before[j as usize].word;
                write_desc(&mut mem, r.ring.slot + j, &stale);
                for k in j + 1..r.blocks {
                    write_desc(&mut mem, r.ring.slot + k, &before[k as usize]);
                }
                if j > 0 {
                    runs.push(r.slice(0, j));
                }
                a = Some(reattach(mem, &mut runs, &mut attaches)?);
            }
            Op::KillMidPlace(i, j) => {
                let Some(i) = pick(&runs, St::Filled, i) else {
                    continue;
                };
                arena
                    .place(runs[i].ring, runs[i].blocks)
                    .map_err(|e| TestCaseError::fail(format!("place: {e}")))?;
                let mut r = runs.remove(i);
                r.st = St::Placed;
                let j = j % (r.blocks + 1);
                let mut mem = a.take().expect("an arena").into_memory();
                for k in j..r.blocks {
                    set_state(&mut mem, r.ring.slot + k, SlotState::Filled);
                }
                if j > 0 {
                    let mut prefix = r.slice(0, j);
                    if j < r.blocks {
                        // A place that did not finish: the write was never
                        // acknowledged and goes whole (Step 6A).
                        prefix.st = St::Torn;
                    }
                    runs.push(prefix);
                }
                if j < r.blocks {
                    let mut rest = r.slice(j, r.blocks);
                    rest.st = St::Filled;
                    runs.push(rest);
                }
                a = Some(reattach(mem, &mut runs, &mut attaches)?);
            }
            Op::KillMidCommit(i, j) => {
                let Some(i) = pick(&runs, St::Placed, i) else {
                    continue;
                };
                arena
                    .commit(runs[i].ring, runs[i].blocks)
                    .map_err(|e| TestCaseError::fail(format!("commit: {e}")))?;
                let r = runs.remove(i);
                let j = j % (r.blocks + 1);
                let mut mem = a.take().expect("an arena").into_memory();
                for k in j..r.blocks {
                    set_state(&mut mem, r.ring.slot + k, SlotState::Placed);
                }
                if j < r.blocks {
                    runs.push(r.slice(j, r.blocks));
                }
                a = Some(reattach(mem, &mut runs, &mut attaches)?);
            }
        }
        let killed = matches!(
            op,
            Op::Kill
                | Op::KillMidFill(..)
                | Op::KillInsideFill(..)
                | Op::KillMidPlace(..)
                | Op::KillMidCommit(..)
        );
        check(a.as_ref().expect("an arena"), &runs, killed)?;
    }
    check(a.as_ref().expect("an arena"), &runs, true)
}

proptest! {
    #![proptest_config(ProptestConfig { failure_persistence: None, ..ProptestConfig::default() })]

    #[test]
    fn reattach_after_any_interleaving_keeps_exactly_the_placed_slots(
        ops in prop::collection::vec(op(), 1..200)
    ) {
        run_ops(&ops)?;
    }
}
