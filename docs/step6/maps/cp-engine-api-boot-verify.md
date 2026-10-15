# Map: the engine's public surface and the boot verification (for Step 6 scopes B and C)

Read-only survey of `E:/Coding/OpenTAP` at the Step 5 commit (5114866, format 2.3). Every
line number below is from the files as they stand today; "E-numbers" are sections of
`docs/07-engineering-decisions.md`, "F-rulings" are the crate-root documentation of
`crates/otap-format/src/lib.rs` (lines 68-228), "S-rules" are E06 (docs/07 lines 106-163).

---

## 1. Scope: what was read

| File | Lines read | Why |
|---|---|---|
| `crates/otap-cp/src/lib.rs` | 1-115 (all) | crate docs, re-exports |
| `crates/otap-cp/src/traits.rs` | 1-501 (all) | `Backstore::read`, `IoError`, profile, `Net` |
| `crates/otap-cp/src/layout.rs` | 1-327 (all) | pba arithmetic |
| `crates/otap-cp/src/segments.rs` | 1-304 (all) | `SegmentTable`, `set_live_bytes`, `from_image` |
| `crates/otap-cp/src/boot.rs` | 1-796 (all) | ring ranking, candidate verification, tearing |
| `crates/otap-cp/src/meta.rs` | 1-348 (all) | region placement, `bytes_per_segment` |
| `crates/otap-cp/src/cleaner.rs` | 1-278 (all) | `scan`: the only existing summary-vs-map reconciliation |
| `crates/otap-cp/src/engine.rs` | 1-3352 (all), 3600-3760, 3813-4620, 4620-5449 (`prepare_meta_inner` whole), 5451-6266 | public API, `audit_liveness`, liveness accounting, deferred frees, commit |
| `crates/otap-map/src/lib.rs` | 1-910, 1270-1345, 1600-1875 | `MapSet`, `VolumeMap`, `PinnedRoot`, `diff`, `pinned_diff`, `replay` |
| `crates/otap-format/src/{lib,superblock,stripe,map,records,types,constants}.rs` | all | layouts, rankings, F-rulings |
| `crates/otap-format/src/{catalog,checksum,stream}.rs` | signatures, `StreamHeader`, `HoleRecord`, `FrameKind` | |
| `crates/otap-backstore/src/{lib,file}.rs`, `uring.rs` 1-60, 221-256 | | synchronous read contracts |
| `crates/otap-sim/src/{lib,harness,net}.rs`, `backstore.rs` 1-140, 410-440 | | test patterns |
| `crates/otap-sim/tests/step5_sim_tests.rs` 1-130, 1370-1409; `review_step5_sim_tests.rs` 1-60, 905-935; `review_sim_tests.rs` 355-410, 590-630, 990-1030 | | test patterns, `PROPTEST_CASES` |
| `crates/otap-stream/src/lib.rs` (all), `frame.rs` 230-610, `cursor.rs` 1-90 + API list, `flow.rs` API list | | what a sender must fill |
| `crates/otap-daemon/src/pool.rs` 1-64, 430-600; `daemon.rs` 270-345; `backstore.rs` 150-222; `tests/daemon_tests.rs` 1-110 | | where a pass runs, in-process daemon tests |
| `docs/07` E06, E14-E17, E21-E25, E30-E39; `docs/06` sections 7-8; `docs/10` Step 6 brief (578-627); `docs/05` 7.3 and the `DeltaSource` passages; `docs/09` 40-70, 120-145; `docs/reviews/step4-adversarial-qa.md` 28-57 | | rulings, limits, the brief |

---

## 2. Public API and key internals

### 2.1 `otap-cp` traits (`crates/otap-cp/src/traits.rs`)

- `pub trait Backstore` (227-260). The read is **synchronous**:
  `fn read(&mut self, pba: u64, dst: &mut [u8]) -> Result<(), IoError>` (250): reads `dst.len()` bytes
  (whole 4 KiB blocks) at block number `pba`. Writes are asynchronous (`Tag`, `poll`). `profile()` (229).
  `impl<B: Backstore + ?Sized> Backstore for Box<B>` (381-417), which is how the daemon's
  `Engine<Box<dyn Backstore>, ...>` works (pool.rs:33).
- `IoError` (45-66, `#[non_exhaustive]`): `Device { errno }`, `OutOfRange { pba, bytes }`,
  `BadLength { bytes }`, `Corrupt`. `Busy` (32) is "nothing was done, retry".
- Read contracts of the three implementations:
  - `FileBackstore::read` (`otap-backstore/src/file.rs` 186-193): `check` (110-121) refuses
    `bytes == 0` or not a 4096 multiple (`BadLength`) and an end beyond `capacity_bytes`
    (`OutOfRange`); then seek + `read_exact`; an OS error becomes `Device { errno }`. No alignment
    requirement on `dst`.
  - `SimBackstore::read` (`otap-sim/src/backstore.rs` 410-425): `BadLength` on a non-multiple,
    `OutOfRange` past the device, reads the *current* image (cached writes included).
  - `UringBackstore::read` (`otap-backstore/src/uring.rs` 226-234): same `check`, then
    `BlockRing::read_at`; Linux only, never executed.
- Zero-copy views (`StripeSummary::parse`, `<[PageIndexEntry]>::ref_from_bytes`) need an 8-aligned
  buffer; every existing reader uses a `vec![0u8; n]` and relies on the allocator's alignment
  (boot.rs 632, 644, 652; cleaner.rs 204, 213-214). Page-sized views need 4096 alignment; the code
  uses the copying `read` paths for those (`MapPage::read`, `SuperblockSlot::read_from_bytes`).
- `MediaProfile` (162-221): `trusts_opaque_barrier()` (218) decides the deferral depth (E21).
- `Net` (494-501) and `EndpointId` (466): the in-memory transport contract scope C builds on.

### 2.2 `DiskLayout` (`crates/otap-cp/src/layout.rs`)

Fields (82-103): `stripe_bytes`, `stripes_per_segment`, `segment_bytes`, `segments` (numbered
`1..=segments`), `summary: SummaryLayout`, `slot_stripes`, `intent_stripes`, `seg_base`, `b_base`,
`capacity_bytes`. Device picture (3-5): `| A0 | A1 | I0 | I1 | seg 1 .. seg n | gap | B0 | B1 |`.

- `new(&Geometry)` (107-154): refuses a stripe outside `[MIN_STRIPE_BYTES, MAX_STRIPE_BYTES]` or
  not a 4 KiB multiple, a segment not a whole number of stripes or above 256 MiB, a device without
  room for the fixed regions plus two segments, more segments than a `u32`.
- `blocks_per_stripe()` 158, `blocks_per_segment()` 164, `data_blocks_per_stripe()` 170 (`summary.nblocks`).
- `stripe_pba(pseg, idx) -> Option<u64>` 189 (`None` for `NULL_PSEG`, `pseg > segments`, `idx >= stripes_per_segment`).
- `block_off(idx, i) -> u32` 197: **what a map entry's `off` holds** for data block `i` of stripe `idx`:
  `idx * blocks_per_stripe + summary.block_offset(i) / BLOCK_SIZE`.
- `block_pba(pseg, off) -> Option<u64>` 203: the block number of a map entry (`read_entry` uses it).
- `pseg_of_pba(pba)` 215, `stripe_of_off(off) = off / blocks_per_stripe` 226, `stripe_of_pba(pba) -> Option<(pseg, idx)>` 319.
- `sb_pba(SbSlot)` 232 (`A0` at 0, `A1` at `slot_stripes` stripes, `B0` at `b_base`, `B1` after it), `intent_pba(which)` 245.
- `segment_first_pba` 282, `segment_end_pba` 288, `segments_of_region(pba, pages) -> Vec<(pseg, bytes)>` 297-314
  (blocks outside every segment are not reported).
- Inverse of `block_off` (not provided; derive it): `idx = off / blocks_per_stripe`,
  `i = off - idx * blocks_per_stripe - summary.summary_bytes / BLOCK_SIZE`.

### 2.3 `SegmentTable` (`crates/otap-cp/src/segments.rs`)

- `entry(pseg) -> Result<&SegEntry, SegmentError>` 86 (refuses `NULL_PSEG`), `iter()` 107 (pseg 1..),
  `free_count()` 151, `used_segments()` 200, `open_segments(kind)` 191, `is_free(pseg)` 301.
- `set_live_bytes(pseg, live_bytes: u32)` 179-187: writes the field and re-seals the CRC; refuses a free entry.
- `image()` 209 (every entry, padded to pages) and `from_image(bytes, segments)` 218-246: an all-zero
  entry is free; every other entry must `verify()` and carry `phys == index`; trailing bytes zero.
- Comment at the top (4): "Step 4 keeps `lseg == pseg` (no relocation)". Still true after Step 5
  (E34/E37: relocation tables unused; `SegState::Relocated` and `reloc_index` reserved).
- There is **no public accessor on `Engine` for the table itself**: only `free_segments()`,
  `live_blocks(pseg)`, `meta_live_bytes(pseg)`. A verifier either lives inside `otap-cp` or reads
  the committed table from the slot's `seg_table` root.

### 2.4 Boot (`crates/otap-cp/src/boot.rs`)

Public:
- `CACHE_PAGES` 45; `BootError` 50-80 (`NoSuperblock(FormatError)`, `NewerFormat`, `OldLayout { ver }`,
  `NoVerifiableCp { tried: Vec<(u64, TornReason)> }`, `Io(IoError)`, `Geometry { recorded, profile }`).
- `TornReason` 125-150: `SegmentTable(String)`, `Map(String)`, `Snapshot(String)`,
  `Stripe { lseg, idx, reason: FormatError }`, `RootOutOfRange { field }`, `Io(IoError)`.
- `RingImages { slots: [SuperblockSlot; 4], intents: [IntentSlot; 2] }` 169-174 and
  `pub fn read_ring<B: Backstore>(b, layout) -> Result<RingImages, IoError>` 177-191: reads all four
  slots raw at `layout.sb_pba(slot)` (`SUPERBLOCK_SLOT_SIZE` = 20480 bytes each) and both intent
  slots; **no verification**. This is the reusable entry point for an attestation of the ring.
- `BootSnapshot { record, pinned: Option<PinnedRoot>, damage: Option<String> }` 196-203.
- `BootState` 207-246: `slot`, `intents`, `map`, `segments`, `open_data`, `open_meta`, `torn`,
  `meta_live: HashMap<u64, u64>`, `protected`, `snapshots`, `table_regions`, `cps_since_fold`,
  `catalog`, `catalog_ref`, `catalog_damage`; `rewound()` 251.
- `pub fn evaluate<B>(b, layout, agg_id) -> Result<BootState, BootError>` 257-348.

Private (the implementer will want `pub(crate)` or a tolerant copy): `Loaded` 350, `Walk` 366
(`regions`, `roots`, `indexes`, `dir`, `deltas`, `snapshots`, `table_regions`, `stripes`, catalog),
`walk_cp` 385-533, `load_snapshot` 538-575, `load_cp` 577-772, `read_root` 774-796.

### 2.5 `Engine` public surface (`crates/otap-cp/src/engine.rs`)

Constants: `MAX_REQUEST_BLOCKS` 37, `MAX_PUNCH_BLOCKS` 41, `META_RESERVE_SEGMENTS = 2` 46,
`MAX_USER_SNAPSHOTS = 255` 49, `MAX_RESERVED_SNAPSHOTS = 16` 53, `reserve_for(segments)` 61-70.

Structs an implementer reads:
- `PeekSource` 854-863: `Arena | Disk | Unmapped | Hole`.
- `VolumeStatus` 867-890: `present`, `serving`, `size_blocks`, `rollback_gen`, `nvfail`, `clear_pending`,
  `active_windows`, `offline`, `name`, `op_pending`.
- `SnapshotInfo` 894-919: `snap_id` (monotonic *between* rewinds; reused after one), `uuid`, `vol`,
  `cp_seq` (= `record.birth_cp`), `kind`, `origin`, `created_wall_ns`, `damaged`, `flags` (PIN bits),
  `name`, `deleting`. **No nonce of the pinned CP** (see open question 1).
- `LivenessDrift { pseg, counted, found }` 924-931 (blocks, not bytes).
- `PoolStatus` 935-984: `cp_seq`, `fold_cp`, `cps_since_fold`, `log_bytes_since_fold`, `dirty_pages`,
  `overlay_entries`, `free_segments`, `segments`, `reserve_segments`, `utilization`, `admissible_blocks`,
  `unflushed_blocks`, `arena_occupancy`, `flush_policy`, `flush_opaque`, `discard`, `warnings`, `snapshots`,
  `pool_full`, `serving`, `volumes`, `catalog_entries`, `cleaned_segments`, `relocated_blocks`.
- `Phase` 988-1001: `Idle | Staging | Metadata | Flushing | CommitA | CommitB`.
- `OperatorWarning` 263-297: `FlushOpaqueTrusted`, `MapPageDamaged { vol, page_no }`,
  `SnapshotDamaged { snap_id, vol, reason }`, `CatalogDamaged { reason }`.
- `Event` 334-532 (`#[non_exhaustive]`): `Booted {..}`, `CpCommitted { cp_seq, trigger, gen, blocks, max_seq }`,
  `CpFailed`, `Folded`, `IntentWritten`, `NvfailCleared`, `VolumeCreated`, `SnapshotCreated { snap_id, vol, cp_seq }`,
  `SnapshotFailed { uuid, vol, reason }`, `SegmentFreed { pseg, discarded }`, `SegmentCleaned { pseg, relocated, cp_seq }`,
  `CleanSkipped`, `SnapshotDeleted { snap_id, vol, cp_seq, released }`, `SnapshotPinned { snap_id, flags }`,
  `VolumeRolledBack`, `VolumeResized`, `VolumeDeleted`, `OperatorWarning(_)`, `ArenaDamage`, `Internal { what }`,
  `Stopped { cp_seq, clean }`.
- `EngineError` 537-696 (`#[non_exhaustive]`): `Format`, `Arena`, `Index`, `Map(MapError)`, `Meta`, `Layout`,
  `Segment`, `Boot(BootError)`, `Io(IoError)`, `NoSpace`, `NoSuchVolume { vol }`, `VolumeExists`, `BadSize`,
  `MetadataTooLarge`, `NvFailed`, `Corrupt { vol, lba }`, `NoSuchSnapshot { snap_id }`, `SnapshotExists { uuid }`,
  `SnapshotDamaged { snap_id }`, `SnapshotOrder { snap_id, base }`, `VolumeSizeMismatch`, `MapDamaged`,
  `FlushOpaqueRefused`, `SnapshotPinned { snap_id, flags }`, `SnapshotLimit`, `VolumeOnline`, `VolumeDirty`,
  `NewerSnapshots`, `HasSnapshots`, `VolumeBusy`, `NameExists`, `NoSuchName`, `NotServing`, `Unsupported`, `Internal`.

Methods (all on `Engine<B: Backstore, F: FrontDoor, C: Clock>`):

| Method | Line | Meaning |
|---|---|---|
| `format(&mut B, &C, &EngineConfig) -> Result<(), EngineError>` | 1566 | wipes the ring and both intents, writes CP 0 (`CleanShutdown`, volumes present, nothing mapped, geometry recorded) to A then B |
| `boot(cfg, B, F, C, HugeArena) -> Result<Self, BootFailure<B,F,C>>` | 1637 | `evaluate`, size check, arena attach, `assemble`, then starts the boot CP (`CpTrigger::Boot`); serves once it lands |
| `tick(&mut self) -> Result<(), EngineError>` | 2076 | one turn (order in section 4.9) |
| `request_cp()` 2122, `request_clean()` 2129, `request_fold()` 2224, `shutdown()` 2240 | | triggers |
| `nvfail_clear(vol) -> Result<u32>` | 2251 | durable at the next CP |
| `create_volume(vol, size)` 2273, `create_named_volume(name, size) -> u32` 2720, `set_volume_name` 2744 | | |
| `create_snapshot(vol, uuid, kind, origin)` 2302, `create_named_snapshot(vol, name, kind, origin) -> Uuid` 2315 | | taken at the next CP, which folds; limits by class |
| `delete_snapshot(snap_id)` | 2464 | refuses while any PIN bit is set (`effective_pins`, pending changes included) |
| `set_snapshot_pins(snap_id, flags)` | 2500 | durable at the next CP, `Event::SnapshotPinned` |
| `set_replication_base(vol, Option<snap_id>)` | 2523 | sets `PIN_REPLICATION` on the new base, clears it on every other snapshot of the volume, same CP |
| `set_volume_online(vol, online)` 2565, `rollback_volume` 2627, `resize_volume` 2667, `delete_volume` 2697 | | |
| `volumes() -> Vec<(u32, VolumeStatus)>` 2806, `volume(vol) -> Option<VolumeStatus>` 3241 | | |
| `audit_liveness(&mut self) -> Result<Vec<LivenessDrift>>` | 2820 | section 4.3 |
| `snapshots() -> Vec<SnapshotInfo>` | 2895 | id order |
| `snapshot_read(snap_id, lba, dst) -> Result<Vec<PeekSource>>` | 2916 | through the pinned root, overlay-free |
| `snapshot_diff(snap_id, base: Option<u64>) -> Result<Vec<(u64, MapEntry)>>` | 2958 | section 4.7 |
| `live_diff(vol, base: Option<u64>) -> Result<Vec<(u64, MapEntry)>>` | 2995 | committed map, overlay included |
| `pool_status() -> PoolStatus` | 3028 | |
| `reserve_segments()` 3093 | | |
| `peek(vol, lba, dst)` 3100 (arena first), `peek_disk(vol, lba, dst)` 3117 (committed map only) | | per-block `PeekSource`; CRC-checked |
| `map_entry(&mut self, vol, lba) -> Option<MapEntry>` | 3157 | **swallows errors**: `None` for unmapped, no root, or an unreadable page |
| `take_events() -> Vec<Event>` | 3166 | `mem::take`: single consumer |
| `is_serving` 3172, `is_stopped` 3178, `is_pool_full` 3185, `is_quiet` 3194 | | |
| `cp_seq()` 3204, `cp_nonce()` 3210 | | the committed CP and its nonce only |
| `phase() -> Phase` 3216, `nvfail_set()` 3229 | | |
| `free_segments()` 3276, `live_blocks(pseg)` 3282 (`data_live`), `meta_live_bytes(pseg)` 3288 (`meta_live`) | | |
| `stats()` 3294, `layout() -> &DiskLayout` 3300, `config()` 3306, `map() -> &MapSet` 3312 | | `map()` is `&`: `MapSet::get/diff` need `&mut` |
| `front_door() -> &mut F` 3317, `backstore() -> &mut B` 3322, `arena_memory_mut` 3328, `into_parts` 3336 | | |

Private fields a verifier needs (1364-1483): `segments` 1373, `open_data` 1374, `open_meta` 1375,
`meta_live` 1378, `data_live` 1380, `freed_pending: VecDeque<(u64, Vec<RootRef>)>` 1383,
`boot_protect: VecDeque<(u64, Vec<u64>)>` 1386, `table_regions` 1388, `snapshots: Vec<SnapState>` 1389,
`dead_data_pending: VecDeque<(u64, Vec<u64>)>` 1432, `freed_during_cp` 1436, `discarding` 1443,
`pending_discards` 1445, `job_data_segments` 1428, `state: State` 1462, `cp_seq` 1447, `cp_nonce` 1448.
`SnapState { record: SnapRecord, pinned: Option<PinnedRoot>, damage: Option<String> }` 1075-1092.
`BackstoreReader<'a, B>(&'a mut B)` 1351-1361 adapts `Backstore::read` to `otap_map::PageReader`
(error -> `MapError::Io { pba, bytes, reason }`); the pattern `let Engine { map, backstore, .. } = self;`
(2827, 2983, 3158) is how methods borrow both.

### 2.6 `MetaStream` (`crates/otap-cp/src/meta.rs`) and the cleaner (`cleaner.rs`)

- `MetaStream::new(layout, &segments, open_meta, cp_seq, reserve)` 108 works on a **copy** of the
  table; `reserve(pages)` 186, `place_at` 220, `place_region` 241, `regions()` 254, `segments()` 261,
  `segments_mut()` 266, `bytes_per_segment()` 272-283 (bytes placed per segment, via
  `segments_of_region`), `finish()` 290-341 (stripes, table, next open segment).
- `cleaner::References` 138-153 (`read`, `live`, `snapshots`, `replaced_by_cp`) and
  `cleaner::scan(layout, agg_id, pseg, refs) -> Result<Scan, SkipReason>` 197-264: the existing
  "read every stripe summary of a segment and ask the map who references each block" routine.
  Identity rule used there (223-226): skip a stripe whose header does not name `(agg_id, pseg, idx)`;
  an all-zero summary is "never written" (216); any other unparsable summary skips the segment (217-221).
  Expected entry rebuilt from a descriptor (231-235): `MapEntry::data(pseg, layout.block_off(idx, i), SECTORS_PER_BLOCK, desc.birth_cp, desc.crc32c)`.
- `Scan { blocks, referenced }` 268-278; `candidates(..)` 161-183.
- `EngineRefs` (engine.rs 6206-6266) is the engine's `References` impl: `live` = `map.get == entry`,
  `snapshots` = every snapshot of the volume with `birth_cp >= entry.birth_cp` whose `pinned_get == entry`
  (a damaged one is `SkipReason::DamagedSnapshot`).

### 2.7 `otap-map` (`crates/otap-map/src/lib.rs`)

- `PageReader` 156-159 (`read_blocks(pba, dst)`); `MapError` 74-124 (`Format`, `Io { pba, bytes, reason }`,
  `NoSuchVolume`, `VolumeExists`, `OutOfRange { vol, lba }`, `BadSize`, `Replay { field, value }`, `NoSpace`, `Internal`).
- `index_root_ref(&MapRoot) -> RootRef` 318-324 (`pba = index_pba`, `pages = index_pages * 4`, `crc = index_crc`);
  `page_root_ref(&PageIndexEntry) -> RootRef` 328-330 (`pages = 4`, `crc = page_crc`);
  `volume_regions(&VolumeMap)` 1868-1872; `MAP_PAGE_BLOCKS = 4` 65, `INDEX_PAGE_BLOCKS = 4` 67.
- `VolumeMap` 335-399: `root() -> &MapRoot` 360, `index() -> &[PageIndexEntry]` 366 (exactly
  `map_pages_for(lba_count)` entries; `index_entries` 295-314 refuses a non-NONE entry beyond),
  `lba_count()` 372, `overlay()` 383 (entries changed since the fold, LBA order), `dirty_pages()` 389.
- `MapSet`: `volumes()` 667, `volume(vol)` 673, `delta_roots()` 680 (delta regions since the fold),
  `fold_cp()` 656, `committed_cp()` 662, `roots_table()` 724, `parse_delta` 735, `replay` 782-864,
  `get(reader, vol, lba)` 875-892 (overlay, else page, else `UNMAPPED`; `OutOfRange` past `lba_count`),
  `diff(reader, vol, base_cp, f)` 1620-1673, `pin(vol)` 1677 (only with an empty overlay),
  `pinned_get(reader, &PinnedRoot, lba)` 1693-1711, `pinned_diff(reader, &PinnedRoot, base_cp, f)` 1714-1734.
- `PinnedRoot { vol, root: MapRoot, index: Vec<PageIndexEntry> }` 1759-1766: `load(root, index_bytes)` 1771,
  `get(cache, reader, lba)` 1794, `diff(cache, reader, base_cp, f)` 1814, `references_page(page_no, pba)` 1839,
  `references_index(pba)` 1847, `regions()` 1853-1862 (index + every non-NONE page).
- `load_page` 246-267: reads `MAP_PAGE_SIZE` at `entry.page_pba` and `MapPage::read(&bytes, entry)`
  (CRC + validate + `max_birth`), caches by pba (pages are immutable once written).
- Fold bookkeeping (1277-1341): the root's `max_birth` is `max(old root.max_birth, max_birth of every
  rewritten page)`; the root is rewritten only when a page or a resize changed; entries beyond a shrunk
  volume's end are set `UNMAPPED` in the boundary page (1295-1300).

### 2.8 `otap-format`

Superblock (`superblock.rs`):
- `SbSlot::{ALL, for_commit(cp_seq) -> [A?, B?], parity, location}` 122-179; `SbLocation` 112.
- `Superblock` fields 431-516: `run_state` 437, `slot` 440, `agg_id`, `owner_epoch`, `cp_seq`, `cut_seq`,
  `cp_nonce` 451, `intent_seq_seen`, `opened_stamp`, `wall_ns`, `acked_bytes`, `vol_count` 462,
  `vol_table_crc`, `write_active` 466, `nvfail` 468, `freed_pending: [RootRef; 2]` 473 (**reserved, always NONE**;
  the engine never sets it: `build_slot` 5589-5691), `map_roots` 475, `seg_table` 477, `reloc_index` 479 (unused),
  `cp_log_head` 481, `snap_table` 483, `cluster_state`, `durable_log`, `inbound`, `open_data_pseg` 492,
  `open_data_idx` 494, `geom_*` 499-504, `catalog` 507, `cp_stripes` 511, `crc32c`.
- `Superblock::verify` 583-646 (magic, major, CRC, run_state, slot parity == `cp_seq & 1`, bounds, geometry),
  `geometry()` 546, `is_clean_shutdown()` 684, `commit_key()` 696.
- `SuperblockSlot { sb, vols }` 705; `verify` 737-768 (page, table CRC, bitset/table agreement, `acked_bytes` sum);
  `read(bytes)` 778 (copy, any alignment); `rank(agg_id, (SbSlot, &Slot) iter) -> Result<Vec<&Slot>>` 789-812;
  `select` 815. Ranking rule (793-811): a slot with this magic and a higher major is `NewerFormat` (hard error);
  a slot counts only if `verify()` passes, `agg_id` matches and `sb.slot == location.index()`; order by
  `(owner_epoch, cp_seq, cut_seq, is_A)` descending; empty -> `NoValidSlot`.
- `boot_write_active(sb, intents)` 933-947.

Stripe (`stripe.rs`):
- `StripeHdr` 118-145 (`cp_seq`, `cp_nonce`, `lseg`, `stripe_idx`, `nblocks`, `group`, `summary_crc`, `hdr_crc`);
  `verify_hdr` 198, `check_identity(&StripeId)` 206-224 (agg_id, lseg, stripe_idx, cp_seq, cp_nonce), `read` 234, `id()` 260.
- `StripeId { agg_id, lseg, stripe_idx, cp_seq, cp_nonce }` 274-285.
- `StripeDescriptor { vol_lba_kind, birth_cp, crc32c }` 305-313; `vol()` 374, `lba()` 380, `kind()` 392,
  `validate` 404-423 (hole CRC must be `ZERO_BLOCK_CRC32C`, birth >= 1).
- `SummaryLayout { stripe_bytes, nblocks, summary_bytes }` 457-528; `block_offset(i)` 516.
- `StripeSummary::parse(region)` 547-585 (header CRC, summary CRC over `[64..summary_bytes)`, every descriptor
  validated, bitmap must equal `derive_vol_bitmap`); `parse_expected(region, &StripeId)` 590-594 adds identity;
  `descriptors()` 610, `hdr()` 598. Region must be 8-aligned and at least `summary_bytes(nblocks)` long.
- Module doc 20-28: `parse` proves integrity, not identity; "a physical location reused across CPs can
  hold an older stripe whose CRCs pass".

Map layouts (`map.rs`): `MapEntry` 54-61 (`addr = lseg:36 | off:16 | plen:12`, `birth_cp`, `crc32c`),
`lseg()` 141, `off()` 147, `plen()` 153, `is_unmapped` 159 (`addr == 0 && birth == 0`), `is_hole` 165
(`addr == 0 && birth != 0`), `is_data` 171, `validate` 177; `MapPage` 198 (1024 entries, 16 KiB), `verify(&entry)`
254-264, `read` 274, `page_of`, `slot_of`, `first_lba`; `PageIndexEntry { page_pba, max_birth, page_crc }` 286-295;
`MapRoot` 383-404 (`vol`, `index_pba`, `index_pages`, `index_crc`, `lba_count`, `checkpoint_cp_seq`, `max_birth`),
`verify_index(&[u8])` 458-471, `is_absent` 531; `parse_map_roots(bytes, vol_count)` 562-596.

Records (`records.rs`): `SegEntry { phys, stripe_data_bytes, live_bytes, birth_cp, lap, kind, state, rsvd, crc32c }`
129-153, `validate` 225-267 (`live_bytes` must be a 4 KiB multiple at most 256 MiB), `verify` 271;
`SegState` 79-95 (`FreedPending` "never written"), `SegKind` 51; `SnapRecord` 359-384 (`snap_id`, `snap_uuid`,
`map_root_pba`, `deadlist_pba` (0), `created_wall_ns`, `birth_cp: u32`, `vol`, `kind`, `origin`, `flags`),
`PIN_USER/SCHEDULE/REPLICATION/CLONE/MASK` 344-352; `CpLogRec` 486-499 (`map_entry()` 541); `CpLogDir { cp_seq,
fold_cp_seq, deltas, pages }` 995-1006, `decode(bytes, agg_id)` 1056-1107; `SnapRootPage { snap_id, agg_id,
birth_cp: u64, root: MapRoot, deadlist, crc32c }` 1123-1144, `read` 1231; `parse_snap_table` 1258-1286 (ids strictly
ascending); `CpStripe { pseg, idx, kind }` 1335-1344, `parse_cp_stripes` 1408-1441 (strictly ascending `(pseg, idx)`).

Types (`types.rs`): `FormatError` 23-91 (`BadMagic`, `BadVersion`, `NewerFormat`, `BadCrc { region, computed, stored }`,
`BadLength`, `Misaligned`, `BadField`, `OutOfRange`, `IdentityMismatch { field }`, `NoValidSlot`);
`RootRef { pba, pages, crc32c }` 479-539, `is_none` (`pages == 0`), `bytes()`, `verify(region)` 529-538 (length + CRC).
Checksum (`checksum.rs`): `crc32c(&[u8])` 23, `crc32c_combine(a, b, len_b)` 30, an incremental `Crc32c` hasher
(`new/update/finalize` 49-62), `blake3_256` 141. Catalogue: `parse_catalog(bytes) -> Vec<CatalogRec>` 272,
`CatalogRec` 78-99 (`kind`, `name_len`, `vol`, `snap_uuid`, `created_wall_ns`, `name`, `crc32c`).
Constants: `BLOCK_SIZE` 4096, `MAX_CP_SEQ = u32::MAX`, `SUPERBLOCK_SLOT_SIZE` 20480, `INTENT_SLOT_SIZE` 4096,
`MAP_PAGE_SIZE` 16384, `INDEX_PAGE_SIZE` 16384, `MAX_SEG_BYTES` 256 MiB, `NULL_PSEG = NULL_LSEG = 0`,
`SECTORS_PER_BLOCK = 8`, `ZERO_BLOCK_CRC32C = 0x98f9_4189`, `SNAP_ROOT_PAGE_SIZE` 4096, `FORMAT_VERSION = 0x0203`.

### 2.9 What the sender fills (`otap-stream`, `otap-format/src/stream.rs`)

`FrameKind` 41-67 (`Hello 1 .. Pong 12`). `StreamHeader` 135-166: `rel`, `chunk_idx`, `cp_seq` (the delta's
end), `base` (<= cp_seq), `cp_nonce` ("the nonce of cp_seq"), `owner_epoch`, `wire_len`, `raw_len`, two CRCs.
`HoleRecord { vol, len, lba }` 332-339. Bodies (`frame.rs`): `DeltaBegin { mode: Steady | Increment, chunks, bytes,
cut_age_ms, write_active: VolBitSet, tags: Vec<SnapTag { uuid, retention_s }>, acked: Vec<AckedBytes { vol, bytes }> }`
297-313 (`DeltaBeginBody` 268-293 carries `from_snap`/`to_snap` for an increment); `VolEvent { event: Create|Resize|
Delete|SnapTag, vol, class, size_blocks, stripe_bytes, retention_s, uuid, name }` 420-437; `Chunk { image: Bytes,
codec, raw_len }` 443-451 (`raw(image)` 455, `stripe_hdr()` 490, `summary()` 507 needs 8-aligned bytes);
`Holes { records }` 517; `DeltaEnd { digest: [u8;32] (BLAKE3 over images in index order, S6), chunks, bytes }` 540-549;
`Landed { idx, credits, landed_contig }` 569-577; `Committed { applied_cp_seq, applied_epoch }` 593-598.
Cursor API (`cursor.rs`): `Cursor::new(applied_cp_seq, applied_epoch, credits)` 325, `begin` 343, `land` 387,
`commit(epoch)` 409, `abort` 424, `resume_point` 431, `to_wire/from_wire` 450/475, `to_page/from_page` 505/522;
`ResumeBitmap` 73. Flow: `credits_for(bytes)` 28, `ReceiverWindow` 41, `SenderWindow` 148.

### 2.10 The daemon's engine thread (`crates/otap-daemon/src/pool.rs`, `daemon.rs`)

`PoolEngine = Engine<Box<dyn Backstore>, DaemonFrontDoor, SystemClock>` (33). `Pool::tick_raw` 435-443
(one `Engine::tick`; busy when stats moved, not quiet, or requests in the engine), `tick` 447-453 (notes events),
`wait_for(what, pred)` 476-520 (ticks until an event matches, `op_timeout`, interruptible), `wait_until` 523-556,
`wait_quiet` 561-566 (except on a full pool), `wait_cp_outcome` 570-587, `sync_now` 592-596 (quiet, `request_cp`,
wait). `Daemon::run` (daemon.rs 278-318): `tick`, then `try_recv` a control call when busy or `recv_timeout(idle_wait)`
when idle (200 us doubling to 2 ms, 325-331); each call is handled synchronously on this thread. Every
control request therefore runs **between ticks on the engine thread** with exclusive access to the engine.

---

## 3. Invariants and rules in force

Format and layout:
1. **F2** (lib.rs 82-92): `addr == 0` is a hole when `birth != 0`, unmapped when `birth == 0`; `lseg == 0` reserved.
   **F5**: `birth_cp >= 1` for anything written; CP 0 is the format commit. **F9**: a hole descriptor's data slot
   is present and zero (`ZERO_BLOCK_CRC32C`); map holes/unmapped carry CRC 0.
2. **F7** (lib.rs 111-117, superblock.rs 3-41): commit N writes `A(N mod 2)` then `B(N mod 2)`; a page's `slot`
   has the parity of its `cp_seq`; ranking is by `(owner_epoch, cp_seq, cut_seq)`, A wins ties; a higher major is a
   hard stop. A commit is rebuilt from `Superblock::new`, never carried forward (superblock.rs 36-41).
3. **F12-F14** (lib.rs 156-179): the directory lists delta regions since the fold in CP order plus the pages the
   writing CP wrote; a fold CP writes no delta region; the snapshot root page pins a `MapRoot`; `map_roots` has one
   slot per index up to `vol_count`; `snap_table` is id-ascending.
4. **F17** (lib.rs 212-220): every commit lists the data stripes it wrote (ingest and relocation); the boot verifies
   each by summary CRC and identity *including the CP's nonce* (boot.rs 624-674).
5. **F16/F18** (lib.rs 202-228): names are in the catalogue; `VOL_OFFLINE` in the volume table; directory pages in any CP.
6. `lseg == pseg` (segments.rs 4; E34/E37): no relocation tables; `SegState::Relocated`, `reloc_index`,
   `Superblock::freed_pending` and `SegState::FreedPending` are reserved and never written (superblock.rs 469-473,
   records.rs 89-94; Step 4 review hazard 15 recorded as a doc note).
7. **E14** (docs/07 282-310): a boot trusts only what a root names exactly (block, length, CRC) and never looks at
   a stripe or page no committed CP named; a CP sequence number may be re-issued after a rewind.

Engine and liveness:
8. **E17** (383-464): one generation per CP, one CP at a time; disk order data stripes -> metadata -> flush -> A -> B;
   the map, the generation and the arena change only after B lands (`finalize` 5711); a failed write fails the attempt
   and the generation is re-staged with a **new nonce** (`new_job` 3972-3973: `cp_seq = self.cp_seq + 1`, `new_nonce`).
9. **E21/E24** (558-616, 704-738): a region CP N stopped referencing is released after N+1 commits, N+2 under a trusted
   opaque barrier (`release_freed` 6004-6036: `keep = 2 | 1`, released when `cp + keep <= cp_seq`); the segments of
   the other verified ring slot are protected for two commits after boot (`boot_protect`, retained while
   `cp + keep + 1 > cp_seq`); a segment whose discard is in flight stays allocated (`discarding`, `discard_pending` 3866).
10. **E25/E36** data liveness rule: a data block stays live while the live map names it or a snapshot of the volume
    with `birth_cp >= block.birth_cp` exists (`pinned_by` 6175-6179); replaced entries decrement the old segment only
    when not pinned (5277-5295); a relocation (same birth, same CRC, new addr) moves one count (5269-5285); a deleted
    snapshot releases exactly the entries born after the next older snapshot that the next newer snapshot (or the live
    map) does not name at the same address (4901-5021); a drained segment is forced to 0 (5304-5320).
11. **E24** metadata liveness: `meta_live[pseg]` grows by every region a CP places (`stream.bytes_per_segment()` plus the
    reserved table region, 5321-5329) and shrinks only when a queued region is released (`release_freed` 6021-6036);
    a metadata segment is freed when `meta_live == 0`, not open, not protected, no discard pending (6046-6066).
    The on-disk `SegEntry::live_bytes` of a **data** segment is `data_live * 4096` and of a **metadata** segment is
    `meta_live`, as of the table image the CP wrote (5330-5343, `set_live_bytes`).
12. **E31**: data allocations keep `META_RESERVE_SEGMENTS` (2) free; a boot commit may take the last segment, a
    shutdown leaves one (4850-4857); a sealed data segment whose live count reaches zero is freed after the deferral
    (`note_dead_data` 6080-6105, `dead_data_pending`).
13. **E30/E32**: a read error never tears a candidate (`BootError::Io`, boot.rs 292-295); only a region that reads
    back and does not verify tears; damage to an old snapshot's root page or index is `SnapshotDamaged`, not torn
    (boot.rs 488-519, 511); a damaged map page answers `Corrupt` and blocks folds; `snapshot_diff` refuses a base
    not older than the snapshot (`SnapshotOrder`).
14. **E37/E39**: a CP that relocates carries no deletion and no volume operation; the cleaner's scan treats an
    unparsable non-zero summary as damage (`SkipReason::Summary`) and refuses to drain a segment whose referenced
    count is below the table's (`Scan::referenced`, engine.rs 4104-4119).
15. **E38**: one engine thread; control requests execute between ticks; events are drained each tick by
    `Pool::tick`/`wait_for` (`take_events` is `mem::take`, 3166-3168): there is exactly one consumer of events.
16. `is_quiet()` (3194-3200): `Idle && frozen == 0 && !cp_requested && !boot_retry && paced.is_empty()`; the daemon's
    `wait_quiet` is the hook every request without an event of its own uses (E38).
17. S-rules (E06): S1 header bounds (`base <= cp_seq`, payload <= 8 MiB), S2 canonical bodies, S3 builder refuses what
    the parser refuses, S4 bounds (`MAX_DELTA_CHUNKS = 2^24`, `CREDIT_BYTES = 4096`), S6 delta byte counts are raw
    image bytes and the digest is BLAKE3 over chunk images in index order, S8 no sockets in `otap-stream`.
18. Working agreements (docs/09): `#![forbid(unsafe_code)]` outside `otap-sys`; every public item documented
    (`missing_docs` is an error in the gates); the 10,000-case gate is mandatory and `PROPTEST_CASES` overrides any
    explicit `cases`.

---

## 4. How things happen, in order

### 4.1 `boot::evaluate` (boot.rs 257-348)

1. `read_ring(b, layout)` 262: four slots raw + two intents (`IoError` -> `BootError::Io`).
2. `SuperblockSlot::rank(agg_id, SbSlot::ALL zip images.slots)` 263-267: `NewerFormat` -> `BootError::NewerFormat`,
   any other failure -> `NoSuperblock`.
3. Newest candidate checks (268-283): minor `< 3` -> `OldLayout { ver }`; `sb.geometry()` must equal
   `(layout.stripe_bytes, layout.segment_bytes as u32, layout.capacity_bytes)` else `Geometry { recorded, profile }`.
4. Candidates newest first (284-298): `load_cp`; `Ok` selects and stops; `Err(TornReason::Io(e))` fails the boot
   (**a read error is not a torn CP**); any other `TornReason` pushes `(cp_seq, reason)` to `torn` and tries the next.
5. None verifies -> `NoVerifiableCp { tried }` (299-301).
6. Other verified slots with a different `cp_seq` are walked (`walk_cp`, 302-317) only to collect the segments
   their regions occupy into `protected` (sorted, deduped); a walk failure there is ignored.
7. `BootState` assembled (331-347); `rewound() = !torn.is_empty()`.

### 4.2 `load_cp` (577-772) and `walk_cp` (385-533): what verifies, what tears

`load_cp`:
1. `seg_table`: NONE -> `SegmentTable::empty(layout.segments)` (a fresh format); else `read_root` then
   `SegmentTable::from_image` (584-592); a bad image is `TornReason::SegmentTable`.
2. `walk_cp` (below).
3. `regions` = the walk's regions + the `seg_table` root (594-597).
4. Map: `MapSet::from_roots(agg_id, CACHE_PAGES, sb.cp_seq, dir.fold_cp_seq, &roots, load_index)` 600-613; each delta
   region replayed in directory order (`map.replay`, 614-622; errors -> `TornReason::Map("cp log replay: ..")`);
   `cps_since_fold` = distinct delta CPs above the fold; `set_delta_roots`.
5. **Stripe list** (624-674): `check_data = profile.trusts_opaque_barrier() && !sb.is_clean_shutdown()`; buffer is one
   whole stripe when checking data, else `summary_bytes` only; for every `CpStripe { pseg, idx }`:
   `layout.stripe_pba` (out of range -> `TornReason::Stripe` with `OutOfRange`), `b.read` (error -> `Io`),
   `StripeSummary::parse_expected(&buf[..summary_len], &StripeId { agg_id, lseg: pseg, stripe_idx: idx, cp_seq:
   sb.cp_seq, cp_nonce: sb.cp_nonce })` (failure -> `TornReason::Stripe { lseg, idx, reason }`); with `check_data`,
   every `Data` descriptor's block at `layout.summary.block_offset(i)` must match `desc.crc32c`
   (654-673, `BadCrc { region: "data block" }`).
6. `open_data` (675-696): `sb.open_data_pseg` must be an open data segment and `open_data_idx <= stripes_per_segment`,
   else torn (`SegmentTable`).
7. `meta_live` (697-703): bytes of every region in `regions` summed per segment via `segments_of_region`
   (**no dedup by pba**; see section 7, pitfall 2).
8. Every open segment must have a kind (704-710); the metadata append point is derived from the highest region end in
   the highest open metadata segment (711-751); older open metadata segments are sealed.

`walk_cp` (what it reads, in order; every `read_root` failure other than `Io` is `TornReason::Map`/`Snapshot`):
1. `map_roots` -> `parse_map_roots(bytes, sb.vol_count)` (395-403); region recorded in `regions` and `table_regions`.
2. For every present root: `index_root_ref(root)` read and `root.verify_index` (410-413); every non-NONE
   `PageIndexEntry` adds `page_root_ref(entry)` to `regions` (418-420) **without reading the page** (pages are read
   lazily later by the map; a damaged page is contained at read time, E32).
3. `cp_log_head` -> `CpLogDir::decode(bytes, agg_id)` (424-444); `dir.cp_seq` must equal `sb.cp_seq`; NONE means an
   empty directory with `fold_cp_seq = cp_seq`.
4. Every delta region read and verified (445-450); every directory page region read and verified (454-456: a fold's
   pages, or a relocation's rewritten snapshot pages).
5. `cp_stripes` -> `parse_cp_stripes` (457-465).
6. `catalog` (466-487): a region that does not verify or parse is **not** torn: `catalog_damage` is recorded and the
   boot goes on with no names (E39 W4); a read error still fails.
7. `snap_table` -> `parse_snap_table` (493-498); for each record `load_snapshot` (538-575): bounds-check
   `map_root_pba`, read one page, `SnapRootPage::read`, identity check (`agg_id`, `snap_id`, `birth_cp`, `root.vol`),
   then the pinned index via `index_root_ref(&root_page.root)` + `PinnedRoot::load`; regions = root page + index +
   pages. A failure tears only when `record.birth_cp == sb.cp_seq` (`own`, 500, 511); otherwise `BootSnapshot.damage`.

`read_root` (774-796): NONE -> empty; refuses `pba == 0`, an end beyond `capacity_bytes`, or more than 4 GiB
(`RootOutOfRange { field }`); one `b.read` of the whole region into a `Vec`; `root.verify(&buf)` (length + CRC).

### 4.3 `audit_liveness` (engine.rs 2820-2878): the existing reconciliation

1. For every volume in `self.map.volumes()` (volumes with a root): `map.diff(reader, vol, 0, |_, e| entries.push(e))`
   (base 0 -> every mapped entry, overlay interleaved with pages, holes included).
2. For each data entry, `seen.insert((lseg, off))` dedups across trees; `found[lseg] += 1`.
3. For every snapshot with `pinned.is_some()`: `map.pinned_diff(reader, &root, 0, ..)` and the same counting.
   Damaged snapshots (`pinned == None`) are **skipped**, so blocks only they reference show as drift (`counted > found`).
4. Over the union of `data_live` keys and `found` keys, report `LivenessDrift { pseg, counted: data_live[pseg],
   found }` where they differ.
What it does not do: read any data block, verify any CRC, touch `meta_live`, the deferred lists or the on-disk table.
It runs whole between ticks (the sim calls it after every recovery, harness.rs 1343); it mutates nothing but the map's
page cache.

### 4.4 How a committed CP's accounting is produced (`prepare_meta_inner` 4741-5449, `finalize` 5711-5995)

1. Deltas: relocation deltas first, then the generation's blocks, then holes (4749-4772).
2. `MetaStream::new(&layout, &self.segments, self.open_meta, cp32, reserve)` on a **copy** of the table (4858-4859);
   `map.prepare_cp(reader, &input, &mut stream)` places deltas, fold pages and indexes (4860-4863).
3. Snapshot deletions against the shrinking `remaining` set (4901-5021); cleaner rewrites of pinned trees
   (5023-5118); pins (5120-5128); new snapshots pinned to `prepared.fold_roots` (5130-5165); `snap_table` placed
   (5173-5179); catalogue carried or rewritten (5181-5234); `cp_stripes` placed (5236-5244); `map_roots`, the directory
   (with `extra_pages`), then the table region **reserved** so its own bytes count (5246-5254).
4. `data_live` recomputed (5256-5320) and `meta_live` (5321-5329); every used segment's `live_bytes` set
   (5330-5343); the table image placed at the reserved pba (5344).
5. `freed` (5346-5397): previous `table_regions`, dropped deltas, deleted root pages, the old catalogue, plus pages and
   indexes no remaining tree references (`page_referenced`, `index_referenced`), deduped by `pba`.
6. `table_regions` for this CP = `[map_roots, cp_log_head, snap_table, cp_stripes, seg_table]` minus NONE (5398-5401).
7. `step_meta` writes the stripes, `step_flush` flushes, `step_commit` writes A (`build_slot` 5589-5691: roots,
   `open_data_*`, rows for `carried` volumes, `write_active`, `nvfail`) then B with identical content (5545-5550).
8. `finalize`: `map.apply`, `self.segments = segments`, snapshots updated, `freed_during_cp` re-applied (5855-5864),
   `open_meta`/`meta_live`/`data_live`/`table_regions`/`cp_seq`/`cp_nonce` swapped (5865-5870), `note_dead_data`
   (5871), arena slots released, `freed_pending.push_back((cp_seq, freed))` and `release_freed(cp_seq)` (5967-5968),
   `CpCommitted`, serving on the boot CP, stopped on the shutdown CP.
9. `release_freed` (6004-6076): drops `boot_protect` entries older than `keep + 1`, pops `dead_data_pending` and
   `freed_pending` entries with `cp + keep <= cp_seq` (subtracting region bytes from `meta_live`), then frees every used
   segment that is dead (metadata: `meta_live == 0`; data: in `dead_now` and `data_live == 0`), skipping the open
   segments, protected and discard-pending ones; `WholeSegment` -> `discard` (3822) else `free_segment` (3841).
10. `free_segment` (3841-3863) zeroes the entry and removes the `meta_live`/`data_live` keys; if a CP is past
    staging, the free is queued in `freed_during_cp`.
11. A failed attempt (`fail_cp` 4256-4313) returns `job_data_segments` to free at once, keeps `self.open_data.next_idx`
    where the attempt left it, and the retry reuses `cp_seq + 1` with a new nonce.

### 4.5 The disk read path (`read_entry` 3710-3736)

`entry.is_data()` -> `layout.block_pba(entry.lseg(), entry.off())` (`None` -> `EngineError::Internal`),
`backstore.read(pba, out)`, then `crc32c(out) != entry.crc32c` -> `EngineError::Corrupt { vol, lba }`; a hole or an
unmapped entry fills zeros. `read_disk_block` 3691-3707 maps `MapError::NoSuchVolume` to `UNMAPPED`; `serve_read`
3630-3662 turns `Corrupt` and `Map(Format)` into `Status::Corrupt`, `Io` into `Status::Failed`.

### 4.6 What a stripe holds (`build_stripe` 4475-4527, `build_reloc_stripe` 4537-4611)

Ingest: descriptor `StripeDescriptor::data(vol, lba, birth = job.cp_seq, recorded_crc)` and map entry
`MapEntry::data(pseg, layout.block_off(idx, i), SECTORS_PER_BLOCK, birth, recorded_crc)`; header
`StripeHdr { kind: Ingest, cp_seq: job.cp_seq, cp_nonce: job.nonce, lseg: pseg, stripe_idx: idx, nblocks: n, group: 0 }`;
`write_summary` over `image[..summary_bytes]`. Relocation: descriptors keep `old.birth_cp` and `old.crc32c`, header
`kind: Relocate` with the relocating CP's `cp_seq`/nonce. So for every data entry the map expects:
`desc.vol == vol`, `desc.lba == lba`, `desc.kind == Data`, `desc.birth_cp == entry.birth_cp`,
`desc.crc32c == entry.crc32c == crc32c(block)`; and `hdr.cp_seq >= entry.birth_cp` (equal for ingest).

### 4.7 The diffs (what the sender gets)

- `base_cp(vol, base)` 3009-3024: `None -> 0`; `Some(id)` -> the snapshot's `birth_cp` (as u64), `NoSuchSnapshot`
  if absent or of another volume.
- `snapshot_diff(snap_id, base)` 2958-2991: the snapshot must exist and be readable (`SnapshotDamaged`); `base_cp >=
  birth` -> `SnapshotOrder`; then `map.pinned_diff(reader, &pinned, base_cp, f)` collecting `(lba, MapEntry)`.
  `pinned_diff` (1714-1734) reads only index pages with `max_birth > base_cp` and emits entries with
  `birth_cp > base_cp && !is_unmapped()`, in LBA order: data entries (current address in the *snapshot's* tree, i.e.
  after any relocation) and holes. One entry per LBA (the state at the snapshot, not a history). Nothing from the overlay.
- `live_diff(vol, base)` 2995-3007: `map.diff(reader, vol, base_cp, f)` (1620-1673): overlay entries interleaved
  with page entries, same birth filter, same order; pages with `max_birth <= base_cp` are not read.
- "base" is a snapshot **id** of the same volume; the comparison is on `birth_cp` numbers only (re-issued after a
  rewind, E14); identity across rewinds is the uuid.
- `snapshot_read(snap_id, lba, dst)` 2916-2954: `dst` whole blocks; bounds against the **snapshot's** `lba_count`
  (not the volume's current size); `pinned_get` per block then `read_entry` (CRC checked).

### 4.8 Pins and the replication base

`set_replication_base(vol, Some(id))` 2523-2560: validates the snapshot (same volume), then for every snapshot of the
volume computes `wanted = effective_pins | PIN_REPLICATION` (the new base) or `& !PIN_REPLICATION` (the rest) and calls
`set_snapshot_pins` for each that changes; `set_snapshot_pins` 2500-2517 refuses unknown bits and a snapshot being
deleted, replaces any pending change for that id, `request_cp()`. Durable at the next CP (5120-5128, `finalize`
5766-5781 emits one `SnapshotPinned` per change). `delete_snapshot` 2464-2486 checks `effective_pins` (pending changes
win), so "set the new base then delete the old" works without waiting for the CP in between.

### 4.9 `tick` (2076-2118)

`backstore.poll` -> discard completions free segments -> intent completions -> `advance_cp(cp_dones)` (the state
machine) -> `retry_intent` -> `retry_discards` -> boot retry -> paced acks -> `serve_front_door` (when serving) ->
`maybe_cut` -> `maybe_start_cp` -> `maybe_pump`. Between two ticks nothing runs; the backstore may still be completing
in-flight writes (they land in free or open segments only).

---

## 5. Hook points for Step 6

### 5.1 The attestation pass (scope B)

**Where it lives.** Inside `otap-cp` (a new `src/verify.rs` plus one `Engine` method), because what it must
reconcile is private: `segments`, `data_live`, `meta_live`, `freed_pending`, `dead_data_pending`, `boot_protect`,
`snapshots[].pinned`, `open_data`, `state`. The daemon exposes it as a `Request` handled in `Pool::handle` (pool.rs
744) like `status`; `otapctl pool verify` is an `otap-ctl` args/render addition.

**Entry point shape.** `Engine::verify_pool(&mut self, opts) -> Result<PoolAttestation, EngineError>` where the
`Err` is only for an internal failure; every finding (bad CRC, identity mismatch, read error, drift) is a record in
the report, never a panic and never a `TornReason`. Reuse: `boot::read_ring` + `SuperblockSlot::rank` for the ring;
the private `read_root`/`walk_cp`/`load_snapshot` either made `pub(crate)` with a tolerant variant (collect instead
of `?`) or re-implemented in `verify.rs` with the same bounds (`pba != 0`, end <= capacity, <= 4 GiB).

**The checks, in the order the data allows (every read through `self.backstore.read`):**

1. Ring: `read_ring` (both intents too); for each slot `SuperblockSlot::verify()` and `agg_id`/location agreement
   (the three conditions of `rank` 801); report per slot. Monotonicity: `rank` gives the order; the A and B slots of
   one commit must be byte-identical but for `sb.slot` and `crc32c`; the two pairs must carry different `cp_seq`
   (or the same `cp_seq` only after a rewind re-issue, which the nonces distinguish); the newest must equal
   `self.cp_seq`/`self.cp_nonce` when the engine is quiet. The selected slot's `geometry()` must equal the layout.
2. Roots of the committed slot: `map_roots`, `cp_log_head`, every delta, every directory page, `cp_stripes`,
   `catalog`, `snap_table`, `seg_table`: read + `RootRef::verify` (use the incremental `Crc32c` hasher to avoid a
   4 GiB `Vec` for a huge index); parse each with the format parsers. Compare `parse_map_roots` with
   `self.map.roots_table()` and the table image with `self.segments` (`SegmentTable::from_image` then `==`, the
   table derives `PartialEq`) when quiet.
3. Volumes: for every present root, index region (`verify_index`), then every non-NONE `PageIndexEntry`:
   `MapPage::read(&bytes, entry)` (CRC, `max_birth`, every entry validates); bounds: exactly `map_pages_for(lba_count)`
   entries matter, extra entries NONE; in the last page entries at `lba >= lba_count` must be `UNMAPPED`; every data
   entry's `(lseg, off)` must satisfy `lseg <= layout.segments`, `off < blocks_per_segment`, the segment's `SegEntry`
   is `Data` and `Open|Sealed`, and `off` falls in a data slot (`i < summary.nblocks` after the inverse of `block_off`).
   `root.max_birth >= max(entry.max_birth)` holds after a fold; between folds the overlay carries higher births, so do
   not assert equality. The live map adds the overlay (`VolumeMap::overlay()`) on top of the pages; the delta regions
   (`map.delta_roots()`) must replay to exactly that overlay (`MapSet::parse_delta` gives the records).
4. Snapshots: every `SnapRecord` -> root page (`SnapRootPage::read`, identity as in `load_snapshot` 558-567) -> pinned
   index -> pages; compare with `self.snapshots[i].pinned` (the engine's in-memory copy) and flag `damage` ones.
5. Stripes: for every used data segment (`self.segments.iter()` with `seg_kind == Data`, state `Open|Sealed`) and
   every stripe `idx < stripes_per_segment` (for the open segment only `idx < open_data.next_idx` as committed):
   read the whole stripe (`stripe_pba`, `stripe_bytes`), `StripeSummary::parse(&buf[..summary_bytes])`; an all-zero
   summary is "never written"; a header that does not name `(agg_id, pseg, idx)` is a stale/foreign image
   (report as "unowned", like `cleaner::scan` 223-226); `hdr.cp_seq > self.cp_seq` is a failed attempt's leftover;
   otherwise for every descriptor compute `expected = MapEntry::data(pseg, block_off(idx, i), 8, desc.birth_cp,
   desc.crc32c)` (holes: `ZERO_BLOCK_CRC32C`, zero slot) and check the data block's `crc32c` against `desc.crc32c`.
   Only the committed CP's stripes can be checked for full identity: `parse_expected` with
   `StripeId { cp_seq: self.cp_seq, cp_nonce: self.cp_nonce }` over the `cp_stripes` list (boot.rs 645-653).
6. Map -> stripe reconciliation: for every data entry of every tree (live `diff(base 0)` and each `pinned_diff(0)`),
   the stripe's descriptor at `i` must equal `(vol, lba, Data, birth, crc)`; dedup by `(lseg, off)` exactly as
   `audit_liveness` does; this gives `found[pseg]` and the set of referenced `(pseg, off)`.
7. Liveness reconciliation (the formulas the code implies):
   - data: `self.data_live[pseg] == found[pseg]` for every pseg (what `audit_liveness` checks), and when quiet the
     committed table's `entry.live_bytes == data_live[pseg] * 4096` for every used data segment (both are set from
     the same number at 5330-5343; they diverge only by frees applied since, which remove the key);
   - metadata: `self.meta_live[pseg] == bytes of the committed CP's regions in pseg (as `load_cp` counts them, 697-703)
     + bytes of every region still queued in `self.freed_pending` that lies in pseg`; `self.table_regions` must be
     the committed slot's five table roots; `dead_data_pending` must list only sealed data segments with
     `data_live == 0`; `boot_protect` segments must be in use; `discarding`/`pending_discards` segments must be in use.
   - dedup caveat: `load_cp` counts a page shared by the live tree and a pinned snapshot twice (section 7, pitfall 2);
     the attestation should count regions deduplicated by `pba` and report the engine's over-count rather than
     mirror it.
8. Catalogue: every record names a present volume or a listed snapshot (what `assemble` enforces, 1932-1955).

**When it runs and what it must not touch.** Run it on the engine thread between ticks, after `Pool::wait_quiet`
(`is_quiet()`), so `self.segments` holds no uncommitted allocations (`open_data_segment` 4613-4635 allocates on the
live table during staging and `fail_cp` returns them; `job_data_segments` is empty when `Idle`) and `open_data` equals
the committed slot's append point. It must not: call anything that mutates (`request_cp`, `free_segment`, `discard`),
hold `backstore()` across a tick, or drain `take_events` (the daemon's `Pool::tick` owns the events). The map's page
cache filling (`MapSet::page`) is the only side effect and is harmless. Mid-CP operation is possible (the committed
state is immutable until `finalize`), but then skip the open data segment's stripes at `idx >= committed open_data_idx`
and the metadata segments the stream is writing (`open_meta` and any segment allocated by the in-flight stream are
only in the stream's table copy, invisible here, so unparsable regions there are not findings).

**Chunking.** A full scan reads every used data segment; the cleaner already does synchronous whole-segment reads
inside `new_job` (`plan_clean` 4020-4146) and the daemon's `--op-timeout` is 120 s with `otapctl` waiting ten minutes,
so a first version may run whole; a resumable version keeps a `VerifyJob { next_pseg, findings }` in the engine and
advances one segment per control-plane poll (each segment's stripes are immutable until the segment is freed, so a
per-segment unit is consistent on its own; re-check `self.cp_seq` at each resume and restart the map walk if it moved).

**Smallest coherent change.** (a) `verify.rs` with a tolerant walker over `&mut B` returning a `RingReport`,
`RootReports`, `VolumeReports`, `SnapshotReports`, `StripeReports`; (b) `Engine::verify_pool` that calls it with
`&mut self.backstore`, then reconciles against the private maps and returns `PoolAttestation { cp_seq, cp_nonce, ring,
roots, volumes, snapshots, segments: Vec<SegmentReport { pseg, kind, state, live_bytes_on_disk, counted, found,
meta_bytes_walked, deferred_bytes }>, findings: Vec<Finding> }`; (c) a control `Request::PoolVerify` + `Response`;
(d) `otapctl pool verify`. Nothing in the CP path changes, so the existing gates stay valid.

### 5.2 The sender (scope C): what it needs from the engine

Per volume, with a committed base snapshot `B` (uuid known, `PIN_REPLICATION` set) and a target:
1. Take the target snapshot: `create_snapshot(vol, uuid, SnapKind::Crash, SnapOrigin::Transfer)` (so it counts in
   the 16 reserved slots, not the 255 user ones: `is_user_class` 1086-1091), wait for `Event::SnapshotCreated { snap_id,
   cp_seq }` with that volume (the daemon waits by uuid, E39); it costs a fold.
2. `snapshot_diff(target_id, Some(base_id))` -> `Vec<(lba, MapEntry)>` born in `(base_cp, target_cp]`; holes become
   `HoleRecord { vol, lba, len }` runs (coalesce consecutive LBAs; `Holes` precede chunks); data entries group by
   `(pseg = lseg, idx = stripe_of_off(off))`; chunks are whole stripe images read at `layout().stripe_pba(pseg, idx)`
   for `stripe_bytes` through `backstore().read` (the chunk is "the stripe image byte for byte", F3), ordered by
   `(pseg, idx)`; `Chunk::raw(Bytes)`; `DeltaBegin { mode: Increment { from: base uuid, to: target uuid }, chunks, bytes
   = sum of raw_len }`, `DeltaEnd.digest` = BLAKE3 over the images in index order.
3. The header's `cp_seq` = target `cp_seq` (`SnapshotInfo.cp_seq`), `base` = base `cp_seq`, `owner_epoch` =
   `config().owner_epoch`; `cp_nonce`: see open question 1 (only `cp_nonce()` of the committed CP exists).
4. `VolEvent::Create` needs `size_blocks` (`volume(vol).size_blocks` or `map().volume(vol).lba_count()` at the
   snapshot: use `pinned.root.lba_count`), `stripe_bytes` (`layout().stripe_bytes`), a volume uuid (none exists: open
   question 3), `name` (`volume_name`); `SnapTag { uuid, retention_s }` for the target.
5. `DeltaBegin.write_active` and `acked` per volume: not exposed; the committed slot has both
   (`sb.write_active`, `vols.vols[v].acked_bytes`) and `read_ring` + `rank` gives it without a new API; `nvfail_set()`
   is public.
6. On `Committed`: `set_replication_base(vol, Some(target_id))` (clears the old base's bit in the same CP), then
   `delete_snapshot(old_base_id)` if no other pin, then wait for `SnapshotPinned`/`SnapshotDeleted`.
7. Events: inside the daemon only `Pool::tick`/`wait_for` may call `take_events`; a sender state machine must be fed
   from `Pool::note` (pool.rs 331) or run its waits through `wait_for`.

The receiver's engine side (landing `Applied` stripes as a CP) is outside this map; note only that nothing in the
engine today accepts an externally built stripe image or a delta of entries (`CpInput.deltas` is private to the CP
path), so the receiver needs a new engine entry point.

---

## 6. Test patterns to copy

- **The simulator harness** (`crates/otap-sim/src/harness.rs`): `SimConfig::small(seed)` (83-111: 32 KiB stripes,
  8 per segment, 48 segments, two 64-block volumes, folds every 5 CPs, whole-segment discards), tweak fields, then
  `Sim::new(cfg)` (formats), `boot()` (386-462, retries on `VolumeSizeMismatch`), `write`/`punch`/`read` (618-660),
  `request_cp()` 661 / `request_fold()` 670 (return the CP), `snapshot(vol) -> snap_id` 683, `settle()` 864,
  `tick_until(pred, what)` 500 (budget `TICK_BUDGET` = 100,000 with diagnostics), `crash_power()` 1169,
  `crash_daemon()` 1186, `shutdown_clean()` 1202, `verify_recovery()` 1261 (image prefix, nvfail, `verify_snapshots`,
  `audit_liveness`), `check_diff(snap_id)` 787 (the model-vs-`live_diff` oracle: copy this for a sender test),
  `clear_nvfail()` 1354, `engine()`, `disk()` (346), `events`.
- **Damaging the device**: `SimBackstore::corrupt_durable(offset_bytes, &[u8])` (backstore.rs 232) after `crash_power`,
  as in `review_sim_tests.rs` 1013-1017 (corrupt the newest slot's `map_roots` region to force a rewind) and 355-403
  (a metadata-liveness leak pinned by `meta_live_bytes`). The tests' `newest_slot(&mut s)` helper
  (review_sim_tests.rs 103-118) reads the four slots with `s.disk().read(layout.sb_pba(slot), &mut buf)` into a
  `vec![0u8; SUPERBLOCK_SLOT_SIZE]`, parses each with `SuperblockSlot::read` and keeps the highest `cp_seq`: the
  minimal "read the ring outside the engine" pattern (review_step5 imports the same types, 17-20).
- **Phase-targeted crashes**: `tick_until(|e| e.phase() == phase && e.backstore().pending_kinds().contains(&kind), ..)`
  (adversarial_sim_tests.rs 1264, review_sim_tests.rs 832) with `fifo1()` schedules (`Schedule { max_per_poll: 1,
  reorder: false, queue_depth: 64 }`, step5 43-49) so every phase lasts ticks; `no_auto_fold()` (51-59) to control folds;
  `TearPolicy` constants `DROP/WHOLE/TORN` (18-32).
- **`PROPTEST_CASES`**: the walks read it themselves: `fn proptest_cases(default: u32) -> u32` (step5_sim_tests.rs
  1376-1383; `cases(default)` in review_step5 912-919) and `proptest! { #![proptest_config(ProptestConfig {
  failure_persistence: None, cases: proptest_cases(64), max_shrink_iters: 512, ..Default::default() })] ... }`
  (1385-1409); E39 C1 and docs/09 130-140: the variable overrides any explicit count, so every property test is in the
  gate. Keep the plain run within minutes (64 cases) and seed every fake from `seed`.
- **Daemon in process** (`crates/otap-daemon/tests/daemon_tests.rs` 1-110): a `Scratch` temp dir, `DaemonConfig
  { control: tcp:127.0.0.1:0, ublk: false, op_timeout: 60 s }`, `Daemon::new(config())`, `d.handle(&Request::PoolFormat
  {..})` / `PoolBoot { arena: ArenaKind::Heap, arena_bytes: 64 MiB, cp_interval_secs: 1, .. }` on a 128 MiB file; this is
  where `pool verify` and the sender/receiver over a file sink get their end-to-end tests; `otapctl` runs as a process
  against the server (`ctl_tests.rs`).
- **The loopback net** (`otap-sim/src/net.rs`): `LoopbackNet::new(endpoints)`, `end(EndpointId)` implements
  `otap_cp::Net`; `tick()`, `set_delay`, `set_partitioned(true|false)` (holds, then delivers in order), `set_queue_limit`
  (`Busy`); nothing accepted is ever dropped. Real TCP goes through `otap-control`'s loopback pattern (`serve`,
  `Server`, `Client`, daemon_tests "the server over TCP").
- **Stream unit tests**: `otap-stream/tests/stream_tests.rs` (round trips, cursor resumption, credits) and
  `adversarial_stream_tests.rs` (every refusal, the counting allocator) are the shape for frame-level tests of a sender.

---

## 7. Risks, pitfalls and traps

1. **`Superblock::freed_pending` is not the deferred-free list.** It is reserved and always `NONE`
   (superblock.rs 469-473); `SegState::FreedPending` is never written (records.rs 89-94). The real lists are the
   in-memory `Engine::freed_pending` (metadata regions), `dead_data_pending` (data segments) and `boot_protect`
   (engine.rs 1383, 1432, 1386), recomputed at every boot from the ring (E24). An attestation "against
   freed_pending[2]" has nothing on disk to read; it reconciles the in-memory lists.
2. **Boot double-counts shared metadata regions.** `load_cp` 697-703 sums every `RootRef` in `regions` without dedup;
   `walk_cp` pushes the live index and pages (410-421) *and* each snapshot's `pinned.regions()` (502-503). Right after a
   snapshot (taken at a fold, pinning the same root and index as the live map: `PinnedRoot::from_parts` 5161) the
   shared index and pages are counted twice, so after a reboot `meta_live` of those segments can never reach zero
   until the next boot recounts. The Step 4 review listed it as hazard 12 (`docs/reviews/step4-adversarial-qa.md` 52);
   the log (docs/08 623-631) records neither a fix nor a limit for it, and the code still does it. The attestation will
   report it as metadata drift unless it dedups by `pba` and treats the engine's over-count as the finding.
3. **The ring can hold the same `cp_seq` twice with different nonces** and data segments can hold leftover stripes
   of failed attempts with `cp_seq == committed + 1` (`fail_cp` keeps `open_data.next_idx`, 4275-4292; the retry
   reuses the number with a new nonce, 3972-3973). Never identify a stripe by `cp_seq` alone; the committed CP's
   stripes are the `cp_stripes` list plus the nonce; older stripes are identified by the map's `(birth, crc)` agreeing
   with the descriptor. Stripes in the open data segment at `idx >= open_data_idx` of the committed slot are not
   committed. A sealed segment may also contain stripes nothing references (dead blocks or failed-attempt leftovers):
   not a finding.
4. **Only the committed CP's nonce is known** (`cp_nonce()` 3210; `SnapRecord`/`SnapRootPage` carry none). A stripe
   header is the only on-disk record of an older CP's nonce.
5. **`map_entry` hides damage** (3157-3163): `None` for unmapped, missing root, or an unreadable page. Use
   `map().volume(vol)` + explicit page reads (or `live_diff`, which returns the `MapError`) in a verifier.
6. **Borrowing**: `map()` is `&MapSet` while `backstore()` is `&mut B`; an external walker must clone the index
   (`v.index().to_vec()`) before reading pages; inside the engine use the `let Engine { map, backstore, .. } = self`
   pattern (2827). `MapSet::get/diff/pinned_get/pinned_diff` all need `&mut self` (the page cache).
7. **Table consistency mid-CP**: `open_data_segment` allocates on the live table during staging (4621-4634) and
   `MetaStream` on a copy (4858); when not `Idle`, `self.segments` shows data segments no committed CP names
   (`job_data_segments`) and misses the metadata segments the CP is placing. Verify when `is_quiet()`, or exclude
   `job_data_segments` (private) by running inside the engine.
8. **`SegEntry::live_bytes` saturates**: `u32::try_from(bytes).unwrap_or(u32::MAX)` (5341) would write a value that
   is not a 4 KiB multiple and fails `SegEntry::validate` at the next boot; unreachable while `MAX_SEG_BYTES` is
   256 MiB, but do not copy the pattern.
9. **`audit_liveness` skips damaged snapshots and damaged pages** (2838-2842; E32 keeps replaced entries of a
   damaged page counted): drift there is expected, not a leak; the report should classify it.
10. **Index region size vs volume**: `index_entries` 295-314 accepts an index longer than `map_pages_for(lba_count)`
    if the extra entries are NONE (a shrink leaves them); the attestation must bound its walk by `lba_count`.
11. **`read_root` reads a whole region into one `Vec`** (791) and caps at 4 GiB; a 4 GiB index is legal
    (`check_index_fits` 227-237 allows up to a quarter of the pool, capped at 4 GiB). Verify big regions with the
    incremental `Crc32c` hasher in stripe-sized reads.
12. **Alignment**: `StripeSummary::parse` and `<[PageIndexEntry]>::ref_from_bytes` need 8-aligned buffers; a `Vec<u8>`
    is 8- or 16-aligned on every target the project builds for, which every existing reader relies on; a `Bytes` slice
    cut at an odd offset is not (`Chunk::summary` doc, frame.rs 503-506). Page-aligned views (`parse` of pages) need
    4096 alignment; use the copying `read` paths.
13. **The sender's "pinned" data is not at a fixed address.** A snapshot pins blocks by birth, but the cleaner may
    relocate them (`rewrite_pinned`, `Event::SegmentCleaned`) and free the old segment after the deferral; a diff's
    `(lseg, off)` can go stale during a long send. Re-resolve through `snapshot_diff`/`pinned_get` after any
    `SegmentCleaned` for a segment the delta reads, or read chunks promptly after the diff. `live_diff` entries are not
    pinned at all (the next CP may overwrite the block's address and a dead segment is freed after `keep` CPs).
14. **A stripe image carries foreign blocks**: other volumes', dead and pre-base blocks share a stripe with the delta's
    blocks (the design's per-group packing is not implemented; docs/05 line 70/162). A receiver that lands images
    verbatim must map only the descriptors the delta names. Repacking into fresh images needs `write_summary` with a
    `StripeHdr` the receiver can identify (`Applied` kind, the delta's cp_seq/nonce).
15. **Snapshot ids are reused after a rewind** (SnapshotInfo doc 895-896; docs/06 544-546) and `cp_seq` numbers are
    re-issued (E14): key a replication base by uuid and verify `SnapshotInfo.cp_seq` against the stored base before
    using a cached id. A rewind below the base loses it (`snapshot_diff` then answers `NoSuchSnapshot`, review_sim_tests
    1026).
16. **`create_snapshot` is refused** while the volume is `creating`, in nvfail, has a pending operation, while a fold
    is blocked for space (`fold_blocked_at`, requests wait), while a map page is damaged (`MapDamaged`), when the uuid
    or name repeats, or at the class limit; and `Event::SnapshotFailed { uuid }` arrives when the fold did not fit or
    hit a damaged page (4228-4242, 4442-4460). A sender must handle both the refusal and the event.
17. **Events are single-consumer** (`mem::take`); the daemon drains them every tick; a second consumer loses them.
18. **A damaged catalogue does not tear** (E39 W4) but a damaged `cp_stripes` or `snap_table` region does (E35):
    the attestation should not promote a catalogue failure to a candidate failure, mirroring the boot.
19. `SegEntry::lap` is always 1 for an allocated segment (`free` zeroes the entry; `allocate_keeping` sets
    `old.lap + 1`), so it cannot distinguish laps (Step 4 review hazard 2, recorded as a doc note).
20. **Only one engine thread, and the control request blocks the data path** for its whole duration (E38);
    a whole-pool verification on a large pool stalls ublk I/O for that long. The simulator cannot show it (virtual time);
    the daemon tests can time it.
21. `peek_bounds` (3132-3152) uses the volume's current `size_blocks`; `snapshot_read` uses the snapshot's
    `lba_count`; a resized volume differs between the two.
22. Writing into the tree is forbidden for this survey, but note for implementers: the project's scripted-edit rule
    (docs/09 141-145) and `cargo check` after every edit.

---

## 8. Open questions for the lead

1. **Which nonce keys an increment delta?** `StreamHeader.cp_nonce` is "the nonce of cp_seq" and keys resume
   (`Cursor` by `(cp_seq, cp_nonce)`), but a snapshot's CP nonce is not recorded anywhere except in the stripe headers
   of stripes born at that CP (which relocation can rewrite). Options: record `cp_nonce` in `SnapRootPage` (reserved
   bytes, format 2.4), derive the delta nonce from the snapshot uuid, or carry the target snapshot's uuid as the
   identity and leave `cp_nonce` a per-delta random value.
2. **Chunk form on the wire**: verbatim stripe images (foreign blocks included, the receiver filters by descriptor)
   or repacked `Applied` stripes built from the delta's blocks (docs/05 162 "chunked as the live blocks of each on-disk
   stripe born after base")? This decides whether the receiver needs `write_summary` and what `DeltaBegin.bytes` means.
3. **Volume identity for `VolEvent::Create`**: volumes have an index and a catalogue name, no uuid; the stream body
   wants one. Derive from `(agg_id, vol)`, add a uuid to the catalogue record (format change), or send nil?
4. **Where the attestation runs**: whole between ticks (simple, blocks the data path), chunked per segment across
   control-plane polls, or on a second thread with its own read-only backstore (consistent only per segment,
   races with segment reuse after the deferral)?
5. **Mid-CP attestation**: require `is_quiet()` (the daemon's `wait_quiet` cannot be reached on a full pool,
   pool.rs 561-566), or allow it with the open-segment exclusions of section 5.1?
6. **The boot's double count (pitfall 2)**: fix `load_cp` to dedup regions by `pba` (changes `meta_live` after every
   boot with snapshots; the review test `the_boot_cps_segment_table_region_is_released` shows the style of test), or
   record it as a limit and have the attestation tolerate it?
7. **Report shape and exit status**: should `otapctl pool verify` exit non-zero on any finding, and should findings
   that the engine already tolerates (damaged snapshot drift, damaged page, catalogue) be warnings rather than failures?
8. **Does the attestation also verify intent slots and the other ring pair's regions** (what `evaluate` walks for
   `protected`), or only the committed CP?
9. **`DeltaBegin.write_active`/`acked` and `tags`**: expose per-volume `acked_bytes` and the slot's `write_active`
   through a new `Engine` accessor, or let the sender read the committed slot through `read_ring`?
10. **Replication snapshots and limits**: `SnapOrigin::Transfer` counts against `MAX_RESERVED_SNAPSHOTS` (16) per
    volume; with a base and a target per relationship and retries, is 16 enough, and should a transfer snapshot be
    user-deletable (it has no pin until it becomes the base)?
11. **Incremental tables and the attestation's roots walk**: once the catalogue and snapshot table become
    incremental (scope B's other half), the walk in section 5.1 step 2 changes shape; should the attestation be
    written against the new tables from the start?
