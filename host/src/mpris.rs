//! One guest player on the session bus. Each gets its own connection: every
//! MPRIS player lives at the same object path, so two cannot share one.

use crate::playback::Playback;
use protocol::{State, Status};
use std::collections::HashMap;
use std::time::Instant;
use zbus::object_server::{InterfaceRef, SignalEmitter};
use zbus::{connection, interface, Connection, ObjectPath, Variant};

const PATH: &str = "/org/mpris/MediaPlayer2";
const NO_TRACK: &str = "/org/mpris/MediaPlayer2/TrackList/NoTrack";

fn us(ms: f64) -> i64 {
    (ms * 1000.0) as i64
}

struct Root {
    identity: String,
}

#[interface(name = "org.mpris.MediaPlayer2")]
impl Root {
    fn raise(&self) {}

    fn quit(&self) {}

    #[zbus(property(emits_changed_signal = "const"))]
    fn can_quit(&self) -> bool {
        false
    }

    #[zbus(property(emits_changed_signal = "const"))]
    fn can_raise(&self) -> bool {
        false
    }

    #[zbus(property(emits_changed_signal = "const"))]
    fn has_track_list(&self) -> bool {
        false
    }

    #[zbus(property(emits_changed_signal = "const"))]
    fn identity(&self) -> String {
        self.identity.clone()
    }

    #[zbus(property(emits_changed_signal = "const"))]
    fn supported_uri_schemes(&self) -> Vec<String> {
        Vec::new()
    }

    #[zbus(property(emits_changed_signal = "const"))]
    fn supported_mime_types(&self) -> Vec<String> {
        Vec::new()
    }
}

struct Player {
    playback: Playback,
}

/// Control is out of scope (`CanControl` is false), so the methods do nothing.
#[interface(name = "org.mpris.MediaPlayer2.Player")]
impl Player {
    fn next(&self) {}

    fn previous(&self) {}

    fn pause(&self) {}

    fn play_pause(&self) {}

    fn stop(&self) {}

    fn play(&self) {}

    fn seek(&self, _offset: i64) {}

    fn set_position(&self, _track_id: ObjectPath<'_>, _position: i64) {}

    fn open_uri(&self, _uri: &str) {}

    #[zbus(signal)]
    async fn seeked(emitter: &SignalEmitter<'_>, position: i64) -> zbus::Result<()>;

    #[zbus(property)]
    fn playback_status(&self) -> String {
        match self.playback.status {
            Status::Playing => "Playing",
            Status::Paused => "Paused",
            Status::Stopped => "Stopped",
        }
        .into()
    }

    #[zbus(property)]
    fn rate(&self) -> f64 {
        self.playback.rate
    }

    #[zbus(property)]
    fn metadata(&self) -> HashMap<String, Variant<'static>> {
        let p = &self.playback;
        let mut m = HashMap::new();
        let id = match &p.track {
            Some(_) => format!("/org/smtc_mpris_bridge/track/{}", p.track_seq),
            None => NO_TRACK.into(),
        };
        m.insert(
            "mpris:trackid".into(),
            Variant::from(ObjectPath::try_from(id).expect("track ids are valid paths")),
        );
        if let Some(t) = &p.track {
            m.insert("xesam:title".into(), Variant::from(t.title.clone()));
            m.insert("xesam:artist".into(), Variant::from(t.artists.clone()));
            if let Some(album) = &t.album {
                m.insert("xesam:album".into(), Variant::from(album.clone()));
            }
            if let Some(length) = t.length_ms {
                m.insert("mpris:length".into(), Variant::from(us(length)));
            }
        }
        m
    }

    #[zbus(property(emits_changed_signal = "const"))]
    fn volume(&self) -> f64 {
        1.0
    }

    /// Computed when read; MPRIS never signals Position changes.
    #[zbus(property(emits_changed_signal = "false"))]
    fn position(&self) -> i64 {
        us(self.playback.position_ms(Instant::now()))
    }

    #[zbus(property(emits_changed_signal = "const"))]
    fn minimum_rate(&self) -> f64 {
        1.0
    }

    #[zbus(property(emits_changed_signal = "const"))]
    fn maximum_rate(&self) -> f64 {
        1.0
    }

    #[zbus(property(emits_changed_signal = "const"))]
    fn can_go_next(&self) -> bool {
        false
    }

    #[zbus(property(emits_changed_signal = "const"))]
    fn can_go_previous(&self) -> bool {
        false
    }

    #[zbus(property(emits_changed_signal = "const"))]
    fn can_play(&self) -> bool {
        // Against the spec's advice for CanControl = false, but playerctl skips
        // players that cannot play for `metadata` and the like. Play still
        // does nothing.
        true
    }

    #[zbus(property(emits_changed_signal = "const"))]
    fn can_pause(&self) -> bool {
        false
    }

    #[zbus(property(emits_changed_signal = "const"))]
    fn can_seek(&self) -> bool {
        false
    }

    #[zbus(property(emits_changed_signal = "const"))]
    fn can_control(&self) -> bool {
        false
    }
}

pub struct GuestPlayer {
    conn: Connection,
    name: String,
    player: InterfaceRef<Player>,
}

impl GuestPlayer {
    /// Take `name` on the session bus. Names are requested replaceable, so a
    /// reconnecting guest takes them over before its stale connection lets go.
    pub async fn publish(name: String, identity: String, state: State) -> zbus::Result<Self> {
        let conn = connection::Builder::session()
            .name(name.as_str())
            .allow_name_replacements(true)
            .replace_existing_names(true)
            .serve_at(PATH, Root { identity })
            .serve_at(
                PATH,
                Player {
                    playback: Playback::new(state, Instant::now()),
                },
            )
            .build()
            .await?;
        let player = conn.object_server().interface::<_, Player>(PATH).await?;
        Ok(Self { conn, name, player })
    }

    pub async fn update(&self, state: State) -> zbus::Result<()> {
        let now = Instant::now();
        let mut player = self.player.get_mut().await;
        let changes = player.playback.apply(state, now);
        let emitter = self.player.signal_emitter();
        if changes.status {
            player.playback_status_changed(emitter).await?;
        }
        if changes.metadata {
            player.metadata_changed(emitter).await?;
        }
        if changes.rate {
            player.rate_changed(emitter).await?;
        }
        if changes.seeked {
            Player::seeked(emitter, us(player.playback.position_ms(now))).await?;
        }
        Ok(())
    }

    /// Give the name up now rather than when the connection is dropped. It may
    /// already belong to a newer connection of the same guest, which is fine.
    pub async fn close(self) {
        let _ = self.conn.release_name(self.name.as_str()).await;
    }
}
