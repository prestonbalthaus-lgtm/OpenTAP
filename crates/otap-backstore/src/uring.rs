//! The io_uring backstore over a block device (Linux).

use std::collections::HashMap;
use std::io;
use std::path::Path;

use otap_cp::{Backstore, Busy, DevClass, Done, IoError, MediaProfile, Tag};
use otap_format::{IntentSlot, IntoBytes, SuperblockSlot, BLOCK_SIZE};
use otap_sys::{open_block_device, BlockRing, Reaped};

/// Counters.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct UringStats {
    /// Stripe writes submitted.
    pub stripes: u64,
    /// Metadata writes submitted.
    pub metas: u64,
    /// Flushes submitted.
    pub flushes: u64,
    /// Superblock commits submitted.
    pub commits: u64,
    /// Intent writes submitted.
    pub intents: u64,
    /// Reads served.
    pub reads: u64,
    /// Segment discards issued.
    pub discards: u64,
    /// Submissions refused with `Busy` (no slot or no ring room).
    pub busy: u64,
    /// `io_uring_enter` calls that failed and were retried.
    pub submit_errors: u64,
}

/// A block device on io_uring.
#[derive(Debug)]
pub struct UringBackstore {
    ring: BlockRing,
    profile: MediaProfile,
    /// Bytes each in-flight write must report, by tag.
    expected: HashMap<u64, u32>,
    completions: Vec<Done>,
    stats: UringStats,
}

impl UringBackstore {
    /// Opens the device at `path` for `profile`: a staging pool of
    /// `inflight_stripes` stripes plus four, and the device must hold the
    /// profile's capacity.
    ///
    /// # Errors
    /// The open or ring error, `Unsupported` off Linux, or `InvalidInput`
    /// when the device is smaller than the profile says.
    pub fn open(path: &Path, profile: MediaProfile) -> io::Result<Self> {
        let stripe = profile.geom.stripe_bytes as usize;
        let ring = open_block_device(
            path,
            stripe,
            usize::from(profile.inflight_stripes.clamp(1, 64)),
        )?;
        let capacity = ring.capacity_bytes()?;
        if capacity < profile.geom.capacity_bytes {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!(
                    "{} holds {capacity} bytes; the profile needs {}",
                    path.display(),
                    profile.geom.capacity_bytes
                ),
            ));
        }
        // The pool is page-aligned memory the kernel reads directly; its
        // slots are stripe-sized, which every write here fits.
        let _ = ring.free_slots();
        Ok(Self {
            ring,
            profile,
            expected: HashMap::new(),
            completions: Vec::new(),
            stats: UringStats::default(),
        })
    }

    /// Counters.
    #[must_use]
    pub const fn stats(&self) -> UringStats {
        self.stats
    }

    /// Operations submitted and not yet reported.
    #[must_use]
    pub fn outstanding(&self) -> usize {
        self.ring.outstanding()
    }

    fn check(&self, pba: u64, bytes: usize) -> Result<(), IoError> {
        if !bytes.is_multiple_of(BLOCK_SIZE) {
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

    /// Stages and pushes a write of `image` (padded with zeros to `len`
    /// bytes when `len` is longer) at `pba`.
    fn write(
        &mut self,
        pba: u64,
        image: &[u8],
        len: usize,
        dsync: bool,
        tag: Tag,
    ) -> Result<(), Busy> {
        if let Err(e) = self.check(pba, len) {
            self.completions.push(Done {
                tag,
                result: Err(e),
            });
            return Ok(());
        }
        let Some(slot) = self.ring.stage(image) else {
            self.stats.busy += 1;
            return Err(Busy);
        };
        let Ok(len32) = u32::try_from(len) else {
            self.ring.unstage(slot);
            self.completions.push(Done {
                tag,
                result: Err(IoError::BadLength { bytes: len }),
            });
            return Ok(());
        };
        match self
            .ring
            .write(slot, len32, pba * BLOCK_SIZE as u64, dsync, tag.0)
        {
            Ok(()) => {
                self.expected.insert(tag.0, len32);
                Ok(())
            }
            Err(_) => {
                self.ring.unstage(slot);
                self.stats.busy += 1;
                Err(Busy)
            }
        }
    }

    fn report(&mut self, reaped: Reaped) {
        let tag = Tag(reaped.user_data);
        let result = if reaped.result < 0 {
            Err(IoError::Device {
                errno: -reaped.result,
            })
        } else if let Some(expected) = self.expected.remove(&reaped.user_data) {
            if reaped.result as u32 == expected {
                Ok(())
            } else {
                Err(IoError::BadLength {
                    bytes: reaped.result as usize,
                })
            }
        } else {
            Ok(())
        };
        self.completions.push(Done { tag, result });
    }
}

impl Backstore for UringBackstore {
    fn profile(&self) -> &MediaProfile {
        &self.profile
    }

    fn write_stripe(&mut self, pba: u64, image: &[u8], tag: Tag) -> Result<(), Busy> {
        self.stats.stripes += 1;
        self.write(pba, image, image.len(), false, tag)
    }

    fn write_meta(&mut self, pba: u64, image: &[u8], tag: Tag) -> Result<(), Busy> {
        self.stats.metas += 1;
        self.write(pba, image, image.len(), false, tag)
    }

    fn flush(&mut self, _class: DevClass, tag: Tag) -> Result<(), Busy> {
        self.stats.flushes += 1;
        match self.ring.fsync(tag.0) {
            Ok(()) => Ok(()),
            Err(_) => {
                self.stats.busy += 1;
                Err(Busy)
            }
        }
    }

    fn commit(&mut self, pba: u64, slot: &SuperblockSlot, tag: Tag) -> Result<(), Busy> {
        self.stats.commits += 1;
        // One full stripe, written whole with RWF_DSYNC (R26): the layout
        // reserves whole stripes for every slot.
        let stripe = self.profile.geom.stripe_bytes as usize;
        self.write(
            pba,
            slot.as_bytes(),
            stripe.max(slot.as_bytes().len()),
            true,
            tag,
        )
    }

    fn write_intent(&mut self, pba: u64, slot: &IntentSlot, tag: Tag) -> Result<(), Busy> {
        self.stats.intents += 1;
        let stripe = self.profile.geom.stripe_bytes as usize;
        self.write(
            pba,
            slot.as_bytes(),
            stripe.max(slot.as_bytes().len()),
            true,
            tag,
        )
    }

    fn read(&mut self, pba: u64, dst: &mut [u8]) -> Result<(), IoError> {
        self.stats.reads += 1;
        self.check(pba, dst.len())?;
        self.ring
            .read_at(pba * BLOCK_SIZE as u64, dst)
            .map_err(|e| IoError::Device {
                errno: e.raw_os_error().unwrap_or(0),
            })
    }

    fn discard_segment(&mut self, pba: u64, bytes: u64, tag: Tag) -> Result<(), Busy> {
        self.stats.discards += 1;
        let result = match usize::try_from(bytes) {
            Err(_) => Err(IoError::BadLength { bytes: usize::MAX }),
            Ok(len) => self.check(pba, len).and_then(|()| {
                match self.ring.discard(pba * BLOCK_SIZE as u64, bytes) {
                    Ok(()) => Ok(()),
                    // A device without discard support: nothing to do.
                    Err(e) if e.kind() == io::ErrorKind::Unsupported => Ok(()),
                    Err(e) if e.raw_os_error() == Some(95) => Ok(()),
                    Err(e) => Err(IoError::Device {
                        errno: e.raw_os_error().unwrap_or(0),
                    }),
                }
            }),
        };
        self.completions.push(Done { tag, result });
        Ok(())
    }

    fn poll(&mut self, out: &mut Vec<Done>) -> usize {
        if self.ring.submit().is_err() {
            self.stats.submit_errors += 1;
        }
        let mut reaped = Vec::new();
        self.ring.reap(&mut reaped);
        for r in reaped {
            self.report(r);
        }
        let n = self.completions.len();
        out.append(&mut self.completions);
        n
    }
}
