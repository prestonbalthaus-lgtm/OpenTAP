# Step 6B-2: incremental catalogue and snapshot tables (format 2.4), pool attestation (Opus-Storage-Catalogue, 2026-10-16)

Base `8e34b4a` (6B-1). Format 2.3 -> 2.4 (F19 to F21, vectors `v2.4/`, older
vector dirs and their tests kept). Arena unchanged; control protocol stays 1
(variants only). `06`/`07`/`08`/README: the lead's.

## 1. On-disk records (format 2.4)

- **F19 table log page** (`TableLogPage`, 4096 B, magic `OTTL`): 64-byte
  `TableLogHdr` (magic, ver, count, cp_seq, cp_nonce, agg_id, page_seq, table
  byte 1 = catalogue / 2 = snapshots, reserved zero, CRC-32C over 0..60 at
  60), then 4032 B: 63 snapshot records (64 B) or 7 catalogue records (576 B),
  the rest zero; each record keeps its own CRC. A delta region is whole pages
  of one CP: same cp/nonce, page_seq in order, every page but the last full,
  keys strictly ascending (snapshot id; the catalogue's sort key). Upserts are
  whole records, deletions tombstones: `SNAP_TOMBSTONE` (flags bit 7),
  `CATALOG_TOMBSTONE` (bit 0). A base keeps the 2.3 image (`snap_table_image`,
  `catalog_image`) and refuses tombstones.
- **F20 table head page** (`TableHeadPage`, 4096 B, magic `OTTH`): magic, ver,
  table, cp_seq, agg_id, delta_count, 28 reserved, the base entry, 124 delta
  entries, 28 reserved, CRC-32C over 0..4092 at 4092. `TableChainEntry` 32 B:
  region `RootRef`, cp_seq, records (tombstones included), rsvd. Rules: base
  CP <= head CP; empty base = `NONE` with 0 records; delta CPs strictly ascend
  after the base's, none after the head's; each delta has a region and
  records; unused entries zero. Byte 56 is always zero: a 2.3 reader takes the
  head for a snapshot table, reads `kind` 0, refuses it and the whole ring
  before writing anything.
- **Superblock** (minor >= 4): `snap_head_cp` at 1760, `catalog_head_cp` at
  1768, reserved now 1776..4092 (2316 B); `snap_table`/`catalog` name the
  heads. head_cp <= cp_seq (OutOfRange); a `NONE` root has head_cp 0 (BadField).
- **SnapRecord**: `SNAP_UNCERTAIN` (bit 6, persisted) marks a record recovered
  from a damaged chain; `SNAP_FLAGS_MASK = PIN_MASK | SNAP_UNCERTAIN`. CP log
  directory pages also list rewritten snapshot root pages (hazard 9).

## 2. Chain and fold policy

- An unchanged table carries its head (no write); a changed one gets a delta
  of the changed keys + a new head, or a fold (new base, empty chain, head).
- `TableFoldPolicy` (`EngineConfig::table_fold`, `SimConfig::table_fold`):
  `max_deltas` 64 (clamped 1..=124), `min_fold_records` 256. A fold is due
  when the chain would exceed `max_deltas` or delta records + this CP's changes
  > max(base records, 256): a boot replays at most 64 regions per table and at
  most twice the table plus 256 records. Forced folds: no head, chain full at
  124, repair. Pure decision: `plan_table` (Carry / Drop / Delta / Fold).
- An empty table has no head (the chain is dropped whole), except that the
  snapshot head stays, empty or not, while any name or snapshot exists (the
  2.3-reader guard). Freed regions (old head; on a fold the old base and
  deltas) follow the deferred-free rule like every metadata region.

## 3. Boot replay and damage (F21)

- `load_table`: head, base, deltas in order (upsert replaces, tombstone removes).
- A region the booted commit wrote (head_cp or entry cp == candidate) that
  does not verify tears that commit (`TornReason::Table`); the boot falls back.
- An older region that does not verify is shared damage: tolerant scans
  (`scan_table_delta` needs a verifying header naming agg, table, CP and
  position; `scan_table_base`) keep every record that verifies alone. A key
  is uncertain when its last readable record precedes the last damaged region.
- Uncertain or clashing names are dropped; uncertain or duplicate-uuid
  snapshots get `SNAP_UNCERTAIN` (quarantined; a pin keeps the mark). An
  unreadable older head boots without the table. Each case warns
  (`SnapshotTableDamaged`, catalogue damage) and sets `repair`: the first
  commit after the boot folds the table into a clean chain.
- Deleting an uncertain snapshot, or one whose root did not load at boot,
  frees nothing (the second was a latent defect, fixed and tested).
- Hazard 12 fixed (boot dedupes `meta_live` by pba). Boots 2.4 only: minor
  < 4 is `OldLayout`.

## 4. Reserve rule

Numbers unchanged (boot 0 / shutdown 1 / others 2); table regions are placed
by the CP's `MetaStream` under that reserve. New: a policy fold is planned only
if `MetaStream::can_place(base pages + 1)` (`SegmentTable::has_run`); one that
still hits NoSpace sets the chain's `fold_blocked_at` (free segments then) and
the CP is retried at once as a delta CP without flagging pool full; the policy
stays quiet until free segments rise above the mark. Forced folds never wait.

## 5. Verify job (`otapctl pool verify`)

- Engine job (`otap_cp::verify`): `start_verify(VerifyOptions)`; each tick
  advances at most `units_per_tick` (256; block 1 unit, map page 4, summary
  its pages) and `max_ns_per_tick` (2 ms). Captures CP N when no superblock
  write is in flight, then holds deferred frees with cp > N so N's view stays
  on disk while writes, CPs, folds, deletions and the cleaner go on. Aborts
  (holds released) on shutdown, pool full, or free segments at the reserve
  while holding anything. `cancel_verify`, `verify_report`,
  `Event::VerifyFinished`, `OperatorWarning::AttestationFindings`.
- Checks: ring (4 slots, CRCs, agg, location; N's pair = engine's slot; other
  pair A = N-1, B = N-1 or older same parity; monotone owner_epoch,
  intent_seq_seen, opened_stamp); map roots, segment table, CP log dir +
  deltas + pages, stripe list + summaries; both table chains replayed against
  the engine; every tree (index vs engine, page CRC / max_birth / entries /
  births / bounds, overlay, snapshot root identity, quarantine); every
  distinct live block (address, data segment in use, payload CRC, summary CRC
  + identity, descriptor = entry); accounting (data live_bytes vs blocks
  reached: more found Error, fewer Warning; metadata vs referenced regions +
  pending frees). 17 kinds; at most 64 findings kept, counts complete.
- Repair: only table chains (`table_chain` sets `repair`, next CP refolds).
- Daemon: `PoolVerify` starts it (or returns the running job's report) and
  keeps ticking; `PoolVerifyStatus` (NotFound if none); `PoolVerifyCancel`.
  otapctl `pool verify [--no-wait|--status|--cancel]` polls every 100 ms;
  exit 4 when a waited verdict is not clean.
- Protocol (stays 1): `PoolVerify {}`, `PoolVerifyStatus {}`,
  `PoolVerifyCancel {}`; response `Verify(VerifyReport)`; `VerifyReport`,
  `VerifyFinding` (`deny_unknown_fields`); `VerifyState` (running, done,
  cancelled, aborted), `VerifyVerdict` (clean, warnings, damaged, incomplete),
  `FindingSeverity` (error, warning).

## 6. Known limits

- Cost per TiB of live data: about 1 TiB of block reads, plus map pages (16 KiB
  per 4 MiB mapped, about 4 GiB per tree-unique TiB) and summaries (16 B per
  block, about 4 GiB). About 1 MiB per tick at defaults; held frees grow with
  the run, so a long verify on a nearly full pool aborts.
- Cannot see: blocks under a tree that fails to load (drift then a Warning);
  stripes and regions nothing references (only via live_bytes); free space;
  slots older than N-1; the arena and ublk side.
- Hazard 8 (relocated blocks failing CRC) not fixed; reported as `block_crc`.
  A table chain is checked in one slice (up to 126 region reads).
- In memory the snapshot set and catalogue are still cloned and diffed O(n)
  per CP that changes them; only the disk cost is incremental.
- A 2.3 binary still boots a 2.4 pool with neither names nor snapshots (no
  head to trip on) and rewrites it as 2.3; this release then says `OldLayout`.

## 7. Proposed decisions for E-entries

1. Snapshot table and catalogue = base + delta chain behind a head page,
   folded by `TableFoldPolicy` (64 deltas, 256 records, ratio), cap 124; empty
   tables have no head, but the snapshot head stays while any name or
   snapshot exists (2.3 guard via byte 56).
2. Damage in an older region makes later-changed keys uncertain: names
   dropped, snapshots quarantined with persisted `SNAP_UNCERTAIN`, refold at
   the first commit; deleting such a snapshot frees nothing.
3. A policy fold that does not fit is put off (`fold_blocked_at`), never a
   pool-full; forced folds are not.
4. Attestation is an engine job pinning N's view by holding deferred frees,
   yielding to space pressure and shutdown, repairing only table chains.

## 8. Tests

- otap-format `step6b_catalogue_format_tests` (10; proptests
  `every_head_bit_is_covered`, `every_delta_bit_is_covered`); v2.4 lane in
  `format_tests`. otap-sim `step6b_catalogue_sim_tests` (16; proptest
  `random_table_changes_with_folds_and_crashes_boot_to_the_engines_tables`)
  and `step6b_verify_sim_tests` (13; proptest
  `random_workloads_with_crashes_always_attest_clean`). otap-daemon
  `step6b_verify_daemon_tests` (3), otap-ctl `step6b_verify_ctl_tests` (2),
  otap-control `step6b_verify_control_tests` (4); `otap_cp::verify` units.
- At `PROPTEST_CASES=1000`: catalogue walk 59 s, verify walk 43 s, format 0.1 s.
