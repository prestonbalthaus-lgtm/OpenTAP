# Step 6: the lead's preliminary positions (input to the design phase)

These are the architect's starting positions after reading the state of the
tree, the Step 5 records and the saved 6.12 ublk driver. They are not
decisions yet: each design proposal must either adopt a position with the
reasons, or argue against it with evidence from the code or the kernel.

## Facts established from the kernel source (ublk_drv.c, v6.12)

- `UBLK_F_USER_RECOVERY` alone: when the daemon dies, requests it had fetched
  and not committed are FAILED (`__ublk_fail_req` -> `ublk_put_req_ref`,
  -EIO to the block layer); requests it had not fetched stay queued.
- `UBLK_F_USER_RECOVERY_REISSUE` in addition: fetched-and-uncommitted
  requests are REQUEUED and re-dispatched to the new daemon after
  `END_USER_RECOVERY` (`blk_mq_requeue_request`, kicked at END).
- `START_USER_RECOVERY` returns -EBUSY until (1) the dying process has
  released `/dev/ublkcN` (UB_STATE_OPEN clear) and (2) the quiesce work has
  run (state == UBLK_S_DEV_QUIESCED). The quiesce work is scheduled from the
  dying task's uring_cmd cancel path or from the request timeout path. So the
  new daemon must retry START with a short period inside the budget.
- After START: `ublk_queue_reinit` forgets every io slot; the new daemon
  opens `/dev/ublkcN` (exclusive: -EBUSY if still open), mmaps the
  descriptors, pushes FETCH_REQ on every tag; `END_USER_RECOVERY` waits for
  all queues ready, takes the new pid in `data[0]`, unquiesces, state LIVE.
- `ublksrv_ctrl_dev_info.ublksrv_flags` (u64) is "for ublksrv internal use,
  invisible to the ublk driver": userspace can stamp a device with an
  identity at ADD_DEV and read it back with GET_DEV_INFO from any process.
- `DEL_DEV` goes through `ublk_stop_dev`, which handles QUIESCED devices
  (quiesce if LIVE, unquiesce with force_abort, del_gendisk): another
  process can delete a dead daemon's device.
- The current code creates devices with USER_COPY | USER_RECOVERY |
  CMD_IOCTL_ENCODE: no REISSUE, no identity stamp, no recovery path.

## Scope A: recovery and the reaper (proposed E40)

1. Add `UBLK_F_USER_RECOVERY_REISSUE`. Every request OpenTAP serves is
   idempotent at the block level (a write re-executed carries the same bytes
   to the same LBAs; discards, zeroes, reads, flushes trivially), so a
   re-dispatched request is executed again rather than failed with EIO. The
   guarantee: no request the kernel completed is executed twice; no request
   the kernel had issued is lost; a request in flight at the crash is
   executed once or twice with the same effect. Design agents: confirm or
   refute with the arena/index analysis (ordering hazards with concurrent
   writes to the same LBA).
2. Stamp every device: `ublksrv_flags = MAGIC(16) << 48 | pool_fp(32) << 16
   | vol(16)` with pool_fp = CRC-32C of the aggregate id. Any process can
   then classify a device as ours / which pool / which volume without a
   state file.
3. Recovery state machine in `otap-ublk` (a `recovery` module, Linux impl
   plus a mock): Found -> Starting (START, retry on EBUSY every ~20 ms) ->
   Fetching (open, mmap, FETCH all) -> Ending (END with our pid) -> Live,
   or Failed(reason, at which step). Budget: 2 s target, 5 s hard (R57);
   what the daemon does when the hard budget is exceeded is a decision
   (proposal: reap and create a fresh device, with a warning event, since
   serving matters more than the device number).
4. The daemon at boot (Linux): after the boot commit and BEFORE creating any
   device: enumerate devices, classify; for each online volume with a
   recoverable device of this pool run the recovery; devices of this pool
   whose volume is gone or offline are reaped; devices of this pool with a
   LIVE server pid that is alive and is not us: refuse to boot (two daemons
   on one pool); foreign pools' devices are left alone and listed.
5. A `UblkControlPlane` trait (list/info/add/params/start/stop/delete/
   start_recovery/end_recovery/pid_alive) with the Linux implementation
   over `otap-sys` and a mock registry in `otap-ublk::mock`, so the daemon's
   recovery and reaper logic runs on Windows in tests; the mock transport
   gains device survival across the front door's drop, QUIESCED, the
   re-dispatch of uncommitted tags after END, unfetched requests kept, and
   EBUSY until the old owner is gone.
6. `otapctl device list [--all]` and `otapctl device reap [--dev N |
   --stale] [--force]`, served with or without a booted pool.
7. `scripts/linux-smoke.sh`: after SIGKILL the restarted daemon must serve
   the SAME `/dev/ublkbN`; the script also orphans a device on purpose and
   reaps it.

## Scope B1: incremental tables (proposed E41, format 2.4)

1. Keep the record formats (SnapRecord 64 B, CatalogRec 512 B) and their
   image builders/parsers. Add a tombstone marker to each record type (a
   SnapKind or flag bit; a CatalogRec flag) so a DELTA REGION is just a
   smaller table image whose records fold over the base: upsert by key,
   tombstone deletes.
2. Add a 4 KiB `TableHeadPage` (new magic) that names the base region
   (RootRef + record count) and the delta regions since the last fold (in
   order, up to ~250), with the aggregate/CP/nonce identity and a CRC. The
   superblock's `snap_table` and `catalog` RootRefs point at the head page.
   A boot tells a 2.3 whole-table region from a 2.4 head page by magic and
   accepts both; the first naming CP of a 2.3 pool writes heads (the
   in-place upgrade), and the superblock minor becomes 2.4.
3. Per CP that changes a table: one delta region (usually one page) and one
   head page. A FOLD (fresh base, empty chain) when the chain reaches a
   bound, when the delta records outnumber the base, and at clean shutdown;
   the old base and the deltas are released after the next commit like any
   region (E24).
4. Format 2.4: F19 tombstones, F20 the head page, F21 the chain rules;
   vectors `v2.4/`, the older sets kept; golden-vector procedure as in
   BUILDING.md.

## Scope B2: the attestation (proposed E42)

1. `otap_cp::verify`: a resumable `Verifier` run on the engine thread in
   budgeted slices between ticks (so the devices keep being served) over a
   captured committed view (CP N's roots, tables, segment table). While it
   runs: deferred frees are HELD (nothing freed at N is reused) and the
   cleaner is paused, so the view stays verifiable; writes and CPs continue.
   Design agents: weigh this against a simpler blocking pass when quiet.
2. Phases: superblock ring (4 slots: CRC, identity, monotonic ranking);
   roots table; every volume's index and pages (page CRCs, bounds, LBA
   continuity); every snapshot's root page and tree; every live segment's
   stripe summaries (header, identity) and block payloads (CRC against the
   map entry's CRC and the expected (lseg, off, birth_cp)); reconciliation
   of recomputed live blocks per segment against SegEntry::live_bytes and
   the deferred-free lists; a structured report (findings with kind, where,
   detail; per-phase counts; a verdict), never a panic.
3. `otapctl pool verify [--wait]` and `pool verify status`; the report in
   text and JSON; the pool status shows a verification in progress.

## Scope C: the transport (proposed E43; wire rules S11+ if any)

1. A new crate `otap-repl` (the replication engine: `Link` trait, TCP and
   Unix-socket links over std, the file sink/source, the sender and receiver
   sessions over `otap-stream`, the pipe's cursor persistence) so no socket
   enters `otap-stream` and `otap-cp` stays the engine.
2. Sender: base B (PIN_REPLICATION), target T (a new snapshot, pinned),
   delta = T's entries with birth_cp > B's CP (`snapshot_diff`) as extents
   and holes; chunks are stripe images PACKED BY THE SENDER (header,
   VolBitSet, descriptors, blocks with CRCs), not on-disk stripes; frames
   DeltaBegin{Increment{from: B, to: T}} -> VolEvent (create/resize on the
   target) -> Chunk/Holes under credits -> DeltaEnd; on Committed the base
   moves to T (`set_replication_base`). A first transfer has no base: the
   mode for it is a design question (Steady? Increment from nil?).
3. Receiver: stages chunks durably (a staging file in a `--state-dir`),
   persists the CursorPage after each durable landing, grants credits in
   Landed, applies the delta at DeltaEnd through the engine (writes and
   punches, then the snapshot with T's uuid), waits for the committing CP,
   persists `applied`, replies Committed. The replica volume is offline to
   clients. Reconnect resumes from the cursor; a daemon restart resumes from
   the persisted CursorPage and staging file.
4. Where the sender/receiver run: the engine is one thread; block reads and
   writes must happen there. Proposal: sender and receiver 'apply' as
   budgeted engine-thread work (like the verifier) feeding/consuming a
   transport thread through bounded channels.
5. Tests on Windows: two in-process daemons (or two engines) over files,
   connected by loopback TCP; disconnects mid-delta and resumes; the
   simulator's LoopbackNet for deterministic partition tests if the Net
   trait fits; dump --since to a file and restore.

## Scope D: the WSL 2 tooling

`scripts/wsl2-prep.sh` (probe mode by default: kernel, io_uring, hugetlbfs,
/dev/ublk-control, tools, cargo, systemd; `--install` installs packages;
never reboots or touches firmware), `scripts/wsl2-kernel.sh` (reproducible
Microsoft-kernel build with CONFIG_BLK_DEV_UBLK and CONFIG_DM_LOG_WRITES,
output to /mnt/e/WSL/kernel, prints the .wslconfig line),
`scripts/linux-tests.sh` (the Linux-only suites with their privileges), and
the updated `scripts/linux-smoke.sh` (recovery re-attach, device list/reap,
pool verify, a loopback replication between two loop devices).

## Working rules for the step

- Four implementation workers in parallel in git worktrees under
  E:/Coding/otap-wt/<scope>, each on a branch step6/<scope>, sharing
  CARGO_TARGET_DIR=E:/Coding/OpenTAP/target through the cargo-one.sh lock
  (one cargo at a time; -j 2; --test-threads=6). The lead merges, then an
  independent adversarial review attacks the merged tree, then the gates.
- Each worker owns new modules/crates and makes additive changes to shared
  files (protocol enums, args, render, daemon dispatch); the lead resolves
  the merges.
