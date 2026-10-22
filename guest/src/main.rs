#[cfg(windows)]
mod probe;
#[cfg(windows)]
mod smtc;
// Only the Windows probe uses these; elsewhere they are built for their tests.
#[cfg_attr(not(windows), allow(dead_code))]
mod stats;
#[cfg_attr(not(windows), allow(dead_code))]
mod timeline;

use std::path::PathBuf;
use std::process::ExitCode;
use std::time::Duration;

const USAGE: &str =
    "usage: smtc-mpris-bridge-guest probe [--out-dir DIR] [--interval SECONDS] [--slack-ms MS]";

fn main() -> ExitCode {
    let mut args = std::env::args().skip(1);
    if args.next().as_deref() != Some("probe") {
        eprintln!("{USAGE}");
        return ExitCode::from(2);
    }
    let mut out_dir = PathBuf::from("logs");
    let mut interval = Duration::from_secs(1);
    let mut slack_ms = timeline::DEFAULT_SLACK_MS;
    while let Some(flag) = args.next() {
        let value = args.next();
        match (flag.as_str(), value) {
            ("--out-dir", Some(v)) => out_dir = PathBuf::from(v),
            ("--interval", Some(v)) => match v.parse::<f64>() {
                Ok(secs) if secs > 0.0 => interval = Duration::from_secs_f64(secs),
                _ => {
                    eprintln!("--interval must be a positive number of seconds");
                    return ExitCode::from(2);
                }
            },
            ("--slack-ms", Some(v)) => match v.parse::<f64>() {
                Ok(ms) if ms >= 0.0 => slack_ms = ms,
                _ => {
                    eprintln!("--slack-ms must be a non-negative number of milliseconds");
                    return ExitCode::from(2);
                }
            },
            _ => {
                eprintln!("{USAGE}");
                return ExitCode::from(2);
            }
        }
    }
    run(out_dir, interval, slack_ms)
}

#[cfg(windows)]
fn run(out_dir: PathBuf, interval: Duration, slack_ms: f64) -> ExitCode {
    match probe::run(out_dir, interval, slack_ms) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("error: {e}");
            ExitCode::FAILURE
        }
    }
}

#[cfg(not(windows))]
fn run(_out_dir: PathBuf, _interval: Duration, _slack_ms: f64) -> ExitCode {
    eprintln!("probe reads Windows SMTC and only runs on Windows");
    ExitCode::FAILURE
}
