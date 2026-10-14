//! Geometry discovery against mocked sysfs trees: classification, the
//! stripe and segment rules, the E19 flush policy, the discard posture,
//! the overrides and the refusals.

use otap_cp::{DiscardPolicy, DiskLayout, FlushPolicy, Media};
use otap_format::{MAX_SEG_BYTES, MAX_STRIPE_BYTES};
use otap_geom::{
    discover, plan, plan_device, DeviceKind, GeomError, GeomSource, GeomWarning, MockSysfs,
    Overrides, Sysfs, SysfsSource, WriteCacheView,
};
use proptest::prelude::*;

/// A device of `size` bytes with the common queue attributes.
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

const TIB: u64 = 1 << 40;
const GIB: u64 = 1 << 30;
const MIB: u64 = 1 << 20;

#[test]
fn an_md_raid6_takes_chunk_times_data_disks_and_large_rotational_segments() {
    let mut m = device(40 * TIB, true);
    m.set("md/level", "raid6")
        .set("md/chunk_size", "524288")
        .set("md/raid_disks", "8")
        .set("md/layout", "2")
        .set("queue/optimal_io_size", "3145728");
    let facts = discover(&m, "md0").unwrap();
    assert_eq!(facts.kind, DeviceKind::Md);
    assert_eq!(facts.md.as_ref().unwrap().data_disks(), Some(6));
    let g = plan(&facts, &Overrides::default()).unwrap();
    assert_eq!(g.stripe_bytes, 3 * MIB as u32);
    assert_eq!(g.source, GeomSource::MdSysfs);
    assert_eq!((g.chunk_bytes, g.data_disks), (512 * 1024, 6));
    assert_eq!(g.media, Media::Hdd);
    // 80 MiB rounded up to whole 3 MiB stripes: 81 MiB.
    assert_eq!(g.segment_bytes, 27 * 3 * MIB);
    assert!(!g.flush_opaque);
    assert_eq!(g.flush_policy, FlushPolicy::RequireFuaOrFlush);
    assert_eq!(g.discard, DiscardPolicy::Never);
    assert!(g.warnings.contains(&GeomWarning::StripeExceedsMaxSectors {
        stripe_bytes: 3 * MIB as u32,
        max_bytes: 1280 * 1024
    }));
    assert!(!g
        .warnings
        .iter()
        .any(|w| matches!(w, GeomWarning::OptimalIoDisagrees { .. })));
    assert!(DiskLayout::new(&g.geometry()).is_ok());
    let profile = g.media_profile();
    assert_eq!(profile.cp_timer_ns, 10_000_000_000);
    assert_eq!(profile.flush_policy, FlushPolicy::RequireFuaOrFlush);
}

#[test]
fn md_levels_map_to_data_disks() {
    for (level, disks, layout, expected) in [
        ("raid0", 4, 0, Some(4)),
        ("raid4", 4, 0, Some(3)),
        ("raid5", 4, 2, Some(3)),
        ("raid6", 4, 2, Some(2)),
        ("raid10", 4, 0x102, Some(2)),
        ("raid10", 6, 0x103, Some(2)),
        ("raid10", 8, 0x201, Some(4)),
        ("raid1", 2, 0, None),
        ("linear", 3, 0, None),
        ("raid6", 2, 0, None),
    ] {
        let mut m = device(8 * TIB, true);
        m.set("md/level", level)
            .set("md/chunk_size", "65536")
            .set("md/raid_disks", disks.to_string())
            .set("md/layout", layout.to_string());
        let facts = discover(&m, "md1").unwrap();
        assert_eq!(
            facts.md.as_ref().unwrap().data_disks(),
            expected,
            "{level}/{disks}"
        );
    }
    // raid1 strides nothing: the default write unit applies.
    let mut m = device(2 * TIB, true);
    m.set("md/level", "raid1")
        .set("md/chunk_size", "0")
        .set("md/raid_disks", "2");
    let g = plan_device(&m, "md2", &Overrides::default()).unwrap();
    assert_eq!(
        (g.stripe_bytes, g.source),
        (MIB as u32, GeomSource::Default)
    );
    // An md stripe disagreeing with optimal_io_size is a warning.
    let mut m = device(8 * TIB, true);
    m.set("md/level", "raid5")
        .set("md/chunk_size", "65536")
        .set("md/raid_disks", "5")
        .set("queue/optimal_io_size", "131072");
    let g = plan_device(&m, "md3", &Overrides::default()).unwrap();
    assert_eq!(g.stripe_bytes, 4 * 65536);
    assert!(g.warnings.contains(&GeomWarning::OptimalIoDisagrees {
        optimal_io_size: 131072
    }));
}

#[test]
fn a_perc_virtual_drive_is_flush_opaque_with_the_barrier_trusted() {
    let mut m = device(12 * TIB, true);
    m.set("device/vendor", "DELL    ")
        .set("device/model", "PERC H730P Adp  ")
        .set("queue/write_cache", "write through")
        .set("queue/fua", "0")
        .set("queue/optimal_io_size", "1572864")
        .set("queue/max_sectors_kb", "256");
    let facts = discover(&m, "sda").unwrap();
    assert_eq!(facts.kind, DeviceKind::HwRaidVd);
    assert_eq!(facts.write_cache, WriteCacheView::WriteThrough);
    assert!(!facts.fua);
    let g = plan(&facts, &Overrides::default()).unwrap();
    assert_eq!(
        (g.stripe_bytes, g.source),
        (1_572_864, GeomSource::OptimalIoSize)
    );
    assert!(g.flush_opaque);
    assert_eq!(g.flush_policy, FlushPolicy::TrustBarrierWithNvfail);
    assert!(g.warnings.contains(&GeomWarning::FlushOpaqueTrusted));
    assert_eq!(g.discard, DiscardPolicy::Never);
    assert!(g.geometry().flush_opaque);
    assert!(g.media_profile().trusts_opaque_barrier());
    // Strict mode: the policy refuses, which the engine then enforces.
    let strict = plan(
        &facts,
        &Overrides {
            flush_policy: Some(FlushPolicy::RequireFuaOrFlush),
            ..Overrides::default()
        },
    )
    .unwrap();
    assert_eq!(strict.flush_policy, FlushPolicy::RequireFuaOrFlush);
    assert!(strict.flush_opaque);
    assert!(!strict.warnings.contains(&GeomWarning::FlushOpaqueTrusted));
    // A virtual drive that reports no optimal size gets the default and a
    // warning that the probe has not run.
    let mut m = device(12 * TIB, true);
    m.set("device/vendor", "AVAGO")
        .set("device/model", "MR9361-8i")
        .set("queue/write_cache", "write through");
    let g = plan_device(&m, "sdb", &Overrides::default()).unwrap();
    assert_eq!(
        (g.stripe_bytes, g.source),
        (MIB as u32, GeomSource::Default)
    );
    assert!(g.warnings.contains(&GeomWarning::OptimalIoUnknown));
    for (vendor, model) in [
        ("HP", "LOGICAL VOLUME"),
        ("HPE", "Smart Array P408i"),
        ("ADAPTEC", "ASR8805"),
        ("LSI", "MR9271-8i"),
        ("Broadcom", "Virtual Disk"),
    ] {
        let mut m = device(TIB, true);
        m.set("device/vendor", vendor).set("device/model", model);
        assert_eq!(
            discover(&m, "sdc").unwrap().kind,
            DeviceKind::HwRaidVd,
            "{vendor} {model}"
        );
    }
}

#[test]
fn nvme_and_sata_flash_get_small_segments_and_whole_segment_discards() {
    let mut m = device(2 * TIB, false);
    m.set("queue/discard_granularity", "512")
        .set("queue/discard_max_bytes", "2199023255040")
        .set("queue/optimal_io_size", "0");
    let g = plan_device(&m, "nvme0n1", &Overrides::default()).unwrap();
    assert_eq!(g.kind, DeviceKind::Nvme);
    assert_eq!(g.media, Media::Flash);
    assert_eq!(g.stripe_bytes, MIB as u32);
    assert_eq!(g.segment_bytes, 64 * MIB);
    assert_eq!(g.discard, DiscardPolicy::WholeSegment);
    assert!(!g.flush_opaque);
    assert_eq!(g.media_profile().cp_timer_ns, 5_000_000_000);
    let mut m = device(960 * GIB, false);
    m.set("device/vendor", "ATA")
        .set("device/model", "Samsung SSD 870")
        .set("queue/discard_granularity", "4096")
        .set("queue/discard_max_bytes", "2147450880");
    let g = plan_device(&m, "sdd", &Overrides::default()).unwrap();
    assert_eq!(g.kind, DeviceKind::Disk);
    assert_eq!(g.segment_bytes, 8 * MIB);
    assert_eq!(g.discard, DiscardPolicy::WholeSegment);
    // Flash without a discard granularity: no discards, with a warning.
    let m = device(960 * GIB, false);
    let g = plan_device(&m, "sde", &Overrides::default()).unwrap();
    assert_eq!(g.discard, DiscardPolicy::Never);
    assert!(g.warnings.contains(&GeomWarning::DiscardUnavailable));
}

#[test]
fn virtual_loop_and_dm_devices_are_classified_and_honest() {
    let mut m = device(64 * GIB, true);
    m.set("device/vendor", "QEMU")
        .set("device/model", "QEMU HARDDISK");
    assert_eq!(discover(&m, "sda").unwrap().kind, DeviceKind::Virtual);
    assert_eq!(
        discover(&device(8 * GIB, false), "vda").unwrap().kind,
        DeviceKind::Virtual
    );
    assert_eq!(
        discover(&device(8 * GIB, false), "loop3").unwrap().kind,
        DeviceKind::Loop
    );
    assert_eq!(
        discover(&device(8 * GIB, false), "dm-1").unwrap().kind,
        DeviceKind::Dm
    );
    assert_eq!(
        discover(&device(8 * GIB, false), "mmcblk0").unwrap().kind,
        DeviceKind::Other
    );
    let g = plan_device(&device(8 * GIB, false), "loop3", &Overrides::default()).unwrap();
    assert!(!g.flush_opaque);
    assert_eq!(g.flush_policy, FlushPolicy::RequireFuaOrFlush);
}

#[test]
fn small_devices_shrink_the_segment_and_tiny_ones_are_refused() {
    // A 256 MiB CI disk: 8 segments of 1 MiB stripes fit; 64 MiB segments
    // do not, so they shrink to the budget.
    let mut m = device(256 * MIB, false);
    m.set("queue/discard_granularity", "4096");
    let g = plan_device(&m, "nvme1n1", &Overrides::default()).unwrap();
    assert!(g.warnings.contains(&GeomWarning::SmallDevice));
    assert!(g.segment_bytes <= 256 * MIB / 8);
    assert_eq!(g.segment_bytes % u64::from(g.stripe_bytes), 0);
    assert!(DiskLayout::new(&g.geometry()).is_ok());
    let layout = DiskLayout::new(&g.geometry()).unwrap();
    assert!(layout.segments >= 8);
    // Too small for eight stripes plus the fixed regions.
    let err = plan_device(&device(4 * MIB, false), "nvme2n1", &Overrides::default()).unwrap_err();
    assert!(matches!(err, GeomError::TooSmall { .. }), "{err}");
    // A segment override that does not fit is refused, not shrunk.
    let err = plan_device(
        &device(2 * GIB, false),
        "nvme1n1",
        &Overrides {
            segment_bytes: Some(256 * MIB),
            ..Overrides::default()
        },
    )
    .unwrap_err();
    assert!(matches!(err, GeomError::TooSmall { .. }), "{err}");
}

#[test]
fn overrides_win_and_are_bounded() {
    let mut m = device(10 * TIB, true);
    m.set("device/vendor", "DELL")
        .set("device/model", "PERC H740P");
    let g = plan_device(
        &m,
        "sda",
        &Overrides {
            stripe_bytes: Some(2 * MIB as u32),
            segment_bytes: Some(128 * MIB),
            flush_opaque: Some(false),
            discard: Some(DiscardPolicy::WholeSegment),
            media: Some(Media::Flash),
            ..Overrides::default()
        },
    )
    .unwrap();
    assert_eq!(
        (g.stripe_bytes, g.source),
        (2 * MIB as u32, GeomSource::Manual)
    );
    assert_eq!(g.segment_bytes, 128 * MIB);
    assert!(!g.flush_opaque);
    assert_eq!(g.flush_policy, FlushPolicy::RequireFuaOrFlush);
    assert_eq!(g.discard, DiscardPolicy::WholeSegment);
    assert_eq!(g.media, Media::Flash);
    for stripe in [0u32, 4096, 4096 * 3 + 1, MAX_STRIPE_BYTES as u32 * 2] {
        let err = plan_device(
            &m,
            "sda",
            &Overrides {
                stripe_bytes: Some(stripe),
                ..Overrides::default()
            },
        )
        .unwrap_err();
        assert!(
            matches!(
                err,
                GeomError::BadOverride {
                    what: "stripe_bytes",
                    ..
                }
            ),
            "{stripe}: {err}"
        );
    }
    for segment in [0u64, 3 * MIB, MAX_SEG_BYTES + MIB] {
        let err = plan_device(
            &m,
            "sda",
            &Overrides {
                stripe_bytes: Some(2 * MIB as u32),
                segment_bytes: Some(segment),
                ..Overrides::default()
            },
        )
        .unwrap_err();
        assert!(
            matches!(
                err,
                GeomError::BadOverride {
                    what: "segment_bytes",
                    ..
                }
            ),
            "{segment}: {err}"
        );
    }
}

#[test]
fn misalignment_and_odd_blocks_are_refused_unless_allowed() {
    let mut m = device(TIB, true);
    m.set("queue/alignment_offset", "3584");
    let err = plan_device(&m, "sda", &Overrides::default()).unwrap_err();
    assert!(
        matches!(err, GeomError::Misaligned { offset: 3584 }),
        "{err}"
    );
    let g = plan_device(
        &m,
        "sda",
        &Overrides {
            allow_misaligned: true,
            ..Overrides::default()
        },
    )
    .unwrap();
    assert!(g
        .warnings
        .contains(&GeomWarning::Misaligned { offset: 3584 }));
    let mut m = device(TIB, true);
    m.set("queue/logical_block_size", "8192");
    assert!(matches!(
        plan_device(&m, "sda", &Overrides::default()).unwrap_err(),
        GeomError::Unsupported { .. }
    ));
}

#[test]
fn missing_and_malformed_attributes_name_themselves() {
    let mut m = device(TIB, true);
    m.unset("size");
    assert_eq!(
        discover(&m, "sda").unwrap_err(),
        GeomError::Missing { attr: "size" }
    );
    let mut m = device(TIB, true);
    m.unset("queue/rotational");
    assert_eq!(
        discover(&m, "sda").unwrap_err(),
        GeomError::Missing {
            attr: "queue/rotational"
        }
    );
    let mut m = device(TIB, true);
    m.set("queue/optimal_io_size", "lots");
    assert!(matches!(
        discover(&m, "sda").unwrap_err(),
        GeomError::BadValue {
            attr: "queue/optimal_io_size",
            ..
        }
    ));
    let mut m = device(TIB, true);
    m.set("queue/write_cache", "sometimes");
    assert!(matches!(
        discover(&m, "sda").unwrap_err(),
        GeomError::BadValue {
            attr: "queue/write_cache",
            ..
        }
    ));
    // Optional attributes default: a minimal tree still plans.
    let mut m = MockSysfs::new();
    m.set("size", "4194304\n")
        .set("queue/logical_block_size", "512\n")
        .set("queue/rotational", "0\n");
    let facts = discover(&m, "sdz").unwrap();
    assert_eq!(facts.write_cache, WriteCacheView::Unknown);
    assert_eq!(facts.minimum_io_size, 512);
    assert!(plan(&facts, &Overrides::default()).is_ok());
}

#[test]
fn the_file_source_reads_a_copied_tree() {
    let dir = std::env::temp_dir().join(format!("otap-geom-{}", std::process::id()));
    let dev = dir.join("sdq");
    std::fs::create_dir_all(dev.join("queue")).unwrap();
    std::fs::write(dev.join("size"), "2097152\n").unwrap();
    std::fs::write(dev.join("queue").join("logical_block_size"), "512\n").unwrap();
    std::fs::write(dev.join("queue").join("rotational"), "1\n").unwrap();
    let source = Sysfs::at(&dir, "sdq");
    assert_eq!(source.read("size").as_deref(), Some("2097152"));
    assert_eq!(source.read("queue/optimal_io_size"), None);
    let facts = discover(&source, "sdq").unwrap();
    assert_eq!(facts.size_bytes, 2097152 * 512);
    assert!(facts.rotational);
    std::fs::remove_dir_all(&dir).unwrap();
}

proptest! {
    #![proptest_config(ProptestConfig {
        failure_persistence: None,
        ..ProptestConfig::default()
    })]

    /// Every plan the rules produce lays out, keeps the segment a whole
    /// number of stripes within the format's bounds, and leaves at least
    /// eight segments.
    #[test]
    fn every_plan_lays_out(
        size_gib in 1u64..=4096,
        rotational in any::<bool>(),
        optimal_kib in prop_oneof![Just(0u32), 4u32..=8192],
        nvme in any::<bool>(),
        raid in prop_oneof![Just(None), (2u32..=16, 4u32..=1024).prop_map(Some)],
    ) {
        let mut m = device(size_gib * GIB, rotational);
        m.set("queue/optimal_io_size", (optimal_kib * 1024).to_string());
        let name = if nvme { "nvme0n1" } else { "sda" };
        if let Some((disks, chunk_kib)) = raid {
            m.set("md/level", "raid6")
                .set("md/chunk_size", (chunk_kib * 1024).to_string())
                .set("md/raid_disks", disks.to_string());
        }
        let name = if raid.is_some() { "md0" } else { name };
        match plan_device(&m, name, &Overrides::default()) {
            Ok(g) => {
                prop_assert_eq!(g.segment_bytes % u64::from(g.stripe_bytes), 0);
                prop_assert!(g.segment_bytes <= MAX_SEG_BYTES);
                prop_assert!(u64::from(g.stripe_bytes) <= MAX_STRIPE_BYTES as u64);
                let layout = DiskLayout::new(&g.geometry());
                prop_assert!(layout.is_ok(), "{:?}", layout);
                prop_assert!(layout.unwrap().segments >= 8);
            }
            Err(GeomError::TooSmall { .. } | GeomError::Unsupported { .. }) => {}
            Err(e) => prop_assert!(false, "unexpected refusal: {e}"),
        }
    }
}
