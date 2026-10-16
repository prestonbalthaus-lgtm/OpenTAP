# Step 6 adversarial QA report (worker "Opus-QA-Gatekeeper", 2026-10-16)

One bounded adversarial review of the Step 6 diff, `cfeb37f..1637340`: the
four worker commits `217dad3` (6A, five audit defects), `8e34b4a` (6B-1,
ublk `USER_RECOVERY` re-attach, the reaper, `otapctl device`), `4f34e17`
(6B-2, format 2.4 incremental tables, `otapctl pool verify`) and `1637340`
(6C, replication over a simulated network and TCP, `otapctl replicate`,
the WSL 2 scripts). Unlike the earlier reviews, this worker also fixed what
it reproduced (the owner's thin-orchestrator mode for Step 6, `09`
section 1): every finding below that is called a defect was reproduced by
a test that failed on the code as committed, then fixed in the sources,
and the test now passes. A finding nobody reproduced is listed separately
as cited.

## How the pass was run

Six read-only reader agents (Opus 5.5) each read one area of the diff
against a list of what to attack: the 6A durability paths, the format 2.4
tables, the verification job, the replication engine, the protocol, CLI,
daemon and TCP transport, and the Linux-only ublk code with the scripts
(against the saved Linux 6.12 `ublk_drv.c` and `ublk_cmd.h`). They built
nothing and changed nothing; they returned 38 candidate findings with
file:line mechanisms and suggested reproductions. The gatekeeper then wrote
a failing test for each candidate it took up, one cargo process at a time,
fixed the code, and looked for the same pattern nearby. Two candidates the
gatekeeper found itself (D1, carried forward as the Step 5 review's hazard
8, and R1, named in the 6B-1 notes) are in the same list.

## Files created

| File | Tests | Findings |
|---|---|---|
| `crates/otap-sim/tests/review_step6_replication_sim_tests.rs` | 6 | D2, D3, D4, D5, D6, W1 |
| `crates/otap-sim/tests/review_step6_sim_tests.rs` | 6 | D1, D8, D9, R3, W3, C1 |
| `crates/otap-daemon/tests/review_step6_replication_daemon_tests.rs` | 2 | D4, D7 |
| `crates/otap-daemon/tests/review_step6_recovery_daemon_tests.rs` | 5 | D11 (two), R4, R2, the recovery-timeout decision |
| `crates/otap-arena/tests/review_step6_arena_tests.rs` | 3 | D10 (two), R5 |
| `crates/otap-ublk/tests/review_step6_ublk_tests.rs` | 2 | W2 (two) |
| `crates/otap-ctl/tests/review_step6_ctl_tests.rs` | 2 | R2, R6 |
| `crates/otap-control/tests/review_step6_control_tests.rs` | 2 | R1 (two) |

28 tests in eight files. Changed existing tests, each with a comment naming
the finding: three in `step6b_catalogue_sim_tests.rs` (D9 reverses the
tolerance they pinned; see below) and one in
`step6c_replication_sim_tests.rs` (its wait condition read the previous
sink job, so the rollback it names never happened). The Step 5 and Step 6
call sites of the field-less control variants (R1) were rewritten
mechanically.

## Verdict

Step 6 was not ready to ship as committed. The replication sink, the
newest and largest piece, held five ways to land wrong data under the right
name or to lose a mirror's protection, and the format 2.4 snapshot table
could lose every snapshot and then free its blocks after one bad sector.
The arena, the device identity and the quarantine each had one way to lose
or misdirect an acknowledged write. All eleven are fixed and pinned, with
three wedges and six robustness defects; the Linux paths changed in this
review are type-checked and clippy-clean only, like the rest of the Linux
code. The common thread of the replication defects is the one the Step 5
review named for the CP: decisions taken against different views of the
same state (a target taken from the newest handshake while the content came
from the old delta; a record trusted against a pool that rewound under it;
a volume's identity read from an index that a deletion recycles).

## Findings, by severity

Classes: **D** data loss, misdirection or exposure; **W** wedge or
liveness; **R** robustness, protocol or contract; **C** cited, not
reproduced (separate section).

### D1. A block that already failed its CRC was relocated, and an unclean boot then rewound past a sound CP

- **Test:** `review_step6_sim_tests::d1_a_block_that_fails_its_crc_is_never_relocated`
  (failed: the boot rewound, torn reason `BadCrc { region: "data block" }`
  for the relocation stripe of the cleaning CP).
- **Mechanism.** The cleaner copied a block under its recorded CRC whether
  or not the bytes still matched it (the Step 5 review's hazard 8). Under a
  trusted opaque barrier an unclean boot checks every data block of the
  newest CP's stripes, relocation stripes included, so one block of bit rot
  made the cleaning CP look torn and the boot rewound past it, losing the
  writes that CP carried (held under nvfail, but gone).
- **Fix.** `build_reloc_stripe` verifies each block against its CRC as it
  reads it; a block that fails ends the attempt like an unreadable one
  (`RelocError::Read`): its segment joins the cleaner's skip list
  (`CleanSkipped`), the CP is retried without it, and the damaged block
  stays where it was, still reading `Corrupt`.
- **Cost, recorded as a limit:** such a segment is never cleaned, and the
  skip list is cleared whenever the snapshot set changes, so each change
  costs one failed and retried cleaning CP.

### D2. A later handshake renamed the delta that had landed

- **Test:** `review_step6_replication_sim_tests::d2_a_later_handshake_never_renames_the_delta_that_landed`
  (failed: block 100 of the destination snapshot differed from the source
  snapshot it was named after).
- **Mechanism.** While a sink job was taking the destination snapshot of
  delta X, a source that had cancelled X and pushed a newer snapshot Y
  attached to the same job; `check_source` overwrote the job's target with
  Y, and `finish` created the destination snapshot under Y's uuid over X's
  content and recorded Y as the base. The source then accepted the sink's
  base Y as committed, and every later increment built on a mirror holding
  the wrong data under the right name.
- **Fix.** The destination snapshot takes the identity recorded when the
  delta began (`SinkRecord::landing_target`), never the latest handshake's;
  the newer delta waits for this one's commit.

### D3. The sink's rewind guard never fired, and the sink cleared the hold that reported the loss

- **Test:** `review_step6_replication_sim_tests::d3_a_rewound_sink_never_trusts_the_chunks_the_lost_cp_carried`
  (failed: block 754 of the destination snapshot differed).
- **Mechanism.** The sink record kept the pool's CP number and compared
  `pool_cp > cp_seq`, and only when a job was created. A boot that rewinds
  by one CP commits the boot under the lost CP's number, so the comparison
  never fired; by the time the source reconnected more CPs had committed.
  The record's bitmap still claimed chunks the lost CP had carried, the
  source skipped them, and the digest was not checked (a new job). The
  sink also cleared the `TornCp` hold on the mirror that reported the
  loss.
- **Fix.** The record carries the CP's nonce in what were reserved bytes
  (the layout stays version 1; a reader ignores reserved bytes, an older
  record reads 0, unknown); `SinkRecord::outlived_by_rewind` compares both;
  `Replicator::attach` checks every record right after the boot commit,
  when a one-CP rewind is still visible, and forgets the landing durably
  before any source can resume from it (`SinkRecord::forget_landing`).

### D4. A rollback of the mirror wiped chunks its record still called landed

- **Tests:** `review_step6_replication_sim_tests::d4_a_rollback_forgets_the_landing_before_it_wipes_it`
  (failed: block 0 of the re-pushed snapshot differed: chunks skipped as
  landed had been rolled away) and
  `review_step6_replication_daemon_tests::d4_an_operator_cannot_roll_back_or_resize_a_mirror`
  (failed: `snapshot rollback` of a mirror succeeded).
- **Mechanism.** Before landing a delta of another identity on a dirty
  mirror, the sink asked for a rollback to its base and cleared the record
  only once the rollback was durable. A job that ended in between (a
  cancel, a kill) left a record still claiming the abandoned delta's
  chunks, so a later push of that delta resumed and the source skipped
  chunks the rollback had wiped. An operator's `snapshot rollback` or
  `volume resize` of a mirror reached the engine the same way, under the
  sink.
- **Fix.** `step_reset` forgets the landing and saves the record before it
  asks for the rollback or the deletion; the daemon refuses `snapshot
  rollback` and `volume resize` of a volume marked as a mirror
  (`replicate release` first).

### D5. An existing offline volume named as a destination was deleted, not refused

- **Test:** `review_step6_replication_sim_tests::d5_an_existing_volume_is_refused_as_a_destination_not_deleted`
  (failed: the push completed; the operator's volume had been deleted and
  created anew).
- **Mechanism.** `SinkJob::load` marked every offline destination as a
  replica, which made the refusal of "a volume that is not a replication
  destination" dead code (note 6c's decision 9 did not hold). With no
  record the volume read as a dirty mirror without a base, so a full
  transfer deleted it and created it again.
- **Fix.** A destination is a mirror only when a sink record names it or
  the pool does (`is_mirror_by_pool`: its newest transfer snapshot carries
  the replication pin, which `replicate release` clears); anything else is
  refused. So that a sink killed between creating the mirror and saving
  its record still recognises it, the record is now saved as soon as the
  mirror is asked for (the replication walk found that case at once).

### D6. A mirror whose record was lost lost its protection

- **Test:** `review_step6_replication_sim_tests::d6_a_mirror_whose_record_was_lost_stays_a_mirror`
  (failed: `set_volume_online` of a mirror holding part of a delta
  succeeded). Two readers found it independently.
- **Mechanism.** The replica mark lives in memory and was set again at boot
  only from the sink records, which live in `--state-dir`, a tmpfs by
  default; a host reboot in the middle of a delta left an ordinary offline
  volume that `volume online` served as a torn mix of the base and the next
  snapshot.
- **Fix.** `Replicator::attach` also marks every offline volume the pool
  names as a mirror (`is_mirror_by_pool`). **Left as a limit:** a partial
  *first* transfer has no transfer snapshot yet, so with its record lost it
  is an ordinary offline volume; a sink's `--state-dir` should be
  persistent storage.

### D7. A sink record outlived its volume and took over the next one at the index

- **Test:** `review_step6_replication_daemon_tests::d7_a_deleted_mirrors_record_never_attaches_to_the_next_volume`
  (failed: the operator's new volume refused to come online, "a
  replication destination").
- **Mechanism.** Records are keyed by volume index, `volume delete` left the
  record, and the engine reuses the lowest free index.
- **Fix.** The daemon forgets the record when it deletes the volume
  (`Replicator::forget`).

### D8. A cleaning CP lifted a snapshot's quarantine

- **Test:** `review_step6_sim_tests::d8_a_cleaner_pass_never_lifts_a_snapshots_quarantine`
  (failed: the snapshot listed whole after a relocation). Two readers found
  it independently.
- **Mechanism.** The cleaner's rewrite of a snapshot's tree built the new
  state with `damage: None` and `finalize` copied it over, so a
  quarantined snapshot (a 6A damaged page, or a 6B-2 `SNAP_UNCERTAIN`
  record) could be read, diffed, rolled back to and replicated until the
  next reboot.
- **Fix.** The rewrite keeps the quarantine, and `finalize` keeps any damage
  found while the CP was in flight.

### D9. A snapshot table that lost records booted without them, and their blocks were freed

- **Test:** `review_step6_sim_tests::d9_a_snapshot_table_that_lost_records_refuses_the_boot`
  (failed: the pool booted with no snapshots).
- **Mechanism.** 6B-2 booted past an unreadable head written by an older CP
  ("every record is unknown") and past older regions whose records it
  could not read. One bad 4 KiB head dropped every snapshot, user pins and
  replication bases included; a record lost from a delta dropped the
  snapshot it created. The next commit folded the loss into a clean chain,
  the blocks only those snapshots held were counted dead, and the cleaner
  freed them. Format 2.3 refused the boot on any damage of the table.
- **Fix.** A snapshot table of an older CP whose head does not verify, or
  whose region yields fewer records than the head counts, tears every
  candidate that names it, so the boot is refused (as 2.3 refused it)
  rather than freeing what the lost snapshots pin. Damage that loses no
  record (a page header, padding) keeps 6B-2's tolerant path with its
  uncertainty marks, and the catalogue keeps it always (names only, W4 of
  the Step 5 review). Three tests of 6B-2 were ported: the lost head and
  the lost tombstone now expect the refusal, and the uncertain-record test
  damages padding instead so its quarantine and persistence coverage
  stays. This reverses part of E51 (recorded in E60).

### D10. A damaged placed word of a one-block write was released as an interrupted fill

- **Tests:** `review_step6_arena_tests::d10_a_damaged_placed_word_of_a_one_block_write_is_damage_not_a_fill`
  and `d10_a_damaged_placed_word_with_a_flipped_lap_is_damage_not_a_fill`
  (both failed: `interrupted_fills` 1 or a torn write kept, `damaged`
  empty).
- **Mechanism.** 6A judged a `Free` slot under a stale seal by its
  neighbours of the same write, compared under the damaged word's own lap.
  A one-block write (every 4 KiB write, every hole record) has no such
  neighbour, and a flipped lap bit defeated the comparison for any write:
  the slot was released as an interrupted fill, the acknowledged block
  never replayed, the volume not held.
- **Fix.** A stale slot is judged at the position its index takes in the
  window, never under its own lap; and for a serial writer (the engine
  fills one reservation at a time, whole, on its thread) an interrupted
  fill is always the newest reservation's, so a stale slot with anything
  written after it is damage. The arena's general contract still allows
  several outstanding reservations (its random walk drives that), so the
  serial rule is the writer's declaration: `Arena::attach_with` and
  `attach_deferred_with` take `Fills::Serial` or `Fills::Interleaved`, and
  the engine's boot attaches `Serial`. A damaged word of the newest
  write's last slot still cannot be told from a fill killed there (left as
  a limit; it needs damage to that one word).

### D11. A ublk device was re-attached to another volume

- **Tests:** `review_step6_recovery_daemon_tests::d11_a_device_never_outlives_its_volume_into_the_next_one_at_its_index`
  (failed: the deleted volume's device served the new volume) and
  `d11_two_devices_claiming_one_volume_without_a_record_are_both_left_stale`
  (failed: the first of two stamped devices was taken).
- **Mechanism.** A device's identity is its pool and volume index; the
  binding record is what tells incarnations apart. A `--no-ublk` boot
  neither loaded nor updated it, so a volume deleted there and a new one
  created at its index left the binding in place, and the next boot with
  ublk re-attached the old device, with the old volume's reissued writes,
  to the new volume. With the record gone, the first of two devices
  carrying one stamp was taken, possibly the older one holding overtaken
  writes.
- **Fix.** The record is loaded and saved on every boot; a deletion marks a
  device still bound to the volume stale; two or more unrecorded devices
  claiming one volume are all left stale and the volume gets a new device.

### W1. A target snapshot held by another volume of the sink pool wedged both jobs

- **Test:** `review_step6_replication_sim_tests::w1_a_target_held_by_another_volume_of_the_sink_pool_is_refused`
  (failed: the push never ended within 100,000 steps).
- **Mechanism.** Snapshot uuids are unique per pool; `create_snapshot_as`
  answered `SnapshotExists` for ever, the sink read it as "asked" and
  retried, the source waited for `Committed` kept alive by the sink's
  pings.
- **Fix.** Refused at the handshake (`snapshot_elsewhere`), after the
  volume checks; a `SnapshotExists` naming another volume at snapshot time
  fails the job.

### W2. A device live under a reused pid was refused, or handed to `STOP_DEV`

- **Tests:** `review_step6_ublk_tests::w2_a_device_live_under_a_reused_pid_is_waited_for_and_re_attached`
  (failed: "this process serves it already") and
  `w2_a_device_live_under_this_pid_that_this_process_does_not_serve_is_never_reaped`.
- **Mechanism.** A restarted `dbladed` may get the dead one's pid (PID 1 of
  a container). The recovery refused such a `LIVE` device at once, so the
  volume lost its device number whatever the timeout; `classify` called it
  stale with its server alive, so `reap` passed it to `STOP_DEV`, which
  waits on the engine thread for the requests the dead server held.
- **Fix.** A device naming this process's pid that this process does not
  serve is the earlier process's, and its server is dead: the recovery
  waits for it, and while `LIVE` it is quiescing and never reaped.
  `MockKernel::process_with_pid` models the reused pid.

### W3. A clean resize killed after the boot commit's `A` slot could not boot at either size

- **Test:** `review_step6_sim_tests::w3_a_clean_resize_killed_after_the_boot_commit_landed_still_boots`
  (failed: `ArenaResize`, "the pool did not shut down cleanly").
- **Mechanism.** 6A defers the new arena's format to the boot commit's `A`
  slot even when there is nothing to hold; a kill after `A` left the old
  header at the new size, and `A` (a running slot) made the pool unclean,
  so the resize was refused, and in the daemon (whose file was already
  resized) the old size too: only `--discard-arena` booted it, holding
  every volume for nothing.
- **Fix.** A resize of a clean pool's empty arena is written at once.

### R1. Field-less requests and the `Ok` reply accepted unknown fields

- **Tests:** `review_step6_control_tests::r1_a_request_without_fields_refuses_an_unknown_field`,
  `r1_the_ok_reply_refuses_an_unknown_field` (both failed: `{"op":"ping","force":true}`
  decoded).
- **Mechanism.** serde lets an internally tagged *unit* variant ignore the
  rest of the map, whatever `deny_unknown_fields` says: `Ping`,
  `PoolStatus`, `PoolSync`, `PoolClean`, `PoolStop`, `DaemonStop`,
  `VolumeList` and the reply `Ok` (named in the 6B-1 notes; the protocol
  reader confirmed these eight were all).
- **Fix.** Empty struct variants, as `DeviceList {}` already was; the JSON
  on the wire is unchanged.

### R2. A switch given a value was turned on, whatever the value

- **Tests:** `review_step6_ctl_tests::r2_a_switch_given_a_value_is_refused_not_turned_on`
  (failed: `device reap --stale=false` parsed as `stale: true`) and
  `review_step6_recovery_daemon_tests::r2_a_dbladed_switch_given_a_value_is_refused`
  (failed: `--heap-arena=no` accepted).
- **Mechanism.** `--name=value` was stored and the switch read as present:
  `pool format --force=no` overwrote a pool, `--delete-newer=false` deleted
  the newer snapshots, `--discard-arena=false` discarded the arena.
- **Fix.** Both parsers refuse a value on a switch.

### R3. A full pool could not be verified

- **Test:** `review_step6_sim_tests::r3_a_full_pool_can_still_be_verified`
  (failed: aborted at once, "the verification holds deferred frees").
- **Fix.** The full pool aborts a verification only when it holds
  something, as the tight pool already did.

### R4. One unprivileged ublk device of other software refused every pool boot

- **Test:** `review_step6_recovery_daemon_tests::r4_an_unprivileged_device_of_other_software_never_refuses_a_boot`
  (failed: "cannot list the host's ublk devices: GET_DEV_INFO (errno 22)").
- **Mechanism.** The 6.12 driver refuses `GET_DEV_INFO` without the
  character device's path for a device made with
  `UBLK_F_UNPRIVILEGED_DEV` (`ublk_ctrl_uring_cmd_permission`, checked in
  the saved source), and `LinuxHost::list` failed the whole listing on one
  refusal; the mock never refused, so no test could see it.
- **Fix.** One listing policy for both hosts (`host::listing`): a device
  that answers `EINVAL` is left out (OpenTAP makes only privileged devices,
  so it is never one of them); the mock now answers as the kernel does
  (`MockKernel::set_unprivileged`). The Linux half is type-checked only.

### R5. A rewritten damaged slot could move the next attach's head by laps

- **Test:** `review_step6_arena_tests::r5_a_rewritten_damaged_slot_never_moves_the_next_attachs_head`
  (failed: the second attach refused the arena as inconsistent, which makes
  the engine recreate it and lose every unflushed write).
- **Mechanism (older than Step 6).** A slot whose word did not verify was
  rewritten `Committed` under the lap the damaged word claimed, and the
  rewrite sealed it as verified.
- **Fix.** Such a slot is rewritten under the lap its position implies.

### R6. Peer text reached the operator's terminal unescaped

- **Test:** `review_step6_ctl_tests::r6_peer_text_in_a_replication_report_reaches_the_terminal_escaped`
  (failed: escape sequences in the output).
- **Mechanism.** A replication report carries a refusal's text and the
  handshake's names from an unauthenticated peer.
- **Fix.** `otapctl` escapes control characters in every peer-derived field
  of the replication renderers.

## Decisions the review took

- **`--recovery-timeout` defaults to 35 s, not 5 s.** From the saved 6.12
  driver: a server that died with a fetch pending is quiesced at once; one
  that died holding every tag is quiesced only by `ublk_timeout`, at the
  block layer's request timeout, and the driver sets none of its own, so
  blk-mq's default of 30 s applies (the default value itself is from
  memory: `blk-mq.c` is not among the saved sources). There is no
  `ublk_daemon_monitor_work` in 6.12. Every tag held is likeliest exactly
  when the engine is back-pressured under a deep queue, when a crash is
  also likely; at 5 s such a volume always lost its device number, which
  breaks every mount and open file. Only that case waits, and devices
  already re-attached are served meanwhile. Pinned by
  `the_recovery_timeout_outlasts_the_block_layers_request_timeout`;
  unmeasured until a Linux run. Writing
  `/sys/block/ublkbN/queue/io_timeout` after `START_DEV` would shorten the
  wait; `blk-sysfs.c` is not saved, so it is left for the WSL run.
- **D9 reverses E51's tolerance for lost snapshot records** (E60).
- **D10 makes the writer's fill discipline part of the arena's API**
  (`Fills`, E60).

## Test corrections

- `step6c_replication_sim_tests::a_delta_to_another_target_rolls_the_mirror_back_first`
  waited for `sink_landed() >= 20`, which read the previous, finished sink
  job (64 chunks landed), so the push was cancelled before it connected
  and the mirror was never dirty: the rollback the test names never
  happened. It now waits for the new job's landings, and the rollback runs.

## Cited, not reproduced

Recorded as cited and untested; none is counted as a defect fixed.

- **C1.** A verification that gives way to a full pool releases its frees
  without asking for a CP, so a pool whose failed CP kept its generation
  frozen would refuse writes until a request. Driven to exactly that abort
  (`c1_a_verification_that_gives_way_to_a_full_pool_lets_it_recover`), the
  pool went on by itself; the test pins what was seen, and no code changed.
- **C2.** The verification dedupes block references by address before it
  checks the CRC and the descriptor, so a cross-linked entry (one LBA's
  entry naming another's block) would go unreported. No fault hook can
  make one; a hook like `skew_live_blocks` and a comparison of every revisit
  with the first reference would close it.
- **C3.** `otapctl pool verify` follows whichever verification is current:
  a second client's run started within one poll would be reported as the
  first's. The status carries no job identity.
- **C4.** A peer that reaches the replication port can pin memory and
  threads: each connection queues 64 messages of up to 8 MiB, the sink's
  inbox is bounded in frames rather than credits before `Apply`, and
  nothing caps the connections. Loopback-only by default.
- **C5.** The delta's identity leaves out the chunk size; two source
  processes configured differently would disagree on chunk boundaries for
  one identity. `chunk_bytes` is not exposed by `dbladed`.
- **C6.** The re-attach's rewrites apply the dropped `Filled` slots before
  a torn place's prefix; a kill between them would bring the torn-place
  replay back. Needs a partial apply to test.
- **C7.** `proc_alive` counts a process still in `do_exit` (not yet a
  zombie) as alive; reading `PF_EXITING` from `/proc/<pid>/stat` would
  match the driver. Linux-only.
- **C8.** With no pool booted, a stamped device of another pool that was
  never started is stale, so `reap --stale` could race another daemon's
  creation of it. Needs two daemons on one host.
- **C9.** A forced fold of a table (a chain of 124 deltas, a repair, no
  head) that cannot be placed marks the pool full, and every retry plans
  the same fold. The policy fold's put-off can drive a chain there on a
  pool whose free segments are never adjacent.
- **C10.** `scan_table_base` checks no aggregate, table or CP, and
  `scan_table_delta` no nonce: stale record bytes in a partly written
  region could pass. Needs a lost write.
- **C11.** A persistent read error on a page of a snapshot being deleted
  defers the deletion every CP, and no cleaning is planned while a deletion
  waits: the cleaner stops until a reboot.
- **C12.** A 6A quarantine (a damaged map page) lives in memory: after a
  reboot a rollback to that snapshot is accepted (it reads no pages), the
  volume reads `Corrupt` over that range, and the first fold that rewrites
  the page stops folds pool-wide (E32).
- **C13.** A request also decodes from a JSON array (serde's tagged-content
  visitor); `["ping"]` is accepted.
- **C14.** `otapctl`'s usage says exit 4 covers a waited verification that
  did not finish; a poll that fails (the pool stopped, the job pruned)
  exits 1 or 3.
- **C15.** A Step 5 binary (format 2.3) boots a 2.4 pool that has no table
  head (no names, no snapshots) and rewrites it as 2.3; and when the first
  name or snapshot is created, the other parity's slot has no head, so a
  2.3 binary tears the newest CP and boots the one before it, silently
  rewinding an acknowledged CP. No superblock field exists that 2.3
  refuses and 2.4 accepts; always keeping a snapshot head (empty, written
  by `format` into CP 0) would make every 2.4 slot fail 2.3's table parse.
- **C16.** `scripts/linux-smoke.sh`: under `set -o pipefail`,
  `ctl ... | grep -q` can fail when grep exits at its first match and
  `otapctl` meets `EPIPE` (rewritten to read the output first); the
  SIGKILL-under-I/O step passed without testing anything if `dd` finished
  before the kill (it now says so) and never exercises the every-tag-held
  path; a failed format leaves its daemon running, `stop_daemon` waits
  without escalation, and a failed run leaves stamped devices behind.
  Fixed by reading where noted; `bash -n` only, never run.
- **C17.** `scripts/wsl2-prep.sh` checks after `olddefconfig` that `UBLK`
  and `DM_LOG_WRITES` survived, but `DM_FLAKEY` and `DM_DELAY` can drop
  silently (the probe only warns).

## Checked and found sound

The readers' negative results, which bound what this pass covered: no
path reserves or serves before the deferred arena writes apply, and the
nvfail hold survives any number of kills; every free in the engine goes
through the verification's holds, the view capture sees exactly the
captured CP, and the job ends without leaking holds; torn and older table
regions are told apart soundly and the deferred-free rule holds for folds;
the link framing refuses an oversize length before allocating, and
truncation, CRC, version, name and refusal bounds hold; chunk fields are
checked before use and credit counters cannot underflow; the loopback rule
checks the resolved address on both ends; the binding record's codec
bounds every count before allocating; the 6.12 opcodes, structure offsets,
recovery order and error rules match the saved driver; every `dbladed` and
`otapctl` flag the scripts use exists.
