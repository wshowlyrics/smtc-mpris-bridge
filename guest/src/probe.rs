//! Log mode: record what media apps publish to SMTC without sending anything
//! (wshowlyrics/smtc-mpris-bridge#1).
//!
//! Every session event, a periodic sample and any mark typed on stdin go to a
//! JSON Lines file. Type what the player shows (e.g. "1:23") and press Enter
//! to put a mark next to the computed position; `q` or Ctrl+C prints a summary.

use crate::stats::{self, round1, AppStats};
use crate::timeline::{self, Observed, Timeline, Tracker};
use serde_json::{json, Map, Value};
use std::collections::{BTreeMap, HashMap};
use std::fs::File;
use std::io::{BufRead, Write};
use std::path::PathBuf;
use std::sync::mpsc::{self, RecvTimeoutError, Sender};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use windows::core::Result;
use windows::Foundation::{EventRegistrationToken, TypedEventHandler};
use windows::Media::Control::{
    GlobalSystemMediaTransportControlsSession as Session,
    GlobalSystemMediaTransportControlsSessionManager as Manager,
    GlobalSystemMediaTransportControlsSessionPlaybackStatus as Status,
};

/// Apple Music truncates positions to whole seconds (#1, note 4524).
const QUANTUM_MS: f64 = 1_000.0;

#[derive(Clone, Copy, Debug)]
enum Kind {
    Timeline,
    Playback,
    Media,
}

enum Msg {
    Session(Kind, String),
    Resubscribe,
    Mark(String),
    Quit,
}

fn now_ms() -> f64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs_f64()
        * 1000.0
}

fn status_name(s: Status) -> &'static str {
    match s {
        Status::Closed => "CLOSED",
        Status::Opened => "OPENED",
        Status::Changing => "CHANGING",
        Status::Stopped => "STOPPED",
        Status::Playing => "PLAYING",
        Status::Paused => "PAUSED",
        _ => "UNKNOWN",
    }
}

fn obj(v: Value) -> Map<String, Value> {
    match v {
        Value::Object(m) => m,
        _ => Map::new(),
    }
}

fn insert_estimate(fields: &mut Map<String, Value>, (pos, half): (f64, f64)) {
    fields.insert("est_ms".into(), json!(round1(pos)));
    fields.insert("est_half_width_ms".into(), json!(round1(half)));
}

struct Log {
    file: File,
}

impl Log {
    fn write(&mut self, event: &str, mut fields: Map<String, Value>) {
        fields.insert("ts_ms".into(), json!(round1(now_ms())));
        fields.insert("event".into(), json!(event));
        let line = Value::Object(fields).to_string();
        // A full disk should not stop the measurement; stdout still shows it.
        let _ = self.file.write_all(format!("{line}\n").as_bytes());
        println!("{line}");
    }
}

/// A session's state read at one moment.
struct Snapshot {
    app: String,
    fields: Map<String, Value>,
    /// `None` until the app stamps a timeline (`LastUpdatedTime` is zero before the first play).
    timeline: Option<Timeline>,
    age_ms: Option<f64>,
}

struct Subscription {
    app: String,
    session: Session,
    tokens: [EventRegistrationToken; 3],
}

struct Probe {
    log: Log,
    manager: Manager,
    tx: Sender<Msg>,
    subscriptions: Vec<Subscription>,
    titles: HashMap<String, Option<String>>,
    trackers: HashMap<String, Tracker>,
    slack_ms: f64,
    stats: BTreeMap<String, AppStats>,
}

impl Probe {
    fn sessions(&self) -> Vec<Session> {
        let Ok(list) = self.manager.GetSessions() else {
            return Vec::new();
        };
        (0..list.Size().unwrap_or(0))
            .filter_map(|i| list.GetAt(i).ok())
            .collect()
    }

    fn find(&self, app: &str) -> Option<Session> {
        self.subscriptions
            .iter()
            .find(|sub| sub.app == app)
            .map(|sub| sub.session.clone())
    }

    fn snapshot(&self, s: &Session) -> Result<Snapshot> {
        let app = s.SourceAppUserModelId()?.to_string();
        let now = now_ms();
        let tl = s.GetTimelineProperties()?;
        let info = s.GetPlaybackInfo()?;
        let status = info.PlaybackStatus()?;
        let playing = status == Status::Playing;
        let rate = info.PlaybackRate().and_then(|r| r.Value()).unwrap_or(1.0);
        let start = tl.StartTime()?.Duration;
        let position_ms = timeline::span_ticks_to_ms(tl.Position()?.Duration - start);
        let last_updated_ms = timeline::last_updated_unix_ms(tl.LastUpdatedTime()?.UniversalTime);
        let age_ms = last_updated_ms.map(|lu| now - lu);
        let mut fields = obj(json!({
            "app": app,
            "status": status_name(status),
            "rate": rate,
            "position_ms": round1(position_ms),
            "end_ms": round1(timeline::span_ticks_to_ms(tl.EndTime()?.Duration - start)),
            "last_updated_ms": last_updated_ms.map(round1),
            "age_ms": age_ms.map(round1),
            "corrected_ms": round1(last_updated_ms.map_or(position_ms, |lu| {
                timeline::corrected_position_ms(position_ms, lu, now, rate, playing)
            })),
        }));
        if let Some(est) = self
            .trackers
            .get(&app)
            .filter(|_| playing)
            .and_then(|t| t.position_at(now))
        {
            insert_estimate(&mut fields, est);
        }
        let title = self.titles.get(&app).cloned().flatten();
        let timeline = last_updated_ms.map(|last_updated_ms| Timeline {
            position_ms,
            last_updated_ms,
            rate,
            playing,
            title,
        });
        Ok(Snapshot {
            app,
            fields,
            timeline,
            age_ms,
        })
    }

    fn record(&mut self, event: &str, s: &Session) {
        let Snapshot {
            app,
            mut fields,
            timeline,
            age_ms,
        } = match self.snapshot(s) {
            Ok(v) => v,
            // The session can close between the event and the read.
            Err(e) => return self.log.write(event, obj(json!({"error": e.to_string()}))),
        };
        let slack_ms = self.slack_ms;
        let tracker = self
            .trackers
            .entry(app.clone())
            .or_insert_with(|| Tracker::new(QUANTUM_MS, slack_ms));
        if !timeline.as_ref().is_some_and(|t| t.playing) {
            tracker.stopped();
        }
        if event == "timeline" {
            let stats = self.stats.entry(app).or_default();
            stats.timeline_events += 1;
            if let Some(tl) = timeline {
                let lu = tl.last_updated_ms;
                let u = tracker.observe(tl);
                stats.add(age_ms.unwrap_or_default(), &u);
                for (key, value) in [
                    ("update_interval_ms", u.interval_ms),
                    ("residual_ms", u.residual_ms),
                    ("edge_miss_ms", u.edge_miss_ms),
                ] {
                    if let Some(v) = value {
                        fields.insert(key.into(), json!(round1(v)));
                    }
                }
                match u.observed {
                    Some(Observed::Seek { jump_ms }) => {
                        fields.insert("seek_jump_ms".into(), json!(round1(jump_ms)));
                    }
                    Some(Observed::Jitter { gap_ms }) => {
                        fields.insert("jitter_gap_ms".into(), json!(round1(gap_ms)));
                    }
                    _ => {}
                }
                if u.observed.is_some() {
                    if let Some(est) = tracker.position_at(lu) {
                        insert_estimate(&mut fields, est);
                    }
                }
            }
        }
        self.log.write(event, fields);
    }

    fn record_media(&mut self, s: &Session) {
        let read = || -> Result<(String, Map<String, Value>)> {
            let app = s.SourceAppUserModelId()?.to_string();
            let p = s.TryGetMediaPropertiesAsync()?.get()?;
            let fields = obj(json!({
                "app": app,
                "title": p.Title()?.to_string(),
                "artist": p.Artist()?.to_string(),
                "album_title": p.AlbumTitle()?.to_string(),
                "album_artist": p.AlbumArtist()?.to_string(),
                "subtitle": p.Subtitle()?.to_string(),
                "track_number": p.TrackNumber()?,
                "playback_type": p.PlaybackType().and_then(|t| t.Value()).map(|t| t.0).ok(),
                "has_thumbnail": p.Thumbnail().is_ok(),
            }));
            Ok((app, fields))
        };
        match read() {
            Ok((app, fields)) => {
                self.titles
                    .insert(app, fields["title"].as_str().map(String::from));
                self.log.write("media", fields);
            }
            Err(e) => self
                .log
                .write("media", obj(json!({"error": e.to_string()}))),
        }
    }

    fn handler<A>(&self, kind: Kind) -> TypedEventHandler<Session, A>
    where
        A: windows::core::RuntimeType + 'static,
    {
        let tx = self.tx.clone();
        TypedEventHandler::new(move |sender: &Option<Session>, _| {
            if let Some(app) = sender.as_ref().and_then(|s| s.SourceAppUserModelId().ok()) {
                let _ = tx.send(Msg::Session(kind, app.to_string()));
            }
            Ok(())
        })
    }

    fn resubscribe(&mut self) {
        for sub in self.subscriptions.drain(..) {
            let [t, p, m] = sub.tokens;
            let _ = sub.session.RemoveTimelinePropertiesChanged(t);
            let _ = sub.session.RemovePlaybackInfoChanged(p);
            let _ = sub.session.RemoveMediaPropertiesChanged(m);
        }
        let sessions: Vec<(String, Session)> = self
            .sessions()
            .into_iter()
            .filter_map(|s| Some((s.SourceAppUserModelId().ok()?.to_string(), s)))
            .collect();
        let current = self
            .manager
            .GetCurrentSession()
            .ok()
            .and_then(|s| s.SourceAppUserModelId().ok())
            .map(|id| id.to_string());
        let apps: Vec<&str> = sessions.iter().map(|(app, _)| app.as_str()).collect();
        self.log
            .write("sessions", obj(json!({"apps": apps, "current": current})));
        for (app, session) in sessions {
            let tokens = (|| -> Result<[EventRegistrationToken; 3]> {
                Ok([
                    session.TimelinePropertiesChanged(&self.handler(Kind::Timeline))?,
                    session.PlaybackInfoChanged(&self.handler(Kind::Playback))?,
                    session.MediaPropertiesChanged(&self.handler(Kind::Media))?,
                ])
            })();
            self.record_media(&session);
            self.record("initial", &session);
            match tokens {
                Ok(tokens) => self.subscriptions.push(Subscription {
                    app,
                    session,
                    tokens,
                }),
                Err(e) => self
                    .log
                    .write("subscribe", obj(json!({"error": e.to_string()}))),
            }
        }
    }

    fn mark(&mut self, note: String) {
        let snaps: Vec<Value> = self
            .sessions()
            .iter()
            .filter_map(|s| self.snapshot(s).ok().map(|snap| Value::Object(snap.fields)))
            .collect();
        self.log
            .write("mark", obj(json!({"note": note, "sessions": snaps})));
    }

    fn on_session(&mut self, kind: Kind, app: &str) {
        let Some(s) = self.find(app) else { return };
        match kind {
            Kind::Timeline => self.record("timeline", &s),
            Kind::Playback => self.record("playback", &s),
            Kind::Media => self.record_media(&s),
        }
    }
}

pub fn run(
    out_dir: PathBuf,
    interval: Duration,
    slack_ms: f64,
) -> std::result::Result<(), Box<dyn std::error::Error>> {
    std::fs::create_dir_all(&out_dir)?;
    let path = out_dir.join(format!("smtc-{}.jsonl", (now_ms() / 1000.0) as u64));
    let file = File::create(&path)?;

    let (tx, rx) = mpsc::channel();
    let manager = Manager::RequestAsync()?.get()?;
    fn changed<A: windows::core::RuntimeType + 'static>(
        tx: Sender<Msg>,
    ) -> TypedEventHandler<Manager, A> {
        TypedEventHandler::new(move |_, _| {
            let _ = tx.send(Msg::Resubscribe);
            Ok(())
        })
    }
    manager.SessionsChanged(&changed(tx.clone()))?;
    manager.CurrentSessionChanged(&changed(tx.clone()))?;

    let quit = tx.clone();
    let _ = ctrlc::set_handler(move || {
        let _ = quit.send(Msg::Quit);
    });
    let marks = tx.clone();
    std::thread::spawn(move || {
        for line in std::io::stdin()
            .lock()
            .lines()
            .map_while(std::io::Result::ok)
        {
            let line = line.trim().to_string();
            let msg = if line == "q" {
                Msg::Quit
            } else {
                Msg::Mark(line)
            };
            if marks.send(msg).is_err() {
                break;
            }
        }
    });

    let mut probe = Probe {
        log: Log { file },
        manager,
        tx,
        subscriptions: Vec::new(),
        titles: HashMap::new(),
        trackers: HashMap::new(),
        slack_ms,
        stats: BTreeMap::new(),
    };
    probe.resubscribe();
    // A deadline, not a plain timeout: timeline events arrive every ~272 ms
    // and would otherwise restart the wait before a sample is ever due.
    let mut next_sample = Instant::now() + interval;
    loop {
        match rx.recv_timeout(next_sample.saturating_duration_since(Instant::now())) {
            Ok(Msg::Session(kind, app)) => probe.on_session(kind, &app),
            Ok(Msg::Resubscribe) => probe.resubscribe(),
            Ok(Msg::Mark(note)) => probe.mark(note),
            Ok(Msg::Quit) | Err(RecvTimeoutError::Disconnected) => break,
            Err(RecvTimeoutError::Timeout) => {
                for s in probe.sessions() {
                    probe.record("sample", &s);
                }
                next_sample = Instant::now() + interval;
            }
        }
    }
    let summary = stats::summarize(&probe.stats);
    probe.log.write("summary", obj(json!({"apps": summary})));
    eprintln!("\nlog: {}", path.display());
    Ok(())
}
