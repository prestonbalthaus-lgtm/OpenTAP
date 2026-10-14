# OpenTAP engineering decisions

Living log of the decisions the implementation made that the design documents
(`05-architecture-brainstorm-03.md` and its predecessors) or the step briefs
did not make, or made differently. Entries are numbered E01 onward and never
renumbered; a reversed decision gets a new entry that points back. The format
rulings F1 to F11 are owned by the code (`crates/otap-format/src/lib.rs`) and
only summarised here; the stream rules S1 to S10 are owned by this file and
enforced by the tests named under each.

Each entry: date, status, context, decision, consequences, where it is
enforced.

---

## E01. The Rust floor is 1.89, not the brief's 1.82

2026-10-13. Accepted.

- **Context.** The Step-1 brief asked for "1.82+". The pinned `crc-fast`
  1.10.0 declares `rust-version = 1.89` and `proptest` 1.12.0 declares 1.88.
- **Decision.** `rust-version = "1.89"` in the workspace; the tree is built
  and tested with stable 1.99. `rust-toolchain.toml` selects `stable` with
  `rustfmt` and `clippy`.
- **Consequences.** Any host older than 1.89 cannot build. Reversing it means
  replacing `crc-fast` (the `crc32c` 0.6.8 crate was the brief's alternative)
  and downgrading `proptest`.
- **Enforced by** `Cargo.toml` (`[workspace.package]`, with the reason in a
  comment).

## E02. Dependency pins and features

2026-10-13. Accepted.

- **Context.** Round 3 fact check C39 verified a 2026-10-12 snapshot of
  crate versions; the briefs pinned them.
- **Decision.** `zerocopy` 0.8.62 with `derive`; `crc-fast` exactly 1.10.0
  with `default-features = false, features = ["std"]` (the default set also
  exports a C FFI surface and a `no_std` panic handler); `blake3` 1.8.7;
  `bytes` 1.10; `proptest` 1.12.0; `io-uring` 0.7.15, `libublk` exactly
  0.4.8, `rustix` 1.1.5 and `memmap2` 0.9.11 declared at the workspace level
  and pulled in only under `cfg(target_os = "linux")` by `otap-sys`;
  `libfuzzer-sys` 0.4 in the fuzz package only. `Cargo.lock` is the authority
  for the resolved graph; a pin is bumped only with the change that needs it.
- **Consequences.** `libublk` is pinned but used by no crate: `otap-sys`
  encodes the ublk UAPI itself (E08). Step 3 decides whether the front door
  uses the crate or the pin goes.
- **Enforced by** `Cargo.toml`, `crates/*/Cargo.toml`, `fuzz/Cargo.toml`.

## E03. Lints and profiles

2026-10-13. Accepted.

- **Decision.** Workspace lints: `missing_docs` warn, `unsafe_op_in_unsafe_fn`
  deny, `unused_qualifications` warn, `rust_2018_idioms` warn;
  `clippy::all` warn, `clippy::undocumented_unsafe_blocks` deny,
  `clippy::missing_safety_doc` deny. The gates run clippy with
  `-D warnings`, so every warning above is an error in practice.
  `otap-format` and `otap-stream` are `#![forbid(unsafe_code)]` and
  `#![deny(missing_docs)]`. Profiles: dependencies at `opt-level = 3` even in
  dev builds (the CRC kernels and proptest shrinking dominate test time);
  release is `lto = "fat"`, `codegen-units = 1`, `debug = 1` and
  `overflow-checks = true`, because a storage engine traps on overflow rather
  than wrapping.
- **Enforced by** `Cargo.toml` `[workspace.lints]` and `[profile.*]`; the
  crate roots.

## E04. The format rulings F1 to F11 (summary; the code is the authority)

2026-10-13. Accepted. Full text with the reasoning in
`crates/otap-format/src/lib.rs`, "Format rulings".

| Ruling | One line |
|---|---|
| F1 | The superblock page is exactly 4 KiB; per-volume state lives in a 16 KiB volume table inside the same 20 KiB slot, covered by `vol_table_crc`; `MAX_VOLS == 1024`. |
| F2 | Map entries use the round-3 packing `lseg:36 \| off:16 \| plen:12`; logical segment 0 is reserved; `addr == 0` with a non-zero birth is a hole, with a zero birth unmapped. |
| F3 | A stripe summary is `[StripeHdr][VolBitSet][StripeDescriptor; nblocks]` padded to 4 KiB; the bitmap is derived from the descriptors; the stripe image is the chunk payload byte for byte; a stripe is at most 8 MiB. |
| F4 | The descriptor's 2-bit kind is the block kind (`Data`, `Hole`); the stripe kind is in the header once. |
| F5 | CP numbers are `u64` in headers and `u32` as `birth_cp`; every header CP is verified against `MAX_CP_SEQ = u32::MAX`; CP 0 is mkfs, so a zero birth means never written. |
| F6 | The superblock carries both `cut_seq` and `cp_nonce`. |
| F7 | Ring protocol: slots `A0 A1 B0 B1`, commit N writes `A(N mod 2)` then `B(N mod 2)`; a reader ranks whole verified slots by `(owner_epoch, cp_seq, cut_seq)`, A wins ties, a newer major is a hard stop. |
| F8 | A volume index is a dense per-aggregate index below 1024, checked everywhere; raising it is a format change. |
| F9 | A hole descriptor's data slot is present and zero (CRC `0x98f9_4189`); a map hole carries CRC 0. |
| F10 | Narrowings that change no size: `nblocks` is `u16`; `PageIndexEntry` carries `page_crc`; headerless pages carry no `ver`; the frame header lives in `otap-format`. |
| F11 | Step-2 records: `SegEntry` (32 B) keyed by position with `phys` and the geometry at birth; `SnapRecord` (64 B) without a name; `CpLogRec` (32 B) with the packed map address; `CpLogPage` 4 KiB with 126 records; physical segment 0 reserved; the v2.0 vectors extended once on 2026-10-13. |

- **Consequences worth repeating.** Snapshot names must be persisted by the
  control plane keyed by uuid. The 20-bit descriptor volume field is wider
  than the 1024 bound on purpose. Compression in v2 can only save wire bytes,
  because every block occupies a 4 KiB slot on disk.

## E05. Golden vectors are the format's contract

2026-10-13. Accepted.

- **Decision.** `crates/otap-format/tests/vectors/v2.0/` holds byte-exact
  images of every structure with a BLAKE3 manifest; the manifest hash is
  pinned in `format_tests.rs` and the decode lane reads the files on every
  run. A semantic change needs a new vector directory and a minor bump in the
  same commit; the old directory stays so N decodes N-1. The set was
  regenerated once, on 2026-10-13, when the Step-2 records were added, which
  is allowed only before a release has written the format.
- **Enforced by** `format_tests.rs` (the vectors lane and the pinned hash),
  `.gitattributes` (`*.bin binary`), README "Golden vectors".

## E06. Stream rules S1 to S10

2026-10-13. Accepted. These are the rules of `otap-stream` v1 that go beyond
the §7.3 sketch of doc 05. Every one is pinned by `stream_tests.rs` or
`adversarial_stream_tests.rs`.

- **S1. The header is the contract.** Kinds 1 to 12 only; the only flag bits
  are the codec; `wire_len` and `raw_len` at most 8 MiB (`MAX_WIRE_PAYLOAD
  == MAX_STRIPE_BYTES`); `cp_seq <= MAX_CP_SEQ`; `base <= cp_seq`;
  `raw_len == wire_len` for a raw payload. The header has no reserved bytes,
  so new kinds, codecs and flags are negotiated in `Hello` first.
- **S2. Bodies are canonical.** Reserved bytes are zero; enumerations hold
  defined values; a field an event or mode does not use is zero, nil or
  empty, and a refusal names that field; `chunk_idx` is zero on every frame
  but a chunk and below 2^24 on one; sparse ranges in a `HelloAck` are
  sorted, disjoint, above `landed_contig` and inside `chunks`.
- **S3. The builder refuses what the parser refuses, and never clamps.**
  `Frame::validate`, `Encoder::header`, `encode_into`, `encode` and
  `Cursor::to_page` (which returns a `Result` for this reason) refuse exactly
  what `Frame::decode` and `Cursor::from_page` refuse, so two different
  values never share one encoding and nothing a peer would drop is sent.
  `Cursor::new` caps its `credits` argument as a constructor convenience; a
  public field set over a bound afterwards is refused at the boundary.
- **S4. Bounds.** `MAX_DELTA_CHUNKS = 2^24` (a 2 MiB bitmap for the largest
  delta); `MAX_WIRE_RANGES = 4096` per `HelloAck` (a longer fragmentation
  is truncated to the first 4,096 ranges; the chunks beyond are resent,
  which is harmless); `MAX_NAME_LEN = 255`; `CursorPage` keeps 500 ranges;
  `CREDIT_BYTES = 4096`, `INITIAL_GRANT_BYTES = 64 MiB`,
  `MAX_STAGING_BYTES = 2 GiB`, so `MAX_CREDITS = 524,288`.
- **S5. Decoder error semantics.** A bad header (magic, major, CRC, kind,
  codec, flags, bounds) poisons the decoder: it repeats that error on every
  call, drops further input, keeps what it refused for diagnostics, and
  reports zero bytes needed. A bad payload CRC consumes the frame, reports
  `PayloadCrc { kind, chunk_idx, cp_seq }` and leaves the decoder in sync;
  the receiver answers `Abort(Corrupt, idx)` for a chunk and
  `Abort(Corrupt, 0)` for anything else. `drain` never loses a decoded
  frame: an error after some frames is held for the next call, and
  `needed()` is zero while one is held.
- **S6. Delta byte counts are raw image bytes.** `DeltaBegin.bytes` and
  `DeltaEnd.bytes` are sums of the chunks' `raw_len`, bounded between
  `chunks` smallest and `chunks` largest stripe images, and never change
  under a codec. `DeltaEnd.digest` is BLAKE3 over the chunk images in index
  order.
- **S7. Codecs are negotiated, not assumed.** `Hello.capabilities` reserves
  `CAP_CODEC_LZ4` and `CAP_CODEC_ZSTD`; v1 sends raw payloads and passes a
  chunk that already carries a codec through with its own `raw_len`.
  Compression itself is Phase B.
- **S8. No sockets.** `otap-stream` works over memory buffers and generic
  byte sources and sinks; the transport (`TlsTcp`, `FileSink`) is a consumer.
- **S9. The error type is `#[non_exhaustive]`.** Variants may be added in a
  minor; the unused `Poisoned` variant was removed on 2026-10-13 (a poisoned
  decoder repeats the error that poisoned it instead).
- **S10. Nothing proportional to an untrusted count is allocated before the
  count is checked.** Record counts are checked against the payload length,
  the payload bound is checked at the header before space is reserved, and
  the encoder checks the frame and the header context before it reserves
  room for a frame. The adversarial suite proves this with a counting
  global allocator.

## E07. The frame header lives in `otap-format`

2026-10-13. Accepted (part of F10).

- **Decision.** The 64-byte `StreamHeader` and `HoleRecord` are defined and
  frozen with the disk layouts, so one crate owns every byte layout and one
  vector set covers both. `otap-stream` builds its bodies on top.
- **Consequences.** A wire change is a `STREAM_MINOR` bump in `otap-format`.

## E08. `otap-sys` policy and the verified kernel facts

2026-10-13. Accepted.

- **Decision.** `otap-sys` is the only crate that may contain `unsafe`. Five
  blocks exist, all Linux-only, each as small as the call it wraps and
  opened with a `// SAFETY:` proof (inventory in the crate root). No
  `unsafe fn` is exported or exists internally. The Linux implementation is
  behind `cfg(target_os = "linux")` with a fallback behind
  `cfg(not(target_os = "linux"))` under one public API; items whose
  signatures name Linux types exist on Linux only. The ublk 6.12 UAPI is
  encoded as data (constants, `repr(C)` structures with size asserts,
  command builders) rather than through `libublk`; every fetch carries
  `addr = 0` under `UBLK_F_USER_COPY`; ublk control commands are
  `IORING_OP_URING_CMD` on an SQE128 ring; the startup `Allowlist` is the
  only place an opcode or ublk feature is named.
- **Facts verified against the v6.12 sources** by the sys worker, with the
  headers saved beside the work: the `_IOC` encoding
  (`UBLK_U_CMD_START_DEV = 0xC020_7506`, `UBLK_U_IO_FETCH_REQ =
  0xC010_7520`), the user-copy offset layout and its decoder,
  `sizeof(struct ublk_params) == 112`, `SZ_1G` per fixed buffer and
  `IORING_MAX_REG_BUFFERS = 1 << 14`, `IO_URING_F_SQE128` for ublk control
  commands, the `EBUSY`/`ENXIO` outcomes of buffer-table cloning,
  `HUGETLBFS_MAGIC = 0x958458f6`, and that the zero-copy flag is masked off
  in v0.1.
- **Consequences.** Nothing kernel-facing has executed yet; the first Linux
  run is the real test. `libublk` stays pinned but unused until Step 3
  decides (E02).
- **Enforced by** the workspace lints (E03), `crates/otap-sys/src/lib.rs`,
  `sys_tests.rs`, and the Linux-target clippy gate.

## E09. Test conventions

2026-10-13. Accepted.

- **Property tests** use `proptest`; the local gate is
  `PROPTEST_CASES=10000`, the documented merge gate is 100,000 (doc 05
  §8.4), and no CI exists yet to run it. `prop_assert!` always carries a
  message: without one, a condition containing braces (a struct pattern in
  `matches!`) is misread as a format string and fails to compile.
- **Adversarial suites** are written by an independent agent per crate
  (`adversarial_tests.rs`, `adversarial_stream_tests.rs`), named after the
  behaviour they pin, and every defect they find is fixed in the sources and
  kept as a test; a test is never weakened to pass. The stream suite carries
  a counting global allocator for S10.
- **Golden vectors and the N/N-1 lane** (E05) run on every `cargo test`.
- **Fuzz targets** compile under `--no-default-features` on any host and run
  under nightly `cargo fuzz` only.
- **Every gate** is listed in `09-working-agreements.md` and recorded per
  step in `06-implementation-status.md`.

## E10. Build etiquette on the development PC

2026-10-13. Accepted. The PC has 6 cores and 12 threads and has frozen under
parallel agent builds before. One cargo process at a time; `-j 3` for the
lead, `-j 2` for a worker; `--test-threads=6`; no two heavy jobs at once.
Details and the PATH prefix are in `09-working-agreements.md` and the README.

## E11. How a departure from the design is recorded

2026-10-13. Accepted.

- A layout decision becomes an F-ruling in `crates/otap-format/src/lib.rs`
  and a row in E04.
- A wire or protocol decision becomes an S-rule in E06.
- Anything else (toolchain, dependencies, policy, process) becomes a new
  E-entry here.
- The dated event (who found what, what was fixed) goes to
  `08-engineering-log.md`; the resulting state goes to
  `06-implementation-status.md`.

## E12. Decisions deferred to Step 3

2026-10-13. Partly closed by E13 to E18 (the traits' crate); the rest open.

- ~~The crate that holds the `Backstore`, `FrontDoor`, `Clock` and `Net`
  traits~~ (closed: E13).
- Whether the ublk front door uses `libublk` or the `otap-sys` encodings
  (E02, E08).
- The `TlsTcp` and `FileSink` transports (doc 05 week 11) and how they
  consume `otap-stream` and the `Net` trait.
- Where snapshot names live in the `StateStore` (F11 consequence).
- Whether `Cursor::new` should also return a `Result` once the engine
  constructs cursors (S3).

## E13. The four system traits live in `otap-cp`, shaped for a reactor

2026-10-13. Accepted (Step 3 brief: "otap-cp ... including the Backstore,
FrontDoor, and Clock traits").

- **Decision.** `otap_cp::traits` defines `Backstore` (tagged asynchronous
  writes: stripe, metadata, flush, FUA superblock commit, FUA intent;
  synchronous block reads; `poll` for completions; every address a 4 KiB
  block number), `FrontDoor` (fetch a request, copy a write's data into the
  arena run, copy a read out, complete with a status and the write's
  sequence number), `Clock` (`mono_ns`, `wall_ns`) and `Net` (ordered
  reliable byte chunks to named endpoints with `Busy` back-pressure).
  `Busy` means "nothing was done, retry on a later tick". The engine is
  driven by `tick()`, never by a thread it owns, so the simulator controls
  every interleaving.
- **Consequences.** `otap-arena` and `otap-index` need no trait (memory
  comes from `otap-sys`, the index is pure). A later crate that needs
  `Backstore` and that `otap-cp` must depend on (`otap-map`, week 4) would
  create a cycle; the traits module then moves to its own crate with the
  same contents. `dbladed` implements the traits on io_uring, ublk and the
  sockets; `otap-sim` fakes all four.
- **Enforced by** `crates/otap-cp/src/traits.rs` and the simulator's fakes.

## E14. Recovery reads exact roots, so no written-extent field is needed

2026-10-13. Accepted.

- **Context.** Segments are log-structured and reused, so a torn or
  abandoned CP leaves stale stripes and pages behind, some with a `cp_seq`
  a later reader would accept. Round 3 §7.1 has no per-segment "written up
  to here" field, and adding one would be the first format change.
- **Decision.** Every CP writes its whole map as CP log pages (one
  `CpLogRec` per mapped block) and its whole segment table into the open
  metadata segment, packed page after page into whole stripes, and the
  superblock's `cp_log_head` and `seg_table` roots name those two regions
  exactly (block number, page count, CRC). A boot trusts only what a root
  names, verifies every stripe the selected CP wrote by its summary (CRC
  and identity, nonce included) and never looks at a stripe or page no
  committed CP named, so stale data is simply invisible. The append point
  of the open data segment is derived from the map's live blocks, that of
  the open metadata segment from the roots' regions. A CP sequence number
  may be re-issued after a rewind.
- **Consequences.** Metadata per CP is proportional to the mapped blocks
  (fine for the simulator and lab pools; the radix map with incremental
  checkpoints of week 4, `otap-map`, replaces the dump behind the same
  roots). `SegEntry.live_bytes` keeps its documented meaning (live bytes,
  for the cleaner); for a metadata segment it is the bytes the current
  roots occupy. A freshly formatted pool has `NONE` roots, read as an empty
  table and an empty map.
- **Enforced by** `otap_cp::boot::evaluate`, `BlockMap::{dump, load}`,
  `SegmentTable::{image, from_image}`, the simulator's torn-stripe and
  commit-crash tests.

## E15. `arc-swap` for the epoch flip, `parking_lot` for the maps

2026-10-13. Accepted (departs from the brief's `crossbeam-epoch`).

- **Context.** The brief names crossbeam-epoch for the lock-free cut. Its
  readers dereference a `Shared` pointer, which is `unsafe`, and doc 05 §8.3
  makes `otap-sys` the only crate that may contain `unsafe` (E08).
- **Decision.** The active generation is an `ArcSwap<Generation>`: writers
  `load()` it without a lock, insert under the volume's `parking_lot`
  mutex and drop the guard; the cutter `swap()`s in a fresh generation and
  waits until the old one's strong count is 1, which is the grace period
  (the swap has already accounted every outstanding guard). The wait never
  depends on a flush. The frozen queue holds at most `ring - 1`
  generations (8 in the ring).
- **What the wait covers (review, 2026-10-13).** Every holder of the old
  pointer: inserts in progress and readers inside `with_active` or
  `lookup`. A long reader holds the cut for as long as it reads, a stream
  of short readers never starves it, and a cut issued from inside
  `with_active` would wait for itself; the crate documentation says so.
  An acquire fence follows the wait (the count is read relaxed; the maps
  are read under their own locks either way).
- **Consequences.** `otap-index` stays `forbid(unsafe_code)`. The grace
  wait is spin-then-yield and is measured (`Index::grace_spins`), never
  asserted; the index tests prove a cut waits for a reader that saw the old
  generation and that concurrent inserts are never lost across cuts.
- **Enforced by** `crates/otap-index/src/lib.rs`, `index_tests.rs`.

## E16. The arena: layout, states, re-attach, and single-writer access

2026-10-13. Accepted.

- **Decision (layout version 1.1 after the review of 2026-10-13).** The
  arena is a 4 KiB header (`OTAR`, identity and geometry sealed by a CRC
  once at creation and never rewritten; the attach counter is a
  self-checking word beside the seal, updated by one store), a descriptor
  table (32 bytes per slot: `vol << 40 | lba`, sequence, block CRC, and a
  sealed *word* holding the state in 2 bits, the lap in 30 bits and a
  CRC-32C over the descriptor) padded to pages, then 4 KiB slots. Runs
  never wrap; a `RingRef` is `(slot, lap)` and a reference whose lap no
  longer matches is refused. States: `Free` (reserved on the current lap,
  or unused), `Filled` (data and descriptor written), `Placed` (in the
  index, may be acknowledged), `Committed` (reusable: its CP landed, or it
  was skipped, discarded or superseded). The tail passes `Committed` slots
  and slots of older laps; the window `[tail, head)` bounds occupancy with
  no collector; a run that does not fit the lap remainder skips to the
  next lap, and the skipped slots count as passable when the fit is judged.
- **Every transition is one store.** The fields that change only while a
  slot is `Free` are stored first; the word is stored last, by one aligned
  8-byte store, with a compiler fence between. A `kill -9` at any
  instruction therefore leaves every descriptor either before or after a
  transition, never between. Re-attach keeps `Placed` slots whose word and
  data verify (replayed in sequence order), drops `Filled` ones (never
  acknowledged), turns the dead process's reservations into `Committed`,
  takes the next sequence number as the maximum over every descriptor (a
  number handed out before the kill is never handed out again), and
  *reports* every slot it cannot trust (a `Placed` or `Committed` word that
  does not verify, a `Free` or `Filled` word sealed in another state, a
  `Placed` block that fails its recorded CRC); the engine treats any such
  report like a power loss. A `Free` reservation that is never filled is
  released by `Arena::discard`, which the engine calls when a copy fails.
  A block superseded inside the active generation releases its slot at
  once (the index returns the superseded runs); without that the ring fills
  with dead data.
- **Single writer.** Every arena method takes `&self` or `&mut self`;
  concurrent reactors in `dbladed` will partition the ring or go through
  an `otap-sys` cell type that can hand out disjoint slot views safely
  (the one place such an API needs `unsafe`). The engine of Step 3 is one
  thread by design.
- **Enforced by** `crates/otap-arena/src/lib.rs`, `arena_tests.rs`, the
  simulator's daemon-crash tests.

## E17. The consistency point in Step 3

2026-10-13. Accepted.

- **One generation per CP, one CP at a time.** Triggers (shutdown,
  request, arena half full, timer, back to back) cut only when no
  generation is already waiting for the flusher, which stops a held
  trigger from cutting a sliver every tick; a request on an idle engine
  still produces an empty CP. A clean shutdown cuts once more and commits
  with `CleanShutdown`; a boot commits once before it serves.
- **On disk, in this order:** full data stripes (summary page first, every
  byte assembled in memory, `Ingest` kind, group 0), then the metadata
  region, then a pool flush, then the superblock slot at `A`, then at `B`,
  each a FUA write. The map, the retired generation and the arena slots
  change only after `B` lands. A failed write fails the attempt; the
  generation stays frozen and is re-staged with a new nonce.
- **nvfail.** Conservative mode (the default) writes every present,
  writable volume into `write_active` and never writes an intent slot.
  Precise mode keeps per-volume windows: a write sets two, a CP that
  carries the volume resets them to two, a CP without a write for the
  volume takes one, and the slot lists the volumes with a window left
  *after* the CP it commits. A write to a volume outside the engine's
  `durable_active` waits for an intent slot that lists the durable set
  plus the volumes waiting to be acknowledged, nothing more. The one slot
  built per commit is written to `A` and then to `B` with identical
  content.
- **`durable_active`, as the review of 2026-10-13 settled it.** The set of
  volumes a write may be acknowledged to without an intent: volumes that
  are durably active now *and* in the set of every superblock slot built
  and not yet landed. Building a slot therefore intersects the set with
  the slot's `write_active` (never assigns it: a volume may be in the
  slot's set only because of a write still waiting for its intent); a
  landed intent unions its set in; the `A` slot landing unions the slot's
  set in; boot loads `boot_write_active`. A slot records as
  `intent_seq_seen` the highest intent whose completion the engine has
  seen, not the highest issued, so an intent in flight when the slot is
  built is honoured by the boot whether or not the slot lands. A CP that
  commits also acknowledges every write still waiting for an intent whose
  sequence it carried (sequence numbers are in placement order).
- **A dirty boot** is one whose selected CP is not `CleanShutdown` *or*
  whose boot rewound (a rewind proves the engine ran past the selected
  CP). It holds every volume in `boot_write_active` (cause `TornCp` after
  a rewind, `DirtyBoot` otherwise) unless the arena re-attached whole and
  nothing was rewound: a rewind loses the rewound CP's acknowledged writes
  whether or not the arena survived, a damaged arena slot or header means
  an acknowledged write may be gone, and both hold. Volumes the control
  plane lists and the disk lacks are created with the boot commit.
- **Durable before serving.** A volume created at runtime answers
  `NoSuchVolume` until the CP that carries its table row commits
  (`Event::VolumeCreated`); a clear keeps the volume at `NvFailed` until
  the CP that records the bumped `rollback_gen` commits
  (`Event::NvfailCleared`), so a generation a client saw is never handed
  out twice (section 7.6) and no acknowledged write can outlive a volume
  that was never durable.
- **Bounds and refusals.** Volume sizes are `1..=MAX_LBA + 1`; a request
  longer than `MAX_REQUEST_BLOCKS` (2,048) or than the arena answers
  `TooLarge`; `format` and `create_volume` refuse a volume set whose CP
  metadata (the whole map image plus the segment table, E14) could outgrow
  one segment on the geometry. Every request-level refusal is a status on
  the request; `tick` returns `Err` only for internal failures.
- **A full pool.** Data allocations keep `META_RESERVE_SEGMENTS` (2)
  segments free for the metadata a boot or a shutdown commit needs, and
  metadata segments are recycled: when CP `N` commits, every metadata
  segment that names neither `N` nor `N - 1` (nor, after a boot, any
  verified slot on the ring) is freed, because a boot reaches only the
  newest committed CP or the one before it. A CP that still finds no
  segment fails, `is_pool_full` holds, new writes answer `NoSpace`, reads
  are served, and no CP is attempted until one is requested; a shutdown
  that cannot write what it has stops without the clean marker
  (`Event::Stopped { clean: false }`) and the arena keeps the unflushed
  writes for the next boot.
- **Arena damage while running.** The read path checks a block from the
  arena against the CRC recorded when it was written and answers
  `Corrupt`; the stripe builder writes such a block under the recorded
  CRC and reports `Event::ArenaDamage`, so the disk answers the same and
  damaged data is never served as good.
- **Deferred:** relocation and freeing of data segments (the cleaner), the
  metadata device class, discard, snapshots, the back-pressure delay curve
  (a full arena defers writes, first in first out, while reads and flushes
  are served past them), `cut_within` for freezes, and `ShipApply`.
- **Enforced by** `crates/otap-cp/src/engine.rs`, `sim_tests.rs` and
  `adversarial_sim_tests.rs`.

## E18. The simulator: proptest and a seeded device, not madsim

2026-10-13. Accepted.

- **Decision.** The harness is single-threaded and seeded: proptest
  generates scenarios (write, read, ticks, time, request, power cut, daemon
  crash, clean restart), a SplitMix64 drives the device's completion order
  (a few per poll, reordered), and virtual time moves only when the
  scenario says so. The fake device keeps pending, cached and durable
  layers; a flush makes the cache durable unless the device is
  `flush_opaque`; FUA writes are durable on completion; a power cut tears
  every unsafe write at 512-byte sector granularity (dropped, whole, or a
  random subset of sectors); a daemon crash lets the device finish what it
  had. madsim was not taken: it brings a tokio runtime the engine does not
  use, and determinism here comes from the engine being tick-driven.
- **The checker (as widened by the review of 2026-10-13).** Every block a
  write carries names its write, so a recovered image decodes to writes,
  zeros or garbage. The model knows every write from the moment it is
  *issued*, acknowledged or not, because the engine places writes in issue
  order and a CP carries whole generations: whatever a crash leaves is the
  state after the issued writes up to some operation `j`. After any
  recovery one `j` must explain every block; an acknowledged write above
  `j` is lost (none after a daemon crash or a clean restart; after a power
  cut its volume must be held); an unacknowledged write above `j` never
  happened and one at or below it lives on like an acknowledged one
  (`Model::settle`). nvfail holds every volume that lost a write, never a
  volume after a clean shutdown, in conservative mode every volume on a
  dirty boot, and in precise mode no volume idle for two CPs, where an
  issued but unacknowledged write counts as activity (its intent may have
  landed). The committed image alone (map only) is checked to be a prefix
  after every operation.
- **The harness.** Writes go one at a time (`write`) or several in flight
  (`submit_write`, `collect`, `wait`); a refused boot gives the device and
  the arena back (`BootFailure`), so a scenario can go on; a shutdown that
  stops without the clean marker is recorded as a daemon stop; runtime
  volumes join the model and the configuration. The loopback network holds
  chunks across a partition and delivers them, in order, when it heals,
  and refuses an unknown endpoint, because the `Net` contract is ordered,
  reliable delivery.
- **Enforced by** `crates/otap-sim`, `sim_tests.rs`,
  `adversarial_sim_tests.rs`.

## E19. Flush-opaque backstores are refused until the owner decides

2026-10-13. Accepted; needs an owner decision before Step 4's real
`Backstore` on a RAID virtual drive.

- **Context.** Doc 05 (C07, invariant 5 of section 7.7) says a torn CP on
  a `flush-opaque` backstore "falls back to N-2 intact" and that segment
  reuse is deferred one more CP there. The superblock ring F7 lays out
  holds two CPs (`A0/A1`, `B0/B1` alternate by parity), so N-2 cannot be
  reached from it, and the Step 3 engine never read the flag. The review
  showed four consequences on a `flush_opaque` fake (a rewind with a
  re-attached arena losing the rewound CP without a hold, a rewind onto a
  clean-shutdown CP booting clean, torn data behind an intact summary
  served as `Corrupt`, no N-2 fallback), two of which were engine defects
  independent of the flag and are fixed (E17).
- **Decision.** `Engine::format` and `Engine::boot` refuse a geometry with
  `flush_opaque` set (`EngineError::Unsupported`). The fake keeps its
  `flush_opaque` mode for future work. Two designs are on the table for
  the owner: a three-CP ring (a format minor bump: a third slot pair, or
  `B` slots keyed by `cp_seq / 2`) with the deferred-reuse lists, which is
  what doc 05 wrote; or a post-commit flush discipline (flush after `B`
  before anything else is issued; an intent is durable only after a flush
  that follows it), which makes N-1 always intact and needs no ring
  change but costs a flush per CP and per intent. Which one is right
  depends on what the target controllers do with FUA, which Step 4 will
  measure.
- **Enforced by** `engine.rs` (`format`, `boot`) and
  `flush_opaque_geometries_are_refused_until_the_owner_decides`.

## E20. The index's ordering contract

2026-10-13. Accepted (from the review).

- **Context.** The index coalesced extents of different writes and kept
  the highest sequence of the parts, so a late insert with a lower
  sequence could lose blocks it should have won; and the blocks a late
  insert lost were neither kept nor returned, so their arena slots leaked.
  Both needed an insert to arrive out of sequence order, which the engine
  never does (sequence numbers come from the arena at placement, and a
  re-attach replays in sequence order).
- **Decision.** The contract is explicit: `Index::insert` refuses a
  sequence below the volume's newest (`IndexError::StaleSeq`; equal is
  another piece of the same write, as a re-attach produces when a later
  overwrite split a run), so insertion order and sequence order are one
  order; `VolumeDirty::insert` lets the later insert win its whole range
  and returns every block it displaced. A merged extent's sequence is then
  exactly the newest write it holds.
- **Enforced by** `crates/otap-index/src/lib.rs`, `index_tests.rs`,
  `adversarial_index_tests.rs`.
