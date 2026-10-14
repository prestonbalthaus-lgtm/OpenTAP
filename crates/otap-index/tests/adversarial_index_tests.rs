//! Adversarial tests for `otap-index` (the Step 3 review, ported).
//!
//! Overlaps in every pattern with equal and unequal sequence labels,
//! coalescing across many small writes, the conservation of arena slots
//! (every slot an insert hands over is either live in the index or returned
//! for release, exactly once), lookups across several frozen generations
//! against a model, the ring limit for every ring size, `retire` out of
//! order, cuts racing with inserts from three writer threads, and readers
//! holding `with_active` across a cut.
//!
//! The review found two ordering defects (a merged extent took the highest
//! sequence of its parts, so a late lower sequence lost blocks it should
//! have won; and the blocks a late insert lost were neither kept nor
//! returned, leaking arena slots). The fix made the contract explicit: the
//! index refuses a sequence that is not above the volume's newest, and the
//! map below it lets the later insert win its whole range. The first two
//! tests pin that.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Barrier};
use std::thread;
use std::time::{Duration, Instant};

use otap_arena::RingRef;
use otap_index::{
    superseded_blocks, DirtyExtent, GenId, Generation, Index, IndexError, VolumeDirty,
};
use proptest::prelude::*;

/// A generation of the lookup model: its identity and `(vol, lba) -> slot`.
type GenModel = (GenId, BTreeMap<(u32, u64), u32>);

fn ring(slot: u32) -> RingRef {
    RingRef { slot, lap: 0 }
}

fn ext(slot: u32, blocks: u32, seq: u64) -> DirtyExtent {
    DirtyExtent {
        ring: ring(slot),
        blocks,
        seq,
    }
}

/// Every slot the map holds, block by block.
fn live_slots(v: &VolumeDirty) -> Vec<(u32, u32)> {
    let mut out = Vec::new();
    for (_, e) in v.iter() {
        for k in 0..e.blocks {
            out.push((e.ring.lap, e.ring.slot + k));
        }
    }
    out
}

/// Every slot a list of superseded runs names, block by block.
fn returned_slots(runs: &[(RingRef, u32)]) -> Vec<(u32, u32)> {
    let mut out = Vec::new();
    for (r, n) in runs {
        for k in 0..*n {
            out.push((r.lap, r.slot + k));
        }
    }
    out
}

/// The extents are sorted, non-empty and never overlap, and `blocks()`
/// counts them.
fn well_formed(v: &VolumeDirty) -> Result<(), String> {
    let mut end = 0u64;
    let mut total = 0u64;
    for (lba, e) in v.iter() {
        if e.blocks == 0 {
            return Err(format!("an empty extent at {lba}"));
        }
        if lba < end {
            return Err(format!(
                "extent at {lba} overlaps the previous one ending at {end}"
            ));
        }
        end = lba + u64::from(e.blocks);
        total += u64::from(e.blocks);
    }
    if total != v.blocks() {
        return Err(format!(
            "blocks() {} but the extents hold {total}",
            v.blocks()
        ));
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Defects
// ---------------------------------------------------------------------------

#[test]
fn a_stale_sequence_is_refused_untouched_even_after_coalescing() {
    // Before the fix a merged extent took the highest sequence of its
    // parts, so the seq 5 block below claimed seq 9 and a later seq 7 lost
    // to it. Now the index never lets a lower sequence in.
    let index = Index::new(4, 0);
    index.insert(0, 0, ext(0, 1, 5)).unwrap();
    index.insert(0, 1, ext(1, 1, 9)).unwrap();
    index.with_active(|g| {
        assert_eq!(
            g.with_volume(0, VolumeDirty::extents),
            Some(1),
            "slots 0 and 1 continue each other: one extent"
        );
    });
    assert_eq!(
        index.insert(0, 0, ext(40, 1, 7)),
        Err(IndexError::StaleSeq {
            vol: 0,
            seq: 7,
            newest: 9
        })
    );
    let hit = index.lookup(0, 0).unwrap();
    assert_eq!((hit.ring, hit.seq), (ring(0), 9));
    assert_eq!(index.unflushed_blocks(), 2);
    assert_eq!(index.newest_seq(0), 9, "a refused insert moves nothing");
    // The next valid sequence wins the block.
    let (_, superseded) = index.insert(0, 0, ext(40, 1, 10)).unwrap();
    assert_eq!(superseded, vec![(ring(0), 1)]);
    assert_eq!(index.lookup(0, 0).unwrap().ring, ring(40));
}

#[test]
fn every_slot_an_insert_hands_over_is_live_or_returned() {
    // Before the fix a late insert lost the blocks a newer extent covered
    // and their slots were neither kept nor returned. The map now lets the
    // later insert win, so nothing it brings can be lost.
    let mut v = VolumeDirty::new();
    assert!(v.insert(10, ext(0, 2, 9)).is_empty());
    let superseded = v.insert(11, ext(50, 2, 3));
    let live = live_slots(&v);
    let returned = returned_slots(&superseded);
    for slot in [0u32, 1, 50, 51] {
        assert!(
            live.contains(&(0, slot)) || returned.contains(&(0, slot)),
            "slot {slot} is neither live ({live:?}) nor returned ({returned:?})"
        );
    }
    assert_eq!(returned, vec![(0, 1)], "the block at LBA 11 was replaced");
    assert_eq!(v.lookup(11), Some((ring(50), 3)));
    assert_eq!(v.lookup(12), Some((ring(51), 3)));
    well_formed(&v).unwrap();
}

// ---------------------------------------------------------------------------
// Behaviour that holds
// ---------------------------------------------------------------------------

#[test]
fn many_small_writes_coalesce_and_slot_gaps_or_laps_split_them() {
    let mut v = VolumeDirty::new();
    for i in 0..10_000u64 {
        assert!(v.insert(i, ext(i as u32, 1, i + 1)).is_empty());
    }
    assert_eq!(v.extents(), 1);
    assert_eq!(v.blocks(), 10_000);
    assert_eq!(v.lookup(9_999), Some((ring(9_999), 10_000)));

    // The same in reverse LBA order: each insert continues its right
    // neighbour.
    let mut v = VolumeDirty::new();
    for i in (0..2_000u64).rev() {
        v.insert(i, ext(i as u32, 1, 2_001 - i));
    }
    assert_eq!(v.extents(), 1);

    // A slot gap every 100 writes: 100 extents.
    let mut v = VolumeDirty::new();
    for i in 0..10_000u64 {
        v.insert(i, ext((i + i / 100) as u32, 1, i + 1));
    }
    assert_eq!(v.extents(), 100);
    well_formed(&v).unwrap();

    // Contiguous slots on different laps never merge.
    let mut v = VolumeDirty::new();
    v.insert(
        0,
        DirtyExtent {
            ring: RingRef { slot: 5, lap: 1 },
            blocks: 1,
            seq: 1,
        },
    );
    v.insert(
        1,
        DirtyExtent {
            ring: RingRef { slot: 6, lap: 2 },
            blocks: 1,
            seq: 2,
        },
    );
    assert_eq!(v.extents(), 2);

    // Interleaved volumes: one volume's slots are not contiguous, so
    // nothing merges inside either volume.
    let index = Index::new(8, 0);
    for i in 0..200u64 {
        let slot = (i * 2) as u32;
        index.insert(0, i, ext(slot, 1, i * 2 + 1)).unwrap();
        index.insert(1, i, ext(slot + 1, 1, i * 2 + 2)).unwrap();
    }
    index.with_active(|g| {
        assert_eq!(g.with_volume(0, VolumeDirty::extents), Some(200));
        assert_eq!(g.with_volume(1, VolumeDirty::extents), Some(200));
        assert_eq!(g.blocks(), 400);
    });
}

#[test]
fn every_overlap_pattern_lets_the_later_insert_win_whatever_its_label() {
    // An old extent [10, 20) of seq `old` and a new one at every start and
    // length that touches it, labelled below, equal to and above it: the
    // later insert wins its range and returns exactly the old blocks it
    // covered.
    for start in 5u64..=21 {
        for len in 1u32..=17 {
            for (old_seq, new_seq) in [(5u64, 4u64), (5, 5), (5, 6)] {
                let mut v = VolumeDirty::new();
                v.insert(10, ext(100, 10, old_seq));
                let sup = v.insert(start, ext(200, len, new_seq));
                well_formed(&v)
                    .map_err(|e| format!("{start}+{len}: {e}"))
                    .unwrap();
                for lba in 0..40u64 {
                    let in_old = (10..20).contains(&lba);
                    let in_new = lba >= start && lba < start + u64::from(len);
                    let expected = match (in_old, in_new) {
                        (_, true) => Some((ring(200 + (lba - start) as u32), new_seq)),
                        (true, false) => Some((ring(100 + (lba - 10) as u32), old_seq)),
                        (false, false) => None,
                    };
                    assert_eq!(
                        v.lookup(lba),
                        expected,
                        "old [10,20) seq {old_seq}, new [{start},+{len}) seq {new_seq}, LBA {lba}"
                    );
                }
                let overlap: Vec<(u32, u32)> = (10u64..20)
                    .filter(|l| *l >= start && *l < start + u64::from(len))
                    .map(|l| (0, 100 + (l - 10) as u32))
                    .collect();
                let mut returned = returned_slots(&sup);
                returned.sort_unstable();
                assert_eq!(
                    returned, overlap,
                    "{start}+{len}: the overlapped old blocks"
                );
            }
        }
    }
}

#[test]
fn retire_refuses_anything_but_the_oldest_frozen_generation() {
    let index = Index::new(8, 0);
    let g1 = index.cut(1).unwrap();
    let g2 = index.cut(2).unwrap();
    let g3 = index.cut(3).unwrap();
    let active = index.active_id();
    assert_eq!((g1.id(), g2.id(), g3.id(), active), (1, 2, 3, 4));
    for wrong in [g2.id(), g3.id(), active, 0, 99] {
        assert_eq!(
            index.retire(wrong),
            Err(IndexError::NotOldest { gen: wrong })
        );
    }
    assert_eq!(index.frozen_len(), 3, "a refused retire changes nothing");
    index.retire(g1.id()).unwrap();
    assert_eq!(index.retire(g1.id()), Err(IndexError::NotOldest { gen: 1 }));
    index.retire(g2.id()).unwrap();
    index.retire(g3.id()).unwrap();
    assert_eq!(
        index.retire(active),
        Err(IndexError::NotOldest { gen: active })
    );
    assert_eq!(index.frozen_len(), 0);
}

#[test]
fn the_ring_limit_holds_for_every_ring_size() {
    for asked in 0usize..=10 {
        let index = Index::new(asked, 0);
        let ring = asked.max(2);
        assert_eq!(index.ring(), ring);
        for i in 0..ring - 1 {
            assert_eq!(index.cut_within(), Ok(()), "ring {ring}, cut {i}");
            index.cut(i as u64).unwrap();
        }
        let full = Err(IndexError::RingFull { frozen: ring - 1 });
        assert_eq!(index.cut_within(), full);
        assert_eq!(
            index.cut(99).map(|g| g.id()),
            Err(IndexError::RingFull { frozen: ring - 1 })
        );
        assert_eq!(
            index.frozen_len(),
            ring - 1,
            "a refused cut freezes nothing"
        );
        let oldest = index.oldest_frozen().unwrap();
        index.retire(oldest.id()).unwrap();
        assert_eq!(index.cut_within(), Ok(()));
        assert!(index.cut(100).is_ok());
    }
}

#[test]
fn bad_extents_and_volumes_are_refused_without_touching_the_index() {
    let index = Index::new(8, 0);
    let max = otap_format::MAX_LBA;
    for (vol, lba, blocks) in [
        (0u32, 0u64, 0u32),
        (0, max, 2),
        (0, max + 1, 1),
        (0, u64::MAX, 1),
        (0, u64::MAX - 1, 4),
        (1024, 0, 1),
        (u32::MAX, 0, 1),
    ] {
        let got = index.insert(vol, lba, ext(0, blocks, 1));
        assert!(got.is_err(), "{vol}/{lba}+{blocks} accepted");
    }
    assert!(
        index.insert(0, max, ext(0, 1, 1)).is_ok(),
        "the last LBA is fine"
    );
    assert_eq!(index.unflushed_blocks(), 1);
    assert_eq!(index.lookup(1024, 0), None);
    assert_eq!(index.lookup(u32::MAX, 0), None);
}

#[test]
fn a_reader_holding_with_active_holds_the_cut_for_as_long_as_it_holds() {
    let index = Arc::new(Index::new(4, 0));
    let entered = Arc::new(Barrier::new(2));
    let reader = {
        let index = Arc::clone(&index);
        let entered = Arc::clone(&entered);
        thread::spawn(move || {
            index.with_active(|g| {
                entered.wait();
                thread::sleep(Duration::from_millis(300));
                g.id()
            })
        })
    };
    entered.wait();
    let started = Instant::now();
    let frozen = index.cut(1).unwrap();
    let waited = started.elapsed();
    // Not only inserts in progress (the crate docs' wording): any guard of
    // the old generation, readers included, holds the cut, for as long as
    // it is held. A cut issued from inside `with_active` would wait for
    // itself for ever.
    assert!(
        waited >= Duration::from_millis(250),
        "the cut returned after {waited:?}"
    );
    assert_eq!(frozen.id(), 1);
    assert_eq!(reader.join().unwrap(), 1);
}

#[test]
fn a_stream_of_short_readers_never_starves_the_cut() {
    let index = Arc::new(Index::new(8, 0));
    let stop = Arc::new(AtomicBool::new(false));
    let readers: Vec<_> = (0..2)
        .map(|_| {
            let index = Arc::clone(&index);
            let stop = Arc::clone(&stop);
            thread::spawn(move || {
                let mut seen = 0u64;
                while !stop.load(Ordering::Relaxed) {
                    seen = seen.max(index.with_active(Generation::id));
                    let _ = index.lookup(0, 0);
                }
                seen
            })
        })
        .collect();
    let mut slowest = Duration::ZERO;
    for i in 0..200u64 {
        let started = Instant::now();
        let gen = index.cut(i).unwrap();
        slowest = slowest.max(started.elapsed());
        index.retire(gen.id()).unwrap();
    }
    stop.store(true, Ordering::Relaxed);
    let seen: Vec<u64> = readers.into_iter().map(|h| h.join().unwrap()).collect();
    assert!(
        slowest < Duration::from_millis(500),
        "a cut waited {slowest:?} behind short readers"
    );
    assert!(seen.iter().all(|s| *s >= 1));
}

#[test]
fn concurrent_inserts_land_in_exactly_the_generation_they_report() {
    const WRITERS: u32 = 3;
    const PER: u64 = 20_000;
    let index = Arc::new(Index::new(8, 0));
    let start = Arc::new(Barrier::new(WRITERS as usize + 1));
    let done = Arc::new(AtomicUsize::new(0));
    let writers: Vec<_> = (0..WRITERS)
        .map(|w| {
            let index = Arc::clone(&index);
            let start = Arc::clone(&start);
            let done = Arc::clone(&done);
            thread::spawn(move || {
                start.wait();
                // (generation, first slot, superseded runs) per insert. Even
                // inserts write LBA i alone; odd inserts write LBAs i - 1
                // and i, so they rewrite the even block before them and
                // supersession races the cut too. Every insert owns fresh
                // slots, as an arena run would.
                let mut reported = Vec::with_capacity(PER as usize);
                let mut next_slot = w * 3 * PER as u32;
                for i in 0..PER {
                    let (lba, blocks) = if i % 2 == 1 { (i - 1, 2) } else { (i, 1) };
                    let slot = next_slot;
                    next_slot += blocks;
                    let (gen, superseded) = index
                        .insert(w, lba, ext(slot, blocks, i + 1))
                        .expect("a valid insert");
                    reported.push((gen, slot, superseded));
                }
                done.fetch_add(1, Ordering::SeqCst);
                reported
            })
        })
        .collect();
    start.wait();
    // The cutter freezes as fast as it can, retires all but a few, and
    // records what each generation holds when it retires it (after the
    // grace period, so no insert can still be landing in it).
    let mut contents: HashMap<(GenId, u32, u64), u32> = HashMap::new();
    let mut total_blocks = 0u64;
    let mut record = |gen: &Arc<Generation>, total: &mut u64| {
        *total += gen.blocks();
        gen.for_each_dirty_volume(|vol, dirty| {
            for (lba, e) in dirty.iter() {
                for k in 0..e.blocks {
                    let fresh = contents
                        .insert((gen.id(), vol, lba + u64::from(k)), e.ring.slot + k)
                        .is_none();
                    assert!(fresh, "a block appears twice in one generation");
                }
            }
        });
    };
    let mut cuts = 0u64;
    while done.load(Ordering::SeqCst) < WRITERS as usize {
        if index.cut(cuts).is_ok() {
            cuts += 1;
        }
        while index.frozen_len() > 3 {
            let oldest = index.oldest_frozen().unwrap();
            record(&oldest, &mut total_blocks);
            index.retire(oldest.id()).unwrap();
        }
        thread::yield_now();
    }
    type Reported = Vec<(GenId, u32, Vec<(RingRef, u32)>)>;
    let reported: Vec<Reported> = writers.into_iter().map(|h| h.join().unwrap()).collect();
    index.cut(cuts + 1).unwrap();
    while let Some(oldest) = index.oldest_frozen() {
        record(&oldest, &mut total_blocks);
        index.retire(oldest.id()).unwrap();
    }
    assert!(cuts >= 5, "the cutter raced the writers ({cuts} cuts)");
    let mut expected_blocks = 0u64;
    for (w, list) in reported.iter().enumerate() {
        let w = w as u32;
        let mut last_gen = 0;
        for (i, (gen, slot, superseded)) in list.iter().enumerate() {
            let (i, gen, slot) = (i as u64, *gen, *slot);
            assert!(gen >= last_gen, "writer {w}: generations went backwards");
            last_gen = gen;
            if i % 2 == 0 {
                // The even block lives on in its own generation only if the
                // odd insert after it landed in a later one.
                let next_gen = list[i as usize + 1].0;
                if next_gen != gen {
                    expected_blocks += 1;
                    assert_eq!(
                        contents.get(&(gen, w, i)),
                        Some(&slot),
                        "writer {w} insert {i}: not in the generation {gen} it reported"
                    );
                }
            } else {
                expected_blocks += 2;
                assert_eq!(
                    contents.get(&(gen, w, i - 1)),
                    Some(&slot),
                    "writer {w} insert {i}: not in the generation {gen} it reported"
                );
                assert_eq!(contents.get(&(gen, w, i)), Some(&(slot + 1)));
                let (prev_gen, prev_slot, _) = list[i as usize - 1];
                let expected: Vec<(RingRef, u32)> = if prev_gen == gen {
                    vec![(ring(prev_slot), 1)]
                } else {
                    Vec::new()
                };
                assert_eq!(
                    superseded, &expected,
                    "writer {w} insert {i}: generation {gen}, previous in {prev_gen}"
                );
            }
        }
    }
    assert_eq!(total_blocks, expected_blocks, "no block lost or duplicated");
}

// ---------------------------------------------------------------------------
// Properties
// ---------------------------------------------------------------------------

proptest! {
    #![proptest_config(ProptestConfig { failure_persistence: None, ..ProptestConfig::default() })]

    /// Writes with any sequence labels, ties included, with a gap between
    /// the slot runs of different writes (so nothing merges across writes):
    /// the map applies them in insertion order, agrees with a per-block
    /// model and returns exactly the older blocks it replaced.
    #[test]
    fn any_sequence_labels_agree_with_a_block_model_in_insertion_order(
        writes in prop::collection::vec((0u64..48, 1u32..8, 0u64..12), 1..80)
    ) {
        let mut v = VolumeDirty::new();
        let mut model: BTreeMap<u64, (u32, u64)> = BTreeMap::new();
        let mut next_slot = 0u32;
        for (lba, blocks, seq) in writes {
            let slot = next_slot;
            next_slot += blocks + 1;
            let sup = v.insert(lba, ext(slot, blocks, seq));
            let mut displaced = BTreeSet::new();
            for k in 0..blocks {
                let b = lba + u64::from(k);
                if let Some((old_slot, _)) = model.insert(b, (slot + k, seq)) {
                    displaced.insert(old_slot);
                }
            }
            let returned: Vec<u32> = returned_slots(&sup).into_iter().map(|(_, s)| s).collect();
            let unique: BTreeSet<u32> = returned.iter().copied().collect();
            prop_assert_eq!(unique.len(), returned.len(), "a slot returned twice");
            prop_assert_eq!(&unique, &displaced);
            prop_assert_eq!(superseded_blocks(&sup), displaced.len() as u64);
            if let Err(e) = well_formed(&v) {
                prop_assert!(false, "{}", e);
            }
            for b in 0..64u64 {
                prop_assert_eq!(v.lookup(b), model.get(&b).map(|&(s, q)| (ring(s), q)), "LBA {}", b);
            }
        }
    }

    /// The engine's pattern: sequence numbers in order and each write's
    /// slots right after the previous write's, so writes coalesce. Every
    /// slot ever handed over is live or returned, never both, never twice.
    #[test]
    fn engine_order_conserves_every_slot(
        writes in prop::collection::vec((0u64..64, 1u32..10), 1..150)
    ) {
        let mut v = VolumeDirty::new();
        let mut model: BTreeMap<u64, u32> = BTreeMap::new();
        let mut returned_all: BTreeSet<(u32, u32)> = BTreeSet::new();
        let mut next_slot = 0u32;
        for (i, (lba, blocks)) in writes.into_iter().enumerate() {
            let slot = next_slot;
            next_slot += blocks;
            let sup = v.insert(lba, ext(slot, blocks, i as u64 + 1));
            for s in returned_slots(&sup) {
                prop_assert!(returned_all.insert(s), "slot {:?} returned twice", s);
            }
            for k in 0..blocks {
                model.insert(lba + u64::from(k), slot + k);
            }
            let live: BTreeSet<(u32, u32)> = live_slots(&v).into_iter().collect();
            prop_assert!(live.is_disjoint(&returned_all), "a returned slot is still live");
            prop_assert_eq!(live.len() + returned_all.len(), next_slot as usize, "a slot leaked");
            for b in 0..80u64 {
                prop_assert_eq!(v.lookup(b).map(|(r, _)| r.slot), model.get(&b).copied(), "LBA {}", b);
            }
            if let Err(e) = well_formed(&v) {
                prop_assert!(false, "{}", e);
            }
        }
    }

    /// Inserts, cuts and retires over several volumes: a lookup answers from
    /// the active generation first, then the frozen ones newest first, with
    /// the generation named; the ring limit and every count agree.
    #[test]
    fn lookups_across_frozen_generations_agree_with_a_model(
        ring_size in 2usize..=8,
        ops in prop::collection::vec(
            prop_oneof![
                6 => (0u32..3, 0u64..24, 1u32..5).prop_map(|(v, l, b)| (0u8, v, l, b)),
                2 => Just((1u8, 0u32, 0u64, 0u32)),
                2 => any::<bool>().prop_map(|right| (2u8, u32::from(right), 0u64, 0u32)),
            ],
            1..120,
        )
    ) {
        let index = Index::new(ring_size, 0);
        // Generations, oldest first; the last is active.
        let mut gens: Vec<GenModel> = vec![(1, BTreeMap::new())];
        let mut next_id: GenId = 2;
        let mut next_slot = 0u32;
        let mut seq = 0u64;
        for (kind, vol, lba, blocks) in ops {
            match kind {
                0 => {
                    seq += 1;
                    let slot = next_slot;
                    next_slot += blocks;
                    let (gen, sup) = index.insert(vol, lba, ext(slot, blocks, seq)).unwrap();
                    let active = gens.last_mut().unwrap();
                    prop_assert_eq!(gen, active.0);
                    let mut displaced = BTreeSet::new();
                    for k in 0..blocks {
                        if let Some(old) = active.1.insert((vol, lba + u64::from(k)), slot + k) {
                            displaced.insert(old);
                        }
                    }
                    let returned: BTreeSet<u32> =
                        returned_slots(&sup).into_iter().map(|(_, s)| s).collect();
                    prop_assert_eq!(returned, displaced, "only the active generation supersedes");
                }
                1 => {
                    let frozen = gens.len() - 1;
                    let refused = frozen + 1 >= ring_size;
                    prop_assert_eq!(index.cut_within().is_err(), refused);
                    match index.cut(seq) {
                        Ok(g) => {
                            prop_assert!(!refused);
                            prop_assert_eq!(g.id(), gens.last().unwrap().0);
                            gens.push((next_id, BTreeMap::new()));
                            next_id += 1;
                        }
                        Err(IndexError::RingFull { frozen: f }) => {
                            prop_assert!(refused);
                            prop_assert_eq!(f, frozen);
                        }
                        Err(e) => prop_assert!(false, "{e}"),
                    }
                }
                _ => {
                    if gens.len() > 1 {
                        if vol == 1 {
                            let oldest = gens[0].0;
                            index.retire(oldest).unwrap();
                            gens.remove(0);
                        } else {
                            // The active generation, or one frozen but not
                            // the oldest, is refused.
                            let wrong = gens.last().unwrap().0;
                            prop_assert_eq!(index.retire(wrong), Err(IndexError::NotOldest { gen: wrong }));
                        }
                    } else {
                        let id = gens[0].0;
                        prop_assert_eq!(index.retire(id), Err(IndexError::NotOldest { gen: id }));
                    }
                }
            }
            prop_assert_eq!(index.frozen_len(), gens.len() - 1);
            prop_assert_eq!(index.active_id(), gens.last().unwrap().0);
            let total: usize = gens.iter().map(|(_, m)| m.len()).sum();
            prop_assert_eq!(index.unflushed_blocks(), total as u64);
            prop_assert_eq!(index.active_blocks(), gens.last().unwrap().1.len() as u64);
            for vol in 0..3u32 {
                for lba in 0..30u64 {
                    let expected = gens
                        .iter()
                        .rev()
                        .find_map(|(id, m)| m.get(&(vol, lba)).map(|s| (*id, *s)));
                    let got = index.lookup(vol, lba).map(|h| (h.gen, h.ring.slot));
                    prop_assert_eq!(got, expected, "volume {} LBA {}", vol, lba);
                }
            }
        }
    }
}
