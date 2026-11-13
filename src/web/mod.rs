//! The web UI served by `newcut run` (the `server` service): HTML pages
//! (askama templates, htmx), an Atom feed of new releases, and a JSON API
//! under `/api/v1`.
//!
//! Security: the `Host` header must name the listen address when it's a
//! loopback one (no DNS rebinding); with `server.token` set, every request
//! needs it (a bearer header, the cookie the sign-in page sets, or `?token=`
//! for the feed); and every POST must be same-origin: pages send the CSRF
//! token htmx adds, the API takes a bearer token or a JSON body.

pub mod data;
mod download;
mod downloads;
mod feed;
mod thumbs;

use std::fmt::Write as _;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;

use askama::Template;
use axum::Router;
use axum::extract::{Form, Path, Query, Request, State};
use axum::http::{HeaderMap, HeaderValue, Method, StatusCode, header};
use axum::middleware::{self, Next};
use axum::response::{Html, IntoResponse, Json, Redirect, Response};
use axum::routing::{get, post};
use serde::Deserialize;

use crate::config::store as revisions;
use crate::config::{Loaded, ServerConfig};
use crate::daemon::service::{Service, Wakes};
use crate::state::State as Db;
use crate::state::daemon as store;
use crate::state::library;
use data::Row;

const SESSION_COOKIE: &str = "newcut_session";
const PER_PAGE: usize = 50;

/// What every request handler shares.
pub struct Web {
    db: PathBuf,
    token: String,
    csrf: String,
    /// `Host` values accepted; empty: any (a token protects the server).
    hosts: Vec<String>,
    wakes: Arc<Wakes>,
    dirs: crate::paths::Dirs,
    config_path: PathBuf,
    /// Ends the live event streams when the server stops.
    shutdown: crate::shutdown::Shutdown,
}

impl Web {
    #[must_use]
    pub fn new(
        db: PathBuf,
        dirs: crate::paths::Dirs,
        config_path: PathBuf,
        server: &ServerConfig,
        addr: SocketAddr,
        wakes: Arc<Wakes>,
        shutdown: crate::shutdown::Shutdown,
    ) -> Self {
        let port = addr.port();
        let hosts = if addr.ip().is_loopback() {
            vec![
                format!("127.0.0.1:{port}"),
                format!("localhost:{port}"),
                format!("[::1]:{port}"),
            ]
        } else {
            Vec::new()
        };
        Self {
            db,
            token: server
                .token
                .as_deref()
                .and_then(crate::config::secret::resolve)
                .unwrap_or_default(),
            csrf: random_hex(16),
            hosts,
            wakes,
            dirs,
            config_path,
            shutdown,
        }
    }

    /// What the sign-in cookie holds: derived from the token, so changing
    /// the token signs everyone out.
    fn session(&self) -> String {
        use sha2::Digest;
        let digest = sha2::Sha256::digest(format!("newcut-session:{}", self.token));
        crate::hex::encode(&digest)
    }

    /// Where thumbnails of pictures in your folders are kept.
    fn thumbs(&self) -> PathBuf {
        self.dirs.state().cache.join("thumbs")
    }

    /// Run `f` on a database connection with the current config, off the
    /// async threads.
    async fn db<T: Send + 'static>(
        self: &Arc<Self>,
        f: impl FnOnce(&rusqlite::Connection, i64, &Loaded) -> anyhow::Result<T> + Send + 'static,
    ) -> Result<T, WebError> {
        let path = self.db.clone();
        let home = self.dirs.home.clone();
        tokio::task::spawn_blocking(move || {
            let state = Db::open(&path)?;
            let (rev, loaded) = revisions::load_current(&state.conn, &home)?;
            f(&state.conn, rev, &loaded)
        })
        .await
        .map_err(|e| WebError(anyhow::anyhow!(e)))?
        .map_err(WebError)
    }
}

/// Serve `web` on `listener` until `shutdown` stops it.
///
/// # Errors
///
/// Fails if accepting connections fails.
pub async fn serve(
    web: Arc<Web>,
    listener: tokio::net::TcpListener,
    shutdown: crate::shutdown::Shutdown,
) -> std::io::Result<()> {
    axum::serve(listener, router(web))
        .with_graceful_shutdown(async move { shutdown.stopping().await })
        .await
}

pub fn router(web: Arc<Web>) -> Router {
    Router::new()
        .route("/", get(dashboard))
        .route("/fragments/status", get(status_fragment))
        .route("/fragments/live", get(live_fragment))
        .route("/actions/run/{service}", post(action_run))
        .route("/actions/services/{name}/{action}", post(action_service))
        .route("/releases", get(releases_page))
        .route("/releases/{mbid}/row", get(release_row))
        .route("/artists", get(artists_page))
        .route("/artists/search", get(artist_search))
        .route("/artists/add", post(artist_add))
        .route("/artists/{mbid}", get(artist_page))
        .route("/artists/{mbid}/feed.atom", get(feed::artist_atom))
        .route("/artists/{mbid}/image", get(artist_image))
        .route("/artists/{mbid}/{action}", post(artist_action))
        .route(
            "/release-groups/{mbid}/{action}",
            post(release_group_action),
        )
        .route("/downloads", get(downloads::page))
        .route("/downloads/{id}/releases", get(downloads::releases))
        .route("/downloads/{id}/file", post(downloads::file))
        .route("/downloads/{id}/{action}", post(downloads::action))
        .route("/download/{key}", get(download::page))
        .route("/download/{key}/searches", post(download::start))
        .route("/download/{key}/searches/{id}", get(download::poll))
        .route("/download/{key}/searches/{id}/queue", post(download::queue))
        .route("/download/{key}/downloads", get(download::downloads))
        .route(
            "/download/{key}/downloads/{download}/forget",
            post(download::forget),
        )
        .route("/missing", get(missing_page))
        .route("/untagged", get(untagged_page))
        .route("/config", get(config_page))
        .route("/config/check", post(config_check))
        .route("/config/save", post(config_save))
        .route("/config/revisions/{rev}", get(revision_page))
        .route("/config/rollback/{rev}", post(config_rollback))
        .route("/feed.atom", get(feed::atom))
        .route("/login", get(login_page).post(login))
        .route("/assets/{file}", get(asset))
        .route("/covers/{mbid}", get(cover))
        .route("/api/v1/events", get(api_events))
        .route("/api/v1/status", get(api_status))
        .route("/api/v1/new", get(api_new))
        .route("/api/v1/missing", get(api_missing))
        .route("/api/v1/run/{service}", post(api_run))
        .layer(middleware::from_fn_with_state(web.clone(), guard))
        .layer(middleware::map_response(no_store))
        .with_state(web)
}

/// An error shown as a plain 500.
#[derive(Debug)]
pub struct WebError(anyhow::Error);

impl IntoResponse for WebError {
    fn into_response(self) -> Response {
        tracing::error!("web UI: {:#}", self.0);
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("something went wrong: {:#}", self.0),
        )
            .into_response()
    }
}

fn render(template: &impl Template) -> Result<Html<String>, WebError> {
    template
        .render()
        .map(Html)
        .map_err(|e| WebError(anyhow::anyhow!(e)))
}

pub(super) fn not_found(what: &str) -> Response {
    (StatusCode::NOT_FOUND, format!("there's no {what}")).into_response()
}

/// htmx reloads the page (after an action on it).
fn refresh() -> Response {
    ([("hx-refresh", "true")], StatusCode::NO_CONTENT).into_response()
}

// ---------- dashboard ----------

#[derive(Template)]
#[template(path = "dashboard.html")]
struct DashboardPage {
    csrf: String,
    status: data::Overview,
    message: Option<String>,
    live: crate::events::Live,
}

#[derive(Template)]
#[template(path = "status.html")]
struct StatusFragment {
    status: data::Overview,
    message: Option<String>,
}

#[derive(Template)]
#[template(path = "live.html")]
struct LiveFragment {
    live: crate::events::Live,
}

async fn overview(web: &Arc<Web>) -> Result<data::Overview, WebError> {
    web.db(data::overview).await
}

async fn dashboard(State(web): State<Arc<Web>>) -> Result<Html<String>, WebError> {
    let status = overview(&web).await?;
    render(&DashboardPage {
        csrf: web.csrf.clone(),
        status,
        message: None,
        live: crate::events::live(),
    })
}

async fn status_fragment(State(web): State<Arc<Web>>) -> Result<Html<String>, WebError> {
    fragment(&web, None).await
}

async fn fragment(web: &Arc<Web>, message: Option<String>) -> Result<Html<String>, WebError> {
    let status = overview(web).await?;
    render(&StatusFragment { status, message })
}

async fn live_fragment() -> Result<Html<String>, WebError> {
    render(&LiveFragment {
        live: crate::events::live(),
    })
}

/// Wake a service; returns what happened.
async fn run_now(web: &Arc<Web>, name: &str) -> Result<String, WebError> {
    let Some(service) = Service::parse(name).filter(|s| *s != Service::Server) else {
        return Ok(format!("there's no service {name:?} to run"));
    };
    let paused = web
        .db(move |conn, _, loaded| {
            Ok(store::service(conn, service.name())?.paused
                || !crate::daemon::service::enabled(service, &loaded.config))
        })
        .await?;
    if paused {
        return Ok(format!("The {name} is paused or off."));
    }
    web.wakes.wake(service);
    Ok(format!("The {name} runs now."))
}

async fn action_run(
    State(web): State<Arc<Web>>,
    Path(service): Path<String>,
) -> Result<Html<String>, WebError> {
    let message = run_now(&web, &service).await?;
    fragment(&web, Some(message)).await
}

async fn action_service(
    State(web): State<Arc<Web>>,
    Path((name, action)): Path<(String, String)>,
) -> Result<Response, WebError> {
    let Some(service) = Service::parse(&name) else {
        return Ok(not_found("such service"));
    };
    let pause = match action.as_str() {
        "pause" => true,
        "resume" => false,
        _ => return Ok(not_found("such action")),
    };
    if pause && service == Service::Server {
        return Ok((
            StatusCode::BAD_REQUEST,
            "pausing the web UI from the web UI would lock you out; use `newcut daemon pause server`",
        )
            .into_response());
    }
    web.db(move |conn, _, _| Ok(store::set_paused(conn, service.name(), pause)?))
        .await?;
    let message = format!(
        "{} {name}; it takes a moment.",
        if pause { "Pausing" } else { "Resuming" }
    );
    Ok(fragment(&web, Some(message)).await?.into_response())
}

// ---------- artists ----------

#[derive(Deserialize, Default)]
struct ListQuery {
    #[serde(default)]
    q: String,
    #[serde(default)]
    status: String,
    #[serde(default)]
    page: Option<usize>,
}

impl ListQuery {
    fn page(&self) -> usize {
        self.page.unwrap_or(1).max(1)
    }
}

/// A filter chip.
struct Chip {
    key: &'static str,
    label: &'static str,
    count: usize,
    selected: bool,
}

/// Page `page` of `items`, and how many pages there are.
fn paginate<T>(items: Vec<T>, page: usize) -> (Vec<T>, usize) {
    let pages = items.len().div_ceil(PER_PAGE).max(1);
    let page = page.min(pages);
    let items = items
        .into_iter()
        .skip((page - 1) * PER_PAGE)
        .take(PER_PAGE)
        .collect();
    (items, pages)
}

fn link(base: &str, q: &str, status: &str, page: usize) -> String {
    let mut params = Vec::new();
    if !q.is_empty() {
        params.push(format!("q={}", encode(q)));
    }
    if !status.is_empty() {
        params.push(format!("status={}", encode(status)));
    }
    if page > 1 {
        params.push(format!("page={page}"));
    }
    if params.is_empty() {
        base.to_string()
    } else {
        format!("{base}?{}", params.join("&"))
    }
}

fn encode(text: &str) -> String {
    let mut out = String::new();
    for b in text.bytes() {
        if b.is_ascii_alphanumeric() || b"-_.~".contains(&b) {
            out.push(char::from(b));
        } else {
            let _ = write!(out, "%{b:02X}");
        }
    }
    out
}

fn matches_query(q: &str, fields: &[&str]) -> bool {
    let q = deunicode::deunicode(q).to_lowercase();
    q.is_empty()
        || fields
            .iter()
            .any(|f| deunicode::deunicode(f).to_lowercase().contains(&q))
}

#[derive(Template)]
#[template(path = "artists.html")]
struct ArtistsPage {
    csrf: String,
    rows: Vec<data::ArtistRow>,
    chips: Vec<Chip>,
    query: String,
    status: String,
    page: usize,
    pages: usize,
    total: usize,
    all_count: usize,
}

impl ArtistsPage {
    fn status_link(&self, status: &str) -> String {
        link("/artists", &self.query, status, 1)
    }

    fn page_link(&self, page: usize) -> String {
        link("/artists", &self.query, &self.status, page)
    }
}

/// An artists list filter: its key, label, and who it keeps.
type Filter = (&'static str, &'static str, fn(&data::ArtistRow) -> bool);

async fn artists_page(
    State(web): State<Arc<Web>>,
    Query(query): Query<ListQuery>,
) -> Result<Html<String>, WebError> {
    let rows = web
        .db(|conn, _, loaded| data::artist_rows(conn, loaded))
        .await?;
    let all_count = rows.len();
    let filters: [Filter; 4] = [
        ("missing", "Missing albums", |a| a.followed && a.missing > 0),
        ("new", "With new releases", |a| a.followed && a.new > 0),
        ("complete", "Complete", |a| {
            a.followed && a.missing == 0 && a.owned > 0
        }),
        ("unfollowed", "Not followed", |a| !a.followed),
    ];
    let chips = filters
        .iter()
        .map(|(key, label, f)| Chip {
            key,
            label,
            count: rows.iter().filter(|a| f(a)).count(),
            selected: query.status == *key,
        })
        .collect();
    let filter = filters
        .iter()
        .find(|(k, _, _)| *k == query.status)
        .map(|f| f.2);
    let rows: Vec<_> = rows
        .into_iter()
        .filter(|a| filter.is_none_or(|f| f(a)))
        .filter(|a| matches_query(&query.q, &[&a.name, &a.mbid]))
        .collect();
    let total = rows.len();
    let (rows, pages) = paginate(rows, query.page());
    render(&ArtistsPage {
        csrf: web.csrf.clone(),
        rows,
        chips,
        status: if filter.is_some() {
            query.status.clone()
        } else {
            String::new()
        },
        page: query.page().min(pages),
        query: query.q,
        pages,
        total,
        all_count,
    })
}

#[derive(Template)]
#[template(path = "partials/artist_hits.html")]
struct ArtistHits {
    query: String,
    hits: Vec<ArtistHitView>,
    error: Option<String>,
}

/// An artist found on MusicBrainz, and whether they're here already.
struct ArtistHitView {
    mbid: String,
    name: String,
    disambiguation: Option<String>,
    area: Option<String>,
    /// In the state DB (files or followed).
    known: bool,
    followed: bool,
    /// What following them posts (`hx-vals`).
    vals: String,
}

#[derive(Deserialize)]
struct SearchQuery {
    #[serde(default)]
    q: String,
}

/// Search the configured MusicBrainz server for artists to follow.
async fn artist_search(
    State(web): State<Arc<Web>>,
    Query(query): Query<SearchQuery>,
) -> Result<Html<String>, WebError> {
    let q = query.q.trim().to_string();
    if q.chars().count() < 2 {
        return render(&ArtistHits {
            query: String::new(),
            hits: Vec::new(),
            error: None,
        });
    }
    let musicbrainz = web
        .db(|_, _, loaded| Ok(loaded.config.musicbrainz.clone()))
        .await?;
    let http = crate::http::Http::new(musicbrainz.contact.as_deref());
    let mb = crate::musicbrainz::MusicBrainz {
        http: &http,
        server: &musicbrainz.server,
    };
    let hits = match mb.search_artists(&q, 10).await {
        Ok(hits) => hits,
        Err(error) => {
            tracing::warn!("artist search: {error:#}");
            return render(&ArtistHits {
                query: q,
                hits: Vec::new(),
                error: Some(format!("{error:#}")),
            });
        }
    };
    let hits = web
        .db(move |conn, _, _| {
            hits.into_iter()
                .map(|hit| {
                    let here = library::artist(conn, &hit.mbid)?;
                    let vals = serde_json::json!({
                        "mbid": hit.mbid,
                        "name": hit.name,
                        "sort_name": hit.sort_name,
                    })
                    .to_string();
                    Ok(ArtistHitView {
                        known: here.is_some(),
                        followed: here.is_some_and(|a| a.followed),
                        mbid: hit.mbid,
                        name: hit.name,
                        disambiguation: hit.disambiguation,
                        area: hit.area,
                        vals,
                    })
                })
                .collect::<anyhow::Result<Vec<_>>>()
        })
        .await?;
    render(&ArtistHits {
        query: q,
        hits,
        error: None,
    })
}

#[derive(Deserialize)]
struct AddArtist {
    mbid: String,
    name: String,
    #[serde(default)]
    sort_name: String,
}

/// Follow an artist found by a search; they're looked up and checked next.
async fn artist_add(
    State(web): State<Arc<Web>>,
    Form(form): Form<AddArtist>,
) -> Result<Response, WebError> {
    let mbid = form.mbid.trim().to_ascii_lowercase();
    if !crate::scan::tags::is_mbid(&mbid) {
        return Ok((
            StatusCode::BAD_REQUEST,
            "that isn't a MusicBrainz artist id",
        )
            .into_response());
    }
    let id = mbid.clone();
    let added = web
        .db(move |conn, _, _| {
            Ok(library::add_artist(
                conn,
                &id,
                form.name.trim(),
                form.sort_name.trim(),
                jiff::Timestamp::now(),
            )?)
        })
        .await?;
    if !added {
        return Ok((StatusCode::BAD_REQUEST, "that artist can't be followed").into_response());
    }
    // Their name, then their discography.
    web.wakes.wake(Service::Resolver);
    Ok((
        [("hx-redirect", format!("/artists/{mbid}"))],
        StatusCode::NO_CONTENT,
    )
        .into_response())
}

#[derive(Template)]
#[template(path = "artist.html")]
struct ArtistPage {
    csrf: String,
    artist: library::Artist,
    sections: Vec<TypeSection>,
    chips: Vec<Chip>,
    status: String,
    all_count: usize,
    unmatched: Vec<library::OwnedAlbum>,
    /// A joined artist's members.
    members: Vec<(String, String)>,
    image: Option<String>,
    initial: String,
    checked: Option<String>,
    next_check: Option<String>,
}

/// An artist's release groups of one primary type.
struct TypeSection {
    /// Its primary type, as `type_key` says it.
    key: &'static str,
    title: &'static str,
    rows: Vec<Row>,
    /// How many are missing (and can be ignored all at once).
    ignorable: usize,
}

/// The primary types the artist page has a section for, in order.
const TYPE_SECTIONS: [(&str, &str); 5] = [
    ("album", "Albums"),
    ("ep", "EPs"),
    ("single", "Singles"),
    ("broadcast", "Broadcasts"),
    ("other", "Other"),
];

/// The section a release group of `primary_type` goes in: one of
/// `TYPE_SECTIONS`' keys, or `unknown`.
fn type_key(primary_type: Option<&str>) -> &'static str {
    primary_type
        .and_then(|p| {
            TYPE_SECTIONS
                .iter()
                .find(|(k, _)| k.eq_ignore_ascii_case(p))
        })
        .map_or("unknown", |(k, _)| k)
}

/// `rows` by primary type: albums, EPs, singles, then the rest (keeping
/// their order within each).
fn type_sections(rows: Vec<Row>) -> Vec<TypeSection> {
    let mut sections: Vec<TypeSection> = TYPE_SECTIONS
        .iter()
        .chain([&("unknown", "Unknown type")])
        .map(|(key, title)| TypeSection {
            key,
            title,
            rows: Vec::new(),
            ignorable: 0,
        })
        .collect();
    for row in rows {
        let key = type_key(row.primary_type.as_deref());
        let Some(section) = sections.iter_mut().find(|s| s.key == key) else {
            continue;
        };
        section.ignorable += usize::from(row.status == "missing");
        section.rows.push(row);
    }
    sections.retain(|s| !s.rows.is_empty());
    sections
}

impl ArtistPage {
    fn status_link(&self, status: &str) -> String {
        link(&format!("/artists/{}", self.artist.mbid), "", status, 1)
    }
}

/// Chips for release group lists, and the rows `status` keeps.
fn release_chips(rows: &[Row], status: &str) -> Vec<Chip> {
    [
        ("missing", "Missing"),
        ("upcoming", "Upcoming"),
        ("new", "New"),
        ("owned", "Owned"),
        ("ignored", "Ignored"),
        ("other", "Not counted"),
    ]
    .into_iter()
    .map(|(key, label)| Chip {
        key,
        label,
        count: rows.iter().filter(|r| row_is(r, key)).count(),
        selected: status == key,
    })
    .filter(|c| c.count > 0 || c.selected || matches!(c.key, "missing" | "owned"))
    .collect()
}

fn row_is(row: &Row, status: &str) -> bool {
    match status {
        "" => row.status != "other" && row.status != "ignored",
        "new" => row.is_new,
        key => row.status == key,
    }
}

async fn artist_page(
    State(web): State<Arc<Web>>,
    Path(mbid): Path<String>,
    Query(query): Query<ListQuery>,
) -> Result<Response, WebError> {
    let id = mbid.clone();
    let found = web
        .db(move |conn, _, loaded| {
            let Some(artist) = library::artist(conn, &id)? else {
                return Ok(None);
            };
            let (_, rows) = data::rows(conn, loaded, Some(&id))?;
            // Owned folders MusicBrainz's discography doesn't list.
            let listed: std::collections::HashSet<String> =
                rows.iter().filter_map(|r| r.mbid.clone()).collect();
            let unmatched = library::owned_albums(conn, &id)?
                .into_iter()
                .filter(|a| {
                    a.release_group_mbid
                        .as_ref()
                        .is_none_or(|g| !listed.contains(g))
                })
                .collect();
            let has_files =
                loaded.config.covers.local && !library::artist_album_dirs(conn, &id)?.is_empty();
            let image = data::artist_image(loaded, &artist, has_files);
            let members = library::members(conn, &id)?;
            Ok(Some((artist, rows, unmatched, image, members)))
        })
        .await?;
    let Some((artist, rows, unmatched, image, members)) = found else {
        return Ok(not_found("such artist"));
    };
    let chips = release_chips(&rows, &query.status);
    let all_count = rows.iter().filter(|r| row_is(r, "")).count();
    let rows: Vec<Row> = rows
        .into_iter()
        .filter(|r| row_is(r, &query.status))
        .collect();
    Ok(render(&ArtistPage {
        csrf: web.csrf.clone(),
        checked: artist.last_checked_at.map(crate::daemon::service::local),
        next_check: artist.next_check_at.map(crate::daemon::service::local),
        initial: data::initial(&artist.name),
        image,
        members,
        artist,
        sections: type_sections(rows),
        chips,
        status: query.status,
        all_count,
        unmatched,
    })?
    .into_response())
}

#[derive(Deserialize)]
struct ArtistActionQuery {
    /// `ignore-missing`: only release groups of this type (a section's key).
    #[serde(default, rename = "type")]
    kind: Option<String>,
}

async fn artist_action(
    State(web): State<Arc<Web>>,
    Path((mbid, action)): Path<(String, String)>,
    Query(query): Query<ArtistActionQuery>,
) -> Result<Response, WebError> {
    let wake = action == "refresh";
    let id = mbid.clone();
    let done = web
        .db(move |conn, _, loaded| {
            Ok(match action.as_str() {
                "refresh" => library::check_now(conn, &mbid)?,
                "follow" => library::set_followed(conn, &mbid, true)?,
                "unfollow" => library::set_followed(conn, &mbid, false)?,
                "ignore-missing" => {
                    let now = jiff::Timestamp::now();
                    for entry in crate::app::entries(conn, loaded, Some(&mbid))? {
                        let of_kind = query
                            .kind
                            .as_deref()
                            .is_none_or(|k| k == type_key(entry.group.primary_type.as_deref()));
                        if entry.status == crate::catalog::Status::Missing && of_kind {
                            library::set_ignored(conn, &entry.group.mbid, true, now)?;
                        }
                    }
                    true
                }
                _ => false,
            })
        })
        .await?;
    if !done {
        return Ok(not_found("such artist or action"));
    }
    if wake {
        // Their folders first: what you just added there counts.
        if scan_artist(&web, &id).await? {
            web.wakes.wake(Service::Resolver);
        }
        web.wakes.wake(Service::Discography);
    }
    Ok(refresh())
}

/// Scan artist `mbid`'s folders (where their albums are, and
/// `<library>/<their name>`); returns whether anything was new or gone.
pub(super) async fn scan_artist(web: &Arc<Web>, mbid: &str) -> Result<bool, WebError> {
    let path = web.db.clone();
    let home = web.dirs.home.clone();
    let mbid = mbid.to_string();
    tokio::task::spawn_blocking(move || {
        let mut state = Db::open(&path)?;
        let (_, loaded) = revisions::load_current(&state.conn, &home)?;
        let Some(artist) = library::artist(&state.conn, &mbid)? else {
            return Ok(false);
        };
        let mut changed = false;
        for (library, dirs) in artist_folders(&state.conn, &loaded, &artist)? {
            let report = crate::scan::scan_dirs(&mut state.conn, &library, &dirs)?;
            tracing::info!("scanned {}'s folders: {}", artist.name, report.summary());
            changed |= report.read > 0 || report.removed > 0;
        }
        Ok(changed)
    })
    .await
    .map_err(|e| WebError(anyhow::anyhow!(e)))?
    .map_err(WebError)
}

/// An artist's folders, by library: the folders their album folders are in
/// (or those album folders, when they're right in the library), and
/// `<library>/<their name>`, when there's one.
fn artist_folders(
    conn: &rusqlite::Connection,
    loaded: &Loaded,
    artist: &library::Artist,
) -> anyhow::Result<Vec<(PathBuf, Vec<PathBuf>)>> {
    let albums = library::artist_album_dirs(conn, &artist.mbid)?;
    let mut out: Vec<(PathBuf, Vec<PathBuf>)> = Vec::new();
    for library in &loaded.libraries {
        let mut dirs: Vec<PathBuf> = Vec::new();
        let mut add = |dir: PathBuf| {
            // Nested in another one already: scanned with it.
            if !dirs.iter().any(|d| dir.starts_with(d)) {
                dirs.retain(|d| !d.starts_with(&dir));
                dirs.push(dir);
            }
        };
        for (_, dir) in albums
            .iter()
            .filter(|(l, _)| std::path::Path::new(l) == library)
        {
            let dir = PathBuf::from(dir);
            match dir.parent() {
                Some(parent) if parent != library.as_path() && parent.starts_with(library) => {
                    add(parent.to_path_buf());
                }
                _ => add(dir),
            }
        }
        let named = library.join(crate::import::safe(&artist.name));
        if named.is_dir() {
            add(named);
        }
        if !dirs.is_empty() {
            out.push((library.clone(), dirs));
        }
    }
    Ok(out)
}

async fn release_group_action(
    State(web): State<Arc<Web>>,
    Path((mbid, action)): Path<(String, String)>,
) -> Result<Response, WebError> {
    let ignored = match action.as_str() {
        "ignore" => true,
        "unignore" => false,
        _ => return Ok(not_found("such action")),
    };
    if !crate::scan::tags::is_mbid(&mbid) {
        return Ok(not_found("such release group"));
    }
    web.db(move |conn, _, _| {
        Ok(library::set_ignored(
            conn,
            &mbid,
            ignored,
            jiff::Timestamp::now(),
        )?)
    })
    .await?;
    Ok(refresh())
}

// ---------- missing, untagged ----------

/// A list of releases across artists: Latest releases, Missing.
struct ReleaseList {
    path: &'static str,
    rows: Vec<Row>,
    chips: Vec<Chip>,
    query: String,
    status: String,
    page: usize,
    pages: usize,
    total: usize,
}

impl ReleaseList {
    /// Every followed artist's releases that `wanted`, with the chips in
    /// `chips`, filtered and paged by `query`.
    async fn load(
        web: &Arc<Web>,
        path: &'static str,
        query: ListQuery,
        wanted: fn(&Row) -> bool,
        chips: &[&str],
    ) -> Result<Self, WebError> {
        let (_, rows) = web
            .db(|conn, _, loaded| data::rows(conn, loaded, None))
            .await?;
        let rows: Vec<Row> = rows.into_iter().filter(wanted).collect();
        let chips: Vec<Chip> = release_chips(&rows, &query.status)
            .into_iter()
            .filter(|c| chips.contains(&c.key))
            .collect();
        let status = if chips.iter().any(|c| c.selected) {
            query.status.clone()
        } else {
            String::new()
        };
        let rows: Vec<Row> = rows
            .into_iter()
            .filter(|r| status.is_empty() || row_is(r, &status))
            .filter(|r| matches_query(&query.q, &[&r.title, &r.artist]))
            .collect();
        let total = rows.len();
        let (rows, pages) = paginate(rows, query.page());
        Ok(Self {
            path,
            rows,
            chips,
            page: query.page().min(pages),
            query: query.q,
            status,
            pages,
            total,
        })
    }

    fn status_link(&self, status: &str) -> String {
        link(self.path, &self.query, status, 1)
    }

    fn page_link(&self, page: usize) -> String {
        link(self.path, &self.query, &self.status, page)
    }
}

#[derive(Template)]
#[template(path = "releases.html")]
struct ReleasesPage {
    csrf: String,
    list: ReleaseList,
}

/// Everything the artists you follow released (and will), newest first.
#[derive(Template)]
#[template(path = "partials/release_row.html")]
struct RowFragment {
    row: Row,
    show_artist: bool,
}

#[derive(Deserialize)]
struct RowQuery {
    /// Shown on this artist's page (no artist line); else in a list.
    artist: Option<String>,
}

/// One release group's row, as it is now (redrawn when its download
/// changes).
async fn release_row(
    State(web): State<Arc<Web>>,
    Path(mbid): Path<String>,
    Query(query): Query<RowQuery>,
) -> Result<Response, WebError> {
    let artist = query.artist.filter(|a| crate::scan::tags::is_mbid(a));
    let show_artist = artist.is_none();
    let row = web
        .db(move |conn, _, loaded| {
            let (_, rows) = data::rows(conn, loaded, artist.as_deref())?;
            Ok(rows.into_iter().find(|r| r.key == mbid))
        })
        .await?;
    let Some(row) = row else {
        return Ok(not_found("such release"));
    };
    Ok(render(&RowFragment { row, show_artist })?.into_response())
}

async fn releases_page(
    State(web): State<Arc<Web>>,
    Query(query): Query<ListQuery>,
) -> Result<Html<String>, WebError> {
    let list = ReleaseList::load(
        &web,
        "/releases",
        query,
        |r| row_is(r, ""),
        &["new", "upcoming", "owned", "missing"],
    )
    .await?;
    render(&ReleasesPage {
        csrf: web.csrf.clone(),
        list,
    })
}

#[derive(Template)]
#[template(path = "missing.html")]
struct MissingPage {
    csrf: String,
    list: ReleaseList,
}

async fn missing_page(
    State(web): State<Arc<Web>>,
    Query(query): Query<ListQuery>,
) -> Result<Html<String>, WebError> {
    let list = ReleaseList::load(
        &web,
        "/missing",
        query,
        |r| matches!(r.status.as_str(), "missing" | "upcoming"),
        &["missing", "upcoming", "new"],
    )
    .await?;
    render(&MissingPage {
        csrf: web.csrf.clone(),
        list,
    })
}

#[derive(Template)]
#[template(path = "untagged.html")]
struct UntaggedPage {
    csrf: String,
    folders: Vec<library::Untagged>,
}

async fn untagged_page(State(web): State<Arc<Web>>) -> Result<Html<String>, WebError> {
    let folders = web.db(|conn, _, _| Ok(library::untagged(conn)?)).await?;
    render(&UntaggedPage {
        csrf: web.csrf.clone(),
        folders,
    })
}

// ---------- config ----------

#[derive(Template)]
#[template(path = "config.html")]
struct ConfigPage {
    csrf: String,
    rev: i64,
    text: String,
    file: String,
    history: Vec<revisions::Revision>,
    message: Option<String>,
}

#[derive(Template)]
#[template(path = "config_result.html")]
struct ConfigResult {
    errors: Option<String>,
    warnings: Vec<String>,
    /// What it changes, one line per key.
    changes: Vec<String>,
    /// The text differs (the settings may not: comments, formatting).
    text_changed: bool,
    /// A unified diff from the current revision, secrets masked.
    diff: String,
    /// The current revision.
    rev: i64,
    saved: bool,
}

#[derive(Template)]
#[template(path = "revision.html")]
struct RevisionPage {
    csrf: String,
    revision: revisions::Revision,
    current: i64,
    /// Its text, secrets masked.
    text: String,
    /// What rolling back to it changes.
    changes: Vec<String>,
    diff: String,
}

#[derive(Deserialize)]
struct ConfigForm {
    text: String,
    /// The revision the page showed, so a save doesn't replace a newer one.
    #[serde(default)]
    base: Option<i64>,
}

#[derive(Deserialize)]
struct ConfigQuery {
    saved: Option<i64>,
}

async fn config_page(
    State(web): State<Arc<Web>>,
    Query(query): Query<ConfigQuery>,
) -> Result<Html<String>, WebError> {
    let (rev, text, history) = web
        .db(|conn, rev, _| {
            let text = revisions::get(conn, rev)?
                .map(|r| r.text)
                .unwrap_or_default();
            Ok((rev, text, revisions::list(conn, 50)?))
        })
        .await?;
    render(&ConfigPage {
        csrf: web.csrf.clone(),
        rev,
        text: crate::config::diff::mask_toml(&text),
        file: web.config_path.display().to_string(),
        history,
        message: query.saved.map(|rev| format!("Saved as revision {rev}.")),
    })
}

fn check(web: &Web, text: &str) -> Result<Loaded, String> {
    crate::config::check_text(text, "config", &web.dirs.home).map_err(|e| e.join("\n"))
}

/// Check edited text against the current revision: its mistakes, or what
/// it changes. Also returns the text with its secrets restored.
async fn compare(web: &Arc<Web>, edited: &str) -> Result<(ConfigResult, String), WebError> {
    let (rev, current, loaded) = web
        .db(|conn, rev, loaded| {
            let text = revisions::get(conn, rev)?
                .map(|r| r.text)
                .unwrap_or_default();
            Ok((rev, text, loaded.clone()))
        })
        .await?;
    let text = crate::config::diff::unmask_toml(&edited.replace("\r\n", "\n"), &current);
    let mut result = ConfigResult {
        errors: None,
        warnings: Vec::new(),
        changes: Vec::new(),
        text_changed: text != current,
        diff: String::new(),
        rev,
        saved: false,
    };
    match check(web, &text) {
        Err(errors) => result.errors = Some(errors),
        Ok(checked) => {
            result.warnings = checked.warnings;
            result.changes = crate::config::diff::config_changes(&loaded.config, &checked.config);
            result.diff = crate::config::diff::text_diff(
                &crate::config::diff::mask_toml(&current),
                &crate::config::diff::mask_toml(&text),
                &format!("revision {rev}"),
                "edited",
            );
        }
    }
    Ok((result, text))
}

async fn config_check(
    State(web): State<Arc<Web>>,
    Form(form): Form<ConfigForm>,
) -> Result<Html<String>, WebError> {
    render(&compare(&web, &form.text).await?.0)
}

/// Write `text` to the config file, and store it as a revision.
async fn store_and_write(web: &Arc<Web>, text: String, origin: String) -> Result<i64, WebError> {
    let path = web.config_path.clone();
    web.db(move |conn, _, _| {
        crate::config::store::write_file(&path, &text)?;
        let (rev, _) = revisions::add(conn, &text, &origin, jiff::Timestamp::now())?;
        Ok(rev)
    })
    .await
}

async fn config_save(
    State(web): State<Arc<Web>>,
    Form(form): Form<ConfigForm>,
) -> Result<Response, WebError> {
    let (mut result, text) = compare(&web, &form.text).await?;
    if form.base.is_some_and(|base| base != result.rev) {
        result.errors = Some(format!(
            "The config changed since you opened this page (it's revision {} now). \
             Reload to see it; saving would have replaced it.",
            result.rev
        ));
    }
    if result.errors.is_some() || !result.text_changed {
        return Ok(render(&result)?.into_response());
    }
    let rev = store_and_write(&web, text, "web".into()).await?;
    Ok((
        [("hx-redirect", format!("/config?saved={rev}"))],
        StatusCode::NO_CONTENT,
    )
        .into_response())
}

async fn revision_page(
    State(web): State<Arc<Web>>,
    Path(rev): Path<i64>,
) -> Result<Response, WebError> {
    let found = web
        .db(move |conn, current, loaded| {
            let Some(revision) = revisions::get(conn, rev)? else {
                return Ok(None);
            };
            let current_text = revisions::get(conn, current)?
                .map(|r| r.text)
                .unwrap_or_default();
            Ok(Some((revision, current, current_text, loaded.clone())))
        })
        .await?;
    let Some((revision, current, current_text, loaded)) = found else {
        return Ok(not_found("such revision"));
    };
    // What rolling back would change (nothing to say when it no longer
    // checks out: rolling back refuses then).
    let changes = check(&web, &revision.text)
        .map(|old| crate::config::diff::config_changes(&loaded.config, &old.config))
        .unwrap_or_default();
    let diff = if revision.rev == current {
        String::new()
    } else {
        crate::config::diff::text_diff(
            &crate::config::diff::mask_toml(&current_text),
            &crate::config::diff::mask_toml(&revision.text),
            &format!("revision {current} (current)"),
            &format!("revision {}", revision.rev),
        )
    };
    Ok(render(&RevisionPage {
        csrf: web.csrf.clone(),
        text: crate::config::diff::mask_toml(&revision.text),
        revision,
        current,
        changes,
        diff,
    })?
    .into_response())
}

async fn config_rollback(
    State(web): State<Arc<Web>>,
    Path(rev): Path<i64>,
) -> Result<Response, WebError> {
    let Some(revision) = web
        .db(move |conn, _, _| Ok(revisions::get(conn, rev)?))
        .await?
    else {
        return Ok(not_found("such revision"));
    };
    if let Err(errors) = check(&web, &revision.text) {
        return Ok((StatusCode::UNPROCESSABLE_ENTITY, errors).into_response());
    }
    let new = store_and_write(&web, revision.text, format!("rollback to {rev}")).await?;
    Ok((
        [("hx-redirect", format!("/config?saved={new}"))],
        StatusCode::NO_CONTENT,
    )
        .into_response())
}

// ---------- API ----------

/// What happens as it happens, as server-sent events (JSON data, one
/// `Event` each). Ends when the server stops.
async fn api_events(State(web): State<Arc<Web>>) -> Response {
    use axum::response::sse::{Event as SseEvent, KeepAlive, Sse};
    use tokio::sync::broadcast::error::RecvError;

    let Some(receiver) = crate::events::subscribe() else {
        return (StatusCode::SERVICE_UNAVAILABLE, "no live events here").into_response();
    };
    let shutdown = web.shutdown.clone();
    let stream = futures_util::stream::unfold(
        (receiver, shutdown),
        |(mut receiver, shutdown)| async move {
            let data = tokio::select! {
                () = shutdown.stopping() => return None,
                got = receiver.recv() => match got {
                    Ok(event) => serde_json::to_string(&event).expect("events serialize"),
                    Err(RecvError::Lagged(_)) => r#"{"type":"changed"}"#.to_string(),
                    Err(RecvError::Closed) => return None,
                },
            };
            Some((
                Ok::<_, std::convert::Infallible>(SseEvent::default().data(data)),
                (receiver, shutdown),
            ))
        },
    );
    Sse::new(stream)
        .keep_alive(KeepAlive::default())
        .into_response()
}

async fn api_status(State(web): State<Arc<Web>>) -> Result<Json<data::Overview>, WebError> {
    overview(&web).await.map(Json)
}

async fn api_rows(web: &Arc<Web>, keep: fn(&Row) -> bool) -> Result<Json<Vec<Row>>, WebError> {
    let (_, rows) = web
        .db(|conn, _, loaded| data::rows(conn, loaded, None))
        .await?;
    Ok(Json(rows.into_iter().filter(keep).collect()))
}

async fn api_new(State(web): State<Arc<Web>>) -> Result<Json<Vec<Row>>, WebError> {
    api_rows(&web, |r| r.is_new).await
}

async fn api_missing(State(web): State<Arc<Web>>) -> Result<Json<Vec<Row>>, WebError> {
    api_rows(&web, |r| {
        matches!(r.status.as_str(), "missing" | "upcoming")
    })
    .await
}

async fn api_run(
    State(web): State<Arc<Web>>,
    Path(service): Path<String>,
) -> Result<Json<serde_json::Value>, WebError> {
    let message = run_now(&web, &service).await?;
    Ok(Json(serde_json::json!({ "message": message })))
}

// ---------- sign in, assets, guard ----------

#[derive(Template)]
#[template(path = "login.html")]
struct LoginPage {
    csrf: String,
    failed: bool,
}

async fn login_page(State(web): State<Arc<Web>>) -> Result<Html<String>, WebError> {
    render(&LoginPage {
        csrf: web.csrf.clone(),
        failed: false,
    })
}

#[derive(Deserialize)]
struct LoginForm {
    token: String,
}

async fn login(
    State(web): State<Arc<Web>>,
    Form(form): Form<LoginForm>,
) -> Result<Response, WebError> {
    if web.token.is_empty() {
        return Ok(Redirect::to("/").into_response());
    }
    if !same(&form.token, &web.token) {
        let page = render(&LoginPage {
            csrf: web.csrf.clone(),
            failed: true,
        })?;
        return Ok((StatusCode::UNAUTHORIZED, page).into_response());
    }
    let cookie = format!(
        "{SESSION_COOKIE}={}; Path=/; HttpOnly; SameSite=Strict",
        web.session()
    );
    let mut response = Redirect::to("/").into_response();
    response.headers_mut().insert(
        header::SET_COOKIE,
        HeaderValue::from_str(&cookie).expect("a valid cookie"),
    );
    Ok(response)
}

/// An asset built into the binary, and its content type.
fn asset_file(file: &str) -> Option<(&'static str, &'static str)> {
    Some(match file {
        "htmx.min.js" => (
            include_str!("../../assets/vendor/htmx.min.js"),
            "text/javascript",
        ),
        "app.css" => (include_str!("../../assets/app.css"), "text/css"),
        "live.js" => (include_str!("../../assets/live.js"), "text/javascript"),
        "downloads.js" => (include_str!("../../assets/downloads.js"), "text/javascript"),
        "events.js" => (include_str!("../../assets/events.js"), "text/javascript"),
        "events-worker.js" => (
            include_str!("../../assets/events-worker.js"),
            "text/javascript",
        ),
        "toml.js" => (include_str!("../../assets/toml.js"), "text/javascript"),
        "rss.svg" => (include_str!("../../assets/rss.svg"), "image/svg+xml"),
        _ => return None,
    })
}

/// A short tag of an asset's contents.
fn asset_tag(body: &str) -> String {
    let digest = <sha2::Sha256 as sha2::Digest>::digest(body.as_bytes());
    crate::hex::encode(&digest)[..16].to_string()
}

/// An asset's address, which changes with its contents, so browsers never
/// use a copy from an older build.
pub(crate) fn asset_url(file: &str) -> String {
    let tag = asset_file(file)
        .map(|(body, _)| asset_tag(body))
        .unwrap_or_default();
    format!("/assets/{file}?v={tag}")
}

/// An asset built into the binary. The browser checks back each time (a
/// tag of its contents), so a new build's assets show up right away.
async fn asset(Path(file): Path<String>, headers: HeaderMap) -> Response {
    let Some((body, kind)) = asset_file(&file) else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let etag = format!("\"{}\"", asset_tag(body));
    let cache = [
        (header::ETAG, etag.as_str()),
        (header::CACHE_CONTROL, "no-cache"),
    ];
    let fresh = headers
        .get(header::IF_NONE_MATCH)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| value.split(',').any(|tag| tag.trim() == etag));
    if fresh {
        return (StatusCode::NOT_MODIFIED, cache).into_response();
    }
    (cache, [(header::CONTENT_TYPE, kind)], body).into_response()
}

/// A release group's cover from the folder of an album that has it
/// (`covers.local`), or else from the Cover Art Archive.
async fn cover(State(web): State<Arc<Web>>, Path(mbid): Path<String>) -> Response {
    let thumbs = web.thumbs();
    let found = web
        .db(move |conn, _, loaded| {
            let covers = &loaded.config.covers;
            let url = data::cover_art_archive(&mbid);
            if !covers.local {
                tracing::debug!("cover {mbid}: covers.local is off; {url}");
                return Ok((url, None));
            }
            let dirs = library::dirs_with(conn, &mbid)?;
            let found = find_cover(&dirs, &covers.files);
            if found.is_none() {
                tracing::debug!(
                    "cover {mbid}: none of {:?} in {}; {url}",
                    covers.files,
                    listed(&dirs)
                );
            }
            let image = found.and_then(|(path, kind)| {
                tracing::debug!("cover {mbid}: {}", path.display());
                small(&thumbs, &path, kind)
            });
            Ok((url, image))
        })
        .await;
    match found {
        Ok((_, Some(image))) => image_response(image),
        Ok((url, None)) => Redirect::temporary(&url).into_response(),
        Err(error) => error.into_response(),
    }
}

/// An artist's picture, from their folder (`covers.local`).
async fn artist_image(State(web): State<Arc<Web>>, Path(mbid): Path<String>) -> Response {
    let thumbs = web.thumbs();
    let found = web
        .db(move |conn, _, loaded| {
            let Some(artist) = library::artist(conn, &mbid)? else {
                return Ok(None);
            };
            let covers = &loaded.config.covers;
            let image = if covers.local {
                let album_dirs = library::artist_album_dirs(conn, &mbid)?;
                let mut dirs = Vec::new();
                for dir in artist_dirs(&album_dirs) {
                    let albums = main_artists(&library::tagged_files_under(conn, &dir)?);
                    if is_artist_folder(&dir, &artist, &albums) {
                        dirs.push(dir);
                    }
                }
                let found = find_cover(&dirs, &covers.artist_files);
                match &found {
                    Some((path, _)) => {
                        tracing::debug!("artist image {mbid}: {}", path.display());
                    }
                    None if dirs.is_empty() => tracing::debug!(
                        "artist image {mbid}: no artist folder (album folders: {})",
                        listed(
                            &album_dirs
                                .iter()
                                .map(|(_, d)| d.clone())
                                .collect::<Vec<_>>()
                        )
                    ),
                    None => tracing::debug!(
                        "artist image {mbid}: none of {:?} in {}",
                        covers.artist_files,
                        listed(&dirs)
                    ),
                }
                found.and_then(|(path, kind)| small(&thumbs, &path, kind))
            } else {
                tracing::debug!("artist image {mbid}: covers.local is off");
                None
            };
            Ok(Some(image))
        })
        .await;
    match found {
        Ok(Some(Some(image))) => image_response(image),
        Ok(Some(None)) => StatusCode::NOT_FOUND.into_response(),
        Ok(None) => not_found("such artist"),
        Err(error) => error.into_response(),
    }
}

/// The folders an artist's album folders are in (inside their library, not
/// the library itself), the one with most of them first.
fn artist_dirs(album_dirs: &[(String, String)]) -> Vec<String> {
    let mut counts: Vec<(String, usize)> = Vec::new();
    for (library, dir) in album_dirs {
        let Some(parent) = std::path::Path::new(dir).parent() else {
            continue;
        };
        let library = std::path::Path::new(library);
        if parent == library || !parent.starts_with(library) {
            continue;
        }
        let parent = parent.to_string_lossy().into_owned();
        match counts.iter_mut().find(|(d, _)| *d == parent) {
            Some((_, n)) => *n += 1,
            None => counts.push((parent, 1)),
        }
    }
    counts.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    counts.into_iter().map(|(d, _)| d).collect()
}

/// How many tracks of each album to look at.
const SAMPLE: usize = 5;

/// Each album's main artists: those on most of up to `SAMPLE` of its
/// tracks (spread over it). `files` are a folder's tagged files' album
/// folders and artists, by album folder.
fn main_artists(files: &[(String, Vec<String>)]) -> Vec<Vec<String>> {
    let mut albums = Vec::new();
    for album in files.chunk_by(|a, b| a.0 == b.0) {
        let step = album.len().div_ceil(SAMPLE).max(1);
        let sample: Vec<&Vec<String>> = album.iter().step_by(step).map(|(_, a)| a).collect();
        let mut counts: Vec<(&String, usize)> = Vec::new();
        for artist in sample.iter().flat_map(|artists| artists.iter()) {
            match counts.iter_mut().find(|(a, _)| *a == artist) {
                Some((_, n)) => *n += 1,
                None => counts.push((artist, 1)),
            }
        }
        albums.push(
            counts
                .into_iter()
                .filter(|(_, n)| n * 2 > sample.len())
                .map(|(a, _)| a.clone())
                .collect(),
        );
    }
    albums
}

/// The artists who are a main artist of every album (see [`main_artists`]).
fn folder_owners(albums: &[Vec<String>]) -> Vec<String> {
    let Some((first, rest)) = albums.split_first() else {
        return Vec::new();
    };
    first
        .iter()
        .filter(|a| rest.iter().all(|album| album.contains(a)))
        .cloned()
        .collect()
}

/// Whether `dir` is `artist`'s folder, given its `albums`' main artists:
/// named after them, with an album of theirs in it ("Elvis Costello/", with
/// albums by Elvis Costello & The Attractions too); or else, theirs alone:
/// they're a main artist of every album, and nobody else is (not someone
/// else's folder with an album they're on).
fn is_artist_folder(dir: &str, artist: &library::Artist, albums: &[Vec<String>]) -> bool {
    let key = |s: &str| -> String {
        deunicode::deunicode(s)
            .to_lowercase()
            .chars()
            .filter(char::is_ascii_alphanumeric)
            .collect()
    };
    let named = std::path::Path::new(dir)
        .file_name()
        .is_some_and(|n| key(&n.to_string_lossy()) == key(&artist.name));
    if named {
        albums.iter().any(|album| album.contains(&artist.mbid))
    } else {
        folder_owners(albums) == [artist.mbid.clone()]
    }
}

fn image_response((body, kind): (Vec<u8>, &'static str)) -> Response {
    (
        [
            (header::CONTENT_TYPE, kind),
            (header::CACHE_CONTROL, "private, max-age=3600"),
        ],
        body,
    )
        .into_response()
}

/// The picture at `path` as pages get it: its thumbnail (see [`thumbs`]),
/// or nothing when it can't be read.
fn small(
    thumbs: &std::path::Path,
    path: &std::path::Path,
    kind: &'static str,
) -> Option<(Vec<u8>, &'static str)> {
    thumbs::get(thumbs, path, kind)
        .map_err(|e| tracing::debug!("{}: {e}", path.display()))
        .ok()
}

/// Folders for a log line.
fn listed(dirs: &[String]) -> String {
    if dirs.is_empty() {
        "no folders".into()
    } else {
        dirs.join(", ")
    }
}

/// The first of `names` (any case) in the first of `dirs` that has one:
/// its path and content type.
fn find_cover(dirs: &[String], names: &[String]) -> Option<(PathBuf, &'static str)> {
    for dir in dirs {
        let entries = match std::fs::read_dir(dir) {
            Ok(entries) => entries,
            Err(error) => {
                tracing::debug!("{dir}: {error}");
                continue;
            }
        };
        let files: Vec<(String, PathBuf)> = entries
            .filter_map(Result::ok)
            .filter(|e| e.file_type().is_ok_and(|t| t.is_file() || t.is_symlink()))
            .filter_map(|e| Some((e.file_name().into_string().ok()?, e.path())))
            .collect();
        for name in names {
            let Some((_, path)) = files.iter().find(|(f, _)| f.eq_ignore_ascii_case(name)) else {
                continue;
            };
            let extension = name.rsplit_once('.').map(|(_, e)| e.to_ascii_lowercase());
            let kind = match extension.as_deref() {
                Some("png") => "image/png",
                Some("webp") => "image/webp",
                Some("gif") => "image/gif",
                _ => "image/jpeg",
            };
            return Some((path.clone(), kind));
        }
    }
    None
}

/// Pages, fragments and API answers are always fetched fresh. Assets say
/// their own.
async fn no_store(mut response: Response) -> Response {
    response
        .headers_mut()
        .entry(header::CACHE_CONTROL)
        .or_insert(HeaderValue::from_static("no-store"));
    response
}

/// Host, origin, sign-in and CSRF checks, before any handler.
async fn guard(State(web): State<Arc<Web>>, request: Request, next: Next) -> Response {
    let headers = request.headers();
    let host = headers
        .get(header::HOST)
        .and_then(|h| h.to_str().ok())
        .unwrap_or_default()
        .to_string();
    if !web.hosts.is_empty() && !web.hosts.iter().any(|h| h.eq_ignore_ascii_case(&host)) {
        return (StatusCode::MISDIRECTED_REQUEST, "unknown host").into_response();
    }
    let path = request.uri().path().to_string();
    let api = path.starts_with("/api/");
    let post = request.method() == Method::POST;
    // A POST from another site's page.
    if post
        && let Some(origin) = headers.get(header::ORIGIN).and_then(|o| o.to_str().ok())
        && origin.split("://").nth(1) != Some(host.as_str())
    {
        return (StatusCode::FORBIDDEN, "cross-site request").into_response();
    }
    let bearer = headers
        .get(header::AUTHORIZATION)
        .and_then(|a| a.to_str().ok())
        .and_then(|a| a.strip_prefix("Bearer "))
        .is_some_and(|t| !web.token.is_empty() && same(t, &web.token));
    // Feed readers can't sign in, and the feed only says what came out.
    let feed =
        path == "/feed.atom" || (path.starts_with("/artists/") && path.ends_with("/feed.atom"));
    let open = path == "/login" || feed || path.starts_with("/assets/");
    if !web.token.is_empty() && !open && !bearer && !signed_in(&web, headers) {
        return if api {
            (
                StatusCode::UNAUTHORIZED,
                Json(serde_json::json!({ "error": "sign in with a bearer token" })),
            )
                .into_response()
        } else if headers.contains_key("hx-request") {
            ([("hx-redirect", "/login")], StatusCode::UNAUTHORIZED).into_response()
        } else {
            Redirect::to("/login").into_response()
        };
    }
    if post && path != "/login" && !bearer {
        let csrf_ok = headers
            .get("x-csrf-token")
            .and_then(|t| t.to_str().ok())
            .is_some_and(|t| same(t, &web.csrf));
        let json = headers
            .get(header::CONTENT_TYPE)
            .and_then(|c| c.to_str().ok())
            .is_some_and(|c| c.starts_with("application/json"));
        if !(csrf_ok || (api && json)) {
            return (
                StatusCode::FORBIDDEN,
                if api {
                    "send Content-Type: application/json (or a bearer token)"
                } else {
                    "missing CSRF token; reload the page"
                },
            )
                .into_response();
        }
    }
    next.run(request).await
}

fn signed_in(web: &Web, headers: &HeaderMap) -> bool {
    let session = web.session();
    headers
        .get_all(header::COOKIE)
        .iter()
        .filter_map(|c| c.to_str().ok())
        .flat_map(|c| c.split(';'))
        .filter_map(|c| c.trim().split_once('='))
        .any(|(name, value)| name == SESSION_COOKIE && same(value, &session))
}

/// Compare secrets in constant time.
fn same(a: &str, b: &str) -> bool {
    a.len() == b.len()
        && a.bytes()
            .zip(b.bytes())
            .fold(0u8, |diff, (x, y)| diff | (x ^ y))
            == 0
}

pub(crate) fn random_hex(bytes: usize) -> String {
    use std::io::Read;
    let mut buf = vec![0u8; bytes];
    std::fs::File::open("/dev/urandom")
        .and_then(|mut f| f.read_exact(&mut buf))
        .expect("/dev/urandom");
    crate::hex::encode(&buf)
}

#[cfg(test)]
mod tests;
