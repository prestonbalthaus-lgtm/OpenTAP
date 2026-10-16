//! The arena's memory.
//!
//! A hugetlbfs arena file outlives the daemon, and its length is the
//! arena's geometry: a file of another length than the one asked for holds
//! an arena that a resize would recreate, destroying every acknowledged
//! write it still holds (the Step 6 audit's arena-size defect). So the pool
//! boot never resizes such a file blindly: [`open_pool_arena`] maps it at
//! its own length first, reads the arena with [`otap_arena::Arena::survey`],
//! and resizes it only under [`otap_cp::check_arena_resize`] (nothing in it
//! may be an acknowledged write and the pool shut down cleanly) or when the
//! operator discards it (`dbladed --discard-arena`). Another pool's arena is
//! never resized.

use std::path::Path;

use otap_arena::{Arena, ArenaError};
use otap_control::{check_arena_bytes, ArenaKind, ControlError, ErrorCode};
use otap_format::Uuid;
use otap_sys::{HugeArena, HugePageSize};

/// Opens the arena `kind` of `bytes` bytes: the hugetlbfs file (Linux,
/// re-attached with its contents when it exists; a file of another length
/// is refused, see [`open_pool_arena`]) or heap memory.
pub fn open_arena(kind: &ArenaKind, bytes: u64) -> Result<HugeArena, ControlError> {
    check_arena_bytes(bytes).map_err(|m| ControlError::new(ErrorCode::BadRequest, m))?;
    let len = arena_len(bytes)?;
    match kind {
        ArenaKind::Heap => HugeArena::heap(len, HugePageSize::TwoMiB).map_err(|e| {
            ControlError::new(
                ErrorCode::Io,
                format!("cannot allocate a {bytes}-byte heap arena: {e}"),
            )
        }),
        ArenaKind::Hugetlb { path } => {
            let path = Path::new(path);
            check_arena_dir(path)?;
            HugeArena::map_file(path, len, HugePageSize::TwoMiB)
                .map_err(|e| map_error(path, bytes, &e))
        }
    }
}

/// Opens the arena of pool `agg_id` for a boot: [`open_arena`], except that
/// an arena file of another length than `bytes` is first mapped at its own
/// length and judged by [`check_resize`] (with `clean`, asked only then,
/// saying whether the pool's selected CP is a clean shutdown), and resized
/// to `bytes` only when that allows it. The old arena is never changed
/// before the judgement.
pub fn open_pool_arena(
    kind: &ArenaKind,
    bytes: u64,
    agg_id: &Uuid,
    discard: bool,
    clean: impl FnOnce() -> Result<bool, ControlError>,
) -> Result<HugeArena, ControlError> {
    let ArenaKind::Hugetlb { path } = kind else {
        // A heap arena is new memory: nothing in it to lose.
        return open_arena(kind, bytes);
    };
    let path = Path::new(path);
    let have = match std::fs::metadata(path) {
        Ok(meta) => meta.len(),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => 0,
        Err(e) => {
            return Err(ControlError::new(
                ErrorCode::Io,
                format!("cannot read the arena file {}: {e}", path.display()),
            ))
        }
    };
    if have == 0 || have == bytes {
        return open_arena(kind, bytes);
    }
    check_arena_bytes(bytes).map_err(|m| ControlError::new(ErrorCode::BadRequest, m))?;
    check_arena_dir(path)?;
    // The arena at the length it was made for, read before anything
    // changes it; the mapping (and the file's lock) is released before the
    // resize takes the lock again.
    {
        let old = HugeArena::map_file(path, arena_len(have)?, HugePageSize::TwoMiB)
            .map_err(|e| map_error(path, have, &e))?;
        check_resize(&old, agg_id, bytes, discard, clean).map_err(|e| {
            ControlError::new(
                e.code,
                format!(
                    "the arena file {} holds {have} bytes, not {bytes}: {}",
                    path.display(),
                    e.message
                ),
            )
        })?;
    }
    HugeArena::map_file_resizing(path, arena_len(bytes)?, HugePageSize::TwoMiB)
        .map_err(|e| map_error(path, bytes, &e))
}

/// Whether the arena memory `old`, mapped at its own length, may be
/// recreated at `bytes` bytes for pool `agg_id`:
///
/// - another pool's arena, never (whatever `discard` says: it is the wrong
///   file, not this pool's to destroy);
/// - memory that holds no arena (blank, or a header that does not verify)
///   holds nothing a boot could replay: yes;
/// - this pool's arena: under [`otap_cp::check_arena_resize`], with `clean`
///   asked only here, or when `discard`.
///
/// # Errors
/// The refusal, naming what may be lost and the way out.
pub fn check_resize(
    old: &HugeArena,
    agg_id: &Uuid,
    bytes: u64,
    discard: bool,
    clean: impl FnOnce() -> Result<bool, ControlError>,
) -> Result<(), ControlError> {
    let survey = match Arena::survey(old, agg_id) {
        Ok(survey) => Some(survey),
        Err(ArenaError::WrongAggregate { found }) => {
            return Err(ControlError::new(
                ErrorCode::BadRequest,
                format!(
                    "it holds the arena of pool {found}, not of pool {agg_id}; give this pool \
                     its own --arena-file"
                ),
            ))
        }
        // A header that does not verify, or a blank one: no arena whose
        // writes a boot could replay.
        Err(ArenaError::BadMagic { .. } | ArenaError::BadVersion { .. } | ArenaError::BadCrc) => {
            return Ok(())
        }
        // This pool's arena under a geometry that cannot be read: what it
        // holds cannot be told.
        Err(_) => None,
    };
    let clean = if discard { false } else { clean()? };
    otap_cp::check_arena_resize(survey.as_ref(), clean, bytes, discard).map_err(|e| {
        ControlError::new(
            ErrorCode::BadRequest,
            format!(
                "{e} (dbladed --discard-arena recreates it, destroying those writes and holding \
                 their volumes under nvfail)"
            ),
        )
    })
}

fn arena_len(bytes: u64) -> Result<usize, ControlError> {
    usize::try_from(bytes).map_err(|_| {
        ControlError::new(
            ErrorCode::BadRequest,
            format!("an arena of {bytes} bytes does not fit this host"),
        )
    })
}

fn check_arena_dir(path: &Path) -> Result<(), ControlError> {
    if let Some(dir) = path.parent() {
        if !dir.as_os_str().is_empty() && !dir.exists() {
            return Err(ControlError::new(
                ErrorCode::Io,
                format!(
                    "the arena directory {} does not exist; mount hugetlbfs there \
                     (pagesize=2M) or use a heap arena",
                    dir.display()
                ),
            ));
        }
    }
    Ok(())
}

fn map_error(path: &Path, bytes: u64, e: &std::io::Error) -> ControlError {
    let code = if e.kind() == std::io::ErrorKind::Unsupported {
        ErrorCode::Unsupported
    } else {
        ErrorCode::Io
    };
    ControlError::new(
        code,
        format!(
            "cannot map the arena file {}: {e} (a hugetlbfs mount of 2 MiB pages \
             with {bytes} bytes reserved is needed; --heap-arena needs none)",
            path.display()
        ),
    )
}
