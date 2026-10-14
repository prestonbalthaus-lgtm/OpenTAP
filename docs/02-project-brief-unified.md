# OpenTAP — Unified Project Brief (revision 2)

Saved 2026-10-12 from the project owner's revised charter. Supersedes the scope in `00-project-brief.md`; the round-1 brainstorm in `01-architecture-brainstorm.md` remains the technical baseline this revision builds on.

## Role definition

Fable 5.1 is the Lead Systems Architect and Engineering Manager for an ambitious bare-metal storage infrastructure project, leading a team of specialized "Opus 5.5" worker agents, orchestrating, delegating and synthesizing their technical outputs to architect OpenTAP: an all-encompassing, unified SAN/NAS storage operating system modeled directly after clustered ONTAP, tailored for power-user and homelab deployments.

## The unified objective

OpenTAP must avoid single-fabric lock-in and single-protocol rigidity. Just as NetApp ONTAP operates across all media and protocols, OpenTAP is a unified, multi-fabric, multi-protocol storage platform:

- **Media agnostic:** runs on mechanical HDDs (leveraging log-structured sequential full-stripe Consistency Points to eliminate head thrashing), all-flash NVMe/SSD configurations, or hybrid tiered pools.
- **Fabric agnostic:** supports Fibre Channel (FC), Ethernet (standard TCP, 10/25/100GbE, RoCEv2), and InfiniBand (IPoIB/verbs) for both client traffic and inter-node replication.
- **Protocol agnostic:** terminates block protocols (FC-SCSI, iSCSI, NVMe-oF/TCP/RDMA) and native file/object interfaces (NFSv4, SMB3, local file system mounts, and an integrated S3 object gateway).
- **Homelab-rationalized durability:** authorizes a volatile DRAM write cache mirrored across the HA interconnect (analogous to running ZFS with sync=disabled or non-BBU hardware RAID). In the event of a simultaneous catastrophic site power loss, clients accept losing the last 10–15 second in-flight Consistency Point window.
- **Quorum and consensus:** designed for a 3-voter model (two active storage controller nodes plus one lightweight Witness VM/node running Raft) to guarantee zero split-brain scenarios without requiring costly quorum hardware.
- **Operating system base:** Debian 12+ (specifically chosen to leverage native in-box kernel target support for QLogic Fibre Channel via tcm_qla2xxx, modern io_uring, and standard open networking toolchains).

## Core architectural layers to design

### Frontend / protocol blades (unified N-Blade)
- Stateless userspace abstraction binding to front-end interfaces.
- Block services: interfacing with Linux kernel target infrastructure (LIO for FC-SCSI and iSCSI; nvmet for NVMe-oF) or userspace passthroughs.
- File and object services: native export services for NFS and SMB, backed by an internal metadata/inode layer mapped over storage extents, and an embedded S3-compatible gateway daemon.
- Translates incoming commands into a unified internal RPC layer destined for the D-Blade.

### Storage engine (log-structured D-Blade)
- Media-adaptive block allocator in Rust (the WAFL replacement).
- Write pipeline: absorbs incoming random writes into an in-memory dirty index (BTreeMap). Every 10–15 seconds (or at memory watermark triggers), flushes queued updates to the storage aggregate.
- Disk management: supports standard mdadm RAID arrays (RAID6 full-stripe flushes bypassing parity read-modify-write penalties) or direct block aggregates on flash.
- Read caching: extensible flash read cache (L2ARC / Flash Cache equivalent) for mechanical arrays.

### Data plane and memory mirroring (the interconnect abstraction)
- Pluggable transport trait (MirrorTransport) for the node-to-node replication path:
  - Ethernet: busy-polled TCP sockets and RoCEv2 RDMA.
  - Fibre Channel: point-to-point and switched FC replication rings utilizing in-box target mode.
  - InfiniBand: native verbs queue-pair mirroring.
- A write is acknowledged to the client as soon as it is placed in Node A's local RAM and mirrored into Node B's RAM over the active transport.

### Control plane and quorum
- Lightweight 3-voter Raft consensus engine running out-of-band on Node A, Node B, and a tiny Witness VM.
- Handles aggregate ownership, ALUA/ANA state transitions, failover orchestration, and fencing leases backed by /dev/watchdog.

## Immediate directive: the brainstorming session (round 2)

No implementation code yet. Four Opus 5.5 workers tailored to the unified vision:

- **Opus-Fabrics (Multi-Transport Specialist):** the pluggable MirrorTransport trait in Rust; zero-copy / low-latency RAM-to-RAM mirroring across Fibre Channel (QLogic via Debian LIO), standard TCP, RoCE and InfiniBand under a single abstraction; link health, heartbeats, and transport fallback (e.g. FC primary, Ethernet secondary).
- **Opus-Storage (Engine and Tiering Specialist):** the D-Blade allocator: queuing random writes in volatile DRAM, assembling contiguous stripes, Consistency Point flushes; adapting between slow mechanical arrays (full-stripe writes to avoid RAID-6 write holes) and flash/NVMe pools; crash consistency and journal replay after a clean reboot vs. post-power-loss array recovery when the volatile cache drops.
- **Opus-Protocols (Unified N-Blade Specialist):** presenting block (FC, iSCSI, NVMe-oF), file (NFS, SMB) and object (S3) protocols from a unified Rust daemon architecture; Debian's LIO target via configfs vs. userspace io_uring/ublk daemons for block; embedding file/object layers on top of internal volume objects.
- **Opus-Consensus (HA and Quorum Specialist):** the 3-node Raft architecture (Node A + Node B + Witness VM); failover state machines: detection timers, epoch fencing, ALUA/ANA path flipping, preventing split-brain when interconnect links break.

Output requirements: individual agent brainstorms (specific Rust crates, kernel interfaces, memory structures, modular trait definitions); adversarial critique (architectural bottlenecks, protocol translation overheads, edge-case failure modes under network partitions, complexity of simultaneous multi-protocol front ends); Fable 5.1 executive synthesis (a cohesive blueprint reconciling the findings, standardized modular Rust trait contracts, and the concrete Phase 1 roadmap for building OpenTAP on Debian).
