//! Bridge mode: send each SMTC session's state to the host over TCP
//! (wshowlyrics/smtc-mpris-bridge#2; the contract is docs/protocol.md).

use crate::player::{self, Player};
use crate::smtc::{self, now_ms, Event, Kind, Session, Smtc, Status as SmtcStatus};
use crate::timeline::DEFAULT_SLACK_MS;
use crate::RunArgs;
use protocol::{
    encode, Message, Status, HEARTBEAT_IDLE, HEARTBEAT_PLAYING, KEEPALIVE_IDLE, KEEPALIVE_INTERVAL,
    KEEPALIVE_RETRIES, VERSION,
};
use socket2::{SockRef, TcpKeepalive};
use std::collections::BTreeMap;
use std::io::Write;
use std::net::{SocketAddr, TcpStream, ToSocketAddrs};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::time::{Duration, Instant};
use windows::core::Result;

const CONNECT_TIMEOUT: Duration = Duration::from_secs(2);
const WRITE_TIMEOUT: Duration = Duration::from_secs(2);
const BACKOFF_MIN: Duration = Duration::from_secs(1);
const BACKOFF_MAX: Duration = Duration::from_secs(30);
/// How often a settling seek is checked for having gone quiet.
const SETTLE_POLL: Duration = Duration::from_millis(100);

enum Msg {
    Smtc(Event),
    Quit,
}

/// `Changing` maps to `None`: it is transient and not sent.
fn status(s: SmtcStatus) -> Option<Status> {
    match s {
        SmtcStatus::Playing => Some(Status::Playing),
        SmtcStatus::Paused => Some(Status::Paused),
        SmtcStatus::Stopped | SmtcStatus::Closed | SmtcStatus::Opened => Some(Status::Stopped),
        _ => None,
    }
}

fn log(message: &str) {
    eprintln!("[{:.3}] {message}", now_ms() / 1000.0);
}

/// The connection to the host, reconnected with backoff.
struct Link {
    host: String,
    port: u16,
    stream: Option<TcpStream>,
    retry_at: Instant,
    backoff: Duration,
}

impl Link {
    fn resolve(&self) -> std::io::Result<SocketAddr> {
        (self.host.as_str(), self.port)
            .to_socket_addrs()?
            .next()
            .ok_or_else(|| std::io::Error::other("no address"))
    }

    /// Returns whether a new connection was just made.
    fn connect_if_due(&mut self) -> bool {
        if self.stream.is_some() || Instant::now() < self.retry_at {
            return false;
        }
        let connected = self.resolve().and_then(|addr| {
            let stream = TcpStream::connect_timeout(&addr, CONNECT_TIMEOUT)?;
            stream.set_nodelay(true)?;
            stream.set_write_timeout(Some(WRITE_TIMEOUT))?;
            let keepalive = TcpKeepalive::new()
                .with_time(KEEPALIVE_IDLE)
                .with_interval(KEEPALIVE_INTERVAL)
                .with_retries(KEEPALIVE_RETRIES);
            SockRef::from(&stream).set_tcp_keepalive(&keepalive)?;
            Ok((addr, stream))
        });
        match connected {
            Ok((addr, stream)) => {
                log(&format!("connected to {addr}"));
                self.stream = Some(stream);
                self.backoff = BACKOFF_MIN;
                true
            }
            Err(e) => {
                self.retry_later(&format!(
                    "cannot connect to {}:{}: {e}",
                    self.host, self.port
                ));
                false
            }
        }
    }

    fn retry_later(&mut self, why: &str) {
        log(&format!("{why}; retrying in {}s", self.backoff.as_secs()));
        self.stream = None;
        self.retry_at = Instant::now() + self.backoff;
        self.backoff = (self.backoff * 2).min(BACKOFF_MAX);
    }

    /// The host never writes, so anything readable means it closed the
    /// connection (or keepalive gave up on it). Without this a guest with no
    /// sessions, which writes nothing, would only notice at its next write.
    fn check(&mut self) {
        let Some(stream) = &self.stream else {
            return;
        };
        let peeked = stream.set_nonblocking(true).and_then(|()| {
            let peeked = stream.peek(&mut [0u8; 1]);
            stream.set_nonblocking(false)?;
            peeked
        });
        match peeked {
            Ok(0) => self.retry_later("the host closed the connection"),
            Err(e) if e.kind() != std::io::ErrorKind::WouldBlock => {
                self.retry_later(&format!("connection lost: {e}"))
            }
            _ => {}
        }
    }

    fn send(&mut self, message: &Message) {
        let Some(stream) = &mut self.stream else {
            return;
        };
        if let Err(e) = stream.write_all(encode(message).as_bytes()) {
            self.retry_later(&format!("connection lost: {e}"));
        }
    }
}

struct Entry {
    player: Player,
    next_heartbeat: Instant,
}

struct Bridge {
    smtc: Smtc<Msg>,
    link: Link,
    guest: String,
    players: BTreeMap<String, Entry>,
}

impl Bridge {
    /// Feed the session's current playback info and timeline to its player.
    fn read_timeline(entry: &mut Entry, s: &Session) -> Result<bool> {
        let raw = smtc::read(s)?;
        let now = now_ms();
        // Not short-circuited: both must see the new values.
        Ok(entry.player.observe_status(status(raw.status), raw.rate)
            | entry
                .player
                .observe_timeline(raw.position_ms, raw.last_updated_ms, raw.end_ms, now))
    }

    fn read_media(entry: &mut Entry, s: &Session) -> Result<bool> {
        let p = s.TryGetMediaPropertiesAsync()?.get()?;
        let track = player::track(
            &p.Title()?.to_string(),
            &p.Artist()?.to_string(),
            &p.AlbumTitle()?.to_string(),
        );
        Ok(entry.player.observe_track(track))
    }

    fn send_state(&mut self, app: &str) {
        let Some(entry) = self.players.get_mut(app) else {
            return;
        };
        let state = entry.player.state(app, now_ms());
        entry.next_heartbeat = Instant::now()
            + if entry.player.playing() {
                HEARTBEAT_PLAYING
            } else {
                HEARTBEAT_IDLE
            };
        if let Some(state) = state {
            self.link.send(&Message::State(state));
        }
    }

    /// Send a state if a read made one due.
    fn apply(&mut self, app: &str, due: Result<bool>) {
        match due {
            Ok(true) => self.send_state(app),
            Ok(false) => {}
            // The session can close between the event and the read.
            Err(e) => log(&format!("{app}: {e}")),
        }
    }

    fn on_session(&mut self, kind: Kind, app: &str) {
        let (Some(s), Some(entry)) = (self.smtc.find(app), self.players.get_mut(app)) else {
            return;
        };
        let due = match kind {
            Kind::Media => Self::read_media(entry, &s),
            Kind::Timeline | Kind::Playback => Self::read_timeline(entry, &s),
        };
        self.apply(app, due);
    }

    fn resync(&mut self) {
        let sessions = self.smtc.resubscribe();
        let gone: Vec<String> = self
            .players
            .keys()
            .filter(|app| !sessions.iter().any(|(a, ..)| a == *app))
            .cloned()
            .collect();
        for player in gone {
            log(&format!("{player}: session closed"));
            self.players.remove(&player);
            self.link.send(&Message::Gone { player });
        }
        for (app, session, subscribed) in sessions {
            if let Err(e) = subscribed {
                log(&format!("{app}: cannot subscribe: {e}"));
                continue;
            }
            let entry = self.players.entry(app.clone()).or_insert_with(|| {
                log(&format!("{app}: session opened"));
                Entry {
                    player: Player::new(DEFAULT_SLACK_MS),
                    next_heartbeat: Instant::now(),
                }
            });
            // Media first, so the first state already carries the track.
            let due = (|| {
                Ok(Self::read_media(entry, &session)? | Self::read_timeline(entry, &session)?)
            })();
            self.apply(&app, due);
        }
    }

    /// Notice a closed connection, reconnect, settle seeks and send heartbeats
    /// that are due. Runs at least every `HEARTBEAT_IDLE`, sessions or not.
    fn tick(&mut self) {
        self.link.check();
        if self.link.connect_if_due() {
            self.link.send(&Message::Hello {
                v: VERSION,
                guest: self.guest.clone(),
            });
            let apps: Vec<String> = self.players.keys().cloned().collect();
            for app in apps {
                self.send_state(&app);
            }
        }
        let now = now_ms();
        let due: Vec<String> = self
            .players
            .iter_mut()
            .filter_map(|(app, e)| {
                (e.player.poll(now) || Instant::now() >= e.next_heartbeat).then(|| app.clone())
            })
            .collect();
        for app in due {
            self.send_state(&app);
        }
    }

    fn next_wakeup(&self) -> Instant {
        let now = Instant::now();
        let mut at = self
            .players
            .values()
            .map(|e| e.next_heartbeat)
            .min()
            .unwrap_or(now + HEARTBEAT_IDLE);
        if self.link.stream.is_none() {
            at = at.min(self.link.retry_at);
        }
        if self.players.values().any(|e| e.player.settling()) {
            at = at.min(now + SETTLE_POLL);
        }
        at
    }
}

pub fn run(config: RunArgs) -> std::result::Result<(), Box<dyn std::error::Error>> {
    let (tx, rx) = mpsc::channel();
    let smtc = Smtc::new(tx.clone(), Msg::Smtc)?;
    let _ = ctrlc::set_handler(move || {
        let _ = tx.send(Msg::Quit);
    });
    log(&format!(
        "sending to {}:{} as guest {:?}",
        config.host, config.port, config.guest
    ));
    let mut bridge = Bridge {
        smtc,
        link: Link {
            host: config.host,
            port: config.port,
            stream: None,
            retry_at: Instant::now(),
            backoff: BACKOFF_MIN,
        },
        guest: config.guest,
        players: BTreeMap::new(),
    };
    bridge.resync();
    loop {
        bridge.tick();
        let wait = bridge
            .next_wakeup()
            .saturating_duration_since(Instant::now());
        match rx.recv_timeout(wait) {
            Ok(Msg::Smtc(Event::Session(kind, app))) => bridge.on_session(kind, &app),
            Ok(Msg::Smtc(Event::SessionsChanged)) => bridge.resync(),
            Ok(Msg::Quit) | Err(RecvTimeoutError::Disconnected) => break,
            Err(RecvTimeoutError::Timeout) => {}
        }
    }
    Ok(())
}
