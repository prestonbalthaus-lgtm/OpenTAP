//! `otap-arena`: the write-cache arena (round 3, sections 1.3 and 7.2; Step 3
//! brief, "Opus-Storage-Arena").
//!
//! The arena is the only copy of an acknowledged write until the consistency
//! point that carries it commits. It is a hugetlbfs file in production (8 GiB
//! of boot-reserved 2 MiB pages, created by a systemd unit and re-attached by
//! a restarted daemon, with no holder process) and an aligned heap buffer in
//! the simulator; both come from [`otap_sys::HugeArena`], so this crate does
//! no I/O and contains no `unsafe`.
//!
//! # Layout
//!
//! | Region | Bytes | Holds |
//! |---|---|---|
//! | header | 4,096 | identity and geometry ([`ArenaHeader`]), sealed once at creation; the attach counter lives outside the seal |
//! | descriptors | 32 per slot, padded to 4 KiB | one [`SlotDesc`] per slot: address, sequence, block CRC, and a sealed word with the state and the lap |
//! | slots | 4,096 per slot | the data, one 4 KiB block per slot |
//!
//! # Slots, runs and laps
//!
//! A write of `n` blocks takes `n` consecutive slots, a *run*, from a bump
//! allocator that walks the slot ring in order. A run never wraps: when the
//! ring's end is too close, the allocator skips to the next lap. A [`RingRef`]
//! names a run by its first slot and the *lap* the ring was on, so a reference
//! held by the dirty index can be checked against the slot's descriptor: a
//! slot that has been handed out again since carries a higher lap and never
//! matches ([`Arena::run`] refuses it with [`ArenaError::Stale`]). The lap
//! counter has 30 bits: an 8 GiB arena wraps it after 8 EiB of writes.
//!
//! # Slot states, and why there is no collector
//!
//! A slot moves `Free -> Filled -> Placed -> Committed`, and a committed slot
//! is free for the allocator. `Filled` means the data and its descriptor are
//! in the arena; `Placed` means the write is in the dirty index and may be
//! acknowledged; `Committed` means the consistency point that carried it is
//! durable (or that nothing live was ever there: a skipped, discarded or
//! released slot). A reserved slot stays `Free`, on the current lap, until it
//! is filled or [discarded](Arena::discard). Because runs are handed out in
//! ring order and consistency points retire generations in order, the slots
//! still holding unflushed data form one window `[tail, head)` of the ring:
//! the allocator refuses a run that would wrap into it ([`ArenaError::Full`]),
//! and the tail advances over committed slots and over slots of older laps by
//! itself. Nothing is ever scanned for garbage.
//!
//! # Every transition is one store
//!
//! The daemon may die at any instruction, so a descriptor can never be half
//! written. Its address, sequence and block CRC are stored first (they only
//! change while the slot is `Free`), then one 8-byte aligned store writes the
//! *word*: the state, the lap and a CRC-32C over the descriptor including
//! that state and lap. A kill between the two leaves a `Free` slot whose word
//! does not verify, which is what a never-used slot looks like too; a `Placed`
//! or `Committed` slot whose word does not verify was damaged by something
//! other than this code, and [`Arena::attach`] reports it instead of trusting
//! it (invariant 3 of section 7.7: every byte under a CRC).
//!
//! # Re-attach after `kill -9`
//!
//! [`Arena::attach`] verifies the header, then keeps every `Placed` slot whose
//! descriptor and data verify (its write was in the index, so it may have been
//! acknowledged), drops every `Filled` slot (its write was never acknowledged,
//! because the acknowledgement follows `Placed`), reports every damaged slot,
//! recomputes the window and the next sequence number (the maximum over every
//! descriptor, so a number handed out before the kill is never handed out
//! again), and lists the kept slots in sequence order so the engine can
//! rebuild its active generation. The header is never rewritten after
//! creation; the attach counter is a self-checking word beside it, so no
//! restart can turn a live arena into an unreadable one. A power loss empties
//! the arena: the header is gone, [`Arena::is_blank`] says so, and the boot is
//! a dirty one (invariant 1 of section 7.7).
//!
//! The arena is single-writer: every method takes `&mut self` or `&self`.
//! Concurrent reactors in `dbladed` partition the slot ring between them
//! (E16 in `docs/07-engineering-decisions.md`).

#![forbid(unsafe_code)]
#![deny(missing_docs)]
#![warn(rust_2018_idioms, unreachable_pub)]

use core::fmt;
use core::mem::{align_of, offset_of, size_of};
use core::sync::atomic::{compiler_fence, Ordering};

use otap_format::checksum::crc32c;
use otap_format::{
    FromBytes, FromZeros, Immutable, IntoBytes, KnownLayout, Uuid, BLOCK_SIZE, MAX_LBA, MAX_VOL,
};
use otap_sys::HugeArena;

/// The arena header magic: `OTAR`.
pub const ARENA_MAGIC: u32 = u32::from_le_bytes(*b"OTAR");
/// The arena layout version, `major << 8 | minor`. Minor 1: sealed
/// descriptor words, the attach counter outside the header seal.
pub const ARENA_VERSION: u16 = 0x0101;
/// Bytes per slot: one 4 KiB block.
pub const SLOT_BYTES: usize = BLOCK_SIZE;
/// Bytes per slot descriptor.
pub const SLOT_DESC_SIZE: usize = 32;
/// Byte offset of the sealed word inside a descriptor.
pub const SLOT_WORD_OFFSET: usize = 24;
/// Bytes of the header region.
pub const ARENA_HEADER_SIZE: usize = BLOCK_SIZE;
/// Header bytes under the header CRC: everything before the CRC field.
pub const ARENA_HEADER_SEALED: usize = 56;
/// Byte offset of the attach counter word in the header.
pub const ARENA_ATTACH_WORD_OFFSET: usize = 64;
/// The production arena: 8 GiB of 2 MiB pages (section 6.4).
pub const DEFAULT_ARENA_BYTES: u64 = 8 * 1024 * 1024 * 1024;
/// Reserved bytes in the header.
pub const ARENA_HEADER_RESERVED: usize = ARENA_HEADER_SIZE - 72;
/// The highest lap a slot can record (30 bits).
pub const MAX_LAP: u32 = (1 << 30) - 1;

/// The slot states.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[repr(u8)]
pub enum SlotState {
    /// Never used, reserved, or reused since: no live data.
    Free = 0,
    /// Data and descriptor written; the write is not yet in the index.
    Filled = 1,
    /// In the dirty index; the write may have been acknowledged.
    Placed = 2,
    /// Carried by a committed consistency point, or never live; reusable.
    Committed = 3,
}

impl SlotState {
    /// Decodes a two-bit state.
    #[must_use]
    pub const fn from_bits(value: u32) -> Self {
        match value & 3 {
            0 => Self::Free,
            1 => Self::Filled,
            2 => Self::Placed,
            _ => Self::Committed,
        }
    }

    /// The state bits.
    #[must_use]
    pub const fn as_u8(self) -> u8 {
        self as u8
    }

    /// `true` when the allocator may hand the slot out.
    #[must_use]
    pub const fn is_reusable(self) -> bool {
        matches!(self, Self::Free | Self::Committed)
    }
}

/// The arena header: identity and geometry, sealed once at creation and
/// never rewritten. The attach counter beside the seal is a self-checking
/// word (`count | !count << 32`) updated by one store.
#[derive(FromBytes, IntoBytes, Immutable, KnownLayout, Clone, Copy, PartialEq, Eq, Debug)]
#[repr(C, align(4096))]
pub struct ArenaHeader {
    /// [`ARENA_MAGIC`].
    pub magic: u32,
    /// [`ARENA_VERSION`].
    pub ver: u16,
    /// Reserved, zero.
    pub rsvd0: u16,
    /// The aggregate the arena belongs to.
    pub agg_id: Uuid,
    /// Slots in the ring.
    pub slots: u32,
    /// Bytes per slot ([`SLOT_BYTES`]).
    pub slot_bytes: u32,
    /// Byte offset of the descriptor table.
    pub desc_off: u64,
    /// Byte offset of slot 0.
    pub data_off: u64,
    /// Wall-clock time of creation, nanoseconds since the Unix epoch.
    pub created_wall_ns: u64,
    /// CRC-32C over bytes `0..56`.
    pub crc32c: u32,
    /// Reserved, zero.
    pub rsvd1: u32,
    /// Attaches so far, including the creating one, as `count | !count << 32`;
    /// outside the seal, so updating it can never damage the identity.
    pub attach_word: u64,
    /// Reserved, zero.
    pub reserved: [u8; ARENA_HEADER_RESERVED],
}

impl ArenaHeader {
    /// The CRC-32C over the sealed identity.
    #[must_use]
    pub fn compute_crc(&self) -> u32 {
        crc32c(&self.as_bytes()[..ARENA_HEADER_SEALED])
    }

    /// Stores the CRC.
    pub fn seal(&mut self) {
        self.crc32c = self.compute_crc();
    }

    /// Checks magic, major version, CRC and the geometry against `len`.
    pub fn verify(&self, len: usize) -> Result<(), ArenaError> {
        if self.magic != ARENA_MAGIC {
            return Err(ArenaError::BadMagic { found: self.magic });
        }
        if self.ver >> 8 != ARENA_VERSION >> 8 {
            return Err(ArenaError::BadVersion { found: self.ver });
        }
        if self.compute_crc() != self.crc32c {
            return Err(ArenaError::BadCrc);
        }
        let geometry = Geometry::for_len(len)?;
        if self.slots != geometry.slots
            || self.slot_bytes as usize != SLOT_BYTES
            || self.desc_off as usize != geometry.desc_off
            || self.data_off as usize != geometry.data_off
        {
            return Err(ArenaError::BadGeometry);
        }
        Ok(())
    }

    /// Encodes an attach count as the self-checking word.
    #[must_use]
    pub const fn encode_attach_count(count: u32) -> u64 {
        count as u64 | ((!count) as u64) << 32
    }

    /// The attach count, if the word is consistent (a torn update reads as
    /// unknown).
    #[must_use]
    pub const fn attach_count(&self) -> Option<u32> {
        let lo = self.attach_word as u32;
        let hi = (self.attach_word >> 32) as u32;
        if hi == !lo {
            Some(lo)
        } else {
            None
        }
    }
}

/// A slot descriptor: what the slot holds and where it is in the slot's life.
///
/// `addr`, `seq` and `crc32c` change only while the slot is `Free`; `word`
/// is written last, by one store, and seals the whole descriptor.
#[derive(FromBytes, IntoBytes, Immutable, KnownLayout, Clone, Copy, PartialEq, Eq, Hash, Debug)]
#[repr(C)]
pub struct SlotDesc {
    /// `vol << 40 | lba` (see [`SlotDesc::pack_addr`]).
    pub addr: u64,
    /// The write's sequence number (one per request).
    pub seq: u64,
    /// CRC-32C of the block.
    pub crc32c: u32,
    /// Reserved, zero.
    pub rsvd: u32,
    /// Low 32 bits: `lap << 2 | state`; high 32 bits: CRC-32C over bytes
    /// `0..24` followed by those low 32 bits.
    pub word: u64,
}

impl SlotDesc {
    /// Packs a volume and an LBA into `addr`.
    #[must_use]
    pub const fn pack_addr(vol: u32, lba: u64) -> u64 {
        (vol as u64) << 40 | (lba & ((1 << 40) - 1))
    }

    /// The volume.
    #[must_use]
    pub const fn vol(&self) -> u32 {
        (self.addr >> 40) as u32
    }

    /// The LBA.
    #[must_use]
    pub const fn lba(&self) -> u64 {
        self.addr & ((1 << 40) - 1)
    }

    /// The lap.
    #[must_use]
    pub const fn lap(&self) -> u32 {
        (self.word as u32) >> 2
    }

    /// The state.
    #[must_use]
    pub const fn state(&self) -> SlotState {
        SlotState::from_bits(self.word as u32)
    }

    /// The sealed word this descriptor's fields would carry with `state` and
    /// `lap`.
    #[must_use]
    pub fn word_for(&self, state: SlotState, lap: u32) -> u64 {
        let low = (lap & MAX_LAP) << 2 | u32::from(state.as_u8());
        let mut bytes = [0u8; SLOT_WORD_OFFSET + 4];
        bytes[..SLOT_WORD_OFFSET].copy_from_slice(&self.as_bytes()[..SLOT_WORD_OFFSET]);
        bytes[SLOT_WORD_OFFSET..].copy_from_slice(&low.to_le_bytes());
        u64::from(low) | u64::from(crc32c(&bytes)) << 32
    }

    /// `true` when the word is non-zero and its CRC matches the descriptor:
    /// the descriptor was written whole by this code and not touched since.
    #[must_use]
    pub fn verify(&self) -> bool {
        self.word != 0 && self.word_for(self.state(), self.lap()) == self.word
    }

    /// For a word that does not verify: the state whose sealed word the
    /// stored CRC belongs to, if only the state bits differ. A slot that was
    /// placed and now reads `Free` or `Filled` is damage, not an
    /// interrupted fill.
    #[must_use]
    pub fn sealed_as(&self) -> Option<SlotState> {
        let stored = self.word >> 32;
        [
            SlotState::Free,
            SlotState::Filled,
            SlotState::Placed,
            SlotState::Committed,
        ]
        .into_iter()
        .filter(|s| *s != self.state())
        .find(|s| self.word_for(*s, self.lap()) >> 32 == stored)
    }
}

/// A run's first slot and the lap it was handed out on.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct RingRef {
    /// The first slot of the run.
    pub slot: u32,
    /// The lap.
    pub lap: u32,
}

impl RingRef {
    /// The reference `blocks` slots further into the run.
    #[must_use]
    pub const fn offset(self, blocks: u32) -> Self {
        Self {
            slot: self.slot + blocks,
            lap: self.lap,
        }
    }
}

/// A `Placed` slot found by [`Arena::attach`]; its descriptor and its data
/// verified.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct PlacedSlot {
    /// Where the data is.
    pub ring: RingRef,
    /// The volume.
    pub vol: u32,
    /// The LBA.
    pub lba: u64,
    /// The write's sequence number.
    pub seq: u64,
    /// The block's CRC-32C, as recorded at fill time and verified at attach.
    pub crc32c: u32,
}

/// What was wrong with a damaged slot.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Damage {
    /// The descriptor's word does not verify.
    Descriptor,
    /// The descriptor verifies but the block fails its recorded CRC.
    Data,
}

/// A slot [`Arena::attach`] could not trust: a `Placed` or `Committed` slot
/// whose descriptor does not verify, or a `Placed` slot whose data does not.
/// The slot was released; if it was `Placed`, an acknowledged write may be
/// lost and the engine treats the boot like a power loss.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct DamagedSlot {
    /// The slot.
    pub slot: u32,
    /// The descriptor as found (its fields may be garbage).
    pub desc: SlotDesc,
    /// What failed.
    pub damage: Damage,
}

/// What [`Arena::attach`] found.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Reattach {
    /// The `Placed` slots, in `(seq, position)` order: the writes that may
    /// have been acknowledged and are not yet committed.
    pub placed: Vec<PlacedSlot>,
    /// Slots that could not be trusted.
    pub damaged: Vec<DamagedSlot>,
    /// `Filled` slots dropped because their write was never acknowledged.
    pub dropped_filled: u32,
    /// `Committed` slots found (free for reuse).
    pub committed: u32,
    /// The attach count after this attach (1 after a torn counter).
    pub attach_count: u32,
}

impl Reattach {
    /// `true` when any slot was damaged. A damaged descriptor's state cannot
    /// be trusted, so any damage may mean an acknowledged write is gone.
    #[must_use]
    pub fn is_damaged(&self) -> bool {
        !self.damaged.is_empty()
    }
}

/// Why the arena refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum ArenaError {
    /// The memory is too small for a header, one descriptor page and one slot.
    TooSmall {
        /// Bytes offered.
        len: usize,
    },
    /// The header magic is wrong (not an arena, or a lost one).
    BadMagic {
        /// What was found.
        found: u32,
    },
    /// The header was written by another major version.
    BadVersion {
        /// What was found.
        found: u16,
    },
    /// The header CRC does not match.
    BadCrc,
    /// The header's geometry does not match the memory's length.
    BadGeometry,
    /// The header names another aggregate.
    WrongAggregate {
        /// The aggregate the arena belongs to.
        found: Uuid,
    },
    /// The descriptor table cannot be reconciled into one window (damage
    /// that passed the descriptor CRCs).
    Inconsistent {
        /// What did not add up.
        reason: &'static str,
    },
    /// A run of this length cannot be handed out now; a consistency point
    /// must retire a generation first.
    Full {
        /// Slots asked for.
        wanted: u32,
        /// Slots free in the window right now.
        free: u32,
    },
    /// A run length of zero, or longer than the ring.
    BadRun {
        /// Slots asked for.
        wanted: u32,
    },
    /// The lap counter reached [`MAX_LAP`].
    LapExhausted,
    /// A reference whose lap no longer matches the slot's.
    Stale {
        /// The reference.
        ring: RingRef,
        /// The lap the slot is on now.
        lap: u32,
    },
    /// A slot index at or beyond the ring.
    OutOfRange {
        /// The slot.
        slot: u32,
    },
    /// A volume or LBA outside the format's bounds.
    BadAddress {
        /// The volume.
        vol: u32,
        /// The LBA.
        lba: u64,
    },
    /// A transition that the slot's state does not allow.
    BadTransition {
        /// The slot.
        slot: u32,
        /// Its state.
        state: SlotState,
    },
    /// A block's data fails the CRC its descriptor recorded.
    DataCrc {
        /// The slot.
        slot: u32,
    },
}

impl fmt::Display for ArenaError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::TooSmall { len } => write!(f, "{len} bytes is too small for an arena"),
            Self::BadMagic { found } => write!(f, "not an arena: magic {found:#010x}"),
            Self::BadVersion { found } => write!(f, "arena layout version {found:#06x}"),
            Self::BadCrc => f.write_str("arena header CRC mismatch"),
            Self::BadGeometry => f.write_str("arena header geometry does not match the memory"),
            Self::WrongAggregate { found } => write!(f, "arena belongs to aggregate {found}"),
            Self::Inconsistent { reason } => write!(f, "arena descriptors inconsistent: {reason}"),
            Self::Full { wanted, free } => {
                write!(f, "arena full: {wanted} slots wanted, {free} free")
            }
            Self::BadRun { wanted } => write!(f, "a run of {wanted} slots cannot exist"),
            Self::LapExhausted => f.write_str("arena lap counter exhausted"),
            Self::Stale { ring, lap } => write!(
                f,
                "slot {} of lap {} was reused (now on lap {lap})",
                ring.slot, ring.lap
            ),
            Self::OutOfRange { slot } => write!(f, "slot {slot} is beyond the ring"),
            Self::BadAddress { vol, lba } => write!(f, "volume {vol} LBA {lba} is out of bounds"),
            Self::BadTransition { slot, state } => {
                write!(
                    f,
                    "slot {slot} in state {state:?} cannot make this transition"
                )
            }
            Self::DataCrc { slot } => write!(f, "slot {slot} fails its block CRC"),
        }
    }
}

impl std::error::Error for ArenaError {}

/// The regions of an arena of a given length.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Geometry {
    slots: u32,
    desc_off: usize,
    data_off: usize,
}

impl Geometry {
    /// The most slots `len` bytes can hold: the header, a descriptor table
    /// padded to whole pages, then the slots.
    fn for_len(len: usize) -> Result<Self, ArenaError> {
        if len < ARENA_HEADER_SIZE + 2 * BLOCK_SIZE {
            return Err(ArenaError::TooSmall { len });
        }
        let usable = len - ARENA_HEADER_SIZE;
        let mut slots = usable / (SLOT_BYTES + SLOT_DESC_SIZE);
        loop {
            let desc_bytes = (slots * SLOT_DESC_SIZE).div_ceil(BLOCK_SIZE) * BLOCK_SIZE;
            if desc_bytes + slots * SLOT_BYTES <= usable {
                break;
            }
            slots -= 1;
        }
        if slots == 0 || slots > u32::MAX as usize {
            return Err(ArenaError::TooSmall { len });
        }
        let desc_bytes = (slots * SLOT_DESC_SIZE).div_ceil(BLOCK_SIZE) * BLOCK_SIZE;
        Ok(Self {
            slots: slots as u32,
            desc_off: ARENA_HEADER_SIZE,
            data_off: ARENA_HEADER_SIZE + desc_bytes,
        })
    }
}

/// The write-cache arena.
#[derive(Debug)]
pub struct Arena {
    mem: HugeArena,
    slots: u32,
    desc_off: usize,
    data_off: usize,
    /// The next position to hand out (`lap * slots + slot`).
    head: u64,
    /// The oldest position that may still hold unflushed data.
    tail: u64,
    next_seq: u64,
}

impl Arena {
    /// Formats `mem` as a fresh arena for `agg_id`. Every slot is `Free`.
    /// Gives the memory back with the error when it is too small.
    pub fn create(
        mut mem: HugeArena,
        agg_id: Uuid,
        wall_ns: u64,
    ) -> Result<Self, (ArenaError, HugeArena)> {
        let geometry = match Geometry::for_len(mem.len()) {
            Ok(g) => g,
            Err(e) => return Err((e, mem)),
        };
        let bytes = mem.as_mut_slice();
        bytes[..geometry.data_off].fill(0);
        let mut hdr = ArenaHeader::new_zeroed();
        hdr.magic = ARENA_MAGIC;
        hdr.ver = ARENA_VERSION;
        hdr.agg_id = agg_id;
        hdr.slots = geometry.slots;
        hdr.slot_bytes = SLOT_BYTES as u32;
        hdr.desc_off = geometry.desc_off as u64;
        hdr.data_off = geometry.data_off as u64;
        hdr.created_wall_ns = wall_ns;
        hdr.attach_word = ArenaHeader::encode_attach_count(1);
        hdr.seal();
        bytes[..ARENA_HEADER_SIZE].copy_from_slice(hdr.as_bytes());
        Ok(Self {
            mem,
            slots: geometry.slots,
            desc_off: geometry.desc_off,
            data_off: geometry.data_off,
            head: 0,
            tail: 0,
            next_seq: 1,
        })
    }

    /// `true` when the header page is all zero: fresh memory, or memory a
    /// power loss emptied. Anything else that fails
    /// [`can_attach`](Self::can_attach) is damage or another arena.
    #[must_use]
    pub fn is_blank(mem: &HugeArena) -> bool {
        let len = mem.len().min(ARENA_HEADER_SIZE);
        mem.as_slice()[..len].iter().all(|b| *b == 0)
    }

    /// Checks whether `mem` holds an arena of `agg_id` that
    /// [`attach`](Self::attach) would accept, without touching it.
    pub fn can_attach(mem: &HugeArena, agg_id: &Uuid) -> Result<(), ArenaError> {
        let len = mem.len();
        if len < ARENA_HEADER_SIZE {
            return Err(ArenaError::TooSmall { len });
        }
        let hdr = ArenaHeader::read_from_bytes(&mem.as_slice()[..ARENA_HEADER_SIZE])
            .map_err(|_| ArenaError::BadGeometry)?;
        hdr.verify(len)?;
        if hdr.agg_id != *agg_id {
            return Err(ArenaError::WrongAggregate { found: hdr.agg_id });
        }
        Ok(())
    }

    /// Re-attaches to an arena left by an earlier process. See the crate
    /// documentation for what is kept, dropped and reported. Gives the
    /// memory back with the error when it refuses.
    pub fn attach(
        mem: HugeArena,
        agg_id: &Uuid,
    ) -> Result<(Self, Reattach), (ArenaError, HugeArena)> {
        if let Err(e) = Self::can_attach(&mem, agg_id) {
            return Err((e, mem));
        }
        let len = mem.len();
        let hdr = match ArenaHeader::read_from_bytes(&mem.as_slice()[..ARENA_HEADER_SIZE]) {
            Ok(hdr) => hdr,
            Err(_) => return Err((ArenaError::BadGeometry, mem)),
        };
        let geometry = match Geometry::for_len(len) {
            Ok(g) => g,
            Err(e) => return Err((e, mem)),
        };
        let mut arena = Self {
            mem,
            slots: geometry.slots,
            desc_off: geometry.desc_off,
            data_off: geometry.data_off,
            head: 0,
            tail: 0,
            next_seq: 1,
        };
        match arena.scan_on_attach(&hdr) {
            Ok(report) => Ok((arena, report)),
            Err(e) => Err((e, arena.mem)),
        }
    }

    /// The scan behind [`attach`](Self::attach), once the header verified.
    fn scan_on_attach(&mut self, hdr: &ArenaHeader) -> Result<Reattach, ArenaError> {
        let arena = self;
        let mut report = Reattach::default();
        let slots = u64::from(arena.slots);
        let mut max_pos: Option<u64> = None;
        let mut min_placed: Option<u64> = None;
        let mut max_seq = 0u64;
        // What each slot's word becomes, applied after the scan so the head
        // and the tail come from the table as it was found.
        let mut rewrite: Vec<(u32, SlotState, u32)> = Vec::new();
        for slot in 0..arena.slots {
            let desc = *arena.desc(slot);
            max_seq = max_seq.max(desc.seq);
            let valid = desc.verify();
            let lap = desc.lap();
            let pos = u64::from(lap) * slots + u64::from(slot);
            let mut note_pos = || max_pos = Some(max_pos.map_or(pos, |m| m.max(pos)));
            match desc.state() {
                SlotState::Free => {
                    if valid {
                        // A reservation of the dead process: never filled,
                        // never acknowledged, reusable.
                        note_pos();
                        rewrite.push((slot, SlotState::Committed, lap));
                    } else if desc.word != 0 && desc.sealed_as().is_some() {
                        // The word was sealed in another state and only the
                        // state bits differ: damage, not an interrupted
                        // fill (a never-used slot has a zero word; an
                        // interrupted fill changed the fields, not the word).
                        report.damaged.push(DamagedSlot {
                            slot,
                            desc,
                            damage: Damage::Descriptor,
                        });
                        rewrite.push((slot, SlotState::Committed, lap));
                    }
                }
                SlotState::Filled => {
                    if valid {
                        report.dropped_filled += 1;
                        note_pos();
                    } else {
                        // A fill seals its word in one store: an unsealed
                        // `Filled` slot was something else before.
                        report.damaged.push(DamagedSlot {
                            slot,
                            desc,
                            damage: Damage::Descriptor,
                        });
                    }
                    rewrite.push((slot, SlotState::Committed, lap));
                }
                SlotState::Placed => {
                    if !valid {
                        report.damaged.push(DamagedSlot {
                            slot,
                            desc,
                            damage: Damage::Descriptor,
                        });
                        rewrite.push((slot, SlotState::Committed, lap));
                        continue;
                    }
                    note_pos();
                    if crc32c(arena.data_of(slot)) != desc.crc32c {
                        report.damaged.push(DamagedSlot {
                            slot,
                            desc,
                            damage: Damage::Data,
                        });
                        rewrite.push((slot, SlotState::Committed, lap));
                        continue;
                    }
                    report.placed.push(PlacedSlot {
                        ring: RingRef { slot, lap },
                        vol: desc.vol(),
                        lba: desc.lba(),
                        seq: desc.seq,
                        crc32c: desc.crc32c,
                    });
                    min_placed = Some(min_placed.map_or(pos, |m| m.min(pos)));
                }
                SlotState::Committed => {
                    if valid {
                        report.committed += 1;
                        note_pos();
                    } else {
                        report.damaged.push(DamagedSlot {
                            slot,
                            desc,
                            damage: Damage::Descriptor,
                        });
                        rewrite.push((slot, SlotState::Committed, lap));
                    }
                }
            }
        }
        arena.head = max_pos.map_or(0, |m| m + 1);
        arena.tail = min_placed.unwrap_or(arena.head);
        if arena.head - arena.tail > slots {
            return Err(ArenaError::Inconsistent {
                reason: "the live window is wider than the ring",
            });
        }
        arena.next_seq = max_seq + 1;
        for (slot, state, lap) in rewrite {
            arena.write_word(slot, state, lap);
        }
        arena.advance_tail();
        report.placed.sort_by_key(|p| {
            (
                p.seq,
                u64::from(p.ring.lap) * slots + u64::from(p.ring.slot),
            )
        });
        let count = hdr.attach_count().map_or(1, |c| c.wrapping_add(1).max(1));
        let word = ArenaHeader::encode_attach_count(count).to_le_bytes();
        arena.mem.as_mut_slice()[ARENA_ATTACH_WORD_OFFSET..ARENA_ATTACH_WORD_OFFSET + 8]
            .copy_from_slice(&word);
        report.attach_count = count;
        Ok(report)
    }

    /// The header as stored.
    #[must_use]
    pub fn header(&self) -> ArenaHeader {
        ArenaHeader::read_from_bytes(&self.mem.as_slice()[..ARENA_HEADER_SIZE])
            .unwrap_or_else(|_| ArenaHeader::new_zeroed())
    }

    /// Slots in the ring.
    #[must_use]
    pub const fn slots(&self) -> u32 {
        self.slots
    }

    /// Bytes of data the ring can hold.
    #[must_use]
    pub const fn capacity_bytes(&self) -> u64 {
        self.slots as u64 * SLOT_BYTES as u64
    }

    /// Slots in the window `[tail, head)`: handed out and not yet reusable
    /// (plus the free slots the tail has not passed yet).
    #[must_use]
    pub const fn occupied(&self) -> u32 {
        (self.head - self.tail) as u32
    }

    /// `occupied / slots`, in `0.0..=1.0`.
    #[must_use]
    pub fn occupancy(&self) -> f64 {
        f64::from(self.occupied()) / f64::from(self.slots)
    }

    /// The next sequence number, which is never handed out twice.
    pub fn next_seq(&mut self) -> u64 {
        let seq = self.next_seq;
        self.next_seq += 1;
        seq
    }

    /// The sequence number the next call to [`next_seq`](Self::next_seq)
    /// returns.
    #[must_use]
    pub const fn peek_seq(&self) -> u64 {
        self.next_seq
    }

    /// Hands out a run of `blocks` consecutive slots. The run never wraps;
    /// the slots are `Free` afterwards with their lap set, and their data is
    /// whatever the previous lap left (callers overwrite it). Refused with
    /// [`ArenaError::Full`] when the run would reach into live data, counting
    /// the slots a skip to the next lap would leave dead as passable.
    pub fn reserve(&mut self, blocks: u32) -> Result<RingRef, ArenaError> {
        if blocks == 0 || blocks > self.slots {
            return Err(ArenaError::BadRun { wanted: blocks });
        }
        self.advance_tail();
        let slots = u64::from(self.slots);
        let at = self.head % slots;
        let pos = if at + u64::from(blocks) > slots {
            self.head + (slots - at)
        } else {
            self.head
        };
        // Where the tail would stand once the skipped slots are dead.
        let mut tail = self.tail;
        while tail < pos && (tail >= self.head || self.passable(tail)) {
            tail += 1;
        }
        if pos + u64::from(blocks) - tail > slots {
            return Err(ArenaError::Full {
                wanted: blocks,
                free: slots.saturating_sub(self.head - self.tail) as u32,
            });
        }
        let lap = pos / slots;
        if lap > u64::from(MAX_LAP) {
            return Err(ArenaError::LapExhausted);
        }
        let lap = lap as u32;
        // Skipped slots at the end of the previous lap hold nothing: they
        // are marked reusable so the tail passes them.
        let mut skip = self.head;
        while skip < pos {
            self.write_word(
                (skip % slots) as u32,
                SlotState::Committed,
                (skip / slots) as u32,
            );
            skip += 1;
        }
        self.head = pos;
        self.advance_tail();
        let first = (pos % slots) as u32;
        for i in 0..blocks {
            debug_assert!(self.desc(first + i).state().is_reusable());
            self.write_word(first + i, SlotState::Free, lap);
        }
        self.head = pos + u64::from(blocks);
        Ok(RingRef { slot: first, lap })
    }

    /// The data of a run, for filling. The run must have been handed out
    /// by [`reserve`](Self::reserve) with the same length and not be placed.
    pub fn run_mut(&mut self, ring: RingRef, blocks: u32) -> Result<&mut [u8], ArenaError> {
        self.check_run(ring, blocks)?;
        self.check_states(ring, blocks, &[SlotState::Free, SlotState::Filled])?;
        let start = self.data_off + ring.slot as usize * SLOT_BYTES;
        let end = start + blocks as usize * SLOT_BYTES;
        Ok(&mut self.mem.as_mut_slice()[start..end])
    }

    /// The data of a run that is `Filled` or `Placed` on the given lap.
    pub fn run(&self, ring: RingRef, blocks: u32) -> Result<&[u8], ArenaError> {
        self.check_run(ring, blocks)?;
        self.check_states(ring, blocks, &[SlotState::Filled, SlotState::Placed])?;
        let start = self.data_off + ring.slot as usize * SLOT_BYTES;
        let end = start + blocks as usize * SLOT_BYTES;
        Ok(&self.mem.as_slice()[start..end])
    }

    /// Checks every block of a `Filled` or `Placed` run against the CRC its
    /// descriptor recorded at fill time.
    pub fn check_data(&self, ring: RingRef, blocks: u32) -> Result<(), ArenaError> {
        let data = self.run(ring, blocks)?;
        for i in 0..blocks {
            let block = &data[i as usize * SLOT_BYTES..(i as usize + 1) * SLOT_BYTES];
            if crc32c(block) != self.desc(ring.slot + i).crc32c {
                return Err(ArenaError::DataCrc {
                    slot: ring.slot + i,
                });
            }
        }
        Ok(())
    }

    /// Records what a run holds: block `i` of the run is `lba + i` of `vol`,
    /// written by request `seq` with the given CRC. Every slot becomes
    /// `Filled`, or none does. The data must already be in place
    /// ([`run_mut`](Self::run_mut)).
    pub fn fill(
        &mut self,
        ring: RingRef,
        vol: u32,
        lba: u64,
        seq: u64,
        crcs: &[u32],
    ) -> Result<(), ArenaError> {
        let blocks =
            u32::try_from(crcs.len()).map_err(|_| ArenaError::BadRun { wanted: u32::MAX })?;
        self.check_run(ring, blocks)?;
        if vol > MAX_VOL || lba.saturating_add(u64::from(blocks)) > MAX_LBA + 1 {
            return Err(ArenaError::BadAddress { vol, lba });
        }
        self.check_states(ring, blocks, &[SlotState::Free])?;
        for (i, crc) in crcs.iter().enumerate() {
            let slot = ring.slot + i as u32;
            let desc = self.desc_mut(slot);
            desc.addr = SlotDesc::pack_addr(vol, lba + i as u64);
            desc.seq = seq;
            desc.crc32c = *crc;
            desc.rsvd = 0;
            self.write_word(slot, SlotState::Filled, ring.lap);
        }
        Ok(())
    }

    /// Marks a `Filled` run `Placed`: it is in the dirty index and may be
    /// acknowledged.
    pub fn place(&mut self, ring: RingRef, blocks: u32) -> Result<(), ArenaError> {
        self.transition(ring, blocks, &[SlotState::Filled], SlotState::Placed)
    }

    /// Marks a `Placed` run `Committed`: the consistency point that carried
    /// it is durable, and the slots are reusable once the tail passes them.
    pub fn commit(&mut self, ring: RingRef, blocks: u32) -> Result<(), ArenaError> {
        self.transition(ring, blocks, &[SlotState::Placed], SlotState::Committed)?;
        self.advance_tail();
        Ok(())
    }

    /// Releases a run that was never placed, reserved (`Free`) or `Filled`:
    /// the write was refused or its copy failed, and the slots become
    /// reusable (`Committed`).
    pub fn discard(&mut self, ring: RingRef, blocks: u32) -> Result<(), ArenaError> {
        self.transition(
            ring,
            blocks,
            &[SlotState::Free, SlotState::Filled],
            SlotState::Committed,
        )?;
        self.advance_tail();
        Ok(())
    }

    /// The descriptor of a slot.
    pub fn descriptor(&self, slot: u32) -> Result<SlotDesc, ArenaError> {
        if slot >= self.slots {
            return Err(ArenaError::OutOfRange { slot });
        }
        Ok(*self.desc(slot))
    }

    /// The window of positions that may hold unflushed data: `(tail, head)`.
    #[must_use]
    pub const fn window(&self) -> (u64, u64) {
        (self.tail, self.head)
    }

    /// Gives the memory back (the sim keeps it across a daemon restart).
    #[must_use]
    pub fn into_memory(self) -> HugeArena {
        self.mem
    }

    /// The memory, for the owner's bookkeeping (locking, core-dump policy).
    #[must_use]
    pub const fn memory(&self) -> &HugeArena {
        &self.mem
    }

    /// The memory, writable: for a support bundle or a fault injection.
    /// Nothing the arena relies on is checked again after this hands out
    /// the slice; the descriptor and data CRCs catch what gets changed.
    pub fn memory_mut(&mut self) -> &mut HugeArena {
        &mut self.mem
    }

    fn transition(
        &mut self,
        ring: RingRef,
        blocks: u32,
        from: &[SlotState],
        to: SlotState,
    ) -> Result<(), ArenaError> {
        self.check_run(ring, blocks)?;
        self.check_states(ring, blocks, from)?;
        for i in 0..blocks {
            self.write_word(ring.slot + i, to, ring.lap);
        }
        Ok(())
    }

    fn check_run(&self, ring: RingRef, blocks: u32) -> Result<(), ArenaError> {
        if blocks == 0 || blocks > self.slots {
            return Err(ArenaError::BadRun { wanted: blocks });
        }
        if ring.slot >= self.slots || ring.slot + blocks > self.slots {
            return Err(ArenaError::OutOfRange { slot: ring.slot });
        }
        for i in 0..blocks {
            let desc = self.desc(ring.slot + i);
            if desc.lap() != ring.lap {
                return Err(ArenaError::Stale {
                    ring,
                    lap: desc.lap(),
                });
            }
        }
        Ok(())
    }

    fn check_states(
        &self,
        ring: RingRef,
        blocks: u32,
        allowed: &[SlotState],
    ) -> Result<(), ArenaError> {
        for i in 0..blocks {
            let state = self.desc(ring.slot + i).state();
            if !allowed.contains(&state) {
                return Err(ArenaError::BadTransition {
                    slot: ring.slot + i,
                    state,
                });
            }
        }
        Ok(())
    }

    /// `true` when the tail may move past `pos`: the slot there is on
    /// another lap (never handed out on this one) or committed. On the
    /// tail's own lap a `Free` slot is a reservation not yet filled and
    /// stays in the window.
    fn passable(&self, pos: u64) -> bool {
        let slots = u64::from(self.slots);
        let desc = self.desc((pos % slots) as u32);
        desc.lap() != (pos / slots) as u32 || desc.state() == SlotState::Committed
    }

    fn advance_tail(&mut self) {
        while self.tail < self.head && self.passable(self.tail) {
            self.tail += 1;
        }
    }

    /// Seals and stores a slot's word: the one store that moves a slot
    /// between states. The compiler fence keeps the field stores before it
    /// in program order; the memory outlives the process, so program order
    /// is all a `kill -9` can observe.
    fn write_word(&mut self, slot: u32, state: SlotState, lap: u32) {
        let desc = self.desc_mut(slot);
        let word = desc.word_for(state, lap);
        compiler_fence(Ordering::SeqCst);
        desc.word = word;
    }

    fn data_of(&self, slot: u32) -> &[u8] {
        let start = self.data_off + slot as usize * SLOT_BYTES;
        &self.mem.as_slice()[start..start + SLOT_BYTES]
    }

    fn desc(&self, slot: u32) -> &SlotDesc {
        let at = self.desc_off + slot as usize * SLOT_DESC_SIZE;
        SlotDesc::ref_from_bytes(&self.mem.as_slice()[at..at + SLOT_DESC_SIZE])
            .expect("descriptor table is 8-byte aligned and sized")
    }

    fn desc_mut(&mut self, slot: u32) -> &mut SlotDesc {
        let at = self.desc_off + slot as usize * SLOT_DESC_SIZE;
        SlotDesc::mut_from_bytes(&mut self.mem.as_mut_slice()[at..at + SLOT_DESC_SIZE])
            .expect("descriptor table is 8-byte aligned and sized")
    }
}

/// The most slots an arena of `len` bytes holds, or an error for a length
/// too small to hold one.
pub fn slots_for_len(len: usize) -> Result<u32, ArenaError> {
    Geometry::for_len(len).map(|g| g.slots)
}

const _: () = {
    assert!(size_of::<ArenaHeader>() == ARENA_HEADER_SIZE);
    assert!(align_of::<ArenaHeader>() == 4096);
    assert!(offset_of!(ArenaHeader, agg_id) == 8);
    assert!(offset_of!(ArenaHeader, slots) == 24);
    assert!(offset_of!(ArenaHeader, desc_off) == 32);
    assert!(offset_of!(ArenaHeader, created_wall_ns) == 48);
    assert!(offset_of!(ArenaHeader, crc32c) == ARENA_HEADER_SEALED);
    assert!(offset_of!(ArenaHeader, attach_word) == ARENA_ATTACH_WORD_OFFSET);
    assert!(offset_of!(ArenaHeader, reserved) == 72);
    assert!(size_of::<SlotDesc>() == SLOT_DESC_SIZE);
    assert!(align_of::<SlotDesc>() == 8);
    assert!(offset_of!(SlotDesc, seq) == 8);
    assert!(offset_of!(SlotDesc, crc32c) == 16);
    assert!(offset_of!(SlotDesc, word) == SLOT_WORD_OFFSET);
    assert!(SlotState::Free.as_u8() == 0 && SlotState::Committed.as_u8() == 3);
    assert!(matches!(SlotState::from_bits(2), SlotState::Placed));
    assert!(matches!(SlotState::from_bits(7), SlotState::Committed));
    assert!(MAX_LAP == 0x3FFF_FFFF);
};
