//! slskd (<https://github.com/slskd/slskd>), a Soulseek client with a web
//! API: searching it for an album, and queueing a folder's files.
//!
//! A search runs in the background: [`Slskd::start_search`] returns at
//! once, [`Slskd::search`] tells how it's going, and its responses are
//! there once it's complete ([`Slskd::responses`]).

use std::collections::BTreeMap;
use std::time::Duration;

use serde::Deserialize;
use serde_json::{Value, json};

use crate::config::SlskdConfig;

pub const DEFAULT_SERVER: &str = "http://localhost:5030";

/// A client for the configured slskd.
#[derive(Debug, Clone)]
pub struct Slskd {
    client: reqwest::Client,
    server: String,
}

/// How a search is going.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SearchState {
    pub complete: bool,
    /// The users who answered so far.
    pub responses: u64,
    pub files: u64,
}

/// A user's answer to a search.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Response {
    pub username: String,
    #[serde(default)]
    pub has_free_upload_slot: bool,
    #[serde(default)]
    pub queue_length: u64,
    /// Bytes a second.
    #[serde(default)]
    pub upload_speed: u64,
    #[serde(default)]
    pub files: Vec<File>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct File {
    /// The full path on the user's side, with `\` between folders.
    pub filename: String,
    pub size: u64,
    pub bit_rate: Option<u32>,
    pub bit_depth: Option<u32>,
    pub sample_rate: Option<u32>,
    /// Seconds.
    pub length: Option<u32>,
}

/// A folder of audio files someone shares: what a download takes.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Folder {
    pub username: String,
    /// The folder's path on the user's side.
    pub dir: String,
    /// The audio files in it, by name.
    pub files: Vec<(String, u64)>,
    /// Each file's length in seconds, when the sharer says.
    pub lengths: Vec<Option<u32>>,
    pub size: u64,
    /// Seconds, when every file says.
    pub length: Option<u64>,
    /// `FLAC`, `MP3`, … (the most common one, then any others).
    pub format: String,
    /// `24-bit/96 kHz`, `320 kbps`, … when the files say.
    pub quality: Option<String>,
    /// The same, to compare: its worst file's.
    pub audio: crate::quality::Quality,
    pub lossless: bool,
    pub free_slot: bool,
    pub queue: u64,
    pub speed: u64,
}

impl Folder {
    /// The folder's own name (the last part of `dir`).
    #[must_use]
    pub fn name(&self) -> &str {
        self.dir.rsplit('\\').next().unwrap_or(&self.dir)
    }

    /// Its files, to match against releases.
    #[must_use]
    pub fn candidates(&self) -> Vec<crate::matching::Candidate> {
        self.files
            .iter()
            .zip(&self.lengths)
            .map(|((filename, _), length)| crate::matching::Candidate {
                filename: filename.clone(),
                length: *length,
            })
            .collect()
    }
}

use crate::quality::LOSSLESS;

impl Slskd {
    /// # Errors
    ///
    /// When there's no API key (or the variable it names isn't set).
    ///
    /// # Panics
    ///
    /// If the TLS backend can't start.
    pub fn new(config: &SlskdConfig) -> anyhow::Result<Self> {
        let key = crate::config::secret::resolve(&config.api_key)
            .ok_or_else(|| anyhow::anyhow!("slskd.api_key isn't set"))?;
        let mut headers = reqwest::header::HeaderMap::new();
        let mut value = reqwest::header::HeaderValue::from_str(&key)
            .map_err(|_| anyhow::anyhow!("slskd.api_key has characters a header can't"))?;
        value.set_sensitive(true);
        headers.insert("x-api-key", value);
        let client = reqwest::Client::builder()
            .user_agent(crate::http::user_agent(None))
            .default_headers(headers)
            .connect_timeout(Duration::from_secs(10))
            .timeout(Duration::from_secs(30))
            .build()
            .expect("an HTTP client");
        Ok(Self {
            client,
            server: config.server.trim_end_matches('/').to_string(),
        })
    }

    fn url(&self, segments: &[&str]) -> anyhow::Result<reqwest::Url> {
        let mut url = reqwest::Url::parse(&format!("{}/api/v0", self.server))?;
        url.path_segments_mut()
            .map_err(|()| anyhow::anyhow!("slskd.server can't be a base URL"))?
            .extend(segments);
        Ok(url)
    }

    async fn send(&self, request: reqwest::RequestBuilder) -> anyhow::Result<Option<Value>> {
        let response = request
            .header("accept", "application/json")
            .send()
            .await
            .map_err(|e| anyhow::anyhow!("slskd: {e}"))?;
        let status = response.status();
        if status == reqwest::StatusCode::NOT_FOUND {
            return Ok(None);
        }
        if !status.is_success() {
            let text = response.text().await.unwrap_or_default();
            let text = text.trim();
            if matches!(status.as_u16(), 401 | 403) {
                anyhow::bail!("slskd answered {status}: check slskd.api_key");
            }
            if text.is_empty() {
                anyhow::bail!("slskd answered {status}");
            }
            anyhow::bail!("slskd answered {status}: {text}");
        }
        let text = response
            .text()
            .await
            .map_err(|e| anyhow::anyhow!("slskd: {e}"))?;
        if text.trim().is_empty() {
            return Ok(Some(Value::Null));
        }
        serde_json::from_str(&text)
            .map(Some)
            .map_err(|e| anyhow::anyhow!("slskd: unexpected answer: {e}"))
    }

    /// Start searching for `text`; returns the search's id.
    ///
    /// # Errors
    ///
    /// When slskd can't be reached, or won't search (it isn't connected to
    /// Soulseek, say).
    pub async fn start_search(&self, text: &str) -> anyhow::Result<String> {
        let url = self.url(&["searches"])?;
        tracing::debug!("POST {url} ({text})");
        let answer = self
            .send(self.client.post(url).json(&json!({ "searchText": text })))
            .await?
            .ok_or_else(|| anyhow::anyhow!("slskd answered 404: is slskd.server right?"))?;
        answer["id"]
            .as_str()
            .map(str::to_string)
            .ok_or_else(|| anyhow::anyhow!("slskd: unexpected answer: no search id"))
    }

    /// How search `id` is going (`None`: slskd doesn't know it).
    ///
    /// # Errors
    ///
    /// When slskd can't be reached.
    pub async fn search(&self, id: &str) -> anyhow::Result<Option<SearchState>> {
        let url = self.url(&["searches", id])?;
        let Some(answer) = self.send(self.client.get(url)).await? else {
            return Ok(None);
        };
        let complete = answer["isComplete"].as_bool().unwrap_or_else(|| {
            answer["state"]
                .as_str()
                .is_some_and(|s| s.contains("Completed"))
        });
        Ok(Some(SearchState {
            complete,
            responses: answer["responseCount"].as_u64().unwrap_or(0),
            files: answer["fileCount"].as_u64().unwrap_or(0),
        }))
    }

    /// What search `id` found (empty until it's complete).
    ///
    /// # Errors
    ///
    /// When slskd can't be reached, or answers something else.
    pub async fn responses(&self, id: &str) -> anyhow::Result<Vec<Response>> {
        let url = self.url(&["searches", id, "responses"])?;
        match self.send(self.client.get(url)).await? {
            None | Some(Value::Null) => Ok(Vec::new()),
            Some(answer) => serde_json::from_value(answer)
                .map_err(|e| anyhow::anyhow!("slskd: unexpected answer: {e}")),
        }
    }

    /// Queue `files` (full names and sizes) from `username`.
    ///
    /// # Errors
    ///
    /// When slskd can't be reached, or won't (the user's offline, say).
    pub async fn enqueue(&self, username: &str, files: &[(String, u64)]) -> anyhow::Result<()> {
        let url = self.url(&["transfers", "downloads", username])?;
        tracing::debug!("POST {url} ({} files)", files.len());
        let body: Vec<Value> = files
            .iter()
            .map(|(filename, size)| json!({ "filename": filename, "size": size }))
            .collect();
        self.send(self.client.post(url).json(&body))
            .await?
            .ok_or_else(|| anyhow::anyhow!("slskd answered 404: is slskd.server right?"))?;
        Ok(())
    }

    /// Queue `files` from `username` as a batch that slskd puts in
    /// `destination` (a folder under its downloads folder).
    ///
    /// # Errors
    ///
    /// When slskd can't be reached, or won't queue any of them (the user's
    /// offline, say).
    pub async fn enqueue_into(
        &self,
        username: &str,
        files: &[(String, u64)],
        destination: &str,
    ) -> anyhow::Result<()> {
        let url = self.url(&["transfers", "downloads", "batches"])?;
        tracing::debug!("POST {url} ({} files into {destination})", files.len());
        let body = json!({
            "username": username,
            "files": files
                .iter()
                .map(|(filename, size)| json!({ "filename": filename, "size": size }))
                .collect::<Vec<_>>(),
            "options": { "destination": destination },
        });
        let answer = self
            .send(self.client.post(url).json(&body))
            .await?
            .ok_or_else(|| anyhow::anyhow!("slskd answered 404: the user may be offline, or this slskd is older than 0.26 (no batches)"))?;
        let failures = answer["failures"].as_array().map_or(0, Vec::len);
        if failures > 0 && failures >= files.len() {
            let why = answer["failures"][0]["message"]
                .as_str()
                .unwrap_or("no reason given");
            anyhow::bail!("slskd queued none of the files: {why}");
        }
        if failures > 0 {
            tracing::warn!("slskd: {failures} of {} files weren't queued", files.len());
        }
        Ok(())
    }

    /// Cancel download `id` (a transfer's) and take it off slskd's list.
    /// What's been downloaded of it stays where it is.
    ///
    /// # Errors
    ///
    /// When slskd can't be reached, or won't.
    pub async fn cancel(&self, username: &str, id: &str) -> anyhow::Result<()> {
        let mut url = self.url(&["transfers", "downloads", username, id])?;
        url.query_pairs_mut().append_pair("remove", "true");
        tracing::debug!("DELETE {url}");
        // 404: it's gone already.
        self.send(self.client.delete(url)).await?;
        Ok(())
    }

    /// What's being (or was) downloaded from `username`.
    ///
    /// # Errors
    ///
    /// When slskd can't be reached, or answers something else.
    pub async fn transfers(&self, username: &str) -> anyhow::Result<Vec<Transfer>> {
        let url = self.url(&["transfers", "downloads", username])?;
        let Some(answer) = self.send(self.client.get(url)).await? else {
            return Ok(Vec::new());
        };
        let user: UserTransfers = serde_json::from_value(answer)
            .map_err(|e| anyhow::anyhow!("slskd: unexpected answer: {e}"))?;
        Ok(user.directories.into_iter().flat_map(|d| d.files).collect())
    }
}

#[derive(Debug, Deserialize)]
struct UserTransfers {
    #[serde(default)]
    directories: Vec<TransferDir>,
}

#[derive(Debug, Deserialize)]
struct TransferDir {
    #[serde(default)]
    files: Vec<Transfer>,
}

/// A file being (or that was) downloaded.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Transfer {
    /// slskd's id for it.
    #[serde(default)]
    pub id: String,
    pub filename: String,
    /// `Queued, Remotely`, `InProgress`, `Completed, Succeeded`, …
    pub state: String,
    #[serde(default)]
    pub bytes_transferred: u64,
    /// Bytes a second.
    #[serde(default)]
    pub average_speed: f64,
    pub place_in_queue: Option<u64>,
    #[serde(default)]
    pub requested_at: String,
}

/// How a queued folder's files are doing.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Progress {
    pub total: usize,
    pub done: usize,
    pub failed: usize,
    /// Of the failed ones: cancelled (by you, in slskd).
    pub cancelled: usize,
    pub active: usize,
    pub queued: usize,
    /// Not in slskd (anymore): removed there.
    pub gone: usize,
    pub bytes: u64,
    pub transferred: u64,
    /// Bytes a second, of the files downloading now.
    pub speed: f64,
    /// The best place in the user's queue, when they say.
    pub place: Option<u64>,
}

impl Progress {
    /// Nothing's waiting or downloading.
    #[must_use]
    pub fn finished(&self) -> bool {
        self.active == 0 && self.queued == 0
    }

    #[must_use]
    pub fn percent(&self) -> u64 {
        if self.bytes == 0 {
            return 0;
        }
        #[allow(
            clippy::cast_precision_loss,
            clippy::cast_possible_truncation,
            clippy::cast_sign_loss
        )]
        let percent = (self.transferred as f64 / self.bytes as f64 * 100.0).floor() as u64;
        percent.min(100)
    }

    /// `ok`, `active`, `queued`, `failed` or `gone`: how it went, in a word.
    #[must_use]
    pub fn status(&self) -> &'static str {
        if self.total > 0 && self.done == self.total {
            "ok"
        } else if self.gone == self.total {
            "gone"
        } else if self.active > 0 || (self.done > 0 && !self.finished()) {
            "active"
        } else if self.queued > 0 {
            "queued"
        } else {
            "failed"
        }
    }

    #[must_use]
    pub fn label(&self) -> String {
        match self.status() {
            "ok" => "Downloaded".into(),
            "gone" => "Not in slskd anymore".into(),
            "active" => format!(
                "Downloading {}/{} · {}%",
                self.done,
                self.total,
                self.percent()
            ),
            "queued" => match self.place {
                Some(place) => format!("Queued · #{place} in their queue"),
                None => "Queued".into(),
            },
            _ => {
                let missing = self.failed + self.gone;
                format!("{missing} of {} failed", self.total)
            }
        }
    }
}

/// How `files` (a queued folder's) are doing, by `transfers` (from the
/// same user; the latest for each file counts).
#[must_use]
pub fn progress(files: &[(String, u64)], transfers: &[Transfer]) -> Progress {
    let mut out = Progress {
        total: files.len(),
        ..Progress::default()
    };
    for (filename, size) in files {
        out.bytes += size;
        let transfer = transfers
            .iter()
            .filter(|t| &t.filename == filename)
            .max_by(|a, b| a.requested_at.cmp(&b.requested_at));
        let Some(transfer) = transfer else {
            out.gone += 1;
            continue;
        };
        let state = transfer.state.as_str();
        if state.contains("Succeeded") {
            out.done += 1;
            out.transferred += size;
        } else if state.contains("Completed") {
            out.failed += 1;
            out.cancelled += usize::from(state.contains("Cancelled"));
        } else if state.contains("InProgress") || state.contains("Initializing") {
            out.active += 1;
            out.transferred += transfer.bytes_transferred.min(*size);
            out.speed += transfer.average_speed;
        } else {
            out.queued += 1;
            if let Some(place) = transfer.place_in_queue {
                out.place = Some(out.place.map_or(place, |p| p.min(place)));
            }
        }
    }
    out
}

/// What to search Soulseek for to find `album` by `artist`: their words,
/// without punctuation (which Soulseek's search doesn't do well with).
#[must_use]
pub fn search_text(artist: &str, album: &str) -> String {
    format!("{artist} {album}")
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

/// The audio folders in `responses`, best first: a free upload slot, then
/// lossless, then the fastest.
#[must_use]
pub fn folders(responses: &[Response]) -> Vec<Folder> {
    let mut out = Vec::new();
    for response in responses {
        let mut dirs: BTreeMap<&str, Vec<&File>> = BTreeMap::new();
        for file in &response.files {
            if extension(&file.filename).is_some_and(|e| crate::scan::AUDIO.contains(&e.as_str())) {
                let dir = file.filename.rsplit_once('\\').map_or("", |(dir, _)| dir);
                dirs.entry(dir).or_default().push(file);
            }
        }
        for (dir, mut files) in dirs {
            files.sort_by(|a, b| a.filename.cmp(&b.filename));
            out.push(folder(response, dir, &files));
        }
    }
    out.sort_by(|a, b| {
        b.free_slot
            .cmp(&a.free_slot)
            .then(b.lossless.cmp(&a.lossless))
            .then(a.queue.cmp(&b.queue))
            .then(b.speed.cmp(&a.speed))
    });
    out
}

fn folder(response: &Response, dir: &str, files: &[&File]) -> Folder {
    let mut formats: BTreeMap<String, usize> = BTreeMap::new();
    for file in files {
        *formats
            .entry(extension(&file.filename).unwrap_or_default())
            .or_default() += 1;
    }
    let mut formats: Vec<(String, usize)> = formats.into_iter().collect();
    formats.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
    let main = formats.first().map_or_default(|(e, _)| e.clone());
    let lossless = LOSSLESS.contains(&main.as_str());
    let format = formats
        .iter()
        .map(|(e, _)| e.to_ascii_uppercase())
        .collect::<Vec<_>>()
        .join(" + ");
    Folder {
        username: response.username.clone(),
        dir: dir.to_string(),
        files: files.iter().map(|f| (f.filename.clone(), f.size)).collect(),
        lengths: files.iter().map(|f| f.length).collect(),
        size: files.iter().map(|f| f.size).sum(),
        length: files
            .iter()
            .map(|f| f.length.map(u64::from))
            .sum::<Option<u64>>(),
        quality: quality(files, lossless),
        audio: crate::quality::Quality {
            format: main.to_ascii_uppercase(),
            lossless,
            bit_depth: files.iter().filter_map(|f| f.bit_depth).min(),
            sample_rate: files.iter().filter_map(|f| f.sample_rate).min(),
            bitrate: files.iter().filter_map(|f| f.bit_rate).min(),
        },
        format,
        lossless,
        free_slot: response.has_free_upload_slot,
        queue: response.queue_length,
        speed: response.upload_speed,
    }
}

/// The files' quality, when they agree on it (or the lowest bitrate).
fn quality(files: &[&File], lossless: bool) -> Option<String> {
    if lossless {
        let depth = same(files.iter().map(|f| f.bit_depth))?;
        let rate = same(files.iter().map(|f| f.sample_rate))?;
        let khz = f64::from(rate) / 1000.0;
        Some(format!("{depth}-bit/{} kHz", trim_float(khz)))
    } else {
        let lowest = files.iter().filter_map(|f| f.bit_rate).min()?;
        let varies = same(files.iter().map(|f| f.bit_rate)).is_none();
        Some(if varies {
            format!("{lowest}+ kbps")
        } else {
            format!("{lowest} kbps")
        })
    }
}

fn same(mut values: impl Iterator<Item = Option<u32>>) -> Option<u32> {
    let first = values.next()??;
    values.all(|v| v == Some(first)).then_some(first)
}

fn trim_float(value: f64) -> String {
    let text = format!("{value:.1}");
    text.strip_suffix(".0").unwrap_or(&text).to_string()
}

fn extension(filename: &str) -> Option<String> {
    let name = filename.rsplit(['\\', '/']).next()?;
    let (_, ext) = name.rsplit_once('.')?;
    Some(ext.to_ascii_lowercase())
}

#[cfg(test)]
mod tests {
    use super::*;
    use wiremock::matchers::{body_json, header, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn file(
        name: &str,
        size: u64,
        depth: Option<u32>,
        rate: Option<u32>,
        kbps: Option<u32>,
    ) -> File {
        File {
            filename: name.into(),
            size,
            bit_rate: kbps,
            bit_depth: depth,
            sample_rate: rate,
            length: Some(200),
        }
    }

    fn response(user: &str, free: bool, speed: u64, files: Vec<File>) -> Response {
        Response {
            username: user.into(),
            has_free_upload_slot: free,
            queue_length: 0,
            upload_speed: speed,
            files,
        }
    }

    #[test]
    fn groups_audio_files_by_folder_best_first() {
        let responses = vec![
            response(
                "mp3er",
                true,
                100,
                vec![
                    file("@@a\\Music\\Album\\01.mp3", 5, None, None, Some(320)),
                    file("@@a\\Music\\Album\\02.mp3", 5, None, None, Some(256)),
                    file("@@a\\Music\\Album\\cover.jpg", 1, None, None, None),
                ],
            ),
            response(
                "busy",
                false,
                9999,
                vec![file("x\\Album\\01.flac", 30, Some(16), Some(44100), None)],
            ),
            response(
                "flacer",
                true,
                50,
                vec![
                    file("m\\Album [24-96]\\02.flac", 60, Some(24), Some(96000), None),
                    file("m\\Album [24-96]\\01.flac", 60, Some(24), Some(96000), None),
                    file("m\\Album [24-96]\\notes.txt", 1, None, None, None),
                    file("m\\Other\\01.flac", 10, Some(16), Some(44100), None),
                ],
            ),
        ];
        let folders = folders(&responses);
        let order: Vec<(&str, &str)> = folders
            .iter()
            .map(|f| (f.username.as_str(), f.name()))
            .collect();
        assert_eq!(
            order,
            [
                ("flacer", "Album [24-96]"),
                ("flacer", "Other"),
                ("mp3er", "Album"),
                ("busy", "Album")
            ]
        );
        let best = &folders[0];
        assert_eq!(best.dir, "m\\Album [24-96]");
        assert_eq!(
            best.files,
            [
                ("m\\Album [24-96]\\01.flac".to_string(), 60),
                ("m\\Album [24-96]\\02.flac".to_string(), 60)
            ]
        );
        assert_eq!((best.size, best.length), (120, Some(400)));
        assert_eq!(best.format, "FLAC");
        assert_eq!(best.quality.as_deref(), Some("24-bit/96 kHz"));
        assert_eq!(best.audio.tier(), crate::quality::Tier::HiRes);
        assert_eq!(folders[1].quality.as_deref(), Some("16-bit/44.1 kHz"));
        assert_eq!(folders[2].format, "MP3");
        assert_eq!(folders[2].quality.as_deref(), Some("256+ kbps"));
        assert_eq!(folders[2].files.len(), 2, "the cover isn't a track");
    }

    fn config(server: &str) -> SlskdConfig {
        SlskdConfig {
            enabled: true,
            server: server.into(),
            api_key: "the-key".into(),
            ..SlskdConfig::default()
        }
    }

    #[tokio::test]
    async fn searches_and_queues_with_the_api_key() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/api/v0/searches"))
            .and(header("x-api-key", "the-key"))
            .and(body_json(json!({ "searchText": "Turnstile Glow On" })))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "id": "0b5f…", "isComplete": false, "state": "InProgress",
                "responseCount": 0, "fileCount": 0
            })))
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/api/v0/searches/s1"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "id": "s1", "state": "Completed, TimedOut",
                "responseCount": 3, "fileCount": 40
            })))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/api/v0/searches/s1/responses"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!([{
                "username": "someone", "hasFreeUploadSlot": true, "queueLength": 0,
                "uploadSpeed": 1000, "fileCount": 1, "lockedFileCount": 0,
                "files": [{ "filename": "a\\b\\01.flac", "size": 10, "code": 1,
                            "extension": "", "bitDepth": 16, "sampleRate": 44100,
                            "length": 180, "isLocked": false }],
                "lockedFiles": [], "token": 1
            }])))
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/api/v0/transfers/downloads/batches"))
            .and(body_json(json!({
                "username": "some one",
                "files": [{ "filename": "a\\b\\01.flac", "size": 10 }],
                "options": { "destination": "newcut/x1" }
            })))
            .respond_with(ResponseTemplate::new(201).set_body_json(json!({
                "batch": { "id": "b" }, "failures": []
            })))
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/api/v0/transfers/downloads/some%20one"))
            .and(body_json(
                json!([{ "filename": "a\\b\\01.flac", "size": 10 }]),
            ))
            .respond_with(ResponseTemplate::new(201).set_body_json(json!({
                "enqueued": [], "failed": []
            })))
            .expect(1)
            .mount(&server)
            .await;

        let slskd = Slskd::new(&config(&server.uri())).unwrap();
        assert_eq!(
            slskd.start_search("Turnstile Glow On").await.unwrap(),
            "0b5f…"
        );
        assert_eq!(
            slskd.search("s1").await.unwrap(),
            Some(SearchState {
                complete: true,
                responses: 3,
                files: 40
            })
        );
        assert_eq!(slskd.search("gone").await.unwrap(), None);
        let responses = slskd.responses("s1").await.unwrap();
        assert_eq!(responses[0].files[0].bit_depth, Some(16));
        slskd
            .enqueue("some one", &[("a\\b\\01.flac".into(), 10)])
            .await
            .unwrap();
        slskd
            .enqueue_into("some one", &[("a\\b\\01.flac".into(), 10)], "newcut/x1")
            .await
            .unwrap();
    }

    fn transfer(name: &str, state: &str, bytes: u64, at: &str) -> Transfer {
        Transfer {
            id: String::new(),
            filename: name.into(),
            state: state.into(),
            bytes_transferred: bytes,
            average_speed: 1000.0,
            place_in_queue: Some(7),
            requested_at: at.into(),
        }
    }

    #[test]
    fn sums_up_a_folders_transfers() {
        let files: Vec<(String, u64)> = (1..=4).map(|i| (format!("d\\0{i}.flac"), 100)).collect();
        let none = progress(&files, &[]);
        assert_eq!(
            (none.status(), none.label().as_str()),
            ("gone", "Not in slskd anymore")
        );

        let queued: Vec<Transfer> = files
            .iter()
            .map(|(f, _)| transfer(f, "Queued, Remotely", 0, "1"))
            .collect();
        let p = progress(&files, &queued);
        assert_eq!(p.label(), "Queued · #7 in their queue");

        let mut going = vec![
            transfer("d\\01.flac", "Completed, Succeeded", 100, "1"),
            transfer("d\\02.flac", "InProgress", 50, "1"),
            transfer("d\\03.flac", "Queued, Remotely", 0, "1"),
            transfer("d\\04.flac", "Completed, Errored", 0, "1"),
        ];
        let p = progress(&files, &going);
        assert_eq!((p.done, p.active, p.queued, p.failed), (1, 1, 1, 1));
        assert_eq!(p.label(), "Downloading 1/4 · 37%");

        going[1].state = "Completed, Succeeded".into();
        going[2].state = "Completed, TimedOut".into();
        let p = progress(&files, &going);
        assert!(p.finished());
        assert_eq!(p.label(), "2 of 4 failed");

        // Retried: the latest try counts.
        going.push(transfer("d\\03.flac", "Completed, Succeeded", 100, "2"));
        going.push(transfer("d\\04.flac", "Completed, Succeeded", 100, "2"));
        let p = progress(&files, &going);
        assert_eq!(
            (p.status(), p.label().as_str(), p.percent()),
            ("ok", "Downloaded", 100)
        );
    }

    #[tokio::test]
    async fn says_when_the_key_is_wrong() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/api/v0/searches"))
            .respond_with(ResponseTemplate::new(401))
            .mount(&server)
            .await;
        let slskd = Slskd::new(&config(&server.uri())).unwrap();
        let error = slskd.start_search("x").await.unwrap_err().to_string();
        assert!(error.contains("slskd.api_key"), "{error}");
        let missing = SlskdConfig {
            api_key: String::new(),
            ..config(&server.uri())
        };
        assert!(Slskd::new(&missing).is_err());
    }
}
