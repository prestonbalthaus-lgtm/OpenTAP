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
the background. A segment cleaner moves the live blocks out of mostly dead
segments and frees them, keeping a reserve so a full pool can still delete
a snapshot or compact itself. The daemon, `dbladed`, hosts the engine,
exposes one block device per volume through ublk, and answers the
command-line client `otapctl` over a control socket.

## Status

Phase C (v0.1, a single node) is in progress. Steps 1 to 5 of the
implementation plan are done: the on-disk format, the replication stream
format, the kernel boundary, the write cache, the dirty index, the
consistency-point engine with snapshots, folds, snapshot deletion, rollback
and the segment cleaner, geometry discovery, the Linux backstore, the ublk
front door, and the daemon `dbladed` with its command-line client `otapctl`,
each step with its independent review.

The whole lifecycle (format, boot, volumes, writes and reads, snapshots,
their deletion, rollback, the cleaner, a clean restart) runs end to end over
a plain file, which is how it is tested on the Windows development host.
Nothing has executed on a Linux host yet: the io_uring backstore and the
ublk devices compile for Linux and wait for the first smoke test (below).
OpenTAP is not usable as storage yet.

`docs/06-implementation-status.md` has the current state in detail, the
test gates and what comes next.

## Repository

| Path | What it is |
|---|---|
| `crates/` | the Cargo workspace, fourteen crates (listed in `BUILDING.md`) |
| `fuzz/` | libFuzzer targets for the on-disk and on-wire parsers |
| `scripts/linux-smoke.sh` | the first-run smoke test for a Debian 13 host (below) |
| `docs/00` to `05` | the project briefs and the three design rounds, with their full records |
| `docs/06` to `10` | status, engineering decisions, the engineering log, working agreements, the step briefs |
| `docs/reviews/` | the independent review reports, as the reviewers handed them back |
| `BUILDING.md` | how to build and test, the crate layout, the golden vectors, notes for a Windows host |

## Building

Stable Rust 1.89 or later.

```sh
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
cargo build --release -p otap-daemon -p otap-ctl   # target/release/dbladed and otapctl
```

`BUILDING.md` has the full set of gates, the Linux cross-check and the
Windows host notes.

## Running the smoke test on Debian 13

The first live run is `scripts/linux-smoke.sh`. It has never been executed
yet (the development host is Windows), so expect it to find things; what it
finds goes into `docs/08-engineering-log.md`.

Requirements: Debian 13 (Trixie) with kernel 6.12 or later; root; the ublk
driver (`modprobe ublk_drv`; Debian's kernel ships it as a module);
`util-linux` (`losetup`, `blkdiscard`), `diffutils` and `coreutils`;
optionally `fio` and `jq`; stable Rust 1.89 or later from rustup for the
user who builds.

```sh
git clone https://github.com/prestonbalthaus-lgtm/OpenTAP.git
cd OpenTAP
cargo build -p otap-daemon -p otap-ctl              # the script builds too; this shows errors early
sudo scripts/linux-smoke.sh                          # a 2 GiB loop device, a 256 MiB hugetlbfs arena
sudo scripts/linux-smoke.sh --heap-arena             # the same without reserving huge pages
sudo scripts/linux-smoke.sh --device /dev/sdX        # a real device: it is WIPED
```

The script reserves the huge pages and mounts hugetlbfs under a work
directory, formats the pool, starts `dbladed`, creates a 512 MiB volume and
waits for its `/dev/ublkbN`, writes 64 MiB and reads it back (and runs a
20-second fio verify when fio is installed), takes a snapshot, overwrites
and discards part of the volume, rolls back and verifies, deletes the
snapshot, runs the cleaner, stops the daemon with `SIGTERM` and restarts it
(a clean boot), kills it with `SIGKILL` and restarts it (a dirty boot whose
hugetlbfs arena re-attaches, so no acknowledged write is lost; with
`--heap-arena` the volume is held by nvfail and cleared), verifies once
more and tears everything down. It prints `SMOKE TEST PASSED` or stops at
the first failed step with the daemon's log.

The same by hand, on a device you can wipe:

```sh
sudo mkdir -p /run/opentap/arena
sudo mount -t hugetlbfs -o pagesize=2M none /run/opentap/arena
echo 600 | sudo tee /proc/sys/vm/nr_hugepages      # 1 GiB arena: 512 pages plus slack
sudo target/debug/dbladed --control unix:/run/opentap/dbladed.sock &
sudo target/debug/otapctl pool format --device /dev/sdX
sudo target/debug/otapctl pool boot --device /dev/sdX --arena-bytes 1G
sudo target/debug/otapctl volume create --name data --size 10G
sudo target/debug/otapctl volume show --name data    # names the /dev/ublkbN device
sudo mkfs.xfs /dev/ublkbN && sudo mount /dev/ublkbN /mnt
sudo target/debug/otapctl snapshot create --volume data --name s1
sudo target/debug/otapctl pool status
sudo umount /mnt && sudo target/debug/otapctl daemon stop   # a clean shutdown
```

`otapctl --help` lists every command; `--json` prints a reply as the daemon
sent it.

## License

Copyright 2026 the OpenTAP authors. Licensed under the Apache License,
Version 2.0; see `LICENSE`. Contributions are accepted under the same
terms. OpenTAP runs on Linux and drives the kernel through its public
interfaces (ublk, io_uring, configfs); it links no GPL code, and every
crate it depends on is permissively licensed.

OpenTAP is an independent project with no affiliation to NetApp. ONTAP is a
trademark of NetApp, Inc.
