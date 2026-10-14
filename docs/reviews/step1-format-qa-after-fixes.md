# Step 1 review: the otap-format adversarial suite after the fixes

Worker report, the same worker porting its suite to the fixed `otap-format` (2026-10-13): 49 tests, every former `bug_*` test now pinning the fixed behaviour. Reproduced verbatim from the worker's hand-back apart from the harness wrapper and the path encoding.

---

## adversarial_tests.rs ported to the new API: clippy clean, all 49 tests pass

I edited only `E:\Coding\OpenTAP\crates\otap-format\tests\adversarial_tests.rs` (3,554 lines after rustfmt). Nothing else was touched, and the only commands run were the three sanctioned ones, one process at a time.

Results from the final run (I made one small fuzz tweak after the first green run and re-ran all three; both runs were green):
```
cargo clippy -p otap-format --test adversarial_tests -j 2 -- -D warnings
    Checking otap-format v0.1.0 (E:\Coding\OpenTAP\crates\otap-format)
    Finished `dev` profile [unoptimized + debuginfo] target(s) in 0.73s

cargo test -p otap-format --test adversarial_tests -j 2 -- --test-threads=4
test result: ok. 49 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.27s
```
No `// BUG:` or `// TODO` comments remain. Every `bug_*` test has been renamed and now asserts the fixed behaviour. I found no new defects: the src fixes held under every test, including the rewritten fuzz oracle (below), which now checks each parser in both directions. `ZERO_BLOCK_CRC32C` (0x98F94189) matches my independent bitwise CRC of 4 KiB of zeros.

### Former `bug_*` tests, now asserting the fixes
- **`summary_layout_answers_any_size_immediately`** (was `bug_summary_layout_for_huge_stripes_takes_linear_time`): 2 TiB and `usize::MAX & !4095` get `BadField{stripe_bytes}` well within a 2 s deadline. 8 MiB gives 2039 blocks, and the call compiles inside a `const`.
- **`superblock_verify_refuses_an_undefined_or_dirty_durable_log`** (was `bug_superblock_verify_accepts_an_undefined_durable_log_kind`):
  - kinds 2, 0x80 and 0xFF are refused with `BadField{durable_log.kind}`;
  - kind 0 with any one of bytes 1 to 63 set is refused with `BadField{durable_log, 0}`;
  - kind 1 may carry anything;
  - `parse` and `verify` agree.
- **`slot_verify_enforces_the_volume_table_rules`** (was `bug_slot_verify_accepts_an_undefined_nvfail_cause`): one case per table rule, each with its exact error and row index:
  - undefined cause;
  - non-absent row at 4 and at 1023 (with `vol_count` 4);
  - absent row with acked bytes or a cause;
  - nvfail bit without a cause, and a cause without its bit;
  - write-active on an absent row;
  - `acked_bytes` sum mismatch;
  - `seal()` overflow;
  - reserved row bytes are accepted.
- **`hole_records_from_the_wire_are_checked_without_panicking`** (was `bug_hole_record_end_lba_overflows_on_peer_input`): the original repro (lba `u64::MAX-1`, len 16) now gives `end_lba() == None` and an `OutOfRange{lba}` refusal. It also checks the edges at `MAX_LBA` and `MAX_VOL`, and that `len` 0 is refused.
- **`both_summary_builders_refuse_what_parse_refuses`** (was `bug_write_summary_accepts_headers_that_in_place_sealing_refuses`):
  - six bad headers: zeroed, foreign major, kind 4, lseg 0, cp > `MAX_CP_SEQ`, count mismatch;
  - four bad descriptors: reserved kind, vol 1024, birth 0, a hole with a data CRC;
  - both builders return the same exact error, and `write_summary` leaves its buffer untouched.

### Tests whose intent changed
- **`select_sees_pages_only_so_slots_must_be_verified_first`** (pinned the old hazard) is now **`ring_selection_trusts_only_whole_slots_of_this_aggregate_at_their_location`**. It asserts:
  - a torn slot is skipped even though its page alone verifies;
  - `rank` orders the ring A1, B1, B0;
  - the A copy wins a tie (checked by pointer);
  - a newer commit from another aggregate is skipped, and so is a commit read from the wrong location;
  - a blank ring, or a wrong aggregate id, gives `NoValidSlot`.
- **`in_place_sealing_seals_whatever_padding_it_finds`** (pinned the old behaviour) is now **`in_place_sealing_refuses_dirty_padding_and_writes_the_bitmap`**:
  - dirty padding gives `BadField{padding, 352}` and nothing is written;
  - once the padding is zeroed, the derived bitmap overwrites a dirty bitmap zone and the bytes equal `write_summary`'s.
- **`summary_layout_of_256_mib_and_1_gib_stripes`** is now **`summary_layout_refuses_stripes_outside_8_kib_to_8_mib`**: those sizes are refused; 8 MiB gives (2039, 36,864, 0 slack) and 8 KiB gives (1, 4096, 0).
- **`..._up_to_the_cap`** is now **`summary_layout_matches_an_independent_search_over_its_whole_range`**: it compares against the oracle for every stripe from 2 to 2048 blocks and checks refusal outside that range. The u16 cap can no longer be reached.
- **`all_zero_and_all_ones_images_are_refused_by_their_magic`** is now **`all_zero_and_all_ones_images_are_refused`**:
  - the old pin "an all-0xFF `MapPage` validates" is reversed: it is now refused with `BadField{plen, u64::MAX}`, through `MapPage::parse` too;
  - a degenerate ring now gives `Err(NoValidSlot)` rather than `None`;
  - added all-zero and all-0xFF checks for `MapRoot`, `HoleRecord`, `DurableLogHdr`, `StripeDescriptor` and `PageIndexPage`.
- **`write_summary_and_in_place_sealing_agree`** (valid inputs only) is now **`summary_builders_agree_on_every_input`**. For arbitrary headers and descriptor words, both builders return the same result and the same bytes; `write_summary` writes nothing when it refuses; and whatever is accepted also parses.
- **`write_summary_fills_exact_and_offset_regions_like_in_place_sealing`**: n = 0 is no longer a round-trip case. It is asserted to be refused (`BadField{nblocks, 0}`) by both builders and by `parse`.
- **`parsers_never_panic_and_never_lie`**: the oracle is rewritten for the new rules and now runs in both directions.
  - An input is accepted exactly when every documented rule holds: page parity and the CP bound, bits below `vol_count`, durable-log rules, all six table rules, intent slot parity, the stripe header fields, the descriptor rules and derived bitmap, and `raw_len`/`base` on frames.
  - Length, magic, major and CRC refusals must match exactly, including the new `region` strings.
  - When a field rule fails, the error must be `BadField` or `OutOfRange`.
  - The generators now build rule-consistent images, and two thirds of the mutated inputs keep their length (this was the final tweak).
- **`map_root_validate_never_panics`** is now **`map_root_validate_matches_the_documented_bounds`**. It has exact expected errors for vol, lba_count, checkpoint and index_pages.
- **`descriptor_words_unpack_repack_and_validate`** and **`map_addresses_unpack_repack_and_validate`** now use exact oracles for the new rules: vol up to 1023, birth at least 1, hole CRC (`ZERO_BLOCK_CRC32C` for descriptors, 0 for map entries), plen 1 to 8. `new`, `data`, `hole`, `pack` and `pack_addr` are all covered.
- **`other_majors_are_refused_and_newer_ones_stop_the_boot`** (renamed) adds the F7 hard stop. A newer major in any ring slot gives `Err(NewerFormat)`, even with a broken CRC or another aggregate's id; an older major is skipped. The same rule is checked for `boot_write_active`.
- **`stream_reserved_flag_bits_are_refused_even_from_a_later_minor`**: the assertion is unchanged. Its comment now cites the documented wire policy instead of flagging a conflict with convention 4.
- **`later_minors_round_trip_with_reserved_bytes_verbatim`**: the durable-log reserved bytes now sit under kind 1, since kind 0 must be all zero, and the row with reserved bits is inside `vol_count`.

### New tests
- `parse_checks_descriptors_and_the_bitmap_behind_valid_crcs`: each descriptor rule, header rule and bitmap mismatch, behind valid CRCs.
- `parse_expected_refuses_a_stale_stripe_at_a_reused_location`: every `StripeId` field, a stale older-CP image, and that integrity is checked before identity.
- `map_pages_and_indexes_verify_against_their_parents`: `MapPage::parse`/`read`/`verify`, `PageIndexEntry::validate`, `MapRoot::verify_index`, and `RootRef::for_region`/`verify`.
- `frame_builders_refuse_what_verify_would_refuse`: `set_payload` and `set_encoded_payload`.
- `the_spec_constants_this_file_assumes`.
- `hole_records_validate_exactly_the_documented_range` (proptest).

Everything else was adapted to the new API with its intent unchanged: the torn slot, sector, ring, summary and intent tests; alignment; the u16::MAX summary; `summary_bytes` (plus saturation); check order (extended with the new rules); cross-structure magic; short regions; the CRC tests; determinism; and 2- and 3-bit flips.

### One behaviour I pinned that you may want to confirm is intended
A newer-major slot stops the boot even when it carries another aggregate's id. That follows from "checked before anything else", but it means stale slots left by a newer release on a reused disk block the mount unless mkfs wipes all four.

