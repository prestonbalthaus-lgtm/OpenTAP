//! `otap-ctl`: `otapctl`, the command-line client of `dbladed` (Step 5
//! brief, "Opus-Daemon-CLI"; E38 in `docs/07-engineering-decisions.md`).
//!
//! The grammar is ONTAP's: `otapctl <group> <verb> --flag value`, with
//! `pool`, `daemon`, `volume`, `snapshot`, `io` and `device` (Step 6B: the
//! host's ublk devices and the reaper) as the groups ([`USAGE`]
//! lists every verb). [`args::parse`] turns a command line into one
//! [`otap_control::Request`]; [`run`] connects, checks the protocol, sends
//! it and renders the reply as text ([`render()`]) or JSON (`--json`).
//!
//! Exit status: 0 for a reply, 1 when the daemon refused (its message is
//! on stderr, or in the JSON), 2 for a command line that does not parse,
//! 3 when the daemon cannot be reached or the stream broke, 4 when a
//! `pool verify` that waited for its report (Step 6B) did not conclude
//! clean. `pool verify` follows the verification job by asking for its
//! status until it ends; the daemon never holds a request for it.

#![forbid(unsafe_code)]
#![deny(missing_docs)]
#![warn(rust_2018_idioms, unreachable_pub)]

pub mod args;
pub mod render;

use std::io::Write;
use std::time::Duration;

use otap_control::client::ClientError;
use otap_control::{Client, ErrorCode, Request, Response, VerifyState, VerifyVerdict};

/// How often `pool verify` asks for the verification's progress while it
/// waits for the report.
const VERIFY_POLL: Duration = Duration::from_millis(100);

/// Follows a verification the daemon started until it ends: asks for its
/// status every [`VERIFY_POLL`]; returns the final reply.
fn follow_verify(client: &mut Client, first: Response) -> Result<Response, ClientError> {
    let mut reply = first;
    loop {
        match &reply {
            Response::Verify(report) if report.state == VerifyState::Running => {}
            _ => return Ok(reply),
        }
        std::thread::sleep(VERIFY_POLL);
        reply = client.call(&Request::PoolVerifyStatus {})?;
    }
}

pub use args::{parse, Command, Parsed, USAGE};
pub use render::{render, render_json};

/// The crate version.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

/// An error code as the CLI prints it.
#[must_use]
pub fn code_word(code: ErrorCode) -> &'static str {
    match code {
        ErrorCode::NoPool => "no pool",
        ErrorCode::PoolBooted => "pool booted",
        ErrorCode::NotFound => "not found",
        ErrorCode::BadRequest => "bad request",
        ErrorCode::Engine => "refused",
        ErrorCode::Timeout => "timeout",
        ErrorCode::Io => "i/o error",
        ErrorCode::Unsupported => "unsupported",
        ErrorCode::Protocol => "protocol",
    }
}

/// Runs `otapctl` with `args` (the command line without the program
/// name), writing to `out` and `err`; returns the exit status.
pub fn run(args: &[String], out: &mut dyn Write, err: &mut dyn Write) -> i32 {
    let cmd = match parse(args) {
        Ok(Parsed::Run(cmd)) => cmd,
        Ok(Parsed::Help) => {
            let _ = out.write_all(USAGE.as_bytes());
            return 0;
        }
        Ok(Parsed::Version) => {
            let _ = writeln!(out, "otapctl {VERSION}");
            return 0;
        }
        Err(message) => {
            let _ = writeln!(
                err,
                "otapctl: {message}\nRun `otapctl --help` for the commands."
            );
            return 2;
        }
    };
    let mut client = match Client::connect(&cmd.control, Some(cmd.timeout)) {
        Ok(client) => client,
        Err(e) => {
            let _ = writeln!(err, "otapctl: {e} ({})", cmd.control);
            return 3;
        }
    };
    if !matches!(cmd.request, Request::Ping) {
        if let Err(e) = client.hello() {
            let _ = writeln!(err, "otapctl: {e}");
            return 3;
        }
    }
    let reply = match client.call(&cmd.request) {
        Ok(response) if cmd.wait_verify => follow_verify(&mut client, response),
        other => other,
    };
    match reply {
        Ok(response) => {
            if cmd.json {
                let _ = writeln!(out, "{}", render_json(&response));
            } else if let Err(e) = render(&response, &cmd, out) {
                let _ = writeln!(err, "otapctl: {e}");
                return 1;
            }
            match &response {
                Response::Verify(report)
                    if cmd.wait_verify && report.verdict != Some(VerifyVerdict::Clean) =>
                {
                    4
                }
                _ => 0,
            }
        }
        Err(ClientError::Daemon(e)) => {
            if cmd.json {
                let _ = writeln!(
                    out,
                    "{}",
                    serde_json::to_string_pretty(&serde_json::json!({ "error": e }))
                        .unwrap_or_default()
                );
            } else {
                let _ = writeln!(err, "otapctl: {}: {}", code_word(e.code), e.message);
            }
            1
        }
        Err(e) => {
            let _ = writeln!(err, "otapctl: {e}");
            3
        }
    }
}
