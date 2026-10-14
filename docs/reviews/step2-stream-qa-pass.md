# Step 2 review: the otap-stream adversarial QA pass

Worker report, the QA-Fuzz worker's first pass over `otap-stream` and the fuzz targets (2026-10-13): 51 tests, eight of them `bug_*` tests failing on real defects. Reproduced verbatim from the worker's hand-back apart from the harness wrapper and the path encoding. The fixes are in the Step 2 entry of `../08-engineering-log.md`; the second pass is `step2-stream-qa-after-fixes.md`.

---

## Summary
I added `E:\Coding\OpenTAP\crates\otap-stream\tests\adversarial_stream_tests.rs`, 51 tests. 43 pass. The other 8 are named `bug_*`, each has a `// BUG:` comment, and each fails for a real defect in `src/`. I also improved all three fuzz targets in place. Nothing under `crates/*/src`, `stream_tests.rs` or `fuzz/Cargo.toml` was touched.

- **Merge gate:** at `PROPTEST_CASES=100000` the 43 passing tests take 26 s. Proptests use `failure_persistence: None`, so no regression files land in the tree.
- **Failing tests block the old suite by default:** because `adversarial_stream_tests` now fails and runs before `stream_tests`, plain `cargo test -p otap-stream` stops before `stream_tests` runs. With `--no-fail-fast` it runs, 13/13 green.
- **Allocation probe:** to prove the decoder refuses hostile record counts before allocating anything proportional to them, the test binary installs a counting global allocator. That is the only `unsafe` in the file, with SAFETY comments. Tests that allocate 1 MiB or more take a mutex so the probe only sees its own allocations.

## Tests added
**Feeding the decoder**
1. `the_samples_are_valid`: checks that every hand-built body and every sample frame decodes, so the negative tests mean something.
2. `empty_feeds_change_nothing_at_any_point_of_a_frame`: empty feeds at the frame start, one byte in, mid-header, header end and mid-payload change nothing.
3. `one_byte_at_a_time_needed_and_pending_are_exact` (proptest): `needed()` and `pending()` are exact after every byte, and each frame is yielded on its last byte.
4. `a_damaged_stream_decodes_the_same_however_it_arrives` (proptest): random cuts and byte damage give the same frames, CRC failures and fatal error as one whole feed. A decoded frame validates and round-trips.
5. `a_frame_split_in_two_at_every_offset_decodes_once`: covers header offsets 1..63 and every payload offset for every kind. The second feed also carries the next frame.
6. `several_frames_in_one_feed_then_the_rest_of_a_partial_one`
7. `drain_returns_what_is_complete_and_keeps_the_rest`: `drain()` after partial feeds.
8. `a_transport_may_receive_straight_into_the_decoder_buffer`: writes through `buffer()` with `put_slice`; once a header parses, there is room for the whole payload.
9. `a_poisoned_decoder_yields_nothing_more`
10. `header_versions_kinds_and_cp_bounds_through_the_decoder`: a later minor is accepted; other majors, kinds 0/13/255, `cp_seq` above the bound and `base > cp_seq` poison.

**The 8 MiB bound (plain tests)**
11. `a_chunk_of_exactly_the_payload_bound_round_trips`: also checks the payload is reserved after the header, and the vectored `Encoder::header` path.
12. `one_byte_past_the_bound_is_refused_at_the_header_before_any_allocation`: `pending()` stays 64, the decoder is poisoned, capacity stays under 1 MiB (probe), and `raw_len` over the bound and the tight-bound `PayloadTooLarge` case behave the same.
13. `a_decoder_bound_above_the_format_bound_is_clamped`: also covers a bound of 0.

**Empty, truncated, overlong and over-counted bodies**
14. `no_kind_accepts_an_empty_payload`
15. `truncated_bodies_are_too_short_at_every_length`: fixed bodies plus the HelloAck, DeltaBegin and VolEvent record arrays.
16. `bytes_after_every_body_are_too_long`: +1, 7, 8, 16 and 24 bytes after every body.
17. `record_counts_beyond_the_payload_are_refused_before_any_allocation`: `sparse_ranges`, `tags` and `acked` claims of 2, 2^16, 2^24 and u32::MAX, plus VolEvent `name_len`. The largest allocation stays under 4 MiB, and `records()` checks the length before `with_capacity`, so this passes.

**Every enumeration and bound behind valid CRCs**
18. `delta_begin_enumerations_and_bounds`: mode 0/3/4/0x7F/255; increments with nil or equal snapshots; chunks 0, 1, MAX, MAX+1, u32::MAX; nil tag uuid; `acked.vol` at the bound and over it.
19. `vol_event_enumerations_and_bounds`: event 0/5/6/255; class 0/6 on Create and non-zero class on the other events; vol bounds; size and nil-uuid rules; five non-UTF-8 forms; a 255-byte name; name_len 256.
20. `abort_hello_committed_delta_end_and_holes_bounds`: reason 0/7; `max_payload` edges; nil ids; unknown capability bits ignored; `applied_cp_seq` above the bound; DeltaEnd chunks; Landed accepts anything; every Holes record rule, with the bad record alone, first and last.

**Codecs and kinds**
21. `compressed_chunks_pass_through_with_their_raw_len`: both codecs; `stripe_hdr()` and `summary()` refuse them.
22. `every_other_kind_refuses_a_codec`
23. `undefined_codecs_and_flag_bits_poison_at_the_header`
24. `the_header_for_a_held_payload_refuses_another_length`
25. `encoding_an_invalid_frame_appends_nothing`: 14 invalid frames, including an over-bound Holes; the buffer is unchanged afterwards.

**The cursor**
26. `hello_ack_sparse_ranges_must_be_ordered_disjoint_and_in_bounds`: overlapping, unordered, below contig, at contig, past `chunks`, `u32::MAX`, contig equal to chunks with extra ranges.
27. `landing_fields_need_the_landing_flag_and_the_flag_needs_sane_fields`
28. `the_largest_landing_a_peer_can_describe_decodes_and_lands_at_both_ends`: 2^24 chunks.
29. `chunk_indices_at_and_past_the_end`: `chunks - 1`, `chunks`, `u32::MAX`, and bitmap word edges.
30. `a_commit_moves_the_base_for_the_next_delta`
31. `begin_refuses_a_cp_beyond_the_format_bound`
32. `a_refused_begin_leaves_the_partial_alone`
33. `resume_bitmap_from_parts_edges`
34. `cursor_page_adversaries`: `sparse_ranges` 501 and u32::MAX with a valid CRC, a later minor, another major, reserved bytes ignored on disk, unaligned and wrong-size reads, and a restart that re-lands what the page dropped.

**Credits**
35. `credit_arithmetic_saturates`
36. `a_receiver_window_of_exactly_two_gib`
37. `grants_after_a_reset_and_empty_landings`
38. `the_windows_hold_their_invariants_through_resets_and_regrants` (proptest): `outstanding + staged <= capacity`, refused operations change nothing, and the sender's view equals the receiver's `outstanding`.

**Determinism and injectivity (proptests)**
39. `encoding_is_deterministic`
40. `a_change_to_one_field_changes_the_encoding`
41. `distinct_frames_never_share_an_encoding`: independent pairs with `prop_assume!(a != b)`.
42. `damaged_bodies_behind_valid_crcs_are_refused_or_round_trip`
43. `typed_decoding_never_panics_on_an_unverified_header`

**Defect tests (fail)**
44 to 51 are the eight `bug_*` tests below.

## Failing tests: repro and diagnosis
1. **`bug_drain_loses_the_frames_it_decoded_before_a_payload_crc_error`** (medium-high)
   - Repro: encode [Holes, Ping, Ping], flip one payload byte of the first Ping, feed it all, call `drain()`. You get `Err(PayloadCrc)`, and later calls return only the second Ping, yet `frames()` is 2.
   - Cause: `codec.rs:269-275` runs `while let Some(f) = self.next_frame()? { frames.push(f) }`. The `?` drops the local Vec, whose frames were already split out of the buffer.
   - Impact: a lost `Holes` or `VolEvent` is never resent. The cursor tracks chunks only, and the DeltaEnd digest covers chunks, so the replica commits without it.
   - Fix: return the frames collected so far and hold the error for the next call.
2. **`bug_the_encoder_emits_headers_its_own_decoder_refuses`** (medium)
   - Repro: encode a Ping with `FrameMeta { cp_seq: MAX_CP_SEQ+1 }` or `{ cp_seq: 5, base: 6 }`. Both encode; the peer's decoder answers `OutOfRange{cp_seq}` / `BadField{base}` and is poisoned.
   - Cause: `Encoder::header` (`codec.rs:76-97`) never calls `hdr.verify()`, and `Frame::validate` only checks the body.
3. **`bug_decoding_accepts_bodies_the_encoder_never_produces`** (low-medium)
   - 14 cases are accepted. All reserved fields are ignored on decode: Hello `rsvd`, CursorBody `rsvd`, DeltaBegin `rsvd0`/`rsvd1`, VolEvent, DeltaEnd, Landed and Abort `rsvd`, SnapTag/AckedBytes `rsvd`. A Steady DeltaBegin with a `from_snap` or `to_snap` is accepted (`frame.rs:949-956`). HelloAck adjacent ranges 5..7, 7..9 are accepted because `from_parts` allows `start == previous end` (`cursor.rs:112`). The encoder writes SnapTag/AckedBytes `rsvd` as given.
   - 11 of these re-encode to different bytes. That contradicts the `frame.rs` module doc and the fuzz target's own byte-identity assertion.
   - Caveat: `CursorBody` is shared with the on-disk `CursorPage`, where reserved bytes must be ignored (format convention 4). The check belongs on the wire path, not in `CursorBody::validate`; test 34 pins the disk behaviour.
4. **`bug_a_raw_chunk_whose_raw_len_disagrees_with_its_image_is_encoded`** (low)
   - Repro: `Chunk { image: 8 KiB, codec: Raw, raw_len: 0 }` encodes, and the peer decodes `raw_len` 8192.
   - Cause: `validate` (`frame.rs:868-874`) never compares `raw_len` with `image.len()`.
5. **`bug_a_fragmented_landing_makes_the_hello_ack_unencodable`** (low-medium, liveness)
   - Repro: a 2,097,138-chunk delta with every odd chunk landed gives 1,048,569 ranges. Encoding the HelloAck fails with `PayloadTooLarge { len: 8388616, max: 8388608 }`, so the receiver cannot answer `Hello`.
   - Cause: HelloAck carries every range (`frame.rs:680-683, 811-814`), while `to_page` truncates to 500.
   - The credit window makes this unlikely from a well-behaved sender, but a peer that chooses which chunk indices to send can get there.
6. **`bug_a_compressed_chunk_raw_len_escapes_the_stripe_rules`** (low; compression is Phase B)
   - Repro: Lz4/Zstd chunks with `raw_len` 0, 1, 4096, 8191 or 8193, or with an empty payload, pass both decode and encode.
   - Cause: only `raw_len <= 8 MiB` is checked (`frame.rs:872, 913-923`).
7. **`bug_chunk_stripe_hdr_panics_on_a_short_raw_image`** (low)
   - Repro: `Chunk { image: [0;10], codec: Raw, raw_len: 10 }.stripe_hdr()` panics with "range end index 64 out of range for slice of length 10".
   - Cause: `frame.rs:413` slices `&self.image[..STRIPE_HDR_SIZE]` without a length check. It is reachable only through the public fields; `summary()` returns an error for the same chunk.
8. **`bug_some_refusals_name_the_wrong_field_or_contradict_themselves`** (low, diagnostics)
   - A nil `pipe_id` is reported as `field: "node_id"` (`frame.rs:139`).
   - A Holes payload of 0 to 15 bytes gives `BodyTooLong { expected: 0, found: n }` (`frame.rs:1009`).
   - `Frame::decode` given a longer payload than `wire_len` gives `BodyTooShort { needed: 16, have: 24 }` (`frame.rs:907`).
   - `Encoder::header` given a shorter payload gives `BodyTooLong { expected: 16, found: 15 }` (`codec.rs:83`).

## Fuzz targets
**Did the originals panic on their own?** No. There is no out-of-bounds `data[pos]`, no modulo by zero and no `MAX_LBA + 1 - len` underflow, and every `expect` holds (anything that decodes also validates). What they had were these problems:

- **stream:**
  - Its "re-encodes to the same bytes" assertion fires on BUG #3 once libFuzzer solves the two CRCs.
  - Part 1 `return`ed on the first bad header, so part 2 never ran for almost any input of 64 bytes or more.
  - Part 2 built only Ping and Holes.
  - It never varied alignment, and it tolerated a flipped bit that went unnoticed.
- **stripe and superblock:** the zero-copy views and copy paths were only ever tried aligned.

**Changes to `fuzz_stream_frame.rs`**
- **Part 1:** now compares an input-chunked decoder with a whole-input decoder; it also checks `needed() > 0` after `Ok(None)` and that a poisoned decoder repeats its error.
- **Part 2 (new):** seals the input as the body of any kind and codec with valid CRCs, so the typed decoders are reachable without CRC solving. A direct decode must agree with the receive path, and an optional 65-byte frame ahead puts the payload at an odd offset.
- **Decoded frames:** must validate and round-trip; chunk accessors run on them; byte and header identity is checked behind a `const CANONICAL: bool = true`.
- **Part 3:** builds all 12 kinds from an input reader that zero-fills when the input runs out, so it cannot index out of bounds. Refused frames must leave the buffer untouched; valid ones go through `encode`, `encode_into` and `header`. A flipped bit must be detected, and a payload flip must leave the decoder in sync.
- **Part 4 (new):** `StreamHeader` read/parse at offsets 0..7, and `CursorPage` read at offsets 0 and 3; a restored cursor must re-persist and encode as a HelloAck.
- **Cost guards:** sealed HelloAck chunk counts in (2^16, 2^24] are cut down, and the part 4 round trip is skipped above 2^16 chunks.

**Decision for you:** with `CANONICAL = true` the stream target will crash within seconds on any body with non-zero reserved bytes until BUG #3 is fixed. If you'd rather keep nightly fuzzing running before that fix, set it to `false`. If you rule that reserved bytes are ignored on the wire, it should stay `false`.

**Changes to `fuzz_stripe_summary.rs`:** the same bytes are also placed one byte past an 8-byte boundary. Both `parse` paths must refuse them, `StripeHdr::read` must give the same result, and `seal_summary_in_place` must give the same verdict and write the same bytes at both alignments.

**Changes to `fuzz_superblock.rs`:** `Superblock::parse` and `IntentSlot::parse` are compared with their copying reads. At shifts of 1 and 512 bytes every view must be `Misaligned` and every copying read must match the aligned one.

**Smoke test:** a throwaway harness in the scratchpad (not in the repo) ran the three targets about 2.88 million times on random and structured inputs (valid, mutated and resealed streams, bodies, pages, stripe summaries and superblock slots). The only panic was the `CANONICAL` assertion on BUG #3 bodies; with it set to `false` there were none.

## Hazards seen but not tested
1. `Decoder::feed` keeps buffering after poisoning, and the buffer is unbounded in general.
2. `needed()` is 0 between `feed` and `next_frame` when 64 or more bytes are buffered but the header hasn't been parsed yet.
3. `chunk_idx` is documented as 0 for non-chunk frames but is not enforced anywhere; `stream_tests` relies on arbitrary values.
4. These fields are unbounded at the frame level:
   - Landed `idx` and `landed_contig`
   - credits in Hello, CursorBody and Landed
   - VolEvent Create `stripe_bytes`
   - VolEvent `size_blocks` above `MAX_LBA + 1`
   - DeltaBegin/DeltaEnd `bytes` against `chunks`
5. `credits_for` saturating means `reserve(usize::MAX)` succeeds when holding u32::MAX credits.
6. CPU cost of large landings:
   - `from_parts` marks bits one by one, which is O(contig): 2^24 bits per HelloAck or cursor page.
   - Encoding one HelloAck walks the bitmap about 6 times through repeated `to_wire` calls.
7. `ResumeBitmap::new` doesn't check its chunk count: u32::MAX allocates 512 MiB, and 0 counts as complete.
8. `Encoder::encode` allocates the payload length before `validate` runs.
9. A corrupt `Holes`, `VolEvent` or `DeltaBegin` has no resend path; the session layer must abort the delta.
10. `RawFrame::kind()` reports `Abort` for a fabricated undefined kind.
11. The DeltaEnd digest doc ("chunks in index order") disagrees with design section 3.3 ("summary pages").
12. `records()` arithmetic can overflow on 32-bit targets.
13. Zero-copy views pin the whole receive allocation for as long as they live.

## Final lines (verbatim)
```
cargo clippy -p otap-stream --all-targets -j 2 -- -D warnings
    Finished `dev` profile [unoptimized + debuginfo] target(s) in 0.06s
cargo clippy --manifest-path fuzz/Cargo.toml --no-default-features -j 2 -- -D warnings
    Checking otap-fuzz v0.0.0 (E:\Coding\OpenTAP\fuzz)
    Finished `dev` profile [unoptimized + debuginfo] target(s) in 0.15s
rustfmt --edition 2021 --check ... : clean
cargo test -p otap-stream -j 2 -- --test-threads=4
test result: ok. 0 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.00s
test result: FAILED. 43 passed; 8 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.84s
error: test failed, to rerun pass `-p otap-stream --test adversarial_stream_tests`
(with --no-fail-fast, additionally)
test result: ok. 13 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.04s
test result: ok. 0 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.00s
```
The otap-stream clippy line above is a cached result. The earlier uncached pass on the same file content reported `Checking otap-stream v0.1.0 ... Finished ... in 0.80s`.

Files touched:
- E:\Coding\OpenTAP\crates\otap-stream\tests\adversarial_stream_tests.rs
- E:\Coding\OpenTAP\fuzz\fuzz_targets\fuzz_stream_frame.rs
- E:\Coding\OpenTAP\fuzz\fuzz_targets\fuzz_stripe_summary.rs
- E:\Coding\OpenTAP\fuzz\fuzz_targets\fuzz_superblock.rs
