//! Downloading a missing album through slskd: a page that searches for it,
//! and queues the folder you pick.

use std::collections::HashMap;
use std::sync::Arc;

use askama::Template;
use axum::extract::{Form, Path, State};
use axum::http::StatusCode;
use axum::response::{Html, IntoResponse, Response};
use serde::Deserialize;

use super::data::{self, Row};
use super::{Web, WebError, not_found, render};
use crate::config::{MusicBrainzConfig, SlskdConfig};
use crate::matching;
use crate::musicbrainz::Release;
use crate::slskd::{self, Slskd};
use crate::state::downloads;

/// The folders a search page lists, at most.
const SHOWN: usize = 100;

#[derive(Template)]
#[template(path = "download.html")]
struct DownloadPage {
    csrf: String,
    row: Row,
    query: String,
    /// Search right away: nothing's been queued for it yet.
    search_now: bool,
}

#[derive(Template)]
#[template(path = "partials/download_results.html")]
struct Results {
    key: String,
    id: String,
    view: ResultsView,
}

enum ResultsView {
    Error(String),
    /// Still searching: so many users answered, with so many files.
    Searching(u64, u64),
    Done {
        folders: Vec<FolderView>,
        total: usize,
        users: usize,
        /// Folders that match a release exactly.
        exact: usize,
        /// The release group's releases.
        releases: usize,
        /// Why folders couldn't be matched at all.
        unmatched: Option<String>,
    },
}

struct FolderView {
    name: String,
    username: String,
    dir: String,
    tracks: usize,
    size: String,
    length: Option<String>,
    format: String,
    quality: Option<String>,
    lossless: bool,
    free_slot: bool,
    queue: u64,
    speed: String,
    kind: matching::Kind,
    /// The release it matches.
    release: Option<String>,
    /// How many other releases it matches as well.
    alike: usize,
    reason: Option<String>,
    files: Vec<FileView>,
    /// What queueing it posts (`hx-vals`).
    vals: String,
}

/// A file in a folder, and the release track it matches.
struct FileView {
    name: String,
    length: Option<String>,
    size: String,
    /// "3. Title (3:20)": the matched track on the release.
    track: Option<String>,
}

impl FolderView {
    fn new(folder: &slskd::Folder, m: &matching::Match, releases: &[Release]) -> Self {
        Self {
            kind: m.kind,
            release: m.release.map(|i| release_label(&releases[i])),
            alike: m.alike,
            reason: m.reason.clone(),
            files: file_views(folder, m, releases),
            name: folder.name().to_string(),
            username: folder.username.clone(),
            dir: folder.dir.clone(),
            tracks: folder.files.len(),
            size: bytes(folder.size),
            length: folder.length.map(duration),
            format: folder.format.clone(),
            quality: folder.quality.clone(),
            lossless: folder.lossless,
            free_slot: folder.free_slot,
            queue: folder.queue,
            speed: format!("{}/s", bytes(folder.speed)),
            vals: serde_json::json!({ "username": folder.username, "dir": folder.dir }).to_string(),
        }
    }
}

fn file_views(folder: &slskd::Folder, m: &matching::Match, releases: &[Release]) -> Vec<FileView> {
    let release = m.release.map(|i| &releases[i]);
    folder
        .files
        .iter()
        .zip(&folder.lengths)
        .zip(&m.tracks)
        .map(|(((filename, size), length), track)| {
            let track = track.as_deref().and_then(|id| {
                let tracks = &release?.tracks;
                let t = tracks.iter().find(|t| t.mbid == id)?;
                let multi = tracks.last().is_some_and(|last| last.disc > 1);
                let number = if multi {
                    format!("{}-{}", t.disc, t.position)
                } else {
                    t.position.to_string()
                };
                Some(match t.length_ms {
                    Some(ms) => format!("{number}. {} ({})", t.title, duration(ms.div_ceil(1000))),
                    None => format!("{number}. {}", t.title),
                })
            });
            FileView {
                name: filename.rsplit('\\').next().unwrap_or(filename).to_string(),
                length: length.map(|l| duration(u64::from(l))),
                size: bytes(*size),
                track,
            }
        })
        .collect()
}

fn bytes(n: u64) -> String {
    #[allow(clippy::cast_precision_loss)]
    let n = n as f64;
    if n >= 1e9 {
        format!("{:.1} GB", n / 1e9)
    } else if n >= 1e6 {
        format!("{:.0} MB", n / 1e6)
    } else {
        format!("{:.0} KB", n / 1e3)
    }
}

fn duration(seconds: u64) -> String {
    let (h, m, s) = (seconds / 3600, seconds / 60 % 60, seconds % 60);
    if h > 0 {
        format!("{h}:{m:02}:{s:02}")
    } else {
        format!("{m}:{s:02}")
    }
}

/// The slskd config, when it's on, and MusicBrainz's.
async fn config(web: &Arc<Web>) -> Result<Option<(SlskdConfig, MusicBrainzConfig)>, WebError> {
    Ok(setup(web).await?.map(|s| (s.slskd, s.musicbrainz)))
}

/// What the download pages need from the config.
pub(super) struct Setup {
    pub slskd: SlskdConfig,
    pub musicbrainz: MusicBrainzConfig,
    /// Finished downloads get filed (`slskd.downloads_dir` and
    /// `output_dir` are set).
    pub filing: bool,
}

/// The config's slskd setup, when slskd's on.
pub(super) async fn setup(web: &Arc<Web>) -> Result<Option<Setup>, WebError> {
    web.db(|_, _, loaded| {
        let config = &loaded.config;
        Ok(config.slskd.enabled.then(|| Setup {
            slskd: config.slskd.clone(),
            musicbrainz: config.musicbrainz.clone(),
            filing: loaded.import.is_some(),
        }))
    })
    .await
}

/// The releases of release group `key`.
pub(super) async fn releases(
    key: &str,
    musicbrainz: &MusicBrainzConfig,
) -> anyhow::Result<Vec<Release>> {
    if !crate::scan::tags::is_mbid(key) {
        return Ok(Vec::new());
    }
    let http = crate::http::Http::new(musicbrainz.contact.as_deref());
    crate::musicbrainz::MusicBrainz {
        http: &http,
        server: &musicbrainz.server,
    }
    .releases_of_group(key)
    .await
}

/// What a release is, in a few words: "CD · US · 2021-03-05 · deluxe".
pub(super) fn release_label(release: &Release) -> String {
    [
        Some(release.formats.clone()).filter(|f| !f.is_empty()),
        release.country.clone(),
        release.date.clone(),
        release.disambiguation.clone(),
    ]
    .into_iter()
    .flatten()
    .collect::<Vec<_>>()
    .join(" · ")
}

/// A folder's match, to store with its download.
fn matched(m: &matching::Match, releases: &[Release]) -> downloads::Matched {
    let release = m.release.map(|i| &releases[i]);
    downloads::Matched {
        kind: m.kind.key().into(),
        release_mbid: release.map(|r| r.mbid.clone()),
        release_label: release.map(release_label),
        reason: m.reason.clone(),
        tracks: m.tracks.clone(),
    }
}

pub(super) fn off() -> Response {
    (
        StatusCode::NOT_FOUND,
        "slskd isn't set up: see [slskd] in the config",
    )
        .into_response()
}

/// The page for a missing album (`key`: its release group's id).
pub(super) async fn page(
    State(web): State<Arc<Web>>,
    Path(key): Path<String>,
) -> Result<Response, WebError> {
    if config(&web).await?.is_none() {
        return Ok(off());
    }
    let row = web
        .db(move |conn, _, loaded| {
            let (_, rows) = data::rows(conn, loaded, None)?;
            let queued = downloads::queued_releases(conn)?.contains(&key);
            Ok(rows
                .into_iter()
                .find(|r| r.key == key)
                .map(|row| (row, queued)))
        })
        .await?;
    let Some((row, queued)) = row else {
        return Ok(not_found("such release"));
    };
    Ok(render(&DownloadPage {
        csrf: web.csrf.clone(),
        search_now: !queued,
        query: search_text(&row),
        row,
    })?
    .into_response())
}

/// What to search Soulseek for: the artist and the title, without what
/// Soulseek's search trips on (it matches words; punctuation only gets in
/// the way).
fn search_text(row: &Row) -> String {
    format!("{} {}", row.artist, row.title)
        .chars()
        .map(|c| {
            if c.is_alphanumeric() || c == '\'' {
                c
            } else {
                ' '
            }
        })
        .collect::<String>()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

#[derive(Deserialize)]
pub(super) struct StartForm {
    #[serde(default)]
    q: String,
}

/// Start a search; the answer polls it until it's done.
pub(super) async fn start(
    State(web): State<Arc<Web>>,
    Path(key): Path<String>,
    Form(form): Form<StartForm>,
) -> Result<Response, WebError> {
    let Some((config, _)) = config(&web).await? else {
        return Ok(off());
    };
    if !is_key(&key) {
        return Ok(not_found("such release"));
    }
    let q = form.q.trim();
    if q.is_empty() {
        return Ok(Html(String::new()).into_response());
    }
    let started = match Slskd::new(&config) {
        Ok(client) => client.start_search(q).await,
        Err(error) => Err(error),
    };
    Ok(match started {
        Ok(id) => render(&Results {
            key,
            id,
            view: ResultsView::Searching(0, 0),
        })?,
        Err(error) => error_view(key, &error)?,
    }
    .into_response())
}

fn error_view(key: String, error: &anyhow::Error) -> Result<Html<String>, WebError> {
    tracing::warn!("slskd: {error:#}");
    render(&Results {
        key,
        id: String::new(),
        view: ResultsView::Error(format!("{error:#}")),
    })
}

/// How search `id` is going, or what it found.
pub(super) async fn poll(
    State(web): State<Arc<Web>>,
    Path((key, id)): Path<(String, String)>,
) -> Result<Response, WebError> {
    let Some((config, musicbrainz)) = config(&web).await? else {
        return Ok(off());
    };
    if !is_key(&key) {
        return Ok(not_found("such release"));
    }
    let view = async {
        let client = Slskd::new(&config)?;
        let Some(state) = client.search(&id).await? else {
            anyhow::bail!("slskd doesn't have this search anymore; search again");
        };
        if !state.complete {
            return Ok(ResultsView::Searching(state.responses, state.files));
        }
        let responses = client.responses(&id).await?;
        let folders = slskd::folders(&responses);
        let (releases, unmatched) = match releases(&key, &musicbrainz).await {
            Ok(releases) => (releases, None),
            Err(e) => {
                tracing::warn!("releases of {key}: {e:#}");
                (
                    Vec::new(),
                    Some(format!("couldn't get its releases: {e:#}")),
                )
            }
        };
        let mut views: Vec<FolderView> = folders
            .iter()
            .map(|folder| {
                let m = matching::match_folder(&folder.candidates(), &releases);
                FolderView::new(folder, &m, &releases)
            })
            .collect();
        // Matches first; the rest as they were (the best sources first).
        views.sort_by_key(|v| v.kind);
        let exact = views
            .iter()
            .filter(|v| v.kind == matching::Kind::Exact)
            .count();
        Ok(ResultsView::Done {
            total: views.len(),
            users: responses.len(),
            exact,
            releases: releases.len(),
            unmatched,
            folders: views.into_iter().take(SHOWN).collect(),
        })
    }
    .await;
    Ok(match view {
        Ok(view) => render(&Results { key, id, view })?,
        Err(error) => error_view(key, &error)?,
    }
    .into_response())
}

#[derive(Deserialize)]
pub(super) struct QueueForm {
    username: String,
    dir: String,
}

/// Queue a folder a search found (its audio files, as the search saw
/// them), and remember it for the release.
pub(super) async fn queue(
    State(web): State<Arc<Web>>,
    Path((key, id)): Path<(String, String)>,
    Form(form): Form<QueueForm>,
) -> Result<Response, WebError> {
    let Some(Setup {
        slskd: config,
        musicbrainz,
        filing,
    }) = setup(&web).await?
    else {
        return Ok(off());
    };
    if !is_key(&key) {
        return Ok(not_found("such release"));
    }
    // Where slskd puts it, for filing: a folder of its own.
    let destination = filing.then(|| format!("newcut/{}", super::random_hex(8)));
    let queued = async {
        let client = Slskd::new(&config)?;
        let responses = client.responses(&id).await?;
        let folder = slskd::folders(&responses)
            .into_iter()
            .find(|f| f.username == form.username && f.dir == form.dir)
            .ok_or_else(|| anyhow::anyhow!("that folder isn't in the search anymore"))?;
        // Matched again, rather than trusting what the page said.
        let releases = releases(&key, &musicbrainz).await.unwrap_or_else(|e| {
            tracing::warn!("releases of {key}: {e:#}");
            Vec::new()
        });
        let m = matching::match_folder(&folder.candidates(), &releases);
        let matched = matched(&m, &releases);
        match &destination {
            Some(destination) => {
                client
                    .enqueue_into(&folder.username, &folder.files, destination)
                    .await?;
            }
            None => client.enqueue(&folder.username, &folder.files).await?,
        }
        tracing::info!(
            "slskd: queued {} files from {} ({}) for {key}",
            folder.files.len(),
            folder.username,
            folder.dir
        );
        Ok::<_, anyhow::Error>((folder, matched))
    }
    .await;
    let (folder, matched) = match queued {
        Ok(queued) => queued,
        Err(error) => {
            tracing::warn!("slskd: {error:#}");
            let queued = Err(format!("{error:#}"));
            return Ok(render(&Queued { queued })?.into_response());
        }
    };
    let n = folder.files.len();
    let announced = key.clone();
    web.db(move |conn, _, _| {
        downloads::add(
            conn,
            &downloads::NewDownload {
                release_key: &key,
                username: &folder.username,
                dir: &folder.dir,
                files: &folder.files,
                matched: &matched,
                destination: destination.as_deref(),
            },
            jiff::Timestamp::now(),
        )?;
        Ok(())
    })
    .await?;
    announce(&announced, downloads::DOWNLOADING);
    Ok((
        [("hx-trigger", "downloads-changed")],
        render(&Queued { queued: Ok(n) })?,
    )
        .into_response())
}

/// Tell pages showing release `key`'s downloads (in other tabs, say) that
/// one changed.
pub(super) fn announce(key: &str, state: &str) {
    crate::events::emit(crate::events::Event::Download {
        key: key.to_string(),
        state: state.to_string(),
    });
}

/// A release's key: its release group's MBID.
fn is_key(key: &str) -> bool {
    crate::scan::tags::is_mbid(key)
}

#[derive(Template)]
#[template(path = "partials/downloads.html")]
struct DownloadsFragment {
    key: String,
    downloads: Vec<DownloadView>,
    error: Option<String>,
    /// Something's still waiting or downloading: check again soon.
    pending: bool,
}

struct DownloadView {
    id: i64,
    name: String,
    username: String,
    dir: String,
    tracks: usize,
    size: String,
    queued_at: String,
    /// `ok`, `active`, `queued`, `failed`, `gone`; none when slskd couldn't
    /// say.
    status: Option<&'static str>,
    label: String,
    percent: u64,
    speed: Option<String>,
    /// exact, likely or none
    match_kind: String,
    release: Option<String>,
    match_reason: Option<String>,
    /// downloading, failed, manual, filed or handled.
    state: String,
    detail: Option<String>,
    filed_to: Option<String>,
}

/// The folders queued for release `key`, and how they're doing in slskd.
pub(super) async fn downloads(
    State(web): State<Arc<Web>>,
    Path(key): Path<String>,
) -> Result<Response, WebError> {
    let Some((config, _)) = config(&web).await? else {
        return Ok(off());
    };
    let id = key.clone();
    let stored = web
        .db(move |conn, _, _| Ok(downloads::for_release(conn, &id)?))
        .await?;
    let mut error = None;
    let mut transfers: HashMap<String, Vec<slskd::Transfer>> = HashMap::new();
    if !stored.is_empty() {
        let fetched = async {
            let client = Slskd::new(&config)?;
            for download in &stored {
                if !transfers.contains_key(&download.username) {
                    let found = client.transfers(&download.username).await?;
                    transfers.insert(download.username.clone(), found);
                }
            }
            Ok::<_, anyhow::Error>(())
        }
        .await;
        if let Err(e) = fetched {
            tracing::warn!("slskd: {e:#}");
            error = Some(format!("{e:#}"));
        }
    }
    let views: Vec<DownloadView> = stored
        .iter()
        .map(|d| {
            let progress = (error.is_none()).then(|| {
                slskd::progress(
                    &d.files,
                    transfers.get(&d.username).map_or(&[][..], Vec::as_slice),
                )
            });
            DownloadView {
                match_kind: d.matched.kind.clone(),
                release: d.matched.release_label.clone(),
                match_reason: d.matched.reason.clone(),
                state: d.state.clone(),
                detail: d.detail.clone(),
                filed_to: d.filed_to.clone(),
                id: d.id,
                name: d.dir.rsplit('\\').next().unwrap_or(&d.dir).to_string(),
                username: d.username.clone(),
                dir: d.dir.clone(),
                tracks: d.files.len(),
                size: bytes(d.files.iter().map(|(_, size)| size).sum()),
                queued_at: crate::daemon::service::local(d.queued_at),
                status: progress.as_ref().map(slskd::Progress::status),
                label: progress
                    .as_ref()
                    .map_or_else(|| "Unknown".into(), slskd::Progress::label),
                percent: progress.as_ref().map_or(0, slskd::Progress::percent),
                #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
                speed: progress
                    .as_ref()
                    .filter(|p| p.active > 0 && p.speed > 0.0)
                    .map(|p| format!("{}/s", bytes(p.speed as u64))),
            }
        })
        .collect();
    let pending = views
        .iter()
        .any(|v| matches!(v.status, Some("active" | "queued")));
    Ok(render(&DownloadsFragment {
        key,
        downloads: views,
        error,
        pending,
    })?
    .into_response())
}

/// Forget a queued folder (slskd keeps it).
pub(super) async fn forget(
    State(web): State<Arc<Web>>,
    Path((key, id)): Path<(String, i64)>,
) -> Result<Response, WebError> {
    let announced = key.clone();
    web.db(move |conn, _, _| Ok(downloads::remove(conn, &key, id)?))
        .await?;
    announce(&announced, "forgotten");
    Ok((
        [("hx-trigger", "downloads-changed")],
        StatusCode::NO_CONTENT,
    )
        .into_response())
}

#[derive(Template)]
#[template(
    source = r#"{% match queued %}{% when Ok(n) %}<span class="pill st-owned">Queued {{ n }} file{% if *n != 1 %}s{% endif %}</span>{% when Err(error) %}<span class="pill st-missing" title="{{ error }}">Couldn't queue: {{ error }}</span>{% endmatch %}"#,
    ext = "html"
)]
struct Queued {
    queued: Result<usize, String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formats_sizes_and_lengths() {
        assert_eq!(bytes(1_500_000_000), "1.5 GB");
        assert_eq!(bytes(312_400_000), "312 MB");
        assert_eq!(bytes(2_000), "2 KB");
        assert_eq!(duration(59), "0:59");
        assert_eq!(duration(2_712), "45:12");
        assert_eq!(duration(3_725), "1:02:05");
    }
}
