// Only the Windows modes use these; elsewhere they are built for their tests.
#[cfg_attr(not(windows), allow(dead_code))]
mod player;
#[cfg(windows)]
mod probe;
#[cfg(windows)]
mod run;
#[cfg(windows)]
mod smtc;
#[cfg_attr(not(windows), allow(dead_code))]
mod stats;
#[cfg_attr(not(windows), allow(dead_code))]
mod timeline;

use std::path::PathBuf;
use std::process::ExitCode;
use std::time::Duration;

const USAGE: &str = "usage: smtc-mpris-bridge-guest run --host ADDR [--port PORT] [--guest NAME]
       smtc-mpris-bridge-guest probe [--out-dir DIR] [--interval SECONDS] [--slack-ms MS]";

#[cfg_attr(not(windows), allow(dead_code))]
struct RunArgs {
    host: String,
    port: u16,
    guest: String,
}

#[cfg_attr(not(windows), allow(dead_code))]
struct ProbeArgs {
    out_dir: PathBuf,
    interval: Duration,
    slack_ms: f64,
}

/// Flag/value pairs; `Err` carries the message to print.
fn pairs(mut args: impl Iterator<Item = String>) -> Result<Vec<(String, String)>, String> {
    let mut out = Vec::new();
    while let Some(flag) = args.next() {
        out.push((flag, args.next().ok_or(USAGE)?));
    }
    Ok(out)
}

fn parse_run(args: impl Iterator<Item = String>) -> Result<RunArgs, String> {
    let mut host = None;
    let mut port = protocol::DEFAULT_PORT;
    let mut guest = std::env::var("COMPUTERNAME").unwrap_or_else(|_| "guest".into());
    for (flag, v) in pairs(args)? {
        match flag.as_str() {
            "--host" => host = Some(v),
            "--port" => port = v.parse().map_err(|_| "--port must be a port number")?,
            "--guest" => guest = v,
            _ => return Err(USAGE.into()),
        }
    }
    Ok(RunArgs {
        host: host.ok_or("--host is required (e.g. 192.168.122.1)")?,
        port,
        guest,
    })
}

fn parse_probe(args: impl Iterator<Item = String>) -> Result<ProbeArgs, String> {
    let mut parsed = ProbeArgs {
        out_dir: PathBuf::from("logs"),
        interval: Duration::from_secs(1),
        slack_ms: timeline::DEFAULT_SLACK_MS,
    };
    for (flag, v) in pairs(args)? {
        match flag.as_str() {
            "--out-dir" => parsed.out_dir = PathBuf::from(v),
            "--interval" => match v.parse::<f64>() {
                Ok(secs) if secs > 0.0 => parsed.interval = Duration::from_secs_f64(secs),
                _ => return Err("--interval must be a positive number of seconds".into()),
            },
            "--slack-ms" => match v.parse::<f64>() {
                Ok(ms) if ms >= 0.0 => parsed.slack_ms = ms,
                _ => return Err("--slack-ms must be a non-negative number of milliseconds".into()),
            },
            _ => return Err(USAGE.into()),
        }
    }
    Ok(parsed)
}

#[cfg_attr(not(windows), allow(dead_code))]
enum Mode {
    Run(RunArgs),
    Probe(ProbeArgs),
}

fn main() -> ExitCode {
    let mut args = std::env::args().skip(1);
    let mode = match args.next().as_deref() {
        Some("run") => parse_run(args).map(Mode::Run),
        Some("probe") => parse_probe(args).map(Mode::Probe),
        _ => Err(USAGE.into()),
    };
    match mode {
        Ok(mode) => start(mode),
        Err(message) => {
            eprintln!("{message}");
            ExitCode::from(2)
        }
    }
}

#[cfg(windows)]
fn start(mode: Mode) -> ExitCode {
    let result = match mode {
        Mode::Run(a) => run::run(a),
        Mode::Probe(a) => probe::run(a.out_dir, a.interval, a.slack_ms),
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("error: {e}");
            ExitCode::FAILURE
        }
    }
}

#[cfg(not(windows))]
fn start(_: Mode) -> ExitCode {
    eprintln!("this reads Windows SMTC and only runs on Windows");
    ExitCode::FAILURE
}
