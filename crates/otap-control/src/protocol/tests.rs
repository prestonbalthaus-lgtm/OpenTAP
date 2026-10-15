//! Every request and reply round-trips through JSON. serde's internally
//! tagged enums refuse some shapes only at serialization time (a newtype
//! variant holding a bare sequence), which nothing but a round trip of
//! every variant catches.

use super::*;
use crate::{
    BLOCK_SIZE, DEFAULT_ARENA_BYTES, DEFAULT_CP_INTERVAL_SECS, DEFAULT_HUGETLB_PATH,
    MIN_ARENA_BYTES, PIN_CLONE, PIN_USER,
};

fn round_trip<T>(value: &T)
where
    T: Serialize + serde::de::DeserializeOwned + PartialEq + std::fmt::Debug,
{
    let json = serde_json::to_string(value).unwrap_or_else(|e| panic!("{value:?}: {e}"));
    let back: T = serde_json::from_str(&json).unwrap_or_else(|e| panic!("{json}: {e}"));
    assert_eq!(&back, value, "{json}");
}

fn volume() -> VolumeReport {
    VolumeReport {
        index: 3,
        name: Some("data".into()),
        size_bytes: 16 << 20,
        serving: true,
        offline: false,
        nvfail: None,
        rollback_gen: 2,
        op_pending: None,
        device: Some("/dev/ublkb0".into()),
        snapshots: 1,
    }
}

fn snapshot() -> SnapshotReport {
    SnapshotReport {
        id: 7,
        uuid: "00000000-0000-4000-8000-000000000007".into(),
        name: Some("s1".into()),
        volume_index: 3,
        volume: Some("data".into()),
        cp_seq: 41,
        created_wall_ns: 1_776_000_000_000_000_000,
        consistency: "crash".into(),
        origin: "user".into(),
        flags: PIN_USER,
        damaged: false,
        deleting: false,
    }
}

fn pool_report() -> PoolReport {
    PoolReport {
        device: "/dev/loop0".into(),
        agg_id: "00000000-0000-4000-8000-0000000000aa".into(),
        cp_seq: 42,
        fold_cp: 40,
        serving: true,
        free_segments: 50,
        segments: 63,
        reserve_segments: 8,
        segment_bytes: 2 << 20,
        stripe_bytes: 64 << 10,
        utilization: 0.2,
        admissible_bytes: 80 << 20,
        unflushed_bytes: 0,
        arena_occupancy: 0.0,
        arena_bytes: 64 << 20,
        arena: "heap".into(),
        cp_interval_secs: 5,
        pool_full: false,
        flush_policy: "TrustBarrierWithNvfail".into(),
        flush_opaque: false,
        discard: "Never".into(),
        warnings: vec!["a warning".into()],
        volumes: 1,
        snapshots: 1,
        cleaned_segments: 2,
        relocated_blocks: 300,
        cps: 42,
        writes: 1000,
        reads: 500,
        last_boot: "clean boot at cp 40".into(),
    }
}

#[test]
fn every_reply_round_trips() {
    let replies: Vec<Reply> = vec![
        Ok(Response::Ok),
        Ok(Response::Pong {
            protocol: PROTOCOL_VERSION,
            version: "0.1.0".into(),
            pool_booted: true,
        }),
        Ok(Response::Formatted {
            agg_id: "00000000-0000-4000-8000-0000000000aa".into(),
            stripe_bytes: 64 << 10,
            segment_bytes: 2 << 20,
            capacity_bytes: 128 << 20,
            segments: 63,
        }),
        Ok(Response::Pool(pool_report())),
        Ok(Response::Volumes {
            volumes: vec![volume(), volume()],
        }),
        Ok(Response::Volumes {
            volumes: Vec::new(),
        }),
        Ok(Response::Volume(volume())),
        Ok(Response::Snapshots {
            snapshots: vec![snapshot()],
        }),
        Ok(Response::Snapshots {
            snapshots: Vec::new(),
        }),
        Ok(Response::Snapshot(snapshot())),
        Ok(Response::Io(IoResultReport {
            status: "ok".into(),
            seq: 12,
            data_hex: "ab".repeat(BLOCK_SIZE),
        })),
        Ok(Response::Committed { cp_seq: 9 }),
        Err(ControlError::new(ErrorCode::NoPool, "no pool is booted")),
        Err(ControlError::new(ErrorCode::Timeout, "")),
    ];
    for reply in &replies {
        round_trip(reply);
    }
    // The tag is `kind`, next to the fields, so a client switches on it.
    let json = serde_json::to_value(Response::Volumes {
        volumes: vec![volume()],
    })
    .unwrap();
    assert_eq!(json["kind"], "volumes");
    assert_eq!(json["volumes"][0]["name"], "data");
    let json = serde_json::to_value(Response::Pool(pool_report())).unwrap();
    assert_eq!(json["kind"], "pool");
    assert_eq!(json["segments"], 63);
    let json = serde_json::to_value(Response::Ok).unwrap();
    assert_eq!(json, serde_json::json!({ "kind": "ok" }));
    let json = serde_json::to_value(ControlError::new(ErrorCode::BadRequest, "x")).unwrap();
    assert_eq!(json["code"], "bad_request");
}

#[test]
fn every_request_round_trips() {
    let requests = vec![
        Request::Ping,
        Request::PoolFormat {
            device: "/dev/sdb".into(),
            size_bytes: Some(1 << 30),
            media: Some(MediaKind::Hdd),
            stripe_bytes: Some(1 << 20),
            segment_bytes: Some(64 << 20),
            flush_opaque: Some(true),
            strict_flush: true,
            force: true,
        },
        Request::PoolBoot {
            device: "/dev/sdb".into(),
            media: None,
            stripe_bytes: None,
            segment_bytes: None,
            flush_opaque: None,
            strict_flush: false,
            arena_bytes: DEFAULT_ARENA_BYTES,
            arena: ArenaKind::Hugetlb {
                path: DEFAULT_HUGETLB_PATH.into(),
            },
            cp_interval_secs: DEFAULT_CP_INTERVAL_SECS,
        },
        Request::PoolBoot {
            device: "pool.img".into(),
            media: Some(MediaKind::Flash),
            stripe_bytes: None,
            segment_bytes: None,
            flush_opaque: None,
            strict_flush: false,
            arena_bytes: MIN_ARENA_BYTES,
            arena: ArenaKind::Heap,
            cp_interval_secs: 1,
        },
        Request::PoolStatus,
        Request::PoolSync,
        Request::PoolClean,
        Request::PoolStop,
        Request::DaemonStop,
        Request::VolumeCreate {
            name: "data".into(),
            size_bytes: 10 << 30,
        },
        Request::VolumeList,
        Request::VolumeShow {
            name: "data".into(),
        },
        Request::VolumeDelete {
            name: "data".into(),
        },
        Request::VolumeOnline {
            name: "data".into(),
            online: false,
        },
        Request::VolumeResize {
            name: "data".into(),
            size_bytes: 20 << 30,
        },
        Request::VolumeRename {
            name: "data".into(),
            new_name: "db".into(),
        },
        Request::VolumeNvfailClear {
            name: "data".into(),
        },
        Request::SnapshotCreate {
            volume: "data".into(),
            name: "s1".into(),
        },
        Request::SnapshotList { volume: None },
        Request::SnapshotList {
            volume: Some("data".into()),
        },
        Request::SnapshotDelete {
            volume: "data".into(),
            name: "s1".into(),
        },
        Request::SnapshotRollback {
            volume: "data".into(),
            name: "s1".into(),
            delete_newer: true,
        },
        Request::SnapshotPin {
            volume: "data".into(),
            name: "s1".into(),
            flags: PIN_USER | PIN_CLONE,
        },
        Request::SnapshotReplicationBase {
            volume: "data".into(),
            name: None,
        },
        Request::IoWrite {
            volume: "data".into(),
            lba: 8,
            data_hex: "00".repeat(BLOCK_SIZE),
        },
        Request::IoRead {
            volume: "data".into(),
            lba: 8,
            blocks: MAX_IO_BLOCKS,
        },
        Request::IoPunch {
            volume: "data".into(),
            lba: 8,
            blocks: 1,
        },
    ];
    for request in &requests {
        round_trip(request);
    }
    let json = serde_json::to_value(Request::VolumeList).unwrap();
    assert_eq!(json, serde_json::json!({ "op": "volume_list" }));
    let json = serde_json::to_value(Request::PoolBoot {
        device: "d".into(),
        media: None,
        stripe_bytes: None,
        segment_bytes: None,
        flush_opaque: None,
        strict_flush: false,
        arena_bytes: MIN_ARENA_BYTES,
        arena: ArenaKind::Heap,
        cp_interval_secs: 1,
    })
    .unwrap();
    assert_eq!(json["op"], "pool_boot");
    assert_eq!(json["arena"], "heap");
}
