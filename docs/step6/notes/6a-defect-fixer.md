# Step 6A: the five candidate defects (Opus-Defect-Fixer, 2026-10-16)

Base `cfeb37f`; candidates 1-5 of `../README.md` section 2. All five were
reproduced before the fix (E39) and all five were real. **No format, layout
or wire change**: format 2.3, arena layout 1.2, control protocol 1; no
vectors touched. New tests in `crates/*/tests/step6a_*_tests.rs`; one
existing property model changed (item 2). `07`/`08` entries are the lead's.

## 1. Dirty-boot hold lost across a second crash: confirmed

- **Cause.** The hold became durable only with the boot commit's slot
  (volume-table `nvfail_cause` rows, `sb.nvfail`), but `Engine::boot` had
  already written the arena: the attach rewrote damaged/filled/reserved
  slots `Committed`, `Arena::create` formatted a blank or lost arena. A kill,
  `Pool::boot` timeout or failed boot commit before `A` landed left the next
  boot a whole arena and no cause to hold. Reproduced for all three causes
  (damaged slot, blank arena after a power cut, damaged header).
- **Fix.** The slot already carries nvfail; the defect was the order.
  `otap-arena`: `attach_deferred`, `create_deferred` (write nothing),
  `apply_pending_writes`, `has_pending_writes`, `release_after_attach`
  (replay releases join the pending writes); `reserve` answers the new
  `ArenaError::Pending` meanwhile. `Engine::boot` uses them, defers the
  replay's orphan/superseded/unreplayable releases too, and `step_commit`
  applies them when the boot commit's `A` slot completes. A stopped boot
  leaves the memory as found, so any number of kills keeps the hold.
- **Tests.** `step6a_sim_tests`: `a_boot_killed_before_its_commit_keeps_the_hold_a_{damaged_slot,blank_arena,lost_arena}_calls_for`,
  `consecutive_kills_during_the_boot_never_lose_the_hold`,
  `kills_during_the_boot_of_a_whole_arena_lose_nothing_and_hold_nothing`;
  `step6a_arena_tests`: `a_deferred_{attach,create}_writes_nothing_until_it_is_applied`,
  `a_release_during_a_deferred_attach_waits_for_the_apply`.
- **Decide/record.** Deferral, not the map's "pending hold" header word (no
  layout change; the evidence itself survives). Applied at `A`, not at
  finalize; a kill between `A` and the apply just holds again (harmless).

## 2. Kill inside a multi-block fill stalls the ring: confirmed

- **Cause.** A kill between a slot's field stores and its word left a
  `Free` slot under the reservation's stale seal that the attach neither
  released nor reported; behind a kept `Placed` slot the tail stopped there
  (probe: ring `Full` at write 125 of the first lap).
- **Fix (run continuity).** A stale-sealed `Free` slot is judged by its
  neighbours of the same write (seq, vol, kind, lap, adjacent LBA): a
  `Placed` one before it or any non-`Free` one after it means the fill went
  past it, so it is damage (`Damage::Descriptor`, the boot holds; this case
  was silent loss before); otherwise an interrupted fill, released
  `Committed` on its lap when below the head, never noted (an unverified lap
  must not move the head): `Reattach::interrupted_fills`. At or past the
  head it waits for `reserve`. Also: a `Placed` slot whose write still has a
  verifying `Filled` slot (kill inside `place`) is dropped with the whole
  write (`Reattach::torn_places`); the torn prefix of a never-acknowledged
  write used to be replayed (the map's open question 8).
- **Tests.** `step6a_arena_tests`: `a_fill_killed_{between_a_slots_fields_and_its_word,on_its_first_slot}_does_not_stall_the_ring`,
  `a_fill_killed_on_its_last_slot_is_left_to_the_next_reservation`,
  `a_placed_word_damaged_to_read_free_is_reported_as_damage`,
  `a_place_killed_midway_drops_the_whole_write`. The arena walk in
  `adversarial_arena_tests` gained `KillInsideFill`; its `KillMidPlace`
  model now expects the prefix dropped (run at 3,000 cases).
- **Decide/record.** Dropping a torn place changes replay semantics (both
  are within the contract).

## 3. A snapshot's damaged map page wedges the pool: confirmed, worse

- **Cause.** The deletion walk's `pinned_diff(...)?` (the boot checks only a
  snapshot's root page and index): in a plain CP the CRC error escaped
  `tick()` every tick; in a fold CP it would set `fold_damaged` and stop
  folds pool-wide. No CP of any volume committed again.
- **Fix.** `MapSet::pinned_diff_tolerant` skips and returns damaged pages.
  `Engine::deletion_walk` releases the readable pages' blocks, keeps the
  damaged pages' blocks counted (the safe direction, as E36), and the
  deletion commits; a read error defers that one deletion to the next CP
  instead of failing it. **Quarantine** (memory only): `SnapState.damage`,
  `OperatorWarning::SnapshotDamaged` (pool status, `damaged` in the
  snapshot list), reads, diffs and new rollbacks refused, still deletable
  and still pinning. Set by the deletion (the snapshot, or a newer
  neighbour whose page fails), the cleaner's `DamagedSnapshot` skip,
  `snapshot_read`, `snapshot_diff`, `audit_liveness`. The warning now goes
  with a deleted snapshot. No pool verify path exists yet.
- **Tests.** `step6a_sim_tests`: `deleting_a_snapshot_with_a_damaged_map_page_does_not_wedge_the_pool`
  (own page, and a page shared with the live map),
  `a_snapshot_found_damaged_is_quarantined_and_the_pool_goes_on`.
- **Decide/record.** No `CORRUPT` bit in `SnapRecord.flags` (a 2.4 change
  with the README's forward hazard; the next walk re-finds the damage). A
  rollback accepted before the quarantine still runs (E32 contains the
  page); dropping it would make `finalize` report a rollback that did not
  happen.

## 4. `START_DEV` on the serving thread: confirmed, one link unverified

- **Cause (saved 6.12 `ublk_drv.c`).** The fetch wait is met (`create`
  submits the fetches); but for a privileged server (`CAP_SYS_ADMIN` at the
  last fetch, :1630) partition scanning stays on (:2268) before `add_disk`
  (:2281); control commands run in io-wq (:2904); the scan's read reaches
  the server as task work on the fetching task, the engine thread, blocked
  in `execute`. Unverified (genhd.c not saved): `add_disk` reads the
  partition table synchronously. The mock reproduces the ordering deadlock.
- **Fix.** `UblkControlRing::begin`/`poll` (bufferless, not waited for;
  `execute` refuses meanwhile; unsafe inventory 14). `UblkDevice::begin_start`/
  `poll_start` replace `start`. `otap_ublk::start` (`DeviceControl`,
  `start_while_serving`, `StartError`): fetches submitted, front door in
  the engine, `START_DEV` begun, ticks with polls between. Daemon:
  `create_device` no longer starts; `Pool::start_device` (op timeout, stop
  flag); `Pool::abandon_device` drops the front door before `DEL_DEV`,
  fixing the map's other defect (delete with `/dev/ublkcN` still open).
- **Tests.** `step6a_ublk_tests` over the new `MockStart` (models the linked
  queue, then a read of LBA 0 that must be answered):
  `start_dev_completes_while_its_queue_is_served`,
  `start_dev_waited_for_on_the_serving_thread_never_completes`, and five
  edges (no queue, failed start, serve failure, all tags busy, double
  start). The Linux code is type-checked and clippy-clean only.
- **Open.** After a timeout `abandon_device` waits up to 5 s for the start,
  then logs a device left to the operator; unmeasured on Linux.

## 5. A larger `--arena-bytes` recreates the arena: confirmed

- **Cause.** `map_file` grew the file, `verify` then failed `BadGeometry`,
  and the boot recreated the arena, destroying placed writes; `can_attach`
  also checked geometry before aggregate, so a foreign arena file was
  formatted over. Both reproduced on the original engine.
- **Fix.** `map_file` refuses a length change of a non-empty file;
  `map_file_resizing` is explicit. `can_attach`: identity, aggregate, then
  geometry. `Arena::survey` reads an arena under its recorded geometry
  untouched (`recorded_bytes`, `placed`, including slots sealed as placed).
  One rule, `otap_cp::check_arena_resize`: resize only when nothing is
  placed and the pool's CP is a clean shutdown with nothing rewound
  (`BootState::is_clean`), or on discard; else `EngineError::ArenaResize`,
  memory untouched. Daemon `open_pool_arena` maps a mismatched file at its
  own length, judges it (`check_resize`: never another pool's arena,
  non-arena memory freely, `boot::evaluate` for clean), then resizes.
  `dbladed --discard-arena` (needs `--device`; `PoolBoot` never discards):
  recreated, `arena_lost`, held.
- **Tests.** `step6a_sim_tests`: `a_larger_arena_over_placed_writes_is_refused_not_recreated`,
  `a_larger_arena_after_an_unclean_stop_is_refused`, `a_larger_arena_after_a_clean_shutdown_is_recreated`,
  `a_discarded_arena_is_recreated_and_its_volumes_held`, `another_pools_arena_under_another_size_is_refused`;
  four survey tests in `step6a_arena_tests`; nine in `step6a_daemon_tests`.
- **Decide/record.** "Dirty" is the pool's (superblock), so an empty arena
  of an uncleanly stopped pool is refused too (owner's rule). New CLI flag,
  no protocol field (that would be a wire change).

## The sixth candidate (late commits), noticed only

The start loop pays it: the scan's commit is pushed in one tick and
submitted at the next poll. Nothing here changes the commit cadence.

## Tooling finding

A worktree sharing the main `CARGO_TARGET_DIR` **collides** (cargo's stable
hash ignores the checkout path): its build overwrote the main artifacts and
the next main build linked them; a `touch` forced the rebuild. The README's
"leaves the main cache valid" is wrong: give a worktree its own target.
