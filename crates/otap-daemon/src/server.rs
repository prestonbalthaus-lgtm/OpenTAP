//! The control server: accepts connections on the endpoint and answers each
//! request through the engine thread, one thread per connection.

use std::io::{self, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::Duration;

use otap_control::{
    read_frame, write_frame, ControlError, Endpoint, ErrorCode, Reply, Request, WireError,
};

use crate::daemon::DaemonHandle;

/// How often the accept loop looks at the stop flags.
const ACCEPT_POLL: Duration = Duration::from_millis(20);

/// How long a connection may stay silent between requests before it is
/// closed: a client that opens connections and never speaks would pin a
/// thread each for ever otherwise. `otapctl` opens one per command.
const IDLE_TIMEOUT: Duration = Duration::from_secs(300);

enum Listener {
    Tcp(TcpListener),
    #[cfg(unix)]
    Unix(std::os::unix::net::UnixListener),
}

/// An accepted connection.
enum Conn {
    Tcp(TcpStream),
    #[cfg(unix)]
    Unix(std::os::unix::net::UnixStream),
}

impl Read for Conn {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        match self {
            Self::Tcp(s) => s.read(buf),
            #[cfg(unix)]
            Self::Unix(s) => s.read(buf),
        }
    }
}

impl Write for Conn {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        match self {
            Self::Tcp(s) => s.write(buf),
            #[cfg(unix)]
            Self::Unix(s) => s.write(buf),
        }
    }

    fn flush(&mut self) -> io::Result<()> {
        match self {
            Self::Tcp(s) => s.flush(),
            #[cfg(unix)]
            Self::Unix(s) => s.flush(),
        }
    }
}

/// A bound control endpoint.
pub struct Server {
    listener: Listener,
    endpoint: Endpoint,
}

impl std::fmt::Debug for Server {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Server")
            .field("endpoint", &self.endpoint)
            .finish_non_exhaustive()
    }
}

impl Server {
    /// Binds `endpoint`. A TCP port of 0 takes an ephemeral port, which
    /// [`endpoint`](Self::endpoint) then reports; a TCP address that is
    /// not loopback is refused (see [`bind_with`](Self::bind_with)). A
    /// Unix socket path that exists is taken over when nothing answers on
    /// it, and refused when another daemon does.
    pub fn bind(endpoint: &Endpoint) -> Result<Self, ControlError> {
        Self::bind_with(endpoint, false)
    }

    /// [`bind`](Self::bind) with the choice to listen beyond loopback. The
    /// control plane is unauthenticated and can format any device the
    /// daemon can write, so a TCP address that is not loopback is refused
    /// unless `allow_remote` says the operator knows.
    pub fn bind_with(endpoint: &Endpoint, allow_remote: bool) -> Result<Self, ControlError> {
        match endpoint {
            Endpoint::Tcp(addr) => {
                if !allow_remote && !addr.ip().is_loopback() {
                    return Err(ControlError::new(
                        ErrorCode::BadRequest,
                        format!(
                            "tcp:{addr} is not a loopback address and the control plane is \
                             unauthenticated; bind 127.0.0.1 or a Unix socket, or pass \
                             --allow-remote-control"
                        ),
                    ));
                }
                let listener = TcpListener::bind(addr).map_err(|e| {
                    ControlError::new(ErrorCode::Io, format!("cannot listen on tcp:{addr}: {e}"))
                })?;
                let local = listener.local_addr().map_err(|e| {
                    ControlError::new(ErrorCode::Io, format!("cannot read the bound address: {e}"))
                })?;
                Ok(Self {
                    listener: Listener::Tcp(listener),
                    endpoint: Endpoint::Tcp(local),
                })
            }
            #[cfg(unix)]
            Endpoint::Unix(path) => bind_unix(path),
            #[cfg(not(unix))]
            Endpoint::Unix(path) => Err(ControlError::new(
                ErrorCode::Unsupported,
                format!(
                    "Unix socket {path} is not available on this host; use tcp:<address>:<port>"
                ),
            )),
        }
    }

    /// The endpoint as bound.
    #[must_use]
    pub const fn endpoint(&self) -> &Endpoint {
        &self.endpoint
    }

    fn set_nonblocking(&self, on: bool) -> io::Result<()> {
        match &self.listener {
            Listener::Tcp(l) => l.set_nonblocking(on),
            #[cfg(unix)]
            Listener::Unix(l) => l.set_nonblocking(on),
        }
    }

    /// Accepts a connection, blocking, with the idle timeout on its reads
    /// (a client that sends nothing for that long is dropped, so idle
    /// connections do not pin threads for ever).
    fn accept(&self) -> io::Result<Conn> {
        match &self.listener {
            Listener::Tcp(l) => {
                let (s, _) = l.accept()?;
                s.set_nonblocking(false)?;
                s.set_nodelay(true)?;
                s.set_read_timeout(Some(IDLE_TIMEOUT))?;
                Ok(Conn::Tcp(s))
            }
            #[cfg(unix)]
            Listener::Unix(l) => {
                let (s, _) = l.accept()?;
                s.set_nonblocking(false)?;
                s.set_read_timeout(Some(IDLE_TIMEOUT))?;
                Ok(Conn::Unix(s))
            }
        }
    }

    /// Runs the accept loop until `stop` is set or the engine thread
    /// behind `handle` has ended.
    pub fn serve(self, handle: &DaemonHandle, stop: &AtomicBool) -> io::Result<()> {
        serve(self, handle, stop)
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        #[cfg(unix)]
        if let Endpoint::Unix(path) = &self.endpoint {
            let _ = std::fs::remove_file(path);
        }
    }
}

#[cfg(unix)]
fn bind_unix(path: &str) -> Result<Server, ControlError> {
    use std::os::unix::fs::PermissionsExt;
    use std::os::unix::net::{UnixListener, UnixStream};
    let p = std::path::Path::new(path);
    if p.exists() {
        if UnixStream::connect(p).is_ok() {
            return Err(ControlError::new(
                ErrorCode::Io,
                format!("another dbladed answers on unix:{path}"),
            ));
        }
        std::fs::remove_file(p).map_err(|e| {
            ControlError::new(
                ErrorCode::Io,
                format!("cannot remove the stale socket {path}: {e}"),
            )
        })?;
    }
    if let Some(dir) = p.parent() {
        if !dir.as_os_str().is_empty() && !dir.exists() {
            return Err(ControlError::new(
                ErrorCode::Io,
                format!(
                    "the socket directory {} does not exist; create it first (mkdir -p)",
                    dir.display()
                ),
            ));
        }
    }
    let listener = UnixListener::bind(p).map_err(|e| {
        ControlError::new(ErrorCode::Io, format!("cannot listen on unix:{path}: {e}"))
    })?;
    let _ = std::fs::set_permissions(p, std::fs::Permissions::from_mode(0o660));
    Ok(Server {
        listener: Listener::Unix(listener),
        endpoint: Endpoint::Unix(path.to_string()),
    })
}

/// Runs `server`'s accept loop until `stop` is set or the engine thread
/// behind `handle` has ended; every connection gets a thread that reads
/// requests and writes replies until the peer closes.
pub fn serve(server: Server, handle: &DaemonHandle, stop: &AtomicBool) -> io::Result<()> {
    server.set_nonblocking(true)?;
    loop {
        if stop.load(Ordering::Relaxed) || handle.is_stopped() {
            return Ok(());
        }
        match server.accept() {
            Ok(conn) => {
                let h = handle.clone();
                if let Err(e) = thread::Builder::new()
                    .name("dbladed-control".to_string())
                    .spawn(move || connection(conn, &h))
                {
                    eprintln!("dbladed: cannot start a connection thread: {e}");
                }
            }
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => thread::sleep(ACCEPT_POLL),
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(e) => {
                eprintln!("dbladed: accept failed: {e}");
                thread::sleep(Duration::from_millis(100));
            }
        }
    }
}

/// One connection: a request per frame, a reply per request. A frame that
/// is not a request gets an error reply and the connection goes on; a
/// broken stream ends it.
fn connection(mut conn: Conn, handle: &DaemonHandle) {
    loop {
        let reply: Reply = match read_frame::<Request, _>(&mut conn) {
            Ok(Some(req)) => handle.call(&req),
            Ok(None) | Err(WireError::Closed) => return,
            // An idle client (the read timeout) or a reset: the connection
            // ends quietly.
            Err(WireError::Io(e))
                if matches!(
                    e.kind(),
                    io::ErrorKind::WouldBlock
                        | io::ErrorKind::TimedOut
                        | io::ErrorKind::ConnectionReset
                ) =>
            {
                return
            }
            Err(WireError::Decode(e)) => Err(ControlError::new(
                ErrorCode::BadRequest,
                format!("the request does not decode: {e}"),
            )),
            Err(e) => {
                eprintln!("dbladed: control connection: {e}");
                return;
            }
        };
        if let Err(e) = write_frame(&mut conn, &reply) {
            match e {
                WireError::Closed => return,
                WireError::Encode(e) => {
                    // A reply that does not encode is the daemon's bug; the
                    // client still gets an answer and the connection goes on.
                    let fallback: Reply = Err(ControlError::new(
                        ErrorCode::Io,
                        format!("the reply does not encode: {e}"),
                    ));
                    eprintln!("dbladed: a reply did not encode: {e}");
                    if write_frame(&mut conn, &fallback).is_err() {
                        return;
                    }
                }
                other => {
                    eprintln!("dbladed: control connection: {other}");
                    return;
                }
            }
        }
    }
}
