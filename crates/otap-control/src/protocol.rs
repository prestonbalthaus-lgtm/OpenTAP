//! The request and reply types.

use core::fmt;

use serde::{Deserialize, Serialize};

/// The protocol version a client and a daemon must agree on; carried by
/// `Ping` and checked by the client.
///
/// Every request, reply and report refuses unknown fields, so a field
/// added to an existing shape is a change both peers would refuse, and it
/// bumps the version. A new request or reply variant does not: a daemon
/// that does not know a request answers it `BadRequest`, and a reply
/// variant reaches only a client that sent the request it answers. Step 6B
/// added `DeviceList` and `DeviceReap` (answered `Devices` and `Reaped`)
/// that way, under version 1.
pub const PROTOCOL_VERSION: u32 = 1;

/// The most blocks one `IoWrite` or `IoRead` moves (1 MiB).
pub const MAX_IO_BLOCKS: u32 = 256;

/// The media a pool is formatted for, when the device cannot say.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MediaKind {
    /// Spinning disks: a 10 s CP timer by default, no discards.
    Hdd,
    /// Flash: a 5 s CP timer by default, whole-segment discards.
    Flash,
}

/// Where the arena's memory comes from (S5-1).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum ArenaKind {
    /// A file on a hugetlbfs mount of 2 MiB pages, re-attached after a
    /// daemon crash (Linux).
    Hugetlb {
        /// The file.
        path: String,
    },
    /// Aligned heap memory: no reservation, no survival across the
    /// process.
    Heap,
}

/// What `otapctl` asks of `dbladed`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case", deny_unknown_fields)]
pub enum Request {
    /// Is anyone there, and which protocol.
    Ping,
    /// Formats `device` as a new pool. Refused while a pool is booted.
    PoolFormat {
        /// A block device or a file (created at `size_bytes` when it does
        /// not exist and a size is given).
        device: String,
        /// The size to create a file with, when it does not exist.
        size_bytes: Option<u64>,
        /// The media, when the device cannot say (files).
        media: Option<MediaKind>,
        /// A stripe size override, in bytes.
        stripe_bytes: Option<u32>,
        /// A segment size override, in bytes.
        segment_bytes: Option<u64>,
        /// Treat the device as flush-opaque (a hardware RAID virtual disk).
        flush_opaque: Option<bool>,
        /// Refuse a flush-opaque device (`RequireFuaOrFlush`).
        strict_flush: bool,
        /// Overwrite a device that already holds a pool (refused otherwise).
        force: bool,
    },
    /// Boots the pool on `device` with an arena of `arena_bytes` bytes.
    PoolBoot {
        /// The device the pool is on.
        device: String,
        /// The media override, as at format.
        media: Option<MediaKind>,
        /// Stripe override, as at format (a difference is refused).
        stripe_bytes: Option<u32>,
        /// Segment override, as at format.
        segment_bytes: Option<u64>,
        /// Flush-opaque override, as at format.
        flush_opaque: Option<bool>,
        /// Strict flush policy.
        strict_flush: bool,
        /// Arena bytes (a multiple of 2 MiB, at least 64 MiB).
        arena_bytes: u64,
        /// Where the arena lives.
        arena: ArenaKind,
        /// The CP timer in seconds, 1 to 15.
        cp_interval_secs: u32,
    },
    /// The pool as the engine sees it.
    PoolStatus,
    /// A consistency point now; returns when it is durable.
    PoolSync,
    /// A cleaning consistency point now; returns when it is durable.
    PoolClean,
    /// A clean shutdown of the pool; the daemon keeps running.
    PoolStop,
    /// A clean shutdown of the pool (when one is booted) and the daemon's
    /// exit.
    DaemonStop,
    /// Creates a named, thin volume of `size_bytes` (rounded up to whole
    /// 4 KiB blocks); online once durable.
    VolumeCreate {
        /// The name.
        name: String,
        /// The size.
        size_bytes: u64,
    },
    /// Every volume.
    VolumeList,
    /// One volume.
    VolumeShow {
        /// The name.
        name: String,
    },
    /// Deletes a volume (offline, no snapshots).
    VolumeDelete {
        /// The name.
        name: String,
    },
    /// Brings a volume online or takes it offline.
    VolumeOnline {
        /// The name.
        name: String,
        /// Online, or offline.
        online: bool,
    },
    /// Resizes a volume (a shrink needs it offline).
    VolumeResize {
        /// The name.
        name: String,
        /// The new size.
        size_bytes: u64,
    },
    /// Renames a volume.
    VolumeRename {
        /// The name.
        name: String,
        /// The new name.
        new_name: String,
    },
    /// Clears nvfail on a volume; returns when the clear is durable.
    VolumeNvfailClear {
        /// The name.
        name: String,
    },
    /// Takes a named snapshot of a volume; returns when durable.
    SnapshotCreate {
        /// The volume.
        volume: String,
        /// The snapshot's name, unique within the volume.
        name: String,
    },
    /// Every snapshot, or those of one volume.
    SnapshotList {
        /// The volume, or every volume.
        volume: Option<String>,
    },
    /// Deletes a snapshot; returns when the deletion is durable.
    SnapshotDelete {
        /// The volume.
        volume: String,
        /// The snapshot.
        name: String,
    },
    /// Rolls a volume back to a snapshot: the volume is taken offline,
    /// its writes flushed, newer snapshots deleted when `delete_newer`,
    /// the rollback committed, and the volume brought back online.
    SnapshotRollback {
        /// The volume.
        volume: String,
        /// The snapshot.
        name: String,
        /// Delete the snapshots newer than `name` first (else refused).
        delete_newer: bool,
    },
    /// Sets a snapshot's pin flags (`PIN_*` bits of the format).
    SnapshotPin {
        /// The volume.
        volume: String,
        /// The snapshot.
        name: String,
        /// The flags.
        flags: u8,
    },
    /// Makes a snapshot the volume's replication base (superseding the
    /// previous one), or releases the base with `None`.
    SnapshotReplicationBase {
        /// The volume.
        volume: String,
        /// The snapshot, or none.
        name: Option<String>,
    },
    /// Writes whole blocks (hexadecimal) at `lba` of a volume through the
    /// daemon's own front door (the smoke test's data path).
    IoWrite {
        /// The volume.
        volume: String,
        /// The first block.
        lba: u64,
        /// The data, `blocks * 4096` bytes as hexadecimal.
        data_hex: String,
    },
    /// Reads `blocks` blocks at `lba` of a volume.
    IoRead {
        /// The volume.
        volume: String,
        /// The first block.
        lba: u64,
        /// Blocks, at most [`MAX_IO_BLOCKS`].
        blocks: u32,
    },
    /// Discards `blocks` blocks at `lba` of a volume.
    IoPunch {
        /// The volume.
        volume: String,
        /// The first block.
        lba: u64,
        /// Blocks.
        blocks: u32,
    },
    /// The ublk devices on the host and how each stands with this daemon
    /// (Step 6B); answered with or without a booted pool. A struct variant
    /// with no fields, not a unit one: serde lets an internally tagged unit
    /// variant ignore fields it does not know, which `deny_unknown_fields`
    /// is meant to refuse.
    DeviceList {},
    /// Deletes ublk devices that no daemon will re-attach: the one named by
    /// `dev_id` (stale, or recoverable by a pool not booted here), or, with
    /// `stale`, every stale one. Exactly one of the two. A device that is
    /// served, belongs to another pool or other software, or is still
    /// quiescing is never deleted.
    DeviceReap {
        /// The device `N` of `/dev/ublkbN`.
        dev_id: Option<u32>,
        /// Every stale device.
        stale: bool,
    },
}

/// How a ublk device on the host stands with the daemon (Step 6B).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DeviceState {
    /// This daemon serves it for a volume of the booted pool.
    Serving,
    /// Its server is dead and it carries an OpenTAP stamp: a daemon booting
    /// its pool re-attaches it, keeping its number.
    Recoverable,
    /// Its server is dead and nothing will re-attach it; `device reap`
    /// deletes it (the requests it holds fail).
    Stale,
    /// Not this daemon's: another live process serves it, another pool owns
    /// it, or other software may still be setting it up.
    Foreign,
}

/// A ublk device on the host as the daemon reports it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DeviceReport {
    /// `N` of `/dev/ublkbN`.
    pub dev_id: u32,
    /// The block device, `/dev/ublkbN`.
    pub path: String,
    /// How it stands with the daemon.
    pub state: DeviceState,
    /// Why, for the operator.
    pub reason: String,
    /// The driver's state: `live`, `quiesced` or `dead`.
    pub kernel_state: String,
    /// The server pid the driver records (0 before a start, -1 after a
    /// stop).
    pub pid: i32,
    /// That pid is a live process.
    pub pid_alive: bool,
    /// The volume it exposes: the one the daemon serves it for, or the one
    /// its OpenTAP stamp names.
    pub volume_index: Option<u32>,
    /// That volume's name, when it is a volume of the booted pool.
    pub volume: Option<String>,
    /// The fingerprint of the OpenTAP pool its stamp names, as eight hex
    /// digits.
    pub pool: Option<String>,
    /// The capacity in bytes, when the driver reported it.
    pub size_bytes: Option<u64>,
    /// Created with `UBLK_F_USER_RECOVERY_REISSUE`: the requests a dead
    /// server held are delivered again after a re-attach.
    pub reissue: bool,
}

/// A volume as the daemon reports it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct VolumeReport {
    /// The engine's index.
    pub index: u32,
    /// The catalogue name, if any.
    pub name: Option<String>,
    /// The size in bytes.
    pub size_bytes: u64,
    /// Serving I/O.
    pub serving: bool,
    /// Taken offline by the operator.
    pub offline: bool,
    /// Held by nvfail, with the cause.
    pub nvfail: Option<String>,
    /// The rollback generation.
    pub rollback_gen: u32,
    /// A rollback, resize or deletion waits for its CP.
    pub op_pending: Option<String>,
    /// The block device path on Linux (`/dev/ublkbN`), if exposed.
    pub device: Option<String>,
    /// Snapshots of the volume.
    pub snapshots: usize,
}

/// A snapshot as the daemon reports it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SnapshotReport {
    /// The engine's id.
    pub id: u64,
    /// The identity.
    pub uuid: String,
    /// The catalogue name, if any.
    pub name: Option<String>,
    /// The volume's index.
    pub volume_index: u32,
    /// The volume's name, if any.
    pub volume: Option<String>,
    /// The CP it pins.
    pub cp_seq: u64,
    /// When it was taken, nanoseconds since the Unix epoch.
    pub created_wall_ns: u64,
    /// `crash` or `fsfreeze`: the consistency of the image.
    pub consistency: String,
    /// `user`, `scheduled`, `transfer` or `divergent`.
    pub origin: String,
    /// The `PIN_*` bits.
    pub flags: u8,
    /// Damaged at boot: unreadable.
    pub damaged: bool,
    /// A deletion waits for its CP.
    pub deleting: bool,
}

/// The pool as the daemon reports it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PoolReport {
    /// The device.
    pub device: String,
    /// The aggregate's identity.
    pub agg_id: String,
    /// The committed CP.
    pub cp_seq: u64,
    /// The CP of the last fold.
    pub fold_cp: u64,
    /// Serving.
    pub serving: bool,
    /// Free segments.
    pub free_segments: u32,
    /// Segments.
    pub segments: u32,
    /// The reserve (S5-3).
    pub reserve_segments: u32,
    /// Bytes per segment.
    pub segment_bytes: u64,
    /// The stripe in bytes.
    pub stripe_bytes: u32,
    /// Segments in use over segments.
    pub utilization: f64,
    /// Bytes clients may still write before the reserve.
    pub admissible_bytes: u64,
    /// Bytes acknowledged and not yet in a CP.
    pub unflushed_bytes: u64,
    /// Arena occupancy.
    pub arena_occupancy: f64,
    /// Arena bytes.
    pub arena_bytes: u64,
    /// The arena kind, as a word.
    pub arena: String,
    /// The CP timer in seconds.
    pub cp_interval_secs: u32,
    /// A CP failed for want of space and none was requested since.
    pub pool_full: bool,
    /// The flush policy in force.
    pub flush_policy: String,
    /// The device is flush-opaque.
    pub flush_opaque: bool,
    /// The discard policy in force.
    pub discard: String,
    /// Operator warnings in force.
    pub warnings: Vec<String>,
    /// Volumes present.
    pub volumes: usize,
    /// Snapshots.
    pub snapshots: usize,
    /// Segments the cleaner drained since boot.
    pub cleaned_segments: u64,
    /// Blocks it relocated since boot.
    pub relocated_blocks: u64,
    /// Consistency points committed since boot.
    pub cps: u64,
    /// Writes acknowledged since boot.
    pub writes: u64,
    /// Reads served since boot.
    pub reads: u64,
    /// Whether the last boot was dirty, rewound, and re-attached the arena.
    pub last_boot: String,
}

/// What an `Io*` request did.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct IoResultReport {
    /// The engine's status, as a word (`ok`, `nvfailed`, ...).
    pub status: String,
    /// The write's sequence number, 0 otherwise.
    pub seq: u64,
    /// A read's data as hexadecimal; empty otherwise.
    pub data_hex: String,
}

/// A successful reply.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum Response {
    /// Done, nothing to say.
    Ok,
    /// The answer to `Ping`.
    Pong {
        /// The daemon's protocol version.
        protocol: u32,
        /// The daemon's crate version.
        version: String,
        /// A pool is booted.
        pool_booted: bool,
    },
    /// The result of a format.
    Formatted {
        /// The aggregate's identity.
        agg_id: String,
        /// Stripe, segment and capacity in bytes, and segments.
        stripe_bytes: u32,
        /// Segment bytes.
        segment_bytes: u64,
        /// Capacity bytes.
        capacity_bytes: u64,
        /// Segments.
        segments: u32,
    },
    /// The pool.
    Pool(PoolReport),
    /// Volumes.
    Volumes {
        /// Every volume, by index.
        volumes: Vec<VolumeReport>,
    },
    /// One volume.
    Volume(VolumeReport),
    /// Snapshots.
    Snapshots {
        /// Every snapshot asked for, by id.
        snapshots: Vec<SnapshotReport>,
    },
    /// One snapshot.
    Snapshot(SnapshotReport),
    /// An `Io*` result.
    Io(IoResultReport),
    /// A CP committed: its sequence.
    Committed {
        /// The CP.
        cp_seq: u64,
    },
    /// The ublk devices on the host (`DeviceList`), by id.
    Devices {
        /// Every device.
        devices: Vec<DeviceReport>,
    },
    /// What a `DeviceReap` deleted, as each device stood before.
    Reaped {
        /// The deleted devices; empty when nothing was stale.
        devices: Vec<DeviceReport>,
    },
}

/// Why a request was refused or failed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ErrorCode {
    /// No pool is booted.
    NoPool,
    /// A pool is booted already (format, boot).
    PoolBooted,
    /// The request names nothing the daemon knows.
    NotFound,
    /// The request is malformed or out of bounds.
    BadRequest,
    /// The engine refused (its message says why).
    Engine,
    /// The operation did not complete in time.
    Timeout,
    /// An I/O or system error.
    Io,
    /// Not available on this host or build.
    Unsupported,
    /// The daemon and the client disagree on the protocol.
    Protocol,
}

/// An error reply.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ControlError {
    /// The class.
    pub code: ErrorCode,
    /// What happened, for the operator.
    pub message: String,
}

impl ControlError {
    /// An error of `code` with `message`.
    #[must_use]
    pub fn new(code: ErrorCode, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }
}

impl fmt::Display for ControlError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:?}: {}", self.code, self.message)
    }
}

impl std::error::Error for ControlError {}

/// What comes back for a request.
pub type Reply = Result<Response, ControlError>;

#[cfg(test)]
mod tests;
