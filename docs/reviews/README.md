# Review reports

The independent reviews behind the implementation steps, as the worker
agents handed them back: verbatim apart from the harness wrapper around
each hand-back and the encoding of Windows paths. The lead's response to
each report (what was fixed, what was accepted as it stood, and why) is in
the step's entry of `../08-engineering-log.md`, and the departures from the
design they caused are in `../07-engineering-decisions.md`. The tests the
reviewers wrote live with the crates they test (`adversarial_*_tests.rs`,
`review_*_tests.rs` and `qa_*_tests.rs` under `crates/*/tests/`).

| File | Step | What it is |
|---|---|---|
| `step1-format-audit.md` | 1 | a read-only audit of `otap-format`: one blocker, fourteen findings |
| `step1-format-qa-pass.md` | 1 | the adversarial QA pass over `otap-format`: 43 tests, five defects |
| `step1-format-qa-after-fixes.md` | 1 | the same suite ported to the fixed code: 49 tests |
| `step2-otap-sys-handback.md` | 2 | the hand-back of `otap-sys` from the worker that wrote it, with its unsafe inventory |
| `step2-stream-qa-pass.md` | 2 | the adversarial QA pass over `otap-stream` and the fuzz targets: 51 tests, eight defects |
| `step2-stream-qa-after-fixes.md` | 2 | the second pass over the fixed stream: 62 tests, four more defects |
| `step4-adversarial-qa.md` | 4 | the adversarial QA report across map, simulator, ublk, geom and backstore: 24 reproduced and 2 cited defects |
| `2026-10-14-step5-qa-adversarial.md` | 5 | the adversarial QA report across snapshot deletion, the cleaner, the reserve, format 2.3 and the daemon: thirteen findings: twelve defects reproduced by fifteen tests and one cited item that proved not to be a defect (E39), twelve hazards; its 32 tests were ported in place as the `review_*` files of `otap-sim`, `otap-daemon` and `otap-control` |

The Step 3 review (the "QA-Crash" worker, 23 defects) was not saved as a
file; its findings and their fixes are listed in the Step 3 entry of
`../08-engineering-log.md`.
