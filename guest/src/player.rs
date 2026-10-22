//! One SMTC session as the host will see it: what goes into its `state` and
//! when one is due (docs/protocol.md "Guest rules"), kept off WinRT so it is
//! testable anywhere.

use crate::timeline::{corrected_position_ms, round1, Observed, Timeline, Tracker, QUANTUM_MS};
use protocol::{State, Status, Track, SEEK_THRESHOLD_MS};

/// Longest title, artist or album sent. Even if every byte needed a six-byte
/// JSON escape, a `state` stays under `protocol::MAX_LINE_BYTES`.
pub const MAX_FIELD_BYTES: usize = 2048;
/// A seek counts as settled after this long without another timeline update,
const SETTLE_QUIET_MS: f64 = 500.0;
/// or after this many consecutive updates agree with the restarted estimate.
const SETTLE_AGREEING: u32 = 2;

fn clip(s: &str) -> String {
    let mut end = s.len().min(MAX_FIELD_BYTES);
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    s[..end].to_string()
}

/// The track from SMTC media properties; `length_ms` is filled in from the
/// timeline when the state is built (docs/protocol.md "Metadata").
pub fn track(title: &str, artist: &str, album: &str) -> Option<Track> {
    // Apple Music: `Artist — Album` in Artist, AlbumTitle empty.
    let (artist, album) = match artist.split_once(" — ") {
        Some(parts) if album.is_empty() => parts,
        _ => (artist, album),
    };
    // No artist is Apple Music loading the next track: the title is then a
    // localized placeholder ("연결 중…"), not something to search lyrics for.
    if title.is_empty() || artist.is_empty() {
        return None;
    }
    Some(Track {
        title: clip(title),
        artists: vec![clip(artist)],
        album: (!album.is_empty()).then(|| clip(album)),
        length_ms: None,
    })
}

#[derive(Debug)]
struct Settling {
    agreeing: u32,
    last_update_ms: f64,
}

#[derive(Debug)]
pub struct Player {
    tracker: Tracker,
    /// Last stable status; `None` until the app reports one.
    status: Option<Status>,
    rate: f64,
    track: Option<Track>,
    length_ms: Option<f64>,
    /// Latest raw timeline; `last_updated_ms` is `None` before the first play.
    position_ms: f64,
    last_updated_ms: Option<f64>,
    /// The raw timeline was stamped while playing, so extrapolating it holds.
    /// A stamp from before a resume would add the whole pause.
    stamped_playing: bool,
    settling: Option<Settling>,
    /// The next state reports a settled seek.
    seek: bool,
}

impl Player {
    pub fn new(slack_ms: f64) -> Self {
        Self {
            tracker: Tracker::new(QUANTUM_MS, slack_ms),
            status: None,
            rate: 1.0,
            track: None,
            length_ms: None,
            position_ms: 0.0,
            last_updated_ms: None,
            stamped_playing: false,
            settling: None,
            seek: false,
        }
    }

    pub fn playing(&self) -> bool {
        self.status == Some(Status::Playing)
    }

    pub fn settling(&self) -> bool {
        self.settling.is_some()
    }

    /// `status` is `None` while SMTC reports `Changing`, which is not sent.
    /// Returns whether a state is due now; so do the other `observe_*`.
    pub fn observe_status(&mut self, status: Option<Status>, rate: f64) -> bool {
        self.rate = rate;
        let Some(status) = status.filter(|&s| self.status != Some(s)) else {
            return false;
        };
        self.status = Some(status);
        if status != Status::Playing {
            self.tracker.stopped();
            self.settling = None;
        }
        true
    }

    pub fn observe_track(&mut self, track: Option<Track>) -> bool {
        if track == self.track {
            return false;
        }
        self.track = track;
        // The raw timeline may still be the old track's.
        self.position_ms = 0.0;
        self.length_ms = None;
        self.stamped_playing = false;
        // A jump to the new track's start is not a seek.
        self.tracker.stopped();
        self.settling = None;
        self.seek = false;
        true
    }

    pub fn observe_timeline(
        &mut self,
        position_ms: f64,
        last_updated_ms: Option<f64>,
        length_ms: f64,
        now_ms: f64,
    ) -> bool {
        let length_ms = (length_ms > 0.0).then_some(length_ms);
        let mut due = self.track.is_some() && length_ms != self.length_ms;
        self.length_ms = length_ms;
        self.position_ms = position_ms;
        // Each event rereads the timeline; only a new stamp is a new update.
        let fresh = last_updated_ms.filter(|&lu| Some(lu) != self.last_updated_ms);
        self.last_updated_ms = last_updated_ms;
        if let Some(lu) = fresh {
            self.stamped_playing = self.playing();
            let observed = self
                .tracker
                .observe(Timeline {
                    position_ms,
                    last_updated_ms: lu,
                    rate: self.rate,
                    playing: self.playing(),
                    title: self.track.as_ref().map(|t| t.title.clone()),
                })
                .observed;
            // Right after a seek or a track start Apple Music's stamps conflict
            // by more than the slack yet move the estimate less than a second:
            // the estimator restarts, but the host would see no jump.
            let seek = matches!(observed,
                Some(Observed::Seek { jump_ms }) if jump_ms.abs() >= SEEK_THRESHOLD_MS);
            match &mut self.settling {
                Some(s) => {
                    s.last_update_ms = now_ms;
                    if seek {
                        s.agreeing = 0;
                    } else if matches!(observed, Some(Observed::Narrowed | Observed::Jitter { .. }))
                    {
                        s.agreeing += 1;
                        if s.agreeing >= SETTLE_AGREEING {
                            self.settle();
                            due = true;
                        }
                    }
                }
                None if seek => {
                    self.settling = Some(Settling {
                        agreeing: 0,
                        last_update_ms: now_ms,
                    })
                }
                None => {}
            }
        }
        due && self.settling.is_none()
    }

    /// A settling seek that has been quiet long enough is due now.
    pub fn poll(&mut self, now_ms: f64) -> bool {
        let quiet = self
            .settling
            .as_ref()
            .is_some_and(|s| now_ms - s.last_update_ms >= SETTLE_QUIET_MS);
        if quiet {
            self.settle();
        }
        quiet
    }

    fn settle(&mut self) {
        self.settling = None;
        self.seek = true;
    }

    /// The state to send at `now_ms`, or `None` while there is nothing stable
    /// to say: no status yet, or a seek still settling.
    pub fn state(&mut self, player: &str, now_ms: f64) -> Option<State> {
        let status = self.status?;
        if self.settling.is_some() {
            return None;
        }
        let playing = self.playing();
        let (position_ms, uncertainty) = match self.tracker.position_at(now_ms).filter(|_| playing)
        {
            Some((pos, half)) => (pos, Some(round1(half))),
            None => (
                self.last_updated_ms.map_or(self.position_ms, |lu| {
                    corrected_position_ms(
                        self.position_ms,
                        lu,
                        now_ms,
                        self.rate,
                        playing && self.stamped_playing,
                    )
                }),
                None,
            ),
        };
        Some(State {
            player: player.to_string(),
            status,
            track: self.track.clone().map(|t| Track {
                length_ms: self.length_ms,
                ..t
            }),
            position_ms: round1(position_ms),
            position_uncertainty_ms: uncertainty,
            rate: self.rate,
            seek: std::mem::take(&mut self.seek),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use protocol::{encode, Message, MAX_LINE_BYTES};

    const APP: &str = "AppleInc.AppleMusicWin_nzyj5cx40ttqa!App";

    fn playing(track_title: &str) -> Player {
        let mut p = Player::new(100.0);
        p.observe_track(track(track_title, "Indila — Mini World", ""));
        p.observe_status(Some(Status::Playing), 1.0);
        p
    }

    #[test]
    fn apple_music_artist_is_split_at_the_first_dash() {
        let t = track("S.O.S", "Indila — Mini World", "").unwrap();
        assert_eq!(t.artists, ["Indila"]);
        assert_eq!(t.album.as_deref(), Some("Mini World"));

        let t = track("x", "A — B — C", "").unwrap();
        assert_eq!(
            (t.artists[0].as_str(), t.album.as_deref()),
            ("A", Some("B — C"))
        );
    }

    #[test]
    fn artist_is_kept_whole_when_the_album_is_given() {
        let t = track("x", "A — B", "Album").unwrap();
        assert_eq!(t.artists, ["A — B"]);
        assert_eq!(t.album.as_deref(), Some("Album"));
        assert_eq!(track("x", "A", "").unwrap().album, None);
        assert_eq!(track("", "A", "B"), None);
    }

    #[test]
    fn no_artist_is_no_track() {
        // What Apple Music reports while it loads the next track.
        assert_eq!(track("연결 중…", "", ""), None);
        assert_eq!(track("x", " — Album", ""), None);
    }

    #[test]
    fn longest_fields_fit_a_line() {
        let worst = "\u{1}".repeat(MAX_FIELD_BYTES * 2);
        let mut p = Player::new(100.0);
        p.observe_track(track(&worst, &format!("{worst} — {worst}"), ""));
        p.observe_status(Some(Status::Playing), 1.0);
        let line = encode(&Message::State(p.state(APP, 0.0).unwrap()));
        assert!(line.len() < MAX_LINE_BYTES, "{} bytes", line.len());
    }

    #[test]
    fn clip_keeps_char_boundaries() {
        let s = "가".repeat(MAX_FIELD_BYTES);
        let clipped = clip(&s);
        assert!(clipped.len() <= MAX_FIELD_BYTES);
        assert!(clipped.chars().all(|c| c == '가'));
    }

    #[test]
    fn nothing_is_sent_before_a_status_and_changing_is_ignored() {
        let mut p = Player::new(100.0);
        assert!(p.state(APP, 0.0).is_none());
        assert!(!p.observe_status(None, 1.0));
        assert!(p.observe_status(Some(Status::Paused), 1.0));
        assert!(!p.observe_status(None, 1.0));
        assert!(!p.observe_status(Some(Status::Paused), 1.0));
        assert_eq!(p.state(APP, 0.0).unwrap().status, Status::Paused);
    }

    #[test]
    fn position_uses_the_estimate_once_there_is_one() {
        let mut p = playing("a");
        let s = p.state(APP, 10_000.0).unwrap();
        assert_eq!((s.position_ms, s.position_uncertainty_ms), (0.0, None));

        p.observe_timeline(5_000.0, Some(10_300.0), 272_000.0, 10_300.0); // origin (4300, 5300]
        p.observe_timeline(5_000.0, Some(10_900.0), 272_000.0, 10_900.0); // (4900, 5300]
        let s = p.state(APP, 11_000.0).unwrap();
        assert_eq!(
            (s.position_ms, s.position_uncertainty_ms),
            (5_900.0, Some(200.0))
        );
        assert_eq!(s.track.unwrap().length_ms, Some(272_000.0));
    }

    #[test]
    fn paused_sends_the_raw_position() {
        let mut p = playing("a");
        p.observe_timeline(5_000.0, Some(10_300.0), 0.0, 10_300.0);
        p.observe_status(Some(Status::Paused), 1.0);
        p.observe_timeline(5_000.0, Some(10_500.0), 0.0, 10_500.0);
        let s = p.state(APP, 20_000.0).unwrap();
        assert_eq!((s.position_ms, s.position_uncertainty_ms), (5_000.0, None));
        assert_eq!(s.track.unwrap().length_ms, None);
    }

    #[test]
    fn a_seek_is_held_back_until_two_updates_agree() {
        let mut p = playing("a");
        p.observe_timeline(5_000.0, Some(10_300.0), 0.0, 10_300.0);
        assert!(!p.observe_timeline(60_000.0, Some(10_572.0), 0.0, 10_572.0));
        assert!(p.state(APP, 10_600.0).is_none());
        // Apple Music flips back to the old position once, then holds the new one.
        assert!(!p.observe_timeline(5_000.0, Some(10_844.0), 0.0, 10_844.0));
        assert!(!p.observe_timeline(60_000.0, Some(11_116.0), 0.0, 11_116.0));
        assert!(!p.observe_timeline(60_000.0, Some(11_388.0), 0.0, 11_388.0));
        assert!(p.observe_timeline(61_000.0, Some(11_660.0), 0.0, 11_660.0));
        assert!(p.state(APP, 11_700.0).unwrap().seek);
        assert!(!p.state(APP, 11_800.0).unwrap().seek);
    }

    #[test]
    fn a_quiet_seek_settles_after_500_ms() {
        let mut p = playing("a");
        p.observe_timeline(5_000.0, Some(10_300.0), 0.0, 10_300.0);
        p.observe_timeline(60_000.0, Some(10_572.0), 0.0, 10_572.0);
        assert!(!p.poll(11_000.0));
        assert!(p.poll(11_072.0));
        assert!(p.state(APP, 11_100.0).unwrap().seek);
    }

    #[test]
    fn a_track_change_ends_settling_and_is_due_at_once() {
        let mut p = playing("a");
        p.observe_timeline(5_000.0, Some(10_300.0), 0.0, 10_300.0);
        // The new track's timeline can arrive before its media properties.
        p.observe_timeline(0.0, Some(10_572.0), 0.0, 10_572.0);
        assert!(p.settling());
        assert!(p.observe_track(track("b", "Indila — Mini World", "")));
        let s = p.state(APP, 10_600.0).unwrap();
        assert!(!s.seek);
        assert_eq!(s.track.unwrap().title, "b");
    }

    #[test]
    fn a_rereading_of_the_same_update_is_not_counted() {
        let mut p = playing("a");
        p.observe_timeline(5_000.0, Some(10_300.0), 0.0, 10_300.0);
        p.observe_timeline(60_000.0, Some(10_572.0), 0.0, 10_572.0);
        for now in [10_580.0, 10_590.0, 10_600.0] {
            assert!(!p.observe_timeline(60_000.0, Some(10_572.0), 0.0, now));
        }
        assert!(p.settling());
    }

    #[test]
    fn a_resume_does_not_extrapolate_across_the_pause() {
        let mut p = Player::new(100.0);
        p.observe_track(track("a", "Indila — Mini World", ""));
        p.observe_status(Some(Status::Paused), 1.0);
        p.observe_timeline(29_000.0, Some(10_000.0), 0.0, 10_000.0);
        // Resumed 2.5 s later, before the app stamps a new timeline.
        p.observe_status(Some(Status::Playing), 1.0);
        let s = p.state(APP, 12_500.0).unwrap();
        assert_eq!((s.position_ms, s.position_uncertainty_ms), (29_000.0, None));
    }

    #[test]
    fn a_track_change_drops_the_old_position() {
        let mut p = playing("a");
        p.observe_timeline(21_000.0, Some(10_000.0), 272_000.0, 10_000.0);
        p.observe_track(track("b", "Indila — Mini World", ""));
        let s = p.state(APP, 10_200.0).unwrap();
        assert_eq!(s.position_ms, 0.0);
        assert_eq!(s.track.unwrap().length_ms, None);
    }

    #[test]
    fn a_conflict_under_a_second_is_not_a_seek() {
        let mut p = playing("a");
        p.observe_timeline(5_000.0, Some(10_300.0), 0.0, 10_300.0); // origin (4300, 5300]
        p.observe_timeline(5_000.0, Some(10_999.0), 0.0, 10_999.0); // (4999, 5300]
        p.observe_timeline(6_000.0, Some(11_100.0), 0.0, 11_100.0); // (4999, 5100]
                                                                    // (5300, 6300]: 200 ms past the range, but the estimate moves only 0.75 s.
        assert!(!p.observe_timeline(5_000.0, Some(11_300.0), 0.0, 11_300.0));
        assert!(!p.settling());
        assert!(!p.state(APP, 11_500.0).unwrap().seek);
    }

    #[test]
    fn a_new_length_is_due_at_once() {
        let mut p = playing("a");
        assert!(p.observe_timeline(0.0, None, 272_000.0, 0.0));
        assert!(!p.observe_timeline(0.0, None, 272_000.0, 0.0));
    }
}
