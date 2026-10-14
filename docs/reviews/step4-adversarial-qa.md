# Step 4 adversarial QA report (worker "Opus-QA-Adversarial", 2026-10-13/14)

Saved by the lead from the worker's final message (the worker could not create files outside the test directories).

## Files created
- crates/otap-map/tests/qa_map.rs: 10 tests, 1 bug_*
- crates/otap-sim/tests/qa_step4.rs: 23 tests, 15 bug_*
- crates/otap-ublk/tests/qa_ublk.rs: 12 tests, 2 bug_*
- crates/otap-geom/tests/qa_geom.rs: 10 tests, 5 bug_*
- crates/otap-backstore/tests/qa_backstore.rs: 5 tests, 1 bug_*

## (a) Defects (D durability, C correctness, R robustness, Doc)
1. D. The boot never checks the data stripes of a fold CP (boot.rs takes the stripes to check from the delta records; a fold writes none). Test: bug_the_boot_never_checks_the_stripes_a_fold_cp_wrote. Fix: take them from the fold pages (entries born at this CP).
2. D. A rewind below a volume's creation CP re-creates it empty with no nvfail. Test: bug_a_rewind_below_a_volumes_creation_serves_it_empty_without_nvfail. Fix: create it held (TornCp).
3. D. A pool boots under a different stripe geometry and the boot commit rewrites the old layout (the superblock records no geometry). Test: bug_a_pool_boots_under_another_stripe_geometry_and_rewrites_it. Fix: record the geometry; refuse a mismatch.
4. D (E19). A stripe that lost a data block under an intact summary is selected as good (boot checks summaries only). Test: bug_a_trusted_opaque_cp_that_lost_a_data_block_is_selected_as_good. Fix: under the trust policy also check the newest CP's data blocks.
5. D/R. A transient read error at boot discards the newest CP permanently (read errors are torn CPs). Test: bug_a_transient_read_error_at_boot_discards_the_newest_cp. Fix: an I/O error fails the boot (retryable).
6. C. A snapshot requested while a fold CP is in flight is lost (finalize clears every pending request). Test: bug_a_snapshot_asked_for_during_an_in_flight_fold_is_lost.
7. C (latent). otap-map: a fold drops the records of a volume created in the same CP. Test: bug_a_fold_drops_the_records_of_a_volume_created_by_the_same_cp.
8. C/R. A volume grown in the configuration is served at the larger size; writes past the root wedge every CP. Test: bug_a_volume_grown_in_the_configuration_is_served_but_never_committed.
9. C. snapshot_diff accepts a base newer than the snapshot and returns an empty delta. Test: bug_snapshot_diff_accepts_a_newer_base_and_reports_no_change.
10. R. Running out of metadata space fails the tick instead of reporting a full pool. Test: bug_running_out_of_metadata_space_fails_the_tick_instead_of_reporting_a_full_pool.
11. R. A pool filled by snapshot metadata can no longer boot (the reserve applies only to data). Test: bug_a_pool_filled_by_snapshot_metadata_can_no_longer_boot.
12. R. A policy fold that does not fit wedges a nearly full pool. Test: bug_a_policy_fold_that_does_not_fit_wedges_a_nearly_full_pool.
13. R. A damaged map page turns a read into an engine failure. Test: bug_a_damaged_map_page_fails_the_tick_instead_of_answering_corrupt.
14. R. A damaged map page wedges every CP and burns the data segments. Test: bug_a_damaged_map_page_wedges_every_consistency_point.
15. R. The boot CP's segment-table region is never released (one segment leaked per boot). Test: bug_the_boot_cps_segment_table_region_is_never_released.
16. R. One damaged sector in an old snapshot's root page makes the pool unbootable. Test: bug_one_damaged_snapshot_root_page_makes_the_pool_unbootable.
17. R. ublk: requests decoded before going offline still reach the engine. Test: bug_requests_decoded_before_going_offline_still_reach_the_engine.
18. R. ublk: a high watermark of 0 starves every request. Test: bug_a_zero_high_watermark_starves_every_request.
19. C. geom: VMware and Hyper-V virtual disks are classified as hardware RAID. Test: bug_hypervisor_virtual_disks_are_classified_as_hardware_raid.
20. R. geom: a logical block size that does not divide 4 KiB is accepted. Test: bug_a_logical_block_that_does_not_divide_4k_is_accepted.
21. R. geom: a striped md array with no chunk size silently gets the 1 MiB default. Test: bug_an_md_parity_array_without_a_chunk_size_falls_back_silently.
22. R. geom: discards switched off by the administrator (discard_max_bytes = 0) are still planned. Test: bug_discards_switched_off_by_the_administrator_are_still_planned.
23. R. Devices with more segments than a u32 holds are silently cut short. Test: bug_a_device_with_more_segments_than_a_u32_is_silently_cut_short.
24. Doc/R. Dead data segments are never freed though docs say they are. Test: bug_dead_data_segments_are_never_freed.
25. D (citation). A volume whose page index exceeds 4 GiB makes the pool unbootable (check_index_fits vs the boot's 4 GiB region limit).
26. R/Doc (citation). FileBackstore::open refuses block devices (metadata().len() is 0).

## (b) Hazards seen but not tested
1. A discard in flight when the daemon is killed can complete after the new process reuses the segment.
2. SegEntry::lap never advances (SegmentTable::free zeroes it).
3. Snapshot ids are reused after a rewind below a snapshot.
4. DiskLayout::stripes_for_pages overflows u32 at 2^20 pages; unused, as are segment_data_bytes and pages_per_stripe.
5. The segment table is written whole at every CP (400 MiB per CP at 100 TiB with 8 MiB segments).
6. A fold rewrites the whole index of each dirty volume; a boot from a fold reads every fold page.
7. MapSet::replay is not atomic and accepts regions above the committed CP or not starting at page 0.
8. The ublk front door drops UBLK_IO_F_FUA and other flags (by design per D2, undocumented).
9. The 8 MiB ublk request size is fixed regardless of the arena.
10. LinuxTransport retires a tag on any fetch error; USER_RECOVERY is requested with no recovery path.
11. boot_protect lasts two commits even under the trust policy.
12. At boot, meta_live double-counts regions shared by the live map and a snapshot.
13. FileBackstore completes a zero-length stripe write as success.
14. A discard completing between a CP's prepare and finalize gets the segment discarded twice and SegmentFreed emitted twice.
15. The superblock's freed_pending lists and SegState::FreedPending are never written.
16. A damaged live index shared by both CPs refuses the boot; no per-volume quarantine.
17. The ASR, AAC and LSI substring markers in otap-geom could misclassify plain disks.

## (c) Commands and results
All with -j 2, one cargo at a time. qa_map 9 pass / 1 fail; qa_geom 5/5; qa_backstore 4/1; qa_ublk 10/2; qa_step4 8/15; rustfmt clean; clippy clean in the worker's files. Clippy -D warnings stops in the lead's code: chunks_exact_to_as_chunks at otap-format/src/records.rs:1059 and otap-map/src/lib.rs:705; without -D also boot.rs:455, engine.rs:2773, engine.rs:2790, meta.rs:282.

## (d) Wrong or misleading documentation
1. Doc 06 §8 and E24 say dead data segments are freed (defect 24).
2. E24 and doc 06 say two segments are reserved for the boot and shutdown commits; only against data (defects 11, 12).
3. E21's "never serves a torn image" contradicted by defects 1 and 4.
4. boot.rs module doc: every stripe the CP's delta names is checked (excludes fold CPs).
5. segments.rs says Step 3 "never frees a segment": stale.
6. otap-ublk/src/device.rs: optimal I/O size is 2^trailing_zeros(stripe), not the stripe.
7. otap-backstore/src/lib.rs says a block device can be opened as a file (defect 26).
8. frontdoor.rs set_offline doc (defect 17).
9. frontdoor_tests a_depth_of_zero_still_serves_one_at_a_time: nothing is served.
10. SegEntry::lap, Superblock::freed_pending, SegState::FreedPending describe unused mechanisms.
11. SnapshotInfo::snap_id "monotonic per aggregate" fails across a rewind.
