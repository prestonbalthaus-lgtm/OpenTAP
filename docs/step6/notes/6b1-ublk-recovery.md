# Step 6B-1: ublk USER_RECOVERY re-attach and the reaper (Opus-Ublk-Recovery, 2026-10-16)

Base `217dad3` (6A); scope A minus the smoke script. No format or arena
change (2.3, 1.2); control protocol stays 1. `06`/`07`/`08`/README: the lead's.

## 1. The 6.12 contract, as verified in the saved `ublk_drv.c`

- Death with a fetch pending: io_uring cancels it (`ublk_uring_cmd_cancel_fn`)
  -> `ublk_abort_requests`/`ublk_abort_queue` -> `ublk_quiesce_work_fn`/
  `__ublk_quiesce_dev`: `QUIESCED`. A request the dead server held goes to
  `__ublk_fail_req`: requeued under `USER_RECOVERY_REISSUE`, `EIO` without;
  dispatched-not-delivered is requeued either way (`__ublk_abort_rq`).
- Death holding every tag: nothing to cancel; only `ublk_timeout` (blk-mq's
  30 s, the driver sets none) quiesces. Until then: `LIVE`, dead pid.
- `ublk_ctrl_start_recovery`: `EINVAL` without `USER_RECOVERY` or with
  `nr_queues_ready == 0`; `EBUSY` while `UB_STATE_OPEN` (cleared by
  `ublk_ch_release`) or not `QUIESCED`; then `ublk_queue_reinit`, `ub->mm =
  NULL`, `nr_queues_ready = 0`. New server: `ublk_ch_open` (exclusive), mmap,
  `FETCH_REQ` per tag (`ublk_mark_io_ready` pins `ubq_daemon` at the last).
- `ublk_ctrl_end_recovery`: waits for the fetches (`EINTR`), `EBUSY` unless
  `QUIESCED`, sets `ublksrv_pid`, unquiesces, kicks the requeue list, `LIVE`;
  same number. Killed after START before its last fetch: `nr_queues_ready ==
  0`, START `EINVAL` forever (reap it). Killed after the fetches, before END:
  stays `QUIESCED` with a ready queue; the next START works.
- `ublk_stop_dev`: `QUIESCED` -> unquiesce with `force_abort` (held I/O
  fails); `LIVE` -> quiesce first, waiting for started requests (a device
  still quiescing blocks it up to 30 s). `ublk_ctrl_del_dev` with wait blocks
  while any `/dev/ublkcN` is open; `DEL_DEV_ASYNC` does not. No owner check.
- No list command: the `ublk-char` class (`ublk_add_chdev`); `GET_DEV_INFO`
  needs `CAP_SYS_ADMIN`; `ublksrv_flags` copied at `ADD_DEV`, never read.

## 2. What was built

- `otap-sys`: `CtrlCommand::del_dev_async`, `UBLK_MAX_UBLKS`,
  `UBLK_CHAR_CLASS_DIR`, `parse_char_name`; `REQUIRED_UBLK_FEATURES` gains
  `USER_RECOVERY_REISSUE`. No new `unsafe`.
- `otap-ublk`: `spec` (DeviceSpec, host-independent), `host` (`UblkHost`,
  `HostDevice`, `DevInfo`, the stamp), `recovery` (`Recovery`, `Expect`,
  `classify`/`probe`/`reap_check`), `mock_kernel` (the driver in memory),
  `Transport for Box<T>`. Linux `device`: REISSUE + stamp at create (a
  driver not granting every required flag is refused), `UblkDevice::reattach`,
  END as the re-attached device's start, `LinuxHost` (sysfs list,
  `GET_DEV_INFO` + `GET_PARAMS`, reap = `STOP_DEV` + `DEL_DEV_ASYNC`, pid
  liveness from `/proc/<pid>/stat`, zombies dead).
- `otap-daemon`: devices host-independent (`VolumeDevice` over `Box<dyn
  HostDevice>`, `UblkBackend::{Kernel, Custom}`); `bindings`; `devices`;
  `pool/devices.rs`; `--state-dir`, `--recovery-timeout`. `otapctl device
  list`, `device reap <id>|--stale`.

## 3. The recovery (`otap_ublk::Recovery`, driven by `Pool::boot`)

1. Before the pool is opened: refused if a live process other than this one
   serves a device stamped with this pool (a second daemon on it).
2. After the boot commit, before any new device: list the host; record
   entries whose device is gone, or whose number now carries another stamp,
   are forgotten; a bound device of a volume not online becomes stale.
3. Each online volume with a device (its binding, else a stamped device)
   gets a `Recovery`: `GET_DEV_INFO` + checks (flag, stamp, 1 queue, size ==
   volume, max_io <= arena bound, not DEAD, no live server) -> `QUIESCED`:
   START (`EBUSY` retried >= 10 ms apart; the attempt at the budget's end is
   never held back) / `LIVE` with dead pid: wait -> re-attach (open, mmap,
   fetch all) -> front door into the engine -> END via `start_while_serving`.
   Recoveries are stepped in turn with engine ticks between rounds.
4. Gone -> forgotten, new device. Any other failure (incl. `TimedOut` after
   `--recovery-timeout`, default 5 s) -> recorded stale, new device. Nothing
   is deleted at boot. A success logs "serving N ms after the arena re-attach
   began" (flagged beyond 2 s).

## 4. REISSUE: on (decision for the lead)

Every request the front door serves is idempotent at the block level: a
re-executed write puts the same bytes at the same LBAs with a sequence above
every replayed one (arena map 5.0: `Placed` before `complete`, so an
acknowledged write replays from a re-attached arena; a reissued one is
re-placed and wins its range; a write it overtakes was concurrent with it,
so either order was allowed). Reads, flushes, discards, write-zeroes are
harmless twice. Without REISSUE a write durable in the arena whose commit
had not reached the kernel fails `EIO`, which aborts a filesystem. Step 5
devices (no REISSUE, no stamp) list as stale. On a heap-arena restart the
volumes are held (S5-1): reissued requests get `EIO`, delivered once.

## 5. Identity and the record

- Stamp (`ublksrv_flags`): `0x07A9 << 48 | crc32c(agg_id) << 16 | vol`.
- Record: `<state-dir>/ublk-<agg_id>.bindings`, default `/run/opentap`
  (tmpfs; device ids are host-local, never in the pool). Layout v1 (crate
  docs of `bindings`): 40-byte header (`OTAPBIND`, version, entry size 32,
  count <= 2048, agg id, writer pid), 32-byte entries by device id (dev, vol,
  size, bound/stale, reason code), CRC-32C. Verified before use; tmp + fsync
  + rename + dir fsync; empty removes it; unreadable -> logged, rebuilt from
  stamps. Written at create (before START), recovery, teardown, stale, reap.

## 6. Standings and the reaper (`classify`, `reap_check`)

serving: this daemon's. foreign: a live server not us (checked before any
record), another pool's stamp while a pool is booted, unstamped DEAD pid 0.
recoverable: stamped, server dead, and its pool not booted here (or its
online volume has no device). stale: recorded stale (this pool's stamp
only); this pool's stamp but volume gone/offline/served elsewhere; DEAD;
unstamped with no live server. `reap <id>`: stale or recoverable; `--stale`:
stale only; never serving/foreign/still quiescing (STOP would block the
engine thread). Works with no pool; `Unsupported` without ublk.

## 7. Protocol (version 1: additive variants, see the `PROTOCOL_VERSION` doc)

`DeviceList {}`, `DeviceReap { dev_id: Option<u32>, stale }` (exactly one,
else BadRequest), `Devices { devices }`, `Reaped { devices }`, `DeviceReport`
(state `DeviceState`, never `kind`). `DeviceList` is an empty struct variant
because serde lets an internally tagged unit variant ignore unknown fields.
Pre-existing gap for the lead: `Ping`, `PoolStatus`, `PoolSync`, `PoolClean`,
`PoolStop`, `DaemonStop`, `VolumeList` accept unknown fields (against R3).

## 8. Linux-only, never executed (type-checked, clippy- and rustdoc-clean)

`LinuxHost`, `UblkDevice::reattach`, END via `UblkControlRing::begin`, REISSUE
granted by `ADD_DEV`, STOP + `DEL_DEV_ASYNC` on a dying device. First WSL run:
kill -9 under fio -> same `/dev/ublkbN`, no fio error, time to serving vs
2 s; kill with every tag held -> `LIVE` ~30 s, the 5 s fallback, stale
listing, reap after the quiesce; length of the `EBUSY` window; kill between
START and END (gdb) then restart; a Step 5 leftover reaped; ext4 mounted
across a restart; `GET_PARAMS` on other software's devices; `ublks_max`.

## 9. Decisions to record (future E-entries)

REISSUE on and required; the stamp layout; the record (format, path, a hint
the stamps back up); `--recovery-timeout` 5 s default (1-300) falling back to
a new device with the old one stale (a server that held every tag needs
~30 s: raising it keeps the number at the cost of boot time); a boot refused
while a live other process serves this pool's device; the standings and reap
rules (nothing deleted silently); protocol 1 kept for additive variants,
empty-struct variants for field-less requests.

## 10. Tests (new files `step6b_recovery_*_tests.rs`)

- sys 5: `del_dev_async` bytes, recovery pids, flags/states, REISSUE required,
  char-class names. control 4: round trips, tags, unknown fields, framing.
- ublk 15: stamp; same number + held I/O delivered; vanished; not quiesced
  until the timeout, reissued; `EBUSY` until release and the retry interval;
  budget end; START/END/GET_DEV_INFO errors; kill between START and END; kill
  during fetches -> reaper; 2 s target and budget on `SimClock`; mismatches
  refused; standings; `reap_check`; probe + `DEL_DEV_ASYNC`; through the
  engine with a kept arena: reissued writes land once, none lost.
- daemon 15: stamp/record/offline/stop; restart keeps ids (< 2 s);
  queued requests answered once; killed between START and END, restarted;
  vanished; timeout -> stale -> reap; foreign/stale listing and refusals; no
  pool / no ublk; stamp without record; recorded volume gone; second daemon
  refused; reused number left alone; record codec; record file; dbladed flags.
- ctl 3: list table/JSON, reap and refusals, no pool / no ublk. Entries in
  `protocol/tests.rs`, `args.rs`; changed `required_set_is_the_v0_1_set`.
