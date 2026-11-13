//! The work the daemon's services do (and `newcut scan` / `newcut sync`
//! do once): scan the libraries, look up what the tags don't say, check
//! discographies and fresh releases.

use std::collections::HashSet;
use std::fmt::Write as _;

use jiff::Timestamp;

use crate::catalog::{self, Today};
use crate::config::Loaded;
use crate::daemon::schedule;
use crate::events::{Event, Reporter, emit};
use crate::http::Http;
use crate::musicbrainz::MusicBrainz;
use crate::scan;
use crate::shutdown::Shutdown;
use crate::state::State;
use crate::state::library;

/// What a piece of work needs.
pub struct Work<'a> {
    pub state: &'a mut State,
    pub loaded: &'a Loaded,
    pub http: &'a Http,
    pub report: &'a Reporter,
    pub shutdown: &'a Shutdown,
}

/// Which artists to check.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Which {
    /// Those due.
    Due,
    /// These, due or not.
    Only(Vec<String>),
}

impl Work<'_> {
    fn mb(&self) -> MusicBrainz<'_> {
        MusicBrainz {
            http: self.http,
            server: &self.loaded.config.musicbrainz.server,
        }
    }

    fn today(&self) -> Today {
        Today::now(self.loaded.config.schedule.new_window.0)
    }

    /// Scan the libraries.
    ///
    /// # Errors
    ///
    /// On database errors.
    pub fn scan(&mut self) -> anyhow::Result<scan::Report> {
        let shutdown = self.shutdown.clone();
        let report = self.report.clone();
        report.working("scanning");
        scan::scan(
            &mut self.state.conn,
            &self.loaded.libraries,
            &|| shutdown.is_stopping(),
            &mut |text| report.working(format!("scanning {text}")),
        )
    }

    /// Look up release groups for files tagged with a release only, and new
    /// artists' names. Returns a summary.
    ///
    /// # Errors
    ///
    /// On database errors; a failed lookup is logged and tried next time.
    pub async fn resolve(&mut self) -> anyhow::Result<String> {
        let releases = library::releases_to_look_up(&self.state.conn)?;
        let artists = library::artists_to_resolve(&self.state.conn)?;
        let (mut found, mut failed) = (0, 0);
        for (i, release) in releases.iter().enumerate() {
            if self.shutdown.is_stopping() {
                break;
            }
            self.report.working(format!(
                "looking up release {} of {}",
                i + 1,
                releases.len()
            ));
            match self.mb().release_group_of(release).await {
                Ok(group) => {
                    library::set_release_group(
                        &self.state.conn,
                        release,
                        group.as_deref(),
                        Timestamp::now(),
                    )?;
                    found += 1;
                }
                Err(e) => {
                    tracing::warn!("release {release}: {e:#}");
                    failed += 1;
                }
            }
        }
        for (i, artist) in artists.iter().enumerate() {
            if self.shutdown.is_stopping() {
                break;
            }
            self.report
                .working(format!("looking up artist {} of {}", i + 1, artists.len()));
            let now = Timestamp::now();
            match self.mb().artist(artist).await {
                Ok(Some(info)) => {
                    if !info.joined_of.is_empty() {
                        let names: Vec<&str> =
                            info.joined_of.iter().map(|(_, n)| n.as_str()).collect();
                        tracing::info!("{} is a joined artist: {}", info.name, names.join(", "));
                    }
                    library::set_members(&self.state.conn, artist, &info.joined_of, now)?;
                    library::set_resolved(
                        &self.state.conn,
                        artist,
                        &info.name,
                        &info.sort_name,
                        now,
                    )?;
                    found += 1;
                }
                Ok(None) => {
                    let name = library::artist(&self.state.conn, artist)?
                        .map_or_else(|| artist.clone(), |a| a.name);
                    library::set_resolved(&self.state.conn, artist, &name, &name, now)?;
                    library::set_artist_error(
                        &self.state.conn,
                        artist,
                        "MusicBrainz doesn't know this artist id (check the tags)",
                    )?;
                }
                Err(e) => {
                    tracing::warn!("artist {artist}: {e:#}");
                    library::set_artist_error(&self.state.conn, artist, &format!("{e:#}"))?;
                    failed += 1;
                }
            }
        }
        let mut summary = format!("looked up {found}");
        if failed > 0 {
            let _ = write!(summary, ", {failed} failed (tried again later)");
        }
        Ok(summary)
    }

    /// Whether any artist is due for a check.
    ///
    /// # Errors
    ///
    /// On database errors.
    pub fn any_due(state: &State) -> anyhow::Result<bool> {
        let now = Timestamp::now();
        Ok(library::followed_schedule(&state.conn)?
            .iter()
            .any(|(_, next)| next.is_none_or(|t| t <= now)))
    }

    fn due(&self, which: &Which) -> anyhow::Result<Vec<String>> {
        let now = Timestamp::now();
        let artists = library::followed_schedule(&self.state.conn)?;
        Ok(match which {
            Which::Due => artists
                .into_iter()
                .filter(|(_, next)| next.is_none_or(|t| t <= now))
                .map(|(mbid, _)| mbid)
                .collect(),
            Which::Only(only) => artists
                .into_iter()
                .map(|(mbid, _)| mbid)
                .filter(|mbid| only.contains(mbid))
                .collect(),
        })
    }

    /// Check discographies on MusicBrainz. Returns a summary.
    ///
    /// # Errors
    ///
    /// On database errors; a failed check is logged and tried again later.
    pub async fn discographies(&mut self, which: &Which) -> anyhow::Result<String> {
        let due = self.due(which)?;
        let every = self.loaded.config.schedule.artist.0;
        let (mut checked, mut failed, mut new) = (0, 0, 0);
        for (i, artist) in due.iter().enumerate() {
            if self.shutdown.is_stopping() {
                break;
            }
            let name = artist_name(self.state, artist);
            self.report
                .working(format!("checking {name} ({} of {})", i + 1, due.len()));
            let now = Timestamp::now();
            match self.mb().release_groups(artist).await {
                Ok(groups) => {
                    let tx = State::write_transaction(&mut self.state.conn)?;
                    let mut added = HashSet::new();
                    for group in &groups {
                        if library::upsert_release_group(&tx, group, "musicbrainz", now)? {
                            added.insert(group.mbid.clone());
                        }
                    }
                    let next = schedule::next_sync(now, every, schedule::jitter());
                    library::set_checked(&tx, artist, now, next)?;
                    tx.commit()?;
                    new += self.announce(artist, &added)?;
                    checked += 1;
                }
                Err(e) => {
                    tracing::warn!("{name}: {e:#}");
                    library::set_artist_error(&self.state.conn, artist, &format!("{e:#}"))?;
                    // Try again in an hour.
                    let next = now
                        .checked_add(jiff::SignedDuration::from_hours(1))
                        .unwrap_or(now);
                    self.state.conn.execute(
                        "UPDATE artists SET next_check_at = ?2 WHERE mbid = ?1",
                        rusqlite::params![artist, next.to_string()],
                    )?;
                    failed += 1;
                }
            }
        }
        Ok(summarize(checked, failed, new))
    }

    /// Tell about the new ones among `added`; returns how many.
    fn announce(&self, artist: &str, added: &HashSet<String>) -> anyhow::Result<usize> {
        if added.is_empty() {
            return Ok(0);
        }
        let name = artist_name(self.state, artist);
        let today = self.today();
        let mut count = 0;
        for group in library::release_groups(&self.state.conn, Some(artist))? {
            if !added.contains(&group.mbid) {
                continue;
            }
            let entry = catalog::classify(group, &self.loaded.config.discography, today);
            if entry.is_new {
                count += 1;
                tracing::info!("new: {name} — {}", entry.group.title);
                emit(Event::Found {
                    artist: name.clone(),
                    title: entry.group.title.clone(),
                    href: format!("/artists/{artist}"),
                });
            }
        }
        Ok(count)
    }

    /// Check ListenBrainz's fresh releases. Returns a summary.
    ///
    /// # Errors
    ///
    /// When ListenBrainz can't be reached, or on database errors.
    pub async fn fresh(&mut self) -> anyhow::Result<String> {
        let followed = library::followed_ids(&self.state.conn)?;
        if followed.is_empty() {
            return Ok("no artists to look for".into());
        }
        self.report.working("checking fresh releases");
        let groups = crate::listenbrainz::fresh_releases(
            self.http,
            &self.loaded.config.listenbrainz.server,
            &followed,
        )
        .await?;
        let now = Timestamp::now();
        let tx = State::write_transaction(&mut self.state.conn)?;
        let mut added: Vec<(String, String)> = Vec::new();
        for group in &groups {
            if library::upsert_release_group(&tx, group, "listenbrainz", now)? {
                for artist in group.artist_mbids.iter().filter(|a| followed.contains(*a)) {
                    added.push((artist.clone(), group.mbid.clone()));
                }
            }
        }
        tx.commit()?;
        let mut new = 0;
        for (artist, group) in &added {
            new += self.announce(artist, &HashSet::from([group.clone()]))?;
        }
        Ok(format!(
            "{} fresh releases by your artists; {new} new",
            groups.len()
        ))
    }

    /// Follow queued downloads: file those done that match a release
    /// exactly; leave the rest for you. Returns a summary, and how many it
    /// filed.
    ///
    /// # Errors
    ///
    /// When slskd can't be asked, or on database errors.
    pub async fn import(&mut self) -> anyhow::Result<(String, usize)> {
        use crate::state::downloads::{self, DOWNLOADING, FAILED, FILED, MANUAL};
        let Some(paths) = self.loaded.import.clone() else {
            return Ok((
                "nothing to file: set slskd.downloads_dir and output_dir".into(),
                0,
            ));
        };
        let pending = downloads::pending(&self.state.conn)?;
        if pending.is_empty() {
            return Ok(("nothing filed; 0 downloading".into(), 0));
        }
        let client = crate::slskd::Slskd::new(&self.loaded.config.slskd)?;
        let mut transfers: std::collections::HashMap<String, Vec<crate::slskd::Transfer>> =
            std::collections::HashMap::new();
        let (mut filed, mut manual, mut downloading, mut unknown) = (0, 0, 0, 0);
        let mut needs_you = Vec::new();
        let mut filed_dirs: Vec<(String, String)> = Vec::new();
        for download in pending {
            if self.shutdown.is_stopping() {
                break;
            }
            if !transfers.contains_key(&download.username) {
                match client.transfers(&download.username).await {
                    Ok(found) => {
                        transfers.insert(download.username.clone(), found);
                    }
                    Err(e) => {
                        // Next time: the others may be fine.
                        tracing::warn!("{e:#}");
                        unknown += 1;
                        continue;
                    }
                }
            }
            let progress = crate::slskd::progress(&download.files, &transfers[&download.username]);
            let now = Timestamp::now();
            let set = |state: &str, detail: Option<&str>, to: Option<&str>| {
                let changed =
                    downloads::set_state(&self.state.conn, download.id, state, detail, to, now)?;
                if changed {
                    emit(Event::Download {
                        key: download.release_key.clone(),
                        state: state.to_string(),
                    });
                }
                Ok::<_, rusqlite::Error>(changed)
            };
            if !progress.finished() {
                downloading += 1;
                set(DOWNLOADING, None, None)?;
                continue;
            }
            if progress.status() != "ok" {
                set(FAILED, Some(&progress.label()), None)?;
                continue;
            }
            self.report.working(format!(
                "filing {}",
                download.dir.rsplit('\\').next().unwrap_or("")
            ));
            let filing = crate::import::file_download(
                self.http,
                &self.loaded.config.musicbrainz.server,
                &download,
                &paths,
                false,
            );
            match filing.await {
                Ok(dir) => {
                    let dir = dir.display().to_string();
                    tracing::info!("filed {} into {dir}", download.dir);
                    set(FILED, None, Some(&dir))?;
                    filed_dirs.push((download.release_key.clone(), dir));
                    filed += 1;
                }
                Err(why) => {
                    tracing::info!("left {} for you: {why}", download.dir);
                    if set(MANUAL, Some(&why), None)? {
                        crate::events::emit(Event::Changed);
                        needs_you.push(download_note(&self.state.conn, &download, why)?);
                    }
                    manual += 1;
                }
            }
        }
        // Scanned now (not at the next full scan): it's yours right away.
        for (key, dir) in filed_dirs {
            crate::scan::scan_new_album(
                &mut self.state.conn,
                &self.loaded.libraries,
                std::path::Path::new(&dir),
            )?;
            // Scanned: pages showing it can say it's yours.
            emit(Event::Download {
                key,
                state: FILED.into(),
            });
        }
        let told = self.tell_about_downloads(needs_you).await;
        let summary = import_summary(filed, manual, downloading, unknown, told);
        Ok((summary, filed))
    }

    /// Tell you (through every notification channel that's on) about each
    /// download that now needs you. Returns what happened, when there was
    /// something to tell.
    async fn tell_about_downloads(
        &self,
        notes: Vec<crate::notify::DownloadNote>,
    ) -> Option<String> {
        let channels = self.loaded.config.notification.channels();
        if notes.is_empty() || channels.is_empty() {
            return None;
        }
        let server = &self.loaded.config.server;
        let url = server
            .enabled
            .then(|| format!("http://{}/downloads", server.listen));
        let (mut sent, mut failed) = (0, 0);
        for note in notes {
            let message = crate::notify::download_message(note, url.clone());
            for channel in &channels {
                match self.send(channel, &message).await {
                    Ok(()) => sent += 1,
                    Err(e) => {
                        tracing::warn!("telling you through {channel} failed: {e:#}");
                        failed += 1;
                    }
                }
            }
        }
        Some(if failed == 0 {
            format!("told you ({sent} sent)")
        } else {
            format!("told you ({sent} sent, {failed} failed)")
        })
    }
}

/// What the importer did, in a line.
fn import_summary(
    filed: usize,
    manual: usize,
    downloading: usize,
    unknown: usize,
    told: Option<String>,
) -> String {
    let mut summary = if filed == 0 {
        "nothing filed".to_string()
    } else {
        format!("filed {filed}")
    };
    if manual > 0 {
        let _ = write!(summary, "; {manual} for you to look at");
    }
    let _ = write!(summary, "; {downloading} downloading");
    if unknown > 0 {
        let _ = write!(summary, "; slskd couldn't say how {unknown} are doing");
    }
    if let Some(told) = told {
        let _ = write!(summary, "; {told}");
    }
    summary
}

/// What to tell you about `download`, left for you because of `why`.
fn download_note(
    conn: &rusqlite::Connection,
    download: &crate::state::downloads::Download,
    why: String,
) -> anyhow::Result<crate::notify::DownloadNote> {
    let (album, artist) = library::release_group_title(conn, &download.release_key)?
        .unwrap_or_else(|| ("An album".into(), None));
    Ok(crate::notify::DownloadNote {
        album,
        artist: artist.unwrap_or_default(),
        release_group_mbid: download.release_key.clone(),
        folder: download.dir.clone(),
        username: download.username.clone(),
        reason: why,
    })
}

fn artist_name(state: &State, mbid: &str) -> String {
    library::artist(&state.conn, mbid)
        .ok()
        .flatten()
        .map_or_else(|| mbid.to_string(), |a| a.name)
}

fn summarize(checked: usize, failed: usize, new: usize) -> String {
    let mut text = format!(
        "checked {checked} artist{}",
        if checked == 1 { "" } else { "s" }
    );
    if failed > 0 {
        let _ = write!(text, ", {failed} failed (tried again in an hour)");
    }
    if new > 0 {
        let _ = write!(text, "; {new} new");
    }
    text
}

impl Work<'_> {
    /// Tell each notification channel what became new since it last heard.
    /// A channel's first run only starts the clock; one that fails hears
    /// the same next time.
    ///
    /// # Errors
    ///
    /// On database errors (a channel failing is in the summary).
    pub async fn notify(&mut self) -> anyhow::Result<String> {
        use crate::state::daemon as store;
        let config = &self.loaded.config.notification;
        let channels = config.channels();
        if channels.is_empty() {
            return Ok("no notification channels".into());
        }
        let now = Timestamp::now();
        let (_, rows) = crate::web::data::rows(&self.state.conn, self.loaded, None)?;
        let today = jiff::Zoned::now().date();
        let mut said = Vec::new();
        for channel in channels {
            let key = store::notified_key(channel);
            let Some(since) = store::get_time(&self.state.conn, &key)? else {
                store::set_time(&self.state.conn, &key, now)?;
                said.push(format!("{channel}: starting from now"));
                continue;
            };
            let pending = crate::notify::pending(&rows, config.strategy, since);
            if pending.is_empty() {
                store::set_time(&self.state.conn, &key, now)?;
                said.push(format!("{channel}: nothing new"));
                continue;
            }
            self.report.working(format!("notifying through {channel}"));
            let message = crate::notify::message(&pending, today);
            match self.send(channel, &message).await {
                Ok(()) => {
                    store::set_time(&self.state.conn, &key, now)?;
                    let n = pending.len();
                    said.push(format!(
                        "{channel}: sent {n} release{}",
                        if n == 1 { "" } else { "s" }
                    ));
                }
                Err(e) => {
                    tracing::warn!("notifying through {channel} failed: {e:#}");
                    said.push(format!("{channel}: failed: {e:#}"));
                }
            }
        }
        Ok(said.join("; "))
    }

    /// Send `message` through `channel`.
    ///
    /// # Errors
    ///
    /// When the channel isn't set up, or sending fails.
    pub async fn send(
        &self,
        channel: &str,
        message: &crate::notify::Message,
    ) -> anyhow::Result<()> {
        let config = &self.loaded.config.notification;
        match channel {
            "pushover" => {
                let pushover = config
                    .pushover
                    .as_ref()
                    .ok_or_else(|| anyhow::anyhow!("Pushover isn't set up"))?;
                crate::notify::pushover(self.http, pushover, message).await
            }
            "command" => {
                let (Some(command), Some(bin)) = (&config.command, &self.loaded.notify_bin) else {
                    anyhow::bail!("the notification command isn't set up");
                };
                crate::notify::command(bin, command, message, self.shutdown).await
            }
            other => anyhow::bail!("there's no notification channel {other:?}"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::downloads;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    const GROUP: &str = "11111111-0000-4000-8000-000000000009";

    #[tokio::test]
    async fn a_download_that_needs_you_is_told_once() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let got = dir.path().join("got.json");
        let script = dir.path().join("notify.sh");
        std::fs::write(
            &script,
            format!("#!/bin/sh\ncat >> {0}\necho >> {0}\n", got.display()),
        )
        .unwrap();
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
        let slskd = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/v0/transfers/downloads/someone"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "directories": [{ "files": [{
                    "filename": "x\\Album\\01.flac", "size": 1,
                    "state": "Completed, Succeeded", "bytesTransferred": 1
                }]}]
            })))
            .mount(&slskd)
            .await;
        let config = format!(
            "version = 1\n[server]\nlisten = \"127.0.0.1:55460\"\n\
             [slskd]\nenabled = true\nserver = {:?}\napi_key = \"k\"\n\
             downloads_dir = {:?}\noutput_dir = \"{}/music/%{{album}}\"\n\
             [notification.command]\nbin = {:?}\n",
            slskd.uri(),
            dir.path().display().to_string(),
            dir.path().display(),
            script.display().to_string(),
        );
        let loaded = crate::config::check_text(&config, "test", dir.path()).unwrap();
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
        downloads::add(
            &state.conn,
            &downloads::NewDownload {
                release_key: GROUP,
                username: "someone",
                dir: "x\\Album",
                files: &[("x\\Album\\01.flac".into(), 1)],
                matched: &downloads::Matched {
                    kind: "none".into(),
                    reason: Some("track 1 is 9:00 here, 3:00 on MusicBrainz".into()),
                    ..downloads::Matched::default()
                },
                destination: Some("newcut/a"),
            },
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
        let (summary, filed) = work.import().await.unwrap();
        assert_eq!(filed, 0);
        assert!(summary.contains("1 for you to look at; "), "{summary}");
        assert!(summary.contains("told you (1 sent)"), "{summary}");
        let told: serde_json::Value =
            serde_json::from_str(std::fs::read_to_string(&got).unwrap().trim()).unwrap();
        assert_eq!(told["kind"], "download");
        assert_eq!(told["title"], "A download needs you");
        assert_eq!(
            told["message"],
            "The Artist — The Album: no exact match: track 1 is 9:00 here, 3:00 on MusicBrainz."
        );
        assert_eq!(told["url"], "http://127.0.0.1:55460/downloads");
        assert_eq!(told["download"]["username"], "someone");
        assert_eq!(told["download"]["release_group_mbid"], GROUP);

        // Told once: it's waiting for you now, not followed.
        let (summary, _) = work.import().await.unwrap();
        assert!(!summary.contains("told you"), "{summary}");
        assert_eq!(std::fs::read_to_string(&got).unwrap().lines().count(), 1);
    }
}
