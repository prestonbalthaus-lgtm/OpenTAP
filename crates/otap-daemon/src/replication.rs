//! The replication transport of `dbladed` (Step 6C):
//! [`TcpReplicationTransport`], framed TCP over `std::net` that runs on
//! Windows and Linux alike, and [`FileSinkStore`], the sink's resume
//! records in `--state-dir`.
//!
//! # Framing and threads
//!
//! A connection carries the link frames of [`otap_cp::repl::wire`]
//! (magic, length, CRC-32C, payload; at most
//! [`otap_cp::repl::MAX_LINK_PAYLOAD`] bytes, refused at the header before
//! anything is allocated). Each connection has a reader thread and a
//! writer thread doing blocking I/O with timeouts, and the engine thread
//! only ever exchanges whole messages with them through bounded channels
//! ([`TcpLink`]): a slow or dead peer never stalls the data path, and a
//! full channel pushes back on the socket. A peer that stalls inside a
//! frame longer than the I/O timeout, sends a frame that does not verify,
//! or closes in the middle of one ends the connection with the reason;
//! the replication session then resumes after a new handshake.
//!
//! # Security
//!
//! The transport has no TLS and no authentication: whoever reaches the
//! port can push into a mirror's pool or pull a snapshot. `dbladed`
//! therefore listens on, and dials, loopback addresses only, unless the
//! operator passes `--allow-remote-replication`, the same rule and wording
//! as the control plane's `--allow-remote-control`.

use std::fs::{self, File};
use std::io::{self, Read, Write};
use std::net::{Shutdown, SocketAddr, TcpListener, TcpStream, ToSocketAddrs};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, SyncSender, TryRecvError, TrySendError};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use otap_cp::repl::encode_frame;
use otap_cp::repl::wire::FrameReader;
use otap_cp::{Dialer, Link, LinkError, LinkListener, SinkStore};
use otap_format::Uuid;

/// The port `dbladed` listens on for replication by default.
pub const DEFAULT_REPLICATION_PORT: u16 = 7422;

/// How often a blocked reader or writer looks at its stop flag.
const POLL: Duration = Duration::from_millis(200);

/// How the TCP transport behaves.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TcpConfig {
    /// How long a dial may take.
    pub connect_timeout: Duration,
    /// How long a write, or a frame begun by the peer, may stall.
    pub io_timeout: Duration,
    /// Messages each direction's channel holds before it pushes back.
    pub queue_frames: usize,
    /// Listen on, and dial, addresses beyond loopback.
    pub allow_remote: bool,
}

impl Default for TcpConfig {
    fn default() -> Self {
        Self {
            connect_timeout: Duration::from_secs(5),
            io_timeout: Duration::from_secs(30),
            queue_frames: 64,
            allow_remote: false,
        }
    }
}

/// The refusal of an address beyond loopback.
fn remote_refused(addr: &str, what: &str) -> String {
    format!(
        "tcp:{addr} is not a loopback address and the replication transport is unauthenticated \
         and unencrypted (no TLS); {what} 127.0.0.1, or pass --allow-remote-replication"
    )
}

/// Resolves `addr` (`host:port`), checking the loopback rule.
fn resolve(addr: &str, allow_remote: bool, what: &str) -> Result<SocketAddr, String> {
    let resolved = addr
        .to_socket_addrs()
        .map_err(|e| format!("{addr:?} is not a host:port address: {e}"))?
        .next()
        .ok_or_else(|| format!("{addr:?} resolves to no address"))?;
    if !allow_remote && !resolved.ip().is_loopback() {
        return Err(remote_refused(addr, what));
    }
    Ok(resolved)
}

/// The TCP transport: dials links and binds listeners.
#[derive(Debug, Clone, Default)]
pub struct TcpReplicationTransport {
    cfg: TcpConfig,
}

impl TcpReplicationTransport {
    /// A transport.
    #[must_use]
    pub const fn new(cfg: TcpConfig) -> Self {
        Self { cfg }
    }

    /// The configuration.
    #[must_use]
    pub const fn config(&self) -> &TcpConfig {
        &self.cfg
    }

    /// Binds `addr` (`host:port`; port 0 takes an ephemeral one) and starts
    /// accepting. An address beyond loopback is refused unless the
    /// configuration allows it.
    pub fn listen(&self, addr: &str) -> Result<TcpReplicationListener, String> {
        let local = resolve(addr, self.cfg.allow_remote, "bind")?;
        let listener = TcpListener::bind(local)
            .map_err(|e| format!("cannot listen for replication on tcp:{local}: {e}"))?;
        let local = listener
            .local_addr()
            .map_err(|e| format!("cannot read the bound address: {e}"))?;
        listener
            .set_nonblocking(true)
            .map_err(|e| format!("cannot set the replication listener non-blocking: {e}"))?;
        let (tx, rx) = mpsc::channel();
        let stop = Arc::new(AtomicBool::new(false));
        let cfg = self.cfg.clone();
        let flag = Arc::clone(&stop);
        let thread = thread::Builder::new()
            .name("dbladed-repl-accept".to_string())
            .spawn(move || accept_loop(&listener, &tx, &flag, &cfg))
            .map_err(|e| format!("cannot start the replication listener: {e}"))?;
        Ok(TcpReplicationListener {
            local,
            rx,
            stop,
            thread: Some(thread),
        })
    }
}

impl Dialer for TcpReplicationTransport {
    fn dial(&mut self, addr: &str) -> Result<Box<dyn Link>, LinkError> {
        // An address literal is checked at once; a host name resolves on
        // the connecting thread (name resolution may block).
        if let Ok(literal) = addr.parse::<SocketAddr>() {
            if !self.cfg.allow_remote && !literal.ip().is_loopback() {
                return Err(LinkError::Address(remote_refused(addr, "dial")));
            }
        } else if !addr.contains(':') {
            return Err(LinkError::Address(format!(
                "{addr:?} is not a host:port address"
            )));
        }
        Ok(Box::new(TcpLink::dial(addr.to_string(), self.cfg.clone())))
    }
}

fn accept_loop(
    listener: &TcpListener,
    tx: &mpsc::Sender<TcpLink>,
    stop: &AtomicBool,
    cfg: &TcpConfig,
) {
    while !stop.load(Ordering::Relaxed) {
        match listener.accept() {
            Ok((stream, peer)) => {
                if !cfg.allow_remote && !peer.ip().is_loopback() {
                    // Never answered: the rule is the operator's, not the
                    // peer's to negotiate.
                    let _ = stream.shutdown(Shutdown::Both);
                    continue;
                }
                match TcpLink::accepted(stream, peer, cfg.clone()) {
                    Ok(link) => {
                        if tx.send(link).is_err() {
                            return;
                        }
                    }
                    Err(e) => eprintln!("dbladed: a replication connection from {peer}: {e}"),
                }
            }
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                thread::sleep(Duration::from_millis(20))
            }
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(e) => {
                eprintln!("dbladed: the replication listener: {e}");
                thread::sleep(Duration::from_millis(100));
            }
        }
    }
}

/// A bound replication port; the engine thread takes its connections.
#[derive(Debug)]
pub struct TcpReplicationListener {
    local: SocketAddr,
    rx: Receiver<TcpLink>,
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl TcpReplicationListener {
    /// The address as bound.
    #[must_use]
    pub const fn local_addr(&self) -> SocketAddr {
        self.local
    }
}

impl LinkListener for TcpReplicationListener {
    fn accept(&mut self) -> Option<Box<dyn Link>> {
        self.rx
            .try_recv()
            .ok()
            .map(|l| Box::new(l) as Box<dyn Link>)
    }
}

impl Drop for TcpReplicationListener {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

/// What the I/O threads and the engine thread share about a connection.
#[derive(Debug, Default)]
struct Shared {
    /// The engine thread closed the link: the reader stops.
    closed: AtomicBool,
    /// The socket, once connected, for the shutdown that ends both threads.
    stream: Mutex<Option<TcpStream>>,
}

impl Shared {
    fn shutdown(&self) {
        if let Ok(guard) = self.stream.lock() {
            if let Some(s) = guard.as_ref() {
                let _ = s.shutdown(Shutdown::Both);
            }
        }
    }
}

/// One replication connection over TCP, as the engine thread sees it.
#[derive(Debug)]
pub struct TcpLink {
    peer: String,
    out: Option<SyncSender<Vec<u8>>>,
    inbound: Receiver<Result<Vec<u8>, String>>,
    shared: Arc<Shared>,
    ended: Option<String>,
}

impl TcpLink {
    /// A link to `addr`, connected by its reader thread.
    fn dial(addr: String, cfg: TcpConfig) -> Self {
        let (out_tx, out_rx) = mpsc::sync_channel::<Vec<u8>>(cfg.queue_frames.max(1));
        let (in_tx, in_rx) = mpsc::sync_channel(cfg.queue_frames.max(1));
        let shared = Arc::new(Shared::default());
        let link = Self {
            peer: addr.clone(),
            out: Some(out_tx),
            inbound: in_rx,
            shared: Arc::clone(&shared),
            ended: None,
        };
        let spawned = thread::Builder::new()
            .name("dbladed-repl-io".to_string())
            .spawn(move || {
                let stream = match connect(&addr, &cfg) {
                    Ok(s) => s,
                    Err(e) => {
                        let _ = in_tx.send(Err(e));
                        return;
                    }
                };
                run_connection(stream, &shared, out_rx, &in_tx, &cfg);
            });
        if let Err(e) = spawned {
            let mut link = link;
            link.ended = Some(format!("cannot start the connection's thread: {e}"));
            return link;
        }
        link
    }

    /// A link over a connection a listener accepted.
    fn accepted(stream: TcpStream, peer: SocketAddr, cfg: TcpConfig) -> io::Result<Self> {
        // Windows hands out accepted sockets in the listener's
        // non-blocking mode.
        stream.set_nonblocking(false)?;
        let _ = stream.set_nodelay(true);
        let (out_tx, out_rx) = mpsc::sync_channel::<Vec<u8>>(cfg.queue_frames.max(1));
        let (in_tx, in_rx) = mpsc::sync_channel(cfg.queue_frames.max(1));
        let shared = Arc::new(Shared::default());
        let thread_shared = Arc::clone(&shared);
        thread::Builder::new()
            .name("dbladed-repl-io".to_string())
            .spawn(move || run_connection(stream, &thread_shared, out_rx, &in_tx, &cfg))?;
        Ok(Self {
            peer: peer.to_string(),
            out: Some(out_tx),
            inbound: in_rx,
            shared,
            ended: None,
        })
    }
}

/// Connects with the timeout, after the loopback rule.
fn connect(addr: &str, cfg: &TcpConfig) -> Result<TcpStream, String> {
    let target = resolve(addr, cfg.allow_remote, "dial")?;
    let stream = TcpStream::connect_timeout(&target, cfg.connect_timeout)
        .map_err(|e| format!("cannot connect to tcp:{target}: {e}"))?;
    let _ = stream.set_nodelay(true);
    Ok(stream)
}

/// The connection's threads: this one reads, a second one writes.
fn run_connection(
    stream: TcpStream,
    shared: &Arc<Shared>,
    out_rx: Receiver<Vec<u8>>,
    in_tx: &SyncSender<Result<Vec<u8>, String>>,
    cfg: &TcpConfig,
) {
    let writer_stream = match stream.try_clone() {
        Ok(s) => s,
        Err(e) => {
            let _ = in_tx.send(Err(format!("cannot share the socket: {e}")));
            return;
        }
    };
    if let Ok(mut guard) = shared.stream.lock() {
        *guard = stream.try_clone().ok();
    }
    let writer_shared = Arc::clone(shared);
    let io_timeout = cfg.io_timeout;
    let writer = thread::Builder::new()
        .name("dbladed-repl-write".to_string())
        .spawn(move || write_loop(writer_stream, &writer_shared, &out_rx, io_timeout));
    if let Err(e) = writer {
        let _ = in_tx.send(Err(format!("cannot start the writer: {e}")));
        return;
    }
    read_loop(stream, shared, in_tx, io_timeout);
}

/// How long a closed connection keeps reading (and discarding) before the
/// socket is shut: long enough for the peer to read what was written last
/// (a refusal) and close its own end, so the close is a FIN on both sides
/// and never a reset that would destroy that last message.
const LINGER: Duration = Duration::from_secs(2);

fn write_loop(
    mut stream: TcpStream,
    shared: &Shared,
    out_rx: &Receiver<Vec<u8>>,
    io_timeout: Duration,
) {
    let _ = stream.set_write_timeout(Some(io_timeout));
    loop {
        match out_rx.recv_timeout(POLL) {
            Ok(bytes) => {
                if stream.write_all(&bytes).is_err() {
                    shared.shutdown();
                    return;
                }
            }
            // Idle: the engine side still holds the link.
            Err(RecvTimeoutError::Timeout) => {}
            // The engine side closed (or dropped) the link: everything it
            // queued was written.
            Err(RecvTimeoutError::Disconnected) => break,
        }
    }
    let _ = stream.flush();
    // A FIN after the last message; the reader lingers until the peer
    // closes too, then shuts the socket.
    let _ = stream.shutdown(Shutdown::Write);
}

fn read_loop(
    mut stream: TcpStream,
    shared: &Shared,
    in_tx: &SyncSender<Result<Vec<u8>, String>>,
    io_timeout: Duration,
) {
    let _ = stream.set_read_timeout(Some(POLL));
    let mut reader = FrameReader::new();
    let mut buf = vec![0u8; 256 * 1024];
    let mut stalled: Option<Instant> = None;
    let mut closing: Option<Instant> = None;
    let why = loop {
        let closed = shared.closed.load(Ordering::Relaxed);
        if closed && closing.is_none() {
            closing = Some(Instant::now());
        }
        if closing.is_some_and(|at| at.elapsed() > LINGER) {
            shared.shutdown();
            return;
        }
        match stream.read(&mut buf) {
            Ok(0) => {
                if closed {
                    shared.shutdown();
                    return;
                }
                break match reader.finish() {
                    Ok(()) => "closed by the peer".to_string(),
                    Err(e) => format!("the peer closed the connection: {e}"),
                };
            }
            // This side closed: what still arrives is read and dropped
            // until the peer's FIN.
            Ok(_) if closed => {}
            Ok(n) => {
                stalled = None;
                reader.feed(&buf[..n]);
                let mut failed = None;
                loop {
                    match reader.next_payload() {
                        Ok(Some(payload)) => {
                            if in_tx.send(Ok(payload)).is_err() {
                                shared.shutdown();
                                return;
                            }
                        }
                        Ok(None) => break,
                        Err(e) => {
                            failed = Some(format!("a frame from the peer was refused: {e}"));
                            break;
                        }
                    }
                }
                if let Some(why) = failed {
                    break why;
                }
            }
            Err(e)
                if e.kind() == io::ErrorKind::WouldBlock || e.kind() == io::ErrorKind::TimedOut =>
            {
                if !closed && reader.pending() > 0 {
                    let since = *stalled.get_or_insert_with(Instant::now);
                    if since.elapsed() > io_timeout {
                        break format!(
                            "the peer stalled for {} s in the middle of a frame",
                            io_timeout.as_secs()
                        );
                    }
                }
            }
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(e) => {
                if closed {
                    shared.shutdown();
                    return;
                }
                break format!("the connection failed: {e}");
            }
        }
    };
    let _ = in_tx.send(Err(why));
    shared.shutdown();
}

impl Link for TcpLink {
    fn send(&mut self, msg: &[u8]) -> Result<(), LinkError> {
        if let Some(why) = &self.ended {
            return Err(LinkError::Closed(why.clone()));
        }
        let out = self
            .out
            .as_ref()
            .ok_or_else(|| LinkError::Closed("closed by this side".to_string()))?;
        let frame = encode_frame(msg).map_err(|e| LinkError::Closed(e.to_string()))?;
        match out.try_send(frame) {
            Ok(()) => Ok(()),
            Err(TrySendError::Full(_)) => Err(LinkError::Busy),
            Err(TrySendError::Disconnected(_)) => {
                let why = self.drain_reason();
                Err(LinkError::Closed(why))
            }
        }
    }

    fn recv(&mut self) -> Result<Option<Vec<u8>>, LinkError> {
        if let Some(why) = &self.ended {
            return Err(LinkError::Closed(why.clone()));
        }
        match self.inbound.try_recv() {
            Ok(Ok(payload)) => Ok(Some(payload)),
            Ok(Err(why)) => {
                self.ended = Some(why.clone());
                Err(LinkError::Closed(why))
            }
            Err(TryRecvError::Empty) => Ok(None),
            Err(TryRecvError::Disconnected) => {
                let why = "the connection's thread ended".to_string();
                self.ended = Some(why.clone());
                Err(LinkError::Closed(why))
            }
        }
    }

    fn close(&mut self) {
        if self.ended.is_none() {
            self.ended = Some("closed by this side".to_string());
        }
        self.shared.closed.store(true, Ordering::Relaxed);
        // Dropping the sender lets the writer finish what is queued, then
        // shut the socket.
        self.out = None;
        while self.inbound.try_recv().is_ok() {}
    }

    fn peer(&self) -> String {
        self.peer.clone()
    }
}

impl TcpLink {
    /// The reason the reader gave, when the writer is gone.
    fn drain_reason(&mut self) -> String {
        while let Ok(item) = self.inbound.try_recv() {
            if let Err(why) = item {
                self.ended = Some(why.clone());
                return why;
            }
        }
        let why = "the connection ended".to_string();
        self.ended = Some(why.clone());
        why
    }
}

impl Drop for TcpLink {
    fn drop(&mut self) {
        self.close();
    }
}

/// The sink's resume records as files in the daemon's state directory:
/// `repl-<pool>-<key>.rec`, written to a temporary file, synced, renamed
/// over the record and the directory synced, like the ublk bindings.
#[derive(Debug, Clone)]
pub struct FileSinkStore {
    dir: PathBuf,
    agg_id: Uuid,
}

impl FileSinkStore {
    /// The records of pool `agg_id` in `dir`.
    #[must_use]
    pub fn new(dir: &Path, agg_id: Uuid) -> Self {
        Self {
            dir: dir.to_path_buf(),
            agg_id,
        }
    }

    fn prefix(&self) -> String {
        format!("repl-{}-", self.agg_id)
    }

    fn path(&self, key: &str) -> Result<PathBuf, String> {
        if key.is_empty()
            || !key
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
        {
            return Err(format!("{key:?} is not a record key"));
        }
        Ok(self.dir.join(format!("{}{key}.rec", self.prefix())))
    }
}

#[cfg(unix)]
fn sync_dir(dir: &Path) -> io::Result<()> {
    File::open(dir)?.sync_all()
}

#[cfg(not(unix))]
fn sync_dir(_dir: &Path) -> io::Result<()> {
    // A directory cannot be opened for a sync here; the rename itself is
    // what the platform makes durable.
    Ok(())
}

impl SinkStore for FileSinkStore {
    fn load(&mut self, key: &str) -> Result<Option<Vec<u8>>, String> {
        let path = self.path(key)?;
        match fs::read(&path) {
            Ok(bytes) => Ok(Some(bytes)),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(format!("cannot read {}: {e}", path.display())),
        }
    }

    fn save(&mut self, key: &str, bytes: &[u8]) -> Result<(), String> {
        let path = self.path(key)?;
        let tmp = path.with_extension("tmp");
        let write = || -> io::Result<()> {
            fs::create_dir_all(&self.dir)?;
            {
                let mut file = File::create(&tmp)?;
                file.write_all(bytes)?;
                file.sync_all()?;
            }
            fs::rename(&tmp, &path)?;
            sync_dir(&self.dir)
        };
        write().map_err(|e| format!("cannot write {}: {e}", path.display()))
    }

    fn remove(&mut self, key: &str) -> Result<(), String> {
        let path = self.path(key)?;
        match fs::remove_file(&path) {
            Ok(()) => sync_dir(&self.dir).map_err(|e| format!("{}: {e}", self.dir.display())),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(format!("cannot remove {}: {e}", path.display())),
        }
    }

    fn keys(&mut self) -> Result<Vec<String>, String> {
        let prefix = self.prefix();
        let entries = match fs::read_dir(&self.dir) {
            Ok(entries) => entries,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(e) => return Err(format!("cannot list {}: {e}", self.dir.display())),
        };
        let mut keys = Vec::new();
        for entry in entries.flatten() {
            let name = entry.file_name();
            let Some(name) = name.to_str() else {
                continue;
            };
            if let Some(key) = name
                .strip_prefix(&prefix)
                .and_then(|rest| rest.strip_suffix(".rec"))
            {
                keys.push(key.to_string());
            }
        }
        keys.sort();
        Ok(keys)
    }
}
