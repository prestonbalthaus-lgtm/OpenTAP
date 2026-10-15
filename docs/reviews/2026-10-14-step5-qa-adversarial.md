# Step 5 adversarial QA report (worker "Opus-QA-Adversarial", 2026-10-14)

An independent review of the uncommitted Step 5 working tree against the
Step 5 brief (`../10-phase-c-step-briefs.md`, section 4 of the brief) and
the rulings E34 (S5-1 to S5-4), E24, E25 and E31: snapshot deletion and
pins, volume rollback, resize, deletion and offline, the segment cleaner
and its reserve, format 2.3 (the catalogue, the stripe list, the offline
flag), `dbladed` and the control protocol. The lead's tests were read for
the harness patterns only; every scenario below is the reviewer's own.

## Files created

| File | Tests | `bug_*` (fail) | Pinned (pass) |
|---|---|---|---|
| `crates/otap-sim/tests/qa_step5_tests.rs` | 18 | 10 | 8, including the brief's two property tests |
| `crates/otap-daemon/tests/qa_daemon_tests.rs` | 9 | 4 | 5 |
| `crates/otap-control/tests/qa_protocol_tests.rs` | 5 | 1 | 4 |

No non-test file was changed. `crates/otap-cp/tests/qa_lifecycle_tests.rs`
(optional) was not needed: every engine path the review attacks is reachable
through the simulator's `Sim::engine()` between ticks.

## Verdict

Step 5 is not ready to commit. The happy paths, the crash matrix the lead
wrote and both of the brief's properties (run here at 1,000 cases each)
hold, but the review finds thirteen defects, twelve of them reproduced by
fifteen failing tests. Three lose data: a rollback that races a cleaning CP
points the volume at a segment the same CP drains; a rollback accepted
while a snapshot of the volume is in flight forks the snapshot history,
after which deleting the rollback's target releases blocks the live map
still maps; and a stripe summary that does not verify makes the cleaner
free the live blocks behind it. A fourth, a volume deletion accepted while
a snapshot of it is in flight, leaves a snapshot of an absent volume that a
new volume at the same index inherits. Three wedge the pool: deleting a
snapshot that maps blocks beyond a since-shrunk volume fails every CP from
then on (in the daemon, the planned stop too); cleaning blocks that many
snapshots share needs metadata the plan never budgets, so CPs fail for
space, the pool is marked full while the admission rule still admits
writes, and every retry eats more of the reserve; and one damaged byte in
the catalogue makes every candidate of the ring torn, so the pool no longer
boots. The common thread of the engine defects is that the cleaner's
plan, the snapshot set and the volume operations are decided at different
moments (the plan when the CP is cut, the deletions and operations when its
metadata is prepared, the in-flight snapshot when it commits) and nothing
stops a request that lands in between.

## (a) Findings, by severity

Severity classes: **D** durability or data loss, **A** accounting, **W**
availability or wedge, **R** protocol and robustness, **C** cosmetic.

### D1. A rollback asked for while a cleaning CP stages lands on the drained segment

- **Test:** `qa_step5_tests.rs::bug_a_rollback_asked_while_a_cleaning_cp_stages_points_the_volume_at_drained_blocks`
  (fails: `LivenessDrift { pseg: 5, counted: 0, found: 8 }, { pseg: 8, counted: 0, found: 13 }`).
  Past that assertion (checked with a throwaway copy of the test, since
  deleted) the volume reads as the snapshot right after the rollback, and
  three CPs later, the drained segments freed and discarded, `peek` answers
  "volume 0 LBA 192 failed its CRC": the rolled-back volume lost its data.
- **Scenario:** blocks only snapshot R references sit in fragmented
  segments; the volume is offline and quiet; `request_clean`; while the
  cleaning CP stages its relocation stripes, `rollback_volume(vol, R)`.
- **Root cause:** `Engine::rollback_volume` → `check_vol_op` /
  `in_flight_touches` (engine.rs 2544, 2563) look at the CP's generation
  (`job.blocks`, `job.holes`) and not at `job.clean`, so the rollback is
  accepted. `prepare_meta` (4531) runs at the end of that very staging,
  `should_fold` sees the pending operation, and `prepare_meta_inner` (4570)
  builds `Rollback { root, index }` from `self.snapshots`, i.e. R's root
  before the relocation, then rewrites R to the new addresses in the
  `rewritten` loop of the same CP and forces the drained segment's count to
  zero (`data_live.insert(target.pseg, 0)`, 5089). The live map now names
  the old addresses of a segment that is counted empty, freed after the
  deferral and discarded whole on flash.
- **Fix:** make the cleaner's exclusion two-sided. `plan_clean` already
  refuses to plan while a deletion or a volume operation waits; the CP that
  carries clean targets must equally refuse to take deletions and volume
  operations that arrived after its plan: in `prepare_meta`, when
  `!job.clean.is_empty()`, leave `self.snap_deletes` and every `v.op` for the
  next CP (and do not let them make this CP a fold). Defence in depth: build
  a rollback's root after the `rewritten` loop, from the rewritten state.

### D2. A rollback is accepted while a snapshot of the volume is in flight; deleting its target then releases live blocks

- **Test:** `qa_step5_tests.rs::bug_a_rollback_is_accepted_while_a_snapshot_of_the_volume_is_in_flight`
  (fails: "the rollback was accepted with snapshot(s) [2] newer than its
  target"). Past that assertion (checked with a throwaway copy, since
  deleted) the deletion of the target leaves `LivenessDrift { pseg: 2,
  counted: 0, found: 8 }`, and three CPs later the live volume answers
  "volume 0 LBA 0 failed its CRC".
- **Scenario:** write A, snapshot R, write B, quiesce; `create_snapshot`;
  once that CP is past its metadata preparation, `rollback_volume(vol, R)`.
- **Root cause:** `rollback_volume` (2576) counts newer snapshots in
  `self.snapshots` and waiting requests in `self.snap_requests`, but a
  request the CP in flight already took lives in `in_flight_job().snaps`
  and is neither. The rollback lands one CP after the newer snapshot S, so
  the volume's history forks: the live tree is R's, S holds B. The deletion
  logic in `prepare_meta_inner` (the `older`/`newer` neighbour walk, 4740 to
  4800) and `pinned_by` assume the snapshots of a volume form one chain whose
  newest member is an ancestor of the live tree; deleting R compares R's
  entries with S's (B), releases A, and the live map's A is counted dead.
- **Fix:** treat a snapshot request held by the CP in flight like a waiting
  one: in `rollback_volume` refuse with `VolumeBusy` when `in_flight_job()`
  holds a snapshot request of the volume (the same check belongs in
  `delete_volume`, see D4, and in a shrink).

### D3. A damaged stripe summary makes the cleaner free the live blocks behind it

- **Test:** `qa_step5_tests.rs::bug_a_damaged_stripe_summary_makes_the_cleaner_free_live_blocks`
  (fails: reads answer `Corrupt`; the engine said so itself:
  `internal: segment 2 counted 7 live blocks after draining (9 found, 16 before)`).
- **Scenario:** one byte of a stripe's summary header is damaged on the
  medium (the data blocks are intact and still read); `request_clean`.
- **Root cause:** `cleaner::scan` (cleaner.rs 211) `continue`s past a stripe
  whose summary does not parse, treating it as never written, so its blocks
  are neither proven referenced nor relocated; `prepare_meta_inner` then sets
  the drained segment's count to zero whatever the scan found (5089), with
  only an `Internal` event, and the deferred free discards live data. The
  `SkipReason::Summary` variant documents the intended behaviour ("a stripe
  summary does not parse") but `scan` does not return it for that case.
- **Fix:** a summary that fails its CRC is damage, not absence: return
  `Err(SkipReason::Summary)` unless the summary bytes are all zero (a stripe
  never written); and never drain a segment whose scan found fewer
  referenced blocks than the table counts (skip it with `CleanSkipped`
  instead of forcing zero).

### D4 (A). A volume deletion is accepted while a snapshot of it is in flight; the snapshot outlives its volume

- **Test:** `qa_step5_tests.rs::bug_a_volume_deletion_is_accepted_while_a_snapshot_of_it_is_in_flight`
  (fails: "snapshots of absent volumes: [(1, 1)]").
- **Root cause:** `delete_volume` (2646) counts `self.snapshots` and
  `self.snap_requests`, not the in-flight job's requests (same gap as D2).
  The snapshot keeps its blocks counted (`pinned_by`), names a volume index
  that is free again, and a volume created later at that index inherits it:
  `rollback_volume` of the new volume to it is accepted (`record.vol` is all
  it checks), exposing the deleted volume's data to whoever owns the new one.
- **Fix:** as D2.

### W1. Deleting a snapshot that maps a block beyond the volume's current end fails every CP

- **Tests:** `qa_step5_tests.rs::bug_deleting_a_snapshot_after_its_volume_shrank_fails_every_consistency_point`,
  `qa_step5_tests.rs::bug_deleting_a_snapshot_longer_than_its_newer_neighbour_fails_every_consistency_point`,
  `qa_daemon_tests.rs::bug_deleting_a_snapshot_after_a_shrink_breaks_the_pool_and_its_clean_stop`
  (fail: `tick: map: volume 0 LBA 40 is out of range`; in the daemon both
  the deletion and the following `PoolStop` answer `engine tick: map: volume
  0 LBA 6000 is out of range`).
- **Scenario:** write LBA 40 of a 64-block volume, snapshot, quiesce, shrink
  to 32, delete the snapshot (or take a second snapshot after the shrink and
  delete the first).
- **Root cause:** the deletion loop of `prepare_meta_inner` asks
  `map.get(vol, lba)` (the live map) or `map.pinned_get(newer, lba)` for each
  entry of the deleted snapshot; both refuse an LBA beyond the volume's or
  the newer snapshot's end with `MapError::OutOfRange`, which falls into
  `Err(err) => return Err(err.into())` (4776, 4791). The deletion stays in
  `snap_deletes`, so every later CP prepares it again and fails: no CP ever
  commits again until a restart forgets the request. A shrink with
  snapshots is legal (`resize_volume` does not refuse it), and the
  cleaner's own lookups already treat `OutOfRange` as "not referenced".
- **Consequence in the daemon:** the request answers an engine error, every
  tick fails (and `Daemon::tick` prints the error on every idle-loop pass,
  hundreds of lines a second), `pool sync` cannot commit, and `pool stop`
  fails; the daemon has already taken the pool out of its slot, so the pool
  is dropped without the clean record: a planned stop becomes a dirty boot,
  every write acknowledged since the deletion was asked for (no CP commits
  after it) is lost with the heap arena, and nvfail holds the volumes.
- **Fix:** in both arms treat `MapError::OutOfRange` as `false` (the block is
  beyond what the neighbour or the live volume maps, so it is the deleted
  snapshot's alone). More generally, an error in the metadata preparation of
  a request the operator made should fail the request (`SnapshotFailed`-like
  event, the request dropped), not every future CP.

### W2. Cleaning blocks that many snapshots share runs the pool out of space and consumes the reserve

- **Test:** `qa_step5_tests.rs::bug_cleaning_blocks_that_many_snapshots_share_runs_the_pool_out_of_space`
  (fails: "3 CPs failed for space; pool_full true, free 7 of 48, reserve 8,
  admissible 0 blocks").
- **Scenario:** 160 scheduled snapshots of a quiet volume (five folds of 32
  requests, so they share one tree; 255 are allowed), the blocks they pin
  overwritten in the live volume, then a second volume filled until the
  admission rule refuses. The background cleaner (on by default) starts at
  70% utilisation.
- **Root cause, three parts:**
  1. `plan_clean` (3968) budgets the relocation's data plus one segment for
     the CP's metadata (`needed = …div_ceil(per_segment) + 1`, 4010), but
     `prepare_meta_inner` rewrites, for every snapshot in any relocated
     block's `snaps`, a copy-on-write map page (16 KiB), an index region and
     a root page (`MapSet::rewrite_pinned` per snapshot), about six blocks
     per snapshot per touched page: 160 snapshots need about 960 blocks, 15
     segments of the simulator's 64-block segments (3 of the 2 MiB segments
     the daemon gives files under 1 GiB). Snapshots that shared one tree
     each get their own copy, so a successful clean also multiplies the
     pinned metadata.
  2. A CP that fails for space with clean targets is retried with the same
     plan: `fail_cp` re-arms `clean_requested` and nothing like
     `fold_blocked_at` stops the next `plan_clean`; every CP that carries
     the clients' writes carries the doomed clean and fails.
  3. Every failed attempt re-stages the generation and the relocation into
     data segments taken from the live table (`open_data_segment`, 4448);
     `fail_cp` does not give them back, they are reclaimed only by the dead
     data pass after a later CP commits. In the reviewer's trace the free
     count fell 10, 7, 4 across three attempts: the reserve S5-3 keeps for
     exactly these CPs is spent by the failures.
  While `pool_full` holds, `serve` answers discards `NoSpace` too, so a
  client cannot punch its way out, which S5-3 promises it can. A snapshot
  deletion lets one CP through (`plan_clean` skips while a deletion waits)
  and the next CP plans the same clean with one snapshot fewer. The same
  trace also hit W3 below on the second of three deletions.
- **Fix:** count the metadata of the snapshot rewrites in the plan (per
  candidate: the distinct pinned pages it touches times the snapshots
  referencing them, plus an index and a root page per snapshot) and stop
  adding candidates that do not fit; rewrite each distinct old page once and
  point every snapshot that shared it at the one copy (likewise identical
  indexes), so a clean never un-shares a tree; on a failure for space of a
  CP with clean targets drop the clean for the retry (`clean_blocked_at`,
  as E31 did for folds); give a failed CP's freshly allocated data segments
  back to the table.

### W3. A snapshot deletion or a volume operation asked for while a cleaning CP stages fails the tick

- **Tests:** `qa_step5_tests.rs::bug_a_snapshot_deletion_asked_while_a_cleaning_cp_stages_fails_the_tick`
  (fails: `tick: internal: relocation for snapshot 1, which is gone`),
  `qa_step5_tests.rs::bug_a_volume_deletion_asked_while_a_cleaning_cp_stages_fails_the_tick`
  (fails: `tick: internal: volume 0 has deltas in the CP that carries its delete`).
- **Found independently:** the full-pool property hit it with a two-step
  minimal case (`plan = [Punch, Snapshot]`, then two deletions in a row: the
  background pump had started a cleaning CP between them), and the W2 trace
  hit it on the second of three deletions. On a pool past the cleaner's
  threshold it is the normal outcome of deleting snapshots one after
  another.
- **Root cause:** as D1: `delete_snapshot` (2424), `delete_volume`, a shrink
  and a rollback are accepted while the CP in flight carries clean targets,
  and `prepare_meta_inner` processes them in that CP: the deleted snapshot
  is removed from `remaining` before the `rewritten` loop looks for it
  (4843), or the volume's relocation deltas trip the "deltas in the CP that
  carries its op" guard (4632). The error leaves `step_staging` through
  `other => Err(other)` (4296), which drops the job: its snapshot requests
  vanish without `SnapshotFailed` (a daemon `snapshot create` then waits for
  its timeout), its fold and clean requests are lost, and the stripes it
  wrote are abandoned. In the daemon the waiting control request fails with
  an engine error although the deletion lands at a later CP.
- **Fix:** as D1 (defer deletions and volume operations that arrived after
  the plan); route every error of the metadata preparation through
  `fail_cp` so the job's requests go back or are answered.

### W4. One damaged byte in the catalogue makes the pool unbootable

- **Test:** `qa_step5_tests.rs::bug_a_damaged_catalogue_fails_every_boot`
  (fails: "no verifiable consistency point among 4 candidates; CP 4: map:
  catalog: crc32c mismatch … CP 3: … (all four)").
- **Root cause:** `boot::walk_cp` turns a catalogue that does not verify into
  a torn candidate (`read_root(…, "catalog")?`, `parse_catalog(…)
  .map_err(TornReason::Map)`, boot.rs 459 to 462). F16 carries the region
  forward unchanged in every commit that changes no name, so every candidate
  of the ring names the same region and every one is torn. Names are the
  least important thing on the pool, and the snapshot roots (E30, defect 16
  of the Step 4 review) already have the rule that damage older than the
  candidate is reported, not fatal.
- **Fix:** tear the candidate only when the catalogue region was written by
  that CP (a torn write); otherwise boot with an empty catalogue and an
  operator warning ("the catalogue does not verify; names were dropped"),
  and let the next CP that changes a name write a fresh one.

### W5. The daemon refuses a pool whose A slots were wiped, although its B slots hold it

- **Tests:** `qa_daemon_tests.rs::bug_a_pool_whose_a_slots_were_wiped_is_refused_although_its_b_slots_hold_it`
  (only A0 and A1 are zeroed, the first 128 KiB of the 64 KiB-stripe pool;
  fails: "holds no OpenTAP superblock at its start; format it first");
  the engine itself boots such a device clean with every block and name
  (`qa_step5_tests.rs::the_engine_boots_from_the_b_slots_when_both_a_slots_are_wiped`, passes).
- **Root cause:** `otap-daemon/src/backstore.rs::probe_superblock` (163)
  reads A0 at byte 0 and A1 at each candidate stripe offset, never the B
  slots F7 puts at the end of the device for exactly this case (`wipefs`, a
  partitioning tool, a `dd` of the first megabytes). Worse, `pool format`
  without `--force` relies on the same probe, so it would format over the
  pool (see R2).
- **Fix:** probe B0 and B1 too, at `size - 2 * slot_stripes * stripe` (and
  the slot before) for each candidate stripe, accepting a slot whose
  recorded capacity matches the position.

### R1. `snapshot rollback --delete-newer` deletes snapshots before it finds one it cannot

- **Test:** `qa_daemon_tests.rs::bug_a_rollback_with_delete_newer_destroys_snapshots_before_it_meets_a_pinned_one`
  (fails: after the refusal the volume has `["s1", "s3"]`; s2 is gone).
- **Root cause:** `Pool::snapshot_rollback` (pool.rs 936) deletes the newer
  snapshots one by one through `delete_snapshot_by_id`, each a committed CP,
  and only the deletion of the replication base (s3) is refused; the
  rollback is then refused too, with s2 already destroyed.
- **Fix:** check every newer snapshot (pins, replication base, damage) and
  the target before deleting any; refuse up front.

### R2. `pool format` without `--force` overwrites any existing file or device that is not an OpenTAP pool

- **Test:** `qa_daemon_tests.rs::bug_pool_format_without_force_overwrites_an_existing_file_that_is_not_a_pool`
  (fails: `Formatted { … }` and 393,216 bytes of the 128 MiB file changed).
- **Root cause:** `Pool::format` (pool.rs 159) refuses only when
  `probe_superblock` finds an OpenTAP superblock; a file of someone's data
  or, on Linux, a disk with a filesystem is formatted without `--force`.
- **Fix:** without `--force`, refuse an existing file or device unless its
  first and last megabyte are all zero (what `mkfs` tools do with their
  signature probes); with W5's probe fixed this also protects a pool whose
  A slots are damaged.

### R3. A misspelled optional field decodes as its absence

- **Test:** `qa_protocol_tests.rs::bug_a_misspelled_optional_field_decodes_as_its_absence`
  (fails: `{"op":"snapshot_replication_base","volume":"data","nmae":"s1"}`
  decodes as `SnapshotReplicationBase { volume: "data", name: None }`).
- **Root cause:** the request types do not deny unknown fields, and an
  optional field's absence means something: here "release the replication
  base", the opposite of what the client asked; likewise a misspelled
  `size_bytes` of `PoolFormat`.
- **Fix:** refuse unknown fields: `#[serde(deny_unknown_fields)]` on
  `Request` (check that the `op` tag itself is still accepted with the
  internally tagged representation; if not, decode into a map first and
  compare its keys with the variant's) and on the reply structs a client
  decodes.

### C1. The Step 4 and Step 5 random walks ignore `PROPTEST_CASES`

`step5_sim_tests.rs` (line 1377) and `step4_sim_tests.rs` (1386) set
`cases: 64` in their `ProptestConfig`, which overrides the environment, so
the 10,000-case gate runs them at 64 cases. The reviewer's two properties
read `PROPTEST_CASES` explicitly (`cases()` in `qa_step5_tests.rs`) and fall
back to 24 and 12 cases for the plain run.

## (b) Hazards seen but not tested

1. **A full pool with no frozen generation cannot run the CP a deletion
   needs.** `maybe_cut` cuts nothing while `pool_full` (engine.rs 3845), and
   `maybe_pump` neither runs for a pending snapshot deletion (`can_clean`
   requires `snap_deletes` to be empty) nor more than three times without a
   free. If the CP that set `pool_full` carried no generation (a pump CP, as
   in W2 once the clients stop writing), a `delete_snapshot` has no CP to
   ride until a reboot (and punches are refused while `pool_full` holds).
   W2's test reached `pool_full` through a CP with a generation, where a
   deletion does get through.
2. **`pool sync` and `pool clean` on a full pool time out instead of
   retrying.** `Pool::sync_now` and `Pool::clean` (pool.rs 525, 751) wait
   for the engine to go quiet before they call `request_cp`; with a frozen
   generation held by `pool_full` the engine never goes quiet, so they time
   out without making the request that is the engine's documented way out.
3. **A signal waits for the control request in progress.** `Daemon::run`
   reads the stop flag only between requests; `Pool::wait_for`,
   `wait_until` and `io` do not look at it, so `SIGTERM` waits up to the
   operation timeout (120 s by default) per queued request. systemd's
   default stop timeout is 90 s, after which `SIGKILL` loses a heap arena's
   unflushed writes and trips nvfail, the outcome S5-4's clean shutdown is
   for.
4. **The control endpoint accepts any TCP address.** `Endpoint::parse` and
   `Server::bind` take `tcp:0.0.0.0:7421`, exposing an unauthenticated
   control plane (which can format any path the daemon can write) beyond
   loopback. Not tested, to keep a firewall prompt off the owner's PC.
5. **One thread per connection, no read timeout, and an allocation of the
   announced frame length before the body arrives** (`read_frame`, up to
   64 MiB): N idle connections pin N threads and N × 64 MiB.
6. **Catalogue scale.** `check_catalog` is quadratic in the names and runs
   in every CP that changes a name and at every boot (1,024 volumes × 271
   snapshots is 277,000 records and about 4 × 10¹⁰ comparisons); the
   per-CP catalogue rebuild in `prepare_meta_inner` is O(snapshots ×
   catalogue) in every CP; once the region outgrows a segment (4,096
   records in a 2 MiB segment) it needs a run of adjacent free segments,
   which a fragmented pool may not have (every name-changing CP then fails
   for space). The snapshot table is likewise rewritten whole in every CP.
7. **A deletion next to a damaged snapshot.** The deletion's `newer`
   neighbour is `…and_then(|s| s.pinned.clone())` (engine.rs 4753): a
   damaged newer snapshot is skipped and the entries are judged against the
   live map, releasing blocks the damaged snapshot may still reference
   (its warning says it "still pins its data"). `audit_liveness` skips
   damaged snapshots too, so the drift would not show.
8. **Relocating a block that already fails its CRC** (copied as is, under
   the recorded CRC) makes the relocating CP look torn at an unclean boot
   under a trusted opaque barrier, because the boot checks every listed
   stripe's blocks against their descriptors (boot.rs, `check_data`): the
   boot rewinds past a sound CP. Ingest stripes with arena damage share it.
9. **Rewritten snapshot roots are not verified as the CP's own.** The
   directory's `pages` lists the map pages a relocation rewrote, not the new
   index region or root page; `walk_cp` tears a candidate only for a
   snapshot whose `birth_cp` is the candidate's, so under a trusted opaque
   barrier a lost rewritten root page reads as a damaged snapshot instead of
   a torn CP.
10. **`snapshot create` in the daemon matches any snapshot of the volume.**
    `Pool::snapshot_create` waits for `SnapshotCreated { vol }` without the
    uuid; after an earlier request timed out and landed late, the next
    create can take that event and then answer "committed but not listed".
11. **No upper bound on the arena size.** `check_arena_bytes` has none and
    `HeapBuffer::new` zero-fills the buffer, so an absurd `--arena-bytes` on
    an overcommitting Linux host is killed by the OOM killer rather than
    refused.
12. **Timeouts.** A `--delete-newer` rollback runs one CP per deletion, each
    allowed the daemon's 120 s, under the client's single 180 s reply
    timeout.

## (c) Behaviour pinned as right (passing tests)

- Pins: a pending replication-base change counts at once; a base moved and
  the old base deleted in one CP land together and survive a daemon crash
  (`pending_pin_changes_govern_deletion`).
- Rollbacks, deletions, snapshot requests and onlining refuse each other
  while one waits (`rollbacks_and_deletions_refuse_each_other_while_waiting`).
- Three deletions in one CP, in any request order, release each version
  once; exact after a power cut (`several_deletions_in_one_cp_release_each_version_once`).
- Writes and punches landing while a cleaning CP stages win over the
  relocation, across every stop kind
  (`writes_and_punches_during_a_cleaning_cp_win_over_the_relocation`).
- A power cut in each phase of a deleting CP after a clean that rewrote the
  snapshot, under three tear policies
  (`a_power_cut_in_each_phase_of_a_deleting_cp_after_a_clean_is_consistent`).
- The engine boots from the B slots with both A slots wiped
  (`the_engine_boots_from_the_b_slots_when_both_a_slots_are_wiped`).
- **Property (a), `gc_and_crash_invariance`:** both volumes filled, then
  overwrite bursts, punches, snapshots, deletions, cleans, and cuts (power
  cuts under all four tear policies, daemon kills, clean stops) driven into
  a chosen phase of a cleaning CP or a snapshot-deleting CP with writes in
  flight. After every recovery: the image is a prefix the stop allows,
  nvfail holds what it must, every snapshot reads as taken, no committed
  deletion comes back (tracked by uuid), the live-block accounting matches a
  walk of every tree, and no CP fails for space. A trace run
  (`QA_TRACE=1`) over 100 cases cut relocations in flight in every phase
  (Staging 22, Metadata 19, Flushing 24, CommitA 30, CommitB 22) and
  deletions likewise. Requests are made with the engine idle, so the walk
  does not trip W3 (its own test).
- **Property (b), `a_full_pool_recovers`:** a pool filled until the
  admission rule refuses even after a CP, with up to four snapshots, then
  random deletions, punches, cleans, overwrites, snapshots and stops; every
  requested CP commits, no CP fails for space and the pool is never marked
  full; after deleting the snapshots, punching half the volume and cleaning,
  writes are admitted again, before and after a reboot. Deletions are asked
  with the engine idle (this walk found W3 before it did).
- Daemon: concurrent clients served in turn with the right answers; dropping
  every handle stops the pool cleanly; malformed frames are answered
  `BadRequest` or end the connection while the daemon serves on; names at
  255 bytes, Unicode names and per-volume snapshot names survive a restart;
  I/O at the edges (LBA near `u64::MAX`, zero blocks, partial blocks) is
  answered, never crashed.
- Protocol: frames survive a stream that splits every byte; hostile bodies
  (empty, not UTF-8, 10,000-deep nesting, out-of-range numbers) are decode
  errors; the 64 MiB bound holds and the largest `IoWrite` fits under it.

## (d) Commands and results

Every command runs in Git Bash from the repository root after
`export PATH="$HOME/.cargo/bin:$HOME/.cargo/mingw-tools/bin:$PATH"`.

```
cargo test -j 2 -p otap-sim --test qa_step5_tests -- --test-threads=4
cargo test -j 2 -p otap-daemon --test qa_daemon_tests -- --test-threads=4
cargo test -j 2 -p otap-control --test qa_protocol_tests -- --test-threads=4
PROPTEST_CASES=1000 cargo test -j 2 -p otap-sim --test qa_step5_tests -- --test-threads=4 gc_and_crash_invariance a_full_pool_recovers
cargo clippy -j 2 -p otap-sim --test qa_step5_tests -- -D warnings
cargo clippy -j 2 -p otap-daemon --test qa_daemon_tests -- -D warnings
cargo clippy -j 2 -p otap-control --test qa_protocol_tests -- -D warnings
```

| File | Passed | Failed (all `bug_*`) | Time |
|---|---|---|---|
| `qa_step5_tests.rs` (default cases: 24 and 12) | 8 | 10 | 6.4 s |
| `qa_daemon_tests.rs` | 5 | 4 | 3.4 s |
| `qa_protocol_tests.rs` | 4 | 1 | 0.1 s |

The two properties at `PROPTEST_CASES=1000` (the fourth command): both
pass, 267.6 s for the two together (they run on two test threads). At that
rate 10,000 cases take about 40 minutes for `gc_and_crash_invariance` and
45 for `a_full_pool_recovers`, past the 30-minute bound of the brief to the
reviewer, so the 10,000-case gate is left to the lead's run; with
`--test-threads=4` the two run side by side, about 45 minutes in all.
Earlier runs during the review: 100 cases of (a) with `QA_TRACE=1` (the
coverage figures above) and 300 cases of (b) (80 s), all passing once the
walks asked for deletions with the engine idle.

All three test targets are clippy-clean under `-D warnings` and
rustfmt-clean.
