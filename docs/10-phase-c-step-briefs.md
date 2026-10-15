# OpenTAP Phase C step briefs

The owner's implementation briefs, saved verbatim (spacing and all) so the
mandate behind each step survives the session that received it. Each brief
is followed by a short note on how it was read: what was delivered against
it and where the departures are recorded. Add each new brief at the bottom.

---

## Step 1 brief (received 2026-10-13)

The message opened with "this work will be in the opentap file" and pasted
the following:

> Role Definition:You are Fable 5.1, the Lead Systems Architect and Engineering Director for OpenTAP. You command a specialized team of Opus 5.5 implementation agents. Our architectural brainstorming rounds are complete, and we are now moving to production Rust code.Objective: Step 1 (Workspace Scaffolding & otap-format)
> Your mandate is to generate the complete, compilable root workspace and the foundational otap-format crate. This crate defines the immutable on-disk binary layouts, page formats, superblock ring, stripe descriptors, and CRC32C verification routines for the Phase C (v0.1 Single-Node Foundation) storage engine.   Task Delegation to Opus 5.5 WorkersDirect three specialized Opus 5.5 sub-agents to implement the codebase:Opus-Storage-Core (Format Architect):Author the exact binary structs for the on-disk format in crates/otap-format/src/:superblock.rs: The 4-slot A0/A1/B0/B1 superblock ring (repr(C, align(4096))), tracking magic, ver, agg_id, owner_epoch, cp_seq, cut_seq, run_state (Running vs. CleanShutdown), write_active bitset, acked_bytes, and crc32c.   stripe.rs: Stripe summary header and per-block descriptors (16 bytes per 4 KiB block tracking vol_id, lba, birth_cp, and crc32c).   map.rs: Two-level radix map page definitions (16 KiB pages holding 1,024 physical block allocation entries: seg:28 | off:20 | plen:16, birth_cp, and crc32c).   stream.rs: 64-byte binary framing headers for the asynchronous replication stream (magic, ver, kind, flags, cp_nonce, chunk_idx, payload_crc, hdr_crc).   Opus-Kernel-Safety (Memory & Alignment Engineer):Ensure all binary structures are strictly #[repr(C)] or #[repr(C, align(4096))] with zero uninitialized padding bytes.   Implement zerocopy traits (FromBytes, IntoBytes, Immutable, KnownLayout) across all disk and wire structures.   Provide safe casting and slice-transmutation APIs so buffers can be written directly to O_DIRECT block devices without heap allocations.   Enforce #![forbid(unsafe_code)] throughout otap-format using pure zerocopy abstractions.   Opus-QA (Property & Invariant Tester):Implement unit and property-based tests in crates/otap-format/tests/ using proptest:Assert exact struct byte sizes using core::mem::size_of (Superblock == 4096 bytes, StripeDescriptor == 16 bytes, StreamHeader == 64 bytes).   Verify field offsets using core::mem::offset_of! to eliminate platform padding drift.   Round-trip serialization and deserialization across random bitstreams.   Checksum corruption tests verifying that single-bit flips fail CRC32C checks.   Strict Dependencies & StandardsRust Version: Stable 1.82+ (2021 Edition).Core Crates to Pin in Workspace Cargo.toml:zerocopy = { version = "0.8.62", features = ["derive"] }   crc-fast = "=1.10.0" (or crc32c = "0.6.8")   blake3 = "1.8.7"   proptest = "1.12.0" (dev-dependency)   io-uring = "0.7.15" (workspace declaration)   libublk = "=0.4.8" (workspace declaration)   rustix = "1.1.5" (workspace declaration)   memmap2 = "0.9.11" (workspace declaration)   Deliverables RequiredProvide complete, valid, non-truncated code for the following files:Cargo.toml (root workspace declaration).crates/otap-format/Cargo.toml.crates/otap-format/src/lib.rs.crates/otap-format/src/constants.rs.crates/otap-format/src/superblock.rs.crates/otap-format/src/stripe.rs.crates/otap-format/src/map.rs.crates/otap-format/src/stream.rs.crates/otap-format/src/checksum.rs.crates/otap-format/tests/format_tests.rs.Ensure all code compiles cleanly under cargo check and passes all cargo test invariant suites without warnings. Do not emit placeholder comments (e.g., // TODO: implement later)—write full production implementations for Step 1.

### How Step 1 was read

- Every listed file was delivered; `types.rs` was added beside them, and
  the crate root carries the layout specification.
- The brief's "Opus-Kernel-Safety" and "Opus-QA" roles ran as an independent
  read-only audit and an adversarial QA pass over the lead's code rather
  than as authors (`09-working-agreements.md`, section 1); the format
  itself was written by the lead for coherence.
- Departures, each recorded: the Rust floor is 1.89, not 1.82 (E01); the map
  packing is the round-3 `lseg:36 | off:16 | plen:12`, not the quoted
  `seg:28 | off:20 | plen:16` (F2); the superblock is a 4 KiB page plus a
  16 KiB volume table in one 20 KiB slot, because the per-volume arrays do
  not fit a page at 1,024 volumes (F1); the descriptor carries the block
  kind (F4); the rest of F3 to F10 settle points the brief left open.
- "Round-trip serialization across random bitstreams" became the
  property suites plus stored golden vectors with a BLAKE3 manifest and the
  N/N-1 decode lane (E05), after the audit found that vectors rebuilt from
  code prove nothing across versions.

---

## Step 2 brief (received 2026-10-13)

> You are Fable 5.1, the Lead Systems Architect and Engineering Director for OpenTAP. You command a specialized team of Opus 5.5 implementation agents. Step 1 (otap-format core primitives) is verified and passing 104 tests under MSRV 1.89 / Stable 1.99. You are now directing Step 2 of the Phase C implementation.
>
> Objective: Step 2 (Deferred Records, otap-stream, otap-sys Boundary, and Fuzzing)
> Your mandate is to complete the format specification, implement the asynchronous streaming engine crate (otap-stream), scaffold the Linux system call abstraction boundary (otap-sys), and implement cargo-fuzz targets for all on-wire and on-disk parsers.
> Task Delegation to Opus 5.5 Workers
>
> Direct four specialized Opus 5.5 sub-agents to implement the codebase:
>
>     Opus-Storage-Core (Format Completion):
>
>         Implement the deferred §7.1 records in crates/otap-format/src/:
>
>             records.rs:
>
>                 SegEntry: 32-byte segment tracking descriptor (lseg: u64, stripe_data_bytes: u32, live_bytes: u32, birth_cp: u32, lap: u32, kind: u8, state: u8, crc32c: u32).
>
>                 SnapRecord: 64-byte snapshot catalogue entry (snap_id: u64, snap_uuid: [u8; 16], map_root_pba: u64, created_wall_ns: u64, birth_cp: u32, kind: u8, flags: u8, crc32c: u32).
>
>                 CpLogRec: Logged extent delta entries (vol_id: u32, lba: u64, pba: u64, birth_cp: u32, plen: u16, crc32c: u32) enabling fast in-memory map reconstruction without rewriting full radix pages on every CP flush.
>
>         Re-export records in crates/otap-format/src/lib.rs and update const-assert size and offset verification tests.
>
>     Opus-Stream (Asynchronous Wire Engine):
>
>         Create crates/otap-stream:
>
>             frame.rs: High-level typed abstraction over otap_format::stream::StreamHeader (covering Hello, HelloAck, DeltaBegin, Chunk, DeltaEnd, Landed, Committed, Abort, Ping, Pong).
>
>             cursor.rs: Resumability tracker (Cursor, ResumeBitmap) maintaining the chunk landing index across disconnects.
>
>             codec.rs: Frame encoder/decoder enforcing zero-copy payload views (bytes::Bytes), header CRC-32C validation, and frame payload length limits (enforcing the 8 MiB maximum frame bound).
>
>             flow.rs: Receiver credit management window ensuring transmission pace aligns with local staging buffer limits.
>
>     Opus-Sys-Kernel (Linux FFI & System Boundary):
>
>         Create crates/otap-sys:
>
>             Isolate all OS-specific unsafe boundaries and Linux syscall interfaces:
>
>                 hugetlb.rs: Safe allocation, mapping, and locking of 2 MiB / 1 GiB hugetlbfs files (mmap, MAP_HUGETLB, MADV_DONTDUMP). Provides platform-agnostic fallback shims for non-Linux targets to preserve workspace compilation.
>
>                 ublk.rs: Command wrappers and structure mappings for /dev/ublk-control and /dev/ublkcN (UBLK_F_USER_COPY, UBLK_CMD_START_DEV, UBLKSRV_IO_BUF_OFFSET).
>
>                 uring.rs: Helpers for buffer registration and buffer table cloning (IORING_REGISTER_CLONE_BUFFERS).
>
>             Enforce minimal unsafe blocks, each documented with a strict // SAFETY: invariant proof.
>
>     Opus-QA-Fuzz (Adversarial Fuzzing & Property Verification):
>
>         Establish fuzz/ directory using libfuzzer-sys:
>
>             fuzz_targets/fuzz_superblock.rs: Mutates raw 20 KiB slot buffers to verify ranking logic and detect any parser panics.
>
>             fuzz_targets/fuzz_stripe_summary.rs: Fuzzes malformed stripe descriptors and hole records.
>
>             fuzz_targets/fuzz_stream_frame.rs: Fuzzes otap-stream frame demuxing against arbitrary byte streams.
>
>         Add property tests in crates/otap-stream/tests/ asserting stream reconnection, frame round-tripping, and chunk ordering invariance.
>
> Strict Dependencies & Standards
>
>     Workspace Root Updates: Add crates/otap-stream and crates/otap-sys to workspace.members in root Cargo.toml.
>
>     Crates:
>
>         bytes = "1.10"
>
>         libfuzzer-sys = "0.4" (fuzz target dependency)
>
>         rustix = "1.1.5"
>
>         memmap2 = "0.9.11"
>
>         zerocopy = "0.8.62"
>
>     Architecture Invariants:
>
>         otap-format remains #![forbid(unsafe_code)] with zero I/O dependencies.
>
>         otap-stream must not execute direct socket syscalls; it operates over memory buffers and generic byte sources/sinks.
>
>         All on-wire payloads must be validated against crc32c before exposure to upper layers.
>
> Deliverables Required
>
> Provide complete, valid, non-truncated code for:
>
>     Updated Cargo.toml (root workspace).
>
>     Updates to crates/otap-format/src/records.rs and crates/otap-format/src/lib.rs.
>
>     crates/otap-stream/Cargo.toml.
>
>     crates/otap-stream/src/lib.rs.
>
>     crates/otap-stream/src/frame.rs.
>
>     crates/otap-stream/src/cursor.rs.
>
>     crates/otap-stream/src/codec.rs.
>
>     crates/otap-stream/src/flow.rs.
>
>     crates/otap-sys/Cargo.toml.
>
>     crates/otap-sys/src/lib.rs.
>
>     crates/otap-sys/src/hugetlb.rs.
>
>     crates/otap-sys/src/ublk.rs.
>
>     fuzz/Cargo.toml and fuzz targets (fuzz_superblock.rs, fuzz_stream_frame.rs).
>
>     Comprehensive test suites validating cursor resumption, frame encoding, and record boundaries under cargo test --workspace.

### How Step 2 was read

- Every listed file was delivered, plus `crates/otap-sys/src/uring.rs` and
  `probe.rs`, `fuzz_targets/fuzz_stripe_summary.rs`, `stream_tests.rs` and
  the two adversarial suites.
- The record layouts depart from the field lists quoted above and are
  recorded as F11: `SegEntry` is keyed by its position in the segment table
  and carries `phys` plus the geometry at birth instead of a redundant
  `lseg`; `SnapRecord` adds the volume, the deadlist and the origin and
  keeps the name off disk; `CpLogRec` carries the packed map address rather
  than a separate `pba` and `plen`, so a replay is a copy into the map page.
- `VolEvent` and `Holes` frames were added to the ten kinds the brief lists,
  because the §7.3 contract and the frame header define twelve.
- The three architecture invariants hold and are pinned: `otap-format` has
  no I/O dependency and forbids `unsafe`; `otap-stream` has no socket code;
  every payload is CRC-checked before any typed decode (S1 to S10 in
  `07-engineering-decisions.md`).
- "Enforce minimal unsafe blocks" produced five, all Linux-only, listed in
  the `otap-sys` crate root (E08). The Linux paths are type-checked only on
  the Windows development PC.
- The fuzz targets exist and compile; running them needs nightly and
  cargo-fuzz, which the PC does not have.

---

## Step 3 brief (received 2026-10-13)

> You are Fable 5.1, the Lead Systems Architect and Engineering Director for OpenTAP. You command a specialized team of Opus 5.5 implementation agents. Step 2 is fully verified. We are now executing Step 3: The Engine Crates & Simulator.
>
> Objective:
> Implement the core storage engine (otap-arena, otap-index, otap-cp) strictly against abstract system traits (Backstore, FrontDoor, Clock, Net). Then, implement a deterministic simulator (otap-sim) to prove crash-consistency and the nvfail recovery logic across simulated power losses.
> Task Delegation to Opus 5.5 Workers
>
> Direct four specialized Opus 5.5 sub-agents to implement the codebase:
>
>     Opus-Storage-Arena (Memory & Slot Management):
>
>         Create crates/otap-arena.
>
>         Implement the Arena struct managing the 8 GiB hugetlbfs mapping (via otap-sys).
>
>         Build the bump allocator providing RingRef (slot + lap counter) to incoming writes.
>
>         Implement the SlotState page tracker (Free, Filled, Placed, Committed) to safely manage slot lifetimes without a garbage collector.
>
>     Opus-Storage-Index (Volatile BTree & The Cut):
>
>         Create crates/otap-index.
>
>         Implement the per-volume BTreeMap<u64, DirtyExtent> that coalesces random 4 KiB writes into contiguous ranges.
>
>         Implement the Epoch-Flip Cut Engine: the lock-free grace period mechanism that swaps the active and frozen generations in under 50 µs without stalling front-door I/O.
>
>     Opus-Storage-CP (The Flusher & State Machine):
>
>         Create crates/otap-cp.
>
>         Define the hardware-agnostic traits: Backstore (for disk I/O), FrontDoor (for ublk acknowledgements), and Clock.
>
>         Implement the Consistency Point (CP) flusher: pulling the frozen index, mapping extents to otap-format stripes, and writing them to the Backstore.
>
>         Implement the nvfail dirty-boot evaluator: reading the A0/A1/B0/B1 superblocks on boot, detecting torn CPs via CRC, and asserting nvfail if the run_state was not CleanShutdown.
>
>     Opus-Sim-Harness (Deterministic Testing):
>
>         Create crates/otap-sim.
>
>         Implement mock versions of Backstore and FrontDoor.
>
>         Build a proptest / madsim state machine that drives random I/O into the engine, triggers artificial kill -9 power cuts mid-CP, and verifies that upon reboot, the disk perfectly matches a seq-prefix of acknowledged writes.
>
> Strict Dependencies & Standards
>
>     Workspace Integration: Add the new crates to workspace.members in the root Cargo.toml.
>
>     Crates to leverage:
>
>         crossbeam-epoch (for the lock-free epoch flip).
>
>         parking_lot (for fast uncontended mutexes).
>
>         madsim or proptest (for the simulation harness).
>
>     Architecture Invariants:
>
>         The engine crates (otap-arena, otap-index, otap-cp) must compile on x86_64-pc-windows-gnu. They must not contain direct OS syscalls. All I/O must route through the traits or otap-sys.
>
>         The otap-cp flusher must assemble full, contiguous stripes in memory before handing them to the Backstore trait.
>
> Deliverables Required
>
> Provide complete, valid, non-truncated code for:
>
>     crates/otap-arena/Cargo.toml and crates/otap-arena/src/lib.rs.
>
>     crates/otap-index/Cargo.toml and crates/otap-index/src/lib.rs.
>
>     crates/otap-cp/Cargo.toml and crates/otap-cp/src/lib.rs (including the Backstore, FrontDoor, and Clock traits).
>
>     crates/otap-sim/Cargo.toml and crates/otap-sim/src/lib.rs (the simulation harness and mock implementations).
>
>     Comprehensive unit tests inside otap-sim proving that injecting a crash during a Backstore::commit correctly rolls back to the previous CP without data corruption.

### How Step 3 was read

- Every listed file was delivered, as crates with modules behind each
  `lib.rs` (`otap-cp` has `traits`, `layout`, `segments`, `blockmap`,
  `boot`, `engine`; `otap-sim` has the four fakes, the model, the harness
  and a seeded generator), plus `arena_tests.rs`, `index_tests.rs` and
  `sim_tests.rs`.
- The four worker roles ran as in Steps 1 and 2: the lead wrote the four
  crates for coherence, and an independent adversarial pass attacked the
  result (`09-working-agreements.md`, section 1).
- "Implement the Arena struct managing the 8 GiB hugetlbfs mapping": the
  arena takes any `HugeArena` (a hugetlbfs file on Linux, an aligned heap
  buffer in the simulator) and is sized by it; 8 GiB is the production
  default constant, 2 MiB the simulator's (E16).
- "crossbeam-epoch (for the lock-free epoch flip)": `arc-swap` was used
  instead, because crossbeam-epoch's reader path needs `unsafe` and only
  `otap-sys` may contain it (E15). The flip is still lock-free for writers
  and waits only for inserts in progress.
- "madsim or proptest": proptest plus a seeded fake device; madsim was not
  taken (E18).
- "assemble full, contiguous stripes in memory before handing them to the
  Backstore": every stripe image, summary page and data, is complete in
  memory before `write_stripe`; metadata regions are packed into whole
  stripes the same way (E17).
- "reading the A0/A1/B0/B1 superblocks on boot, detecting torn CPs via CRC":
  the boot ranks the ring, then also verifies the roots and every stripe of
  the selected CP by summary CRC and identity, and rewinds to the previous
  CP when any fails (E14); nvfail follows `run_state` as the brief says,
  with the arena re-attach as the exception that loses nothing.
- The "comprehensive unit tests inside otap-sim proving that injecting a
  crash during a Backstore::commit correctly rolls back" are
  `a_crash_during_the_commit_rolls_back_to_the_previous_cp` (the slot
  dropped, torn, or landed unreported, at both the `A` and the `B`
  location) and the random-crash property test.
- Nothing in the engine crates calls the operating system: the Linux
  target clippy gate passes for all four, and they run on the Windows
  development PC under the simulator.
- The independent adversarial pass (the "Opus-Sim-Harness" role's
  review half) added 68 tests and found 23 defects, recorded with their
  fixes in `08-engineering-log.md` (Step 3 entry, "Independent review")
  and `07-engineering-decisions.md` (E16 to E20). The review's files stay
  in the tree as `adversarial_*_tests.rs`, with the review's failing
  reproductions turned into regression tests.
- One item of the brief's "nvfail recovery logic" is deliberately not
  claimed: a flush-opaque backstore, where doc 05's N-2 fallback does not
  fit the two-CP ring of F7. The engine refuses the geometry and the owner
  decides between the two designs in E19 before Step 4 drives a RAID
  virtual drive.

---

## Step 4 brief (received 2026-10-13)

The message opened with "ok its been updated, time to move onto step 4" and
pasted the following:

> # OpenTAP Step 4 Implementation Brief: Radix Map, Geometry Discovery, Linux `io_uring` Backstore, and `ublk` Front Door
>
> **Role Definition:**
> You are Fable 5.1, Lead Systems Architect and Engineering Director for **OpenTAP**. Step 3 (`otap-arena`, `otap-index`, `otap-cp`, `otap-sim`) is complete, merged to `main`, and passing all 318 unit tests and the 10,000-case `proptest` gate. You are now directing **Step 4**.
>
> ---
>
> ## 1. Executive Rulings on Open Decisions (Record in `06` & Engineering Log)
>
> Before writing Step 4 code, record and enforce the project owner's binding rulings on the open architectural items:
>
> 1. **Decision E19 (Flush-Opaque Backstores & Hardware RAID Compatibility):**
>    * **Ruling:** OpenTAP MUST NOT hard-refuse flush-opaque backstores in production. A core requirement of OpenTAP is Ceph-like hardware agnosticism across Dell PERC, MegaRAID, HPE Smart Array virtual disks, `mdadm`, and raw drives.
>    * **Mechanism:** Introduce a `FlushPolicy` / `BackstoreCapabilities` configuration mode:
>      * Strict mode (`RequireFuaOrFlush`) remains available for strict verification.
>      * Standard/Homelab mode (`TrustBarrierWithNvfail`, default for hardware RAID / opaque LUNs) accepts devices that do not expose FUA or transparent cache reporting. It enforces a strict host-side barrier (`fdatasync` / `IORING_OP_FSYNC` completion) after stripe + map writes and *before* issuing the `A0/A1/B0/B1` superblock commit, emits a structured operator warning in the pool status, and relies on `nvfail` to quarantine volumes if an ungraceful power loss corrupts an un-flushed controller cache.
> 2. **Decision D1 / D2 / D8 / D9 / D10 Closure:**
>    * **D1 (Hardware Phasing):** Execute Phase C (Single-Node v0.1) $\rightarrow$ Phase B (Dual-Node internal drives with async SnapMirror delta shipping) $\rightarrow$ Phase A (Shared SAS shelf).
>    * **D2 (Degraded Durability):** Lock `unmirrored_flush = placed` as the default. Never throttle client writes to disk speeds when a partner or witness node is unreachable; acknowledge at DRAM speed and assert a degraded cluster health alert.
>    * **D8 (Protocol Rollout Order):** 1. iSCSI (`LIO` over `ublk`) $\rightarrow$ 2. NFSv3/v4.1 & SMB3 (in SVM `netns`/`utsns`) $\rightarrow$ 3. Fibre Channel (`tcm_qla2xxx`) & S3 (`versitygw`).
>    * **D9 (Provisioning & Trim Posture):** Volumes are thin-provisioned by default (`nvfail = on` by default). Segment reclamation uses whole-segment `BLKDISCARD` on flash/SSD backstores and sequential stripe overwrite without discard on rotational HDD / hardware RAID backstores.
>    * **D10 (Witness Placement):** 3rd Raft voter (Phase B/A) runs on the local LAN (VM, container, or Raspberry Pi) with `<100ms` RTT. Witness failure never degrades or throttles storage I/O.
>
> ---
>
> ## 2. Step 4 Objectives & Task Delegation
>
> Your mandate for Step 4 is to lift the single-segment metadata limit via `otap-map`, build hardware geometry discovery (`otap-geom`), implement the real Linux `io_uring` `Backstore` alongside cross-platform host compilation, and implement the `otap-ublk` front door.
>
> Direct four specialized Opus 5.5 worker agents:
>
> ### 1. Opus-Storage-Map (`crates/otap-map` & `otap-cp` Integration)
> * Create `crates/otap-map` (`#![forbid(unsafe_code)]`) implementing the persistent two-level Radix Map and `CpLogRec` delta journal defined in `otap-format`:
>   * **Two-Level Radix Tree:** Manage 16 KiB `MapPage` nodes (L1 root directory $\rightarrow$ L0 leaf pages of 1,024 packed `lseg:36 | off:16 | plen:12` entries) with Copy-on-Write dirty page tracking per CP.
>   * **Fast CP Logging (`CpLogPage` / `CpLogRec`):** Allow CPs to log incremental extent updates via `CpLogPage` records and periodically fold dirty leaf pages into full map segments, lifting Step 3's single-segment metadata limit.
>   * **Snapshot Pinning & `birth_cp` Walk:** Support pinning a volume's root at any CP into a `SnapRecord` and walking extents with `birth_cp > base_cp` to feed `otap-stream` delta shipping and segment live-byte accounting (`SegEntry::live_bytes`).
> * Wire `otap-map` into `otap-cp` so multi-segment pools and snapshots persist and recover cleanly across simulated crashes.
>
> ### 2. Opus-Storage-Geom (`crates/otap-geom` & Linux `IoUringBackstore`)
> * Create `crates/otap-geom` (`#![forbid(unsafe_code)]`):
>   * Inspect Linux sysfs queue parameters (`/sys/block/<dev>/queue/rotational`, `minimum_io_size`, `optimal_io_size`, `alignment_offset`, `discard_granularity`, `write_cache`, `fua`) with deterministic mock inputs for Windows/unit testing.
>   * Compute the optimal `StripeGeometry` (aligning stripe width and chunk size to hardware RAID or `mdadm` `optimal_io_size`, selecting 8 MiB flash segments vs. large rotational segments, and deriving the E19 `FlushPolicy`).
> * Implement the production Linux `Backstore` driver (`UringBackstore` in `otap-cp` or `otap-backstore` via `otap-sys`):
>   * Use `O_DIRECT` + `io_uring` batched vectored writes for stripes and map segments, `IORING_OP_FSYNC` (`FSYNC_DATASYNC`) barriers, and ordered `A` then `B` superblock slot writes.
>   * Keep Linux-specific execution gated behind `#[cfg(target_os = "linux")]` with full cross-target compilation (`x86_64-unknown-linux-gnu`) and portable file-backed fallback so `cargo test --workspace` continues to run on the Windows host.
>
> ### 3. Opus-Protocols-Ublk (`crates/otap-ublk`)
> * Create `crates/otap-ublk`:
>   * Implement the `FrontDoor` trait bridging incoming block requests (`Read`, `Write`, `Flush`, `Discard`, `WriteZeroes`) into the D-Blade engine (`otap-arena`, `otap-index`, `otap-cp`).
>   * Provide the Linux `ublk` runner (`#[cfg(target_os = "linux")]`) using `otap-sys::ublk` (`UBLK_F_USER_COPY`) and provide a platform-agnostic loop/channel driver so the entire `otap-ublk` command state machine (including `nvfail` `EIO` / offline rejection, read-hit from dirty arena vs. read-miss from `otap-map`/`Backstore`, and write backpressure watermarks) is 100% unit- and property-tested on Windows.
>
> ### 4. Opus-QA-Adversarial (Independent Audit & 10k Property Gate)
> * Perform an independent adversarial review across `otap-map`, `otap-geom`, `otap-ublk`, and the updated `otap-cp`.
> * Add `proptest` suites in `otap-sim` verifying:
>   * Multi-segment radix map persistence, `CpLogRec` replay, and snapshot read isolation across random power-loss crashes.
>   * Hybrid read path correctness (reading a 4 KiB range that partially overlaps the active volatile `otap-index`, the frozen flushing generation, and committed on-disk `otap-map` extents).
>   * E19 `TrustBarrierWithNvfail` behavior when the backstore caches writes up to the explicit `fdatasync` barrier.
>
> ---
>
> ## 3. Gates & Deliverables
> 1. All new and updated crates committed cleanly to the repository.
> 2. `cargo fmt --check`, `cargo clippy --workspace --all-targets -- -D warnings` (on both host Windows and `--target x86_64-unknown-linux-gnu`), `fuzz` clippy, and `cargo doc -D warnings` must be 100% clean.
> 3. `cargo test --workspace` and `PROPTEST_CASES=10000` must pass completely.
> 4. Update the status and engineering log documentation (`06`) with the E19, D1, D2, D8, D9, and D10 rulings and the Step 4 verification report.

### How Step 4 was read

- The rulings of section 1 were recorded first (E21 in
  `07-engineering-decisions.md`, the status in `06`, this log's Step 4
  entry) and the E19 part was built: `FlushPolicy` with the strict and the
  trust mode, the barrier where E17 already had it, the structured warning
  in the pool status, the deferred free, nvfail doing the quarantine.
  Nothing in D1, D8 or D10 has code yet; D2's "ack at DRAM speed" is how
  the engine has always completed a flush; D9's trim posture is the
  `DiscardPolicy`.
- "Two-Level Radix Tree: 16 KiB `MapPage` nodes": the format's `MapPage`
  (Step 2, F8) is 1,024 packed entries of `lseg:36 | off:16 | plen:12`
  with a birth each, and the index above it one `PageIndexEntry` per page;
  the page size is the format's, not the brief's figure. Copy-on-write
  dirty page tracking per CP is the fold (E22): between folds the CP log
  carries the changes and an in-memory overlay answers lookups, which is
  the "fast CP logging" the brief asks for.
- "pinning a volume's root at any CP into a `SnapRecord`": at any *fold*
  CP (the engine makes the snapshot's CP a fold), because a root is
  complete only then (E25). The `birth_cp > base_cp` walk is `diff`,
  `live_diff` and `snapshot_diff`; `SegEntry::live_bytes` is kept as
  `data_live` and `meta_live` in the segment table (E24).
- "`UringBackstore` in `otap-cp` or `otap-backstore`": its own crate, with
  the ring in `otap-sys` (E27), so `otap-cp` stays free of `unsafe` and
  of Linux types. "Batched vectored writes": the ring batches submissions
  and stages each write in an aligned slot; one write per stripe, which
  is already the engine's unit, rather than `writev`.
- "deriving the E19 `FlushPolicy`": a hardware RAID virtual disk is
  flush-opaque and gets the trust policy; everything else the strict one;
  an operator override exists (E26).
- "platform-agnostic loop/channel driver": the `Transport` trait and its
  `MockTransport` (E28); the state machine never names a ring, so every
  path, including nvfail `EIO`, offline `ENODEV`, the read hit from the
  arena against the read miss from the map, the watermarks, aborts and
  stale completions, runs under unit and property tests on the Windows
  PC and behind the engine over the simulator's device.
- "Opus-QA-Adversarial": the independent review ran as in Step 3 (a
  worker with its own test files, the lead porting its `bug_*` tests and
  fixing); its findings are in this log's Step 4 entry. The brief's three
  proptest subjects are `step4_sim_tests.rs` and the extended random walk
  of `adversarial_sim_tests.rs`.
- The gates of section 3 were run as listed in `06`, section 5; the
  Linux halves (`otap-sys::block`, the ublk device module,
  `UringBackstore`, `Sysfs`) are type-checked for
  `x86_64-unknown-linux-gnu` and have not executed, which the owner knows
  from Step 2 (no Linux host).

---

## Step 5 brief (received 2026-10-14)

The message pasted the following:

> # OpenTAP Step 5 Implementation Brief: Snapshot Deletion, Segment Cleaner (GC), `dbladed` Daemon, and `otapctl` CLI
>
> **Role Definition:**
> You are Fable 5.1, Lead Systems Architect and Engineering Director for **OpenTAP**. Step 4 (`otap-map`, `otap-geom`, `otap-backstore`, `otap-ublk`) is committed on `main` (`f4dc64e`) with 458 passing unit tests and a 10,000-case property-test gate. You are now directing **Step 5**.
>
> ---
>
> ## 1. Executive Rulings on Open Decisions (D3, D5, D6, D14)
>
> Record these binding project-owner rulings in the decision log (`07`) and status document (`06`) before writing Step 5 code:
>
> 1. **D3 (Arena Sizing, Hugepage Fallback & CP Cadence):**
>    * Production default remains an 8 GiB `hugetlbfs` arena (2 MiB pages), but `dbladed` MUST support configurable arena sizes down to 64 MiB and a `--heap-arena` flag (using aligned heap memory) so homelab operators and CI VMs can run without hugepage reservation.
>    * Default CP timer is **5 seconds** (configurable 1–15s) or **50% active generation fullness**, with backpressure pacing at **80%** and hard stall at **95%** arena fullness.
>    * Should `D3` in your internal ledger also track any remaining OS/fabric baseline item, lock Debian 13 (Trixie, kernel 6.12+) as the Linux target baseline and keep cross-platform Windows host testing mandatory.
> 2. **D5 (Snapshot Deletion, Retention & Replication Pinning):**
>    * Support up to 255 user snapshots per volume plus reserved slots for active replication baselines.
>    * **Snapshot Deletion (`snap_delete`):** Deleting a snapshot unpins its `SnapRecord` in the next CP and accurately decrements `SegEntry::live_bytes` across unshared extents (comparing against adjacent older/newer snapshots or the active volume root) without corrupting shared Copy-on-Write pages.
>    * **Replication Lock:** Snapshots flagged as an active replication baseline (`ReplicationBase`) MUST refuse user deletion with a clear error until explicitly unpinned or superseded by a newer `Committed` replication cursor.
> 3. **D6 (Segment Cleaner / Garbage Collection & Reserve Slack):**
>    * **Deadlock-Free Reserve:** Reserve a hard floor of free segments (`max(8, total_segments / 12)`) exclusively for cleaner compaction and metadata folds so a 100%-full user pool can still commit CPs to delete snapshots, punch holes (`Discard`), or compact fragmented segments.
>    * **Cleaner Policy:** Background cleaning activates above **70% segment utilization** (or on explicit `otapctl pool scrub/clean`). Select candidate `Data` segments via cost-benefit scoring (lowest `live_bytes`, breaking ties by oldest `birth_cp`).
>    * **Crash-Safe Relocation:** Relocate still-live blocks only if the volume/LBA currently maps to the exact physical `(lseg, off)` being cleaned (never overwriting newer random writes that landed since the segment was written), commit the remapped extents through the normal CP pipeline, and transition the drained segment to `Free` (issuing whole-segment discard on flash policies and direct overwrite on rotational policies).
> 4. **D14 (`dbladed` Daemon Lifecycle, Control IPC & `otapctl`):**
>    * Build `dbladed` and `otapctl` so the entire management lifecycle (`pool format`, `pool status`, `vol create/resize/delete/nvfail-clear`, `snap create/list/delete/rollback`, `cp trigger`, `gc run`) works end-to-end on both Linux (`UringBackstore` + `ublk`) and Windows/portable test harnesses (`PortableFileBackstore` + in-process/TCP control channel).
>    * Persist human-readable volume and snapshot names in a CRC-protected catalogue volume/record so names survive reboots and crashes.
>
> *(Note: If your internal numbering in `05`/`06` assigns any adjacent scope to D3, D5, D6, or D14, map the corresponding ruling above and document the mapping cleanly in `07`.)*
>
> ---
>
> ## 2. Step 5 Objectives & Task Delegation
>
> Direct four specialized Opus 5.5 worker agents to deliver Step 5:
>
> ### 1. Opus-Storage-Lifecycle (Snapshot Deletion, Rollback & Segment Accounting)
> * Extend `otap-map` and `otap-cp` with full **Snapshot Deletion** and **Volume Rollback**:
>   * Accurately track extent sharing between the live volume tree and pinned snapshots so deleting a snapshot reclaims only extents that are neither referenced by an older snapshot nor still live in a newer snapshot/head.
>   * Implement volume rollback to a pinned snapshot (`snap_rollback`), safely discarding newer unshared extents and updating `SegEntry::live_bytes`.
>   * Enforce the `ReplicationBase` pin guard so active SnapMirror baselines cannot be accidentally deleted.
>
> ### 2. Opus-Storage-Cleaner (`otap-gc` or Cleaner Module in `otap-cp`)
> * Implement the **Segment Cleaner (Garbage Collector)**:
>   * Reserve the deadlock-free segment slack (`reserve_segments`) so `PoolFull` applies to front-door user writes before exhausting internal CP/cleaner segments.
>   * Implement background and on-demand segment compaction: scan candidate segments with low `live_bytes`, verify live ownership via the stripe summary and current radix map, rewrite live blocks into the next CP stripe without clobbering newer writes, and reclaim drained segments after the CP commits.
>   * Prove that a pool filled to 95% with fragmented 4 KiB overwrites and deleted snapshots reclaims space and sustains writes indefinitely without wedging.
>
> ### 3. Opus-Daemon-CLI (`bins/dbladed` & `bins/otapctl` or `crates/otap-daemon` & `crates/otap-ctl`)
> * Create the daemon and CLI crates/binaries (`#![forbid(unsafe_code)]`):
>   * **`dbladed`:** Hosts the pool engine, runs the periodic CP ticker and background cleaner, exposes `otap-ublk` devices on Linux, and listens on a control endpoint (Unix socket on Linux, loopback TCP / channel in portable mode). Handles graceful `SIGTERM`/`SIGINT` shutdown (`CleanShutdown` superblock commit) so planned restarts never trigger `nvfail`.
>   * **`otapctl`:** Clean ONTAP-style CLI supporting:
>     * `pool format`, `pool boot`, `pool status`, `pool sync`, `pool clean`
>     * `volume create`, `volume list`, `volume delete`, `volume online`, `volume nvfail-clear`
>     * `snapshot create`, `snapshot list`, `snapshot delete`, `snapshot rollback`
>   * Provide a portable integration harness (`scripts/linux-smoke.sh` for the upcoming Debian 13 VM + automated Rust integration tests that run on Windows today).
>
> ### 4. Opus-QA-Adversarial (Independent Audit & 10k Property Gate)
> * Perform an independent adversarial review across snapshot deletion, segment cleaning, reserve accounting, and daemon control handling.
> * Extend `otap-sim` with property tests (`PROPTEST_CASES=10000`) proving:
>   * **GC + Crash Invariance:** Injecting random power cuts (`kill -9` / torn writes) mid-compaction and mid-snapshot-deletion never loses live data, never resurrects deleted data, and never leaks `live_bytes` accounting after reboot.
>   * **100%-Full Pool Recovery:** A pool driven to 100% user capacity can still delete a snapshot or punch holes, run the cleaner, and resume accepting writes without wedging on boot or during a CP.
>
> ---
>
> ## 3. Gates & Deliverables
> 1. All new and updated crates/binaries committed and pushed to `main`.
> 2. `cargo fmt --check`, `cargo clippy --workspace --all-targets -- -D warnings` (on both Windows host and `--target x86_64-unknown-linux-gnu`), `fuzz` clippy, and `cargo doc -D warnings` 100% clean.
> 3. `cargo test --workspace` and `PROPTEST_CASES=10000` passing completely.
> 4. Update `06` (status), `07` (decisions), `08` (engineering log), and `README.md` with the Step 5 report and exact instructions for running the first live Linux smoke test on Debian 13.

### How Step 5 was read

- The four rulings of section 1 were recorded first (E34 in
  `07-engineering-decisions.md`, with the mapping the brief's note asks
  for: the labels D3, D5, D6 and D14 belong to other items in Part 9, so
  the rulings are S5-1 to S5-4 there and in `06`), and built as E35 to
  E38.
- "Direct four specialized Opus 5.5 worker agents": the lead built the
  three construction scopes itself, in order (lifecycle, cleaner, daemon
  and CLI), because all three change one file (`otap-cp/src/engine.rs`,
  the CP's `prepare_meta` and `finalize`) and the format at once, and
  three agents editing the same functions would have spent the step
  merging. The fourth scope, the adversarial review, ran as an
  independent worker with its own test files and its own report
  (`docs/reviews/2026-10-14-step5-qa-adversarial.md`), as in Steps 3 and
  4; its findings and what was done about them are in the Step 5 entry
  of `08-engineering-log.md`.
- "D3": the arena bounds, `--heap-arena`, the 1 to 15 s timer and the 80%
  and 95% pacing points are `otap-control`'s `units`, `dbladed`'s
  command line and `Pacing` (E37). "50% active generation fullness" was
  already `EngineConfig::arena_half`. Debian 13 is the smoke script's
  target; the Windows host runs every suite.
- "D5": `MAX_USER_SNAPSHOTS` 255 and `MAX_RESERVED_SNAPSHOTS` 16; the
  deletion compares against the adjacent older and newer snapshot or the
  live root, as written, and against every other remaining tree for
  pages (E36). "Superseded by a newer Committed replication cursor" is
  `set_replication_base` moving the pin to the newer base; the cursor
  itself (the replication stream's `Committed`) is Phase B's.
- "D6": `reserve_for` is the brief's formula with a cap at half the pool
  so a tiny test pool keeps some data segments; "`otapctl pool
  scrub/clean`" is `pool clean`; candidates, the exact-address check and
  the drained segment's fate are as written (E37). "Cost-benefit scoring"
  is the ordering by live blocks then age; no age-weighted formula was
  added.
- "D14": `vol`, `snap`, `cp trigger` and `gc run` are `volume`,
  `snapshot`, `pool sync` and `pool clean`, and the surface grew (`pool
  boot|stop`, `daemon ping|stop`, `volume show|offline|rename`,
  `snapshot pin|unpin|replication-base`, `io write|read|punch`) because
  the lifecycle needs them. "`PortableFileBackstore`" is Step 4's
  `FileBackstore`. "In-process/TCP control channel": the daemon's
  `handle` in process and loopback TCP, both tested on Windows; the Unix
  socket is Linux-only code. The "CRC-protected catalogue volume/record"
  is a record region, not a volume (F16, E35).
- "Opus-Storage-Lifecycle": `snap_rollback` requires the volume offline
  and quiet, which the brief did not say; a rollback of a volume that is
  taking writes has no defined content, so the daemon takes the volume
  offline, flushes it, rolls it back and brings it back (E36, E38).
  "Safely discarding newer unshared extents" is the `released` list of
  the fold.
- "Opus-Storage-Cleaner (`otap-gc` or Cleaner Module in `otap-cp`)": a
  module (`otap_cp::cleaner`) plus the engine's driver; "verify live
  ownership via the stripe summary and current radix map" reads the
  segment's summaries and checks the live map and every snapshot; the
  95%-fragmented proof is `the_cleaner_reclaims_a_fragmented_pool_so_writes_never_stop`
  and `a_pool_full_for_users_still_punches_deletes_and_cleans_then_serves_writes_again`.
- "Opus-Daemon-CLI (`bins/` or `crates/`)": `crates/otap-daemon` and
  `crates/otap-ctl`, with `otap-control` between them for the protocol
  so the CLI does not depend on the daemon. `scripts/linux-smoke.sh` is
  written against Debian 13 and has not run (no Linux host); the Rust
  integration tests run on Windows today, as asked.
- "Opus-QA-Adversarial": the review's files, ported in place as
  `review_step5_sim_tests.rs`, `review_daemon_tests.rs` and
  `review_control_tests.rs`, and its report; the two property tests the
  brief names are there (`gc_and_crash_invariance`,
  `a_full_pool_recovers`) and in the lead's `step5_sim_tests.rs`. The
  review made thirteen findings: twelve defects, four of them losing or
  exposing data, all fixed and pinned, and one cited item (the walks' case
  count) that proved not to be a defect (E39).
- The gates of section 3 were run as listed in `06`, section 5, the
  10,000-case run in the background with its timing recorded; the Linux
  halves are type-checked for `x86_64-unknown-linux-gnu` and have not
  executed.

## Step 6 brief (received 2026-10-14)

The owner pasted the brief into the session; the paste ended mid-sentence
in the "Dual Transport Backends" list of the third worker (at "1"), so
everything after that point (the rest of the transport scope, the fourth
worker, and any later sections) did not reach the lead. The text below is
what arrived, verbatim; the note at the end of this entry says how the
missing part was read.

> # OpenTAP Step 6 Implementation Brief: `USER_RECOVERY` Re-Attach, Incremental Catalogue & Snapshot Tables, Pool Attestation, and Replication Transport (Windows Host Execution)
>
> **Role Definition:**
> You are Fable 5.1, Lead Systems Architect and Engineering Director for **OpenTAP**. Step 5 (`5114866`) is committed on `main` with 541 passing unit tests and a 179-minute, 10,000-case property-test gate. You are now directing **Step 6**.
>
> **Execution Environment Directive (Windows Today, WSL 2.0 Tomorrow):**
> The project owner is running this session on the **Windows host** today and will install **WSL 2.0** tomorrow for live Linux kernel execution. Therefore:
> 1. **Build and verify 100% of the Step 6 code on Windows today** using `PortableFileBackstore`, the mock `ublk` transport, the `otap-sim` deterministic harness, and cross-platform loopback TCP sockets, while keeping all Linux-specific syscalls (`io_uring`, `/dev/ublk-control`, `hugetlbfs`) cleanly gated and type-checked under `--target x86_64-unknown-linux-gnu`.
> 2. **Prepare WSL 2.0 tooling (`scripts/wsl2-prep.sh` and updated `scripts/linux-smoke.sh`)** so tomorrow's Linux test run handles WSL 2 nuances cleanly (specifically probing for `io_uring`, `hugetlbfs`, and `/dev/ublk-control`, running available Linux kernel suites immediately, and providing a reproducible script to compile a Linux 6.12 WSL 2 kernel with `CONFIG_BLK_DEV_UBLK=y` if the stock Microsoft WSL2 kernel lacks `ublk_drv`).
>
> ---
>
> ## 1. Step 6 Objectives & Task Delegation
>
> Direct four specialized Opus 5.5 worker agents to deliver the complete Step 6 feature set:
>
> ### 1. Opus-Ublk-Recovery (`UBLK_F_USER_RECOVERY` Re-Attach & Stale Device Reaper)
> * Extend `otap-sys::ublk`, `otap-ublk`, and `dbladed`:
>   * Implement the full `UBLK_F_USER_RECOVERY` lifecycle (`UBLK_CMD_START_USER_RECOVERY` and `UBLK_CMD_END_USER_RECOVERY` in `otap-sys`, state machine in `otap-ublk`).
>   * Coordinate daemon restart (`kill -9` recovery) with `otap-arena` re-attachment: when `dbladed` restarts over an existing persistent arena and surviving `ublk` device, recover placed arena runs, reconcile in-flight `ublk` I/O tags without duplicating or dropping acknowledged writes, and resume serving within the 2-second recovery budget.
>   * Implement the **Stale Device Reaper**: detect and clean up orphaned/wedged `ublk` device instances left behind by dead daemons or removed volumes, plus explicit `otapctl device list` and `otapctl device reap` commands.
>   * Test the entire crash, `USER_RECOVERY` re-attach, and reaper state machine deterministically on Windows via the mock transport in `otap-ublk` and `otap-sim`.
>
> ### 2. Opus-Storage-Catalogue (Incremental Catalogue/Snapshot Tables & Pool Attestation)
> * **Incremental Catalogue & Snapshot Persistence:**
>   * Replace full-table rewrites with incremental catalogue and snapshot table updates across Consistency Points (logging incremental `SnapRecord` / volume catalogue deltas in `CpLogPage` or CoW catalogue pages and folding them periodically), ensuring O(1) CP metadata overhead even with hundreds of volumes and snapshots.
>   * Maintain backward/forward format clarity and update golden vectors if required by pre-release format rules.
> * **End-to-End Pool Attestation (`otapctl pool verify` / `attest`):**
>   * Build a comprehensive verification and attestation pass in `otap-cp` (exposed via `dbladed` and `otapctl pool verify`) that audits:
>     1. Superblock ring (`A0/A1/B0/B1`) CRC and epoch/sequence monotonicity.
>     2. Every active volume's two-level radix map (`MapPage` CRCs, bounds, and LBA continuity) and pinned `SnapRecord` roots.
>     3. Every live segment's on-disk stripe summary headers and block payload CRCs against the radix map's expected `(lseg, off, birth_cp)`.
>     4. Exact reconciliation between recomputed live block references and `SegEntry::live_bytes` / deferred-free accounting, returning a structured attestation report without panicking on corrupted sectors.
>
> ### 3. Opus-Fabrics-Transport (Async Replication Transport & SnapMirror Engine)
> * Wire `otap-stream` (frames, `Cursor`, `ResumeBitmap`, credit `flow`) into `otap-cp` and `dbladed`:
>   * **Sender & Receiver Pipeline:** Implement the asynchronous replication sender (pinning a `ReplicationBase` snapshot, walking `otap-map` for extents and holes with `birth_cp > base_cp`, packaging `DeltaBegin` $\rightarrow$ `Chunk`/`Holes`/`VolEvent` $\rightarrow$ `DeltaEnd`, and advancing the base snapshot only upon `Committed`) and receiver (staging incoming chunks, enforcing credit windows, maintaining the 4 KiB `CursorPage` / `ResumeBitmap` across disconnects, committing the received delta as an atomic CP on the target pool, and replying `Landed` $\rightarrow$ `Committed`).
>   * **Dual Transport Backends:**
>     1

(The paste ends here.)

### Status (2026-10-14)

Step 6 was paused before the design, at the owner's request; its state and
the remaining work are in `step6/README.md`. The note on how the brief was
read follows when the step ends.
