# Map: the ublk path (otap-sys::ublk, otap-ublk, the 6.12 driver) for Step 6 scope (A)

Reader's map for the lead architect. Scope (A): `UBLK_F_USER_RECOVERY` re-attach after
`kill -9`, the stale device reaper, `otapctl device list | reap`. Every claim below cites a
file:line; where the kernel is the authority the citation is `ublk_drv.c:NN` (the saved
Linux 6.12 source) or `ublk_cmd.h:NN`. Where the code and the docs disagree it is said so.

---

## 1. Scope: what I read

| File | Lines | Notes |
|---|---|---|
| `E:/Coding/OpenTAP/crates/otap-sys/src/ublk.rs` | 1-1517, whole | UAPI constants, 9 `repr(C)` structs, `CtrlCommand`/`IoCommand` builders, Linux `UblkControl`, `UblkChar`, `UblkControlRing`, `UblkQueue` |
| `E:/Coding/OpenTAP/crates/otap-sys/src/lib.rs` | 1-131, whole | unsafe policy and the 13-block inventory |
| `E:/Coding/OpenTAP/crates/otap-sys/src/probe.rs` | 25-40, 134-210, 306-333 (grep) | `REQUIRED_UBLK_FEATURES`, `Allowlist` |
| `E:/Coding/OpenTAP/crates/otap-sys/src/block.rs` | 21-33 | `Full` |
| `E:/Coding/OpenTAP/crates/otap-sys/src/hugetlb.rs` | 60-135 | `HugeArena::map_file` (flock exclusivity), `anonymous`, `heap` |
| `E:/Coding/OpenTAP/crates/otap-ublk/src/lib.rs` | 1-54, whole | crate doc: "no recovery path yet" |
| `E:/Coding/OpenTAP/crates/otap-ublk/src/frontdoor.rs` | 1-532, whole | `Transport`, `QueueEvent`, `TransportError`, `UblkFrontDoor` |
| `E:/Coding/OpenTAP/crates/otap-ublk/src/mock.rs` | 1-225, whole | `MockTransport` |
| `E:/Coding/OpenTAP/crates/otap-ublk/src/device.rs` | 1-382, whole | `DeviceSpec`, `UblkDevice`, `LinuxTransport`, `front_door`, `serve` |
| `E:/Coding/OpenTAP/crates/otap-ublk/Cargo.toml`, `otap-sys/Cargo.toml` | whole | deps |
| `E:/Coding/OpenTAP/crates/otap-ublk/tests/engine_tests.rs` | 1-432, whole | |
| `E:/Coding/OpenTAP/crates/otap-ublk/tests/frontdoor_tests.rs` | 1-716, whole | |
| `E:/Coding/OpenTAP/crates/otap-ublk/tests/review_ublk_tests.rs` | 1-584, whole | |
| `E:/Coding/OpenTAP/crates/otap-ublk/tests/frontdoor_proptests.rs` | 1-482, whole | the model-based property test |
| `E:/Coding/OpenTAP/crates/otap-daemon/src/device.rs` | 1-56, whole | `create_device`, `destroy_device` |
| `E:/Coding/OpenTAP/crates/otap-daemon/src/frontdoor.rs` | 1-339, whole | `VolumeDevice`, `DaemonFrontDoor` |
| `E:/Coding/OpenTAP/crates/otap-daemon/src/pool.rs` | 129-150, 205-300, 330-362, 434-453, 520-560, 675-760, 836-863 | `Pool`, `boot`, `ensure_device`, `drop_device`, `stop`, `wait_until` |
| `E:/Coding/OpenTAP/crates/otap-daemon/src/daemon.rs` | 1-366, whole | the engine thread |
| `E:/Coding/OpenTAP/crates/otap-daemon/src/arena.rs` | 1-56, whole | `open_arena` |
| `E:/Coding/OpenTAP/crates/otap-daemon/src/bin/dbladed.rs` | grep of flags | `--no-ublk`, `--arena-file`, `--heap-arena` |
| `E:/Coding/OpenTAP/crates/otap-daemon/src/config.rs` | 64-84 | `DaemonConfig.ublk` |
| `E:/Coding/OpenTAP/crates/otap-control/src/protocol.rs` | 9, 42-210, 223-244, 357-440 | `Request`, `VolumeReport.device`, `Response`, `ErrorCode` |
| `E:/Coding/OpenTAP/crates/otap-ctl/src/args.rs` | 186-466 (grep + 436-466) | command groups |
| `E:/Coding/OpenTAP/crates/otap-ctl/src/{lib,render}.rs` | grep | `run`, `render` |
| `E:/Coding/OpenTAP/crates/otap-cp/src/traits.rs` | 45-80, 264-380, 438-452 | `FrontDoor`, `IoError`, `Status`, `ReqId`, `Request`, `RequestKind` |
| `E:/Coding/OpenTAP/crates/otap-cp/src/engine.rs` | 336-350, 1637-1643, 1988-2000 | `Event::Booted`, `Engine::boot` |
| `E:/Coding/OpenTAP/crates/otap-arena/src/lib.rs` | 690-800 | `Arena::create`, `Arena::attach` |
| `E:/Coding/OpenTAP/crates/otap-sim/src/harness.rs` | 1160-1200 | `crash_power`, `crash_daemon` |
| `E:/Coding/OpenTAP/crates/otap-sim/src/frontdoor.rs` | 117-127 | `SimFrontDoor::reset` |
| `E:/Coding/OpenTAP/crates/otap-sim/tests/step5_sim_tests.rs` | 1374-1398 | `proptest_cases` helper |
| `E:/Coding/OpenTAP/crates/otap-daemon/tests/daemon_tests.rs` | 40-75 | `DaemonConfig { ublk: false }` pattern |
| `E:/Coding/OpenTAP/docs/07-engineering-decisions.md` | 827-874 (E28), 1316-1427 (E38, esp. 1419-1427), 1498-1512 | |
| `E:/Coding/OpenTAP/docs/06-implementation-status.md` | 280-300, 430-445, 545-560 | the "no recovery yet" limit |
| `E:/Coding/OpenTAP/docs/10-phase-c-step-briefs.md` | 585-640 | the Step 6 brief, scope 1 |
| `E:/Coding/OpenTAP/docs/09-working-agreements.md` | 1-60, 95-140 | rulings, gate, etiquette |
| `E:/Coding/OpenTAP/scripts/linux-smoke.sh` | 112-182, 236-290 | `start_daemon`, `wait_device`, the SIGKILL section |
| `OpenTAP-local/kernel-6.12/ublk_drv.c` | 1-3063, whole | Linux 6.12 driver |
| `OpenTAP-local/kernel-6.12/ublk_cmd.h` | 1-406, whole | Linux 6.12 UAPI |

Not run: cargo, tests, builds (read-only assignment).

---

## 2. Public API and key internals

### 2.1 `otap_sys::ublk` (pure, every host) — `crates/otap-sys/src/ublk.rs`

Constants an implementer of (A) will name:

| Item | Value | Line |
|---|---|---|
| `UBLK_CMD_START_USER_RECOVERY` / `END_USER_RECOVERY` / `GET_DEV_INFO2` | `0x10` / `0x11` / `0x12` | 104 / 106 / 108 |
| `UBLK_U_CMD_GET_DEV_INFO` (`_IOR`) | `0x8020_7502` | 113 |
| `UBLK_U_CMD_DEL_DEV` (`_IOWR`) | `0xC020_7505` | 117 |
| `UBLK_U_CMD_STOP_DEV` | `0xC020_7507` | 121 |
| `UBLK_U_CMD_GET_PARAMS` (`_IOR`) | `0x8020_7509` | 125 |
| `UBLK_U_CMD_START_USER_RECOVERY` (`_IOWR`) | `0xC020_7510` | 127 |
| `UBLK_U_CMD_END_USER_RECOVERY` (`_IOWR`) | `0xC020_7511` | 129 |
| `UBLK_U_CMD_GET_DEV_INFO2` (`_IOR`) | `0x8020_7512` | 131 |
| `UBLK_U_CMD_GET_FEATURES` | `0x8020_7513` | 134 |
| `UBLK_U_CMD_DEL_DEV_ASYNC` (`_IOR`, no legacy nr) | `0x8020_7514` | 137 |
| `UBLK_IO_RES_OK` / `UBLK_IO_RES_ABORT` | `0` / `-19` (`-ENODEV`) | 163 / 170 |
| `UBLK_F_USER_RECOVERY` / `UBLK_F_USER_RECOVERY_REISSUE` | `1<<3` / `1<<4` | 283 / 286 |
| `UBLK_F_URING_CMD_COMP_IN_TASK` | `1<<1` (the driver ORs it in at ADD_DEV, so it comes back in `flags`) | 277 |
| `UBLK_S_DEV_DEAD` / `LIVE` / `QUIESCED` | `0` / `1` / `2` (u16) | 319 / 321 / 324 |
| `UBLK_MAX_QUEUE_DEPTH` / `UBLK_MAX_NR_QUEUES` | 4096 / 4096 | 181 / 204 |
| `feature_name(u64) -> Option<&'static str>` | names one bit | 303 |

Structures (all `#[repr(C)]`, zerocopy `FromBytes + IntoBytes + ...`, layout asserted at compile time L891-1027):

- `UblksrvCtrlCmd` L406-425: `dev_id: u32, queue_id: u16, len: u16, addr: u64, data: [u64;1], dev_path_len: u16, pad: u16, reserved: u32` (32 bytes). `NO_QUEUE = u16::MAX` L430; `ANY_DEV = u32::MAX` L434.
- `UblksrvCtrlDevInfo` L443-472 (64 bytes): `nr_hw_queues u16 @0, queue_depth u16 @2, state u16 @4, pad0, max_io_buf_bytes u32 @8, dev_id u32 @12, ublksrv_pid i32 @16, pad1, flags u64 @24, ublksrv_flags u64 @32, owner_uid u32 @40, owner_gid u32 @44, reserved1, reserved2`. Doc on `ublksrv_flags` L462: "For the server's own use; the driver does not interpret it."
- `UblksrvIoDesc` L481-494 (24 bytes): `op_flags u32, nr_sectors u32 (union nr_zones), start_sector u64, addr u64`; `op()` L500, `flags()` L510 (unshifted mask), `has_flag` L516.
- `UblksrvIoCmd` L541-553 (16 bytes): `q_id u16, tag u16, result i32, addr u64`.
- `UblkParamBasic` L568, `UblkParamDiscard` L594, `UblkParamDevt` L615 (read-only: char/disk major+minor, filled by GET_PARAMS), `UblkParamZoned` L631, `UblkParams` L653-670 (112 bytes with named `tail_pad`), `UblkParams::new()` sets `len = 112` L675.
- `CtrlBuf { addr: u64, len: u16 }` L695-700.

Command builders:

- `CtrlCommand { cmd_op: u32, header: UblksrvCtrlCmd }` L710-715; `CMD_LEN = 80` L720; `new(cmd_op, dev_id, Option<CtrlBuf>)` L724 (queue_id = NO_QUEUE, everything else zero).
- `add_dev(dev_id, info: CtrlBuf)` L748; `del_dev(dev_id)` L754; `start_dev(dev_id, ublksrv_pid: u32)` L762 (`data[0] = pid`); `stop_dev` L770; `set_params` L777; `get_params` L783; `get_dev_info(dev_id, info: CtrlBuf)` L789; `get_features(addr)` L796.
- **`start_user_recovery(dev_id)` L807** and **`end_user_recovery(dev_id, ublksrv_pid: u32)` L815** (`data[0] = pid`) **exist already**. There is **no** `get_dev_info2`, no `del_dev_async` builder, no `queue_affinity`; `CtrlCommand::new(UBLK_U_CMD_DEL_DEV_ASYNC, dev_id, None)` builds the async delete without a new builder.
- `cmd_bytes()` L823: the 32-byte header then zeros.
- `IoCommand { cmd_op, cmd: UblksrvIoCmd }` L835-840; `fetch(q_id, tag)` L850 (doc: "issued once per tag when a queue starts (and again after `START_USER_RECOVERY`)"); `commit_and_fetch(q_id, tag, result)` L866; both keep `addr = 0`.
- `user_copy_offset(q_id, tag, buf_off)` L244 = `0x8000_0000 + (q_id << 41) + (tag << 25) + buf_off`; `checked_` variant L223 (`None` when `q_id >= 4096` or `buf_off >= 32 MiB`); `split_user_copy_offset` L256.

### 2.2 `otap_sys::ublk` (Linux only, `mod linux` L1033-1514, re-exported L1517)

- `UblkControl` L1050: `PATH = "/dev/ublk-control"` L1056; `open() -> io::Result<Self>` L1063 (`O_RDWR|O_CLOEXEC`; `ENOENT` = module not loaded); `from_fd(OwnedFd)` L1072; `command_sqe(&CtrlCommand) -> squeue::Entry128` L1082 (`UringCmd80`, `user_data` 0). `AsFd`/`AsRawFd`.
- `UblkChar` L1110: `open(dev_id) -> io::Result<Self>` L1120 opens `/dev/ublkc{dev_id}` `O_RDWR|O_NONBLOCK|O_CLOEXEC` (C03: `O_NONBLOCK` is a requirement, L1104-1108); `from_fd(OwnedFd, dev_id)` L1133 sets `O_NONBLOCK` if missing; `dev_id()` L1143; `command_sqe(&IoCommand) -> squeue::Entry` L1152 (`UringCmd16`).
- `UblkControlRing` L1184: private 2-entry SQE128 ring over one `UblkControl`; `new(control)` L1194; `control()` L1201; **`execute(&mut self, cmd: &CtrlCommand, buf: Option<&mut [u8]>) -> io::Result<i32>` L1213-1276**: checks `buf` is exactly the address/length the header names (else `InvalidInput`, L1214-1233); `user_data = cmd_op` L1237; one `unsafe` push L1244 (inventory #11); `submit_and_wait(1)` loop retrying `EINTR` L1248-1270; a negative CQE result becomes `Err(io::Error::from_raw_os_error(-result))` L1272-1273, so **`-EBUSY` arrives as `Err(e)` with `e.raw_os_error() == Some(16)`** (kind `ResourceBusy`), `-ENODEV` as `Some(19)`, `-EINVAL` `Some(22)`, `-EEXIST` `Some(17)`, `-EACCES` `Some(13)`, `-EPERM` `Some(1)`, `-EINTR` `Some(4)` (not retried: the kernel's own `-EINTR` from `wait_for_completion_interruptible` is a CQE result, not an `io_uring_enter` error).
- `UBLK_QUEUE_CMD_BUF_STRIDE = 24 * 4096` L1290.
- `UblkQueue` L1299-1306 `{ chr, ring: IoUring<Entry, Entry>, descs: Mmap, q_id, depth, pushed }`. "Every command of the queue must be issued from the thread that issued its first fetch (the driver pins the queue to that task)" L1296-1298.
  - `new(chr, q_id, depth, entries) -> io::Result<Self>` L1316-1346: `depth` 1..=4096 else `InvalidInput`; read-only `mmap` of `round_up(depth*24, 4096)` bytes at offset `q_id * STRIDE` (inventory #12); ring of `max(entries, depth)` entries.
  - `chr()` L1350, `q_id()` L1356, `depth()` L1362, `desc(tag) -> Option<UblksrvIoDesc>` L1369 (None beyond depth).
  - `push_fetch(tag) -> Result<(), Full>` L1382; `push_commit(tag, result) -> Result<(), Full>` L1391; both via `push_command` L1395-1408 (`user_data = tag`, inventory #13, `Full` when the SQ has no room, `pushed += 1`).
  - `submit() -> io::Result<usize>` L1414 (EINTR retried); `submit_and_wait()` L1428 (waits for ≥1 CQE).
  - `reap(&mut self, out: &mut Vec<(u16, i32)>) -> usize` L1441-1451: `cq.sync()`, every CQE as `(tag from user_data, result)`.
  - `read_data(tag, off, dst) -> io::Result<()>` L1459-1478: `pread` loop at the user-copy position; `UnexpectedEof` on a zero read. `write_data(tag, off, src)` L1485-1501: `pwrite` loop; `WriteZero` on a zero write. Both `InvalidInput` when the position is out of range.

### 2.3 `otap_sys` unsafe policy — `crates/otap-sys/src/lib.rs`

- L41-50: every `unsafe` block minimal, opens with `// SAFETY:` (`clippy::undocumented_unsafe_blocks` denied workspace-wide, `Cargo.toml` `[workspace.lints.clippy]`); no `unsafe fn` exported or internal (`#![deny(unsafe_op_in_unsafe_fn)]` L108); safety rests on ownership; pushing an SQE is the unsafe step and its proof covers every address the entry names until its CQE is reaped.
- L52-88: the inventory is enumerated by number ("Thirteen blocks, all Linux-only"). **Adding a block means renumbering/extending this list** (#11 `UblkControlRing::execute`, #12 `UblkQueue::new`, #13 `UblkQueue::push_command`). A recovery path needs **no new unsafe block**: START/END/GET_DEV_INFO/DEL go through `execute`, fetches through `push_fetch`.
- `otap-ublk` is `#![forbid(unsafe_code)]` (`otap-ublk/src/lib.rs:41`); `otap-daemon` likewise (E38 says the daemon holds no durable state; no unsafe).
- `probe.rs:37-38`: `REQUIRED_UBLK_FEATURES = UBLK_F_USER_COPY | UBLK_F_USER_RECOVERY | UBLK_F_CMD_IOCTL_ENCODE` — **REISSUE is not required**, and today not requested either (see 2.5).

### 2.4 `otap_ublk::frontdoor` — `crates/otap-ublk/src/frontdoor.rs`

- errnos L13-22: `EIO=5, ENODEV=19, EINVAL=22, ENOSPC=28, EOPNOTSUPP=95`.
- `QueueEvent` L29-42: `Request { tag: u16, desc: UblksrvIoDesc }` | `Abort { tag }` ("the driver aborted `tag`: it must not be fetched again").
- `TransportError { what: &'static str, errno: i32 }` L46-51, `Display` "`{what} (errno {errno})`".
- **`trait Transport`** L62-76:
  - `fn poll(&mut self, out: &mut Vec<QueueEvent>) -> Result<usize, TransportError>` — moves every arrived event into `out`, returns how many.
  - `fn read_data(&mut self, tag: u16, off: u32, dst: &mut [u8]) -> Result<(), IoError>` — copies `dst.len()` bytes of a write's data out of the driver.
  - `fn write_data(&mut self, tag: u16, off: u32, src: &[u8]) -> Result<(), IoError>` — copies into a read's data.
  - `fn commit(&mut self, tag: u16, result: i32) -> Result<(), TransportError>` — completes `tag` with `result` and fetches its next request.
- `Watermarks { high, low }` L80-85; `for_depth(depth)` L90 → `high = max(depth,1)`, `low = depth/2`.
- `FrontDoorStats` L101-128: `requests, reads, writes, flushes, discards, write_zeroes, refused, offline_rejected, stale, aborted, errors, gate_closed, commit_failures`.
- `enum TagState { Idle, Ready, InEngine, Aborted }` L131-140 (private); `struct TagSlot { state, kind: Option<RequestKind>, gen: u32 }` L143-147 (private).
- `decode_request(&UblksrvIoDesc, vol, size_blocks) -> Result<RequestKind, i32>` L153-187.
- `errno_for(Status, &RequestKind) -> i32` L192-206 (`Ok` → bytes or 0; `NvFailed|Corrupt|Failed` → `-EIO`; `NoSuchVolume|Stopped|Offline` → `-ENODEV`; `NoSpace` → `-ENOSPC`; `OutOfRange|TooLarge` → `-EINVAL`; anything else `-EIO`).
- **`request_id(q_id, tag, gen) -> ReqId`** L213 = `gen << 32 | q_id << 16 | tag`; `split_id(ReqId) -> (q_id, tag, gen)` L219.
- `UblkFrontDoor<T: Transport>` L224-238: `transport, q_id, vol, size_blocks, tags: Vec<TagSlot>, ready: VecDeque<u16>, events, in_engine, gate_closed, offline, watermarks, stats, last_error`.
  - `new(transport, q_id, vol, size_blocks, depth, watermarks)` L247-282 (high clamped to ≥1, low ≤ high; every tag `Idle`, gen 0).
  - `set_offline(bool)` L288-302 (going offline answers every `Ready` tag `-ENODEV` at once; `InEngine` finish normally); `is_offline` L306; `in_engine()` L312; `ready()` L318; `stats()` L324; `watermarks()` L330; `transport(&mut self) -> &mut T` L335; `last_error()` L341; `aborted_tags() -> Vec<u16>` L347-354.
  - `pump() -> Result<usize, TransportError>` L359-423 (see §4.3).
  - `answer(tag, result)` L426-434 (private): `transport.commit`; on `Err` → `commit_failures += 1`, `last_error`, **tag state := Aborted** ("a refused commit retires the tag", E28).
- `impl FrontDoor` L437-518: `fetch` L438-469, `copy_in` L471-486, `copy_out` L488-497, `complete` L499-518 (see §4.3).
- `Debug` L521-532 (`q_id, vol, depth, ready, in_engine, offline`).

### 2.5 `otap_ublk::device` (Linux only) — `crates/otap-ublk/src/device.rs`

- `DeviceSpec { size_blocks: u64, depth: u16, max_io_bytes: u32, io_opt_bytes: u32, max_discard_bytes: Option<u32>, rotational: bool }` L22-38; `new(size_blocks, stripe_bytes, rotational)` L44 (depth 128, 8 MiB, discard 1 GiB); `for_arena(arena_slots)` L60 caps `max_io_bytes` at half the arena.
- `UblkDevice { control: UblkControlRing, dev_id: u32, info: UblksrvCtrlDevInfo, queue: Option<UblkQueue>, started: bool }` L70-76. **No `Drop` impl**: a dropped `UblkDevice` leaves the kernel device alive; only `delete()` removes it.
  - `create(&DeviceSpec) -> io::Result<Self>` L88-125: `ADD_DEV` with `flags = USER_COPY | USER_RECOVERY | CMD_IOCTL_ENCODE` L95 (**`ublksrv_flags` left 0**, `..default()` L96), `dev_id = ANY_DEV`; reads back the 64-byte info L107; then `set_params` L127-164 and `open_queue` L166-177; on failure `delete_inner`.
  - `open_queue` L166-177: `UblkChar::open(dev_id)`, `UblkQueue::new(chr, 0, depth, depth*2)`, `push_fetch` for every tag, `submit()`.
  - `start()` L184-190: `START_DEV(dev_id, std::process::id())`, `started = true`. `stop()` L196-201. `delete_inner` L203-208: **drops the queue first** (closes `/dev/ublkcN`, clears `UB_STATE_OPEN`), then `DEL_DEV`. `delete(self)` L214-219: `stop()` if started, then `delete_inner`.
  - `dev_id()` L223; `info()` L229 (the info as `ADD_DEV` returned it, state 0 at that time); `block_path()` L235 = `/dev/ublkb{N}`; `take_transport() -> Option<LinuxTransport>` L242-247 (once; doc: "the runner's thread must be the one that created the device").
- `LinuxTransport { queue: UblkQueue, aborted: Vec<u16> }` L261-264; `queue()` L269; `wait()` L277 = `submit_and_wait`.
  - `poll` L283-306: `queue.submit()` (error → `TransportError{what:"submit"}`), `reap`, `RES_OK` + `desc(tag)` → `Request`; **any other result (not only `RES_ABORT`) → `Abort { tag }`** L297-303. So a fetch answered `-EBUSY`/`-EINVAL` by the driver silently retires the tag.
  - `read_data`/`write_data` L308-322 map errors to `IoError::Device { errno }`.
  - `commit` L324-340: `push_commit`; on `Full` submit once and retry; second `Full` → `TransportError { "queue ring full", 11 }`.
- `front_door(&mut UblkDevice, vol, &DeviceSpec) -> io::Result<UblkFrontDoor<LinuxTransport>>` L344-360 (q_id 0, `Watermarks::for_depth(spec.depth)`).
- `serve(engine, stop: &AtomicBool) -> Result<(), EngineError>` L368-382: tick loop with a 200 µs sleep when idle. The daemon does not call it (it has its own loop, `daemon.rs:278-318`).

### 2.6 `otap_ublk::mock` — `crates/otap-ublk/src/mock.rs`

`MockTransport` L17-37: `depth, events: VecDeque<QueueEvent>, payloads: HashMap<u16, Vec<u8>>, read_back, busy: HashSet<u16>, aborted: HashSet<u16>, commits: Vec<(u16,i32)>, fail_copies, fail_commits, fail_poll, polls`.

- `new(depth)` L42. `deliver` L58-66 **panics** on a tag ≥ depth, an aborted tag, or a busy tag (test misuse). `submit_write(tag, lba, data)` L69 (blocks = len/4096), `submit_read` L76, `submit_flush` L81, `submit_discard` L86, `submit_write_zeroes` L91, `submit_raw(tag, desc)` L97.
- `abort(tag)` L104-111: marks aborted, un-busies, drops the payload, withdraws an undelivered `Request` event, queues `Abort`.
- `inject(QueueEvent)` L116: no bookkeeping (driver misbehaviour).
- `take_commits()` L121, `take_commit(tag)` L126, `read_payload(tag)` L132, `outstanding()` L138, `fail_copies(bool)` L143, `fail_commits(bool)` L148, `fail_next_poll()` L153, `polls()` L159.
- `Transport`: `poll` L165-177 (one-shot failure errno 5); `read_data` L179-190 (`Corrupt` when no payload, `BadLength` on overrun); `write_data` L192-206 (`Corrupt` if not busy); `commit` L208-224 (`fail_commits` → errno 19 "commit refused"; not busy → errno 22 "commit of an idle tag").

What it does **not** model (needed for (A), see §5.4): a device object outliving the transport, `UB_STATE_OPEN`, device states, a pid, `ublksrv_flags`, the control commands, the daemon's death, requeue/re-dispatch, the block layer's view of a request's final result.

### 2.7 The daemon side (where (A) plugs in)

- `otap-daemon/src/device.rs`: `create_device(vol, size_blocks, stripe_bytes, rotational, arena_slots) -> Result<(VolumeDevice, String), ControlError>` L12-45 ("on the calling thread (the one that serves it)"): `DeviceSpec::new(..).for_arena(..)`, `UblkDevice::create`, `front_door`, `device.start()`, `block_path`. `destroy_device(VolumeDevice)` L50-56: drops the front door (and with it the queue/char fd), then `device.delete()`.
- `otap-daemon/src/frontdoor.rs`: `VolumeDevice { device: UblkDevice, front: UblkFrontDoor<LinuxTransport> }` L178-183 (Linux); `DaemonFrontDoor` L186-195 (`channel`, `devices: BTreeMap<u32, VolumeDevice>`, `turn`); `add_device` L254, `remove_device` L260, `device` L266, `device_mut` L272; `fetch` round-robins the devices then the channel L278-296; `copy_in/copy_out/complete` route by `req.vol()` L298-329. `CHANNEL_ID_BIT = 1<<63` L63: channel ids are told from ublk ids by bit 63 (a tag's generation reaches it after 2^31 completions).
- `otap-daemon/src/pool.rs`: `Pool` L129-150 (`ublk: bool` L140, `devices: BTreeMap<u32, String>` L141 = vol → `/dev/ublkbN`); `boot` L209-295: superblock probe, `open_backstore`, `open_arena` L241, `PoolEngine::boot(cfg, backstore, DaemonFrontDoor::new(), clock, memory)` L245-251, `wait_until("the boot commit", is_serving)` L280, then **`ensure_device(vol)` for every online volume** L281-293 (an error stops the pool and fails the boot). `ensure_device` L678-709 (skips when `!ublk`, when a device exists, when the volume is absent/offline; else `create_device`, `add_device`, logs "volume {vol} is {path}"). `drop_device` L714-739 (`set_offline(true)`, `wait_until(in_engine == 0)`, `remove_device`, `destroy_device`). `stop` L840-863 drops every device first L842-845. `wait_until` L523-556 (ticks until `done`, `op_timeout`, interrupted by the stop flag). Other `ensure/drop` sites: L876 (create), L902/907 (online/offline), L930-931 (resize), L1088/1098/1111 (rollback, nvfail clear).
- `otap-daemon/src/daemon.rs`: the pool is booted **on the engine thread** `"dbladed-engine"` (`spawn` L232-275: `boot_configured()` L247 on the thread, then `run` L278-318). So every device is created, fetched and served from that one thread, which satisfies the driver's task pinning (§3). `DaemonStop`/signal → `shutdown` L217-225 → `pool.stop()`.
- `otap-daemon/src/arena.rs` `open_arena` L10-56: `HugeArena::map_file` for `ArenaKind::Hugetlb{path}` (re-attached with contents; `WouldBlock` if another process holds the file's `flock`, `hugetlb.rs:87-95`), `HugeArena::heap` for `--heap-arena`.
- `otap-cp` `Event::Booted { cp_seq, dirty, reattached, replayed_blocks, rewound, arena_lost, damaged_slots, orphaned_blocks, nvfail, torn }` `engine.rs:336-350`; the pool notes "arena re-attached (N blocks replayed)" `pool.rs:353-357`. **`reattached == true` is the daemon's own signal that this boot follows a crash with the arena intact**; no ublk fact is consulted today.
- `otap-control` `Request` `protocol.rs:42-210` (`op`-tagged JSON, `deny_unknown_fields`); `VolumeReport.device: Option<String>` L240-241; `Response` L357-405; `ErrorCode { NoPool, PoolBooted, NotFound, BadRequest, Engine, Timeout, Io, Unsupported, Protocol }` L410-429; `PROTOCOL_VERSION = 1` L9.
- `otap-ctl/src/args.rs`: verbs matched as `(group, verb)` L244-442; an unknown verb of a known group L442-444, an unknown group L445 ("unknown command group"). `otapctl` has no privileged path of its own: every command is a `Request` to the daemon.
- `DaemonConfig.ublk: bool` (`config.rs:77-78`, `--no-ublk` `dbladed.rs:109`); the daemon tests run with `ublk: false` (`daemon_tests.rs:47-54`).

### 2.8 The kernel (6.12 `ublk_drv.c`) — items userspace depends on

- Flags the driver accepts: `UBLK_F_ALL` L55-63 = ZERO_COPY | COMP_IN_TASK | NEED_GET_DATA | USER_RECOVERY | USER_RECOVERY_REISSUE | UNPRIVILEGED_DEV | CMD_IOCTL_ENCODE | USER_COPY | ZONED. **There is no `UBLK_F_USER_RECOVERY_FAIL_IO` in 6.12** (it is 6.13+): a quiesced device **hangs** I/O rather than failing it.
- Per-io flags L90-120: `ACTIVE 0x01` (fetch/commit pending in the driver), `OWNED_BY_SRV 0x02` (delivered to the server, not committed), `ABORTED 0x04`, `NEED_GET_DATA 0x08`, `CANCELED 0x80000000`.
- `struct ublk_queue` L131-150: `ubq_daemon` (the task pinned at the queue's last fetch), `force_abort`, `timeout`, `canceling`, `nr_io_ready`. `struct ublk_device` L152-184: `dev_info`, `state` bits `UB_STATE_OPEN=0, UB_STATE_USED=1, UB_STATE_DELETED=2` L165-167, `mm` (the one process allowed to mmap, L174), `completion`, `nr_queues_ready`, `quiesce_work`, `stop_work`.
- Enumeration: `ublk_index_idr` L481 keyed by dev id; `UBLK_MAX_UBLKS = 1<<20` L49/L493; **`ublks_max` module parameter, default 64** L494, L3058 (`/sys/module/ublk_drv/parameters/ublks_max`); `ublks_added` L495. `ADD_DEV` with `ublks_added >= ublks_max` → `-EACCES` L2417-2419. A specific `dev_id` in ADD_DEV → `-EEXIST` if taken L2070-2073. There is **no "list devices" command**: enumerate by `GET_DEV_INFO` over ids `0..ublks_max` (`-ENODEV` for an unused id, L2921-2925) or by listing the char class `/sys/class/ublk-char/ublkcN` (class name L477-479, device name `"ublkc%d"` L2117). `/dev/ublkcN` exists from ADD_DEV to DEL_DEV; `/dev/ublkbN` from START_DEV (`add_disk` L2281) to STOP_DEV/DEL (`del_gendisk` L1605).
- Permission L2821-2842: a device created without `UNPRIVILEGED_DEV` (ours) accepts control commands only with `CAP_SYS_ADMIN` (`-EPERM`), and **`GET_DEV_INFO2` always demands a `dev_path` in the buffer with `dev_path_len`** (L2840-2851) even for root; plain `GET_DEV_INFO` (0x02) needs only the capability. Use `GET_DEV_INFO`.
- Control dispatch L2896-2980: needs `IO_URING_F_SQE128` L2909 (else `-EINVAL`); `IO_URING_F_NONBLOCK` → `-EAGAIN` L2904 (the private ring never sets it); `GET_FEATURES` needs no device L2916; unknown op → `-ENOTSUPP` L2968.

---

## 3. Invariants and rules in force

Kernel rules (authoritative, 6.12):

- K1. **One open of `/dev/ublkcN` at a time**: `ublk_ch_open` `test_and_set_bit(UB_STATE_OPEN)` → `-EBUSY` on a second open (L1301-1310); cleared in `ublk_ch_release` when the last file reference drops (L1312-1318).
- K2. **One mm may mmap the descriptor buffers**: the first `mmap` pins `ub->mm`; another mm → `-EINVAL` (L1329-1337); `START_USER_RECOVERY` resets `ub->mm = NULL` (L2728). The mapping must be read-only (`VM_WRITE` → `-EPERM`, L1339) and exactly the page-rounded queue array (L1351).
- K3. **A queue is pinned to the task that issued its last `FETCH_REQ`** (`ublk_mark_io_ready` L1621-1636: `ubq->ubq_daemon = current` when `nr_io_ready == q_depth`); every later command from another task is refused `-EINVAL` (L1706-1707). `ublk_queue_reinit` clears it (L2680).
- K4. **`FETCH_REQ` only before the queue is ready** (`-EBUSY` once `nr_io_ready == q_depth`, L1736-1739) and never on a tag `OWNED_BY_SRV` (`-EINVAL`, L1744-1745); any command on a tag with `ACTIVE` set → `-EBUSY` (L1715-1718). Under `USER_COPY`, `addr` must be 0 (`-EINVAL`, L1754-1758, L1777-1784).
- K5. **`START_DEV` and `END_USER_RECOVERY` block until every queue has every tag fetched** (`wait_for_completion_interruptible(&ub->completion)` L2244, L2748; `-EINTR` on a signal). `START_DEV` requires `pid > 0` (L2208) and refuses a LIVE/USED device with `-EEXIST` (L2248-2251).
- K6. **`START_USER_RECOVERY`** (L2694-2736): `-EINVAL` without `USER_RECOVERY` or when `nr_queues_ready == 0` (no server ever completed its fetches); **`-EBUSY` while `UB_STATE_OPEN` is set or `state != QUIESCED`** (L2719-2723). On success: every queue reinit (`nr_io_ready = 0`, `ubq_daemon = NULL`, `timeout = canceling = false`, every io `flags = 0, cmd = NULL, addr = 0`, L2669-2692), `mm = NULL`, `nr_queues_ready = 0`, `nr_privileged_daemon = 0`, completion re-armed (L2728-2731).
- K7. **`END_USER_RECOVERY`** (L2738-2774): waits for all fetches first (K5), then `-EINVAL` without the flag, **`-EBUSY` if `state != QUIESCED`** (a STOP_DEV raced in), else `ublksrv_pid = data[0]`, `blk_mq_unquiesce_queue`, `blk_mq_kick_requeue_list`, `state = LIVE` (L2762-2769).
- K8. **Quiesce is only reached from the dying-daemon path**: `ublk_uring_cmd_cancel_fn` (io_uring cancelling the dead task's pending fetch/commit cmds, L1464-1497) or `ublk_timeout` when the daemon is `PF_EXITING` and every tag is non-ACTIVE (L1224-1245, i.e. nothing cancellable remained because every tag was owned by the server) → `ublk_abort_requests` (L1410-1440: `canceling = true`, quiesce the blk-mq queue, `ublk_abort_queue`, unquiesce) → `schedule_work(quiesce_work)` when `USER_RECOVERY`, else `stop_work`. `ublk_quiesce_work_fn` (L1558-1570): only if `LIVE`: `blk_mq_quiesce_queue`, `ublk_wait_tagset_rqs_idle` (polls every 3 ms until no started request remains, L1532-1545), `state = QUIESCED`; then `ublk_cancel_dev` completes every remaining ACTIVE cmd with `UBLK_IO_RES_ABORT` (L1442-1458, L1504-1519).
- K9. **What happens to requests at the daemon's death**:
  - delivered to the server and not committed (`OWNED_BY_SRV`): `ublk_abort_queue` (L1387-1408) sets `ABORTED` and `__ublk_fail_req` (L1061-1070): **with `REISSUE` → `blk_mq_requeue_request` (kept, re-dispatched to the new daemon); without → `ublk_put_req_ref` → `__ublk_complete_rq` → `blk_mq_end_request(BLK_STS_IOERR)` (the application gets EIO)**.
  - dispatched by blk-mq but never delivered (task_work ran after the task started exiting, or arrived while `canceling`): `__ublk_abort_rq` (L1090-1098) → with `USER_RECOVERY` → `blk_mq_requeue_request` (kept) regardless of `REISSUE` (L1121-1124, L1274-1277).
  - not yet dispatched: held by the quiesced blk-mq queue until `END` (or failed by `force_abort` at `STOP_DEV`, L1271-1272, L1585-1586).
  - the dead server's pending fetches: completed with `RES_ABORT` (nobody reads them).
- K10. **`STOP_DEV` on a QUIESCED device works from any root process** (`ublk_stop_dev` L1593-1618: state not DEAD; recovery device: `__ublk_quiesce_dev` only if LIVE, then `ublk_unquiesce_dev` sets `force_abort` on every queue and unquiesces so requeued/new requests fail with IOERR, then `del_gendisk`, `state = DEAD`, `ublksrv_pid = -1`, `ub_disk = NULL`, `ublk_cancel_dev`). `DEL_DEV` (L2512-2549) calls `ublk_remove` (L2162-2170: `ublk_stop_dev`, cancel both works, `cdev_device_del`, put) and, with `wait`, **blocks (interruptibly) until the idr entry is freed**, which needs the last reference to the cdev dropped — an open `/dev/ublkcN` fd anywhere holds one. `DEL_DEV_ASYNC` returns without waiting (L2949-2951).
- K11. `SET_PARAMS` is refused only while `LIVE` (`-EACCES`, L2653-2654); `GET_PARAMS` works in any state and fills `devt` (L2585-2598, L2621). `GET_DEV_INFO` copies the whole 64-byte `dev_info` as it stands (L2569-2582).
- K12. The data copies (`pread`/`pwrite`) are refused `-EACCES` when `state == DEAD` (L1912-1913) and `-EINVAL` for a tag whose request is not started/has no data (L1808-1836).
- K13. `ADD_DEV` ORs in `UBLK_F_CMD_IOCTL_ENCODE | UBLK_F_URING_CMD_COMP_IN_TASK` (L2447-2448), clamps `nr_hw_queues` to `nr_cpu_ids` (L2464), rounds `max_io_buf_bytes` down to a page (L2142-2148), stores `owner_uid/gid` (L2397) and **copies `ublksrv_flags` through untouched** (memcpy of the whole info L2434; nothing else reads it). It is write-once: no command updates it later.

Project rules:

- E28 (`docs/07:827-874`): transport vs state machine split; tag states `Idle/Ready/InEngine/Aborted`; request id `gen << 32 | queue << 16 | tag`, generation advances at every completion or abort; refused commit retires the tag; `MockTransport` drives everything on any host; the Linux module is type-checked only; "a queue's commands must come from the task that issued its first fetch" (the code comment says "first", the kernel pins at the **last** fetch of the queue — same thread in practice because `open_queue` pushes all fetches from one thread).
- E38 "Not done" (`docs/07:1419-1427`) and `docs/06:550-556`: the device is created with the flag; a killed daemon's devices stay quiesced until a tool deletes them or the module is reloaded; the restarted daemon creates new ones so `/dev/ublkbN` numbers change. The recovery re-attach and the reaper are Step 6.
- S5-4 (`docs/07:1084-1102`, E38 "Names" L1405-1409): the daemon holds no durable state beyond its command line; names live in the pool's catalogue (F16). A device→volume mapping therefore cannot live in a daemon file by that rule; the kernel's `ublksrv_flags` is the natural place (§5.2, §8).
- S5-1 (`docs/07:1037`, `hugetlb.rs:119-124`): a heap arena dies with the process; a crash on it is a dirty boot with nvfail. Recovery over `USER_RECOVERY` is orthogonal to the arena kind: the device can be re-attached either way, but with a heap arena the volume sits under nvfail and every request answers `EIO` until cleared (`engine_tests.rs:287-334`).
- D2 (`otap-ublk/src/lib.rs:31-34`): a write is acknowledged when placed in the arena; `UBLK_IO_F_FUA` is dropped.
- Working agreements (`docs/09`): rulings go in `07` (an S-rule for a wire/protocol decision, L238; E-entry for a departure, L154); no placeholders; one cargo at a time, `-j 2` for a worker; `PROPTEST_CASES` overrides any explicit `cases` (L134-136).
- The code and the docs agree everywhere I checked, with one wording nit: E28 says "the task that issued its first fetch", the kernel pins at the queue's last fetch (`ublk_mark_io_ready` L1625-1627).

---

## 4. How things happen, in order

### 4.1 Device creation and start today (Linux, the engine thread)

1. `daemon.rs:247` `boot_configured` → `Pool::boot` (`pool.rs:209`) on `"dbladed-engine"`.
2. `pool.rs:241` `open_arena` → `HugeArena::map_file` (exclusive `flock`) or `heap`.
3. `pool.rs:245-251` `Engine::boot(cfg, backstore, DaemonFrontDoor::new(), clock, memory)`: the arena is `Arena::attach`ed (`otap-arena/src/lib.rs:766`) when its header verifies, else created; `Event::Booted { reattached, .. }` queued (`engine.rs:1988-1999`).
4. `pool.rs:280` ticks until `is_serving()`; `pool.rs:288-293` `ensure_device(vol)` for every online volume.
5. `pool.rs:687` → `device.rs:12` `create_device`: `DeviceSpec::new(..).for_arena(arena_slots)`.
6. `ublk/device.rs:88` `UblkDevice::create`: `UblkControl::open` + `UblkControlRing::new` (L89); `ADD_DEV(ANY_DEV, info{nr_hw_queues 1, queue_depth, max_io_buf_bytes, flags USER_COPY|USER_RECOVERY|CMD_IOCTL_ENCODE})` (L103-106); kernel `ublk_ctrl_add_dev` (L2351-2499) allocates the id, creates `/dev/ublkcN`, writes the info back; `dev_id = info.dev_id` (L109).
7. `set_params` (L127-164): `SET_PARAMS` basic (4 KiB shifts, `VOLATILE_CACHE|FUA`, max_sectors, dev_sectors) + discard.
8. `open_queue` (L166-177): `UblkChar::open(dev_id)` (sets `UB_STATE_OPEN`), `UblkQueue::new` (mmap pins `ub->mm`), `push_fetch(tag)` for `0..depth`, `submit()` → each fetch is `-EIOCBQUEUED` in the driver with `ACTIVE` set; the last one sets `ubq_daemon = current` and completes `ub->completion` (L1760-1761, L1621-1636).
9. `device.rs:26` `front_door` → `take_transport` → `UblkFrontDoor::new(transport, 0, vol, size_blocks, depth, for_depth(depth))`.
10. `device.rs:36` `device.start()` → `START_DEV(dev_id, pid)` (L184-190): the kernel waits on the completion (already done), allocates the disk, `ublksrv_pid = pid`, `state = LIVE`, `add_disk` → `/dev/ublkbN` (L2244-2285).
11. `pool.rs:694-696` `add_device(vol, VolumeDevice)`, `devices[vol] = "/dev/ublkbN"`.

### 4.2 Serving one request (Linux)

1. blk-mq → `ublk_queue_rq` (L1250-1283): `ublk_setup_iod` writes the tag's descriptor (L937-977), `blk_mq_start_request`, `ublk_queue_cmd` → task_work on `ubq_daemon`.
2. In the daemon task: `__ublk_rq_task_work` (L1100-1177) → `ublk_init_req_ref`, `ubq_complete_io_cmd(io, RES_OK)`: clears `ACTIVE`, sets `OWNED_BY_SRV`, completes the fetch CQE with `res = 0`.
3. `DaemonFrontDoor::fetch` (`daemon/frontdoor.rs:278`) → `UblkFrontDoor::fetch` → `pump` → `LinuxTransport::poll` (`submit`, `reap`, `desc(tag)`) → `QueueEvent::Request`.
4. Engine `copy_in` → `read_data` (`pread` at the user-copy offset; kernel `ublk_ch_read_iter` L1946-1962 takes a request ref) / `copy_out` → `write_data`.
5. `complete` → `answer` → `LinuxTransport::commit` → `push_commit` (`COMMIT_AND_FETCH_REQ`) → next `poll`'s `submit` → kernel `__ublk_ch_uring_cmd` COMMIT: requires `OWNED_BY_SRV` (L1766), `ublk_fill_io_cmd` (ACTIVE again), `ublk_commit_completion` (L1358-1380: clears OWNED, `io->res = result`, `ublk_put_req_ref` → `__ublk_complete_rq` L991-1042: a zero-byte READ is `-EIO`, a short count requeues the rest).

### 4.3 The front door state machine (`frontdoor.rs`)

`pump` L359-423: `transport.poll(&mut events)`; on `Err` → `last_error`, return `Err`. For each event:
- `Abort { tag }` L373-388: `Ready` → removed from `ready`; `InEngine` → `gen += 1`, `in_engine -= 1` (its completion will be stale); state := `Aborted`, `kind = None`, `aborted += 1`. (Idle → just Aborted.)
- `Request { tag, desc }` L389-418: `requests += 1`; unknown tag → ignored; tag not `Idle` → `answer(-EINVAL)`, `refused += 1` ("the driver never does this"); offline → `answer(-ENODEV)`, `offline_rejected += 1`; `decode_request` Ok → `Ready`, pushed to `ready`; Err → `answer(-errno)`, `refused += 1`.

`fetch` L438-469: `pump` only if `ready` is empty (errors swallowed); gate: `in_engine >= high` → closed (`gate_closed += 1` once), `None`; closed and `in_engine > low` → `None`; else open, pop a ready tag, `InEngine`, `in_engine += 1`, per-kind stat, `Request { id: request_id(q_id, tag, gen), kind }`.

`copy_in` L471-486 / `copy_out` L488-497: live iff `q == q_id && state == InEngine && gen == gen` else `IoError::Corrupt`; `copy_in` also requires `Write` and `dst.len() == blocks*4096` (`BadLength`).

`complete` L499-518: unknown/stale (wrong queue, not `InEngine`, wrong gen) → `stale += 1`, return; else `Idle`, `kind = None`, `gen += 1`, `in_engine -= 1`, `errors += 1` if not `Ok`, `answer(tag, errno_for(..))`.

`answer` L426-434: `transport.commit(tag, result)`; `Err` → `commit_failures += 1`, `last_error`, **state := Aborted** (the tag is retired for the front door's life; nothing un-retires it).

**What the generation protects against**: a completion (or copy) for an earlier use of a tag — after the tag completed or was aborted while the engine still held the request (the abort-mid-flight test, `engine_tests.rs:356-432`; `frontdoor_tests.rs:442-480`). After recovery a brand-new front door starts every tag at gen 0, and the old daemon's `Request`s died with the old process, so no cross-daemon staleness arises.

### 4.4 Daemon death with `USER_RECOVERY` (kernel, in order)

1. `kill -9` → the task exits; io_uring cancels the task's pending uring_cmds: for each ACTIVE io, `ublk_ch_uring_cmd` with `IO_URING_F_CANCEL` → `ublk_uring_cmd_cancel_fn` (L1464-1497).
2. The first cancel runs `ublk_abort_requests` (L1410-1440): `canceling = true`; quiesce; `ublk_abort_queue` (L1387-1408) handles every non-ACTIVE io with a started request: `ABORTED`, then `__ublk_fail_req` — REISSUE: requeue; no REISSUE: **fail with EIO**; unquiesce. Each cancelled fetch gets `RES_ABORT`. `schedule_work(quiesce_work)`.
3. Requests dispatched afterwards or whose task_work runs in a workqueue → `__ublk_abort_rq` → requeued (K9).
4. `ublk_quiesce_work_fn` (L1558-1570): `__ublk_quiesce_dev` → `blk_mq_quiesce_queue`, wait until no started request (3 ms polls), **`state = QUIESCED`**; then `ublk_cancel_dev`.
5. `exit_files` closes `/dev/ublkcN` → `ublk_ch_release` clears `UB_STATE_OPEN`; the io_uring ctx is freed. (Order between 4 and 5 is not guaranteed; `START_USER_RECOVERY` needs both.)
6. **If every tag was OWNED at death** (no ACTIVE cmd to cancel), step 1-2 never run; the quiesce is scheduled by `ublk_timeout` (L1209-1248) when a request times out (blk-mq default 30 s) and the daemon is `PF_EXITING`. Until then the device is `LIVE` with a dead pid and I/O hangs.
7. From here `/dev/ublkbN` still exists, every I/O to it queues in blk-mq and **hangs** (no FAIL_IO in 6.12) until `END_USER_RECOVERY` or `STOP_DEV`. `GET_DEV_INFO` shows `state = 2`, `ublksrv_pid = <dead pid>`, `flags` with bit 3, `ublksrv_flags` as created, `owner_uid/gid` = root.

### 4.5 The recovery protocol a new daemon must run (exact order, with the kernel's answers)

Preconditions: root (`CAP_SYS_ADMIN`), the same `nr_hw_queues`/`queue_depth` the device was created with (read them from `GET_DEV_INFO`, do not trust the spec), and the **serving thread** runs steps 5-8.

1. `UblkControl::open()` → `UblkControlRing::new` (any thread; one ring per device as today, or one shared ring for the scan).
2. `GET_DEV_INFO(dev_id)` → `UblksrvCtrlDevInfo`. Decide: `flags & USER_RECOVERY == 0` → not ours/not recoverable (skip); `state == DEAD (0)` → stopped or never started: not recoverable (`START_USER_RECOVERY` would be `-EBUSY` forever); `state == LIVE (1)` and `kill(pid, 0)` succeeds → a live server owns it (leave it alone; if the pid is ours it is one we created); `state == LIVE` and the pid is gone → the quiesce has not run yet (wait, see step 3's retry); `state == QUIESCED (2)` → proceed. Match the device to a volume by `ublksrv_flags` (§5.2) and corroborate with `GET_PARAMS` `basic.dev_sectors == size_blocks * 8` and `queue_depth`.
3. `START_USER_RECOVERY(dev_id)` (`CtrlCommand::start_user_recovery`, `execute(.., None)`):
   - `Ok(0)` → continue;
   - `Err(EBUSY)` (raw 16) → the dying process still holds the char fd, or the quiesce work has not run: **retry with a backoff** (e.g. 5 ms doubling to 250 ms) until a deadline. The deadline must tolerate the 30 s request-timeout path of 4.4 step 6 if the budget allows, or report "still quiescing" and let the daemon come up without the device (then recover later, or reap). Never open `/dev/ublkcN` before this succeeds (that would set `UB_STATE_OPEN` and make START `-EBUSY` by our own hand);
   - `Err(EINVAL)` (22) → no recovery flag or no queue was ever ready → reap (DEL_DEV);
   - `Err(ENODEV)` (19) → gone (someone else reaped it);
   - `Err(EPERM)` (1) → not root.
4. (Nothing else before the char device: `SET_PARAMS` is possible in QUIESCED but pointless; do not issue `START_DEV`.)
5. `UblkChar::open(dev_id)` → `-EBUSY` only if the fd is held elsewhere (cannot happen after step 3 succeeded unless another process raced in; treat as a retry of 3).
6. `UblkQueue::new(chr, 0, info.queue_depth, depth*2)` — the mmap pins the new `ub->mm` (K2).
7. `push_fetch(tag)` for every tag, `submit()` — the driver accepts them because `ublk_queue_reinit` cleared `nr_io_ready` and every io's flags (K4/K6); the last fetch pins `ubq_daemon = current` and completes `ub->completion`.
8. `END_USER_RECOVERY(dev_id, pid)` (`CtrlCommand::end_user_recovery`) — returns at once since the completion is done (same shape as `create` + `start` today): `Ok(0)` → `state = LIVE`, the blk-mq queue unquiesced, the requeue list kicked; `Err(EBUSY)` → a `STOP_DEV` raced in (the device is DEAD; reap); `Err(EINTR)` → a signal interrupted the wait (retry END, the fetches stay pending); `Err(EINVAL)` → the flag vanished (impossible for ours).
9. Build `UblkFrontDoor::new(LinuxTransport{queue}, 0, vol, size_blocks, depth, for_depth(depth))`, mark `started = true` in the `UblkDevice` so `delete()` stops it first, record `block_path()` (unchanged `/dev/ublkbN`).
10. The requeued requests (K9) re-enter `ublk_queue_rq` and arrive as ordinary `RES_OK` fetch completions on the new daemon's tags, as if freshly issued; no "re-dispatch" is visible to the front door and nothing from the old daemon's generation survives.

### 4.6 The reaper's protocol (another process, root)

1. Scan: for `id in 0..ublks_max`: `GET_DEV_INFO(id)`; `ENODEV` → skip. (Or `ls /sys/class/ublk-char/` for the ids that exist, then `GET_DEV_INFO` each.)
2. Classify: `ublksrv_flags` has the OpenTAP tag? (`ours`); `state`; `ublksrv_pid` alive (`kill(pid, 0)`: `ESRCH` → gone; `EPERM` → exists, not ours to signal, treat as alive); `owner_uid`.
3. Reap when: ours and (`state == QUIESCED`, or `state == DEAD`, or `state == LIVE` with the pid gone and the quiesce still pending — only after the 30 s window, or when explicitly forced). Never when `LIVE` with a live pid (that is a serving daemon: STOP_DEV would give every one of its tags `RES_ABORT`).
4. `STOP_DEV(id)` (safe in any state, K10; a no-op when DEAD), then `DEL_DEV(id)` (blocks until the idr is freed; if a stray process holds `/dev/ublkcN`, the wait is interruptible and returns `-EINTR`) or `DEL_DEV_ASYNC(id)` to not wait.

### 4.7 Pool stop (today) — for contrast

`Pool::stop` `pool.rs:840-863`: for each device `drop_device` (`set_offline(true)`, drain, `destroy_device` → drop front door [closes the char fd and the ring] → `delete()` → `STOP_DEV`, `DEL_DEV`), then `engine.shutdown()`, wait `Stopped`. A `kill -9` skips all of it: the device stays `QUIESCED` (after the quiesce work), the arena file stays mapped-with-contents on hugetlbfs, the `flock` is released by the kernel with the process.

---

## 5. Hook points for Step 6

### 5.1 `otap-ublk/src/device.rs`: `UblkDevice::recover` (smallest coherent change)

Add, mirroring `create`/`open_queue`/`start`:

```rust
/// Re-attaches a quiesced `UBLK_F_USER_RECOVERY` device of a dead daemon
/// (START_USER_RECOVERY, the char device, a fetch on every tag,
/// END_USER_RECOVERY). Linux; the calling thread serves the queue.
pub fn recover(dev_id: u32, retry: RecoveryRetry) -> io::Result<Self>  // or (dev_id, &DeviceSpec)
```

Steps inside: `UblkControlRing::new(UblkControl::open()?)`; `get_dev_info` (the 64-byte buffer pattern of `create` L98-108: `CtrlBuf { addr: bytes.as_ptr().expose_provenance() as u64, len: 64 }`, `execute(&cmd, Some(&mut bytes))`, `read_from_bytes`); check `flags & UBLK_F_USER_RECOVERY`, `state == UBLK_S_DEV_QUIESCED`, `nr_hw_queues == 1`; loop `execute(&CtrlCommand::start_user_recovery(dev_id), None)` while `raw_os_error() == Some(16)` within `retry`; `UblkChar::open`; `UblkQueue::new(chr, 0, info.queue_depth, depth*2)`; fetch every tag; `submit`; `execute(&CtrlCommand::end_user_recovery(dev_id, std::process::id()), None)`; return `Self { control, dev_id, info, queue: Some(queue), started: true }`. On any failure after START succeeded and before END: close the queue (drop) — the device stays QUIESCED-with-fetches-reset; the caller decides (retry or reap). Add `pub fn get_dev_info(&mut self) -> io::Result<UblksrvCtrlDevInfo>` (refreshes `info`) and `pub fn delete_async`/`stop` as needed.

A separate `pub fn probe_device(control: &mut UblkControlRing, dev_id) -> io::Result<Option<UblksrvCtrlDevInfo>>` (`ENODEV` → `None`) and `pub fn delete_device(control, dev_id, wait: bool)` serve the reaper without a `UblkDevice`.

Must not break: `create`'s flag set (add `UBLK_F_USER_RECOVERY_REISSUE` only by decision, §8 Q2); `take_transport` single use; `delete()`'s "queue dropped before DEL_DEV" order (L203-208) — for a recovered device the same order holds.

Also set `ublksrv_flags` in `create` (L90-97): a new `DeviceSpec` field or a `tag: u64` parameter (§5.2). Pure, testable on Windows: a `pub fn device_tag(agg_id: &Uuid, vol: u32) -> u64` / `parse_device_tag(u64) -> Option<(u32, u16)>` pair in `otap-ublk` (or `otap-sys::ublk`) with a const-asserted layout.

### 5.2 The device→volume tag (`ublksrv_flags`, 64 bits, write-once)

The kernel does not know the pool or volume; `ublksrv_flags` is the only kernel-held userspace word (K13). Proposal for the lead: `magic:16 = 0x4F54 ("OT") << 48 | layout:8 << 40 | vol:16 << 24 ... ` — simplest sound layout: `[63:48] magic 0x4F54, [47:32] vol (u16; MAX_VOLS = 1024, otap-format/src/constants.rs:54), [31:0] the first 4 bytes of agg_id (Uuid)`. Corroborate on recovery with `GET_PARAMS` (`dev_sectors == size_blocks * 8`) and `queue_depth == 128` before trusting a match; a 32-bit pool prefix collides only between two pools on one host sharing a prefix, which the size check and the "one pool per daemon" rule make harmless. Record it as an S-rule or E-entry (it is a protocol between daemon generations).

### 5.3 `otap-daemon`

- `pool.rs:281-293` (boot): before the `ensure_device` loop, when `spec.ublk`, scan the kernel once (`otap_ublk::device::scan()` → `Vec<(dev_id, info)>`), keep the ones whose tag names this pool, and for each online volume with a QUIESCED survivor call a new `device.rs::recover_device(dev_id, vol, size_blocks, ..)` instead of `create_device`; log "volume {vol} re-attached {path}". Survivors that match no online volume (deleted/offline volume, or `state` DEAD) → reap (STOP+DEL), logged. Survivors with a foreign tag → leave, log. The `Booted.reattached` flag is not required for this (the ublk state is the authority) but a crash with a heap arena still recovers the device (the volume is under nvfail, so requests answer `EIO` until cleared — consistent with `engine_tests.rs:287-334`).
- `pool.rs:678` `ensure_device`: unchanged for the create path; `drop_device` unchanged.
- `daemon.rs:127` `handle` and `pool.rs:744` `handle`: new `Request::DeviceList` → `Response::Devices { devices: Vec<DeviceReport> }` and `Request::DeviceReap { dev_id: Option<u32>, force: bool }` → `Response::Reaped { dev_ids }` (or `Ok`). `DeviceList` should work **without a booted pool** (the daemon-level `handle`, like `Ping`), since the reaper's main use is a wedged device from a previous daemon. `otap-control` `protocol.rs:42` adds the variants (`deny_unknown_fields` means an old daemon answers a new client with a parse error → `ErrorCode::Protocol`; bump `PROTOCOL_VERSION` only if the lead wants old clients refused).
- `otap-ctl/src/args.rs:244-445`: a `"device"` group with `list` and `reap [--dev-id N] [--all] [--force]`; `render.rs:20` arms for the new responses; `ctl_tests.rs` covers parsing.
- The engine thread is the only place that may run `recover_device` (K3); `DeviceList`/`DeviceReap` touch only the control device and may run on the engine thread too (they are quick, except `DEL_DEV` with `wait=true`, see §7).

### 5.4 `otap-ublk/src/mock.rs`: what the mock must simulate for (A) to be testable on Windows

Split the mock into a **device** that outlives any transport and a **transport** that is one daemon's handle:

- `MockDevice` (shared, `Rc<RefCell<..>>` or `Arc<Mutex<..>>`; `Clone` handles): `dev_id`, `flags` (USER_RECOVERY, REISSUE as knobs), `state: DEAD|LIVE|QUIESCED`, `ublksrv_pid: i32`, `ublksrv_flags: u64`, `owner_uid`, `open: bool` (UB_STATE_OPEN), `depth`, `nr_io_ready`, `owner_token` (the "task" that fetched; the transport's identity), per-tag `io: Active|Owned|None` with the owned request's descriptor and payload, `requeued: VecDeque<(desc, payload, app_req_id)>` (the blk-mq requeue list), `pending: VecDeque<..>` (requests the block layer holds while quiesced or with no fetch), `app_results: Vec<(app_req_id, i32)>` (what the block layer/application saw: the only place a REISSUE-vs-fail difference shows).
- Control-plane methods returning `Result<_, i32>` (errno, never panicking): `add_dev(info) -> dev_id`, `start_dev(pid)` (-EINVAL pid ≤ 0, -EEXIST if LIVE, blocks → in the mock: `-EAGAIN` unless every tag fetched), `stop_dev()`, `del_dev(wait)` (-EINTR/-EBUSY-like when `open` and `wait`), `get_dev_info() -> UblksrvCtrlDevInfo`, `get_params`, `start_user_recovery()` (-EINVAL without the flag or `nr_queues_ready == 0`; **-EBUSY while `open` or state != QUIESCED**; else reinit: every tag `None`, `nr_io_ready = 0`, owner cleared), `end_user_recovery(pid)` (-EAGAIN until every tag fetched [the real one blocks]; -EBUSY if not QUIESCED; else LIVE and **re-dispatch**: move `requeued` then `pending` onto tags with a pending fetch as `QueueEvent::Request`), `open_char() -> Result<MockTransport, EBUSY>`, and the transport's drop/`close()` clears `open`.
- Death: `kill_daemon()` models 4.4 steps 1-5 as explicit sub-steps so tests can hit the windows: `cancel()` (owned tags: REISSUE → `requeued`, else `app_results.push((id, -EIO))`; active fetches → dropped; undelivered events → `pending`/`requeued`), `run_quiesce_work()` (LIVE → QUIESCED), `release_char()` (`open = false`). A convenience `crash()` runs all three; tests for the `-EBUSY` retry call them one at a time. Also the "every tag owned" case where only the timeout path quiesces (a `timeout_fires()` step).
- `MockTransport` becomes a handle: `poll` drains the device's delivered events for this owner; `commit` on a tag not `Owned` → `TransportError { "commit of an idle tag", 22 }` (as today); a `commit`/`poll` after `kill_daemon` → error (the fd is dead); keep today's `submit_*`, `abort`, `inject`, `fail_*`, `take_commits`, `read_payload`, `outstanding` on the transport, implemented against the shared device so the existing tests keep passing unchanged (`MockTransport::new(depth)` = a fresh device + handle).
- Application-level ids: `submit_write` etc. should return an `app_req_id` so a test can assert after recovery that every acknowledged write (a commit the mock recorded before `kill_daemon`) is never re-delivered, every owned-not-committed request is re-delivered exactly once (REISSUE) or failed exactly once (no REISSUE), and every pending one is delivered exactly once.
- The reaper's view: a `MockKernel { devices: BTreeMap<u32, MockDevice>, ublks_max }` with `get_dev_info(id)` (-ENODEV), `add_dev` (-EACCES at `ublks_max`), `del_dev`, so `otapctl device list/reap` logic can be tested against a trait (`trait UblkControlPlane`) that the Linux `UblkControlRing` also implements.

### 5.5 Where recovery must not change behaviour

- `UblkFrontDoor` needs **no change** for recovery: a recovered queue is a fresh front door with gen 0 tags; the requeued requests arrive as normal `Request`s. Keep E28's state machine and its proptest model untouched (`frontdoor_proptests.rs` mirrors `pump/fetch/complete` exactly).
- `LinuxTransport::poll`'s "any non-OK result is an Abort" (L297-303) stays, but see §7 for the fetch-refused trap.
- `Pool::stop`'s order (devices first, then the shutdown CP) stays.

---

## 6. Test patterns to copy

- **State machine without an engine**: `frontdoor_tests.rs` — `door(depth)` L32-41 builds `UblkFrontDoor<MockTransport>`; the test plays the engine through `FrontDoor` (`fetch`, `copy_in/out`, `complete`), then reads `transport().take_commits()`. Abort/stale/refused-commit patterns at L394-440, L442-480, L644-670. A recovery test in this style: build a `MockDevice`, a first front door, fetch some requests, commit some, `kill_daemon`, drop the front door, build a second front door over `device.open_char()`, run START/fetch/END on the mock, pump, assert which tags arrive and what `app_results` holds.
- **Model-based property test**: `frontdoor_proptests.rs` — `Op` enum L28-52, `op_strategy` L65-81, a `Model` that mirrors `pump/fetch/complete` L98-242, invariants after every step L429-447, drain at the end L449-465; `proptest!` with `cases: 256` L468-472. For (A), add `Op::Crash`, `Op::QuiesceWork`, `Op::ReleaseChar`, `Op::StartRecovery`, `Op::EndRecovery`, `Op::Reattach` and model the kernel's io states; the invariant to pin is the app-results ledger (no acknowledged write re-delivered or lost; each owned request failed-or-reissued exactly once; `-EBUSY` exactly while `open || state != QUIESCED`).
- **Engine behind the front door**: `engine_tests.rs` — `fresh_engine_with` L105-110 (format + boot over `SimBackstore`), `tick_until` L112-120 (200 000 ticks cap), `wait(e, tag)` L124-135 (`take_commit(tag)`), `write`/`read` helpers L146-165. **The crash pattern** L287-334: `e.into_parts()` → `(disk, front, clock, memory)`, `disk.crash_power()`, a new front door, `boot(disk, front)`, `take_events()` → `Event::Booted { dirty, nvfail }`. For (A), copy it with `disk.crash_daemon()` (`otap-sim/src/harness.rs:1186-1196` keeps `memory`) and **reuse `memory`** so `Booted.reattached == true`, replace `front_door(..)` by a front door re-attached to the surviving `MockDevice`, then assert the re-dispatched requests complete and the acknowledged data is intact. The `boot` helper L81-97 passes a fresh `HugeArena::anonymous`; a variant `boot_with_memory(cfg, disk, front, memory)` is needed.
- **Adversarial/regression style**: `review_ublk_tests.rs` — each defect-reproducing test carries a doc comment naming the defect; the `proptest!` block uses `failure_persistence: None` with the defaults L318-322 (so `PROPTEST_CASES` drives it).
- **Simulator daemon crash**: `otap-sim/src/harness.rs:1186` `Sim::crash_daemon` + `SimFrontDoor::reset` (`otap-sim/src/frontdoor.rs:120`) — the full-stack "kill -9, arena kept" scenario the sim suites already run (`docs/06:348`).
- **Daemon-level tests on Windows**: `otap-daemon/tests/daemon_tests.rs` — `DaemonConfig { control: tcp:127.0.0.1:0, ublk: false, op_timeout: 60 s, ..default() }` L47-54, `Daemon::new(config())`, requests through `handle`. `DeviceList` on Windows should answer `ErrorCode::Unsupported` (or an empty list) — pick one and test it; the reaper's decision logic (`classify`) is pure and tested against the mock kernel.
- **`otapctl` parsing**: `otap-ctl/src/args.rs` tests at L505-760 (`request("pool status")` style) — add `device list`, `device reap --dev-id 3`, `device reap --all`.
- **PROPTEST_CASES**: honoured two ways in the tree. (i) `proptest!` with an explicit `cases:` still obeys the variable because proptest's `contextualize_config` applies `PROPTEST_CASES` over any explicit count (`docs/07:1503-1510`, `docs/09:134-136`). (ii) The Step 5 reviewer preferred the intent in the file: `fn proptest_cases(default: u32) -> u32 { std::env::var("PROPTEST_CASES").ok().and_then(|v| v.parse().ok()).unwrap_or(default) }` used as `cases: proptest_cases(64)` (`step5_sim_tests.rs:1376-1391`). Use (ii) for a new walk; keep `failure_persistence: None` (no regression files in the tree, `docs/reviews/step1-format-qa-pass.md:11`). The gate is `PROPTEST_CASES=10000 cargo test --workspace -j 3 -- --test-threads=6` (`docs/09:57`), ~180 min; a worker runs only its own test binaries with `-j 2`.
- Linux-only code stays type-checked: `cargo clippy --workspace --all-targets --target x86_64-unknown-linux-gnu -j 3 -- -D warnings` (`docs/09:52`). `device.rs` is `#[cfg(target_os = "linux")]` at the module level (`lib.rs:45-46`); keep the pure parts (tag encoding, classification, retry policy) outside that cfg so Windows tests cover them.

---

## 7. Risks, pitfalls and traps

1. **Open the char device only after `START_USER_RECOVERY` succeeds.** Opening it first sets `UB_STATE_OPEN` and START answers `-EBUSY` forever (K1, K6). Likewise the reaper must never open `/dev/ublkcN` to "probe" a device.
2. **`-EBUSY` is ambiguous and can last 30 s.** It means "fd still open or not yet QUIESCED". When the daemon died with every tag owned by the server (the watermark lets all 128 tags be in flight), nothing is cancellable and only `ublk_timeout` (blk-mq's 30 s request timeout) schedules the quiesce (4.4 step 6). A retry deadline of 2 s will miss that case; decide what the daemon does then (boot without the device and retry in the background, or wait). Also `GET_DEV_INFO` shows `LIVE` with a dead pid during that window: do not read `LIVE` as "a daemon is alive"; check the pid.
3. **Without `REISSUE`, every request the dead daemon had fetched but not committed is failed with EIO to the application** (K9), including writes the daemon had already placed in the arena (they are then replayed on re-attach: the application saw an error for a write that landed — allowed by block semantics, but a surprise). With `REISSUE` those requests are re-delivered and written again (same data, same LBA; idempotent). `REISSUE` is in `UBLK_F_ALL` (6.12 supports it) and is not in `REQUIRED_UBLK_FEATURES`; adding it changes `ADD_DEV`'s flags and the probe. Decision for the lead (§8 Q2).
4. **A quiesced device hangs I/O, it does not fail it** (no `FAIL_IO` in 6.12). `dd` on `/dev/ublkbN` of a dead daemon blocks in D state until END or STOP_DEV. The reaper's `STOP_DEV` is what unwedges it (force_abort fails the requeued requests). Note `scripts/linux-smoke.sh:260-262` already expects the old device to linger.
5. **`DEL_DEV` blocks while anyone holds `/dev/ublkcN`** (K10): the idr wait needs the cdev's last reference dropped. A reaper running inside `dbladed` must not DEL a device whose fd the daemon itself holds (its own live devices), and a `wait=true` delete of someone else's open device hangs the engine thread until `-EINTR`. Use `STOP_DEV` + `DEL_DEV_ASYNC` from the daemon, or `DEL_DEV` only from a path that can afford to block.
6. **`STOP_DEV`/`DEL_DEV` have no owner check**: a reaper that mis-classifies a live device kills it (every fetch gets `RES_ABORT` → the front door marks every tag `Aborted` and the volume silently serves nothing: there is no "all tags aborted → device gone" detection in `UblkFrontDoor` or `Pool`). Classify on `state` (QUIESCED/DEAD) and on the pid; consider adding `UblkFrontDoor::all_aborted()` and having `Pool::tick` drop such a device.
7. **`LinuxTransport::poll` turns any non-zero fetch result into an `Abort`** (L297-303). A fetch refused by the driver (`-EBUSY` because the queue was already ready, `-EINVAL` from the wrong thread, K3/K4) retires the tag silently. After recovery, issue the fetches exactly once per tag and from the serving thread; a double fetch loses the tag for the daemon's life.
8. **Thread pinning**: the queue's commands must come from the thread that issued the last fetch (K3); `Pool::boot` and `handle` run on `"dbladed-engine"` so `recover_device` must run there too (not on the control server thread). The mmap must come from the same process as every other queue's (K2); one process is fine.
9. **`queue_depth`/`nr_hw_queues` come from the kernel on recovery**, not from `DeviceSpec` (if a future spec changes `depth`, the fetch count must match the device's `queue_depth` or END waits forever / fetches are refused). Also `max_io_buf_bytes` was rounded by the driver (K13).
10. **`ublksrv_flags` is write-once at `ADD_DEV`** and today `create` leaves it 0: devices created by Step 5 binaries carry no tag. The reaper should treat `ublksrv_flags == 0` with `owner_uid == 0` and `flags` == exactly our set (`USER_COPY|USER_RECOVERY|CMD_IOCTL_ENCODE|URING_CMD_COMP_IN_TASK`) as "probably ours, untagged" and reap only with `--force`.
11. **`GET_DEV_INFO2` needs a dev path** even for root (L2840-2851); use `GET_DEV_INFO` (`CtrlCommand::get_dev_info`, builder exists). `GET_FEATURES` reports the flags the driver supports (`UBLK_F_ALL & ~ZERO_COPY`, L2780), which is how to detect a kernel without `USER_RECOVERY` at all (older than 6.1) — `Allowlist::probe` already does this on Linux (`probe.rs:306-333`).
12. **`ublks_max` (64) counts stale devices**: 64 unreaped crashes and `ADD_DEV` answers `-EACCES`. The reaper is availability, not hygiene. Running `dbladed` with more than 64 volumes needs the module parameter raised anyway (D for scope D's scripts).
13. **The pid check is heuristic**: `ublksrv_pid` may be reused by another process; `state == QUIESCED` is the kernel's own statement that the server died (only the dying path sets it); prefer it. `STOP_DEV` sets `pid = -1` (L1611).
14. **`UblkControlRing::execute` returns `-EINTR` as an error without retry** (L1272-1273): `END_USER_RECOVERY`'s wait and `DEL_DEV`'s idr wait are interruptible; a signal (SIGTERM during a restart) surfaces as `Err(Interrupted)`; the fetches remain pending, so END can simply be reissued.
15. **No `Drop` on `UblkDevice`**: a `recover` that fails midway must say what it left behind (QUIESCED with reset fetches is recoverable again; after END it is LIVE under our pid and must be `delete()`d or served).
16. **The mock's `deliver` panics on misuse** (L58-66); the recovery mock must return errnos, since the whole point is to exercise the kernel's refusals (`-EBUSY`, `-EINVAL`, `-ENODEV`, `-EEXIST`, `-EACCES`).
17. **`CHANNEL_ID_BIT`** (`daemon/frontdoor.rs:63`) and `DaemonFrontDoor::complete` route ublk ids by `req.vol()`; a recovered device keyed by the same `vol` slots into the same `BTreeMap` entry. Nothing else in the composite needs to know.
18. **Heap arena + recovery**: with `--heap-arena` the device recovers but the volume is under nvfail (dirty boot); the recovered requests answer `EIO` until `volume nvfail-clear`. The smoke script's heap lane (`linux-smoke.sh:265-269`) expects exactly that.
19. **Two pools, one host**: each `dbladed` must only recover/reap devices whose tag names its own `agg_id`; the 32-bit prefix plus the size check is the guard. A standalone `otapctl device reap --all` (if the lead wants one that works with no daemon) needs the tag's magic only.
20. **E28's doc wording "first fetch"** vs the kernel's "last fetch": same thread in practice; do not split a queue's fetches across threads.

---

## 8. Open questions for the lead

- **Q1. Where does the reaper live and what are its privileges?** `otapctl` is unprivileged and only speaks to `dbladed` (S5-4). A `Request::DeviceList/DeviceReap` served by the daemon keeps that model but cannot reap when no daemon runs (the wedged-device case after a crash *before* the restart, or when the restart itself fails). Options: (a) the daemon reaps at boot (automatic) and answers `device list/reap` while running; (b) `otapctl device reap --direct` opens `/dev/ublk-control` itself when run as root; (c) both. Which?
- **Q2. `UBLK_F_USER_RECOVERY_REISSUE`: on or off?** Off (today): fetched-uncommitted requests fail with EIO at the crash, re-attach only resumes new and never-delivered I/O. On: they are re-delivered to the new daemon (same data written twice for a placed-but-uncommitted write, harmless). The brief's "without duplicating or dropping acknowledged writes" holds either way (acknowledged = committed to the driver = gone from the driver). It changes `ADD_DEV`'s flags, `REQUIRED_UBLK_FEATURES`, and the mock's ledger. I lean to ON for application-visible availability; it needs an E-entry.
- **Q3. The `ublksrv_flags` tag layout** (§5.2) and whether it is recorded as an S-rule (a protocol between daemon generations and tools) or an E-entry. Also whether a mismatch between the tag's `vol` and the catalogue (a volume deleted and its index reused across the crash) reaps the device or refuses to boot the volume.
- **Q4. The `-EBUSY` retry budget and the 30 s timeout window** (§7.2): does the daemon boot without the device and keep retrying in the tick loop (then `ensure_device` creates nothing while a tagged survivor exists), or wait up to 30 s at boot, or create a fresh device and reap the old one once it quiesces (which changes `/dev/ublkbN`, the thing recovery is meant to keep stable)?
- **Q5. Should recovery require `Booted.reattached` (the arena came back)?** With a heap arena the device recovers into a volume under nvfail; is that wanted, or should a heap-arena restart reap and recreate?
- **Q6. Deterministic dev ids?** `ADD_DEV` accepts a requested id (`-EEXIST` if taken). A stable `/dev/ublkbN` per volume across a *clean* restart would also be possible by choosing the id; today `ANY_DEV` is used. Out of scope unless the lead wants it.
- **Q7. Protocol version**: adding `Request::Device*`/`Response::Devices` variants — bump `PROTOCOL_VERSION` (refuses old clients) or keep 1 (old daemons answer a `Protocol` error to the new verbs)?
- **Q8. Reaper policy for foreign or untagged ublk devices** (other software's devices, or Step 5 binaries' untagged ones): list only, or reap with `--force`?
- **Q9. Should `UblkFrontDoor`/`Pool` detect "every tag aborted" (device torn down under us) and drop the device?** Not needed for (A) but the reaper makes it reachable (§7.6).
