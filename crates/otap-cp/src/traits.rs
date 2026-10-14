//! The four system traits (round 3, section 7.2; Step 3 brief): everything
//! the engine needs from the machine, so `dbladed` implements them on
//! io_uring, ublk, the clock and the sockets, and `otap-sim` fakes them.
//!
//! Writes are asynchronous: the engine submits with a [`Tag`] and later reads
//! completions with [`Backstore::poll`]. Reads are synchronous, because the
//! engine only reads what a committed consistency point named, on a path
//! where a reactor would block anyway. A [`Busy`] answer means the queue is
//! full and the call is retried on a later tick; nothing was done.

use core::fmt;

use otap_format::{IntentSlot, SuperblockSlot};

/// The tag of an asynchronous operation; unique while the operation is in
/// flight.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Tag(pub u64);

/// A completion reported by [`Backstore::poll`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Done {
    /// The operation.
    pub tag: Tag,
    /// Its outcome.
    pub result: Result<(), IoError>,
}

/// "Try again on a later tick": the submission queue is full; nothing was
/// done.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Busy;

impl fmt::Display for Busy {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("busy")
    }
}

impl std::error::Error for Busy {}

/// Why an I/O failed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum IoError {
    /// The device reported an error.
    Device {
        /// The errno, or 0 when unknown.
        errno: i32,
    },
    /// An address beyond the device.
    OutOfRange {
        /// The first block.
        pba: u64,
        /// The length in bytes.
        bytes: usize,
    },
    /// A length that is not whole blocks, or a buffer of another length.
    BadLength {
        /// The length offered.
        bytes: usize,
    },
    /// The data arrived with the wrong CRC (a front-door copy, or a client
    /// buffer gone).
    Corrupt,
}

impl fmt::Display for IoError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Device { errno } => write!(f, "device error (errno {errno})"),
            Self::OutOfRange { pba, bytes } => {
                write!(f, "{bytes} bytes at block {pba} are beyond the device")
            }
            Self::BadLength { bytes } => write!(f, "{bytes} bytes is not whole blocks"),
            Self::Corrupt => f.write_str("data corrupt"),
        }
    }
}

impl std::error::Error for IoError {}

/// The device class an operation targets.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum DevClass {
    /// The pool (data and, without a metadata class, metadata too).
    Pool,
    /// The metadata class, when the profile has one.
    Metadata,
}

/// The media behind the pool.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Media {
    /// Spinning disks: a 10 s CP timer by default.
    Hdd,
    /// Flash: a 5 s CP timer by default.
    Flash,
}

/// How the write-active set is kept (R27).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum NvfailIntent {
    /// Every present, writable volume counts as active; no intent slots are
    /// written. The v0.1 default.
    Conservative,
    /// A volume is active from its first write after idle (an intent slot is
    /// written before that write is acknowledged) until two consistency
    /// points pass without a write.
    Precise,
}

/// The geometry the engine lays the aggregate out with.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Geometry {
    /// The full stripe: the unit of every write, summary included.
    pub stripe_bytes: u32,
    /// The segment: a whole number of stripes, at most 256 MiB.
    pub segment_bytes: u64,
    /// The device, in bytes.
    pub capacity_bytes: u64,
    /// `true` when a completed write is durable only after a flush that the
    /// device may not honour (C07): a hardware RAID virtual drive or any
    /// LUN whose cache the kernel cannot see. What the engine does with
    /// such a device is the profile's [`FlushPolicy`] (E19, closed by the
    /// owner's Step 4 ruling; E21 in `docs/07-engineering-decisions.md`).
    pub flush_opaque: bool,
}

/// How the engine treats a backstore whose flushes it cannot verify (the
/// owner's E19 ruling, Step 4).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum FlushPolicy {
    /// Strict: the backstore must honour FUA and flushes; a flush-opaque
    /// geometry is refused at format and boot.
    RequireFuaOrFlush,
    /// Standard and homelab mode, the default for hardware RAID and opaque
    /// LUNs: the engine issues its host-side barrier (the flush before the
    /// superblock commit) and trusts it, publishes a structured operator
    /// warning in the pool status, defers segment reuse one more CP, and
    /// relies on nvfail to quarantine volumes after a power loss that lost
    /// the controller's cache.
    #[default]
    TrustBarrierWithNvfail,
}

/// What the engine does with a segment it frees (D9).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum DiscardPolicy {
    /// Nothing: free segments are reused by whole-stripe overwrite
    /// (rotational media and hardware RAID virtual drives).
    #[default]
    Never,
    /// A whole-segment discard (`BLKDISCARD`) when a segment is freed
    /// (flash and NVMe).
    WholeSegment,
}

/// The profile that drives the consistency-point engine (section 7.2's
/// `MediaProfile`, the fields Steps 3 and 4 use).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct MediaProfile {
    /// The media.
    pub media: Media,
    /// The geometry.
    pub geom: Geometry,
    /// The CP timer.
    pub cp_timer_ns: u64,
    /// Stripe writes in flight at once, 1 to 64.
    pub inflight_stripes: u16,
    /// Unflushed bytes above which a CP follows the previous one at once.
    pub back_to_back_bytes: u64,
    /// The write-active policy.
    pub nvfail_intent: NvfailIntent,
    /// What to do with a flush-opaque geometry (E19 / E21).
    pub flush_policy: FlushPolicy,
    /// What to do with a freed segment (D9).
    pub discard: DiscardPolicy,
}

impl MediaProfile {
    /// The flash profile for a lab-sized aggregate: 5 s timer, 8 stripes in
    /// flight, back to back above 1 GiB, conservative intents, whole-segment
    /// discards, the barrier trusted on an opaque geometry.
    #[must_use]
    pub const fn flash(geom: Geometry) -> Self {
        Self {
            media: Media::Flash,
            geom,
            cp_timer_ns: 5_000_000_000,
            inflight_stripes: 8,
            back_to_back_bytes: 1 << 30,
            nvfail_intent: NvfailIntent::Conservative,
            flush_policy: FlushPolicy::TrustBarrierWithNvfail,
            discard: DiscardPolicy::WholeSegment,
        }
    }

    /// The HDD profile: 10 s timer, 16 stripes in flight, no discards, the
    /// barrier trusted on an opaque geometry.
    #[must_use]
    pub const fn hdd(geom: Geometry) -> Self {
        Self {
            media: Media::Hdd,
            geom,
            cp_timer_ns: 10_000_000_000,
            inflight_stripes: 16,
            back_to_back_bytes: 1 << 30,
            nvfail_intent: NvfailIntent::Conservative,
            flush_policy: FlushPolicy::TrustBarrierWithNvfail,
            discard: DiscardPolicy::Never,
        }
    }

    /// `true` when the geometry is flush-opaque and the policy trusts the
    /// barrier: the engine runs with the E19 warning and the deferred reuse.
    #[must_use]
    pub const fn trusts_opaque_barrier(&self) -> bool {
        self.geom.flush_opaque && matches!(self.flush_policy, FlushPolicy::TrustBarrierWithNvfail)
    }
}

/// The block device under the aggregate: md, a single device, or a hardware
/// virtual drive, all behind one interface. Addresses are 4 KiB blocks
/// (`pba`); every write is whole blocks, and the stripe writes are whole
/// stripes.
pub trait Backstore {
    /// The profile.
    fn profile(&self) -> &MediaProfile;

    /// Submits a data stripe: `image.len()` is one full stripe, written at
    /// `pba` as one direct write (`WRITE_FIXED` on io_uring).
    fn write_stripe(&mut self, pba: u64, image: &[u8], tag: Tag) -> Result<(), Busy>;

    /// Submits a whole stripe of metadata pages at `pba`.
    fn write_meta(&mut self, pba: u64, image: &[u8], tag: Tag) -> Result<(), Busy>;

    /// Submits an empty flush of a device class: every write completed
    /// before this call is durable when the flush completes.
    fn flush(&mut self, class: DevClass, tag: Tag) -> Result<(), Busy>;

    /// Submits a superblock slot at `pba` as one FUA write (`RWF_DSYNC`):
    /// durable when it completes.
    fn commit(&mut self, pba: u64, slot: &SuperblockSlot, tag: Tag) -> Result<(), Busy>;

    /// Submits an intent slot at `pba` as one FUA write.
    fn write_intent(&mut self, pba: u64, slot: &IntentSlot, tag: Tag) -> Result<(), Busy>;

    /// Reads `dst.len()` bytes (whole blocks) at `pba`.
    fn read(&mut self, pba: u64, dst: &mut [u8]) -> Result<(), IoError>;

    /// Submits a discard of `bytes` bytes at `pba` (a whole freed segment,
    /// D9): the device may return zeros or the old data for the range
    /// afterwards; nothing the engine references lives there. A backstore
    /// without discard support completes it as a no-op.
    fn discard_segment(&mut self, pba: u64, bytes: u64, tag: Tag) -> Result<(), Busy>;

    /// Moves every completion that has arrived into `out`; returns how many.
    fn poll(&mut self, out: &mut Vec<Done>) -> usize;
}

/// A front-door request's identity.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct ReqId(pub u64);

/// What a client asked for.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum RequestKind {
    /// Write `blocks` blocks at `lba` of `vol`; the data comes through
    /// [`FrontDoor::copy_in`].
    Write {
        /// The volume.
        vol: u32,
        /// The first LBA.
        lba: u64,
        /// Blocks, at least 1.
        blocks: u32,
    },
    /// Read `blocks` blocks at `lba` of `vol`; the data goes through
    /// [`FrontDoor::copy_out`].
    Read {
        /// The volume.
        vol: u32,
        /// The first LBA.
        lba: u64,
        /// Blocks, at least 1.
        blocks: u32,
    },
    /// A flush: an ordering barrier, complete once every earlier
    /// acknowledged write of the volume is placed (which it already is).
    Flush {
        /// The volume.
        vol: u32,
    },
    /// Discard `blocks` blocks at `lba` of `vol`: the blocks read as zeros
    /// afterwards and the map holds holes (thin provisioning, D9). Held in
    /// the arena as one hole record until its CP commits, like a write.
    Discard {
        /// The volume.
        vol: u32,
        /// The first LBA.
        lba: u64,
        /// Blocks, at least 1.
        blocks: u32,
    },
    /// Write zeros over `blocks` blocks at `lba` of `vol`: served exactly
    /// like a discard (a hole reads as zeros).
    WriteZeroes {
        /// The volume.
        vol: u32,
        /// The first LBA.
        lba: u64,
        /// Blocks, at least 1.
        blocks: u32,
    },
}

/// A front-door request.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Request {
    /// Identity.
    pub id: ReqId,
    /// What was asked.
    pub kind: RequestKind,
}

impl Request {
    /// The volume the request names.
    #[must_use]
    pub const fn vol(&self) -> u32 {
        match self.kind {
            RequestKind::Write { vol, .. }
            | RequestKind::Read { vol, .. }
            | RequestKind::Flush { vol }
            | RequestKind::Discard { vol, .. }
            | RequestKind::WriteZeroes { vol, .. } => vol,
        }
    }

    /// `true` for a discard or a write of zeros: a request the engine
    /// serves as a hole punch.
    #[must_use]
    pub const fn is_punch(&self) -> bool {
        matches!(
            self.kind,
            RequestKind::Discard { .. } | RequestKind::WriteZeroes { .. }
        )
    }
}

/// How a request ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum Status {
    /// Done; for a write, the sequence number the engine gave it.
    Ok,
    /// The volume is not present.
    NoSuchVolume,
    /// The range runs past the volume.
    OutOfRange,
    /// The volume is held by nvfail: the engine answers EIO until the clear.
    NvFailed,
    /// Data on disk failed its CRC.
    Corrupt,
    /// The engine has shut down.
    Stopped,
    /// The copy in or out failed.
    Failed,
    /// The pool has no free segment: the write could never be made durable.
    NoSpace,
    /// The request is longer than the engine accepts: above
    /// [`MAX_REQUEST_BLOCKS`](crate::engine::MAX_REQUEST_BLOCKS) blocks (or
    /// [`MAX_PUNCH_BLOCKS`](crate::engine::MAX_PUNCH_BLOCKS) for a discard),
    /// or longer than the arena.
    TooLarge,
}

/// The client side: ublk in production, a request script in the simulator.
pub trait FrontDoor {
    /// The next request, if any.
    fn fetch(&mut self) -> Option<Request>;

    /// Copies a write's data into `dst`, which is `blocks * 4096` bytes of
    /// arena memory.
    fn copy_in(&mut self, req: &Request, dst: &mut [u8]) -> Result<(), IoError>;

    /// Copies a read's data from `src` to the client.
    fn copy_out(&mut self, req: &Request, src: &[u8]) -> Result<(), IoError>;

    /// Completes a request. `seq` is the write's sequence number, 0 for
    /// anything else.
    fn complete(&mut self, req: Request, status: Status, seq: u64);
}

/// Time.
pub trait Clock {
    /// Monotonic nanoseconds since an arbitrary origin; never runs backwards
    /// while the process lives.
    fn mono_ns(&self) -> u64;

    /// Wall-clock nanoseconds since the Unix epoch.
    fn wall_ns(&self) -> u64;
}

/// A peer on the network, for the replication stream.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct EndpointId(pub u32);

/// Why a send was refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum NetError {
    /// The endpoint's queue is full; nothing was queued, retry later.
    Busy,
    /// No such endpoint.
    Unreachable,
}

impl fmt::Display for NetError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Busy => f.write_str("queue full"),
            Self::Unreachable => f.write_str("no such endpoint"),
        }
    }
}

impl std::error::Error for NetError {}

/// The network, as the replication transport sees it: ordered, reliable
/// byte streams to named endpoints. `TlsTcp` implements it on sockets; the
/// simulator's loopback delivers in order with partitions and delays. A
/// chunk that `send` accepted is delivered in order; nothing is dropped
/// silently (a partition holds chunks until it heals).
pub trait Net {
    /// Queues `bytes` for `to`.
    fn send(&mut self, to: EndpointId, bytes: &[u8]) -> Result<(), NetError>;

    /// Moves every delivered chunk into `out`, in arrival order; returns how
    /// many.
    fn poll_recv(&mut self, out: &mut Vec<(EndpointId, Vec<u8>)>) -> usize;
}
