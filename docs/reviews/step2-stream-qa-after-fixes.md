# Step 2 review: the otap-stream adversarial suite after the fixes

Worker report, the QA-Fuzz worker's second pass over the fixed `otap-stream` (2026-10-13): 62 tests, four new `bug_*` tests, the fuzz target repaired. Reproduced verbatim from the worker's hand-back apart from the harness wrapper and the path encoding. The lead's response is in the Step 2 entry of `../08-engineering-log.md`.

---

## Summary
I ported `E:\Coding\OpenTAP\crates\otap-stream\tests\adversarial_stream_tests.rs` to the new rules; it now has 62 tests. 58 pass, including at `PROPTEST_CASES=100000` (27.8 s). The 4 that fail are new `bug_*` tests for things I believe are still wrong in src. The eight old `bug_*` tests are renamed and now pin the fixed behaviour, with their `// BUG:` comments gone. `stream_tests.rs` (13/13) and src were not touched.

The fuzz targets still compiled, but part 3 of `fuzz_stream_frame.rs` had lost most of its coverage, so I fixed it (details below). The stripe and superblock targets are unchanged.

The scratchpad harness ran all three targets 2.88 million times against the new src with zero panics. The `CANONICAL` byte-identity assertion now holds, so it stays `true`.

## Final lines (verbatim)
```
cargo clippy -p otap-stream --all-targets -j 2 -- -D warnings
    Checking otap-stream v0.1.0 (E:\Coding\OpenTAP\crates\otap-stream)
    Finished `dev` profile [unoptimized + debuginfo] target(s) in 0.38s
cargo test -p otap-stream -j 2 -- --test-threads=4
test result: ok. 0 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.00s
test result: FAILED. 58 passed; 4 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.84s
error: test failed, to rerun pass `-p otap-stream --test adversarial_stream_tests`
  (with --no-fail-fast it goes on to stream_tests and the doc tests:)
test result: ok. 13 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.04s
test result: ok. 0 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.00s
cargo clippy --manifest-path fuzz/Cargo.toml --no-default-features -j 2 -- -D warnings
    Checking otap-fuzz v0.0.0 (E:\Coding\OpenTAP\fuzz)
    Finished `dev` profile [unoptimized + debuginfo] target(s) in 0.19s
rustfmt --edition 2021 --check (test file + 3 targets): clean
```

## Tests whose intent changed
**Old bug tests, renamed, now pinning the fixed behaviour**
- `drain_returns_the_frames_before_an_error_and_reports_it_next`: drain gives `Ok([Holes])`, then `Err(PayloadCrc)`, then `Ok([Ping])`. `next_frame` sees a held error first, and a poison after a good frame repeats.
- `the_encoder_refuses_headers_its_decoder_would_refuse`: exact `Format` errors from `encode`, `encode_into` and `header`, nothing appended; `cp_seq == base == MAX_CP_SEQ` is still accepted.
- `decoding_refuses_bodies_the_encoder_never_produces`: 13 cases, each with its exact `BadBody` field and value; the encoder refuses `tag.rsvd`.
- `a_raw_chunk_carries_its_image_length`
- `a_compressed_chunk_must_decode_to_a_stripe`
- `chunk_stripe_hdr_refuses_a_short_raw_image`: `BodyTooShort { 64, 10 }`.
- `refusals_name_the_field_at_fault_and_agree_with_themselves`
- `a_fragmented_landing_still_fits_a_hello_ack`: the old 1,048,569-range repro. The HelloAck now carries 4096 ranges, the peer learns a subset of what landed, and resuming completes the delta.

**Tests that pinned old behaviour**
- `hello_ack_sparse_ranges_must_be_canonical_and_in_bounds` (renamed): a range starting at `landed_contig` now gives `sparse` instead of `landed_contig`. A range 20..21 on a full landing gives `sparse` instead of `ChunkOutOfRange`. Adjacent ranges are refused.
- `resume_bitmap_from_parts_edges`: same changes. The `landed_contig` error is no longer reachable from `from_parts`. I added a check that a rebuilt bitmap equals one marked directly.
- `record_counts_beyond_the_payload_are_refused_before_any_allocation`: HelloAck claims above 4096 now give `BadBody { sparse_ranges }`. Claims up to 4096 still give `BodyTooShort`, still with no allocation proportional to the claim.
- The old combined bounds test is split into:
  - `abort_reasons_and_chunk_indices`
  - `hello_bounds`
  - `committed_delta_end_and_landed_bounds`
  - `hole_records_are_validated_one_by_one`

  Landed at `u32::MAX` is now refused (it used to be pinned as accepted). A non-Corrupt Abort needs `idx` 0. Chunks equal to `MAX_DELTA_CHUNKS` need bytes in range. `pipe_id` is checked by its exact label. The credit bounds are covered.
- `delta_begin_enumerations_bounds_and_canonical_form` and `vol_event_enumerations_bounds_and_per_event_fields` (both renamed) add the bytes bounds, reserved bytes, steady deltas with snapshots, and the per-event field rules.
- `truncated_bodies_...`: Holes of 1–15 bytes gives `BodyTooShort { 16 }`; 17–31 bytes gives `BodyTooLong { 16 }`.
- `no_kind_accepts_...`: an empty Holes gives `BodyTooShort { 16, 0 }`.
- `the_header_for_a_held_payload...`: asserts the exact short/long errors.
- `credit_arithmetic_saturates_and_windows_cap_at_max_credits` (renamed): the sender caps at `MAX_CREDITS`, `reserve(usize::MAX)` is refused, and `Cursor::new` caps.
- `a_poisoned_decoder_yields_nothing_more_and_drops_further_input` (renamed)
- `chunk_indices_...`: `ResumeBitmap::new` now returns a `Result`.
- `cursor_page_adversaries`: page credits over the bound are refused. A page's reserved bytes are ignored on disk but refused when sent in a HelloAck.
- `damaged_bodies_behind_valid_crcs_are_refused_or_canonical` (renamed), the damaged-stream test, and the typed-decode-never-panics test now also assert that re-encoding is byte-identical and that no refusal contradicts itself.
- Strategies follow the new rules: `meta_for` for the `chunk_idx` rule, `delta_bytes`, credit and index bounds, and stripe-sized Create geometry.

**New tests**
- `chunk_idx_is_zero_off_chunks_and_bounded_on_them`
- `a_raw_frame_names_its_kind_unless_built_by_hand`
- `hello_ack_refuses_more_ranges_than_the_wire_carries`
- `the_word_wise_bitmap_agrees_with_a_bit_model` (proptest against a plain bit vector, covering word edges and whole-word runs)

## Still wrong in src
**The four failing tests**
1. **`bug_needed_hides_an_error_that_drain_holds_back`** (low).
   - What happens: after `drain()` holds back a payload CRC failure, `needed()` reports 64 while `next_frame()` would report the error at once.
   - Why it matters: a transport that reads `needed()` bytes before calling `next_frame` sits on the `Abort(Corrupt)` it owes. If the sender is idle waiting for that answer (say the corrupt frame was DeltaEnd), that lasts until the next Ping.
   - Fix: `needed()` returns 0 when an error is held.
2. **`bug_a_hello_ack_cursor_over_the_credit_bound_is_sent_with_other_credits`** (low).
   - What happens: `Cursor.credits` is a public field, and `Cursor::to_wire` clamps it to `MAX_CREDITS`. `validate` only checks the clamped form, so credits of 524289 and of u32::MAX both encode, to identical bytes, and the peer reads 524288.
   - Why it matters: Hello and Landed refuse over-bound credits. This is the same class as the old `raw_len` bug.
   - Fix: refuse in `validate` instead of clamping.
3. **`bug_vol_event_refusals_name_the_wrong_field`** (low, diagnostics). `VolEvent::validate` names the wrong field in six cases, mostly with value 0:
   - a Resize retention is reported as `stripe_bytes`;
   - a name on Resize or Delete is reported as `uuid`;
   - a Delete stripe size or retention is reported as `size_blocks`;
   - a SnapTag stripe size is reported as `size_blocks`.
4. **`bug_encode_allocates_before_it_refuses_the_header`** (nit). `encode` checks `cp_seq` and `base` only at `hdr.verify()`, after `with_capacity(64 + payload)`. Refusing an 8 MiB chunk under a bad `cp_seq` first allocates 8,388,672 bytes, which contradicts the doc ("Nothing is allocated for a frame that is refused"). Fix: check the context before allocating.

**Noted, no test**
- `StreamError::Poisoned` is never constructed any more, since a poisoned decoder repeats its original error. Either remove the variant or return it.
- **Needs a decision before Phase B compression:** `DeltaEnd.bytes` is documented as "payload bytes sent", but `check_delta_bytes` bounds it by `chunks` × 8 KiB..8 MiB of stripe images. With compression the bytes sent can fall below that, so a correct DeltaEnd for a compressed delta would be refused. Settle whether it counts raw or wire bytes.
- `Cursor::begin` allocates the new bitmap (up to 2 MiB) before it knows the outcome, so a Resumed begin or a chunk-count mismatch allocates and then throws it away.

## Fuzz target changes (`fuzz_stream_frame.rs`)
- **`chunk_idx` regression fixed:** part 3 encoded all 13 built frames under one input-derived `chunk_idx`. Under the new rule nearly every non-chunk frame was refused, so their round trip never ran. Each frame is now fitted with `meta_for`, and about 1 input in 8 keeps the raw context so the refusal is still fuzzed.
- **Builders:** they now make mostly valid frames under the new bounds (credits, delta bytes, Landed/Abort indices, stripe-sized `raw_len`, per-event VolEvent fields). An `Input::raw()` escape takes about 1 value in 8 as it comes, to exercise the refusals.
- **Constants:** the `CANONICAL` doc no longer refers to the bug. `MAX_FUZZ_CHUNKS` goes from 2^16 to 2^20, because the bitmap work is now word-wise.

Files touched:
- E:\Coding\OpenTAP\crates\otap-stream\tests\adversarial_stream_tests.rs
- E:\Coding\OpenTAP\fuzz\fuzz_targets\fuzz_stream_frame.rs
