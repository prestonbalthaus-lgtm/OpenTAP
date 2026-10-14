# Step 1 review: the otap-format adversarial QA pass

Worker report, the first adversarial QA pass over `otap-format` (2026-10-13): 43 tests, five of them `bug_*` tests failing on real defects. Reproduced verbatim from the worker's hand-back apart from the harness wrapper and the path encoding. The fixes are in the Step 1 entry of `../08-engineering-log.md`; the port of this suite to the fixed code is `step1-format-qa-after-fixes.md`.

---

## Opus-QA adversarial pass on otap-format: 43 tests added, 5 fail on purpose for real defects

**File:** `E:\Coding\OpenTAP\crates\otap-format\tests\adversarial_tests.rs` (2,574 lines). I edited nothing under `src/` and did not touch `format_tests.rs`.

**Proptest config:** it starts from `ProptestConfig::default()`, so `PROPTEST_CASES` still drives the merge gate. It also sets `failure_persistence: None` (no regression files) and `max_shrink_iters: 4096`. The oracle is a bitwise reflected CRC-32C (0x82F63B78), plus a table derived from it, written in the test file. The layout offsets and magics are hard-coded from the lib.rs spec rather than read from crate constants.

Result: 38 tests pass and 5 fail. The 5 failures are exactly the `bug_*` tests, one per defect. The fuzz found no panics and no parser that accepted bad input or reported a wrong error. crc-fast agreed with the independent CRC everywhere.

### Defects (failing tests, each with a `// BUG:` comment)

**1. `bug_hole_record_end_lba_overflows_on_peer_input` (highest severity: a peer can crash the receiver).**
- **Repro:** `HoleRecord::new(7, u64::MAX - 1, 16).end_lba()`, or the same 16 bytes read off the wire with `read_from_bytes`.
- **Observed:** `panicked at crates\otap-format\src\stream.rs:293:9: attempt to add with overflow`.
- **Cause:** `end_lba` is `self.lba + self.len as u64`, unchecked. `[profile.release] overflow-checks = true`, so this panics in production too, on bytes the peer controls. The payload CRC only proves the bytes arrived as sent. `HoleRecord` has no `validate()`.
- **Fix:** make `end_lba` checked or saturating, and add a `HoleRecord::validate`: `len >= 1`, vol in range, `lba + len <= MAX_LBA + 1`.

**2. `bug_superblock_verify_accepts_an_undefined_durable_log_kind`.**
- **Repro:** `sb.durable_log.kind = 2` (or 0x80, 0xFF), then `sb.seal()`; `Superblock::parse` returns `Ok`.
- **Observed:** `BUG: Superblock::parse accepts durable_log.kind values [2, 128, 255]; only 0 and 1 are defined`.
- **Cause:** `Superblock::verify` (superblock.rs 411-436) checks magic, major, CRC, `run_state`, `slot` and `vol_count`, but never `durable_log.kind`. Its own doc says "every enumerated field", and lib.rs convention 4 requires defined values.
- Related, not tested: the rule "kind == 0 means every other field is zero" is not enforced either.

**3. `bug_slot_verify_accepts_an_undefined_nvfail_cause`.**
- **Repro:** a slot with `vol_count = 1` and `vols[0].nvfail_cause = 4`, then `seal()`; `SuperblockSlot::read` and `parse` return `Ok`.
- **Observed:** `BUG: SuperblockSlot::read accepts nvfail_cause values [4, 128, 255]; NvCause defines 0 to 3`.
- **Cause:** `SuperblockSlot::verify` (superblock.rs 518-522) is the page check plus the table CRC. Nothing checks the records themselves, which contradicts convention 4.
- **Fix:** reject `NvCause::from_u8(..) == None`, at least for records `0..vol_count`.

**4. `bug_write_summary_accepts_headers_that_in_place_sealing_refuses`.**
- **Repro:** call `write_summary` with `StripeHdr::new_zeroed()` (`nblocks = 2`) and 2 descriptors. It returns `Ok(4096)`, and `StripeSummary::parse` then refuses the result with `BadMagic`.
- **Observed:** `assertion left == right failed: BUG: write_summary returned Ok(4096) for a zeroed header, seal_summary_in_place Err(BadMagic { expected: 1414747215, found: 0 })`. A header with `ver = 0x0300` is accepted the same way, while `seal_summary_in_place` returns `BadVersion`.
- **Cause:** `write_summary` (stripe.rs 432-463) only checks `nblocks`; `seal_summary_in_place` checks magic and major first. An engine that forgot `StripeHdr::new` would write unreadable stripes and only find out at boot.
- **Fix:** run `check_magic` and `check_major` at the top of `write_summary`, before any write.

**5. `bug_summary_layout_for_huge_stripes_takes_linear_time` (low severity).**
- **Repro:** `SummaryLayout::for_stripe(1 << 41)` does not return within 500 ms (`BUG: SummaryLayout::for_stripe(2199023255552) did not return within 500 ms`). `for_stripe(usize::MAX & !4095)` would take about 4.5e15 iterations.
- **Cause:** stripe.rs 339-343 starts from `(stripe_bytes - 192) / 4112` and steps down by one. `fits()` is false for every count above `u16::MAX`, so the loop runs about `stripe_bytes / 4112` times.
- **Fix:** clamp the estimate to `u16::MAX`, a one-line change. Also consider refusing these stripes: a 1 GiB stripe currently gets 65,535 blocks and 804,257,792 bytes (75%) of slack, silently.
- The test leaves a detached worker spinning for a few seconds after its deadline; it dies when the process exits.

### Tests added (43; one line each)

**Torn writes**
1. `torn_slot_mixes_at_every_page_boundary_never_verify`: of all 32 four-KiB page mixes of commits 41/42 (every page differs), only the two whole images verify; every mix is `BadCrc`. The new-page/old-table and old-page/new-table splices report the exact table CRCs.
2. `torn_sectors_inside_the_superblock_page_never_verify`: "first 512 B new, rest old" fails. Across all 256 sector mixes, under both tables, a page verifies only if it is byte-identical to a whole commit, and a slot only if page and table belong to the same commit.
3. `ring_falls_back_to_the_previous_commit_through_any_torn_write`: ring built with `SbSlot::for_commit` for n = 10 and 11. Every page mix of the first write gives N-1 (N only when whole), also with location A or B lost; a torn second write gives N; 256 sector tears give N-1. This goes through the documented parse-then-select path.
4. `select_sees_pages_only_so_slots_must_be_verified_first` (pins current behaviour): over bare pages, `Superblock::select` returns the torn commit N (page N over table N-2); parse-then-select returns N-1.
5. `torn_stripe_summary_pages_never_verify`: 3-page summaries from two CPs; every non-whole page mix fails.
6. `torn_intent_writes_never_verify_and_never_reach_the_boot_union`: 256 sector mixes of one intent over another; an intent verifies only if whole, and `boot_write_active` picks up exactly the whole intent's volumes.

**Alignment**
7. `page_views_need_4096_alignment_and_copies_do_not`: `Superblock` and `IntentSlot` at offsets 1, 8, 512 and 4095 of an 8 KiB aligned buffer give `parse` → `Misaligned{4096}` and `read` → an identical value. Whole slots tested at 0, 1, 8, 512, 4095 and 4096.
8. `headers_view_at_eight_byte_alignment_and_copy_at_any_offset`: `StreamHeader` and `StripeHdr` at every offset 0..=64; `read` always works, `parse` only at multiples of 8.
9. `stripe_summary_needs_eight_byte_alignment_only`: 20 offsets up to 4104; the summary parses at multiples of 8 and returns `Misaligned{8}` otherwise.

**Extremes**
10. `summary_with_u16_max_descriptors_round_trips`: 65,535 descriptors (257 pages) round-trip exactly; one byte short gives `BadLength`; 65,536 descriptors give `BadField{nblocks}` with the buffer untouched.
11. `summary_bytes_matches_the_reference_for_every_block_count`: `summary_bytes` and `descriptor_offset` match an independent formula for every n from 0 to 65,535 (including 0, 1, 244 and 245) and at every exact page boundary.
12. `summary_layout_matches_an_independent_search_up_to_the_cap`: `for_stripe` matches a binary-search oracle for every stripe from 2 to 70,000 blocks, crossing the cap at 65,792 blocks.
13. `summary_layout_of_256_mib_and_1_gib_stripes`: 256 MiB gives 65,280 blocks with zero slack; 1 GiB hits the cap. For both, `fits(n) && !fits(n+1)` holds or the cap is reached; `fits` never overflows; invalid sizes give `BadField`.
14. `bug_summary_layout_for_huge_stripes_takes_linear_time`: defect 5.

**Degenerate and cross-structure images**
15. `all_zero_and_all_ones_images_are_refused_by_their_magic`: every verify, parse and read path returns `BadMagic` with `found` 0 or 0xFFFFFFFF. `select` returns `None`, the boot union ignores them, and `seal_summary_in_place` refuses and writes nothing. An all-0xFF `MapPage` validates (pinned); an all-0xFF `MapRoot` does not.
16. `checks_run_in_the_order_magic_version_crc_fields`: four structures, each fixed one field at a time. The exact error and payload are asserted at each stage, including the stream's kind → flags → codec → wire_len → raw_len order.
17. `every_parser_refuses_every_other_structure_by_magic`: each sealed structure is fed to every other parser and gets `BadMagic` naming both magics.

**Short regions and the two builders**
18. `summary_parse_refuses_short_regions_with_the_length_needed`: regions of 0, 1, 8, 63, exactly 64 and `summary_bytes(n) - 1` bytes. A header claiming 65,535 blocks gets `BadLength` before the CRC is read; a header claiming fewer gets `BadCrc`.
19. `write_summary_fills_exact_and_offset_regions_like_in_place_sealing`: an exact-length region, a sub-slice at offset 4096 of a dirty buffer (tail untouched), and hand layout plus `seal_summary_in_place` are byte-identical.
20. `in_place_sealing_seals_whatever_padding_it_finds` (pins current behaviour): dirty padding verifies, but the bytes differ from `write_summary`'s.
21. `write_summary_and_in_place_sealing_agree` (proptest): the two builders produce identical bytes for random inputs.

**No-panic fuzz**
22. `parsers_never_panic_and_never_lie` (proptest): random inputs of 0 to 32 KiB, plus mutated, resealed, truncated or extended valid images of all six structures, at random alignments, through every parser and the 64 B / 4 KiB / 20 KiB prefixes.
    - No panics.
    - `Ok` implies magic, major, the reference CRC and the guarded fields are right, with bytes returned verbatim.
    - `Err` names the first failing check, carrying the values actually read.
    - The zero-copy view and the copy agree except for `Misaligned`.

**CRC-32C**
23. `reference_crc_and_crate_crc_agree_on_the_rfc_3720_vectors`: includes the iSCSI Read(10) PDU vector (0xD9963A56) and a byte-by-byte `Crc32c`.
24. `trailing_crc_helpers_refuse_records_shorter_than_their_crc`: records of 0 to 3 bytes are refused and left untouched; 4 bytes seal to 0.
25. `crate_crc_matches_the_bitwise_reference` (proptest): 0 to 4096 bytes at start offsets 0 to 63.
26. `crc_digest_matches_the_reference_under_any_chunking` (proptest): any chunking gives the same CRC, and `finalize` does not end the computation.
27. `crc_combine_matches_the_reference` (proptest).
28. `crc_combine_zero_extension_is_linear_and_composes` (proptest): lengths up to 2^62.

**Packed fields**
29. `descriptor_words_unpack_and_repack_exactly` (proptest): random descriptor words unpack and repack exactly.
30. `map_addresses_unpack_repack_and_validate` (proptest): random map addresses round-trip. An address with `lseg == 0` or `plen == 0` gives `BadField{lseg|plen, addr}` and poisons its `MapPage`.
31. `map_root_validate_never_panics` (proptest): checked against an independent page count.

**Determinism**
32. `identical_inputs_build_identical_images`: same inputs give the same bytes, including over clean versus dirty buffers.
33. `any_byte_change_in_a_slot_moves_the_blake3_and_fails_the_crc` (proptest).
34. `any_byte_change_in_a_summary_or_frame_is_caught` (proptest).
35. `two_and_three_bit_errors_in_a_slot_are_always_caught` (proptest).
36. `two_and_three_bit_errors_in_headers_are_always_caught` (proptest).

**Version policy**
37. `later_minors_round_trip_with_reserved_bytes_verbatim`: minor+1 and 0x02FF. Reserved bytes come back verbatim in the page, the volume table, the intent slot, the summary header flags, descriptor reserved bits and the stream frame.
38. `other_majors_are_refused_even_with_a_valid_crc`: exact `BadVersion` for every disk structure and the stream.
39. `stream_reserved_flag_bits_are_refused_even_from_a_later_minor` (pins current behaviour; see item d below).

**Defects:** 40 to 43 are `bug_superblock_verify_accepts_an_undefined_durable_log_kind`, `bug_slot_verify_accepts_an_undefined_nvfail_cause`, `bug_hole_record_end_lba_overflows_on_peer_input` and `bug_write_summary_accepts_headers_that_in_place_sealing_refuses`.

### Final results, verbatim

Clippy, second run (the first run flagged two `clippy::chunks_exact_to_as_chunks` calls in my file; I fixed them with `as_chunks::<16>()`):
```
    Checking otap-format v0.1.0 (E:\Coding\OpenTAP\crates\otap-format)
    Finished `dev` profile [unoptimized + debuginfo] target(s) in 0.98s
```

`cargo fmt --all` was run; the tree was fmt-clean beforehand, so only my file changed.

`cargo test --workspace -j 3 -- --test-threads=6`:
```
test result: ok. 0 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.00s      (unittests src\lib.rs)
test result: FAILED. 38 passed; 5 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.50s   (tests\adversarial_tests.rs)
error: test failed, to rerun pass `--test adversarial_tests`
```
Cargo stops at the first failing test binary, so `format_tests.rs` and the doc-tests did not run in this invocation. Nothing they depend on was changed. Add `--no-fail-fast` to see all of them.

### Hazards and design gaps (no failing test)
- **a. `Superblock::select` sees only pages.** Attack item 1 as written ("select over a four-slot ring where commit N tore returns N-1") does not hold if a caller passes the bare pages: a page-new/table-old tear wins. It only holds if slots are filtered with `SuperblockSlot::parse` first. I recommend a `SuperblockSlot::select`.
- **b. `select` has no `agg_id` check.** A verifying stale slot from an earlier aggregate with a higher epoch wins, so mkfs must wipe all four slots. Nothing checks the `slot` field against the read position or against `cp_seq` parity, so a misdirected write is invisible at this layer.
- **c. Stream `raw_len` is unbounded for Lz4 and Zstd.** `u32::MAX` is accepted, which means a 4 GiB decode buffer per peer frame. Bound it before the wire format freezes.
- **d. Wire strictness versus convention 4.** Flag bits 2-7 are the frame header's only spare bits, and they are refused in every minor, so no v1.x minor can add a flag a v1.0 peer tolerates. Either amend convention 4 for the wire or accept these bits when minor > `STREAM_MINOR`. Attack item 10's "the same for StreamHeader" cannot hold as the code stands.
- **e. `set_payload` with `Raw` accepts `raw_len != payload.len()`.** The builder can emit a header that `verify` refuses (the same class as defect 4).
- **f. `MapEntry` validation is loose.** `plen` is accepted up to 4095 although a 4 KiB block never needs more than 8 sectors. A hole or unmapped entry with a non-zero `crc32c` is accepted.
- **g. `MapRoot::validate` does not bound `lba_count` to `MAX_LBA + 1`.** A root can claim 2^41 blocks that descriptors cannot address.
- **h. `StripeSummary::parse` does not check descriptor volumes.** A descriptor's vol need not be in `vol_bitmap` and may be ≥ `MAX_VOLS`; `nblocks = 0` is accepted.
- **i. Size helpers can panic on huge inputs.** `summary_bytes`, `descriptor_offset`, `block_offset` and `slack_bytes` overflow-panic on absurd `usize` values. Every internal caller bounds them by `u16`; consider taking `u16`.
- **j. No cross-checks in the slot.** Nothing checks `sb.acked_bytes == Σ vols.acked_bytes`, `vol_count` against the present records, or the `nvfail` bitset against the causes. `PageIndexEntry` has no validation.

### Weak or missing in format_tests.rs
- No slot-level torn writes or ring scenarios. `select_picks_newest_verified` uses bare pages, so the most likely tear of a 20 KiB write is never exercised.
- Bit-flip tests are single-bit only and assert `is_err()`. A parser that refuses for the wrong reason, or reports wrong values, passes.
- The CRC is checked only against crc-fast itself: no independent implementation, no Read(10) vector, no unaligned start or byte-by-byte digest, and combine is tested only up to 2 KiB.
- The version test covers only `Superblock` and `StreamHeader`, does not check that reserved bytes survive verbatim, and has no foreign-major-with-valid-CRC case for the other structures. The N/N-1 lane has no image with non-zero reserved bytes.
- The layout tests stop at 16 MiB, have no oracle and never reach the `u16` cap. Alignment is tested only at offset 1 of a `Superblock`.
- Every proptest feeds exact-size, aligned inputs. The 100,000-case gate never sees a wrong-length or misaligned buffer.
- `enumerated_fields_are_checked` omits `durable_log.kind` and `nvfail_cause`, which is how defects 2 and 3 got through. Nothing covers `HoleRecord` or `write_summary` on a bad header.
- There are no golden vectors for `HoleRecord`, `MapRoot` or a later-minor image.

