//! Platform-independent timeline tracking: what the probe measures with and
//! the real reader (wshowlyrics/smtc-mpris-bridge#2) will send from, kept off
//! WinRT so it is testable anywhere.

/// Apple Music truncates positions to whole seconds (#1, note 4524).
pub const QUANTUM_MS: f64 = 1_000.0;
/// Boundary spacing in the first Apple Music log overshot by up to ~60 ms.
pub const DEFAULT_SLACK_MS: f64 = 100.0;

/// Milliseconds to 0.1 ms, as logged and sent.
pub fn round1(v: f64) -> f64 {
    (v * 10.0).round() / 10.0
}

/// Offset between the WinRT `DateTime` epoch (1601-01-01) and the Unix epoch, in 100 ns ticks.
const WINRT_TO_UNIX_TICKS: i64 = 116_444_736_000_000_000;

/// WinRT `DateTime::UniversalTime` (100 ns ticks since 1601) to Unix milliseconds.
pub fn winrt_ticks_to_unix_ms(ticks: i64) -> f64 {
    (ticks - WINRT_TO_UNIX_TICKS) as f64 / 10_000.0
}

/// `LastUpdatedTime` as Unix milliseconds, or `None` for the zero `DateTime`
/// an app reports before its first timeline update (Apple Music: until the
/// first play).
pub fn last_updated_unix_ms(ticks: i64) -> Option<f64> {
    (ticks != 0).then(|| winrt_ticks_to_unix_ms(ticks))
}

/// WinRT `TimeSpan::Duration` (100 ns ticks) to milliseconds.
pub fn span_ticks_to_ms(ticks: i64) -> f64 {
    ticks as f64 / 10_000.0
}

/// Position at `now_ms`, extrapolated from the app's last timeline update.
pub fn corrected_position_ms(
    position_ms: f64,
    last_updated_ms: f64,
    now_ms: f64,
    rate: f64,
    playing: bool,
) -> f64 {
    if playing {
        position_ms + (now_ms - last_updated_ms) * rate
    } else {
        position_ms
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct Timeline {
    pub position_ms: f64,
    pub last_updated_ms: f64,
    pub rate: f64,
    pub playing: bool,
    pub title: Option<String>,
}

/// How far a new timeline update lands from the previous one extrapolated to
/// its `LastUpdatedTime`. Near zero means `LastUpdatedTime` can be trusted.
///
/// Only defined while the same track kept playing between the two updates.
pub fn residual_ms(prev: &Timeline, cur: &Timeline) -> Option<f64> {
    if !(prev.playing && cur.playing) || prev.title != cur.title {
        return None;
    }
    let expected = corrected_position_ms(
        prev.position_ms,
        prev.last_updated_ms,
        cur.last_updated_ms,
        prev.rate,
        true,
    );
    Some(cur.position_ms - expected)
}

/// Recovers sub-second position from positions an app truncates to
/// `quantum_ms` (Apple Music reports whole seconds).
///
/// It tracks the playback origin: the Unix time at which position 0 would
/// have played. A report of `pos` stamped `lu` puts the origin in
/// `(lu - pos - quantum, lu - pos]`; reports are intersected, so the range
/// shrinks as updates land at different phases of a second. A conflict of up
/// to `slack_ms` is jitter between the app's position and its timestamp and
/// collapses the range to the middle of the gap; a larger one is a seek.
#[derive(Debug)]
pub struct PhaseEstimator {
    quantum_ms: f64,
    slack_ms: f64,
    range: Option<(f64, f64)>,
    /// Largest jitter since the last restart: a range collapsed by jitter is
    /// not exact, so the reported uncertainty never drops below it.
    max_jitter_ms: f64,
}

#[derive(Debug, PartialEq)]
pub enum Observed {
    Started,
    Narrowed,
    Jitter {
        gap_ms: f64,
    },
    /// Positive when the position jumped forward.
    Seek {
        jump_ms: f64,
    },
}

impl PhaseEstimator {
    pub fn new(quantum_ms: f64, slack_ms: f64) -> Self {
        Self {
            quantum_ms,
            slack_ms,
            range: None,
            max_jitter_ms: 0.0,
        }
    }

    /// Forget the origin; call when playback stops or the track changes.
    pub fn reset(&mut self) {
        self.range = None;
        self.max_jitter_ms = 0.0;
    }

    /// Feed a timeline update reported while playing.
    pub fn observe(&mut self, position_ms: f64, last_updated_ms: f64) -> Observed {
        let (lo, hi) = (
            last_updated_ms - position_ms - self.quantum_ms,
            last_updated_ms - position_ms,
        );
        let Some((a, b)) = self.range else {
            self.range = Some((lo, hi));
            return Observed::Started;
        };
        let (nlo, nhi) = (a.max(lo), b.min(hi));
        if nlo <= nhi {
            self.range = Some((nlo, nhi));
            return Observed::Narrowed;
        }
        let gap_ms = nlo - nhi;
        if gap_ms <= self.slack_ms {
            let mid = (nlo + nhi) / 2.0;
            self.range = Some((mid, mid));
            self.max_jitter_ms = self.max_jitter_ms.max(gap_ms);
            Observed::Jitter { gap_ms }
        } else {
            self.range = Some((lo, hi));
            self.max_jitter_ms = 0.0;
            Observed::Seek {
                jump_ms: (a + b) / 2.0 - (lo + hi) / 2.0,
            }
        }
    }

    /// Estimated position at `t_ms` and the half-width of its uncertainty.
    pub fn position_at(&self, t_ms: f64) -> Option<(f64, f64)> {
        self.range.map(|(lo, hi)| {
            (
                t_ms - (lo + hi) / 2.0,
                ((hi - lo) / 2.0).max(self.max_jitter_ms),
            )
        })
    }
}

/// How far a predicted quantum boundary falls outside the window in which the
/// app showed it happening: after the update still reporting the old value
/// (`prev_lu_ms`) and no later than the first one reporting the new value.
/// Zero inside; negative when predicted too early, positive when too late.
pub fn edge_miss_ms(predicted_ms: f64, prev_lu_ms: f64, cur_lu_ms: f64) -> f64 {
    if predicted_ms < prev_lu_ms {
        predicted_ms - prev_lu_ms
    } else if predicted_ms > cur_lu_ms {
        predicted_ms - cur_lu_ms
    } else {
        0.0
    }
}

/// One app's timeline as it evolves: the previous update and the estimator,
/// with the rules for when the estimate restarts.
#[derive(Debug)]
pub struct Tracker {
    quantum_ms: f64,
    last: Option<Timeline>,
    est: PhaseEstimator,
}

/// What one timeline update told the tracker.
#[derive(Debug, Default, PartialEq)]
pub struct Update {
    /// Time since the previous update's `LastUpdatedTime`.
    pub interval_ms: Option<f64>,
    /// See [`residual_ms`]; left out across a seek, which is not a timestamp error.
    pub residual_ms: Option<f64>,
    /// See [`edge_miss_ms`]; set when this update crossed a quantum boundary.
    pub edge_miss_ms: Option<f64>,
    /// `None` unless the update was playing and quantized.
    pub observed: Option<Observed>,
    /// Remaining uncertainty when the estimate carried on from a playing update.
    pub half_width_ms: Option<f64>,
    /// Playing, but the position was not a multiple of the quantum.
    pub unquantized: bool,
}

impl Tracker {
    pub fn new(quantum_ms: f64, slack_ms: f64) -> Self {
        Self {
            quantum_ms,
            last: None,
            est: PhaseEstimator::new(quantum_ms, slack_ms),
        }
    }

    /// Playback left the playing state; the origin no longer holds.
    pub fn stopped(&mut self) {
        self.est.reset();
    }

    pub fn observe(&mut self, tl: Timeline) -> Update {
        let prev = self.last.take();
        let mut update = Update {
            interval_ms: prev
                .as_ref()
                .map(|p| tl.last_updated_ms - p.last_updated_ms),
            ..Update::default()
        };
        if tl.playing && tl.position_ms % self.quantum_ms == 0.0 {
            if prev.as_ref().is_some_and(|p| p.title != tl.title) {
                self.est.reset();
            }
            // Check the prediction against a boundary the app just showed.
            if let (Some(p), Some((pos, _))) = (&prev, self.est.position_at(tl.last_updated_ms)) {
                if p.playing && tl.position_ms == p.position_ms + self.quantum_ms {
                    let predicted = tl.last_updated_ms - pos + tl.position_ms;
                    update.edge_miss_ms = Some(edge_miss_ms(
                        predicted,
                        p.last_updated_ms,
                        tl.last_updated_ms,
                    ));
                }
            }
            let observed = self.est.observe(tl.position_ms, tl.last_updated_ms);
            if !matches!(observed, Observed::Seek { .. })
                && prev.as_ref().is_some_and(|p| p.playing)
            {
                update.half_width_ms = self.est.position_at(tl.last_updated_ms).map(|(_, h)| h);
            }
            update.observed = Some(observed);
        } else if tl.playing {
            update.unquantized = true;
        }
        let seek = matches!(update.observed, Some(Observed::Seek { .. }));
        update.residual_ms = prev
            .as_ref()
            .and_then(|p| residual_ms(p, &tl))
            .filter(|_| !seek);
        self.last = Some(tl);
        update
    }

    /// Estimated position at `t_ms` and its uncertainty, while playing.
    pub fn position_at(&self, t_ms: f64) -> Option<(f64, f64)> {
        self.est.position_at(t_ms)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tl(position_ms: f64, at_ms: f64, playing: bool, title: &str) -> Timeline {
        Timeline {
            position_ms,
            last_updated_ms: at_ms,
            rate: 1.0,
            playing,
            title: Some(title.into()),
        }
    }

    #[test]
    fn winrt_epoch_maps_to_unix_epoch() {
        assert_eq!(winrt_ticks_to_unix_ms(WINRT_TO_UNIX_TICKS), 0.0);
        assert_eq!(winrt_ticks_to_unix_ms(WINRT_TO_UNIX_TICKS + 15_000), 1.5);
        assert_eq!(span_ticks_to_ms(30_000_000), 3_000.0);
    }

    #[test]
    fn corrected_position_extrapolates_while_playing() {
        assert_eq!(
            corrected_position_ms(10_000.0, 0.0, 2_500.0, 1.0, true),
            12_500.0
        );
        assert_eq!(corrected_position_ms(0.0, 0.0, 2_000.0, 1.5, true), 3_000.0);
    }

    #[test]
    fn corrected_position_frozen_while_paused() {
        assert_eq!(
            corrected_position_ms(10_000.0, 0.0, 30_000.0, 1.0, false),
            10_000.0
        );
    }

    #[test]
    fn residual_zero_when_last_updated_is_accurate() {
        assert_eq!(
            residual_ms(
                &tl(10_000.0, 0.0, true, "a"),
                &tl(15_000.0, 5_000.0, true, "a")
            ),
            Some(0.0)
        );
    }

    #[test]
    fn residual_shows_stale_last_updated() {
        // The app reports 5 s of progress but stamps the update only 4 s later.
        assert_eq!(
            residual_ms(
                &tl(10_000.0, 0.0, true, "a"),
                &tl(15_000.0, 4_000.0, true, "a")
            ),
            Some(1_000.0)
        );
    }

    #[test]
    fn residual_undefined_across_pause_or_track_change() {
        assert_eq!(
            residual_ms(
                &tl(10_000.0, 0.0, false, "a"),
                &tl(10_000.0, 5_000.0, true, "a")
            ),
            None
        );
        assert_eq!(
            residual_ms(&tl(10_000.0, 0.0, true, "a"), &tl(0.0, 5_000.0, true, "b")),
            None
        );
    }

    #[test]
    fn zero_last_updated_is_unknown() {
        assert_eq!(last_updated_unix_ms(0), None);
        assert_eq!(last_updated_unix_ms(WINRT_TO_UNIX_TICKS), Some(0.0));
    }

    #[test]
    fn estimator_intersects_truncated_reports() {
        let mut e = PhaseEstimator::new(1_000.0, 100.0);
        assert_eq!(e.observe(5_000.0, 10_300.0), Observed::Started); // origin (4300, 5300]
        assert_eq!(e.observe(5_000.0, 10_900.0), Observed::Narrowed); // (4900, 5300]
        assert_eq!(e.observe(6_000.0, 11_100.0), Observed::Narrowed); // (4900, 5100]
        assert_eq!(e.position_at(11_100.0), Some((6_100.0, 100.0)));
    }

    #[test]
    fn estimator_absorbs_small_conflicts_as_jitter() {
        let mut e = PhaseEstimator::new(1_000.0, 100.0);
        e.observe(5_000.0, 10_300.0);
        e.observe(6_000.0, 11_100.0); // (4300, 5100]
        assert_eq!(
            e.observe(5_000.0, 11_150.0), // (5150, 6150]: 50 ms past the range
            Observed::Jitter { gap_ms: 50.0 }
        );
        // Collapsed to a point, but the uncertainty keeps the jitter as a floor.
        assert_eq!(e.position_at(11_150.0), Some((6_025.0, 50.0)));
    }

    #[test]
    fn jitter_floor_clears_on_restart() {
        let mut e = PhaseEstimator::new(1_000.0, 100.0);
        e.observe(5_000.0, 10_300.0);
        e.observe(6_000.0, 11_100.0);
        e.observe(5_000.0, 11_150.0); // 50 ms jitter
        e.reset();
        e.observe(5_000.0, 10_300.0);
        assert_eq!(e.position_at(10_300.0), Some((5_500.0, 500.0)));
        e.observe(5_000.0, 10_999.0); // (4999, 5300]
        assert_eq!(e.position_at(10_999.0).unwrap().1, 150.5);
    }

    #[test]
    fn estimator_restarts_on_seek() {
        let mut e = PhaseEstimator::new(1_000.0, 100.0);
        e.observe(5_000.0, 10_300.0); // origin mid 4800
        match e.observe(60_000.0, 10_600.0) {
            // origin mid -49900: 54.7 s forward
            Observed::Seek { jump_ms } => assert_eq!(jump_ms, 54_700.0),
            other => panic!("expected a seek, got {other:?}"),
        }
        assert_eq!(e.position_at(10_600.0), Some((60_500.0, 500.0)));
    }

    #[test]
    fn estimator_converges_on_apple_music_cadence() {
        // Apple Music: whole-second positions, an update every ~272 ms. Without
        // jitter the true position must never leave the estimated range, and
        // after ~33 s (120 updates) the range is down to a few ms.
        for origin in [1_000.3, 1_137.0, 1_501.9, 1_888.8] {
            for period in [272.0, 271.7, 268.0] {
                let mut e = PhaseEstimator::new(1_000.0, 100.0);
                let mut half = f64::INFINITY;
                for k in 0..120 {
                    let lu = 2_000.0 + k as f64 * period;
                    let pos = ((lu - origin) / 1_000.0).floor() * 1_000.0;
                    assert!(!matches!(e.observe(pos, lu), Observed::Seek { .. }));
                    let (est, h) = e.position_at(lu).unwrap();
                    let err = est - (lu - origin);
                    assert!(
                        err.abs() <= h + 1e-6,
                        "origin {origin} period {period} k {k}: error {err} outside ±{h}"
                    );
                    half = h;
                }
                assert!(
                    half < 10.0,
                    "origin {origin} period {period}: half-width {half}"
                );
            }
        }
    }

    #[test]
    fn edge_miss_is_zero_inside_the_window() {
        assert_eq!(edge_miss_ms(1_100.0, 1_000.0, 1_272.0), 0.0);
        assert_eq!(edge_miss_ms(950.0, 1_000.0, 1_272.0), -50.0);
        assert_eq!(edge_miss_ms(1_300.0, 1_000.0, 1_272.0), 28.0);
    }

    #[test]
    fn tracker_reports_interval_residual_and_edges() {
        let mut t = Tracker::new(1_000.0, 100.0);
        let first = t.observe(tl(5_000.0, 10_300.0, true, "a"));
        assert_eq!(first.observed, Some(Observed::Started));
        assert_eq!((first.interval_ms, first.half_width_ms), (None, None));

        let same_second = t.observe(tl(5_000.0, 10_900.0, true, "a"));
        assert_eq!(same_second.interval_ms, Some(600.0));
        assert_eq!(same_second.residual_ms, Some(-600.0));
        assert_eq!(same_second.edge_miss_ms, None);
        assert_eq!(same_second.half_width_ms, Some(200.0)); // (4900, 5300]

        // Crossing into 6 s: the boundary predicted from origin 5100 is 11100,
        // inside the window (10900, 11100].
        let crossed = t.observe(tl(6_000.0, 11_100.0, true, "a"));
        assert_eq!(crossed.edge_miss_ms, Some(0.0));
        assert_eq!(t.position_at(11_100.0), Some((6_100.0, 100.0)));
    }

    #[test]
    fn tracker_restarts_on_track_change_and_stop() {
        let mut t = Tracker::new(1_000.0, 100.0);
        t.observe(tl(5_000.0, 10_300.0, true, "a"));
        t.observe(tl(5_000.0, 10_900.0, true, "a"));
        let next_track = t.observe(tl(0.0, 11_000.0, true, "b"));
        assert_eq!(next_track.observed, Some(Observed::Started));
        assert_eq!(next_track.residual_ms, None);

        t.stopped();
        assert_eq!(t.position_at(11_000.0), None);
        let paused = t.observe(tl(1_000.0, 12_000.0, false, "b"));
        assert_eq!(paused.observed, None);
    }

    #[test]
    fn tracker_keeps_seeks_out_of_residuals() {
        let mut t = Tracker::new(1_000.0, 100.0);
        t.observe(tl(5_000.0, 10_300.0, true, "a"));
        let seek = t.observe(tl(60_000.0, 10_600.0, true, "a"));
        assert!(matches!(seek.observed, Some(Observed::Seek { .. })));
        assert_eq!((seek.residual_ms, seek.half_width_ms), (None, None));
    }

    #[test]
    fn tracker_flags_unquantized_positions() {
        let mut t = Tracker::new(1_000.0, 100.0);
        let u = t.observe(tl(5_250.0, 10_300.0, true, "a"));
        assert!(u.unquantized);
        assert_eq!(u.observed, None);
    }
}
