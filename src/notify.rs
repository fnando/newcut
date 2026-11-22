//! Notifications of new releases (and of downloads that need you, and of
//! albums upgraded): what to
//! tell you about, what to say, and the channels that say it (Pushover,
//! your own command).
//!
//! The notifier keeps, for each channel, when it last told you something:
//! each run tells it what became new since (one release by name, more in
//! general), and only moves that time on when the channel took it.

use std::path::Path;
use std::time::Duration;

use jiff::Timestamp;
use serde::Serialize;

use crate::catalog::parse_date;
use crate::config::{CommandConfig, PushoverConfig, Strategy};
use crate::http::Http;
use crate::shutdown::Shutdown;
use crate::web::data::Row;

/// The Pushover API.
pub const PUSHOVER_SERVER: &str = "https://api.pushover.net";

/// How long a command gets between SIGTERM and SIGKILL.
const KILL_GRACE: Duration = Duration::from_secs(5);

/// What a notification says.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Message {
    /// `releases`, `download` (one needs you), `upgrade`, `request` (albums
    /// you requested, downloaded) or `test`.
    pub kind: &'static str,
    pub title: String,
    pub message: String,
    /// Where to see it (one release: on MusicBrainz).
    pub url: Option<String>,
    pub url_title: Option<String>,
    pub count: usize,
    pub releases: Vec<Release>,
    /// The download that needs you (`kind` `download`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub download: Option<DownloadNote>,
    /// The albums upgraded (`kind` `upgrade`).
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub upgrades: Vec<UpgradeNote>,
    /// The albums you requested, downloaded (`kind` `request`).
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub requests: Vec<RequestNote>,
}

/// An album you requested, downloaded and filed, as the command gets it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct RequestNote {
    pub album: String,
    pub artist: String,
    pub release_group_mbid: String,
    /// "FLAC 16-bit/44.1 kHz", …
    pub quality: String,
    /// Where it is.
    pub folder: String,
}

/// An album upgraded, as the command gets it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct UpgradeNote {
    pub album: String,
    pub artist: String,
    pub release_group_mbid: String,
    /// Its quality before, and now ("MP3 192 kbps", "FLAC 16-bit/44.1 kHz").
    pub from: String,
    pub to: String,
    /// Where it is now.
    pub folder: String,
}

/// A download that needs you, as the command gets it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct DownloadNote {
    pub album: String,
    pub artist: String,
    pub release_group_mbid: String,
    /// The folder downloaded, and from whom.
    pub folder: String,
    pub username: String,
    /// Why it wasn't filed.
    pub reason: String,
}

/// A release, as the command gets it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Release {
    pub title: String,
    pub artist: String,
    pub artist_mbid: Option<String>,
    pub release_group_mbid: Option<String>,
    pub kind: String,
    pub date: Option<String>,
    /// owned, missing, upcoming
    pub status: String,
    pub upcoming: bool,
    pub link: String,
    pub cover: Option<String>,
    pub found_at: String,
}

/// The rows to tell a channel about: new (per `strategy`), and found after
/// `since`, oldest found first.
#[must_use]
pub fn pending(rows: &[Row], strategy: Strategy, since: Timestamp) -> Vec<Row> {
    let mut out: Vec<Row> = rows
        .iter()
        .filter(|r| match strategy {
            Strategy::Missing => r.is_new,
            Strategy::All => r.fresh,
        })
        .filter(|r| r.found_at > since)
        .cloned()
        .collect();
    out.sort_by(|a, b| a.found_at.cmp(&b.found_at).then(a.title.cmp(&b.title)));
    out
}

/// What to say about `rows` (at least one): the release by name when
/// there's one, how many otherwise.
#[must_use]
pub fn message(rows: &[Row], today: jiff::civil::Date) -> Message {
    let releases: Vec<Release> = rows.iter().map(|r| release(r, today)).collect();
    let count = rows.len();
    if let [row] = rows {
        let r = &releases[0];
        let when = match r.date.as_deref() {
            _ if r.upcoming => format!(" is coming {}", r.date.as_deref().unwrap_or_default()),
            Some(date) if parse_date(date, false) == Some(today) && date.len() == 10 => {
                " is out today".to_string()
            }
            Some(date) => format!(" came out {date}"),
            None => " is out".to_string(),
        };
        return Message {
            kind: "releases",
            download: None,
            upgrades: Vec::new(),
            requests: Vec::new(),
            title: format!("New release: {}", row.artist),
            message: format!("{} ({}){when}.", row.title, row.kind),
            url: Some(row.link.clone()),
            url_title: Some(format!("See it on {}", row.link_label)),
            count,
            releases,
        };
    }
    Message {
        kind: "releases",
        download: None,
        upgrades: Vec::new(),
        requests: Vec::new(),
        title: format!("{count} new releases"),
        message: format!("{count} new releases by artists you follow."),
        url: None,
        url_title: None,
        count,
        releases,
    }
}

fn release(row: &Row, today: jiff::civil::Date) -> Release {
    Release {
        title: row.title.clone(),
        artist: row.artist.clone(),
        artist_mbid: row.artist_mbid.clone(),
        release_group_mbid: row.mbid.clone(),
        kind: row.kind.clone(),
        date: row.date.clone(),
        status: row.status.clone(),
        upcoming: row
            .date
            .as_deref()
            .and_then(|d| parse_date(d, false))
            .is_some_and(|d| d > today),
        link: row.link.clone(),
        cover: row.cover.clone(),
        found_at: row.found_at.to_string(),
    }
}

/// A test notification, for `newcut notify --test`.
#[must_use]
pub fn test_message() -> Message {
    Message {
        kind: "test",
        download: None,
        upgrades: Vec::new(),
        requests: Vec::new(),
        title: "newcut".into(),
        message: "newcut can reach you: new releases will show up here.".into(),
        url: None,
        url_title: None,
        count: 0,
        releases: Vec::new(),
    }
}

/// What to say about a download that needs you; `url` is the Downloads
/// page, when the web UI is on.
#[must_use]
pub fn download_message(note: DownloadNote, url: Option<String>) -> Message {
    let what = if note.artist.is_empty() {
        note.album.clone()
    } else {
        format!("{} — {}", note.artist, note.album)
    };
    Message {
        kind: "download",
        title: "A download needs you".into(),
        message: format!("{what}: {}.", note.reason.trim_end_matches('.')),
        url_title: url.as_ref().map(|_| "See it in newcut".into()),
        url,
        count: 0,
        releases: Vec::new(),
        download: Some(note),
        upgrades: Vec::new(),
        requests: Vec::new(),
    }
}

/// What to say about albums just upgraded; `url` is where to see them.
#[must_use]
pub fn upgrade_message(notes: Vec<UpgradeNote>, url: Option<String>) -> Message {
    let (title, message) = match notes.as_slice() {
        [note] => (
            if note.artist.is_empty() {
                "Album upgraded".to_string()
            } else {
                format!("Upgraded: {}", note.artist)
            },
            format!("{}: {} → {}.", note.album, note.from, note.to),
        ),
        notes => (
            format!("{} albums upgraded", notes.len()),
            notes
                .iter()
                .map(|n| format!("{} ({} → {})", n.album, n.from, n.to))
                .collect::<Vec<_>>()
                .join(", ")
                + ".",
        ),
    };
    Message {
        kind: "upgrade",
        title,
        message,
        url_title: url.as_ref().map(|_| "See it in newcut".into()),
        url,
        count: notes.len(),
        releases: Vec::new(),
        download: None,
        upgrades: notes,
        requests: Vec::new(),
    }
}

/// What to say about albums you requested, just downloaded and filed;
/// `url` is where to see them.
#[must_use]
pub fn request_message(notes: Vec<RequestNote>, url: Option<String>) -> Message {
    let (title, message) = match notes.as_slice() {
        [note] => (
            if note.artist.is_empty() {
                "Requested album downloaded".to_string()
            } else {
                format!("Downloaded: {}", note.artist)
            },
            if note.quality.is_empty() {
                format!("{} is in your library.", note.album)
            } else {
                format!("{} ({}) is in your library.", note.album, note.quality)
            },
        ),
        notes => (
            format!("{} requested albums downloaded", notes.len()),
            notes
                .iter()
                .map(|n| {
                    if n.artist.is_empty() {
                        n.album.clone()
                    } else {
                        format!("{} — {}", n.artist, n.album)
                    }
                })
                .collect::<Vec<_>>()
                .join(", ")
                + ".",
        ),
    };
    Message {
        kind: "request",
        title,
        message,
        url_title: url.as_ref().map(|_| "See it in newcut".into()),
        url,
        count: notes.len(),
        releases: Vec::new(),
        download: None,
        upgrades: Vec::new(),
        requests: notes,
    }
}

/// Send through Pushover.
///
/// # Errors
///
/// When a key's variable isn't set, the request fails, or Pushover says no.
pub async fn pushover(
    http: &Http,
    config: &PushoverConfig,
    message: &Message,
) -> anyhow::Result<()> {
    let secret = |key: &str, value: &str| {
        crate::config::secret::resolve(value).ok_or_else(|| {
            anyhow::anyhow!("notification.pushover.{key} is empty (or its variable isn't set)")
        })
    };
    let token = secret("app_api_token", &config.app_api_token)?;
    let user = secret("user_key", &config.user_key)?;
    let mut form = vec![
        ("token", token.as_str()),
        ("user", user.as_str()),
        ("title", message.title.as_str()),
        ("message", message.message.as_str()),
    ];
    if let (Some(url), Some(title)) = (&message.url, &message.url_title) {
        form.push(("url", url));
        form.push(("url_title", title));
    }
    let url = format!("{}/1/messages.json", config.server.trim_end_matches('/'));
    let (status, body) = http.post_form("Pushover", &url, &form).await?;
    let accepted = body.get("status").and_then(serde_json::Value::as_i64) == Some(1);
    if status.is_success() && accepted {
        return Ok(());
    }
    let errors = body
        .get("errors")
        .and_then(|e| e.as_array())
        .map(|e| {
            e.iter()
                .filter_map(|e| e.as_str())
                .collect::<Vec<_>>()
                .join("; ")
        })
        .filter(|e| !e.is_empty())
        .unwrap_or_else(|| "no reason given".into());
    anyhow::bail!("Pushover answered {status}: {errors}")
}

/// Run the command with the message as JSON on its stdin.
///
/// # Errors
///
/// When it can't start, exits with an error, or runs out of time.
pub async fn command(
    bin: &Path,
    config: &CommandConfig,
    message: &Message,
    shutdown: &Shutdown,
) -> anyhow::Result<()> {
    let payload = serde_json::to_vec(message)?;
    let mut command = tokio::process::Command::new(bin);
    let output = crate::process::output_with_input(
        &mut command,
        payload,
        config.timeout.0,
        KILL_GRACE,
        shutdown,
    )
    .await
    .map_err(|e| anyhow::anyhow!("{}: {e}", bin.display()))?;
    if output.status.success() {
        return Ok(());
    }
    anyhow::bail!(crate::process::failure(bin, &output))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(title: &str, status: &str, is_new: bool, fresh: bool, found: &str) -> Row {
        Row {
            key: title.into(),
            mbid: Some(format!("{title}-mbid")),
            title: title.into(),
            artist: "Artist".into(),
            artist_mbid: Some("a".into()),
            kind: "Album".into(),
            date: Some("2026-11-10".into()),
            status: status.into(),
            ignore_path: None,
            label: String::new(),
            is_new,
            fresh,
            found_at: found.parse().unwrap(),
            owned_in: Vec::new(),
            cover: None,
            image: None,
            primary_type: None,
            link: format!("https://musicbrainz.org/release-group/{title}"),
            link_label: "MusicBrainz".into(),
            source: "musicbrainz".into(),
            downloadable: false,
            page: None,
            queued: None,
            quality: None,
            upgrade: None,
            upgraded: None,
            request: None,
        }
    }

    #[test]
    fn says_what_was_upgraded() {
        let note = |album: &str| UpgradeNote {
            album: album.into(),
            artist: "Radiohead".into(),
            release_group_mbid: "rg".into(),
            from: "MP3 192 kbps".into(),
            to: "FLAC 16-bit/44.1 kHz".into(),
            folder: "/music/x".into(),
        };
        let one = upgrade_message(vec![note("OK Computer")], None);
        assert_eq!(one.kind, "upgrade");
        assert_eq!(one.title, "Upgraded: Radiohead");
        assert_eq!(
            one.message,
            "OK Computer: MP3 192 kbps → FLAC 16-bit/44.1 kHz."
        );
        let json = serde_json::to_value(&one).unwrap();
        assert_eq!(json["upgrades"][0]["from"], "MP3 192 kbps");
        assert!(json.get("download").is_none());
        let two = upgrade_message(vec![note("A"), note("B")], Some("http://x".into()));
        assert_eq!(two.title, "2 albums upgraded");
        assert_eq!(two.count, 2);
        let release = serde_json::to_value(test_message()).unwrap();
        assert!(release.get("upgrades").is_none(), "only on upgrades");
    }

    #[test]
    fn says_what_you_requested_was_downloaded() {
        let note = |album: &str| RequestNote {
            album: album.into(),
            artist: "Radiohead".into(),
            release_group_mbid: "rg".into(),
            quality: "FLAC 16-bit/44.1 kHz".into(),
            folder: "/music/x".into(),
        };
        let one = request_message(vec![note("OK Computer")], None);
        assert_eq!(one.kind, "request");
        assert_eq!(one.title, "Downloaded: Radiohead");
        assert_eq!(
            one.message,
            "OK Computer (FLAC 16-bit/44.1 kHz) is in your library."
        );
        let json = serde_json::to_value(&one).unwrap();
        assert_eq!(json["requests"][0]["quality"], "FLAC 16-bit/44.1 kHz");
        assert!(json.get("upgrades").is_none());
        let two = request_message(vec![note("A"), note("B")], Some("http://x".into()));
        assert_eq!(two.title, "2 requested albums downloaded");
        assert_eq!(two.message, "Radiohead — A, Radiohead — B.");
        assert_eq!(two.count, 2);
        let release = serde_json::to_value(test_message()).unwrap();
        assert!(release.get("requests").is_none(), "only on requests");
    }

    #[test]
    fn picks_whats_new_since_the_last_time() {
        let rows = vec![
            row("missing", "missing", true, true, "2026-11-10T10:00:00Z"),
            row("owned", "owned", false, true, "2026-11-10T11:00:00Z"),
            row("old", "missing", true, true, "2026-11-01T10:00:00Z"),
            row("not new", "missing", false, false, "2026-11-10T12:00:00Z"),
        ];
        let since: Timestamp = "2026-11-09T00:00:00Z".parse().unwrap();
        let titles = |rows: Vec<Row>| rows.into_iter().map(|r| r.title).collect::<Vec<_>>();
        assert_eq!(
            titles(pending(&rows, Strategy::Missing, since)),
            ["missing"]
        );
        assert_eq!(
            titles(pending(&rows, Strategy::All, since)),
            ["missing", "owned"]
        );
        let later: Timestamp = "2026-11-10T10:30:00Z".parse().unwrap();
        assert_eq!(titles(pending(&rows, Strategy::All, later)), ["owned"]);
    }

    #[test]
    fn names_one_release_and_counts_more() {
        let today = jiff::civil::date(2026, 11, 10);
        let one = row("OK Computer", "missing", true, true, "2026-11-10T10:00:00Z");
        let m = message(std::slice::from_ref(&one), today);
        assert_eq!(m.title, "New release: Artist");
        assert_eq!(m.message, "OK Computer (Album) is out today.");
        assert_eq!(m.url.as_deref(), Some(one.link.as_str()));
        assert_eq!(m.url_title.as_deref(), Some("See it on MusicBrainz"));

        let mut coming = one.clone();
        coming.date = Some("2026-12-01".into());
        let m = message(&[coming], today);
        assert_eq!(m.message, "OK Computer (Album) is coming 2026-12-01.");
        assert!(m.releases[0].upcoming);

        let mut earlier = one.clone();
        earlier.date = Some("2026-11".into());
        assert_eq!(
            message(&[earlier], today).message,
            "OK Computer (Album) came out 2026-11."
        );

        let m = message(&[one.clone(), one], today);
        assert_eq!(m.title, "2 new releases");
        assert_eq!(m.message, "2 new releases by artists you follow.");
        assert_eq!((m.url, m.count, m.releases.len()), (None, 2, 2));
    }
}
