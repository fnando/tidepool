//! What the daemon is doing right now, for the web UI's live panel and
//! `/api/v1/events`. The work (syncing, downloading, finishing videos)
//! reports to a process-wide bus; when nothing listens (one-shot commands),
//! reporting does nothing.

use std::collections::{BTreeMap, VecDeque};
use std::sync::{Mutex, OnceLock};

use serde::Serialize;
use tokio::sync::broadcast;

/// Something that happened.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Event {
    SyncStarted,
    SyncFinished {
        summary: String,
    },
    /// A source is being checked.
    Checking {
        source: String,
    },
    /// A download started, or made progress.
    Downloading {
        source: String,
        video_id: String,
        title: String,
        percent: Option<u8>,
        size: Option<String>,
        speed: Option<String>,
        eta: Option<String>,
    },
    /// Downloaded; waiting to be finished.
    Fetched {
        source: String,
        video_id: String,
        title: String,
    },
    /// Being finished: `step` is a hook, embedding or saving.
    Finishing {
        source: String,
        video_id: String,
        title: String,
        step: String,
    },
    Saved {
        source: String,
        video_id: String,
        title: String,
    },
    Failed {
        source: String,
        video_id: String,
        title: String,
        error: String,
    },
    /// Put back unfinished (the daemon is stopping or the part paused).
    Stopped {
        source: String,
        video_id: String,
    },
    /// Something else changed (a part paused, a job done, …).
    Changed,
}

impl Event {
    /// Download progress: frequent, and only the live panel cares.
    pub fn is_progress(&self) -> bool {
        matches!(
            self,
            Event::Downloading {
                percent: Some(_),
                ..
            }
        )
    }
}

/// A video in flight.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Active {
    pub source: String,
    pub video_id: String,
    pub title: String,
    /// `downloading`, `waiting` (downloaded, to be finished) or `finishing`.
    pub state: String,
    pub percent: Option<u8>,
    pub detail: Option<String>,
}

/// Recently saved or failed.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Recent {
    pub at: String,
    pub source: String,
    pub video_id: String,
    pub title: String,
    pub error: Option<String>,
}

/// What's going on right now.
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct Live {
    pub syncing: bool,
    pub checking: Option<String>,
    pub active: Vec<Active>,
    pub recent: Vec<Recent>,
}

const RECENT: usize = 10;

#[derive(Default)]
struct State {
    syncing: bool,
    checking: Option<String>,
    active: BTreeMap<(String, String), Active>,
    recent: VecDeque<Recent>,
}

struct Bus {
    sender: broadcast::Sender<Event>,
    state: Mutex<State>,
}

static BUS: OnceLock<Bus> = OnceLock::new();

/// Start listening (the daemon does, once); reporting does nothing before.
pub fn enable() {
    BUS.get_or_init(|| Bus {
        sender: broadcast::channel(256).0,
        state: Mutex::default(),
    });
}

/// New events, from now on; `None` when the bus isn't on.
pub fn subscribe() -> Option<broadcast::Receiver<Event>> {
    BUS.get().map(|bus| bus.sender.subscribe())
}

/// What's going on right now.
pub fn live() -> Live {
    let Some(bus) = BUS.get() else {
        return Live::default();
    };
    let state = bus.state.lock().unwrap_or_else(|e| e.into_inner());
    Live {
        syncing: state.syncing,
        checking: state.checking.clone(),
        active: state.active.values().cloned().collect(),
        recent: state.recent.iter().cloned().collect(),
    }
}

/// Report `event`.
pub fn emit(event: Event) {
    let Some(bus) = BUS.get() else {
        return;
    };
    {
        let mut state = bus.state.lock().unwrap_or_else(|e| e.into_inner());
        apply(&mut state, &event);
    }
    let _ = bus.sender.send(event);
}

fn apply(state: &mut State, event: &Event) {
    let key = |s: &str, v: &str| (s.to_string(), v.to_string());
    let now = || {
        jiff::Timestamp::now()
            .to_zoned(jiff::tz::TimeZone::system())
            .strftime("%H:%M")
            .to_string()
    };
    match event {
        Event::SyncStarted => state.syncing = true,
        Event::SyncFinished { .. } => {
            state.syncing = false;
            state.checking = None;
        }
        Event::Checking { source } => state.checking = Some(source.clone()),
        Event::Downloading {
            source,
            video_id,
            title,
            percent,
            size,
            speed,
            eta,
        } => {
            state.checking = None;
            let detail = [
                size.clone(),
                speed.as_ref().map(|s| format!("{s}/s")),
                eta.as_ref().map(|e| format!("{e} left")),
            ]
            .into_iter()
            .flatten()
            .collect::<Vec<_>>()
            .join(" · ");
            state.active.insert(
                key(source, video_id),
                Active {
                    source: source.clone(),
                    video_id: video_id.clone(),
                    title: title.clone(),
                    state: "downloading".into(),
                    percent: *percent,
                    detail: (!detail.is_empty()).then_some(detail),
                },
            );
        }
        Event::Fetched {
            source,
            video_id,
            title,
        } => {
            state.active.insert(
                key(source, video_id),
                Active {
                    source: source.clone(),
                    video_id: video_id.clone(),
                    title: title.clone(),
                    state: "waiting".into(),
                    percent: None,
                    detail: None,
                },
            );
        }
        Event::Finishing {
            source,
            video_id,
            title,
            step,
        } => {
            state.active.insert(
                key(source, video_id),
                Active {
                    source: source.clone(),
                    video_id: video_id.clone(),
                    title: title.clone(),
                    state: "finishing".into(),
                    percent: None,
                    detail: Some(step.clone()),
                },
            );
        }
        Event::Saved {
            source,
            video_id,
            title,
        }
        | Event::Failed {
            source,
            video_id,
            title,
            ..
        } => {
            state.active.remove(&key(source, video_id));
            let error = match event {
                Event::Failed { error, .. } => Some(error.clone()),
                _ => None,
            };
            state.recent.push_front(Recent {
                at: now(),
                source: source.clone(),
                video_id: video_id.clone(),
                title: title.clone(),
                error,
            });
            state.recent.truncate(RECENT);
        }
        Event::Stopped { source, video_id } => {
            state.active.remove(&key(source, video_id));
        }
        Event::Changed => {}
    }
}

/// Throttles download progress to about once a second per video.
#[derive(Default)]
pub struct Throttle {
    last: Option<std::time::Instant>,
}

impl Throttle {
    pub fn ready(&mut self) -> bool {
        let now = std::time::Instant::now();
        if self
            .last
            .is_some_and(|last| now.duration_since(last) < std::time::Duration::from_secs(1))
        {
            return false;
        }
        self.last = Some(now);
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn video(id: &str) -> (String, String, String) {
        ("lofi".into(), id.into(), format!("Video {id}"))
    }

    #[test]
    fn tracks_videos_in_flight_and_recent_ones() {
        let mut state = State::default();
        let (s, v, t) = video("a");
        apply(&mut state, &Event::SyncStarted);
        apply(&mut state, &Event::Checking { source: s.clone() });
        apply(
            &mut state,
            &Event::Downloading {
                source: s.clone(),
                video_id: v.clone(),
                title: t.clone(),
                percent: Some(40),
                size: Some("10 MB".into()),
                speed: Some("2 MB".into()),
                eta: Some("0:03".into()),
            },
        );
        let active = &state.active[&(s.clone(), v.clone())];
        assert_eq!(active.detail.as_deref(), Some("10 MB · 2 MB/s · 0:03 left"));
        assert_eq!(state.checking, None, "downloading now");
        apply(
            &mut state,
            &Event::Finishing {
                source: s.clone(),
                video_id: v.clone(),
                title: t.clone(),
                step: "hook notify".into(),
            },
        );
        assert_eq!(state.active[&(s.clone(), v.clone())].state, "finishing");
        apply(
            &mut state,
            &Event::Failed {
                source: s.clone(),
                video_id: v.clone(),
                title: t,
                error: "boom".into(),
            },
        );
        assert!(state.active.is_empty());
        assert_eq!(state.recent[0].error.as_deref(), Some("boom"));
        apply(
            &mut state,
            &Event::SyncFinished {
                summary: String::new(),
            },
        );
        assert!(!state.syncing);
    }

    #[test]
    fn progress_is_throttled() {
        let mut throttle = Throttle::default();
        assert!(throttle.ready());
        assert!(!throttle.ready(), "within a second");
    }
}
