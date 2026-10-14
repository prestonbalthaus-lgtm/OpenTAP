//! io_uring: the opcode numbers of Linux 6.12, fixed-buffer registration of
//! an arena, buffer-table cloning between rings, and the user-copy read of
//! the ublk write path.
//!
//! The opcodes are the positions of `enum io_uring_op` in
//! `include/uapi/linux/io_uring.h` (v6.12; `io_uring.h:NN` names the line).
//! No engine crate names an opcode outside this module and the allowlist built
//! from it ([`crate::probe`]).
//!
//! # Platforms
//! The opcode table and [`FixedBufferLayout`] are pure and the same
//! everywhere. On Linux [`RegisteredBuffers`] registers an arena with a ring,
//! `clone_buffers` copies a ring's buffer table and `user_copy_read_sqe`
//! builds the ublk copy-out read; these name `io_uring` types and exist on
//! Linux only, except `RegisteredBuffers`, whose constructor elsewhere fails
//! with [`std::io::ErrorKind::Unsupported`].

/// `IORING_OP_NOP` (io_uring.h:204).
pub const IORING_OP_NOP: u8 = 0;
/// `IORING_OP_READV` (io_uring.h:205).
pub const IORING_OP_READV: u8 = 1;
/// `IORING_OP_WRITEV` (io_uring.h:206).
pub const IORING_OP_WRITEV: u8 = 2;
/// `IORING_OP_FSYNC` (io_uring.h:207): the per-device-class flush of a CP.
pub const IORING_OP_FSYNC: u8 = 3;
/// `IORING_OP_READ_FIXED` (io_uring.h:208). Never on `/dev/ublkcN`: the 6.12
/// driver refuses iterators that are not user-backed with `-EACCES` (C03).
pub const IORING_OP_READ_FIXED: u8 = 4;
/// `IORING_OP_WRITE_FIXED` (io_uring.h:209): one per stripe from the staging
/// pool.
pub const IORING_OP_WRITE_FIXED: u8 = 5;
/// `IORING_OP_POLL_ADD` (io_uring.h:210).
pub const IORING_OP_POLL_ADD: u8 = 6;
/// `IORING_OP_POLL_REMOVE` (io_uring.h:211).
pub const IORING_OP_POLL_REMOVE: u8 = 7;
/// `IORING_OP_SYNC_FILE_RANGE` (io_uring.h:212).
pub const IORING_OP_SYNC_FILE_RANGE: u8 = 8;
/// `IORING_OP_SENDMSG` (io_uring.h:213).
pub const IORING_OP_SENDMSG: u8 = 9;
/// `IORING_OP_RECVMSG` (io_uring.h:214).
pub const IORING_OP_RECVMSG: u8 = 10;
/// `IORING_OP_TIMEOUT` (io_uring.h:215).
pub const IORING_OP_TIMEOUT: u8 = 11;
/// `IORING_OP_TIMEOUT_REMOVE` (io_uring.h:216).
pub const IORING_OP_TIMEOUT_REMOVE: u8 = 12;
/// `IORING_OP_ACCEPT` (io_uring.h:217).
pub const IORING_OP_ACCEPT: u8 = 13;
/// `IORING_OP_ASYNC_CANCEL` (io_uring.h:218).
pub const IORING_OP_ASYNC_CANCEL: u8 = 14;
/// `IORING_OP_LINK_TIMEOUT` (io_uring.h:219).
pub const IORING_OP_LINK_TIMEOUT: u8 = 15;
/// `IORING_OP_CONNECT` (io_uring.h:220).
pub const IORING_OP_CONNECT: u8 = 16;
/// `IORING_OP_FALLOCATE` (io_uring.h:221).
pub const IORING_OP_FALLOCATE: u8 = 17;
/// `IORING_OP_OPENAT` (io_uring.h:222).
pub const IORING_OP_OPENAT: u8 = 18;
/// `IORING_OP_CLOSE` (io_uring.h:223).
pub const IORING_OP_CLOSE: u8 = 19;
/// `IORING_OP_FILES_UPDATE` (io_uring.h:224).
pub const IORING_OP_FILES_UPDATE: u8 = 20;
/// `IORING_OP_STATX` (io_uring.h:225).
pub const IORING_OP_STATX: u8 = 21;
/// `IORING_OP_READ` (io_uring.h:226): the ublk user-copy read.
pub const IORING_OP_READ: u8 = 22;
/// `IORING_OP_WRITE` (io_uring.h:227): the ublk user-copy write of READ data.
pub const IORING_OP_WRITE: u8 = 23;
/// `IORING_OP_FADVISE` (io_uring.h:228).
pub const IORING_OP_FADVISE: u8 = 24;
/// `IORING_OP_MADVISE` (io_uring.h:229).
pub const IORING_OP_MADVISE: u8 = 25;
/// `IORING_OP_SEND` (io_uring.h:230).
pub const IORING_OP_SEND: u8 = 26;
/// `IORING_OP_RECV` (io_uring.h:231): the replication receiver.
pub const IORING_OP_RECV: u8 = 27;
/// `IORING_OP_OPENAT2` (io_uring.h:232).
pub const IORING_OP_OPENAT2: u8 = 28;
/// `IORING_OP_EPOLL_CTL` (io_uring.h:233).
pub const IORING_OP_EPOLL_CTL: u8 = 29;
/// `IORING_OP_SPLICE` (io_uring.h:234).
pub const IORING_OP_SPLICE: u8 = 30;
/// `IORING_OP_PROVIDE_BUFFERS` (io_uring.h:235).
pub const IORING_OP_PROVIDE_BUFFERS: u8 = 31;
/// `IORING_OP_REMOVE_BUFFERS` (io_uring.h:236).
pub const IORING_OP_REMOVE_BUFFERS: u8 = 32;
/// `IORING_OP_TEE` (io_uring.h:237).
pub const IORING_OP_TEE: u8 = 33;
/// `IORING_OP_SHUTDOWN` (io_uring.h:238).
pub const IORING_OP_SHUTDOWN: u8 = 34;
/// `IORING_OP_RENAMEAT` (io_uring.h:239).
pub const IORING_OP_RENAMEAT: u8 = 35;
/// `IORING_OP_UNLINKAT` (io_uring.h:240).
pub const IORING_OP_UNLINKAT: u8 = 36;
/// `IORING_OP_MKDIRAT` (io_uring.h:241).
pub const IORING_OP_MKDIRAT: u8 = 37;
/// `IORING_OP_SYMLINKAT` (io_uring.h:242).
pub const IORING_OP_SYMLINKAT: u8 = 38;
/// `IORING_OP_LINKAT` (io_uring.h:243).
pub const IORING_OP_LINKAT: u8 = 39;
/// `IORING_OP_MSG_RING` (io_uring.h:244).
pub const IORING_OP_MSG_RING: u8 = 40;
/// `IORING_OP_FSETXATTR` (io_uring.h:245).
pub const IORING_OP_FSETXATTR: u8 = 41;
/// `IORING_OP_SETXATTR` (io_uring.h:246).
pub const IORING_OP_SETXATTR: u8 = 42;
/// `IORING_OP_FGETXATTR` (io_uring.h:247).
pub const IORING_OP_FGETXATTR: u8 = 43;
/// `IORING_OP_GETXATTR` (io_uring.h:248).
pub const IORING_OP_GETXATTR: u8 = 44;
/// `IORING_OP_SOCKET` (io_uring.h:249).
pub const IORING_OP_SOCKET: u8 = 45;
/// `IORING_OP_URING_CMD` (io_uring.h:250): every ublk command.
pub const IORING_OP_URING_CMD: u8 = 46;
/// `IORING_OP_SEND_ZC` (io_uring.h:251): the replication sender.
pub const IORING_OP_SEND_ZC: u8 = 47;
/// `IORING_OP_SENDMSG_ZC` (io_uring.h:252).
pub const IORING_OP_SENDMSG_ZC: u8 = 48;
/// `IORING_OP_READ_MULTISHOT` (io_uring.h:253).
pub const IORING_OP_READ_MULTISHOT: u8 = 49;
/// `IORING_OP_WAITID` (io_uring.h:254).
pub const IORING_OP_WAITID: u8 = 50;
/// `IORING_OP_FUTEX_WAIT` (io_uring.h:255).
pub const IORING_OP_FUTEX_WAIT: u8 = 51;
/// `IORING_OP_FUTEX_WAKE` (io_uring.h:256).
pub const IORING_OP_FUTEX_WAKE: u8 = 52;
/// `IORING_OP_FUTEX_WAITV` (io_uring.h:257).
pub const IORING_OP_FUTEX_WAITV: u8 = 53;
/// `IORING_OP_FIXED_FD_INSTALL` (io_uring.h:258).
pub const IORING_OP_FIXED_FD_INSTALL: u8 = 54;
/// `IORING_OP_FTRUNCATE` (io_uring.h:259).
pub const IORING_OP_FTRUNCATE: u8 = 55;
/// `IORING_OP_BIND` (io_uring.h:260).
pub const IORING_OP_BIND: u8 = 56;
/// `IORING_OP_LISTEN` (io_uring.h:261).
pub const IORING_OP_LISTEN: u8 = 57;
/// `IORING_OP_LAST` (io_uring.h:264): one past the last opcode 6.12 defines.
/// Later kernels define more; the probe reports what the running one has.
pub const IORING_OP_LAST: u8 = 58;

/// The name of an opcode below [`IORING_OP_LAST`], for diagnostics.
#[must_use]
pub const fn opcode_name(opcode: u8) -> Option<&'static str> {
    Some(match opcode {
        IORING_OP_NOP => "IORING_OP_NOP",
        IORING_OP_READV => "IORING_OP_READV",
        IORING_OP_WRITEV => "IORING_OP_WRITEV",
        IORING_OP_FSYNC => "IORING_OP_FSYNC",
        IORING_OP_READ_FIXED => "IORING_OP_READ_FIXED",
        IORING_OP_WRITE_FIXED => "IORING_OP_WRITE_FIXED",
        IORING_OP_POLL_ADD => "IORING_OP_POLL_ADD",
        IORING_OP_POLL_REMOVE => "IORING_OP_POLL_REMOVE",
        IORING_OP_SYNC_FILE_RANGE => "IORING_OP_SYNC_FILE_RANGE",
        IORING_OP_SENDMSG => "IORING_OP_SENDMSG",
        IORING_OP_RECVMSG => "IORING_OP_RECVMSG",
        IORING_OP_TIMEOUT => "IORING_OP_TIMEOUT",
        IORING_OP_TIMEOUT_REMOVE => "IORING_OP_TIMEOUT_REMOVE",
        IORING_OP_ACCEPT => "IORING_OP_ACCEPT",
        IORING_OP_ASYNC_CANCEL => "IORING_OP_ASYNC_CANCEL",
        IORING_OP_LINK_TIMEOUT => "IORING_OP_LINK_TIMEOUT",
        IORING_OP_CONNECT => "IORING_OP_CONNECT",
        IORING_OP_FALLOCATE => "IORING_OP_FALLOCATE",
        IORING_OP_OPENAT => "IORING_OP_OPENAT",
        IORING_OP_CLOSE => "IORING_OP_CLOSE",
        IORING_OP_FILES_UPDATE => "IORING_OP_FILES_UPDATE",
        IORING_OP_STATX => "IORING_OP_STATX",
        IORING_OP_READ => "IORING_OP_READ",
        IORING_OP_WRITE => "IORING_OP_WRITE",
        IORING_OP_FADVISE => "IORING_OP_FADVISE",
        IORING_OP_MADVISE => "IORING_OP_MADVISE",
        IORING_OP_SEND => "IORING_OP_SEND",
        IORING_OP_RECV => "IORING_OP_RECV",
        IORING_OP_OPENAT2 => "IORING_OP_OPENAT2",
        IORING_OP_EPOLL_CTL => "IORING_OP_EPOLL_CTL",
        IORING_OP_SPLICE => "IORING_OP_SPLICE",
        IORING_OP_PROVIDE_BUFFERS => "IORING_OP_PROVIDE_BUFFERS",
        IORING_OP_REMOVE_BUFFERS => "IORING_OP_REMOVE_BUFFERS",
        IORING_OP_TEE => "IORING_OP_TEE",
        IORING_OP_SHUTDOWN => "IORING_OP_SHUTDOWN",
        IORING_OP_RENAMEAT => "IORING_OP_RENAMEAT",
        IORING_OP_UNLINKAT => "IORING_OP_UNLINKAT",
        IORING_OP_MKDIRAT => "IORING_OP_MKDIRAT",
        IORING_OP_SYMLINKAT => "IORING_OP_SYMLINKAT",
        IORING_OP_LINKAT => "IORING_OP_LINKAT",
        IORING_OP_MSG_RING => "IORING_OP_MSG_RING",
        IORING_OP_FSETXATTR => "IORING_OP_FSETXATTR",
        IORING_OP_SETXATTR => "IORING_OP_SETXATTR",
        IORING_OP_FGETXATTR => "IORING_OP_FGETXATTR",
        IORING_OP_GETXATTR => "IORING_OP_GETXATTR",
        IORING_OP_SOCKET => "IORING_OP_SOCKET",
        IORING_OP_URING_CMD => "IORING_OP_URING_CMD",
        IORING_OP_SEND_ZC => "IORING_OP_SEND_ZC",
        IORING_OP_SENDMSG_ZC => "IORING_OP_SENDMSG_ZC",
        IORING_OP_READ_MULTISHOT => "IORING_OP_READ_MULTISHOT",
        IORING_OP_WAITID => "IORING_OP_WAITID",
        IORING_OP_FUTEX_WAIT => "IORING_OP_FUTEX_WAIT",
        IORING_OP_FUTEX_WAKE => "IORING_OP_FUTEX_WAKE",
        IORING_OP_FUTEX_WAITV => "IORING_OP_FUTEX_WAITV",
        IORING_OP_FIXED_FD_INSTALL => "IORING_OP_FIXED_FD_INSTALL",
        IORING_OP_FTRUNCATE => "IORING_OP_FTRUNCATE",
        IORING_OP_BIND => "IORING_OP_BIND",
        IORING_OP_LISTEN => "IORING_OP_LISTEN",
        _ => return None,
    })
}

/// The largest fixed buffer the kernel registers: 1 GiB (`io_buffer_validate`
/// in `io_uring/rsrc.c` refuses `iov_len > SZ_1G` with `EFAULT`). An arena is
/// therefore registered as consecutive buffers of at most this size.
pub const MAX_FIXED_BUFFER_LEN: usize = 1 << 30;

/// The most fixed buffers one table holds (`IORING_MAX_REG_BUFFERS`,
/// `io_uring/rsrc.c`): 16384.
pub const MAX_FIXED_BUFFERS: usize = 1 << 14;

/// How a region of `len` bytes is cut into fixed buffers: consecutive buffers
/// of [`MAX_FIXED_BUFFER_LEN`] bytes, the last one shorter when `len` is not a
/// multiple of it, numbered from 0 in address order.
///
/// A fixed-buffer operation must lie within one registered buffer, so a
/// staging allocator keeps stripes from straddling a boundary and finds the
/// `buf_index` of an operation with [`FixedBufferLayout::index_of`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct FixedBufferLayout {
    len: usize,
    chunk_len: usize,
}

impl FixedBufferLayout {
    /// The layout of `len` bytes; `None` if `len` is zero or would need more
    /// than [`MAX_FIXED_BUFFERS`] buffers.
    #[must_use]
    pub const fn new(len: usize) -> Option<Self> {
        if len == 0 {
            return None;
        }
        let chunk_len = if len < MAX_FIXED_BUFFER_LEN {
            len
        } else {
            MAX_FIXED_BUFFER_LEN
        };
        if len.div_ceil(chunk_len) > MAX_FIXED_BUFFERS {
            return None;
        }
        Some(Self { len, chunk_len })
    }

    /// The region's length in bytes.
    #[must_use]
    pub const fn len(&self) -> usize {
        self.len
    }

    /// Always `false`: a layout covers at least one byte.
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// The length of every buffer but possibly the last.
    #[must_use]
    pub const fn chunk_len(&self) -> usize {
        self.chunk_len
    }

    /// The number of buffers, at most [`MAX_FIXED_BUFFERS`].
    #[must_use]
    pub const fn count(&self) -> usize {
        self.len.div_ceil(self.chunk_len)
    }

    /// Buffer `index` as `(offset, len)` within the region; `None` past the
    /// last buffer.
    #[must_use]
    pub const fn buffer(&self, index: usize) -> Option<(usize, usize)> {
        if index >= self.count() {
            return None;
        }
        let start = index * self.chunk_len;
        let end = if self.len - start < self.chunk_len {
            self.len
        } else {
            start + self.chunk_len
        };
        Some((start, end - start))
    }

    /// The `buf_index` of the buffer that holds all of `[offset, offset +
    /// len)`; `None` if the range is empty, runs past the region or crosses a
    /// buffer boundary.
    #[must_use]
    pub const fn index_of(&self, offset: usize, len: usize) -> Option<u16> {
        let end = match offset.checked_add(len) {
            Some(end) => end,
            None => return None,
        };
        if len == 0 || end > self.len {
            return None;
        }
        let index = offset / self.chunk_len;
        if (end - 1) / self.chunk_len != index {
            return None;
        }
        // `index < count() <= MAX_FIXED_BUFFERS = 1 << 14`, so it fits.
        Some(index as u16)
    }
}

#[cfg(target_os = "linux")]
mod linux {
    use core::marker::PhantomData;
    use core::mem::{align_of, size_of};
    use std::io::{self, IoSlice};
    use std::os::fd::{AsRawFd, RawFd};

    use io_uring::{cqueue, opcode, squeue, types, IoUring, Submitter};

    use super::FixedBufferLayout;
    use crate::hugetlb::HugeArena;
    use crate::ublk::UblkChar;

    /// An arena registered as a ring's fixed-buffer table (Linux).
    ///
    /// The arena is cut into buffers of at most 1 GiB as
    /// [`FixedBufferLayout`] describes, so `buf_index` `i` covers
    /// [`FixedBufferLayout::buffer`]`(i)` of the arena. The value borrows the
    /// arena and the ring for `'a`, so neither can go away while the kernel
    /// holds the registration, and dropping it unregisters the table. One
    /// value owns its ring's table: io_uring keeps one table per ring and
    /// refuses a second registration (`EBUSY`).
    ///
    /// Registration lets the kernel use the memory but does not make it do
    /// so: the kernel writes registered memory only for a fixed-buffer
    /// operation that was pushed, and `SubmissionQueue::push` is `unsafe`
    /// because the pusher must prove that no Rust reference to the target
    /// bytes is live until the operation's CQE.
    pub struct RegisteredBuffers<'a> {
        submitter: Submitter<'a>,
        ring_fd: RawFd,
        layout: FixedBufferLayout,
        _memory: PhantomData<&'a [u8]>,
    }

    impl<'a> RegisteredBuffers<'a> {
        /// Registers `arena` with `ring` (`IORING_REGISTER_BUFFERS`). The
        /// pages are pinned, and counted against `RLIMIT_MEMLOCK` unless the
        /// process has `CAP_IPC_LOCK`.
        ///
        /// # Errors
        /// `EBUSY` if the ring already has a buffer table, `ENOMEM` past the
        /// memlock limit, other `io_uring_register(2)` errors as returned.
        pub fn new<S, C>(ring: &'a IoUring<S, C>, arena: &'a HugeArena) -> io::Result<Self>
        where
            S: squeue::EntryMarker,
            C: cqueue::EntryMarker,
        {
            let layout = FixedBufferLayout::new(arena.len()).ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "arena needs more fixed buffers than one io_uring table holds",
                )
            })?;
            let bytes = arena.as_slice();
            let slices: Vec<IoSlice<'_>> = (0..layout.count())
                .filter_map(|index| layout.buffer(index))
                .map(|(start, len)| IoSlice::new(&bytes[start..start + len]))
                .collect();
            // `Submitter::register_buffers` takes `&[libc::iovec]`. libc is
            // not a dependency of this crate, so the element type is left to
            // inference from that call; std guarantees `IoSlice` is ABI
            // compatible with `iovec` on Unix, and the const assertion in
            // `assert_iovec_layout` checks size and alignment of the inferred
            // type at compile time.
            let first = slices.as_ptr().cast();
            assert_iovec_layout(first);
            // SAFETY: `first` points to `slices.len()` initialised, properly
            // aligned `IoSlice` values in one allocation, which `slices` keeps
            // alive and unmodified for the rest of this function. The element
            // type is `libc::iovec` (inferred from `register_buffers` below),
            // with which std guarantees `IoSlice` to be ABI compatible on
            // Unix, and whose size and alignment `assert_iovec_layout` proved
            // equal to `IoSlice`'s; so the bytes are valid `iovec`s.
            let iovecs = unsafe { std::slice::from_raw_parts(first, slices.len()) };
            let submitter = ring.submitter();
            // SAFETY: `register_buffers` requires each `iov_base`/`iov_len`
            // to stay valid until the table is unregistered or the ring is
            // destroyed. They describe `arena`'s mapping, which `Self` borrows
            // for 'a, as it borrows `ring` (through `submitter`) for 'a: the
            // mapping cannot be unmapped nor the ring closed while the value
            // lives, and `Drop` unregisters the table before the borrows end.
            // Should the table outlive the borrows (the value leaked with
            // `mem::forget`, or the kernel refusing to unregister), what the
            // requirement protects still holds: the kernel reaches registered
            // memory through the pages it pinned here, never through this
            // address, keeps them pinned until the ring closes, and writes
            // them only for a fixed-buffer operation whose `unsafe` push must
            // prove the target memory valid and unaliased. A stale table thus
            // keeps memory pinned but cannot make the kernel write anywhere.
            unsafe { submitter.register_buffers(iovecs) }?;
            Ok(Self {
                submitter,
                ring_fd: ring.as_raw_fd(),
                layout,
                _memory: PhantomData,
            })
        }

        /// How the arena is cut into buffers: `buf_index` `i` is
        /// `layout().buffer(i)`.
        #[must_use]
        pub const fn layout(&self) -> FixedBufferLayout {
            self.layout
        }

        /// Clones this table into `dst` with [`clone_buffers`] and returns the
        /// guard of `dst`'s copy, which unregisters it on drop. The guard
        /// borrows `self`, so the copy cannot outlive the memory it names.
        ///
        /// # Errors
        /// As [`clone_buffers`].
        pub fn clone_to<'b, S, C>(
            &'b self,
            dst: &'b IoUring<S, C>,
        ) -> io::Result<RegisteredBuffers<'b>>
        where
            S: squeue::EntryMarker,
            C: cqueue::EntryMarker,
        {
            dst.submitter().register_buffers_clone(self.ring_fd)?;
            Ok(RegisteredBuffers {
                submitter: dst.submitter(),
                ring_fd: dst.as_raw_fd(),
                layout: self.layout,
                _memory: PhantomData,
            })
        }
    }

    impl Drop for RegisteredBuffers<'_> {
        /// Unregisters the table. Unregistering waits for the operations
        /// that still use it, and a signal interrupts that wait, so `EINTR`
        /// is retried. `ENXIO` means the table was already unregistered
        /// through the ring. Any other refusal (`ENOMEM` while quiescing)
        /// leaves the table registered until the ring closes, the same state
        /// as a value leaked with `mem::forget`, which the SAFETY argument in
        /// [`RegisteredBuffers::new`] covers: the pages stay pinned and the
        /// kernel writes them only for a pushed fixed-buffer operation.
        fn drop(&mut self) {
            loop {
                match self.submitter.unregister_buffers() {
                    Err(err) if err.kind() == io::ErrorKind::Interrupted => {}
                    Ok(()) | Err(_) => return,
                }
            }
        }
    }

    impl core::fmt::Debug for RegisteredBuffers<'_> {
        fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
            f.debug_struct("RegisteredBuffers")
                .field("ring_fd", &self.ring_fd)
                .field("layout", &self.layout)
                .finish()
        }
    }

    /// Compile-time check that `T`, the element type `register_buffers`
    /// takes (inferred at the call site), is laid out like `IoSlice`.
    const fn assert_iovec_layout<T>(_: *const T) {
        const {
            assert!(size_of::<T>() == size_of::<IoSlice<'static>>());
            assert!(align_of::<T>() == align_of::<IoSlice<'static>>());
        }
    }

    /// Clones `src`'s whole buffer table into `dst`
    /// (`IORING_REGISTER_CLONE_BUFFERS`, 6.12): `dst` then uses the same
    /// pinned pages under the same indices without pinning them again, which
    /// is how every reactor ring shares one registration of the arena.
    ///
    /// On 6.12 the clone is all or nothing: the whole table goes into a ring
    /// whose table is empty (`EBUSY` otherwise), and the source must have one
    /// (`ENXIO` otherwise). The copy holds its own references and stays
    /// registered until `dst` unregisters it or is closed; nothing ties it to
    /// the memory's lifetime, so prefer [`RegisteredBuffers::clone_to`],
    /// whose guard does.
    ///
    /// # Errors
    /// The `io_uring_register(2)` error.
    pub fn clone_buffers<S1, C1, S2, C2>(
        dst: &IoUring<S1, C1>,
        src: &IoUring<S2, C2>,
    ) -> io::Result<()>
    where
        S1: squeue::EntryMarker,
        C1: cqueue::EntryMarker,
        S2: squeue::EntryMarker,
        C2: cqueue::EntryMarker,
    {
        dst.submitter().register_buffers_clone(src.as_raw_fd())
    }

    /// The `IORING_OP_READ` that copies `len` bytes of a ublk WRITE request
    /// out of the driver into `buf`, at the user-copy position `offset`
    /// ([`crate::ublk::user_copy_offset`]) of the `O_NONBLOCK` descriptor
    /// `chr`.
    ///
    /// A plain read into a plain pointer, never `IORING_OP_READ_FIXED`: the
    /// 6.12 driver accepts only user-backed iterators on `/dev/ublkcN`
    /// (`ublk_check_and_get_req` returns `-EACCES`). No `IOSQE_ASYNC`
    /// either: with `O_NONBLOCK` the copy runs inline at submission, and
    /// forcing it to io-wq costs a worker hand-off per request (C03). The
    /// entry has no flags and `user_data` 0; add `user_data`, not
    /// `Flags::ASYNC`.
    ///
    /// Building the entry is safe. Pushing it is not: `buf` must stay valid
    /// for writes of `len` bytes, with no live Rust reference to them, until
    /// the CQE is reaped.
    #[must_use]
    pub fn user_copy_read_sqe(
        chr: &UblkChar,
        buf: *mut u8,
        len: u32,
        offset: u64,
    ) -> squeue::Entry {
        opcode::Read::new(types::Fd(chr.as_raw_fd()), buf, len)
            .offset(offset)
            .build()
    }

    // The opcode table against io-uring's own (bindgen) values.
    const _: () = {
        use super::*;
        assert!(IORING_OP_NOP == opcode::Nop::CODE);
        assert!(IORING_OP_FSYNC == opcode::Fsync::CODE);
        assert!(IORING_OP_READ_FIXED == opcode::ReadFixed::CODE);
        assert!(IORING_OP_WRITE_FIXED == opcode::WriteFixed::CODE);
        assert!(IORING_OP_READ == opcode::Read::CODE);
        assert!(IORING_OP_WRITE == opcode::Write::CODE);
        assert!(IORING_OP_SEND == opcode::Send::CODE);
        assert!(IORING_OP_RECV == opcode::Recv::CODE);
        assert!(IORING_OP_URING_CMD == opcode::UringCmd16::CODE);
        assert!(IORING_OP_URING_CMD == opcode::UringCmd80::CODE);
        assert!(IORING_OP_SEND_ZC == opcode::SendZc::CODE);
        assert!(IORING_OP_SENDMSG_ZC == opcode::SendMsgZc::CODE);
        assert!(IORING_OP_FTRUNCATE == opcode::Ftruncate::CODE);
        assert!(IORING_OP_BIND == opcode::Bind::CODE);
        assert!(IORING_OP_LISTEN == opcode::Listen::CODE);
    };
}

#[cfg(target_os = "linux")]
pub use linux::{clone_buffers, user_copy_read_sqe, RegisteredBuffers};

#[cfg(not(target_os = "linux"))]
mod fallback {
    use core::convert::Infallible;
    use core::marker::PhantomData;
    use std::io;

    use super::FixedBufferLayout;
    use crate::hugetlb::HugeArena;

    /// An arena registered as a ring's fixed-buffer table. io_uring exists on
    /// Linux only: on this host the constructor fails and no value exists.
    pub struct RegisteredBuffers<'a> {
        never: Infallible,
        _memory: PhantomData<&'a [u8]>,
    }

    impl<'a> RegisteredBuffers<'a> {
        /// Fails with [`io::ErrorKind::Unsupported`]: there is no io_uring
        /// on this host.
        ///
        /// # Errors
        /// Always.
        pub fn new(arena: &'a HugeArena) -> io::Result<Self> {
            Err(io::Error::new(
                io::ErrorKind::Unsupported,
                format!(
                    "cannot register a {}-byte arena: io_uring exists on Linux only",
                    arena.len()
                ),
            ))
        }

        /// How the arena is cut into buffers.
        #[must_use]
        pub const fn layout(&self) -> FixedBufferLayout {
            match self.never {}
        }
    }

    impl core::fmt::Debug for RegisteredBuffers<'_> {
        fn fmt(&self, _: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
            match self.never {}
        }
    }
}

#[cfg(not(target_os = "linux"))]
pub use fallback::RegisteredBuffers;
