# Step 1 review: the otap-format audit

Worker report, a read-only audit of `otap-format` as it stood after Step 1 (2026-10-13). Reproduced verbatim from the worker's hand-back apart from the harness wrapper and the path encoding. The lead's response, fix by fix, is in the Step 1 entry of `../08-engineering-log.md`; the format rulings it changed are in `../07-engineering-decisions.md`.

---

## otap-format audit: one blocker; the layouts are clean, the logic around them is not

I edited nothing and ran no build or test. The source files were rustfmt'd at 10:51 while I was reading them; they were unchanged when I re-checked at 11:08, and every line number below refers to that state. Crate root: `E:\Coding\OpenTAP\crates\otap-format`.

**Bottom line:** I found no layout defect. Every struct is padding-free and has the same offsets on any target. The problems are in ring selection, stripe and frame verification, the seal paths and the golden-vector lane. Finding 1 is a blocker; findings 2–11 are should-fix; 12–14 are nits.

### Findings, most severe first

**1. [blocker] `Superblock::select` silently skips a slot from a newer major and mounts an older commit**
- **Where:** `E:\Coding\OpenTAP\crates\otap-format\src\superblock.rs:481-489`, and the same pattern in `boot_write_active` at 633-644.
- **Claim:** The filter `verify().is_ok()` treats a valid newer-major slot as corrupt, so an older commit of our own major wins.
- **Scenario:**
  - A v3 build commits CP 200 to A0 and B0 (`ver` 0x0300, same magic). A1 and B1 still hold CP 199 (v2).
  - The operator downgrades. `check_major` fails on A0 and B0, so `select` returns CP 199.
  - The next commit is CP 200 again (parity 0) and overwrites the v3 slots. A committed CP is lost and nothing reports it.
  - `boot_write_active` drops a newer-major intent the same way, so nvfail misses volumes. That is the unsafe direction.
- **Why it blocks now:** shipped readers cannot be patched later, so this behaviour must be right in v0.1.
- **Fix:**
  - Freeze the rule "magic at offset 0 and `ver` at offset 4 never move across majors".
  - In `select` and `boot_write_active`: matching magic plus a higher major is a hard error, never a skip. Do not require the CRC first, because a new major may change it.
  - Return a `Result` that gives the reason (blank / all corrupt / newer format) instead of `Option`.

**2. [should-fix, high] `select` ranks 4 KiB pages, not slots, ignores aggregate and location, and its tie-break picks the stale copy**
- **Where:** `E:\Coding\OpenTAP\crates\otap-format\src\superblock.rs:481-489`, its module doc at 10-16 and 22-23, F7 in `E:\Coding\OpenTAP\crates\otap-format\src\lib.rs:85-89`, and the test at `E:\Coding\OpenTAP\crates\otap-format\tests\format_tests.rs:804-832`, which encodes the current rule.
- **(a) Pages, not slots.** F7 says "the verified slot", but `select` runs only `Superblock::verify`, which never checks the VolTable. The module doc even invites page-only reads ("is anything dirty" from 4 KiB).
  - Scenario: a power cut during commit N's B0 write persists B0's page but not its 16 KiB table. A0 holds N intact.
  - A page-level `select` ties A0 and B0 and picks B0 (higher slot). B0's table is from commit N-2.
  - Result: a volume whose `rollback_gen` was bumped in N-1 is re-exported under its old fsid and LUN serial — exactly what `rollback_gen` exists to prevent.
- **(b) Tie-break.** Within a pair, A is written before B. So when both copies verify but differ, A is the newer one; `max(slot)` picks B.
  - This is reachable: the design record (05-…-record.md, S1) re-issues a cp_seq after a fallback, and nothing makes `cut_seq` unique across a rewind.
  - Sequence:
    1. CP 1204 commits to A0 and B0.
    2. A dirty boot finds its summaries bad and rewinds to 1203.
    3. The re-issued 1204' writes A0; power fails before B0.
    4. Boot sees A0 = 1204' and B0 = old 1204 with equal (epoch, cp_seq, cut_seq), and picks B0.
    5. Its summaries fail again, and a second rewind throws away the intact 1204' and trips nvfail.
- **(c) Identity.** There is no `agg_id` filter (`boot_write_active` has one).
  - A previous aggregate's A1/B1 with a higher cp_seq beats a fresh mkfs that wrote only the CP-0 pair.
  - Nothing checks that `sb.slot` matches the location the page was read from. A misdirected ring write, or one buffer written to both A and B, is invisible.
- **Fix:**
  - Add `SuperblockSlot::select(agg_id, [(SbSlot, &SuperblockSlot)])` that:
    - verifies the whole slot;
    - requires a matching `agg_id`;
    - requires the `slot` field to equal the location read;
    - orders by (epoch, cp_seq, cut_seq) and resolves ties to the A copy;
    - returns the ranked list, because the rewind needs the next candidate.
  - Document that the engine must not re-issue a cp_seq whose superblock still verifies unless it overwrites that superblock first.
  - Keep page-level `select` only as a hint, or drop it.

**3. [should-fix, high] `StripeSummary::parse` proves integrity, not identity, and the crate offers no freshness check**
- **Where:** `E:\Coding\OpenTAP\crates\otap-format\src\stripe.rs:382-420`; its doc at 15-17 says a dirty boot verifies a CP "by reading summaries only".
- **Scenario:**
  - Physical segment P held lseg 900 (CP 10). It was freed and reused for lseg 1500 in CP 25.
  - The controller acks the stripe write and drops it (the section 1.3 case of "superblock landed before the data").
  - The dirty boot of CP 25 reads P, stripe 0, and gets lseg 900's old stripe. Magic, major, both CRCs and every descriptor pass, so there is no rewind.
  - Reads of those LBAs then fail their map CRC: permanent EIO instead of a clean rewind to CP 24 under nvfail.
  - A resend through `read_chunk` ships the stale stripe as a valid chunk, unless the receiver cross-checks the header's `cp_nonce` — which nothing here does or asks for.
- **Fix:** add `parse_expect(region, StripeId { agg, lseg, stripe_idx, cp_seq, cp_nonce })` returning a distinct `Stale` error, and reword the doc so the boot scan is not described as "CRC only".

**4. [should-fix; blocks declaring the freeze done] There is no N-1 decode lane: golden vectors are rebuilt from code, not stored**
- **Where:** `E:\Coding\OpenTAP\crates\otap-format\tests\format_tests.rs:1275-1553`.
- **Claim:** The builders call `Superblock::new`, `IntentSlot::new`, `StripeHdr::new` and `StreamHeader::new`, which stamp the current `FORMAT_VERSION`/`STREAM_VERSION`. Only hashes are pinned, and no stored bytes are ever decoded.
- **Scenario:**
  - The first `FORMAT_MINOR` bump changes `ver` in the slot, intent and stripe images, so every v2.0 constant fails.
  - The comment's instruction to "add a new vector set beside this one" therefore cannot be followed.
  - Section 7.7 invariant 3 / 8.4 ("N decodes N-1's vectors") has nothing to decode.
  - The cargo-fuzz targets that 8.4 requires do not exist either.
- **Fix:**
  - Check in `tests/vectors/v2.0/*.bin` plus a BLAKE3 manifest.
  - The test decodes those files with the current reader and asserts field values.
  - Compare builder output to the files only for the current version.
  - Add fuzz targets for `StripeSummary::parse`, `SuperblockSlot::parse` and `StreamHeader::read`.

**5. [should-fix] The seal paths accept input that `parse` rejects, and keep data that `parse` silently drops**
- **Where:** `E:\Coding\OpenTAP\crates\otap-format\src\stripe.rs:432-463` and 469-492; `E:\Coding\OpenTAP\crates\otap-format\src\stream.rs:165-188`.
- **Unvalidated headers and descriptors:** `write_summary` checks neither magic, major, kind nor descriptor validity; `seal_summary_in_place` checks only magic and major.
  - A header from `new_zeroed()` or a kind-2 descriptor seals fine. It fails only at the next dirty boot, when both the selected CP and the one before it fail and the aggregate cannot come up.
- **Padding and nblocks:** `seal_summary_in_place` trusts the header's `nblocks` and neither zeroes nor checks the padding.
  - If `nblocks` is one too low, the last descriptor sits in CRC-covered padding and `parse` returns one fewer. GC never relocates that block and frees its segment (EIO later), and replication never ships it.
  - Stale staging bytes (an earlier stripe's descriptors or data) get sealed into the padding. They go to disk, onto the wire and into dump files for other groups, and two equal summaries stop being byte-identical (invariant 3).
- **Frames:** `set_payload` accepts `raw_len` ≠ length for `Raw`, which the peer's `verify` rejects.
- **Fix:**
  - One validate-for-seal routine shared by both seal paths and `parse`.
  - `seal_summary_in_place` returns an error on non-zero padding; zeroing it would hide the off-by-one.
  - `set_payload` derives `raw_len` for `Raw`, or provide a one-shot build-and-seal constructor.

**6. [should-fix] Summary bitmap and descriptor `vol` are never checked against each other or against `MAX_VOLS`; F8's text is wrong**
- **Where:** `E:\Coding\OpenTAP\crates\otap-format\src\stripe.rs:223-241`, 287-295 and 432-437; `E:\Coding\OpenTAP\crates\otap-format\src\lib.rs:90-92`.
- **Bitmap:** `vol_bitmap` is supplied by the caller and never checked. `format_tests.rs:1238-1255` seals vol-1 descriptors with `VolBitSet::EMPTY` and `parse` accepts it, so any consumer that skips stripes by bitmap misses blocks.
- **Vol range:** `pack`/`validate` accept vol up to 2^20-1, but F8 says vol < `MAX_VOLS`. A vol-5000 descriptor validates yet has no bitmap bit, VolTable row or map root.
- **F8 text:** it says the bound "can rise to 2^20 without a layout change". In fact `VolBitSet` (the summary bitmap, `write_active`, `nvfail`, `IntentSlot`) and `VolTable` are sized by `MAX_VOLS`, and changing it moves `STRIPE_SUMMARY_FIXED` and with it every descriptor offset.
- **Fix:**
  - `write_summary` derives the bitmap itself; drop the parameter.
  - `parse` checks bitmap equality (O(nblocks)).
  - Reject vol ≥ `MAX_VOLS` in both `pack` and `validate`.
  - Correct F8.
  - Update the proptests at `format_tests.rs:562-604`, which use random bitmaps and vol up to `MAX_VOL_ID`.

**7. [should-fix; decide before the freeze] birth 0 and the hole descriptor's CRC are undefined**
- **Where:** `E:\Coding\OpenTAP\crates\otap-format\src\stripe.rs:70-79` and 287-295; `E:\Coding\OpenTAP\crates\otap-format\src\map.rs:112-120` and 154-176; golden builder at `format_tests.rs:1371-1402`.
- **birth 0:** `MapEntry::validate` rejects data with birth 0, which implies CP 0 never carries data. But `StripeDescriptor::validate` accepts birth 0, an all-zero descriptor (vol 0, lba 0, Data, birth 0) passes, and `MapEntry::hole(0)` silently equals `UNMAPPED`.
- **Hole CRC:** a Hole descriptor's `crc32c` is undefined.
  - The doc ("CRC-32C of the 4 KiB block") implies the CRC of a zero block; `MapEntry` uses 0 for holes; the golden stripe freezes arbitrary values (`i*0x01010101`).
  - A scrub that checks every slot would reject the golden image. If the value is 0, hole slots are covered by no CRC (invariant 3).
- **Fix:**
  - Document "CP 0 is the mkfs commit; birth 0 means never written".
  - Reject birth 0 in descriptors, and make `hole()` take a `NonZeroU32`.
  - Define the hole CRC as a `ZERO_BLOCK_CRC32C` constant, enforce it, and regenerate the golden stripe.

**8. [should-fix] Two gaps in "a layout change must fail to compile"**
- **Where:** `E:\Coding\OpenTAP\crates\otap-format\src\types.rs:417-428`; the enums in `superblock.rs:49-173`, `stripe.rs:37-95` and `stream.rs:26-112`.
- **RootRef:** its field offsets are asserted only in the tests (`format_tests.rs:246-248`), so swapping `pages` and `crc32c` compiles.
- **Enums:** nothing ties discriminants to their on-disk values, or `from_u8` to `as_u8`. Changing `Hole = 1` to `Hole = 2` compiles, after which the writer emits 2 and its own `validate` rejects it. Reordering `FrameKind` also compiles.
- **Fix:** add `offset_of!` asserts for `RootRef` in `types.rs`, and a const block asserting each variant's value and `from_u8(v.as_u8()) == Some(v)`.

**9. [should-fix] `verify` accepts undefined enumerations and unbounded lengths**
- **Where:** `E:\Coding\OpenTAP\crates\otap-format\src\superblock.rs:412-436` and 519-522; `E:\Coding\OpenTAP\crates\otap-format\src\stream.rs:192-226` and 291-294.
- **Superblock slot:** `VolState::nvfail_cause` (0-3) and `DurableLogHdr::kind` (0-1) are never validated, contrary to convention 4 (`lib.rs:27-28`). Also accepted:
  - `VOL_PRESENT` rows at or above `vol_count`;
  - `write_active`/`nvfail` bits at or above `vol_count`;
  - an nvfail bit that disagrees with its cause.
- **Stream header:** `verify` bounds `wire_len` but not `raw_len` for Lz4/Zstd. `raw_len` 0xFFFFFFFF passes, so a receiver that sizes its decode buffer from it tries to allocate 4 GiB.
- **HoleRecord:** it has no `validate`, and `end_lba` is `lba + len` with overflow checks on in release, so an lba near `u64::MAX` panics the receiving reactor. `len` 0 and lba+len > 2^40 also pass.
- **F5:** not enforced. A `cp_seq` above `u32::MAX` verifies, and callers must write `cp as u32`, which wraps silently.
- **Fix:**
  - Validate the enums and the bit/`vol_count` rules in `SuperblockSlot::verify`.
  - Require `raw_len` ≤ `MAX_WIRE_PAYLOAD`.
  - Add `HoleRecord::validate` (vol < `MAX_VOLS`, len ≥ 1, lba+len ≤ `MAX_LBA`+1) and use `checked_add` in `end_lba`.
  - Add a checked `birth_of(cp_seq)` and a cp_seq ≤ `u32::MAX` check in `verify`.

**10. [should-fix] Records whose CRC lives in the parent have no verify path in this crate**
- **Where:** `E:\Coding\OpenTAP\crates\otap-format\src\map.rs:179-350`; `E:\Coding\OpenTAP\crates\otap-format\src\types.rs:328-366`.
- **Claim:** `MapPage`, `PageIndexPage`, the `MapRoot` table and every `RootRef` region expose `crc32c()`, but nothing compares it with the parent. No function here would catch a single-bit flip in a map page, and the bit-flip suite covers none of these records.
- **Fix:**
  - `RootRef::verify(region)` checking length and CRC.
  - `MapPage::parse(bytes, &PageIndexEntry)` checking CRC and running `validate`.
  - A page-index verify that folds per-page CRCs with `crc32c_combine` and compares against `MapRoot::index_crc`.
  - Bit-flip proptests for each.

**11. [should-fix, doc/policy] The stream header contradicts the documented minor-version rule**
- **Where:** `E:\Coding\OpenTAP\crates\otap-format\src\stream.rs:192-211` against `lib.rs:25-28`; the test at `format_tests.rs:1065`.
- **Claim:** The rule is that readers accept any minor because a minor only gives meaning to reserved bytes. But `StreamHeader` has no reserved bytes, and `verify` rejects flag bits 2-7 and kinds above 12.
  - So a 1.x minor cannot change the header at all, and a 1.1 frame using a new flag or kind is rejected by a 1.0 peer.
- **Fix:** state that stream features (flags, kinds, codecs) are negotiated in Hello and never sent to a peer that did not advertise them, and that new on-disk enum values need a major bump. Alternatively, ignore unknown flag bits.

**12. [nit] `for_stripe` has no upper bound**
- **Where:** `E:\Coding\OpenTAP\crates\otap-format\src\stripe.rs:330-352`; `E:\Coding\OpenTAP\crates\otap-format\src\constants.rs:111-113`.
- **Claim:** any multiple of 4096 is accepted, and the first loop steps down one at a time from (stripe − 192)/4112. A 2^50-byte value (say, a typo in a TOML override) spins for minutes; 2^60 spins for days.
- **Related bounds:**
  - Stripes above `MAX_WIRE_PAYLOAD` (16 MiB) are accepted, though under F3 their image cannot ship as one Chunk.
  - `MAX_WIRE_PAYLOAD` itself exceeds the 8 MiB receive and bounce buffers of section 3.3.
  - Its doc ("8 MiB plus its summary") counts the summary twice.
- **Fix:** clamp the estimate to `u16::MAX`, reject stripes above an 8 MiB `MAX_STRIPE_BYTES`, and tie `MAX_WIRE_PAYLOAD` to the buffer size.

**13. [nit] Alignment behaviour; every failure is a safe `Misaligned` error**
- **Where:** `E:\Coding\OpenTAP\crates\otap-format\src\types.rs:121-127`.
- **4096-aligned types on `Vec<u8>`:** success depends on the allocator. glibc gives 16-byte alignment and fails; jemalloc's 4 KiB size class happens to be page-aligned and succeeds. A test can pass under one allocator and fail under another.
- **Stripe summaries:** `StripeSummary` has no copy-out path.
- **Contiguous receive or dump buffers:** every frame after a payload with `wire_len % 8 != 0` is misaligned, so only `read` works for it.
- **Fix:** document that zero-copy views need `Block4K`, `Page16K` or arena memory; consider padding wire payloads to 8 bytes.

**14. [nit] Smaller items**
- Verification is opt-in. The re-exported `FromBytes::ref_from_bytes` gives an unverified `&Superblock` of the same type `parse` returns. Consider a `Verified<'a, T>` wrapper, and have `select` and `boot_write_active` take it.
- `StripeSummary` (described as a verified view) and `SummaryLayout` have public fields, so hand-built values bypass verification, and `slack_bytes` can underflow on them.
- The crate example at `lib.rs:100-116` seals a bare `Superblock` as "a commit record". Under F1 the unit is `SuperblockSlot`, and calling `Superblock::seal` on a slot's page leaves `vol_table_crc` stale.
- `IntentSlot::new(agg, slot: u8)` does not derive the slot from `intent_seq`. Nothing checks that an intent's slot equals `intent_seq & 1`, or that a superblock's slot parity matches its cp_seq.
- Errors lose information:
  - `BadCrc` does not say which region failed (page or table; header or summary), which the four-fact alert wants.
  - `pack_addr` with lseg 0 reports `OutOfRange` "0 exceeds its maximum".
  - `write_summary`'s `BadField "nblocks"` covers two different failures.
  - `verify_trailing_crc` reports `expected: 4`, which is a minimum, not an exact size.
  - `FormatError` is not `#[non_exhaustive]`.
- `seal_trailing_crc` returns `Option` without `#[must_use]`, so it silently does nothing on buffers under 4 bytes. `#[must_use]` is also missing on `compute_crc`, the `crc32c()` methods, `summary_bytes`, `union`, `boot_write_active` and the packed-field accessors.
- Could be `const fn`: `VolBitSet` set/clear/union/is_empty/count, `Uuid::is_nil`, `supersedes`, `is_clean_shutdown`, the `zeroed()` constructors, `for_stripe`.
- `Uuid` is `repr(transparent)`, not `repr(C)` as the mandate states; the layout is equivalent.
- Deviations from the 7.1 sketches that no F-ruling records:
  - `StripeHdr.nblocks` narrowed from u32 to u16;
  - `PageIndexEntry.rsvd` became `page_crc`;
  - headerless records carry no `ver`, though Part 7 says every structure has one;
  - the frame header lives in otap-format, not otap-stream.
- F2: `off` counts 4 KiB blocks and stripes have fixed 4 KiB slots, so a `plen` below 8 saves no disk space. In v2 the "compression reservation" applies on the wire only.
- The CRC-32C tests are known answers on 9- and 32-byte inputs plus self-consistency checks within crc-fast. Add a bitwise reference proptest over random lengths and start offsets; otherwise a bug in one SIMD code path would surface only as mismatches between nodes.
- Writers should rebuild from `new()` rather than carry a read superblock's newer `ver` and reserved bytes forward, or downgrades break.

### Checked, no issue
- **Padding:** field sizes add up to each struct's size (Superblock 4096, IntentSlot 4096, SuperblockSlot 20480, VolState 16, InboundState 72, DurableLogHdr 64, StripeHdr/StreamHeader/MapRoot 64, all 16-byte records). Array strides equal element sizes. The `IntoBytes` derive and the size/offset asserts would reject padding.
- **Cross-target layout:** every u64 sits at a multiple of 8, so offsets and sizes cannot vary by target. Only `align_of::<u64>()` varies (4 on i686-linux), and the align asserts turn that into a compile error. No `usize`, `bool`, `char`, enum, `Option` or `u128` fields anywhere.
- **Endianness:** all reads are native, which equals LE behind the `compile_error!` guard. The trailing-CRC helpers decode LE explicitly and agree with the fields. Packed words and `VolBitSet` bit numbering are defined on u64 values.
- **StripeDescriptor packing:** shifts 44/4/2 and mask 0b11 match `vol:20 | lba:40 | kind:2 | rsvd:2`, most significant bit first. Range checks and accessors are correct.
- **MapEntry packing:** shifts 28/12 match `lseg:36 | off:16 | plen:12`. `pack_addr` rejects lseg 0, lseg above 2^36-1, off above 65535, plen 0 and plen above 4095.
- **Summary CRC coverage:** `hdr_crc` covers bytes 0..60, including `summary_crc`. `summary_crc` covers 64 to the padded end (bitmap, descriptors, padding). Both seal paths hash identical ranges, so every summary byte is covered.
- **Slot CRC coverage:** the page CRC covers `vol_table_crc`, which covers all 16 KiB of the table. A flip anywhere in the 20 KiB slot fails `SuperblockSlot::verify`.
- **Stream CRC coverage:** `hdr_crc` covers `wire_len` and `payload_crc`; `verify_payload` requires the exact length.
- **Check order:** magic, then major, then CRC, which is correct because a future major's CRC rules are not assumed.
- **`StripeSummary::parse` alignment:** the three views sit at offsets 0, 64 and 192, so one 8-aligned start aligns all of them. Page-aligned registered buffers and 16-aligned `Vec<u8>` both work. Length is checked before the CRC. No `nblocks` value can cause a panic.
- **`for_stripe` correctness:** `fits()` is monotone and both loops terminate; the upward loop is capped by the `u16` short-circuit and cannot overflow. The result is maximal. Checked by hand: 2.5 MiB gives 637 blocks with a 12288-byte summary; 1 MiB gives 254 blocks with 8192 bytes.
- **Ring order:** `for_commit` plus A-then-B writes keep N-1 intact through a torn A, a torn B or a lost location, even if cp_seq skips a number.
- **`boot_write_active` / `IntentSlot::supersedes`:** the union only adds volumes, and intents are filtered by verify, `agg_id` and `intent_seq > intent_seq_seen`. This matches 7.1, apart from finding 1.
- **`write_summary` count check:** it enforces `nblocks == descriptors.len()` and a maximum of 65535.
- **Basics:** `#![forbid(unsafe_code)]` is present. Every disk and wire struct derives `FromBytes`, `IntoBytes`, `Immutable` and `KnownLayout`. No parse path has a slice or arithmetic operation that can panic.
