//! The arena's memory.

use std::path::Path;

use otap_control::{check_arena_bytes, ArenaKind, ControlError, ErrorCode};
use otap_sys::{HugeArena, HugePageSize};

/// Opens the arena `kind` of `bytes` bytes: the hugetlbfs file (Linux,
/// re-attached with its contents when it exists) or heap memory.
pub fn open_arena(kind: &ArenaKind, bytes: u64) -> Result<HugeArena, ControlError> {
    check_arena_bytes(bytes).map_err(|m| ControlError::new(ErrorCode::BadRequest, m))?;
    let len = usize::try_from(bytes).map_err(|_| {
        ControlError::new(
            ErrorCode::BadRequest,
            format!("an arena of {bytes} bytes does not fit this host"),
        )
    })?;
    match kind {
        ArenaKind::Heap => HugeArena::heap(len, HugePageSize::TwoMiB).map_err(|e| {
            ControlError::new(
                ErrorCode::Io,
                format!("cannot allocate a {bytes}-byte heap arena: {e}"),
            )
        }),
        ArenaKind::Hugetlb { path } => {
            let path = Path::new(path);
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
            HugeArena::map_file(path, len, HugePageSize::TwoMiB).map_err(|e| {
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
            })
        }
    }
}
