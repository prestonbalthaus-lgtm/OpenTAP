# Step 6C: replication transport, `otapctl replicate`, WSL 2 prep (Opus-Fabrics-Transport, 2026-10-16)

Base `4f34e17` (6B-2). **No format change** (2.4 kept: no pool record; the sink's cursor lives in
`--state-dir`). **No new dependency**: `otap-cp` now uses the workspace's `otap-stream` and `bytes`
(Cargo.lock adds no package). Control protocol stays **1** (variants only). `06`/`07`/`08`: the lead's.

## 1. Frame flow and the state machines (`otap_cp::repl`)

`Handshake` (dialer first) -> `Handshake` | `Refuse` -> stream `Hello` -> `HelloAck{cursor}` ->
`DeltaBegin` -> `VolEvent` (Create on a full transfer / Resize on an increment, then SnapTag) ->
`Holes`* -> `Chunk`* (credit-paced) -> `DeltaEnd{digest}`; sink: `Landed{idx,credits}` per durable
chunk, then `Committed`. A pull is the same with the sink dialing. `Replicator` (one per pool) owns the
jobs, routes accepted links by the first handshake (role Source = inbound push -> `SinkJob`, Sink =
inbound pull -> `SourceJob::serve`), keeps 16 finished jobs, and is stepped after every engine tick.
- **Source** (`source.rs`): Wait/dial -> Handshake -> (decide base from the sink's reported base uuid)
  -> Hello -> Plan (pass 1: `snapshot_delta_scan` sliced by pages counts data blocks, records each
  chunk's start LBA, collects hole runs) -> Begin -> Stream (chunk i rebuilt on demand from its start:
  scan + `snapshot_read_blocks`, packed as a stripe image: StripeHdr{agg, cp=target birth, nonce,
  lseg=i+1, stripe_idx=i}, descriptors with LBA/birth/CRC) -> DeltaEnd -> Commit -> Pin -> Done.
  Delta identity: `cp_seq` = target birth CP, `base` = base birth CP, `nonce` = BLAKE3(target, base)
  so the same delta has the same identity in every connection and process. Increment = `Increment
  {from,to}`; a first transfer = `Steady` with base 0 and the target in the SnapTag. No data block ->
  one **anchor** chunk (block 0 as the target reads it; the stream refuses 0 chunks).
- **Sink** (`sink.rs`): stages Idle / Nvfail / Reset / Create / Resize / Apply / Snapshot / Done.
  Chunk: credits checked, image verified (`parse_expected` identity, exact length, per-block CRC, vol,
  LBA in range), turned into runs, placed by `apply_write`/`apply_punch` (busy -> retry next step).
  Landed only when `durable_seq() >= chunk max seq` (a committed CP carries it): cursor `land`,
  BLAKE3 over images in index order, record persisted, **then** `Landed` with credits back. DeltaEnd +
  complete + digest ok -> `create_snapshot_as(target uuid, Transfer, name)` -> when listed: cursor
  commit, record (base=target, dirty=false), `set_replication_base` on the mirror, old transfer base
  deleted, `Committed`. Digest mismatch -> abort the delta, reset, start over.

## 2. Resume, and what survives a sink restart (decision: resume from the durable cursor)

- The record (`SinkRecord`, 4228 B: header + the stream's 4 KiB `CursorPage` + CRC; `OTRS` v1) is
  saved atomically (tmp+fsync+rename+dir fsync) in `--state-dir` as `repl-<pool>-sink-<vol>.rec`
  after the CP that made chunks durable, so it never claims more than the pool holds; `pool_cp` in it
  catches a pool rewound below it (landing distrusted). On restart the same delta resumes: `HelloAck`
  carries the bitmap, the source skips landed chunks (`resumed_chunks`), only missing ones come.
- `HelloAck` is deferred while chunks of the delta are still in flight (placed, not durable), so
  the cursor holds every chunk received: **a resume never re-sends a landed chunk** (tests count 0).
- The destination snapshot is taken only after every chunk landed; its existence (source uuid) is
  the commit record: a sink finding the landing's target among its snapshots finishes the commit.
- Delta of another identity on a dirty mirror -> roll back to the base (`rollback_volume`), or delete
  a base-less partial first transfer, then land. A lost record (tmpfs `/run/opentap` after reboot):
  base = newest `Transfer` snapshot of the mirror, live volume treated dirty (rolled back first).
- nvfail on the mirror after a dirty boot is cleared by the sink (nothing un-durable was counted).

## 3. The pin rule

The source calls `set_replication_base(T)` only on `Committed`, or when a later handshake reports T
as the sink's base (the `Committed` was lost with a connection); then waits for the pin to be
durable. Anything short leaves the old pin. The mirror pins its own base the same way. Quarantined
(damaged or `SNAP_UNCERTAIN`) snapshots are refused as source (push, pull) and as base (both ends).

## 4. Link framing, handshake, transports

- Link frame: `OTRL` | len u32 (<= `MAX_LINK_PAYLOAD` = 8 MiB + 64 + 1 KiB, refused at the header)
  | CRC-32C | payload. `FrameReader` poisons on bad magic/oversize/CRC; `finish` reports truncation.
  Message byte: 1 Handshake (96 B fixed, `OTRH`, version u16 first, role, accept, agg, vol index,
  target uuid/cp, base uuid/cp, 3 names <= 255 B), 2 Refuse (code + text <= 1 KiB), 3 one stream
  frame. `REPL_PROTOCOL_VERSION = 1`; another version -> `Refuse(Version, "...")` before parsing.
- `Link`/`Dialer`/`LinkListener`/`SinkStore` traits in `otap-cp::traits` (E13). Within a connection
  nothing is dropped, duplicated or reordered; resent chunks after a reconnect are the only duplicates.
- `otap_sim::SimNet`: byte pipes carrying the same frames, delay in ticks, seeded faults by rule
  (`n`-th frame of a kind/direction) or random within a budget: drop (frame + rest lost, reset), cut
  (prefix then end -> truncated), corrupt (CRC refuses); refused dials, queue limit, cable pull.
  `MemStore` = the state dir surviving a simulated crash.
- `otap_daemon::TcpReplicationTransport`: std::net, per connection a reader and a writer thread
  (blocking I/O; read poll 200 ms, write and mid-frame stall timeout 30 s, connect 5 s), bounded
  channels (64) to the engine thread; graceful close. **Security**: no TLS, no authentication
  (whoever reaches the port can push into a mirror or pull a snapshot), so listen and dial are
  loopback-only unless `--allow-remote-replication` (wording mirrors the control plane; help says so).

## 5. Engine API (`otap-cp/src/engine/replica.rs`, child module of `engine`)

`set_replica`/`is_replica` (in-memory mark; needs offline; `set_volume_online(true)` -> `ReplicaVolume`),
`apply_write`/`apply_punch` (`Ok(None)` = arena/stall/admission "not now"; `NotReplica`, `NoSpace`, ...;
precise-mode intents via `apply_intent`), `durable_seq`, `volume_ready`, `snapshot_pending`,
`create_snapshot_as`, `snapshot_delta_scan` (data + holes, incl. unmapped-in-target/data-in-base after
a shrink+grow; shared pages unread; damage quarantines), `snapshot_read_blocks`, `mono_ns`,
`Stats::applied`. Stable view: each call reads the snapshot's current tree in one engine call (trees
taken out of the table, no index copy); snapshots are immutable and relocation keeps births, so
nothing is held between calls and no deferred free is pinned (unlike verify). `MapSet::read_page`.

## 6. Daemon

`dbladed --replication-listen <host:port>` (bound in `Daemon::spawn`, lent to the booted pool,
refusing with `NoPool` when none), `--allow-remote-replication`. `Pool` owns the `Replicator`
(TCP dialer, `FileSinkStore`), attaches it after the boot commit (mirrors marked before any request),
steps it in `tick_raw` (also during other requests' waits). Release: sync, rollback if dirty, mark and
pin off, record removed, volume left offline. Defaults: 1 MiB chunks, 64 MiB sink staging.

## 7. Protocol and CLI

Requests `ReplicatePush{volume,snapshot,to,destination}`, `ReplicatePull{volume,snapshot,from,source}`,
`ReplicateStatus{id}`, `ReplicateCursor{volume}`, `ReplicateCancel{id}`, `ReplicateRelease{volume}`;
replies `Replication`, `Replications{jobs}`, `ReplicationCursor`; types `ReplicationReport`,
`ReplicationCursorReport`, `ReplicationLandingReport`, `ReplicationState` (all `deny_unknown_fields`).
`otapctl replicate push <vol> <snap> --to <host:port>[/<vol>] [--no-wait]`, `pull ... --from`,
`status [<id>]`, `cursor <vol>`, `cancel <id>`, `release <vol>`; snapshots by name or `#<id>` (quote
it); push/pull follow the job (100 ms polls), exit 4 when it ends failed or cancelled.

## 8. Scripts and tomorrow's WSL order

`scripts/wsl2-prep.sh` (new, 755): `--probe` (default) table: kernel >= 6.12, io_uring and
`kernel.io_uring_disabled`, hugetlbfs + 2 MiB + mounted + `nr_hugepages` writable (as root),
`/dev/ublk-control` after `modprobe ublk_drv`, dm-flakey/log-writes/delay, losetup/blkdiscard/cmp (+fio,
jq), cargo for the sudo user, repo at /mnt/e/Coding/OpenTAP, init; `--install-deps [--dist-upgrade]`;
`--build-kernel [tag]` (default linux-msft-wsl-6.18.54.1; `--work`, `--jobs 6`, `--kernel-out`):
tarball -> ext4 tree -> config-wsl + `BLK_DEV_UBLK=m DM_LOG_WRITES=m` (+DM_FLAKEY/DELAY m, IO_URING,
HUGETLBFS, LOCALVERSION -opentap) -> bzImage+modules -> staged modules -> /mnt/e/WSL/kernel/bzImage,
then prints the `.wslconfig` line and the steps. Idempotent; no BIOS, no reboot, no `wsl --shutdown`.
`scripts/linux-smoke.sh` gains: SIGKILL with dd in flight -> same `/dev/ublkbN`, writer completes,
cmp; an orphan made by kill -9 + a `--no-ublk` boot deleting its volume -> `device list` stale ->
`device reap --stale`; `pool verify` clean; second dbladed (heap arena, `--replication-listen
127.0.0.1:47422`), push r1 then r2 (with a discard), release, online, `cmp` the two devices. Also
`--state-dir`, builds as `$SUDO_USER` (rustup PATH), ext4 `CARGO_TARGET_DIR` for a repo under /mnt.
README invocation unchanged, so not edited; its prose list of steps is now behind (docs worker).
Order: BIOS SVM on + reboot; `wsl --install --no-distribution`; Debian (VHDX to E:); `sudo
scripts/wsl2-prep.sh --install-deps`; `scripts/wsl2-prep.sh --build-kernel`; `.wslconfig` kernel
line + `wsl --shutdown`; copy the staged modules + `depmod`; `--probe` all PASS; `sudo
scripts/linux-smoke.sh` (then `--heap-arena`); record findings in `08`.

## 9. Decisions to record (future E-entries)

1. Engines speak `Link` (otap-cp traits); TCP in otap-daemon, sim network in otap-sim; no `otap-repl` crate.
2. A chunk lands when a committed CP carries it (`durable_seq`); HelloAck waits for in-flight chunks.
3. Sink cursor in `--state-dir` (no 2.5 format); committed state rebuildable from the pool (same uuid).
4. Replica = offline + in-memory mark (`ReplicaVolume`); release rolls back a dirty mirror.
5. Replica stamps its own births. 6. First transfer = `Steady`/base 0; empty or holes-only = anchor.
7. Pin moves on `Committed` or a handshake reporting the target; one destination per source volume.
8. Digest = BLAKE3 of chunk images in order, verified unless the sink restarted mid-delta. 9. Same
pool on both ends refused; a pre-existing non-replica destination refused.

## 10. Limits, not done

Never run on Linux (std TCP, tested on Windows). No TLS/auth. Source jobs not durable (a re-push
resumes from the sink cursor). One destination per source volume. Plan: 8 B per chunk + 12 B per
hole run, whole delta walked before DeltaBegin. Sink lands through the arena (a CP per batch).

## 11. Tests (new files)

otap-cp `step6c_transport_wire_tests` (13, 3 proptests); otap-sim `step6c_transport_simnet_tests`
(7), `step6c_replication_engine_tests` (5), `step6c_replication_sim_tests` (15: proptest
`random_transfers_with_faults_and_crashes_converge`, ~60 s at 1,000 cases;
`every_frame_kind_and_position_tolerates_a_fault`, 57 runs); otap-daemon `step6c_transport_tcp_tests`
(11) and `step6c_replication_daemon_tests` (2); otap-control `step6c_replication_control_tests` (4);
otap-ctl `step6c_replication_ctl_tests` (2).
