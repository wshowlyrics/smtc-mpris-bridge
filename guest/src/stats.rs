//! Measurement-only statistics for the probe (wshowlyrics/smtc-mpris-bridge#1).
//! The real reader does not keep these: they grow with every update.

use crate::timeline::{round1, Observed, Update};
use serde_json::{json, Map, Value};
use std::collections::BTreeMap;

#[derive(Default, Debug)]
pub struct AppStats {
    pub timeline_events: u32,
    pub update_intervals_ms: Vec<f64>,
    pub residuals_ms: Vec<f64>,
    pub arrival_ages_ms: Vec<f64>,
    pub seeks: u32,
    pub unquantized: u32,
    pub jitter_gaps_ms: Vec<f64>,
    pub half_widths_ms: Vec<f64>,
    pub edge_misses_ms: Vec<f64>,
}

impl AppStats {
    pub fn add(&mut self, arrival_age_ms: f64, u: &Update) {
        self.arrival_ages_ms.push(arrival_age_ms);
        self.update_intervals_ms.extend(u.interval_ms);
        self.residuals_ms.extend(u.residual_ms);
        self.edge_misses_ms.extend(u.edge_miss_ms);
        self.half_widths_ms.extend(u.half_width_ms);
        match u.observed {
            Some(Observed::Seek { .. }) => self.seeks += 1,
            Some(Observed::Jitter { gap_ms }) => self.jitter_gaps_ms.push(gap_ms),
            _ => {}
        }
        self.unquantized += u32::from(u.unquantized);
    }
}

fn spread(values: &[f64]) -> Value {
    if values.is_empty() {
        return Value::Null;
    }
    let mut sorted = values.to_vec();
    sorted.sort_by(f64::total_cmp);
    let n = sorted.len();
    let median = if n % 2 == 1 {
        sorted[n / 2]
    } else {
        (sorted[n / 2 - 1] + sorted[n / 2]) / 2.0
    };
    json!({"n": n, "median": round1(median), "min": round1(sorted[0]), "max": round1(sorted[n - 1])})
}

pub fn summarize(stats: &BTreeMap<String, AppStats>) -> Value {
    stats
        .iter()
        .map(|(app, s)| {
            (
                app.clone(),
                json!({
                    "timeline_events": s.timeline_events,
                    "update_interval_ms": spread(&s.update_intervals_ms),
                    "residual_ms": spread(&s.residuals_ms),
                    "arrival_age_ms": spread(&s.arrival_ages_ms),
                    "seeks": s.seeks,
                    "unquantized_reports": s.unquantized,
                    "estimator": {
                        "jitter_gap_ms": spread(&s.jitter_gaps_ms),
                        "half_width_ms": spread(&s.half_widths_ms),
                        "edge_miss_ms": spread(&s.edge_misses_ms),
                    },
                }),
            )
        })
        .collect::<Map<_, _>>()
        .into()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn add_files_each_outcome() {
        let mut s = AppStats::default();
        s.add(
            1.5,
            &Update {
                interval_ms: Some(272.0),
                observed: Some(Observed::Jitter { gap_ms: 3.0 }),
                half_width_ms: Some(3.0),
                ..Update::default()
            },
        );
        s.add(
            0.5,
            &Update {
                observed: Some(Observed::Seek { jump_ms: 9.0 }),
                unquantized: true,
                ..Update::default()
            },
        );
        assert_eq!(s.arrival_ages_ms, vec![1.5, 0.5]);
        assert_eq!(s.update_intervals_ms, vec![272.0]);
        assert_eq!(s.jitter_gaps_ms, vec![3.0]);
        assert_eq!((s.seeks, s.unquantized), (1, 1));
    }

    #[test]
    fn summarize_reports_spread_per_app() {
        let mut stats = BTreeMap::new();
        stats.insert(
            "AppleMusic".to_string(),
            AppStats {
                timeline_events: 3,
                update_intervals_ms: vec![1_000.0, 3_000.0],
                residuals_ms: vec![20.0, -5.0, 5.0],
                seeks: 1,
                ..Default::default()
            },
        );
        let out = &summarize(&stats)["AppleMusic"];
        assert_eq!(out["timeline_events"], 3);
        assert_eq!(
            out["update_interval_ms"],
            json!({"n": 2, "median": 2000.0, "min": 1000.0, "max": 3000.0})
        );
        assert_eq!(out["residual_ms"]["median"], 5.0);
        assert_eq!(out["arrival_age_ms"], Value::Null);
        assert_eq!(out["seeks"], 1);
        assert_eq!(out["estimator"]["half_width_ms"], Value::Null);
    }
}
