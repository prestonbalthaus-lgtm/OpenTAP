//! Step 6C: the TCP replication transport on 127.0.0.1: whole frames in
//! order both ways, clean refusals of garbage, truncation, oversize and
//! corrupt frames and of a peer of another protocol version, the loopback
//! rule, and a live push between two pools that survives a connection cut
//! in the middle of the stream and completes by resuming.

use std::io::{Read, Write};
use std::net::{Shutdown, SocketAddr, TcpListener, TcpStream};
use std::thread;
use std::time::{Duration, Instant};

use otap_cp::repl::{
    encode_frame, FrameReader, Handshake, JobReport, JobState, Message, RefuseCode, ReplConfig,
    Replicator, Role, SinkConfig, SourceConfig, MAX_LINK_PAYLOAD,
};
use otap_cp::{Dialer, Link, LinkError, LinkListener};
use otap_daemon::{TcpConfig, TcpReplicationListener, TcpReplicationTransport};
use otap_format::{Uuid, BLOCK_SIZE};
use otap_sim::{MemStore, Sim, SimConfig};

const DEADLINE: Duration = Duration::from_secs(60);

fn transport() -> TcpReplicationTransport {
    TcpReplicationTransport::new(TcpConfig {
        io_timeout: Duration::from_secs(2),
        ..TcpConfig::default()
    })
}

fn accept_within(listener: &mut TcpReplicationListener) -> Box<dyn Link> {
    let start = Instant::now();
    while start.elapsed() < DEADLINE {
        if let Some(link) = listener.accept() {
            return link;
        }
        thread::sleep(Duration::from_millis(2));
    }
    panic!("no connection was accepted");
}

fn recv_within(link: &mut dyn Link) -> Result<Vec<u8>, LinkError> {
    let start = Instant::now();
    while start.elapsed() < DEADLINE {
        match link.recv() {
            Ok(Some(payload)) => return Ok(payload),
            Ok(None) => thread::sleep(Duration::from_millis(1)),
            Err(e) => return Err(e),
        }
    }
    panic!("nothing arrived");
}

fn send_within(link: &mut dyn Link, msg: &[u8]) {
    let start = Instant::now();
    while start.elapsed() < DEADLINE {
        match link.send(msg) {
            Ok(()) => return,
            Err(LinkError::Busy) => thread::sleep(Duration::from_millis(1)),
            Err(e) => panic!("send: {e}"),
        }
    }
    panic!("the link stayed busy");
}

/// A raw client of the listener, and the link the listener made of it.
fn raw_pair() -> (TcpStream, Box<dyn Link>, TcpReplicationListener) {
    let mut listener = transport().listen("127.0.0.1:0").expect("listens");
    let raw = TcpStream::connect(listener.local_addr()).expect("connects");
    let link = accept_within(&mut listener);
    (raw, link, listener)
}

fn closed_reason(link: &mut dyn Link) -> String {
    match recv_within(link) {
        Err(LinkError::Closed(why)) => why,
        other => panic!("the link should have ended: {other:?}"),
    }
}

#[test]
fn frames_cross_a_connection_whole_and_in_order() {
    let mut t = transport();
    let mut listener = t.listen("127.0.0.1:0").expect("listens");
    let addr = listener.local_addr().to_string();
    let mut a = t.dial(&addr).expect("dials");
    let mut b = accept_within(&mut listener);
    let big: Vec<u8> = (0..(1usize << 20)).map(|i| (i % 251) as u8).collect();
    let messages: Vec<Vec<u8>> = vec![b"one".to_vec(), big.clone(), vec![7u8; 4096], vec![9]];
    for m in &messages {
        send_within(&mut *a, m);
    }
    for m in &messages {
        assert_eq!(&recv_within(&mut *b).expect("arrives"), m);
    }
    send_within(&mut *b, b"back");
    assert_eq!(recv_within(&mut *a).expect("arrives"), b"back");
    // A close is graceful: what was queued arrives, then the end.
    send_within(&mut *a, b"last");
    a.close();
    assert_eq!(recv_within(&mut *b).expect("arrives"), b"last");
    assert!(closed_reason(&mut *b).contains("closed by the peer"));
    assert!(matches!(a.send(b"x"), Err(LinkError::Closed(_))));
}

#[test]
fn garbage_is_refused_and_ends_the_connection() {
    let (mut raw, mut link, _l) = raw_pair();
    raw.write_all(b"GET / HTTP/1.1\r\nHost: x\r\n\r\n")
        .expect("writes");
    let why = closed_reason(&mut *link);
    assert!(why.contains("replication magic"), "{why}");
}

#[test]
fn a_truncated_frame_is_reported() {
    let (mut raw, mut link, _l) = raw_pair();
    let frame = encode_frame(&[0x5au8; 1000]).expect("encodes");
    raw.write_all(&frame[..600]).expect("writes");
    raw.shutdown(Shutdown::Both).expect("shuts");
    let why = closed_reason(&mut *link);
    assert!(why.contains("inside a frame"), "{why}");
    assert!(why.contains("600 of 1012"), "{why}");
}

#[test]
fn an_oversize_frame_is_refused_before_it_is_read() {
    let (mut raw, mut link, _l) = raw_pair();
    let mut header = Vec::new();
    header.extend_from_slice(&otap_cp::repl::LINK_MAGIC.to_le_bytes());
    header.extend_from_slice(&((MAX_LINK_PAYLOAD + 1) as u32).to_le_bytes());
    header.extend_from_slice(&0u32.to_le_bytes());
    raw.write_all(&header).expect("writes");
    let why = closed_reason(&mut *link);
    assert!(why.contains("above the bound"), "{why}");
}

#[test]
fn a_corrupt_frame_is_refused() {
    let (mut raw, mut link, _l) = raw_pair();
    let mut frame = encode_frame(b"a perfectly good payload").expect("encodes");
    let last = frame.len() - 1;
    frame[last] ^= 1;
    raw.write_all(&frame).expect("writes");
    let why = closed_reason(&mut *link);
    assert!(why.contains("crc"), "{why}");
}

#[test]
fn a_stall_inside_a_frame_times_out() {
    let (mut raw, mut link, _l) = raw_pair();
    let frame = encode_frame(&[1u8; 100]).expect("encodes");
    raw.write_all(&frame[..50]).expect("writes");
    let why = closed_reason(&mut *link);
    assert!(why.contains("stalled"), "{why}");
    drop(raw);
}

#[test]
fn addresses_beyond_loopback_are_refused_without_the_flag() {
    let t = transport();
    let refused = t.listen("0.0.0.0:0").expect_err("refused");
    assert!(refused.contains("--allow-remote-replication"), "{refused}");
    assert!(refused.contains("no TLS"), "{refused}");
    let mut t = transport();
    match t.dial("192.0.2.10:7422") {
        Err(LinkError::Address(why)) => assert!(why.contains("loopback"), "{why}"),
        Err(other) => panic!("{other:?}"),
        Ok(_) => panic!("a dial beyond loopback was let through"),
    }
    assert!(matches!(t.dial("no-port"), Err(LinkError::Address(_))));
    let open = TcpReplicationTransport::new(TcpConfig {
        allow_remote: true,
        ..TcpConfig::default()
    });
    let listener = open.listen("0.0.0.0:0").expect("allowed with the flag");
    assert!(listener.local_addr().port() != 0);
}

#[test]
fn a_dial_to_a_closed_port_ends_the_link() {
    let port = {
        let l = TcpListener::bind("127.0.0.1:0").expect("binds");
        l.local_addr().expect("addr").port()
    };
    let mut t = transport();
    let mut link = t.dial(&format!("127.0.0.1:{port}")).expect("dials");
    let why = closed_reason(&mut *link);
    assert!(why.contains("cannot connect"), "{why}");
}

/// A pool and its replicator over the TCP transport.
struct Side {
    sim: Sim,
    repl: Replicator,
    listener: Option<TcpReplicationListener>,
}

impl Side {
    fn new(cfg: SimConfig, listen: bool) -> Self {
        let mut sim = Sim::new(cfg).expect("formats");
        sim.boot().expect("boots");
        let t = transport();
        let listener = listen.then(|| t.listen("127.0.0.1:0").expect("listens"));
        let mut repl = Replicator::new(repl_config(), Box::new(t), Box::new(MemStore::new()));
        repl.attach(sim.engine());
        Self {
            sim,
            repl,
            listener,
        }
    }

    fn tick(&mut self) -> bool {
        self.sim.advance(1_000_000);
        self.sim.tick().expect("ticks");
        if let Some(l) = self.listener.as_mut() {
            while let Some(link) = l.accept() {
                let now = self.sim.engine().mono_ns();
                self.repl.offer(link, now);
            }
        }
        self.repl.step(self.sim.engine())
    }

    fn addr(&self) -> SocketAddr {
        self.listener.as_ref().expect("listens").local_addr()
    }
}

fn repl_config() -> ReplConfig {
    ReplConfig {
        source: SourceConfig {
            chunk_bytes: 32 * 1024,
            retry_initial_ns: 20_000_000,
            ..SourceConfig::default()
        },
        sink: SinkConfig {
            staging_bytes: 512 * 1024,
            ..SinkConfig::default()
        },
        ..ReplConfig::default()
    }
}

/// Ticks both sides until `done`, in real time (the sockets are real).
fn run(src: &mut Side, dst: &mut Side, mut done: impl FnMut(&mut Side, &mut Side) -> bool) {
    let start = Instant::now();
    while start.elapsed() < DEADLINE {
        if done(src, dst) {
            return;
        }
        let busy = src.tick() | dst.tick();
        if !busy {
            thread::sleep(Duration::from_micros(200));
        }
    }
    panic!(
        "timed out; source {:#?}; sink {:#?}",
        src.repl.jobs(),
        dst.repl.jobs()
    );
}

/// A TCP relay to `target` that cuts its first connection after
/// `cut_after` bytes from the client, and relays every later one whole.
fn cutting_proxy(target: SocketAddr, cut_after: usize) -> SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").expect("the proxy binds");
    let addr = listener.local_addr().expect("addr");
    thread::spawn(move || {
        let mut first = true;
        for client in listener.incoming() {
            let Ok(client) = client else {
                continue;
            };
            let Ok(server) = TcpStream::connect(target) else {
                continue;
            };
            let limit = first.then_some(cut_after);
            first = false;
            relay(client, server, limit);
        }
    });
    addr
}

fn relay(client: TcpStream, server: TcpStream, limit: Option<usize>) {
    let (Ok(mut c_read), Ok(mut s_write)) = (client.try_clone(), server.try_clone()) else {
        return;
    };
    let (mut s_read, mut c_write) = (server, client);
    let upstream = thread::spawn(move || {
        let mut buf = [0u8; 16 * 1024];
        let mut moved = 0usize;
        loop {
            let n = match c_read.read(&mut buf) {
                Ok(0) | Err(_) => break,
                Ok(n) => n,
            };
            let n = match limit {
                Some(limit) if moved + n >= limit => {
                    let _ = s_write.write_all(&buf[..limit - moved]);
                    let _ = s_write.shutdown(Shutdown::Both);
                    let _ = c_read.shutdown(Shutdown::Both);
                    return;
                }
                _ => n,
            };
            if s_write.write_all(&buf[..n]).is_err() {
                break;
            }
            moved += n;
        }
        let _ = s_write.shutdown(Shutdown::Both);
    });
    thread::spawn(move || {
        let mut buf = [0u8; 16 * 1024];
        loop {
            match s_read.read(&mut buf) {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    if c_write.write_all(&buf[..n]).is_err() {
                        break;
                    }
                }
            }
        }
        let _ = c_write.shutdown(Shutdown::Both);
        let _ = upstream.join();
    });
}

fn snapshot_uuid(sim: &mut Sim, snap_id: u64) -> Uuid {
    sim.engine()
        .snapshots()
        .iter()
        .find(|s| s.snap_id == snap_id)
        .expect("the snapshot")
        .uuid
}

/// Every block of the source snapshot reads the same on the destination.
fn assert_converged(src: &mut Side, dst: &mut Side, snap_id: u64, blocks: u64) {
    let uuid = snapshot_uuid(&mut src.sim, snap_id);
    let mirror = dst.sim.engine().find_volume("mirror").expect("the mirror");
    let dsnap = dst
        .sim
        .engine()
        .snapshots()
        .into_iter()
        .find(|s| s.vol == mirror && s.uuid == uuid)
        .expect("the destination snapshot");
    let mut a = vec![0u8; BLOCK_SIZE];
    let mut b = vec![0u8; BLOCK_SIZE];
    for lba in 0..blocks {
        src.sim
            .engine()
            .snapshot_read(snap_id, lba, &mut a)
            .expect("reads");
        dst.sim
            .engine()
            .snapshot_read(dsnap.snap_id, lba, &mut b)
            .expect("reads");
        assert!(a == b, "block {lba} differs");
    }
}

fn finished(side: &Side, id: u64) -> Option<JobReport> {
    side.repl.job(id).filter(|j| j.state != JobState::Running)
}

/// A live push between two pools over 127.0.0.1, through a relay that
/// cuts the first connection in the middle of the chunks: the source
/// dials again, the sink's cursor says what landed, and the transfer
/// completes without re-landing a chunk. A second delta follows over the
/// advanced base.
#[test]
fn a_push_over_tcp_survives_a_cut_and_resumes() {
    let mut scfg = SimConfig::small(8101);
    scfg.volumes = vec![(0, 1024)];
    let mut dcfg = SimConfig::small(8102);
    dcfg.volumes = vec![];
    let mut src = Side::new(scfg, false);
    let mut dst = Side::new(dcfg, true);
    for lba in (0..1024).step_by(8) {
        let c = src.sim.write(0, lba, 8).expect("writes");
        assert_eq!(c.status, otap_cp::Status::Ok);
    }
    let s1 = src.sim.snapshot(0).expect("a snapshot");
    let proxy = cutting_proxy(dst.addr(), 1 << 20);
    let id = src
        .repl
        .push(
            src.sim.engine(),
            "#0",
            &format!("#{s1}"),
            &proxy.to_string(),
            Some("mirror"),
        )
        .expect("the push starts")
        .id;
    run(&mut src, &mut dst, |s, _| finished(s, id).is_some());
    let job = finished(&src, id).expect("ended");
    assert_eq!(job.state, JobState::Done, "{job:#?}");
    assert!(job.retries >= 1, "the cut was met: {job:#?}");
    assert!(
        job.resumed_chunks > 0,
        "the resume skipped landed chunks: {job:#?}"
    );
    run(&mut src, &mut dst, |_, d| {
        d.repl.jobs().iter().all(|j| j.state != JobState::Running)
    });
    for sink in dst.repl.jobs() {
        assert_eq!(sink.duplicate_chunks, 0, "{sink:#?}");
    }
    assert_converged(&mut src, &mut dst, s1, 1024);

    // A second delta, straight to the sink.
    for lba in [3u64, 500, 900] {
        let c = src.sim.write(0, lba, 4).expect("writes");
        assert_eq!(c.status, otap_cp::Status::Ok);
    }
    let s2 = src.sim.snapshot(0).expect("a snapshot");
    let id = src
        .repl
        .push(
            src.sim.engine(),
            "#0",
            &format!("#{s2}"),
            &dst.addr().to_string(),
            Some("mirror"),
        )
        .expect("the push starts")
        .id;
    run(&mut src, &mut dst, |s, _| finished(s, id).is_some());
    let job = finished(&src, id).expect("ended");
    assert_eq!(job.state, JobState::Done, "{job:#?}");
    assert_eq!(job.chunks, 2, "12 changed blocks: {job:#?}");
    assert_converged(&mut src, &mut dst, s2, 1024);
}

/// A peer that speaks another protocol version is refused with a
/// refusal that names the versions, then the connection closes.
#[test]
fn a_peer_of_another_version_is_refused_cleanly() {
    let mut dcfg = SimConfig::small(8103);
    dcfg.volumes = vec![];
    let mut dst = Side::new(dcfg, true);
    let mut raw = TcpStream::connect(dst.addr()).expect("connects");
    let mut h = Handshake::new(Role::Source, false, Uuid::from_bytes([3; 16]));
    h.version = 99;
    h.peer_volume = "mirror".into();
    let payload = Message::Handshake(h).encode().expect("encodes");
    raw.write_all(&encode_frame(&payload).expect("frames"))
        .expect("writes");
    raw.set_read_timeout(Some(Duration::from_millis(5)))
        .expect("timeout");
    let mut reader = FrameReader::new();
    let start = Instant::now();
    let refusal = loop {
        assert!(start.elapsed() < DEADLINE, "no refusal came");
        dst.tick();
        let mut buf = [0u8; 4096];
        match raw.read(&mut buf) {
            Ok(0) => panic!("closed without a refusal"),
            Ok(n) => reader.feed(&buf[..n]),
            Err(_) => {}
        }
        if let Some(p) = reader.next_payload().expect("a good frame") {
            break Message::decode(&p).expect("a message");
        }
    };
    match refusal {
        Message::Refuse(r) => {
            assert_eq!(r.code, RefuseCode::Version);
            assert!(r.text.contains("99"), "{}", r.text);
        }
        other => panic!("{other:?}"),
    }
}

/// A sink that refuses: pushing into a destination volume that is online
/// fails the job with the sink's reason, and nothing is retried.
#[test]
fn a_refusal_fails_the_push_with_the_sinks_reason() {
    let mut scfg = SimConfig::small(8104);
    scfg.volumes = vec![(0, 64)];
    let mut dcfg = SimConfig::small(8105);
    dcfg.volumes = vec![(0, 64)];
    let mut src = Side::new(scfg, false);
    let mut dst = Side::new(dcfg, true);
    dst.sim.engine().set_volume_name(0, "busy").expect("named");
    src.sim.write(0, 0, 4).expect("writes");
    let s1 = src.sim.snapshot(0).expect("a snapshot");
    let id = src
        .repl
        .push(
            src.sim.engine(),
            "#0",
            &format!("#{s1}"),
            &dst.addr().to_string(),
            Some("busy"),
        )
        .expect("the push starts")
        .id;
    run(&mut src, &mut dst, |s, _| finished(s, id).is_some());
    let job = finished(&src, id).expect("ended");
    assert_eq!(job.state, JobState::Failed);
    let why = job.last_error.unwrap_or_default();
    assert!(why.contains("online"), "{why}");
    assert_eq!(job.retries, 0, "a refusal is not retried");
}
