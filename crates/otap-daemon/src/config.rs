//! What the daemon is started with.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use otap_control::{ArenaKind, Endpoint, MediaKind, DEFAULT_ARENA_BYTES, DEFAULT_CP_INTERVAL_SECS};
use otap_cp::{DiscardPolicy, FlushPolicy, Media};
use otap_ublk::{UblkHost, DEFAULT_RECOVERY_BUDGET_NS};

/// The longest `--recovery-timeout` the daemon takes, in seconds.
pub const MAX_RECOVERY_TIMEOUT_SECS: u64 = 300;

/// A function that starts a ublk control plane, called once per daemon.
pub type HostFactory = Arc<dyn Fn() -> Box<dyn UblkHost> + Send + Sync>;

/// Whose ublk control plane the daemon drives.
#[derive(Clone, Default)]
pub enum UblkBackend {
    /// The kernel's ublk driver over `/dev/ublk-control` on Linux; none
    /// elsewhere, where no volume gets a device.
    #[default]
    Kernel,
    /// A control plane the caller supplies: the tests' in-memory driver
    /// (`otap_ublk::MockKernel`), so the device lifecycle, the restart
    /// reconciliation and the reaper run on any host.
    Custom(HostFactory),
}

impl UblkBackend {
    /// A new control plane for one daemon, or `None` when this host has
    /// none.
    #[must_use]
    pub fn start(&self) -> Option<Box<dyn UblkHost>> {
        match self {
            Self::Custom(make) => Some(make()),
            Self::Kernel => {
                #[cfg(target_os = "linux")]
                {
                    Some(Box::new(otap_ublk::device::LinuxHost::new()))
                }
                #[cfg(not(target_os = "linux"))]
                {
                    None
                }
            }
        }
    }
}

impl std::fmt::Debug for UblkBackend {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Kernel => f.write_str("Kernel"),
            Self::Custom(_) => f.write_str("Custom"),
        }
    }
}

impl PartialEq for UblkBackend {
    fn eq(&self, other: &Self) -> bool {
        match (self, other) {
            (Self::Kernel, Self::Kernel) => true,
            (Self::Custom(a), Self::Custom(b)) => Arc::ptr_eq(a, b),
            _ => false,
        }
    }
}

impl Eq for UblkBackend {}

/// Where the daemon keeps its host-local runtime state when none is given:
/// `/run/opentap` on Linux (a tmpfs, emptied at reboot like the ublk
/// devices the state describes), the temporary directory's `opentap`
/// elsewhere.
#[must_use]
pub fn default_state_dir() -> PathBuf {
    if cfg!(target_os = "linux") {
        PathBuf::from("/run/opentap")
    } else {
        std::env::temp_dir().join("opentap")
    }
}

/// Geometry and policy overrides an operator may give at format and at
/// boot (a boot must give the same geometry the pool was formatted with,
/// or none: the daemon reads the recorded geometry).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Overrides {
    /// The media, when the device cannot say (a file).
    pub media: Option<MediaKind>,
    /// The stripe, in bytes.
    pub stripe_bytes: Option<u32>,
    /// The segment, in bytes.
    pub segment_bytes: Option<u64>,
    /// Treat the device as flush-opaque.
    pub flush_opaque: Option<bool>,
    /// Refuse a flush-opaque device.
    pub strict_flush: bool,
}

impl Overrides {
    /// The engine's flush policy.
    #[must_use]
    pub const fn flush_policy(&self) -> FlushPolicy {
        if self.strict_flush {
            FlushPolicy::RequireFuaOrFlush
        } else {
            FlushPolicy::TrustBarrierWithNvfail
        }
    }

    /// The media as the engine names it, `None` when not given.
    #[must_use]
    pub const fn media(&self) -> Option<Media> {
        match self.media {
            Some(MediaKind::Hdd) => Some(Media::Hdd),
            Some(MediaKind::Flash) => Some(Media::Flash),
            None => None,
        }
    }

    /// The geometry planner's overrides.
    #[must_use]
    pub fn geom_overrides(&self) -> otap_geom::Overrides {
        otap_geom::Overrides {
            stripe_bytes: self.stripe_bytes,
            segment_bytes: self.segment_bytes,
            flush_opaque: self.flush_opaque,
            flush_policy: Some(self.flush_policy()),
            discard: None::<DiscardPolicy>,
            media: self.media(),
            allow_misaligned: false,
        }
    }
}

/// The daemon's configuration.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DaemonConfig {
    /// Where to listen.
    pub control: Endpoint,
    /// A pool to boot at start, if any.
    pub device: Option<PathBuf>,
    /// Arena bytes for the pool booted at start.
    pub arena_bytes: u64,
    /// Where that arena lives.
    pub arena: ArenaKind,
    /// The CP timer, 1 to 15 seconds.
    pub cp_interval_secs: u32,
    /// Geometry and policy overrides for the pool booted at start.
    pub overrides: Overrides,
    /// Expose a ublk device per online volume (Linux, or any host with a
    /// [`UblkBackend::Custom`] control plane; ignored otherwise).
    pub ublk: bool,
    /// The ublk control plane.
    pub ublk_backend: UblkBackend,
    /// Where the daemon keeps its host-local state: the binding of each
    /// volume to its ublk device (`crate::bindings`), which lets a daemon
    /// restarted after `kill -9` re-attach the same devices.
    pub state_dir: PathBuf,
    /// How long a restarted daemon waits for the driver to quiesce a dead
    /// daemon's device before it gives up on re-attaching it, creates a new
    /// one for the volume and lists the old one as stale.
    pub recovery_timeout: Duration,
    /// Listen on a TCP address beyond loopback. The control plane is
    /// unauthenticated, so this is off unless the operator says so.
    pub allow_remote: bool,
    /// How long a control request may wait for its consistency point.
    pub op_timeout: Duration,
    /// For the pool booted at start only: recreate its arena file when it
    /// was made for another size, even if it may hold acknowledged writes
    /// (they are destroyed and their volumes held under nvfail). A
    /// `PoolBoot` request never discards.
    pub discard_arena: bool,
}

impl Default for DaemonConfig {
    fn default() -> Self {
        Self {
            control: Endpoint::default_for_host(),
            device: None,
            arena_bytes: DEFAULT_ARENA_BYTES,
            arena: ArenaKind::default_for_host(),
            cp_interval_secs: DEFAULT_CP_INTERVAL_SECS,
            overrides: Overrides::default(),
            ublk: cfg!(target_os = "linux"),
            ublk_backend: UblkBackend::Kernel,
            state_dir: default_state_dir(),
            recovery_timeout: Duration::from_nanos(DEFAULT_RECOVERY_BUDGET_NS),
            allow_remote: false,
            op_timeout: Duration::from_secs(120),
            discard_arena: false,
        }
    }
}
