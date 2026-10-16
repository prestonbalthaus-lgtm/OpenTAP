//! The shape of a device: what `ADD_DEV` and `SET_PARAMS` are given. Pure
//! data, the same on every host, so the mock control plane
//! ([`crate::mock_kernel`]) and the Linux one (`device`) create the same
//! devices.

use otap_format::BLOCK_SIZE;

/// How the device is shaped.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DeviceSpec {
    /// The volume's size in 4 KiB blocks.
    pub size_blocks: u64,
    /// Tags per queue (one queue), at most 4096.
    pub depth: u16,
    /// The largest request in bytes (8 MiB, the engine's request bound,
    /// or less when the arena is small: [`DeviceSpec::for_arena`]).
    pub max_io_bytes: u32,
    /// What `optimal_io_size` is derived from: the full stripe. The driver
    /// takes a power of two (a shift), so the device reports the largest
    /// power of two dividing the stripe (the stripe itself when it is one).
    pub io_opt_bytes: u32,
    /// Report discard support, with this bound per request.
    pub max_discard_bytes: Option<u32>,
    /// Report rotational media.
    pub rotational: bool,
}

impl DeviceSpec {
    /// A device for `size_blocks` blocks over a `stripe_bytes` geometry:
    /// 128 tags, 8 MiB requests, discards up to 1 GiB.
    #[must_use]
    pub const fn new(size_blocks: u64, stripe_bytes: u32, rotational: bool) -> Self {
        Self {
            size_blocks,
            depth: 128,
            max_io_bytes: 8 << 20,
            io_opt_bytes: stripe_bytes,
            max_discard_bytes: Some(1 << 30),
            rotational,
        }
    }

    /// Bounds the largest request by the arena: the engine refuses a write
    /// longer than the arena's slots (`TooLarge`, `EINVAL` on the device),
    /// so a device over a small arena must not advertise 8 MiB requests.
    /// Half the arena, rounded down to whole 4 KiB blocks, at least one.
    #[must_use]
    pub fn for_arena(mut self, arena_slots: u32) -> Self {
        let cap = (arena_slots / 2).max(1).saturating_mul(BLOCK_SIZE as u32);
        if cap < self.max_io_bytes {
            self.max_io_bytes = cap;
        }
        self
    }
}
