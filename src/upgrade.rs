//! Upgrades: the albums you have below `upgrade.target` are searched for
//! on slskd again, and the best folder for each (closest to the target, or
//! else the best better than yours) is queued to replace it. The importer
//! files it, and sets the old files aside (see [`crate::import`]).

use std::fmt::Write as _;

use jiff::Timestamp;

use crate::events::{Event, emit};
use crate::quality::Quality;
use crate::state::downloads::{self, DOWNLOADING, MANUAL};
use crate::state::library::{self, AlbumQuality};
use crate::state::upgrades;
use crate::work::Work;

/// What a run did.
#[derive(Debug, Default, PartialEq, Eq)]
struct Tally {
    below: usize,
    searched: usize,
    queued: usize,
    nothing: usize,
    failed: usize,
}

impl Tally {
    fn summary(&self) -> String {
        if self.searched == 0 {
            return format!("{} below the target; none due", self.below);
        }
        let mut text = format!(
            "{} searched: {} queued, {} with nothing better",
            self.searched, self.queued, self.nothing
        );
        if self.failed > 0 {
            let _ = write!(text, ", {} failed", self.failed);
        }
        let _ = write!(text, "; {} below the target", self.below);
        text
    }
}

/// The albums below `target` to look for upgrades of now, first first: those
/// you asked for, then those never looked for, then the longest ago. Not
/// ignored ones, nor those with a download going or waiting for you.
fn due(work: &Work<'_>, now: Timestamp) -> anyhow::Result<(Vec<AlbumQuality>, usize)> {
    let conn = &work.state.conn;
    let target = work.loaded.config.upgrade.target;
    let checks = upgrades::all(conn)?;
    let states = downloads::release_states(conn)?;
    let ignored: std::collections::HashSet<String> = {
        let mut stmt = conn.prepare("SELECT rg_mbid FROM ignored")?;
        stmt.query_map([], |r| r.get(0))?
            .collect::<rusqlite::Result<_>>()?
    };
    let below: Vec<AlbumQuality> = library::album_qualities(conn, None)?
        .into_iter()
        .filter(|a| !a.quality.meets(target))
        .filter(|a| !ignored.contains(&a.release_group_mbid))
        .collect();
    let count = below.len();
    let mut due: Vec<(AlbumQuality, Option<&upgrades::Check>)> = below
        .into_iter()
        .filter(|a| {
            !states
                .get(&a.release_group_mbid)
                .is_some_and(|s| s == DOWNLOADING || s == MANUAL)
        })
        .map(|a| {
            let check = checks.get(&a.release_group_mbid);
            (a, check)
        })
        .filter(|(_, check)| {
            check.is_none_or(|c| c.requested_at.is_some() || c.next_at.is_none_or(|n| n <= now))
        })
        .collect();
    let eligible: std::collections::HashSet<&str> = due
        .iter()
        .map(|(a, _)| a.release_group_mbid.as_str())
        .collect();
    upgrades::drop_requests(conn, &eligible)?;
    due.sort_by_key(|(a, check)| {
        let requested = check.and_then(|c| c.requested_at);
        let checked = check.and_then(|c| c.checked_at);
        (
            requested.is_none(),
            requested,
            checked.is_some(),
            checked,
            a.release_group_mbid.clone(),
        )
    });
    let per_run = usize::try_from(work.loaded.config.upgrade.per_run).unwrap_or(usize::MAX);
    Ok((
        due.into_iter().take(per_run).map(|(a, _)| a).collect(),
        count,
    ))
}

impl Work<'_> {
    /// Look for upgrades of the albums due, and queue those found.
    ///
    /// # Errors
    ///
    /// On database errors (slskd's are each album's).
    pub async fn upgrade(&mut self) -> anyhow::Result<String> {
        let config = &self.loaded.config.upgrade;
        if !config.enabled || self.loaded.import.is_none() {
            return Ok(
                "off: set upgrade.enabled, with slskd's downloads_dir and output_dir".into(),
            );
        }
        let client = crate::slskd::Slskd::new(&self.loaded.config.slskd)?;
        let now = Timestamp::now();
        let (albums, below) = due(self, now)?;
        let mut tally = Tally {
            below,
            ..Tally::default()
        };
        for album in albums {
            if self.shutdown.is_stopping() {
                break;
            }
            tally.searched += 1;
            match self.upgrade_one(&client, &album).await? {
                Some(true) => tally.queued += 1,
                Some(false) => tally.nothing += 1,
                None => tally.failed += 1,
            }
        }
        Ok(tally.summary())
    }

    /// Look for an upgrade of `album`: whether one was queued (`None`: the
    /// search failed).
    async fn upgrade_one(
        &mut self,
        client: &crate::slskd::Slskd,
        album: &AlbumQuality,
    ) -> anyhow::Result<Option<bool>> {
        let config = &self.loaded.config.upgrade;
        let key = &album.release_group_mbid;
        let from = album.quality.label();
        let now = Timestamp::now();
        let later = now + config.retry.0;
        let Some((title, artist)) = library::release_group_title(&self.state.conn, key)? else {
            // Not looked up yet: next time.
            tracing::debug!("upgrade of {key}: its release group isn't known yet");
            upgrades::checked(
                &self.state.conn,
                key,
                &from,
                "not looked up on MusicBrainz yet",
                now,
                now,
            )?;
            return Ok(Some(false));
        };
        let tried: Vec<(String, String)> = downloads::for_release(&self.state.conn, key)?
            .into_iter()
            .map(|d| (d.username, d.dir))
            .collect();
        self.report
            .working(format!("looking for an upgrade of {title} ({from})"));
        let shutdown = self.shutdown.clone();
        let search = crate::fallback::Search {
            client,
            http: self.http,
            musicbrainz_server: &self.loaded.config.musicbrainz.server,
            key,
            artist: artist.as_deref().unwrap_or_default(),
            album: &title,
            tried: &tried,
            filing: true,
        };
        let queued = crate::fallback::queue_upgrade(
            &search,
            &album.quality,
            config.target,
            &album.release_mbids,
            &|| shutdown.is_stopping(),
        )
        .await;
        let queued = match queued {
            Ok(Some(queued)) => queued,
            Ok(None) => {
                tracing::info!("no upgrade of {title} ({from}) on Soulseek");
                upgrades::checked(
                    &self.state.conn,
                    key,
                    &from,
                    "nothing better on Soulseek",
                    later,
                    now,
                )?;
                return Ok(Some(false));
            }
            Err(e) => {
                tracing::warn!("upgrade of {title}: {e:#}");
                // Again next run.
                upgrades::checked(
                    &self.state.conn,
                    key,
                    &from,
                    &format!("looking failed: {e:#}"),
                    now,
                    now,
                )?;
                return Ok(None);
            }
        };
        self.record(album, &queued, &from, later, now)?;
        Ok(Some(true))
    }

    /// `queued` for `album` (at `from`): remembered as an upgrade, and not
    /// looked for again before `later`.
    fn record(
        &mut self,
        album: &AlbumQuality,
        queued: &crate::fallback::Queued,
        from: &str,
        later: Timestamp,
        now: Timestamp,
    ) -> anyhow::Result<()> {
        let key = &album.release_group_mbid;
        let to = queued.folder.audio.label();
        let id = downloads::add(
            &self.state.conn,
            &downloads::NewDownload {
                release_key: key,
                username: &queued.folder.username,
                dir: &queued.folder.dir,
                files: &queued.folder.files,
                matched: &queued.matched,
                destination: queued.destination.as_deref(),
            },
            now,
        )?;
        downloads::set_upgrade(
            &self.state.conn,
            id,
            &downloads::Upgrade {
                replaces: album.dirs.clone(),
                from: from.to_string(),
                to: to.clone(),
            },
        )?;
        tracing::info!("upgrading {key}: {from} → {to}");
        upgrades::checked(
            &self.state.conn,
            key,
            from,
            &format!("queued {to} from {}", queued.folder.username),
            later,
            now,
        )?;
        emit(Event::Download {
            key: key.clone(),
            state: DOWNLOADING.into(),
        });
        Ok(())
    }

    /// Upgrades just filed (each download, and its album's folder now): the
    /// folders they replaced forgotten (or scanned again, when something's
    /// left in them), and you told, when `upgrade.notify` says so.
    pub(crate) async fn after_upgrades(
        &mut self,
        upgraded: Vec<(downloads::Download, String)>,
    ) -> anyhow::Result<Option<String>> {
        let now = Timestamp::now();
        let mut notes = Vec::new();
        for (download, dir) in upgraded {
            let Some(upgrade) = &download.upgrade else {
                continue;
            };
            let key = &download.release_key;
            let quality = filed(&mut self.state.conn, self.loaded, &download, &dir, now)?;
            let (album, artist) = library::release_group_title(&self.state.conn, key)?
                .unwrap_or_else(|| (download.dir.rsplit('\\').next().unwrap_or("").into(), None));
            notes.push(crate::notify::UpgradeNote {
                album,
                artist: artist.unwrap_or_default(),
                release_group_mbid: key.clone(),
                from: upgrade.from.clone(),
                to: quality,
                folder: dir,
            });
        }
        if notes.is_empty() || !self.loaded.config.upgrade.notify {
            return Ok(None);
        }
        let server = &self.loaded.config.server;
        let url = server
            .enabled
            .then(|| format!("http://{}/downloads", server.listen));
        Ok(self
            .tell(vec![crate::notify::upgrade_message(notes, url)])
            .await)
    }
}

/// Upgrade `download` was filed into `dir` (scanned already): the folders
/// it replaced are forgotten (or scanned again, when something's left in
/// them), and it's not looked for again before `upgrade.retry`. Returns
/// the album's quality now.
///
/// # Errors
///
/// On database errors.
pub fn filed(
    conn: &mut rusqlite::Connection,
    loaded: &crate::config::Loaded,
    download: &downloads::Download,
    dir: &str,
    now: Timestamp,
) -> anyhow::Result<String> {
    let Some(upgrade) = &download.upgrade else {
        return Ok(String::new());
    };
    for old in upgrade.replaces.iter().filter(|o| *o != dir) {
        let path = std::path::Path::new(old);
        if path.is_dir()
            && let Some(library) = loaded.libraries.iter().find(|l| path.starts_with(l))
        {
            crate::scan::scan_dirs(conn, library, &[path.to_path_buf()])?;
        } else {
            library::remove_files_under(conn, old)?;
        }
    }
    let key = &download.release_key;
    let quality = library::album_qualities(conn, Some(key))?
        .pop()
        .map_or_else(|| upgrade.to.clone(), |a| a.quality.label());
    upgrades::checked(
        conn,
        key,
        &quality,
        &format!("upgraded from {}", upgrade.from),
        now + loaded.config.upgrade.retry.0,
        now,
    )?;
    upgrades::upgraded(conn, key, &upgrade.from, &quality, now)?;
    Ok(quality)
}

/// Ask for an upgrade of release group `key` now: first in line at the
/// upgrader's next run.
///
/// # Errors
///
/// On database errors.
pub fn request(conn: &rusqlite::Connection, key: &str) -> anyhow::Result<()> {
    upgrades::request(conn, key, Timestamp::now())?;
    Ok(())
}

/// Whether `quality` is below `upgrade.target` (and upgrades are on).
#[must_use]
pub fn wanted(config: &crate::config::Config, quality: &Quality) -> bool {
    config.upgrade.enabled && !quality.meets(config.upgrade.target)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::events::Reporter;
    use crate::http::Http;
    use crate::shutdown::Shutdown;
    use crate::state::State;
    use wiremock::matchers::{body_partial_json, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    const GROUP: &str = "11111111-0000-4000-8000-000000000009";
    const RELEASE: &str = "22222222-0000-4000-8000-000000000001";

    /// slskd and MusicBrainz both: a search that finds a 320 kbps MP3 and
    /// a CD-quality FLAC of the album; queueing the FLAC.
    async fn soulseek() -> MockServer {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/api/v0/searches"))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(serde_json::json!({ "id": "s1" })),
            )
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/api/v0/searches/s1"))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(serde_json::json!({ "isComplete": true })),
            )
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/api/v0/searches/s1/responses"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!([
                { "username": "mp3", "hasFreeUploadSlot": true, "files": [
                    { "filename": "m\\Album\\01 One.mp3", "size": 1, "length": 200, "bitRate": 320 }] },
                { "username": "flac", "files": [
                    { "filename": "f\\Album\\01 One.flac", "size": 1, "length": 200,
                      "bitDepth": 16, "sampleRate": 44100 }] },
            ])))
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/api/v0/transfers/downloads/batches"))
            .and(body_partial_json(serde_json::json!({ "username": "flac" })))
            .respond_with(
                ResponseTemplate::new(201).set_body_json(serde_json::json!({ "failures": [] })),
            )
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/ws/2/release"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "release-count": 1, "releases": [{
                    "id": RELEASE, "title": "The Album",
                    "media": [{ "position": 1, "format": "CD", "tracks": [
                        { "id": "track-1", "position": 1, "title": "One", "length": 200_000,
                          "recording": { "id": "rec-1" } }]}]
                }]
            })))
            .mount(&server)
            .await;
        server
    }

    #[tokio::test]
    async fn an_album_below_the_target_is_upgraded() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let got = root.join("got.json");
        let script = root.join("notify.sh");
        std::fs::write(
            &script,
            format!("#!/bin/sh\ncat >> {0}\necho >> {0}\n", got.display()),
        )
        .unwrap();
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
        let server = soulseek().await;
        let config = format!(
            "version = 1\nlibraries = [{1:?}]\n[musicbrainz]\nserver = {0:?}\n\
             [slskd]\nenabled = true\nserver = {0:?}\napi_key = \"k\"\n\
             downloads_dir = {2:?}\noutput_dir = \"{1}/%{{album}}\"\n\
             [upgrade]\nenabled = true\nnotify = true\n\
             [notification.command]\nbin = {3:?}\n",
            server.uri(),
            root.join("music").display().to_string(),
            root.join("downloads").display().to_string(),
            script.display().to_string(),
        );
        let loaded = crate::config::check_text(&config, "test", root).unwrap();
        let mut state = State::open_in_memory().unwrap();
        let now = Timestamp::now();
        library::upsert_release_group(
            &state.conn,
            &library::NewReleaseGroup {
                mbid: GROUP.into(),
                title: "The Album".into(),
                artist_credit: Some("The Artist".into()),
                primary_type: Some("Album".into()),
                ..library::NewReleaseGroup::default()
            },
            "musicbrainz",
            now,
        )
        .unwrap();
        // What you have: a 192 kbps MP3 (gone from its folder already, as
        // filing it would have done).
        let old = root.join("music/Old").display().to_string();
        library::upsert_file(
            &state.conn,
            &library::ScannedFile {
                path: format!("{old}/01.mp3"),
                library: root.join("music").display().to_string(),
                dir: old.clone(),
                release_mbid: Some(RELEASE.into()),
                release_group_mbid: Some(GROUP.into()),
                artist_mbids: vec!["a".into()],
                quality: Quality {
                    format: "MP3".into(),
                    bitrate: Some(192),
                    ..Quality::default()
                },
                ..library::ScannedFile::default()
            },
            now,
        )
        .unwrap();
        let http = Http::new(None);
        let report = Reporter {
            service: "upgrader",
        };
        let shutdown = Shutdown::default();
        let mut work = Work {
            state: &mut state,
            loaded: &loaded,
            http: &http,
            report: &report,
            shutdown: &shutdown,
        };
        let summary = work.upgrade().await.unwrap();
        assert_eq!(
            summary,
            "1 searched: 1 queued, 0 with nothing better; 1 below the target"
        );
        let queued = downloads::for_release(&work.state.conn, GROUP).unwrap();
        assert_eq!(queued.len(), 1);
        assert_eq!(queued[0].username, "flac", "the lossless one");
        assert_eq!(
            queued[0].upgrade,
            Some(downloads::Upgrade {
                replaces: vec![old.clone()],
                from: "MP3 192 kbps".into(),
                to: "FLAC 16-bit/44.1 kHz".into(),
            })
        );
        // Queued: not again until it's done (or `retry` from now).
        assert_eq!(
            work.upgrade().await.unwrap(),
            "1 below the target; none due"
        );

        // Filed (as the importer does): what it replaced is forgotten, and
        // you're told.
        let new = root.join("music/The Album").display().to_string();
        library::upsert_file(
            &work.state.conn,
            &library::ScannedFile {
                path: format!("{new}/1. One.flac"),
                library: root.join("music").display().to_string(),
                dir: new.clone(),
                release_group_mbid: Some(GROUP.into()),
                quality: Quality {
                    format: "FLAC".into(),
                    lossless: true,
                    bit_depth: Some(16),
                    sample_rate: Some(44_100),
                    bitrate: Some(900),
                },
                ..library::ScannedFile::default()
            },
            now,
        )
        .unwrap();
        let told = work
            .after_upgrades(vec![(queued[0].clone(), new.clone())])
            .await
            .unwrap();
        assert_eq!(told.as_deref(), Some("told you (1 sent)"));
        let albums = library::album_qualities(&work.state.conn, Some(GROUP)).unwrap();
        assert_eq!(albums[0].dirs, [new]);
        let told: serde_json::Value =
            serde_json::from_str(std::fs::read_to_string(&got).unwrap().trim()).unwrap();
        assert_eq!(told["kind"], "upgrade");
        assert_eq!(told["title"], "Upgraded: The Artist");
        assert_eq!(
            told["message"],
            "The Album: MP3 192 kbps → FLAC 16-bit/44.1 kHz."
        );
        let check = upgrades::all(&work.state.conn)
            .unwrap()
            .remove(GROUP)
            .unwrap();
        assert_eq!(check.result.as_deref(), Some("upgraded from MP3 192 kbps"));
        let upgraded = check.upgraded.unwrap();
        assert_eq!(
            (upgraded.from.as_str(), upgraded.to.as_str()),
            ("MP3 192 kbps", "FLAC 16-bit/44.1 kHz")
        );
    }

    #[tokio::test]
    async fn a_failed_upgrade_is_tried_again_quietly() {
        let dir = tempfile::tempdir().unwrap();
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/v0/transfers/downloads/someone"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "directories": [{ "files": [{
                    "id": "t1", "filename": "x\\Album\\01.flac", "size": 1,
                    "state": "Completed, Errored"
                }]}]
            })))
            .mount(&server)
            .await;
        // No search: the upgrader does that.
        Mock::given(method("POST"))
            .and(path("/api/v0/searches"))
            .respond_with(ResponseTemplate::new(500))
            .expect(0)
            .mount(&server)
            .await;
        let config = format!(
            "version = 1\n[slskd]\nenabled = true\nserver = {0:?}\napi_key = \"k\"\n\
             downloads_dir = {1:?}\noutput_dir = \"{1}/music/%{{album}}\"\n\
             [upgrade]\nenabled = true\n",
            server.uri(),
            dir.path().display().to_string(),
        );
        let loaded = crate::config::check_text(&config, "test", dir.path()).unwrap();
        let mut state = State::open_in_memory().unwrap();
        let now = Timestamp::now();
        let id = downloads::add(
            &state.conn,
            &downloads::NewDownload {
                release_key: GROUP,
                username: "someone",
                dir: "x\\Album",
                files: &[("x\\Album\\01.flac".into(), 1)],
                matched: &downloads::Matched::default(),
                destination: Some("newcut/a"),
            },
            now,
        )
        .unwrap();
        downloads::set_upgrade(
            &state.conn,
            id,
            &downloads::Upgrade {
                replaces: vec!["/music/old".into()],
                from: "MP3 192 kbps".into(),
                to: "FLAC".into(),
            },
        )
        .unwrap();
        upgrades::checked(
            &state.conn,
            GROUP,
            "MP3 192 kbps",
            "queued",
            now + std::time::Duration::from_secs(3600),
            now,
        )
        .unwrap();
        let http = Http::new(None);
        let report = Reporter {
            service: "importer",
        };
        let shutdown = Shutdown::default();
        let mut work = Work {
            state: &mut state,
            loaded: &loaded,
            http: &http,
            report: &report,
            shutdown: &shutdown,
        };
        let (summary, _) = work.import().await.unwrap();
        assert!(!summary.contains("told"), "{summary}");
        let check = upgrades::all(&work.state.conn)
            .unwrap()
            .remove(GROUP)
            .unwrap();
        assert_eq!(check.next_at, None, "due at the next run");
        assert_eq!(check.result.as_deref(), Some("its download failed"));
        assert_eq!(
            downloads::for_release(&work.state.conn, GROUP)
                .unwrap()
                .len(),
            1,
            "nothing else queued"
        );
    }

    #[test]
    fn summaries() {
        let tally = Tally {
            below: 12,
            searched: 3,
            queued: 1,
            nothing: 1,
            failed: 1,
        };
        assert_eq!(
            tally.summary(),
            "3 searched: 1 queued, 1 with nothing better, 1 failed; 12 below the target"
        );
        assert_eq!(Tally::default().summary(), "0 below the target; none due");
    }
}
