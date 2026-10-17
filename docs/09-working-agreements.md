# OpenTAP working agreements

How the implementation work is done. Living document; last updated
2026-10-16. Change it when a rule changes, and record why in
`08-engineering-log.md`.

## 1. Roles

- **The owner** writes the charter and the step briefs, answers the
  decisions in doc 05 Part 9, and accepts each step's report.
- **Fable 5.1, Lead Systems Architect and Engineering Director,** turns a
  brief into the plan, writes or delegates the code, directs the workers,
  reviews everything that comes back, runs the gates, and reports. The
  architect owns coherence: the format and stream crates were written by the
  lead for that reason.
- **"Opus 5.5" workers** are sub-agents with one narrow brief each: an
  independent audit, a crate behind a clear boundary (`otap-sys`), an
  adversarial QA pass over a crate, a fuzz-target review. A worker owns only
  the files its brief names, never the sources it reviews; it reports
  defects, and the lead fixes them. A worker's report is read in full and
  every finding is either fixed and pinned by a test or refused in writing.
  Workers run on Opus 5.5, never on the lead's model (the owner's rule of
  2026-10-15).
- **The thin-orchestrator mode**, when a brief asks for it (Step 6, re-issued
  on 2026-10-16): the lead keeps the plan and the decisions but delegates
  every read, change, test run and checkpoint commit to Opus 5.5 workers
  that return short reports, run one at a time, and each commit their own
  checkpoint on `main` with their decisions written to a notes file
  (`docs/step6/notes/`); no multi-round verifier loops. A final worker, the
  QA gatekeeper, then makes one bounded adversarial review in which it
  fixes what it reproduces (tests first), runs the gates, and writes the
  step's documents from the notes. The rule that a reviewer never fixes
  the sources it reviews is suspended for that worker, and its report
  says so.
- **Personas for design rounds** (Opus-Storage, Opus-Protocols, Opus-Fabrics,
  Opus-Consensus, plus the critics) are kept for any further design round;
  they are not used for implementation steps.

## 2. How a step runs

1. The owner's brief is saved verbatim in `10-phase-c-step-briefs.md`; the
   lead lists what it asks for, what the design already settled, and what it
   leaves open.
2. Open points that block the code are decided by the lead and recorded in
   `07-engineering-decisions.md` before or while the code is written; points
   that need the owner are listed in the report, not guessed silently.
3. Code is written complete: no placeholders, no `TODO`, no stubs that
   compile but do nothing. If part of a brief cannot be done here (a kernel
   path, a lab box), the rest is finished and the gap is named.
4. An independent worker reviews or attacks the result; its findings are
   fixed in the sources and pinned by tests.
5. The gates in section 3 run on the whole workspace.
6. The report to the owner states deliverables, every departure from the
   brief, gate results, and what could not run here.
7. The documents in section 7 are updated.

## 3. Definition of done for a step

Every line below green on the whole workspace, with the PATH prefix and the
etiquette of section 6:

```sh
cargo fmt --all -- --check
cargo fmt --manifest-path fuzz/Cargo.toml -- --check
cargo clippy --workspace --all-targets -j 3 -- -D warnings
cargo clippy --workspace --all-targets --target x86_64-unknown-linux-gnu -j 3 -- -D warnings
cargo clippy --manifest-path fuzz/Cargo.toml --no-default-features -j 3 -- -D warnings
RUSTDOCFLAGS="-D warnings" cargo doc --workspace --no-deps -j 3
cargo test --workspace -j 3 -- --test-threads=6
PROPTEST_CASES=10000 cargo test --workspace -j 3 -- --test-threads=6
```

plus: the worker reports answered in full; the departures recorded; the
documents of section 7 updated; the step reported.

## 4. Code rules

- `#![forbid(unsafe_code)]` everywhere except `otap-sys`. In `otap-sys` every
  `unsafe` block is minimal, Linux-only, and opens with a `// SAFETY:` proof;
  the crate root keeps the inventory current. No `unsafe fn` anywhere.
- Every public item is documented; the gates turn `missing_docs` into an
  error. Module docs state what the module owns and its invariants.
- Layouts are `#[repr(C)]`, padding-free, little-endian, `const`-asserted for
  size and offset, and covered by a CRC-32C. A layout change is a format
  change (section 5).
- Parsers verify before they expose; builders refuse what parsers refuse;
  nothing is clamped silently; nothing proportional to an untrusted count is
  allocated before the count is checked (stream rules S3 and S10).
- Refusals name the field at fault and its value.
- No I/O in `otap-format`, no sockets in `otap-stream`; the engine crates
  take `Backstore`, `FrontDoor`, `Clock` and `Net` as traits so the
  simulator can drive them.
- Dependencies are pinned at the workspace level and bumped only with the
  change that needs them; `Cargo.lock` is the authority.
- Anything a client can observe (a volume, a rollback generation, an
  acknowledgement) exists only once it is durable: the engine serves it
  after the CP that records it commits, never before (E17; two Step 3
  review defects broke this).
- A set the engine keeps as "what the disk guarantees" is narrowed by
  intersection and widened only by what landed; assigning it is a bug
  (E17, twice).
- A reviewer's `bug_*` tests (expected failures with a `// BUG:` comment)
  are ported in place once the defect is fixed: same file, the review's
  scenario kept, the expectation changed to the fixed behaviour, the
  defect's number in the log named in a comment. The review's passing
  tests stay as written; the review's files may take the crate's
  `review_*_tests.rs` names when they are ported. Nothing is deleted to
  make a suite green.

## 5. Changing a format or the wire

1. Bump `FORMAT_MINOR` or `STREAM_MINOR` in `otap-format`.
2. Write the new vectors into a new `tests/vectors/v2.<minor>/` directory
   with `cargo test -p otap-format -- --ignored write_golden_vectors
   --nocapture`, pin the printed hash, and keep the previous directory so the
   N-1 lane keeps decoding it.
3. Record the ruling (an F-ruling in the crate root for a layout, an S-rule
   in `07-engineering-decisions.md` for the wire) and the event in the log.
4. A reader must accept any minor of its own major and ignore reserved bytes
   on disk; on the wire reserved bits stay refused and new features are
   negotiated in `Hello`.

The one exception, regenerating the current vector directory in place, was
used once (2026-10-13, before any release) and is closed.

## 6. Build etiquette on the development PC

The PC is a shared 6-core, 12-thread Windows desktop that has frozen under
parallel builds. Rules:

- Every cargo command is prefixed with
  `export PATH="$HOME/.cargo/bin:$HOME/.cargo/mingw-tools/bin:$PATH"` in
  Git Bash (rustup was installed with `--no-modify-path`; `dlltool.exe` and
  `as.exe` from winlibs live in `mingw-tools`). The Windows section of `BUILDING.md`
  has the detail.
- One cargo process at a time on the machine. The lead builds with `-j 3`;
  a worker with `-j 2`; tests run with `--test-threads=6`. Never two heavy
  jobs at once, and never a stress test in parallel with a build.
- A worker touches only its own files and runs only the test binaries it
  owns; it never runs a workspace-wide gate while the lead may be building.
- A git worktree needs its own `CARGO_TARGET_DIR`. Cargo's artifact hashes
  do not include the checkout's path, so a worktree that shares the main
  tree's target directory overwrites the main tree's artifacts with a
  build of its own sources, and the next build in the main tree links
  them without rebuilding (Step 6A saw it; a `touch` forced the rebuild).
  The rest of Step 6 ran in the main tree, one worker at a time.
- Temporary files, extracted sources and toolchains go to the session
  scratchpad, never into the tree.
- The 10,000-case gate (`PROPTEST_CASES=10000 cargo test --workspace`)
  takes about four hours (116 minutes after Step 4, 180 after Step 5,
  about 241 after Step 6; the figures are in `06`, section 5) and runs in the background with its
  output captured to a file; nothing else uses cargo meanwhile. The
  variable overrides an explicit `cases` in a `proptest!` configuration,
  so no property test is exempt from the gate. The simulator random walks
  (`sim_tests`, `adversarial_sim_tests`, `step4_sim_tests`,
  `step5_sim_tests`, and since Step 6 `step6b_catalogue_sim_tests`,
  `step6b_verify_sim_tests` and `step6c_replication_sim_tests`) are single
  property tests, so more test threads do not shorten them; after one
  build they can be run as separate test processes at once to cut the wall
  time (on a machine with the cores to spare: one at a time on the
  development PC). Their seeds differ per run, so the gate is repeated over
  time rather than treated as final once. Step 6 ran its new suites one at
  a time at 10,000 cases before the full gate, so a failure shows within
  the hour rather than after four.
- Scripted edits of source files (perl, sed) are limited to one-line,
  literal replacements, never with `|` as the perl delimiter when the
  pattern holds a `\|`, and `cargo check` runs right after; multi-line
  edits go through the editor's exact-match replace or a whole-block
  splice from a file (Step 4 mangled three files this way, Step 5 two).
- The owner prefers the work done well over done fast: case counts are
  never trimmed and the review is never skipped to save time.

## 7. Documentation rules

| When | Update |
|---|---|
| A step ends, or "Next" changes | `06-implementation-status.md` (state, gates, next, limits) |
| The code departs from the design or a brief | `07-engineering-decisions.md` (E-entry, F-ruling or S-rule) |
| Anything happens worth remembering: a review, a defect, a gate run, a lesson | `08-engineering-log.md` (dated, append only) |
| A rule in this file changes | this file, with the reason in the log |
| A build or layout fact changes | `BUILDING.md` and the crate root docs |

The code is the authority for what it enforces; these documents are the
authority for why, and for what is not yet code.

## 8. Source control

Since 2026-10-13 the tree is a git repository with `origin` at the
GitHub repository `prestonbalthaus-lgtm/OpenTAP`, branch `main`. Rules:

- One commit per reviewed change with the gates green; the message says
  what changed and why, and ends with the co-author line for the model
  that wrote it.
- Commits are authored by the owner's GitHub identity (the no-reply
  address, so pushes are never rejected for exposing an email).
- Nothing is pushed anywhere but `origin`; the repository's visibility is
  the owner's call.
- `target/`, the fuzz build output and corpora are ignored; `Cargo.lock`
  files are committed; the golden vectors are binary (`.gitattributes`).
- The GitHub CLI lives in `%USERPROFILE%\tools\gh\bin` (on the user PATH)
  and holds the owner's token; `gh auth status` shows the login.
- The tree is licensed under the Apache License 2.0 (`LICENSE`); every
  manifest carries the field, and contributions are accepted under the
  same licence.
- A dependency must carry a permissive licence. Nothing under the GPL,
  LGPL or AGPL enters the linked graph; a crate that offers a copyleft
  licence only as one choice among permissive ones is fine. The check (the
  declared licence of every package in both lockfiles, read from the
  registry sources) is repeated on every dependency bump and its result
  noted in the log.
- The full records of the three design rounds stay in the repository: the
  owner decided on 2026-10-14 that they may be public.
