# Building and testing OpenTAP

This is the build, test and layout guide. `README.md` says what the project
is for. The design is in `docs/`, with the current synthesis in
`docs/05-architecture-brainstorm-03.md` (rulings R25 to R64, the Phase C
plan, owner decisions D1 to D15). Start with
`docs/06-implementation-status.md` for what is built, what the gates say and
what comes next; `docs/07-engineering-decisions.md` records every departure
from the design, `docs/08-engineering-log.md` the dated history and
`docs/09-working-agreements.md` the rules of the work; `docs/reviews/` holds
the independent review reports.

## Workspace

Phase C (v0.1, single-node foundation) is one Cargo workspace.

| Crate | Step | Purpose |
|---|---|---|
| `crates/otap-format` | 1, 2, 4, 5, 6 | On-disk format v2.4 and the stream frame header v1: zerocopy layouts, CRC-32C sealing and verification, the superblock ring, stripe summaries, map pages and roots, segment and snapshot records, the CP log and its directory, the snapshot root page, the name catalogue, the commit's stripe list, and the table log and head pages of the incremental snapshot table and catalogue. No I/O, no `unsafe`. |
| `crates/otap-stream` | 2 | Stream v1 over the frame header: typed frames, the codec (CRC-checked frames with zero-copy `Bytes` payloads, 8 MiB bound), the resumable landing cursor and its persisted page, credit windows. No sockets. |
| `crates/otap-sys` | 2, 4, 6 | The system boundary and the only crate with `unsafe`: hugetlbfs arenas, the ublk UAPI mappings, command encodings, the control ring (waited for, or begun and polled) and queue mapping, `DEL_DEV_ASYNC` and the device class listing, io_uring buffer registration and cloning, the block-device ring behind the Linux backstore, the startup capability probe. Linux implementations with portable fallbacks. |
| `crates/otap-arena` | 3, 4, 6 | The write-cache arena over `otap-sys` memory: a bump allocator handing out slot runs with lap counters, the per-slot state table, hole records for discards, and re-attach after `kill -9` with no acknowledged write or punch lost, its writes deferred until the boot commit lands. |
| `crates/otap-index` | 3, 4 | The volatile dirty index: per-volume B-trees of dirty extents (data and holes) that coalesce writes into slot runs, and the epoch-flip cut (`arc-swap`) that freezes a generation without stalling writers. |
| `crates/otap-map` | 4, 5, 6 | The persistent two-level map: per-volume roots and page indexes, an in-memory overlay of changes since the last fold, delta regions and the CP log directory, copy-on-write folds, snapshot pinning and the `birth_cp` walk (strict and tolerant), the rollback, resize and deletion of volumes, the rewrite of a pinned tree. No I/O: pages come through a reader, regions go through a writer. |
| `crates/otap-cp` | 3, 4, 5, 6 | The consistency-point engine and its traits (`Backstore`, `FrontDoor`, `Clock`, `Net`, and for replication `Link`, `Dialer`, `LinkListener`, `SinkStore`): the write, read and punch paths, the flusher that assembles full stripes and packs the CP's metadata regions into whole stripes, the fold policy, snapshots, their deletion and quarantine, pins and the replication base, rollback, resize and offline volumes, the snapshot table and catalogue as chains, the segment cleaner with its reserve, the admission rule and pacing, segment liveness and deferred frees, the pool verification, boot with replay, torn-CP rewind and nvfail, the E19 flush policy; the replication sessions (`repl`). |
| `crates/otap-geom` | 4 | Geometry discovery: sysfs queue facts (mockable), device classification, the stripe and segment plan aligned to md or hardware RAID, the media profile with its flush and discard policies, operator overrides. |
| `crates/otap-backstore` | 4 | The real `Backstore`s: `UringBackstore` over a block device on io_uring (Linux: `O_DIRECT`, staged writes, `fsync` barriers, `BLKDISCARD`) and `FileBackstore`, a synchronous file-backed fallback that runs the engine on any host. |
| `crates/otap-ublk` | 4, 6 | The ublk front door: the tag state machine from queue descriptors to engine requests and back to results (watermarks, offline and nvfail rejection, stale-completion defence), a mock transport for tests on any host; the host-independent device control, the `USER_RECOVERY` re-attach and the standings of the reaper, `MockKernel` (the 6.12 driver's rules in memory); on Linux the device lifecycle, `LinuxHost` and the runner over `otap-sys`. |
| `crates/otap-sim` | 3, 4, 5, 6 | The deterministic simulator: fake device (pending, cached, durable layers; sector-level tears; discards; a controller cache that may or may not survive a power cut), fake client, virtual clock, loopback network, `SimNet` (byte pipes with seeded drops, cuts, corruption and refused dials) and `MemStore`, the checker model (writes, punches, rollbacks and resizes), and the harness that crashes and reboots the engine, takes and deletes snapshots, rolls back, resizes and deletes volumes, runs the cleaner and the verification, and checks the images, the diffs and the liveness accounting. |
| `crates/otap-control` | 5, 6 | The control protocol between `dbladed` and `otapctl`: the request and reply types as tagged JSON (unknown fields refused everywhere), length-prefixed frames, the endpoint syntax (a Unix socket on Linux, loopback TCP anywhere), a blocking client, the shared size and bound helpers. No sockets opened except by the client. |
| `crates/otap-daemon` | 5, 6 | `dbladed`: one engine thread over a block device (io_uring on Linux) or a file, the CP ticker and the cleaner inside the tick, a ublk device per online volume on Linux with its re-attach after a crash and a binding record in `--state-dir`, the replicator and its TCP transport, the control server, a clean shutdown on `SIGTERM`, `SIGINT` and `daemon stop`. Runs in process in the tests. |
| `crates/otap-ctl` | 5, 6 | `otapctl`: the ONTAP-style command line (`daemon`, `pool`, `volume`, `snapshot`, `io`, `device` and `replicate` groups), text or JSON output, exit codes for refusals, unreachable daemons and waited jobs that did not end well. |
| `fuzz/` | 2 | cargo-fuzz targets for the superblock ring, stripe summaries and frame demuxing (its own workspace; nightly). |

The format crate's root documentation (`crates/otap-format/src/lib.rs`) is
the layout specification and records the format rulings F1 to F21.

## Building and testing

Stable Rust 1.89 or later (the floor set by `crc-fast` 1.10.0 and
`proptest` 1.12.0). `rust-toolchain.toml` selects `stable` with `rustfmt`
and `clippy`.

```sh
cargo check --workspace --all-targets
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
PROPTEST_CASES=10000 cargo test --workspace             # the gate before a commit, about four hours
RUSTDOCFLAGS="-D warnings" cargo doc --no-deps
cargo build --release -p otap-daemon -p otap-ctl        # target/release/dbladed and otapctl

# otap-sys, otap-backstore and otap-ublk carry Linux code; type-check the
# workspace from any host with the Linux target installed
# (rustup target add x86_64-unknown-linux-gnu):
cargo clippy --workspace --all-targets --target x86_64-unknown-linux-gnu -- -D warnings

# The fuzz targets type-check without a C++ toolchain; running them needs
# nightly and cargo-fuzz:
cargo check --manifest-path fuzz/Cargo.toml --no-default-features
cargo +nightly fuzz run fuzz_superblock
```

The suites: `otap-format/tests/format_tests.rs` (layout, round trips, CRC
coverage, ring and identity rules, the golden-vector decode lanes for 2.0
and 2.1), `records_tests.rs` (the Step-2 records and the 2.1 directory,
snapshot root page and table regions), `adversarial_tests.rs` (torn
writes, alignment, extremes, degenerate images, no-panic fuzzing, an
independent CRC-32C oracle); `otap-stream/tests/stream_tests.rs` (frame round
trips, chunking invariance, corruption reporting, cursor order invariance and
resumption, credit windows), `adversarial_stream_tests.rs` (every
fragmentation, every bound and canonical-form rule of every body, cursor wire
and page attacks, random window sequences, codec determinism and injectivity,
an allocation probe that proves hostile counts and contexts are refused before
anything proportional is allocated); `otap-sys/tests/sys_tests.rs` (UAPI
constants and encodings, the fallback arena, the capability allowlist);
`otap-arena/tests/arena_tests.rs` (geometry, the slot lifecycle, the ring and
its laps, re-attach, damaged data, hole records), `adversarial_arena_tests.rs`
(the Step 3 review: re-attach after every interleaving with kills
mid-transition, damaged descriptors, stale references over two laps, `Full`
accounting); `otap-index/tests/index_tests.rs` (split, trim and coalesce,
holes, the sequence contract, the generation ring, a cut that waits for a
reader, concurrent inserts across cuts), `adversarial_index_tests.rs` (every
overlap pattern, slot conservation, lookups across frozen generations
against a model, readers holding a cut); `otap-map/tests/map_tests.rs` (a
model of the map across random CPs, folds, replays and reboots, damage and
misdirected regions, replay out of order, what a fold frees, snapshot pins
and diffs); `otap-geom/tests/geom_tests.rs` (classification, the plan for
md, NVMe, hardware RAID and lone drives, overrides, small and misaligned
devices); `otap-backstore/tests/file_backstore_tests.rs` (the engine over a
plain file: format, writes, punches, a snapshot, folds, a restart; the
backstore's contract); `otap-ublk/tests/frontdoor_tests.rs` (decoding, the
errno map, every tag transition, aborts, stale completions, the
watermarks, transport failures), `frontdoor_proptests.rs` (the state
machine against a model under random driver and engine interleavings),
`engine_tests.rs` (the engine behind the front door over the simulator's
device: round trips, many requests in flight, the engine's own checks as
errnos, nvfail after a power cut, a stopped engine, an abort in flight);
`otap-sim/tests/sim_tests.rs` (the Step 3 proofs: a crash during the
superblock commit rolls back to the previous CP, a torn stripe rewinds one
CP, a daemon crash loses nothing, power cuts hold the right volumes in both
nvfail modes, clean shutdowns never trip it, a full arena defers writes, and
random I/O with random crashes recovers a prefix every time),
`adversarial_sim_tests.rs` (the Step 3 review's 23 defects pinned, power
cuts in every phase under every tear policy and schedule, crashes inside the
recovery's own boot commit, I/O errors, a full pool, runtime volumes, arena
damage between two processes, and a random walk with bursts of writes in
flight, punches, snapshots and folds), `step4_sim_tests.rs` (the map across
pages and segments through crashes, CP log replay from the last fold,
snapshot images and diffs across random crashes, punches through every
crash, the hybrid read path, every fold trigger, both discard policies, the
trust policy over a protected and an unprotected controller cache, the
late-discard regression, a mixed seeded walk, and a property walk over
volumes wider than a map page), `review_sim_tests.rs` (the Step 4 review's
23 reproductions and pinned properties: the boot and a fold CP's stripes,
damaged pages and snapshot roots, the reserve and a fold that does not fit,
dead data segments, a rewind below a volume's creation, a transient read
error at boot, the trust policy with a lost data block); the review's
`review_map_tests.rs`, `review_geom_tests.rs`, `review_ublk_tests.rs` and
`review_backstore_tests.rs` likewise (page edges across folds and reboots,
hypervisor disks and odd block sizes, the front door offline and at zero
watermarks, a pool opened under another geometry). Step 5 added
`otap-format/tests/catalog_tests.rs` (the catalogue records, the name
rules, the canonical order, the stripe list), `otap-map/tests/lifecycle_map_tests.rs`
(rollback, resize, deletion and the rewrite of a pinned tree against the
model), `otap-sim/tests/step5_sim_tests.rs` (deletion releasing exactly
the unshared blocks, the replication base, deletion and rollback through
power cuts in every phase, the snapshot limits, the cleaner on a fragmented
pool, blocks snapshots reference moving with their pages, a pool full for
users that still punches, deletes and cleans, relocation against a newer
write, the admission rule, pacing, the catalogue across crashes, offline
volumes, and a random walk of deletions, rollbacks, cleaning and crashes
with the liveness audit after every recovery), `otap-daemon/tests/daemon_tests.rs`
(the daemon in process: format, boot, the lifecycle, a clean and a dirty
restart, the refusals, the spawn, the TCP server) and
`otap-ctl/tests/ctl_tests.rs` (`otapctl` as a process against that daemon);
the Step 5 review's files, ported in place:
`otap-sim/tests/review_step5_sim_tests.rs` (rollbacks, deletions and
volume operations racing a cleaning CP or a snapshot in flight, a damaged
stripe summary, a damaged catalogue, the B slots, the reserve under many
snapshots, and the brief's two properties: GC plus crash invariance and
the recovery of a pool at 100% of its user capacity),
`otap-daemon/tests/review_daemon_tests.rs` (the rollback with
`--delete-newer`, the format guard, a wiped start, a deletion after a
shrink, concurrent clients, malformed frames, names at their bounds, I/O
at the edges) and `otap-control/tests/review_control_tests.rs` (unknown
fields, a byte-at-a-time stream, hostile bodies, the frame bound).

### Golden vectors

`crates/otap-format/tests/vectors/v2.0/` to `v2.4/` hold byte-exact images
of every structure as formats 2.0 to 2.4 (stream 1.0) wrote them, each with
a BLAKE3 manifest whose hash is pinned in `format_tests.rs`. The decode lane
reads every set with the current reader on every run, so a later release
proves it still reads what an earlier one wrote. Regenerating a set is a
format change: bump `FORMAT_MINOR` or `STREAM_MINOR`, run

```sh
cargo test -p otap-format -- --ignored write_golden_vectors --nocapture
```

into a new `vectors/v2.<minor>/` directory, pin the hash it prints, and keep
the previous directories so the older lanes keep decoding them. (The v2.0
set was extended once, on 2026-10-13, when Step 2 added the segment,
snapshot and CP log records; Step 4's 2.1 set was the first minor bump,
for the CP log directory, the snapshot root page and the superblock's
open-data fields, and its review's 2.2 set the second, for the recorded
geometry; Step 5's 2.3 set the third, for the catalogue, the stripe list
and the offline flag; Step 6's 2.4 set the fourth, for the table log and
head pages and the superblock's head CPs.)

### The smoke test on Debian 13 or WSL 2

`scripts/linux-smoke.sh` is the first-run test of the Linux halves: a loop
device (or `--device /dev/sdX`, which is wiped), a hugetlbfs arena (or
`--heap-arena`), `dbladed` with one volume on `/dev/ublkbN`, dd and fio,
a snapshot, a rollback, the cleaner, a `SIGTERM` restart, a `SIGKILL`
restart under I/O that must re-attach the same `/dev/ublkbN`, an orphaned
device listed and reaped, `pool verify`, and a push to a second `dbladed`
over 127.0.0.1 compared byte for byte. It needs root, kernel 6.12 or later
with `ublk_drv`, `util-linux`, `diffutils`, optionally `fio` and `jq`;
`--release` builds the release profile, `--keep` leaves the work
directory, `--repl-port` names the second daemon's port. Under `sudo` it
builds as the calling user with that user's rustup toolchain, and a
repository on a Windows drive builds into a target directory on the Linux
file system. `scripts/wsl2-prep.sh` prepares a WSL 2 distribution: `--probe`
(the default) checks the kernel, io_uring, hugetlbfs, `/dev/ublk-control`,
the dm targets, the tools and the toolchain; `--install-deps` installs the
packages; `--build-kernel [tag]` builds Microsoft's WSL kernel (6.18 by
default) with ublk and dm-log-writes as modules. The README's "Running the
smoke test" has the exact steps and the manual equivalent. Neither script
has run yet.

### Windows development hosts (x86_64-pc-windows-gnu)

Everything but the Linux halves of `otap-sys`, `otap-backstore`,
`otap-ublk` and `otap-daemon` builds and runs on Windows; the engine runs
there over the simulator's device and over a plain file, and `dbladed` and
`otapctl` run there over a file with the control channel on loopback TCP
(no block device is exposed). Two notes for a windows-gnu toolchain:

- rustup's bundled MinGW has no `dlltool.exe`, and the test build needs one
  (proptest pulls in `windows-sys`, which links with `raw-dylib`). GNU
  `dlltool.exe` and `as.exe` from a winlibs release, with their DLLs, must be
  on `PATH`. On the development PC they live in
  `%USERPROFILE%\.cargo\mingw-tools\bin`.
- The toolchain was installed with `--no-modify-path`, so
  `%USERPROFILE%\.cargo\bin` must be on `PATH` as well.

Keep heavy work polite on the shared desktop: `cargo build -j 3`, tests with
`--test-threads=6`.

A git worktree needs its own target directory (`CARGO_TARGET_DIR`): cargo's
artifact hashes do not include the checkout's path, so a worktree that
shares the main tree's `target/` overwrites the main tree's artifacts with
a build of its own sources, and the next build in the main tree links them
without rebuilding (`docs/09-working-agreements.md`, section 6).

## Source control

The tree is a git repository (since 2026-10-13) whose `origin` is
`github.com/prestonbalthaus-lgtm/OpenTAP`, branch `main`. The rules (one
commit per reviewed change with the gates green, the commit identity, what
is ignored) are in `docs/09-working-agreements.md`, section 8.
`docs/06-implementation-status.md` is where a new session starts.
