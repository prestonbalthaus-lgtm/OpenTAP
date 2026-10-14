# OpenTAP

OpenTAP is an open storage operating system for homelab and power-user
hardware, written in Rust. It is modeled on the architecture of clustered
enterprise storage arrays, and the aim is to bring that class of design to
ordinary servers and disks: SAN and NAS features of that grade, without the
licensing, on the hardware people already own.

## The goal

Most open storage stacks make you choose: fast or safe, block or file, one
fabric or another. OpenTAP is built around one engine that serves all of it.

- **Writes at memory speed.** Client writes land in a DRAM write cache and
  are acknowledged at once. Every few seconds a consistency point writes
  everything that changed to disk as a small number of large, sequential,
  full-stripe writes, so spinning disks and parity RAID see streaming
  writes instead of random ones.
- **Any disks, any box.** Internal drives, a hardware RAID virtual disk or
  an mdadm array, mechanical or flash. The engine discovers the device's
  geometry and plans its stripes, its flush policy and its discards around
  it.
- **Block, file and object from the same volumes.** iSCSI, and later Fibre
  Channel, through the kernel's LIO target over ublk; NFS and SMB over XFS
  inside a LUN; S3 through an embedded gateway. Each tenant lives in its own
  storage virtual machine with its own network namespace.
- **Honest durability.** The write cache is volatile by design, like ZFS
  with sync disabled, and OpenTAP is explicit about what that means. After
  a power loss it comes back to the last complete consistency point, and a
  volume whose acknowledged writes were lost is taken offline and reported
  (`nvfail`) rather than served silently.
- **One node first, then two.** Phase C, the current work, is a single
  node. Phase B adds a second node with asynchronous replication of each
  consistency point's deltas, and Phase A adds shared-shelf failover, with
  a three-voter Raft quorum and a lightweight witness that can never stall
  I/O.
- **Built to be trusted.** Complete code with no placeholders, `unsafe`
  confined to the one crate that talks to the kernel with a written proof
  for every block, every on-disk layout pinned by golden vectors, and a
  deterministic simulator that crashes and power-cuts the engine thousands
  of times per test run. Every step ends with an independent adversarial
  review whose findings are fixed and kept as regression tests.

## How it works

A write arrives through the ublk front door, is copied into a huge-page
arena, is recorded in a per-volume dirty index, and is acknowledged. The
consistency-point engine periodically freezes the dirty index, packs the
data into full stripes, writes them to free segments on the backing device
together with the map pages and log records that describe them, and commits
the whole consistency point by writing a superblock slot last. Nothing is
overwritten in place, so a consistency point that did not finish is never
visible after a reboot. Reads are served from the arena when the data is
still there and from the persistent map otherwise. Snapshots pin a
consistency point's map root, and folds roll the log back into the map in
the background.

## Status

Phase C (v0.1, a single node) is in progress. Steps 1 to 4 of the
implementation plan are done: the on-disk format, the replication stream
format, the kernel boundary, the write cache, the dirty index, the
consistency-point engine with snapshots and folds, geometry discovery, the
Linux backstore and the ublk front door, each with its independent review.

The engine has run only on the deterministic simulator and over a plain
file so far. The Linux halves compile for Linux but have not yet executed
on a Linux host, and there is no daemon, no segment cleaner and no protocol
front end. OpenTAP is not usable as storage yet.

`docs/06-implementation-status.md` has the current state in detail, the
test gates and what comes next.

## Repository

| Path | What it is |
|---|---|
| `crates/` | the Cargo workspace, eleven crates (listed in `BUILDING.md`) |
| `fuzz/` | libFuzzer targets for the on-disk and on-wire parsers |
| `docs/00` to `05` | the project briefs and the three design rounds, with their full records |
| `docs/06` to `10` | status, engineering decisions, the engineering log, working agreements, the step briefs |
| `docs/reviews/` | the independent review reports, as the reviewers handed them back |
| `BUILDING.md` | how to build and test, the crate layout, the golden vectors, notes for a Windows host |

## Building

Stable Rust 1.89 or later.

```sh
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
```

`BUILDING.md` has the full set of gates, the Linux cross-check and the
Windows host notes.

## License

No license has been chosen yet. Until one is added, the code is copyright
its authors, all rights reserved.

OpenTAP is an independent project with no affiliation to NetApp. ONTAP is a
trademark of NetApp, Inc.
