//! Guest-to-host messages of smtc-mpris-bridge. `docs/protocol.md` is the
//! contract; this crate is its single implementation for both sides.

use serde::{Deserialize, Serialize};
use std::time::Duration;

pub const VERSION: u32 = 1;
pub const MAX_LINE_BYTES: usize = 64 * 1024;
/// Port the host listens on unless configured otherwise.
pub const DEFAULT_PORT: u16 = 47800;

/// Guest heartbeat while a player is playing.
pub const HEARTBEAT_PLAYING: Duration = Duration::from_secs(1);
/// Guest keepalive while a player is paused or stopped.
pub const HEARTBEAT_IDLE: Duration = Duration::from_secs(5);
/// Host drops a connection's players after this long without a message. A
/// connection without players is left open: a guest with no sessions has
/// nothing to send.
pub const HOST_TIMEOUT: Duration = Duration::from_secs(15);
/// TCP keepalive on both ends, so a peer that vanished while nothing is being
/// sent is noticed within about a minute.
pub const KEEPALIVE_IDLE: Duration = Duration::from_secs(30);
pub const KEEPALIVE_INTERVAL: Duration = Duration::from_secs(10);
pub const KEEPALIVE_RETRIES: u32 = 3;
/// A heartbeat this far from the extrapolated position counts as a seek.
pub const SEEK_THRESHOLD_MS: f64 = 1_000.0;

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Message {
    Hello { v: u32, guest: String },
    State(State),
    Gone { player: String },
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
pub struct State {
    pub player: String,
    pub status: Status,
    pub track: Option<Track>,
    /// Position at the moment the guest writes the line.
    pub position_ms: f64,
    pub position_uncertainty_ms: Option<f64>,
    pub rate: f64,
    #[serde(default)]
    pub seek: bool,
}

#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Status {
    Playing,
    Paused,
    Stopped,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
pub struct Track {
    pub title: String,
    pub artists: Vec<String>,
    pub album: Option<String>,
    pub length_ms: Option<f64>,
}

/// One line of the stream, newline included.
pub fn encode(message: &Message) -> String {
    let mut line = serde_json::to_string(message).expect("messages always serialize");
    line.push('\n');
    line
}

pub fn decode(line: &str) -> serde_json::Result<Message> {
    serde_json::from_str(line.trim_end_matches(['\r', '\n']))
}

#[cfg(test)]
mod tests {
    use super::*;

    const APP: &str = "AppleInc.AppleMusicWin_nzyj5cx40ttqa!App";

    // The examples in docs/protocol.md, verbatim.
    const DOC_HELLO: &str = r#"{"type":"hello","v":1,"guest":"win11"}"#;
    const DOC_STATE: &str = r#"{"type":"state","player":"AppleInc.AppleMusicWin_nzyj5cx40ttqa!App","status":"playing","track":{"title":"S.O.S","artists":["Indila"],"album":"Mini World","length_ms":272000.0},"position_ms":61211.7,"position_uncertainty_ms":6.6,"rate":1.0,"seek":false}"#;
    const DOC_GONE: &str = r#"{"type":"gone","player":"AppleInc.AppleMusicWin_nzyj5cx40ttqa!App"}"#;

    fn doc_state() -> Message {
        Message::State(State {
            player: APP.into(),
            status: Status::Playing,
            track: Some(Track {
                title: "S.O.S".into(),
                artists: vec!["Indila".into()],
                album: Some("Mini World".into()),
                length_ms: Some(272_000.0),
            }),
            position_ms: 61_211.7,
            position_uncertainty_ms: Some(6.6),
            rate: 1.0,
            seek: false,
        })
    }

    #[test]
    fn documented_examples_encode_exactly() {
        let hello = Message::Hello {
            v: VERSION,
            guest: "win11".into(),
        };
        assert_eq!(encode(&hello), format!("{DOC_HELLO}\n"));
        assert_eq!(encode(&doc_state()), format!("{DOC_STATE}\n"));
        assert_eq!(
            encode(&Message::Gone { player: APP.into() }),
            format!("{DOC_GONE}\n")
        );
    }

    #[test]
    fn documented_examples_decode() {
        assert_eq!(decode(DOC_STATE).unwrap(), doc_state());
        assert!(matches!(
            decode(DOC_HELLO).unwrap(),
            Message::Hello { v: 1, .. }
        ));
        assert!(matches!(decode(DOC_GONE).unwrap(), Message::Gone { .. }));
    }

    #[test]
    fn decode_accepts_crlf_and_newline() {
        assert!(decode(&format!("{DOC_GONE}\r\n")).is_ok());
    }

    #[test]
    fn unknown_fields_are_ignored() {
        let line = r#"{"type":"gone","player":"x","added_in_v1_1":true}"#;
        assert_eq!(decode(line).unwrap(), Message::Gone { player: "x".into() });
    }

    #[test]
    fn seek_defaults_to_false() {
        let line = r#"{"type":"state","player":"x","status":"paused","track":null,"position_ms":0.0,"position_uncertainty_ms":null,"rate":1.0}"#;
        match decode(line).unwrap() {
            Message::State(s) => {
                assert!(!s.seek);
                assert_eq!(s.status, Status::Paused);
                assert_eq!(s.track, None);
            }
            other => panic!("expected a state, got {other:?}"),
        }
    }

    #[test]
    fn unknown_type_is_an_error() {
        assert!(decode(r#"{"type":"control","action":"pause"}"#).is_err());
    }

    #[test]
    fn documented_examples_fit_a_line() {
        assert!(DOC_STATE.len() < MAX_LINE_BYTES);
    }
}
