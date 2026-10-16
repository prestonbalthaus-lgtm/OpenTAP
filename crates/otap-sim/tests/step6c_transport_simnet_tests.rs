//! Step 6C: the simulated network itself: ordered delivery after the
//! delay, and what each fault does to the bytes and to both ends (a drop
//! loses the frame and what follows and resets the connection, a cut
//! delivers a prefix and the receiver reports the truncation, a
//! corruption is refused by the receiver's CRC), refused dials, a full
//! queue, a pulled cable, and the random faults' budget.

use otap_cp::{Dialer, Link, LinkError, LinkListener};
use otap_sim::{Direction, FaultKind, FaultRule, SimNet};

fn pump(net: &SimNet, ticks: u32) {
    for _ in 0..ticks {
        net.tick();
    }
}

fn drain(link: &mut dyn Link) -> (Vec<Vec<u8>>, Option<String>) {
    let mut got = Vec::new();
    loop {
        match link.recv() {
            Ok(Some(p)) => got.push(p),
            Ok(None) => return (got, None),
            Err(LinkError::Closed(why)) => return (got, Some(why)),
            Err(e) => panic!("{e:?}"),
        }
    }
}

fn connected(net: &SimNet) -> (Box<dyn Link>, Box<dyn Link>) {
    let mut listener = net.listen("there");
    let a = net.dialer().dial("there").expect("dials");
    let b = listener.accept().expect("accepted");
    (a, b)
}

#[test]
fn frames_arrive_in_order_after_the_delay() {
    let net = SimNet::new(1);
    net.set_delay(3);
    let (mut a, mut b) = connected(&net);
    for m in [&b"one"[..], b"two", b"three"] {
        a.send(m).expect("sent");
    }
    pump(&net, 2);
    assert!(drain(&mut *b).0.is_empty(), "not before the delay");
    pump(&net, 1);
    let (got, end) = drain(&mut *b);
    assert_eq!(
        got,
        vec![b"one".to_vec(), b"two".to_vec(), b"three".to_vec()]
    );
    assert!(end.is_none());
    b.send(b"back").expect("sent");
    pump(&net, 3);
    assert_eq!(drain(&mut *a).0, vec![b"back".to_vec()]);
    assert_eq!(net.stats().frames, 4);
    assert_eq!(net.open_connections(), 1);
    // A graceful close: queued frames arrive, then the end.
    a.send(b"last").expect("sent");
    a.close();
    pump(&net, 3);
    let (got, end) = drain(&mut *b);
    assert_eq!(got, vec![b"last".to_vec()]);
    assert!(end.expect("ended").contains("closed by the peer"));
    assert!(matches!(b.send(b"x"), Err(LinkError::Closed(_))));
}

#[test]
fn a_drop_loses_the_frame_and_what_follows() {
    let net = SimNet::new(2);
    net.add_fault(FaultRule {
        kind: FaultKind::Drop,
        direction: Some(Direction::FromDialer),
        message: None,
        stream_kind: None,
        nth: 1,
    });
    let (mut a, mut b) = connected(&net);
    a.send(b"kept").expect("sent");
    a.send(b"dropped").expect("accepted, then lost");
    assert!(matches!(a.send(b"after"), Err(LinkError::Closed(_))));
    pump(&net, 2);
    let (got, end) = drain(&mut *b);
    assert_eq!(got, vec![b"kept".to_vec()]);
    assert!(end.expect("reset").contains("lost frame"));
    assert!(matches!(b.send(b"x"), Err(LinkError::Closed(_))));
    assert_eq!(net.stats().drops, 1);
}

#[test]
fn a_cut_delivers_a_prefix_and_the_receiver_reports_it() {
    let net = SimNet::new(3);
    net.add_fault(FaultRule {
        kind: FaultKind::Cut,
        direction: Some(Direction::FromAcceptor),
        message: None,
        stream_kind: None,
        nth: 0,
    });
    let (mut a, mut b) = connected(&net);
    b.send(&[9u8; 500]).expect("sent");
    pump(&net, 2);
    let (got, end) = drain(&mut *a);
    assert!(got.is_empty());
    let why = end.expect("ended");
    assert!(why.contains("inside a frame"), "{why}");
    assert_eq!(net.stats().cuts, 1);
}

#[test]
fn a_corruption_is_refused_by_the_receiver() {
    let net = SimNet::new(4);
    net.add_fault(FaultRule {
        kind: FaultKind::Corrupt,
        direction: None,
        message: None,
        stream_kind: None,
        nth: 0,
    });
    let (mut a, mut b) = connected(&net);
    a.send(&[1u8; 200]).expect("sent");
    pump(&net, 2);
    let (got, end) = drain(&mut *b);
    assert!(got.is_empty());
    let why = end.expect("refused");
    assert!(why.contains("refused"), "{why}");
    assert_eq!(net.stats().corruptions, 1);
}

#[test]
fn dials_are_refused_when_nothing_listens_or_when_asked() {
    let net = SimNet::new(5);
    assert!(matches!(
        net.dialer().dial("nowhere"),
        Err(LinkError::Closed(why)) if why.contains("nothing listens")
    ));
    let _l = net.listen("here");
    net.refuse_dials(2);
    assert!(net.dialer().dial("here").is_err());
    assert!(net.dialer().dial("here").is_err());
    assert!(net.dialer().dial("here").is_ok());
    net.unlisten("here");
    assert!(net.dialer().dial("here").is_err());
    assert_eq!(net.stats().refused, 4);
}

#[test]
fn a_full_queue_answers_busy_and_a_pulled_cable_ends_everything() {
    let net = SimNet::new(6);
    net.set_queue_limit(2);
    let (mut a, mut b) = connected(&net);
    a.send(b"1").expect("sent");
    a.send(b"2").expect("sent");
    assert_eq!(a.send(b"3"), Err(LinkError::Busy));
    pump(&net, 1);
    assert_eq!(drain(&mut *b).0.len(), 2);
    a.send(b"3").expect("room again");
    net.cut_all();
    assert!(matches!(a.send(b"4"), Err(LinkError::Closed(_))));
    pump(&net, 1);
    let (got, end) = drain(&mut *b);
    assert_eq!(got, vec![b"3".to_vec()], "what was in flight arrives");
    assert!(end.expect("ended").contains("cable"));
    assert_eq!(net.open_connections(), 0);
}

#[test]
fn random_faults_stay_within_their_budget() {
    let net = SimNet::new(7);
    net.set_random_faults(1000, 3);
    let mut listener = net.listen("x");
    let mut faults = 0;
    for _ in 0..10 {
        let mut a = net.dialer().dial("x").expect("dials");
        let _b = listener.accept().expect("accepted");
        let _ = a.send(b"frame");
        faults = net.stats().faults();
    }
    assert_eq!(faults, 3, "every frame is hit until the budget is spent");
}
