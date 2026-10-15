# Map: the control protocol (`otap-control`) and the CLI (`otap-ctl`), for Step 6

Written 2026-10-14 from the tree at `E:/Coding/OpenTAP` (Step 5 = commit 5114866,
format 2.3, protocol 1). Read-only survey; every claim carries a `file:line`.
Line numbers are those of the files as they stand today.

---

## 1. Scope: what I read

Whole files, every function:

| File | Lines | What it is |
|---|---|---|
| `crates/otap-control/Cargo.toml` | 1-17 | deps: `otap-format`, `serde` (1.0.229, derive), `serde_json` (1.0.154); no dev-deps |
| `crates/otap-control/src/lib.rs` | 1-43 | crate doc, lints, module list, the re-export list |
| `crates/otap-control/src/protocol.rs` | 1-464 | `Request`, `Response`, the four report structs, `ErrorCode`, `ControlError`, `Reply` |
| `crates/otap-control/src/protocol/tests.rs` | 1-277 | the two round-trip tests (the only enforcement of the serde clauses) |
| `crates/otap-control/src/wire.rs` | 1-200 | the 4-byte LE length frame, `MAX_FRAME_BYTES`, `WireError`, hex helpers, 2 tests |
| `crates/otap-control/src/client.rs` | 1-247 | `Endpoint`, `Client`, `ClientError`, 1 test |
| `crates/otap-control/src/units.rs` | 1-183 | arena and CP bounds, `parse_size`, `format_size`, `ArenaKind` helpers, 3 tests |
| `crates/otap-control/tests/review_control_tests.rs` | 1-168 | the Step 5 review's 5 adversarial control tests (R3 ported in place) |
| `crates/otap-ctl/Cargo.toml` | 1-23 | bin `otapctl` = `src/main.rs`; deps `otap-control`, `serde_json`; dev-dep `otap-daemon` |
| `crates/otap-ctl/src/main.rs` | 1-9 | argv -> `otap_ctl::run` -> `process::exit` |
| `crates/otap-ctl/src/lib.rs` | 1-110 | `run()`, `code_word()`, exit codes, re-exports |
| `crates/otap-ctl/src/args.rs` | 1-778 | `USAGE`, `BOOL_FLAGS`, `Command`, `Parsed`, `Flags`, `parse()`, 3 unit tests |
| `crates/otap-ctl/src/render.rs` | 1-365 | `render()`, `render_json()`, `render_pool()`, the table and date helpers, 2 unit tests |
| `crates/otap-ctl/tests/ctl_tests.rs` | 1-316 | the `Rig` (in-process daemon on loopback TCP) and the 2 binary tests |

For the hook points and the control flow I also read, on the daemon side:
`crates/otap-daemon/src/daemon.rs` 1-366 (all), `server.rs` 1-301 (all),
`config.rs` 1-100 (all), `device.rs` 1-56 (all), `report.rs` 1-104 (all),
`lib.rs` 1-59 (all), `bin/dbladed.rs` 1-188 (all), `pool.rs` 1-1251 (all),
`tests/daemon_tests.rs` 1-205 and 855-949, `tests/review_daemon_tests.rs` 342-545;
`crates/otap-cp/src/engine.rs` 867-960 (status structs), 2500-2560
(`set_snapshot_pins`, `set_replication_base`), 2761-2900 (`find_*`, `volumes`,
`audit_liveness`); `crates/otap-stream/src/lib.rs` 1-80 and `frame.rs` 150-760
(the frame vocabulary scope C will surface); `crates/otap-sys/src/ublk.rs`
280-325, 439-472, 724-830 (device-info struct, control commands);
`crates/otap-ublk/src/device.rs` 60-260, `lib.rs` 1-54; `scripts/linux-smoke.sh`
100-230; docs: `07` E38 (1316-1426), E39 control bullets (1511-1529), E09
(205-224), S5-4 (1084-1102), E36's replication-base lines; `08` Step 5 entry
(840-1020, the serde facts at 871-877 and 1005-1008); `06` 3.6 (296-323),
section 7 (427-457), limits (505-556); `09` 50-100, 125-147; `10` the Step 6
brief as received (578-627).

---

## 2. Public API and key internals

### 2.1 `otap-control` crate root (`src/lib.rs`)

- Lints: `#![forbid(unsafe_code)] #![deny(missing_docs)] #![warn(rust_2018_idioms, unreachable_pub)]` (lib.rs:20-22). Every new `pub` item needs a doc comment or the build fails under the gates.
- Modules: `client`, `protocol`, `units`, `wire` (lib.rs:24-27).
- Re-exports (lib.rs:29-39), which is what `otap-ctl` and `otap-daemon` import from:
  - `client::{Client, Endpoint}` (29). **`ClientError` is not re-exported**; `otap-ctl` imports it as `otap_control::client::ClientError` (otap-ctl lib.rs:23).
  - `protocol::{ArenaKind, ControlError, ErrorCode, IoResultReport, MediaKind, PoolReport, Reply, Request, Response, SnapshotReport, VolumeReport, MAX_IO_BLOCKS, PROTOCOL_VERSION}` (30-33). **Every new report struct must be appended here** or the CLI cannot name it without the `protocol::` path.
  - `units::{check_arena_bytes, check_cp_interval, format_size, parse_size, ARENA_PAGE_BYTES, DEFAULT_ARENA_BYTES, DEFAULT_CP_INTERVAL_SECS, DEFAULT_HUGETLB_PATH, MAX_ARENA_BYTES, MAX_CP_INTERVAL_SECS, MIN_ARENA_BYTES, MIN_CP_INTERVAL_SECS}` (34-38).
  - `wire::{decode_hex, encode_hex, read_frame, write_frame, WireError, MAX_FRAME_BYTES}` (39).
  - `otap_format::{BLOCK_SIZE, PIN_CLONE, PIN_MASK, PIN_REPLICATION, PIN_SCHEDULE, PIN_USER}` (43): "the protocol speaks in these and a client must not redefine them" (41-42). `BLOCK_SIZE: usize = 4096` (otap-format constants.rs:27); `PIN_USER = 1<<0`, `PIN_SCHEDULE = 1<<1`, `PIN_REPLICATION = 1<<2`, `PIN_CLONE = 1<<3`, `PIN_MASK` = all four (records.rs:344-352).

### 2.2 `protocol.rs`: the messages

Constants:
- `pub const PROTOCOL_VERSION: u32 = 1;` (protocol.rs:9) "carried by `Ping` and checked by the client".
- `pub const MAX_IO_BLOCKS: u32 = 256;` (12) the most blocks one `IoWrite`/`IoRead` moves (1 MiB).

Small enums:
- `MediaKind` (15-22): `#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)] #[serde(rename_all = "snake_case")]`, variants `Hdd`, `Flash` (JSON `"hdd"`, `"flash"`). No `deny_unknown_fields` (unit variants only).
- `ArenaKind` (25-37): `#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)] #[serde(rename_all = "snake_case", deny_unknown_fields)]`; **externally tagged** (no `tag`): `Hugetlb { path: String }` serializes as `{"hugetlb":{"path":"..."}}`, `Heap` as the bare string `"heap"` (asserted at protocol/tests.rs:276 `json["arena"] == "heap"`). Shows that a nested enum field inside an internally tagged variant is fine.

**`Request`** (39-218):
```rust
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case", deny_unknown_fields)]
pub enum Request { ... }
```
Every variant, its fields, and its `op` tag (the tag is the variant name in snake_case):

| Variant (line) | Fields | `op` |
|---|---|---|
| `Ping` (44) | — | `ping` |
| `PoolFormat` (46-64) | `device: String`, `size_bytes: Option<u64>`, `media: Option<MediaKind>`, `stripe_bytes: Option<u32>`, `segment_bytes: Option<u64>`, `flush_opaque: Option<bool>`, `strict_flush: bool`, `force: bool` | `pool_format` |
| `PoolBoot` (66-85) | `device: String`, `media: Option<MediaKind>`, `stripe_bytes: Option<u32>`, `segment_bytes: Option<u64>`, `flush_opaque: Option<bool>`, `strict_flush: bool`, `arena_bytes: u64`, `arena: ArenaKind`, `cp_interval_secs: u32` | `pool_boot` |
| `PoolStatus` (87) | — | `pool_status` |
| `PoolSync` (89) | — | `pool_sync` |
| `PoolClean` (91) | — | `pool_clean` |
| `PoolStop` (93) | — | `pool_stop` |
| `DaemonStop` (96) | — | `daemon_stop` |
| `VolumeCreate` (99-104) | `name: String`, `size_bytes: u64` | `volume_create` |
| `VolumeList` (106) | — | `volume_list` |
| `VolumeShow` (108-111) | `name: String` | `volume_show` |
| `VolumeDelete` (113-116) | `name` | `volume_delete` |
| `VolumeOnline` (118-123) | `name`, `online: bool` | `volume_online` |
| `VolumeResize` (125-130) | `name`, `size_bytes: u64` | `volume_resize` |
| `VolumeRename` (132-137) | `name`, `new_name: String` | `volume_rename` |
| `VolumeNvfailClear` (139-142) | `name` | `volume_nvfail_clear` |
| `SnapshotCreate` (144-149) | `volume: String`, `name: String` | `snapshot_create` |
| `SnapshotList` (151-154) | `volume: Option<String>` | `snapshot_list` |
| `SnapshotDelete` (156-161) | `volume`, `name` | `snapshot_delete` |
| `SnapshotRollback` (165-172) | `volume`, `name`, `delete_newer: bool` | `snapshot_rollback` |
| `SnapshotPin` (174-181) | `volume`, `name`, `flags: u8` | `snapshot_pin` |
| `SnapshotReplicationBase` (184-189) | `volume`, `name: Option<String>` (`None` releases the base) | `snapshot_replication_base` |
| `IoWrite` (192-199) | `volume`, `lba: u64`, `data_hex: String` (`blocks * 4096` bytes as hex) | `io_write` |
| `IoRead` (201-208) | `volume`, `lba: u64`, `blocks: u32` (at most `MAX_IO_BLOCKS`) | `io_read` |
| `IoPunch` (210-217) | `volume`, `lba: u64`, `blocks: u32` | `io_punch` |

`Request` derives `Eq` (40); every variant's fields are `Eq` today. A new variant with an `f64` would strip `Eq` from the whole enum and break `assert_eq!` users only through `PartialEq` (fine) but `Command` derives `Eq` (args.rs:92) and holds a `Request`: **a non-`Eq` `Request` breaks `otap-ctl`'s `Command`/`Parsed` derives**.

Report structs (all `#[serde(deny_unknown_fields)]`, all `Debug, Clone, PartialEq, Serialize, Deserialize`):
- `VolumeReport` (221-244, also `Eq`): `index: u32`, `name: Option<String>`, `size_bytes: u64`, `serving: bool`, `offline: bool`, `nvfail: Option<String>`, `rollback_gen: u32`, `op_pending: Option<String>`, `device: Option<String>` (`/dev/ublkbN` on Linux), `snapshots: usize`.
- `SnapshotReport` (247-274, `Eq`): `id: u64`, `uuid: String`, `name: Option<String>`, `volume_index: u32`, `volume: Option<String>`, `cp_seq: u64`, `created_wall_ns: u64`, `consistency: String` (`crash`|`fsfreeze`; **renamed from `kind` because of the tag collision**, 264-265), `origin: String` (`user`|`scheduled`|`transfer`|`divergent`), `flags: u8`, `damaged: bool`, `deleting: bool`.
- `PoolReport` (277-340, **`PartialEq` only**, two `f64`s): `device`, `agg_id`, `cp_seq`, `fold_cp`, `serving`, `free_segments: u32`, `segments: u32`, `reserve_segments: u32`, `segment_bytes: u64`, `stripe_bytes: u32`, `utilization: f64`, `admissible_bytes: u64`, `unflushed_bytes: u64`, `arena_occupancy: f64`, `arena_bytes: u64`, `arena: String`, `cp_interval_secs: u32`, `pool_full: bool`, `flush_policy: String`, `flush_opaque: bool`, `discard: String`, `warnings: Vec<String>`, `volumes: usize`, `snapshots: usize`, `cleaned_segments: u64`, `relocated_blocks: u64`, `cps: u64`, `writes: u64`, `reads: u64`, `last_boot: String`.
- `IoResultReport` (343-352, `Eq`): `status: String` (`ok`, `nvfailed`, ... from `status_word`, pool.rs:104-118), `seq: u64`, `data_hex: String`.

**`Response`** (354-405):
```rust
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum Response { ... }
```
| Variant (line) | Shape | `kind` |
|---|---|---|
| `Ok` (359) | unit | `ok` (`{"kind":"ok"}`, asserted tests.rs:142) |
| `Pong { protocol: u32, version: String, pool_booted: bool }` (361-368) | struct | `pong` |
| `Formatted { agg_id: String, stripe_bytes: u32, segment_bytes: u64, capacity_bytes: u64, segments: u32 }` (370-381) | struct | `formatted` |
| `Pool(PoolReport)` (383) | newtype over a struct: the report's fields are **flattened next to `kind`** (`json["segments"] == 63`, tests.rs:140) | `pool` |
| `Volumes { volumes: Vec<VolumeReport> }` (385-388) | struct variant **because a newtype over a `Vec` cannot serialize** | `volumes` |
| `Volume(VolumeReport)` (390) | newtype | `volume` |
| `Snapshots { snapshots: Vec<SnapshotReport> }` (392-395) | struct | `snapshots` |
| `Snapshot(SnapshotReport)` (397) | newtype | `snapshot` |
| `Io(IoResultReport)` (399) | newtype | `io` |
| `Committed { cp_seq: u64 }` (401-404) | struct (a newtype over a bare `u64` would not serialize either) | `committed` |

`Response` has no `Eq` (355) because `PoolReport` has `f64`s.

**`ErrorCode`** (408-429): `#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)] #[serde(rename_all = "snake_case")]`: `NoPool` ("No pool is booted"), `PoolBooted` ("A pool is booted already (format, boot)"), `NotFound`, `BadRequest` ("malformed or out of bounds"), `Engine` ("The engine refused (its message says why)"), `Timeout`, `Io`, `Unsupported` ("Not available on this host or build"), `Protocol` ("The daemon and the client disagree on the protocol"). JSON `no_pool`, `pool_booted`, `not_found`, `bad_request`, `engine`, `timeout`, `io`, `unsupported`, `protocol` (asserted `bad_request` at tests.rs:144).

**`ControlError`** (432-458): `#[serde(deny_unknown_fields)] pub struct ControlError { pub code: ErrorCode, pub message: String }`; `ControlError::new(code, impl Into<String>)` (444); `Display` = `"{:?}: {}"` of code and message, e.g. `NoPool: no pool is booted` (453-455); `impl std::error::Error` (458).

**`Reply`** (461): `pub type Reply = Result<Response, ControlError>;` — on the wire this is serde's **externally tagged `Result`**: `{"Ok":{"kind":"ok"}}` / `{"Err":{"code":"no_pool","message":"..."}}` (see review test at review_control_tests.rs:120 `{"Ok":{"kind":"nonsense"}}`).

`#[cfg(test)] mod tests;` (463-464) is `protocol/tests.rs`.

### 2.3 `wire.rs`: framing

- `pub const MAX_FRAME_BYTES: u32 = 4 << 20;` (wire.rs:14): "an `IoWrite` of `MAX_IO_BLOCKS` blocks as hexadecimal is about 2 MiB and fits with room to spare, and a peer that sends more is broken or hostile. The bound is also what one idle connection can make the daemon allocate, so it stays small." (9-13).
- `pub enum WireError { Io(io::Error), TooLong { bytes: u32 }, Decode(serde_json::Error), Encode(serde_json::Error), Closed }` (18-32); `From<io::Error>` maps `UnexpectedEof` to `Closed` (53-61); `Display` texts at 34-49 (`control stream: {e}`, `a {bytes}-byte frame exceeds the {MAX}-byte bound`, `control message does not decode: {e}`, `control message does not encode: {e}`, `the control stream closed`).
- `pub fn write_frame<T: Serialize, W: Write>(w: &mut W, message: &T) -> Result<(), WireError>` (64-74): `serde_json::to_vec` (Encode on failure), `u32::try_from(len)` else `TooLong{u32::MAX}`, `len > MAX_FRAME_BYTES` -> `TooLong`, then `len.to_le_bytes()`, body, `flush()`.
- `pub fn read_frame<T: DeserializeOwned, R: Read>(r: &mut R) -> Result<Option<T>, WireError>` (78-100): reads the 4 length bytes in a loop (a `read` of 0 at `got == 0` is a clean close -> `Ok(None)`; mid-length -> `Closed`); `len > MAX_FRAME_BYTES` -> `TooLong` **before any allocation** (92-94); `vec![0; len]`, `read_exact` (EOF -> `Closed`), `serde_json::from_slice` -> `Decode`.
- `pub fn encode_hex(&[u8]) -> String` lower-case (104-112); `pub fn decode_hex(&str) -> Option<Vec<u8>>` either case, `None` on odd length or a non-digit (117-135; uses `as_chunks`, stable since 1.88, workspace floor 1.89).

### 2.4 `client.rs`: endpoint and the blocking client

- `pub enum Endpoint { Unix(String), Tcp(SocketAddr) }` (client.rs:13-18), `Debug, Clone, PartialEq, Eq`.
- `Endpoint::default_for_host()` (25-31): `cfg!(target_os = "linux")` -> `Unix("/run/opentap/dbladed.sock")`, else `Tcp(127.0.0.1:7421)`.
- `Endpoint::parse(text: &str) -> Result<Self, String>` (35-57), in order: `unix:<path>` (empty path -> `Err("an empty Unix socket path")`); `tcp:<addr>` (`SocketAddr` parse, else `"{addr:?} is not an address and port: {e}"`); a bare `SocketAddr`; a bare path starting with `/` or `.`; else `Err("{text:?} is neither unix:<path> nor tcp:<address>:<port>")`.
- `Display`: `unix:{path}` / `tcp:{addr}` (60-67); `parse(default.to_string()) == default` is tested (244-245).
- `enum Stream { Tcp(TcpStream), #[cfg(unix)] Unix(UnixStream) }` private (70-74) with `Read`/`Write` forwarding (76-102).
- `pub struct Client { stream, endpoint }` (105-108); `Debug` prints only the endpoint (110-116).
- `pub enum ClientError { Connect(io::Error), Wire(WireError), Daemon(ControlError), Unsupported(String) }` (120-129); `Display` (131-140): `cannot reach dbladed: {e}` / `{wire}` / **the daemon error's message only** / `{s}`.
- `Client::connect(endpoint: &Endpoint, timeout: Option<Duration>) -> Result<Self, ClientError>` (147-179): TCP `connect_timeout` (or `connect` when `None`), then `set_read_timeout(timeout)`, `set_write_timeout(timeout)`, `set_nodelay(true)`; Unix the same on `cfg(unix)`; on `cfg(not(unix))` a `Unix` endpoint is `ClientError::Unsupported("Unix socket {path} is not available on this host; use tcp:<address>:<port>")` (168-173). **The timeout is per read/write, not per call**: a daemon that takes longer than it to answer one request makes the client fail with `Wire(Io(TimedOut))`.
- `Client::endpoint(&self) -> &Endpoint` (183).
- `Client::call(&mut self, request: &Request) -> Result<Response, ClientError>` (189-195): `write_frame`, `read_frame::<Reply>`; `Ok(None)` (a clean close instead of a reply) -> `Wire(WireError::Closed)`; an `Err` reply -> `ClientError::Daemon(e)`.
- `Client::hello(&mut self) -> Result<Response, ClientError>` (198-211): `call(&Request::Ping)`; if the answer is `Pong { protocol, .. }` with `protocol != PROTOCOL_VERSION` -> `Daemon(ControlError::new(Protocol, "the daemon speaks protocol {p}, this client {PROTOCOL_VERSION}"))`; **any non-`Pong` answer passes through unchecked**.

### 2.5 `units.rs`: bounds and sizes shared by both ends

- `DEFAULT_ARENA_BYTES = 8 << 30` (units.rs:7), `MIN_ARENA_BYTES = 64 << 20` (9), `ARENA_PAGE_BYTES = 2 << 20` (11), `MAX_ARENA_BYTES = 256 << 30` (14), `DEFAULT_CP_INTERVAL_SECS = 5` (16), `MIN_CP_INTERVAL_SECS = 1` (18), `MAX_CP_INTERVAL_SECS = 15` (20), `DEFAULT_HUGETLB_PATH = "/run/opentap/arena/dbladed"` (22).
- `ArenaKind::default_for_host()` (28-36): Linux -> `Hugetlb { path: DEFAULT_HUGETLB_PATH }`, else `Heap`. `ArenaKind::describe()` (40-45): `hugetlb:<path>` | `heap` (what `PoolReport.arena` carries, pool.rs:264).
- `check_arena_bytes(u64) -> Result<(), String>` (50-71): below min, above max, not a multiple of 2 MiB, each with its message.
- `check_cp_interval(u32)` (74-81): outside 1..=15.
- `parse_size(&str) -> Result<u64, String>` (85-113): trims, lower-cases, suffixes `tib|gib|mib|kib|t|g|m|k`, digits only (no sign, no fraction), `checked_mul` (`"{text:?} is too large"`).
- `format_size(u64) -> String` (118-136): `64 MiB` when exact, `1.50 GiB` otherwise, `N B` below 1 KiB.

### 2.6 `review_control_tests.rs` (5 tests, the Step 5 review's)

`a_misspelled_optional_field_is_refused` (40-52; R3: `nmae` must not decode as `name: None`), `frames_survive_a_stream_that_splits_every_byte` (56-94; the `Trickle` one-byte reader, 14-29), `hostile_bodies_are_decode_errors` (98-125; empty, `null`, non-UTF-8, 10,000-deep nesting, out-of-range numbers, `"op":"Ping"` wrong case, a reply where a request is expected; and `{"Ok":{"kind":"nonsense"}}` refused as a `Reply`), `the_frame_bound_holds_and_the_largest_write_fits` (130-154; `u32::MAX` length refused at once; the largest legal `IoWrite` round-trips under 4 MiB), `hex_and_sizes_refuse_what_they_cannot_represent` (157-168).

### 2.7 `otap-ctl` crate root (`src/lib.rs`)

- Lints as in `otap-control` (lib.rs:14-16). Modules `args`, `render` (18-19). Re-exports `args::{parse, Command, Parsed, USAGE}`, `render::{render, render_json}` (26-27). `pub const VERSION: &str = env!("CARGO_PKG_VERSION")` (30).
- `pub fn code_word(code: ErrorCode) -> &'static str` (34-46), **exhaustive match** (a new `ErrorCode` variant must get an arm): `NoPool`->`no pool`, `PoolBooted`->`pool booted`, `NotFound`->`not found`, `BadRequest`->`bad request`, `Engine`->`refused`, `Timeout`->`timeout`, `Io`->`i/o error`, `Unsupported`->`unsupported`, `Protocol`->`protocol`.
- `pub fn run(args: &[String], out: &mut dyn Write, err: &mut dyn Write) -> i32` (50-110): see section 4.2 for the order; the exit codes are in the crate doc (10-12): **0 a reply; 1 the daemon refused (message on stderr, or in the JSON); 2 the command line does not parse; 3 the daemon cannot be reached or the stream broke.**

### 2.8 `args.rs`: argv to `Request`

- `pub const USAGE: &str` (args.rs:14-72): the help text; lists every verb; the test `usage_and_connection_errors_have_their_own_exit_codes` asserts `--help` output contains `"snapshot rollback"` (ctl_tests.rs:303). Sizes note, LBA note, the `#<index>` addressing note (64-67), the exit-status block (69-71).
- `const DEFAULT_TIMEOUT: Duration = 600 s` (75).
- `const BOOL_FLAGS: &[&str]` (78-89): `--json --force --heap-arena --flush-opaque --strict-flush --delete-newer --user --schedule --clone --release`. **A flag that takes no value must be listed here**, otherwise the tokenizer consumes the next word as its value (206-212).
- `pub struct Command { pub control: Endpoint, pub json: bool, pub timeout: Duration, pub request: Request, pub read_to: Option<PathBuf> }` (92-104), `Debug, Clone, PartialEq, Eq`. `read_to` is `io read --out` (418), consumed by `render()`.
- `pub enum Parsed { Run(Command), Help, Version }` (107-115).
- `struct Flags { values: BTreeMap<String, Option<String>>, context: String }` (118-122) with: `take(name) -> Option<Option<String>>` (125), `string(name) -> Result<Option<String>, String>` (`Some(None)` -> `"{name} needs a value"`, 129-135), `required(name) -> Result<String, String>` (`"`{context}` needs {name}"`, 137-140), `flag(name) -> bool` (present at all, 142-144), `size(name) -> Result<Option<u64>, String>` via `parse_size` (`"{name}: {e}"`, 146-150), `number<T: FromStr>(name)` (`"{name}: {v:?} is not a number"`, 152-159), `media()` (`hdd` | `flash`/`ssd`/`nvme`, 161-169), `stripe()` (`u32::try_from`, 171-175), `done(self)` (any leftover -> `"`{context}` does not take {name}"`, 177-182).
- `pub fn parse(args: &[String]) -> Result<Parsed, String>` (186-455):
  1. Tokenize (190-219): `-h`/`--help` anywhere -> `Parsed::Help` (even `pool status -h`, test 494); `-V`/`--version` -> `Version`; `--` alone -> `Err("a bare `--`")`; `--name=value` -> `Some(value)` whatever the name; a name in `BOOL_FLAGS` -> `None` (no value); any other `--name` takes the next word (`"{arg} needs a value"` if none); `-x` -> `Err("unknown option")`; bare words are collected.
  2. Globals (220-233): `--control` else `$OTAPCTL_CONTROL` (non-empty) else `Endpoint::default_for_host()`; `--json`; `--timeout` (`u64` seconds, zero refused: `"--timeout needs at least one second"`).
  3. Exactly two words (234-240): `no command given` / `` `{w}` needs a verb `` / `too many words: [...]`. `flags.context = "{group} {verb}"` (241).
  4. The `match (group, verb)` (243-446) builds the `Request`; verbs and flags:
     - `daemon ping` -> `Ping`; `daemon stop` -> `DaemonStop` (244-245).
     - `pool format --device R [--size] [--media] [--stripe] [--segment] [--flush-opaque] [--strict-flush] [--force]` (246-255); note `flush_opaque: flags.flag("--flush-opaque").then_some(true)` (`Some(true)` or `None`, never `Some(false)`).
     - `pool boot --device R [--arena-bytes] [--arena-file P | --heap-arena] [--cp-interval] [--media] [--stripe] [--segment] [--flush-opaque] [--strict-flush]` (256-280): both arena flags -> `"--heap-arena and --arena-file exclude each other"`; neither -> `ArenaKind::default_for_host()`; `arena_bytes` default `DEFAULT_ARENA_BYTES` (8 GiB, **so a Windows test must pass `--arena-bytes 64M`**), `cp_interval_secs` default 5.
     - `pool status|sync|clean|stop` (281-284).
     - `volume create --name R --size R` (285-290), `list`, `show --name`, `delete --name`, `online --name` (`VolumeOnline{online:true}`), `offline --name` (`online:false`), `resize --name --size`, `rename --name --new-name`, `nvfail-clear --name` (291-318).
     - `snapshot create --volume --name` (319-322), `list [--volume]` (323-325), `delete --volume --name` (326-329), `rollback --volume --name [--delete-newer]` (330-334), `pin --volume --name [--user] [--schedule] [--clone]` (no bit given -> `PIN_USER`, 335-354), `unpin --volume --name` (`SnapshotPin{flags:0}`, 355-359), `replication-base --volume (--name | --release)` (exactly one, else `"`snapshot replication-base` takes --name <snapshot> or --release"`, 360-372).
     - `io write --volume --lba (--hex H | --file P | --fill B --blocks N)` (373-414): exactly one source; `--file` is read by the **client** and hex-encoded; the data must be whole 4 KiB blocks, 1..=256 (`check_blocks`, 457-464); `parse_byte` accepts `0xAB` or decimal (467-474).
     - `io read --volume --lba [--blocks N=1] [--out P]` (415-426): `read_to = --out`.
     - `io punch --volume --lba --blocks` (427-441).
     - Catch-alls (442-445): `("daemon" | "pool" | "volume" | "snapshot" | "io", verb) => Err("`{group}` has no verb {verb:?}")`, then `(group, _) => Err("unknown command group {group:?}")`. **A new group must join the alternation** at 442 to get the "no verb" message (test `mistakes_are_named` 758-759 pins both messages).
  5. `flags.done()?` (447) refuses leftover flags; `Ok(Parsed::Run(Command { control, json, timeout, request, read_to }))` (448-454).
- Unit tests (476-777): `args(line)` splits on whitespace (480-482); `request(line)` (484-489); `global_options_and_help` (491-511), `every_verb_parses` (513-750; one `assert_eq!` per verb, the place a new verb's parse is pinned), `mistakes_are_named` (752-777; one `err(...)contains(...)` per refusal).

### 2.9 `render.rs`: `Response` to text and JSON

- `pub fn render_json(response: &Response) -> String` (render.rs:14-16): `serde_json::to_string_pretty(response)` — **the `Response` alone, `kind`-tagged, not wrapped in `Ok`**; `unwrap_or_default()` (an unencodable reply prints an empty line).
- `pub fn render(response: &Response, cmd: &Command, out: &mut dyn Write) -> io::Result<()>` (20-146), **exhaustive over `Response`** (a new variant is a compile error until it gets an arm). Per variant:
  - `Ok` -> `ok` (22).
  - `Pong` -> `dbladed {version}, protocol {protocol}, pool booted: yes|no` (23-31).
  - `Formatted` -> `formatted pool {agg_id}` + `  stripe {}, segment {}, {segments} segments over {capacity}` (32-47).
  - `Pool(r)` -> `render_pool` (48; 148-213): `pool {agg_id}` then aligned `  key:  value` lines: `device`, `cp: N (last fold at M)`, `serving`, `segments: F free of T, reserve R, P% in use[, POOL FULL]`, `geometry: stripe S, segment G`, `admissible: X more before the reserve; Y unflushed`, `arena: {word}, {size}, P% occupied`, `cp interval: N s`, `flush policy: P (flush-opaque: yes|no), discard D`, `volumes: V, snapshots S`, `cleaner: ...`, `counters: C cps, W writes, R reads`, `last boot: ...`, then one `  WARNING:      w` per warning. (ctl_tests greps `serving:      yes`, `arena:        heap, 64 MiB`, `last boot:    clean boot` — the column widths are part of the contract.)
  - `Volumes { volumes }` -> `no volumes` when empty, else a table `NAME INDEX SIZE STATE DEVICE SNAPSHOTS` (49-72).
  - `Volume(v)` -> `volume {name|-}` + `index`, `size`, `state`, `generation`, `device`, `snapshots` lines with a fixed `  state:      ` layout (73-81; tests grep `state:      online`, `size:       8 MiB`).
  - `Snapshots { snapshots }` -> `no snapshots` or a table `VOLUME NAME ID CP CREATED (UTC) KIND ORIGIN PINS STATE` (82-101; the column header says `KIND` for `consistency`).
  - `Snapshot(s)` -> `snapshot {name}` + `volume`, `id`, `uuid`, `cp`, `created: ... UTC`, `kind: {consistency}, origin {origin}`, `pins`, `state` (102-112; tests grep `kind:     crash, origin user`, `pins:     user`).
  - `Io(r)` -> `{status}, seq {seq}` when no data; with data: to `cmd.read_to` (`std::fs::write`, then `{status}, {size} written to {path}`) or a hex dump `{offset:08x}  xx xx ...` of 32 bytes per line (113-143).
  - `Committed { cp_seq }` -> `committed cp {cp_seq}` (144).
- Private helpers (new render code in another module cannot call them unless they become `pub(crate)`): `snapshot_row` (215-229), `volume_state` (231-246: `pending:{op}` > `nvfail:{cause}` > `offline` > `online` > `creating`), `snapshot_state` (248-256: `deleting` > `damaged` > `ok`), `pins(u8)` (258-277: `user,schedule,replication,clone` or `-`), `yes_no` (279-285), `write_table(&[Vec<String>], out)` (288-313: two-space gutters, the last cell unpadded, widths by `chars().count()`).
- `pub fn utc(ns: u64) -> String` (317-335): `YYYY-MM-DD HH:MM:SS` from nanoseconds since the epoch (Hinnant's civil-from-days), reusable.
- Tests: `dates_render` (342-347), `pins_and_tables_render` (350-364; pins the exact table output `"A    BB\nccc  d\n"`).

### 2.10 `ctl_tests.rs`: the binary against an in-process daemon

- `struct Rig { endpoint: String, dir: PathBuf, stop: Arc<AtomicBool>, handle: Option<DaemonHandle>, engine: Option<JoinHandle<()>>, server: Option<JoinHandle<io::Result<()>>> }` (14-21).
- `Rig::new(name)` (24-51): scratch dir `temp_dir()/otap-ctl-{pid}-{name}` (removed and recreated); `DaemonConfig { control: Endpoint::parse("tcp:127.0.0.1:0"), ublk: false, op_timeout: 60 s, ..Default::default() }`; `Server::bind(&cfg.control)` (an ephemeral port; `server.endpoint().to_string()` is the `--control` value); `Daemon::spawn(cfg, stop)` -> `(handle, engine thread)`; a server thread running `serve(server, &handle, &stop)`.
- `Rig::path(file)` (53-55); `Rig::ctl(&[&str]) -> (code, stdout, stderr)` runs `env!("CARGO_BIN_EXE_otapctl") --control <endpoint> <args>` (59-71); `ok` asserts exit 0 and returns stdout (74-78); `refused` asserts exit 1 and returns stderr (81-85); `json` prepends `--json`, asserts 0 and parses stdout as `serde_json::Value` (87-92).
- `Drop` (95-106): sets the stop flag, drops the handle (the engine thread stops the pool cleanly when the last handle goes, daemon.rs:298-301), joins both threads, removes the dir.
- `the_command_line_runs_the_whole_lifecycle` (110-286): ping; `pool status` refused `no pool`; `pool format --device --size 128M` (`63 segments`); a second format refused `already holds`; `pool boot --device --heap-arena --arena-bytes 64M --cp-interval 1`; volume create/list; io write (`--fill 0xab --blocks 2`), `--json io read` (`kind == io`, `data_hex`), `io read --out`, `--file` write, hex dump, punch; snapshot create/list/rollback/pin/unpin/replication-base/release/delete; resize refused online, offline, shrink, online, grow; rename, show, not found; `pool clean`/`sync` (`committed cp `), `--json pool status`; offline + delete; `pool stop` (`committed cp `), ping says `pool booted: no`; `--json pool boot` (`cp_interval_secs == 5`, `last_boot` starts `clean boot`); `daemon stop` -> `ok`; waits `handle.is_stopped()` (30 s); then `daemon ping` exits 3.
- `usage_and_connection_errors_have_their_own_exit_codes` (289-316): no `--control` (so `OTAPCTL_CONTROL` or the host default is used only for the failing case); `--help` 0 and contains `snapshot rollback`; `--version` 0 starts `otapctl `; no args 2 `no command`; `volume create --name x` 2 mentions `--size`; `--control tcp:127.0.0.1:1 daemon ping` 3 `cannot reach dbladed`.

---

## 3. Invariants and rules in force

Protocol and wire (code and E38/E39):
1. **One request, one reply, per frame; a frame is a 4-byte little-endian length and that many bytes of JSON** (lib.rs:4-5; wire.rs:1). The bound is 4 MiB (`MAX_FRAME_BYTES`, wire.rs:14; E38 docs/07:1322-1323 "bounded at 64 MiB, 4 MiB since E39"; E39:1518). It is checked **before** the body is allocated on both sides (wire.rs:92-94; review test 130-146).
2. **`Request` is internally tagged by `op`, `Response` by `kind`, both `snake_case`, both `deny_unknown_fields`; every report struct is `deny_unknown_fields`** (protocol.rs:41, 356, 222, 248, 278, 344, 433). The reason is review defect R3 (docs/08:940-942; review_control_tests.rs:37-52): an optional field's absence means something (`name: None` releases the replication base), so a misspelled field must be refused, not read as absent.
3. **The two hidden serde clauses** (docs/08:871-877 and the lesson at 1005-1008; E38 docs/07:1389-1395; the test module doc protocol/tests.rs:1-4):
   - (a) an internally tagged enum **cannot serialize a newtype variant whose payload is a bare sequence** (`Volumes(Vec<_>)`); serde reports it only at serialization time ("cannot serialize tagged newtype variant ... containing a sequence"). Hence `Volumes { volumes }` and `Snapshots { snapshots }` are struct variants (protocol.rs:385-395). The same holds for a newtype over a primitive or a string: a newtype variant's payload must be a map/struct.
   - (b) a report struct used as a newtype payload **cannot have a field named like the tag** (`kind` for `Response`, `op` for `Request`): the snapshot's `kind` became `consistency` (protocol.rs:264-265).
   - Nothing enforces these but the round-trip of every variant: `every_reply_round_trips` (protocol/tests.rs:88-145) and `every_request_round_trips` (147-277). The docs say so in as many words ("a test that round-trips every variant is the only thing that enforces them", docs/08:1007-1008).
4. **The protocol carries no data path** (lib.rs:12-15; E38:1328-1331; docs/06:517-519): the `Io*` requests move whole blocks as hex, at most 256 per request, for the smoke test and the portable harness. Bulk data (a delta, a verify's evidence) must not travel in control frames.
5. **`PROTOCOL_VERSION` is 1; `Ping`/`Pong` carries it and `Client::hello` refuses a mismatch** (protocol.rs:9; client.rs:198-211). A change to the messages that an old client could misread is a version bump.
6. **The format's `BLOCK_SIZE` and `PIN_*` are re-exported, never redefined** (lib.rs:41-43; E38:1326-1327; docs/08:976-978).
7. **`ErrorCode` is what the CLI acts on; the message is for people** (lib.rs:17-18; protocol.rs:407-439). Today the CLI acts on it only by printing `code_word` (otap-ctl lib.rs:101); the exit code is 1 for every daemon error.
8. **The daemon keeps no durable state beyond its command line; every name lives in the pool's catalogue (F16)** (E38:1406-1409; S5-4 docs/07:1093-1095; otap-daemon lib.rs:29-31). A replication relationship or a reaper's memory has nowhere to live unless it is put in the pool or accepted as ephemeral.
9. **One engine thread executes every control request between ticks; requests are answered one at a time; the control plane is not a hot path** (E38:1332-1351; daemon.rs:278-318). A request that waits for a CP must tick the engine itself (`wait_for`, pool.rs:476-520) so the data path keeps flowing (E38:1341-1343).
10. **The control server: loopback TCP only unless `--allow-remote-control`** (server.rs:93-105; E39:1517-1518); a frame that does not decode gets a `BadRequest` reply and the connection goes on; a reply that does not encode gets an `Io` error reply instead of a dropped connection (server.rs:270-293; E38:1385-1390); an idle connection is closed after 300 s (server.rs:22; E39:1518-1519).
11. **Client timeouts**: `otapctl` waits 600 s for a reply by default (args.rs:75; E39:1521-1522); the daemon's `--op-timeout` (120 s default, config.rs:97; 60 s in the test rigs) bounds every wait for a consistency point (pool.rs:481, 528, 1170).
12. **`otapctl` grammar is ONTAP's `<group> <verb> --flags` with full words** (`volume`, `snapshot`, not the brief's `vol`, `snap`; `pool sync`/`pool clean` for the CP trigger and the GC run) (E38:1396-1405; otap-ctl lib.rs:4-8).
13. **Exit status: 0 reply, 1 refused, 2 usage, 3 unreachable** (otap-ctl lib.rs:10-12; USAGE args.rs:69-71; E38:1402-1403; docs/06:323).
14. **Every public item documented** (`deny(missing_docs)` in both crates; docs/09:68-69); `#![forbid(unsafe_code)]` (E38:1328); refusals name the field at fault and its value (docs/09:76).
15. **Test rules** (docs/09:89-95, 146-147; E09 docs/07:209-218): nothing is deleted or weakened to make a suite green; case counts are never trimmed; `PROPTEST_CASES` overrides any explicit `cases` (docs/09:133-135; E39:1505-1510); `prop_assert!` carries a message.
16. **`snapshot pin` never sets `PIN_REPLICATION` by hand**; the replication pin is set only through `SnapshotReplicationBase` (pool.rs:1124-1128; E36 docs/07 "set_replication_base(vol, Some(id)) sets the bit on the new base and clears it on the volume's previous base in the same CP"; engine.rs:2519-2560). "The cursor itself (the replication stream's `Committed`) is Phase B's" (docs/10:530-532) — Step 6 scope C makes it this step's.

Where the docs and the code disagree:
- `crates/otap-ublk/src/lib.rs:34-36` says the `USER_RECOVERY` re-attach "is `dbladed`'s, Step 5"; it was not done in Step 5 (E38:1419-1424, docs/06:550-556) and is Step 6 scope A.
- `docs/10:580-585` promises "the note at the end of this entry says how the missing part [of the Step 6 brief] was read", but the entry ends at line 627 with "(The paste ends here.)" and no note exists. The brief's "Dual Transport Backends" list, the fourth worker (scope D) and anything after are not in the repository; this map's scope D facts come only from the task text.
- `docs/05-architecture-brainstorm-03-record.md:351` planned the file sink as **`otapctl volume dump --since <snap>` to a file and `restore` into a second aggregate**, and `docs/06:450-452` repeats "the `FileSink` behind `dump --since`"; the task text puts `dump`/`restore` under a replication group. The group is a lead decision (section 8).
- E38:1322-1323 phrases the bound as "64 MiB, 4 MiB since E39": the code only ever shows 4 MiB; read it as "4 MiB".

---

## 4. How things happen, in order

### 4.1 A request on the wire (both ends)

1. The CLI builds a `Request` (args.rs:243-446) and `Client::call` writes it with `write_frame` (client.rs:190; wire.rs:64-74): `serde_json::to_vec`, length check, `[len LE][json]`, `flush`.
2. The daemon's connection thread (`server.rs:253-301`) reads it with `read_frame::<Request>` (255): a clean close between frames or `Closed` ends the thread (257); a read timeout/reset ends it quietly (260-269); a `Decode` error becomes `Err(ControlError(BadRequest, "the request does not decode: {e}"))` and the loop goes on (270-273); any other error is logged and ends it (274-277).
3. A decoded request goes to `handle.call(&req)` (256): `DaemonHandle::call` (daemon.rs:342-350) sends `(req.clone(), reply_tx)` over an mpsc channel and blocks on `reply_rx.recv()`; a dead engine thread answers `Io("the daemon has stopped")` / `Io("the daemon stopped before it answered")`.
4. The engine thread's loop (daemon.rs:278-318): if the stop flag or `exit` -> `shutdown()` and return; `tick()`; when busy, `try_recv` a call, else `recv_timeout(idle_wait)` (200 µs doubling to 2 ms with a pool, **50 ms with no pool**, 325-331); a disconnected channel (every handle dropped) sets `exit`; a call is answered by `self.handle(&req)` (314) and sent back.
5. `Daemon::handle` (daemon.rs:127-213): `Ping` -> `Pong { PROTOCOL_VERSION, crate::VERSION, pool.is_some() }`; `PoolFormat` -> refused `PoolBooted("...before formatting")` when a pool is booted, else `Pool::format` (154-160); `PoolBoot` -> refused when booted, else `Pool::boot` then **replies with `pool.status()` (a `Pool` report)** (191-194); `PoolStop` -> `pool.take()` or `NoPool`, `pool.stop()`; `DaemonStop` -> stop the pool if any, `exit = true`, `Ok`; **`other => pool.as_mut().ok_or_else(no_pool)?.handle(other)`** (208-211): every other variant is `NoPool` ("no pool is booted (`otapctl pool boot`)", 41-43) without a pool.
6. `Pool::handle` (pool.rs:744-809) is an **exhaustive match with no wildcard**; its last arm sends `Ping | PoolFormat | PoolBoot | PoolStop | DaemonStop` to `bad_request("not a request for a booted pool")` (803-807).
7. The reply (`Reply`) is written with `write_frame` (server.rs:279); an `Encode` failure writes a fallback `Err(Io, "the reply does not encode: {e}")` (282-293).
8. The client's `read_frame::<Reply>` (client.rs:191-193); `Err` -> `ClientError::Daemon`.

### 4.2 `otapctl` from argv to exit code (`otap-ctl/src/lib.rs:50-110`)

1. `parse(args)` (51): `Help` prints `USAGE`, exit 0 (53-56); `Version` prints `otapctl {VERSION}`, exit 0 (57-60); `Err(message)` prints `otapctl: {message}\nRun \`otapctl --help\` for the commands.` to stderr, **exit 2** (61-67). Note that `io write --file` reads the file during `parse` (args.rs:384-386), so a missing file is exit 2.
2. `Client::connect(&cmd.control, Some(cmd.timeout))` (69): failure prints `otapctl: {e} ({endpoint})`, **exit 3** (70-74).
3. Unless the request is `Ping`, `client.hello()` (76-81): a connect-level or protocol failure prints `otapctl: {e}`, **exit 3** (a `Protocol` mismatch is exit 3, not 1, because `hello`'s error is not matched as `Daemon` here). So every non-ping command costs **two round trips**.
4. `client.call(&cmd.request)` (82):
   - `Ok(response)`: `--json` -> `render_json` to stdout; else `render(&response, &cmd, out)`; a render `io::Error` (for example `--out` not writable) prints `otapctl: {e}` and **exits 1 although the daemon succeeded** (86-89); else **exit 0** (90).
   - `Err(ClientError::Daemon(e))`: `--json` -> stdout `{"error": {"code": "...", "message": "..."}}` (pretty, 93-99); else stderr `otapctl: {code_word}: {message}` (101); **exit 1** (103).
   - any other `Err` (wire, closed, timeout): stderr `otapctl: {e}`, **exit 3** (105-108).
5. `main` calls `std::process::exit(code)` (main.rs:7-8).

### 4.3 `Endpoint::parse` (client.rs:35-57)

`unix:` prefix -> `Unix` (empty refused) -> `tcp:` prefix -> `SocketAddr` -> bare `SocketAddr` -> bare `/...` or `./...` -> `Unix` -> error. `otapctl --control` and `$OTAPCTL_CONTROL` both go through it (args.rs:220-226); `dbladed --control` too (dbladed.rs:82).

### 4.4 The round-trip tests (protocol/tests.rs)

`round_trip<T>(value)` (12-19): `to_string`, `from_str`, `assert_eq!` with the JSON in the message. `every_reply_round_trips` (88-145) builds `Vec<Reply>` with one instance of every `Response` variant (both an empty and a filled `Vec` for the lists, 105-117) and two `Err`s, round-trips each, then asserts the tag placement (`json["kind"] == "volumes"`, `json["volumes"][0]["name"]`, `json["kind"] == "pool"` with `json["segments"] == 63` flattened, `{"kind":"ok"}`, `json["code"] == "bad_request"`). `every_request_round_trips` (147-277) does the same for every `Request` variant (both `SnapshotList { volume: None }` and `Some`, `SnapshotReplicationBase { name: None }`, hugetlb and heap arenas) and asserts `{"op":"volume_list"}` and `json["arena"] == "heap"`.

**To add a variant**: push one literal of it into `replies` (90-127) or `requests` (149-257); for an `Option` whose absence means something, push both `Some` and `None`; for a `Vec`, push empty and non-empty; if the variant is a newtype over a new report struct, add a `serde_json::to_value` assertion that the report's fields sit next to `kind` and that no field is named `kind`. The fixture fns `volume()`, `snapshot()`, `pool_report()` (21-86) are the pattern for a new report fixture.

### 4.5 The pool-level request helpers an implementer will call (pool.rs)

- `vol_index(&self, name) -> Result<u32, ControlError>` (601-614): the catalogue name, or `#<index>` for a nameless volume; `NotFound("no volume named {name:?}")`.
- `snap_index(&self, vol, name) -> Result<u64, ControlError>` (616-626).
- `report_volume(vol) -> Reply` (644-648), `volume_report_for(vol) -> Option<VolumeReport>` (628-642), `snapshot_reports(Option<vol>) -> Vec<SnapshotReport>` (650-657), `report_snapshot(snap_id) -> Reply` (659-673).
- `wait_for(what, pred: FnMut(&Event) -> Option<T>) -> Result<T, ControlError>` (476-520): ticks, notes events, finds the one, fails with `Engine("the engine stopped while waiting for ...")`, `Timeout("{what} did not complete within {N} s")`, or the interrupt error when a signal set the stop flag (467-472); sleeps 200 µs between idle ticks (`IDLE_TICK`, 58).
- `wait_until(what, done: FnMut(&mut PoolEngine) -> bool)` (523-556); `wait_quiet()` (561-566, a full pool is exempt); `wait_cp_outcome(what) -> Result<u64>` (570-587, a `CpFailed` is an `Engine` error naming the fix); `sync_now() -> Result<u64>` (592-596) = quiet + `request_cp` + outcome.
- `io(kind, data) -> Result<IoCompletion>` (1164-1205): the pattern for a long operation that must keep the engine ticking while it waits (`try_recv`, `is_stopped`, deadline, interrupt, `tick`, sleep).
- `engine_error(EngineError) -> ControlError` (82-96): `NoSuchVolume | NoSuchSnapshot | NoSuchName -> NotFound`; `BadSize | NameExists | MetadataTooLarge | Format -> BadRequest`; `Unsupported -> Unsupported`; `Io -> Io`; everything else `Engine`. `bad_request(msg)` (98-100).
- `snapshot_replication_base(volume, Option<name>)` (1149-1160): `vol_index`, `snap_index`, `engine.set_replication_base(vol, snap_id)`, `sync_now()`, reply `Snapshot(report)` or `Ok` for a release. The engine side (engine.rs:2523-2560) sets `PIN_REPLICATION` on the new base and clears it on every other snapshot of the volume in the same CP.
- `Pool::devices: BTreeMap<u32, String>` (141) is the daemon's own device table (volume index -> `/dev/ublkbN`); `device_path(vol)` (311-313); `ensure_device` (678-709) and `drop_device` (714-739) are `cfg(target_os = "linux")` inside; `crate::device::{create_device, destroy_device}` (device.rs:12-56) wrap `otap_ublk::device::UblkDevice`.

---

## 5. Hook points for Step 6

### 5.0 The shared files every scope touches (the conflict surface)

| File | Exact place | Who touches it |
|---|---|---|
| `crates/otap-control/src/protocol.rs` | `Request` enum body, after `IoPunch` (217) and before `}` (218); `Response` enum body, after `Committed` (404) before `}` (405); new report structs anywhere between 352 and 354 (after `IoResultReport`) | A, B, C |
| `crates/otap-control/src/lib.rs` | the `pub use protocol::{...}` list (30-33) | A, B, C |
| `crates/otap-control/src/protocol/tests.rs` | `replies` vec (90-127), `requests` vec (149-257), new fixture fns after `pool_report()` (86) | A, B, C |
| `crates/otap-ctl/src/args.rs` | `USAGE` (27-62, add a block per group), `BOOL_FLAGS` (78-89), the match arms before the catch-all (441), the catch-all alternation (442), `Command` if a field is added (93-104) and its construction (448-454), `every_verb_parses` (514-750), `mistakes_are_named` (753-777) | A, B, C |
| `crates/otap-ctl/src/render.rs` | the `render` match, after `Committed` (144) before `}` (145); helpers; tests | A, B, C |
| `crates/otap-ctl/src/lib.rs` | `code_word` (34-46) **only if** a new `ErrorCode` is added; the crate doc's group list (5) | whoever adds an `ErrorCode` |
| `crates/otap-daemon/src/daemon.rs` | `Daemon::handle` before `other =>` (208) | A (no-pool verbs) |
| `crates/otap-daemon/src/pool.rs` | `Pool::handle` arms before the last arm (803); **the last arm's alternation (803-807) for every variant the daemon handles itself** | A (the last arm), B, C |
| `crates/otap-ctl/tests/ctl_tests.rs` | new `#[test]` fns after 316; the `Rig` is shared | A, B, C |
| `crates/otap-daemon/tests/daemon_tests.rs` | `a_daemon_without_a_pool_answers_ping_and_refuses_the_rest` (185-199) if a no-pool verb is added | A |
| docs `06` 3.6, `07` (a new E-number), `08` | — | lead |

**Smallest-conflict plan**: the lead lands one skeleton commit first that (1) adds every new `Request`/`Response` variant and report struct to `protocol.rs` with their doc comments, (2) extends the re-export list, (3) adds the round-trip entries, (4) adds the `USAGE` blocks, `BOOL_FLAGS` entries and one delegating match arm per group in `args.rs` (`("device", verb) => device::parse(verb, &mut flags)?` etc.), (5) adds one delegating `render` arm per new `Response` variant, (6) adds the daemon arms returning `Err(ControlError::new(ErrorCode::Unsupported, "not built yet"))`. Each worker then owns a new file: `otap-ctl/src/args/device.rs` + `render/device.rs` + `otap-daemon/src/devices.rs` (A); `.../verify.rs` ×3 (B); `.../replication.rs` ×3 (C); and appends its own tests. With that, three people never edit the same lines. (`args.rs` and `render.rs` become `args/mod.rs` and `render/mod.rs`, or keep the file and add `mod device;` siblings under `args/`; either way `write_table`, `yes_no`, `pins`, `volume_state`, `snapshot_state` must become `pub(crate)` for the submodules.)

### 5.1 Scope A: `device list` and `device reap` (must work with no pool booted)

Protocol (`protocol.rs`):
- `Request::DeviceList` (unit; `op: "device_list"`).
- `Request::DeviceReap { dev_id: Option<u32>, all: bool }` (`op: "device_reap"`): one device, or every stale one; a reap with neither is `BadRequest` from `args.rs`, not the daemon. (Whether `dry_run: bool` is wanted is the lead's call.)
- A report struct, suggested:
  ```rust
  #[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
  #[serde(deny_unknown_fields)]
  pub struct DeviceReport {
      pub dev_id: u32,            // N of /dev/ublkbN
      pub path: String,           // "/dev/ublkbN"
      pub state: String,          // "live" | "quiesced" | "dead" (UBLK_S_DEV_*, otap-sys ublk.rs:318-324)
      pub pid: i32,               // UblksrvCtrlDevInfo.ublksrv_pid (ublk.rs:457)
      pub pid_alive: bool,        // the serving process exists
      pub ours: bool,             // this daemon created it (Pool.devices, pool.rs:141)
      pub volume: Option<u32>,    // when ours
      pub volume_name: Option<String>,
      pub queues: u16, pub depth: u16,
      pub flags: Vec<String>,     // feature_name() of each UBLK_F_* bit (ublk.rs:303)
      pub stale: bool,            // what `reap` would take
  }
  ```
  **No field named `kind`** (use `state`). Everything is `Eq`, so `Request`/`Command` keep `Eq`.
- `Response::Devices { devices: Vec<DeviceReport> }` (`kind: "devices"`; **struct variant, not `Devices(Vec<_>)`**) for both `list` and `reap` (reap returns what it reaped, `stale == true`), or a separate `Response::Reaped { reaped: Vec<DeviceReport>, kept: Vec<DeviceReport> }`. One variant is fewer arms everywhere.
- Append to `lib.rs:30-33`: `DeviceReport`.
- Round-trip: `Request::DeviceList`, `Request::DeviceReap { dev_id: Some(3), all: false }`, `Request::DeviceReap { dev_id: None, all: true }`; `Response::Devices { devices: vec![] }` and `vec![device()]` with a `device()` fixture; assert `json["kind"] == "devices"` and `json["devices"][0]["path"]`.

Daemon (`daemon.rs:127-213`): add the arms **before `other =>`** (208):
```rust
Request::DeviceList => crate::devices::list(self.pool.as_ref()),
Request::DeviceReap { dev_id, all } => crate::devices::reap(self.pool.as_ref(), *dev_id, *all),
```
`crate::devices` is a new module; on `cfg(not(target_os = "linux"))` the two functions answer either `Ok(Response::Devices { devices: Vec::new() })` or `Err(Unsupported, "ublk devices exist only on Linux")` — section 8 asks which. The Linux body enumerates with `CtrlCommand::get_dev_info(dev_id, CtrlBuf)` over `UblkControlRing::execute` (otap-sys ublk.rs:789, 1213) for `dev_id` in a bounded range (or `/sys/class/ublk-char`/`/dev/ublkc*`), reads `UblksrvCtrlDevInfo { state, ublksrv_pid, flags, nr_hw_queues, queue_depth, .. }` (ublk.rs:443-472), marks `ours` from `Pool::devices` (`self.pool.map(|p| p.device_path(vol))`, pool.rs:311-313) and `pid_alive` from `kill(pid, 0)`/`/proc/<pid>`; `reap` runs `CtrlCommand::stop_dev` + `del_dev` (ublk.rs:770, 754) on stale ones only, **never on `ours`** and never on a `QUIESCED` device another live pid serves. It runs on the engine thread, as the pool's own device work does (E38:1352-1354), so no locking.
- `Pool::handle`'s last arm (pool.rs:803-807) must list `| Request::DeviceList | Request::DeviceReap { .. }` to stay exhaustive (the arm is unreachable for them, but the match must cover them).
- `a_daemon_without_a_pool_answers_ping_and_refuses_the_rest` (daemon_tests.rs:185-199) lists the requests that are `NoPool` without a pool; `DeviceList` belongs in a positive assertion next to `Ping`, not in that list.

CLI:
- `USAGE`: a `device list` / `device reap (--dev <n> | --all)` block after `daemon` (args.rs:28-29).
- `BOOL_FLAGS`: `--all` (78-89).
- Match arms before 441: `("device", "list") => Request::DeviceList`, `("device", "reap") => { let all = flags.flag("--all"); let dev_id = flags.number::<u32>("--dev")?; if all == dev_id.is_some() { return Err("`device reap` takes --dev <n> or --all".into()) } Request::DeviceReap { dev_id, all } }`; add `"device"` to the alternation at 442.
- `render`: `Response::Devices { devices }` -> `no ublk devices` when empty, else a table `DEV PATH STATE PID VOLUME OWNER STALE` via `write_table`.
- Tests: `every_verb_parses` two lines; `mistakes_are_named` one line (`device reap` without flags); `ctl_tests`: on Windows `device list` prints the no-device line (or is refused `unsupported`, per the decision) **with no pool booted**, which the `Rig` gives for free (it boots nothing until `pool boot`).
- `scripts/linux-smoke.sh` (scope D) will read `--json device list` after a `kill -9`; freeze the JSON field names (`dev_id`, `path`, `state`, `pid`, `stale`) before D writes it.

What it must not break: `daemon ping` on a daemon with no pool (`pool_booted: false`, ctl_tests.rs:114-116) and the `NoPool` refusals of everything else (daemon_tests.rs:185-199); the `Pool::handle` exhaustiveness; the smoke script's `volume show --json | .device` (linux-smoke.sh:157-166).

### 5.2 Scope B: `pool verify` with a structured report

Protocol:
- `Request::PoolVerify { superblock: bool, maps: bool, segments: bool, liveness: bool, max_findings: u32 }` (`op: "pool_verify"`); all four false means all (decided in `args.rs`, not the daemon), or a single `level: String` (`quick`|`full`) — fewer flags, less to validate. **No field named `op`.**
- Report structs (all `deny_unknown_fields`; **no field named `kind`**; `f64` only if truly needed, since it removes `Eq`):
  ```rust
  pub struct VerifyFinding { pub severity: String /* "error"|"warning" */, pub area: String /* "superblock"|"map"|"segment"|"liveness" */, pub subject: String /* "A1", "volume 3 page 17", "pseg 40 stripe 2" */, pub detail: String }
  pub struct VerifySlotReport { pub slot: String /* "A0".."B1" */, pub readable: bool, pub crc_ok: bool, pub cp_seq: u64, pub epoch: u64, pub cut_seq: u64 }
  pub struct VerifyVolumeReport { pub index: u32, pub name: Option<String>, pub pages: u64, pub bad_pages: u64, pub entries: u64, pub pinned_roots: u32, pub bad_roots: u32 }
  pub struct VerifyReport {
      pub device: String, pub agg_id: String, pub cp_seq: u64, pub started_wall_ns: u64, pub elapsed_ms: u64,
      pub slots: Vec<VerifySlotReport>, pub monotonic: bool,
      pub volumes: Vec<VerifyVolumeReport>,
      pub segments_checked: u32, pub stripes_checked: u64, pub blocks_checked: u64, pub summary_crc_failures: u64, pub block_crc_failures: u64,
      pub liveness: Vec<LivenessDriftReport /* { pseg: u64, counted: u64, found: u64 } mirrors otap_cp::LivenessDrift, engine.rs:924-931 */>,
      pub findings: Vec<VerifyFinding>, pub findings_truncated: bool, pub ok: bool,
  }
  ```
- `Response::Verified(VerifyReport)` (`kind: "verified"`; a newtype over a struct is fine; the struct holds the `Vec`s). Append the five structs to `lib.rs:30-33`. Round-trip: a `verify_report()` fixture with one finding and one drift, plus an empty-`Vec`s instance; assert `json["kind"] == "verified"` and `json["findings"][0]["severity"]`.
- **Frame bound**: the reply must fit 4 MiB (wire.rs:14). A per-segment or per-page listing of a large pool will not; the report carries counts plus a capped `findings` list (`max_findings`, `findings_truncated`). A full evidence file, if wanted, is written by the daemon to a path given in the request, as the data-path rule demands (section 3, rule 4).

Daemon: `Pool::handle` arm `Request::PoolVerify { .. } => self.verify(...)` (pool.rs, next to `status()` 812-821). `verify` builds the report from the engine's new attestation API (scope B's otap-cp work). Two constraints from this side: (i) it runs on the engine thread and **no other request is answered while it runs** (E38:1345-1347); (ii) if it does not tick the engine while it walks, the ublk data path stalls for its whole duration (the `io()` loop at pool.rs:1164-1205 is the pattern: do a bounded unit of verify work, `self.tick()?`, repeat; or have the engine API be a resumable iterator). The existing `Engine::audit_liveness(&mut self) -> Result<Vec<LivenessDrift>, EngineError>` (engine.rs:2815-2820) is the liveness half and "reads every map page of every tree" in one call. `elapsed_ms` is `Instant`-based; `started_wall_ns` from `SystemClock::wall_ns()` (pool.rs:68).

CLI:
- `USAGE` under `pool`: `pool verify [--superblock] [--maps] [--segments] [--liveness] [--max-findings <n>]` (or `[--level quick|full]`).
- `BOOL_FLAGS`: the four area flags.
- Arm `("pool", "verify")` before 441; the group already exists in the alternation.
- `render`: `Response::Verified(r)` -> a header `verify pool {agg_id} at cp {cp_seq}: OK|FAILED ({n} errors, {m} warnings) in {elapsed}`, a `slots` line per slot, a volumes table, a segments/blocks summary, a liveness table when non-empty, then a `findings` table `SEVERITY AREA SUBJECT DETAIL` (and `... and N more` when truncated); reuse `write_table`, `utc`, `format_size`.
- Exit code: today a verify that found damage is exit 0 (any `Ok` reply is 0, lib.rs:83-90). Section 8 asks whether a 4th code or `--strict` is wanted; the implementer should not change `run()` on their own.
- Tests: `every_verb_parses`, `mistakes_are_named` (a bad `--max-findings`), `render` unit test for an empty and a one-finding report (`render.rs:337-364` pattern), `ctl_tests`: after the lifecycle's `pool sync`, `--json pool verify` has `ok == true` and `kind == "verified"`; a second test that corrupts a byte of `pool.img` under a known stripe (the daemon is stopped, the file edited, the pool booted again) and expects `ok == false` with a `segment` finding.

What it must not break: the `Pool::handle` exhaustiveness (a new arm, nothing else), `pool status`'s layout (greps in ctl_tests.rs:137-139, 250-253).

### 5.3 Scope C: the replication group (create / show / list / delete / update / dump --since / restore)

Naming: the docs call the object a **relationship** (docs/05 record:111, 201, 1171 `pub struct Relationship { id: RelId, src: VolRef, dst: VolRef, .. }`), the engine calls the pin the **replication base** (`snapshot replication-base`, pool.rs:1149), and the project writes full words for groups (E38:1404-1405). ONTAP's own group is `snapmirror` (`create`, `initialize`, `update`, `show`, `delete`); `dump --since`/`restore` were planned under `volume` (docs/05 record:351; docs/06:450-452). Section 8 asks the lead to fix the group name; the shapes below are group-agnostic.

Protocol:
- A target enum, externally tagged like `ArenaKind` (protocol.rs:25-37): `#[serde(rename_all = "snake_case", deny_unknown_fields)] pub enum ReplicationTarget { Tcp { addr: String }, File { path: String } }` -> JSON `{"tcp":{"addr":"127.0.0.1:7500"}}` / `{"file":{"path":"..."}}`.
- Requests (`op` in parentheses): `ReplicationCreate { name: String, volume: String, target: ReplicationTarget }` (`replication_create`); `ReplicationList` (`replication_list`); `ReplicationShow { name }` (`replication_show`); `ReplicationDelete { name, release_base: bool }` (`replication_delete`); `ReplicationUpdate { name, wait: bool }` (`replication_update`: run one delta now; `wait` blocks until `Committed`); `ReplicationDump { volume: String, since: Option<String>, path: String }` (`replication_dump`: the file sink; `since` a snapshot name, `None` = a baseline); `ReplicationRestore { volume: Option<String>, path: String }` (`replication_restore`). **`path` is interpreted on the daemon's host and written/read by the daemon**, never streamed through control frames (rule 4; 4 MiB bound).
- Reports: `RelationshipReport { name, volume_index: u32, volume: Option<String>, target: ReplicationTarget, state: String /* idle|catch_up|steady|broken (docs/05 record:111-115) */, base_snapshot: Option<String>, base_cp: u64, applied_cp: u64, last_update_wall_ns: u64, lag_cps: u64, bytes_sent: u64, deltas: u64, last_error: Option<String> }` — **`state`, not `kind`**; `DeltaReport { relationship: Option<String>, volume: Option<String>, mode: String /* "steady"|"increment", not `kind` */, base_cp: u64, cp_seq: u64, cp_nonce: u64, chunks: u32, holes: u64, bytes: u64, digest_hex: String, applied_cp_seq: u64, committed: bool, path: Option<String> }` mirroring `DeltaBegin`/`DeltaEnd`/`Committed` (otap-stream frame.rs:268-293, 527-549, 584-598). All `Eq`-able.
- Responses: `Relationship(RelationshipReport)` (`relationship`), `Relationships { relationships: Vec<RelationshipReport> }` (`relationships`; struct variant), `Delta(DeltaReport)` (`delta`). Append the three structs and the enum to `lib.rs:30-33`. Round-trip every request (`since: None` and `Some`, `volume: None` and `Some`, both targets), both responses with empty and filled lists; assert `json["target"]["tcp"]["addr"]` and `json["kind"] == "relationship"`.

Daemon: `Pool::handle` arms delegating to `crate::replication::{create, list, show, delete, update, dump, restore}(self, ..)`. Facts the sender/receiver must respect on this side:
- **One pool per daemon** (`Daemon.pool: Option<Pool>`, daemon.rs:24; `PoolBoot` refuses a second, 173-175). "Commit the delta as a CP on the target pool" over loopback TCP therefore means **two daemons** (two `Rig`s in a test, two processes in the smoke script) and a **second listener for the stream** on the receiver (not the control endpoint; `Server` only speaks control frames). Where that listener is configured (`dbladed --replication-listen`, or a control request) is open (section 8); it must follow the loopback-only rule (server.rs:93-105).
- The sender pins the base through `engine.set_replication_base(vol, Some(id))` (engine.rs:2523-2560; the previous base loses the pin in the same CP) and the transfer snapshot is `SnapOrigin::Transfer` (report.rs:48 already names it); the base advances only on `Committed` (the task text; docs/05 record:109 "when the walk ends, T becomes the new base").
- A delta's walk and reads happen on the engine thread; the loop must `tick()` between units of work (the `io()` loop, pool.rs:1164-1205) and honour `interrupted()` (459-465) and `op_timeout`, or the data path and every other control request stall for the whole transfer. A long `update --wait` also runs into `otapctl`'s 600 s read timeout (exit 3 while the daemon keeps going); a non-waiting `update` that answers at once with `Delta { committed: false }` plus `show` to poll is the shape that fits the serialized control plane.
- Relationship state has no durable home (rule 8): in memory per daemon life (lost at restart; `list` is empty after a restart) or in the pool (a new catalogue record kind under F16, which is a format change and scope B's incremental-tables territory). Open (section 8).

CLI:
- `USAGE`: a `replication` block (or `snapmirror`) listing `create --name --volume (--to tcp:<addr>:<port> | --to-file <path>)`, `list`, `show --name`, `delete --name [--release-base]`, `update --name [--wait]`, `dump --volume [--since <snapshot>] --out <path>`, `restore --file <path> [--volume <name>]`.
- `BOOL_FLAGS`: `--wait`, `--release-base`.
- Arms before 441; the group name joins the alternation at 442. `--to` parses `tcp:<addr>:<port>` (reuse `Endpoint::parse` and refuse `Unix`) and `file:<path>`/`--to-file`.
- `render`: `Relationship` -> `relationship {name}` with `  volume:`, `  target:`, `  state:`, `  base:`, `  applied cp:`, `  last update:` (`utc`), `  lag:` lines; `Relationships` -> `no relationships` or a table `NAME VOLUME TARGET STATE BASE APPLIED LAG`; `Delta` -> `delta cp {cp_seq} from base {base_cp}: {chunks} chunks, {holes} holes, {size}; {committed|landed, not committed}` (+ `written to {path}` for a dump).
- Tests: parse lines; a `ctl_tests` test with **two `Rig`s**: format and boot a pool on each, create a volume on the source, write a pattern, `replication create --to tcp:<receiver stream addr>`, `update --wait`, then `--json io read` on the receiver's volume equals the source's; a file-sink test: `dump --since s1 --out <path>`, `restore --file` into the second rig, compare reads. The `Rig` needs one addition for the receiver's stream endpoint (a config field or a `--json` field of `pool status`/`show`).

What it must not break: `snapshot pin` still refuses `PIN_REPLICATION` (pool.rs:1124-1128; ctl_tests.rs:197-227 pins the pin/unpin/replication-base flow and `snapshot delete` refused while the base is pinned); `snapshot replication-base --release` still releases (the relationship code must not fight the operator's release, or must refuse it while a relationship exists, which is a decision).

### 5.4 Scope D (for completeness)

`scripts/linux-smoke.sh` calls `otapctl` only through `ctl() { "$OTAPCTL" --control "unix:$SOCK" "$@"; }` (linux-smoke.sh:111) and parses `--json volume show` with `jq -r '.device'` or a `sed` fallback (157-166). A `wsl2-prep.sh` and the updated smoke script will add `ctl device list`/`ctl --json device list` after a `kill -9` and `ctl pool verify`; the JSON field names of scopes A and B are its interface.

---

## 6. Test patterns to copy

1. **Round-trip of every variant**: `crates/otap-control/src/protocol/tests.rs` (`round_trip`, 12-19; fixtures 21-86; the two vectors). Add literals, not generated values; the test is a list, deliberately.
2. **Hostile and malformed input at the wire**: `crates/otap-control/tests/review_control_tests.rs` (`frame(body)` builder 31-35, `Trickle` reader 14-29). A new variant with an `Option` whose absence means something should get a misspelled-field case next to R3's (40-52).
3. **argv parsing**: `args.rs` tests (`args(line)`/`request(line)` 480-489; `every_verb_parses` 513-750; `mistakes_are_named` 752-777) — one `assert_eq!` per verb, one `contains` per refusal.
4. **Rendering**: `render.rs` tests (337-364) write into a `Vec<u8>` and compare the exact string; `write_table`'s output is pinned to the byte.
5. **The binary against an in-process daemon**: `ctl_tests.rs` `Rig` (14-107). Two rigs in one test are fine (each gets its own temp dir keyed by name and pid, and its own ephemeral port). The test asserts on substrings of the text output and on `--json` fields; it uses `--heap-arena --arena-bytes 64M --cp-interval 1` on a 128 MiB file (63 segments, 2 MiB each: docs/08:1009-1011 for why 63 not 64).
6. **In-process daemon without the binary**: `daemon_tests.rs` helpers (`config()` 47-54, `daemon()` 56-58, `format_req`/`boot_req` 60-85, `ok`/`err`/`volume`/`snapshot`/`pool`/`io` 87-127, `write_fill`/`read` 129-150) drive `Daemon::handle` directly; `the_server_answers_over_tcp_and_survives_a_bad_frame` (855-949) is the pattern for a raw-socket test (a hand-built frame, `read_frame::<Reply>` on the raw `TcpStream`); `review_daemon_tests.rs` has the concurrent-clients pattern (342-434, three worker threads over one `DaemonHandle`), the dropped-handle clean stop (439-474), and the malformed-frames list (481-540: empty body, `null`, wrong types, missing fields, negative numbers, `{"op":"pool_boot","device":"x"}` short of fields, non-UTF-8 -> all `BadRequest`; a length past the bound closes the connection).
7. **A no-pool verb**: `a_daemon_without_a_pool_answers_ping_and_refuses_the_rest` (daemon_tests.rs:171-203).
8. **PROPTEST_CASES**: no control or CLI test is a property test today. If one is added (for example a round-trip over `proptest`-generated reports), use `proptest!` with proptest's default configuration so `PROPTEST_CASES` applies (`contextualize_config`, docs/07:1505-1510; the comment at `otap-format/tests/adversarial_tests.rs:366` "Proptest's defaults (so `PROPTEST_CASES` still drives the merge gate)"), or read the variable explicitly as the simulator walks do (`crates/otap-sim/tests/step5_sim_tests.rs:1376-1380`, 64 the default); every `prop_assert!` carries a message (E09). The local gate is `PROPTEST_CASES=10000 cargo test --workspace -j 3 -- --test-threads=6` (docs/09:57) and nothing else uses cargo while it runs (docs/09:130-133). `otap-control` has no `proptest` dev-dependency today (Cargo.toml:14-17); adding one is a workspace-level pin already present (`proptest = "1.12.0"`, Cargo.toml:52).
9. **Where the worker's tests go**: a worker owns only the files its brief names and runs only its own test binaries (docs/09:126-127); `cargo test -p otap-control`, `-p otap-ctl`, `-p otap-daemon` are the three binaries here. A reviewer's `bug_*` tests are later ported in place under `review_*_tests.rs` names (docs/09:89-95).

---

## 7. Risks, pitfalls and traps

1. **A newtype variant over a `Vec`, a primitive or a `String` compiles and fails at run time** (serialization error, not deserialization): `Response::Devices(Vec<DeviceReport>)` would pass `cargo check` and `clippy`, and fail only in `every_reply_round_trips` — if the implementer remembered to add it there. Always a struct variant or a newtype over a struct (section 3, rule 3a).
2. **A report field named `kind` (or a request field named `op`) silently breaks decoding of that variant**: the tag is extracted first and the struct then misses its field. Use `state`, `consistency`, `mode`, `severity` (rule 3b).
3. **`deny_unknown_fields` is on every protocol type; keep it on new ones** (R3). It also means **an old client and a new daemon (or the reverse) refuse each other's new fields**; a field added to an existing variant is a wire change for every pair, and the only version signal is `PROTOCOL_VERSION` checked on `Ping`. Adding whole new variants is safe (an old daemon answers an unknown `op` with `BadRequest`, server.rs:270-273; an old client never sends it).
4. **`Option<T>` fields default to `None` when missing** in serde derive even with `deny_unknown_fields`; a field whose absence must be an error should not be an `Option`.
5. **A new boolean flag not in `BOOL_FLAGS` eats the next word** (args.rs:206-212): `otapctl device reap --all` would fail with `--all needs a value` or swallow a following word.
6. **A new group must join the catch-all alternation** at args.rs:442 or `device fly` says `unknown command group "device"` (confusing, and `mistakes_are_named` pins the two messages for the existing groups).
7. **`Flags::done()` refuses leftovers**: every flag a verb accepts must be consumed (`flags.flag`/`string`/`number`) on every path, including the error paths before `done` — otherwise a legal flag is reported as "does not take".
8. **`Command` derives `Eq`** (args.rs:92): a `Request` variant carrying an `f64` breaks the build of `otap-ctl`; keep floats in `Response` reports only (where `PoolReport` already prevents `Eq`).
9. **`render()` is exhaustive** — good — but `render_json` prints an empty line if a reply does not encode (`unwrap_or_default`, render.rs:15); the daemon side guards encoding (server.rs:282-293), the client side does not.
10. **`run()` exits 1 for a render I/O error even when the daemon succeeded** (lib.rs:86-89): a `--out`/`dump` path the client cannot write looks like a refusal. For daemon-written files this does not arise.
11. **Two round trips per command** (`hello()` then the call, lib.rs:76-82): a verb that must work while the daemon is wedged still needs `Ping` answered.
12. **The client's `--timeout` is a per-read socket timeout** (client.rs:155-156): any single request whose reply takes longer than 600 s fails with exit 3 while the daemon continues (and later writes into a closed socket, ending that connection thread quietly). `pool verify` of a large pool and `replication update --wait` must either answer fast, be asynchronous (start + poll), or document a larger `--timeout`.
13. **The daemon's `--op-timeout` (120 s; 60 s in the rigs) bounds each wait loop**, not a whole request; a verify or a transfer that waits for many CPs is bounded per wait, not overall.
14. **Everything runs on the engine thread between ticks** (E38:1335-1337): a handler that does not tick while it works stops the ublk data path and queues every other control request behind it; `IDLE_TICK` sleeps of 200 µs between idle ticks (pool.rs:58) are the pacing.
15. **`Daemon::handle`'s `other =>` arm swallows every new variant into `NoPool`** (daemon.rs:208-211): scope A's verbs must be matched before it; scope B/C's verbs are correctly `NoPool` without a pool.
16. **`Pool::handle` is exhaustive without a wildcard** (pool.rs:745-808): every new `Request` variant is a compile error there until it has an arm (scope A's go in the last arm's alternation).
17. **The 4 MiB frame bound applies to replies too** (`write_frame`, wire.rs:67-69); a reply that exceeds it is `TooLong`, which `connection()` logs and **drops the connection** (server.rs:294-297), so the client sees `the control stream closed` (exit 3) and nothing tells it why. A verify report or a device list must be bounded by construction.
18. **`Client::hello` only checks a `Pong`** (client.rs:200-209); a daemon that answers `Ping` with anything else is accepted silently.
19. **Non-loopback TCP is refused by the server without `--allow-remote-control`** (server.rs:96-105) and the control plane is unauthenticated: a replication stream listener gets the same rule or it becomes an unauthenticated remote write path into a pool.
20. **`pool boot` replies with a `Pool` report, not `Ok`** (daemon.rs:191-194); `pool stop` replies `Committed` (pool.rs:851-852); `snapshot replication-base --release` replies `Ok` while `--name` replies `Snapshot` (pool.rs:1156-1159). Tests grep these shapes (ctl_tests.rs:137-139, 213-222, 259-260); new verbs should follow the convention "an operation on X replies with X's report".
21. **Volume names may be `#<index>`** (pool.rs:601-614, USAGE 66-67): any new verb taking `--volume` resolves through `vol_index`, so it inherits this; a replication relationship keyed by volume *name* breaks on a rename (`volume rename` is a catalogue change, pool.rs:937-944) — key it by index or uuid and report the name.
22. **`snapshot pin` refuses `PIN_REPLICATION`** and the replication base is one per volume (engine.rs:2538-2558 clears it on every other snapshot of the volume): a relationship per volume is the model the engine supports; two relationships on one volume cannot each pin their base today.
23. **The CP timer is 1-15 s and the test rigs use 1 s** (`--cp-interval 1`); a transfer test that expects a CP to carry its delta must `pool sync` or wait for the event, as the lifecycle test does (ctl_tests.rs:246-249).
24. **Windows is the only host today**: `device list`/`reap` (Linux-only semantics) need a deterministic non-Linux answer or the `ctl_tests` cannot exercise the verb; `cfg(target_os = "linux")` code is only type-checked (`cargo clippy --target x86_64-unknown-linux-gnu`, docs/09:53) and `otap_daemon::device` is `cfg(target_os = "linux")` at the module level (lib.rs:42-43).
25. **The format of the `--json` error is `{"error": {...}}` on stdout** (lib.rs:93-99) while the text error goes to stderr; scripts (scope D) that pipe `--json` must check the exit code, not the presence of stdout.
26. **Scripted edits**: never `perl -pi -e 's|..|..|'` with `\|` inside (docs/08:998-1004, docs/09:141-145); `cargo check` after any scripted edit. The `protocol.rs` and `args.rs` edits are exactly the multi-line kind that mangled files in Steps 4 and 5.

---

## 8. Open questions for the lead

1. **Group and verb names for scope C**: `replication` or `snapmirror`? `create|show|list|delete|update` plus `initialize` for the baseline, or does `update` on a relationship without a base do the baseline (docs/05 record:115 "full copy = CatchUp with base = none")? Do `dump --since` and `restore` live under `volume` (docs/05 record:351; docs/06:450-452) or under the replication group (the task text)?
2. **Where does relationship state live?** The daemon holds no durable state (E38:1406-1409, S5-4); either relationships are ephemeral (lost at restart, `list` empty after a reboot, the base pin on the snapshot the only trace) or they are recorded in the pool (a new catalogue record kind under F16 = a format change, format 2.4; and whether the relationship is keyed by volume uuid or index).
3. **Does the receiver need a stream listener in `dbladed`?** One pool per daemon means loopback replication is two daemons; the receiver's stream endpoint is new configuration (`dbladed --replication-listen tcp:127.0.0.1:0`?) and its address must reach the test (`pool status`/`show` field, or the `Rig`). Loopback-only like the control plane?
4. **Non-Linux answer for `device list` / `device reap`**: an empty list and an empty reap (testable on Windows, silent on a host where the question is meaningless), or `ErrorCode::Unsupported` (honest, but then the Windows `ctl_tests` can only assert the refusal)? And does `reap` need `--dry-run`, and may it ever delete a `QUIESCED` device whose pid is dead (the exact device a `USER_RECOVERY` re-attach wants)?
5. **Should `device reap` work with no daemon running at all?** Today `otapctl` always connects first (lib.rs:69-74); a CLI-local reaper (no daemon) is a change to `run()`'s shape. The brief says "otapctl device list/reap" only.
6. **Exit status of `pool verify` with findings**: 0 with the report (scripts read `--json .ok`), a new code (4: "verified, findings"), or `--strict` mapping findings to 1? Any of these touches `run()` (lib.rs:83-90), which no worker should change alone.
7. **Verify and update: synchronous or asynchronous?** The serialized control plane and the 600 s client timeout argue for start-and-poll (`verify` returns a job id; `show`/`status` reports progress), but that is a new pattern for the protocol (no job table exists; the daemon keeps no state). The lead decides before B and C design their request shapes.
8. **`PROTOCOL_VERSION` bump?** New variants alone do not need one (old peers refuse them cleanly); a changed `PoolReport` (for example a `replication` or `verify` summary added to `pool status`) does, because `deny_unknown_fields` makes an old client refuse the new report.
9. **Is a new `ErrorCode` wanted** (for example `Busy` for "a transfer is running", `Damaged` for a verify failure, `Stale` for a device)? Each needs a `code_word` arm (lib.rs:34-46) and a round-trip line; today everything else maps to `Engine`.
10. **File-sink base advance**: with no `Committed` from a file, does `dump` move the replication base to the transfer snapshot on a successful fsync of the file, or leave the base alone (a backup, not a replication)?
11. **The missing tail of the Step 6 brief** (docs/10:580-585 vs 627): the "Dual Transport Backends" list and the fourth worker's text never reached the repository; the task text is the only source for scope D and the file sink. Should the lead paste the remainder into `10` so the implementers can cite it?
12. **Skeleton-first commit**: will the lead land the enum variants, re-exports, round-trip entries, `USAGE`, delegating arms and stub daemon arms in one commit before the three workers start (section 5.0)? Without it, three people edit the same five line ranges.
