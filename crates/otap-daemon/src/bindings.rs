//! The daemon's host-local record of its ublk devices (Step 6B): which
//! device exposes which volume, and which devices it gave up on, so a
//! daemon restarted after `kill -9` re-attaches the same `/dev/ublkbN`
//! (`otap_ublk::recovery`) and lists the rest as stale until they are
//! reaped.
//!
//! Device numbers are the host's, not the pool's: they mean nothing on
//! another host or after a reboot, so the record lives in the daemon's
//! state directory (`--state-dir`, `/run/opentap` by default, a tmpfs that
//! a reboot empties together with the devices), never in the pool. One
//! file per pool, `ublk-<aggregate id>.bindings`, replaced atomically
//! (written to a temporary file, synced, renamed over the old one, the
//! directory synced). It is a hint, not an authority: each device also
//! carries its pool and volume in `ublksrv_flags`, a device is re-attached
//! only when the kernel's description of it matches the volume, and a
//! record that does not verify is rebuilt from the devices.
//!
//! # Layout (version 1, little-endian)
//!
//! | Offset | Size | Field |
//! |---|---|---|
//! | 0 | 8 | magic `OTAPBIND` |
//! | 8 | 2 | version, 1 |
//! | 10 | 2 | entry size, 32 |
//! | 12 | 4 | entries, at most [`MAX_ENTRIES`] |
//! | 16 | 16 | the pool's aggregate identity |
//! | 32 | 4 | the pid of the daemon that wrote it |
//! | 36 | 4 | reserved, 0 |
//! | 40 | 32 x n | entries, by device id |
//! | 40 + 32n | 4 | CRC-32C of every byte before it |
//!
//! An entry: device id (`u32`, below `UBLK_MAX_UBLKS`), volume index
//! (`u32`, below `MAX_VOLS`), the volume's size in blocks when bound
//! (`u64`, 0 when stale), the kind (`u8`: 1 bound, 2 stale), the stale
//! reason (`u8`, [`StaleReason`]; 0 when bound), 14 reserved zero bytes.
//! A device appears at most once, and a volume is bound to at most one
//! device.

use core::fmt;
use std::fs::{self, File};
use std::io::{self, Write};
use std::path::{Path, PathBuf};

use otap_format::checksum::crc32c;
use otap_format::{Uuid, MAX_VOLS};
use otap_sys::ublk::UBLK_MAX_UBLKS;

/// The file's magic.
pub const BINDINGS_MAGIC: [u8; 8] = *b"OTAPBIND";
/// The layout's version.
pub const BINDINGS_VERSION: u16 = 1;
/// Bytes before the entries.
pub const HEADER_BYTES: usize = 40;
/// Bytes per entry.
pub const ENTRY_BYTES: usize = 32;
/// The CRC after the entries.
pub const TRAILER_BYTES: usize = 4;
/// The most entries a file holds: a bound device per volume, and as many
/// stale ones again.
pub const MAX_ENTRIES: usize = 2 * MAX_VOLS;

const KIND_BOUND: u8 = 1;
const KIND_STALE: u8 = 2;

/// Why a device the daemon recorded is stale.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum StaleReason {
    /// Its volume was offline at the boot that found it.
    VolumeOffline,
    /// Its volume no longer exists.
    VolumeGone,
    /// Its re-attach failed (the daemon's log has the cause).
    RecoveryFailed,
    /// The driver had not quiesced it within `--recovery-timeout`.
    RecoveryTimedOut,
    /// Its start failed and the device could not be deleted.
    StartAbandoned,
    /// Taking it down failed.
    TeardownFailed,
}

impl StaleReason {
    const fn code(self) -> u8 {
        match self {
            Self::VolumeOffline => 1,
            Self::VolumeGone => 2,
            Self::RecoveryFailed => 3,
            Self::RecoveryTimedOut => 4,
            Self::StartAbandoned => 5,
            Self::TeardownFailed => 6,
        }
    }

    const fn from_code(code: u8) -> Option<Self> {
        Some(match code {
            1 => Self::VolumeOffline,
            2 => Self::VolumeGone,
            3 => Self::RecoveryFailed,
            4 => Self::RecoveryTimedOut,
            5 => Self::StartAbandoned,
            6 => Self::TeardownFailed,
            _ => return None,
        })
    }

    /// The reason as the device list gives it, for volume `vol`.
    #[must_use]
    pub fn describe(self, vol: u32) -> String {
        match self {
            Self::VolumeOffline => format!("volume {vol}'s device; the volume was offline at boot"),
            Self::VolumeGone => format!("volume {vol}'s device; the volume no longer exists"),
            Self::RecoveryFailed => format!(
                "volume {vol}'s device; its re-attach failed (the daemon's log has why) and the \
                 volume got a new device"
            ),
            Self::RecoveryTimedOut => format!(
                "volume {vol}'s device; the kernel had not quiesced it within the recovery \
                 timeout and the volume got a new device"
            ),
            Self::StartAbandoned => {
                format!("volume {vol}'s device; its start failed and it could not be deleted")
            }
            Self::TeardownFailed => format!("volume {vol}'s device; its deletion failed"),
        }
    }
}

/// One record.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Binding {
    /// Device `dev_id` exposes volume `vol` of `size_blocks` blocks.
    Bound {
        /// The device.
        dev_id: u32,
        /// The volume.
        vol: u32,
        /// The volume's size when the device was made.
        size_blocks: u64,
    },
    /// Device `dev_id` was volume `vol`'s and will not be re-attached.
    Stale {
        /// The device.
        dev_id: u32,
        /// The volume it was made for.
        vol: u32,
        /// Why.
        reason: StaleReason,
    },
}

impl Binding {
    /// The device.
    #[must_use]
    pub const fn dev_id(&self) -> u32 {
        match *self {
            Self::Bound { dev_id, .. } | Self::Stale { dev_id, .. } => dev_id,
        }
    }

    /// The volume.
    #[must_use]
    pub const fn vol(&self) -> u32 {
        match *self {
            Self::Bound { vol, .. } | Self::Stale { vol, .. } => vol,
        }
    }
}

/// Why a record was refused.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BindingsError {
    /// Reading the file failed.
    Io(String),
    /// Shorter than a header and a CRC.
    TooShort {
        /// Its length.
        len: usize,
    },
    /// Not a bindings file.
    BadMagic,
    /// A version this daemon does not read.
    BadVersion {
        /// The version found.
        found: u16,
    },
    /// An entry size this version does not have.
    BadEntrySize {
        /// The size found.
        found: u16,
    },
    /// More entries than [`MAX_ENTRIES`].
    TooMany {
        /// The count found.
        count: u32,
    },
    /// A length that is not the header, the entries and the CRC.
    BadLength {
        /// The file's length.
        len: usize,
        /// The entries the header names.
        count: u32,
    },
    /// The CRC does not match.
    BadCrc {
        /// The CRC stored.
        stored: u32,
        /// The CRC of the bytes.
        computed: u32,
    },
    /// The record of another pool.
    OtherPool {
        /// Its aggregate identity.
        found: Uuid,
    },
    /// An entry's field out of range.
    BadEntry {
        /// The entry.
        index: usize,
        /// The field.
        field: &'static str,
        /// Its value.
        value: u64,
    },
    /// A device recorded twice.
    DuplicateDevice {
        /// The device.
        dev_id: u32,
    },
    /// A volume bound to two devices.
    DuplicateVolume {
        /// The volume.
        vol: u32,
    },
}

impl fmt::Display for BindingsError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(e) => write!(f, "cannot read it: {e}"),
            Self::TooShort { len } => write!(f, "{len} bytes is shorter than a header"),
            Self::BadMagic => f.write_str("its magic is not OTAPBIND"),
            Self::BadVersion { found } => write!(
                f,
                "version {found}; this daemon reads version {BINDINGS_VERSION}"
            ),
            Self::BadEntrySize { found } => write!(
                f,
                "entry size {found}; version {BINDINGS_VERSION} has {ENTRY_BYTES}"
            ),
            Self::TooMany { count } => write!(f, "{count} entries, more than {MAX_ENTRIES}"),
            Self::BadLength { len, count } => {
                write!(
                    f,
                    "{len} bytes do not hold a header, {count} entries and a CRC"
                )
            }
            Self::BadCrc { stored, computed } => {
                write!(f, "CRC {stored:#010x} stored, {computed:#010x} computed")
            }
            Self::OtherPool { found } => write!(f, "it is the record of pool {found}"),
            Self::BadEntry {
                index,
                field,
                value,
            } => write!(f, "entry {index}: {field} {value} is out of range"),
            Self::DuplicateDevice { dev_id } => write!(f, "device {dev_id} is recorded twice"),
            Self::DuplicateVolume { vol } => write!(f, "volume {vol} is bound twice"),
        }
    }
}

impl std::error::Error for BindingsError {}

/// The record of one pool.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Bindings {
    agg_id: Uuid,
    /// By device id.
    entries: Vec<Binding>,
}

impl Bindings {
    /// An empty record for pool `agg_id`.
    #[must_use]
    pub const fn new(agg_id: Uuid) -> Self {
        Self {
            agg_id,
            entries: Vec::new(),
        }
    }

    /// The pool.
    #[must_use]
    pub const fn agg_id(&self) -> &Uuid {
        &self.agg_id
    }

    /// Every record, by device id.
    #[must_use]
    pub fn entries(&self) -> &[Binding] {
        &self.entries
    }

    /// `true` with no records.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// The device bound to `vol`, with the size recorded.
    #[must_use]
    pub fn bound(&self, vol: u32) -> Option<(u32, u64)> {
        self.entries.iter().find_map(|b| match *b {
            Binding::Bound {
                dev_id,
                vol: v,
                size_blocks,
            } if v == vol => Some((dev_id, size_blocks)),
            _ => None,
        })
    }

    /// `true` when `dev_id` is recorded stale.
    #[must_use]
    pub fn is_stale(&self, dev_id: u32) -> bool {
        self.entries
            .iter()
            .any(|b| matches!(*b, Binding::Stale { dev_id: d, .. } if d == dev_id))
    }

    /// The stale devices with their reasons, by id.
    #[must_use]
    pub fn stale(&self) -> Vec<(u32, String)> {
        self.entries
            .iter()
            .filter_map(|b| match *b {
                Binding::Stale {
                    dev_id,
                    vol,
                    reason,
                } => Some((dev_id, reason.describe(vol))),
                Binding::Bound { .. } => None,
            })
            .collect()
    }

    fn put(&mut self, entry: Binding) {
        self.entries.retain(|b| b.dev_id() != entry.dev_id());
        let at = self
            .entries
            .partition_point(|b| b.dev_id() < entry.dev_id());
        self.entries.insert(at, entry);
    }

    /// Records `dev_id` as the device of `vol` (of `size_blocks` blocks),
    /// replacing any record of the device and any other device bound to
    /// the volume.
    pub fn bind(&mut self, vol: u32, dev_id: u32, size_blocks: u64) {
        self.entries
            .retain(|b| !matches!(*b, Binding::Bound { vol: v, .. } if v == vol));
        self.put(Binding::Bound {
            dev_id,
            vol,
            size_blocks,
        });
    }

    /// Records `dev_id`, made for `vol`, as stale.
    pub fn mark_stale(&mut self, dev_id: u32, vol: u32, reason: StaleReason) {
        self.put(Binding::Stale {
            dev_id,
            vol,
            reason,
        });
    }

    /// Forgets `dev_id`; `true` when it was recorded.
    pub fn forget(&mut self, dev_id: u32) -> bool {
        let before = self.entries.len();
        self.entries.retain(|b| b.dev_id() != dev_id);
        self.entries.len() != before
    }

    /// The file's bytes, naming `pid` as the writer.
    #[must_use]
    pub fn encode(&self, pid: u32) -> Vec<u8> {
        let mut out = Vec::with_capacity(HEADER_BYTES + ENTRY_BYTES * self.entries.len() + 4);
        out.extend_from_slice(&BINDINGS_MAGIC);
        out.extend_from_slice(&BINDINGS_VERSION.to_le_bytes());
        out.extend_from_slice(&(ENTRY_BYTES as u16).to_le_bytes());
        out.extend_from_slice(&(self.entries.len() as u32).to_le_bytes());
        out.extend_from_slice(self.agg_id.as_array());
        out.extend_from_slice(&pid.to_le_bytes());
        out.extend_from_slice(&0u32.to_le_bytes());
        for entry in &self.entries {
            let (dev_id, vol, size, kind, reason) = match *entry {
                Binding::Bound {
                    dev_id,
                    vol,
                    size_blocks,
                } => (dev_id, vol, size_blocks, KIND_BOUND, 0),
                Binding::Stale {
                    dev_id,
                    vol,
                    reason,
                } => (dev_id, vol, 0, KIND_STALE, reason.code()),
            };
            out.extend_from_slice(&dev_id.to_le_bytes());
            out.extend_from_slice(&vol.to_le_bytes());
            out.extend_from_slice(&size.to_le_bytes());
            out.push(kind);
            out.push(reason);
            out.extend_from_slice(&[0u8; 14]);
        }
        let crc = crc32c(&out);
        out.extend_from_slice(&crc.to_le_bytes());
        out
    }

    /// Reads a file's bytes as the record of pool `agg_id`, verifying
    /// everything before anything is exposed.
    ///
    /// # Errors
    /// [`BindingsError`], naming the field at fault.
    pub fn decode(bytes: &[u8], agg_id: &Uuid) -> Result<Self, BindingsError> {
        let u16_at = |at: usize| u16::from_le_bytes([bytes[at], bytes[at + 1]]);
        let u32_at = |at: usize| {
            u32::from_le_bytes([bytes[at], bytes[at + 1], bytes[at + 2], bytes[at + 3]])
        };
        let u64_at = |at: usize| u64::from(u32_at(at)) | u64::from(u32_at(at + 4)) << 32;
        if bytes.len() < HEADER_BYTES + TRAILER_BYTES {
            return Err(BindingsError::TooShort { len: bytes.len() });
        }
        if bytes[..8] != BINDINGS_MAGIC {
            return Err(BindingsError::BadMagic);
        }
        let version = u16_at(8);
        if version != BINDINGS_VERSION {
            return Err(BindingsError::BadVersion { found: version });
        }
        let entry = u16_at(10);
        if usize::from(entry) != ENTRY_BYTES {
            return Err(BindingsError::BadEntrySize { found: entry });
        }
        let count = u32_at(12);
        if count as usize > MAX_ENTRIES {
            return Err(BindingsError::TooMany { count });
        }
        let body = HEADER_BYTES + ENTRY_BYTES * count as usize;
        if bytes.len() != body + TRAILER_BYTES {
            return Err(BindingsError::BadLength {
                len: bytes.len(),
                count,
            });
        }
        let stored = u32_at(body);
        let computed = crc32c(&bytes[..body]);
        if stored != computed {
            return Err(BindingsError::BadCrc { stored, computed });
        }
        let mut id = [0u8; 16];
        id.copy_from_slice(&bytes[16..32]);
        let found = Uuid::from_bytes(id);
        if &found != agg_id {
            return Err(BindingsError::OtherPool { found });
        }
        let bad = |index: usize, field: &'static str, value: u64| BindingsError::BadEntry {
            index,
            field,
            value,
        };
        if u32_at(36) != 0 {
            return Err(bad(0, "header reserved word", u64::from(u32_at(36))));
        }
        let mut entries: Vec<Binding> = Vec::with_capacity(count as usize);
        for index in 0..count as usize {
            let at = HEADER_BYTES + ENTRY_BYTES * index;
            let dev_id = u32_at(at);
            let vol = u32_at(at + 4);
            let size_blocks = u64_at(at + 8);
            let kind = bytes[at + 16];
            let reason = bytes[at + 17];
            if dev_id >= UBLK_MAX_UBLKS {
                return Err(bad(index, "device id", u64::from(dev_id)));
            }
            if vol as usize >= MAX_VOLS {
                return Err(bad(index, "volume", u64::from(vol)));
            }
            if let Some(nonzero) = bytes[at + 18..at + ENTRY_BYTES].iter().find(|b| **b != 0) {
                return Err(bad(index, "reserved byte", u64::from(*nonzero)));
            }
            let binding = match kind {
                KIND_BOUND => {
                    if reason != 0 {
                        return Err(bad(index, "reason of a bound entry", u64::from(reason)));
                    }
                    if size_blocks == 0 {
                        return Err(bad(index, "size", 0));
                    }
                    Binding::Bound {
                        dev_id,
                        vol,
                        size_blocks,
                    }
                }
                KIND_STALE => {
                    if size_blocks != 0 {
                        return Err(bad(index, "size of a stale entry", size_blocks));
                    }
                    let reason = StaleReason::from_code(reason)
                        .ok_or_else(|| bad(index, "stale reason", u64::from(reason)))?;
                    Binding::Stale {
                        dev_id,
                        vol,
                        reason,
                    }
                }
                other => return Err(bad(index, "kind", u64::from(other))),
            };
            if let Some(prev) = entries.last() {
                if prev.dev_id() == dev_id {
                    return Err(BindingsError::DuplicateDevice { dev_id });
                }
                if prev.dev_id() > dev_id {
                    return Err(bad(index, "device id out of order", u64::from(dev_id)));
                }
            }
            if let Binding::Bound { vol, .. } = binding {
                if entries
                    .iter()
                    .any(|b| matches!(*b, Binding::Bound { vol: v, .. } if v == vol))
                {
                    return Err(BindingsError::DuplicateVolume { vol });
                }
            }
            entries.push(binding);
        }
        Ok(Self {
            agg_id: *agg_id,
            entries,
        })
    }
}

/// The file name of pool `agg_id`'s record.
#[must_use]
pub fn file_name(agg_id: &Uuid) -> String {
    format!("ublk-{agg_id}.bindings")
}

/// The path of pool `agg_id`'s record in `dir`.
#[must_use]
pub fn file_path(dir: &Path, agg_id: &Uuid) -> PathBuf {
    dir.join(file_name(agg_id))
}

/// Reads pool `agg_id`'s record from `dir`: `None` when there is none.
///
/// # Errors
/// The file cannot be read, or does not verify.
pub fn load(dir: &Path, agg_id: &Uuid) -> Result<Option<Bindings>, BindingsError> {
    let path = file_path(dir, agg_id);
    let bytes = match fs::read(&path) {
        Ok(bytes) => bytes,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(BindingsError::Io(e.to_string())),
    };
    Bindings::decode(&bytes, agg_id).map(Some)
}

/// Replaces the record in `dir` with `bindings`, atomically: the bytes go
/// to a temporary file, which is synced and renamed over the record, and
/// the directory is synced (on Unix), so a crash leaves the old record or
/// the new one. An empty record removes the file. The directory is created
/// when missing.
///
/// # Errors
/// The file system's error.
pub fn save(dir: &Path, bindings: &Bindings, pid: u32) -> io::Result<()> {
    let path = file_path(dir, &bindings.agg_id);
    if bindings.is_empty() {
        match fs::remove_file(&path) {
            Ok(()) => return sync_dir(dir),
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(()),
            Err(e) => return Err(e),
        }
    }
    fs::create_dir_all(dir)?;
    let tmp = dir.join(format!("{}.tmp", file_name(&bindings.agg_id)));
    {
        let mut file = File::create(&tmp)?;
        file.write_all(&bindings.encode(pid))?;
        file.sync_all()?;
    }
    fs::rename(&tmp, &path)?;
    sync_dir(dir)
}

#[cfg(unix)]
fn sync_dir(dir: &Path) -> io::Result<()> {
    File::open(dir)?.sync_all()
}

#[cfg(not(unix))]
fn sync_dir(_dir: &Path) -> io::Result<()> {
    // A directory cannot be opened for a sync here; the rename itself is
    // what the platform makes durable.
    Ok(())
}
