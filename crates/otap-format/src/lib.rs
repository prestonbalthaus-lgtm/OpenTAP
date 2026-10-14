//! `otap-format`: the OpenTAP on-disk format (v2) and the stream frame header (v1).
//!
//! This crate is the single owner of every byte layout OpenTAP writes to a
//! block device or a replication link. It contains no I/O, no policy and no
//! `unsafe`: every structure is a plain-old-data `#[repr(C)]` view with the
//! `zerocopy` traits derived, so engine crates cast page-aligned `O_DIRECT`
//! buffers to and from these types without copying or allocating, and the
//! only computation here is packing, unpacking, CRC-32C sealing and
//! verification.
//!
//! # Conventions (round 3, Part 7)
//! 1. Every structure is `#[repr(C)]` with no padding and derives
//!    [`FromBytes`], [`IntoBytes`], [`Immutable`] and [`KnownLayout`].
//!    Page-sized structures are also `align(4096)`, so a `&Superblock` is a
//!    valid direct-I/O buffer and a 4096-aligned buffer is a valid
//!    `&Superblock`. Slices of records (`&[StripeDescriptor]`, `&[MapEntry]`)
//!    are views too. Zero-copy views need an aligned buffer ([`Block4K`],
//!    [`Page16K`], arena or registered memory; not a `Vec<u8>`); the `read`
//!    paths copy and work anywhere.
//! 2. All integers are little-endian. The crate refuses to compile on a
//!    big-endian host rather than read a foreign byte order.
//! 3. Every byte of every structure is covered by a CRC-32C (the iSCSI
//!    polynomial). Fixed records carry the CRC in their last four bytes,
//!    computed over everything before it; headerless pages (map pages, index
//!    pages, the volume table) are covered by the CRC in their parent
//!    reference, and the parent's `verify`/`parse` checks it. Reserved fields
//!    are written as zero and are covered.
//! 4. `ver` is `major << 8 | minor`. On disk, a reader accepts any minor of
//!    its own major (a later minor may only give meaning to reserved bytes,
//!    which readers ignore) and rejects every other major; a *higher* major
//!    met in the superblock ring or an intent slot is a hard stop, never a
//!    fallback (F7). Enumerated fields must hold defined values. On the wire
//!    the header has no reserved bytes, so unknown kinds, codecs and flag
//!    bits are refused and new ones are negotiated in `Hello` first.
//! 5. Size and offset invariants, enum discriminants and their decoders are
//!    `const`-asserted in each module, so a layout change fails to compile
//!    (section 7.7, invariant 3).
//! 6. `parse` returns a verified view and `read` a verified copy. The raw
//!    zerocopy conversions (`ref_from_bytes`, `read_from_bytes`) are also
//!    available and verify nothing; a caller that uses them calls `verify`
//!    itself. The seal paths validate what `parse` would reject, so a
//!    builder cannot write an image its reader refuses.
//!
//! # Layouts
//!
//! | Structure | Bytes | Align | CRC | Module |
//! |---|---|---|---|---|
//! | [`Superblock`] | 4096 | 4096 | trailing | [`superblock`] |
//! | [`VolTable`] (`[VolState; 1024]`) | 16384 | 4096 | in `Superblock` | [`superblock`] |
//! | [`SuperblockSlot`] | 20480 | 4096 | both | [`superblock`] |
//! | [`IntentSlot`] | 4096 | 4096 | trailing | [`superblock`] |
//! | [`StripeHdr`] | 64 | 8 | `hdr_crc`, `summary_crc` | [`stripe`] |
//! | [`StripeDescriptor`] | 16 | 8 | block CRC inside | [`stripe`] |
//! | [`MapEntry`], [`PageIndexEntry`] | 16 | 8 | in parent | [`map`] |
//! | [`MapPage`], [`PageIndexPage`] | 16384 | 4096 | in parent | [`map`] |
//! | [`MapRoot`] | 64 | 8 | in table region | [`map`] |
//! | [`StreamHeader`] | 64 | 8 | `hdr_crc`, `payload_crc` | [`stream`] |
//! | [`HoleRecord`] | 16 | 8 | payload CRC | [`stream`] |
//! | [`SegEntry`] | 32 | 8 | trailing | [`records`] |
//! | [`SnapRecord`] | 64 | 8 | trailing | [`records`] |
//! | [`CpLogRec`] | 32 | 8 | trailing | [`records`] |
//! | [`CpLogPage`] (`CpLogPageHdr` + 126 records) | 4096 | 4096 | header + records | [`records`] |
//! | [`CpLogDirPage`] (`CpLogDirHdr` + 252 root references; 2.1) | 4096 | 4096 | header | [`records`] |
//! | [`SnapRootPage`] (2.1) | 4096 | 4096 | trailing | [`records`] |
//!
//! # Format rulings (F1 to F15)
//! These settle the points on which the Step-1 brief, the round-3 synthesis
//! (docs/05, section 7.1) and the 4 KiB page bound differ. They are
//! architect decisions, recorded so that a later change is deliberate.
//!
//! - **F1. The superblock page is exactly 4 KiB; per-volume state lives in a
//!   volume table inside the same slot.** [`Superblock`] holds identity,
//!   counters, the `write_active` and `nvfail` bitsets and the roots;
//!   [`VolTable`] holds `acked_bytes`, `rollback_gen` and the nvfail cause per
//!   volume and is covered by `Superblock::vol_table_crc`. A slot is the
//!   20 KiB [`SuperblockSlot`], written whole with FUA (one full stripe on
//!   parity pools, R26) and trusted only as a whole. This keeps the page
//!   layout fixed with `MAX_VOLS == 1024`, where the round-3 sketch's inline
//!   arrays would fit about 128 volumes in 4 KiB.
//! - **F2. Map entries use the round-3 packing `lseg:36 | off:16 | plen:12`**,
//!   not the round-2 `seg:28 | off:20 | plen:16` quoted in the brief. Logical
//!   segment numbers are allocated monotonically and never reused (R19 and the
//!   relocation table), so 28 bits are exhausted in about 200 days of
//!   full-rate ingest with 64 MiB segments; 36 bits last 136 years. `off:16`
//!   bounds a segment at 256 MiB. `plen` is the physical length in 512-byte
//!   sectors; v2 writes `1..=8`, and the width above that is the compression
//!   reservation (on disk every block still occupies a 4 KiB slot, so in v2
//!   compression can only save wire bytes). `lseg == 0` is reserved: an
//!   entry with `addr == 0` is a hole when its birth is non-zero and
//!   unmapped when it is zero.
//! - **F3. A stripe summary is `[StripeHdr][VolBitSet][StripeDescriptor; nblocks]`
//!   padded to a 4 KiB multiple**, `summary_crc` over everything after the
//!   header and `hdr_crc` over the header. The bitmap is derived from the
//!   descriptors by the writer and checked by the reader. The on-disk stripe
//!   image is the chunk payload byte for byte; the 64-byte stream frame
//!   belongs to the transport and is never stored (R25). A stripe is at most
//!   8 MiB ([`MAX_STRIPE_BYTES`]), which is also the frame payload bound.
//! - **F4. The per-descriptor 2-bit `kind` is the block kind, `Data` or
//!   `Hole`**; the stripe kind (`Ingest | Relocate | Applied | Merge`) is in
//!   the header once, because stripes are homogeneous in kind.
//! - **F5. CP sequence numbers are `u64` in headers and `u32` as `birth_cp` in
//!   16-byte entries.** Every header field that carries a CP is verified
//!   against [`MAX_CP_SEQ`] (`u32::MAX`); at one CP per second that is 136
//!   years. CP 0 is the mkfs commit, so a written block or punched hole has
//!   `birth_cp >= 1` and a zero birth means "never written".
//! - **F6. The superblock carries both `cut_seq` and `cp_nonce`.** `cut_seq`
//!   is the generation-cut counter the brief names; `cp_nonce` is the random
//!   per-CP identity that keys resume and partial discard (R25, section 1.3).
//! - **F7. Ring protocol.** The slots are `A0`, `A1`, `B0`, `B1`. Commit `N`
//!   writes `A(N mod 2)` then `B(N mod 2)`, so commit `N - 1` stays intact in
//!   the other pair through a torn write, and one copy survives the loss of
//!   one location; a page's `slot` has the parity of its `cp_seq`. A reader
//!   ranks whole verified slots of its aggregate, each at its own location,
//!   by `(owner_epoch, cp_seq, cut_seq)`, the `A` copy winning a tie; a slot
//!   from a newer major is a hard error ([`SuperblockSlot::rank`]).
//! - **F8. A volume index is a dense per-aggregate index below `MAX_VOLS`**
//!   (1024), the index into the bitsets and the volume table, and every
//!   descriptor, map root and hole record is verified against that bound.
//!   The 20-bit descriptor field is wider than the bound; raising `MAX_VOLS`
//!   is a format change, because the bitsets and the volume table are sized
//!   by it.
//! - **F9. Holes.** A hole descriptor's data slot is present and zero, so its
//!   CRC is [`ZERO_BLOCK_CRC32C`] and the data region is always
//!   `nblocks * 4 KiB`; a map hole or unmapped entry carries CRC 0.
//! - **F10. Narrowings against the 7.1 sketches**, none of which changes a
//!   size: `StripeHdr::nblocks` is `u16` (a stripe is at most 8 MiB);
//!   `PageIndexEntry` spends its reserved word on `page_crc` (the parent-CRC
//!   rule); headerless pages carry no `ver` because their parent does; and
//!   the frame header lives in this crate so the disk and wire layouts are
//!   frozen together, with `otap-stream` building on it.
//!
//! - **F11. The deferred records (Step 2).** [`SegEntry`] is 32 bytes and
//!   keyed by its position in the segment table, so it carries `phys` (the
//!   physical segment, or the relocation table once relocated) and the
//!   stripe geometry at birth in place of a redundant `lseg` field: the
//!   §7.1 sketch's `Physical(pseg) | Relocated(table_pba)` and `geom` are
//!   what the map needs to resolve an address, and the Step-2 brief's
//!   `lseg` is the index. [`SnapRecord`] is 64 bytes with the volume, the
//!   pinned map root, the deadlist, kind, origin and pin bits; snapshot
//!   names are control-plane metadata keyed by the uuid and are not on
//!   disk. [`CpLogRec`] is 32 bytes and carries the packed map address
//!   rather than a separate physical address and length, so replaying the
//!   journal is a copy into the map page; records live in 4 KiB
//!   [`CpLogPage`]s whose header names the aggregate, the CP, its nonce and
//!   the page's position. Physical segment 0 is reserved like logical
//!   segment 0. The v2.0 golden vectors were extended with these records on
//!   2026-10-13, before any release wrote the format, which is the only time
//!   a vector set may change without a minor bump.
//!
//! Format 2.1 (Step 4, 2026-10-13) adds three rulings for the persistent
//! map (`otap-map`); the 2.0 vectors stay in `tests/vectors/v2.0/` and are
//! still decoded, and `tests/vectors/v2.1/` holds the 2.1 set.
//!
//! - **F12. The CP log is a directory of delta regions.** The superblock's
//!   `cp_log_head` names a region of [`CpLogDirPage`]s ([`CpLogDir`]): the
//!   [`RootRef`]s of the delta regions (runs of [`CpLogPage`]s, one
//!   [`CpLogRec`] per block written or hole punched) of every consistency
//!   point since the last *fold*, in CP order, plus, when the writing CP is
//!   the fold, the map pages the fold wrote. A fold rewrites the map pages
//!   the deltas touched (copy on write) and the page indexes of the volumes
//!   concerned, so the map pages hold the state at the fold CP and a boot
//!   replays the deltas born after it; the fold CP's own records go into
//!   the pages it rewrites, so it writes no delta region. Every region is
//!   named exactly (block, length, CRC), as E14 requires.
//! - **F13. A snapshot pins a root through a [`SnapRootPage`].** The page a
//!   [`SnapRecord`]'s `map_root_pba` names carries the pinned [`MapRoot`],
//!   the snapshot's identity, the deadlist reference and its own CRC, so a
//!   pinned root is verified like every other structure. Snapshots are
//!   taken at fold CPs, where the root is complete.
//! - **F14. The table regions.** `map_roots` names one [`MapRoot`] per
//!   volume index up to `vol_count`, absent volumes all zero
//!   ([`map_roots_image`], [`parse_map_roots`]); `snap_table` names the
//!   [`SnapRecord`]s in ascending id order ([`snap_table_image`],
//!   [`parse_snap_table`]). Both are padded to whole pages and sealed by
//!   the superblock's root reference. A pool written by format 2.0 (whose
//!   `cp_log_head` held a whole-map dump, E14) is refused by the Step 4
//!   boot; none was ever released.
//!
//! Format 2.2 (Step 4 review, 2026-10-14) adds one ruling; the 2.0 and 2.1
//! vectors stay and are still decoded, and `tests/vectors/v2.2/` holds the
//! 2.2 set.
//!
//! - **F15. The superblock records the geometry.** Every commit carries the
//!   stripe, the segment and the capacity the pool was laid out with
//!   (`geom_stripe_bytes`, `geom_segment_bytes`, `geom_capacity_bytes`, in
//!   the former reserved area), verified whole and consistent on a 2.2
//!   page. A boot compares them with the profile it was given and refuses a
//!   difference before it writes anything: the slots `A1`, `B0` and `B1`
//!   and every segment move with the stripe, so a pool opened under another
//!   geometry would find a verified `A0` at block 0 and lay itself out anew
//!   over the old layout. Pools written by 2.1 (no geometry recorded) are
//!   refused like 2.0 pools; none was released.
//!
//! # Using the views
//! ```
//! use otap_format::{FromBytes, IntoBytes, SbLocation, SbSlot, SuperblockSlot, Uuid, VOL_PRESENT};
//!
//! // A 4096-aligned buffer of one slot: what goes to the device.
//! #[repr(C, align(4096))]
//! struct SlotBuf([u8; otap_format::SUPERBLOCK_SLOT_SIZE]);
//! let mut buf = SlotBuf([0; otap_format::SUPERBLOCK_SLOT_SIZE]);
//!
//! // Build commit 42 in place: no heap, no copy.
//! {
//!     let slot = SuperblockSlot::mut_from_bytes(&mut buf.0).unwrap();
//!     *slot = SuperblockSlot::new(Uuid::from_bytes([7; 16]), 42, SbLocation::A);
//!     slot.sb.owner_epoch = 1;
//!     slot.sb.vol_count = 1;
//!     slot.vols.vols[0].flags = VOL_PRESENT;
//!     slot.vols.vols[0].acked_bytes = 8192;
//!     slot.seal().unwrap();
//! }
//!
//! // Reading it back verifies both CRCs and the page/table agreement.
//! let slot = SuperblockSlot::parse(&buf.0).unwrap();
//! assert_eq!(slot.sb.cp_seq, 42);
//! assert_eq!(slot.sb.acked_bytes, 8192);
//! assert_eq!(slot.sb.sb_slot(), Some(SbSlot::A0));
//! assert_eq!(slot.as_bytes(), &buf.0[..]);
//! ```

#![forbid(unsafe_code)]
#![deny(missing_docs)]
#![warn(rust_2018_idioms, unreachable_pub)]

#[cfg(target_endian = "big")]
compile_error!("otap-format defines a little-endian format and supports little-endian hosts only");

pub mod checksum;
pub mod constants;
pub mod map;
pub mod records;
pub mod stream;
pub mod stripe;
pub mod superblock;
pub mod types;

pub use constants::*;
pub use map::{
    map_roots_image, parse_map_roots, MapEntry, MapPage, MapRoot, PageIndexEntry, PageIndexPage,
    MAP_LSEG_SHIFT, MAP_OFF_SHIFT,
};
pub use records::{
    parse_snap_table, snap_table_image, CpLogDir, CpLogDirHdr, CpLogDirPage, CpLogPage,
    CpLogPageHdr, CpLogRec, SegEntry, SegKind, SegState, SnapKind, SnapOrigin, SnapRecord,
    SnapRootPage, PIN_CLONE, PIN_MASK, PIN_REPLICATION, PIN_SCHEDULE, PIN_USER,
};
pub use stream::{Codec, FrameKind, HoleRecord, StreamHeader, FLAGS_CODEC_MASK};
pub use stripe::{
    derive_vol_bitmap, descriptor_offset, seal_summary_in_place, summary_bytes, write_summary,
    BlockKind, StripeDescriptor, StripeHdr, StripeId, StripeKind, StripeSummary, SummaryLayout,
    DESC_KIND_SHIFT, DESC_LBA_SHIFT, DESC_RSVD_MASK, DESC_VOL_SHIFT,
};
pub use superblock::{
    boot_write_active, DurableLogHdr, InboundState, IntentSlot, NvCause, RunState, SbLocation,
    SbSlot, Superblock, SuperblockSlot, VolState, VolTable, DURABLE_LOG_LOCAL_NVME_RING,
    DURABLE_LOG_NONE, INBOUND_VALID, VOL_PRESENT,
};
pub use types::{AggId, Block4K, FormatError, Page16K, RootRef, Uuid, VolBitSet};

pub use zerocopy::{FromBytes, FromZeros, Immutable, IntoBytes, KnownLayout};
