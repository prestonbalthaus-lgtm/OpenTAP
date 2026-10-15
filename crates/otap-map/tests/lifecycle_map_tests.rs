//! Step 5 (E36, E37): what the map does for a volume rollback, a resize,
//! a deletion and a relocation that rewrites a snapshot's pages, against a
//! model, with reboots from the published roots in between.

use std::collections::{BTreeMap, BTreeSet};

use otap_format::{
    CpLogDir, MapEntry, MapPage, MapRoot, PageIndexEntry, RootRef, Uuid, BLOCK_SIZE,
    MAP_ENTRIES_PER_PAGE, SECTORS_PER_BLOCK,
};
use otap_map::{
    index_root_ref, map_pages_for, page_root_ref, CpInput, MapError, MapSet, PageCache, PageReader,
    PinnedRoot, RegionWriter, Rollback, VecReader, VecWriter,
};
use proptest::prelude::*;

fn agg() -> Uuid {
    Uuid::from_bytes([5; 16])
}

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

    fn region(&self, root: &RootRef) -> Vec<u8> {
        let start = root.pba as usize * BLOCK_SIZE;
        let bytes = self.bytes[start..start + root.bytes() as usize].to_vec();
        root.verify(&bytes).expect("a placed region verifies");
        bytes
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

fn data(lseg: u64, off: u32, birth: u32, crc: u32) -> MapEntry {
    MapEntry::data(lseg, off, SECTORS_PER_BLOCK, birth, crc).unwrap()
}

type Model = BTreeMap<(u32, u64), MapEntry>;

/// A snapshot the harness pinned at a fold: the CP, the root and the model.
#[derive(Debug, Clone)]
struct Pin {
    cp: u64,
    root: PinnedRoot,
    model: Model,
}

struct Pool {
    map: MapSet,
    image: Image,
    model: Model,
    sizes: BTreeMap<u32, u64>,
    cp_seq: u64,
    dir: Option<(RootRef, CpLogDir)>,
    roots: Vec<MapRoot>,
    roots_root: Option<RootRef>,
    pins: Vec<Pin>,
}

/// What one CP asks beyond its deltas.
#[derive(Debug, Clone, Default)]
struct Ops {
    fold: bool,
    new_volumes: Vec<(u32, u64)>,
    rollbacks: Vec<(u32, usize)>,
    resizes: Vec<(u32, u64)>,
    deletes: Vec<u32>,
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
        pool.cp(
            Vec::new(),
            Ops {
                new_volumes: volumes.to_vec(),
                ..Ops::default()
            },
        )
        .unwrap();
        pool
    }

    /// Runs one CP and checks what it released and freed against the
    /// model; returns the prepared CP's released list.
    fn cp(
        &mut self,
        deltas: Vec<(u32, u64, MapEntry)>,
        ops: Ops,
    ) -> Result<Vec<(u32, u64, MapEntry)>, MapError> {
        let input = CpInput {
            cp_seq: self.cp_seq + 1,
            cp_nonce: 0x99 + self.cp_seq,
            deltas: deltas.clone(),
            fold: ops.fold,
            new_volumes: ops.new_volumes.clone(),
            max_region_pages: 4,
            rollbacks: ops
                .rollbacks
                .iter()
                .map(|(vol, pin)| {
                    let p = &self.pins[*pin];
                    Rollback {
                        vol: *vol,
                        birth_cp: p.cp,
                        root: p.root.root,
                        index: p.root.index.clone(),
                    }
                })
                .collect(),
            resizes: ops.resizes.clone(),
            deletes: ops.deletes.clone(),
        };
        let old_indexes: BTreeMap<u32, (MapRoot, Vec<PageIndexEntry>)> = self
            .map
            .volumes()
            .map(|(v, m)| (v, (*m.root(), m.index().to_vec())))
            .collect();
        let Pool { map, image, .. } = self;
        let mut reader = image.reader();
        let prepared = map.prepare_cp(&mut reader, &input, image)?;
        self.cp_seq += 1;
        assert!(prepared.damaged_pages.is_empty());
        let roots_bytes = otap_format::map_roots_image(&prepared.roots_table).unwrap();
        let roots_root = image.place(&roots_bytes).unwrap();
        let dir_bytes = prepared.dir.encode(agg()).unwrap();
        let dir_root = image.place(&dir_bytes).unwrap();
        for root in &prepared.written {
            image.region(root);
        }
        // What the CP released is exactly what the model says the
        // rollbacks, shrinks and deletions drop.
        let mut expect_released: BTreeMap<(u32, u64), MapEntry> = BTreeMap::new();
        for (vol, pin) in &ops.rollbacks {
            let s = self.pins[*pin].cp;
            for ((v, lba), e) in &self.model {
                if v == vol && u64::from(e.birth_cp) > s {
                    expect_released.insert((*v, *lba), *e);
                }
            }
        }
        for (vol, size) in &ops.resizes {
            for ((v, lba), e) in &self.model {
                if v == vol && lba >= size {
                    expect_released.insert((*v, *lba), *e);
                }
            }
        }
        for vol in &ops.deletes {
            for ((v, lba), e) in &self.model {
                if v == vol {
                    expect_released.insert((*v, *lba), *e);
                }
            }
        }
        let got: BTreeMap<(u32, u64), MapEntry> = prepared
            .released
            .iter()
            .map(|(v, l, e)| ((*v, *l), *e))
            .collect();
        assert_eq!(got.len(), prepared.released.len(), "released twice");
        assert_eq!(got, expect_released, "released entries");
        // Freed pages: for a rollback, every old page the snapshot does not
        // share; for a deletion, every page; for a shrink, every page past
        // the new size.
        for (vol, pin) in &ops.rollbacks {
            let (_, old_index) = &old_indexes[vol];
            let snap = &self.pins[*pin].root;
            for (page_no, old) in old_index.iter().enumerate() {
                if old.is_none() {
                    continue;
                }
                let shared = snap.references_page(page_no as u64, old.page_pba);
                let freed = prepared.freed_pages.iter().any(|f| {
                    f.vol == *vol && f.page_no == page_no as u64 && f.root.pba == old.page_pba
                });
                assert_eq!(freed, !shared, "volume {vol} page {page_no} freed");
            }
        }
        for vol in &ops.deletes {
            let (root, old_index) = &old_indexes[vol];
            for (page_no, old) in old_index.iter().enumerate() {
                if !old.is_none() {
                    assert!(
                        prepared
                            .freed_pages
                            .iter()
                            .any(|f| f.vol == *vol && f.root == page_root_ref(old)),
                        "deleted page {page_no}"
                    );
                }
            }
            assert!(prepared
                .freed_indexes
                .iter()
                .any(|f| f.vol == *vol && f.root == index_root_ref(root)));
            assert!(
                prepared.roots_table.len() <= *vol as usize
                    || prepared.roots_table[*vol as usize].is_absent()
            );
        }
        let fold_roots = prepared.fold_roots.clone();
        map.apply(prepared.update);
        // The model.
        for (vol, size) in &ops.new_volumes {
            self.sizes.insert(*vol, *size);
        }
        for (vol, pin) in &ops.rollbacks {
            let snap = self.pins[*pin].model.clone();
            self.model.retain(|(v, _), _| v != vol);
            for ((v, lba), e) in snap {
                if v == *vol {
                    self.model.insert((v, lba), e);
                }
            }
            self.sizes.insert(*vol, self.pins[*pin].root.root.lba_count);
        }
        for (vol, size) in &ops.resizes {
            self.model.retain(|(v, lba), _| v != vol || lba < size);
            self.sizes.insert(*vol, *size);
        }
        for vol in &ops.deletes {
            self.model.retain(|(v, _), _| v != vol);
            self.sizes.remove(vol);
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
        if ops.fold {
            for (vol, root, index) in fold_roots {
                let pinned = PinnedRoot::from_parts(vol, root, index);
                let snap: Model = self
                    .model
                    .iter()
                    .filter(|((v, _), _)| *v == vol)
                    .map(|(k, e)| (*k, *e))
                    .collect();
                self.pins.push(Pin {
                    cp: self.cp_seq,
                    root: pinned,
                    model: snap,
                });
            }
        }
        Ok(prepared.released)
    }

    /// The pin index of the latest fold root of `vol`.
    fn pin_of(&self, vol: u32) -> usize {
        self.pins
            .iter()
            .rposition(|p| p.root.vol == vol)
            .expect("a pin of the volume")
    }

    fn check_lookups(&mut self) {
        let mut reader = self.image.reader();
        for (vol, size) in self.sizes.clone() {
            let m = self.map.volume(vol).expect("the volume is mapped");
            assert_eq!(m.lba_count(), size, "volume {vol} size");
            assert_eq!(m.index().len(), map_pages_for(size));
            for lba in 0..size {
                let got = self.map.get(&mut reader, vol, lba).unwrap();
                let want = self
                    .model
                    .get(&(vol, lba))
                    .copied()
                    .unwrap_or(MapEntry::UNMAPPED);
                assert_eq!(got, want, "volume {vol} LBA {lba}");
            }
            assert!(matches!(
                self.map.get(&mut reader, vol, size),
                Err(MapError::OutOfRange { .. })
            ));
        }
        for (vol, _) in self.map.volumes().map(|(v, _)| (v, ())).collect::<Vec<_>>() {
            assert!(self.sizes.contains_key(&vol), "volume {vol} should be gone");
        }
    }

    /// Every pin still reads as the model did at its CP, through its own
    /// pages: a rollback shares pages with the snapshot and must not have
    /// changed them.
    fn check_pins(&mut self) {
        let mut reader = self.image.reader();
        let mut cache = PageCache::new(4);
        for pin in &self.pins {
            for lba in 0..pin.root.root.lba_count {
                let got = pin.root.get(&mut cache, &mut reader, lba).unwrap();
                let want = pin
                    .model
                    .get(&(pin.root.vol, lba))
                    .copied()
                    .unwrap_or(MapEntry::UNMAPPED);
                assert_eq!(got, want, "pin at CP {}, LBA {lba}", pin.cp);
            }
        }
    }

    /// A fresh map from the published roots and directory equals the live
    /// one.
    fn reboot(&mut self) {
        let (dir_root, _) = self.dir.clone().unwrap();
        let dir = CpLogDir::decode(&self.image.region(&dir_root), &agg()).unwrap();
        let roots_bytes = self.image.region(&self.roots_root.unwrap());
        let roots = otap_format::parse_map_roots(&roots_bytes, self.roots.len() as u32).unwrap();
        let image = &self.image;
        let mut fresh = MapSet::from_roots(agg(), 8, dir.cp_seq, dir.fold_cp_seq, &roots, |root| {
            Ok(image.region(&index_root_ref(root)))
        })
        .expect("roots load");
        let mut cps: BTreeSet<u64> = BTreeSet::new();
        for delta in &dir.deltas {
            let replayed = fresh.replay(&self.image.region(delta)).expect("replay");
            cps.insert(replayed.cp_seq);
        }
        fresh.set_delta_roots(dir.deltas.clone(), cps.len() as u64);
        let mut reader = self.image.reader();
        for (vol, size) in self.sizes.clone() {
            for lba in 0..size {
                let a = fresh.get(&mut reader, vol, lba).unwrap();
                let b = self.map.get(&mut reader, vol, lba).unwrap();
                assert_eq!(a, b, "after reboot: volume {vol} LBA {lba}");
            }
        }
        assert_eq!(
            fresh.volumes().map(|(v, _)| v).collect::<Vec<_>>(),
            self.map.volumes().map(|(v, _)| v).collect::<Vec<_>>()
        );
        self.map = fresh;
    }
}

fn fold() -> Ops {
    Ops {
        fold: true,
        ..Ops::default()
    }
}

// ---------------------------------------------------------------------------
// rollback
// ---------------------------------------------------------------------------

#[test]
fn a_rollback_restores_the_snapshot_and_releases_what_came_after() {
    let page = MAP_ENTRIES_PER_PAGE as u64;
    let mut p = Pool::new(&[(0, 2 * page + 10), (1, 40)]);
    p.cp(
        vec![
            (0, 1, data(3, 0, 2, 1)),
            (0, page + 5, data(3, 1, 2, 2)),
            (1, 7, data(3, 2, 2, 3)),
        ],
        Ops::default(),
    )
    .unwrap();
    // The snapshot (CP 3, a fold).
    p.cp(vec![(0, 2 * page, data(4, 0, 3, 4))], fold()).unwrap();
    let pin = p.pin_of(0);
    assert_eq!(p.pins[pin].cp, 3);
    // Changes after it: in the pages (a fold) and in the overlay.
    p.cp(
        vec![
            (0, 1, data(5, 0, 4, 5)),
            (0, page + 6, MapEntry::hole(4).unwrap()),
            (1, 8, data(5, 1, 4, 6)),
        ],
        fold(),
    )
    .unwrap();
    p.cp(vec![(0, 2 * page + 1, data(6, 0, 5, 7))], Ops::default())
        .unwrap();
    p.check_lookups();
    // Rollback outside a fold is refused; so is a rollback with deltas.
    assert!(matches!(
        p.cp(
            vec![],
            Ops {
                rollbacks: vec![(0, pin)],
                ..Ops::default()
            }
        ),
        Err(MapError::Internal(_))
    ));
    assert!(matches!(
        p.cp(
            vec![(0, 3, data(9, 0, 6, 9))],
            Ops {
                fold: true,
                rollbacks: vec![(0, pin)],
                ..Ops::default()
            }
        ),
        Err(MapError::Internal(_))
    ));
    // The rollback: volume 0 reads as at CP 3; volume 1 keeps its changes;
    // the entries born after CP 3 were released (three of them: the data
    // at LBA 1, the hole at page+6, the data at 2*page+1).
    let released = p
        .cp(
            vec![(1, 9, data(7, 0, 6, 8))],
            Ops {
                fold: true,
                rollbacks: vec![(0, pin)],
                ..Ops::default()
            },
        )
        .unwrap();
    assert_eq!(released.len(), 3);
    assert!(released.iter().all(|(v, _, _)| *v == 0));
    p.check_lookups();
    p.check_pins();
    let (_, dir) = p.dir.clone().unwrap();
    assert!(dir.deltas.is_empty(), "a fold");
    // The live root of volume 0 is the snapshot's tree (shared pages).
    let live = p.map.volume(0).unwrap();
    assert_eq!(live.root().index_pba, p.pins[pin].root.root.index_pba);
    assert_eq!(live.root().checkpoint_cp_seq, p.cp_seq);
    assert_eq!(live.index(), p.pins[pin].root.index.as_slice());
    assert_eq!(live.overlay_len(), 0);
    p.reboot();
    p.check_lookups();
    // Writes after the rollback fold copy-on-write away from the shared
    // pages; the snapshot is untouched.
    p.cp(vec![(0, 1, data(8, 0, 7, 10))], Ops::default())
        .unwrap();
    p.cp(vec![(0, page + 5, data(8, 1, 8, 11))], fold())
        .unwrap();
    p.check_lookups();
    p.check_pins();
    p.reboot();
    p.check_pins();
    // Rolling back to the same snapshot again releases what was written
    // since, and nothing else.
    let released = p
        .cp(
            vec![],
            Ops {
                fold: true,
                rollbacks: vec![(0, pin)],
                ..Ops::default()
            },
        )
        .unwrap();
    assert_eq!(released.len(), 2);
    p.check_lookups();
    p.check_pins();
}

// ---------------------------------------------------------------------------
// resize
// ---------------------------------------------------------------------------

#[test]
fn a_resize_grows_and_shrinks_the_index_and_releases_beyond_the_end() {
    let page = MAP_ENTRIES_PER_PAGE as u64;
    let mut p = Pool::new(&[(0, 100)]);
    p.cp(vec![(0, 50, data(2, 0, 1, 1))], Ops::default())
        .unwrap();
    // Grow across a page boundary: the index gets a second page slot,
    // nothing is released, old data stays.
    let released = p
        .cp(
            vec![],
            Ops {
                fold: true,
                resizes: vec![(0, page + 500)],
                ..Ops::default()
            },
        )
        .unwrap();
    assert!(released.is_empty());
    p.check_lookups();
    assert_eq!(p.map.volume(0).unwrap().index().len(), 2);
    p.cp(vec![(0, page + 400, data(3, 0, 3, 2))], Ops::default())
        .unwrap();
    p.cp(vec![(0, 60, data(3, 1, 4, 3))], fold()).unwrap();
    p.check_lookups();
    p.reboot();
    // A grow outside a fold is refused.
    assert!(matches!(
        p.cp(
            vec![],
            Ops {
                resizes: vec![(0, 2 * page)],
                ..Ops::default()
            }
        ),
        Err(MapError::Internal(_))
    ));
    // Shrink through the page boundary into the first page: the entries at
    // or beyond 55 are released (60 and page+400), the second page is
    // freed, the first page is rewritten without LBA 60.
    let released = p
        .cp(
            vec![(0, 10, data(4, 0, 6, 4))],
            Ops {
                fold: true,
                resizes: vec![(0, 55)],
                ..Ops::default()
            },
        )
        .unwrap();
    assert_eq!(released.len(), 2);
    p.check_lookups();
    assert_eq!(p.map.volume(0).unwrap().index().len(), 1);
    p.reboot();
    p.check_lookups();
    // A shrink with a delta beyond the new size is refused.
    assert!(matches!(
        p.cp(
            vec![(0, 54, data(5, 0, 8, 5))],
            Ops {
                fold: true,
                resizes: vec![(0, 20)],
                ..Ops::default()
            }
        ),
        Err(MapError::Internal(_))
    ));
    // Grow again: what was beyond the old end reads as unmapped.
    p.cp(
        vec![],
        Ops {
            fold: true,
            resizes: vec![(0, 2 * page)],
            ..Ops::default()
        },
    )
    .unwrap();
    p.check_lookups();
    let mut reader = p.image.reader();
    assert_eq!(
        p.map.get(&mut reader, 0, 60).unwrap(),
        MapEntry::UNMAPPED,
        "a shrunk-away block does not come back"
    );
    // Sizes out of range are refused.
    assert!(matches!(
        p.cp(
            vec![],
            Ops {
                fold: true,
                resizes: vec![(0, 0)],
                ..Ops::default()
            }
        ),
        Err(MapError::BadSize { .. })
    ));
}

// ---------------------------------------------------------------------------
// delete
// ---------------------------------------------------------------------------

#[test]
fn a_deletion_releases_everything_and_frees_the_tree() {
    let page = MAP_ENTRIES_PER_PAGE as u64;
    let mut p = Pool::new(&[(0, 30), (2, page + 10), (3, 8)]);
    p.cp(
        vec![
            (2, 1, data(2, 0, 1, 1)),
            (2, page + 2, data(2, 1, 1, 2)),
            (3, 3, data(2, 2, 1, 3)),
        ],
        fold(),
    )
    .unwrap();
    p.cp(vec![(2, 5, MapEntry::hole(2).unwrap())], Ops::default())
        .unwrap();
    // Outside a fold, or with deltas, a deletion is refused.
    assert!(matches!(
        p.cp(
            vec![],
            Ops {
                deletes: vec![2],
                ..Ops::default()
            }
        ),
        Err(MapError::Internal(_))
    ));
    assert!(matches!(
        p.cp(
            vec![(2, 6, data(3, 0, 3, 4))],
            Ops {
                fold: true,
                deletes: vec![2],
                ..Ops::default()
            }
        ),
        Err(MapError::Internal(_))
    ));
    assert!(matches!(
        p.cp(
            vec![],
            Ops {
                fold: true,
                deletes: vec![7],
                ..Ops::default()
            }
        ),
        Err(MapError::NoSuchVolume { vol: 7 })
    ));
    // Deleting the highest volume shrinks the roots table; the other
    // volumes are untouched.
    let released = p
        .cp(
            vec![(0, 2, data(3, 0, 3, 5))],
            Ops {
                fold: true,
                deletes: vec![3],
                ..Ops::default()
            },
        )
        .unwrap();
    assert_eq!(released.len(), 1);
    assert_eq!(p.roots.len(), 3);
    p.check_lookups();
    p.reboot();
    let released = p
        .cp(
            vec![],
            Ops {
                fold: true,
                deletes: vec![2],
                ..Ops::default()
            },
        )
        .unwrap();
    assert_eq!(released.len(), 3, "two data entries and a hole");
    assert_eq!(p.roots.len(), 1);
    p.check_lookups();
    let mut reader = p.image.reader();
    assert!(matches!(
        p.map.get(&mut reader, 2, 0),
        Err(MapError::NoSuchVolume { vol: 2 })
    ));
    p.reboot();
    // The index can be reused by a new volume.
    p.cp(
        vec![(2, 0, data(4, 0, 6, 6))],
        Ops {
            fold: true,
            new_volumes: vec![(2, 16)],
            ..Ops::default()
        },
    )
    .unwrap();
    p.check_lookups();
    p.reboot();
    p.check_lookups();
}

// ---------------------------------------------------------------------------
// relocation: rewriting a pinned root
// ---------------------------------------------------------------------------

#[test]
fn rewriting_a_pinned_root_moves_only_the_addresses() {
    let page = MAP_ENTRIES_PER_PAGE as u64;
    let mut p = Pool::new(&[(0, page + 20)]);
    p.cp(
        vec![
            (0, 1, data(2, 0, 1, 11)),
            (0, 2, data(2, 1, 1, 12)),
            (0, page + 3, data(2, 2, 1, 13)),
        ],
        fold(),
    )
    .unwrap();
    let pin = p.pin_of(0);
    let pinned = p.pins[pin].root.clone();
    let changes = vec![
        (1, data(2, 0, 1, 11), data(9, 0, 1, 11)),
        (page + 3, data(2, 2, 1, 13), data(9, 1, 1, 13)),
    ];
    let Pool { map, image, .. } = &mut p;
    let mut reader = image.reader();
    let rewritten = map
        .rewrite_pinned(&mut reader, image, &pinned, &changes)
        .unwrap();
    assert_eq!(rewritten.pages.len(), 2);
    assert_eq!(rewritten.freed_pages.len(), 2);
    assert_eq!(rewritten.freed_index.root, index_root_ref(&pinned.root));
    assert_ne!(rewritten.pinned.root.index_pba, pinned.root.index_pba);
    assert_eq!(rewritten.pinned.root.lba_count, pinned.root.lba_count);
    assert_eq!(
        rewritten.pinned.root.checkpoint_cp_seq,
        pinned.root.checkpoint_cp_seq
    );
    assert_eq!(rewritten.pinned.root.max_birth, pinned.root.max_birth);
    for root in rewritten.pages.iter().chain([&rewritten.index]) {
        image.region(root);
    }
    let mut reader = image.reader();
    let mut cache = PageCache::new(4);
    assert_eq!(
        rewritten.pinned.get(&mut cache, &mut reader, 1).unwrap(),
        data(9, 0, 1, 11)
    );
    assert_eq!(
        rewritten.pinned.get(&mut cache, &mut reader, 2).unwrap(),
        data(2, 1, 1, 12)
    );
    assert_eq!(
        rewritten
            .pinned
            .get(&mut cache, &mut reader, page + 3)
            .unwrap(),
        data(9, 1, 1, 13)
    );
    // The old pinned root still reads the old addresses (its pages are
    // untouched), and the live map was not changed.
    assert_eq!(
        pinned.get(&mut cache, &mut reader, 1).unwrap(),
        data(2, 0, 1, 11)
    );
    assert_eq!(map.get(&mut reader, 0, 1).unwrap(), data(2, 0, 1, 11));
    // The index entries of untouched pages are the same; max_birth is
    // kept on the rewritten ones.
    assert_eq!(
        rewritten.pinned.index[0].max_birth,
        pinned.index[0].max_birth
    );
    // A change that does not match the page, changes the birth, or names
    // an absent page is refused.
    assert!(matches!(
        map.rewrite_pinned(
            &mut reader,
            image,
            &pinned,
            &[(2, data(2, 1, 1, 99), data(9, 5, 1, 99))]
        ),
        Err(MapError::Internal(_))
    ));
    assert!(matches!(
        map.rewrite_pinned(
            &mut reader,
            image,
            &pinned,
            &[(2, data(2, 1, 1, 12), data(9, 5, 2, 12))]
        ),
        Err(MapError::Internal(_))
    ));
    assert!(matches!(
        map.rewrite_pinned(
            &mut reader,
            image,
            &pinned,
            &[(5, MapEntry::UNMAPPED, data(9, 5, 1, 0))]
        ),
        Err(MapError::Internal(_))
    ));
    assert!(matches!(
        map.rewrite_pinned(
            &mut reader,
            image,
            &pinned,
            &[(page + 20, data(2, 1, 1, 12), data(9, 5, 1, 12))]
        ),
        Err(MapError::OutOfRange { .. })
    ));
    // Nothing to change: a new index region, no pages.
    let same = map
        .rewrite_pinned(&mut reader, image, &pinned, &[])
        .unwrap();
    assert!(same.pages.is_empty());
    assert_eq!(same.pinned.index, pinned.index);
}

// ---------------------------------------------------------------------------
// a random walk
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
enum Op {
    Write { vol: u8, lba: u16, seg: u8 },
    Fold,
    Rollback { vol: u8, pin: u8 },
    Resize { vol: u8, size: u16 },
    Delete { vol: u8 },
    Reboot,
}

fn op() -> impl Strategy<Value = Op> {
    prop_oneof![
        8 => (any::<u8>(), any::<u16>(), any::<u8>()).prop_map(|(vol, lba, seg)| Op::Write { vol, lba, seg }),
        2 => Just(Op::Fold),
        1 => (any::<u8>(), any::<u8>()).prop_map(|(vol, pin)| Op::Rollback { vol, pin }),
        1 => (any::<u8>(), 1..3000u16).prop_map(|(vol, size)| Op::Resize { vol, size }),
        1 => any::<u8>().prop_map(|vol| Op::Delete { vol }),
        1 => Just(Op::Reboot),
    ]
}

/// Three volumes around a page boundary; writes, folds, rollbacks to any
/// earlier pin of the volume, grows and shrinks, deletions with a
/// re-creation at the next CP, and reboots, all checked against the model
/// after every CP.
fn walk(ops: &[Op]) -> Result<(), String> {
    let page = MAP_ENTRIES_PER_PAGE as u64;
    let mut p = Pool::new(&[(0, page + 50), (1, 200), (2, 2 * page)]);
    // Writes waiting for the next CP: `(vol, lba, seg)`; their entries are
    // built when the CP runs, born at that CP like the engine's.
    let mut pending: Vec<(u32, u64, u8)> = Vec::new();
    let mut pending_ops = Ops::default();
    let fail = |i: usize, op: &Op, e: MapError| format!("op {i} {op:?}: {e}");
    let entries = |pending: &[(u32, u64, u8)], cp: u64| -> Vec<(u32, u64, MapEntry)> {
        pending
            .iter()
            .map(|(vol, lba, seg)| {
                let birth = cp as u32;
                let entry = if seg % 7 == 0 {
                    MapEntry::hole(birth).unwrap()
                } else {
                    data(
                        1 + u64::from(*seg),
                        *lba as u32,
                        birth,
                        u32::from(*seg) ^ 77,
                    )
                };
                (*vol, *lba, entry)
            })
            .collect()
    };
    for (i, op) in ops.iter().enumerate() {
        match op {
            Op::Write { vol, lba, seg } => {
                let vol = u32::from(*vol % 3);
                let Some(size) = p.sizes.get(&vol).copied() else {
                    continue;
                };
                // A shrink in this CP bounds the deltas; a rollback or a
                // deletion forbids them.
                let bound = pending_ops
                    .resizes
                    .iter()
                    .find(|(v, _)| *v == vol)
                    .map_or(size, |(_, s)| (*s).min(size));
                if pending_ops.rollbacks.iter().any(|(v, _)| *v == vol)
                    || pending_ops.deletes.contains(&vol)
                {
                    continue;
                }
                let lba = u64::from(*lba) % bound;
                pending.push((vol, lba, *seg));
            }
            Op::Fold => pending_ops.fold = true,
            Op::Rollback { vol, pin } => {
                let vol = u32::from(*vol % 3);
                if !p.sizes.contains_key(&vol)
                    || pending.iter().any(|(v, _, _)| *v == vol)
                    || pending_ops.resizes.iter().any(|(v, _)| *v == vol)
                    || pending_ops.rollbacks.iter().any(|(v, _)| *v == vol)
                    || pending_ops.deletes.contains(&vol)
                {
                    continue;
                }
                let pins: Vec<usize> = (0..p.pins.len())
                    .filter(|k| p.pins[*k].root.vol == vol)
                    .collect();
                if pins.is_empty() {
                    continue;
                }
                let chosen = pins[usize::from(*pin) % pins.len()];
                pending_ops.fold = true;
                pending_ops.rollbacks.push((vol, chosen));
            }
            Op::Resize { vol, size } => {
                let vol = u32::from(*vol % 3);
                if !p.sizes.contains_key(&vol)
                    || pending_ops.resizes.iter().any(|(v, _)| *v == vol)
                    || pending_ops.rollbacks.iter().any(|(v, _)| *v == vol)
                    || pending_ops.deletes.contains(&vol)
                {
                    continue;
                }
                let size = u64::from(*size);
                pending.retain(|(v, lba, _)| *v != vol || *lba < size);
                pending_ops.fold = true;
                pending_ops.resizes.push((vol, size));
            }
            Op::Delete { vol } => {
                let vol = u32::from(*vol % 3);
                if !p.sizes.contains_key(&vol)
                    || pending_ops.resizes.iter().any(|(v, _)| *v == vol)
                    || pending_ops.rollbacks.iter().any(|(v, _)| *v == vol)
                    || pending_ops.deletes.contains(&vol)
                {
                    continue;
                }
                pending.retain(|(v, _, _)| *v != vol);
                pending_ops.fold = true;
                pending_ops.deletes.push(vol);
            }
            Op::Reboot => {
                let deltas = entries(&std::mem::take(&mut pending), p.cp_seq + 1);
                let ops = std::mem::take(&mut pending_ops);
                let deleted = ops.deletes.clone();
                p.cp(deltas, ops).map_err(|e| fail(i, op, e))?;
                p.check_lookups();
                p.check_pins();
                p.reboot();
                p.check_lookups();
                // A deleted volume comes back empty at the next CP, so the
                // walk keeps three volumes.
                if !deleted.is_empty() {
                    p.cp(
                        vec![],
                        Ops {
                            fold: true,
                            new_volumes: deleted
                                .iter()
                                .map(|v| (*v, 100 + u64::from(*v) * 300))
                                .collect(),
                            ..Ops::default()
                        },
                    )
                    .map_err(|e| fail(i, op, e))?;
                    // Pins of the deleted volume are no longer rollback
                    // targets of the new one (a different volume in all but
                    // index): drop them.
                    p.pins.retain(|pin| !deleted.contains(&pin.root.vol));
                    p.check_lookups();
                }
            }
        }
    }
    let deleted = pending_ops.deletes.clone();
    let deltas = entries(&pending, p.cp_seq + 1);
    p.cp(deltas, pending_ops)
        .map_err(|e| format!("final: {e}"))?;
    p.check_lookups();
    p.pins.retain(|pin| !deleted.contains(&pin.root.vol));
    p.check_pins();
    p.reboot();
    p.check_lookups();
    Ok(())
}

proptest! {
    #![proptest_config(ProptestConfig {
        failure_persistence: None,
        max_shrink_iters: 512,
        ..ProptestConfig::default()
    })]

    #[test]
    fn random_writes_rollbacks_resizes_and_deletions_match_the_model(
        ops in prop::collection::vec(op(), 1..40),
    ) {
        if let Err(e) = walk(&ops) {
            prop_assert!(false, "{e}");
        }
    }
}

#[test]
fn page_arithmetic_used_by_the_walk() {
    assert_eq!(MapPage::page_of(MAP_ENTRIES_PER_PAGE as u64), 1);
    assert_eq!(map_pages_for(MAP_ENTRIES_PER_PAGE as u64 + 1), 2);
}
