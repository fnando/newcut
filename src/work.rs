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
        let mut filed_dirs: Vec<(downloads::Download, String)> = Vec::new();
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
                // Failed (not cancelled, nor taken off slskd's list: that's
                // you): another source, if there's one.
                let just_failed = set(FAILED, Some(&progress.label()), None)?
                    && progress.status() == "failed"
                    && progress.cancelled == 0;
                if just_failed && let Some(why) = self.replace(&client, &download).await? {
                    needs_you.push(download_note(&self.state.conn, &download, why)?);
                }
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
                self.shutdown,
            );
            match filing.await {
                Ok(dir) => {
                    let dir = dir.display().to_string();
                    tracing::info!("filed {} into {dir}", download.dir);
                    set(FILED, None, Some(&dir))?;
                    filed_dirs.push((download.clone(), dir));
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
        let told_upgrades = self.after_filing(filed_dirs).await?;
        let told = match (self.tell_about_downloads(needs_you).await, told_upgrades) {
            (Some(a), Some(b)) => Some(format!("{a}; upgrades: {b}")),
            (a, b) => a.or(b),
        };
        let summary = import_summary(filed, manual, downloading, unknown, told);
        Ok((summary, filed))
    }

    /// The album folders just filed (with their downloads): told to
    /// Jellyfin, and scanned; then what upgrades replaced is forgotten.
    /// Returns what telling you of upgrades did.
    async fn after_filing(
        &mut self,
        filed_dirs: Vec<(crate::state::downloads::Download, String)>,
    ) -> anyhow::Result<Option<String>> {
        use crate::state::downloads::FILED;
        // Jellyfin first: scanning ours takes a while.
        let dirs: Vec<std::path::PathBuf> = filed_dirs.iter().map(|(_, d)| d.into()).collect();
        crate::jellyfin::tell(self.loaded, &dirs).await;
        // Scanned now (not at the next full scan): it's yours right away.
        let mut upgraded = Vec::new();
        for (download, dir) in filed_dirs {
            crate::scan::scan_new_album(
                &mut self.state.conn,
                &self.loaded.libraries,
                std::path::Path::new(&dir),
            )?;
            // Scanned: pages showing it can say it's yours.
            emit(Event::Download {
                key: download.release_key.clone(),
                state: FILED.into(),
            });
            if download.upgrade.is_some() {
                upgraded.push((download, dir));
            }
        }
        self.after_upgrades(upgraded).await
    }

    /// Download `failed` failed: queue another source for its album (the
    /// best untried lossless exact match a new search finds). Returns why
    /// that didn't happen, when it should be told.
    async fn replace(
        &mut self,
        client: &crate::slskd::Slskd,
        failed: &crate::state::downloads::Download,
    ) -> anyhow::Result<Option<String>> {
        use crate::state::downloads::{self, FAILED};
        let key = &failed.release_key;
        if failed.upgrade.is_some() {
            // Another source at the upgrader's next run; nothing you need
            // to know.
            crate::state::upgrades::again(&self.state.conn, key, "its download failed")?;
            return Ok(None);
        }
        let all = downloads::for_release(&self.state.conn, key)?;
        if all.iter().any(|d| d.id > failed.id) {
            // Another's been queued since.
            return Ok(None);
        }
        let failures = all.iter().filter(|d| d.state == FAILED).count();
        if failures >= crate::fallback::MAX_FAILURES {
            return Ok(Some(format!(
                "its download failed {failures} times; pick another source yourself"
            )));
        }
        let Some((album, artist)) = library::release_group_title(&self.state.conn, key)? else {
            return Ok(None);
        };
        let tried: Vec<(String, String)> = all
            .iter()
            .map(|d| (d.username.clone(), d.dir.clone()))
            .collect();
        self.report
            .working(format!("looking for another source of {album}"));
        let shutdown = self.shutdown.clone();
        let search = crate::fallback::Search {
            client,
            http: self.http,
            musicbrainz_server: &self.loaded.config.musicbrainz.server,
            key,
            artist: artist.as_deref().unwrap_or_default(),
            album: &album,
            tried: &tried,
            filing: self.loaded.import.is_some(),
        };
        let queued = match crate::fallback::queue_another(&search, &|| shutdown.is_stopping()).await
        {
            Ok(Some(queued)) => queued,
            Ok(None) => {
                return Ok(Some(
                    "its download failed, and Soulseek has no other lossless folder that matches exactly"
                        .into(),
                ));
            }
            Err(e) => {
                tracing::warn!("another source for {album}: {e:#}");
                return Ok(Some(format!(
                    "its download failed, and looking for another source did too: {e:#}"
                )));
            }
        };
        downloads::add(
            &self.state.conn,
            &downloads::NewDownload {
                release_key: key,
                username: &queued.folder.username,
                dir: &queued.folder.dir,
                files: &queued.folder.files,
                matched: &queued.matched,
                destination: queued.destination.as_deref(),
            },
            Timestamp::now(),
        )?;
        emit(Event::Download {
            key: key.clone(),
            state: downloads::DOWNLOADING.into(),
        });
        Ok(None)
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
        let messages = notes
            .into_iter()
            .map(|note| crate::notify::download_message(note, url.clone()))
            .collect();
        self.tell(messages).await
    }

    /// Send `messages` through every notification channel that's on.
    /// Returns what happened, when there was something to tell.
    pub(crate) async fn tell(&self, messages: Vec<crate::notify::Message>) -> Option<String> {
        let channels = self.loaded.config.notification.channels();
        if messages.is_empty() || channels.is_empty() {
            return None;
        }
        let (mut sent, mut failed) = (0, 0);
        for message in messages {
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

    #[tokio::test]
    async fn a_failed_download_is_replaced_by_another_source() {
        use wiremock::matchers::body_partial_json;
        let dir = tempfile::tempdir().unwrap();
        // slskd and MusicBrainz both.
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
        Mock::given(method("POST"))
            .and(path("/api/v0/searches"))
            .and(body_partial_json(
                serde_json::json!({ "searchText": "The Artist The Album" }),
            ))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(serde_json::json!({ "id": "s2" })),
            )
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/api/v0/searches/s2"))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(serde_json::json!({ "isComplete": true })),
            )
            .mount(&server)
            .await;
        let flac = |dir: &str| serde_json::json!({ "filename": format!("{dir}\\01 One.flac"), "size": 1, "length": 200 });
        Mock::given(method("GET"))
            .and(path("/api/v0/searches/s2/responses"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!([
                // Tried already.
                { "username": "someone", "hasFreeUploadSlot": true, "files": [flac("x\\Album")] },
                { "username": "lossy", "hasFreeUploadSlot": true, "files": [
                    { "filename": "z\\Album\\01 One.mp3", "size": 1, "length": 200 }] },
                { "username": "other", "files": [flac("y\\Album")] },
            ])))
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/api/v0/transfers/downloads/batches"))
            .and(body_partial_json(
                serde_json::json!({ "username": "other" }),
            ))
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
                    "id": "22222222-0000-4000-8000-000000000001", "title": "The Album",
                    "media": [{ "position": 1, "format": "CD", "tracks": [
                        { "id": "track-1", "position": 1, "title": "One", "length": 200_000,
                          "recording": { "id": "rec-1" } }]}]
                }]
            })))
            .mount(&server)
            .await;
        let config = format!(
            "version = 1\n[musicbrainz]\nserver = {0:?}\n\
             [slskd]\nenabled = true\nserver = {0:?}\napi_key = \"k\"\n\
             downloads_dir = {1:?}\noutput_dir = \"{2}/music/%{{album}}\"\n",
            server.uri(),
            dir.path().display().to_string(),
            dir.path().display(),
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
                matched: &downloads::Matched::default(),
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
        work.import().await.unwrap();
        let all = downloads::for_release(&work.state.conn, GROUP).unwrap();
        let (old, new): (Vec<_>, Vec<_>) = all.iter().partition(|d| d.id == 1);
        assert_eq!(old[0].state, downloads::FAILED);
        assert_eq!(new.len(), 1, "{all:?}");
        assert_eq!(new[0].username, "other");
        assert_eq!(new[0].dir, "y\\Album");
        assert_eq!(new[0].state, downloads::DOWNLOADING);
        assert_eq!(new[0].matched.kind, "exact");
        assert!(
            new[0]
                .destination
                .as_deref()
                .unwrap()
                .starts_with("newcut/")
        );

        // Once: the next run leaves it be (the new one isn't in slskd's
        // list here, which isn't a failure to replace).
        work.import().await.unwrap();
        assert_eq!(
            downloads::for_release(&work.state.conn, GROUP)
                .unwrap()
                .len(),
            2
        );
    }
}
