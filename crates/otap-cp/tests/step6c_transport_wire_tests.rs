//! Step 6C: the replication link's framing, its messages and the sink's
//! durable record, byte for byte: what goes through comes out whole and in
//! order however the bytes are split; garbage, truncation, an oversize
//! length and a flipped bit are refused, and a refusal poisons the reader;
//! a handshake of another version is told by its version first; a record
//! that does not verify is never believed.

use otap_cp::repl::wire::{peek_kind, MAX_HANDSHAKE_NAME, MAX_REFUSAL_TEXT};
use otap_cp::repl::{
    delta_nonce, encode_frame, pipe_id, FrameReader, Handshake, Message, Refusal, RefuseCode, Role,
    SinkRecord, WireError, LINK_HEADER_BYTES, LINK_MAGIC, MAX_LINK_PAYLOAD, NO_VOLUME,
    REPL_PROTOCOL_VERSION,
};
use otap_format::{FrameKind, Uuid};
use otap_stream::{Cursor, Encoder, Frame, FrameMeta, Ping};
use proptest::prelude::*;

fn uuid(b: u8) -> Uuid {
    Uuid::from_bytes([b; 16])
}

fn handshake() -> Handshake {
    let mut h = Handshake::new(Role::Source, false, uuid(1));
    h.volume_index = 3;
    h.snapshot = uuid(2);
    h.snapshot_cp = 77;
    h.base = uuid(4);
    h.base_cp = 41;
    h.volume = "data".into();
    h.peer_volume = "mirror".into();
    h.snapshot_name = "nightly-1".into();
    h
}

#[test]
fn a_frame_is_magic_length_crc_payload() {
    let f = encode_frame(b"abc").expect("encodes");
    assert_eq!(f.len(), LINK_HEADER_BYTES + 3);
    assert_eq!(&f[0..4], &LINK_MAGIC.to_le_bytes());
    assert_eq!(&f[4..8], &3u32.to_le_bytes());
    assert_eq!(
        &f[8..12],
        &otap_format::checksum::crc32c(b"abc").to_le_bytes()
    );
    assert_eq!(&f[12..], b"abc");
    let empty = encode_frame(b"").expect("an empty payload is a frame");
    let mut r = FrameReader::new();
    r.feed(&empty);
    assert_eq!(r.next_payload(), Ok(Some(Vec::new())));
    assert!(matches!(
        encode_frame(&vec![0u8; MAX_LINK_PAYLOAD + 1]),
        Err(WireError::TooLarge { .. })
    ));
}

#[test]
fn garbage_poisons_the_reader() {
    let mut r = FrameReader::new();
    r.feed(b"GET / HTTP/1.1\r\n\r\n");
    let e = r.next_payload().expect_err("refused");
    assert!(matches!(e, WireError::BadMagic { .. }), "{e:?}");
    assert!(e.to_string().contains("replication magic"));
    // Poisoned: the same error, whatever comes next.
    r.feed(&encode_frame(b"fine").expect("encodes"));
    assert_eq!(r.next_payload(), Err(e.clone()));
    assert_eq!(r.finish(), Err(e));
    assert_eq!(r.pending(), 0, "nothing more is buffered");
}

#[test]
fn an_oversize_length_is_refused_at_the_header() {
    let mut header = Vec::new();
    header.extend_from_slice(&LINK_MAGIC.to_le_bytes());
    header.extend_from_slice(&u32::MAX.to_le_bytes());
    header.extend_from_slice(&0u32.to_le_bytes());
    let mut r = FrameReader::new();
    r.feed(&header);
    assert!(matches!(
        r.next_payload(),
        Err(WireError::TooLarge { len, max }) if len == u32::MAX as usize && max == MAX_LINK_PAYLOAD
    ));
}

#[test]
fn a_flipped_bit_is_refused() {
    let mut f = encode_frame(b"payload").expect("encodes");
    f[LINK_HEADER_BYTES + 2] ^= 0x10;
    let mut r = FrameReader::new();
    r.feed(&f);
    assert!(matches!(r.next_payload(), Err(WireError::BadCrc { .. })));
    let mut f = encode_frame(b"payload").expect("encodes");
    f[9] ^= 1;
    let mut r = FrameReader::new();
    r.feed(&f);
    assert!(matches!(r.next_payload(), Err(WireError::BadCrc { .. })));
}

#[test]
fn a_truncated_frame_is_reported_at_the_end() {
    let f = encode_frame(&[7u8; 100]).expect("encodes");
    let mut r = FrameReader::new();
    r.feed(&f[..60]);
    assert_eq!(r.next_payload(), Ok(None));
    assert_eq!(
        r.finish(),
        Err(WireError::Truncated {
            have: 60,
            need: 112
        })
    );
    let mut r = FrameReader::new();
    r.feed(&f[..5]);
    assert_eq!(r.finish(), Err(WireError::Truncated { have: 5, need: 12 }));
    let mut r = FrameReader::new();
    r.feed(&f);
    assert!(r.next_payload().expect("whole").is_some());
    assert_eq!(r.finish(), Ok(()), "an end between frames is clean");
}

#[test]
fn messages_round_trip() {
    let h = handshake();
    let msg = Message::Handshake(h.clone());
    assert_eq!(Message::decode(&msg.encode().unwrap()), Ok(msg));
    let mut answer = Handshake::new(Role::Sink, true, uuid(9));
    answer.volume_index = NO_VOLUME;
    let msg = Message::Handshake(answer);
    assert_eq!(Message::decode(&msg.encode().unwrap()), Ok(msg));
    let r = Message::Refuse(Refusal::new(RefuseCode::Conflict, "the volume is online"));
    assert_eq!(Message::decode(&r.encode().unwrap()), Ok(r));
    let frame = Encoder::new()
        .encode(
            &FrameMeta::default(),
            &Frame::Ping(Ping {
                nonce: 5,
                mono_ns: 6,
            }),
        )
        .expect("a ping");
    let s = Message::Stream(frame.clone());
    let payload = s.encode().unwrap();
    assert_eq!(peek_kind(&payload), (3, Some(FrameKind::Ping.as_u8())));
    assert_eq!(Message::decode(&payload), Ok(s));
    assert_eq!(
        peek_kind(&Message::Handshake(h).encode().unwrap()),
        (1, None)
    );
}

#[test]
fn malformed_messages_are_refused_by_field() {
    assert!(matches!(
        Message::decode(&[]),
        Err(WireError::BadMessage {
            field: "message_len",
            ..
        })
    ));
    assert!(matches!(
        Message::decode(&[9, 1, 2]),
        Err(WireError::BadMessage {
            field: "message",
            ..
        })
    ));
    assert!(matches!(
        Message::decode(&[3, 0, 0]),
        Err(WireError::BadMessage {
            field: "stream_len",
            ..
        })
    ));
    let good = Message::Handshake(handshake()).encode().unwrap();
    // Reserved bytes, a role, the accept flag, the names' lengths.
    for (at, field) in [
        (1 + 30, "rsvd0"),
        (1 + 90, "rsvd1"),
        (1 + 6, "role"),
        (1 + 7, "accept"),
    ] {
        let mut bad = good.clone();
        bad[at] = 0x77;
        assert!(
            matches!(Message::decode(&bad), Err(WireError::BadMessage { field: f, .. }) if f == field),
            "{field}: {:?}",
            Message::decode(&bad)
        );
    }
    let mut short = good.clone();
    short.truncate(good.len() - 1);
    assert!(Message::decode(&short).is_err());
    let mut long = good.clone();
    long.push(0);
    assert!(Message::decode(&long).is_err());
    let mut h = handshake();
    h.volume = "x".repeat(MAX_HANDSHAKE_NAME + 1);
    assert!(Message::Handshake(h).encode().is_err());
    let refusal = Refusal::new(RefuseCode::Internal, "é".repeat(MAX_REFUSAL_TEXT));
    assert!(refusal.text.len() <= MAX_REFUSAL_TEXT);
    assert!(refusal.text.chars().all(|c| c == 'é'), "cut at a character");
}

#[test]
fn a_handshake_of_another_version_is_told_by_its_version() {
    let mut h = handshake();
    h.version = REPL_PROTOCOL_VERSION + 7;
    let payload = Message::Handshake(h).encode().unwrap();
    assert_eq!(
        Handshake::peek_version(&payload[1..]),
        Ok(REPL_PROTOCOL_VERSION + 7)
    );
    assert!(matches!(
        Message::decode(&payload),
        Err(WireError::BadMessage {
            field: "version",
            ..
        })
    ));
    assert!(Handshake::peek_version(b"OTRX\x01\x00").is_err());
    assert!(Handshake::peek_version(b"OT").is_err());
}

#[test]
fn identities_are_stable_and_distinct() {
    assert_eq!(delta_nonce(uuid(1), uuid(2)), delta_nonce(uuid(1), uuid(2)));
    assert_ne!(delta_nonce(uuid(1), uuid(2)), delta_nonce(uuid(2), uuid(1)));
    assert_ne!(delta_nonce(uuid(1), Uuid::NIL), 0);
    assert_eq!(
        pipe_id(uuid(1), 0, uuid(2), 1),
        pipe_id(uuid(1), 0, uuid(2), 1)
    );
    assert_ne!(
        pipe_id(uuid(1), 0, uuid(2), 1),
        pipe_id(uuid(1), 0, uuid(2), 2)
    );
    assert!(!pipe_id(uuid(1), 0, uuid(2), 1).is_nil());
}

fn record() -> SinkRecord {
    let mut rec = SinkRecord::new(uuid(5), 2);
    rec.src_agg = uuid(6);
    rec.src_vol = 0;
    rec.base = uuid(7);
    rec.dirty = true;
    rec.landing_target = uuid(8);
    rec.landing_from = uuid(7);
    rec.pool_cp = 91;
    rec.cursor = Cursor::new(40, 1, 0);
    rec.cursor.begin(40, 52, 0xabc, 30).expect("a delta begins");
    for idx in [0, 1, 2, 5, 6, 9] {
        rec.cursor.land(52, 0xabc, idx).expect("lands");
    }
    rec
}

#[test]
fn a_sink_record_round_trips_and_refuses_damage() {
    let rec = record();
    let bytes = rec.encode().expect("encodes");
    assert_eq!(bytes.len(), otap_cp::repl::record::RECORD_BYTES);
    let back = SinkRecord::decode(&bytes).expect("decodes");
    assert_eq!(back, rec);
    let landing = back.cursor.landing.as_ref().expect("the landing");
    assert_eq!(landing.bitmap.landed_contig(), 3);
    assert_eq!(landing.bitmap.landed_count(), 6);
    // Credits are a connection's, never a record's.
    let mut with_credits = rec.clone();
    with_credits.cursor.credits = 100;
    let back = SinkRecord::decode(&with_credits.encode().unwrap()).unwrap();
    assert_eq!(back.cursor.credits, 0);
    // A flipped bit anywhere is refused.
    for at in [0, 7, 30, 100, 200, 4000, bytes.len() - 1] {
        let mut bad = bytes.clone();
        bad[at] ^= 0x01;
        assert!(SinkRecord::decode(&bad).is_err(), "byte {at}");
    }
    assert!(SinkRecord::decode(&bytes[..bytes.len() - 1]).is_err());
    assert_eq!(SinkRecord::key(2), "sink-2");
}

proptest! {
    /// However the stream is cut into reads, the payloads come out whole,
    /// in order, and nothing is left over.
    #[test]
    fn frames_survive_any_split(
        payloads in prop::collection::vec(prop::collection::vec(any::<u8>(), 0..2000), 1..12),
        cuts in prop::collection::vec(1usize..700, 1..40),
    ) {
        let mut stream = Vec::new();
        for p in &payloads {
            stream.extend_from_slice(&encode_frame(p).unwrap());
        }
        let mut r = FrameReader::new();
        let mut out = Vec::new();
        let mut at = 0;
        let mut i = 0;
        while at < stream.len() {
            let n = cuts[i % cuts.len()].min(stream.len() - at);
            r.feed(&stream[at..at + n]);
            at += n;
            i += 1;
            while let Some(p) = r.next_payload().unwrap() {
                out.push(p);
            }
        }
        prop_assert_eq!(&out, &payloads, "the payloads in order");
        prop_assert_eq!(r.finish(), Ok(()), "nothing left over");
        prop_assert_eq!(r.frames(), payloads.len() as u64, "every frame counted");
    }

    /// A handshake of any identities and names round-trips.
    #[test]
    fn handshakes_round_trip(
        source in any::<bool>(),
        accept in any::<bool>(),
        ids in any::<[u8; 48]>(),
        index in any::<u32>(),
        cps in any::<(u64, u64)>(),
        names in ("[a-z0-9#._-]{0,40}", "[a-z0-9#._-]{0,40}", "[a-z0-9#._-]{0,40}"),
    ) {
        let id = |k: usize| {
            let mut b = [0u8; 16];
            b.copy_from_slice(&ids[k * 16..k * 16 + 16]);
            Uuid::from_bytes(b)
        };
        let role = if source { Role::Source } else { Role::Sink };
        let mut h = Handshake::new(role, accept, id(0));
        h.volume_index = index;
        h.snapshot = id(1);
        h.snapshot_cp = cps.0;
        h.base = id(2);
        h.base_cp = cps.1;
        h.volume = names.0;
        h.peer_volume = names.1;
        h.snapshot_name = names.2;
        let msg = Message::Handshake(h);
        let payload = msg.encode().unwrap();
        prop_assert_eq!(Message::decode(&payload), Ok(msg), "round trip");
    }

    /// Any single flipped bit of a frame is refused by its CRC or its
    /// header checks; none slips through as another payload.
    #[test]
    fn a_flipped_bit_never_passes(
        payload in prop::collection::vec(any::<u8>(), 1..300),
        bit in any::<u32>(),
    ) {
        let mut frame = encode_frame(&payload).unwrap();
        let bit = bit as usize % (frame.len() * 8);
        frame[bit / 8] ^= 1 << (bit % 8);
        let mut r = FrameReader::new();
        r.feed(&frame);
        match r.next_payload() {
            Err(_) => {}
            Ok(None) => prop_assert!(r.finish().is_err(), "a length grown past the bytes"),
            Ok(Some(p)) => prop_assert!(false, "a damaged frame passed as {} bytes", p.len()),
        }
    }
}
