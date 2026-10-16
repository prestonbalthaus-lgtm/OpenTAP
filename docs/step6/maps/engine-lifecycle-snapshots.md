# Map: the engine's snapshot and volume lifecycle (for Step 6 scopes B and C)

Reader: engine-lifecycle-snapshots. Read-only reading of the code as it stands on 2026-10-15
(Step 5 = commit 5114866 state). Every `file:line` is the line as it is now; line numbers
inside `crates/otap-cp/src/engine.rs` are written `engine.rs:NNNN` (or bare `NNNN` inside
sections that only cite engine.rs).

## 1. Scope: what I read

- `crates/otap-cp/src/engine.rs` (6266 lines): 1-340 (constants, `VolumeSpec`, `FoldPolicy`,
  `EngineConfig`, `CpTrigger`, `OperatorWarning`), 334-537 (`Event`), 537-810 (`EngineError`,
  Display, `From` impls), 854-1365 (`PeekSource`, `VolumeStatus`, `SnapshotInfo`, `PoolStatus`,
  `Phase`, `CpJob`, `Staging`, `SnapState`, `SnapRequest`, `VolOp`, `Rewritten`, `PreparedParts`,
  `Prepared`, `State`, `VolRt`, `Stats`), 1364-1500 (the `Engine` struct), 1895-2035 (the end of
  `assemble`: snapshot loading, `next_snap_id`, the catalogue filter), 2076-2135 (`tick`,
  `request_cp`, `request_clean`), 2151-2200 (`maybe_pump`), 2224-3030 (every lifecycle entry
  point from `request_fold` to `base_cp`, whole functions), 3100-3165 (`peek`, `peek_disk`,
  `map_entry`), 3166-3260 (`take_events`, `is_quiet`, `phase`, `volume`), 3290-3325
  (`stats`, `layout`, `config`, `map`, `front_door`, `backstore`), 3353-3745 (the front-door
  serve path), 3874-3935 (`maybe_cut`), 3933-4020 (`maybe_start_cp`, `new_job`), 4189-4320
  (`start_job`, `advance_cp`, `fail_fold_requests`, `fail_pending_snapshots`, `fail_cp`),
  4350-4380 and 4475-4520 (`step_staging`'s stripe loop, `build_stripe`), 4642-4745
  (`snap_eligible`, `should_fold`, `prepare_meta`), 4741-5460 (`prepare_meta_inner`,
  whole), 5493-5720 (`step_flush`, `step_commit`, `build_slot`, `unflushed_bytes_after`),
  5711-6010 (`finalize`, whole), 6004-6266 (`release_freed`, `note_dead_data`, `nonce_for`,
  `runs_of`, `pinned_by`, `EngineRefs`).
- `crates/otap-cp/src/traits.rs` 260-470 (`RequestKind`, `Request`, `Status`, `FrontDoor`,
  `Clock`); `crates/otap-cp/src/boot.rs` 196-203 (`BootSnapshot`), 488-585 (snapshot table and
  root loading); `crates/otap-cp/src/lib.rs` 92-115 (modules 92-98, re-exports 100-115).
- `crates/otap-format/src/records.rs` 283-486 (`SnapKind`, `SnapOrigin`, `PIN_*`, `SnapRecord`),
  1118-1340 (`SnapRootPage`, `snap_table_image`, `parse_snap_table`); `src/lib.rs` whole
  (F1-F18); `src/map.rs` 40-185 (`MapEntry`), 286-302 (`PageIndexEntry`); `src/stream.rs`
  41-67 (`FrameKind`).
- `crates/otap-map/src/lib.rs` 74-124 (`MapError`), 439-475 (`Rollback`, `CpInput`), 509-544
  (`PreparedCp`), 870-900 (`MapSet::get`), 1600-1830 (`apply` tail, `diff`, `pin`,
  `pinned_get`, `pinned_diff`, `RewrittenRoot`, `PinnedRoot`).
- `crates/otap-stream/src/frame.rs` 215-320, 318-396, 390-445, 520-605, 700-760 (bodies of
  `DeltaBegin`, `SnapTag`, `VolEvent`, `VolClass`, `DeltaEnd`, `Landed`, `Committed`, `Frame`).
- `crates/otap-daemon/src/pool.rs` 331-430 (`note`, the event log), 440-600 (`tick`, `wait_for`,
  `wait_quiet`, `wait_cp_outcome`, `sync_now`), 855-935 (volume ops), 955-1015 (snapshot
  create/delete), 1118-1160 (`snapshot_pin`, `snapshot_replication_base`), 1160-1230 (`io`);
  `crates/otap-daemon/src/frontdoor.rs` 30-180 (`IoHandle`, `ChannelFrontDoor`).
- `crates/otap-control/src/protocol.rs` 99-215 (requests), 250-290 (`SnapshotReport`).
- `crates/otap-sim/src/harness.rs` 28-115, 170-290, 495-535, 661-730, 729-864, 864-1030,
  1121-1165; `crates/otap-sim/src/frontdoor.rs` 20-60, 129-155 (the file's end); every `impl FrontDoor for` by
  grep (traits.rs:419, daemon frontdoor.rs:140/277, sim frontdoor.rs:129,
  otap-sim/tests/adversarial_sim_tests.rs:171, otap-ublk/src/frontdoor.rs:437).
- Tests: `crates/otap-sim/tests/step5_sim_tests.rs` 1-165, 205-295, 347-433, 1370-1400;
  `review_step5_sim_tests.rs` 234-320, 690-735, 908-920; `step4_sim_tests.rs` 265-340, 1385-1392.
- Docs: `docs/07-engineering-decisions.md` E06 (106-163), E22 (618-669), E25 (740-769), E32
  (980-1002), E34 through S5-2 (1021-1066; S5-2 is 1056-1066), E36 (1152-1234), E39 (1428-1535, to the end of the file);
  the Step 6 brief in `docs/10-phase-c-step-briefs.md` 578-633. I did not read `docs/step6/` or
  `OpenTAP-local`.
- Added in the repair rounds: `crates/otap-arena/src/lib.rs` 100-120, 655-690, 1318-1323
  (slot geometry); `crates/otap-stream/src/frame.rs` 38-56, 578-615 and `cursor.rs` 305-421
  (`FrameMeta`, `Committed`, `Cursor::begin`/`commit`); `crates/otap-map/src/lib.rs` 955-1000,
  1230-1285 (resizes); `crates/otap-format/src/superblock.rs` 470-500 (no `next_snap_id`);
  `crates/otap-index/src/lib.rs` 636-639; `crates/otap-daemon/src/pool.rs` 78-96, 268-296,
  555-600, 670-720, 860-935, 1078-1114 (error mapping, device exposure, waits, volume ops);
  `crates/otap-sim/src/harness.rs` 75-115, 655-680, 990-1030, 1366-1374; engine.rs 1840-1915,
  2224-2300, 3383-3470, 3486-3580, 4186-4194, 4224-4470, 4636-4745, 4806-4836, 4896-4920,
  5118-5145, 5693-5705, 5740-5782, 5806-5830, 5915-5995 again, whole functions.
- Added in repair round 4: engine.rs 30-56, 145-180, 626-636, 740-750, 850-866, 890-922,
  1072-1110, 1270-1312, 1540-1560, 1637-1670, 2070-2135, 2140-2224, 2224-2268, 2268-2300,
  2327-2460, 2455-2560, 2560-2715, 2715-2810, 2890-2960, 3190-3245, 3262-3330, 3400-3620,
  3706-3742, 3800-3870, 3870-3990, 4196-4320, 4315-4475, 4478-4512, 4600-4745, 4836-4870,
  4895-4935, 5000-5030, 5105-5185, 5586-5645, 5711-6140, again, whole functions;
  `crates/otap-format/src/superblock.rs` 296-360, 430-545, 700-730, `constants.rs` 61-62,
  `catalog.rs` 60-110, `stripe.rs` 305-330, `stream.rs` 332-346;
  `crates/otap-format/tests/format_tests.rs` 2140-2175, 2620-2636; `crates/otap-stream/src/frame.rs`
  36-60, 210-340, 396-450, 507-513, 575-640, `cursor.rs` 1-14, 270-425, 500-530, 612-645;
  `crates/otap-cp/src/traits.rs` 133-145; `cp_nonce` sites by grep (otap-map lib.rs:1098, 1120;
  otap-format records.rs:609, 664-669, stripe.rs:132); `crates/otap-daemon/src/pool.rs` 435-525, 598-626,
  1114-1165; `crates/otap-sim/src/harness.rs` 783-866; `docs/07-engineering-decisions.md`
  every `## E` heading and 666-670, 766-770, 999-1003, 1021-1030, 1064-1068, 1231-1235,
  1517-1535 (the range ends); `docs/10-phase-c-step-briefs.md` 618-633.
- Added in repair round 5: engine.rs 222-240, 786-806, 1020-1047, 1340-1366, 1560-1580,
  1785-1812, 1858-1892, 2028-2034, 2076-2100, 2248-2268, 2336-2346, 2366-2374, 2470-2720,
  2888-3027, 3276-3284, 3704-3745, 4186-4296, 4415-4470, 4505-4515, 4585-4750, 4806-4865,
  4896-5022, 5116-5170, 5345-5400, 5590-5615, 5740-5830, every `nonce`, `VolumeBusy`,
  `check_index_fits`, `rollback_gen`, `fold_blocked_at` and `job_data_segments` site by grep;
  `crates/otap-cp/src/meta.rs` 85-140; `boot.rs` 300-325, 486-580; `traits.rs` 236-262, 350-380;
  `crates/otap-map/src/lib.rs` 74-100, 148-152, 174-265, 895-903, 1080-1130, 1686-1792;
  `crates/otap-format/src/stream.rs` 36-70, 318-380, `constants.rs` 100-115, `map.rs` 48-64,
  `records.rs` 1340-1370, every `inbound` site by grep; `crates/otap-format/tests/format_tests.rs`
  262-272, 556-580; `crates/otap-stream/src/frame.rs` 34-60, 210-450, 575-670, 900-960,
  1025-1075, `cursor.rs` 1-16, 262-430, 530-562, 612-642; `crates/otap-daemon/src/pool.rs`
  455-600, 1000-1165, 1190-1215, `daemon.rs` 108-135, 270-330;
  `crates/otap-sim/tests/review_sim_tests.rs` 270-380; `docs/07-engineering-decisions.md`
  1508-1535.
- Added in repair round 6: engine.rs 537-600, 864-893, 935-990, 1270-1310, 1346-1365,
  1414-1425, 1768-1800, 2150-2192, 2220-2300, 2327-2345, 2460-2716, 2812-2880, 2912-2960,
  3095-3165, 3238-3256, 3402-3436, 3486-3560, 3625-3745, 3840-3852, 4015-4035, 4186-4320,
  4355-4380, 4415-4470, 4636-4745, 4772-4840, 4855-4885, 4896-5024, 5408-5422, 5640-5690,
  5738-5782, 5796-5810, 5926-5942, every `ops_here`, `op_in`, `fold_damaged`, `pump_cps`,
  `clearing` and `Corrupt {` site by grep; `crates/otap-cp/src/boot.rs` 296-330, 486-600,
  690-750 and every `regions` site of `load_cp`; `crates/otap-map/src/lib.rs` 1700-1745,
  1805-1875, every `pinned_diff` and `.diff(` caller by grep, `tests/review_map_tests.rs`
  236-290; `crates/otap-sim/src/harness.rs` 783-866; `crates/otap-stream/src/cursor.rs`
  262-430, `frame.rs` 30-70, 180-230, 430-640; `crates/otap-format/src/stripe.rs` 60-140,
  290-340, 520-640, `stream.rs` 1-12, 190-222, 278-290; `crates/otap-daemon/src/pool.rs`
  200-300, 470-525, 596-630, 780-812, 866-936, 1019-1117, 1160-1251, every `fn open` by grep
  (none in the daemon but `open_arena`, `open_backstore` and `open_block`);
  `crates/otap-daemon/src/frontdoor.rs` 1-330; `docs/05-architecture-brainstorm-03-record.md`
  840-900; `docs/07-engineering-decisions.md` 1490-1500.
- Added in repair round 7: engine.rs 1270-1312, 1766-1790, 2040-2044, 2327-2410, 2455-2720,
  2820-2880, 2912-2960, 3706-3745, 3968-3975, 4415-4470, 4790-4870, 4896-5025, 5236-5320,
  5626-5645, 6080-6105, 6168-6232; `crates/otap-cp/src/boot.rs` 270-330, 486-600, 774-796;
  `traits.rs` 316-350, 415-455; `crates/otap-map/src/lib.rs` 1140-1200, 1400-1450;
  `crates/otap-format/src/records.rs` 355-386; `crates/otap-stream/src/frame.rs` 30-60, 312-332,
  394-450, 598-625, `cursor.rs` 305-318; `crates/otap-daemon/src/pool.rs` 268-300, 555-600,
  676-712, 860-936, 996-1020, 1114-1165, `frontdoor.rs` 180-200, 270-330;
  `crates/otap-ublk/src/frontdoor.rs` 20-40, 430-475; `crates/otap-sim/tests/adversarial_sim_tests.rs`
  160-195, `step5_sim_tests.rs` 1060-1090, `review_step5_sim_tests.rs` 320-362, 440-475; every
  `impl FrontDoor for` and every front-door `Request {` literal by grep;
  `docs/05-architecture-brainstorm-03-record.md` 3470-3485.

## 2. Public API and key internals

### 2.1 `otap-format` (`crates/otap-format/src/records.rs`, `map.rs`, `lib.rs`)

| Item | Where | Meaning |
|---|---|---|
| `enum SnapKind { Fsfreeze = 1, Crash = 2 }` | records.rs:283 | How the snapshot was taken; `from_u8`/`as_u8` at 294/304. |
| `enum SnapOrigin { Scheduled = 1, User = 2, Transfer = 3, Divergent = 4 }` | records.rs:312 | Why it exists. `Transfer` = "a replication transfer base"; `Divergent` = the head kept after a failover. Decoders 326/338. |
| `PIN_USER = 1<<0`, `PIN_SCHEDULE = 1<<1`, `PIN_REPLICATION = 1<<2`, `PIN_CLONE = 1<<3`, `PIN_MASK` | records.rs:344, 346, 348, 350, 352 | The `SnapRecord::flags` bits; any other bit is refused by `validate` (records.rs:448-450) and by `set_snapshot_pins` (engine.rs:2501). |
| `struct SnapRecord` (64 bytes, `#[repr(C)]`) | records.rs:359 | The on-disk snapshot table record. Fields: `snap_id: u64` (361; >= 1, 0 = empty slot; its doc, 360, says "Monotonic per aggregate", which holds only within one engine run: every boot recomputes the next id, rule 4), `snap_uuid: Uuid` (363; never nil), `map_root_pba: u64` (365; the `SnapRootPage`, >= 1), `deadlist_pba: u64` (367; **always 0 today**, "or 0 for none"), `created_wall_ns: u64` (369), `birth_cp: u32` (371; the CP it pins, >= 1), `vol: u32` (373), `kind: u8` (375), `origin: u8` (377), `flags: u8` (379; `PIN_*`), `rsvd: u8` (381), `crc32c: u32` (383; over bytes 0..60). |
| `SnapRecord::EMPTY`, `compute_crc`, `seal`, `is_empty`, `snap_kind`, `snap_origin`, `validate`, `verify` | records.rs:388, 405, 410, 416, 422, 428, 435, 471 | `validate` (435-468): empty slot all zero; else defined kind and origin, known pin bits, `vol <= MAX_VOL`, `birth_cp != 0`, non-nil uuid, `map_root_pba != 0`. |
| `struct SnapRootPage` (4 KiB, F13) | records.rs:1123 | The page `map_root_pba` names: `magic` (1125), `ver` (1127), `rsvd0` (1129), `snap_id: u64` (1131), `agg_id: Uuid` (1133), `birth_cp: u64` (1135), `root: MapRoot` (1137; the pinned root), `deadlist: RootRef` (1139; `RootRef::NONE` today), `reserved` (1141), `crc32c` (1143; over 0..4092). |
| `SnapRootPage::new(agg_id, snap_id, birth_cp, root, deadlist)` | records.rs:1149 | Builds and seals; `validate` (1183) requires `root.lba_count != 0` and `root.checkpoint_cp_seq <= birth_cp`; `verify` 1218; `parse`/`read` 1224/1231. |
| `snap_table_image(&[SnapRecord]) -> Result<Vec<u8>, FormatError>` | records.rs:1241 | F14: records in strictly ascending `snap_id`, each verified (an unverifiable record or an id out of order is the `Err`, 1243-1248), padded to whole 4 KiB pages; an empty table is `RootRef::NONE`, not an empty region. |
| `parse_snap_table(&[u8]) -> Result<Vec<SnapRecord>, FormatError>` | records.rs:1258 | Records until the first all-zero slot (a length that is not whole pages, a record that does not verify, an id out of order or a non-zero byte after the end is the `Err`, 1259-1284), ids strictly ascending, only zeros after. **This is the whole-region table scope B replaces.** |
| `struct MapEntry { addr: u64, birth_cp: u32, crc32c: u32 }` | map.rs:54 | `addr = lseg:36 \| off:16 \| plen:12` (F2); `UNMAPPED` (65) is all zero; `hole(birth_cp)` (128) is `addr 0, birth != 0`; `lseg()` 141, `off()` 147, `plen()` 153, `is_unmapped()` 159, `is_hole()` 165, `is_data()` 171 (`addr != 0`). F5: `birth_cp >= 1` for anything written or punched; 0 = never written. |
| `struct PageIndexEntry { page_pba, max_birth: u32, page_crc }` | map.rs:286 | `max_birth` (292) is what lets `diff(base)` skip whole pages; `NONE` (299). |
| F13, F14, F16, F18 | lib.rs:167, 172, 202, 221 (each ruling starts there) | Snapshot root page; table regions (`snap_table` = records in id order); the catalogue (names keyed by volume index or by `(vol, snap_uuid)`); `VOL_OFFLINE`, and "pages in any directory". |

### 2.2 `otap-map` (`crates/otap-map/src/lib.rs`)

| Item | Where | Meaning |
|---|---|---|
| `MapError::{Format, Io, NoSuchVolume, VolumeExists, OutOfRange, BadSize, Replay, NoSpace, Internal}` | lib.rs:74-124 | `OutOfRange { vol, lba }` at 97: a block beyond the volume. The engine converts with `From` (`EngineError::Map`). |
| `MapSet::get(reader, vol, lba) -> Result<MapEntry>` | lib.rs:875 | Committed entry: overlay first, then the page the index names (verified), else `UNMAPPED`; `NoSuchVolume` for a volume with no root yet. |
| `MapSet::diff(reader, vol, base_cp, f(lba, entry)) -> Result<(), MapError>` | lib.rs:1620-1626 | Walks the committed map (overlay merged with pages) and calls `f` for every entry with `birth_cp > base_cp && !is_unmapped()`, in LBA order; reads only pages with `max_birth > base_cp`. **Holes are included (they have a birth).** |
| `MapSet::pin(vol) -> Result<PinnedRoot, MapError>` | lib.rs:1677 | Refuses with `Internal` unless the overlay is empty: "pin after a fold". (The engine does not call it on the snapshot path; it builds the root from `PreparedCp::fold_roots`.) |
| `MapSet::pinned_get(reader, &PinnedRoot, lba) -> Result<MapEntry, MapError>` | lib.rs:1693-1698 | `Err(OutOfRange)` if `lba >= pinned.root.lba_count`; `UNMAPPED` for a page-less range; else the page entry through the shared cache. |
| `MapSet::pinned_diff(reader, &PinnedRoot, base_cp, f) -> Result<(), MapError>` | lib.rs:1714-1720 | The birth walk over a pinned root (same filter as `diff`: `birth_cp > base_cp && !is_unmapped()`), page by page by `max_birth`. |
| `struct PinnedRoot { vol, root: MapRoot, index: Vec<PageIndexEntry> }` | lib.rs:1759 | A snapshot's complete root; `load` (verified from the index region), `from_parts` (a fold's result), `get`, `diff`, `references_page` (1839), `regions` (1853). |
| `struct CpInput { cp_seq, cp_nonce, deltas, fold, new_volumes, max_region_pages, rollbacks: Vec<Rollback>, resizes: Vec<(u32,u64)>, deletes: Vec<u32> }` | lib.rs:452 | What a CP asks of the map; rollbacks/resizes/deletes need `fold`. `Rollback { vol, birth_cp, root, index }` at 439. |
| `PreparedCp::fold_roots: Vec<(u32, MapRoot, Vec<PageIndexEntry>)>` | lib.rs:531 | "After a fold: every volume's root and index, complete at this CP, which is what a snapshot pins." `released` (543) are the entries a rollback/shrink/deletion stops referencing. |

### 2.3 `otap-cp` engine: constants, config, status types

| Item | Where | Meaning |
|---|---|---|
| `MAX_REQUEST_BLOCKS: u32 = 2048` | engine.rs:37 | Longest read/write served (8 MiB): a read is checked against it alone (`TooLarge`, engine.rs:3474-3477); a write is also bounded by `arena.slots()` (engine.rs:3434, the Write arm). |
| `MAX_PUNCH_BLOCKS: u32 = 1 << 18` | engine.rs:41 | Longest discard/write-zeroes (1 GiB): one hole record. |
| `META_RESERVE_SEGMENTS: u32 = 2` | engine.rs:46 | Segments every allocation leaves free; any CP but the boot's and the shutdown's that would breach it fails for space (engine.rs:4853-4857: the metadata stream gets this reserve for every other trigger, plain CPs included, though the comment 4850-4852 names only "a fold or a snapshot"), in `step_staging`'s NoSpace arm (4439-4448, 2.4 `CpFailed`). |
| `MAX_USER_SNAPSHOTS: usize = 255`, `MAX_RESERVED_SNAPSHOTS: usize = 16` | engine.rs:49, 53 | Per volume: User+Scheduled, and Transfer+Divergent (S5-2). |
| `struct VolumeSpec { vol: u32, size_blocks: u64 }` | engine.rs:110 (112, 114) | A volume the control plane knows; `EngineConfig::volumes` lists them; a listed volume absent from the disk is created at boot (engine.rs:1798-1799). `size_blocks` in `1..=MAX_LBA + 1` (`check_size` 218). |
| `struct FoldPolicy` / `EngineConfig` | engine.rs:121 / 151 | `EngineConfig::new(agg_id, volumes)` 179; `validate` 195 (`NoSuchVolume` for an index >= `MAX_VOLS`, `VolumeExists` for a repeat). |
| `enum CpTrigger { Timer, ArenaHalf, BackToBack, Request, Shutdown, Boot, Clean }` | engine.rs:241 | Carried by `Event::CpCommitted`. |
| `OperatorWarning::{FlushOpaqueTrusted, MapPageDamaged, SnapshotDamaged { snap_id, vol, reason }, CatalogDamaged}` | engine.rs:263 | `SnapshotDamaged`: the root page or pinned index did not verify at boot: listed, unreadable, undiffable, still pins its data. |
| `struct SnapshotInfo` | engine.rs:894 | What `snapshots()` returns: `snap_id` (897; doc 895-896: "monotonic per aggregate between rewinds ... the id is reused"; in fact any boot after the pool's highest-id snapshot was deleted reuses that id too, rule 4), `uuid` (899), `vol` (901), `cp_seq` (903; = `birth_cp`; numbers above a rewind's selected CP are handed out again, rule 4), `kind` (905), `origin` (907), `created_wall_ns` (909), `damaged` (912), `flags` (914), `name` (916; from the catalogue), `deleting` (918; a deletion waits for its CP). |
| `struct VolumeStatus { present, serving, size_blocks, rollback_gen, nvfail, clear_pending, active_windows, offline, name, op_pending }` | engine.rs:867 | `serving` = present, creation durable, nvfail clear, online (`VolRt::serving` 1304-1306, in `impl VolRt` 1303-1307). `op_pending` is the `VolOp` display string. |
| `struct PoolStatus` | engine.rs:935 | `cp_seq`, `fold_cp`, `unflushed_blocks`, `snapshots`, `pool_full`, `serving`, `catalog_entries`, ... |
| `enum Phase { Idle, Staging, Metadata, Flushing, CommitA, CommitB }` | engine.rs:988 | `phase()` at 3216. |
| `struct Stats` | engine.rs:1311 | `snapshots`, `snapshots_deleted`, `folds`, `rollbacks`, ... (`stats()` at 3294). |

### 2.4 `otap-cp` engine: `Event` (every variant; `engine.rs:334`)

`take_events()` (engine.rs:3166) drains them. In the daemon every event goes to `Pool::note`
(`crates/otap-daemon/src/pool.rs:331-432`) and nowhere else (`tick` 447-453, `wait_for` 476-520
hand each event to `note` and then to the wait's predicate), and `note` turns **17 of the 21
variants** into one journal line each (`log` 322-328: `eprintln!("dbladed: {line}")` on stderr
plus a 64-line ring, `LOG_LINES` 55, read back by `recent_events` 317). The other four,
`CpCommitted`, `IntentWritten`, `SegmentFreed` and `CleanSkipped`, fall into `_ => return` (429)
and **produce no log line at all**; `CpCommitted` is consumed only by waits (`wait_cp_outcome`
570-577, which `sync_now` 592 uses). A sender/receiver that reports its progress through the
daemon log must log those four itself. Variants, fields, and the line `note` writes:

| Variant | Line | Fields and meaning | Daemon log line (`Pool::note`, pool.rs) |
|---|---|---|---|
| `Booted` | 336 | `cp_seq, dirty, reattached, replayed_blocks, rewound, arena_lost, damaged_slots, orphaned_blocks, nvfail: VolBitSet, torn: Vec<(u64, String)>`: the boot evaluated the ring (`rewound` = the newest candidate was torn and an older CP selected: **snapshots above it are gone**). | `"clean boot at cp {cp_seq}"` / `"dirty boot at cp {cp_seq}"` (arm 333-375, format 347) with clauses appended as they apply: ", rewound past a torn commit", ", arena re-attached ({replayed_blocks} blocks replayed)", ", the arena memory held no usable arena", ", {damaged_slots} damaged arena slots dropped", ", {orphaned_blocks} orphaned blocks dropped", ", {held} volume(s) held by nvfail", ", torn cp {cp}: {why}" per torn CP; also kept as `last_boot` (373) for `pool status` (819). |
| `CpCommitted` | 361 | `cp_seq, trigger: CpTrigger, gen: Option<GenId>, blocks, max_seq`: a CP is durable (pushed at 5971-5977, after the events of what that CP changed: `SnapshotDeleted` 5749, `SnapshotPinned` 5777, `SegmentCleaned` 5789, `VolumeRolledBack`/`VolumeResized`/`VolumeDeleted` 5823/5840/5848, `VolumeCreated` 5926, `NvfailCleared` 5936, `SnapshotCreated` 5944, `Folded` 5961, and the `SegmentFreed`s of `release_freed` 5968). **Not last on the Shutdown CP:** `finalize` then pushes `SnapshotFailed("the engine stopped")` for every waiting request (`fail_pending_snapshots` 5987) and `Stopped { clean: true }` (5988-5991) *after* it, so a waiter that stops at that `CpCommitted` has not seen them. | **none** (`_ => return`, 429); consumed by `wait_cp_outcome` (570-577) only. |
| `CpFailed` | 375 | `cp_seq, reason`: an attempt failed; retried, or waits for `request_cp` on a full pool. With `reason == "no free segment"` (4363: a data stripe found no segment; 4377: a relocation stripe; 4447: the CP's metadata did not fit, in `step_staging`'s NoSpace arm 4439-4448, which takes any CP, fold or plain: only a fold also sets `fold_blocked_at` (`if job.folds()` 4444-4446), and only a shutdown's fold passes `no_space = false` (`shutdown_fold` 4442-4443, `!shutdown_fold` 4447), so a plain CP whose metadata does not fit fails with `no_space = true` and sets `pool_full` (4301) without touching `fold_blocked_at`) it is the only event a request already waiting when the attempt fails gets from that failure while the engine runs (at 4363/4377, and at 4447 for a plain CP, the requests are still in `snap_requests`, since only a folding CP takes them, at `prepare_meta`, 4714-4722; at 4447 a fold's eligible requests are the job's and go back, 4262); a request made after a fold failed for space, while `fold_blocked_at` holds, gets none at all, and neither does one left behind by a fold that failed on a damaged map page (4.8 step 2, rule 14: only a timeout catches every case). The same `fail_cp` (4256-4313) also pushes a `SegmentFreed` per data segment the attempt allocated (4283-4286) and, on a space failure after `shutdown()`, `SnapshotFailed("the engine stopped")` for every waiting request (4305) and `Stopped { clean: false }` (4306-4309). | `"cp {cp_seq} failed: {reason}"` (376); `wait_cp_outcome` also turns it into the operation's error (573-575). |
| `Folded` | 382 | `cp_seq, pages`: the map folded (5961). | `"cp {cp_seq} folded the map ({pages} pages)"` (377). |
| `IntentWritten` | 389 | `intent_seq, write_active`. | **none** (429). |
| `NvfailCleared` | 397 | `vol, rollback_gen`. | `"nvfail cleared on volume {vol}, rollback generation {rollback_gen}"` (379). |
| `VolumeCreated` | 404 | `vol, cp_seq`: the creation is durable, the volume serves from now on. | `"volume {vol} created at cp {cp_seq}"` (381). |
| `SnapshotCreated` | 411 | `snap_id, vol, cp_seq`: durable. **No uuid in it**: match by `vol`, then look the uuid up in `snapshots()` (the daemon does exactly that, pool.rs:986-994). | `"snapshot {snap_id} of volume {vol} taken at cp {cp_seq}"` (382-386; an exhaustive pattern without `..`, see 5.1). |
| `SnapshotFailed` | 422 | `uuid, vol, reason`: an accepted request could not be taken; **the request is dropped, not retried**. Two causes only in the code: (a) the fold had to rewrite a map page that does not verify (or, trap 7.22, a deletion the fold carries walked a page of the deleted snapshot that does not verify): `fail_fold_requests` (4228-4242), whose single call site is 4455 in `step_staging`'s `Map(MapError::Format(e)) if job.folds()` arm (4453-4460), which also sets `fold_damaged` (4454; never cleared before a reboot, 2032), after which every `create_snapshot` is refused with `MapDamaged` (2370-2371); (b) the engine stopping: `fail_pending_snapshots` (4246-4254), reason "the engine stopped", from 5987 (the clean shutdown's commit, which also answers a request still parked behind `fold_blocked_at`: the Shutdown CP does not fold while 4660-4665 blocks) and from 4305 (`fail_cp` on a space failure once `shutdown()` was asked: a fold CP started before the shutdown has `run_state` Running, so 4442-4447 passes `no_space = true`; 4305 answers the requests 4262 just put back too). **While the engine runs, a fold that finds no free segment emits no snapshot event**: the NoSpace arm (4439-4448) sets `fold_blocked_at` (4445) and calls `fail_cp` (4256-4313), which pushes `CpFailed { reason: "no free segment" }` (4257-4260), splices `job.snaps` back to the front of `snap_requests` (4262), pushes a `SegmentFreed` for every data segment the attempt allocated (4275-4292, the event 4283-4286), and stops the engine only when a shutdown was asked (4300-4310); the requests wait, unanswered, until a segment frees and a CP runs after that (rule 14). Two more ways a request waits with no event at all: one made after that failure while `fold_blocked_at` holds (`create_snapshot_named` never checks it; the CP its `request_fold` starts stays plain, 4660-4665, and commits without a `CpFailed`), and one left in `snap_requests` as not yet eligible (4642-4648, 4716-4722) when a fold fails on a damaged map page (`fail_fold_requests` drains only `job.snaps`, 4229, and no fold follows while `fold_damaged`, 4655-4657), which hears only the stop's `SnapshotFailed`. The variant's doc comment (419-421) still names "found no free segment"; the code disagrees (3.17). | `"snapshot {uuid} of volume {vol} failed: {reason}"` (388). |
| `SegmentFreed` | 432 | `pseg, discarded`. | **none** (429). |
| `SegmentCleaned` | 441 | `pseg, relocated, cp_seq`. | `"segment {pseg} cleaned at cp {cp_seq}, {relocated} blocks relocated"` (419). |
| `CleanSkipped` | 450 | `pseg, reason`. | **none** (429). |
| `SnapshotDeleted` | 458 | `snap_id, vol, cp_seq, released`: the record left the table, `released` data blocks went. | `"snapshot {snap_id} of volume {vol} deleted at cp {cp_seq}, {released} blocks released"` (396). |
| `SnapshotPinned` | 469 | `snap_id, flags`: the `PIN_*` bits now in force, durably. | `"snapshot {snap_id} pins are now {flags:#04x}"` (399). |
| `VolumeRolledBack` | 478 | `vol, snap_id, cp_seq, rollback_gen`. | `"volume {vol} rolled back to snapshot {snap_id} at cp {cp_seq}, generation {rollback_gen}"` (407). |
| `VolumeResized` | 489 | `vol, size_blocks, cp_seq`. | `"volume {vol} resized to {size_blocks} blocks at cp {cp_seq}"` (413). |
| `VolumeDeleted` | 498 | `vol, cp_seq`. | `"volume {vol} deleted at cp {cp_seq}"` (414). |
| `OperatorWarning(OperatorWarning)` | 505 | A warning the pool status carries. | `"warning: {w}"` (420). |
| `ArenaDamage` | 508 | `vol, lba, slot`. | `"arena slot {slot} (volume {vol}, lba {lba}) failed its CRC"` (422). |
| `Internal` | 518 | `what: String`: an invariant did not hold where the engine could not stop. | `"internal: {what}"` (424). |
| `Stopped` | 526 | `cp_seq, clean`: serves nothing any more. | `"stopped at cp {cp_seq}, clean"` or `"stopped at cp {cp_seq}, without a clean shutdown record"` (425-428). |

### 2.5 `otap-cp` engine: `EngineError` (every variant; `engine.rs:537`, Display 698-785)

| Variant | Line | Meaning |
|---|---|---|
| `Format(FormatError)` | 539 | The format refused a structure (a nil snapshot uuid arrives as `Format(BadField { field: "snap_uuid", value: 0 })`, 2346-2351; bad pin bits as `BadField { field: "flags" }`, 2501-2506). |
| `Arena`, `Index`, `Map(MapError)`, `Meta`, `Layout`, `Segment`, `Boot`, `Io(IoError)` | 541-555 | Wrapped subsystem errors (`From` impls 798-806). `Io(IoError::BadLength)` / `Io(IoError::OutOfRange)` are what `snapshot_read` answers for a bad buffer or range (2922-2923, 2935-2942; the bound is the pinned `lba_count`), and what `peek` (3100) and `peek_disk` (3117) get from `peek_bounds` (3132: `BadLength` 3133-3135, `OutOfRange` 3142-3149 against the volume's current `size_blocks`; it also answers `NoSuchVolume` for an absent volume, 3136-3140). `snapshot_read` also answers `Io(..)` when the device fails a data block's read (`read_entry` 3724), and `snapshot_read`/`snapshot_diff` answer `Map(MapError::Format(..))` for a pinned map page that does not verify and `Map(MapError::Io { .. })` for one the device fails to read, whatever `SnapshotInfo.damaged` says (4.4 step 4, trap 7.22). |
| `NoSpace` | 557 | No free segment (also `create_named_volume` when no index is free, 2731-2736). |
| `NoSuchVolume { vol }` | 559 | Not present, or present and still `creating` (2338). |
| `VolumeExists { vol }` | 564 | Returned from two places only: `create_volume` on an index that is already present (2280-2281) and `EngineConfig::validate` on a repeated index (203). `create_named_volume` takes the lowest index with `!present` (2731-2736), so it never meets it; a receiver that creates by name sees `NameExists` or `NoSpace` instead. |
| `BadSize { vol, size_blocks }` | 569 | Zero or beyond `MAX_LBA + 1`. |
| `MetadataTooLarge { bytes, limit }` | 577 | The page index would exceed a quarter of the pool, and never more than 4 GiB (`check_index_fits` 227-237, the limit 232, the only constructor 234; called by `Engine::format` for every configured volume, 1572 in `format` 1566, by `create_volume` 2275 and by `resize_volume` 2669; the boot's creation of a configured volume the disk lacks, 1798-1799, does not call it). |
| `NvFailed { vol }` | 584 | Held by nvfail (snapshot creation refuses, 2340). |
| `Corrupt { vol, lba }` | 589 | A block failed its CRC, from two sites only: a disk block against its map entry's CRC (`read_entry` 3725-3726, reached by `snapshot_read` 2951, `peek_disk` through `read_disk_block` 3706, and `read_block`'s map fallback 3687), or an arena block against the CRC recorded when it was written (`read_block` 3680, on `ArenaError::DataCrc`; reached by `peek` 3110 and by `serve_read` 3640, which answers the client `Status::Corrupt`, 3644-3646, as it does for a map page that does not verify). The doc comment (588, "Data read from disk failed its CRC") names only the first. `snapshot_read` answers it for the **whole call** at the first bad block: the `?` on `read_entry` (2951) ends the loop, `dst` holds the blocks before it (and the bad block's bytes as read) and nothing after it, and `PeekSource` (854-863) has no corrupt case. A run read in 4.8 step 4 loses the rest of the run (7.8). |
| `NoSuchSnapshot { snap_id }` | 596 | Also for a snapshot of another volume (2535, 2633, 3018-3020) and for one whose deletion waits (2510-2511, 2633; `set_replication_base` passes it through at 2556 after queueing the other snapshots' pin changes, trap 7.19). |
| `SnapshotExists { uuid }` | 601 | The uuid is on disk, requested, or riding the CP in flight (2352-2356). **Pool-wide, not per volume.** |
| `SnapshotDamaged { snap_id }` | 607 | Root/index did not verify at boot: `snapshot_read`, `snapshot_diff`, `rollback_volume` refuse (2934, 2972, 2636). |
| `SnapshotOrder { snap_id, base }` | 612 | The diff's base is not older (`base_cp >= birth`, 2975). |
| `VolumeSizeMismatch` | 620 | Configuration vs root at boot (E32). |
| `MapDamaged` | 630 | A map page is damaged: no fold, so no snapshot (2370-2372). |
| `FlushOpaqueRefused` | 632 | The backstore's geometry is flush-opaque and its `FlushPolicy` is the strict `RequireFuaOrFlush` (doc 631, Display 745-746; traits.rs:133-145, where the default is `TrustBarrierWithNvfail`): `check_flush_policy` (1554-1559, the error at 1556) refuses it at `format` (1568) and as the first check of `boot` (1655-1657, a `BootFailure`). E19/E21. |
| `SnapshotPinned { snap_id, flags }` | 635 | Any pin bit set: deletion refused (2471-2472). |
| `SnapshotLimit { vol, limit }` | 642 | 255 user/scheduled or 16 transfer/divergent, pending and in-flight counted (2373-2393). |
| `VolumeOnline { vol }` | 649 | Rollback/shrink/delete need the volume offline (2591). |
| `VolumeDirty { vol }` | 655 | Acknowledged writes of the volume still in the arena or in the CP in flight (2596-2598). |
| `NewerSnapshots { vol, count }` | 661 | A rollback would orphan them (2650-2651). |
| `HasSnapshots { vol, count }` | 668 | A volume with snapshots is not deleted (2705). |
| `VolumeBusy { vol }` | 676 | A rollback/resize/deletion waits (2343, 2475, 2571, 2593, and `resize_volume`'s own check 2675-2677, which a grow meets because only a shrink goes through `check_vol_op`, 2678-2679), **or a snapshot of the volume waits or rides the CP in flight** (2653, 2683, 2707): eight sites in all. |
| `NameExists { name }` | 681 | Catalogue name in use in its scope (2367, 2727, 2750). |
| `NoSuchName { name }` | 686 | Doc "No volume or snapshot of that name". **The engine never returns it**: only its Display (779) and the daemon's error mapping (`engine_error`, pool.rs:86, to `NotFound`) name it. Lookups by name answer `Option` (`find_volume` 2768, `find_snapshot` 2794), so a caller turns `None` into its own error. |
| `NotServing` | 691 | Doc "The engine is not serving (booting or stopped)". **Never returned by the engine** (only Display, 780); a stopped engine answers front-door requests with `Status::Stopped` (`refuse_everything` 3383-3392) and its entry points do not check `serving`. |
| `Unsupported(&'static str)` | 693 | Doc "Something this step does not support". **Never returned by the engine** (only Display, 781, and the daemon's mapping to `ErrorCode::Unsupported`, pool.rs:91). |
| `Internal(String)` | 695 | "An invariant the engine relies on did not hold": a broken invariant, returned where the engine cannot go on, e.g. the CP sequence exhausted (`start_job` 4190-4191), a volume operation's CP carrying deltas of that volume (4808-4811), a map entry that points off the device (`read_entry` 3721-3722); a CP-path one comes back from `tick` (via `step_staging`'s `other => Err(other)`, 4461). Not to be confused with `Event::Internal` (518), which reports an invariant breach where the engine goes on. |

### 2.6 `otap-cp` engine: internal state an implementer must know

| Item | Where | Meaning |
|---|---|---|
| `struct SnapState { record: SnapRecord, pinned: Option<PinnedRoot>, damage: Option<String> }` | engine.rs:1075 | One held snapshot; `birth()` (1082) = `u64::from(record.birth_cp)`; `is_user_class()` (1086) = origin User/Scheduled **or an undecodable origin (`None`)**. |
| `Engine::snapshots: Vec<SnapState>` | engine.rs:1389 | **The in-memory list: a `Vec`, kept sorted by `snap_id`** (5953), not a map. Every mutation site: built at boot (1960-1984, stored 2017); `finalize`: deleted records removed (5745), rewritten roots replaced (5762), pin flags applied (5772-5773), new snapshots pushed (5950), sort (5953). `prepare_meta_inner` works on a clone (`remaining`, 4906) and never writes back. |
| `Engine::snap_requests: Vec<SnapRequest>` | engine.rs:1390 | Requests not yet taken. `SnapRequest { vol, uuid, kind, origin, name: Option<String>, after_gen: Option<GenId> }` (1096; fields 1097-1106). Pushed 2395; partitioned into `job.snaps` at `prepare_meta` (4716-4722); spliced back on `fail_cp` (4262); drained with `SnapshotFailed` at stop (4246-4254). |
| `Engine::snap_deletes: Vec<u64>` | engine.rs:1392 | Deletions waiting; pushed 2481-2482; cleared 5746. |
| `Engine::pin_changes: Vec<(u64, u8)>` | engine.rs:1394 | Pin changes waiting (last one per id wins, 2513-2514); cleared 5747 (deleted), 5775-5776 (landed). |
| `Engine::catalog: Vec<CatalogRec>`, `catalog_ref: RootRef` | engine.rs:1397-1398 | The committed catalogue (names), carried forward unchanged when nothing changed (5222-5223). |
| `Engine::next_snap_id: u64` | engine.rs:1412 | Boot: `max(snap_id) + 1` over the table (1908-1913, stored 2028); advanced only at `finalize` (5949). **Never persisted** (no superblock field, superblock.rs:482-483), so a boot after the highest id was deleted reuses it (rule 4). |
| `CpJob.snaps: Vec<SnapRequest>` | engine.rs:1025 | "The snapshot requests this CP took when it decided to fold: they are the CP's until it commits or fails." `fold_requested` 1027, `policy_fold` 1030, `clean` 1032. `CpJob::folds()` 1039. |
| `enum VolOp { Rollback { snap_id, gen }, Resize { size_blocks }, Delete }` | engine.rs:1111 (1113, 1115, 1117) | One pending operation per volume, in `VolRt::op` / `op_in` (1280-1301). |
| `PreparedParts { ..., snap_table: RootRef, catalog: Vec<CatalogRec>, catalog_ref, new_snapshots: Vec<SnapState>, deleted_snapshots: Vec<(u64, u32, u64)>, pins: Vec<(u64, u8)>, rewritten: Vec<Rewritten>, vol_ops: Vec<(u32, VolOp)>, fold, fold_pages }` | engine.rs:1148 | What `prepare_meta_inner` lays out; `finalize` applies it (5711). |

### 2.7 `otap-cp` engine: lifecycle entry points (all `pub` unless marked)

| Signature | Line | Meaning |
|---|---|---|
| `fn create_snapshot(&mut self, vol: u32, uuid: Uuid, kind: SnapKind, origin: SnapOrigin) -> Result<(), EngineError>` | 2302 | A caller-chosen uuid, no name. Delegates to the private `create_snapshot_named(vol, uuid, kind, origin, None)` (2327). Returns nothing; the answer is `Event::SnapshotCreated` / `SnapshotFailed`. |
| `fn create_named_snapshot(&mut self, vol, name: &str, kind, origin) -> Result<Uuid, EngineError>` | 2315 | Derives the uuid (`new_uuid`, 2443: blake3 of agg_id, clocks, counter, boot_count; RFC 4122 v4 bits) and names it in the catalogue (F16). **There is no public way to give both a uuid and a name** (the private 2327 does). |
| `fn delete_snapshot(&mut self, snap_id: u64) -> Result<(), EngineError>` | 2464 | Marks for deletion at the next CP that prepares its metadata after the call (no fold needed; a CP already past `prepare_meta` does not carry it, and a cleaning CP defers it, 4912-4917; 4.8 step 5). Refuses `SnapshotPinned` on the *effective* pins (pending changes included, `effective_pins` 2489), `VolumeBusy` if the volume has an op waiting (2475-2478). Idempotent (2481). |
| `fn set_snapshot_pins(&mut self, snap_id: u64, flags: u8) -> Result<(), EngineError>` | 2500 | Replaces the whole flag byte at the next CP that prepares its metadata after the call (5121-5128; `Event::SnapshotPinned`); refuses unknown bits, an unknown id, or one with a deletion waiting (`NoSuchSnapshot`, 2510-2511). |
| `fn set_replication_base(&mut self, vol: u32, snap_id: Option<u64>) -> Result<(), EngineError>` | 2523 | **The replication base is not a per-volume field: it is the `PIN_REPLICATION` bit on a `SnapRecord`.** For every snapshot of `vol`: the named one gets `flags \| PIN_REPLICATION`, every other one `flags & !PIN_REPLICATION` (2538-2558), each via `set_snapshot_pins`, so the old base loses the bit in the same CP. `None` releases the base. The named snapshot must belong to `vol` (2535). **Not atomic on error:** the `?` at 2556 returns with the lower-id changes already queued (trap 7.19). |
| `fn snapshot_read(&mut self, snap_id: u64, lba: u64, dst: &mut [u8]) -> Result<Vec<PeekSource>, EngineError>` | 2916 | `dst.len()/4096` blocks from `lba` through the pinned root (`pinned_get` 2949 per block, then `read_entry` 2951: disk read + CRC check against `entry.crc32c`, holes/unmapped read as zeros). Bounds: `lba + blocks <= pinned.root.lba_count` (2937-2942). Refuses `SnapshotDamaged` (2934). The first block that fails its CRC ends the whole call with `Err(Corrupt { vol, lba })` (the `?` at 2951; 2.5). A pinned map page that does not verify answers `Map(MapError::Format(..))` even when `damaged` is false (the `?` at 2949; 7.22). |
| `fn snapshot_diff(&mut self, snap_id: u64, base: Option<u64>) -> Result<Vec<(u64, MapEntry)>, EngineError>` | 2958 | Every entry of the snapshot with `birth_cp > base_cp` where `base_cp` = the base snapshot's `birth_cp` (or 0 for `None`): **data and holes, in LBA order, no data bytes** (`pinned_diff` 2984-2989). Base must be a snapshot of the same volume (`base_cp` 3009-3024) and strictly older (`SnapshotOrder` 2975-2979). One call returns the whole delta (memory and engine-thread stall, 7.8); a pinned map page that does not verify answers `Map(MapError::Format(..))` (the `?` at 2989; 7.22). |
| `fn live_diff(&mut self, vol: u32, base: Option<u64>) -> Result<Vec<(u64, MapEntry)>, EngineError>` | 2995 | The same over the committed live map (`MapSet::diff` 3003), overlay included. Entries acknowledged but not yet in a CP are **not** in it. |
| `fn snapshots(&self) -> Vec<SnapshotInfo>` | 2895 | On-disk snapshots in id order (`cp_seq` = `birth_cp` 2902; `deleting` 2909). Pending requests are not listed. |
| `fn snapshot_name(&self, snap_id) -> Option<&str>`, `fn find_snapshot(&self, vol, name) -> Option<u64>` | 2777, 2794 | Through the catalogue, keyed by `(vol, snap_uuid)`. |
| `fn create_volume(&mut self, vol: u32, size_blocks: u64) -> Result<(), EngineError>` | 2273 | Present at once (`present`, `creating` 2283-2284), serving once its CP commits (`VolumeCreated`); requests answer `NoSuchVolume` until then. |
| `fn create_named_volume(&mut self, name: &str, size_blocks: u64) -> Result<u32, EngineError>` | 2720 | Lowest free index (2731-2735), then `create_volume` (2737), then the name (2738). The volume is created **online** (`VolRt::default()` has `offline: false`). |
| `fn set_volume_name`, `volume_name`, `find_volume` | 2744, 2761, 2768 | Catalogue names of volumes (unique among volumes). |
| `fn set_volume_online(&mut self, vol: u32, online: bool) -> Result<(), EngineError>` | 2565 | Immediate (2577), recorded as `VOL_OFFLINE` at the next CP (2578, `build_slot` 5637-5639). Bringing online refuses `VolumeBusy` while an op waits (2571). |
| `fn rollback_volume(&mut self, vol: u32, snap_id: u64) -> Result<(), EngineError>` | 2627 | Needs: snapshot of `vol`, not deleting (2633), readable (2636), `check_vol_op` (offline, no op, quiet: 2584-2600), no newer snapshot (the count 2641-2649, the refusal 2650-2652), no snapshot request in flight (2653-2655); then `VolOp::Rollback { snap_id, gen: rollback_gen + 1 }` and `request_fold` (2657-2659). Only a fold carries it (4797), and it can wait forever (trap 7.23; so can a resize and a deletion). |
| `fn resize_volume(&mut self, vol: u32, size_blocks: u64) -> Result<(), EngineError>` | 2667 | A grow needs only present + no op (2672-2677); a shrink `check_vol_op` (2678-2679); equal size is a no-op (2680-2682); refuses while a snapshot is in flight (2683); `VolOp::Resize`, `request_fold` (2687-2689). |
| `fn delete_volume(&mut self, vol: u32) -> Result<(), EngineError>` | 2697 | `check_vol_op`, `HasSnapshots` (2699-2706), snapshot in flight (2707); `VolOp::Delete`, `request_fold` (2711-2713). |
| `fn volumes(&self) -> Vec<(u32, VolumeStatus)>`, `fn volume(&self, vol) -> Option<VolumeStatus>` | 2806, 3241 | |
| `fn request_cp(&mut self)`, `fn request_fold(&mut self)`, `fn request_clean(&mut self)` | 2122, 2224, 2129 | `request_cp` sets `cp_requested` and `pool_retry`; `request_fold` refuses silently (an `Internal` event) when `fold_damaged` (2225-2230). |
| `fn tick(&mut self) -> Result<(), EngineError>` | 2076 | One turn of the reactor: poll dones, `advance_cp`, serve the front door, `maybe_cut`, `maybe_start_cp`, `maybe_pump`. |
| `fn take_events(&mut self) -> Vec<Event>`, `fn is_quiet(&self) -> bool`, `fn phase(&self) -> Phase` | 3166, 3194, 3216 | `is_quiet`: idle, no frozen generation, no request pending, no boot retry, no paced ack. `cp_seq()` and `cp_nonce()` sit between 3194 and 3216. |
| `fn peek(&mut self, vol, lba, dst) -> Result<Vec<PeekSource>>`, `fn peek_disk(...)`, `fn map_entry(&mut self, vol, lba) -> Option<MapEntry>` | 3100, 3117, 3157 | Checker reads of the live volume (arena-first / committed-only) and the committed entry. |
| `fn audit_liveness(&mut self) -> Result<Vec<LivenessDrift>, EngineError>` | 2820 | Walks the live map and every pinned root (`pinned_diff(.., 0, ..)` 2847) and compares per-segment counts with `data_live`. |
| `fn pool_status(&self) -> PoolStatus`, `fn stats(&self) -> Stats`, `fn layout(&self) -> &DiskLayout`, `fn config(&self) -> &EngineConfig`, `fn map(&self) -> &MapSet`, `fn front_door(&mut self) -> &mut F`, `fn backstore(&mut self) -> &mut B` | 3028, 3294, 3300, 3306, 3312, 3317, 3322 | Accessors (the backstore is reachable, the private `read_entry` is not). |
| `fn shutdown(&mut self)` | 2240 | Final CP with `CleanShutdown`; pending snapshot requests are answered `SnapshotFailed("the engine stopped")` (5987, 4305). |

### 2.8 The front door (the only write path; `crates/otap-cp/src/traits.rs`)

| Item | Where | Meaning |
|---|---|---|
| `enum RequestKind { Write { vol, lba, blocks }, Read {..}, Flush { vol }, Discard { vol, lba, blocks }, WriteZeroes {..} }` | traits.rs:268 | `blocks >= 1`. Discard and WriteZeroes are served identically as hole punches. |
| `struct Request { id: ReqId, kind }`, `vol()`, `is_punch()` | traits.rs:320, 330, 343 | |
| `enum Status { Ok, NoSuchVolume, OutOfRange, NvFailed, Corrupt, Stopped, Failed, NoSpace, TooLarge, Offline }` | traits.rs:354-379 | `#[non_exhaustive]` (353): a caller's match needs a catch-all. `Offline` (F18): the volume is taken down by the operator. Where a write or punch gets each: 4.9 step 2. |
| `trait FrontDoor { fetch, copy_in(req, dst), copy_out(req, src), complete(req, status, seq) }` | traits.rs:438 (440, 444, 447, 451) | `copy_in` fills `blocks * 4096` bytes of arena memory. |
| `ChannelFrontDoor` / `IoHandle::submit(kind, data: Option<Vec<u8>>) -> Receiver<IoCompletion>` | daemon frontdoor.rs:66 / 38, 53 | The daemon's non-ublk door (used by `otapctl io write`, `io read` and `io punch`, pool.rs:1225-1250, through the pool's own clone of the handle, 258): any thread submits, and every submission gets an id of the same form (`CHANNEL_ID_BIT \| next_id`, 123-136), so the door cannot tell one submitter from another (5.1); the engine's `complete` answers on the channel (140-174). `DaemonFrontDoor` (186) multiplexes it with the ublk devices. |
| `SimFrontDoor::submit_write(vol, lba, data)` etc. | sim frontdoor.rs:45 | The harness's door; `impl FrontDoor` at 129. |

### 2.9 Stream touchpoints the lifecycle must feed (`crates/otap-stream/src/frame.rs`)

`DeltaMode::{Steady, Increment { from: Uuid, to: Uuid }}` (218; an `Increment` with a nil `from` or `to`, or with `from == to`, is refused, 930-933, and `Steady` decodes only with nil `from_snap`/`to_snap`, 1047-1051, so a delta with no base can only be `Steady`, 4.8 step 1); `SnapTag { uuid, retention_s }` (238);
`DeltaBegin { mode, chunks, bytes, cut_age_ms, write_active, tags, acked }` (297; body 268 with
`from_snap`/`to_snap`); `VolEventKind::{Create, Resize, Delete, SnapTag}` (318) with `VolEvent { event,
vol, class: Option<VolClass>, size_blocks, stripe_bytes, retention_s, uuid, name }` (420);
`Committed { applied_cp_seq, applied_epoch }` (593; both in the sender's numbering: the delta header's `cp_seq` and `owner_epoch`, 4.9 step 4); `AbortReason` (603) includes `BaseMismatch`,
`NoSpace`, `UnknownVol`. `FrameKind` 1..12 at `otap-format/src/stream.rs:41`.

## 3. Invariants and rules in force

1. **A snapshot is taken only at a fold** (F13, lib.rs:170-171 "Snapshots are taken at fold CPs,
   where the root is complete"; E25; code: `create_snapshot_named` ends with `request_fold()`
   2403, `should_fold` returns true for an eligible request 4671-4676, the root comes from
   `prepared.fold_roots` 5135-5138, and `MapSet::pin` refuses a non-empty overlay 1677-1684).
2. **`birth_cp >= 1`; CP 0 is mkfs; 0 means never written** (F5, lib.rs:103). A hole has a
   birth (E23; `MapEntry::hole` map.rs:128). So "born after base" includes punches.
3. **The snapshot table is the whole set, in ascending `snap_id`, no empty slot inside, padded to
   pages; an empty table is `RootRef::NONE`** (F14; `snap_table_image` records.rs:1241-1254;
   written whole every CP at engine.rs:5168-5179). Scope B changes this ruling; the boot
   (`boot.rs:493-517`) and `parse_snap_table` are its readers.
4. **`snap_id` is monotonic per aggregate only within one engine run; ids are reused across a
   boot, and CP numbers after a rewind.** `next_snap_id` is never persisted: its only sites are
   the field (1412), the boot's `max(surviving snap_id) + 1` (1908-1913, stored at 2028), the
   assignment `next_snap_id + i` (5140) and the advance (5949), and the superblock has no such
   field (only `snap_table`, superblock.rs:482-483). So any boot, clean or not, after the pool's
   highest-id snapshot was deleted (an aborted transfer's target, any volume's newest) hands that
   id to the next snapshot, with no rewind at all; a rewind that lost snapshots does the same.
   The doc comments say less (records.rs:360 "Monotonic per aggregate"; `SnapshotInfo` 895-896
   names only the rewind; rule 17). A boot that rewinds below a snapshot's CP loses the snapshot
   (E25: "which the simulator's checker allows and nothing else does"), and the engine then
   numbers CPs again from the selected one (`cp_seq: sb.cp_seq` 2042; `new_job` takes `cp_seq +
   1`, 3972), so `birth_cp` values, and with them `SnapshotInfo.cp_seq`, above it are handed out
   again to different writes and snapshots.
5. **A snapshot uuid is unique across the pool** (not per volume): 2352-2356 checks every held
   snapshot, every pending request and the CP in flight; a nil uuid is refused (2346-2351).
6. **Pins live in `SnapRecord::flags`; any pin bit refuses deletion** (S5-2, E36 "Pins";
   2470-2472). `PIN_REPLICATION` is only a bit: the engine does not enforce one base per volume
   (only `set_replication_base` clears the others, 2538-2558); `set_snapshot_pins` may set it on
   several. The daemon refuses setting it by hand (pool.rs:1118-1127).
7. **Deletion judging (E36, E39 W1):** processed in id order against a `remaining` set that
   shrinks (4901-4923: the comment 4901-4905, the clone 4906, the id sort 4918, the loop 4919,
   the shrink `remaining.remove(pos)` 4923); the deleted snapshot's entries born after the next
   older snapshot of the volume (4926-4931, `pinned_diff(.., older, ..)` 4954) are released
   unless the next newer snapshot (4932-4935) or, with none, the committed live map names the
   same entry (4962-4999); `OutOfRange` in either lookup means "not referenced" (4973, 4992); a
   page that does not verify in either lookup means "still referenced" (4969, 4988), but one of
   the deleted snapshot's own pages that does not verify fails the CP (the `?` at 4956; trap
   7.22); a damaged newer neighbour releases nothing (4939, 4945-4951); a volume deleted in the
   same CP releases (4981-4983). That last case seems to count a block twice: `delete_volume`'s
   count skips pending deletions (2702), so a fold with `job.clean` empty carries both (4797,
   4912-4917); each block that the newest deleted snapshot (no newer one left, 4932-4935) shares
   with the live map then goes into `released_blocks`, decremented unconditionally (5301-5303),
   and is also in the map's release of every live entry of the deleted volume (otap-map
   lib.rs:1163-1167), decremented again at 5296-5300 since the snapshot has already left
   `remaining` (4923; `pinned_by` is false). So `data_live` undercounts the segment: drift for
   `audit_liveness` (2866-2875), and a count that can reach 0 while the segment holds other trees'
   live blocks, which `note_dead_data` then frees (6099). From reading only: no test runs the case
   (step5_sim_tests.rs:1076-1078 waits for `SnapshotDeleted` before `delete_volume`, as 4.9 step
   1 does); a sim test of the same-CP case is the lead's to ask for. A damaged snapshot releases
   nothing when deleted (5013-5019).
8. **Snapshots taken in the same CP as a deletion see the new entries, and the liveness
   comparison for what this CP replaces or releases runs against `remaining` only**: the
   snapshots that existed before the CP minus its deletions, pending pins applied (cloned at
   4906; `pinned_by(&remaining, ..)` 5292 and 5297; comment 5289-5291: "only the snapshots that
   already exist (and remain) can pin what is replaced or released here"). `after` = `remaining`
   + `new_snapshots` (5168-5172) serves only `page_referenced`/`index_referenced` (5351-5368: map
   pages and index regions), the table (5173-5179) and the catalogue. The deletion judging's
   live-map lookup is the map as committed *before* this CP (comment 4978-4980, `map.get` 4985).
   docs/07 E36 second bullet (1179-1180) says the comparison runs against the remaining set "new
   snapshots included (`pinned_by(&remaining, ..)`)": the code does not match (3.17).
9. **Limits:** 255 User+Scheduled and 16 Transfer+Divergent per volume, counting held, waiting
   and in-flight requests (2373-2393; S5-2).
10. **A CP that relocates blocks takes no deletion and no volume operation** (E39 D1; 4797, 4912-
    4917); **a snapshot in flight blocks rollback/resize/delete** (E39 D2/D4; `snapshot_in_flight`
    2613-2618, used 2653/2683/2707); **a volume operation blocks snapshot creation and deletion**
    (`VolumeBusy` 2343, 2475).
11. **Snapshot requests ride the CP that carries the generation they were made in, or a later
    one** (`SnapRequest::after_gen` doc 1102-1105; `snap_eligible` 4642-4648). A request made when
    the active generation was empty rides any CP.
12. **Rollback, shrink and deletion need the volume offline and quiet** (F18; `check_vol_op`
    2584-2600: offline, no op, `unflushed_bytes_after(vol, None) == 0` and nothing of it in the CP
    in flight). A grow may run online (E36 "Resize"; 2672-2679). Rollback refuses newer snapshots
    (2641-2652) and moves `rollback_gen` on (5809-5828). A volume with snapshots is not deleted
    (2699-2706). All four (rollback, shrink, grow, deletion) ride only a fold (`ops_here = fold &&
    job.clean.is_empty()`, 4797), so under `fold_damaged` or `fold_blocked_at` they wait, silently
    and with no way to withdraw them, while the volume answers `VolumeBusy` (trap 7.23).
13. **An offline volume answers `Status::Offline` to every request before the kind is examined**
    (`serve` 3414-3416), and `NoSuchVolume` while `creating` (3403-3412).
14. **Only the boot and shutdown commits may take the reserve** (4850-4857); a fold that does not
    fit fails the attempt for space (`step_staging`'s NoSpace arm 4439-4448: `fold_blocked_at =
    Some(free_segments())` 4445, then `fail_cp` 4447, which puts `job.snaps` back at the front of
    `snap_requests` 4262 and restores `fold_requested` 4263), the fold policy then stays quiet
    until segments free up (`should_fold` 4658-4665; cleared when a fold commits, 5958), and the
    requests wait, **unanswered**, until then **and until a CP runs after that**. Neither comes
    by itself on an idle pool: the frees advance only at commits (`release_freed` from
    `finalize`, 5967-5968; a whole-segment discard frees when it completes, 2081-2090, but is
    issued at a commit, 6067-6071), and `fold_requested` (restored at 4263) starts no CP:
    `maybe_cut` (3874-3929) cuts on `cp_requested` (3899) or on the arena, timer and
    back-to-back triggers, which all need `active_blocks > 0` (3901-3906), and `maybe_pump`
    (2151-2190) runs only while `pool_full` or `tight` (2152-2162; not while a generation is
    frozen, 2155, which the failed attempt leaves behind) and counts only deletions and volume
    operations as pending work (2167), never `snap_requests` (it also runs for deferred frees and
    a cleaning candidate, 2163-2181, at most three CPs without a free, 2159, reset at 3848). So on
    an idle pool (a receiver after its last write) a parked request can stay parked after space
    has come back, until someone calls `request_cp` (2122) (client writes are refused `NoSpace`
    while `pool_full` holds, 3438-3444, and start timer CPs again only once a commit has cleared
    it, 5966). The failed
    attempt's `CpFailed { reason: "no free segment" }` (4257-4260) is the only event those
    requests get while the engine runs
    (`fail_cp`, 4256-4313, also pushes a `SegmentFreed` per data segment the attempt allocated,
    4283-4286, and, when `shutdown()` was asked, fails every waiting request and stops,
    4300-4310), and a request made *after* the failure gets no event at all:
    `create_snapshot_named` (2327-2405) never looks at `fold_blocked_at`, its `request_fold`
    starts a CP that `should_fold` keeps plain (4660-4665), that CP commits without a `CpFailed`
    (the purpose stated in the comment 4431-4438), and the request stays in `snap_requests`
    (requests enter `job.snaps` only at a fold, 4714-4722). `pool_full` (set 4301) is no measure
    of the wait, since *any* committed CP clears it (5966) while `fold_blocked_at` stays (4.8 step
    2). A request is failed (`SnapshotFailed`) only by
    `fail_fold_requests` (4228-4242), whose single call site is 4455 in the damaged-map-page arm
    of `step_staging` (4453-4460) and which answers only the job's own requests (4229: one left
    in `snap_requests` as not yet eligible, 4642-4648 and 4716-4722, is never folded while
    `fold_damaged`, 4655-4657, and waits silently for the stop), and by the stop
    (`fail_pending_snapshots` 4246-4254 from 4305 and 5987); `step_meta` (5451) and the other
    `fail_cp` callers (4330, 4346, 4363, 4377, 5465, 5519, 5570) requeue (4363 and 4377 pass
    `no_space`, so after `shutdown()` they stop the engine as 4447 does). **Only a timeout
    catches every silent case**, and while a request waits `snapshot_in_flight` (2613-2618)
    answers `VolumeBusy` to `rollback_volume`, `resize_volume` and `delete_volume` of its volume
    (2653, 2683, 2707). **A timeout withdraws nothing:** no public call removes a waiting request
    (`snap_requests` changes only at the push 2395, the fold's partition 4716-4722, `fail_cp`'s
    splice 4262 and the stop's drain 4246-4254), so it keeps blocking those operations, keeps
    counting against the class limit (2386-2390), answers a retry with the same uuid
    `SnapshotExists` (2353), and becomes a snapshot whenever a later fold takes it (4.8 step 2).
15. **Names are in the catalogue** (F16): a snapshot's name is a `CatalogRec` keyed by `(vol,
    uuid)`, written at creation only from `SnapRequest::name` (5201-5218), dropped with the
    snapshot (the catalogue lists `after`). A damaged catalogue drops every name (E39 W4).
16. **Stream rules S1-S10** (E06, docs/07:106-163) bind the frames the sender emits: `DeltaBegin.bytes`
    /`DeltaEnd.bytes` are raw image sums (S6), no sockets in `otap-stream` (S8), bounds (S4).
17. **Code vs docs:** E25 still says "names stay off the disk (F11)"; F16 (format 2.3) put them in
    the catalogue, and `create_named_snapshot` writes them. E36 says `delete_snapshot` "marks the
    snapshot and asks for a CP; any CP may carry it, a fold is not needed": true, except that a
    *cleaning* CP defers it (E39 D1, 4912-4917). `SnapRecord::deadlist_pba` is documented "or 0
    for none" and is always 0 (E36 "Rejected alternatives": deadlists were not built). E36's
    second bullet (docs/07:1179-1180) says the liveness comparison runs against the remaining set
    "new snapshots included (`pinned_by(&remaining, ..)`)": the code passes `remaining` without
    this CP's new snapshots (5292, 5297; comment 5289-5291) and uses `after` only for pages and
    indexes (5351-5368), rule 8. The `Event::SnapshotFailed` doc comment (engine.rs:419-421) names
    "found no free segment" as a cause: the code never emits it with a space reason (4439-4448
    requeues through `fail_cp` 4262; a space failure after `shutdown()` answers the requests
    "the engine stopped", 4305), rule 14. `SnapRecord::snap_id` is documented "Monotonic per
    aggregate" (records.rs:360) and `SnapshotInfo::snap_id` "monotonic per aggregate between
    rewinds" (895-896): every boot recomputes the next id from the surviving table (1908-1913), so
    an id is also reused after a plain boot that follows the deletion of the highest id, rule 4.

## 4. How things happen, in order

### 4.1 A snapshot request, from the call to `Event::SnapshotCreated`

1. `create_snapshot(vol, uuid, kind, origin)` (2302) -> `create_snapshot_named(.., None)` (2327):
   the volume must be present and not creating (2335-2339), not nvfail (2340-2342), without a
   pending op (2343-2344); uuid non-nil (2346-2351) and unused anywhere (2352-2356); a name, if
   any, validated and unique among the volume's snapshots, requests and in-flight requests
   (2358-2369); no damaged map page (2370-2372); the class limit (2373-2393).
2. The request is pushed with `after_gen = (active_blocks > 0).then(active_id)` (2395-2402) and
   `request_fold()` (2403) sets `fold_requested` and `cp_requested` (2224-2233, 2122-2125).
3. Next `tick` (2076): `maybe_cut` (3874) cuts the active generation on `CpTrigger::Request`
   (3899-3900: `cp_requested && serving && idle`, no write needed; an empty generation is cut),
   then `maybe_start_cp` (3941) takes `oldest_frozen()` (3945) and `start_job` (4189) begins
   staging. Only one generation is ever frozen at a time (`waiting` 3886, 3897).
4. At the end of staging, `prepare_meta` (4702): `should_fold` (4651) is true when
   `fold_requested` or any request is `snap_eligible` for this job (4671-4676; `ops_here` 4670 for
   a volume op). The eligible requests move from `snap_requests` into `job.snaps` (4716-4722);
   `job.fold_requested` is taken (4723); `policy_fold` is false when a snapshot rides (4724-4727).
5. `prepare_meta_inner` (4741): the relocation deltas, the generation's deltas (map entries
   already: `build_stripe` 4507 with `birth = job.cp_seq` 4488) and the holes
   (`MapEntry::hole(cp32)` 4768-4772) are assembled into one list (4749-4772), `CpInput { fold:
   true, .. }` (4839-4849) -> `map.prepare_cp` (4860-4863) rewrites the dirty pages and the
   indexes and returns `fold_roots` for every volume (one root+index per volume, complete at this
   CP). Then, in this order: deletions (4901-5021), cleaner rewrites of pinned trees (5023-5118),
   pending pin changes applied to `remaining` (5121-5128), **new snapshots** (5130-5165): for
   `(i, req)` in `job.snaps`, the volume's `fold_roots` entry (5135-5138; a volume without one is
   silently skipped), `snap_id = next_snap_id + i` (5140), a `SnapRootPage::new(agg_id, snap_id,
   job.cp_seq, root, RootRef::NONE)` placed as a one-page region (5141-5143), the `SnapRecord`
   with `map_root_pba = page_ref.pba`, `deadlist_pba: 0`, `created_wall_ns = clock.wall_ns()`
   (5132), `birth_cp = cp_seq as u32`, `kind`, `origin`, **`flags: 0`** (5144-5158), sealed;
   `SnapState { pinned: Some(PinnedRoot::from_parts(vol, root, index)) }` (5159-5163). Then the
   table region (`after` = remaining + new, sorted, `snap_table_image`, 5168-5179), the catalogue
   (5181-5234), the stripe list, roots, directory, segment table, liveness, and the freed set.
6. Metadata stripes are written (`step_meta` 5451), the pool flushed (`step_flush` 5493), slot A
   then slot B committed (`step_commit` 5525; `build_slot` 5589 puts `snap_table`, `catalog`,
   `map_roots`, ... into the superblock 5608-5613).
7. `finalize` (5711): `map.apply` (5735); deletions out (5744-5755, `SnapshotDeleted`);
   rewrites in (5756-5765); pins in (5766-5781, `SnapshotPinned`); volume ops (5799-5852);
   `cp_seq` (5869); creations (`VolumeCreated` 5924-5930); **new snapshots pushed, ids advanced,
   `SnapshotCreated { snap_id, vol, cp_seq }`** (5943-5952), the list sorted (5953); `Folded`
   (5954-5964); `CpCommitted` (5971-5977).
8. On a failed attempt: `fail_cp` (4256-4313) pushes `CpFailed` (4257-4260), puts `job.snaps`
   back at the front of `snap_requests` (4262), restores `fold_requested` (4263), hands the data
   segments the attempt allocated back with a `SegmentFreed` each (4275-4292, the event
   4283-4286), and on a space failure sets `pool_full` (4301) and, if `shutdown()` was asked,
   stops the engine (4302-4310: `fail_pending_snapshots("the engine stopped")` at 4305 answers
   every waiting request, the ones 4262 just put back included, then `Stopped { clean: false }`,
   4306-4309). Short of that stop, this is the whole story when the fold does not
   fit (the NoSpace arm of `step_staging`, 4439-4448, first sets `fold_blocked_at`, 4445) and on
   every other failure (stripe write 4330, relocation 4346, staging out of space 4363/4377,
   metadata write 5465, flush 5519, commit 5570): **no event for the requests beyond the
   attempt's `CpFailed` (4257-4260); they ride the next fold**, which needs both free space and
   a CP that runs after it (rule 14), and a request made while
   `fold_blocked_at` holds gets no event at all (rule 14). The **only** path that fails them
   while the engine runs is the arm for a map page that does not verify
   during a fold (4453-4460): it sets `fold_damaged` (4454) and calls `fail_fold_requests`
   (4228-4242; its one call site is 4455), which answers each request of the job with
   `SnapshotFailed { uuid, vol, reason }` and drops it (4229-4235), answers an explicit fold
   request with an `Internal` event (4236-4240), then `fail_cp` (4459). A request still in
   `snap_requests` (not yet eligible for that job, 4642-4648) is not answered, and no fold
   follows while `fold_damaged` (4655-4657): it waits silently for the stop. At shutdown every
   waiting request gets `SnapshotFailed("the engine stopped")` (4246-4254, from 4305 and 5987).

### 4.2 Deletion (`delete_snapshot` -> `SnapshotDeleted`)

1. 2464: find by id (2465-2469) else `NoSuchSnapshot`; `effective_pins` (2470, 2489-2495: the
   last pending change for the id, else the committed flags) & `PIN_MASK` -> `SnapshotPinned`
   (2471-2472); the volume's pending op -> `VolumeBusy` (2474-2479); push to `snap_deletes`
   (2481-2483); `request_cp` (2484).
2. The next non-cleaning CP (4912-4917) judges it as in rule 7 above; it frees the root page
   (4942), and nominates the pinned pages and index (5003-5012) under the uniform
   `page_referenced`/`index_referenced` rule (5351-5369, applied 5386-5395).
3. `finalize` 5744-5755: removed from `snapshots`, `snap_deletes`, `pin_changes`;
   `Event::SnapshotDeleted { released }`. A deleted snapshot's name leaves the catalogue because
   the catalogue is rebuilt from `after` (5201-5218).

### 4.3 Pins and the replication base

1. `set_snapshot_pins(id, flags)` (2500): mask check, existence, not deleting; replaces any
   pending change for the id (2513-2514); `request_cp` (2515). Effective at once for
   `delete_snapshot` (through `effective_pins`), durable at the CP (`prepare_meta_inner`
   5121-5128 writes the flags into the table; `finalize` 5766-5781 applies them and emits
   `SnapshotPinned`).
2. `set_replication_base(vol, Some(new))` (2523): validates `new` belongs to `vol` (2529-2536);
   computes the effective flags of every snapshot of `vol` (2538-2548); for each, `wanted` =
   `flags | PIN_REPLICATION` for `new`, `flags & !PIN_REPLICATION` for the rest (2549-2558);
   calls `set_snapshot_pins` only where it changes something (2556). So the move is one set of
   pending changes that one CP makes durable, and the old base's replication pin stops refusing
   `delete_snapshot` immediately after the call (`pending_pin_changes_govern_deletion`,
   review_step5_sim_tests.rs:690-729 proves it); another pin bit, or a volume operation waiting,
   still refuses (4.2 step 1, 4.8 step 5).
   **On an error the call is not atomic:** the `?` at 2556 returns from the middle of the
   id-ordered loop (2549-2558) and the changes queued before it stand (2513-2515); a target whose
   deletion is pending (2510-2511) therefore unpins the old base and pins nothing (4.8 step 5,
   trap 7.19).
3. The daemon's `snapshot_replication_base` (pool.rs:1149-1160) calls it then `sync_now`
   (pool.rs:592: wait quiet, `request_cp`, wait `CpCommitted`).

### 4.4 Reading and diffing a snapshot

1. `snapshot_read` (2916): buffer a multiple of 4 KiB (2922-2924); id -> position (2925-2929);
   `pinned` cloned or `SnapshotDamaged` (2931-2934); range check against `pinned.root.lba_count`
   (2935-2943); per block `map.pinned_get` (2946-2950) then `read_entry` (2951; 3710-3736: data ->
   `backstore.read(layout.block_pba(lseg, off))` + CRC check -> `PeekSource::Disk`, hole ->
   zeros/`Hole`, unmapped -> zeros/`Unmapped`). Returns one `PeekSource` per block, or, at the
   first block whose CRC does not match (3725-3726), `Err(Corrupt { vol, lba })` for the whole
   call (the `?` at 2951), with the blocks after it unread.
2. `snapshot_diff(snap, base)` (2958): snapshot by id (2963-2967), `birth` (2968), `pinned` or
   `SnapshotDamaged` (2969-2972), `base_cp` (2973; 3009-3024: `None -> 0`, else the base's
   `birth_cp`, same volume required), `SnapshotOrder` if `base_cp >= birth` (2974-2981),
   `pinned_diff` collects `(lba, entry)` (2982-2989).
3. `live_diff(vol, base)` (2995): `base_cp` then `MapSet::diff` (3003-3006).
4. Both snapshot walks read the pinned tree's map pages lazily, through the shared cache
   (`pinned_get` otap-map lib.rs:1709 and `pinned_diff` 1725 -> `MapSet::page` 895-903 ->
   `load_page` 246-260, which verifies each page as it reads it, `MapPage::read` 259), so a page
   that does not verify surfaces only there, as `Map(MapError::Format(..))`, and a failed page
   read as `Map(MapError::Io { .. })` (`BackstoreReader` 1353-1360), whatever `damaged` says: the
   boot checked only the root page and the index (boot.rs:554-572). Trap 7.22 has what follows,
   including the wedge a deletion of such a snapshot causes.

### 4.5 Volumes as the receiver needs them

1. **Create with a name and size:** `create_named_volume(name, size_blocks)` (2720) -> index =
   lowest free (2731-2735) -> `create_volume` (2273: `check_size`, `check_index_fits`, `present`
   + `creating`, `request_cp`) -> name set in `VolRt` (2738). The CP gives the volume a root
   (`new_volumes` 4776-4782, `created_in` 5411-5415) and `finalize` clears `creating` and emits
   `VolumeCreated` (5924-5930). Until then every request answers `NoSuchVolume` (3403-3412).
   Alternatively `EngineConfig::volumes` creates it at boot (1798-1799).
2. **Offline:** `set_volume_online(vol, false)` (2565) is immediate; the next CP records it.
   Offline, the front door answers `Status::Offline` (3414-3416).
3. **Rollback:** harness order (`quiesce` harness.rs:995-1005: offline, `request_cp`, wait
   `unflushed_blocks == 0 && Idle`; that wait is pool-wide and safe only because `quiesce` first
   refuses when anything is in flight, 996-998: outside the harness use the per-volume rule of
   4.9 step 1), then `rollback_volume` (2627), then wait for
   `VolumeRolledBack`, with a timeout: only a fold carries the op, and under `fold_damaged` or
   `fold_blocked_at` none comes while the volume stays offline (trap 7.23). The CP: `rollbacks`
   built from the pinned root (4815-4831), the map restores
   root and index and releases the newer entries (`PreparedCp::released`), `finalize` sets
   `size_blocks = m.lba_count()`, `rollback_gen = gen` (5809-5828).
4. **Resize:** `resize_volume` (2667); grow online, shrink offline+quiet; `VolumeResized`
   (5838-5845). **Delete:** `delete_volume` (2697); `VolumeDeleted` and `*v = VolRt::default()`
   (5846-5852), index reusable. Both ride a fold like the rollback, and their waits need the same
   timeout (trap 7.23).

### 4.6 The front-door write path (what "apply a chunk" means in the engine)

1. `tick` -> `serve_front_door` (3353): drains `deferred` first, then up to 64 fetched requests
   per tick (3360); a write or punch behind a deferred one is deferred too (3364-3368).
2. `serve` (3402): volume present and not creating (3403-3412) else `NoSuchVolume`; offline ->
   `Offline` (3414-3416); nvfail -> `NvFailed` (3418-3421); range (the `in_range` closure 3422-3427, applied at 3430-3433 to a write and 3455-3458 to a punch); for a write:
   `blocks <= MAX_REQUEST_BLOCKS && blocks <= arena.slots()` else `TooLarge` (3434-3437);
   `pool_full || !admits(blocks)` -> `NoSpace` (3438-3444); at/above the stall occupancy the
   request is handed back and retried later (3448-3450); `serve_write` (3488): arena reserve,
   `copy_in`, per-block CRCs, `index.insert(DirtyExtent::data)`, place, acknowledge (3607: after
   an intent in precise mode, after the pacing delay). Punches: `serve_punch` (3556): one hole
   record, `DirtyExtent::hole`, acknowledged likewise; `blocks <= MAX_PUNCH_BLOCKS` (3459-3462).
3. The acknowledged write sits in the active generation until a cut; the CP that carries the
   generation makes it a map entry with `birth_cp = that CP`: for a data block, `build_stripe`
   (4475, called from `step_staging` 4355) sets `let birth = job.cp_seq as u32` (4488) into the
   stripe descriptor (`StripeDescriptor::data(.., birth, recorded)` 4502) and into the delta's
   `MapEntry::data(pseg, off, SECTORS_PER_BLOCK, birth, recorded)` (4507); for a punch,
   `prepare_meta_inner` turns every block of `job.holes` into `MapEntry::hole(cp32)` (4768-4772,
   `cp32 = job.cp_seq as u32` 4748). 4749-4766 only assembles the already-built deltas
   (relocations first, then the generation's: `reloc_deltas.append(&mut deltas)` 4765); no
   birth is assigned there. **Granularity: one
   request = one volume, one contiguous LBA run, 1..=2048 blocks (writes) or 1..=262144 blocks
   (punches), whole 4 KiB blocks.** There is no other write API: `Engine` exposes no
   `write()`; `CpInput.deltas` is private to the CP.

### 4.7 Boot: how the list is rebuilt

`walk_cp` reads `sb.snap_table` (boot.rs:493-497, `parse_snap_table`), and for each record
`load_snapshot` (538-575: the root page by `map_root_pba`, verified and matched to `agg_id`,
`snap_id`, `birth_cp`, `vol` 556-567; the index region; `PinnedRoot::load` 571). A device read
error on any snapshot's root page or index, however old, fails the whole boot: `walk_cp` passes
`TornReason::Io` up ahead of the own/older test (510; from the root page's read, 554-555, or
`read_root`'s, 792), and `evaluate` turns it into `BootError::Io` ("the boot fails and is
retried", 292-295). Any other failure (a page that reads but does not verify or match, or a range
off the device) on a snapshot this CP took itself tears the candidate (500, 511); an older
snapshot's becomes `BootSnapshot { pinned: None, damage: Some(..) }` (512-516). `assemble` turns them into
`SnapState`s with an `OperatorWarning::SnapshotDamaged` each (1960-1984), computes
`next_snap_id` (1908-1913), and keeps only catalogue names whose object exists (1932-1955).

### 4.8 The sender: exact calls, in order (what the code allows today)

Let `eng: &mut Engine<..>` be driven by the daemon's tick loop (pool.rs `tick` 447-453 /
`wait_for` 476-520 style), all on the engine's thread. Inside `dbladed` the engine's events are
drained only by `Pool::tick` (pool.rs:447-453, called from `Daemon::tick`, daemon.rs:113-124,
from `wait_until` 551 and from `io` 1200) and by `wait_for` (486-494), and each drained event
goes only to `Pool::note` (331) and, in `wait_for`, to the running operation's predicate (489);
`Daemon::run` answers each control request synchronously on the engine thread (daemon.rs:278-316),
so while an operation waits its loop is the only drain. An in-daemon sender (or receiver)
therefore sees its `SnapshotCreated`, `SnapshotFailed` and `CpFailed`, and the tick in which
step 2 reads `cp_nonce`, only through a hook in `Pool::note`, which runs in the tick that
produced the event (448-449, 483-487) and can read `cp_nonce()` and `cp_seq()` there; calling
`take_events` (3166) itself would take those events from the journal, which `note` writes (2.4).

1. **Find or make the base: by the uuid stored at the last `Committed`, not by the pin.** The
   sender keeps, durably and outside the engine, the committed base's `uuid` and `cp_seq`
   (written at step 5) and resolves the uuid at every use: `let base =
   eng.snapshots().into_iter().find(|s| s.vol == vol && s.uuid == base_uuid)`, then requires
   `!s.deleting` (the pending deletion, 2909: `base_cp` 3009-3024 does not look at
   `snap_deletes`, so a diff against such a base is accepted and the base then vanishes under the
   delta, trap 7.3). `s.damaged` does not disqualify it: `snapshot_diff` refuses a damaged
   *target* (2969-2972) but takes from the base only its `birth_cp`, read from the CRC-checked
   record (`base_cp` 3009-3024 never looks at `pinned` or the damage), and the receiver holds its
   own copy of the base, so the increment is correct; a damaged base still blocks every rollback
   below itself (`NewerSnapshots` counts it, 2641-2649), and its deletion at step 5 releases
   nothing (5013-5019, an `Internal` event): a space leak to report, not a reason for a full
   resync (trap 7.17). **Then re-pin it:** `eng.set_replication_base(vol, Some(base.snap_id))`
   (2523) before step 2. Step 5 sets the bit and the start-up repeats it, but between deltas the
   daemon's `snapshot_replication_base` (pool.rs:1149-1155) clears it with `None`, which reaches
   every snapshot of the volume, nameless ones included (2549-2558), or moves it to a snapshot it
   names, after which `delete_snapshot(base)` is accepted (2470-2472; through the daemon, a
   `snapshot_rollback --delete-newer` to an older named snapshot deletes an unpinned base,
   pool.rs:1036-1072; 7.3). The call is idempotent: a change is queued only where the effective
   flags differ (2555-2556). Testing `SnapshotInfo.flags` instead misses pending changes (it is
   the committed byte, 2907). When a target is already in flight (a retry of the same target,
   step 3), re-pin it right after, since the call clears its bit too (2549-2558); the re-pin's
   pending change replaces that one (2513-2514). The call also undoes an operator's move of the
   bit to another snapshot: who owns the bit is the lead's call (8.2). "The newest snapshot with
   the committed `PIN_REPLICATION` bit" is **not** the base. This recipe pins the target at step
   3, before the
   receiver has it, so from the CP that lands that pin (pins land only at `finalize` 5766-5781)
   until the CP that lands step 5's `set_replication_base` (which only queues the changes,
   2549-2558 via 2513-2515), two snapshots of the volume carry the committed bit (`flags` is the
   committed byte, 2907), and **neither is deleting** until step 5 calls `delete_snapshot`
   (`deleting` is `snap_deletes.contains`, 2909, and only `delete_snapshot` pushes there,
   2481-2482). A delta that aborts or is retried in that window (a receiver `Abort`, a timeout,
   a disconnect, a sender restart) leaves the target pinned and newest, so a newest-pinned
   lookup picks a base the receiver never committed (`BaseMismatch`), and a bare `find` on the
   bit walks the list in id order (2895-2912, sorted at 5953) and returns the lowest id, which
   need not be the base either. If no base is stored (the initial transfer), the delta opens with
   the volume's `VolEvent::Create` (4.9 step 1 creates the replica from it) and is
   `snapshot_diff(target, None)` = every mapped block and hole of the target (`base_cp = 0`,
   3011), and step 5 makes the target the first base; if the stored uuid is no longer listed (a
   rewind lost it, or an operator released and deleted it, 7.3), the next delta is a full
   resync. Store `base.uuid` (the identity) and `base.cp_seq` (the delta header's `base`, 4.9
   step 4), never only `base.snap_id`: across a boot the id, and after a rewind the CP number,
   can name something else (trap 7.1).
   **A delta with no base has no frame of its own** (the initial transfer, and every full resync:
   step 5, 5.1, 7.3, 7.21). It cannot be an `Increment`, whose `from` may not be nil
   (`Frame::validate`, otap-stream frame.rs:930-933, which also refuses a nil `to` and `from ==
   to`), so it travels as `Steady`, which the decoder accepts only with nil `from_snap` and
   `to_snap` (1047-1051): the mode names no `to`, the receiver's `to` uuid must come from a
   `DeltaBegin` tag (`SnapTag` 238-245, which names no volume) or a `VolEvent::SnapTag` (326-328,
   which does), and 4.9 step 5 (b) has no `from` to test. The header's `base` must still be the
   receiver's applied CP (`Cursor::begin` 350-355, else `BaseMismatch`; the sender reads it from
   `HelloAck.cursor`, frame.rs:211-214): 0 only on a pipe whose cursor starts at 0
   (`Cursor::new`, cursor.rs:325), the last applied CP for a resync; and the target's `cp_seq`
   must be above it (`cp_seq <= base` is refused, 356-361), which after a rewind of the sender
   below the receiver's applied CP (rule 4) holds only once the sender's CP numbers pass it
   again. Nothing tells the receiver that this `Steady` delta is a whole image: no frame kind
   resets a volume (`FrameKind`, otap-format stream.rs:41-67; `VolEventKind` is `Create`,
   `Resize`, `Delete` and `SnapTag`, frame.rs:318-329), yet a full diff has no entry for an LBA
   the target never mapped (`pinned_diff` skips unmapped entries, otap-map lib.rs:1728), so it
   lands right only on a replica punched or recreated first (5.1). With today's frames the sender
   can say so only inside the delta: a `Holes` frame ahead of every chunk over the volume's range,
   or over the ranges the full diff has no entry for (`Holes` come "before any chunk that
   references the volume", stream.rs:53; `HoleRecord.len` is a `u32`, 336, which the receiver
   splits into punches of at most 262,144 blocks, 4.9 step 2; every punched block becomes a hole
   entry in the replica's map, 4768-4772), or a `VolEvent::Delete` then `Create` of the volume
   (events also come before any chunk that references it, frame.rs:417-418; the receiver's
   sequences are in 4.9 step 1). Which framing, or a new mode, is the lead's (8.11).
2. **Take the target with a known uuid.** `eng.create_snapshot(vol, target_uuid, SnapKind::Crash,
   SnapOrigin::Transfer)` (2302; `Transfer` counts against the 16 reserved slots, 2373-2379).
   Then tick until `Event::SnapshotCreated { vol: v, cp_seq, .. }` with `v == vol` **and**
   `eng.snapshots()` lists `target_uuid` (the event has no uuid; pool.rs:969-995 shows the loop),
   or `Event::SnapshotFailed { uuid == target_uuid, reason }` (abort: it comes for a map page
   that does not verify during the fold that carries the request, after which `create_snapshot`
   answers `MapDamaged` (2370-2371) until a reboot after the repair, or for the engine stopping;
   **a full pool gives no snapshot event**: the request waits, rule 14). **Bound the wait with a
   timeout: it is the only test that catches every parked request.** `Event::CpFailed { cp_seq,
   reason }` with `reason == "no free segment"` (pushed by `fail_cp` 4257-4260, from 4447, or
   from 4363/4377 when a data or relocation stripe found no segment; the daemon logs it at
   pool.rs:376 and `wait_cp_outcome` turns it into an error, pool.rs:573-575) is an early hint
   when it comes between the call and `SnapshotCreated`, not the test. A request can wait with no
   event at all: (i) made after a fold failed for space and before any fold committed
   (`fold_blocked_at` set 4445, cleared only at 5958), it is accepted (`create_snapshot_named`
   2327-2405 never looks at `fold_blocked_at`), its `request_fold` starts a CP that `should_fold`
   keeps plain (4660-4665), that CP commits with no `CpFailed` (the purpose of the comment
   4431-4438) and clears `pool_full` (5966), and the request never enters a `job.snaps` (they
   are taken only at a fold, 4714-4722); (ii) not yet eligible (4642-4648) when a fold fails on a
   damaged map page, it stays in `snap_requests` (`fail_fold_requests` drains only `job.snaps`,
   4229) and no fold follows while `fold_damaged` (4655-4657), so only the stop answers it
   (4246-4254). Either way `snapshot_in_flight` (2613-2618) stays true, so `rollback_volume`,
   `resize_volume` and `delete_volume` of that volume answer `VolumeBusy` (2653, 2683, 2707)
   meanwhile. **Do not use `eng.is_pool_full()` (3185) / `pool_status().pool_full` (3053)
   to tell "parked" from "not yet":** `pool_full` is set by the failed attempt (4301) and cleared
   by *every* committed CP (5966), while the request stays parked until `free_segments()`, as
   `should_fold` reads it inside `prepare_meta` (4714) after that CP's own data segments are
   allocated, exceeds `fold_blocked_at` (set 4445, after the failed attempt had allocated its
   own, 4621-4631, which `fail_cp` then hands back, 4275-4292, so right after the failure the
   free count is above it by those segments; tested by `should_fold` 4660-4665, cleared only by a
   fold's commit 5958), which no public accessor and no `PoolStatus` field (935-984) exposes,
   **and after that until a CP runs**: the frees advance only at commits and nothing starts a
   CP for the parked fold on an idle pool (rule 14), so the timeout handler calls
   `eng.request_cp()` (2122) once more, and waits a little, before it gives up.
   After the failed fold the frozen generation survives (`fail_cp` 4256-4313 retires nothing;
   the only `retire` is in `finalize`, 5894), so the next `request_cp` (2122-2125) from anyone
   (`delete_snapshot` 2484, `set_snapshot_pins` 2515, `set_volume_online` 2578, the daemon's
   `sync_now` pool.rs:592-596) starts a plain CP (`maybe_start_cp` 3948-3951 via `pool_retry`)
   that commits *without* the fold (`should_fold` is false at 4660-4665 even with
   `fold_requested` restored at 4263) and clears `pool_full` while the request still sits in
   `snap_requests`: a false negative. There is no public discriminator today (5.1 lists the
   hook); `pool_status().free_segments` against `reserve_segments` (949, 953) is the only hint.
   The snapshot's
   content is everything acknowledged before the cut of the generation the CP carried
   (4.1 steps 2-4), not "at the call".
   **A timeout withdraws nothing.** No public call removes a waiting request: `snap_requests`
   changes only at `create_snapshot_named`'s push (2395), the fold's partition (4716-4722),
   `fail_cp`'s splice (4262) and the stop's drain (4246-4254), and `delete_snapshot` takes an id
   (2464-2469), which a request gets only in the CP that takes it (5140). After the timeout the
   request therefore still keeps `snapshot_in_flight` true (2613-2618: `VolumeBusy` at 2653,
   2683, 2707), still counts against the class limit (2386-2390), makes a retry with the same
   uuid answer `SnapshotExists` (2353), and, when a later fold takes it, produces a
   `SnapshotCreated` for the abandoned transfer: on the sender an unpinned `Transfer` target that
   nothing deletes (it counts against the 16, 7.11), on the receiver a `to` holding the
   replica's state at that later cut, which 4.9 step 5's uuid test alone would accept as a
   `from`. Short of a fold taking it, a damaged-page fold failing it (4455) or the stop answering
   it, only an engine restart removes it (`snap_requests` is memory only: `Vec::new()` at boot,
   2018). So either retry the **same** target (on `SnapshotExists`, keep waiting for that
   uuid's `SnapshotCreated` instead of creating it again) or keep the abandoned uuid and delete
   its snapshot when it lands (step 5's sweep).
   **Capture the target's CP nonce in the tick that delivers its `SnapshotCreated`.** The delta
   header's `cp_nonce` is "The nonce of `cp_seq`" (otap-stream frame.rs:49-50), and the
   receiver's `Cursor` identifies a resumable partial by `(cp_seq, cp_nonce)`: the same `cp_seq`
   with another nonce is `Discarded` (cursor.rs:1-10, `BeginOutcome` 280-292, `begin` 363-376).
   The engine keeps no nonce per snapshot (`SnapRecord` records.rs:359-384, `SnapRootPage`
   1123-1144 and `SnapshotInfo` engine.rs:894-919 have none): `cp_nonce()` (3208-3212) returns
   only the newest committed CP's (set by `finalize` at 5870, before it pushes `SnapshotCreated`
   at 5944; on disk it is only in that CP's superblock, 5600, and in the headers of any data or
   relocation stripes that CP wrote, 4512 and 4593, neither of which a public call reads back for
   an older CP: `StripeHdr` is built only there, 4510 and 4591, and the metadata stream carries
   no nonce, otap-cp meta.rs:91-102; the CP log pages carry one too, otap-map lib.rs:1098 and
   1120, but are written only `if !input.fold` (1096; "A fold writes none", 1091-1093), and the
   target's CP is a fold, rule 1, 5133), and a nonce hashes the clocks and a counter (`new_nonce`
   6113-6122, `nonce_for` 6127-6138), so it cannot be recomputed later.
   So read `eng.cp_nonce()` right after the `tick` whose events carry the target's
   `SnapshotCreated`, checking that `eng.cp_seq()` equals the event's `cp_seq` (one `finalize`
   per tick: it runs only from `step_commit`, 5581, on completions polled at the tick's start,
   2078; drain the events after every tick, as `Pool::tick` pool.rs:447-453 does, which inside
   `dbladed` means the `Pool::note` hook of the preamble, since an operation's wait drains them
   too), and persist
   it with `target_uuid` and that `cp_seq`; or define a sender nonce of its own, persisted the
   same way. Without it a sender restart turns every resume into `Discarded`, and after a rewind
   (rule 4) the nonce is the only header field that tells a reused `cp_seq` apart. The header's
   `owner_epoch` is `eng.config().owner_epoch` (engine.rs:154-155, `config()` 3306; written into
   every commit at 5598).
3. **Pin the target now.** `eng.set_snapshot_pins(target_id, PIN_REPLICATION)` (2500) so an
   operator's `delete_snapshot(target)` is refused during the transfer (the bit governs at once
   through `effective_pins`). Nothing in the engine otherwise protects a transfer in flight.
   (The daemon's hand-pin refuses `PIN_REPLICATION`; an in-engine sender is not bound by that.)
   Persist the target (`target_uuid`, its `cp_seq` and nonce, step 2) as the transfer in flight
   before the pin, so a restart knows it: until `Committed` it is a pinned snapshot that is
   **not** the base (step 1). **First, at every (re)connect, ask whether the receiver already
   committed it.** A `Committed` lost in a disconnect, or a sender crash after the receiver's
   cursor commit and before step 5's persist, leaves the sender with a target in flight that the
   receiver holds as applied: `Cursor::commit` has moved `applied_cp_seq` to the target's `cp_seq`
   and cleared the landing (otap-stream cursor.rs:417-419), so a resend of the delta (its `base`
   the old base's CP) is refused `BaseMismatch` by `Cursor::begin` (350-355) before anything
   resumes, and `BaseMismatch` leads to a full resync (7.3). So compare `HelloAck.cursor`
   (frame.rs:211-214: the receiver's whole `Cursor`, that is `applied_cp_seq`, `applied_epoch`,
   the landing and the credits, cursor.rs:307-316) with the persisted target: `applied_cp_seq ==
   target cp_seq` and `applied_epoch == owner_epoch` mean it was committed (short of a sender
   rewind below the receiver's applied CP, 7.24), so go to step 5 as if
   `Committed` had come (the design's Connecting state also says "read cursor; choose delta",
   docs/05-architecture-brainstorm-03-record.md:854); the receiver's side is 4.9 step 5 (a), and
   trap 7.24 has the whole case. Otherwise, on an abort (a receiver `Abort`, a timeout, a
   disconnect), either retry the **same** target, which re-sends the same delta (the same `to`,
   `cp_seq` and nonce,
   so the receiver's cursor resumes, cursor.rs:363-372, and its `to`, if already taken or
   requested, is recognised, 4.9 step 5), or abandon it: `set_snapshot_pins(target_id, flags &
   !PIN_REPLICATION)` (2500), then `delete_snapshot(target_id)` (2464). The chunks the abandoned
   delta already landed stay in the replica's live map, which the next delta from the same base
   repairs only if the source volume never went back in between (trap 7.21), and a target whose
   diff or read failed on a bad map page must not be deleted at all (trap 7.22). An abandoned target
   left pinned holds one of the 16 reserved slots (`is_user_class` 1086-1091; 7.11) until the next
   step 5, which deletes it: step 5's `set_replication_base` clears its bit with every other one
   (2549-2558), effective at once for `delete_snapshot` (`effective_pins` 2470, 2489-2495), and its
   sweep then deletes it as an unpinned `Transfer` snapshot. Only one that also carries an operator
   pin (`set_replication_base` clears only `PIN_REPLICATION`, 2553), one kept for a bad map page
   (7.22, which the sweep skips), or any while a rollback, resize or deletion of the volume waits
   survives step 5: the sweep's `delete_snapshot` then answers `VolumeBusy` (2474-2479; the op can
   be an operator's online grow, 2672-2677, or one that never lands under `fold_damaged` or
   `fold_blocked_at`, 7.23), so the target keeps its slot until a later sweep after the op lands
   or a reboot drops it (`VolRt::op` is memory only, 1298), as step 5 says of the old base. The
   kept one loses its `PIN_REPLICATION` there too, so a sender that wants it protected from an
   in-engine `delete_snapshot` pins it with another bit, which `set_replication_base` leaves alone
   (2553). A restarted sender recognises the kept one only by that other bit (the sweep deletes
   only unpinned snapshots) or by its uuid, persisted with the transfer state as for the target in
   flight above: `damaged` does not mark it (7.17, 7.22), so without either the start-up sweep
   (step 5) deletes it, which wedges the CPs until a reboot (7.22: the `?` at 4954-4956,
   `other => Err(other)` at 4461) and does so again at the next start.
4. **Enumerate the delta.** `let entries = eng.snapshot_diff(target_id, base.map(|b| b.snap_id))?`
   (2958) -> `Vec<(lba, MapEntry)>` in LBA order: `entry.is_data()` -> a block to ship, read it by
   `eng.snapshot_read(target_id, run_lba, &mut buf[..run_blocks * 4096])` over runs of consecutive
   data LBAs (2916; each block CRC-checked; the first mismatch ends the call with
   `Err(Corrupt { vol, lba })`, 2951 and 3725-3726, so the run's blocks after `lba` are not read:
   read them again from `lba + 1` in a new call, 7.8); put each data block's `entry.crc32c` (the
   CRC recorded when it was written) into its chunk descriptor, never a CRC computed over the
   bytes read, since the receiver's check of each block against its descriptor (4.9 step 2) is
   the only one on the way. What the delta does with the bad block itself has no good answer
   today: no frame can carry a block as corrupt and no engine call can land one, so shipping its
   bytes lands it on the replica as a valid block (7.8: abort, punch or skip it with a report, or
   change the protocol and the engine; 8.12). `entry.is_hole()` -> a `HoleRecord` for the
   `Holes` frame; unmapped entries never appear.
   Either call can also answer `Map(MapError::Format(..))` (a pinned map page of the target that
   does not verify, though `damaged` is false) or `Map(MapError::Io { .. })` (the device failed
   that page's read): the target cannot be shipped while the page is bad; abort the delta,
   keep the target and never delete it (its deletion wedges the CPs, trap 7.22), take a new one.
   `snapshot_read` also answers `Io(..)` when the device fails a data block's read (`read_entry`
   3724); the keep rule does not cover it, since the deletion's walk reads map pages only
   (`pinned_diff` 4954, `pinned_get` 4965, `map.get` 4985), never data blocks: retry the read, or
   abandon the target as in step 3.
   The diff is one call that returns the whole delta and stalls the engine thread for its walk
   (about 6.4 GB and up to 262,144 synchronous page reads for a full diff of 1 TiB, trap 7.8);
   read the data in bounded runs, ticking between `snapshot_read` calls.
   Blocks beyond the base's size (the volume grew) appear with their births; the sender must emit
   `VolEvent::Resize` before any chunk that references them. A shrink between base and target
   shows as *nothing* (released entries are unmapped): the sender must compare
   `target.pinned.root.lba_count` with the base's and emit `Resize` for a shrink too (sizes are
   not in `SnapshotInfo`; `snapshot_read`'s bound is the only public exposure of the pinned
   `lba_count`: a one-block read answers `Io(OutOfRange)` exactly when `lba >= lba_count`,
   2935-2942, so a binary search finds it, see 8.3). A shrink followed by a grow between base and
   target defeats that comparison (trap 7.9).
5. **After `Committed`** (or a `HelloAck.cursor` that already names the target, step 3: the same
   steps, since the receiver has done its part): first persist the target as the base (its `uuid`
   and `cp_seq`, step 1), then `eng.set_replication_base(vol, Some(target_id))` (2523), which
   clears the old base's bit
   in the same pending set (and that of every other snapshot of the volume, 2549-2558), then
   `eng.delete_snapshot(old_base_id)` (2464). The cleared bit no longer blocks that deletion (the
   effective pins govern, 2470, 2489-2495), but it can still be refused (4.2 step 1):
   `VolumeBusy` while a rollback, resize or deletion of the volume waits (2474-2479; an
   operator's grow runs online, 2672-2677, so one can be pending on a source volume): retry after
   that operation's event (`VolumeResized`, `VolumeRolledBack`), which only a fold brings and
   which never comes under `fold_damaged` and comes late under `fold_blocked_at` (trap 7.23): bound
   the retry with a timeout and otherwise leave the old base, unpinned and not `deleting`, to a
   later sweep (below), which deletes it once the op has landed or a reboot dropped it; and
   `SnapshotPinned` when the
   old base carries an operator pin besides `PIN_REPLICATION` (2470-2472): `set_replication_base`
   clears only `PIN_REPLICATION` (2553), and the daemon's `snapshot_pin` sets `PIN_USER`,
   `PIN_SCHEDULE` or `PIN_CLONE` while keeping the replication bit (pool.rs:1137; it reaches a
   snapshot only through its catalogue name, `snap_index` pool.rs:616-626): leave that snapshot
   to the operator, noting that it still holds a reserved slot (7.11). Then **sweep** (for one
   replication relationship per volume, 7.12): delete
   every other unpinned `Transfer` snapshot of the volume that is neither the new base nor a
   target in flight (step 3) nor a target kept for a bad map page (7.22; after a restart known only
   by step 3's record or pin): an abandoned target, the
   late `SnapshotCreated` of a timed-out request (step 2), or an old base whose deletion a crash
   lost (a cleaning CP lands the unpin,
   since pins are not deferred, 5121-5128, but defers the deletion, 4912-4917, and
   `snap_deletes` is not durable). Run the same `set_replication_base` and sweep at sender start
   too, since a crash can fall between any two of these calls, and then re-pin a target in
   flight (step 3), whose bit that `set_replication_base` clears with every other one
   (2549-2558). Both calls are queued (`request_cp` was called by each), and a CP takes them
   only when it prepares its metadata: the pin changes from
   `self.pin_changes` (5121-5128), the deletions from a clone of `self.snap_deletes`
   (4912-4913). A CP already past `prepare_meta` when the calls are made (phase `Metadata`,
   `Flushing`, `CommitA` or `CommitB`) commits without them, and its `CpCommitted` is the first
   one after the calls (`finalize` retires only what it carried, 5746, 5775-5776); a cleaning CP
   already staging takes the pin change but defers the deletion to the next non-cleaning CP
   (4912-4917, 4.2 step 2). Until the deletion lands, `snapshots()` still shows the old base with
   `deleting: true` (2909), and with the committed `PIN_REPLICATION` bit (2907) only until a CP
   takes the pin change: when that CP is the deleting one, the bit stays to the end (the deletion
   takes the snapshot out of `remaining`, 4923, before the pins are applied to it, 5121-5128);
   when a cleaning CP lands the unpin first (pins are applied whatever `job.clean` holds,
   5121-5128, and `finalize` sets the flags, 5766-5774), it shows `flags & !PIN_REPLICATION` with
   `deleting: true` from that commit on (the state 7.11's crash case starts from): wait for
   `SnapshotDeleted { snap_id: old_base_id, .. }`, the one reliable signal (a `CpCommitted` counts
   only for a CP started after the calls, which is why the daemon's `sync_now` waits for quiet
   before it requests one, pool.rs:589-596, and the harness's `request_cp` waits for `cp_seq >
   before && is_quiet()`, harness.rs:658-665), before the next delta's step 1 (which resolves the
   stored uuid, so it does not pick the old base either way). **A `set_replication_base`
   that fails with `NoSuchSnapshot` is not a rewind, and the call is not atomic.** The error
   leaves it from three places: an unknown id (2529-2533), an id of another volume (2534-2535),
   and, through the `?` at 2556, `set_snapshot_pins`' refusal of a snapshot whose deletion is
   pending (2510-2511). Ids change hands only across a boot (a rewind, or any boot after the
   pool's highest-id snapshot was deleted: `next_snap_id` is recomputed at every boot, 1908-1913,
   rule 4), after which a sender that resolves the id by uuid at every use (trap 7.1) never calls
   with a stale id; the
   cause that can arise mid-transfer is trap 7.3 aimed at the *target*: an operator's
   `set_snapshot_pins(target, 0)` then `delete_snapshot(target)`, accepted at 2470-2472 once the
   effective pins are 0. By then the loop (2549-2558) has already queued `flags & !PIN_REPLICATION`
   for every lower-id snapshot of the volume (`current`, 2538-2548, follows `snapshots` in id
   order, sorted at 5953; the old base has the lower id) before it reaches the target and
   returns `Err`, so the failed call leaves the old base **unpinned**: pending (it lands with the
   next CP that prepares its metadata, 5121-5128, 5766-5781) and effective at once for
   `delete_snapshot` through `effective_pins`
   (2470, 2489-2495). A sender that keeps the old base as a fallback must then first re-pin it
   (`set_snapshot_pins(old_base_id, old_flags | PIN_REPLICATION)`, 2500, which replaces the
   queued change, 2513-2514) before any `delete_snapshot` takes it at the next CP, and the next
   delta needs a new target (trap 7.19). The stored base is then a snapshot whose deletion is
   pending, so step 1 asks for a full resync; a delta from the old base instead carries a `base`
   behind the receiver's applied CP (the receiver committed the target), which
   `Cursor::begin` refuses with `BaseMismatch` (cursor.rs:350-355), so that fallback needs a
   protocol decision.

### 4.9 The receiver: exact calls, in order

1. **Create or find the replica volume, through the receiver's own volume map.** Every volume
   reference in a delta is the **sender's** index: a chunk's descriptors
   (`StripeDescriptor::vol_lba_kind`, otap-format stripe.rs:305-307, which the engine's own
   stripes fill with `blk.vol`, `StripeDescriptor::data(blk.vol, ..)` 4502), a `HoleRecord.vol`
   (otap-format stream.rs:332-334) and `VolEvent.vol` (frame.rs:423-424). The replica's index
   comes from `find_volume(name)` (2768) or `create_named_volume` (the lowest free index,
   2731-2736) and equals the sender's only by chance, and the replica engine cannot hold the
   mapping: `VolRt` (1280-1301) has no source index and no volume uuid, and the catalogue holds
   only names (`CatalogRec.snap_uuid` is "nil for a volume", otap-format catalog.rs:89-90; its
   `flags` are reserved and "ignored by readers", 83-84). The source engine has no volume uuid
   either (its `VolRt` is the same), so the uuid `VolEvent::Create` carries ("the volume's",
   frame.rs:319-321, body 411-412) is one the sender mints and keeps. So the receiver keeps its
   own durable map from the sender's volume (keyed by that uuid, or by the name) to the replica
   index, translates every `vol` of steps 2, 3 and 5 through it, and answers a chunk, a hole
   record or an event other than `Create` of an unmapped volume with `Abort(UnknownVol)`
   (frame.rs:616-618): a `VolEvent::Create` (frame.rs:319-321) is what creates the replica (below).
   `find_volume(name)` alone is not that map: nothing marks a volume as a replica, so it returns
   a client volume that happens to carry the name (and `create_named_volume` then answers
   `NameExists`, 2726-2729). Keeping the indexes equal instead (`create_volume(src_vol,
   size_blocks)`, 2273, then `set_volume_name`, 2744) fails with `VolumeExists` when that index
   is taken on the replica pool (2280-2281). On a `Create` event of an unmapped volume:
   `eng.create_named_volume(name, size_blocks)` (2720), both from the event (`VolEvent`
   `size_blocks` 427-428, `name` 435-436), record the mapping, and wait for
   `Event::VolumeCreated { vol }`
   (the daemon's `volume_create` pool.rs:866-878 is the pattern, **without** its `ensure_device`
   at 876). For a volume that exists, check `volume(vol).nvfail` (`VolumeStatus`, field 878)
   first: a held replica refuses every write and the snapshot until `nvfail_clear(vol)` commits
   (trap 7.20). The volume is online at creation, and an online volume does not stay hidden in
   `dbladed`: `Pool::boot` (pool.rs:209-295) exposes every volume that is not offline once the
   boot commits (the wait for the boot commit 280, the `!v.offline` filter 281-287, `ensure_device`
   for each 288-293), `volume_create` calls `ensure_device` right after `VolumeCreated` (876), and
   so do `volume_online(true)` (902) and `snapshot_rollback` (1098, 1111; `volume_resize` only
   re-creates a device that exists, 929-931). `ensure_device` (678-709) skips only offline
   volumes (684), and nothing durable marks a volume as a replica (`VolRt` 1280-1301 has only
   `offline`; the catalogue holds names), so an online replica becomes a client-writable ublk
   device at the next daemon start (Linux with ublk). Keeping it from clients therefore needs a
   durable replica mark plus a check at those daemon sites (5.1, 8.5), and at one more that is
   not an exposure site: `otapctl io write` and `io punch` (`Pool::io_write` 1225 and `io_punch`
   1245, dispatched at 788-802) reach any present volume by name or `#index` (`vol_index`
   601-614) and submit through the pool's clone of the channel handle (258, `self.io.submit`
   1169), the very door the receiver writes through (step 2), so an engine-online replica stays
   writable by them whatever `ensure_device` decides, unless they honour the mark too or the
   receiver gets a door of its own (5.1). `set_volume_online(vol,
   false)` (2565) alone is no way out: the engine then refuses the receiver's own writes (3414-3416), so
   an offline replica cannot be written through the front door at all (trap 7.5). What the engine
   allows today is a toggle: keep the replica offline at rest (`set_volume_online` takes any
   present volume, one still `creating` included, 2565-2570, and `Pool::boot` skips a volume
   offline in its row, pool.rs:281-287), bring it online in the engine only while a delta lands
   (`eng.set_volume_online(vol, true)`, not `Pool::volume_online`, whose `ensure_device`, 902,
   would expose it), and take it offline again once the writes are acknowledged; the `to` request
   needs no online volume (`create_snapshot_named` does not look at `offline`, 2335-2344). Each
   toggle costs a CP (2578). Its windows are the lead's to weigh (8.5): `io_write`/`io_punch`
   reach the replica while it lands, and a daemon restart after a CP recorded it online exposes
   it (pool.rs:280-293). Apply a
   `Resize` event with `resize_volume` (2667). A grow runs online. A shrink needs the volume
   offline and quiet **for that volume**: `set_volume_online(vol, false)`, then a CP that carries
   its acknowledged writes (the daemon's `sync_now`, pool.rs:592-596: wait quiet, `request_cp`,
   wait for the next `CpCommitted`), then `resize_volume` (retry after another CP while it
   answers `VolumeDirty`), wait `VolumeResized` with a timeout (only a fold carries the resize,
   and under `fold_damaged` or `fold_blocked_at` none comes, while `set_volume_online(vol, true)`
   answers `VolumeBusy` and the replica stays offline: trap 7.23), then
   `set_volume_online(vol, true)`. The engine's own test is per volume (`check_vol_op` 2596:
   `unflushed_bytes_after(vol, None) == 0`, 5695-5705,
   and `!in_flight_touches(vol)`, 2603-2607); do **not** wait for `pool_status().unflushed_blocks
   == 0`, which counts the active and frozen generations of every volume (3046; otap-index
   lib.rs:636-639) and, on a pool whose clients write other volumes, may never reach 0. The
   harness's `quiesce` (harness.rs:995-1005) waits that way only because it first refuses when
   anything is in flight (996-998); the daemon's rollback does `sync_now` + `wait_quiet`
   (pool.rs:1092-1093) and lets the engine's per-volume check decide. Apply a `Delete` event
   (frame.rs:324-325) in this order, since `delete_volume` needs the volume offline, quiet and
   without a snapshot that is not being deleted (2697-2709), and `delete_snapshot` refuses a
   pinned snapshot (2470-2472) and answers `VolumeBusy` once a volume operation is queued
   (2474-2479): `set_replication_base(replica_vol, None)` (2523; unpins the last `to`),
   `delete_snapshot` for every snapshot of the volume, then wait for each one's `SnapshotDeleted`
   (pending deletions do not count against `delete_volume`, 2699-2703, and a fold that carries a
   snapshot's deletion together with its volume's seems to count shared blocks released twice,
   rule 7), `set_volume_online(replica_vol, false)` (2565), a CP that carries
   the volume's writes (`sync_now`, as for a shrink), `delete_volume(replica_vol)` (2697;
   `VolumeDirty` while writes remain, `VolumeBusy` while a `to` request of the volume waits,
   2707), wait for `VolumeDeleted` (5846-5852; the index is free again) with a timeout (a fold
   carries the deletion, trap 7.23), then drop the volume from the receiver's map.
2. **Apply writes and punches: through the front door only.** For each chunk, decompose the stripe
   image (`Chunk::summary` gives the descriptors with `(vol, lba, kind, crc)`) into runs of
   consecutive LBAs of one volume, translate the sender's `vol` to the replica's index (step 1),
   and submit `RequestKind::Write { vol, lba, blocks }` with the
   data (`IoHandle::submit`, daemon frontdoor.rs:53, in `dbladed`; `SimFrontDoor::submit_write`,
   sim frontdoor.rs:45, in tests), `blocks <= 2048` and `<= arena.slots()`; hole descriptors and
   `Holes` records become `RequestKind::Discard { vol, lba, blocks }` (`blocks <= 262144`). Wait for
   `IoCompletion.status == Status::Ok` for every one. The refusals a write or punch can get
   (`serve` 3402, `serve_write` 3488, `serve_punch` 3556; `Status` is `#[non_exhaustive]`,
   traits.rs:353, so the match needs a catch-all): `NoSuchVolume` (absent, or still creating,
   3404-3412), `Offline` (3414-3416), **`NvFailed`** (3418-3421), `OutOfRange` (`serve`'s range
   checks 3430-3433, 3455-3458; also when `index.insert` refuses with `IndexError::BadExtent` or
   `BadVol`: `serve_write` 3532-3538, the status at 3536, and `serve_punch` 3583-3589, the status
   at 3587, after giving the arena slots back), `TooLarge` (3434-3437; a punch above
   `MAX_PUNCH_BLOCKS`, 3459-3462), `NoSpace`
   (3438-3444, 3463-3466), `Failed` (the arena's lap counter is exhausted, 3502 and 3570, with an
   `Event::Internal`; or `copy_in` failed, 3511-3515, e.g. a data buffer that is not `blocks *
   4096` bytes: daemon frontdoor.rs:151-152, sim frontdoor.rs:138-139) and `Stopped` (the engine
   has stopped: `refuse_everything` 3383-3392 answers the deferred requests, the ones not yet
   fetched, and the placed writes whose acknowledgement still waited for a precise-mode intent,
   `pending_acks` 3611-3616). A stalled arena only delays (3448-3450). `NvFailed` is the one a
   receiver will meet in practice: after a dirty boot every write answers it until the clear
   commits (trap 7.20). Whole 4 KiB blocks;
   each write is CRC'd again by the engine (3517-3524), so the chunk's CRC is not what lands
   in the map (the map CRC is recomputed, equal if the bytes are). **So check every `Data` block
   of a chunk against its descriptor's `crc32c` before submitting it:** nothing on the way does
   (`Chunk::summary` verifies the summary only, otap-format stripe.rs:547-585; the frame's
   `payload_crc` covers the bytes as the sender sent them), and a block the sender read bad
   would otherwise land as a valid one (7.8). What to do with a block that fails is open (7.8,
   8.12); the engine offers no way to land it as corrupt.
3. **Take the snapshot with the sender's uuid at the end.** After the last `Ok`:
   `eng.create_snapshot(replica_vol, to_uuid, kind, SnapOrigin::Transfer)` for each volume of the
   delta, with **one uuid per volume**: engine snapshots are per volume and their uuids unique
   across the pool (2352-2356), so one uuid names one volume's snapshot only, yet
   `DeltaMode::Increment` carries a single `from`/`to` pair (frame.rs:222-227;
   `DeltaBeginBody::from_snap`/`to_snap` 287-290) and `DeltaBegin`'s `SnapTag` names no volume
   (238-245). Only `VolEvent::SnapTag` carries a volume and a uuid together (frame.rs:326-328;
   the body's `vol` 403-404 and `uuid` 411-412), so a delta over several volumes needs one
   `SnapTag` event per volume, and the sender one target per volume, each under its own uuid;
   the frames carry no per-volume `from` at all (step 5, 8.10). `after_gen` makes the
   request ride the CP that carries the generation holding those writes, or a later one
   (4642-4648): the snapshot includes every acknowledged write. One `request_fold` serves all.
   Wait for `SnapshotCreated { vol, cp_seq }` and the uuid in `snapshots()`, or `SnapshotFailed
   { uuid }` (a damaged map page or the engine stopping: `Abort`, no retry on this pool until a
   reboot after the repair, 2370-2371). **A full pool gives no snapshot event at all** (rule 14),
   and a request made while an earlier failed fold still blocks (`fold_blocked_at`), or left
   behind by a fold that failed on a damaged page, gets no `CpFailed` either (4.8 step 2 (i) and
   (ii)): bound the wait with a timeout; when it expires, call `eng.request_cp()` (2122) once
   more and wait a little, since on an idle replica nothing else starts the CP the parked fold
   needs (rule 14), then answer `Abort(NoSpace)`; or answer at once on an `Event::CpFailed {
   reason: "no free segment" }` (4257-4260, from 4447 or 4363/4377; `wait_cp_outcome`
   pool.rs:573-575 already turns it into an error) when one comes. `eng.is_pool_full()`
   (3185) / `pool_status().pool_full` is **not** the test: any committed CP clears it (5966)
   while the request waits behind `fold_blocked_at` (4660-4665), which nothing public exposes
   (4.8 step 2 has the whole chain). **The abort withdraws nothing:** the request stays in
   `snap_requests` and becomes a snapshot whenever a later fold takes it (4.8 step 2), so keep
   its uuid: a retry of the same delta finds it (step 5 (a)), and a `to` that lands after
   another delta has begun (`BeginOutcome::Replaced` or `Discarded`, cursor.rs:286-292, or
   `Started` after the receiver dropped the partial, `Cursor::abort` 423-425) holds the replica
   at some later cut, mixed with the new delta's writes, and is deleted when its
   `SnapshotCreated` comes. **A name for the replica snapshot cannot be given together
   with the uuid today** (2315 derives the uuid; 2327 is private).
   **Then pin the new `to` and drop the superseded one.** After `SnapshotCreated`:
   `eng.set_replication_base(replica_vol, Some(to_id))` (2523), which pins `to` (an operator's
   `delete_snapshot` on the replica pool then answers `SnapshotPinned`, 2470-2472) and unpins the
   previous one, and, once step 4's cursor commit is durable, `eng.delete_snapshot(from_id)`
   (2464; the refusals of 4.8 step 5 apply). Each delta adds one `Transfer` snapshot to the
   replica volume, and the class limit counts held, waiting and in-flight ones (2373-2393,
   `MAX_RESERVED_SNAPSHOTS` = 16 at 53): without the deletion the 17th delta's `create_snapshot`
   answers `SnapshotLimit { vol, limit: 16 }`, and without the pin nothing protects the replica's
   latest `to`, which is the next delta's `from` (step 5). As on the sender (4.8 step 5), a
   crash or an abort can leave other `Transfer` snapshots behind (a late `to` whose deletion a
   restart forgot, a `from` whose deletion a crash lost), so after each commit, and at receiver
   start after re-issuing `set_replication_base` for the last committed `to` (its pin may not
   have landed before a crash), delete every `Transfer` snapshot of the replica volume except
   the last committed `to` and the in-flight delta's `to` (for one relationship per volume, 7.12).
   The receiver can spare the latter only
   if it recorded it: the cursor names no snapshot (`CursorBody`, otap-stream cursor.rs:538-559,
   holds `applied_cp_seq`, `applied_epoch` and the landing's `cp_seq` and nonce; `Landing`
   269-276), so persist the in-flight `to` uuid of each volume, with the landing's `cp_seq` and
   nonce, in the receiver's own store **before** calling `create_snapshot`, and clear it at the
   cursor commit. The start-up `set_replication_base` for the last committed `to` (in step 4's
   crash window that is `from`) clears the in-flight `to`'s bit (2549-2558): re-pin it
   (`set_snapshot_pins(to_id, PIN_REPLICATION)`, 2500, whose pending change replaces that one,
   2513-2514) when it is listed and not `deleting`. Without the record the sweep deletes the very
   `to` that step 5 (a) relies on (`delete_snapshot` accepts it once it is unpinned, 2470-2472).
4. **Report the committed CP, in the sender's numbering.** `Committed` is "The receiver
   committed the delta's CP" (crates/otap-stream/src/frame.rs:591); its `applied_cp_seq` is "The
   CP the replica is now at" and its `applied_epoch` "The sender epoch of that CP" (585-588,
   594-597). Every frame header carries the delta's CPs in the sender's numbering (`FrameMeta`
   40-53: `cp_seq` "The CP the delta ends at" 45-46, `base` "The CP the delta starts from" 47-48,
   `cp_nonce` "The nonce of `cp_seq`" 49-50, which with `cp_seq` identifies the receiver's
   partial, `Landing` cursor.rs:270-273 and `begin` 363-376, and which the sender captured at 4.8
   step 2, and `owner_epoch` "The sender's owner epoch" 51-52, the sending engine's
   `config().owner_epoch`, engine.rs:154-155). So send `Committed { applied_cp_seq:
   header.cp_seq, applied_epoch: header.owner_epoch }`, which is exactly what the receiver's
   `Cursor::commit(epoch)` records (crates/otap-stream/src/cursor.rs:409-421: `applied_cp_seq =
   landing.cp_seq`, the `cp_seq` given to `Cursor::begin` 343-383; `applied_epoch = epoch`,
   documented "The sender's owner epoch of that CP" 310-311); the next `DeltaBegin`'s `base` is
   checked against it (`Cursor::begin` 350-355 -> `BaseMismatch`, "The delta's base is not the
   receiver's applied CP", frame.rs:610-611). The receiver engine's `SnapshotCreated.cp_seq`
   (== `SnapshotInfo.cp_seq` == the replica snapshot's `birth_cp`) is the replica pool's own CP
   numbering, unrelated to the sender's: it only tells the receiver that `to` is durable, which
   is the condition for sending `Committed` at all; putting it in `applied_cp_seq` makes the
   sender's next delta (base = the sender's last CP) mismatch the receiver's cursor. With several
   volumes in one delta, send `Committed` once every tagged volume's `to` snapshot has its
   `SnapshotCreated` (they share one replica CP only if all requests were made before that CP's
   `prepare_meta`).
   **Where `applied_cp_seq` and `applied_epoch` live durably, and the crash window.** The format
   reserves per-group receiver state in every superblock: `InboundState` (otap-format
   superblock.rs:317-346: `applied_cp_seq` "The sender CP this group's volumes are at" 324-325,
   `applied_owner_epoch` 326-327, `acked_cp_seq` "The last CP acknowledged back to the sender as
   `Committed`" 328-329, `INBOUND_VALID` 315; the field `sb.inbound: [InboundState; MAX_GROUPS]`
   488-489, `MAX_GROUPS = 16` constants.rs:62), documented "Reserved in v0.1; written by Phase B"
   (318). Written by the CP that takes `to`, it would commit atomically with it. Today nothing
   writes it: `build_slot` starts every CP from a zeroed slot (engine.rs:5591 ->
   `SuperblockSlot::new` superblock.rs:715-720 -> `Superblock::new` 523-532, `new_zeroed` at 524)
   and never sets `inbound`, and no crate outside otap-format touches the field (only its tests:
   the offset pin `offset_of!(Superblock, inbound) == 544`, otap-format/tests/format_tests.rs:269,
   the golden superblock vector's `inbound[0]`, format_tests.rs:2161-2173, checked at 2633-2634,
   and adversarial_tests.rs:3193, which sets `inbound[3].rsvd`; the proptest at
   format_tests.rs:560-563 round-trips the `InboundState` type, not the field). The stream
   crate's `CursorPage` is the page "a receiver persists per pipe at every receiver CP" (its doc
   comment, cursor.rs:619; the struct 620-639), but no engine CP writes such a page either.
   So the receiver's cursor commit is a write of its own after `to`'s `SnapshotCreated`, and a
   crash between the two leaves `to` durable while the cursor is still at the base: the resent
   `DeltaBegin` passes `Cursor::begin` (base == applied, cursor.rs:350-355), and taking `to` again
   answers `SnapshotExists` (2352-2356). The same happens while a timed-out `to` request is still
   parked (step 3). Step 5 (a) therefore treats an existing (not `deleting`) or pending `to` on
   the mapped volume as the delta's commit, which needs the in-flight `to` recorded before it was
   asked for (step 3); the engine-side fix is in 5.1. The other window, after the cursor commit
   and before the sender hears `Committed`, is trap 7.24 (step 5 (a), 4.8 step 3).
5. **The checks on `DeltaBegin`, `to` first.** (a) **First, whether the cursor already committed
   this delta:** a `DeltaBegin` whose header `cp_seq` is the cursor's `applied_cp_seq` and whose
   `owner_epoch` is its `applied_epoch`, with `to` held (not `deleting`) on the mapped volume, is
   a resend of a delta whose `Committed` the sender never got (a disconnect, or a sender crash
   before 4.8 step 5's persist; trap 7.24). Answer it `Committed { applied_cp_seq,
   applied_epoch }` again **without** calling `Cursor::begin`, which would refuse it with
   `BaseMismatch` (its `base` is the old base's CP, cursor.rs:350-355); there is no landing to
   re-land into or commit either (`Cursor::commit` cleared it, 417-419). A new delta never
   matches this test, since its `cp_seq` must be above the applied CP (`Cursor::begin`
   350-361). The cursor keeps no nonce for the applied CP (`Cursor` 307-316; the nonce lives only
   in `Landing`, 270-273), so `to` is the identity that remains. Otherwise, if the mapped volume
   already has a snapshot with uuid `to` that is not `deleting` (`snapshots()`; `deleting` 2909),
   or the receiver itself asked for `to` earlier in this engine run and has not seen it land (a
   request still waiting or riding a CP: `snapshots()` lists only what is on disk, 2893-2912, a
   pending request makes `create_snapshot` answer `SnapshotExists`, 2352-2356, and a restart
   drops it, 2018; do not probe with `create_snapshot`, which takes the snapshot when nothing is
   pending), this delta was committed, or is being committed, before a crash or a timeout (step
   4's window, step 3's abort), with the cursor still at the base: re-land its chunks
   (idempotent, and `Cursor::commit` needs them all, cursor.rs:409-416), wait for that uuid's
   `SnapshotCreated` if it is still pending, and answer `Committed` without taking it again. A
   `to` that is `deleting` (a sweep or an operator got to it) is not that commit: it leaves at
   the next CP that takes the deletion and its uuid stays taken until then (`SnapshotExists`
   counts every held snapshot, 2352-2356), so wait for its `SnapshotDeleted`, then land the
   delta and take `to` again; answering `Committed` names a `to` that vanishes at that CP, and
   the next delta's `from` check then fails (`BaseMismatch`, a full resync). That reading holds
   only for the delta that asked for it (the same `to`, `cp_seq` and nonce, with no other delta's
   writes applied to the volume since), so it relies on the sender never re-sending a target it
   abandoned (4.8 step 3) and on the receiver deleting a `to` that lands after another delta
   began (step 3); a `to` on another volume is a mapping error. (b) Then the base: `Increment { from, to }`:
   `eng.snapshots().iter().any(|s| s.vol == replica_vol && s.uuid == from)` else
   `Abort(BaseMismatch)`. That tests one `from` against one volume: for a delta over several
   volumes the frames carry no per-volume `from` (step 3), so a per-volume check needs the
   receiver's own record of each volume's last `to` (8.10). A uuid test alone also accepts a `to`
   that landed late for an abandoned transfer (4.8 step 2); the cursor's applied CP (step 4) is
   what says which `to` was committed. (c) A `Steady` delta has no `from` (4.8 step 1): (b) does
   not apply, its `to` comes from the tags (one `VolEvent::SnapTag` per volume), and the frames do
   not say whether it is a whole image to land on a clean replica (8.11). And a `DeltaBegin` that
   does not resume a partial landed since `from` (`Replaced`, `Discarded`) lands on top of that
   partial: right only if the source never went back in between (7.21).

## 5. Hook points for Step 6

### 5.1 Scope C (sender/receiver) in the engine

- **No new engine write path is needed for the receiver if the replica stays online in the
  engine, but hiding it from clients is then a daemon change, not a free choice.** The daemon
  exposes every volume that is not offline: at start (`Pool::boot` pool.rs:209-295, which after
  the boot commit, 280, calls `ensure_device` for every volume with `!v.offline`, 281-293) and after
  `volume_create` (876), `volume_online(true)` (902) and `snapshot_rollback` (1098, 1111), all
  through `ensure_device` (678-709), which skips only offline volumes (684). Nothing durable says
  "replica": `VolRt` (1280-1301) has `offline` and no such flag, the volume-table row carries only
  `VOL_PRESENT`/`VOL_OFFLINE` (superblock.rs:216-221, written by `build_slot` 5636-5639), and the
  catalogue holds names. So an online replica becomes a client-writable ublk device at the next
  daemon start (Linux with ublk) unless a durable replica mark is added (a new volume-table flag,
  which is a format change, or a catalogue convention) and `ensure_device` and those call sites
  honour it, and so do `io_write` and `io_punch` (pool.rs:1225, 1245; dispatched 788-802), which
  reach any present volume by name or `#index` (`vol_index` 601-614) through the channel door the
  receiver also writes through (1169). If the lead wants the replica `offline` in the engine's
  sense, the change is **not confined to `VolRt` and `serve`**: `serve` (3402) runs in otap-cp
  over the generic `front: F` and sees only `Request { id: ReqId, kind }` (traits.rs:320-325;
  `ReqId(pub u64)` 264); the `FrontDoor` trait (traits.rs:438-452) has `fetch`, `copy_in`,
  `copy_out` and `complete` and nothing that says who submitted a request; and
  `ChannelFrontDoor::owns` with its `CHANNEL_ID_BIT` convention (daemon frontdoor.rs:63, 117-119)
  belongs to that daemon type and is unreachable through the trait. Reached, it still would not
  tell the receiver from `otapctl io write`: `IoHandle` is `Clone` over one `Sender` (daemon
  frontdoor.rs:37-40, `submit` 53-57), the pool submits `io_write`/`io_punch` through its own
  clone (pool.rs:258, 1169), and `pull` gives every submission an id of the same form,
  `CHANNEL_ID_BIT | next_id` (frontdoor.rs:123-136, the id at 125). So the owner test also needs
  the receiver's own door (a second channel that `DaemonFrontDoor`, 186-195, routes by its own id
  bit) or a submitter mark carried from its handle through `Submitted` (23-27) and `InFlight`
  (29-33), so the door can answer the question (a) below asks, or else `io_write` and `io_punch`
  refuse a replica themselves. The engine can tell a
  replication write from a client's only with (a) a new `FrontDoor` method (e.g.
  `fn is_replication(&self, req: &Request) -> bool`, defaulted to `false`) or an owner mark on
  `Request` (it derives `Copy, Hash`, traits.rs:319), plus (b) a `VolRt` flag (e.g. `replica:
  bool`, next to `offline` at 1280-1301) that `serve` (3414-3416) treats like offline for every
  request the door does not vouch for. With the default, only the doors that vouch or forward
  change: the `Box<F>` forwarding impl (traits.rs:419-435, which must forward the new method or
  boxed doors get the default), `DaemonFrontDoor` (daemon frontdoor.rs:277, which must route the
  question to the channel), the door that vouches (`ChannelFrontDoor`, 140, or a new receiver
  door; `SimFrontDoor`, sim frontdoor.rs:129, for receiver tests in the simulator) and a wrapper
  over a vouching door, such as `FlakyFront` (otap-sim/tests/adversarial_sim_tests.rs:166-190,
  over a `SimFrontDoor`), where a test runs it on a replica; a client-only door such as
  `UblkFrontDoor` (otap-ublk/src/frontdoor.rs:437) keeps the default. An owner mark on `Request`
  instead must be filled by every door that builds a `Request` (daemon frontdoor.rs:135, sim
  frontdoor.rs:50-90, otap-ublk/src/frontdoor.rs:465). Must not break: `build_slot`'s `VOL_OFFLINE` row
  (5637-5639), `check_vol_op` (2588-2592), `set_volume_online`'s `VolumeBusy` rule (2571), the
  `Status::Offline` contract for client doors (F18; the variant and its doc comment at
  traits.rs:376-378, in `enum Status` 354), and those six impls.
- **A public `create_snapshot_with_name(vol, uuid, kind, origin, name)`:** make 2327 `pub` (or a
  thin wrapper). It already does every check. Must not break: `create_named_snapshot`'s
  derived-uuid contract (2315-2325) and the daemon's `snapshot_create` wait by uuid.
- **No `snapshot_diff` by a remembered base CP for a new target.** A `pub fn
  snapshot_diff_since(&mut self, snap_id, base_cp: u64)` sharing the body of 2958-2991 (the
  `SnapshotOrder` check staying, 2975) is safe only to recompute the delta of a target taken
  while the base still existed: that target blocks every rollback below itself (`NewerSnapshots`,
  2641-2652), so its entries born at or below `base_cp` are the base's. It is **not** safe for a
  target taken after the base was deleted or lost. *Deleted*: with the base gone,
  `rollback_volume` to an older snapshot is no longer refused by `NewerSnapshots` (2641-2652);
  the rollback restores that snapshot's root, whose entries are born at or below `base_cp`
  (4815-4831; `rollback_gen` moves at 5818), and a since-`base_cp` diff misses them, so the
  replica keeps blocks the source no longer has. *Lost* (a rewind, rule 4): the engine resumes
  numbering from the selected CP (`cp_seq: sb.cp_seq` 2042; `new_job` takes `cp_seq + 1`,
  3972), so CP numbers above it are reused by different writes; a `birth > base_cp` diff skips
  the new writes born in the reused range and never undoes the lost CPs' writes, which the
  receiver may already hold. In both cases the answer is the `BaseMismatch` path (7.3): a full
  resync, `snapshot_diff(target, None)` into a replica punched or recreated first (a full diff
  has no entry for an LBA the target never mapped; how today's frames can carry that, 4.8 step 1
  and 8.11). Must not break: `check_diff` in the harness
  (787-854, with its private helper `engine_hole_is_newer` 856-861, called at 822), which uses
  `live_diff(vol, Some(id))` (795-798).
- **Carry `uuid` in `Event::SnapshotCreated`** (411-418, pushed 5944-5948): a one-field addition.
  Adding a field breaks every pattern that spells `SnapshotCreated { snap_id, vol, cp_seq }`
  without `..`: **two** do, the harness's wait (harness.rs:700-704) and the daemon's `Pool::note`
  (pool.rs:382-386), so neither `otap-sim` nor `otap-daemon` compiles until both are touched. The
  `..` patterns keep working: the daemon's `snapshot_create` loop (pool.rs:971, in 963-996) and
  review_sim_tests.rs:198 and :445; `step5_sim_tests.rs` and `review_step5_sim_tests.rs` contain
  no `SnapshotCreated` pattern at all. Grep for the full pattern before adding.
- **A transfer-in-flight guard:** the engine has no notion of "a delta is reading this
  snapshot". Options: the sender pins the target with `PIN_REPLICATION` at creation (no engine
  change, see 4.8 step 3), or a `transfer_pins: HashSet<u64>` in `Engine` consulted by
  `delete_snapshot` (2470-2472) and `set_replication_base` (not durable, cleared at boot). A crash
  does not end the transfer: 4.8 steps 1, 3 and 5 persist the target in flight and resume it
  across a sender restart with the same `to`, `cp_seq` and nonce (cursor.rs:363-372), so the
  sender must register it again at start, and the target is unprotected until it does (the
  durable bit has no such window: the start-up `set_replication_base` clears it only as a
  pending change, which the re-pin right after replaces, 2513-2514). The lead decides (8.2).
- **The deletion's own walk must not fail the CP:** the `?` on `pinned_diff(.., older, ..)` at
  4954-4956 turns one unreadable map page of the deleted snapshot into a failed CP, a fold that
  sets `fold_damaged` (a page that does not verify) or a CP that leaves `tick` and repeats (a
  plain CP, or any CP whose page read the device fails) (trap 7.22), and every
  deletion a sender or receiver makes (the old base of 4.8 step 5, an abandoned target, the
  sweeps, 4.9's `from` and its volume deletion) is exposed, since short of the same walk
  (`snapshot_diff(snap, next_older)`, 7.8's cost) nothing
  public tells beforehand. The smallest change that makes this bullet's title true covers
  `MapError::Io` as well as `MapError::Format`, at all three places: the `?` of 4954-4956, and the
  two neighbour lookups, which today answer only `Format` with "may still name the block" (4969,
  4988) and pass `Io` and every other error up (`Err(err) => return Err(err.into())`, 4974,
  4993). Treating only `Format` at the `?` leaves a persistent read error on the deleted
  snapshot's page, on its newer neighbour's page or on a live-map page failing `prepare_meta` as
  before: `step_staging`'s guard takes only `Format` (4453), so a fold and a plain CP alike leave
  `tick` at `other => Err(other)` (4461) and the job is dropped with its generation frozen
  (4210-4213; 7.22). For each such error, either release nothing from that page and report it
  with an `Event::Internal`, as a damaged snapshot's deletion does (5013-5019), and let the CP
  commit (a leak: the blocks stay counted in `data_live` with no root naming them, which
  `audit_liveness` reports as drift, 2866-2875, and for an `Io` that was transient a needless
  one), or drop the deletion from this CP with a report and keep the snapshot (out of
  `deleted_snapshots`, 5020, and out of `snap_deletes` too, which `finalize` clears only for what
  the CP deleted, 5746, or every CP walks it again). Must not break: E36's release accounting
  (`released`, 5020), `audit_liveness` (2820-2878) and the deferred frees (`page_referenced`,
  5351-5361).
- **A public "fold blocked" signal:** `fold_blocked_at` (1417; set 4445, cleared 5958) and
  `snap_requests` (1390) have no accessor and no `PoolStatus` field (935-984), and `pool_full`
  (`is_pool_full` 3185, `PoolStatus::pool_full` 3053) is cleared by every committed CP (5966), so
  a sender or receiver cannot tell a parked snapshot request from one whose CP has not run yet
  (4.8 step 2). Smallest change: `PoolStatus::fold_blocked_at: Option<u32>` and
  `PoolStatus::pending_snapshots: usize`, filled at 3032-3059 (or a `pub fn fold_blocked(&self)
  -> bool` next to `is_pool_full`; `fold_damaged`, 1421, needs the same for case (ii) of 4.8
  step 2; but see the catch below: the raw value compared with `free_segments()` misleads).
  Alternative: `fail_cp` (4256-4313) answers `job.snaps` with `SnapshotFailed` when
  `no_space` instead of requeueing them (4262), which makes the doc comment 419-421 true but
  reaches only the requests the failing CP carries: a request made while `fold_blocked_at` holds
  never enters a `job.snaps` (they are taken only at a fold, 4714-4722; the CPs meanwhile are
  plain, 4660-4665, and commit), so it also needs a refusal at the call (`create_snapshot_named`
  2327-2405 answering `NoSpace` while the fold is blocked, by a test that is not the raw one of
  4660-4663, see below), and a request left behind by a damaged-page fold needs
  `fail_fold_requests` (4228-4242) to drain `snap_requests` too, since no fold follows while
  `fold_damaged` (4655-4657). Those three still miss two groups of waiting requests, both in
  `snap_requests` rather than in a `job.snaps`: (a) requests made before a **data or relocation
  stripe** found no segment (4360-4364, 4376-4378): that path does not set `fold_blocked_at`
  (only 4445 does), and with `pool_full` set (4301) the frozen generation is retried only on
  someone's `request_cp` (`maybe_start_cp` 3948-3950; `maybe_pump` returns while a generation
  is frozen, 2155); (b) requests not yet eligible (4642-4648) for a fold that fails at 4447: the
  partition left them in `snap_requests` (4716-4722), only `job.snaps` is spliced back (4262),
  and they were accepted before the failure, so the call-time refusal does not reach them. Both
  need a fourth change, which subsumes the first: on `no_space`, `fail_cp` answers every request
  in `snap_requests` (after its splice at 4262), as `fail_pending_snapshots` already does at 4305
  when a shutdown was asked. **The test is the catch.** `free_segments() <= at` means "blocked"
  only where `should_fold` runs it, inside `prepare_meta` (4714) after the CP's own data stripes
  are allocated: the failed attempt's staging allocated its data segments (`open_data_segment`
  pushes each into `job_data_segments`, 4621-4631), `fold_blocked_at` is recorded after that
  (4445), and `fail_cp` (4447) frees them again (4275-4292; the `SegmentFreed`s of 7.4), so
  right after the failure `free_segments()` is `fold_blocked_at + D` (D = the data segments the
  attempt allocated; the metadata stream allocates from a copy of the table, otap-cp meta.rs:117)
  while the requests are still parked. A request made then passes a call-time refusal built on
  the raw test; its `request_fold` retries the frozen generation, whose staging takes the D
  segments again; `should_fold` finds `free_segments() <= at` and keeps the CP plain; the plain
  CP commits with no space failure, so the fourth change never fires; and the request parks
  silently (4.8 step 2 case (i)). An accessor built on the same test (`fold_blocked()`, or
  `PoolStatus::fold_blocked_at` compared with the public `free_segments()`, 3276) gives the same
  false "not blocked". So the second and fourth changes move the retry to the sender's
  clock only when the failed attempt allocated no new data segment (D = 0; the third needs no
  such test, since after a damaged-page fold every new request is refused `MapDamaged` at the
  call, 2370-2371). Otherwise a
  call-time test or an accessor must allow for the CP's own data segments, which only
  `prepare_meta` knows: comparing against the free count before the attempt's staging (record it
  after `fail_cp`'s frees, with `should_fold` adding back the CP's own new data segments, or
  remember D) is right while the next CP allocates no more than D. The exact place is the gate
  itself: a CP that `should_fold` keeps plain only because of `fold_blocked_at` (4660-4665) while
  requests are eligible (4671-4676) answers them `SnapshotFailed`. Short of one of these the
  sender keeps its timeout.
  Must not break: `should_fold`'s gate
  (4660-4665), the daemon's `wait_quiet` (pool.rs:561-566, keyed on `is_pool_full()`),
  `wait_cp_outcome` (pool.rs:570-587), and, with the alternative, the limit test's batches of
  requests riding one fold (step5_sim_tests.rs:347-422).
- **Exposing the pinned size:** `SnapshotInfo` has no `size_blocks`; add `size_blocks:
  pinned.root.lba_count` at 2895-2912 (damaged snapshots: 0) so the sender can emit `Resize`
  events and bound reads without probing `snapshot_read`'s `OutOfRange`.
- **A ranged diff:** `snapshot_diff` (2958-2991) walks the whole pinned index in one call and
  returns one `Vec`, stalling the engine thread for the walk (7.8). A `pub fn
  snapshot_diff_range(&mut self, snap_id, base: Option<u64>, from_lba: u64, max_entries: usize)
  -> Result<(Vec<(u64, MapEntry)>, Option<u64>), EngineError>` (the second value the LBA to
  resume from) needs a page range in `pinned_diff` (otap-map lib.rs:1714-1734, which iterates the
  index from page 0, 1721) and bounds both the memory and the stall per call, so the sender ticks
  between calls; it looks the pinned root up again at each call, as `snapshot_read` does
  (2931-2934), since a cleaning CP may rewrite it in between (5756-5765). Must not break the
  other callers of `pinned_diff`, which is every one there is: `audit_liveness` (`pinned_diff(..,
  0, ..)` 2847), `snapshot_diff` itself (2984; tested by `snapshot_diff_refuses_a_newer_base`,
  otap-sim/tests/review_sim_tests.rs:608-621, and at 1026), the deletion's walk (4954) and the map
  test's `check_pins` (otap-map/tests/review_map_tests.rs:266-286, the call 274). The harness's
  `check_diff` (harness.rs:787-854) is not among them: it calls `live_diff` (795-798), which walks
  `MapSet::diff` (engine.rs:3003; otap-map lib.rs:1620), so a page range added to `pinned_diff`
  alone leaves it as it is.
- **The receiver's "atomic CP":** the engine commits CPs on its own triggers (`maybe_cut`
  3888-3909: timer, arena half, back-to-back); a delta's writes become durable in the live map
  across several CPs before its snapshot. There is no transaction. If the brief's "committing the
  received delta as an atomic CP on the target pool" (docs/10-phase-c-step-briefs.md:623) must be
  literal, it needs either staging outside the live map
  (a staging volume rolled over by `rollback_volume`-like map surgery, new `CpInput` work in
  `otap-map`) or an acceptance that atomicity is "the `to` snapshot exists or not" (what the
  current API gives). See 8.4.
- **The receiver's applied CP, committed with its `to`:** `InboundState` (otap-format
  superblock.rs:317-346; `sb.inbound` 488-489) is reserved for exactly this, but `build_slot`
  (5589-5591) starts every CP from a zeroed `SuperblockSlot` (superblock.rs:715-720;
  `Superblock::new` 523-532, whose doc says "a page that was read is never carried forward",
  519-521) and never sets it, so 4.9 step 4's crash window stays open. Closing it needs (a) an
  in-memory copy of the groups' `InboundState` in `Engine`, restored by the boot from the
  selected slot (as `cp_nonce` is, 2043) and written by `build_slot` into every slot, and (b) a
  way to change it in the CP that takes `to`: an update attached to the `SnapRequest`
  (1096-1107) and applied only when that request rides the CP, or a pending update queued like a
  pin change (2513-2515) that the same CP carries. A receiver restart then reads its applied CP
  from the engine instead of a separate cursor write. Must not break: the layout the golden
  superblock vector pins (it carries an `inbound[0]`, otap-format/tests/format_tests.rs:2161-2173,
  checked at 2633-2634; E05) and the offset test pins (`inbound` at byte 544,
  format_tests.rs:269), the boot's slot selection, and `Superblock::new`'s rule that every
  slot is built fresh.

### 5.2 Scope B (incremental tables) in the engine

- The snapshot table is produced at **one place**: 5168-5179 (`records` from `after`,
  `snap_table_image`, `stream.place_region`), consumed by `build_slot` (5611 `sb.snap_table =
  prep.snap_table`), `table_regions` (5398-5401; the previous CP's regions go into `freed` at 5370 and are released under the deferral), and
  read by `boot.rs:493-517`. The catalogue likewise: produced 5181-5234 (carried forward
  unchanged when equal, 5222-5223), stored 5612, read by the boot (`catalog_damage` handling,
  E39 W4). An incremental design replaces `place_region(&snap_table_image(..))` with "append
  this CP's `SnapRecord` deltas (new 5159, deleted 5020, pins 5126, rewritten roots 5097) to a
  log region and fold the table at a fold CP"; `finalize` already has the per-CP delta lists
  (`new_snapshots`, `deleted_snapshots`, `pins`, `rewritten`) in `Prepared` (1210-1241).
- Must not break: (a) `parse_snap_table`'s strict ascending ids (records.rs:1258-1286), (b) the
  boot's rule that damage to a snapshot this CP took tears the candidate (boot.rs:500, 511), and
  that a read error on any snapshot's root page or index fails the boot (510, 292-295),
  (c) the deferred free of the previous table region (`table_regions` -> `freed` 5370 ->
  `release_freed` 6004), (d) `audit_liveness`'s walk of `snapshots` (2820-2878), (e) the golden
  vectors (`tests/vectors/v2.3/`; E05: a vector set changes only with a minor bump).
- The attestation pass (B) can reuse: `snapshots()` + `SnapState.pinned` (private; expose a
  `pinned_root(snap_id) -> Option<&PinnedRoot>`), `audit_liveness` (2820), `map_entry` (3157),
  `peek_disk` (3117), `EngineRefs::snapshots` (6229-6258: which snapshots pin an entry).

## 6. Test patterns to copy

- **Harness:** `otap_sim::Sim` (harness.rs:242): `SimConfig::small(seed)` (83-111: 48 segments
  of 8 x 32 KiB stripes, a 2 MiB arena (`arena_bytes`, 89) = **507 slots, not 512**: the arena
  takes its 4 KiB header (`ARENA_HEADER_SIZE = BLOCK_SIZE`, crates/otap-arena/src/lib.rs:108) and
  a 32-byte descriptor per slot padded to whole pages (`SLOT_DESC_SIZE`, 104) off the top, so
  `Geometry::for_len` (lib.rs:662-684) gives (2,097,152 - 4,096) / (4,096 + 32) = 507 slots with 4
  descriptor pages; adversarial_sim_tests.rs:368 calls it "a 507-slot ring". Ask
  `s.arena_slots()` (harness.rs:1370-1373, `otap_arena::slots_for_len`) instead of hard-coding
  it: a write longer than `arena.slots()` answers `TooLarge` (engine.rs:3434), so under `small`
  no single write may exceed 507 blocks (on the two default 64-block volumes the range check,
  3430-3433, which runs first, bounds it at 64; the slot bound matters for a larger replica
  volume a receiver test creates). Volumes `(0, 64), (1, 64)`, a fold policy that folds every
  5 CPs), `Sim::new(cfg)` (289), `boot()` (386), `tick()`/`tick_until(pred,
  what)` (484/500; `TICK_BUDGET = 100_000` at 28), `write(vol, lba, blocks)` (618),
  `punch(vol, lba, blocks, zeroes)` (565), `read` (627), `request_cp()` (661: request then wait
  `cp_seq > before && is_quiet()`), `request_fold()` (670: waits `map().fold_cp() > before`),
  `snapshot(vol)` (683-723: a deterministic uuid from `next_uuid` and the seed, `create_snapshot(..,
  Crash, User)`, waits for `SnapshotCreated` of the volume, records the model's image in
  `s.snaps: Vec<SnapExpect>` 176-187), `verify_snapshots()` (729-782: every snapshot read back
  block by block via `snapshot_read`; one that vanished is allowed only after a boot that
  rewound below its CP or a requested deletion), `check_diff(snap_id)` (787-854: `live_diff(vol,
  Some(id))` at 795-798 against the model, holes included; its private helper
  `engine_hole_is_newer` is 856-861, called at 822), `settle()` (864), `request_snapshot_deletion` /
  `delete_snapshot` (875/886: waits `SnapshotDeleted`, returns `released`), `quiesce(vol)` (995),
  `rollback` (1011), `resize_volume` (1027), `delete_volume` (1069), `create_volume` (1121),
  `audit_liveness()` (1107), `crash_power()` (1169), `crash_daemon()` (1186), `shutdown_clean()`
  (1202), `verify_recovery()` (1261), `clear_nvfail()` (1354). `s.events: Vec<Event>` (264) holds
  every event. The engine is reachable as `s.engine()` (329) for direct calls.
- **Test files and helpers:** `step5_sim_tests.rs` 1-165 (`small`, `sim_with`, `fifo1`,
  `no_auto_fold`, `ok`, `punch_ok`, `power_cut_and_recover`, `daemon_crash_and_recover`,
  `clean_restart`, `check_everything` = disk prefix + snapshots + every diff + liveness,
  `tick_to_phase`). The pin/base test to copy for scope C's base handling:
  `a_replication_base_refuses_deletion_until_superseded_or_unpinned` (210-284: `set_replication_base`,
  `request_cp`, flags read through `snapshots()`, the exact `SnapshotPinned` message, the pin
  surviving a daemon crash, supersession, `None`). The limit test: 347-422 (255 then
  `SnapshotLimit`, 16 `Transfer` then `SnapshotLimit`, batches of requests riding one fold with
  `tick_until(|e| e.snapshots().len() == n)`). `review_step5_sim_tests.rs:234-313` (a rollback
  refused while a snapshot is in flight: `fifo1()` to slow the device, `tick_until(phase in
  {Metadata, Flushing, CommitA})` to catch the CP past `prepare_meta`), 690-729 (pending pins
  govern deletion). `step4_sim_tests.rs:270-332` (snapshots + diffs across random crashes:
  the loop shape for a sender's "diff is exactly what changed since").
- **Random walks:** `proptest!` with `cases: proptest_cases(64)` (step5_sim_tests.rs:1385-1391;
  the helper 1378-1384 reads `PROPTEST_CASES` itself, 64 default; the same in
  step4_sim_tests.rs:1387 and `cases()` in review_step5_sim_tests.rs:914), `failure_persistence:
  None`, `max_shrink_iters: 512`. E39 C1 explains why the file reads the variable itself. A new
  replication walk should follow `random_ops_with_deletions_rollbacks_cleaning_and_crashes...`
  (1401) and use `check_everything` after every step.
- **Daemon-level:** `pool.rs` `wait_for` (476) / `sync_now` (592) / `snapshot_create` (963-996)
  are the shapes for an in-daemon receiver; `ChannelFrontDoor` + `IoHandle::submit` (frontdoor.rs
  53) is the data path a loopback-TCP test can drive (`otapctl io write` already does,
  pool.rs:1225).

## 7. Risks, pitfalls and traps

1. **Snapshot ids are reused across boots, CP numbers after a rewind** (rule 4). `next_snap_id`
   is recomputed at every boot as max(surviving id) + 1 (1908-1913) and never persisted, so
   deleting the pool's highest-id snapshot (an aborted transfer's target, any volume's newest)
   and then booting, cleanly or not, gives that id to the next snapshot, with no rewind; a rewind
   that lost snapshots does the same (895-896 names only that case). A sender or a cursor that
   stores the base as `snap_id` can address another snapshot after any restart. Store the uuid
   and resolve the id through `snapshots()` at every use. The `cp_seq` is no identity either:
   after a rewind the engine numbers CPs again from the selected one (2042, 3972), so a
   remembered `base.cp_seq` can name other writes; keep it only as the delta header's `base`,
   next to the uuid. (The uuid stays unique only as long as nobody issues one twice: the engine
   checks held, waiting and in-flight snapshots, 2352-2356, so a uuid lost to a rewind could be
   given again; a sender that draws fresh uuids never does.) The header field that tells a reused
   `cp_seq` apart is `cp_nonce` (frame.rs:49-50; the receiver's `Cursor` discards a partial whose
   `cp_seq` matches with another nonce, cursor.rs:363-376), and the engine keeps no nonce per
   snapshot: `cp_nonce()` (3208-3212) is only the newest committed CP's, so the sender captures it
   in the tick that delivers the target's `SnapshotCreated` and persists it with the target
   (4.8 step 2).
2. **Uuid uniqueness is pool-wide** (2352-2356). Replicating one source volume into two replica
   volumes of one pool, or a loopback test with sender and receiver on the **same** engine, cannot
   create the `to` snapshot twice: `SnapshotExists`. Loopback tests need two engines (two
   `Sim`s / two pools), or the receiver must map uuids (then `Increment { to }` no longer names
   what the replica holds). For the same reason a delta over several volumes needs one `to` uuid
   per volume on both pools, which only `VolEvent::SnapTag` can carry (it names a volume,
   frame.rs:326-328; `Increment` holds one pair, 222-227, and `DeltaBegin`'s `SnapTag` names no
   volume, 238-245), and no frame carries a per-volume `from` (4.9 steps 3 and 5).
3. **The base can vanish under a delta.** Only the pin bit protects it; `set_replication_base(vol,
   None)`, `set_snapshot_pins(base, 0)` or a competing `set_replication_base` from another
   session unpins it and a `delete_snapshot` lands at the next CP. The target's data stays
   readable (E36 releases only what the deleted snapshot alone referenced), but
   `snapshot_diff(target, Some(base))` then fails with `NoSuchSnapshot`. A kept `base.cp_seq`
   helps only to recompute the delta of a target taken before the deletion; a new target diffed
   against it is unsafe (a rollback the deletion unblocked, or CP numbers reused after a rewind,
   5.1), so the next delta after a lost base is a full resync. The same unpin-then-delete aimed
   at the *target* makes the sender's own `set_replication_base(vol, Some(target))` fail with `NoSuchSnapshot`
   after it has already unpinned the old base (7.19). Also a boot that **rewinds** below the
   target's CP loses the target (E25), while the receiver may already hold `to`: handle
   `BaseMismatch` on the next `DeltaBegin`.
4. **A snapshot needs a fold and a fold needs room.** Each `create_snapshot` costs a fold of
   every dirty page and every touched index; a fold that does not fit the reserve fails the
   attempt (4439-4448), sets `fold_blocked_at` (4445) and **puts the requests back** (`fail_cp`
   4262): no snapshot event reaches the sender while the engine runs. The attempt's `fail_cp`
   (4256-4313) pushes `CpFailed { reason: "no free segment" }` (4257-4260, logged at
   pool.rs:376) and a `SegmentFreed { pseg, discarded: false }` for every data segment the
   attempt allocated (the loop 4275-4292, the event 4283-4286; at 4447 the generation's data
   stripes are already staged, since `prepare_meta` runs only once every stripe has landed,
   4420-4427, so these normally come with the `CpFailed`), none of which names a request; only
   when a shutdown was asked does 4305 fail the requests. The requests then wait with no answer
   at all while `should_fold`'s test (4660-4665) blocks, that is until a segment is freed
   (`free_segments()` above `fold_blocked_at` where `should_fold` reads it, inside `prepare_meta`
   after the CP's own data stripes are allocated, 4714; read anywhere else it does not answer the
   same question: right after the failure, for one, the count is higher by the data segments
   `fail_cp` just handed back, which the retry takes again, 5.1), **and after that until some CP
   runs**, or a reboot: the frees advance only at commits and `fold_requested` starts no CP by
   itself, so on
   an idle pool the request waits for someone's `request_cp` (rule 14). A
   request made *during* that block does not even get the `CpFailed`: it is accepted (2327-2405
   never checks `fold_blocked_at`), the CP it asks for commits plain, and it waits silently
   (rule 14).
   `SnapshotFailed` comes only from a map page that does not verify during the fold that carries
   the request (4453-4460: `fold_damaged` is set at 4454 and never cleared in the engine, so
   every later `create_snapshot` is refused with `MapDamaged`, 2370-2371, until a reboot after
   the repair; a request of the volume not yet eligible for that fold, 4642-4648, is not
   answered and waits for the stop, 4246-4254) or from the engine stopping (4305, 5987). So the
   sender needs a timeout on `SnapshotCreated`, which is the test for every silent case (and
   which withdraws nothing: the request still waits, blocks volume operations, holds a reserved
   slot and becomes a snapshot when a later fold takes it, 4.8 step 2); a
   `CpFailed { reason: "no free segment" }` seen meanwhile is only an early hint (**not** an
   `is_pool_full()` (3185) check: `pool_full` is cleared by any committed CP, 5966, and a plain
   CP commits after anyone's `request_cp` while the fold stays blocked behind the private
   `fold_blocked_at`, 4.8 step 2), and must treat `SnapshotFailed` as a hard stop for this pool,
   not as "retry later" (the uuid itself is free again: the request is dropped at 4229-4235 and
   `SnapshotExists` no longer sees it).
5. **Offline volumes refuse the front door, which is the only write path** (3414-3416). "A
   replica volume must be offline to clients" cannot be implemented with `set_volume_online(false)`
   if the receiver applies writes through `FrontDoor`. Keeping the replica online in the engine
   does not hide it either: the daemon exposes every online volume at start and after create,
   online and rollback (pool.rs:280-293, 876, 902, 1098, 1111; `ensure_device` skips only offline
   volumes, 684), and nothing durable marks a replica (`VolRt` 1280-1301), so it becomes a
   client-writable ublk device at the next daemon start; and `otapctl io write` and `io punch`
   (pool.rs:1225, 1245) write any present volume through the same channel the receiver uses
   (1169), exposed or not. Both options are changes: a durable replica mark honoured by the
   daemon's exposure sites and by `io_write`/`io_punch`, or a replica flag together with a
   `FrontDoor`-level owner test (5.1: the trait and the doors that vouch or forward change, not
   only `VolRt`), which on today's single channel cannot tell the receiver from `io write`
   without a door or a submitter mark of the receiver's own (5.1). A third needs no change but
   leaves windows: offline at rest, online in the engine only while a delta lands (4.9 step 1).
6. **The snapshot is "at the next cut", not "at the call".** Writes acknowledged after
   `create_snapshot` but before the generation is cut are in the snapshot (4.1 steps 2-4). A
   receiver must not have further delta writes in flight when it requests the `to` snapshot
   (and must not start the next delta's writes until `SnapshotCreated`); a sender taking a
   target while clients write gets whatever landed before the cut, which is correct for
   crash-consistency but not a "quiesced" image.
7. **`Event::SnapshotCreated` carries no uuid, and `SnapshotFailed` carries no id.** Correlate by
   `vol` + a `snapshots()` lookup by uuid (pool.rs:986-994); an older request of the same volume
   landing late produces an event that is not yours.
8. **`snapshot_diff` output has no data and no run structure.** It is one `(lba, MapEntry)` per
   block; holes are entries with `addr == 0, birth != 0`; coalesce runs yourself; `MapEntry.crc32c`
   is the block's CRC (0 for holes). `snapshot_read` re-checks the CRC and answers
   `Err(Corrupt { vol, lba })` for the **whole call** at the first bad block (the `?` at 2951;
   `PeekSource`, 854-863, has no corrupt case; `dst` is filled only up to that block): read the
   rest of the run again from `lba + 1` in a new call (or block by block), since the failed call
   returned none of it, and decide what the delta does with the bad block. **"Ship it as
   corrupt" has no path today.** A chunk descriptor knows only `Data` and `Hole` (`BlockKind`,
   otap-format stripe.rs:85-95, where values 2 and 3 are reserved; the descriptor is the on-disk
   format's, F4) plus a CRC (310-312). `Chunk::summary` (otap-stream frame.rs:507-512) is
   `StripeSummary::parse`, which checks the header, the summary CRC and the descriptors but no
   block against its descriptor's CRC (stripe.rs:547-585); the frame's `payload_crc` and
   `DeltaEnd`'s digest are computed by the sender over what it sends (otap-format stream.rs:5-7,
   193-201; frame.rs:528), so both pass. The receiver's only path, the front door, CRCs whatever
   bytes it copies in (`serve_write` 3517-3524). So a bad block shipped as its bytes under its
   recorded CRC lands on the replica as a valid block that reads `Ok` (silent corruption) unless
   the receiver checks every `Data` block against its descriptor's `crc32c` before submitting it
   (4.9 step 2), and even then the receiver engine has no call that records a block as corrupt
   (no public `Engine` call takes a block with a CRC of its own, and every front-door write is
   CRC'd afresh, 3517-3524). `Abort(Corrupt)` does not fit either: it says a payload CRC failed
   and the sender resends (frame.rs:604-607), and a resend reads the same bad block. The choices
   are to abort the delta (the target cannot ship while that block is bad, and a new target
   shares it unless the source rewrote it), to punch or skip the block with a report (the
   replica then reads zeros, or its previous content, at that LBA, both as valid data), or a
   protocol and engine change (a reserved block kind or a chunk flag for "corrupt at the
   source", and a receiver call that lands the LBA so that reads answer `Status::Corrupt`). The
   lead decides (8.12). It is also one allocation and one engine-thread stall per call:
   `snapshot_diff` returns the whole delta as one `Vec<(u64, MapEntry)>` (2982-2990), 24 bytes
   per block in practice (`MapEntry` is 16, otap-format map.rs:475, plus the `u64` LBA), so a full
   diff of a 1 TiB volume (2^28 blocks) is about 6.4 GB, more while the `Vec` grows, and it reads
   every map page with `max_birth > base_cp` (otap-map lib.rs:1722; a page holds 1024 entries,
   16 KiB, for 4 MiB of volume, otap-format constants.rs:107-111, so up to 262,144 page reads for
   1 TiB) through the synchronous `Backstore::read` (traits.rs:249-250, via `BackstoreReader`
   1351-1360) on the engine thread. No `tick` runs during the walk, so the front door, the CPs
   and, in `dbladed`, every control request (answered on the same thread, daemon.rs:278-316) wait
   for it. `snapshot_read` reads synchronously too (`read_entry` 3724, one device read per data
   block), but its caller chooses the run. There is no ranged or resumable diff (5.1).
9. **Shrinks are invisible in a diff.** A shrink releases the entries beyond the new end, which
   read as unmapped (birth 0) afterwards; `snapshot_diff` emits nothing for them. The replica
   would keep stale data beyond the new end unless a `Resize` event is sent and applied
   (and a shrink on the replica needs it offline and quiet, 2678-2679). A shrink followed by a
   grow between base and target is invisible to the diff **and** to a comparison of the two
   snapshots' sizes: the shrink releases the entries beyond its end and drops their pages
   (otap-map lib.rs:1248-1271), the grow pads the index with absent pages (1272-1274), so those
   LBAs read unmapped with birth 0 in the target while the base, and the replica, still hold
   data there. The sender has to learn of it from the `VolumeResized` events between the two
   snapshots (in memory only: a restart in between loses them) or treat any resize between them
   as needing a full resync.
10. **Rollback semantics on a replica:** `rollback_volume` refuses with `NewerSnapshots`
    (2641-2652) and requires offline+quiet; a replica that keeps every `to` snapshot cannot roll
    back to an older one without deleting the newer ones first (pinned ones refuse). If an
    aborted delta leaves partial writes in the replica's live map, a resume that re-sends only
    the missing chunks is consistent (writes are whole-block and idempotent), unless a dirty
    boot held the replica by nvfail in between: its acknowledged writes may then be lost, so the
    delta restarts from `DeltaBegin` (7.20); a rollback is not needed and would also bump
    `rollback_gen` (5809-5828). That covers a resume of the same delta only: a delta that does
    not resume the partial lands on top of it, and needs the replica rolled back to `from` first
    once the source may have gone back (7.21).
11. **The 16 reserved slots count pending and in-flight `Transfer` requests** (2373-2393). A
    sender that takes a new target per delta and never deletes old bases hits `SnapshotLimit`
    after 16; delete the superseded base after every `Committed` (4.8 step 5). That alone does
    not prevent it: every attempt aborted after its target was taken leaves one more `Transfer`
    snapshot (pinned at step 3; a later `set_replication_base` clears its bit, 2549-2558, and
    deletes nothing), so does a timed-out request that lands later (4.8 step 2), and so does a
    crash after a cleaning CP landed the old base's unpin (5121-5128) but deferred its deletion
    (4912-4917; `snap_deletes` is not durable); each is in the reserved class (`is_user_class`
    1086-1091). Retry the same target until it commits, or unpin and delete an abandoned one
    (4.8 step 3), and sweep at every `Committed` and at start (4.8 step 5). The receiver meets
    the same limit with its `to` snapshots (4.9 step 3).
12. **`set_replication_base` re-pins only within one volume** (2538-2548) and the engine never
    enforces a single `PIN_REPLICATION` per volume: `set_snapshot_pins` sets the bit on any number
    (the daemon's `snapshot_pin` refuses it by hand, pool.rs:1118-1127, but an in-engine sender is
    not bound by that). **Two pinned snapshots per volume is this map's own recipe's normal
    state** (4.8 step 3 pins the target for the transfer; the old base keeps its committed bit
    until step 5's `set_replication_base(target)` lands, and shows `deleting: true` only from
    step 5's `delete_snapshot` (2481-2482, read at 2909) until the CP that takes it: during the
    transfer neither is deleting, and after an aborted delta the newest pinned snapshot is a
    target the receiver never committed), so the base lookup must never go by the bit, neither a
    bare `find` nor the newest: resolve the uuid stored at the last `Committed` (4.8 step 1).
    Both recipes also rest on **one replication relationship per volume**: `set_replication_base`
    keeps one bit per volume (it clears every other snapshot's, 2549-2558), each side runs it at
    start, and the sweeps delete every other unpinned `Transfer` snapshot of the source volume
    (4.8 step 5) and every `Transfer` snapshot of the replica volume but the last committed and
    the in-flight `to` (4.9 step 3). A source volume in two relationships, or a replica volume
    sent onward, would have one relationship unpin and then delete the other's base and targets.
    The frames carry a relationship id (`FrameMeta.rel`, otap-stream frame.rs:41-42) and the
    design multiplexes relationships on a pipe (docs/05-architecture-brainstorm-03-record.md:3477),
    but `SnapRecord` has no such field (records.rs:359-384).
13. **Rewinds and `next_snap_id` in one CP:** ids in one CP are `next_snap_id + i` in request
    order (5140) and `next_snap_id` moves only at `finalize` (5949); a failed attempt reuses them,
    which is fine, but code that remembers an id before `SnapshotCreated` is wrong.
14. **Volume operations and snapshots exclude each other in both directions** (2343, 2475, 2653,
    2683, 2707) and a cleaning CP defers both deletions and ops (4797, 4912-4917): a receiver
    applying `Resize`/`Delete` events while its own `to` snapshot is pending gets `VolumeBusy`;
    order events before the snapshot request, and wait for `VolumeResized` before writing beyond
    the old end (writes past `size_blocks` answer `OutOfRange`, 3430-3433).
15. **`live_diff` and `snapshot_diff` see committed state only.** Acknowledged writes in the
    arena are not in them; a sender that diffs the live map (a `Steady` delta) must `request_cp`
    and wait quiet first. `is_quiet()` (3194) is the harness's gate; the daemon uses `wait_quiet`
    (pool.rs:561) which skips the wait on a full pool.
16. **Names:** `create_snapshot` (uuid given) writes no catalogue entry; the replica's snapshot
    will have `name: None` until a public uuid+name entry point exists; `find_snapshot(vol, name)`
    then cannot find it, and `otapctl snapshot delete <vol> <name>` cannot address it.
17. **Damaged snapshots** (`pinned: None`) are listed, pin their data, refuse reads, rollback and
    being diffed (2934, 2636, 2972: the refusal at 2969-2972 is the diffed snapshot's, the
    target's) and release nothing when deleted (5013-5019). A damaged **base** is no obstacle:
    `snapshot_diff` takes only its record's `birth_cp` (`base_cp` 3009-3024, which never looks at
    `pinned` or the damage), so the increment from it is correct (the receiver holds its own copy
    of the base), it still blocks rollbacks below itself (2641-2649), and its cost is the leak
    when 4.8 step 5 deletes it. Check `SnapshotInfo.damaged` (2906) on the **target**, where
    `snapshot_diff` refuses anyway; a snapshot is built with `damage: None` (5159-5163), so only a
    boot finds one damaged (a target resumed after a restart). `damaged: false` says nothing about
    the map pages (7.22).
18. **The `(vol, uuid)` catalogue key and the boot's filter** (the loop 1932-1955, introduced by
    the comment 1922-1924; 1925-1931 is the `CatalogDamaged` warning) drop a name whose
    snapshot is absent; a scope-B incremental catalogue must keep this rule or the boot reports
    `Internal` events for every orphan.
19. **`set_replication_base` is not atomic on error.** It queues one `set_snapshot_pins` per
    snapshot of the volume in id order (2538-2558) and returns at the first `Err` (`?` 2556).
    When the new base's deletion is already pending (an operator's `set_snapshot_pins(target, 0)`
    then `delete_snapshot(target)`, accepted at 2470-2472), the refusal at 2510-2511 comes
    *after* every lower-id snapshot, the old base among them, has been queued with `flags &
    !PIN_REPLICATION` (2513-2515). The old base is then unpinned for `delete_snapshot` at once
    (2470, 2489-2495) and durably with the next CP that prepares its metadata (5121-5128,
    5766-5781), and the error is the same
    `NoSuchSnapshot` as for an unknown id (2529-2533) or another volume's (2534-2535). A sender
    that gets it and wants to keep the old base must re-pin it before anything else, takes a
    fresh target either way, and must not read the error as "lost to a rewind" (ids change hands
    only across a boot, 1908-1913, rule 4, and trap 7.1's uuid lookup never yields a stale id).
    Since the receiver already committed the target, a delta from the old base needs a protocol
    decision (its `base` is behind the receiver's applied CP, `Cursor::begin` cursor.rs:350-355);
    without one the next delta is a full resync (4.8 step 5). The daemon's `snapshot_replication_base`
    (pool.rs:1149-1160) forwards the call and returns on the error before its `sync_now`
    (1152-1155), so the queued unpin lands with whatever CP comes next.
20. **A dirty boot can hold the replica by nvfail, and then the receiver can neither write nor
    snapshot it.** A dirty boot whose arena did not re-attach whole, that rewound, that found the
    arena lost or that dropped orphaned blocks holds every volume of the write-active set
    (1866-1885: `hold` at 1870, `NvCause::TornCp` for a rewind, else `DirtyBoot`), a replica that
    was being written at the crash included; `Event::Booted { nvfail, .. }` lists them (the
    daemon logs ", {held} volume(s) held by nvfail") and `volume(vol).nvfail` (`VolumeStatus`
    867, field 878) shows the cause. From then on every write and punch answers
    `Status::NvFailed` (3418-3421) and `create_snapshot` refuses with `EngineError::NvFailed`
    (2340-2342), until `nvfail_clear(vol)` (2251-2267) has its CP commit
    (`Event::NvfailCleared`, 5931-5940; the clear bumps `rollback_gen`). The hold exists because
    acknowledged writes may be gone (the comment 1866-1868: only a whole re-attached arena with
    no rewind keeps "every acknowledged write ... in hand"), so a receiver resuming a partial
    delta on a held replica must clear the hold **and** restart the delta from `DeltaBegin`
    rather than trust the chunks its cursor marked landed (their acknowledged writes may be the
    lost ones). The last committed `to` snapshot survives unless the boot rewound below its CP
    (`TornCp`; trap 7.3: then the next `DeltaBegin` meets `BaseMismatch`). The 4.9 recipe
    therefore checks `volume(vol).nvfail` before any chunk.
21. **An abandoned delta's landed chunks stay under the next delta from the same base, and a
    source rollback in between leaves them there for good.** The sender may abandon a target (4.8
    step 3: unpin, `delete_snapshot`) and send the next delta from the same base; the receiver's
    `Cursor::begin` then answers `Replaced` (another `cp_seq`, cursor.rs:374) or `Discarded` (the
    same `cp_seq` with another nonce, 373), which replace only the landing's bookkeeping
    (377-381): the replica's live map keeps every chunk the abandoned delta landed. That is right
    only if every such LBA is born after the base again in the next target, so that the next
    delta overwrites it. Writes and punches keep that true (a later one has a later birth, rule 2)
    and a shrink is replayed by its `Resize` event (7.9); a rollback of the source does not. Once
    the abandoned target's deletion is requested, `rollback_volume` no longer counts it as newer
    (the count skips `snap_deletes`, 2641-2649), so the source volume can be rolled back to the
    base or to any snapshot between the base and that target; through the daemon,
    `snapshot_rollback` leaves `deleting` snapshots out of its newer list (pool.rs:1036-1039)
    and, with `--delete-newer`, deletes every newer one first, all or nothing (E39 R1,
    docs/07:1495-1498: "a refusal leaves the history whole"): one newer snapshot with any pin bit
    refuses the whole call with "nothing was deleted" (`newer.iter().find(|s| s.flags != 0)`,
    1056-1066); with none pinned it deletes them all (1068-1072) and waits for their
    `SnapshotDeleted` (1073-1081). The test reads the committed `flags` (2907), so a pin change
    still pending makes the engine's `delete_snapshot` refuse at the `?` of 1071 after the lower
    ids were already queued, which R1's wording does not allow for. It names its rollback target
    by catalogue name only (1021 and 616-626), so a nameless base is reached only through the
    engine. The rollback installs that snapshot's root and index (4815-4831) and
    releases everything born after it, so in the next target the LBAs the abandoned delta wrote
    carry that snapshot's entries, and wherever those are the base's own (born at or below
    `base_cp`) or unmapped, `snapshot_diff(next_target, Some(base))` (2958-2991: `birth_cp >
    base_cp`, unmapped entries skipped) omits them: the replica keeps blocks the source no longer
    has, and nothing on either side notices. A rewind of the source below the abandoned or
    in-flight target's CP (7.3) does the same. 7.10 covers only a resume of the same delta, and
    4.9 never rolls the replica back before a delta that is not a resume. Two fixes (the lead's
    choice, 8.13): (a) the
    receiver rolls the replica back to `from` before it lands a delta that does not resume a
    partial landed since `from` (`Replaced`, `Discarded`, or `Started` after `Cursor::abort`,
    423-425, dropped one, so it needs a durable "landed since `from`" mark of its own): offline
    and quiet (`check_vol_op` 2584-2600), no newer snapshot (a late `to` that landed is deleted
    first, else `NewerSnapshots`, 2650-2652; a waiting `to` request answers `VolumeBusy`,
    2653-2655), `rollback_volume(replica_vol, from_id)` (2627), wait for `VolumeRolledBack` with a
    timeout (only a fold carries the rollback; on a fold-damaged or fold-blocked replica pool none
    comes and the replica stays offline, since `set_volume_online(true)` answers `VolumeBusy`
    while the op waits, 2571: trap 7.23), then online again; or (b) the sender sends a full resync
    instead of an increment when the source
    volume's `rollback_gen` (`VolumeStatus.rollback_gen` 876, durable in the volume row,
    otap-format superblock.rs:235, restored at boot 1774) moved since it began the abandoned delta
    (a rollback moves it, 5818; so does an nvfail clear, 2263 and 5934, which costs a needless
    resync after a clear that lost nothing), and whenever a boot lost its target in flight.
22. **A snapshot that boots undamaged can still hold a map page that does not verify, and
    deleting it wedges the CPs.** The boot checks a snapshot's root page and index only
    (`load_snapshot` boot.rs:554-572). The regions it returns (568-574: the root page, then
    `pinned.regions()`, which names the index and every map page from the index entries without
    reading a page, otap-map lib.rs:1853-1862), appended to the walk's at 503, are bookkeeping,
    not verification: for the other verified slots they serve segment protection (308-313), and
    for the selected slot `load_cp` takes them (593-594) into the metadata liveness count
    (698-703) and the metadata append point (736-742). So
    `SnapshotInfo.damaged` (2906) says nothing about the map pages.
    `snapshot_read` and `snapshot_diff` load them lazily (`pinned_get` otap-map lib.rs:1709 and
    `pinned_diff` 1725 -> `load_page` -> `MapPage::read`, 246-260) and answer
    `Map(MapError::Format(..))` for a page that fails verification, `Map(MapError::Io { .. })` for
    one the device fails to read (`BackstoreReader`, engine.rs:1353-1360; `From` 801). A page that
    failed is not cached (260), so every call that needs it fails again; `snapshot_diff` is all or
    nothing, so it fails every diff that reads the page, that is every one whose `base_cp` is
    below the page's `max_birth` (1722; a full diff always), of that target and of every newer
    snapshot that still shares the page. The deletion of such a snapshot reads the same pages: its
    own walk (`pinned_diff(.., older, ..)` 4954-4956, `older` the next older snapshot's birth)
    ends in a `?` that fails `prepare_meta` (passed through at 4729-4736). (A page the device
    fails to read, `Map(MapError::Io { .. })`, passes the same `?` and the neighbour lookups'
    `Err(err) => return Err(err.into())`, 4974 and 4993, and since `step_staging`'s guard takes
    only `Format`, 4453, it takes the plain CP's path below in a fold too.) In a fold,
    `step_staging` then sets `fold_damaged` (4453-4459) and fails the job's snapshot requests,
    whoever made them (`fail_fold_requests` 4455), after which every `create_snapshot` of the pool
    answers `MapDamaged` (2370-2371) until a reboot and the CPs that follow are plain, failing as
    the next sentence says; in a plain CP (`folds()` is false,
    1039-1041) the error leaves `tick` (`other => Err(other)`, 4461), `advance_cp` drops the job
    with its generation still frozen (4210-4213), no `fail_cp` hands its data segments back
    (4275-4292; `start_job` clears the list, 4193), and the next CP fails the same way: while the
    deletion waits no CP but a cleaning one (which defers deletions, 4912-4917) commits, on any
    volume, and the daemon only logs the failed ticks (daemon.rs:115-119; an operation waiting in
    `wait_for` returns the error, pool.rs:483). This is the wedge Step 4 fixed for live pages
    (review_sim_tests.rs:316-351), still open for a deleted snapshot's own pages. `snap_deletes`
    is memory only, so a reboot ends it and the snapshot stays. Every deletion is exposed the same
    way, the old base of 4.8 step 5 included, and nothing public tells beforehand (5.1 has the
    engine-side fix). A sender whose target answers
    `Map(..)` must abort the delta, keep the target (never `delete_snapshot` it, and skip it in
    4.8 step 5's sweep), report it, and take a new target (which shares the page, and fails the
    same way, unless the live map rewrote it since).
23. **A volume operation needs a fold too, and can wait forever.** `rollback_volume`,
    `resize_volume` (a grow as well as a shrink) and `delete_volume` only queue a `VolOp` on the
    volume and call `request_fold` (2657-2659, 2687-2689, 2711-2713). A CP carries the op only
    when it folds and relocates nothing (`ops_here = fold && job.clean.is_empty()`, 4797-4798),
    and `should_fold` folds for a waiting op (`ops_here` 4670, 4676) only past its two gates,
    `fold_damaged` (4655-4657) and `fold_blocked_at` (4660-4665). None of the three calls looks at
    either flag (2627-2661, 2667-2691, 2697-2715), so each answers `Ok` when no fold can come.
    Under `fold_damaged` (a fold met a map page that does not verify, 4453-4459) `request_fold`
    refuses with only an `Event::Internal` and asks for no CP (2225-2230), no CP folds
    (4655-4657), and the op never lands; a reboot drops it, since `VolRt::op` is memory only
    (1298) and the boot rebuilds each `VolRt` from its volume row without one (1770-1785), while
    the `offline` bit survives (1775). Under `fold_blocked_at` (a fold failed for space, 4445) the
    op waits silently like a parked snapshot request (rule 14), until `free_segments()` passes the
    mark where `should_fold` reads it and a CP runs after that: on a full or tight pool
    `maybe_pump` counts a waiting op as work and starts such CPs (2152-2167, once no generation
    is frozen, 2155, and at most three per free, 2159); on a pool that is
    neither, someone must call `request_cp` (2122). Meanwhile the waiting op also stops the cleaner
    (`plan_clean` returns at 4024-4026; `maybe_pump`'s `can_clean` needs `!ops_pending`,
    2167-2169), one of the ways the space it waits for could come back. While the op waits, the
    volume answers `VolumeBusy` to `create_snapshot` (2343), `delete_snapshot` (2474-2479),
    `set_volume_online(vol, true)` (2571), `resize_volume` (2675-2677) and, through
    `check_vol_op` (2593-2595), `rollback_volume`, a shrink and `delete_volume`. So the three
    recipes that take a replica offline and then wait, 4.9 step 1's shrink (for `VolumeResized`)
    and `Delete` (for `VolumeDeleted`) and 7.21 fix (a)'s rollback (for `VolumeRolledBack`), leave
    the replica offline on a fold-damaged or fold-blocked replica pool: it cannot come back online
    while the op waits (2571), and the receiver's own writes answer `Offline` (3414-3416). A
    waiting grow keeps the volume online but blocks its `to` snapshot (2343) and every write
    beyond the old end (`OutOfRange`, 3430-3433; 7.14). 4.8 step 5's retry of the old base's
    deletion after `VolumeResized` or `VolumeRolledBack` can wait forever the same way. **Bound
    every such wait with a timeout**, as for snapshot requests (4.8 step 2), calling `request_cp()`
    once more before giving up. **A timeout withdraws nothing:** no public call removes a `VolOp`
    (`op` is set only at 2657, 2687 and 2711, and cleared only by the `finalize` of the CP that
    carried it, 5803-5807, or by the volume's own deletion, `VolRt::default()` 5847), so after
    the timeout the op still blocks all of the above and still lands at the first fold that takes
    it (under `fold_blocked_at`, perhaps long after). `volume(vol).op_pending` (`VolumeStatus`
    889, filled at 3252) tells that it still waits, not whether a fold can come (`fold_damaged`
    and `fold_blocked_at` are private, 5.1). The daemon's waits time out the same way (`wait_for`
    504-511) and return with the op pending: `snapshot_rollback` then leaves the volume offline
    (taken down at 1083-1089, brought back only after `VolumeRolledBack`, 1102-1112). Volume
    creation and an nvfail clear need no fold: `create_volume` and `nvfail_clear` ask only for a
    CP (2291, 2265), every CP builds `new_volumes` (4776-4782), and a clear rides any CP's slot
    (`build_slot` 5647-5653, 5677-5683; `finalize` 5931-5940).
24. **A `Committed` the sender never records costs a full resync unless both sides read the
    cursor.** The receiver's `Committed` can be lost in a disconnect, and the sender can crash
    after the receiver's cursor commit but before 4.8 step 5 persists the target as the base.
    Either way the sender still holds the target as in flight and the old base as its base, while
    the receiver's `Cursor::commit` has moved `applied_cp_seq` to the target's `cp_seq` and
    `applied_epoch` to its epoch and dropped the landing (otap-stream cursor.rs:417-419).
    Retrying the same target (4.8 step 3) then re-sends a `DeltaBegin` whose `base` is the old
    base's CP: `Cursor::begin` refuses it with `BaseMismatch` (350-355) before the resume test
    (363-372) is reached, and 4.9 step 5 (a)'s re-landing has no landing to land into or commit
    (409-416), so the delta can neither resume nor be recognised, and `BaseMismatch` sends the
    pair to a full resync (7.3, 5.1). The cursor itself is the evidence: the receiver sends its
    whole `Cursor` in `HelloAck` (frame.rs:211-214), and the design's sender reads it on connecting,
    before it chooses the delta (docs/05-architecture-brainstorm-03-record.md:854). So (1) at every
    (re)connect the sender compares `HelloAck.cursor`'s `applied_cp_seq` and `applied_epoch` with
    its persisted target's `cp_seq` and `owner_epoch` and, on a match, runs 4.8 step 5 as if
    `Committed` had come (4.8 step 3); and (2) a receiver that gets a `DeltaBegin` whose `cp_seq`
    and `owner_epoch` are its applied CP and epoch, with every tagged volume's `to` held on the
    mapped volume, answers `Committed` again without calling `Cursor::begin` (4.9 step 5 (a)). The
    match is on CP and epoch only, since the cursor keeps no nonce and no snapshot for the applied
    CP (`Cursor` cursor.rs:307-316, `CursorBody` 538-559): the sender must also find its persisted
    target still listed by uuid (4.8 step 1; a rewind below the target's CP loses it and reuses its
    CP numbers, rule 4, 7.3), and the receiver must find the delta's `to`. That guard excludes only
    a target the rewind lost, so the match is still no proof: a target taken after a sender rewind
    below the receiver's applied CP can get that very `cp_seq` again (2042, 3972), is listed, and
    matches, and the sender promotes a target the receiver never received. Nothing diverges: 4.9
    step 5 (b)'s `from` test answers the next `Increment` `BaseMismatch`, and the full resync that
    follows is the one such a rewind calls for anyway (7.3).

## 8. Open questions for the lead

1. **Where does the receiver's `to` snapshot get its name and uuid?** The sender's `SnapTag` has
   both; the engine offers uuid-without-name (`create_snapshot`) or name-with-derived-uuid
   (`create_named_snapshot`). Make `create_snapshot_named` public, or let replica snapshots be
   nameless?
2. **How is a transfer in flight protected?** Pin the target with `PIN_REPLICATION` at creation
   (two pinned snapshots per volume during a delta is then the normal state, 7.12, so the base
   is resolved by the uuid stored at the last `Committed`, never by the bit, 4.8 step 1;
   `set_replication_base` afterwards clears the old one, and an aborted target must be retried
   or unpinned and deleted, 4.8 step 3), or add a non-durable in-flight guard in the engine
   (registered again at every sender start, since a transfer resumes across a restart, 5.1)? And
   should a delta in flight survive its base's deletion (a `snapshot_diff_since(snap_id, base_cp)` that only
   recomputes the delta of a target taken while the base existed; for a new target it is unsafe,
   5.1)? And who owns `PIN_REPLICATION` once a sender runs: the sender, whose re-pin of its stored
   base at every delta undoes an operator's move of the bit (4.8 step 1), or the daemon's
   `snapshot_replication_base` (pool.rs:1149-1160), which clears it with `None` or moves it to a
   named snapshot?
3. **Should `SnapshotInfo` expose the pinned `lba_count`** (and should `SnapshotCreated` carry the
   uuid)? Both are one-field additions. Without the first, the sender finds a snapshot's size by
   probing `snapshot_read`: a one-block read answers `Io(OutOfRange)` exactly when `lba >=
   pinned.root.lba_count` (2935-2942, checked before any block is read), so a binary search over
   one-block reads finds it, each in-range probe costing one block read. The field is a
   convenience, not a precondition for `Resize` events (and neither it nor the probe sees a shrink
   followed by a grow between base and target, 7.9).
4. **What does "committing the received delta as an atomic CP on the target pool"
   (docs/10-phase-c-step-briefs.md:623) mean against an engine that commits on its own
   triggers?** The replica's live map takes the writes CP by CP; the `to` snapshot is
   the only atomic marker. Is "replica consistent = its last `to` snapshot; the live map may be
   ahead" acceptable, or must the receiver stage the delta outside the live map?
5. **Replica visibility:** keep the replica online in the engine and add a durable replica mark
   that the daemon's exposure sites honour (today it would be exposed at every daemon start and
   after create, online and rollback, 4.9 step 1), or add a replica/offline-to-clients flag that
   admits only the receiver's writes (a `FrontDoor` trait change touching the doors that vouch or
   forward, 5.1), or keep it offline at rest and online in the engine only while a delta lands (no
   code change; `io write`/`io punch` and a daemon restart after a CP recorded it online reach it
   in that window, 4.9 step 1)? The
   channel door's writes are not the receiver's alone: `otapctl io write` and `io punch`
   (pool.rs:1225, 1245) use the same channel with ids of the same form (daemon
   frontdoor.rs:123-136), so either the receiver gets its own door or a submitter mark, or
   `io_write`/`io_punch` honour the replica mark too (with the first option they must anyway).
6. **Loopback tests on one pool are impossible with equal uuids** (7.2): is the Windows loopback
   test two `Sim` engines in one process (two file backstores), and does the file sink replay
   into a second pool?
7. **Which origin does a replica's `to` snapshot take** (`Transfer`, counted in the 16 reserved
   slots, or `User`/`Scheduled` mirroring the source's tag)? The retention of `SnapTag` (`retention_s`)
   has no engine counterpart; who deletes expired tags on the replica? As written (one `Transfer`
   `to` per delta, none deleted) the receiver stops at the 17th delta with `SnapshotLimit`
   (4.9 step 3 pins the newest `to` and deletes the superseded one to avoid that).
8. **Scope B's table format:** append-only `SnapRecord` deltas in the CP log pages (a new
   `CpLogRec` kind; note that a fold writes no CP log pages today, `if !input.fold`, otap-map
   lib.rs:1091-1096, and every new snapshot is born in a fold, rule 1, so that variant needs
   folds to write them or a log region of its own) versus a CoW table of pages; and whether
   `SnapRecord::deadlist_pba`
   stays reserved (E36 rejected deadlists) so the record layout is untouched.
9. **How does a sender learn that its snapshot request is parked?** Today only by a timeout:
   `CpFailed { reason: "no free segment" }` comes only from an attempt that fails while the
   request already waits (4.8 step 2); a request made while `fold_blocked_at` holds, or left behind by a fold
   that failed on a damaged page, gets no event at all; `is_pool_full()` is cleared by any later
   CP (5966); and `fold_blocked_at` and `fold_damaged` are private. Expose them (and the pending
   request count) in `PoolStatus`, refuse `create_snapshot` while the fold is blocked, or fail
   the requests with `SnapshotFailed` on space as the doc comment 419-421 already says (5.1:
   that last one alone reaches only the requests the failing CP carries; answering every
   request in `snap_requests` on a space failure reaches the ones waiting at the failure, and a
   request made after it is reached only by a call-time refusal whose test allows for the CP's
   own data segments, since right after the failure the free count is above `fold_blocked_at` by
   those of the failed attempt, or exactly by answering the eligible requests where `should_fold`
   keeps a CP plain because of `fold_blocked_at`, 4660-4665)?
10. **Volume identity across the pipe.** Every `vol` in a delta is the sender's index, the
    replica's index is whatever the receiver pool had free, and neither engine stores a volume
    uuid or a replica mark (4.9 step 1). Who owns the durable map from the sender's volume to the
    replica's (the receiver's cursor store, a catalogue convention, a new format field), what
    keys it (the `VolEvent::Create` uuid, which the sender must mint, or the name), and how does a
    delta over several volumes name each volume's `from` and `to` (one `VolEvent::SnapTag` per
    volume for `to`; nothing carries a per-volume `from`, 4.9 steps 3 and 5)?
11. **How is a delta with no base framed?** The initial transfer and every full resync (4.8
    steps 1 and 5, 5.1, 7.3, 7.21) cannot be an `Increment` (a nil `from` is refused,
    otap-stream frame.rs:930-933), so they travel as `Steady`, whose mode carries no `to`
    (1047-1051) and which the receiver cannot tell from a CP write set; the header `base` must be
    the receiver's applied CP (cursor.rs:350-355) and the target's `cp_seq` above it (356-361).
    Does the protocol get a mode or a header flag for "whole image", or does the sender say it
    inside the delta (a `Holes` frame over the volume, or over the ranges the full diff does not
    name, ahead of the chunks; or `VolEvent::Delete` then `Create`, 4.8 step 1), and which cost
    does the lead accept (every punched block becomes a hole entry in the replica's map,
    4768-4772; a recreate first needs the replica's snapshots deleted, 4.9 step 1)?
12. **What does a delta do with a source block that fails its CRC?** `snapshot_read` answers
    `Err(Corrupt)` for it (3725-3726), and today nothing can carry it as corrupt (a chunk
    descriptor is `Data` or `Hole`, otap-format stripe.rs:85-95) or land it so (the receiver's
    front door CRCs whatever it copies in, 3517-3524), so its bytes would land as a valid block
    unless the receiver checks each block against its descriptor (4.9 step 2). Abort the delta
    (no target of that volume ships until the source rewrites the block), punch or skip the block
    with a report (the replica then reads zeros or older data there, with no error), or add a
    "corrupt at the source"
    block kind or flag (a format change: values 2 and 3 of `BlockKind` are reserved) and a
    receiver engine call that makes the LBA read `Status::Corrupt` (7.8)?
13. **A delta that does not resume a partial landed since `from`** (7.21; 4.9 step 5 (c)): does
    the receiver roll the replica back to `from` before landing it (fix (a): it needs a durable
    "landed since `from`" mark, and under trap 7.23 the replica stays offline), or does the sender
    send a full resync instead of an increment when the source volume's `rollback_gen` moved since
    it began the abandoned delta, or a boot lost its target in flight (fix (b))?
