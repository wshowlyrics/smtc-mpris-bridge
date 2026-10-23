mod config;
mod guest;
mod mpris;
mod names;
mod playback;

use config::Config;
use socket2::{Domain, Socket, TcpKeepalive, Type};
use std::net::SocketAddr;
use std::path::PathBuf;
use std::process::ExitCode;
use tokio::net::TcpListener;
use tokio::signal::unix::{signal, SignalKind};
use tokio::sync::watch;

const USAGE: &str = "usage: smtc-mpris-bridge-host [--config FILE] [--listen ADDR[:PORT]] [--allow APP]... [--ignore APP]...\n\
     FILE defaults to $XDG_CONFIG_HOME/smtc-mpris-bridge/host.ini; options override it";

/// Command-line overrides of `host.ini`.
#[derive(Default)]
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

/// Bind with `IP_FREEBIND`: started at login, the daemon can come up before
/// libvirt has created `virbr0` and its address. Accepted connections inherit
/// the listener's TCP keepalive.
fn listen(addr: SocketAddr) -> std::io::Result<TcpListener> {
    let socket = Socket::new(Domain::for_address(addr), Type::STREAM, None)?;
    socket.set_reuse_address(true)?;
    socket.set_tcp_keepalive(
        &TcpKeepalive::new()
            .with_time(protocol::KEEPALIVE_IDLE)
            .with_interval(protocol::KEEPALIVE_INTERVAL)
            .with_retries(protocol::KEEPALIVE_RETRIES),
    )?;
    match addr {
        SocketAddr::V4(_) => socket.set_freebind_v4(true)?,
        SocketAddr::V6(_) => socket.set_freebind_v6(true)?,
    }
    socket.bind(&addr.into())?;
    socket.listen(16)?;
    socket.set_nonblocking(true)?;
    TcpListener::from_std(socket.into())
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> ExitCode {
    let args = match parse(std::env::args().skip(1)) {
        Ok(args) => args,
        Err(message) => {
            eprintln!("{message}");
            return ExitCode::from(2);
        }
    };
    let config = match args.load() {
        Ok(config) => config,
        Err(e) => {
            eprintln!("{e}");
            return ExitCode::FAILURE;
        }
    };
    let listener = match listen(config.listen) {
        Ok(listener) => listener,
        Err(e) => {
            eprintln!("cannot listen on {}: {e}", config.listen);
            return ExitCode::FAILURE;
        }
    };
    let signals = (
        signal(SignalKind::terminate()),
        signal(SignalKind::hangup()),
    );
    let (mut term, mut hup) = match signals {
        (Ok(term), Ok(hup)) => (term, hup),
        (Err(e), _) | (_, Err(e)) => {
            eprintln!("cannot watch signals: {e}");
            return ExitCode::FAILURE;
        }
    };
    eprintln!("listening on {}, {}", config.listen, config.players);
    let (filter_tx, filter) = watch::channel(config.players);
    let guests = guest::Guests::default();
    let mut next_id = 0u64;
    loop {
        tokio::select! {
            accepted = listener.accept() => match accepted {
                Ok((stream, peer)) => {
                    next_id += 1;
                    let serve = guest::serve(stream, peer, next_id, filter.clone(), guests.clone());
                    tokio::spawn(serve);
                }
                Err(e) => eprintln!("accept: {e}"),
            },
            _ = hup.recv() => match args.load() {
                Ok(new) => {
                    if new.listen != config.listen {
                        eprintln!("reload: listen changes need a restart; still on {}", config.listen);
                    }
                    // A list given on the command line wins over the file again.
                    let pinned = if args.allow.is_empty() && args.ignore.is_empty() {
                        ""
                    } else {
                        " (command-line lists kept)"
                    };
                    eprintln!("reload: {}{pinned}", new.players);
                    filter_tx.send_replace(new.players);
                }
                Err(e) => eprintln!("reload: {e}; keeping the previous settings"),
            },
            _ = term.recv() => break,
            _ = tokio::signal::ctrl_c() => break,
        }
    }
    // Dropping the runtime drops every connection, and with it every player.
    ExitCode::SUCCESS
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
