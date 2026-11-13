//! Running a command: where things are, the state DB, the config, and each
//! subcommand.

use std::io::Write;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use jiff::Timestamp;

use crate::catalog::{self, Entry, Status, Today};
use crate::cli::{Cli, Command, ConfigCmd, DaemonCmd, ListArgs};
use crate::config::store::{self as revisions, FileCheck};
use crate::config::{self, Loaded};
use crate::daemon::service::Service;
use crate::events::Reporter;
use crate::http::Http;
use crate::paths::{Dirs, StatePaths};
use crate::shutdown::Shutdown;
use crate::state::State;
use crate::state::daemon as store;
use crate::state::library;
use crate::work::{Which, Work};

/// Exit codes.
pub mod exit {
    pub const OK: i32 = 0;
    /// Something failed.
    pub const FATAL: i32 = 1;
    /// The command line or config is wrong.
    pub const USAGE: i32 = 2;
}

/// A mistake in how newcut was asked to run (exit code 2).
#[derive(Debug, thiserror::Error)]
#[error("{0}")]
pub struct UsageError(pub String);

fn usage(message: impl Into<String>) -> anyhow::Error {
    UsageError(message.into()).into()
}

/// What every command shares.
#[derive(Debug, Clone)]
pub struct App {
    pub dirs: Dirs,
    pub paths: StatePaths,
    pub config_path: PathBuf,
    /// The env file: `--env-file`, or the default one.
    pub env_path: PathBuf,
    pub shutdown: Shutdown,
}

impl App {
    /// # Errors
    ///
    /// When the home directory can't be found.
    pub fn new(cli: &Cli, shutdown: Shutdown) -> anyhow::Result<Self> {
        let dirs = Dirs::from_env().map_err(usage)?;
        let config_path = cli
            .config
            .clone()
            .unwrap_or_else(|| dirs.default_config_file());
        let env_path = cli
            .env_file
            .clone()
            .unwrap_or_else(|| dirs.config_dir.join("env"));
        Ok(Self {
            paths: dirs.state(),
            dirs,
            config_path,
            env_path,
            shutdown,
        })
    }

    /// The same, with its own shutdown (a service's).
    #[must_use]
    pub fn with_shutdown(&self, shutdown: Shutdown) -> Self {
        Self {
            shutdown,
            ..self.clone()
        }
    }

    /// Open the state DB.
    ///
    /// # Errors
    ///
    /// When it can't be opened or migrated.
    pub fn open_state(&self) -> anyhow::Result<State> {
        Ok(State::open(&self.paths.state_db)?)
    }

    /// Store the config file as the latest revision (when it changed) and
    /// return what to run with.
    ///
    /// # Errors
    ///
    /// When the file is missing or has problems.
    pub fn import_config(&self, state: &State) -> anyhow::Result<(i64, Loaded)> {
        match revisions::import_file(&state.conn, &self.config_path, &self.dirs.home) {
            Ok(FileCheck::Invalid(errors)) => return Err(usage(errors.join("\n"))),
            Ok(_) => {}
            Err(e) => {
                if !self.config_path.exists() {
                    return Err(usage(format!(
                        "there's no config at {}; create one with `newcut init`",
                        self.config_path.display()
                    )));
                }
                return Err(e);
            }
        }
        revisions::load_current(&state.conn, &self.dirs.home)
    }

    /// Whether a daemon is running (it holds the scheduler lock).
    #[must_use]
    pub fn daemon_running(&self) -> bool {
        matches!(
            crate::lock::acquire(&self.paths.scheduler_lock),
            Err(crate::lock::LockError::Held { .. })
        )
    }
}

/// Run the command; returns the exit code.
///
/// # Errors
///
/// When the command fails ([`UsageError`] for exit code 2).
pub async fn run(cli: &Cli, out: &mut dyn Write) -> anyhow::Result<i32> {
    let app = Arc::new(App::new(cli, Shutdown::listen())?);
    match &cli.command {
        Command::Init(args) => init(&app, &args.libraries, args.force, out),
        Command::Run => {
            if app.env_path.is_file() {
                tracing::info!("read {}", app.env_path.display());
            }
            if let Some(warning) = crate::env_file::readable(&app.env_path) {
                tracing::warn!("{warning}");
            }
            crate::daemon::run(&app).await
        }
        Command::Scan => scan(&app, out).await,
        Command::Sync(args) => sync(&app, args.artist.as_deref(), out).await,
        Command::Notify(args) => notify(&app, args.test, out).await,
        Command::Missing(args) => list(&app, args, false, out),
        Command::New(args) => list(&app, args, true, out),
        Command::Artists => artists(&app, out),
        Command::Ignore(args) => ignore(&app, &args.release_groups, true, out),
        Command::Unignore(args) => ignore(&app, &args.release_groups, false, out),
        Command::Config { cmd } => config_cmd(&app, cmd, out),
        Command::Daemon { cmd } => daemon_cmd(&app, cmd, out).await,
    }
}

fn init(app: &App, libraries: &[String], force: bool, out: &mut dyn Write) -> anyhow::Result<i32> {
    let path = &app.config_path;
    if path.exists() && !force {
        return Err(usage(format!(
            "{} exists already; pass --force to replace it",
            path.display()
        )));
    }
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    std::fs::write(path, config::starter(libraries))?;
    writeln!(out, "Wrote {}", path.display())?;
    if !app.env_path.exists() {
        use std::os::unix::fs::OpenOptionsExt;
        if let Some(dir) = app.env_path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&app.env_path)?
            .write_all(crate::env_file::STARTER.as_bytes())?;
        writeln!(out, "Wrote {} (for secrets)", app.env_path.display())?;
    }
    writeln!(
        out,
        "Next: check the libraries in it, then run `newcut scan` (once) or `newcut run` (the daemon and web UI)."
    )?;
    Ok(exit::OK)
}

/// Ask the running daemon to run `services` now; prints its answer.
async fn ask_daemon(
    state: &State,
    services: &[Service],
    out: &mut dyn Write,
) -> anyhow::Result<i32> {
    for service in services {
        let id = store::request(&state.conn, Some(service.name()), "run", Timestamp::now())?;
        let mut answer = None;
        for _ in 0..50 {
            answer = store::answer_of(&state.conn, id)?;
            if answer.is_some() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(200)).await;
        }
        match answer {
            Some(answer) => writeln!(out, "The daemon says: {answer}")?,
            None => writeln!(out, "Asked the daemon; it hasn't answered yet")?,
        }
    }
    Ok(exit::OK)
}

async fn scan(app: &App, out: &mut dyn Write) -> anyhow::Result<i32> {
    let mut state = app.open_state()?;
    if app.daemon_running() {
        return ask_daemon(&state, &[Service::Scanner], out).await;
    }
    let (_, loaded) = app.import_config(&state)?;
    let http = Http::new(loaded.config.musicbrainz.contact.as_deref());
    let report = Reporter { service: "scanner" };
    let mut work = Work {
        state: &mut state,
        loaded: &loaded,
        http: &http,
        report: &report,
        shutdown: &app.shutdown,
    };
    let scanned = work.scan()?;
    writeln!(out, "Scanned: {}", scanned.summary())?;
    store::set_time(&work.state.conn, store::LAST_SCAN, Timestamp::now())?;
    let resolved = work.resolve().await?;
    writeln!(out, "MusicBrainz: {resolved}")?;
    Ok(exit::OK)
}

async fn sync(app: &App, artist: Option<&str>, out: &mut dyn Write) -> anyhow::Result<i32> {
    let mut state = app.open_state()?;
    let only = match artist {
        Some(reference) => Some(find_artist(&state, reference)?),
        None => None,
    };
    if let Some(mbid) = &only {
        library::check_now(&state.conn, mbid)?;
    }
    if app.daemon_running() {
        let mut services = vec![Service::Discography];
        if only.is_none() {
            services.insert(0, Service::Resolver);
            services.push(Service::Fresh);
        }
        return ask_daemon(&state, &services, out).await;
    }
    let (_, loaded) = app.import_config(&state)?;
    let http = Http::new(loaded.config.musicbrainz.contact.as_deref());
    let report = Reporter { service: "sync" };
    let mut work = Work {
        state: &mut state,
        loaded: &loaded,
        http: &http,
        report: &report,
        shutdown: &app.shutdown,
    };
    let which = only.map_or(Which::Due, |mbid| Which::Only(vec![mbid]));
    if which == Which::Due {
        writeln!(out, "MusicBrainz lookups: {}", work.resolve().await?)?;
    }
    writeln!(out, "Discographies: {}", work.discographies(&which).await?)?;
    if which == Which::Due && loaded.config.listenbrainz.enabled {
        match work.fresh().await {
            Ok(summary) => {
                writeln!(out, "Fresh releases: {summary}")?;
                store::set_time(&work.state.conn, store::LAST_FRESH, Timestamp::now())?;
            }
            Err(e) => writeln!(out, "Fresh releases: failed: {e:#}")?,
        }
    }
    Ok(exit::OK)
}

async fn notify(app: &App, test: bool, out: &mut dyn Write) -> anyhow::Result<i32> {
    let mut state = app.open_state()?;
    if !test && app.daemon_running() {
        return ask_daemon(&state, &[Service::Notifier], out).await;
    }
    let (_, loaded) = app.import_config(&state)?;
    let channels = loaded.config.notification.channels();
    if channels.is_empty() {
        return Err(usage(
            "no notification channel is on: set up [notification.pushover] or [notification.command]",
        ));
    }
    let http = Http::new(loaded.config.musicbrainz.contact.as_deref());
    let report = Reporter {
        service: "notifier",
    };
    let mut work = Work {
        state: &mut state,
        loaded: &loaded,
        http: &http,
        report: &report,
        shutdown: &app.shutdown,
    };
    if !test {
        writeln!(out, "{}", work.notify().await?)?;
        store::set_time(&work.state.conn, store::LAST_NOTIFY, Timestamp::now())?;
        return Ok(exit::OK);
    }
    let message = crate::notify::test_message();
    let mut failed = false;
    for channel in channels {
        match work.send(channel, &message).await {
            Ok(()) => writeln!(out, "{channel}: sent")?,
            Err(e) => {
                failed = true;
                writeln!(out, "{channel}: failed: {e:#}")?;
            }
        }
    }
    Ok(if failed { exit::FATAL } else { exit::OK })
}

/// An artist by MusicBrainz id or (unique) name.
fn find_artist(state: &State, reference: &str) -> anyhow::Result<String> {
    let artists = library::artists(&state.conn)?;
    if let Some(a) = artists
        .iter()
        .find(|a| a.mbid == reference.to_ascii_lowercase())
    {
        return Ok(a.mbid.clone());
    }
    let matches: Vec<_> = artists
        .iter()
        .filter(|a| a.name.eq_ignore_ascii_case(reference))
        .collect();
    match matches.as_slice() {
        [one] => Ok(one.mbid.clone()),
        [] => Err(usage(format!("no artist {reference:?} in your libraries"))),
        many => Err(usage(format!(
            "{} artists are called {reference:?}; use one's MusicBrainz id: {}",
            many.len(),
            many.iter()
                .map(|a| a.mbid.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        ))),
    }
}

/// The config to list with: the file if it checks out, else the latest
/// revision.
fn list_config(app: &App, state: &State) -> anyhow::Result<Loaded> {
    match app.import_config(state) {
        Ok((_, loaded)) => Ok(loaded),
        Err(e) => revisions::load_current(&state.conn, &app.dirs.home)
            .map(|(_, l)| l)
            .map_err(|_| e),
    }
}

/// Every followed artist's (or one's) release groups, classified.
pub fn entries(
    conn: &rusqlite::Connection,
    loaded: &Loaded,
    artist: Option<&str>,
) -> anyhow::Result<Vec<Entry>> {
    let today = Today::now(loaded.config.schedule.new_window.0);
    Ok(library::release_groups(conn, artist)?
        .into_iter()
        .map(|g| catalog::classify(g, &loaded.config.discography, today))
        .collect())
}

fn list(app: &App, args: &ListArgs, new: bool, out: &mut dyn Write) -> anyhow::Result<i32> {
    let state = app.open_state()?;
    let loaded = list_config(app, &state)?;
    let artist = args
        .artist
        .as_deref()
        .map(|a| find_artist(&state, a))
        .transpose()?;
    let mut rows: Vec<Entry> = entries(&state.conn, &loaded, artist.as_deref())?
        .into_iter()
        .filter(|e| {
            if new {
                e.is_new
            } else {
                matches!(e.status, Status::Missing | Status::Upcoming)
            }
        })
        .collect();
    rows.sort_by(|a, b| {
        let credit = |e: &Entry| {
            e.group
                .artist_credit
                .clone()
                .unwrap_or_default()
                .to_lowercase()
        };
        if new {
            b.group.first_release_date.cmp(&a.group.first_release_date)
        } else {
            credit(a)
                .cmp(&credit(b))
                .then(a.group.first_release_date.cmp(&b.group.first_release_date))
        }
    });
    if args.json {
        let json: Vec<_> = rows
            .iter()
            .map(|e| {
                serde_json::json!({
                    "release_group": e.group.mbid,
                    "title": e.group.title,
                    "artist": e.group.artist_credit,
                    "type": e.group.primary_type,
                    "date": e.group.first_release_date,
                    "status": e.status.key(),
                    "new": e.is_new,
                })
            })
            .collect();
        writeln!(out, "{}", serde_json::to_string_pretty(&json)?)?;
        return Ok(exit::OK);
    }
    if rows.is_empty() {
        writeln!(
            out,
            "{}",
            if new {
                "Nothing new."
            } else {
                "Nothing missing."
            }
        )?;
        return Ok(exit::OK);
    }
    let table: Vec<[String; 5]> = rows
        .iter()
        .map(|e| {
            [
                e.group
                    .first_release_date
                    .clone()
                    .unwrap_or_else(|| "?".into()),
                e.group.artist_credit.clone().unwrap_or_default(),
                e.group.title.clone(),
                e.group.primary_type.clone().unwrap_or_default(),
                e.group.mbid.clone(),
            ]
        })
        .collect();
    print_table(
        out,
        &["DATE", "ARTIST", "TITLE", "TYPE", "RELEASE GROUP"],
        &table,
    )?;
    Ok(exit::OK)
}

fn artists(app: &App, out: &mut dyn Write) -> anyhow::Result<i32> {
    let state = app.open_state()?;
    let loaded = list_config(app, &state)?;
    let all = entries(&state.conn, &loaded, None)?;
    let mut table = Vec::new();
    for artist in library::artists(&state.conn)? {
        let mine: Vec<&Entry> = all
            .iter()
            .filter(|e| e.group.artists.iter().any(|(a, _)| *a == artist.mbid))
            .collect();
        let count = |s: Status| mine.iter().filter(|e| e.status == s).count();
        table.push([
            artist.name.clone(),
            count(Status::Owned).to_string(),
            count(Status::Missing).to_string(),
            count(Status::Upcoming).to_string(),
            if artist.followed { "yes" } else { "no" }.into(),
            artist.mbid.clone(),
        ]);
    }
    if table.is_empty() {
        writeln!(out, "No artists yet; run `newcut scan`.")?;
    } else {
        print_table(
            out,
            &["ARTIST", "OWNED", "MISSING", "UPCOMING", "FOLLOWED", "MBID"],
            &table,
        )?;
    }
    Ok(exit::OK)
}

fn ignore(app: &App, groups: &[String], ignored: bool, out: &mut dyn Write) -> anyhow::Result<i32> {
    let state = app.open_state()?;
    for group in groups {
        if !crate::scan::tags::is_mbid(group) {
            return Err(usage(format!("{group:?} isn't a MusicBrainz id")));
        }
        library::set_ignored(
            &state.conn,
            &group.to_ascii_lowercase(),
            ignored,
            Timestamp::now(),
        )?;
    }
    writeln!(
        out,
        "{} {} release group{}",
        if ignored { "Ignored" } else { "Counting in" },
        groups.len(),
        if groups.len() == 1 { "" } else { "s" }
    )?;
    Ok(exit::OK)
}

fn config_cmd(app: &App, cmd: &ConfigCmd, out: &mut dyn Write) -> anyhow::Result<i32> {
    match cmd {
        ConfigCmd::Path => writeln!(out, "{}", app.config_path.display())?,
        ConfigCmd::Schema => write!(out, "{}", config::schema_json())?,
        ConfigCmd::Check => {
            let (_, loaded) = config::load_file(&app.config_path, &app.dirs.home)
                .map_err(|e| usage(format!("{e:#}")))?;
            for warning in &loaded.warnings {
                writeln!(out, "warning: {warning}")?;
            }
            if let Some(warning) = crate::env_file::readable(&app.env_path) {
                writeln!(out, "warning: {warning}")?;
            }
            if app.env_path.is_file() {
                writeln!(out, "Read {}", app.env_path.display())?;
            }
            writeln!(out, "{} is fine", app.config_path.display())?;
        }
        ConfigCmd::Show { show_secrets } => {
            let state = app.open_state()?;
            match revisions::current(&state.conn)? {
                Some(revision) => {
                    writeln!(
                        out,
                        "# revision {} ({}, {})",
                        revision.rev, revision.origin, revision.created_at
                    )?;
                    if *show_secrets {
                        write!(out, "{}", revision.text)?;
                    } else {
                        write!(out, "{}", config::diff::mask_toml(&revision.text))?;
                    }
                }
                None => writeln!(out, "No config imported yet.")?,
            }
        }
        ConfigCmd::Diff { show_secrets } => {
            let state = app.open_state()?;
            let (text, loaded) = config::load_file(&app.config_path, &app.dirs.home)
                .map_err(|e| usage(format!("{e:#}")))?;
            let name = app.config_path.display().to_string();
            let Some(current) = revisions::current(&state.conn)? else {
                writeln!(out, "No config imported yet; {name} would be revision 1.")?;
                return Ok(exit::OK);
            };
            if current.text == text {
                writeln!(
                    out,
                    "No differences between revision {} and {name}",
                    current.rev
                )?;
                return Ok(exit::OK);
            }
            let mask = |t: &str| {
                if *show_secrets {
                    t.to_string()
                } else {
                    config::diff::mask_toml(t)
                }
            };
            match config::check_text(&current.text, "stored", &app.dirs.home) {
                Ok(stored) => {
                    let changes = config::diff::config_changes(&stored.config, &loaded.config);
                    if changes.is_empty() {
                        writeln!(out, "Same settings; only comments or formatting differ")?;
                    } else {
                        for change in changes {
                            writeln!(out, "  {change}")?;
                        }
                    }
                }
                Err(_) => writeln!(out, "Revision {} no longer checks out", current.rev)?,
            }
            writeln!(out)?;
            write!(
                out,
                "{}",
                config::diff::text_diff(
                    &mask(&current.text),
                    &mask(&text),
                    &format!("revision {}", current.rev),
                    &name
                )
            )?;
        }
        ConfigCmd::Import => {
            let state = app.open_state()?;
            let (rev, _) = app.import_config(&state)?;
            writeln!(out, "Revision {rev} is current")?;
        }
    }
    Ok(exit::OK)
}

async fn daemon_cmd(app: &App, cmd: &DaemonCmd, out: &mut dyn Write) -> anyhow::Result<i32> {
    let state = app.open_state()?;
    let service = |name: &str| {
        Service::parse(name).ok_or_else(|| {
            usage(format!(
                "there's no service {name:?}; there's {}",
                Service::ALL.map(Service::name).join(", ")
            ))
        })
    };
    match cmd {
        DaemonCmd::Status => {
            let running = app.daemon_running();
            writeln!(
                out,
                "The daemon is {}",
                if running { "running" } else { "not running" }
            )?;
            let mut table = Vec::new();
            for s in Service::ALL {
                let row = store::service(&state.conn, s.name())?;
                let mut status = row.status.unwrap_or_else(|| "never ran".into());
                if row.paused {
                    status = "paused".into();
                }
                table.push([
                    s.name().to_string(),
                    status,
                    row.activity.unwrap_or_default(),
                    if row.restarts > 0 {
                        row.restarts.to_string()
                    } else {
                        String::new()
                    },
                ]);
            }
            print_table(out, &["SERVICE", "STATUS", "LAST", "RESTARTS"], &table)?;
        }
        DaemonCmd::Pause { service: name } | DaemonCmd::Resume { service: name } => {
            let s = service(name)?;
            let pause = matches!(cmd, DaemonCmd::Pause { .. });
            store::set_paused(&state.conn, s.name(), pause)?;
            writeln!(
                out,
                "{} {}{}",
                if pause { "Paused" } else { "Resumed" },
                s.name(),
                if app.daemon_running() {
                    ""
                } else {
                    " (for when the daemon runs)"
                }
            )?;
        }
        DaemonCmd::Restart { service: name } => {
            if !app.daemon_running() {
                return Err(usage(
                    "the daemon isn't running; start it with `newcut run`",
                ));
            }
            let target = name.as_deref().map(service).transpose()?;
            let id = store::request(
                &state.conn,
                target.map(Service::name),
                "restart",
                Timestamp::now(),
            )?;
            for _ in 0..50 {
                if let Some(answer) = store::answer_of(&state.conn, id)? {
                    writeln!(out, "The daemon says: {answer}")?;
                    return Ok(exit::OK);
                }
                tokio::time::sleep(Duration::from_millis(200)).await;
            }
            writeln!(out, "Asked the daemon; it hasn't answered yet")?;
        }
    }
    Ok(exit::OK)
}

/// Columns padded to their widest cell.
fn print_table<const N: usize>(
    out: &mut dyn Write,
    head: &[&str; N],
    rows: &[[String; N]],
) -> std::io::Result<()> {
    let mut widths = head.map(|h| h.chars().count());
    for row in rows {
        for (w, cell) in widths.iter_mut().zip(row) {
            *w = (*w).max(cell.chars().count());
        }
    }
    let line = |cells: Vec<&str>| {
        let mut text = String::new();
        for (i, (cell, w)) in cells.iter().zip(widths).enumerate() {
            text.push_str(cell);
            if i + 1 < N {
                text.push_str(&" ".repeat(w - cell.chars().count() + 2));
            }
        }
        text.trim_end().to_string()
    };
    writeln!(out, "{}", line(head.to_vec()))?;
    for row in rows {
        writeln!(out, "{}", line(row.iter().map(String::as_str).collect()))?;
    }
    Ok(())
}
