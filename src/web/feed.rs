//! `/feed.atom`: new releases by the artists you follow that you don't have
//! (and didn't ignore), newest found first; `/artists/{mbid}/feed.atom`: the
//! same, for one artist.

use std::sync::Arc;

use askama::Template;
use axum::extract::{Path, State};
use axum::http::{HeaderMap, header};
use axum::response::{IntoResponse, Response};

use super::{Web, WebError, data};

/// One feed entry.
pub struct Item {
    pub id: String,
    pub title: String,
    pub link: String,
    pub updated: String,
    pub summary: String,
}

#[derive(Template)]
#[template(path = "feed.atom", escape = "xml")]
struct Feed {
    base: String,
    /// This feed's path, and the page it's about.
    path: String,
    page: String,
    title: String,
    subtitle: String,
    updated: String,
    items: Vec<Item>,
}

/// The newest this many.
const ITEMS: usize = 100;

pub(super) async fn atom(
    State(web): State<Arc<Web>>,
    headers: HeaderMap,
) -> Result<Response, WebError> {
    feed(&web, &headers, None).await
}

pub(super) async fn artist_atom(
    State(web): State<Arc<Web>>,
    Path(mbid): Path<String>,
    headers: HeaderMap,
) -> Result<Response, WebError> {
    feed(&web, &headers, Some(mbid)).await
}

async fn feed(
    web: &Arc<Web>,
    headers: &HeaderMap,
    artist: Option<String>,
) -> Result<Response, WebError> {
    let host = headers
        .get(header::HOST)
        .and_then(|h| h.to_str().ok())
        .unwrap_or("localhost")
        .to_string();
    let found = web
        .db(move |conn, _, loaded| {
            let name = match &artist {
                None => None,
                Some(mbid) => match crate::state::library::artist(conn, mbid)? {
                    Some(a) => Some((a.mbid, a.name)),
                    None => return Ok(None),
                },
            };
            let artist = artist.as_deref();
            let (_, rows) = data::rows(conn, loaded, artist)?;
            let mut items: Vec<Item> = rows
                .into_iter()
                .filter(|r| r.is_new)
                .map(|r| {
                    let updated = r.found_at.to_string();
                    let id = r.mbid.as_ref().map_or_else(
                        || format!("urn:newcut:{}", r.key),
                        |m| format!("urn:musicbrainz:release-group:{m}"),
                    );
                    let date = r.date.clone().unwrap_or_else(|| "date unknown".into());
                    Item {
                        id,
                        title: format!("{} — {}", r.artist, r.title),
                        link: r.link.clone(),
                        updated,
                        summary: format!("{} · {} · {}", r.kind, date, r.label),
                    }
                })
                .collect();
            items.sort_by(|a, b| b.updated.cmp(&a.updated));
            items.truncate(ITEMS);
            Ok(Some((name, items)))
        })
        .await?;
    let Some((name, rows)) = found else {
        return Ok(super::not_found("such artist"));
    };
    let (path, page, title, subtitle) = match name {
        None => (
            "/feed.atom".to_string(),
            "/missing?status=new".to_string(),
            "newcut: new releases".to_string(),
            "New releases by the artists in your library that you don't have yet".to_string(),
        ),
        Some((mbid, name)) => (
            format!("/artists/{mbid}/feed.atom"),
            format!("/artists/{mbid}"),
            format!("newcut: new releases by {name}"),
            format!("New releases by {name} that you don't have yet"),
        ),
    };
    let updated = rows.first().map_or_else(
        || jiff::Timestamp::UNIX_EPOCH.to_string(),
        |i| i.updated.clone(),
    );
    let body = Feed {
        base: format!("http://{host}"),
        path,
        page,
        title,
        subtitle,
        updated,
        items: rows,
    }
    .render()
    .map_err(|e| WebError(anyhow::anyhow!(e)))?;
    Ok((
        [(header::CONTENT_TYPE, "application/atom+xml; charset=utf-8")],
        body,
    )
        .into_response())
}
