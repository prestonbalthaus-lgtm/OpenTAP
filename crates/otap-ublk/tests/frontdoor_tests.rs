//! The front door state machine against the mock transport: decoding,
//! the errno map, the watermarks, every tag transition, stale and aborted
//! completions, and the transport's failures. No engine: the test plays
//! the engine's part through the `FrontDoor` trait.

use otap_cp::{FrontDoor, IoError, ReqId, Request, RequestKind, Status};
use otap_format::BLOCK_SIZE;
use otap_sys::ublk::{
    UblksrvIoDesc, UBLK_IO_OP_DISCARD, UBLK_IO_OP_FLUSH, UBLK_IO_OP_READ, UBLK_IO_OP_REPORT_ZONES,
    UBLK_IO_OP_WRITE, UBLK_IO_OP_WRITE_SAME, UBLK_IO_OP_WRITE_ZEROES, UBLK_IO_OP_ZONE_APPEND,
    UBLK_IO_OP_ZONE_RESET,
};
use otap_ublk::{
    decode_request, errno_for, request_id, split_id, MockTransport, UblkFrontDoor, Watermarks,
    EINVAL, EIO, ENODEV, ENOSPC, EOPNOTSUPP,
};

const VOL: u32 = 3;
const SIZE: u64 = 1024;
const Q: u16 = 0;
const SPB: u64 = (BLOCK_SIZE / 512) as u64;

fn desc(op: u8, start_sector: u64, nr_sectors: u32) -> UblksrvIoDesc {
    UblksrvIoDesc {
        op_flags: u32::from(op),
        nr_sectors,
        start_sector,
        addr: 0,
    }
}

fn door(depth: u16) -> UblkFrontDoor<MockTransport> {
    UblkFrontDoor::new(
        MockTransport::new(depth),
        Q,
        VOL,
        SIZE,
        depth,
        Watermarks::for_depth(depth),
    )
}

fn data(blocks: u32, fill: u8) -> Vec<u8> {
    vec![fill; blocks as usize * BLOCK_SIZE]
}

#[test]
fn descriptors_decode_into_requests() {
    let d = |op, lba: u64, blocks: u32| desc(op, lba * SPB, blocks * (SPB as u32));
    assert_eq!(
        decode_request(&d(UBLK_IO_OP_READ, 5, 2), VOL, SIZE),
        Ok(RequestKind::Read {
            vol: VOL,
            lba: 5,
            blocks: 2
        })
    );
    assert_eq!(
        decode_request(&d(UBLK_IO_OP_WRITE, 0, 1), VOL, SIZE),
        Ok(RequestKind::Write {
            vol: VOL,
            lba: 0,
            blocks: 1
        })
    );
    assert_eq!(
        decode_request(&d(UBLK_IO_OP_DISCARD, 1000, 24), VOL, SIZE),
        Ok(RequestKind::Discard {
            vol: VOL,
            lba: 1000,
            blocks: 24
        })
    );
    assert_eq!(
        decode_request(&d(UBLK_IO_OP_WRITE_ZEROES, 1023, 1), VOL, SIZE),
        Ok(RequestKind::WriteZeroes {
            vol: VOL,
            lba: 1023,
            blocks: 1
        })
    );
    // A flush carries no range; whatever the driver put there is ignored.
    assert_eq!(
        decode_request(&desc(UBLK_IO_OP_FLUSH, 7, 3), VOL, SIZE),
        Ok(RequestKind::Flush { vol: VOL })
    );
}

#[test]
fn unsupported_operations_are_eopnotsupp() {
    for op in [
        UBLK_IO_OP_WRITE_SAME,
        UBLK_IO_OP_ZONE_APPEND,
        UBLK_IO_OP_ZONE_RESET,
        UBLK_IO_OP_REPORT_ZONES,
        6,
        9,
        255,
    ] {
        assert_eq!(
            decode_request(&desc(op, 0, SPB as u32), VOL, SIZE),
            Err(EOPNOTSUPP),
            "op {op}"
        );
    }
}

#[test]
fn malformed_ranges_are_einval() {
    let cases: [(u8, u64, u32); 8] = [
        // Zero length.
        (UBLK_IO_OP_READ, 0, 0),
        // Partial blocks.
        (UBLK_IO_OP_READ, 0, 3),
        (UBLK_IO_OP_WRITE, 1, SPB as u32),
        (UBLK_IO_OP_DISCARD, 0, SPB as u32 + 1),
        // Past the end.
        (UBLK_IO_OP_READ, SIZE * SPB, SPB as u32),
        (UBLK_IO_OP_WRITE, (SIZE - 1) * SPB, 2 * SPB as u32),
        // The far end of the sector space, aligned and not.
        (UBLK_IO_OP_READ, u64::MAX - SPB + 1, SPB as u32),
        (UBLK_IO_OP_WRITE_ZEROES, u64::MAX - 3, SPB as u32),
    ];
    for (op, start, n) in cases {
        assert_eq!(
            decode_request(&desc(op, start, n), VOL, SIZE),
            Err(EINVAL),
            "op {op} start {start} sectors {n}"
        );
    }
    // The whole volume in one request is in range.
    assert!(decode_request(&desc(UBLK_IO_OP_READ, 0, (SIZE * SPB) as u32), VOL, SIZE).is_ok());
    // The largest sector count the descriptor can carry is not whole
    // blocks; the largest that is, on a volume that holds it, decodes.
    let huge = u64::from(u32::MAX) * SPB;
    assert_eq!(
        decode_request(&desc(UBLK_IO_OP_READ, 0, u32::MAX), VOL, huge),
        Err(EINVAL)
    );
    let most = u32::MAX - (u32::MAX % SPB as u32);
    assert_eq!(
        decode_request(&desc(UBLK_IO_OP_READ, 0, most), VOL, huge),
        Ok(RequestKind::Read {
            vol: VOL,
            lba: 0,
            blocks: most / SPB as u32
        })
    );
}

#[test]
fn statuses_map_to_results() {
    let read = RequestKind::Read {
        vol: VOL,
        lba: 0,
        blocks: 3,
    };
    let write = RequestKind::Write {
        vol: VOL,
        lba: 0,
        blocks: 1,
    };
    let flush = RequestKind::Flush { vol: VOL };
    let discard = RequestKind::Discard {
        vol: VOL,
        lba: 0,
        blocks: 100,
    };
    let zeroes = RequestKind::WriteZeroes {
        vol: VOL,
        lba: 0,
        blocks: 100,
    };
    assert_eq!(errno_for(Status::Ok, &read), 3 * BLOCK_SIZE as i32);
    assert_eq!(errno_for(Status::Ok, &write), BLOCK_SIZE as i32);
    assert_eq!(errno_for(Status::Ok, &flush), 0);
    assert_eq!(errno_for(Status::Ok, &discard), 0);
    assert_eq!(errno_for(Status::Ok, &zeroes), 0);
    for kind in [read, write, flush, discard, zeroes] {
        assert_eq!(errno_for(Status::NvFailed, &kind), -EIO);
        assert_eq!(errno_for(Status::Corrupt, &kind), -EIO);
        assert_eq!(errno_for(Status::Failed, &kind), -EIO);
        assert_eq!(errno_for(Status::NoSuchVolume, &kind), -ENODEV);
        assert_eq!(errno_for(Status::Stopped, &kind), -ENODEV);
        assert_eq!(errno_for(Status::NoSpace, &kind), -ENOSPC);
        assert_eq!(errno_for(Status::OutOfRange, &kind), -EINVAL);
        assert_eq!(errno_for(Status::TooLarge, &kind), -EINVAL);
    }
    // A byte count past `i32` saturates rather than wrapping negative.
    let vast = RequestKind::Read {
        vol: VOL,
        lba: 0,
        blocks: 1 << 20,
    };
    assert_eq!(errno_for(Status::Ok, &vast), i32::MAX);
}

#[test]
fn request_ids_carry_queue_tag_and_generation() {
    let id = request_id(5, 77, 0xDEAD_BEEF);
    assert_eq!(split_id(id), (5, 77, 0xDEAD_BEEF));
    assert_eq!(id, ReqId(0xDEAD_BEEF_0005_004D));
    assert_eq!(
        split_id(request_id(u16::MAX, u16::MAX, u32::MAX)),
        (u16::MAX, u16::MAX, u32::MAX)
    );
}

#[test]
fn watermarks_for_a_depth() {
    assert_eq!(Watermarks::for_depth(0), Watermarks { high: 1, low: 0 });
    assert_eq!(Watermarks::for_depth(1), Watermarks { high: 1, low: 0 });
    assert_eq!(
        Watermarks::for_depth(128),
        Watermarks { high: 128, low: 64 }
    );
}

#[test]
fn a_write_flows_through() {
    let mut fd = door(4);
    let payload = data(2, 0xAB);
    fd.transport().submit_write(2, 10, payload.clone());
    assert_eq!(fd.ready(), 0);
    let req = fd.fetch().expect("the write");
    assert_eq!(split_id(req.id), (Q, 2, 0));
    assert_eq!(
        req.kind,
        RequestKind::Write {
            vol: VOL,
            lba: 10,
            blocks: 2
        }
    );
    assert_eq!(fd.in_engine(), 1);
    let mut buf = data(2, 0);
    fd.copy_in(&req, &mut buf).unwrap();
    assert_eq!(buf, payload);
    fd.complete(req, Status::Ok, 9);
    assert_eq!(fd.in_engine(), 0);
    assert_eq!(
        fd.transport().take_commits(),
        vec![(2, 2 * BLOCK_SIZE as i32)]
    );
    assert_eq!(fd.transport().outstanding(), 0);
    let s = fd.stats();
    assert_eq!((s.requests, s.writes, s.errors, s.stale), (1, 1, 0, 0));
    // The tag's next request is a new generation.
    fd.transport().submit_write(2, 0, data(1, 1));
    let req = fd.fetch().expect("the second write");
    assert_eq!(split_id(req.id), (Q, 2, 1));
}

#[test]
fn a_read_flows_through() {
    let mut fd = door(4);
    fd.transport().submit_read(0, 100, 3);
    let req = fd.fetch().expect("the read");
    assert_eq!(
        req.kind,
        RequestKind::Read {
            vol: VOL,
            lba: 100,
            blocks: 3
        }
    );
    let payload = data(3, 0x5A);
    fd.copy_out(&req, &payload).unwrap();
    fd.complete(req, Status::Ok, 0);
    assert_eq!(
        fd.transport().take_commits(),
        vec![(0, 3 * BLOCK_SIZE as i32)]
    );
    assert_eq!(fd.transport().read_payload(0), Some(payload));
    assert_eq!(fd.stats().reads, 1);
}

#[test]
fn flush_discard_and_write_zeroes_complete_with_zero() {
    let mut fd = door(4);
    fd.transport().submit_flush(0);
    fd.transport().submit_discard(1, 8, 16);
    fd.transport().submit_write_zeroes(2, 24, 8);
    let a = fd.fetch().unwrap();
    let b = fd.fetch().unwrap();
    let c = fd.fetch().unwrap();
    assert_eq!(a.kind, RequestKind::Flush { vol: VOL });
    assert_eq!(
        b.kind,
        RequestKind::Discard {
            vol: VOL,
            lba: 8,
            blocks: 16
        }
    );
    assert_eq!(
        c.kind,
        RequestKind::WriteZeroes {
            vol: VOL,
            lba: 24,
            blocks: 8
        }
    );
    assert_eq!(fd.in_engine(), 3);
    fd.complete(c, Status::Ok, 0);
    fd.complete(a, Status::Ok, 0);
    fd.complete(b, Status::Ok, 0);
    assert_eq!(fd.transport().take_commits(), vec![(2, 0), (0, 0), (1, 0)]);
    let s = fd.stats();
    assert_eq!((s.flushes, s.discards, s.write_zeroes), (1, 1, 1));
}

#[test]
fn engine_errors_become_errnos() {
    let mut fd = door(8);
    let statuses = [
        (Status::NvFailed, -EIO),
        (Status::Corrupt, -EIO),
        (Status::Failed, -EIO),
        (Status::NoSuchVolume, -ENODEV),
        (Status::Stopped, -ENODEV),
        (Status::NoSpace, -ENOSPC),
        (Status::OutOfRange, -EINVAL),
        (Status::TooLarge, -EINVAL),
    ];
    for (i, _) in statuses.iter().enumerate() {
        fd.transport()
            .submit_write(i as u16, i as u64, data(1, i as u8));
    }
    let reqs: Vec<Request> = (0..statuses.len()).map(|_| fd.fetch().unwrap()).collect();
    for (req, (status, _)) in reqs.into_iter().zip(statuses.iter()) {
        fd.complete(req, *status, 0);
    }
    let commits = fd.transport().take_commits();
    let expected: Vec<(u16, i32)> = statuses
        .iter()
        .enumerate()
        .map(|(i, (_, errno))| (i as u16, *errno))
        .collect();
    assert_eq!(commits, expected);
    assert_eq!(fd.stats().errors, statuses.len() as u64);
}

#[test]
fn unsupported_and_malformed_requests_never_reach_the_engine() {
    let mut fd = door(4);
    fd.transport()
        .submit_raw(0, desc(UBLK_IO_OP_WRITE_SAME, 0, SPB as u32));
    fd.transport()
        .submit_raw(1, desc(UBLK_IO_OP_READ, 1, SPB as u32));
    fd.transport()
        .submit_raw(2, desc(UBLK_IO_OP_WRITE, SIZE * SPB, SPB as u32));
    assert!(fd.fetch().is_none());
    assert_eq!(
        fd.transport().take_commits(),
        vec![(0, -EOPNOTSUPP), (1, -EINVAL), (2, -EINVAL)]
    );
    let s = fd.stats();
    assert_eq!((s.requests, s.refused, s.reads, s.writes), (3, 3, 0, 0));
    assert_eq!(fd.in_engine(), 0);
    // The tags are idle again.
    fd.transport().submit_read(0, 0, 1);
    assert!(fd.fetch().is_some());
}

#[test]
fn offline_answers_enodev_until_back_online() {
    let mut fd = door(4);
    assert!(!fd.is_offline());
    fd.set_offline(true);
    assert!(fd.is_offline());
    fd.transport().submit_read(0, 0, 1);
    fd.transport().submit_write(1, 0, data(1, 1));
    fd.transport().submit_flush(2);
    assert!(fd.fetch().is_none());
    assert_eq!(
        fd.transport().take_commits(),
        vec![(0, -ENODEV), (1, -ENODEV), (2, -ENODEV)]
    );
    assert_eq!(fd.stats().offline_rejected, 3);
    // Requests already in the engine finish normally.
    fd.set_offline(false);
    fd.transport().submit_read(3, 0, 1);
    let req = fd.fetch().unwrap();
    fd.set_offline(true);
    fd.copy_out(&req, &data(1, 7)).unwrap();
    fd.complete(req, Status::Ok, 0);
    assert_eq!(fd.transport().take_commits(), vec![(3, BLOCK_SIZE as i32)]);
    fd.set_offline(false);
    fd.transport().submit_read(0, 0, 1);
    assert!(fd.fetch().is_some());
}

#[test]
fn an_aborted_tag_drops_its_completion_and_is_never_fetched_again() {
    let mut fd = door(4);
    fd.transport().submit_write(1, 0, data(1, 1));
    let req = fd.fetch().unwrap();
    assert_eq!(fd.in_engine(), 1);
    fd.transport().abort(1);
    assert_eq!(fd.pump().unwrap(), 1);
    assert_eq!(fd.in_engine(), 0, "the engine's copy no longer counts");
    assert_eq!(fd.aborted_tags(), vec![1]);
    // The engine still holds the request: its copies and completion are
    // refused as stale, and nothing is committed.
    let mut buf = data(1, 0);
    assert_eq!(fd.copy_in(&req, &mut buf), Err(IoError::Corrupt));
    assert_eq!(fd.copy_out(&req, &buf), Err(IoError::Corrupt));
    fd.complete(req, Status::Ok, 0);
    assert!(fd.transport().take_commits().is_empty());
    assert_eq!(fd.stats().stale, 1);
    assert_eq!(fd.stats().aborted, 1);
    // The driver has no fetch pending on an aborted tag, so it delivers
    // nothing there; were one to arrive, it is answered `EINVAL`, the
    // queue (owing nothing on the tag) refuses the commit, and the tag
    // stays retired.
    fd.transport().inject(otap_ublk::QueueEvent::Request {
        tag: 1,
        desc: desc(UBLK_IO_OP_READ, 0, SPB as u32),
    });
    assert!(fd.fetch().is_none());
    assert!(fd.transport().take_commits().is_empty());
    assert_eq!(fd.stats().refused, 1);
    assert_eq!(fd.stats().commit_failures, 1);
    assert_eq!(fd.aborted_tags(), vec![1]);
    // An abort of a tag that is merely ready un-readies it.
    fd.transport().submit_read(2, 0, 1);
    fd.pump().unwrap();
    assert_eq!(fd.ready(), 1);
    fd.transport().abort(2);
    fd.pump().unwrap();
    assert_eq!(fd.ready(), 0);
    assert!(fd.fetch().is_none());
    assert_eq!(fd.aborted_tags(), vec![1, 2]);
    // An abort of an idle tag just retires it.
    fd.transport().abort(3);
    fd.pump().unwrap();
    assert_eq!(fd.aborted_tags(), vec![1, 2, 3]);
    assert_eq!(fd.stats().aborted, 3);
}

#[test]
fn a_completion_for_an_earlier_generation_is_stale() {
    let mut fd = door(4);
    fd.transport().submit_write(0, 0, data(1, 1));
    let first = fd.fetch().unwrap();
    fd.complete(first, Status::Ok, 0);
    fd.transport().take_commits();
    fd.transport().submit_write(0, 0, data(1, 2));
    let second = fd.fetch().unwrap();
    assert_ne!(first.id, second.id);
    // The old id again: refused everywhere.
    let mut buf = data(1, 0);
    assert_eq!(fd.copy_in(&first, &mut buf), Err(IoError::Corrupt));
    fd.complete(first, Status::Ok, 0);
    assert_eq!(fd.stats().stale, 1);
    assert!(fd.transport().take_commits().is_empty());
    assert_eq!(fd.in_engine(), 1);
    // The current one is fine.
    fd.copy_in(&second, &mut buf).unwrap();
    assert_eq!(buf, data(1, 2));
    fd.complete(second, Status::Ok, 0);
    assert_eq!(fd.transport().take_commits(), vec![(0, BLOCK_SIZE as i32)]);
    // Ids from another queue, an unknown tag, or a wrong generation are all
    // stale.
    for id in [
        request_id(Q + 1, 0, 2),
        request_id(Q, 9, 0),
        request_id(Q, 0, 7),
    ] {
        let forged = Request {
            id,
            kind: RequestKind::Flush { vol: VOL },
        };
        assert_eq!(fd.copy_out(&forged, &buf), Err(IoError::Corrupt));
        fd.complete(forged, Status::Ok, 0);
    }
    assert_eq!(fd.stats().stale, 4);
    assert!(fd.transport().take_commits().is_empty());
}

#[test]
fn copies_check_the_request() {
    let mut fd = door(4);
    fd.transport().submit_write(0, 0, data(2, 1));
    fd.transport().submit_read(1, 0, 1);
    let write = fd.fetch().unwrap();
    let read = fd.fetch().unwrap();
    let mut short = data(1, 0);
    assert_eq!(
        fd.copy_in(&write, &mut short),
        Err(IoError::BadLength { bytes: BLOCK_SIZE })
    );
    assert_eq!(fd.copy_in(&read, &mut short), Err(IoError::Corrupt));
    let mut right = data(2, 0);
    fd.copy_in(&write, &mut right).unwrap();
    assert_eq!(right, data(2, 1));
    // A transport copy failure comes through as the device's error.
    fd.transport().fail_copies(true);
    assert_eq!(
        fd.copy_in(&write, &mut right),
        Err(IoError::Device { errno: 5 })
    );
    assert_eq!(
        fd.copy_out(&read, &short),
        Err(IoError::Device { errno: 5 })
    );
    fd.transport().fail_copies(false);
    fd.copy_out(&read, &short).unwrap();
}

#[test]
fn the_watermarks_gate_the_engine() {
    let depth = 8;
    let mut fd = UblkFrontDoor::new(
        MockTransport::new(depth),
        Q,
        VOL,
        SIZE,
        depth,
        Watermarks { high: 4, low: 2 },
    );
    assert_eq!(fd.watermarks(), Watermarks { high: 4, low: 2 });
    for tag in 0..depth {
        fd.transport().submit_read(tag, 0, 1);
    }
    let mut held: Vec<Request> = (0..4).map(|_| fd.fetch().unwrap()).collect();
    assert_eq!(fd.in_engine(), 4);
    assert_eq!(fd.ready(), 4);
    assert!(fd.fetch().is_none(), "the high watermark closes the gate");
    assert_eq!(fd.stats().gate_closed, 1);
    let req = held.pop().unwrap();
    fd.copy_out(&req, &data(1, 0)).unwrap();
    fd.complete(req, Status::Ok, 0);
    assert_eq!(fd.in_engine(), 3);
    assert!(
        fd.fetch().is_none(),
        "above the low watermark the gate stays closed"
    );
    let req = held.pop().unwrap();
    fd.copy_out(&req, &data(1, 0)).unwrap();
    fd.complete(req, Status::Ok, 0);
    assert_eq!(fd.in_engine(), 2);
    assert!(
        fd.fetch().is_some(),
        "at the low watermark fetching resumes"
    );
    assert!(fd.fetch().is_some());
    assert!(fd.fetch().is_none(), "closed again at the high watermark");
    assert_eq!(fd.stats().gate_closed, 2);
    assert_eq!(fd.transport().take_commits().len(), 2);
}

#[test]
fn a_depth_of_zero_serves_nothing() {
    // `for_depth(0)` lets one request through; the tag table is empty, so
    // every request the mock could deliver is beyond it and ignored.
    let mut fd = UblkFrontDoor::new(
        MockTransport::new(2),
        Q,
        VOL,
        SIZE,
        0,
        Watermarks::for_depth(0),
    );
    fd.transport().submit_read(0, 0, 1);
    assert!(fd.fetch().is_none());
    assert_eq!(fd.stats().requests, 1);
    assert!(fd.transport().take_commits().is_empty());
    assert_eq!(fd.transport().outstanding(), 1);
}

#[test]
fn a_tag_beyond_the_depth_is_ignored() {
    let mut fd = UblkFrontDoor::new(
        MockTransport::new(16),
        Q,
        VOL,
        SIZE,
        8,
        Watermarks::for_depth(8),
    );
    fd.transport().submit_read(12, 0, 1);
    fd.transport().abort(13);
    assert!(fd.fetch().is_none());
    assert!(fd.transport().take_commits().is_empty());
    assert_eq!(fd.stats().requests, 1);
    assert_eq!(fd.stats().aborted, 0);
    assert!(fd.aborted_tags().is_empty());
}

#[test]
fn a_request_on_a_busy_tag_is_refused() {
    // The driver never delivers a request on a tag whose request is still
    // out; if it did, the front door answers the duplicate `EINVAL` rather
    // than lose it, and the real request's completion then finds the tag
    // owed nothing and retires it.
    let mut fd = door(4);
    fd.transport().submit_read(0, 0, 1);
    let req = fd.fetch().unwrap();
    fd.transport().inject(otap_ublk::QueueEvent::Request {
        tag: 0,
        desc: desc(UBLK_IO_OP_READ, 0, SPB as u32),
    });
    fd.pump().unwrap();
    assert_eq!(fd.transport().take_commits(), vec![(0, -EINVAL)]);
    assert_eq!(fd.stats().refused, 1);
    assert_eq!(fd.in_engine(), 1, "the real request is still out");
    // That answer completed the tag as far as the driver is concerned:
    // the real request's data copy fails, the engine fails the request,
    // and the commit is refused.
    assert_eq!(fd.copy_out(&req, &data(1, 0)), Err(IoError::Corrupt));
    fd.complete(req, Status::Failed, 0);
    assert!(fd.transport().take_commits().is_empty());
    assert_eq!(fd.stats().commit_failures, 1);
    assert_eq!(fd.aborted_tags(), vec![0]);
    assert_eq!(fd.in_engine(), 0);
    // A duplicate on a ready (unfetched) tag is refused the same way.
    fd.transport().submit_read(1, 0, 1);
    fd.pump().unwrap();
    fd.transport().inject(otap_ublk::QueueEvent::Request {
        tag: 1,
        desc: desc(UBLK_IO_OP_READ, 0, SPB as u32),
    });
    fd.pump().unwrap();
    assert_eq!(fd.transport().take_commits(), vec![(1, -EINVAL)]);
    assert_eq!(fd.stats().refused, 2);
    assert_eq!(fd.ready(), 1);
}

#[test]
fn an_abort_withdraws_an_unpumped_request() {
    let mut fd = door(4);
    fd.transport().submit_write(0, 0, data(1, 1));
    fd.transport().abort(0);
    assert_eq!(fd.pump().unwrap(), 1, "only the abort arrives");
    assert_eq!(fd.ready(), 0);
    assert_eq!(fd.aborted_tags(), vec![0]);
    assert_eq!(fd.stats().requests, 0);
    assert!(fd.transport().take_commits().is_empty());
    assert_eq!(fd.transport().outstanding(), 0);
}

#[test]
fn a_failed_commit_retires_the_tag() {
    let mut fd = door(4);
    fd.transport().submit_read(0, 0, 1);
    let req = fd.fetch().unwrap();
    fd.transport().fail_commits(true);
    fd.copy_out(&req, &data(1, 0)).unwrap();
    fd.complete(req, Status::Ok, 0);
    assert_eq!(fd.stats().commit_failures, 1);
    assert_eq!(fd.last_error().map(|e| e.errno), Some(19));
    assert_eq!(fd.aborted_tags(), vec![0]);
    assert_eq!(fd.in_engine(), 0);
    fd.transport().fail_commits(false);
    // The failed tag is never fetched again; other tags still work.
    fd.transport().submit_read(1, 0, 1);
    let req = fd.fetch().unwrap();
    assert_eq!(split_id(req.id).1, 1);
    fd.complete(req, Status::Ok, 0);
    assert_eq!(fd.transport().take_commits(), vec![(1, BLOCK_SIZE as i32)]);
    // A refused commit of a direct answer retires the tag too.
    fd.transport().fail_commits(true);
    fd.transport()
        .submit_raw(2, desc(UBLK_IO_OP_WRITE_SAME, 0, SPB as u32));
    assert!(fd.fetch().is_none());
    assert_eq!(fd.stats().commit_failures, 2);
    assert_eq!(fd.aborted_tags(), vec![0, 2]);
}

#[test]
fn a_failed_poll_is_reported_and_retried() {
    let mut fd = door(4);
    fd.transport().fail_next_poll();
    assert!(fd.pump().is_err());
    assert_eq!(fd.last_error().map(|e| e.what), Some("poll failed"));
    fd.transport().submit_read(0, 0, 1);
    // `fetch` swallows the error and tries again next time.
    fd.transport().fail_next_poll();
    assert!(fd.fetch().is_none());
    assert!(fd.fetch().is_some());
    assert!(fd.transport().polls() >= 3);
    assert_eq!(fd.ready(), 0);
}

#[test]
fn fetch_serves_ready_requests_before_polling_again() {
    let mut fd = door(4);
    fd.transport().submit_read(0, 0, 1);
    fd.transport().submit_read(1, 0, 1);
    fd.pump().unwrap();
    let polls = fd.transport().polls();
    assert_eq!(fd.ready(), 2);
    assert!(fd.fetch().is_some());
    assert_eq!(fd.transport().polls(), polls, "ready requests need no poll");
    assert!(fd.fetch().is_some());
    assert!(fd.fetch().is_none());
    assert_eq!(fd.transport().polls(), polls + 1);
}

#[test]
fn debug_output_names_the_queue() {
    let fd = door(4);
    let text = format!("{fd:?}");
    assert!(
        text.contains("UblkFrontDoor") && text.contains("q_id: 0") && text.contains("depth: 4")
    );
    let event = otap_ublk::QueueEvent::Abort { tag: 1 };
    assert_eq!(format!("{event:?}"), "Abort { tag: 1 }");
    let err = otap_ublk::TransportError {
        what: "x",
        errno: 5,
    };
    assert_eq!(err.to_string(), "x (errno 5)");
}
