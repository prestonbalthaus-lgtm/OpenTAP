# `otap-map` for Step 6: the persistent map as the attestation (B) and the sender (C) will use it

Reader's map, written 2026-10-14 from the sources at `E:/Coding/OpenTAP` (Step 5 commit `5114866` state). Read-only: nothing was built or run; every claim below cites a file and line, and where the code and a document disagree the text says so.

Paths are repository-relative: `map` = `crates/otap-map/src/lib.rs`, `fmt/map` = `crates/otap-format/src/map.rs`, `fmt/records` = `crates/otap-format/src/records.rs`, `fmt/types` = `crates/otap-format/src/types.rs`, `fmt/constants` = `crates/otap-format/src/constants.rs`, `engine` = `crates/otap-cp/src/engine.rs`, `boot` = `crates/otap-cp/src/boot.rs`.

---

## 1. Scope: what was read

| File | Lines | Why |
|---|---|---|
| `crates/otap-map/src/lib.rs` | 1–1942 (all) | the crate |
| `crates/otap-map/tests/map_tests.rs` | 1–700 (all) | the model harness, the property, damage tests |
| `crates/otap-map/tests/review_map_tests.rs` | 1–809 (all) | the Step 4 review suite: edges, regions, abandons, older births |
| `crates/otap-map/tests/lifecycle_map_tests.rs` | 1–1019 (all) | Step 5: rollback, resize, delete, `rewrite_pinned`, the random walk |
| `crates/otap-format/src/map.rs` | 1–596 (all) | `MapEntry`, `MapPage`, `PageIndexEntry`, `PageIndexPage`, `MapRoot`, the roots table |
| `crates/otap-format/src/lib.rs` | 1–300 (all) | the F-rulings (F2, F5, F9, F10, F12–F14, F17, F18) |
| `crates/otap-format/src/constants.rs` | 1–241 (all) | widths and sizes |
| `crates/otap-format/src/records.rs` | 480–1180, struct bodies of `SnapRecord` (359), `SegEntry` (129) | `CpLogRec`, `CpLogPage`, `CpLogDir`, `SnapRootPage` |
| `crates/otap-format/src/types.rs` | 15–120, 470–600 | `FormatError`, `RootRef` |
| `crates/otap-format/src/stream.rs` | `HoleRecord`, `FrameKind` | what the sender must produce from the walk |
| `crates/otap-cp/src/engine.rs` | 915–1000, 1060–1090, 1345–1375, 1660–1700, 1775–1830, 1895–1912, 2795–3060, 3278–3296, 3630–3730, 4840–5180, 5250–5410, 6075–6110, 6170–6200 | every call into the map: `audit_liveness`, `snapshot_read/diff`, `live_diff`, `prepare_meta`'s map part, the liveness rules, `pinned_by` |
| `crates/otap-cp/src/boot.rs` | 385–533 (`walk_cp`), 538–640, 690–760, 774–796 | what the boot already verifies; `load_snapshot`; `read_root`; metadata liveness at boot |
| `crates/otap-cp/src/layout.rs` | `block_pba` (203–211) | `lseg` is the physical segment today |
| `crates/otap-sim/src/harness.rs` | 729–790 (`verify_snapshots`), 787–908 (`check_diff`), 1104–1117 (`audit_liveness`) | the oracle patterns |
| `docs/07-engineering-decisions.md` | E09 (205), E22 (618), E23 (670), E24 (704), E25 (740), E31 (938), E32 (980), E36 (1152), E39 (1428) | the rulings in force |
| `docs/06-implementation-status.md` | 100–110, 427–562 | the map's status row, Step 6 plan, known limits |
| `docs/10-phase-c-step-briefs.md` | 578–627 | the Step 6 brief as received (truncated at the transport's backends) |
| `docs/08-engineering-log.md` | 940–960; `docs/reviews/2026-10-14-step5-qa-adversarial.md` 318–335 | the `PROPTEST_CASES` ruling (C1) |

Line counts: `lib.rs` 1,942; the three test files 2,528; total 4,470 (the "about 4,500 lines" of the assignment is the crate plus its tests).

---

## 2. Public API and key internals

### 2.1 `otap-format` (`fmt/map`): the on-disk shapes the map is made of

Every structure is `#[repr(C)]`, little-endian, zerocopy; headerless pages carry their CRC in the parent (crate doc convention 3, `crates/otap-format/src/lib.rs:22-27`).

**Constants** (`fmt/constants`): `BLOCK_SIZE = 4096` (:27); `SECTORS_PER_BLOCK = 8` (:33); `MAX_CP_SEQ = u32::MAX` (:40); `MAX_VOLS = 1024` (:54), `MAX_VOL = 1023` (:56); `MAP_ENTRY_SIZE = 16` (:107); `MAP_ENTRIES_PER_PAGE = 1024` (:109); `MAP_PAGE_SIZE = 16384` (:111); `MAP_PAGE_SHIFT = 10` (:113); `INDEX_ENTRY_SIZE = 16`, `INDEX_ENTRIES_PER_PAGE = 1024`, `INDEX_PAGE_SIZE = 16384`, `INDEX_PAGE_SHIFT = 10` (:115-121); `MAP_ROOT_SIZE = 64` (:123); `CP_LOG_REC_SIZE = 32`, `CP_LOG_PAGE_HDR_SIZE = 64`, `CP_LOG_PAGE_SIZE = 4096`, `CP_LOG_RECS_PER_PAGE = 126` (:143-149); `CP_LOG_DIR_ENTRIES = 252` (:155); `MAX_LBA = 2^40 - 1` (:200); `LSEG_BITS = 36`, `OFF_BITS = 16`, `PLEN_BITS = 12` (:203-207); `MAX_LSEG = 2^36 - 1` (:209); `MAX_SEG_OFF = 65535` (:211); `MAX_PLEN = 4095` (:215); `NULL_LSEG = 0` (:217). Shifts: `MAP_LSEG_SHIFT = 28`, `MAP_OFF_SHIFT = 12` (`fmt/map:45-47`, asserted :508-510).

**`MapEntry`** (`fmt/map:54-61`), 16 bytes: `addr: u64` (`lseg:36 | off:16 | plen:12`, or 0), `birth_cp: u32`, `crc32c: u32`.
- `MapEntry::UNMAPPED` = all zero (:65-69): never written, no birth.
- `pack_addr(lseg, off, plen) -> Result<u64, FormatError>` (:73-102): refuses `lseg == 0` (`BadField{lseg}`), `lseg > MAX_LSEG`, `off > MAX_SEG_OFF`, `plen == 0`, `plen > SECTORS_PER_BLOCK` (`OutOfRange`).
- `data(lseg, off, plen, birth_cp, crc32c)` (:105-124): `birth_cp >= 1`.
- `hole(birth_cp)` (:128-137): `addr 0, crc 0, birth >= 1`.
- `lseg() = addr >> 28`, `off() = (addr >> 12) & 0xFFFF`, `plen() = addr & 0xFFF` (:141-155).
- `is_unmapped() = addr == 0 && birth == 0` (:159); `is_hole() = addr == 0 && birth != 0` (:165); `is_data() = addr != 0` (:171). The three are exhaustive and disjoint.
- `validate()` (:177-192): data: `lseg != 0`, `1 <= plen <= 8`, `birth != 0`; non-data: `crc32c == 0`. It does **not** bound `off` against the pool geometry or `lseg` against the segment count, and does not bound `birth_cp` against any CP: those are the caller's (the attestation's) checks.
- Meaning of the fields (F2, `otap-format/src/lib.rs:82-92`; `fmt/map:20-29`): `lseg` is the *logical* segment, resolved through the segment table; **today `lseg == pseg`**: the engine builds entries with `MapEntry::data(pseg, off, SECTORS_PER_BLOCK, ..)` (`engine:4507`, `:4583`, `crates/otap-cp/src/cleaner.rs:232`) and resolves them with `layout.block_pba(entry.lseg(), entry.off())` (`engine:3718-3720`, `layout.rs:203-211`), and `SegState::Relocated`/`reloc_index` stay reserved (F18, `lib.rs:227-228`). `off` is the 4 KiB block offset within the segment. `plen` is the physical length in 512-byte sectors; v2 writes `1..=8` and the engine always writes exactly 8 (every data entry in the engine passes `SECTORS_PER_BLOCK`); the 12-bit width is the compression reservation and on disk every block still occupies a 4 KiB slot. `crc32c` is the CRC-32C of the 4 KiB block, checked on every read (`engine:3725`); 0 for holes and unmapped (F9).

**`MapPage`** (`fmt/map:198-201`), 16 KiB, `align(4096)`: `entries: [MapEntry; 1024]`, entry `i` = LBA `page_no * 1024 + i`.
- `zeroed()` (:206); `crc32c()` over the whole page (:212); `max_birth()` = max of `birth_cp` over the 1024 entries, 0 for an all-unmapped page (:218-220); `index_entry(page_pba) -> PageIndexEntry` (:224-226).
- `page_of(lba) = lba >> 10` (:230), `slot_of(lba) = lba & 1023` (:236), `first_lba(page_no) = page_no << 10` (:242).
- `validate()` every entry (:247); `verify(&self, entry: &PageIndexEntry)` (:254-264): refuses `entry.is_none()` (`BadField{page_pba,0}`), then `check_crc("map page", page.crc32c(), entry.page_crc)` (`BadCrc{region:"map page"}`), then `validate()`, then `max_birth() == entry.max_birth` (`BadField{max_birth}`). `parse(bytes, entry)` zero-copy view (needs 4096 alignment) (:267); `read(bytes, entry) -> MapPage` copy from any alignment (:274-278), which is what the map crate uses.

**`PageIndexEntry`** (`fmt/map:286-295`), 16 bytes: `page_pba: u64` (0 = no page, every block of the 4 MiB unmapped; PBA 0 is superblock `A0`, never a page), `max_birth: u32`, `page_crc: u32`.
- `NONE` (:299), `new(page_pba, max_birth, page_crc)` (:307), `is_none() = page_pba == 0` (:317), `validate()` (:322-327): an absent entry must carry `max_birth == 0 && page_crc == 0`.

**`PageIndexPage`** (`fmt/map:333-375`), 16 KiB: `entries: [PageIndexEntry; 1024]`; `index_page_of(p) = p >> 10`, `slot_of(p) = p & 1023` (:361-369). The map crate never uses it as a type at read time: it flattens the whole index region into a `Vec<PageIndexEntry>` (see `index_entries`, below).

**`MapRoot`** (`fmt/map:383-404`), 64 bytes: `vol: u32`, `flags: u32` (reserved), `index_pba: u64`, `index_pages: u32` (in 16 KiB index pages), `index_crc: u32` (over the whole index region), `lba_count: u64`, `checkpoint_cp_seq: u64`, `max_birth: u32`, `rsvd0`, `rsvd1: [u8;16]`.
- `map_pages_for(lba_count) = ceil(lba_count / 1024)` (:409); `index_pages_for = ceil(map_pages / 1024)` (:415).
- `validate()` (:421-447): `vol <= 1023`, `lba_count <= MAX_LBA + 1`, `checkpoint_cp_seq <= MAX_CP_SEQ`, `index_pages == index_pages_for(lba_count)`. Not checked: `index_pba`, `max_birth`, `flags`.
- `index_bytes() = index_pages * 16384` (:451); `verify_index(&self, index: &[u8])` (:458-471): `validate()`, length `== index_bytes()` (`BadLength`), `check_crc("page index", crc32c(index), index_crc)`, then every `PageIndexEntry::validate()`. Needs an 8-aligned buffer (a `Vec<u8>` is).
- `ABSENT` all-zero (:515-526); `is_absent() = lba_count == 0` (:531).
- `map_roots_image(&[MapRoot]) -> Vec<u8>` (:540-557): one slot per volume index, absent slots exactly `ABSENT`, present ones validate and sit at `i == root.vol`, padded to whole 4 KiB pages, at least one page. `parse_map_roots(bytes, vol_count)` (:562-596): the inverse, and only zero bytes may follow the table.

### 2.2 `otap-format` (`fmt/records`, `fmt/types`): the journal and the references

**`RootRef`** (`fmt/types:479-486`), 16 bytes: `pba: u64`, `pages: u32` (4 KiB pages; 0 = none), `crc32c: u32` over the whole region. `NONE` (:490), `new` (:498), `for_region(pba, bytes)` (:503-513, whole pages), `is_none()` (:517), `bytes()` (:523), `verify(&self, region)` (:529-538): exact length then `check_crc("root region", ..)`.

**`FormatError`** (`fmt/types:23-91`, `#[non_exhaustive]`): `BadMagic{expected,found}`, `BadVersion{expected_major,found}`, `NewerFormat{found}`, `BadCrc{region: &'static str, computed, stored}` (:46), `BadLength{expected,found}` (:55), `Misaligned{required}`, `BadField{field: &'static str, value: u64}` (:68), `OutOfRange{field,value,max}` (:75), `IdentityMismatch{field}` (:85), `NoValidSlot`.

**`CpLogRec`** (`fmt/records:486-499`), 32 bytes: `lba: u64`, `addr: u64`, `vol: u32`, `birth_cp: u32`, `block_crc32c: u32`, `crc32c: u32` (over bytes 0..28). `from_entry(vol, lba, MapEntry)` (:503-515) validates and seals; `map_entry()` (:541-547) is the inverse; `validate()` (:563-583): `vol <= MAX_VOL`, `lba <= MAX_LBA`, the entry is data or hole (an unmapped record is refused: "a CP log never records nothing happened"), then `MapEntry::validate`; `verify()` adds the CRC (:586).

**`CpLogPage`** (`fmt/records:654-659`), 4 KiB: `hdr: CpLogPageHdr` (magic `OTCL`, `ver`, `count: u16`, `cp_seq: u64`, `page_seq: u64`, `agg_id`, `cp_nonce: u64`, `crc32c` over 0..60; :595-614) + `recs: [CpLogRec; 126]`. `new(agg_id, cp_seq, cp_nonce, page_seq)` (:664), `push(rec) -> Option<usize>` (:676, `None` when full), `records()` (:688), `seal()` (:699), `verify()` = header + every used record (:705), `read(bytes)` (:718).

**`CpLogDir`** (`fmt/records:995-1006`): `cp_seq`, `fold_cp_seq`, `deltas: Vec<RootRef>` (delta regions of CPs `fold_cp_seq..=cp_seq` in CP order), `pages: Vec<RootRef>` (map pages this CP wrote: a fold's, and since 2.3 a relocation's rewritten snapshot pages, F18). `encode(agg_id)` (:1010-1050) into `CpLogDirPage`s (252 refs each, header `cp_seq`, `fold_cp_seq`, `delta_entries`, `page_entries`, `page_seq`; :831-859); `decode(bytes, &agg_id)` (:1056-1107) checks every page, identity, position, totals; `pages_needed()` (:1111).

**`SnapRecord`** (`fmt/records:359-384`), 64 bytes: `snap_id: u64` (0 = empty), `snap_uuid`, `map_root_pba: u64` (the `SnapRootPage`), `deadlist_pba` (0, unused: E36 rejected deadlists), `created_wall_ns`, `birth_cp: u32`, `vol: u32`, `kind`, `origin`, `flags` (`PIN_*` bits incl. `PIN_REPLICATION`), `rsvd`, `crc32c`.

**`SnapRootPage`** (`fmt/records:1123-1140`), 4 KiB, own trailing CRC: `magic OTSR`, `ver`, `snap_id`, `agg_id`, `birth_cp: u64`, `root: MapRoot`, `deadlist: RootRef`, reserved (3,972 bytes). `new(agg_id, snap_id, birth_cp, root, deadlist)` (:1149-1166) validates and seals.

**`SegEntry`** (`fmt/records:129-150`), 32 bytes: `phys`, `stripe_data_bytes`, `live_bytes: u32` ("bytes still referenced by a live or pinned root, for the cleaner"), `birth_cp`, `lap`, `kind`, `state`, `rsvd`, `crc32c`. This is the number scope (B) item 4 reconciles.

### 2.3 `otap-map` constants, errors, traits, cache (`map:64-243`)

- `MAP_PAGE_BLOCKS: u32 = 4`, `INDEX_PAGE_BLOCKS: u32 = 4` (:65-67, asserted :1938-1942); `DEFAULT_CACHE_PAGES = 1024` (16 MiB) (:69).
- `MapError` (:71-124, `#[non_exhaustive]`): `Format(FormatError)` (:76; `From<FormatError>` :148-152); `Io{pba: u64, bytes: usize, reason: String}` (:78-85, what a `PageReader`/`RegionWriter` said); `NoSuchVolume{vol}` (:87); `VolumeExists{vol}` (:92); `OutOfRange{vol, lba}` (:97); `BadSize{vol, lba_count}` (:105); `Replay{field: &'static str, value: u64}` (:113); `NoSpace` (:121, "no free metadata segment", the writer's); `Internal(String)` (:123). `Display` :126-144.
- `trait PageReader { fn read_blocks(&mut self, pba: u64, dst: &mut [u8]) -> Result<(), MapError>; }` (:156-159): `dst.len()` whole 4 KiB blocks at block `pba`.
- `trait RegionWriter { fn place(&mut self, bytes: &[u8]) -> Result<RootRef, MapError>; }` (:165-168): places a whole-page region and names it exactly.
- `PageCache` (:174-243): `HashMap<u64, Box<MapPage>>` keyed by **page pba** + a FIFO `VecDeque<u64>` of insertion order, `cap >= 1`, hit/miss counters. `new(cap)` (:185), `get(&mut self, pba) -> Option<&MapPage>` (:197, counts a hit or a miss), `insert(pba, Box<MapPage>)` (:212-224, evicts the oldest *insert*, not the least recently used), `len`, `is_empty`, `counters() -> (hits, misses)` (:240). Pages are immutable once written (a fold writes new pages, never rewrites one; :170-172), so an entry is never stale.
- `fn load_page<'c>(cache: &'c mut PageCache, reader: &mut dyn PageReader, entry: &PageIndexEntry) -> Result<&'c MapPage, MapError>` (:246-267, private): `Internal("loading an absent page")` if `entry.is_none()`; on a cache miss reads `MAP_PAGE_SIZE` bytes at `entry.page_pba` and `MapPage::read(&bytes, entry)?` (CRC, entries, `max_birth`); **a page that fails verification is not cached**, so every retry re-reads it.

### 2.4 `otap-map` free functions (`map:269-330`, `:1865-1872`)

- `pub fn map_pages_for(lba_count: u64) -> usize` (:271).
- `pub fn index_image(entries: &[PageIndexEntry]) -> Vec<u8>` (:278-292): whole index pages (at least one), absent entries zero.
- `fn index_entries(root: &MapRoot, bytes: &[u8]) -> Result<Vec<PageIndexEntry>, MapError>` (:295-314, private): `root.verify_index(bytes)?`, flatten to `[PageIndexEntry]`, require `len >= map_pages_for(root.lba_count)` (`Internal`), require every entry **beyond** `map_pages` to be `NONE` (`Format(BadField{page_pba})`), return the first `map_pages` entries. This is the only index parser; `PinnedRoot::load` and `MapSet::from_roots` go through it.
- `pub fn index_root_ref(root: &MapRoot) -> RootRef` (:318-324) = `(root.index_pba, root.index_pages * 4, root.index_crc)`.
- `pub fn page_root_ref(entry: &PageIndexEntry) -> RootRef` (:328-330) = `(entry.page_pba, 4, entry.page_crc)`.
- `pub fn volume_regions(v: &VolumeMap) -> Vec<RootRef>` (:1868-1872): the index region then every non-`NONE` page. (The boot imports it and discards it: `boot:752 let _ = volume_regions;` — `walk_cp` builds the same list by hand at `boot:405-422`.)

### 2.5 `VolumeMap` (`map:335-399`)

```rust
pub struct VolumeMap { root: MapRoot, index: Vec<PageIndexEntry>, overlay: BTreeMap<u64, MapEntry> }
```
- `fn from_root(root, index_bytes) -> Result<Self>` (:342-356, private): `root.validate()`, refuses absent (`BadSize{vol, 0}`), `index_entries`.
- `pub const fn root(&self) -> &MapRoot` (:360); `pub fn index(&self) -> &[PageIndexEntry]` (:366, exactly `map_pages_for(lba_count)` entries); `pub const fn lba_count(&self) -> u64` (:372); `pub fn overlay_len(&self) -> usize` (:378); `pub fn overlay(&self) -> impl Iterator<Item = (u64, MapEntry)>` in LBA order (:383); `pub fn dirty_pages(&self) -> BTreeSet<u64>` = `page_of` of every overlay key (:389).
- The overlay holds the committed entries of CPs in `(fold_cp, committed_cp]`; each of them is also on disk in a delta region the directory lists (see 4.3). It is a cache of the replayed log, nothing more.

### 2.6 The CP transaction types (`map:401-570`)

- `Replaced { vol: u32, lba: u64, old: MapEntry, new: MapEntry }` (:404-413): `old` is `UNMAPPED` for a first write or for a damaged page.
- `FreedPage { vol, page_no: u64, root: RootRef }` (:417-424); `FreedIndex { vol, root: RootRef }` (:428-433).
- `Rollback { vol, birth_cp: u64, root: MapRoot, index: Vec<PageIndexEntry> }` (:439-448): the snapshot's CP and tree.
- `CpInput` (:452-475, `Default`): `cp_seq`, `cp_nonce`, `deltas: Vec<(u32, u64, MapEntry)>` (data or hole, any order, last wins), `fold: bool`, `new_volumes: Vec<(u32, u64)>`, `max_region_pages: u32`, `rollbacks: Vec<Rollback>`, `resizes: Vec<(u32, u64)>`, `deletes: Vec<u32>`.
- `MapUpdate` (:479-503, opaque): `cp_seq()`, `is_fold()`.
- `PreparedCp` (:509-544): `update: MapUpdate`; `dir: CpLogDir`; `roots_table: Vec<MapRoot>`; `replaced: Vec<Replaced>`; `written: Vec<RootRef>` (deltas, fold pages, index regions, new volumes' indexes); `freed_pages: Vec<FreedPage>`; `freed_indexes: Vec<FreedIndex>`; `freed_deltas: Vec<RootRef>`; `fold_roots: Vec<(u32, MapRoot, Vec<PageIndexEntry>)>` (after a fold: every volume's complete tree, "what a snapshot pins"); `damaged_pages: Vec<(u32, u64)>` (pages that did not verify while the CP looked up replaced entries or walked a rollback/shrink/deletion); `released: Vec<(u32, u64, MapEntry)>` (entries a rollback, shrink or deletion stops referencing).
- `Replayed { cp_seq, records: Vec<CpLogRec>, applied, skipped }` (:548-557); `MapStats { page_reads, folds, pages_written, records_written }` (:561-570).

### 2.7 `MapSet` (`map:574-1735`)

```rust
pub struct MapSet { agg_id: Uuid, fold_cp: u64, committed_cp: u64, vols: BTreeMap<u32, VolumeMap>,
    cache: PageCache, delta_roots: Vec<RootRef>, log_bytes: u64, cps_since_fold: u64,
    last_replayed: Option<(u64, u64)>, stats: MapStats }
```
Construction and boot:
- `pub fn new(agg_id: Uuid, cache_pages: usize) -> Self` (:595-608): empty, CP 0 is the fold.
- `pub fn from_roots(agg_id, cache_pages, cp_seq: u64, fold_cp: u64, roots: &[MapRoot], load_index: impl FnMut(&MapRoot) -> Result<Vec<u8>, MapError>) -> Result<Self, MapError>` (:614-646): `fold_cp > cp_seq` → `Replay{fold_cp_seq}`; absent roots skipped; `root.vol != i` → `Format(BadField{vol})`; each index loaded and verified through `VolumeMap::from_root`.
- `pub fn parse_delta(bytes, agg_id) -> Result<(u64, Vec<CpLogRec>), MapError>` (:735-774, associated, no `self`): verifies every page, one `cp_seq`, consecutive `page_seq`.
- `pub fn replay(&mut self, bytes: &[u8]) -> Result<Replayed, MapError>` (:782-864): see 4.3.
- `pub fn set_delta_roots(&mut self, roots: Vec<RootRef>, cps_since_fold: u64)` (:868-871).

Accessors: `agg_id()` (:650), `fold_cp()` (:656), `committed_cp()` (:662), `volumes() -> impl Iterator<Item = (u32, &VolumeMap)>` in volume order (:667), `volume(vol) -> Option<&VolumeMap>` (:673), `delta_roots() -> &[RootRef]` (:680), `log_bytes_since_fold()` (:686), `cps_since_fold()` (:693), `dirty_pages() -> usize` (:699), `overlay_entries() -> usize` (:705), `stats()` (:711), `cache_counters()` (:717), `roots_table() -> Vec<MapRoot>` (:724-731, one slot per index up to the highest present volume).

Lookup and the walks:
- `pub fn get(&mut self, reader: &mut dyn PageReader, vol: u32, lba: u64) -> Result<MapEntry, MapError>` (:875-892): overlay first, then the page the index names (verified), else `UNMAPPED`.
- `fn page(&mut self, reader, entry: &PageIndexEntry) -> Result<&MapPage, MapError>` (:895-904, private): counts `stats.page_reads` on a miss, then `load_page`.
- `pub fn diff(&mut self, reader, vol, base_cp: u64, f: impl FnMut(u64, MapEntry)) -> Result<(), MapError>` (:1620-1673): the live tree merged with the overlay; see 4.5.
- `pub fn pin(&self, vol) -> Result<PinnedRoot, MapError>` (:1677-1690): refuses a volume with a non-empty overlay (`Internal("... pin after a fold")`); returns a clone of the live root and index.
- `pub fn pinned_get(&mut self, reader, pinned: &PinnedRoot, lba) -> Result<MapEntry, MapError>` (:1693-1711): through this map's cache; `OutOfRange` beyond `pinned.root.lba_count`.
- `pub fn pinned_diff(&mut self, reader, pinned: &PinnedRoot, base_cp, f) -> Result<(), MapError>` (:1714-1734): see 4.6.

The transaction:
- `pub fn prepare_cp(&mut self, reader, input: &CpInput, writer: &mut dyn RegionWriter) -> Result<PreparedCp, MapError>` (:909-1401): see 4.2. Changes nothing in `self` except the cache and `stats`.
- `pub fn apply(&mut self, update: MapUpdate)` (:1568-1615): cannot fail; see 4.2 step 20.
- `fn collect_entries(&mut self, reader, vol, from_lba, base_cp) -> Result<(Vec<(u64, MapEntry)>, Vec<u64>), MapError>` (:1407-1456, private, *tolerant*): see 4.4.
- `pub fn rewrite_pinned(&mut self, reader, writer, pinned: &PinnedRoot, changes: &[(u64, MapEntry, MapEntry)]) -> Result<RewrittenRoot, MapError>` (:1465-1557): see 4.8.
- `pub fn cache_pages(&mut self, pages: Vec<(u64, Box<MapPage>)>)` (:1561-1565).

### 2.8 `PinnedRoot`, `RewrittenRoot` (`map:1737-1863`)

```rust
pub struct PinnedRoot { pub vol: u32, pub root: MapRoot, pub index: Vec<PageIndexEntry> }   // :1759-1766
```
- `pub fn load(root: MapRoot, index_bytes: &[u8]) -> Result<Self, MapError>` (:1771-1785): `validate`, refuse absent, `index_entries`.
- `pub fn from_parts(vol, root, index) -> Self` (:1789-1791): no verification (the engine uses it for a fold's `fold_roots`, `engine:5161`, and for `rewrite_pinned`'s result).
- `pub fn get(&self, cache: &mut PageCache, reader, lba) -> Result<MapEntry, MapError>` (:1794-1809): its own cache; `OutOfRange` past `root.lba_count`.
- `pub fn diff(&self, cache: &mut PageCache, reader, base_cp, f: impl FnMut(u64, MapEntry)) -> Result<(), MapError>` (:1814-1834): same walk as `MapSet::pinned_diff` with a caller's cache.
- `pub fn references_page(&self, page_no: u64, page_pba: u64) -> bool` (:1839-1843): `index[page_no]` present and at `page_pba`.
- `pub const fn references_index(&self, index_pba: u64) -> bool` (:1847-1849): `index_pages > 0 && index_pba == ..`.
- `pub fn regions(&self) -> Vec<RootRef>` (:1853-1862): the index region then every present page, as `RootRef`s with their CRCs. This plus the `SnapRootPage` block is what the boot counts as the snapshot's metadata (`boot:566-573`).

```rust
pub struct RewrittenRoot { pub pinned: PinnedRoot, pub pages: Vec<RootRef>, pub index: RootRef,
    pub freed_pages: Vec<FreedPage>, pub freed_index: FreedIndex, pub new_pages: Vec<(u64, Box<MapPage>)> }  // :1741-1754
```

### 2.9 Test doubles (`map:1874-1936`)

`VecReader { bytes, reads }` implements `PageReader` over a byte vector (`Io{reason:"beyond the image"}` past the end or for a non-block length). `VecWriter { bytes, placed }` appends whole-page regions at the end of its vector and names them by block number (`starting_at(first_pba)`; `Io{"not whole pages"}`).

### 2.10 The engine's use of the map (the call sites an implementer must not break)

- `BackstoreReader<'a, B: Backstore>(&'a mut B)` implements `PageReader` by `self.0.read(pba, dst)` mapped to `MapError::Io` (`engine:1351-1358`). Private to the engine.
- `SnapState { record: SnapRecord, pinned: Option<PinnedRoot>, damage: Option<String> }` (`engine:1075-1079`): `pinned` is `None` for a snapshot whose root page or index did not verify at boot.
- `Engine::audit_liveness(&mut self) -> Result<Vec<LivenessDrift>, EngineError>` (`engine:2820-2878`), `LivenessDrift { pseg, counted, found }` (:924-932): see 4.9.
- `snapshot_read` (:2916-2954) resolves through `pinned_get`; `snapshot_diff(snap_id, base: Option<u64>)` (:2958-2991) = `pinned_diff(pinned, base_cp)` with `SnapshotOrder` if `base_cp >= birth`; `live_diff(vol, base)` (:2995-3007) = `MapSet::diff`; `base_cp` (:3009-3024) maps a base snapshot id to its `birth_cp`, `None` to 0.
- `prepare_meta` builds `CpInput` (:4839-4849, `max_region_pages = blocks_per_segment()`), calls `prepare_cp` (:4860-4863), turns `damaged_pages` into `OperatorWarning::MapPageDamaged` (:4864-4876), builds `live_after` (:4877-4899), runs the snapshot deletions with `pinned_diff`/`pinned_get`/`get` (:4901-5021), the cleaner's `rewrite_pinned` (:5023-5118), pins new snapshots from `fold_roots` (:5130-5165), does the liveness arithmetic (:5256-5343) and the one free rule (:5346-5397); `apply` at :5735; `cache_pages` for rewritten trees at :5764.
- `pinned_by(snaps, vol, birth_cp) -> bool` (`engine:6175-6181`): any snapshot of `vol` with `record.birth_cp >= birth_cp`.
- `boot::walk_cp` (`boot:385-533`), `load_snapshot` (:538-575), `load_cp` (:577-; `from_roots` :600-614, `replay` :615-623), `read_root` (:774-796: `pba != 0`, end within `capacity_bytes`, `bytes <= 4 GiB`, read, `RootRef::verify`).

---

## 3. Invariants and rules in force

Numbered for citation; each names its source.

**I1. The tree shape.** A volume's map is `MapRoot -> index region (index_pages x 16 KiB, flattened to map_pages_for(lba_count) entries) -> MapPage (1,024 entries) -> MapEntry` (`fmt/map:4-18`, `map:4-11`). LBA `l` is at map page `l >> 10`, slot `l & 1023`. A map page is 4 blocks at `PageIndexEntry::page_pba`; an index region is `index_pages * 4` blocks at `MapRoot::index_pba`.

**I2. CRC in the parent** (`otap-format/src/lib.rs:22-27`, F10 :127-132). A map page is verified by `page_crc` **and** `max_birth` in its index entry; an index region by `index_crc` in its root; the roots table by the superblock's `RootRef`; a `SnapRootPage` by its own trailing CRC. A flipped bit or a misdirected page (page 1's bytes at page 0's address) is refused before any entry is believed (crate invariant, `map:37-40`; `map_tests.rs:412-473`).

**I3. Entry classes** (F2 `lib.rs:89-92`, F5 :103-107, F9 :124-126). `addr == 0 && birth == 0` is unmapped (never written, reads zeros); `addr == 0 && birth >= 1` is a hole (punched at `birth`, reads zeros, CRC 0); `addr != 0` is data with `birth >= 1`. CP 0 is the mkfs commit, so every written block or punched hole has `birth_cp >= 1`. A hole punched over an unmapped block is a change with a birth (E23 `docs/07:670-703`; `review_map_tests.rs:541-564`).

**I4. Pages are immutable; a fold is copy on write** (`map:170-172`, F12 `lib.rs:161-165`). A fold never rewrites a page in place; it places a new page and a new index region and reports the old ones in `freed_pages`/`freed_indexes`. Hence (a) the cache keyed by pba is never stale, (b) a snapshot pinned at CP K keeps reading the state at K whatever the live tree does (`map_tests.rs:210-237`), (c) a page or index region can be referenced by several trees at once (see I8).

**I5. The log and the fold** (F12 `lib.rs:156-166`; E22 `docs/07:618-669`; `map:44-46`). Pages hold the state at `fold_cp`; every CP after it writes its records as a delta region (runs of `CpLogPage`s, one `CpLogRec` per block, last per block wins) listed by the directory in CP order; a fold CP writes **no** delta region (its records go into the pages it rewrites) and empties the directory. Replay applies a region iff `region cp > fold_cp` (a record's own birth is not the rule, because a relocation keeps an older birth: `map:776-779`). A boot therefore never applies a record twice.

**I6. `diff(base)` = exactly the entries with `birth_cp > base`, data or hole, in LBA order, reading only pages whose `max_birth > base`** (`map:47-48`, :1617-1619; E25 `docs/07:740-769`). Not "entries that differ between two trees": a block born after `base` and overwritten again appears once with its newest entry; a block relocated after `base` keeps its old birth and does not appear; a block beyond the volume's current end (shrunk) does not appear. `pinned_diff` and `PinnedRoot::diff` have the same contract over a snapshot tree.

**I7. A prepared CP changes nothing until `apply`** (`map:41-43`, :906-908; `review_map_tests.rs:497-539`). `prepare_cp` may fill the cache and bump `stats`; a dropped `PreparedCp` leaves lookups, the overlay, the log and the next CP as if it never happened.

**I8. What may reference a page.** The live tree of a volume and every pinned snapshot of it. Right after a snapshot's fold the live root and the snapshot's root are byte-identical (`pin` clones the live root and index, `map:1685-1689`; the engine pins `fold_roots` which `apply` installs as the live tree, `engine:5134-5163`, `map:1583-1589`): same index region pba, same page pbas. Later folds un-share only the pages they dirty and the index region (a fold with nothing dirty and no resize keeps the index region and only re-stamps `checkpoint_cp_seq`, `map:1318-1322`), so snapshots taken by consecutive quiet folds share one whole tree (E39, `docs/07:1484-1488`). A rollback makes the live tree the snapshot's again (`map:1219-1225`). The engine frees a page or index only when no tree after the CP references it (`page_referenced`/`index_referenced`, `engine:5351-5369`; E36 `docs/07:1165-1168`).

**I9. `max_birth` is a skip hint, never a bound.** `PageIndexEntry::max_birth` must equal the page's maximum birth (verified on every read, `fmt/map:260-262`). `MapRoot::max_birth` is written as the running maximum over the volume's rewritten pages (`map:1277, :1306, :1336`), is kept through a shrink (so it may exceed every page's max after a shrink), is `0` for a new volume, and **is read by nothing** outside the map crate (grep of `otap-cp`, `otap-sim`, `otap-daemon`, `otap-ctl`). Invariant that holds: `root.max_birth >= max(index[i].max_birth)`; equality is not guaranteed.

**I10. `checkpoint_cp_seq`.** After every fold at K, every present live root carries `checkpoint_cp_seq == K` (`map:1220, :1320, :1335`, and `apply` re-stamps every volume, :1590-1592). A volume created by a non-fold CP gets `checkpoint_cp_seq = self.fold_cp` (the *last* fold, not the creating CP: `map:1003-1007, :1031`). So on disk: `every present root.checkpoint_cp_seq == dir.fold_cp_seq`. A pinned snapshot root carries `checkpoint_cp_seq == the snapshot's birth_cp` (pinned from the fold's roots at `job.cp_seq`, `engine:5134-5163`), and `rewrite_pinned` keeps it (`..pinned.root`, `map:1535-1540`; `lifecycle_map_tests.rs:743-746`).

**I11. Root-changing operations need a fold CP and exclude deltas** (`map:924-934`, :952-957, :970-979, :990-998; E36). A rollback, resize or deletion rides a fold; a rollback or deletion CP has no deltas of the volume; a shrink has no deltas at or beyond the new size; one volume is not rolled back and resized, or deleted and anything, in one CP. The `rollback.root` must be the volume's own, with an index of `map_pages_for(lba_count)` entries and `birth_cp <= committed_cp` (:940-950).

**I12. Damage containment** (E32 `docs/07:980-1002`; `map:533-538`, :1062-1065). A page that does not verify when a CP looks up what it replaces, or when a rollback/shrink/deletion walks it, does not fail the CP: the old entry is taken as `UNMAPPED` (its bytes stay counted live, the safe direction) and the page is reported in `damaged_pages`; the engine warns once (`MapPageDamaged`) and folds that must rewrite it fail (E31). **But `get`, `diff`, `pinned_get`, `pinned_diff` and `PinnedRoot::{get,diff}` are not tolerant**: they return `Err(MapError::Format(_))` at the first damaged page (`map:890, :1637, :1709, :1725, :1807, :1825`). Only the private `collect_entries` tolerates damage.

**I13. Size and range.** `lba_count` in `1..=MAX_LBA + 1`; `vol < 1024`; every delta's LBA below its volume's size (`map:1011-1016, :1050-1059`); a record beyond a volume's root refuses the boot (`Replay{lba}`, :834-839). A volume's size is its root's (E32); a resize is a CP that rewrites the root.

**I14. Liveness of a data block** (E25 `docs/07:757-760`; E36; `engine:2820-2878`). A physical block `(lseg, off)` is live iff the live map or any (non-damaged) pinned snapshot names it as a data entry; it is counted once however many trees name it. Holes count nothing. The engine keeps `data_live[pseg]` exact at every committed CP (`engine:5256-5320`), persists it as `SegEntry::live_bytes` (:5330-5343) and reloads it from the table at boot (:1901-1907). `audit_liveness` is the oracle (the simulator runs it after every recovery, `harness.rs:1104-1117`).

**I15. Liveness of metadata** (E24 `docs/07:704-739`). `meta_live[pseg]` = bytes of every region the committed CP references, placed once per region at its CP (`engine:5321-5329`), released once after the deferral (`freed` deduplicated by pba, :5396-5397). At boot it is recomputed by summing `layout.segments_of_region` over the `regions` list of `walk_cp` **without deduplication** (`boot:697-703`), and that list contains a page once per tree that references it (`boot:419` for the live index, `boot:573` for each snapshot). See 7.9.

**I16. The sender's precondition (derived, not written anywhere).** `diff`-by-birth describes "target minus base" only if no rollback to a CP below `base` happened between them. The lifecycle rules make this hold as long as the base keeps its pin: a rollback refuses while a newer snapshot exists (`NewerSnapshots`, E36 `docs/07:1197-1201`), and the base cannot be deleted while `PIN_REPLICATION` is set (`SnapshotPinned`, E36 :1184-1187; `engine:2520-2553`). If the operator releases the base (`set_replication_base(vol, None)`) the guarantee is gone (open question Q3).

---

## 4. How things happen, in order

### 4.1 A lookup: `MapSet::get` (`map:875-892`) and `load_page` (`:246-267`)

1. `vols.get(&vol)` else `NoSuchVolume`.
2. `check_lba`: `lba >= lba_count` → `OutOfRange{vol, lba}` (:393-398).
3. `overlay.get(&lba)` → return it (an overlay entry is a committed entry of a CP after the fold).
4. `entry = index[page_of(lba)]`; `entry.is_none()` → `UNMAPPED` (the whole 4 MiB has no page).
5. `self.page(reader, &entry)` (:895-904): a cache miss bumps `stats.page_reads`; `load_page` reads 16 KiB at `entry.page_pba` (`reader.read_blocks` → `Io` on failure), `MapPage::read(&bytes, &entry)` (`Format(BadLength)` impossible here; `Format(BadCrc{region:"map page"})`; `Format(BadField{lseg|plen|birth_cp|crc32c})` from entry validation; `Format(BadField{max_birth})`), caches it under `page_pba`.
6. `page.entries[slot_of(lba)]`.

`PinnedRoot::get` (:1794-1809) and `MapSet::pinned_get` (:1693-1711) are steps 2, 4, 5, 6 without the overlay.

### 4.2 A consistency point: `prepare_cp` (`map:909-1401`) then `apply` (`:1568-1615`)

1. Refuse `cp_seq == 0`, `> MAX_CP_SEQ`, `<= committed_cp` (`Replay{cp_seq}`, :915-920); `max_region_pages == 0` (`Internal`, :921-923).
2. Any rollback/resize/delete without `fold` → `Internal("a rollback, resize or deletion needs a fold CP")` (:928-934).
3. Validate each rollback (:935-958): volume exists; `rb.root.validate()`; `rb.root.vol == rb.vol`, present, `rb.index.len() == map_pages_for(rb.root.lba_count)`, `rb.birth_cp <= committed_cp`; no deltas for the volume.
4. Validate each resize (:959-985): volume exists; size in range (`BadSize`); a shrink has no delta at or beyond the new size; not also rolled back.
5. Validate each delete (:986-999): exists; not also written, rolled back, resized or created.
6. New volumes (:1000-1039): `vol < 1024`, size in range, not present (`VolumeExists`); an all-`NONE` index image is **placed now** through `writer` (`written` gets its ref); the root has `checkpoint_cp_seq = fold_cp_after` (= `cp_seq` if this is a fold, else the last fold), `max_birth 0`.
7. Deltas deduplicated into `BTreeMap<(vol, lba), MapEntry>` (:1040-1061): each `entry.validate()`, refuse unmapped and `birth_cp > cp_seq` (`Format(BadField{birth_cp})`; an older birth is allowed: a relocation), volume must exist or be created now, LBA below its size.
8. `replaced` (:1062-1090): for every delta, `self.get(reader, vol, lba)`; a `Format` error means a damaged page: old = `UNMAPPED`, page recorded in `damaged_pages`; other errors propagate.
9. Delta regions (:1091-1137), **non-fold only**: records in `(vol, lba)` order into `CpLogPage`s of `cp_seq`/`cp_nonce` numbered `page_seq` 0..; a region is flushed to `writer.place` every `max_region_pages` pages; `delta_roots`, `delta_bytes`, `written`, `stats.records_written`.
10. **The fold** (:1139-1344), when `input.fold`: `folded` = every existing volume (`index.clone()`, `root`) plus every new volume (:1154-1161). For each:
    - **deleted** (:1163-1187): `collect_entries(vol, 0, 0)` → everything it maps goes to `released` (damage merged into `damaged_pages`); every present page to `freed_pages`; its index to `freed_indexes`; no `fold_roots` entry (the volume leaves).
    - **rolled back** (:1188-1226): `collect_entries(vol, 0, rb.birth_cp)` → live entries born after the snapshot go to `released`; each live page not shared with the snapshot at the same `page_no`/`page_pba` → `freed_pages`; the live index if its pba differs → `freed_indexes`; `fold_roots` gets `MapRoot { checkpoint_cp_seq: cp_seq, ..rb.root }` with `rb.index` **unchanged** (the snapshot's pages become the live pages, no page written).
    - otherwise: `dirty` = the volume's `dirty_pages()` (empty for a new volume) ∪ pages of this CP's deltas (:1227-1235). A **shrink** (:1246-1271): `collect_entries(vol, size, 0)` → entries at or beyond `size` to `released`; pages past the new page count to `freed_pages`; index truncated; dirty pages beyond it dropped; the boundary page made dirty when `size % 1024 != 0` and it exists. A **grow**: index extended with `NONE` (:1273). `root.lba_count = size`.
    - each dirty page (:1277-1317): start from the old page (read and verified through the cache: a damaged page **fails the fold** here, `?` at :1283) or a zeroed page; overlay entries of its range, then this CP's deltas, are written into slots; entries at or beyond a shrunk `new_size` zeroed (:1295-1300); `writer.place(page)`; `index[page_no] = page.index_entry(pba)`; old page to `freed_pages`; new page kept for the cache; `max_birth` running max.
    - the root (:1318-1341): nothing dirty and no resize → same root re-stamped (index region kept, **not** freed); else a new index image placed, the old index to `freed_indexes`, new root with `index_pba/pages/crc`, `checkpoint_cp_seq`, `max_birth`.
    - `freed_deltas = self.delta_roots.clone()` (:1343): the whole log is dropped by the fold.
11. The directory (:1347-1358): `fold_cp_seq = fold_cp_after`; `deltas` = this CP's regions alone (fold) or the old list plus this CP's; `pages` = the fold's `page_roots` (the engine appends a relocation's pages, `engine:5250-5251`).
12. The roots table (:1359-1376): one slot per index up to the highest non-deleted volume; existing roots, then new volumes' first roots, then `fold_roots` win.
13. Return `PreparedCp` (:1388-1400).

The engine then (`engine:4864-5410`) warns about `damaged_pages`, computes `live_after`, processes snapshot deletions and rewrites, pins new snapshots from `fold_roots`, places the tables, does liveness, computes `freed`, writes the superblock; on commit:

20. `apply` (:1568-1615): remove deleted volumes; insert new volumes (empty overlay); if fold: install every `fold_roots` tree and **clear its overlay**, re-stamp every root's `checkpoint_cp_seq`, insert the new pages into the cache, `fold_cp = cp_seq`, `delta_roots = []`, `log_bytes = 0`, `cps_since_fold = 0`, `stats.folds += 1`; else: overlay-insert every delta, `cps_since_fold += 1` if a region was written, extend `delta_roots`, add bytes. Then `committed_cp = cp_seq`, `last_replayed = None`.

### 4.3 A boot: `from_roots` then `replay` (`map:614-646`, `:782-864`; `boot:600-623`)

1. The boot reads and verifies the roots table (`parse_map_roots`), every present root's index region (`verify_index`) and lists every page it names as a region to verify later (`boot:395-422`); reads the directory (`CpLogDir::decode`, must be the slot's CP) and every delta region (`read_root` = length + CRC) (`boot:424-450`); verifies every page in `dir.pages` (`boot:454-456`).
2. `MapSet::from_roots(agg, CACHE_PAGES, sb.cp_seq, dir.fold_cp_seq, &roots, load_index)` (`boot:600-614`): per present root `VolumeMap::from_root` (validate, `index_entries`).
3. For each delta in directory order: `replay(bytes)` (`boot:615-623`):
   - `first_page` = the first page's `page_seq` (:783-787); `parse_delta` (:788): non-empty multiple of 4 KiB (`Format(BadLength)`), every page `CpLogPage::read` (header magic/major/CRC/count/CP bound + every used record's fields and CRC), `agg_id` (`Format(IdentityMismatch)`), one `cp_seq` per region (`Replay{cp_seq}`), consecutive `page_seq` (`Replay{page_seq}`).
   - `cp_seq < fold_cp || cp_seq > committed_cp` → `Replay{cp_seq}` (:791-796).
   - Ordering against `last_replayed` (:797-824): CPs non-decreasing; within a CP the next region starts at the page after the last; a new CP starts at page 0; the very first region starts at page 0.
   - Per record (:828-852): unknown volume → `Replay{vol}`; `lba >= lba_count` → `Replay{lba}`; `birth_cp > cp_seq` → `Replay{birth_cp}`; applied to the overlay iff `cp_seq > fold_cp`, else skipped.
   - `log_bytes` grows only for applied regions; `last_replayed = (cp_seq, first_page + pages)`.
4. `set_delta_roots(dir.deltas, number of distinct CPs above the fold)` (`boot:623`).

The committed map is therefore **fully reconstructible from disk**: pages + directory + delta regions. The in-memory overlay after boot equals the fold of the listed deltas with `cp > fold_cp_seq`, last record per `(vol, lba)` in directory order winning.

### 4.4 `collect_entries` — the tolerant walk (`map:1407-1456`, private)

1. Clone the volume's index and the overlay entries at or above `from_lba`; `lba_count`.
2. For each `page_no` from `page_of(from_lba)`: `first = first_lba(page_no)`, `last = min(first + 1023, lba_count - 1)`.
3. If the index entry is present **and** `max_birth > base_cp`: read the page through the cache; `Err(Format(_))` → push `page_no` to `damaged` and treat the page as absent; other errors propagate (:1426-1432).
4. For each `lba` in `max(first, from_lba)..=last`: the overlay entry if any, else the page's slot if the page was read, else skip; keep it when `!is_unmapped() && birth_cp > base_cp` (:1437-1453).
5. Return `(entries in LBA order, damaged page numbers)`.

Used by: deletion (`from 0, base 0`: everything), rollback (`from 0, base rb.birth_cp`: what the snapshot does not know), shrink (`from size, base 0`: everything beyond the end). Not public.

### 4.5 `MapSet::diff` — the live birth walk (`map:1620-1673`)

1. Clone the index and the overlay (as a sorted `Vec`); `ov` peekable.
2. Per `page_no`: `first`, `last = first + 1023` (**not** clamped to `lba_count`; the last page's unused slots are `UNMAPPED` by construction and are filtered by `!is_unmapped()`); `page_may = present && max_birth > base_cp`; if so read the page (`?`: **a damaged page fails the whole walk**, :1637).
3. Walk `lba` from `first`: if the next overlay entry is at `lba`: consume it, report it iff `birth > base && !unmapped`, skip the page's slot (`continue` after `lba += 1`, :1645-1655: an overlay entry overrides the page's entry even when the overlay entry is older, the relocation case pinned by `review_map_tests.rs:656-695`); else if the page was read: report its slot iff `birth > base && !unmapped`; else jump `lba` to the next overlay entry in this page or leave the page (:1663-1669).
4. (`emitted` at :1639/:1652 is written and never read: dead state.)

Cost: one 16 KiB read per present page with `max_birth > base`, plus the overlay scan; pages with `max_birth <= base` cost nothing even when the overlay touches them.

### 4.6 `pinned_diff` / `PinnedRoot::diff` — the snapshot birth walk (`map:1714-1734`, `:1814-1834`)

1. For each `(page_no, entry)` of `pinned.index`: skip if `entry.is_none() || max_birth <= base_cp`.
2. Read the page (`MapSet::page` through the map's cache, or `load_page` through the caller's cache): `?` on any error.
3. For every slot `0..1024`: report `(first_lba(page_no) + slot, e)` iff `e.birth_cp > base_cp && !e.is_unmapped()`.

No overlay, no clamp to `lba_count` (unused slots are unmapped), LBA order.

### 4.7 `pin` (`map:1677-1690`)

Refuses while the overlay is non-empty, otherwise clones the live root and index. The engine does not call it: it pins `PreparedCp::fold_roots` inside the fold CP (`engine:5134-5163`), which is the same tree `apply` installs.

### 4.8 `rewrite_pinned` — relocation of a snapshot's addresses (`map:1465-1557`)

1. Group `changes` by page; each `(lba, old, new)`: `lba < lba_count` (`OutOfRange`), `new.validate()`, both data, same `birth_cp`, same `crc32c` (`Internal("... changes more than the address")`).
2. Per page: the index entry must be present (`Internal`); read the page (not tolerant); every `old` must equal the slot (`Internal("... holds another entry")`); write `new`; `writer.place` the page; `index[page_no] = page.index_entry(pba)` (`max_birth` unchanged, `debug_assert`); old page to `freed_pages`; keep the page for the cache.
3. Always place a new index image (even with no changes, `lifecycle_map_tests.rs:819-825`); new root = `..pinned.root` with the new index pba/pages/crc (`checkpoint_cp_seq`, `lba_count`, `max_birth` kept).
4. Return `RewrittenRoot`; the live map is untouched; the engine writes a new `SnapRootPage`, re-seals the record, lists the pages in `dir.pages`, nominates the old pages/index/root page for freeing (`engine:5076-5116`, :5250-5251, :5379-5385).

### 4.9 `Engine::audit_liveness` (`engine:2820-2878`) — how the engine computes per-segment liveness from the map

1. For each volume of `map.volumes()`: `map.diff(reader, vol, 0, ..)` collects every entry born after 0, i.e. every mapped entry (data and holes) of the live tree, overlay merged.
2. For each data entry: `seen.insert((lseg, off))`; if new, `found[lseg] += 1`.
3. For each snapshot with `pinned.is_some()`: `map.pinned_diff(reader, &root, 0, ..)` and the same counting through the same `seen` set.
4. For each segment in `data_live.keys() ∪ found.keys()`: report `LivenessDrift { pseg, counted: data_live[pseg], found: found[pseg] }` when they differ.

So: **live = live tree ∪ every readable snapshot tree, counted once per physical block, keyed by `lseg` which equals `pseg`**. A damaged page anywhere makes the audit return `Err` (I12). Damaged snapshots (`pinned == None`) are silently skipped, so their blocks show as drift (`counted > found`) — E36 says their blocks leak until a repair.

### 4.10 The per-CP liveness arithmetic (`engine:5256-5343`) that the attestation must reproduce or reconcile against

1. `data_live = self.data_live.clone()`.
2. Cleaner moves: `-1` at `old.lseg()`, `+1` at `new.lseg()` (:5269-5276).
3. For each `Replaced { old, new }`: skip a pure relocation (`same birth, same crc, different addr`); `+1` at `new.lseg()` if data; `-1` at `old.lseg()` if `old` is data and `!pinned_by(&remaining, vol, old.birth_cp)` (no snapshot that remains after this CP has `birth_cp >= old.birth_cp`) (:5277-5295).
4. For each `released` entry: `-1` under the same `pinned_by` rule (:5296-5300).
5. For each block a deleted snapshot alone referenced: `-1` (:5301-5303).
6. A drained segment is forced to 0 with an `Internal` event if the count disagrees (:5304-5320).
7. `meta_live` = previous + bytes placed by this CP's stream, per segment (:5321-5329); frees are deferred.
8. `SegEntry::live_bytes` written per used segment: data → `data_live * 4096`, metadata → `meta_live` bytes (:5330-5343).

---

## 5. Hook points for Step 6

### 5.1 Scope (B): the attestation's map part (`otapctl pool verify`, brief items 2 and 4)

**What already exists.** `boot::walk_cp` (`boot:385-533`) is a verification pass over one superblock candidate: roots table, every index region (`verify_index`), every page the directory lists, every delta region, every snapshot root page and index; it does **not** read the live trees' map pages (it only lists them as regions, `boot:419`, which the boot counts for metadata liveness) and it stops at the first failure (a torn candidate). `audit_liveness` (`engine:2820`) is the data-liveness reconciliation but reads through non-tolerant walks and compares against the in-memory `data_live`, not against `SegEntry::live_bytes` on disk (equal at a committed CP by construction, :5341).

**The smallest coherent change in `otap-map`: one public tolerant page enumerator.** Nothing public today reads every page of a tree and keeps going past a damaged one (I12). Add, next to `collect_entries`:

```rust
/// Every page a tree names, verified or not, in page order: what an attestation
/// or a liveness recount walks. `f(page_no, &entry, Ok(&page) | Err(e))`.
pub fn walk_pages(cache: &mut PageCache, reader: &mut dyn PageReader, index: &[PageIndexEntry],
                  mut f: impl FnMut(u64, &PageIndexEntry, Result<&MapPage, &MapError>))
    -> Result<(), MapError>   // Err only for a reader I/O failure, never for damage
```
built on `load_page` (`map:246-267`): `Err(Format(_))` is handed to `f` and the walk continues; `Err(Io{..})` propagates (E30: a read error is the device's, never "torn"). Expose it both as a free function over a `&[PageIndexEntry]` (so the attestation can walk a `PinnedRoot`, a `VolumeMap::index()` or an index it parsed from disk itself) and as `MapSet::walk_volume_pages(&mut self, reader, vol, f)` through the map's cache. This is the single addition; `collect_entries` could then be re-expressed over it, but need not be.

**The exact enumeration of one tree, from disk alone** (what the attestation does per live root and per snapshot root; all names cited):

1. `root.validate()` (`fmt/map:421`). Read `index_root_ref(&root)` (`map:318`) with `read_root`'s bounds (`boot:774-796`: `pba != 0`, end within capacity, `RootRef::verify`). `root.verify_index(&bytes)` (`fmt/map:458`), then `PinnedRoot::load(root, &bytes)` (`map:1771`) to get `index: Vec<PageIndexEntry>` of exactly `map_pages_for(lba_count)` entries with the "entries beyond the volume are NONE" check (`map:305-312`). Report, don't stop, on failure: the tree's pages are then unknown.
2. For `page_no in 0..index.len()`, `e = index[page_no]`:
   - `e.is_none()`: 1,024 unmapped LBAs; `e.validate()` already ran in `verify_index` (an absent entry carries no birth/CRC).
   - else `pba = e.page_pba`; bounds: `pba != 0`, `pba + 4` blocks within the device, and (a check nothing does today) the page lies in a segment the table marks as *metadata* and not free. Read 16 KiB; `MapPage::read(&bytes, &e)` (`fmt/map:274`): `BadCrc{"map page"}` = corrupt or misdirected; `BadField{max_birth}` = index/page disagreement; `BadField{lseg|plen|birth_cp|crc32c}` = an entry the format refuses. Record `(vol, page_no, pba, err)` and continue.
   - `first = MapPage::first_lba(page_no)`; for `slot in 0..1024`: `lba = first + slot`, `entry = page.entries[slot]`, yield `(lba, entry, pba)`.
   - Extra checks the format does **not** make and the brief's "bounds, LBA continuity" wants: for `lba >= lba_count` the entry must be `UNMAPPED` (a shrink zeroes them, `map:1295-1300`; a grow adds `NONE` pages); for a data entry `lseg` in `1..=segments` and `off < blocks_per_segment()` (`layout.block_pba` returns `None` otherwise, `layout.rs:203-211`), `plen == 8`, `birth_cp <= sb.cp_seq`; the segment at `lseg` is a data segment that is not free; for the live tree `root.checkpoint_cp_seq == dir.fold_cp_seq` (I10), for a snapshot root `== record.birth_cp`; `root.max_birth >= e.max_birth` for every entry (I9).
3. Cost: one index-region read (`index_pages` × 16 KiB) + one 16 KiB read per present page; 1,024 entries scanned per page; no cache needed if each tree is walked once, but a `PageCache` keyed by pba (`map:174`) deduplicates the pages shared between the live tree and its snapshots (I8), which on a pool with quiet snapshots is most of them. A `PageCache::new(n)` sized to the pool's distinct pages makes the whole attestation read each page once.

**Liveness recount (brief item 4), the exact rule to mirror** (4.9, 4.10): over the live tree and every readable snapshot tree, count each data entry's `(lseg, off)` once; compare `count * 4096` with `SegEntry::live_bytes` of each data segment (`fmt/records:134`; `engine:5336`). From disk alone this is exact for data at a committed CP. For metadata, `live_bytes` = bytes placed and not yet released, so recomputed referenced bytes `<=` recorded; the difference should equal the deferred frees the engine still holds (`freed_pending`, E24) — reconcilable only with the running engine's list, and complicated by 7.9. Note the engine reads `live_bytes` into `data_live` at boot without recounting (`engine:1901-1907`), so a drift written to the table survives reboots until something recounts: the attestation is that something.

**Cross-check against stripes (brief item 3), what the map provides:** for each data entry the expected `(vol, lba, birth_cp, crc32c)` at physical `(lseg, off)`; the stripe descriptor at that block carries `birth_cp` and `crc32c` (`cleaner.rs:232` rebuilds the entry from `desc.birth_cp, desc.crc32c`). Do **not** compare `entry.birth_cp` with the stripe header's `cp_seq`: a relocation writes old births into a new stripe (F17 `lib.rs:217-220`).

**Where it runs.** The attestation needs a `PageReader` over the backstore (`BackstoreReader`, `engine:1351`, is private to the engine: either make it `pub(crate)` and add an `Engine::attest` that borrows `self.map`'s cache and `self.backstore`, or give the attestation its own `PageCache` and a reader over `Backstore::read`). Against a *running* engine the committed trees are `map.volumes()` (`root()`, `index()`) and `snapshots[i].pinned` (`SnapState`, `engine:1075`); the overlay must be ignored for a disk-only pass or merged (`VolumeMap::overlay()`) for a "what the engine believes" pass — both are reasonable; say which (Q1).

**What it must not break:** `walk_cp`'s tearing rules (a damaged page of an older CP is *not* torn, `boot:488-491`); E30 (a read error fails, never tears); the E32 containment (an attestation must never make a CP fail).

### 5.2 Scope (C): the sender's walk

**The exact iteration** for a pinned target snapshot `T` (its `PinnedRoot` from `SnapState.pinned`, or loaded from its `SnapRootPage.root` + index region with `PinnedRoot::load`) and a base `base_cp` (the base snapshot's `record.birth_cp`, or 0 for a full send):

```rust
// engine side: Engine::snapshot_diff(snap_id, Some(base_id)) does exactly this and refuses base_cp >= birth
// map side: PinnedRoot::diff(&cache, &mut reader, base_cp, |lba, e| ..)        // map:1814-1834
for (page_no, e) in pinned.index.iter().enumerate() {
    if e.is_none() || u64::from(e.max_birth) <= base_cp { continue; }        // whole 4 MiB unchanged
    let page = load_page(cache, reader, e)?;                                   // 16 KiB, verified; Err on damage
    let first = MapPage::first_lba(page_no as u64);
    for (slot, en) in page.entries.iter().enumerate() {
        if u64::from(en.birth_cp) > base_cp && !en.is_unmapped() {
            emit(first + slot as u64, *en);                                   // data: lseg/off/plen/birth/crc; hole: birth
        }
    }
}
```
Output: `(lba, MapEntry)` per block, LBA ascending, one per block. Cost: an in-memory scan of `map_pages_for(lba_count)` index entries + one 16 KiB read per present page with `max_birth > base_cp` (a single changed block in a 4 MiB page costs the whole page read); nothing else. The sender coalesces: runs of consecutive LBAs with `is_hole()` → `HoleRecord { vol, len, lba }` (`crates/otap-format/src/stream.rs:324-331`, `Holes` frame kind 6); runs of data → chunk payloads built by **reading the blocks** at `layout.block_pba(lseg, off)` and verifying `crc32c(block) == entry.crc32c` (as `read_entry`, `engine:3710-3728`), because the receiver's addresses are its own: ship `(vol, lba, birth_cp, crc32c, bytes)`, never `lseg/off`.

**Semantics the sender inherits from I6:** a block written twice since base appears once (newest); a block written then punched appears as a hole with the hole's birth; a block punched before base and untouched does not appear; a block relocated by the cleaner after base keeps its old birth and does not appear (content unchanged); a block beyond `T.root.lba_count` never appears, so a shrink must travel as a `VolEvent::Resize` and a grow needs no event for the map's sake (new blocks appear as writes). Blocks born after base and later rolled back below base cannot exist while the base keeps its pin (I16).

**Where it plugs in.** `Engine::snapshot_diff` (`engine:2958-2991`) already returns the whole walk as a `Vec<(u64, MapEntry)>`; for a large volume the sender wants the callback form (`pinned_diff`) to stream chunks and to bound memory. The base pin is `PIN_REPLICATION` via `set_replication_base` (`engine:2520-2553`); advancing it on `Committed` is `set_replication_base(vol, Some(new))`, which clears the old base's bit in the same CP (E36). A `DeltaBegin`'s `cp_seq`/`cp_nonce` should be the target snapshot's CP (`record.birth_cp`) and that CP's nonce, which the snapshot record does not carry (Q4).

**Must not break:** `snapshot_diff`'s `SnapshotOrder` refusal (`engine:2974-2981`); the deletion walk's use of `pinned_diff` with `older` as base (`engine:4954`), which relies on the same semantics; the `diff == model` property in the three test files.

### 5.3 Both scopes: what the map crate should **not** gain

- No tolerance in `diff`/`pinned_diff`: the deletion walk (`engine:4943-5002`) and `audit_liveness` rely on an error there to stop and the engine chose "release nothing" for a damaged neighbour (E39 W1). Add the tolerant enumerator beside them.
- No `lseg`→`pseg` resolution: the map stays I/O-free and policy-free (`map:25-33`); the attestation resolves through `DiskLayout::block_pba`.
- No change to `MapEntry`/`PageIndexEntry`/`MapRoot` layouts: every size is `const`-asserted (`fmt/map:474-511`) and a change is a format bump with new golden vectors (`fmt/constants:1-6`).

---

## 6. Test patterns to copy

**The model harness** (`map_tests.rs:21-273`, the simplest): an `Image { bytes }` that is both the `RegionWriter` (append at page granularity through a `VecWriter`, :42-52) and the `PageReader` (:54-64); `read_region(image, &RootRef)` verifies a placed region (:67-72); `data(lseg, off, birth, crc)` builds entries with `SECTORS_PER_BLOCK` (:74-76); `Model = BTreeMap<(vol, lba), MapEntry>` (:79); `Pool::cp(deltas, fold, new_vols)` (:115-174) runs `prepare_cp` → places the roots table (`map_roots_image`) and the directory (`dir.encode(agg)`) as the engine would → checks every `written` region verifies and every `replaced.old` equals the model → `apply` → updates the model → records a `PinnedRoot::from_parts` per `fold_roots` with a copy of the model (the snapshot oracle, :162-173). `check_lookups` (:177-190), `check_diff(base)` (:193-208: `diff(base)` must equal the model filtered by `birth > base`, in order), `check_pins` (:211-237: every pin through `PinnedRoot::get` and `pinned_get`, plus `diff(0)` equals the snapshot model), `reboot` (:241-272: `from_roots` + `replay` of `dir.deltas` + `set_delta_roots`, then every lookup equal, then the fresh map replaces the live one). This is exactly the oracle an attestation test needs: a model of what every tree must contain, an image to corrupt, a reboot to prove the disk is complete.

**Damage** (`map_tests.rs:412-473`): flip a byte of a page → `Err(MapError::Format(FormatError::BadCrc{..}))` on `get`; copy page 1's bytes over page 0 → refused; flip a byte of an index region → `from_roots` fails. Note the trick at :423-430: a *fresh* `MapSet` over the damaged image, because the live map's cache still holds the good page.

**Borrowed-image reader** (`review_map_tests.rs:59-74`): `SliceReader<'a>(&'a [u8])` avoids a copy per check; `Pool::abandoned_cp` (:210-218) prepares against a cloned image and drops the result (I7). `check_pins(base)` (:266-286) checks `pinned_diff` at every base, not only 0.

**Lifecycle** (`lifecycle_map_tests.rs:98-301`): `Ops { fold, new_volumes, rollbacks: Vec<(vol, pin index)>, resizes, deletes }`; `cp` builds `Rollback` from a recorded `Pin { cp, root, model }` (:146-158), asserts `damaged_pages.is_empty()` (:171), checks `released` against the model (:179-210, deduplicated: "released twice"), checks `freed_pages` against `references_page` for a rollback and against every page for a deletion (:211-249); the random walk (:856-996) keeps three volumes around a page boundary, re-creates deleted volumes and drops their pins (:964-982).

**`rewrite_pinned`** (`lifecycle_map_tests.rs:714-825`): the old pinned root still reads the old addresses and the live map is untouched (:768-774); refusals for a mismatching entry, a changed birth, an absent page, an LBA out of range.

**The engine-level oracle for the sender:** `harness.rs:787-908 check_diff(snap_id)` compares `live_diff(vol, Some(snap))` with the simulator's image, block by block, with the hole subtlety at :870-879 (zeros now and zeros then differ only if a newer punch covers the block: "the committed map tells"); `verify_snapshots` (:729-782) reads every snapshot block through `snapshot_read`. `audit_liveness` runs after every recovery (:1104-1117, called at :1343).

**`PROPTEST_CASES`.** The convention (E09 `docs/07:205-215`): `PROPTEST_CASES=10000` is the local gate, 100,000 the documented merge gate; `prop_assert!` always carries a message. The three map suites set `failure_persistence: None` and either `cases: 64` (`map_tests.rs:641-645`) or the default (`review_map_tests.rs:744-747`, `lifecycle_map_tests.rs:999-1003` with `max_shrink_iters: 512`). The environment overrides an explicit `cases` because `proptest!` passes the config through `contextualize_config` (E39, `docs/07:1500-1510`; log `docs/08:946-951`); the lead nonetheless chose to make the intent visible, so new suites copy the helper of `crates/otap-sim/tests/review_step5_sim_tests.rs:912-919`:

```rust
/// The case count: `PROPTEST_CASES` when set (the merge gate), else `default`.
fn cases(default: u32) -> u32 {
    std::env::var("PROPTEST_CASES").ok().and_then(|v| v.parse().ok()).unwrap_or(default)
}
proptest! { #![proptest_config(ProptestConfig { cases: cases(24), failure_persistence: None, max_shrink_iters: 256, ..ProptestConfig::default() })] ... }
```
Gate timing to budget for: `map_tests` takes 19 minutes at 10,000 cases (`docs/06:392`); the owner prefers thorough over fast and never trims case counts.

---

## 7. Risks, pitfalls and traps

7.1 **`diff(0)` is not "every page".** A present page can have `max_birth == 0` (every entry unmapped) after a shrink that empties a boundary page's remaining range (`map:1267-1271`, :1295-1300): `diff`, `pinned_diff` and `collect_entries` skip it (`max_birth <= 0`) and the boot never reads it. An attestation that enumerates pages through `diff(0)` misses such a page and its CRC; iterate the index (5.1).

7.2 **The public walks are not tolerant** (I12). One damaged page in any tree makes `audit_liveness`, `snapshot_diff`, `live_diff` return `Err`. The attestation must not be built on them, and the sender must decide what a `Format` error mid-delta means (abort with `Corrupt`, per the stream's `AbortReason`, `docs/05:684`).

7.3 **Unmapped past the end.** `diff`/`pinned_diff` do not clamp `last` to `lba_count`; they rely on the slots beyond the end being `UNMAPPED` (filtered by `!is_unmapped()`). The fold guarantees it (`map:1295-1300`), and `index_entries` guarantees no page beyond `map_pages_for` (:305-312). An attestation should assert it rather than assume it.

7.4 **Relocation keeps the birth** (`map:776-779`, `review_map_tests.rs:656-695`, `rewrite_pinned` :1482-1491). Consequences: a delta record may carry a birth older than its CP; the overlay entry overrides a newer page entry; `diff` after a relocation does not report the block; `Replaced` with same birth and CRC but another address is skipped by the liveness arithmetic (`engine:5278-5285`). Any "birth_cp == the CP that wrote this stripe" check is wrong.

7.5 **`lseg == pseg` today, by construction, not by contract.** `SegEntry::phys`, `lap` and `Relocated` are reserved (F11, F18). Code that keys liveness by `lseg` (`audit_liveness`, `data_live`) and resolves addresses by `block_pba(lseg, off)` will need the segment table once relocation tables exist. Write the attestation with a `resolve(lseg) -> pseg` seam even if it is the identity.

7.6 **The cache is keyed by pba and FIFO.** Shared pages between trees are one cache entry (good for an attestation); the cache does not cache failures (every lookup of a damaged page re-reads it: an attestation that touches a damaged page's blocks many times re-reads it many times); eviction is insertion order, so a long sequential walk through a small cache evicts nothing useful but also helps nothing.

7.7 **`pin()` is not what the engine uses.** `pin` refuses a non-empty overlay; the engine pins `fold_roots` inside the fold CP. A sender that needs "the live state now" must request a snapshot (a fold) and walk its `PinnedRoot`; walking `MapSet::diff` gives a moving target with the overlay merged and no CP identity.

7.8 **`roots_table` and `volumes()` disagree during a CP.** `MapSet::volumes()` is the committed set until `apply`; `PreparedCp::roots_table` and `fold_roots` are the set after. The engine's `live_after` (`engine:4877-4899`) shows the merge; an attestation of a running engine must take the committed view and say so.

7.9 **Metadata liveness is counted differently at boot and at CP time** (I15; observed in code, not proven by a run). `boot:697-703` sums every region in `walk_cp`'s `regions` without deduplication, and a page shared by the live tree and `n` snapshots (I8: every snapshot shares its whole tree with the live map at birth) appears `n + 1` times in that list (`boot:419`, `boot:573`), whereas the engine places a region once and frees it once (`engine:5321-5329`, `:5396-5397`). If this reading is right, `meta_live` (and `SegEntry::live_bytes` of metadata segments) inflates on every boot of a pool with snapshots and such a metadata segment can never reach zero and be freed: a slow metadata leak across reboots, in the safe direction (nothing freed early). No test checks `meta_live_bytes` against a walk (`review_sim_tests.rs:401`, `step4_sim_tests.rs:181` only read it). The attestation's reconciliation (brief item 4) is exactly what would surface it; the lead should decide whether to fix the boot (dedup by pba, as the engine does) in Step 6 before the attestation makes it a reported drift (Q2).

7.10 **`volume_regions` is dead at the boot** (`boot:752`). Harmless; an attestation that wants "every region a live tree references" should call it (or `PinnedRoot::regions`) rather than copy `walk_cp`'s loop.

7.11 **`MapRoot::max_birth` is informational** (I9). Do not use it to skip a volume ("nothing born after base") unless the attestation first proves `root.max_birth == max(index[i].max_birth)`; after a shrink it can be larger, and `prepare_cp` never lowers it.

7.12 **Damaged snapshots skew the audit.** A snapshot with `pinned == None` contributes nothing to `found` while its blocks stay in `data_live` (deletion releases nothing for it, E36): `counted > found` is expected there, not a defect of the accounting. The attestation report should attribute such drift to the damaged snapshot.

7.13 **Delta regions and the fold CP.** A directory written by a fold lists no deltas (`map:1350-1352`), so a region with `cp_seq == fold_cp_seq` cannot appear in a real directory, but `replay` accepts and skips it (`map:846-851`); a disk-only attestation that re-derives the overlay must apply the same rule (`cp > fold_cp_seq`) and the same ordering checks (4.3).

7.14 **`prepare_cp` on a damaged page differs by path.** A delta over a damaged page commits with `old = UNMAPPED` (reported); a fold that must rewrite that page fails (`map:1283`, E31 "folds stop until the volume is repaired"); a rollback/shrink/deletion walking it reports and continues with the entries unknown (so `released` under-counts, the safe direction). An attestation's "damaged page" finding therefore has three different consequences depending on what the next CP does.

7.15 **The sender's hole runs.** `HoleRecord::len` is `u32` blocks and the hole "ends at or before `MAX_LBA + 1`" (`stream.rs:324-331`); coalesce by LBA adjacency only, since hole entries carry no address, and split at `u32::MAX` and at the `Holes` payload bound (`MAX_WIRE_PAYLOAD`, `fmt/constants:170`).

7.16 **The walk is by birth, the receiver applies by LBA.** Two LBAs in one page share a read, but the receiver's CP must apply the delta as `(vol, lba, entry-with-its-own-addresses)`; are the receiver's `CpInput::deltas` entries born at the *receiver's* CP or do they keep the sender's birth? `prepare_cp` accepts any `birth_cp <= cp_seq` (`map:1044`), so both are possible; the choice decides whether a later `diff` on the replica agrees with the source's (Q5).

---

## 8. Open questions for the lead

Q1. **Which view does `pool verify` attest on a running pool:** the committed disk state alone (pages + directory + deltas re-derived, ignoring the engine's overlay and `data_live`), the engine's in-memory view (`map.volumes()` with overlays, `snapshots[].pinned`, `data_live`, `freed_pending`), or both with a reconciliation section? The map can serve either; the report shape differs.

Q2. **The metadata liveness double-count at boot (7.9):** fix `boot:697-703` to deduplicate regions by pba before the attestation lands (so the reconciliation starts clean), or let the attestation report it and fix after? And should metadata reconciliation be exact (needs `freed_pending`) or an inequality (`referenced <= recorded`)?

Q3. **The sender's base when the pin is released or the base snapshot is gone** (I16): `NoCommonBase -> Resync` per `docs/05` section 7.3, or refuse to send? Who detects a rollback below the base — the sender (compare `record.birth_cp` of the base against the volume's rollback generation) or the receiver (`BaseMismatch`)?

Q4. **The delta's identity:** `DeltaBegin` carries `cp_seq` and `cp_id` (the CP nonce, `docs/05:669-672`). The target snapshot's `SnapRecord` carries `birth_cp` but not that CP's nonce; the superblock keeps only the current `cp_nonce`. Should `SnapRootPage` (which has 3,972 reserved bytes) record the nonce, or does the delta take a nonce of its own?

Q5. **Births on the replica (7.16):** does the receiver's CP write the sender's `birth_cp` into its map entries (so `diff` on both sides agrees and a reverse resync is by birth) or its own CP (simpler, `prepare_cp` happy either way)? Note a sender birth above the receiver's CP is refused by `prepare_cp` (`map:1044`), so keeping sender births forces the receiver's CP numbering to stay ahead.

Q6. **Should the tolerant enumerator (5.1) live in `otap-map` or in `otap-cp`?** In the map it is ten lines over `load_page` and keeps the crate's "no I/O" rule; in the engine it needs `BackstoreReader` made `pub(crate)` and reimplements `MapPage::read` handling.

Q7. **Granularity of the attestation's page reads on a real device:** one 16 KiB read per page (as `load_page`) or coalesced reads of whole metadata segments scanned for pages the trees name? The latter needs the per-segment list of referenced pages, which `PinnedRoot::regions`/`volume_regions` give.

Q8. **Does scope (B)'s "incremental tables" touch the map crate at all?** The brief names the catalogue and snapshot tables; the status document (`docs/06:536-539`) also names "a fold rewrites the whole index of each dirty volume" and the segment table as Step 6's incremental tables. If the index rewrite is in scope, the hook is `prepare_cp:1324-1338` (one `index_image` per dirty volume) and the format of `MapRoot` (one contiguous index region, `index_crc` over all of it), which cannot be made incremental without a format ruling.
