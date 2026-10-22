//! One guest player's playback as last reported, and what each new `state`
//! changes (docs/protocol.md "Host rules"). Kept off D-Bus so it is testable.

use protocol::{State, Status, Track, SEEK_THRESHOLD_MS};
use std::time::Instant;

#[derive(Debug)]
pub struct Playback {
    pub status: Status,
    pub track: Option<Track>,
    pub rate: f64,
    position_ms: f64,
    /// Local monotonic time the position was received at.
    received: Instant,
    /// Counts track changes; `mpris:trackid` is built from it.
    pub track_seq: u64,
}

/// What a `state` changed, i.e. which MPRIS signals to emit.
#[derive(Debug, Default, PartialEq)]
pub struct Changes {
    pub status: bool,
    pub metadata: bool,
    pub rate: bool,
    pub seeked: bool,
}

/// The same song, ignoring a length that arrives after the title.
fn same_track(a: &Option<Track>, b: &Option<Track>) -> bool {
    match (a, b) {
        (Some(a), Some(b)) => a.title == b.title && a.artists == b.artists && a.album == b.album,
        (None, None) => true,
        _ => false,
    }
}

impl Playback {
    pub fn new(state: State, now: Instant) -> Self {
        Self {
            status: state.status,
            track: state.track,
            rate: state.rate,
            position_ms: state.position_ms,
            received: now,
            track_seq: 0,
        }
    }

    /// Network delay on the libvirt bridge is well under a millisecond and is ignored.
    pub fn position_ms(&self, now: Instant) -> f64 {
        let pos = if self.status == Status::Playing {
            self.position_ms + now.duration_since(self.received).as_secs_f64() * 1000.0 * self.rate
        } else {
            self.position_ms
        };
        match self.track.as_ref().and_then(|t| t.length_ms) {
            Some(length) => pos.clamp(0.0, length),
            None => pos.max(0.0),
        }
    }

    pub fn apply(&mut self, state: State, now: Instant) -> Changes {
        let new_track = !same_track(&self.track, &state.track);
        // A heartbeat far from where playback should be is a seek the guest missed.
        let missed_seek = self.status == Status::Playing
            && state.status == Status::Playing
            && (state.position_ms - self.position_ms(now)).abs() > SEEK_THRESHOLD_MS;
        let changes = Changes {
            status: state.status != self.status,
            metadata: state.track != self.track,
            rate: state.rate != self.rate,
            seeked: !new_track && (state.seek || missed_seek),
        };
        if new_track {
            self.track_seq += 1;
        }
        self.status = state.status;
        self.track = state.track;
        self.rate = state.rate;
        self.position_ms = state.position_ms;
        self.received = now;
        changes
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn state(status: Status, title: &str, position_ms: f64) -> State {
        State {
            player: "app".into(),
            status,
            track: Some(Track {
                title: title.into(),
                artists: vec!["Indila".into()],
                album: None,
                length_ms: Some(272_000.0),
            }),
            position_ms,
            position_uncertainty_ms: None,
            rate: 1.0,
            seek: false,
        }
    }

    #[test]
    fn position_runs_only_while_playing() {
        let t0 = Instant::now();
        let mut p = Playback::new(state(Status::Playing, "a", 10_000.0), t0);
        assert_eq!(p.position_ms(t0 + Duration::from_millis(1_500)), 11_500.0);
        p.apply(
            state(Status::Paused, "a", 11_500.0),
            t0 + Duration::from_millis(1_500),
        );
        assert_eq!(p.position_ms(t0 + Duration::from_secs(60)), 11_500.0);
    }

    #[test]
    fn position_stays_inside_the_track() {
        let t0 = Instant::now();
        let p = Playback::new(state(Status::Playing, "a", 271_000.0), t0);
        assert_eq!(p.position_ms(t0 + Duration::from_secs(10)), 272_000.0);
    }

    #[test]
    fn heartbeats_change_nothing() {
        let t0 = Instant::now();
        let mut p = Playback::new(state(Status::Playing, "a", 10_000.0), t0);
        let c = p.apply(
            state(Status::Playing, "a", 11_020.0),
            t0 + Duration::from_secs(1),
        );
        assert_eq!(c, Changes::default());
    }

    #[test]
    fn a_far_heartbeat_is_a_missed_seek() {
        let t0 = Instant::now();
        let mut p = Playback::new(state(Status::Playing, "a", 10_000.0), t0);
        let c = p.apply(
            state(Status::Playing, "a", 60_000.0),
            t0 + Duration::from_secs(1),
        );
        assert!(c.seeked);
    }

    #[test]
    fn a_resume_elsewhere_is_not_a_seek_but_a_flagged_one_is() {
        let t0 = Instant::now();
        let mut p = Playback::new(state(Status::Paused, "a", 10_000.0), t0);
        let c = p.apply(state(Status::Playing, "a", 40_000.0), t0);
        assert_eq!((c.status, c.seeked), (true, false));

        let mut seek = state(Status::Paused, "a", 5_000.0);
        seek.seek = true;
        assert!(p.apply(seek, t0).seeked);
    }

    #[test]
    fn a_new_track_bumps_the_id_but_a_late_length_does_not() {
        let t0 = Instant::now();
        let mut p = Playback::new(state(Status::Playing, "a", 200_000.0), t0);
        let mut next = state(Status::Playing, "b", 0.0);
        next.track.as_mut().unwrap().length_ms = None;
        let c = p.apply(next, t0);
        assert_eq!((c.metadata, c.seeked, p.track_seq), (true, false, 1));

        let c = p.apply(state(Status::Playing, "b", 500.0), t0);
        assert_eq!((c.metadata, p.track_seq), (true, 1));
    }

    #[test]
    fn no_track_to_no_track_is_the_same() {
        let t0 = Instant::now();
        let mut idle = state(Status::Stopped, "a", 0.0);
        idle.track = None;
        let mut p = Playback::new(idle.clone(), t0);
        assert_eq!(p.apply(idle, t0), Changes::default());
    }
}
