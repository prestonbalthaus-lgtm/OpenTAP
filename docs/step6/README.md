# Step 6: work in progress

Paused on 2026-10-14 in the evening, at the end of the first phase. **No
source file has changed**: the code is Step 5's commit `5114866`; only
documents were added. Nothing is decided yet and nothing is recorded in
`07-engineering-decisions.md`. A new session starts here.

## 1. What exists

- **The brief**, verbatim, at the end of `10-phase-c-step-briefs.md`. The
  owner's paste was cut off in the middle of the transport scope, at
  "Dual Transport Backends: 1". The rest of that scope, the fourth worker
  and anything after it never arrived.
- **Ten maps of the code** in `maps/`, written by read-only reader agents.
  Each maps one subsystem for the Step 6 scopes, with file:line facts,
  invariants, hook points, test patterns, pitfalls and open questions.
  `maps/00-summaries.md` holds each reader's own summary, key findings and
  open questions. These are the readers' claims; the lead has not yet
  checked them against the code.

  | Map | Subsystem | Scopes |
  |---|---|---|
  | `cp-engine-cp-path.md` | the CP path, the snapshot table and the catalogue | B1 |
  | `cp-engine-api-boot-verify.md` | the engine's surface, the boot's verification, liveness | B2, C |
  | `format.md` | the on-disk format and the minor-bump procedure | B1 |
  | `map.md` | the persistent map: trees, diffs, folds | B2, C |
  | `ublk-sys-frontdoor.md` | the ublk path and the kernel's recovery protocol | A |
  | `daemon.md` | the daemon: engine thread, boot, devices, requests | A, B, C |
  | `control-ctl.md` | the control protocol and `otapctl` | A, B, C |
  | `stream.md` | `otap-stream` and the sender and receiver sessions | C |
  | `sim-harness.md` | the simulator and how Step 6's tests would be built | all |
  | `arena-index-replay.md` | the write cache, replay and reissued requests | A |

- **Three maps are missing.** Their readers stopped on a usage limit
  before writing anything: `engine-lifecycle-snapshots` (the snapshot and
  volume calls a replication sender and receiver make), `docs-rules`
  (every rule that constrains Step 6 and the documents it must deliver)
  and `linux-wsl` (the Linux halves, the smoke test's host assumptions,
  the WSL 2 traps, the prep and kernel scripts). The reader of
  `arena-index-replay.md` wrote its map in full but stopped before handing
  back its summary.
- **The lead's preliminary positions** in `00-lead-positions.md`, written
  before the maps came back: the kernel facts read from the 6.12 driver,
  and a starting position for each scope. They are input to the design,
  not decisions.
- **Tooling, tested once**: a git worktree per worker under
  `E:\Coding\otap-wt\` that shares the main tree's target directory builds
  in seconds and leaves the main cache valid; a wrapper lets only one cargo
  process run at a time on the PC. The whole workspace's test binaries were
  built on 2026-10-14, so the cache is warm.
- **Kept outside the repository** in `E:\Coding\OpenTAP-local\` (see its
  README): the saved Linux 6.12 sources (GPL-2.0, so never in this tree),
  the workflow script with every reader's full assignment, the readers' raw
  results, and the cargo wrapper.

## 2. What the maps found that changes the plan

Readers' claims, to be verified before anything is built on them:

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

## 3. What the owner needs to provide

- **The rest of the Step 6 brief**, from "Dual Transport Backends: 1"
  onward. Without it the lead will read the transport as doc 05 Part 7.3
  and week 11 describe it: a TCP stream on loopback, TLS later, and a file
  sink behind `dump --since` and `restore`.
- Everything else in the readers' open questions is an architect's call
  and will be recorded in `07`, starting at E40.

## 4. Next, in order

1. Add the rest of the brief to `10` when the owner sends it.
2. Run the three missing readers again. Their assignments are in
   `E:\Coding\OpenTAP-local\step6\understand-workflow.js`; point its
   scratch path at the new session's scratchpad and the kernel files at
   `E:\Coding\OpenTAP-local\kernel-6.12\`.
3. Design one proposal per scope, judged against the maps and the
   positions: A, the recovery re-attach and the reaper; B1, the incremental
   tables; B2, the attestation; C, the transport; D, the WSL 2 tooling.
   Record the decisions from E40 onward and list the owner's questions.
4. Land a skeleton commit for the files every scope touches: the protocol
   variants, the `otapctl` verbs, the render arms and the round-trip
   entries. Each worker then owns separate files.
5. Implement with one worker per scope, each in its own worktree on a
   `step6/<scope>` branch, sharing the target directory through the
   one-cargo lock, building with `-j 2`. The lead merges.
6. An independent adversarial review of the merged tree; every finding
   fixed and pinned by a test.
7. The gates of `09` section 3, with the 10,000-case run in the
   background (about three hours or more).
8. The documents (`06`, `07`, `08`, the reading note in `10`,
   `BUILDING.md`, the README), one commit, push.
