//! Step 6A: `dbladed` and an arena file made for another size.
//!
//! Before the fix a larger `--arena-bytes` grew the hugetlbfs arena file
//! in `HugeArena::map_file`, the arena's header no longer matched the
//! memory, and the boot recreated the arena: every acknowledged write it
//! still held was destroyed (the engine-level reproduction is
//! `a_larger_arena_over_placed_writes_is_refused_not_recreated` in
//! `otap-sim/tests/step6a_sim_tests.rs`). Now `map_file` refuses a file of
//! another length, and the boot maps it at its own length and judges it
//! with [`check_resize`] before anything changes it. The file path itself
//! runs on Linux only; the judgement, which is all of the decision, runs
//! here on any host over the same arena bytes.

use std::cell::Cell;

use otap_arena::{Arena, RingRef, SLOT_BYTES};
use otap_control::{ArenaKind, ControlError, ErrorCode};
use otap_daemon::{check_resize, open_pool_arena};
use otap_format::checksum::crc32c;
use otap_format::Uuid;
use otap_sys::{HugeArena, HugePageSize};

const ARENA_BYTES: usize = 64 << 20;

fn pool() -> Uuid {
    Uuid::from_bytes([0x61; 16])
}

/// An arena of `pool()` with `placed` acknowledged, uncommitted blocks and
/// some committed ones, as a killed or stopped daemon leaves it.
fn arena_holding(placed: u32) -> HugeArena {
    let mut a = Arena::create(
        HugeArena::anonymous(ARENA_BYTES, HugePageSize::TwoMiB).expect("memory"),
        pool(),
        1,
    )
    .expect("an arena");
    let write = |a: &mut Arena, lba: u64| -> RingRef {
        let ring = a.reserve(1).expect("room");
        let block = vec![lba as u8 | 1; SLOT_BYTES];
        a.run_mut(ring, 1).expect("run").copy_from_slice(&block);
        let seq = a.next_seq();
        a.fill(ring, 0, lba, seq, &[crc32c(&block)]).expect("fill");
        a.place(ring, 1).expect("place");
        ring
    };
    for lba in 0..3 {
        let ring = write(&mut a, 100 + lba);
        a.commit(ring, 1).expect("commit");
    }
    for lba in 0..u64::from(placed) {
        write(&mut a, lba);
    }
    a.into_memory()
}

/// A `clean` answer that records whether it was asked.
fn answer(clean: bool, asked: &Cell<bool>) -> impl FnOnce() -> Result<bool, ControlError> + '_ {
    move || {
        asked.set(true);
        Ok(clean)
    }
}

#[test]
fn an_arena_holding_placed_writes_is_not_resized_even_after_a_clean_stop() {
    let old = arena_holding(2);
    let asked = Cell::new(false);
    let err = check_resize(
        &old,
        &pool(),
        2 * ARENA_BYTES as u64,
        false,
        answer(true, &asked),
    )
    .expect_err("refused");
    assert_eq!(err.code, ErrorCode::BadRequest);
    assert!(
        err.message
            .contains("2 of its blocks are not yet in a consistency point"),
        "{}",
        err.message
    );
    assert!(
        err.message
            .contains(&format!("made for {ARENA_BYTES} bytes")),
        "{}",
        err.message
    );
    assert!(err.message.contains("--discard-arena"), "{}", err.message);
}

#[test]
fn an_empty_arena_of_a_pool_that_did_not_stop_cleanly_is_not_resized() {
    let old = arena_holding(0);
    let asked = Cell::new(false);
    let err = check_resize(
        &old,
        &pool(),
        2 * ARENA_BYTES as u64,
        false,
        answer(false, &asked),
    )
    .expect_err("refused");
    assert!(asked.get(), "the pool was asked");
    assert!(
        err.message.contains("did not shut down cleanly"),
        "{}",
        err.message
    );
}

#[test]
fn an_empty_arena_of_a_cleanly_stopped_pool_is_resized() {
    let old = arena_holding(0);
    let asked = Cell::new(false);
    check_resize(
        &old,
        &pool(),
        2 * ARENA_BYTES as u64,
        false,
        answer(true, &asked),
    )
    .expect("allowed");
    assert!(asked.get());
    // Smaller too: nothing in it to lose.
    check_resize(&old, &pool(), 32 << 20, false, || Ok(true)).expect("allowed");
}

#[test]
fn a_discard_resizes_whatever_the_arena_holds_without_asking() {
    let old = arena_holding(5);
    let asked = Cell::new(false);
    check_resize(
        &old,
        &pool(),
        2 * ARENA_BYTES as u64,
        true,
        answer(true, &asked),
    )
    .expect("the discard is the operator's");
    assert!(!asked.get(), "a discard needs no answer from the pool");
}

#[test]
fn another_pools_arena_is_never_resized_even_with_a_discard() {
    let old = arena_holding(1);
    let other = Uuid::from_bytes([0x62; 16]);
    for discard in [false, true] {
        let err = check_resize(&old, &other, 2 * ARENA_BYTES as u64, discard, || Ok(true))
            .expect_err("refused");
        assert!(
            err.message.contains(&format!("arena of pool {}", pool())),
            "{}",
            err.message
        );
    }
}

#[test]
fn memory_that_holds_no_arena_is_resized_without_asking() {
    let blank = HugeArena::anonymous(ARENA_BYTES, HugePageSize::TwoMiB).expect("memory");
    check_resize(&blank, &pool(), 2 * ARENA_BYTES as u64, false, || {
        panic!("a blank file needs no answer")
    })
    .expect("nothing to keep");
    let mut garbage = HugeArena::anonymous(ARENA_BYTES, HugePageSize::TwoMiB).expect("memory");
    garbage.as_mut_slice()[..64].fill(0xA5);
    check_resize(&garbage, &pool(), 2 * ARENA_BYTES as u64, false, || {
        panic!("no arena, nothing to ask")
    })
    .expect("not an arena");
}

#[test]
fn the_pools_answer_is_passed_through_when_it_fails() {
    let old = arena_holding(0);
    let err = check_resize(&old, &pool(), 2 * ARENA_BYTES as u64, false, || {
        Err(ControlError::new(ErrorCode::Engine, "cannot read the pool"))
    })
    .expect_err("the pool could not say");
    assert_eq!(err.code, ErrorCode::Engine);
}

#[test]
fn a_heap_arena_is_new_memory_and_needs_no_judgement() {
    let m = open_pool_arena(&ArenaKind::Heap, ARENA_BYTES as u64, &pool(), false, || {
        panic!("a heap arena holds nothing")
    })
    .expect("a heap arena");
    assert_eq!(m.len(), ARENA_BYTES);
    assert!(Arena::is_blank(&m));
}

/// Off Linux an arena file cannot be mapped at all; the refusal says so
/// before anything is created.
#[cfg(not(target_os = "linux"))]
#[test]
fn an_arena_file_is_unsupported_off_linux() {
    let path = std::env::temp_dir().join(format!("otap-6a-arena-{}", std::process::id()));
    let err = open_pool_arena(
        &ArenaKind::Hugetlb {
            path: path.display().to_string(),
        },
        ARENA_BYTES as u64,
        &pool(),
        false,
        || Ok(true),
    )
    .expect_err("unsupported");
    assert_eq!(err.code, ErrorCode::Unsupported);
    assert!(!path.exists());
}
