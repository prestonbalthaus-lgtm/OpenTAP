# Step 2 hand-back: otap-sys

Worker report from the Opus-Sys-Kernel worker that wrote `otap-sys` in Step 2 (2026-10-13): its unsafe inventory, its departures from the brief and the kernel facts it asked the lead to confirm (confirmed against the saved 6.12 sources; see the Step 2 entry of `../08-engineering-log.md` and E08 in `../07-engineering-decisions.md`). Reproduced verbatim apart from the harness wrapper and the path encoding.

---

## otap-sys, Step 2: done. All six gates pass on the final files.

I changed nothing outside `crates/otap-sys/` and added no dependencies; Cargo.lock and Cargo.toml timestamps are unchanged.

### Files (all under E:\Coding\OpenTAP\crates\otap-sys\)
- `src/lib.rs`: crate docs (what lives here and why, the two-platform split, the unsafe policy, a numbered unsafe inventory, a doctest), the required `#![deny]`/`#![warn]` lines, a 64-bit-only `compile_error!`, the modules and re-exports.
- `src/hugetlb.rs`:
  - `HugePageSize { TwoMiB, OneGiB }` with `bytes()` and `page_bits()` (21 and 30).
  - `HugeArena` with `map_file`, `anonymous`, `as_slice`, `as_mut_slice`, `as_ptr`, `as_mut_ptr`, `len`, `is_empty`, `page_size`, `lock`, `exclude_from_core_dumps`.
  - Linux is built on memmap2. Elsewhere `anonymous` is an over-allocated, page-aligned heap buffer, `lock` and `exclude_from_core_dumps` return Ok, and `map_file` returns `Unsupported`.
- `src/ublk.rs`:
  - Every constant in ublk_cmd.h, each cited by header line, and `const fn ioc` (`_IOC`).
  - `checked_user_copy_offset`, `user_copy_offset` (panics out of range), and its inverse `split_user_copy_offset`.
  - The 9 structures as repr(C) zerocopy views, with const size and offset asserts.
  - `CtrlCommand` builders: ADD/DEL/START/STOP_DEV, SET/GET_PARAMS, GET_FEATURES, plus GET_DEV_INFO and START/END_USER_RECOVERY. `cmd_bytes()` returns the 80-byte area.
  - `IoCommand::fetch` and `IoCommand::commit_and_fetch`, which always set `addr = 0`.
  - Linux only: `UblkControl` (O_RDWR|O_CLOEXEC) and `UblkChar` (O_RDWR|O_NONBLOCK|O_CLOEXEC), each with `AsFd`, `from_fd` and `command_sqe` (UringCmd80 and UringCmd16 respectively).
- `src/uring.rs`:
  - The full `IORING_OP_*` table of 6.12 (0 to 57, LAST = 58), each cited by line and cross-checked against io-uring's `CODE` values under cfg(linux); `opcode_name`.
  - `FixedBufferLayout`, a pure helper on every host.
  - Linux only: `RegisteredBuffers<'a>` (`new(ring, &arena)`, `layout`, `clone_to`, unregisters on Drop), `clone_buffers(dst, src)`, and `user_copy_read_sqe(&UblkChar, *mut u8, len, offset)`, a plain READ with no flags.
  - Fallback: `RegisteredBuffers::new(&arena)` returns `Unsupported`.
- `src/probe.rs`: `OpcodeSet` (256-bit set), `Allowlist` (`from_parts`, `supports_opcode`, `has_ublk_feature`, `requires(&[u8])`, `requires_ublk_features`, `check_required`), `MissingCapability` (Display, Error, `From` for `io::Error`), the `REQUIRED_OPCODES` and `REQUIRED_UBLK_FEATURES` constants, and on Linux `Allowlist::probe`.
- `tests/sys_tests.rs`: 33 host-independent tests:
  - every constant against the headers, and the ioctl encodings;
  - hand-computed user-copy offsets plus 4 proptests (round trip through an independent copy of the driver's decode, injectivity and order, exact range refusal, inverse);
  - builder bytes against the C layout;
  - allowlist checks and messages;
  - fixed-buffer layout (with a proptest);
  - arena validation, and the fallback arena's alignment, zeroing, read/write and stable address.
  - A `linux_sqes` module builds SQEs on `/dev/null` descriptors. It is only type-checked here.

### Unsafe inventory (5 blocks, all Linux-only; the fallback has none)
1. `hugetlb.rs:323`, `MmapOptions::map_mut`. statfs/fstatfs prove the file is on hugetlbfs (which has no write path) with the right page size. We sized it ourselves and never shrink it. An exclusive non-blocking `flock` is held for the mapping's life, so this is the only view; other OpenTAP mappers also take the lock first.
2. `uring.rs:382`, `slice::from_raw_parts`. It reads the `IoSlice`s as `libc::iovec`. libc is not a direct dependency, so the element type is inferred from `register_buffers`. std guarantees the ABI match, and a const assert checks size and alignment.
3. `uring.rs:398`, `Submitter::register_buffers`. The guard borrows the arena and the ring for its life and unregisters on Drop. If the table outlives the guard (`mem::forget`, or the kernel refusing to unregister), the kernel still only writes the pages it pinned, and only for pushed fixed-buffer operations.
4. `probe.rs:349`, `SubmissionQueue::push` of GET_FEATURES. The 8-byte buffer from `Box::into_raw` is freed only after its CQE is reaped or when the SQE never left the ring; if neither can be established it is leaked.
5. `probe.rs:392`, `Box::from_raw` of that buffer, once the kernel is done with it.

### Derived values
- `sizeof(struct ublk_params)` = **112**. The fields take 108 bytes at offsets 0, 4, 8, 40, 60 and 76; `basic` contains u64s, so the struct is 8-aligned and gets 4 bytes of tail padding. That padding is the named field `tail_pad: [u8; 4]` at offset 108. This is asserted, and also checked against a repr(C) mirror without the padding.
- `UBLK_U_CMD_START_DEV` = **0xC020_7506** and `UBLK_U_IO_FETCH_REQ` = **0xC010_7520**, both asserted in a const block along with the rest of the table.

### Deviations from the brief, and why
- **`map_file` does not pass a MAP_HUGE_\* flag.** memmap2's `huge()` only applies to anonymous maps, and for a hugetlbfs file the kernel takes the page size from the mount anyway. Instead I check HUGETLBFS_MAGIC and `f_bsize` on both the directory and the file. It also refuses to shrink an existing larger file, since that would discard arena contents.
- **`Allowlist::probe` takes `&IoUring<S, C>` (generic) and sends GET_FEATURES through a private two-entry ring with 128-byte SQEs.** The driver only accepts control commands on 128-byte SQEs, and a reactor's ring must not have its completions consumed. As a result no `&mut` ring is needed.
- **`RegisteredBuffers` takes an arena only, not regions.** It splits the arena into buffers of at most 1 GiB, because the kernel refuses larger fixed buffers, and `FixedBufferLayout` maps an offset to a `buf_index`. The fallback constructor has no ring parameter, because there is no io_uring type on that host.
- **`user_copy_read_sqe` takes `&UblkChar` rather than a raw fd,** so the O_NONBLOCK flag is guaranteed.
- **`UblksrvIoDesc::flags()` returns `op_flags & !0xff`, not C's `op_flags >> 8`.** The `UBLK_IO_F_*` masks are defined on the unshifted word, so this is the form that can be tested against them. It is documented.

### Things I could not verify here
- **Nothing ran against a real kernel:** `map_file`, the hugetlb `anonymous` path, buffer registration, cloning and the probe are compile- and clippy-checked only.
- **The iovec layout assert is not evaluated by `cargo check`.** It is checked after monomorphisation, so it only fires in a real Linux build.
- **These kernel facts come from my reading of the driver and io_uring source, not from the supplied headers. Please confirm them against the 6.12 tree:**
  - the 1 GiB limit per fixed buffer (`io_buffer_validate`) and the 16384-buffer table limit;
  - the requirement for 128-byte SQEs on ublk control commands;
  - HUGETLBFS_MAGIC = 0x958458f6;
  - mlock on a hugetlb mapping does not lock it but still checks RLIMIT_MEMLOCK;
  - the EBUSY/ENXIO rules for cloning a buffer table;
  - ENODEV = 19;
  - 6.12 not granting zero copy.

### Final result lines (verbatim)
- rustfmt `--check`: clean (my echo: `rustfmt: clean`)
- `cargo check` (host): `Finished `dev` profile [unoptimized + debuginfo] target(s) in 0.56s`
- `cargo check --target x86_64-unknown-linux-gnu`: `Finished `dev` profile [unoptimized + debuginfo] target(s) in 0.63s`
- `cargo clippy -- -D warnings` (host): `Finished `dev` profile [unoptimized + debuginfo] target(s) in 0.69s`
- `cargo clippy --target x86_64-unknown-linux-gnu -- -D warnings`: `Finished `dev` profile [unoptimized + debuginfo] target(s) in 0.66s`
- `cargo test` (host only; the Linux target has no linker):
  - unit tests: `test result: ok. 0 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.00s`
  - sys_tests: `test result: ok. 33 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.40s`
  - doctests: `test result: ok. 1 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.33s`
