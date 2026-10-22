mod guest;
mod mpris;
mod names;
mod playback;

use socket2::{Domain, Socket, Type};
use std::net::{IpAddr, SocketAddr};
use std::process::ExitCode;
use std::sync::Arc;
use tokio::net::TcpListener;
use tokio::signal::unix::{signal, SignalKind};

const USAGE: &str = "usage: smtc-mpris-bridge-host [--listen ADDR[:PORT]] [--ignore APP]...\n\
     ADDR defaults to 192.168.122.1 (libvirt's default network), PORT to 47800";

struct Args {
    listen: SocketAddr,
    /// SMTC app ids or app names (`SpotifyMusic`) not to publish.
    ignore: Vec<String>,
}

fn parse(mut args: impl Iterator<Item = String>) -> Result<Args, String> {
    let mut parsed = Args {
        listen: SocketAddr::from(([192, 168, 122, 1], protocol::DEFAULT_PORT)),
        ignore: Vec::new(),
    };
    while let Some(flag) = args.next() {
        let value = args.next().ok_or(USAGE)?;
        match flag.as_str() {
            "--listen" => {
                parsed.listen = value
                    .parse()
                    .or_else(|_| {
                        value
                            .parse::<IpAddr>()
                            .map(|ip| SocketAddr::new(ip, protocol::DEFAULT_PORT))
                    })
                    .map_err(|_| format!("--listen: not an address: {value}"))?
            }
            "--ignore" => parsed.ignore.push(value),
            _ => return Err(USAGE.into()),
        }
    }
    Ok(parsed)
}

/// Bind with `IP_FREEBIND`: started at login, the daemon can come up before
/// libvirt has created `virbr0` and its address.
fn listen(addr: SocketAddr) -> std::io::Result<TcpListener> {
    let socket = Socket::new(Domain::for_address(addr), Type::STREAM, None)?;
    socket.set_reuse_address(true)?;
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
    let listener = match listen(args.listen) {
        Ok(listener) => listener,
        Err(e) => {
            eprintln!("cannot listen on {}: {e}", args.listen);
            return ExitCode::FAILURE;
        }
    };
    let mut term = match signal(SignalKind::terminate()) {
        Ok(term) => term,
        Err(e) => {
            eprintln!("cannot watch SIGTERM: {e}");
            return ExitCode::FAILURE;
        }
    };
    eprintln!("listening on {}", args.listen);
    let ignore: Arc<[String]> = args.ignore.into();
    let guests = guest::Guests::default();
    let mut next_id = 0u64;
    loop {
        tokio::select! {
            accepted = listener.accept() => match accepted {
                Ok((stream, peer)) => {
                    let (ignore, guests) = (ignore.clone(), guests.clone());
                    next_id += 1;
                    let id = next_id;
                    tokio::spawn(async move { guest::serve(stream, peer, id, &ignore, &guests).await });
                }
                Err(e) => eprintln!("accept: {e}"),
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
    fn listen_takes_an_address_with_or_without_a_port() {
        assert_eq!(args(&[]).unwrap().listen.to_string(), "192.168.122.1:47800");
        assert_eq!(
            args(&["--listen", "10.0.0.1"]).unwrap().listen.to_string(),
            "10.0.0.1:47800"
        );
        assert_eq!(
            args(&["--listen", "127.0.0.1:5000"])
                .unwrap()
                .listen
                .to_string(),
            "127.0.0.1:5000"
        );
        assert!(args(&["--listen", "virbr0"]).is_err());
    }

    #[test]
    fn ignore_repeats() {
        let a = args(&["--ignore", "SpotifyMusic", "--ignore", "x!y"]).unwrap();
        assert_eq!(a.ignore, ["SpotifyMusic", "x!y"]);
    }
}
