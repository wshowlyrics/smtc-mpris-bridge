//! WinRT plumbing shared by `probe` and `run`: the session manager, one
//! subscription per session, and session events forwarded to the main loop.

use crate::timeline;
use std::sync::mpsc::Sender;
use std::time::{SystemTime, UNIX_EPOCH};
use windows::core::Result;
use windows::Foundation::{EventRegistrationToken, TypedEventHandler};
pub use windows::Media::Control::{
    GlobalSystemMediaTransportControlsSession as Session,
    GlobalSystemMediaTransportControlsSessionManager as Manager,
    GlobalSystemMediaTransportControlsSessionPlaybackStatus as Status,
};

/// Wall clock as Unix milliseconds, the time base of `LastUpdatedTime`.
pub fn now_ms() -> f64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs_f64()
        * 1000.0
}

#[derive(Clone, Copy, Debug)]
pub enum Kind {
    Timeline,
    Playback,
    Media,
}

pub enum Event {
    /// Something changed in the session of this app.
    Session(Kind, String),
    /// Sessions came or went, or the current one changed.
    SessionsChanged,
}

/// Playback info and timeline of a session, read at one moment.
pub struct Raw {
    pub status: Status,
    pub rate: f64,
    pub position_ms: f64,
    pub end_ms: f64,
    /// `None` until the app stamps a timeline (`LastUpdatedTime` is zero before the first play).
    pub last_updated_ms: Option<f64>,
}

pub fn read(s: &Session) -> Result<Raw> {
    let tl = s.GetTimelineProperties()?;
    let info = s.GetPlaybackInfo()?;
    let start = tl.StartTime()?.Duration;
    Ok(Raw {
        status: info.PlaybackStatus()?,
        rate: info.PlaybackRate().and_then(|r| r.Value()).unwrap_or(1.0),
        position_ms: timeline::span_ticks_to_ms(tl.Position()?.Duration - start),
        end_ms: timeline::span_ticks_to_ms(tl.EndTime()?.Duration - start),
        last_updated_ms: timeline::last_updated_unix_ms(tl.LastUpdatedTime()?.UniversalTime),
    })
}

pub fn app_id(s: &Session) -> Result<String> {
    Ok(s.SourceAppUserModelId()?.to_string())
}

struct Subscription {
    app: String,
    session: Session,
    tokens: [EventRegistrationToken; 3],
}

/// Forwards events as `M`, so each mode keeps its own message type.
pub struct Smtc<M> {
    manager: Manager,
    tx: Sender<M>,
    wrap: fn(Event) -> M,
    subscriptions: Vec<Subscription>,
}

impl<M: Send + 'static> Smtc<M> {
    pub fn new(tx: Sender<M>, wrap: fn(Event) -> M) -> Result<Self> {
        let manager = Manager::RequestAsync()?.get()?;
        fn changed<M: Send + 'static, A: windows::core::RuntimeType + 'static>(
            tx: Sender<M>,
            wrap: fn(Event) -> M,
        ) -> TypedEventHandler<Manager, A> {
            TypedEventHandler::new(move |_, _| {
                let _ = tx.send(wrap(Event::SessionsChanged));
                Ok(())
            })
        }
        manager.SessionsChanged(&changed(tx.clone(), wrap))?;
        manager.CurrentSessionChanged(&changed(tx.clone(), wrap))?;
        Ok(Self {
            manager,
            tx,
            wrap,
            subscriptions: Vec::new(),
        })
    }

    pub fn sessions(&self) -> Vec<Session> {
        let Ok(list) = self.manager.GetSessions() else {
            return Vec::new();
        };
        (0..list.Size().unwrap_or(0))
            .filter_map(|i| list.GetAt(i).ok())
            .collect()
    }

    pub fn current_app(&self) -> Option<String> {
        app_id(&self.manager.GetCurrentSession().ok()?).ok()
    }

    /// The subscribed session of `app`.
    pub fn find(&self, app: &str) -> Option<Session> {
        self.subscriptions
            .iter()
            .find(|sub| sub.app == app)
            .map(|sub| sub.session.clone())
    }

    /// Each subscription knows its app, so events need no WinRT call to say whose they are.
    fn handler<A>(&self, kind: Kind, app: &str) -> TypedEventHandler<Session, A>
    where
        A: windows::core::RuntimeType + 'static,
    {
        let (tx, wrap, app) = (self.tx.clone(), self.wrap, app.to_string());
        TypedEventHandler::new(move |_, _| {
            let _ = tx.send(wrap(Event::Session(kind, app.clone())));
            Ok(())
        })
    }

    /// Drop every subscription and subscribe to the sessions there are now.
    /// Returns each of them with the result of subscribing to it.
    pub fn resubscribe(&mut self) -> Vec<(String, Session, Result<()>)> {
        for sub in self.subscriptions.drain(..) {
            let [t, p, m] = sub.tokens;
            let _ = sub.session.RemoveTimelinePropertiesChanged(t);
            let _ = sub.session.RemovePlaybackInfoChanged(p);
            let _ = sub.session.RemoveMediaPropertiesChanged(m);
        }
        let mut sessions = Vec::new();
        for session in self.sessions() {
            let Ok(app) = app_id(&session) else { continue };
            let tokens = (|| -> Result<[EventRegistrationToken; 3]> {
                Ok([
                    session.TimelinePropertiesChanged(&self.handler(Kind::Timeline, &app))?,
                    session.PlaybackInfoChanged(&self.handler(Kind::Playback, &app))?,
                    session.MediaPropertiesChanged(&self.handler(Kind::Media, &app))?,
                ])
            })();
            let result = tokens.map(|tokens| {
                self.subscriptions.push(Subscription {
                    app: app.clone(),
                    session: session.clone(),
                    tokens,
                })
            });
            sessions.push((app, session, result));
        }
        sessions
    }
}
