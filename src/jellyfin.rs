//! Jellyfin (<https://jellyfin.org>): told about the albums newcut files,
//! so it scans their folders right away (instead of at its next library
//! scan).

use std::path::{Path, PathBuf};
use std::time::Duration;

use serde_json::json;

use crate::config::JellyfinConfig;
use crate::config::load::Loaded;

/// A client for the configured Jellyfin.
#[derive(Debug, Clone)]
pub struct Jellyfin {
    client: reqwest::Client,
    server: String,
}

impl Jellyfin {
    /// # Errors
    ///
    /// When there's no API key (or the variable it names isn't set).
    ///
    /// # Panics
    ///
    /// If the TLS backend can't start.
    pub fn new(config: &JellyfinConfig) -> anyhow::Result<Self> {
        let key = crate::config::secret::resolve(&config.api_key)
            .ok_or_else(|| anyhow::anyhow!("jellyfin.api_key isn't set"))?;
        let mut headers = reqwest::header::HeaderMap::new();
        // Not X-Emby-Token: recent Jellyfin turns it off (12.4 does).
        let mut value =
            reqwest::header::HeaderValue::from_str(&format!("MediaBrowser Token=\"{key}\""))
                .map_err(|_| anyhow::anyhow!("jellyfin.api_key has characters a header can't"))?;
        value.set_sensitive(true);
        headers.insert(reqwest::header::AUTHORIZATION, value);
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

    /// Tell Jellyfin `paths` (as it sees them) are new: it scans them.
    ///
    /// # Errors
    ///
    /// When Jellyfin can't be reached, or answers something else.
    pub async fn created(&self, paths: &[String]) -> anyhow::Result<()> {
        self.updated(paths, "Created").await
    }

    /// Tell Jellyfin `paths` (as it sees them) changed: it scans them
    /// again.
    ///
    /// # Errors
    ///
    /// When Jellyfin can't be reached, or answers something else.
    pub async fn modified(&self, paths: &[String]) -> anyhow::Result<()> {
        self.updated(paths, "Modified").await
    }

    async fn updated(&self, paths: &[String], kind: &str) -> anyhow::Result<()> {
        let url = format!("{}/Library/Media/Updated", self.server);
        let body = json!({
            "Updates": paths
                .iter()
                .map(|p| json!({ "Path": p, "UpdateType": kind }))
                .collect::<Vec<_>>(),
        });
        tracing::debug!("POST {url} ({})", paths.join(", "));
        let response = self
            .client
            .post(&url)
            .json(&body)
            .send()
            .await
            .map_err(|e| anyhow::anyhow!("jellyfin: {e}"))?;
        let status = response.status();
        if status.is_success() {
            return Ok(());
        }
        if matches!(status.as_u16(), 401 | 403) {
            anyhow::bail!("jellyfin answered {status}: check jellyfin.api_key");
        }
        let text = response.text().await.unwrap_or_default();
        let text = text.trim();
        if text.is_empty() {
            anyhow::bail!("jellyfin answered {status}");
        }
        anyhow::bail!("jellyfin answered {status}: {text}")
    }
}

/// `dir` as Jellyfin sees it: by the longest of `paths` (newcut's folder,
/// Jellyfin's) it's in; as it is when there's none.
#[must_use]
pub fn their_path(dir: &Path, paths: &[(PathBuf, String)]) -> String {
    paths
        .iter()
        .filter_map(|(ours, theirs)| Some((dir.strip_prefix(ours).ok()?, ours, theirs)))
        .max_by_key(|(_, ours, _)| ours.components().count())
        .map_or_else(
            || dir.display().to_string(),
            |(rest, _, theirs)| {
                let rest = rest.to_string_lossy().replace('\\', "/");
                if rest.is_empty() {
                    theirs.clone()
                } else {
                    format!("{theirs}/{rest}")
                }
            },
        )
}

/// Have Jellyfin scan `dirs` (newcut's) again; returns them as Jellyfin
/// sees them.
///
/// # Errors
///
/// When Jellyfin is off, or can't be reached, or answers something else.
pub async fn refresh(loaded: &Loaded, dirs: &[PathBuf]) -> anyhow::Result<Vec<String>> {
    let config = &loaded.config.jellyfin;
    if !config.enabled {
        anyhow::bail!("Jellyfin is off: set [jellyfin] enabled = true");
    }
    let paths: Vec<String> = dirs
        .iter()
        .map(|d| their_path(d, &loaded.jellyfin_paths))
        .collect();
    Jellyfin::new(config)?.modified(&paths).await?;
    tracing::info!("asked Jellyfin to scan {} again", paths.join(", "));
    Ok(paths)
}

/// Tell Jellyfin (when it's on) about the album folders just filed. Its
/// problems are logged: filing went fine either way.
pub async fn tell(loaded: &Loaded, dirs: &[PathBuf]) {
    let config = &loaded.config.jellyfin;
    if !config.enabled || dirs.is_empty() {
        return;
    }
    let paths: Vec<String> = dirs
        .iter()
        .map(|d| their_path(d, &loaded.jellyfin_paths))
        .collect();
    let told = match Jellyfin::new(config) {
        Ok(jellyfin) => jellyfin.created(&paths).await,
        Err(e) => Err(e),
    };
    match told {
        Ok(()) => tracing::info!("told Jellyfin about {}", paths.join(", ")),
        Err(e) => tracing::warn!("telling Jellyfin about {}: {e:#}", paths.join(", ")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use wiremock::matchers::{body_json, header, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    #[test]
    fn maps_folders_to_jellyfin_s() {
        let paths = vec![
            (
                PathBuf::from("/Volumes/storage/media"),
                "/data/media".into(),
            ),
            (
                PathBuf::from("/Volumes/storage/media/music"),
                "/music".into(),
            ),
        ];
        let at = |p: &str| their_path(Path::new(p), &paths);
        assert_eq!(
            at("/Volumes/storage/media/music/A/B (2001)"),
            "/music/A/B (2001)"
        );
        assert_eq!(at("/Volumes/storage/media/other/X"), "/data/media/other/X");
        assert_eq!(at("/Volumes/storage/media/music"), "/music");
        assert_eq!(
            at("/Volumes/storage/media-old/X"),
            "/Volumes/storage/media-old/X"
        );
        assert_eq!(at("/elsewhere/X"), "/elsewhere/X");
    }

    #[tokio::test]
    async fn tells_jellyfin_what_s_new() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/Library/Media/Updated"))
            .and(header("authorization", "MediaBrowser Token=\"k3y\""))
            .and(body_json(json!({
                "Updates": [{ "Path": "/music/A/B", "UpdateType": "Created" }]
            })))
            .respond_with(ResponseTemplate::new(204))
            .expect(1)
            .mount(&server)
            .await;
        let config = JellyfinConfig {
            enabled: true,
            server: format!("{}/", server.uri()),
            api_key: "k3y".into(),
            ..JellyfinConfig::default()
        };
        let jellyfin = Jellyfin::new(&config).unwrap();
        jellyfin.created(&["/music/A/B".into()]).await.unwrap();

        let wrong = Jellyfin::new(&JellyfinConfig {
            api_key: "nope".into(),
            ..config
        })
        .unwrap();
        let error = wrong.created(&["/x".into()]).await.unwrap_err();
        assert!(error.to_string().contains("404"), "{error}");
    }
}
