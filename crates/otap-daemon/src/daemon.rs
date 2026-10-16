//! The daemon: one engine thread that owns the pool, ticks it, and answers
//! every control request between ticks.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender, TryRecvError};
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::Duration;

use std::cell::RefCell;
use std::rc::Rc;

use otap_control::{ControlError, ErrorCode, Reply, Request, Response, PROTOCOL_VERSION};
use otap_ublk::Owner;

use crate::clock::SystemClock;
use crate::config::{DaemonConfig, Overrides};
use crate::device::{no_ublk, HostRef};
use crate::pool::{BootSpec, Pool};

/// A request and where its reply goes.
type Call = (Request, Sender<Reply>);

/// The daemon's state: its configuration and the pool, if one is booted.
pub struct Daemon {
    cfg: DaemonConfig,
    clock: SystemClock,
    pool: Option<Pool>,
    exit: bool,
    /// The stop flag the signals set, handed to the pool so a long
    /// operation gives way to the shutdown.
    stop: Option<Arc<AtomicBool>>,
    /// The ublk control plane (when `cfg.ublk` and the host has one),
    /// shared with the booted pool.
    host: Option<HostRef>,
}

impl std::fmt::Debug for Daemon {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Daemon")
            .field("control", &self.cfg.control)
            .field("pool", &self.pool)
            .field("exit", &self.exit)
            .finish()
    }
}

fn no_pool() -> ControlError {
    ControlError::new(ErrorCode::NoPool, "no pool is booted (`otapctl pool boot`)")
}

fn pool_booted(what: &str) -> ControlError {
    ControlError::new(
        ErrorCode::PoolBooted,
        format!("a pool is booted; stop it (`otapctl pool stop`) before {what}"),
    )
}

impl Daemon {
    /// A daemon with no pool booted. Its ublk control plane is started
    /// here when the configuration asks for devices (the kernel's opens
    /// `/dev/ublk-control` only at its first command).
    #[must_use]
    pub fn new(cfg: DaemonConfig) -> Self {
        let host = if cfg.ublk {
            cfg.ublk_backend.start().map(|h| Rc::new(RefCell::new(h)))
        } else {
            None
        };
        Self {
            cfg,
            clock: SystemClock::new(),
            pool: None,
            exit: false,
            stop: None,
            host,
        }
    }

    /// [`new`](Self::new) with the stop flag a signal sets: an operation
    /// waiting for its consistency point gives up when it is set, so the
    /// shutdown does not wait for the operation timeout.
    #[must_use]
    pub fn with_stop_flag(cfg: DaemonConfig, stop: Arc<AtomicBool>) -> Self {
        Self {
            stop: Some(stop),
            ..Self::new(cfg)
        }
    }

    /// The configuration.
    #[must_use]
    pub const fn config(&self) -> &DaemonConfig {
        &self.cfg
    }

    /// The booted pool, if any.
    #[must_use]
    pub const fn pool(&self) -> Option<&Pool> {
        self.pool.as_ref()
    }

    /// `true` after `DaemonStop`, a signal, or the loss of every handle.
    #[must_use]
    pub const fn exit_requested(&self) -> bool {
        self.exit
    }

    /// Boots the pool the configuration names, if it names one.
    pub fn boot_configured(&mut self) -> Result<(), ControlError> {
        let Some(device) = self.cfg.device.clone() else {
            return Ok(());
        };
        let spec = BootSpec {
            device,
            overrides: self.cfg.overrides.clone(),
            arena_bytes: self.cfg.arena_bytes,
            arena: self.cfg.arena.clone(),
            cp_interval_secs: self.cfg.cp_interval_secs,
            ublk: self.cfg.ublk,
            op_timeout: self.cfg.op_timeout,
            discard_arena: self.cfg.discard_arena,
            state_dir: self.cfg.state_dir.clone(),
            recovery_timeout: self.cfg.recovery_timeout,
        };
        self.pool = Some(Pool::boot(
            &spec,
            &self.clock,
            self.stop.clone(),
            self.host.clone(),
        )?);
        Ok(())
    }

    /// `device list` and `device reap` with no pool booted: every device
    /// is classified without a pool's knowledge (an OpenTAP device of a
    /// dead daemon is recoverable by its pool, any other with no live
    /// server is stale).
    fn devices_without_pool(&mut self, req: &Request) -> Reply {
        let host = self.host.clone().ok_or_else(no_ublk)?;
        let wants = |_: u16| false;
        let name_of = |_: u32| None;
        let owner = Owner {
            pool: None,
            serving: &[],
            stale: &[],
            wants: &wants,
        };
        let mut host = host.borrow_mut();
        match req {
            Request::DeviceReap { dev_id, stale } => {
                crate::devices::reap(&mut **host, &owner, &name_of, *dev_id, *stale)?.into_reply()
            }
            _ => Ok(Response::Devices {
                devices: crate::devices::list(&mut **host, &owner, &name_of)?,
            }),
        }
    }

    /// One tick of the pool; `true` when it has work in hand.
    pub fn tick(&mut self) -> bool {
        match &mut self.pool {
            Some(pool) => match pool.tick() {
                Ok(busy) => busy,
                Err(e) => {
                    eprintln!("dbladed: {}", e.message);
                    false
                }
            },
            None => false,
        }
    }

    /// Answers one request.
    pub fn handle(&mut self, req: &Request) -> Reply {
        match req {
            Request::Ping => Ok(Response::Pong {
                protocol: PROTOCOL_VERSION,
                version: crate::VERSION.to_string(),
                pool_booted: self.pool.is_some(),
            }),
            Request::PoolFormat {
                device,
                size_bytes,
                media,
                stripe_bytes,
                segment_bytes,
                flush_opaque,
                strict_flush,
                force,
            } => {
                if self.pool.is_some() {
                    return Err(pool_booted("formatting"));
                }
                let overrides = Overrides {
                    media: *media,
                    stripe_bytes: *stripe_bytes,
                    segment_bytes: *segment_bytes,
                    flush_opaque: *flush_opaque,
                    strict_flush: *strict_flush,
                };
                Pool::format(
                    Path::new(device),
                    &overrides,
                    *size_bytes,
                    *force,
                    &self.clock,
                )
            }
            Request::PoolBoot {
                device,
                media,
                stripe_bytes,
                segment_bytes,
                flush_opaque,
                strict_flush,
                arena_bytes,
                arena,
                cp_interval_secs,
            } => {
                if self.pool.is_some() {
                    return Err(pool_booted("booting another"));
                }
                let spec = BootSpec {
                    device: PathBuf::from(device),
                    overrides: Overrides {
                        media: *media,
                        stripe_bytes: *stripe_bytes,
                        segment_bytes: *segment_bytes,
                        flush_opaque: *flush_opaque,
                        strict_flush: *strict_flush,
                    },
                    arena_bytes: *arena_bytes,
                    arena: arena.clone(),
                    cp_interval_secs: *cp_interval_secs,
                    ublk: self.cfg.ublk,
                    op_timeout: self.cfg.op_timeout,
                    // Destroying an arena is asked for on dbladed's command
                    // line only, for the pool it boots at start.
                    discard_arena: false,
                    state_dir: self.cfg.state_dir.clone(),
                    recovery_timeout: self.cfg.recovery_timeout,
                };
                let pool = Pool::boot(&spec, &self.clock, self.stop.clone(), self.host.clone())?;
                let reply = pool.status();
                self.pool = Some(pool);
                reply
            }
            Request::PoolStop => {
                let mut pool = self.pool.take().ok_or_else(no_pool)?;
                pool.stop()
            }
            Request::DaemonStop => {
                let reply = match self.pool.take() {
                    Some(mut pool) => pool.stop().map(|_| Response::Ok),
                    None => Ok(Response::Ok),
                };
                self.exit = true;
                reply
            }
            Request::DeviceList {} | Request::DeviceReap { .. } if self.pool.is_none() => {
                self.devices_without_pool(req)
            }
            other => {
                let pool = self.pool.as_mut().ok_or_else(no_pool)?;
                pool.handle(other)
            }
        }
    }

    /// Shuts the pool down cleanly, if one is booted, and marks the daemon
    /// for exit.
    pub fn shutdown(&mut self) {
        if let Some(mut pool) = self.pool.take() {
            match pool.stop() {
                Ok(_) => eprintln!("dbladed: pool {} stopped cleanly", pool.device()),
                Err(e) => eprintln!("dbladed: {}", e.message),
            }
        }
        self.exit = true;
    }

    /// Starts the engine thread: it boots the configured pool (an error
    /// is returned here and the thread ends), then serves requests from
    /// the returned handle until the handle's last clone is dropped,
    /// `DaemonStop` arrives, or `stop` is set (a signal), shutting the
    /// pool down cleanly on every one of those.
    pub fn spawn(
        cfg: DaemonConfig,
        stop: Arc<AtomicBool>,
    ) -> Result<(DaemonHandle, JoinHandle<()>), ControlError> {
        let (tx, rx) = mpsc::channel::<Call>();
        let (boot_tx, boot_rx) = mpsc::channel::<Result<(), ControlError>>();
        let stopped = Arc::new(AtomicBool::new(false));
        let handle = DaemonHandle {
            tx,
            stopped: Arc::clone(&stopped),
        };
        let thread = std::thread::Builder::new()
            .name("dbladed-engine".to_string())
            .spawn(move || {
                let mut daemon = Self::with_stop_flag(cfg, Arc::clone(&stop));
                let booted = daemon.boot_configured();
                let failed = booted.is_err();
                let _ = boot_tx.send(booted);
                if !failed {
                    daemon.run(&rx, &stop);
                }
                stopped.store(true, Ordering::SeqCst);
            })
            .map_err(|e| {
                ControlError::new(
                    ErrorCode::Io,
                    format!("cannot start the engine thread: {e}"),
                )
            })?;
        match boot_rx.recv() {
            Ok(Ok(())) => Ok((handle, thread)),
            Ok(Err(e)) => {
                let _ = thread.join();
                Err(e)
            }
            Err(_) => {
                let _ = thread.join();
                Err(ControlError::new(
                    ErrorCode::Io,
                    "the engine thread ended before it reported its boot",
                ))
            }
        }
    }

    /// The engine thread's loop.
    fn run(&mut self, rx: &Receiver<Call>, stop: &AtomicBool) {
        let mut idle_streak: u32 = 0;
        loop {
            if stop.load(Ordering::Relaxed) {
                self.exit = true;
            }
            if self.exit {
                self.shutdown();
                return;
            }
            let busy = self.tick();
            idle_streak = if busy {
                0
            } else {
                idle_streak.saturating_add(1)
            };
            let call = if busy {
                match rx.try_recv() {
                    Ok(call) => Some(call),
                    Err(TryRecvError::Empty) => None,
                    Err(TryRecvError::Disconnected) => {
                        self.exit = true;
                        continue;
                    }
                }
            } else {
                match rx.recv_timeout(idle_wait(idle_streak, self.pool.is_some())) {
                    Ok(call) => Some(call),
                    Err(RecvTimeoutError::Timeout) => None,
                    Err(RecvTimeoutError::Disconnected) => {
                        self.exit = true;
                        continue;
                    }
                }
            };
            if let Some((req, reply)) = call {
                let answer = self.handle(&req);
                let _ = reply.send(answer);
            }
        }
    }
}

/// How long the engine thread waits for a request when the last tick did
/// nothing: from 200 µs, doubling with the idle streak to 2 ms, so an idle
/// daemon costs little and a request on a device waits at most 2 ms; 50 ms
/// with no pool booted.
fn idle_wait(streak: u32, pool: bool) -> Duration {
    if !pool {
        return Duration::from_millis(50);
    }
    let micros = 200u64 << streak.min(4);
    Duration::from_micros(micros.min(2_000))
}

/// A handle that sends requests to the engine thread from any thread.
#[derive(Debug, Clone)]
pub struct DaemonHandle {
    tx: Sender<Call>,
    stopped: Arc<AtomicBool>,
}

impl DaemonHandle {
    /// Sends `req` and waits for the reply.
    pub fn call(&self, req: &Request) -> Reply {
        let (reply_tx, reply_rx) = mpsc::channel();
        self.tx
            .send((req.clone(), reply_tx))
            .map_err(|_| ControlError::new(ErrorCode::Io, "the daemon has stopped"))?;
        reply_rx.recv().map_err(|_| {
            ControlError::new(ErrorCode::Io, "the daemon stopped before it answered")
        })?
    }

    /// `true` once the engine thread has ended.
    #[must_use]
    pub fn is_stopped(&self) -> bool {
        self.stopped.load(Ordering::SeqCst)
    }
}

/// Installs `SIGTERM` and `SIGINT` handlers that set the returned flag,
/// which the engine thread polls for a clean shutdown.
pub fn install_signal_flag() -> std::io::Result<Arc<AtomicBool>> {
    let flag = Arc::new(AtomicBool::new(false));
    signal_hook::flag::register(signal_hook::consts::SIGTERM, Arc::clone(&flag))?;
    signal_hook::flag::register(signal_hook::consts::SIGINT, Arc::clone(&flag))?;
    Ok(flag)
}
