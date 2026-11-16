//! `newcut run`: a supervisor on the main thread that starts the services
//! (`scanner`, `resolver`, `discography`, `fresh`, `notifier`, `importer`, `server`),
//! picks up config changes, restarts a service that stops unexpectedly, and
//! stops everything on SIGINT/SIGTERM.

pub mod schedule;
pub mod service;

use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::bail;
use jiff::Timestamp;
use tokio::sync::watch;

use crate::app::{App, exit};
use crate::config::store::{self as revisions, FileCheck};
use crate::lock::{self, LockError};
use crate::state::State;
use crate::state::daemon as store;
use service::{Handle, Service, Signals, Wakes};

/// How often the supervisor looks for config changes and requests.
const POLL: Duration = Duration::from_secs(2);
/// How long services get to wind down.
const GRACE: Duration = Duration::from_secs(20);

/// One service and how it's been doing.
struct Slot {
    service: Service,
    handle: Option<Handle>,
    /// Unexpected stops in a row.
    failures: u32,
    /// When to start it (again).
    restart_at: Option<Instant>,
    /// Asked to stop, and why.
    stopping: Option<Stopping>,
    /// Whether the config wants it (`server.enabled`, …).
    enabled: bool,
}

/// Why the supervisor stopped a service.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Stopping {
    Pause,
    Restart,
}

impl Slot {
    fn all(config: &crate::config::Config) -> Vec<Self> {
        Service::ALL
            .into_iter()
            .map(|service| Slot {
                service,
                handle: None,
                failures: 0,
                restart_at: Some(Instant::now()),
                stopping: None,
                enabled: service::enabled(service, config),
            })
            .collect()
    }

    /// Ask it to stop what it's doing and end (`why` decides what's next).
    fn stop(&mut self, why: Stopping) {
        if let Some(handle) = &self.handle
            && self.stopping.is_none()
        {
            handle.shutdown.stop(143);
            self.stopping = Some(why);
        }
    }
}

pub(crate) async fn run(app: &Arc<App>) -> anyhow::Result<i32> {
    let _daemon = match lock::acquire(&app.paths.scheduler_lock) {
        Ok(held) => held,
        Err(LockError::Held { pid }) => bail!(
            "another newcut daemon is running{}",
            pid.map_or_default(|p| format!(" (pid {p})"))
        ),
        Err(e) => return Err(e.into()),
    };
    crate::events::enable();
    let state = app.open_state()?;
    let (mut revision, loaded) = app.import_config(&state)?;
    for warning in &loaded.warnings {
        tracing::warn!("{warning}");
    }
    let mut config = loaded.config;
    let (config_tx, config_rx) = watch::channel(revision);
    let signals = Signals {
        wakes: Arc::new(Wakes::default()),
        config: config_rx,
    };
    let mut slots = Slot::all(&config);
    ignore_old_requests(&state)?;
    tracing::info!(
        "newcut daemon started (pid {}), with {} {}",
        std::process::id(),
        loaded.libraries.len(),
        if loaded.libraries.len() == 1 {
            "library"
        } else {
            "libraries"
        }
    );

    let mut poll = tokio::time::interval(POLL);
    let mut told_invalid: Option<Vec<String>> = None;
    loop {
        tokio::select! {
            _ = poll.tick() => {}
            () = app.shutdown.stopping() => break,
        }
        let now = Timestamp::now();
        // The config file changed: a new revision, if it checks out.
        match revisions::import_file(&state.conn, &app.config_path, &app.dirs.home) {
            Ok(FileCheck::Invalid(errors)) => {
                if told_invalid.as_ref() != Some(&errors) {
                    tracing::warn!(
                        "the config file has problems; keeping revision {revision}:\n{}",
                        errors.join("\n")
                    );
                    told_invalid = Some(errors);
                }
            }
            Ok(FileCheck::New(_) | FileCheck::Same(_)) => told_invalid = None,
            Err(e) => tracing::debug!("{e:#}"),
        }
        if let Some(rev) = revisions::current(&state.conn)?.map(|r| r.rev)
            && rev != revision
        {
            revision = rev;
            let (_, loaded) = revisions::load_current(&state.conn, &app.dirs.home)?;
            tracing::info!("applying config revision {rev}");
            for slot in &mut slots {
                slot.enabled = service::enabled(slot.service, &loaded.config);
            }
            // A new address or token: the web UI starts over.
            if loaded.config.server != config.server {
                for slot in slots.iter_mut().filter(|s| s.service == Service::Server) {
                    slot.stop(Stopping::Restart);
                }
            }
            config = loaded.config;
            config_tx.send_replace(rev);
            crate::events::emit(crate::events::Event::Changed);
        }
        for request in store::waiting_requests(&state.conn)? {
            let answer = handle_request(&request, &mut slots, &signals, &state)?;
            tracing::info!("{answer}");
            store::answer(&state.conn, request.id, &answer, now)?;
        }
        for slot in &mut slots {
            tend(app, &state, slot, &signals, now)?;
        }
    }

    stop_all(&mut slots, &state).await?;
    tracing::info!("newcut daemon stopped");
    Ok(exit::OK)
}

/// One supervisor round for a service: pause or resume it, deal with it
/// having stopped, and start it when it's due.
fn tend(
    app: &Arc<App>,
    state: &State,
    slot: &mut Slot,
    signals: &Signals,
    now: Timestamp,
) -> anyhow::Result<()> {
    let name = slot.service.name();
    let paused = store::service(&state.conn, name)?.paused || !slot.enabled;
    let idle_status = if slot.enabled { "paused" } else { "off" };
    if paused {
        slot.restart_at = None;
        if slot.handle.is_some() && slot.stopping.is_none() {
            tracing::info!("pausing {name}");
            slot.stop(Stopping::Pause);
        }
    } else if slot.handle.is_none() && slot.restart_at.is_none() {
        tracing::info!("resuming {name}");
        slot.restart_at = Some(Instant::now());
    }
    if slot.handle.as_ref().is_some_and(Handle::is_finished) {
        let handle = slot.handle.take().expect("checked");
        let ran = handle.started.elapsed();
        let result = handle.join();
        match slot.stopping.take() {
            Some(Stopping::Pause) => {
                store::set_status(&state.conn, name, idle_status, None, now)?;
                tracing::info!("{name} is {idle_status}");
                crate::events::emit(crate::events::Event::Changed);
                return Ok(());
            }
            Some(Stopping::Restart) => slot.restart_at = Some(Instant::now()),
            None if app.shutdown.is_stopping() => return Ok(()),
            None => {
                let why = match result {
                    Err(e) => e,
                    Ok(()) => "it stopped by itself".into(),
                };
                slot.failures = if ran >= schedule::HEALTHY_AFTER {
                    1
                } else {
                    slot.failures + 1
                };
                let delay = schedule::restart_delay(slot.failures);
                tracing::error!(
                    "{name} stopped unexpectedly ({why}); restarting it in {}",
                    crate::config::duration::format(delay)
                );
                store::failed(&state.conn, name, &why, now)?;
                crate::events::emit(crate::events::Event::Changed);
                slot.restart_at = Some(Instant::now() + delay);
            }
        }
    }
    if paused && slot.handle.is_none() && slot.stopping.is_none() {
        let row = store::service(&state.conn, name)?;
        if row.status.as_deref() != Some(idle_status) {
            store::set_status(&state.conn, name, idle_status, None, now)?;
        }
    }
    if slot.handle.is_none() && slot.restart_at.is_some_and(|at| at <= Instant::now()) {
        slot.restart_at = None;
        store::started(&state.conn, name, now)?;
        let handle = service::spawn(
            slot.service,
            app.clone(),
            app.shutdown.child(),
            signals.clone(),
        )?;
        slot.handle = Some(handle);
        crate::events::emit(crate::events::Event::Changed);
    }
    if slot.failures > 0
        && slot
            .handle
            .as_ref()
            .is_some_and(|h| h.started.elapsed() >= schedule::HEALTHY_AFTER)
    {
        slot.failures = 0;
    }
    Ok(())
}

/// Stop every service, giving each a while to wind down.
async fn stop_all(slots: &mut [Slot], state: &State) -> anyhow::Result<()> {
    for slot in slots.iter_mut() {
        slot.stop(Stopping::Restart);
    }
    for slot in slots {
        if let Some(handle) = slot.handle.take() {
            let name = slot.service.name();
            match tokio::time::timeout(GRACE, tokio::task::spawn_blocking(|| handle.join())).await {
                Ok(Ok(Err(e))) => tracing::warn!("{name}: {e}"),
                Ok(_) => {}
                Err(_) => tracing::warn!("{name} didn't stop in time"),
            }
        }
        store::set_status(
            &state.conn,
            slot.service.name(),
            "stopped",
            None,
            Timestamp::now(),
        )?;
    }
    Ok(())
}

/// Answer the requests made while no daemon was running.
fn ignore_old_requests(state: &State) -> anyhow::Result<()> {
    for request in store::waiting_requests(&state.conn)? {
        store::answer(
            &state.conn,
            request.id,
            "ignored: the daemon wasn't running",
            Timestamp::now(),
        )?;
    }
    Ok(())
}

/// Do what `newcut daemon …`, `newcut scan` or `newcut sync` asked;
/// returns the answer.
fn handle_request(
    request: &store::Request,
    slots: &mut [Slot],
    signals: &Signals,
    state: &State,
) -> anyhow::Result<String> {
    let targets: Vec<Service> = match request.service.as_deref() {
        None => Service::ALL.to_vec(),
        Some(name) => match Service::parse(name) {
            Some(service) => vec![service],
            None => return Ok(format!("there's no service {name:?}")),
        },
    };
    match request.action.as_str() {
        "run" => {
            let mut woken = Vec::new();
            let mut paused = Vec::new();
            for service in targets.into_iter().filter(|s| *s != Service::Server) {
                if store::service(&state.conn, service.name())?.paused
                    || slots.iter().any(|s| s.service == service && !s.enabled)
                {
                    paused.push(service.name());
                } else {
                    signals.wakes.wake(service);
                    woken.push(service.name());
                }
            }
            let mut parts = Vec::new();
            if !woken.is_empty() {
                parts.push(format!("{} will run now", woken.join(", ")));
            }
            if !paused.is_empty() {
                parts.push(format!("{} paused or off", paused.join(", ")));
            }
            Ok(parts.join("; "))
        }
        "restart" => {
            let mut restarted = Vec::new();
            for slot in slots.iter_mut().filter(|s| targets.contains(&s.service)) {
                if slot.handle.is_some() {
                    slot.stop(Stopping::Restart);
                    restarted.push(slot.service.name());
                } else if slot.enabled && !store::service(&state.conn, slot.service.name())?.paused
                {
                    slot.restart_at = Some(Instant::now());
                    restarted.push(slot.service.name());
                }
            }
            Ok(if restarted.is_empty() {
                "nothing to restart (paused or off)".into()
            } else {
                format!("restarting {}", restarted.join(", "))
            })
        }
        other => Ok(format!("unknown request {other:?}")),
    }
}
