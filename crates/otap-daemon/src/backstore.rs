//! Opening the pool's device: a block device through the geometry planner
//! and io_uring on Linux, a file anywhere.

use std::io::{Read, Seek, SeekFrom};
use std::path::Path;

use otap_backstore::FileBackstore;
use otap_control::{ControlError, ErrorCode};
use otap_cp::{Backstore, DiscardPolicy, Geometry, Media, MediaProfile};
use otap_format::{Superblock, Uuid, SUPERBLOCK_SIZE, SUPERBLOCK_SLOT_SIZE};

use crate::config::Overrides;

/// A device opened for the engine.
pub struct Opened {
    /// The backstore.
    pub backstore: Box<dyn Backstore>,
    /// Its profile.
    pub profile: MediaProfile,
    /// `block` or `file`.
    pub kind: &'static str,
    /// What the geometry planner said, when it ran.
    pub warnings: Vec<String>,
}

impl std::fmt::Debug for Opened {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Opened")
            .field("profile", &self.profile)
            .field("kind", &self.kind)
            .field("warnings", &self.warnings)
            .finish_non_exhaustive()
    }
}

/// The geometry recorded in a superblock (F15): stripe, segment and
/// capacity in bytes.
pub type RecordedGeometry = (u32, u32, u64);

/// The geometry a file gets when the operator says nothing: a 1 MiB stripe
/// and 64 MiB segments from 1 GiB up, a 64 KiB stripe and 2 MiB segments
/// below (the test files), flash media, no discards (a file keeps what it
/// holds).
fn file_geometry(size: u64, overrides: &Overrides) -> Result<(Geometry, Media), ControlError> {
    let small = size < (1 << 30);
    let stripe = overrides
        .stripe_bytes
        .unwrap_or(if small { 64 << 10 } else { 1 << 20 });
    let segment = overrides
        .segment_bytes
        .unwrap_or(if small { 2 << 20 } else { 64 << 20 });
    if stripe == 0 || !segment.is_multiple_of(u64::from(stripe)) {
        return Err(ControlError::new(
            ErrorCode::BadRequest,
            format!("a segment of {segment} bytes is not a whole number of {stripe}-byte stripes"),
        ));
    }
    let media = overrides.media().unwrap_or(Media::Flash);
    Ok((
        Geometry {
            stripe_bytes: stripe,
            segment_bytes: segment,
            capacity_bytes: size,
            flush_opaque: overrides.flush_opaque.unwrap_or(false),
        },
        media,
    ))
}

fn profile_for(
    geom: Geometry,
    media: Media,
    overrides: &Overrides,
    file: bool,
    cp_timer_ns: Option<u64>,
) -> MediaProfile {
    let mut profile = match media {
        Media::Hdd => MediaProfile::hdd(geom),
        Media::Flash => MediaProfile::flash(geom),
    };
    profile.flush_policy = overrides.flush_policy();
    if file {
        profile.discard = DiscardPolicy::Never;
    }
    if let Some(ns) = cp_timer_ns {
        profile.cp_timer_ns = ns;
    }
    profile
}

fn io_error(what: &str, path: &Path, e: &std::io::Error) -> ControlError {
    ControlError::new(ErrorCode::Io, format!("{what} {}: {e}", path.display()))
}

/// `true` when `path` is a block device (Linux).
fn is_block_device(path: &Path) -> bool {
    #[cfg(target_os = "linux")]
    {
        use std::os::unix::fs::FileTypeExt;
        std::fs::metadata(path).is_ok_and(|m| m.file_type().is_block_device())
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = path;
        false
    }
}

/// The size of a file or block device.
fn device_size(path: &Path) -> Result<u64, ControlError> {
    let mut f = std::fs::File::open(path).map_err(|e| io_error("cannot open", path, &e))?;
    let len = f
        .metadata()
        .map_err(|e| io_error("cannot stat", path, &e))?
        .len();
    if len > 0 {
        return Ok(len);
    }
    f.seek(SeekFrom::End(0))
        .map_err(|e| io_error("cannot size", path, &e))
}

/// The stripes a probe tries for the `A1` slot when `A0` does not verify:
/// every power of two from the smallest stripe to the largest.
const PROBE_STRIPES: [u64; 10] = [
    8 << 10,
    16 << 10,
    32 << 10,
    64 << 10,
    128 << 10,
    256 << 10,
    512 << 10,
    1 << 20,
    2 << 20,
    4 << 20,
];

/// Reads a superblock page at `offset`; `Ok(None)` when none verifies
/// there or the device ends first.
fn read_superblock(
    f: &mut std::fs::File,
    path: &Path,
    offset: u64,
) -> Result<Option<Superblock>, ControlError> {
    if f.seek(SeekFrom::Start(offset)).is_err() {
        return Ok(None);
    }
    let mut page = vec![0u8; SUPERBLOCK_SIZE];
    match f.read_exact(&mut page) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(e) => return Err(io_error("cannot read", path, &e)),
    }
    Ok(Superblock::read(&page).ok())
}

/// Finds a verifying superblock in the ring of `path`: `A0` at byte 0,
/// else, for every candidate stripe, `A1` at the stripe-rounded slot size
/// and `B0` and `B1` at the end of the device (F7 keeps two slots at each
/// end, so a pool whose start was wiped or torn still has two whole
/// commits). Returns the aggregate's identity and the recorded geometry
/// (F15), which a boot takes over the operator's overrides so the profile
/// matches the pool; `Ok(None)` when nothing verifies.
pub fn probe_superblock(path: &Path) -> Result<Option<(Uuid, RecordedGeometry)>, ControlError> {
    let mut f = std::fs::File::open(path).map_err(|e| io_error("cannot open", path, &e))?;
    if let Some(sb) = read_superblock(&mut f, path, 0)? {
        return Ok(sb.geometry().map(|g| (sb.agg_id, g)));
    }
    let size = device_size(path)?;
    for stripe in PROBE_STRIPES {
        let slot_bytes = (SUPERBLOCK_SLOT_SIZE as u64).div_ceil(stripe) * stripe;
        // A1, then B0 and B1 under the capacity the layout was given: the
        // device's size, or the size rounded down to the stripe.
        let mut offsets = vec![slot_bytes];
        for capacity in [size, size / stripe * stripe] {
            if capacity >= 2 * slot_bytes {
                let b_base = capacity - 2 * slot_bytes;
                offsets.push(b_base);
                offsets.push(b_base + slot_bytes);
            }
        }
        offsets.dedup();
        for offset in offsets {
            if let Some(sb) = read_superblock(&mut f, path, offset)? {
                if let Some(g) = sb.geometry() {
                    let at_a1 = offset == slot_bytes;
                    if u64::from(g.0) == stripe && (at_a1 || g.2 <= size) {
                        return Ok(Some((sb.agg_id, g)));
                    }
                }
            }
        }
    }
    Ok(None)
}

/// `true` when the first or the last MiB of `path` holds a non-zero byte:
/// a filesystem signature, another pool's slots, someone's data. What
/// `pool format` refuses to overwrite without `--force`.
pub fn holds_data(path: &Path) -> Result<bool, ControlError> {
    const WINDOW: u64 = 1 << 20;
    let size = device_size(path)?;
    if size == 0 {
        return Ok(false);
    }
    let mut f = std::fs::File::open(path).map_err(|e| io_error("cannot open", path, &e))?;
    let mut buf = vec![0u8; WINDOW.min(size) as usize];
    let mut starts = vec![0u64];
    if size > WINDOW {
        starts.push(size - WINDOW);
    }
    for start in starts {
        f.seek(SeekFrom::Start(start))
            .map_err(|e| io_error("cannot seek", path, &e))?;
        f.read_exact(&mut buf)
            .map_err(|e| io_error("cannot read", path, &e))?;
        if buf.iter().any(|b| *b != 0) {
            return Ok(true);
        }
    }
    Ok(false)
}

/// Opens `path` for the engine. `create_bytes` creates a file of that size
/// when the path does not exist (a format); otherwise the file or block
/// device must exist. The geometry comes from the planner for a block
/// device, from `recorded` (a probed superblock) when given, else from the
/// overrides and the defaults; `cp_timer_ns` replaces the profile's CP
/// timer when given.
pub fn open_backstore(
    path: &Path,
    overrides: &Overrides,
    create_bytes: Option<u64>,
    recorded: Option<RecordedGeometry>,
    cp_timer_ns: Option<u64>,
) -> Result<Opened, ControlError> {
    if is_block_device(path) {
        return open_block(path, overrides, cp_timer_ns);
    }
    if !path.exists() {
        let Some(size) = create_bytes else {
            return Err(ControlError::new(
                ErrorCode::NotFound,
                format!(
                    "{} does not exist; give --size to create a file",
                    path.display()
                ),
            ));
        };
        let (geom, media) = file_geometry(size, overrides)?;
        let profile = profile_for(geom, media, overrides, true, cp_timer_ns);
        let backstore = FileBackstore::create(path, profile)
            .map_err(|e| io_error("cannot create", path, &e))?;
        return Ok(Opened {
            backstore: Box::new(backstore),
            profile,
            kind: "file",
            warnings: Vec::new(),
        });
    }
    let size = device_size(path)?;
    let (mut geom, media) = file_geometry(size, overrides)?;
    if let Some((stripe, segment, capacity)) = recorded {
        if let Some(s) = overrides.stripe_bytes {
            if s != stripe {
                return Err(ControlError::new(
                    ErrorCode::BadRequest,
                    format!("the pool was formatted with a {stripe}-byte stripe, not {s}"),
                ));
            }
        }
        if let Some(s) = overrides.segment_bytes {
            if s != u64::from(segment) {
                return Err(ControlError::new(
                    ErrorCode::BadRequest,
                    format!("the pool was formatted with a {segment}-byte segment, not {s}"),
                ));
            }
        }
        if capacity > size {
            return Err(ControlError::new(
                ErrorCode::BadRequest,
                format!(
                    "the pool was laid out over {capacity} bytes; {} holds {size}",
                    path.display()
                ),
            ));
        }
        geom.stripe_bytes = stripe;
        geom.segment_bytes = u64::from(segment);
        geom.capacity_bytes = capacity;
    }
    let profile = profile_for(geom, media, overrides, true, cp_timer_ns);
    let backstore =
        FileBackstore::open(path, profile).map_err(|e| io_error("cannot open", path, &e))?;
    Ok(Opened {
        backstore: Box::new(backstore),
        profile,
        kind: "file",
        warnings: Vec::new(),
    })
}

#[cfg(target_os = "linux")]
fn open_block(
    path: &Path,
    overrides: &Overrides,
    cp_timer_ns: Option<u64>,
) -> Result<Opened, ControlError> {
    use otap_backstore::UringBackstore;
    use otap_geom::{plan_device, Sysfs};
    let name = path
        .file_name()
        .and_then(|n| n.to_str())
        .ok_or_else(|| ControlError::new(ErrorCode::BadRequest, "a device needs a name"))?;
    let plan = plan_device(&Sysfs::new(name), name, &overrides.geom_overrides()).map_err(|e| {
        ControlError::new(
            ErrorCode::Io,
            format!("cannot plan the geometry of {}: {e}", path.display()),
        )
    })?;
    let mut profile = plan.media_profile();
    profile.flush_policy = overrides.flush_policy();
    if let Some(ns) = cp_timer_ns {
        profile.cp_timer_ns = ns;
    }
    let warnings = plan.warnings.iter().map(|w| format!("{w:?}")).collect();
    let backstore =
        UringBackstore::open(path, profile).map_err(|e| io_error("cannot open", path, &e))?;
    Ok(Opened {
        backstore: Box::new(backstore),
        profile,
        kind: "block",
        warnings,
    })
}

#[cfg(not(target_os = "linux"))]
fn open_block(
    path: &Path,
    _overrides: &Overrides,
    _cp_timer_ns: Option<u64>,
) -> Result<Opened, ControlError> {
    Err(ControlError::new(
        ErrorCode::Unsupported,
        format!(
            "{} is a block device; block devices are served on Linux only",
            path.display()
        ),
    ))
}
