//! What pages show, gathered from the state DB.

use std::collections::{HashMap, HashSet};

use rusqlite::Connection;
use serde::Serialize;

use crate::catalog::{self, Entry, Status, Today};
use crate::config::Loaded;
use crate::daemon::service::{Service, enabled, local};
use crate::state::daemon as store;
use crate::state::library::{self, Artist};

/// The dashboard's status block.
#[derive(Debug, Clone, Serialize)]
pub struct Overview {
    pub services: Vec<ServiceView>,
    pub libraries: Vec<LibraryView>,
    pub new: Vec<Row>,
    pub problems: Vec<Problem>,
    pub artists: usize,
    pub followed: usize,
    pub owned: usize,
    pub missing: usize,
    pub upcoming: usize,
    pub untagged: usize,
    pub last_scan: Option<String>,
    pub last_fresh: Option<String>,
    pub rev: i64,
}

#[derive(Debug, Clone, Serialize)]
pub struct ServiceView {
    pub name: String,
    pub describe: String,
    pub status: String,
    pub activity: Option<String>,
    pub restarts: i64,
    pub last_error: Option<String>,
    pub paused: bool,
    /// Off in the config.
    pub off: bool,
    /// Has work to run on demand.
    pub runnable: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct LibraryView {
    pub path: String,
    pub exists: bool,
    pub files: i64,
    pub albums: i64,
    pub artists: i64,
    pub untagged: i64,
}

#[derive(Debug, Clone, Serialize)]
pub struct Problem {
    pub title: String,
    pub text: String,
}

/// A release group in a list.
#[derive(Debug, Clone, Serialize)]
pub struct Row {
    pub key: String,
    pub mbid: Option<String>,
    pub title: String,
    pub artist: String,
    pub artist_mbid: Option<String>,
    pub kind: String,
    /// Album, EP, Single, Broadcast, Other (when known).
    #[serde(skip)]
    pub primary_type: Option<String>,
    pub date: Option<String>,
    /// owned, missing, upcoming, ignored, other
    pub status: String,
    /// Where ignoring and unignoring it posts to.
    pub ignore_path: Option<String>,
    pub label: String,
    /// New, and you don't have it.
    pub is_new: bool,
    /// New, whether you have it or not.
    pub fresh: bool,
    /// When newcut found it.
    pub found_at: jiff::Timestamp,
    pub owned_in: Vec<String>,
    pub cover: Option<String>,
    /// What the pages show: `cover`, or the one in the album's folder
    /// (`covers.local`).
    #[serde(skip)]
    pub image: Option<String>,
    pub link: String,
    pub link_label: String,
    pub source: String,
    /// Out, and you don't have it: something to download.
    #[serde(skip)]
    pub downloadable: bool,
    /// Where the title leads, when not to `link`: the download page
    /// (`slskd`).
    #[serde(skip)]
    pub page: Option<String>,
    /// Its latest download, when a folder's been queued in slskd for it.
    #[serde(skip)]
    pub queued: Option<DownloadBadge>,
    /// Yours: how good it is (its worst file).
    #[serde(skip)]
    pub quality: Option<String>,
    /// Yours, and below `upgrade.target`.
    #[serde(skip)]
    pub upgrade: Option<UpgradeView>,
}

/// What an album row says of its upgrades.
#[derive(Debug, Clone)]
pub struct UpgradeView {
    /// Where asking for one posts.
    pub path: String,
    /// You asked; the upgrader hasn't looked yet.
    pub requested: bool,
    /// What the last look found.
    pub hint: String,
}

impl Row {
    #[must_use]
    pub fn from_entry(entry: &Entry, artist_mbid: Option<&str>, local_covers: bool) -> Self {
        let g = &entry.group;
        let mut kind = g
            .primary_type
            .clone()
            .unwrap_or_else(|| "Unknown type".into());
        for secondary in &g.secondary_types {
            kind.push_str(" + ");
            kind.push_str(secondary);
        }
        let artist_mbid = artist_mbid
            .map(str::to_string)
            .or_else(|| g.artists.first().map(|(a, _)| a.clone()));
        Self {
            key: g.mbid.clone(),
            mbid: Some(g.mbid.clone()),
            title: g.title.clone(),
            artist: g.artist_credit.clone().unwrap_or_default(),
            artist_mbid,
            kind,
            primary_type: g.primary_type.clone(),
            date: g.first_release_date.clone(),
            status: entry.status.key().into(),
            ignore_path: Some(format!("/release-groups/{}", g.mbid)),
            label: entry.status.label().into(),
            is_new: entry.is_new,
            fresh: entry.fresh,
            found_at: g.discovered_at,
            owned_in: g.owned_in.clone(),
            cover: Some(cover_art_archive(&g.mbid)),
            image: Some(if local_covers && !g.owned_in.is_empty() {
                format!("/covers/{}", g.mbid)
            } else {
                cover_art_archive(&g.mbid)
            }),
            link: format!("https://musicbrainz.org/release-group/{}", g.mbid),
            link_label: "MusicBrainz".into(),
            source: g.source.clone(),
            downloadable: matches!(entry.status, Status::Missing),
            page: None,
            queued: None,
            quality: None,
            upgrade: None,
        }
    }
}

/// What album rows say of their latest download (it leads to the album's
/// download page).
#[derive(Debug, Clone)]
pub struct DownloadBadge {
    pub page: String,
    pub label: &'static str,
    /// The `st-` class.
    pub class: &'static str,
    pub hint: &'static str,
}

impl DownloadBadge {
    /// For a download in `state` (none once you dealt with it).
    #[must_use]
    pub fn new(state: &str, page: &str) -> Option<Self> {
        use crate::state::downloads::{FAILED, FILED, MANUAL};
        let (label, class, hint) = match state {
            MANUAL => (
                "Download needs you",
                "missing",
                "Downloaded, but not filed: see why",
            ),
            FAILED => (
                "Download failed",
                "missing",
                "slskd couldn't get every file",
            ),
            FILED => (
                "Filed",
                "owned",
                "Tagged and moved into your library; it's yours once scanned",
            ),
            crate::state::downloads::HANDLED | crate::state::downloads::CANCELLED => return None,
            _ => (
                "Downloading",
                "upcoming",
                "Queued in slskd: see how it's going",
            ),
        };
        Some(Self {
            page: page.to_string(),
            label,
            class,
            hint,
        })
    }
}

/// A release group's front cover on the Cover Art Archive.
#[must_use]
pub fn cover_art_archive(mbid: &str) -> String {
    format!("https://coverartarchive.org/release-group/{mbid}/front-250")
}

/// An artist's picture: from their folder, when `covers.local` is on and
/// they have files (`has_files`).
#[must_use]
pub fn artist_image(loaded: &Loaded, artist: &Artist, has_files: bool) -> Option<String> {
    (loaded.config.covers.local && has_files).then(|| format!("/artists/{}/image", artist.mbid))
}

/// What stands in for a missing picture: the name's first letter or digit.
#[must_use]
pub fn initial(name: &str) -> String {
    name.chars()
        .find(|c| c.is_alphanumeric())
        .map_or_else(|| "♪".into(), |c| c.to_uppercase().collect())
}

/// Every followed artist's (or one's) release groups, classified.
///
/// # Errors
///
/// On database errors.
pub fn rows(
    conn: &Connection,
    loaded: &Loaded,
    artist: Option<&str>,
) -> anyhow::Result<(Vec<Entry>, Vec<Row>)> {
    let entries = crate::app::entries(conn, loaded, artist)?;
    let mut rows: Vec<Row> = entries
        .iter()
        .map(|e| Row::from_entry(e, artist, loaded.config.covers.local))
        .collect();
    if loaded.config.slskd.enabled {
        let states = crate::state::downloads::release_states(conn)?;
        for row in rows.iter_mut().filter(|r| r.downloadable) {
            let page = format!("/download/{}", row.key);
            row.queued = states
                .get(&row.key)
                .and_then(|state| DownloadBadge::new(state, &page));
            row.page = Some(page);
        }
    }
    let owned = rows.iter().any(|r| !r.owned_in.is_empty());
    if owned {
        add_qualities(conn, loaded, &mut rows)?;
    }
    rows.sort_by(|a, b| {
        b.date
            .as_deref()
            .unwrap_or("0")
            .cmp(a.date.as_deref().unwrap_or("0"))
            .then(a.title.cmp(&b.title))
    });
    Ok((entries, rows))
}

/// Owned albums' qualities, and what upgrading them is at.
fn add_qualities(conn: &Connection, loaded: &Loaded, rows: &mut [Row]) -> anyhow::Result<()> {
    use crate::state::downloads::{DOWNLOADING, MANUAL};
    let qualities: HashMap<String, crate::quality::Quality> = library::album_qualities(conn, None)?
        .into_iter()
        .map(|a| (a.release_group_mbid, a.quality))
        .collect();
    let upgrading = loaded.config.upgrade.enabled && enabled(Service::Upgrader, &loaded.config);
    let checks = if upgrading {
        crate::state::upgrades::all(conn)?
    } else {
        HashMap::new()
    };
    let states = crate::state::downloads::release_states(conn)?;
    for row in rows.iter_mut().filter(|r| !r.owned_in.is_empty()) {
        let Some(quality) = qualities.get(&row.key) else {
            continue;
        };
        row.quality = Some(quality.label());
        if !upgrading || quality.meets(loaded.config.upgrade.target) {
            continue;
        }
        let page = format!("/download/{}", row.key);
        match states.get(&row.key).map(String::as_str) {
            Some(DOWNLOADING) => {
                row.queued = Some(DownloadBadge {
                    page,
                    label: "Upgrading",
                    class: "upcoming",
                    hint: "A better copy is on its way: see how it's going",
                });
                continue;
            }
            Some(MANUAL) => {
                row.queued = DownloadBadge::new(MANUAL, &page);
                continue;
            }
            _ => {}
        }
        let check = checks.get(&row.key);
        let target = loaded.config.upgrade.target.name();
        row.upgrade = Some(UpgradeView {
            path: format!("/release-groups/{}/upgrade", row.key),
            requested: check.is_some_and(|c| c.requested_at.is_some()),
            hint: match check.and_then(|c| c.result.as_deref().zip(c.checked_at)) {
                Some((result, at)) => format!(
                    "Below {target}. Last looked {}: {result}. Look for a better copy now",
                    at.strftime("%Y-%m-%d")
                ),
                None => format!("Below {target}: look for a better copy now"),
            },
        });
    }
    Ok(())
}

/// The dashboard's status block.
///
/// # Errors
///
/// On database errors.
pub fn overview(conn: &Connection, rev: i64, loaded: &Loaded) -> anyhow::Result<Overview> {
    let mut services = Vec::new();
    let mut problems = Vec::new();
    for service in Service::ALL {
        let row = store::service(conn, service.name())?;
        let off = !enabled(service, &loaded.config);
        let status = if off {
            "off".to_string()
        } else if row.paused {
            "paused".to_string()
        } else {
            row.status.clone().unwrap_or_else(|| "stopped".into())
        };
        if let Some(error) = &row.last_error
            && row.status.as_deref() == Some("restarting")
        {
            problems.push(Problem {
                title: format!("The {} stopped unexpectedly", service.name()),
                text: error.clone(),
            });
        }
        services.push(ServiceView {
            name: service.name().into(),
            describe: service.describe().into(),
            status,
            activity: row.activity,
            restarts: row.restarts,
            last_error: row.last_error,
            paused: row.paused,
            off,
            runnable: service != Service::Server,
        });
    }
    let totals = library::library_totals(conn)?;
    let libraries = loaded
        .libraries
        .iter()
        .map(|path| {
            let name = path.to_string_lossy().into_owned();
            let t = totals.get(&name).cloned().unwrap_or_default();
            LibraryView {
                exists: path.is_dir(),
                path: name,
                files: t.files,
                albums: t.albums,
                artists: t.artists,
                untagged: t.untagged,
            }
        })
        .collect();
    let artists = library::artists(conn)?;
    for artist in artists.iter().filter(|a| a.followed) {
        if let Some(error) = &artist.last_error {
            problems.push(Problem {
                title: artist.name.clone(),
                text: error.clone(),
            });
        }
    }
    let (entries, rows) = rows(conn, loaded, None)?;
    let count = |s: Status| entries.iter().filter(|e| e.status == s).count();
    let new: Vec<Row> = rows.into_iter().filter(|r| r.is_new).collect();
    Ok(Overview {
        services,
        libraries,
        new,
        problems,
        artists: artists.len(),
        followed: artists.iter().filter(|a| a.followed).count(),
        owned: count(Status::Owned),
        missing: count(Status::Missing),
        upcoming: count(Status::Upcoming),
        untagged: library::untagged(conn)?
            .iter()
            .filter(|u| !u.ignored)
            .count(),
        last_scan: store::get_time(conn, store::LAST_SCAN)?.map(local),
        last_fresh: store::get_time(conn, store::LAST_FRESH)?.map(local),
        rev,
    })
}

/// An artist in the artists list.
#[derive(Debug, Clone, Serialize)]
pub struct ArtistRow {
    pub mbid: String,
    pub name: String,
    pub followed: bool,
    pub owned: usize,
    pub missing: usize,
    pub upcoming: usize,
    pub new: usize,
    pub checked: Option<String>,
    pub error: Option<String>,
    /// Their picture, from their folder (`covers.local`).
    pub image: Option<String>,
    /// For when there's no picture.
    pub initial: String,
}

/// Every artist, with their counts.
///
/// # Errors
///
/// On database errors.
pub fn artist_rows(conn: &Connection, loaded: &Loaded) -> anyhow::Result<Vec<ArtistRow>> {
    let today = Today::now(loaded.config.schedule.new_window.0);
    let groups = library::release_groups_all(conn)?;
    let mut by_artist: HashMap<String, Vec<Entry>> = HashMap::new();
    for group in groups {
        let entry = catalog::classify(group, &loaded.config.discography, today);
        for (artist, _) in &entry.group.artists {
            by_artist
                .entry(artist.clone())
                .or_default()
                .push(entry.clone());
        }
    }
    let with_files = if loaded.config.covers.local {
        library::artists_with_files(conn)?
    } else {
        HashSet::new()
    };
    Ok(library::artists(conn)?
        .into_iter()
        .map(|a| {
            let image = artist_image(loaded, &a, with_files.contains(&a.mbid));
            let mine = by_artist.get(&a.mbid).map_or(&[][..], Vec::as_slice);
            let count = |s: Status| mine.iter().filter(|e| e.status == s).count();
            ArtistRow {
                owned: count(Status::Owned),
                missing: count(Status::Missing),
                upcoming: count(Status::Upcoming),
                new: mine.iter().filter(|e| e.is_new).count(),
                checked: a.last_checked_at.map(local),
                error: a.last_error,
                followed: a.followed,
                image,
                initial: initial(&a.name),
                name: a.name,
                mbid: a.mbid,
            }
        })
        .collect())
}
