//! One guest connection: `hello`, then states republished as MPRIS players
//! until the connection closes or goes quiet (docs/protocol.md).

use crate::mpris::GuestPlayer;
use crate::names;
use protocol::{Message, HOST_TIMEOUT, MAX_LINE_BYTES, VERSION};
use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use tokio::io::{AsyncBufRead, AsyncBufReadExt, AsyncReadExt, BufReader};
use tokio::net::TcpStream;
use tokio::sync::Notify;
use tokio::time::timeout;

pub type Error = Box<dyn std::error::Error + Send + Sync>;

/// The live connection of each guest name. A guest that reconnects before its
/// old connection timed out ends the old one, so the stale task does not keep
/// players for up to `HOST_TIMEOUT`; two VMs using one name show up in the log.
#[derive(Clone, Default)]
pub struct Guests(Arc<Mutex<HashMap<String, Live>>>);

/// Connection id and the signal that ends that connection.
type Live = (u64, Arc<Notify>);

impl Guests {
    fn register(&self, name: &str, id: u64) -> Arc<Notify> {
        let stop = Arc::new(Notify::new());
        let mut live = self.0.lock().unwrap_or_else(|e| e.into_inner());
        if let Some((_, old)) = live.insert(name.to_string(), (id, stop.clone())) {
            eprintln!("{name}: replacing its earlier connection");
            // Stored as a permit if the old task is not waiting right now.
            old.notify_one();
        }
        stop
    }

    fn unregister(&self, name: &str, id: u64) {
        let mut live = self.0.lock().unwrap_or_else(|e| e.into_inner());
        if live.get(name).is_some_and(|(live_id, _)| *live_id == id) {
            live.remove(name);
        }
    }
}

/// The next message, or `None` once the guest closes the connection. Lines
/// that are not a message this side knows are logged and skipped.
async fn next<R: AsyncBufRead + Unpin>(
    reader: &mut R,
    buf: &mut Vec<u8>,
) -> Result<Option<Message>, Error> {
    loop {
        buf.clear();
        let limit = MAX_LINE_BYTES as u64 + 1; // the newline
        let mut line = (&mut *reader).take(limit);
        let n = timeout(HOST_TIMEOUT, line.read_until(b'\n', buf))
            .await
            .map_err(|_| format!("nothing for {} s", HOST_TIMEOUT.as_secs()))??;
        if n == 0 {
            return Ok(None);
        }
        if buf.last() != Some(&b'\n') {
            if buf.len() as u64 == limit {
                return Err(format!("line over {MAX_LINE_BYTES} bytes").into());
            }
            return Ok(None); // closed mid-line
        }
        match std::str::from_utf8(buf)
            .map_err(Error::from)
            .and_then(|line| protocol::decode(line).map_err(Error::from))
        {
            Ok(message) => return Ok(Some(message)),
            Err(e) => eprintln!("skipping a line: {e}"),
        }
    }
}

struct Guest<'a> {
    name: String,
    ignore: &'a [String],
    players: HashMap<String, GuestPlayer>,
}

impl Guest<'_> {
    fn ignored(&self, app: &str) -> bool {
        let name = names::app_name(app);
        self.ignore.iter().any(|i| i == app || i == name)
    }

    async fn on_message(&mut self, message: Message) {
        match message {
            Message::State(state) if !self.ignored(&state.player) => {
                let result = match self.players.get(&state.player) {
                    Some(player) => player.update(state).await,
                    None => {
                        let app = state.player.clone();
                        let bus = names::bus_name(&self.name, &app);
                        let identity = format!("{} ({})", names::app_name(&app), self.name);
                        GuestPlayer::publish(bus.clone(), identity, state)
                            .await
                            .map(|player| {
                                eprintln!("{}: published {bus}", self.name);
                                self.players.insert(app, player);
                            })
                    }
                };
                if let Err(e) = result {
                    eprintln!("{}: {e}", self.name);
                }
            }
            Message::Gone { player } => {
                if let Some(p) = self.players.remove(&player) {
                    eprintln!("{}: {player} gone", self.name);
                    p.close().await;
                }
            }
            Message::Hello { .. } => eprintln!("{}: ignoring a repeated hello", self.name),
            Message::State(_) => {}
        }
    }
}

async fn handle(
    stream: TcpStream,
    id: u64,
    ignore: &[String],
    guests: &Guests,
) -> Result<(), Error> {
    let mut reader = BufReader::new(stream);
    let mut buf = Vec::new();
    let name = match next(&mut reader, &mut buf).await? {
        Some(Message::Hello { v, guest }) if v == VERSION => guest,
        Some(Message::Hello { v, .. }) => {
            return Err(format!("guest speaks protocol {v}, this host {VERSION}").into())
        }
        Some(_) => return Err("the first message was not hello".into()),
        None => return Ok(()),
    };
    eprintln!("{name}: connected");
    let stop = guests.register(&name, id);
    let mut guest = Guest {
        name,
        ignore,
        players: HashMap::new(),
    };
    let result = async {
        loop {
            tokio::select! {
                message = next(&mut reader, &mut buf) => match message? {
                    Some(message) => guest.on_message(message).await,
                    None => return Ok(()),
                },
                _ = stop.notified() => return Err("replaced by a newer connection".into()),
            }
        }
    }
    .await;
    guests.unregister(&guest.name, id);
    eprintln!("{}: disconnected", guest.name);
    for (_, player) in guest.players.drain() {
        player.close().await;
    }
    result
}

pub async fn serve(
    stream: TcpStream,
    peer: SocketAddr,
    id: u64,
    ignore: &[String],
    guests: &Guests,
) {
    if let Err(e) = handle(stream, id, ignore, guests).await {
        eprintln!("{peer}: {e}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::AsyncWriteExt;

    async fn read_all(input: &[u8]) -> (Vec<Option<Message>>, Option<String>) {
        let (mut tx, rx) = tokio::io::duplex(MAX_LINE_BYTES * 2);
        tx.write_all(input).await.unwrap();
        drop(tx);
        let mut reader = BufReader::new(rx);
        let mut buf = Vec::new();
        let mut out = Vec::new();
        loop {
            match next(&mut reader, &mut buf).await {
                Ok(Some(m)) => out.push(Some(m)),
                Ok(None) => return (out, None),
                Err(e) => return (out, Some(e.to_string())),
            }
        }
    }

    #[tokio::test]
    async fn unknown_and_broken_lines_are_skipped() {
        let input = b"{\"type\":\"hello\",\"v\":1,\"guest\":\"g\"}\n\
            {\"type\":\"control\"}\n\
            not json\n\
            {\"type\":\"gone\",\"player\":\"p\"}\n";
        let (messages, error) = read_all(input).await;
        assert_eq!(messages.len(), 2);
        assert_eq!(error, None);
    }

    #[tokio::test]
    async fn an_overlong_line_ends_the_connection() {
        let mut input = vec![b'x'; MAX_LINE_BYTES + 10];
        input.push(b'\n');
        let (messages, error) = read_all(&input).await;
        assert!(messages.is_empty());
        assert!(error.unwrap().contains("line over"));
    }

    #[tokio::test]
    async fn a_line_cut_off_by_close_is_dropped() {
        let (messages, error) = read_all(b"{\"type\":\"gone\",\"pla").await;
        assert!(messages.is_empty());
        assert_eq!(error, None);
    }
}
