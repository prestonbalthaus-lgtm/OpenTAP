//! `otap-geom`: geometry discovery (round 3, section 1.3, "Geometry,
//! caches, probe"; Step 4 brief, "Opus-Storage-Geom").
//!
//! The engine lays an aggregate out from a [`otap_cp::Geometry`] and runs it
//! under a [`otap_cp::MediaProfile`]. This crate derives both from what the
//! kernel says about the device: the block queue's limits under
//! `/sys/block/<dev>/queue/`, the md topology under `/sys/block/<dev>/md/`,
//! and the SCSI identity under `/sys/block/<dev>/device/`. The trust order
//! of section 1.3 is kept where it can be: md sysfs first (stripe = chunk
//! times data disks, `optimal_io_size` as a cross-check), then the block
//! queue's `optimal_io_size`, then a default with a warning that the probe
//! (a lab-box item) has not run. The TOML override of section 1.3 is the
//! [`Overrides`] value.
//!
//! Everything is read through a [`SysfsSource`], so the rules are tested on
//! any host with a [`MockSysfs`] and run on Linux with [`Sysfs`].
//!
//! # What is derived
//!
//! - The full stripe and the segment ([`StripeGeometry`]): 8 MiB segments
//!   on flash, 64 MiB on NVMe, 80 MiB on rotational media (section 7.2),
//!   rounded to whole stripes and bounded by the pool.
//! - The media ([`otap_cp::Media`]) from `rotational`.
//! - Whether the device is flush-opaque: a hardware RAID virtual drive
//!   (C05 to C08: the controller presents no write cache and no FUA, so
//!   the kernel strips every flush and the host cannot see the cache), and
//!   the [`otap_cp::FlushPolicy`] the owner's E19 ruling attaches to it
//!   (trust the barrier, warn, rely on nvfail) unless strict mode is asked
//!   for.
//! - The discard posture (D9): whole-segment discards on flash and NVMe
//!   that report a discard granularity, none on rotational media and on
//!   virtual drives.

#![forbid(unsafe_code)]
#![deny(missing_docs)]
#![warn(rust_2018_idioms, unreachable_pub)]

use core::fmt;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use otap_cp::{DiscardPolicy, FlushPolicy, Geometry, Media, MediaProfile};
use otap_format::{BLOCK_SIZE, MAX_SEG_BYTES, MAX_STRIPE_BYTES, MIN_STRIPE_BYTES};

/// Where sysfs attributes come from: the kernel on Linux, a map in the
/// tests. Paths are relative to `/sys/block/<dev>/`, such as
/// `queue/rotational` or `md/level`; a missing attribute is `None`.
pub trait SysfsSource {
    /// The attribute's text, trailing whitespace removed.
    fn read(&self, path: &str) -> Option<String>;
}

/// A sysfs source over a map, for the tests and the simulator.
#[derive(Debug, Clone, Default)]
pub struct MockSysfs {
    attrs: BTreeMap<String, String>,
}

impl MockSysfs {
    /// An empty mock.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Sets an attribute.
    pub fn set(&mut self, path: &str, value: impl Into<String>) -> &mut Self {
        self.attrs.insert(path.to_string(), value.into());
        self
    }

    /// Removes an attribute.
    pub fn unset(&mut self, path: &str) -> &mut Self {
        self.attrs.remove(path);
        self
    }
}

impl SysfsSource for MockSysfs {
    fn read(&self, path: &str) -> Option<String> {
        self.attrs.get(path).map(|v| v.trim_end().to_string())
    }
}

/// The kernel's sysfs, read from files: `<root>/<dev>/<path>` with `root`
/// `/sys/block` by default. Reading works on any host (there is simply
/// nothing under `/sys/block` elsewhere).
#[derive(Debug, Clone)]
pub struct Sysfs {
    root: PathBuf,
    dev: String,
}

impl Sysfs {
    /// The device `dev` under `/sys/block`.
    #[must_use]
    pub fn new(dev: &str) -> Self {
        Self::at(Path::new("/sys/block"), dev)
    }

    /// The device `dev` under `root` (a copied sysfs tree in the tests).
    #[must_use]
    pub fn at(root: &Path, dev: &str) -> Self {
        Self {
            root: root.to_path_buf(),
            dev: dev.to_string(),
        }
    }
}

impl SysfsSource for Sysfs {
    fn read(&self, path: &str) -> Option<String> {
        std::fs::read_to_string(self.root.join(&self.dev).join(path))
            .ok()
            .map(|s| s.trim_end().to_string())
    }
}

/// What the kernel says about the device's write cache.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum WriteCacheView {
    /// `queue/write_cache` is `write back`: the device has a volatile cache
    /// the kernel flushes.
    WriteBack,
    /// `write through`: the kernel sees no cache (an honest cacheless
    /// device, or a controller that hides its cache).
    WriteThrough,
    /// The attribute is absent.
    Unknown,
}

/// What kind of device sysfs describes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum DeviceKind {
    /// An md array (`md/level` present).
    Md,
    /// An NVMe namespace.
    Nvme,
    /// A hardware RAID virtual drive: the SCSI vendor or model names a
    /// controller (PERC, MegaRAID, Smart Array, Adaptec, ...).
    HwRaidVd,
    /// A plain SCSI, SATA or SAS disk.
    Disk,
    /// A virtual disk of a hypervisor (virtio, Xen, VMware, Hyper-V,
    /// VirtualBox).
    Virtual,
    /// A loop device.
    Loop,
    /// A device-mapper target.
    Dm,
    /// Anything else.
    Other,
}

/// The md topology, when the device is an array.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MdFacts {
    /// `md/level`: `raid0`, `raid1`, `raid4`, `raid5`, `raid6`, `raid10`,
    /// `linear`.
    pub level: String,
    /// `md/chunk_size`, bytes (0 for levels without a chunk).
    pub chunk_bytes: u32,
    /// `md/raid_disks`.
    pub raid_disks: u32,
    /// `md/layout` (raid10: near copies in the low byte, far copies in the
    /// next).
    pub layout: u32,
}

impl MdFacts {
    /// Data disks a full stripe spans, or `None` for a level without
    /// striping (raid1, linear).
    #[must_use]
    pub fn data_disks(&self) -> Option<u32> {
        let n = self.raid_disks;
        match self.level.as_str() {
            "raid0" => Some(n),
            "raid4" | "raid5" => n.checked_sub(1).filter(|d| *d >= 1),
            "raid6" => n.checked_sub(2).filter(|d| *d >= 1),
            "raid10" => {
                let near = (self.layout & 0xff).max(1);
                let far = ((self.layout >> 8) & 0xff).max(1);
                let copies = near.saturating_mul(far).max(1);
                Some((n / copies).max(1))
            }
            _ => None,
        }
    }
}

/// What sysfs says about a device.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeviceFacts {
    /// The device name (`sda`, `md0`, `nvme0n1`).
    pub name: String,
    /// The size in bytes (`size` is in 512-byte sectors).
    pub size_bytes: u64,
    /// `queue/logical_block_size`.
    pub logical_block_size: u32,
    /// `queue/physical_block_size`.
    pub physical_block_size: u32,
    /// `queue/minimum_io_size`.
    pub minimum_io_size: u32,
    /// `queue/optimal_io_size` (0 when the device reports none).
    pub optimal_io_size: u32,
    /// `queue/alignment_offset`.
    pub alignment_offset: u64,
    /// `queue/rotational`.
    pub rotational: bool,
    /// `queue/discard_granularity` (0 when discards are unsupported).
    pub discard_granularity: u32,
    /// `queue/discard_max_bytes`.
    pub discard_max_bytes: u64,
    /// `queue/write_cache`.
    pub write_cache: WriteCacheView,
    /// `queue/fua`.
    pub fua: bool,
    /// `queue/max_sectors_kb`.
    pub max_sectors_kb: u32,
    /// `queue/max_hw_sectors_kb`.
    pub max_hw_sectors_kb: u32,
    /// `device/vendor`, trimmed.
    pub vendor: String,
    /// `device/model`, trimmed.
    pub model: String,
    /// The md topology, when the device is an array.
    pub md: Option<MdFacts>,
    /// The classification.
    pub kind: DeviceKind,
}

/// Why discovery or planning refused.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum GeomError {
    /// A required attribute is absent.
    Missing {
        /// The attribute's path.
        attr: &'static str,
    },
    /// An attribute does not parse.
    BadValue {
        /// The attribute's path.
        attr: &'static str,
        /// What was read.
        value: String,
    },
    /// The device's logical block is larger than the engine's 4 KiB block,
    /// or its geometry cannot be laid out.
    Unsupported {
        /// Why.
        reason: String,
    },
    /// `alignment_offset` is not zero: the partition or the virtual drive
    /// starts off the physical boundary, and full-stripe writes would split
    /// on every stripe. Override with `allow_misaligned` to proceed.
    Misaligned {
        /// The offset in bytes.
        offset: u64,
    },
    /// The device is too small for a pool: the fixed regions and at least
    /// eight segments must fit.
    TooSmall {
        /// The size in bytes.
        size_bytes: u64,
        /// The least the plan needs.
        needed: u64,
    },
    /// An override is out of the format's bounds.
    BadOverride {
        /// Which.
        what: &'static str,
        /// The value.
        value: u64,
    },
}

impl fmt::Display for GeomError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Missing { attr } => write!(f, "sysfs attribute {attr} is missing"),
            Self::BadValue { attr, value } => write!(f, "sysfs attribute {attr} is {value:?}"),
            Self::Unsupported { reason } => write!(f, "unsupported device: {reason}"),
            Self::Misaligned { offset } => write!(
                f,
                "the device starts {offset} bytes off its physical boundary; allow_misaligned \
                 overrides"
            ),
            Self::TooSmall { size_bytes, needed } => {
                write!(f, "{size_bytes} bytes is too small; {needed} bytes needed")
            }
            Self::BadOverride { what, value } => {
                write!(f, "override {what} = {value} is out of bounds")
            }
        }
    }
}

impl std::error::Error for GeomError {}

/// Vendor and model fragments (upper case) that name a RAID controller's
/// virtual drive rather than a disk (C05 to C08, R50).
const RAID_VD_MARKERS: [&str; 12] = [
    "PERC",
    "MEGARAID",
    "MR9",
    "LSI",
    "AVAGO",
    "BROADCOM",
    "LOGICAL VOLUME",
    "SMART ARRAY",
    "ADAPTEC",
    "ASR",
    "AAC",
    "VIRTUAL DISK",
];

/// Vendor fragments (upper case) of hypervisors' virtual disks. These are
/// tested before the controller markers: a VMware or Hyper-V disk calls
/// itself "Virtual disk" too, and it is not a flush-opaque controller.
const VIRTUAL_MARKERS: [&str; 5] = ["QEMU", "VMWARE", "MSFT", "VBOX", "XEN"];

/// `true` when `upper` (a vendor and model string, upper case) carries
/// `marker`: a short marker (`LSI`, `ASR`, `AAC`, `MR9`) must be a whole
/// word, so a model name that merely contains the letters does not make a
/// plain disk a controller.
fn has_marker(upper: &str, marker: &str) -> bool {
    if marker.len() > 3 {
        return upper.contains(marker);
    }
    upper
        .split(|c: char| !c.is_ascii_alphanumeric())
        .any(|word| word == marker)
}

fn read_u64(source: &dyn SysfsSource, attr: &'static str) -> Result<Option<u64>, GeomError> {
    match source.read(attr) {
        None => Ok(None),
        Some(text) => text
            .trim()
            .parse::<u64>()
            .map(Some)
            .map_err(|_| GeomError::BadValue { attr, value: text }),
    }
}

fn required_u64(source: &dyn SysfsSource, attr: &'static str) -> Result<u64, GeomError> {
    read_u64(source, attr)?.ok_or(GeomError::Missing { attr })
}

fn to_u32(attr: &'static str, value: u64) -> Result<u32, GeomError> {
    u32::try_from(value).map_err(|_| GeomError::BadValue {
        attr,
        value: value.to_string(),
    })
}

/// Reads and classifies device `name` through `source`.
pub fn discover(source: &dyn SysfsSource, name: &str) -> Result<DeviceFacts, GeomError> {
    let sectors = required_u64(source, "size")?;
    let size_bytes = sectors.saturating_mul(512);
    let logical_block_size = to_u32(
        "queue/logical_block_size",
        required_u64(source, "queue/logical_block_size")?,
    )?;
    let physical_block_size = to_u32(
        "queue/physical_block_size",
        read_u64(source, "queue/physical_block_size")?.unwrap_or(u64::from(logical_block_size)),
    )?;
    let minimum_io_size = to_u32(
        "queue/minimum_io_size",
        read_u64(source, "queue/minimum_io_size")?.unwrap_or(u64::from(physical_block_size)),
    )?;
    let optimal_io_size = to_u32(
        "queue/optimal_io_size",
        read_u64(source, "queue/optimal_io_size")?.unwrap_or(0),
    )?;
    let alignment_offset = read_u64(source, "queue/alignment_offset")?.unwrap_or(0);
    let rotational = required_u64(source, "queue/rotational")? != 0;
    let discard_granularity = to_u32(
        "queue/discard_granularity",
        read_u64(source, "queue/discard_granularity")?.unwrap_or(0),
    )?;
    let discard_max_bytes = read_u64(source, "queue/discard_max_bytes")?.unwrap_or(0);
    let write_cache = match source.read("queue/write_cache").as_deref().map(str::trim) {
        Some("write back") => WriteCacheView::WriteBack,
        Some("write through") => WriteCacheView::WriteThrough,
        Some(other) => {
            return Err(GeomError::BadValue {
                attr: "queue/write_cache",
                value: other.to_string(),
            })
        }
        None => WriteCacheView::Unknown,
    };
    let fua = read_u64(source, "queue/fua")?.unwrap_or(0) != 0;
    let max_sectors_kb = to_u32(
        "queue/max_sectors_kb",
        read_u64(source, "queue/max_sectors_kb")?.unwrap_or(0),
    )?;
    let max_hw_sectors_kb = to_u32(
        "queue/max_hw_sectors_kb",
        read_u64(source, "queue/max_hw_sectors_kb")?.unwrap_or(0),
    )?;
    let vendor = source
        .read("device/vendor")
        .map(|s| s.trim().to_string())
        .unwrap_or_default();
    let model = source
        .read("device/model")
        .map(|s| s.trim().to_string())
        .unwrap_or_default();
    let md = match source.read("md/level") {
        Some(level) => {
            let chunk_bytes = to_u32(
                "md/chunk_size",
                read_u64(source, "md/chunk_size")?.unwrap_or(0),
            )?;
            let raid_disks = to_u32("md/raid_disks", required_u64(source, "md/raid_disks")?)?;
            let layout = to_u32("md/layout", read_u64(source, "md/layout")?.unwrap_or(0))?;
            Some(MdFacts {
                level: level.trim().to_string(),
                chunk_bytes,
                raid_disks,
                layout,
            })
        }
        None => None,
    };
    let upper = format!("{} {}", vendor.to_uppercase(), model.to_uppercase());
    let kind = if md.is_some() {
        DeviceKind::Md
    } else if name.starts_with("nvme") {
        DeviceKind::Nvme
    } else if name.starts_with("loop") {
        DeviceKind::Loop
    } else if name.starts_with("dm-") {
        DeviceKind::Dm
    } else if name.starts_with("vd")
        || name.starts_with("xvd")
        || VIRTUAL_MARKERS.iter().any(|m| upper.contains(m))
    {
        DeviceKind::Virtual
    } else if RAID_VD_MARKERS.iter().any(|m| has_marker(&upper, m)) {
        DeviceKind::HwRaidVd
    } else if name.starts_with("sd") {
        DeviceKind::Disk
    } else {
        DeviceKind::Other
    };
    Ok(DeviceFacts {
        name: name.to_string(),
        size_bytes,
        logical_block_size,
        physical_block_size,
        minimum_io_size,
        optimal_io_size,
        alignment_offset,
        rotational,
        discard_granularity,
        discard_max_bytes,
        write_cache,
        fua,
        max_sectors_kb,
        max_hw_sectors_kb,
        vendor,
        model,
        md,
        kind,
    })
}

/// The operator's overrides (the TOML of section 1.3).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Overrides {
    /// The full stripe, bytes.
    pub stripe_bytes: Option<u32>,
    /// The segment, bytes.
    pub segment_bytes: Option<u64>,
    /// Treat (or do not treat) the device as flush-opaque.
    pub flush_opaque: Option<bool>,
    /// The flush policy (`RequireFuaOrFlush` is strict mode).
    pub flush_policy: Option<FlushPolicy>,
    /// The discard policy.
    pub discard: Option<DiscardPolicy>,
    /// The media.
    pub media: Option<Media>,
    /// Lay out a device whose `alignment_offset` is not zero.
    pub allow_misaligned: bool,
}

/// Where the stripe came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum GeomSource {
    /// md's chunk times data disks.
    MdSysfs,
    /// The block queue's `optimal_io_size`.
    OptimalIoSize,
    /// A default, because the device reported nothing usable: the probe
    /// of section 1.3 has not run.
    Default,
    /// The operator's override.
    Manual,
}

/// Something the operator should know about the plan.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum GeomWarning {
    /// The stripe is longer than one command the queue accepts: the block
    /// layer splits every stripe write, and a controller may read-modify-
    /// write.
    StripeExceedsMaxSectors {
        /// The stripe.
        stripe_bytes: u32,
        /// `max_sectors_kb` in bytes.
        max_bytes: u64,
    },
    /// The device reported no usable `optimal_io_size`; the default stripe
    /// stands until the probe runs.
    OptimalIoUnknown,
    /// md's stripe and the queue's `optimal_io_size` disagree; md's is used.
    OptimalIoDisagrees {
        /// The queue's value.
        optimal_io_size: u32,
    },
    /// The device is flush-opaque and the barrier is trusted (E19 / E21).
    FlushOpaqueTrusted,
    /// Flash without a discard granularity: no discards.
    DiscardUnavailable,
    /// The segment was shrunk so that eight segments fit the device.
    SmallDevice,
    /// The segment was grown so that the segment table can count the
    /// device's segments (a `u32`).
    SegmentGrown {
        /// The segment chosen, bytes.
        segment_bytes: u64,
    },
    /// The device is misaligned and the override let it through.
    Misaligned {
        /// The offset in bytes.
        offset: u64,
    },
}

/// The plan: the geometry the engine lays out and the profile it runs
/// under.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StripeGeometry {
    /// The full stripe, bytes.
    pub stripe_bytes: u32,
    /// The segment, bytes (a whole number of stripes).
    pub segment_bytes: u64,
    /// The device, bytes.
    pub capacity_bytes: u64,
    /// The chunk (md) or the stripe itself.
    pub chunk_bytes: u32,
    /// Data disks a stripe spans (1 for a single device).
    pub data_disks: u32,
    /// Where the stripe came from.
    pub source: GeomSource,
    /// The media.
    pub media: Media,
    /// The device is flush-opaque.
    pub flush_opaque: bool,
    /// The flush policy.
    pub flush_policy: FlushPolicy,
    /// The discard policy.
    pub discard: DiscardPolicy,
    /// The device kind.
    pub kind: DeviceKind,
    /// What the operator should know.
    pub warnings: Vec<GeomWarning>,
}

impl StripeGeometry {
    /// The engine's geometry.
    #[must_use]
    pub const fn geometry(&self) -> Geometry {
        Geometry {
            stripe_bytes: self.stripe_bytes,
            segment_bytes: self.segment_bytes,
            capacity_bytes: self.capacity_bytes,
            flush_opaque: self.flush_opaque,
        }
    }

    /// The engine's media profile: the media's defaults with this plan's
    /// geometry, flush policy and discard posture.
    #[must_use]
    pub const fn media_profile(&self) -> MediaProfile {
        let mut profile = match self.media {
            Media::Hdd => MediaProfile::hdd(self.geometry()),
            Media::Flash => MediaProfile::flash(self.geometry()),
        };
        profile.flush_policy = self.flush_policy;
        profile.discard = self.discard;
        profile
    }
}

/// Segment targets (section 7.2): rotational, NVMe, other flash.
const SEGMENT_HDD: u64 = 80 << 20;
const SEGMENT_NVME: u64 = 64 << 20;
const SEGMENT_FLASH: u64 = 8 << 20;
/// The stripe when the device reports nothing usable (a lone drive's write
/// unit, section 1.3).
const DEFAULT_STRIPE: u32 = 1 << 20;
/// The smallest `optimal_io_size` taken as a stripe: below it the value
/// describes a physical block, not a RAID stripe.
const MIN_USEFUL_OPTIMAL: u32 = 64 << 10;
/// Segments a pool must hold at least.
const MIN_SEGMENTS: u64 = 8;

fn stripe_ok(stripe: u64) -> bool {
    stripe >= MIN_STRIPE_BYTES as u64
        && stripe <= MAX_STRIPE_BYTES as u64
        && stripe.is_multiple_of(BLOCK_SIZE as u64)
}

/// Derives the plan for `facts` under `overrides`.
pub fn plan(facts: &DeviceFacts, overrides: &Overrides) -> Result<StripeGeometry, GeomError> {
    let mut warnings = Vec::new();
    // Every engine I/O is 4 KiB at a 4 KiB-aligned offset, which direct I/O
    // allows only when the logical block divides 4 KiB (512, 1024, 2048 or
    // 4096 bytes); a 520-byte DIF disk or a 3 KiB block cannot take it.
    let lbs = facts.logical_block_size;
    if lbs == 0
        || lbs > BLOCK_SIZE as u32
        || !lbs.is_power_of_two()
        || !(BLOCK_SIZE as u32).is_multiple_of(lbs)
    {
        return Err(GeomError::Unsupported {
            reason: format!(
                "a logical block of {lbs} bytes; the engine addresses 4 KiB blocks, which needs a \
                 power of two dividing 4096"
            ),
        });
    }
    if facts.alignment_offset != 0 {
        if overrides.allow_misaligned {
            warnings.push(GeomWarning::Misaligned {
                offset: facts.alignment_offset,
            });
        } else {
            return Err(GeomError::Misaligned {
                offset: facts.alignment_offset,
            });
        }
    }
    let media = overrides.media.unwrap_or(if facts.rotational {
        Media::Hdd
    } else {
        Media::Flash
    });

    // The stripe.
    let optimal = u64::from(facts.optimal_io_size);
    let optimal_usable = facts.optimal_io_size >= MIN_USEFUL_OPTIMAL
        && stripe_ok(optimal)
        && (facts.minimum_io_size == 0 || optimal % u64::from(facts.minimum_io_size.max(1)) == 0);
    let (mut stripe_bytes, mut chunk_bytes, mut data_disks, mut source) = match &facts.md {
        Some(md) => match md.data_disks() {
            Some(disks) if md.chunk_bytes > 0 => {
                let stripe = u64::from(md.chunk_bytes) * u64::from(disks);
                if !stripe_ok(stripe) {
                    return Err(GeomError::Unsupported {
                        reason: format!(
                            "md stripe of {stripe} bytes ({} x {} chunk) is outside the format's bounds",
                            disks, md.chunk_bytes
                        ),
                    });
                }
                if facts.optimal_io_size != 0 && optimal != stripe {
                    warnings.push(GeomWarning::OptimalIoDisagrees {
                        optimal_io_size: facts.optimal_io_size,
                    });
                }
                (stripe as u32, md.chunk_bytes, disks, GeomSource::MdSysfs)
            }
            // A striped level whose chunk size sysfs does not give: a
            // guessed stripe would turn every full-stripe write into a
            // read-modify-write on raid5 and raid6.
            Some(_) => {
                return Err(GeomError::Missing {
                    attr: "md/chunk_size",
                })
            }
            None => {
                // raid1 or linear: no striping; the write unit is a choice.
                if optimal_usable {
                    (
                        facts.optimal_io_size,
                        facts.optimal_io_size,
                        1,
                        GeomSource::OptimalIoSize,
                    )
                } else {
                    (DEFAULT_STRIPE, DEFAULT_STRIPE, 1, GeomSource::Default)
                }
            }
        },
        None => {
            if optimal_usable {
                (
                    facts.optimal_io_size,
                    facts.optimal_io_size,
                    1,
                    GeomSource::OptimalIoSize,
                )
            } else {
                if facts.kind == DeviceKind::HwRaidVd {
                    warnings.push(GeomWarning::OptimalIoUnknown);
                }
                (DEFAULT_STRIPE, DEFAULT_STRIPE, 1, GeomSource::Default)
            }
        }
    };
    if let Some(stripe) = overrides.stripe_bytes {
        if !stripe_ok(u64::from(stripe)) {
            return Err(GeomError::BadOverride {
                what: "stripe_bytes",
                value: u64::from(stripe),
            });
        }
        stripe_bytes = stripe;
        chunk_bytes = stripe;
        data_disks = 1;
        source = GeomSource::Manual;
    }
    if facts.max_sectors_kb > 0 && u64::from(stripe_bytes) > u64::from(facts.max_sectors_kb) << 10 {
        warnings.push(GeomWarning::StripeExceedsMaxSectors {
            stripe_bytes,
            max_bytes: u64::from(facts.max_sectors_kb) << 10,
        });
    }

    // The segment: the media's target, whole stripes, bounded above by the
    // format and by the pool (eight segments at least), below by one
    // stripe.
    let stripe = u64::from(stripe_bytes);
    let target = match (media, facts.kind) {
        (Media::Hdd, _) => SEGMENT_HDD,
        (Media::Flash, DeviceKind::Nvme) => SEGMENT_NVME,
        (Media::Flash, _) => SEGMENT_FLASH,
    };
    let mut segment_bytes = match overrides.segment_bytes {
        Some(s) => {
            if s == 0 || s % stripe != 0 || s > MAX_SEG_BYTES {
                return Err(GeomError::BadOverride {
                    what: "segment_bytes",
                    value: s,
                });
            }
            s
        }
        None => (target.div_ceil(stripe) * stripe).min(MAX_SEG_BYTES / stripe * stripe),
    };
    let fixed = fixed_bytes(stripe);
    let per_segment_budget =
        facts.size_bytes.saturating_sub(fixed) / MIN_SEGMENTS / stripe * stripe;
    if per_segment_budget < stripe {
        return Err(GeomError::TooSmall {
            size_bytes: facts.size_bytes,
            needed: fixed + MIN_SEGMENTS * stripe,
        });
    }
    if segment_bytes > per_segment_budget {
        if overrides.segment_bytes.is_some() {
            return Err(GeomError::TooSmall {
                size_bytes: facts.size_bytes,
                needed: fixed + MIN_SEGMENTS * segment_bytes,
            });
        }
        segment_bytes = per_segment_budget;
        warnings.push(GeomWarning::SmallDevice);
    }
    // The segment table counts segments in a `u32`: a device with more
    // gets larger segments (whole stripes, up to the format's bound), never
    // a silently shortened layout.
    let max_segments = u64::from(u32::MAX) - 1;
    let usable = facts.size_bytes.saturating_sub(fixed);
    if usable / segment_bytes > max_segments {
        let grown = usable.div_ceil(max_segments).div_ceil(stripe) * stripe;
        if overrides.segment_bytes.is_some() || grown > MAX_SEG_BYTES {
            return Err(GeomError::Unsupported {
                reason: format!(
                    "{} bytes in segments of {segment_bytes} are more segments than the table \
                     can count",
                    facts.size_bytes
                ),
            });
        }
        segment_bytes = grown;
        warnings.push(GeomWarning::SegmentGrown { segment_bytes });
    }

    // Flush semantics (E19 / E21) and the discard posture (D9).
    let flush_opaque = overrides
        .flush_opaque
        .unwrap_or(facts.kind == DeviceKind::HwRaidVd);
    let flush_policy = overrides.flush_policy.unwrap_or(if flush_opaque {
        FlushPolicy::TrustBarrierWithNvfail
    } else {
        FlushPolicy::RequireFuaOrFlush
    });
    if flush_opaque && flush_policy == FlushPolicy::TrustBarrierWithNvfail {
        warnings.push(GeomWarning::FlushOpaqueTrusted);
    }
    // A `discard_max_bytes` of zero is a device that cannot discard, or an
    // administrator who switched discards off: planned as `Never`.
    let discard = overrides.discard.unwrap_or_else(|| {
        if media == Media::Flash
            && facts.kind != DeviceKind::HwRaidVd
            && facts.discard_granularity > 0
            && facts.discard_max_bytes > 0
        {
            DiscardPolicy::WholeSegment
        } else {
            DiscardPolicy::Never
        }
    });
    if media == Media::Flash && discard == DiscardPolicy::Never && overrides.discard.is_none() {
        warnings.push(GeomWarning::DiscardUnavailable);
    }
    Ok(StripeGeometry {
        stripe_bytes,
        segment_bytes,
        capacity_bytes: facts.size_bytes,
        chunk_bytes,
        data_disks,
        source,
        media,
        flush_opaque,
        flush_policy,
        discard,
        kind: facts.kind,
        warnings,
    })
}

/// Bytes the layout's fixed regions take for a stripe: four superblock
/// slots and two intent slots, each whole stripes.
fn fixed_bytes(stripe: u64) -> u64 {
    let slot_stripes = (otap_format::SUPERBLOCK_SLOT_SIZE as u64).div_ceil(stripe);
    let intent_stripes = (otap_format::INTENT_SLOT_SIZE as u64).div_ceil(stripe);
    (4 * slot_stripes + 2 * intent_stripes) * stripe
}

/// Discovers and plans in one step.
pub fn plan_device(
    source: &dyn SysfsSource,
    name: &str,
    overrides: &Overrides,
) -> Result<StripeGeometry, GeomError> {
    let facts = discover(source, name)?;
    plan(&facts, overrides)
}
