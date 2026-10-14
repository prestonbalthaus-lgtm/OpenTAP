//! Credits: the receiver paces the sender by the staging space it has free
//! (section 7.3, "Cursor, credits, integrity").
//!
//! A credit is 4 KiB of receiver staging. The receiver grants an initial
//! window (64 MiB, capped at its staging size), then grants more in `Landed`
//! frames as its segment writer drains staged stripes to disk, so the sender
//! is bounded by the landing rate and never by TCP alone. A sender reserves
//! credits before it sends a chunk and must stop when the window is empty
//! ([`StreamError::Window`]). No window ever holds more than
//! [`MAX_CREDITS`], the largest staging a receiver may offer.

use otap_format::BLOCK_SIZE;

use crate::StreamError;

/// Bytes per credit.
pub const CREDIT_BYTES: u32 = BLOCK_SIZE as u32;
/// The initial grant: 64 MiB.
pub const INITIAL_GRANT_BYTES: u64 = 64 * 1024 * 1024;
/// The largest staging a receiver may offer: 2 GiB.
pub const MAX_STAGING_BYTES: u64 = 2 * 1024 * 1024 * 1024;
/// The most credits any window holds: [`MAX_STAGING_BYTES`] in credits.
pub const MAX_CREDITS: u32 = (MAX_STAGING_BYTES / CREDIT_BYTES as u64) as u32;

/// Credits needed to carry `bytes`, rounded up; saturates at `u32::MAX`,
/// which no window can hold, so an absurd length is always refused.
#[must_use]
pub const fn credits_for(bytes: usize) -> u32 {
    let credits = bytes.div_ceil(CREDIT_BYTES as usize);
    if credits > u32::MAX as usize {
        u32::MAX
    } else {
        credits as u32
    }
}

/// The receiver's view: how much staging exists, how much the sender may
/// still fill, and how much is holding landed data that has not been
/// drained yet.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ReceiverWindow {
    capacity: u32,
    outstanding: u32,
    staged: u32,
}

impl ReceiverWindow {
    /// A window over `staging_bytes` of staging (a multiple of 4 KiB, at
    /// most [`MAX_STAGING_BYTES`]).
    pub fn new(staging_bytes: u64) -> Result<Self, StreamError> {
        if staging_bytes == 0
            || !staging_bytes.is_multiple_of(u64::from(CREDIT_BYTES))
            || staging_bytes > MAX_STAGING_BYTES
        {
            return Err(StreamError::BadBody {
                field: "staging_bytes",
                value: staging_bytes,
            });
        }
        Ok(Self {
            capacity: (staging_bytes / u64::from(CREDIT_BYTES)) as u32,
            outstanding: 0,
            staged: 0,
        })
    }

    /// Staging in credits.
    #[must_use]
    pub const fn capacity(&self) -> u32 {
        self.capacity
    }

    /// Credits the sender holds and has not used.
    #[must_use]
    pub const fn outstanding(&self) -> u32 {
        self.outstanding
    }

    /// Credits holding landed, undrained data.
    #[must_use]
    pub const fn staged(&self) -> u32 {
        self.staged
    }

    /// Credits neither granted nor staged: what the next grant can offer.
    #[must_use]
    pub const fn free(&self) -> u32 {
        self.capacity - self.outstanding - self.staged
    }

    /// The opening grant, sent in `HelloAck` or the first `Landed`: the
    /// smaller of 64 MiB and the free staging. Returns the credits granted.
    pub fn initial_grant(&mut self) -> u32 {
        let want = credits_for(INITIAL_GRANT_BYTES as usize);
        self.grant_up_to(want)
    }

    /// Grants up to `want` credits, bounded by the free staging. Returns the
    /// credits granted (0 when nothing is free).
    pub fn grant_up_to(&mut self, want: u32) -> u32 {
        let granted = want.min(self.free());
        self.outstanding += granted;
        granted
    }

    /// A chunk of `bytes` landed into staging: its credits move from
    /// outstanding to staged. A sender that exceeded its window is refused
    /// and nothing changes.
    pub fn landed(&mut self, bytes: usize) -> Result<(), StreamError> {
        let credits = credits_for(bytes);
        if credits > self.outstanding {
            return Err(StreamError::Window {
                needed: credits,
                available: self.outstanding,
            });
        }
        self.outstanding -= credits;
        self.staged += credits;
        Ok(())
    }

    /// `bytes` of staged data were written to disk: their credits are free
    /// again and can be granted. More than is staged is refused and nothing
    /// changes.
    pub fn drained(&mut self, bytes: usize) -> Result<(), StreamError> {
        let credits = credits_for(bytes);
        if credits > self.staged {
            return Err(StreamError::Window {
                needed: credits,
                available: self.staged,
            });
        }
        self.staged -= credits;
        Ok(())
    }

    /// A delta was aborted: whatever was staged for it is discarded and the
    /// sender's outstanding credits are withdrawn (the next `HelloAck` or
    /// `Landed` grants anew).
    pub fn reset(&mut self) {
        self.outstanding = 0;
        self.staged = 0;
    }
}

/// The sender's view: credits it may spend, never more than [`MAX_CREDITS`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct SenderWindow {
    credits: u32,
}

impl SenderWindow {
    /// A window with `credits` granted (capped at [`MAX_CREDITS`]).
    #[must_use]
    pub const fn new(credits: u32) -> Self {
        Self {
            credits: if credits > MAX_CREDITS {
                MAX_CREDITS
            } else {
                credits
            },
        }
    }

    /// Credits available.
    #[must_use]
    pub const fn available(&self) -> u32 {
        self.credits
    }

    /// `true` when a chunk of `bytes` may be sent now.
    #[must_use]
    pub const fn can_send(&self, bytes: usize) -> bool {
        credits_for(bytes) <= self.credits
    }

    /// Spends the credits for a chunk of `bytes`, or refuses with
    /// [`StreamError::Window`] and spends nothing.
    pub fn reserve(&mut self, bytes: usize) -> Result<u32, StreamError> {
        let credits = credits_for(bytes);
        if credits > self.credits {
            return Err(StreamError::Window {
                needed: credits,
                available: self.credits,
            });
        }
        self.credits -= credits;
        Ok(credits)
    }

    /// Adds credits from a `Landed` or `HelloAck` grant, capped at
    /// [`MAX_CREDITS`].
    pub fn grant(&mut self, credits: u32) {
        self.credits = self.credits.saturating_add(credits).min(MAX_CREDITS);
    }

    /// Forgets every credit (a reconnect starts from the receiver's cursor).
    pub fn reset(&mut self) {
        self.credits = 0;
    }
}

const _: () = {
    assert!(MAX_CREDITS == 524_288);
    assert!(credits_for(0) == 0);
    assert!(credits_for(1) == 1);
    assert!(credits_for(4096) == 1);
    assert!(credits_for(4097) == 2);
    assert!(credits_for(usize::MAX) == u32::MAX);
};
