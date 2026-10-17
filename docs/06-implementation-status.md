# OpenTAP implementation status and handoff

Living document. Last updated 2026-10-16, after Phase C Step 6 and its
review. Update it at the end of every step and whenever section 7 ("Next")
changes. Section 1 is what a new session reads first; the rest is the
detail behind it.

## 1. Where we are, in one screen

- **Step 6 is done** (2026-10-16): the ublk `USER_RECOVERY` re-attach and
  the stale-device reaper (`otapctl device list|reap`), format 2.4 with an
  incremental snapshot table and catalogue, the pool verification
  (`otapctl pool verify`), replication between two pools over a simulated
  network and over TCP (`otapctl replicate`), and the WSL 2 scripts. It was
  built by five Opus 5.5 workers in sequence under the owner's
  thin-orchestrator mode (`09` section 1): `217dad3`, `8e34b4a`, `4f34e17`,
  `1637340`, and the review `758adb8`. What each decided is in
  `step6/notes/`, the decisions in `07` (E40 to E60), the account in the
  2026-10-16 entry of `08`.
- Step 6 closed with one bounded adversarial review that reproduced every
  finding with a failing test before fixing it: eleven data-loss,
  misdirection or exposure defects (five in the replication sink, the
  snapshot table that lost records and then their blocks, a relocated
  block that already failed its CRC, a lifted quarantine, a damaged arena
  word of a one-block write, a ublk device re-attached to the wrong
  volume, a deleted mirror's record), three wedges and six robustness
  defects, all fixed; seventeen cited items recorded as not reproduced
  (`reviews/2026-10-16-step6-qa-adversarial.md`, E60).
- The design is complete through round 3: `05-architecture-brainstorm-03.md`
  holds rulings R25 to R64, the trait contracts (Part 7), the Phase C plan
  (Part 8) and the fifteen owner decisions (Part 9).
- Implementation started on 2026-10-13 in `E:\Coding\OpenTAP`. Steps 1 to 6
  of the implementation briefs are done and every gate in section 5 is
  green: the on-disk format (now 2.4) and the stream frame header v1
  (`otap-format`), the replication stream v1 (`otap-stream`), the kernel
  boundary (`otap-sys`), the three fuzz targets, the engine core (the
  write-cache arena `otap-arena`, the dirty index `otap-index`, the
  consistency-point engine `otap-cp`, the simulator `otap-sim`), since
  Step 4 the persistent map with folds and snapshots (`otap-map`), discards
  and writes of zeros as holes, geometry discovery (`otap-geom`), the real
  backstores (`otap-backstore`: io_uring on Linux, a file anywhere) and
  the ublk front door (`otap-ublk`), since Step 5 snapshot deletion with
  pins and the replication base, rollback, resize, deletion and offline
  volumes, the segment cleaner with its reserve, admission rule and pacing,
  the name catalogue, and the daemon `dbladed` with its control protocol
  (`otap-control`) and its client `otapctl` (`otap-daemon`, `otap-ctl`),
  and since Step 6 the device recovery and reaper, the incremental tables,
  the verification and replication.
- The owner closed E19 and recorded D1, D2, D8, D9 and D10 with the Step 4
  brief (`07-engineering-decisions.md`, E21): flush-opaque backstores are
  trusted under `TrustBarrierWithNvfail` (the default) with a structured
  warning, and refused only under the strict policy.
- The engine, the daemon and the CLI have run only on the simulator's
  device and on a plain file (the whole lifecycle, replication between two
  daemons over loopback TCP included, runs on the Windows host). The Linux
  halves (`otap-sys::block`, the ublk device module with Step 6's
  `LinuxHost` and re-attach, `UringBackstore`, `Sysfs`, the daemon's device
  and Unix-socket code) are type-checked for `x86_64-unknown-linux-gnu`
  and have never executed. The first Linux host is WSL 2 on the
  development PC: the Windows features are on, the owner's BIOS "SVM Mode"
  switch and a reboot are pending, and `scripts/wsl2-prep.sh` and
  `scripts/linux-smoke.sh` are ready (section 7).
- Step 4 closed with an independent adversarial review that found 24
  reproducible defects, five of them in the boot (E29 to E33); Step 5's
  review found twelve, four of them losing or exposing data (E39). All are
  fixed and pinned.
- The tree is under version control: branch `main` of the GitHub
  repository `github.com/prestonbalthaus-lgtm/OpenTAP`, public since
  2026-10-14 with the design records in it by the owner's decision; one
  commit per reviewed change with the gates green, pushed through the
  GitHub CLI.
- Licensed under Apache-2.0 since 2026-10-14 (`LICENSE`; every manifest
  carries the field). Every package in both lockfiles declares a
  permissive license; Step 6 added no dependency.
- Owner decisions D3 to D7 and D11 to D15 of Part 9 have no recorded
  answers; the code depends on them only through the reservations listed
  in section 6. The Step 5 brief's rulings labelled D3, D5, D6 and D14 are
  S5-1 to S5-4 in E34.
- The development PC runs Windows. The first Linux run will find things in
  the Linux halves; budget for it.

## 2. Document map

| File | What it is | Status |
|---|---|---|
| `00-project-brief.md` | the owner's first charter (2026-10-12) | superseded by 02 and 04 |
| `01-architecture-brainstorm.md`, `-record.md` | round 1 synthesis and its 46-agent record | baseline; three of the brief's premises did not survive it |
| `02-project-brief-unified.md` | charter revision 2 (multi-fabric, multi-protocol) | superseded by 04 where they conflict |
| `03-architecture-brainstorm-02.md`, `-record.md` | round 2 synthesis: trait contracts, Phase 1 roadmap on Debian | standing where the revision-3 charter did not re-open it |
| `04-project-brief-async-svm.md` | charter revision 3: async replication, SVM-first, single-node-first | current charter |
| `05-architecture-brainstorm-03.md`, `-record.md` | round 3 synthesis: rulings R25 to R64, Part 7 contracts, Part 8 Phase C plan, Part 9 owner decisions | current design |
| `06-implementation-status.md` | this file: what exists, what the gates say, what is next | living |
| `07-engineering-decisions.md` | the implementation decision log: where the code departs from the design or a brief, and why | living (E01 to E60) |
| `08-engineering-log.md` | dated record of what happened: steps, reviews, defects, gate results, lessons | living, append only |
| `09-working-agreements.md` | how the work is done: roles, definition of done, code and test rules, build etiquette, documentation rules | living |
| `10-phase-c-step-briefs.md` | the owner's step briefs, verbatim, each with a note on how it was read | append per step |
| `../README.md` | the public face of the repository: the goal, how the system works, status, where to start | kept current |
| `../BUILDING.md` | build, test and layout guide, including the Windows host notes | kept current with the tree |
| `reviews/` | the worker reports behind the Step 1, 2, 4, 5 and 6 reviews, verbatim, with an index | archive |
| `step6/` | Step 6 working notes: the handoff, the lead's preliminary positions, thirteen maps of the code, and each worker's notes (`notes/`) | record; Step 6 done 2026-10-16 |
| `../crates/otap-format/src/lib.rs` | the layout specification and format rulings F1 to F21; the code is the authority | format 2.4, stream header 1.0 |
| `../scripts/linux-smoke.sh` | the first-run smoke test for a Linux host (the README has the steps) | written and extended in Step 6, not yet run |
| `../scripts/wsl2-prep.sh` | the WSL 2 probe, package install and kernel build (ublk as a module) | written in Step 6, not yet run |

## 3. The workspace as built

One Cargo workspace (`Cargo.toml` at the root), edition 2021, `rust-version`
1.89, built and tested with stable 1.99. `fuzz/` is excluded from the
workspace because it needs nightly and links libFuzzer.

| Crate | Role | `unsafe` | Modules | Tests |
|---|---|---|---|---|
| `crates/otap-format` 0.1.0 (format 2.4) | on-disk format and the frame header v1: zerocopy views, CRC-32C sealing and verification, the superblock ring, stripe summaries, map pages, the records, the CP log directory, the snapshot root page, the table regions, the recorded geometry, the name catalogue, the commit's stripe list, and since 2.4 the table log and head pages; no I/O, no policy | forbidden | `constants`, `types`, `checksum`, `superblock`, `stripe`, `map`, `stream`, `records`, `catalog`, `tables` | `format_tests` 53 (+1 ignored vector writer), `records_tests` 15, `catalog_tests` 9, `adversarial_tests` 49, `step6b_catalogue_format_tests` 10, 1 doctest |
| `crates/otap-stream` 0.1.0 | stream v1: typed frames, the codec, the landing cursor and its page, credit windows; no sockets | forbidden | `frame`, `codec`, `cursor`, `flow` | `stream_tests` 13, `adversarial_stream_tests` 62 |
| `crates/otap-sys` 0.1.0 | the kernel boundary: hugetlbfs arenas, the ublk 6.12 UAPI with the control ring (waited for, or begun and polled) and the queue mapping, `DEL_DEV_ASYNC`, io_uring buffer registration and cloning, the block-device ring, the capability probe (`USER_RECOVERY_REISSUE` required); Linux implementations behind `cfg`, portable fallbacks | fourteen blocks, all Linux-only, each with a `// SAFETY:` proof | `hugetlb`, `ublk`, `uring`, `block`, `probe` | `sys_tests` 33, `step6b_recovery_sys_tests` 5, 1 doctest |
| `crates/otap-arena` 0.1.0 (layout 1.2) | the write-cache arena over `otap-sys` memory: an immutable sealed header, a descriptor table whose every transition is one sealed-word store, 4 KiB slots of data or hole records; runs with lap counters; `Free`/`Filled`/`Placed`/`Committed`; re-attach after `kill -9` with data and descriptor CRCs verified and damage reported, deferred until the boot commit lands, judging a stale slot by its neighbours and its implied position (`Fills`), a survey under the recorded geometry | forbidden | one module | `arena_tests` 12, `adversarial_arena_tests` 12, `step6a_arena_tests` 12, `review_step6_arena_tests` 3 |
| `crates/otap-index` 0.1.0 | per-volume dirty extents of data and holes (split, trim, coalesce; later insert wins; stale sequences refused), generations, the cut over `arc-swap` with a grace wait, the frozen queue | forbidden | one module | `index_tests` 10, `adversarial_index_tests` 13 |
| `crates/otap-map` 0.1.0 | per-volume roots, page indexes and overlays; delta regions and the directory per CP; copy-on-write folds; replay; snapshot pins and the birth walk (strict and tolerant); rollback, resize and deletion at a fold with the released entries; the rewrite of a pinned tree; a tolerant walk; a pba-keyed page cache; no I/O | forbidden | one module | `map_tests` 8 (a model with proptest), `review_map_tests` 10, `lifecycle_map_tests` 6 |
| `crates/otap-cp` 0.1.0 | the traits (the four of Step 3, and `Link`, `Dialer`, `LinkListener`, `SinkStore` for replication) with the flush and discard policies; the engine: write, read and punch paths, intents, the CP state machine, folds, snapshots and their deletion and quarantine, pins and the replication base, rollback, resize, deletion and offline volumes, the catalogue and the snapshot table as chains (format 2.4), the reserve and the admission rule, pacing, the cleaner's driver and relocation stripes, the full-pool pump, liveness and deferred frees, the liveness audit, the verification job, the replica's apply paths and the source's delta walk, boot with replay and rewind, nvfail, metadata recycling, clean shutdown; the replication sessions (`repl`: the replicator, source and sink jobs, the link framing, the sink's record) | forbidden | `traits`, `layout`, `segments`, `meta`, `boot`, `engine` (with `engine::replica`), `cleaner`, `tables`, `verify`, `repl` | 2 unit tests, `step6c_transport_wire_tests` 13; otherwise tested through `otap-sim`, `otap-backstore`, `otap-ublk` and `otap-daemon` |
| `crates/otap-geom` 0.1.0 | sysfs facts behind a source trait with a mock, device classification, the stripe and segment plan, the media profile, overrides | forbidden | one module | `geom_tests` 11, `review_geom_tests` 10 |
| `crates/otap-backstore` 0.1.0 | `UringBackstore` (Linux, over `otap_sys::block`) and `FileBackstore` (anywhere) | forbidden | `file`, `uring` | `file_backstore_tests` 2 (the engine over a file), `review_backstore_tests` 5 |
| `crates/otap-ublk` 0.1.0 | the `Transport` trait, the `UblkFrontDoor` state machine, the mock transport; the host-independent device control (`DeviceSpec`, `UblkHost`, `HostDevice`, the stamp), the recovery (`Recovery`, the standings, the reap check), `start_while_serving`, `MockKernel` (the 6.12 driver's rules in memory); on Linux the device lifecycle with the re-attach, `LinuxHost`, the real transport and a runner | forbidden | `frontdoor`, `mock`, `mock_kernel`, `spec`, `host`, `recovery`, `start`, `device` (Linux) | `frontdoor_tests` 24, `frontdoor_proptests` 1, `engine_tests` 7, `review_ublk_tests` 12, `step6a_ublk_tests` 7, `step6b_recovery_ublk_tests` 15, `review_step6_ublk_tests` 2 |
| `crates/otap-sim` 0.1.0 | fake device (pending, cached, durable; seeded completion order; sector tears; discards; a protected or unprotected controller cache), fake client, virtual clock, loopback net, `SimNet` (byte pipes with delays and seeded drops, cuts, corruption, refused dials) and `MemStore`, the checker model, the harness (snapshots, deletions, rollbacks, resizes, volume deletions, offline, cleaning, the liveness audit and the verification) | forbidden | `backstore`, `frontdoor`, `clock`, `net`, `simnet`, `memstore`, `model`, `harness`, `rng` | `sim_tests` 15, `adversarial_sim_tests` 43, `step4_sim_tests` 15, `review_sim_tests` 23, `step5_sim_tests` 15, `review_step5_sim_tests` 18, `step6a_sim_tests` 12, `step6b_catalogue_sim_tests` 16, `step6b_verify_sim_tests` 13, `step6c_replication_engine_tests` 5, `step6c_replication_sim_tests` 15, `step6c_transport_simnet_tests` 7, `review_step6_sim_tests` 6, `review_step6_replication_sim_tests` 6; seven random-walk property tests |
| `crates/otap-control` 0.1.0 (protocol 1) | the control protocol: requests, replies and errors as tagged JSON (every shape refuses unknown fields), length-prefixed frames bounded at 4 MiB, the endpoint syntax, a blocking client, the size and bound helpers shared by both ends; re-exports the format's block size and pin bits | forbidden | `protocol`, `wire`, `client`, `units` | 8 unit tests (every request and reply round-trips), `review_control_tests` 5, `step6b_recovery_control_tests` 4, `step6b_verify_control_tests` 4, `step6c_replication_control_tests` 4, `review_step6_control_tests` 2 |
| `crates/otap-daemon` 0.1.0 | `dbladed`: the engine thread over `Box<dyn Backstore>` and a composite front door, the pool's operations waiting for their consistency points, the arena and device opening, the device recovery and its binding record, the replicator and `TcpReplicationTransport`, the superblock probe, the control server, the signal flag, the binary's command line | forbidden | `config`, `arena`, `backstore`, `bindings`, `clock`, `frontdoor`, `report`, `pool` (with `pool::devices`), `daemon`, `server`, `device`, `devices`, `replication` | `daemon_tests` 6, `review_daemon_tests` 9, `step6a_daemon_tests` 9, `step6b_recovery_daemon_tests` 15, `step6b_verify_daemon_tests` 3, `step6c_replication_daemon_tests` 2, `step6c_transport_tcp_tests` 11, `review_step6_recovery_daemon_tests` 5, `review_step6_replication_daemon_tests` 2 |
| `crates/otap-ctl` 0.1.0 | `otapctl`: the command line to a request, the client call, text and JSON rendering, the waits of `pool verify` and `replicate`, exit codes | forbidden | `args`, `render` | 5 unit tests, `ctl_tests` 2, `step6b_recovery_ctl_tests` 3, `step6b_verify_ctl_tests` 2, `step6c_replication_ctl_tests` 2, `review_step6_ctl_tests` 2 |
| `fuzz/` | cargo-fuzz targets `fuzz_superblock`, `fuzz_stripe_summary`, `fuzz_stream_frame` | none | own workspace, `libfuzzer-sys` 0.4 | type-checked only on this host |

Totals on 2026-10-16 after Step 6 and its review: 760 tests and 2 doctests,
all passing, 1 ignored (section 5).

### 3.1 otap-format: what is on disk

Every structure is `#[repr(C)]`, little-endian, padding-free, covered by a
CRC-32C, and `const`-asserted for size and offset. Magics: `OTSB` superblock,
`OTIN` intent slot, `OTST` stripe header, `OTSF` stream frame, `OTCL` CP log
page, `OTCD` CP log directory page, `OTSR` snapshot root page, and since 2.4
`OTTL` table log page and `OTTH` table head page.

| Structure | Bytes | Notes |
|---|---|---|
| `Superblock` | 4,096 | identity (`agg_id`, `owner_epoch`), counters (`cp_seq`, `cut_seq`, `cp_nonce`, `intent_seq_seen`, `opened_stamp`, `wall_ns`, `acked_bytes`), `run_state`, `slot`, `vol_count`, `write_active` and `nvfail` bitsets (1,024 volumes), roots (`freed_pending[2]`, `map_roots`, `seg_table`, `reloc_index`, `cp_log_head`, `snap_table`, `cluster_state`), `durable_log` header, `inbound[16]`, the open data segment (2.1), the geometry (2.2: stripe, segment, capacity), the catalogue and the stripe list roots (2.3), the CP of each table head (2.4: `snap_head_cp`, `catalog_head_cp`; `snap_table` and `catalog` name the heads), 2,316 reserved bytes |
| `VolTable` | 16,384 | `[VolState; 1024]`: per-volume flags (present, offline since 2.3), `acked_bytes`, `rollback_gen`, nvfail cause; covered by `Superblock::vol_table_crc` |
| `SuperblockSlot` | 20,480 | page plus table, written whole with FUA; four slots `A0 A1 B0 B1` ranked by `(owner_epoch, cp_seq, cut_seq)` |
| `IntentSlot` | 4,096 | two slots; `boot_write_active` unions the newer intent into the superblock's bitset |
| `StripeHdr` + `VolBitSet` + `[StripeDescriptor; nblocks]` | 64 + 128 + 16 n, padded to 4 KiB | the stripe summary; the stripe image is the chunk payload byte for byte |
| `MapEntry`, `PageIndexEntry` | 16 | `lseg:36 \| off:16 \| plen:12`, `birth_cp`, block CRC; 1,024 per 16 KiB page; a hole is an entry with a birth and no address |
| `MapRoot` | 64 | per-volume root with its index CRC and the CP it was checkpointed at |
| `StreamHeader` | 64 | the frame header, `hdr_crc` and `payload_crc` |
| `HoleRecord` | 16 | `vol`, `len`, `lba` |
| `SegEntry` | 32 | keyed by segment index: `phys`, geometry at birth, live bytes, `birth_cp`, `lap`, kind, state |
| `SnapRecord` | 64 | id, uuid, map root, deadlist, creation time, `birth_cp`, volume, kind, origin, pin flags; since 2.4 `SNAP_UNCERTAIN` (a record recovered from a damaged chain, quarantined) and, in a delta region only, `SNAP_TOMBSTONE` |
| `CpLogRec`, `CpLogPage` | 32, 4,096 | 126 records per page behind a 64-byte header naming aggregate, CP, nonce and page position |
| `CpLogDirPage` (2.1) | 4,096 | 252 `RootRef`s behind a 64-byte header: the delta regions since the fold and the pages written (F12; since 2.3 in any CP, F18) |
| `SnapRootPage` (2.1) | 4,096 | the pinned `MapRoot`, the snapshot's identity, the deadlist reference (F13) |
| `CatalogRec` (2.3) | 512 | a volume's or a snapshot's name: kind, name length, flags, volume, snapshot uuid, creation time, 255 bytes of name, its own CRC; the region in canonical order (F16) |
| `CpStripe` (2.3) | 16 | `(pseg, idx, kind)` of a data stripe the commit wrote, ingest or relocation; the list in ascending order (F17) |
| `TableLogPage` (2.4) | 4,096 | a 64-byte header (aggregate, CP and nonce, page position, table) and 63 snapshot records or 7 catalogue records, each with its own CRC; a delta region is whole pages of one CP, keys ascending, upserts whole and deletions as tombstones (F19) |
| `TableHeadPage` (2.4) | 4,096 | the base entry and up to 124 delta entries (`TableChainEntry`, 32 bytes: region, CP, records); byte 56 always zero, so a 2.3 reader refuses a snapshot head before it writes anything (F20); the replay and damage rules are F21 |

Golden vectors: `crates/otap-format/tests/vectors/v2.0/` (twelve images),
`v2.1/` and `v2.2/` (fourteen each), `v2.3/` (sixteen) and `v2.4/` (nineteen) hold byte-exact images with BLAKE3 manifests whose hashes
are pinned in `format_tests.rs`; every set is decoded on every run. Any
layout change is a minor bump plus a new vector directory (`BUILDING.md`, "Golden
vectors").

### 3.2 otap-stream: what is on the wire

- Twelve frame kinds (`Hello`, `HelloAck`, `DeltaBegin`, `VolEvent`, `Chunk`,
  `Holes`, `DeltaEnd`, `Landed`, `Committed`, `Abort`, `Ping`, `Pong`), each
  with a fixed little-endian body and, where needed, trailing records
  (`SnapTag`, `AckedBytes`, `ChunkRange`, a volume name of at most 255 bytes).
- `FrameMeta` is the header context a frame travels under: `rel`,
  `chunk_idx`, `cp_seq`, `base`, `cp_nonce`, `owner_epoch`.
- `Encoder` seals header and payload and refuses every frame and every
  context the peer's decoder would refuse, before it allocates. `Decoder`
  yields CRC-verified `RawFrame`s with zero-copy `Bytes` payloads; the 8 MiB
  payload bound is enforced at the header.
- `Cursor` is the receiver's resume state keyed by `(cp_seq, cp_nonce)`:
  a `ResumeBitmap` of landed chunks (at most 2^24 chunks), the wire form
  carried by `HelloAck` (at most 4,096 sparse ranges), and a 4 KiB
  `CursorPage` (`OTCU`, 500 ranges) persisted per pipe.
- `ReceiverWindow` and `SenderWindow` count credits of 4 KiB: a 64 MiB
  initial grant, a 2 GiB staging cap, so at most 524,288 credits.

### 3.3 otap-sys: what touches the kernel

- `hugetlb`: `HugeArena` over a hugetlbfs file (2 MiB or 1 GiB pages) or an
  anonymous mapping, locked and excluded from core dumps; an aligned heap
  buffer stands in off Linux.
- `ublk`: the 6.12 UAPI as data (ioctl encodings, feature bits, nine
  `repr(C)` structures with size asserts), `CtrlCommand` and `IoCommand`
  builders producing the 80-byte command area, the user-copy offset and its
  decoder, and on Linux the control and character devices, the control
  ring (`UblkControlRing::execute`) and the queue (`UblkQueue`: the mapped
  descriptors, fetch and commit pushes, the user-copy reads and writes).
- `uring`: the opcode table, `FixedBufferLayout` (1 GiB cuts), and on Linux
  `RegisteredBuffers`, buffer-table cloning and the user-copy read SQE.
- `block` (Step 4): `BlockRing`, a block device on io_uring: `O_DIRECT`,
  a page-aligned staging pool whose slots the kernel reads until the
  completion is reaped, writes with and without `RWF_DSYNC`, `fsync`
  barriers, `BLKDISCARD`, reads; drop waits for what is in flight.
- `probe`: `OpcodeSet`, `Allowlist` and the v0.1 requirement check
  (`REQUIRED_OPCODES`, `REQUIRED_UBLK_FEATURES`, which since Step 6 include
  `USER_RECOVERY_REISSUE`); on Linux the probe runs on a private SQE128
  ring.
- Step 6 added `CtrlCommand::del_dev_async`, `UBLK_MAX_UBLKS`, the
  `ublk-char` class listing (`UBLK_CHAR_CLASS_DIR`, `parse_char_name`), and
  `UblkControlRing::begin` and `poll`, a control command begun and polled
  between engine ticks rather than waited for (E44; one new block in the
  unsafe inventory).

### 3.4 otap-cp and otap-sim: what the engine does

- A write reserves a slot run, copies in, computes a CRC per block, takes a
  sequence number, inserts into the active generation, marks the run
  `Placed`, and is acknowledged (after an intent slot in precise mode when
  the volume was idle). Blocks it supersedes inside the active generation
  release their slots at once. A discard or a write of zeros takes one
  slot for a hole record, inserts a hole extent, and is acknowledged the
  same way; reads of it return zeros (E23).
- A cut is triggered by the timer, the arena at half, back to back, a
  request, shutdown or boot, and only when no generation is already waiting.
  The CP writes full stripes (summary page then data, assembled in memory),
  then its metadata regions through the metadata stream (the deltas, a
  fold's pages and indexes, the snapshot root pages and table, the roots
  table, the directory, the segment table) packed into whole stripes of the
  open metadata segment and into runs of free segments, then a flush, then
  the superblock at `A` and at `B`; the map, the generation and the slots
  change only after `B` lands. Regions a CP stops referencing are released
  after the next commit (one later on a flush-opaque device), and a freed
  segment is discarded or returned per the discard policy, staying
  allocated until its discard completes (E24).
- A fold rewrites the pages the log touched and empties the log, on the
  fold policy's bounds, on request, for a snapshot and at a clean shutdown
  (E22); a snapshot is taken at a fold, pinned by a root page, read through
  its pinned root and diffed by births (E25).
- Boot ranks the ring, loads the roots, indexes, directory, deltas and
  snapshot roots of the newest candidate, verifies the fold's pages when
  the candidate is the fold, replays the deltas, verifies every stripe that
  CP wrote by summary CRC and identity, rewinds to the next candidate on
  failure, re-attaches the arena when it can (verifying every descriptor
  and block, hole records included, and replaying the placed writes and
  punches) and holds the boot's write-active set under nvfail whenever the
  boot is dirty and the arena did not come back whole, or the boot
  rewound; it commits once before serving and retries that commit after a
  failure that was not a lack of space. Volumes the control plane lists
  and the disk lacks are created with that commit. A flush-opaque device
  boots under the trust policy with a warning in the pool status, or is
  refused under the strict one (E21).
- A write is acknowledged without an intent only to a volume that is
  durably active now and in the set of every superblock slot built and not
  yet landed; a volume created at runtime and a cleared nvfail serve only
  once their CP commits; metadata segments are recycled so empty CPs
  cannot exhaust the pool.
- Space (E37): `reserve_for(n) = min(max(8, n / 12), n / 2)` free segments
  are kept from front-door writes, with the two-segment floor for the boot
  and shutdown commits inside; a write is admitted only when the blocks
  acknowledged and not yet on disk plus its own fit outside the reserve,
  else it is answered `NoSpace` at once; acknowledgements are paced from
  80% arena occupancy and stalled at 95%. The cleaner runs in a CP on
  request, above 70% utilisation or while the pool is tight: it takes the
  data segments with the fewest live blocks (below 80% live, oldest first),
  rereads their stripes, keeps every block the live map or a snapshot still
  maps to that exact address, rewrites them as relocation stripes with their
  births and CRCs, rewrites the map entries (and a snapshot's pages with a
  new root page, once per shared tree), lists every stripe in the commit
  (F17) and frees the drained segments after the deferral. The plan counts
  the rewrites' metadata, skips a segment whose summaries do not all
  verify, and waits after a failure for space until a segment is freed; a
  failed attempt gives its segments back; a CP that relocates carries no
  deletion and no volume operation, which wait for the next (E39). A full
  or tight pool runs cleaning CPs of its own until something is freed, for
  a pending deletion too.
- The lifecycle (E36): a snapshot deletion releases, at the next CP, the
  blocks the snapshot alone referenced (born after the older neighbour,
  not named by the newer neighbour or the live map) and the pages no
  remaining tree shares; a pinned snapshot refuses deletion; the
  replication base moves with `set_replication_base`; a rollback (offline,
  quiet, no newer snapshot) makes the snapshot's tree the live tree at a
  fold and bumps the generation; a grow is online, a shrink and a deletion
  offline; names live in the catalogue; `audit_liveness` walks every tree
  and reports any segment whose live count drifted.
- The simulator's checker knows every issued write and punch from the data
  itself and checks, after every recovery, that the image is the state
  after the issued operations up to one operation number (the largest the
  image agrees with, since zeros are ambiguous), that no acknowledged
  operation is lost except after a power cut, and that nvfail holds
  exactly what the rules say; the committed image alone is checked to be
  a prefix after every operation; every snapshot the harness took is read
  back against the image the model gave it, and the live diff against it
  is checked against what changed.
- Step 6 (E41 to E45, E50 to E57, E60). A dirty boot's arena writes wait
  for the boot commit's `A` slot, so a kill during the boot never loses the
  hold; the re-attach judges a stale-sealed slot by its neighbours and its
  implied position, and drops a torn place whole; an arena of another size
  is refused unless nothing can be lost (`--discard-arena` overrides). A
  snapshot found damaged (a map page, or a record recovered from a damaged
  table, `SNAP_UNCERTAIN`) is quarantined: reads, diffs, rollbacks and
  replication refuse it, its deletion releases only what can be read, and
  a cleaning CP keeps the quarantine. The snapshot table and the catalogue
  are a base and a chain of deltas behind a head page (format 2.4): an
  unchanged table writes nothing, a changed one a delta of its changed
  keys, folded by `TableFoldPolicy`; a snapshot table that lost records
  refuses the boot. A relocation verifies each block's CRC before it moves
  it. The verification job (`start_verify`) attests one committed CP in
  slices while the pool runs, holding the frees that would release N's
  regions. The replica side (`set_replica`, `apply_write`, `apply_punch`,
  `durable_seq`, `create_snapshot_as`) and the source side
  (`snapshot_delta_scan`, `snapshot_read_blocks`) serve `otap_cp::repl`,
  whose replicator steps source and sink jobs after every tick over the
  `Link` traits. The simulator carries replication between two pools over
  `SimNet` (drops, cuts, corruption, refused dials) with `MemStore` as the
  state directory that survives a crash.

### 3.5 otap-geom, otap-backstore and otap-ublk: the machine

- `otap-geom` reads sysfs through a source trait (`Sysfs` on Linux,
  `MockSysfs` anywhere), classifies the device, and plans the stripe (md
  chunk times data disks, else `optimal_io_size` of at least 64 KiB, else
  1 MiB), the segment (80 MiB rotational, 64 MiB NVMe, 8 MiB flash, shrunk
  until the device holds eight) and the policies (trust for hardware RAID
  virtual disks, strict elsewhere; whole-segment discards on flash), with
  warnings and operator overrides (E26).
- `otap-backstore`: `UringBackstore` stages every write in the ring's
  aligned pool, writes the superblock and intent slots with `RWF_DSYNC`,
  issues the barrier as `fsync`, discards with `BLKDISCARD` and checks
  the capacity at open; `FileBackstore` does the same synchronously on a
  file and is what the Windows host runs the engine on (E27).
- `otap-ublk`: a queue descriptor decodes into an engine request or an
  immediate errno; the engine's status maps back to bytes or `-errno`;
  request ids carry the queue, the tag and a generation so a stale
  completion is dropped; watermarks gate the engine; offline answers
  `ENODEV` without the engine; a refused commit retires the tag. On Linux
  the device is created with `USER_COPY`, `USER_RECOVERY` and the ioctl
  encoding, 4 KiB blocks, a volatile cache with FUA, discard limits and
  8 MiB requests, one queue, a fetch on every tag before `START_DEV`
  (E28).
- Step 6 (E40, E44, E46 to E49): the device control is host-independent
  (`UblkHost`, `HostDevice`, `DeviceSpec`), so `MockKernel`, an in-memory
  model of the 6.12 driver's recovery rules, and `LinuxHost` share it.
  Devices are created with `USER_RECOVERY_REISSUE` and a stamp in
  `ublksrv_flags` (pool fingerprint and volume); `START_DEV` and
  `END_USER_RECOVERY` are begun and polled while the engine serves the
  queue (`start_while_serving`); `Recovery` re-attaches a dead daemon's
  device (wait for `QUIESCED`, `START_USER_RECOVERY`, fetch every tag,
  `END`), giving up after `--recovery-timeout`; `classify` and
  `reap_check` give every device on the host a standing (serving,
  recoverable, stale, foreign) and allow deleting only stale or named
  recoverable ones, never one still quiescing.

### 3.6 otap-control, otap-daemon and otap-ctl: the daemon and its client

- `otap-control`: `Request` (`op`-tagged JSON: `ping`, the pool's format,
  boot, status, sync, clean and stop, the daemon's stop, the volume and
  snapshot verbs, and `io_write`, `io_read`, `io_punch` for the harness)
  and `Response` (`kind`-tagged: reports of the pool, a volume, a snapshot,
  the lists, an I/O result, a committed CP), `ControlError` with an
  `ErrorCode`; frames of a 4-byte little-endian length and JSON, 4 MiB at
  most; `Endpoint::parse` for `unix:<path>` and `tcp:<addr>:<port>`;
  `Client` with timeouts and a protocol check (E38).
- `otap-daemon`: the engine thread owns the engine and executes every
  request between ticks, waiting for the event each needs within the
  operation timeout; a composite front door feeds the engine from the
  control channel and, on Linux, from one `UblkFrontDoor` per online
  volume (created on the engine thread, torn down at offline, rollback,
  stop and exit, recreated after a resize); `pool format` makes a fresh
  aggregate identity and refuses an existing pool without `--force`;
  `pool boot` probes the superblock for the identity and the recorded
  geometry, opens the device (planner and io_uring for a block device, the
  small-file or large-file defaults and `FileBackstore` for a file), the
  arena (hugetlbfs or heap) and the engine with no configured volumes, and
  waits for the boot commit; `SIGTERM`, `SIGINT`, `daemon stop`, `pool
  stop` and the loss of the last handle all end in the engine's clean
  shutdown; the server takes a Unix socket or TCP, a thread per connection,
  and answers a bad frame or an unencodable reply with an error reply.
- `otap-ctl`: `otapctl [--control E] [--json] [--timeout S] <group> <verb>
  --flags` over `daemon`, `pool`, `volume`, `snapshot`, `io`, and since
  Step 6 `device` and `replicate`; sizes with `K`, `M`, `G`, `T`; exit 0, 1
  (refused), 2 (usage), 3 (unreachable), 4 (a waited verification or
  replication that ended other than clean or done).
- Step 6 added, under protocol version 1 (new variants only, E59; every
  field-less request and reply an empty struct variant, so unknown fields
  are refused): `DeviceList`, `DeviceReap` (answered `Devices`, `Reaped`);
  `PoolVerify`, `PoolVerifyStatus`, `PoolVerifyCancel` (answered `Verify`);
  `ReplicatePush`, `ReplicatePull`, `ReplicateStatus`, `ReplicateCursor`,
  `ReplicateCancel`, `ReplicateRelease` (answered `Replication`,
  `Replications`, `ReplicationCursor`, `Ok`). The verbs: `otapctl device
  list`, `device reap <id>|--stale`; `pool verify [--no-wait|--status|--cancel]`;
  `replicate push <vol> <snap> --to <host:port>[/<vol>]`, `replicate pull
  <vol> <snap> --from <host:port>[/<vol>]`, `replicate status [<id>]`,
  `replicate cursor <vol>`, `replicate cancel <id>`, `replicate release
  <vol>`. `dbladed` gained `--state-dir` (the device record and the sink
  records; default `/run/opentap` on Linux), `--recovery-timeout` (1 to
  300 s, default 35), `--discard-arena` (with `--device`),
  `--replication-listen <host:port>` and `--allow-remote-replication`
  (listening and dialling are loopback-only without it). The daemon
  re-attaches the devices a killed daemon left before it creates any,
  keeps the binding record, steps the replicator after every tick, keeps a
  mirror offline and marked, and refuses a rollback or resize of a mirror.
  A switch given a value (`--force=no`) is refused by both binaries.

## 4. Where this sits in the Phase C plan (doc 05, Part 8)

Week 1 of the §8.5 timeline, by column:

| Item | Status |
|---|---|
| E1: `otap-format` v2 frozen with every structure of §7.1, including the `geom`, `cluster_state`, `durable_log` and `inbound[]` reservations; proptest, golden vectors, layout asserts | done, with the F1 to F11 departures from the §7.1 sketch recorded in `07-engineering-decisions.md`; format 2.1 (F12 to F14) added the CP log directory, the snapshot root page and the table regions in Step 4 |
| E2: workspace, `otap-sys` boundary and lints | done |
| E2: `otap-stream` v1 frozen | done for the frame layer; since Step 6 a framed TCP transport (no TLS) and the simulated network carry it (E58); the `TlsTcp` and `FileSink` transports are not written |
| E2: `Backstore`, `FrontDoor`, `Clock`, `Net` traits | done (Step 3, `otap_cp::traits`, E13); `Backstore` gained discards and the policies in Step 4 |
| E2: the `ClusterState` schema with the SVM-owned and cluster-owned split and its snapshot format | not written; only the superblock root is reserved |
| E2: the CI VM image | none; there is no CI and no Debian host |
| Exit test "100k cases per structure green" | the merge gate is documented (`PROPTEST_CASES=100000`); 10,000 cases run locally on every change, 100,000 on the stream suite once |
| Exit test "CI VM runs a ublk null device and an LIO loopback login" | not possible on this host |

Week 2 (Step 3):

| Item | Status |
|---|---|
| E1: `otap-arena`, the hugetlbfs arena, slot-state tracking, re-attach | done; the hugetlbfs file and the systemd units are `dbladed`'s (the arena takes any `HugeArena`) |
| E1: `otap-index`, per-volume maps, split and trim, the epoch-flip cut, the generation ring | done; `cut_within` for freezes is not yet needed |
| E2: `otap-sim` scheduler and fakes; the checker model | done |
| Exit test "linearizability versus the model" | the checker's prefix and nvfail rules pass at 30,000 random scenarios; the engine is single-threaded, so linearizability reduces to sequential consistency against the model |
| Exit test "100 `kill -9` with zero acknowledged-write loss" | the simulator's daemon crashes (arena kept) lose nothing, in named tests and in the random scenarios; a real `kill -9` lane needs Linux |
| Exit test "re-attach under 2 s p99" | not measurable here |

Weeks 3 to 6 (Step 4):

| Item | Status |
|---|---|
| E1 (week 3): a real `Backstore`, md and a single device, geometry from sysfs, `WRITE_FIXED` stripes, `FSYNC` then the `RWF_DSYNC` superblock | written (`otap-backstore`, `otap-geom`, `otap-sys::block`): staged `O_DIRECT` writes rather than `WRITE_FIXED` (the fixed-buffer registration of `otap-sys::uring` is not yet wired), the barrier and the dsync slots as planned; type-checked only, never run on a device |
| E2 (week 4): `otap-map`, the CP log as its journal, the checkpoint, holes with birth, `diff` | done (E22, E23, E25); per-read CRC of map pages is the page CRC in the index entry |
| E1 (week 5): triggers, deferred reuse, dirty-boot summary verification, `cut_within`, the forced dirty-degraded assembly test on md | deferred reuse done (E24); `cut_within` and the md lane not done |
| E2 (week 5): `otap-geom`, the attestation | the planner done; the attestation (section 7.3) not written (Step 6's `pool verify` is the engine's verification, below) |
| E1 (week 6): the ublk front door and `dbladed` | the front door done with one queue (E28); `dbladed` written in Step 5 (E38): the process, the hugetlbfs arena file (and a heap arena), the control socket, one ublk device per volume, the back-pressure curve; the systemd units, multi-queue, the reactor partitioning of the arena and `cut_within` are not |
| Exit tests of weeks 3 to 6 (dm-flakey, dm-log-writes, the null ublk device, fio) | need Linux; `scripts/linux-smoke.sh` is the fio and restart lane, written and not run |

Weeks 7 to 8 (Step 5):

| Item | Status |
|---|---|
| E1 (week 7): the cleaner, relocation of live blocks, the reserve | done (E37): candidates by live blocks and age, the exact-address check, relocation stripes with the old births, drained segments freed and discarded, the reserve and the admission rule, the full-pool pump; relocation tables not used |
| E2 (week 7): snapshot deletion, deadlists, the space a snapshot pins returned | done without deadlists (E36): the deletion compares against the neighbouring snapshots and the live map at the CP that deletes; pins and the replication base; rollback, resize, deletion and offline volumes beyond the plan |
| E1 (week 8): `WRITE_FIXED` stripes through the registered buffers | not done; the backstore stages `O_DIRECT` writes |
| `otapctl` (R56, a single node) and the catalogue | done (E38, F16): the lifecycle end to end over a file; the Linux run is the smoke script |

Step 6 (items from weeks 5, 6 and 11 and the incremental tables):

| Item | Status |
|---|---|
| E1 (week 6 leftover): the `USER_RECOVERY` re-attach of a device after a daemon crash, and a reaper for stale devices | written (E40, E44, E46 to E49): `REISSUE`, the stamp, the binding record, the recovery, the standings, `otapctl device`; modelled by `MockKernel`, never run on Linux |
| Exit test "re-attach under 2 s p99" | met on the simulated clock for a device the driver has quiesced; a device whose daemon died holding every tag waits for the block layer's 30 s request timeout (E48); unmeasured on a kernel |
| The incremental tables | done for the snapshot table and the catalogue (format 2.4, E50 to E52); the segment table is still written whole, and a fold still rewrites a dirty volume's whole index |
| E2 (week 5): the attestation | the engine's verification of one committed CP (`otapctl pool verify`, E53); the design's attestation of a RAID cache (§7.3) is a different item, not written |
| E2 (week 11): the transport, the replication cursor that supersedes a replication base | replication between two pools: the source and the sink over the `Link` traits, a framed TCP transport without TLS and the simulated network, the sink's cursor in `--state-dir`, the base pin moved on `Committed` (E54 to E58); TLS and the `FileSink` behind `dump --since` are not written |

§8.4 test strategy: the format-freeze lane and the simulation lane exist,
the latter now covering the map, folds, snapshots, holes, the incremental
tables, the verification and replication between two simulated pools with
network faults; the engine also runs over a real file, and two daemons
replicate over loopback TCP on the Windows host. Crash consistency on a
kernel (dm-log-writes, dm-flakey) and the CI image do not exist. Milestone
M1 ("formats frozen and the arena survives `kill -9`") is met under the
simulator; the physical re-attach test is a Linux item.

## 5. Gates, as run on 2026-10-16 after Step 6 and its review

On the development PC every cargo command is prefixed with
`export PATH="$HOME/.cargo/bin:$HOME/.cargo/mingw-tools/bin:$PATH"` (see the
Windows notes in `BUILDING.md`) and runs with `-j 3`; tests use `--test-threads=6`.
Every gate ran on the committed tree `758adb8` (the review commit), except
the second run of the full 10,000-case gate, which ran on `80c9423` (the
docs commit after it; the same code).

| Gate | Result |
|---|---|
| `cargo fmt --all -- --check`; `cargo fmt --manifest-path fuzz/Cargo.toml -- --check` | clean |
| `cargo clippy --workspace --all-targets -- -D warnings` (host) | clean |
| `cargo clippy --workspace --all-targets --target x86_64-unknown-linux-gnu -- -D warnings` | clean |
| `cargo clippy --manifest-path fuzz/Cargo.toml --no-default-features -- -D warnings` | clean |
| `RUSTDOCFLAGS="-D warnings" cargo doc --workspace --no-deps` | clean |
| `bash -n scripts/wsl2-prep.sh scripts/linux-smoke.sh` | clean |
| `cargo test --workspace` | 760 tests and 2 doctests passed, 0 failed, 1 ignored (the vector writer), in 3 min 43 s |
| `PROPTEST_CASES=10000`, the Step 6 property suites one at a time | all passed: `step6c_replication_sim_tests` 10 min 0 s, `step6b_catalogue_sim_tests` 10 min 11 s, `step6b_verify_sim_tests` 7 min 25 s, `adversarial_arena_tests` (6A's walk with kills inside fills) 3 min 58 s, `step6c_transport_wire_tests` 5 s, `step6b_catalogue_format_tests` 1 s |
| `PROPTEST_CASES=10000 cargo test --workspace` | all passed, over two runs: 760 tests and 2 doctests passed, 0 failed, 1 ignored, in about 241 min of test wall time. The first run (17:02:28 to 20:20:04, 197 min 36 s, on `758adb8`) completed 58 test binaries, 496 tests passed, 0 failed, 1 ignored, and ended when `step5_sim_tests` aborted with Windows status 0xc0000409 after "memory allocation of 12779520 bytes failed": system memory stood at 92% from an unrelated foreground application (about 17 GB; the test process used about 30 MB), not a code defect. The second run (20:22:09 to 21:05:30, 43 min 21 s, on `80c9423`, with 25 GB free) ran only what had not completed: `step5_sim_tests`, `step6a_sim_tests`, `step6b_catalogue_sim_tests`, `step6b_verify_sim_tests`, `step6c_replication_engine_tests`, `step6c_replication_sim_tests`, `step6c_transport_simnet_tests`, all of `otap-stream`, `otap-sys` and `otap-ublk`, and the doctests: 264 tests and the 2 doctests passed, 0 failed. Slowest: `step4_sim_tests` 62 min 48 s, `review_step5_sim_tests` 50 min 51 s, `adversarial_sim_tests` 20 min 41 s, `map_tests` 20 min 31 s, `sim_tests` 17 min 12 s, `step5_sim_tests` 13 min 5 s, `step6c_replication_sim_tests` 11 min 2 s, `step6b_catalogue_sim_tests` 10 min 16 s, `step6b_verify_sim_tests` 8 min 6 s; every other suite under six minutes. Logs in `OpenTAP-local/step6/gate-10k-2026-10-16/` |

The Step 5 figures for comparison: 541 tests and 2 doctests in 3 min 13 s;
the 10,000-case gate in 179 min 46 s.

Not runnable here: the Linux halves of `otap-sys`, `otap-backstore`,
`otap-ublk` and `otap-daemon` (type-checked only), the fuzz targets (need
nightly and cargo-fuzz), both scripts, and every kernel lane of §8.4.

## 6. What the code assumes about the owner decisions

D1, D2, D8, D9 and D10 are answered (E21). Of the rest no answer is
recorded; the implementation touches them only here:

- **D2 (the durable log).** The superblock carries a 64-byte `DurableLogHdr`
  whose `NONE` state is verified; that is the "reserved in v0.1's format"
  option. `unmirrored_flush = placed` is how a flush completes today.
- **D3 (the control plane).** The `cluster_state` root is reserved in the
  superblock. No `StateStore` code exists. The Step 5 brief's ruling
  labelled "D3" is about the arena, the CP cadence and the platform
  baseline (S5-1 in E34), not about the store; Step 5's daemon keeps
  every durable fact in the pool and has no store, so D3 proper stays
  open with less to persist.
- **D5, D6, D14 (nvfail).** The `nvfail` bitset and the per-volume cause in
  the volume table exist and nvfail is on; no policy code beyond that.
  The Step 5 brief's rulings labelled "D5", "D6" and "D14" are snapshot
  deletion, the cleaner's reserve and the daemon (S5-2 to S5-4 in E34);
  the nvfail class table, the failover identity and `nvfail_force_lag`
  remain unanswered.
- **D9 (`--write-journal`).** Nothing written; the trim posture is the
  `DiscardPolicy`.
- The remaining decisions have no footprint in the code yet.

Decisions the architect took without an owner answer because the code needed
one (the Rust floor, F1 to F15, the stream rules, E13 to E33) are in
`07-engineering-decisions.md`; each is reversible at the cost stated there.

## 7. Next

1. **The first Linux run, on WSL 2 on the development PC** (the order of
   the 6C notes): the owner switches BIOS "SVM Mode" on and reboots;
   `wsl --install --no-distribution`; Debian, its VHDX on E:; `sudo
   scripts/wsl2-prep.sh --install-deps`; `scripts/wsl2-prep.sh
   --build-kernel` (Microsoft's `linux-msft-wsl-6.18.54.1` with
   `BLK_DEV_UBLK=m`, `DM_LOG_WRITES=m`, `DM_FLAKEY` and `DM_DELAY`); the
   `.wslconfig` kernel line and `wsl --shutdown`; the staged modules copied
   and `depmod`; `scripts/wsl2-prep.sh --probe` all PASS; `sudo
   scripts/linux-smoke.sh`, then `--heap-arena`. Before the re-attach is
   trusted there, save the 6.18 ublk driver and read it against E40 (the
   saved sources are 6.12). Measure what the sources only predict: the
   time to serving after `kill -9` against the 2 s target, the `EBUSY`
   window, a kill with every tag held (fio with a deep queue; the smoke
   test's dd keeps one request in flight) and the 35 s wait, ext4 mounted
   across a restart, a discard in flight at a kill, `GET_PARAMS` on other
   software's devices, `ublks_max`. Every finding goes into `08`.
2. **Then**: the kernel lanes (dm-flakey, dm-log-writes) and the fuzz
   targets on nightly; and the review's open items worth code: a
   cross-link check in the verification (C2), a forced table fold that
   does not fit (C9), a deletion held by a persistent read error (C11), a
   quarantine that outlives a reboot or a page check before a rollback
   (C12), always keeping a snapshot head to fence older binaries (C15),
   TLS and authentication for replication with per-connection byte budgets
   (C4), and a persistent default for a sink's state.
3. **What doc 05 schedules next**, in the order its dependencies allow: the
   daemon's leftovers (the systemd units, multi-queue devices and the
   reactor partitioning of the arena, `cut_within`, `WRITE_FIXED` stripes
   through the registered buffers); the segment table made incremental (it
   is still written whole at every CP, and a fold rewrites a dirty volume's
   whole index); the metadata device class; the `FileSink` behind `dump
   --since` and `restore`.

What the owner needs to provide: the BIOS switch and the reboot; answers to
Part 9's D3 (the control plane's store), D5, D6 and D14 (the nvfail class
table, the failover identity, `nvfail_force_lag`) before the nvfail policy
code; whether a pool whose snapshot table lost records should stay refused
(the review's D9) or, once a repair tool exists, boot read-only; and when
TLS for replication is wanted (before any LAN use, or with the multi-node
work).

## 8. Known limits, debts and risks

- **History starts at the Step 3 commit.** Everything before `aee54fc`
  exists only as this log describes it; the Step 1 and Step 2 states cannot
  be checked out.
- **Nothing kernel-facing has executed.** The ublk, io_uring, hugetlbfs and
  block-device code is checked against the saved 6.12 sources and
  type-checked for Linux only; so are the daemon's device lifecycle, the
  Unix socket, Step 6's `LinuxHost`, the re-attach (`UblkDevice::reattach`,
  `END_USER_RECOVERY` through the control ring), `REISSUE` granted by
  `ADD_DEV`, the reap (`STOP_DEV` then `DEL_DEV_ASYNC`), the pid checks in
  `/proc` and the replication transport on Linux. The geometry planner has
  run only against mocks. `scripts/linux-smoke.sh` and
  `scripts/wsl2-prep.sh` have passed `bash -n` only. The WSL 2 host is
  waiting for the owner's BIOS switch (section 7).
- **The re-attach's timings are unmeasured.** The 2 s target, the length of
  the `EBUSY` window, and the wait for a device whose daemon died holding
  every tag (the block layer's 30 s request timeout, which is why
  `--recovery-timeout` defaults to 35 s, E48, E60) are figures from the
  driver's source, not from a run. A `dbladed` that cannot run as root
  cannot list privileged devices.
- **The fuzz targets have never run.** They compile under
  `--no-default-features`; a nightly toolchain and cargo-fuzz are needed to
  execute them.
- **The 100,000-case merge gate has no CI to run it.** Locally every change
  runs at 10,000 cases.
- **Replication has no TLS and no authentication** (E58). Whoever reaches
  the replication port can push into a mirror or pull a snapshot, so
  listening and dialling are loopback-only unless
  `--allow-remote-replication`. A peer that reaches the port can also pin
  memory and threads: each connection queues up to 64 messages of up to
  8 MiB, and nothing caps the connections (the review's C4).
- **One destination per source volume** (E57): the replication base is the
  `PIN_REPLICATION` bit, not a per-destination field, so a second
  destination would move the first one's base; and a pool keeps one copy
  of a source snapshot, so two mirrors of one source on one pool are
  refused (E60).
- **Source jobs do not survive a daemon restart.** A source keeps its jobs
  in memory; after a restart the operator pushes again, and the push
  resumes from the sink's durable cursor.
- **The sink's cursor lives in `--state-dir`** (E56), a tmpfs by default.
  A host reboot of the sink loses it: the next transfer starts the delta
  over from the base the pool holds, and a mirror with a committed base
  stays marked by its pinned transfer snapshot (E60); a partial *first*
  transfer whose record is lost is an ordinary offline volume that `volume
  online` would serve half-landed. A sink's `--state-dir` belongs on
  persistent storage.
- **The delta's identity omits the chunk size** (C5 of the review), and the
  source walks the whole delta before `DeltaBegin` (8 bytes per chunk and
  12 per hole run). The sink lands through the arena, a CP per batch.
- **The verification cannot see everything and costs a full read** (E53).
  About 1 TiB of reads per TiB of live data plus map pages and summaries
  (about 4 GiB each per TiB), about 1 MiB per tick at the defaults; the
  frees it holds grow with the run, so a long verification of a nearly full
  pool gives way. It cannot see blocks under a tree that fails to load (the
  accounting then drifts and is reported as a warning), stripes and regions
  nothing references, free space, superblock slots older than N-1, or the
  arena and the ublk side. It dedupes block references by address before
  it checks them, so a cross-linked entry would go unreported (C2); and
  `pool verify` follows whichever verification is current (C3).
- **A snapshot table that lost records refuses the boot** (E60, the review's
  D9): one unreadable head or record of the snapshot table, written by an
  older CP, now stops the pool, as format 2.3 did, rather than dropping
  snapshots and freeing their blocks. There is no repair tool yet; damage
  that loses no record, and any damage to the catalogue, boots with a
  warning.
- **A quarantine found while running is held in memory** (E43, C12): after
  a reboot a rollback to a snapshot whose map page was found damaged is
  accepted, the volume reads `Corrupt` over that range, and the first fold
  that rewrites the page stops folds pool-wide (E32) until a repair exists.
  `SNAP_UNCERTAIN` (a damaged table) is persisted and not affected.
- **A damaged snapshot's deletion releases nothing** (E36, E43): its blocks
  stay counted live until a repair tool exists. A persistent read error on
  a page of a snapshot being deleted defers the deletion every CP and stops
  the cleaner until a reboot (C11).
- **A segment holding a block that fails its CRC is never cleaned** (E60,
  D1): the block keeps reading `Corrupt` where it is; each change of the
  snapshot set costs one failed and retried cleaning CP over it.
- **A forced table fold that cannot be placed marks the pool full** (C9):
  a chain of 124 deltas on a pool whose free segments are never adjacent
  can reach it; a restart drops the pending change.
- **Format 2.4 and older binaries.** The engine boots 2.4 pools only (minor
  below 4 is `OldLayout`; nothing was released at 2.3). A Step 5 (2.3)
  binary boots a 2.4 pool with neither names nor snapshots and rewrites it
  as 2.3, and when the first name or snapshot is created, the other
  parity's slot has no head, so a 2.3 binary would tear the newest CP and
  silently boot the one before it (C15). No superblock field exists that
  2.3 refuses and 2.4 accepts; always keeping a snapshot head, written by
  `format` into CP 0, would close it. Do not run a Step 5 binary on a
  Step 6 pool.
- **The tables are incremental on disk, not in memory.** The snapshot set
  and the catalogue are still cloned and compared, O(n), in each CP that
  changes them.
- **Arena damage is held, not repaired.** A damaged descriptor or block
  found at re-attach is dropped and the boot holds the write-active set. A
  damaged word of the newest write's last slot cannot be told from a fill
  killed there and is released (E60, D10).
- **Delta byte counts are raw image bytes.** `DeltaBegin.bytes` and
  `DeltaEnd.bytes` are sums of decoded chunk lengths; the Phase B codec work
  must keep that meaning.
- **The golden-vector sets were regenerated once (2.0) and bumped four
  times (2.1 to 2.4).** Allowed only before a release; the procedure for
  any later change is in `BUILDING.md`.
- **The engine is one thread, and so is the control plane.** The reactor
  partitioning of the arena is a `dbladed` item (E16); the ublk front door
  serves one queue per device; the daemon answers one control request at a
  time and a long one (a rollback, a stop, a boot that waits for a device
  to quiesce) delays the next; an idle daemon wakes every 200 µs to 2 ms.
- **The cleaner has limits** (E37). A segment above 80% live is never a
  candidate; the scan rereads whole segments and compares against every
  snapshot tree; a candidate the scan found uncleanable is skipped until a
  free changes the picture. The reserve is `max(8, n / 12)` segments.
- **A heap arena dies with the process** (S5-1): under `--heap-arena` a
  daemon crash is a dirty boot that holds the write-active volumes under
  nvfail; only a hugetlbfs arena re-attaches.
- **The `Io*` requests are a test aid, not a data path**: whole blocks as
  hexadecimal, 256 per request, serialized with every other control
  request.
- **A volume's index must fit a quarter of the pool, and 4 GiB.**
- **A flush-opaque backstore is trusted by default** (E21).
- **A permanently unreadable sector in the newest CP's regions stops the
  boot** (E30).
- **A damaged map page is contained, not repaired** (E32).
- **The segment table is written whole at every CP**, and a fold rewrites
  the whole index of each dirty volume; both scale with the pool.
- **A discard in flight when the daemon is killed can still land** after
  the new process reuses the segment; the Linux run must measure it.
- **Snapshot ids are monotonic between rewinds**, not across one; the
  catalogue, the daemon and replication key snapshots by uuid and name.
- **The front door drops `UBLK_IO_F_FUA`** by design (D2).
- **Stale ublk devices are listed, never deleted silently** (E49): an
  orphan costs one of `ublks_max` (64) until the operator reaps it; a
  stamped device of another pool that was never started is reapable with
  no pool booted, which could race another daemon's creation of it (C8).
- **The 10,000-case gate is slow.** About four hours on the development PC
  (section 5); the simulator's random walks dominate, each one property
  test that more test threads cannot shorten.
