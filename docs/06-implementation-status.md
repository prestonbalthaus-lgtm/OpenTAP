# OpenTAP implementation status and handoff

Living document. Last updated 2026-10-13, after Phase C Step 3. Update it at
the end of every step and whenever section 7 ("Next") changes. Section 1 is
what a new session reads first; the rest is the detail behind it.

## 1. Where we are, in one screen

- The design is complete through round 3: `05-architecture-brainstorm-03.md`
  holds rulings R25 to R64, the trait contracts (Part 7), the Phase C plan
  (Part 8) and the fifteen owner decisions (Part 9).
- Implementation started on 2026-10-13 in `E:\Coding\OpenTAP`. Steps 1 to 3
  of the implementation briefs are done and every gate in section 5 is green:
  the on-disk format v2 and the stream frame header v1 (`otap-format`), the
  replication stream v1 (`otap-stream`), the kernel boundary (`otap-sys`),
  the three fuzz targets, and now the engine core: the write-cache arena
  (`otap-arena`), the dirty index with the epoch-flip cut (`otap-index`),
  the consistency-point engine with the `Backstore`, `FrontDoor`, `Clock`
  and `Net` traits, boot, torn-CP rewind and nvfail (`otap-cp`), and the
  deterministic simulator that proves crash consistency against a checker
  model (`otap-sim`).
- The engine runs only under the simulator so far: there is no real
  `Backstore` (md, a device), no ublk front door, no daemon, no radix map
  (the whole map is written at every CP), no cleaner for data segments, no
  snapshots and no transport. Step 4 is the next brief (section 7).
- Step 3 closed with an independent adversarial review that found 23
  defects, among them real durability holes (precise-mode acknowledgements
  against an unlanded superblock, rewinds with a re-attached arena,
  volumes and clears that served before they were durable, unchecked arena
  CRCs). All are fixed and pinned (`08-engineering-log.md`, Step 3 entry;
  decisions E16 to E20). One item is refused rather than fixed: a
  flush-opaque backstore, which needs an owner decision (E19).
- The tree is not under version control (no `.git` as of 2026-10-13).
  `08-engineering-log.md` is the only history. Initialising a repository is
  the first recommendation to the owner.
- Owner decisions D1 to D15 have no recorded answers. The code depends on them
  only through the reservations listed in section 6.
- The development PC runs Windows. Everything Linux-specific is type-checked
  against `x86_64-unknown-linux-gnu` and has never executed. The first run on
  a Debian 13 host will find things; budget for it.

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
| `07-engineering-decisions.md` | the implementation decision log: where the code departs from the design or a brief, and why | living |
| `08-engineering-log.md` | dated record of what happened: steps, reviews, defects, gate results, lessons | living, append only |
| `09-working-agreements.md` | how the work is done: roles, definition of done, code and test rules, build etiquette, documentation rules | living |
| `10-phase-c-step-briefs.md` | the owner's step briefs, verbatim, each with a note on how it was read | append per step |
| `../README.md` | build, test and layout guide, including the Windows host notes | kept current with the tree |
| `../crates/otap-format/src/lib.rs` | the layout specification and format rulings F1 to F11; the code is the authority | frozen at format 2.0, stream header 1.0 |

## 3. The workspace as built

One Cargo workspace (`Cargo.toml` at the root), edition 2021, `rust-version`
1.89, built and tested with stable 1.99. `fuzz/` is excluded from the
workspace because it needs nightly and links libFuzzer.

| Crate | Role | `unsafe` | Modules | Tests |
|---|---|---|---|---|
| `crates/otap-format` 0.1.0 | on-disk format v2 and the frame header v1: zerocopy views, CRC-32C sealing and verification, the superblock ring, stripe summaries, map pages, the Step-2 records; no I/O, no policy | forbidden | `constants`, `types`, `checksum`, `superblock`, `stripe`, `map`, `stream`, `records` | `format_tests` 53 (+1 ignored vector writer), `records_tests` 9, `adversarial_tests` 49, 1 doctest |
| `crates/otap-stream` 0.1.0 | stream v1: typed frames, the codec, the landing cursor and its page, credit windows; no sockets | forbidden | `frame`, `codec`, `cursor`, `flow` | `stream_tests` 13, `adversarial_stream_tests` 62 |
| `crates/otap-sys` 0.1.0 | the kernel boundary: hugetlbfs arenas, the ublk 6.12 UAPI, io_uring buffer registration and cloning, the capability probe; Linux implementations behind `cfg`, portable fallbacks | five blocks, all Linux-only, each with a `// SAFETY:` proof | `hugetlb`, `ublk`, `uring`, `probe` | `sys_tests` 33, 1 doctest |
| `crates/otap-arena` 0.1.0 (layout 1.1) | the write-cache arena over `otap-sys` memory: an immutable sealed header, a descriptor table whose every transition is one sealed-word store, 4 KiB slots; runs with lap counters; `Free`/`Filled`/`Placed`/`Committed`; re-attach after `kill -9` with data and descriptor CRCs verified and damage reported | forbidden | one module | `arena_tests` 10, `adversarial_arena_tests` 12 |
| `crates/otap-index` 0.1.0 | per-volume dirty extents (split, trim, coalesce; later insert wins; stale sequences refused), generations, the cut over `arc-swap` with a grace wait, the frozen queue | forbidden | one module | `index_tests` 7, `adversarial_index_tests` 13 |
| `crates/otap-cp` 0.1.0 | the four traits; the engine: write and read paths, intents, the CP state machine (staging, metadata, flush, commit A then B), boot with rewind, nvfail, durable-before-serving volumes and clears, a full pool, metadata recycling, clean shutdown; the disk layout; the segment table; the in-memory block map and its on-disk image | forbidden | `traits`, `layout`, `segments`, `blockmap`, `boot`, `engine` | tested through `otap-sim` (57 tests) |
| `crates/otap-sim` 0.1.0 | fake device (pending, cached, durable; seeded completion order; sector tears), fake client, virtual clock, loopback net, the checker model (issued writes, settled after each recovery), the harness (one write at a time or several in flight) | forbidden | `backstore`, `frontdoor`, `clock`, `net`, `model`, `harness`, `rng` | `sim_tests` 15, `adversarial_sim_tests` 42, each with a random-crash property test |
| `fuzz/` | cargo-fuzz targets `fuzz_superblock`, `fuzz_stripe_summary`, `fuzz_stream_frame` | none | own workspace, `libfuzzer-sys` 0.4 | type-checked only on this host |

Totals on 2026-10-13 after Step 3 and its review: 318 tests and 2 doctests,
all passing. The 10,000-case property gate takes about three quarters of an
hour on the development PC, most of it the two simulator random walks (each
about 15 minutes per 10,000 scenarios) and the arena's two ring properties.

### 3.4 otap-cp and otap-sim: what the engine does

- A write reserves a slot run, copies in, computes a CRC per block, takes a
  sequence number, inserts into the active generation, marks the run
  `Placed`, and is acknowledged (after an intent slot in precise mode when
  the volume was idle). Blocks it supersedes inside the active generation
  release their slots at once.
- A cut is triggered by the timer, the arena at half, back to back, a
  request, shutdown or boot, and only when no generation is already waiting.
  The CP writes full stripes (summary page then data, assembled in memory),
  then the map image and the segment table packed into whole stripes of the
  open metadata segment, then a flush, then the superblock at `A` and at
  `B`; the map, the generation and the slots change only after `B` lands.
- Boot ranks the ring, loads the roots of the newest candidate, verifies
  every stripe that CP wrote by summary CRC and identity, rewinds to the
  next candidate on failure, re-attaches the arena when it can (verifying
  every descriptor and block and replaying the placed writes) and holds the
  boot's write-active set under nvfail whenever the boot is dirty and the
  arena did not come back whole, or the boot rewound; it commits once
  before serving. Volumes the control plane lists and the disk lacks are
  created with that commit.
- A write is acknowledged without an intent only to a volume that is
  durably active now and in the set of every superblock slot built and not
  yet landed; a volume created at runtime and a cleared nvfail serve only
  once their CP commits; a full pool refuses writes with `NoSpace`, serves
  reads, and metadata segments are recycled so empty CPs cannot exhaust it.
- The simulator's checker knows every issued write from the data itself and
  checks, after every recovery, that the image is the state after the
  issued writes up to one operation number, that no acknowledged write is
  lost except after a power cut, and that nvfail holds exactly what the
  rules say; the committed image alone is checked to be a prefix after
  every operation.

### 3.1 otap-format: what is on disk

Every structure is `#[repr(C)]`, little-endian, padding-free, covered by a
CRC-32C, and `const`-asserted for size and offset. Magics: `OTSB` superblock,
`OTIN` intent slot, `OTST` stripe header, `OTSF` stream frame, `OTCL` CP log
page.

| Structure | Bytes | Notes |
|---|---|---|
| `Superblock` | 4,096 | identity (`agg_id`, `owner_epoch`), counters (`cp_seq`, `cut_seq`, `cp_nonce`, `intent_seq_seen`, `opened_stamp`, `wall_ns`, `acked_bytes`), `run_state`, `slot`, `vol_count`, `write_active` and `nvfail` bitsets (1,024 volumes), roots (`freed_pending[2]`, `map_roots`, `seg_table`, `reloc_index`, `cp_log_head`, `snap_table`, `cluster_state`), `durable_log` header, `inbound[16]`, 2,396 reserved bytes |
| `VolTable` | 16,384 | `[VolState; 1024]`: per-volume flags, `acked_bytes`, `rollback_gen`, nvfail cause; covered by `Superblock::vol_table_crc` |
| `SuperblockSlot` | 20,480 | page plus table, written whole with FUA; four slots `A0 A1 B0 B1` ranked by `(owner_epoch, cp_seq, cut_seq)` |
| `IntentSlot` | 4,096 | two slots; `boot_write_active` unions the newer intent into the superblock's bitset |
| `StripeHdr` + `VolBitSet` + `[StripeDescriptor; nblocks]` | 64 + 128 + 16 n, padded to 4 KiB | the stripe summary; the stripe image is the chunk payload byte for byte |
| `MapEntry`, `PageIndexEntry` | 16 | `lseg:36 \| off:16 \| plen:12`, `birth_cp`, block CRC; 1,024 per 16 KiB page |
| `MapRoot` | 64 | per-volume root with its index CRC |
| `StreamHeader` | 64 | the frame header, `hdr_crc` and `payload_crc` |
| `HoleRecord` | 16 | `vol`, `len`, `lba` |
| `SegEntry` | 32 | keyed by segment index: `phys`, geometry at birth, live bytes, `birth_cp`, `lap`, kind, state |
| `SnapRecord` | 64 | id, uuid, map root, deadlist, creation time, `birth_cp`, volume, kind, origin, pin flags |
| `CpLogRec`, `CpLogPage` | 32, 4,096 | 126 records per page behind a 64-byte header naming aggregate, CP, nonce and page position |

Golden vectors: `crates/otap-format/tests/vectors/v2.0/` holds twelve
byte-exact images and a BLAKE3 manifest whose hash is pinned in
`format_tests.rs`. The set was extended once, on 2026-10-13, when the Step-2
records were added; from now on any layout change is a minor bump plus a new
vector directory (README, "Golden vectors").

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
  decoder, and on Linux the control and character devices.
- `uring`: the opcode table, `FixedBufferLayout` (1 GiB cuts), and on Linux
  `RegisteredBuffers`, buffer-table cloning and the user-copy read SQE.
- `probe`: `OpcodeSet`, `Allowlist` and the v0.1 requirement check
  (`REQUIRED_OPCODES`, `REQUIRED_UBLK_FEATURES`); on Linux the probe runs
  on a private SQE128 ring.

## 4. Where this sits in the Phase C plan (doc 05, Part 8)

Week 1 of the §8.5 timeline, by column:

| Item | Status |
|---|---|
| E1: `otap-format` v2 frozen with every structure of §7.1, including the `geom`, `cluster_state`, `durable_log` and `inbound[]` reservations; proptest, golden vectors, layout asserts | done, with the F1 to F11 departures from the §7.1 sketch recorded in `07-engineering-decisions.md` (`geom` lives in `SegEntry`; the `cluster_state` root, the `durable_log` header and `inbound[16]` are in the superblock) |
| E2: workspace, `otap-sys` boundary and lints | done |
| E2: `otap-stream` v1 frozen | done for the frame layer; the `TlsTcp` and `FileSink` transports are consumers of the crate and are not written |
| E2: `Backstore`, `FrontDoor`, `Clock`, `Net` traits | done (Step 3, `otap_cp::traits`, E13) |
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

Weeks 3 to 5 in part: the CP engine's commit protocol, triggers and dirty-boot
evaluation (week 5's "triggers, deferred reuse, dirty-boot summary
verification") exist in `otap-cp` with the torn-superblock and torn-stripe
cases covered by the simulator instead of dm-flakey; deferred reuse and the
throttle curve do not exist because nothing frees a segment yet.

§8.4 test strategy: the format-freeze lane and the simulation lane exist.
Crash consistency on a kernel (dm-log-writes, dm-flakey), the CI image and
the loopback replication harness do not. Milestone M1 ("formats frozen and
the arena survives `kill -9`") is met under the simulator; the physical
re-attach test is a Linux item.

## 5. Gates, as run on 2026-10-13

On the development PC every cargo command is prefixed with
`export PATH="$HOME/.cargo/bin:$HOME/.cargo/mingw-tools/bin:$PATH"` (see the
README's Windows notes) and runs with `-j 3`; tests use `--test-threads=6`.

| Gate | Result |
|---|---|
| `cargo fmt --all -- --check`; `cargo fmt --manifest-path fuzz/Cargo.toml -- --check` | clean |
| `cargo clippy --workspace --all-targets -- -D warnings` (host) | clean |
| `cargo clippy --workspace --all-targets --target x86_64-unknown-linux-gnu -- -D warnings` | clean |
| `cargo clippy --manifest-path fuzz/Cargo.toml --no-default-features -- -D warnings` | clean |
| `RUSTDOCFLAGS="-D warnings" cargo doc --workspace --no-deps` | clean |
| `cargo test --workspace` | 318 passed, 0 failed, 1 ignored, 2 doctests passed |
| `PROPTEST_CASES=10000` | the whole workspace, after the review's fixes; the result and timing are in the Step 3 review entry of `08-engineering-log.md` (the simulator's seeds differ per run, so the walks are repeated) |

Not runnable here: the Linux halves of `otap-sys` (type-checked only), the
fuzz targets (need nightly and cargo-fuzz), and every kernel lane of §8.4.

## 6. What the code assumes about the owner decisions

No answer to D1 to D15 is recorded. The implementation so far touches them
only here:

- **D1 (v0.1 scope).** Nothing for Fibre Channel or S3 has been written.
  Consistent with either answer.
- **D2 (the durable log).** The superblock carries a 64-byte `DurableLogHdr`
  whose `NONE` state is verified; that is the "reserved in v0.1's format"
  option. Choosing the alternative (the log inside the twelve weeks) changes
  Step 3 scope, not the format.
- **D3 (the control plane).** The `cluster_state` root is reserved in the
  superblock. No `StateStore` code exists.
- **D5, D6, D14 (nvfail).** The `nvfail` bitset and the per-volume cause in
  the volume table exist; no policy code exists.
- **D9 (`--write-journal`).** A `Backstore` concern; nothing written.
- The remaining decisions have no footprint in the code yet.

Decisions the architect took without an owner answer because the code needed
one (the Rust floor, F1 to F11, the stream rules) are in
`07-engineering-decisions.md`; each is reversible at the cost stated there.

## 7. Next: Step 4

What doc 05 schedules next, in the order its dependencies allow:

1. **A real `Backstore`** (week 3, E1): md and a single device, geometry
   from sysfs and the probe, `WRITE_FIXED` stripes, the empty `FSYNC` then
   the `RWF_DSYNC` superblock, on io_uring through `otap-sys`. This is the
   first Linux-only engine code; it needs a Debian 13 host or VM and the
   dm-log-writes and dm-flakey lanes of §8.4.
2. **`otap-map`** (week 4): the radix map, the CP log as its journal, the
   checkpoint, per-read CRC, holes with birth, `diff`; it replaces the
   whole-map image `otap-cp` writes at every CP, behind the same superblock
   roots (E14). The `otap-cp` traits module moves to its own crate if the
   dependency cycle appears (E13).
3. **The rest of `otap-cp`** (week 5): deferred reuse, the back-pressure
   delay curve, `cut_within`, the forced dirty-degraded assembly test on
   md; and `otap-geom` and the attestation.
4. **The ublk front door and `dbladed`** (week 6): the first real
   `FrontDoor`; multi-reactor access to the arena needs the `otap-sys`
   cell API noted in E16.

What Step 4 needs from the owner: the Step 4 brief; a Debian 13 host or VM
(the plan's CI image is the Debian 13 cloud image under QEMU/KVM with 64
huge pages), without which the real backstore and front door cannot be
run; answers to D2 (the durable log) and D9 (`--write-journal`) before the
md work starts; and a decision on flush-opaque backstores (E19: a
three-CP ring, or a post-commit flush discipline) before the real
`Backstore` is pointed at a RAID virtual drive.

## 8. Known limits, debts and risks

- **No version control.** One tree on one disk, no history beyond the log.
- **Nothing kernel-facing has executed.** The ublk, io_uring and hugetlbfs
  code is verified against the 6.12 sources and type-checked for Linux only.
- **The fuzz targets have never run.** They compile under
  `--no-default-features`; a nightly toolchain and cargo-fuzz are needed to
  execute them.
- **The 100,000-case merge gate has no CI to run it.** Locally every change
  runs at 10,000 cases.
- **`libublk` 0.4.8 is pinned in the workspace but used by no crate.**
  `otap-sys` encodes the UAPI itself. Decide in Step 3 whether the front door
  uses the crate or the pin is dropped.
- **Snapshot names are not on disk** (F11). The control plane must persist
  them keyed by snapshot uuid; that is an obligation on the `StateStore`.
- **Delta byte counts are raw image bytes.** `DeltaBegin.bytes` and
  `DeltaEnd.bytes` are sums of decoded chunk lengths; the Phase B codec work
  must keep that meaning.
- **Stream v1 has no transport.** `TlsTcp` and the `FileSink` behind
  `dump --since` are doc 05 week 11 items.
- **The golden-vector set was regenerated once.** Allowed only before a
  release; the procedure for any later change is in the README.
- **The engine is one thread and writes the whole map at every CP.** Both
  are Step 3 simplifications recorded in E14 and E16; the radix map and
  the reactor partitioning of the arena are Step 4 items.
- **Nothing frees a data segment.** Metadata segments are recycled, and
  data allocations keep two segments in reserve for boot and shutdown
  commits, but without the cleaner a pool fills with data; the engine then
  refuses writes with `NoSpace`, keeps serving reads, boots, and a
  shutdown stops without the clean marker. The simulator's pools are sized
  so that random scenarios never get there; named tests cover the full
  pool.
- **The CP metadata must fit one segment.** With the whole-map image of
  E14 that bounds the mapped data per pool (about 8 GiB of blocks per
  64 MiB segment); `format` and `create_volume` enforce it. `otap-map`
  lifts it.
- **Flush-opaque backstores are refused** (E19) until the owner decides.
- **Arena damage is held, not repaired.** A damaged descriptor or block
  found at re-attach is dropped and the boot holds the write-active set;
  a block damaged while running reads as `Corrupt` on that block alone.
- **The 10,000-case gate is slow.** About three quarters of an hour for the
  workspace, dominated by the two simulator random walks; it found real
  defects that 256 cases missed, so it stays, but it is a background run.
