# Map: `crates/otap-daemon` (dbladed) for Step 6 scopes A, B and C

Read-only survey, 2026-10-14, of the daemon crate at the Step 5 commit (5114866)
plus the neighbouring facts the daemon leans on. Line numbers are from the files
as they stand today. Where a doc and the code disagree it is said in place and
collected in section 7 (item 23).

## 1. Scope: what was read

Whole files, every line:

| File | Lines | What it is |
|---|---|---|
| `crates/otap-daemon/src/lib.rs` | 1-59 | crate doc (the design in nine sentences), module list, re-exports, `VERSION` |
| `crates/otap-daemon/src/config.rs` | 1-100 | `Overrides`, `DaemonConfig` and its `Default` |
| `crates/otap-daemon/src/daemon.rs` | 1-366 | `Daemon`, `spawn`/`run`/`tick`/`handle`, `DaemonHandle`, `install_signal_flag` |
| `crates/otap-daemon/src/pool.rs` | 1-1251 | `Pool` (boot, every request, waits, devices, status, log) |
| `crates/otap-daemon/src/frontdoor.rs` | 1-339 | `IoHandle`, `ChannelFrontDoor`, `VolumeDevice`, `DaemonFrontDoor` |
| `crates/otap-daemon/src/device.rs` | 1-56 | Linux: `create_device`, `destroy_device` |
| `crates/otap-daemon/src/arena.rs` | 1-56 | `open_arena` |
| `crates/otap-daemon/src/backstore.rs` | 1-351 | probe, `open_backstore`, file geometry, `holds_data` |
| `crates/otap-daemon/src/clock.rs` | 1-40 | `SystemClock` |
| `crates/otap-daemon/src/report.rs` | 1-104 | `volume_report`, `snapshot_report`, `pool_report` |
| `crates/otap-daemon/src/server.rs` | 1-301 | `Server`, `serve`, `connection` |
| `crates/otap-daemon/src/bin/dbladed.rs` | 1-188 | argument parsing and `main` |
| `crates/otap-daemon/tests/daemon_tests.rs` | 1-949 | the lead's in-process suite (6 tests) |
| `crates/otap-daemon/tests/review_daemon_tests.rs` | 1-640 | the Step 5 review's suite (9 tests) |
| `crates/otap-daemon/Cargo.toml` | all | deps: arena, backstore, control, cp, format, geom, sys, ublk, blake3, signal-hook; dev-dep otap-sim |

Neighbours, read in the parts the daemon calls (cited where used):
`otap-control/src/{protocol,units,wire,client}.rs` (the whole protocol, the bounds, the framing, `Endpoint`/`Client`);
`otap-cp/src/engine.rs` (`EngineConfig` 151-192, `Event` 334-532, `BootFailure` 811-836, status structs 867-984, `boot` 1637-1727, `tick` 2076-2118, `request_cp`/`request_clean` 2122-2132, `shutdown` 2240-2243, `set_replication_base` 2523-2560, `audit_liveness` 2820, `snapshot_read`/`snapshot_diff`/`live_diff` 2916-3007, the accessors 3154-3330, `serve_front_door` 3353-3381);
`otap-cp/src/traits.rs` 220-501 (`Backstore`, `ReqId`, `RequestKind`, `Request`, `Status`, `FrontDoor`, `Clock`, `Net`);
`otap-sys/src/hugetlb.rs` 1-160 and 280-400 (`HugeArena::map_file`/`heap`, the Linux re-attach); `otap-sys/src/ublk.rs` (flags 270-300, states 318-324, `UblksrvCtrlDevInfo` 443-472, recovery commands 790-820, `UblkControl`/`UblkChar`/`UblkControlRing` 1043-1240);
`otap-ublk/src/{lib,device}.rs` whole, `frontdoor.rs` 1-532, `mock.rs` 1-60;
`otap-sim/src/{lib,frontdoor}.rs` whole, `harness.rs` (pub surface, `crash_daemon` 1186-1196), `net.rs` 1-60, `tests/step5_sim_tests.rs` 1376-1391;
`otap-ublk/tests/engine_tests.rs` 1-90; `otap-ctl/src/args.rs` (dispatch 244-451), `tests/ctl_tests.rs` 1-80, `render.rs` 148-208;
`docs/07` E09, E16, E28, E34, E38, E39; `docs/06` sections 7-8 (427-562); `docs/08` Step 5 entry (797-1031); `docs/10` Step 6 brief (578-627); `docs/09` 40-75, 120-145; `scripts/linux-smoke.sh` (daemon lines); workspace `Cargo.toml`.

## 2. Public API and key internals

### 2.1 `lib.rs`

- `#![forbid(unsafe_code)] #![deny(missing_docs)]` (lib.rs:33-34). `pub mod device` only under `#[cfg(target_os = "linux")]` (42-43).
- Re-exports (49-56): `open_arena`; `open_backstore, probe_superblock, Opened`; `SystemClock`; `DaemonConfig, Overrides`; `install_signal_flag, Daemon, DaemonHandle`; `ChannelFrontDoor, DaemonFrontDoor, IoCompletion, IoHandle`; `engine_error, status_word, BootSpec, Pool, PoolEngine`; `serve, Server`.
- `pub const VERSION: &str = env!("CARGO_PKG_VERSION")` (59), reported by `Ping`.
- The crate doc (4-31) is the design statement: one engine thread owns the engine, ticks it, serves the devices through `DaemonFrontDoor`, executes every control request between ticks; a request that needs a CP ticks until its event arrives within a timeout; the daemon "keeps no state of its own beyond the command line it was started with" (29-31).

### 2.2 `config.rs`

- `Overrides { media: Option<MediaKind>, stripe_bytes: Option<u32>, segment_bytes: Option<u64>, flush_opaque: Option<bool>, strict_flush: bool }` (13-24); `flush_policy()` -> `RequireFuaOrFlush` when strict else `TrustBarrierWithNvfail` (29-35); `media()` (39-45); `geom_overrides()` -> `otap_geom::Overrides` with `discard: None`, `allow_misaligned: false` (49-59).
- `DaemonConfig { control: Endpoint, device: Option<PathBuf>, arena_bytes: u64, arena: ArenaKind, cp_interval_secs: u32, overrides: Overrides, ublk: bool, allow_remote: bool, op_timeout: Duration }` (64-84).
- `Default` (86-99): `Endpoint::default_for_host()` (unix `/run/opentap/dbladed.sock` on Linux, `tcp:127.0.0.1:7421` elsewhere, client.rs:25-31), `DEFAULT_ARENA_BYTES` = 8 GiB (units.rs:7), `ArenaKind::default_for_host()` (hugetlb `/run/opentap/arena/dbladed` on Linux, heap elsewhere, units.rs:28-36), `cp_interval_secs` 5, `ublk: cfg!(target_os = "linux")`, `allow_remote: false`, `op_timeout: 120 s`.

### 2.3 `daemon.rs`

- `type Call = (Request, Sender<Reply>)` (18): what travels over the engine thread's channel.
- `pub struct Daemon { cfg, clock: SystemClock, pool: Option<Pool>, exit: bool, stop: Option<Arc<AtomicBool>> }` (21-29).
- `Daemon::new(cfg)` (55-63); `with_stop_flag(cfg, stop)` (69-74); `config()` (78); `pool() -> Option<&Pool>` (84); `exit_requested()` (90).
- `boot_configured(&mut self) -> Result<(), ControlError>` (95-110): builds a `BootSpec` from `cfg` and calls `Pool::boot(&spec, &self.clock, self.stop.clone())`.
- `tick(&mut self) -> bool` (113-124): `pool.tick()`; an `Err` is printed to stderr and reads as not busy.
- `handle(&mut self, req: &Request) -> Reply` (127-213): the daemon-level dispatch. Handles `Ping`, `PoolFormat`, `PoolBoot`, `PoolStop`, `DaemonStop` itself; every other variant goes to `pool.handle(other)` or fails `ErrorCode::NoPool` (208-211). `PoolBoot` takes `ublk` and `op_timeout` from the daemon's own `cfg`, not from the request (188-189).
- `shutdown(&mut self)` (217-225): `pool.take()` -> `pool.stop()`, logs "pool {} stopped cleanly" or the error, sets `exit`.
- `spawn(cfg, stop) -> Result<(DaemonHandle, JoinHandle<()>), ControlError>` (232-275): thread "dbladed-engine"; boots the configured pool *inside the thread* and reports over `boot_tx`; `spawn` blocks on `boot_rx.recv()` so the pool is booted (or the error returned) before `spawn` returns; on failure the thread is joined and the error returned (263-266).
- `run(&mut self, rx, stop)` (278-318): the engine loop, see section 4.2.
- `idle_wait(streak, pool) -> Duration` (325-331): 50 ms with no pool; else `200 us << min(streak, 4)` capped at 2 ms (200, 400, 800, 1600, 2000 us).
- `DaemonHandle { tx: Sender<Call>, stopped: Arc<AtomicBool> }` (335-338), `Clone`; `call(&self, req: &Request) -> Reply` (342-350): clones the request, sends, blocks on a one-shot reply channel; `ErrorCode::Io` "the daemon has stopped" / "the daemon stopped before it answered". `is_stopped()` (354-356).
- `install_signal_flag() -> io::Result<Arc<AtomicBool>>` (361-366): `signal_hook::flag::register` for SIGTERM and SIGINT.

### 2.4 `pool.rs`

- `pub type PoolEngine = Engine<Box<dyn Backstore>, DaemonFrontDoor, SystemClock>` (33).
- `BootSpec { device: PathBuf, overrides, arena_bytes: u64, arena: ArenaKind, cp_interval_secs: u32, ublk: bool, op_timeout: Duration }` (37-52).
- `const LOG_LINES: usize = 64` (55); `const IDLE_TICK: Duration = 200 us` (58).
- `new_agg_id(clock) -> Uuid` (65-78): BLAKE3 over wall, mono, pid, counter; v4/variant bits.
- `pub fn engine_error(e: EngineError) -> ControlError` (82-96): `NoSuchVolume|NoSuchSnapshot|NoSuchName` -> `NotFound`; `BadSize|NameExists|MetadataTooLarge|Format` -> `BadRequest`; `Unsupported` -> `Unsupported`; `Io` -> `Io`; everything else (`VolumeBusy`, `SnapshotPinned`, ...) -> `Engine`.
- `pub fn status_word(Status) -> &'static str` (104-118): `ok`, `no_such_volume`, `out_of_range`, `nvfailed`, `corrupt`, `stopped`, `failed`, `no_space`, `too_large`, `offline`, `unknown`.
- `blocks_for(size_bytes) -> Result<u64>` (121-126): zero refused, rounds up to 4 KiB blocks.
- `pub struct Pool` (129-150): `engine: PoolEngine`, `io: IoHandle` (private; the only Send way into the engine), `device: String`, `arena_bytes`, `arena_word` (= `ArenaKind::describe()`: `heap` or `hugetlb:<path>`), `arena_slots: u32`, `cp_interval_secs`, `last_boot: String`, `rotational: bool`, `stripe_bytes: u32`, `ublk: bool`, `devices: BTreeMap<u32, String>` (vol -> `/dev/ublkbN` path), `op_timeout`, `log: VecDeque<String>`, `stop: Option<Arc<AtomicBool>>`, `stopping: bool`.
- `Pool::format(device, overrides, size_bytes, force, clock) -> Reply` (167-202).
- `Pool::boot(spec, clock, stop) -> Result<Pool, ControlError>` (209-295): section 4.4.
- Accessors: `device()` (299), `engine() -> &PoolEngine` (305, shared ref only), `device_path(vol)` (311), `recent_events() -> Vec<String>` (317).
- `log(&mut self, line)` (322-328): `eprintln!("dbladed: {line}")` **and** push to the 64-line ring. `note(&mut self, ev: &Event)` (331-432): turns an event into a log line; `Booted` also sets `self.last_boot` (373). Events with no line (`CpCommitted`, `IntentWritten`, `SegmentFreed`, `CleanSkipped`) hit the `_ => return` arm (429).
- `tick_raw() -> Result<bool>` (435-443): `engine.tick()`; busy = `stats changed || !engine.is_quiet() || front_door().in_engine() > 0`. `tick()` (447-453) = `tick_raw` + `note` every event.
- `interrupted()` (459-465): `!self.stopping && stop flag set`. `interrupted_error(what)` (467-472): `ErrorCode::Engine`, "the daemon is shutting down; {what} was not waited for".
- `wait_for<T>(what, pred: FnMut(&Event) -> Option<T>) -> Result<T>` (476-520) and `wait_until(what, done: FnMut(&mut PoolEngine) -> bool)` (523-556): section 4.3.
- `wait_quiet()` (561-566): `Ok(())` at once when `engine.is_pool_full()`, else `wait_until(is_quiet)`.
- `wait_cp_outcome(what) -> Result<u64>` (570-587): waits for `CpCommitted{cp_seq}` -> Ok, or `CpFailed` -> `ErrorCode::Engine` with the "if the pool is full, delete a snapshot or punch holes, then `pool clean`" hint. Discards `CpCommitted.max_seq`.
- `sync_now() -> Result<u64>` (592-596): `wait_quiet`; `engine.request_cp()`; `wait_cp_outcome`.
- `vol_index(name) -> Result<u32>` (601-614): catalogue name, or `#<index>` for a present volume (the nameless-volume escape of E39 W4). `snap_index(vol, name) -> Result<u64>` (616-626).
- Reports: `volume_report_for(vol)` (628-642, counts non-deleting snapshots, device path from `self.devices`), `report_volume` (644), `snapshot_reports(Option<vol>)` (650), `report_snapshot(snap_id)` (659).
- `ensure_device(vol)` (678-709) and `drop_device(vol)` (714-739): section 4.5.
- `handle(&mut self, req) -> Reply` (744-809): exhaustive `match` over `Request` (no wildcard): adding a variant to `otap_control::Request` fails this compile until it is dispatched.
- `status()` (812-821); `sync()` (824-827); `clean()` (830-835); `stop()` (840-863); `volume_create` (866-878); `volume_delete` (881-890); `volume_online` (895-911); `volume_resize` (915-934); `volume_rename` (937-944); `volume_nvfail_clear` (947-959); `snapshot_create` (963-996); `snapshot_delete` (999-1004) and `delete_snapshot_by_id` (1006-1012); `snapshot_rollback` (1019-1114); `snapshot_pin` (1118-1146); `snapshot_replication_base` (1149-1160); `io(kind, data) -> Result<IoCompletion>` (1164-1205); `io_report` (1207-1213); `check_blocks` (1215-1222, 1..=`MAX_IO_BLOCKS`); `io_write` (1225-1234); `io_read` (1237-1242); `io_punch` (1245-1250).

### 2.5 `frontdoor.rs`

- `IoCompletion { status: Status, seq: u64, data: Option<Vec<u8>> }` (14-21); `data` is `Some` only for a read that ended `Status::Ok` (166-170).
- `Submitted { kind, data, reply }` (23-27) and `InFlight { data, read, reply }` (29-33), private.
- `IoHandle { tx: Sender<Submitted> }` (38-40), `Clone`, `Send`; `submit(&self, kind: RequestKind, data: Option<Vec<u8>>) -> Receiver<IoCompletion>` (53-57). A send error is ignored (`let _ =`), so a handle whose front door is gone yields a receiver that reads `Disconnected`.
- `const CHANNEL_ID_BIT: u64 = 1 << 63` (63): channel request ids carry bit 63; ublk ids are `gen << 32 | q << 16 | tag` (otap-ublk frontdoor.rs:213-215) and reach bit 63 only after 2^31 completions of one tag.
- `ChannelFrontDoor { rx, handle, queue: VecDeque<Request>, inflight: HashMap<ReqId, InFlight>, next_id }` (66-72); `new()` (92-101, `next_id` starts at 1); `handle()` (105); `in_engine()` (111); `owns(id)` = bit 63 set **and** present in `inflight` (117-119); `pull()` (123-137) drains the channel into `queue` assigning ids.
- `impl FrontDoor for ChannelFrontDoor` (140-174): `fetch` pulls when the queue is empty, pops front; `copy_in` requires `data.len() == dst.len()` exactly (`BadLength` otherwise); `copy_out` stores a copy; `complete` removes the entry and sends the completion.
- `VolumeDevice { device: UblkDevice, front: UblkFrontDoor<LinuxTransport> }` (177-183), Linux only.
- `DaemonFrontDoor { channel, devices: BTreeMap<u32, VolumeDevice> (Linux) | BTreeMap<u32, ()> (else), turn: usize (Linux) }` (186-195); `new()` (215); `io_handle()` (226); `in_engine()` (232-244, channel + every device); `device_volumes()` (248); Linux-only `add_device` (254), `remove_device` (260), `device` (266), `device_mut` (272).
- `impl FrontDoor for DaemonFrontDoor` (277-330): `fetch` polls the devices round-robin from `turn` and returns the first request found, advancing `turn` past it; **the channel is asked only when no device had one** (295). `copy_in`/`copy_out`/`complete` route by `channel.owns(req.id)` first, else by `req.vol()` to that volume's device, else `IoError::Corrupt` / silently dropped (298-329).
- `pub fn blocks_of(bytes: usize) -> Option<u32>` (334-339): whole non-empty 4 KiB blocks.

### 2.6 `device.rs` (Linux only)

- `create_device(vol, size_blocks, stripe_bytes, rotational, arena_slots) -> Result<(VolumeDevice, String), ControlError>` (12-45): `DeviceSpec::new(size_blocks, stripe_bytes, rotational).for_arena(arena_slots)` (otap-ublk device.rs:44-66: 128 tags, 8 MiB requests capped to half the arena, discards to 1 GiB); `UblkDevice::create` (ADD_DEV with `USER_COPY | USER_RECOVERY | CMD_IOCTL_ENCODE`, SET_PARAMS, open `/dev/ublkcN`, a fetch on every tag, device.rs:88-125); `front_door(&mut device, vol, &spec)` takes the queue as `LinuxTransport` (344-360); `device.start()` (START_DEV with the pid, 184-190); path `/dev/ublkbN` (235-237). Every failure deletes the device before returning. Must run on the thread that will serve the queue (doc 8-11; E28).
- `destroy_device(dev: VolumeDevice) -> Result<(), ControlError>` (50-56): drops the front door, then `device.delete()` (STOP_DEV if started, then DEL_DEV, device.rs:214-219).

### 2.7 `arena.rs`

- `open_arena(kind: &ArenaKind, bytes: u64) -> Result<HugeArena, ControlError>` (10-56): `check_arena_bytes` (64 MiB <= bytes <= 256 GiB, multiple of 2 MiB; units.rs:50-71); `Heap` -> `HugeArena::heap(len, TwoMiB)`; `Hugetlb{path}` -> refuses a missing parent directory, then `HugeArena::map_file(path, len, TwoMiB)`; an `Unsupported` io error maps to `ErrorCode::Unsupported`, anything else to `Io`.
- Re-attach semantics live in otap-sys: `map_file` opens with `create(true).truncate(false)` ("Never truncate on open: an existing file is the arena to re-attach, with its contents", hugetlb.rs:336-344), takes a non-blocking exclusive `flock` (346-355, `WouldBlock` = "mapped by another arena"), refuses a longer file, grows a shorter one, `MAP_SHARED` (390). So re-attach is implicit: the same path maps the same bytes. On non-Linux `map_file` is `Unsupported` (hugetlb.rs:17).
- Whether the *contents* are a usable arena is decided by the engine, not the daemon: `Engine::boot` calls `Arena::can_attach(&memory, &agg_id)` then `Arena::attach` (engine.rs:1687-1709); a wrong aggregate fails the boot (`WrongAggregate`), an unreconcilable table creates afresh with `arena_lost = true`, a blank arena creates afresh. `Event::Booted.reattached` is `reattach.is_some()` (engine.rs:1991), i.e. the `Arena::attach` report exists.

### 2.8 `backstore.rs`

- `Opened { backstore: Box<dyn Backstore>, profile: MediaProfile, kind: "block"|"file", warnings: Vec<String> }` (15-24).
- `type RecordedGeometry = (u32, u32, u64)` = (stripe, segment, capacity) from the superblock (F15) (38).
- `file_geometry(size, overrides)` (44-68): below 1 GiB a 64 KiB stripe and 2 MiB segments, else 1 MiB and 64 MiB; flash; segment must be whole stripes.
- `profile_for(geom, media, overrides, file, cp_timer_ns)` (70-89): a file gets `DiscardPolicy::Never`; the CP timer replaces the profile's.
- `is_block_device` (96-107, Linux only true), `device_size` (110-121).
- `PROBE_STRIPES` 8 KiB..4 MiB (125-136); `read_superblock(f, path, offset)` (140-155).
- `probe_superblock(path) -> Result<Option<(Uuid, RecordedGeometry)>>` (164-195): `A0` at 0, else for every candidate stripe `A1` at the stripe-rounded slot size and `B0`/`B1` at the end under both the raw size and the stripe-rounded size (E39 W5).
- `holds_data(path) -> Result<bool>` (200-222): a non-zero byte in the first or last MiB (E39 R2).
- `open_backstore(path, overrides, create_bytes, recorded, cp_timer_ns) -> Result<Opened>` (230-302): block device -> `open_block`; missing path needs `create_bytes` (a format) -> `FileBackstore::create`; existing file: the recorded geometry overrides the file defaults, an operator stripe/segment that disagrees is refused, a capacity above the file size is refused (263-292); `FileBackstore::open`.
- `open_block` Linux (305-336): `plan_device(&Sysfs::new(name), name, &overrides.geom_overrides())`, `UringBackstore::open`, warnings as `{w:?}`; non-Linux (339-351) -> `Unsupported`.

### 2.9 `clock.rs`, `report.rs`

- `SystemClock { started: Instant }` (9-11), `mono_ns` since construction, `wall_ns` since the epoch (29-40).
- `volume_report(index, &VolumeStatus, device: Option<String>, snapshots: usize) -> VolumeReport` (9-27): `size_bytes = size_blocks * 4096`, `nvfail` as the lower-cased `Debug` of the cause.
- `snapshot_report(&SnapshotInfo, volume: Option<String>)` (31-56): `consistency` = `fsfreeze|crash`, `origin` = `scheduled|user|transfer|divergent`.
- `pool_report(engine, device, arena_bytes, arena, cp_interval_secs, last_boot) -> PoolReport` (61-104): from `engine.pool_status()`, `engine.stats()`, `engine.layout()`, `engine.config().agg_id`.

### 2.10 `server.rs`

- `ACCEPT_POLL = 20 ms` (17); `IDLE_TIMEOUT = 300 s` (22): the read timeout between requests on a connection.
- `Server { listener, endpoint }` (66-69). `bind(endpoint)` = `bind_with(endpoint, false)` (85-87); `bind_with(endpoint, allow_remote)` (93-127): a TCP address that is not loopback is refused with `BadRequest` unless `allow_remote` (96-105); port 0 takes an ephemeral port reported by `endpoint()` (109-115, 131); Unix via `bind_unix` (182-219): a path that exists is probed with `UnixStream::connect`, a live daemon is refused, a stale socket removed; missing directory refused; mode 0660; non-unix hosts -> `Unsupported`.
- `accept()` (146-163): blocking accept, `set_nodelay(true)` on TCP, `set_read_timeout(IDLE_TIMEOUT)`.
- `Drop for Server` removes the Unix socket file (172-179).
- `pub fn serve(server, handle: &DaemonHandle, stop: &AtomicBool) -> io::Result<()>` (224-248): non-blocking accept loop; returns when `stop` is set or `handle.is_stopped()`; one thread "dbladed-control" per connection running `connection(conn, &h)`.
- `connection(conn, handle)` (253-301): per frame `read_frame::<Request>` -> `handle.call(&req)` -> `write_frame(reply)`; `Ok(None)`/`Closed` end it quietly; `WouldBlock|TimedOut|ConnectionReset` (the idle timeout) end it quietly; `Decode` -> `BadRequest` reply and the connection goes on; an `Encode` failure of a reply gets a fallback error reply.

### 2.11 `bin/dbladed.rs`

- `USAGE` (15-48) lists every flag; `enum Parsed { Run(DaemonConfig), Help, Version }` (50-54).
- `parse(args: &[String]) -> Result<Parsed, String>` (56-136): accepts `--flag value` and `--flag=value` (69-78); arms at 80-121: `-h/--help`, `-V/--version`, `--control`, `--device`, `--arena-bytes` (`parse_size`), `--arena-file`, `--heap-arena`, `--cp-interval`, `--media hdd|flash|ssd|nvme`, `--stripe`, `--segment`, `--flush-opaque`, `--strict-flush`, `--no-ublk`, `--allow-remote-control`, `--op-timeout` (>= 1 s); unknown -> error (121). After the loop: `--heap-arena` and `--arena-file` exclude each other (125-127), `ArenaKind` chosen (128-132), `check_arena_bytes`, `check_cp_interval` (133-134).
- **To add a flag**: a `DaemonConfig` field with a `Default` (config.rs:64-99), a USAGE line, one `"--name" => ...` arm using `take(&mut i)?` for a value. There are no unit tests of `parse` in this file (otap-ctl's `args.rs` has its own, 497-583).
- `main` (138-188): parse -> `install_signal_flag` -> `Server::bind_with(&cfg.control, cfg.allow_remote)` -> log "listening on" -> `Daemon::spawn(cfg, stop)` (boots the pool before returning; failure -> exit 1) -> `serve(server, &handle, &stop)` -> `drop(handle)` -> `thread.join()` -> "stopped". Exit codes: 2 for a bad command line, 1 for failures, 0 otherwise.

### 2.12 Control protocol items the daemon depends on (`otap-control`)

- `PROTOCOL_VERSION: u32 = 1` (protocol.rs:9); `MAX_IO_BLOCKS: u32 = 256` (12, "1 MiB"); `MAX_FRAME_BYTES: u32 = 4 << 20` (wire.rs:14).
- `Request` (protocol.rs:42-218), `#[serde(tag = "op", rename_all = "snake_case", deny_unknown_fields)]`: `Ping`, `PoolFormat{device, size_bytes, media, stripe_bytes, segment_bytes, flush_opaque, strict_flush, force}`, `PoolBoot{device, media, stripe_bytes, segment_bytes, flush_opaque, strict_flush, arena_bytes, arena: ArenaKind, cp_interval_secs}`, `PoolStatus`, `PoolSync`, `PoolClean`, `PoolStop`, `DaemonStop`, `VolumeCreate{name, size_bytes}`, `VolumeList`, `VolumeShow{name}`, `VolumeDelete{name}`, `VolumeOnline{name, online}`, `VolumeResize{name, size_bytes}`, `VolumeRename{name, new_name}`, `VolumeNvfailClear{name}`, `SnapshotCreate{volume, name}`, `SnapshotList{volume: Option}`, `SnapshotDelete{volume, name}`, `SnapshotRollback{volume, name, delete_newer}`, `SnapshotPin{volume, name, flags: u8}`, `SnapshotReplicationBase{volume, name: Option}`, `IoWrite{volume, lba, data_hex}`, `IoRead{volume, lba, blocks}`, `IoPunch{volume, lba, blocks}`.
- `Response` (357-405), `#[serde(tag = "kind", ...)]`: `Ok`, `Pong{protocol, version, pool_booted}`, `Formatted{agg_id, stripe_bytes, segment_bytes, capacity_bytes, segments}`, `Pool(PoolReport)`, `Volumes{volumes}`, `Volume(VolumeReport)`, `Snapshots{snapshots}`, `Snapshot(SnapshotReport)`, `Io(IoResultReport)`, `Committed{cp_seq}`. `Response` is `PartialEq` but not `Eq` (`PoolReport` holds `f64`).
- `ErrorCode` (410-429): `NoPool, PoolBooted, NotFound, BadRequest, Engine, Timeout, Io, Unsupported, Protocol`. `ControlError { code, message }` (434-450). `type Reply = Result<Response, ControlError>` (461).
- `PoolReport` fields (279-340): `device, agg_id, cp_seq, fold_cp, serving, free_segments, segments, reserve_segments, segment_bytes, stripe_bytes, utilization, admissible_bytes, unflushed_bytes, arena_occupancy, arena_bytes, arena, cp_interval_secs, pool_full, flush_policy, flush_opaque, discard, warnings, volumes, snapshots, cleaned_segments, relocated_blocks, cps, writes, reads, last_boot`. `VolumeReport` (223-244): `index, name, size_bytes, serving, offline, nvfail, rollback_gen, op_pending, device, snapshots`. `SnapshotReport` (249-274). `IoResultReport { status, seq, data_hex }` (345-352).
- `ArenaKind::{Hugetlb{path}, Heap}` (27-37); `describe()` (units.rs:40-45); `check_arena_bytes` (units.rs:50-71); `check_cp_interval` 1..=15 (74-81); `parse_size` (85).
- Framing: 4-byte LE length + JSON (wire.rs:1, 64-100). `Endpoint::parse` (client.rs:35-57); `Client::connect(endpoint, timeout)` (147) sets the timeout on connect, reads and writes; `Client::hello` checks the protocol.
- `protocol.rs:463-464` has a `mod tests` that round-trips every variant through JSON (E38, docs/08 871-877): new variants belong there.

### 2.13 Engine items `pool.rs` calls (`otap-cp`)

- `Engine::boot(cfg, backstore, front, clock, memory: HugeArena) -> Result<Engine, BootFailure<B,F,C>>` (engine.rs:1637-1727); `BootFailure { error, backstore, front, clock, memory }` with `into_parts()` (811-836): the daemon discards the parts and keeps only `f.error` (pool.rs:252-257).
- `tick() -> Result<(), EngineError>` (2076-2118): backstore poll, CP state machine, intents, discards, the boot retry, paced acks, `serve_front_door` only `if self.serving && !self.stopped` (2109-2113; a stopped engine refuses everything with `Status::Stopped`), `maybe_cut`, `maybe_start_cp`, `maybe_pump`.
- `serve_front_door` (3353-3381): deferred requests first, then **at most 64 `fetch` calls per tick**; a write or punch is deferred when something is already deferred; a deferred write asks for a CP.
- `request_cp()` (2122-2125) sets `cp_requested` and `pool_retry`; `request_clean()` (2129-2132); `shutdown()` (2240-2243) sets `shutdown_requested` and `serving = false`.
- `take_events() -> Vec<Event>` (3166, `mem::take`: an event is delivered once, to whoever ticks); `is_serving()` (3172); `is_stopped()` (3178); `is_pool_full()` (3185); `is_quiet()` (3194-3200: `State::Idle && frozen_len() == 0 && !cp_requested && !boot_retry && paced.is_empty()`); `cp_seq()` (3204); `stats() -> Stats` (3294, `Copy + PartialEq`: `cps, stripes, writes, punches, reads, deferred, intents, recycled, discards, folds, snapshots, ...`, 1311-1333); `layout()` (3300); `config()` (3306); `map() -> &MapSet` (3312); `front_door() -> &mut F` (3317); `backstore() -> &mut B` (3322); `arena_memory_mut()` (3328).
- Volume/snapshot API: `create_named_volume(name, blocks) -> Result<u32>` (2720), `delete_volume` (2697), `set_volume_online(vol, bool)` (2565, immediate; durable at the next CP, F18), `resize_volume` (2667), `set_volume_name` (2744), `nvfail_clear(vol) -> Result<u32>` (2251), `create_named_snapshot(vol, name, SnapKind, SnapOrigin) -> Result<Uuid>` (2315), `delete_snapshot(snap_id)` (2464), `set_snapshot_pins(snap_id, flags)` (2500), `set_replication_base(vol, Option<snap_id>)` (2523-2560: sets `PIN_REPLICATION` on the named snapshot and clears it on every other snapshot of the volume, through `set_snapshot_pins`, so the events are `SnapshotPinned`), `rollback_volume(vol, snap_id)` (2627), `volume_name` (2761), `find_volume` (2768), `find_snapshot(vol, name)` (2794), `volumes() -> Vec<(u32, VolumeStatus)>` (2806), `volume(vol) -> Option<VolumeStatus>` (3241), `snapshots() -> Vec<SnapshotInfo>` (2895), `pool_status()` (3028).
- Reading API (all `&mut self`, engine thread only): `snapshot_read(snap_id, lba, dst) -> Result<Vec<PeekSource>>` (2916-2954), `snapshot_diff(snap_id, base: Option<u64>) -> Result<Vec<(u64, MapEntry)>>` (2958-2991, "every block of snapshot `snap_id` born after `base`: what a delta ships"; refuses `base` not older than the snapshot with `SnapshotOrder`), `live_diff(vol, base) -> Result<Vec<(u64, MapEntry)>>` (2995-3007), `map_entry(vol, lba)` (3157), `audit_liveness() -> Result<Vec<LivenessDrift>>` (2820-2860, walks every tree; `LivenessDrift { pseg, counted, found }` 924-931), `peek`/`peek_disk` (3100-3117).
- Bounds: `MAX_REQUEST_BLOCKS = 2048` (8 MiB, engine.rs:37), `MAX_PUNCH_BLOCKS = 1 << 18` (41), `META_RESERVE_SEGMENTS = 2` (46).
- `Event` (334-532): `Booted{cp_seq, dirty, reattached, replayed_blocks, rewound, arena_lost, damaged_slots, orphaned_blocks, nvfail: VolBitSet, torn}`, `CpCommitted{cp_seq, trigger, gen, blocks, max_seq}`, `CpFailed{cp_seq, reason}`, `Folded`, `IntentWritten`, `NvfailCleared{vol, rollback_gen}`, `VolumeCreated{vol, cp_seq}`, `SnapshotCreated{snap_id, vol, cp_seq}`, `SnapshotFailed{uuid, vol, reason}`, `SegmentFreed`, `SegmentCleaned`, `CleanSkipped`, `SnapshotDeleted{snap_id, vol, cp_seq, released}`, `SnapshotPinned{snap_id, flags}`, `VolumeRolledBack{vol, snap_id, cp_seq, rollback_gen}`, `VolumeResized{vol, size_blocks, cp_seq}`, `VolumeDeleted{vol, cp_seq}`, `OperatorWarning(_)`, `ArenaDamage{vol, lba, slot}`, `Internal{what}`, `Stopped{cp_seq, clean}`.
- `VolumeStatus` (867-890: `present, serving, size_blocks, rollback_gen, nvfail, clear_pending, active_windows, offline, name, op_pending`), `SnapshotInfo` (894-919), `PoolStatus` (935-984).
- Traits (traits.rs): `FrontDoor { fetch, copy_in, copy_out, complete }` (438-452); `Request { id: ReqId, kind }` with `vol()` and `is_punch()` (320-349); `RequestKind::{Write, Read, Flush, Discard, WriteZeroes}` (268-316); `Status` (354-379, `#[non_exhaustive]`); `Net { send(to, bytes) -> Result<(), NetError>; poll_recv(out) }` with `EndpointId(u32)` and `NetError::{Busy, Unreachable}` (464-501): the network abstraction the replication transport was designed against; `otap_sim::LoopbackNet` implements it (net.rs:28-60).

### 2.14 ublk and recovery items (`otap-ublk`, `otap-sys`)

- `UblkDevice::create(&DeviceSpec)` (device.rs:88-125), `start` (184), `stop` (196), `delete` (214), `dev_id()` (223), `info() -> &UblksrvCtrlDevInfo` (229), `block_path()` (235), `take_transport() -> Option<LinuxTransport>` (242-247, once; same-thread rule). `open_queue` (166-177): `UblkChar::open(dev_id)`, `UblkQueue::new(chr, 0, depth, depth*2)`, `push_fetch` on every tag, `submit`. `ADD_DEV` leaves `ublksrv_flags` at its default 0 (device.rs:90-97): the 64-bit field "for the server's own use; the driver does not interpret it" (ublk.rs:462-463) is unused today.
- `UblkFrontDoor<T: Transport>` (frontdoor.rs:224-238): `new(transport, q_id, vol, size_blocks, depth, watermarks)` (247-282, every tag starts `Idle`: a fetch is assumed pending on each), `set_offline(bool)` (288-302: answers ready requests `ENODEV`, in-engine ones finish), `is_offline`, `in_engine()` (312), `ready()`, `stats()`, `transport()`, `last_error()` (341), `aborted_tags()` (347), `pump()` (359-423: a transport error is remembered and returned; `Abort` retires the tag; a request on a busy tag is answered `EINVAL`), `fetch` (438-469, the watermark gate), `copy_in`/`copy_out` check the id's generation (471-497), `complete` (499-518) answers the driver through `transport.commit`.
- `Transport { poll, read_data, write_data, commit }` (62-76); `QueueEvent::{Request{tag, desc}, Abort{tag}}` (29-42); `MockTransport::new(depth)` (mock.rs:42) drives it on any host.
- otap-ublk lib.rs:31-36 states: "no recovery path for `UBLK_F_USER_RECOVERY` yet (the device is created with the flag; a tag whose fetch fails is retired; a new daemon re-attaching the queue is `dbladed`'s, Step 5)". **Disagrees with E38 "Not done" and docs/06 section 8:** Step 5 did not do it; it is Step 6's.
- otap-sys: `UBLK_F_USER_RECOVERY` (ublk.rs:283), `UBLK_F_USER_RECOVERY_REISSUE` (286, "requests in flight at the server's death are reissued to the new server"; not set by `create`), `UBLK_S_DEV_DEAD = 0 | LIVE = 1 | QUIESCED = 2` (319-324), `UblksrvCtrlDevInfo { nr_hw_queues, queue_depth, state, max_io_buf_bytes, dev_id, ublksrv_pid: i32, flags, ublksrv_flags, owner_uid, owner_gid, .. }` (443-472), `CtrlCommand::get_dev_info(dev_id, buf)` (~790), `start_user_recovery(dev_id)` (807), `end_user_recovery(dev_id, pid)` (815-819: "completes once the new server has a fetch pending on every tag"), `UblkControl::open()` `/dev/ublk-control` (1063), `UblkChar::open(dev_id)` (1120), `UblkControlRing::new(control)` and `execute(&cmd, buf) -> io::Result<i32>` (1194, 1213: synchronous, one command at a time). There is no device enumeration helper anywhere in otap-sys or otap-ublk.

## 3. Invariants and rules in force

Docs (E-numbers in `docs/07-engineering-decisions.md`, S5 rulings in E34):

- **E38 "One engine thread"** (1332-1351): the engine thread owns the `Engine` over `Box<dyn Backstore>` and the composite front door; "it ticks the engine, and executes every control request between ticks, so nothing else ever touches the engine and no lock exists"; a request that needs a CP ticks until its event; requests whose durability has no event (online/offline, rename, replication base) "wait for quiet, request a CP and wait for its commit"; "requests are answered one at a time ... the control plane is not a hot path"; idle backoff 200 us to 2 ms "so a request on a device waits at most 2 ms".
- **E38 "Devices"** (1352-1361): a ublk device is created and started on the engine thread (the driver pins the queue to the task that issued its first fetch, E28 827-868) for every online volume at boot, at `volume create` and `volume online`; taken down at `volume offline`, before a rollback, at `pool stop` and at exit; recreated at the new size after a resize; requests bounded by half the arena; `--no-ublk` serves none; Windows never does.
- **E38 "Names"/"no state"** (1406-1409) and **S5-4** (E34 1084-1098) and **S5-1** (1037-1055): every durable fact lives in the pool (catalogue F16, offline flags F18); "the daemon holds no durable state at all beyond its command line"; a heap arena dies with the process; only a hugetlbfs arena re-attaches; arena 64 MiB..256 GiB (units.rs), CP timer 1-15 s.
- **E38 "Shutdown"** (1379-1384): SIGTERM/SIGINT, `pool stop`, `daemon stop` and the loss of the last handle all run one path: every device down, the engine's clean shutdown, the `Stopped` event; `clean == false` is reported as an error.
- **E38 "Not done"** (1419-1426) and **docs/06 section 8, 550-556**: no `USER_RECOVERY` re-attach; a restarted daemon creates new devices (numbers change); a killed daemon's devices stay quiesced until a tool deletes them or the module is reloaded; a clean stop deletes its devices; "the `Io*` path is a test aid, not a data path".
- **E39** (1435-1522): a CP carrying clean targets takes no deletion and no volume operation, so "the daemon's waits see the event one CP later"; rollback/resize/deletion refuse `VolumeBusy` while a snapshot of the volume is in flight; `pool sync`/`pool clean` on a full pool request the CP and answer the attempt's failure, not a timeout; a signal interrupts an operation's wait except the shutdown's own; loopback-only TCP without `--allow-remote-control`; frames 4 MiB; idle connections closed after five minutes; `snapshot create` waits for its own uuid; `deny_unknown_fields` on every protocol type (R3); `otapctl` waits ten minutes by default.
- **E16** (339-381): the arena's re-attach rules (placed slots whose word and data verify are replayed, filled ones dropped, every untrusted slot reported and treated like a power loss); "Every arena method takes `&self` or `&mut self`; ... The engine of Step 3 is one thread by design."
- **E28** (827-868): the queue's commands must come from the task that issued its first fetch; a fetch or commit under `USER_COPY` carries `addr = 0`; `START_DEV` waits until every tag has a fetch; the kernel facts are from the saved 6.12 `ublk_drv.c` and "the first Linux run still has to exercise them".
- **E09 / docs/09** (205-224; 49-58, 123-145): every gate line including `cargo clippy --workspace --all-targets --target x86_64-unknown-linux-gnu -j 3 -- -D warnings`; `-j 3`, `--test-threads=6`; `PROPTEST_CASES=10000` locally; `prop_assert!` always with a message; adversarial suites never weakened; scripted edits one-line only.
- **Format rulings** the daemon relies on (otap-format lib.rs): F7 ring `A0 A1 B0 B1` (111) is what `probe_superblock` walks; F15 recorded geometry (185) wins over the operator's overrides at boot (backstore.rs:263-292, E38 1365-1369); F16 catalogue (202) is where every name lives; F18 offline volumes persisted (otap-format lib.rs:221; `VolumeStatus.offline`).
- **otap-cp lib.rs doc (60-72)**: a daemon crash whose arena verifies whole and nothing rewound replays placed writes and loses nothing; a power loss, a rewind or a damaged arena holds the write-active set under nvfail; "the boot writes a superblock before it serves".

Code-level rules (comments and structure):

- Only the engine thread touches `Pool`/`PoolEngine`: `Daemon` is moved into the thread (daemon.rs:245-254); `Pool::engine()` hands out `&PoolEngine` only (pool.rs:305); the only `Send` entry into the engine is `IoHandle` (frontdoor.rs:38-57), which is private to `Pool` (`io`, pool.rs:131) and has no accessor.
- `CHANNEL_ID_BIT` (frontdoor.rs:60-63): channel ids are `1<<63 | n`; `DaemonFrontDoor` routes by `channel.owns(id)` before falling back to the volume's device (298-329). A new door needs a disjoint id space and a claim in that routing.
- `copy_in` lengths are exact (frontdoor.rs:151-153): the engine asks for `blocks * 4096` bytes.
- The stop flag interrupts every wait except while `stopping` (pool.rs:144-149, 459-465, 841); `Pool::stop` sets `stopping = true` first (841).
- `wait_quiet` is exempt on a full pool (561-566) because a full pool "holds a frozen generation until a consistency point is requested and so never goes quiet on its own".
- A request's events are consumed by the wait that ticks (`take_events`); nothing is replayed to a later wait. Every existing wait matches by identity (`vol`, `snap_id`, `uuid`), never by kind alone; `snapshot_create` loops until its own uuid is listed (983-995).
- `ensure_device` is idempotent (`devices.contains_key` -> `Ok`, 681-683) and a no-op when `!self.ublk` or the volume is absent/offline (684-686); `drop_device` is a no-op when no device exists (717-719).
- `Pool::handle`'s match is exhaustive (744-808) and refuses the daemon-level variants with `BadRequest` "not a request for a booted pool" (803-807).
- Protocol facts serde enforces only at run time (E38 1389-1395, docs/08 871-877): an internally tagged enum cannot serialize a newtype variant holding a bare sequence (list replies are struct variants: `Volumes{volumes}`), and a payload field named `kind` collides with the `Response` tag.
- `--allow-remote-control` is the only way off loopback (server.rs:96-105); the control plane "can format any device the daemon can write".

## 4. How things happen, in order

### 4.1 Process start (`bin/dbladed.rs:138-188`)

1. `parse(&args)` (140) -> `DaemonConfig`; help/version exit 0; a bad line exits 2 with the usage.
2. `install_signal_flag()` (155) -> the `Arc<AtomicBool>` `stop`.
3. `Server::bind_with(&cfg.control, cfg.allow_remote)` (162): the socket exists from here, but nothing is accepted yet (connections queue in the listen backlog).
4. `Daemon::spawn(cfg, stop)` (170): the engine thread starts, `boot_configured()` runs **on the engine thread** (daemon.rs:246-247) and `spawn` returns only after the boot succeeded or failed (261-274). A failed boot ends the process with exit 1 (dbladed.rs:172-175).
5. `serve(server, &handle, &stop)` (177): the accept loop on the main thread until `stop` or the engine thread ends.
6. `drop(handle)` (180) -> the engine thread's `rx` disconnects -> `run` sets `exit` -> clean shutdown (daemon.rs:298-301, 307-310); `thread.join()` (181).

### 4.2 The engine thread's loop (`Daemon::run`, daemon.rs:278-318)

Per iteration:
1. `if stop.load() { exit = true }` (281-283). `if exit { shutdown(); return }` (284-287).
2. `busy = self.tick()` (288): one `Pool::tick` (engine tick + events noted).
3. `idle_streak` = 0 when busy, else +1 (289-293).
4. If busy: `rx.try_recv()` (non-blocking; 295-302) -> the loop spins tick/try_recv with **no sleep** while the engine has work (a CP in flight, requests in the engine, stats moving). If not busy: `rx.recv_timeout(idle_wait(streak, pool.is_some()))` (304-311): 200 us -> 2 ms backoff with a pool, 50 ms without. `Disconnected` -> `exit = true; continue` (the handle-drop path).
5. One request at most per iteration: `self.handle(&req)` then `reply.send(answer)` (313-316). While `handle` runs, nothing else is read from `rx`; the data path keeps flowing only because the request's own waits tick (4.3). The `stop` flag is seen inside those waits through `interrupted()`; `exit` is seen only at the top of the next iteration.

### 4.3 How a request waits (`pool.rs:476-556, 1164-1205`)

`wait_for(what, pred)` (476-520), per iteration: `tick_raw` (483) -> `take_events`, `note` each, apply `pred` to each until one matches, remember a `Stopped` (486-494) -> found -> `Ok(t)` (495-497) -> `stopped || engine.is_stopped()` -> `Engine` error "the engine stopped while waiting for {what}" (498-503) -> deadline (`Instant::now() + op_timeout`, set once at 481) -> `ErrorCode::Timeout` "{what} did not complete within N s" (504-512) -> `interrupted()` -> `interrupted_error` (513-515) -> `sleep(IDLE_TICK)` when not busy (516-518). A match in the same batch as `Stopped` wins.

`wait_until(what, done)` (523-556): `done(&mut engine)` first (530-532, before any tick) -> `is_stopped` -> deadline -> `interrupted` -> `self.tick()` (noted) -> sleep when not busy.

`io(kind, data)` (1164-1205): `rx = self.io.submit(kind, data)` (1169) -> loop: `rx.try_recv()` (`Ok` -> return; `Disconnected` -> `Engine` "the request was dropped by the engine") -> `is_stopped` -> deadline -> `interrupted` -> `tick` -> sleep. The request is fetched by the engine's next `serve_front_door` (only while `serving && !stopped`), the completion arrives through `ChannelFrontDoor::complete` on a later tick.

What happens on a request while a CP is in flight:
- `PoolStatus`, `VolumeList`, `VolumeShow`, `SnapshotList`: answered at once from engine state (pending work shows as `op_pending` / `deleting`).
- `PoolSync`, `PoolClean`, `VolumeOnline`, `VolumeRename`, `SnapshotReplicationBase`: `wait_quiet` ticks the in-flight CP (and any frozen generation) to completion, then `request_cp`, then the **next** `CpCommitted` (`sync_now`, 592-596): one extra CP when one was in flight; on a full pool the quiet wait is skipped and the attempt's `CpFailed` is the answer.
- `VolumeCreate/Delete/Resize/NvfailClear`, `SnapshotCreate/Delete/Pin`, `SnapshotRollback`: the engine call queues the operation and the daemon waits for its identity-matched event; with a CP in flight the operation rides the next CP, and if the in-flight CP carries clean targets, deletions and volume operations wait one more (E39); a snapshot of the volume in flight makes rollback/resize/delete fail at once with `VolumeBusy` -> `ErrorCode::Engine`.
- `Io*`: submitted into the channel; served by the next tick; a write is acknowledged when placed in the arena (deferred when the arena is full, paced from 80% occupancy); a CP in flight does not block it.

### 4.4 `Pool::boot` in full (`pool.rs:209-295`)

1. `check_arena_bytes(spec.arena_bytes)`, `check_cp_interval(spec.cp_interval_secs)` -> `BadRequest` (214-215).
2. `spec.device.exists()` else `NotFound` (216-221).
3. `probe_superblock(&spec.device)` (222) -> `(agg_id, recorded)`; `None` -> `NotFound` "holds no OpenTAP superblock in its ring ... format it first" (223-231).
4. `cp_timer_ns = cp_interval_secs * 1e9` (232); `open_backstore(&device, &overrides, None, Some(recorded), Some(cp_timer_ns))` (233-239): a block device goes through the planner and `UringBackstore` (Linux), a file through `FileBackstore::open` under the recorded geometry; operator overrides that disagree are refused.
5. `open_arena(&spec.arena, spec.arena_bytes)` (241): heap or `map_file` (re-attach is implicit: the same path, the same bytes). `arena_slots = otap_arena::slots_for_len(memory.len())` (242-243).
6. `EngineConfig::new(agg_id, Vec::new())` (244): **no configured volumes**; every volume comes from the disk.
7. `PoolEngine::boot(cfg, opened.backstore, DaemonFrontDoor::new(), clock.clone(), memory)` (245-257): the engine evaluates the ring, decides re-attach (`Arena::can_attach`/`attach`, engine.rs:1687-1709), decides nvfail, and schedules the boot commit; a `BootFailure` becomes `ErrorCode::Engine` "cannot boot {device}: {error}" and its parts (including the arena memory) are dropped.
8. `io = engine.front_door().io_handle()` (258); the `Pool` is assembled with `last_boot: "booting"`, `rotational = profile.media == Hdd`, `stripe_bytes`, `devices` empty (259-276); the planner's warnings are logged (277-279).
9. `pool.wait_until("the boot commit", |e| e.is_serving())` (280): ticks until the boot CP is durable; the `Booted` event is noted on the way and sets `last_boot` (331-375), e.g. `"dirty boot at cp 7, arena re-attached (12 blocks replayed)"`; `interrupted()` applies (a SIGTERM during boot aborts it).
10. For every present, non-offline volume (`engine.volumes()` filtered, 281-287): `ensure_device(vol)` (288-293); a failure runs `pool.stop()` (a clean shutdown CP) and returns the error.

"arena re-attached" in the status is therefore the `Booted.reattached` flag rendered by `note` (353-357), computed by the engine from the arena's header and table, not by the daemon from the file.

### 4.5 Devices: `ensure_device`, `drop_device`, the Linux `VolumeDevice` lifecycle

`ensure_device(vol)` (678-709), Linux: return if `!self.ublk` or a device exists (681-683); return if the volume is not present or is offline (684-686); `create_device(vol, st.size_blocks, self.stripe_bytes, self.rotational, self.arena_slots)` (687-693) on the engine thread; `engine.front_door().add_device(vol, device)` (694); `devices.insert(vol, path)`; log "volume {vol} is {path}" (695-696). Non-Linux: nothing (698-707).

`drop_device(vol)` (714-739), Linux: no device -> `Ok`; `front.set_offline(true)` (720-722: every ready and every new request answers `ENODEV`); `wait_until("the device's requests to drain", in_engine == 0)` (723-727: the requests the engine holds finish, under the op timeout and the stop flag); `remove_device` -> `destroy_device` (STOP_DEV, DEL_DEV) -> log "volume {vol} left {path}" (728-732). Non-Linux: just `devices.remove` (734-737).

When devices are created and torn down (all on the engine thread):
- Boot: created for every online volume after the boot commit (4.4 step 10).
- `volume_create` (866-878): after `VolumeCreated` -> `ensure_device`.
- `volume_online(name, true)` (897-902): `set_volume_online(true)` -> `sync_now` -> `ensure_device`. `volume_online(name, false)` (903-909): `set_volume_online(false)` -> `drop_device` -> `sync_now`.
- `volume_resize` (915-934): after `VolumeResized`, if a device exists: `drop_device` then `ensure_device` (recreated at the new size; a new `/dev/ublkbN` number).
- `snapshot_rollback` (1083-1112): if the volume was online: offline + `drop_device` before `sync_now`/`wait_quiet`/`rollback_volume`; after `VolumeRolledBack`: online + `sync_now` + `ensure_device`; on a refused rollback the online state and device are restored best-effort (1094-1101).
- `volume_delete` (881-890): no `drop_device`; it relies on the engine refusing to delete an online volume (the offline step already dropped the device) and just removes the map entry (888).
- `Pool::stop` (840-863): `stopping = true`; `drop_device` for every device; `engine.shutdown()`; `wait_for(Stopped{cp_seq, clean})`; `clean == false` -> `ErrorCode::Engine` with the hugetlbfs/heap explanation.
- Exit: `Daemon::shutdown` -> `Pool::stop` (daemon.rs:217-225); `DaemonStop` and `PoolStop` likewise (196-207). There is no `Drop` for `VolumeDevice`/`UblkDevice`: a path that drops a `Pool` without `stop` (a panic, `kill -9`) leaves the devices in the driver, quiesced under `USER_RECOVERY`.

### 4.6 `DaemonFrontDoor` routing (frontdoor.rs:277-330)

`fetch`: with devices, walk `keys[(turn + k) % n]` for `k in 0..n`, return the first `dev.front.fetch()` that yields and set `turn` past it (281-293); then, only if no device yielded, `channel.fetch()` (295). The engine calls `fetch` up to 64 times per tick (engine.rs:3360), so the channel is served only in ticks where the devices have nothing ready. `copy_in`/`copy_out`/`complete`: `channel.owns(id)` (bit 63 and in `inflight`) -> the channel; else the device of `req.vol()`; else `Corrupt`/dropped.

### 4.7 `Pool::handle` dispatch and the wait pattern (744-809)

| Request | Method | Engine call | Wait | Reply |
|---|---|---|---|---|
| `PoolStatus` | `status` 812 | none | none | `Pool(report)` |
| `PoolSync` | `sync` 824 | `request_cp` | `sync_now` | `Committed{cp_seq}` |
| `PoolClean` | `clean` 830 | `request_clean` | `wait_quiet` + `wait_cp_outcome` | `Committed` |
| `VolumeCreate` | 866 | `create_named_volume` | `wait_for VolumeCreated{vol}` then `ensure_device` | `Volume` |
| `VolumeList` | inline 750 | `volumes()` | none | `Volumes{..}` |
| `VolumeShow` | inline 758 | `vol_index` | none | `Volume` |
| `VolumeDelete` | 881 | `delete_volume` | `wait_for VolumeDeleted{vol}` | `Ok` |
| `VolumeOnline` | 895 | `set_volume_online` | `sync_now` (+ device up/down) | `Volume` |
| `VolumeResize` | 915 | `resize_volume` | `wait_for VolumeResized{vol}` (+ device recreate) | `Volume` |
| `VolumeRename` | 937 | `set_volume_name` | `sync_now` | `Volume` |
| `VolumeNvfailClear` | 947 | `nvfail_clear` | `wait_for NvfailCleared{vol}`; no-op when not held | `Volume` |
| `SnapshotCreate` | 963 | `create_named_snapshot(vol, name, Crash, User)` | `wait_for SnapshotCreated{vol}` or `SnapshotFailed{uuid}` in a loop until the uuid is listed | `Snapshot` |
| `SnapshotList` | inline 768 | `snapshots()` | none | `Snapshots{..}` |
| `SnapshotDelete` | 999 | `delete_snapshot` | `wait_for SnapshotDeleted{snap_id}` | `Ok` |
| `SnapshotRollback` | 1019 | checks, `delete_snapshot` x n, offline, `rollback_volume`, online | `wait_for` all `SnapshotDeleted`, `sync_now`, `wait_quiet`, `wait_for VolumeRolledBack{vol}`, `sync_now` | `Volume` |
| `SnapshotPin` | 1118 | `set_snapshot_pins` (keeps `PIN_REPLICATION`) | `wait_for SnapshotPinned{snap_id}` | `Snapshot` |
| `SnapshotReplicationBase` | 1149 | `set_replication_base` | `sync_now` | `Snapshot` or `Ok` |
| `IoWrite/IoRead/IoPunch` | 1225/1237/1245 | `io(RequestKind::..)` | `io` loop | `Io(report)` |
| `Ping`, `PoolFormat`, `PoolBoot`, `PoolStop`, `DaemonStop` | refused 803-807 | | | `BadRequest` |

The pattern to copy for a new request that needs a CP: resolve names (`vol_index`/`snap_index`) -> call the engine (`map_err(engine_error)`) -> `wait_for` an event matched by identity (or `sync_now` when the operation has no event) -> report from engine state. For a request without a CP: compute and reply. For a daemon-level request (no pool needed): an arm in `Daemon::handle` before `other =>` (daemon.rs:208-211) **and** an arm in `Pool::handle`'s refusal list (803-807) to keep the match exhaustive.

### 4.8 `status()`, `PoolReport` and the recent-events log

`status()` (812-821) -> `pool_report(&engine, &device, arena_bytes, &arena_word, cp_interval_secs, &last_boot)` (report.rs:61-104). Sources: `pool_status()` for `cp_seq, fold_cp, serving, free_segments, segments, reserve_segments, utilization, admissible_blocks*4096, unflushed_blocks*4096, arena_occupancy, pool_full, flush_policy ({:?}), flush_opaque, discard ({:?}), warnings (to_string), volumes, snapshots, cleaned_segments, relocated_blocks`; `stats()` for `cps, writes, reads`; `layout()` for `segment_bytes, stripe_bytes`; `config().agg_id`; the daemon's `device`, `arena_bytes`, `arena` (`heap` | `hugetlb:<path>`), `cp_interval_secs`, `last_boot`. `otapctl pool status` prints `last boot:    {last_boot}` (render.rs:208) and `arena:        {arena}, {size}, {occupancy}` (180-183); the smoke test greps both.

The recent-events log: `Pool.log` (64 lines, `LOG_LINES`), fed by `note` (every noteworthy event) and by direct `log` calls (geometry warnings, device up/down); read by `Pool::recent_events()` (317-319). **Not in the protocol**: no `Request` returns it; every line is also `eprintln!`ed as `dbladed: {line}` (323). The smoke script reads those lines from the daemon's log file (`stopped cleanly` at line 239 of the script).

### 4.9 Shutdown paths

- `Request::PoolStop` (daemon.rs:196-199): `pool.take()` then `stop()`; the daemon keeps running; the reply is `Committed{cp_seq}` or the `clean == false` error.
- `Request::DaemonStop` (200-207): the same, then `exit = true`; the reply is `Ok`. The engine thread exits at the top of the next loop (`shutdown` finds no pool).
- Signal: `stop` set -> `run` sets `exit` -> `shutdown()`; a request in flight is interrupted first (`interrupted_error`), except while the shutdown's own waits run.
- Handle loss: `Disconnected` on `rx` -> `exit`.
- `Daemon::spawn` boot failure: the thread ends without running `run`; `stopped` is set (253).

### 4.10 The server

`serve` (224-248): listener non-blocking; every 20 ms check `stop` and `handle.is_stopped()`; a connection -> a new thread (no thread limit); `connection` (253-301) reads one frame, calls `handle.call` (blocks on the engine thread for as long as the request takes; no timeout on the server side), writes the reply. The 300 s read timeout only triggers when the client is silent *between* requests. Loopback-only unless `allow_remote` (93-105). Unix sockets: stale takeover / live refusal (182-199), mode 0660 (214), removed on `Drop` (172-179).

### 4.11 Test harness flows

See section 6.

## 5. Hook points for Step 6

### 5.1 Scope A: `USER_RECOVERY` re-attach and the stale-device reaper

**What the daemon knows after `kill -9`:** only what its command line says. The pool device path and the arena path come from `--device`/`--arena-file` (or from a later `otapctl pool boot` request, whose `arena: ArenaKind` carries the path, protocol.rs:81-82). The arena re-attaches by path (section 2.7). The volumes, their names, sizes and offline flags come from the pool (`engine.volumes()`, pool.rs:281-287). **Nothing records which ublk `dev_id` served which volume**: `Pool.devices` is in-memory (pool.rs:141), `UblkDevice::create` lets the driver choose the id (`ANY_DEV`, device.rs:94) and leaves `ublksrv_flags` zero (device.rs:90-97). The driver keeps a killed daemon's devices in `UBLK_S_DEV_QUIESCED` (ublk.rs:322-324) with the dead `ublksrv_pid` in their info.

**Where recovery plugs in:** `Pool::boot` between the boot-commit wait (280) and the `ensure_device` loop (288-293). The smallest coherent change:

1. A `recover_devices(&mut self) -> Result<(), ControlError>` run after `wait_until(is_serving)`: enumerate candidate devices (new otap-sys/otap-ublk API: there is no `LIST` command in ublk; the kernel's enumeration is `/sys/class/ublk-char/ublkc*` or probing `GET_DEV_INFO` over ids), keep those with `state == QUIESCED`, `flags & UBLK_F_USER_RECOVERY`, a dead `ublksrv_pid`, and an identity that names this pool and a volume. For each: a new `UblkDevice::attach(dev_id, &DeviceSpec)` (GET_DEV_INFO -> `START_USER_RECOVERY` -> `UblkChar::open` + `UblkQueue::new` + a fetch on every tag -> `END_USER_RECOVERY(pid)`: ublk.rs:804-819 says END completes once every tag has a fetch pending, i.e. the same `open_queue` sequence as device.rs:166-177), then `front_door(&mut device, vol, &spec)` and `self.engine.front_door().add_device(vol, device)`, `self.devices.insert(vol, path)`. `ensure_device` then skips those volumes (`contains_key`), creating fresh devices only for volumes without a survivor. Everything stays on the engine thread, which the queue-pinning rule requires (device.rs:8-11, E28).
2. The identity needs a decision (section 8, Q1): the cheapest host-local tag is `ublksrv_flags` (a `u64` the driver stores and ignores) set at `ADD_DEV`, e.g. a 32-bit hash of `agg_id` and the volume index, paired with the `DeviceSpec` re-derived from `engine.volume(vol).size_blocks` and `GET_DEV_INFO`'s `queue_depth`/`max_io_buf_bytes` (the device must keep the depth and size it was created with; a resize recreates the device, so sizes agree at crash time).
3. The `DeviceSpec` for a re-attached device must equal the one at creation for `UblkFrontDoor::new(.., size_blocks, depth, Watermarks::for_depth(depth))` (otap-ublk frontdoor.rs:247-282) to decode requests correctly; `for_arena` depends on `arena_slots`, which is the same if the arena size is the same (it must be, or `map_file` refuses the longer file).
4. In-flight tags at the crash: the device was created **without** `UBLK_F_USER_RECOVERY_REISSUE` (device.rs:95). What the 6.12 driver does with requests the dead server had fetched and not committed (fail them `EIO` to the block layer, or requeue them for the new server) decides whether the new daemon can see a write twice; it must be checked against the saved `ublk_drv.c` (E28 lists the facts that were checked; this one was not). Either way the arena replay (`Arena::attach`) already happened in `PoolEngine::boot` before any device is touched, so an acknowledged write is never lost by the re-attach itself; a duplicate redelivery of a write already placed is harmless (same data, newer sequence).
5. The 2-second budget runs from exec to `END_USER_RECOVERY`: today's boot is synchronous inside `Daemon::spawn` and the boot CP must commit first (`wait_until(is_serving)`); the arena replay and the boot CP are the cost; the control socket is bound before the boot (dbladed.rs:162) so `otapctl` connects queue in the backlog.

**Where the reaper plugs in:** a request that must work with **no pool booted** (a dead daemon's devices are exactly the case where the new daemon may not have booted yet, or boots another pool): an arm in `Daemon::handle` before `other =>` (daemon.rs:208-211), like `Ping` (129-133), plus the refusal arm in `Pool::handle` (803-807) to keep the match exhaustive. `DeviceList` enumerates and reports `{dev_id, path, state, ublksrv_pid, alive, flags, tag/identity, owned_by_this_daemon}`; `DeviceReap{dev_id: Option<u32>, all_dead: bool}` deletes (`STOP_DEV`+`DEL_DEV` through a fresh `UblkControlRing`) devices whose server pid is dead and which this daemon does not serve (`pool.devices` values / `front_door().device_volumes()`); never a device the running pool serves. On non-Linux both answer `ErrorCode::Unsupported` (the `device` module does not exist off Linux, lib.rs:42-43) or an empty list; the lead decides (Q2). Protocol: `Request::DeviceList`, `Request::DeviceReap{..}`, `Response::Devices{devices: Vec<DeviceReport>}` (a struct variant, not a newtype over a `Vec`), `DeviceReport` with `deny_unknown_fields` and no field named `kind`; `otapctl` adds a `"device"` group in args.rs (the dispatch at 244-442 and the group list in the wildcard arm at 442) and a renderer in render.rs; the protocol round-trip test (protocol.rs:464) and `review_control_tests.rs` get the new variants.

**Must not break:** the queue-pinning rule (recovery on the engine thread only); `drop_device`'s drain-then-delete order at stop (714-739); `ensure_device` idempotence; the boot's `let _ = pool.stop()` on a device failure (289-292); the arena `flock` (a `device list` with no pool must not open the arena); the smoke script's expectations after `SIGKILL` (scripts/linux-smoke.sh 251-273: `pool status` contains `arena re-attached`, `volume show` says `state:      online`, `.device` from `volume show --json` is a block device; the "note:" lines 256-258 describing the old behaviour are to be removed when A lands).

### 5.2 Scope B: a long verification on the engine thread

**Where:** `Request::PoolVerify{..}` -> `Pool::handle` arm -> `Pool::verify(&mut self, ..) -> Reply`. The engine thread executes it; while it runs, no other request is read (4.2) and the devices are served **only by the ticks the request itself makes**. The only pattern that exists for "do work and keep the devices alive" is the wait loops (4.3): `tick` (or `tick_raw` + `note`), then `sleep(IDLE_TICK)` only when the tick was idle.

**Cadence facts:** each tick serves up to 64 device requests (engine.rs:3360) and advances the CP state machine; the devices' latency promise is "at most 2 ms" between ticks (E38, idle_wait). A verify slice therefore must be bounded in wall time (reads through `Backstore::read` are synchronous, traits.rs:250; on `UringBackstore` a read of a stripe is a blocking round trip) and interleaved with `self.tick()`. `audit_liveness` (engine.rs:2820-2860) is monolithic ("it reads every map page of every tree") and cannot be sliced from the daemon; `snapshot_diff`/`live_diff` return whole vectors. A verify pass therefore needs a **resumable job in otap-cp** (a `Verifier` holding its cursor: ring slot -> volume -> map page -> segment -> stripe; `step(&mut Engine, budget) -> Progress`), and the daemon drives `loop { step; tick; }`.

**Timeouts:** `op_timeout` (120 s default) applies only to the waits; a verify loop must not be written over `wait_for`/`wait_until` or it will time out on a large pool. `interrupted()` must be polled every slice so SIGTERM is not delayed (E39's hazard "a signal interrupts an operation's wait"). The server has no reply timeout, but `otapctl` gives up after `--timeout` (600 s default, args.rs:75) while the daemon keeps running the request: a verify longer than that needs the asynchronous shape (start / poll status / final report), which the daemon has no precedent for. Two shapes for the lead (Q5): (a) synchronous, bounded per slice, with `Response::Verified(VerifyReport)`; (b) a `Pool` field `verify: Option<VerifyJob>` advanced in `Pool::tick` (447-453) with a per-tick budget, started by `PoolVerify{wait: false}` and read by `PoolVerifyStatus`; in (b) the job must make `tick` report `busy` (so `run` ticks again at once rather than sleeping up to 2 ms) and the report must be kept in the `Pool` until read.

**Consistency of the view:** the CP timer fires every 1-15 s regardless; a pass over a big pool spans CPs, folds and cleans. Reading the ring through `boot::read_ring(backstore, layout)` (boot.rs:177, public) and ranking with `boot::evaluate` (257) is safe at any moment; map pages and summaries can move under the cleaner. The pass should record `cp_seq` at start (`engine.cp_seq()`) and either tolerate movement (re-read what changed) or run inside a quiet window by refusing to start while a clean is requested; and the cleaner should not relocate while a verify is pending or the "exact (lseg, off)" checks will report drift that is not damage. `is_quiet()` (3194-3200) is the only signal; there is no "pause the cleaner" API.

**Must not break:** the 2 ms promise; `busy` semantics of `tick_raw` (435-443); the single-request rule (a verify must never recurse into another request); the stderr log volume (`Pool::log` prints every line; a verify must summarise, not log per block).

### 5.3 Scope C: attaching a replication sender and receiver

**What exists for feeding block I/O into the engine from another thread:**
1. `DaemonHandle::call(&Request)` from any thread (daemon.rs:342-350): in-process it carries the `Request` enum (no JSON), but `IoWrite.data_hex` is **hexadecimal even in process** (`decode_hex`, pool.rs:1227-1228) and capped at `MAX_IO_BLOCKS = 256` blocks (1 MiB) per request by `check_blocks` (1215-1222); each call is serialized with every control request and waits for its completion (`io`, 1164-1205); over the wire the 2 MiB hex frame fits the 4 MiB bound. A receiver built on this pays hex encode/decode per MiB and blocks the control plane per request.
2. `IoHandle::submit(kind, data: Option<Vec<u8>>) -> Receiver<IoCompletion>` (frontdoor.rs:53-57): raw bytes, any `RequestKind` (write, read, flush, discard, write-zeroes), bounded only by the engine (`MAX_REQUEST_BLOCKS` 2048 = 8 MiB; `MAX_PUNCH_BLOCKS` 262,144; `TooLarge` beyond the arena's slots) and by `copy_in`'s exact-length rule; completions arrive when the engine thread ticks (which `run` does every <= 2 ms while idle, continuously while busy); the handle is `Clone + Send`. **Today it is unreachable from outside `Pool`** (`io` private, 131; set at 258). The smallest change: `pub fn io_handle(&self) -> IoHandle { self.io.clone() }` on `Pool` and a way to hand it to the replication thread: the `Daemon` lives on the engine thread and holds no handle to itself (`spawn` keeps `tx` outside, 236-242), so either (i) `spawn` gives the thread a clone of `tx` as `self_handle` and a new daemon-level request (`ReplicationStart{..}`) spawns the receiver/sender thread with `io_handle` + `DaemonHandle` clones, or (ii) `DaemonConfig` grows the replication settings and `Daemon::run` starts the threads after a successful boot, or (iii) the `Call` type (daemon.rs:18) is generalised to an internal enum with a closure variant (`Op::Engine(Box<dyn FnOnce(&mut Daemon) + Send>)`) so a replication thread can run typed work on the engine thread without the protocol (E38's "nothing else touches the engine" then holds by discipline, not by type). Lead decision (Q6).
3. Back-pressure on the channel door is **absent**: `mpsc::channel` is unbounded (frontdoor.rs:93), `pull` moves everything into `queue` (123-137), the engine fetches 64 per tick and acknowledges a write when placed; a flooding receiver grows `inflight` without bound while the arena pacing (80%/95%, `paced`) merely delays completions. The ublk door has watermarks (`Watermarks::for_depth`); the channel door needs either a receiver-side cap on outstanding submissions (count receivers not yet answered) or watermarks of its own. Also the channel is polled only when no device has a ready request (4.6): under device load a receiver through the channel is the lowest priority.

**Engine-side reads the sender needs are engine-thread only:** `snapshot_diff(snap_id, base)`, `live_diff(vol, base)`, `snapshot_read(snap_id, lba, dst)` (engine.rs:2916-3007) take `&mut Engine`. A sender thread needs either new requests (`SnapshotDiff{volume, snapshot, base, lba_from, max_entries}` -> paged entries; `SnapshotRead{..}` -> hex, 256-block cap again) or the closure path of (iii). `snapshot_diff` returns the whole diff as a `Vec<(u64, MapEntry)>`: a sender over a large volume must page it (the map's `pinned_diff` callback would allow a cursor; the engine wrapper does not expose one).

**Receiver commit = `sync_now`** (592-596): every write acknowledged before the request rides the CP whose `cp_seq` comes back (the comment at 1090-1091). For a per-delta durability fence that does not need quiet, `Event::CpCommitted.max_seq` >= the highest `IoCompletion.seq` of the delta would do; `wait_cp_outcome` discards `max_seq` today (571-577). The `Landed -> Committed` reply maps onto: chunks placed (completions `Ok`) -> `sync_now` -> `Committed{cp_seq}`.

**`VolEvent` application** maps onto existing methods: `volume_create(name, size_bytes)` (866), `volume_resize` (915), `snapshot_create` (963, hard-codes `SnapKind::Crash, SnapOrigin::User` at 967: a transferred snapshot wants `SnapOrigin::Transfer`, so the method needs an origin parameter or a sibling), `snapshot_replication_base(volume, Some(name))` to advance the base on `Committed` (1149-1160; `set_replication_base` supersedes the previous base's pin, engine.rs:2538-2558), `snapshot_delete` for the superseded base when policy says so. Holes: `RequestKind::Discard` through the handle (or `IoPunch`, capped at 256 blocks on the protocol path).

**Transport placement:** the control server's shape (server.rs: a bound `Server`, `serve` on a thread, one thread per connection, `set_nodelay`, a read timeout, `Endpoint::parse`) is the pattern for the receiver's listener; the loopback-only rule and `--allow-remote-control` are the control plane's policy (unauthenticated), and a replication listener needs its own flag. `otap_cp::Net` (traits.rs:494-501) is the abstraction `otap-stream` was designed to sit on; `LoopbackNet` (otap-sim net.rs) implements it for deterministic tests; a `TcpNet` over `TcpStream` is what loopback TCP needs; the file sink is a third implementation.

**Must not break:** the 64-fetch budget and the 2 ms device latency (a receiver that floods the channel starves nothing on the devices, since devices are polled first, but the channel's own requests then wait); `IoCompletion` semantics (`data` only on `Ok`); the id-space rule: a new front door must not produce ids with bit 63 set and must be claimed in `DaemonFrontDoor::copy_in/copy_out/complete` before the per-volume device fallback (298-329), or a request for a volume that also has a ublk device is routed to the device's queue; `Pool::stop`'s order (devices, then `engine.shutdown()`, then `Stopped`): replication threads must be stopped or their handles dropped before, and `refuse_everything` (engine.rs:3383-3399) will complete anything still queued with `Status::Stopped`.

## 6. Test patterns to copy

- **In-process daemon over a file** (`tests/daemon_tests.rs`): `Scratch::new(name)` makes `%TEMP%/otap-daemon-<pid>-<name>` and removes it on drop (24-45); `config()` = `tcp:127.0.0.1:0`, `ublk: false`, `op_timeout 60 s` (47-54); `format_req(device, force)` = a 128 MiB file (63 segments of 2 MiB, reserve 8, 64 KiB stripes) (60-71); `boot_req(device)` = heap arena of 64 MiB (`ARENA_BYTES`, the minimum), `cp_interval_secs: 1` (73-85); `ok/err/volume/snapshot/pool/io` unwrap a `Daemon::handle` reply (87-127); `write_fill(d, vol, lba, fill, blocks)` and `read(d, vol, lba, blocks)` through `IoWrite`/`IoRead` with `encode_hex`/`decode_hex` (129-149). No thread: `let mut d = Daemon::new(config()); ok(&mut d, &req)`.
- **Dirty restart** (663-721): a scoped `Daemon` formats, boots, creates a volume, writes, and is **dropped without `PoolStop`** ("No stop: the daemon dies with its heap arena", 678); a fresh `Daemon` boots the same file and asserts `last_boot.starts_with("dirty boot")`, the volume `nvfail.is_some()` and `!serving`, reads/writes answer `nvfailed`, `VolumeNvfailClear` brings it back with `rollback_gen == 1`. For Step 6 A on Windows, the equivalent of "the arena survived" is the simulator's `Sim::crash_daemon` (harness.rs:1186-1196: `engine.into_parts()` keeps the `HugeArena` and the next `boot` re-attaches it); the daemon cannot do that today because `Pool::boot` creates its own arena from `ArenaKind` (a heap arena is new every time). A `BootSpec`/`Pool::boot_with_memory(spec, memory: HugeArena, ..)` seam would let a daemon test keep the arena across a simulated kill.
- **Clean restart and the whole lifecycle** (206-660): every request in one test, including rollback/pins/base/resize/rename/delete, `PoolStop` -> `Ping{pool_booted: false}` -> re-boot -> `last_boot.starts_with("clean boot")`.
- **Boot refusals** (724-801), **spawn with a configured device and a signal** (804-852: `Daemon::spawn(cfg, stop)`, `stop.store(true)`, `thread.join()`, `handle.is_stopped()`, a later boot sees a clean stop; a missing device fails `spawn` with `NotFound`).
- **The TCP server** (855-949): `Server::bind(tcp:127.0.0.1:0)`, `server.endpoint().clone()`, `Daemon::spawn(config(), stop)`, `serve` on a thread, `Client::connect(&endpoint, Some(60 s))`, `client.hello()`, two clients, a raw `TcpStream` writing a length-prefixed bad frame and reading a `BadRequest` reply then a `Ping`, `DaemonStop`, wait for `handle.is_stopped()`, join both threads, `Client::connect` must then fail.
- **Review suite** (`review_daemon_tests.rs`): `booted(&scratch) -> (Daemon, device)` (166-172); concurrent clients over `DaemonHandle` from worker threads with interleaved writes, snapshots and syncs, then a clean stop (341-434); dropping every handle stops the pool cleanly and the acknowledged write survives (438-474); malformed frames (480-540: empty, `null`, wrong field types, missing fields, negative lba, unknown op, a length beyond `MAX_FRAME_BYTES`, a cut frame); names at their bounds (546-578); I/O at the edges (585-639).
- **`otapctl` as a process** (`otap-ctl/tests/ctl_tests.rs:14-80`): `Rig::new` spawns the engine and the server in-process on `tcp:127.0.0.1:0` and runs `env!("CARGO_BIN_EXE_otapctl") --control <endpoint> ...` with `Command`, checking exit status/stdout/stderr; the pattern for `otapctl device list/reap` and `pool verify`.
- **The engine behind a ublk queue on any host** (`otap-ublk/tests/engine_tests.rs:1-90`): `Engine<SimBackstore, UblkFrontDoor<MockTransport>, SimClock>` with `UblkFrontDoor::new(MockTransport::new(DEPTH), 0, vol, size_blocks, DEPTH, Watermarks::for_depth(DEPTH))`, `pacing.max_delay_ns = 0` under virtual time; `frontdoor_proptests.rs` models the tag states; this is where a recovery state machine (a queue whose tags are mid-flight at attach, aborts, duplicate redelivery) is testable on Windows. The daemon's `VolumeDevice` is Linux-only (frontdoor.rs:177-183) and `devices` is `BTreeMap<u32, ()>` elsewhere (190-191): a daemon-level recovery test on Windows needs a seam that lets `DaemonFrontDoor` hold a `UblkFrontDoor<MockTransport>` (e.g. a generic or enum device slot).
- **The simulator** (`otap-sim`): `SimFrontDoor` (frontdoor.rs:22-155) is the scripted door with `in_flight` and `take_completions`; `Sim::{boot, tick, tick_until, crash_daemon, crash_power, shutdown_clean, verify_recovery, audit_liveness}` (harness.rs); `LoopbackNet::new(endpoints)` and `.end(EndpointId)` for `Net` (net.rs:28-60).
- **`PROPTEST_CASES`**: workspace `Cargo.toml:51` documents the merge gate; the pattern (step5_sim_tests.rs:1376-1391) is `fn proptest_cases(default: u32) -> u32 { std::env::var("PROPTEST_CASES").ok().and_then(|v| v.parse().ok()).unwrap_or(default) }` used as `cases: proptest_cases(64)` with `failure_persistence: None` and `max_shrink_iters: 512`; E39 (1502-1510) and docs/09 (134-135) record that proptest's `contextualize_config` applies the variable over any explicit `cases` anyway. Keep `failure_persistence: None` (no regression files in the tree; E09).
- Gate etiquette (docs/09 49-58, 123-127): `-j 3`, `--test-threads=6`, one cargo at a time; a worker runs only its own test binaries.

## 7. Risks, pitfalls and traps

1. **Everything mutable is on the engine thread.** `Pool::engine()` is `&`; `front_door()`/`backstore()` are `&mut` on the engine; `IoHandle` is the only `Send` path and it is private. Any Step 6 thread that "talks to the engine" needs one of the seams of 5.3.
2. **Events are consumed once.** `take_events` empties the engine's list (engine.rs:3166); `wait_for` notes and drops everything that is not its match. A new request that waits must match by identity; two operations cannot wait "at the same time" (there is only one request at a time anyway).
3. **One request at a time, `Ping` included.** A rollback, a stop, a verify or a long replication step delays every client (review test 341-434 shows pings interleave only between requests). The server thread blocks in `handle.call` with no timeout; the client's `--timeout` (600 s) is the only bound.
4. **`exit` vs `stop`.** `run` checks `stop` only at the loop top; a wait sees it through `interrupted()` (not while `stopping`). After `DaemonStop` in process (`exit = true`), `handle` still answers (daemon_tests 651-659); only `run` acts on `exit`.
5. **`PoolBoot` ignores the request for `ublk` and `op_timeout`** (daemon.rs:188-189): they come from the daemon's config.
6. **Windows never has devices.** `ensure_device`/`drop_device` are no-ops off Linux; `VolumeReport.device` is always `None` on Windows; `Pool.devices` stays empty; `device_volumes()` is empty. Tests set `ublk: false` anyway.
7. **`volume_delete` does not drop the device** (888); it relies on the engine's "offline first" rule. A new path that deletes an online volume would leak a device.
8. **`tick_raw`'s `busy` spins the thread.** While a CP is in flight (`!is_quiet`) `run` uses `try_recv` with no sleep (100% of a core during every CP, by design). A background job in `tick` that reports busy for minutes will do the same.
9. **50 ms idle wait with no pool** (daemon.rs:326-328): undocumented in E38/06 ("200 us to 2 ms"); a `device list` with no pool booted waits up to 50 ms.
10. **`Pool::boot` on a device failure writes a clean shutdown** (289-292: `let _ = pool.stop()`), so the next boot is clean even though this one failed.
11. **`log()` always prints to stderr** (323): per-block logging in a verify or a replication path will flood the journal.
12. **Protocol serde traps** (E38, docs/08): no newtype variant over a bare `Vec`; no `kind` field in a response payload; `deny_unknown_fields` everywhere; `Response` cannot derive `Eq`. Add every new variant to protocol.rs's round-trip test and to `review_control_tests.rs`.
13. **`PROTOCOL_VERSION = 1` is checked by `Client::hello`**; an old daemon answers an unknown `op` with `BadRequest` (Decode error, server.rs:270-273), not a crash. Whether adding requests bumps the version is undecided (Q8).
14. **Id space.** Channel ids set bit 63; ublk ids use bits 0-47 (gen, queue, tag). A third door (replication staging as its own `FrontDoor`) must own a disjoint range and be claimed in `DaemonFrontDoor` routing before the per-volume fallback (298-329), or its requests for a volume with a ublk device will be copied to/from the ublk queue.
15. **Channel starvation and no back-pressure** (4.6, 5.3): devices first, channel last; the channel is unbounded.
16. **The arena file lock.** `map_file` holds an exclusive non-blocking `flock` (hugetlb.rs:346-355); a second open (another daemon, or a tool that "checks" the arena) fails `WouldBlock` -> `ErrorCode::Io`. `map_file` also refuses a file longer than the requested arena (358-367): restart with the same `--arena-bytes`.
17. **Devices are not dropped on panic or kill**; `UblkDevice` has no `Drop`. A reaper must also handle devices from a daemon that was stopped by `SIGKILL` *after* `drop_device` set the queue offline but before `DEL_DEV`.
18. **`UblkFrontDoor::new` assumes every tag Idle with a fetch pending** (frontdoor.rs:265-271); a re-attached queue must satisfy that before `END_USER_RECOVERY` and must present the same `size_blocks`/`depth`.
19. **`REISSUE` is not set** (device.rs:95); the fate of in-flight requests at the crash is a kernel fact to verify (5.1 step 4).
20. **The smoke script hard-codes rendered strings** (`last boot:    clean boot`, `arena re-attached`, `state:      online`, `stopped cleanly`): changing `note`'s wording or render.rs breaks it.
21. **`IoHandle::submit` swallows send errors**; a receiver must treat `Disconnected` on the completion receiver as "the pool is gone".
22. **Timeouts differ by layer**: `op_timeout` 120 s (daemon default; 60/30 s in tests), server read idle 300 s (between requests only), otapctl reply 600 s, `Client::connect` timeout on every read/write.
23. **Doc/code disagreements found:** otap-ublk lib.rs:34-36 says the re-attach is "dbladed's, Step 5" (it is not done, E38 1419-1424, docs/06 550-556); E38 1365-1367 describes the probe as `A0` then `A1` (superseded by E39 W5; code probes the four slots, backstore.rs:164-195); E38 1322-1323 says frames were "bounded at 64 MiB, 4 MiB since E39" (code: 4 MiB); lib.rs:24-27 says "a hugetlbfs arena is re-attached after a crash" (the arena yes, the devices no); docs/06 section 8 489-493 states the 200 us-2 ms wake without the 50 ms no-pool case.

## 8. Open questions for the lead

1. **Device identity across `kill -9`** (A): tag devices host-locally in `ublksrv_flags` at `ADD_DEV` (zero today) and enumerate through sysfs/`GET_DEV_INFO`, or keep a state file (contradicts E38 "no durable state beyond the command line" and S5-4), or record `dev_id`s in the pool (host-local numbers in a portable format: probably wrong)? What exactly identifies "this pool": `agg_id`?
2. **`device list`/`reap` without a pool** (A): handled in `Daemon::handle` (yes, by the shape of the request); may `reap` delete a device whose serving pid is alive? only quiesced ones? only ones tagged as OpenTAP's? Off Linux: `Unsupported` or an empty list?
3. **In-flight requests at the crash** (A): with `USER_RECOVERY` and without `REISSUE`, does 6.12 fail them with `EIO` or requeue them? Should `REISSUE` be set at `ADD_DEV`, accepting a possible duplicate delivery of an already-placed write?
4. **Reaper trigger** (A): only on request, or also at boot before `ensure_device` (reap what cannot be re-attached) and at `pool stop`?
5. **Verify shape** (B): synchronous with sliced ticking and a bounded total, or a background job advanced in `Pool::tick` with start/status requests? Must it hold the cleaner off (no API exists), and is a report that tolerates concurrent CPs acceptable? What is the report's structure (`Response::Verified(VerifyReport)` with per-check counts and a list of findings)?
6. **Replication placement** (C): separate threads reaching the engine through (i) new requests (hex, 256-block cap, serialized), (ii) an `IoHandle` plus a closure-call variant in the engine channel (typed, no hex; E38's "nothing else touches the engine" kept by discipline), or (iii) sliced jobs inside the engine thread? Where does the receiver's listener policy live (loopback-only flag like `--allow-remote-control`)?
7. **Snapshot origin for transferred snapshots** (C): `SnapOrigin::Transfer` through a parameterised `snapshot_create`; does the target volume get the source's name; who creates it on the first delta (a `VolEvent`)?
8. **Protocol versioning**: does adding `Device*`, `PoolVerify`, `Replication*` requests bump `PROTOCOL_VERSION`?
9. **Channel back-pressure** (C): watermarks in `ChannelFrontDoor` like the ublk door's, or a receiver-side cap?
10. **A per-tick background budget** in `Pool::tick` (B, and a periodic reaper if wanted): acceptable change to the loop's `busy` contract?
11. **`recent_events` over the protocol**: a `PoolEvents` request would let `otapctl` and the tests see the log without stderr; wanted now (the verify and the recovery will both log)?
12. **Acceptance for A on Linux**: is "the same `/dev/ublkbN` path after `SIGKILL`" the criterion the smoke script should assert, with the "note:" lines removed?
