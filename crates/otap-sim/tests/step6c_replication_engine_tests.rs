//! Step 6C: the engine's half of replication, call by call: the replica
//! mark and its refusals, the sink's writes and holes and when they are
//! durable, back-pressure from a full arena, intents in precise mode, and
//! the delta walk a source ships (data, holes, a shrink and a grow that a
//! birth walk cannot see, shared pages skipped, quarantine).

use otap_cp::{DeltaKind, EngineError, Event, NvfailIntent, PeekSource, Status};
use otap_format::{SnapKind, SnapOrigin, Uuid, BLOCK_SIZE};
use otap_sim::{Sim, SimConfig};

fn sim(seed: u64, volumes: Vec<(u32, u64)>) -> Sim {
    let mut cfg = SimConfig::small(seed);
    cfg.volumes = volumes;
    let mut s = Sim::new(cfg).expect("formats");
    s.boot().expect("boots");
    s
}

fn block(byte: u8) -> Vec<u8> {
    vec![byte; BLOCK_SIZE]
}

/// Ticks until every replication write placed so far is durable.
fn settle(s: &mut Sim, seq: u64) {
    s.engine().request_cp();
    s.tick_until(|e| e.durable_seq() >= seq && e.is_quiet(), "durable")
        .expect("the CP commits");
}

#[test]
fn the_replica_mark_guards_the_mirror() {
    let mut s = sim(6101, vec![(0, 64)]);
    let e = s.engine().apply_write(0, 0, &block(1));
    assert!(
        matches!(e, Err(EngineError::NotReplica { vol: 0 })),
        "{e:?}"
    );
    let e = s.engine().set_replica(0, true);
    assert!(
        matches!(e, Err(EngineError::VolumeOnline { vol: 0 })),
        "{e:?}"
    );
    s.set_online(0, false).expect("offline");
    s.engine().set_replica(0, true).expect("marked");
    assert!(s.engine().is_replica(0));
    let e = s.engine().set_volume_online(0, true);
    assert!(
        matches!(e, Err(EngineError::ReplicaVolume { vol: 0 })),
        "{e:?}"
    );
    assert!(e.unwrap_err().to_string().contains("replicate release"));
    // Clients are refused: the mirror is offline.
    let id = s.submit_write(0, 0, 1);
    let c = s.wait(id, 1000).expect("answered").expect("completed");
    assert_eq!(c.status, Status::Offline);
    // The sink writes.
    let seq = s
        .engine()
        .apply_write(0, 2, &[block(7), block(8)].concat())
        .expect("placed")
        .expect("room");
    assert!(seq > s.engine().durable_seq(), "not durable before its CP");
    settle(&mut s, seq);
    let mut buf = vec![0u8; 2 * BLOCK_SIZE];
    let src = s.engine().peek_disk(0, 2, &mut buf).expect("reads");
    assert_eq!(src, vec![PeekSource::Disk, PeekSource::Disk]);
    assert_eq!(&buf[..BLOCK_SIZE], &block(7)[..]);
    assert_eq!(&buf[BLOCK_SIZE..], &block(8)[..]);
    // A hole.
    let seq = s
        .engine()
        .apply_punch(0, 3, 1)
        .expect("placed")
        .expect("room");
    settle(&mut s, seq);
    let mut one = vec![0xffu8; BLOCK_SIZE];
    let src = s.engine().peek_disk(0, 3, &mut one).expect("reads");
    assert_eq!(src, vec![PeekSource::Hole]);
    assert!(one.iter().all(|b| *b == 0));
    // Out of range, bad lengths.
    assert!(s.engine().apply_write(0, 64, &block(1)).is_err());
    assert!(s.engine().apply_write(0, 0, &[0u8; 100]).is_err());
    assert!(s.engine().apply_punch(0, 60, 5).is_err());
    assert!(s.engine().apply_punch(0, 0, 0).is_err());
    assert_eq!(s.engine().stats().applied, 3);
    // Released, it may come online.
    s.engine().set_replica(0, false).expect("cleared");
    s.engine().set_volume_online(0, true).expect("online");
}

#[test]
fn a_full_arena_says_not_now_and_a_cp_makes_room() {
    let mut s = sim(6102, vec![(0, 2048)]);
    s.set_online(0, false).expect("offline");
    s.engine().set_replica(0, true).expect("marked");
    let run = vec![0x5au8; 64 * BLOCK_SIZE];
    let mut placed = 0u64;
    let mut last = 0;
    let mut refused = false;
    for k in 0..32u64 {
        match s.engine().apply_write(0, k * 64, &run).expect("no error") {
            Some(seq) => {
                placed += 1;
                last = seq;
            }
            None => {
                refused = true;
                break;
            }
        }
    }
    assert!(refused, "a 2 MiB arena cannot take 8 MiB unflushed");
    assert!(placed > 0);
    settle(&mut s, last);
    assert!(
        s.engine()
            .apply_write(0, 0, &run)
            .expect("no error")
            .is_some(),
        "the CP made room"
    );
}

#[test]
fn replication_writes_take_an_intent_in_precise_mode() {
    let mut cfg = SimConfig::small(6103);
    cfg.volumes = vec![(0, 64), (1, 64)];
    cfg.intent = NvfailIntent::Precise;
    let mut s = Sim::new(cfg).expect("formats");
    s.boot().expect("boots");
    s.set_online(1, false).expect("offline");
    s.engine().set_replica(1, true).expect("marked");
    let from = s.events.len();
    let seq = s
        .engine()
        .apply_write(1, 0, &block(3))
        .expect("placed")
        .expect("room");
    settle(&mut s, seq);
    assert!(
        s.events[from..].iter().any(|e| matches!(
            e,
            Event::IntentWritten { write_active, .. } if write_active.get(1)
        )),
        "an intent carried the mirror"
    );
}

#[test]
fn the_delta_walk_ships_data_holes_and_lost_blocks() {
    let mut s = sim(6104, vec![(0, 3000)]);
    for lba in [0u64, 1, 2, 100, 1500, 2500] {
        s.write(0, lba, 1).expect("writes");
    }
    s.punch(0, 50, 2, false).expect("punches");
    let b = s.snapshot(0).expect("base");
    // The full walk: every data block, no hole.
    let full = s
        .engine()
        .snapshot_delta_scan(b, None, 0, usize::MAX, 1000)
        .expect("walks");
    let lbas: Vec<u64> = full.items.iter().map(|i| i.lba).collect();
    assert_eq!(lbas, vec![0, 1, 2, 100, 1500, 2500]);
    assert!(full
        .items
        .iter()
        .all(|i| matches!(i.kind, DeltaKind::Data { .. })));
    assert_eq!(full.next_lba, None);
    assert_eq!(full.size_blocks, 3000);
    // Changes: an overwrite, a punch over data, a punch over nothing, a
    // shrink below 2500 and a grow back (2500 is lost), a new block.
    s.write(0, 1, 1).expect("writes");
    s.punch(0, 100, 1, false).expect("punches");
    s.punch(0, 900, 1, false).expect("punches");
    s.resize_volume(0, 2048).expect("shrinks");
    s.set_online(0, true).expect("online");
    s.resize_volume(0, 3000).expect("grows");
    s.write(0, 2900, 1).expect("writes");
    let t = s.snapshot(0).expect("target");
    let delta = s
        .engine()
        .snapshot_delta_scan(t, Some(b), 0, usize::MAX, 1000)
        .expect("walks");
    let got: Vec<(u64, bool)> = delta
        .items
        .iter()
        .map(|i| (i.lba, matches!(i.kind, DeltaKind::Data { .. })))
        .collect();
    assert_eq!(
        got,
        vec![
            (1, true),
            (100, false),
            (900, false),
            (2500, false),
            (2900, true)
        ],
        "the delta: data at 1 and 2900, holes at 100 and 900, and 2500 lost to the shrink"
    );
    // Sliced by data blocks: the walk resumes where it stopped.
    let first = s
        .engine()
        .snapshot_delta_scan(t, Some(b), 0, 1, 1000)
        .expect("walks");
    assert_eq!(first.items.last().map(|i| i.lba), Some(1));
    assert_eq!(first.next_lba, Some(2));
    let rest = s
        .engine()
        .snapshot_delta_scan(t, Some(b), 2, usize::MAX, 1000)
        .expect("walks");
    assert_eq!(rest.items.first().map(|i| i.lba), Some(100));
    // An empty delta walks to the end with nothing.
    let t2 = s.snapshot(0).expect("unchanged");
    let none = s
        .engine()
        .snapshot_delta_scan(t2, Some(t), 0, usize::MAX, 1000)
        .expect("walks");
    assert!(none.items.is_empty(), "{:?}", none.items);
    assert_eq!(none.pages, 0, "shared pages are not read");
    // The base must be older and of the same volume.
    let e = s.engine().snapshot_delta_scan(b, Some(t), 0, 1, 1);
    assert!(matches!(e, Err(EngineError::SnapshotOrder { .. })), "{e:?}");
    // Reads through the tree, zeros for holes and unwritten blocks.
    let mut buf = vec![0u8; 3 * BLOCK_SIZE];
    let info = s
        .engine()
        .snapshot_read_blocks(t, &[1, 100, 2500], &mut buf)
        .expect("reads");
    assert_eq!(info[0].source, PeekSource::Disk);
    assert_eq!(info[1].source, PeekSource::Hole);
    assert_eq!(info[2].source, PeekSource::Unmapped);
    assert!(buf[BLOCK_SIZE..].iter().all(|b| *b == 0));
    assert!(s
        .engine()
        .snapshot_read_blocks(t, &[3000], &mut buf[..BLOCK_SIZE])
        .is_err());
    assert!(s.engine().snapshot_read_blocks(t, &[1], &mut buf).is_err());
}

#[test]
fn a_sink_snapshot_takes_the_identity_and_the_name_it_is_given() {
    let mut s = sim(6105, vec![(0, 64)]);
    let uuid = Uuid::from_bytes([0x42; 16]);
    s.engine()
        .create_snapshot_as(
            0,
            uuid,
            SnapKind::Crash,
            SnapOrigin::Transfer,
            Some("nightly"),
        )
        .expect("asked");
    assert!(s.engine().snapshot_pending(uuid));
    s.tick_until(|e| !e.snapshot_pending(uuid), "the snapshot")
        .expect("taken");
    let info = s
        .engine()
        .snapshots()
        .into_iter()
        .find(|i| i.uuid == uuid)
        .expect("listed");
    assert_eq!(info.origin, SnapOrigin::Transfer);
    assert_eq!(info.name.as_deref(), Some("nightly"));
    let again = s
        .engine()
        .create_snapshot_as(0, uuid, SnapKind::Crash, SnapOrigin::Transfer, None);
    assert!(
        matches!(again, Err(EngineError::SnapshotExists { .. })),
        "{again:?}"
    );
}
