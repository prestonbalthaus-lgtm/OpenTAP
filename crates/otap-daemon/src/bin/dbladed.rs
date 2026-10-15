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
use otap_daemon::{install_signal_flag, serve, Daemon, DaemonConfig, Server, VERSION};

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
    --cp-interval <secs>    the consistency point timer, 1 to 15 [default: 5]
    --media <hdd|flash>     the media, when the device cannot say (files)
    --stripe <size>         stripe override (must match the pool at boot)
    --segment <size>        segment override (must match the pool at boot)
    --flush-opaque          treat the device as flush-opaque (hardware RAID)
    --strict-flush          refuse a flush-opaque device
    --no-ublk               do not expose ublk devices (Linux)
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
    Run(DaemonConfig),
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
    if heap {
        cfg.arena = ArenaKind::Heap;
    } else if let Some(path) = arena_file {
        cfg.arena = ArenaKind::Hugetlb { path };
    }
    check_arena_bytes(cfg.arena_bytes)?;
    check_cp_interval(cfg.cp_interval_secs)?;
    Ok(Parsed::Run(cfg))
}

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let cfg = match parse(&args) {
        Ok(Parsed::Run(cfg)) => cfg,
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
