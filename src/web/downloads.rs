//! The Downloads page: every folder queued in slskd, and the ones waiting
//! for you (downloaded, but not filed): tag them as a release you pick, or
//! say you dealt with them.

use std::collections::HashMap;
use std::sync::Arc;

use askama::Template;
use axum::extract::{Form, Path, State};
use axum::response::{IntoResponse, Response};
use serde::Deserialize;

use super::download::{Setup, setup};
use super::{Web, WebError, not_found, refresh, render};
use crate::daemon::service::{Service, local};
use crate::matching::{self, Candidate};
use crate::state::downloads::{
    self, CANCELLED, DOWNLOADING, Download, FAILED, FILED, HANDLED, MANUAL,
};

#[derive(Template)]
#[template(path = "downloads.html")]
struct DownloadsPage {
    csrf: String,
    /// Finished downloads get filed.
    filing: bool,
    sections: Vec<Section>,
    /// Nothing's been queued.
    empty: bool,
}

struct Section {
    title: &'static str,
    hint: &'static str,
    rows: Vec<Row>,
}

struct Row {
    id: i64,
    key: String,
    album: String,
    artist: String,
    folder: String,
    username: String,
    tracks: usize,
    state: String,
    detail: Option<String>,
    match_kind: String,
    release: Option<String>,
    match_reason: Option<String>,
    /// Where its files are now: in slskd's downloads folder, or filed.
    path: Option<String>,
    queued_at: String,
}

/// The Downloads page.
pub(super) async fn page(State(web): State<Arc<Web>>) -> Result<Response, WebError> {
    let found = web
        .db(|conn, _, loaded| {
            let all = downloads::all(conn)?;
            let (_, rows) = super::data::rows(conn, loaded, None)?;
            let names: HashMap<String, (String, String)> = rows
                .into_iter()
                .map(|r| (r.key, (r.title, r.artist)))
                .collect();
            let downloads_dir = loaded.import.as_ref().map(|i| i.downloads.clone());
            Ok((all, names, downloads_dir, loaded.config.slskd.enabled))
        })
        .await?;
    let (all, names, downloads_dir, enabled) = found;
    if !enabled {
        return Ok(super::download::off());
    }
    let row = |d: Download| {
        let (album, artist) = names
            .get(&d.release_key)
            .cloned()
            .unwrap_or_else(|| ("Unknown album".into(), String::new()));
        let path = if d.state == FILED {
            d.filed_to.clone()
        } else {
            downloads_dir.as_ref().map(|dir| {
                let dest = d.destination.clone().unwrap_or_else(|| {
                    d.dir
                        .rsplit('\\')
                        .next()
                        .unwrap_or(&d.dir)
                        .replace(['/', '\0'], "_")
                });
                dir.join(dest).display().to_string()
            })
        };
        Row {
            id: d.id,
            key: d.release_key.clone(),
            album,
            artist,
            folder: d.dir.rsplit('\\').next().unwrap_or(&d.dir).to_string(),
            username: d.username.clone(),
            tracks: d.files.len(),
            detail: d.detail.clone(),
            match_kind: d.matched.kind.clone(),
            release: d.matched.release_label.clone(),
            match_reason: d.matched.reason.clone(),
            path,
            queued_at: local(d.queued_at),
            state: d.state,
        }
    };
    let mut sections = vec![
        Section {
            title: "Needs you",
            hint: "Downloaded, but not filed: tag it as the right release, or deal with it yourself",
            rows: Vec::new(),
        },
        Section {
            title: "Downloading",
            hint: "Filed once every file is there",
            rows: Vec::new(),
        },
        Section {
            title: "Filed",
            hint: "Tagged and moved into your library",
            rows: Vec::new(),
        },
        Section {
            title: "Dealt with",
            hint: "You took care of these, or cancelled them",
            rows: Vec::new(),
        },
    ];
    for d in all {
        let i = match d.state.as_str() {
            MANUAL => 0,
            FILED => 2,
            HANDLED | CANCELLED => 3,
            _ => 1,
        };
        sections[i].rows.push(row(d));
    }
    Ok(render(&DownloadsPage {
        csrf: web.csrf.clone(),
        filing: downloads_dir.is_some(),
        empty: sections.iter().all(|s| s.rows.is_empty()),
        sections,
    })?
    .into_response())
}

#[derive(Template)]
#[template(path = "partials/download_releases.html")]
struct ReleasesFragment {
    id: i64,
    files: usize,
    releases: Vec<ReleaseChoice>,
    error: Option<String>,
}

struct ReleaseChoice {
    mbid: String,
    label: String,
    tracks: usize,
    /// It has at least as many tracks as there are files.
    fits: bool,
    /// The one it matched (or was tagged as) before.
    current: bool,
}

/// The releases download `id` could be tagged as (its release group's).
pub(super) async fn releases(
    State(web): State<Arc<Web>>,
    Path(id): Path<i64>,
) -> Result<Response, WebError> {
    let Some(Setup { musicbrainz, .. }) = setup(&web).await? else {
        return Ok(super::download::off());
    };
    let Some(download) = web
        .db(move |conn, _, _| Ok(downloads::get(conn, id)?))
        .await?
    else {
        return Ok(not_found("such download"));
    };
    let (releases, error) =
        match super::download::releases(&download.release_key, &musicbrainz).await {
            Ok(releases) if releases.is_empty() => (
                Vec::new(),
                Some("MusicBrainz has no releases for it: tag it yourself".into()),
            ),
            Ok(releases) => (releases, None),
            Err(e) => (
                Vec::new(),
                Some(format!("couldn't get its releases: {e:#}")),
            ),
        };
    let files = download.files.len();
    let mut releases: Vec<ReleaseChoice> = releases
        .iter()
        .map(|r| ReleaseChoice {
            current: download.matched.release_mbid.as_deref() == Some(r.mbid.as_str()),
            mbid: r.mbid.clone(),
            label: super::download::release_label(r),
            tracks: r.tracks.len(),
            fits: r.tracks.len() >= files,
        })
        .collect();
    // As many tracks first, then those it's part of; the rest last.
    releases.sort_by_key(|r| (!r.fits, r.tracks != files));
    Ok(render(&ReleasesFragment {
        id,
        files,
        releases,
        error,
    })?
    .into_response())
}

#[derive(Deserialize)]
pub(super) struct FileForm {
    release: String,
}

/// Tag download `id` as the release you picked (its files in track order),
/// and file it.
pub(super) async fn file(
    State(web): State<Arc<Web>>,
    Path(id): Path<i64>,
    Form(form): Form<FileForm>,
) -> Result<Response, WebError> {
    let Some(Setup { musicbrainz, .. }) = setup(&web).await? else {
        return Ok(super::download::off());
    };
    let found = web
        .db(move |conn, _, loaded| Ok((downloads::get(conn, id)?, loaded.clone())))
        .await?;
    let (Some(download), Some(paths)) = (found.0, found.1.import.clone()) else {
        return Ok(not_found(
            "such download, or slskd.downloads_dir and output_dir",
        ));
    };
    let releases = super::download::releases(&download.release_key, &musicbrainz)
        .await
        .map_err(WebError)?;
    let Some(release) = releases.iter().find(|r| r.mbid == form.release) else {
        return Ok(not_found("such release of it"));
    };
    let candidates: Vec<Candidate> = download
        .files
        .iter()
        .map(|(filename, _)| Candidate {
            filename: filename.clone(),
            length: None,
        })
        .collect();
    let now = jiff::Timestamp::now();
    let tracks = match matching::assign(&candidates, release) {
        Ok(tracks) => tracks,
        Err(why) => {
            web.db(move |conn, _, _| {
                downloads::set_state(conn, id, MANUAL, Some(&why), None, now)?;
                Ok(())
            })
            .await?;
            return Ok(refresh());
        }
    };
    let matched = downloads::Matched {
        kind: "chosen".into(),
        release_mbid: Some(release.mbid.clone()),
        release_label: Some(super::download::release_label(release)),
        reason: None,
        tracks,
    };
    let key = download.release_key.clone();
    let mut download = download;
    download.matched = matched.clone();
    let http = crate::http::Http::new(musicbrainz.contact.as_deref());
    let filed =
        crate::import::file_download(&http, &musicbrainz.server, &download, &paths, true).await;
    let filed_to = filed.as_ref().ok().cloned();
    web.db(move |conn, _, _| {
        downloads::set_matched(conn, id, &matched)?;
        match filed {
            Ok(dir) => {
                let dir = dir.display().to_string();
                tracing::info!("filed {} into {dir}", download.dir);
                downloads::set_state(conn, id, FILED, None, Some(&dir), now)?;
            }
            Err(why) => {
                downloads::set_state(conn, id, MANUAL, Some(&why), None, now)?;
            }
        }
        Ok(())
    })
    .await?;
    super::download::announce(&key, if filed_to.is_some() { FILED } else { MANUAL });
    if let Some(dir) = filed_to {
        crate::jellyfin::tell(&found.1, std::slice::from_ref(&dir)).await;
        // Its folder now (not at the next full scan): it's yours right away.
        let path = web.db.clone();
        let home = web.dirs.home.clone();
        tokio::task::spawn_blocking(move || {
            let mut state = crate::state::State::open(&path)?;
            let (_, loaded) = crate::config::store::load_current(&state.conn, &home)?;
            crate::scan::scan_new_album(&mut state.conn, &loaded.libraries, &dir)
        })
        .await
        .map_err(|e| WebError(anyhow::anyhow!(e)))?
        .map_err(WebError)?;
        // Scanned: its rows can say it's yours.
        super::download::announce(&key, FILED);
        web.wakes.wake(Service::Resolver);
    }
    Ok(refresh())
}

/// Cancel download `id` (still downloading, or failed): its files in
/// slskd, and the folder newcut had slskd put them in.
pub(super) async fn cancel(
    State(web): State<Arc<Web>>,
    Path(id): Path<i64>,
) -> Result<Response, WebError> {
    let Some(Setup { slskd: config, .. }) = setup(&web).await? else {
        return Ok(super::download::off());
    };
    let found = web
        .db(move |conn, _, loaded| Ok((downloads::get(conn, id)?, loaded.import.clone())))
        .await?;
    let (Some(download), paths) = found else {
        return Ok(not_found("such download"));
    };
    if download.state != DOWNLOADING && download.state != FAILED {
        return Ok(refresh());
    }
    let cancelled = async {
        let client = crate::slskd::Slskd::new(&config)?;
        let transfers = client.transfers(&download.username).await?;
        let mut n = 0;
        for transfer in transfers
            .iter()
            .filter(|t| download.files.iter().any(|(f, _)| *f == t.filename))
        {
            client.cancel(&download.username, &transfer.id).await?;
            n += 1;
        }
        Ok::<_, anyhow::Error>(n)
    }
    .await;
    let n = match cancelled {
        Ok(n) => n,
        Err(e) => {
            tracing::warn!("cancelling {}: {e:#}", download.dir);
            return Ok(
                (axum::http::StatusCode::BAD_GATEWAY, format!("slskd: {e:#}")).into_response(),
            );
        }
    };
    tracing::info!("cancelled {n} files of {} in slskd", download.dir);
    // The folder newcut picked for it: nothing else is in there.
    if let (Some(paths), Some(destination)) = (paths, download.destination.as_deref())
        && destination.starts_with("newcut/")
    {
        let dir = paths.downloads.join(destination);
        match std::fs::remove_dir_all(&dir) {
            Ok(()) => tracing::info!("removed {}", dir.display()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => tracing::warn!("can't remove {}: {e}", dir.display()),
        }
    }
    let key = download.release_key.clone();
    web.db(move |conn, _, _| {
        downloads::set_state(conn, id, CANCELLED, None, None, jiff::Timestamp::now())?;
        Ok(())
    })
    .await?;
    super::download::announce(&key, CANCELLED);
    Ok(refresh())
}

/// Download `id`: `handled` (you dealt with it; its artists' folders are
/// scanned for it) or `retry` (file it again, as it matched: after you
/// freed its place, say).
pub(super) async fn action(
    State(web): State<Arc<Web>>,
    Path((id, action)): Path<(i64, String)>,
) -> Result<Response, WebError> {
    let state = match action.as_str() {
        "handled" => HANDLED,
        "retry" => downloads::DOWNLOADING,
        _ => return Ok(not_found("such action")),
    };
    let changed = web
        .db(move |conn, _, _| {
            let Some(download) = downloads::get(conn, id)? else {
                return Ok(None);
            };
            if !downloads::set_state(conn, id, state, None, None, jiff::Timestamp::now())? {
                return Ok(None);
            }
            super::download::announce(&download.release_key, state);
            let artists = crate::state::library::group_artist_ids(conn, &download.release_key)?;
            Ok(Some((download.release_key, artists)))
        })
        .await?;
    match changed {
        Some((key, artists)) if state == HANDLED => {
            // You put its files in place, most likely in its artists'
            // folders: those are scanned (not the whole library).
            let mut found = false;
            for artist in &artists {
                found |= super::scan_artist(&web, artist).await?;
            }
            if found {
                web.wakes.wake(Service::Resolver);
            }
            // Scanned: its rows can say it's yours.
            super::download::announce(&key, HANDLED);
        }
        Some(_) => web.wakes.wake(Service::Importer),
        None => {}
    }
    Ok(refresh())
}
