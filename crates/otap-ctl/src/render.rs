//! Replies as people read them.

use std::io::{self, Write};

use otap_control::{
    decode_hex, format_size, DeviceReport, DeviceState, PoolReport, Response, SnapshotReport,
    VolumeReport, PIN_CLONE, PIN_REPLICATION, PIN_SCHEDULE, PIN_USER,
};

use crate::args::Command;

/// A reply as pretty JSON.
#[must_use]
pub fn render_json(response: &Response) -> String {
    serde_json::to_string_pretty(response).unwrap_or_default()
}

/// Writes a reply as text. For `io read --out`, the data goes to that
/// file instead of the terminal.
pub fn render(response: &Response, cmd: &Command, out: &mut dyn Write) -> io::Result<()> {
    match response {
        Response::Ok => writeln!(out, "ok"),
        Response::Pong {
            protocol,
            version,
            pool_booted,
        } => writeln!(
            out,
            "dbladed {version}, protocol {protocol}, pool booted: {}",
            yes_no(*pool_booted)
        ),
        Response::Formatted {
            agg_id,
            stripe_bytes,
            segment_bytes,
            capacity_bytes,
            segments,
        } => {
            writeln!(out, "formatted pool {agg_id}")?;
            writeln!(
                out,
                "  stripe {}, segment {}, {segments} segments over {}",
                format_size(u64::from(*stripe_bytes)),
                format_size(*segment_bytes),
                format_size(*capacity_bytes)
            )
        }
        Response::Pool(report) => render_pool(report, out),
        Response::Volumes { volumes } => {
            if volumes.is_empty() {
                return writeln!(out, "no volumes");
            }
            let mut rows = vec![vec![
                "NAME".to_string(),
                "INDEX".to_string(),
                "SIZE".to_string(),
                "STATE".to_string(),
                "DEVICE".to_string(),
                "SNAPSHOTS".to_string(),
            ]];
            for v in volumes {
                rows.push(vec![
                    v.name.clone().unwrap_or_else(|| "-".to_string()),
                    v.index.to_string(),
                    format_size(v.size_bytes),
                    volume_state(v),
                    v.device.clone().unwrap_or_else(|| "-".to_string()),
                    v.snapshots.to_string(),
                ]);
            }
            write_table(&rows, out)
        }
        Response::Volume(v) => {
            writeln!(out, "volume {}", v.name.as_deref().unwrap_or("-"))?;
            writeln!(out, "  index:      {}", v.index)?;
            writeln!(out, "  size:       {}", format_size(v.size_bytes))?;
            writeln!(out, "  state:      {}", volume_state(v))?;
            writeln!(out, "  generation: {}", v.rollback_gen)?;
            writeln!(out, "  device:     {}", v.device.as_deref().unwrap_or("-"))?;
            writeln!(out, "  snapshots:  {}", v.snapshots)
        }
        Response::Snapshots { snapshots } => {
            if snapshots.is_empty() {
                return writeln!(out, "no snapshots");
            }
            let mut rows = vec![vec![
                "VOLUME".to_string(),
                "NAME".to_string(),
                "ID".to_string(),
                "CP".to_string(),
                "CREATED (UTC)".to_string(),
                "KIND".to_string(),
                "ORIGIN".to_string(),
                "PINS".to_string(),
                "STATE".to_string(),
            ]];
            for s in snapshots {
                rows.push(snapshot_row(s));
            }
            write_table(&rows, out)
        }
        Response::Snapshot(s) => {
            writeln!(out, "snapshot {}", s.name.as_deref().unwrap_or("-"))?;
            writeln!(out, "  volume:   {}", s.volume.as_deref().unwrap_or("-"))?;
            writeln!(out, "  id:       {}", s.id)?;
            writeln!(out, "  uuid:     {}", s.uuid)?;
            writeln!(out, "  cp:       {}", s.cp_seq)?;
            writeln!(out, "  created:  {} UTC", utc(s.created_wall_ns))?;
            writeln!(out, "  kind:     {}, origin {}", s.consistency, s.origin)?;
            writeln!(out, "  pins:     {}", pins(s.flags))?;
            writeln!(out, "  state:    {}", snapshot_state(s))
        }
        Response::Io(io_result) => {
            if io_result.data_hex.is_empty() {
                return writeln!(out, "{}, seq {}", io_result.status, io_result.seq);
            }
            let data = decode_hex(&io_result.data_hex).unwrap_or_default();
            match &cmd.read_to {
                Some(path) => {
                    std::fs::write(path, &data)?;
                    writeln!(
                        out,
                        "{}, {} written to {}",
                        io_result.status,
                        format_size(data.len() as u64),
                        path.display()
                    )
                }
                None => {
                    writeln!(
                        out,
                        "{}, {}",
                        io_result.status,
                        format_size(data.len() as u64)
                    )?;
                    for (i, chunk) in data.chunks(32).enumerate() {
                        let hex: Vec<String> = chunk.iter().map(|b| format!("{b:02x}")).collect();
                        writeln!(out, "{:08x}  {}", i * 32, hex.join(" "))?;
                    }
                    Ok(())
                }
            }
        }
        Response::Committed { cp_seq } => writeln!(out, "committed cp {cp_seq}"),
        Response::Devices { devices } => render_devices(devices, out),
        Response::Reaped { devices } => {
            if devices.is_empty() {
                return writeln!(out, "nothing to reap");
            }
            for d in devices {
                writeln!(
                    out,
                    "reaped {} ({}: {})",
                    d.path,
                    device_state(d.state),
                    d.reason
                )?;
            }
            Ok(())
        }
    }
}

/// `serving`, `recoverable`, `stale` or `foreign`.
const fn device_state(state: DeviceState) -> &'static str {
    match state {
        DeviceState::Serving => "serving",
        DeviceState::Recoverable => "recoverable",
        DeviceState::Stale => "stale",
        DeviceState::Foreign => "foreign",
    }
}

fn render_devices(devices: &[DeviceReport], out: &mut dyn Write) -> io::Result<()> {
    if devices.is_empty() {
        return writeln!(out, "no ublk devices");
    }
    let mut rows = vec![vec![
        "ID".to_string(),
        "PATH".to_string(),
        "STATE".to_string(),
        "VOLUME".to_string(),
        "KERNEL".to_string(),
        "PID".to_string(),
        "REASON".to_string(),
    ]];
    for d in devices {
        let volume = match (&d.volume, d.volume_index) {
            (Some(name), _) => name.clone(),
            (None, Some(index)) => format!("#{index}"),
            (None, None) => "-".to_string(),
        };
        let pid = if d.pid <= 0 {
            "-".to_string()
        } else if d.pid_alive {
            d.pid.to_string()
        } else {
            format!("{} (dead)", d.pid)
        };
        rows.push(vec![
            d.dev_id.to_string(),
            d.path.clone(),
            device_state(d.state).to_string(),
            volume,
            d.kernel_state.clone(),
            pid,
            d.reason.clone(),
        ]);
    }
    write_table(&rows, out)
}

fn render_pool(r: &PoolReport, out: &mut dyn Write) -> io::Result<()> {
    writeln!(out, "pool {}", r.agg_id)?;
    writeln!(out, "  device:       {}", r.device)?;
    writeln!(
        out,
        "  cp:           {} (last fold at {})",
        r.cp_seq, r.fold_cp
    )?;
    writeln!(out, "  serving:      {}", yes_no(r.serving))?;
    writeln!(
        out,
        "  segments:     {} free of {}, reserve {}, {:.1}% in use{}",
        r.free_segments,
        r.segments,
        r.reserve_segments,
        r.utilization * 100.0,
        if r.pool_full { ", POOL FULL" } else { "" }
    )?;
    writeln!(
        out,
        "  geometry:     stripe {}, segment {}",
        format_size(u64::from(r.stripe_bytes)),
        format_size(r.segment_bytes)
    )?;
    writeln!(
        out,
        "  admissible:   {} more before the reserve; {} unflushed",
        format_size(r.admissible_bytes),
        format_size(r.unflushed_bytes)
    )?;
    writeln!(
        out,
        "  arena:        {}, {}, {:.1}% occupied",
        r.arena,
        format_size(r.arena_bytes),
        r.arena_occupancy * 100.0
    )?;
    writeln!(out, "  cp interval:  {} s", r.cp_interval_secs)?;
    writeln!(
        out,
        "  flush policy: {} (flush-opaque: {}), discard {}",
        r.flush_policy,
        yes_no(r.flush_opaque),
        r.discard
    )?;
    writeln!(
        out,
        "  volumes:      {}, snapshots {}",
        r.volumes, r.snapshots
    )?;
    writeln!(
        out,
        "  cleaner:      {} segments cleaned, {} blocks relocated",
        r.cleaned_segments, r.relocated_blocks
    )?;
    writeln!(
        out,
        "  counters:     {} cps, {} writes, {} reads",
        r.cps, r.writes, r.reads
    )?;
    writeln!(out, "  last boot:    {}", r.last_boot)?;
    for w in &r.warnings {
        writeln!(out, "  WARNING:      {w}")?;
    }
    Ok(())
}

fn snapshot_row(s: &SnapshotReport) -> Vec<String> {
    vec![
        s.volume
            .clone()
            .unwrap_or_else(|| s.volume_index.to_string()),
        s.name.clone().unwrap_or_else(|| "-".to_string()),
        s.id.to_string(),
        s.cp_seq.to_string(),
        utc(s.created_wall_ns),
        s.consistency.clone(),
        s.origin.clone(),
        pins(s.flags),
        snapshot_state(s),
    ]
}

fn volume_state(v: &VolumeReport) -> String {
    if let Some(op) = &v.op_pending {
        return format!("pending:{op}");
    }
    if let Some(cause) = &v.nvfail {
        return format!("nvfail:{cause}");
    }
    if v.offline {
        return "offline".to_string();
    }
    if v.serving {
        "online".to_string()
    } else {
        "creating".to_string()
    }
}

fn snapshot_state(s: &SnapshotReport) -> String {
    if s.deleting {
        "deleting".to_string()
    } else if s.damaged {
        "damaged".to_string()
    } else {
        "ok".to_string()
    }
}

fn pins(flags: u8) -> String {
    let mut names = Vec::new();
    if flags & PIN_USER != 0 {
        names.push("user");
    }
    if flags & PIN_SCHEDULE != 0 {
        names.push("schedule");
    }
    if flags & PIN_REPLICATION != 0 {
        names.push("replication");
    }
    if flags & PIN_CLONE != 0 {
        names.push("clone");
    }
    if names.is_empty() {
        "-".to_string()
    } else {
        names.join(",")
    }
}

fn yes_no(b: bool) -> &'static str {
    if b {
        "yes"
    } else {
        "no"
    }
}

/// Writes rows as aligned columns; the first row is the header.
fn write_table(rows: &[Vec<String>], out: &mut dyn Write) -> io::Result<()> {
    let columns = rows.iter().map(Vec::len).max().unwrap_or(0);
    let widths: Vec<usize> = (0..columns)
        .map(|c| {
            rows.iter()
                .map(|r| r.get(c).map_or(0, |s| s.chars().count()))
                .max()
                .unwrap_or(0)
        })
        .collect();
    for row in rows {
        let mut line = String::new();
        for (c, cell) in row.iter().enumerate() {
            if c > 0 {
                line.push_str("  ");
            }
            line.push_str(cell);
            if c + 1 < row.len() {
                let pad = widths[c].saturating_sub(cell.chars().count());
                line.extend(std::iter::repeat_n(' ', pad));
            }
        }
        writeln!(out, "{line}")?;
    }
    Ok(())
}

/// `YYYY-MM-DD HH:MM:SS` of nanoseconds since the Unix epoch, in UTC.
#[must_use]
pub fn utc(ns: u64) -> String {
    let secs = ns / 1_000_000_000;
    let days = secs / 86_400;
    let rem = secs % 86_400;
    let (h, m, s) = (rem / 3600, (rem % 3600) / 60, rem % 60);
    // Civil-from-days (Howard Hinnant's algorithm), valid for every day of
    // the proleptic Gregorian calendar the u64 reaches.
    let z = i64::try_from(days).unwrap_or(i64::MAX / 4) + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let mo = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if mo <= 2 { y + 1 } else { y };
    format!("{y:04}-{mo:02}-{d:02} {h:02}:{m:02}:{s:02}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dates_render() {
        assert_eq!(utc(0), "1970-01-01 00:00:00");
        assert_eq!(utc(951_782_400 * 1_000_000_000), "2000-02-29 00:00:00");
        assert_eq!(utc(1_700_000_000 * 1_000_000_000), "2023-11-14 22:13:20");
        assert_eq!(utc(1_776_000_000 * 1_000_000_000), "2026-04-12 13:20:00");
    }

    #[test]
    fn pins_and_tables_render() {
        assert_eq!(pins(0), "-");
        assert_eq!(pins(PIN_USER | PIN_CLONE), "user,clone");
        assert_eq!(pins(PIN_REPLICATION), "replication");
        let mut out = Vec::new();
        write_table(
            &[
                vec!["A".into(), "BB".into()],
                vec!["ccc".into(), "d".into()],
            ],
            &mut out,
        )
        .unwrap();
        assert_eq!(String::from_utf8(out).unwrap(), "A    BB\nccc  d\n");
    }
}
