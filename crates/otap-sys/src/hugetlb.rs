//! Huge-page arenas: the memory of the write cache (round 3, section 1.3).
//!
//! The production arena is a file on a hugetlbfs mount
//! (`run-opentap-arena.mount`, `pagesize=2M`, section 6.5) whose pages the
//! kernel reserved at boot (`hugepagesz=2M hugepages=<arena>`). The contents
//! belong to the file, not to the process, so after `kill -9` the restarted
//! daemon maps the same file again and finds every acknowledged write that
//! was not yet flushed; no holder process exists. [`HugeArena::map_file`] is
//! that re-attachable mapping and [`HugeArena::anonymous`] a private one.
//!
//! # Platforms
//! On Linux both constructors map huge pages of the requested size, and the
//! mapping starts on a page boundary. Elsewhere [`HugeArena::anonymous`] is a
//! zeroed heap buffer aligned to the page size, which is what the simulator
//! and the tests run on; [`HugeArena::lock`] and
//! [`HugeArena::exclude_from_core_dumps`] succeed without effect, and
//! [`HugeArena::map_file`] fails with [`io::ErrorKind::Unsupported`].

use std::fmt;
use std::io;
use std::path::Path;

/// A huge page size an arena can be built on.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum HugePageSize {
    /// 2 MiB pages (`hugepagesz=2M`, `MAP_HUGE_2MB`), the platform profile's
    /// arena pages.
    TwoMiB,
    /// 1 GiB pages (`hugepagesz=1G`, `MAP_HUGE_1GB`).
    OneGiB,
}

impl HugePageSize {
    /// The page size in bytes.
    #[must_use]
    pub const fn bytes(self) -> usize {
        1 << self.page_bits()
    }

    /// log2 of the page size: 21 or 30, the value `mmap(2)` takes shifted by
    /// `MAP_HUGE_SHIFT` (`MAP_HUGE_2MB`, `MAP_HUGE_1GB`).
    #[must_use]
    pub const fn page_bits(self) -> u8 {
        match self {
            Self::TwoMiB => 21,
            Self::OneGiB => 30,
        }
    }
}

impl fmt::Display for HugePageSize {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::TwoMiB => "2 MiB",
            Self::OneGiB => "1 GiB",
        })
    }
}

/// An owned, page-aligned mapping of `len` bytes of huge pages, `len` a
/// positive multiple of the page size.
///
/// Dropping the arena unmaps it. A file-backed arena's file, and with it the
/// contents, stays on hugetlbfs for the next [`HugeArena::map_file`].
///
/// The kernel reaches the arena through its address ([`HugeArena::as_ptr`],
/// [`HugeArena::as_mut_ptr`]): io_uring buffer registration and the
/// destination of a user-copy read. Safe code cannot dereference the address,
/// and the kernel writes the arena only for an operation pushed to a ring,
/// which is the `unsafe` step: its proof must show that no Rust reference to
/// the bytes written is live until the operation completes.
pub struct HugeArena {
    map: imp::Mapping,
    len: usize,
    page_size: HugePageSize,
}

impl HugeArena {
    /// Maps the arena file at `path`, creating it (mode 0600) and sizing it
    /// to `len` bytes if it does not exist or is empty, `MAP_SHARED` and
    /// read-write. Linux only.
    ///
    /// This is the arena that survives the daemon: an existing file is mapped
    /// with its contents, and only at its own length. A file of another
    /// length is refused, never grown or truncated: either changes the
    /// arena's geometry and so discards what it holds (Step 6A); a caller
    /// that has established that the contents may go uses
    /// [`HugeArena::map_file_resizing`]. The file and its directory must be
    /// on a hugetlbfs mount whose page size is `page_size` (checked with
    /// `statfs(2)` before anything is created and with `fstatfs(2)` on the
    /// file), and the arena holds an exclusive `flock(2)` on the file for
    /// its lifetime, so a second arena, in this process or another, cannot
    /// map it concurrently.
    ///
    /// # Errors
    /// - [`io::ErrorKind::InvalidInput`]: `len` is not a positive multiple of
    ///   the page size, the path is not on a hugetlbfs mount of that page
    ///   size, or the existing file holds a length other than `len` (the
    ///   message names it).
    /// - [`io::ErrorKind::WouldBlock`]: another arena holds the file's lock.
    /// - The `open`, `ftruncate` or `mmap` error otherwise; `ENOMEM` from
    ///   `mmap` means the hugetlb pool cannot back the whole mapping.
    /// - [`io::ErrorKind::Unsupported`] on hosts other than Linux.
    pub fn map_file(path: &Path, len: usize, page_size: HugePageSize) -> io::Result<Self> {
        check_len(len, page_size)?;
        let map = imp::Mapping::map_file(path, len, page_size, false)?;
        Ok(Self::from_mapping(map, len, page_size))
    }

    /// [`HugeArena::map_file`] that first sets an existing file of another
    /// length to `len` bytes (grown, or truncated), under the file's lock.
    /// The arena's geometry changes with the length, so whatever the file
    /// held is lost to the arena: only for a caller that has read the old
    /// arena at its own length and found nothing in it to keep, or that was
    /// told to discard it. Linux only.
    ///
    /// # Errors
    /// As [`HugeArena::map_file`], except that a length mismatch is not one.
    pub fn map_file_resizing(path: &Path, len: usize, page_size: HugePageSize) -> io::Result<Self> {
        check_len(len, page_size)?;
        let map = imp::Mapping::map_file(path, len, page_size, true)?;
        Ok(Self::from_mapping(map, len, page_size))
    }

    /// A private, zeroed arena of `len` bytes: on Linux
    /// `MAP_PRIVATE | MAP_ANONYMOUS | MAP_HUGETLB` with the matching
    /// `MAP_HUGE_*` size, elsewhere an aligned heap buffer.
    ///
    /// # Errors
    /// [`io::ErrorKind::InvalidInput`] if `len` is not a positive multiple of
    /// the page size; on Linux the `mmap` error (`ENOMEM` when the hugetlb
    /// pool cannot back it), elsewhere [`io::ErrorKind::OutOfMemory`].
    pub fn anonymous(len: usize, page_size: HugePageSize) -> io::Result<Self> {
        check_len(len, page_size)?;
        let map = imp::Mapping::anonymous(len, page_size)?;
        Ok(Self::from_mapping(map, len, page_size))
    }

    /// A private, zeroed arena of `len` bytes on ordinary heap memory,
    /// aligned to `page_size`, on every host: `dbladed --heap-arena`
    /// (S5-1), for a homelab host or a CI VM that reserved no huge pages.
    /// It does not survive the process: a daemon crash on a heap arena is
    /// a dirty boot that holds the write-active volumes under nvfail,
    /// which a clean shutdown avoids.
    ///
    /// # Errors
    /// [`io::ErrorKind::InvalidInput`] if `len` is not a positive multiple of
    /// the page size, [`io::ErrorKind::OutOfMemory`] when the allocation
    /// fails.
    pub fn heap(len: usize, page_size: HugePageSize) -> io::Result<Self> {
        check_len(len, page_size)?;
        let map = imp::Mapping::heap(len, page_size)?;
        Ok(Self::from_mapping(map, len, page_size))
    }

    fn from_mapping(map: imp::Mapping, len: usize, page_size: HugePageSize) -> Self {
        debug_assert_eq!(map.as_slice().len(), len);
        debug_assert!(map.as_ptr().addr().is_multiple_of(page_size.bytes()));
        Self {
            map,
            len,
            page_size,
        }
    }

    /// The arena's length in bytes.
    #[must_use]
    pub const fn len(&self) -> usize {
        self.len
    }

    /// Always `false`: an arena holds at least one page.
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// The page size the arena is built on.
    #[must_use]
    pub const fn page_size(&self) -> HugePageSize {
        self.page_size
    }

    /// The arena's bytes.
    #[must_use]
    pub fn as_slice(&self) -> &[u8] {
        self.map.as_slice()
    }

    /// The arena's bytes, mutably.
    #[must_use]
    pub fn as_mut_slice(&mut self) -> &mut [u8] {
        self.map.as_mut_slice()
    }

    /// The base address, aligned to [`HugeArena::page_size`].
    #[must_use]
    pub fn as_ptr(&self) -> *const u8 {
        self.map.as_ptr()
    }

    /// The base address as the kernel receives it for writing: the
    /// destination of a user-copy read or a fixed-buffer read. Building an
    /// SQE from it is safe; the kernel's writes happen only for an operation
    /// that was pushed, and pushing is the `unsafe` step that must prove no
    /// Rust reference to those bytes is live.
    #[must_use]
    pub fn as_mut_ptr(&self) -> *mut u8 {
        self.map.as_ptr().cast_mut()
    }

    /// Locks the arena in memory (`mlock(2)`), so it can never be swapped.
    ///
    /// On Linux the kernel never swaps huge pages and does not mark hugetlb
    /// mappings `VM_LOCKED`, but it still checks the length against
    /// `RLIMIT_MEMLOCK` unless the process has `CAP_IPC_LOCK`, so an 8 GiB
    /// arena needs one or the other (`LimitMEMLOCK=infinity` in the unit).
    /// Elsewhere this does nothing.
    ///
    /// # Errors
    /// The `mlock` error: `ENOMEM` or `EPERM` past the limit.
    pub fn lock(&self) -> io::Result<()> {
        self.map.lock()
    }

    /// Excludes the arena from core dumps (`MADV_DONTDUMP`): an 8 GiB dump of
    /// cached client data is neither useful nor wanted. Elsewhere this does
    /// nothing.
    ///
    /// # Errors
    /// The `madvise` error.
    pub fn exclude_from_core_dumps(&self) -> io::Result<()> {
        self.map.exclude_from_core_dumps()
    }
}

impl fmt::Debug for HugeArena {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("HugeArena")
            .field("ptr", &self.as_ptr())
            .field("len", &self.len)
            .field("page_size", &self.page_size)
            .finish()
    }
}

/// Checks that `len` is a positive multiple of the page size.
fn check_len(len: usize, page_size: HugePageSize) -> io::Result<()> {
    if len == 0 || !len.is_multiple_of(page_size.bytes()) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("arena length {len} is not a positive multiple of the {page_size} page size"),
        ));
    }
    Ok(())
}

/// A zeroed heap buffer over-allocated by one page less one byte, of which
/// the page-aligned window `[offset, offset + len)` is the arena. The
/// buffer never grows, so the window never moves. The heap arena of every
/// host, and the only arena off Linux.
struct HeapBuffer {
    buf: Vec<u8>,
    offset: usize,
    len: usize,
}

impl HeapBuffer {
    fn new(len: usize, page_size: HugePageSize) -> io::Result<Self> {
        let align = page_size.bytes();
        let too_large = || {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("a {len}-byte arena does not fit this host's address space"),
            )
        };
        let total = len.checked_add(align - 1).ok_or_else(too_large)?;
        let mut buf = Vec::new();
        buf.try_reserve_exact(total)
            .map_err(|err| io::Error::new(io::ErrorKind::OutOfMemory, err))?;
        buf.resize(total, 0);
        let base = buf.as_ptr().addr();
        let offset = base.checked_next_multiple_of(align).ok_or_else(too_large)? - base;
        Ok(Self { buf, offset, len })
    }

    fn as_slice(&self) -> &[u8] {
        &self.buf[self.offset..self.offset + self.len]
    }

    fn as_mut_slice(&mut self) -> &mut [u8] {
        &mut self.buf[self.offset..self.offset + self.len]
    }

    fn as_ptr(&self) -> *const u8 {
        self.buf.as_ptr().wrapping_add(self.offset)
    }
}

#[cfg(target_os = "linux")]
mod imp {
    use std::fs::{File, OpenOptions};
    use std::io;
    use std::os::unix::fs::OpenOptionsExt;
    use std::path::Path;

    use memmap2::{Advice, MmapMut, MmapOptions};
    use rustix::fs::{flock, fstatfs, statfs, FlockOperation, StatFs};
    use rustix::io::Errno;

    use super::{HeapBuffer, HugePageSize};

    /// `HUGETLBFS_MAGIC` (`include/uapi/linux/magic.h`).
    const HUGETLBFS_MAGIC: u32 = 0x9584_58f6;

    /// The memory behind an arena: a huge-page mapping (and for a
    /// file-backed arena the open file that carries its exclusive lock;
    /// fields drop in order, so the mapping is unmapped before the file is
    /// closed and the lock released), or a heap buffer.
    pub(super) enum Mapping {
        Huge {
            map: MmapMut,
            _locked_file: Option<File>,
        },
        Heap(HeapBuffer),
    }

    impl Mapping {
        pub(super) fn anonymous(len: usize, page_size: HugePageSize) -> io::Result<Self> {
            let map = MmapOptions::new()
                .len(len)
                .huge(Some(page_size.page_bits()))
                .map_anon()?;
            Ok(Self::Huge {
                map,
                _locked_file: None,
            })
        }

        pub(super) fn heap(len: usize, page_size: HugePageSize) -> io::Result<Self> {
            Ok(Self::Heap(HeapBuffer::new(len, page_size)?))
        }

        pub(super) fn map_file(
            path: &Path,
            len: usize,
            page_size: HugePageSize,
            resize: bool,
        ) -> io::Result<Self> {
            // Refuse a directory on the wrong filesystem before creating
            // anything in it; the file itself is checked again once open.
            let dir = match path.parent() {
                Some(dir) if !dir.as_os_str().is_empty() => dir,
                _ => Path::new("."),
            };
            check_hugetlbfs(&statfs(dir)?, page_size, dir)?;
            // Never truncate on open: an existing file is the arena to
            // re-attach, with its contents.
            let file = OpenOptions::new()
                .read(true)
                .write(true)
                .create(true)
                .truncate(false)
                .mode(0o600)
                .open(path)?;
            check_hugetlbfs(&fstatfs(&file)?, page_size, path)?;
            flock(&file, FlockOperation::NonBlockingLockExclusive).map_err(|err| {
                if err == Errno::WOULDBLOCK {
                    io::Error::new(
                        io::ErrorKind::WouldBlock,
                        format!("{} is mapped by another arena", path.display()),
                    )
                } else {
                    io::Error::from(err)
                }
            })?;
            let want = len as u64;
            let have = file.metadata()?.len();
            // A new (empty) file is sized; an existing arena is mapped at
            // its own length unless the caller asked for the change: a
            // longer or shorter file is another geometry, and the arena it
            // holds would be recreated, its unflushed writes with it.
            if have != want && have != 0 && !resize {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    format!(
                        "{} holds a {have}-byte arena, not the {len}-byte arena asked for; \
                         refusing to resize arena contents",
                        path.display()
                    ),
                ));
            }
            if have != want {
                file.set_len(want)?;
            }
            // SAFETY: memmap2 requires that the file not be modified, in or
            // out of process, while the mapping lives, other than through the
            // mapping itself. (1) `fstatfs` proved the file is on hugetlbfs,
            // which has no write(2) or splice-write path: its bytes change
            // only through shared mappings, fallocate(2) or truncation. (2)
            // The exclusive flock taken above stays held for as long as the
            // mapping lives (`_locked_file` is dropped after `map`), so no
            // other arena in this or any OpenTAP process maps, grows or
            // truncates the file meanwhile; the file is created 0600 in the
            // daemon's private hugetlbfs mount, and writers that bypass both
            // (root editing the mount by hand) are outside the model, as
            // /proc/self/mem is. (3) We sized the file to exactly `len` bytes
            // above and never resize it while mapped, so the whole mapping is
            // backed by the file: a MAP_SHARED hugetlbfs mapping reserves its
            // pages at mmap time and fails with ENOMEM instead of faulting
            // later. (4) This mapping is the process's only view of the file
            // while the arena lives, and Rust code reaches it only through
            // `&self`/`&mut self` of the arena, so references to it obey the
            // borrow rules.
            let map = unsafe { MmapOptions::new().len(len).map_mut(&file) }?;
            Ok(Self::Huge {
                map,
                _locked_file: Some(file),
            })
        }

        pub(super) fn as_slice(&self) -> &[u8] {
            match self {
                Self::Huge { map, .. } => map,
                Self::Heap(heap) => heap.as_slice(),
            }
        }

        pub(super) fn as_mut_slice(&mut self) -> &mut [u8] {
            match self {
                Self::Huge { map, .. } => map,
                Self::Heap(heap) => heap.as_mut_slice(),
            }
        }

        pub(super) fn as_ptr(&self) -> *const u8 {
            match self {
                Self::Huge { map, .. } => map.as_ptr(),
                Self::Heap(heap) => heap.as_ptr(),
            }
        }

        /// Locks a huge mapping; a heap arena is ordinary memory, which
        /// `mlock` would pin page by page for no benefit a homelab host
        /// wants, so it is left to the kernel.
        pub(super) fn lock(&self) -> io::Result<()> {
            match self {
                Self::Huge { map, .. } => map.lock(),
                Self::Heap(_) => Ok(()),
            }
        }

        pub(super) fn exclude_from_core_dumps(&self) -> io::Result<()> {
            match self {
                Self::Huge { map, .. } => map.advise(Advice::DontDump),
                Self::Heap(_) => Ok(()),
            }
        }
    }

    /// Checks that `stat` describes a hugetlbfs mount of `page_size` pages.
    fn check_hugetlbfs(stat: &StatFs, page_size: HugePageSize, path: &Path) -> io::Result<()> {
        // `f_type` is a C long holding the unsigned 32-bit magic; compare the
        // low 32 bits so a sign-extending libc could not hide a match.
        if stat.f_type as u32 != HUGETLBFS_MAGIC {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("{} is not on a hugetlbfs mount", path.display()),
            ));
        }
        // hugetlbfs reports its page size as the block size.
        if usize::try_from(stat.f_bsize).ok() != Some(page_size.bytes()) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!(
                    "{} is on a hugetlbfs mount of {}-byte pages, not {page_size}",
                    path.display(),
                    stat.f_bsize
                ),
            ));
        }
        Ok(())
    }
}

#[cfg(not(target_os = "linux"))]
mod imp {
    use std::io;
    use std::path::Path;

    use super::{HeapBuffer, HugePageSize};

    /// Off Linux every arena is a heap buffer.
    pub(super) struct Mapping(HeapBuffer);

    impl Mapping {
        pub(super) fn anonymous(len: usize, page_size: HugePageSize) -> io::Result<Self> {
            Self::heap(len, page_size)
        }

        pub(super) fn heap(len: usize, page_size: HugePageSize) -> io::Result<Self> {
            Ok(Self(HeapBuffer::new(len, page_size)?))
        }

        pub(super) fn map_file(
            path: &Path,
            _len: usize,
            _page_size: HugePageSize,
            _resize: bool,
        ) -> io::Result<Self> {
            Err(io::Error::new(
                io::ErrorKind::Unsupported,
                format!(
                    "cannot map {}: hugetlbfs arenas exist on Linux only; use \
                     HugeArena::anonymous on this host",
                    path.display()
                ),
            ))
        }

        pub(super) fn as_slice(&self) -> &[u8] {
            self.0.as_slice()
        }

        pub(super) fn as_mut_slice(&mut self) -> &mut [u8] {
            self.0.as_mut_slice()
        }

        pub(super) fn as_ptr(&self) -> *const u8 {
            self.0.as_ptr()
        }

        pub(super) fn lock(&self) -> io::Result<()> {
            Ok(())
        }

        pub(super) fn exclude_from_core_dumps(&self) -> io::Result<()> {
            Ok(())
        }
    }
}
