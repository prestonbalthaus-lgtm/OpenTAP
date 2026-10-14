# OpenTAP project brief, revision 3: async replication, SVM-first, single-node-first

Date: 2026-10-12. Owner directive for Brainstorming Session 03. Supersedes `02-project-brief-unified.md` wherever the two conflict. Round-2 rulings in `03-architecture-brainstorm-02.md` stand unless section 3 below re-opens them.

## 1. Owner's directive

OpenTAP is an open, hardware-agnostic storage operating system modeled after clustered ONTAP, for homelabbers and self-hosters who want enterprise-grade SAN/NAS features without restrictive licensing. Fable 5.1 is Lead Systems Architect and Engineering Director; the worker personas are Opus-Storage, Opus-Protocols, Opus-Fabrics and Opus-Consensus.

Established platform realities (rounds 1 and 2): Debian 13 (trixie) with the 6.12 LTS kernel, in-box ublk, tcm_qla2xxx and LIO.

### 1.1 Pivot 1: asynchronous replication (the SnapMirror model)

Synchronous microsecond RAM-to-RAM cache mirroring on the active write path is abandoned. Client writes hit Node A's volatile DRAM and are acknowledged immediately at local memory speed (about 5 to 10 us). Every Consistency Point (CP) or snapshot interval (for example 5 to 15 s), block deltas (`birth_cp` extents) are shipped asynchronously to the secondary node over Fibre Channel or Ethernet.

### 1.2 Pivot 2: phased hardware progression C, then B, then A

The platform must be completely hardware-agnostic (like Ceph) and roll out in three milestones:

- Phase C (v0.1, Foundation): a first-class single-node appliance. Validates the core log-structured allocator, the Consistency Point engine and the protocol exports on arbitrary x86 hardware (internal drives, hardware RAID or mdadm) without any clustering overhead.
- Phase B (v0.2, dual-node internal drives): two nodes with internal drives connected by asynchronous replication (plex delta shipping), accepting the 50% capacity trade-off.
- Phase A (v0.3, clustered active/passive SAS): dual nodes cabled to shared external SAS shelves using SCSI-3 persistent reservations and ALUA for controller failover.

### 1.3 Pivot 3: Storage Virtual Machines (SVMs) as the core abstraction

SVMs are the primary organizational unit, exactly as in clustered ONTAP, not an afterthought.

- Multi-tenancy via Linux namespaces: each SVM lives in its own network namespace (IPspace and LIF isolation) and UTS namespace (NFSv4 server scoping).
- SVM root volumes and junctions: every SVM has a dedicated root volume (`/vol/<svm>_root`) holding tenant configuration, Samba machine accounts (`secrets.tdb`), `nfsdcld` databases and mount junctions.
- Service and protocol granularity: protocols toggle independently per SVM (for example a backup SVM running only S3 via versitygw with Object Lock beside a hypervisor SVM serving iSCSI and NFS).
- Resource governance: templated systemd slices per SVM (`CPUWeight`, `MemoryMax`) against noisy neighbours.

### 1.4 Pivot 4: degraded durability and local quorum

- No artificial throttling: if the partner drops offline, OpenTAP runs degraded (`unmirrored_flush = placed`) and keeps acknowledging writes in local RAM without throttling to disk speed.
- Local LAN witness: quorum is a 3-voter Raft topology with a lightweight witness (a local VM, a Proxmox container or a Raspberry Pi on the local network). If the witness fails, storage continues with zero interruption.
- Power-loss safety: on simultaneous dual-node power failure, OpenTAP engages nvfail at reboot, taking affected LUNs offline and alerting, so hypervisors do not write over rolled-back filesystems.

### 1.5 Protocol implementation sequence

1. iSCSI via LIO over `/dev/ublk`.
2. NFSv3 / NFSv4.1 and SMB3 inside isolated SVM namespaces.
3. Fibre Channel: FC-SCSI targets to hypervisors via tcm_qla2xxx.

### 1.6 Directive for Session 03

Do not write implementation code. Run a multi-disciplinary brainstorm with the four personas to align OpenTAP with this async, SVM-first, single-node-first architecture. Deliverables:

1. Individual agent brainstorms: Rust crate selection, Linux kernel interfaces, configuration structures, memory models.
2. Adversarial cross-examination ("poking holes") focused on: edge cases in tracking snapshot deltas during continuous high-ingest writes; permissions and identity collisions across SVMs; client-side timeout behaviour during async replication lag and SVM failovers; performance bottlenecks when mixing hardware RAID cards with the sequential CP flusher.
3. Fable 5.1 executive synthesis and trait contracts: an integrated blueprint; standardized Rust traits for the asynchronous replication stream, the SVM lifecycle manager and the block allocator; a clear, actionable implementation plan for Phase C (single-node v0.1) on Debian 13.

## 2. What stands from round 2 (`03-architecture-brainstorm-02.md`)

Read Part 6 (rulings R1 to R24, timer table, memory budget, platform profile) and Part 7 (trait contracts) before designing. The following carry forward unless this round shows they cannot:

- Platform floor: Debian 13 trixie, kernel 6.12.x, generic amd64 image. Stock bookworm has no `ublk_drv`; the cloud flavour lacks `qla2xxx`. The in-box inventory in Appendix A (LIO fabrics, nvmet, ublk feature set on 6.12, ksmbd, NFSD, watchdogs, dm-log-writes, hugetlbfs) is verified.
- Data path: one ublk device per LUN, handlers inside the engine process, thread-per-core reactors, hugetlbfs arena that survives a daemon restart (daemon crash is not a takeover), LIO or nvmet in front of ublk for block, XFS inside a LUN per volume for NAS, versitygw as a Go child process for S3, two SMB share classes (durable-handle shares and cross-protocol shares cannot be the same share).
- Engine: log-structured, never-overwrite CPs, full-stripe writes, A/B superblock slots with FUA, dirty-since-CP bit, `MediaProfile` as the only HDD-versus-flash knob, LUN classes `Esxi512e` and `Native4k`, format reservations (`plen`, per-segment geometry, `birth_cp`, four superblock slots).
- Durability semantics: advertise WCE=1; FLUSH and NFS COMMIT are ordering barriers, not disk barriers; nvfail after a dirty boot; BUSY does not exist (hold at most 2 s, then ALUA Transitioning; hard bound 10 s).
- Control plane: openraft 0.9.25 with a non-electable witness, leases granted without a log write, per-aggregate epoch page checked at submission and completion, declarative Raft state plus a reconciler, `nbladed` as the only writer of configfs, sysfs, modprobe.d, netns and systemd.
- Host posture: kernel command line `panic=-1 hardlockup_panic=1 softlockup_panic=1 panic_on_oops=1`, softdog `soft_margin=5` with no `soft_panic`, watchdog pet 500 ms.
- Client timers (verified): Windows iSCSI MaxRequestHoldTime 60 s and LinkDownTime 15 s; ESXi iSCSI 10 to 35 s to mark a path dead; ESXi NFSv3 hold 113 to 130 s; NFSv4.1 file SVM lease 10 s and grace 20 s with the courteous server; NLM grace 20 s; SMB `dirty_expire` 2 s. Samba durable handles need `kernel oplocks = no` and `kernel share modes = no`; no Debian Samba carries SMB handles across nodes.
- Verification approach: TLA+ for the ownership protocol, madsim for the daemons, a checker that proves zero acknowledged-write loss, dm-log-writes for crash consistency, property tests that freeze the on-disk format before any code that depends on it.

## 3. What this round must re-decide

The pivot invalidates these round-2 results. Re-derive them; do not patch them.

- `MirrorLink`, `MirrorInbound`, `BatchPage` ring files, the `Placed` acknowledgement, `GrantUnmirrored` hysteresis and the `otap-ringd` mirror role. Replacement: an asynchronous delta stream and a replication-relationship state machine. The hugetlbfs arena stays; whether `otap-ringd` survives as the arena owner is open.
- Phase 1 as "shelf first". Replacement: Phase C single node, Phase B internal drives plus async replication, Phase A shared SAS shelf with PR and ALUA.
- The takeover ladder and barriers. With no mirror, every unplanned takeover promotes a copy that is behind the acknowledged state: it is a rollback to the last applied CP, bounded by one CP interval plus ship time plus the lease TTL. Whether that must engage nvfail by default, per volume class, is a decision this round prepares and the owner makes.
- NAS by SVM class alone. Replacement: SVM as the primary unit with root volume, junction tree, namespaces, slices, per-SVM protocol toggles, SVM-level identity and mobility.
- FC scope: FC-SCSI client targets are now protocol step 3 of Phase C; FC as a bulk replication transport is an option to price, not a Phase 3 mirror leg.
- The timer table and the 16-week plan. Replacement: a timer table for Phase B and a Phase C plan.

## 4. Questions Fable wants answered this round

These are agenda items, not rulings. Argue against them where the evidence points the other way.

Engine and replication (Storage, with Fabrics):

- Is the CP's own write set the delta (shipped from the arena as the CP flushes), or is the delta a map walk for `birth_cp` greater than the base? Which for steady state, which for catch-up and resync? What does the receiver need to apply a CP atomically so its copy is always at a CP boundary?
- The base snapshot: what must be pinned on sender and receiver so resync works in both directions, including after a takeover when the old owner returns with divergent CPs? What happens to space when the partner is away for a day? What is the spill policy and when does the relationship break?
- Geometry discovery on arbitrary devices: `optimal_io_size`, `minimum_io_size`, `alignment_offset`, RAID controller VPD, admin override, and a write-amplification probe. How does the flusher behave on a write-back (BBU) controller, on a write-through one, and when discard is unsupported?
- The 6.12 ublk data path: `UBLK_F_USER_COPY` versus the legacy copy, cost per 4 KiB, and where the 5 to 10 us acknowledgement budget goes.
- Memory model for a 64 GB node: arena, dirty index, map cache, read cache, CP staging. What triggers a CP (timer versus high-water mark), and what back-pressure applies when CP N+1 fills while CP N is still flushing to HDD?
- Snapshots as user-visible objects: schedules, retention, space accounting with a replica, and how a snapshot of an XFS-on-LUN volume is exposed to NAS clients at all.

SVMs and protocols (Protocols):

- The exact namespace set (is a mount namespace required for the junction tree?) and how systemd units enter it. What lives in the SVM root volume versus what is declarative in Raft.
- knfsd per network namespace: nfsdfs and rpc_pipefs mounts, rpcbind, mountd, statd, nfsdcld, idmapd per SVM; `rpc.nfsd --scope`; grace per netns; NFSv3 lock recovery after migration via `sm-notify` from the root volume's state directory.
- Samba 4.22 multi-instance: which directories must be separate, machine account and keytab per SVM, winbind per SVM, idmap ranges, and every identity collision you can find.
- Do LIO iSCSI network portals and nvmet TCP ports bind in the configfs writer's network namespace or in `init_net`? This decides whether block protocols live inside the SVM namespace or stay at node level with per-SVM target names and portal groups.
- LIF implementation: macvlan, ipvlan, veth plus bridge, or SR-IOV VF; MAC persistence across nodes; gratuitous ARP; VLANs.
- Resource governance: what slices govern (smbd, versitygw) and what they cannot (nfsd kernel threads, engine I/O), and the per-volume QoS that fills the gap.
- nvfail from the client's side: iSCSI (PDL versus NOT READY for ESXi and Proxmox), NFS (export `fsid` bump to force ESTALE), SMB.
- Rollout: for each protocol step the client matrix (ESXi 8, Proxmox 8 and 9, Windows 11, Debian) and exit criteria.

Transport and FC (Fabrics):

- Stream framing, flow control, resumability mid-CP, integrity (CRC per record and per CP), optional TLS and compression, zero-copy send from the hugetlbfs arena (io_uring SEND_ZC on 6.12), multi-connection parallelism, link selection and fallback (direct link, then LAN), throughput targets per link class.
- FC as a transport: the concrete mechanism (initiator-mode WRITE into a partner-exported LUN?), a port arrangement that avoids dual mode on one port, and whether it is worth building at all. Recommend a phase.
- FC client targets: per-card firmware facts from round 2, `qlini_mode`, NPIV virtual ports per SVM (`tcm_qla2xxx` NPIV fabric) as FC LIFs that can move between nodes, ALUA for Phases A and B, direct attach without a switch, ESXi and Proxmox specifics (claim rules, `Disk.MaxLUN`, multipath), targetcli versus direct configfs writes from `nbladed`, what persists across reboot.

Quorum and mobility (Consensus):

- With internal drives, takeover promotes a stale replica. What fence evidence is required, what is `max_auto_takeover_lag`, is automatic or manual the default, what is the nvfail posture after takeover, what happens to the old owner's divergent writes, and what are the resync direction rules?
- Witness constraints: a Pi on an SD card (no per-heartbeat fsync), a container, a VM; the witness binary on arm64; what the witness stores.
- The no-quorum posture when both nodes lose majority (witness down and link down): freeze ownership (keep serving what you own, never take over) versus stop. Justify for Phase B (private disks) versus Phase A (shared disks, PR fence available).
- Phase C: the same control plane with one voter? Define "no clustering overhead" concretely.
- Replication relationship states (InSync, Lagging, Broken, Resync), alert thresholds, and their effect on takeover eligibility.
- Planned migration with timings: quiesce, final CP, ship, cutover, SVM namespace teardown and rebuild, LIF move, nfsd grace, SMB reconnect. Unplanned migration with the rollback gate. A revised timer table.
- Epoch page and I/O fencing on the survivor and on the returning node (which must not serve stale data). The Raft content (SVMs, volumes, LIFs, relationships) and what is deliberately not in Raft.

## 5. Persona assignments

Opus-Storage (engine, allocator, async replication): single-node v0.1 engine (dirty index, CP assembly, full-stripe flush to hardware RAID LUNs versus mdadm); async snapshot and delta shipper (`birth_cp` tracking, serialization, streaming); nvfail state machine and on-disk flags; plus the engine questions in section 4.

Opus-Protocols (SVM architecture, protocols): SVM encapsulation with netns, utsns and systemd slices; junction paths and the global namespace off `/vol/<svm>_root`; per-SVM knfsd (NFSv3 and v4.1 scoping), multi-instance Samba 4.22 (isolated smb.conf and machine accounts), versitygw (S3 with Veeam Object Lock); rollout plan iSCSI, then NFS and SMB, then FC client targets; plus the SVM questions in section 4.

Opus-Fabrics (async transfer, Fibre Channel): a simplified high-throughput asynchronous replication stream trait, bulk snapshot chunks over TCP versus FC; FC client target mode for QLogic 8G, 16G and 32G HBAs on Debian 13 via configfs and tcm_qla2xxx serving Proxmox and ESXi; plus the transport questions in section 4.

Opus-Consensus (quorum, failure domains, SVM migration): the 3-voter Raft failure matrix (dropped witness, severed inter-node link, true node failure); the `unmirrored_flush = placed` degraded posture without throttling but with correct alerts; SVM mobility and LIF failover on planned giveback and unplanned failover; plus the quorum questions in section 4.

## 6. Constraints for every persona

- No implementation code. Trait, struct and state-table sketches of at most 40 lines each are fine.
- Do not install, build or benchmark on the machine running this session; it is not the target hardware.
- Every version-specific claim about Debian 13 packages, kernel 6.12 interfaces, Rust crates, Samba, nfs-utils, versitygw, ESXi or Proxmox is marked `[verified: <url>]` or `[believed]`. The fact-check stage will test the load-bearing ones.
- Numbers over adjectives: latencies, throughputs, sizes, timers, with the assumption behind each.
- Prefer in-box Debian components and crates that exist on crates.io today with the version named.
