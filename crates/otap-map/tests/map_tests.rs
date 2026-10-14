//! The persistent map against a model: lookups, folds, the log and its
//! replay, pinned roots, the birth walk, and the checks that catch a flipped
//! bit or a misdirected page.

use std::collections::{BTreeMap, BTreeSet};

use otap_format::{
    CpLogDir, MapEntry, MapPage, MapRoot, RootRef, Uuid, BLOCK_SIZE, MAP_PAGE_SIZE,
    SECTORS_PER_BLOCK,
};
use otap_map::{
    index_image, map_pages_for, CpInput, MapError, MapSet, PageCache, PageReader, PinnedRoot,
    RegionWriter, VecReader, VecWriter, MAP_PAGE_BLOCKS,
};
use proptest::prelude::*;

fn agg() -> Uuid {
    Uuid::from_bytes([3; 16])
}

/// A device image: the writer appends regions, the reader reads them.
#[derive(Debug, Default, Clone)]
struct Image {
    bytes: Vec<u8>,
}

impl Image {
    fn new() -> Self {
        Self {
            bytes: vec![0u8; 8 * BLOCK_SIZE],
        }
    }

    fn reader(&self) -> VecReader {
        VecReader {
            bytes: self.bytes.clone(),
            reads: 0,
        }
    }
}

impl RegionWriter for Image {
    fn place(&mut self, bytes: &[u8]) -> Result<RootRef, MapError> {
        let mut w = VecWriter {
            bytes: std::mem::take(&mut self.bytes),
            placed: Vec::new(),
        };
        let root = w.place(bytes)?;
        self.bytes = w.bytes;
        Ok(root)
    }
}

impl PageReader for Image {
    fn read_blocks(&mut self, pba: u64, dst: &mut [u8]) -> Result<(), MapError> {
        let mut r = VecReader {
            bytes: std::mem::take(&mut self.bytes),
            reads: 0,
        };
        let out = r.read_blocks(pba, dst);
        self.bytes = r.bytes;
        out
    }
}

/// A region reader for an image: reads a root's region and verifies it.
fn read_region(image: &Image, root: &RootRef) -> Vec<u8> {
    let start = root.pba as usize * BLOCK_SIZE;
    let bytes = image.bytes[start..start + root.bytes() as usize].to_vec();
    root.verify(&bytes).expect("a placed region verifies");
    bytes
}

fn data(lseg: u64, off: u32, birth: u32, crc: u32) -> MapEntry {
    MapEntry::data(lseg, off, SECTORS_PER_BLOCK, birth, crc).unwrap()
}

/// The committed truth: every mapped block of every volume.
type Model = BTreeMap<(u32, u64), MapEntry>;

/// One pool under test: the map, the image, the model, and the directory
/// and roots as the last CP published them.
struct Pool {
    map: MapSet,
    image: Image,
    model: Model,
    sizes: BTreeMap<u32, u64>,
    cp_seq: u64,
    dir: Option<(RootRef, CpLogDir)>,
    roots: Vec<MapRoot>,
    roots_root: Option<RootRef>,
    /// Snapshots: `(snap cp, pinned root, the model at that CP)`.
    pins: Vec<(u64, PinnedRoot, Model)>,
}

impl Pool {
    fn new(volumes: &[(u32, u64)]) -> Self {
        let mut pool = Self {
            map: MapSet::new(agg(), 8),
            image: Image::new(),
            model: Model::new(),
            sizes: BTreeMap::new(),
            cp_seq: 0,
            dir: None,
            roots: Vec::new(),
            roots_root: None,
            pins: Vec::new(),
        };
        pool.cp(Vec::new(), false, volumes.to_vec());
        pool
    }

    /// Runs one CP through prepare, the engine's part (directory and roots
    /// table placement) and apply; updates the model.
    fn cp(&mut self, deltas: Vec<(u32, u64, MapEntry)>, fold: bool, new_vols: Vec<(u32, u64)>) {
        self.cp_seq += 1;
        let input = CpInput {
            cp_seq: self.cp_seq,
            cp_nonce: 0x55 + self.cp_seq,
            deltas: deltas.clone(),
            fold,
            new_volumes: new_vols.clone(),
            max_region_pages: 3,
        };
        let Pool { map, image, .. } = self;
        let mut reader = image.reader();
        let prepared = map.prepare_cp(&mut reader, &input, image).expect("prepare");
        // The engine's part: the roots table, then the directory.
        let roots_bytes = otap_format::map_roots_image(&prepared.roots_table).unwrap();
        let roots_root = image.place(&roots_bytes).unwrap();
        let dir_bytes = prepared.dir.encode(agg()).unwrap();
        let dir_root = image.place(&dir_bytes).unwrap();
        // Every placed region verifies in the image.
        for root in &prepared.written {
            read_region(image, root);
        }
        // What was replaced agrees with the model.
        for r in &prepared.replaced {
            let old = self
                .model
                .get(&(r.vol, r.lba))
                .copied()
                .unwrap_or(MapEntry::UNMAPPED);
            assert_eq!(r.old, old, "replaced {}/{}", r.vol, r.lba);
        }
        let fold_roots = prepared.fold_roots.clone();
        map.apply(prepared.update);
        for (vol, size) in new_vols {
            self.sizes.insert(vol, size);
        }
        let mut last: BTreeMap<(u32, u64), MapEntry> = BTreeMap::new();
        for (vol, lba, e) in deltas {
            last.insert((vol, lba), e);
        }
        for ((vol, lba), e) in last {
            self.model.insert((vol, lba), e);
        }
        self.dir = Some((dir_root, prepared.dir));
        self.roots = prepared.roots_table;
        self.roots_root = Some(roots_root);
        if fold {
            for (vol, root, index) in fold_roots {
                let pinned = PinnedRoot::from_parts(vol, root, index);
                let snap: Model = self
                    .model
                    .iter()
                    .filter(|((v, _), _)| *v == vol)
                    .map(|(k, e)| (*k, *e))
                    .collect();
                self.pins.push((self.cp_seq, pinned, snap));
            }
        }
    }

    /// Every block of every volume reads as the model says.
    fn check_lookups(&mut self) {
        let mut reader = self.image.reader();
        for (vol, size) in self.sizes.clone() {
            for lba in 0..size {
                let got = self.map.get(&mut reader, vol, lba).unwrap();
                let want = self
                    .model
                    .get(&(vol, lba))
                    .copied()
                    .unwrap_or(MapEntry::UNMAPPED);
                assert_eq!(got, want, "volume {vol} LBA {lba}");
            }
        }
    }

    /// `diff(base)` equals the model's entries born after `base`.
    fn check_diff(&mut self, base: u64) {
        let mut reader = self.image.reader();
        for (vol, _) in self.sizes.clone() {
            let mut got = Vec::new();
            self.map
                .diff(&mut reader, vol, base, |lba, e| got.push((lba, e)))
                .unwrap();
            let want: Vec<(u64, MapEntry)> = self
                .model
                .iter()
                .filter(|((v, _), e)| *v == vol && u64::from(e.birth_cp) > base)
                .map(|((_, l), e)| (*l, *e))
                .collect();
            assert_eq!(got, want, "diff({base}) of volume {vol}");
        }
    }

    /// Every pinned root still reads as the model did at its CP.
    fn check_pins(&mut self) {
        let mut reader = self.image.reader();
        let mut cache = PageCache::new(4);
        for (cp, pinned, snap) in &self.pins {
            let size = self.sizes[&pinned.vol];
            for lba in 0..size {
                let got = pinned.get(&mut cache, &mut reader, lba).unwrap();
                let want = snap
                    .get(&(pinned.vol, lba))
                    .copied()
                    .unwrap_or(MapEntry::UNMAPPED);
                assert_eq!(
                    got, want,
                    "snapshot at CP {cp}, volume {} LBA {lba}",
                    pinned.vol
                );
                let through_map = self.map.pinned_get(&mut reader, pinned, lba).unwrap();
                assert_eq!(through_map, want);
            }
            let mut walked = Vec::new();
            pinned
                .diff(&mut cache, &mut reader, 0, |lba, e| walked.push((lba, e)))
                .unwrap();
            let want: Vec<(u64, MapEntry)> = snap.iter().map(|((_, l), e)| (*l, *e)).collect();
            assert_eq!(walked, want, "snapshot walk at CP {cp}");
        }
    }

    /// Boots a fresh map from the last published roots and directory, as
    /// the engine does, and checks it equals the live one.
    fn reboot(&mut self) {
        let (dir_root, _) = self.dir.clone().unwrap();
        let dir = CpLogDir::decode(&read_region(&self.image, &dir_root), &agg()).unwrap();
        assert_eq!(dir.cp_seq, self.cp_seq);
        let roots_bytes = read_region(&self.image, &self.roots_root.unwrap());
        let roots = otap_format::parse_map_roots(&roots_bytes, self.roots.len() as u32).unwrap();
        let image = &self.image;
        let mut fresh = MapSet::from_roots(agg(), 8, dir.cp_seq, dir.fold_cp_seq, &roots, |root| {
            Ok(read_region(image, &otap_map::index_root_ref(root)))
        })
        .expect("roots load");
        let mut cps: BTreeSet<u64> = BTreeSet::new();
        for delta in &dir.deltas {
            let replayed = fresh
                .replay(&read_region(&self.image, delta))
                .expect("replay");
            cps.insert(replayed.cp_seq);
        }
        fresh.set_delta_roots(dir.deltas.clone(), cps.len() as u64);
        assert_eq!(fresh.fold_cp(), self.map.fold_cp());
        assert_eq!(fresh.overlay_entries(), self.map.overlay_entries());
        assert_eq!(fresh.delta_roots(), self.map.delta_roots());
        let mut reader = self.image.reader();
        for (vol, size) in self.sizes.clone() {
            for lba in 0..size {
                let a = fresh.get(&mut reader, vol, lba).unwrap();
                let b = self.map.get(&mut reader, vol, lba).unwrap();
                assert_eq!(a, b, "after reboot: volume {vol} LBA {lba}");
            }
        }
        self.map = fresh;
    }
}

#[test]
fn writes_folds_replay_and_reboot_agree_with_the_model() {
    let mut p = Pool::new(&[(0, 3000), (2, 50)]);
    assert_eq!(p.roots.len(), 3, "the table runs to the highest volume");
    assert!(p.roots[1].is_absent());
    p.check_lookups();
    p.cp(
        vec![(0, 5, data(7, 1, 1, 11)), (0, 2047, data(7, 2, 1, 12))],
        false,
        vec![],
    );
    p.check_lookups();
    p.reboot();
    p.cp(
        vec![
            (2, 0, MapEntry::hole(2).unwrap()),
            (0, 5, data(8, 0, 2, 13)),
        ],
        false,
        vec![],
    );
    p.check_lookups();
    p.check_diff(0);
    p.check_diff(1);
    p.check_diff(2);
    assert_eq!(p.map.cps_since_fold(), 2);
    assert_eq!(p.map.delta_roots().len(), 2);
    // A fold: the pages hold everything, the overlay empties, the
    // directory lists only the fold's pages (no delta of its own).
    p.cp(vec![(0, 2048, data(9, 0, 3, 14))], true, vec![]);
    assert_eq!(p.map.overlay_entries(), 0);
    assert_eq!(p.map.fold_cp(), 4);
    assert_eq!(p.map.cps_since_fold(), 0);
    let (_, dir) = p.dir.clone().unwrap();
    assert_eq!((dir.cp_seq, dir.fold_cp_seq), (4, 4));
    assert_eq!(dir.deltas.len(), 0, "a fold writes no delta region");
    assert_eq!(
        dir.pages.len(),
        4,
        "pages 0, 1 and 2 of volume 0 and page 0 of volume 2"
    );
    p.check_lookups();
    p.check_pins();
    p.reboot();
    p.check_lookups();
    // More writes after the fold, a volume created, another fold, a reboot
    // in between: the snapshot keeps reading the state at CP 3.
    p.cp(
        vec![
            (0, 5, data(10, 0, 4, 15)),
            (0, 2048, MapEntry::hole(4).unwrap()),
        ],
        false,
        vec![(1, 10)],
    );
    p.check_lookups();
    p.check_pins();
    p.cp(vec![(1, 9, data(11, 0, 5, 16))], false, vec![]);
    p.reboot();
    p.check_lookups();
    p.check_pins();
    p.check_diff(3);
    p.cp(vec![], true, vec![]);
    p.check_lookups();
    p.check_pins();
    p.reboot();
    p.check_pins();
    p.check_diff(0);
}

#[test]
fn the_fold_frees_replaced_pages_indexes_and_deltas() {
    let mut p = Pool::new(&[(0, 2048)]);
    p.cp(vec![(0, 0, data(1, 0, 1, 1))], false, vec![]);
    p.cp(vec![(0, 1024, data(1, 1, 2, 2))], false, vec![]);
    let old_deltas = p.map.delta_roots().to_vec();
    assert_eq!(old_deltas.len(), 2);
    let Pool { map, image, .. } = &mut p;
    let mut reader = image.reader();
    let input = CpInput {
        cp_seq: 4,
        cp_nonce: 1,
        deltas: vec![(0, 1025, data(1, 2, 4, 3))],
        fold: true,
        new_volumes: vec![],
        max_region_pages: 64,
    };
    let prepared = map.prepare_cp(&mut reader, &input, image).unwrap();
    assert!(
        prepared.freed_pages.is_empty(),
        "no page existed before the first fold"
    );
    assert_eq!(
        prepared.freed_indexes.len(),
        1,
        "the volume's empty index is replaced"
    );
    assert_eq!(prepared.freed_deltas, old_deltas);
    assert_eq!(prepared.dir.pages.len(), 2, "two dirty pages");
    assert_eq!(
        prepared.dir.deltas.len(),
        0,
        "a fold writes no delta region"
    );
    assert_eq!(prepared.fold_roots.len(), 1);
    assert_eq!(prepared.fold_roots[0].1.checkpoint_cp_seq, 4);
    assert_eq!(prepared.fold_roots[0].1.max_birth, 4);
    map.apply(prepared.update);
    // A second fold touching page 1 only frees that page and the index.
    let mut reader = image.reader();
    let input = CpInput {
        cp_seq: 5,
        cp_nonce: 2,
        deltas: vec![(0, 1030, data(1, 3, 5, 4))],
        fold: true,
        new_volumes: vec![],
        max_region_pages: 64,
    };
    let prepared = map.prepare_cp(&mut reader, &input, image).unwrap();
    assert_eq!(prepared.freed_pages.len(), 1);
    assert_eq!(prepared.freed_pages[0].page_no, 1);
    assert_eq!(prepared.freed_pages[0].root.pages, MAP_PAGE_BLOCKS);
    assert_eq!(prepared.freed_indexes.len(), 1);
    assert_eq!(
        prepared.freed_deltas.len(),
        0,
        "the previous fold wrote no delta of its own, so none is freed"
    );
    // The untouched page keeps its block; the index entry is the same.
    let before = map.volume(0).unwrap().index()[0];
    map.apply(prepared.update);
    assert_eq!(map.volume(0).unwrap().index()[0], before);
    assert_eq!(map.stats().folds, 2);
}

#[test]
fn a_flipped_bit_or_a_misdirected_page_is_refused() {
    let mut p = Pool::new(&[(0, 4096)]);
    p.cp(
        vec![(0, 1, data(1, 0, 1, 1)), (0, 2000, data(1, 1, 1, 2))],
        true,
        vec![],
    );
    let index = p.map.volume(0).unwrap().index().to_vec();
    let (a, b) = (index[0], index[1]);
    assert!(!a.is_none() && !b.is_none());
    // A fresh map over a damaged image (the live map may still hold the
    // good page in its cache, which is immutable by design).
    let fresh_over = |image: &Image| {
        MapSet::from_roots(agg(), 2, p.cp_seq, p.map.fold_cp(), &p.roots, |root| {
            Ok(read_region(image, &otap_map::index_root_ref(root)))
        })
        .unwrap()
    };
    // A bit flipped in page 0: the CRC in the index entry catches it.
    let mut damaged = p.image.clone();
    damaged.bytes[a.page_pba as usize * BLOCK_SIZE + 100] ^= 0x10;
    let mut fresh = fresh_over(&damaged);
    let mut reader = damaged.reader();
    assert!(matches!(
        fresh.get(&mut reader, 0, 1),
        Err(MapError::Format(otap_format::FormatError::BadCrc { .. }))
    ));
    assert!(fresh.get(&mut reader, 0, 2000).is_ok(), "page 1 is intact");
    // Page 1's bytes landing where page 0 should be (a misdirected write):
    // the CRC differs, so the page is refused rather than believed.
    let mut misdirected = p.image.clone();
    let src = b.page_pba as usize * BLOCK_SIZE;
    let dst = a.page_pba as usize * BLOCK_SIZE;
    let copy = misdirected.bytes[src..src + MAP_PAGE_SIZE].to_vec();
    misdirected.bytes[dst..dst + MAP_PAGE_SIZE].copy_from_slice(&copy);
    let mut fresh = fresh_over(&misdirected);
    let mut reader = misdirected.reader();
    assert!(
        fresh.get(&mut reader, 0, 1).is_err(),
        "a misdirected page is refused"
    );
    let mut empty = MapSet::new(agg(), 2);
    assert!(matches!(
        empty.get(&mut reader, 0, 1),
        Err(MapError::NoSuchVolume { vol: 0 })
    ));
    // A damaged index region fails at load.
    let mut bad_index = p.image.clone();
    let root = p.roots[0];
    bad_index.bytes[root.index_pba as usize * BLOCK_SIZE + 8] ^= 1;
    let image = &bad_index;
    let err = MapSet::from_roots(agg(), 2, p.cp_seq, p.map.fold_cp(), &p.roots, |root| {
        let start = root.index_pba as usize * BLOCK_SIZE;
        Ok(image.bytes[start..start + root.index_pages as usize * 16384].to_vec())
    })
    .err();
    assert!(
        err.is_some(),
        "a flipped index bit is caught by the root's CRC"
    );
}

#[test]
fn replay_refuses_disorder_and_unknown_volumes() {
    let mut p = Pool::new(&[(0, 100)]);
    p.cp(vec![(0, 1, data(1, 0, 1, 1))], false, vec![]);
    p.cp(vec![(0, 2, data(1, 1, 2, 2))], false, vec![]);
    let (_, dir) = p.dir.clone().unwrap();
    let image = &p.image;
    let boot = || {
        MapSet::from_roots(agg(), 2, dir.cp_seq, dir.fold_cp_seq, &p.roots, |root| {
            Ok(read_region(image, &otap_map::index_root_ref(root)))
        })
        .unwrap()
    };
    let d1 = read_region(image, &dir.deltas[0]);
    let d2 = read_region(image, &dir.deltas[1]);
    // Out of CP order.
    let mut m = boot();
    m.replay(&d2).unwrap();
    assert!(matches!(
        m.replay(&d1),
        Err(MapError::Replay {
            field: "cp_seq",
            ..
        })
    ));
    // The same region twice: the page position repeats.
    let mut m = boot();
    m.replay(&d1).unwrap();
    assert!(matches!(
        m.replay(&d1),
        Err(MapError::Replay {
            field: "page_seq",
            ..
        })
    ));
    // A record for a volume the roots do not list.
    let mut roots = p.roots.clone();
    roots[0] = MapRoot::ABSENT;
    let mut m = MapSet::from_roots(
        agg(),
        2,
        dir.cp_seq,
        dir.fold_cp_seq,
        &roots,
        |_| unreachable!(),
    )
    .unwrap();
    assert!(matches!(
        m.replay(&d1),
        Err(MapError::Replay { field: "vol", .. })
    ));
    // Another aggregate's pages.
    let mut m = MapSet::new(Uuid::from_bytes([9; 16]), 2);
    assert!(m.replay(&d1).is_err());
    // Not whole pages.
    assert!(boot().replay(&d1[..100]).is_err());
}

#[test]
fn prepare_refuses_what_the_format_refuses() {
    let mut p = Pool::new(&[(0, 100)]);
    let Pool { map, image, .. } = &mut p;
    let mut reader = image.reader();
    let bad =
        |deltas: Vec<(u32, u64, MapEntry)>, new_volumes: Vec<(u32, u64)>, cp_seq: u64| CpInput {
            cp_seq,
            cp_nonce: 1,
            deltas,
            fold: false,
            new_volumes,
            max_region_pages: 4,
        };
    assert!(matches!(
        map.prepare_cp(
            &mut reader,
            &bad(vec![(0, 100, data(1, 0, 2, 1))], vec![], 2),
            image
        ),
        Err(MapError::OutOfRange { vol: 0, lba: 100 })
    ));
    assert!(matches!(
        map.prepare_cp(
            &mut reader,
            &bad(vec![(1, 0, data(1, 0, 2, 1))], vec![], 2),
            image
        ),
        Err(MapError::NoSuchVolume { vol: 1 })
    ));
    assert!(matches!(
        map.prepare_cp(
            &mut reader,
            &bad(vec![(0, 0, MapEntry::UNMAPPED)], vec![], 2),
            image
        ),
        Err(MapError::Format(_))
    ));
    assert!(matches!(
        map.prepare_cp(&mut reader, &bad(vec![], vec![(0, 10)], 2), image),
        Err(MapError::VolumeExists { vol: 0 })
    ));
    assert!(matches!(
        map.prepare_cp(&mut reader, &bad(vec![], vec![(1, 0)], 2), image),
        Err(MapError::BadSize { vol: 1, .. })
    ));
    assert!(
        map.prepare_cp(&mut reader, &bad(vec![], vec![], 1), image)
            .is_err(),
        "a CP at or below the fold"
    );
    assert!(map
        .prepare_cp(&mut reader, &bad(vec![], vec![], 0), image)
        .is_err());
    // Nothing changed.
    assert_eq!(map.overlay_entries(), 0);
    assert_eq!(map.volumes().count(), 1);
}

#[test]
fn index_images_and_page_counts_follow_the_format() {
    assert_eq!(map_pages_for(1), 1);
    assert_eq!(map_pages_for(1024), 1);
    assert_eq!(map_pages_for(1025), 2);
    assert_eq!(index_image(&[]).len(), 16384);
    assert_eq!(
        index_image(&vec![otap_format::PageIndexEntry::NONE; 1025]).len(),
        2 * 16384
    );
    // A root's index is as long as the format says, so a 4 GiB + 1 block
    // volume needs two index pages.
    let mut p = Pool::new(&[(5, (1 << 20) + 1)]);
    assert_eq!(p.roots.len(), 6);
    assert_eq!(p.roots[5].index_pages, 2);
    p.cp(vec![(5, 1 << 20, data(1, 0, 1, 7))], true, vec![]);
    p.check_lookups();
    p.reboot();
    assert_eq!(p.map.volume(5).unwrap().index().len(), 1025);
    assert!(p.map.volume(5).unwrap().index()[1024].page_pba > 0);
    p.check_lookups();
}

// ---------------------------------------------------------------------------
// the property: random writes, holes, folds, new volumes, reboots and
// snapshots against the model
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
enum Op {
    Cp {
        writes: Vec<(u8, u16, bool)>,
        fold: bool,
    },
    NewVolume(u8, u16),
    Reboot,
}

fn op() -> impl Strategy<Value = Op> {
    prop_oneof![
        6 => (prop::collection::vec((0u8..4, 0u16..2100, any::<bool>()), 0..12), any::<bool>())
            .prop_map(|(writes, fold)| Op::Cp { writes, fold }),
        1 => (0u8..4, 1u16..2100).prop_map(|(v, s)| Op::NewVolume(v, s)),
        2 => Just(Op::Reboot),
    ]
}

proptest! {
    #![proptest_config(ProptestConfig {
        failure_persistence: None,
        cases: 64,
        ..ProptestConfig::default()
    })]

    #[test]
    fn the_map_persists_every_cp_and_every_snapshot(ops in prop::collection::vec(op(), 1..24)) {
        let mut p = Pool::new(&[(0, 2100)]);
        for op in ops {
            match op {
                Op::Cp { writes, fold } => {
                    let cp = p.cp_seq + 1;
                    let mut deltas = Vec::new();
                    for (i, (vol, lba, hole)) in writes.iter().enumerate() {
                        let vol = u32::from(*vol);
                        let Some(size) = p.sizes.get(&vol) else { continue };
                        let lba = u64::from(*lba) % size;
                        let e = if *hole {
                            MapEntry::hole(cp as u32).unwrap()
                        } else {
                            data(1 + cp, i as u32, cp as u32, (cp as u32) * 100 + i as u32)
                        };
                        deltas.push((vol, lba, e));
                    }
                    p.cp(deltas, fold, vec![]);
                }
                Op::NewVolume(vol, size) => {
                    let vol = u32::from(vol);
                    if p.sizes.contains_key(&vol) {
                        continue;
                    }
                    p.cp(vec![], false, vec![(vol, u64::from(size))]);
                }
                Op::Reboot => p.reboot(),
            }
            p.check_lookups();
            p.check_pins();
            let base = p.cp_seq / 2;
            p.check_diff(base);
        }
        p.reboot();
        p.check_lookups();
        p.check_pins();
        p.check_diff(0);
        prop_assert!(p.map.stats().page_reads <= p.image.reader().bytes.len() as u64, "sanity");
    }
}

#[test]
fn page_cache_keeps_the_newest_pages() {
    let mut cache = PageCache::new(2);
    cache.insert(1, Box::new(MapPage::zeroed()));
    cache.insert(2, Box::new(MapPage::zeroed()));
    cache.insert(3, Box::new(MapPage::zeroed()));
    assert_eq!(cache.len(), 2);
    assert!(cache.get(1).is_none());
    assert!(cache.get(2).is_some() && cache.get(3).is_some());
    assert_eq!(cache.counters(), (2, 1));
}
