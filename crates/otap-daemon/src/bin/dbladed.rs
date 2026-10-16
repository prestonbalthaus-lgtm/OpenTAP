//! `dbladed`: the OpenTAP data daemon. Parses the command line, binds the
//! control endpoint, boots the pool it was given (if any) on the engine
//! thread, and serves until `SIGTERM`, `SIGINT` or `otapctl daemon stop`.

use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::Arc;
use std::time::Duration;

use otap_control::{
    check_arena_bytes, check_cp_interval, parse_size, ArenaKind, Endpoint, MediaKind,
};
use otap_daemon::{
    install_signal_flag, serve, Daemon, DaemonConfig, Server, MAX_RECOVERY_TIMEOUT_SECS, VERSION,
};

const USAGE: &str = "\
dbladed: the OpenTAP data daemon

USAGE
    dbladed [OPTIONS]

OPTIONS
    --control <endpoint>    where to listen: unix:<path> (Linux) or tcp:<addr>:<port>
                            [default: unix:/run/opentap/dbladed.sock on Linux,
                            tcp:127.0.0.1:7421 elsewhere]
    --device <path>         boot the pool on this block device or file at start
                            (otherwise `otapctl pool boot` boots one later)
    --arena-bytes <size>    the write cache: at least 64M, a multiple of 2M [default: 8G]
    --arena-file <path>     the hugetlbfs file that backs the arena (Linux)
                            [default: /run/opentap/arena/dbladed]
    --heap-arena            back the arena with heap memory instead (no huge pages;
                            does not survive a crash) [default off Linux]
    --discard-arena         with --device: recreate an arena file made for another
                            --arena-bytes even if it holds writes not yet on disk;
                            they are destroyed and their volumes held (nvfail).
                            Without it such a boot is refused
    --cp-interval <secs>    the consistency point timer, 1 to 15 [default: 5]
    --media <hdd|flash>     the media, when the device cannot say (files)
    --stripe <size>         stripe override (must match the pool at boot)
    --segment <size>        segment override (must match the pool at boot)
    --flush-opaque          treat the device as flush-opaque (hardware RAID)
    --strict-flush          refuse a flush-opaque device
    --no-ublk               do not expose ublk devices (Linux)
    --state-dir <path>      where the record of the volumes' ublk devices lives, so a
                            daemon restarted after a crash re-attaches the same
                            /dev/ublkbN [default: /run/opentap on Linux]
    --recovery-timeout <secs>
                            how long a restarted daemon waits for the kernel to
                            quiesce a dead daemon's device before the volume gets a
                            new one, 1 to 300 [default: 5]
    --allow-remote-control  accept a TCP control address that is not loopback (the
                            control plane is unauthenticated: it can format any device)
    --op-timeout <secs>     how long a control request may wait for its
                            consistency point [default: 120]
    -h, --help              this text
    -V, --version           the version

Sizes take K, M, G or T. `otapctl` is the client; `otapctl --help` lists
its commands.
";

enum Parsed {
    Run(Box<DaemonConfig>),
    Help,
    Version,
}

fn parse(args: &[String]) -> Result<Parsed, String> {
    let mut cfg = DaemonConfig::default();
    let mut arena_file: Option<String> = None;
    let mut heap = false;
    let mut i = 0;
    let value = |i: &mut usize, flag: &str| -> Result<String, String> {
        *i += 1;
        args.get(*i)
            .cloned()
            .ok_or_else(|| format!("{flag} needs a value"))
    };
    while i < args.len() {
        let arg = args[i].as_str();
        let (flag, inline) = match arg.split_once('=') {
            Some((f, v)) if f.starts_with("--") => (f, Some(v.to_string())),
            _ => (arg, None),
        };
        let take = |i: &mut usize| -> Result<String, String> {
            match &inline {
                Some(v) => Ok(v.clone()),
                None => value(i, flag),
            }
        };
        match flag {
            "-h" | "--help" => return Ok(Parsed::Help),
            "-V" | "--version" => return Ok(Parsed::Version),
            "--control" => cfg.control = Endpoint::parse(&take(&mut i)?)?,
            "--device" => cfg.device = Some(PathBuf::from(take(&mut i)?)),
            "--arena-bytes" => cfg.arena_bytes = parse_size(&take(&mut i)?)?,
            "--arena-file" => arena_file = Some(take(&mut i)?),
            "--heap-arena" => heap = true,
            "--discard-arena" => cfg.discard_arena = true,
            "--cp-interval" => {
                let text = take(&mut i)?;
                cfg.cp_interval_secs = text
                    .parse()
                    .map_err(|_| format!("{text:?} is not a number of seconds"))?;
            }
            "--media" => {
                let text = take(&mut i)?;
                cfg.overrides.media = Some(match text.to_ascii_lowercase().as_str() {
                    "hdd" => MediaKind::Hdd,
                    "flash" | "ssd" | "nvme" => MediaKind::Flash,
                    _ => return Err(format!("{text:?} is neither hdd nor flash")),
                });
            }
            "--stripe" => {
                let bytes = parse_size(&take(&mut i)?)?;
                cfg.overrides.stripe_bytes =
                    Some(u32::try_from(bytes).map_err(|_| "a stripe above 4 GiB".to_string())?);
            }
            "--segment" => cfg.overrides.segment_bytes = Some(parse_size(&take(&mut i)?)?),
            "--flush-opaque" => cfg.overrides.flush_opaque = Some(true),
            "--strict-flush" => cfg.overrides.strict_flush = true,
            "--no-ublk" => cfg.ublk = false,
            "--state-dir" => {
                let text = take(&mut i)?;
                if text.is_empty() {
                    return Err("--state-dir needs a directory".to_string());
                }
                cfg.state_dir = PathBuf::from(text);
            }
            "--recovery-timeout" => {
                let text = take(&mut i)?;
                let secs: u64 = text
                    .parse()
                    .map_err(|_| format!("{text:?} is not a number of seconds"))?;
                if secs == 0 || secs > MAX_RECOVERY_TIMEOUT_SECS {
                    return Err(format!(
                        "--recovery-timeout takes 1 to {MAX_RECOVERY_TIMEOUT_SECS} seconds, not \
                         {secs}"
                    ));
                }
                cfg.recovery_timeout = Duration::from_secs(secs);
            }
            "--allow-remote-control" => cfg.allow_remote = true,
            "--op-timeout" => {
                let text = take(&mut i)?;
                let secs: u64 = text
                    .parse()
                    .map_err(|_| format!("{text:?} is not a number of seconds"))?;
                if secs == 0 {
                    return Err("--op-timeout needs at least one second".to_string());
                }
                cfg.op_timeout = Duration::from_secs(secs);
            }
            other => return Err(format!("unknown option {other:?}")),
        }
        i += 1;
    }
    if heap && arena_file.is_some() {
        return Err("--heap-arena and --arena-file exclude each other".to_string());
    }
    if cfg.discard_arena && cfg.device.is_none() {
        return Err(
            "--discard-arena applies to the pool booted at start: it needs --device".to_string(),
        );
    }
    if heap {
        cfg.arena = ArenaKind::Heap;
    } else if let Some(path) = arena_file {
        cfg.arena = ArenaKind::Hugetlb { path };
    }
    check_arena_bytes(cfg.arena_bytes)?;
    check_cp_interval(cfg.cp_interval_secs)?;
    Ok(Parsed::Run(Box::new(cfg)))
}

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let cfg = match parse(&args) {
        Ok(Parsed::Run(cfg)) => *cfg,
        Ok(Parsed::Help) => {
            print!("{USAGE}");
            return ExitCode::SUCCESS;
        }
        Ok(Parsed::Version) => {
            println!("dbladed {VERSION}");
            return ExitCode::SUCCESS;
        }
        Err(message) => {
            eprintln!("dbladed: {message}\n\n{USAGE}");
            return ExitCode::from(2);
        }
    };
    let stop = match install_signal_flag() {
        Ok(flag) => flag,
        Err(e) => {
            eprintln!("dbladed: cannot install the signal handlers: {e}");
            return ExitCode::FAILURE;
        }
    };
    let server = match Server::bind_with(&cfg.control, cfg.allow_remote) {
        Ok(server) => server,
        Err(e) => {
            eprintln!("dbladed: {}", e.message);
            return ExitCode::FAILURE;
        }
    };
    eprintln!("dbladed {VERSION} listening on {}", server.endpoint());
    let (handle, thread) = match Daemon::spawn(cfg, Arc::clone(&stop)) {
        Ok(parts) => parts,
        Err(e) => {
            eprintln!("dbladed: {}", e.message);
            return ExitCode::FAILURE;
        }
    };
    if let Err(e) = serve(server, &handle, &stop) {
        eprintln!("dbladed: the control server failed: {e}");
    }
    drop(handle);
    let outcome = thread.join();
    eprintln!("dbladed: stopped");
    if outcome.is_ok() {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    }
}
