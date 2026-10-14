//! Step 4 QA (Opus-QA-Adversarial): geometry discovery and planning
//! attacked with the sysfs values real hosts produce and with absurd ones:
//! hypervisor disks, logical blocks the engine cannot address, md arrays
//! with a missing chunk, discards switched off by the administrator,
//! devices too large for the segment table, and a property over arbitrary
//! attribute trees.
//!
//! The tests that reproduced a defect are regression tests now (their doc
//! comments name the defect in the Step 4 log and say what was wrong and
//! what the engine does now). The others pin properties that hold.

use otap_cp::{DiscardPolicy, DiskLayout, FlushPolicy, Media};
use otap_format::{MAX_SEG_BYTES, MAX_STRIPE_BYTES};
use otap_geom::{
    discover, plan, plan_device, DeviceKind, GeomError, GeomSource, GeomWarning, MockSysfs,
    Overrides,
};
use proptest::prelude::*;

const GIB: u64 = 1 << 30;
const TIB: u64 = 1 << 40;
const PIB: u64 = 1 << 50;

/// A device of `size` bytes with the attributes every queue has.
fn device(size: u64, rotational: bool) -> MockSysfs {
    let mut m = MockSysfs::new();
    m.set("size", (size / 512).to_string())
        .set("queue/logical_block_size", "512")
        .set("queue/physical_block_size", "4096")
        .set("queue/minimum_io_size", "4096")
        .set("queue/optimal_io_size", "0")
        .set("queue/alignment_offset", "0")
        .set("queue/rotational", if rotational { "1" } else { "0" })
        .set("queue/discard_granularity", "0")
        .set("queue/discard_max_bytes", "0")
        .set("queue/write_cache", "write back")
        .set("queue/fua", "1")
        .set("queue/max_sectors_kb", "1280")
        .set("queue/max_hw_sectors_kb", "32767");
    m
}

// ---------------------------------------------------------------------------
// Defects
// ---------------------------------------------------------------------------

/// Found by the Step 4 review (defect 19, fixed): VMware and Hyper-V virtual disks are classified as hardware RAID
/// virtual drives.
///
/// `DeviceKind::Virtual` documents "a virtual disk of a hypervisor
/// (virtio, Xen, VMware, Hyper-V, VirtualBox)", but `discover` tests the
/// RAID markers before the hypervisor markers, and `RAID_VD_MARKERS`
/// contains `"VIRTUAL DISK"`: VMware's SCSI disk (`VMware` / `Virtual
/// disk`) and Hyper-V's (`Msft` / `Virtual Disk`) both match it. The plan
/// then treats them as flush-opaque controllers (the trust policy and its
/// operator warning) and turns discards off (`Never`) on SSD-backed
/// datastores. The right behaviour: a hypervisor vendor wins over the
/// generic "Virtual Disk" model string (test `VIRTUAL_MARKERS` first, or
/// drop the bare `VIRTUAL DISK` marker in favour of vendor-qualified
/// ones), so these classify as `Virtual`, honest about flushes.
#[test]
fn hypervisor_virtual_disks_are_virtual_not_hardware_raid() {
    for (vendor, model) in [
        ("VMware", "Virtual disk"),
        ("VMware,", "VMware Virtual S"),
        ("Msft", "Virtual Disk"),
    ] {
        let mut m = device(100 * GIB, false);
        m.set("device/vendor", vendor)
            .set("device/model", model)
            .set("queue/discard_granularity", "512")
            .set("queue/discard_max_bytes", "4294966784");
        let facts = discover(&m, "sda").unwrap();
        // Before the fix: `HwRaidVd` for the first and the third.
        assert_eq!(facts.kind, DeviceKind::Virtual, "{vendor} {model}");
        let g = plan(&facts, &Overrides::default()).unwrap();
        assert!(!g.flush_opaque, "{vendor} {model}");
        assert_eq!(g.discard, DiscardPolicy::WholeSegment, "{vendor} {model}");
    }
}

/// Found by the Step 4 review (defect 20, fixed): a logical block that does not divide 4 KiB is accepted.
///
/// `plan` refuses a logical block larger than 4 KiB or of zero bytes, but
/// takes any other value: 520 (a DIF-formatted SAS disk presented raw),
/// 1000 or 3072. Every engine I/O is 4 KiB at a 4 KiB-aligned offset, which
/// such a device cannot take under `O_DIRECT` (the length and offset must
/// be multiples of the logical block), so every write would fail at run
/// time with `EINVAL`. The right behaviour: refuse with `Unsupported`
/// unless the logical block is a power of two that divides 4096.
#[test]
fn a_logical_block_that_does_not_divide_4k_is_refused() {
    for lbs in [520u32, 1000, 3072, 4095] {
        let mut m = device(TIB, true);
        m.set("queue/logical_block_size", lbs.to_string())
            .set("queue/physical_block_size", lbs.to_string())
            .set("queue/minimum_io_size", lbs.to_string());
        let planned = plan_device(&m, "sda", &Overrides::default());
        // Before the fix: planned as if 4 KiB I/O were possible.
        assert!(
            matches!(planned, Err(GeomError::Unsupported { .. })),
            "a {lbs}-byte logical block: {planned:?}"
        );
    }
}

/// Found by the Step 4 review (defect 21, fixed): an md parity array whose chunk size is missing gets a default
/// stripe silently.
///
/// `md/raid_disks` is required, but `md/chunk_size` defaults to 0, and a
/// striped level with chunk 0 falls into the branch meant for raid1 and
/// linear ("no striping"): the 1 MiB default stripe is used with no
/// warning at all (`OptimalIoUnknown` is pushed only for hardware RAID
/// virtual drives). On a raid5 or raid6 the real stripe is chunk times data
/// disks, so every "full-stripe" write becomes a partial-stripe
/// read-modify-write, the write hole the full-stripe design exists to
/// avoid. The right behaviour: refuse (`Missing { attr: "md/chunk_size" }`)
/// for a striped level, or at least warn that the default stands.
#[test]
fn an_md_parity_array_without_a_chunk_size_is_refused() {
    for level in ["raid0", "raid5", "raid6"] {
        let mut m = device(20 * TIB, true);
        m.set("md/level", level).set("md/raid_disks", "6");
        let planned = plan_device(&m, "md0", &Overrides::default());
        let refused = matches!(planned, Err(GeomError::Missing { .. }));
        let warned = planned
            .as_ref()
            .is_ok_and(|g| g.warnings.contains(&GeomWarning::OptimalIoUnknown));
        // Before the fix: Ok, 1 MiB, `GeomSource::Default`, no warning.
        assert!(
            refused || warned,
            "{level} without md/chunk_size: {planned:?}"
        );
    }
}

/// Found by the Step 4 review (defect 22, fixed): flash whose discards the administrator switched off still gets
/// whole-segment discards.
///
/// `queue/discard_max_bytes` is read but never used: the discard posture
/// looks at `discard_granularity` only. The kernel reports a zero
/// `discard_max_bytes` for a device that cannot discard, and writing 0
/// there is how an administrator turns discards off while the granularity
/// stays; the plan picks `WholeSegment`, and every segment the engine
/// frees then issues a `BLKDISCARD` that fails (`EOPNOTSUPP`), one
/// `Event::Internal` per freed segment. The right behaviour: `Never` (with
/// `DiscardUnavailable`) when `discard_max_bytes` is 0.
#[test]
fn discards_switched_off_by_the_administrator_are_not_planned() {
    let mut m = device(2 * TIB, false);
    m.set("queue/discard_granularity", "4096")
        .set("queue/discard_max_bytes", "0");
    let g = plan_device(&m, "nvme0n1", &Overrides::default()).unwrap();
    // Before the fix: `WholeSegment`.
    assert_eq!(g.discard, DiscardPolicy::Never);
    assert!(g.warnings.contains(&GeomWarning::DiscardUnavailable));
}

/// Found by the Step 4 review (defect 23, fixed): a device with more segments than a `u32` counts is silently cut
/// short.
///
/// `plan` never bounds the segment count, and `DiskLayout::new` clamps it
/// with `.min(u32::MAX - 1)`: for a 64 PiB flash device (8 MiB segments)
/// half of the device is silently left out, and the segment table the
/// engine allocates and writes at every CP is `u32::MAX` entries of 32
/// bytes (128 GiB). Nothing is to be clamped silently (working agreements,
/// section 4). The right behaviour: the plan grows the segment (up to the
/// format's 256 MiB) to keep the count within a bound the segment table
/// can carry, or refuses the device, and the layout refuses rather than
/// clamps.
#[test]
fn a_device_with_more_segments_than_a_u32_gets_larger_segments() {
    let m = device(64 * PIB, false);
    match plan_device(&m, "sdq", &Overrides::default()) {
        Err(_) => {}
        Ok(g) => {
            let layout = DiskLayout::new(&g.geometry()).unwrap();
            let covered = u64::from(layout.segments) * layout.segment_bytes;
            // Before the fix: 32 PiB of 64 laid out.
            assert!(
                covered + 2 * layout.segment_bytes + layout.seg_base >= g.capacity_bytes,
                "{covered} of {} bytes laid out in {} segments",
                g.capacity_bytes,
                layout.segments
            );
        }
    }
}

// ---------------------------------------------------------------------------
// Properties that hold (pinned)
// ---------------------------------------------------------------------------

/// The hardware RAID virtual drives the markers exist for still classify
/// as such, whatever the hypervisor fix does: Dell, LSI, Avago, HPE,
/// Adaptec.
#[test]
fn controller_virtual_drives_classify_as_hardware_raid() {
    for (vendor, model) in [
        ("DELL", "PERC H755 Front"),
        ("LSI", "MR9361-8i"),
        ("AVAGO", "MR9460-16i"),
        ("HP", "LOGICAL VOLUME"),
        ("Adaptec", "ASR8885"),
        ("BROADCOM", "MR9560-16i"),
    ] {
        let mut m = device(10 * TIB, true);
        m.set("device/vendor", vendor).set("device/model", model);
        let facts = discover(&m, "sdb").unwrap();
        assert_eq!(facts.kind, DeviceKind::HwRaidVd, "{vendor} {model}");
        let g = plan(&facts, &Overrides::default()).unwrap();
        assert!(g.flush_opaque);
        assert_eq!(g.flush_policy, FlushPolicy::TrustBarrierWithNvfail);
    }
}

/// Zero and absurd values are refused by name or planned within bounds,
/// never with a panic: a zero size, a size of `u64::MAX` sectors, values
/// past `u32`, negative numbers, an optimal size that is not a multiple of
/// the minimum, a zero chunk on raid1, an md stripe beyond the format.
#[test]
fn absurd_attribute_values_are_refused_by_name_or_bounded() {
    let mut m = device(TIB, true);
    m.set("size", "0");
    assert!(matches!(
        plan_device(&m, "sda", &Overrides::default()),
        Err(GeomError::TooSmall { .. })
    ));
    let mut m = device(TIB, true);
    m.set("queue/optimal_io_size", "4294967296");
    assert!(matches!(
        discover(&m, "sda"),
        Err(GeomError::BadValue {
            attr: "queue/optimal_io_size",
            ..
        })
    ));
    let mut m = device(TIB, true);
    m.set("queue/alignment_offset", "-512");
    assert!(matches!(
        discover(&m, "sda"),
        Err(GeomError::BadValue {
            attr: "queue/alignment_offset",
            ..
        })
    ));
    // An optimal size that is not a multiple of the minimum is not a RAID
    // stripe: the default stands.
    let mut m = device(TIB, true);
    m.set("queue/minimum_io_size", "65536")
        .set("queue/optimal_io_size", "98304");
    let g = plan_device(&m, "sda", &Overrides::default()).unwrap();
    assert_eq!(g.source, GeomSource::Default);
    // An md stripe beyond 8 MiB is refused rather than shrunk.
    let mut m = device(100 * TIB, true);
    m.set("md/level", "raid6")
        .set("md/chunk_size", "1048576")
        .set("md/raid_disks", "16");
    assert!(matches!(
        plan_device(&m, "md9", &Overrides::default()),
        Err(GeomError::Unsupported { .. })
    ));
    // A raid0 whose member count reads zero has no stripe.
    let mut m = device(TIB, true);
    m.set("md/level", "raid0")
        .set("md/chunk_size", "65536")
        .set("md/raid_disks", "0");
    assert!(matches!(
        plan_device(&m, "md8", &Overrides::default()),
        Err(GeomError::Unsupported { .. })
    ));
    // `u64::MAX` sectors saturate rather than wrap.
    let mut m = device(TIB, false);
    m.set("size", u64::MAX.to_string());
    let facts = discover(&m, "nvme9n1").unwrap();
    assert_eq!(facts.size_bytes, u64::MAX);
}

/// Every segment the media rules produce is a whole number of stripes,
/// for stripes that do not divide the media's target (a 6-data-disk raid6
/// with 512 KiB chunks has a 3 MiB stripe).
#[test]
fn segments_round_up_to_whole_odd_stripes() {
    for (rotational, name, chunk, disks) in [
        (true, "md0", 524_288u32, 8u32),
        (false, "md1", 393_216, 6),
        (false, "md2", 131_072, 7),
    ] {
        let mut m = device(50 * TIB, rotational);
        m.set("md/level", "raid6")
            .set("md/chunk_size", chunk.to_string())
            .set("md/raid_disks", disks.to_string());
        let g = plan_device(&m, name, &Overrides::default()).unwrap();
        assert_eq!(g.stripe_bytes, chunk * (disks - 2));
        assert_eq!(g.segment_bytes % u64::from(g.stripe_bytes), 0);
        assert!(g.segment_bytes <= MAX_SEG_BYTES);
        let layout = DiskLayout::new(&g.geometry()).unwrap();
        assert!(layout.segments >= 8);
        let media = if rotational { Media::Hdd } else { Media::Flash };
        assert_eq!(g.media, media);
    }
}

/// A small device shrinks its segment to fit eight; the plan lays out and
/// the eight segments are really there.
#[test]
fn the_smallest_devices_that_plan_lay_out_eight_segments() {
    for size in [9 << 20, 12 << 20, 17 << 20, 64 << 20] {
        let m = device(size, false);
        match plan_device(&m, "nvme0n1", &Overrides::default()) {
            Ok(g) => {
                let layout = DiskLayout::new(&g.geometry()).unwrap();
                assert!(layout.segments >= 8, "{size}: {} segments", layout.segments);
            }
            Err(GeomError::TooSmall { .. }) => {}
            Err(e) => panic!("{size}: {e}"),
        }
    }
}

prop_compose! {
    fn number()(n in prop_oneof![
        Just(0u64),
        Just(512u64),
        Just(4096u64),
        0u64..(16 << 20),
        any::<u32>().prop_map(u64::from),
        any::<u64>(),
    ]) -> u64 { n }
}

proptest! {
    #![proptest_config(ProptestConfig {
        failure_persistence: None,
        ..ProptestConfig::default()
    })]

    /// Arbitrary attribute trees (sizes up to a petabyte, any block, IO and
    /// md values, missing attributes): discovery and planning never panic,
    /// and every plan they produce lays out with whole stripes in whole
    /// segments within the format's bounds and at least eight segments.
    #[test]
    fn arbitrary_sysfs_trees_plan_within_bounds_or_refuse(
        size_sectors in 0u64..(PIB / 512),
        lbs in prop_oneof![Just(512u64), Just(4096u64), number()],
        pbs in prop::option::of(number()),
        min_io in prop::option::of(number()),
        opt_io in prop::option::of(number()),
        align in prop::option::of(prop_oneof![Just(0u64), number()]),
        rotational in any::<bool>(),
        md in prop::option::of((
            prop::sample::select(vec!["raid0", "raid1", "raid4", "raid5", "raid6", "raid10", "linear"]),
            prop::option::of(number()),
            0u64..40,
            any::<u16>(),
        )),
        name in prop::sample::select(vec!["sda", "nvme0n1", "md0", "vda", "loop0", "dm-3", "xyz"]),
        stripe_override in prop::option::of(number()),
    ) {
        let mut m = MockSysfs::new();
        m.set("size", size_sectors.to_string())
            .set("queue/logical_block_size", lbs.to_string())
            .set("queue/rotational", if rotational { "1" } else { "0" });
        for (attr, value) in [
            ("queue/physical_block_size", pbs),
            ("queue/minimum_io_size", min_io),
            ("queue/optimal_io_size", opt_io),
            ("queue/alignment_offset", align),
        ] {
            if let Some(v) = value {
                m.set(attr, v.to_string());
            }
        }
        if let Some((level, chunk, disks, layout)) = md {
            m.set("md/level", level)
                .set("md/raid_disks", disks.to_string())
                .set("md/layout", layout.to_string());
            if let Some(c) = chunk {
                m.set("md/chunk_size", c.to_string());
            }
        }
        let overrides = Overrides {
            stripe_bytes: stripe_override.and_then(|s| u32::try_from(s).ok()),
            ..Overrides::default()
        };
        let Ok(facts) = discover(&m, name) else {
            return Ok(());
        };
        // A refusal is fine; a plan must lay out.
        if let Ok(g) = plan(&facts, &overrides) {
            prop_assert!(u64::from(g.stripe_bytes) <= MAX_STRIPE_BYTES as u64);
            prop_assert_eq!(g.stripe_bytes % 4096, 0);
            prop_assert_eq!(g.segment_bytes % u64::from(g.stripe_bytes), 0);
            prop_assert!(g.segment_bytes <= MAX_SEG_BYTES);
            let layout = DiskLayout::new(&g.geometry());
            prop_assert!(layout.is_ok(), "{:?} -> {:?}", g, layout);
            prop_assert!(layout.unwrap().segments >= 8);
        }
    }
}
