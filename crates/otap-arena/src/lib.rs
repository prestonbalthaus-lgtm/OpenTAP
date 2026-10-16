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
//! no longer verifies: the reservation's seal over fields the fill has since
//! changed (a never-used slot has a zero word). A `Placed` or `Committed`
//! slot whose word does not verify was damaged by something other than this
//! code, and [`Arena::attach`] reports it instead of trusting it (invariant
//! 3 of section 7.7: every byte under a CRC).
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
//! A write is one run of slots with one sequence number, filled and placed in
//! slot order, so the attach judges a slot by its neighbours of the same
//! write (Step 6A):
//!
//! - A `Free` slot under a stale seal whose write has a `Filled` slot before
//!   it, or only its reservation's `Free` slots after it, is a fill the kill
//!   interrupted: never acknowledged, it is released (counted in
//!   [`Reattach::interrupted_fills`]) so the tail can pass it; left `Free`
//!   inside the window it would stop the tail for ever. One whose write has a
//!   `Placed` slot before it, or any filled, placed or committed slot after
//!   it, was past its fill: its word was damaged, and it is reported.
//! - A `Placed` slot whose write still has a `Filled` slot is a `place` the
//!   kill interrupted: the write was never acknowledged (the acknowledgement
//!   follows the whole `place`), so the whole write is dropped (counted in
//!   [`Reattach::torn_places`]) instead of replaying a torn prefix of it.
//!
//! # Deferred writes
//!
//! An attach rewrites descriptor words (the dropped, released and damaged
//! slots) and the attach counter, and a creation formats the memory. Both
//! erase what a boot may still need to see again: the damage, the blank or
//! lost arena that obliges it to hold the volumes whose writes it lost. A
//! boot therefore uses [`Arena::attach_deferred`] or
//! [`Arena::create_deferred`], which write nothing, and calls
//! [`Arena::apply_pending_writes`] only once it has recorded its hold
//! durably; a boot that stops before then leaves the memory as it found it,
//! and the next boot finds the same evidence. Nothing is handed out while
//! writes are pending ([`ArenaError::Pending`]).
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
use std::collections::HashSet;

use otap_format::checksum::crc32c;
use otap_format::{
    FromBytes, FromZeros, Immutable, IntoBytes, KnownLayout, Uuid, BLOCK_SIZE, MAX_LBA, MAX_VOL,
};
use otap_sys::HugeArena;

/// The arena header magic: `OTAR`.
pub const ARENA_MAGIC: u32 = u32::from_le_bytes(*b"OTAR");
/// The arena layout version, `major << 8 | minor`. Minor 1: sealed
/// descriptor words, the attach counter outside the header seal. Minor 2
/// (Step 4): the descriptor's `kind` word and hole record slots.
pub const ARENA_VERSION: u16 = 0x0102;
/// The magic of a hole record block: `OTHL`.
pub const HOLE_RECORD_MAGIC: u32 = u32::from_le_bytes(*b"OTHL");
/// Bytes of a [`HoleRecordBlock`] that carry the record; the rest of the
/// slot is zero.
pub const HOLE_RECORD_SIZE: usize = 32;
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

    /// Checks magic, major version and CRC: the header is an arena's
    /// header, whatever memory it lies in.
    pub fn verify_identity(&self) -> Result<(), ArenaError> {
        if self.magic != ARENA_MAGIC {
            return Err(ArenaError::BadMagic { found: self.magic });
        }
        if self.ver >> 8 != ARENA_VERSION >> 8 {
            return Err(ArenaError::BadVersion { found: self.ver });
        }
        if self.compute_crc() != self.crc32c {
            return Err(ArenaError::BadCrc);
        }
        Ok(())
    }

    /// Checks magic, major version, CRC and the geometry against `len`.
    pub fn verify(&self, len: usize) -> Result<(), ArenaError> {
        self.verify_identity()?;
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

/// What a slot holds.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[repr(u32)]
pub enum SlotKind {
    /// One 4 KiB block of a write.
    Data = 0,
    /// A [`HoleRecordBlock`]: a discard of a run of blocks, held until the
    /// consistency point that records the holes commits (Step 4, E23).
    Hole = 1,
}

impl SlotKind {
    /// Decodes the descriptor's kind word.
    #[must_use]
    pub const fn from_u32(value: u32) -> Option<Self> {
        match value {
            0 => Some(Self::Data),
            1 => Some(Self::Hole),
            _ => None,
        }
    }

    /// The kind word.
    #[must_use]
    pub const fn as_u32(self) -> u32 {
        self as u32
    }
}

/// The record a hole slot's data block carries: the discard of `blocks`
/// blocks from `lba` of `vol`. The descriptor names the same volume and
/// LBA and seals the whole block under its CRC; the magic and the repeated
/// address are a second check at re-attach.
#[derive(FromBytes, IntoBytes, Immutable, KnownLayout, Clone, Copy, PartialEq, Eq, Hash, Debug)]
#[repr(C)]
pub struct HoleRecordBlock {
    /// [`HOLE_RECORD_MAGIC`].
    pub magic: u32,
    /// The volume.
    pub vol: u32,
    /// The first LBA.
    pub lba: u64,
    /// Blocks punched, at least 1.
    pub blocks: u32,
    /// Reserved, zero.
    pub rsvd0: u32,
    /// Reserved, zero.
    pub rsvd1: u64,
}

impl HoleRecordBlock {
    /// The 4 KiB block image of a record: the record, then zeros.
    #[must_use]
    pub fn image(vol: u32, lba: u64, blocks: u32) -> [u8; SLOT_BYTES] {
        let rec = Self {
            magic: HOLE_RECORD_MAGIC,
            vol,
            lba,
            blocks,
            rsvd0: 0,
            rsvd1: 0,
        };
        let mut block = [0u8; SLOT_BYTES];
        block[..HOLE_RECORD_SIZE].copy_from_slice(rec.as_bytes());
        block
    }

    /// Decodes a slot's data block as a record of `vol`/`lba` (what its
    /// descriptor says): the magic, the address and a positive count must
    /// agree, and the rest of the block must be zero.
    #[must_use]
    pub fn decode(block: &[u8], vol: u32, lba: u64) -> Option<Self> {
        if block.len() != SLOT_BYTES {
            return None;
        }
        let rec = Self::read_from_bytes(&block[..HOLE_RECORD_SIZE]).ok()?;
        let ok = rec.magic == HOLE_RECORD_MAGIC
            && rec.vol == vol
            && rec.lba == lba
            && rec.blocks >= 1
            && rec.rsvd0 == 0
            && rec.rsvd1 == 0
            && lba.saturating_add(u64::from(rec.blocks)) <= MAX_LBA + 1
            && block[HOLE_RECORD_SIZE..].iter().all(|b| *b == 0);
        ok.then_some(rec)
    }
}

/// A slot descriptor: what the slot holds and where it is in the slot's life.
///
/// `addr`, `seq`, `crc32c` and `kind` change only while the slot is `Free`;
/// `word` is written last, by one store, and seals the whole descriptor.
#[derive(FromBytes, IntoBytes, Immutable, KnownLayout, Clone, Copy, PartialEq, Eq, Hash, Debug)]
#[repr(C)]
pub struct SlotDesc {
    /// `vol << 40 | lba` (see [`SlotDesc::pack_addr`]).
    pub addr: u64,
    /// The write's sequence number (one per request).
    pub seq: u64,
    /// CRC-32C of the block.
    pub crc32c: u32,
    /// A [`SlotKind`] word (layout 1.2; zero, data, before it).
    pub kind: u32,
    /// Low 32 bits: `lap << 2 | state`; high 32 bits: CRC-32C over bytes
    /// `0..24` followed by those low 32 bits.
    pub word: u64,
}

impl SlotDesc {
    /// The decoded kind, if defined.
    #[must_use]
    pub const fn slot_kind(&self) -> Option<SlotKind> {
        SlotKind::from_u32(self.kind)
    }

    /// `true` for a hole record slot.
    #[must_use]
    pub const fn is_hole(&self) -> bool {
        self.kind == SlotKind::Hole.as_u32()
    }

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
    /// Where the data (or the hole record) is.
    pub ring: RingRef,
    /// The volume.
    pub vol: u32,
    /// The LBA.
    pub lba: u64,
    /// The write's sequence number.
    pub seq: u64,
    /// The block's CRC-32C, as recorded at fill time and verified at attach.
    pub crc32c: u32,
    /// For a hole record slot, the blocks the discard punched from `lba`;
    /// 0 for a data block.
    pub hole_blocks: u32,
}

impl PlacedSlot {
    /// `true` for a hole record.
    #[must_use]
    pub const fn is_hole(&self) -> bool {
        self.hole_blocks != 0
    }
}

/// What was wrong with a damaged slot.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Damage {
    /// The descriptor's word does not verify.
    Descriptor,
    /// The descriptor verifies but the block fails its recorded CRC.
    Data,
    /// The descriptor verifies and names a kind this layout does not
    /// define, or a hole record whose block does not decode.
    Kind,
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

/// How the arena's writer fills its reservations, which decides what a
/// `Free` slot under a stale seal found at re-attach can be: a fill killed
/// between the slot's field stores and its word, or a damaged word.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Fills {
    /// Several reservations may be outstanding and filled in any order (the
    /// arena's general contract, and what its own random walk drives): an
    /// interrupted fill can lie anywhere in the window, so only a neighbour
    /// of the same write shows a stale slot to be damage.
    Interleaved,
    /// One reservation at a time, filled whole before the next is made (the
    /// engine's thread): an interrupted fill is the newest reservation's, so
    /// a stale slot with anything written after it is damage. This is what
    /// catches a damaged word of a one-block write, which has no neighbour
    /// of its own write (the Step 6 review's D10).
    Serial,
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
    /// `Placed` slots dropped because their write still had `Filled` slots:
    /// a `place` the kill interrupted, never acknowledged.
    pub torn_places: u32,
    /// `Free` slots under a stale seal inside the window, released: a fill
    /// the kill interrupted between a slot's field stores and its word.
    pub interrupted_fills: u32,
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

/// The page an arena's length is a whole number of: 2 MiB, the only size
/// the daemon maps.
const SURVEY_PAGE: usize = 2 * 1024 * 1024;

/// What [`Arena::survey`] found in an arena read under the geometry its
/// header records.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ArenaSurvey {
    /// The smallest memory, in whole 2 MiB pages, that holds the recorded
    /// geometry: the length the arena was made for.
    pub recorded_bytes: u64,
    /// Slots in the recorded ring.
    pub slots: u32,
    /// Slots in the `Placed` state, or sealed as placed under damaged state
    /// bits: writes that may have been acknowledged and that a recreation
    /// would destroy.
    pub placed: u32,
}

impl ArenaSurvey {
    /// `true` when nothing in the arena may be an acknowledged write.
    #[must_use]
    pub const fn holds_nothing(&self) -> bool {
        self.placed == 0
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
    /// The attach's or the creation's writes are still pending
    /// ([`Arena::apply_pending_writes`]): nothing is handed out on a table
    /// that is not written yet.
    Pending,
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
            Self::Pending => {
                f.write_str("the arena's re-attach or creation has not been written yet")
            }
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

/// Writes an attach or a creation left for [`Arena::apply_pending_writes`].
#[derive(Debug)]
enum Pending {
    /// The attach's descriptor words, `(slot, state, lap)` in order, and
    /// its attach counter word.
    Attach {
        rewrite: Vec<(u32, SlotState, u32)>,
        attach_word: u64,
    },
    /// A fresh arena's header, written after its descriptor table is
    /// zeroed.
    Create { header: Box<ArenaHeader> },
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
    /// Writes not yet made (deferred attach or creation).
    pending: Option<Pending>,
}

impl Arena {
    /// Formats `mem` as a fresh arena for `agg_id`. Every slot is `Free`.
    /// Gives the memory back with the error when it is too small.
    pub fn create(
        mem: HugeArena,
        agg_id: Uuid,
        wall_ns: u64,
    ) -> Result<Self, (ArenaError, HugeArena)> {
        let mut arena = Self::create_deferred(mem, agg_id, wall_ns)?;
        arena.apply_pending_writes();
        Ok(arena)
    }

    /// [`create`](Self::create) without writing anything yet: the memory
    /// keeps whatever it held (blank, or a lost arena) until
    /// [`apply_pending_writes`](Self::apply_pending_writes), so a process
    /// that stops before then leaves the next one the same evidence.
    /// Gives the memory back with the error when it is too small.
    pub fn create_deferred(
        mem: HugeArena,
        agg_id: Uuid,
        wall_ns: u64,
    ) -> Result<Self, (ArenaError, HugeArena)> {
        let geometry = match Geometry::for_len(mem.len()) {
            Ok(g) => g,
            Err(e) => return Err((e, mem)),
        };
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
        Ok(Self {
            mem,
            slots: geometry.slots,
            desc_off: geometry.desc_off,
            data_off: geometry.data_off,
            head: 0,
            tail: 0,
            next_seq: 1,
            pending: Some(Pending::Create {
                header: Box::new(hdr),
            }),
        })
    }

    /// `true` while an attach's or a creation's writes wait for
    /// [`apply_pending_writes`](Self::apply_pending_writes).
    #[must_use]
    pub const fn has_pending_writes(&self) -> bool {
        self.pending.is_some()
    }

    /// Makes the writes a deferred attach or creation left: the attach's
    /// descriptor words (with every release asked for meanwhile) and its
    /// attach counter, or the creation's zeroed descriptor table and then
    /// its header. A no-op when nothing is pending.
    pub fn apply_pending_writes(&mut self) {
        match self.pending.take() {
            None => {}
            Some(Pending::Attach {
                rewrite,
                attach_word,
            }) => {
                for (slot, state, lap) in rewrite {
                    self.write_word(slot, state, lap);
                }
                self.mem.as_mut_slice()[ARENA_ATTACH_WORD_OFFSET..ARENA_ATTACH_WORD_OFFSET + 8]
                    .copy_from_slice(&attach_word.to_le_bytes());
                self.advance_tail();
            }
            Some(Pending::Create { header }) => {
                // The table first, the header last: a kill in between
                // leaves a blank header, never a valid one over garbage.
                let bytes = self.mem.as_mut_slice();
                bytes[..self.data_off].fill(0);
                compiler_fence(Ordering::SeqCst);
                bytes[..ARENA_HEADER_SIZE].copy_from_slice(header.as_bytes());
            }
        }
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
    /// [`attach`](Self::attach) would accept, without touching it. The
    /// aggregate is checked before the geometry, so another pool's arena is
    /// [`ArenaError::WrongAggregate`] whatever its size, and
    /// [`ArenaError::BadGeometry`] means an arena of this pool made for
    /// another size ([`survey`](Self::survey) reads it).
    pub fn can_attach(mem: &HugeArena, agg_id: &Uuid) -> Result<(), ArenaError> {
        let len = mem.len();
        if len < ARENA_HEADER_SIZE {
            return Err(ArenaError::TooSmall { len });
        }
        let hdr = ArenaHeader::read_from_bytes(&mem.as_slice()[..ARENA_HEADER_SIZE])
            .map_err(|_| ArenaError::BadGeometry)?;
        hdr.verify_identity()?;
        if hdr.agg_id != *agg_id {
            return Err(ArenaError::WrongAggregate { found: hdr.agg_id });
        }
        hdr.verify(len)
    }

    /// Reads the arena of `agg_id` in `mem` under the geometry its header
    /// records, which need not be the one `mem`'s length gives (an arena
    /// file that a larger size grew, or one mapped at its own length to be
    /// judged before a resize), without touching it: how much it holds
    /// that a recreation would lose.
    ///
    /// # Errors
    /// The header's identity errors, [`ArenaError::WrongAggregate`] for
    /// another pool's arena, and [`ArenaError::BadGeometry`] when the
    /// recorded geometry is not one this layout builds or does not fit in
    /// `mem`.
    pub fn survey(mem: &HugeArena, agg_id: &Uuid) -> Result<ArenaSurvey, ArenaError> {
        let len = mem.len();
        if len < ARENA_HEADER_SIZE {
            return Err(ArenaError::TooSmall { len });
        }
        let hdr = ArenaHeader::read_from_bytes(&mem.as_slice()[..ARENA_HEADER_SIZE])
            .map_err(|_| ArenaError::BadGeometry)?;
        hdr.verify_identity()?;
        if hdr.agg_id != *agg_id {
            return Err(ArenaError::WrongAggregate { found: hdr.agg_id });
        }
        // The recorded geometry must be what `Geometry::for_len` builds for
        // the smallest whole number of 2 MiB pages that holds it, and lie
        // inside `mem`.
        let slots = hdr.slots as usize;
        let spans = usize::try_from(hdr.data_off)
            .ok()
            .zip(slots.checked_mul(SLOT_BYTES))
            .and_then(|(data, ring)| data.checked_add(ring))
            .filter(|_| slots > 0 && hdr.slot_bytes as usize == SLOT_BYTES)
            .ok_or(ArenaError::BadGeometry)?;
        let recorded_bytes = spans.div_ceil(SURVEY_PAGE) * SURVEY_PAGE;
        let geometry = Geometry::for_len(recorded_bytes).map_err(|_| ArenaError::BadGeometry)?;
        if geometry.slots != hdr.slots
            || geometry.desc_off as u64 != hdr.desc_off
            || geometry.data_off as u64 != hdr.data_off
            || spans > len
        {
            return Err(ArenaError::BadGeometry);
        }
        let mut survey = ArenaSurvey {
            recorded_bytes: recorded_bytes as u64,
            slots: hdr.slots,
            placed: 0,
        };
        for slot in 0..hdr.slots {
            let at = geometry.desc_off + slot as usize * SLOT_DESC_SIZE;
            let desc = SlotDesc::read_from_bytes(&mem.as_slice()[at..at + SLOT_DESC_SIZE])
                .map_err(|_| ArenaError::BadGeometry)?;
            // Placed, or a word sealed as placed whose state bits were
            // damaged: either may be an acknowledged write.
            let placed = desc.state() == SlotState::Placed
                || (!desc.verify() && desc.sealed_as() == Some(SlotState::Placed));
            if placed {
                survey.placed += 1;
            }
        }
        Ok(survey)
    }

    /// Re-attaches to an arena left by an earlier process. See the crate
    /// documentation for what is kept, dropped and reported. Gives the
    /// memory back with the error when it refuses.
    pub fn attach(
        mem: HugeArena,
        agg_id: &Uuid,
    ) -> Result<(Self, Reattach), (ArenaError, HugeArena)> {
        Self::attach_with(mem, agg_id, Fills::Interleaved)
    }

    /// [`attach`](Self::attach) for a writer that fills as `fills` says.
    pub fn attach_with(
        mem: HugeArena,
        agg_id: &Uuid,
        fills: Fills,
    ) -> Result<(Self, Reattach), (ArenaError, HugeArena)> {
        let (mut arena, report) = Self::attach_deferred_with(mem, agg_id, fills)?;
        arena.apply_pending_writes();
        Ok((arena, report))
    }

    /// [`attach`](Self::attach) without writing anything yet: the report,
    /// the window and the next sequence number are the attach's, and the
    /// memory is as it was found until
    /// [`apply_pending_writes`](Self::apply_pending_writes). Releases asked
    /// for meanwhile ([`release_after_attach`](Self::release_after_attach))
    /// join the pending writes.
    pub fn attach_deferred(
        mem: HugeArena,
        agg_id: &Uuid,
    ) -> Result<(Self, Reattach), (ArenaError, HugeArena)> {
        Self::attach_deferred_with(mem, agg_id, Fills::Interleaved)
    }

    /// [`attach_deferred`](Self::attach_deferred) for a writer that fills as
    /// `fills` says (the engine fills serially, [`Fills::Serial`]).
    pub fn attach_deferred_with(
        mem: HugeArena,
        agg_id: &Uuid,
        fills: Fills,
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
            pending: None,
        };
        match arena.scan_on_attach(&hdr, fills) {
            Ok(report) => Ok((arena, report)),
            Err(e) => Err((e, arena.mem)),
        }
    }

    /// The scan behind [`attach_deferred`](Self::attach_deferred), once the
    /// header verified: the report, the window and the next sequence number
    /// from the table as it was found, and the writes it calls for left
    /// pending.
    #[allow(clippy::too_many_lines)]
    fn scan_on_attach(&mut self, hdr: &ArenaHeader, fills: Fills) -> Result<Reattach, ArenaError> {
        let arena = self;
        let mut report = Reattach::default();
        let slots = u64::from(arena.slots);
        let mut max_pos: Option<u64> = None;
        let mut max_seq = 0u64;
        // What each slot's word becomes, applied after the scan so the head
        // and the tail come from the table as it was found.
        let mut rewrite: Vec<(u32, SlotState, u32)> = Vec::new();
        // The `Placed` slots that verify, with their positions, before the
        // interrupted places are taken out.
        let mut placed: Vec<(PlacedSlot, u64)> = Vec::new();
        // `(seq, lap)` of every `Filled` slot that verifies: a write whose
        // `place` had not finished.
        let mut filling: HashSet<(u64, u32)> = HashSet::new();
        // `Free` slots under a stale seal, judged once the head is known.
        let mut stale: Vec<(u32, SlotDesc)> = Vec::new();
        // The highest position of a slot that verifies as written (filled,
        // placed or committed): an interrupted fill is the newest
        // reservation's, so nothing written lies after it.
        let mut max_written: Option<u64> = None;
        // Slots whose word does not verify, rewritten once the head is known
        // under the lap their position implies, never the lap the damaged
        // word claims (sealed, a wrong lap would move the next attach's head
        // by laps: the Step 6 review's R5).
        let mut unverified: Vec<u32> = Vec::new();
        for slot in 0..arena.slots {
            let desc = *arena.desc(slot);
            max_seq = max_seq.max(desc.seq);
            let valid = desc.verify();
            let lap = desc.lap();
            let pos = u64::from(lap) * slots + u64::from(slot);
            if valid && desc.state() != SlotState::Free {
                max_written = Some(max_written.map_or(pos, |m| m.max(pos)));
            }
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
                    } else if desc.word != 0 {
                        // A seal over other fields: a fill interrupted
                        // between this slot's field stores and its word, or
                        // damage; its neighbours tell (below). Never noted:
                        // its lap is not verified, and a lap it does not
                        // have must not move the head.
                        stale.push((slot, desc));
                    }
                }
                SlotState::Filled => {
                    if valid {
                        report.dropped_filled += 1;
                        filling.insert((desc.seq, lap));
                        note_pos();
                        rewrite.push((slot, SlotState::Committed, lap));
                    } else {
                        // A fill seals its word in one store: an unsealed
                        // `Filled` slot was something else before.
                        report.damaged.push(DamagedSlot {
                            slot,
                            desc,
                            damage: Damage::Descriptor,
                        });
                        unverified.push(slot);
                    }
                }
                SlotState::Placed => {
                    if !valid {
                        report.damaged.push(DamagedSlot {
                            slot,
                            desc,
                            damage: Damage::Descriptor,
                        });
                        unverified.push(slot);
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
                    let hole_blocks = match desc.slot_kind() {
                        Some(SlotKind::Data) => 0,
                        Some(SlotKind::Hole) => {
                            match HoleRecordBlock::decode(
                                arena.data_of(slot),
                                desc.vol(),
                                desc.lba(),
                            ) {
                                Some(rec) => rec.blocks,
                                None => {
                                    report.damaged.push(DamagedSlot {
                                        slot,
                                        desc,
                                        damage: Damage::Kind,
                                    });
                                    rewrite.push((slot, SlotState::Committed, lap));
                                    continue;
                                }
                            }
                        }
                        None => {
                            report.damaged.push(DamagedSlot {
                                slot,
                                desc,
                                damage: Damage::Kind,
                            });
                            rewrite.push((slot, SlotState::Committed, lap));
                            continue;
                        }
                    };
                    placed.push((
                        PlacedSlot {
                            ring: RingRef { slot, lap },
                            vol: desc.vol(),
                            lba: desc.lba(),
                            seq: desc.seq,
                            crc32c: desc.crc32c,
                            hole_blocks,
                        },
                        pos,
                    ));
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
                        unverified.push(slot);
                    }
                }
            }
        }
        // A place the kill interrupted: the write still has `Filled` slots,
        // so it was never acknowledged (the acknowledgement follows the
        // whole `place`). Its placed prefix goes with the rest of it rather
        // than replaying a torn write.
        let mut min_placed: Option<u64> = None;
        for (p, pos) in placed {
            if filling.contains(&(p.seq, p.ring.lap)) {
                report.torn_places += 1;
                rewrite.push((p.ring.slot, SlotState::Committed, p.ring.lap));
            } else {
                min_placed = Some(min_placed.map_or(pos, |m| m.min(pos)));
                report.placed.push(p);
            }
        }
        arena.head = max_pos.map_or(0, |m| m + 1);
        arena.tail = min_placed.unwrap_or(arena.head);
        if arena.head - arena.tail > slots {
            return Err(ArenaError::Inconsistent {
                reason: "the live window is wider than the ring",
            });
        }
        for slot in unverified {
            let pos = implied_pos(slot, slots, arena.head, false);
            rewrite.push((slot, SlotState::Committed, lap_of(pos, slots)));
        }
        for (slot, desc) in stale {
            // The stale word's lap is not verified: the slot is judged at the
            // position its index takes in the window, up to the head (the
            // Step 6 review's D10).
            let pos = implied_pos(slot, slots, arena.head, true);
            let lap = lap_of(pos, slots);
            if pos >= arena.head {
                // At or beyond the head: outside the window, and the next
                // reservation re-seals it. (A fill killed on the last slot
                // of the newest reservation lands here, and so would damage
                // to the newest write's last word: the two cannot be told
                // apart, and the second needs damage to that one word.)
                continue;
            }
            let overtaken = fills == Fills::Serial && max_written.is_some_and(|w| w > pos);
            if arena.fill_went_past(slot, &desc, lap) || overtaken {
                // The write this slot belongs to was filled past it or
                // placed, or (for a serial writer, whose interrupted fill is
                // always its newest reservation's) something was written
                // after it: its word was damaged, and the block may have
                // been acknowledged.
                report.damaged.push(DamagedSlot {
                    slot,
                    desc,
                    damage: Damage::Descriptor,
                });
                rewrite.push((slot, SlotState::Committed, lap));
            } else {
                // An interrupted fill inside the window: released, so the
                // tail passes it.
                report.interrupted_fills += 1;
                rewrite.push((slot, SlotState::Committed, lap));
            }
        }
        arena.next_seq = max_seq + 1;
        report.placed.sort_by_key(|p| {
            (
                p.seq,
                u64::from(p.ring.lap) * slots + u64::from(p.ring.slot),
            )
        });
        let count = hdr.attach_count().map_or(1, |c| c.wrapping_add(1).max(1));
        report.attach_count = count;
        arena.pending = Some(Pending::Attach {
            rewrite,
            attach_word: ArenaHeader::encode_attach_count(count),
        });
        Ok(report)
    }

    /// For a `Free` slot under a stale seal: `true` when a neighbour of the
    /// same write (the same sequence, volume, kind and lap, at the previous
    /// or the next LBA) shows the write got past this slot's fill. A fill
    /// stores slot by slot in order, so an interrupted one has only
    /// `Filled` slots of its write before the stale slot and only its
    /// reservation's `Free` slots after it; a `Placed` slot of the write
    /// before it, or a filled, placed or committed one after it, means the
    /// word was damaged. (A `Committed` slot before it says nothing: an
    /// earlier attach released the `Filled` slots of an interrupted fill.)
    /// `lap` is the lap the slot's position implies, never the stale word's
    /// own, which is not verified.
    fn fill_went_past(&self, slot: u32, desc: &SlotDesc, lap: u32) -> bool {
        let same_write = |n: &SlotDesc, lba: Option<u64>| {
            n.verify()
                && n.seq == desc.seq
                && n.vol() == desc.vol()
                && n.kind == desc.kind
                && n.lap() == lap
                && Some(n.lba()) == lba
        };
        if slot > 0 {
            let prev = self.desc(slot - 1);
            if prev.state() == SlotState::Placed && same_write(prev, desc.lba().checked_sub(1)) {
                return true;
            }
        }
        if slot + 1 < self.slots {
            let next = self.desc(slot + 1);
            if next.state() != SlotState::Free && same_write(next, desc.lba().checked_add(1)) {
                return true;
            }
        }
        false
    }

    /// Releases a `Placed` run like [`commit`](Self::commit); while a
    /// deferred attach's writes are pending, the release joins them (the
    /// slots stay `Placed` in the memory until
    /// [`apply_pending_writes`](Self::apply_pending_writes)).
    pub fn release_after_attach(&mut self, ring: RingRef, blocks: u32) -> Result<(), ArenaError> {
        if !matches!(self.pending, Some(Pending::Attach { .. })) {
            return self.commit(ring, blocks);
        }
        self.check_run(ring, blocks)?;
        self.check_states(ring, blocks, &[SlotState::Placed])?;
        if let Some(Pending::Attach { rewrite, .. }) = &mut self.pending {
            for i in 0..blocks {
                rewrite.push((ring.slot + i, SlotState::Committed, ring.lap));
            }
        }
        Ok(())
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
        // Nothing is handed out on a table whose attach or creation is not
        // written yet.
        if self.pending.is_some() {
            return Err(ArenaError::Pending);
        }
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
            desc.kind = SlotKind::Data.as_u32();
            self.write_word(slot, SlotState::Filled, ring.lap);
        }
        Ok(())
    }

    /// Records a discard in a reserved run of one slot: the slot's block
    /// becomes a [`HoleRecordBlock`] for `blocks` blocks from `lba` of
    /// `vol`, written by request `seq`, and the slot becomes `Filled` with
    /// the hole kind. The slot is placed, committed and re-attached like a
    /// data slot, so an acknowledged discard survives a daemon crash.
    pub fn fill_hole(
        &mut self,
        ring: RingRef,
        vol: u32,
        lba: u64,
        blocks: u32,
        seq: u64,
    ) -> Result<(), ArenaError> {
        self.check_run(ring, 1)?;
        if blocks == 0 || vol > MAX_VOL || lba.saturating_add(u64::from(blocks)) > MAX_LBA + 1 {
            return Err(ArenaError::BadAddress { vol, lba });
        }
        self.check_states(ring, 1, &[SlotState::Free])?;
        let image = HoleRecordBlock::image(vol, lba, blocks);
        let crc = crc32c(&image);
        self.run_mut(ring, 1)?.copy_from_slice(&image);
        let desc = self.desc_mut(ring.slot);
        desc.addr = SlotDesc::pack_addr(vol, lba);
        desc.seq = seq;
        desc.crc32c = crc;
        desc.kind = SlotKind::Hole.as_u32();
        self.write_word(ring.slot, SlotState::Filled, ring.lap);
        Ok(())
    }

    /// The hole record a `Filled` or `Placed` hole slot carries.
    pub fn hole_record(&self, ring: RingRef) -> Result<HoleRecordBlock, ArenaError> {
        self.check_data(ring, 1)?;
        let desc = *self.desc(ring.slot);
        if !desc.is_hole() {
            return Err(ArenaError::BadTransition {
                slot: ring.slot,
                state: desc.state(),
            });
        }
        HoleRecordBlock::decode(self.data_of(ring.slot), desc.vol(), desc.lba())
            .ok_or(ArenaError::DataCrc { slot: ring.slot })
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
    /// Writes still pending are dropped: the memory is as the deferred
    /// attach or creation found it.
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

/// The ring position slot `slot` takes in a window that ends at `head`: the
/// highest position with that slot index at or below `head` (`at_head`) or
/// below it, so within one ring of it. A slot the first lap has not reached
/// yet keeps its own index, beyond the head. What the re-attach judges a
/// slot whose word does not verify by, since the lap such a word claims is
/// not to be trusted (the Step 6 review's D10 and R6).
fn implied_pos(slot: u32, slots: u64, head: u64, at_head: bool) -> u64 {
    let slot = u64::from(slot);
    let top = if at_head {
        head
    } else {
        head.saturating_sub(1)
    };
    if top < slot {
        return slot;
    }
    top - (top - slot) % slots.max(1)
}

/// The lap of ring position `pos`.
fn lap_of(pos: u64, slots: u64) -> u32 {
    u32::try_from(pos / slots.max(1)).map_or(MAX_LAP, |lap| lap.min(MAX_LAP))
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
    assert!(offset_of!(SlotDesc, kind) == 20);
    assert!(size_of::<HoleRecordBlock>() == HOLE_RECORD_SIZE);
    assert!(align_of::<HoleRecordBlock>() == 8);
    assert!(SlotKind::Data.as_u32() == 0 && SlotKind::Hole.as_u32() == 1);
    assert!(SlotKind::from_u32(2).is_none());
    assert!(offset_of!(SlotDesc, word) == SLOT_WORD_OFFSET);
    assert!(SlotState::Free.as_u8() == 0 && SlotState::Committed.as_u8() == 3);
    assert!(matches!(SlotState::from_bits(2), SlotState::Placed));
    assert!(matches!(SlotState::from_bits(7), SlotState::Committed));
    assert!(MAX_LAP == 0x3FFF_FFFF);
};
