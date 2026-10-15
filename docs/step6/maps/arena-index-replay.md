# Map: the write cache (arena), the dirty index, the write path and its replay, for scope (A)'s reconciliation of in-flight ublk requests after a `kill -9`

Written 2026-10-14 from a read-only pass over the repository at `E:/Coding/OpenTAP`. Nothing was built or run. Line numbers are of the files as they stand today (Step 5, `5114866`). Where I state a Linux kernel fact that is not encoded in this repository (the ublk driver's recovery behaviour), I say so explicitly: the saved 6.12 `ublk_drv.c` that E08/E28 cite is not in the tree, only the UAPI header is (`crates/otap-sys/src/ublk.rs`).

---

## 1. Scope: what was read

| File | Lines | Why |
|---|---|---|
| `crates/otap-arena/src/lib.rs` | 1-1349 (all) | layout 1.2, states, runs/laps, holes, attach, single writer |
| `crates/otap-index/src/lib.rs` | 1-724 (all) | generations, insert semantics, sequence rule, cut |
| `crates/otap-cp/src/traits.rs` | 1-501 (all) | `FrontDoor`, `Request`, `Status`, `Backstore` |
| `crates/otap-cp/src/lib.rs` | 1-115 (all) | crate doc: write path, boot, nvfail |
| `crates/otap-cp/src/engine.rs` | 1-120, 334-700, 1252-1500, 1561-2275, 3100-3260, 3310-3940, 3941-4020, 4315-4540, 5525-5720, 5711-6200 | consts, `Event`, `EngineError`, `Engine` struct, `boot`, `assemble`, `tick`, serve/write/punch/ack/read, intents, cut, `new_job`, staging, commit, `build_slot`, `finalize`, `runs_of` |
| `crates/otap-cp/src/boot.rs` | 1-796 (all) | `evaluate`, `load_cp`, `BootState::rewound` |
| `crates/otap-sys/src/hugetlb.rs` | 1-515 (all) | `HugeArena::map_file` re-attach, heap/anonymous, the `flock` |
| `crates/otap-sys/src/ublk.rs` | 1-62, 270-335, 395-425, 520-600, 795-880, 1170-1470 | recovery commands and flags, `IoCommand`, `UblkControlRing::execute`, `UblkQueue` |
| `crates/otap-ublk/src/{lib,frontdoor,device,mock}.rs` | all | the tag state machine, the Linux transport, the mock |
| `crates/otap-daemon/src/{arena,pool,frontdoor,daemon,device,config,lib}.rs` | all | how the daemon opens the arena, boots, creates devices, routes completions |
| `crates/otap-sim/src/{lib,frontdoor,model,harness,backstore}.rs` | lib/frontdoor all; model 1-80, 256-560; harness 28-120, 240-300, 355-665, 1140-1374; backstore 1-60, 273-279 | crash/reboot harness and the checker's rules |
| Tests | `otap-arena/tests/{arena_tests,adversarial_arena_tests}.rs` (all); `otap-sim/tests/sim_tests.rs` 1-300; `adversarial_sim_tests.rs` 123-218, 465-540, 1171-1245, 1325-1337, 1930-2010; `step4_sim_tests.rs` 1380-1409; `review_step5_sim_tests.rs` 1-30, 905-930; `otap-ublk/tests/engine_tests.rs` 1-170, 356-420; `frontdoor_tests.rs` 1-47, 394-482; `otap-daemon/tests/daemon_tests.rs` 1-60, 663-723; index/ublk/daemon test listings | patterns to copy |
| Docs | `07` E08, E09, E16, E17, E20, E23, E28, E30, E34 (S5-1), E38, E39 head; `06` sections 7 and 8; `10` the Step 6 brief (578-640); `08` the two `USER_RECOVERY` mentions; `01` line 205; `scripts/linux-smoke.sh` 1-150 and the SIGKILL step (251-272) | rulings in force and the stated gaps |

---

## 2. Public API and key internals

### 2.1 `otap-arena` (`crates/otap-arena/src/lib.rs`)

Constants

| Item | Line | Meaning |
|---|---|---|
| `ARENA_MAGIC = "OTAR"` | 91 | header magic |
| `ARENA_VERSION: u16 = 0x0102` | 95 | layout 1.2 (minor 1: sealed words + attach counter; minor 2: `kind` word + hole slots). Major mismatch refuses, minor does not (`verify` 211) |
| `HOLE_RECORD_MAGIC = "OTHL"`, `HOLE_RECORD_SIZE = 32` | 97, 100 | hole record block |
| `SLOT_BYTES = 4096`, `SLOT_DESC_SIZE = 32`, `SLOT_WORD_OFFSET = 24` | 102-106 | one 4 KiB block per slot; 32-byte descriptor; the sealed word is its last 8 bytes |
| `ARENA_HEADER_SIZE = 4096`, `ARENA_HEADER_SEALED = 56`, `ARENA_ATTACH_WORD_OFFSET = 64`, `ARENA_HEADER_RESERVED = 4096 - 72` | 108-116 | only bytes 0..56 are under the header CRC; the attach word (64..72) and the 4024 reserved bytes (72..4096) are outside the seal |
| `DEFAULT_ARENA_BYTES = 8 GiB`, `MAX_LAP = 2^30 - 1` | 114, 118 | |

Types

| Item | Line | Meaning |
|---|---|---|
| `enum SlotState { Free=0, Filled=1, Placed=2, Committed=3 }`; `from_bits`, `as_u8`, `is_reusable` (Free or Committed) | 120-157 | 2 state bits |
| `struct ArenaHeader { magic, ver, rsvd0, agg_id: Uuid, slots: u32, slot_bytes: u32, desc_off: u64, data_off: u64, created_wall_ns: u64, crc32c: u32, rsvd1: u32, attach_word: u64, reserved: [u8; 4024] }` (`repr(C, align(4096))`) | 159-192 | sealed once at `create`; `verify(len)` 207-226 checks magic, major, CRC and geometry-for-length; `attach_count()` 237 decodes the self-checking word `count \| !count << 32` |
| `enum SlotKind { Data=0, Hole=1 }`; `from_u32` returns `None` for anything else | 248-275 | the descriptor's `kind` word; **30 bits unused but any value >= 2 is `Damage::Kind` at attach** (890-898) |
| `struct HoleRecordBlock { magic, vol: u32, lba: u64, blocks: u32, rsvd0: u32, rsvd1: u64 }`; `image(vol, lba, blocks) -> [u8; 4096]` (record then zeros); `decode(block, vol, lba) -> Option<Self>` requires magic, same vol/lba as the descriptor, `blocks >= 1`, both reserved fields zero, the rest of the block zero, and `lba + blocks <= MAX_LBA + 1` | 277-334 | |
| `struct SlotDesc { addr: u64 /* vol<<40 \| lba */, seq: u64, crc32c: u32, kind: u32, word: u64 }` (`repr(C)`, 32 bytes, 8-aligned) | 336-354 | `word` low 32 = `lap << 2 \| state`, high 32 = CRC-32C over bytes 0..24 ++ low word. `pack_addr` 371, `vol()` 377, `lba()` 383, `lap()` 389, `state()` 395, `word_for(state, lap)` 402, `verify()` 413 (`word != 0 && word_for(state, lap) == word`), `sealed_as()` 422 (which other state the stored CRC belongs to; tells damage from an interrupted fill) |
| `struct RingRef { slot: u32, lap: u32 }`; `offset(blocks)` | 436-454 | a run is named by its first slot and the lap it was handed out on |
| `struct PlacedSlot { ring: RingRef, vol: u32, lba: u64, seq: u64, crc32c: u32, hole_blocks: u32 }`; `is_hole()` = `hole_blocks != 0` | 456-481 | one entry per Placed slot kept at attach, descriptor and data verified |
| `enum Damage { Descriptor, Data, Kind }`; `struct DamagedSlot { slot, desc, damage }` | 483-507 | |
| `struct Reattach { placed: Vec<PlacedSlot> /* (seq, position) order */, damaged: Vec<DamagedSlot>, dropped_filled: u32, committed: u32, attach_count: u32 }`; `is_damaged()` | 509-532 | the report `Engine::assemble` consumes and then drops |
| `enum ArenaError { TooSmall, BadMagic, BadVersion, BadCrc, BadGeometry, WrongAggregate, Inconsistent, Full{wanted,free}, BadRun, LapExhausted, Stale{ring,lap}, OutOfRange, BadAddress, BadTransition{slot,state}, DataCrc{slot} }` (`non_exhaustive`) | 534-614 | |
| `struct Arena { mem: HugeArena, slots, desc_off, data_off, head: u64, tail: u64, next_seq: u64 }` | 687-699 | `head`/`tail` are positions `lap * slots + slot`; the window `[tail, head)` is what may hold unflushed data |

Methods of `Arena`

| Signature | Line | Meaning |
|---|---|---|
| `create(mem, agg_id, wall_ns) -> Result<Self, (ArenaError, HugeArena)>` | 704-736 | zeroes header + descriptor table (not the data), seals the header, attach count 1, `next_seq = 1` |
| `is_blank(&HugeArena) -> bool` | 742-745 | header page all zero |
| `can_attach(&HugeArena, &Uuid) -> Result<(), ArenaError>` | 749-761 | header verify + aggregate check, no writes |
| `attach(mem, &Uuid) -> Result<(Self, Reattach), (ArenaError, HugeArena)>` | 766-795 | `can_attach` then `scan_on_attach` |
| `scan_on_attach(&mut self, &ArenaHeader) -> Result<Reattach, ArenaError>` (private) | 798-949 | the re-attach scan (section 4.3) |
| `header()`, `slots()`, `capacity_bytes()`, `occupied()` (= head - tail), `occupancy()` | 953-981 | |
| `next_seq(&mut self) -> u64`, `peek_seq()` | 984-995 | one number per request, never reissued (max over all descriptors + 1 after attach) |
| `reserve(&mut self, blocks: u32) -> Result<RingRef, ArenaError>` | 1002-1050 | bump allocation; never wraps a run; skipped slots at a lap end are written `Committed`; the run's slots are written `Free` on the new lap; `Full{wanted, free}` when it would reach live data; `LapExhausted` |
| `run_mut(ring, blocks) -> Result<&mut [u8]>` | 1054-1060 | data of a `Free`/`Filled` run on the right lap (for the copy-in) |
| `run(ring, blocks) -> Result<&[u8]>` | 1063-1069 | data of a `Filled`/`Placed` run |
| `check_data(ring, blocks)` | 1073-1084 | every block against its descriptor CRC; `DataCrc{slot}` |
| `fill(ring, vol, lba, seq, crcs: &[u32])` | 1090-1115 | needs every slot `Free` on the lap; per slot: store `addr, seq, crc32c, kind=Data`, then one sealed word `Filled`. **Slot by slot**, so a kill mid-fill leaves a `Filled` prefix and a `Free` (reserved) suffix |
| `fill_hole(ring, vol, lba, blocks, seq)` | 1122-1145 | one slot: writes the record image, then the descriptor (`kind=Hole`, `crc` of the image), then the word `Filled` |
| `hole_record(ring) -> Result<HoleRecordBlock>` | 1148-1159 | decode a `Filled`/`Placed` hole slot |
| `place(ring, blocks)` | 1163-1165 | `Filled -> Placed`, per slot (`transition` 1222-1235). **A kill mid-place leaves a `Placed` prefix and a `Filled` suffix** |
| `commit(ring, blocks)` | 1169-1173 | `Placed -> Committed`, per slot, then `advance_tail` |
| `discard(ring, blocks)` | 1178-1187 | `Free`/`Filled -> Committed` (a refused write, a failed copy) |
| `descriptor(slot) -> Result<SlotDesc>` | 1190-1195 | any slot, any state: the fields of a released slot stay readable until it is reserved again |
| `window() -> (tail, head)`, `into_memory() -> HugeArena`, `memory()`, `memory_mut()` | 1199-1220 | |
| `write_word(slot, state, lap)` (private) | 1294-1299 | the one store that moves a slot; `compiler_fence(SeqCst)` between the field stores and the word |
| `passable(pos)`, `advance_tail()` (private) | 1278-1288 | the tail passes `Committed` slots and slots on another lap; a `Free` slot on the tail's own lap (a reservation) pins the window |
| `pub fn slots_for_len(len) -> Result<u32>` | 1321-1323 | `Geometry::for_len` (659-685): 2 MiB -> 507 slots; slots fit `(len - 4096) / (4096 + 32)` rounded down so the descriptors fill whole pages |

### 2.2 `otap-index` (`crates/otap-index/src/lib.rs`)

| Item | Line | Meaning |
|---|---|---|
| `type GenId = u64`; `type Superseded = Vec<(RingRef, u32)>`; `superseded_blocks` | 56-66 | |
| `DEFAULT_RING = 8` | 69 | active + at most 7 frozen |
| `struct DirtyExtent { ring: RingRef, blocks: u32, seq: u64, hole: bool }`; `data(ring, blocks, seq)`, `hole(record, blocks, seq)`, `slice(from, to)`, `slot_of(k)` | 71-137 | a hole's `ring` is its one record slot; `slice` keeps it |
| `struct Hit { ring, seq, gen: GenId, hole }` | 139-150 | what `lookup` returns |
| `enum IndexError { BadVol, BadExtent, StaleSeq{vol, seq, newest}, RingFull, NotOldest }` | 152-188 | |
| `VolumeDirty::insert(&mut self, lba, ext) -> Superseded` | 238-272 | the later insert wins its whole range: overlapped extents are removed, their outside parts re-put, the overlapped data runs returned as superseded (holes release nothing), then `coalesce_around` |
| `VolumeDirty::lookup(lba) -> Option<(RingRef, u64)>`, `find(lba) -> Option<(u64, &DirtyExtent)>`, `iter()`, `blocks()`, `data_blocks()`, `extents()`, `is_empty()` | 277-324 | |
| `coalesce_keys` | 349-386 | merges adjacent extents whose slots continue each other on one lap (data) or share one record slot (holes); merged `seq = max` |
| `Generation { id, vols: Vec<Mutex<VolumeDirty>>, blocks, inserts, opened_ns, cut_ns }`; `with_volume`, `for_each_dirty_volume`, `dirty_volumes`, private `insert`/`lookup` | 391-509 | |
| `Index { active: ArcSwap<Generation>, frozen: Mutex<VecDeque<Arc<Generation>>>, ring, next_id, cut_lock, grace_spins, newest_seq: Vec<AtomicU64> }` | 513-522 | `newest_seq` is per volume across generations |
| `Index::new(ring, now_ns)` (generation 1 active) | 528-539 | |
| `Index::insert(&self, vol, lba, ext) -> Result<(GenId, Superseded), IndexError>` | 553-586 | `BadExtent` for 0 blocks or past `MAX_LBA`; **`newest_seq[vol].fetch_max(ext.seq)`: a seq below the previous newest is `StaleSeq`; equal is allowed** ("another piece of the same write", 573-574); then insert into the active generation |
| `newest_seq(vol)` | 590 | |
| `Index::lookup(vol, lba) -> Option<Hit>` | 599-613 | active first, then frozen newest first |
| `with_active`, `active_id`, `active_blocks`, `unflushed_blocks`, `cut_within`, `cut(now_ns)`, `oldest_frozen`, `frozen`, `frozen_len`, `retire(gen)` | 617-716 | `cut` swaps the active pointer and spins until the old `Arc` count is 1 (the grace period); `retire` only the oldest frozen |

### 2.3 `otap-cp` traits (`crates/otap-cp/src/traits.rs`)

| Item | Line | Meaning |
|---|---|---|
| `struct ReqId(pub u64)` | 264 | the front door's identity for a request |
| `enum RequestKind { Write{vol,lba,blocks}, Read{..}, Flush{vol}, Discard{vol,lba,blocks}, WriteZeroes{..} }`; `Request { id, kind }`; `vol()`, `is_punch()` | 268-349 | |
| `enum Status { Ok, NoSuchVolume, OutOfRange, NvFailed, Corrupt, Stopped, Failed, NoSpace, TooLarge, Offline }` (`non_exhaustive`) | 354-379 | `Ok` for a write carries the seq in `complete`'s third argument |
| `trait FrontDoor { fetch() -> Option<Request>; copy_in(&Request, &mut [u8]) -> Result<(), IoError>; copy_out(&Request, &[u8]); complete(Request, Status, seq: u64) }` | 438-452 | `copy_in`'s `dst` is arena memory of `blocks * 4096` bytes |
| `enum IoError { Device{errno}, OutOfRange, BadLength, Corrupt }` | 45-66 | `Corrupt` is what a stale/aborted ublk request's copy returns |

### 2.4 `otap-cp` engine (`crates/otap-cp/src/engine.rs`)

| Item | Line | Meaning |
|---|---|---|
| `MAX_REQUEST_BLOCKS = 2048`, `MAX_PUNCH_BLOCKS = 1 << 18` | 37, 41 | |
| `Pacing { pace_at: 0.80, stall_at: 0.95, max_delay_ns: 50 ms }`; `delay_ns(occupancy)` | 77-106 | acks delayed above 80 % arena occupancy, writes held unplaced at 95 % |
| `Event::Booted { cp_seq, dirty, reattached, replayed_blocks, rewound, arena_lost, damaged_slots, orphaned_blocks, nvfail: VolBitSet, torn }` | 336-359 | the boot's report; the daemon prints it (`pool.rs` 333-375) |
| `Event::CpCommitted { cp_seq, trigger, gen: Option<GenId>, blocks, max_seq }` | 361-372 | `max_seq` = highest write seq the CP carried |
| `Event::ArenaDamage { vol, lba, slot }`, `Event::Internal { what }`, `Event::Stopped { cp_seq, clean }` | 508-531 | |
| `EngineError` (incl. `Arena(ArenaError)`, `Index(IndexError)`, `Boot(BootError)`, `NvFailed`, `Corrupt{vol,lba}`, `VolumeSizeMismatch`, `NotServing`, `Internal(String)`) | 537-696 | |
| `BootFailure<B,F,C> { error, backstore, front, clock, memory }`; `into_parts()` | 811-852 | a refused boot hands everything back, the arena memory included |
| `VolRt { present, creating, created_in, size_blocks, rollback_gen, nvfail: Option<NvCause>, clearing, windows: u8, offline, name, op, op_in }`; `serving()` = present && !creating && nvfail none && !offline | 1280-1307 | |
| `Engine` fields that matter here: `arena: Arena` 1370, `index: Index` 1371, `paced: VecDeque<(Request, u64, u64)>` 1411, `gen_holes: HashMap<GenId, Vec<RingRef>>` 1438, `durable_active: VolBitSet` 1458, `intent_inflight` 1459, `pending_acks: Vec<(Request, u64)>` 1461, `deferred: VecDeque<Request>` 1465, `serving` 1466, `stopped` 1467 | 1364-1483 | **the `Reattach` report is not kept** |
| `Engine::boot(cfg, backstore, front, clock, memory: HugeArena) -> Result<Self, BootFailure>` | 1637-1727 | section 4.2 |
| `assemble(..)` (private) | 1731-2071 | section 4.3 |
| `tick()` | 2076-2118 | poll completions -> `advance_cp` -> `retry_intent` -> `retry_discards` -> boot retry -> `complete_paced` -> `serve_front_door` (only when `serving && !stopped`) or `refuse_everything` -> `maybe_cut` -> `maybe_start_cp` -> `maybe_pump` |
| `shutdown()`, `nvfail_clear(vol)` | 2240-2267 | |
| `peek(vol, lba, dst)`, `peek_disk(..)`, `map_entry` | 3100-3163 | arena-then-map read for checkers; map-only read |
| `take_events()`, `is_serving()`, `is_stopped()`, `is_pool_full()`, `is_quiet()`, `cp_seq()`, `phase()`, `nvfail_set()`, `volume(vol)`, `arena_occupancy()` | 3166-3260 | |
| `front_door() -> &mut F`, `backstore()`, `arena_memory_mut() -> &mut HugeArena`, `into_parts() -> (B, F, C, HugeArena)` | 3317-3343 | `into_parts` is the simulator's `kill -9` |
| `serve_front_door()` | 3353-3381 | deferred writes first, then up to 64 fetches per tick |
| `refuse_everything()` | 3383-3399 | after stop: deferred/fetched -> `Stopped`; `pending_acks` -> `Stopped`; **paced -> `Ok`** (they were placed) |
| `serve(req) -> Result<Option<Request>>` | 3402-3486 | volume/offline/nvfail/range/size/admission checks; `Some(req)` hands it back (arena full or stalled) |
| `serve_write(req, vol, lba, blocks)` | 3488-3552 | section 4.1 |
| `serve_punch(req, vol, lba, blocks)` | 3556-3602 | section 4.1 |
| `acknowledge(req, vol, seq)` | 3607-3628 | windows = 2; precise mode + volume not in `durable_active` -> `pending_acks` (intent first); pacing -> `paced`; else `front.complete(req, Ok, seq)` |
| `serve_read`, `read_block`, `read_disk_block`, `read_entry` | 3630-3736 | index hit -> arena (`check_data` then `run`), hole -> zeros, else map |
| `handle_intent_done`, `retry_intent` | 3742-3810 | an intent landing completes the `pending_acks` of the volumes it covers |
| `maybe_cut()` | 3874-3929 | triggers: Shutdown, Request, ArenaHalf (`cfg.arena_half`), Timer, BackToBack; never while a frozen generation waits |
| `maybe_start_cp()`, `new_job(trigger, gen, run_state)` | 3941-4013 | a job lists every block of the generation as `StagedBlock { vol, lba, ring, seq }` and every hole; `max_seq` |
| `build_stripe` | 4475-4527 | reads `arena.descriptor(slot).crc32c` and `arena.run(ring, 1)`; a mismatch is `Event::ArenaDamage` and the block goes to disk under the recorded CRC |
| `step_commit`, `build_slot` | 5525-5691 | A then B, each FUA; `intent_seq_seen = intent_seq_landed`; `write_active` per section 7.6 windows; `durable_active.intersect_with(write_active)` when a slot is built, `union_with` when A lands |
| `finalize(prep)` | 5711-5995 | section 4.4 |
| `fn runs_of(placed: &[PlacedSlot]) -> Vec<(u32, u64, DirtyExtent)>` (free function) | 6143-6169 | groups consecutive placed slots of one write into extents; a hole is its own extent |

### 2.5 Boot (`crates/otap-cp/src/boot.rs`)

| Item | Line | Meaning |
|---|---|---|
| `BootError { NoSuperblock, NewerFormat, OldLayout, NoVerifiableCp{tried}, Io, Geometry }` | 50-80 | `Io` fails the boot to be retried; it never tears a candidate (E30) |
| `BootState { slot, intents: [IntentSlot; 2], map, segments, open_data, open_meta, torn, meta_live, protected, snapshots, table_regions, cps_since_fold, catalog, catalog_ref, catalog_damage }`; `rewound()` = `!torn.is_empty()` | 207-254 | |
| `pub fn evaluate(b, layout, agg_id) -> Result<BootState, BootError>` | 257-348 | ranks the ring, loads candidates newest first; a torn candidate is recorded and the next tried |
| `load_cp` | 577-772 | tables, map roots, deltas replayed, the CP's stripe list verified (data blocks too under a trusted opaque barrier after an unclean stop), open segments |

### 2.6 `otap-sys`

`hugetlb.rs`

| Item | Line | Meaning |
|---|---|---|
| `HugeArena::map_file(path, len, HugePageSize) -> io::Result<Self>` (Linux) | 99-103, 324-395 | opens with `create(true).truncate(false)` 338-344; `statfs`/`fstatfs` must say hugetlbfs of that page size; **exclusive non-blocking `flock`** 346-355 (`WouldBlock` -> "mapped by another arena"); **a file longer than `len` is refused** 356-367; a shorter one is grown with `set_len` 368-370; `MAP_SHARED` `map_mut` |
| `HugeArena::anonymous(len, page)` / `heap(len, page)` | 113-134 | private memory; the heap one is what `--heap-arena` and every non-Linux host use; **does not survive the process** |
| `as_slice`, `as_mut_slice`, `as_ptr`, `as_mut_ptr`, `lock`, `exclude_from_core_dumps` | 166-214 | |
| non-Linux `map_file` -> `Unsupported` | 480-493 | |

`ublk.rs`

| Item | Line | Meaning |
|---|---|---|
| `UBLK_CMD_START_USER_RECOVERY = 0x10`, `UBLK_CMD_END_USER_RECOVERY = 0x11`; ioctl forms `UBLK_U_CMD_START_USER_RECOVERY = 0xC020_7510`, `..END.. = 0xC020_7511` | 103-129 | encoded; nothing issues them yet |
| `UBLK_F_USER_RECOVERY = 1 << 3` ("the device survives its server's death and a new server re-attaches it"), `UBLK_F_USER_RECOVERY_REISSUE = 1 << 4` ("requests in flight at the server's death are reissued to the new server") | 280-286 | |
| `UBLK_S_DEV_DEAD/LIVE/QUIESCED = 0/1/2` | 318-324 | `QUIESCED`: the server died with `USER_RECOVERY` set |
| `UblksrvCtrlCmd { dev_id, queue_id, len, addr, data: [u64;1], .. }`: `START_DEV` and `END_USER_RECOVERY` carry the pid in `data[0]` | 400-425 | |
| `CtrlCommand::start_user_recovery(dev_id)`, `end_user_recovery(dev_id, pid)`, `get_dev_info(dev_id, buf)` (790), `stop_dev`, `del_dev` | 790-819 | builders exist |
| `IoCommand::fetch(q, tag)` ("issued once per tag when a queue starts (and again after START_USER_RECOVERY)"), `commit_and_fetch(q, tag, result)` | 847-876 | `addr = 0` always (USER_COPY) |
| `UblkControlRing::execute(&CtrlCommand, Option<&mut [u8]>) -> io::Result<i32>` | 1213-1276 | synchronous, one command at a time |
| `UblkQueue::new(chr, q_id, depth, entries)`, `desc(tag)`, `push_fetch(tag)`, `push_commit(tag, result)`, `submit()`, `submit_and_wait()`, `reap(&mut Vec<(u16, i32)>)`, `read_data(tag, off, dst)` | 1316-1470 | **`push_commit` only pushes an SQE; nothing reaches the kernel until `submit()`** |

### 2.7 `otap-ublk`

| Item | Line | Meaning |
|---|---|---|
| crate doc: "no recovery path for `UBLK_F_USER_RECOVERY` yet (the device is created with the flag; a tag whose fetch fails is retired; a new daemon re-attaching the queue is `dbladed`'s, Step 5)" | `lib.rs` 31-36 | **stale**: Step 5 did not do it (E38 "Not done"); it is Step 6's |
| `trait Transport { poll(&mut Vec<QueueEvent>) -> Result<usize, TransportError>; read_data(tag, off, dst); write_data(tag, off, src); commit(tag, result) }` | `frontdoor.rs` 62-76 | |
| `enum QueueEvent { Request{tag, desc: UblksrvIoDesc}, Abort{tag} }` | 29-42 | |
| `Watermarks { high, low }`, `for_depth(depth)` (stop at depth, resume at half) | 80-97 | |
| `FrontDoorStats { requests, reads, writes, flushes, discards, write_zeroes, refused, offline_rejected, stale, aborted, errors, gate_closed, commit_failures }` | 101-128 | |
| `enum TagState { Idle, Ready, InEngine, Aborted }`; `TagSlot { state, kind, gen: u32 }` | 131-147 | **`gen` is process memory: it restarts at 0 in a new daemon** |
| `decode_request(&desc, vol, size_blocks) -> Result<RequestKind, errno>` | 153-187 | |
| `errno_for(status, &kind) -> i32` | 192-206 | `Ok` -> bytes (0 for flush/punch); `NvFailed/Corrupt/Failed` -> `-EIO`; `NoSuchVolume/Stopped/Offline` -> `-ENODEV`; `NoSpace` -> `-ENOSPC`; `OutOfRange/TooLarge` -> `-EINVAL` |
| `request_id(q_id, tag, gen) = gen << 32 \| q_id << 16 \| tag`; `split_id` | 213-221 | |
| `UblkFrontDoor::new(transport, q_id, vol, size_blocks, depth, watermarks)` | 247-282 | one queue, one volume |
| `set_offline(bool)`, `in_engine()`, `ready()`, `stats()`, `transport()`, `last_error()`, `aborted_tags()` | 288-354 | |
| `pump() -> Result<usize, TransportError>` | 359-423 | `transport.poll` then decode: a request on a busy tag is answered `-EINVAL`; offline -> `-ENODEV`; abort of an `InEngine` tag bumps `gen` and decrements `in_engine` |
| `FrontDoor for UblkFrontDoor`: `fetch` 438-469 (**pumps only when `ready` is empty**), `copy_in` 471-486 (refuses a stale/aborted id with `Corrupt`), `copy_out` 488-497, `complete` 499-518 (stale check -> `Idle`, `gen += 1`, `in_engine -= 1`, `answer(tag, errno_for(..))`) | 437-519 | |
| `DeviceSpec::new(size_blocks, stripe_bytes, rotational)` (128 tags, 8 MiB requests, 1 GiB discards); `for_arena(slots)` caps `max_io_bytes` at half the arena | `device.rs` 22-67 | |
| `UblkDevice::create(&spec)`: `ADD_DEV` with **`UBLK_F_USER_COPY \| UBLK_F_USER_RECOVERY \| UBLK_F_CMD_IOCTL_ENCODE`** (no `REISSUE`) 95; `SET_PARAMS`; opens `/dev/ublkcN`; one queue; a fetch on every tag; `submit()` | 88-177 | |
| `start()` (`START_DEV` with the pid), `stop()`, `delete()`, `take_transport()`, `dev_id()`, `block_path()` | 184-247 | |
| `LinuxTransport::poll`: **`queue.submit()` first**, then `reap`; `UBLK_IO_RES_OK` -> `Request{tag, desc}`; anything else -> `Abort{tag}` | 283-306 | |
| `LinuxTransport::commit`: `push_commit`; on a full ring `submit()` and retry once | 324-340 | |
| `front_door(&mut device, vol, &spec)`, `serve(engine, stop)` | 344-382 | |
| `MockTransport::new(depth)`, `submit_write/read/flush/discard/write_zeroes(tag, ..)`, `submit_raw`, `abort(tag)`, `inject(event)`, `take_commits()`, `take_commit(tag)`, `read_payload(tag)`, `outstanding()`, `fail_copies/fail_commits/fail_next_poll` | `mock.rs` 39-162 | the mock keeps a write's payload until its commit; it does not keep the descriptor per tag and has no "server died" or "reissue" operation |

### 2.8 `otap-daemon`

| Item | Line | Meaning |
|---|---|---|
| `open_arena(&ArenaKind, bytes) -> Result<HugeArena, ControlError>` | `arena.rs` 10-56 | `Heap` -> `HugeArena::heap`; `Hugetlb{path}` -> `HugeArena::map_file` (the directory must exist) |
| `Pool::boot(&BootSpec, &SystemClock, stop) -> Result<Pool, ControlError>` | `pool.rs` 209-295 | probe superblock -> open backstore under the recorded geometry -> `open_arena` -> `slots_for_len` -> `PoolEngine::boot(cfg /* no volumes */, backstore, DaemonFrontDoor::new(), clock, memory)` -> `wait_until(is_serving)` -> `ensure_device(vol)` for every online volume |
| `Pool::note(&Event)` | 331-432 | formats `Booted` as "dirty boot at cp N, arena re-attached (B blocks replayed), N damaged arena slots dropped, ..." (the smoke test greps "arena re-attached") |
| `ensure_device(vol)` / `drop_device(vol)` | 678-739 | Linux: `crate::device::create_device` then `front_door().add_device`; teardown: `set_offline(true)`, wait `in_engine == 0`, `remove_device`, `destroy_device` |
| `Pool::stop()` | 840-863 | devices down, `engine.shutdown()`, wait `Stopped` |
| `DaemonFrontDoor { channel: ChannelFrontDoor, devices: BTreeMap<u32, VolumeDevice>, turn }` | `frontdoor.rs` 186-195 | `fetch` round-robins the devices then the channel (278-296); `copy_in`/`copy_out`/`complete` route by `req.vol()` unless the channel owns the id (bit 63) |
| `create_device(vol, size_blocks, stripe_bytes, rotational, arena_slots) -> (VolumeDevice, path)`; `destroy_device` | `device.rs` 12-56 | creates, opens the front door, `start()` |
| `Daemon::boot_configured`, `run` (ticks, answers one request between ticks, idle backoff 200 us..2 ms), `spawn`, `install_signal_flag` | `daemon.rs` 95-366 | |
| `DaemonConfig { control, device, arena_bytes, arena: ArenaKind, cp_interval_secs, overrides, ublk, allow_remote, op_timeout }` | `config.rs` 64-100 | |

### 2.9 `otap-sim` (what a Windows test of the recovery can drive)

| Item | Line | Meaning |
|---|---|---|
| `Sim::crash_daemon()` | `harness.rs` 1186-1196 | `record_stop(Daemon)`; `engine.into_parts()`; `disk.crash_daemon()` (every pending device op completes now, the completions are dropped: `backstore.rs` 273-279); `front.reset()` (the client forgets everything in flight); **the arena memory is kept** |
| `Sim::crash_power()` | 1169-1182 | the device tears, the client forgets, **fresh `HugeArena::anonymous`** |
| `Sim::boot()` | 386-462 | `Engine::boot` (retrying a `VolumeSizeMismatch` with the disk's size), reads `Event::Booted`, ticks to `is_serving` |
| `Sim::verify_recovery() -> Verdict { k, lost, nvfail, boot }` | 1261-1350 | `check_prefix` over the engine's image; after `Stop::Daemon` nothing acknowledged may be lost and nvfail must be unchanged; then `model.settle(k)` |
| `Model` rules | `model.rs` 13-45 | "an unacknowledged operation with `op > j` simply never happened ... one with `op <= j` was placed before the crash and lives on like an acknowledged one" |
| `Sim::submit_write/punch/read/flush`, `collect`, `wait`, `write`, `read`, `request_cp`, `settle`, `clear_nvfail`, `arena_slots`, `memory()` | 538-665, 864, 1354, 1371, 355 | |
| `SimFrontDoor::reset()` | `frontdoor.rs` 120-126 | |

---

## 3. Invariants and rules in force

Arena (E16, crate doc 1-74, code)

- A1. **Every slot transition is one aligned 8-byte store of the sealed word**, after the fields (`addr, seq, crc32c, kind`) and a `compiler_fence` (lib.rs 45-55, 1294-1299). A `kill -9` leaves every descriptor before or after a transition, never between. A multi-slot run transitions **slot by slot** (fill 1105-1113, `transition` 1231-1233), so a kill leaves a prefix in the new state and a suffix in the old.
- A2. `addr`, `seq`, `crc32c`, `kind` change only while the slot is `Free` (doc 338-339; `fill`/`fill_hole` require `Free`).
- A3. **`Placed` means "in the dirty index; may have been acknowledged"; `Filled` means "data and descriptor written; not yet in the index; never acknowledged"** (120-132; engine order in section 4.1). The arena carries no acknowledgement bit and no request identity.
- A4. Re-attach keeps every `Placed` slot whose descriptor word and data CRC verify (and, for a hole, whose record decodes), drops every `Filled` slot, turns the dead process's `Free` reservations into `Committed`, reports every slot it cannot trust, and **never reissues a sequence number** (`next_seq = max over every descriptor + 1`, 810, 932; pinned by `reattach_never_reissues_a_sequence_number`).
- A5. The header is sealed at creation and never rewritten; the attach counter is a self-checking word outside the seal (159-162, 943-947). Attach is **not read-only**: it rewrites words (933-935) and the attach counter; a second kill during the scan is harmless (idempotent).
- A6. Runs never wrap; the window `[tail, head)` bounds occupancy; nothing is scanned for garbage (19-43). A `Free` reservation on the tail's lap pins the window until it is discarded or re-attached (1274-1277; `a_reservation_never_filled_pins_the_window_until_a_reattach`).
- A7. **Single writer**: every method takes `&self`/`&mut self`; the engine is one thread (E16, E38 "One engine thread").
- A8. A slot holding a block the active generation superseded is released at once (`serve_write` 3546-3548; E16 "without that the ring fills with dead data"); at replay likewise (`assemble` 1839-1845).
- A9. `ArenaHeader::verify` checks the geometry against the memory's length (217-224): **an arena file mapped at another `--arena-bytes` than it was created with is `BadGeometry`, and `Engine::boot` then reformats it as lost** (1702-1708; see risk R2).

Index (E20, crate doc 25-38)

- I1. Within a generation **the later insert wins its whole range**; across generations the newer generation wins. `Index::insert` refuses a sequence below the volume's newest (`StaleSeq`); **equal is accepted** (a re-attach yields several pieces of one write with one seq). Insertion order and sequence order are therefore one order; a merged extent's seq is the newest write it holds.
- I2. A hole extent wins its range like data but a superseded hole releases nothing; its record slot is released when its generation's CP commits (`gen_holes`, E23).
- I3. Never cut from inside `with_active` (617-620).

Engine write path and acknowledgement (E17, section 4.1)

- W1. **Order per write: `reserve` -> `copy_in` -> CRC per block -> `next_seq` -> `fill` -> `index.insert` -> `place` -> release superseded -> `acknowledge`**. Placed strictly precedes the acknowledgement; `Filled` strictly precedes `Placed`.
- W2. An acknowledgement can be withheld after placement: precise mode waits for an intent (`pending_acks`), pacing delays it (`paced`), a CP commit releases `pending_acks` at or below its `max_seq` (5914-5922). A stopped engine answers paced writes `Ok` (they are placed, 3393-3398).
- W3. Writes and punches never pass each other in the engine's serving order; reads and flushes may pass a deferred write (3349-3352). A flush completes at once (3481-3484): "complete once every earlier acknowledged write of the volume is placed (which it already is)" (traits.rs 289-290).
- W4. Sequence numbers are in placement order and come from the arena (3525, 3575).
- W5. `FrontDoor::complete(.., Status::Ok, seq)` for a write is called from exactly four places: `acknowledge` 3627, `complete_paced` 2142, `handle_intent_done` 3767, `finalize` 5920; `refuse_everything` 3397 completes paced writes `Ok` at a stop. Every other `complete` is a refusal with a non-`Ok` status.

Boot and nvfail (E17, E30, lib.rs 51-72)

- B1. A boot is dirty when the selected CP is not `CleanShutdown` or the boot rewound (1767). Hold rule (1869-1870): `hold = dirty && (!arena_whole || rewound || arena_lost || orphaned_blocks > 0)` where `arena_whole = reattach.is_some() && !report.is_damaged()`. Held volumes: those in `boot_write_active(sb, intents)` that are present, not creating and not already held; cause `TornCp` after a rewind, else `DirtyBoot`.
- B2. **Replayed writes are inserted into the index whether or not the volume ends up held** (1834), so a held volume's replayed data is still flushed by the next CP; the hold only refuses requests until `nvfail_clear` lands.
- B3. The boot CP (`CpTrigger::Boot`) carries **no generation** (`new_job(CpTrigger::Boot, None, ..)` 1713); the replayed writes sit in the active generation until a normal trigger cuts it (Timer/ArenaHalf/Request/Shutdown). `shutdown_during_the_reattach_boot_commit_flushes_the_replayed_writes` pins that they reach the final CP.
- B4. Nothing is served until the boot commit's B slot lands (`serving = true` in `finalize` 5979-5983); the daemon waits on `is_serving` before creating devices (`pool.rs` 280).
- B5. The arena is re-attached **after** `boot::evaluate` and the volume-size checks (1665-1709); a boot refused after `attach` has already rewritten Filled/Free words and bumped the attach counter (harmless; see A5).

ublk front door (E28)

- U1. Request id = `gen << 32 | q_id << 16 | tag`; `gen` advances at every completion or abort so a stale completion is dropped (`stats.stale`). **`gen` lives in process memory only.**
- U2. A completion reaches the kernel only when `UblkQueue::submit()` runs, which `LinuxTransport` does at the start of `poll` (284) or when the SQ ring is full (328-337); `poll` runs from `pump`, which `fetch` calls **only when `ready` is empty** (439-441).
- U3. The device is created with `USER_RECOVERY` but not `USER_RECOVERY_REISSUE` (device.rs 95). E38 "Not done": a killed daemon's devices stay quiesced; the restarted daemon creates new ones; the old linger.

Test conventions (E09, docs/09)

- T1. Property tests use `proptest`; `prop_assert!` always carries a message; local gate `PROPTEST_CASES=10000 cargo test --workspace -j 3 -- --test-threads=6`; the documented merge gate is 100,000. Adversarial suites are never weakened to pass.

Where code and docs disagree

- D1. `otap-ublk/src/lib.rs` 34-36 says the recovery re-attach "is `dbladed`'s, Step 5"; E38 and `06` section 8 say it was not done and is Step 6's. The crate doc is stale.
- D2. E16's layout paragraph (07:343-350) describes the 32-byte descriptor without the `kind` word; the code is layout 1.2 with `kind` (E23 records the change). E16 is pre-1.2 wording.
- D3. E16 says the engine treats a damaged-slot report "like a power loss"; the code holds the write-active set but still replays every verified placed run (B2). The hold is as described; the replay is more than the sentence implies.
- D4. `docs/01` line 205 (brainstorm, not a ruling) already states the kernel fact the recovery design turns on: "`USER_RECOVERY` alone aborts in-flight requests; `USER_RECOVERY_REISSUE` requeues them and may issue a request twice." No ruling in `07` has adopted either flag's semantics yet.

---

## 4. How things happen, in order

### 4.1 A write, from fetch to acknowledgement (`engine.rs`)

1. `tick` 2109-2110: `serving && !stopped` -> `serve_front_door`.
2. `serve_front_door` 3354-3359 re-serves deferred writes first (FIFO); 3360-3374 fetches up to 64 requests; a write/punch behind a deferred one is deferred too (3364-3368).
3. `serve` 3402-3486: `NoSuchVolume` unless present and not creating; `Offline`; `NvFailed`; range -> `OutOfRange`; `blocks > 2048 || blocks > arena.slots()` -> `TooLarge`; `pool_full || !admits(blocks)` -> `NoSpace`; `arena.occupancy() >= stall_at` -> hand back (deferred, unplaced).
4. `serve_write` 3495-3506: `arena.reserve(blocks)`; `Full` -> hand back; `LapExhausted` -> `Failed`.
5. 3507-3516: `front.copy_in(&req, arena.run_mut(ring, blocks))`; on error `arena.discard(ring, blocks)` and `Failed` (slots become `Committed`, reusable).
6. 3517-3524: CRC-32C per 4 KiB block over the arena copy.
7. 3525: `seq = arena.next_seq()` (one per request, placement order).
8. 3526: `arena.fill(ring, vol, lba, seq, &crcs)`: per slot, fields then the sealed `Filled` word.
9. 3527-3542: `index.insert(vol, lba, DirtyExtent::data(ring, blocks, seq))`; `BadExtent`/`BadVol` -> `arena.discard` + `OutOfRange`; `StaleSeq` would be an internal error (cannot happen: seqs are monotone).
10. 3543: **`arena.place(ring, blocks)`** (per slot `Placed`).
11. 3546-3548: `arena.commit(old, old_blocks)` for every superseded run of the active generation.
12. 3549-3550: `stats.writes += 1`; **`acknowledge(req, vol, seq)`**.
13. `acknowledge` 3607-3628: `windows = 2`; precise mode and `!durable_active.get(vol)` -> `pending_acks.push((req, seq))`, `intent_wanted = true`, return (the ack comes from `handle_intent_done` 3762-3768 or `finalize` 5914-5922); else pacing delay -> `paced.push_back((req, seq, due))` (acked by `complete_paced` 2135-2144 at the next tick whose `mono_ns >= due`); else **`front.complete(req, Status::Ok, seq)`**.
14. In the ublk front door, `complete` 499-518 -> `answer` 426-434 -> `LinuxTransport::commit` 324-340 -> `UblkQueue::push_commit` (an SQE in the ring). The kernel sees it at the next `UblkQueue::submit()`: `LinuxTransport::poll` 284, which runs when the front door next pumps (`fetch` with `ready` empty), or when the SQ ring fills.

A punch (`serve_punch` 3556-3602) is the same with `reserve(1)`, `next_seq`, `fill_hole`, `index.insert(DirtyExtent::hole(ring, blocks, seq))`, `place(ring, 1)`, superseded data released, `gen_holes[gen].push(ring)`, `acknowledge`.

What the arena holds at a `kill -9`, by where the kill landed in this sequence:

| Kill between | Slots of this request | At re-attach | Client saw |
|---|---|---|---|
| 4 and 8 (reserved, copying, computing CRCs) | `Free` on the current lap | released (`Committed`), `note_pos` only | nothing |
| inside 8 (mid-fill) | prefix `Filled`, suffix `Free` | all dropped/released; `dropped_filled` counts the prefix | nothing |
| 8 and 10 (fill done, insert in progress) | all `Filled` | all dropped | nothing |
| inside 10 (mid-place) | prefix `Placed`, suffix `Filled` | **prefix replayed as a partial write**, suffix dropped | nothing (a torn, never-completed write; allowed) |
| 10 and 13's `complete` (placed, superseded release, ack bookkeeping) | all `Placed` | replayed | nothing |
| 13's `pending_acks`/`paced` hold | all `Placed` | replayed | nothing |
| after `complete` but before the next `submit()` (U2) | all `Placed` | replayed | nothing (the kernel never got the commit) |
| after `submit()` | all `Placed` (until a CP's `finalize` commits them) | replayed | completion delivered |

So: **`Placed` is a superset of "acknowledged"**, and from the arena alone the engine cannot tell which `Placed` runs were acknowledged. It also cannot tell a replayed run's original request length when the run was partially placed or partially superseded; it knows, per slot, `(vol, lba, seq, crc32c, kind)` and, grouped by `runs_of`, `(vol, lba, blocks, seq)` per contiguous piece. The per-block CRCs are in `Reattach.placed[i].crc32c` and stay readable through `arena.descriptor(slot)` while the slot is live.

### 4.2 `Engine::boot` (1637-1727)

1. 1655-1660: `check_flush_policy`, `cfg.validate()`.
2. 1661-1664: `DiskLayout::new(profile.geom)`.
3. 1665-1668: `boot::evaluate(&mut backstore, &layout, &cfg.agg_id)` -> `BootState` (the disk's CP, map, tables; `torn` non-empty means rewound).
4. 1672-1685: every configured volume's size must equal its root's (`VolumeSizeMismatch`).
5. 1686-1709: the arena:
   - `Arena::can_attach(&memory, &agg_id)` ok -> `Arena::attach` -> `(arena, Some(report), arena_lost = false)`; if `attach` itself fails (`Inconsistent`, the window wider than the ring) -> `Arena::create` on the same memory, `arena_lost = true`;
   - `WrongAggregate` -> the boot fails (another pool's arena is never overwritten);
   - any other refusal (`BadMagic` on blank memory, `BadCrc`, `BadGeometry`, `BadVersion`) -> `arena_lost = !is_blank(&memory)` and `Arena::create` (a power loss leaves blank memory: `arena_lost = false`).
6. 1710-1712: `assemble(..)` (4.3).
7. 1713-1726: `new_job(CpTrigger::Boot, None, RunState::Running)` and `start_job`; failure returns everything in `BootFailure`.
8. The caller ticks until `is_serving()`; `finalize` of the boot CP sets `serving` (5979-5983). `boot_retry` re-issues a failed boot commit (2099-2107).

### 4.3 `Arena::scan_on_attach` (798-949) and `Engine::assemble`'s replay (1814-1864)

The scan, per slot 0..slots (one pass, no I/O):
1. 810: `max_seq = max(max_seq, desc.seq)` **over every slot regardless of state** (so a reserved slot's previous occupant still counts).
2. 811-813: `valid = desc.verify()`; `pos = lap * slots + slot`.
3. `Free`: valid -> a dead reservation: `note_pos`, rewrite `Committed` (816-821). Invalid with a non-zero word that `sealed_as()` explains -> `Damage::Descriptor`, rewrite `Committed` (822-833). A zero word (never used) is ignored.
4. `Filled`: valid -> `dropped_filled += 1`, `note_pos`; invalid -> `Damage::Descriptor`; either way rewrite `Committed` (835-849).
5. `Placed`: invalid word -> `Damage::Descriptor`, `Committed`, skip (851-859); `note_pos`; data CRC mismatch -> `Damage::Data`, `Committed`, skip (861-869); `kind` `Data` -> `hole_blocks = 0`; `Hole` -> `HoleRecordBlock::decode` or `Damage::Kind`; undefined kind -> `Damage::Kind` (870-899); else push `PlacedSlot`, `min_placed = min(pos)` (900-908).
6. `Committed`: valid -> `committed += 1`, `note_pos`; invalid -> `Damage::Descriptor`, rewrite (910-922).
7. 925-931: `head = max_pos + 1` (or 0), `tail = min_placed` (or head); `head - tail > slots` -> `Inconsistent` (the arena is then recreated by `boot`, see 4.2 step 5).
8. 932: `next_seq = max_seq + 1`.
9. 933-936: apply the rewrites; `advance_tail`.
10. 937-942: **sort `placed` by `(seq, position)`**.
11. 943-947: attach count = stored + 1 (1 after a torn word); one 8-byte store.

The replay in `assemble` (after `vols` is built from the slot's rows and `cfg.volumes`, 1770-1812; `index = Index::new(cfg.ring, now)` 1814):
1. 1822: `for (vol, lba, extent) in runs_of(&report.placed)`: consecutive placed slots with the same `vol`, same `seq`, consecutive `lba`, same lap and consecutive slots become one `DirtyExtent::data`; each hole slot is one `DirtyExtent::hole(ring, hole_blocks, seq)` (6143-6169).
2. 1823-1833: a volume not present -> `orphaned_blocks += blocks`, `arena.commit(extent.ring, slots)` (released).
3. 1834-1850: `index.insert(vol, lba, extent)` -> windows = 2; every superseded run `arena.commit`ted; a hole's record slot pushed to `gen_holes[gen]`; `replayed_blocks += blocks`.
4. 1851-1861: an insert error (`StaleSeq` cannot happen because of the sort; `BadExtent` could for a run past the volume's size after a rewind/resize) -> orphaned, `Event::Internal`, released.
5. 1866-1885: the hold rule (B1); 1886-1891 `nvfail_now`; 1892-1897 `intent_seq` from the slot and the intent images.
6. 1986-2000: `Event::Booted { .. }` inserted first.

Order guarantee of the replay: writes are inserted in `(seq, position)` order, which is the dead daemon's placement order (W4), so I1 holds and the final index equals the dead daemon's active generation restricted to what was placed, with the superseded slots released.

### 4.4 `finalize` after the B slot lands (5711-5995): what releases arena slots

1. 5735-5738: map applied, segment table, catalogue.
2. 5869-5870: `cp_seq`, `cp_nonce`.
3. **5873-5882: `arena.commit(blk.ring, 1)` for every `StagedBlock` of the job (one block at a time)**; 5884-5893: `arena.commit(ring, 1)` for every hole record of the generation; 5894-5898: `index.retire(gen)`.
4. 5914-5922: `pending_acks` with `seq <= job.max_seq` are completed `Ok`.
5. 5971-5977: `Event::CpCommitted`; 5978-5993: `Boot` -> `serving = true`; `Shutdown` -> `stopped`, `Event::Stopped { clean: true }`.

A kill between the B landing and the end of step 3 leaves some slots of a durable generation still `Placed`: they are replayed at the next boot into the active generation and written again by the next CP (same data, a new `birth_cp` in the map; see risk R8).

### 4.5 The daemon's boot over a hugetlbfs arena (`pool.rs` 209-295)

1. 214-231: arena size and CP interval checked; the device must exist and hold a superblock (`probe_superblock`).
2. 233-240: `open_backstore` under the recorded geometry.
3. 241: `open_arena(&spec.arena, spec.arena_bytes)` -> `HugeArena::map_file(path, len)`: the file is re-opened without truncation, `flock`ed exclusively, grown if shorter, refused if longer.
4. 245-257: `PoolEngine::boot(EngineConfig::new(agg_id, vec![]), backstore, DaemonFrontDoor::new(), clock, memory)`.
5. 280: `wait_until("the boot commit", is_serving)`.
6. 281-293: `ensure_device(vol)` for every present, online volume -> `create_device` (`ADD_DEV`, a **new** `/dev/ublkbN`).

---

## 5. Hook points for Step 6 (scope A)

### 5.1 What the kernel does at the daemon's death (needed to place the hooks; verify against the saved 6.12 `ublk_drv.c`, not in this tree)

- With `UBLK_F_USER_RECOVERY` the device goes `UBLK_S_DEV_QUIESCED` instead of dying; the block queue is quiesced so new requests queue in the block layer; requests the dead daemon had fetched and not committed ("owned by the server") are, **without `REISSUE`, failed back to the block layer with an I/O error**, and **with `REISSUE`, requeued and dispatched again to the new server after `END_USER_RECOVERY`** (`docs/01` line 205 says the same). The driver forgets per-tag ownership at `START_USER_RECOVERY` ("queue reinit"), so a reissued request arrives as an ordinary fetch completion: **the daemon cannot tell a reissued request from a new one by the descriptor** (`UblksrvIoDesc` has op/flags, sectors, start, addr only). The requeue order is tag order, not issue order.
- The new server: open `/dev/ublk-control`, `START_USER_RECOVERY(dev_id)`; open `/dev/ublkcN`, mmap the descriptors (`UblkQueue::new`), `FETCH_REQ` on every tag, submit; `END_USER_RECOVERY(dev_id, pid)` (which, per the otap-sys doc 811-813, completes once a fetch is pending on every tag and resumes the device). All queue commands from the thread that issued the first fetch (E28), i.e. the engine thread (E38 "Devices").
- A commit the dead daemon had **pushed but not submitted** (U2) never reached the kernel: that request is "owned" and is reissued/failed like any other. A commit it had submitted completed the request; the kernel will not reissue it. There is no way for the new daemon to learn which commits were submitted.

### 5.2 Where the recovery plugs in, smallest coherent change

1. `crates/otap-ublk/src/device.rs`: add `UblkDevice::recover(dev_id: u32, spec: &DeviceSpec) -> io::Result<Self>`: `UblkControlRing::new`, `GET_DEV_INFO` (builder at ublk.rs 790; needs a `UblksrvCtrlDevInfo` buffer like `create` 98-108), check `state == UBLK_S_DEV_QUIESCED` and the flags, `execute(start_user_recovery(dev_id))`, `open_queue(spec)` (166-177 already pushes a fetch per tag and submits), `execute(end_user_recovery(dev_id, pid))`, `started = true`. `front_door(&mut device, vol, &spec)` (344-360) then works unchanged. Add a `probe(dev_id) -> Option<UblksrvCtrlDevInfo>` for the reaper and `otapctl device list`.
2. `crates/otap-daemon/src/pool.rs` `Pool::boot` step 6 / `ensure_device` 678-709: before `create_device`, look for a quiesced device of this volume (section 8, Q1 decides how) and call a `device::recover_device(vol, dev_id, ..)` wrapper instead; log "volume N re-attached /dev/ublkbN". Keep the order: **the engine must be serving (replay done, boot CP durable) before `END_USER_RECOVERY`**, so every reissued write is placed with a sequence number above every replayed one and `index.insert` never sees `StaleSeq`. Nothing in the engine needs to change for the basic flow: reissued requests enter through `DaemonFrontDoor::fetch` like any other.
3. `crates/otap-ublk/src/frontdoor.rs`: no change for the basic flow (tags start `Idle`, `gen` at 0; a reissued request is a normal `Request` event). Optional: a `recovering: bool` window (section 5.4).
4. If `REISSUE` is adopted: `UblkDevice::create` 95 adds `UBLK_F_USER_RECOVERY_REISSUE` (the flag is fixed at `ADD_DEV`; existing quiesced devices keep whatever they were created with).
5. The reaper (`otapctl device list/reap`): probe dev ids via `GET_DEV_INFO`, list state/pid/size, delete the ones that are `QUIESCED` with a dead `ublksrv_pid` and are not claimed by a booted pool (`STOP_DEV` then `DEL_DEV`; `UblkDevice::delete` 214-219 does both). Deleting a quiesced device fails its owned requests to the block layer; those writes were never acknowledged (A3), so nothing acknowledged is lost.

### 5.3 What the engine already guarantees to the recovery (no change needed)

- Replay happens in `assemble` before anything is served (B4); `next_seq` is above every seq the dead daemon used (A4); replayed extents are in the active generation (B3) and reachable by `index.lookup`/`read_block`.
- `Event::Booted.replayed_blocks/damaged_slots/orphaned_blocks/nvfail` tells the daemon what happened; `Pool::note` already logs it.

### 5.4 Optional: deduplicating a reissued write that the dead daemon had already placed

Not required for correctness (section 7.1). If wanted, the cheapest exact check at `serve_write`, after step 6 of 4.1 (the data and its CRCs are in hand) and before step 7:

- for each block `k`: `index.lookup(vol, lba + k)` -> `Hit { ring, seq, hole: false }`; `arena.descriptor(ring.slot)?.crc32c == crcs[k]`; then `arena.run(ring, 1)? == new_block` (a 4 KiB `memcmp`; CRC equality alone has a 2^-32 false-positive that would lose a write);
- all blocks match -> `arena.discard(ring, blocks)` the fresh run and `acknowledge(req, vol, max hit.seq)` without `fill`/`insert`/`place`;
- cost: `blocks` BTreeMap lookups (`log n`) + `blocks` descriptor reads + `blocks` memcmps, all in memory. The data must still be copied in first (the compare needs it), so the saving is one arena run's lifetime, not the copy.

This only matches the **newest** content per LBA. A reissued write whose copy was already superseded in the dead daemon's generation (W2 then W3 on the same LBA, both unacknowledged) does not match (W3's data is newest) and is re-placed, making W2 the winner; the `Reattach` report (every placed slot with its CRC) would be needed to match older copies, and `assemble` drops that report today (1820, 2001-2070): keeping it (or a `(vol, lba) -> Vec<(seq, crc)>` digest) for the recovery window is a small `Engine` field. Whether to bound the dedupe to a window after `END_USER_RECOVERY` (say, the first `depth` requests per queue, or until the first cut) or to leave it on always is a design choice (Q3); the engine cannot otherwise distinguish a reissue from a new write (5.1).

### 5.5 Request identity in the arena (a `(dev, tag, generation)` stamp)

- There is **no reserved space in the 32-byte descriptor**: `addr(8) + seq(8) + crc32c(4) + kind(4) + word(8)`. The only spare bits are the high 30 bits of `kind` (values 0 and 1 defined; anything else is `Damage::Kind` at attach, 890-898) and nothing else.
- Putting `(q_id, tag)` into `kind`'s high bits (e.g. `kind = SlotKind | (tag + 1) << 8 | q_id << 24`, 16 + 8 bits) and masking in `slot_kind()`/`is_hole()`/`decode` is a **layout 1.3 change** (`ARENA_VERSION = 0x0103`; the attach scan must accept 1.2 descriptors whose high bits are zero: the minor is not checked by `verify`, so old arenas attach; new arenas on old code would be `Damage::Kind` for every stamped slot, i.e. 1.2 code cannot read 1.3: acceptable before release if `07` records it). A `dev_id` (u32) does not fit; it is implied by the volume (one device per volume) through the device table of Q1. The ublk `gen` of U1 is process memory and **cannot** be stamped meaningfully; the kernel keeps the same tag across a requeue, so a reissued request's `(q_id, tag)` equals the stamp of its old copy, but tags are reused, so a stamp match must still be confirmed by data comparison (5.4). The stamp therefore narrows the candidate set to the runs with that tag (usually one) and lets the engine recognise "this reissued request is already placed, acknowledge it with its old seq and do not re-place", which keeps the dead daemon's ordering among concurrent writes (7.1). It is an optimisation of 5.4, not a prerequisite.
- The header has 4024 reserved bytes outside the seal (A5) that could hold a small self-checking device table (`vol -> dev_id`), but a `/run/opentap/<pool>/devices` file is as durable as the devices themselves (both vanish at reboot) and needs no layout change (Q1).
- `HoleRecordBlock` requires `rsvd0 == 0 && rsvd1 == 0` at decode (324-331), so a stamp in a hole record is also a 1.3 change; the descriptor stamp covers holes too (they have a descriptor).

### 5.6 What the change must not break

- A1/A2/A4/A5 (one-store transitions, fields only while `Free`, no seq reissue, header never rewritten). Any new descriptor field must be stored before the word and covered by the word's CRC (`word_for` hashes bytes 0..24: the `kind` word is inside, 404-405).
- I1: inserts in sequence order; equal seqs allowed.
- B4: no request served before the boot CP; the replay precedes `END_USER_RECOVERY`.
- W1: `Placed` before `complete`.
- `DeviceSpec::for_arena`: a recovered device keeps the `max_io_bytes` it was created with; the restarted daemon must map the same arena size (R2) or the engine answers `TooLarge` (`-EINVAL`) for requests the device still advertises.
- The 10,000-case gate and the existing crash tests (section 6); `reattach_after_any_interleaving_keeps_exactly_the_placed_slots` must keep passing for 1.2 arenas if 1.3 is introduced.

---

## 6. Test patterns to copy

1. **Arena re-attach under every kill point**: `crates/otap-arena/tests/adversarial_arena_tests.rs` 397-778. The model is `Vec<Run>` with states `Reserved/Filled/Placed`; `Op::Kill`, `KillMidFill(i, j)`, `KillMidPlace(i, j)`, `KillMidCommit(i, j)` take the memory with `into_memory()`, rewind the words of slots `j..` with `set_state(&mut mem, slot, state)` (92-96: `desc.word = desc.word_for(state, desc.lap())`, exactly what a kill before the store leaves), re-attach with `reattach()` (495-573), which checks `(slot, lap, vol, lba, seq, crc)` of every kept slot against the model, the `(seq, position)` order, `dropped_filled`, `peek_seq() > max_seq` and the window. `check()` 577-615 verifies no overlap, the window, and the data by CRC. Setup: `HugeArena::anonymous(2 MiB)` (507 slots), `Arena::create(memory(), agg(), 42)`. `proptest!` with `failure_persistence: None` and default cases (`PROPTEST_CASES` applies through proptest's own env handling; E09). Copy this for a layout 1.3 stamp (add the stamp to `Run` and to `expected`).
2. **Pinned single cases**: `arena_tests.rs::attach_keeps_placed_runs_and_drops_filled_ones` 170-217 (exact `placed` list, `peek_seq`, window, data survives); `damaged_data_is_caught_by_check_data_and_by_attach` 299-322 (flip a byte at `data_off + slot * SLOT_BYTES + 77`); `a_hole_record_fills_one_slot_and_survives_a_reattach` 404-447; adversarial 120-134 (damaged word), 137-154 (damaged address), 185-211 (no seq reissue), 218-245 (kill between fill and place), 310-326 (superseded slot inside a run not replayed), 366-391 (a run that skipped a lap end survives a kill).
3. **Engine-level daemon crash over the simulator**: `crates/otap-sim/tests/sim_tests.rs::a_daemon_crash_re_attaches_the_arena_and_loses_nothing` 196-227: `s.boot()`, writes, `s.request_cp()`, more writes, `s.crash_daemon()`, `s.boot()` -> `boot.reattached && boot.dirty && nvfail empty`, `s.verify_recovery()` -> `k` = every op; then again with the crash in `Phase::Staging` (`tick_until(|e| e.phase() == Phase::Staging)`). `step4_sim_tests.rs::daemon_crash_and_recover` 100-105 is the two-line helper; `punches_persist_through_daemon_crashes_and_power_cuts` 389 and `a_punch_over_a_write_is_not_a_loss_after_a_daemon_restart` 1190 cover holes.
4. **Direct engine rig with fault injection in the arena memory**: `adversarial_sim_tests.rs` `fresh_rig` 152-162 / `boot_rig` 123-131 (`Engine::boot` + ticks to serving) / `rig_wait` 192-206 / `placed_slot_data(&memory, vol, lba)` 208-218 (walks the descriptor table to find a `Placed` slot's data offset); `reattach_drops_a_damaged_slot_and_holds_its_volume` 465-501 does `e.into_parts()`, `disk.crash_daemon()`, flips a data byte, `boot_rig`, reads `Event::Booted.damaged_slots == 1`, expects `NvCause::DirtyBoot`, `Status::NvFailed`, `PeekSource::Unmapped`. `stall_boot_into(&mut s, phase)` 1325-1337 parks the boot commit in a phase (stalled `Schedule`).
5. **The engine behind the ublk front door on the mock transport**: `crates/otap-ublk/tests/engine_tests.rs` 1-165: `SimBackstore::new(profile(), 7)`, `UblkEngine::format`, `UblkFrontDoor::new(MockTransport::new(DEPTH), 0, vol, size, DEPTH, Watermarks::for_depth(DEPTH))`, `HugeArena::anonymous(2 << 20)`, `UblkEngine::boot`, `tick_until(is_serving)`; `write()` = `transport().submit_write(tag, lba, data)` + `wait(tag)` = tick until `transport().take_commit(tag)`; `cfg.pacing.max_delay_ns = 0` because the clock is virtual. `an_abort_mid_flight_drops_the_completion_and_keeps_the_engine_healthy` 357-420 fills the arena so writes are deferred in the engine, aborts one, and checks `stats().stale == 1` and the data. **A Windows test of the recovery flow needs two things the mock lacks**: (a) a "server died" operation that drops the `UblkFrontDoor` (tag states, `gen`) while the `MockTransport` keeps its `busy` tags and payloads, and (b) a "reissue" (re-push a `Request{tag, desc}` for every busy tag; the mock must start keeping each tag's `UblksrvIoDesc`, which `deliver` 58-66 does not) or an "abort all owned" (push `Abort{tag}` for each and complete them `-EIO`). Then: `engine.into_parts()` -> keep `(disk, memory)`, build a new front door over the same mock, `boot`, `tick_until(serving)`, mock reissues, and assert every tag commits with its byte count and the data reads back once.
6. **The sim harness for a reissue walk**: `Sim::crash_daemon` 1186-1196 calls `front.reset()` (the client forgets). A `crash_daemon_reissue()` variant would keep the in-flight `(ReqId -> op)` map and re-submit those ops after `boot()` with the **same op number and data** (`Model::data_of(op, ..)` is deterministic; `submit_write` 538-546 bumps `next_op`, so add a `resubmit(op, ..)`), in op order. `verify_recovery` then expects `k` to reach them. Caveat in Q5: the model's "newest issued op wins" rule is stricter than the block layer for overlapping unacknowledged ops reissued out of order.
7. **The daemon in process**: `crates/otap-daemon/tests/daemon_tests.rs::a_dirty_restart_holds_the_volume_under_nvfail_until_it_is_cleared` 663-721 (heap arena dies with the daemon -> dirty boot -> `nvfail` -> clear). There is no in-process hugetlbfs re-attach test (Windows cannot `map_file`); the hugetlbfs path is `scripts/linux-smoke.sh` 251-272 (SIGKILL, then `grep "arena re-attached"` and `state: online`).
8. **`PROPTEST_CASES`**: the simulator walks read it themselves: `fn proptest_cases(default: u32) -> u32 { std::env::var("PROPTEST_CASES").ok().and_then(|v| v.parse().ok()).unwrap_or(default) }` (`step4_sim_tests.rs` 1385-1392; `review_step5_sim_tests.rs` 912-918 as `cases`) and pass it as `ProptestConfig { failure_persistence: None, cases: proptest_cases(64), max_shrink_iters: 512, .. }`; the plain suites use `ProptestConfig::default()` and rely on proptest's own reading of the variable (08-engineering-log notes `contextualize_config` applies it over an explicit count anyway). `prop_assert!` always with a message (E09).

---

## 7. Risks, pitfalls and traps

### 7.1 The reissue analysis (the lead's questions, answered)

**Which writes are `Placed` after a kill, acknowledged or not.** See the table in 4.1: `Placed` = acknowledged ∪ placed-but-ack-withheld (`pending_acks`, `paced`) ∪ placed-but-`complete`-not-yet-called ∪ completed-but-commit-not-submitted (U2) ∪ the `Placed` prefix of a kill mid-place ∪ slots of a committed generation whose `finalize` loop was interrupted (4.4). **`Placed` is set before `FrontDoor::complete`** (3543 vs 3627). `Filled`-not-`Placed` = kill between `fill` 3526 and `place` 3543 (during `index.insert`): dropped at attach, never acknowledged, the slots released. Nothing in `Free` is ever replayed.

**What the engine knows about a replayed run.** Per slot `(vol, lba, seq, crc32c, kind/hole_blocks, ring)` from `Reattach.placed`; per contiguous piece `(vol, lba, blocks, seq)` from `runs_of`. Not known: the original request's length (a piece may be a prefix or a remainder), the acknowledgement, the tag.

**Is re-placing a reissued write safe.** Yes. The reissued request carries the same `(vol, lba, blocks)` and the same data (the kernel still holds the client's pages). `serve_write` places it at a fresh seq above every replayed seq (A4); `index.insert` makes it win its range (I1) and returns the old copy's slots, which are released; the content is unchanged. Ordering hazards:
- a write W' to the same LBA **issued after the reissued write W completed** cannot exist: W never completed (that is why it is reissued);
- a write W' to the same LBA that was **concurrently in flight** with W (the client had no completion for either, or had one for W' only): after recovery either may win, because the kernel re-dispatches owned requests in tag order and the new daemon places them in fetch order. The block layer gives a submitter no ordering between concurrent requests and a real disk can land an in-flight write after a concurrently issued one completed, so this is permitted; it is observable only by a client that issues overlapping writes without waiting, or that read in between (a read served by the dead daemon may have returned W' where the recovered image holds W). A stamp (5.5) or a dedupe against the full `Reattach` report (5.4) removes the anomaly for the pairs where both were placed, because then neither is re-placed and the arena's seq order stands; nothing removes it for a pair where only the later one was placed.

**What a dedupe needs and costs.** 5.4: `index.lookup` per block + the descriptor CRC + a memcmp against `arena.run(hit.ring, 1)`; cheap with the index as it is. It matches the newest content only; the superseded-copy case needs the `Reattach` report kept past `assemble`.

**Request identity in the arena.** 5.5: no reserved descriptor space; the `kind` word's high bits are the only room; a stamp is layout 1.3; it cannot carry the front door's `gen`; it must be confirmed by data comparison because tags are reused.

**Reads and flushes reissued.** A read is re-served from the recovered state; a flush completes at once (W3). Trivially safe.

**A discard / write-zeroes reissued.** A punch re-placed as a new hole record wins its range (I2); the old record slot stays until its generation's CP (harmless). A later client write over the range cannot have been issued after the punch's completion (none was delivered); a concurrent one falls under the same "either order" rule as above. Safe.

**The guarantee the recovery can make** (hugetlbfs arena re-attached whole, no rewind, device recovered after the boot CP):
> Every write or punch the dead daemon acknowledged is in the arena as `Placed`, verified by CRC, replayed into the active generation in the dead daemon's placement order before anything is served, and carried by the next consistency point: no acknowledged write is lost and no volume is held. Writes the dead daemon placed but had not acknowledged are kept as well (a client that got no completion may find its write done). Writes whose copy or placement was interrupted are dropped and their slots released; the client never heard of them. Under `UBLK_F_USER_RECOVERY` alone the kernel fails every request the dead daemon owned with an I/O error, which may include writes that in fact landed (allowed for a request that never completed). Under `UBLK_F_USER_RECOVERY_REISSUE` those requests are served again by the new daemon: a read or flush is re-served; a write or punch already placed is placed again with the same data and the same result, and one not yet placed is placed for the first time; the only observable difference from an uninterrupted run is the order among requests that were concurrently in flight and overlap, which the block layer never ordered.

The remaining risks are not in the arena: a damaged slot (B1 holds the write-active set and the replay keeps the rest), a rewind (held), a heap arena (lost: S5-1, `--heap-arena` is documented as a dirty boot), and a different `--arena-bytes` (R2).

### 7.2 Traps

- **R1. Commits reach the kernel late (U2).** `LinuxTransport::commit` only pushes; `submit()` happens in `poll`, which `fetch` calls only when `ready` is empty (439-441), or when the SQ ring fills. Under a steady stream with a long `ready` list a batch of completions can sit unsubmitted across ticks: a latency defect in its own right, and it widens the "placed, completed, not yet committed to the kernel" population that a kill turns into reissued/failed requests. Fix in scope A: submit at the end of each tick (or after `complete`) in `LinuxTransport`.
- **R2. The arena size is part of the arena's identity.** `HugeArena::map_file` grows a shorter file silently and refuses a longer one; `ArenaHeader::verify` compares `slots/desc_off/data_off` against the memory length; `Engine::boot` 1702-1708 treats `BadGeometry` as "not an arena", **reformats the memory and reports `arena_lost`** (every placed write discarded, the write-active set held). A restart with a different `--arena-bytes` is therefore a silent cache loss. `scripts/wsl2-prep.sh`/`linux-smoke.sh` and any systemd unit must pin the size; the daemon could refuse to recreate over a non-blank arena of the right magic but wrong geometry (open question Q6).
- **R3. The attach scan is O(arena data)**: `crc32c` over every `Placed` slot (861) plus a pass over every descriptor. An 8 GiB arena after a busy run is up to 8 GiB of CRC-32C before anything else, then `boot::evaluate` (the selected CP's stripes verified) and the boot CP's writes. The brief's 2-second recovery budget is at risk in a dev build and must be measured on Linux; the replay cannot be deferred (B4).
- **R4. Kill mid-place replays a partial write** (4.1 table). Never acknowledged, so allowed, but under plain `USER_RECOVERY` the client gets `EIO` for a write that partially landed; `REISSUE` heals it (the whole write is placed again and supersedes the prefix). The adversarial arena proptest models this (`KillMidPlace`, 717-740) and the engine relies on `runs_of` grouping the prefix.
- **R5. The front door's tag `gen` restarts at 0** in a new daemon (U1), so a request id can repeat an id the dead daemon used; harmless because every engine state died with it, but a log that correlates ids across restarts must not assume uniqueness.
- **R6. `UblkFrontDoor::pump` answers a request on a busy tag with `-EINVAL`** (394-400). After a recovery the tags start `Idle`, so reissued requests are accepted; but if the new daemon ever rebuilt a front door over a queue it had already fetched on, the second delivery would be refused: build the front door exactly once per recovered queue, on the engine thread.
- **R7. Replay inserts into a volume that is present but `creating`** (1823 checks `present` only); fine today (such a volume cannot have writes), but a volume-identity change across a crash (a deleted and recreated index, a rewind) makes `orphaned_blocks` or a `BadExtent` hold the set; the recovery must not assume `orphaned_blocks == 0`.
- **R8. Duplicate flush after an interrupted `finalize`** (4.4): slots of a durable generation still `Placed` are replayed and written again by the next CP with a new `birth_cp`. Correct for data; it makes a block look "born after the base" to a snapshot diff and to scope (C)'s delta walk although its content did not change (a spurious chunk). Not scope A's to fix; worth a line in the transport's expectations.
- **R9. The arena re-attach happens on the engine thread before the control server is told the boot result** (`Daemon::spawn` 246-250 reports `boot_configured`'s outcome after `Pool::boot` returned); a slow attach (R3) delays `Ping` too. Acceptable; just do not add the recovery's `END_USER_RECOVERY` wait before `is_serving`.
- **R10. `flock` on the arena file**: a second daemon (or a crashed one whose process still exists holding the fd) makes `map_file` fail with `WouldBlock` "mapped by another arena". The recovery path must wait for the old process to be gone (and `START_USER_RECOVERY` has the same dependency: the kernel must have seen the old server's char device released). A retry with a short backoff and a clear error is needed.
- **R11. The mock transport has no notion of a dead server**: `MockTransport::commit` on a tag it does not hold busy fails (215-220), and `deliver` asserts a tag is not already busy (64). A recovery test must model the kernel's view (busy tags survive the front door's death) explicitly, see section 6 item 5.
- **R12. `Engine::boot` with the daemon's empty `cfg.volumes`**: every present volume comes from the disk; a volume the dead daemon created whose creation CP never landed is simply absent (and had no writes: `NoSuchVolume` until `VolumeCreated`). A device for it may still be quiesced in the kernel: the reaper must handle a quiesced device that matches no present volume (delete it; its owned requests were never acknowledged).
- **R13. `otap-ublk/src/lib.rs` 34-36 is stale** (D1); fix the sentence when the recovery lands so the next reader is not told it is Step 5's.
- **R14. Not in the arena: the device's in-flight stripe writes.** `SimBackstore::crash_daemon` completes every pending device op at once (273-279); on Linux a stripe or discard in flight at the kill still lands after the new process starts (`06` section 8 "A discard in flight when the daemon is killed can still land"). The arena replay does not care (its data is in memory), but the boot CP's segment choice does; the Linux run must measure it.

---

## 8. Open questions for the lead

- **Q1. How does the restarted daemon know which quiesced ublk device belongs to which volume of which pool?** The daemon keeps no state (E38); `GET_DEV_INFO` gives state, pid, flags, depth, sizes, no name. Options: a `/run/opentap/<agg_id>/devices` file written at `create_device` (as durable as the devices: both vanish at reboot; no format change), the arena header's 4024 reserved bytes (layout 1.3), or matching by `dev_sectors` (ambiguous). The reaper's "stale" criterion depends on the answer.
- **Q2. Adopt `UBLK_F_USER_RECOVERY_REISSUE`?** Without it a kill turns every owned request into an `EIO` the client sees (a filesystem goes read-only on the first write error); with it the new daemon serves them again, which 7.1 shows is safe but reorders concurrent overlapping requests. The flag is fixed at `ADD_DEV`, so devices created before the change keep the old behaviour until recreated. (Kernel side to verify in the saved 6.12 source: that `REISSUE` is accepted together with `USER_COPY`, and what `ublk_queue_reinit` resets.)
- **Q3. Dedupe policy** (5.4): none (re-place every reissued write; simplest; a spurious second copy and a spurious re-flush per reissued write), newest-content dedupe (cheap; always on, or only in a recovery window), or the full `Reattach`-report match (keeps the dead daemon's ordering for placed pairs; needs the report kept past `assemble`). If a window: what ends it (the first cut? `depth` requests? a timer?).
- **Q4. Layout 1.3 with a `(q_id, tag)` stamp in `SlotDesc.kind`'s high bits** (5.5): worth it for exact "already placed" recognition, or defer? It is the only in-arena identity possible without widening the descriptor (a 40-byte descriptor would be a bigger 1.3).
- **Q5. The checker's ordering rule under reissue** (section 6 item 6): `Model::check_prefix` requires the newest issued op to win every block; two overlapping unacknowledged ops reissued in tag order can legitimately end the other way. Either the harness's reissue re-submits in op order and the engine serves in that order (true for the sim front door; not for the kernel), or the generator never leaves overlapping ops in flight at a crash, or the model is relaxed for ops unresolved at the crash. Which?
- **Q6. Arena size mismatch** (R2): should `Engine::boot` (or the daemon) refuse to recreate over memory whose header has the right magic and aggregate but the wrong geometry, instead of reformatting it as lost? Today only `WrongAggregate` refuses.
- **Q7. The 2-second budget** (R3): is it measured from `exec` to `END_USER_RECOVERY`, and does it include the arena CRC pass over an 8 GiB arena? If so, the budget may force a lazier verification (verify a slot's data on first read instead of at attach), which weakens A4's "every byte under a CRC before it is trusted" and would need a ruling.
- **Q8. Commit submission cadence** (R1): fix in `LinuxTransport` (submit after each tick's completions) as part of scope A, or leave for the reactor step? It changes nothing on Windows but changes the recovery's population (b).
