//! What the daemon is doing right now, for the web UI's live panel and
//! `/api/v1/events`. Services report to a process-wide bus; when nothing
//! listens (one-shot commands), reporting does nothing.

use std::collections::{BTreeMap, VecDeque};
use std::sync::{Mutex, OnceLock};

use serde::Serialize;
use tokio::sync::broadcast;

/// Something that happened.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Event {
    /// A service is working on something.
    Working { service: String, activity: String },
    /// A service is done for now; `summary` says what it did.
    Done { service: String, summary: String },
    /// A release group not seen before.
    Found {
        artist: String,
        title: String,
        href: String,
    },
    /// A download's state changed (or its album was scanned once filed):
    /// `key` is its release group.
    Download { key: String, state: String },
    /// Something else changed (a service paused, a config revision, …).
    Changed,
}

/// A service at work.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Active {
    pub service: String,
    pub activity: String,
}

/// Recently found, or a service's last summary.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Recent {
    pub at: String,
    pub text: String,
    pub detail: String,
    pub href: Option<String>,
}

/// What's going on right now.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct Live {
    pub active: Vec<Active>,
    pub recent: Vec<Recent>,
}

const RECENT: usize = 12;

#[derive(Default)]
struct State {
    active: BTreeMap<String, String>,
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

/// Hear about events as they happen (`None` outside the daemon).
#[must_use]
pub fn subscribe() -> Option<broadcast::Receiver<Event>> {
    BUS.get().map(|bus| bus.sender.subscribe())
}

/// What's going on now.
#[must_use]
pub fn live() -> Live {
    let Some(bus) = BUS.get() else {
        return Live::default();
    };
    let state = lock(bus);
    Live {
        active: state
            .active
            .iter()
            .map(|(service, activity)| Active {
                service: service.clone(),
                activity: activity.clone(),
            })
            .collect(),
        recent: state.recent.iter().cloned().collect(),
    }
}

fn lock(bus: &Bus) -> std::sync::MutexGuard<'_, State> {
    bus.state
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// Report an event.
pub fn emit(event: Event) {
    let Some(bus) = BUS.get() else {
        return;
    };
    {
        let mut state = lock(bus);
        let now = jiff::Zoned::now().strftime("%H:%M").to_string();
        match &event {
            Event::Working { service, activity } => {
                state.active.insert(service.clone(), activity.clone());
            }
            Event::Done { service, summary } => {
                state.active.remove(service);
                remember(
                    &mut state,
                    Recent {
                        at: now,
                        text: service.clone(),
                        detail: summary.clone(),
                        href: None,
                    },
                );
            }
            Event::Found {
                artist,
                title,
                href,
            } => remember(
                &mut state,
                Recent {
                    at: now,
                    text: format!("New: {title}"),
                    detail: artist.clone(),
                    href: Some(href.clone()),
                },
            ),
            Event::Download { .. } | Event::Changed => {}
        }
    }
    let _ = bus.sender.send(event);
}

fn remember(state: &mut State, recent: Recent) {
    state.recent.push_front(recent);
    state.recent.truncate(RECENT);
}

/// A service's own reporter.
#[derive(Debug, Clone)]
pub struct Reporter {
    pub service: &'static str,
}

impl Reporter {
    pub fn working(&self, activity: impl Into<String>) {
        emit(Event::Working {
            service: self.service.into(),
            activity: activity.into(),
        });
    }

    pub fn done(&self, summary: impl Into<String>) {
        emit(Event::Done {
            service: self.service.into(),
            summary: summary.into(),
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_live_view_follows_events() {
        enable();
        let mut events = subscribe().unwrap();
        let scanner = Reporter {
            service: "test-scanner",
        };
        scanner.working("reading /music");
        assert!(live().active.iter().any(|a| a.service == "test-scanner"));
        scanner.done("10 files");
        assert!(!live().active.iter().any(|a| a.service == "test-scanner"));
        assert!(live().recent.iter().any(|r| r.detail == "10 files"));
        assert!(matches!(events.try_recv().unwrap(), Event::Working { .. }));
    }
}
