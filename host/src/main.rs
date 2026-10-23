// Publishing on the session bus and Unix signals are Linux only; the rest is
// built everywhere so `cargo test` passes in a Windows checkout too.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
mod config;
#[cfg(target_os = "linux")]
mod daemon;
#[cfg(target_os = "linux")]
mod guest;
#[cfg(target_os = "linux")]
mod mpris;
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
mod names;
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
mod playback;

use config::Config;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::process::ExitCode;

const USAGE: &str = "usage: smtc-mpris-bridge-host [--config FILE] [--listen ADDR[:PORT]] [--allow APP]... [--ignore APP]...\n\
     FILE defaults to $XDG_CONFIG_HOME/smtc-mpris-bridge/host.ini; options override it";

/// Command-line overrides of `host.ini`.
#[derive(Default)]
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
struct Args {
    config: Option<PathBuf>,
    listen: Option<SocketAddr>,
    allow: Vec<String>,
    ignore: Vec<String>,
}

fn parse(mut args: impl Iterator<Item = String>) -> Result<Args, String> {
    let mut parsed = Args::default();
    while let Some(flag) = args.next() {
        let value = args.next().ok_or(USAGE)?;
        match flag.as_str() {
            "--config" => parsed.config = Some(PathBuf::from(value)),
            "--listen" => {
                parsed.listen =
                    Some(config::parse_listen(&value).map_err(|e| format!("--listen: {e}"))?)
            }
            "--allow" => parsed.allow.push(value),
            "--ignore" => parsed.ignore.push(value),
            _ => return Err(USAGE.into()),
        }
    }
    Ok(parsed)
}

#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
impl Args {
    /// The file's settings with these options laid over them. A list given on
    /// the command line replaces the file's list rather than adding to it.
    fn apply(&self, mut config: Config) -> Config {
        if let Some(listen) = self.listen {
            config.listen = listen;
        }
        if !self.allow.is_empty() {
            config.players.allow = self.allow.clone();
        }
        if !self.ignore.is_empty() {
            config.players.ignore = self.ignore.clone();
        }
        config
    }

    fn load(&self) -> Result<Config, String> {
        let file = match self.config.clone().or_else(config::default_path) {
            Some(path) => config::load(&path)?,
            None => Config::default(),
        };
        Ok(self.apply(file))
    }
}

fn main() -> ExitCode {
    match parse(std::env::args().skip(1)) {
        Ok(args) => run(args),
        Err(message) => {
            eprintln!("{message}");
            ExitCode::from(2)
        }
    }
}

#[cfg(target_os = "linux")]
fn run(args: Args) -> ExitCode {
    match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime.block_on(daemon::run(args)),
        Err(e) => {
            eprintln!("cannot start the runtime: {e}");
            ExitCode::FAILURE
        }
    }
}

#[cfg(not(target_os = "linux"))]
fn run(_: Args) -> ExitCode {
    eprintln!(
        "the host daemon publishes MPRIS players on a Linux session bus and only runs on Linux"
    );
    ExitCode::FAILURE
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(list: &[&str]) -> Result<Args, String> {
        parse(list.iter().map(|s| s.to_string()))
    }

    #[test]
    fn options_override_the_file() {
        let file =
            config::parse("[host]\nlisten = 10.0.0.1\n[players]\nallow = A\nignore = B\n").unwrap();

        let none = args(&[])
            .unwrap()
            .apply(config::parse("[players]\nallow = A\n").unwrap());
        assert_eq!(none.players.allow, ["A"]);

        let a = args(&["--listen", "127.0.0.1:5000", "--allow", "X", "--allow", "Y"]).unwrap();
        let c = a.apply(file);
        assert_eq!(c.listen.to_string(), "127.0.0.1:5000");
        assert_eq!(c.players.allow, ["X", "Y"]);
        assert_eq!(c.players.ignore, ["B"]);
    }

    #[test]
    fn bad_options_are_errors() {
        assert!(args(&["--listen", "virbr0"]).is_err());
        assert!(args(&["--allow"]).is_err());
        assert!(args(&["--bogus", "x"]).is_err());
    }
}
