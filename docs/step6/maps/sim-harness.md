# Map: the simulator (`crates/otap-sim`) for the Step 6 tests

Read-only survey, 2026-10-14. Repository `E:/Coding/OpenTAP`. Line numbers are
from the files as they stand after the Step 5 commit (`5114866`). Paths below
are relative to `E:/Coding/OpenTAP/`.

## 1. Scope: what I read

In full:

- `crates/otap-sim/src/harness.rs` (1,374 lines), `backstore.rs` (449),
  `frontdoor.rs` (155), `model.rs` (566), `net.rs` (153), `clock.rs` (40),
  `rng.rs` (49), `lib.rs` (44), `Cargo.toml`.
- `crates/otap-sim/tests/step5_sim_tests.rs` (1,409) and
  `review_step5_sim_tests.rs` (1,448): every helper, the scenarios and both
  property walks.
- `crates/otap-ublk/src/lib.rs`, `mock.rs` (225), `frontdoor.rs` lines 1-130
  and 220-532, `device.rs` 335-382; `tests/engine_tests.rs` (432) in full;
  `tests/review_ublk_tests.rs` 365-584.
- `crates/otap-cp/src/traits.rs` (501) and `layout.rs` (327) in full;
  `lib.rs`; `engine.rs` lines 330-1010 (events, errors, statuses, phases),
  1305-1485 (`Stats`, the `Engine` struct), 1561-2000 (`format`, `boot`,
  `assemble`), 2060-2130 (`tick`), 2236-2275, 2518-2565, 2816-2900,
  2912-3420 (`audit_liveness`, `snapshot_read`, `snapshot_diff`, `live_diff`,
  `pool_status`, `peek`, `peek_disk`, `map_entry`, accessors, `into_parts`,
  `serve_front_door`), 3620-3760 (the read path); `boot.rs` 1-260
  (`BootError`, `TornReason`, `BootState`, `evaluate`); `meta.rs` 1-60;
  `segments.rs` 1-50.
- `crates/otap-stream/src/lib.rs` (186), `flow.rs` (210); `frame.rs` 1-130,
  130-330, 394-720; `cursor.rs` 1-120, 260-726; `codec.rs` 40-328.
- `crates/otap-map/src/lib.rs` 1-80 and the public-API index, `diff`
  (1620-1640), `pinned_diff` (1714-1725).
- `crates/otap-format/src/lib.rs` 60-235 (F1-F18), `constants.rs`,
  `map.rs` 40-72 and its API index, `stripe.rs` 114-150 and its API index,
  `records.rs` (`SegEntry`, `CpStripe`), `types.rs` (`RootRef`, `VolBitSet`),
  `superblock.rs` (field list, `SbSlot`, `SuperblockSlot` API),
  `stream.rs` (`HoleRecord`).
- `crates/otap-arena/src/lib.rs` 505-545 (`Reattach`) and the attach/create
  API; `crates/otap-sys/src/hugetlb.rs` API index.
- Partially: `tests/sim_tests.rs` 1-300, 440-684; `step4_sim_tests.rs`
  1-260, 1240-1415; `adversarial_sim_tests.rs` 115-240, 470-530, 836-980,
  1025-1065, 1195-1235; `review_sim_tests.rs` 85-135, 270-292, 570-600,
  995-1095, 1280-1350; `otap-daemon/src/pool.rs` and `frontdoor.rs` headers,
  `tests/daemon_tests.rs` 1-60; `scripts/linux-smoke.sh` 1-60.
- Docs: `docs/07-engineering-decisions.md` E06, E09-E13, E18, E30-E32, E36,
  E39; `docs/06-implementation-status.md` sections 7 and 8;
  `docs/08-engineering-log.md` 930-1000; `docs/09-working-agreements.md`
  40-140; `docs/10-phase-c-step-briefs.md` 578-627 (the Step 6 brief as
  received); `docs/05-architecture-brainstorm-03.md` 7.3 (666-700) and 8.4;
  `docs/reviews/2026-10-14-step5-qa-adversarial.md` 310-340; `BUILDING.md`
  30-70; the workspace `Cargo.toml`.

Not run: nothing was built or executed (the assignment is read-only).

## 2. Public API and key internals

### 2.1 `otap_sim::harness` (`crates/otap-sim/src/harness.rs`)

| Item | Where | Meaning |
|---|---|---|
| `pub const TICK_BUDGET: u32 = 100_000` | 28 | Ticks any harness wait spends before it gives up. |
| `pub struct SimConfig { seed, stripe_bytes, stripes_per_segment, segments, arena_bytes, volumes: Vec<(u32,u64)>, intent: NvfailIntent, cp_timer_ns, inflight_stripes: u16, flush_opaque, flush_policy, cache_protected, discard: DiscardPolicy, fold: FoldPolicy, schedule: Schedule, tear: TearPolicy, reserve_segments: Option<u32>, cleaner: CleanerPolicy, pacing_delay_ns }` | 32-75 | The whole simulation, `Clone + PartialEq`. |
| `SimConfig::small(seed) -> Self` | 83-111 | 32 KiB stripes, 8 stripes/segment, 48 segments (reserve `reserve_for(48) == 8`), 2 MiB arena, volumes `[(0,64),(1,64)]`, `Conservative`, 5 s CP timer, 4 stripes in flight, not flush-opaque, `TrustBarrierWithNvfail`, `cache_protected: true`, `WholeSegment` discards, fold `{log_bytes: 64 KiB, interval_ns: 60 s, max_cps: 5, max_dirty_pages: 8, max_overlay_entries: 1024}`, `Schedule::default()`, `TearPolicy::default()`, `reserve_segments: None`, `CleanerPolicy::default()`, `pacing_delay_ns: 0`. |
| `SimConfig::geometry(&self) -> Geometry` | 115-128 | `front = (2*slot_stripes + 2*intent_stripes) * stripe`, `back = 2*slot_stripes*stripe`, `capacity = front + back + segments * segment_bytes`. With 32 KiB stripes a slot (20,480 B) and an intent slot (4,096 B) are one stripe each: front 128 KiB, back 64 KiB, 12 MiB of segments. |
| `SimConfig::profile(&self) -> MediaProfile` | 132-140 | `MediaProfile::flash(geometry)` with `cp_timer_ns`, `inflight_stripes`, `nvfail_intent`, `flush_policy`, `discard` overridden. `back_to_back_bytes` stays 1 GiB. |
| `SimConfig::engine_config(&self) -> EngineConfig` | 144-171 | `agg_id = Uuid::from_bytes(seed.to_le_bytes().repeat(2))` (the identity is a pure function of the seed), volumes as `VolumeSpec`, `fold`, `cache_pages = 16`, `pacing.max_delay_ns = pacing_delay_ns`, `reserve_segments`, `cleaner`. |
| `pub struct SnapExpect { snap_id, vol, uuid, cp_seq, image: Vec<Expected> }` | 176-187 | A snapshot the harness took and what the model said every block held then. |
| `pub type SimEngine = Engine<SimBackstore, SimFrontDoor, SimClock>` | 190 | The engine type the harness is hard-wired to. |
| `pub enum Stop { Fresh, Power, Daemon, Clean }` | 194-204 | How the previous instance ended; `Daemon` also covers a shutdown that could not write its final CP. |
| `pub struct BootInfo { cp_seq, dirty, reattached, rewound, arena_lost, damaged_slots: u32, nvfail: VolBitSet, torn: Vec<(u64, String)> }` | 208-225 | The `Event::Booted` fields the harness keeps (`replayed_blocks` and `orphaned_blocks` are not copied; read them from `s.events`). |
| `pub struct Verdict { k: u64, lost: VolBitSet, nvfail: VolBitSet, boot: BootInfo }` | 229-239 | What `verify_recovery` established: the prefix bound and the sets. |
| `pub struct Sim { pub cfg, clock, disk: Option<SimBackstore>, front: Option<SimFrontDoor>, memory: Option<HugeArena>, engine: Option<SimEngine>, pub model: Model, next_op, inflight: HashMap<ReqId,u64>, completed: HashMap<ReqId,Completion>, gen_commits, gen_commits_at_stop, held_at_stop: VolBitSet, last_stop, last_boot, pub events: Vec<Event>, pub snaps: Vec<SnapExpect>, deleting: HashSet<u64>, rollbacks: HashMap<u64,(u32,u64)>, next_uuid }` | 242-273 | The harness. While an engine runs, `disk`, `front` and `memory` are `None` (the engine owns them). `cfg.volumes` is mutated at runtime (creates, resizes, deletes, and boot retries). |
| `Sim::new(cfg) -> Result<Sim, String>` | 289-326 | Clock at wall base `1_700_000_000_000_000_000` ns, mono 0; `SimBackstore::new(profile, seed)` plus `set_schedule/set_tear_policy/set_cache_protected` from the config; `Engine::<SimBackstore, SimFrontDoor, SimClock>::format(&mut disk, &clock, &engine_config)`; `HugeArena::anonymous(arena_bytes, TwoMiB)`; `Model::new(&cfg.volumes)`. Nothing booted. |
| `engine(&mut self) -> &mut SimEngine` | 329-331 | Panics with "the engine is running" when none runs. |
| `is_running`, `clock()`, `disk()`, `memory()` | 335-357 | `disk()` returns `engine.backstore()` while running, else the parked device; `memory()` is `Some` only while no engine runs. |
| `gen_commits()`, `last_boot()`, `last_stop()`, `inflight_writes()` | 361-381 | Accessors. |
| `boot(&mut self) -> Result<BootInfo, String>` | 386-462 | See flow 4.2. Retries a `VolumeSizeMismatch` with the on-disk size; any other refusal hands the parts back and returns `Err("boot: ...")`. |
| `fn pump(&mut self) -> Vec<Event>` | 464-481 | Takes the engine's events, counts generation commits (`CpCommitted { gen: Some(_), max_seq }` -> `gen_commits += 1; model.mark_committed(max_seq, gen_commits)`), appends to `self.events`, confirms rollbacks. |
| `tick()`, `ticks(n)`, `tick_until(done, what)`, `advance(ns)` | 484-535 | One engine tick plus `pump`; `tick_until` fails after `TICK_BUDGET` with phase, frozen count, `pool_status` and the last six `CpFailed`/`Internal`/`CleanSkipped` events. `advance` moves the virtual clock. |
| `submit_write(vol, lba, blocks) -> ReqId` | 538-546 | `next_op += 1`; `model.issued(op, ..)`; data `Model::data_of(op, vol, lba, blocks)`; queued on the engine's front door; `inflight[id] = op`. |
| `submit_punch(vol, lba, blocks, zeroes) -> ReqId` | 550-562 | Same for a discard or a write of zeros (`model.issued_punch`). |
| `punch(..) -> Result<Completion, String>` | 565-575 | `submit_punch` then `wait`. |
| `submit_read(vol, lba, blocks) -> ReqId`, `submit_flush(vol) -> ReqId` | 578-585 | Not recorded in the model. |
| `collect(&mut self) -> Vec<Completion>` | 589-602 | Takes the front door's completions; for each in-flight write: `Ok` -> `model.acked(op, seq)`, else `model.refused(op)`; keeps them for `wait`. |
| `wait(id, budget) -> Result<Option<Completion>, String>` | 606-615 | `0..=budget` times: `collect`, claim `id`, else `tick`. |
| `write(vol, lba, blocks) -> Result<Completion, String>` | 618-622 | `submit_write` + `wait(TICK_BUDGET)`. |
| `read(vol, lba, blocks) -> Result<(Status, Vec<u8>), String>` | 627-656 | Waits for the read; on `Ok` and only when `inflight.is_empty() && model.unresolved() == 0`, checks every block against `expected_data(vol, lba+k, model.expected(vol, lba+k, u64::MAX))`. |
| `request_cp() -> Result<u64, String>` | 661-666 | `engine.request_cp()`, waits for `cp_seq() > before && is_quiet()`. |
| `request_fold() -> Result<u64, String>` | 670-678 | Waits for `map().fold_cp() > before && is_quiet()`. |
| `snapshot(vol) -> Result<u64, String>` | 683-723 | Refuses while `unsettled()`; uuid = `next_uuid` LE in bytes 0..8, `seed` in 8..16; `create_snapshot(vol, uuid, SnapKind::Crash, SnapOrigin::User)`; waits for `Event::SnapshotCreated` of that volume; `image = model.image_of(vol)` taken at that moment. |
| `verify_snapshots() -> Result<(), String>` | 729-782 | Every kept snapshot must still be listed with the same uuid/vol/cp_seq, or be gone only after a deletion request or a boot that `rewound` below its CP; every block read by `snapshot_read` must equal `expected_data(vol, lba, image[lba])`. |
| `check_diff(snap_id) -> Result<usize, String>` | 787-854 | `live_diff(vol, Some(snap_id))` must list exactly the LBAs whose committed block (`peek_disk`) differs from the snapshot's image (zeros-vs-zeros counts only when the committed hole's `birth_cp > snap.cp_seq`), holes as holes, data as data. |
| `settle() -> Result<(), String>` | 864-869 | Ticks until `phase() == Idle && frozen_generations() == 0`. |
| `request_snapshot_deletion(id)`, `delete_snapshot(id) -> Result<u64 released, String>` | 875-908 | The first marks the snapshot "allowed to be gone"; the second waits for `Event::SnapshotDeleted`. |
| `request_rollback(vol, snap_id) -> Result<u64 op, String>`, `rollback(vol, snap_id)` | 914-930, 1011-1022 | Quiesces the volume (offline + a CP), asks the engine, records `model.rollback(op, vol, image)`; `rollback` waits until `Event::VolumeRolledBack` confirmed it. The volume stays offline. |
| `set_online(vol, bool)`, `quiesce(vol)` | 987-1005 | `quiesce` refuses while unsettled; offline + `request_cp` + wait for `unflushed_blocks == 0 && Idle`. |
| `resize_volume(vol, size)`, `delete_volume(vol)`, `create_volume(vol, size)` | 1027-1140 | Each waits for its event and updates `model` and `cfg.volumes`. |
| `request_clean() -> Result<u64, String>` | 1094-1102 | `engine.request_clean()` + wait for a new CP and quiet. |
| `audit_liveness() -> Result<(), String>` | 1107-1117 | `engine.audit_liveness()` must return no `LivenessDrift`. |
| `fn record_stop(stop)` | 1142-1153 | `held_at_stop = nvfail_set()`, `gen_commits_at_stop`, `last_stop`; clears `inflight`, `completed`, `rollbacks` (in-flight requests die with the process; their ops stay in the model as issued). |
| `fn unsettled() -> bool` | 1157-1159 | `!inflight.is_empty() || model.unresolved() > 0`. |
| `set_pacing_delay(ns)` | 1163-1165 | Before `boot`. |
| `crash_power() -> Result<CrashReport, String>` | 1169-1182 | `record_stop(Power)`; `engine.into_parts()`; `disk.crash_power()`; `front.reset()`; **a fresh `HugeArena::anonymous`** (the arena is lost). |
| `crash_daemon() -> Result<(), String>` | 1186-1196 | `record_stop(Daemon)`; `into_parts()`; `disk.crash_daemon()`; `front.reset()`; **the arena memory is kept**. |
| `shutdown_clean() -> Result<(), String>` | 1202-1217 | `engine.shutdown()`; wait `is_stopped()`; `Stop::Clean` iff an `Event::Stopped { clean: true }` was seen, else `Stop::Daemon`; `front.reset()`; arena kept. |
| `image() -> Result<Vec<(u32,u64,Vec<u8>)>, String>` | 1221-1233 | Every block of every model volume through `engine.peek` (arena first, then the map; nvfail ignored). |
| `check_disk_prefix() -> Result<u64, String>` | 1237-1255 | Every block through `engine.peek_disk` (committed map only), then `model.check_prefix`. |
| `verify_recovery() -> Result<Verdict, String>` | 1261-1350 | The checker; see 3.1 and flow 4.5. |
| `clear_nvfail() -> Result<(), String>` | 1354-1368 | `nvfail_clear` on every held volume, wait until none is held. |
| `arena_slots() -> Result<u32, ArenaError>` | 1371-1373 | `otap_arena::slots_for_len(cfg.arena_bytes)`. |

### 2.2 `otap_sim::model` (`model.rs`)

| Item | Where | Meaning |
|---|---|---|
| `pub struct WriteRecord { op, vol, lba, blocks, seq, acked, landed, committed_at: Option<u64>, punch, rollback: Option<Vec<Expected>> }` | 50-75 | One issued operation. `op` is the harness's counter (1 and up); `seq` the engine's sequence once acknowledged. |
| `pub enum Decoded { Zero, Write { op, k }, Garbage }` | 79-91 | What a block decodes to. |
| `pub type Expected = Option<(u64 op, u32 pos, bool punch)>` | 95 | What a block should hold. |
| `pub fn block_data(op, vol, lba, k) -> Vec<u8>` | 107-126 | Bytes 0..8 `op` LE, 8..12 `vol`, 12..20 `lba` (the write's first LBA), 20..24 `k`; bytes 24.. an xorshift fill seeded by `op*0x9E3779B97F4A7C15 + lba + (vol<<40) + (k<<20) | 1`. **The data encodes its own identity, volume included.** |
| `pub fn expected_data(vol, lba, expected) -> Vec<u8>` | 131-136 | `block_data(op, vol, lba - pos, pos)` for data; zeros for a punch or nothing. |
| `pub fn decode(block) -> Decoded` | 140-156 | All-zero -> `Zero`; else re-encode from the header fields and compare -> `Write` or `Garbage`. |
| `Model::new(&[(vol,size)])`, `volumes()`, `add_volume`, `remove_volume`, `resize(op, vol, size)`, `rollback(op, vol, image)`, `confirm(op)`, `set_size(vol, size)` | 161-252 | Volume bookkeeping; a shrink is recorded as a landed punch; a rollback is an operation whose expectation is the snapshot's image. |
| `Model::data_of(op, vol, lba, blocks) -> Vec<u8>` | 256-262 | `blocks` consecutive `block_data`. |
| `issued`, `issued_punch`, `acked(op, seq)`, `refused(op)`, `acked_now` | 266-317 | Operation numbers must increase (`debug_assert`). `refused` removes the record. |
| `writes()`, `unresolved()`, `settle(j) -> usize lost`, `expected(vol, lba, limit) -> Expected`, `image_of(vol) -> Vec<Expected>`, `last_op()` | 321-403 | `settle` drops every record above `j` and marks the rest landed; `expected` is the newest record with `op <= limit` covering the block (through a rollback: the snapshot image's entry). |
| `check_prefix(read) -> Result<u64, String>` | 408-503 | See 3.1. |
| `lost_volumes(j) -> VolBitSet` | 508-516 | Volumes with an acknowledged op above `j`. |
| `mark_committed(max_seq, cp_index)` | 521-535 | Records which generation commit carried each acknowledged write (`seq != 0 && seq <= max_seq`) and every unacknowledged op issued before the newest such write. |
| `recently_active(cps_now) -> VolBitSet` | 543-555 | Precise mode: volumes with an op not yet carried or carried at most one commit ago. |
| `all_volumes() -> VolBitSet` | 559-565 | Every present volume. |

### 2.3 `otap_sim::backstore` (`backstore.rs`)

| Item | Where | Meaning |
|---|---|---|
| `pub enum OpKind { Stripe, Meta, Flush, Commit, Intent, Discard }` | 28-41 | The kind of a pending operation (visible through `pending_kinds()`; tests park a CP by waiting for `Commit` to be pending). |
| `pub struct Schedule { max_per_poll: usize, reorder: bool, queue_depth: usize }`, default `{4, true, 64}` | 53-70 | `max_per_poll == 0` stalls the device; `reorder` picks a random pending op per completion. |
| `pub struct TearPolicy { drop: u32, whole: u32, torn: u32 }`, default `{1, 1, 2}` | 74-91 | Weights for what a power cut does to each unsafe write. |
| `pub struct CrashReport { dropped, whole, torn }` | 95-102 | Counts per power cut. |
| `pub struct BackstoreStats { stripes, metas, flushes, commits, intents, reads, busy, discards }` | 106-123 | Counters. |
| `pub struct SimBackstore { profile, layout, durable: Vec<u8>, current: Vec<u8>, cached: Vec<(usize,usize)>, pending: VecDeque<Pending>, completions, rng: SplitMix64, schedule, tear, stats, cache_protected }` | 127-144 | `durable` = the medium; `current` = what reads see (medium + cache + landed FUA); `cached` = completed, unflushed byte ranges; `pending` = submitted, not completed. |
| `SimBackstore::new(profile, seed) -> Result<Self, LayoutError>` | 149-166 | Zeroed device of `capacity_bytes`; `cache_protected: true`. |
| `set_cache_protected`, `cache_protected`, `layout()`, `set_schedule`, `schedule()`, `set_tear_policy`, `stats()`, `pending()`, `pending_kinds()`, `cached()`, `durable() -> &[u8]` | 169-229 | Knobs and views. |
| `corrupt_durable(&mut self, offset: usize, bytes: &[u8])` | 232-239 | **The only fault-injection knob.** Byte offset, not pba; overwrites both `durable` and `current` (so a running engine reads the damage at once, except what its page cache already holds); clipped at the device end. There is no `corrupt(pba)`, no read-error injection, no per-range fault. |
| `crash_power(&mut self) -> CrashReport` | 245-268 | Pending `Flush`/`Discard` dropped; every other pending write `land_unsafely`; cached ranges land whole when `flush_opaque && cache_protected`, else `land_unsafely`; `current = durable`; completions cleared. |
| `crash_daemon(&mut self)` | 273-279 | Every pending op is completed normally (`complete_now`), completions discarded. A pending discard therefore lands (the sim cannot model a late discard; `docs/06` 540-543). |
| `fn land_unsafely` | 281-302 | Weighted choice; `torn` keeps each 512-byte sector with probability 1/2. |
| `fn complete_now` | 304-338 | `Flush`: cached -> durable unless `flush_opaque`; `Commit`/`Intent`: durable at once (FUA); `Stripe`/`Meta`: `current` + cached; `Discard`: zero both, drop cached ranges inside. |
| `fn submit` | 340-363 | `pending.len() >= queue_depth` -> `Err(Busy)`; beyond the device -> an `OutOfRange` completion. |
| `impl Backstore` | 366-449 | `read` copies from `current`, errors only `BadLength`/`OutOfRange`; `poll` completes `rng.range(1, max_per_poll)` ops (0 when stalled), random index when `reorder`. |

### 2.4 `otap_sim::frontdoor` (`frontdoor.rs`)

| Item | Where | Meaning |
|---|---|---|
| `pub struct Completion { req: Request, status: Status, seq: u64 }` | 11-18 | What the engine delivered. |
| `pub struct SimFrontDoor { queue, data: HashMap<ReqId,Vec<u8>>, reads, completions, next_id, in_flight }` | 22-30 | A request script. |
| `submit_write(vol, lba, data) -> ReqId` (whole blocks, `debug_assert`), `submit_read`, `submit_flush(vol)`, `submit_discard`, `submit_write_zeroes` | 45-95 | Queue requests; ids are 1, 2, 3, ... per door instance. |
| `take_completions() -> Vec<Completion>`, `take_read(id) -> Option<Vec<u8>>`, `queued()`, `in_flight()`, `reset()` | 98-126 | `reset` drops everything (a client that gave up: what every crash does). |
| `impl FrontDoor` | 129-155 | `copy_in` -> `IoError::Corrupt` when the data is gone, `BadLength` on a size mismatch; `complete` pushes a `Completion` and frees the data. |

### 2.5 `otap_sim::net` (`net.rs`)

| Item | Where | Meaning |
|---|---|---|
| `type Queued = (Option<u64> deliver_at, EndpointId from, Vec<u8>)` | 14 | An inbox entry; `None` while held by a partition. |
| `struct Shared { now, delay, partitioned, queue_limit, inboxes: Vec<VecDeque<Queued>>, held }` | 17-24 | One network, `Rc<RefCell<_>>` (single-threaded, `!Send`). |
| `LoopbackNet::new(endpoints) -> Self` | 35-46 | `delay: 1`, `queue_limit: 1024`, not partitioned. |
| `end(&self, id) -> NetEnd` | 50-55 | A handle for endpoint `id`. |
| `tick(&self)` | 58-60 | `now += 1`. **Separate from `Sim::tick`.** |
| `set_delay(ticks)` | 63-65 | Delivery delay for later sends. |
| `set_partitioned(cut: bool)` | 69-82 | Global (every endpoint, both directions). Healing stamps every held entry `now + delay`, in queue order. |
| `held() -> u64` | 86-88 | Chunks ever held. |
| `set_queue_limit(limit)` | 91-93 | Inbox bound above which `send` answers `Busy`. |
| `NetEnd::id()` | 106 | The endpoint. |
| `impl Net for NetEnd`: `send(to, bytes)` | 112-134 | `Unreachable` when `to.0 >= inboxes.len()`; `Busy` when the inbox is full; otherwise push `(deliver_at, me, bytes)`. |
| `poll_recv(out) -> usize` | 136-152 | Pops from the front while `deliver_at <= now`. A held or later-due entry at the front blocks everything behind it: strict FIFO per inbox even across delay changes. |

Who uses it today: only tests (`tests/sim_tests.rs` 445-508,
`tests/adversarial_sim_tests.rs` 1035-1061 and 1205-1231). No engine or daemon
code implements or consumes `Net`; `otap-cp/src/lib.rs:13` says so, and a grep
finds no `TlsTcp`/`FileSink` anywhere.

### 2.6 `otap_sim::clock`, `otap_sim::rng`, `lib.rs`

- `SimClock::new(wall_base_ns)`, `advance(ns)`; `Clock::mono_ns` (an
  `Arc<AtomicU64>`, clones share time), `wall_ns = base + mono`
  (`clock.rs` 11-40).
- `SplitMix64::new(seed)`, `next_u64`, `below(n)`, `range(lo, hi)`,
  `chance(num, den)` (`rng.rs`).
- Re-exports (`lib.rs` 38-44): `BackstoreStats, CrashReport, OpKind, Schedule,
  SimBackstore, TearPolicy, SimClock, Completion, SimFrontDoor, BootInfo, Sim,
  SimConfig, SimEngine, SnapExpect, Stop, Verdict, TICK_BUDGET, block_data,
  decode, expected_data, Decoded, Expected, Model, WriteRecord, LoopbackNet,
  NetEnd, SplitMix64`.
- `Cargo.toml`: depends on `otap-arena`, `otap-cp`, `otap-format`,
  `otap-index`, `otap-sys`; dev-deps `otap-stream`, `proptest` (workspace
  1.12.0). `otap-sim` does **not** depend on `otap-ublk`; `otap-ublk`'s
  dev-tests depend on `otap-sim` (`engine_tests.rs:10`). A test that marries
  the two therefore lives in `otap-ublk/tests` unless `otap-sim` gains a
  dev-dependency on `otap-ublk` (no cycle: `otap-ublk` depends on `otap-sim`
  only as a dev-dependency).

### 2.7 The engine surface the harness relies on (`crates/otap-cp`)

| Item | Where | Meaning |
|---|---|---|
| `pub struct Engine<B: Backstore, F: FrontDoor, C: Clock>` | engine.rs 1364-1483 | Owns `backstore: B`, `front: F`, `clock: C`, `arena`, `index`, `map: MapSet`, `segments`, liveness maps, `snapshots`, `vols`, `events`, `stats`, ... |
| `Engine::format(&mut B, &C, &EngineConfig) -> Result<(), EngineError>` | 1566-1630 | Wipes the ring and intent slots, writes CP 0 clean to A and B. |
| `Engine::boot(cfg, B, F, C, HugeArena) -> Result<Self, BootFailure<B,F,C>>` | 1637-1727 | `boot::evaluate` -> `VolumeSizeMismatch` check -> arena attach/create -> `assemble` -> the boot commit job is started (not yet landed: drive with `tick` until `is_serving()`). |
| `BootFailure::into_parts(self) -> (EngineError, B, F, C, HugeArena)` | 827-835 | Everything lent, back. |
| `tick(&mut self) -> Result<(), EngineError>` | 2076-2118 | See flow 4.3. An `Err` is an internal failure. |
| `request_cp`, `request_clean`, `request_fold`, `shutdown`, `nvfail_clear(vol) -> Result<u32>` | 2122-2267 | Requests; `shutdown` sets `serving = false` at once. |
| `create_volume`, `create_snapshot(vol, uuid, kind, origin)`, `create_named_snapshot`, `delete_snapshot`, `set_snapshot_pins`, `set_replication_base(vol, Option<snap_id>)`, `set_volume_online`, `rollback_volume`, `resize_volume`, `delete_volume`, `create_named_volume`, `set_volume_name`, names lookup | 2273-2806 | The lifecycle. `set_replication_base` (2523-2560) sets `PIN_REPLICATION` on the new base and clears it on the volume's others in the same CP. |
| `audit_liveness(&mut self) -> Result<Vec<LivenessDrift{pseg, counted, found}>, EngineError>` | 2820-2878 | Walks the live map (`map.diff(.., 0, ..)`) and every pinned root (`pinned_diff`), counts distinct `(lseg, off)` data entries per segment, compares with `data_live`. |
| `snapshots() -> Vec<SnapshotInfo>`, `snapshot_read(snap_id, lba, dst) -> Result<Vec<PeekSource>>`, `snapshot_diff(snap_id, base) -> Result<Vec<(u64, MapEntry)>>`, `live_diff(vol, base) -> Result<Vec<(u64, MapEntry)>>` | 2895-3007 | `live_diff(vol, Some(id))` walks the committed map above the snapshot's `birth_cp`; `None` means base 0 (everything mapped). |
| `pool_status() -> PoolStatus`, `reserve_segments()` | 3028-3095 | Operator view (fields at 935-984). |
| `peek(vol, lba, dst) -> Result<Vec<PeekSource>>`, `peek_disk(..)`, `map_entry(vol, lba) -> Option<MapEntry>` | 3100-3163 | Checker reads; `peek` goes arena-then-map, `peek_disk` map only; both verify CRCs and return `EngineError::Corrupt { vol, lba }` on a mismatch. |
| `take_events`, `is_serving`, `is_stopped`, `is_pool_full`, `is_quiet`, `cp_seq`, `cp_nonce`, `phase`, `nvfail_set`, `volume(vol) -> Option<VolumeStatus>`, `arena_occupancy`, `unflushed_blocks`, `frozen_generations`, `free_segments`, `live_blocks(pseg)`, `meta_live_bytes(pseg)`, `stats`, `layout`, `config`, `map() -> &MapSet`, `front_door() -> &mut F`, `backstore() -> &mut B`, `arena_memory_mut() -> &mut HugeArena` | 3166-3330 | Accessors. `is_quiet` = `Idle && frozen_len == 0 && !cp_requested && !boot_retry && paced.is_empty()`. |
| `into_parts(self) -> (B, F, C, HugeArena)` | 3336-3343 | "As a `kill -9` would": the four parts survive, everything else is gone. |
| `Event` (`#[non_exhaustive]`): `Booted {cp_seq, dirty, reattached, replayed_blocks, rewound, arena_lost, damaged_slots, orphaned_blocks, nvfail, torn}`, `CpCommitted {cp_seq, trigger, gen: Option<GenId>, blocks, max_seq}`, `CpFailed {cp_seq, reason}`, `Folded`, `IntentWritten`, `NvfailCleared`, `VolumeCreated`, `SnapshotCreated {snap_id, vol, cp_seq}`, `SnapshotFailed`, `SegmentFreed`, `SegmentCleaned {pseg, relocated, cp_seq}`, `CleanSkipped`, `SnapshotDeleted {snap_id, vol, cp_seq, released}`, `SnapshotPinned`, `VolumeRolledBack {vol, snap_id, cp_seq, rollback_gen}`, `VolumeResized`, `VolumeDeleted`, `OperatorWarning(_)`, `ArenaDamage`, `Internal {what}`, `Stopped {cp_seq, clean}` | 334-532 | Everything the harness and the walks match on. |
| `EngineError` (`#[non_exhaustive]`): `Format, Arena, Index, Map, Meta, Layout, Segment, Boot, Io, NoSpace, NoSuchVolume, VolumeExists, BadSize, MetadataTooLarge, NvFailed, Corrupt{vol,lba}, NoSuchSnapshot, SnapshotExists, SnapshotDamaged, SnapshotOrder, VolumeSizeMismatch{vol,on_disk,configured}, MapDamaged, FlushOpaqueRefused, SnapshotPinned{snap_id,flags}, SnapshotLimit, VolumeOnline, VolumeDirty, NewerSnapshots, HasSnapshots, VolumeBusy, NameExists, NoSuchName, NotServing, Unsupported, Internal` | 537-696 | |
| `Status` (`#[non_exhaustive]`): `Ok, NoSuchVolume, OutOfRange, NvFailed, Corrupt, Stopped, Failed, NoSpace, TooLarge, Offline` | traits.rs 354-379 | How a request ended. |
| `Phase { Idle, Staging, Metadata, Flushing, CommitA, CommitB }` | 988-1001 | The CP state machine. |
| `PeekSource { Arena, Disk, Unmapped, Hole }` | 854-863 | Where a peeked block came from. |
| `DiskLayout { stripe_bytes, stripes_per_segment, segment_bytes, segments, summary: SummaryLayout, slot_stripes, intent_stripes, seg_base, b_base, capacity_bytes }` with `stripe_offset(pseg, idx) -> Option<u64 bytes>`, `stripe_pba`, `block_off(idx, i)`, `block_pba(pseg, off) -> Option<u64>`, `pseg_of_pba`, `stripe_of_off(off) -> idx`, `sb_pba(SbSlot) -> u64`, `intent_pba(which) -> u64`, `segment_first_pba`, `segment_end_pba`, `segments_of_region`, `stripe_of_pba` | layout.rs 82-327 | Address arithmetic for fault injection. Segments are numbered from 1; `lseg == pseg` today (`segments.rs` 4, F18). |
| `Backstore` trait | traits.rs 227-260 | `profile`, `write_stripe`, `write_meta`, `flush(DevClass, tag)`, `commit(pba, &SuperblockSlot, tag)`, `write_intent`, `read(pba, dst) -> Result<(), IoError>` (synchronous), `discard_segment(pba, bytes, tag)`, `poll(&mut Vec<Done>)`. |
| `FrontDoor` trait | 438-452 | `fetch() -> Option<Request>`, `copy_in(&Request, dst)`, `copy_out(&Request, src)`, `complete(Request, Status, seq)`. |
| `Clock` trait | 455-462 | `mono_ns`, `wall_ns`. |
| `Net` trait, `EndpointId(u32)`, `NetError { Busy, Unreachable }` | 464-501 | `send(to, bytes) -> Result<(), NetError>`, `poll_recv(out) -> usize`. "A chunk that `send` accepted is delivered in order; nothing is dropped silently." |
| `IoError { Device{errno}, OutOfRange{pba,bytes}, BadLength{bytes}, Corrupt }` | 45-66 | |

### 2.8 The ublk front door and its mock (`crates/otap-ublk`)

| Item | Where | Meaning |
|---|---|---|
| `pub trait Transport { poll(&mut Vec<QueueEvent>) -> Result<usize, TransportError>; read_data(tag, off, dst) -> Result<(), IoError>; write_data(tag, off, src); commit(tag, result: i32) -> Result<(), TransportError> }` | frontdoor.rs 62-76 | The queue as the state machine sees it. No recovery verbs exist yet. |
| `QueueEvent { Request { tag, desc: UblksrvIoDesc }, Abort { tag } }` | 29-42 | |
| `UblkFrontDoor<T: Transport>::new(transport, q_id, vol, size_blocks, depth, Watermarks)` | 247-282 | One queue, one volume. |
| `set_offline`, `is_offline`, `in_engine`, `ready`, `stats`, `watermarks`, `transport() -> &mut T`, `last_error`, `aborted_tags`, `pump() -> Result<usize, TransportError>` | 288-423 | No `into_transport(self)`: the transport cannot be taken out of a door (needed for a re-attach; see 5.1). |
| `impl FrontDoor`: `fetch` pumps when `ready` is empty, gates on the watermarks, `ReqId = request_id(q_id, tag, gen)`; `complete` drops a stale (gen mismatch) completion, turns `Status` into an errno (`errno_for`, 192-206: `Ok` -> bytes; `NvFailed|Corrupt|Failed` -> `-EIO`; `NoSuchVolume|Stopped|Offline` -> `-ENODEV`; `NoSpace` -> `-ENOSPC`; `OutOfRange|TooLarge` -> `-EINVAL`) and **discards `seq`** (499: `_seq`). | 437-519 | |
| `MockTransport::new(depth)`; `submit_write(tag, lba, data)`, `submit_read`, `submit_flush`, `submit_discard`, `submit_write_zeroes`, `submit_raw`, `abort(tag)`, `inject(event)`, `take_commits() -> Vec<(u16, i32)>`, `take_commit(tag) -> Option<i32>`, `read_payload(tag)`, `outstanding()`, `fail_copies(bool)`, `fail_commits(bool)`, `fail_next_poll()`, `polls()` | mock.rs 39-162 | A queue in memory: `busy` = tags with a request outstanding; `deliver` asserts a tag is below the depth, not aborted and not busy; `commit` of an idle tag fails with errno 22. |
| `tests/engine_tests.rs`: `type UblkEngine = Engine<SimBackstore, UblkFrontDoor<MockTransport>, SimClock>` (22); `geometry()`/`profile()`/`config_for()` (24-68: 96 segments of 256 KiB, `pacing.max_delay_ns = 0`), `front_door(vol, size)` (70-79), `boot_with(cfg, disk, front)` (85-97: `HugeArena::anonymous(2 << 20)`, `UblkEngine::boot(..).map_err(|f| f.error)`, tick until serving), `fresh_engine_with` (105-110), `tick_until` (112-120, 200,000 ticks), `wait(e, tag) -> i32` (124-135), `block_pattern`, `write`, `read` (137-165) | | The second harness that already drives an engine behind the ublk door over the simulator's device. `nvfail_after_a_power_cut_answers_eio_until_cleared` (288-334) does `into_parts` -> `disk.crash_power()` -> a **new** front door (the mock forgets) -> `boot(disk, front)`. |

### 2.9 The stream the transport tests will drive (`crates/otap-stream`)

- `Frame` (frame.rs 702-): `Hello, HelloAck{cursor}, DeltaBegin, VolEvent,
  Chunk{image: Bytes, codec, raw_len}, Holes{records: Vec<HoleRecord>},
  DeltaEnd{digest, chunks, bytes}, Landed{idx, credits, landed_contig},
  Committed{applied_cp_seq, applied_epoch}, Abort{reason, idx, detail},
  Ping, Pong`. `FrameMeta { rel, chunk_idx, cp_seq, base, cp_nonce,
  owner_epoch }` (40-53) is the header context; `check` enforces
  `base <= cp_seq`, `cp_seq <= MAX_CP_SEQ`, `chunk_idx == 0` off a chunk.
- `DeltaBegin { mode: DeltaMode::{Steady | Increment{from,to}}, chunks,
  bytes, cut_age_ms, write_active: VolBitSet, tags: Vec<SnapTag>, acked:
  Vec<AckedBytes> }` (297-313). `VolEvent { event: Create|Resize|Delete|
  SnapTag, vol, class, size_blocks, stripe_bytes, retention_s, uuid, name }`
  (420-437). `HoleRecord { vol, len, lba }` (otap-format stream.rs 332-339).
  `Chunk::raw(Bytes)` requires a stripe-sized image; `Chunk::summary()` needs
  an 8-aligned image (503-512).
- `Encoder::encode(&FrameMeta, &Frame) -> Result<Bytes>` (codec.rs 153);
  `Decoder::feed(&[u8])`, `next_frame() -> Result<Option<RawFrame>>`,
  `drain()`, `needed()`, `is_poisoned()`; `RawFrame::decode(self) ->
  (FrameMeta, Frame)` (67). A bad header poisons; a bad payload CRC yields
  `StreamError::PayloadCrc{kind, chunk_idx, cp_seq}` and stays in sync (S5).
- `Cursor { applied_cp_seq, applied_epoch, landing: Option<Landing{cp_seq,
  cp_nonce, bitmap: ResumeBitmap}>, credits }`; `begin(base, cp_seq,
  cp_nonce, chunks) -> BeginOutcome::{Started, Resumed, Discarded, Replaced}`
  (343-383; `BaseMismatch` when `base != applied_cp_seq`); `land(cp_seq,
  cp_nonce, idx) -> LandOutcome::{New, Duplicate}` (387-405); `commit(epoch)
  -> Result<u64>` (`Incomplete` unless every chunk landed, 409-421);
  `abort()`; `resume_point()`; `to_wire`/`from_wire`; `to_page(pipe_id) ->
  CursorPage` / `from_page` (505-526; 4 KiB, CRC-sealed, 500 ranges).
- `ReceiverWindow::new(staging_bytes)`, `initial_grant()`, `grant_up_to`,
  `landed(bytes)`, `drained(bytes)`, `reset()`; `SenderWindow::new`,
  `can_send`, `reserve(bytes) -> Result<u32>` (`Window` error), `grant`,
  `reset` (flow.rs). `CREDIT_BYTES = 4096`, `INITIAL_GRANT_BYTES = 64 MiB`,
  `MAX_CREDITS = 524,288`.

## 3. Invariants and rules in force

### 3.1 The checker's rules (model.rs 12-42, E18, `verify_recovery` 1278-1340)

- Every block a write carries names its write (`block_data`); a recovered
  block decodes to a write, zeros, or garbage; garbage fails the check.
- The engine places operations in issue order and a CP carries whole
  generations, so a crash leaves the state after the issued operations up to
  some `j`. `Model::check_prefix` finds the **largest** consistent prefix:
  1. decode every block; `Garbage` -> error; a `Write{op}` must be covered by
     a non-punch, non-rollback record of that volume and range (423-434), and
     `j = max(op)`;
  2. fixed point (447-479): a block that disagrees with `expected(vol, lba, j)`
     must be explained by the oldest punch or rollback above `j` covering it,
     which raises `j`; otherwise error;
  3. every block must agree at `j` (480-487);
  4. zeros are ambiguous (a punch over a write looks like nothing), so `j` is
     raised to the largest `w.op` at which the whole image is still consistent
     (492-501).
- `verify_recovery`:
  - `Stop::Fresh`: `k == 0` and nvfail empty (1279-1283).
  - `Stop::Daemon` and `Stop::Clean`: no acknowledged write lost
    (`lost.is_empty()`), `nvfail == held_at_stop`; a clean stop must not boot
    dirty (1284-1300).
  - `Stop::Power`: `lost ⊆ nvfail`, `held_at_stop ⊆ nvfail`; on a dirty boot,
    Conservative -> `nvfail == all_volumes`, Precise -> `nvfail ⊆
    recently_active(gen_commits_at_stop) ∪ held`; a clean boot keeps `nvfail
    == held` (1301-1339).
  - Then `model.settle(k)` (acknowledged writes above `k` are forgotten, the
    rest landed), `verify_snapshots`, `audit_liveness` (1341-1343).
- `read` checks data against the model only when nothing is in flight
  (627-656); `snapshot` and `quiesce` refuse while `unsettled()`.
- Snapshot ids are monotonic between rewinds, reused after one
  (`SnapshotInfo` doc, engine.rs 895-897); `verify_snapshots` therefore
  forgets a snapshot only after a `rewound` boot below its CP or a deletion
  request (736-749).

### 3.2 The fakes' promises (E18, backstore.rs 1-17, net.rs 1-4)

- A flush makes the cache durable unless `flush_opaque`; FUA writes
  (`Commit`, `Intent`) are durable on completion; a power cut tears each
  unsafe write at sector granularity; a daemon crash lets the device finish
  what it had (and so completes discards: `docs/06` 540-543 records that the
  sim cannot model a late discard).
- Completions arrive in a seeded order, a few per poll (`Schedule`).
- The loopback net never drops an accepted chunk, delivers in order per inbox,
  and refuses an unknown endpoint (`Net` contract, traits.rs 489-493). A
  partition is global and holds in both directions.

### 3.3 Engine rules the tests lean on

- E13: the engine is driven by `tick()` only; the simulator controls every
  interleaving. `tick` fetches at most 64 front-door requests
  (engine.rs 3360) after serving deferred writes.
- E17/09 "Code rules": anything a client can observe exists only once it is
  durable (a volume, a rollback generation, an acknowledgement). A set the
  engine keeps as "what the disk guarantees" is narrowed by intersection.
- E30: a read error while loading a boot candidate fails the boot
  (`BootError::Io`), only a region that reads back and does not verify tears
  a candidate; a damaged old snapshot root is flagged, not torn.
- E32: a damaged map page answers `Status::Corrupt` to its blocks and a device
  read error `Status::Failed` (serve_read 3644-3652); neither fails the tick.
- E39: a CP that carries clean targets takes no deletion and no volume
  operation; a snapshot in flight blocks the volume's operations
  (`VolumeBusy`); a summary that does not parse skips the segment
  (`SkipReason::Summary`); a damaged catalogue drops the names, not the pool.
- Boot/nvfail (`assemble` 1866-1885): `hold = dirty && (!arena_whole ||
  rewound || arena_lost || orphaned_blocks > 0)`; `arena_whole` means the
  re-attach report has no damaged slot; the cause is `TornCp` after a rewind,
  `DirtyBoot` else.
- F-rulings relevant to fault injection: F3 (summary = `[StripeHdr][VolBitSet]
  [StripeDescriptor; nblocks]`, `hdr_crc` over bytes 0..60, `summary_crc`
  over the rest), F7 (ring `A0 A1 ... B0 B1`, commit N writes `A(N mod 2)`
  then `B(N mod 2)`), F12-F14 (directory of delta regions, snapshot root
  pages, table regions), F16 (catalogue region carried forward), F17 (every
  commit lists its stripes in `cp_stripes`; the boot verifies them), F18
  (`VOL_OFFLINE`; drained segments are freed, never relocated, so `lseg ==
  pseg`).
- S-rules for the transport tests (E06): S1 header bounds, S2 canonical
  bodies, S3 "the builder refuses what the parser refuses", S5 decoder error
  semantics (`Abort(Corrupt, idx)` for a chunk, `Abort(Corrupt, 0)` else),
  S6 raw byte counts and the BLAKE3 digest, S8 no sockets in `otap-stream`,
  S10 no allocation proportional to an untrusted count before the check.
- E09: `prop_assert!` always carries a message; adversarial `bug_*` tests are
  ported in place, never weakened; `failure_persistence: None`.
- 09 section 6: one cargo process at a time, `-j 3`/`-j 2`,
  `--test-threads=6`; the 10,000-case gate (180 min after Step 5) runs in the
  background; `PROPTEST_CASES` overrides any explicit `cases`.

### 3.4 Where the docs and the code disagree

- `otap-cp/src/lib.rs` 74-86 ("What this step leaves out": relocation, the
  cleaner, snapshot deletion) and `segments.rs` 4 ("the cleaner is a later
  step") are stale since Step 5; the cleaner exists (E37/E39) but
  `lseg == pseg` still holds because drained segments are freed (F18).
- `otap-ublk/src/lib.rs` 33-36 says the `USER_RECOVERY` re-attach "is
  `dbladed`'s, Step 5"; `docs/06` 550-556 and the Step 6 brief put it in
  Step 6. Nothing of it exists (grep for `USER_RECOVERY`/reaper finds only
  these mentions and `otap-sys/src/ublk.rs`).
- The Step 6 brief as recorded (`docs/10` 578-627) ends mid-sentence in the
  transport's "Dual Transport Backends" list; the fourth worker (WSL 2
  tooling) and the rest of the transport scope were never received in
  writing.
- `docs/05` 8.4 promises "10,000 seeds per merge, one million nightly" and a
  `turmoil`/`TokioTcp` lane; neither a nightly nor a tokio transport exists
  (E18 took proptest + the seeded device instead of madsim).
- The review's C1 ("the walks ignore `PROPTEST_CASES`") was disproved
  (`docs/08` 943-952, E39): `proptest!` applies the variable over an explicit
  `cases`; the walks now read it themselves anyway.

## 4. How things happen, in order

### 4.1 `Sim::new` (harness.rs 289-326)

1. `SimClock::new(1_700_000_000_000_000_000)`.
2. `SimBackstore::new(cfg.profile(), cfg.seed)`; schedule, tear policy and
   cache protection from the config (291-294).
3. `Engine::<SimBackstore, SimFrontDoor, SimClock>::format(&mut disk, &clock,
   &cfg.engine_config())` (295-300): the ring and both intent slots are
   zeroed stripe by stripe through `write_meta`, a flush, then CP 0 is
   committed to `A` and `B` (engine.rs 1584-1628).
4. `HugeArena::anonymous(cfg.arena_bytes, HugePageSize::TwoMiB)` (301).
5. `Model::new(&cfg.volumes)`; `next_op = 0`, `last_stop = Stop::Fresh`.

### 4.2 `Sim::boot` (386-462) and `Engine::boot` (engine.rs 1637-1727)

1. The parked `disk`, `front`, `memory` are taken (390-392).
2. `Engine::boot(cfg.engine_config(), disk, front, clock.clone(), memory)`:
   `check_flush_policy`, `cfg.validate`, `DiskLayout::new`,
   `boot::evaluate(&mut backstore, &layout, &agg_id)` (reads the ring raw,
   ranks the slots, walks candidates newest first: segment table, roots,
   every volume's index, the directory and delta regions, the snapshot table
   and pinned roots, the fold's pages, every stripe the candidate wrote by
   its summary; a candidate that fails is torn, a read error fails the
   boot), then the `VolumeSizeMismatch` check (1672-1685), then
   `Arena::can_attach`/`attach` or `create` (1687-1709), `assemble`, and the
   boot commit job is **started** (1713).
3. On `VolumeSizeMismatch { vol, on_disk, .. }` the harness sets
   `model.set_size(vol, on_disk)`, patches `cfg.volumes`, and retries
   (411-423); any other error restores the parts and returns `Err("boot:
   ...")`.
4. `pump()` finds `Event::Booted` and builds `BootInfo` (432-458); then
   `tick_until(is_serving, "the boot commit")` (460): the engine serves only
   once the boot commit is durable.
5. Inside `assemble` (1731-2000): `dirty = !is_clean_shutdown || rewound`
   (1767); volumes from the slot's table (1771-1785) and the configuration
   (1786-1805: a volume the disk lacks is created with the boot commit,
   held `TornCp` after a rewind); the re-attach report's placed runs are
   replayed into the index (1820-1864; unknown volumes -> orphaned); nvfail
   as in 3.3; the `Booted` event is inserted first (1986-1999).

### 4.3 One engine tick (engine.rs 2076-2118)

1. `backstore.poll(&mut dones)`; discard completions free their segment
   (2081-2091); intent completions (`handle_intent_done`); the rest go to
   `advance_cp` (the CP state machine: Staging -> Metadata -> Flushing ->
   CommitA -> CommitB -> finalize).
2. `retry_intent`, `retry_discards`, a pending boot-commit retry (2097-2107).
3. `complete_paced` (delayed acknowledgements whose time came).
4. If serving and not stopped: `serve_front_door` (3353-3381): deferred writes
   first, then up to 64 `front.fetch()`; writes and punches behind a deferred
   one are deferred too; reads and flushes never wait. If stopped:
   `refuse_everything` (`Status::Stopped`; paced writes complete `Ok`).
5. `maybe_cut` (timer, arena half full, back to back, a request, shutdown),
   `maybe_start_cp`, `maybe_pump` (a full pool with a pending deletion).

A read (3630-3662): per block `read_block` = index lookup (arena, CRC-checked
against the recorded CRC) else `read_disk_block` = `map.get` then `read_entry`
(backstore read + `crc32c(out) != entry.crc32c` -> `Corrupt`); `Corrupt` or a
map page that fails its CRC -> `Status::Corrupt`; an `IoError` -> `Status::
Failed`; then `copy_out` and `complete`.

### 4.4 A write through the harness

1. `submit_write` (538-546): the model learns the op as issued; the door
   queues `Request{ id, Write{vol,lba,blocks} }` with the self-describing data.
2. `wait` (606-615): each round `collect` then `tick`. The engine's
   `serve_front_door` fetches the request, reserves arena slots, `copy_in`,
   CRC per block, takes a sequence number, inserts into the active
   generation, and `complete(req, Ok, seq)` (or defers it behind a full
   arena, or answers a status).
3. `collect` (589-602) maps the completion back to the op: `Ok` ->
   `model.acked(op, seq)`; anything else -> `model.refused(op)` (the record
   disappears: a refused write never happened).
4. A later `request_cp`: the CP commits; `pump` sees `CpCommitted { gen:
   Some(_), max_seq }`, counts a generation commit and calls
   `model.mark_committed(max_seq, gen_commits)` (464-476).

### 4.5 Crash, boot, verify

Power cut (1169-1182): `record_stop(Power)` (held set, commit count, clear the
in-flight maps) -> `into_parts` -> `disk.crash_power()` (pending and cached
writes torn per the policy; `current = durable`) -> `front.reset()` -> a
**new** arena. Then `boot()` (4.2) and `verify_recovery()`:

1. `sync_sizes` (1263): every present volume's size from the engine.
2. `image()` through `peek` (1264), then `model.check_prefix` -> `k` (1269).
3. `lost = model.lost_volumes(k)`, `nvfail = engine.nvfail_set()`, `held =
   held_at_stop` (1275-1277); the rules of 3.1 by `last_stop`.
4. `model.settle(k)`; `verify_snapshots()`; `audit_liveness()`; `Verdict`.

Daemon crash (1186-1196): the same with `disk.crash_daemon()` (everything
pending completes) and the arena kept, so the boot re-attaches
(`Booted.reattached == true`), replays the placed runs, and (if nothing was
rewound and no slot is damaged) holds nothing: the verdict then demands
`lost.is_empty()` and an unchanged nvfail set (`sim_tests.rs` 196-227 is the
canonical test). A daemon crash **with** media damage under the newest CP
rewinds and re-attaches at once and holds the rewound volumes `TornCp`
(`adversarial_sim_tests.rs` 908-938).

Clean shutdown (1202-1217): `engine.shutdown()` (serving stops at once; the
final CP carries the active generation and `CleanShutdown`), wait
`is_stopped`, record `Clean` or `Daemon`, arena kept.

### 4.6 `check_disk_prefix` (1237-1255)

Every block of every model volume through `peek_disk` (committed map only:
"what a power loss right now would leave"), then `model.check_prefix`. Used by
the walks after every op (`sim_tests.rs` 619) and in `check_everything`
(`step5_sim_tests.rs` 135-143).

### 4.7 The loopback net (net.rs)

`send` (112-134): refuse an unknown endpoint, refuse a full inbox, stamp
`now + delay` (or `None` while partitioned, `held += 1`), push. `tick` (58):
`now += 1`. `poll_recv` (136-152): pop while the front is due. Healing
(69-82) stamps every held entry `now + delay` in order. Ordering is FIFO per
inbox regardless of delay changes (`adversarial_sim_tests.rs` 1205-1231).

### 4.8 The ublk door inside the engine tick

`fetch` (frontdoor.rs 438-469): if nothing is ready, `pump()` polls the
transport and decodes descriptors (`decode_request`; unsupported or malformed
answered at once with `-EOPNOTSUPP`/`-EINVAL`, offline with `-ENODEV`); the
watermark gate closes at `high` and reopens below `low`; the request id is
`gen << 32 | q_id << 16 | tag`. `complete` (499-518): a completion whose
`(q, tag, gen)` is not the live one is counted stale and dropped; else the tag
goes idle, `gen += 1`, and `transport.commit(tag, errno_for(status, kind))`.
In the mock, `commit` moves the tag out of `busy` and records `(tag, result)`
(`mock.rs` 208-224); the test's `wait(e, tag)` (`engine_tests.rs` 124-135)
ticks until `take_commit(tag)` returns it.

## 5. Hook points for Step 6

### 5.1 Scope A: an engine behind `UblkFrontDoor<MockTransport>` in a crash/recover scenario

**Does the harness accept another front door?** No. `SimEngine =
Engine<SimBackstore, SimFrontDoor, SimClock>` (harness.rs 190) and `Sim`
stores `front: Option<SimFrontDoor>` (248). Every path that touches the door
uses `SimFrontDoor`'s own methods: `submit_write` (543), `submit_punch`
(554-559), `submit_read` (579), `submit_flush` (584), `collect` (590
`take_completions`), `read` (638 `take_read`), and `front.reset()` in all
three stops (1174, 1191, 1212). Making `Sim` generic (`Sim<F: ScriptFront =
SimFrontDoor>`) would need a small trait with `submit_*`, `take_completions ->
Vec<Completion>`, `take_read`, `reset`, implemented for `SimFrontDoor` and for
an adapter over `UblkFrontDoor<MockTransport>` that maps tags to `ReqId`s and
errnos back to `Status`. Two things make that adapter lossy: the ublk door
discards the write's `seq` (`frontdoor.rs` 499), and `errno_for` folds
`NvFailed|Corrupt|Failed` into `-EIO` and `NoSuchVolume|Stopped|Offline` into
`-ENODEV` (192-206). Without `seq`, `Model::mark_committed` (521-535) marks
nothing (`w.seq != 0` is required), so `recently_active` returns every
volume: only the Precise-mode branch of `verify_recovery` (1322-1332) is
affected, Conservative is not. The status collapse breaks the walks'
`Status` expectations unless the adapter is told what to expect.

**Recommendation.** Keep `Sim` as it is and build scope A's tests on the
existing engine-level rigs:

- `crates/otap-ublk/tests/engine_tests.rs` 22-135 (`UblkEngine`, `boot_with`,
  `fresh_engine_with`, `tick_until`, `wait`), which already runs
  `into_parts` -> `disk.crash_power()` -> a fresh door -> `boot` (288-334);
- `crates/otap-sim/tests/adversarial_sim_tests.rs` 119-161, the generic
  `RigEngine<F> = Engine<SimBackstore, F, SimClock>` with `boot_rig`,
  `boot_rig_with`, `fresh_rig<F: FrontDoor>` (any front door, including a
  wrapper like `FlakyFront`, 163-190) and `rig_wait` (192-205).

The crash sequence for a `kill -9` with the door surviving is one line away
from what exists: `let (mut disk, front, clock, memory) = e.into_parts();
disk.crash_daemon();` keeps the arena (`memory`) and lets the device finish;
what is missing is a way to keep the **transport** across the death. Today
`UblkFrontDoor` has `transport() -> &mut T` (335) but no `into_transport(self)
-> T`; the mock's `busy`/`payloads`/`aborted` sets are the driver's state and
must survive, while the door's `tags`, `ready`, `in_engine` die with the
process. The smallest coherent change: add `UblkFrontDoor::into_transport`
(or `into_parts`) and a constructor for the recovery path (for instance
`UblkFrontDoor::recover(transport, q_id, vol, size_blocks, depth, marks)`)
that starts every tag `Idle` and relies on the transport re-delivering the
outstanding requests, which is what the kernel does after
`START_USER_RECOVERY` when the new server fetches each tag. The mock needs a
`requeue_outstanding()` (re-emit `QueueEvent::Request` for every busy tag
with its payload, as the driver's re-delivery) and a quiesced mode in which
`commit` is refused until an `end_recovery()` call; both are mock-only knobs
plus, if the `Transport` trait grows recovery verbs, their mock
implementation. The checker for such a test is the mock itself: every tag
commits exactly once (a second commit of an idle tag fails with
`"commit of an idle tag"`, errno 22, mock.rs 215-220, and shows up in
`stats.commit_failures`); every acknowledged write reads back
(`read(e, tag, ..)` through the re-attached engine); the `Booted` event says
`reattached: true, nvfail: EMPTY, damaged_slots: 0` for an undamaged arena;
`replayed_blocks` equals the acknowledged-but-uncommitted blocks.

If a `Sim`-model-checked version is wanted as well (to reuse `check_prefix`
after a ublk-door crash), the lossless way is not an adapter but a second
door: a `SimFrontDoor` **and** a `UblkFrontDoor` cannot both feed one engine
(it takes one `F`), so the alternative is to write the ublk payloads with
`Model::data_of(op, vol, lba, blocks)` and keep a private `Model` in the test
(`model.issued`/`acked(op, 0)`/`check_prefix` over `peek`), as
`adversarial_sim_tests.rs` does with its rigs. Nothing in `Model` depends on
`Sim`.

What must not break: `engine_tests.rs` and `review_ublk_tests.rs` construct
the door with `UblkFrontDoor::new(..)` and the mock with `MockTransport::new
(depth)`; keep those signatures. `FrontDoor` is implemented for `Box<F>`
(traits.rs 419-435), so a boxed door is an option for a daemon-side composite.

### 5.2 Scope B: corrupting specific on-disk structures; liveness reconciliation

**The knob.** `SimBackstore::corrupt_durable(offset_bytes, &bytes)`
(backstore.rs 232-239) is the whole of it. It is byte-addressed; convert with
`pba as usize * BLOCK_SIZE`. It writes `durable` and `current` alike, so no
crash is needed for the engine to read the damage, but a map page the engine's
cache (`cache_pages = 16`) already holds is not re-read: the review's recipe
(`review_sim_tests.rs` 275-291) is `crash_daemon` (cold cache), corrupt, `boot`.
`Sim::disk()` (346-351) reaches the device whether or not an engine runs.

**Address recipes already in the tests** (all byte offsets):

| Structure | How to find it | Used at |
|---|---|---|
| A data block's payload | `e = engine.map_entry(vol, lba)`; `pba = layout.block_pba(e.lseg(), e.off())` | `review_sim_tests.rs` 1300-1305 (zeroes the block under a trusted opaque barrier: the boot rewinds) |
| A stripe summary | `idx = layout.stripe_of_off(e.off())`; `off = layout.stripe_offset(e.lseg(), idx)`; `+8` hits `StripeHdr::agg_id` (header CRC), `+64` the first byte of the `VolBitSet` (summary CRC); descriptors start at `+64 + 128` (`STRIPE_DESCRIPTORS_OFFSET`) | `sim_tests.rs` 178-183, `review_sim_tests.rs` 91-99 (`stripe_offset_of`), `review_step5_sim_tests.rs` 495-502 |
| A map page | `engine.map().volume(vol).unwrap().index()[page_no].page_pba` | `review_sim_tests.rs` 284-288 |
| A volume's page index | `engine.map().volume(vol).unwrap().root().index_pba` | `review_sim_tests.rs` 1080-1083 (shared by both candidates: the boot is refused) |
| The roots table region | `newest_slot(s).sb.map_roots.pba` | `review_sim_tests.rs` 1011-1015, 1337-1342 |
| A delta region / the directory | `engine.map().delta_roots().last()` / `slot.sb.cp_log_head` | `review_sim_tests.rs` 1054-1061 |
| The segment table | `slot.sb.seg_table.pba` | `adversarial_sim_tests.rs` 954-969 |
| The snapshot table and a snapshot root page | `slot.sb.snap_table` -> `parse_snap_table(&bytes)` -> `rec.map_root_pba` | `review_sim_tests.rs` 577-586 |
| The catalogue | `slot.sb.catalog.pba` (+33 is a name byte) | `review_step5_sim_tests.rs` 540-561 |
| The stripe list | `slot.sb.cp_stripes` | not yet corrupted by any test |
| A superblock slot | `layout.sb_pba(SbSlot::A0) * BLOCK_SIZE`, `SUPERBLOCK_SLOT_SIZE` (20,480) bytes | `review_step5_sim_tests.rs` 585-589 (both A slots wiped), `sim_tests.rs` 44-47 (the device wiped) |
| An intent slot | `layout.intent_pba(0 | 1)` | none yet |
| The arena | `placed_slot_data(&memory, vol, lba)` over `ArenaHeader`/`SlotDesc` (`adversarial_sim_tests.rs` 208-218), or `engine.arena_memory_mut()` while running (504-530) | |

`newest_slot(s)` (`review_sim_tests.rs` 103-118) reads all four slots through
`s.disk().read(layout.sb_pba(slot), &mut buf)` and `SuperblockSlot::read`; the
`Superblock` roots are `map_roots, seg_table, reloc_index, cp_log_head,
snap_table, cluster_state, catalog, cp_stripes` (superblock.rs 475-511), each
a `RootRef { pba, pages, crc32c }`. These helpers are duplicated across three
test files; lifting them into `otap-sim` (a `probe`/`damage` module:
`newest_slot`, `stripe_offset_of`, `block_offset_of`, `map_page_offset_of`,
and typed `corrupt_*` helpers) is the smallest useful change for scope B.

**What the sim cannot inject today, and the attestation needs.** `read`
(backstore.rs 410-425) never fails: no `IoError::Device`, no per-range fault,
no transient error. The brief's "without panicking on corrupted sectors" and
E30's read-error path (`Status::Failed`, `BootError::Io`) need a knob such as
`SimBackstore::fail_reads(pba_range, IoError, times: Option<u32>)` consulted
at the top of `read`, plus a `corrupt_block(pba, f: impl FnOnce(&mut [u8]))`
convenience. Add them next to `corrupt_durable` (232-239); keep
`corrupt_durable` unchanged (23 call sites).

**What the boot already catches versus what the attestation must find.**
E30/F17: the boot verifies the selected candidate's regions, the fold's
pages, the stripes that candidate wrote, pinned roots and (opaque barrier,
unclean stop) their data blocks. It does **not** read the stripes or the
payloads of older CPs, nor map pages the candidate did not write (those are
verified lazily, on read, by the index entry's CRC). So the attestation tests
should damage **older** structures: write several CPs, then corrupt a block or
summary written two or more CPs back, then `crash_daemon` + `boot` +
`verify_recovery` must still pass (the boot and `peek`-based checker see
nothing until a read lands on the block: `check_prefix` **will** read it and
report `Corrupt`, so corrupt a block the model no longer expects to be read
as data, or corrupt a summary only), and the attestation must name it. The
engine already answers `Status::Corrupt` for a payload whose CRC fails
(3725-3726) and `Status::Failed` for a device error, so a read after the
attestation is the natural cross-check.

**Liveness reconciliation against the model.** `Engine::audit_liveness`
(2820-2878) is the in-memory version: per segment, distinct `(lseg, off)`
data entries over the live map and every pinned root versus `data_live`;
the harness runs it after every recovery (1343) and in `check_everything`.
The attestation's reconciliation should reuse the same two walks
(`MapSet::diff(reader, vol, 0, f)` 1620-1626 and `pinned_diff` 1714-1720)
but compare against the **on-disk** `SegEntry::live_bytes` (the region named
by `sb.seg_table`, `SegEntry` fields at records.rs 129-153), not the engine's
counters, and account for `dead_data_pending`/`freed_pending` (segments
counted zero but still allocated until the deferral). The model supplies an
independent oracle for the live map only: for every `(vol, lba)` with
`model.expected(vol, lba, u64::MAX) == Some((op, pos, false))` after a
settled recovery, the committed entry must be data whose block decodes to
`Write { op, k: pos }` (that is exactly `check_disk_prefix`); the number of
such blocks is the live map's data-entry count, and snapshot-only blocks come
from `SnapExpect.image` entries that differ from the live expectation. A
test can therefore assert `sum(found) == live_data_entries +
snapshot_only_entries` with `found` from the attestation report, dedup by
`(lseg, off)` as `audit_liveness` does (2833, 2852).

**Hook in the harness.** Once `Engine::attest()` (or an associated fn over
`&mut B` for an offline pass) exists, add `Sim::attest() -> Result<Report,
String>` and call it after `audit_liveness` in `verify_recovery` (1343) and
in `check_everything`, provided it is cheap on the small pools (reading every
live block of 128 to 2,048 blocks is nothing; on the step5 walk's pools it is
still under a second per call). Do not make it mandatory in the 10k-case
walks until measured: the gate is already 180 minutes.

### 5.3 Scope C: two Sims connected by `LoopbackNet`

**Two pools.** `Sim::new` builds its own clock (290) and derives `agg_id` from
`cfg.seed` (145-152): two Sims with the same seed would have the **same
aggregate identity** and the same device completion order. Use different
seeds (the agg_id differs), or better, add `SimConfig::agg_id: Option<Uuid>`
and `Sim::with_clock(cfg, clock: SimClock)` (hook: 289-291; `SimClock` clones
share time, so one clock can serve both engines and the transport's timers).
The target's `cfg.volumes` must list the replicated volumes with the same
index and size (a `VolEvent::Create` for a runtime volume maps to
`Sim::create_volume` semantics on the target: the model and `cfg.volumes` must
learn it, 1121-1140). The data's self-description includes `vol` (block_data
8..12), so a replica must keep the source's volume index for the model to
decode it.

**Driving.** `LoopbackNet::new(2)`; `net.end(EndpointId(0))` for the sender,
`EndpointId(1)` for the receiver; a test-local `Pair { src: Sim, dst: Sim,
net: LoopbackNet, sender, receiver }` with `fn tick(&mut self)` that ticks
`src`, `dst`, `net`, and polls the sender and the receiver (their `poll_recv`
+ decode + state machine). Partition: `net.set_partitioned(true)`, run `n`
ticks of traffic (chunks are held, `net.held()` counts them), heal, and
assert the receiver lands the held chunks in order with the cursor's
`landed_contig` advancing. Back-pressure: `net.set_queue_limit(k)` makes
`send` answer `NetError::Busy`, which the sender must retry on a later tick
(the `Busy` contract, E13) without losing credits. Delay:
`net.set_delay(n)`.

**What the loopback net cannot do.** It never drops anything (the `Net`
contract). Doc 05 8.4's "`ss -K` on the connection resumes from the chunk
bitmap with duplicates equal to resends" is a **disconnect with loss of what
was in flight**, and a receiver reboot "re-sends up to 10 s of chunks". The
`Net` trait has no connection notion; the session (`Hello`/`HelloAck`) is the
unit. Scope C needs a loopback knob that models a reset (for instance
`LoopbackNet::drop_in_flight()` or `NetEnd::reset()` that empties the inbox
and counts what it dropped) and a sender `reconnect()` that re-sends `Hello`
and resumes from the `HelloAck` cursor; the duplicates the receiver counts
(`LandOutcome::Duplicate`) must equal the chunks the reset dropped plus those
the sender re-sent above `landed_contig`. Decide whether that knob lives in
`net.rs` (preferred: next to `set_partitioned`, 69-82) or in the transport.

**The oracle.** The source pins a base snapshot (`s.snapshot(vol)` records
`SnapExpect.image = model.image_of(vol)` at that moment, 683-723; the sender
then calls `set_replication_base(vol, Some(id))`, 2523-2560). After the
receiver replies `Committed`, the target volume must read, block for block,
as `expected_data(vol, lba, snap.image[lba])` of the **source's** newer
snapshot (the delta's end), through `dst.engine().peek_disk` (committed map
only, since the delta is a CP on the target). `verify_snapshots`'s loop
(761-777) is the pattern. The alternative oracle, `check_disk_prefix` on the
source right after the CP and `check_prefix` with the source's model on the
target's image, works too (the model is independent of the Sim: build the
target's checker from the source's `model.clone()` restricted to ops at or
below the delta's `j`). For holes: a `Holes` frame must land as holes
(`map_entry(..).is_hole()`, `birth_cp` above the base), which `check_diff`'s
logic (787-854) already encodes. For `VolEvent`s: `dst.engine().volume(vol)`
and `volumes()`.

**The target-side ingest does not exist.** The engine's only write path is
the front door (`serve_front_door`); there is no `stage_chunk`/`commit_delta`
API, no receiver staging, no "commit the delta as a CP on the target pool".
That is scope C's engine work; until it exists the only way to put a delta on
the target in a test is to replay it as front-door writes and punches on the
target Sim (not atomic, not what the brief wants) and to check the image
with the model. The map's open question 5 is about this.

**Frames over the loopback.** `Encoder::new().encode(&meta, &frame)` gives
the bytes to `send`; the receiver feeds every polled chunk into one
`Decoder` per peer and drains it (`sim_tests.rs` 453-488 is the exact
pattern, with `Ping`); a chunk frame's payload is a stripe image, so the
sender builds it from the source's stripes (`layout.stripe_offset` +
`disk().durable()`; or a new engine `read_stripe(lseg, idx)`), and the
receiver verifies `Chunk::summary()` after landing into an aligned buffer.
Corrupt-frame tests flip a byte in the `Vec<u8>` between `poll_recv` and
`feed` (the decoder reports `PayloadCrc` and the receiver sends
`Abort(Corrupt, idx)`; S5).

### 5.4 Determinism caveats (all scopes)

- No wall clock anywhere in the harness: `SimClock` only; `Sim::advance`
  moves it; `HugeArena::anonymous` is plain memory; the engine's timers
  (CP timer, fold interval, pacing) fire only on `advance`.
- Every random choice is seeded: the device (`SimBackstore::new(.., seed)`,
  `SplitMix64`), the tests' own `SplitMix64::new(seed)` generators, and
  proptest's `any::<u64>()` seeds (which differ per run: the gate "is
  repeated over time rather than treated as final once", `docs/09` 139-140).
- `PROPTEST_CASES`: the walks read it explicitly (`proptest_cases(default)`,
  `step5_sim_tests.rs` 1378-1383; `cases(default)`,
  `review_step5_sim_tests.rs` 914-919) and `proptest!`'s
  `contextualize_config` applies it over any explicit `cases` anyway.
  `failure_persistence: None` everywhere; `max_shrink_iters` 128-2048.
- `LoopbackNet` is `Rc<RefCell<_>>`: single-threaded, which is what a
  tick-driven pair needs; a test must tick the net explicitly.
- `HashMap` iteration in `Sim`/`Model` never decides an outcome (the
  `check_prefix` walk is over a `BTreeMap` of volumes and a `Vec` of decoded
  blocks; `by_block` is only looked up).
- `TICK_BUDGET` (100,000) bounds every wait; the error text carries the
  phase, frozen count, pool status and the last complaints.
- Two Sims have two clocks unless a shared clock is threaded in (5.3).

## 6. Test patterns to copy

- **Crash/recover helpers** (`step5_sim_tests.rs` 105-132; `step4` 91-118):
  `power_cut_and_recover(s, tear)` = set tear policy, `crash_power`, `boot`,
  `verify_recovery`, `clear_nvfail`; `daemon_crash_and_recover(s)`;
  `clean_restart(s)` which also demands `v.k == s.model.last_op()`.
- **Every check at once**: `check_everything` (`step5` 135-143):
  `check_disk_prefix`, `verify_snapshots`, `check_diff` per snapshot,
  `audit_liveness`.
- **Parking a CP in a phase**: `fifo1()` (one completion per poll, in order,
  `step5` 43-49) + `tick_to_phase(s, phase)` (146-149), or `tick_into(s,
  phase, extra)` over `CUT_PHASES` (`review_step5` 967-986); restore
  `Schedule::default()` after the crash (`step5` 324, 547). Waiting for a
  commit to be pending: `e.phase() == CommitA && e.backstore().
  pending_kinds().contains(&OpKind::Commit)` (`sim_tests.rs` 117-122).
- **A stop of every kind by a byte**: `stop_and_recover(s, kind)`
  (`review_step5` 1010-1029) and `tear_of(n)` (40-47).
- **Writes in flight at the crash**: `submit_inflight` (`review_step5`
  1033-1043) with `submit_write` and no `wait`; `record_stop` clears the
  in-flight map and the model settles them at the recovery check.
- **"Deleted never comes back"**: `note_deleted`/`gone` by uuid
  (`review_step5` 1045-1055, 1183-1195), because ids are reused after a
  rewind.
- **Failure diagnostics**: `diagnostics(s)` (`step5` 62-88),
  `complaints(s)` (`review_step5` 130-140); every walk wraps errors with
  `format!("seed {seed} op {i} {op:?}: {e}")`.
- **The walk skeleton** (`step5` 1149-1374; `step4` 1232-1383; `sim_tests`
  514-632; `review_step5` 921-1246, 1252-1422): a `#[derive(Debug, Clone)]
  enum WalkOp` with `u8`/`u16` fields; `prop_oneof![weight => strategy, ..]`;
  a `pick` closure that maps `(vol: u8, lba: u16, blocks: u8)` onto the
  volumes (`lba * 7 % (size - blocks + 1)` spreads a 16-bit LBA); expected
  statuses derived from `nvfail_set()`/offline/`NoSpace`; `settle()` before
  requests that must see the engine idle (`review_step5` 1080); the engine's
  own view re-read into the walk's state after every op (`step5`
  1346-1356); a final clean restart with `v.k == model.last_op()` and a
  reserve-floor check (`step5` 1358-1372).
- **The `proptest!` block** (`step5` 1385-1409): `#![proptest_config(
  ProptestConfig { failure_persistence: None, cases: proptest_cases(64),
  max_shrink_iters: 512, ..ProptestConfig::default() })]`, `seed in
  any::<u64>()`, `ops in prop::collection::vec(walk_op(), 4..32)`, and
  `if let Err(e) = walk(seed, &ops) { prop_assert!(false, "{e}"); }`.
  Default case counts: 64 (Step 4/5 walks), 24 and 12 (the review's two
  properties), proptest's 256 default (`sim_tests.rs` 634-647).
- **Virtual time in a scenario**: `Op::Ticks(u8)` and `Op::Advance(u8)`
  (seconds) in `sim_tests.rs` 514-521, 596-597.
- **Engine-level rigs** (any front door): `RigEngine<F>`, `boot_rig`,
  `fresh_rig`, `FlakyFront`, `rig_wait` (`adversarial_sim_tests.rs`
  119-205); the ublk rig (`engine_tests.rs` 22-135) and the review's variant
  with `request_cp(e)` (`review_ublk_tests.rs` 366-454).
- **Corruption recipes**: the table in 5.2.
- **Arena damage between two processes**: `placed_slot_data(&memory, vol,
  lba)` after `into_parts` (`adversarial_sim_tests.rs` 470-501), or
  `engine.arena_memory_mut()` while running (504-530).
- **A lying controller built from another device's medium**:
  `lying.corrupt_durable(0, disk.durable())` (`adversarial_sim_tests.rs`
  854-858) copies a whole image into a device with a different profile.
- **LoopbackNet**: `sim_tests.rs` 445-508 (frames round-tripped through
  `Encoder`/`Decoder` over the net, partition, bounded inbox) and
  `adversarial_sim_tests.rs` 1035-1061 (held across a partition, unknown
  endpoint), 1205-1231 (no reordering when the delay shrinks).
- **Named/scheduled snapshots and pins**: `step5` 210-284 (`PIN_REPLICATION`
  semantics), `review_step5` 690-729 (pending pin changes).
- **Reading the ring from the medium**: `newest_slot` (`review_sim_tests.rs`
  103-118), `SbSlot::ALL`, `SuperblockSlot::read(&bytes)`.

## 7. Risks, pitfalls and traps

1. **`collect` is the only path that records an acknowledgement.** Taking
   completions through `engine().front_door().take_completions()` directly
   (as the ublk rig does through the mock) leaves the model's record
   unacknowledged; `verify_recovery` then treats a lost write as "never
   happened" and misses a real loss. Always go through `Sim::collect`/`wait`
   or keep your own `Model` and call `acked` yourself.
2. **The payload must be `Model::data_of`.** Any other bytes decode as
   `Garbage` and fail `check_prefix`; the `block_pattern` of
   `engine_tests.rs` cannot be mixed with the model.
3. **`snapshot`, `quiesce`, rollback and resize refuse while `unsettled()`**
   (in-flight writes or unconfirmed rollbacks). Call `verify_recovery` after
   every `boot` (it is what settles the model); a walk that boots without
   verifying accumulates unresolved ops until `snapshot` fails with "a
   snapshot needs nothing in flight".
4. **`read` checks data only when nothing is in flight**; with writes in
   flight it checks the status alone (627-656).
5. **`crash_power` replaces the arena; `crash_daemon` keeps it**; `memory()`
   is `None` while an engine runs. To damage the arena between processes,
   do it after a crash and before `boot` (`adversarial_sim_tests.rs`
   470-477), or through `arena_memory_mut()` while running.
6. **A stalled or `fifo1` schedule stays in force across a crash** (the
   device object survives `into_parts`); restore `Schedule::default()` before
   `boot` or every wait times out at `TICK_BUDGET`.
7. **`corrupt_durable` on a running engine is masked by the page cache** for
   map pages (cold cache: crash first); for data blocks and summaries it is
   visible at once because `read_entry` reads the device every time.
8. **The boot verifies only the newest candidate's own stripes and the
   fold's pages** (E30, F17): damage to older stripes, payloads or map pages
   is not a torn CP; it surfaces on read as `Status::Corrupt`. A test that
   expects a rewind must damage what the candidate wrote (its stripes via
   `cp_stripes`, its delta region, its roots table).
9. **A read error has no injection path** (`read` never fails) and a
   permanently bad sector **refuses** the boot rather than tearing the
   candidate (E30); the attestation tests need the new knob of 5.2.
10. **Snapshot ids are reused after a rewind** (engine.rs 895-897); key
    long-lived expectations by uuid (`note_deleted`).
11. **`request_cp` waits for `cp_seq() > before && is_quiet()`**: a CP
    already in flight is not the one that took the request; on a full pool
    `is_quiet` may never hold (`review_sim_tests.rs` has `cp_or_full`,
    133-142).
12. **The engine fetches at most 64 requests per tick** and the ublk door's
    watermark closes at `depth`; deep queues need several ticks before every
    request is in the engine (`an_abort_mid_flight...`, `engine_tests.rs`
    356-432, shows served-vs-deferred after one tick).
13. **`Sim.events` accumulates across reboots** (`Booted` events are pushed
    by `boot`'s `pump`); count from a saved `seen = s.events.len()` as the
    harness does (697, 888, 1043, 1074, 1125, 1204).
14. **`cfg.volumes` is advisory after rollbacks and resizes**: `boot`
    patches it from `VolumeSizeMismatch` (411-423) and `sync_sizes` (978-983)
    takes the engine's word after every boot; a test that keeps its own size
    table must re-read `engine().volume(vol).size_blocks` (the step5 walk
    does, 1348-1356).
15. **Two Sims with one seed share an aggregate identity** (5.3).
16. **The loopback net holds, never drops**, and a held entry at the front
    blocks everything behind it; a disconnect-with-loss needs a new knob;
    `set_partitioned` is global.
17. **`Stop::Daemon` is recorded for a clean shutdown that could not write
    its final CP** (1206-1209): the verdict then allows a dirty boot but no
    loss.
18. **The deferral**: a freed segment stays allocated until the next commit
    (one more under an opaque profile) and until its discard completes; tests
    that count `SegmentFreed` run two or three extra `request_cp`s
    (`step5` 702-706, `review_step5` 210-212).
19. **E39 timing**: a deletion or volume operation asked while a cleaning CP
    stages lands one CP later; a snapshot in flight makes the volume's
    operations `VolumeBusy`; walks `settle()` before such requests
    (`review_step5` 1077-1080, 1378-1383).
20. **The gate's wall time**: 180 minutes at 10,000 cases, dominated by the
    single-threaded walks; every new walk adds minutes per case; keep per-case
    work small (the small pool, a few dozen ops) and run new suites as
    separate processes.
21. **`debug_assert!`s** in `SimFrontDoor::submit_write` (whole blocks) and
    `Model::issue` (increasing ops) only fire in debug builds; the gate runs
    the dev profile (`BUILDING.md` 48), so they do fire there.
22. **`otap-sim` does not depend on `otap-ublk`**; a test that uses both
    lives in `otap-ublk/tests` (or `otap-sim` gains a dev-dependency; no
    cycle).

## 8. Open questions for the lead

1. **Scope A's home.** Keep `Sim` fixed to `SimFrontDoor` and build the
   `USER_RECOVERY` tests on the `UblkEngine` rig in `otap-ublk/tests`
   (recommended; the model can be used standalone there), or make `Sim`
   generic over a small scriptable-door trait at the cost of losing `seq`
   (Precise-mode verdicts) and collapsing statuses to errnos?
2. **The mock's recovery model.** Does the `Transport` trait grow recovery
   verbs (`start_recovery`/`end_recovery`, a quiesced state in which commits
   are refused), with `MockTransport` keeping `busy`/`payloads` across the
   daemon's death and re-delivering outstanding requests, or is the re-attach
   modelled entirely in `dbladed` above an unchanged trait? Either way
   `UblkFrontDoor` needs `into_transport`/a recovery constructor.
3. **Duplicate acknowledgements.** The brief says "without duplicating or
   dropping acknowledged writes". Over the mock, "no duplicate" is "every tag
   commits once" (a second commit fails with errno 22). Is that the
   acceptance check, and what is the oracle for a request that was in the
   engine but not yet acknowledged when the daemon died (re-delivered by the
   driver and written again, which the model tolerates as the same op)?
4. **Read-fault injection.** Add `fail_reads(range, IoError)` and
   `corrupt_block(pba, f)` to `SimBackstore`, and lift the address helpers
   (`newest_slot`, `stripe_offset_of`, ...) into `otap-sim`? Should a read
   fault be transient (n times) as E30 assumes?
5. **Where the attestation runs and what it reports.** On a running engine
   (`&mut self`, interleaved with ticks, or one blocking pass) or offline
   over `&mut B` like `format`? Which structures must it find that the boot
   does not (older stripes, payload CRCs, map pages never read, the segment
   table's `live_bytes` versus a fresh walk, `dead_data_pending`)? Is the
   harness to call it after every recovery in the walks, or only in
   dedicated tests (cost)?
6. **Scope C's engine entry points.** What is the target-side API to stage
   chunks and commit a delta as one CP (staging space, the `CursorPage`'s
   home on the target pool, `Landed` after staging or after a CP)? What is
   the source-side read of a stripe image (`read_stripe(lseg, idx)` or a map
   walk that rebuilds stripes)? The sim tests cannot be written before these
   exist; a stand-in that replays the delta as front-door writes would test
   the wire, not the CP.
7. **Two pools, one clock.** Add `SimConfig::agg_id` and `Sim::with_clock`
   (or a `SimClock` parameter) so a pair shares virtual time and has distinct
   identities? And a `Pair` helper in `otap-sim` (a `pair.rs` with the tick
   loop) rather than per-test code?
8. **Disconnect semantics on the loopback.** The `Net` contract forbids
   drops; doc 05 8.4 wants a reset with loss and resend-equals-duplicates.
   Add `LoopbackNet::reset_inbox(endpoint)`/`drop_in_flight()` and a
   per-direction partition, or model the reset at the session layer only?
9. **The oracle for the replica.** Compare the target's committed image with
   the source's `SnapExpect.image` of the delta's end snapshot (recommended),
   or run the source's `Model::check_prefix` over the target's image? Must
   the test also assert the root-level equality doc 05 names ("a blake3 over
   the pinned root at the common CP is equal on both sides")?
10. **Case counts and placement for the Step 6 walks.** One
    `step6_sim_tests.rs` with three walks (recovery over the ublk rig cannot
    live there; see 1) or one file per scope; default `cases` of 64 like the
    Step 4/5 walks or smaller, given the 180-minute gate?
11. **Precise mode in Step 6 tests.** The ublk path loses `seq`; is Precise
    nvfail out of scope for the recovery tests, or must the door keep `seq`
    (a change to `FrontDoor::complete`'s contract for ublk)?
