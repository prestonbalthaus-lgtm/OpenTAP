//! The superblock ring, the volume table and the intent slots.
//!
//! # The ring (F7)
//! Four slots, `A0`, `A1`, `B0` and `B1`, hold commit records. A slot is one
//! [`SuperblockSlot`]: the 4 KiB [`Superblock`] page followed by the 16 KiB
//! [`VolTable`], 20 KiB written whole with FUA after the CP's data and
//! metadata have been flushed (the "empty flush, wait, FUA record" commit of
//! section 1.3). On a parity pool the slot is padded to one full stripe (R26).
//!
//! Commit `N` writes `A(N mod 2)` and then `B(N mod 2)` ([`SbSlot::for_commit`]),
//! so commit `N - 1` stays intact in the other parity pair through a torn
//! write, and one copy survives the loss of one location. The slot index a
//! page records must therefore have the parity of its `cp_seq`;
//! [`Superblock::verify`] enforces it, which also catches a misdirected
//! ring write.
//!
//! A reader reads all four slots and hands them to [`SuperblockSlot::rank`]
//! with the location each came from. The rule is:
//! - a slot carrying this crate's magic and a **higher major** is a hard
//!   error ([`FormatError::NewerFormat`]), whatever aggregate it names: the
//!   device was written by a newer release and must not be mounted by an
//!   older one, because the next commit would overwrite the newer slots.
//!   This is why mkfs writes all four slots (the unused pair zeroed): no
//!   slot of an earlier life of the disk survives, and a newer release's
//!   leftovers are an operator decision (`otapctl format upgrade`, or a
//!   fresh mkfs), never a silent fallback;
//! - a slot counts only if the whole slot verifies (page and volume table),
//!   belongs to the aggregate asked for and sits at the location its `slot`
//!   field names;
//! - the surviving slots are ordered by `(owner_epoch, cp_seq, cut_seq)`,
//!   newest first, and the `A` copy wins a tie, because `A` is written before
//!   `B` and is therefore the newer one when the two differ.
//!
//! The engine then verifies the map roots and the summary pages of the
//! selected CP before trusting it (the section 7.1 invariant), and rewinds to
//! the next ranked slot when they fail. Two rules on the writer follow from
//! this ranking: a commit is rebuilt from [`Superblock::new`] rather than
//! carried forward from a page that was read (so a page from a later minor
//! never leaks its `ver` or reserved bytes into a new commit), and a
//! `cp_seq` whose slot still verifies is never re-issued without first
//! overwriting that slot.
//!
//! # Volume table (F1)
//! Per-volume commit-time state (`acked_bytes`, `rollback_gen`, the nvfail
//! cause) lives in the [`VolTable`] of the same slot, one [`VolState`] per
//! volume index, covered by [`Superblock::vol_table_crc`]. The page's
//! `write_active` and `nvfail` bitsets and the table must agree:
//! [`SuperblockSlot::verify`] checks that an nvfail bit is set exactly when
//! the record carries a cause, that only present volumes are active or in
//! nvfail, that records at or above `vol_count` are absent, and that
//! [`Superblock::acked_bytes`] is the sum of the records.
//!
//! # Intent slots
//! `write_active` changes between commits, and superblock slots are for
//! commits only. The idle-to-active transition of a volume writes an
//! [`IntentSlot`] (two slots, alternating on `intent_seq` parity) before the
//! first acknowledgement. Boot takes the union of the selected superblock's
//! `write_active` and every intent slot of the same aggregate whose
//! `intent_seq` is above the superblock's `intent_seq_seen`
//! ([`boot_write_active`]); an intent slot from a newer major is a hard error
//! for the same reason as a newer superblock.

use core::mem::{align_of, offset_of, size_of};

use zerocopy::{FromBytes, FromZeros, Immutable, IntoBytes, KnownLayout};

use crate::checksum::crc32c;
use crate::constants::{
    BLOCK_SIZE, DURABLE_LOG_HDR_SIZE, FORMAT_MAJOR, FORMAT_VERSION, INBOUND_STATE_SIZE,
    INTENT_CRC_OFFSET, INTENT_MAGIC, INTENT_RESERVED, INTENT_SLOTS, INTENT_SLOT_SIZE, MAX_CP_SEQ,
    MAX_GROUPS, MAX_STRIPE_BYTES, MAX_VOLS, SUPERBLOCK_CRC_OFFSET, SUPERBLOCK_MAGIC,
    SUPERBLOCK_RESERVED, SUPERBLOCK_SIZE, SUPERBLOCK_SLOTS, SUPERBLOCK_SLOT_SIZE, VOL_STATE_SIZE,
    VOL_TABLE_SIZE,
};
use crate::types::{
    bad_field, check_crc, check_magic, check_major, copy_out, major_of, minor_of, view,
    FormatError, RootRef, Uuid, VolBitSet,
};

/// The run state the last commit recorded.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[repr(u8)]
pub enum RunState {
    /// The aggregate was serving writes when this commit was written. A boot
    /// that finds this state is a dirty boot and evaluates nvfail.
    Running = 1,
    /// The final CP of a clean shutdown (R57, R58). A clean shutdown never
    /// trips nvfail.
    CleanShutdown = 2,
}

impl RunState {
    /// Decodes the on-disk byte.
    #[must_use]
    pub const fn from_u8(value: u8) -> Option<Self> {
        match value {
            1 => Some(Self::Running),
            2 => Some(Self::CleanShutdown),
            _ => None,
        }
    }

    /// The on-disk byte.
    #[must_use]
    pub const fn as_u8(self) -> u8 {
        self as u8
    }
}

/// Which copy of the ring a superblock lives in.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SbLocation {
    /// The `A` location, at the start of the aggregate, written first.
    A,
    /// The `B` location, at the end of the aggregate, written second.
    B,
}

/// A slot of the superblock ring.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
#[repr(u8)]
pub enum SbSlot {
    /// Location `A`, parity 0.
    A0 = 0,
    /// Location `A`, parity 1.
    A1 = 1,
    /// Location `B`, parity 0.
    B0 = 2,
    /// Location `B`, parity 1.
    B1 = 3,
}

impl SbSlot {
    /// Every slot, in index order.
    pub const ALL: [Self; SUPERBLOCK_SLOTS] = [Self::A0, Self::A1, Self::B0, Self::B1];

    /// Decodes a slot index.
    #[must_use]
    pub const fn from_index(index: u8) -> Option<Self> {
        match index {
            0 => Some(Self::A0),
            1 => Some(Self::A1),
            2 => Some(Self::B0),
            3 => Some(Self::B1),
            _ => None,
        }
    }

    /// The slot index, 0 to 3.
    #[must_use]
    pub const fn index(self) -> u8 {
        self as u8
    }

    /// The location of the slot.
    #[must_use]
    pub const fn location(self) -> SbLocation {
        match self {
            Self::A0 | Self::A1 => SbLocation::A,
            Self::B0 | Self::B1 => SbLocation::B,
        }
    }

    /// The parity of the slot: the commits it receives have `cp_seq` of this parity.
    #[must_use]
    pub const fn parity(self) -> u8 {
        self.index() & 1
    }

    /// The two slots commit `cp_seq` is written to, in write order (F7).
    #[must_use]
    pub const fn for_commit(cp_seq: u64) -> [Self; 2] {
        if cp_seq & 1 == 0 {
            [Self::A0, Self::B0]
        } else {
            [Self::A1, Self::B1]
        }
    }
}

/// Why a volume is in nvfail.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[repr(u8)]
pub enum NvCause {
    /// Not in nvfail.
    None = 0,
    /// A boot found `run_state == Running` with the volume write-active.
    DirtyBoot = 1,
    /// The selected CP's summaries failed verification and the volume was rewound.
    TornCp = 2,
    /// The volume was promoted on a replica (R29, R40).
    Promote = 3,
}

impl NvCause {
    /// Decodes the on-disk byte.
    #[must_use]
    pub const fn from_u8(value: u8) -> Option<Self> {
        match value {
            0 => Some(Self::None),
            1 => Some(Self::DirtyBoot),
            2 => Some(Self::TornCp),
            3 => Some(Self::Promote),
            _ => None,
        }
    }

    /// The on-disk byte.
    #[must_use]
    pub const fn as_u8(self) -> u8 {
        self as u8
    }
}

/// `VolState::flags` bit: the volume index is allocated.
pub const VOL_PRESENT: u8 = 1 << 0;
/// `VolState::flags` bit (format 2.3, F18): the volume is offline, taken
/// down by the operator; the engine answers nothing on it until it is
/// brought online, and a rollback, a shrink or a deletion is accepted only
/// in this state.
pub const VOL_OFFLINE: u8 = 1 << 1;

/// Per-volume state recorded at commit time; one per volume index in the
/// [`VolTable`].
#[derive(
    FromBytes, IntoBytes, Immutable, KnownLayout, Clone, Copy, PartialEq, Eq, Hash, Default, Debug,
)]
#[repr(C)]
pub struct VolState {
    /// Bytes acknowledged to clients and not yet in this commit (the nvfail
    /// window the alert reports, R40).
    pub acked_bytes: u64,
    /// Bumped on every rollback; folded into the export `fsid` and the LUN
    /// serial generation.
    pub rollback_gen: u32,
    /// An [`NvCause`] byte.
    pub nvfail_cause: u8,
    /// [`VOL_PRESENT`] and reserved bits.
    pub flags: u8,
    /// Reserved, zero.
    pub rsvd: u16,
}

impl VolState {
    /// The record of an unallocated index.
    pub const ABSENT: Self = Self {
        acked_bytes: 0,
        rollback_gen: 0,
        nvfail_cause: 0,
        flags: 0,
        rsvd: 0,
    };

    /// `true` when the volume index is allocated.
    #[must_use]
    pub const fn is_present(&self) -> bool {
        self.flags & VOL_PRESENT != 0
    }

    /// `true` when the volume is present and offline (format 2.3).
    #[must_use]
    pub const fn is_offline(&self) -> bool {
        self.is_present() && self.flags & VOL_OFFLINE != 0
    }

    /// The decoded nvfail cause.
    #[must_use]
    pub const fn nvfail_cause(&self) -> Option<NvCause> {
        NvCause::from_u8(self.nvfail_cause)
    }
}

/// The per-volume table that follows the superblock page in a slot.
#[derive(FromBytes, IntoBytes, Immutable, KnownLayout, Clone, Copy, PartialEq, Eq, Hash, Debug)]
#[repr(C, align(4096))]
pub struct VolTable {
    /// One record per volume index.
    pub vols: [VolState; MAX_VOLS],
}

impl VolTable {
    /// An all-zero table: no volume present.
    #[must_use]
    pub fn zeroed() -> Self {
        Self::new_zeroed()
    }

    /// CRC-32C over the whole table, as stored in [`Superblock::vol_table_crc`].
    #[must_use]
    pub fn crc32c(&self) -> u32 {
        crc32c(self.as_bytes())
    }

    /// The record of volume `vol`, if the index is in range.
    #[must_use]
    pub fn get(&self, vol: u32) -> Option<&VolState> {
        self.vols.get(vol as usize)
    }

    /// The record of volume `vol`, if the index is in range.
    pub fn get_mut(&mut self, vol: u32) -> Option<&mut VolState> {
        self.vols.get_mut(vol as usize)
    }

    /// The sum of `acked_bytes` over the table, `None` on overflow.
    #[must_use]
    pub fn acked_bytes_total(&self) -> Option<u64> {
        self.vols
            .iter()
            .try_fold(0u64, |sum, row| sum.checked_add(row.acked_bytes))
    }
}

/// `InboundState::flags` bit: the slot describes a live inbound group.
pub const INBOUND_VALID: u16 = 1 << 0;

/// Replication receiver state per inbound consistency group (section 1.3,
/// "nvfail"). Reserved in v0.1; written by Phase B.
#[derive(
    FromBytes, IntoBytes, Immutable, KnownLayout, Clone, Copy, PartialEq, Eq, Hash, Default, Debug,
)]
#[repr(C)]
pub struct InboundState {
    /// The sender CP this group's volumes are at.
    pub applied_cp_seq: u64,
    /// The sender's owner epoch when that CP was cut.
    pub applied_owner_epoch: u64,
    /// The last CP acknowledged back to the sender as `Committed`.
    pub acked_cp_seq: u64,
    /// Receiver monotonic clock at the last heartbeat, nanoseconds.
    pub recv_mono_ns: u64,
    /// The sender's wall clock at `cut_mono` of the applied CP, nanoseconds.
    pub sender_cut_wall_ns: u64,
    /// The receiver `boot_id` the monotonic reading belongs to.
    pub recv_boot_id: Uuid,
    /// CP age at send, milliseconds.
    pub cp_age_at_send_ms: u32,
    /// Ship duration of the applied delta, milliseconds.
    pub ship_duration_ms: u32,
    /// The consistency group id.
    pub group: u16,
    /// [`INBOUND_VALID`] and reserved bits.
    pub flags: u16,
    /// Reserved, zero.
    pub rsvd: u32,
}

impl InboundState {
    /// `true` when the slot describes a live inbound group.
    #[must_use]
    pub const fn is_valid(&self) -> bool {
        self.flags & INBOUND_VALID != 0
    }
}

/// `DurableLogHdr::kind`: no durable log.
pub const DURABLE_LOG_NONE: u8 = 0;
/// `DurableLogHdr::kind`: a `LocalNvmeRing` (D2, the first v0.1.1 item).
pub const DURABLE_LOG_LOCAL_NVME_RING: u8 = 1;

/// The reservation for the optional durable log (R45, D2). `kind == 0` means
/// none; every other field is then zero.
#[derive(
    FromBytes, IntoBytes, Immutable, KnownLayout, Clone, Copy, PartialEq, Eq, Hash, Default, Debug,
)]
#[repr(C)]
pub struct DurableLogHdr {
    /// [`DURABLE_LOG_NONE`] or [`DURABLE_LOG_LOCAL_NVME_RING`].
    pub kind: u8,
    /// Reserved flag bits, zero.
    pub flags: u8,
    /// Reserved, zero.
    pub rsvd0: [u8; 6],
    /// Identity of the log device.
    pub dev_id: Uuid,
    /// First block of the ring on the log device.
    pub ring_pba: u64,
    /// Length of the ring in bytes.
    pub ring_bytes: u64,
    /// Ring head at commit.
    pub head: u64,
    /// Ring tail at commit.
    pub tail: u64,
    /// Reserved, zero.
    pub rsvd1: u64,
}

impl DurableLogHdr {
    /// No durable log.
    pub const NONE: Self = Self {
        kind: DURABLE_LOG_NONE,
        flags: 0,
        rsvd0: [0; 6],
        dev_id: Uuid::NIL,
        ring_pba: 0,
        ring_bytes: 0,
        head: 0,
        tail: 0,
        rsvd1: 0,
    };

    /// `true` when no durable log is configured.
    #[must_use]
    pub const fn is_none(&self) -> bool {
        self.kind == DURABLE_LOG_NONE
    }

    /// Rejects an undefined kind, and any non-zero byte when the kind is none.
    pub fn validate(&self) -> Result<(), FormatError> {
        match self.kind {
            DURABLE_LOG_NONE => {
                if *self != Self::NONE {
                    return Err(bad_field("durable_log", u64::from(self.kind)));
                }
                Ok(())
            }
            DURABLE_LOG_LOCAL_NVME_RING => Ok(()),
            other => Err(bad_field("durable_log.kind", u64::from(other))),
        }
    }
}

/// The superblock page: the commit record of one consistency point.
///
/// Exactly 4096 bytes, 4096-aligned, with the CRC-32C over bytes `0..4092`
/// in its last four bytes. Field order is frozen; new fields take bytes from
/// `reserved` and bump [`FORMAT_MINOR`](crate::FORMAT_MINOR). `magic` and
/// `ver` never move, in any major.
#[derive(FromBytes, IntoBytes, Immutable, KnownLayout, Clone, Copy, PartialEq, Eq, Hash, Debug)]
#[repr(C, align(4096))]
pub struct Superblock {
    /// [`SUPERBLOCK_MAGIC`].
    pub magic: u32,
    /// [`FORMAT_VERSION`].
    pub ver: u16,
    /// A [`RunState`] byte.
    pub run_state: u8,
    /// The [`SbSlot`] index this page was written to; its parity is the
    /// parity of `cp_seq`.
    pub slot: u8,
    /// The aggregate this superblock belongs to.
    pub agg_id: Uuid,
    /// The owner epoch of the node that wrote the commit (R29).
    pub owner_epoch: u64,
    /// The consistency point sequence number of this commit, at most
    /// [`MAX_CP_SEQ`] (F5).
    pub cp_seq: u64,
    /// The generation-cut counter at this commit (F6).
    pub cut_seq: u64,
    /// The random per-CP identity that keys resume and partial discard (R25).
    pub cp_nonce: u64,
    /// The highest intent-slot sequence folded into `write_active`.
    pub intent_seq_seen: u64,
    /// The open stamp of the daemon instance that wrote the commit.
    pub opened_stamp: u64,
    /// Wall-clock time of the commit, nanoseconds since the Unix epoch.
    pub wall_ns: u64,
    /// Sum of [`VolState::acked_bytes`] over the volume table.
    pub acked_bytes: u64,
    /// Volume indices in use: records `0..vol_count` of the volume table may
    /// be present; the rest are absent.
    pub vol_count: u32,
    /// CRC-32C over the [`VolTable`] of the same slot.
    pub vol_table_crc: u32,
    /// Volumes active since their last idle period (section 1.3, "nvfail").
    pub write_active: VolBitSet,
    /// Volumes in nvfail; the cause is in the volume table.
    pub nvfail: VolBitSet,
    /// Segments freed by this CP, reusable after the next one (`[0]`) and the
    /// one after (`[1]`, `flush-opaque` backstores): the deferred-reuse lists.
    /// Reserved: the Step 4 engine keeps the deferral in memory (a boot
    /// recomputes it from the two slots' regions) and writes `NONE` here.
    pub freed_pending: [RootRef; 2],
    /// The per-volume map roots table.
    pub map_roots: RootRef,
    /// The segment table.
    pub seg_table: RootRef,
    /// The relocation index.
    pub reloc_index: RootRef,
    /// The head of the CP log.
    pub cp_log_head: RootRef,
    /// The snapshot table.
    pub snap_table: RootRef,
    /// The `ClusterState` snapshot region (R59).
    pub cluster_state: RootRef,
    /// The durable-log reservation.
    pub durable_log: DurableLogHdr,
    /// Inbound replication state per consistency group.
    pub inbound: [InboundState; MAX_GROUPS],
    /// Format 2.1: the open data segment at this commit (0 for none), so a
    /// boot appends after the last stripe a committed CP wrote.
    pub open_data_pseg: u64,
    /// Format 2.1: the next stripe index of the open data segment.
    pub open_data_idx: u32,
    /// Reserved, zero.
    pub rsvd_open: u32,
    /// Format 2.2: the stripe the pool was laid out with, in bytes (F15).
    /// A boot under a profile whose stripe differs is refused.
    pub geom_stripe_bytes: u32,
    /// Format 2.2: the segment the pool was laid out with, in bytes.
    pub geom_segment_bytes: u32,
    /// Format 2.2: the device capacity the pool was laid out over, in
    /// bytes.
    pub geom_capacity_bytes: u64,
    /// Format 2.3: the name catalogue region (F16), or none. Carried
    /// forward unchanged by a commit that changed no name.
    pub catalog: RootRef,
    /// Format 2.3: the list of data stripes this commit wrote (F17), or
    /// none when it wrote no data stripe. What the boot verifies by summary
    /// before it trusts the commit.
    pub cp_stripes: RootRef,
    /// Reserved, zero.
    pub reserved: [u8; SUPERBLOCK_RESERVED],
    /// CRC-32C over bytes `0..4092`.
    pub crc32c: u32,
}

impl Superblock {
    /// A zeroed page carrying the magic, the version, `Running`, the slot and
    /// the parity-matching `cp_seq`. Every commit starts here; a page that
    /// was read is never carried forward.
    #[must_use]
    pub fn new(agg_id: Uuid, cp_seq: u64, location: SbLocation) -> Self {
        let mut sb = Self::new_zeroed();
        sb.magic = SUPERBLOCK_MAGIC;
        sb.ver = FORMAT_VERSION;
        sb.run_state = RunState::Running.as_u8();
        sb.slot = Self::slot_for(cp_seq, location).index();
        sb.agg_id = agg_id;
        sb.cp_seq = cp_seq;
        sb
    }

    /// Records the layout's geometry (format 2.2, F15): every commit of a
    /// pool carries the stripe, the segment and the capacity it was laid
    /// out with, so a boot can refuse a profile that differs.
    pub fn set_geometry(&mut self, stripe_bytes: u32, segment_bytes: u32, capacity_bytes: u64) {
        self.geom_stripe_bytes = stripe_bytes;
        self.geom_segment_bytes = segment_bytes;
        self.geom_capacity_bytes = capacity_bytes;
    }

    /// The recorded geometry as `(stripe, segment, capacity)` in bytes, or
    /// `None` on a page written before format 2.2.
    #[must_use]
    pub const fn geometry(&self) -> Option<(u32, u32, u64)> {
        let recorded = self.geom_stripe_bytes != 0
            || self.geom_segment_bytes != 0
            || self.geom_capacity_bytes != 0;
        if minor_of(self.ver) >= 2 && recorded {
            Some((
                self.geom_stripe_bytes,
                self.geom_segment_bytes,
                self.geom_capacity_bytes,
            ))
        } else {
            None
        }
    }
    /// The slot commit `cp_seq` occupies at `location` (F7).
    #[must_use]
    pub const fn slot_for(cp_seq: u64, location: SbLocation) -> SbSlot {
        let [a, b] = SbSlot::for_commit(cp_seq);
        match location {
            SbLocation::A => a,
            SbLocation::B => b,
        }
    }

    /// The CRC-32C over every byte before the `crc32c` field.
    #[must_use]
    pub fn compute_crc(&self) -> u32 {
        crc32c(&self.as_bytes()[..SUPERBLOCK_CRC_OFFSET])
    }

    /// Stores the CRC. Call after the last field change and before writing.
    pub fn seal(&mut self) {
        self.crc32c = self.compute_crc();
    }

    /// Checks magic, major version, CRC, every enumerated field, the slot
    /// parity rule, the CP bound and the bitsets against `vol_count`.
    pub fn verify(&self) -> Result<(), FormatError> {
        check_magic(self.magic, SUPERBLOCK_MAGIC)?;
        check_major(self.ver, FORMAT_MAJOR)?;
        check_crc("superblock page", self.compute_crc(), self.crc32c)?;
        if RunState::from_u8(self.run_state).is_none() {
            return Err(bad_field("run_state", u64::from(self.run_state)));
        }
        let Some(slot) = SbSlot::from_index(self.slot) else {
            return Err(bad_field("slot", u64::from(self.slot)));
        };
        if self.cp_seq > MAX_CP_SEQ {
            return Err(FormatError::OutOfRange {
                field: "cp_seq",
                value: self.cp_seq,
                max: MAX_CP_SEQ,
            });
        }
        if u64::from(slot.parity()) != self.cp_seq & 1 {
            return Err(bad_field("slot", u64::from(self.slot)));
        }
        if self.vol_count as usize > MAX_VOLS {
            return Err(FormatError::OutOfRange {
                field: "vol_count",
                value: u64::from(self.vol_count),
                max: MAX_VOLS as u64,
            });
        }
        if let Some(v) = self.write_active.highest() {
            if v >= self.vol_count {
                return Err(bad_field("write_active", u64::from(v)));
            }
        }
        if let Some(v) = self.nvfail.highest() {
            if v >= self.vol_count {
                return Err(bad_field("nvfail", u64::from(v)));
            }
        }
        if self.open_data_pseg == 0 && self.open_data_idx != 0 {
            return Err(bad_field("open_data_idx", u64::from(self.open_data_idx)));
        }
        let recorded = self.geom_stripe_bytes != 0
            || self.geom_segment_bytes != 0
            || self.geom_capacity_bytes != 0;
        if minor_of(self.ver) >= 2 && recorded {
            // Format 2.2: a recorded geometry is whole and consistent (an
            // engine commit always records one; a page without one is
            // refused by the boot).
            let stripe = self.geom_stripe_bytes;
            let segment = self.geom_segment_bytes;
            if stripe == 0
                || !(stripe as usize).is_multiple_of(BLOCK_SIZE)
                || stripe as usize > MAX_STRIPE_BYTES
            {
                return Err(bad_field("geom_stripe_bytes", u64::from(stripe)));
            }
            if segment == 0 || !segment.is_multiple_of(stripe) {
                return Err(bad_field("geom_segment_bytes", u64::from(segment)));
            }
            if self.geom_capacity_bytes < u64::from(segment) {
                return Err(bad_field("geom_capacity_bytes", self.geom_capacity_bytes));
            }
        }
        self.durable_log.validate()
    }

    /// A verified zero-copy view over a 4096-byte, 4096-aligned buffer. The
    /// page alone does not prove a commit: use [`SuperblockSlot::parse`] for
    /// that.
    pub fn parse(bytes: &[u8]) -> Result<&Self, FormatError> {
        let sb: &Self = view(bytes)?;
        sb.verify()?;
        Ok(sb)
    }

    /// A verified copy out of a 4096-byte buffer of any alignment.
    pub fn read(bytes: &[u8]) -> Result<Self, FormatError> {
        let sb: Self = copy_out(bytes)?;
        sb.verify()?;
        Ok(sb)
    }

    /// `true` when the page carries this crate's magic and a higher major:
    /// the aggregate was written by a newer release.
    #[must_use]
    pub const fn is_newer_format(&self) -> bool {
        self.magic == SUPERBLOCK_MAGIC && major_of(self.ver) > FORMAT_MAJOR
    }

    /// The decoded run state.
    #[must_use]
    pub const fn run_state(&self) -> Option<RunState> {
        RunState::from_u8(self.run_state)
    }

    /// Sets the run state.
    pub fn set_run_state(&mut self, state: RunState) {
        self.run_state = state.as_u8();
    }

    /// `true` when the commit is the final CP of a clean shutdown.
    #[must_use]
    pub const fn is_clean_shutdown(&self) -> bool {
        matches!(self.run_state(), Some(RunState::CleanShutdown))
    }

    /// The decoded slot.
    #[must_use]
    pub const fn sb_slot(&self) -> Option<SbSlot> {
        SbSlot::from_index(self.slot)
    }

    /// The ordering key of the commit: `(owner_epoch, cp_seq)`.
    #[must_use]
    pub const fn commit_key(&self) -> (u64, u64) {
        (self.owner_epoch, self.cp_seq)
    }
}

/// One whole slot of the ring: the page and its volume table, the unit
/// written with FUA and the unit a reader trusts.
#[derive(FromBytes, IntoBytes, Immutable, KnownLayout, Clone, Copy, PartialEq, Eq, Hash, Debug)]
#[repr(C, align(4096))]
pub struct SuperblockSlot {
    /// The superblock page.
    pub sb: Superblock,
    /// The volume table, covered by `sb.vol_table_crc`.
    pub vols: VolTable,
}

impl SuperblockSlot {
    /// A new slot for commit `cp_seq` at `location`, with an empty volume table.
    #[must_use]
    pub fn new(agg_id: Uuid, cp_seq: u64, location: SbLocation) -> Self {
        Self {
            sb: Superblock::new(agg_id, cp_seq, location),
            vols: VolTable::zeroed(),
        }
    }

    /// Stores the aggregate `acked_bytes`, the volume-table CRC and the page
    /// CRC. Call after the last field change and before writing. Fails only
    /// when the table's `acked_bytes` overflow a `u64`.
    pub fn seal(&mut self) -> Result<(), FormatError> {
        self.sb.acked_bytes = self
            .vols
            .acked_bytes_total()
            .ok_or(bad_field("acked_bytes", u64::MAX))?;
        self.sb.vol_table_crc = self.vols.crc32c();
        self.sb.seal();
        Ok(())
    }

    /// Verifies the page, the volume table against the page's CRC, and the
    /// agreement between the two (see the module documentation).
    pub fn verify(&self) -> Result<(), FormatError> {
        self.sb.verify()?;
        check_crc("volume table", self.vols.crc32c(), self.sb.vol_table_crc)?;
        let vol_count = self.sb.vol_count as usize;
        let mut acked_sum: u64 = 0;
        for (i, row) in self.vols.vols.iter().enumerate() {
            let index = i as u64;
            let Some(cause) = row.nvfail_cause() else {
                return Err(bad_field("nvfail_cause", u64::from(row.nvfail_cause)));
            };
            if i >= vol_count && *row != VolState::ABSENT {
                return Err(bad_field("vol_count", index));
            }
            if !row.is_present() && (row.acked_bytes != 0 || cause != NvCause::None) {
                return Err(bad_field("vol_present", index));
            }
            let vol = i as u32;
            if self.sb.nvfail.get(vol) != (cause != NvCause::None) {
                return Err(bad_field("nvfail", index));
            }
            if self.sb.write_active.get(vol) && !row.is_present() {
                return Err(bad_field("write_active", index));
            }
            acked_sum = acked_sum
                .checked_add(row.acked_bytes)
                .ok_or(bad_field("acked_bytes", index))?;
        }
        if acked_sum != self.sb.acked_bytes {
            return Err(bad_field("acked_bytes", self.sb.acked_bytes));
        }
        Ok(())
    }

    /// A verified zero-copy view over a 20480-byte, 4096-aligned buffer.
    pub fn parse(bytes: &[u8]) -> Result<&Self, FormatError> {
        let slot: &Self = view(bytes)?;
        slot.verify()?;
        Ok(slot)
    }

    /// A verified copy out of a 20480-byte buffer of any alignment.
    pub fn read(bytes: &[u8]) -> Result<Self, FormatError> {
        let slot: Self = copy_out(bytes)?;
        slot.verify()?;
        Ok(slot)
    }

    /// Ranks the ring's slots for aggregate `agg_id`, newest first, given each
    /// slot and the location it was read from. See the module documentation
    /// for the rule. Fails with [`FormatError::NewerFormat`] when any slot
    /// was written by a newer major, and with [`FormatError::NoValidSlot`]
    /// when no slot verifies for this aggregate at its own location.
    pub fn rank<'a, I>(agg_id: &Uuid, candidates: I) -> Result<Vec<&'a Self>, FormatError>
    where
        I: IntoIterator<Item = (SbSlot, &'a Self)>,
    {
        // (owner_epoch, cp_seq, cut_seq, is the A copy): the ranking key.
        type RankKey = (u64, u64, u64, bool);
        let mut ranked: Vec<(RankKey, &'a Self)> = Vec::new();
        for (location, slot) in candidates {
            let sb = &slot.sb;
            if sb.is_newer_format() {
                return Err(FormatError::NewerFormat { found: sb.ver });
            }
            if slot.verify().is_err() || sb.agg_id != *agg_id || sb.slot != location.index() {
                continue;
            }
            let a_copy = matches!(location.location(), SbLocation::A);
            ranked.push(((sb.owner_epoch, sb.cp_seq, sb.cut_seq, a_copy), slot));
        }
        if ranked.is_empty() {
            return Err(FormatError::NoValidSlot);
        }
        ranked.sort_by_key(|(key, _)| core::cmp::Reverse(*key));
        Ok(ranked.into_iter().map(|(_, slot)| slot).collect())
    }

    /// The newest verified slot of the ring: the first of [`rank`](Self::rank).
    pub fn select<'a, I>(agg_id: &Uuid, candidates: I) -> Result<&'a Self, FormatError>
    where
        I: IntoIterator<Item = (SbSlot, &'a Self)>,
    {
        Self::rank(agg_id, candidates).map(|ranked| ranked[0])
    }
}

/// An intent slot: the write-active set published between commits.
#[derive(FromBytes, IntoBytes, Immutable, KnownLayout, Clone, Copy, PartialEq, Eq, Hash, Debug)]
#[repr(C, align(4096))]
pub struct IntentSlot {
    /// [`INTENT_MAGIC`].
    pub magic: u32,
    /// [`FORMAT_VERSION`].
    pub ver: u16,
    /// The intent slot index, `intent_seq & 1`.
    pub slot: u8,
    /// Reserved, zero.
    pub rsvd0: u8,
    /// The aggregate this slot belongs to.
    pub agg_id: Uuid,
    /// Monotonic intent sequence; higher supersedes lower.
    pub intent_seq: u64,
    /// The CP sequence that was current when the intent was written.
    pub cp_seq: u64,
    /// Wall-clock time of the write, nanoseconds since the Unix epoch.
    pub wall_ns: u64,
    /// The volumes that were write-active when the intent was written.
    pub write_active: VolBitSet,
    /// Reserved, zero.
    pub reserved: [u8; INTENT_RESERVED],
    /// CRC-32C over bytes `0..4092`.
    pub crc32c: u32,
}

impl IntentSlot {
    /// A zeroed slot carrying the magic, the version, the aggregate,
    /// `intent_seq` and the slot index it implies.
    #[must_use]
    pub fn new(agg_id: Uuid, intent_seq: u64) -> Self {
        let mut intent = Self::new_zeroed();
        intent.magic = INTENT_MAGIC;
        intent.ver = FORMAT_VERSION;
        intent.slot = Self::slot_for(intent_seq);
        intent.agg_id = agg_id;
        intent.intent_seq = intent_seq;
        intent
    }

    /// The intent slot a given `intent_seq` is written to.
    #[must_use]
    pub const fn slot_for(intent_seq: u64) -> u8 {
        (intent_seq & 1) as u8
    }

    /// The CRC-32C over every byte before the `crc32c` field.
    #[must_use]
    pub fn compute_crc(&self) -> u32 {
        crc32c(&self.as_bytes()[..INTENT_CRC_OFFSET])
    }

    /// Stores the CRC. Call after the last field change and before writing.
    pub fn seal(&mut self) {
        self.crc32c = self.compute_crc();
    }

    /// Checks magic, major version, CRC, the slot index against `intent_seq`
    /// and the CP bound.
    pub fn verify(&self) -> Result<(), FormatError> {
        check_magic(self.magic, INTENT_MAGIC)?;
        check_major(self.ver, FORMAT_MAJOR)?;
        check_crc("intent slot", self.compute_crc(), self.crc32c)?;
        if self.slot as usize >= INTENT_SLOTS || self.slot != Self::slot_for(self.intent_seq) {
            return Err(bad_field("slot", u64::from(self.slot)));
        }
        if self.cp_seq > MAX_CP_SEQ {
            return Err(FormatError::OutOfRange {
                field: "cp_seq",
                value: self.cp_seq,
                max: MAX_CP_SEQ,
            });
        }
        Ok(())
    }

    /// A verified zero-copy view over a 4096-byte, 4096-aligned buffer.
    pub fn parse(bytes: &[u8]) -> Result<&Self, FormatError> {
        let intent: &Self = view(bytes)?;
        intent.verify()?;
        Ok(intent)
    }

    /// A verified copy out of a 4096-byte buffer of any alignment.
    pub fn read(bytes: &[u8]) -> Result<Self, FormatError> {
        let intent: Self = copy_out(bytes)?;
        intent.verify()?;
        Ok(intent)
    }

    /// `true` when the slot carries this crate's magic and a higher major.
    #[must_use]
    pub const fn is_newer_format(&self) -> bool {
        self.magic == INTENT_MAGIC && major_of(self.ver) > FORMAT_MAJOR
    }

    /// `true` when this intent is newer than what `sb` already folded in and
    /// belongs to the same aggregate.
    #[must_use]
    pub fn supersedes(&self, sb: &Superblock) -> bool {
        self.agg_id == sb.agg_id && self.intent_seq > sb.intent_seq_seen
    }
}

/// The write-active set a boot must assume: the selected superblock's
/// `write_active` united with every verified intent slot of the same
/// aggregate whose `intent_seq` is above the superblock's `intent_seq_seen`.
/// An intent slot written by a newer major is a hard error.
pub fn boot_write_active<'a, I>(sb: &Superblock, intents: I) -> Result<VolBitSet, FormatError>
where
    I: IntoIterator<Item = &'a IntentSlot>,
{
    let mut active = sb.write_active;
    for intent in intents {
        if intent.is_newer_format() {
            return Err(FormatError::NewerFormat { found: intent.ver });
        }
        if intent.verify().is_ok() && intent.supersedes(sb) {
            active.union_with(&intent.write_active);
        }
    }
    Ok(active)
}

const _: () = {
    assert!(RunState::Running.as_u8() == 1);
    assert!(RunState::CleanShutdown.as_u8() == 2);
    assert!(matches!(RunState::from_u8(1), Some(RunState::Running)));
    assert!(matches!(
        RunState::from_u8(2),
        Some(RunState::CleanShutdown)
    ));
    assert!(RunState::from_u8(0).is_none() && RunState::from_u8(3).is_none());

    assert!(SbSlot::A0.index() == 0);
    assert!(SbSlot::A1.index() == 1);
    assert!(SbSlot::B0.index() == 2);
    assert!(SbSlot::B1.index() == 3);
    assert!(matches!(SbSlot::from_index(0), Some(SbSlot::A0)));
    assert!(matches!(SbSlot::from_index(1), Some(SbSlot::A1)));
    assert!(matches!(SbSlot::from_index(2), Some(SbSlot::B0)));
    assert!(matches!(SbSlot::from_index(3), Some(SbSlot::B1)));
    assert!(SbSlot::from_index(4).is_none());

    assert!(NvCause::None.as_u8() == 0);
    assert!(NvCause::DirtyBoot.as_u8() == 1);
    assert!(NvCause::TornCp.as_u8() == 2);
    assert!(NvCause::Promote.as_u8() == 3);
    assert!(matches!(NvCause::from_u8(0), Some(NvCause::None)));
    assert!(matches!(NvCause::from_u8(1), Some(NvCause::DirtyBoot)));
    assert!(matches!(NvCause::from_u8(2), Some(NvCause::TornCp)));
    assert!(matches!(NvCause::from_u8(3), Some(NvCause::Promote)));
    assert!(NvCause::from_u8(4).is_none());

    assert!(size_of::<VolState>() == VOL_STATE_SIZE);
    assert!(align_of::<VolState>() == 8);
    assert!(offset_of!(VolState, acked_bytes) == 0);
    assert!(offset_of!(VolState, rollback_gen) == 8);
    assert!(offset_of!(VolState, nvfail_cause) == 12);
    assert!(offset_of!(VolState, flags) == 13);
    assert!(offset_of!(VolState, rsvd) == 14);

    assert!(size_of::<VolTable>() == VOL_TABLE_SIZE);
    assert!(align_of::<VolTable>() == 4096);

    assert!(size_of::<InboundState>() == INBOUND_STATE_SIZE);
    assert!(align_of::<InboundState>() == 8);
    assert!(offset_of!(InboundState, applied_cp_seq) == 0);
    assert!(offset_of!(InboundState, applied_owner_epoch) == 8);
    assert!(offset_of!(InboundState, acked_cp_seq) == 16);
    assert!(offset_of!(InboundState, recv_mono_ns) == 24);
    assert!(offset_of!(InboundState, sender_cut_wall_ns) == 32);
    assert!(offset_of!(InboundState, recv_boot_id) == 40);
    assert!(offset_of!(InboundState, cp_age_at_send_ms) == 56);
    assert!(offset_of!(InboundState, ship_duration_ms) == 60);
    assert!(offset_of!(InboundState, group) == 64);
    assert!(offset_of!(InboundState, flags) == 66);
    assert!(offset_of!(InboundState, rsvd) == 68);

    assert!(size_of::<DurableLogHdr>() == DURABLE_LOG_HDR_SIZE);
    assert!(align_of::<DurableLogHdr>() == 8);
    assert!(offset_of!(DurableLogHdr, kind) == 0);
    assert!(offset_of!(DurableLogHdr, flags) == 1);
    assert!(offset_of!(DurableLogHdr, rsvd0) == 2);
    assert!(offset_of!(DurableLogHdr, dev_id) == 8);
    assert!(offset_of!(DurableLogHdr, ring_pba) == 24);
    assert!(offset_of!(DurableLogHdr, ring_bytes) == 32);
    assert!(offset_of!(DurableLogHdr, head) == 40);
    assert!(offset_of!(DurableLogHdr, tail) == 48);
    assert!(offset_of!(DurableLogHdr, rsvd1) == 56);

    assert!(size_of::<Superblock>() == SUPERBLOCK_SIZE);
    assert!(align_of::<Superblock>() == 4096);
    assert!(offset_of!(Superblock, magic) == 0);
    assert!(offset_of!(Superblock, ver) == 4);
    assert!(offset_of!(Superblock, run_state) == 6);
    assert!(offset_of!(Superblock, slot) == 7);
    assert!(offset_of!(Superblock, agg_id) == 8);
    assert!(offset_of!(Superblock, owner_epoch) == 24);
    assert!(offset_of!(Superblock, cp_seq) == 32);
    assert!(offset_of!(Superblock, cut_seq) == 40);
    assert!(offset_of!(Superblock, cp_nonce) == 48);
    assert!(offset_of!(Superblock, intent_seq_seen) == 56);
    assert!(offset_of!(Superblock, opened_stamp) == 64);
    assert!(offset_of!(Superblock, wall_ns) == 72);
    assert!(offset_of!(Superblock, acked_bytes) == 80);
    assert!(offset_of!(Superblock, vol_count) == 88);
    assert!(offset_of!(Superblock, vol_table_crc) == 92);
    assert!(offset_of!(Superblock, write_active) == 96);
    assert!(offset_of!(Superblock, nvfail) == 224);
    assert!(offset_of!(Superblock, freed_pending) == 352);
    assert!(offset_of!(Superblock, map_roots) == 384);
    assert!(offset_of!(Superblock, seg_table) == 400);
    assert!(offset_of!(Superblock, reloc_index) == 416);
    assert!(offset_of!(Superblock, cp_log_head) == 432);
    assert!(offset_of!(Superblock, snap_table) == 448);
    assert!(offset_of!(Superblock, cluster_state) == 464);
    assert!(offset_of!(Superblock, durable_log) == 480);
    assert!(offset_of!(Superblock, inbound) == 544);
    assert!(offset_of!(Superblock, open_data_pseg) == 1696);
    assert!(offset_of!(Superblock, open_data_idx) == 1704);
    assert!(offset_of!(Superblock, rsvd_open) == 1708);
    assert!(offset_of!(Superblock, geom_stripe_bytes) == 1712);
    assert!(offset_of!(Superblock, geom_segment_bytes) == 1716);
    assert!(offset_of!(Superblock, geom_capacity_bytes) == 1720);
    assert!(offset_of!(Superblock, catalog) == 1728);
    assert!(offset_of!(Superblock, cp_stripes) == 1744);
    assert!(offset_of!(Superblock, reserved) == 1760);
    assert!(offset_of!(Superblock, crc32c) == SUPERBLOCK_CRC_OFFSET);

    assert!(size_of::<SuperblockSlot>() == SUPERBLOCK_SLOT_SIZE);
    assert!(align_of::<SuperblockSlot>() == 4096);
    assert!(offset_of!(SuperblockSlot, sb) == 0);
    assert!(offset_of!(SuperblockSlot, vols) == SUPERBLOCK_SIZE);

    assert!(size_of::<IntentSlot>() == INTENT_SLOT_SIZE);
    assert!(align_of::<IntentSlot>() == 4096);
    assert!(offset_of!(IntentSlot, magic) == 0);
    assert!(offset_of!(IntentSlot, ver) == 4);
    assert!(offset_of!(IntentSlot, slot) == 6);
    assert!(offset_of!(IntentSlot, rsvd0) == 7);
    assert!(offset_of!(IntentSlot, agg_id) == 8);
    assert!(offset_of!(IntentSlot, intent_seq) == 24);
    assert!(offset_of!(IntentSlot, cp_seq) == 32);
    assert!(offset_of!(IntentSlot, wall_ns) == 40);
    assert!(offset_of!(IntentSlot, write_active) == 48);
    assert!(offset_of!(IntentSlot, reserved) == 176);
    assert!(offset_of!(IntentSlot, crc32c) == INTENT_CRC_OFFSET);
};
