//! What the daemon is started with.

use std::path::PathBuf;
use std::time::Duration;

use otap_control::{ArenaKind, Endpoint, MediaKind, DEFAULT_ARENA_BYTES, DEFAULT_CP_INTERVAL_SECS};
use otap_cp::{DiscardPolicy, FlushPolicy, Media};

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
    /// Expose a ublk device per online volume (Linux; ignored elsewhere).
    pub ublk: bool,
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
            allow_remote: false,
            op_timeout: Duration::from_secs(120),
            discard_arena: false,
        }
    }
}
