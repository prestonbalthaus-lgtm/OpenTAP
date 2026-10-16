//! Opus-QA-Adversarial, Step 5: the control protocol's framing and decoding
//! under malformed and hostile input. The review's one `bug_*` test
//! reproduced defect R3 of the Step 5 entry of `08-engineering-log.md` and
//! is ported in place with the fixed behaviour as its expectation; the
//! others pin behaviour that was right as reviewed.

use std::io::{self, Read};

use otap_control::{
    decode_hex, encode_hex, parse_size, read_frame, write_frame, Reply, Request, Response,
    WireError, BLOCK_SIZE, MAX_FRAME_BYTES, MAX_IO_BLOCKS,
};

/// A reader that hands out one byte per call, as a slow socket may.
struct Trickle {
    bytes: Vec<u8>,
    at: usize,
}

impl Read for Trickle {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if self.at == self.bytes.len() || buf.is_empty() {
            return Ok(0);
        }
        buf[0] = self.bytes[self.at];
        self.at += 1;
        Ok(1)
    }
}

fn frame(body: &[u8]) -> Vec<u8> {
    let mut out = (body.len() as u32).to_le_bytes().to_vec();
    out.extend_from_slice(body);
    out
}

/// An optional field's absence means something (`name: None` releases the
/// replication base), so a misspelled field name must not decode as its
/// absence.
#[test]
fn a_misspelled_optional_field_is_refused() {
    let body = br#"{"op":"snapshot_replication_base","volume":"data","nmae":"s1"}"#;
    let mut cursor = io::Cursor::new(frame(body));
    let decoded = read_frame::<Request, _>(&mut cursor);
    // Review defect R3 (Step 5, `08-engineering-log.md`), fixed; the review's
    // expectation stood and the scenario is kept. As reviewed: an unknown field must be refused (`#[serde(deny_unknown_fields)]`
    // on the request variants); instead the request decodes as
    // `SnapshotReplicationBase { name: None }`, which the daemon executes
    // as "release the volume's replication base", the opposite of what
    // the client asked.
    assert!(decoded.is_err(), "decoded as {:?}", decoded.ok().flatten());
}

/// Frames arrive whole however the stream splits them; a clean close
/// between frames is `None`, inside one is `Closed`.
#[test]
fn frames_survive_a_stream_that_splits_every_byte() {
    let mut bytes = Vec::new();
    write_frame(&mut bytes, &Request::Ping {}).unwrap();
    write_frame(
        &mut bytes,
        &Request::IoRead {
            volume: "v".into(),
            lba: u64::MAX,
            blocks: u32::MAX,
        },
    )
    .unwrap();
    let whole = bytes.len();
    bytes.extend_from_slice(&frame(br#"{"op":"pi"#)[..6]);
    let mut r = Trickle { bytes, at: 0 };
    assert_eq!(
        read_frame::<Request, _>(&mut r).unwrap(),
        Some(Request::Ping {})
    );
    assert_eq!(
        read_frame::<Request, _>(&mut r).unwrap(),
        Some(Request::IoRead {
            volume: "v".into(),
            lba: u64::MAX,
            blocks: u32::MAX,
        })
    );
    assert!(r.at == whole);
    assert!(matches!(
        read_frame::<Request, _>(&mut r),
        Err(WireError::Closed)
    ));
    let mut empty = Trickle {
        bytes: Vec::new(),
        at: 0,
    };
    assert!(read_frame::<Request, _>(&mut empty).unwrap().is_none());
}

/// Hostile bodies decode to errors, never to a panic: empty, not JSON, not
/// UTF-8, deeply nested, numbers out of their type's range, a wrong tag.
#[test]
fn hostile_bodies_are_decode_errors() {
    let deep = "[".repeat(10_000) + &"]".repeat(10_000);
    let bodies: Vec<Vec<u8>> = vec![
        Vec::new(),
        b"null".to_vec(),
        vec![0xff, 0xfe, 0xfd],
        deep.into_bytes(),
        br#"{"op":"io_read","volume":"v","lba":18446744073709551616,"blocks":1}"#.to_vec(),
        br#"{"op":"io_read","volume":"v","lba":1,"blocks":4294967296}"#.to_vec(),
        br#"{"op":"snapshot_pin","volume":"v","name":"s","flags":256}"#.to_vec(),
        br#"{"op":"Ping"}"#.to_vec(),
        br#"{"kind":"ok"}"#.to_vec(),
    ];
    for body in bodies {
        let mut cursor = io::Cursor::new(frame(&body));
        match read_frame::<Request, _>(&mut cursor) {
            Err(WireError::Decode(_)) => {}
            other => panic!("{:?} gave {other:?}", String::from_utf8_lossy(&body)),
        }
    }
    // A reply that is not one is refused by the client's decoder too.
    let mut cursor = io::Cursor::new(frame(br#"{"Ok":{"kind":"nonsense"}}"#));
    assert!(matches!(
        read_frame::<Reply, _>(&mut cursor),
        Err(WireError::Decode(_))
    ));
}

/// The bound is checked before the body is read: a length past it is
/// refused at once, and the largest legitimate request (an `IoWrite` of
/// `MAX_IO_BLOCKS` blocks as hexadecimal) fits under it.
#[test]
fn the_frame_bound_holds_and_the_largest_write_fits() {
    let mut cursor = io::Cursor::new(u32::MAX.to_le_bytes().to_vec());
    assert!(matches!(
        read_frame::<Request, _>(&mut cursor),
        Err(WireError::TooLong { bytes: u32::MAX })
    ));
    let big = Request::IoWrite {
        volume: "v".repeat(255),
        lba: u64::MAX,
        data_hex: encode_hex(&vec![0xab; MAX_IO_BLOCKS as usize * BLOCK_SIZE]),
    };
    let mut out = Vec::new();
    write_frame(&mut out, &big).unwrap();
    assert!(out.len() < MAX_FRAME_BYTES as usize);
    let back: Request = read_frame(&mut io::Cursor::new(out)).unwrap().unwrap();
    assert_eq!(back, big);
    let ok: Reply = Ok(Response::Ok {});
    let mut out = Vec::new();
    write_frame(&mut out, &ok).unwrap();
    assert_eq!(
        read_frame::<Reply, _>(&mut io::Cursor::new(out)).unwrap(),
        Some(ok)
    );
}

/// Hexadecimal and sizes refuse what they cannot represent.
#[test]
fn hex_and_sizes_refuse_what_they_cannot_represent() {
    assert!(decode_hex("é0").is_none(), "a multibyte character");
    assert!(decode_hex("0x").is_none());
    assert_eq!(decode_hex("00FFaa").unwrap(), vec![0, 255, 170]);
    assert!(parse_size("16384P").is_err());
    assert!(parse_size("18446744073709551616").is_err());
    assert!(parse_size("17179869184G").is_err(), "2^64 bytes");
    assert_eq!(parse_size("16777215T").unwrap(), 16_777_215u64 << 40);
    assert!(parse_size("1 G B").is_err());
    assert!(parse_size("+1G").is_err());
}
