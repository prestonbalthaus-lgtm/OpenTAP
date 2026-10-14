# OpenTAP

A Rust SAN/NAS operating system for homelab hardware: a volatile DRAM write
cache, log-structured consistency-point flushes to arbitrary block devices,
kernel block targets over ublk, XFS-on-LUN for NFS and SMB, asynchronous
replication and a small Raft control plane. The design is in `docs/`; the
current synthesis is `docs/05-architecture-brainstorm-03.md` (rulings
R25 to R64, the Phase C plan, owner decisions D1 to D15). Start with
`docs/06-implementation-status.md` for what is built, what the gates say and
what comes next; `docs/07-engineering-decisions.md` records every departure
from the design, `docs/08-engineering-log.md` the dated history and
`docs/09-working-agreements.md` the rules of the work.

## Workspace

Phase C (v0.1, single-node foundation) is one Cargo workspace.

| Crate | Step | Purpose |
|---|---|---|
| `crates/otap-format` | 1, 2 | On-disk format v2 and the stream frame header v1: zerocopy layouts, CRC-32C sealing and verification, the superblock ring, stripe summaries, radix map pages, segment and snapshot records, the CP log. No I/O, no `unsafe`. |
| `crates/otap-stream` | 2 | Stream v1 over the frame header: typed frames, the codec (CRC-checked frames with zero-copy `Bytes` payloads, 8 MiB bound), the resumable landing cursor and its persisted page, credit windows. No sockets. |
| `crates/otap-sys` | 2 | The system boundary and the only crate with `unsafe`: hugetlbfs arenas, the ublk UAPI mappings and command encodings, io_uring buffer registration and cloning, the startup capability probe. Linux implementations with portable fallbacks. |
| `crates/otap-arena` | 3 | The write-cache arena over `otap-sys` memory: a bump allocator handing out slot runs with lap counters, the per-slot state table, and re-attach after `kill -9` with no acknowledged write lost. |
| `crates/otap-index` | 3 | The volatile dirty index: per-volume B-trees of dirty extents that coalesce writes into slot runs, and the epoch-flip cut (`arc-swap`) that freezes a generation without stalling writers. |
| `crates/otap-cp` | 3 | The consistency-point engine and the `Backstore`, `FrontDoor`, `Clock` and `Net` traits: the write and read paths, the flusher that assembles full stripes and commits through the superblock ring, boot with torn-CP rewind and nvfail. |
| `crates/otap-sim` | 3 | The deterministic simulator: fake device (pending, cached, durable layers; sector-level tears), fake client, virtual clock, loopback network, the checker model, and the harness that crashes and reboots the engine. |
| `fuzz/` | 2 | cargo-fuzz targets for the superblock ring, stripe summaries and frame demuxing (its own workspace; nightly). |

The format crate's root documentation (`crates/otap-format/src/lib.rs`) is
the layout specification and records the format rulings F1 to F11.

## Building and testing

Stable Rust 1.89 or later (the floor set by `crc-fast` 1.10.0 and
`proptest` 1.12.0). `rust-toolchain.toml` selects `stable` with `rustfmt`
and `clippy`.

```sh
cargo check --workspace --all-targets
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
PROPTEST_CASES=100000 cargo test --workspace            # the merge gate
RUSTDOCFLAGS="-D warnings" cargo doc --no-deps

# otap-sys is Linux code; type-check the workspace from any host with the
# Linux target installed (rustup target add x86_64-unknown-linux-gnu):
cargo clippy --workspace --all-targets --target x86_64-unknown-linux-gnu -- -D warnings

# The fuzz targets type-check without a C++ toolchain; running them needs
# nightly and cargo-fuzz:
cargo check --manifest-path fuzz/Cargo.toml --no-default-features
cargo +nightly fuzz run fuzz_superblock
```

The suites: `otap-format/tests/format_tests.rs` (layout, round trips, CRC
coverage, ring and identity rules, the golden-vector decode lane),
`records_tests.rs` (the Step-2 records), `adversarial_tests.rs` (torn
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
its laps, re-attach, damaged data), `adversarial_arena_tests.rs` (the Step 3
review: re-attach after every interleaving with kills mid-transition, damaged
descriptors, stale references over two laps, `Full` accounting);
`otap-index/tests/index_tests.rs` (split, trim and coalesce, the sequence
contract, the generation ring, a cut that waits for a reader, concurrent
inserts across cuts), `adversarial_index_tests.rs` (every overlap pattern,
slot conservation, lookups across frozen generations against a model, readers
holding a cut); `otap-sim/tests/sim_tests.rs` (the Step 3 proofs: a crash
during the superblock commit rolls back to the previous CP, a torn stripe
rewinds one CP, a daemon crash loses nothing, power cuts hold the right
volumes in both nvfail modes, clean shutdowns never trip it, a full arena
defers writes, and random I/O with random crashes recovers a prefix every
time), `adversarial_sim_tests.rs` (the review's 23 defects pinned, power cuts
in every phase under every tear policy and schedule, crashes inside the
recovery's own boot commit, I/O errors, a full pool, runtime volumes, arena
damage between two processes, and a random walk with bursts of writes in
flight).

### Golden vectors

`crates/otap-format/tests/vectors/v2.0/` holds byte-exact images of every
structure as format 2.0 / stream 1.0 wrote them, with a BLAKE3 manifest whose
hash is pinned in `format_tests.rs`. The decode lane reads those files with
the current reader on every run, so a later release proves it still reads
what an earlier one wrote. Regenerating them is a format change: bump
`FORMAT_MINOR` or `STREAM_MINOR`, run

```sh
cargo test -p otap-format -- --ignored write_golden_vectors --nocapture
```

into a new `vectors/v2.<minor>/` directory, pin the hash it prints, and keep
the previous directory so the N-1 lane keeps decoding it. (The v2.0 set was
extended once, on 2026-10-13, when Step 2 added the segment, snapshot and CP
log records; that is allowed only before a release has written the format.)

### Windows development hosts (x86_64-pc-windows-gnu)

Everything but the Linux half of `otap-sys` builds and runs on Windows. Two
notes for a windows-gnu toolchain:

- rustup's bundled MinGW has no `dlltool.exe`, and the test build needs one
  (proptest pulls in `windows-sys`, which links with `raw-dylib`). GNU
  `dlltool.exe` and `as.exe` from a winlibs release, with their DLLs, must be
  on `PATH`. On the development PC they live in
  `%USERPROFILE%\.cargo\mingw-tools\bin`.
- The toolchain was installed with `--no-modify-path`, so
  `%USERPROFILE%\.cargo\bin` must be on `PATH` as well.

Keep heavy work polite on the shared desktop: `cargo build -j 3`, tests with
`--test-threads=6`.

## Source control

The tree is a git repository (since 2026-10-13) whose `origin` is the private
GitHub repository `prestonbalthaus-lgtm/OpenTAP`, branch `main`. The rules
(one commit per reviewed change with the gates green, the commit identity,
what is ignored) are in `docs/09-working-agreements.md`, section 8. The
GitHub CLI used for pushes and repository work is installed in
`%USERPROFILE%\tools\gh\bin` (on the user `PATH`); `gh auth status` shows the
login. `docs/06-implementation-status.md` is where a new session starts.
