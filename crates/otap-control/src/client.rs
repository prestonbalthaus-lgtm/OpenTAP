//! The endpoint syntax and the blocking client.

use core::fmt;
use std::io::{self, Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::time::Duration;

use crate::protocol::{ControlError, ErrorCode, Reply, Request, Response, PROTOCOL_VERSION};
use crate::wire::{read_frame, write_frame, WireError};

/// Where a daemon listens.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Endpoint {
    /// A Unix socket at this path (Linux).
    Unix(String),
    /// A TCP socket, loopback by default.
    Tcp(SocketAddr),
}

impl Endpoint {
    /// The default endpoint of this host: the Unix socket
    /// `/run/opentap/dbladed.sock` on Linux, loopback TCP port 7421
    /// elsewhere.
    #[must_use]
    pub fn default_for_host() -> Self {
        if cfg!(target_os = "linux") {
            Self::Unix("/run/opentap/dbladed.sock".to_string())
        } else {
            Self::Tcp(SocketAddr::from(([127, 0, 0, 1], 7421)))
        }
    }

    /// Parses `unix:<path>` or `tcp:<addr>:<port>`; a bare path is a Unix
    /// socket and a bare `addr:port` is TCP.
    pub fn parse(text: &str) -> Result<Self, String> {
        if let Some(path) = text.strip_prefix("unix:") {
            if path.is_empty() {
                return Err("an empty Unix socket path".into());
            }
            return Ok(Self::Unix(path.to_string()));
        }
        if let Some(addr) = text.strip_prefix("tcp:") {
            return addr
                .parse::<SocketAddr>()
                .map(Self::Tcp)
                .map_err(|e| format!("{addr:?} is not an address and port: {e}"));
        }
        if let Ok(addr) = text.parse::<SocketAddr>() {
            return Ok(Self::Tcp(addr));
        }
        if text.starts_with('/') || text.starts_with('.') {
            return Ok(Self::Unix(text.to_string()));
        }
        Err(format!(
            "{text:?} is neither unix:<path> nor tcp:<address>:<port>"
        ))
    }
}

impl fmt::Display for Endpoint {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Unix(path) => write!(f, "unix:{path}"),
            Self::Tcp(addr) => write!(f, "tcp:{addr}"),
        }
    }
}

/// A connected control stream.
enum Stream {
    Tcp(TcpStream),
    #[cfg(unix)]
    Unix(std::os::unix::net::UnixStream),
}

impl Read for Stream {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        match self {
            Self::Tcp(s) => s.read(buf),
            #[cfg(unix)]
            Self::Unix(s) => s.read(buf),
        }
    }
}

impl Write for Stream {
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

/// A blocking client: one connection, one request at a time.
pub struct Client {
    stream: Stream,
    endpoint: Endpoint,
}

impl fmt::Debug for Client {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Client")
            .field("endpoint", &self.endpoint)
            .finish_non_exhaustive()
    }
}

/// Why a call failed on the client's side.
#[derive(Debug)]
pub enum ClientError {
    /// Connecting failed.
    Connect(io::Error),
    /// The stream broke.
    Wire(WireError),
    /// The daemon answered an error.
    Daemon(ControlError),
    /// The endpoint kind is not available on this host.
    Unsupported(String),
}

impl fmt::Display for ClientError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Connect(e) => write!(f, "cannot reach dbladed: {e}"),
            Self::Wire(e) => write!(f, "{e}"),
            Self::Daemon(e) => write!(f, "{}", e.message),
            Self::Unsupported(s) => write!(f, "{s}"),
        }
    }
}

impl std::error::Error for ClientError {}

impl Client {
    /// Connects to `endpoint`, with `timeout` on the connect and on every
    /// read and write (`None` for no bound).
    pub fn connect(endpoint: &Endpoint, timeout: Option<Duration>) -> Result<Self, ClientError> {
        let stream = match endpoint {
            Endpoint::Tcp(addr) => {
                let s = match timeout {
                    Some(t) => TcpStream::connect_timeout(addr, t),
                    None => TcpStream::connect(addr),
                }
                .map_err(ClientError::Connect)?;
                s.set_read_timeout(timeout).map_err(ClientError::Connect)?;
                s.set_write_timeout(timeout).map_err(ClientError::Connect)?;
                s.set_nodelay(true).map_err(ClientError::Connect)?;
                Stream::Tcp(s)
            }
            #[cfg(unix)]
            Endpoint::Unix(path) => {
                let s =
                    std::os::unix::net::UnixStream::connect(path).map_err(ClientError::Connect)?;
                s.set_read_timeout(timeout).map_err(ClientError::Connect)?;
                s.set_write_timeout(timeout).map_err(ClientError::Connect)?;
                Stream::Unix(s)
            }
            #[cfg(not(unix))]
            Endpoint::Unix(path) => {
                return Err(ClientError::Unsupported(format!(
                    "Unix socket {path} is not available on this host; use tcp:<address>:<port>"
                )))
            }
        };
        Ok(Self {
            stream,
            endpoint: endpoint.clone(),
        })
    }

    /// The endpoint.
    #[must_use]
    pub const fn endpoint(&self) -> &Endpoint {
        &self.endpoint
    }

    /// Sends `request` and waits for the reply; a daemon error is
    /// [`ClientError::Daemon`].
    pub fn call(&mut self, request: &Request) -> Result<Response, ClientError> {
        write_frame(&mut self.stream, request).map_err(ClientError::Wire)?;
        let reply: Reply = read_frame(&mut self.stream)
            .map_err(ClientError::Wire)?
            .ok_or(ClientError::Wire(WireError::Closed))?;
        reply.map_err(ClientError::Daemon)
    }

    /// Pings and checks the protocol version.
    pub fn hello(&mut self) -> Result<Response, ClientError> {
        let pong = self.call(&Request::Ping)?;
        if let Response::Pong { protocol, .. } = &pong {
            if *protocol != PROTOCOL_VERSION {
                return Err(ClientError::Daemon(ControlError::new(
                    ErrorCode::Protocol,
                    format!(
                        "the daemon speaks protocol {protocol}, this client {PROTOCOL_VERSION}"
                    ),
                )));
            }
        }
        Ok(pong)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn endpoints_parse_and_print() {
        assert_eq!(
            Endpoint::parse("unix:/run/x.sock").unwrap(),
            Endpoint::Unix("/run/x.sock".into())
        );
        assert_eq!(
            Endpoint::parse("/run/x.sock").unwrap(),
            Endpoint::Unix("/run/x.sock".into())
        );
        assert_eq!(
            Endpoint::parse("tcp:127.0.0.1:7421").unwrap(),
            Endpoint::Tcp(SocketAddr::from(([127, 0, 0, 1], 7421)))
        );
        assert_eq!(
            Endpoint::parse("127.0.0.1:7421").unwrap(),
            Endpoint::Tcp(SocketAddr::from(([127, 0, 0, 1], 7421)))
        );
        assert!(Endpoint::parse("unix:").is_err());
        assert!(Endpoint::parse("tcp:nowhere").is_err());
        assert!(Endpoint::parse("nowhere").is_err());
        assert_eq!(
            Endpoint::parse("tcp:[::1]:9").unwrap().to_string(),
            "tcp:[::1]:9"
        );
        assert_eq!(Endpoint::parse("unix:/a").unwrap().to_string(), "unix:/a");
        let default = Endpoint::default_for_host();
        assert_eq!(Endpoint::parse(&default.to_string()).unwrap(), default);
    }
}
