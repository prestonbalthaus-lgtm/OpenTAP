//! Step 4 QA (Opus-QA-Adversarial): the persistent map attacked on its
//! own, over a byte-vector device. Boundary LBAs at map-page and
//! index-page edges, volumes created and written in the same consistency
//! point, delta logs split into many regions and a directory spanning
//! several pages, prepared consistency points that are abandoned, holes
//! over unmapped blocks, the birth walk at every base, and a model-based
//! property over all of it.
//!
//! The tests that reproduced a defect are regression tests now (their doc
//! comments name the defect in the Step 4 log and say what was wrong and
//! what the engine does now). The others pin properties that hold.

use std::collections::{BTreeMap, BTreeSet};

use otap_format::{
    map_roots_image, parse_map_roots, CpLogDir, MapEntry, MapRoot, RootRef, Uuid, BLOCK_SIZE,
    CP_LOG_DIR_ENTRIES, CP_LOG_RECS_PER_PAGE, MAP_ENTRIES_PER_PAGE, SECTORS_PER_BLOCK,
};
use otap_map::{
    index_root_ref, CpInput, MapError, MapSet, PageCache, PageReader, PinnedRoot, RegionWriter,
    VecReader, VecWriter,
};
use proptest::prelude::*;

const PAGE: u64 = MAP_ENTRIES_PER_PAGE as u64;

fn agg() -> Uuid {
    Uuid::from_bytes([0x5a; 16])
}

/// The device: an append-only image the writer grows and the reader reads.
#[derive(Debug, Clone)]
struct Image {
    bytes: Vec<u8>,
}

impl Image {
    fn new() -> Self {
        Self {
            bytes: vec![0u8; 4 * BLOCK_SIZE],
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

/// A reader over a borrowed image (no copy per check).
struct SliceReader<'a>(&'a [u8]);

impl PageReader for SliceReader<'_> {
    fn read_blocks(&mut self, pba: u64, dst: &mut [u8]) -> Result<(), MapError> {
        let start = pba as usize * BLOCK_SIZE;
        let end = start + dst.len();
        let src = self.0.get(start..end).ok_or(MapError::Io {
            pba,
            bytes: dst.len(),
            reason: "beyond the image".into(),
        })?;
        dst.copy_from_slice(src);
        Ok(())
    }
}

impl RegionWriter for Image {
    fn place(&mut self, bytes: &[u8]) -> Result<RootRef, MapError> {
        let mut w = VecWriter {
            bytes: std::mem::take(&mut self.bytes),
            placed: Vec::new(),
        };
        let out = w.place(bytes);
        self.bytes = w.bytes;
        out
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

fn data(cp: u64, tag: u32) -> MapEntry {
    MapEntry::data(
        1 + cp,
        tag % 60_000,
        SECTORS_PER_BLOCK,
        cp as u32,
        0x1000 + tag,
    )
    .expect("a valid data entry")
}

fn hole(cp: u64) -> MapEntry {
    MapEntry::hole(cp as u32).expect("a valid hole")
}

type Model = BTreeMap<(u32, u64), MapEntry>;

/// A pool: the map, its device, the published roots and directory (what
/// the superblock would name), and a model of every committed entry.
struct Pool {
    map: MapSet,
    image: Image,
    model: Model,
    sizes: BTreeMap<u32, u64>,
    cp: u64,
    region_pages: u32,
    roots: Vec<MapRoot>,
    roots_ref: RootRef,
    dir_ref: RootRef,
    /// Pinned roots from folds: `(fold cp, root, the model of the volume then)`.
    pins: Vec<(u64, PinnedRoot, Model)>,
}

impl Pool {
    fn new(volumes: &[(u32, u64)], region_pages: u32) -> Self {
        let mut pool = Self {
            map: MapSet::new(agg(), 4),
            image: Image::new(),
            model: Model::new(),
            sizes: BTreeMap::new(),
            cp: 0,
            region_pages,
            roots: Vec::new(),
            roots_ref: RootRef::NONE,
            dir_ref: RootRef::NONE,
            pins: Vec::new(),
        };
        pool.cp(Vec::new(), false, volumes.to_vec());
        pool
    }

    fn input(
        &self,
        deltas: Vec<(u32, u64, MapEntry)>,
        fold: bool,
        new: Vec<(u32, u64)>,
    ) -> CpInput {
        CpInput {
            cp_seq: self.cp + 1,
            cp_nonce: 0xC0FF_EE00 + self.cp,
            deltas,
            fold,
            new_volumes: new,
            max_region_pages: self.region_pages,
        }
    }

    /// One consistency point: prepare, the engine's placements (roots
    /// table, directory), apply; the model learns the deltas.
    fn cp(&mut self, deltas: Vec<(u32, u64, MapEntry)>, fold: bool, new: Vec<(u32, u64)>) {
        let input = self.input(deltas.clone(), fold, new.clone());
        let mut reader = self.image.reader();
        let prepared = self
            .map
            .prepare_cp(&mut reader, &input, &mut self.image)
            .expect("prepare");
        for root in &prepared.written {
            self.image.region(root);
        }
        let roots_bytes = map_roots_image(&prepared.roots_table).expect("roots image");
        self.roots_ref = self.image.place(&roots_bytes).expect("roots placed");
        let dir_bytes = prepared.dir.encode(agg()).expect("dir encodes");
        self.dir_ref = self.image.place(&dir_bytes).expect("dir placed");
        self.roots = prepared.roots_table.clone();
        let fold_roots = prepared.fold_roots.clone();
        self.map.apply(prepared.update);
        self.cp += 1;
        for (vol, size) in new {
            self.sizes.insert(vol, size);
        }
        for (vol, lba, e) in deltas {
            self.model.insert((vol, lba), e);
        }
        if fold {
            for (vol, root, index) in fold_roots {
                let snap: Model = self
                    .model
                    .iter()
                    .filter(|((v, _), _)| *v == vol)
                    .map(|(k, e)| (*k, *e))
                    .collect();
                self.pins
                    .push((self.cp, PinnedRoot::from_parts(vol, root, index), snap));
            }
        }
    }

    /// Prepares a CP and drops it, as a CP that fails after its metadata
    /// was laid out: the map must be exactly as it was.
    fn abandoned_cp(&mut self, deltas: Vec<(u32, u64, MapEntry)>, fold: bool) {
        let input = self.input(deltas, fold, Vec::new());
        let mut reader = self.image.reader();
        let mut scratch = self.image.clone();
        let _ = self
            .map
            .prepare_cp(&mut reader, &input, &mut scratch)
            .expect("prepare");
    }

    fn check_lookups(&mut self, lbas: &[(u32, u64)]) {
        let Pool {
            map, image, model, ..
        } = self;
        let mut reader = SliceReader(&image.bytes);
        for &(vol, lba) in lbas {
            let got = map.get(&mut reader, vol, lba).expect("lookup");
            let want = model
                .get(&(vol, lba))
                .copied()
                .unwrap_or(MapEntry::UNMAPPED);
            assert_eq!(got, want, "volume {vol} LBA {lba}");
        }
    }

    fn check_all(&mut self) {
        let all: Vec<(u32, u64)> = self
            .sizes
            .iter()
            .flat_map(|(v, s)| (0..*s).map(move |l| (*v, l)))
            .collect();
        self.check_lookups(&all);
    }

    fn check_diff(&mut self, base: u64) {
        let Pool {
            map,
            image,
            model,
            sizes,
            ..
        } = self;
        let mut reader = SliceReader(&image.bytes);
        for vol in sizes.keys().copied() {
            let mut got = Vec::new();
            map.diff(&mut reader, vol, base, |lba, e| got.push((lba, e)))
                .expect("diff");
            let want: Vec<(u64, MapEntry)> = model
                .iter()
                .filter(|((v, _), e)| *v == vol && u64::from(e.birth_cp) > base)
                .map(|((_, l), e)| (*l, *e))
                .collect();
            assert_eq!(got, want, "diff({base}) of volume {vol}");
        }
    }

    fn check_pins(&mut self, base: u64) {
        let Pool {
            map, image, pins, ..
        } = self;
        let mut reader = SliceReader(&image.bytes);
        let mut cache = PageCache::new(2);
        for (cp, pinned, snap) in pins.iter() {
            let mut walked = Vec::new();
            map.pinned_diff(&mut reader, pinned, base, |lba, e| walked.push((lba, e)))
                .expect("pinned diff");
            let want: Vec<(u64, MapEntry)> = snap
                .iter()
                .filter(|(_, e)| u64::from(e.birth_cp) > base)
                .map(|((_, l), e)| (*l, *e))
                .collect();
            assert_eq!(walked, want, "pin at CP {cp}, base {base}");
            for (lba, e) in snap.iter().map(|((_, l), e)| (*l, *e)).take(64) {
                assert_eq!(pinned.get(&mut cache, &mut reader, lba).unwrap(), e);
            }
        }
    }

    /// A fresh map from the published roots and directory, as a boot
    /// builds it; replaces the live one after checking they agree.
    fn reboot(&mut self) {
        let dir = CpLogDir::decode(&self.image.region(&self.dir_ref), &agg()).expect("dir");
        assert_eq!(dir.cp_seq, self.cp);
        let roots = parse_map_roots(&self.image.region(&self.roots_ref), self.roots.len() as u32)
            .expect("roots");
        let image = &self.image;
        let mut fresh = MapSet::from_roots(agg(), 4, dir.cp_seq, dir.fold_cp_seq, &roots, |r| {
            Ok(image.region(&index_root_ref(r)))
        })
        .expect("roots load");
        let mut cps = BTreeSet::new();
        for delta in &dir.deltas {
            let replayed = fresh.replay(&self.image.region(delta)).expect("replay");
            cps.insert(replayed.cp_seq);
        }
        fresh.set_delta_roots(dir.deltas.clone(), cps.len() as u64);
        assert_eq!(fresh.fold_cp(), self.map.fold_cp());
        assert_eq!(fresh.overlay_entries(), self.map.overlay_entries());
        assert_eq!(
            fresh.log_bytes_since_fold(),
            self.map.log_bytes_since_fold()
        );
        assert_eq!(fresh.cps_since_fold(), self.map.cps_since_fold());
        self.map = fresh;
    }
}

// ---------------------------------------------------------------------------
// Defects
// ---------------------------------------------------------------------------

/// Found by the Step 4 review (defect 7, fixed): a fold drops the records of a volume created by the same CP.
///
/// `MapSet::prepare_cp` accepts deltas for a volume listed in
/// `CpInput::new_volumes` (it adds the new volumes to the sizes it checks
/// against), and a non-fold CP logs them and applies them. A *fold* CP,
/// though, folds only the volumes that existed before it (`self.vols`):
/// the new volume gets its all-absent index, no page is written for its
/// records, and a fold writes no delta region, so the records are in no
/// page, no log and no overlay. The lookup after `apply` says the block was
/// never written, and so does a boot from the published roots. The right
/// behaviour is to fold the new volume's records into pages like any other
/// volume's (or to refuse the input); never to accept and lose them.
#[test]
fn a_fold_keeps_the_records_of_a_volume_created_by_the_same_cp() {
    let mut p = Pool::new(&[(0, 100)], 8);
    let entry = data(2, 7);
    let input = p.input(
        vec![(1, 5, entry), (0, 3, data(2, 8))],
        true,
        vec![(1, 2 * PAGE)],
    );
    let mut reader = p.image.reader();
    match p.map.prepare_cp(&mut reader, &input, &mut p.image) {
        // Refusing the combination would be a correct answer too.
        Err(_) => {}
        Ok(prepared) => {
            p.map.apply(prepared.update);
            let mut reader = p.image.reader();
            assert_eq!(
                p.map.get(&mut reader, 0, 3).unwrap(),
                data(2, 8),
                "the existing volume's record is folded"
            );
            // Before the fix: the new volume's record vanished.
            assert_eq!(
                p.map.get(&mut reader, 1, 5).unwrap(),
                entry,
                "the record of the volume created by the fold CP was dropped"
            );
        }
    }
}

// ---------------------------------------------------------------------------
// Properties that hold (pinned)
// ---------------------------------------------------------------------------

/// The same CP shape without a fold keeps the record: the defect above is
/// specific to the fold path.
#[test]
fn a_non_fold_cp_keeps_the_records_of_a_volume_it_creates() {
    let mut p = Pool::new(&[(0, 100)], 8);
    let entry = data(2, 7);
    p.cp(vec![(1, 5, entry)], false, vec![(1, 2 * PAGE)]);
    p.check_all();
    p.reboot();
    p.check_all();
    p.cp(Vec::new(), true, Vec::new());
    p.check_all();
    p.reboot();
    p.check_all();
}

/// Volumes whose sizes sit on and around map-page and index-page edges,
/// written at their first and last blocks and at every page edge, across
/// CPs, folds and reboots: every lookup matches, `diff` matches at every
/// base, and LBAs one past the end are refused.
#[test]
fn page_and_index_edges_survive_folds_and_reboots() {
    let sizes: [(u32, u64); 7] = [
        (0, 1),
        (1, PAGE - 1),
        (2, PAGE),
        (3, PAGE + 1),
        (4, 3 * PAGE),
        (5, PAGE * PAGE),
        (6, PAGE * PAGE + 1),
    ];
    let mut p = Pool::new(&sizes, 2);
    let mut edges: Vec<(u32, u64)> = Vec::new();
    for (vol, size) in sizes {
        let mut lbas = vec![0, size - 1];
        let mut k = PAGE;
        while k < size && lbas.len() < 12 {
            lbas.extend([k - 1, k]);
            k = if size > 16 * PAGE { k * PAGE } else { k + PAGE };
        }
        for lba in lbas {
            if lba < size {
                edges.push((vol, lba));
            }
        }
    }
    edges.sort_unstable();
    edges.dedup();
    for round in 0..4u64 {
        let cp = p.cp + 1;
        let deltas: Vec<(u32, u64, MapEntry)> = edges
            .iter()
            .enumerate()
            .filter(|(i, _)| (*i as u64 + round).is_multiple_of(2))
            .map(|(i, (v, l))| {
                let e = if (i as u64 + round).is_multiple_of(3) {
                    hole(cp)
                } else {
                    data(cp, i as u32)
                };
                (*v, *l, e)
            })
            .collect();
        p.cp(deltas, round % 2 == 1, Vec::new());
        p.check_lookups(&edges);
        for base in 0..=p.cp {
            p.check_diff(base);
        }
        p.reboot();
        p.check_lookups(&edges);
    }
    let mut reader = p.image.reader();
    for (vol, size) in sizes {
        assert!(matches!(
            p.map.get(&mut reader, vol, size),
            Err(MapError::OutOfRange { .. })
        ));
    }
    p.check_pins(0);
    p.check_pins(2);
}

/// A CP whose records fill hundreds of one-page delta regions, so that
/// the directory spans more than one directory page: the log replays from
/// the directory in order and the boot agrees with the live map; a fold
/// then frees every region and the directory shrinks to one page.
#[test]
fn a_directory_spanning_several_pages_replays_in_order() {
    let vols = [(0u32, 20 * PAGE), (1, 15 * PAGE)];
    let mut p = Pool::new(&vols, 1);
    // Every record a distinct block, so nothing is deduplicated away.
    let records = CP_LOG_RECS_PER_PAGE * (CP_LOG_DIR_ENTRIES + 20);
    let mut deltas = Vec::with_capacity(records);
    for i in 0..records {
        let (vol, lba) = if (i as u64) < vols[0].1 {
            (0, i as u64)
        } else {
            (1, i as u64 - vols[0].1)
        };
        let e = if i % 5 == 0 {
            hole(1)
        } else {
            data(1, i as u32)
        };
        deltas.push((vol, lba, e));
    }
    p.cp(deltas, false, Vec::new());
    let dir = CpLogDir::decode(&p.image.region(&p.dir_ref), &agg()).unwrap();
    assert!(
        dir.deltas.len() > CP_LOG_DIR_ENTRIES,
        "{} delta regions",
        dir.deltas.len()
    );
    assert!(dir.pages_needed() >= 2);
    p.check_all();
    p.reboot();
    p.check_all();
    p.cp(vec![(1, 0, data(3, 1))], false, Vec::new());
    p.reboot();
    p.check_all();
    p.cp(Vec::new(), true, Vec::new());
    let dir = CpLogDir::decode(&p.image.region(&p.dir_ref), &agg()).unwrap();
    assert!(dir.deltas.is_empty() && dir.pages_needed() == 1);
    p.reboot();
    p.check_all();
    p.check_diff(0);
    p.check_diff(1);
}

/// Prepared CPs that are dropped (a CP that fails after its metadata was
/// laid out, folds included) leave the map unchanged: lookups, the log,
/// the overlay and the next CP behave as if they never happened.
#[test]
fn an_abandoned_prepare_changes_nothing() {
    let mut p = Pool::new(&[(0, 3 * PAGE), (2, 40)], 3);
    p.cp(
        vec![(0, 1, data(1, 1)), (2, 39, hole(1))],
        false,
        Vec::new(),
    );
    let before = (
        p.map.fold_cp(),
        p.map.overlay_entries(),
        p.map.delta_roots().to_vec(),
        p.map.log_bytes_since_fold(),
        p.map.cps_since_fold(),
    );
    for fold in [false, true, false, true] {
        let cp = p.cp + 1;
        p.abandoned_cp(
            vec![
                (0, 1, data(cp, 9)),
                (0, 2 * PAGE, hole(cp)),
                (2, 0, data(cp, 3)),
            ],
            fold,
        );
        let after = (
            p.map.fold_cp(),
            p.map.overlay_entries(),
            p.map.delta_roots().to_vec(),
            p.map.log_bytes_since_fold(),
            p.map.cps_since_fold(),
        );
        assert_eq!(after, before, "fold {fold}");
        p.check_all();
    }
    p.cp(vec![(0, 2, data(2, 5))], true, Vec::new());
    p.check_all();
    p.reboot();
    p.check_all();
}

/// A hole punched over a block that was never written is an entry with a
/// birth: lookups return it, `diff` reports it above its base and not at
/// or below, a fold keeps it in the page, and a pinned root reports it.
#[test]
fn a_hole_over_an_unmapped_block_is_a_change_with_a_birth() {
    let mut p = Pool::new(&[(0, 2 * PAGE)], 4);
    p.cp(vec![(0, PAGE + 3, hole(1))], false, Vec::new());
    let mut reader = p.image.reader();
    let e = p.map.get(&mut reader, 0, PAGE + 3).unwrap();
    assert!(e.is_hole() && e.birth_cp == 1);
    p.check_diff(0);
    p.check_diff(1);
    p.cp(Vec::new(), true, Vec::new());
    p.check_all();
    p.check_pins(0);
    let mut reader = p.image.reader();
    let mut walked = Vec::new();
    p.map
        .diff(&mut reader, 0, 0, |lba, e| walked.push((lba, e)))
        .unwrap();
    assert_eq!(walked, vec![(PAGE + 3, hole(1))]);
    p.reboot();
    p.check_all();
}

/// Replay is strict about what a directory could never list: a region of
/// another aggregate, a CP below the fold, a CP that goes backwards, a
/// page position repeated, a record beyond its volume.
#[test]
fn replay_refuses_regions_a_directory_could_not_list() {
    let mut p = Pool::new(&[(0, 64)], 1);
    p.cp(vec![(0, 1, data(1, 1))], false, Vec::new());
    p.cp(vec![(0, 2, data(2, 2))], false, Vec::new());
    let dir = CpLogDir::decode(&p.image.region(&p.dir_ref), &agg()).unwrap();
    let d1 = p.image.region(&dir.deltas[0]);
    let d2 = p.image.region(&dir.deltas[1]);
    let roots = p.roots.clone();
    let image = p.image.clone();
    let boot = |cp: u64, fold: u64| {
        MapSet::from_roots(agg(), 2, cp, fold, &roots, |r| {
            Ok(image.region(&index_root_ref(r)))
        })
        .unwrap()
    };
    // The regions are CP 2's and CP 3's (CP 1 created the volume and
    // logged nothing). A fold at CP 3: CP 2's region is below it.
    let mut m = boot(3, 3);
    assert!(matches!(
        m.replay(&d1),
        Err(MapError::Replay {
            field: "cp_seq",
            ..
        })
    ));
    // The fold CP's own region is skipped, a later one applied.
    let mut m = boot(3, 2);
    let r = m.replay(&d1).unwrap();
    assert_eq!((r.cp_seq, r.applied, r.skipped), (2, 0, 1));
    let r = m.replay(&d2).unwrap();
    assert_eq!((r.cp_seq, r.applied, r.skipped), (3, 1, 0));
    // Backwards.
    let mut m = boot(3, 0);
    m.replay(&d2).unwrap();
    assert!(m.replay(&d1).is_err());
    // A volume shrunk under the record (a root that is too small).
    let mut small = roots.clone();
    small[0].lba_count = 1;
    small[0].index_pages = 1;
    let mut m = MapSet::from_roots(agg(), 2, 3, 0, &small, |r| {
        Ok(p.image.region(&index_root_ref(r)))
    })
    .unwrap();
    assert!(matches!(
        m.replay(&d1),
        Err(MapError::Replay { field: "lba", .. })
    ));
    // A flipped byte in the header or in a used record fails its CRC.
    // (Bytes past the record count are covered only by the region's own
    // reference, which the boot verifies before it replays.)
    for at in [10usize, 70, 90] {
        let mut bad = d1.clone();
        bad[at] ^= 0x40;
        let mut m = boot(3, 0);
        assert!(m.replay(&bad).is_err(), "byte {at}");
    }
}

/// The page cache keeps at least one page, evicts the oldest insert, and a
/// map over a one-page cache still answers every lookup across pages.
#[test]
fn a_one_page_cache_still_answers_across_pages() {
    let mut cache = PageCache::new(0);
    cache.insert(7, Box::new(otap_format::MapPage::zeroed()));
    assert_eq!(cache.len(), 1);
    cache.insert(8, Box::new(otap_format::MapPage::zeroed()));
    assert_eq!(cache.len(), 1);
    assert!(cache.get(7).is_none() && cache.get(8).is_some());
    let mut p = Pool::new(&[(0, 4 * PAGE)], 8);
    p.map = MapSet::new(agg(), 0);
    p.cp = 0;
    p.sizes.clear();
    p.model.clear();
    p.pins.clear();
    p.cp(Vec::new(), false, vec![(0, 4 * PAGE)]);
    let deltas = (0..4u64)
        .map(|k| (0, k * PAGE + k, data(2, k as u32)))
        .collect();
    p.cp(deltas, true, Vec::new());
    for _ in 0..3 {
        p.check_all();
    }
    let (_, misses) = p.map.cache_counters();
    assert!(misses >= 4, "{misses} misses through a one-page cache");
}

/// An entry that keeps an older birth than the CP logging it (what a
/// relocation will log) replaces the page's newer entry: lookups return
/// it, `diff` above its birth does not report the block (nor the page's
/// overridden entry), the log replays it by the region's CP, and a fold
/// puts it in the page. A record born after its region's CP is refused.
#[test]
fn an_entry_with_an_older_birth_overrides_the_page_and_replays_by_region() {
    let mut p = Pool::new(&[(0, 2 * PAGE)], 2);
    p.cp(
        vec![(0, 5, data(2, 1)), (0, PAGE, data(2, 2))],
        true,
        Vec::new(),
    );
    // CP 3 logs block 5 with birth 1 (older than the page's birth 2).
    let relocated = MapEntry::data(77, 9, SECTORS_PER_BLOCK, 1, 0x77).unwrap();
    p.cp(vec![(0, 5, relocated)], false, Vec::new());
    p.check_all();
    for base in 0..=3 {
        p.check_diff(base);
    }
    let mut reader = p.image.reader();
    let mut above_one = Vec::new();
    p.map
        .diff(&mut reader, 0, 1, |lba, _| above_one.push(lba))
        .unwrap();
    assert_eq!(above_one, vec![PAGE], "block 5's page entry is overridden");
    p.reboot();
    p.check_all();
    p.cp(Vec::new(), true, Vec::new());
    p.check_all();
    p.reboot();
    p.check_all();
    // A record born after the CP that logs it cannot come from a directory.
    let input = p.input(vec![(0, 7, data(9, 3))], false, Vec::new());
    let mut reader = p.image.reader();
    assert!(p
        .map
        .prepare_cp(&mut reader, &input, &mut p.image.clone())
        .is_err());
}

// ---------------------------------------------------------------------------
// The property
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
enum Op {
    /// Writes and punches `(vol, lba, hole)` in one CP, maybe a fold.
    Cp {
        writes: Vec<(u8, u16, bool)>,
        fold: bool,
    },
    /// A CP prepared and dropped.
    Abandon {
        writes: Vec<(u8, u16, bool)>,
        fold: bool,
    },
    /// A volume created by a CP of its own (no records in it).
    NewVolume(u8, u16),
    Reboot,
}

fn op() -> impl Strategy<Value = Op> {
    let writes = prop::collection::vec((0u8..3, any::<u16>(), any::<bool>()), 0..10);
    prop_oneof![
        6 => (writes.clone(), any::<bool>()).prop_map(|(writes, fold)| Op::Cp { writes, fold }),
        1 => (writes, any::<bool>()).prop_map(|(writes, fold)| Op::Abandon { writes, fold }),
        1 => (1u8..3, 1u16..2200).prop_map(|(v, s)| Op::NewVolume(v, s)),
        2 => Just(Op::Reboot),
    ]
}

fn deltas_of(p: &Pool, writes: &[(u8, u16, bool)]) -> Vec<(u32, u64, MapEntry)> {
    let cp = p.cp + 1;
    writes
        .iter()
        .enumerate()
        .filter_map(|(i, (v, l, h))| {
            let vol = u32::from(*v);
            let size = *p.sizes.get(&vol)?;
            let lba = u64::from(*l) % size;
            let e = if *h { hole(cp) } else { data(cp, i as u32) };
            Some((vol, lba, e))
        })
        .collect()
}

proptest! {
    #![proptest_config(ProptestConfig {
        failure_persistence: None,
        ..ProptestConfig::default()
    })]

    /// Random CPs (data and holes, folds or not, regions of one page),
    /// abandoned CPs, volumes created along the way and reboots: every
    /// lookup, every `diff` at a random base and every pinned walk matches
    /// the model.
    #[test]
    fn the_map_matches_a_model_through_regions_folds_abandons_and_reboots(
        ops in prop::collection::vec(op(), 1..20),
        base_seed in any::<u64>(),
    ) {
        let mut p = Pool::new(&[(0, 2 * PAGE + 17)], 1);
        for (step, op) in ops.into_iter().enumerate() {
            // Every block after a fold or a reboot; otherwise the blocks the
            // step touched and a sample of the rest.
            let mut touched: Vec<(u32, u64)> = Vec::new();
            let mut full = false;
            match op {
                Op::Cp { writes, fold } => {
                    let deltas = deltas_of(&p, &writes);
                    touched.extend(deltas.iter().map(|(v, l, _)| (*v, *l)));
                    full = fold;
                    p.cp(deltas, fold, Vec::new());
                }
                Op::Abandon { writes, fold } => {
                    let deltas = deltas_of(&p, &writes);
                    touched.extend(deltas.iter().map(|(v, l, _)| (*v, *l)));
                    full = fold;
                    p.abandoned_cp(deltas, fold);
                }
                Op::NewVolume(v, s) => {
                    let vol = u32::from(v);
                    if !p.sizes.contains_key(&vol) {
                        p.cp(Vec::new(), false, vec![(vol, u64::from(s))]);
                    }
                }
                Op::Reboot => {
                    p.reboot();
                    full = true;
                }
            }
            if full {
                p.check_all();
            } else {
                let mut x = base_seed ^ step as u64;
                for (vol, size) in p.sizes.clone() {
                    for _ in 0..32 {
                        x = x.wrapping_mul(6_364_136_223_846_793_005).wrapping_add(1);
                        touched.push((vol, (x >> 17) % size));
                    }
                }
                p.check_lookups(&touched);
            }
            let base = (base_seed.rotate_left(step as u32 * 7)) % (p.cp + 2);
            p.check_diff(base);
            p.check_pins(base);
        }
        p.reboot();
        p.check_all();
        p.check_diff(0);
        p.check_pins(0);
    }
}
