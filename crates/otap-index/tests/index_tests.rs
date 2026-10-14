//! The dirty index: extents, generations, the cut under concurrency.

use std::collections::HashMap;
use std::sync::{Arc, Barrier};
use std::thread;
use std::time::{Duration, Instant};

use otap_arena::RingRef;
use otap_index::{superseded_blocks, DirtyExtent, Index, IndexError, VolumeDirty};
use proptest::prelude::*;

fn ext(slot: u32, blocks: u32, seq: u64) -> DirtyExtent {
    DirtyExtent {
        ring: RingRef { slot, lap: 0 },
        blocks,
        seq,
        hole: false,
    }
}

fn extents(v: &VolumeDirty) -> Vec<(u64, u32, u32, u64)> {
    v.iter()
        .map(|(lba, e)| (lba, e.ring.slot, e.blocks, e.seq))
        .collect()
}

#[test]
fn inserts_split_trim_and_coalesce() {
    let mut v = VolumeDirty::new();
    assert_eq!(superseded_blocks(&v.insert(0, ext(0, 4, 1))), 0);
    assert_eq!(
        superseded_blocks(&v.insert(2, ext(10, 1, 2))),
        1,
        "one block superseded"
    );
    assert_eq!(extents(&v), vec![(0, 0, 2, 1), (2, 10, 1, 2), (3, 3, 1, 1)]);
    assert_eq!(v.blocks(), 4);
    assert_eq!(v.lookup(1), Some((RingRef { slot: 1, lap: 0 }, 1)));
    assert_eq!(v.lookup(2), Some((RingRef { slot: 10, lap: 0 }, 2)));
    assert_eq!(v.lookup(4), None);
    // Slots 4.. continue the run of the extent at LBA 3 (slot 3): merge.
    assert_eq!(superseded_blocks(&v.insert(4, ext(4, 2, 3))), 0);
    assert_eq!(extents(&v), vec![(0, 0, 2, 1), (2, 10, 1, 2), (3, 3, 3, 3)]);
    assert_eq!(v.blocks(), 6);
    assert_eq!(v.extents(), 3);
    // An extent covering everything replaces everything.
    assert_eq!(superseded_blocks(&v.insert(0, ext(100, 8, 4))), 6);
    assert_eq!(extents(&v), vec![(0, 100, 8, 4)]);
    // A run split in two by a later write.
    assert_eq!(superseded_blocks(&v.insert(3, ext(200, 2, 5))), 2);
    assert_eq!(
        extents(&v),
        vec![(0, 100, 3, 4), (3, 200, 2, 5), (5, 105, 3, 4)]
    );
}

#[test]
fn a_stale_sequence_is_refused_by_the_index_and_a_later_insert_wins_in_the_map() {
    // The index keeps insertion order and sequence order one order: a
    // sequence not above the volume's newest is refused untouched.
    let index = Index::new(4, 0);
    index.insert(0, 0, ext(0, 1, 5)).unwrap();
    index.insert(0, 1, ext(1, 1, 9)).unwrap();
    assert_eq!(index.newest_seq(0), 9);
    assert_eq!(
        index.insert(0, 0, ext(40, 1, 7)),
        Err(IndexError::StaleSeq {
            vol: 0,
            seq: 7,
            newest: 9
        })
    );
    assert_eq!(index.lookup(0, 0).unwrap().ring.slot, 0);
    assert_eq!(index.unflushed_blocks(), 2);
    // Equal is another piece of the same write (a run split by a later
    // overwrite comes back from a re-attach in pieces).
    assert!(index.insert(0, 5, ext(40, 1, 9)).is_ok());
    assert_eq!(index.unflushed_blocks(), 3);
    // Another volume has its own order.
    index.insert(1, 0, ext(60, 1, 1)).unwrap();
    assert_eq!(index.newest_seq(1), 1);
    // The map below the index trusts the order it is called in: the later
    // insert wins its whole range whatever its label says.
    let mut v = VolumeDirty::new();
    v.insert(5, ext(50, 1, 9));
    assert_eq!(
        superseded_blocks(&v.insert(4, ext(20, 4, 3))),
        1,
        "the later insert replaced the block at LBA 5"
    );
    assert_eq!(extents(&v), vec![(4, 20, 4, 3)]);
    assert_eq!(v.lookup(5), Some((RingRef { slot: 21, lap: 0 }, 3)));
    assert_eq!(v.blocks(), 4);
}

#[test]
fn lookup_prefers_the_active_generation_then_the_newest_frozen() {
    let index = Index::new(4, 0);
    assert_eq!(index.active_id(), 1);
    assert_eq!(index.insert(0, 1, ext(1, 1, 1)).unwrap().0, 1);
    let frozen = index.cut(10).unwrap();
    assert_eq!(frozen.id(), 1);
    assert_eq!(frozen.cut_ns(), 10);
    assert_eq!(index.active_id(), 2);
    assert_eq!(index.insert(0, 1, ext(2, 1, 2)).unwrap().0, 2);
    let hit = index.lookup(0, 1).unwrap();
    assert_eq!((hit.ring.slot, hit.seq, hit.gen), (2, 2, 2));
    index.cut(20).unwrap();
    assert_eq!(
        index.lookup(0, 1).unwrap().gen,
        2,
        "the newest frozen generation"
    );
    assert_eq!(index.frozen_len(), 2);
    assert_eq!(index.unflushed_blocks(), 2);
    assert_eq!(index.retire(2), Err(IndexError::NotOldest { gen: 2 }));
    index.retire(1).unwrap();
    assert_eq!(index.lookup(0, 1).unwrap().gen, 2);
    index.retire(2).unwrap();
    assert_eq!(index.lookup(0, 1), None);
    assert!(index.lookup(5000, 1).is_none());
    assert!(matches!(
        index.insert(0, 1, ext(1, 0, 1)),
        Err(IndexError::BadExtent { .. })
    ));
    assert!(matches!(
        index.insert(4096, 1, ext(1, 1, 1)),
        Err(IndexError::BadVol { vol: 4096 })
    ));
}

#[test]
fn the_ring_refuses_a_cut_when_full() {
    let index = Index::new(3, 0);
    index.cut(1).unwrap();
    index.cut(2).unwrap();
    assert!(matches!(
        index.cut(3),
        Err(IndexError::RingFull { frozen: 2 })
    ));
    assert_eq!(index.cut_within(), Err(IndexError::RingFull { frozen: 2 }));
    index.retire(1).unwrap();
    assert_eq!(index.cut_within(), Ok(()));
    assert_eq!(index.cut(4).unwrap().id(), 3);
}

#[test]
fn a_cut_waits_for_a_reader_that_saw_the_old_generation() {
    let index = Arc::new(Index::new(4, 0));
    let barrier = Arc::new(Barrier::new(2));
    let reader = {
        let index = Arc::clone(&index);
        let barrier = Arc::clone(&barrier);
        thread::spawn(move || {
            index.with_active(|g| {
                barrier.wait();
                thread::sleep(Duration::from_millis(80));
                g.id()
            })
        })
    };
    barrier.wait();
    let started = Instant::now();
    let frozen = index.cut(1).unwrap();
    let waited = started.elapsed();
    assert_eq!(frozen.id(), 1);
    assert!(
        waited >= Duration::from_millis(60),
        "the cut returned after {waited:?}, before the reader let go"
    );
    assert_eq!(reader.join().unwrap(), 1);
    assert!(index.grace_spins() > 0);
}

#[test]
fn concurrent_inserts_are_never_lost_across_cuts() {
    const WRITERS: u32 = 3;
    const INSERTS: u32 = 3000;
    let index = Arc::new(Index::new(8, 0));
    let mut handles = Vec::new();
    for w in 0..WRITERS {
        let index = Arc::clone(&index);
        handles.push(thread::spawn(move || {
            let mut x = u64::from(w) * 7919 + 1;
            for i in 0..INSERTS {
                x ^= x << 13;
                x ^= x >> 7;
                x ^= x << 17;
                let lba = x % 64;
                let blocks = (x >> 8) % 3 + 1;
                index
                    .insert(
                        w,
                        lba,
                        DirtyExtent {
                            ring: RingRef {
                                slot: i * 4,
                                lap: 0,
                            },
                            blocks: blocks as u32,
                            seq: u64::from(i) + 1,
                            hole: false,
                        },
                    )
                    .unwrap();
            }
        }));
    }
    let mut kept = Vec::new();
    let mut cuts = 0u32;
    while handles.iter().any(|h| !h.is_finished()) {
        if let Ok(gen) = index.cut(cuts.into()) {
            cuts += 1;
            index.retire(gen.id()).unwrap();
            kept.push(gen);
        }
        thread::yield_now();
    }
    for h in handles {
        h.join().unwrap();
    }
    let last = index.cut(u64::from(cuts) + 1).unwrap();
    index.retire(last.id()).unwrap();
    kept.push(last);
    assert!(cuts > 0, "the cutter ran while writers inserted");
    // Every writer's inserts are in order per volume, so the newest
    // generation holding an LBA must hold that volume's highest sequence
    // that covered it, and the total of inserts must match.
    let total: u64 = kept.iter().map(|g| g.inserts()).sum();
    assert_eq!(total, u64::from(WRITERS * INSERTS));
    for w in 0..WRITERS {
        let mut x = u64::from(w) * 7919 + 1;
        let mut newest: HashMap<u64, u64> = HashMap::new();
        for i in 0..INSERTS {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            let lba = x % 64;
            let blocks = (x >> 8) % 3 + 1;
            for k in 0..blocks {
                newest.insert(lba + k, u64::from(i) + 1);
            }
        }
        for (lba, seq) in newest {
            let found = kept
                .iter()
                .rev()
                .find_map(|g| g.with_volume(w, |v| v.lookup(lba)).flatten());
            let (_, got) = found.unwrap_or_else(|| panic!("volume {w} LBA {lba} vanished"));
            assert_eq!(got, seq, "volume {w} LBA {lba}");
        }
    }
}

proptest! {
    #![proptest_config(ProptestConfig { failure_persistence: None, ..ProptestConfig::default() })]

    #[test]
    fn a_volume_map_agrees_with_a_block_model(
        inserts in prop::collection::vec((0u64..32, 1u32..=5, 0u32..1000), 1..60)
    ) {
        let mut v = VolumeDirty::new();
        let mut model: HashMap<u64, (u32, u64)> = HashMap::new();
        for (i, (lba, blocks, slot)) in inserts.into_iter().enumerate() {
            let seq = i as u64 + 1;
            v.insert(lba, ext(slot, blocks, seq));
            for k in 0..blocks {
                model.insert(lba + u64::from(k), (slot + k, seq));
            }
        }
        for lba in 0..40u64 {
            let got = v.lookup(lba);
            let want = model.get(&lba).copied();
            prop_assert_eq!(got.map(|(r, _)| r.slot), want.map(|(slot, _)| slot), "LBA {}", lba);
            // A merged extent carries the highest sequence of its parts, so
            // the sequence reported is at least the block's own.
            if let (Some((_, got_seq)), Some((_, want_seq))) = (got, want) {
                prop_assert!(got_seq >= want_seq, "LBA {}: seq {} below {}", lba, got_seq, want_seq);
            }
        }
        prop_assert_eq!(v.blocks() as usize, model.len(), "block count");
        let list = extents(&v);
        for pair in list.windows(2) {
            let (a_lba, a_slot, a_blocks, _) = pair[0];
            let (b_lba, b_slot, _, _) = pair[1];
            prop_assert!(a_lba + u64::from(a_blocks) <= b_lba, "extents overlap");
            let mergeable = a_lba + u64::from(a_blocks) == b_lba && a_slot + a_blocks == b_slot;
            prop_assert!(!mergeable, "adjacent extents with a continuous run were not merged");
        }
    }
}

// ---------------------------------------------------------------------------
// holes (Step 4, E23): discards as extents without data
// ---------------------------------------------------------------------------

fn hole(record: u32, blocks: u32, seq: u64) -> DirtyExtent {
    DirtyExtent::hole(
        RingRef {
            slot: record,
            lap: 0,
        },
        blocks,
        seq,
    )
}

#[test]
fn holes_split_merge_and_supersede_like_data_but_release_nothing() {
    let mut v = VolumeDirty::new();
    assert_eq!(superseded_blocks(&v.insert(0, ext(0, 8, 1))), 0);
    // A hole over the middle supersedes four data blocks and splits the run.
    let freed = v.insert(2, hole(100, 4, 2));
    assert_eq!(superseded_blocks(&freed), 4);
    assert_eq!(freed, vec![(RingRef { slot: 2, lap: 0 }, 4)]);
    assert_eq!(
        extents(&v),
        vec![(0, 0, 2, 1), (2, 100, 4, 2), (6, 6, 2, 1)]
    );
    assert_eq!(v.blocks(), 8);
    assert_eq!(v.data_blocks(), 4);
    let (k, e) = v.find(4).unwrap();
    assert_eq!((k, e.hole, e.blocks), (2, true, 4));
    assert_eq!(
        v.lookup(5),
        Some((RingRef { slot: 100, lap: 0 }, 2)),
        "a hole answers with its record slot, never offset"
    );
    // A write into the hole splits it; the pieces keep the record slot and
    // nothing is released for the hole blocks it covered.
    let freed = v.insert(3, ext(50, 2, 3));
    assert!(freed.is_empty(), "a superseded hole releases no slot");
    assert_eq!(
        extents(&v),
        vec![
            (0, 0, 2, 1),
            (2, 100, 1, 2),
            (3, 50, 2, 3),
            (5, 100, 1, 2),
            (6, 6, 2, 1)
        ]
    );
    // Overwriting the write with another hole of the same record merges the
    // three hole pieces back into one.
    let freed = v.insert(3, hole(100, 2, 4));
    assert_eq!(superseded_blocks(&freed), 2);
    assert_eq!(
        extents(&v),
        vec![(0, 0, 2, 1), (2, 100, 4, 4), (6, 6, 2, 1)]
    );
    // Holes of different records stay apart; a hole never merges with data.
    let mut v = VolumeDirty::new();
    v.insert(0, hole(1, 2, 1));
    v.insert(2, hole(2, 2, 2));
    v.insert(4, ext(4, 2, 3));
    assert_eq!(v.extents(), 3);
    well_formed_holes(&v);
}

fn well_formed_holes(v: &VolumeDirty) {
    let mut end = 0u64;
    for (lba, e) in v.iter() {
        assert!(lba >= end, "extents overlap");
        end = lba + u64::from(e.blocks);
        if e.hole {
            for k in 0..e.blocks {
                assert_eq!(v.lookup(lba + u64::from(k)), Some((e.ring, e.seq)));
            }
        }
    }
}

#[test]
fn the_index_reports_holes_in_hits_and_counts_them_as_dirty() {
    let index = Index::new(4, 0);
    index.insert(0, 10, ext(0, 4, 1)).unwrap();
    let (_, freed) = index.insert(0, 12, hole(9, 8, 2)).unwrap();
    assert_eq!(superseded_blocks(&freed), 2);
    let hit = index.lookup(0, 15).unwrap();
    assert!(hit.hole);
    assert_eq!(hit.ring, RingRef { slot: 9, lap: 0 });
    assert_eq!(hit.seq, 2);
    let data = index.lookup(0, 11).unwrap();
    assert!(!data.hole);
    assert_eq!(data.ring, RingRef { slot: 1, lap: 0 });
    assert_eq!(index.active_blocks(), 10, "holes count as dirty blocks");
    assert!(index.lookup(0, 20).is_none());
    // A hole is frozen with its generation and found through the frozen
    // queue like any extent.
    let gen = index.cut(1).unwrap();
    assert!(index
        .lookup(0, 15)
        .is_some_and(|h| h.hole && h.gen == gen.id()));
    let mut holes = 0;
    gen.for_each_dirty_volume(|_, d| {
        holes += d.iter().filter(|(_, e)| e.hole).count();
    });
    assert_eq!(holes, 1);
}

proptest! {
    #![proptest_config(ProptestConfig {
        failure_persistence: None,
        ..ProptestConfig::default()
    })]

    /// Random data writes and discards against a per-block model: the
    /// newest insert wins every block, holes answer with their record slot,
    /// superseded runs are exactly the data blocks replaced.
    #[test]
    fn holes_and_data_agree_with_a_block_model(
        ops in prop::collection::vec((0u64..32, 1u32..=6, any::<bool>()), 1..60)
    ) {
        let mut v = VolumeDirty::new();
        let mut model: Vec<Option<(u64, bool, u32)>> = vec![None; 40];
        let mut next_slot = 0u32;
        for (seq0, (lba, blocks, is_hole)) in ops.into_iter().enumerate() {
            let seq = seq0 as u64 + 1;
            let ext = if is_hole {
                let r = next_slot;
                next_slot += 1;
                hole(r, blocks, seq)
            } else {
                let r = next_slot;
                next_slot += blocks;
                ext(r, blocks, seq)
            };
            let freed = v.insert(lba, ext);
            let mut expected_freed = 0u64;
            for k in 0..blocks {
                let at = (lba + u64::from(k)) as usize;
                if let Some((_, false, _)) = model[at] {
                    expected_freed += 1;
                }
                model[at] = Some((seq, is_hole, ext.ring.slot + if is_hole { 0 } else { k }));
            }
            prop_assert_eq!(superseded_blocks(&freed), expected_freed);
            for (at, m) in model.iter().enumerate() {
                let got = v.lookup(at as u64);
                match m {
                    None => prop_assert!(got.is_none(), "LBA {} should be clean", at),
                    Some((s, h, slot)) => {
                        let (ring, seq) = got.expect("dirty block found");
                        // A merged data run carries the newest sequence of its
                        // parts (E20); a hole is never merged with another op here.
                        if *h {
                            prop_assert_eq!(seq, *s);
                        } else {
                            prop_assert!(seq >= *s, "a run never loses sequence");
                        }
                        prop_assert_eq!(ring.slot, *slot);
                        let (_, e) = v.find(at as u64).unwrap();
                        prop_assert_eq!(e.hole, *h);
                    }
                }
            }
            prop_assert_eq!(v.blocks(), model.iter().filter(|m| m.is_some()).count() as u64);
            prop_assert_eq!(v.data_blocks(), model.iter().filter(|m| matches!(m, Some((_, false, _)))).count() as u64);
        }
    }
}
