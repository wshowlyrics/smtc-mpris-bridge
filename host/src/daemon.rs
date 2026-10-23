//! The daemon itself: listen, accept guests, reload on SIGHUP. Linux only,
//! like everything that touches the session bus or Unix signals.

use crate::{guest, Args};
use socket2::{Domain, Socket, TcpKeepalive, Type};
use std::net::SocketAddr;
use std::process::ExitCode;
use tokio::net::TcpListener;
use tokio::signal::unix::{signal, SignalKind};
use tokio::sync::watch;

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

pub async fn run(args: Args) -> ExitCode {
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
