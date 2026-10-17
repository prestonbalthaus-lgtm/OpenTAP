# OpenTAP engineering decisions

Living log of the decisions the implementation made that the design documents
(`05-architecture-brainstorm-03.md` and its predecessors) or the step briefs
did not make, or made differently. Entries are numbered E01 onward and never
renumbered; a reversed decision gets a new entry that points back. The format
rulings F1 to F11 are owned by the code (`crates/otap-format/src/lib.rs`) and
only summarised here (F12 to F14 since format 2.1); the stream rules S1 to S10 are owned by this file and
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
  `.gitattributes` (`*.bin binary`), `BUILDING.md` "Golden vectors".

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
Details and the PATH prefix are in `09-working-agreements.md` and `BUILDING.md`.

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

## E21. E19 is closed by the owner: flush-opaque backstores are trusted, not refused

2026-10-13. Accepted (owner ruling, Step 4 brief, section 1). Supersedes
the refusal in E19.

- **Context.** E19 refused flush-opaque backstores (a hardware RAID
  virtual drive, any LUN whose cache the kernel cannot see) until the
  owner decided between a three-CP ring and a post-commit flush
  discipline. The owner ruled that OpenTAP must not hard-refuse them in
  production: hardware agnosticism across PERC, MegaRAID, Smart Array
  virtual disks, `mdadm` and raw drives is a core requirement.
- **Decision.** `MediaProfile` carries a `FlushPolicy`
  (`otap_cp::traits`): `RequireFuaOrFlush` (strict: a flush-opaque
  geometry is refused at format and boot with
  `EngineError::FlushOpaqueRefused`, as E19 did) and
  `TrustBarrierWithNvfail`, the default and the mode for hardware RAID and
  opaque LUNs. Under the trust policy the engine keeps its host-side
  barrier exactly where E17 put it (the pool flush after the stripes and
  the metadata, before the `A` and `B` commits; on Linux an
  `IORING_OP_FSYNC` with `FSYNC_DATASYNC`), trusts it, pushes a structured
  `OperatorWarning::FlushOpaqueTrusted { policy }` into
  `pool_status().warnings` and emits `Event::OperatorWarning` at boot,
  defers every segment free one more consistency point (regions freed by
  CP `N` are reusable after `N+2` instead of `N+1`), and relies on nvfail
  to quarantine every volume after a power loss, which a dirty boot does
  anyway. `MediaProfile::flash` and `::hdd` default to the trust policy;
  `otap-geom` derives it (E26). The third option of E19 (falling back to
  the N-2 CP when N-1 is torn) is not reachable with the two-slot ring the
  format has and was not built; the ruling does not ask for it.
- **The other rulings recorded with it.** D1: Phase C (single node) then
  Phase B (dual node, internal drives, asynchronous delta shipping) then
  Phase A (shared SAS shelf). D2: `unmirrored_flush = placed` is the
  default; client writes are never throttled to disk speed when a partner
  or witness is unreachable; they are acknowledged at DRAM speed and the
  cluster health degrades. D8: iSCSI (LIO over ublk), then NFSv3/v4.1 and
  SMB3 in SVM namespaces, then Fibre Channel (`tcm_qla2xxx`) and S3
  (`versitygw`). D9: volumes are thin-provisioned by default and `nvfail`
  is on by default; segment reclamation discards whole segments
  (`BLKDISCARD`) on flash and overwrites sequentially without discards on
  rotational drives and hardware RAID. D10: the third Raft voter runs on
  the local LAN under 100 ms; a witness failure never degrades or
  throttles I/O. Of these only D2 and D9 have code behind them today: a
  flush is acknowledged when the writes it covers are placed (the engine's
  `Flush` completes at once), and `DiscardPolicy` (`WholeSegment`,
  `Never`) is a `MediaProfile` field the engine honours when it frees a
  segment.
- **Consequences.** A pool on a lying controller with a dead battery can
  lose the writes the barrier was trusted for; the simulator models that
  (`cache_protected = false`) and the tests show the engine then either
  boots dirty with every volume quarantined behind a consistent prefix, or
  refuses to boot, never serves a torn image. An operator who wants the
  refusal back sets the strict policy.
- **Enforced by** `crates/otap-cp/src/traits.rs` (`FlushPolicy`,
  `DiscardPolicy`, `MediaProfile`), `engine.rs` (`check_flush_policy`,
  the warning, the deferred frees), `otap-sim/src/backstore.rs`
  (`cache_protected`), `adversarial_sim_tests.rs`
  (`flush_opaque_geometries_follow_the_flush_policy`),
  `step4_sim_tests.rs` (the two opaque-cache tests, the discard-policy
  test).

## E22. The persistent map: overlays, a CP log of delta regions, folds, and format 2.1

2026-10-13. Accepted. Lifts the whole-map image of E14.

- **Context.** E14 wrote the whole block map at every consistency point
  and bounded the metadata of a CP to one segment. The Step 4 brief asks
  for the two-level radix map with copy-on-write dirty pages, fast CP
  logging through `CpLogPage`/`CpLogRec` with periodic folds, snapshot
  pinning and the `birth_cp` walk.
- **Decision.** `crates/otap-map` (no `unsafe`, no I/O: it reads pages
  through a `PageReader` and places regions through a `RegionWriter`) holds
  one `VolumeMap` per volume: the on-disk `MapRoot`, the page index (one
  `PageIndexEntry` per 1,024-entry `MapPage`), and an in-memory *overlay*
  of the entries changed since the last fold. A lookup reads the overlay
  first, then the page through a pba-keyed cache. Every CP writes its
  changes as a *delta region* (a run of `CpLogPage`s, one `CpLogRec` per
  block written or hole punched) and a *directory* (`CpLogDirPage`, F12)
  that names every delta region since the last fold in CP order; the
  superblock's `cp_log_head` names the directory and `map_roots` the
  table of roots (F14). A *fold* rewrites, copy on write, every map page
  the overlay and the CP's own records touch, writes the page indexes and
  roots of the volumes concerned, and empties the directory; a fold CP
  writes no delta region of its own (its records are in the pages).
  Replay at boot applies a delta region's records iff the region's CP is
  above the fold CP, which lets a later relocation preserve births. The
  fold policy is the engine's (`FoldPolicy`: log bytes, time, CPs with
  changes, committed dirty pages, committed overlay entries; defaults
  1 GiB, ten minutes, 1,024, 65,536, 8 million), and the engine also folds
  on request, for a snapshot and at a clean shutdown. A volume's index is
  bounded to a quarter of the pool (`EngineError::MetadataTooLarge`)
  instead of E14's one-segment bound.
- **Format 2.1.** `FORMAT_MINOR` 1; rulings F12 (the directory), F13 (the
  `SnapRootPage`) and F14 (the table regions) in
  `crates/otap-format/src/lib.rs`; the superblock's reserved area yields
  `open_data_pseg` and `open_data_idx` so a boot restores the open data
  segment; a new golden-vector set `tests/vectors/v2.1/` (14 files,
  manifest `e85ac0be…13ac61`) next to the kept 2.0 set, both decoded by
  the lane. A pool written by 2.0 is refused at boot (`BootError::
  OldLayout`); none was ever released.
- **Consequences.** The metadata of a CP is a handful of pages (the
  deltas, the directory, the tables) until a fold, whose cost is the dirty
  pages plus the indexes of the volumes touched. Reads of unfolded entries
  hit the overlay (memory); the overlay's size is bounded by the policy.
  The delta regions of a fold are freed under the deferred-free rule of
  E24 so a crash during the fold still has them.
- **Enforced by** `crates/otap-map/src/lib.rs`, `map_tests.rs` (a model
  across random CPs, folds, replays and reboots; damage and misdirection;
  replay disorder), `otap-format/src/lib.rs` (F12 to F14), `records.rs`,
  `records_tests.rs`, `format_tests.rs` (the 2.1 vectors),
  `step4_sim_tests.rs` (`a_map_spanning_many_pages…`,
  `cp_log_records_replay…`, `the_fold_policy_folds_on_each_of_its_triggers`).

## E23. Discards and writes of zeros are holes: an arena record, an index extent, a map entry with a birth

2026-10-13. Accepted.

- **Context.** The ublk front door delivers `Discard` and `WriteZeroes`;
  the format has had a hole entry with a birth since Step 2; the arena and
  the index knew only data.
- **Decision.** A punch is served like a write but without data: one arena
  slot takes a `HoleRecordBlock` (arena layout 1.2: `SlotDesc.kind`
  distinguishes `Data` from `Hole`), the index gets a `DirtyExtent` with
  `hole = true` (it wins its range like a write; a superseded hole releases
  nothing), reads of it return zeros (`PeekSource::Hole`), and the CP
  writes it as a `CpLogRec` hole with the CP as birth, never into a
  stripe; the record slot is released when the generation's CP commits.
  Both request kinds share the path (`serve_punch`), bounded by
  `MAX_PUNCH_BLOCKS` (262,144 blocks, 1 GiB) per request and the volume's
  range. A punch survives a daemon crash through the re-attached arena
  (the record is placed and verified like data) and a power cut through
  the map once committed. `diff` reports every entry born after the base,
  data or hole, including a hole punched over a block that was unmapped
  (a delta shipper punches zeros over zeros; "what changed" is what the
  map knows).
- **Consequences.** A discard costs one arena slot and one log record,
  not a stripe; a volume-wide punch is one request. The simulator's
  checker treats a punched block as expected zeros and, because zeros are
  ambiguous (a punch over a write leaves what nothing at all would), takes
  the largest operation number the image is consistent with.
- **Enforced by** `crates/otap-arena/src/lib.rs` (`fill_hole`,
  `hole_record`, layout 1.2), `otap-index/src/lib.rs` (`DirtyExtent::hole`),
  `otap-cp/src/engine.rs` (`serve_punch`, `gen_holes`), `arena_tests.rs`,
  `index_tests.rs`, `step4_sim_tests.rs` (`punches_persist…`,
  `punches_are_refused_like_writes`,
  `a_punch_over_a_write_is_not_a_loss…`), `otap-ublk/tests/engine_tests.rs`.

## E24. The metadata stream, liveness accounting and deferred frees

2026-10-13. Accepted.

- **Context.** With the map in regions of varying size, a CP's metadata no
  longer fits one image in one segment, and freed regions must not be
  reused while a crash could still boot the CP that references them.
- **Decision.** `MetaStream` (`otap-cp/src/meta.rs`) packs every region of
  a CP (deltas, fold pages and indexes, snapshot root pages, the snapshot
  table, the roots table, the directory, then the segment table, reserved
  first so its own bytes count) back to back into whole stripes of the
  open metadata segment, and into a run of adjacent free segments
  (`SegmentTable::allocate_run`) when a region does not fit what is left;
  it works on a copy of the segment table that the engine swaps in at
  commit. The table keeps `data_live` and `meta_live` per segment; a
  region a CP stops referencing is queued and released after the next CP
  commits (`N+1`; `N+2` under a flush-opaque profile, E21), and the two
  segments of the boot CP are protected for two commits. A freed data or
  metadata segment is discarded whole or merely returned, per the
  `DiscardPolicy`; a segment whose discard is in flight stays allocated
  until the discard completes, because a reordering device completed a
  late discard over a segment the engine had already reused. Data
  allocations keep `META_RESERVE_SEGMENTS` (2) free for the boot and
  shutdown commits.
- **Consequences.** A pool's metadata can span as many segments as it
  needs; the cleaner for data segments is still not written (a segment
  with one live block stays allocated). Events `SegmentFreed { pseg,
  discarded }` and `Folded` make the accounting visible.
- **Enforced by** `crates/otap-cp/src/meta.rs`, `segments.rs`,
  `engine.rs` (`freed_pending`, `boot_protect`, `discarding`,
  `free_segment`), `step4_sim_tests.rs`
  (`a_segment_stays_allocated_until_its_discard_completes`,
  `segment_reclamation_follows_the_discard_policy`,
  `a_map_spanning_many_pages_and_segments…`), the simulator's
  random-geometry walk.

## E25. Snapshots in Step 4: taken at a fold, pinned by a root page, read and diffed, not yet deleted

2026-10-13. Accepted.

- **Context.** The brief asks for pinning a volume's root into a
  `SnapRecord` and the `birth_cp > base_cp` walk for delta shipping.
- **Decision.** `create_snapshot(vol, uuid, kind, origin)` is taken at the
  next consistency point, which the engine makes a fold so the root is
  complete; the CP writes a `SnapRootPage` (F13) with the pinned
  `MapRoot`, lists the `SnapRecord` in the snapshot table (F14), and
  announces `Event::SnapshotCreated` when it commits. `snapshot_read`
  resolves through the pinned root (overlay-free), `snapshot_diff` and
  `live_diff` walk births above the snapshot's CP (`pinned_diff`, `diff`),
  reading only the pages whose `max_birth` says they may hold one. A
  snapshot pins what it references: a data block stays live while a
  snapshot of the volume has a birth at or above the block's; a map page
  or index region stays live while a pinned root references it. Snapshot
  deletion and deadlists are not written (`deadlist` is `NONE`); a boot
  that rewinds below a snapshot's CP loses the snapshot with the CP, which
  the simulator's checker allows and nothing else does. Names stay off the
  disk (F11).
- **Consequences.** A snapshot costs one fold; the space it pins is
  returned only by deletion, which is Step 5 work with the cleaner.
- **Enforced by** `crates/otap-cp/src/engine.rs` (`create_snapshot`,
  `snapshots`, `snapshot_read`, `snapshot_diff`, `live_diff`, the liveness
  rules), `otap-map/src/lib.rs` (`pin`, `pinned_get`, `pinned_diff`),
  `otap-sim/src/harness.rs` (`snapshot`, `verify_snapshots`,
  `check_diff`), `step4_sim_tests.rs` (`snapshots_keep_their_image…`,
  `a_snapshot_is_isolated…`), the extended random walk.

## E26. Geometry discovery: sysfs facts, a classification, and a plan with overrides

2026-10-13. Accepted.

- **Context.** The brief asks for sysfs queue parameters with deterministic
  mock inputs and a `StripeGeometry` aligned to md or hardware RAID, with
  the segment size by media and the E19 policy derived.
- **Decision.** `crates/otap-geom` (no `unsafe`, no I/O of its own) reads
  a `SysfsSource` (`Sysfs` on Linux, `MockSysfs` everywhere) for
  `rotational`, `minimum_io_size`, `optimal_io_size`, `alignment_offset`,
  `discard_granularity`, `write_cache`, `fua`, the device size, the md
  level, chunk and member count, the vendor and model; classifies the
  device (`Md`, `Nvme`, `HwRaidVd`, `Disk`, `Virtual`, `Loop`, `Dm`,
  `Other`); and plans: the stripe is the md chunk times the data disks
  (RAID 10 by its layout), else `optimal_io_size` when it is at least
  64 KiB (below that it describes a physical block), else 1 MiB; the
  segment is 80 MiB rotational, 64 MiB NVMe, 8 MiB other flash, shrunk
  until the device holds eight, or `TooSmall`; a misaligned device is
  refused unless `allow_misaligned`; a hardware RAID virtual disk is
  flush-opaque and gets the trust policy, everything else the strict
  policy; flash with a discard granularity gets `WholeSegment` discards;
  warnings explain each choice. `Overrides` let an operator set any of
  these. `StripeGeometry::geometry()` and `media_profile()` feed the
  engine.
- **Consequences.** The planner has run only against mocks; the first real
  hosts will teach it vendor strings and md layouts it does not know
  (the `Other` and `Virtual` classes exist for that).
- **Enforced by** `crates/otap-geom/src/lib.rs`, `geom_tests.rs`.

## E27. The Linux backstore and its portable fallback live in `otap-backstore`

2026-10-13. Accepted.

- **Context.** The brief allowed the production `Backstore` in `otap-cp`
  or in its own crate. `otap-cp` is pure and forbids `unsafe`; the Linux
  driver needs `otap-sys`.
- **Decision.** `crates/otap-backstore` (no `unsafe`) holds
  `UringBackstore`, the Linux driver over `otap_sys::block::BlockRing`:
  the device opened `O_DIRECT`, every stripe and metadata write staged
  into a page-aligned pool slot the ring owns until the completion is
  reaped, superblock and intent writes with `RWF_DSYNC`, the barrier as
  `IORING_OP_FSYNC` with `FSYNC_DATASYNC`, segment discards as
  `BLKDISCARD`, the capacity checked against the profile at open; the
  engine orders `A` before `B` (E17) and the driver completes in order of
  reaping. `FileBackstore` is the portable fallback: a plain file,
  synchronous, honest (whole blocks only, bounds checked, `sync_data` on
  commits, intents and flushes, discards complete and keep the data), so
  `cargo test --workspace` drives a real `Engine` over a file on any host.
  The ring itself (`BlockRing`, `block.rs` in `otap-sys`) is where the
  five new `unsafe` blocks live, with their proofs.
- **Consequences.** The Linux half is type-checked for
  `x86_64-unknown-linux-gnu` and has never executed; the file fallback
  is what the Windows host runs. The first Linux run is a Step 5 item with
  the dm-flakey and dm-log-writes lanes.
- **Enforced by** `crates/otap-backstore/src/{file,uring}.rs`,
  `otap-sys/src/block.rs`, `file_backstore_tests.rs`.

## E28. The ublk front door: a transport, a tag state machine, and no `libublk`

2026-10-13. Accepted.

- **Context.** The brief asks for the `FrontDoor` over `UBLK_F_USER_COPY`
  with the whole command state machine testable on Windows.
- **Decision.** `crates/otap-ublk` (no `unsafe`) separates the queue
  (`Transport`: poll events, read and write the request data at the
  user-copy offset, commit a result) from the state machine
  (`UblkFrontDoor<T>`). A tag is `Idle`, `Ready`, `InEngine` or `Aborted`;
  a descriptor decodes into the engine's request (`Flush`; `Read`,
  `Write`, `Discard`, `WriteZeroes` in whole 4 KiB blocks inside the
  volume) or is answered at once (`EOPNOTSUPP` for every other operation,
  `EINVAL` for a partial block or a range past the end, `ENODEV` while
  offline); the engine's status maps to the result (bytes moved, or `EIO`
  for `NvFailed`/`Corrupt`/`Failed`, `ENODEV` for `NoSuchVolume`/`Stopped`,
  `ENOSPC`, `EINVAL` for `OutOfRange`/`TooLarge`); the request id is
  `generation << 32 | queue << 16 | tag`, the generation advancing at every
  completion or abort so a stale completion is dropped; watermarks close
  the gate to the engine at `high` requests in flight and reopen it at
  `low`; a refused commit retires the tag. `MockTransport` drives all of
  it on any host, including abort, copy and commit failures. On Linux
  `device.rs` creates the device (`ADD_DEV` with `USER_COPY`,
  `USER_RECOVERY` and the ioctl encoding; `SET_PARAMS` with 4 KiB logical
  and physical blocks, `VOLATILE_CACHE` and `FUA` so the block layer
  sends flushes, discard limits, 8 MiB requests; one queue; a fetch on
  every tag before `START_DEV`) and runs the engine's tick loop. The
  `libublk` pin is dropped from the workspace: `otap-sys` encodes the UAPI
  and owns the rings, which keeps the `unsafe` inventory in one place.
- **Consequences.** One queue per device for now; the multi-queue and
  multi-reactor arena access of E16 remain Step 5 items. The Linux module
  is type-checked only. The kernel facts it relies on were checked against
  the saved 6.12 `ublk_drv.c` (Step 2's sources): the command buffer is
  mapped read-only at `UBLKSRV_CMD_BUF_OFFSET + q_id * 4096 * 24` with
  exactly the page-rounded size of the queue's descriptors, by the
  process that serves it; `START_DEV` waits until every queue has a fetch
  on every tag; a queue's commands must come from the task that issued its
  first fetch; a fetch or commit under `USER_COPY` must carry `addr = 0`;
  a read or write completes with its full byte count (a short count
  requeues the rest, a zero read is `EIO`) and a flush, discard or write
  of zeros with any non-negative result. The first Linux run still has to
  exercise them.
- **Enforced by** `crates/otap-ublk/src/{frontdoor,mock}.rs`,
  `frontdoor_tests.rs`, `frontdoor_proptests.rs` (a model of the tag
  states, the gate and every commit under random interleavings),
  `engine_tests.rs` (the engine behind the front door over the simulator's
  device: round trips, watermarks, nvfail, stop, aborts).

## E29. Format 2.2: the superblock records the geometry

2026-10-14. Accepted (from the Step 4 review, defect 3).

- **Context.** The superblock recorded no geometry, and slot `A0` sits at
  block 0 under every stripe size, so a pool opened under another stripe
  (an operator override, a planner that decided differently after a
  kernel or md change) found a verified `A0`, laid itself out anew, could
  miss the newest CP whose slots had moved, and its boot commit wrote
  over the old layout's segments and slots.
- **Decision.** Format 2.2 (F15): every commit carries `geom_stripe_bytes`,
  `geom_segment_bytes` and `geom_capacity_bytes` in the former reserved
  area; a recorded geometry is verified whole and consistent; the boot
  compares the newest slot's geometry with the profile and refuses a
  difference (`BootError::Geometry`) before it writes anything. Pools
  written by 2.0 or 2.1 are refused like before; none was released. A new
  golden-vector set `tests/vectors/v2.2/` (manifest `3f9a9651…f8168d`)
  sits next to the kept 2.0 and 2.1 sets.
- **Consequences.** A geometry change is a re-format (or, later, a
  migration tool); a planner that learns cannot silently re-lay an
  existing pool.
- **Enforced by** `crates/otap-format/src/{constants,superblock}.rs`,
  `format_tests.rs` (the 2.2 set and lane), `adversarial_tests.rs` (the
  oracle's geometry rule), `otap-cp/src/boot.rs` (`evaluate`),
  `otap-backstore/tests/review_backstore_tests.rs`.

## E30. What a boot verifies, what tears a candidate, and what fails the boot

2026-10-14. Accepted (from the Step 4 review, defects 1, 4, 5, 15, 16).

- **Context.** The review found four gaps in the boot: the stripes a fold
  CP wrote were never checked (a fold writes no delta region, and the
  stripes to check came from the delta records); under the trust policy a
  stripe whose summary landed but whose data block did not was taken as
  good; a transient read error made a sound CP torn, and the boot commit
  then overwrote its slots; and damage to an old snapshot's root page or
  pinned index tore every candidate, taking the whole pool offline over
  secondary data. It also found that the boot CP's own segment-table
  region was never released, leaking one segment per boot.
- **Decision.** For a fold CP the stripes to check come from the entries
  born at that CP in the fold's pages. Under a trusted opaque barrier
  (E21) after an unclean stop, every data block of those stripes is
  checked against its descriptor's CRC as well: one CP's worth of reads,
  at a boot after a power loss only. A read error while loading a
  candidate fails the boot (`BootError::Io`), to be retried, instead of
  marking the candidate torn; only a region that reads back and does not
  verify tears a candidate. A snapshot root page or pinned index that
  does not verify tears the candidate only when the candidate itself
  wrote it (the snapshot's birth is the candidate's CP); otherwise the
  snapshot is kept in the table, flagged damaged
  (`OperatorWarning::SnapshotDamaged`, `SnapshotInfo::damaged`), its reads
  and diffs refused (`EngineError::SnapshotDamaged`), its data still
  pinned by birth, and the boot goes on. The selected slot's `seg_table`
  region joins the table regions the next CP releases.
- **Consequences.** A permanently unreadable sector in the newest CP's
  regions makes the pool refuse to boot until the operator intervenes (a
  zeroed sector then reads back wrong, tears the candidate, and the boot
  rewinds); the review judged a transient error far more common than a
  permanent one, and a silent rewind the worse outcome.
- **Enforced by** `crates/otap-cp/src/boot.rs`, `engine.rs`
  (`assemble`, `snapshot_read`, `snapshot_diff`),
  `otap-sim/tests/review_sim_tests.rs` (the five regressions).

## E31. Space, folds and a full pool: the reserve, a fold that does not fit, and dead data segments

2026-10-14. Accepted (from the Step 4 review, defects 10, 11, 12, 24).

- **Context.** Running out of metadata space while placing the engine's
  own regions failed the tick instead of reporting a full pool; metadata
  runs could take the two segments the data allocations keep for the
  boot and shutdown commits, so a pool filled by snapshot metadata could
  no longer boot; a fold the policy forced on a nearly full pool failed
  for space at every retry although a plain CP would fit; and dead data
  segments were never freed although E24 and the status document said
  they were.
- **Decision.** Every out-of-space failure of a CP, whoever placed the
  region, is a full pool. The metadata stream of a regular CP leaves
  `META_RESERVE_SEGMENTS` (2) free, a shutdown commit leaves one (the
  boot's), and a boot commit may take the last. A fold that fails for
  space is dropped: the policy's folds wait, and snapshot requests wait,
  until more segments are free than there were at the failure; a
  shutdown's fold is retried at once as a plain CP so the clean marker
  still lands; a fold that failed because a map page it had to rewrite
  does not verify blocks every fold until the volume is repaired
  (`EngineError::MapDamaged` for new requests, `Event::SnapshotFailed` for
  the ones it held). A snapshot request waits for the CP that carries the
  generation active when it was made, so the snapshot sees every write
  acknowledged before the request (a fold already in flight does not take
  it). A sealed data segment whose live count reaches zero is freed after
  the same deferral as a metadata region (discarded under
  `WholeSegment`), so a workload that rewrites the same blocks no longer
  fills the pool. A free that lands while a CP past its metadata
  preparation is in flight is re-applied to that CP's table copy when it
  lands.
- **Consequences.** A full pool is reported where it happens; the boot
  and the shutdown always find their room on a pool that has any; a
  policy fold never wedges a pool. Snapshot deletion (the pins) and the
  cleaner for data segments with live blocks remain Step 5.
- **Enforced by** `crates/otap-cp/src/engine.rs` (`step_staging`,
  `should_fold`, `prepare_meta`, `release_freed`, `note_dead_data`,
  `free_segment`, `finalize`), `meta.rs` (the reserve),
  `otap-sim/tests/review_sim_tests.rs`, `adversarial_sim_tests.rs`
  (`a_full_pool_refuses_writes…`,
  `a_free_that_lands_while_a_cp_is_staging…`).

## E32. Containment: a damaged map page, a configuration that disagrees with the disk, a diff's base

2026-10-14. Accepted (from the Step 4 review, defects 2, 6, 7, 8, 9, 13, 14).

- **Decision.** A map page that does not verify answers `Corrupt` to the
  reads of its blocks (and a device read error answers `Failed`), never
  failing the engine's tick; a CP that replaces entries of such a page
  commits with the old entries unknown (their bytes stay counted live, the
  safe direction) and reports the page
  (`OperatorWarning::MapPageDamaged`); folds stop until the volume is
  repaired (E31). A volume's size is its root's; a configuration that
  lists another size refuses the boot (`EngineError::VolumeSizeMismatch`:
  a resize is a CP that rewrites the root, not a boot), and a volume the
  configuration lists that a rewind made absent is created held with
  cause `TornCp`. `snapshot_diff` refuses a base that is not older than
  the snapshot (`EngineError::SnapshotOrder`). A fold folds the records of
  a volume created by the same CP into its pages. The ublk front door
  answers the requests already decoded when it goes offline, clamps a zero
  high watermark to one, and bounds the device's request size by the
  arena (`DeviceSpec::for_arena`).
- **Enforced by** `crates/otap-cp/src/engine.rs`, `otap-map/src/lib.rs`,
  `otap-ublk/src/{frontdoor,device}.rs`, the review's test files.

## E33. Geometry discovery after the review

2026-10-14. Accepted (from the Step 4 review, defects 19 to 23).

- **Decision.** A hypervisor's virtual disk (QEMU, VMware, Hyper-V,
  VirtualBox, Xen) is `Virtual` before any controller marker is tried,
  and a short controller marker (`LSI`, `ASR`, `AAC`, `MR9`) must be a
  whole word of the vendor and model string. A logical block must be a
  power of two dividing 4 KiB. A striped md array without `md/chunk_size`
  is refused (`GeomError::Missing`) rather than given the default stripe.
  A `discard_max_bytes` of zero plans no discards. A device with more
  segments than a `u32` counts gets a larger segment (whole stripes, up to
  the format's bound) with a warning, or is refused; the layout refuses
  rather than clamps (`LayoutError::TooManySegments`). A file backstore
  sizes a block device by its end and refuses an empty write.
- **Enforced by** `crates/otap-geom/src/lib.rs`, `otap-cp/src/layout.rs`,
  `otap-backstore/src/file.rs`, the review's test files.

## E34. The owner's Step 5 rulings (arena and CP cadence, snapshot deletion, the cleaner, the daemon) and how their labels map onto Part 9

2026-10-14. Accepted (the owner's Step 5 brief, section 1; the brief is
verbatim in `10-phase-c-step-briefs.md`).

- **Context.** The Step 5 brief opens with four binding rulings labelled
  D3, D5, D6 and D14 and asks that they be recorded before any Step 5
  code, adding: "If your internal numbering in `05`/`06` assigns any
  adjacent scope to D3, D5, D6, or D14, map the corresponding ruling
  above and document the mapping cleanly in `07`." The numbers do not
  match Part 9 of `05-architecture-brainstorm-03.md`, where D3 is the
  v0.1 control-plane store (redb behind `StateStore`), D5 the nvfail
  class table, D6 the identity after an unplanned failover and D14
  `nvfail_force_lag`. None of those four is answered by the brief. The
  brief's rulings are recorded here as **S5-1 to S5-4**, each with the
  Part 9 or round-3 item it actually touches.
- **S5-1 (the brief's "D3"): arena sizing, the hugepage fallback, CP
  cadence, the platform baseline.** The production arena stays an 8 GiB
  hugetlbfs arena of 2 MiB pages; `dbladed` must take any arena size
  down to 64 MiB and a `--heap-arena` flag (aligned heap memory, no
  hugepage reservation) for homelab hosts and CI VMs. The CP timer
  defaults to 5 s, configurable 1 to 15 s, or 50% active-generation
  fullness; back-pressure paces writes from 80% arena fullness and stalls
  them at 95%. Debian 13 (Trixie, kernel 6.12 or later) is locked as the
  Linux baseline and the Windows host remains a mandatory test target.
  Part 9 mapping: not D3. It settles the arena arithmetic of sections 1.3
  and 6.4 ("every size in the memory table is a profile value"), the CP
  interval rule, the CI image of section 8.3 and the back-pressure delay
  curve of week 6. A heap arena does not survive the daemon: a crash
  under `--heap-arena` is a dirty boot that holds the write-active
  volumes under nvfail, which the flag's documentation says; a clean
  shutdown is unaffected. Part 9's D3 (the control-plane store) stays
  open: Step 5's daemon keeps every durable fact in the pool (S5-4's
  catalogue) and holds no store of its own, which narrows what a later
  `StateStore` has to persist.
- **S5-2 (the brief's "D5"): snapshot deletion, retention, replication
  pinning.** Up to 255 user snapshots per volume plus reserved slots for
  replication baselines. Deleting a snapshot unpins its record at the
  next CP and decrements `SegEntry::live_bytes` exactly for the extents
  the snapshot alone referenced, judged against the adjacent older and
  newer snapshots or the live root, never touching a shared block. A
  snapshot pinned as a replication base (`PIN_REPLICATION`) refuses user
  deletion with a clear error until it is unpinned or superseded by a
  newer base. Part 9 mapping: not D5 (nvfail classes). It closes the
  deletion item E25 left open and the "space a snapshot pins returned on
  deletion" item of week 7; the design is E36.
- **S5-3 (the brief's "D6"): the segment cleaner and the reserve.** A
  hard floor of `max(8, total_segments / 12)` free segments is kept from
  front-door writes and reserved for the cleaner's relocation and the
  metadata of folds, so a pool that is full for users can still commit
  the CPs that delete a snapshot, punch holes or compact a segment.
  Background cleaning starts above 70% segment utilisation or on
  `otapctl pool clean`; candidates are data segments ordered by lowest
  `live_bytes`, ties to the oldest `birth_cp`. A live block is relocated
  only when the volume and LBA still map to the exact `(lseg, off)` being
  cleaned, the remapped extents commit through the normal CP, and the
  drained segment becomes `Free` (discarded whole on flash, overwritten
  on rotational media). Part 9 mapping: not D6 (failover identity). It
  settles the week 7 to 8 cleaner and the GC posture the round-3 audit
  had listed as deferred ("cost-benefit GC with relocation tables"):
  the cost-benefit order is adopted, the relocation tables are not. A
  drained segment is freed, so `SegState::Relocated` and
  `Superblock::reloc_index` stay reserved and unused; the design is E37.
- **S5-4 (the brief's "D14"): `dbladed`, the control IPC, `otapctl`,
  the catalogue.** The whole management lifecycle (`pool format`, `pool
  boot`, `pool status`, `pool sync`, `pool clean`; `volume create`,
  `list`, `delete`, `online`, `nvfail-clear`, with resize; `snapshot
  create`, `list`, `delete`, `rollback`; a CP trigger and a GC run) runs
  end to end on Linux (`UringBackstore` and ublk) and on the Windows and
  portable harness (`FileBackstore`, an in-process or loopback TCP
  control channel). The daemon handles `SIGTERM` and `SIGINT` with a
  clean shutdown (the `CleanShutdown` commit) so a planned restart never
  trips nvfail. Human-readable volume and snapshot names are persisted
  in a CRC-protected catalogue record in the pool so they survive
  reboots and crashes. Part 9 mapping: not D14 (`nvfail_force_lag`). It
  settles week 6 (`dbladed`), the `otapctl` surface of R56 for a single
  node, and amends F11: names are on disk now (F16, format 2.3), which
  removes the obligation `06`, section 8, placed on the control plane.
- **Consequences.** D3, D5, D6 and D14 of Part 9 remain unanswered; the
  status document says so in its section 6. The brief's numbering is
  kept in `10-phase-c-step-briefs.md` as written and is referred to here
  only as S5-1 to S5-4.

## E35. Format 2.3: the catalogue, the commit's stripe list, offline volumes, and pages in any directory

2026-10-14. Accepted (Step 5; S5-4 for the catalogue, S5-3 for the stripe
list and the relaxed directory; the rulings are F16 to F18 in the crate
root of `otap-format`).

- **Context.** F11 kept names off disk and made them the control plane's
  problem; S5-4 puts them in the pool. The cleaner writes relocation
  stripes whose blocks keep their old `birth_cp` and CRC (E37), so the
  Step 4 boot rule "derive the stripes a CP wrote from the births in its
  records and pages" (E30) cannot see a relocation stripe at all. A
  rollback, a shrink and a deletion need the volume offline, and offline
  has to survive a reboot. And a CP that relocates blocks a snapshot
  references rewrites that snapshot's map pages outside a fold, which F12
  forbade.
- **What changed.** `FORMAT_MINOR` is 3 (`0x0203`). The superblock gains
  `catalog: RootRef` at byte 1,728 and `cp_stripes: RootRef` at 1,744;
  `reserved` shrinks to 2,332 bytes; `VolState` gains `VOL_OFFLINE`
  (bit 1). Two new region types: the catalogue, 512-byte `CatalogRec`s
  (kind, name length, flags, volume, snapshot uuid, creation time, a
  255-byte name field, 221 reserved bytes, a CRC) in a canonical order
  with unique keys and unique names per scope, names of 1 to 255 bytes of
  UTF-8 without control characters or `/`; and the stripe list, 16-byte
  `CpStripe`s `(pseg, idx, kind)` in strictly ascending order, at least
  one when the root is not `NONE`. `CpLogDirHdr::verify` and
  `CpLogDir::encode` no longer insist that `page_entries` is zero outside
  a fold.
- **What the boot does with them.** It verifies every stripe the list
  names by summary CRC and identity (the CP's nonce included) before it
  trusts the candidate, and no longer walks births for the purpose; it
  verifies the directory's pages in every CP, not only the fold's; it
  loads the catalogue and the offline flags with the roots. A catalogue
  or stripe-list region that does not verify tears the candidate like any
  other root region (E30).
- **A 2.2 pool is refused.** The engine boots 2.3 pools only
  (`BootError::OldLayout`, "this release reads 2.3 and later"): a 2.2 pool
  has no stripe list to verify and no catalogue, and nothing was ever
  released, so no upgrade path is written. The format crate still decodes
  the 2.0, 2.1 and 2.2 vector sets in their lanes, which is what the
  "any minor of its own major" rule of `09`, section 5, protects; the
  engine's refusal is recorded here as the exception. The 2.3 set is
  `crates/otap-format/tests/vectors/v2.3/` with its manifest hash pinned
  in `format_tests.rs`; the older directories stay.
- **Consequences.** The control plane owes nothing for names any more
  (`06`, section 8, drops the F11 obligation). `SegState::Relocated` and
  `Superblock::reloc_index` stay reserved and unused (E34, E37). Any
  later field takes bytes from `reserved` and bumps the minor again.

## E36. Snapshot deletion, pins, rollback, resize, deletion and offline: how the lifecycle is built

2026-10-14. Accepted (Step 5, S5-2 and the "Opus-Storage-Lifecycle" scope
of the brief; closes the deletion item E25 left open).

- **Deletion releases exactly what the snapshot alone referenced.** A
  request (`delete_snapshot`) marks the snapshot and asks for a CP; any
  CP may carry it, a fold is not needed. In `prepare_meta` the deletions
  are processed in id order against a *remaining* set that shrinks as each
  is taken out: for each deleted snapshot the engine walks its pinned
  tree and releases every data entry born after the next older snapshot
  of the volume (what that older one shares, it keeps) which the next
  newer snapshot, or the live map when there is none, does not name at
  the same physical address; `data_live` of the segment goes down by exactly those blocks
  and `audit_liveness` (a walk of every tree, counting per segment) checks
  the result in every simulator recovery. Map pages and page indexes the
  snapshot pinned are freed under one uniform rule (`page_referenced`,
  `index_referenced`, deduplicated by address), so a page shared with a
  neighbour is never freed. The record leaves the snapshot table with the
  commit (`Event::SnapshotDeleted { released }`), and the catalogue entry
  with it. A damaged snapshot (its root page or index did not verify at
  boot) releases nothing when deleted, the safe direction, and the
  engine records an `Internal` event; the blocks leak until a repair.
- **A snapshot taken in the same CP as a deletion sees the new entries.**
  The first version compared the deleted tree against the snapshots as
  they were *after* the CP, which let a new snapshot's blocks count as
  unshared and be released: the one accounting leak the lead's own audit
  found. The comparison runs against the remaining set, new snapshots
  included (`pinned_by(&remaining, ..)`), and the random walk pins it.
- **Pins.** `PIN_USER`, `PIN_SCHEDULE`, `PIN_CLONE` are the operator's;
  `PIN_REPLICATION` marks the replication base. `delete_snapshot`
  refuses while any pin bit is set (`EngineError::SnapshotPinned`, with
  the bits, "unpin it or supersede the replication base").
  `set_snapshot_pins` records a change for the next CP
  (`Event::SnapshotPinned`); `set_replication_base(vol, Some(id))` sets
  the bit on the new base and clears it on the volume's previous base in
  the same CP, so a newer committed base supersedes the old one and the
  old one becomes deletable; `None` releases the base. Pins live in
  `SnapRecord::flags` (format 2.0's field, used at last).
- **Limits.** A volume holds at most 255 user and scheduled snapshots
  (`MAX_USER_SNAPSHOTS`) and 16 replication baselines and divergent heads
  (`MAX_RESERVED_SNAPSHOTS`), pending requests counted
  (`EngineError::SnapshotLimit`).
- **Rollback.** `rollback_volume(vol, snap)` needs the volume offline,
  with nothing of it in the arena or in a CP in flight (`VolumeOnline`,
  `VolumeDirty`), the snapshot its own and readable, and no newer
  snapshot of the volume (`NewerSnapshots`: delete them first, which
  `otapctl snapshot rollback --delete-newer` does). It is done at the
  next CP, a fold: the live root and index become the snapshot's
  (checkpointed at this CP), every live entry born after the snapshot is
  released (`PreparedCp.released`), the volume's size becomes the
  snapshot's, and the rollback generation moves on so a client sees a new
  identity (section 7.6), `Event::VolumeRolledBack`. The CP refuses
  deltas of the volume. The daemon's rollback takes the volume offline,
  flushes it with a CP, waits for quiet, asks, waits for the event and
  brings the volume back (E38).
- **Resize.** A grow is allowed online and may share its CP with the
  volume's deltas; the index is extended with `NONE` entries. A shrink
  needs the volume offline and quiet, releases everything at or beyond
  the new end and rewrites the boundary page with the entries beyond it
  zeroed. Either way the index is rewritten at a fold,
  `Event::VolumeResized`.
- **Deletion of a volume.** Offline, quiet, no snapshots
  (`HasSnapshots`); at a fold everything the volume mapped is released,
  its root, index and catalogue entry go, and its index number may be
  reused (`Event::VolumeDeleted`). Volumes are created by name at the
  lowest free index (`create_named_volume`) and renamed in place.
- **Offline.** `set_volume_online(vol, false)` is immediate: the engine
  answers `Status::Offline` (the ublk door maps it to `ENODEV`) and the
  next CP records `VOL_OFFLINE` so a reboot keeps it. A volume with a
  pending rollback, resize or deletion cannot be brought online until the
  operation commits, and only one such operation waits per volume
  (`VolumeBusy`); the pending operation is dropped if its snapshot
  vanishes before the CP.
- **Rejected alternatives.** Deadlists (E25's plan: a per-snapshot list
  of blocks it alone owns, maintained on every write) were not built: the
  neighbour comparison at deletion time costs a walk of three trees once
  per deletion and keeps the write path untouched, and the format's
  `SnapRecord::deadlist` field stays reserved. A rollback that keeps the
  newer snapshots (branching the volume) was not built: it would need
  the newer trees to become orphans with their own roots, which is a
  clone, not a rollback.

## E37. The cleaner, the reserve, admission and pacing: how a full pool keeps working

2026-10-14. Accepted (Step 5, S5-1 for pacing, S5-3 for the reserve and
the cleaner; closes the "nothing frees a data segment with a live block"
limit of `06` and the dead-segment item of E31).

- **The reserve.** `reserve_for(n) = min(max(8, n / 12), n / 2)` free
  segments are kept from front-door writes (`EngineConfig::reserve_segments`
  overrides it; the review tests of Step 4 pin the old regime with 0).
  Inside it stays E31's hard floor of two segments
  (`META_RESERVE_SEGMENTS`) that no allocation crosses, so a boot or a
  shutdown commit always finds room. Metadata may dip into the reserve
  above the floor by design: that is what it is for, so a CP that deletes
  a snapshot, punches holes or relocates blocks always has its segments.
- **Admission.** A write of `blocks` blocks is taken only when
  `unflushed_blocks + blocks <= (free - reserve) * data_blocks_per_segment
  + the open data segment's remaining blocks`; otherwise it is answered
  `NoSpace` at once (`Stats::admission_refusals`) and nothing is placed.
  `PoolStatus::admissible_blocks` is what clients may still write. The
  rule counts what is acknowledged and not yet on disk, so a burst cannot
  acknowledge more than the next CPs can place outside the reserve.
- **Pacing.** `Pacing { pace_at: 0.80, stall_at: 0.95, max_delay_ns:
  50 ms }`: from 80% arena occupancy an acknowledgement is delayed by a
  quadratic curve to 50 ms at 95%, in a FIFO the engine completes on later
  ticks; at 95% and above the acknowledgement waits until occupancy falls
  (a CP). Reads are never paced. The simulator runs with
  `max_delay_ns = 0` because its clock does not advance by itself; a
  named test drives the curve.
- **When the cleaner runs.** `CleanerPolicy { utilization: 0.70,
  max_segments_per_cp: 2, background: true, margin_segments: 4,
  max_live_fraction: 0.80 }`. `plan_clean` runs when a CP job starts
  (never for the boot or shutdown commits, never while a snapshot
  deletion or a volume operation is pending, never while a map page is
  damaged): on request (`request_clean`, `otapctl pool clean`,
  `CpTrigger::Clean`), or in the background when segments in use exceed
  70% of the pool, or whenever the pool is *tight* (free segments down to
  the reserve plus four), when the budget doubles. Candidates are data
  segments ordered by lowest live blocks then oldest `birth_cp`, excluding
  the open segment, segments protected since the boot, segments awaiting
  a discard, dead segments, and segments the scan found uncleanable; a
  segment above 80% live is not a candidate at all. The first version
  had no such bound and chased its own tail: a segment filled by one
  cleaning became the next cleaning's candidate.
- **What a relocation does.** `cleaner::scan` reads the candidate's
  stripes and, through the `References` trait, keeps only the blocks
  whose `(vol, lba)` still maps to exactly this `(lseg, off)` in the live
  map or in a snapshot, skipping blocks the same CP replaces and holes
  (S5-3's "never overwrite a newer write"). The live blocks are written
  as relocation stripes (`StripeKind::Relocate`) carrying their old
  births and CRCs, their map entries are rewritten as deltas (live map)
  or by `rewrite_pinned` with a new root page (snapshots), the CP lists
  every stripe it wrote in `cp_stripes` (F17) so the boot verifies them,
  and `data_live` moves from the old segment to the new. The drained
  segment is forced to zero live (an `Internal` event records any drift)
  and freed after the deferral like any other: a whole-segment discard on
  flash, overwrite on rotational media. Relocation tables are not used
  (E34): nothing on disk says a block moved; the map simply names the new
  address.
- **A full pool pumps itself.** `maybe_pump` runs consistency points of
  its own (`CpTrigger::Clean`, no generation) while the pool is full or
  tight and there is something to gain (deferred frees to advance, or
  candidates to compact), at most three in a row until something is
  freed, so a pool that refuses writes recovers without a client's help
  and a boot on a full pool does not wedge. The space check before a
  relocation (`free >= stripes needed + 1 + the floor`) keeps the pump
  from eating the floor.
- **What the tests prove.** A fragmented pool (4 KiB overwrites) at 95%
  keeps accepting writes with the cleaner on and refuses them with it
  off; a pool full for users still punches, deletes a snapshot, cleans
  and serves writes again; relocation never clobbers a write that landed
  since; the admission rule keeps the reserve free; the random walk mixes
  cleaning, deletions, rollbacks and crashes with `audit_liveness` after
  every recovery (`step5_sim_tests.rs`; the review's `review_step5_sim_tests.rs`).
- **Limits.** The scan reads whole segments through the backstore and
  compares against every snapshot tree; on a 100 TiB pool with many
  snapshots that is the cost of a cleaning CP, and the incremental tables
  of a later step are where it is paid down. The 80% bound means a pool
  whose every segment is 80% to 100% live cannot be compacted, which is
  the right answer (there is nothing to gain) but a pool can reach
  `NoSpace` with free space scattered inside such segments.

## E38. `dbladed`, the control protocol, `otapctl` and the catalogue in the daemon

2026-10-14. Accepted (Step 5, S5-4 and the "Opus-Daemon-CLI" scope).

- **Three crates.** `otap-control` (the protocol: `Request` and `Response`
  as internally tagged JSON, `op` and `kind`, a `ControlError` with an
  `ErrorCode` the CLI acts on; 4-byte little-endian length frames bounded
  at 64 MiB, 4 MiB since E39; the `Endpoint` syntax `unix:<path>` or
  `tcp:<addr>:<port>`, with `/run/opentap/dbladed.sock` the Linux default
  and `127.0.0.1:7421` elsewhere; a blocking `Client`; the shared size and
  bound helpers; the format's `BLOCK_SIZE` and `PIN_*` re-exported so no
  client redefines them), `otap-daemon` (`dbladed`) and `otap-ctl`
  (`otapctl`), all `#![forbid(unsafe_code)]`. The protocol carries no
  data path; the `Io*` requests move whole blocks as hexadecimal (at most
  256 per request) for the smoke test and the portable harness, where no
  block device exists.
- **One engine thread.** The daemon's engine thread owns the `Engine`
  over `Box<dyn Backstore>` and a composite front door (the channel the
  `Io*` requests feed, and on Linux one `UblkFrontDoor` per online
  volume, polled round-robin); it ticks the engine, and executes every
  control request between ticks, so nothing else ever touches the engine
  and no lock exists. A request that needs a consistency point ticks the
  engine until the event it waits for arrives (`VolumeCreated`,
  `SnapshotCreated` or `SnapshotFailed`, `SnapshotDeleted`,
  `VolumeRolledBack`, `VolumeResized`, `VolumeDeleted`, `NvfailCleared`,
  `SnapshotPinned`, `CpCommitted`, `Stopped`), within `--op-timeout`
  (120 s by default), and the data path keeps flowing because those
  ticks serve it; requests whose durability has no event of its own
  (online and offline, a rename, a replication base) wait for quiet,
  request a CP and wait for its commit. Requests are answered one at a
  time, so a long operation (a rollback, a shutdown) delays the next
  request behind it; the control plane is not a hot path. When the last
  tick did nothing the thread waits for a request with a backoff from
  200 µs to 2 ms, so an idle daemon costs little and a request on a
  device waits at most 2 ms; the busy-polled reactor of E16 is a later
  step.
- **Devices.** On Linux a ublk device is created and started on the
  engine thread (the driver pins the queue to the task that issued its
  first fetch) for every online volume at boot and at `volume create`
  or `volume online`; it is taken down at `volume offline`, before a
  rollback, at `pool stop` and at exit (the front door answers `ENODEV`
  while the requests the engine holds drain, then `STOP_DEV` and
  `DEL_DEV`), and recreated at the new size after a resize. The device
  advertises 4 KiB blocks, a volatile cache with FUA, discards, and
  requests bounded by half the arena. `--no-ublk` serves no devices;
  Windows never does.
- **Boot and format.** `pool format` writes a fresh pool with a new
  aggregate identity (sixteen bytes of a BLAKE3 digest over the clocks,
  the process and a counter, with the UUID version bits) and refuses a
  device that already holds a pool unless `--force`. `pool boot` probes
  the first superblock slot (and `A1` at every candidate stripe when
  `A0` is torn) for the aggregate's identity and the recorded geometry
  (F15), opens the device under that geometry (an operator override that
  disagrees is refused), opens the arena (`--arena-file` on hugetlbfs,
  re-attached with its contents; `--heap-arena` on heap memory, which
  dies with the process), boots the engine with no configured volumes
  (every volume comes from the disk), ticks until the boot commit is
  durable and exposes the devices. A block device goes through the
  geometry planner and `UringBackstore`; a file gets a 64 KiB stripe and
  2 MiB segments below 1 GiB, a 1 MiB stripe and 64 MiB segments above,
  flash media and no discards unless told otherwise, and `FileBackstore`.
  The CP timer is `--cp-interval` (1 to 15 s, 5 by default), set on the
  profile before the engine sees it.
- **Shutdown.** `SIGTERM` and `SIGINT` set a flag the engine thread polls
  (`signal-hook`); `pool stop`, `daemon stop`, the flag and the loss of
  the last handle all run the same path: every device down, the engine's
  clean shutdown, the `Stopped` event. A final CP that found no segment
  (`clean == false`) is reported as an error and the daemon says what it
  means (a hugetlbfs arena keeps the writes, a heap arena loses them).
- **The server.** Unix socket (mode 0660; a stale socket nobody answers
  is taken over, a live one refused) or TCP; one thread per connection,
  one request per frame; a frame that is not a request gets a
  `BadRequest` reply and the connection goes on; a reply that does not
  encode gets an error reply instead of a dropped connection. The
  protocol test round-trips every request and reply variant through
  JSON, which caught two serde facts the first version tripped on: an
  internally tagged enum cannot serialize a newtype variant that holds a
  bare sequence (the list replies are struct variants), and a report
  field named `kind` collides with the tag (the snapshot's is
  `consistency`).
- **`otapctl`.** `otapctl [--control E] [--json] [--timeout S] <group>
  <verb> --flags`: `daemon ping|stop`; `pool format|boot|status|sync|
  clean|stop`; `volume create|list|show|delete|online|offline|resize|
  rename|nvfail-clear`; `snapshot create|list|delete|rollback
  [--delete-newer]|pin|unpin|replication-base`; `io write|read|punch`.
  Sizes take `K`, `M`, `G`, `T`; `--json` prints the reply as the daemon
  sent it; exit status 0 for a reply, 1 for a refusal, 2 for a command
  line that does not parse, 3 when the daemon cannot be reached. The
  brief's `vol`, `snap`, `cp trigger` and `gc run` are `volume`,
  `snapshot`, `pool sync` and `pool clean`.
- **Names.** Every name lives in the pool's catalogue (F16): the daemon
  resolves names to indexes and ids on every request and keeps no table
  of its own, so a crash or a reboot cannot lose or confuse a name; the
  daemon holds no durable state at all beyond its command line.
- **Tests.** The daemon runs in process (`Daemon::new` and `handle`)
  against a file: format, boot, the whole lifecycle, a clean restart that
  keeps names and content, a dirty restart that holds the volume under
  nvfail until cleared, the refusals, the boot at start, the signal
  flag, and the server over loopback TCP with two clients and a bad
  frame; `otapctl` runs as a process against that daemon. The Linux path
  (`UringBackstore`, the devices, the Unix socket, the signals under
  systemd) is `scripts/linux-smoke.sh`, written for Debian 13 and never
  run: there is still no Linux host.
- **Not done.** No `UBLK_F_USER_RECOVERY` re-attach of a device after a
  daemon crash: the device is created with the flag, so the driver keeps
  a killed daemon's devices quiesced; the restarted daemon creates new
  ones (the `/dev/ublkbN` numbers change) and the old ones linger until
  a tool deletes them or the module is reloaded. A clean stop deletes its
  devices. One queue per device. No systemd units. No `--write-journal`.
  The control plane is a single serialized thread and the `Io*` path is
  a test aid, not a data path.

## E39. After the Step 5 review: the cleaner, the lifecycle and the daemon decide together

2026-10-14. Accepted (the Step 5 adversarial review,
`reviews/2026-10-14-step5-qa-adversarial.md`; the findings and their fixes
are in the Step 5 entry of `08-engineering-log.md`). Amends E36, E37 and
E38; nothing here changes the format.

- **The cleaner and the lifecycle exclude each other both ways** (D1, W3).
  E37 had the cleaner refuse to plan while a snapshot deletion or a volume
  operation waited; a request that arrived *after* the plan rode the same
  CP, and a rollback took the snapshot's root as it was before that CP
  rewrote it, so the live map named a segment the CP drained. Now a CP
  that carries clean targets takes no deletion and no volume operation
  (`prepare_meta`, `should_fold`): what arrived after the plan waits for
  the next CP, which plans no clean while it is pending. The daemon's
  waits see the event one CP later.
- **A snapshot in flight blocks the volume's operations** (D2, D4). A
  rollback, a resize or a deletion refused only the snapshot requests still
  waiting; one the CP in flight had already taken was in neither list, so
  the operation landed behind a snapshot newer than the state it was asked
  for (a forked history, whose deletion then released live blocks). All
  three now refuse with `VolumeBusy` while a snapshot request of the volume
  waits or rides the CP in flight (`snapshot_in_flight`).
- **A summary that does not parse is damage, not absence** (D3). The scan
  skipped a stripe whose summary failed its CRC as never written and the
  drain freed its live blocks. A summary of zeros is a stripe never
  written; anything else that does not parse skips the segment
  (`SkipReason::Summary`). And the scan reports every referenced block it
  saw (`Scan::referenced`, the CP's own replacements included): a count
  below the table's live count means a stripe it could not read as written
  vouches for blocks it cannot see, and the segment is skipped, not
  drained.
- **A deletion beyond the volume's end releases, not fails** (W1). The
  deletion's lookups in the newer neighbour or the live map answered
  `OutOfRange` for a block beyond a since-shrunk volume, which failed the
  CP, and every CP after it (the deletion stayed pending): the daemon's
  stop could not write its clean record. `OutOfRange` is "not referenced"
  in both lookups. A damaged newer neighbour, which cannot be walked, makes
  the deletion release nothing (the safe direction, recorded).
- **The plan budgets the rewrites, shares them, waits after a failure, and
  a failed attempt gives its segments back** (W2). The cleaning CP needed
  a copy of every touched page, an index and a root page per snapshot that
  referenced a relocated block, which the plan never counted: with 160
  snapshots sharing one tree the CPs failed for space, the pool was marked
  full while the admission rule still admitted, and every retry re-staged
  into segments the failure never returned. Now: `rewrite_cost` counts the
  pages per distinct tree, an index per tree, a root page per snapshot and
  the live deltas' records, and candidates that do not fit are not taken;
  snapshots that share a tree (the same index region) are rewritten once
  and point at the one copy, so a clean never un-shares a tree; a cleaning
  CP that fails for space sets `clean_blocked_at` and no clean is planned
  until a segment is freed (the request stands); the data segments an
  attempt allocated (`job_data_segments`) are freed at once when it fails,
  since no committed CP names them.
- **A damaged catalogue drops the names, not the pool** (W4). Every
  candidate of the ring names the one catalogue region (F16 carries it
  forward), so a CRC failure in it tore every candidate. The boot now goes
  on with no names, `OperatorWarning::CatalogDamaged` says so, the next
  naming CP writes a fresh region, and the daemon addresses a nameless
  volume as `#<index>` so it can be renamed. A read error of the region
  still fails the candidate, as every root's does.
- **The daemon probes the whole ring** (W5): `A0`, then for each candidate
  stripe `A1` and the two `B` slots at the end of the device (F7), so a
  pool whose start was wiped boots, and a format without `--force` cannot
  overwrite it.
- **`pool format` refuses anything but a blank device without `--force`**
  (R2): a first or last MiB with a non-zero byte is someone's data.
- **`snapshot rollback --delete-newer` checks before it deletes** (R1):
  every newer snapshot's pins and the target's state are checked first;
  the deletions are issued together and waited for together; a refusal
  leaves the history whole.
- **Unknown fields are refused** (R3): `deny_unknown_fields` on every
  request, reply and report, so a misspelled optional field cannot read as
  its absence (`nmae` is not "release the replication base").
- **The walks' case count was never the gate's problem** (C1, cited, not
  reproduced). The review read `cases: 64` in the Step 4 and Step 5 random
  walks and concluded that the 10,000-case gate had run them at 64. It had
  not: the `proptest!` macro passes an explicit configuration through
  proptest's `contextualize_config`, which applies `PROPTEST_CASES` over
  any explicit count, and the Step 4 gate's 52-minute `step4_sim_tests`
  (20 seconds at 64 cases) had shown it. Both walks now read the variable
  themselves, 64 the default, so the intent is in the file rather than in
  proptest's source; the gate's behaviour is unchanged (53 minutes).
- **Hazards the review saw, closed with it:** a full pool with a pending
  deletion or volume operation runs a CP for it (`maybe_pump`); `pool
  sync` and `pool clean` on a full pool request the CP instead of waiting
  for a quiet the pool cannot reach, and answer the attempt's failure
  rather than a timeout; a signal interrupts an operation's wait (the
  shutdown's own waits excepted), so `SIGTERM` is not delayed by the
  operation timeout; a TCP control address that is not loopback is refused
  without `--allow-remote-control`; frames are bounded at 4 MiB and an
  idle connection is closed after five minutes; the catalogue check is
  linear; the daemon's `snapshot create` waits for its own uuid, not any
  snapshot of the volume; the arena is capped at 256 GiB; `otapctl` waits
  ten minutes for a reply by default.
- **Hazards left as limits** (`06`, section 8): a block that already fails
  its CRC is relocated under its recorded CRC and, under a trusted opaque
  barrier, makes the relocating CP look torn at an unclean boot; a
  rewritten snapshot root page is not verified as the CP's own, so under
  an opaque barrier a lost one reads as a damaged snapshot rather than a
  torn CP; the catalogue and the snapshot table are rewritten whole in
  every CP that changes them.
- **Lesson** (also in the log): the cleaner's plan, the snapshot set and
  the volume operations were decided at three different moments of one CP
  (the plan at the cut, the deletions and operations at the metadata, the
  in-flight snapshot at the commit) and nothing stopped a request that
  landed in between. Every decision a CP makes about shared state must be
  made against the same view, or refused until the view settles.

## E40. What the 6.12 ublk driver does when its server dies, and what a recovery must meet

2026-10-16. Accepted (Step 6B-1; read in the saved 6.12 `ublk_drv.c` and
`ublk_cmd.h`, kept outside the tree, as E28 recorded its facts, before the
mock models them).

- **A server that dies with a fetch pending.** io_uring cancels the fetch
  (`ublk_uring_cmd_cancel_fn`), the abort runs (`ublk_abort_requests`,
  `ublk_abort_queue`) and the device is quiesced (`__ublk_quiesce_dev`,
  state `QUIESCED`). A request the dead server held is failed through
  `__ublk_fail_req`: requeued under `USER_RECOVERY_REISSUE`, `EIO`
  without it; a request dispatched but not yet delivered is requeued
  either way.
- **A server that dies holding every tag.** Nothing is pending, so nothing
  is cancelled: the device stays `LIVE` with a dead pid until the block
  layer's request timeout fires `ublk_timeout`, and the driver sets no
  timeout of its own, so blk-mq's default of 30 s applies.
- **`START_USER_RECOVERY`** answers `EINVAL` without `USER_RECOVERY` or
  with no ready queue, and `EBUSY` while the old character device is still
  open (`UB_STATE_OPEN`, cleared when the dead process's file is released)
  or the device is not `QUIESCED`; then it reinitialises the queues. The
  new server opens `/dev/ublkcN` (exclusive), maps the descriptors and
  issues a `FETCH_REQ` per tag.
- **`END_USER_RECOVERY`** waits for those fetches (`EINTR` if interrupted),
  answers `EBUSY` unless `QUIESCED`, records the new pid, unquiesces,
  kicks the requeue list and returns the device to `LIVE` under the same
  number. A server killed after `START` and before its last fetch leaves no
  ready queue, so `START` answers `EINVAL` from then on: that device can
  only be reaped. One killed after its fetches and before `END` leaves the
  device `QUIESCED` with a ready queue, and the next `START` works.
- **`STOP_DEV`** on a `QUIESCED` device unquiesces with a forced abort (held
  I/O fails); on a `LIVE` one it quiesces first, waiting for started
  requests, so a device still quiescing blocks it for up to 30 s.
  `DEL_DEV` with its wait blocks while any `/dev/ublkcN` is open;
  `DEL_DEV_ASYNC` does not. Neither checks an owner.
- **Identity.** The driver lists no devices (they appear in the `ublk-char`
  class); `GET_DEV_INFO` needs `CAP_SYS_ADMIN`; `ublksrv_flags` is copied
  at `ADD_DEV` and never read, so it is the only identity a device keeps
  across its server's death. `ublks_max` (64) counts every device until its
  `DEL_DEV`.
- **Consequences.** The recovery (E48) waits for `QUIESCED`, retries `EBUSY`,
  fetches every tag from the serving thread before `END`, and gives up on
  a device that will not quiesce within its budget; the reaper (E49) never
  stops a device that is still quiescing. `MockKernel` in `otap-ublk`
  models these rules and is what the Windows host tests run against.

## E41. A dirty boot writes nothing to the arena until its commit has landed

2026-10-16. Accepted (Step 6A, candidate 1 of `step6/README.md`; reproduced
before the fix, three causes).

- **Context.** A dirty boot's hold on the write-active volumes becomes
  durable only with the boot commit's `A` slot (the volume table's
  `nvfail_cause` rows and `sb.nvfail`). `Engine::boot` had already written
  the arena by then: the attach rewrote damaged, filled and reserved slots
  `Committed`, and `Arena::create` formatted a blank or lost arena. A
  second `kill -9`, an `--op-timeout` in `Pool::boot`'s wait or a failed
  boot commit before `A` landed left the next boot a whole arena and no
  cause to hold, so it served volumes that had lost acknowledged writes,
  without nvfail.
- **Decision.** The order, not the layout: `otap-arena` gained
  `attach_deferred` and `create_deferred` (which write nothing),
  `apply_pending_writes`, `has_pending_writes` and `release_after_attach`
  (the replay's releases join the pending writes); `reserve` answers
  `ArenaError::Pending` meanwhile. The boot uses them, defers the replay's
  orphan, superseded and unreplayable releases as well, and `step_commit`
  applies the pending writes when the boot commit's `A` slot completes. A
  boot stopped at any point leaves the memory as it found it, so any
  number of kills keeps the hold. A kill between `A` and the apply only
  holds again, which is harmless.
- **Rejected.** The arena map's "pending hold" word in the arena header: a
  layout change (arena 1.3) for state the evidence already carries, since
  the damaged slots themselves survive untouched until the hold is
  durable.
- **Enforced by** `step6a_sim_tests`
  (`a_boot_killed_before_its_commit_keeps_the_hold_a_{damaged_slot,blank_arena,lost_arena}_calls_for`,
  `consecutive_kills_during_the_boot_never_lose_the_hold`,
  `kills_during_the_boot_of_a_whole_arena_lose_nothing_and_hold_nothing`)
  and `step6a_arena_tests` (`a_deferred_{attach,create}_writes_nothing_until_it_is_applied`,
  `a_release_during_a_deferred_attach_waits_for_the_apply`).

## E42. The re-attach judges a slot by its neighbours of the same write

2026-10-16. Accepted, amended by E60 (the review's D10 and R5) (Step 6A, candidate 2; reproduced: the ring stalled
`Full` at write 125 of the first lap).

- **Context.** A fill stores a slot's fields, then its sealed word. A kill
  between the two left a `Free` slot under the reservation's stale seal,
  which the attach neither released nor reported; behind a kept `Placed`
  slot the tail stopped there and every write within a lap stalled while
  `damaged_slots` read 0. And a kill inside `place` left a `Placed` prefix
  of a write whose later slots were still `Filled`, which the replay
  served: the torn prefix of a write that was never acknowledged.
- **Decision (run continuity).** A stale-sealed `Free` slot is judged by
  its neighbours of the same write (sequence, volume, kind, lap, adjacent
  LBA): a `Placed` slot before it, or any slot after it that is not
  `Free`, means the fill went past it, so it is damage
  (`Damage::Descriptor`, and the boot holds; this case was a silent loss
  before). Otherwise it is an interrupted fill, released `Committed` on
  its lap when it lies below the head and never noted (an unverified lap
  must not move the head), counted in `Reattach::interrupted_fills`; at
  or past the head it waits for the next `reserve`. A `Placed` slot whose
  write still has a verifying `Filled` slot is dropped with the whole
  write (`Reattach::torn_places`).
- **Consequences.** The replay no longer serves a torn prefix: a write the
  kill caught inside `place` is either whole or absent, both within the
  contract, since it was never acknowledged. The arena walk's
  `KillMidPlace` model in `adversarial_arena_tests` changed to expect the
  prefix dropped, and the walk gained `KillInsideFill`.
- **Enforced by** `step6a_arena_tests`
  (`a_fill_killed_{between_a_slots_fields_and_its_word,on_its_first_slot}_does_not_stall_the_ring`,
  `a_fill_killed_on_its_last_slot_is_left_to_the_next_reservation`,
  `a_placed_word_damaged_to_read_free_is_reported_as_damage`,
  `a_place_killed_midway_drops_the_whole_write`) and the arena walk.

## E43. A damaged snapshot is quarantined, not fatal; deleting it releases only what can be read

2026-10-16. Accepted, amended by E60 (the review's D8) (Step 6A, candidate 3, worse than the handoff said;
extended by Step 6B-2's `SNAP_UNCERTAIN`, E51).

- **Context.** The boot verifies a snapshot's root page and index only, so
  `damaged: false` said nothing about its map pages. The deletion walk's
  `pinned_diff(...)?` failed on a page that did not verify: in a plain CP
  the error escaped `tick()` every tick and no CP of any volume committed
  again; in a fold CP it would have set `fold_damaged` and stopped folds
  pool-wide.
- **Decision.** `MapSet::pinned_diff_tolerant` skips and returns damaged
  pages. `Engine::deletion_walk` releases the blocks of the pages it can
  read, keeps the damaged pages' blocks counted live (the safe direction,
  as E36 did for a damaged root), and the deletion commits; a read error
  defers that one deletion to the next CP instead of failing the CP. A
  snapshot found damaged is quarantined in memory (`SnapState.damage`,
  `OperatorWarning::SnapshotDamaged`, `damaged` in the snapshot list):
  reads, diffs, new rollbacks and replication refuse it; it stays
  deletable and keeps pinning. The deletion, the cleaner's
  `DamagedSnapshot` skip, `snapshot_read`, `snapshot_diff` and
  `audit_liveness` set it; the warning leaves with the snapshot.
- **Rejected.** A `CORRUPT` bit in `SnapRecord.flags`: a format change with
  the forward hazard of E50 for state the next walk finds again. (Step 6B-2
  then added a persisted mark for a different cause, a record recovered
  from a damaged table chain, E51.) And dropping a rollback accepted before
  the quarantine: E32 contains the page, and dropping it would make
  `finalize` report a rollback that did not happen.
- **Enforced by** `step6a_sim_tests`
  (`deleting_a_snapshot_with_a_damaged_map_page_does_not_wedge_the_pool`,
  own page and a page shared with the live map;
  `a_snapshot_found_damaged_is_quarantined_and_the_pool_goes_on`).

## E44. `START_DEV` is begun, then polled between engine ticks

2026-10-16. Accepted (Step 6A, candidate 4; the ordering deadlock
reproduced on a mock; one kernel link unverified).

- **Context (the saved 6.12 `ublk_drv.c`).** For a privileged server
  (`CAP_SYS_ADMIN` at the last fetch) partition scanning stays on, and
  `add_disk` runs inside `START_DEV`; control commands run in io-wq; the
  scan's read reaches the server as task work on the fetching task, which
  was the engine thread, blocked waiting for `START_DEV`. Unverified,
  because `genhd.c` was not saved: that `add_disk` reads the partition
  table synchronously. If it does, a root `dbladed` could not create any
  device, and every Linux lane presupposed the fix. The same map found
  `create_device`'s failure path deleting the device while the front door
  still held `/dev/ublkcN`, so `DEL_DEV` waited forever.
- **Decision.** `UblkControlRing::begin` and `poll` (bufferless, not waited
  for; `execute` refuses meanwhile; one new entry in the unsafe
  inventory); `UblkDevice::begin_start` and `poll_start`;
  `otap_ublk::start` (`DeviceControl`, `start_while_serving`,
  `StartError`): the fetches are submitted, the front door joins the
  engine, `START_DEV` is begun, and the engine ticks with polls between.
  The daemon's `create_device` no longer starts; `Pool::start_device` does,
  under the operation timeout and the stop flag; `Pool::abandon_device`
  drops the front door before `DEL_DEV`. The re-attach uses the same loop
  for `END_USER_RECOVERY` (E48).
- **Consequences.** After a timeout `abandon_device` waits up to 5 s for the
  start, then logs a device left to the operator (the reaper lists it,
  E49); unmeasured on Linux.
- **Enforced by** `step6a_ublk_tests` over `MockStart` (the linked queue,
  then a read of LBA 0 that must be answered):
  `start_dev_completes_while_its_queue_is_served`,
  `start_dev_waited_for_on_the_serving_thread_never_completes`, and five
  edge cases.

## E45. An arena of another size is refused unless nothing can be lost; `--discard-arena` is the operator's override

2026-10-16. Accepted, amended by E60 (the review's W3) (Step 6A, candidate 5; both causes reproduced on the
engine as it was).

- **Context.** `map_file` grew the arena file, `verify` then failed
  `BadGeometry`, and the boot recreated the arena over its placed writes; and
  `can_attach` checked the geometry before the aggregate, so another
  pool's arena file under a larger size was formatted over rather than
  refused.
- **Decision.** One rule, `otap_cp::check_arena_resize`: an arena is
  resized only when nothing in it is placed and the pool's newest CP is a
  clean shutdown with nothing rewound (`BootState::is_clean`), or when the
  operator discards it; otherwise `EngineError::ArenaResize`, with the
  memory untouched. `map_file` refuses a length change of a non-empty
  file (`map_file_resizing` is the explicit form); `can_attach` checks
  identity, then aggregate, then geometry; `Arena::survey` reads an arena
  under its recorded geometry without touching it. The daemon's
  `open_pool_arena` maps a mismatched file at its own length, judges it
  (never another pool's arena; memory that holds no arena freely; the
  superblock's clean state through `boot::evaluate`), then resizes.
  `dbladed --discard-arena` (it needs `--device`; `PoolBoot` over the
  control socket never discards) recreates the arena, reports
  `arena_lost` and holds the write-active volumes.
- **Consequences.** "Dirty" is the pool's superblock, so an empty arena of a
  pool that stopped uncleanly is refused too; the operator restarts at the
  old size, stops cleanly, then resizes. A command-line flag rather than a
  protocol field, which would have been a wire change.
- **Enforced by** `step6a_sim_tests`
  (`a_larger_arena_over_placed_writes_is_refused_not_recreated`,
  `a_larger_arena_after_an_unclean_stop_is_refused`,
  `a_larger_arena_after_a_clean_shutdown_is_recreated`,
  `a_discarded_arena_is_recreated_and_its_volumes_held`,
  `another_pools_arena_under_another_size_is_refused`), four survey tests
  in `step6a_arena_tests` and nine in `step6a_daemon_tests`.

## E46. Devices are created with `UBLK_F_USER_RECOVERY_REISSUE`, and it is required

2026-10-16. Accepted (Step 6B-1).

- **Context.** Without `REISSUE` every request the dead daemon had fetched
  and not answered fails with `EIO` when the device is quiesced (E40); with
  it the kernel requeues them for the next server. The flag is fixed at
  `ADD_DEV`.
- **Decision.** `REQUIRED_UBLK_FEATURES` includes `USER_RECOVERY_REISSUE`,
  `create` asks for it, and a driver that does not grant every required
  flag is refused at create. The argument that re-executing is safe:
  every request the front door serves is idempotent at the block level. A
  re-executed write puts the same bytes at the same LBAs under a sequence
  above every replayed one (`Placed` is set before
  `FrontDoor::complete`, so an acknowledged write replays from a
  re-attached arena; a reissued one is placed again and wins its range;
  any write it overtakes was concurrent with it, so either order was
  allowed); reads, flushes, discards and writes of zeros are harmless
  twice. No request identity fits arena layout 1.2, and none could be
  matched anyway: the kernel redispatches with no marker and possibly on
  another tag.
- **Rejected.** Running without `REISSUE`: a write durable in the arena
  whose completion had not reached the kernel would fail `EIO`, which
  aborts a filesystem, for a request the engine had in fact kept.
- **Consequences.** Devices made by Step 5 binaries (no `REISSUE`, no
  stamp) list as stale and are never re-attached (E49). On a heap-arena
  restart the volumes are held (S5-1), so reissued requests get `EIO`,
  delivered once.
- **Enforced by** `step6b_recovery_sys_tests` (REISSUE required) and
  `step6b_recovery_ublk_tests` (reissued writes land once, none lost,
  through the engine over a kept arena).

## E47. A device's identity: a stamp in `ublksrv_flags`, and a host-local binding file in `--state-dir`

2026-10-16. Accepted, amended by E60 (the review's D11: the record is kept on every boot) (Step 6B-1).

- **Context.** The driver lists no devices, and `ublksrv_flags` (64 bits
  copied at `ADD_DEV` and never read) is the only identity it keeps
  across a crash (E40). Device numbers are host-local and meaningless in
  another host's kernel, so they do not belong in the pool.
- **Decision.** The stamp is `0x07A9 << 48 | crc32c(agg_id) << 16 | vol`.
  The record is `<state-dir>/ublk-<agg_id>.bindings`, by default under
  `/run/opentap` (tmpfs: it dies with the kernel whose numbers it
  records): a 40-byte header (`OTAPBIND`, version, entry size 32, at most
  2,048 entries, the aggregate, the writer's pid), 32-byte entries keyed by
  device number (device, volume, size, bound or stale, a reason code) and a
  CRC-32C, verified before use, written by temporary file, `fsync`,
  rename and directory `fsync`; an empty record removes the file; an
  unreadable one is logged and rebuilt from the stamps. It is written at
  create (before `START_DEV`), at recovery, teardown, when a device turns
  stale, and at reap. `dbladed --state-dir` names the directory.
- **Consequences.** The stamps are the authority and the record a hint; a
  host reboot loses the record and, with it, nothing a stamp cannot
  rebuild. A volume index above 65,535 cannot be stamped; the engine's
  volume table holds 1,024.
- **Enforced by** `step6b_recovery_daemon_tests` (the record codec, the
  record file, a stamp without a record, a recorded volume gone, a reused
  number left alone).

## E48. The re-attach at boot: refuse a second server, recover each device in turn, give up after `--recovery-timeout` (35 s)

2026-10-16. Accepted (Step 6B-1; the default and two rules amended by the
Step 6 review, E60).

- **Decision.** `otap_ublk::Recovery`, driven by `Pool::boot`:
  1. Before the pool is opened, the boot is refused if a live process other
     than this one serves a device stamped with this pool (a second daemon
     on it).
  2. After the boot commit and before any new device: the host is listed;
     record entries whose device is gone, or whose number now carries
     another stamp, are forgotten; a bound device of a volume that is not
     online becomes stale.
  3. Each online volume with a device (its binding, else the one device
     that carries its stamp; two or more unrecorded claimants are all left
     stale, E60) gets a `Recovery`: `GET_DEV_INFO` and its checks (the
     flag, the stamp, one queue, the size equal to the volume's, a request
     bound within the arena's, not `DEAD`, no live server; a device naming
     this process's pid is an earlier process's, E60), then for a
     `QUIESCED` device `START_USER_RECOVERY` (`EBUSY` retried at least
     10 ms apart, the attempt at the budget's end never held back), for a
     `LIVE` one with a dead server a wait; then the re-attach (open, map,
     fetch every tag), the front door into the engine, and `END` through
     `start_while_serving` (E44). The recoveries are stepped in turn with
     engine ticks between rounds, so the devices already re-attached are
     served meanwhile.
  4. A device gone means a new device; any other failure, `TimedOut` after
     `--recovery-timeout` included, records the old device stale and gives
     the volume a new one. Nothing is deleted at boot. A success logs the
     time to serving since the arena re-attach began, flagged beyond the
     2 s target.
- **The timeout.** 35 s by default (1 to 300). A dead server's device with a
  fetch pending is quiesced at once; one whose server held every tag is
  quiesced only by `ublk_timeout` at the block layer's request timeout,
  30 s by default since the driver sets none. 6B-1 chose 5 s, which made
  every such volume lose its device number (every mount and open file of
  the old one broken); the review raised it so that only that case waits,
  and long enough for it (E60). Unmeasured until a Linux run; writing
  `queue/io_timeout` after `START_DEV` would shorten the wait and is left
  for the WSL run.
- **Rejected.** Deleting and recreating every device at boot (the device
  number changes, which is what the recovery exists to avoid) and a
  recovery that waits without bound (a device that never quiesces would
  hold the boot).
- **Enforced by** `step6b_recovery_ublk_tests` (15 tests over `MockKernel`),
  `step6b_recovery_daemon_tests` (15, among them a restart that keeps every
  device number within the 2 s target on the simulated clock) and
  `review_step6_recovery_daemon_tests::the_recovery_timeout_outlasts_the_block_layers_request_timeout`.

## E49. Device standings and the reaper: listed, never silently deleted

2026-10-16. Accepted, amended by E60 (the review's W2: a device naming this
process's pid that it does not serve has a dead server) (Step 6B-1).

- **Decision.** `otap_ublk::classify` gives every ublk device on the host
  one standing. *Serving*: this daemon's. *Foreign*: a live server that
  is not this process (checked before any record), another pool's stamp
  while a pool is booted, or an unstamped device with pid 0. *Recoverable*:
  stamped, its server dead, and its pool not booted here (or its online
  volume has no device yet). *Stale*: recorded stale (this pool's stamp
  only); this pool's stamp on a volume that is gone, offline or served
  elsewhere; `DEAD`; unstamped with no live server. `otapctl device list`
  shows them all, with or without a booted pool. `device reap <id>`
  deletes a stale or recoverable device; `device reap --stale` every
  stale one; neither ever touches a serving, foreign or still-quiescing
  device (`STOP_DEV` on a `LIVE` device waits for its started requests,
  E40, and would block the engine thread). A reap is `STOP_DEV` then
  `DEL_DEV_ASYNC`. Nothing is deleted at boot.
- **Rejected.** Deleting stale devices automatically at boot: a device
  another pool, another daemon or other software owns is indistinguishable
  from an orphan by anything but its stamp and server, and deleting it
  destroys someone's block device under them; an orphan costs one of
  `ublks_max` (64) until the operator reaps it.
- **Enforced by** `reap_check` and `classify` in `step6b_recovery_ublk_tests`,
  and the daemon's and `otapctl`'s listing and refusal tests
  (`step6b_recovery_daemon_tests`, `step6b_recovery_ctl_tests`).

## E50. Format 2.4: the snapshot table and the catalogue become a base and a chain of deltas behind a head page

2026-10-16. Accepted (Step 6B-2; F19 to F21 in the format crate's root;
vectors `v2.4/`).

- **Context.** The snapshot table was rewritten in every CP that had a
  snapshot, changed or not, and the catalogue in every CP that named
  anything: over 270,000 records per naming CP at 1,024 volumes with 271
  snapshots each (`06`, section 8, after Step 5). A CP log page cannot
  carry those records, so the change needs new page types.
- **Decision.** F19, the table log page (`OTTL`, 4,096 bytes): a 64-byte
  header naming the aggregate, the CP and its nonce, the page's position
  and the table, then 63 snapshot records or 7 catalogue records, each with
  its own CRC; a delta region is whole pages of one CP with keys strictly
  ascending; upserts are whole records, deletions tombstones
  (`SNAP_TOMBSTONE`, `CATALOG_TOMBSTONE`); a base keeps the 2.3 image and
  refuses tombstones. F20, the table head page (`OTTH`): the base entry and
  up to 124 delta entries (region, CP, records), with the rules of order
  between them. The superblock (minor 4) records each head's CP
  (`snap_head_cp`, `catalog_head_cp`); `snap_table` and `catalog` name the
  heads. An unchanged table carries its head and writes nothing; a changed
  one writes a delta of the changed keys and a new head, or folds (a new
  base, an empty chain, a head). `TableFoldPolicy` (`EngineConfig::table_fold`):
  a fold is due when the chain would pass `max_deltas` (64, clamped 1 to
  124) or the deltas' records plus this CP's would pass the larger of the
  base's records and `min_fold_records` (256), so a boot replays at most 64
  regions per table and at most twice the table plus 256 records; a
  missing head, a full chain and a repair force one. The decision is pure
  (`plan_table`: carry, drop, delta, fold). Freed regions (the old head; on
  a fold the old base and deltas) follow the deferred-free rule like every
  metadata region (E24).
- **The 2.3-reader guard.** Byte 56 of a head page is always zero, and a
  2.3 reader takes the snapshot head for its table, reads record kind 0,
  refuses it and the whole ring before writing anything. So the snapshot
  head stays, empty or not, while any name or snapshot exists; an empty
  table otherwise has no head. A 2.4 pool with neither names nor
  snapshots has no head to trip on (E60 records what the review found
  about that case).
- **Rejected.** Logging the records in `CpLogPage` (126 records of 32 bytes
  cannot hold a 64-byte snapshot record or a 512-byte name) and
  copy-on-write catalogue pages (a B-tree for tables that fit a few pages
  at typical sizes, with a fold in every CP that touches it).
- **Consequences.** The engine boots 2.4 pools only (minor below 4 is
  `OldLayout`): nothing was released at 2.3. In memory the snapshot set and
  the catalogue are still cloned and compared, O(n), in each CP that
  changes them; only the disk cost is incremental.
- **Enforced by** `step6b_catalogue_format_tests` (with the properties
  `every_head_bit_is_covered` and `every_delta_bit_is_covered`), the v2.4
  lane of `format_tests`, and `step6b_catalogue_sim_tests` (the property
  `random_table_changes_with_folds_and_crashes_boot_to_the_engines_tables`).

## E51. Damage in a table chain: torn when the booted commit wrote it, otherwise uncertain keys, quarantine and a refold

2026-10-16. Accepted, partly reversed by E60 (the review's D9: a snapshot table that lost records refuses the boot) (Step 6B-2).

- **Decision.** A region the booted commit wrote (the head's CP or an
  entry's CP equal to the candidate's) that does not verify tears that
  commit (`TornReason::Table`) and the boot falls back. An older region
  that does not verify is shared damage: tolerant scans keep every record
  that verifies alone (a delta page needs a verifying header naming the
  aggregate, the table, the CP and its position), and a key is uncertain
  when its last readable record precedes the last damaged region.
  Uncertain or clashing names are dropped; uncertain or duplicate-uuid
  snapshots get `SNAP_UNCERTAIN` (bit 6 of `SnapRecord.flags`, persisted;
  `SNAP_FLAGS_MASK` admits it with the pins), which quarantines them like
  E43; a pin keeps the mark. An unreadable older head boots without the
  table. Each case warns (`SnapshotTableDamaged`, `CatalogDamaged`) and sets
  `repair`, so the first commit after the boot folds the table into a
  clean chain. Deleting an uncertain snapshot, or one whose root did not
  load at boot, frees nothing (the second was a latent defect, fixed with a
  test).
- **Also.** The Step 4 review's hazard 12 is fixed: the boot dedupes
  `meta_live` by address, so a region the live tree and a snapshot share
  is counted once. And a rewritten snapshot root page is listed in the CP
  log directory (the Step 5 review's hazard 9), so a lost one tears its CP
  rather than reading as a damaged snapshot.
- **Rejected.** Failing the boot on any damaged table region: one bad
  sector in a region every candidate shares would stop the pool, as W4 of
  the Step 5 review showed for the catalogue.
- **Enforced by** `step6b_catalogue_sim_tests`.

## E52. A policy fold of a table that does not fit is put off; a forced fold is not

2026-10-16. Accepted (Step 6B-2).

- **Decision.** The reserve numbers are unchanged (boot 0, shutdown 1,
  others 2 segments); table regions are placed by the CP's metadata stream
  under that reserve. A policy fold is planned only if
  `MetaStream::can_place(base pages + 1)` (`SegmentTable::has_run`); one
  that still meets `NoSpace` sets the chain's `fold_blocked_at` (the free
  segment count then) and the CP is retried at once as a delta CP without
  marking the pool full; the policy stays quiet until free segments rise
  above the mark. A forced fold (no head, a full chain of 124, a repair)
  never waits.
- **Rejected.** Letting a policy fold fail the CP like any `NoSpace`: the
  fold is an optimisation of the next boot, and a full pool that could
  still commit a delta would have been marked full for it.
- **Enforced by** `step6b_catalogue_sim_tests`.

## E53. The pool attestation is an engine job that holds one committed CP readable

2026-10-16. Accepted, amended by E60 (the review's R3) (Step 6B-2).

- **Context.** The brief's "pool attestation" is not the design's
  attestation (the operator's statement about a hardware RAID cache, `05`
  section 7.3); the nearest design item is the engine scrub, deferred past
  v0.1. "Bounds" and "LBA continuity" are defined nowhere. Every public map
  walk stopped at the first damaged page, and a whole-pool pass stalls the
  data path unless it is sliced.
- **Decision.** `otap_cp::verify`: `start_verify(VerifyOptions)`; each tick
  advances at most `units_per_tick` (256: a block is one unit, a map page
  four, a summary its pages) and `max_ns_per_tick` (2 ms). The job captures
  CP N when no superblock write is in flight, then holds the deferred frees
  of every later CP so N's view stays on disk while writes, CPs, folds,
  deletions and the cleaner go on. It checks the ring (four slots, CRCs,
  the aggregate, locations; N's pair against the engine's slot; the other
  pair one CP older; monotone `owner_epoch`, `intent_seq_seen`,
  `opened_stamp`), the map roots, the segment table, the CP log
  directory, deltas and pages, the stripe list and its summaries, both
  table chains replayed against the engine's tables, every tree (index
  against the engine, page CRC, `max_birth`, entries, births, bounds, the
  overlay, each snapshot root's identity, the quarantine), every distinct
  live block (address, a data segment in use, payload CRC, summary CRC and
  identity, the descriptor against the entry), and the accounting (data
  `live_bytes` against the blocks reached: more found is an error, fewer
  a warning; metadata against the referenced regions plus pending frees).
  Seventeen kinds of finding; 64 kept, the counts complete. It aborts,
  releasing its holds, at shutdown, or when the pool is full or the free
  segments reach the reserve while it holds anything (a full pool that it
  holds nothing of is verified, E60); it repairs only table chains (a
  finding sets `repair`, the next CP refolds).
  `dbladed`'s `PoolVerify` starts it or returns the running job's report,
  `PoolVerifyStatus` and `PoolVerifyCancel` follow it, and `otapctl pool
  verify` waits (100 ms polls) and exits 4 on a verdict that is not clean.
- **Rejected.** A verification under a quiet engine (the data path would
  stop for the length of a whole-pool read) and a lock-free read of the
  live state (a CP in flight would move what the job reads).
- **Consequences** (limits, `06` section 8): about 1 TiB of reads per TiB
  of live data, plus map pages and summaries (about 4 GiB each per TiB);
  about 1 MiB per tick at the defaults; held frees grow with the run, so a
  long verify of a nearly full pool aborts. It cannot see blocks under a
  tree that fails to load (the accounting then drifts and is reported as a
  warning), stripes and regions nothing references (only through
  `live_bytes`), free space, slots older than N-1, or the arena and ublk
  side. A table chain is checked in one slice (up to 126 region reads).
- **Enforced by** `step6b_verify_sim_tests` (the property
  `random_workloads_with_crashes_always_attest_clean`; every kind of
  injected damage found with its kind and address; slicing under writes;
  the holds; giving way to space; cancel), `step6b_verify_daemon_tests`,
  `step6b_verify_ctl_tests` and `step6b_verify_control_tests`.

## E54. Replication: the engines speak a `Link`; TCP lives in the daemon, the simulated network in the simulator

2026-10-16. Accepted (Step 6C).

- **Context.** Sockets stay out of `otap-stream` (`09`, section 4), and the
  Step 3 `Net` and `LoopbackNet` cannot lose a connection, which every
  resume test needs. The daemon hosts one pool where a loopback harness
  needs two.
- **Decision.** `Link`, `Dialer`, `LinkListener` and `SinkStore` are traits
  in `otap_cp::traits` beside the other four (E13). The sessions
  (`otap_cp::repl`: `Replicator`, `SourceJob`, `SinkJob`, the link framing
  and the sink's record) are engine-side code stepped on the engine thread
  after every tick, with no I/O of their own. `otap_sim::SimNet` carries
  the same frames over byte pipes with delay in ticks and seeded faults
  (drop, cut, corrupt, refused dials, a queue limit, a pulled cable), and
  `MemStore` is a state directory that survives a simulated crash.
  `otap_daemon::TcpReplicationTransport` is `std::net` with a reader and a
  writer thread per connection and bounded channels (64) to the engine
  thread. No new crate: `otap-cp` uses the workspace's `otap-stream` and
  `bytes`, and `Cargo.lock` gained no package.
- **Rejected.** An `otap-repl` crate (the sessions need the engine's
  private snapshot and apply calls, and a crate boundary would have
  widened the engine's public surface for one caller) and an async runtime
  (one engine thread, `09`).
- **Enforced by** `step6c_transport_wire_tests` (otap-cp),
  `step6c_transport_simnet_tests`, `step6c_transport_tcp_tests`.

## E55. The source: a stable view without held frees, a delta identity every process agrees on, and an anchor chunk

2026-10-16. Accepted (Step 6C).

- **Decision.** Every engine call a source makes (`snapshot_delta_scan`,
  `snapshot_read_blocks`) takes the snapshot's current tree, walks or reads,
  and returns within one call on the engine thread: a snapshot is
  immutable and relocation keeps births and CRCs, so nothing is held
  between calls and no deferred free is pinned (unlike E53). A transfer
  plans first (pass 1 counts data blocks in page-sized slices, records
  each chunk's start LBA and the hole runs), then streams chunk *i* rebuilt
  on demand from its start as a stripe image (`StripeHdr` with the
  aggregate, the target's birth CP, the nonce, `lseg = i + 1`,
  `stripe_idx = i`; descriptors with LBA, birth and CRC). The delta's
  identity: `cp_seq` the target's birth CP, `base` the base's, and the nonce
  BLAKE3 over the two snapshots' identities, so the same delta has the same
  identity in every connection and process, and a resumed sink matches it.
  An increment is `Increment {from, to}`; a first transfer is `Steady` with
  base 0 and the target in the `SnapTag`. A delta with no data block sends
  one anchor chunk (block 0 as the target reads it), because the stream
  refuses a delta of no chunks.
- **Consequences.** The plan costs 8 bytes per chunk and 12 per hole run,
  and the whole delta is walked before `DeltaBegin`. Quarantined snapshots
  (E43, E51) are refused as source and as base at both ends.
- **Enforced by** `step6c_replication_engine_tests` and
  `step6c_replication_sim_tests`.

## E56. The sink: a chunk lands when a committed CP carries it; the cursor lives in `--state-dir`; the destination snapshot is the commit record

2026-10-16. Accepted, amended by E60 (the review's D2 to D7 and W1) (Step 6C).

- **Decision.** The sink verifies each chunk (credits, the image's
  identity, exact length, per-block CRC, volume, LBA in range), turns it
  into runs and places them through `apply_write` and `apply_punch` (the
  front door's path for a volume marked as a replica). A chunk is landed
  only when `durable_seq()` covers its highest sequence, which is when a
  committed CP carries it: then the cursor records it, BLAKE3 over the
  images in index order advances, the record is saved, and only then does
  `Landed` return the credits. `HelloAck` waits while chunks of the delta
  are placed and not yet durable, so the cursor it sends holds every chunk
  received and a resume never re-sends a landed chunk. After `DeltaEnd`,
  with every chunk landed and the digest matching,
  `create_snapshot_as(target uuid, Transfer, name)`; once it is listed,
  the cursor commits, the record is rewritten (base = target, clean), the
  mirror pins its own base, the previous transfer base is deleted, and
  `Committed` goes out. A digest mismatch aborts the delta and starts over.
  The record (`SinkRecord`, `OTRS` v1: a header, the stream's 4 KiB
  `CursorPage`, a CRC) is saved atomically in `--state-dir` as
  `repl-<pool>-sink-<vol>.rec` after the CP that made its chunks durable;
  its `pool_cp` catches a pool rewound below it. The destination
  snapshot's existence, under the source's uuid, is the commit record: a
  sink that finds the landing's target among its snapshots finishes the
  commit. A delta of another identity on a dirty mirror rolls the mirror
  back to its base first (or deletes a base-less partial first transfer).
  A lost record: the base is the mirror's newest `Transfer` snapshot and
  the live volume is treated as dirty.
- **The replica mark.** A mirror is offline and marked in memory
  (`set_replica`); `set_volume_online(true)` answers `ReplicaVolume`, so no
  client reads a partial delta or writes into a mirror. `replicate release`
  syncs, rolls a dirty mirror back to its last complete snapshot, clears
  the mark and the pin, removes the record and leaves the volume offline.
  The replica stamps its own births (a block's birth is the CP that placed
  it on the mirror).
- **Rejected.** A format 2.5 record in the pool for the cursor (the
  superblock's `InboundState` is reserved and never written; a separate
  write has the same crash window, and the committed state is rebuildable
  from the pool through the same-uuid snapshot); keeping the sender's
  births (they mean nothing in another pool's CP sequence).
- **Enforced by** `step6c_replication_sim_tests` (the property
  `random_transfers_with_faults_and_crashes_converge`;
  `every_frame_kind_and_position_tolerates_a_fault`, 57 runs; sink crashes;
  abandoned deltas; pulls) and `step6c_replication_daemon_tests`.

## E57. The source's pin moves on `Committed` only; one destination per source volume

2026-10-16. Accepted, amended by E60 (the review's D2: the sink only reports a base it holds) (Step 6C).

- **Decision.** The source calls `set_replication_base(T)` only on
  `Committed`, or when a later handshake reports T as the sink's base (the
  `Committed` was lost with a connection), and then waits for the pin to
  be durable; anything short leaves the old pin. The mirror pins its own
  base the same way. Because the base is the `PIN_REPLICATION` bit, not a
  per-destination field, a source volume has one replication base and so
  one destination.
- **Consequences.** A second destination would move the first one's base
  (limit, `06`). Source jobs live in memory: a daemon restart forgets them,
  and a new push resumes from the sink's durable cursor.
- **Enforced by** the pin checker of `step6c_replication_sim_tests` (every
  move of the pin is checked against what the destination holds durably).

## E58. The replication link: a framed, CRC-sealed byte stream, a fixed handshake, loopback by default, no TLS yet

2026-10-16. Accepted (Step 6C).

- **Decision.** A link frame is `OTRL`, a length (at most 8 MiB + 64 B +
  1 KiB, refused at the header), a CRC-32C and the payload. `FrameReader`
  poisons itself on a bad magic, an oversize length or a CRC failure, and
  `finish` reports a truncated frame. The payload's first byte: 1, the
  handshake (96 bytes, `OTRH`, the version first, role, accept, aggregate,
  volume index, target and base uuid and CP, three names of at most 255
  bytes); 2, a refusal (a code and at most 1 KiB of text); 3, one stream
  frame. `REPL_PROTOCOL_VERSION` is 1; another version is answered
  `Refuse(Version)` before anything else is parsed. Within a connection
  nothing is dropped, duplicated or reordered; resent chunks after a
  reconnect are the only duplicates. The same pool at both ends, and a
  destination that exists and is not a replica, are refused.
  `dbladed --replication-listen <host:port>` binds in `Daemon::spawn` and
  lends the listener to the booted pool; listening and dialling are
  loopback-only unless `--allow-remote-replication`, worded like the
  control plane's `--allow-remote-control`. TCP timeouts: read poll
  200 ms, write and mid-frame stall 30 s, connect 5 s.
- **Rejected for now.** TLS and authentication: whoever reaches the port can
  push into a mirror or pull a snapshot, which is why the default is
  loopback and the flag's help says so (limit, `06`).
- **Enforced by** `step6c_transport_wire_tests` (three properties),
  `step6c_transport_tcp_tests`, and the replication control and `otapctl`
  suites.

## E59. The control protocol stays at version 1 for new variants; requests without fields are empty struct variants

2026-10-16. Accepted (Steps 6B-1, 6B-2 and 6C; the second half after the
Step 6 review's R1).

- **Decision.** A new request or reply variant does not bump
  `PROTOCOL_VERSION`: a daemon that does not know a request answers it
  `BadRequest` (it cannot decode it), and a reply variant reaches only a
  client that sent the request it answers. A field added to an existing
  shape is still a version change, since both peers refuse unknown fields
  (R3 of the Step 5 review). Step 6 added `DeviceList`, `DeviceReap`,
  `PoolVerify`, `PoolVerifyStatus`, `PoolVerifyCancel`, `ReplicatePush`,
  `ReplicatePull`, `ReplicateStatus`, `ReplicateCursor`, `ReplicateCancel`
  and `ReplicateRelease`, and the replies `Devices`, `Reaped`, `Verify`,
  `Replication`, `Replications` and `ReplicationCursor`, under version 1.
  Every request and reply without fields is an empty struct variant
  (`Ping {}`, `PoolStatus {}`, `Ok {}` and the rest): serde lets an
  internally tagged *unit* variant ignore any field it does not know,
  whatever `deny_unknown_fields` says, so seven requests and the `Ok` reply
  accepted `{"op":"ping","force":true}`. The JSON on the wire is the same
  (`{"op":"ping"}`), so the change is invisible to a peer of either age.
- **Rejected.** Bumping the version for each step's additive verbs: every
  `otapctl` of an older step would then refuse a newer daemon for verbs it
  never sends.
- **Enforced by** `review_step6_control_tests`
  (`r1_a_request_without_fields_refuses_an_unknown_field`,
  `r1_the_ok_reply_refuses_an_unknown_field`), the round-trip test of every
  variant in `protocol/tests.rs`, and the per-step control suites.

## E60. After the Step 6 review: what the sink, the snapshot table, the arena and the devices decide

2026-10-16. Accepted (the Step 6 adversarial review,
`reviews/2026-10-16-step6-qa-adversarial.md`; the findings and their fixes
are in the Step 6 entry of `08-engineering-log.md`). Amends E42, E48, E51,
E56 and E57; nothing here changes the format, the arena layout or the
protocol version.

- **A relocation verifies before it moves** (D1, the Step 5 review's hazard
  8). A block that fails its CRC is not copied under its recorded CRC: its
  segment is skipped like one with an unreadable block, and the damage
  stays where it is. Rejected: re-stamping the block with a fresh CRC (it
  would turn known damage into silent wrong data), and letting the boot's
  data check skip relocation stripes (it would also skip the lost writes
  that check exists to catch).
- **The sink's commit is the delta's own** (D2). The destination snapshot
  takes the identity recorded when the delta began, never the latest
  handshake's; a newer delta waits for the older one's commit.
- **A sink record knows which CP it was written after** (D3). The record
  keeps the CP's nonce beside its number (in reserved bytes: layout 1
  stays), and `Replicator::attach` forgets a landing the pool no longer
  holds right after the boot commit, the one moment a rewind of a single
  CP is visible. The sink still clears the mirror's nvfail, now only after
  that check.
- **The landing goes before the rollback** (D4): the abandoned delta's
  cursor is aborted and saved before the rollback or the deletion is asked
  for; and the daemon refuses `snapshot rollback` and `volume resize` of a
  mirror.
- **What makes a volume a mirror** (D5, D6, D7). A sink record that names
  it, or the pool itself: the replication pin on its newest transfer
  snapshot (set by the sink on the base it committed, cleared by `replicate
  release`). Anything else is refused as a destination; a mirror whose
  record is lost stays marked; the daemon forgets a record when it deletes
  its volume; and the record is saved as soon as the mirror is asked for,
  before its creation can commit. A partial first transfer whose record is
  lost is still an ordinary offline volume (limit): a sink's
  `--state-dir` belongs on persistent storage. Rejected: a format 2.5
  replica flag in the volume table (a layout change late in the step, and
  the pin already says it for every mirror that has a base).
- **One copy of a source snapshot per pool** (W1): a target whose identity
  another volume of the sink pool holds is refused at the handshake.
- **A cleaning CP keeps the quarantine** (D8).
- **A snapshot table that lost records refuses the boot** (D9; reverses
  part of E51). An older head that does not verify, or an older region
  that yields fewer records than its head counts, tears the candidate, as
  format 2.3 tore it; with every candidate naming the region the boot is
  refused. E51's tolerant path stays for damage that loses no record and
  for the catalogue. The reason: a record the boot cannot read may have
  created a snapshot, and booting without it counts that snapshot's blocks
  dead and lets the cleaner free them, an irreversible loss behind one
  warning; a refused boot loses nothing and leaves the decision to the
  operator. Rejected: booting with every data free frozen until an
  acknowledgement (the repair fold would make the loss permanent and
  invisible to the next boot, and the pool would fill).
- **The arena's re-attach takes the writer's fill discipline** (D10,
  amends E42). A stale slot is judged at the position its index takes in
  the window, never under its own unverified lap; `Fills::Serial` (the
  engine) adds that a stale slot with anything written after it is damage,
  since a serial writer's interrupted fill is its newest reservation's;
  `Fills::Interleaved` (the arena's general contract, which E16's future
  cells will need) keeps the neighbour rule alone. A damaged slot is
  rewritten under its implied lap (R5).
- **A device's binding outlives a `--no-ublk` boot** (D11, amends E47 and
  E48): the record is loaded and saved on every boot, a deletion marks the
  deleted volume's device stale, and two unrecorded devices claiming one
  volume are both left stale.
- **A pid is not an identity across a restart** (W2): a device naming this
  process's pid that this process does not serve is an earlier process's,
  its server dead.
- **`--recovery-timeout` is 35 s by default** (amends E48), past the block
  layer's 30 s request timeout that alone quiesces a device whose server
  died holding every tag; only that case waits.
- **A resize with nothing to hold is written at once** (W3, amends E45).
- **A full pool can be verified** (R3): only holding something aborts a
  verification on a full pool.
- **One listing policy for both ublk hosts** (R4): a device that answers
  `EINVAL` to `GET_DEV_INFO` (made with `UBLK_F_UNPRIVILEGED_DEV`) is left
  out of the listing; any other refusal still fails it.
- **Switches take no value** (R2) and **peer text is escaped** (R6).
- **Lesson** (also in the log): every replication defect was a decision
  taken against a different view of the same state than the one it acted
  on: a target from the newest handshake over the old delta's content, a
  record trusted against a pool that rewound under it, an identity read
  from an index that a deletion recycles. The CP lesson of E39 holds for
  the sessions too: what a session commits must be named by the state it
  committed, never by what arrived since.
