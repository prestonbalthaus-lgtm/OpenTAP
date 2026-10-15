# OpenTAP implementation status and handoff

Living document. Last updated 2026-10-14, after Phase C Step 5, with Step 6
begun and paused the same evening. Update it at
the end of every step and whenever section 7 ("Next") changes. Section 1 is
what a new session reads first; the rest is the detail behind it.

## 1. Where we are, in one screen

- **Step 6 is in progress and paused** (2026-10-14, evening). The brief
  is in `10` (it arrived cut off in the transport scope); ten maps of the
  code, their summaries and the lead's preliminary positions are in
  `step6/`. Nothing is decided or built yet: the code is still Step 5's.
  Start with `step6/README.md`.
- The design is complete through round 3: `05-architecture-brainstorm-03.md`
  holds rulings R25 to R64, the trait contracts (Part 7), the Phase C plan
  (Part 8) and the fifteen owner decisions (Part 9).
- Implementation started on 2026-10-13 in `E:\Coding\OpenTAP`. Steps 1 to 5
  of the implementation briefs are done and every gate in section 5 is
  green: the on-disk format (now 2.3) and the stream frame header v1
  (`otap-format`), the replication stream v1 (`otap-stream`), the kernel
  boundary (`otap-sys`), the three fuzz targets, the engine core (the
  write-cache arena `otap-arena`, the dirty index `otap-index`, the
  consistency-point engine `otap-cp`, the simulator `otap-sim`), since
  Step 4 the persistent map with folds and snapshots (`otap-map`), discards
  and writes of zeros as holes, geometry discovery (`otap-geom`), the real
  backstores (`otap-backstore`: io_uring on Linux, a file anywhere) and
  the ublk front door (`otap-ublk`), and since Step 5 (2026-10-14)
  snapshot deletion with pins and the replication base, rollback, resize,
  deletion and offline volumes, the segment cleaner with its reserve,
  admission rule and pacing, the name catalogue on disk, and the daemon
  `dbladed` with its control protocol (`otap-control`) and its client
  `otapctl` (`otap-daemon`, `otap-ctl`).
- The owner closed E19 and recorded D1, D2, D8, D9 and D10 with the Step 4
  brief (`07-engineering-decisions.md`, E21): flush-opaque backstores are
  trusted under `TrustBarrierWithNvfail` (the default) with a structured
  warning, and refused only under the strict policy.
- The engine, the daemon and the CLI have run only on the simulator's
  device and on a plain file (the whole lifecycle, format to clean
  restart, runs end to end over a file on the Windows host). The Linux
  halves (`otap-sys::block`, the ublk device module, `UringBackstore`,
  `Sysfs`, the daemon's device and Unix-socket code) are type-checked for
  `x86_64-unknown-linux-gnu` and have never executed: there is still no
  Linux host; `scripts/linux-smoke.sh` is the first run, waiting for a
  Debian 13 machine. There is no transport yet, and one engine thread.
- Step 4 closed with an independent adversarial review that found 24
  reproducible defects, five of them in the boot (the Step 4 entry of
  `08-engineering-log.md`, decisions E29 to E33); the lead's own suites had
  found five before it. All are fixed and pinned.
- Step 5 closed with an independent adversarial review (twelve defects, four
  of them losing or exposing data and three wedging the pool, all in the
  seams between the cleaner, the snapshot set and the volume operations,
  and one cited finding that proved not to be a defect; E39 and the Step 5
  entry of `08-engineering-log.md`); the lead's own suites had found six
  defects before it, one of them a liveness leak. All are fixed and pinned.
- The tree is under version control: branch `main` of the GitHub
  repository `github.com/prestonbalthaus-lgtm/OpenTAP`, public since
  2026-10-14 with the design records in it by the owner's decision; one
  commit per reviewed change with the gates green, pushed through the
  GitHub CLI.
- Licensed under Apache-2.0 since 2026-10-14 (`LICENSE`; every manifest
  carries the field). Every package in both lockfiles declares a
  permissive license (MIT, Apache-2.0, BSD, Zlib, CC0, Unicode, NCSA);
  nothing copyleft is linked.
- Owner decisions D3 to D7 and D11 to D15 of Part 9 have no recorded
  answers; the code depends on them only through the reservations listed
  in section 6. The Step 5 brief (2026-10-14) carries four rulings the
  owner labelled D3, D5, D6 and D14 that settle other items (the arena
  and CP cadence, snapshot deletion, the cleaner and its reserve, the
  daemon and the catalogue); they are recorded as S5-1 to S5-4 in E34 of
  `07-engineering-decisions.md`, with the mapping, and Step 5 was built
  against them (E35 to E39).
- The development PC runs Windows. The first run on a Debian 13 host
  (`scripts/linux-smoke.sh`, section 7) will find things in the Linux
  halves; budget for it.

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
| `07-engineering-decisions.md` | the implementation decision log: where the code departs from the design or a brief, and why | living (E01 to E39) |
| `08-engineering-log.md` | dated record of what happened: steps, reviews, defects, gate results, lessons | living, append only |
| `09-working-agreements.md` | how the work is done: roles, definition of done, code and test rules, build etiquette, documentation rules | living |
| `10-phase-c-step-briefs.md` | the owner's step briefs, verbatim, each with a note on how it was read | append per step |
| `../README.md` | the public face of the repository: the goal, how the system works, status, where to start | kept current |
| `../BUILDING.md` | build, test and layout guide, including the Windows host notes | kept current with the tree |
| `reviews/` | the worker reports behind the Step 1, 2, 4 and 5 reviews, verbatim, with an index | archive |
| `step6/` | Step 6 working notes: the handoff, the lead's preliminary positions, ten maps of the code with their summaries | in progress, paused 2026-10-14 |
| `../crates/otap-format/src/lib.rs` | the layout specification and format rulings F1 to F18; the code is the authority | format 2.3, stream header 1.0 |
| `../scripts/linux-smoke.sh` | the first-run smoke test for a Debian 13 host (the README has the steps) | written, not yet run |

## 3. The workspace as built

One Cargo workspace (`Cargo.toml` at the root), edition 2021, `rust-version`
1.89, built and tested with stable 1.99. `fuzz/` is excluded from the
workspace because it needs nightly and links libFuzzer.

| Crate | Role | `unsafe` | Modules | Tests |
|---|---|---|---|---|
| `crates/otap-format` 0.1.0 (format 2.3) | on-disk format and the frame header v1: zerocopy views, CRC-32C sealing and verification, the superblock ring, stripe summaries, map pages, the records, the CP log directory, the snapshot root page, the table regions, the recorded geometry, the name catalogue, the commit's stripe list; no I/O, no policy | forbidden | `constants`, `types`, `checksum`, `superblock`, `stripe`, `map`, `stream`, `records`, `catalog` | `format_tests` 53 (+1 ignored vector writer), `records_tests` 15, `catalog_tests` 9, `adversarial_tests` 49, 1 doctest |
| `crates/otap-stream` 0.1.0 | stream v1: typed frames, the codec, the landing cursor and its page, credit windows; no sockets | forbidden | `frame`, `codec`, `cursor`, `flow` | `stream_tests` 13, `adversarial_stream_tests` 62 |
| `crates/otap-sys` 0.1.0 | the kernel boundary: hugetlbfs arenas, the ublk 6.12 UAPI with the control ring and the queue mapping, io_uring buffer registration and cloning, the block-device ring, the capability probe; Linux implementations behind `cfg`, portable fallbacks | thirteen blocks, all Linux-only, each with a `// SAFETY:` proof | `hugetlb`, `ublk`, `uring`, `block`, `probe` | `sys_tests` 33, 1 doctest |
| `crates/otap-arena` 0.1.0 (layout 1.2) | the write-cache arena over `otap-sys` memory: an immutable sealed header, a descriptor table whose every transition is one sealed-word store, 4 KiB slots of data or hole records; runs with lap counters; `Free`/`Filled`/`Placed`/`Committed`; re-attach after `kill -9` with data and descriptor CRCs verified and damage reported | forbidden | one module | `arena_tests` 12, `adversarial_arena_tests` 12 |
| `crates/otap-index` 0.1.0 | per-volume dirty extents of data and holes (split, trim, coalesce; later insert wins; stale sequences refused), generations, the cut over `arc-swap` with a grace wait, the frozen queue | forbidden | one module | `index_tests` 10, `adversarial_index_tests` 13 |
| `crates/otap-map` 0.1.0 | per-volume roots, page indexes and overlays; delta regions and the directory per CP; copy-on-write folds; replay; snapshot pins and the birth walk; rollback, resize and deletion at a fold with the released entries; the rewrite of a pinned tree; a tolerant walk; a pba-keyed page cache; no I/O | forbidden | one module | `map_tests` 8 (a model with proptest), `review_map_tests` 10, `lifecycle_map_tests` 6 |
| `crates/otap-cp` 0.1.0 | the four traits with the flush and discard policies; the engine: write, read and punch paths, intents, the CP state machine (staging, the metadata stream, flush, commit A then B), folds, snapshots and their deletion, pins and the replication base, rollback, resize, deletion and offline volumes, the catalogue, the reserve and the admission rule, pacing, the cleaner's driver and relocation stripes, the full-pool pump, liveness and deferred frees, the liveness audit, boot with replay and rewind, nvfail, durable-before-serving volumes and clears, metadata recycling, clean shutdown; the disk layout; the segment table; the cleaner's candidate scan | forbidden | `traits`, `layout`, `segments`, `meta`, `boot`, `engine`, `cleaner` | tested through `otap-sim`, `otap-backstore`, `otap-ublk` and `otap-daemon` |
| `crates/otap-geom` 0.1.0 | sysfs facts behind a source trait with a mock, device classification, the stripe and segment plan, the media profile, overrides | forbidden | one module | `geom_tests` 11, `review_geom_tests` 10 |
| `crates/otap-backstore` 0.1.0 | `UringBackstore` (Linux, over `otap_sys::block`) and `FileBackstore` (anywhere) | forbidden | `file`, `uring` | `file_backstore_tests` 2 (the engine over a file), `review_backstore_tests` 5 |
| `crates/otap-ublk` 0.1.0 | the `Transport` trait, the `UblkFrontDoor` state machine, the mock transport; on Linux the device lifecycle, the real transport and a runner | forbidden | `frontdoor`, `mock`, `device` (Linux) | `frontdoor_tests` 24, `frontdoor_proptests` 1, `engine_tests` 7, `review_ublk_tests` 12 |
| `crates/otap-sim` 0.1.0 | fake device (pending, cached, durable; seeded completion order; sector tears; discards; a protected or unprotected controller cache), fake client, virtual clock, loopback net, the checker model (writes, punches, rollbacks and resizes, settled after each recovery), the harness (one request at a time or several in flight; snapshots and their expected images; deletions, rollbacks, resizes, volume deletions, offline, cleaning; the liveness audit after every recovery) | forbidden | `backstore`, `frontdoor`, `clock`, `net`, `model`, `harness`, `rng` | `sim_tests` 15, `adversarial_sim_tests` 43, `step4_sim_tests` 15, `review_sim_tests` 23, `step5_sim_tests` 15, `review_step5_sim_tests` 18 (the Step 5 review, with the two property tests of the brief), with four random-walk property tests |
| `crates/otap-control` 0.1.0 (protocol 1) | the control protocol: requests, replies and errors as tagged JSON, length-prefixed frames bounded at 4 MiB, the endpoint syntax, a blocking client, the size and bound helpers shared by both ends; re-exports the format's block size and pin bits | forbidden | `protocol`, `wire`, `client`, `units` | 8 unit tests (every request and reply round-trips), `review_control_tests` 5 |
| `crates/otap-daemon` 0.1.0 | `dbladed`: the engine thread over `Box<dyn Backstore>` and a composite front door (the control channel, the ublk devices on Linux), the pool's operations waiting for their consistency points, the arena and device opening, the superblock probe, the control server, the signal flag, the binary's command line | forbidden | `config`, `arena`, `backstore`, `clock`, `frontdoor`, `report`, `pool`, `daemon`, `server`, `device` (Linux) | `daemon_tests` 6, `review_daemon_tests` 9 |
| `crates/otap-ctl` 0.1.0 | `otapctl`: the command line to a request, the client call, text and JSON rendering, exit codes | forbidden | `args`, `render` | 5 unit tests, `ctl_tests` 2 (the binary against an in-process daemon) |
| `fuzz/` | cargo-fuzz targets `fuzz_superblock`, `fuzz_stripe_summary`, `fuzz_stream_frame` | none | own workspace, `libfuzzer-sys` 0.4 | type-checked only on this host |

Totals on 2026-10-14 after Step 5 and its review: 541 tests and 2 doctests, all passing
(section 5).

### 3.1 otap-format: what is on disk

Every structure is `#[repr(C)]`, little-endian, padding-free, covered by a
CRC-32C, and `const`-asserted for size and offset. Magics: `OTSB` superblock,
`OTIN` intent slot, `OTST` stripe header, `OTSF` stream frame, `OTCL` CP log
page, `OTCD` CP log directory page, `OTSR` snapshot root page.

| Structure | Bytes | Notes |
|---|---|---|
| `Superblock` | 4,096 | identity (`agg_id`, `owner_epoch`), counters (`cp_seq`, `cut_seq`, `cp_nonce`, `intent_seq_seen`, `opened_stamp`, `wall_ns`, `acked_bytes`), `run_state`, `slot`, `vol_count`, `write_active` and `nvfail` bitsets (1,024 volumes), roots (`freed_pending[2]`, `map_roots`, `seg_table`, `reloc_index`, `cp_log_head`, `snap_table`, `cluster_state`), `durable_log` header, `inbound[16]`, the open data segment (2.1), the geometry (2.2: stripe, segment, capacity), the catalogue and the stripe list roots (2.3), 2,332 reserved bytes |
| `VolTable` | 16,384 | `[VolState; 1024]`: per-volume flags (present, offline since 2.3), `acked_bytes`, `rollback_gen`, nvfail cause; covered by `Superblock::vol_table_crc` |
| `SuperblockSlot` | 20,480 | page plus table, written whole with FUA; four slots `A0 A1 B0 B1` ranked by `(owner_epoch, cp_seq, cut_seq)` |
| `IntentSlot` | 4,096 | two slots; `boot_write_active` unions the newer intent into the superblock's bitset |
| `StripeHdr` + `VolBitSet` + `[StripeDescriptor; nblocks]` | 64 + 128 + 16 n, padded to 4 KiB | the stripe summary; the stripe image is the chunk payload byte for byte |
| `MapEntry`, `PageIndexEntry` | 16 | `lseg:36 \| off:16 \| plen:12`, `birth_cp`, block CRC; 1,024 per 16 KiB page; a hole is an entry with a birth and no address |
| `MapRoot` | 64 | per-volume root with its index CRC and the CP it was checkpointed at |
| `StreamHeader` | 64 | the frame header, `hdr_crc` and `payload_crc` |
| `HoleRecord` | 16 | `vol`, `len`, `lba` |
| `SegEntry` | 32 | keyed by segment index: `phys`, geometry at birth, live bytes, `birth_cp`, `lap`, kind, state |
| `SnapRecord` | 64 | id, uuid, map root, deadlist, creation time, `birth_cp`, volume, kind, origin, pin flags |
| `CpLogRec`, `CpLogPage` | 32, 4,096 | 126 records per page behind a 64-byte header naming aggregate, CP, nonce and page position |
| `CpLogDirPage` (2.1) | 4,096 | 252 `RootRef`s behind a 64-byte header: the delta regions since the fold and the pages written (F12; since 2.3 in any CP, F18) |
| `SnapRootPage` (2.1) | 4,096 | the pinned `MapRoot`, the snapshot's identity, the deadlist reference (F13) |
| `CatalogRec` (2.3) | 512 | a volume's or a snapshot's name: kind, name length, flags, volume, snapshot uuid, creation time, 255 bytes of name, its own CRC; the region in canonical order (F16) |
| `CpStripe` (2.3) | 16 | `(pseg, idx, kind)` of a data stripe the commit wrote, ingest or relocation; the list in ascending order (F17) |

Golden vectors: `crates/otap-format/tests/vectors/v2.0/` (twelve images),
`v2.1/` and `v2.2/` (fourteen each) and `v2.3/` (sixteen) hold byte-exact images with BLAKE3 manifests whose hashes
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
  (`REQUIRED_OPCODES`, `REQUIRED_UBLK_FEATURES`); on Linux the probe runs
  on a private SQE128 ring.

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
  --flags` over `daemon`, `pool`, `volume`, `snapshot` and `io`; sizes with
  `K`, `M`, `G`, `T`; exit 0, 1 (refused), 2 (usage), 3 (unreachable).

## 4. Where this sits in the Phase C plan (doc 05, Part 8)

Week 1 of the §8.5 timeline, by column:

| Item | Status |
|---|---|
| E1: `otap-format` v2 frozen with every structure of §7.1, including the `geom`, `cluster_state`, `durable_log` and `inbound[]` reservations; proptest, golden vectors, layout asserts | done, with the F1 to F11 departures from the §7.1 sketch recorded in `07-engineering-decisions.md`; format 2.1 (F12 to F14) added the CP log directory, the snapshot root page and the table regions in Step 4 |
| E2: workspace, `otap-sys` boundary and lints | done |
| E2: `otap-stream` v1 frozen | done for the frame layer; the `TlsTcp` and `FileSink` transports are consumers of the crate and are not written |
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
| E2 (week 5): `otap-geom`, the attestation | the planner done; the attestation (section 7.3) not written |
| E1 (week 6): the ublk front door and `dbladed` | the front door done with one queue (E28); `dbladed` written in Step 5 (E38): the process, the hugetlbfs arena file (and a heap arena), the control socket, one ublk device per volume, the back-pressure curve; the systemd units, multi-queue, the reactor partitioning of the arena and `cut_within` are not |
| Exit tests of weeks 3 to 6 (dm-flakey, dm-log-writes, the null ublk device, fio) | need Linux; `scripts/linux-smoke.sh` is the fio and restart lane, written and not run |

Weeks 7 to 8 (Step 5):

| Item | Status |
|---|---|
| E1 (week 7): the cleaner, relocation of live blocks, the reserve | done (E37): candidates by live blocks and age, the exact-address check, relocation stripes with the old births, drained segments freed and discarded, the reserve and the admission rule, the full-pool pump; relocation tables not used |
| E2 (week 7): snapshot deletion, deadlists, the space a snapshot pins returned | done without deadlists (E36): the deletion compares against the neighbouring snapshots and the live map at the CP that deletes; pins and the replication base; rollback, resize, deletion and offline volumes beyond the plan |
| E1 (week 8): `WRITE_FIXED` stripes through the registered buffers | not done; the backstore stages `O_DIRECT` writes |
| `otapctl` (R56, a single node) and the catalogue | done (E38, F16): the lifecycle end to end over a file; the Linux run is the smoke script |

§8.4 test strategy: the format-freeze lane and the simulation lane exist,
the latter now covering the map, folds, snapshots and holes; the engine
also runs over a real file. Crash consistency on a kernel (dm-log-writes,
dm-flakey), the CI image and the loopback replication harness do not
exist. Milestone M1 ("formats frozen and the arena survives `kill -9`") is
met under the simulator; the physical re-attach test is a Linux item.

## 5. Gates, as run on 2026-10-14 after Step 5

On the development PC every cargo command is prefixed with
`export PATH="$HOME/.cargo/bin:$HOME/.cargo/mingw-tools/bin:$PATH"` (see the
Windows notes in `BUILDING.md`) and runs with `-j 3`; tests use `--test-threads=6`.

| Gate | Result |
|---|---|
| `cargo fmt --all -- --check`; `cargo fmt --manifest-path fuzz/Cargo.toml -- --check` | clean |
| `cargo clippy --workspace --all-targets -- -D warnings` (host) | clean |
| `cargo clippy --workspace --all-targets --target x86_64-unknown-linux-gnu -- -D warnings` | clean |
| `cargo clippy --manifest-path fuzz/Cargo.toml --no-default-features -- -D warnings` | clean |
| `RUSTDOCFLAGS="-D warnings" cargo doc --workspace --no-deps` | clean |
| `cargo test --workspace` | 541 tests and 2 doctests passed, 0 failed, 1 ignored (the vector writer), in 3 min 13 s |
| `PROPTEST_CASES=10000 cargo test --workspace` | all 541 tests and 2 doctests passed, 0 failed, 1 ignored, in 179 min 46 s (`step4_sim_tests` 53 min, `review_step5_sim_tests` 45 min, `map_tests` 19 min, `adversarial_sim_tests` 17 min, `sim_tests` 15 min, `step5_sim_tests` 13 min; every other suite under six minutes) |

Not runnable here: the Linux halves of `otap-sys`, `otap-backstore`,
`otap-ublk` and `otap-daemon` (type-checked only), the fuzz targets (need
nightly and cargo-fuzz), `scripts/linux-smoke.sh`, and every kernel lane
of §8.4.

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

## 7. Next: Step 6

Step 6 began on 2026-10-14 and was paused before the design; its state
and the order of the remaining work are in `step6/README.md`. The list
below is still the plan.

What doc 05 schedules next, in the order its dependencies allow:

1. **The first Linux run.** A Debian 13 host or VM (the plan's CI image:
   the Debian 13 cloud image under QEMU/KVM with huge pages reserved).
   `scripts/linux-smoke.sh` is written for it (the README has the steps):
   a loop device, `UringBackstore`, `dbladed`, one ublk device, dd and
   fio, a snapshot, a rollback, the cleaner, a `SIGTERM` restart and a
   `SIGKILL` restart with the hugetlbfs arena re-attaching. Then the
   dm-flakey and dm-log-writes lanes, `otap-sys`'s probe and a real
   `kill -9` loss count. Expect the run to find things in code that has
   never executed; every finding goes into `08`.
2. **What the daemon still lacks** (week 6 leftovers): the systemd units,
   `UBLK_F_USER_RECOVERY` re-attach of a device after a daemon crash,
   multi-queue devices and the reactor partitioning of the arena (E16's
   cell API), `cut_within`, `WRITE_FIXED` stripes through the registered
   buffers.
3. **The incremental tables.** The segment table is still written whole
   at every CP and a fold rewrites a dirty volume's whole index; the
   cleaner's scan compares against every snapshot tree. All three scale
   with the pool and are the next engine work.
4. **The attestation** (section 7.3) and the metadata device class.
5. **The transport** (week 11): `TlsTcp` and the `FileSink` behind
   `dump --since`, feeding `otap-stream` from the map's birth walk; the
   replication cursor that supersedes a replication base (E36).

What Step 6 needs from the owner: the Linux host, and the Step 6 brief;
answers to Part 9's D3 (the control plane's store, now with less to
persist), D5, D6 and D14 (the nvfail class table, the failover identity,
`nvfail_force_lag`) before the nvfail policy code.

## 8. Known limits, debts and risks

- **History starts at the Step 3 commit.** Everything before `aee54fc`
  exists only as this log describes it; the Step 1 and Step 2 states cannot
  be checked out.
- **Nothing kernel-facing has executed.** The ublk, io_uring, hugetlbfs and
  block-device code is verified against the 6.12 sources and type-checked
  for Linux only; the daemon's device lifecycle and Unix socket likewise.
  The geometry planner has run only against mocks. The smoke test exists
  and has not run.
- **The fuzz targets have never run.** They compile under
  `--no-default-features`; a nightly toolchain and cargo-fuzz are needed to
  execute them.
- **The 100,000-case merge gate has no CI to run it.** Locally every change
  runs at 10,000 cases.
- **A damaged snapshot's deletion releases nothing** (E36): a snapshot
  whose root page or index did not verify at boot can be deleted, but the
  blocks only it referenced stay counted live until a repair tool exists;
  the engine records an `Internal` event. A boot that rewinds below a
  snapshot's CP still loses it with the CP.
- **The engine boots 2.3 pools only** (E35). A 2.2 pool is refused with
  `OldLayout`; nothing was released at 2.2, so no upgrade is written.
- **Delta byte counts are raw image bytes.** `DeltaBegin.bytes` and
  `DeltaEnd.bytes` are sums of decoded chunk lengths; the Phase B codec work
  must keep that meaning.
- **Stream v1 has no transport.** `TlsTcp` and the `FileSink` behind
  `dump --since` are doc 05 week 11 items.
- **The golden-vector sets were regenerated once (2.0) and bumped three
  times (2.1, 2.2, 2.3).** Allowed only before a release; the procedure
  for any later change is in `BUILDING.md`.
- **The engine is one thread, and so is the control plane.** The reactor
  partitioning of the arena is a `dbladed` item (E16); the ublk front door
  serves one queue per device; the daemon answers one control request at a
  time and a long one (a rollback, a stop) delays the next; an idle daemon
  wakes every 200 µs to 2 ms, so a request on a device waits up to 2 ms.
- **The cleaner has limits** (E37). A segment above 80% live is never a
  candidate, so a pool whose segments are all mostly live can reach
  `NoSpace` with free space scattered inside them; the scan rereads whole
  segments and compares against every snapshot tree; a candidate the scan
  found uncleanable is skipped until a free changes the picture. The
  reserve is `max(8, n / 12)` segments, which on a tiny test pool is a
  large fraction of it (the cap at half the pool keeps data segments).
- **A heap arena dies with the process** (S5-1): under `--heap-arena` a
  daemon crash is a dirty boot that holds the write-active volumes under
  nvfail; only a hugetlbfs arena re-attaches.
- **Two boot-verification gaps under a trusted opaque barrier** (the Step 5
  review's hazards 8 and 9, E39): a block that already fails its CRC is
  relocated as it is, under its recorded CRC, so at an unclean boot the
  relocating CP looks torn and the boot rewinds past a sound CP; and the
  root page a relocation writes for a snapshot is not listed as the CP's
  own, so a lost one reads as a damaged snapshot rather than a torn CP.
  Both need the opaque barrier to lose a committed write; both are
  Step 6 items (verify a block before relocating it; list rewritten roots
  in the directory).
- **The catalogue and the snapshot table are rewritten whole** in every CP
  that changes them; with 1,024 volumes and 271 snapshots each that is
  over 270,000 records per naming CP, and a region that outgrows a segment
  needs a run of free segments. Step 6's incremental tables.
- **The `Io*` requests are a test aid, not a data path**: whole blocks as
  hexadecimal, 256 per request, serialized with every other control
  request.
- **A volume's index must fit a quarter of the pool, and 4 GiB.** `format` and
  `create_volume` enforce it (`MetadataTooLarge`); the one-segment bound
  of E14 is gone.
- **A flush-opaque backstore is trusted by default** (E21). An operator
  who needs the refusal sets `FlushPolicy::RequireFuaOrFlush`.
- **Arena damage is held, not repaired.** A damaged descriptor or block
  found at re-attach is dropped and the boot holds the write-active set;
  a block damaged while running reads as `Corrupt` on that block alone.
- **A permanently unreadable sector in the newest CP's regions stops the
  boot** (E30): a read error fails the boot instead of tearing the CP, so
  a transient error never discards a sound CP; the operator zeroes a bad
  sector to make the candidate torn and the boot rewind.
- **A damaged map page is contained, not repaired** (E32): its blocks read
  `Corrupt`, CPs commit around it, folds stop until the volume is
  repaired; there is no per-volume quarantine of a damaged live index yet
  (an index shared by both CPs that does not verify refuses the boot).
- **The segment table is written whole at every CP.** At 100 TiB with
  8 MiB segments that is 400 MiB per CP, and a fold rewrites the whole
  index of each dirty volume; both scale with the pool and are Step 6's
  incremental tables (section 7).
- **A discard in flight when the daemon is killed can still land** after
  the new process reuses the segment (the simulator completes discards at
  a daemon crash, so it cannot model this); the Linux run must measure
  it, and the boot may have to wait for the device's queue to drain.
- **Snapshot ids are monotonic between rewinds**, not across one: a boot
  that rewinds below a snapshot's CP loses the snapshot and its id is
  reused. The catalogue and the daemon key snapshots by uuid and name.
- **The front door drops `UBLK_IO_F_FUA`** by design (D2: a write is
  acknowledged when placed in the arena); a device that needs FUA
  semantics on the medium is a later policy.
- **No daemon recovery over `USER_RECOVERY` yet**: the device is created
  with the flag, and a tag whose fetch fails is retired; a restarted
  daemon creates new devices, so `/dev/ublkbN` numbers change across a
  crash, and the killed daemon's devices stay quiesced in the driver until
  a tool deletes them or the module is reloaded (a clean stop deletes
  them). The recovery re-attach and a reaper for stale devices are Step 6
  items.
- **The 10,000-case gate is slow.** Three hours on the development PC
  (179 minutes after Step 5, 116 after Step 4; the two Step 5 simulator
  suites account for the hour). The simulator's random walks and the map's
  multi-segment persistence walk dominate, and each is one property test
  that more test threads cannot shorten; the gate found real defects that
  256 cases missed, so it stays, but it is a background run.
