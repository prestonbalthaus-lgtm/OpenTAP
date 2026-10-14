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
