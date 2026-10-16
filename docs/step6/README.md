# Step 6: work in progress

Begun on 2026-10-14; the understanding phase ended on 2026-10-15, when the
four readers that had been cut off were run again and their maps verified.
**No source file has changed**: the code is Step 5's commit `5114866`; only
documents were added. Nothing is decided yet and nothing is recorded in
`07-engineering-decisions.md`. A new session starts here.

## 1. What exists

- **The brief**, verbatim, at the end of `10-phase-c-step-briefs.md`. The
  owner's paste was cut off in the middle of the transport scope, at
  "Dual Transport Backends: 1". The rest of that scope, the fourth worker
  and anything after it never arrived.
- **Thirteen maps of the code** in `maps/`, written by read-only reader
  agents. Each maps one subsystem for the Step 6 scopes, with file:line
  facts, invariants, hook points, test patterns, pitfalls and open
  questions. `maps/00-summaries.md` holds each reader's summary, key
  findings and open questions.

  | Map | Subsystem | Scopes | Written |
  |---|---|---|---|
  | `cp-engine-cp-path.md` | the CP path, the snapshot table and the catalogue | B1 | 2026-10-14 |
  | `cp-engine-api-boot-verify.md` | the engine's surface, the boot's verification, liveness | B2, C | 2026-10-14 |
  | `format.md` | the on-disk format and the minor-bump procedure | B1 | 2026-10-14 |
  | `map.md` | the persistent map: trees, diffs, folds | B2, C | 2026-10-14 |
  | `ublk-sys-frontdoor.md` | the ublk path and the kernel's recovery protocol | A | 2026-10-14 |
  | `daemon.md` | the daemon: engine thread, boot, devices, requests | A, B, C | 2026-10-14 |
  | `control-ctl.md` | the control protocol and `otapctl` | A, B, C | 2026-10-14 |
  | `stream.md` | `otap-stream` and the sender and receiver sessions | C | 2026-10-14 |
  | `sim-harness.md` | the simulator and how Step 6's tests would be built | all | 2026-10-14 |
  | `arena-index-replay.md` | the write cache, replay and reissued requests | A | 2026-10-15, verified |
  | `docs-rules.md` | every rule that binds Step 6, what the design settled, the documents to deliver | all | 2026-10-15, verified |
  | `linux-wsl.md` | the Linux halves, the smoke test, WSL 2, the prep and kernel scripts | D, A | 2026-10-15, verified |
  | `engine-lifecycle-snapshots.md` | the snapshot and volume calls a sender and a receiver make | B, C | 2026-10-15, verified |

  The nine maps of 2026-10-14 are the readers' claims; the lead has not yet
  checked them against the code. The four of 2026-10-15 were written from
  scratch (the earlier `arena-index-replay.md`, whose reader was cut off,
  was compared with the new one, found wrong on several points, and
  replaced; what it had and the new map lacked was merged in). Each of the
  four was then checked against the code by two independent verifier
  agents per round, one re-checking every file:line citation and
  quotation, one checking that the map answers every item of its
  assignment and contradicts neither itself nor the code; an editor agent
  fixed what they found, and the next round checked again. The rounds did
  not converge to nothing: every round found between seven and thirty-two
  items, most of them minor (a citation a few lines off, a thin answer, a
  quotation attributed to the wrong line), some substantive. A final audit
  in the small hours of 2026-10-16 tagged every finding by severity and
  had a checker agent re-read every fix in place: 60 findings over the four
  maps, 8 of them substantive, all fixed; 57 fixes confirmed against the
  code, and the three found wrong or partial (one sentence in
  `docs-rules.md`, one in `linux-wsl.md`) fixed once more without a check.
  Those two sentences are the only text in the four maps that no verifier
  has read. Every finding of every round is in
  `OpenTAP-local/step6/rerun-2026-10-15/verify/`, and the counts are in
  the `08` entry for 2026-10-15. The four maps are long (190 to 250 KB
  each); they are reference material to search, not to read through.
- **The lead's preliminary positions** in `00-lead-positions.md`, written
  before the maps came back: the kernel facts read from the 6.12 driver,
  and a starting position for each scope. They are input to the design,
  not decisions.
- **Tooling, tested once**: a git worktree per worker under
  `E:\Coding\otap-wt\` that shares the main tree's target directory builds
  in seconds and leaves the main cache valid; a wrapper lets only one cargo
  process run at a time on the PC. The workspace test gate was run again on
  2026-10-15 on the unchanged tree: 541 tests and 2 doctests passed, 1
  ignored, in 2 min 49 s.
- **Worker agents run on Opus 5.5**, the lead's model only for the lead's
  own work (the owner's rule of 2026-10-15; see the `08` entry).
- **Kept outside the repository** in `E:\Coding\OpenTAP-local\` (see its
  README): the saved Linux 6.12 sources (GPL-2.0, so never in this tree),
  the workflow scripts with every reader's and verifier's full assignment,
  the raw results and the per-round findings, the four re-run maps as
  written, and the cargo wrapper.

## 2. What the maps found that changes the plan

From the nine maps of 2026-10-14, readers' claims to be verified before
anything is built on them:

- **The snapshot table is rewritten in every CP that has a snapshot,**
  changed or not. Section 8 of `06` and E39 say "in every CP that changes
  them". The catalogue is already carried forward when unchanged, though
  the engine rebuilds it in memory with a quadratic search every CP.
- **A CP log page cannot carry a snapshot or catalogue record,** so the
  incremental tables need new page types and format 2.4. The superblock
  has 2,332 reserved bytes at offset 1760. The current golden image
  `superblock_slot_minor1.bin` has test bytes exactly there, a trap for
  the vector lanes.
- **A possible unclosed defect:** at boot, metadata liveness sums every
  region the walk lists without removing duplicates, so a region that the
  live tree and a snapshot share is counted twice. A metadata segment
  holding such a region then cannot be freed until the next boot recounts
  it: a leak, in the safe direction. This is the Step 4 review's hazard
  12, never fixed and never recorded as a limit. The attestation would
  report it; a test should confirm it first.
- **The deferred frees live only in memory.** The superblock's
  `freed_pending` roots are reserved and always empty.
- **ublk recovery has two kernel facts the budget must meet.** Without
  `UBLK_F_USER_RECOVERY_REISSUE`, every request the dead daemon had fetched
  and not answered fails with EIO; with it, the kernel delivers them again.
  The flag is fixed when a device is created, so devices from Step 5
  binaries keep the old behaviour. `START_USER_RECOVERY` refuses with EBUSY
  until the old process has released the device and the kernel has
  quiesced it. When the dead daemon held every tag, only the request
  timeout starts the quiesce, and the driver sets none, so the block
  layer's default of 30 s applies (checked in the saved 6.12 driver).
  The design budget is 2 s.
- **The daemon keeps no state that finds its devices after `kill -9`.**
  The device info's `ublksrv_flags` (64 bits the driver never reads) can
  carry a pool and volume stamp.
- **The engine has no write path besides a front door.** A replication
  receiver writes through one, or through a new apply path; whether the
  replica keeps the sender's block births or stamps its own is a key
  decision.
- **The stream needs identities the pool does not record:** a snapshot's
  CP nonce, a volume uuid for `VolEvent::Create`. The stream also refuses
  a delta with no chunks, so a delta of only holes cannot be sent as the
  rules stand.
- **The attestation has to be an engine method.** Every public map walk
  stops at the first damaged page, so it needs a tolerant page walk. It
  must run while the engine is quiet, or leave out the segments a CP in
  flight is filling, and a whole-pool pass stalls the data path unless it
  is cut into slices between ticks.

From the four maps of 2026-10-15, checked against the code by agents but
not yet by the lead. Candidate defects in the code as it stands first:

- **The hold a stopped boot never recorded** (arena map, pitfall 22). A
  dirty boot's hold on the write-active volumes lives only in memory until
  the boot commit's `A` slot lands, but the attach has already rewritten
  the damaged slots and `Arena::create` has already replaced a lost arena.
  A second `kill -9`, an `--op-timeout` in `Pool::boot`'s wait or a
  `start_job` error during that boot lets the next boot serve volumes that
  lost acknowledged writes, without nvfail. The arena map proposes
  deferring those writes until the `A` slot lands; no layout change.
- **A stale seal after a kill inside a multi-block fill** (arena map,
  pitfall 21). A kill between a slot's field stores and its word store
  leaves a `Free` slot under a stale seal that the attach neither notes
  nor rewrites; behind a kept `Placed` slot it pins the tail and stalls
  every write within a lap while `damaged_slots` stays 0. The proposed fix
  treats it as a reservation at attach; no layout change.
- **Deleting a snapshot whose map page fails verification wedges the
  CPs** (engine map, trap 7.22): the `?` on `pinned_diff` in the deletion's
  walk fails `prepare_meta` every time; the boot checks only a snapshot's
  root page and index, so `damaged: false` says nothing about its pages.
- **A root `dbladed` may be unable to create any ublk device** (Linux map,
  question 15): `START_DEV` waits on the engine thread while the kernel's
  partition scan reads through the queue only that thread serves. One link
  is unverified (that `add_disk` scans synchronously); if it holds, the
  smoke test wedges at its first `volume create` and every Linux lane
  presupposes the fix. The same map finds `create_device`'s failure path
  deleting the device while the front door still holds `/dev/ublkcN`, so
  `DEL_DEV` waits forever.
- **A larger `--arena-bytes` silently recreates the arena** (arena map,
  pitfall 13), destroying acknowledged writes; and `verify` checks the
  geometry before the aggregate, so another pool's arena file under a
  larger value is recreated rather than refused.
- **Finished requests reach the kernel late** (arena map, pitfall 2):
  commits are only pushed and go to the kernel at the next poll, and with
  the gate closed `fetch` returns without pumping, so under back-pressure
  completions wait many ticks. It widens the set of requests a kill turns
  into reissues, or into EIO without `REISSUE`.

Facts that shape the design:

- **`Placed` is set before `FrontDoor::complete`**, so after a `kill -9`
  every acknowledged write that no committed CP carries is `Placed` and
  replayed, and `Placed` also holds never-acknowledged writes. Re-placing
  a reissued write is safe (it takes a sequence above every replayed one
  and wins its range; no write the client ordered after it can exist); no
  dedupe is needed; no request identity fits layout 1.2 and none could be
  matched anyway, because the kernel re-dispatches with no marker and
  possibly on another tag. The guarantee the recovery can make is stated
  in the arena map's section 5.0, with its preconditions.
- **The kernel side, from the saved 6.12 driver:** without `REISSUE` the
  handed-over requests fail with EIO; the abort waits for the block
  layer's request timeout when every tag was held, and `START_USER_RECOVERY`
  answers EBUSY meanwhile; `END_USER_RECOVERY` blocks until every tag has a
  fetch, so the fetches come first and from the serving thread; EINVAL
  from a QUIESCED device means a recovery in progress; `STOP_DEV` and
  `DEL_DEV` fail a quiesced device's held I/O and wait without timeout on
  a LIVE one; the driver lists no devices; `ublks_max` (64) counts every
  device until its `DEL_DEV`; the `ublksrv_flags` field is the only
  identity kept across the crash. These rules are to be recorded as an
  E-entry, as E28 recorded its facts, before the mock models them.
- **Snapshot ids and CP numbers are not identities**: `next_snap_id` is
  recomputed at every boot and a rewind reuses CP numbers, so the uuid is
  the only stable identity, and uuids are unique per pool, not per
  volume: a loopback test on one pool cannot take the same `to` twice.
  The replication base is the `PIN_REPLICATION` bit, not a per-volume
  field; `set_replication_base` is not atomic on error and nothing
  enforces one base per volume. A snapshot is taken only at a fold, and a
  fold that does not fit parks the request with no event, so every wait
  needs a timeout and the timeout withdraws nothing.
- **`snapshot_diff` returns the whole delta in one call** on the engine
  thread (about 6.4 GB for a full diff of 1 TiB), with no data bytes; a
  shrink between base and target is invisible to it.
- **A replica cannot be offline and still be written**: an offline volume
  answers `Status::Offline` to every request, while an online one is
  exposed as a ublk device and writable through `otapctl io write`, which
  uses the receiver's own channel. Replica visibility needs a new
  mechanism (a durable replica mark honoured at the exposure sites, or a
  submitter mark on the front door).
- **The superblock's `InboundState` is reserved but never written**, so
  the receiver's cursor commit is a separate write with a crash window;
  the engine keeps no nonce per snapshot, so the sender must read
  `cp_nonce()` in the tick that delivers its target's `SnapshotCreated`.
- **Format 2.4 has a forward-direction hazard**: the Step 5 binary boots
  a 2.4 pool, ignores its reserved bytes and rewrites the superblock
  without the 2.4 fields, against R57. A major bump, an
  incompatible-feature mark or a recorded exception is needed.
- **The brief's "pool attestation" is not the design's attestation** (the
  operator's statement about a hardware RAID cache); the nearest design
  item is the engine scrub, deferred past v0.1. "Bounds" and "LBA
  continuity" are defined nowhere, and a whole-pool pass needs a start,
  status and cancel form the control protocol lacks.
- **The brief's receiver is Phase B work pulled forward**; sockets stay
  out of `otap-stream`; `Net` and `LoopbackNet` cannot lose a connection,
  which every resume test needs; the daemon hosts one pool where the
  loopback harness has two; the 7.3 "durable on return" signatures clash
  with an engine that reports at a later CP; and which thread the engine
  halves run on (rule 83) is a decision against `05:587`'s lock clause.
- **No startup capability gate exists**: nothing calls `Allowlist::probe`,
  against eight statements in five files.
- **WSL appears in no design document.** The design's Linux lane is
  Debian 13 under QEMU/KVM; the "kernel suites" are the dm-log-writes and
  dm-flakey lanes, none built; only three Linux-only tests exist and the
  kernel-facing code has none; on Linux `HugeArena::anonymous` is
  `MAP_HUGETLB`, so `cargo test` needs about 32 huge pages reserved; the
  README's own `sudo scripts/linux-smoke.sh` sequence fails on rustup's
  PATH under sudo; the kernel line (6.12 per the brief, 6.18 as WSL ships)
  is a choice to record, and 6.18's driver must be saved and re-read
  before the re-attach is written against it.
- **The brief's "four Opus workers" collides with `09`'s "the lead owns
  coherence"**; Step 5 met the same instruction by building the
  construction scopes itself and delegating the review. Scopes B and C
  both change `prepare_meta`, `finalize` and the boot.

## 3. What the owner needs to provide

- **The rest of the Step 6 brief**, from "Dual Transport Backends: 1"
  onward. Without it the lead will read the transport as doc 05 Part 7.3
  and week 11 describe it: a TCP stream on loopback, TLS later, and a file
  sink behind `dump --since` and `restore`.
- **The WSL 2 host**: BIOS "SVM Mode" on and a reboot (the features were
  enabled on 2026-10-14; firmware virtualization still reads off and the
  inbox `wsl.exe` is still the stub).
- Everything else in the readers' open questions is an architect's call
  and will be recorded in `07`, starting at E40.

## 4. Next, in order

1. Add the rest of the brief to `10` when the owner sends it.
2. The lead reads: check the nine unverified maps' plan-changing claims
   (section 2, first list) against the code, read the four verified maps'
   candidate defects and decide which become Step 6 items with tests,
   and record the 6.12 driver's recovery rules as an E-entry.
3. Design one proposal per scope, judged against the maps and the
   positions: A, the recovery re-attach and the reaper; B1, the incremental
   tables; B2, the verify pass; C, the transport; D, the WSL 2 tooling.
   Record the decisions from E40 onward and list the owner's questions.
4. Land a skeleton commit for the files every scope touches: the protocol
   variants, the `otapctl` verbs, the render arms and the round-trip
   entries. Each worker then owns separate files.
5. Implement with one worker per scope (Opus), each in its own worktree on
   a `step6/<scope>` branch, sharing the target directory through the
   one-cargo lock, building with `-j 2`. The lead merges.
6. An independent adversarial review of the merged tree; every finding
   fixed and pinned by a test.
7. The gates of `09` section 3, with the 10,000-case run in the
   background (about three hours or more).
8. The documents (`06`, `07`, `08`, the reading note in `10`,
   `BUILDING.md`, the README), one commit, push.
