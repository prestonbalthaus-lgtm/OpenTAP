//! The file-backed backstore: synchronous, portable, honest.

use std::fs::{File, OpenOptions};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::Path;

use otap_cp::{Backstore, Busy, DevClass, Done, IoError, MediaProfile, Tag};
use otap_format::{IntentSlot, IntoBytes, SuperblockSlot, BLOCK_SIZE};

/// Counters.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct FileStats {
    /// Stripe writes.
    pub stripes: u64,
    /// Metadata writes.
    pub metas: u64,
    /// Flushes.
    pub flushes: u64,
    /// Superblock commits.
    pub commits: u64,
    /// Intent writes.
    pub intents: u64,
    /// Reads.
    pub reads: u64,
    /// Segment discards (completed as no-ops).
    pub discards: u64,
    /// Bytes written.
    pub bytes_written: u64,
}

/// An ordinary file as the aggregate's device.
#[derive(Debug)]
pub struct FileBackstore {
    file: File,
    profile: MediaProfile,
    completions: Vec<Done>,
    stats: FileStats,
}

fn io_err(e: &io::Error) -> IoError {
    IoError::Device {
        errno: e.raw_os_error().unwrap_or(0),
    }
}

impl FileBackstore {
    /// Creates (or truncates) `path` to the profile's capacity and opens
    /// it for the engine.
    ///
    /// # Errors
    /// The file system's error.
    pub fn create(path: &Path, profile: MediaProfile) -> io::Result<Self> {
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(true)
            .open(path)?;
        file.set_len(profile.geom.capacity_bytes)?;
        Ok(Self::over(file, profile))
    }

    /// Opens an existing `path` (a file, or a block device, whose size is
    /// its end), which must be at least the profile's capacity.
    ///
    /// # Errors
    /// The file system's error, or `InvalidInput` for a short file.
    pub fn open(path: &Path, profile: MediaProfile) -> io::Result<Self> {
        let mut file = OpenOptions::new().read(true).write(true).open(path)?;
        // A block device's metadata says 0 bytes; its end says its size.
        let len = match file.metadata()?.len() {
            0 => file.seek(SeekFrom::End(0))?,
            len => len,
        };
        if len < profile.geom.capacity_bytes {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!(
                    "{} holds {len} bytes; the profile needs {}",
                    path.display(),
                    profile.geom.capacity_bytes
                ),
            ));
        }
        Ok(Self::over(file, profile))
    }

    /// Wraps an open file.
    #[must_use]
    pub fn over(file: File, profile: MediaProfile) -> Self {
        Self {
            file,
            profile,
            completions: Vec::new(),
            stats: FileStats::default(),
        }
    }

    /// Counters.
    #[must_use]
    pub const fn stats(&self) -> FileStats {
        self.stats
    }

    /// The file, for a test that wants to damage it.
    pub fn file_mut(&mut self) -> &mut File {
        &mut self.file
    }

    fn check(&self, pba: u64, bytes: usize) -> Result<(), IoError> {
        if bytes == 0 || !bytes.is_multiple_of(BLOCK_SIZE) {
            return Err(IoError::BadLength { bytes });
        }
        let end = pba
            .checked_mul(BLOCK_SIZE as u64)
            .and_then(|o| o.checked_add(bytes as u64));
        if end.is_none_or(|e| e > self.profile.geom.capacity_bytes) {
            return Err(IoError::OutOfRange { pba, bytes });
        }
        Ok(())
    }

    fn write_at(&mut self, pba: u64, bytes: &[u8]) -> Result<(), IoError> {
        self.check(pba, bytes.len())?;
        self.file
            .seek(SeekFrom::Start(pba * BLOCK_SIZE as u64))
            .map_err(|e| io_err(&e))?;
        self.file.write_all(bytes).map_err(|e| io_err(&e))?;
        self.stats.bytes_written += bytes.len() as u64;
        Ok(())
    }

    fn sync(&mut self) -> Result<(), IoError> {
        self.file.sync_data().map_err(|e| io_err(&e))
    }

    fn done(&mut self, tag: Tag, result: Result<(), IoError>) {
        self.completions.push(Done { tag, result });
    }
}

impl Backstore for FileBackstore {
    fn profile(&self) -> &MediaProfile {
        &self.profile
    }

    fn write_stripe(&mut self, pba: u64, image: &[u8], tag: Tag) -> Result<(), Busy> {
        self.stats.stripes += 1;
        let result = self.write_at(pba, image);
        self.done(tag, result);
        Ok(())
    }

    fn write_meta(&mut self, pba: u64, image: &[u8], tag: Tag) -> Result<(), Busy> {
        self.stats.metas += 1;
        let result = self.write_at(pba, image);
        self.done(tag, result);
        Ok(())
    }

    fn flush(&mut self, _class: DevClass, tag: Tag) -> Result<(), Busy> {
        self.stats.flushes += 1;
        let result = self.sync();
        self.done(tag, result);
        Ok(())
    }

    fn commit(&mut self, pba: u64, slot: &SuperblockSlot, tag: Tag) -> Result<(), Busy> {
        self.stats.commits += 1;
        let result = self
            .write_at(pba, slot.as_bytes())
            .and_then(|()| self.sync());
        self.done(tag, result);
        Ok(())
    }

    fn write_intent(&mut self, pba: u64, slot: &IntentSlot, tag: Tag) -> Result<(), Busy> {
        self.stats.intents += 1;
        let result = self
            .write_at(pba, slot.as_bytes())
            .and_then(|()| self.sync());
        self.done(tag, result);
        Ok(())
    }

    fn read(&mut self, pba: u64, dst: &mut [u8]) -> Result<(), IoError> {
        self.stats.reads += 1;
        self.check(pba, dst.len())?;
        self.file
            .seek(SeekFrom::Start(pba * BLOCK_SIZE as u64))
            .map_err(|e| io_err(&e))?;
        self.file.read_exact(dst).map_err(|e| io_err(&e))
    }

    fn discard_segment(&mut self, pba: u64, bytes: u64, tag: Tag) -> Result<(), Busy> {
        // A file keeps what it holds; the engine references nothing there.
        self.stats.discards += 1;
        let result = usize::try_from(bytes)
            .map_err(|_| IoError::BadLength { bytes: usize::MAX })
            .and_then(|b| self.check(pba, b));
        self.done(tag, result);
        Ok(())
    }

    fn poll(&mut self, out: &mut Vec<Done>) -> usize {
        let n = self.completions.len();
        out.append(&mut self.completions);
        n
    }
}
