//! The daemon's services. Each runs on its own thread, with its own tokio
//! runtime and its own database connection, so one can stop (or crash) and
//! start again without the others noticing.

use std::sync::Arc;
use std::time::{Duration, Instant};

use jiff::Timestamp;
use tokio::sync::{Notify, watch};
use tracing::Instrument;

use super::schedule;
use crate::app::App;
use crate::events::Reporter;
use crate::http::Http;
use crate::shutdown::Shutdown;
use crate::state::daemon as store;
use crate::work::{Which, Work};

/// The parts of the daemon that can be paused and restarted.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Service {
    /// Scans the libraries on the schedule.
    Scanner,
    /// Looks up release groups and artists the tags don't fully describe.
    Resolver,
    /// Checks each artist's discography on MusicBrainz.
    Discography,
    /// Checks ListenBrainz's fresh releases.
    Fresh,
    /// Sends notifications of new releases.
    Notifier,
    /// Tags and files finished slskd downloads.
    Importer,
    /// Looks for better copies of albums below `upgrade.target`.
    Upgrader,
    /// The web UI (when `server.enabled`).
    Server,
}

impl Service {
    pub const ALL: [Service; 8] = [
        Service::Scanner,
        Service::Resolver,
        Service::Discography,
        Service::Fresh,
        Service::Notifier,
        Service::Importer,
        Service::Upgrader,
        Service::Server,
    ];

    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Service::Scanner => "scanner",
            Service::Resolver => "resolver",
            Service::Discography => "discography",
            Service::Fresh => "fresh",
            Service::Notifier => "notifier",
            Service::Importer => "importer",
            Service::Upgrader => "upgrader",
            Service::Server => "server",
        }
    }

    #[must_use]
    pub fn describe(self) -> &'static str {
        match self {
            Service::Scanner => "Scans your libraries",
            Service::Resolver => "Looks up releases and artists on MusicBrainz",
            Service::Discography => "Checks discographies on MusicBrainz",
            Service::Fresh => "Checks ListenBrainz's fresh releases",
            Service::Notifier => "Sends notifications of new releases",
            Service::Importer => "Tags and files finished slskd downloads",
            Service::Upgrader => "Looks for better copies of your albums",
            Service::Server => "Serves this web UI",
        }
    }

    #[must_use]
    pub fn parse(name: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|s| s.name() == name)
    }
}

/// Wake a service before its schedule says so.
#[derive(Debug, Default)]
pub struct Wakes {
    scanner: Notify,
    resolver: Notify,
    discography: Notify,
    fresh: Notify,
    notifier: Notify,
    importer: Notify,
    upgrader: Notify,
    server: Notify,
}

impl Wakes {
    #[must_use]
    pub fn get(&self, service: Service) -> &Notify {
        match service {
            Service::Scanner => &self.scanner,
            Service::Resolver => &self.resolver,
            Service::Discography => &self.discography,
            Service::Fresh => &self.fresh,
            Service::Notifier => &self.notifier,
            Service::Importer => &self.importer,
            Service::Upgrader => &self.upgrader,
            Service::Server => &self.server,
        }
    }

    /// Wake `service` (it does its work as soon as it's free).
    pub fn wake(&self, service: Service) {
        self.get(service).notify_one();
    }
}

/// What the supervisor shares with every service.
#[derive(Clone)]
pub struct Signals {
    pub wakes: Arc<Wakes>,
    /// The current config revision; changes when a new one is stored.
    pub config: watch::Receiver<i64>,
}

/// A running service.
pub struct Handle {
    pub service: Service,
    pub shutdown: Shutdown,
    pub started: Instant,
    thread: std::thread::JoinHandle<Result<(), String>>,
}

impl Handle {
    #[must_use]
    pub fn is_finished(&self) -> bool {
        self.thread.is_finished()
    }

    /// Wait for it to end: `Err` says why it ended on its own (an error, or
    /// a panic).
    ///
    /// # Errors
    ///
    /// Returns the service's own error, or a "crashed" message if its thread
    /// panicked.
    pub fn join(self) -> Result<(), String> {
        match self.thread.join() {
            Ok(result) => result,
            Err(panic) => Err(match panic.downcast::<String>() {
                Ok(message) => format!("crashed: {message}"),
                Err(panic) => match panic.downcast::<&str>() {
                    Ok(message) => format!("crashed: {message}"),
                    Err(_) => "crashed".into(),
                },
            }),
        }
    }
}

/// Start `service` on its own thread.
///
/// # Errors
///
/// Fails if the OS can't spawn the thread.
pub fn spawn(
    service: Service,
    app: Arc<App>,
    shutdown: Shutdown,
    signals: Signals,
) -> std::io::Result<Handle> {
    let stop = shutdown.clone();
    let thread = std::thread::Builder::new()
        .name(format!("newcut-{}", service.name()))
        .spawn(move || {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .map_err(|e| format!("cannot start: {e}"))?;
            let span = tracing::info_span!("service", name = service.name());
            runtime
                .block_on(
                    async {
                        let app = app.with_shutdown(stop);
                        test_panic(service);
                        match service {
                            Service::Server => server(&app, signals).await,
                            work => worker(&app, work, signals).await,
                        }
                    }
                    .instrument(span),
                )
                .map_err(|e| format!("{e:#}"))
        })?;
    Ok(Handle {
        service,
        shutdown,
        started: Instant::now(),
        thread,
    })
}

/// Debug builds: `NEWCUT_TEST_PANIC=<service>` crashes it once, to test
/// restarts.
fn test_panic(service: Service) {
    #[cfg(debug_assertions)]
    {
        static PANICKED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
        if std::env::var("NEWCUT_TEST_PANIC").ok().as_deref() == Some(service.name())
            && !PANICKED.swap(true, std::sync::atomic::Ordering::SeqCst)
        {
            panic!("NEWCUT_TEST_PANIC");
        }
    }
    #[cfg(not(debug_assertions))]
    let _ = service;
}

/// A time as pages show it: local, to the minute.
#[must_use]
pub fn local(t: Timestamp) -> String {
    t.to_zoned(jiff::tz::TimeZone::system())
        .strftime("%Y-%m-%d %H:%M")
        .to_string()
}

/// Whether a service is switched off by the config.
#[must_use]
pub fn enabled(service: Service, config: &crate::config::Config) -> bool {
    match service {
        Service::Server => config.server.enabled,
        Service::Fresh => config.listenbrainz.enabled,
        Service::Notifier => !config.notification.channels().is_empty(),
        Service::Importer => {
            config.slskd.enabled
                && config.slskd.downloads_dir.is_some()
                && config.slskd.output_dir.is_some()
        }
        Service::Upgrader => config.upgrade.enabled && enabled(Service::Importer, config),
        Service::Scanner | Service::Resolver | Service::Discography => true,
    }
}

/// Do a service's work when it's due (or when woken), until stopped.
async fn worker(app: &App, service: Service, mut signals: Signals) -> anyhow::Result<()> {
    let name = service.name();
    let report = Reporter { service: name };
    let wake = signals.wakes.clone();
    loop {
        if app.shutdown.is_stopping() {
            return Ok(());
        }
        let mut state = app.open_state()?;
        let (_, loaded) = crate::config::store::load_current(&state.conn, &app.dirs.home)?;
        let http = Http::new(loaded.config.musicbrainz.contact.as_deref());
        let now = Timestamp::now();
        let (due, wait) = due_now(&state, service, &loaded, now)?;
        if due {
            store::set_status(&state.conn, name, "running", None, now)?;
            crate::events::emit(crate::events::Event::Changed);
            let shutdown = app.shutdown.clone();
            let mut work = Work {
                state: &mut state,
                loaded: &loaded,
                http: &http,
                report: &report,
                shutdown: &shutdown,
            };
            let result = run(&mut work, service, &wake).await?;
            let summary = match result {
                Ok(summary) => summary,
                Err(e) => {
                    tracing::warn!("{e:#}");
                    format!("failed: {e:#}")
                }
            };
            if !summary.starts_with("checked 0 artists")
                && summary != "looked up 0"
                && !summary.starts_with("nothing filed")
            {
                tracing::info!("{summary}");
            }
            report.done(summary.clone());
            store::set_status(&state.conn, name, "idle", Some(&summary), Timestamp::now())?;
            crate::events::emit(crate::events::Event::Changed);
        }
        drop(state);
        let wait = if due
            && matches!(
                service,
                Service::Scanner | Service::Fresh | Service::Notifier | Service::Upgrader
            ) {
            // Just did it: the next is a full interval away.
            wait.max(schedule::CHECK_EVERY)
        } else {
            wait
        };
        tokio::select! {
            () = app.shutdown.stopping() => return Ok(()),
            () = tokio::time::sleep(wait.min(schedule::CHECK_EVERY)) => {}
            () = wake.get(service).notified() => {
                if let Some(key) = last_key(service) {
                    // Asked: due right away.
                    let state = app.open_state()?;
                    store::set(&state.conn, key, None)?;
                }
            }
            _ = signals.config.changed() => {}
        }
    }
}

/// Do `service`'s work once; `Err` only for what stops the service (its
/// own failures are in the `Ok` result, to report).
async fn run(
    work: &mut Work<'_>,
    service: Service,
    wake: &Arc<Wakes>,
) -> anyhow::Result<anyhow::Result<String>> {
    Ok(match service {
        Service::Scanner => {
            let result = work.scan().map(|r| r.summary());
            if result.is_ok() && !work.shutdown.is_stopping() {
                store::set_time(&work.state.conn, store::LAST_SCAN, Timestamp::now())?;
                wake.wake(Service::Resolver);
            }
            result
        }
        Service::Resolver => {
            let result = work.resolve().await;
            // New artists: their discographies are due.
            wake.wake(Service::Discography);
            result
        }
        Service::Discography => work.discographies(&Which::Due).await,
        Service::Fresh => {
            let result = work.fresh().await;
            if result.is_ok() {
                store::set_time(&work.state.conn, store::LAST_FRESH, Timestamp::now())?;
            }
            result
        }
        Service::Notifier => {
            let result = work.notify().await;
            if result.is_ok() {
                store::set_time(&work.state.conn, store::LAST_NOTIFY, Timestamp::now())?;
            }
            result
        }
        Service::Importer => {
            let result = work.import().await;
            if matches!(result, Ok((_, filed)) if filed > 0) {
                // Its folder's scanned already; new artists get looked up.
                wake.wake(Service::Resolver);
            }
            result.map(|(summary, _)| summary)
        }
        Service::Upgrader => {
            let result = work.upgrade().await;
            if result.is_ok() && !work.shutdown.is_stopping() {
                store::set_time(&work.state.conn, store::LAST_UPGRADE, Timestamp::now())?;
                // Queued: the importer follows them.
                wake.wake(Service::Importer);
            }
            result
        }
        Service::Server => unreachable!("the server is no worker"),
    })
}

/// Where a periodic service keeps when it last did its work.
fn last_key(service: Service) -> Option<&'static str> {
    match service {
        Service::Scanner => Some(store::LAST_SCAN),
        Service::Fresh => Some(store::LAST_FRESH),
        Service::Notifier => Some(store::LAST_NOTIFY),
        Service::Upgrader => Some(store::LAST_UPGRADE),
        _ => None,
    }
}

/// Whether `service` has work now, and how long until it might.
fn due_now(
    state: &crate::state::State,
    service: Service,
    loaded: &crate::config::Loaded,
    now: Timestamp,
) -> anyhow::Result<(bool, Duration)> {
    Ok(match service {
        Service::Scanner => periodic(state, store::LAST_SCAN, loaded.config.schedule.scan.0, now)?,
        Service::Fresh => {
            let (due, wait) = periodic(
                state,
                store::LAST_FRESH,
                loaded.config.schedule.fresh.0,
                now,
            )?;
            // Not before there are artists to look for (the first scan
            // is still going): an empty check would count for the day.
            let artists = !crate::state::library::followed_ids(&state.conn)?.is_empty();
            (
                due && artists,
                if artists { wait } else { schedule::CHECK_EVERY },
            )
        }
        Service::Resolver => (
            !crate::state::library::releases_to_look_up(&state.conn)?.is_empty()
                || !crate::state::library::artists_to_resolve(&state.conn)?.is_empty(),
            schedule::CHECK_EVERY,
        ),
        Service::Notifier => periodic(
            state,
            store::LAST_NOTIFY,
            loaded.config.schedule.notification.0,
            now,
        )?,
        Service::Discography => (Work::any_due(state)?, schedule::CHECK_EVERY),
        Service::Importer => (
            loaded.import.is_some() && !crate::state::downloads::pending(&state.conn)?.is_empty(),
            schedule::CHECK_EVERY,
        ),
        Service::Upgrader => {
            let (due, wait) = periodic(
                state,
                store::LAST_UPGRADE,
                loaded.config.schedule.upgrade.0,
                now,
            )?;
            // Not before the scanner read what you have; one you asked for
            // right away.
            let scanned = store::get_time(&state.conn, store::LAST_SCAN)?.is_some();
            let asked = crate::state::upgrades::any_requested(&state.conn)?;
            (
                (due && scanned) || asked,
                if scanned { wait } else { schedule::CHECK_EVERY },
            )
        }
        Service::Server => unreachable!("the server is no worker"),
    })
}

/// For work done every `every`: whether it's due, and how long until it is.
fn periodic(
    state: &crate::state::State,
    key: &str,
    every: Duration,
    now: Timestamp,
) -> anyhow::Result<(bool, Duration)> {
    let Some(last) = store::get_time(&state.conn, key)? else {
        return Ok((true, Duration::ZERO));
    };
    let next = schedule::next_sync(last, every, 0.0);
    let wait = schedule::wait_until(next, now);
    Ok((wait.is_zero(), wait))
}

/// Serve the web UI until stopped.
async fn server(app: &App, signals: Signals) -> anyhow::Result<()> {
    let state = app.open_state()?;
    let (_, loaded) = crate::config::store::load_current(&state.conn, &app.dirs.home)?;
    drop(state);
    let addr = loaded.listen();
    let listener = tokio::net::TcpListener::bind(addr)
        .await
        .map_err(|e| anyhow::anyhow!("cannot listen on {addr}: {e}"))?;
    tracing::info!("web UI on http://{addr}");
    let web = Arc::new(crate::web::Web::new(
        app.paths.state_db.clone(),
        app.dirs.clone(),
        app.config_path.clone(),
        &loaded.config.server,
        addr,
        signals.wakes.clone(),
        app.shutdown.clone(),
    ));
    let state = app.open_state()?;
    store::set_status(
        &state.conn,
        Service::Server.name(),
        "running",
        Some(&format!("http://{addr}")),
        Timestamp::now(),
    )?;
    drop(state);
    crate::web::serve(web, listener, app.shutdown.clone()).await?;
    Ok(())
}
