//! A booted pool: the engine over its backstore and arena, the ublk
//! devices of its online volumes (Linux, or the tests' in-memory driver),
//! and every operation the control plane runs against it. Everything here
//! runs on the engine thread, so an operation ticks the engine itself
//! while it waits for the consistency point it needs, and the data path
//! keeps flowing meanwhile. The devices' recovery after a crash and the
//! reaper are in its `devices` module (Step 6B).

mod devices;

use std::collections::{BTreeMap, HashSet, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::TryRecvError;
use std::sync::Arc;
use std::time::{Duration, Instant};

use otap_control::{
    check_arena_bytes, check_cp_interval, decode_hex, encode_hex, ArenaKind, ControlError,
    ErrorCode, IoResultReport, Reply, Request, Response, SnapshotReport, VolumeReport,
    MAX_IO_BLOCKS,
};
use otap_cp::{
    Backstore, Clock, DiskLayout, Engine, EngineConfig, EngineError, Event, Media, RequestKind,
    Status,
};
use otap_format::{SnapKind, SnapOrigin, Uuid, BLOCK_SIZE, MAX_VOLS, PIN_MASK, PIN_REPLICATION};
use otap_ublk::{device_stamp, pool_fingerprint, DeviceSpec};

use crate::arena::open_pool_arena;
use crate::backstore::{holds_data, open_backstore, probe_superblock};
use crate::bindings::{Bindings, StaleReason};
use crate::clock::SystemClock;
use crate::config::Overrides;
use crate::device::{destroy_device, volume_device, HostRef, VolumeDevice};
use crate::frontdoor::{blocks_of, DaemonFrontDoor, IoCompletion, IoHandle};
use crate::report::{pool_report, snapshot_report, verify_report, volume_report};

/// The engine as the daemon runs it.
pub type PoolEngine = Engine<Box<dyn Backstore>, DaemonFrontDoor, SystemClock>;

/// What a boot is told.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BootSpec {
    /// The device the pool is on.
    pub device: PathBuf,
    /// Geometry and policy overrides (the recorded geometry wins).
    pub overrides: Overrides,
    /// Arena bytes.
    pub arena_bytes: u64,
    /// Where the arena lives.
    pub arena: ArenaKind,
    /// The CP timer in seconds.
    pub cp_interval_secs: u32,
    /// Expose ublk devices (when the daemon has a ublk control plane).
    pub ublk: bool,
    /// How long an operation may wait for its consistency point.
    pub op_timeout: Duration,
    /// Recreate an arena file made for another size even when it may hold
    /// acknowledged writes (`dbladed --discard-arena`; see
    /// [`crate::arena::open_pool_arena`]).
    pub discard_arena: bool,
    /// Where the record of the pool's ublk devices lives
    /// ([`crate::bindings`]).
    pub state_dir: PathBuf,
    /// How long the boot waits for the driver to quiesce a dead daemon's
    /// device before it gives the volume a new one.
    pub recovery_timeout: Duration,
}

/// A device the pool serves for a volume.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Served {
    dev_id: u32,
    path: String,
}

/// Lines of recent events the pool keeps.
const LOG_LINES: usize = 64;

/// The sleep between ticks that did nothing while an operation waits.
const IDLE_TICK: Duration = Duration::from_micros(200);

static AGG_COUNTER: AtomicU64 = AtomicU64::new(0);

/// A fresh aggregate identity: 16 bytes of a BLAKE3 digest over the
/// clocks, the process and a counter, with the version-4 and variant bits
/// set so it reads as a UUID.
fn new_agg_id(clock: &SystemClock) -> Uuid {
    let mut hasher = blake3::Hasher::new();
    hasher.update(b"opentap aggregate identity");
    hasher.update(&clock.wall_ns().to_le_bytes());
    hasher.update(&clock.mono_ns().to_le_bytes());
    hasher.update(&u64::from(std::process::id()).to_le_bytes());
    hasher.update(&AGG_COUNTER.fetch_add(1, Ordering::Relaxed).to_le_bytes());
    let digest = hasher.finalize();
    let mut bytes = [0u8; 16];
    bytes.copy_from_slice(&digest.as_bytes()[..16]);
    bytes[6] = (bytes[6] & 0x0f) | 0x40;
    bytes[8] = (bytes[8] & 0x3f) | 0x80;
    Uuid::from_bytes(bytes)
}

/// An engine refusal as the protocol reports it.
#[must_use]
pub fn engine_error(e: EngineError) -> ControlError {
    let code = match &e {
        EngineError::NoSuchVolume { .. }
        | EngineError::NoSuchSnapshot { .. }
        | EngineError::NoSuchName { .. } => ErrorCode::NotFound,
        EngineError::BadSize { .. }
        | EngineError::NameExists { .. }
        | EngineError::MetadataTooLarge { .. }
        | EngineError::Format(_) => ErrorCode::BadRequest,
        EngineError::Unsupported(_) => ErrorCode::Unsupported,
        EngineError::Io(_) => ErrorCode::Io,
        _ => ErrorCode::Engine,
    };
    ControlError::new(code, e.to_string())
}

fn bad_request(message: impl Into<String>) -> ControlError {
    ControlError::new(ErrorCode::BadRequest, message)
}

/// A request's status as a word.
#[must_use]
pub fn status_word(status: Status) -> &'static str {
    match status {
        Status::Ok => "ok",
        Status::NoSuchVolume => "no_such_volume",
        Status::OutOfRange => "out_of_range",
        Status::NvFailed => "nvfailed",
        Status::Corrupt => "corrupt",
        Status::Stopped => "stopped",
        Status::Failed => "failed",
        Status::NoSpace => "no_space",
        Status::TooLarge => "too_large",
        Status::Offline => "offline",
        _ => "unknown",
    }
}

/// Whole 4 KiB blocks for a byte size, rounded up; zero is refused.
fn blocks_for(size_bytes: u64) -> Result<u64, ControlError> {
    if size_bytes == 0 {
        return Err(bad_request("a size of zero"));
    }
    Ok(size_bytes.div_ceil(BLOCK_SIZE as u64))
}

/// A booted pool.
pub struct Pool {
    engine: PoolEngine,
    io: IoHandle,
    device: String,
    arena_bytes: u64,
    arena_word: String,
    arena_slots: u32,
    cp_interval_secs: u32,
    last_boot: String,
    rotational: bool,
    stripe_bytes: u32,
    /// The ublk control plane, when the pool exposes devices.
    host: Option<HostRef>,
    /// The devices served, by volume.
    devices: BTreeMap<u32, Served>,
    /// The record of the devices, kept in `state_dir` ([`crate::bindings`]).
    bindings: Bindings,
    state_dir: PathBuf,
    /// The pool's fingerprint, which its devices' stamps carry.
    pool_fp: u32,
    recovery_timeout: Duration,
    op_timeout: Duration,
    log: VecDeque<String>,
    /// The daemon's stop flag (a signal): an operation waiting for its
    /// consistency point gives up when it is set, so the shutdown is not
    /// delayed by the operation timeout.
    stop: Option<Arc<AtomicBool>>,
    /// The shutdown's own waits are exempt from the flag.
    stopping: bool,
}

impl std::fmt::Debug for Pool {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Pool")
            .field("device", &self.device)
            .field("cp_seq", &self.engine.cp_seq())
            .field("devices", &self.devices)
            .finish_non_exhaustive()
    }
}

impl Pool {
    /// Formats `device` as a new pool: a file is created at `size_bytes`
    /// when it does not exist; a device that already holds a pool, or
    /// anything else (a first or last MiB that is not blank), is refused
    /// unless `force`.
    pub fn format(
        device: &Path,
        overrides: &Overrides,
        size_bytes: Option<u64>,
        force: bool,
        clock: &SystemClock,
    ) -> Reply {
        if !force && device.exists() {
            if let Some((agg_id, _)) = probe_superblock(device)? {
                return Err(bad_request(format!(
                    "{} already holds pool {agg_id}; pass --force to overwrite it",
                    device.display()
                )));
            }
            if holds_data(device)? {
                return Err(bad_request(format!(
                    "{} holds data (its first or last MiB is not blank); pass --force to \
                     overwrite it",
                    device.display()
                )));
            }
        }
        let mut opened = open_backstore(device, overrides, size_bytes, None, None)?;
        let agg_id = new_agg_id(clock);
        let cfg = EngineConfig::new(agg_id, Vec::new());
        PoolEngine::format(&mut opened.backstore, clock, &cfg).map_err(engine_error)?;
        let layout = DiskLayout::new(&opened.profile.geom)
            .map_err(|e| bad_request(format!("the geometry cannot be laid out: {e}")))?;
        Ok(Response::Formatted {
            agg_id: agg_id.to_string(),
            stripe_bytes: layout.stripe_bytes,
            segment_bytes: layout.segment_bytes,
            capacity_bytes: layout.capacity_bytes,
            segments: layout.segments,
        })
    }

    /// Boots the pool on `spec.device`: opens the device under the recorded
    /// geometry, the arena, and the engine; ticks until the boot commit is
    /// durable; then, when `spec.ublk` and the daemon has a ublk control
    /// plane (`host`), re-attaches the devices a killed daemon left to the
    /// online volumes they exposed (Step 6B, its `devices` module) and creates one for
    /// every other online volume. `stop` is the daemon's stop flag, which
    /// interrupts an operation's wait.
    pub fn boot(
        spec: &BootSpec,
        clock: &SystemClock,
        stop: Option<Arc<AtomicBool>>,
        host: Option<HostRef>,
    ) -> Result<Self, ControlError> {
        check_arena_bytes(spec.arena_bytes).map_err(bad_request)?;
        check_cp_interval(spec.cp_interval_secs).map_err(bad_request)?;
        if !spec.device.exists() {
            return Err(ControlError::new(
                ErrorCode::NotFound,
                format!("{} does not exist", spec.device.display()),
            ));
        }
        let Some((agg_id, recorded)) = probe_superblock(&spec.device)? else {
            return Err(ControlError::new(
                ErrorCode::NotFound,
                format!(
                    "{} holds no OpenTAP superblock in its ring (neither at its start nor at \
                     its end); format it first (`otapctl pool format`)",
                    spec.device.display()
                ),
            ));
        };
        let host = if spec.ublk { host } else { None };
        if let Some(host) = &host {
            devices::refuse_a_second_server(host, pool_fingerprint(&agg_id))?;
        }
        let cp_timer_ns = u64::from(spec.cp_interval_secs) * 1_000_000_000;
        let opened = open_backstore(
            &spec.device,
            &spec.overrides,
            None,
            Some(recorded),
            Some(cp_timer_ns),
        )?;
        let profile = opened.profile;
        let mut backstore = opened.backstore;
        // An arena file of another size is judged before it is resized:
        // whether the pool shut down cleanly is read from the device only
        // then, with the boot's own evaluation.
        let memory = open_pool_arena(
            &spec.arena,
            spec.arena_bytes,
            &agg_id,
            spec.discard_arena,
            || {
                let layout = DiskLayout::new(&profile.geom)
                    .map_err(|e| bad_request(format!("the geometry cannot be laid out: {e}")))?;
                let state =
                    otap_cp::boot::evaluate(&mut backstore, &layout, &agg_id).map_err(|e| {
                        ControlError::new(
                            ErrorCode::Engine,
                            format!("cannot read {}: {e}", spec.device.display()),
                        )
                    })?;
                Ok(state.is_clean())
            },
        )?;
        let arena_slots = otap_arena::slots_for_len(memory.len())
            .map_err(|e| bad_request(format!("the arena cannot be laid out: {e}")))?;
        let mut cfg = EngineConfig::new(agg_id, Vec::new());
        cfg.discard_arena = spec.discard_arena;
        // The engine's boot re-attaches the arena: the devices' recovery is
        // timed from here (its target is 2 s, `otap_ublk::recovery`).
        let reattach_began = Instant::now();
        let mut engine = PoolEngine::boot(
            cfg,
            backstore,
            DaemonFrontDoor::new(),
            clock.clone(),
            memory,
        )
        .map_err(|f| {
            ControlError::new(
                ErrorCode::Engine,
                format!("cannot boot {}: {}", spec.device.display(), f.error),
            )
        })?;
        let io = engine.front_door().io_handle();
        let mut pool = Self {
            engine,
            io,
            device: spec.device.display().to_string(),
            arena_bytes: spec.arena_bytes,
            arena_word: spec.arena.describe(),
            arena_slots,
            cp_interval_secs: spec.cp_interval_secs,
            last_boot: "booting".to_string(),
            rotational: profile.media == Media::Hdd,
            stripe_bytes: profile.geom.stripe_bytes,
            host,
            devices: BTreeMap::new(),
            bindings: Bindings::new(agg_id),
            state_dir: spec.state_dir.clone(),
            pool_fp: pool_fingerprint(&agg_id),
            recovery_timeout: spec.recovery_timeout,
            op_timeout: spec.op_timeout,
            log: VecDeque::new(),
            stop,
            stopping: false,
        };
        for warning in &opened.warnings {
            pool.log(format!("geometry: {warning}"));
        }
        pool.wait_until("the boot commit", |e| e.is_serving())?;
        // The devices a killed daemon left are re-attached before any new
        // one is made, so a volume keeps its `/dev/ublkbN` (Step 6B).
        if pool.host.is_some() {
            pool.load_bindings();
            if let Err(e) = pool.reconcile_devices(reattach_began) {
                let _ = pool.stop();
                return Err(e);
            }
        }
        let online: Vec<u32> = pool
            .engine
            .volumes()
            .into_iter()
            .filter(|(_, v)| !v.offline)
            .map(|(vol, _)| vol)
            .collect();
        for vol in online {
            if let Err(e) = pool.ensure_device(vol) {
                let _ = pool.stop();
                return Err(e);
            }
        }
        Ok(pool)
    }

    /// The device the pool is on.
    #[must_use]
    pub fn device(&self) -> &str {
        &self.device
    }

    /// The engine, for a checker.
    #[must_use]
    pub fn engine(&self) -> &PoolEngine {
        &self.engine
    }

    /// The block device path of `vol`, if one is exposed.
    #[must_use]
    pub fn device_path(&self, vol: u32) -> Option<&str> {
        self.devices.get(&vol).map(|s| s.path.as_str())
    }

    /// The ublk device id of `vol`, if one is exposed.
    #[must_use]
    pub fn device_id(&self, vol: u32) -> Option<u32> {
        self.devices.get(&vol).map(|s| s.dev_id)
    }

    /// The record of the pool's ublk devices as it stands.
    #[must_use]
    pub const fn bindings(&self) -> &Bindings {
        &self.bindings
    }

    /// The recent noteworthy events, oldest first.
    #[must_use]
    pub fn recent_events(&self) -> Vec<String> {
        self.log.iter().cloned().collect()
    }

    /// A line for the daemon's journal and the pool's recent-events log.
    fn log(&mut self, line: String) {
        eprintln!("dbladed: {line}");
        if self.log.len() == LOG_LINES {
            self.log.pop_front();
        }
        self.log.push_back(line);
    }

    /// Records what an event means, for the journal.
    fn note(&mut self, ev: &Event) {
        let line = match ev {
            Event::Booted {
                cp_seq,
                dirty,
                reattached,
                replayed_blocks,
                rewound,
                arena_lost,
                damaged_slots,
                orphaned_blocks,
                nvfail,
                torn,
            } => {
                let held = (0..MAX_VOLS as u32).filter(|v| nvfail.get(*v)).count();
                let mut s = format!(
                    "{} boot at cp {cp_seq}",
                    if *dirty { "dirty" } else { "clean" }
                );
                if *rewound {
                    s.push_str(", rewound past a torn commit");
                }
                if *reattached {
                    s.push_str(&format!(
                        ", arena re-attached ({replayed_blocks} blocks replayed)"
                    ));
                }
                if *arena_lost {
                    s.push_str(", the arena memory held no usable arena");
                }
                if *damaged_slots > 0 {
                    s.push_str(&format!(", {damaged_slots} damaged arena slots dropped"));
                }
                if *orphaned_blocks > 0 {
                    s.push_str(&format!(", {orphaned_blocks} orphaned blocks dropped"));
                }
                if held > 0 {
                    s.push_str(&format!(", {held} volume(s) held by nvfail"));
                }
                for (cp, why) in torn {
                    s.push_str(&format!(", torn cp {cp}: {why}"));
                }
                self.last_boot = s.clone();
                s
            }
            Event::CpFailed { cp_seq, reason } => format!("cp {cp_seq} failed: {reason}"),
            Event::Folded { cp_seq, pages } => format!("cp {cp_seq} folded the map ({pages} pages)"),
            Event::NvfailCleared { vol, rollback_gen } => {
                format!("nvfail cleared on volume {vol}, rollback generation {rollback_gen}")
            }
            Event::VolumeCreated { vol, cp_seq } => format!("volume {vol} created at cp {cp_seq}"),
            Event::SnapshotCreated {
                snap_id,
                vol,
                cp_seq,
            } => format!("snapshot {snap_id} of volume {vol} taken at cp {cp_seq}"),
            Event::SnapshotFailed { uuid, vol, reason } => {
                format!("snapshot {uuid} of volume {vol} failed: {reason}")
            }
            Event::SnapshotDeleted {
                snap_id,
                vol,
                cp_seq,
                released,
            } => format!(
                "snapshot {snap_id} of volume {vol} deleted at cp {cp_seq}, {released} blocks released"
            ),
            Event::SnapshotPinned { snap_id, flags } => {
                format!("snapshot {snap_id} pins are now {flags:#04x}")
            }
            Event::VolumeRolledBack {
                vol,
                snap_id,
                cp_seq,
                rollback_gen,
            } => format!(
                "volume {vol} rolled back to snapshot {snap_id} at cp {cp_seq}, generation {rollback_gen}"
            ),
            Event::VolumeResized {
                vol,
                size_blocks,
                cp_seq,
            } => format!("volume {vol} resized to {size_blocks} blocks at cp {cp_seq}"),
            Event::VolumeDeleted { vol, cp_seq } => format!("volume {vol} deleted at cp {cp_seq}"),
            Event::SegmentCleaned {
                pseg,
                relocated,
                cp_seq,
            } => format!("segment {pseg} cleaned at cp {cp_seq}, {relocated} blocks relocated"),
            Event::OperatorWarning(w) => format!("warning: {w}"),
            Event::ArenaDamage { vol, lba, slot } => {
                format!("arena slot {slot} (volume {vol}, lba {lba}) failed its CRC")
            }
            Event::Internal { what } => format!("internal: {what}"),
            Event::Stopped { cp_seq, clean } => format!(
                "stopped at cp {cp_seq}, {}",
                if *clean { "clean" } else { "without a clean shutdown record" }
            ),
            Event::VerifyFinished {
                cp_seq,
                state,
                verdict,
                errors,
                warnings,
            } => format!(
                "verification of cp {cp_seq} ended {state:?}: {verdict:?}, {errors} error(s), \
                 {warnings} warning(s)"
            ),
            _ => return,
        };
        self.log(line);
    }

    /// One tick; `true` when the engine has work in hand.
    fn tick_raw(&mut self) -> Result<bool, ControlError> {
        let before = self.engine.stats();
        self.engine
            .tick()
            .map_err(|e| ControlError::new(ErrorCode::Engine, format!("engine tick: {e}")))?;
        // A verification in progress is work in hand: the loop ticks again
        // at once, each tick one bounded slice of it.
        Ok(self.engine.stats() != before
            || !self.engine.is_quiet()
            || self.engine.front_door().in_engine() > 0
            || self.engine.verify_running())
    }

    /// One tick with its events noted; `true` when the engine has work in
    /// hand (the idle loop then ticks again at once).
    pub fn tick(&mut self) -> Result<bool, ControlError> {
        let busy = self.tick_raw()?;
        for ev in self.engine.take_events() {
            self.note(&ev);
        }
        Ok(busy)
    }

    /// `true` when a signal asked the daemon to stop while an operation
    /// waits: the operation gives up (what it asked of the engine may still
    /// land) so the clean shutdown can start at once. The shutdown's own
    /// waits are exempt.
    fn interrupted(&self) -> bool {
        !self.stopping
            && self
                .stop
                .as_ref()
                .is_some_and(|flag| flag.load(Ordering::Relaxed))
    }

    fn interrupted_error(what: &str) -> ControlError {
        ControlError::new(
            ErrorCode::Engine,
            format!("the daemon is shutting down; {what} was not waited for"),
        )
    }

    /// Ticks until `pred` recognises an event, within the operation
    /// timeout.
    fn wait_for<T>(
        &mut self,
        what: &str,
        mut pred: impl FnMut(&Event) -> Option<T>,
    ) -> Result<T, ControlError> {
        let deadline = Instant::now() + self.op_timeout;
        loop {
            let busy = self.tick_raw()?;
            let mut found = None;
            let mut stopped = false;
            for ev in self.engine.take_events() {
                self.note(&ev);
                if found.is_none() {
                    found = pred(&ev);
                }
                if matches!(ev, Event::Stopped { .. }) {
                    stopped = true;
                }
            }
            if let Some(t) = found {
                return Ok(t);
            }
            if stopped || self.engine.is_stopped() {
                return Err(ControlError::new(
                    ErrorCode::Engine,
                    format!("the engine stopped while waiting for {what}"),
                ));
            }
            if Instant::now() >= deadline {
                return Err(ControlError::new(
                    ErrorCode::Timeout,
                    format!(
                        "{what} did not complete within {} s",
                        self.op_timeout.as_secs()
                    ),
                ));
            }
            if self.interrupted() {
                return Err(Self::interrupted_error(what));
            }
            if !busy {
                std::thread::sleep(IDLE_TICK);
            }
        }
    }

    /// Ticks until `done` holds, within the operation timeout.
    fn wait_until(
        &mut self,
        what: &str,
        mut done: impl FnMut(&mut PoolEngine) -> bool,
    ) -> Result<(), ControlError> {
        let deadline = Instant::now() + self.op_timeout;
        loop {
            if done(&mut self.engine) {
                return Ok(());
            }
            if self.engine.is_stopped() {
                return Err(ControlError::new(
                    ErrorCode::Engine,
                    format!("the engine stopped while waiting for {what}"),
                ));
            }
            if Instant::now() >= deadline {
                return Err(ControlError::new(
                    ErrorCode::Timeout,
                    format!(
                        "{what} did not complete within {} s",
                        self.op_timeout.as_secs()
                    ),
                ));
            }
            if self.interrupted() {
                return Err(Self::interrupted_error(what));
            }
            let busy = self.tick()?;
            if !busy {
                std::thread::sleep(IDLE_TICK);
            }
        }
    }

    /// Waits for the engine to go quiet, except on a full pool, which
    /// holds a frozen generation until a consistency point is requested
    /// and so never goes quiet on its own: the request is the way out.
    fn wait_quiet(&mut self) -> Result<(), ControlError> {
        if self.engine.is_pool_full() {
            return Ok(());
        }
        self.wait_until("the engine to go quiet", |e| e.is_quiet())
    }

    /// Waits for the next commit, or for the failure of the attempt (a
    /// full pool), which is the answer rather than a timeout.
    fn wait_cp_outcome(&mut self, what: &str) -> Result<u64, ControlError> {
        let outcome = self.wait_for(what, |ev| match ev {
            Event::CpCommitted { cp_seq, .. } => Some(Ok(*cp_seq)),
            Event::CpFailed { cp_seq, reason } => {
                Some(Err(format!("consistency point {cp_seq} failed: {reason}")))
            }
            _ => None,
        })?;
        outcome.map_err(|message| {
            ControlError::new(
                ErrorCode::Engine,
                format!(
                    "{message}; if the pool is full, delete a snapshot or punch holes, then \
                     `pool clean`"
                ),
            )
        })
    }

    /// A consistency point that carries everything asked of the engine so
    /// far: waits for the engine to go quiet, requests one, waits for its
    /// commit. Returns its sequence.
    fn sync_now(&mut self) -> Result<u64, ControlError> {
        self.wait_quiet()?;
        self.engine.request_cp();
        self.wait_cp_outcome("the consistency point")
    }

    /// The volume `name` names, or, as `#<index>`, the volume at that
    /// index: how a volume whose name was lost with a damaged catalogue is
    /// reached (and named again).
    fn vol_index(&self, name: &str) -> Result<u32, ControlError> {
        if let Some(vol) = self.engine.find_volume(name) {
            return Ok(vol);
        }
        if let Some(index) = name.strip_prefix('#').and_then(|n| n.parse::<u32>().ok()) {
            if self.engine.volume(index).is_some_and(|v| v.present) {
                return Ok(index);
            }
        }
        Err(ControlError::new(
            ErrorCode::NotFound,
            format!("no volume named {name:?}"),
        ))
    }

    fn snap_index(&self, vol: u32, name: &str) -> Result<u64, ControlError> {
        self.engine.find_snapshot(vol, name).ok_or_else(|| {
            ControlError::new(
                ErrorCode::NotFound,
                format!(
                    "no snapshot named {name:?} on volume {:?}",
                    self.engine.volume_name(vol).unwrap_or("?")
                ),
            )
        })
    }

    fn volume_report_for(&self, vol: u32) -> Option<VolumeReport> {
        let v = self.engine.volume(vol).filter(|v| v.present)?;
        let snapshots = self
            .engine
            .snapshots()
            .iter()
            .filter(|s| s.vol == vol && !s.deleting)
            .count();
        Some(volume_report(
            vol,
            &v,
            self.devices.get(&vol).map(|s| s.path.clone()),
            snapshots,
        ))
    }

    fn report_volume(&self, vol: u32) -> Reply {
        self.volume_report_for(vol)
            .map(Response::Volume)
            .ok_or_else(|| ControlError::new(ErrorCode::NotFound, format!("volume {vol} is gone")))
    }

    fn snapshot_reports(&self, vol: Option<u32>) -> Vec<SnapshotReport> {
        self.engine
            .snapshots()
            .iter()
            .filter(|s| vol.is_none_or(|v| s.vol == v))
            .map(|s| snapshot_report(s, self.engine.volume_name(s.vol).map(str::to_string)))
            .collect()
    }

    fn report_snapshot(&self, snap_id: u64) -> Reply {
        self.engine
            .snapshots()
            .iter()
            .find(|s| s.snap_id == snap_id)
            .map(|s| {
                Response::Snapshot(snapshot_report(
                    s,
                    self.engine.volume_name(s.vol).map(str::to_string),
                ))
            })
            .ok_or_else(|| {
                ControlError::new(ErrorCode::NotFound, format!("snapshot {snap_id} is gone"))
            })
    }

    /// The shape of the device of a volume of `size_blocks` blocks.
    fn device_spec(&self, size_blocks: u64) -> DeviceSpec {
        DeviceSpec::new(size_blocks, self.stripe_bytes, self.rotational).for_arena(self.arena_slots)
    }

    /// Exposes the ublk device of `vol` when the pool serves devices, the
    /// volume is online and no device exists yet: the device is created
    /// with the pool's stamp for the volume, recorded in the bindings
    /// before it is started (so a crash from there on leaves it findable),
    /// handed to the engine, and started while the engine serves it.
    fn ensure_device(&mut self, vol: u32) -> Result<(), ControlError> {
        let Some(host) = self.host.clone() else {
            return Ok(());
        };
        if self.devices.contains_key(&vol) {
            return Ok(());
        }
        let Some(st) = self.engine.volume(vol).filter(|v| v.present && !v.offline) else {
            return Ok(());
        };
        let spec = self.device_spec(st.size_blocks);
        let stamp = device_stamp(self.pool_fp, vol);
        let created = host.borrow_mut().create(&spec, stamp);
        let device = created.map_err(|e| {
            ControlError::new(
                ErrorCode::Io,
                format!("cannot create the ublk device of volume {vol}: {e}"),
            )
        })?;
        let dev_id = device.dev_id();
        self.bindings.bind(vol, dev_id, st.size_blocks);
        self.save_bindings();
        let dev = match volume_device(device, vol, st.size_blocks) {
            Ok(dev) => dev,
            Err(e) => {
                self.bindings.forget(dev_id);
                self.save_bindings();
                return Err(e);
            }
        };
        // The engine serves the queue from here on, so the reads the
        // driver's start makes through it are answered (Step 6A).
        self.engine.front_door().add_device(vol, dev);
        if let Err(e) = self.start_device(vol) {
            self.abandon_device(vol, StaleReason::StartAbandoned);
            return Err(e);
        }
        let path = format!("/dev/ublkb{dev_id}");
        self.devices.insert(
            vol,
            Served {
                dev_id,
                path: path.clone(),
            },
        );
        self.log(format!("volume {vol} is {path}"));
        Ok(())
    }

    /// Brings live the device of `vol` that the engine's front door holds:
    /// `START_DEV` for a created device, `END_USER_RECOVERY` for a
    /// re-attached one, issued without waiting and polled between ticks of
    /// the engine, which serve the device's queue meanwhile, because the
    /// driver's partition scan reads a new disk through that queue before
    /// `START_DEV` completes (Step 6A; waiting for it here instead never
    /// returned for a privileged daemon) and `END_USER_RECOVERY` releases
    /// the requests a dead daemon held into it. Bounded by the operation
    /// timeout and the stop flag.
    fn start_device(&mut self, vol: u32) -> Result<(), ControlError> {
        use otap_ublk::{DeviceControl, StartError, TransportError};
        const GONE: TransportError = TransportError {
            what: "the device left the front door",
            errno: 19,
        };
        let deadline = Instant::now() + self.op_timeout;
        let started = otap_ublk::start_while_serving(
            self,
            |p| match p.engine.front_door().device_mut(vol) {
                Some(dev) => DeviceControl::begin_start(&mut *dev.device),
                None => Err(GONE),
            },
            |p| match p.engine.front_door().device_mut(vol) {
                Some(dev) => DeviceControl::poll_start(&mut *dev.device),
                None => Err(GONE),
            },
            |p| {
                if p.interrupted() {
                    return Err(Self::interrupted_error("the ublk device's start"));
                }
                if !p.tick()? {
                    std::thread::sleep(IDLE_TICK);
                }
                Ok(())
            },
            || Instant::now() >= deadline,
        );
        started.map_err(|e| match e {
            StartError::Serve(e) => e,
            StartError::TimedOut => ControlError::new(
                ErrorCode::Timeout,
                format!(
                    "the ublk device of volume {vol} did not start within {} s",
                    self.op_timeout.as_secs()
                ),
            ),
            StartError::Start(e) => ControlError::new(
                ErrorCode::Io,
                format!("cannot start the ublk device of volume {vol}: {e}"),
            ),
        })
    }

    /// Tears down the device of `vol` whose start failed: out of the
    /// engine's front door, its queue closed (which ends the reads a start
    /// still in flight waits for), the start reaped when it finishes within
    /// a few seconds, then deleted and forgotten. `DEL_DEV` waits for the
    /// character device, so the queue goes first; a device that cannot be
    /// deleted is recorded stale (`reason`) for `otapctl device reap`, and
    /// the log says so.
    fn abandon_device(&mut self, vol: u32, reason: StaleReason) {
        let Some(dev) = self.engine.front_door().remove_device(vol) else {
            return;
        };
        let VolumeDevice { mut device, front } = dev;
        drop(front);
        let deadline = Instant::now() + Duration::from_secs(5);
        while device.start_in_flight() && Instant::now() < deadline {
            if device.poll_start().is_err() {
                break;
            }
            std::thread::sleep(Duration::from_millis(1));
        }
        let id = device.dev_id();
        match device.delete() {
            Ok(()) => {
                self.bindings.forget(id);
            }
            Err(e) => {
                self.bindings.mark_stale(id, vol, reason);
                self.log(format!(
                    "volume {vol}: the ublk device {id} whose start failed was not deleted ({e}); \
                     `otapctl device reap {id}` deletes it"
                ));
            }
        }
        self.save_bindings();
    }

    /// Takes the ublk device of `vol` down: the queue answers `ENODEV`,
    /// the requests the engine holds finish, then the device is deleted and
    /// forgotten (nothing when the volume has none).
    fn drop_device(&mut self, vol: u32) -> Result<(), ControlError> {
        if !self.devices.contains_key(&vol) {
            return Ok(());
        }
        if let Some(dev) = self.engine.front_door().device_mut(vol) {
            dev.front.set_offline(true);
        }
        self.wait_until("the device's requests to drain", |e| {
            e.front_door()
                .device(vol)
                .is_none_or(|d| d.front.in_engine() == 0)
        })?;
        if let Some(dev) = self.engine.front_door().remove_device(vol) {
            let served = self.devices.remove(&vol);
            let (id, path) =
                served.map_or((dev.device.dev_id(), String::new()), |s| (s.dev_id, s.path));
            if let Err(e) = destroy_device(dev) {
                self.bindings
                    .mark_stale(id, vol, StaleReason::TeardownFailed);
                self.save_bindings();
                return Err(e);
            }
            self.bindings.forget(id);
            self.save_bindings();
            self.log(format!("volume {vol} left {path}"));
        }
        Ok(())
    }

    /// Answers a request against the booted pool. `Ping`, the pool-level
    /// format, boot and stop requests and the daemon's stop belong to the
    /// daemon and are refused here.
    pub fn handle(&mut self, req: &Request) -> Reply {
        match req {
            Request::PoolStatus => self.status(),
            Request::PoolSync => self.sync(),
            Request::PoolClean => self.clean(),
            Request::VolumeCreate { name, size_bytes } => self.volume_create(name, *size_bytes),
            Request::VolumeList => Ok(Response::Volumes {
                volumes: self
                    .engine
                    .volumes()
                    .iter()
                    .filter_map(|(vol, _)| self.volume_report_for(*vol))
                    .collect(),
            }),
            Request::VolumeShow { name } => {
                let vol = self.vol_index(name)?;
                self.report_volume(vol)
            }
            Request::VolumeDelete { name } => self.volume_delete(name),
            Request::VolumeOnline { name, online } => self.volume_online(name, *online),
            Request::VolumeResize { name, size_bytes } => self.volume_resize(name, *size_bytes),
            Request::VolumeRename { name, new_name } => self.volume_rename(name, new_name),
            Request::VolumeNvfailClear { name } => self.volume_nvfail_clear(name),
            Request::SnapshotCreate { volume, name } => self.snapshot_create(volume, name),
            Request::SnapshotList { volume } => {
                let vol = volume.as_deref().map(|v| self.vol_index(v)).transpose()?;
                Ok(Response::Snapshots {
                    snapshots: self.snapshot_reports(vol),
                })
            }
            Request::SnapshotDelete { volume, name } => self.snapshot_delete(volume, name),
            Request::SnapshotRollback {
                volume,
                name,
                delete_newer,
            } => self.snapshot_rollback(volume, name, *delete_newer),
            Request::SnapshotPin {
                volume,
                name,
                flags,
            } => self.snapshot_pin(volume, name, *flags),
            Request::SnapshotReplicationBase { volume, name } => {
                self.snapshot_replication_base(volume, name.as_deref())
            }
            Request::IoWrite {
                volume,
                lba,
                data_hex,
            } => self.io_write(volume, *lba, data_hex),
            Request::IoRead {
                volume,
                lba,
                blocks,
            } => self.io_read(volume, *lba, *blocks),
            Request::IoPunch {
                volume,
                lba,
                blocks,
            } => self.io_punch(volume, *lba, *blocks),
            Request::DeviceList {} => self.device_list(),
            Request::DeviceReap { dev_id, stale } => self.device_reap(*dev_id, *stale),
            Request::PoolVerify {} => self.verify_start(),
            Request::PoolVerifyStatus {} => self.verify_status(),
            Request::PoolVerifyCancel {} => self.verify_cancel(),
            Request::Ping
            | Request::PoolFormat { .. }
            | Request::PoolBoot { .. }
            | Request::PoolStop
            | Request::DaemonStop => Err(bad_request("not a request for a booted pool")),
        }
    }

    /// The pool report.
    pub fn status(&self) -> Reply {
        Ok(Response::Pool(pool_report(
            &self.engine,
            &self.device,
            self.arena_bytes,
            &self.arena_word,
            self.cp_interval_secs,
            &self.last_boot,
        )))
    }

    /// Starts a verification of the pool (Step 6B) and answers at once with
    /// its report: it runs as a job, a bounded slice per engine tick, so a
    /// verification of any size never holds the request loop beyond one
    /// slice and never meets the operation timeout; the client follows it
    /// with [`verify_status`](Self::verify_status). A verification already
    /// running is answered with its own report.
    pub fn verify_start(&mut self) -> Reply {
        let report = self
            .engine
            .start_verify(otap_cp::VerifyOptions::default())
            .map_err(engine_error)?;
        Ok(Response::Verify(verify_report(&report)))
    }

    /// The verification in progress, or the last one since the boot.
    pub fn verify_status(&self) -> Reply {
        let report = self.engine.verify_report().ok_or_else(|| {
            ControlError::new(
                ErrorCode::NotFound,
                "no verification has run since the pool booted; start one with `pool verify`",
            )
        })?;
        Ok(Response::Verify(verify_report(&report)))
    }

    /// Cancels the verification in progress.
    pub fn verify_cancel(&mut self) -> Reply {
        let report = self
            .engine
            .cancel_verify()
            .ok_or_else(|| ControlError::new(ErrorCode::NotFound, "no verification is running"))?;
        for ev in self.engine.take_events() {
            self.note(&ev);
        }
        Ok(Response::Verify(verify_report(&report)))
    }

    /// A consistency point now.
    pub fn sync(&mut self) -> Reply {
        let cp_seq = self.sync_now()?;
        Ok(Response::Committed { cp_seq })
    }

    /// A cleaning consistency point now.
    pub fn clean(&mut self) -> Reply {
        self.wait_quiet()?;
        self.engine.request_clean();
        let cp_seq = self.wait_cp_outcome("the cleaning consistency point")?;
        Ok(Response::Committed { cp_seq })
    }

    /// A clean shutdown: every device down, the final consistency point
    /// with the `CleanShutdown` marker. The pool serves nothing afterwards
    /// and is dropped by the daemon.
    pub fn stop(&mut self) -> Reply {
        self.stopping = true;
        let vols: Vec<u32> = self.devices.keys().copied().collect();
        for vol in vols {
            self.drop_device(vol)?;
        }
        self.engine.shutdown();
        let (cp_seq, clean) = self.wait_for("the shutdown consistency point", |ev| match ev {
            Event::Stopped { cp_seq, clean } => Some((*cp_seq, *clean)),
            _ => None,
        })?;
        if clean {
            Ok(Response::Committed { cp_seq })
        } else {
            Err(ControlError::new(
                ErrorCode::Engine,
                format!(
                    "the pool stopped at cp {cp_seq} without a clean shutdown record: the final \
                     consistency point found no free segment; a hugetlbfs arena keeps the \
                     unflushed writes for the next boot, which will be a dirty one"
                ),
            ))
        }
    }

    /// Creates a named volume; online once its consistency point commits.
    pub fn volume_create(&mut self, name: &str, size_bytes: u64) -> Reply {
        let blocks = blocks_for(size_bytes)?;
        let vol = self
            .engine
            .create_named_volume(name, blocks)
            .map_err(engine_error)?;
        self.wait_for("the volume's consistency point", |ev| match ev {
            Event::VolumeCreated { vol: v, .. } if *v == vol => Some(()),
            _ => None,
        })?;
        self.ensure_device(vol)?;
        self.report_volume(vol)
    }

    /// Deletes a volume (offline, no snapshots).
    pub fn volume_delete(&mut self, name: &str) -> Reply {
        let vol = self.vol_index(name)?;
        self.engine.delete_volume(vol).map_err(engine_error)?;
        self.wait_for("the deletion's consistency point", |ev| match ev {
            Event::VolumeDeleted { vol: v, .. } if *v == vol => Some(()),
            _ => None,
        })?;
        self.devices.remove(&vol);
        Ok(Response::Ok)
    }

    /// Brings a volume online (and exposes its device) or takes it offline
    /// (and takes the device down); durable with the consistency point
    /// this waits for.
    pub fn volume_online(&mut self, name: &str, online: bool) -> Reply {
        let vol = self.vol_index(name)?;
        if online {
            self.engine
                .set_volume_online(vol, true)
                .map_err(engine_error)?;
            self.sync_now()?;
            self.ensure_device(vol)?;
        } else {
            self.engine
                .set_volume_online(vol, false)
                .map_err(engine_error)?;
            self.drop_device(vol)?;
            self.sync_now()?;
        }
        self.report_volume(vol)
    }

    /// Resizes a volume; a shrink needs it offline. A device the volume
    /// exposes is recreated at the new size.
    pub fn volume_resize(&mut self, name: &str, size_bytes: u64) -> Reply {
        let vol = self.vol_index(name)?;
        let blocks = blocks_for(size_bytes)?;
        let current = self.engine.volume(vol).map_or(0, |v| v.size_blocks);
        if blocks == current {
            return self.report_volume(vol);
        }
        self.engine
            .resize_volume(vol, blocks)
            .map_err(engine_error)?;
        self.wait_for("the resize's consistency point", |ev| match ev {
            Event::VolumeResized { vol: v, .. } if *v == vol => Some(()),
            _ => None,
        })?;
        if self.devices.contains_key(&vol) {
            self.drop_device(vol)?;
            self.ensure_device(vol)?;
        }
        self.report_volume(vol)
    }

    /// Renames a volume in the catalogue.
    pub fn volume_rename(&mut self, name: &str, new_name: &str) -> Reply {
        let vol = self.vol_index(name)?;
        self.engine
            .set_volume_name(vol, new_name)
            .map_err(engine_error)?;
        self.sync_now()?;
        self.report_volume(vol)
    }

    /// Clears nvfail on a volume; returns when the clear is durable.
    pub fn volume_nvfail_clear(&mut self, name: &str) -> Reply {
        let vol = self.vol_index(name)?;
        let held = self.engine.volume(vol).is_some_and(|v| v.nvfail.is_some());
        if !held {
            return self.report_volume(vol);
        }
        self.engine.nvfail_clear(vol).map_err(engine_error)?;
        self.wait_for("the clear's consistency point", |ev| match ev {
            Event::NvfailCleared { vol: v, .. } if *v == vol => Some(()),
            _ => None,
        })?;
        self.report_volume(vol)
    }

    /// Takes a named snapshot (crash-consistent, user origin); returns when
    /// it is durable.
    pub fn snapshot_create(&mut self, volume: &str, name: &str) -> Reply {
        let vol = self.vol_index(volume)?;
        let uuid = self
            .engine
            .create_named_snapshot(vol, name, SnapKind::Crash, SnapOrigin::User)
            .map_err(engine_error)?;
        loop {
            let outcome = self.wait_for("the snapshot's consistency point", |ev| match ev {
                Event::SnapshotCreated { vol: v, .. } if *v == vol => Some(Ok(())),
                Event::SnapshotFailed {
                    uuid: u, reason, ..
                } if *u == uuid => Some(Err(reason.clone())),
                _ => None,
            })?;
            if let Err(reason) = outcome {
                return Err(ControlError::new(
                    ErrorCode::Engine,
                    format!("the snapshot could not be taken: {reason}"),
                ));
            }
            // The event names the volume, not the snapshot: one of an
            // earlier request that landed late is not ours, and the wait
            // goes on until ours is listed.
            if let Some(snap_id) = self
                .engine
                .snapshots()
                .iter()
                .find(|s| s.uuid == uuid)
                .map(|s| s.snap_id)
            {
                return self.report_snapshot(snap_id);
            }
        }
    }

    /// Deletes a snapshot; returns when the deletion is durable.
    pub fn snapshot_delete(&mut self, volume: &str, name: &str) -> Reply {
        let vol = self.vol_index(volume)?;
        let snap_id = self.snap_index(vol, name)?;
        self.delete_snapshot_by_id(snap_id)?;
        Ok(Response::Ok)
    }

    fn delete_snapshot_by_id(&mut self, snap_id: u64) -> Result<(), ControlError> {
        self.engine.delete_snapshot(snap_id).map_err(engine_error)?;
        self.wait_for("the deletion's consistency point", |ev| match ev {
            Event::SnapshotDeleted { snap_id: id, .. } if *id == snap_id => Some(()),
            _ => None,
        })
    }

    /// Rolls a volume back to a snapshot: newer snapshots deleted first
    /// when asked (every one checked before any is deleted, so a refusal
    /// leaves the history whole), the volume taken offline and its writes
    /// flushed, the rollback committed, the volume brought back (if it
    /// was online).
    pub fn snapshot_rollback(&mut self, volume: &str, name: &str, delete_newer: bool) -> Reply {
        let vol = self.vol_index(volume)?;
        let snap_id = self.snap_index(vol, name)?;
        let snapshots = self.engine.snapshots();
        let target = snapshots
            .iter()
            .find(|s| s.snap_id == snap_id)
            .ok_or_else(|| {
                ControlError::new(ErrorCode::NotFound, format!("snapshot {snap_id} is gone"))
            })?;
        if target.damaged {
            return Err(ControlError::new(
                ErrorCode::Engine,
                format!("snapshot {name:?} is damaged and cannot be rolled back to"),
            ));
        }
        let target_cp = target.cp_seq;
        let newer: Vec<&otap_cp::SnapshotInfo> = snapshots
            .iter()
            .filter(|s| s.vol == vol && s.cp_seq > target_cp && !s.deleting)
            .collect();
        if !newer.is_empty() {
            let names: Vec<String> = newer
                .iter()
                .map(|s| s.name.clone().unwrap_or_else(|| format!("#{}", s.snap_id)))
                .collect();
            if !delete_newer {
                return Err(ControlError::new(
                    ErrorCode::Engine,
                    format!(
                        "volume {volume:?} has {} newer snapshot(s) ({}); pass --delete-newer \
                         to delete them first",
                        newer.len(),
                        names.join(", ")
                    ),
                ));
            }
            if let Some(pinned) = newer.iter().find(|s| s.flags != 0) {
                return Err(ControlError::new(
                    ErrorCode::Engine,
                    format!(
                        "snapshot {:?} is pinned (flags {:#04x}); unpin it or move the \
                         replication base before rolling back; nothing was deleted",
                        pinned.name.as_deref().unwrap_or("?"),
                        pinned.flags
                    ),
                ));
            }
        }
        let ids: Vec<u64> = newer.iter().map(|s| s.snap_id).collect();
        drop(snapshots);
        for id in &ids {
            self.engine.delete_snapshot(*id).map_err(engine_error)?;
        }
        if !ids.is_empty() {
            let mut pending: HashSet<u64> = ids.iter().copied().collect();
            self.wait_for("the deletions' consistency point", |ev| match ev {
                Event::SnapshotDeleted { snap_id, .. } => {
                    pending.remove(snap_id);
                    pending.is_empty().then_some(())
                }
                _ => None,
            })?;
        }
        let was_online = self.engine.volume(vol).is_some_and(|v| !v.offline);
        if was_online {
            self.engine
                .set_volume_online(vol, false)
                .map_err(engine_error)?;
            self.drop_device(vol)?;
        }
        // The CP carries every acknowledged write of the volume; quiet
        // means no CP in flight touches it.
        self.sync_now()?;
        self.wait_quiet()?;
        if let Err(e) = self.engine.rollback_volume(vol, snap_id) {
            if was_online {
                let _ = self.engine.set_volume_online(vol, true);
                let _ = self.sync_now();
                let _ = self.ensure_device(vol);
            }
            return Err(engine_error(e));
        }
        self.wait_for("the rollback's consistency point", |ev| match ev {
            Event::VolumeRolledBack { vol: v, .. } if *v == vol => Some(()),
            _ => None,
        })?;
        if was_online {
            self.engine
                .set_volume_online(vol, true)
                .map_err(engine_error)?;
            self.sync_now()?;
            self.ensure_device(vol)?;
        }
        self.report_volume(vol)
    }

    /// Sets a snapshot's operator pins (`PIN_USER`, `PIN_SCHEDULE`,
    /// `PIN_CLONE`); the replication pin is kept as it is.
    pub fn snapshot_pin(&mut self, volume: &str, name: &str, flags: u8) -> Reply {
        if flags & !PIN_MASK != 0 {
            return Err(bad_request(format!(
                "flags {flags:#04x} carry bits outside the PIN_* set"
            )));
        }
        if flags & PIN_REPLICATION != 0 {
            return Err(bad_request(
                "the replication pin is set with the replication base, not by hand",
            ));
        }
        let vol = self.vol_index(volume)?;
        let snap_id = self.snap_index(vol, name)?;
        let current = self
            .engine
            .snapshots()
            .iter()
            .find(|s| s.snap_id == snap_id)
            .map_or(0, |s| s.flags);
        let wanted = (current & PIN_REPLICATION) | flags;
        self.engine
            .set_snapshot_pins(snap_id, wanted)
            .map_err(engine_error)?;
        self.wait_for("the pin's consistency point", |ev| match ev {
            Event::SnapshotPinned { snap_id: id, .. } if *id == snap_id => Some(()),
            _ => None,
        })?;
        self.report_snapshot(snap_id)
    }

    /// Makes a snapshot the volume's replication base, or releases it.
    pub fn snapshot_replication_base(&mut self, volume: &str, name: Option<&str>) -> Reply {
        let vol = self.vol_index(volume)?;
        let snap_id = name.map(|n| self.snap_index(vol, n)).transpose()?;
        self.engine
            .set_replication_base(vol, snap_id)
            .map_err(engine_error)?;
        self.sync_now()?;
        match snap_id {
            Some(id) => self.report_snapshot(id),
            None => Ok(Response::Ok),
        }
    }

    /// Submits a request through the channel front door and ticks until
    /// the engine answers.
    fn io(
        &mut self,
        kind: RequestKind,
        data: Option<Vec<u8>>,
    ) -> Result<IoCompletion, ControlError> {
        let rx = self.io.submit(kind, data);
        let deadline = Instant::now() + self.op_timeout;
        loop {
            match rx.try_recv() {
                Ok(c) => return Ok(c),
                Err(TryRecvError::Disconnected) => {
                    return Err(ControlError::new(
                        ErrorCode::Engine,
                        "the request was dropped by the engine",
                    ))
                }
                Err(TryRecvError::Empty) => {}
            }
            if self.engine.is_stopped() {
                return Err(ControlError::new(
                    ErrorCode::Engine,
                    "the engine has stopped",
                ));
            }
            if Instant::now() >= deadline {
                return Err(ControlError::new(
                    ErrorCode::Timeout,
                    format!(
                        "the request was not answered within {} s",
                        self.op_timeout.as_secs()
                    ),
                ));
            }
            if self.interrupted() {
                return Err(Self::interrupted_error("the request"));
            }
            let busy = self.tick()?;
            if !busy {
                std::thread::sleep(IDLE_TICK);
            }
        }
    }

    fn io_report(c: &IoCompletion) -> Response {
        Response::Io(IoResultReport {
            status: status_word(c.status).to_string(),
            seq: c.seq,
            data_hex: c.data.as_deref().map(encode_hex).unwrap_or_default(),
        })
    }

    fn check_blocks(blocks: u32) -> Result<(), ControlError> {
        if blocks == 0 || blocks > MAX_IO_BLOCKS {
            return Err(bad_request(format!(
                "{blocks} blocks: a request moves 1 to {MAX_IO_BLOCKS} blocks"
            )));
        }
        Ok(())
    }

    /// Writes whole blocks given as hexadecimal.
    pub fn io_write(&mut self, volume: &str, lba: u64, data_hex: &str) -> Reply {
        let vol = self.vol_index(volume)?;
        let data =
            decode_hex(data_hex).ok_or_else(|| bad_request("data_hex is not hexadecimal"))?;
        let blocks = blocks_of(data.len())
            .ok_or_else(|| bad_request("the data must be whole 4 KiB blocks, at least one"))?;
        Self::check_blocks(blocks)?;
        let c = self.io(RequestKind::Write { vol, lba, blocks }, Some(data))?;
        Ok(Self::io_report(&c))
    }

    /// Reads blocks.
    pub fn io_read(&mut self, volume: &str, lba: u64, blocks: u32) -> Reply {
        let vol = self.vol_index(volume)?;
        Self::check_blocks(blocks)?;
        let c = self.io(RequestKind::Read { vol, lba, blocks }, None)?;
        Ok(Self::io_report(&c))
    }

    /// Discards blocks.
    pub fn io_punch(&mut self, volume: &str, lba: u64, blocks: u32) -> Reply {
        let vol = self.vol_index(volume)?;
        Self::check_blocks(blocks)?;
        let c = self.io(RequestKind::Discard { vol, lba, blocks }, None)?;
        Ok(Self::io_report(&c))
    }
}
