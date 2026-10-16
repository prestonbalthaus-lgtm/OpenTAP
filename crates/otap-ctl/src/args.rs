//! The command line: `otapctl [global options] <group> <verb> [--flags]`.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::time::Duration;

use otap_control::{
    encode_hex, parse_size, ArenaKind, Endpoint, MediaKind, Request, BLOCK_SIZE,
    DEFAULT_ARENA_BYTES, DEFAULT_CP_INTERVAL_SECS, MAX_IO_BLOCKS, PIN_CLONE, PIN_SCHEDULE,
    PIN_USER,
};

/// The help text.
pub const USAGE: &str = "\
otapctl: the OpenTAP control client

USAGE
    otapctl [--control <endpoint>] [--json] [--timeout <secs>] <group> <verb> [--flags]

GLOBAL OPTIONS
    --control <endpoint>  the daemon: unix:<path> or tcp:<addr>:<port>
                          [default: $OTAPCTL_CONTROL, else unix:/run/opentap/dbladed.sock
                          on Linux and tcp:127.0.0.1:7421 elsewhere]
    --json                print the reply as JSON
    --timeout <secs>      give up waiting for a reply after this long [default: 600]

COMMANDS
    daemon ping
    daemon stop                              clean shutdown of the pool, then exit

    pool format --device <path> [--size <size>] [--media hdd|flash]
                [--stripe <size>] [--segment <size>] [--flush-opaque]
                [--strict-flush] [--force]    (--size creates a file)
    pool boot --device <path> [--arena-bytes <size>] [--arena-file <path> | --heap-arena]
              [--cp-interval <secs>] [--media hdd|flash] [--stripe <size>]
              [--segment <size>] [--flush-opaque] [--strict-flush]
    pool status
    pool sync                                a consistency point now
    pool clean                               a cleaning consistency point now
    pool verify [--no-wait | --status | --cancel]
                                             attest the pool: the superblock ring,
                                             every table, tree and live block, and
                                             the accounting; waits for the report
    pool stop                                clean shutdown; the daemon keeps running

    volume create --name <name> --size <size>
    volume list
    volume show --name <name>
    volume delete --name <name>              (offline, no snapshots)
    volume online --name <name>
    volume offline --name <name>
    volume resize --name <name> --size <size>   (a shrink needs it offline)
    volume rename --name <name> --new-name <name>
    volume nvfail-clear --name <name>

    snapshot create --volume <name> --name <name>
    snapshot list [--volume <name>]
    snapshot delete --volume <name> --name <name>
    snapshot rollback --volume <name> --name <name> [--delete-newer]
    snapshot pin --volume <name> --name <name> [--user] [--schedule] [--clone]
    snapshot unpin --volume <name> --name <name>
    snapshot replication-base --volume <name> (--name <name> | --release)

    io write --volume <name> --lba <n> (--hex <hex> | --file <path> | --fill <byte> --blocks <n>)
    io read --volume <name> --lba <n> [--blocks <n>] [--out <path>]
    io punch --volume <name> --lba <n> --blocks <n>

    device list                              the host's ublk devices: serving,
                                             recoverable, stale or foreign
    device reap (<id> | --stale)             delete a stale device (or, by id, a
                                             recoverable one), or every stale one

Sizes take K, M, G or T (binary). LBAs and block counts are 4 KiB blocks;
an io request moves at most 256 blocks (1 MiB).
A volume whose name was lost (a damaged catalogue) is addressed as #<index>
until it is named again with `volume rename`.
A device id is N of /dev/ublkbN, given as N or as the path.

EXIT STATUS
    0 a reply; 1 the daemon refused; 2 the command line does not parse;
    3 the daemon cannot be reached; 4 a `pool verify` that waited for its
    report did not conclude clean (findings, or it did not finish).
";

/// The default reply timeout.
const DEFAULT_TIMEOUT: Duration = Duration::from_secs(600);

/// Flags that take no value.
const BOOL_FLAGS: &[&str] = &[
    "--json",
    "--force",
    "--heap-arena",
    "--flush-opaque",
    "--strict-flush",
    "--delete-newer",
    "--user",
    "--schedule",
    "--clone",
    "--release",
    "--stale",
    "--no-wait",
    "--status",
    "--cancel",
];

/// A parsed command.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Command {
    /// The daemon to talk to.
    pub control: Endpoint,
    /// Print JSON.
    pub json: bool,
    /// The reply timeout.
    pub timeout: Duration,
    /// What to ask.
    pub request: Request,
    /// For `io read --out`: where the data goes.
    pub read_to: Option<PathBuf>,
    /// For `pool verify`: follow the verification until it ends and print
    /// its final report (without `--no-wait`).
    pub wait_verify: bool,
}

/// What a command line asked for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Parsed {
    /// Run a command.
    Run(Command),
    /// Print the help.
    Help,
    /// Print the version.
    Version,
}

/// The flags of a command, consumed as the command is built.
#[derive(Debug, Default)]
struct Flags {
    values: BTreeMap<String, Option<String>>,
    context: String,
}

impl Flags {
    fn take(&mut self, name: &str) -> Option<Option<String>> {
        self.values.remove(name)
    }

    fn string(&mut self, name: &str) -> Result<Option<String>, String> {
        match self.take(name) {
            None => Ok(None),
            Some(Some(v)) => Ok(Some(v)),
            Some(None) => Err(format!("{name} needs a value")),
        }
    }

    fn required(&mut self, name: &str) -> Result<String, String> {
        self.string(name)?
            .ok_or_else(|| format!("`{}` needs {name}", self.context))
    }

    fn flag(&mut self, name: &str) -> bool {
        self.take(name).is_some()
    }

    fn size(&mut self, name: &str) -> Result<Option<u64>, String> {
        self.string(name)?
            .map(|v| parse_size(&v).map_err(|e| format!("{name}: {e}")))
            .transpose()
    }

    fn number<T: std::str::FromStr>(&mut self, name: &str) -> Result<Option<T>, String> {
        self.string(name)?
            .map(|v| {
                v.parse::<T>()
                    .map_err(|_| format!("{name}: {v:?} is not a number"))
            })
            .transpose()
    }

    fn media(&mut self) -> Result<Option<MediaKind>, String> {
        self.string("--media")?
            .map(|v| match v.to_ascii_lowercase().as_str() {
                "hdd" => Ok(MediaKind::Hdd),
                "flash" | "ssd" | "nvme" => Ok(MediaKind::Flash),
                _ => Err(format!("--media: {v:?} is neither hdd nor flash")),
            })
            .transpose()
    }

    fn stripe(&mut self) -> Result<Option<u32>, String> {
        self.size("--stripe")?
            .map(|s| u32::try_from(s).map_err(|_| "--stripe: above 4 GiB".to_string()))
            .transpose()
    }

    fn done(self) -> Result<(), String> {
        if let Some(name) = self.values.keys().next() {
            return Err(format!("`{}` does not take {name}", self.context));
        }
        Ok(())
    }
}

/// Parses a command line (without the program name).
pub fn parse(args: &[String]) -> Result<Parsed, String> {
    let mut words: Vec<String> = Vec::new();
    let mut flags = Flags::default();
    let mut i = 0;
    while i < args.len() {
        let arg = args[i].as_str();
        if arg == "-h" || arg == "--help" {
            return Ok(Parsed::Help);
        }
        if arg == "-V" || arg == "--version" {
            return Ok(Parsed::Version);
        }
        if let Some(rest) = arg.strip_prefix("--") {
            if rest.is_empty() {
                return Err("a bare `--`".to_string());
            }
            if let Some((name, value)) = rest.split_once('=') {
                flags
                    .values
                    .insert(format!("--{name}"), Some(value.to_string()));
            } else if BOOL_FLAGS.contains(&arg) {
                flags.values.insert(arg.to_string(), None);
            } else {
                i += 1;
                let value = args.get(i).ok_or_else(|| format!("{arg} needs a value"))?;
                flags.values.insert(arg.to_string(), Some(value.clone()));
            }
        } else if arg.starts_with('-') && arg.len() > 1 {
            return Err(format!("unknown option {arg:?}"));
        } else {
            words.push(arg.to_string());
        }
        i += 1;
    }
    let control = match flags.string("--control")? {
        Some(text) => Endpoint::parse(&text)?,
        None => match std::env::var("OTAPCTL_CONTROL") {
            Ok(text) if !text.is_empty() => Endpoint::parse(&text)?,
            _ => Endpoint::default_for_host(),
        },
    };
    let json = flags.flag("--json");
    let timeout = flags
        .number::<u64>("--timeout")?
        .map_or(DEFAULT_TIMEOUT, Duration::from_secs);
    if timeout.is_zero() {
        return Err("--timeout needs at least one second".to_string());
    }
    let (group, verb, rest) = match words.as_slice() {
        [] => return Err("no command given".to_string()),
        [group] => return Err(format!("`{group}` needs a verb")),
        [group, verb, rest @ ..] => (group, verb, rest),
    };
    // Only `device reap` takes a word after its verb: the device.
    let operand = match (group.as_str(), verb.as_str(), rest) {
        (_, _, []) => None,
        ("device", "reap", [device]) => Some(device.as_str()),
        _ => return Err(format!("too many words: {words:?}")),
    };
    flags.context = format!("{group} {verb}");
    let mut read_to = None;
    let mut wait_verify = false;
    let request = match (group.as_str(), verb.as_str()) {
        ("daemon", "ping") => Request::Ping,
        ("daemon", "stop") => Request::DaemonStop,
        ("pool", "format") => Request::PoolFormat {
            device: flags.required("--device")?,
            size_bytes: flags.size("--size")?,
            media: flags.media()?,
            stripe_bytes: flags.stripe()?,
            segment_bytes: flags.size("--segment")?,
            flush_opaque: flags.flag("--flush-opaque").then_some(true),
            strict_flush: flags.flag("--strict-flush"),
            force: flags.flag("--force"),
        },
        ("pool", "boot") => {
            let heap = flags.flag("--heap-arena");
            let file = flags.string("--arena-file")?;
            let arena = match (heap, file) {
                (true, Some(_)) => {
                    return Err("--heap-arena and --arena-file exclude each other".to_string())
                }
                (true, None) => ArenaKind::Heap,
                (false, Some(path)) => ArenaKind::Hugetlb { path },
                (false, None) => ArenaKind::default_for_host(),
            };
            Request::PoolBoot {
                device: flags.required("--device")?,
                media: flags.media()?,
                stripe_bytes: flags.stripe()?,
                segment_bytes: flags.size("--segment")?,
                flush_opaque: flags.flag("--flush-opaque").then_some(true),
                strict_flush: flags.flag("--strict-flush"),
                arena_bytes: flags.size("--arena-bytes")?.unwrap_or(DEFAULT_ARENA_BYTES),
                arena,
                cp_interval_secs: flags
                    .number::<u32>("--cp-interval")?
                    .unwrap_or(DEFAULT_CP_INTERVAL_SECS),
            }
        }
        ("pool", "status") => Request::PoolStatus,
        ("pool", "sync") => Request::PoolSync,
        ("pool", "clean") => Request::PoolClean,
        ("pool", "verify") => {
            let no_wait = flags.flag("--no-wait");
            let status = flags.flag("--status");
            let cancel = flags.flag("--cancel");
            match (no_wait, status, cancel) {
                (true, true, _) | (true, _, true) | (_, true, true) => {
                    return Err(
                        "`pool verify` takes one of --no-wait, --status and --cancel".to_string(),
                    )
                }
                (_, true, false) => Request::PoolVerifyStatus {},
                (_, false, true) => Request::PoolVerifyCancel {},
                (no_wait, false, false) => {
                    wait_verify = !no_wait;
                    Request::PoolVerify {}
                }
            }
        }
        ("pool", "stop") => Request::PoolStop,
        ("volume", "create") => Request::VolumeCreate {
            name: flags.required("--name")?,
            size_bytes: flags
                .size("--size")?
                .ok_or_else(|| "`volume create` needs --size".to_string())?,
        },
        ("volume", "list") => Request::VolumeList,
        ("volume", "show") => Request::VolumeShow {
            name: flags.required("--name")?,
        },
        ("volume", "delete") => Request::VolumeDelete {
            name: flags.required("--name")?,
        },
        ("volume", "online") => Request::VolumeOnline {
            name: flags.required("--name")?,
            online: true,
        },
        ("volume", "offline") => Request::VolumeOnline {
            name: flags.required("--name")?,
            online: false,
        },
        ("volume", "resize") => Request::VolumeResize {
            name: flags.required("--name")?,
            size_bytes: flags
                .size("--size")?
                .ok_or_else(|| "`volume resize` needs --size".to_string())?,
        },
        ("volume", "rename") => Request::VolumeRename {
            name: flags.required("--name")?,
            new_name: flags.required("--new-name")?,
        },
        ("volume", "nvfail-clear") => Request::VolumeNvfailClear {
            name: flags.required("--name")?,
        },
        ("snapshot", "create") => Request::SnapshotCreate {
            volume: flags.required("--volume")?,
            name: flags.required("--name")?,
        },
        ("snapshot", "list") => Request::SnapshotList {
            volume: flags.string("--volume")?,
        },
        ("snapshot", "delete") => Request::SnapshotDelete {
            volume: flags.required("--volume")?,
            name: flags.required("--name")?,
        },
        ("snapshot", "rollback") => Request::SnapshotRollback {
            volume: flags.required("--volume")?,
            name: flags.required("--name")?,
            delete_newer: flags.flag("--delete-newer"),
        },
        ("snapshot", "pin") => {
            let mut pins = 0u8;
            if flags.flag("--user") {
                pins |= PIN_USER;
            }
            if flags.flag("--schedule") {
                pins |= PIN_SCHEDULE;
            }
            if flags.flag("--clone") {
                pins |= PIN_CLONE;
            }
            if pins == 0 {
                pins = PIN_USER;
            }
            Request::SnapshotPin {
                volume: flags.required("--volume")?,
                name: flags.required("--name")?,
                flags: pins,
            }
        }
        ("snapshot", "unpin") => Request::SnapshotPin {
            volume: flags.required("--volume")?,
            name: flags.required("--name")?,
            flags: 0,
        },
        ("snapshot", "replication-base") => {
            let release = flags.flag("--release");
            let name = flags.string("--name")?;
            if release == name.is_some() {
                return Err(
                    "`snapshot replication-base` takes --name <snapshot> or --release".to_string(),
                );
            }
            Request::SnapshotReplicationBase {
                volume: flags.required("--volume")?,
                name,
            }
        }
        ("io", "write") => {
            let volume = flags.required("--volume")?;
            let lba = flags
                .number::<u64>("--lba")?
                .ok_or_else(|| "`io write` needs --lba".to_string())?;
            let hex = flags.string("--hex")?;
            let file = flags.string("--file")?;
            let fill = flags.string("--fill")?;
            let data_hex = match (hex, file, fill) {
                (Some(hex), None, None) => hex,
                (None, Some(path), None) => {
                    let bytes = std::fs::read(&path)
                        .map_err(|e| format!("--file: cannot read {path}: {e}"))?;
                    encode_hex(&bytes)
                }
                (None, None, Some(byte)) => {
                    let byte = parse_byte(&byte)?;
                    let blocks = flags
                        .number::<u32>("--blocks")?
                        .ok_or_else(|| "--fill needs --blocks".to_string())?;
                    check_blocks(blocks)?;
                    encode_hex(&vec![byte; blocks as usize * BLOCK_SIZE])
                }
                _ => {
                    return Err(
                        "`io write` takes exactly one of --hex, --file or --fill".to_string()
                    )
                }
            };
            let bytes = data_hex.len() / 2;
            if data_hex.len() % 2 != 0 || bytes == 0 || bytes % BLOCK_SIZE != 0 {
                return Err(format!(
                    "the data is {bytes} bytes; a write is whole 4 KiB blocks, at least one"
                ));
            }
            check_blocks((bytes / BLOCK_SIZE) as u32)?;
            Request::IoWrite {
                volume,
                lba,
                data_hex,
            }
        }
        ("io", "read") => {
            let blocks = flags.number::<u32>("--blocks")?.unwrap_or(1);
            check_blocks(blocks)?;
            read_to = flags.string("--out")?.map(PathBuf::from);
            Request::IoRead {
                volume: flags.required("--volume")?,
                lba: flags
                    .number::<u64>("--lba")?
                    .ok_or_else(|| "`io read` needs --lba".to_string())?,
                blocks,
            }
        }
        ("io", "punch") => {
            let blocks = flags
                .number::<u32>("--blocks")?
                .ok_or_else(|| "`io punch` needs --blocks".to_string())?;
            if blocks == 0 {
                return Err("--blocks: at least one".to_string());
            }
            Request::IoPunch {
                volume: flags.required("--volume")?,
                lba: flags
                    .number::<u64>("--lba")?
                    .ok_or_else(|| "`io punch` needs --lba".to_string())?,
                blocks,
            }
        }
        ("device", "list") => Request::DeviceList {},
        ("device", "reap") => {
            let stale = flags.flag("--stale");
            let dev_id = operand.map(parse_device).transpose()?;
            match (dev_id, stale) {
                (Some(_), true) => {
                    return Err("`device reap` takes a device id or --stale, not both".to_string())
                }
                (None, false) => {
                    return Err("`device reap` needs a device id or --stale".to_string())
                }
                _ => Request::DeviceReap { dev_id, stale },
            }
        }
        ("daemon" | "pool" | "volume" | "snapshot" | "io" | "device", verb) => {
            return Err(format!("`{group}` has no verb {verb:?}"))
        }
        (group, _) => return Err(format!("unknown command group {group:?}")),
    };
    flags.done()?;
    Ok(Parsed::Run(Command {
        control,
        json,
        timeout,
        request,
        read_to,
        wait_verify,
    }))
}

/// A ublk device as `N`, `ublkbN` or `/dev/ublkbN`.
fn parse_device(text: &str) -> Result<u32, String> {
    let digits = text
        .strip_prefix("/dev/ublkb")
        .or_else(|| text.strip_prefix("ublkb"))
        .unwrap_or(text);
    if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return Err(format!(
            "{text:?} is not a ublk device (N, ublkbN or /dev/ublkbN)"
        ));
    }
    digits
        .parse::<u32>()
        .map_err(|_| format!("{text:?} is not a ublk device number"))
}

fn check_blocks(blocks: u32) -> Result<(), String> {
    if blocks == 0 || blocks > MAX_IO_BLOCKS {
        return Err(format!(
            "--blocks: {blocks}; a request moves 1 to {MAX_IO_BLOCKS} blocks"
        ));
    }
    Ok(())
}

/// A byte as `0xAB` or decimal.
fn parse_byte(text: &str) -> Result<u8, String> {
    let lower = text.trim().to_ascii_lowercase();
    let parsed = match lower.strip_prefix("0x") {
        Some(hex) => u8::from_str_radix(hex, 16),
        None => lower.parse::<u8>(),
    };
    parsed.map_err(|_| format!("--fill: {text:?} is not a byte (0..=255 or 0x00..=0xff)"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(line: &str) -> Vec<String> {
        line.split_whitespace().map(str::to_string).collect()
    }

    fn request(line: &str) -> Request {
        match parse(&args(line)).unwrap() {
            Parsed::Run(cmd) => cmd.request,
            other => panic!("{line}: {other:?}"),
        }
    }

    #[test]
    fn global_options_and_help() {
        assert_eq!(parse(&args("--help")).unwrap(), Parsed::Help);
        assert_eq!(parse(&args("pool status -h")).unwrap(), Parsed::Help);
        assert_eq!(parse(&args("--version")).unwrap(), Parsed::Version);
        let Parsed::Run(cmd) = parse(&args(
            "--control tcp:127.0.0.1:9 --json --timeout 7 pool status",
        ))
        .unwrap() else {
            panic!("a command")
        };
        assert_eq!(cmd.control.to_string(), "tcp:127.0.0.1:9");
        assert!(cmd.json);
        assert_eq!(cmd.timeout, Duration::from_secs(7));
        assert_eq!(cmd.request, Request::PoolStatus);
        let Parsed::Run(cmd) = parse(&args("--control=tcp:127.0.0.1:9 daemon ping")).unwrap()
        else {
            panic!("a command")
        };
        assert_eq!(cmd.request, Request::Ping);
    }

    #[test]
    fn every_verb_parses() {
        assert_eq!(request("daemon stop"), Request::DaemonStop);
        assert_eq!(
            request("pool format --device /dev/sdb --media hdd --stripe 1M --segment 64M --flush-opaque --strict-flush --force"),
            Request::PoolFormat {
                device: "/dev/sdb".into(),
                size_bytes: None,
                media: Some(MediaKind::Hdd),
                stripe_bytes: Some(1 << 20),
                segment_bytes: Some(64 << 20),
                flush_opaque: Some(true),
                strict_flush: true,
                force: true,
            }
        );
        assert_eq!(
            request("pool format --device pool.img --size 1G"),
            Request::PoolFormat {
                device: "pool.img".into(),
                size_bytes: Some(1 << 30),
                media: None,
                stripe_bytes: None,
                segment_bytes: None,
                flush_opaque: None,
                strict_flush: false,
                force: false,
            }
        );
        assert_eq!(
            request("pool boot --device pool.img --heap-arena --arena-bytes 64M --cp-interval 2"),
            Request::PoolBoot {
                device: "pool.img".into(),
                media: None,
                stripe_bytes: None,
                segment_bytes: None,
                flush_opaque: None,
                strict_flush: false,
                arena_bytes: 64 << 20,
                arena: ArenaKind::Heap,
                cp_interval_secs: 2,
            }
        );
        let Request::PoolBoot {
            arena,
            arena_bytes,
            cp_interval_secs,
            ..
        } = request("pool boot --device /dev/sdb --arena-file /mnt/huge/a")
        else {
            panic!("a boot")
        };
        assert_eq!(
            arena,
            ArenaKind::Hugetlb {
                path: "/mnt/huge/a".into()
            }
        );
        assert_eq!(arena_bytes, DEFAULT_ARENA_BYTES);
        assert_eq!(cp_interval_secs, DEFAULT_CP_INTERVAL_SECS);
        assert_eq!(request("pool sync"), Request::PoolSync);
        assert_eq!(request("pool clean"), Request::PoolClean);
        // `pool verify` waits for its report unless told not to.
        let Parsed::Run(cmd) = parse(&args("pool verify")).unwrap() else {
            panic!("pool verify runs");
        };
        assert_eq!(cmd.request, Request::PoolVerify {});
        assert!(cmd.wait_verify);
        let Parsed::Run(cmd) = parse(&args("pool verify --no-wait")).unwrap() else {
            panic!("pool verify --no-wait runs");
        };
        assert_eq!(cmd.request, Request::PoolVerify {});
        assert!(!cmd.wait_verify);
        assert_eq!(
            request("pool verify --status"),
            Request::PoolVerifyStatus {}
        );
        assert_eq!(
            request("pool verify --cancel"),
            Request::PoolVerifyCancel {}
        );
        assert_eq!(request("pool stop"), Request::PoolStop);
        assert_eq!(
            request("volume create --name data --size 10G"),
            Request::VolumeCreate {
                name: "data".into(),
                size_bytes: 10 << 30,
            }
        );
        assert_eq!(request("volume list"), Request::VolumeList);
        assert_eq!(
            request("volume show --name data"),
            Request::VolumeShow {
                name: "data".into()
            }
        );
        assert_eq!(
            request("volume delete --name data"),
            Request::VolumeDelete {
                name: "data".into()
            }
        );
        assert_eq!(
            request("volume online --name data"),
            Request::VolumeOnline {
                name: "data".into(),
                online: true,
            }
        );
        assert_eq!(
            request("volume offline --name data"),
            Request::VolumeOnline {
                name: "data".into(),
                online: false,
            }
        );
        assert_eq!(
            request("volume resize --name data --size 20G"),
            Request::VolumeResize {
                name: "data".into(),
                size_bytes: 20 << 30,
            }
        );
        assert_eq!(
            request("volume rename --name data --new-name db"),
            Request::VolumeRename {
                name: "data".into(),
                new_name: "db".into(),
            }
        );
        assert_eq!(
            request("volume nvfail-clear --name data"),
            Request::VolumeNvfailClear {
                name: "data".into()
            }
        );
        assert_eq!(
            request("snapshot create --volume data --name s1"),
            Request::SnapshotCreate {
                volume: "data".into(),
                name: "s1".into(),
            }
        );
        assert_eq!(
            request("snapshot list"),
            Request::SnapshotList { volume: None }
        );
        assert_eq!(
            request("snapshot list --volume data"),
            Request::SnapshotList {
                volume: Some("data".into())
            }
        );
        assert_eq!(
            request("snapshot delete --volume data --name s1"),
            Request::SnapshotDelete {
                volume: "data".into(),
                name: "s1".into(),
            }
        );
        assert_eq!(
            request("snapshot rollback --volume data --name s1 --delete-newer"),
            Request::SnapshotRollback {
                volume: "data".into(),
                name: "s1".into(),
                delete_newer: true,
            }
        );
        assert_eq!(
            request("snapshot pin --volume data --name s1"),
            Request::SnapshotPin {
                volume: "data".into(),
                name: "s1".into(),
                flags: PIN_USER,
            }
        );
        assert_eq!(
            request("snapshot pin --volume data --name s1 --schedule --clone"),
            Request::SnapshotPin {
                volume: "data".into(),
                name: "s1".into(),
                flags: PIN_SCHEDULE | PIN_CLONE,
            }
        );
        assert_eq!(
            request("snapshot unpin --volume data --name s1"),
            Request::SnapshotPin {
                volume: "data".into(),
                name: "s1".into(),
                flags: 0,
            }
        );
        assert_eq!(
            request("snapshot replication-base --volume data --name s1"),
            Request::SnapshotReplicationBase {
                volume: "data".into(),
                name: Some("s1".into()),
            }
        );
        assert_eq!(
            request("snapshot replication-base --volume data --release"),
            Request::SnapshotReplicationBase {
                volume: "data".into(),
                name: None,
            }
        );
        assert_eq!(
            request("io write --volume data --lba 8 --fill 0xab --blocks 2"),
            Request::IoWrite {
                volume: "data".into(),
                lba: 8,
                data_hex: "ab".repeat(2 * BLOCK_SIZE),
            }
        );
        assert_eq!(
            request(&format!(
                "io write --volume data --lba 0 --hex {}",
                "00".repeat(BLOCK_SIZE)
            )),
            Request::IoWrite {
                volume: "data".into(),
                lba: 0,
                data_hex: "00".repeat(BLOCK_SIZE),
            }
        );
        let Parsed::Run(cmd) = parse(&args(
            "io read --volume data --lba 3 --blocks 4 --out /tmp/x",
        ))
        .unwrap() else {
            panic!("a command")
        };
        assert_eq!(
            cmd.request,
            Request::IoRead {
                volume: "data".into(),
                lba: 3,
                blocks: 4,
            }
        );
        assert_eq!(cmd.read_to, Some(PathBuf::from("/tmp/x")));
        assert_eq!(
            request("io read --volume data --lba 3"),
            Request::IoRead {
                volume: "data".into(),
                lba: 3,
                blocks: 1,
            }
        );
        assert_eq!(
            request("io punch --volume data --lba 3 --blocks 1000"),
            Request::IoPunch {
                volume: "data".into(),
                lba: 3,
                blocks: 1000,
            }
        );
        assert_eq!(request("device list"), Request::DeviceList {});
        assert_eq!(
            request("device reap 3"),
            Request::DeviceReap {
                dev_id: Some(3),
                stale: false,
            }
        );
        assert_eq!(
            request("device reap /dev/ublkb12"),
            Request::DeviceReap {
                dev_id: Some(12),
                stale: false,
            }
        );
        assert_eq!(
            request("device reap --stale"),
            Request::DeviceReap {
                dev_id: None,
                stale: true,
            }
        );
    }

    #[test]
    fn mistakes_are_named() {
        let err = |line: &str| parse(&args(line)).unwrap_err();
        assert!(err("").contains("no command"));
        assert!(err("pool").contains("needs a verb"));
        assert!(err("pool status extra").contains("too many"));
        assert!(err("pool fly").contains("no verb"));
        assert!(err("galaxy status").contains("unknown command group"));
        assert!(err("volume create --name data").contains("--size"));
        assert!(err("volume create --size 1G").contains("--name"));
        assert!(err("pool status --name x").contains("does not take --name"));
        assert!(err("pool boot --device d --heap-arena --arena-file f").contains("exclude"));
        assert!(err("snapshot replication-base --volume v").contains("--release"));
        assert!(
            err("snapshot replication-base --volume v --name s --release").contains("--release")
        );
        assert!(err("io write --volume v --lba 0").contains("exactly one"));
        assert!(err("io write --volume v --lba 0 --hex abc").contains("whole 4 KiB"));
        assert!(err("io write --volume v --lba 0 --fill 0xab --blocks 257").contains("1 to 256"));
        assert!(err("io write --volume v --lba 0 --fill 300 --blocks 1").contains("not a byte"));
        assert!(err("io read --volume v --lba 0 --blocks 0").contains("1 to 256"));
        assert!(err("volume create --name d --size tiny").contains("--size"));
        assert!(err("--timeout 0 pool status").contains("--timeout"));
        assert!(err("--control nowhere pool status").contains("neither"));
        assert!(err("-x pool status").contains("unknown option"));
        assert!(err("device reap").contains("needs a device id or --stale"));
        assert!(err("device reap 3 --stale").contains("not both"));
        assert!(err("device reap sda").contains("not a ublk device"));
        assert!(err("device reap 3 4").contains("too many"));
        assert!(err("device list 3").contains("too many"));
        assert!(err("device fly").contains("no verb"));
        assert!(err("pool verify --status --cancel").contains("one of"));
        assert!(err("pool verify --no-wait --status").contains("one of"));
        assert!(err("pool verify --name x").contains("does not take --name"));
    }
}
