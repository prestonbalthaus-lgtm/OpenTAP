# Map: the write cache (arena), the dirty index, and the replay after a daemon crash

Reader assignment: scope (A), the reconciliation of in-flight ublk requests after a
`kill -9`. Independent reading of the code and the named documents (2026-10-15).
Paths are relative to `E:/Coding/OpenTAP` unless they start with `E:/`. Line numbers
were taken from the files as they stand on 2026-10-15.

Citation shorthand: `engine.rs:N` is `crates/otap-cp/src/engine.rs`; `traits.rs:N` is `crates/otap-cp/src/traits.rs`; `boot.rs:N` is `crates/otap-cp/src/boot.rs`; `arena:N` and `arena/src/lib.rs:N` are `crates/otap-arena/src/lib.rs`; `index:N` and `index/src/lib.rs:N` are `crates/otap-index/src/lib.rs`; `device.rs:N`, `mock.rs:N` and `otap-ublk/src/frontdoor.rs:N` are under `crates/otap-ublk/src/` (in section 2.4 a bare `frontdoor.rs:N` is that crate too); `pool.rs:N` and `daemon/src/*.rs:N` are under `crates/otap-daemon/src/`; `hugetlb.rs:N` and `otap-sys/src/ublk.rs:N` are under `crates/otap-sys/src/`; `harness.rs:N`, `model.rs:N` and `sim/src/frontdoor.rs:N` are under `crates/otap-sim/src/`; test files are named with their `crates/<crate>/tests/` path once per paragraph; `ublk_drv.c:N` and `ublk_cmd.h:N` are the saved kernel sources under `OpenTAP-local/kernel-6.12/`; `docs/07:N` is `docs/07-engineering-decisions.md` (likewise `docs/06`, `docs/09`, `docs/10`). A bare `:N` continues the file last named in the same table row, list item or paragraph. Before any file is named there, the default is the first file the subsection heading names, in its tables, lists and paragraphs alike; in section 3, whose groups have no heading, it is the group's file (`arena` for A1-A10, `index` for I1-I5, `engine.rs` for E1-E11, `ublk_drv.c` for K1-K5); and in a table whose heading names no file (sections 2.4 and 2.6) it is the file the row above cites last. A decision number before a citation does not change its file: a citation of a decision's text always names `docs/07`.

Short form of the answer the lead asked for (details in sections 4, 5 and 7):

- `Placed` is set **before** `FrontDoor::complete`: `serve_write` places at
  `crates/otap-cp/src/engine.rs:3543` and acknowledges at `:3550` (which reaches
  `front.complete` at `:3627`, or later via pacing `:2142`, an intent `:3767`, or a
  CP `:5920`). Same for punches (`:3594` then `:3600`). So after a `kill -9`:
  **acknowledged and not yet carried by a committed CP ⊆ Placed** (block by block: a
  block that a later write superseded inside the active generation was released at
  once, `:3546-3548`, and the later write's block stands in for it). An acknowledged
  write that a committed CP carried is on disk and, once `finalize` released its
  slots (`:5873-5882`), `Committed` and not replayed; killed after that CP's `A` slot
  landed (from then on a boot selects that CP) and before the release, which waits for
  `B` (`:5580-5582`), it is on disk **and** `Placed`, so it is replayed and written
  again by a later CP under a new birth (pitfall 23). The Placed set may also hold
  writes that were never acknowledged (killed between place and complete, parked in
  `paced` or `pending_acks`, or whose commit SQE was pushed but never submitted to the
  kernel, which under back-pressure can last many ticks: pitfall 2). The replayed
  writes reach the disk at the CP of a later cut, not at the boot CP, which carries
  no generation (`:1713`; section 4.4 step 8).
- `Filled` but not `Placed` = killed after `Arena::fill` began and before
  `Arena::place` finished (`engine.rs:3526` to `:3543`). Both store one word per slot,
  in slot order (`fill`, `crates/otap-arena/src/lib.rs:1105-1113`; `place` through
  `transition`, `:1231-1233`), so a kill leaves one of three shapes: between the two
  calls, that is during `Index::insert` (`engine.rs:3527-3542`), the whole run
  `Filled`; inside `fill`, a `Filled` prefix and the rest `Free` (5.0 table row 5);
  inside `place`, a `Placed` prefix and a `Filled` suffix. A copy in progress is not
  `Filled` at all: the data is copied into `Free` (reserved) slots
  (`engine.rs:3507-3510`) and `fill` runs only after the CRCs are computed
  (`:3517-3524`). At re-attach every `Filled` slot and every reservation is released,
  never replayed (`arena:835-848` Filled, counted in `Reattach::dropped_filled`;
  `:816-821` reservation). But in the third shape the `Placed` prefix is kept and
  `runs_of` replays it as a shorter extent: part of a write that was never
  acknowledged lands (a torn write, which the contract allows, since `place` precedes
  every acknowledgement; with `REISSUE` the whole write is re-placed over it: 5.0
  point 6, pitfall 8, open question 8). One gap: a kill inside `fill` between a slot's
  field stores and its word store (`arena:1108-1112`) leaves a `Free` slot under a
  stale seal that the attach neither notes nor rewrites (`:816-833`); it is not
  replayed either, but when it is not the run's last slot and the attach keeps a
  `Placed` slot before it, it pins the tail and stalls every write within a lap
  (pitfall 21: an arena fix to propose, no layout change).
- For every replayed run the engine knows `(vol, lba, blocks, seq)` from
  `runs_of` (`engine.rs:6143-6169`) and the per-block CRC from
  `Arena::descriptor(slot).crc32c` (`arena/src/lib.rs:1190-1195`; `PlacedSlot.crc32c`
  at `:468-469` carries it too, but `DirtyExtent` does not). But `blocks` is the length
  of one contiguous piece (same vol and seq, consecutive lba and slot on one lap,
  `engine.rs:6155-6160`), not of the request: a prefix left by a kill inside `place`,
  or one of several pieces with one `seq` that a later overwrite split
  (`index:573-574`). Nothing records the request's own extent, whether it was
  acknowledged, or its tag (pitfall 3).
- Re-placing a reissued write is semantically safe because a client-ordered later
  write cannot exist (the completion never reached the client), so whatever it
  overwrites was ordered before it or concurrent with it. Where its replayed copy is
  the newest placement, re-placing writes the request's bytes again over that copy
  (same `(vol, lba)`, later-wins): as a rule the same bytes, but not by any guarantee,
  since ublk does not ask for stable pages, so a page the client changed while the
  write was in flight reaches D2 with newer bytes (5.0 point 1); either content is
  permitted, the write never having completed. Where D1 placed an overlapping write
  after it (a higher sequence, which proves the two concurrent, since placement
  precedes completion), the replay keeps that write and re-placing changes those
  blocks to the reissued write's bytes: the other outcome the contract permits for two
  concurrent writes, correct because they were concurrent, not because the bytes are
  the same. Where D1 placed the overlapping write first, the order is unchanged and
  only point 1's newer bytes can differ. A dedupe is cheap (`Index::lookup` per
  block plus a descriptor CRC and a 4 KiB compare) but unnecessary for correctness.
  Nothing in the arena can carry a request identity without a layout 1.3 change, and a
  `(dev, tag, generation)` stamp would be useless anyway: the kernel re-dispatches with
  no marker and the tag is not guaranteed to be the same; the right place for a
  device-to-volume binding is `ublksrv_ctrl_dev_info.ublksrv_flags`, which the kernel
  stores verbatim and never reads. Nothing writes it today
  (`crates/otap-ublk/src/device.rs:90-97` leaves it 0), so no existing device can be
  bound to its volume that way (pitfall 1).
- Reads and flushes reissued are trivially safe; a reissued discard or write of zeros is
  safe for the same reason as a write.
- The kernel (6.12) with `UBLK_F_USER_RECOVERY` alone **fails** every request the dead
  daemon had been handed and had not committed (`OWNED_BY_SRV` set, `ACTIVE` clear)
  with `EIO` (`ublk_abort_queue`, `ublk_drv.c:1387-1408` → `__ublk_fail_req`,
  `:1061-1070` → `:999-1001`); only `UBLK_F_USER_RECOVERY_REISSUE` requeues them
  (`:1066-1067`) for re-dispatch after `END_USER_RECOVERY` (`ublk_ctrl_end_recovery`,
  `:2738-2774`; the kick at `:2765-2769`). A request the block layer had dispatched
  but the dead daemon was never handed (its task work found the task exiting,
  `:1121-1123`, or it arrived while `canceling`, `:1274-1277`) is requeued under
  `USER_RECOVERY` alone (`__ublk_abort_rq`, `:1090-1098`) and delivered after `END`
  with or without `REISSUE`. The abort runs at the cancel of a still-pending fetch
  when the process exits (`:1464-1497`); when every tag was owned by the daemon
  nothing is cancelable and it waits for the block layer's request timeout
  (`ublk_timeout`, `:1209-1248`; 30 s by default), during which `START_USER_RECOVERY`
  (`ublk_ctrl_start_recovery`, `:2694-2736`) answers `EBUSY` (`:2719-2722`; section
  4.7, pitfall 6). Today's devices are created without `REISSUE`
  (`crates/otap-ublk/src/device.rs:95`). A second crash during the recovery, after
  `START_USER_RECOVERY` and before the last `FETCH_REQ` is processed, leaves the device
  `QUIESCED` with every later `START` answering `EINVAL` (`ublk_drv.c:2704-2705`, tested
  before the `EBUSY` at `:2719`), which is also what a recovery still in progress
  answers until its last fetch is processed (`:2729`, `:1624-1628`): only the reaper can
  take it, once it knows the recovering daemon gone, and its requeued requests fail
  (K2, pitfall 6). A device that was never started is `DEAD`, not `QUIESCED`, and
  cannot be recovered either.
- The guarantee (5.0): after a `kill -9` over a hugetlbfs arena, a boot that re-attached
  the arena whole (`reattached`, no damaged slot), with no rewind, no lost arena and no
  orphaned run, loses no acknowledged write and changes no nvfail (save benign
  operations in flight at the kill whose CP's `A` slot had landed, which take effect
  at the restart: a clear, `engine.rs:5647-5653`, `:5931-5935`; a deletion, which
  takes the volume's acknowledged writes with it and, for a held volume, its hold,
  `:5632-5634`, `:5846-5847`; and a rollback or a shrinking resize, which releases
  the writes since the snapshot or beyond the new end, `:2625`, `:2665`, `:5809`,
  `:5838`), and with `REISSUE` every request still in flight on a
  device D2 recovers is served again safely; otherwise a dirty boot holds the
  write-active set. A clean boot holds nothing and needs no hold, also the one after a
  `kill -9` once a clean shutdown's final CP has landed its `A` slot: that CP carries
  every write the arena held (pitfall 23; 5.0, the end of (c)). The guarantee holds
  only if no earlier boot since the selected CP met the hold's condition
  (`engine.rs:1869-1870`), whether or not it got as far as setting the hold, and
  stopped after its attach's rewrites or `Arena::create`, before its boot commit's
  `A` slot landed, which the boot cannot check today: such a boot sets its hold only
  after its replay (`:1866-1885`, after `:1820-1864`) and keeps it only in memory
  until that slot (written at `:5656` and `:5674`), while its attach has already
  rewritten every damaged slot `Committed` (`arena:933-935`) and `Arena::create` has
  already overwritten a lost or blank arena (`engine.rs:1691`, `:1704`), both before
  the replay. A second `kill -9` (during the replay too), a stop request,
  `--op-timeout` or a tick error in `Pool::boot`'s wait for
  the boot commit (`pool.rs:280`; a failed boot commit is retried, not fatal, so one
  that keeps failing, or one that failed for want of space and waits for a
  `request_cp` nothing sends during the boot, ends there at `--op-timeout`: 4.4 step
  8), or a `start_job` error (`engine.rs:1714-1725`), then leaves the next boot
  nothing to hold on, and it serves volumes whose acknowledged writes are gone
  without nvfail (5.0, the gap; pitfall 22: an engine fix to propose).
  Two operational edges: a read error fails the boot (to be retried, which is safe
  unless the failed boot held volumes and stopped after its attach, before its boot
  commit's `A` slot landed) instead of rewinding it (B1), and a larger
  `--arena-bytes` at the restart recreates the arena and destroys the acknowledged
  writes it held (pitfall 13). And the guarantee covers data, not progress: a kill
  inside a multi-block `fill` meets every report condition above and can still stall
  every write within a lap until another restart (pitfall 21), so a promise of
  progress needs that arena fix first (5.0, the last case not covered).

## 1. Scope: what I read

Code, in full unless a range is given:

- `crates/otap-arena/src/lib.rs` (1-1349, whole file).
- `crates/otap-index/src/lib.rs` (1-724, whole file).
- `crates/otap-cp/src/engine.rs`: 1-60 (constants), 334-696 (`Event`, `EngineError`),
  808-836 (`BootFailure` and its `into_parts`), 1004-1070 (`StagedBlock`, `CpJob`,
  `Staging`), 1252-1483 (`State`, `VolRt`, `Stats`, `Engine`), 1637-2118 (`boot`,
  `assemble`, `tick`), 2119-2144 (`request_cp`, `complete_paced`), 2240-2293
  (`shutdown`, `nvfail_clear`, `create_volume`), 2555-2610 (`set_volume_online`,
  `check_vol_op`), 2690-2720 (`delete_volume`), 3100-3160
  (`peek`, `peek_disk`), 3220-3237 (`nvfail_set`), 3326-3343 (`arena_memory_mut`,
  `into_parts`), 3349-3736 (the front door: `serve_front_door`, `refuse_everything`,
  `serve`, `serve_write`, `serve_punch`, `acknowledge`, `serve_read`, `read_block`,
  `read_disk_block`, `read_entry`), 3742-3810 (intents), 3874-3929 (`maybe_cut`),
  3966-4013 (`new_job`), 4315-4527 (`step_staging`, `build_stripe`), 5403-5416 (the
  `created_in` stamp in `prepare_meta_inner`), 5451-5691 (`step_meta`, `step_flush`,
  `step_commit`, `build_slot`), 5711-5995 (`finalize`), 6004-6105 (`release_freed`,
  `note_dead_data`), 6143-6169 (`runs_of`).
- `crates/otap-cp/src/traits.rs` (whole file), `crates/otap-cp/src/lib.rs` (whole),
  `crates/otap-cp/src/boot.rs` 200-300 (`BootState`, `rewound`, `evaluate`), 600-700
  (the stripe and data-block checks); `crates/otap-format/src/superblock.rs` 240-262
  (`VolState::ABSENT`), 676-686, 712-755 (`SuperblockSlot::new`, `verify`), 784-821
  (`SuperblockSlot::rank`, `select`); `crates/otap-format/src/constants.rs` 50-58
  (`MAX_VOLS`, `MAX_VOL`); `crates/otap-map/src/lib.rs` 1340-1380 (the roots table of
  a CP that deletes a volume).
- `crates/otap-daemon/src/arena.rs` (whole), `lib.rs` (whole), `frontdoor.rs` (whole),
  `pool.rs` 120-300, 320-392, 440-558, 676-739, 865-900 (`volume_create`,
  `volume_delete`); `daemon.rs` 90-125, 175-205, 225-275 (and by grep);
  `bin/dbladed.rs` 160-188; `crates/otap-daemon/src/device.rs` (whole: `create_device`
  creates the device, builds its front door, then starts it);
  `crates/otap-control/src/units.rs` 50-71.
- `crates/otap-sys/src/hugetlb.rs` 1-430; `crates/otap-sys/src/ublk.rs` 88-142,
  275-330, 440-540, 795-950, 1290-1517; `crates/otap-sys/src/block.rs` 145-162 (the
  pool device's `open` flags).
- `crates/otap-ublk/src/frontdoor.rs` (whole), `device.rs` (whole), `mock.rs` (whole),
  `lib.rs` (whole).
- `crates/otap-sim/src/harness.rs` 1-32, 144-164, 194-304, 386-640, 661-666, 995-1005,
  1060-1095, 1130-1225, 1255-1374; `frontdoor.rs` (whole); `model.rs` 1-60, 250-540;
  `lib.rs` (whole).
- Tests: `crates/otap-arena/tests/arena_tests.rs` 1-38, 169-219, 320-403 (and the
  index of every test); `adversarial_arena_tests.rs` 31, 60-95, 184-310, 495-580, 765-778 (and
  the index); `crates/otap-sim/tests/sim_tests.rs` 1-80, 196-262, 299-332, 618-650;
  `adversarial_sim_tests.rs` 119-232, 465-520, 960-1010, 1171-1450, 1490-1548 (and the
  index); `step5_sim_tests.rs` 1374-1409; `crates/otap-ublk/tests/engine_tests.rs`
  1-167, 310-334, 356-432; `frontdoor_proptests.rs` 1-60, 238-250, 466-482; the indexes
  of `frontdoor_tests.rs` and `step4_sim_tests.rs`; every `nvfail_clear`,
  `clear_nvfail` and `crash_daemon` call under `crates/otap-sim/tests/` (by grep);
  `crates/otap-daemon/tests/daemon_tests.rs` 70-100, 740-820 and
  `review_daemon_tests.rs` 75-90, 345-356, 442-452 (the arena kind);
  `crates/otap-sys/tests/sys_tests.rs` 826-891; every `ArenaKind::` and
  `HugeArena::anonymous`/`heap`/`map_file` call under `crates/` (by grep).
- Kernel (GPL sources kept outside the tree, read only, nothing copied):
  `OpenTAP-local/kernel-6.12/ublk_drv.c` 515-575, 580-625, 670-695, 937-984,
  985-1300, 1301-1320, 1355-1640, 1676-1800, 1838-1878, 2000-2016, 2125-2172,
  2188-2296, 2370-2470, 2505-2600, 2640-2790,
  2896-2975; `OpenTAP-local/kernel-6.12/ublk_cmd.h` 15-52, 55-125,
  128-200, 226-238.
- Third party, to settle one fact about the gate: `proptest-1.12.0`
  `src/test_runner/config.rs` 20-90, 225-240, 585-600 and `src/sugar.rs` 160-244
  (in `~/.cargo/registry`).
- Documents: `docs/07-engineering-decisions.md` E09 (205-223), E16 (339-381),
  E17 (383-464), E18 (466-506), E20 (537-556), E23 (670-702), E28 (827-873),
  E30 (901-936), E34 (1021-1102), E38 (1316-1426), E39 (1428-1433);
  `docs/09-working-agreements.md` 50-62, 126-136; `docs/10-phase-c-step-briefs.md`
  578-633 (the Step 6 brief as received); `docs/06-implementation-status.md` 399.
- Added by the editor's pass (2026-10-15), for the items it checked:
  `crates/otap-cp/src/engine.rs` 60-110 (`Pacing`), 160-200, 3180-3205, 3345-3402,
  3434-3455, 3600-3630, 3870-3964, 3966-4060, 4255-4460 (`fail_cp`, `step_staging`),
  4475-4515 (`build_stripe`), 5130-5160, 5400-5418, 5515-5572, 5860-5932, 6136-6172;
  `crates/otap-cp/src/traits.rs` 160-212; `crates/otap-cp/src/layout.rs` 228-250;
  `crates/otap-ublk/src/frontdoor.rs` 78-100, 128-290, 355-520; `device.rs` 1-390;
  `mock.rs` (whole); `lib.rs` 1-54; `crates/otap-sys/src/ublk.rs` 740-880, 1195-1280,
  1290-1440; `crates/otap-sys/src/hugetlb.rs` 320-400, 470-500;
  `crates/otap-sys/src/block.rs` 1-12, 127-128, 384-399;
  `crates/otap-backstore/src/uring.rs` 170-269; `crates/otap-sim/src/backstore.rs`
  255-295; `crates/otap-map/src/lib.rs` 35-60, 430-455, 1036-1052, 1440-1456,
  1600-1665, 1800-1835 (and `birth_cp` by grep); `crates/otap-format/src/superblock.rs`
  436-470; `crates/otap-daemon/src/` `daemon.rs` 80-130, 170-345, `server.rs` 60-235,
  `bin/dbladed.rs` 140-188, `arena.rs` and `device.rs` (whole), `config.rs` 88-99,
  `backstore.rs` (by grep), `pool.rs` 200-300, 331-376, 435-560, 676-740;
  `crates/otap-arena/tests/adversarial_arena_tests.rs` 55-160, 425-778;
  `arena_tests.rs` 295-325; `crates/otap-sim/tests/step4_sim_tests.rs` 95-108,
  385-420, 1185-1225, 1385-1400; `review_step5_sim_tests.rs` 910-925;
  `adversarial_sim_tests.rs` 1230-1275, 1370-1400; `step5_sim_tests.rs` 285-330;
  `crates/otap-daemon/tests/daemon_tests.rs` 655-725; `scripts/linux-smoke.sh` 235-285;
  `docs/01-architecture-brainstorm.md` 198-212 and
  `docs/01-architecture-brainstorm-record.md` (by grep for `REISSUE`); `docs/06`
  338-350, 395-402, 540-560; `docs/07` 205-224, 339-382, 668-704, 827-875, 1316-1362,
  1400-1430; `docs/09` 50-62; `docs/10` 578-633; `ublk_drv.c` 2155-2175, 2505-2570.
- Added by the second repair round (2026-10-15), beyond the ranges above:
  `OpenTAP-local/kernel-6.12/ublk_drv.c` 1658-1668 (`ublk_fill_io_cmd`);
  `crates/otap-ublk/tests/engine_tests.rs` 288-336; `crates/otap-sim/src/backstore.rs`
  1-24, 296-340.
- Added by the third repair round (2026-10-15), beyond the ranges above:
  `crates/otap-cp/src/engine.rs` 1590-1636 (format's CP 0), 2186-2190 (the clean
  job), 3238-3262 (`Engine::volume`); `crates/otap-sim/src/harness.rs` 956-994
  (`sync_size`, `sync_sizes`); `crates/otap-sim/src/model.rs` 165-190 (`volumes`,
  `add_volume`, `remove_volume`), 540-566.
- Added by the final audit's fixes (2026-10-16), beyond the ranges above:
  `crates/otap-cp/src/engine.rs` 2610-2692 (`rollback_volume`, `resize_volume`),
  4790-4870 (the volume operations a CP hands `prepare_cp`).
- Not read, by the rules: `docs/step6/` and `OpenTAP-local/step6/`.

## 2. Public API and key internals

### 2.1 `otap-arena` (`crates/otap-arena/src/lib.rs`)

Layout 1.2 (`ARENA_VERSION = 0x0102`, `:95`): a 4 KiB header, a descriptor table of
32 bytes per slot padded to pages, then 4 KiB slots (`:11-17`). Constants:
`ARENA_MAGIC` `:91` (`OTAR`), `HOLE_RECORD_MAGIC` `:97` (`OTHL`), `HOLE_RECORD_SIZE =
32` `:100`, `SLOT_BYTES = 4096` `:102`, `SLOT_DESC_SIZE = 32` `:104`,
`SLOT_WORD_OFFSET = 24` `:106`, `ARENA_HEADER_SIZE = 4096` `:108`,
`ARENA_HEADER_SEALED = 56` `:110`, `ARENA_ATTACH_WORD_OFFSET = 64` `:112`,
`DEFAULT_ARENA_BYTES = 8 GiB` `:114`, `ARENA_HEADER_RESERVED = 4096 - 72` `:116`,
`MAX_LAP = 2^30 - 1` `:118`.

| Item | Where | Meaning |
|---|---|---|
| `enum SlotState { Free=0, Filled=1, Placed=2, Committed=3 }` | `:123-132` | the two state bits of a descriptor word; `from_bits` `:137`, `as_u8` `:148`, `is_reusable` (`Free` or `Committed`) `:154-156` |
| `struct ArenaHeader` (`#[repr(C, align(4096))]`) | `:164-192` | `magic, ver, rsvd0, agg_id: Uuid, slots, slot_bytes, desc_off, data_off, created_wall_ns, crc32c (over 0..56), rsvd1, attach_word (offset 64, outside the seal), reserved: [u8; 4024]` |
| `ArenaHeader::verify(&self, len) -> Result<(), ArenaError>` | `:207-226` | magic `:208`, **major version only** (`ver >> 8`) `:211`, CRC `:214`, geometry against `len` `:217-224` |
| `ArenaHeader::encode_attach_count(count) -> u64` / `attach_count() -> Option<u32>` | `:230-232` / `:237-245` | the self-checking word `count \| !count << 32`; a torn word reads as `None` |
| `enum SlotKind { Data=0, Hole=1 }`, `from_u32` (any other value → `None`) | `:251-257`, `:262-268` | the descriptor's `kind` word |
| `struct HoleRecordBlock { magic, vol, lba, blocks, rsvd0: u32, rsvd1: u64 }` | `:283-296` | the 32-byte record at the start of a hole slot; `image(vol, lba, blocks)` `:301-313`; `decode(block, vol, lba)` `:319-333` requires magic, the same `vol`/`lba` as the descriptor, `blocks >= 1`, **both reserved fields zero and the remaining 4064 bytes zero** |
| `struct SlotDesc { addr: u64, seq: u64, crc32c: u32, kind: u32, word: u64 }` | `:342-354` | `addr = vol << 40 \| lba` (`pack_addr` `:371-373`, `vol()` `:377`, `lba()` `:383`); `word` low 32 = `lap << 2 \| state`, high 32 = CRC-32C over bytes 0..24 plus the low word (`word_for` `:402-408`); `verify()` `:413-415` = word non-zero and CRC matches; `sealed_as()` `:422-433` = the other state whose seal the stored CRC matches (damage detection); `lap()` `:389`, `state()` `:395`, `slot_kind()` `:359`, `is_hole()` `:365`. **No reserved bytes: all 32 are used** (`const` asserts `:1335-1344`) |
| `struct RingRef { slot: u32, lap: u32 }`, `offset(blocks)` | `:438-443`, `:448-453` | a run's first slot and the lap it was handed out on |
| `struct PlacedSlot { ring, vol, lba, seq, crc32c, hole_blocks }`, `is_hole()` | `:459-473`, `:478-480` | one `Placed` slot the attach kept, descriptor and data verified; `hole_blocks == 0` for data |
| `enum Damage { Descriptor, Data, Kind }` | `:485-493` | what failed on a damaged slot |
| `struct DamagedSlot { slot, desc, damage }` | `:500-507` | a slot the attach released instead of trusting |
| `struct Reattach { placed: Vec<PlacedSlot>, damaged: Vec<DamagedSlot>, dropped_filled: u32, committed: u32, attach_count: u32 }`, `is_damaged()` | `:511-523`, `:529-531` | the attach report; `placed` is sorted by `(seq, position)` (`:937-942`) |
| `enum ArenaError` (`#[non_exhaustive]`) | `:537-614` | `TooSmall{len}`, `BadMagic{found}`, `BadVersion{found}`, `BadCrc`, `BadGeometry`, `WrongAggregate{found}`, `Inconsistent{reason}`, `Full{wanted, free}`, `BadRun{wanted}`, `LapExhausted`, `Stale{ring, lap}`, `OutOfRange{slot}`, `BadAddress{vol, lba}`, `BadTransition{slot, state}`, `DataCrc{slot}` |
| `Arena::create(mem: HugeArena, agg_id, wall_ns) -> Result<Self, (ArenaError, HugeArena)>` | `:704-736` | zeroes header and descriptor table (`:714`), seals the header with `attach_word = encode(1)` (`:724-726`), `next_seq = 1` (`:734`) |
| `Arena::is_blank(&HugeArena) -> bool` | `:742-745` | header page all zero: fresh memory or memory a power loss emptied |
| `Arena::can_attach(&HugeArena, &Uuid) -> Result<(), ArenaError>` | `:749-761` | header verify plus aggregate identity, touching nothing |
| `Arena::attach(mem, &Uuid) -> Result<(Self, Reattach), (ArenaError, HugeArena)>` | `:766-795` | `can_attach`, then `scan_on_attach` (below); gives the memory back on refusal |
| `scan_on_attach(&mut self, hdr) -> Result<Reattach, ArenaError>` (private) | `:798-949` | the whole re-attach policy, section 4.3 |
| `Arena::header()`, `slots()`, `capacity_bytes()`, `occupied()`, `occupancy()` | `:953`, `:960`, `:966`, `:973`, `:979` | geometry and the window `[tail, head)` as a count / fraction |
| `Arena::next_seq(&mut self) -> u64`, `peek_seq()` | `:984-988`, `:993-995` | the sequence counter: one per request, never handed out twice |
| `Arena::reserve(&mut self, blocks) -> Result<RingRef, ArenaError>` | `:1002-1050` | hands out `blocks` consecutive `Free` slots on the current lap, never wrapping; skipped end-of-lap slots are written `Committed` (`:1032-1040`); `Full` when the run would reach live data (`:1019-1024`) |
| `Arena::run_mut(ring, blocks) -> Result<&mut [u8], _>` | `:1054-1060` | the run's bytes for filling; states `Free` or `Filled` only |
| `Arena::run(ring, blocks) -> Result<&[u8], _>` | `:1063-1069` | the bytes of a `Filled` or `Placed` run |
| `Arena::check_data(ring, blocks)` | `:1073-1084` | every block against its descriptor's CRC; `DataCrc{slot}` on the first mismatch |
| `Arena::fill(ring, vol, lba, seq, crcs: &[u32])` | `:1090-1115` | records `addr`, `seq`, `crc32c`, `kind = Data` per slot then one word store `Filled` per slot (`:1105-1113`); the data must already be in place; states must be `Free` (`:1104`) |
| `Arena::fill_hole(ring, vol, lba, blocks, seq)` | `:1122-1145` | writes a `HoleRecordBlock` image into the one slot (`:1135-1137`), descriptor `kind = Hole`, word `Filled` |
| `Arena::hole_record(ring) -> Result<HoleRecordBlock, _>` | `:1148-1159` | decodes a `Filled`/`Placed` hole slot (after `check_data`) |
| `Arena::place(ring, blocks)` | `:1163-1165` | `Filled -> Placed` for every slot of the run, one word store each (`transition` `:1222-1235`) |
| `Arena::commit(ring, blocks)` | `:1169-1173` | `Placed -> Committed`, then `advance_tail` |
| `Arena::discard(ring, blocks)` | `:1178-1187` | `Free \| Filled -> Committed` (a refused write, a failed copy), then `advance_tail` |
| `Arena::descriptor(slot) -> Result<SlotDesc, _>` | `:1190-1195` | a copy of one descriptor (`OutOfRange` past the ring). **This is how an implementer reads a replayed block's CRC and sequence** |
| `Arena::window() -> (u64, u64)` | `:1199-1201` | `(tail, head)` as positions `lap * slots + slot` |
| `Arena::into_memory()`, `memory()`, `memory_mut()` | `:1205`, `:1211`, `:1218` | the `HugeArena` back (the sim keeps it across a "kill"); `memory_mut` is for fault injection, nothing is re-checked |
| `check_run` / `check_states` / `passable` / `advance_tail` / `write_word` | `:1237-1254` / `:1256-1272` / `:1278-1282` / `:1284-1288` / `:1294-1299` | the lap check (`Stale`), the state check (`BadTransition`), the tail rule (another lap or `Committed` passes; a `Free` reservation on the tail's own lap does not), and the one store with a `compiler_fence` before it |
| `slots_for_len(len) -> Result<u32, ArenaError>`; private `Geometry::for_len(len)` | `:1321-1323`; `:660-684` | slots an arena of `len` bytes holds (the daemon uses it at `pool.rs:242`): `for_len` starts at `(len - 4096) / (4096 + 32)` slots and decrements until the descriptor table, padded to whole 4 KiB pages, plus the slots fit (`arena:666-674`), then puts the slots after the padded table (`data_off`, `:678-683`). A 2 MiB arena holds 507 slots (`crates/otap-arena/tests/adversarial_arena_tests.rs:105`) with `data_off = 4096 + 16384` (`crates/otap-arena/tests/arena_tests.rs:307`) |

### 2.2 `otap-index` (`crates/otap-index/src/lib.rs`)

| Item | Where | Meaning |
|---|---|---|
| `type GenId = u64`; `type Superseded = Vec<(RingRef, u32)>`; `superseded_blocks` | `:56`, `:60`, `:64-66` | generation ids count from 1; superseded runs are `(first slot, blocks)` the caller releases |
| `DEFAULT_RING = 8` | `:69` | the active generation plus at most seven frozen |
| `struct DirtyExtent { ring: RingRef, blocks: u32, seq: u64, hole: bool }` | `:78-87` | a run of dirty blocks in consecutive slots, or a punched hole whose `ring` names the one record slot; `data()` `:92-99`, `hole()` `:103-110`, `slice(from, to)` `:115-126`, `slot_of(k)` `:130-136`. **No per-block CRC** |
| `struct Hit { ring, seq, gen, hole }` | `:141-150` | where a block is: its own slot (or the hole's record slot), the extent's sequence, the generation |
| `enum IndexError { BadVol, BadExtent, StaleSeq{vol, seq, newest}, RingFull, NotOldest }` | `:155-188` | `StaleSeq` is the ordering contract (E20) |
| `VolumeDirty::insert(&mut self, lba, ext) -> Superseded` | `:238-272` | the later insert wins its whole range: overlapped extents are trimmed/split (`:251-268`), data pieces displaced are returned (`:262-267`), holes displaced return nothing; then `coalesce_around` (`:270`) |
| `VolumeDirty::lookup(lba) -> Option<(RingRef, u64)>`, `find(lba) -> Option<(u64, &DirtyExtent)>` | `:277-280`, `:284-291` | the slot and sequence of one block / the extent holding it |
| `VolumeDirty::iter()`, `blocks()`, `extents()`, `is_empty()`, `data_blocks()` | `:304`, `:310`, `:316`, `:322`, `:295` | the map in LBA order and its counters |
| `coalesce_keys` | `:349-386` | merges neighbours only when the slot runs continue each other on the same lap (`:368`) or holes share one record slot (`:366`); the merged `seq` is the max (`:375`) |
| `Generation` (`id`, per-volume `Mutex<VolumeDirty>`, counters) | `:391-398` | `id()` `:416`, `blocks()` `:422`, `inserts()` `:434`, `opened_ns()` `:440`, `cut_ns()` `:446`, `is_empty()` `:452`, `with_volume(vol, f)` `:457-460`, `for_each_dirty_volume(f)` `:463-470`, `dirty_volumes()` `:474-478`; private `insert` `:480-497` and `lookup` `:499-508` |
| `Index::new(ring, now_ns) -> Self` | `:528-539` | generation **1** is active, `next_id = 2`, `newest_seq` per volume starts at 0 |
| `Index::insert(&self, vol, lba, ext) -> Result<(GenId, Superseded), IndexError>` | `:553-586` | bounds (`:559-568`), then `newest_seq[vol].fetch_max(ext.seq)` and **refuse `seq` below the newest; equal is allowed** (`:573-581`), then insert into the active generation loaded lock-free (`:583-585`) |
| `Index::newest_seq(vol) -> u64` | `:590-594` | the highest sequence inserted for the volume |
| `Index::lookup(vol, lba) -> Option<Hit>` | `:599-613` | active generation first, then frozen newest first |
| `Index::with_active(f)`, `active_id()`, `active_blocks()`, `unflushed_blocks()` | `:617-620`, `:624`, `:630`, `:636` | a cut waits for `f`; never cut inside it |
| `Index::cut_within()`, `cut(now_ns) -> Result<Arc<Generation>, _>` | `:642-648`, `:654-685` | swap the active pointer, spin until the old `Arc` has one holder (`:672-679`), queue it |
| `Index::oldest_frozen()`, `frozen()`, `frozen_len()`, `retire(gen)` | `:689`, `:695`, `:701`, `:707-716` | the flusher's queue; `retire` must name the oldest |

### 2.3 The engine (`crates/otap-cp/src/engine.rs`) and the traits (`traits.rs`)

Constants: `MAX_REQUEST_BLOCKS = 2048` `:37`, `MAX_PUNCH_BLOCKS = 1 << 18` `:41`.

| Item | Where | Meaning |
|---|---|---|
| `Event::Booted { cp_seq, dirty, reattached, replayed_blocks, rewound, arena_lost, damaged_slots, orphaned_blocks, nvfail: VolBitSet, torn }` | `:336-359` | what the boot found; the daemon turns it into a journal line (`pool.rs:333-375`) |
| `Event::CpCommitted { cp_seq, trigger, gen: Option<GenId>, blocks, max_seq }` | `:361-372` | `max_seq` is the highest write sequence the CP carried |
| `Event::ArenaDamage { vol, lba, slot }` | `:508-515` | a placed block no longer matches its recorded CRC at staging |
| `EngineError::{Arena(ArenaError), Index(IndexError), Io(IoError), NoSuchVolume{vol}, NvFailed{vol}, Corrupt{vol, lba}, NotServing, Internal(String), ...}` | `:537-696` | `tick` returns `Err` only for internal failures; request refusals are statuses |
| `struct StagedBlock { vol, lba, ring: RingRef, seq }`; `CpJob { cp_seq, nonce, trigger, gen: Option<Arc<Generation>>, blocks: Vec<StagedBlock>, holes: Vec<(u32,u64,u32)>, max_seq, run_state, ... }` | `:1004-1009`, `:1012-1035` | one CP's work, built by `new_job` from the frozen generation (`:3977-3995`) |
| `struct VolRt { present, creating, created_in, size_blocks, rollback_gen, nvfail: Option<NvCause>, clearing, windows: u8, offline, name, op, op_in }`, `serving()` | `:1280-1301`, `:1304-1306` | per-volume runtime state; `serving` = present, not creating, no nvfail, not offline |
| `Stats { cps, stripes, writes, punches, reads, deferred, intents, ..., paced, ... }` | `:1311-1348` | counters; `writes` `:1317`, `punches` `:1319`, `deferred` `:1323`, `paced` `:1345` |
| `Engine<B: Backstore, F: FrontDoor, C: Clock>` fields | `:1364-1483` | `arena` `:1370`, `index` `:1371`, `paced: VecDeque<(Request, u64, u64)>` `:1411`, `gen_holes: HashMap<GenId, Vec<RingRef>>` `:1438`, `durable_active` `:1458`, `intent_inflight` `:1459`, `pending_acks: Vec<(Request, u64)>` `:1461`, `deferred: VecDeque<Request>` `:1465`, `serving` `:1466`, `stopped` `:1467` |
| `Engine::boot(cfg, backstore, front, clock, memory: HugeArena) -> Result<Self, BootFailure<B,F,C>>` | `:1637-1727` | section 4.4; `BootFailure::into_parts` (`:827`) gives everything back |
| `Engine::assemble(...)` (private) | `:1731-2071` | builds the engine; **the arena replay is here** (`:1820-1864`) and the nvfail hold (`:1866-1885`) |
| `Engine::tick(&mut self) -> Result<(), EngineError>` | `:2076-2118` | poll → discards/intents → CP state machine → `retry_intent` → `retry_discards` (`:2098`) → boot retry → `complete_paced` → front door (only while `serving && !stopped`, `:2109-2110`; once `stopped`, `refuse_everything` instead, `:2111-2112`) → `maybe_cut` → `maybe_start_cp` → `maybe_pump` |
| `Engine::request_cp()`, `shutdown()`, `nvfail_clear(vol) -> Result<u32, _>` | `:2122-2125`, `:2240-2243`, `:2251-2267` | control entry points |
| `Engine::peek(vol, lba, dst) -> Result<Vec<PeekSource>, _>`, `peek_disk(...)` | `:3100-3113`, `:3117-3130` | read through arena+map / map only (for checkers) |
| `Engine::arena_memory_mut() -> &mut HugeArena`, `into_parts() -> (B, F, C, HugeArena)` | `:3328-3330`, `:3336-3343` | fault injection; the doc of `into_parts` (`:3332-3334`): "Takes the engine apart, as a `kill -9` would" |
| `serve_front_door`, `refuse_everything`, `serve`, `serve_write`, `serve_punch`, `acknowledge`, `serve_read`, `read_block` | `:3353-3381`, `:3383-3399`, `:3402-3486`, `:3488-3552`, `:3556-3602`, `:3607-3628`, `:3630-3662`, `:3667-3688` | section 4.1 and 4.2 |
| `struct Pacing { pace_at, stall_at, max_delay_ns }`, its `Default`, `delay_ns(occupancy)` | `:77-84`, `:86-94`, `:99-105` | defaults 0.80, 0.95 and 50 ms (`EngineConfig::new` takes them, `:190`, and so does the daemon, `pool.rs:244`); the delay is 0 up to `pace_at` and grows quadratically to `max_delay_ns` at `stall_at` (`engine.rs:100-104`). `acknowledge` parks a write in `paced` while the delay is non-zero or `paced` is not empty (`:3618-3626`), so that population exists only once the occupancy has passed `pace_at`; at `stall_at` `serve` hands writes back unplaced (`:3448-3450`) |
| `fn runs_of(placed: &[PlacedSlot]) -> Vec<(u32, u64, DirtyExtent)>` (free function, private) | `:6143-6169` | coalesces consecutive placed slots of one write into extents (same vol, same seq, consecutive lba, consecutive slot on the same lap; `:6155-6160`); a hole slot is one hole extent (`:6146-6152`) |
| `FrontDoor` trait: `fetch() -> Option<Request>`, `copy_in(&Request, &mut [u8]) -> Result<(), IoError>`, `copy_out(&Request, &[u8])`, `complete(Request, Status, seq: u64)` | `traits.rs:438-452` | `seq` is the write's sequence number, 0 otherwise (`:449-451`); the ublk front door ignores it (`otap-ublk/src/frontdoor.rs:499`) |
| `Request { id: ReqId, kind: RequestKind }`, `RequestKind::{Write, Read, Flush, Discard, WriteZeroes}`, `vol()`, `is_punch()` | `traits.rs:320-325`, `:268-316`, `:330-338`, `:343-348` | `ReqId(pub u64)` `:264` |
| `Status::{Ok, NoSuchVolume, OutOfRange, NvFailed, Corrupt, Stopped, Failed, NoSpace, TooLarge, Offline}` | `traits.rs:354-379` | mapped to errnos by `otap_ublk::errno_for` (`otap-ublk/src/frontdoor.rs:192-206`) |
| `IoError::{Device{errno}, OutOfRange, BadLength, Corrupt}` | `traits.rs:45-66` | `copy_in` failure = `Corrupt` when the request is gone |
| `NvfailIntent::{Conservative, Precise}`; `MediaProfile.nvfail_intent` | `traits.rs:103-111`, `:174` | precise mode makes `acknowledge` wait for an intent |

### 2.4 The ublk front door (`crates/otap-ublk`) and `otap-sys::ublk`

| Item | Where | Meaning |
|---|---|---|
| `enum QueueEvent { Request { tag, desc: UblksrvIoDesc }, Abort { tag } }` | `frontdoor.rs:29-42` | what a transport poll delivers |
| `trait Transport { poll(&mut Vec<QueueEvent>) -> Result<usize, TransportError>; read_data(tag, off, dst); write_data(tag, off, src); commit(tag, result: i32) }` | `otap-ublk/src/frontdoor.rs:62-76` | the queue as the state machine sees it |
| `Watermarks { high, low }`, `for_depth(depth)` | `:80-97` | gate at `high` in engine, reopen at `low` |
| `FrontDoorStats { requests, reads, writes, flushes, discards, write_zeroes, refused, offline_rejected, stale, aborted, errors, gate_closed, commit_failures }` | `:101-128` | |
| `enum TagState { Idle, Ready, InEngine, Aborted }`; `TagSlot { state, kind, gen: u32 }` | `:131-140`, `:143-147` | **the generation counter lives in process memory only** |
| `decode_request(&UblksrvIoDesc, vol, size_blocks) -> Result<RequestKind, i32>` | `:153-187` | errno `EOPNOTSUPP`/`EINVAL` for unservable requests |
| `errno_for(Status, &RequestKind) -> i32` | `:192-206` | `Ok`: the bytes a read or write moved, 0 for a flush or a punch (`:194-199`); `NvFailed`, `Corrupt`, `Failed` → `-EIO` (`:200`); `NoSuchVolume`, `Stopped`, `Offline` → `-ENODEV` (`:201`); `NoSpace` → `-ENOSPC` (`:202`); `OutOfRange`, `TooLarge` → `-EINVAL` (`:203`); anything else `-EIO` (`:204`). So a reissued request answers `ENOSPC` on a full pool and `ENODEV` from a stopping engine |
| `request_id(q_id, tag, gen) -> ReqId` (= `gen << 32 \| q_id << 16 \| tag`), `split_id` | `:213-215`, `:219-221` | |
| `UblkFrontDoor::new(transport, q_id, vol, size_blocks, depth, watermarks)` | `:247-282` | |
| `set_offline(bool)`, `is_offline()`, `in_engine()`, `ready()`, `stats()`, `transport()`, `last_error()`, `aborted_tags()` | `:288-302`, `:306`, `:312`, `:318`, `:324`, `:335`, `:341`, `:347-354` | |
| `pump() -> Result<usize, TransportError>` | `:359-423` | polls and decodes; an `Abort` on an `InEngine` tag bumps `gen` so the engine's later completion is dropped as stale (`:378-383`) |
| `impl FrontDoor for UblkFrontDoor`: `fetch` `:438-469`, `copy_in` `:471-486`, `copy_out` `:488-497`, `complete` `:499-518` | | `complete` checks `q_id`, `InEngine` and `gen` (`:505-508`), retires the tag (`:509-512`), maps the status (`:516`) and commits (`:517`) |
| `DeviceSpec { size_blocks, depth, max_io_bytes, io_opt_bytes, max_discard_bytes, rotational }`, `new`, `for_arena(slots)` | `device.rs:22-38`, `:44-53`, `:60-66` | Linux only. `new`: 128 tags, 8 MiB requests, discards up to 1 GiB; `for_arena` caps `max_io_bytes` at half the arena's slots in 4 KiB blocks, at least one block (E38 `docs/07:1358-1360`: "requests bounded by half the arena"); the daemon builds `new(...).for_arena(arena_slots)` (`crates/otap-daemon/src/device.rs:19`), and a recovered device keeps the values it was created with (pitfall 13). The two with their docs: `device.rs:41-53`, `:55-66` |
| `UblkDevice::create(&DeviceSpec)` | `:88-125` | `ADD_DEV` with `UBLK_F_USER_COPY \| UBLK_F_USER_RECOVERY \| UBLK_F_CMD_IOCTL_ENCODE` (`:95`), **no `REISSUE`**, every other `dev_info` field default (`state` 0, `ublksrv_flags` 0: `device.rs:90-97`); `set_params` (`:127-164`, `VOLATILE_CACHE \| FUA` `:130`), `open_queue` (`:166-177`, a fetch on every tag) |
| `start()`, `stop()`, `delete()`, `take_transport()`, `dev_id()`, `info()`, `block_path()` | `:184-190`, `:196-201`, `:214-219`, `:242-247`, `:223`, `:229`, `:235` | |
| `LinuxTransport` and its `Transport` impl | `:261-264`, `:282-341` | `poll` = `submit()` then `reap` (`:284-289`); `UBLK_IO_RES_OK` → `Request` with the mapped descriptor (`:292-296`), anything else → `Abort` (`:297-303`); `commit` only **pushes** the SQE (`:325`), submitting only when the ring is full (`:328`) |
| `front_door(&mut UblkDevice, vol, &DeviceSpec)`, `serve(engine, stop)` | `:344-360`, `:368-382` | |
| `MockTransport` | `mock.rs:17-37` | `new(depth)` `:42`; `submit_write/read/flush/discard/write_zeroes/raw` `:69-99`; `abort(tag)` `:104-111`; `inject(event)` `:116`; `take_commits()` `:121`, `take_commit(tag)` `:126`, `read_payload(tag)` `:132`, `outstanding()` `:138`; `fail_copies`, `fail_commits`, `fail_next_poll` `:143-155`; `commit` records at once and refuses an idle tag (`:208-224`) |
| `UBLK_U_CMD_START_USER_RECOVERY = 0xC020_7510`, `UBLK_U_CMD_END_USER_RECOVERY = 0xC020_7511` | `otap-sys/src/ublk.rs:127`, `:129` | asserted at `:902-903` |
| `UBLK_F_USER_RECOVERY = 1 << 3`, `UBLK_F_USER_RECOVERY_REISSUE = 1 << 4` | `:283`, `:286` | |
| `UBLK_S_DEV_DEAD = 0`, `LIVE = 1`, `QUIESCED = 2` | `:319`, `:321`, `:324` | |
| `UblksrvCtrlDevInfo { nr_hw_queues, queue_depth, state, pad0, max_io_buf_bytes, dev_id, ublksrv_pid, pad1, flags, ublksrv_flags, owner_uid, owner_gid, reserved1, reserved2 }` | `:443-472` | `ublksrv_flags` (`:462-463`): "for the server's own use; the driver does not interpret it"; nothing in the tree writes it (its only other mention is the offset assert, `:944`) |
| `UblksrvIoDesc { op_flags, nr_sectors, start_sector, addr }`, `op()`, `flags()`, `nr_sectors()` | `:481-494`, `:500`, `:510`, `:523` | |
| `CtrlCommand::start_user_recovery(dev_id)`, `end_user_recovery(dev_id, ublksrv_pid)` | `:807-809`, `:815-819` | already encoded in `otap-sys` |
| `CtrlCommand::get_dev_info(dev_id, buf)`; `UblkControlRing::execute(&CtrlCommand, Option<&mut [u8]>)` | `:787-791`; `:1213-1276` | the `GET_DEV_INFO` builder the enumeration of 5.1 item 4 needs. `execute` is synchronous and runs one command at a time on a 2-entry ring (`:1195`): it pushes the SQE and loops on `submit_and_wait(1)` until the CQE (`EINTR` retried, `:1248-1270`), so `START_USER_RECOVERY`, its `EBUSY` retries and `END_USER_RECOVERY` block the calling thread, the engine thread in the daemon (F2), until the kernel answers |
| `IoCommand::fetch(q_id, tag)`, `commit_and_fetch(q_id, tag, result)` | `:850-860`, `:866-876` | `addr = 0` always (user copy); the doc of `fetch` says it is issued again after `START_USER_RECOVERY` (`:847-848`) |
| `UblkQueue::new(chr, q_id, depth, entries)`, `desc(tag)`, `push_fetch(tag)`, `push_commit(tag, result)`, `submit()`, `submit_and_wait()`, `reap(&mut Vec<(u16, i32)>)`, `read_data`, `write_data` | `:1316-1346`, `:1369-1376`, `:1382`, `:1391`, `:1414-1421`, `:1428-1435`, `:1441-1451`, `:1459-1478`, `:1485-1501` | the real queue; its doc (`:1296-1298`) says every command must come from the thread that issued the queue's first fetch, but the driver pins the queue to the task whose fetch completed it (F2) |

### 2.5 The memory (`crates/otap-sys/src/hugetlb.rs`) and the daemon

| Item | Where | Meaning |
|---|---|---|
| `HugeArena::map_file(path, len, page_size) -> io::Result<Self>` | `hugetlb.rs:99-103`, impl `:324-395` | Linux only: `statfs` the directory (`:335`), open **without truncation** (`:338-344`), `fstatfs` the file (`:345`), **exclusive non-blocking `flock`** (`:346-355`, `WouldBlock` when another arena holds it), refuse a longer file (`:358-367`), grow a shorter one (`:368-370`), `MAP_SHARED` mmap (`:390`). This is the re-attach: an existing file is mapped with its contents |
| `HugeArena::anonymous(len, page_size)`, `heap(len, page_size)` | `:113-117`, `:130-134` | private memory; neither survives the process. `anonymous` is `MAP_PRIVATE \| MAP_ANONYMOUS \| MAP_HUGETLB` on Linux and an aligned heap buffer elsewhere (`:105-107`); it is the arena memory of the sim harness and of every test outside `otap-daemon` (`arena_tests.rs:12`, `adversarial_arena_tests.rs:31`, `otap-ublk/tests/engine_tests.rs:91`, `harness.rs:301`, `:1178`). `heap` is heap memory on every host and has one caller, the `--heap-arena` path (`open_arena`'s `ArenaKind::Heap` arm, `daemon/src/arena.rs:19`, S5-1); its doc (`hugetlb.rs:119-124`) says a daemon crash on it is a dirty boot that holds the write-active volumes, which is true of every crash but a `kill -9` that leaves a clean shutdown's final CP the newest on disk (pitfall 12). The daemon tests take that arm: they boot their pools with `arena: ArenaKind::Heap` (`crates/otap-daemon/tests/daemon_tests.rs:82`, `:746`, `:762`, `:777`, `:814`; `crates/otap-daemon/tests/review_daemon_tests.rs:84`, `:351`, `:448`), so a crash there is never a re-attach and, but for that exception, a dirty boot that holds the volumes. No test maps a file: the only `map_file` calls in the tests expect a refusal (`crates/otap-sys/tests/sys_tests.rs:837`, `:888`) |
| `as_slice()`, `as_mut_slice()`, `as_ptr()`, `as_mut_ptr()`, `lock()`, `exclude_from_core_dumps()` | `:166`, `:172`, `:178`, `:188`, `:202`, `:212` | |
| `otap_daemon::open_arena(kind: &ArenaKind, bytes) -> Result<HugeArena, ControlError>` | `daemon/src/arena.rs:10-56` | `Heap` → `HugeArena::heap` (`:19-24`); `Hugetlb { path }` → `HugeArena::map_file` (`:39-54`) |
| `Pool::boot(spec, clock, stop)` | `pool.rs:209-295` | probe superblock, open backstore, `open_arena` (`:241`), `slots_for_len` (`:242`), `PoolEngine::boot` with **no configured volumes** (`:244-251`), wait for the boot commit (`:280`), `ensure_device` for every online volume (`:281-293`) |
| `Pool::ensure_device(vol)` / `drop_device(vol)` | `pool.rs:678-709` / `:714-739` | always **creates** a new device (`create_device` `:687-693`, `add_device` `:694`); teardown = offline, drain, `destroy_device` (`:720-731`) |
| `Pool::note(&Event)` | `pool.rs:331` (the `Event::Booted` arm `:333-375`) | the `Booted` journal line: dirty/clean and `cp_seq` (`:346-349`), `rewound` (`:350-352`), `replayed_blocks` (`:353-357`), `arena_lost` (`:358-360`), `damaged_slots` (`:361-363`), `orphaned_blocks` (`:364-366`), the nvfail count (`:367-369`), the torn list (`:370-372`); kept as `last_boot` (`:373`) |
| `ChannelFrontDoor` / `IoHandle::submit(kind, data) -> Receiver<IoCompletion>` | `daemon/src/frontdoor.rs:66-72`, `:53-57` | the `Io*` test path; ids carry `CHANNEL_ID_BIT = 1 << 63` (`:63`) |
| `DaemonFrontDoor { channel, devices: BTreeMap<u32, VolumeDevice>, turn }`, `new`, `io_handle`, `in_engine`, `device_volumes`, `add_device`, `remove_device`, `device`, `device_mut` | `daemon/src/frontdoor.rs:186-195`, `:215-222`, `:226`, `:232-244`, `:248`, `:254`, `:260`, `:266`, `:272` | one `UblkFrontDoor<LinuxTransport>` per online volume |
| `impl FrontDoor for DaemonFrontDoor` | `daemon/src/frontdoor.rs:277-330` | `fetch` round-robins the devices then the channel (`:278-296`); `copy_in`/`copy_out`/`complete` route by `req.vol()` unless the channel owns the id (`:298-329`) |
| `VolumeDevice { device: UblkDevice, front: UblkFrontDoor<LinuxTransport> }` | `daemon/src/frontdoor.rs:178-183` | Linux only |

### 2.6 The simulator surfaces an implementer of the tests needs

| Item | Where | Meaning |
|---|---|---|
| `Sim::new(SimConfig)`, `boot() -> Result<BootInfo, String>`, `tick`, `ticks`, `tick_until`, `advance` | `harness.rs:289`, `:386-462`, `:484`, `:491`, `:500`, `:533` | the harness; `boot` retries on `VolumeSizeMismatch` (`:411-423`) and ticks until serving (`:460`) |
| `submit_write`, `submit_punch`, `submit_read`, `submit_flush`, `collect`, `wait`, `write`, `read` | `:538`, `:550`, `:578`, `:583`, `:589`, `:606`, `:618`, `:627` | one at a time or several in flight |
| `crash_power()`, `crash_daemon()`, `shutdown_clean()` | `:1169-1182`, `:1186-1196`, `:1202-1217` | `crash_daemon` = `into_parts`, `disk.crash_daemon()`, `front.reset()`, **the arena memory kept** (`:1189-1194`) |
| `verify_recovery() -> Result<Verdict, String>` | `:1261-1350` | after `Stop::Daemon` no acknowledged write may be lost and nvfail must be unchanged (`:1284-1296`) |
| `BootInfo { cp_seq, dirty, reattached, rewound, arena_lost, damaged_slots, nvfail, torn }`, `Verdict { k, lost, nvfail, boot }`, `Stop::{Fresh, Power, Daemon, Clean}` | `:208-225`, `:229-239`, `:194-204` | |
| `SimFrontDoor`: `submit_write(vol, lba, data) -> ReqId`, `take_completions() -> Vec<Completion>`, `take_read(id)`, `queued()`, `in_flight()`, `reset()` | `sim/src/frontdoor.rs:45-55`, `:98-100`, `:103`, `:109`, `:115`, `:120-126` | `complete` records `Completion { req, status, seq }` (`:150-154`) |
| `Model::issued`, `issued_punch`, `acked(op, seq)`, `refused`, `acked_now`, `settle(j)`, `check_prefix`, `lost_volumes`, `mark_committed`, `data_of(op, vol, lba, blocks)` | `model.rs:266`, `:271`, `:299-304`, `:307`, `:314`, `:335-343`, `:408`, `:508`, `:521`, `:256` | the checker's contract is in its header (`:12-37`) |

## 3. Invariants and rules in force

Arena (E16, `docs/07:339-381`, and the code):

- **A1. Every transition is one store.** The fields that change only while a slot is
  `Free` (`addr`, `seq`, `crc32c`, `kind`) are stored first, then one aligned 8-byte
  store of the sealed `word` after a `compiler_fence` (`arena/src/lib.rs:45-55`,
  `:1294-1299`). A kill at any instruction leaves the *word* whole: the state and lap
  it names are those of the last completed transition (E16, `docs/07:358-362`). The
  descriptor as a whole can be between: a kill inside `fill` after the field stores
  (`arena:1108-1111`) and before the word store (`:1112`) leaves the reservation's `Free`
  seal over fields that have changed, so the word no longer verifies and `sealed_as()`
  is `None` (`:413-415`, `:422-433`). The crate's doc calls that "what a never-used
  slot looks like too" (`:51-52`), which is true of the word's validity only: a
  never-used slot has a zero word (`:714`, `:825`) and lies ahead of the head, this one
  is non-zero and can lie inside the window (pitfall 21).
- **A2. The state machine** is `Free -> Filled -> Placed -> Committed`, `Committed` and
  `Free` reusable (`:32-43`, `:154-156`); `fill`/`fill_hole` need `Free` (`:1104`,
  `:1134`), `place` needs `Filled` (`:1164`), `commit` needs `Placed` (`:1170`),
  `discard` needs `Free` or `Filled` (`:1179-1184`); anything else is
  `BadTransition`. A reserved slot is `Free` on the current lap until filled or
  discarded (`:37-38`).
- **A3. Runs never wrap** and a `RingRef` is `(slot, lap)`; every slot of the run must
  carry the reference's lap or the call is `Stale` (`:1244-1252`). `reserve` skips to
  the next lap when the run does not fit, marking the skipped slots `Committed`
  (`:1009-1013`, `:1032-1040`).
- **A4. The window `[tail, head)`** bounds occupancy without a collector: the tail passes
  a `Committed` slot or a slot of another lap, never a `Free` reservation on its own lap
  (`:1274-1282`); `Full` is judged after counting the slots a lap skip would leave dead
  as passable (`:1014-1024`).
- **A5. Sequence numbers** are one per request (`:984-988`), strictly increasing, and
  after a re-attach `next_seq = max over every descriptor + 1`, whatever the
  descriptor's state (`:932`; E16 `docs/07:365-366`; pinned by
  `adversarial_arena_tests.rs:185-211`).
- **A6. Re-attach keeps exactly the `Placed` slots whose word and data verify**, drops
  `Filled` slots whose word verifies (`:835-838`), converts the dead process's
  reservations (`Free`, word verifies) to `Committed` (`:816-821`), and reports as
  damage: a `Placed`, `Filled` or `Committed` word that does not verify (for `Filled`
  any unverifiable word is damage, with no `sealed_as` test: `:839-847`, "an unsealed
  `Filled` slot was something else before"), a `Free` word that is non-zero and sealed
  in another state (`sealed_as`, `:822-833`), a `Placed` block failing its CRC, an
  unknown kind, a hole record that does not decode (`:816-922`). A `Free` word that is
  non-zero, does not verify and is sealed as no other state (a `fill` killed between
  its field stores and its word store; the comment at `:825-826` names the case) is
  **neither noted nor rewritten** (`:816-833` has no branch for it) and keeps its lap:
  4.3 step 3 and pitfall 21. On a dirty boot any damage means an acknowledged write
  may be gone, and the engine treats the boot like a power loss (`:495-498`,
  `:525-531`; E16 `docs/07:367-370`); a clean boot holds nothing, as after a power
  loss that followed a clean shutdown, and loses nothing, since its selected CP
  carries whatever the arena held (5.0, the end of (c)). The engine does so only in
  its hold: `assemble` still replays every verified
  `Placed` run of a present volume (`engine.rs:1820-1864`) before it adds the hold
  (`:1866-1885`), where a power loss leaves a blank arena and replays nothing (5.0
  case (c)), which neither E16 nor the doc of `DamagedSlot` (`arena:497-498`) says.
  A window wider than the ring is `Inconsistent` and the arena is recreated
  (`arena:927-931`; `engine.rs:1690-1694`).
- **A7. The header is sealed once and never rewritten**; only `attach_word`, outside the
  seal, is updated (`:943-946`). `verify` checks the **major** version only (`:211`),
  so a minor bump is attachable by older code and vice versa.
- **A8. Single writer**: every method takes `&self`/`&mut self` (`:72-74`; E16
  `docs/07:375-379`); on Linux the file also carries an exclusive `flock` for the mapping's
  life (`hugetlb.rs:346-355`, `:371-389`).
- **A9. Hole records** (E23 `docs/07:670-702`): one slot per punch, `kind = Hole`,
  placed, committed and re-attached like data (`arena:1117-1121`); `decode` demands the
  reserved fields and the tail of the block be zero (`:324-331`).
- **A10. Invariant 3 of section 7.7**: every byte under a CRC (`:55`); the descriptor's
  CRC covers the fields and the state/lap word, the block's CRC is recorded at fill.

Index (E20, `docs/07:537-556`, and the code):

- **I1. Insert order is sequence order per volume**: `Index::insert` refuses a sequence
  below the volume's newest, equal is allowed (`index/src/lib.rs:575-581`; the code's
  comment at `:573-574` reads "Equal is fine: a write whose run was split by a later
  overwrite comes back from a re-attach as several pieces with one sequence", and
  `:31` and `:550` say "another piece of the same write"; the longer sentence "equal is
  another piece of the same write, as a re-attach produces when a later overwrite split
  a run" is E20's, `docs/07:549-551`).
- **I2. The later insert wins its whole range**; displaced data pieces are returned for
  release, a displaced hole returns nothing (`:231-237`, `:262-267`); a merged extent's
  sequence is the newest write it holds (`:375`; E20 `docs/07:552-554`).
- **I3. Coalescing** happens only for data runs whose slots continue each other on the
  same lap, or holes sharing one record slot (`:361-369`).
- **I4. Generation 1 is active after `Index::new`** (`:531`); a lookup answers from the
  active generation first, then frozen newest first (`:599-613`); across generations
  the newer wins because that is the order CPs reach the disk (`:33-35`).
- **I5. Never cut from inside `with_active`** (`:615-617`, `:17-20`).

Engine (E17 `docs/07:383-464`, and the code; the boot's own checks, E30, are B1 and B2 below):

- **E1. The write path order** is reserve, copy in, CRC, sequence, fill, insert, place,
  release superseded, acknowledge (`engine.rs:3495-3550`); `fill` precedes `insert`
  and `place` precedes every acknowledgement (section 4.1).
- **E2. Four acknowledgement paths**: immediate (`:3627`), paced (`:2142`), after an
  intent (`:3767`), after the CP that carried the sequence (`:5914-5922`). On a stop
  `refuse_everything` completes paced writes `Ok` and intent-waiting ones `Stopped`
  (`:3390-3398`).
- **E3. Writes and punches are FIFO among themselves; reads and flushes pass deferred
  writes** (`:3349-3352`, `:3364-3368`; the checker relies on it, `model.rs:14-16`).
- **E4. The dirty-boot hold**: `hold = dirty && (!arena_whole || rewound || arena_lost
  || orphaned_blocks > 0)` with `arena_whole = reattach.is_some_and(|r|
  !r.is_damaged())` (`:1869-1870`); it holds every volume of `boot_write_active`
  (`:1871-1884`) with cause `TornCp` after a rewind, else `DirtyBoot` (`:1873-1877`).
  E17 `docs/07:422-429` says the same in words.
- **E5. The boot commit carries no generation** (`:1713`: `new_job(CpTrigger::Boot,
  None, …)`), so replayed writes stay in active generation 1 until a trigger cuts
  (`:3888-3909`); the engine serves only after the boot CP's B slot lands
  (`:5580-5583`, `:5978-5983`).
- **E6. Arena slots of a generation are released in `finalize`, after B lands**
  (`:5873-5893`; E17 `docs/07:393-397`); a run superseded inside the active generation is
  released at once (`engine.rs:3546-3548`; E16 `docs/07:372-374`).
- **E7. A flush is acknowledged without disk I/O** (`:3481-3484`; `traits.rs:289-294`):
  the arena is the durable copy against a daemon crash, power loss is nvfail's business
  (E17 `docs/07:399-429`).
- **E8. Bounds**: a write above `MAX_REQUEST_BLOCKS` or above `arena.slots()` is
  `TooLarge` (`:3434-3437`); a punch above `MAX_PUNCH_BLOCKS` likewise (`:3459-3462`);
  the admission rule and the stall (`:3438-3450`) come before the arena is touched.
- **E9. Precise mode**: a write to a volume outside `durable_active` waits for an intent
  before its acknowledgement (`:3611-3617`; E17 `docs/07:399-421`).
- **E10. Arena damage while running**: a read answers `Corrupt` (`:3678-3681`); the
  stripe builder writes the block under the recorded CRC and reports `ArenaDamage`
  (`:4491-4501`; E17 `docs/07:454-458`).
- **E11. `tick` order** (`:2076-2118`): completions, CP state machine, intents,
  discards, boot retry, paced acknowledgements, the front door (only while
  `serving && !stopped`), cut, start CP, pump.

Boot (E30 `docs/07:901-936`, and the code):

- **B1. A read error fails the boot; only a region that reads back and does not verify
  tears a candidate.** `boot::evaluate` returns `BootError::Io` on a read error while
  loading a candidate, to be retried, instead of marking it torn (`boot.rs:292-295`;
  the stripe reads map to `TornReason::Io` at `:644`; E30 `docs/07:918-921`). So
  `rewound` (`boot.rs:251-253`) means only that a newer candidate read back and did
  not verify. `evaluate` runs before the arena is touched (`engine.rs:1665-1668`; the
  attach is at `:1687`), and every failure before the attach hands the memory back
  unchanged (`fail!`, `:1644-1654`). The daemon does not retry: `Pool::boot` turns the
  failure into a `ControlError` and drops the arena memory with the `BootFailure`
  (`pool.rs:245-257`; a hugetlbfs file keeps its contents and is mapped again by the
  next boot), and a configured `dbladed` exits (`daemon/src/daemon.rs:247-251`,
  `:263-265`; `daemon/src/bin/dbladed.rs:170-175`). A recovery must retry the boot (in
  the daemon or through its supervisor); meanwhile the devices stay `QUIESCED` with
  their requests requeued. That retry is safe unless the failed boot held volumes
  (`hold`, `engine.rs:1869-1870`) and stopped after its attach, before its boot
  commit's `A` slot landed. A dirty boot that held nothing (whole arena, no rewind, no
  lost arena, no orphan) and failed after the attach has rewritten only the dead
  process's reservations and `Filled` slots (`arena:816-821`, `:835-848`), the runs its
  replay superseded (`engine.rs:1839-1845`; a later replayed run covers their blocks)
  and the attach counter (`arena:943-947`), and has created no arena, so its retry
  re-attaches the same `Placed` slots and is as safe as a retry after a failure before
  the attach. A clean boot holds nothing (the hold needs `dirty`, `engine.rs:1870`),
  and whatever it finds `Placed` the selected CP already carries: a clean shutdown's
  last `finalize` leaves nothing `Placed`, and a kill between its final CP's `A` slot
  and that `finalize` leaves only the final generation's slots (pitfalls 23 and 13). A
  holding boot's failure after the attach, a `start_job` error (`engine.rs:1714-1725`,
  which hands back memory the attach has already rewritten or `Arena::create` has
  already overwritten, `:1687-1709`) or `Pool::boot` giving up while it waits for the
  boot commit (`pool.rs:280` → `wait_until`, `:523-556`: the stop flag, `--op-timeout`
  or a tick error), drops a hold the failed boot had only in memory: the retry, like
  any later boot, finds nothing to hold on (5.0, the gap; pitfall 22).
- **B2. Unless the selected CP is a clean shutdown's final one, a trusted opaque barrier
  makes the boot read and CRC one CP's data, after a `kill -9` too.** `check_data =
  trusts_opaque_barrier() && !is_clean_shutdown()` (`boot.rs:629`;
  `MediaProfile::trusts_opaque_barrier`, `traits.rs:218-220`): every stripe the
  selected CP wrote is then read whole instead of its summary only (`boot.rs:632`,
  `:644`) and every data block is checked against its descriptor's CRC (`:654-672`).
  The test is the selected slot's run state (`is_clean_shutdown`,
  `crates/otap-format/src/superblock.rs:682-686`), not the way the process stopped.
  The engine writes `RunState::CleanShutdown` only into the final CP of a clean
  shutdown: a slot takes its job's run state (`engine.rs:5607`), which is
  `CleanShutdown` only for the generation the `Shutdown` cut froze (`:3957-3961`) and
  `Running` for every other job, the boot commit's included (`:1713`, `:2105`,
  `:2188`); format's CP 0 is written `CleanShutdown` too (`:1616`) and maps nothing
  (`:1607`). So a `kill -9` leaves the selected slot without `CleanShutdown` and the
  check runs after a daemon crash too, except a `kill -9` that leaves a clean
  shutdown's final CP the newest on disk: one after that CP's `A` slot landed (from
  then on a boot selects it, pitfall 23), or one during the next boot before its own
  commit's `A` slot landed. The boot after it is clean (`dirty` is false,
  `engine.rs:1767`) and reads the stripe summaries only. That is harmless: a `kill -9`
  costs the device nothing, and the CP's data were flushed before its slot was
  written (4.5 step 4). E30's "at a boot after a power loss only" (`docs/07:915-918`)
  understates the code's condition all the same. This is boot time inside the
  recovery budget (pitfall 5, open question 6).

Front door and kernel (E28 `docs/07:827-873`, `ublk_drv.c`):

- **F1. The tag generation advances at every completion and at the abort of an
  `InEngine` tag**, so a stale completion is dropped: `complete` always advances it
  (`otap-ublk/src/frontdoor.rs:511`, after the check at `:505-508`), the `Abort` arm of
  `pump` only for a tag the engine holds (`:378-383`). An abort of a `Ready` or `Idle`
  tag leaves it unchanged, which is safe because no `ReqId` was handed out for that use
  of the tag. Both docs overstate the abort case: the doc of `request_id`
  (`:209-211`) says "The generation advances every time the tag completes or is
  aborted", and E28 (`docs/07:844-845`) "the generation advancing at every completion
  or abort". It is process memory.
- **F2. Every command of a queue must come from the task whose fetch completed the
  queue.** `ublk_mark_io_ready` pins `ubq_daemon = current` only when `nr_io_ready`
  reaches `q_depth` (`ublk_drv.c:1624-1626`; `ublk_queue_ready`, `:1499-1502`); until
  then `ubq_daemon` is `NULL` and the task check (`:1706-1707`) lets any task through.
  The `otap-sys` doc (`otap-sys/src/ublk.rs:1296-1298`: "issued from the thread that
  issued its first fetch"), `take_transport`'s doc (`device.rs:240-241`: "the driver
  pins the queue to the task that issued its first fetch") and E28
  (`docs/07:863-864`: "a queue's commands must come from the task that issued its
  first fetch") all name the issuer of the first fetch. No difference today, since
  one thread pushes and submits every fetch of the queue (`open_queue`,
  `device.rs:166-177`); a recovery must likewise issue the fetches from the thread
  that will serve the queue (5.1 item 4, 4.7 step 4). In the daemon that is the
  engine thread: devices are created and started there (E38 `docs/07:1352-1354`;
  `create_device` runs on the calling thread, `crates/otap-daemon/src/device.rs:8-11`),
  and so is `Pool::boot`, where 5.1 item 4 puts the recovery
  (`daemon/src/daemon.rs:245-247`, `:108`).
- **F3. A commit is only pushed** into the SQ ring (`device.rs:325`); it reaches the
  kernel at the next `poll` (`:284`) or when the ring is full (`:328`). Those are the
  only submits while serving (the others are the creation's, `:174`, and
  `LinuxTransport::wait`, `:277-279`, which nothing calls), and the second never
  fires: the ring has twice the depth in entries (`:168`; `otap-sys/src/ublk.rs:1337`)
  and at most one commit per tag is pushed between two submits, since a tag gets its
  next request only once its `COMMIT_AND_FETCH` was submitted. `poll` is reached only
  from `UblkFrontDoor::fetch` with nothing `Ready` (`otap-ublk/src/frontdoor.rs:439-441`,
  the only call of `UblkFrontDoor::pump` outside the tests); with a tag `Ready` and the
  gate closed (`in_engine >= high`, or `gate_closed` and `in_engine > low`,
  `:442-451`; `for_depth` sets `high = depth`, `low = depth / 2`, `:90-96`) `fetch`
  returns `None` having submitted nothing (pitfall 2).
- **F4. The `otap-ublk` crate doc is stale on recovery.** It says the front door "has no
  recovery path for `UBLK_F_USER_RECOVERY` yet" and that "a new daemon re-attaching the
  queue is `dbladed`'s, Step 5" (`crates/otap-ublk/src/lib.rs:31-36`), while E38 lists
  the re-attach as not done (`docs/07:1419-1423`). Scope A should rewrite the sentence;
  its "FUA semantics on the medium are a Step 5 policy"
  (`crates/otap-ublk/src/lib.rs:32-33`) is stale the same way (`docs/06:558-560` calls
  it a later policy).
- **K1. A request handed to the server has `ACTIVE` clear and `OWNED_BY_SRV` set**
  (`ublk_drv.c:1072-1086`). After the daemon's death `ublk_abort_queue` marks every such
  started request `ABORTED` and calls `__ublk_fail_req` (`:1387-1408`), which requeues
  it under `REISSUE` and otherwise drops the reference (`:1061-1070`) so that
  `__ublk_complete_rq` ends it with `BLK_STS_IOERR` (`:991-1002`). What runs
  `ublk_abort_queue` is the cancel of a still-pending uring_cmd at the exit
  (`:1464-1497`) when any tag is `ACTIVE`, and otherwise the block layer's request
  timeout (`ublk_timeout`, `:1209-1248`, when `nr_inflight == q_depth`, `:1235`): 4.7
  step 1. A request that arrives while the queue is `canceling` is requeued under
  `USER_RECOVERY` and failed without it (`:1090-1098`, `:1274-1277`).
- **K2. `START_USER_RECOVERY` (`ublk_ctrl_start_recovery`, `:2694-2736`) requires the
  char device closed (`UB_STATE_OPEN` clear: the dead process fully exited) and the
  device `QUIESCED`** (`:2719-2723`); it then calls `ublk_queue_reinit` (`:2669-2692`)
  once per queue (`:2725-2726`), which resets every io (`flags = 0`, `cmd = NULL`,
  `addr = 0`), `nr_io_ready = 0`, `ubq_daemon = NULL`, `timeout = false` and
  `canceling = false`; `ublk_ctrl_start_recovery` itself then sets `ub->mm = NULL`
  (`:2728`), `nr_queues_ready = 0` (`:2729`) and re-initialises the completion that
  `END` waits on (`:2731`). Before the `EBUSY` test it answers `EINVAL` when the
  device lacks the recovery flag (`:2702-2703`) or has `nr_queues_ready == 0`
  (`:2704-2705`). That counter is not about `START_DEV`: `ublk_mark_io_ready` raises it
  when every tag of a queue has had a `FETCH_REQ` (`:1624-1628`), and only
  `START_USER_RECOVERY` clears it (`:2729`). A device that was never started is `DEAD`,
  never `QUIESCED`: `LIVE` is set only by `START_DEV` (`:2273`) or `END_USER_RECOVERY`
  (`:2769`), `QUIESCED` only by `__ublk_quiesce_dev` (`:1555`), which runs only on a
  `LIVE` device (`:1563-1566`, `:1601-1602`), and without a disk the exit's cancel
  schedules no work at all (`:1428-1430`, `:1491-1496`); `ADD_DEV` copies the caller's
  `dev_info` (`:2434`), whose `state` OpenTAP leaves 0, `DEAD`
  (`crates/otap-ublk/src/device.rs:90-97`). `START_USER_RECOVERY` on such a device
  answers `EBUSY` every time (`ublk_drv.c:2719-2722`; its count is already 1, because
  `open_queue` fetched every tag at create, `device.rs:166-177`), or `EINVAL` while
  those fetches are unprocessed (`ublk_drv.c:2704-2705`), whether the creator is still
  before `open_queue`'s submit (`device.rs:174`) or died before it. A `QUIESCED`
  device with `nr_queues_ready == 0` is a recovery **in progress or interrupted**:
  every successful `START` zeroes the counter (`ublk_drv.c:2729`), only a queue's last
  processed `FETCH_REQ` raises it again (`:1624-1628`), and `LIVE` comes only at `END`
  (`:2769`), so from a successor's `START` until its last fetch is processed the
  device looks the same whether that successor is alive or died in the window. Every
  other `START` on it answers `EINVAL` (`:2704-2705`, tested before the `EBUSY` at
  `:2719`). An interrupted one can, on 6.12, only be reaped, and `STOP_DEV` then fails
  its requeued requests (`:1572-1591`, `:1271-1272`); one in progress belongs to its
  successor, whose requests the same `STOP_DEV` would fail. Userspace cannot read the
  counter (it is not in `ublksrv_ctrl_dev_info`, `crates/otap-sys/src/ublk.rs:443-472`);
  what it sees in both cases is a `QUIESCED` device whose `START` answers `EINVAL`,
  and a probe open of `/dev/ublkcN` (`EBUSY` while another process holds it,
  `ublk_drv.c:1306-1307`) cannot tell the two apart before the successor's own open;
  a probe open that succeeds even makes that open fail `EBUSY` until the probe closes.
- **K3. `END_USER_RECOVERY` (`ublk_ctrl_end_recovery`, `:2738-2774`) waits until every
  queue has a fetch on every tag** (`wait_for_completion_interruptible`, `:2748`;
  completed by `ublk_mark_io_ready`, `:1621-1636`, when the last queue's last tag has
  a processed `FETCH_REQ`, `:1633-1634`, reached from the `FETCH_REQ` case at
  `:1761`), requires `QUIESCED` (`:2758-2761`), records the new pid (`:2762`),
  unquiesces and kicks the requeue list (`:2765-2768`), and sets `LIVE` (`:2769`).
- **K4. The flags are fixed at `ADD_DEV`**: the whole user `dev_info` is copied
  (`:2434`), the queue copies the flags at creation (`:2011`), and
  `ublk_queue_can_use_recovery_reissue` reads the queue's copy (`:675-680`). The
  recovery flags are stripped only for `UBLK_F_UNPRIVILEGED_DEV` (`:2383-2385`).
  `ublksrv_flags` is never read by the driver and comes back from `GET_DEV_INFO`
  (`:2578`).
- **K5. A re-dispatched request carries no marker**: `ublk_setup_iod` (`:937-977`)
  fills `op_flags`, `nr_sectors` and `start_sector` from the request (`:971-973`) and
  `addr` from `io->addr` (`:974`), the buffer address the server passed with the tag's
  last fetch or commit (`ublk_fill_io_cmd`, `:1660-1666`, from the fetch at `:1760` or
  the commit at `:1786`; `NEED_GET_DATA`, `:1792`, which OpenTAP does not use, sets it
  too), which `ublk_queue_reinit` resets to 0 (`:2690`) and which is always 0
  under `USER_COPY`: a non-zero one is refused (`:1754-1758`; `:1777-1784`, zone
  append excepted), and OpenTAP's fetches and commits pass 0
  (`otap-sys/src/ublk.rs:850-876`). Nothing in the descriptor tells a re-dispatch
  from a first dispatch.

Tests (E09 `docs/07:205-223`, E18 `docs/07:466-506`, `docs/09:126-136`):

- **T1. `PROPTEST_CASES` overrides an explicit `cases`** in a `proptest!` configuration
  (`docs/09:134-135`); verified in proptest 1.12.0: the `proptest!` macro passes the
  test's config through `contextualize_config` (`sugar.rs:160`, `:175`, `:226-244`),
  which applies the environment (`config.rs:20-90`, `PROPTEST_CASES` at `:26`, `:86-87`).
- **T2. `prop_assert!` always carries a message** (E09 `docs/07:211-213`).
- **T3. After a daemon crash or a clean restart no acknowledged write may be lost and
  nvfail must be unchanged** (`harness.rs:1284-1296`; E18 `docs/07:487-489`). The
  harness makes no allowance for two windows whose CP's `A` slot landed before a
  daemon crash (5.0, the paragraphs after the guarantee). A clear legitimately
  releases its volume at the restart, which the nvfail check counts as a changed
  nvfail (`harness.rs:1291-1296`). A deletion, of any volume, held or not,
  legitimately removes it, but the harness's boot recreates it empty from the
  configuration, which forgets a volume only on `VolumeDeleted` (`:1080-1081`), so
  its acknowledged writes fail the image check before the nvfail check is reached
  (`check_prefix` refuses the image, `:1269-1274`, or `lost_volumes` counts them
  lost, `:1275`, `:1285-1290`), unless the model held only zeros for it. No test
  reaches either window today.

## 4. How things happen, in order

### 4.1 A write, from fetch to acknowledgement (`engine.rs`)

1. `tick` reaches `serve_front_door` only while `serving && !stopped` (`:2109-2110`).
   Deferred writes are retried first, in order, and the first that still does not fit
   stops the retry (`:3354-3359`); then up to 64 fetches (`:3360`), a write or punch
   behind a deferred one is deferred too (`:3364-3368`).
2. `serve` (`:3402-3486`): the volume must be present and not creating (`:3404-3412`,
   else `NoSuchVolume`), online (`:3414-3417`, else `Offline`), not held
   (`:3418-3421`, else `NvFailed`); the range must fit (`:3422-3433`, else
   `OutOfRange`); `blocks <= MAX_REQUEST_BLOCKS` and `<= arena.slots()` (`:3434-3437`,
   else `TooLarge`); the pool not full and the reserve respected (`:3438-3444`, else
   `NoSpace`); below the stall occupancy (`:3448-3450`, else handed back to be deferred,
   unplaced).
3. `serve_write` (`:3488-3552`): `arena.reserve(blocks)` (`:3495`; `Full` → deferred,
   `LapExhausted` → `Failed`).
4. `front.copy_in(&req, arena.run_mut(ring, blocks))` (`:3507-3510`): the client's
   bytes land in `Free` slots. A failure releases the run with `arena.discard`
   and answers `Failed` (`:3511-3516`).
5. CRC-32C per 4 KiB block over the run (`:3517-3524`).
6. `seq = arena.next_seq()` (`:3525`), then `arena.fill(ring, vol, lba, seq, &crcs)`
   (`:3526`): every slot becomes `Filled`, one word store each.
7. `index.insert(vol, lba, DirtyExtent::data(ring, blocks, seq))` (`:3527-3529`). On
   `BadExtent`/`BadVol` the run is discarded and the answer is `OutOfRange`
   (`:3533-3538`); any other index error (which includes `StaleSeq`) is an internal
   failure of `tick` (`:3539`).
8. `arena.place(ring, blocks)` (`:3543`): `Placed`, the write is in the index and may be
   acknowledged.
9. Every run the insert superseded inside the active generation is released at once:
   `arena.commit(old, old_blocks)` (`:3546-3548`).
10. `stats.writes += 1` (`:3549`); `acknowledge(req, vol, seq)` (`:3550`).
11. `acknowledge` (`:3607-3628`): `windows = 2` (`:3608-3610`); in precise mode a
    volume outside `durable_active` parks the request in `pending_acks` and asks for an
    intent (`:3611-3617`); a pacing delay (or an earlier paced write) parks it in
    `paced` with a due time (`:3618-3626`); otherwise `front.complete(req, Status::Ok,
    seq)` (`:3627`).
12. The parked acknowledgements leave later: `complete_paced` on a later tick
    (`:2135-2144`), `handle_intent_done` when the intent lands (`:3762-3768`), or
    `finalize` when a CP carried the sequence (`:5914-5922`).
13. On Linux `front.complete` is `DaemonFrontDoor::complete` → `UblkFrontDoor::complete`
    (`otap-ublk/src/frontdoor.rs:499-518`) → `answer` → `LinuxTransport::commit`
    (`device.rs:324-340`) → `UblkQueue::push_commit` (`otap-sys/src/ublk.rs:1391`), an
    SQE in the ring. The kernel processes it only at the next `LinuxTransport::poll`
    (`device.rs:284`, `submit()`), where `ublk_ch_uring_cmd` (`ublk_drv.c:1864-1878`)
    → `ublk_ch_uring_cmd_local` (`:1838-1856`, copies the command words) →
    `__ublk_ch_uring_cmd` (`:1683`), whose `UBLK_IO_COMMIT_AND_FETCH_REQ` case
    (`:1763-1788`) runs `ublk_commit_completion` (`:1787`, `:1358-1380`), and the block
    layer sees the write complete (`:1379` → `:1034-1037`).

### 4.2 A punch, a read, a flush (`engine.rs`)

- `serve_punch` (`:3556-3602`): `reserve(1)` (`:3563`), `seq` (`:3575`), `fill_hole`
  (`:3576`), `index.insert(DirtyExtent::hole(ring, blocks, seq))` (`:3577-3593`),
  `place(ring, 1)` (`:3594`), release superseded data runs (`:3595-3597`), remember
  the record slot under its generation in `gen_holes` (`:3598`), `acknowledge`
  (`:3600`). The record slot is released when the generation's CP commits
  (`:5883-5893`).
- `serve_read` (`:3630-3662`): per block `read_block` (`:3667-3688`): `index.lookup`
  first (a hole reads as zeros `:3674-3677`; a data hit is CRC-checked with
  `check_data` and copied from `arena.run` `:3678-3685`), else the committed map
  (`:3687`, `:3691-3736`); then `copy_out` and `complete` (`:3657-3660`).
- Flush: `complete(req, Ok, 0)` at once (`:3481-3484`).

### 4.3 `Arena::attach` (`arena/src/lib.rs:766-949`)

1. `can_attach`: header magic, major version, CRC, geometry, aggregate (`:770-772`,
   `:749-761`).
2. A fresh `Arena` with `head = tail = 0`, `next_seq = 1` (`:782-790`), then
   `scan_on_attach` (`:791`).
3. One pass over every slot (`:808-924`). For each: `max_seq = max(max_seq, desc.seq)`
   whatever the state (`:810`); `valid = desc.verify()` (`:811`); `pos = lap * slots +
   slot` (`:813`).
   - `Free`: valid → a reservation of the dead process, noted in the window and
     rewritten `Committed` (`:816-821`); word non-zero and `sealed_as()` is some →
     damage (`Descriptor`), rewritten `Committed` (`:822-833`); **any other word is
     left alone**, neither noted nor rewritten (`:816-833` has no further branch).
     That is a zero word (a never-used slot, `:714`, `:825`) but also a non-zero word
     that neither verifies nor is sealed as another state: a `fill` killed between
     the field stores and the word store (`:1108-1112`; the comment at `:825-826`
     names it). Such a slot keeps `Free` and its reservation lap; when a noted slot
     lies beyond it (the run's later slots are `Free`-valid, `:820`) the head passes
     it (`:925`), and when a kept `Placed` slot lies before it (`tail = min_placed`,
     `:926`) it stays inside `[tail, head)`, where `passable` never passes a `Free`
     slot of the tail's own lap (`:1278-1282`): pitfall 21. With no kept `Placed` slot
     before it, the tail starts past it (at the first kept `Placed` slot beyond it, or
     at `head`, `:926`) and the slot lies behind the window, where a later `reserve`
     re-seals it when the head comes round.
   - `Filled`: valid → `dropped_filled += 1`, noted (`:835-838`); invalid → damage
     (`:839-847`); either way rewritten `Committed` (`:848`).
   - `Placed`: invalid word → damage, `Committed` (`:851-859`); data CRC mismatch →
     `Damage::Data`, `Committed` (`:861-869`); kind `Hole` decoded with
     `HoleRecordBlock::decode` (`:872-889`, failure → `Damage::Kind`); unknown kind →
     `Damage::Kind` (`:890-898`); otherwise a `PlacedSlot { ring, vol, lba, seq,
     crc32c, hole_blocks }` is kept (`:900-907`) and `min_placed` tracked (`:908`).
   - `Committed`: valid → counted (`:911-913`); invalid → damage, rewritten (`:914-921`).
4. `head = max_pos + 1`, `tail = min_placed` or `head` (`:925-926`); a window wider
   than the ring is `Inconsistent` (`:927-931`).
5. `next_seq = max_seq + 1` (`:932`).
6. The queued rewrites are applied (`:933-935`) and the tail advanced (`:936`).
7. `placed` is sorted by `(seq, lap * slots + slot)` (`:937-942`).
8. The attach counter is bumped by one 8-byte store (1 after a torn word) (`:943-947`).

### 4.4 `Engine::boot` and the replay (`engine.rs:1637-2071`)

1. Flush policy, config, layout, `boot::evaluate` (the superblock ring, torn
   candidates, the map) (`:1655-1668`); configured sizes must match the roots
   (`:1672-1685`). A read error while loading a candidate fails the boot here
   (`BootError::Io`, `boot.rs:292-295`), before the arena is touched, and the memory
   is handed back (`fail!`, `engine.rs:1644-1654`; B1); under a trusted opaque barrier
   the candidate's data blocks are read and CRC-checked here too, unless it is a clean
   shutdown's final CP, which a `kill -9` once that CP's `A` slot landed also leaves
   selected (B2).
2. The arena (`:1686-1709`): `Arena::can_attach` ok → `Arena::attach`; an
   `Inconsistent` table makes the arena lost and recreated (`:1690-1694`);
   `WrongAggregate` fails the boot (`:1696-1701`); any other refusal recreates the
   arena with `arena_lost = !is_blank` (`:1702-1708`), so blank memory (a power loss
   or a first boot) is not "lost".
3. `assemble` (`:1710`): `dirty = !sb.is_clean_shutdown() || rewound` (`:1767`);
   `VolRt` from the volume table (`:1770-1785`) and the configuration (`:1786-1805`;
   a volume the disk lacks is created with the boot commit, held `TornCp` after a
   rewind).
4. `Index::new(cfg.ring, now)` (`:1814`): generation 1 active, `newest_seq` zero.
5. The replay (`:1820-1864`): for each `(vol, lba, extent)` of `runs_of(&report.placed)`
   in `(seq, position)` order: a volume not present → `orphaned_blocks += blocks`
   and `arena.commit(ring, slots)` releases it (`:1823-1833`); else
   `index.insert(vol, lba, extent)` (`:1834`); on success `windows = 2`
   (`:1836-1838`), superseded runs released (`:1839-1845`), a hole's record slot
   remembered in `gen_holes[gen]` (`:1846-1848`), `replayed_blocks += blocks`
   (`:1849`); on an insert error the run is orphaned and released (`:1851-1861`).
   `StaleSeq` cannot occur here because the input is sorted by sequence and equal
   sequences are allowed; `BadVol`/`BadExtent` cannot either, because `fill` and
   `fill_hole` checked the same bounds (`arena:1101-1103`, `:1131-1133`), and an
   absent volume is today only a rewind's doing (pitfall 9).
6. The hold (`:1866-1885`): see E4. `nvfail_now` is collected for the event
   (`:1886-1891`); `intent_seq` from the slots (`:1892-1897`); `durable_active` =
   `boot_write_active(&sb, intents)` (`:1871`, `:2048`).
7. `Event::Booted { … replayed_blocks, damaged_slots, orphaned_blocks, nvfail … }` is
   the first event (`:1986-2000`); `serving = false` (`:2056`); the `Reattach` report
   is dropped at the end of `assemble` (nothing keeps `report.placed`).
8. Back in `boot`: `new_job(CpTrigger::Boot, None, RunState::Running)` and `start_job`
   (`:1713-1714`); the boot CP carries no generation; `serving` becomes true in
   `finalize` once its B slot landed (`:5978-5983`). Until then `tick` serves nothing
   (`:2109-2110`), but `maybe_cut` runs every tick and may cut generation 1 on
   `ArenaHalf` or `Timer` (`:3901-3904`), the only triggers that can fire before
   serving (pitfall 15). A failed boot commit does not end the boot: `fail_cp` sets
   `boot_retry` when the failed job is the boot CP (`:4295-4299`), and `tick` issues a
   new boot job once the engine is `Idle` and `!pool_full || pool_retry`
   (`:2099-2107`). A failed flush or slot write (`:5519`, `:5570`) is retried in the
   same tick (`fail_cp` leaves the engine `Idle`, `:4312`, and the retry check follows
   `advance_cp`, `:2096`); a failure for want of space sets `pool_full` (`:4300-4301`), and
   then only `request_cp` (`:2122-2125`) lets the retry run, which nothing calls while
   `Pool::boot` waits (`wait_until` only ticks, `pool.rs:523-556`), and no control
   request can reach the engine before `Pool::boot` returns (4.6 step 1). Meanwhile
   `serving` stays false, and the wait ends at `--op-timeout` (the deadline, `pool.rs:528`;
   120 s by default, `daemon/src/config.rs:97`), dropping a hold the boot had only in
   memory (5.0, the gap).

### 4.5 How a replayed (or any) run leaves the arena (`engine.rs`)

1. A trigger cuts the active generation (`:3874-3929`; `index.cut` at `:3913`).
2. `maybe_start_cp` → `new_job` lists every data block of the frozen generation as a
   `StagedBlock { vol, lba, ring, seq }` and every hole, with `max_seq` (`:3977-3995`).
3. `step_staging` builds full stripes from the arena (`:4353-4419`; `build_stripe`
   reads `arena.descriptor(slot).crc32c` and `arena.run(ring, 1)` at `:4491-4492`).
4. Metadata (`step_meta` `:5451-5491`), the pool flush (`step_flush` `:5493-5523`),
   then the superblock slot at `A` and then at `B` (`step_commit` `:5525-5587`: the
   slot is built for `A` at `:5541`, `B` is written once `A` landed, `:5571-5578`, and
   `finalize` runs once `B` landed, `:5580-5582`).
5. `finalize` (`:5711-5995`): `arena.commit` per staged block (`:5873-5882`) and per
   hole record of the generation (`:5883-5893`), `index.retire` (`:5894-5898`),
   windows (`:5902-5909`), acknowledgements waiting for an intent whose sequence the CP
   carried (`:5914-5922`), `serving = true` on the boot trigger (`:5978-5983`).

### 4.6 The daemon: boot, devices, and where the kernel enters

1. `Pool::boot` (`pool.rs:209-295`): `probe_superblock`, `open_backstore`,
   `open_arena(&spec.arena, spec.arena_bytes)` (`:241`; a hugetlbfs file is mapped
   with its contents by `HugeArena::map_file`, `hugetlb.rs:324-395`), `slots_for_len`
   (`pool.rs:242`), `PoolEngine::boot(cfg with no volumes, backstore, DaemonFrontDoor::new(),
   clock, memory)` (`:245-251`), `wait_until(is_serving)` (`:280`), then
   `ensure_device(vol)` for every online volume (`:281-293`), which today always
   creates a new device (`:687-694`). A failed engine boot becomes a `ControlError`
   (`:252-257`) before any device is touched (B1). The arena file's `flock` is taken
   without blocking, so a restart before the dead daemon has fully exited fails at
   `pool.rs:241` (pitfall 24). In `dbladed` all of this runs on the engine thread
   before the control server answers anything: the socket is bound first
   (`daemon/src/bin/dbladed.rs:162`), but `serve` starts only once `Daemon::spawn`
   has returned (`:170-177`), which waits on `boot_rx` (`daemon/src/daemon.rs:261`)
   until `boot_configured`, and so `Pool::boot`, has run (`:245-250`, `:108`). The
   attach's CRC pass, the boot commit (and its retries, 4.4 step 8) and any recovery
   handshake placed in `Pool::boot` (5.1 item 4) therefore delay `Ping` and every
   `otapctl` call; an `EBUSY` retry of `START_USER_RECOVERY` can last the block
   layer's 30 s timeout (pitfall 6). After the boot the engine thread takes at most
   one control call per loop (`daemon/src/daemon.rs:278-318`), waiting 200 µs to
   2 ms for one when idle (`idle_wait`, `:325-331`).
2. A device (`create_device`, `crates/otap-daemon/src/device.rs:12-45`, called by
   `Pool::ensure_device`, `pool.rs:687-693`), in this order: `UblkDevice::create`
   (`crates/otap-daemon/src/device.rs:20` → `device.rs:88-125`) issues `ADD_DEV`
   (`USER_COPY | USER_RECOVERY | CMD_IOCTL_ENCODE`) and `SET_PARAMS`, opens
   `/dev/ublkcN` and submits a `FETCH_REQ` on every tag (`open_queue`, `:166-177`);
   then `front_door` takes the queue and wraps it in a `UblkFrontDoor`
   (`crates/otap-daemon/src/device.rs:26` → `device.rs:344-360`; its doc, `:343`,
   says "of a started device", but the daemon builds it before the start); only then
   does `start` issue `START_DEV` (`crates/otap-daemon/src/device.rs:36` →
   `device.rs:184-190`), and `Pool::ensure_device` registers the device last
   (`DaemonFrontDoor::add_device`, `pool.rs:694`). So the front door exists before
   `START_DEV`, and nothing polls the queue until `add_device`.
3. Serving: `DaemonFrontDoor::fetch` round-robins the devices (`daemon/src/frontdoor.rs:278-296`);
   `UblkFrontDoor::fetch` pumps the transport when nothing is ready
   (`otap-ublk/src/frontdoor.rs:439-441`), which submits the pushed commits and reaps
   completions (`device.rs:283-306`).

### 4.7 The kernel at the daemon's death and at recovery (`ublk_drv.c`, 6.12)

1. The process dies. What happens next has two triggers, depending on whether any
   tag still has a pending uring_cmd (`ACTIVE`: a `FETCH`/`COMMIT_AND_FETCH` the
   kernel has not yet answered):
   - **Some tag is `ACTIVE`** (the daemon had not been handed every tag). The io_uring
     context's teardown cancels each pending uring_cmd: `ublk_uring_cmd_cancel_fn`
     (`:1464-1497`) → `ublk_abort_requests` (`:1410-1440`): `canceling = true`
     (`:1419`), quiesce the blk-mq queue (`:1433`), `ublk_abort_queue` (`:1435`),
     unquiesce (`:1436`); then `ublk_cancel_cmd` completes the pending fetch with
     `UBLK_IO_RES_ABORT` (`:1442-1458`), and with recovery `quiesce_work` is scheduled
     (`:1491-1494`). This runs at the exit.
   - **Every tag is owned by the daemon** (handed over and not committed:
     `OWNED_BY_SRV` set, `ACTIVE` clear, `:1072-1086`, which the kernel does at the
     dispatch whether or not the daemon has reaped the completion). The front door
     permits it: `Watermarks::for_depth` sets `high = depth`
     (`otap-ublk/src/frontdoor.rs:90-96`) and the tags in `ready` are owned too; a
     saturated device makes it likely. Nothing is cancelable; the char device's
     release only clears `UB_STATE_OPEN` (`ublk_ch_release`, `ublk_drv.c:1312-1318`). The abort
     and the quiesce then come from the block layer's request timeout: `ublk_timeout`
     (`:1209-1248`) sees `ubq_daemon_is_dying` (`:1224`, defined `:985`), counts the
     tags without `ACTIVE` (`:1227-1232`) and, when `nr_inflight == q_depth`
     (`:1235`), runs `ublk_abort_requests` and schedules `quiesce_work`
     (`:1238-1240`); with a tag still `ACTIVE` it resets the timer instead (`:1247`)
     and leaves the work to the cancel path. `ublk_mq_ops.timeout = ublk_timeout`
     (`:1298`) and the tag set sets no timeout (`ublk_add_tag_set`, `:2150-2160`), so
     the block layer's default applies (30 s in mainline; outside the saved sources).
     Until the quiesce, `START_USER_RECOVERY` answers `EBUSY` (`:2719-2722`).
2. `ublk_abort_queue` (`:1387-1408`): for every tag whose `ACTIVE` flag is clear (no
   fetch pending: the request was handed to the server) and whose request is started:
   `ABORTED`, then `__ublk_fail_req` (`:1061-1070`): **`REISSUE` → requeue; else
   `ublk_put_req_ref` → `__ublk_complete_rq` → `BLK_STS_IOERR`** (`:999-1001`).
   Requests dispatched while the daemon is exiting are requeued under recovery
   (`:1121-1123`, `:1090-1098`); requests arriving while `canceling` likewise
   (`:1274-1277`).
3. `ublk_quiesce_work_fn` (`:1558-1570`) → `__ublk_quiesce_dev` (`:1547-1556`):
   quiesce, `ublk_wait_tagset_rqs_idle` (polls every 3 ms until no started request
   remains, `:1532-1545`), `state = QUIESCED`; then `ublk_cancel_dev`.
4. The new daemon: `START_USER_RECOVERY` (`ublk_ctrl_start_recovery`, `:2694-2736`,
   see K2; it runs `ublk_queue_reinit`, `:2669-2692`, per queue at `:2725-2726`) →
   open `/dev/ublkcN` (`ublk_ch_open` sets `UB_STATE_OPEN`, `:1301-1310`) → mmap the
   descriptors → `FETCH_REQ` on every tag, **submitted** to the kernel (`:1734-1762`;
   allowed because `nr_io_ready` and the io flags were reset) → the last fetch of a
   queue pins `ubq_daemon = current` (`ublk_mark_io_ready`, `:1621-1636`) and the
   last queue completes `ub->completion` (`:1633-1634`) → `END_USER_RECOVERY`
   (`ublk_ctrl_end_recovery`, `:2738-2774`, see K3), which blocks on that completion
   (`:2748`): issued while the fetches are only pushed and not submitted it never
   returns (5.1 item 4).
5. After the unquiesce the requeued requests are dispatched again through
   `ublk_queue_rq` (`:1250-1283`): `ublk_setup_iod` writes a fresh descriptor for
   `req->tag` (`:937-977`), `blk_mq_start_request`, `ublk_queue_cmd` (`:1279-1280`) →
   task work `__ublk_rq_task_work` (`:1100-1177`) → the pending `FETCH` completes with
   `UBLK_IO_RES_OK` (`:1176`). The daemon sees an ordinary request; the tag is whatever
   blk-mq assigned at this dispatch.
6. Without recovery, and when a device with recovery is stopped. Without
   `UBLK_F_USER_RECOVERY` the death requeues nothing: a request the daemon had been
   handed ends with `BLK_STS_IOERR` when `__ublk_fail_req` drops its reference
   (`:1069` → `ublk_put_req_ref`, `:613-623` → `:999-1001`), and one dispatched but
   never handed over ends through `__ublk_abort_rq`'s
   `blk_mq_end_request(rq, BLK_STS_IOERR)` (`:1097`); the cancel and the timeout then
   schedule `stop_work`, not `quiesce_work` (`:1495`, `:1242`), and
   `ublk_stop_work_fn` (`:2133-2139`) calls `ublk_stop_dev` (`:1593-1618`), which
   skips the quiesce and the unquiesce (run only under `ublk_can_use_recovery`,
   `:1600-1604`) and goes straight to `del_gendisk` (`:1605`) and `DEAD` (`:1610`).
   When a device **with** recovery is stopped (`STOP_DEV`, `:2562`, or `DEL_DEV`
   through `ublk_remove`, `:2523`, `:2164`: the reaper's case), `ublk_stop_dev`
   quiesces it if it is still `LIVE` (`:1601-1602`) and calls `ublk_unquiesce_dev`
   (`:1603`), which sets `force_abort` on every queue (`:1585-1586`; the comment at
   `:1262-1263`: "With recovery feature enabled, force_abort is set in
   ublk_stop_dev()") before it unquiesces and kicks the requeue list (`:1588-1590`),
   so every requeued request fails in `ublk_queue_rq` (`:1271-1272`); then
   `del_gendisk` (`:1605`) and `DEAD` (`:1610`). Every OpenTAP device sets
   `USER_RECOVERY` (`crates/otap-ublk/src/device.rs:95`), so a stopped OpenTAP device
   takes the second path.

## 5. Hook points for Step 6

### 5.0 The reissue analysis, and the guarantee the recovery can make

**What is in the arena after a `kill -9`, by request state.** Let D1 be the dead daemon
and D2 its successor over the same hugetlbfs file.

| The request at the kill | Arena | Kernel view (6.12) | What D2 sees |
|---|---|---|---|
| Completed to the block layer (D1 committed it and the kernel processed the commit, `ublk_drv.c:1358-1380`) | `Placed` (place precedes complete, E1) until a committed CP carries it; then on disk and, once `finalize` released it (`engine.rs:5873-5882`), `Committed`; killed after that CP's `A` slot landed and before or during the release (`engine.rs:5873-5893`, which runs only once `B` landed, `:5580-5582`), both on disk and `Placed` (pitfall 23). A block a later write superseded inside the active generation is `Committed` at once (`engine.rs:3546-3548`), the later write standing in for it | done; its tag had a fresh fetch pending (`ACTIVE`), which the cancel answers with `RES_ABORT` to nobody (`ublk_drv.c:1442-1458`) | replayed from the arena into active generation 1, or already on disk, or both (pitfall 23); nothing to reissue |
| Placed, `front.complete` called, but the commit SQE not yet submitted (`device.rs:325` pushed, `:284` not reached; under back-pressure for many ticks, pitfall 2), or parked in `paced`/`pending_acks` (`engine.rs:3615`, `:3624`; `paced` fills only once the occupancy has passed `pace_at`, 80 % by default, 2.3), or killed between `:3543` and `:3627` | `Placed` | still in flight: `OWNED_BY_SRV`, not `ACTIVE` → aborted (`ublk_drv.c:1394-1405`) → **`REISSUE`: requeued; else `EIO`** (`:1061-1070`) | replayed **and** (with `REISSUE`) delivered again as an ordinary request |
| Killed between `fill` (`engine.rs:3526`) and `place` (`:3543`) | `Filled` → dropped at attach (`arena:835-848`) | in flight, as above | not replayed; (with `REISSUE`) delivered again, served for the first time |
| Killed during `copy_in` or the CRC loop (`engine.rs:3507-3524`), or between `reserve` and `fill` | `Free` reservation → `Committed` at attach (`arena:816-821`) | in flight, as above | as above |
| Killed inside `Arena::fill` (`engine.rs:3526` → `arena:1105-1113`) between one slot's field stores (`:1108-1111`) and its word store (`:1112`), on any slot but the run's last | the slots before it `Filled`-valid (dropped, `arena:835-848`), the slots after it `Free`-valid (noted and released, `:816-821`), the slot itself `Free` under the reservation's stale seal: neither noted nor rewritten (`:816-833`), it stays `Free` on its lap, inside `[tail, head)` when the attach keeps a `Placed` slot before it (`tail = min_placed`, `arena:925-926`; with none the tail starts past it, the slot lies behind the window and nothing stalls). Then the tail stops at it once what precedes it is committed (`:1278-1288`); within one lap every `reserve` is `Full` (`:1019-1024`), so every write is deferred (`engine.rs:3497`; retried in order, `:3354-3359`; punches behind it too, `:3364-3368`) until a restart whose attach finds no `Placed` slot before it (`arena:926`). Pitfall 21 | in flight, as above | as above for the request itself (never `Placed`, so never replayed); the pool stalls later when a kept `Placed` slot preceded it |
| Fetched by D1 and deferred (`engine.rs:3449`, `:3497`) or still `Ready` in the front door | nothing | in flight, as above | as above |
| Dispatched by the block layer but never fetched (the fetch was pending, `ACTIVE`) | nothing | the task work sees the exiting task and requeues (`ublk_drv.c:1121-1123`); arrivals while `canceling` are requeued under `USER_RECOVERY` alone (`:1274-1277`, `:1090-1098`) | delivered after `END_USER_RECOVERY`, with or without `REISSUE` |

So: **(acknowledged and not yet carried by a committed CP) ⊆ Placed ⊆ (acknowledged ∪
reissued)** when `REISSUE` is set (the left side block by block, table row 1), and
every write D2 replays and never sees again was acknowledged to the block layer or was
never going to be (the client got `EIO` without `REISSUE`). Nothing is silently lost
and nothing acknowledged is dropped. The only writes in two places are the reissued
Placed ones, and the index makes the second placement supersede the first (released
at once while the first is in the active generation; outranked in lookup order and
released at its generation's CP when generation 1 was already cut: point 1 below).

**Is re-placing a reissued write safe?** Yes:

1. Same `(vol, lba, blocks)` and the same pages (the kernel re-dispatches the same
   request; `ublk_setup_iod` rebuilds `start_sector`/`nr_sectors`/`op` from it,
   `ublk_drv.c:971-973`; the data is read again from the request's pages via user
   copy), but not necessarily the same bytes: `ublk_ctrl_start_dev` sets no queue
   feature but `WRITE_CACHE`, `FUA`, `ROTATIONAL` and `ZONED` (`ublk_drv.c:2193-2242`),
   so not stable writes, and the block layer does not keep a write's pages stable
   while it is in flight: a buffered page under writeback can be redirtied between
   D1's `copy_in` and D2's (mainline page-cache behaviour, outside the saved
   sources). Either content is permitted, since the write had not completed, and D2's
   copy is what the page holds now.
   `serve_write` places it under a new sequence `s' > every replayed seq`
   (`arena:932`, `:984-988`) and `Index::insert` makes it win its range in the
   **active** generation (`index:583-584`; `VolumeDirty::insert` trims only its own
   generation's map, `:238-272`). While the replayed copy is still in active
   generation 1 the insert returns its run and `engine.rs:3546-3548` releases it, so
   the arena holds one copy again. Once `maybe_cut` has frozen generation 1 (it can run before
   serving: 4.4 step 8, pitfall 15) the replayed copy is not superseded: it stays
   `Placed` in the frozen generation until that generation's CP finalizes
   (`engine.rs:5873-5882`) and the arena holds two copies meanwhile (the cost the
   dedupe paragraph below counts). Either way, on every block where the replayed copy
   is the newest placement, the volume ends with D2's copy: a lookup answers the
   active generation first (`index:599-613`), so reads see the new placement, and the
   CPs flush the frozen copy and then the active one, so the disk ends with it too. As
   a rule its bytes are the replayed copy's and the content does not change; when the
   page was changed in flight, the content becomes the page's newer bytes, which the
   unfinished write permits. On a block where the replay kept instead an overlapping
   write that D1 placed after it (`arena:937-942`, `index:238-272`), re-placing
   changes the content to the reissued write's bytes, which is correct only because
   the two were concurrent (point 3), not because the bytes are the same.
2. A client-ordered later write to the same LBA cannot exist: the client never
   received the completion of the reissued write, so nothing it issued "after" that
   completion exists. (This is the lead's own argument; it holds for discards and
   writes of zeros too.)
3. A write W2 to the same LBA that was *concurrently* in flight with the reissued W1 can
   exist, acknowledged or not. The block-device contract gives two overlapping in-flight
   requests no defined order, so either final content is correct. The arena proves the
   overlap when W2 is the newer one: `s2 > s1` means W2 was placed after W1, hence
   issued before W1 completed (completion follows placement, E1), hence concurrent, and
   re-placing W1 after the replay merely picks the other permitted outcome. When W2 is
   the older one (`s2 < s1`), re-placing W1 reproduces the order D1 already had (W1
   newest), so the content changes only as point 1 allows (W1's own pages may carry
   newer bytes). The same holds between two reissued
   writes whichever order blk-mq re-dispatches them (the requeue-list order is a
   block-layer property not visible in `ublk_drv.c`; D2 must not depend on it and does
   not need to). One consequence is permitted but visible: when W2 is the newer one, a
   read D1 served and completed after placing W2 returned W2's bytes, and after the
   recovery the reissued W1 is re-placed over W2, so a later read returns W1 although
   the client issued nothing in between. The contract allows it (W1 had not completed,
   and takes effect at its re-placement), but the guarantee names it and a test that
   compares reads across the restart must accept it. The simulator's checker is
   stricter than this contract: it expects op order (`crates/otap-sim/src/model.rs:14-25`),
   so a harness test of these outcomes needs the recipe of 5.1 item 6.
4. A reissued **discard or write of zeros** over an LBA that a *later* write covered: that
   later write can only be a concurrent one (point 2), so re-punching is permitted.
   The hole record path is the same as a write (`engine.rs:3594` before `:3600`), the hole
   extent wins its range (`index:231-237`), and a superseded data run is released.
5. A reissued **read** is served again from the current state (idempotent; the data may
   differ from what D1 copied out only if a concurrent write landed, which is allowed).
   A reissued **flush** answers `Ok` again (`engine.rs:3481-3484`) and touches nothing.
6. The partial-placement corner: a kill inside the `place` loop (`arena:1231-1233`)
   leaves a prefix of the run `Placed` and the rest `Filled`; attach keeps the prefix
   and `runs_of` replays it as a shorter extent. The write was unacknowledged, so a
   partial landing is within the contract, and with `REISSUE` the whole write is
   re-placed over it. Without `REISSUE` the prefix stays (the client got `EIO`).

**Would a dedupe be cheap, and is it needed?** Cheap: for each block `k` of the incoming
write, `index.lookup(vol, lba + k)` (`index:599-613`, a `BTreeMap` range per volume
under its mutex) gives a `Hit { ring, seq, gen, hole }`; `arena.descriptor(hit.ring.slot)`
(`arena:1190-1195`) gives the recorded `crc32c` and `seq`; `arena.run(hit.ring, 1)`
gives the bytes. A duplicate is: every block hits, none is a hole, the descriptor CRC
equals the incoming block's CRC, **and the 4 KiB compare agrees** (a CRC-32C match alone
is a 32-bit coincidence, not an equality proof). Cost `O(n log m)` plus `n` memcmp for an
`n`-block write; `n ≤ 2048`. The incoming data must be in hand first, which means
`copy_in` into a reserved run (`engine.rs:3507-3510`) and `arena.discard(ring, blocks)`
on a match (`arena:1178-1187` allows `Free`), or a scratch `Vec` like `serve_read` uses
(`engine.rs:3637`). Not needed for correctness: wherever it would match (every
block's newest entry equal to the incoming bytes), re-placing gives the same content
(point 1). Nor would a dedupe catch every reissue: a reissue need not match its
replayed copy (its pages are not held stable, point 1), and one that does not is
re-placed with its newer bytes, the right outcome anyway. Re-placing releases the
replayed run at once only while that run is in the active
generation (`engine.rs:3546-3548`); when generation 1 was already cut before the
reissue arrived (E5, pitfall 15) the replayed run stays `Placed` until its
generation's CP finalizes (`:5873-5882`). That is the only cost: a second copy of the
data in a CP (the frozen copy is flushed, then the new one) and the slots held
meanwhile. A dedupe cannot tell "the same request reissued" from "a new write of
identical bytes", which is harmless because the outcome is identical. Recommendation:
no dedupe in the first cut; re-place, and count writes whose content already matched if
a statistic is wanted (`Stats`, `engine.rs:1311-1348`).

**Can the arena carry a request identity?** No, not in layout 1.2, and it would not help:

- `SlotDesc` has no spare byte (`arena:342-354`, asserts `:1335-1344`). E16's layout
  paragraph still describes the 1.1 descriptor (`vol << 40 | lba`, sequence, block CRC
  and the sealed word, `docs/07:343-349`), but bytes 20..24 are the 1.2 `kind` word,
  not padding (`arena:349-350`, assert `:1339`; `ARENA_VERSION`, `:92-95`; E23 records
  it, `docs/07:677-679`). `kind` accepts only 0 and 1, anything else is `Damage::Kind`
  at attach (`arena:890-898`); the data slot is 4096 bytes of client data;
  `HoleRecordBlock.rsvd0/rsvd1` and the slot's tail must be zero for `decode`
  (`:324-331`). Spare bits remain, though: 1.2 never sets bits 50-63 of `addr`
  (`pack_addr`, `:371-373`, with `vol <= MAX_VOL = 1023`, checked at `:1101` and
  `:1131`; `crates/otap-format/src/constants.rs:54-56`), and `kind` uses one of its
  32 bits, so a stamp there would keep the descriptor's size and escape the
  `BadGeometry` path below. Using any of these is layout **1.3** and, since `verify`
  checks the major version only (`arena:211`), a 1.2 attach of a 1.3 arena would
  misread stamped descriptors as damage (`kind`) or as data of a `vol` above
  `MAX_VOL` (`addr`, `arena:377-379`), whose runs the replay orphans
  (`engine.rs:1823-1833`), either a holding cause (`:1869-1870`); a descriptor-size
  change would make
  `Geometry::for_len` disagree with the header → `BadGeometry` → `arena_lost` → the
  acknowledged writes are gone and every volume is held. Any such change needs the
  major bumped or an explicit minor check.
- The header's `reserved: [u8; 4024]` at offset 72 and its `rsvd1: u32` at offset 60
  (between the CRC at 56 and `attach_word` at 64: `arena:183-191`, asserts
  `:1332-1334`) are outside the seal (`ARENA_HEADER_SEALED = 56`, `:110`) and never
  read by `verify` (`:207-226`); a self-checking word there (like `attach_word`,
  `:230-245`) is 1.2-compatible. `rsvd0` (offset 6) is under the seal. Beyond the
  header only the geometry leaves slack: the descriptor table is padded to whole pages
  (`Geometry::for_len`, `:669`, `:678-682`), and the padding is zeroed at create
  (`:714`) and never read, since descriptors are addressed by slot below `slots`
  (`:1307`, `:1313`). Its size follows the slot count: 160 bytes for the 2 MiB test
  arena's 507 slots, 64 for the default 8 GiB arena's 2,080,894, none when the count
  is a multiple of 128 (a 128 MiB arena's 32,512, which leaves instead one page past
  the last slot that the arena never touches), so nothing can rely on it. Those are
  the only free arena bytes, enough for a small table, not for per-request stamps.
- A `(dev, tag, generation)` stamp would be meaningless anyway: the generation is D1's
  process memory (`TagSlot.gen`, `otap-ublk/src/frontdoor.rs:143-147`); the kernel re-dispatches with
  no marker (K5) and `req->tag` at the second dispatch is whatever blk-mq assigns, so a
  stamp could not be matched. The only identity the kernel preserves across the crash
  is per device: `ublksrv_flags` (K4), the right home for a `(pool, volume)` binding,
  which nothing writes today (5.1 item 4). The brainstorm record proposed exactly such
  a stamp for the partner's dedupe (`req_id` = (ublk tag, daemon generation) in every
  record, `docs/01-architecture-brainstorm-record.md:1346`; kept because "REISSUE
  needs it", `:1426`); it rules nothing, and K5 is why it could not match a reissued
  request.

**The guarantee, stated crisply.** After a `kill -9` of `dbladed` over a hugetlbfs
arena, once a boot has succeeded (a read error fails the boot rather than rewinding
it, and the recovery retries the boot while the devices wait `QUIESCED`: B1, a retry
being safe unless the failed boot held volumes and stopped after its attach, before
its boot commit's `A` slot landed), when that boot's
`Event::Booted` reports `reattached` with `damaged_slots == 0`, `arena_lost == false`,
`rewound == false` and `orphaned_blocks == 0` (the daemon's journal line,
`pool.rs:333-375`, shows exactly these; they are the four holding causes of
`engine.rs:1870`, `hold = dirty && (!arena_whole || rewound || arena_lost ||
orphaned_blocks > 0)`), **and provided no earlier boot since the selected CP met
the hold's condition and stopped after its attach's rewrites or `Arena::create`,
before its boot commit's `A` slot landed** (whether or not it had set its hold, which
it does only after the replay, `:1866-1885` after `:1820-1864`, while the rewrites
and `Arena::create` come before it; a condition no boot can check today: the gap
below):

> (a) Every write, discard or write of zeros whose completion reached the block layer
> before the crash is present in the volume after the restart (on disk already when a
> committed CP carried it; otherwise replayed from the arena into active generation 1
> and flushed by the CP of a later cut, not by the boot CP, which carries no
> generation: E5, `engine.rs:1713`), except on a volume whose deletion, rollback or
> shrinking resize was in flight at the kill and whose CP's `A` slot had landed, held
> or not (the operation takes effect at the restart: the volume's writes go with a
> deletion, those since the snapshot with a rollback, `engine.rs:2625`, and those
> beyond the new end with a shrink, `:2665`), and **nvfail is
> unchanged**: no volume is newly held, and a volume held before the crash stays held
> with its recorded cause (`engine.rs:1776` loads the table's cause, `:1880` never
> overwrites one; the harness checks `nvfail == held`, `harness.rs:1291-1296`, T3),
> except in two benign cases (the paragraphs after the guarantee): a volume whose
> nvfail clear was in flight at the kill and whose clearing CP's `A` slot had landed
> (that clear takes effect at the restart and the volume serves), and a held volume
> whose deletion landed that way (the volume and its hold are gone at the restart).
> (b) Every request the block layer still had in flight at the crash is, with
> `UBLK_F_USER_RECOVERY_REISSUE` and
> on a device D2 recovers (5.1 item 4), delivered again to the new daemon and served
> exactly as a fresh request; serving it again is correct whether or not the dead
> daemon had placed it, because no request the client ordered after it can exist
> (5.0 point 2): every request whose blocks it now overwrites was ordered before it,
> which it must overwrite, or concurrent with it, which leaves either content
> permitted. If the dead daemon had not placed it, this is its first effect. If it
> had, re-serving it places the request again over its own replayed copy wherever
> that copy is the newest: as a rule the same bytes, the page's newer ones when the
> client changed a page while the request was in flight (ublk does not ask for
> stable pages, and the unfinished request permits either: 5.0 point 1). Elsewhere it
> overwrites a write the dead daemon placed after it (a higher
> sequence, acknowledged or not, which the replay keeps over it: `arena:937-942`,
> `index:238-272`): correct because the two were concurrent (that write was issued
> before the reissued request completed, since placement precedes completion: E1;
> 5.0 points 3 and 4), not because the bytes are the same; a read the dead daemon
> served after it placed that later write returned the later write's bytes, which a
> read after the recovery no longer does (permitted: 5.0 point 3). Without
> `REISSUE` the kernel fails the requests the dead daemon had been handed
> (`OWNED_BY_SRV` set, `ACTIVE` clear: `ublk_abort_queue`, `ublk_drv.c:1394-1405` →
> `__ublk_fail_req`, `:1061-1070` → `:999-1001`) with `EIO`, and the ones among them
> the dead daemon had placed nonetheless persist; a request dispatched while the task
> was exiting (`:1121-1123`) or arriving while `canceling` (`:1274-1277`) is requeued
> under `USER_RECOVERY` alone (`__ublk_abort_rq`, `:1090-1098`) and delivered after
> `END` either way (table row 7). (c) Otherwise, on a dirty boot (`dirty =
> !sb.is_clean_shutdown() || rewound`, `engine.rs:1767`), that is exactly when the
> arena did not re-attach (blank memory: a heap arena, a power loss or a deleted file,
> recreated with `arena_lost == false`, `engine.rs:1702-1708`) or a slot was damaged
> (together `!arena_whole`, `engine.rs:1869`), the arena was lost (which includes a
> header that verifies but a window wider than the ring, `arena:927-931` →
> `engine.rs:1690-1694`, and a header whose geometry no longer fits a changed
> `--arena-bytes`, pitfall 13), the boot rewound (a newer candidate read back and did
> not verify, B1), or a replayed run was orphaned (its volume absent, `:1823-1833`,
> which today only a rewind produces, or its insert refused, `:1851-1861`, unreachable
> today; both defensive, pitfall 9), the boot holds every write-active volume not
> already held, under `DirtyBoot` (`TornCp` after a rewind, `:1873-1877`), and every
> reissued request to a held volume answers `EIO` (`NvFailed`) until the operator
> clears it. In case (c) the replay still inserts every undamaged placed run of a
> present volume (`:1834`), but (a) is not promised: a damaged slot, a blank or lost
> arena or an orphaned run is an acknowledged write that may be gone, which is what
> the hold is for (E4). A clean boot holds nothing, whatever the arena shows (`hold`
> needs `dirty`, `engine.rs:1870`), and needs no hold: it selects a clean shutdown's
> final CP (or a format's CP 0, before anything was written: B2), which carries every
> write the arena can still hold `Placed` (B1). After a
> `kill -9` that is the boot of one that left that CP the newest on disk (one after
> its `A` slot landed, pitfall 23, or one during the next boot before that boot's
> commit landed its `A` slot, B2), even over a blank heap arena, a damaged slot or a
> lost arena: the `Shutdown` cut waits until nothing else is frozen
> (`engine.rs:3886-3896`) and nothing is fetched after `shutdown`, which clears
> `serving` (`:2240-2243`; `tick` serves only while `serving`, `:2109-2110`), so every
> slot still `Placed` is that CP's, and (a) holds, with its exceptions. (a) to (c)
> are about data, not progress: they say what the volumes hold and how a request is
> answered once it is served, not that every request will be served. A `kill -9`
> inside a multi-block `fill` meets every report condition above and can still stall
> every write of the pool (pitfall 21; the last case below).

**The exceptions in (a): a clear, a deletion, a rollback or a shrink, whose CP's `A` slot landed before the kill.**
The first case is a clear. `nvfail_clear` records the clear and asks for a CP
(`VolRt.clearing`, `engine.rs:2260-2266`); the next CP's `build_slot` writes the
clearing volume's row with `nvfail_cause` 0 and the new `rollback_gen` (`:5647-5653`;
the slot is built for `A` at `:5541`, and the clear is stamped with that CP at
`:5677-5683`), while the in-memory hold ends only in
`finalize`, once `B`'s completion is seen (`:5580-5582` → `:5931-5935`). Until then
`serve` answers `NvFailed` (`:3418-3421`) and `nvfail_set` counts the volume
(`:3227-3236`, "a clear in progress still counts"). A `kill -9` after that CP's `A`
slot landed and before `finalize` leaves that slot the newest verified one on disk
(`SuperblockSlot::rank` ranks by `(owner_epoch, cp_seq, cut_seq, A copy)`, newest
first, `crates/otap-format/src/superblock.rs:793-811`; the CP's metadata and the pool
flush preceded it, 4.5 step 4), so the next boot selects it (`boot.rs:286-291`), loads
cause 0, which `engine.rs:1776` filters to `None`, and under (a)'s conditions `hold`
is false (`:1869-1870`): the volume, held at the crash, serves after the restart under
the clear's generation (`:1774`). The direction is benign (the operator's clear
landed, and the generation it brings is durable before any client sees it), and the
doc of `nvfail_clear` ("A crash before that CP leaves the volume held", `:2245-2249`)
is true of a crash before the `A` slot lands. But the harness would report it as a
failure: `held_at_stop` is `nvfail_set` at the stop (`harness.rs:1143`), compared at
`:1291-1296`. No test reaches the window: `Sim::clear_nvfail` ticks until the clear is
durable (`harness.rs:1354-1368`), and the tests that call `nvfail_clear` directly kill
the daemon on a stalled disk before the `A` slot is written
(`crates/otap-sim/tests/adversarial_sim_tests.rs:990-1003`), cut power instead
(`:1505-1522`) or let the CP finish first (`:1526-1528`;
`crates/otap-sim/tests/sim_tests.rs:308-320`, whose `request_cp` ticks until the CP is
quiet, `harness.rs:661-666`; `crates/otap-ublk/tests/engine_tests.rs:328-329`). A
reissue proptest that kills the daemon at arbitrary ticks with clears in play (5.1
item 6, open question 10) must accept the window, as `record_stop` already does for a
rollback in flight (`harness.rs:1146-1152`): for instance by dropping from
`held_at_stop` a volume whose `rollback_gen` after the restart is the one its clear
brought (`engine.rs:2263`). Derived from the code, not executed.

The second case is a deletion, of any volume, held or not. Nothing refuses a held
volume's: `set_volume_online` (`engine.rs:2565-2580`), `delete_volume`
(`:2697-2715`) and `check_vol_op` (`:2584-2599`, which asks only that the volume be
present and not being created, `:2588`, offline, without an operation waiting, with
nothing unflushed and no CP in flight touching it) have no nvfail check, nor does
the daemon's `volume_delete` (`crates/otap-daemon/src/pool.rs:881-890`). The
deletion CP's roots table drops the volume (`crates/otap-map/src/lib.rs:1363-1368`),
so it is not in `carried` (`engine.rs:5403-5410`) and its row in the slot built for
`A` (`:5541`) stays `ABSENT` (`:5632-5634`; the slot starts with an empty table,
`:5591`); its `VolRt` is reset only in `finalize`, after `B` (`:5846-5847`, which
then emits `VolumeDeleted`, `:5848`), and until then `nvfail_set` still counts a
held one (`:3229-3236`). A `kill -9` after that CP's `A` slot landed and before
`finalize` makes the deletion take effect at the restart: the next boot selects that
slot, as for the clear above, and it has no row for the volume. The daemon's boot,
which configures no volumes (`pool.rs:244`), finds no such volume: its data and, for
a held volume, its hold are gone, as the operator asked. The harness's boot does
not: the model and the configuration forget a volume only on `VolumeDeleted`
(`crates/otap-sim/src/harness.rs:1080-1081`), so the configuration still lists it
and the boot creates it afresh, empty and unheld unless the boot rewound
(`engine.rs:1791-1803`). Whenever the model holds data for the volume,
`verify_recovery` then fails before its nvfail check, held volume or not: `image`
peeks every volume of the model (`harness.rs:1221-1233`) and reads zeros from the
new one (nothing of it was in the arena, which `check_vol_op` demands of a
deletion, and its new map maps nothing: an unmapped block reads as zeros,
`engine.rs:3732-3735`, and a volume without a root yet reads `UNMAPPED` too,
`:3701-3702`), so the volume's acknowledged writes either
contradict the image, which `check_prefix` refuses (`harness.rs:1269-1274`), or lie
above the prefix it returns, which `lost_volumes` counts as lost (`:1275`; the
verdict at `:1285-1290`). Only a volume for which the model holds nothing but zeros
passes those checks; then a held one fails the nvfail check (`held_at_stop` still
names it, `:1143`, compared at `:1291-1296`), and an unheld one passes with the
deleted volume back in the engine. The direction is benign (the operator's deletion
landed), and no test reaches the window: `Sim::delete_volume` ticks until
`VolumeDeleted` (`harness.rs:1069-1089`). A reissue
proptest that kills at arbitrary ticks with deletions in play must accept it too, and
dropping the volume from `held_at_stop` is not enough: the test must forget a volume
whose deletion landed in the model and the configuration as well, as
`Sim::delete_volume` does on `VolumeDeleted` (`harness.rs:1080-1082`). One way:
before the boot, take every volume whose deletion it had requested out of the
configuration (the boot loads a volume the disk still has from the slot's table,
`engine.rs:1770-1785`, and recreates one the disk lacks only from the configuration,
`:1786-1803`); after the boot, a volume that `Engine::volume` (`:3241`) reports not
`present` (the test `sync_size` makes, `harness.rs:958-966`) is one whose deletion
landed, which the test then drops from the model and from `held_at_stop` before
`verify_recovery`, and a volume still present goes back into the configuration.
Derived from the code, not executed.

The rollback and the shrinking resize land like the deletion: `check_vol_op` gates
both alike (`engine.rs:2584-2599`), the CP's map input carries them (`:4813-4849`),
and `finalize` applies them to memory only after `B` (`:5809`, `:5838`), so a
`kill -9` in that window makes them take effect at the restart, releasing what the
operator asked: every block written since the snapshot (`:2625`) or what lies beyond
the new end (`:2665`). Neither changes nvfail (a held row keeps its cause,
`:5654-5656`). Derived from the code, not executed.

**The gap: a holding boot that stops before its boot commit's `A` slot lands.** A
dirty boot's hold exists only in memory (`VolRt.nvfail`, `engine.rs:1866-1885`); the
causes reach the disk only in the boot commit's slot (the table rows at
`:5647-5662`, `sb.nvfail` at `:5674`, built for the `A` slot at `:5541`). Before
that, the same boot has already erased the evidence in the shared arena file:
`scan_on_attach` writes every damaged slot back `Committed` before it returns
(`arena:933-935`; queued at `:857`, `:867`, `:885` and `:896` for a `Placed` slot,
at `:832`, `:848` and `:920` for the others), and `Arena::create` overwrites a lost
or blank arena with a fresh sealed header (`engine.rs:1691`, `:1704`; `arena:714`,
`:726`). Several things end that boot early: a second `kill -9` during the recovery;
the stop flag, `--op-timeout` or a tick error while `Pool::boot` waits for the boot
commit (`pool.rs:280` → `wait_until`, `:523-556`; `--op-timeout` is also where a
boot commit that keeps failing ends, a failed boot commit being retried rather than
fatal, and where one that failed for want of space always ends, its retry waiting for
a `request_cp` nothing sends during the boot: 4.4 step 8), after which the pool and its
engine are dropped (neither has a `Drop` impl) with no shutdown CP; or a `start_job`
error (`engine.rs:1714-1725`). The next boot then re-attaches a whole arena and
reports `reattached`, `damaged_slots == 0`, `arena_lost == false`, `rewound == false`
and `orphaned_blocks == 0`, so `hold` is false (`engine.rs:1869-1870`): the volumes
whose acknowledged writes the first stop cost (a power loss that left the file
blank, a damaged slot, a window wider than the ring, pitfall 13's larger
`--arena-bytes`) serve without nvfail, and the report conditions of the guarantee
are met although writes are lost. The harness's `Stop::Daemon` verdict would fail on
it (`held_at_stop` is the engine's in-memory set, `harness.rs:1143`, compared at
`:1291-1296`; lost acknowledged writes are checked at `:1285-1290`, a check the
writes lost before the stop fail even with the fix, so a test must assert the hold
itself: pitfall 22), but no test stops a holding boot inside its boot commit:
`crates/otap-sim/tests/adversarial_sim_tests.rs:1428-1438` does so only after a
daemon crash, when nothing is held, and `:1354-1357` cuts power twice, which leaves
the arena blank again, so the next boot holds again. The fix to propose is in the
engine: make the hold durable before the attach's rewrites and `Arena::create`, or
defer them until the boot commit's `A` slot lands (pitfall 22, open question 12).
Derived from the code, not executed.

Not covered by the guarantee: a heap arena (`--heap-arena`, S5-1 `docs/07:1049-1052`),
where every crash is (c) but a `kill -9` that leaves a clean shutdown's final CP the
newest on disk, which boots clean and needs no hold (the end of (c), pitfall 12); a
recovery after a boot that met the hold's condition, whether or not it had set the
hold, and stopped after its attach's rewrites or `Arena::create`, before its boot
commit's `A` slot landed (the gap above); a device whose volume is
gone, offline, resized or whose params no longer fit (section 7); a device with no
binding (every device created before the binding is written, pitfall 1), which D2
can neither bind nor prove its own and so leaves alone (5.1 item 5, open question
2); a device of this pool whose earlier recovery was interrupted (K2), which only the
reaper can take, failing its requeued requests (5.1 item 5); and progress after a
`kill -9` inside a multi-block `fill` (pitfall 21). The slot that kill leaves `Free`
under the reservation's stale seal is neither reported nor rewritten by the attach
(`crates/otap-arena/src/lib.rs:816-833`), so `damaged_slots == 0` and every report
condition of the guarantee holds; yet when a `Placed` slot comes before it, the tail
stops at it once the CPs have committed what precedes it (`arena:1278-1288`), and
within one lap every write, reissued or new, is deferred
(`crates/otap-cp/src/engine.rs:3448-3450`, `:3497`; punches too, `:3565`) until a
restart whose attach finds nothing `Placed` before it (`arena:926`). The data
guarantee still holds (the deferred writes are unacknowledged, and each is served
correctly once it is served); a promise of progress needs the pitfall-21 arena fix
(open question 11) first.

### 5.1 Where the new code plugs in

1. **Keep the replay result** (`engine.rs:1820-1864`). Today `report.placed` is consumed
   and dropped. Smallest change: a new `Engine` field (e.g.
   `replayed: Vec<(u32, u64, DirtyExtent)>` taken from `runs_of`, or per-volume
   counts) plus an accessor, so the daemon or `otapctl` can report what was replayed
   per volume; `Event::Booted.replayed_blocks` (`:344-345`, `:1992`) and the daemon's
   journal line (`pool.rs:353-357`) already carry the total. Must not break: the
   `hold` predicate (`engine.rs:1869-1870`), the orphan release (`:1825-1833`), the
   `(seq, position)` order of the input (`arena:937-942`).
2. **A reissued write needs no engine change**: `serve_write` already re-places it under
   a new sequence (`engine.rs:3525-3543`). It releases the superseded replayed run at
   once only while that run is in the active generation (`:3546-3548`); once
   `maybe_cut` has frozen generation 1 the replayed run stays `Placed` until that
   generation's CP finalizes (`:5873-5882`; 5.0 point 1, pitfall 15). If the lead
   wants the optional dedupe, it goes between the CRC loop (`:3517-3524`) and `fill`
   (`:3526`): look up each block, compare CRC and bytes, and on a full match
   `self.arena.discard(ring, blocks)?` then `self.acknowledge(req, vol, hit_seq)`
   (`acknowledge` sets `windows`, honours intents and pacing). It must not run when
   `self.index.lookup` answers from a frozen generation with a different `gen` unless
   bytes are compared, and it must keep `Stats.writes` consistent. Nothing else in the
   engine distinguishes a reissued request (there is no marker to carry).
3. **The front door needs no change for reissue**: a reissued request arrives as a
   `QueueEvent::Request` on a fresh `UblkFrontDoor` (built by `device.rs:344-360` with
   `TagSlot.gen = 0`). It must be fresh: `pump` answers a `Request` on a tag that is
   not `Idle` with `-EINVAL` ("the driver never does this; answer it rather than lose
   it", `otap-ublk/src/frontdoor.rs:394-399`), and only `UblkFrontDoor::new` makes
   every tag `Idle` with `gen` 0 (`:265-272`), so one new front door per recovered
   queue, built on the thread that will serve it (F2), never one reused with
   surviving tag states. Build it, as `Pool::ensure_device` builds every front door
   today (`pool.rs:687-693` → `DeviceSpec::new` at `crates/otap-daemon/src/device.rs:19`
   → `device.rs:352-357`), from the **volume's current size** (`VolRt.size_blocks`,
   `engine.rs:1287`), the size `decode_request` (`otap-ublk/src/frontdoor.rs:153-187`)
   checks against, rather than from the device's `dev_sectors`. That is a preference,
   not a requirement: the kernel's `dev_sectors` (fixed at `START_DEV`, pitfall 13)
   bounds what can arrive, and a request past a volume shrunk below it is refused
   either way, by `decode_request` with `EINVAL` (`otap-ublk/src/frontdoor.rs:175-180`)
   or, from a front door built on `dev_sectors`, by the engine's range check before
   anything is reserved (`engine.rs:3422-3433`), whose `OutOfRange` `errno_for` maps
   to `-EINVAL` too (`otap-ublk/src/frontdoor.rs:203`). The volume's size only keeps
   the refusal in the front door, where it is always `EINVAL`; through the engine the
   volume's own checks come first (`engine.rs:3404-3421`), so a volume gone or
   offline answers `ENODEV` and a held one `EIO` (`otap-ublk/src/frontdoor.rs:200-201`).
4. **The daemon's recovery hook is in `Pool::boot` before `ensure_device`**
   (`pool.rs:281-293`, `:678-709`), so it runs only after a boot that succeeded (a
   failed engine boot returns at `:252-257` and must be retried, B1): enumerate
   devices (`GET_DEV_INFO` per id, built by `CtrlCommand::get_dev_info`,
   `otap-sys/src/ublk.rs:787-791`; `UblksrvCtrlDevInfo.state == UBLK_S_DEV_QUIESCED`,
   `otap-sys/src/ublk.rs:449`, `:324`; a device still `LIVE` whose `ublksrv_pid` is
   dead is one waiting for the
   kernel's quiesce, pitfall 6; a `DEAD` device was never started or was stopped and
   cannot be recovered, K2), match one to the volume by `ublksrv_flags` (`:463`; the
   kernel stores it at `ADD_DEV`, `ublk_drv.c:2434`). **Nothing writes that field
   today**: `UblkDevice::create` fills the rest of `ADD_DEV`'s `dev_info` with
   `..UblksrvCtrlDevInfo::default()` (`device.rs:90-97`; the field is only declared,
   `otap-sys/src/ublk.rs:463`, and its offset asserted, `:944`), so every device
   created so far carries 0. The binding must be written there, beside `REISSUE`
   (`device.rs:95`), and D2 must decide what to do with a device whose
   `ublksrv_flags` is 0, which is every Step 5 device: the match cannot bind it, and
   the reaper cannot take it either, since 0 is also what another ublk server's device
   may carry (item 5); open question 2.
   Then `START_USER_RECOVERY` (`CtrlCommand::start_user_recovery`,
   `otap-sys/src/ublk.rs:807-809`), **retried on `EBUSY`** while the device is `LIVE`
   or `QUIESCED`, until the corpse has released the char device and the device is
   `QUIESCED` (`ublk_drv.c:2719-2722`; up to the block layer's request timeout when
   every tag was owned at the kill, 4.7 step 1); `EINVAL` from a `QUIESCED` device
   means another recovery was started on it (K2), still in progress (another daemon
   between its `START` and its last processed fetch) or interrupted (that daemon died
   in the window): D2 never retries it and leaves the device to the reaper's rule
   (item 5), which must tell the two apart. Then open `/dev/ublkcN`, `UblkQueue::new` +
   `push_fetch` on every tag **and then `UblkQueue::submit`**
   (`otap-sys/src/ublk.rs:1316-1346`, `:1382`, `:1414-1421`), from the thread that
   will serve the queue (F2), exactly as `open_queue` does (`device.rs:166-177`, the
   `submit()` at `:174`): `push_fetch` only queues SQEs, and `ublk_ctrl_end_recovery`
   blocks in `wait_for_completion_interruptible(&ub->completion)`
   (`ublk_drv.c:2748`) until `ublk_mark_io_ready` has counted a processed `FETCH_REQ`
   on every tag of every queue (`:1621-1634`, from the `FETCH_REQ` case at `:1761`),
   so an `END` issued with the fetches still unsubmitted never returns (`START_DEV`
   blocks the same way, which is why `open_queue` submits first, `device.rs:179-180`).
   Then `END_USER_RECOVERY` (`CtrlCommand::end_user_recovery`,
   `otap-sys/src/ublk.rs:815-819`) with this pid, and `add_device` as today
   (`daemon/src/frontdoor.rs:254`). Order: `END` after `wait_until(is_serving)`
   (`pool.rs:280`), in the loop that creates devices today. The only reason is that
   the engine serves nothing before the boot commit (`tick` calls `serve_front_door`
   only while `serving`, `engine.rs:2109-2110`; E11): requests delivered earlier would
   merely sit as unreaped completions in the queue's io_uring completion ring
   (`ubq_complete_io_cmd`, `ublk_drv.c:1072-1086`) until the first
   `LinuxTransport::poll` (`device.rs:284`), with the block layer's timer reset while
   the daemon lives (`ublk_timeout`, `ublk_drv.c:1224-1225`), so nothing is lost either
   way. The arena is no emptier after the boot commit (it carries no generation, E5,
   `engine.rs:1713`, and the replayed runs stay in active generation 1), and the
   engine's `deferred` queue fills only when `serve` hands a request back (`:3449`,
   `:3497`), which the timing of `END` does not change. `ADD_DEV` must add
   `UBLK_F_USER_RECOVERY_REISSUE` (`device.rs:95`; K4) for new devices.
   A concrete home for the handshake is `UblkDevice::recover(dev_id, &DeviceSpec)`
   beside `create` (`device.rs:88-125`): open the control ring; `GET_DEV_INFO`,
   checking the state (`QUIESCED`), the flags and the binding, and taking the depth
   from the device (`queue_depth`, `otap-sys/src/ublk.rs:443-472`), since `END` waits
   for a fetch on every tag (K3); `START_USER_RECOVERY`; `open_queue`
   (`device.rs:166-177`), which opens `/dev/ublkcN`, pushes a fetch per tag and
   submits; `END_USER_RECOVERY` with the pid; and `started = true`, so that `delete()`
   issues `STOP_DEV` before `DEL_DEV` as for a created device (`:214-219`; `start` sets
   the flag, `:184-190`; without it only `DEL_DEV` runs, which on 6.12 stops the
   device itself, `ublk_remove`, `ublk_drv.c:2162-2164`). Then
   `front_door(&mut device, vol, &spec)` (`device.rs:344-360`) works unchanged. Every
   control command is synchronous on the calling thread (`UblkControlRing::execute`,
   `otap-sys/src/ublk.rs:1213-1276`), so the `EBUSY` retries and `END` block the
   engine thread, which must also issue the fetches and serve the queue (F2); and
   wherever the handshake sits in `Pool::boot`, the control server answers nothing
   until it is done (4.6 step 1). The reaper's side would be a `probe(dev_id)` that runs
   only `GET_DEV_INFO`, behind the brief's `otapctl device list` and `device reap`
   (`docs/10:607`), and the re-attach deserves its own journal line (say "volume N
   re-attached /dev/ublkbN", beside today's "volume N is /dev/ublkbN", `pool.rs:696`).
5. **The reaper** (scope A's other half, another reader's map) may take only a device
   it can prove is this pool's. `ublksrv_flags` is the server's own field
   (`ublk_cmd.h:237-238`: "For ublksrv internal use, invisible to ublk driver"), so 0,
   or any value without OpenTAP's mark, can be another ublk server's device, and a
   device bound to another pool belongs to that pool's daemon (telling pools apart is
   why open question 2 puts the pool's identity in the binding). The ownership test is
   the binding itself, written at `ADD_DEV` (item 4): a magic, this pool's identity (a
   hash of `agg_id`) and the volume. Among the devices whose binding names this pool,
   the reaper stops and deletes (`ublk_ctrl_stop_dev` `ublk_drv.c:2560-2567`,
   `ublk_ctrl_del_dev` `:2512-2549`) a `QUIESCED` device whose volume is gone or no
   longer fits (open question 5), and one whose `START_USER_RECOVERY` answers `EINVAL`
   only when the daemon that started its recovery is known gone, by the same kind of
   rule as for a `DEAD` device's creator below: `EINVAL` (`nr_queues_ready == 0`,
   which userspace cannot read, K2) is what an interrupted recovery answers, and also
   what a live one answers from its `START` until its last fetch is processed
   (`ublk_drv.c:2729`, `:1624-1628`, `:2704-2705`), whose requeued requests `STOP_DEV`
   would fail (`:1572-1591`, `:1271-1272`). It leaves alone every device without this
   pool's binding: other pools' devices, other servers' devices, and every Step 5
   device (`ublksrv_flags == 0`, item 4), which it cannot tell from another server's
   (open question 2). Nor is `DEAD` proof of an orphan: a device between `ADD_DEV` and
   `START_DEV` is `DEAD` (K2: `ADD_DEV` copies the creator's `dev_info`, whose `state`
   OpenTAP leaves 0,
   `ublk_drv.c:2434`; `LIVE` is set only at `:2273` or, from `QUIESCED`, at `:2769`),
   and `UblkDevice::create` issues `ADD_DEV`, `SET_PARAMS` and the fetches
   (`crates/otap-ublk/src/device.rs:88-125`) before
   `crates/otap-daemon/src/device.rs:36` starts the device. A `DEAD` device
   bound to this pool is an orphan only if no other daemon serves this pool, which
   nothing enforces (the pool device is opened without `O_EXCL`,
   `crates/otap-sys/src/block.rs:155`; the arena file's `flock`, `hugetlb.rs:346-355`,
   excludes only a daemon on the same file), and a probe that opens `/dev/ublkcN`
   (`EBUSY` while another process holds it, `ublk_drv.c:1306-1307`) misses a creator
   still between `ADD_DEV` and its own open (`crates/otap-ublk/src/device.rs:167`). So
   the reaper deletes a `DEAD` device only when it is this pool's and its creator is
   known gone, by a rule the lead sets. `STOP_DEV` on a `DEAD` device is not quite a
   no-op: `ublk_drv.c:1598-1599` skips the quiesce, `del_gendisk` and the state change
   (`:1600-1614`), but `ublk_stop_dev` still calls `ublk_cancel_dev` after the unlock
   (`:1617`; `DEL_DEV` reaches it too, through `ublk_remove`, `:2523`, `:2164`), which
   completes every fetch still `ACTIVE` and not yet `CANCELED` with
   `UBLK_IO_RES_ABORT` (`ublk_cancel_queue`, `:1504-1510`; `ublk_cancel_cmd`,
   `:1447-1453`, `:1456-1457`). That changes nothing once a gone creator's exit has
   canceled its fetches (`ublk_uring_cmd_cancel_fn` → `ublk_cancel_cmd`,
   `:1487-1489`); it releases the fetches of a creator that died before its queue's
   last fetch was processed, which its exit could not cancel (`ubq_daemon` is set only
   by that last fetch, `:1625-1627`, and the exit's cancel returns at `:1480-1482`
   when the command's task, from `io_uring_cmd_get_task`, outside the saved sources,
   is not `ubq_daemon`); and for a creator still between `ADD_DEV` and `START_DEV` it
   aborts that creator's pending fetches, which is why the rule must prove the
   creator gone. `ublk_stop_dev` fails every requeued request
   (`:1572-1591`, `:1271-1272`), which is the honest outcome for a device of this pool
   that nobody will serve, and the wrong one for a device someone else serves. A
   second crash during a recovery (after `START_USER_RECOVERY`, before the last fetch)
   therefore costs the client the requests in flight on that device (`EIO`), even with
   `REISSUE`.
6. **The simulator** has no reissue: `Sim::crash_daemon` resets the front door
   (`harness.rs:1191`) and the model settles each unacknowledged in-flight write as
   either never having happened (above the prefix `j`) or landed (at or below it)
   (`model.rs:29-32`, `settle` `:335-343`). A reissue scenario must respect the
   checker, which is stricter than the block-device contract of 5.0 point 3: a
   recovered image must be the state after the issued operations, in op order, up to
   some `j` (`model.rs:14-25`; the engine places writes in issue order, E3), and every
   block names the op that wrote it (`:4-6`; `Model::data_of(op, vol, lba, blocks)`,
   `:256`). Re-submitting each in-flight write after `crash_daemon` under its own op
   number, with its own data and a late acknowledgement (`acked(op, seq)`,
   `:299-304`, tolerates one), passes exactly when every block ends up holding the
   newest issued op that covers it, which is what `expected` demands at the prefix
   (`:361-372`, checked at `:447-487`). With every in-flight write re-submitted, that
   holds when every later-issued op that overlaps a re-submitted write is itself
   re-submitted after it (the engine places in arrival order, E3,
   `engine.rs:3364-3368`): trivially when nothing later overlaps it, and whenever the
   in-flight writes are re-submitted in op order and no later-issued overlapping op was
   acknowledged before the crash. In conservative mode no later-issued op can be
   acknowledged before an earlier one, so re-submitting every in-flight write in op
   order always passes: acknowledgements leave in placement order (at once only while
   `paced` is empty, through it in order otherwise, `engine.rs:3618-3627`), placement
   follows issue order (E3), and `collect` records what reached the harness before the
   kill (`harness.rs:589-602`), so the acknowledged writes are a prefix of the issued
   ones. In precise mode they need not be: the landing of a CP's `A` slot puts the
   volume in `durable_active` (`engine.rs:5576`), so a later write to it is
   acknowledged without an intent (`:3611-3627`) while an earlier one still waits in
   `pending_acks` for its intent or for a CP that carries its sequence (`:3762-3768`,
   `:5914-5921`), and a kill in between leaves the later write acknowledged and the
   earlier one in flight. Otherwise the outcome point 3 calls correct is refused: an
   in-flight W1 re-placed over a later-issued, overlapping W2 that is not re-submitted
   after it (acknowledged before the crash, which takes precise mode, or re-dispatched
   before W1) leaves blocks naming W1 where the prefix expects W2, which
   `check_prefix` rejects (`model.rs:447-479`, the error at `:468-472`), unless no block
   names an op at or above W2's, in which case `check_prefix` returns a `j` below W2's
   op and `lost_volumes(j)` (`:508-516`) counts the acknowledged W2 as lost, which fails
   the `Stop::Daemon` verdict (`harness.rs:1285-1290`). Two ways to test the general
   case (a re-dispatch out of op order, and precise mode):
   (a) run `verify_recovery`, which settles the model, after the boot and before any
   reissued request is served, then re-submit each in-flight write as a new op above
   every op issued before the crash, numbered in the order the harness re-dispatches
   them (`issue` asserts increasing op numbers, `model.rs:276`; the new op's own
   `data_of` is fine, since the engine never compares bytes); or (b) extend the model
   so that a reissued op keeps its number and its data but takes a place in the order
   after every op issued before the crash, with `expected`, `check_prefix`, `settle`
   and `lost_volumes` (`:361-372`, `:408-503`, `:335-343`, `:508-516`) ordering by that
   place instead of by op number. (a) needs no model change, only a harness that
   keeps its list of in-flight writes across the crash (`record_stop` clears it
   today, `harness.rs:1150`); (b) also covers the same-bytes re-placement of point 1.
   Either way a `Stop::Daemon` verdict with reissue still demands `lost.is_empty()`
   (`harness.rs:1284-1290`); and a test that kills the daemon at arbitrary ticks with
   clears or deletions in play must also accept the two windows of 5.0, where a clear
   or a deletion whose CP's `A` slot landed before the kill takes effect at the
   restart: today's verdict counts the clear as a changed nvfail, and it fails a
   deletion earlier, on the image, held volume or not (the harness's boot recreates
   the volume empty from its configuration, so its acknowledged writes fail
   `check_prefix` or count as lost), so the test must forget a volume whose
   deletion landed in the model and the configuration, not only in `held_at_stop`
   (5.0 gives one way). The mock transport
   (`otap-ublk/src/mock.rs`) can model the kernel's re-dispatch: after a "crash",
   re-deliver every request still outstanding on a new `MockTransport` with fresh
   events (it must be new: `deliver` asserts that the tag is neither aborted nor busy,
   "tag {tag} already has a request", `mock.rs:58-66`, so re-delivering a request
   still outstanding on the old mock panics; `inject`, `:113-118`, skips that
   bookkeeping, but `commit` then refuses a tag that is not busy, `:215-220`). But the
   old mock cannot say in full what those requests were: it keeps only the busy tags
   (`busy`, `:25`) and the write payloads (`payloads`, `:21`,
   filled only by `submit_write`, `:69-73`; reads, flushes, discards and writes of
   zeros have none), and the descriptor goes into `events` (`deliver`, `:58-66`) and
   leaves with the next `poll` (`:165-177`, drained at `:174-175`). Both fields are
   private but not unobservable: `MockTransport` implements the public `Transport`
   trait (`mock.rs:164`; re-exported at `crates/otap-ublk/src/lib.rs:50-51`) and
   `UblkFrontDoor::transport()` hands out `&mut T`
   (`otap-ublk/src/frontdoor.rs:335-337`), so through `front_door().transport()` a
   test can read a busy write's payload with `read_data` (`mock.rs:179-190`, straight
   from `payloads` at `:183`), and `write_data` (`:196`) and `commit` (`:215`) answer
   by whether the tag is in `busy` (neither is a clean probe: the first records into
   `read_back`, `:199-204`, the second retires the tag, `:215-222`); `outstanding`
   (`:136-140`) gives the count. What no public call yields is a tag's descriptor
   (op, `lba`, blocks). A reissue test therefore needs its own record of what it
   submitted on each tag, dropped as `take_commits`/`take_commit` (`:121-129`) report
   the tag answered (the recipe of section 6), or a mock extension that keeps each
   busy tag's descriptor and exposes it beside the payload. That record already
   reproduces the pushed-but-unsubmitted window of `device.rs:325` (pitfall 2):
   re-delivering on the new `MockTransport` a write whose commit the old mock recorded
   (`commit` records at once, `mock.rs:208-224`) is the kernel re-dispatching a write
   whose commit SQE never reached it, and D1's engine and `UblkFrontDoor` are then in
   the Linux state (`front.complete` ran and retired the tag,
   `otap-ublk/src/frontdoor.rs:499-518`). A second knob that **holds commits until the
   next poll** would only let the mock produce that window by itself: a convenience
   for a test that kills at arbitrary ticks, not a requirement.

## 6. Test patterns to copy

- **Arena-level re-attach tests**: `crates/otap-arena/tests/arena_tests.rs` helpers
  `memory()` (a 2 MiB `HugeArena::anonymous`, `:11-13`; heap memory on a non-Linux
  host, `hugetlb.rs:105-107`), `agg()` `arena_tests.rs:15-17`, `arena()` `:19-21`,
  `paint` `:24-28` (fills a run and returns its CRCs), `filled` `:31-36` (reserve,
  paint, fill); the canonical re-attach test
  `attach_keeps_placed_runs_and_drops_filled_ones` `:170-217` (`into_memory`,
  `can_attach`, `attach`, asserts on `report.placed` as `(seq, vol, lba, slot)`
  tuples, `peek_seq`, `window`, descriptor states, data intact); hole records
  `:404` and `:450`. Property test `the_ring_never_hands_out_a_live_slot`
  `:339-397` with `ProptestConfig { failure_persistence: None,
  ..ProptestConfig::default() }` (`:340`): the default honours `PROPTEST_CASES`.
- **Adversarial arena tests** (`adversarial_arena_tests.rs`): raw descriptor access
  `desc_offset` `:72`, `read_desc` `:78`, `write_desc` `:83`, `set_state` `:92`
  (reads the header to find the table and rewrites one descriptor through
  `HugeArena::as_mut_slice`); `placed_run` `:63-70`; the facts an implementer may
  need to re-pin: `reattach_never_reissues_a_sequence_number` `:185-211`,
  `a_kill_between_fill_and_place_drops_the_write_and_frees_its_slots` `:218-245`,
  `a_reservation_never_filled_pins_the_window_until_a_reattach` `:248`,
  `a_superseded_slot_released_inside_a_run_is_not_replayed` `:310`,
  `a_write_that_does_not_fit_the_lap_end_skips_it_and_survives_a_kill` `:366`; the
  model-based property `reattach_after_any_interleaving_keeps_exactly_the_placed_slots`
  `:769-778` with its `reattach` checker `:495-573` (expected `(slot, lap, vol, lba,
  seq, crc)` per placed block). Its `Op` already has `Kill`, `KillMidFill(i, j)`,
  `KillMidPlace(i, j)` and `KillMidCommit(i, j)` (`:446-449`, drawn at `:460-463`):
  each takes the memory with `into_memory` and calls `reattach`. `Kill` (`:694-697`)
  rewinds nothing; the other three first complete one transition on a run and then
  rewind its slots `j..` with `set_state`: `KillMidFill` fills a reserved run and puts
  them back to `Free` (`:698-716`), `KillMidPlace` places a filled run and puts them
  back to `Filled` (`:717-740`), and `KillMidCommit` commits a placed run and puts
  them back to `Placed` (`:741-758`). `reattach` checks the attach count, every
  kept slot's `(slot, lap, vol, lba, seq, crc)`, the `(seq, position)` order,
  `dropped_filled`, the window and `peek_seq` above the largest kept sequence
  (`:503-571`) and returns the arena (`:572`); it does not call `check`
  (`:577-615`), which `run_ops` runs after every op, reading the live runs' data back
  after a kill (`:760-764`), and once more at the end (`:766`). `KillMidPlace` already
  pins the partial-place replay of pitfall 8 at arena level (`:717-740`). Copy it for any
  attach change (pitfalls 21 and 22, open questions 8 and 11), with a raw-word
  variant of `KillMidFill` for pitfall 21, since `set_state` re-seals (`:88-96`).
  Single cases to copy: `attach_reports_a_placed_slot_whose_word_is_damaged`
  (`:120-134`, bits flipped in the sealed word) and
  `attach_reports_a_descriptor_whose_address_was_damaged` (`:137-154`, one `addr` bit
  flipped under an intact word); and in `crates/otap-arena/tests/arena_tests.rs`,
  `damaged_data_is_caught_by_check_data_and_by_attach` (`:299-322`: the byte at
  `data_off + 3 * SLOT_BYTES + 77` flipped, `data_off = 4096 + 16384` in the 2 MiB
  arena, then `Damage::Data`, the slot `Committed` and the run's other block intact).
- **Engine-level daemon crash through the harness**: `crates/otap-sim/tests/sim_tests.rs`
  `a_daemon_crash_re_attaches_the_arena_and_loses_nothing` `:196-227` (writes, a CP,
  more writes, `crash_daemon`, `boot` asserting `reattached && dirty` and an empty
  nvfail set, `verify_recovery` with `k == 12`, then a crash mid-CP at
  `Phase::Staging` via `tick_until`); the proptest `:634-647` uses
  `ProptestConfig::default()` (so the gate's case count applies) with
  `failure_persistence: None` and `max_shrink_iters: 2048`. In
  `crates/otap-sim/tests/step4_sim_tests.rs`, `daemon_crash_and_recover` (`:100-104`:
  `crash_daemon`, `boot`, `verify_recovery`) is the three-statement helper, and
  `punches_persist_through_daemon_crashes_and_power_cuts` (`:389`) and
  `a_punch_over_a_write_is_not_a_loss_after_a_daemon_restart` (`:1190`) cover the hole
  records' replay through a daemon crash.
- **Crash with a custom front door and arena damage**:
  `crates/otap-sim/tests/adversarial_sim_tests.rs` rig helpers `boot_rig` `:123-131`,
  `boot_rig_with` `:135-150`, `fresh_rig` `:152-161` (format + boot over
  `SimBackstore` and an anonymous `HugeArena`), `FlakyFront` `:166-190` (a `FrontDoor`
  wrapper whose `copy_in` fails for chosen ids: the template for a reissuing front
  door), `rig_wait` `:192-205`, `placed_slot_data` `:208-218` (finds the data offset
  of the placed slot of `(vol, lba)` by scanning descriptors),
  `reattach_drops_a_damaged_slot_and_holds_its_volume` `:465-501` (`into_parts`,
  `disk.crash_daemon()`, flip a byte, `boot_rig`, assert `damaged_slots == 1`,
  `NvCause::DirtyBoot`, `NvFailed` on read, `PeekSource::Unmapped` on disk);
  `a_daemon_crash_lets_the_device_finish_what_it_had_and_loses_only_the_completions`
  `:1171-1202` (the device's crash semantics); `crash_in_phase` `:1245` and `PHASES`
  `:1237-1243`; `shutdown_during_the_reattach_boot_commit_flushes_the_replayed_writes`
  `:1930`.
- **The engine behind the ublk front door over the mock**:
  `crates/otap-ublk/tests/engine_tests.rs`: `UblkEngine` type `:22`, `geometry()`
  `:24-36`, `config_for` `:48-68` (pacing delay 0 under virtual time, `:66`),
  `front_door(vol, size)` `:70-79` (`MockTransport::new(DEPTH)`, `Watermarks::for_depth`),
  `boot_with` `:85-97` (a 2 MiB anonymous arena, tick until serving), `wait(e, tag)`
  `:124-135` (`take_commit(tag)` per tick), `write`/`read` `:146-165`;
  `an_abort_mid_flight_drops_the_completion_and_keeps_the_engine_healthy` `:357-432`
  is the closest to a reissue test: writes beyond the arena are deferred in the engine,
  one is aborted, the rest complete, `stats().stale == 1`, data checked by reads. A
  reissue test is this plus: `into_parts()`; `disk.crash_daemon()` on the
  `SimBackstore` it hands back (these tests run on one, `:22`), which completes every
  pending device write and drops the completions
  (`crates/otap-sim/src/backstore.rs:273-279`): skipped, D1's late completions reach
  D2, whose backstore tags restart at 1 (`next_tag: 1`,
  `crates/otap-cp/src/engine.rs:2053`), and can be taken for D2's own (the
  adversarial rig crashes its disk the same way,
  `crates/otap-sim/tests/adversarial_sim_tests.rs:473-474`, and so does
  `Sim::crash_daemon`, `crates/otap-sim/src/harness.rs:1189-1190`); a new
  `UblkFrontDoor` over a new `MockTransport` that re-delivers the in-flight writes
  (from the test's own record of what it submitted on each tag: the mock keeps no
  descriptor per tag, 5.1 item 6); `UblkEngine::boot` with the memory and the clock
  `into_parts` returned, ticked until serving (`engine_tests.rs:112`; not
  `boot_with`, which allocates a fresh arena, `engine_tests.rs:91`); and reads proving
  every block holds the write's pattern once. The power-cut test is the same sequence
  with `crash_power` and a fresh arena (`engine_tests.rs:296-303`).
- **The daemon's crash tests**: `crates/otap-daemon/tests/daemon_tests.rs`
  `a_dirty_restart_holds_the_volume_under_nvfail_until_it_is_cleared` (`:663-721`) is
  the in-process pattern: the daemon is dropped on its heap arena without a stop
  (`:666-679`), booted again, and the test checks that `last_boot` starts with "dirty
  boot", that reads and writes answer `nvfailed`, and that a clear brings the volume
  back (`:680-706`). The only hugetlbfs re-attach test is
  `scripts/linux-smoke.sh:251-277`: `SIGKILL`, `wait`, restart, "arena re-attached" in
  `pool status` (the `Booted` line, `pool.rs:353-357`), the volume online, and a `cmp`
  of the 16 MiB written before the kill. It has never run (E38 `docs/07:1415-1418`),
  and its note at `scripts/linux-smoke.sh:260-262` ("the recovery re-attach is a Step
  6 item") must change when the recovery lands, which is also when it should check
  that the old device, not a new one, serves the volume. Windows cannot map a file
  (`hugetlb.rs:480-493` answers `Unsupported`).
- **State-machine property over the mock**: `frontdoor_proptests.rs` `Op` `:29-52`,
  `run` `:244`, config `cases: 256` `:468-472` (overridden by the gate, T1).
- **The gate**: `PROPTEST_CASES=10000 cargo test --workspace -j 3 -- --test-threads=6`
  (`docs/09:57`) is the local gate; the documented merge gate is 100,000 cases, which
  no CI runs (E09 `docs/07:209-211`: "no CI exists yet to run it") and which has run
  once, on the stream suite only (`docs/06:344`: "10,000 cases run locally on every
  change, 100,000 on the stream suite once"). The explicit helper
  `proptest_cases(default)` in `step5_sim_tests.rs:1378-1383` lowers the plain-run
  default (64) while the environment still wins (`:1385-1391`), as do
  `crates/otap-sim/tests/step4_sim_tests.rs:1387` (`proptest_cases`) and
  `review_step5_sim_tests.rs:914` (`cases`); the gate took 180 min after Step 5
  (`docs/06:399`), and `-j 3`/`--test-threads=6` keep a core free for the OS.

## 7. Risks, pitfalls and traps

1. **Today's devices cannot be reissued, nor bound to a volume.** `UblkDevice::create`
   sets `USER_RECOVERY` but not `REISSUE` (`device.rs:95`); the flags are fixed at
   `ADD_DEV` (K4). The first crash of a Step 5 device fails every request the daemon
   had been handed with `EIO` (K1) even though the placed ones are durable; the
   never-handed ones are requeued (5.0 table row 7) and redelivered only if D2
   recovers the device, which for an unbound Step 5 device takes the weaker match
   below. Nor does a Step 5 device carry a binding: nothing writes `ublksrv_flags`
   (`device.rs:90-97` leaves it 0 through `..UblksrvCtrlDevInfo::default()`), so the
   match of 5.1 item 4 cannot bind
   it to a volume, and since another ublk server's device may carry 0 as well, the
   reaper cannot prove it this pool's and must leave it alone (5.1 item 5): it stays
   `QUIESCED`, its requeued requests waiting (4.7 step 3), until the operator removes
   it, which fails them (`ublk_stop_dev` → `ublk_unquiesce_dev` with `force_abort`,
   `ublk_drv.c:1572-1591`, `:1271-1272`; 4.7 step 6), or the lead accepts a weaker
   match (open question 2), and a device so recovered keeps the old flags. Under open
   question 2's recommendation (leave it to the operator) its never-handed requests
   are therefore never redelivered either. Only newly created devices get the new
   behaviour.
2. **The pushed-but-unsubmitted commit window is real on Linux, and the mock does not
   produce it by itself.** `LinuxTransport::commit` pushes an SQE (`device.rs:325`)
   that reaches the kernel at the next `poll` (`:284`), that is at the next `fetch`
   with nothing ready (`otap-ublk/src/frontdoor.rs:439-441`): typically the next tick,
   but not under back-pressure. A kill in that window leaves writes the engine believed
   acknowledged and the kernel considers in flight. The full-ring submit never fires
   (F3), and with a tag `Ready` while the gate is closed `fetch` returns `None` without
   pumping (`otap-ublk/src/frontdoor.rs:442-451`). Once a saturated device's gate has
   closed (`in_engine` at `high = depth`) and the commits of its first completions went
   out with a pump that brought new requests back, every later completion (a paced,
   intent-waiting or deferred write finishing) stays unsubmitted until `in_engine`
   falls to `low = depth / 2` (`:90-96`), which while writes are deferred at the stall
   can take until a CP has drained the arena: many ticks. That is a latency defect
   (finished requests are not reported to the kernel), and it widens the population a
   kill turns into reissues, or into `EIO` without `REISSUE` (5.0 table row 2). The
   fix is small: submit at the end of each tick that completed something (a
   `FrontDoor` hook `tick` calls), or in `UblkFrontDoor::complete` (one
   `io_uring_enter` per completion); open question 13. The mock commits instantly
   (`mock.rs:208-224`), but a test reproduces the window without a knob: from its own
   record (5.1 item 6, section 6)
   it re-delivers on the new `MockTransport` a write whose commit the old mock
   recorded, which is exactly the kernel re-dispatching a write whose commit SQE never
   reached it; D1's engine and `UblkFrontDoor` are in the Linux state (`front.complete`
   ran and retired the tag, `otap-ublk/src/frontdoor.rs:499-518`). A knob that holds
   commits until the next poll would make the mock produce the window on its own (a
   realism convenience for a test that kills at arbitrary ticks, not a requirement).
3. **`Reattach.placed` is not kept** (`assemble` consumes it, `engine.rs:1820-1864`);
   `DirtyExtent` has no CRC; the per-block CRC and the per-slot `seq` must be read
   back from `Arena::descriptor` (`arena:1190-1195`). `PlacedSlot` has both but is
   gone after the boot. And a replayed extent is a piece, not a request: `runs_of`
   joins slots of one `seq` only while vol, lba, slot and lap continue
   (`engine.rs:6155-6160`), so a write a later overwrite split comes back as several
   pieces with one sequence (`index:573-574`) and a kill inside `place` leaves a
   prefix (5.0 point 6); nothing records the request's extent, whether it was
   acknowledged, or its tag.
4. **The attach is not read-only**: it rewrites descriptor words (`arena:933-935`) and
   the attach counter (`:943-946`). A diagnostic that wants to look at an arena must use
   `can_attach` and raw reads (`placed_slot_data`, `adversarial_sim_tests.rs:208-218`
   is the pattern), never `attach`; on Linux the `flock` refuses a second mapping
   anyway (`hugetlb.rs:346-355`). The same rewrites erase a damaged slot's evidence
   before the boot has recorded its hold (pitfall 22).
5. **The boot CRCs every Placed block, and under a trusted barrier one CP's data too.**
   `scan_on_attach` runs `crc32c` over the data of every Placed slot (`arena:861`). For
   a nearly full 8 GiB arena that is up to 8 GiB of CRC-32C before the boot CP even
   starts. Before the attach, under a trusted opaque barrier, `boot::evaluate` also
   reads every stripe the selected CP wrote in full and CRCs each data block, after a
   `kill -9` as after a power loss (`boot.rs:629`, `:644`, `:654-672`; B2), unless the
   selected CP is a clean shutdown's final one (a `kill -9` after that CP's `A` slot
   landed boots clean and reads the summaries only): one CP's worth of reads from the
   disk. The 2-second budget of the brief (`docs/10:606`) includes both, the boot CP
   (stripes, metadata, flush, two FUA writes) and the
   kernel's recovery handshake. Worth measuring on the Linux host before the budget is
   promised. If the budget must include the attach's CRC pass, the way out is lazier
   verification (a slot's data checked at its first read, as `read_block` already
   does, `engine.rs:3678-3681`, or at staging, `:4495-4501`), which changes what the
   boot can hold: E4's `arena_whole` is decided at the attach (`:1869-1870`), so a
   damaged `Placed` slot found later would be replayed, served `Corrupt` and staged
   under its recorded CRC with no hold, and the attach would no longer keep only
   slots whose data verifies (`arena:59-62`; invariant 3, `:55`). That needs a ruling
   (open question 6).
6. **`START_USER_RECOVERY` waits for the corpse, and sometimes for a timeout.** It
   (`ublk_ctrl_start_recovery`, `ublk_drv.c:2694-2736`) needs `UB_STATE_OPEN` clear
   and the device `QUIESCED`, else `EBUSY` (`:2719-2722`): the dead process must have
   released `/dev/ublkcN`, which
   for a process with an 8 GiB mapping and a core dump in progress is not instant (the
   arena is excluded from dumps when `exclude_from_core_dumps` is called,
   `hugetlb.rs:212`), and `quiesce_work` must have run. When every tag was owned by
   the daemon at the kill (4.7 step 1, second trigger) nothing is cancelable and the
   quiesce is scheduled only by `ublk_timeout` (`ublk_drv.c:1209-1248`) at the block
   layer's request timeout, the default (30 s in mainline) since the tag set sets none
   (`:2150-2160`). The recovery sequence must therefore retry `START_USER_RECOVERY` on
   `EBUSY` (or poll `GET_DEV_INFO` until `state == QUIESCED`), and a 2-second budget
   measured from the old daemon's death cannot be met in that case; measured from the
   moment the device is `QUIESCED` it can. Retry only a `LIVE` or `QUIESCED` device: a
   device created and never started is `DEAD`, never becomes `QUIESCED` and answers
   `EBUSY` forever, or `EINVAL` while its creator's fetches are unprocessed, which is
   forever when the creator died before they were (K2); it is the reaper's only when
   it is this pool's and its creator is gone (5.1 item 5). And `START` answers
   `EINVAL` on a `QUIESCED` device whose earlier recovery was interrupted, and on one
   whose recovery another daemon is running, until that daemon's last fetch is
   processed (`nr_queues_ready == 0`, tested at `:2704-2705` before the `EBUSY` test;
   K2): a second crash between `START_USER_RECOVERY` and the processing of the last
   `FETCH_REQ` leaves a device only the reaper can take, once it knows the recovering
   daemon gone, and its requeued requests fail (5.1 item 5). Keep that window short:
   `START_USER_RECOVERY` only after the boot succeeded, and the fetches submitted
   right after it (5.1 item 4).
7. **The tag is not an identity across the crash.** `TagSlot.gen` restarts at 0 in D2;
   the kernel's re-dispatch may use another `req->tag` (blk-mq assigns it at dispatch;
   `ublk_setup_iod` indexes `ios[req->tag]`, `ublk_drv.c:939-940`); nothing marks a request as
   reissued (K5). Any design keyed on tags is wrong. Request ids repeat across a
   restart too: `request_id` is `gen << 32 | q_id << 16 | tag`
   (`otap-ublk/src/frontdoor.rs:213-215`) and a new front door starts every tag at
   `gen` 0 (`:265-272`), so D2's first request on a tag has the id D1's first request
   on it had. Harmless to the engine; a log or diagnostic that correlates ids across
   restarts must not take them as unique.
8. **The partial `place`**: a kill inside `transition` (`arena:1231-1233`) replays a
   prefix (section 5.0 point 6). The sim cannot produce it (crashes happen between
   ticks); an arena-level test can, and one does: `KillMidPlace(i, j)` in
   `reattach_after_any_interleaving_keeps_exactly_the_placed_slots` places a run and
   rewinds its slots `j..` to `Filled` before the attach
   (`crates/otap-arena/tests/adversarial_arena_tests.rs:717-740`; section 6).
9. **`hold` on `orphaned_blocks > 0`** (`engine.rs:1870`) is defensive: the engine
   cannot orphan a run today except through a rewind, where `rewound` already holds.
   A volume created at runtime is `creating` from `create_volume` (`:2283-2284`)
   until `finalize` of the CP that recorded it clears the flag (`:5923-5926`,
   `created_in` set at `:5413`), and `serve` refuses every request to a `creating`
   volume (`:3404-3412`; E17 "Durable before serving", `docs/07:430-436`), so a
   volume whose creation CP never committed has no `Placed` run; a deletion needs the
   volume offline with nothing unflushed and no CP in flight touching it
   (`check_vol_op`, `engine.rs:2584-2599`), so a deleted volume leaves none either. The
   absent-volume branch (`:1823-1833`) is therefore reached only when the boot rewound
   past the creation CP (`rewound`, `boot.rs:251-253`: a torn newer candidate, one that
   read back and did not verify, was skipped, B1, and the hold is then `TornCp`
   anyway), and the insert-error branch (`engine.rs:1851-1861`) is unreachable: `fill`
   and `fill_hole` checked `vol <= MAX_VOL` and `lba + blocks <= MAX_LBA + 1`
   (`arena:1101-1103`, `:1131-1133`; a hole's `blocks
   >= 1` and the same bound again at `decode`, `:327-330`), which is exactly what
   `Index::insert` and `Generation::insert` test (`index:559-572`, `:480-484`;
   `MAX_VOL = MAX_VOLS - 1`, `crates/otap-format/src/constants.rs:54-56`), and the
   input is sorted by sequence with equal allowed (4.4 step 5), so `StaleSeq` cannot
   occur. Both branches are worth keeping (a run of an unknown volume must be released,
   never inserted), but only an adversarial arena (a descriptor written for an absent
   volume with `write_desc`, `adversarial_arena_tests.rs:83`) produces
   `orphaned_blocks > 0` without a rewind. A reissued write to the rewound-away volume
   answers `NoSuchVolume` → `ENODEV` (`engine.rs:3404-3412`;
   `otap-ublk/src/frontdoor.rs:201`) in the daemon, whose boot configures no volume
   (`pool.rs:244`). Under a configuration that lists the volume (the harness's), the
   boot recreates it present and creating (`engine.rs:1791-1803`), and the replay,
   which tests `present` only, not `creating` (`:1823`), inserts the rewound-away
   runs into it instead of orphaning them; it is held `TornCp` (`:1800-1802`), so a
   reissued write answers `NvFailed` → `EIO` once the boot commit has created it. A
   refused release at replay (`:1827-1830`, `:1840-1843`, `:1856-1859`) surfaces only
   as `Event::Internal`. A refused insert does more (4.4 step 5): besides its
   `Event::Internal` (`:1853-1855`) it adds the run's blocks to `orphaned_blocks`
   (`:1851-1852`), which `Event::Booted` carries (`:1996`), the daemon's journal line
   prints (`pool.rs:364-366`) and which makes a dirty boot hold the write-active
   volumes (`engine.rs:1870`; 5.0 case (c)). Nothing else lets a creating volume hold
   a `Placed` run (`serve` refuses one, `:3404-3412`); a change that lets writes reach
   a creating volume would replay into it.
10. **Sequence numbers restart above the arena's maximum, not the index's.** After a
    boot `newest_seq` is rebuilt from the replay only (`index:575`), while
    `arena.next_seq` is the maximum over every descriptor including `Committed` and
    reserved ones (`arena:810`, `:932`). This is the safe direction; do not "reset"
    either.
11. **The `kind` word and the header version**: a 1.3 that changes the descriptor must
    bump the **major** (`arena:211`) or an old daemon will attach a new arena and treat
    stamped slots as `Damage::Kind` (`:890-898`), holding every volume; a descriptor
    size change on an existing file gives `BadGeometry` → `arena_lost` (`engine.rs:
    1702-1708`) and the acknowledged writes are gone. The only fixed 1.2-compatible
    free bytes are in the header, outside its seal: `reserved` (4024 bytes at offset
    72, `arena:191`) and `rsvd1` (4 bytes at offset 60, `:186`); the descriptor
    table's padding is free too, but its size follows the slot count and can be zero
    (5.0).
12. **A heap arena has nothing to replay** (`hugetlb.rs:119-124`, S5-1 `docs/07:
    1049-1052`): every crash is a dirty boot that holds the write-active set, and
    with `REISSUE` the reissued writes to those volumes answer `EIO` until the clear,
    with one exception. A `kill -9` that leaves a clean shutdown's final CP the newest
    on disk (one after that CP's `A` slot landed, pitfall 23, or one during the next
    boot before its own commit's `A` slot landed, B2) boots clean (`dirty =
    !sb.is_clean_shutdown() || rewound` is false, `engine.rs:1767`), so `hold` is
    false (`:1870`) and nothing is held over the blank arena. That is harmless: the
    final CP carries every write the arena held (5.0, the end of (c)). Both documents
    make the broader claim ("a daemon crash on a heap arena is a dirty boot that holds
    the write-active volumes under nvfail", `hugetlb.rs:122-124`; S5-1,
    `docs/07:1049-1051`). Recovery of the device is still worth doing (the client sees
    `EIO` instead of a vanished disk), but the test for "no write lost after kill -9"
    only holds with a file arena.
13. **The device's params outlive the daemon, and `--arena-bytes` must not grow across
    a crash restart.** `dev_sectors` (`ublk_apply_params`, `ublk_drv.c:2266` →
    `set_capacity`, `:523`) and `max_sectors` (the queue limits, `.max_hw_sectors =
    p->max_sectors`, `:2198`, which `blk_mq_alloc_disk` receives at `:2254`) were
    applied at `START_DEV` and are not re-applied at `END_USER_RECOVERY`; `SET_PARAMS`
    is refused while `LIVE` (`:2653`). A volume resized before the crash (the daemon
    recreates devices after a resize, E38 `docs/07:1358`) means the old device no
    longer fits; so does a smaller `--arena-bytes` (`DeviceSpec::for_arena`,
    `device.rs:60-66`; `TooLarge` at `engine.rs:3434-3437`), but only with a heap arena
    or after the arena file was deleted: a hugetlbfs file longer than the new size is
    refused (`HugeArena::map_file`, `hugetlb.rs:358-367`), so `open_arena` fails and
    `Pool::boot` stops before any device (`pool.rs:241`). Compare
    `GET_PARAMS`/`GET_DEV_INFO` with the volume and the arena before recovering, else
    reap and recreate. The dangerous change is the opposite one: a **larger**
    `--arena-bytes` makes `map_file` grow the file (`hugetlb.rs:368-370`), the sealed
    header's geometry no longer matches `Geometry::for_len` (`arena:217-224`,
    `BadGeometry`), and `Engine::boot` recreates the arena with `arena_lost = true`
    (`engine.rs:1702-1708`): every acknowledged write the arena held is destroyed and
    the write-active set is held (5.0 case (c)), a hold that a stop of that boot
    before its boot commit's `A` slot lands loses (pitfall 22). Nothing refuses it today
    (`check_arena_bytes` checks only the bounds and the 2 MiB multiple,
    `crates/otap-control/src/units.rs:50-71`); after a clean shutdown it is harmless
    (the hold needs `dirty`, `engine.rs:1870`, and the arena holds nothing the selected
    CP lacks: nothing is `Placed` once the final CP's `finalize` ran, and a kill
    between that CP's `A` slot and its `finalize`, which still boots clean, leaves
    `Placed` only slots that CP carries: pitfall 23).
14. **Precise nvfail mode and reissue**: a reissued write to a volume outside
    `durable_active` waits for an intent (`engine.rs:3611-3617`) like any other; the
    intent machinery restarts from the slots (`:1892-1897`, `:2048`). After the boot
    commit that is only a volume with nothing replayed: the boot commit's slot lists
    every replayed volume that is not held, and the landing of its `A` copy puts them
    in `durable_active` before the engine serves (open question 9). Fine, but a
    recovery test in precise mode must tick until the intent lands.
15. **The replayed generation can be cut before serving** (`maybe_cut` runs every tick,
    `engine.rs:2114`; `ArenaHalf`/`Timer` need only `active_blocks > 0`, `:3901-3904`).
    Exactly those two can: `ArenaHalf` at an occupancy of `cfg.arena_half` (0.5 by
    default, `:184`), which a large replay meets at the first tick, and `Timer` when
    the boot commit outlasts the CP interval (`last_cut_ns` starts at the boot,
    `:2065`; 5 s on flash and 10 s on HDDs, `traits.rs:190`, `:206`, which the daemon
    replaces with `--cp-interval`, `daemon/src/backstore.rs:85-86`). `Request` needs
    `serving` (`engine.rs:3899`); `BackToBack` needs an `Idle` engine with nothing
    frozen (`:3887`, `:3905`), which the boot CP, or its retry started earlier in the
    same tick (`:2099-2107`), rules out; `Shutdown` needs `serving_or_booted()`
    (`:3888-3891`, `:3933-3935`), false until the boot commit's `finalize`
    (`boot_done` is `stats.cps > 0`, `:3937-3939`); and nothing is cut while a
    generation is frozen or the pool is full (`:3886`, `:3897`). A dedupe that assumes
    "the replayed entry is in the active generation" is wrong; a reissue that
    re-places it is still right.
16. **`StaleSeq` would be an internal error on the write path** (`engine.rs:3539` → `tick`
    returns `Err`). It cannot happen today (sequences come from the arena at placement,
    E20 `docs/07:545-547`); any new code that inserts with a sequence it did not just
    take from `arena.next_seq()` (a dedupe that re-inserts the old entry, say) risks
    it.
17. **Round-robin fetch and the 64-per-tick cap** (`daemon/src/frontdoor.rs:278-296`,
    `engine.rs:3360`): a reissue burst of `depth` requests per device interleaves with
    the other devices and takes a few ticks; the watermarks (`for_depth`, high = depth)
    never block it, but a full arena defers it (`:3448-3450`, `:3497`).
18. **Model semantics in the sim**: `Model::settle` deletes every op above the prefix,
    including unacknowledged in-flight ones (`model.rs:335-343`), `issue` asserts
    increasing op numbers (`:276`), and the prefix follows op order (`:14-25`). A
    reissue test that re-submits an op under its own number, reusing its data
    (`data_of`, `:256`) without a new `issued` call, therefore passes only when every
    block ends up holding the newest op that covers it, which, with every in-flight
    op re-submitted, holds when every later-issued op that overlaps one is
    re-submitted after it. That is always so in conservative mode with the in-flight
    ops re-submitted in op order (the acknowledged ops are then a prefix of the issued
    ones), and need not be under a re-dispatch out of op order or in precise mode,
    where a later write can be acknowledged before an earlier one (5.1 item 6); in
    general the test must settle at the boot and re-submit the in-flight writes as new
    ops, or the model must order by something other than the op number (5.1 item 6).
    Otherwise the checker refuses outcomes that 5.0 point 3 shows are correct.
19. **`refuse_everything` completes paced writes `Ok` on a stop**
    (`engine.rs:3396-3398`) but a `kill -9` never runs it; the paced write is replayed
    and, with `REISSUE`, delivered
    again. Without `REISSUE` the client sees `EIO` for a write that is durable. This is
    the honest answer; it is not a bug.
20. **The `Io*` channel front door** (`daemon/src/frontdoor.rs:66-72`) has no kernel
    behind it: a crash loses its queue entirely (the requester's `Receiver` sees a
    hang-up). Nothing to reconcile there.
21. **A `fill` killed between its field stores and its word store leaves a slot the
    attach neither notes nor rewrites.** `fill` stores `addr`, `seq`, `crc32c`, `kind`
    and then the word, slot by slot (`arena:1105-1113`); a kill after `:1108-1111` and
    before `:1112` leaves the reservation's `Free` seal over changed fields: the word
    is non-zero, `verify()` is false (`:413-415`) and `sealed_as()` is `None`
    (`:422-433`). The `Free` arm of `scan_on_attach` handles only a verifying word and
    a `sealed_as` word (`:816-833`); its comment (`:825-826`) knows the interrupted
    fill and lets it through, so the slot keeps `Free` and the reservation's lap. When
    the kill hit any slot but the run's last, the run's later slots are `Free`-valid
    and noted (`:820`) and `head` passes the stale slot (`:925`). Whether it then lies
    inside `[tail, head)` depends on what the attach keeps before it: `tail` is the
    lowest kept `Placed` position, or `head` when nothing `Placed` is kept (`:926`).
    With a kept `Placed` slot before it, it sits inside the window; `passable` never
    passes a `Free` slot of the tail's own lap (`:1278-1282`), so once the CPs commit
    everything before it the tail stops there (`:1284-1288`) and, within one more lap,
    `reserve` answers `Full` for every request (`:1016-1024`): every write is deferred
    (`engine.rs:3497`, retried in order `:3354-3359`, punches behind it too
    `:3364-3368`), reads and flushes still pass (E3), nothing is reported, and the
    clients' writes hang (the kernel resets its timer while the daemon lives,
    `ublk_drv.c:1224-1225`). With none, the tail starts past it (at the first kept
    `Placed` slot beyond it, or at `head`) and the slot lies behind the window: the
    attach that found it, the first one after the kill included, has already cleared
    it, and `reserve` reuses it when the head comes round (`Free` is reusable,
    `arena:154-156`, `:1045-1046`). So a restart clears a stuck slot only when its
    attach finds no `Placed` slot before the stale one (`:926`). Killed on the run's
    last slot (every single-block write and every hole record, `:1122-1145`) it is
    benign: nothing beyond it is noted, `head` lands
    on it and the next `reserve` re-seals it. The crate's doc (`:51-52`) calls the
    state "what a never-used slot looks like too", which holds for the word only: a
    never-used slot has a zero word and lies ahead of the head. The sim cannot produce
    it (crashes happen between ticks, pitfall 8);
    `a_reservation_never_filled_pins_the_window_until_a_reattach`
    (`adversarial_arena_tests.rs:248-289`) pins the same stall for a verifying
    reservation and shows the attach releasing that one (`:281-283`), not this one.
    **Arena fix to propose** (inside `scan_on_attach`, no layout change): rewrite a
    non-zero `Free` word that neither verifies nor is sealed as another state
    `Committed` on its recorded lap, `rewrite.push((slot, SlotState::Committed,
    lap))` as `arena:821` does, which releases it, but without the `note_pos()` of
    `:820`. The attach notes only words that verify (`:820`, `:838`, `:860`, `:913`),
    since `head` is the highest noted position plus one (`:925`) and a window wider
    than the ring is `Inconsistent` (`:927-931`), which `Engine::boot` turns into a
    recreated arena with `arena_lost` (`engine.rs:1690-1694`). Noting adds nothing
    for an interrupted fill (the run's later slots are `Free`-valid and noted,
    `arena:820`; on the run's last slot the case is benign) and would let an
    unverified lap drive `head` (lap and position come from the word whether or not
    it verifies, `:811-814`): a `Free` word damaged in its lap bits, which today's
    attach ignores and the tail passes (`:1278-1282`), could put a position laps
    ahead into `max_pos` and, with a `Placed` slot kept, make the attach fail and the
    boot destroy every acknowledged write the arena held. The `Committed` rewrite alone
    lets the tail pass the slot (`:1278-1282`) and `reserve` reuse it
    (`:1045-1046`); like the rewrites of an unverifiable `Filled`, `Placed` or
    `Committed` word (`:848`, `:857`, `:920`), it seals the recorded lap unverified,
    which a later attach notes (`:913`) until `reserve` re-seals the slot. (The
    alternative, reporting it as `Damage::Descriptor` like an unverifiable `Filled`
    word, would hold the write-active set after any kill inside a multi-block write,
    which defeats the recovery; neither choice detects a `Placed` descriptor damaged
    in both its state bits and other bytes, and the code today does not either.)
    Test, in
    `crates/otap-arena/tests/adversarial_arena_tests.rs`: place a run and leave it
    `Placed` (`placed_run`, `:63-70`), reserve a multi-slot run after it, and change
    the `addr` of one of its slots other than the last under its untouched word with
    `read_desc`/`write_desc` (`:78-86`; `set_state`, `:92-95`, re-seals and is the
    wrong helper here); after the attach assert that slot `Committed` (today it stays
    `Free`), then commit the kept run and assert the window empty (today the tail
    stops at the slot: `window().0` is its position and `occupied()` is not 0). The
    `:248` pattern alone does not show the defect: that test commits every run it
    fills (`:255-270`) and re-attaches with nothing `Placed` (`:279-283`), so whatever
    reservation it leaves stale, today's attach sets `tail = head` (`arena:926`) and
    `occupied() == 0` passes without the fix. Derived from the code, not executed.
22. **A hold that a stopped boot never recorded is lost.** A dirty boot decides its
    hold in memory (`engine.rs:1866-1885`) and records it only in its boot commit's
    slot (`:5647-5662`, `:5674`; the `A` slot built at `:5541`), but before that its
    attach has already rewritten every damaged slot `Committed` (`arena:933-935`) and
    `Arena::create` has already overwritten a lost or blank arena (`engine.rs:1691`,
    `:1704`). A boot that stops in between (a second `kill -9`; `Pool::boot` giving up in
    `wait_until`, `pool.rs:280`, `:523-556`, which is also where a boot commit that
    keeps failing, or that failed for want of space, ends: 4.4 step 8; a `start_job` error,
    `engine.rs:1714-1725`) leaves the next boot a whole arena and no cause to hold, so
    volumes that lost acknowledged writes serve without nvfail (5.0, the gap; B1).
    **Engine fix to propose**, no layout or format change, one of two: (1) let
    `scan_on_attach` return its rewrites (`arena:933-935`) instead of applying them,
    and let `Arena::create` defer its writes (`arena:714`, `:726`), until the boot
    commit's `A` slot has landed (`engine.rs:5571-5577` is where the engine learns
    it); until the engine serves (after `B`, E5) nothing else writes the arena but the
    replay's releases (`engine.rs:1827`, `:1840`, `:1856`), of which a superseded
    run's is harmless (the run that superseded it is replayed again) and an orphan's
    should wait with the rest; or (2) record the pending hold first, in a
    self-checking word in the arena header's reserved bytes (outside the seal like
    `attach_word`, 5.0; 1.2-compatible), stored before the attach's rewrites and inside
    the header `Arena::create` writes, honoured by every later dirty boot and cleared
    once an `A` slot that records the hold has landed. Tests, in the
    `crates/otap-sim/tests/adversarial_sim_tests.rs` rig: the damaged-slot boot of
    `reattach_drops_a_damaged_slot_and_holds_its_volume` (`:465-501`) on a stalled
    disk, killed (`into_parts`, `disk.crash_daemon()`) before its `A` slot lands, then
    `boot_rig` and the same `NvCause::DirtyBoot` assertion (`:487`); and through the
    harness, `crash_power`, `stall_boot_into` `Phase::Metadata` or `Phase::Flushing`
    (`:1325-1338`; in `CommitA` the device finishes the slot's write at the crash,
    E18 `docs/07:478-479`), `crash_daemon`, `boot`, then the hold asserted on the
    engine directly, as `adversarial_sim_tests.rs:998-1001` does, because
    `verify_recovery` judges the last stop as `Stop::Daemon` (`harness.rs:1187`) and
    fails on the writes the power cut lost with or without the fix
    (`harness.rs:1285-1290`). Derived from the code, not executed.
23. **A kill between a CP's `A` slot and the end of its `finalize` replays writes a
    durable CP already carries.** The CP is durable once its `A` slot lands:
    `SuperblockSlot::rank` orders the slots newest first by
    `(owner_epoch, cp_seq, cut_seq, A copy)`, so CP N's `A` copy outranks every slot
    of N-1 (`crates/otap-format/src/superblock.rs:793-811`), and `boot::evaluate`
    takes the first candidate that loads (`crates/otap-cp/src/boot.rs:286-291`). But
    `finalize`, which releases the generation's slots
    (`crates/otap-cp/src/engine.rs:5873-5893`), runs only once `B` landed
    (`:5580-5582`), and `B`, the same slot re-sealed for its `B` location
    (`:5545-5549`), is issued only after `A` completed (`:5571-5578`). So a `kill -9`
    at any point from the landing of `A`, through the whole `B` write, to the end of
    the release leaves the CP durable and its slots `Placed`. The next boot selects
    that CP, the attach keeps the slots, and the replay, which has no filter by
    sequence (`:1820-1864`), puts them into generation 1 again; a later CP stages them again
    under its own birth (`build_stripe` stamps each staged block with the CP's
    sequence, `:4488`, `:4507`; a hole goes into the log with the CP as birth, E23
    `docs/07:681-683`). The data is the same, so nothing is lost or wrong, but `diff`
    yields every entry with `birth_cp > base` (`crates/otap-map/src/lib.rs:47`; the
    filters at `:1649`, `:1659`, `:1828`), so a snapshot diff and scope (C)'s
    replication walk ("extents and holes with `birth_cp > base_cp`", `docs/10:623`)
    see those blocks as changed. The boot cannot skip them today: the superblock slot
    records no write sequence (`cp_seq`, `cut_seq`, `cp_nonce`, `intent_seq_seen`,
    `crates/otap-format/src/superblock.rs:445-453`; `max_seq` lives only in the job and
    in `Event::CpCommitted`, `engine.rs:361-372`), so a filter needs a format change or
    a word in the arena header's reserved bytes (like pitfall 22's option 2). The
    simulator can reach the window: `SimBackstore::crash_daemon` completes every
    pending write and drops the completions (`crates/otap-sim/src/backstore.rs:270-279`;
    a superblock slot lands durable at once, `:315-319`), so a daemon crash with the
    `A` or the `B` write pending lands it and never runs `finalize`. The final CP of a
    clean shutdown is no exception: it carries `RunState::CleanShutdown`
    (`crates/otap-cp/src/engine.rs:3957-3961`, written into the slot at `:5607`), so a
    `kill -9` inside its window gives a clean boot
    (`dirty = !sb.is_clean_shutdown() || rewound`, `:1767`) that re-attaches the arena
    (unconditionally, `:1687-1689`) and replays the final generation's `Placed` slots;
    so does a `kill -9` during that boot before its own commit lands its `A` slot
    (every boot commit is written `Running`, `:1713`, `:2105`; B2). Nothing is at risk
    (that CP carries them, and a clean boot never holds, `:1870`, even over a blank
    heap arena, a damaged slot or a lost arena: 5.0, the end of (c); pitfall 12), but
    a clean boot can find slots `Placed` (B1, pitfall 13), and under a trusted opaque
    barrier it skips the data CRC pass (B2).
24. **A restart that comes too early fails on the arena file's lock.**
    `HugeArena::map_file` takes an exclusive non-blocking `flock` and fails with
    `WouldBlock` ("<path> is mapped by another arena", `hugetlb.rs:346-355`) while the
    dying daemon still holds the file, as it does while it is still exiting or dumping
    core. `open_arena` reports it as an `Io` error (`daemon/src/arena.rs:39-53`),
    `Pool::boot` returns before the engine boot (`pool.rs:241`), and a configured
    `dbladed` exits (B1). The recovery, in the supervisor or in the daemon, needs a
    wait or a backoff on that error; `scripts/linux-smoke.sh` reaps the corpse before
    it restarts (`wait` at `:256`, `sleep 1` at `:258`). The same corpse holds
    `/dev/ublkcN` until it is gone, which is the `EBUSY` of pitfall 6.
25. **Device writes the dead daemon had in flight may land after the new one started.**
    `docs/06:551-554` records it for discards ("can still land after the new process
    reuses the segment ... the boot may have to wait for the device's queue to
    drain"). On Linux the discard itself is a synchronous `BLKDISCARD` here
    (`crates/otap-sys/src/block.rs:384-389`, called inline by
    `crates/otap-backstore/src/uring.rs:236-254`), but stripe, metadata, slot and
    intent writes go through the backstore's io_uring (`:179-224`;
    `crates/otap-sys/src/block.rs:1-5`): a lost CP's stripes and metadata go to space
    no committed CP names (the engine's own reasoning for a failed attempt,
    `engine.rs:4271-4274`, `:4361-4362`), which D2 may reuse, and a slot or intent
    write goes to a fixed slot D2 writes too (`crates/otap-cp/src/layout.rs:232`,
    `:245`). Whether a killed process's in-flight io_uring writes can complete after
    its successor started is a kernel property outside the saved sources, to be
    measured on the Linux host. The simulator models neither: `SimBackstore::crash_daemon`
    completes every pending op at once (`crates/otap-sim/src/backstore.rs:270-279`). Not
    an arena issue, but part of what the restart must reconcile (open question 6).

## 8. Open questions for the lead

1. **Set `UBLK_F_USER_RECOVERY_REISSUE` on new devices?** Without it a daemon crash
   returns `EIO` to the filesystem for every request the daemon had been handed (K1;
   only the never-handed ones are redelivered, on a device D2 recovers), which ext4
   treats as a disk error. The analysis in 5.0 says re-serving is safe. No decision
   adopts either flag's semantics yet: E28 records only that devices are created with
   `USER_RECOVERY` (`docs/07:849-850`) and E38 lists the re-attach as not done
   (`docs/07:1419-1423`); the brainstorm, which rules nothing, already says
   "`USER_RECOVERY` alone aborts in-flight requests; `USER_RECOVERY_REISSUE` requeues
   them and may issue a request twice" (`docs/01-architecture-brainstorm.md:205`) and
   proposes `USER_RECOVERY|REISSUE|USER_COPY` devices (`:213`).
   Recommendation: yes.
2. **Where does the device-to-volume binding live across the crash?** The kernel keeps
   `ublksrv_flags` (64 bits, verbatim, `ublk_drv.c:2434`, `:2578`) per device; a
   `(hash of agg_id, vol)` stamp there, behind a magic, needs no format or layout
   change. Alternatives: the arena header's reserved bytes (1.2-compatible but ties
   the binding to the arena file), a `/run/opentap/<pool>/devices` file (no format or
   layout change, and it lives about as long as the devices, both gone at a reboot;
   but E38 says "the daemon holds no durable state at all beyond its command line",
   `docs/07:1406-1409`, and a stale file can name a device id the driver has since
   handed to another device, since `DEL_DEV` frees the id for reuse,
   `ublk_drv.c:2532-2535`, whereas `ublksrv_flags` lives and dies with its device).
   Nothing writes the field today (`device.rs:90-97`), so the lead must also say what
   D2 does with an unbound device
   (every Step 5 device, pitfall 1), which it cannot tell from another ublk server's
   device (the field is "For ublksrv internal use", `ublk_cmd.h:237-238`; 5.1 item 5):
   leave it to the operator (it stays `QUIESCED`), reap it anyway (its requeued
   requests fail, the client sees the disk vanish, and another server's device may
   go with it), or recover it under a weaker match (say, exactly one unbound
   `QUIESCED` device and one online volume without a device, its `GET_PARAMS`
   `dev_sectors` equal to the volume's size). The lead must also say how the reaper
   knows a `DEAD` device's creator, or the daemon that started the recovery of a
   `QUIESCED` device whose `START` answers `EINVAL`, is gone (5.1 item 5).
   Recommendation: `ublksrv_flags`, written at `ADD_DEV` from Step 6 on as a magic, a
   hash of `agg_id` and the volume; leave an unbound device to the operator: a wrong
   match would serve one volume's requests from another, and an automatic reap can
   delete a device another server serves.
3. **Dedupe or re-place?** Re-placing is correct and simple; the dedupe saves a second
   copy in a CP only when generation 1 was cut before the reissue (pitfall 15), and
   only for a reissue whose bytes still match its replayed copy (5.0 point 1). If a
   dedupe is adopted after all, the lead must also say whether it is always on (a
   lookup, a CRC and a 4 KiB compare per incoming block, 5.0) or bounded to a
   recovery window after `END_USER_RECOVERY` (until the first cut, a timer, or a count
   of requests per queue), none of which is exact, since nothing marks a reissued
   request (K5).
   Recommendation: re-place; no dedupe.
4. **When to issue `END_USER_RECOVERY`**: after the boot commit (the engine serves) or
   before (the re-dispatched requests sit as unreaped completions in the queue's
   io_uring ring meanwhile, 5.1 item 4)? Recommendation: after `wait_until(is_serving)`
   (`pool.rs:280`), inside the same loop that creates devices today; nothing is lost
   either way.
5. **Policy for a quiesced device that no longer matches**: volume offline, deleted,
   resized, or the arena smaller than the device's `max_io_bytes` (a heap arena or a
   deleted arena file only, pitfall 13). Reap and recreate, or recover and let the
   engine answer `EINVAL`/`ENODEV`? The client's in-flight requests get `EIO` on a
   reap (`ublk_drv.c:1572-1591`). Related: should `dbladed` refuse to boot after an
   unclean stop when `--arena-bytes` differs from the existing arena file's length,
   since a larger value silently destroys the arena (pitfall 13)?
6. **Is the 2-second budget measured from the new daemon's start or from the old
   daemon's death?** The kernel's quiesce (`ublk_drv.c:1532-1545`) and the release of
   the char device are outside the daemon's control; the attach CRC pass and, under a
   trusted opaque barrier, the boot's read and CRC of one CP's data (pitfall 5, B2) are
   inside, and a transient read error makes it a failed boot to retry (B1). When
   every tag was owned by the daemon at the kill, the quiesce waits for the block
   layer's request timeout (`ublk_timeout`, `:1209-1248`; the tag set sets
   none, `:2150-2160`, so the default applies, 30 s in mainline), and no front-door
   watermark prevents it: the watermarks gate what the engine takes, not what the
   kernel dispatches, and the kernel hands a tag to the server (`OWNED_BY_SRV` set,
   `ACTIVE` clear: `ubq_complete_io_cmd`, `:1072-1086`, "mark this cmd owned by
   ublksrv" at `:1075`) the moment it dispatches a request to it, reaped or not, which
   is why `ublk_timeout` counts that tag in flight (`:1227-1232`). A budget from the
   death is then out of reach unless the host shortens the block device's timeout
   (mainline exposes `/sys/block/<dev>/queue/io_timeout`; outside the saved sources,
   to be checked on the Linux host), so the recommendation is to measure from
   `QUIESCED`. If the budget must also hold the attach's CRC pass over a full arena,
   only lazier verification makes room, and it changes what a boot can hold
   (pitfall 5): a ruling, not an implementation detail. The dead daemon's device
   writes still in flight at the kill (pitfall 25) are one more wait to measure.
7. **Should `Event::Booted` or `PoolStatus` expose the replayed set per volume** (the
   hook in 5.1 item 1) for `otapctl`, or is the total enough?
8. **Should the partial-`place` replay (5.0 point 6) be left as is** (a prefix of an
   unacknowledged write lands) or should `attach` drop it? Dropping needs no run
   boundary: the sequence number identifies the write (one per request, never handed
   out twice: A5, `arena:932`, `:984-988`) and `fill` stores it in every slot of the
   run (`:1105-1112`), while a superseded piece goes `Placed -> Committed`
   (`engine.rs:3546-3548` through `Arena::commit`, `arena:1169-1173`; pinned by
   `adversarial_arena_tests.rs:310`) and is never `Filled` again under that sequence.
   So `Placed` slots share a sequence with verifying `Filled` slots only after a kill
   inside `place` (`arena:1231-1233`), and `scan_on_attach` already sees those
   `Filled` slots (`:835-838`): after its pass it could drop every kept `Placed` slot
   whose sequence one of them carries (rewritten `Committed`, the tail taken from what
   is kept). A change inside `scan_on_attach`, no layout change; the write was never
   acknowledged, so either outcome is within the contract.
9. **The precise-mode intent after a recovery**: a reissued write to a volume outside
   `durable_active` writes an intent before its acknowledgement (E9). The replayed
   volumes are not among them. The replay gives each one a window (`windows = 2`,
   `engine.rs:1836-1838`); the boot commit carries no generation, so its slot keeps
   every window as it stands (`dirty_now` is `None`, `windows_after = v.windows`,
   `:5626`, `:5663-5671`) and lists every replayed volume that is not held as
   write-active; building that slot narrows `durable_active` to that set (`:5689`) and
   the landing of its `A` copy unions the set back in (`:5571-5577`), all before the
   engine serves (`:5978-5983`) and so before `END_USER_RECOVERY` (5.1 item 4). The
   volumes that wait are those with nothing replayed: their windows are 0 at boot (set
   to 2 only at `:1837`, `:3609` and `:5905`), so the boot commit drops them from
   `durable_active`, even when the dead daemon had writes for them in flight that it
   never placed (5.0 table rows 3 to 7) or had flushed everything it placed. Is one
   intent on the first reissued write to such a volume acceptable on the recovery
   path, or should the boot commit also keep a window for every volume of
   `boot_write_active` (which keeps those volumes write-active, and so held by a power
   loss, until two later CPs carry no write to them)?
10. **Testing on Windows**: the reissue path can be driven end to end on the mock
    (5.1 item 6; the test keeps its own record of the in-flight requests, or the mock
    is extended to keep each busy tag's descriptor), and that record already
    reproduces the pushed-but-unsubmitted commit window (pitfall 2) by re-delivering a
    write whose commit the old mock recorded; a mock knob that holds commits until the
    next poll would only let the mock produce the window by itself. Should such a knob
    (a convenience, not a requirement) and a harness-level "crash with reissue"
    scenario (which must also accept the clear and deletion windows of 5.0, the second
    for any volume, held or not, by forgetting a volume whose deletion landed in the
    model and the configuration, not only in `held_at_stop`) be part of scope A's
    deliverable, and under the 10,000-case gate?
11. **The interrupted-`fill` slot (pitfall 21)**: should scope A's deliverable include
    the arena fix (rewrite it `Committed` at attach without noting its position,
    `arena:816-833`) and its test?
    A `kill -9` during any multi-block write can produce it, and when the attach keeps
    a `Placed` slot before it the result is a silent stall of every write after at
    most one lap, cleared only by a restart that finds nothing `Placed` before it
    (with nothing `Placed` before it, the attach already leaves it behind the tail).
    It is a change inside `scan_on_attach` and no layout change, so it is
    1.2-compatible.
12. **The hold a stopped boot never recorded (pitfall 22)**: should scope A's
    deliverable include the engine fix and its tests, and in which form: defer the
    attach's rewrites and `Arena::create`'s writes until the boot commit's `A` slot
    lands, or record a pending hold in the arena header first? Until it is fixed the
    guarantee of 5.0 carries a condition no boot can check (no earlier boot since the
    selected CP met the hold's condition, whether or not it had set the hold, and
    stopped after its attach's rewrites or `Arena::create`, before its `A` slot
    landed), and a daemon
    killed or stopped again during the boot that follows a power loss, a damaged slot
    or a larger `--arena-bytes` serves volumes that lost acknowledged writes without
    nvfail. Recommendation: the deferral, in scope A; it stays inside
    `Engine::boot`/`assemble` and `Arena::attach`/`Arena::create`, with no layout or
    format change.
13. **The commit cadence (F3, pitfall 2)**: should scope A also submit the pushed
    commits at the end of every tick that completed something (a `FrontDoor` hook that
    `tick` calls, a no-op for the other front doors), or in `UblkFrontDoor::complete`
    (one `io_uring_enter` per completion), so that a finished request reaches the
    kernel within a tick under back-pressure too? It is a latency fix on its own, and
    it narrows the window a kill turns into reissues (or `EIO` without `REISSUE`).
    Recommendation: the hook, in scope A; the mock can check it by its poll count
    (`MockTransport::polls`, `crates/otap-ublk/src/mock.rs:157-161`).
