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
`PROPTEST_CASES=10000 cargo test --workspace`: every suite passed, 40
minutes 39 seconds in all (the review's simulator walk 16 min 16 s, the
original random-crash walk 13 min 30 s, the arena's two ring properties
5 min 27 s and 3 min 55 s, the index properties 47 s, format 21 s). The
two simulator walks draw fresh seeds every run, so this is one more
10,000-scenario sample of each on top of the three rounds before the
review.

## 2026-10-13: source control

The owner approved putting the tree under version control. The lead
installed the GitHub CLI 2.102.0 (zip release, SHA-256 verified against the
published checksums, in `%USERPROFILE%\tools\gh`), the owner logged in as
`prestonbalthaus-lgtm`, and the lead initialised the repository at
`E:\Coding\OpenTAP`, committed the tree as it stood after the Step 3 review
(`aee54fc`, 93 files, "Phase C Steps 1 to 3"), created the private
repository `github.com/prestonbalthaus-lgtm/OpenTAP` and pushed `main`.
The ignore file gained the fuzz crate's build output. The rules are in
`09-working-agreements.md`, section 8.

## 2026-10-13: Phase C Step 4, the persistent map, geometry, the Linux backstore and the ublk front door

**Brief.** The owner's Step 4 brief (verbatim in
`10-phase-c-step-briefs.md`) opened with the rulings on E19 and D1, D2,
D8, D9, D10, recorded as E21, and asked for four deliverables: `otap-map`
(the two-level radix map, `CpLogRec` logging with periodic folds,
snapshot pinning and the `birth_cp` walk) wired into `otap-cp`;
`otap-geom` (sysfs discovery with mocks, `StripeGeometry`, the E19
policy) plus the production Linux `Backstore` over io_uring with a
portable fallback; `otap-ublk` (the `FrontDoor` over `UBLK_F_USER_COPY`
with the whole state machine tested on Windows); and an independent
adversarial review with proptest suites in `otap-sim`.

**Design before code.** Recorded as E21 to E28 before or while the code
was written: the trust policy and the deferred free; the map's overlay,
delta regions, directory and folds, with format 2.1 (F12 to F14) and a
second golden-vector set; holes as arena records, index extents and map
entries; the metadata stream and liveness accounting; snapshots at folds;
the geometry planner; the backstore crate and its file fallback; the ublk
transport and tag state machine, and the end of the `libublk` pin.

**Code.** The lead wrote `otap-map`, `otap-geom`, `otap-backstore`,
`otap-ublk`, the `block` module of `otap-sys` and the ublk queue mapping,
the `meta` module and the new engine paths of `otap-cp`, the format 2.1
structures, the arena's hole records (layout 1.2), the index's hole
extents, and the simulator's punches, snapshots, discards and controller
cache. `otap-cp` lost `blockmap.rs` (the whole-map image of E14). The
lead's own suites found these defects before any review:

1. **A late discard landed on a reused segment.** The engine freed a
   metadata segment when it issued the discard, reused it for the next
   CP's pages, and a reordering device completed the discard afterwards
   over the new pages ("index: crc32c mismatch" as a torn CP in the
   random-geometry walk). A freed segment now stays allocated until its
   discard completes (E24); the regression test holds every discard back
   while the engine runs forty CPs and folds, and checks that no write
   lands inside a held segment.
2. **The boot CP was not retried** after a failure that was not a lack
   of space, so a transient refusal left the engine never serving; it is
   retried now.
3. **A volume created in a CP attempt that failed** kept its `created_in`
   mark, so the retry treated it as already carried; the mark is reset
   with the attempt.
4. **A fold CP wrote its own delta region**, listed it in the directory
   and kept it alive until the next fold although its records were
   already in the pages it rewrote and a region with the fold's own CP is
   never replayed; found by the Step 4 fold-policy test as a non-zero log
   size right after a fold. A fold writes no delta region now (F12, E22).
5. **The geometry planner's test expectations** were wrong about the
   segment shrink and the misalignment rule; the planner was right and
   the tests were fixed to it.

Two simulator defects were fixed on the way: the checker took the
smallest operation number an image agreed with, so a punch over an
acknowledged write (zeros where the write had been, as nothing at all
would leave) counted as a lost write after a daemon restart that had kept
both; it now takes the largest consistent number (E23). And its diff
check expected a hole punched over an unmapped block to be absent from
the live diff; the engine reports every entry born after the base, holes
over nothing included, and the harness now expects that (E23). A
third scenario the Step 4 tests exposed was a test expectation, not a
defect: the dirty-page and overlay bounds of the fold policy read the
committed state, so the CP after the one that crosses them folds; the
policy's documentation says so now.

The ublk mock was tightened twice by its own property test: an abort
withdraws a request the server has not pumped (the driver answers the
fetch with the abort instead), and nothing is ever delivered on an
aborted tag (no fetch is pending there); both are the driver's behaviour
and both changed what the state machine had to defend against (a copy
that fails after an abort fails the request, whose commit the queue then
refuses, and the tag is retired).

**Tests added.** `otap-map/tests/map_tests.rs` (a model across random CPs,
folds, replays and reboots; damage; misdirection; replay disorder),
`otap-geom/tests/geom_tests.rs`, `otap-backstore/tests/file_backstore_tests.rs`
(the whole engine over a file, with a restart),
`otap-ublk/tests/{frontdoor_tests,frontdoor_proptests,engine_tests}.rs`,
`otap-sim/tests/step4_sim_tests.rs` (the map across pages and segments
with crashes, CP log replay from the last fold, snapshot images and diffs
across random crashes and isolation from later writes, punches through
every crash, the hybrid read path over the active index, a frozen
generation and the map, every fold trigger, both discard policies, the
trust policy over a protected and an unprotected controller cache, the
late-discard regression, a mixed seeded walk, the checker regression, and
a property walk over volumes wider than a map page with punches,
snapshots, folds and crashes of every kind), the hole tests of the arena
and index suites, the format
2.1 structures in `records_tests.rs`, and the simulator's random walk
extended with punches, snapshots, folds and snapshot checks.
`format_tests.proptest-regressions` and the front door's
`frontdoor_proptests.proptest-regressions` are new and checked in:
proptest's records of shrunk inputs from the format and front-door work,
replayed before every run.

**Independent review (worker "Opus-QA-Adversarial", Opus, 2026-10-13 to
14).** The worker read the brief, the format rulings, the crate docs and
the suites, then wrote 60 tests in five files of its own (`qa_*.rs`, kept
as `review_*_tests.rs`), 24 of them `bug_*` reproductions that failed
against the tree, plus two defects it could only cite, seventeen hazards
it saw but did not test, and eleven documentation faults. Every
reproduction failed as claimed. The lead fixed every defect and ported
the reproductions in place as regression tests, each naming its defect
number below in its doc comment; the decisions the fixes needed are E29
to E33. The defects, numbered as in the worker's report:

1. (durability) The boot never checked the data stripes of a fold CP: the
   stripes to check came from the delta records, and a fold writes none,
   so a fold CP whose stripe never landed (every snapshot fold, every
   clean shutdown) was selected as good. Fix: the stripes of a fold CP
   come from the entries born at it in the fold's pages (E30).
2. (durability) A rewind below the CP that created a volume re-created it
   empty and served it without nvfail. Fix: created held, `TornCp` (E32).
3. (durability) A pool booted under another stripe geometry, and its boot
   commit rewrote the old layout. Fix: format 2.2 records the geometry and
   the boot refuses a difference (E29).
4. (durability, E19 path) A stripe that lost a data block behind an intact
   summary was selected as good under the trust policy. Fix: the data
   blocks are checked against their descriptors after an unclean stop
   (E30).
5. (durability) A transient read error at boot made the newest CP torn for
   good, and the boot commit overwrote its slots. Fix: a read error fails
   the boot, to be retried (E30).
6. (correctness) A snapshot requested while a fold CP was in flight was
   dropped by that CP's finalize. Fix: a CP owns only the requests it took
   (E31), and a request waits for the CP that carries the generation
   active when it was made.
7. (correctness, latent) A fold dropped the records of a volume created by
   the same CP. Fix: folded like any other volume's (E32).
8. (correctness) A volume grown in the configuration was served at the
   larger size and the writes past its root wedged every CP. Fix: the size
   is the root's; a differing configuration refuses the boot (E32).
9. (correctness) `snapshot_diff` accepted a base newer than the snapshot
   and answered "nothing changed". Fix: refused (E32).
10. (robustness) Running out of metadata space while placing the engine's
    own regions failed the tick instead of reporting a full pool. Fix:
    every out-of-space failure of a CP is a full pool (E31).
11. (robustness) A pool filled by snapshot metadata could no longer boot:
    the reserve held only against data. Fix: regular CPs leave two
    segments, a shutdown one, a boot may take the last (E31).
12. (robustness) A policy fold that did not fit wedged a nearly full pool.
    Fix: a fold that fails for space is dropped and the plain CP commits
    (E31).
13. (robustness) A damaged map page failed the tick on a read. Fix: the
    read answers `Corrupt` (E32).
14. (robustness) A damaged map page wedged every CP, each retry restaging
    its stripes. Fix: the CP commits with the old entries unknown; folds
    stop until the volume is repaired (E32).
15. (robustness) The boot CP's segment-table region was never released:
    one segment leaked per boot. Fix: it joins the table regions (E30).
16. (robustness) One damaged sector in an old snapshot's root page made the
    pool unbootable. Fix: the snapshot is flagged damaged and the boot
    goes on (E30).
17. (robustness) The ublk front door handed requests decoded before going
    offline to the engine. Fix: they are answered `ENODEV` (E32).
18. (robustness) A zero high watermark starved every request. Fix: taken
    as one (E32).
19. (correctness) VMware and Hyper-V disks were classified as hardware
    RAID. Fix: hypervisor markers first (E33).
20. (robustness) A logical block that does not divide 4 KiB was accepted.
    Fix: a power of two dividing 4096 (E33).
21. (robustness) A striped md array without a chunk size got the default
    stripe silently. Fix: refused (E33).
22. (robustness) Discards an administrator switched off were still
    planned. Fix: `discard_max_bytes` of zero plans none (E33).
23. (robustness) A device with more segments than a `u32` was silently cut
    short. Fix: larger segments, or a refusal; the layout refuses rather
    than clamps (E33).
24. (documentation, robustness) Dead data segments were never freed though
    E24 and the status document said they were. Fix: freed after the
    deferral (E31).
25. (durability, cited) A volume whose page index exceeds 4 GiB could not
    boot. Fix: the index bound includes the 4 GiB region limit (E32).
26. (robustness, cited) `FileBackstore::open` refused block devices. Fix:
    sized by their end (E33).

Of the hazards, the lead fixed six (the boot protection's deferral under
the trust policy, a free landing during a CP, the file backstore's empty
write, the layout helper's overflow, the ublk request bound against the
arena, replay's validation of a region's CP and first page) and recorded
the rest as limits in `06-implementation-status.md`, section 8, or as doc
notes on the structures concerned (`SegEntry::lap`, the `freed_pending`
roots and the `FreedPending` state, snapshot ids across a rewind, the FUA
flag the front door drops by design under D2, daemon recovery over
`USER_RECOVERY`).

Two of the fixes needed a correction of their own, found by the suites at
once: the rule for re-applying a free that landed during a CP had to
exclude CPs that had not yet copied the table (a walk scenario handed a
snapshot's pinned page to the next CP; pinned as
`a_free_that_lands_while_a_cp_is_staging_is_not_applied_twice`), and a
snapshot request now waits for the CP that carries the generation active
when it was made, because a fold already in flight took the request and
pinned a root without the write acknowledged just before it (found by the
extended random walk).

**Gates (2026-10-14).** fmt clean (workspace and fuzz); clippy clean on
the host, on `x86_64-unknown-linux-gnu` (the Linux halves of `otap-sys`,
`otap-backstore` and `otap-ublk` included) and on the fuzz crate; rustdoc
clean with warnings denied; `cargo test --workspace`: 458 tests and 2
doctests passed, 1 ignored (the vector writer). The clippy and rustdoc
runs each flagged a handful of lints and two redundant doc links in code
written this step (`chunks_exact` with a constant size, manual
`is_multiple_of`, a boxed error, two type aliases, a `drop` of a
reference, a `const fn` calling `max`), fixed before the gates were
rerun. `PROPTEST_CASES=10000 cargo test --workspace`: all 458 tests and 2
doctests passed, 1 ignored, in 116 min 6 s on this host
(`step4_sim_tests` 52 min, `map_tests` 19 min, `adversarial_sim_tests`
17 min, `sim_tests` 14 min; every other suite under six minutes).

## 2026-10-14: the public face of the repository

The owner asked for the repository to carry everything that exists and for
a short README about the goal of the project, ahead of making the
repository public. Everything in the working tree was already committed;
what was missing lived in session scratch directories. Changes, all of
them documentation:

- `README.md` is now the public face: what OpenTAP is for, the goal in six
  points, how the engine works in one paragraph, an honest status (the
  engine has run only on the simulator and over a plain file), the
  repository map, the two build commands and the licensing position. The
  build, test and layout guide it replaced moved to `BUILDING.md` with its
  title, its introduction and its source-control section reworded and its
  gate line naming the 10,000-case run; `06`, `07` and `09` point there.
- `docs/reviews/` archives the worker reports behind the Step 1, 2 and 4
  reviews (the format audit and the two format QA passes, the `otap-sys`
  hand-back, the two stream QA passes, the Step 4 adversarial QA report),
  verbatim apart from the hand-back wrapper and the path encoding, with an
  index. The Step 3 review's report was never saved as a file; the Step 3
  entry above remains its record.
- The mentions of the development PC's user directory in `06`, `08` and
  `09` were removed or replaced with `%USERPROFILE%`; nothing else in the
  tree names a local account. The design records (`01`, `03`, `05`) are
  session transcripts and were left as they are.
- GitHub: ten topics added (rust, storage, san, nas, iscsi, ublk, io-uring,
  homelab, block-storage, log-structured). The repository stays private
  until the owner chooses a license and flips it; the README says that no
  license has been chosen yet.

No code changed, so the gates of the Step 4 commit stand.

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
- A fold must not write what its pages already hold: a counter that is
  not zero right after the event that resets it is the smell (Step 4,
  the fold's own delta region).
- When an image is ambiguous (zeros after a punch over a write), the
  checker's prefix must be the largest consistent operation number, or
  the checker reports losses that did not happen.
- A mock of a kernel driver must follow the driver's rules, or the state
  machine ends up defending against ghosts: two of the ublk "defects"
  were the mock delivering what the driver never would (a request on an
  aborted tag, a request the abort had already withdrawn).
- A discard is a write that lands late: a segment is free when its
  discard completes, not when the discard was issued.
- One-line text substitutions over Rust source (perl, sed with regexes)
  mangled files three times in Step 4; splice whole blocks from files, or
  use the editor's exact-match replace, and re-read before the next edit.
- A free that lands while a CP is in flight belongs to whichever table
  copy exists: re-apply it only to a CP that copied the table before the
  free, or the CP's own allocation of that segment is undone.
- A snapshot's content is "everything acknowledged before the request":
  the CP that takes the request must carry the generation that was active
  when it was made, never one cut before.
- A reviewer's severity list is worth reading in order: the five
  durability defects of the Step 4 review were all in the boot, which
  the lead's own suites exercised through the simulator's crashes but
  never with a fold as the newest CP.
