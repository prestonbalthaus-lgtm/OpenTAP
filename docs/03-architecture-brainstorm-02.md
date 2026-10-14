# OpenTAP Architecture Brainstorm 02: unified multi-fabric, multi-protocol scope on Debian

Lead architect: Fable 5.1. Workers: Opus-Fabrics, Opus-Storage, Opus-Protocols, Opus-Consensus (persona names for the workflow subagents). Produced 2026-10-12.

Inputs: `docs/02-project-brief-unified.md` (charter revision 2), the round-1 verified digest (`docs/01-architecture-brainstorm.md`, Parts 6 and 7) and the round-2 workflow record `docs/03-architecture-brainstorm-02-record.md` (4 brainstorms, 4 cross-examinations, 32 web-sourced fact checks, 4 revised positions, 3 critics; 47 agents, 0 errors, 62 minutes).

How to read this document. Parts 1 to 4 present the four specialists individually, each ending with what the adversarial review changed in their position. Part 5 is the adversarial critique: the cross-examination conflicts, the fact-check scoreboard and the three critics. Part 6 is my executive synthesis and rulings. Part 7 standardizes the Rust trait contracts. Part 8 is the concrete Phase 1 roadmap on Debian. Part 9 lists the decisions only the owner can make. Every number is an estimate unless marked measured; nothing was installed, built or benchmarked in this round.

---

## Part 0. What the revised charter resolved, and the findings that reshape it

The three failed premises of round 1 are resolved by the owner's revision: Debian replaces Rocky, so the Fibre Channel SCSI target is in-box again; a volatile DRAM write cache mirrored to the partner is explicitly authorized, so the NVLOG medium question is closed; the 3-voter model (two data nodes plus a witness) is adopted. The widened scope (block, file and object over FC, Ethernet and InfiniBand, on HDD, flash and hybrid pools) is what this round examined. Eleven findings came out of it; each is argued and sourced in Parts 1 to 5 and ruled on in Part 6.

1. **Debian 12 as shipped cannot run the design.** Bookworm's stock 6.1 kernel has no `CONFIG_BLK_DEV_UBLK` line in any config fragment and no package ships `ublk_drv.ko`; four independent checks confirmed it. The floor is Debian 13 trixie (6.12.x, generic amd64 image) or bookworm with the bookworm-backports 6.12 kernel, which is the same source. The cloud image flavour disables the QLogic driver and must not be used.
2. **Everything else the design needs is in-box on trixie.** `tcm_qla2xxx`, `target_core_user`, `tcm_loop`, iSCSI and FC targets, nvmet for FC/TCP/RDMA plus loop and fcloop test transports with DH-HMAC-CHAP and TCP TLS, ublk with user recovery, reissue and user copy (no fail-io flag, no zero copy on 6.12), ksmbd, NFSv4 server, md-cluster, mlx4/mlx5/rxe RDMA providers, five watchdog drivers, dm-log-writes and hugetlbfs. Userspace: rtslib-fb exposes the qla2xxx fabric unchanged, nfs-utils 2.8.3 has `rpc.nfsd --scope`, Samba 4.22, rdma-core 56, fence-agents 4.16.
3. **FC is a client fabric, not the mirror leg.** A QD1 4 KiB put to Placed over 8GFC through LIO is 80 to 200 us (four frames, eight firmware traversals, three or four kernel hops), against 25 to 40 us on busy-polled 10GbE TCP and about 10 us on 25GbE RoCE. Unknown fate on FC lasts up to R_A_TOV (10 s). Initiator error handling on a dual-mode port stalls the port that also serves clients. 16G and 32G cards run flash-resident firmware with no Debian blob, and 8G cards are silently downgraded to Debian's 8.07.00 blob unless `ql2xfwloadbin=1`. Symmetric dual-mode N2N between two Linux ports is unreported in public, although asymmetric target-to-initiator N2N has public history since 2015. The charter's reason for Debian still holds for FC-SCSI client targets.
4. **The DRAM cache changes what FLUSH means.** Round 1's "advertise write-cache-disabled" is reversed: initiators told there is no cache stop sending SYNCHRONIZE CACHE and lose their own ordering barrier. OpenTAP advertises a volatile cache (LIO `emulate_write_cache=1`, NVMe VWC) and defines FLUSH as "everything before me is Placed on both nodes", which acks already guarantee, so a flush costs one in-flight-put wait. Under GrantUnmirrored durability silently drops to one DRAM copy; that needs a named policy, a mandatory alarm and an honest forced-takeover printout.
5. **ROLLBACK must be nvfail.** After a dual power loss, clients that survived on their own UPS (ESXi retrying through APD, Windows inside its hold time, Linux with queueing multipath) resume against a disk that lost up to a CP window of acknowledged writes while their journals and page caches are ahead: silent corruption no fsck finds. ONTAP's answer is nvfail; OpenTAP adopts it: the aggregate's LUNs go Inaccessible and NAS exports stay down until an operator acknowledges, with the list of volumes that had writes in the lost window.
6. **The takeover ladder is lease-centred and has two bounds.** Lease TTL 3 s, renewed every 500 ms through the Raft leader with no log write. A silent node is taken over at about 4.3 s plus data steps: block resume 5 s p50 on plex, 6 to 8 s on a shelf (md assembly), p99 9 s. A wedged node that still answers on the wire is fenced by silence (lease lapse, then watchdog 5 s, then platform reset): 11 to 13 s, hard bound 15 s. Client detection dominates the unplanned case: ESXi pauses about 35 s on a silent iSCSI target unless the shipped recipe and the gated RST proxy are in place; Windows sends no NOP-Out and relies on TCP and SCSI timeouts.
7. **BUSY does not exist on this stack.** Through ublk, LIO iblock and nvmet an error is a CHECK CONDITION or an NVMe error, never a retriable BUSY. Back-pressure is held requests under a throttle (at most 2 s per request), then ALUA Transitioning or ANA change on the owner's own ports, with a hard bound and a self-takeover proposal for node-local stalls. The round-1 "Stalled then BUSY" line is deleted.
8. **A daemon crash must not be a takeover.** Rings are files (a hugetlbfs outbound arena, a tmpfs inbound ring) owned by a ring-holder process; the engine re-attaches after a crash. The budget is 2 s target and 5 s hard, and a supervised restart suspends the watchdog's worker check-ins so the restart does not become an 8 s fence.
9. **The replay rule changed.** Replaying the maximal contiguous prefix drops an acknowledged write that sits behind a slot whose fate is unknown. Takeover replays every CRC-valid Placed record above the last committed CP in sequence order; the CP cut itself advances only over Placed records, so plexes cannot diverge from the owner's disk.
10. **HDD drain is worse than claimed, and DRAM sets the CP interval.** A 12-drive RAID6 of modern CMR drives absorbs 1.2 to 2 GB/s of full-stripe writes when empty, but under uniform random overwrite at 80% full the honest sustained figure is 0.3 to 0.5 GB/s, not 0.7 to 1.0. Reserves move to 70/80/90, usable capacity is advertised as 0.8 times physical, and on a 64 GB node the ring (8 GiB per direction), not the 10 to 15 s timer, sets the CP interval: about 4 s at 1 GB/s with back-to-back CPs as the steady state.
11. **NAS high availability is protocol state, not engine design.** NFSv4 grace cannot go below 10 s, early grace end is defeated by one absent or v4.0 client and by lockd's grace when NFSv3 is enabled in the same namespace, and no Debian-packaged Samba or ksmbd carries open SMB handles across nodes. NFSv3 datastores carry no server state and fail over at the block ladder. XFS inside a LUN remains the file layer; a native inode layer stays off the roadmap.

And the one decision that remained open after round 1: the shared SAS shelf is the only medium where takeover of flushed data exists before plex replication ships, and its fence (SCSI-3 persistent reservations passed through the shelf's expanders to the drives) is unverified. It is the week-1 spike and the Phase 1 reference tier; plex-by-reference with matched geometry is Phase 2; plex-by-replay and spill-on-silence are dropped; single-node operation must be first-class.

---

## Part 1. Opus-Fabrics (MirrorTransport across FC, TCP, RoCE and InfiniBand)

### 1.1 Executive take

Build one `MirrorLink` trait with honest capability bits and thin implementations: TCP (io_uring, pinned busy-poll thread), RDMA (one code path for RoCEv2 and InfiniBand through the legacy `ibv_post_send` API) and FC (the local qla2xxx initiator role writing O_DIRECT from the ring into the partner's LIO fileio LUN, which is a tmpfs file the partner daemon mmaps). All legs land in one tmpfs-backed inbound ring per node. The ack contract is the same on every leg: Placed means bytes are in the partner's DRAM, confirmed by a hardware ack (SCSI GOOD, RC ACK) or a peer process. What differs between legs, write granularity, how long an unknown-fate slot stays unknown and who produces the ack, belongs in `Caps`, not in a second trait. Baseline is Debian 13 trixie, because bookworm's 6.1 kernel never enabled ublk. The biggest risk is Linux-to-Linux symmetric dual-mode N2N FC, which nobody has published. The strongest disagreement with the charter: FC should not be a first-class Phase 1 mirror tier; it is the slowest leg, the one where initiator error recovery can reset a port shared with client targets, and the one needing the most kernel plumbing.

### 1.2 Libraries and kernel interfaces

Debian packages (trixie / bookworm versions, verified on packages.debian.org and sources.debian.org): `linux-image-amd64` 6.12.x / 6.1.x, `bookworm-backports` 6.12.x~bpo12 (same 6.12 config as trixie); `firmware-qlogic` (non-free-firmware) ships `ql2100` to `ql2500_fw.bin` only, with `ql2500_fw.bin` at 8.07.00 on trixie (8.08.207 reached linux-firmware in September 2025 and is in sid only); `targetcli-fb` 2.1.53 and `python3-rtslib-fb` 2.1.76 carry only a saveconfig-path patch, so the qla2xxx fabric is exposed at `/sys/kernel/config/target/qla2xxx/<naa.wwpn>/tpgt_1/`; `rdma-core` 56.1 (mlx4, mlx5 and rxe providers), `opensm` 3.3.23, `infiniband-diags`, `multipath-tools`, `nvme-cli`, `sg3-utils`.

Kernel modules on trixie 6.12 (all `=m` unless noted, verified from Debian's config): target core, fileio, TCMU, `tcm_qla2xxx`, `tcm_loop` (lets the FC receive path be tested with no FC hardware), `tcm_fc`, iSCSI target, `ib_srpt`, `ib_isert`; nvmet with FC, TCP, RDMA, FCLOOP and LOOP transports, TCP TLS and AUTH (trixie only); `ublk_drv` (trixie only); md raid456 and md-cluster, dm-multipath, ksmbd, nfsd with V4, block and SCSI layouts; `ib_core`, `ib_uverbs`, `rdma_ucm`, mlx4 (still built in 6.12), mlx5, IPoIB, `rdma_rxe` (soft-RoCE for CI and VMs), irdma, bnxt_re, qedr; softdog, iTCO, sp5100_tco, ipmi_watchdog, i6300esb (VM witness); tmpfs, hugetlbfs and THP built in; HZ 250, voluntary preemption, NO_HZ_FULL and CPU isolation available.

Rust crates: `sideway` 0.4.3 (2026-06, on `rdma-mummy-sys`): `BasicQueuePair` through `ibv_create_qp` plus `ibv_post_send`, `ExtendedQueuePair` through `ibv_create_qp_ex` plus `ibv_wr_*`, and an rdmacm module; `ibverbs` 0.9.2 (jonhoo, 2025-03): the fact check reversed Fabrics' reading, it drives queue pairs exclusively through the legacy API and therefore works on ConnectX-3; `rdma-sys` 0.3.0 is stale raw bindings. Also `io-uring` 0.7, `rustix`, `memmap2`, `crc32c`, `rustls` and `tokio` for the control plane, `openraft` 0.9.25 per Consensus.

### 1.3 Architecture

Local ring (sender side): one file per node, registered once with io_uring and once per RDMA device in 1 GiB chunks (mlx4 memory-translation-table limits). It is the local NVLOG; the D-Blade's dirty index points into it and the CP DMAs from it. Inbound ring (receiver side): a tmpfs file mounted `huge=always,noswap` (6.4 and later), mlocked by a small ring-holder unit so it outlives daemon crashes. tmpfs, not hugetlbfs, because LIO fileio writes through `vfs_iter_write` and hugetlbfs has no write path (confirmed, FABRICS-5 and STORAGE-4). The same file is the FC LUN backing store, the RDMA memory region and the TCP `readv` target; three writers share one address space without conflict because slot ownership is sender-side.

Initial layout proposal: link-private 4 KiB descriptor blocks of 64 B headers, rewritten whole by the owning link, because a SCSI LUN cannot write 64 bytes. Withdrawn after cross-examination (see 1.7) in favour of a per-batch format written exactly once. Records self-validate: a record is accepted only if its header CRC and payload CRC both verify, so a header that lands before its payload (FC commands complete out of order) is simply "not yet placed".

Copy counts, sender / receiver. TCP: 1 / 1 on 6.12 (`SEND_ZC` removes the sender copy above about 16 KiB; `RECV_ZC` is 6.15 and not on trixie; io_uring NAPI busy-poll registration from 6.9 bounds the poller's cost). RDMA: 0 / 0 (two RDMA WRITE work requests per batch, the second signaled; Placed is the sender's completion after the responder's hardware ack; the heartbeat is a WRITE-with-immediate so a sleeping scanner wakes). FC: 0 / 1 in the kernel (`WRITE_FIXED` O_DIRECT to the mirror LUN at slot offsets; the partner's HBA DMAs into target-core pages and fileio copies once into tmpfs pages; `fd_buffered_io=1`, 8 MiB per command, 4 KiB block size; both nodes load `qlini_mode=dual`, export the mirror LUN to the partner's WWPN and log in as initiator). Dual-port cards: mirror on one port, clients on the other, so initiator-side error handling on the mirror port can never reset client sessions; the per-port sysfs `qlini_mode` sets the client ports to `disabled`.

Link health and fallback: a 20 ms ring heartbeat per link (link health only, never node liveness); link dead at 1 s triggers redelivery only. FC adds `port_state` and remote-port state polled every 20 ms, and udev sets the mirror disk's command timeout to 2 s with `fast_io_fail_tmo=1` and `dev_loss_tmo=5`, so a wedged partner fails puts in about 2 s instead of 30. On a flip, outstanding tokens resolve Unknown, their slots are quarantined for the link's unknown-fate bound, and the records are re-put on the secondary into fresh slots; identity is (aggregate, epoch, seq). Active/standby only, never striping; fail-back after 30 s of stable health and a probe put, manual by default on FC. Mixed transports per aggregate are allowed because the record format is transport-blind.

Control plane: TCP plus mTLS over Ethernet (heartbeats, leases, epochs, Raft, the receiver's advisory reject). FC cannot carry it (no IP over FC in Linux). Initially "Ethernet is always present and FC is never the sole link"; revised to accept a control sideband over the mirror fabric (see 1.7 and ruling R6).

### 1.4 Bottlenecks

Revised put-to-Placed estimates, QD1 4 KiB / 64 KiB, with sustained payload bandwidth and the cores the leg owns:

| Transport | 4 KiB | 64 KiB | Bandwidth | Cores |
|---|---|---|---|---|
| 8GFC QLE2562 via LIO | 80-200 us | 160-280 us | ~0.8 GB/s; ISP25xx ceiling ~100-150k IOPS caps unbatched 4 KiB puts at ~0.4-0.6 GB/s | receiver ~1 core of kernel LIO work at line rate |
| 16GFC QLE2692 | 70-170 us | 110-220 us | ~1.6 GB/s | same |
| 10GbE TCP, busy-polled | 25-40 us (p99 several times p50) | 80-100 us | ~1.15 GB/s | 1 pinned core each side while loaded |
| 25GbE TCP | 20-30 us | 40-60 us | ~2.9 GB/s, copy-bound near 3 GB/s per core | 2 cores each side |
| 40/100GbE TCP | 20-30 us | 35-50 us | 4.5 / 11 GB/s | 3-6 cores: use RDMA instead |
| 25GbE RoCEv2 CX-4 Lx | 8-12 us | ~30 us | ~2.9 GB/s | 0 dedicated |
| FDR IB / 40GbE RoCE CX-3 | 8-14 us | 20-25 us | ~6 GB/s (PCIe 3 x8) | 0 dedicated |
| EDR IB / 100GbE RoCE CX-4/5 | 5-7 us | 12-15 us | ~11.5 GB/s | 0 dedicated |

Batching is structural: a link allocates slots contiguously, so N consecutive records are one SCSI WRITE, one `sendmsg` or one scatter list. Ring sizing by the textbook formula (ingest times CP interval plus maximum CP duration) asks for 23 GB per direction at 10GbE; instead the ring is capped by DRAM and fires a CP at half full: 8 GiB per direction on a 64 GB node fires every ~3.5 s at full ingest, and a 12-drive RAID6 drains 4 GiB in about 3 s when empty. 25GbE ingest exceeds any HDD pool's drain, so the honest behaviour is back-pressure, not a bigger ring. Translation overheads on trixie: ublk is copy-based on 6.12 (user copy, about 6 us per 64 KiB plus two wakeups), acceptable and the floor until 6.15 arrives through a later Debian; the FC receive memcpy is about 15% of a core at 1.6 GB/s and the scanner is off the ack path on FC and RDMA. FC-specific limits: initiator error handling escalates abort, LUN reset, target reset, ISP abort (seconds of port reset); 1024 exchanges per port by default; R_A_TOV 10 s bounds unknown fate.

### 1.5 Creative solutions

- **Receiver-passive FC and RDMA.** Placed comes from hardware and records self-validate, so the receiving daemon is off the ack path; the inbound ring keeps filling through a daemon restart because the tmpfs file outlives the process. Only the TCP leg needs a live receiver, which is why TCP receive moves into the ring-holder process.
- **Spike the FC receive path with no FC hardware.** `tcm_loop` plus fileio on tmpfs reproduces the exact kernel path (target-core scatter list, `vfs_iter_write`, shmem pages, mmap) on any machine or VM, so the qla2xxx spike only has to prove login and exchange behaviour.
- **rxe for CI.** Soft-RoCE is built in both Debian kernels; the RDMA leg runs unchanged in a VM, so fallback-flip and replay tests run in CI without ConnectX hardware.
- **Dedicated-port rule.** Mirror port `dual`, client ports `disabled`; the same rule for NICs: the busy-polled mirror link gets its own port and core.
- **A probe slot per FC link** proves the partner's LIO is live before puts flip back, without touching the control link.
- **Spill-on-silence** (write the partner's inbound ring to local media when the partner goes silent) was proposed for node-local disks; scoped to the manual tier in round 3 and dropped by the lead in favour of LocalNvmeRing (ruling R11).

### 1.6 Holes Fabrics found in the plan

Descriptor granularity under SCSI writes; Debian 12 cannot run the D-Blade; the RDMA crate decides whether ConnectX-3 works (reversed by the fact check: both crates reach the legacy path); 16G firmware is whatever is in flash; volatile DRAM plus node-local disks leaves the survivor's ring the only copy of the partner's last window for hours; flush semantics under GrantUnmirrored need a per-aggregate policy; petting the watchdog on control-plane liveness alone misses FC stalls (pet on I/O progress too; softdog needs `soft_margin=5` and the NMI hardlockup detector); the partition matrix (Ethernet down with FC up: the owner cannot renew and must stop exporting while the partner's ring is complete; both links down on a shared switch: no quorum, full outage unless the witness sits elsewhere); N2N dual mode is unproven and its failure mode subtle (`fcport_is_bigger` decides who logs in); a 4-core box cannot host busy pollers plus LIO, nvmet, nfsd and ksmbd (8C/16T floor, one or two isolated cores).

### 1.7 What the review changed

Fact checks: FABRICS-1, -3 and -5 confirmed; -2, -4, -6, -7 and -8 partially correct. The corrections: ublk alone disqualifies stock bookworm (the nvmet loop transports were irrelevant); the ibverbs crate claim was backwards, so ConnectX-3 works with either crate through the legacy queue-pair path; trixie ships `ql2500_fw.bin` 8.07.00, QLE2672 is the ISP83xx family (flash-first like 27xx) and `qla24xx_load_risc` never compares versions, so an 8G card with newer flash firmware is silently downgraded to the Debian blob unless `ql2xfwloadbin=1`; switchless target-to-initiator N2N has public history (2015 reports, YADRO's 2019 target-mode login fixes), only symmetric dual mode is unreported; an FCP write is four frames and eight firmware traversals, so 8GFC put-to-Placed is 80 to 200 us.

Concessions from cross-examination: whole-block descriptor rewrites withdrawn (out-of-order tags or work requests could erase a Placed header; QD1 commands double; quarantine widens to 64 slots), replaced by a per-batch page written once; BUSY does not exist through ublk, iblock or nvmet; TCP `readv` and ack move into `otap-ringrecv` beside the ring-holder so a `dbladed` restart never becomes GrantUnmirrored; the "plex doubles link load" claim was wrong for by-reference plex (about 5% of CP bytes); the kernel command line `panic=5 nowayout=1` stacks to about 13 s and reboots on planned restarts, so `panic=-1`, softdog without `soft_panic`, `nowayout=0` with `otap-ctl` refusing the magic close while it owns an aggregate; lease loss on a healthy owner means stop exporting, keep petting, alarm; prefix replay drops acknowledged writes, so replay all valid records and judge the flip spike by the ACK-history checker; quiesce order, per-port `qlini_mode` and a heartbeat-timer-driven flip that never waits for SCSI error handling; the latency criteria were in-process numbers and the client-visible targets belong to the N-Blade; "RoCE buys a core, not latency" stands, so TCP is built before RDMA; a control path over the mirror fabric is required (first as a ring mailbox, superseded by the `otap-ctl`-owned sideband in round 3).

Holds firm: fileio on tmpfs rather than TCMU (the fact check's aside conflicts with its own FABRICS-5 confirmation; TCMU adds a second kernel copy into a uio area that cannot be the cache); one put/ack trait with `Caps::placed_by`; FC ships after TCP and RDMA; trixie baseline with bookworm-backports tolerated and trixie-backports 7.x an optional lane; tmpfs for every inbound file so a fallback flip never changes files; dedicated mirror port; ConnectX-3 supported; active/standby links with manual FC fail-back and the 1 s timer-driven flip; TCP core cost bounded by NAPI, not zero, which is why RDMA remains the recommended Ethernet leg on 8-core boxes.

### 1.8 Revised Phase 1 recommendation

In order: `otap-ring` (batch format, CRCs, scanner, replay-all above the last CP, property tests for reordering, duplicates, torn batches and epoch flips, plus the ACK-history model for the checker); the TCP leg (io_uring with NAPI registration, `SEND_ZC` above 16 KiB, `otap-ringrecv`, direct-attached DAC as the default topology); the RDMA leg on `ibv_post_send` (rxe in CI, ConnectX-3 and ConnectX-4 Lx in the lab, `ibv_wr_*` probed at runtime); the control sideband, fallback and fail-back, and the udev, modprobe, sysctl and command-line drop-ins shipped as package files; FC spikes only. Spikes with pass criteria: S1 N2N (QLE2562 and QLE2692 pairs, symmetric dual mode, 100 cable pulls, re-login under 5 s, no oops, QD1 4 KiB put p50 under 200 us; if symmetric fails, asymmetric two-link must pass the same bar); S2 `tcm_loop` plus fileio on tmpfs `huge=always` (1 M batches, every GOOD followed by a CRC-valid record in the mapping within 10 us, the LIO thread above 2 GB/s, the scanner under 1% of a core at 1 ms sleep); S3 RDMA (4 KiB WRITE completion p50 under 12 us on CX-3 and 10 us on CX-4 Lx, 64 KiB at 4 GB/s on FDR and 2.8 GB/s on 25G, queue pair to error flushes every work request within 10 ms, an 8 GiB tmpfs region registers on mlx4 in 1 GiB chunks); S4 flip under load (cable pull at 80% of line rate, the secondary carries puts within 1 s, forced takeover, the checker finds every client-acked write, fail-back without a stall over 2 s, partner `otap-ringrecv` and `dbladed` kills resume puts inside the 2 s re-attach budget with no GrantUnmirrored); S5 LAN-dead quorum (unplug both LAN ports, A and B keep two of three over the sideband, no fence, no takeover, no epoch bump on restore).

### 1.9 Risks Fabrics carries forward

Symmetric dual-mode N2N unexercised anywhere public; FC firmware outside our control on used-market cards; ring file and format ownership split across three specialists; dual-mode port error-handling coupling (one ISP abort stalls both mirror directions for 2 to 10 s and leaves up to 10 s of quarantine); the CPU budget on 4-core boxes.

---

## Part 2. Opus-Storage (log-structured D-Blade, allocator, CP, crash consistency)

### 2.1 Executive take

Build the D-Blade as a hugetlbfs-backed DRAM ring that is the only log, a per-volume dirty index over it, and a never-overwrite segment store with a copy-on-write extent map committed by an A/B superblock stripe; one code path for HDD and flash, with a `MediaProfile` (segment size, stripe geometry, CP timer, discard policy, I/O method) as the only media-specific knob. High availability is media-dependent: a used dual-expander SAS shelf for spinning pools, plex-by-reference for node-local flash, manual failover for everything else. The single biggest risk is not the volatile cache the owner has priced in; it is where the ring lives: if it is anonymous memory inside `dbladed`, every daemon segfault becomes a takeover and an unmirrored segfault becomes data loss, so the ring must be a file the daemon re-attaches to. Two disagreements with round 1 and the brief: DRAM mode must advertise a write cache (WCE=1) with FLUSH defined as "everything before me is Placed on both nodes"; and node-local disks and takeover are not compatible, so a 300 to 500 dollar used shelf buys real takeover for HDD pools while flash gets plex at 50% capacity.

### 2.2 Libraries and kernel interfaces

Crates: `io-uring` 0.7 (thread-per-core reactor with `SINGLE_ISSUER` and `DEFER_TASKRUN`; tokio stays off the data path); `libublk` 0.4.8 (the ublk maintainer's crate, confirmed current, pre-1.0 with frequent yanked releases, pin exactly and vendor); `crc-fast` 1.10 or `crc32c`; `blake3` for superblock and map-root digests; `zerocopy` 0.8 for the `repr(C)` headers, superblock and map pages; `rustix` and `libc` for hugetlbfs mmap, `mlock2`, `fallocate`, `BLKDISCARD` and `SG_IO`; `parking_lot`, `crossbeam` (epoch for the CP cut grace period, SPSC queues); `hdrhistogram`, `metrics`; `proptest`, `loom`, `cargo-fuzz`, Miri on the arena and index crates. No KV-store crate for the block map; the CoW B+tree is ours. Phase 3 option: Reed-Solomon GF(2^8) for user-space RAID6 repair.

Kernel interfaces by Debian release: ublk absent on stock bookworm, present on trixie and bookworm-backports 6.12 with user copy (6.4) but without zero copy (6.15) or per-I/O daemons (6.16); io_uring registered buffers coalesce huge pages, but the fact check showed the multi-huge-page coalescing arrived in 6.12 and needs uniform folios (hugetlbfs, not best-effort THP), each registered entry is capped at 1 GiB, and `IORING_REGISTER_CLONE_BUFFERS` (6.12) requires the destination ring to have no buffers registered; md raid456 (`stripe_cache_size` 32768 costs 1.5 GiB per 12-drive array, `group_thread_cnt`, `skip_copy`, write-intent bitmap, `devices_handle_discard_safely=Y` evaluated only at array start, so it belongs in modprobe.d plus the initramfs, and even then only whole-stripe discards are honoured and the kernel never verifies RZAT); hugetlbfs has no write path and mlock on it is a no-op (it is never swappable anyway); LIO fileio on tmpfs for the FC ring LUN; `dm-log-writes` and `dm-flakey` for the crash harness; `sg3_utils` (`sg_persist`, `sg_vpd`), `smartctl -l scterc,70,70`, `mdadm` 4.3/4.4.

### 2.3 Architecture

**Write pipeline.** The outbound arena is a hugetlbfs file (2 MiB pages reserved at boot), `MAP_SHARED`, registered once with io_uring and once as an RDMA memory region, cloned into the other rings. ublk request, slot chosen by a per-queue bump allocator with a lap counter, one kernel copy into the slot (user copy on 6.12), CRC32C per 4 KiB, aggregate-wide `seq`, insert into the volume's `BTreeMap<u64, DirtyExtent>` with split and trim of overlaps, post to the mirror. Client ack = local insert done and partner Placed, overlapped. Superseded extents stay slot-resident until the CP covering their `seq` commits: slot free is a CP-commit event only, so local and partner slot lifetimes are identical by construction. Read-your-writes resolves from the active index, then the frozen (CP-in-flight) index, then the map: ring slot, disk address or zero. Sub-4 KiB writes (512e LUNs are mandatory for ESXi before 9.0) are patch records with a sector mask, merged at read and CP time against the old block.

**CP triggers and throttle.** Timer (HDD 10 s, flash 5 s, a ceiling), ring half full (the ring is double-buffered), back-to-back (previous CP done and the active half over 25%), takeover, shutdown, unmirrored flush, snapshot. Effective interval under load is min(timer, ring/2 divided by ingest): 4 s at 1 GB/s with 8 GiB rings. ZFS-shaped throttle: no delay below 60% of the active half, then `delay = k * occ/(1-occ)` with k recomputed from the last three CPs' drain rate, a single delay capped at 2 s; never fail a write.

**The cut is a consistent global cut.** One `seq` per aggregate; the CP flips an epoch and waits a microsecond grace period until no ingest thread is inside an insert of the old epoch, then `cut = all records with seq <= S`. A guest can only make W2 depend on W1 by waiting for W1's ack, and W1 is inserted before it is acked, so a seq-prefix is exactly an image a power cut could have produced. Revised in round 3: S advances only over Placed records (or under GrantUnmirrored), so the survivor's replay set equals the owner's disk and plexes cannot diverge.

**Flush and FUA.** Advertise WCE=1 and VWC. FLUSH completes when every record with `seq <= flush.seq` is Placed on both nodes, which acks already guarantee; FUA is an ordinary write. Under GrantUnmirrored a cluster knob lets the operator choose how a flush is honoured while the partner is down (ruling R10).

**Media-adaptive allocator.** HDD (shared shelf, 12 x 12 to 16 TB NL-SAS): md RAID6 10+2, 256 KiB chunk, 2.5 MiB data stripe, segment = 32 stripes = 80 MiB, bitmap chunk 128 MiB, `stripe_cache_size` 32768, `group_thread_cnt` 4, SCT ERC 7 s, SCSI timeout 10 s; the CP writes data segments with O_DIRECT `writev` straight from ring slots (md merges to full stripes, zero member reads), then map and segment-table pages, one `fsync`, then the superblock as a FUA full stripe (A/B alternating). Flash (node-local NVMe): md RAID5 (RAID6 from eight drives), 64 KiB chunk, segment about 8 MiB rounded to a whole number of stripes, CP timer 5 s, two open segments (ingest and GC-cold) plus a metadata stream, whole-segment discards with RZAT verified at pool creation, guest UNMAP becomes map holes; ingest costs about 6 to 8 us of CPU per 4 KiB write. Hybrid: HDD pool plus a node-local NVMe read cache (1 to 4 TB) keyed by physical address with 32 KiB lines, plus two SAS SSDs in the shelf as a RAID1 metadata class (map pages, segment summaries, superblocks). Tiering hot extents to flash: no, a cache gives most of the benefit with none of the map complexity.

**Map and features.** Per-volume CoW B+tree, 16 KiB nodes, about 16 B per extent; worst case 0.4% of volume size, so the map lives on disk with a 4 to 16 GiB cache and each CP writes changed leaves and their paths. Snapshots pin a volume root at a cut; clones are writable child roots; frees go to per-snapshot deadlists and decrement per-segment live bytes. GC selects victims by cost-benefit (free times age). Scrub verifies per-4 KiB CRC32C from segment summaries that carry `(vol, lba, seq)` so lost writes are caught against the map's birth CP.

**Crash consistency.** Clean shutdown: final CP, clean flag, no scan. Single-node crash on a shelf: fence (lease expiry plus PR preempt-and-abort on every member, 0.2 s), `mdadm --assemble` 1 to 3 s with a background bitmap resync, read superblocks A/B, take the highest `(epoch, cp_seq)` whose root pages verify, residual scan of placed-but-unindexed records, admit I/O, then an immediate CP to close the dual-fault window. Dual power loss: boot to the last committed CP and raise the ROLLBACK event (nvfail, ruling R5) with `{aggregate, cp_seq, wall clock, window, estimated bytes}`.

**Memory per node.** 64 GB: 8 GiB outbound arena plus 8 GiB inbound ring, indexes 0.3 GiB, map cache 6 GiB, segment tables 0.1 GiB, cache index 0.5 GiB, md stripe cache 1.5 GiB per array, 30 GB left for the OS, XFS page cache and headroom; 128 GB: 16 plus 16 GiB rings and a 16 GiB map cache. Footprint fixed at start, swap off, `oom_score_adj=-1000`.

**Node-local disks versus takeover.** HDD pools: a used dual-expander shelf (MD1200/MD1220 at 150 to 300 dollars, MD1400 at 400 to 800, two LSI 9300-8e or 9207-8e at 30 to 50 each, 12 TB NL-SAS at 90 to 120; prices are beliefs to verify), host A on one expander, host B on the other, one path per drive per host, 100% capacity and ingest, takeover 6 to 8 s, md assembled by the lease holder only with PR on the members as the second fence. Flash pools: plex-by-reference (identical md geometry on both nodes; the partner already holds every CP payload in its inbound ring, so the owner ships only a manifest of placements, new map pages and freed segments, about 5% of CP bytes; link traffic stays 1x; usable 50%; takeover 5 to 6 s with no assembly). Manual failover for everything else.

### 2.4 Bottlenecks

Link versus media: 10GbE (1.1 GB/s) is below an empty HDD pool's drain, so a spinning pool on 10GbE is link-bound; on 25GbE the same pool is drain-bound and the throttle holds ingest. Four NVMe absorb 4 to 8 GB/s, so flash is link-bound until 100GbE RoCE and CPU-bound before that at small blocks. DRAM, not disks, sets the CP interval (above). The map: on a spinning pool every uncached map page costs a 10 ms seek before the data seek, so the flash metadata class is mandatory for HDD pools above about 20 TB. Translation hops: iSCSI through LIO, iblock, ublk and `dbladed` is about 150 to 250 us at QD1 on 10GbE, the ublk hop under 10% of it. GC and the cliff: write amplification 1/(1-u) for victim utilisation u; uniform random overwrite at 80% full collapses HDD sustained ingest to 0.3 to 0.5 GB/s (corrected by the fact check from 0.7 to 1.0), hence reserves at 70/80/90 and hot/cold separation. Partial-block writes: each sub-4 KiB write needs the old block at CP time; 1,000 unaligned writes per CP cost 10 s on spindles, so 512e with a 4 KiB physical exponent, a per-volume alarm and patch targets pinned into the read cache at ingest. md itself: 1.5 GiB stripe cache per array; md raid5 on NVMe reaches perhaps 3 to 5 GB/s per array before the stripe-cache memcpy saturates (measure); discards on RAID5/6 only with RZAT drives and whole stripes.

### 2.5 Creative solutions

- **The ring is a file, not daemon memory.** Rings survive `dbladed` crashes; restart is re-attach, rescan and resume under ublk reissue. This removes the class "software bug equals failover equals (if unmirrored) data loss".
- **Plex-by-reference.** The partner's inbound ring already contains every CP payload, so plex replication ships a manifest instead of segments: 1x link, halved disks only.
- **Read cache keyed by physical address.** The log never overwrites, so a physical address is a stable identity; persistence across reboot is free and invalidation is the segment lap counter.
- **The FC mirror ring as a LUN.** Export the partner's tmpfs ring file through fileio over `tcm_qla2xxx`; the completion is the Placed ack. This is the only shape under which "a kernel target writing into a tmpfs file" is zero-copy on receive: the file must be the ring, not a client LUN image.
- **Honest WCE plus a degraded-mode knob.** WCE=1, FLUSH is a free Placed barrier, the knob applies only while the partner is away.
- **Epoch-flip cut**, **takeover-triggered CP** (the dual-fault exposure shrinks from a CP interval to a CP duration), **native log-structured RAID later** (full-stripe-only writes make parity trivial; a Phase 3 user-space layer replaces md on flash and enables NVMe passthrough with IOPOLL end to end), **ZNS and FDP readiness** (segment = zone; placement hints separate the streams).

### 2.6 Holes Storage found in the plan

Stock bookworm has no ublk; "mirror ring is the only log" in anonymous DRAM makes every daemon crash a takeover; advertising WCE=0 is wrong under the revised brief; flush-forces-a-CP as a general policy destroys batching; the CP interval claim (DRAM sets it); the tmpfs zero-copy receive claim is half true (fileio writes at the LUN's offset, so the daemon learns nothing unless the file is the ring itself); takeover with a dead partner and no witness leaves the survivor at one of three votes; the dual fault after takeover (mandate the takeover CP); stale I/O from a stalled owner on a shared shelf (HBA queues outlive leases; PR preempt-and-abort on every member at takeover; softdog adds nothing here); partition combinations; one `MirrorTransport` trait is honest only as put-at-slot plus Placed with capability bits; md RAID5/6 discard default; md's periodic `checkarray` will report mismatches in torn never-committed stripes (document and run our scrub instead); the unified N-Blade on one box needs cgroups or the first OOM takes `dbladed`; DM-SMR must be excluded by model list; consumer SSDs without power-loss protection.

### 2.7 What the review changed

Fact checks: STORAGE-1 and -8 confirmed; -2 through -7 partially correct. Corrections: the cloud image flavour lacks the QLogic driver, state the kernel as "6.12.x"; discard needs the module parameter and whole-stripe alignment, the kernel never checks RZAT, and `EOPNOTSUPP` from `BLKDISCARD` means the parameter is missing; hugetlbfs cannot back fileio but mlock on it is a no-op and tmpfs `huge=always` could serve both rings; multi-huge-page coalescing is 6.12 with uniform folios and 1 GiB entries; ESXi before 9.0 needs 512-byte logical sectors on SAN LUNs, local 4Kn HDDs are emulated since 6.7, 9.0 adds 4Kn only for SSD-class devices, and sub-4 KiB writes are a minority path; the 80%-full drain is 0.3 to 0.5 GB/s under uniform overwrite.

Concessions: one tmpfs inbound ring per node (slots tagged by aggregate, per-aggregate quotas) and the FC fileio LUN is that file; the outbound arena stays hugetlbfs for the verdict's reason (uniform folios for coalescing); the link dies with the daemon, so a ring-holder process owns the files, protection domain, memory regions and the receiver queue pair (`dbladed` imports them through `ibv_import_mr`), a persisted per-slot state page and the request-age page the watchdog petter reads; `/dev/ublkcN` lives in `dbladed` and `nbladed` is control-plane only; the restart budget enters the timer table; the cut is Placed-gated and takeover replay is every valid Placed record; slot reuse needs a receiver credit under every plex variant and after dual loss authority is the Raft owner epoch, never max-select; BUSY is unimplementable and the hung-disk "degraded service" is an error-handling storm, so Stalled drives Transitioning with a hard stop; ROLLBACK is nvfail; internal NAS and S3 LUNs are 4Kn; LocalNvmeRing is the degraded device and forcing a CP per flush only without one; one of three votes is one of three; drain numbers and reserves revised; generic amd64 image only; split-role FC consumes both ports; `RingRef` is the only put source, so 6.15 zero copy is not a product tier.

Holds firm: WCE=1 with FLUSH as the Placed barrier; the ring is a file and a daemon crash is not a takeover (restart becomes a header walk of non-free slots plus an index rebuild, not a payload CRC scan); the epoch-flip cut; never-overwrite CP on md with the A/B FUA superblock stripe, the physical-address read cache, `(vol, lba, seq)` segment summaries, a CoW map with a flat Phase 1 image, XFS-on-LUN (planned giveback with a clean CP and unmount is 5 to 10 s, not 2 to 4); DRAM sets the CP interval; media-dependent HA with the shelf as the only credit-free, 100%-capacity takeover and the shelf fence as the first spike; sender-chosen slots and one allocator; thread-per-core ingest and CP reactors feeding one mirror core over SPSC queues.

### 2.8 Revised Phase 1 recommendation

Trixie 6.12, two nodes, TCP mirror, shelf for HDD, manual HA on flash. (1) `otap-ring`: both files, slot-state page, re-attach without a CRC scan, per-aggregate quotas, `Credit`, LocalNvmeRing. (2) `otap-index`: Placed-gated cut, replay-all-valid, sector masks only for the ESXi 512e class. (3) `otap-cp`: md RAID6 (12 HDD) and md RAID5 (4 NVMe) behind `Backstore` with two `MediaProfile`s, a flat map image, the A/B FUA superblock, whole-segment discard on flash. (4) In-process ublk in `dbladed` under LIO iSCSI, WCE=1, Transitioning as the only back-pressure, nvfail rollback. (5) Checker and harness: `dm-log-writes`, QEMU power cuts, PDU pulls, `dbladed` kills, hung-drive injection, an aged-pool GC run. (6) Shelf spike: MD1200 plus two 9300-8e, `sg_persist` preempt-and-abort through the expanders, assemble time, `eh_deadline` behaviour on a pulled and a hung drive. Pass criteria: 1,000 simulated and 50 real dual cuts recover to the committed cut plus every Placed record with zero acked-write divergence and nvfail engaged every time; 50 `kill -9` at 1 GB/s resume with the mirror re-established p99 2 s, worst 5 s, zero watchdog fires; HDD at least 1.2 GB/s empty with zero member reads during a CP and a published drain curve (0.4 GB/s at 70%, 0.3 GB/s at 80%); 2x drain offered for 10 minutes on ESXi 8, Windows and Linux with zero guest errors and a hung drive failed by md inside the hold; flash at least 2 GB/s and 300 K mirrored 4 KiB IOPS within 6 cores; D-Blade ack p50 60 us, p99 200 us on 10GbE TCP; RSS fixed after 24 h; boot-to-serving under 10 s at 50 TB; the PR-through-expander verdict and the plex credit lag measured with a deliberately slowed partner.

### 2.9 Risks Storage carries forward

The shelf fence is unverified; the HDD GC cliff; a hung disk versus the hold bound (SCSI error handling plus an mpt3sas host reset can outlast 10 to 20 s); the restart budget versus the watchdog's worker check-ins; plex credit on non-rejecting transports.

---

## Part 3. Opus-Protocols (block, file and object from one daemon family)

### 3.1 Executive take

Build the N-Blade as a control-plane program, not a data path. On trixie the block front end is in-kernel LIO (iSCSI, `tcm_qla2xxx`) and nvmet (TCP, RDMA) over one ublk device per LUN, with the ublk queue handlers inside the D-Blade process, so the "unified internal RPC" is a trait call with zero hops and one copy in Phase 1. File and object are XFS inside a LUN, served per SVM from a network plus UTS namespace by knfsd, Samba 4.22 and versitygw, so one primitive (move the namespace with its home aggregate) is the whole NAS failover story. The single biggest risk is file-protocol takeover semantics: NFSv4 grace and Samba's durable-handle rules turn a 10 s block takeover into a 20 to 90 s NAS stall, and no Rust fixes that. Disagreements: "Debian 12+" is not viable as written; the brief's native inode layer and per-I/O RPC layer are ONTAP-shaped costs (10 or more engineer-years, an extra hop and copy) that buy a homelab nothing XFS plus block snapshots cannot give in Phases 1 and 2; and durable handles plus kernel oplocks cannot coexist on one Samba share, so shares must be declared SMB-only or multiprotocol from day one.

### 3.2 Libraries and kernel interfaces

Kernel (trixie, verified): LIO core, iSCSI, `tcm_qla2xxx`, `tcm_fc`, `tcm_loop`; nvmet TCP, RDMA, FC, FCLOOP, LOOP with AUTH (DH-HMAC-CHAP) and TCP TLS (needs `tlshd`); reservations arrive in 6.13 and are not on trixie; ublk with recovery, reissue and user copy, without fail-io (6.13), zero copy (6.15) or per-I/O daemons (6.16); TCMU as a fallback only (no initiator identity); knfsd with per-namespace instances; ksmbd (durable handles since 6.9, Phase 2 experiment only); watchdogs; md-cluster present but unused. Userspace: Samba 2:4.22 (`vfs_acl_xattr`, `streams_xattr`, `fruit`, `shadow_copy2`), nfs-utils 2.8.3 (`rpc.nfsd --scope`, `nfsdcld -p -s`), targetcli-fb for debugging only, versitygw v1.8.0 (Go, Apache-2.0, POSIX backend with object lock, experimental versioning, multipart, bucket policies, six IAM backends, no lifecycle, no object ACLs; shipped as a static binary), ktls-utils for NVMe/TCP TLS later, nvme-cli, multipath-tools, open-iscsi for test initiators. Crates: `libublk` 0.4.8, `io-uring` 0.7, `tokio` (control), `zbus` (systemd D-Bus), `rtnetlink`, `nix`/`rustix` (namespaces, mounts), `inotify` (APTPL file), `rustls`/`axum` (mTLS management API), `crc32c`; `s3s` 0.17 is a Phase 3 option only; not used: nfsserve (NFSv3 only), fuser (FUSE cannot grant leases), any Rust iSCSI or NVMe-oF target.

### 3.3 Architecture

**Components per node.** `nbladed` (control plane, no I/O) reconciles Raft-derived desired state into kernel and daemon state through modules `blade_lio`, `blade_nvmet`, `svm`, `blade_nfs`, `blade_smb`, `blade_s3`, `identity`, `state_export` (APTPL, nfsdcld), `firewall`; per-SVM children run as systemd units in a slice with a network namespace path, driven over D-Bus. `dbladed` holds one `UblkDevice` per LUN, one io_uring per queue thread, user copy on `/dev/ublkcN` so the kernel copies request data straight into a ring slot. Kernel: LIO iblock over `/dev/ublkbN`, nvmet block-device namespaces over the same, knfsd, XFS. Per-SVM children: rpc.nfsd, rpc.mountd, nfsdcld (and rpc.gssd when AD-joined), smbd plus winbindd with a per-SVM `smb.conf`, versitygw.

**Data flow.** Block: initiator, LIO or nvmet, bio, ublk, `dbladed` queue thread, `BlockService::submit`, ring put plus mirror, ack, ublk commit, target completion. File: client, knfsd or smbd, VFS, XFS, bio, same path. Object: versitygw, syscalls, XFS, same path. Every protocol ends in one block abstraction; the only custom data-path code is the ublk front door and the engine.

**One device from two nodes.** SCSI: `vpd_unit_serial` = volume UUID (LIO derives the NAA-6 identifier), fixed vendor and product strings, the same IQN on both nodes with TPGT 1 on A and 2 on B, one ALUA target port group per aggregate (group id = aggregate id) created on both nodes with only local ports linked, explicit relative target port identifiers (A 1 to 99, B 101 to 199), `emulate_write_cache=1`, `emulate_tpu/tpws/3pc/caw=1`, ALUA and PR support on, CHAP in the TPG. NVMe: identical NQN, serial and model, namespace UUID = volume UUID, disjoint controller id ranges, ANA group id = aggregate id with every port publishing both groups. NAS: SVM name = UTS nodename = NFS server scope = NetBIOS name, `fsid` = hash(volume UUID), and the SVM root volume holds nfsdcld's database, Samba's private, state, lock and cache directories, the keytab, TLS certificates and versitygw's IAM directory, so the survivor restarts the same server identity.

**Takeover on the survivor (revised).** Both nodes carry byte-identical target trees differing only in portal addresses, port identifiers and controller id ranges; devices, backstores, LUNs and namespaces are pre-created on both nodes with the standby's gate closed; takeover is the ALUA/ANA group flip only (target under 50 ms), then XFS mounts (log replay), namespace creation, LIFs with gratuitous ARP, nfsd in grace with the replicated client database, smbd, versitygw.

**Stateless, per protocol.** iSCSI and FC: sessions re-login, PRs survive only under the `pr_survival` policy, ALUA state is Raft-derived. NVMe: controllers are per node, hosts reconnect, no reservations on 6.12. NFSv4: client state recovered by the client in grace from the replicated nfsdcld database. SMB: sessions and handles are lost; the machine account, deterministic idmap and share configuration live on the SVM root. S3: no session state; multipart parts and lock and version metadata are files and xattrs on the volume.

**Security posture.** Mutual CHAP per TPG, DH-HMAC-CHAP on nvmet, TLS PSK later; NFS `sec=sys` with export ACLs and `krb5p` when AD-joined; SMB3 minimum with signing and encryption desired; S3 over TLS with SigV4; the management API (axum, rustls, mTLS) on the management VLAN; per-namespace nftables default-deny with a per-LIF allowlist.

### 3.4 Bottlenecks

4 KiB write latency budgets at 10GbE with a 25 to 35 us TCP mirror (estimates): iSCSI 70 to 100 us without the mirror, 100 to 150 with; NVMe/TCP 40 to 60 and 80 to 120; FC 8G through `tcm_qla2xxx` 100 to 150 and 140 to 200; NFSv4.1 sync write 200 to 300 and 300 to 450; SMB3 sync write 250 to 350 and 350 to 500; S3 small PUT 1 to 3 ms. Async NFS and SMB writes batch to line rate; the mirror RTT is the floor for every sync path. Copies and CPU at 10GbE line rate: iSCSI three copies at 1.1 GB/s is about half a core plus a tenth for CRC (the data-digest cost is about 0.1 core, not a core, corrected by Fabrics); FC two copies; NFS three copies and knfsd needs two to three cores; SMB four copies with smbd at 0.6 to 0.9 GB/s per core; S3 about two cores per GB/s. Cores on a homelab box with iSCSI, NFS, SMB and S3 live: mirror poll 1, ublk handlers 1 to 2, engine 1 to 2, kernel targets 1, knfsd 1 to 2, smbd 0.5 to 1 per active client, versitygw 1 to 2 during backups, core 0 for the OS: 8C/16T minimum, 12C comfortable; memory 64 GB floor, 128 GB comfortable. Queue depth: 16 LUNs times depth 128 is 2,048 outstanding, but filling 10GbE at 30 us RTT needs only 33 KB in flight, so latency, not bandwidth, drives the mirror design. Takeover windows clients see: iSCSI and FC the ALUA flip (10 s p99 target) with ESXi's 35 s only on a silent node; NVMe immediate after reconnect; NFSv4.1 reconnect plus reclaim with grace blocking new opens; SMB sessions reset and handles lost (10 to 60 s, in-flight writes fail); S3 connections reset, Veeam retries. The two-block-layer tax for file: XFS over ublk adds one queue hop (about 10 us) and XFS metadata randomness on HDD, so XFS metadata must sit in the SSD read cache; online discard is cheap under the engine (a map hole, batched per log commit).

### 3.5 Creative solutions

- **SVM = network namespace plus UTS namespace.** Running each SVM's nfsd, mountd, nfsdcld, smbd and versitygw inside `unshare -n -u` with the SVM name as nodename makes knfsd's NFSv4 scope and major id the SVM name on whichever node hosts it, gives Samba a NetBIOS identity, isolates ports and firewall rules, and makes "LIF migration" one primitive for all three NAS protocols.
- **ublk front door inside `dbladed`, user copy into the ring.** A separate ublk server process costs a shared-memory hop and a wakeup and doubles reissue duplicates across two failure domains.
- **Previous Versions and `.snapshot` for free.** fsfreeze (milliseconds), D-Blade snapshot, unfreeze; snapshots mounted `ro,nouuid,norecovery`; `vfs_shadow_copy2` exposes them as Windows Previous Versions; single-file restore through reflink across the snapshot mount.
- **ALUA/ANA group = aggregate, published by both nodes**, so the target configuration is byte-identical and can be diffed as a health check.
- **Fail-closed completion gate.** A shared lease deadline and owner epoch checked on every ublk completion is the safety signal; the ALUA flip is the courtesy signal.
- **Ship SCSI-3 PRs across takeover** by watching LIO's APTPL file (revised to a per-LUN policy with a documented one-operation regression window).
- **Share classes instead of a new filesystem**: `smb-only` (durable handles, SMB2 leases) or `multiprotocol` (kernel oplocks, no leases or durable handles, share modes never enforced against NFS).
- **systemd as the supervisor**: per-SVM slices with CPU weight, memory limits derived from DRAM minus pinned memory, core 0 free, a network namespace path.

### 3.6 Holes Protocols found in the plan

Stock bookworm cannot run the design (no ublk, no nvmet auth or loop, nfs-utils without `--scope`); the per-I/O "unified RPC layer" is a tax with no Phase 1 payoff in a two-node pair; durable handles and cross-protocol correctness are mutually exclusive in Samba; NFSv4 grace can erase the takeover budget; one `MirrorTransport` trait cannot hide SCSI error handling without capability bits; volatile DRAM changes FLUSH and GrantUnmirrored quietly removes durability; a hung node that still answers NOP-Out is worse than a dead one (fence by silence); node-local disks make Standby paths a lie, so manual mode must present single-node targets; thin LUNs under XFS turn aggregate-full into a filesystem shutdown (file volumes thick-reserved including the GC reserve and snapshot deadlists); VAAI is LID1 XCOPY between LUNs on the same session only and there is no ODX (confirmed); Windows needs iSCSI for a long time (confirmed: no inbox NVMe-oF initiator in Windows Server 2025 or Windows 11, the Insider preview has no multipathing); Ethernet-only quorum strands FC clients; busy-polling eats a homelab's cores.

### 3.7 What the review changed

Fact checks: PROTOCOLS-1, -2, -4, -5 and -8 confirmed; -3, -6 and -7 partially correct. Corrections: Samba grants durable handles with `posix locking = yes` as long as the handle holds no byte-range locks, so the SMB-only class keeps POSIX locking on; `rpc.nfsd --scope` is implemented as a UTS unshare plus `sethostname` in the rpc.nfsd process, the kernel copies the caller's nodename into the per-namespace scope on every `nfsd_svc()` call, so `nbladed` sets the SVM hostname itself, mounts nfsd and rpc_pipefs per namespace and issues every nfsd control call from inside the SVM; versitygw's POSIX versioning is labelled experimental, object lock needs xattrs and an explicit `--object-lock-mode`, Garage has no released versioning or object lock, and MinIO's community repository was archived in April 2026.

Concessions: ADD_DEV at takeover was wrong, devices are pre-created on both nodes and takeover is the flip only; DEL_DEV as a fence hands errors to guests (Broadcom KB 345174: only ALUA or NOT READY sense and no-connect host status trigger failover), so the fence is silence and `fence()` leaves the trait; a healthy node that loses its lease stops exporting; the gate is per aggregate (a slot in the epoch page); one thread per LUN was wrong, ublk queues live on Storage's per-core reactors and the TCP mirror wait uses NAPI registration; rings leave any slice with a memory limit; a `dbladed` crash is an all-LUN recovery that must fit the 2 s budget with a structural `req_id` = (device, queue, tag, generation); NFS lease 10 s punishes every 10 s stall, so Protocols moved to 30/45 with NFSv3 for ESXi datastores (then crossed with Consensus, resolved in ruling R8); unplanned takeover loses up to `dirty_expire` of acknowledged SMB writes, so `dirty_expire_centisecs=200`, `dirty_writeback_centisecs=100`, `strict sync` and a printed "SMB at most 2 s" line; the iSCSI digest costs 0.1 core; sub-second SG_IO timeouts were the wrong fix for FC; `qlini_mode=dual` is module-wide and `nbladed` is the sole configfs, sysfs and modprobe.d writer; under plex Standby paths are real; PR shipping can regress one operation; the lab adds a ConnectX-4 pair.

Holds firm: kernel floor 6.12 with the installer probing `/sys/module/ublk_drv`; XFS-on-LUN through Phase 2 (NAS takeover time is protocol state, not the filesystem); the external takeover promise of 10 s p99 and 15 s hard with the RST proxy keeping "no floating IPs for block" because the borrowed address never serves I/O; BUSY might exist for SCSI if ublk's `-ENOMEM` maps through `BLK_STS_RESOURCE` to SAM BUSY in `iblock_complete_cmd` (unverified on 6.12; one-day spike); hung-but-alive is worse than dead; share classes, versitygw, iSCSI first for Windows, the VAAI limits, no per-I/O RPC wire format, the FC mirror needs tmpfs and is not Phase 1.

### 3.8 Revised Phase 1 recommendation

Platform: trixie 6.12 (bookworm-backports accepted, 7.x in the test matrix); two nodes, 10/25GbE plus a ConnectX-4 pair; a QLE2562 pair for FC-SCSI client testing; HA tier per the lead's decision (the front end is identical, only takeover timing differs); clients ESXi 8, Proxmox, Windows 11 and Server 2025, macOS, Linux, Veeam. Build order: the ublk front door on the reactors (pre-created devices on both nodes, recovery and reissue flags, user copy into ring slots, the epoch page, structural `req_id`, a null engine first); `blade_lio` (single configfs writer, pre-staged identical trees, group flip, `stop_exporting`, APTPL shipping with the documented window, the RST-proxy test hook); `blade_nvmet` (TCP and RDMA ports, ANA group per aggregate, DH-HMAC-CHAP); SVM namespaces (knfsd, Samba share classes, `vfs_shadow_copy2`, deterministic idmap); versitygw per SVM; client recipes (multipath.conf, ESXi claim rules, Windows MPIO). Success criteria: ublk null round trip p50 10 us on the reactor; daemon kill and recovery of 16 LUNs times 128 tags within 2 s with zero initiator errors; iSCSI 4 KiB QD1 write with mirror p50 150 us and p99 400 us, NVMe/TCP p50 120 us, 1.0 GB/s sequential on two cores for target plus ublk plus engine front end; 100 forced takeovers of each class with the flip under 50 ms, I/O resuming within 10 s p99 on ESXi, dm-multipath and Windows MPIO, zero guest errors; configfs dumps differing only in addresses; NFSv3 datastore pause equal to the block ladder; NFSv4.1 resume within 45 s worst with early-lift behaviour documented; SMB reconnect within 20 s with measured unplanned loss at most 2 s; Veeam immutable repository tests pass with a takeover mid-job; the CPU and memory table validated on 8 cores and 64 GB with all four protocols live.

### 3.9 Risks Protocols carries forward

NAS RTO is protocol state (20 to 45 s unless early grace end works); silent death on ESXi rests on the unproven RST proxy; 6.12 ublk limits (no fail-io, quiesce or online resize, so LUN grow is a planned relocation); shelf versus plex undecided; durability promises users will not read (the SMB 2 s window, the PR regression, versitygw's experimental versioning, iblock BUSY).

---

## Part 4. Opus-Consensus (3-voter Raft, leases, fencing, failover state machine)

### 4.1 Executive take

Build a lease-centred control plane, not a fence-centred one: openraft 0.9.25 on Node A, Node B and a non-electable witness (`enable_elect=false`, confirmed) holding a sub-megabyte state machine; leases granted by the Raft leader through `ensure_linearizable()` (one quorum heartbeat round, no log write, so a Raspberry Pi on microSD survives as the witness); every I/O checked lock-free against a seqlock epoch page at submission and at completion; takeover as a Raft-committed, idempotent plan driving block flips and NAS migrations from one executor. Watchdog, BMC, PDU and SCSI-3 PR are media fences and accelerators; the safety proof rests on the lease arithmetic alone. The single biggest risk is the shared-shelf path: late DMA from a wedged owner (md stripe-cache drain 1 to 2 s, SCSI timeouts up to 30 s) can land on segments the survivor just allocated, and softdog cannot bound that, so the shelf requires drive-level preempt-and-abort on SAS drives or a confirmed power fence. Disagreements with the revised plan: prefix replay silently drops acknowledged writes; the 8 s watchdog bound is not on the survivor's critical path if the data path checks leases at completion (the real floor is lease TTL plus the leader-change wait, about 4.3 s); lease renewals must not be Raft log entries.

### 4.2 Libraries and kernel interfaces

openraft 0.9.25 (stable; confirmed: `enable_elect`, `enable_heartbeat`, `ensure_linearizable()`, joint-consensus membership, with the caveats that `Trigger::elect()` bypasses the flag, that `ensure_linearizable()` is a read-index round trip rather than a lease, so the zero-fsync lease layer is OpenTAP's own, and that 0.9 carries no API stability promise), with the storage traits on redb 2.x and `fdatasync` per append; fallback tikv raft-rs 0.7. tokio for the control daemon; the Raft RPC on TCP plus mTLS over every Ethernet path. Fencing: the Linux watchdog API (`WDIOC_SETTIMEOUT` 5 s, keepalive every 500 ms), drivers in order of trust `ipmi_watchdog`, `hpwdt`, `iTCO_wdt` (often inert because of the BIOS NO_REBOOT bit; test at install by letting it fire once), `sp5100_tco`, then softdog (confirmed: an hrtimer on CLOCK_MONOTONIC that migrates between CPUs on each ping, firing `emergency_restart()`; on Debian `soft_panic=1` hangs forever because `CONFIG_PANIC_TIMEOUT=0` unless `kernel.panic` is set; `hardlockup_panic=1` closes the IRQ-off case softdog cannot); fence-agents (exec with the standard stdin protocol, `fence_ipmilan`, `fence_redfish`, APC SNMP), Shelly and Tasmota outlets as 30-line fences, every power fence confirmed before it counts; SCSI-3 PR via `SG_IO` or `sg_persist` on SAS drives only. Kernel targets and NAS pieces the control plane drives: LIO's ALUA state attributes, `res_aptpl_metadata` and `force_pr_aptpl` (corrected: gated by the device's export count, write-only, the metadata lives in `$dbroot/pr/aptpl_<serial>` written synchronously inside PR OUT, OpenTAP must replicate it and strip `initiator_sid`); nvmet `ana_grpid` and `ana_state`; nfsd scope, lease and grace, `v4_end_grace`, nfsdcld; Samba and ksmbd durable handles (neither replicates handles across nodes; Samba 4.25 adds experimental persistent handles under CTDB only, Debian sid). Testing: TLA+ with Apalache, stateright, madsim or turmoil, loom, a Porcupine-style linearizability checker port, `tc netem`.

### 4.3 Architecture

**Processes.** `otap-ctl` per node, pinned to one core, `mlockall`, `SCHED_FIFO`: openraft voter, lease client, fence executor, plan executor, epoch-page writer, watchdog petter, heartbeat sender on every path; budget one core, 200 MB, 30 messages per second. The D-Blade and the protocol blades only read the epoch page. Witness: `otap-ctl --witness`, 1 vCPU, 512 MB, 2 GB disk, fsync rate near zero in steady state; RTT budget 100 ms with p99 jitter 200 ms; a path independent of the mirror cable, not necessarily of the LAN switch.

**Replicated state (under 1 MB).** CA roots, nodes, aggregates `{owner, home, epoch, mode: Mirrored | Unmirrored{granted_epoch} | Takeover{step} | Giveback{step} | Down, media}`, SVMs, LUN identities (NAA, NGUID, port ids, APTPL text). Operations: takeover, barrier progress, giveback, grant and revoke unmirrored, place SVM, set PR, set desired ALUA/ANA, configuration deltas. Never in Raft: lease renewals, heartbeats, ring traffic, CP decisions, per-I/O checks, SMB handles, NFS state.

**Leases (the safety argument).** The owner sends `RenewLease{agg, epoch}` every 500 ms; the leader checks owner, epoch and mode, runs `ensure_linearizable()`, records the grant on its own monotonic clock and replies; the holder sets expiry = send time + 3 s on CLOCK_MONOTONIC and publishes it in the epoch page. Safety needs bounded clock rate drift only. L1: a new leader waits TTL plus margin (3.1 s) after its term starts before committing any takeover. L2: a takeover commits only after max(partner silent 2 s, last grant plus 3.1 s). L3: the data path checks (epoch, state, lease) at submission and completion; a failed check is a hold or Transitioning, never an ack. L4: `otap-ctl` pets the watchdog only if every data-path worker checked in within 1 s and no owned aggregate has been export-active past lease expiry; the 5 s watchdog plus about 3 s of reset caps how long a sick node can hold clients hostage and is not on the survivor's critical path.

**Failover state machine and the ladder (silent A, LAN witness).** 0.02 to 1.0 s ring heartbeats missing (evidence only); 1.0 to 1.2 s B wins the election with the witness; 2.0 s partner silent on all control paths (BMC "Off" is positive evidence, "On" is nothing); 4.3 s L1 satisfied, `Takeover{X, B, e+1}` committed, B's ports Transitioning; 4.4 s shelf: PR preempt-and-abort on all members in parallel (about 100 ms) and old-epoch inbound teardown; plex: nothing; power fence in parallel off the critical path; 4.5 to 7 s assemble, superblock max-select, segment table, residual scan, epoch flip, takeover CP, Active/Optimized and ANA Optimized with the PR table loaded before export; 7 to 9 s SVM namespace, ublk plus XFS mount, LIFs with gratuitous ARP, nfsd in grace with the replicated client database, smbd, S3; then the CP interval shortens to 2 s while unmirrored. Block p50 5 s on plex, 6 to 8 s on a shelf. Wedged node: pet withheld at lease expiry or 2 s after a worker stall, watchdog 5 s, reset about 3 s, re-login 1 s: 11 to 13 s, hard 15 s. Planned takeover and giveback: client-visible pause 2 to 4 s (revised to 5 to 10 s with XFS unmount and md assembly on a shelf).

**Fence policy by media.** Plex (node-local arrays): lease plus any watchdog, softdog accepted; accelerators BMC and PDU. Shelf with SAS drives: lease plus PR preempt-and-abort, hardware watchdog and BMC or PDU as accelerators. Shelf without PR: refuse to enable HA unless a confirmed power fence exists.

**Giveback.** The returning node joins as a follower and publishes no ownership until synced; LIO and nvmet exports are created only after a grant; superblocks with an epoch above the granted one are refused. Plex: resync segments since the last CP. Shelf: re-register PR as non-holder. The survivor forces a CP, streams the ring from that boundary, and when the returning ring covers the window the mirror is live; auto-giveback after 10 minutes stable or the nightly slot.

### 4.4 Bottlenecks

Takeover floor: TTL 3 s plus 0.1 s margin after the new term is 4.3 s before anything can commit, plus 0.3 to 0.7 s of flips and replay; shortening TTL to 2 s buys 1 s and exposes any 2 s control-daemon stall as a false takeover (keep 3 s). Client-side detection dominates the unplanned case (ESXi iSCSI about 35 s on a silent target; Windows iSCSI holds 15 s with MPIO or 60 s without and sends no NOP-Out; Linux dm-multipath fails at 15 s unless surviving ports report Transitioning and `no_path_retry queue` is shipped; FC initiators get RSCN within a second; ESXi NFSv3 holds VM I/O for about 113 to 130 s at stock heartbeat settings). Witness RTT: steady-state commits need A and B only; degraded, each renewal is one RTT and takeover adds two. NFSv4 grace and SMB handles (above). Memory and CPU: the standby index on the partner is pointers only; the control plane is one core; the Raft log under 100 MB with snapshots. Shelf hazard: the owner's stripe cache drains for 1 to 2 s after its last submit and a hung drive stretches that to the SCSI timeout; without PR the only safe wait is 15 s or more.

### 4.5 Creative solutions

- **Leader-granted leases with no log writes**, and a **non-electable witness** with leadership pulled back to a data node by `trigger().elect()`.
- **RST proxy for a dead node's block IPs.** After the fence point, the survivor briefly claims the dead node's iSCSI and NVMe/TCP portal addresses and answers every segment with TCP RST, serving nothing there; initiators drop the session immediately and fail over to pre-logged-in paths. Revised to run only on positive fence evidence; its real payoff is Windows, which sends no NOP-Out.
- **Two-arena allocation and max-select superblocks** so late DMA from the old owner cannot hit anything the survivor wrote; revised to defence in depth with per-node superblock slots, because md's own superblock and bitmap sit at fixed addresses.
- **Plex by log replay** (withdrawn in round 3 for identical-geometry by-reference).
- **Rings and the dirty index in files**, **LocalNvmeRing** (the slot-addressed ring written to a local PLP namespace turns single-node and GrantUnmirrored operation into locally persisted at NVMe latency), **SVM = network namespace**, **data-path liveness folded into the pet** with a stall-to-release policy (revised: node-local stalls only, proposal at 5 s), **replay all CRC-valid records**, **the witness as synthetic client** recording the real client-visible pause of every nightly rehearsal into Raft so the dashboard shows "last failover: 2.8 s, 03:00 today".

### 4.6 Holes Consensus found in the plan

Prefix replay loses acknowledged writes; one trait hides three ack semantics (collapsed in round 3 to `placed_by`); GrantUnmirrored must be chosen (grant, alarm loudly, shorten CPs, make forced takeover an operator command that prints the exact seconds of acknowledged data it will discard); softdog is adequate only where the media is private; shared-shelf fencing on consumer hardware; openraft's "leader lease" is follower-side vote rejection, so a partitioned leader may believe it leads indefinitely and `ensure_linearizable()` failing is the only signal; Debian 12's 6.1 lacks ksmbd durable handles, NAPI registration, nfsd netlink and recent ublk flags; XFS-on-LUN as an HA liability (overruled: exclusive mount is the same fence and log replay is bounded); control heartbeats on every Ethernet path are not enough when the LAN switch dies and the mirror is a direct cable; the NAS LIF design was absent from the brief; virtualized data nodes need the paused-VM clock rule (re-read CLOCK_MONOTONIC after every blocking call and refuse to ack if the page's expiry is in the past); every piece of state that must outlive a node lives in exactly one of two places, Raft or the aggregate.

### 4.7 What the review changed

Fact checks: CONSENSUS-1 and -6 confirmed; -2, -3, -4, -5, -7 and -8 partially correct. Corrections: `rpc.nfsd --scope` is trixie-only (bookworm's 2.6.2 lacks it) and identical scope alone yields nothing without the replicated nfsdcld database and a grace period on the takeover node; early grace end requires every client of the previous epoch to return and send RECLAIM_COMPLETE, grace >= lease is not enforced, lockd's grace overrides it when NFSv3 is enabled, and an empty nfsdcld database skips grace entirely; the PR mechanics (above); softdog's hrtimer migrates and `soft_panic` needs `kernel.panic`; Windows and ESXi timers conflate detection with hold time, which makes the RST proxy most valuable for Windows; Samba 4.25 persistent handles under CTDB exist in sid only.

Concessions: the ring mailbox withdrawn (it ties control liveness to `dbladed` and inherits FC timeouts) for an `otap-ctl`-owned sideband; the ladder and proxy assumed a silent node, so two bounds are published and the proxy is gated on evidence; stall-to-release at 10 s was too late and ping-pongs on a shelf; two-arena does not fence an md shelf; geometry-independent plex dropped; PR survival versus pre-staged paths is a per-LUN policy; NFS moved to 10/20 with NFSv4.1-only exports, replicated nfsdcld and a controller-written `v4_end_grace` (crossed with Protocols; ruling R8); the shelf ladder gains storage steps and L3 gates CP submits and the superblock write; BUSY removed; `AckLevel` collapses to `placed_by`; LocalNvmeRing and spill-on-silence merge into one local link; XFS-on-LUN stays; versitygw replaces s3s; trixie pinned.

Holds firm: leases carry the safety proof and fences accelerate; the non-electable witness; replay all CRC-valid records; lease TTL 3 s; a control path over the mirror fabric is mandatory for a direct DAC with a dead switch; GrantUnmirrored means grant, alarm, 2 s CPs and a forced takeover that prints the loss; plex as the long-term homelab default because softdog plus private media is the only fence tier consumer boards without a BMC or SAS can honestly meet; SVM = namespace, block flips before NAS IPs move, SMB labelled disruptive.

### 4.8 Revised Phase 1 recommendation

`otap-ctl` complete on trixie: pinned openraft 0.9.25 on redb, leader-granted leases, the gate page, barrier commits, a fence executor with evidence (softdog, iTCO or ipmi_watchdog, `fence_ipmilan` or `fence_redfish`, one Shelly or Tasmota outlet, PR via `sg_persist`, ARP probing), a watchdog petter consuming link health, TCP mTLS and the fabric sideband. Testbed: two data nodes on a dual-attached MD1200-class shelf with PR-capable drives, a witness run as both a Proxmox VM and a Pi, a stub `dbladed` as its own process (`otap-ctl` never holds `/dev/ublkcN`), LIO iSCSI with ALUA and both PR policies, one SVM namespace with nfsd and Samba, a versitygw canary bucket. Prove: a TLA+ model of leases, epochs, GrantUnmirrored, replay-all and barriers ("no overlapping acks per aggregate in real time", "every acked write is replayed"); 100,000 madsim or turmoil schedules over the partition matrix with zero violations and loom on the gate page; 100 unplanned takeovers on real gear with silent-node block resume p99 9 s, wedged 13 s, hard 15 s on every run, Windows MPIO reconnecting inside 15 s through the gated proxy, ESXi within 10 s with the shipped recipe, and the checker showing zero acked-write loss and no stale reads; NAS reconnect plus I/O on reclaimed state within 8 s and new-open latency within 20 s; planned takeover and giveback within 3 s client-visible over 30 nightly rehearsals measured by the witness; GrantUnmirrored within 2 s with the correct loss printout; dual power loss converging with no operator input; the witness at 100 ms plus or minus 50 ms passing everything with the Pi logging under 100 fsyncs per day; fence quality enforced (HA refused on a shelf without PR or a confirmed power fence; each watchdog driver self-fires at install; `soft_panic` without `kernel.panic` flagged).

### 4.9 Risks Consensus carries forward

Wedged-node recovery rests on the watchdog plus a platform reset (SMI hangs, paused VMs and failed resets are unbounded; FC clients have no RST equivalent); NFS grace defeated by one absent or v4.0 client and by lockd; md shelves without PR-capable SAS members cannot be made safe; ownership fragmentation (who holds `/dev/ublkcN`, one ring file type, one orchestration owner, one gate-page format); openraft 0.9 carries no API promise and the lease layer is ours to model-check.

---

## Part 5. Adversarial critique

### 5.1 Cross-examination: the conflicts that crossed the table

Each specialist cross-examined the other three and listed the cross-cutting conflicts. Eleven survived into round 3 or were created by it; the ruling that settles each is in Part 6.

| # | Conflict | Positions after round 3 | Ruling |
|---|---|---|---|
| 1 | Inbound ring files | Storage conceded one tmpfs file per node; Fabrics' revised trait is per (aggregate, direction) | R3 |
| 2 | Record format and slot-map ownership | Three slot maps (Storage lap allocator, Fabrics batch pages, Protocols `reserve_slot`); owner only "unless reassigned" | R3 |
| 3 | Control sideband over the mirror fabric | Consensus withdrew the ring mailbox for an `otap-ctl`-owned channel; Fabrics kept a mailbox region; Protocols wanted a second NIC, then withdrew it | R6 |
| 4 | Plex variant | Storage: by-replay general, by-reference an optimization; Consensus: identical-geometry by-reference only; Fabrics: credits needed under either | R2 |
| 5 | HA tier in Phase 1 and the long-term default | Storage and Consensus (revised): shelf in Phase 1; Consensus: plex the long-term default; Protocols tier-neutral; homelab critic: shelf, drop by-replay and spill | R1 |
| 6 | NFS defaults | Protocols moved 10/20 to 30/45 with NFSv3 datastores; Consensus moved 30/45 to 10/20 with NFSv4.1-only | R8 |
| 7 | Hold hard bound and self-takeover | Consensus 5 s proposal for node-local stalls; Protocols hard stop near 20 s; Storage defers | R7 |
| 8 | BUSY | Protocols: possibly SAM BUSY through iblock (unverified); Storage and Consensus: deleted | R7 |
| 9 | Degraded durability | Storage: LocalNvmeRing Phase 1; Consensus: PLP-only Phase 3 with 2 s CPs; Protocols: forcing a CP per flush collapses NAS and S3; Fabrics: sync-to-disk without a UPS | R10, R11 |
| 10 | Kernel command line and the timer-table owner | Fabrics `panic=-1`; Consensus argued `panic=-1` then shipped `panic=5` (13 s stack); nobody owns the table | R7 |
| 11 | Kernel matrix | Protocols: 7.x in the matrix; Storage: not a product tier; Fabrics: an optional lane | R12 |

Conflicts settled inside round 3 without a ruling: whole-block descriptor rewrites (withdrawn), the second bulk link for plex (withdrawn), replay rule (replay-all adopted by all four), ADD_DEV at takeover (pre-created devices adopted), DEL_DEV as a fence (silence adopted), one thread per LUN (per-core reactors adopted), TCMU for the FC receive path (fileio on tmpfs held), ring mailbox (withdrawn), geometry-independent plex (withdrawn), the 2 s daemon-restart claim (budgeted at 2 s target and 5 s hard with supervised-restart suspension), lease loss on a healthy owner (stop exporting, stay up), the latency measurement point (in-process versus client-visible, both stated).

### 5.2 Fact checks: 32 load-bearing claims, 13 confirmed, 19 partially correct, 0 refuted

Every verdict is sourced in the record. The corrections that changed the design:

| Claim | Verdict | What changed |
|---|---|---|
| FABRICS-1, STORAGE-1, STORAGE-2, PROTOCOLS-1, PROTOCOLS-2 (Debian kernel configs) | confirmed / partially correct | Floor is 6.12.x (trixie or bookworm-backports), generic amd64 image only; state versions as ranges; the cloud flavour lacks qla2xxx |
| FABRICS-2 | partially correct | ublk alone disqualifies stock bookworm; nvmet loop transports were irrelevant |
| FABRICS-3 | confirmed | rtslib-fb exposes the qla2xxx fabric; saveconfig lives at `/etc/rtslib-fb-target/`; sid already has rtslib 3.0 |
| FABRICS-4 | partially correct | jonhoo ibverbs uses the legacy path and works on mlx4; sideway needs `build()` not `build_ex()`; probe `ibv_wr_*` at runtime |
| FABRICS-5, STORAGE-4 | confirmed / partially correct | fileio on tmpfs is one kernel copy; hugetlbfs cannot back fileio; mlock on hugetlbfs is a no-op |
| FABRICS-6 | partially correct | trixie ships ql2500 8.07.00; QLE2672 is ISP83xx (flash-first); 8G cards silently downgraded unless `ql2xfwloadbin=1` |
| FABRICS-7 | partially correct | asymmetric Linux-to-Linux N2N has public history; only symmetric dual mode is unreported |
| FABRICS-8 | partially correct | 8GFC put-to-Placed 80 to 200 us; RoCE about 10 us median; ranking stands |
| STORAGE-3 | partially correct | discard needs the module parameter at array start, whole-stripe alignment, no kernel RZAT check; `EOPNOTSUPP` means the parameter is missing |
| STORAGE-5 | partially correct | multi-huge-page coalescing is 6.12 with uniform folios; 1 GiB per registered entry; clone before any other registration |
| STORAGE-6 | partially correct | 512-byte logical sectors mandatory for ESXi SAN LUNs before 9.0; 4Kn only for SSD-class devices on 9.0; sub-4 KiB writes a minority path |
| STORAGE-7 | partially correct | 0.3 to 0.5 GB/s at 80% full under uniform overwrite; reserves 70/80/90 |
| STORAGE-8 | confirmed | libublk 0.4.8 and crc-fast 1.10 current; pin and vendor, libublk yanks releases |
| PROTOCOLS-3 | partially correct | durable handles survive with `posix locking = yes` unless byte-range locks are held |
| PROTOCOLS-4 | confirmed | LIO XCOPY is LID1 on one session on one node; no ODX on any Debian kernel |
| PROTOCOLS-5 | confirmed | 6.12 ublk: recovery, reissue, user copy; no fail-io, zero copy, auto buffer registration or per-I/O daemons |
| PROTOCOLS-6, CONSENSUS-2 | partially correct | `--scope` is UTS unshare plus `sethostname` in rpc.nfsd (trixie only); the scope is rewritten from the caller's nodename on every `nfsd_svc()`; replicated nfsdcld is the load-bearing piece |
| PROTOCOLS-7 | partially correct | versitygw versioning is experimental; object lock needs xattrs and `--object-lock-mode`; Garage lacks versioning and object lock; MinIO archived |
| PROTOCOLS-8 | confirmed | no inbox Windows NVMe-oF initiator; the Insider preview lacks multipathing; iSCSI stays mandatory |
| CONSENSUS-1 | confirmed | non-electable witness is sound; `Trigger::elect()` bypasses the flag; the lease layer is OpenTAP's own |
| CONSENSUS-3 | partially correct | early grace end needs every prior-epoch client; grace >= lease unenforced; lockd grace overrides; empty nfsdcld skips grace |
| CONSENSUS-4 | partially correct | PR metadata gated by export count, write-only, a file OpenTAP must replicate, `initiator_sid` stripped |
| CONSENSUS-5 | partially correct | softdog's hrtimer migrates; `soft_panic` needs `kernel.panic`; `hardlockup_panic=1` closes the IRQ-off case |
| CONSENSUS-6 | confirmed | five watchdog drivers and ksmbd durable handles on trixie; durable v2 CVEs in 2026 |
| CONSENSUS-7 | partially correct | timers conflate detection with hold; the RST proxy matters most for Windows |
| CONSENSUS-8 | partially correct | no Debian Samba or ksmbd carries handles across nodes; Samba 4.25 persistent handles under CTDB exist in sid only |

### 5.3 Completeness critic

**Nobody owns these** (the phase in which the decision must exist): management plane, API, CLI, cluster bootstrap and installer (Phase 1); observability: a metrics schema, per-I/O trace through ublk, ring, mirror and CP, alert transport (Phase 1); CI, lab plan and bill of materials keyed to the week each spike needs it (Phase 1); upgrades, rollback and version skew, with every persistent and on-wire structure versioned under an N/N+1 promise (rules Phase 1); the privilege model and secrets (every daemon is implicitly root; CHAP keys and keytabs "from Raft secrets" today means plaintext on the witness's microSD) (Phase 1); the core and memory budget with all protocols live, including an isolcpus, nohz_full and IRQ-affinity profile (Phase 1); performance methodology (Phase 1); md operations on a PR-fenced shelf, including PR registration of a replacement member before `mdadm --add` (Phase 1); UPS integration through NUT (Phase 2, cheap); data reduction and encryption (reserve a physical-length field in the extent now; build in Phase 3); thin-provisioning attributes across protocols (Phase 1); identity and ACL mapping (Phase 2; SVM-root layout Phase 1); S3 behaviour after a rollback (Phase 2); backups, replication and snapshot scheduling (primitives in the v1 format now: `birth_cp`, stable snapshot ids); licensing and a dependency allowlist (before the first crate); documentation and runbooks (install guide and the three alarms in Phase 1); smaller gaps: `otap-ringd` restart semantics, CRC verification on normal reads, NTP, LUN resize as relocation, NAS LIF behaviour on link failure, and the flash segment size not being a full-stripe multiple of a 4-drive RAID5 at a 64 KiB chunk.

**Unresolved after round 3** (the table in 5.1) and **ten decisions for the lead**, ordered by how much each constrains the rest: Phase 1 HA tier and the lab purchase (shelf, gated by the week-1 PR spike, Redfish fence as fallback, plex in Phase 2); plex variant (by-reference with the geometry constraint relaxed to a common segment size that is a full-stripe multiple of both arrays, aggregate size = the smaller array, `Credit` in the contract now); ring files and format ownership (one tmpfs inbound file per node plus a hugetlbfs arena; Fabrics owns the batch format and scanner, Storage owns slot lifetime; freeze v1 before the TCP leg); process topology and the control sideband (`otap-ringd`, `dbladed`, `nbladed`, `otap-ctl`; the sideband is `otap-ctl`-owned, in Phase 1 simply a second mTLS path over the DAC); the back-pressure contract and one timer table owned by Consensus; NAS defaults by SVM class (datastore SVMs NFSv3-only, file SVMs NFSv4.1-only with 10/20 and a controller-written grace end; verify whether 6.12's courteous server removes the 10 s lease objection); degraded durability as one knob `unmirrored_flush = placed | local_nvme | cp`; the platform floor (trixie 6.12.x generic, 64 GB for HA tiers, 8C/16T); FC in Phase 2 with S1 and S2 in Phase 1 week 1; owner-only items before the first crate (licence, secrets posture, long-term default tier, 0.8x capacity marketing).

**Phase 1 risks, ranked**: the shelf fence (week 1: register and preempt-and-abort through both expanders, then SIGSTOP the owner with a dirty 1.5 GiB stripe cache, preempt, resume, prove nothing lands); the restart budget versus the watchdog's check-ins (build the slot-state page and the memory-region import first and measure the 16-LUN re-fetch); three slot maps and format drift; NAS RTO (week 2: measure early grace end with replicated nfsdcld, one absent client and one v4.0 client); the HDD drain cliff (aged-pool benchmark before ring sizing is frozen); silent death on ESXi; a hung disk versus the hold bound; iblock BUSY (one-day spike); the CPU budget on an 8-core box; openraft 0.9.

### 5.4 ONTAP veteran

**Where the plan diverges from ONTAP, and whether that is legitimate.** Layering: ONTAP makes everything a WAFL file (LUNs are files in FlexVols, FlexVols are container files with their own virtual block numbers, snapshots are root-inode copies, qtrees are tagged subtrees); the plan's native LUN with XFS inside is legitimate at stated costs: two block layers, NAS snapshots need fsfreeze, qtree becomes directory plus XFS project quota, and there is no vol move without a second address level. Multiprotocol: ONTAP has security styles, a name-mapping engine, NT ACLs and streams in the inode and one lock manager; two share classes is "security style plus oplocks off per qtree" at a fraction of the cost, but the identity decision is missing: `sec=sys` NFS plus `idmap_rid` SMB does not give one person one uid on both sides. NAS state across takeover: ONTAP's public matrix (NFSv3 non-disruptive, NFSv4 re-establishes under grace with 30/45, delegations off, SMB non-disruptive only on continuously available shares for Hyper-V and SQL Server) is the plan's matrix; the residual divergence is 10/20 versus 30/45, and ONTAP chose 30/45 because short leases expire healthy clients during ordinary stalls. LIFs: NAS LIFs move independently of volumes in ONTAP (an indirect path over the cluster network); SAN LIFs never move; the plan's "SVM follows its aggregate" is correct for two nodes. HA interconnect: ONTAP carries NVLOG, heartbeat and takeover coordination, never a quorum service on the data path, and arbitrates through mailbox disks on the shared shelves plus hardware-assisted takeover from the BMC; the plan's leases are legitimate for the plex tier and stricter than ONTAP on the shelf tier, where a free third channel is ignored. ONTAP Select: requires a battery- or flash-backed RAID controller and emulates NVRAM as a disk-backed log; HA is SyncMirror with both NVLOG and CP data crossing the network at 50% usable; the mediator is an iSCSI mailbox on the Deploy VM, and losing it disables automatic takeover, not data service. RDB: ONTAP's four configuration rings collapse correctly into one state machine, but Raft loss stopping data through leases is the divergence where ONTAP keeps serving. Takeover unit: per-aggregate is aggregate relocation and right; ONTAP never promised clients 10 s, it shipped Host Utilities and 190 s guest disk timeouts.

**Hard lessons**: identity before shares; Windows semantics on POSIX are the long tail (case-insensitive lookups, DOS attributes, streams, ACL inheritance, Mac forks, Time Machine); snapshots eat the pool (ship autodelete, reserve and autogrow, or ship snapshots off); `.snapshot` directories leak into rsync and S3 listings; delegations off; giveback is the dangerous half (vetoes, terminated SMB sessions, partial givebacks, the returning node must never assemble shelf arrays at boot); randomly written, sequentially read (GC should re-sequence survivors by volume and LBA; a read cache does not fix backups); hung disks and dirty SAS paths (SCT ERC, short timeouts, PHY error counters); lost NVLOG is a data event (nvfail plus a per-volume dirty-since-last-CP bit so the operator sees which volumes had writes in the window); resync needs a common base (pin a common CP on both plexes until resync completes); the NVLOG device is the product (a used Optane P1600X is Select's disk-backed log at homelab prices); clients detect slower than you fail over (ship host timeouts as Host Utilities did).

**Copy this**: nvfail with the affected-volume list; mailbox blocks on the shared shelf (1 MiB reserved on two members, a 4 KiB heartbeat and epoch block written O_DIRECT every 500 ms, a third control channel needing no Ethernet and no witness); giveback vetoes and the 600 s auto-giveback delay with a preflight; Host Utilities (shipped multipath.conf, ESXi claim rules and recipe, Windows MPIO and TimeOutValue, guest disk timeouts, an `otap-host-check` script); qtrees as XFS project quotas; snapshot reserve, autodelete and autogrow as one per-volume policy on by default; SnapMirror-async from `birth_cp` (extents with `birth > base` are the diff; stream them over TLS to a second OpenTAP box or as objects to S3; for a homelab this beats HA for data survival); Select's disk-backed NVLOG as LocalNvmeRing on a PLP device by default when present; the published NDO matrix per protocol and client; and keep hardware-assisted takeover as fence evidence, SVM as the identity unit, aggregate relocation, the nightly rehearsal measured by a synthetic client.

**Do not copy**: a WAFL-class inode layer, lock manager and name-mapping engine; N-blade to D-blade RPC over a cluster network before a third node; multiple RDB rings; continuously available shares and persistent handles (experimental at most); MetroCluster and synchronous replication across sites; FlexVol virtual addressing and vol move; dedupe and compression before block ships; pNFS, referrals, FlexGroups; fractional reserve and option sprawl; the 10 s CP timer as a design centre.

**Phase 1 advice.** Order: engine plus checker with no protocol; iSCSI with ALUA, pre-staged paths and host recipes; NFSv3 datastores; S3 through versitygw as a black box; SMB-only shares labelled disruptive with Previous Versions and Time Machine; NFSv4.1 home directories after early grace end is measured; NVMe/TCP; FC-SCSI client targets only if the owner's lab needs them. Phase 2: async snapshot replication (pulled ahead of plex), plex, FC mirror, multiprotocol shares. Decide now: the identity source; shelf for Phase 1 with the PR spike in week one; Optane LocalNvmeRing as the default durability device; mailbox blocks on the shelf; no md auto-assembly in the initramfs; snapshot autodelete. Contracts before code: the NDO matrix, one timer table with one owner, the client timeout recipe.

### 5.5 Homelab operator

**Hardware reality.** The four specialists quietly re-scoped "generic homelab x86" into a used dual-socket Dell with 64 GB, 8C/16T, a BMC, a ConnectX and a SAS shelf; say it on page one, because the N100, 32 GB, 2.5GbE box most TrueNAS and Unraid users own cannot host 16 to 32 GiB of pinned rings and isolated cores. FC: the 15 dollar QLE2562 is OEM-branded with vendor firmware bundles, 16G cards run flash firmware with no Debian blob, 8G cards get downgraded, symmetric dual mode is unproven, split-role eats both ports and nobody at home owns a switch; FC is a client fabric for old ESXi hosts, never the mirror, which is a 15 dollar DAC. RDMA: the 20 dollar ConnectX-3 everyone owns is the non-Pro card, which is RoCEv1 only; "one code path for RoCEv2 and IB" needs CX-3 Pro or CX-4, or IB mode with opensm on both nodes. Flash: used R730xd and R740xd ship SAS backplanes with zero or four NVMe bays; real flash pools are a two-NVMe mirror, SATA SSDs on the HBA or used SAS SSDs in the shelf; the NVMe is a consumer drive without power-loss protection, so the degraded-mode device does not exist in most labs, except used Optane (P1600X at 40 to 60 dollars, 900P about 100), the ZFS SLOG everyone already has. Shelf: MD1200 plus two 9207-8e is real and the critic would buy it, but used SAS drives arrive 520- or 528-byte formatted (hours of `sg_format` per drive), PR through the expanders is unverified, a replaced drive needs the owner's PR key, and it draws 150 to 200 W audibly. Boot device: nobody mentioned it; Unraid culture boots from USB and this design puts the Raft log, the synchronously written APTPL file, nfsdcld's database and journald on it; refuse USB, require a SATA or BOSS SSD. Power: one consumer UPS and one breaker for everything, so simultaneous site loss is the common case.

**Ops burden.** Install is acceptable only as an ISO that lays down every drop-in and rebuilds the initramfs; two kernel command lines in the record and no timer-table owner is a deal-breaker until fixed. Upgrades: every trixie-security kernel is a rolling takeover and giveback (5 to 15 s of block pause plus NFS grace, twice) and needs one command with a preflight, apt pin and hold. Disk replacement: md rebuilds all 16 TB where ZFS resilvers live data; `checkarray` flags torn never-committed stripes; a new member needs PR registration. Monitoring is acceptable if Prometheus, smartd, `mdadm --monitor` and webhooks (ntfy, Pushover, email) ship in Phase 1; nvfail by default is fine if the alert carries the one-line clear command, with auto-clear offered at first boot for one-UPS labs. UI and CLI: deal-breaker; four specialists on rings and Raft, none on a UI, installer or docs; TrueNAS and Unraid are their UIs. No configuration export. Witness: acceptable (a 5 dollar VPS), with two rules nobody wrote: its disk must never live on OpenTAP, and it will live on the Proxmox host the operator reboots for updates, so witness-down plus a DAC flap is a pair-wide hold. The 3 a.m. flapping DAC: as designed every flap is a 2 s stall, Transitioning, GrantUnmirrored and a revoke after 30 s stable, so ESXi paths flap and VMs pause every 30 s; after N grants in 10 minutes stay unmirrored, no auto-revoke under 10 minutes stable, never stall I/O to re-mirror, rate-limited alerts that say what to do.

**Over-engineering.** Fabric-agnostic mirror: TCP in Phase 1, RDMA in Phase 2, FC mirror never; cut the fabric sideband (the DAC has an IP and mTLS over it suffices). Protocol count: iSCSI, NFS, SMB first, NVMe/TCP next, S3 cheap through versitygw, FC clients later, NVMe/RDMA cut. Per-SVM namespaces with AD-joined machine accounts, Kerberos and per-namespace nftables: one default SVM in Phase 1, the namespace only as the IP-migration mechanism. The HA tier matrix: one tier in Phase 1, the shelf; plex-by-reference for flash in Phase 2; drop plex-by-replay and spill-on-silence; at 50% of twelve 16 TB drives nobody at home accepts plex for HDD. PR survival and WSFC: Phase 3. Performance bars: 300 K mirrored IOPS and 25GbE ring arithmetic are datacentre targets; the homelab bar is 10GbE line rate, 50 K IOPS, p99 under 1 ms. Keep: TLA+ and madsim on the lease core, the RST proxy, the 512e and 4Kn classes, XFS-on-LUN, versitygw. Concrete shape: v0.1 single node (md, D-Blade, ublk, iSCSI, NFS, SMB, snapshots, Prometheus, a UI); v0.2 the pair over TCP on a shelf; v0.3 NVMe/TCP, S3, replication.

**Missing for a homelab.** Migration (a pull-from-NFS, SMB, iSCSI or `zfs send` wizard; nobody has 12 spare drives); pool growth one drive at a time and mismatched drives (`mdadm --grow` reshapes for days and changes the stripe width under the segment layout; the top blocker after HA); compression and encryption at rest; bit-rot repair (detect-only until the Phase 3 parity repair; ZFS heals today); scheduled snapshots and replication (the plex-rejoin diff by `birth_cp` is the incremental send; build it as replication first, plex second); NUT and UPS (on battery: force a CP, stop exporting, clean shutdown); single-node mode as a first-class, quiet mode (no Raft, no watchdog starvation, no GrantUnmirrored) with LocalNvmeRing on used Optane pulled into Phase 1; the hyperconverged case (bless "OpenTAP as a Proxmox VM with HBA and NIC passthrough" with the i6300esb watchdog and the paused-VM lease rule); Time Machine as a share type; a management API for Kubernetes CSI; SMB ACLs on XFS; noise and power (a pair plus shelf plus witness is 500 to 700 W, the strongest argument for single node plus replication); docs (an HCL, the failover contract per protocol, a "what you lose and when" page, 3 a.m. runbooks).

**Would I run it.** Against the field: TrueNAS SCALE has no HA, Proxmox plus ZFS replication is minutes of RPO and a VM restart, Ceph wants three nodes and RAM per OSD, DRBD split-brained on the critic, Unraid has no HA. A witness-arbitrated two-node active/standby pair serving iSCSI, NFS and SMB from a 300 dollar used shelf with a 10 s block failover and zero split brain: nothing in the homelab world does that, and it is the only reason to run OpenTAP. What stops the critic today: single-node OpenTAP is worse than ZFS (volatile cache, no compression, detect-only bit rot, full-disk rebuilds, no growth, no migration), so the pair is needed just to break even on durability; no UI, docs or alerting; stability rides on `tcm_qla2xxx`, ublk and io_uring across kernel updates; and after three rounds the specialists swapped NFS timers, shipped two kernel command lines and left the record format with a provisional owner. The single compelling feature: verified sub-15 s iSCSI and NFS failover for ESXi and Proxmox on 300 dollars of used SAS shelf with a witness that can be a 5 dollar VPS, proven by the linearizability checker and shown on a dashboard as "last failover: 2.8 s, 03:00 today". Prove that in Phase 1 and the critic buys the shelf tomorrow.

---

## Part 6. Fable 5.1 executive synthesis

### 6.1 The pair, as it will be built

Two Debian 13 nodes and a witness. Each data node runs four processes. `otap-ringd` holds the two ring files (a hugetlbfs outbound arena and a tmpfs inbound ring), the RDMA protection domain, memory regions and receiver queue pair when an RDMA leg exists, the TCP receiver (`otap-ringrecv`), the slot-state page and the request-age page. `dbladed` is the engine: per-core reactors that own the ublk queues of their aggregate's LUNs, the dirty index, the CP, garbage collection, the read cache and the mirror sender. `nbladed` is control plane only: the sole writer of configfs, sysfs, modprobe.d, network namespaces and systemd units, reconciling Raft-derived desired state. `otap-ctl` is the Raft voter, lease client, epoch-page writer, barrier sequencer, fence executor and watchdog petter. The witness runs `otap-ctl --witness` and nothing else.

A write enters through a kernel target (LIO iSCSI in Phase 1; FC-SCSI and nvmet in Phase 2), becomes a ublk request on the reactor that owns the LUN, is copied once by the kernel into a slot of the hugetlbfs arena, CRC-checked, assigned an aggregate-wide sequence number, inserted into the volume's dirty index, batched and put to the partner's tmpfs inbound ring over TCP on a direct DAC (RDMA in Phase 2). The client is acked when the local insert is done and the partner reports Placed. Every 10 to 15 s, or when the ring is half full, a CP writes full stripes from the arena to the md array, then the map and segment-table pages, one flush, then a FUA superblock stripe; slots are freed only by a committed CP and, under plex, a partner credit. The control path is leases every 500 ms through the Raft leader, heartbeats every 100 ms over mTLS on the LAN and the DAC's own IP, and on the shelf tier a 500 ms heartbeat block on two shelf members; a per-aggregate epoch page is checked at submission, CP submit, superblock write and completion. Block clients see the same LUN on both nodes through ALUA groups per aggregate; NAS clients see an SVM (a network plus UTS namespace) that follows its home aggregate.

### 6.2 Rulings

Each ruling names who argued what and why I decided as I did. "Owner decision" marks items in Part 9 that the owner must ratify.

**R1. Phase 1 HA tier: the PR-fenced dual-expander SAS shelf, gated by a week-1 spike.** Storage, Consensus (revised), the completeness critic, the ONTAP veteran and the homelab operator all land here: it is the only medium where takeover of flushed data exists before plex ships, it is credit-free and 100% capacity, and it is what the homelab critic would buy. The spike registers keys and runs preempt-and-abort through both expanders, then stops the owner with a dirty 1.5 GiB stripe cache, preempts, resumes, and proves nothing lands. If PR does not pass through the expanders, the fallback is a confirmed Redfish or IPMI power fence on the iDRAC; if neither works, the Phase 1 HA proof moves to plex and the roadmap flips. HA is refused on a shelf without PR-capable members and without a confirmed power fence (SATA-interposer shelves included). Plex-by-reference is Phase 2 and the flash and consumer tier; plex-by-replay and spill-on-silence are dropped. Single-node operation is first-class from v0.1. The long-term default tier is an owner decision (D1); my recommendation is shelf for HDD pools and plex for flash.

**R2. Plex variant: by-reference with a relaxed geometry rule.** One CP program executed twice, a manifest of about 5% of CP bytes, physical resync, no second allocator. The "identical geometry" condition becomes: a common segment size that is a whole number of full stripes on both arrays (the least common multiple of the two full-stripe data sizes; the installer refuses pairs whose multiple exceeds 256 MiB), with the aggregate sized to the smaller array. `Credit{agg, released_up_to}` enters the ring contract in v1; the owner reuses a slot only after its own commit and the partner's credit; partner lag past half a ring drives Transitioning; the backlog is reported at takeover. Fabrics and Consensus were right that any plex variant changes the contract, so the contract carries it from day one even though plex itself is Phase 2.

**R3. Ring files and format ownership.** One tmpfs `huge=always,noswap` inbound file per node, mlocked by `otap-ringd`, slots tagged by aggregate with per-aggregate quotas in the sender's allocator; one hugetlbfs outbound arena per node (2 MiB pages reserved at boot, registered with io_uring in entries of at most 1 GiB, cloned into the other rings before any other buffer registration). The FC mirror LUN, if it ever ships, is the inbound file. Fabrics owns `otap-record` (the batch format, CRCs, scanner); Storage owns slot lifetime (the slot-state page, the Placed-gated cut, `Credit`). The v1 format is frozen with property tests before the TCP leg exists, and every persistent and on-wire structure carries a version with an N/N+1 compatibility promise.

**R4. Process topology.** `/dev/ublkcN` lives in `dbladed`; `nbladed` has no I/O path; `otap-ctl` never holds a data-path descriptor; `otap-ringd` is the process that must not die. The ring files are created by systemd mount units and a one-shot setup unit, not by `otap-ringd`, so its death loses no data; its restart is supervised exactly like `dbladed`'s (systemd reports exit and new PID to `otap-ctl`, which suspends worker check-ins for at most 5 s and raises the stall threshold to 5 s for that node), after which `dbladed` re-imports the memory regions. Restart budget: 2 s target, 5 s hard, measured in week 3 before anything is built on top.

**R5. ROLLBACK is nvfail.** After a dual power loss the aggregate boots to its last committed CP, its LUNs go Inaccessible, its NAS exports stay down, and a CRIT alert carries the CP sequence, the wall clock, the window, the estimated bytes and the list of volumes whose dirty-since-last-CP bit was set, plus the one-line clear command. Auto-clear is an opt-in chosen at install for single-UPS labs. Protocols' argument decided it: surviving clients would otherwise resume against a disk behind their own journals.

**R6. Control paths.** `ControlLink` has two Phase 1 implementations: `TcpMtls` over every IP path (the LAN, the DAC's own address, IPoIB when InfiniBand is present) and `ShelfMailbox` (1 MiB reserved on two shelf members; each node writes a 4 KiB heartbeat and epoch block with O_DIRECT every 500 ms and reads the partner's; it carries liveness, epoch and the stopped-exporting flag, never Raft entries). There is no ring mailbox region (Consensus withdrew it; Fabrics' critique that it ties control liveness to `dbladed` was right) and no RDMA sideband (RoCE and IPoIB carry IP, so `TcpMtls` over the DAC already covers them; the homelab critic was right to cut it). An FC LUN sideband exists only if an FC mirror leg ever ships. Raft RPC rides `TcpMtls` only. Install guide: the mirror runs on a direct DAC, never through the LAN switch; the witness sits on separate power and a separate switch where possible. A dead LAN switch then costs only the witness, and A and B keep two of three.

**R7. Back-pressure contract and one timer table.** The table in 6.3 has one owner, `otap-ctl` (the Consensus persona), with Fabrics' link timers, Protocols' initiator and NFS timers and Storage's restart budget as inputs; no other document may carry a different number. The contract: the throttle delays a request at most 2 s; a stall past 2 s flips the owner's own ports to Transitioning or ANA change; a node-local stall (ublk queues stuck while the backstore progresses, an HBA or daemon wedge) proposes self-takeover at 5 s; the hard bound is 10 s, not 20, because legitimate throttle waits never exceed 2 s and 20 s breaks the 15 s hard promise to dm-multipath; a member stall is a drive failure (SCT ERC 7 s, SCSI timeout 10 s, `eh_deadline` 5 s, md marks the member faulty) and never a takeover. BUSY stays out of the design: a one-day spike may show that ublk's `-ENOMEM` reaches SAM BUSY through iblock on 6.12, but my reading of `iblock_complete_cmd` is that only reservation conflicts are special-cased, and even a confirmed BUSY would be a SCSI-only optimization, not a design input.

**R8. NAS defaults by SVM class.** A datastore SVM serves NFSv3 only (ESXi; Proxmox with lockd and statd inside the SVM and `nlm_grace_period` set to 20 s), so its failover pause equals the block ladder. A file SVM serves NFSv4.1 only with lease 10 s and grace 20 s, delegations off, the nfsdcld database and pinned fsids on the SVM root volume, and `otap-ctl` writing `v4_end_grace` once its own tally of live clients has reclaimed. The 10 s lease relies on knfsd's courteous-server behaviour (an expired client keeps its state unless a conflicting request arrives), which the week-2 measurement must confirm on 6.12; the fallback is 30/45. SMB shares default to `Expire2s` (`vm.dirty_expire_centisecs=200`, `vm.dirty_writeback_centisecs=100`, `strict sync = yes`) with `SyncAlways` per share, and SMB is labelled disruptive. Phase 1 ships one default SVM; the namespace is the migration mechanism, not a multi-tenancy feature.

**R9. Takeover orchestration.** Desired state is declarative in Raft (owner, epoch, per-LUN ALUA and ANA targets, SVM placement) and `nbladed` reconciles toward it; `otap-ctl` sequences four committed barriers: `Fenced(evidence)`, `DataReady`, `BlockServing`, `NasServing`. The epoch page is the completion gate; there is one format. Fence order on a node that loses its lease: gate closed, Standby or Inaccessible, LIFs down (under 50 ms), then lease lapse, then the watchdog; `DEL_DEV` is teardown, never a fence; a wedged node is fenced by silence. Both nodes carry byte-identical target trees differing only in addresses and id ranges, devices pre-created with the standby's gate closed, so takeover is a group flip. The RST proxy runs only on positive fence evidence (BMC off, PDU state, watchdog deadline passed, or three unanswered ARP probes), and block portal addresses are raised only by `nbladed` under Raft state, never by the distribution's network configuration. Per-LUN PR policy: `standby_paths` (default; reservations lost at takeover, documented) or `pr_survival` (Phase 3; the LUN is mapped on the partner only at takeover, with the one-operation regression window documented). Giveback has a preflight with vetoes (CP in flight, GC urgent, S3 job running, open SMB handles above a threshold), an explicit override, a 600 s auto-giveback delay and no automatic giveback in business hours by default.

**R10. Degraded durability: one knob.** `unmirrored_flush = placed | local_nvme | cp`. `local_nvme` is selected automatically when a local namespace reports no volatile write cache (NVMe VWC bit clear, which is what Optane and most power-loss-protected enterprise drives report; verify per device) or is on the operator's allowlist; otherwise `placed` with the mandatory alarm and the forced-takeover loss printout; `cp` is opt-in only, because Protocols showed it collapses NAS and S3 to a few megabytes per second. Hysteresis from the homelab critic: after three grants in ten minutes the aggregate stays unmirrored until an operator acts; no automatic revoke before ten minutes of stable mirror health; re-mirroring never stalls client I/O (resync rides ordinary ring records ordered by sequence); alerts are rate-limited and carry the action. The `placed` default is an owner decision (D2): it is the "one more fault loses the window" stance.

**R11. LocalNvmeRing.** A fourth `MirrorLink` implementation that writes slot-addressed records with FUA to a raw namespace at slot offsets, Placed being the FUA completion. It is the degraded-mode device, the single-node durability device and ONTAP Select's disk-backed log at homelab prices; the ONTAP veteran and the homelab critic both asked for it by default when present. It is Phase 1b (weeks 13 to 16), PLP-only by the rule in R10, with used Optane (P1600X, 900P) as the recommended device. Spill-on-silence is dropped.

**R12. Platform floor.** Debian 13 trixie, kernel 6.12.x, the generic amd64 image (the cloud flavour lacks the QLogic driver); bookworm with the bookworm-backports 6.12 kernel is tolerated as the same source; stock bookworm is refused by the installer, which probes for `ublk_drv`; trixie-backports 7.x is a CI lane with no product claims. 64 GB DRAM for HA tiers (8 GiB plus 8 GiB rings), 32 GB for single-node only; 8C/16T minimum; boot on a SATA or BOSS SSD, USB refused; boards with only 2.5GbE excluded. The reference node is a used dual-socket Dell with iDRAC; OpenTAP as a Proxmox VM with HBA and NIC passthrough (i6300esb watchdog, paused-VM lease rule) is a Phase 2 qualification target.

**R13. Fibre Channel.** FC-SCSI client targets through `tcm_qla2xxx` are Phase 2, pulled into Phase 1b if the owner's hosts are FC-attached, since `blade_lio` is the same configfs applier with a second fabric directory. The FC mirror leg is Phase 3 at the earliest, conditional on S1 passing and a demonstrated need; the trait carries the capability bits so no redesign is required. S2 (`tcm_loop` plus fileio on tmpfs) runs in week 1 because it validates the record format against a kernel writer at no hardware cost; S1 (N2N dual mode) runs in weeks 1 to 2 off the critical path if cards are on hand. Policy: module `qlini_mode=dual`, mirror port `dual`, client ports `disabled`, `ql2xfwloadbin=1` for 25xx cards after S1, a firmware floor per family recorded by the installer, the receive path fileio on tmpfs rather than TCMU.

**R14. RDMA.** Phase 2. The legacy `ibv_create_qp` plus `ibv_post_send` path is mandatory because ConnectX-3 is the used-market card; a plain ConnectX-3 is RoCEv1 on a direct DAC (layer 2 only) or InfiniBand with opensm on both nodes, and RoCEv2 needs ConnectX-3 Pro or ConnectX-4. The crate (sideway or ibverbs) is chosen in Phase 2 on ergonomics; `ibv_wr_*` is probed at runtime as a fast lane. rxe is the CI lane. The NVMe/RDMA client port is cut until the RDMA leg exists.

**R15. Replay and the cut.** Takeover replays every CRC-valid Placed record above the last committed CP in sequence order, deduplicated by sequence; the CP cut advances only over Placed records (or under GrantUnmirrored); the owner's own hole-free ring keeps the prefix cut for normal CPs; old-epoch inbound paths are torn down (socket closed, queue pair to error, LUN unmapped plus abort) before slots are reused at giveback; the scanner rejects records whose owner epoch is below the current epoch of their aggregate.

**R16. Threads and cores.** Thread-per-core ingest and CP reactors, one per data core, each owning the ublk queues of its LUNs (queue threads = data cores, not LUNs), feeding one mirror core over single-producer queues; the mirror core busy-polls under io_uring NAPI registration while loaded and sleeps when idle; `otap-ctl` on one pinned core; core 0 and its SMT sibling to the OS and kernel targets; knfsd, smbd and versitygw in per-SVM slices with CPU weights on the non-isolated cores. The installer ships the `isolcpus`, `nohz_full` and IRQ-affinity profile.

**R17. Memory.** The 64 GB budget in 6.4; rings live outside any slice with a memory limit, tmpfs files carry `size=` caps, huge pages are reserved at boot, and per-SVM limits are derived from DRAM minus pinned memory, map cache and stripe caches.

**R18. LUN classes and thin provisioning.** `Esxi512e` (512-byte logical, 4 KiB physical, sector-mask patch records) and `Native4k` (internal XFS LUNs with 4 KiB sectors, Linux and NVMe-oF hosts); never 4Kn to ESXi before 9.0 and never an HDD-backed 4Kn LUN on 9.0. Block LUNs are thin with alerts at 70, 80 and 90% and hold-then-Transitioning at write-stop; file volumes are thick-reserved including the GC reserve and snapshot deadlists; thin-provisioning attributes (VPD B2h, threshold unit attention, nvmet thin reporting) are Phase 1 attributes.

**R19. Format reservations that cost nothing now.** A physical-length field in every map extent (CP-time compression in Phase 3); the stripe geometry recorded per segment at birth (so md reshape and one-drive-at-a-time growth can coexist with old segments in Phase 2); `birth_cp` and stable snapshot identifiers (async replication in Phase 2, plex resync); a per-volume dirty-since-last-CP bit in the superblock (nvfail's volume list); four superblock slots (A0, A1, B0, B1) with max-select over `(epoch, cp_seq)` as defence in depth on shelves; CRC verification on every read from disk, not only in scrub.

**R20. Identity.** No multiprotocol share until the SVM has an identity source. Phase 1 default: standalone with local users, SMB-only shares, NFS `sec=sys` with export ACLs. AD join (winbind or SSSD with the RID algorithm) is Phase 2; multiprotocol shares are Phase 3.

**R21. Snapshots and replication.** When snapshots ship (Phase 2), snapshot reserve, autodelete and autogrow are one per-volume policy on by default; `.snapshot` mounts live outside share and bucket roots; delegations stay off; NAS snapshots use fsfreeze. Async replication from `birth_cp` (extents with birth above the common base, streamed over TLS to a second OpenTAP or as objects to S3) is pulled ahead of plex in Phase 2, because for a homelab it beats HA for data survival and the map already provides it.

**R22. Operations.** The product installs from an ISO that lays down every modprobe.d, udev, sysctl, hugepage, isolation and command-line drop-in, rebuilds the initramfs, sets `AUTO -all` in mdadm.conf with no incremental assembly in the initramfs, disables the monthly `checkarray` (our scrub replaces it), enforces SCT ERC and SCSI timeouts at pool creation, and registers PR keys before `mdadm --add`. Rolling upgrade is one command with a preflight (modules, ublk flags, a target smoke test), apt pin and hold. Configuration export exists from Phase 1. Prometheus metrics, smartd, `mdadm --monitor` and webhook alerts (ntfy, Pushover, email) ship in Phase 1; the per-I/O trace is Phase 2. The NUT hook (on battery: final CP, stop exporting, clean shutdown; on return: boot, rejoin, giveback) is Phase 1b. The witness's disk never lives on OpenTAP; the three binaries are version-locked per release. The management API (axum over mTLS) and the `otapctl` CLI are Phase 1; the web UI is a budgeted Phase 2 item, not an afterthought.

**R23. Privilege and secrets.** Per-daemon systemd hardening (capabilities, `ProtectSystem`, memory-lock and hugetlb limits). Secrets (CHAP, DH-HMAC-CHAP keys, keytabs, TLS keys) are stored in Raft only as ciphertext sealed with a cluster key that exists on the data nodes alone (`/etc/opentap/cluster.key`, TPM sealing later), so the witness never holds plaintext (owner decision D4 on the posture). Rings in tmpfs are plaintext by design; encryption at rest is for disks (dm-crypt above md, Phase 2). mTLS on every control path; per-namespace nftables default-deny.

**R24. Licensing and dependencies.** Owner decision D3; my recommendation is Apache-2.0 for OpenTAP's own code, GPL components (the kernel, Samba, nfs-utils, fence-agents) used or executed and never linked, versitygw (Apache-2.0) shipped as a separate binary, AGPL excluded, Marvell firmware never redistributed, and a `cargo-deny` allowlist in place before the first crate lands.

### 6.3 The single timer table (owner: `otap-ctl`)

| Item | Value | Note |
|---|---|---|
| Ring heartbeat | 20 ms per link | link health only; carries committed and applied cp_seq and released_up_to |
| Control heartbeat | 100 ms per path | an answered RPC is liveness; hardware acks never are |
| Link dead | 1 s | triggers redelivery on the secondary; in-flight tokens resolve Unknown and quarantine for the link's unknown-fate bound |
| Partner silent | 2 s | on all control paths; BMC off is positive evidence |
| Lease TTL / renew / margin | 3 s / 500 ms / 100 ms | L1: a new leader waits TTL plus margin (3.1 s) before any takeover |
| Election timeout | 0.5 to 1 s | at least 10 times the witness RTT; witness RTT at most 100 ms, p99 jitter 200 ms |
| Throttle delay per request | at most 2 s | never fail a write; the CP interval shrinks first |
| Stalled | 2 s | owner's own ports to Transitioning or ANA change |
| Node-local stall, self-takeover proposal | 5 s | ublk queues stuck while the backstore progresses, HBA or daemon wedge |
| Hard hold bound | 10 s | then stall-to-release |
| Member stall | SCT ERC 7 s, SCSI timeout 10 s, `eh_deadline` 5 s | md marks the member faulty; never a takeover |
| Daemon restart (supervised) | 2 s target, 5 s hard | check-ins suspended, stall threshold raised to 5 s for that node |
| Watchdog | 5 s timeout, 500 ms pet | pet only if every data-path worker checked in within 1 s and no aggregate is export-active past lease expiry |
| Kernel command line | `panic=-1 hardlockup_panic=1 softlockup_panic=1 panic_on_oops=1` | softdog `soft_margin=5`, no `soft_panic`, no `soft_reboot_cmd`, `nowayout=0`; `otap-ctl` refuses the magic close while it owns an aggregate |
| Silent-node block resume | 5 s p50 plex, 6 to 8 s shelf, p99 9 s | published bound 1 |
| Wedged-node block resume | 11 to 13 s | published bound 2; hard bound 15 s |
| Planned takeover and giveback | at most 3 s client-visible | 5 to 10 s with XFS unmount and md assembly on a shelf; NAS adds grace |
| FC mirror disk (if ever) | command timeout 2 s, `fast_io_fail_tmo=1`, `dev_loss_tmo=5` | quarantine up to R_A_TOV 10 s |
| GrantUnmirrored | within 2 s of mirror loss | CP interval 2 s while unmirrored; revoke after 10 min stable; three grants in 10 min stay unmirrored |
| Auto-giveback | 600 s after the partner is synced | preflight vetoes; not in business hours by default |
| NFS (file SVM) | lease 10 s, grace 20 s | NFSv4.1 only; courteous server verified in week 2; fallback 30/45 |
| NLM (datastore SVM) | `nlm_grace_period` 20 s | NFSv3 with lockd and statd inside the SVM |
| SMB write-back | `dirty_expire` 2 s, `dirty_writeback` 1 s | `Expire2s` class; `SyncAlways` per share |
| Fail-back after a link flip | 30 s stable plus a probe put | manual by default on FC |

### 6.4 Memory and core budget (64 GB, 8C/16T reference node)

| Consumer | Size |
|---|---|
| Outbound arena (hugetlbfs, 2 MiB pages, reserved at boot) | 8 GiB |
| Inbound ring (tmpfs `huge=always,noswap`, mlocked) | 8 GiB |
| Dirty indexes (active and frozen) | 0.3 GiB |
| Map page cache | 6 GiB |
| Segment tables, read-cache index (1 TB of cache) | 0.6 GiB |
| md stripe cache (12-drive array at 32768) | 1.5 GiB per array |
| `otap-ctl`, `nbladed`, `otap-ringd` | 0.5 GiB |
| Per-SVM slices (knfsd page cache, smbd, versitygw), capped | DRAM minus the above minus 8 GiB headroom |

Cores: core 0 and its sibling to the OS and kernel targets; one core to `otap-ctl`; one to the mirror; the remaining data cores to reactors; NAS and S3 processes weighted on the non-isolated cores. A 128 GB node doubles the rings and the map cache; nothing else scales.

### 6.5 Platform profile (shipped as package files, applied by the installer)

Kernel command line: the entry in 6.3 plus `isolcpus`, `nohz_full` and `rcu_nocbs` for the mirror and reactor cores, `hugepagesz=2M hugepages=<arena>`. modprobe.d: `options softdog soft_margin=5`; `options raid456 devices_handle_discard_safely=Y` on flash pools only; `options qla2xxx qlini_mode=dual ql2xmqsupport=1` only when FC hardware is present, followed by `update-initramfs`. sysctl: `vm.swappiness=0` with swap off, `vm.dirty_expire_centisecs=200`, `vm.dirty_writeback_centisecs=100`, `kernel.hardlockup_panic=1`, `fs.nfs.nlm_grace_period=20`, busy-poll and `tcp_autocorking=0` on the mirror NIC. udev: shelf members get a 10 s SCSI timeout and `eh_deadline=5`; `ublkb*` is masked from blkid, md and LVM; the FC mirror disk, if any, gets the 2 s timeout and the remote-port timers. Mount units: the hugetlbfs arena and the tmpfs inbound ring with `size=` caps, plus a one-shot that allocates, mlocks and formats the ring superblocks. mdadm: `AUTO -all`, no incremental assembly in the initramfs, `checkarray` disabled. Watchdog: `ipmi_watchdog`, `hpwdt`, `iTCO_wdt` or `sp5100_tco` preferred, softdog as the fallback, an install-time self-fire test for each driver, the NO_REBOOT bit detected. Pool creation: SCT ERC 7 s, RZAT verified before discard is enabled, DM-SMR models refused, consumer SSDs without power-loss protection accepted only with a warning.

### 6.6 The non-disruptive-operation matrix (what the owner may promise)

| Protocol, client | Planned takeover or giveback | Unplanned, silent node | Unplanned, wedged node | What is lost |
|---|---|---|---|---|
| iSCSI, ESXi with the shipped recipe | pause at most 3 s | at most 10 s with the gated RST proxy, about 35 s without it | 11 to 13 s | nothing; reservations under `standby_paths` |
| iSCSI, Linux dm-multipath with the shipped `multipath.conf` | pause at most 3 s | p99 9 s | 11 to 13 s | nothing |
| iSCSI, Windows MPIO | pause at most 3 s | under 15 s with the RST proxy (Windows sends no NOP-Out) | 11 to 13 s | nothing; reservations under `standby_paths` |
| FC-SCSI (Phase 2) | pause at most 3 s | RSCN within 1 s plus the ladder | logins free only at the node's reset | nothing |
| NVMe/TCP (Phase 2) | pause at most 3 s | ANA, immediate after reconnect | 11 to 13 s | nothing; no reservations on 6.12 |
| NFSv3 datastore (ESXi, Proxmox) | pause at most 3 s | block ladder plus about 1 s | 11 to 13 s | nothing; NLM locks reclaimed under a 20 s grace (Proxmox) |
| NFSv4.1 file SVM (Phase 1b) | reconnect plus reclaimed I/O at most 8 s | same | same | new opens wait up to 20 s of grace unless early end fires |
| SMB (Phase 1b) | sessions reconnect in 10 to 20 s | same | same | files open at the instant of failover get errors; up to 2 s of acknowledged writes under `Expire2s` |
| S3 through versitygw (Phase 1b) | connections reset, clients retry | same | same | nothing; multipart parts survive on the volume |
| Any protocol, dual power loss | n/a | n/a | n/a | up to one CP window of acknowledged writes; nvfail holds the aggregate offline until acknowledged |

### 6.7 What we are not building

Not in Phase 1: SMB, S3, NVMe-oF, FC, RDMA, plex, snapshots, replication, the web UI, AD, Kerberos, multi-SVM tenancy, PR survival, pool growth, compression, encryption (all phased in Part 8). Not ever, on the current evidence: a WAFL-class inode layer with a unified lock manager and name-mapping engine; a per-I/O N-to-D RPC wire format before a third node exists; multiple configuration rings; MetroCluster-style synchronous replication across sites; FlexVol virtual addressing and volume move (replication plus cutover instead); pNFS, referrals and multi-node volumes; continuously available SMB shares except as an experimental SMB-only class on Samba 4.25 with CTDB; the 10 s CP timer as a design centre; a Rust iSCSI or NVMe-oF target; datacentre performance bars as promises (300 K mirrored IOPS and 25GbE ring arithmetic are measured, not promised; the homelab bar is 10GbE line rate, 50 K IOPS, p99 under 1 ms).

---

## Part 7. Standardized Rust trait contracts

Ownership: `otap-record` and `MirrorLink` (Fabrics); slot lifetime, `Backstore`, `BlockMap` (Storage); `BlockService`, `ProtocolBlade` (Protocols); `ControlLink`, `QuorumService`, `ClusterState`, `EpochPage`, `FenceDevice` and the timer table (Consensus). Every struct that is persisted or sent on the wire carries `ver`, and a release reads its own and the previous version. These are contracts, not implementations; each is at most a few dozen lines by design.

### 7.1 `otap-record` v1: the batch format

```rust
// Little-endian, repr(C), CRC32C (Castagnoli). A slot is 4 KiB. A batch is [BatchPage | payloads],
// contiguous in the partner's inbound file, written exactly once by exactly one link, one aggregate per batch.
pub const SLOT_BYTES: u32 = 4096;
#[repr(C)] pub struct BatchPage {                 // 4 KiB: 64 B header + 63 record headers
    pub magic: u32, pub ver: u16, pub n: u16,     // n <= 63 records in this batch
    pub link_id: u32, pub agg: u32,
    pub batch_seq: u64, pub owner_epoch: u64,     // the aggregate's owner epoch when written
    pub first_slot: u32, pub nslots: u32,         // this batch's extent in the inbound file
    pub page_crc: u32, pub _r: [u8; 20],
    pub rec: [RecordHeader; 63],
}
#[repr(C)] pub struct RecordHeader {              // 64 B; accepted only when hdr_crc and payload_crc both verify
    pub op: u8, pub flags: u8, pub lun_class: u8, pub sector_mask: u8, pub hdr_crc: u32,
    pub nvlog_seq: u64, pub cp_seq: u64,          // aggregate-wide sequence; CP that covers it (0 while open)
    pub vol: u32, pub len: u32, pub lba: u64,
    pub req_id: u64,                              // ReqId packed: dev:24 | queue:8 | tag:16 | generation:16
    pub payload_crc: u32, pub slot_lap: u32,      // payload starts at the next slot; lap is the ABA guard
    pub _r: [u8; 8],
}
pub enum Op { Write, Patch, Discard, WriteZeroes, Snap, Heartbeat, Resync, Manifest }
// Inbound file layout: [RingSuper page | slot-state pages | acked bitmap | slots]. The scanner walks slots in
// order, accepts records individually, and rejects owner_epoch below the aggregate's current epoch.
```

### 7.2 `MirrorLink`, `MirrorInbound`, `Caps`

```rust
pub type Slot = u32;
pub struct RingRef { pub slot: Slot, pub lap: u32, pub nslots: u32 }   // in the local arena; the only put source
pub struct Batch { pub local: RingRef, pub remote_first: Slot, pub agg: AggId }
#[derive(Clone, Copy)] pub enum PlacedBy { Hardware, PeerProcess }     // SCSI GOOD, RC ACK, FUA completion / otap-ringrecv
#[derive(Clone, Copy)] pub struct Caps {
    pub max_batch_bytes: u32, pub max_inflight: u32, pub placed_by: PlacedBy,
    pub reject_advisory: bool,            // only PeerProcess legs can NAK before placement; always advisory
    pub unknown_fate_bound: Duration,     // TCP/RDMA/local ~0 after close or QP->ERR; FC = R_A_TOV 10 s unless ABTS confirmed
    pub cores_owned: u8,                  // TCP 1 while loaded; RDMA, FC, local 0
}
pub enum Ack { Placed, Rejected, Unknown }  // Unknown => slot quarantined for unknown_fate_bound, record re-put elsewhere
pub struct PutToken(pub u64);
pub enum LinkHealth { Up { rtt_ewma: Duration }, Degraded, Dead { since: Instant } }
pub trait MirrorLink: Send {                // impls: TcpLink (P1), LocalNvmeLink (P1b), RdmaLink (P2), FcLink (P3, conditional)
    fn caps(&self) -> Caps;
    fn put(&mut self, b: Batch) -> Result<PutToken, PutError>;                 // non-blocking; Err(Full) at max_inflight; never retries or reorders
    fn heartbeat(&mut self, hb: &HeartbeatRec) -> Result<PutToken, PutError>;  // every 20 ms; carries committed cp_seq, applied cp_seq, released_up_to
    fn poll(&mut self, out: &mut Vec<(PutToken, Ack)>) -> usize;               // each token resolves exactly once; order unspecified
    fn health(&self) -> LinkHealth;
    fn fence_and_close(&mut self) -> Result<(), LinkError>;                    // after Ok no late write from this link can land
}
pub trait MirrorInbound {                   // one per node: the tmpfs inbound file, owned by otap-ringd
    fn mapping(&self) -> &RingMapping;
    fn scan(&mut self, cur: &mut ScanCursor, epochs: &EpochPage, out: &mut Vec<RecordLoc>) -> usize;
    fn quiesce(&mut self, link: LinkId) -> Result<(), LinkError>;              // close / QP->ERR / unmap+ABTS, drain, one residual scan
}
// Never leaked through this boundary: WWPN, LUN, NQN, rkeys, QPNs, descriptors, SCSI tags, exchange ids,
// sense data, delivery order, MTU, the transport's own retry policy or timeouts.
```

### 7.3 Slot lifetime, cut and credit (Storage)

```rust
#[repr(u8)] pub enum SlotState { Free, Filled, Placed, Indexed, Committed, Quarantined }   // persisted page in each ring file
pub struct Cut { pub agg: AggId, pub seq: u64 }              // max seq with every record <= seq Placed, or GrantUnmirrored in force
pub struct Credit { pub agg: AggId, pub released_up_to: u64 }  // partner -> owner over ControlLink; implicit on a shelf, explicit under plex
pub trait SlotLifetime { fn reusable(&self, r: RingRef) -> bool; }   // own_committed(seq) && partner_released(seq) && !quarantined
pub enum CpTrigger { Timer, RingHalfFull, BackToBack, Takeover, Shutdown, UnmirroredFlush, Snapshot(SnapId), UpsOnBattery }
```

### 7.4 Control plane (Consensus)

```rust
pub enum CtlPath { TcpMtls { nic: String }, ShelfMailbox { member: PathBuf } }   // FcLun only if an FC mirror ever ships
pub trait ControlLink: Send + Sync {
    fn path(&self) -> CtlPath;
    fn send(&self, to: NodeId, m: CtlMsg) -> Result<(), CtlError>;     // postcard-framed, versioned enum; best effort
    fn recv(&self, out: &mut Vec<(NodeId, CtlMsg)>) -> usize;
    fn health(&self) -> PathHealth;                                     // an answered RPC is liveness; hardware acks never are
}
pub trait QuorumService {                       // otap-ctl over openraft 0.9.25 on redb; Raft RPC rides TcpMtls only
    async fn renew_lease(&self, agg: AggId, holder: NodeId, epoch: u64) -> Result<LeaseGrant, LeaseRefused>; // leader: ensure_linearizable(); no log write
    async fn propose(&self, op: ClusterOp) -> Result<LogId, ProposeErr>;
    fn applied(&self) -> Arc<ClusterState>;
}
pub struct ClusterState { pub ver: u64, pub ca_roots: Vec<Cert>, pub nodes: BTreeMap<NodeId, NodeInfo>,
    pub aggregates: BTreeMap<AggId, AggState>, pub svms: BTreeMap<SvmId, SvmState>,
    pub luns: BTreeMap<LunId, LunIdentity>, pub secrets: BTreeMap<SecretId, Sealed> }      // ciphertext only
pub struct AggState { pub owner: NodeId, pub home: NodeId, pub epoch: u64, pub mode: AggMode, pub media: Media,
    pub durability: UnmirroredFlush /* Placed | LocalNvme | Cp */ }
pub enum AggMode { Mirrored, Unmirrored { granted_epoch: u64, grants_in_10m: u8 }, Takeover { reached: Barrier },
    Giveback { reached: Barrier }, Down, NvFailed { cp_seq: u64, window: Duration, volumes: Vec<VolId> } }
pub enum Media { Shelf { pr_keys: [u64; 2] }, Plex { copies: [NodeId; 2], synced: bool }, Single }
pub enum FenceEvidence { Lease, PrPreempted, PowerOffConfirmed, WatchdogDeadlinePassed(MonoNs), ArpSilent }
pub enum Barrier { Fenced(FenceEvidence), DataReady, BlockServing, NasServing }
pub enum ClusterOp { Takeover { agg: AggId, new_owner: NodeId, new_epoch: u64 }, Barrier { agg: AggId, epoch: u64, b: Barrier },
    Giveback { agg: AggId }, GrantUnmirrored { agg: AggId, epoch: u64 }, RevokeUnmirrored { agg: AggId },
    PlaceSvm { svm: SvmId, node: NodeId }, SetPr { lun: LunId, aptpl: String },
    SetDesired { lun: LunId, alua: AluaState, ana: AnaState }, NvFail { agg: AggId, cp_seq: u64, volumes: Vec<VolId> },
    NvFailClear { agg: AggId }, Config(ConfigDelta) }
pub trait FenceDevice { fn kind(&self) -> FenceKind;                 // Watchdog | Bmc | Pdu | ScsiPr | Outlet
    async fn fence(&self, victim: NodeId) -> Result<FenceEvidence, Unconfirmed>; }   // a power fence counts only when confirmed
// Never in Raft: lease renewals, heartbeats, ring traffic, CP decisions, per-I/O checks, SMB handles, NFS state.
```

### 7.5 The epoch page (one format; written by `otap-ctl`, read by everyone)

```rust
#[repr(C, align(4096))] pub struct EpochPage { pub seq: AtomicU64, pub slots: [AggSlot; 60] }   // seqlock; MAP_SHARED
#[repr(C, align(64))] pub struct AggSlot { pub agg: u64, pub epoch: u64, pub state: u32 /* Owned | Standby | Transitioning | Fenced */,
    pub serving: u32, pub lease_expiry_mono_ns: u64, pub gate_seq: u64, pub degraded: u32 /* Mirrored | Unmirrored */, pub _pad: [u8; 20] }
impl EpochPage { pub fn check(&self, agg: AggId, epoch: u64, now_ns: u64) -> Result<(), GateErr>; }
// Checked at submission, CP submit, superblock write and completion. Readers re-read CLOCK_MONOTONIC after every blocking
// call (paused-VM rule). A failed check at completion is a hold or Transitioning, never an ack.
```

### 7.6 `BlockService` (Protocols): the N-to-D "RPC" is a call on the owning reactor

```rust
pub struct ReqId { pub dev: u32, pub queue: u16, pub tag: u16, pub generation: u32 }   // stable across restarts: the REISSUE dedupe key
pub struct IoCtx { pub lun: LunId, pub agg: AggId, pub owner_epoch: u64, pub req: ReqId }
pub enum BlockOp<'a> { Write { lba: u64, slot: RingRef, len: u32, fua: bool },   // payload already in the arena (ublk user copy)
    Read { lba: u64, buf: &'a mut [u8] }, Flush /* completes when every earlier write of this LUN is Placed */,
    Discard { lba: u64, len: u32 }, WriteZeroes { lba: u64, len: u32 } }
pub enum BlockErr { StaleEpoch, NotOwner, Fenced, Stalled /* hold; Transitioning after 2 s */, Media }
pub trait BlockService {                    // no hop, no Send bound; a network implementation can come with a third node
    fn reserve_slot(&self, lun: LunId, len: u32) -> Result<RingRef, BlockErr>;
    fn submit(&self, ctx: IoCtx, op: BlockOp<'_>) -> impl Future<Output = Result<(), BlockErr>>;
    fn epochs(&self) -> &EpochPage;
}
```

### 7.7 `ProtocolBlade` (Protocols): `nbladed` reconciles, never serves

```rust
pub enum BladeKind { Iscsi, FcScsi, NvmeTcp, NvmeRdma, Nfs, Smb, S3, FcMirror }
pub enum AccessState { ActiveOptimized, Transitioning, Standby, Unavailable }
pub enum LunClass { Esxi512e, Native4k }       pub enum PrPolicy { StandbyPaths, PrSurvival }
pub enum SvmClass { Datastore /* NFSv3 only */, File /* NFSv4.1 only */ }
pub enum ShareClass { SmbOnly, Multiprotocol }  pub enum WriteDurability { Expire2s, SyncAlways }
pub trait ProtocolBlade {                   // nbladed: sole writer of configfs, sysfs, modprobe.d, namespaces, systemd units
    fn kind(&self) -> BladeKind;
    fn apply(&mut self, desired: &BladeSpec) -> Result<(), ApplyErr>;                 // idempotent; devices pre-created on both nodes
    fn set_access(&mut self, agg: AggId, st: AccessState) -> Result<(), ApplyErr>;    // THE takeover primitive: group flip or LIF up/down
    fn stop_exporting(&mut self, agg: AggId) -> Result<(), ApplyErr>;                 // under 50 ms; never deletes devices; the hard fence is silence
    fn exported_state(&self) -> Vec<StateBlob>;                                       // APTPL file, nfsdcld db, Samba state
}
```

### 7.8 Engine contracts (Storage)

```rust
pub struct MediaProfile { pub segment_bytes: u64 /* a whole number of full stripes, about 80 MiB HDD, 8 MiB flash */,
    pub stripe_data_bytes: u32, pub cp_timer: Duration /* HDD 10 s, flash 5 s; ring/2 wins under load */,
    pub io: IoPath /* Md { fua_super } | NvmePassthru { iopoll } | Plain */, pub discard: Discard /* Never | WholeSegment { requires_rzat } */,
    pub gc_start: f32, pub gc_urgent: f32, pub write_stop: f32 /* 0.70, 0.80, 0.90 */ }
pub struct Pba(u64);                                            // seg:28 | off:20 (4 KiB units) | spare:16
pub struct MapExtent { pub lba: u64, pub len: u32, pub plen: u32 /* reserved for compression */, pub pba: Pba, pub birth: CpSeq }
pub struct SegmentInfo { pub id: SegId, pub lap: u32, pub stripe_data_bytes: u32 /* geometry at birth */, pub live_bytes: u64, pub age: CpSeq }
pub struct Superblock { pub ver: u16, pub agg: AggId, pub epoch: u64, pub cp_seq: CpSeq, pub cut: Cut, pub map_root: Pba,
    pub seg_table_root: Pba, pub dirty_since_cp: VolBitSet, pub opened_stamp: u64, pub crc: u32 }   // four slots A0 A1 B0 B1; readers take max (epoch, cp_seq)
pub trait Backstore {                        // md array | single NVMe | (Phase 3) native log-structured RAID
    fn profile(&self) -> MediaProfile;
    fn write_segment(&mut self, seg: SegId, iov: &[IoSlice<'_>], tag: u64) -> Result<(), Busy>;   // full stripes only
    fn read(&mut self, pba: Pba, len: u32, dst: &mut [u8], tag: u64) -> Result<(), Busy>;         // CRC verified on every read
    fn commit(&mut self, sb: &Superblock, slot: SbSlot, tag: u64) -> Result<(), Busy>;            // flush fan-out, then FUA stripe
    fn discard(&mut self, seg: SegId) -> Result<(), Busy>;
    fn poll(&mut self, out: &mut Vec<Done>) -> usize;
}
pub trait BlockMap {                         // per-volume CoW B+tree (flat image in Phase 1), one root per CP
    fn resolve(&self, vol: VolId, lba: u64, len: u32, out: &mut Vec<MapExtent>) -> Result<()>;
    fn apply(&mut self, vol: VolId, new: &[MapExtent], dead: &mut DeadList) -> Result<()>;
    fn checkpoint(&mut self, w: &mut SegmentWriter) -> Result<MapRoot>;
    fn pin(&mut self, vol: VolId, at: CpSeq) -> SnapId;                                            // snapshot = pinned root
    fn diff(&self, vol: VolId, base: CpSeq, out: &mut Vec<MapExtent>) -> Result<()>;               // birth > base: replication, plex resync
}
pub trait ReadCache { fn get(&self, pba: Pba, len: u32, dst: &mut [u8]) -> bool;
    fn fill(&self, pba: Pba, src: &[u8], class: CacheClass, at: CpSeq); fn drop_segment(&self, seg: SegId, lap: u32); }
pub struct CpManifest { pub agg: AggId, pub cp: CpSeq, pub cut: Cut, pub placements: Vec<(RingRef, Pba)>,
    pub meta_pages: Vec<(Pba, Box<[u8; 16384]>)>, pub frees: Vec<SegId> }                           // plex-by-reference, Phase 2
```

### 7.9 Invariants every crate's property tests encode

1. ACKed implies Placed on both nodes and slot retained until the covering CP commits (and, under plex, until the partner's credit).
2. The CP cut advances only over Placed records; a seq-prefix of Placed records is an image a power cut could have produced.
3. Takeover replays every CRC-valid Placed record above the last committed CP, in seq order, deduplicated by seq; no acknowledged write is lost on a single fault.
4. No two nodes ACK writes for one aggregate in overlapping real time (leases L1 to L4 with bounded clock-rate drift).
5. A record whose owner epoch is below the aggregate's current epoch is never indexed or replayed.
6. Old-epoch inbound paths are torn down before any slot is reused.
7. A client never observes BUSY, CHECK CONDITION or an NVMe error for a held request; it observes a hold, then Transitioning.
8. Every persisted or on-wire structure carries a version; release N reads N and N-1.
9. Nothing in process memory or `/var` is load-bearing; state that must outlive a node lives in Raft (tiny) or on the aggregate.

---

## Part 8. Phase 1 roadmap on Debian

### 8.1 Objective

On Debian 13 trixie, two data nodes plus a witness, on a PR-fenced dual-expander SAS shelf (HDD RAID6) with a TCP mirror over a direct DAC, deliver verified failover for iSCSI and NFSv3 datastores: silent-node block resume p99 9 s, wedged-node 13 s, hard 15 s, zero acknowledged-write loss on single faults proven by a linearizability checker, nvfail on dual power loss, 10GbE line-rate ingest with back-to-back CPs, a daemon crash that is never a takeover, and a dashboard line that says "last failover: 2.8 s, 03:00 today". That sentence is the homelab critic's single compelling feature and the Phase 1 exit. Everything else is Phase 1b (weeks 13 to 16, if the critical path is green) or a later phase.

### 8.2 Platform baseline

Debian 13 trixie, `linux-image-amd64` 6.12.x (generic flavour), `targetcli-fb` for debugging only, `nfs-utils` 2.8.3, `samba` 4.22 (Phase 1b), `mdadm` 4.3 or newer, `sg3-utils`, `smartmontools`, `fence-agents` (ipmilan, redfish), `multipath-tools` (shipped configuration), `nvme-cli`, `rdma-core` (Phase 2), `versitygw` v1.8.0 static binary (Phase 1b), `ktls-utils` (Phase 2). Rust stable, pinned crates: `openraft` =0.9.25, `redb` 2.x, `libublk` =0.4.8 (vendored), `io-uring` 0.7, `crc-fast` =1.10.0 or `crc32c`, `zerocopy` 0.8, `rustix`, `memmap2`, `tokio` and `rustls` for the control plane, `axum`, `zbus`, `rtnetlink`, `postcard`, `proptest`, `loom`, `stateright` or `madsim`. Refused by the installer: stock bookworm (no `ublk_drv`), the cloud kernel flavour, USB boot media, nodes under 32 GB, HA tiers under 64 GB, fewer than 8 cores, a shelf without PR-capable members and without a confirmed power fence, DM-SMR drives. Kernel matrix: 6.12.x product; bookworm-backports 6.12 one extra qualification lane; trixie-backports 7.x a CI lane only.

### 8.3 Workspace

Crates: `otap-record` (format v1, CRCs, versioning), `otap-ring` (ring files, slot-state page, acked bitmap, scanner, replay-all, re-attach), `otap-index` (per-volume dirty index, aggregate seq, epoch-flip cut, Placed gating, throttle), `otap-cp` (`Backstore` for md and single NVMe, `MediaProfile`, segment writer, A/B/A/B superblocks, the flat map image, recovery, nvfail), `otap-mirror` (`TcpLink`, `LocalNvmeLink` in 1b; `RdmaLink` Phase 2), `otap-control` (`TcpMtls`, `ShelfMailbox`), `otap-raft` (openraft on redb, `ClusterState`, leases, barriers, sealed secrets), `otap-fence` (watchdog, fence-agents exec, `sg_persist`, Shelly and Tasmota, ARP probe), `otap-gate` (the epoch page), `otap-ublk` (libublk front door, `ReqId`, user copy, recovery and reissue), `otap-blade` (`blade_lio`, `svm`, `blade_nfs`; `blade_nvmet`, `blade_smb`, `blade_s3` later), `otap-api` (axum over mTLS, `otapctl`), `otap-check` (the Porcupine-style checker, the `dm-log-writes` harness, the fault injector), `otap-bench`. Binaries: `otap-ringd`, `dbladed`, `nbladed`, `otap-ctl` (and `--witness`), `otapctl`, `otap-faults`, `otap-host-check`. Package: one `.deb` set plus the installer ISO that applies the platform profile.

### 8.4 Spikes (weeks 1 to 3) and their pass criteria

| Spike | Pass criterion | Decides |
|---|---|---|
| P1 PR through the shelf expanders (week 1, critical) | `sg_persist` register and preempt-and-abort through both expanders; SIGSTOP the owner with a dirty 1.5 GiB stripe cache, preempt, resume: zero bytes land; assemble time measured | the Phase 1 HA tier (R1) |
| P2 Redfish or IPMI power fence (week 1) | `fence_redfish` or `fence_ipmilan` powers off and confirms `PowerState=Off` within 5 s on the lab iDRACs | the fallback fence |
| S2 `tcm_loop` plus fileio on tmpfs `huge=always` (week 1) | 1 M batches; every GOOD is followed by a CRC-valid record in the mapping within 10 us; the LIO thread sustains 2 GB/s; the scanner costs under 1% of a core at 1 ms sleep | the record format against a kernel writer |
| W1 watchdog self-fire and NO_REBOOT (week 1) | each driver on the lab boards fires once at install; the NO_REBOOT bit is detected | the fence tier per board |
| B1 iblock BUSY (one day, week 1) | an injected `-ENOMEM` from ublk observed as SAM BUSY (or not) on 6.12 | whether BUSY is a SCSI-only optimization |
| R1 hugetlbfs and tmpfs registration (week 2) | an 8 GiB arena registered as at most 8 entries with huge-page coalescing on 6.12; `IORING_REGISTER_CLONE_BUFFERS` into three rings; tmpfs THP backing confirmed | the ring file types (R3) |
| D1 HDD drain curve (weeks 2 to 3) | at least 1.2 GB/s empty with zero member reads during a CP; the aged-pool curve at 70% and 80% under uniform 4 KiB overwrite published | ring sizing and throttle constants |
| X1 restart budget (week 3) | 50 `kill -9` of `dbladed` and of `otap-ringd` at 1 GB/s: serving again with the mirror re-established p99 2 s, worst 5 s, zero watchdog fires | R4 and the timer table |
| N1 NFS grace and courteous server (week 2, measurement) | early grace end with replicated nfsdcld, one absent client and one v4.0 client; a client idle past the 10 s lease keeps its state | R8 |
| S1 N2N dual mode (weeks 1 to 2, off the critical path, if cards exist) | 100 cable pulls, re-login under 5 s, no oops, QD1 put p50 under 200 us; asymmetric two-link as fallback | Phase 2 FC hardware and whether an FC mirror is ever viable |

### 8.5 Timeline (16 weeks)

- **Week 1.** Rulings ratified by the owner (Part 9); lab order placed; spikes P1, P2, S2, W1, B1 and S1 started; `otap-record` v1 frozen with property tests; the timer table, the NDO matrix and the client timeout recipes written before any takeover test exists.
- **Weeks 2 to 4.** `otap-ring` (both files, slot-state page, scanner, replay-all, re-attach without a payload scan), `otap-index` (Placed-gated cut, throttle), `TcpLink` and `otap-ringrecv`, the epoch page, `otap-ublk` with a null engine; spikes R1, D1, N1, X1; the checker skeleton; md tuning on the shelf (stripe cache, group threads, bitmap).
- **Weeks 5 to 7.** `otap-cp` on md RAID6 (full-stripe `writev` from the arena, flush, FUA superblocks, the flat map image), recovery for clean shutdown, single crash and dual loss, nvfail with the volume list; `otap-raft` on redb with the witness as both a Proxmox VM and a Pi, leases L1 to L4, the fence executor with evidence, `ShelfMailbox`; the TLA+ model of leases, epochs, GrantUnmirrored and replay-all checked with Apalache.
- **Weeks 8 to 10.** `blade_lio` (iSCSI with ALUA groups per aggregate, pre-staged identical trees, group flip, `stop_exporting`, the RST-proxy test hook), the four barriers, planned takeover and giveback with the preflight, 100 forced takeovers of each class against ESXi 8, Linux dm-multipath and Windows MPIO with the shipped recipes; the Porcupine-style checker on a canary LUN; 100,000 madsim or turmoil schedules over the partition matrix; loom on the epoch page.
- **Weeks 11 to 12.** The datastore SVM (network and UTS namespace, knfsd NFSv3, lockd and statd inside the SVM, LIF moves with gratuitous ARP) against ESXi and Proxmox; GrantUnmirrored with hysteresis; Prometheus metrics, smartd, `mdadm --monitor`, webhook alerts; `otapctl` and cluster formation (join, CA enrolment, witness replacement, configuration export); the installer profile as an ISO; the 10-minute line-rate soak, the 24-hour RSS soak, 30 nightly rehearsals measured by the witness as a synthetic client; exit review against 8.7.
- **Weeks 13 to 16 (Phase 1b, only if the critical path is green).** `LocalNvmeLink` and single-node mode on used Optane; the file SVM with NFSv4.1 at 10/20 (or 30/45 per N1) and replicated nfsdcld; SMB-only shares on Samba 4.22 with `Expire2s`, Previous Versions from manually taken snapshots and the disruptive label; versitygw as a per-SVM black box with object lock and versioning validated against Veeam; the NUT hook; FC-SCSI client targets through `tcm_qla2xxx` if the owner's hosts are FC-attached; the four-protocol load test on an 8-core, 64 GB node.

### 8.6 Lab bill of materials, keyed to the week it is needed

| Item | Week | Note |
|---|---|---|
| Two used dual-socket Dell nodes (R730xd or R740xd class), at least 8C/16T and 64 GB, iDRAC Enterprise, BOSS or SATA boot SSD | 1 | the owner's R740xd qualifies; the second node matches |
| Dual-expander SAS shelf (MD1200, MD1220 or MD1400) plus two SAS HBAs (9207-8e for 6G, 9300-8e for 12G) plus two cables | 1 | spike P1 |
| Twelve NL-SAS drives of 12 to 16 TB, 512-byte formatted (used NetApp or EMC drives need `sg_format`) | 1 | SAS, never SATA, for PR |
| Two SAS SSDs for the metadata class | 5 | optional in Phase 1 |
| Two 10GbE or 25GbE NICs per node and one DAC for the mirror | 1 | mirror on its own port |
| Witness: a Proxmox VM on a third box and a Raspberry Pi 4 or 5 | 5 | both exercised; disk never on OpenTAP |
| One smart outlet (Shelly or Tasmota) or a switched PDU | 5 | power pulls and the outlet fence |
| Clients: an ESXi 8 host, a Proxmox host, Windows Server 2025 or Windows 11, a Linux host | 8 | recipes shipped with the product |
| Two used Optane P1600X or 900P | 13 | `LocalNvmeLink` |
| QLE2562 pair and cables (S1); QLE2692 pair if available | 1 (off the critical path) | Phase 2 FC decisions |
| ConnectX-3 Pro or ConnectX-4 Lx pair plus DAC | Phase 2 | RDMA leg |
| Veeam server | 13 | S3 immutability tests |

### 8.7 Exit criteria (all measured, all automated; the homelab bar)

1. 100 unplanned takeovers of each class (power pull, DAC pull, LAN switch kill, SIGSTOP of `dbladed`, sysrq crash): silent-node block resume p99 9 s, wedged-node p99 13 s, hard bound 15 s on every run; ESXi with the shipped recipe within 10 s, dm-multipath within 9 s, Windows MPIO within 15 s through the gated RST proxy; zero guest I/O errors; the checker shows zero acknowledged-write loss and no stale reads on every single-fault run.
2. 1,000 simulated and 50 real dual power cuts recover to the committed cut plus every Placed record, nvfail engages every time with the correct CP sequence, window and volume list, and XFS over the LUN mounts clean at every `dm-log-writes` cut point.
3. 50 `kill -9` of `dbladed` and 50 of `otap-ringd` at 1 GB/s: serving again with the mirror re-established p99 2 s, worst 5 s, zero watchdog fires, zero lost acks, zero duplicate applies through ublk reissue.
4. LAN dead on both nodes: A and B keep two of three over the DAC and the shelf mailbox; no fence, no takeover, no epoch bump on restore. Ethernet-only pair with the LAN dead and no DAC path: A stops exporting at 3 s, B promotes within 10 s, zero acknowledged writes lost.
5. GrantUnmirrored within 2 s of mirror loss; revoke only after 10 minutes stable; a flapping DAC (one flap every 20 s for 10 minutes) produces one alert and no client-visible pause after the first; forced takeover prints the correct loss figure.
6. 10GbE line rate (at least 1.0 GB/s) of 4 KiB random writes for 10 minutes on a 64 GB node with 8 GiB rings, CPs every 4 s or less, no request held over 2 s, zero initiator errors at twice the drain rate offered; a hung drive is failed by md inside the hold bound with no escalation past ABORT TASK.
7. The HDD drain curve published: at least 1.2 GB/s empty, 0.4 GB/s at 70% and 0.3 GB/s at 80% under uniform overwrite; reserves and throttle constants derived from it.
8. iSCSI 4 KiB QD1 mirrored write p50 200 us and p99 1 ms through LIO and ublk; 50 K mirrored 4 KiB IOPS on at most 4 cores; D-Blade ack p50 60 us; a sequential write of 1.0 GB/s on at most 2 cores for target, ublk and engine front end.
9. NFSv3 datastore pause equal to the block ladder on ESXi and Proxmox, planned and unplanned.
10. Planned takeover and giveback at most 3 s client-visible over 30 nightly rehearsals, measured by the witness as a synthetic client and shown on the dashboard.
11. Fence quality enforced: HA refused on a shelf without PR-capable members or a confirmed power fence; each watchdog driver self-fires at install; `soft_panic` without `kernel.panic` flagged.
12. Configfs dumps of both nodes differ only in addresses and id ranges; daemon RSS unchanged after 24 hours; boot-to-serving at most 10 s for a 50 TB aggregate; the witness passes everything at 100 ms plus or minus 50 ms of injected RTT and the Pi logs under 100 fsyncs per day.

### 8.8 Phase map

- **Phase 2.** NVMe/TCP (ESXi 8, Proxmox, Linux); FC-SCSI client targets (if not in 1b); the RDMA leg with rxe in CI; snapshot scheduling with reserve, autodelete and autogrow; async replication from `birth_cp` to a second OpenTAP or to S3; plex-by-reference with the `Credit` contract (flash and consumer tier); pool growth one drive at a time via md reshape with per-segment geometry; encryption at rest (dm-crypt above md); the CoW B+tree map; AD join; the web UI; OpenTAP-in-a-VM qualification; the per-I/O trace; NFSv4.1 and SMB general availability with Time Machine and Previous Versions.
- **Phase 3.** The FC mirror leg (conditional on S1 and need); multiprotocol shares with an identity source; PR survival and WSFC; native log-structured RAID with parity-guided repair (bit-rot healing); CP-time compression; `LocalLink` general availability; a management API for Kubernetes CSI; LIF-only migration (which needs the remote N-to-D path and a third node to pay off).
- **Phase 4.** Cold tiering to S3; a third node; continuously available SMB on Samba 4.25 with CTDB as an experimental SMB-only share class.

---

## Part 9. Decisions only the owner can make

1. **Ratify the Phase 1 HA tier and the lab purchase** (R1): the PR-fenced SAS shelf, gated by spike P1, with the iDRAC power fence as fallback; and the long-term default tier (my recommendation: shelf for HDD, plex for flash).
2. **The degraded-durability default** (R10): `placed` (serve unmirrored, alarm, one more fault loses the window) versus `cp` (fsync costs a CP while degraded); and whether to recommend used Optane as the standard `LocalNvmeRing` device.
3. **Project licence and the dependency allowlist** (R24): Apache-2.0 recommended; AGPL excluded.
4. **Secrets-at-rest posture** (R23): sealed in Raft with a key held only on the data nodes, or an external secret store.
5. **Capacity marketing** (R19, Storage): usable = 0.8 times physical on a single plex, 0.4 times under plex.
6. **Identity source default** (R20): standalone local users in Phase 1, AD optional in Phase 2, no multiprotocol shares until an identity source exists.
7. **Platform floor as a published requirement** (R12): 64 GB for HA, 32 GB single-node, 8C/16T, SSD boot, no 2.5GbE-only boards.
8. **FC scope** (R13): FC-SCSI client targets in Phase 2 or Phase 1b; the FC mirror leg conditional in Phase 3 or dropped.
9. **A Go child process (versitygw) inside the product** (R22): yes recommended; the alternative is 1 to 1.5 engineer-years on a native gateway.
10. **Witness placement** the owner will actually run: a Proxmox VM, a Pi, or a VPS within 100 ms; and the rule that its disk never lives on OpenTAP.
11. **Single-node mode as the v0.1 shape** (R1, R11): yes recommended; it is also the only mode a 32 GB node can run.
12. **nvfail auto-clear** (R5): opt-in at install for single-UPS labs, or always manual.

---

## Appendix A. Round-2 verified facts (carry forward with the round-1 digest)

- Debian bookworm 6.1.x: no `CONFIG_BLK_DEV_UBLK` line in any fragment, no `ublk_drv.ko` in any package; `TCM_QLA2XXX`, `TCM_USER2`, nvmet FC/TCP/RDMA, `SMB_SERVER`, `NFSD_V4`, `MD_CLUSTER` present. Trixie and bookworm-backports 6.12.x: `BLK_DEV_UBLK=m`, nvmet loop and fcloop, `NVME_TARGET_AUTH=y`, `NVME_TARGET_TCP_TLS=y`, `DM_LOG_WRITES`, `HUGETLBFS=y`, `SOFT_WATCHDOG`, `ITCO_WDT`, `SP5100_TCO`, `IPMI_WATCHDOG`, `HP_WATCHDOG`, `MLX4_INFINIBAND`, `RDMA_RXE`, `LOOPBACK_TARGET`, `TCM_FC`. The cloud flavour disables `SCSI_QLA_FC`.
- rtslib-fb 2.1.76 and targetcli-fb 2.1.53 carry only path patches; the qla2xxx fabric is exposed; saveconfig lives at `/etc/rtslib-fb-target/saveconfig.json`; sid has rtslib 3.0.
- rdma-core's mlx4 provider lacks `IBV_QP_INIT_ATTR_SEND_OPS_FLAGS` and `ibv_wr_*`; jonhoo `ibverbs` 0.9.2 uses `ibv_create_qp` and `ibv_post_send` and works on mlx4; sideway 0.4.3 `build()` works on mlx4, `build_ex()` does not.
- LIO fileio opens `O_RDWR|O_CREAT|O_LARGEFILE|O_DSYNC`; `fd_buffered_io=1` clears O_DSYNC and forces `emulate_write_cache=1`; one kernel copy via `vfs_iter_write`; 8 MiB per command; hugetlbfs has no `write_iter`, so fileio on hugetlbfs configures but every write fails; mlock on hugetlbfs is a no-op.
- qla2xxx on 6.12: ISP24xx/25xx load the request_firmware blob before flash without comparing versions; ISP81xx, 83xx and 27xx load flash first; `ql2xfwloadbin` overrides; linux-firmware never shipped ql2600/2700 blobs; trixie's firmware-qlogic 20250410 carries ql2500 8.07.00; `qlini_mode` accepts exclusive, disabled, enabled, dual; N2N login branches exist; asymmetric Linux-to-Linux N2N has public history, symmetric dual mode does not.
- FCP write = FCP_CMND, XFER_RDY, DATA, FCP_RSP; 8GFC QD1 4 KiB put-to-Placed through LIO about 80 to 200 us; 25GbE RoCE 4 KiB RDMA write about 6 to 12 us; busy-polled 10GbE TCP 4 KiB app RTT 20 to 35 us median.
- md raid456 discard: `devices_handle_discard_safely` read at array start, members must advertise at least one full stripe of discard, partial-stripe discards silently dropped, `max_write_zeroes_sectors=0`, no RZAT check; dm-raid has the same parameter.
- io_uring: single-folio coalescing in 6.3, multi-huge-page coalescing in 6.12 with uniform folios, 1 GiB per registered entry, `CLONE_BUFFERS` in 6.12 with an empty destination (replace and offsets in 6.13).
- ESXi before 9.0: SAN LUNs must report 512-byte logical sectors; local 4Kn HDDs emulated since 6.7; 9.0 adds 4Kn for SSD-class devices only.
- HDD: 16 to 24 TB CMR 255 to 285 MB/s outer diameter, about 80 IOPS QD1, 168 to 214 at QD16 to 32; a 12-drive RAID6 1.2 to 2 GB/s empty; 0.3 to 0.5 GB/s at 80% under uniform overwrite.
- libublk 0.4.8 (2026-08-29) and crc-fast 1.10 current; libublk yanks releases often.
- Samba 4.22: durable handles need `kernel oplocks = no` and `kernel share modes = no` (defaults since 4.16) and survive `posix locking = yes` unless byte-range locks are held; leases need kernel oplocks off; kernel share modes need a VFS module only GPFS provides. Samba 4.25 (sid) adds experimental persistent handles under CTDB only.
- LIO XCOPY: LID1 only, 0xE4 and 0x02 descriptors, same session, same node, `emulate_3pc=1` on both; no ODX anywhere.
- ublk flags by kernel: USER_RECOVERY and REISSUE 6.1; USER_COPY 6.4; FAIL_IO 6.13; zero-copy 6.15; AUTO_BUF_REG, QUIESCE, PER_IO_DAEMON, UPDATE_SIZE 6.16; 6.12 clears ZERO_COPY silently.
- nfs-utils 2.8.3 (trixie only) `rpc.nfsd --scope` = UTS unshare plus `sethostname`, skipped if nfsd already runs in the namespace; the kernel rewrites the per-namespace scope and major id from the caller's nodename on every `nfsd_svc()`; `nfsdcld -p -s`; early grace end only with nfsdcld tracking and every prior-epoch client reclaiming; grace >= lease unenforced, range 10 to 3600 s; lockd grace shared per namespace; an empty nfsdcld database skips grace.
- versitygw v1.8.0: object lock, experimental POSIX versioning, multipart, bucket policies, six IAM backends, no lifecycle, no object ACLs; Garage 2.3 has no versioning or object lock; MinIO archived 2026-04-25.
- Windows Server 2025 and Windows 11: no inbox NVMe-oF initiator; the Insider preview lacks multipathing.
- openraft 0.9.25: `enable_elect`, `enable_heartbeat`, `ensure_linearizable()` (a read-index quorum round trip); `Trigger::elect()` bypasses the flag; 0.10 restructures the API.
- LIO PR: `res_aptpl_metadata` and `force_pr_aptpl` gated on export count, write-only, the file at `$dbroot/pr/aptpl_<serial>` written synchronously inside PR OUT; ISID restored and matched; generation not saved.
- softdog: CLOCK_MONOTONIC hrtimer, migrates between CPUs, `emergency_restart()` or `panic()`; Debian `CONFIG_PANIC_TIMEOUT=0`; `HARDLOCKUP_DETECTOR=y` with panic unset.
- Windows iSCSI: MaxRequestHoldTime 60 s, LinkDownTime 15 s with MPIO, no NOP-Out by default; ESXi NFSv3 heartbeat defaults 12/10/5 give 113 to 130 s; ESXi software iSCSI 10 to 35 s to mark a path dead; APD holds VM I/O.
- Samba 4.17 to 4.24 and ksmbd: no cross-node handle state; ksmbd durable handles since 6.9 with 2026 CVEs in the v2 path.

## Appendix B. Host Utilities (shipped client recipes)

- **ESXi 8.** Software iSCSI advanced settings NoopOutInterval 5, NoopOutTimeout 10, RecoveryTimeout 5; claim rule SATP_ALUA with round-robin PSP; ATS on; both portals in discovery; guest disk timeout 190 s; NFSv3 datastores with the stock heartbeat settings (I/O is held, not failed).
- **Linux.** `multipath.conf`: ALUA priority, `path_checker tur`, `no_path_retry queue`, `fast_io_fail_tmo 5`, `dev_loss_tmo 30`, both portals; `node.session.timeo.replacement_timeout 15`.
- **Windows.** MPIO with the Microsoft DSM and ALUA; LinkDownTime 15 (default with MPIO), PDORemovePeriod 20, disk TimeOutValue 60; both portals; the RST proxy is what makes Windows reconnect inside its hold window.
- **`otap-host-check`.** A script that audits a client's timers, multipath configuration and portal list and prints what to change.
