# OpenTAP engineering log

Dated, append-only record of what happened: steps, reviews, defects found and
fixed, gate results, lessons. Newest entries at the bottom. Every entry says
who did what and where the result lives. Decisions go to
`07-engineering-decisions.md`; the current state goes to
`06-implementation-status.md`; this file is the history.

The entries for 2026-10-12 and 2026-10-13 were written on 2026-10-13 from the
session transcripts and the workers' hand-back reports, after the owner asked
for documentation "so that things aren't being forgotten or lost". The tree
had no version control, so this log and the test files are the only record of
that work.

---

## 2026-10-12: design rounds 1 to 3

- **Charter 1** (`00-project-brief.md`): a SAN OS that makes spinning disks
  perform at network speed with Rust daemons, a RAM write cache, HA memory
  mirroring and log-structured flushes; Fable 5.1 as lead architect directing
  "Opus 5.5" workers.
- **Round 1** (`01-architecture-brainstorm.md`, 46-agent workflow): four
  personas, cross-examination, 32 fact checks, two critics. Three premises of
  the charter did not survive: a RAM mirror is not RPO=0, node-local disks
  cannot do ONTAP-style takeover, Fibre Channel cannot be the primary
  interconnect on a stock kernel and buys no latency.
- **Charter 2** (`02-project-brief-unified.md`): unified multi-fabric,
  multi-protocol scope. **Round 2** (`03-architecture-brainstorm-02.md`,
  47 agents): Debian 13 with the 6.12 kernel as the baseline (in-box ublk,
  tcm_qla2xxx, LIO), trait contracts, the Phase 1 roadmap.
- **Charter 3** (`04-project-brief-async-svm.md`): asynchronous replication
  (the SnapMirror model), SVM-first, single-node-first. **Round 3**
  (`05-architecture-brainstorm-03.md`, 57 agents, 19:22 to 22:36): rulings
  R25 to R64, the trait contracts of Part 7, the twelve-week Phase C plan of
  Part 8, owner decisions D1 to D15. The homelab-operator critic's return
  failed on the account's usage limit; its finished document was included
  from its working file.

## 2026-10-13: Phase C Step 1, the workspace and `otap-format` v2

**Brief.** The owner's Step 1 brief (saved verbatim in
`10-phase-c-step-briefs.md`): the root workspace and `otap-format` with the
superblock ring, stripe summaries, 16 KiB map pages, the 64-byte frame header
and CRC-32C, all `forbid(unsafe_code)` on zerocopy views, proptest suites,
pinned dependencies, "compiles cleanly under cargo check and passes all cargo
test invariant suites without warnings", no placeholders.

**Toolchain.** The PC had no Rust. rustup installed stable 1.99
(`x86_64-pc-windows-gnu`) with `--no-modify-path`; the first `cargo test`
failed with `error calling dlltool 'dlltool.exe': program not found` because
proptest's `windows-sys` links with `raw-dylib`, so GNU `dlltool.exe` and
`as.exe` from a winlibs release were placed in `~/.cargo/mingw-tools/bin`.
The `x86_64-unknown-linux-gnu` standard library was added for cross
type-checking (no linker). The brief's "Rust 1.82+" could not hold with the
pinned `crc-fast` (E01).

**Code.** The lead wrote the workspace, `otap-format` (`constants`, `types`,
`checksum`, `superblock`, `stripe`, `map`, `stream`) and `format_tests.rs`,
with the format rulings F1 to F10 settling where the brief, the §7.1 sketch
and the 4 KiB page bound disagreed.

**Independent kernel-safety audit** (Opus worker, read-only). Verdict: no
layout defect, "the logic around them is not" clean. Fourteen findings, all
fixed by the lead:

1. Blocker: `Superblock::select` skipped a slot from a newer major as if
   corrupt and mounted an older commit, which a later commit would then
   overwrite. Fix: a newer major is a hard stop (`NewerFormat`) checked
   before anything else; selection returns a `Result` with the reason (F7).
2. Selection ranked 4 KiB pages, not whole slots, ignored the aggregate id
   and the read location, and broke ties toward the stale B copy. Fix:
   `SuperblockSlot::rank`/`select` over whole verified slots of this
   aggregate at their location, ordered by `(owner_epoch, cp_seq, cut_seq)`,
   A wins ties (F7).
3. `StripeSummary::parse` proved integrity, not identity: a stale stripe at
   a reused location passed. Fix: `parse_expected` with a `StripeId`
   (aggregate, lseg, stripe index, CP, nonce) and a distinct error.
4. No N-1 decode lane: golden vectors were rebuilt from code, not stored.
   Fix: `tests/vectors/v2.0/*.bin` with a BLAKE3 manifest and a decode lane;
   fuzz targets added in Step 2 (E05).
5. The seal paths accepted what `parse` rejects and sealed dirty padding.
   Fix: one validation shared by both builders and the parser; dirty padding
   refused; `set_payload` derives `raw_len` for raw frames.
6. The summary bitmap was caller-supplied and unchecked; descriptor volumes
   were not bounded by `MAX_VOLS`; F8's text was wrong. Fix: the bitmap is
   derived by the writer and checked by the reader; vol < 1024 everywhere;
   F8 rewritten.
7. Birth 0 and the hole descriptor's CRC were undefined. Fix: CP 0 is mkfs,
   a zero birth means never written, hole CRC is `ZERO_BLOCK_CRC32C` (F5,
   F9); the golden stripe regenerated.
8. Two gaps in "a layout change fails to compile": `RootRef` offsets and
   enum discriminants. Fix: `offset_of!` asserts and `from_u8(as_u8())`
   const asserts.
9. `verify` accepted undefined enumerations and unbounded lengths
   (`nvfail_cause`, `durable_log.kind`, bits at or above `vol_count`,
   `raw_len`, `HoleRecord` overflow, CP above `u32::MAX`). Fix: all checked.
10. Records whose CRC lives in the parent had no verify path. Fix:
    `RootRef::verify`, `MapPage::parse`, `MapRoot::verify_index`, with
    bit-flip tests.
11. The frame header contradicted the minor-version rule. Fix: the wire
    policy is documented (features negotiated in `Hello`; reserved bits
    refused), convention 4 amended.
12. `SummaryLayout::for_stripe` had no upper bound. Fix: stripes bounded to
    8 KiB to 8 MiB, `MAX_WIRE_PAYLOAD` tied to `MAX_STRIPE_BYTES`.
13. Alignment behaviour documented (views need `Block4K`/`Page16K`/arena
    memory; copies work anywhere).
14. Smaller items: `#[must_use]`, `const fn`, error detail (`BadCrc` names
    its region), `FormatError` non-exhaustive, `IntentSlot` slot parity,
    the crate example sealing a bare page, deviations from §7.1 recorded as
    F10, writers rebuild from `new()`.

**Adversarial QA** (Opus worker, own test file). 43 tests; five defects, all
fixed by the lead and then re-pinned by the worker in its port (49 tests):

1. `HoleRecord::end_lba` overflowed on peer-controlled input and panicked
   (`overflow-checks` are on in release). Fix: checked `end_lba`, a
   `HoleRecord::validate` (`len >= 1`, vol in range, `lba + len <= MAX_LBA
   + 1`).
2. The superblock accepted an undefined `durable_log.kind`. Fix: kinds
   checked; kind 0 must be all zero.
3. The slot accepted an undefined `nvfail_cause`. Fix: the six volume-table
   rules (defined cause, rows at or above `vol_count` absent, absent rows
   empty, nvfail bit and cause agree, write-active only on present rows,
   `acked_bytes` sum).
4. `write_summary` accepted headers that in-place sealing refuses. Fix:
   both builders refuse what `parse` refuses, with the same error.
5. `for_stripe` took linear time on huge stripes. Fix: the 8 MiB bound.

Hazards the QA listed without a failing test (page-only selection, no
aggregate check, unbounded `raw_len`, wire strictness, loose `MapEntry`
`plen`, unbounded `MapRoot::lba_count`, unchecked descriptor volumes, size
helpers panicking on absurd input, no cross-checks inside the slot) were all
closed by the audit fixes above. One pinned behaviour was flagged for
confirmation and accepted: a newer-major slot stops the boot even when it
carries another aggregate's id, so mkfs must wipe all four slots.

**Result.** 103 tests plus the doctest green, clippy clean, the 10,000-case
gate in 28 s. Reported to the owner.

## 2026-10-13: Phase C Step 2, records, `otap-stream`, `otap-sys`, fuzz

**Brief.** The owner's Step 2 brief (verbatim in
`10-phase-c-step-briefs.md`): the deferred §7.1 records; `otap-stream` with
`frame.rs`, `cursor.rs`, `codec.rs` (zero-copy `bytes::Bytes`, header CRC
validation, 8 MiB bound), `flow.rs`; `otap-sys` with `hugetlb.rs`,
`ublk.rs`, `uring.rs` and minimal `unsafe` with `// SAFETY:` proofs; `fuzz/`
with three libfuzzer targets; property tests; `cargo test --workspace`.

**Records.** The lead wrote `records.rs` (F11): `SegEntry` keyed by position
with `phys` and the geometry at birth, `SnapRecord` without a name,
`CpLogRec` with the packed map address, `CpLogPage`. The v2.0 vector set was
extended to twelve files and its manifest hash re-pinned, the one allowed
regeneration before a release (E05).

**Stream.** The lead wrote `otap-stream` (`frame`, `codec`, `cursor`,
`flow`) and `stream_tests.rs`, and the three fuzz targets.

**`otap-sys`** was written by the Opus-Sys-Kernel worker against the 6.12
sources saved in the scratchpad (`ublk_cmd.h`, `ublk_drv.c`, `io_uring.h`,
`rsrc.c`, `magic.h`). Five `unsafe` blocks, all Linux-only. Departures from
its brief, all accepted: `map_file` passes no `MAP_HUGE_*` flag (the kernel
takes the page size from the hugetlbfs mount; the code checks
`HUGETLBFS_MAGIC` and the block size instead, and refuses to shrink an
existing file); `Allowlist::probe` sends `GET_FEATURES` through a private
two-entry ring with 128-byte SQEs rather than the reactor's ring;
`RegisteredBuffers` takes the whole arena and splits it at 1 GiB per fixed
buffer; `user_copy_read_sqe` takes `&UblkChar` so `O_NONBLOCK` is
guaranteed; `UblksrvIoDesc::flags` returns the unshifted word the `UBLK_IO_F_*`
masks are defined on. Derived and asserted: `sizeof(ublk_params) == 112`,
`UBLK_U_CMD_START_DEV == 0xC020_7506`, `UBLK_U_IO_FETCH_REQ ==
0xC010_7520`. The facts the worker asked to have confirmed were checked by
the lead against the same sources: the 1 GiB limit per fixed buffer and the
16,384-entry table, the 128-byte SQE requirement for ublk control commands,
`HUGETLBFS_MAGIC`, the `EBUSY`/`ENXIO` outcomes of cloning a buffer table,
and 6.12 not granting zero copy (E08). Nothing ran against a real kernel.

**Adversarial QA pass 1** (QA-Fuzz worker, own test file plus the fuzz
targets). 51 tests; eight defects, all fixed by the lead:

1. `Decoder::drain` lost the frames it had decoded before a payload CRC
   error (a lost `Holes` or `VolEvent` would never be resent). Fix: the
   frames are returned and the error held for the next call (S5).
2. The encoder emitted headers its own decoder refuses (`cp_seq` over the
   bound, `base > cp_seq`). Fix: the sealed header is verified before it is
   emitted (S3).
3. Decoding accepted bodies the encoder never produces (reserved bytes,
   steady deltas with snapshots, adjacent sparse ranges), so eleven of
   fourteen cases re-encoded to different bytes. Fix: canonical bodies
   (S2); reserved bytes stay ignored on the on-disk cursor page.
4. A raw chunk whose `raw_len` disagreed with its image was encoded. Fix:
   `raw_len == image.len()` for raw chunks.
5. A fragmented landing made the `HelloAck` unencodable (a million ranges
   exceed 8 MiB), a liveness hole a hostile sender could reach. Fix:
   `MAX_WIRE_RANGES = 4096`; the peer learns a subset and resumes (S4).
6. A compressed chunk's `raw_len` escaped the stripe rules. Fix: it must
   decode to a stripe-sized image.
7. `Chunk::stripe_hdr` panicked on a short raw image. Fix: `BodyTooShort`.
8. Several refusals named the wrong field or contradicted themselves. Fix:
   exact fields and consistent short/long errors.

Hazards it listed were closed in the same pass: the `chunk_idx` rule is
enforced (`FrameMeta::check_chunk_idx`); `Landed.idx`, credits,
`stripe_bytes`, `size_blocks` and the delta byte counts are bounded;
`ResumeBitmap::new` returns a `Result` (a `u32::MAX` count had allocated
512 MiB); the bitmap works word-wise instead of bit by bit; the encoder
validates before it reserves the payload; the 64-bit-only `compile_error!`
answers the 32-bit overflow note; the session layer's duty to abort a delta
on a corrupt `Holes`, `VolEvent` or `DeltaBegin` is documented in the codec.
The worker also found the original fuzz targets never reached their part 2
and tried only aligned views; it rewrote them (four parts, `CANONICAL`
byte-identity, unaligned views) and smoke-ran them 2.88 million times in a
scratch harness with no panic once the defects were fixed.

**Adversarial QA pass 2** (same worker). The file ported to the fixed API:
62 tests, 58 passing at 100,000 cases in 27.8 s, four new defects, three
notes. Fixed by the lead in the afternoon session:

1. `needed()` reported 64 while `drain` held an error, so a transport that
   reads `needed()` bytes before each call would sit on the
   `Abort(Corrupt)` it owes. Fix: zero while an error waits, and for a
   poisoned decoder.
2. A `HelloAck` cursor over the credit bound was clamped by `to_wire`, so
   two cursors shared one encoding. Fix: no clamping; `Frame::validate` and
   `Cursor::to_page` (now a `Result`) refuse it (S3).
3. `VolEvent::validate` named the wrong field in six cases. Fix: exact
   fields and values.
4. `Encoder::encode` reserved the whole frame before it checked the header
   context, so refusing an 8 MiB chunk under a bad `cp_seq` allocated
   8 MiB. Fix: `FrameMeta::check` before allocation (S10).

Notes: the unused `StreamError::Poisoned` variant was removed (S9); delta
byte counts were settled as raw image bytes (S6); `Cursor::begin` now builds
its bitmap only once the outcome needs it. The lead converted the four
`bug_*` tests to pin the fixed behaviour; the worker had already fixed a
coverage regression in part 3 of the stream fuzz target caused by the
`chunk_idx` rule.

**Gates, final run.** fmt clean; clippy clean on the host, on the Linux
target and for the fuzz package; rustdoc clean; `cargo test --workspace` 221
passed, 0 failed, 1 ignored, 2 doctests; the same at 10,000 cases in 36 s.
Reported to the owner.

**Documentation.** The owner asked for documentation files "so that things
aren't being forgotten or lost". `06-implementation-status.md`,
`07-engineering-decisions.md`, this log, `09-working-agreements.md` and
`10-phase-c-step-briefs.md` were started; the worker reports above were
recovered from the session transcript for this entry; the README now points
at them.

## 2026-10-13: Phase C Step 3, the engine crates and the simulator

**Brief.** The owner's Step 3 brief (verbatim in
`10-phase-c-step-briefs.md`): `otap-arena` (the arena, the bump allocator
with `RingRef`, the slot-state tracker), `otap-index` (per-volume
`BTreeMap<u64, DirtyExtent>`, the epoch-flip cut), `otap-cp` (the
`Backstore`, `FrontDoor`, `Clock` traits, the flusher, the dirty-boot
evaluator), `otap-sim` (mock traits, a proptest state machine with crashes
mid-CP), with `crossbeam-epoch`, `parking_lot`, `madsim or proptest`, no
syscalls in the engine crates, full stripes assembled in memory, and unit
tests proving that a crash during `Backstore::commit` rolls back to the
previous CP.

**Design before code.** The lead re-read doc 05 (sections 1.3, 6.2, 7.2,
7.6, 7.7, 8.3 to 8.5) and settled six decisions first, recorded as E13 to
E18: the traits' home and shape; recovery through exact roots with a
per-CP map image instead of a written-extent field; `arc-swap` in place
of crossbeam-epoch (the `unsafe` rule); the arena layout and re-attach
rules; the CP protocol, triggers and both nvfail modes; the simulator's
device model and checker.

**Code.** The lead wrote the four crates and their suites. Three defects
were found by the lead's own suites before any review:

1. A block superseded inside the active generation kept its arena slot
   `Placed`: it was never staged, never committed, so the tail never
   passed it. Symptoms: after a clean restart the re-attached arena
   replayed the dead block over the newer data, and a workload that
   rewrites the same LBAs filled the arena for good, whereupon a trigger
   held every tick and cut an empty CP per tick until the pool ran out of
   segments. Fix: the index returns the superseded runs and the engine
   releases them at once (E16); triggers cut only when no generation is
   waiting (E17).
2. The precise-mode `write_active` bit was computed before the CP's own
   window decrement, so a volume left the set at the third CP without a
   write instead of the second (section 7.6). Fix: the slot is built with
   the windows as they stand after the commit.
3. A segment sealed on a failed allocation was sealed again on the retry,
   which the table refused. Fix: the open pointer is cleared before the
   allocation.
4. Found only at 10,000 cases of the random-crash property test (case
   4,795 of one run; the 256-case default and a first 10,000-case run had
   passed): in precise nvfail mode, while a commit that dropped a volume
   from `write_active` was in flight, the engine still believed the volume
   durably active and acknowledged a write to it without an intent slot; a
   power cut lost the write and the dirty boot did not hold the volume,
   which breaks invariant 6. Fix: the engine narrows its view of what the
   disk guarantees the moment the slot is built, widens it only by intents
   that land afterwards, and no longer recomputes it from the windows at
   commit. The same change builds the slot once and writes the identical
   content to `A` and `B`; before it, a write arriving between the two
   could make the two copies of one commit differ. A named regression test
   reproduces the race deterministically.

Two harness defects were fixed on the way: the model kept writes a power
cut had lost (they are now forgotten at verification), and the model's
precise-mode bound counted consistency points from a write's issue instead
of from the CP that carried it (it now learns each write's CP from the
commit events' highest sequence number). Two tests expected an arena to
fill under a workload that rewrites the same blocks (they now write fresh
blocks).

5. Also from the 10,000-case rounds, in the other direction: an intent slot
   listed every volume with a window left at the moment it was issued, so
   an intent issued while a commit was in flight could carry a volume that
   commit had just dropped, and the next dirty boot held a volume idle for
   two CPs (invariant 6's precise-mode clause). Fix: an intent lists the
   durable set plus the volumes waiting on it, nothing else; and a CP that
   carries a volume now resets its two windows instead of leaving a count
   an earlier CP had already decremented.

**Gates.** fmt clean; clippy clean on the host and on the Linux target
(the four new crates included); fuzz clippy and rustdoc clean;
`cargo test --workspace`: 249 tests passed, 1 ignored, 2 doctests. The
random-crash property test was run at 10,000 cases three times after the
last fix (30,000 scenarios, about 13 minutes per round); each round draws
its own seeds, which is how defects 4 and 5 surfaced after earlier rounds
had passed.

**Independent review (worker "QA-Crash", Opus, 2026-10-13).** The worker
was given the four crates and the brief to attack crash consistency, the
model's rules, the arena, the index, the engine's edges and the fakes,
writing only new test files. It added 68 tests in three files
(`adversarial_arena_tests.rs`, `adversarial_index_tests.rs`,
`adversarial_sim_tests.rs`), 29 of them deliberately failing `bug_*`
reproductions, and reported 23 defects, a hazard list and a list of checker
weaknesses. Every finding was handled; the `bug_*` tests were ported into
regression tests with the fixed behaviour as their expectation (the review
files now carry the pinned defects by number). The defects, grouped by what
the fix was:

- *Precise-mode holds (D1, D2).* A slot build set `durable_active` to the
  new slot's set, which could be wider than anything on disk (a volume in
  it only because of a write still waiting for its intent), so a second
  write to that volume was acknowledged against an unlanded superblock; and
  an intent issued before a slot was built re-widened the set when it
  landed although the slot's `intent_seq_seen` made the next boot ignore
  it. Fix (E17): a slot build intersects, a landed intent or the landed
  `A` slot unions, and a slot records the highest *landed* intent as seen.
  D1 needed two writes in flight, which the harness could not issue; it
  can now.
- *Rewinds (D3, D4).* nvfail engaged only when the arena did not
  re-attach, so a rewind with a re-attached arena lost the rewound CP's
  writes (released at its `finalize`) with no hold; and `dirty` came from
  the selected CP's run state alone, so a rewind onto a clean-shutdown CP
  booted clean. Fix (E17): a rewound boot is dirty and holds with
  `TornCp` whether or not the arena re-attached. The worker's
  reproductions used the `flush_opaque` fake; the ported tests produce the
  same rewinds with media damage under the newest CP.
- *Durable before serving (D5, D12).* A volume created at runtime served
  at once, so its acknowledged writes could outlive a volume no CP had
  recorded; a clear bumped `rollback_gen` in memory and served at once, so
  a crash before its CP re-issued a generation a client had already seen.
  Fix (E17): both wait for their CP (`VolumeCreated`, `NvfailCleared`).
- *The arena (D6, D7, D10, D13, D18, D19).* Block CRCs were recorded and
  never checked (a damaged slot was replayed, served and written to disk
  under a valid CRC); a failed `copy_in` leaked its reservation and the
  tail never passed it; `reserve` refused a run that fit an empty ring
  whenever it was longer than the lap remainder; every attach rewrote the
  whole header, so a kill inside the copy left an arena the next boot
  formatted away; descriptors had no CRC, so a flipped bit in a state
  byte, an LBA or a lap was replayed, misdirected or built an absurd
  window; a re-attach reissued acknowledged sequence numbers. Fix (E16,
  arena layout 1.1): a sealed descriptor word written by one store, an
  immutable header with the attach counter beside the seal, data CRCs
  verified at attach and on every arena read (`Corrupt`) and preserved
  through the stripe builder (`ArenaDamage`), a damage report the engine
  treats like a power loss, `discard` for reservations, the fit judged
  with the skipped slots counted as passable, and the next sequence number
  taken over every descriptor.
- *Availability (D8, D9, D11, D16, D17).* A full pool could not boot (the
  boot commit found no metadata stripe) and a failed boot dropped the
  device and the arena; metadata larger than a segment wedged the engine;
  a deferred write starved reads; request-level refusals became tick
  errors with the request never answered; `peek` overflowed near
  `u64::MAX`. Fix (E17): a metadata reserve and metadata recycling, a
  `BootFailure` that gives the parts back, a geometry bound at `format`
  and `create_volume`, reads and flushes served past deferred writes
  (writes stay first in first out), `TooLarge` and `NoSpace` statuses with
  `tick` reserved for internal failures, checked arithmetic.
- *The index (D20).* The ordering defects became an explicit contract
  (E20).
- *The checker and the fakes (D21, D22, D23).* The daemon-restart rule
  rejected the documented hold after a lost clear (moot now that a clear
  is durable before it serves); the precise-mode bound counted
  acknowledged writes only; the loopback network dropped a chunk `send`
  had accepted. Fix (E18): the model tracks issued writes and settles them
  after each recovery, and the network holds chunks across a partition.
- *Flush-opaque (D14, D15, and the setting of D3, D4).* Not implemented:
  the engine refuses the flag until the owner chooses between a three-CP
  ring and a post-commit flush discipline (E19).

From the hazard list: writes waiting for an intent are acknowledged by the
CP that carries them (so a shutdown no longer completes them `Stopped`);
the index documents that a cut waits for readers too, and an acquire fence
follows the grace wait; `fill` checks every slot before it writes any;
`finalize` never fails (an inconsistency is an `Event::Internal`);
`format` wipes every slot of the ring and both intent slots; reads are
bounded by `MAX_REQUEST_BLOCKS`; an unknown endpoint is `Unreachable`.
From the checker-weakness list: concurrent writes, unacknowledged data,
runtime volumes and a refused boot are now first-class in the harness, and
the random walk runs its bursts and in-flight crashes in both nvfail
modes. Not taken: per-block CRC verification of the whole selected CP at
boot (the review's suggestion for flush-opaque controllers; moot under
E19, and a full read of the CP at every dirty boot otherwise), and the
`BackToBack` trigger, whose 1 GiB threshold the simulator cannot reach.

Porting the review's tests found one more defect of the fix itself: the
new sequence rule refused the second piece of a write whose run a later
overwrite had split, so a re-attach dropped part of an acknowledged write;
equal sequences are the same write and are accepted (E20).

**Gates after the review.** fmt clean (workspace and fuzz); clippy clean on
the host and on the Linux target with every test target; fuzz clippy and
rustdoc clean; `cargo test --workspace`: 318 tests passed, 1 ignored, 2
doctests (arena 22, index 20, format 111, stream 75, sys 33, simulator 57).
The 10,000-case gate's result is recorded in the next entry.

## Lessons, running list

- `prop_assert!(cond)` without a message fails to compile when `cond`
  contains braces (a struct pattern): always pass a message.
- Clippy 1.99 lints met and fixed rather than allowed:
  `manual_is_multiple_of`, `chunks_exact_to_as_chunks`, `type_complexity`,
  `redundant_locals`, `unnecessary_map_or`, `field_reassign_with_default`,
  `uninlined_format_args`, `needless_lifetimes`, `double_must_use`.
- Hand-computed expectations need an oracle: a 1 MiB stripe holds 254
  blocks with an 8,192-byte summary and an 8 MiB stripe 2,039 blocks with
  36,864 bytes, not the first numbers written down; `summary_bytes(65535)`
  is 1,052,672.
- rustfmt rewriting files mid-edit invalidates an editor's view of them;
  re-read before editing.
- A worker's hazard list is worth as much as its failing tests: most of the
  Step 2 bounds came from the "seen but not tested" section.
- The kernel facts a worker cites from memory must be checked against the
  saved sources before they are asserted in tests.
- A transport-facing accessor (`needed`, `pending`) is part of the protocol:
  its documented meaning has to hold in every decoder state, including the
  error states.
- Without version control, everything a worker reported lived only in the
  session transcript. Recovering it took an afternoon; a repository would
  have made it a commit message.
- A set that is "narrowed at build time" must be narrowed by intersection:
  assigning the new set can widen it. Twice now a durability bug was a
  union or an assignment where an intersection was meant.
- "Durable before it serves" is the rule for every piece of state a
  client can observe (a volume, a rollback generation): the two defects
  that broke it both let a crash undo something a client had already seen.
- A harness that can only issue one request at a time cannot reach the
  races a real client produces; the review found the worst engine defect
  with two writes in flight.
- Every byte under a CRC includes the structures the code trusts about
  itself (descriptors, headers), not only the data they describe.
- When a review's reproductions depend on a mode the engine does not
  really support, separate the mode from the defect: two of the four
  "flush-opaque" findings were plain engine bugs.
