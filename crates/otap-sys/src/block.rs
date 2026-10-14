//! A block device on io_uring: the ring, the descriptor and the aligned
//! staging pool behind OpenTAP's Linux `Backstore` (Step 4 brief,
//! "Opus-Storage-Geom": `O_DIRECT` plus io_uring writes for stripes and
//! metadata, `IORING_OP_FSYNC` with `FSYNC_DATASYNC` as the barrier, and
//! `RWF_DSYNC` writes for the superblock and intent slots).
//!
//! The engine hands the backstore stripe images it owns only transiently,
//! so every write is staged: copied into one of the pool's page-aligned
//! slots (`O_DIRECT` needs the alignment anyway), which the kernel reads
//! from until the write's completion is reaped. The pool is the owner of
//! that memory and the only party that hands a slot out, so the `unsafe`
//! push of a write can prove the entry's buffer valid and unaliased for the
//! operation's whole life (the crate's unsafe policy).
//!
//! On hosts other than Linux the type exists with a constructor that fails
//! with [`std::io::ErrorKind::Unsupported`], so the backstore crate builds
//! and its pure parts run everywhere.

use std::io;
use std::path::Path;

/// "The submission queue is full": nothing was pushed; submit and reap
/// first.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Full;

impl std::fmt::Display for Full {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("submission queue full")
    }
}

impl std::error::Error for Full {}

/// A completion the ring reaped: the operation's `user_data` and its
/// result (bytes transferred, or a negative errno).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Reaped {
    /// What the caller tagged the operation with.
    pub user_data: u64,
    /// The CQE result.
    pub result: i32,
}

/// One 4 KiB page, aligned for direct I/O.
#[cfg(target_os = "linux")]
#[repr(C, align(4096))]
#[derive(Clone, Copy)]
struct Page([u8; 4096]);

/// A page-aligned staging pool of `slots` slots of `slot_bytes` each.
#[cfg(target_os = "linux")]
struct Pool {
    pages: Vec<Page>,
    slot_pages: usize,
    free: Vec<usize>,
    busy: Vec<bool>,
}

#[cfg(target_os = "linux")]
impl Pool {
    fn new(slot_bytes: usize, slots: usize) -> io::Result<Self> {
        if slot_bytes == 0 || !slot_bytes.is_multiple_of(4096) || slots == 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "a staging slot is a positive number of whole pages",
            ));
        }
        let slot_pages = slot_bytes / 4096;
        let total = slot_pages
            .checked_mul(slots)
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "pool too large"))?;
        let mut pages = Vec::new();
        pages
            .try_reserve_exact(total)
            .map_err(|e| io::Error::new(io::ErrorKind::OutOfMemory, e))?;
        pages.resize(total, Page([0; 4096]));
        Ok(Self {
            pages,
            slot_pages,
            free: (0..slots).rev().collect(),
            busy: vec![false; slots],
        })
    }

    fn slot_bytes(&self) -> usize {
        self.slot_pages * 4096
    }

    fn slot_mut(&mut self, slot: usize) -> &mut [u8] {
        let start = slot * self.slot_pages;
        let pages = &mut self.pages[start..start + self.slot_pages];
        // A `Page` is exactly 4096 bytes with no padding, so the pages of a
        // slot are one contiguous byte range.
        let ptr = pages.as_mut_ptr().cast::<u8>();
        let len = pages.len() * 4096;
        // SAFETY: `ptr` points at `len` initialised bytes of `pages`, a
        // contiguous slice of `Page` values (each `[u8; 4096]`, alignment
        // 4096, no padding); the borrow of `pages` is exclusive for the
        // returned lifetime, so no other reference to those bytes is live.
        unsafe { std::slice::from_raw_parts_mut(ptr, len) }
    }

    fn slot(&self, slot: usize) -> &[u8] {
        let start = slot * self.slot_pages;
        let pages = &self.pages[start..start + self.slot_pages];
        let ptr = pages.as_ptr().cast::<u8>();
        let len = pages.len() * 4096;
        // SAFETY: as in `slot_mut`, with a shared borrow of `pages`.
        unsafe { std::slice::from_raw_parts(ptr, len) }
    }
}

#[cfg(target_os = "linux")]
mod linux {
    use std::collections::HashMap;
    use std::io;
    use std::os::fd::{AsRawFd, OwnedFd};
    use std::path::Path;

    use io_uring::{cqueue, opcode, squeue, types, IoUring};
    use rustix::fs::{open, seek, Mode, OFlags, SeekFrom};
    use rustix::io::{pread, ReadWriteFlags};

    use super::{Full, Pool, Reaped};

    /// `BLKDISCARD`: `_IO(0x12, 119)`, `include/uapi/linux/fs.h`.
    const BLKDISCARD: rustix::ioctl::Opcode = rustix::ioctl::opcode::none(0x12, 119);

    /// A block device opened `O_DIRECT | O_RDWR | O_CLOEXEC` with an
    /// io_uring ring and a staging pool. Linux only.
    pub struct BlockRing {
        ring: IoUring<squeue::Entry, cqueue::Entry>,
        fd: OwnedFd,
        pool: Pool,
        /// The slot each in-flight write holds, by `user_data`.
        inflight: HashMap<u64, usize>,
        /// Operations pushed and not yet reaped, writes and fsyncs alike.
        outstanding: usize,
    }

    impl BlockRing {
        /// Opens `path` for direct I/O with a ring of `entries` entries
        /// and a pool of `slots` staging slots of `slot_bytes` each.
        ///
        /// # Errors
        /// The `open(2)` or `io_uring_setup(2)` error, or an invalid pool.
        pub fn open(
            path: &Path,
            entries: u32,
            slot_bytes: usize,
            slots: usize,
        ) -> io::Result<Self> {
            let pool = Pool::new(slot_bytes, slots)?;
            let flags = OFlags::RDWR | OFlags::DIRECT | OFlags::CLOEXEC;
            let fd = open(path, flags, Mode::empty())?;
            let ring = IoUring::builder().build(entries.max(2))?;
            Ok(Self {
                ring,
                fd,
                pool,
                inflight: HashMap::new(),
                outstanding: 0,
            })
        }

        /// The device's size in bytes (`lseek(SEEK_END)`, which block
        /// devices answer).
        pub fn capacity_bytes(&self) -> io::Result<u64> {
            Ok(seek(&self.fd, SeekFrom::End(0))?)
        }

        /// Bytes per staging slot.
        #[must_use]
        pub fn slot_bytes(&self) -> usize {
            self.pool.slot_bytes()
        }

        /// Free staging slots.
        #[must_use]
        pub fn free_slots(&self) -> usize {
            self.pool.free.len()
        }

        /// Operations pushed and not reaped.
        #[must_use]
        pub const fn outstanding(&self) -> usize {
            self.outstanding
        }

        /// Copies `bytes` into a free slot (zero-padded to whole pages) and
        /// returns the slot, or `None` when every slot is in flight.
        pub fn stage(&mut self, bytes: &[u8]) -> Option<usize> {
            if bytes.len() > self.pool.slot_bytes() {
                return None;
            }
            let slot = self.pool.free.pop()?;
            self.pool.busy[slot] = true;
            let dst = self.pool.slot_mut(slot);
            dst[..bytes.len()].copy_from_slice(bytes);
            let padded = bytes.len().next_multiple_of(4096);
            dst[bytes.len()..padded].fill(0);
            Some(slot)
        }

        /// Gives a staged slot back without writing it.
        pub fn unstage(&mut self, slot: usize) {
            if self.pool.busy.get(slot).copied() == Some(true) {
                self.pool.busy[slot] = false;
                self.pool.free.push(slot);
            }
        }

        /// Pushes a write of the first `len` bytes of `slot` (a multiple of
        /// 4 KiB) at byte `offset`, with `RWF_DSYNC` when `dsync` (the
        /// superblock and intent slots: durable when complete). The slot
        /// stays busy until the completion is reaped.
        ///
        /// # Errors
        /// [`Full`] when the submission queue has no room; the slot stays
        /// staged.
        pub fn write(
            &mut self,
            slot: usize,
            len: u32,
            offset: u64,
            dsync: bool,
            user_data: u64,
        ) -> Result<(), Full> {
            let slot_bytes = self.pool.slot_bytes();
            if slot >= self.pool.busy.len()
                || !self.pool.busy[slot]
                || len as usize > slot_bytes
                || !(len as usize).is_multiple_of(4096)
            {
                return Err(Full);
            }
            let ptr = self.pool.slot(slot).as_ptr();
            let mut entry =
                opcode::Write::new(types::Fd(self.fd.as_raw_fd()), ptr, len).offset(offset);
            if dsync {
                entry = entry.rw_flags(ReadWriteFlags::DSYNC.bits() as i32);
            }
            let sqe = entry.build().user_data(user_data);
            // SAFETY: `push` requires every address the entry names to stay
            // valid until the operation completes. The entry names `len`
            // bytes of slot `slot` of `self.pool`, which this value owns:
            // the slot is marked busy, so `stage` never hands it out again
            // and `slot_mut` is never called on it until `reap` sees this
            // operation's CQE (or `Drop` has waited for it), and the pool
            // is dropped only after the ring (field order) once nothing is
            // outstanding. The kernel only reads the bytes.
            let pushed = unsafe { self.ring.submission().push(&sqe) };
            if pushed.is_err() {
                return Err(Full);
            }
            self.inflight.insert(user_data, slot);
            self.outstanding += 1;
            Ok(())
        }

        /// Pushes `IORING_OP_FSYNC` with `FSYNC_DATASYNC`: the barrier
        /// every write completed before it is durable behind.
        ///
        /// # Errors
        /// [`Full`] when the submission queue has no room.
        pub fn fsync(&mut self, user_data: u64) -> Result<(), Full> {
            let sqe = opcode::Fsync::new(types::Fd(self.fd.as_raw_fd()))
                .flags(types::FsyncFlags::DATASYNC)
                .build()
                .user_data(user_data);
            // SAFETY: an fsync entry names no memory of ours; the
            // descriptor it names is owned by `self` and open until `self`
            // drops, after every outstanding operation was reaped.
            let pushed = unsafe { self.ring.submission().push(&sqe) };
            if pushed.is_err() {
                return Err(Full);
            }
            self.outstanding += 1;
            Ok(())
        }

        /// Submits every pushed entry to the kernel (`io_uring_enter`).
        ///
        /// # Errors
        /// The `io_uring_enter(2)` error (`EINTR` is retried).
        pub fn submit(&mut self) -> io::Result<usize> {
            loop {
                match self.ring.submit() {
                    Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                    other => return other,
                }
            }
        }

        /// Submits and waits until at least one completion is available, or
        /// returns at once when nothing is outstanding.
        ///
        /// # Errors
        /// The `io_uring_enter(2)` error (`EINTR` is retried).
        pub fn submit_and_wait(&mut self) -> io::Result<usize> {
            if self.outstanding == 0 {
                return self.submit();
            }
            loop {
                match self.ring.submit_and_wait(1) {
                    Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                    other => return other,
                }
            }
        }

        /// Moves every available completion into `out`, releasing the
        /// staging slot of each completed write; returns how many.
        pub fn reap(&mut self, out: &mut Vec<Reaped>) -> usize {
            let mut n = 0;
            let mut cq = self.ring.completion();
            cq.sync();
            let entries: Vec<cqueue::Entry> = cq.by_ref().collect();
            drop(cq);
            for cqe in entries {
                let user_data = cqe.user_data();
                if let Some(slot) = self.inflight.remove(&user_data) {
                    self.pool.busy[slot] = false;
                    self.pool.free.push(slot);
                }
                self.outstanding = self.outstanding.saturating_sub(1);
                out.push(Reaped {
                    user_data,
                    result: cqe.result(),
                });
                n += 1;
            }
            n
        }

        /// Reads `dst.len()` bytes (whole pages) at byte `offset`,
        /// synchronously, through an aligned bounce slot (`O_DIRECT`
        /// refuses an unaligned buffer). Needs one free slot.
        ///
        /// # Errors
        /// The `pread(2)` error, `WouldBlock` when no slot is free, or an
        /// `UnexpectedEof` for a short read.
        pub fn read_at(&mut self, offset: u64, dst: &mut [u8]) -> io::Result<()> {
            if !dst.len().is_multiple_of(4096) {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "reads are whole pages",
                ));
            }
            let slot = self.pool.free.pop().ok_or_else(|| {
                io::Error::new(io::ErrorKind::WouldBlock, "no staging slot free for a read")
            })?;
            self.pool.busy[slot] = true;
            let result = self.read_through(slot, offset, dst);
            self.pool.busy[slot] = false;
            self.pool.free.push(slot);
            result
        }

        fn read_through(&mut self, slot: usize, offset: u64, dst: &mut [u8]) -> io::Result<()> {
            let slot_bytes = self.pool.slot_bytes();
            let mut done = 0usize;
            while done < dst.len() {
                let take = (dst.len() - done).min(slot_bytes);
                let buf = &mut self.pool.slot_mut(slot)[..take];
                let mut got = 0usize;
                while got < take {
                    let n = pread(&self.fd, &mut buf[got..], offset + (done + got) as u64)?;
                    if n == 0 {
                        return Err(io::Error::new(
                            io::ErrorKind::UnexpectedEof,
                            "short read from the device",
                        ));
                    }
                    got += n;
                }
                dst[done..done + take].copy_from_slice(&self.pool.slot(slot)[..take]);
                done += take;
            }
            Ok(())
        }

        /// Discards `len` bytes at `offset` (`BLKDISCARD`), synchronously.
        /// A device without discard support answers `EOPNOTSUPP`.
        ///
        /// # Errors
        /// The `ioctl(2)` error.
        pub fn discard(&self, offset: u64, len: u64) -> io::Result<()> {
            let range = [offset, len];
            // SAFETY: `Setter::new` requires that `BLKDISCARD` takes an
            // input of exactly this type, and `ioctl` that the request is
            // sound for the descriptor: `BLKDISCARD` (`_IO(0x12, 119)`,
            // `include/uapi/linux/fs.h`, handled by `blk_ioctl_discard` in
            // `block/ioctl.c`) takes a pointer to two `u64`s, start and
            // length, which the kernel reads and never writes; `Setter`
            // passes exactly that, and `self.fd` is an open block device.
            unsafe {
                let setter = rustix::ioctl::Setter::<BLKDISCARD, [u64; 2]>::new(range);
                rustix::ioctl::ioctl(&self.fd, setter)
            }?;
            Ok(())
        }
    }

    impl Drop for BlockRing {
        /// Waits for every outstanding operation before the pool goes:
        /// the kernel may still read a staged slot until its completion.
        fn drop(&mut self) {
            let mut sink = Vec::new();
            let mut rounds = 0u32;
            while self.outstanding > 0 && rounds < 1_000_000 {
                if self.submit_and_wait().is_err() {
                    rounds += 1;
                }
                self.reap(&mut sink);
                rounds += 1;
            }
            if self.outstanding > 0 {
                // Never free memory the kernel may still read.
                let pages = std::mem::take(&mut self.pool.pages);
                std::mem::forget(pages);
            }
        }
    }

    impl std::fmt::Debug for BlockRing {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.debug_struct("BlockRing")
                .field("fd", &self.fd.as_raw_fd())
                .field("slot_bytes", &self.pool.slot_bytes())
                .field("free_slots", &self.pool.free.len())
                .field("outstanding", &self.outstanding)
                .finish()
        }
    }
}

#[cfg(target_os = "linux")]
pub use linux::BlockRing;

#[cfg(not(target_os = "linux"))]
mod fallback {
    use core::convert::Infallible;
    use std::io;
    use std::path::Path;

    use super::{Full, Reaped};

    /// A block device on io_uring. io_uring exists on Linux only: on this
    /// host the constructor fails and no value exists.
    pub struct BlockRing {
        never: Infallible,
    }

    impl BlockRing {
        /// Fails with [`io::ErrorKind::Unsupported`].
        ///
        /// # Errors
        /// Always.
        pub fn open(
            path: &Path,
            _entries: u32,
            _slot_bytes: usize,
            _slots: usize,
        ) -> io::Result<Self> {
            Err(io::Error::new(
                io::ErrorKind::Unsupported,
                format!("cannot open {} on io_uring: Linux only", path.display()),
            ))
        }

        /// The device's size.
        pub fn capacity_bytes(&self) -> io::Result<u64> {
            match self.never {}
        }

        /// Bytes per staging slot.
        #[must_use]
        pub fn slot_bytes(&self) -> usize {
            match self.never {}
        }

        /// Free staging slots.
        #[must_use]
        pub fn free_slots(&self) -> usize {
            match self.never {}
        }

        /// Operations pushed and not reaped.
        #[must_use]
        pub fn outstanding(&self) -> usize {
            match self.never {}
        }

        /// Stages bytes.
        pub fn stage(&mut self, _bytes: &[u8]) -> Option<usize> {
            match self.never {}
        }

        /// Gives a slot back.
        pub fn unstage(&mut self, _slot: usize) {
            match self.never {}
        }

        /// Pushes a write.
        pub fn write(
            &mut self,
            _slot: usize,
            _len: u32,
            _offset: u64,
            _dsync: bool,
            _user_data: u64,
        ) -> Result<(), Full> {
            match self.never {}
        }

        /// Pushes an fsync.
        pub fn fsync(&mut self, _user_data: u64) -> Result<(), Full> {
            match self.never {}
        }

        /// Submits.
        pub fn submit(&mut self) -> io::Result<usize> {
            match self.never {}
        }

        /// Submits and waits.
        pub fn submit_and_wait(&mut self) -> io::Result<usize> {
            match self.never {}
        }

        /// Reaps.
        pub fn reap(&mut self, _out: &mut Vec<Reaped>) -> usize {
            match self.never {}
        }

        /// Reads.
        pub fn read_at(&mut self, _offset: u64, _dst: &mut [u8]) -> io::Result<()> {
            match self.never {}
        }

        /// Discards.
        pub fn discard(&self, _offset: u64, _len: u64) -> io::Result<()> {
            match self.never {}
        }
    }

    impl std::fmt::Debug for BlockRing {
        fn fmt(&self, _: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            match self.never {}
        }
    }
}

#[cfg(not(target_os = "linux"))]
pub use fallback::BlockRing;

/// Opens a block device for the engine's backstore: a convenience over
/// [`BlockRing::open`] that sizes the pool for `inflight` stripes of
/// `stripe_bytes` plus four slots for metadata, commits and reads.
pub fn open_block_device(
    path: &Path,
    stripe_bytes: usize,
    inflight: usize,
) -> io::Result<BlockRing> {
    let slots = inflight.max(1) + 4;
    let entries = u32::try_from((slots * 2).next_power_of_two()).unwrap_or(256);
    BlockRing::open(path, entries, stripe_bytes, slots)
}
