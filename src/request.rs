//! Requests: albums you asked for, out or still to come, are searched for
//! on Soulseek until it has them. The best folder that matches one of the
//! album's releases exactly (closest to `upgrade.target` while meeting it,
//! else the best there is) is queued, and the importer files it.

use std::fmt::Write as _;

use jiff::Timestamp;

use crate::events::{Event, emit};
use crate::quality::Quality;
use crate::state::downloads::{self, DOWNLOADING, MANUAL};
use crate::state::library;
use crate::state::requests;
use crate::work::Work;

/// What a run did.
#[derive(Debug, Default, PartialEq, Eq)]
struct Tally {
    requested: usize,
    searched: usize,
    queued: usize,
    nothing: usize,
    failed: usize,
}

impl Tally {
    fn summary(&self) -> String {
        if self.searched == 0 {
            return format!("{} requested; none due", self.requested);
        }
        let mut text = format!(
            "{} searched: {} queued, {} not on Soulseek yet",
            self.searched, self.queued, self.nothing
        );
        if self.failed > 0 {
            let _ = write!(text, ", {} failed", self.failed);
        }
        let _ = write!(text, "; {} requested", self.requested);
        text
    }
}

/// The requests to look for now, oldest first, and how many there are.
/// Those for albums you have now are forgotten; those with a download going
/// or waiting for you wait. Ignored albums are looked for too: you asked.
///
/// # Errors
///
/// On database errors.
pub fn due(conn: &rusqlite::Connection, now: Timestamp) -> anyhow::Result<(Vec<String>, usize)> {
    requests::drop_owned(conn)?;
    let all = requests::all(conn)?;
    let states = downloads::release_states(conn)?;
    let mut due: Vec<&requests::Request> = all
        .values()
        .filter(|r| r.due(now))
        .filter(|r| {
            !states
                .get(&r.rg_mbid)
                .is_some_and(|s| s == DOWNLOADING || s == MANUAL)
        })
        .collect();
    due.sort_by_key(|r| (r.checked_at.is_some(), r.requested_at, r.rg_mbid.clone()));
    Ok((
        due.into_iter().map(|r| r.rg_mbid.clone()).collect(),
        all.len(),
    ))
}

/// Ask for release group `key`: looked for at the requester's next run.
///
/// # Errors
///
/// On database errors.
pub fn request(conn: &rusqlite::Connection, key: &str) -> anyhow::Result<()> {
    requests::add(conn, key, Timestamp::now())?;
    Ok(())
}

/// Take the request for release group `key` back.
///
/// # Errors
///
/// On database errors.
pub fn unrequest(conn: &rusqlite::Connection, key: &str) -> anyhow::Result<()> {
    requests::remove(conn, key)?;
    Ok(())
}

impl Work<'_> {
    /// Look for the albums requested that are due, and queue those found.
    ///
    /// # Errors
    ///
    /// On database errors (slskd's are each album's).
    pub async fn requests(&mut self) -> anyhow::Result<String> {
        if self.loaded.import.is_none() {
            return Ok("off: set slskd's downloads_dir and output_dir".into());
        }
        let client = crate::slskd::Slskd::new(&self.loaded.config.slskd)?;
        let (keys, requested) = due(&self.state.conn, Timestamp::now())?;
        let mut tally = Tally {
            requested,
            ..Tally::default()
        };
        for key in keys {
            if self.shutdown.is_stopping() {
                break;
            }
            tally.searched += 1;
            match self.request_one(&client, &key).await? {
                Some(true) => tally.queued += 1,
                Some(false) => tally.nothing += 1,
                None => tally.failed += 1,
            }
        }
        Ok(tally.summary())
    }

    /// Look for requested album `key`: whether a folder was queued
    /// (`None`: the search failed).
    async fn request_one(
        &mut self,
        client: &crate::slskd::Slskd,
        key: &str,
    ) -> anyhow::Result<Option<bool>> {
        let now = Timestamp::now();
        let later = now + self.loaded.config.schedule.request.0;
        let conn = &self.state.conn;
        let Some((title, artist)) = library::release_group_title(conn, key)? else {
            tracing::debug!("request for {key}: its release group isn't known yet");
            requests::checked(conn, key, "not looked up on MusicBrainz yet", later, now)?;
            return Ok(Some(false));
        };
        let tried: Vec<(String, String)> = downloads::for_release(conn, key)?
            .into_iter()
            .map(|d| (d.username, d.dir))
            .collect();
        self.report.working(format!("looking for {title}"));
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
        // Nothing yet: anything is better.
        let queued = crate::fallback::queue_upgrade(
            &search,
            &Quality::default(),
            self.loaded.config.upgrade.target,
            &[],
            &|| shutdown.is_stopping(),
        )
        .await;
        let conn = &self.state.conn;
        let queued = match queued {
            Ok(Some(queued)) => queued,
            Ok(None) => {
                tracing::info!("{title}: not on Soulseek yet");
                requests::checked(conn, key, "not on Soulseek yet", later, now)?;
                return Ok(Some(false));
            }
            Err(e) => {
                tracing::warn!("request for {title}: {e:#}");
                requests::checked(conn, key, &format!("looking failed: {e:#}"), later, now)?;
                return Ok(None);
            }
        };
        downloads::add(
            conn,
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
        let quality = queued.folder.audio.label();
        tracing::info!("requested {key}: queued {quality}");
        requests::checked(
            conn,
            key,
            &format!("queued {quality} from {}", queued.folder.username),
            later,
            now,
        )?;
        emit(Event::Download {
            key: key.to_string(),
            state: DOWNLOADING.into(),
        });
        Ok(Some(true))
    }
}

impl Work<'_> {
    /// Albums you requested, just filed (each download, and its album's
    /// folder now; scanned already): the requests are done, and you're
    /// told.
    pub(crate) async fn after_requests(
        &mut self,
        filed: Vec<(downloads::Download, String)>,
    ) -> anyhow::Result<Option<String>> {
        let mut notes = Vec::new();
        for (download, dir) in filed {
            let key = &download.release_key;
            requests::remove(&self.state.conn, key)?;
            let (album, artist) = library::release_group_title(&self.state.conn, key)?
                .unwrap_or_else(|| (download.dir.rsplit('\\').next().unwrap_or("").into(), None));
            let quality = library::album_qualities(&self.state.conn, Some(key))?
                .pop()
                .map_or_else(String::new, |a| a.quality.label());
            notes.push(crate::notify::RequestNote {
                album,
                artist: artist.unwrap_or_default(),
                release_group_mbid: key.clone(),
                quality,
                folder: dir,
            });
        }
        if notes.is_empty() {
            return Ok(None);
        }
        let server = &self.loaded.config.server;
        let url = server
            .enabled
            .then(|| format!("http://{}/downloads", server.listen));
        Ok(self
            .tell(vec![crate::notify::request_message(notes, url)])
            .await)
    }
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

    const GROUP: &str = "11111111-0000-4000-8000-00000000000a";
    const RELEASE: &str = "22222222-0000-4000-8000-000000000002";

    /// slskd and MusicBrainz both: a search that finds a 320 kbps MP3 and
    /// a CD-quality FLAC of the album; queueing the FLAC.
    async fn soulseek(found: bool) -> MockServer {
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
        let responses = if found {
            serde_json::json!([
                { "username": "mp3", "hasFreeUploadSlot": true, "files": [
                    { "filename": "m\\Album\\01 One.mp3", "size": 1, "length": 200, "bitRate": 320 }] },
                { "username": "flac", "files": [
                    { "filename": "f\\Album\\01 One.flac", "size": 1, "length": 200,
                      "bitDepth": 16, "sampleRate": 44100 }] },
            ])
        } else {
            serde_json::json!([])
        };
        Mock::given(method("GET"))
            .and(path("/api/v0/searches/s1/responses"))
            .respond_with(ResponseTemplate::new(200).set_body_json(responses))
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/api/v0/transfers/downloads/batches"))
            .and(body_partial_json(serde_json::json!({ "username": "flac" })))
            .respond_with(
                ResponseTemplate::new(201).set_body_json(serde_json::json!({ "failures": [] })),
            )
            .expect(u64::from(found))
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

    fn setup(server: &MockServer, root: &std::path::Path) -> (crate::config::Loaded, State) {
        use std::os::unix::fs::PermissionsExt;
        // Notifications land in `told.json`.
        let script = root.join("notify.sh");
        std::fs::write(
            &script,
            format!(
                "#!/bin/sh\ncat >> {0}\necho >> {0}\n",
                root.join("told.json").display()
            ),
        )
        .unwrap();
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
        let config = format!(
            "version = 1\nlibraries = [{1:?}]\n[musicbrainz]\nserver = {0:?}\n\
             [slskd]\nenabled = true\nserver = {0:?}\napi_key = \"k\"\n\
             downloads_dir = {2:?}\noutput_dir = \"{1}/%{{album}}\"\n\
             [notification.command]\nbin = {3:?}\n",
            server.uri(),
            root.join("music").display().to_string(),
            root.join("downloads").display().to_string(),
            script.display().to_string(),
        );
        let loaded = crate::config::check_text(&config, "test", root).unwrap();
        let state = State::open_in_memory().unwrap();
        library::upsert_release_group(
            &state.conn,
            &library::NewReleaseGroup {
                mbid: GROUP.into(),
                title: "The Album".into(),
                artist_credit: Some("The Artist".into()),
                primary_type: Some("Album".into()),
                first_release_date: Some("2027-02-01".into()),
                ..library::NewReleaseGroup::default()
            },
            "musicbrainz",
            Timestamp::now(),
        )
        .unwrap();
        (loaded, state)
    }

    #[tokio::test]
    async fn a_requested_album_is_queued_once_found() {
        let dir = tempfile::tempdir().unwrap();
        let server = soulseek(true).await;
        let (loaded, mut state) = setup(&server, dir.path());
        let http = Http::new(None);
        let report = Reporter {
            service: "requester",
        };
        let shutdown = Shutdown::default();
        let mut work = Work {
            state: &mut state,
            loaded: &loaded,
            http: &http,
            report: &report,
            shutdown: &shutdown,
        };
        assert_eq!(work.requests().await.unwrap(), "0 requested; none due");
        request(&work.state.conn, GROUP).unwrap();
        assert_eq!(
            work.requests().await.unwrap(),
            "1 searched: 1 queued, 0 not on Soulseek yet; 1 requested"
        );
        let queued = downloads::for_release(&work.state.conn, GROUP).unwrap();
        assert_eq!(queued.len(), 1);
        assert_eq!(queued[0].username, "flac", "the one meeting the target");
        assert!(queued[0].upgrade.is_none());
        // On its way: not again, even when asked.
        request(&work.state.conn, GROUP).unwrap();
        assert_eq!(work.requests().await.unwrap(), "1 requested; none due");

        // Filed and scanned (as the importer does): the request's done,
        // and you're told.
        library::upsert_file(
            &work.state.conn,
            &library::ScannedFile {
                path: "/music/The Album/1. One.flac".into(),
                library: "/music".into(),
                dir: "/music/The Album".into(),
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
            Timestamp::now(),
        )
        .unwrap();
        let told = work
            .after_requests(vec![(queued[0].clone(), "/music/The Album".into())])
            .await
            .unwrap();
        assert_eq!(told.as_deref(), Some("told you (1 sent)"));
        assert!(requests::all(&work.state.conn).unwrap().is_empty());
        let told: serde_json::Value = serde_json::from_str(
            std::fs::read_to_string(dir.path().join("told.json"))
                .unwrap()
                .trim(),
        )
        .unwrap();
        assert_eq!(told["kind"], "request");
        assert_eq!(told["title"], "Downloaded: The Artist");
        assert_eq!(
            told["message"],
            "The Album (FLAC 16-bit/44.1 kHz) is in your library."
        );
        assert_eq!(told["requests"][0]["folder"], "/music/The Album");
        assert_eq!(work.requests().await.unwrap(), "0 requested; none due");
    }

    #[tokio::test]
    async fn a_request_not_found_waits_for_the_next_look() {
        let dir = tempfile::tempdir().unwrap();
        let server = soulseek(false).await;
        let (loaded, mut state) = setup(&server, dir.path());
        request(&state.conn, GROUP).unwrap();
        let http = Http::new(None);
        let report = Reporter {
            service: "requester",
        };
        let shutdown = Shutdown::default();
        let mut work = Work {
            state: &mut state,
            loaded: &loaded,
            http: &http,
            report: &report,
            shutdown: &shutdown,
        };
        assert_eq!(
            work.requests().await.unwrap(),
            "1 searched: 0 queued, 1 not on Soulseek yet; 1 requested"
        );
        let request_now = requests::all(&work.state.conn)
            .unwrap()
            .remove(GROUP)
            .unwrap();
        assert_eq!(request_now.result.as_deref(), Some("not on Soulseek yet"));
        let next = request_now.next_at.unwrap();
        let checked = request_now.checked_at.unwrap();
        assert_eq!(next.duration_since(checked).as_secs(), 6 * 3600);
        assert_eq!(work.requests().await.unwrap(), "1 requested; none due");
        // Asked again: right away, ignored or not.
        library::set_ignored(&work.state.conn, GROUP, true, Timestamp::now()).unwrap();
        request(&work.state.conn, GROUP).unwrap();
        assert!(work.requests().await.unwrap().starts_with("1 searched"));
        unrequest(&work.state.conn, GROUP).unwrap();
        assert_eq!(work.requests().await.unwrap(), "0 requested; none due");
    }

    #[test]
    fn summaries() {
        let tally = Tally {
            requested: 4,
            searched: 3,
            queued: 1,
            nothing: 1,
            failed: 1,
        };
        assert_eq!(
            tally.summary(),
            "3 searched: 1 queued, 1 not on Soulseek yet, 1 failed; 4 requested"
        );
    }
}
