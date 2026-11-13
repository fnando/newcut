//! The HTTP client for MusicBrainz, ListenBrainz and Pushover, and the spacing
//! between requests each server asks for.

use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use serde_json::Value;

/// A client. Each service makes its own (clients hold connections that
/// belong to the runtime they were made on).
#[derive(Debug, Clone)]
pub struct Http {
    client: reqwest::Client,
}

/// newcut's User-Agent, with how to reach whoever runs it.
#[must_use]
pub fn user_agent(contact: Option<&str>) -> String {
    let version = env!("CARGO_PKG_VERSION");
    match contact.filter(|c| !c.trim().is_empty()) {
        Some(contact) => format!("newcut/{version} ( {} )", contact.trim()),
        None => format!("newcut/{version} ( https://github.com/fnando/newcut )"),
    }
}

/// What a request got.
#[derive(Debug)]
pub enum Answer {
    Json(Value),
    NotFound,
}

impl Http {
    /// # Panics
    ///
    /// If the TLS backend can't start.
    #[must_use]
    pub fn new(contact: Option<&str>) -> Self {
        let client = reqwest::Client::builder()
            .user_agent(user_agent(contact))
            .connect_timeout(Duration::from_secs(10))
            .timeout(Duration::from_secs(30))
            .build()
            .expect("an HTTP client");
        Self { client }
    }

    /// GET `url` as JSON, waiting `spacing` since the last request to the
    /// same `host`, and backing off while it answers 503 or 429.
    ///
    /// # Errors
    ///
    /// When the request fails, the server answers with an error (or stays
    /// busy), or the answer isn't JSON. `name` says who answered.
    pub async fn get_json(
        &self,
        name: &str,
        host: &str,
        spacing: Duration,
        url: &str,
    ) -> anyhow::Result<Answer> {
        self.fetch_json(name, host, spacing, url, 4).await
    }

    /// GET `url` as JSON once: a 503 or 429 is an error right away (for
    /// answers someone is waiting for).
    ///
    /// # Errors
    ///
    /// As [`Http::get_json`].
    pub async fn get_json_once(
        &self,
        name: &str,
        host: &str,
        spacing: Duration,
        url: &str,
    ) -> anyhow::Result<Answer> {
        self.fetch_json(name, host, spacing, url, 1).await
    }

    async fn fetch_json(
        &self,
        name: &str,
        host: &str,
        spacing: Duration,
        url: &str,
        attempts: u32,
    ) -> anyhow::Result<Answer> {
        let mut backoff = Duration::from_secs(2);
        for attempt in 0..attempts {
            wait_turn(host, spacing).await;
            tracing::debug!("GET {url}");
            let response = self
                .client
                .get(url)
                .header("accept", "application/json")
                .send()
                .await
                .map_err(|e| anyhow::anyhow!("{name}: {e}"))?;
            let status = response.status();
            if matches!(status.as_u16(), 503 | 429) && attempt + 1 < attempts {
                tokio::time::sleep(backoff).await;
                backoff *= 2;
                continue;
            }
            if status.as_u16() == 404 {
                return Ok(Answer::NotFound);
            }
            if !status.is_success() {
                anyhow::bail!("{name} answered {status}");
            }
            let json = response
                .json()
                .await
                .map_err(|e| anyhow::anyhow!("{name}: unexpected answer: {e}"))?;
            return Ok(Answer::Json(json));
        }
        anyhow::bail!("{name} is busy; try again later")
    }

    /// GET `url`'s body (`None`: not found).
    ///
    /// # Errors
    ///
    /// When the request fails, or the server answers with another error.
    pub async fn get_bytes(&self, name: &str, url: &str) -> anyhow::Result<Option<Vec<u8>>> {
        tracing::debug!("GET {url}");
        let response = self.client.get(url).send().await.map_err(|e| {
            // reqwest's message alone hides why (a timeout, TLS, DNS…).
            let mut why = e.to_string();
            let mut source = std::error::Error::source(&e);
            while let Some(cause) = source {
                why.push_str(": ");
                why.push_str(&cause.to_string());
                source = cause.source();
            }
            anyhow::anyhow!("{name}: {why}")
        })?;
        let status = response.status();
        if status.as_u16() == 404 {
            return Ok(None);
        }
        if !status.is_success() {
            anyhow::bail!("{name} answered {status}");
        }
        let body = response
            .bytes()
            .await
            .map_err(|e| anyhow::anyhow!("{name}: {e}"))?;
        Ok(Some(body.to_vec()))
    }

    /// POST a form to `url`; returns the status and the JSON answer (`null`
    /// when it isn't JSON).
    ///
    /// # Errors
    ///
    /// When the request fails. `name` says who was asked.
    pub async fn post_form(
        &self,
        name: &str,
        url: &str,
        form: &[(&str, &str)],
    ) -> anyhow::Result<(reqwest::StatusCode, Value)> {
        tracing::debug!("POST {url}");
        let response = self
            .client
            .post(url)
            .header("accept", "application/json")
            .form(form)
            .send()
            .await
            .map_err(|e| anyhow::anyhow!("{name}: {e}"))?;
        let status = response.status();
        let body = response.json().await.unwrap_or(Value::Null);
        Ok((status, body))
    }
}

/// Wait for `host`'s next slot: one request per `spacing`, across every
/// service in the process.
async fn wait_turn(host: &str, spacing: Duration) {
    if spacing.is_zero() {
        return;
    }
    let wait = next_slot(host, spacing, Instant::now());
    if !wait.is_zero() {
        tokio::time::sleep(wait).await;
    }
}

/// Take `host`'s next slot; returns how long until it.
fn next_slot(host: &str, spacing: Duration, now: Instant) -> Duration {
    static NEXT: OnceLock<Mutex<HashMap<String, Instant>>> = OnceLock::new();
    let mut next = NEXT
        .get_or_init(Mutex::default)
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let slot = next.get(host).copied().unwrap_or(now).max(now);
    next.insert(host.to_string(), slot + spacing);
    slot - now
}

/// The host part of a URL, for spacing requests.
#[must_use]
pub fn host(url: &str) -> &str {
    let rest = url.split_once("://").map_or(url, |(_, r)| r);
    rest.split(['/', '?']).next().unwrap_or(rest)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn requests_to_a_host_are_spaced() {
        let now = Instant::now();
        let spacing = Duration::from_millis(1100);
        let host = "spaced.test";
        assert_eq!(next_slot(host, spacing, now), Duration::ZERO);
        assert_eq!(next_slot(host, spacing, now), spacing);
        assert_eq!(next_slot(host, spacing, now), spacing * 2);
        assert_eq!(next_slot("other.test", spacing, now), Duration::ZERO);
        // Long after: right away again.
        assert_eq!(next_slot(host, spacing, now + spacing * 10), Duration::ZERO);
    }

    #[test]
    fn user_agents_say_how_to_reach_you() {
        assert!(user_agent(Some("me@example.com")).ends_with("( me@example.com )"));
        assert!(user_agent(Some("  ")).contains("github.com/fnando/newcut"));
    }

    #[test]
    fn hosts() {
        assert_eq!(host("https://musicbrainz.org/ws/2/x?y"), "musicbrainz.org");
        assert_eq!(host("http://127.0.0.1:5000/ws"), "127.0.0.1:5000");
    }
}
