//! MusicBrainz: an artist's name and members, a release's release
//! group, and an artist's release groups (browse requests, which mirrors
//! without a search server answer too). musicbrainz.org gets one request a
//! second, as it asks.

use std::time::Duration;

use serde_json::Value;

use crate::http::{Answer, Http, host};
use crate::state::library::NewReleaseGroup;

pub const DEFAULT_SERVER: &str = "https://musicbrainz.org";
/// musicbrainz.org allows one request per second.
const OFFICIAL_SPACING: Duration = Duration::from_millis(1100);
const PAGE: usize = 100;

pub struct MusicBrainz<'a> {
    pub http: &'a Http,
    pub server: &'a str,
}

/// An artist found by a search.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ArtistHit {
    pub mbid: String,
    pub name: String,
    pub sort_name: String,
    /// What tells them from others of the same name.
    pub disambiguation: Option<String>,
    pub area: Option<String>,
}

/// A release (an edition of a release group) and its tracklist.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Release {
    pub mbid: String,
    pub title: String,
    pub date: Option<String>,
    pub country: Option<String>,
    /// Official, Promotion, Bootleg, …
    pub status: Option<String>,
    pub disambiguation: Option<String>,
    /// Its media: `CD`, `2×CD`, `Digital Media`, `CD + DVD`, …
    pub formats: String,
    /// Every medium's tracks, in order.
    pub tracks: Vec<ReleaseTrack>,
}

/// A track on a release.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ReleaseTrack {
    /// The track's id (`MUSICBRAINZ_RELEASETRACKID`).
    pub mbid: String,
    pub recording_mbid: String,
    /// The medium (1-based) and the position on it.
    pub disc: u32,
    pub position: u32,
    pub title: String,
    pub length_ms: Option<u64>,
}

/// An artist credit: "A feat. B", with each artist's id and sort name.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Credit {
    /// As credited, join phrases and all.
    pub name: String,
    /// The same, with sort names.
    pub sort: String,
    pub ids: Vec<String>,
    /// Each artist's own name.
    pub names: Vec<String>,
}

/// A release with everything its files' tags say (as Picard writes them).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ReleaseInfo {
    pub mbid: String,
    pub title: String,
    pub artist: Credit,
    pub date: Option<String>,
    pub country: Option<String>,
    pub status: Option<String>,
    pub barcode: Option<String>,
    pub script: Option<String>,
    pub language: Option<String>,
    pub label: Option<String>,
    pub catalog_number: Option<String>,
    pub group_mbid: String,
    /// When the release group first came out.
    pub original_date: Option<String>,
    /// album, single, ep, … then the secondary types (lowercase).
    pub types: Vec<String>,
    /// Each medium's format.
    pub media: Vec<String>,
    /// How many tracks each medium has.
    pub track_counts: Vec<u32>,
    pub tracks: Vec<TrackInfo>,
}

/// A track on a [`ReleaseInfo`].
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct TrackInfo {
    pub mbid: String,
    pub recording_mbid: String,
    pub disc: u32,
    pub position: u32,
    pub title: String,
    pub artist: Credit,
    pub isrcs: Vec<String>,
    pub length_ms: Option<u64>,
}

/// An artist as MusicBrainz knows them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ArtistInfo {
    pub name: String,
    pub sort_name: String,
    /// When it's a joined artist (a group named after its members, like
    /// "K-Salaam & Beatnick"): those members' ids and names.
    pub joined_of: Vec<(String, String)>,
}

impl MusicBrainz<'_> {
    fn spacing(&self) -> Duration {
        if host(self.server).ends_with("musicbrainz.org") {
            OFFICIAL_SPACING
        } else {
            Duration::ZERO
        }
    }

    async fn get(&self, path: &str) -> anyhow::Result<Option<Value>> {
        let url = format!("{}/ws/2/{path}", self.server.trim_end_matches('/'));
        let name = format!("MusicBrainz ({})", host(self.server));
        match self
            .http
            .get_json(&name, host(self.server), self.spacing(), &url)
            .await?
        {
            Answer::Json(json) => Ok(Some(json)),
            Answer::NotFound => Ok(None),
        }
    }

    /// A release's release group (`None`: MusicBrainz doesn't know it).
    ///
    /// # Errors
    ///
    /// When the request fails.
    pub async fn release_group_of(&self, release: &str) -> anyhow::Result<Option<String>> {
        let Some(json) = self
            .get(&format!("release/{release}?inc=release-groups&fmt=json"))
            .await?
        else {
            return Ok(None);
        };
        Ok(json["release-group"]["id"].as_str().map(str::to_string))
    }

    /// Artists named like `query`: from the search server, or, when there's
    /// none (a mirror without Solr), from the database directly.
    ///
    /// # Errors
    ///
    /// When neither answers.
    pub async fn search_artists(
        &self,
        query: &str,
        limit: usize,
    ) -> anyhow::Result<Vec<ArtistHit>> {
        let limit = limit.to_string();
        let solr = self
            .get_once(
                "2/artist",
                &[
                    ("query", &lucene_escape(query)),
                    ("limit", &limit),
                    ("fmt", "json"),
                ],
            )
            .await;
        match solr {
            Ok(Some(json)) if json["artists"].is_array() => {
                return Ok(json["artists"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter_map(parse_search_hit)
                    .collect());
            }
            Ok(json) => tracing::debug!(
                "artist search: no search server ({}); searching the database",
                json.map_or_else(|| "not found".into(), |j| j["error"].to_string())
            ),
            Err(e) => tracing::debug!("artist search: {e:#}; searching the database"),
        }
        let json = self
            .get_once(
                "js/artist",
                &[
                    ("q", query),
                    ("direct", "true"),
                    ("limit", &limit),
                    ("fmt", "json"),
                ],
            )
            .await?
            .ok_or_else(|| anyhow::anyhow!("MusicBrainz can't search artists"))?;
        if let Some(error) = json["error"].as_str() {
            anyhow::bail!("MusicBrainz can't search artists: {}", error.trim());
        }
        Ok(json
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(parse_direct_hit)
            .collect())
    }

    /// GET `path` (under `/ws/`) with `params`, once.
    async fn get_once(&self, path: &str, params: &[(&str, &str)]) -> anyhow::Result<Option<Value>> {
        let base = format!("{}/ws/{path}", self.server.trim_end_matches('/'));
        let url = reqwest::Url::parse_with_params(&base, params)?;
        let name = format!("MusicBrainz ({})", host(self.server));
        match self
            .http
            .get_json_once(&name, host(self.server), self.spacing(), url.as_str())
            .await?
        {
            Answer::Json(json) => Ok(Some(json)),
            Answer::NotFound => Ok(None),
        }
    }

    /// An artist's names and members (`None`: unknown artist).
    ///
    /// # Errors
    ///
    /// When the request fails.
    pub async fn artist(&self, mbid: &str) -> anyhow::Result<Option<ArtistInfo>> {
        let Some(json) = self
            .get(&format!("artist/{mbid}?inc=artist-rels&fmt=json"))
            .await?
        else {
            return Ok(None);
        };
        Ok(Some(parse_artist(&json)))
    }

    /// Every release group of an artist's, page by page.
    ///
    /// # Errors
    ///
    /// When a request fails, or the artist is unknown.
    pub async fn release_groups(&self, artist: &str) -> anyhow::Result<Vec<NewReleaseGroup>> {
        let mut groups = Vec::new();
        loop {
            let path = format!(
                "release-group?artist={artist}&inc=artist-credits&limit={PAGE}&offset={}&fmt=json",
                groups.len()
            );
            let Some(json) = self.get(&path).await? else {
                anyhow::bail!("MusicBrainz doesn't know artist {artist}");
            };
            let page = json["release-groups"]
                .as_array()
                .cloned()
                .unwrap_or_default();
            let total = json["release-group-count"]
                .as_u64()
                .and_then(|n| usize::try_from(n).ok())
                .unwrap_or(0);
            let got = page.len();
            groups.extend(page.iter().filter_map(|g| parse_group(g, artist)));
            if got < PAGE || groups.len() >= total {
                return Ok(groups);
            }
        }
    }

    /// A release with everything its files' tags need (`None`: MusicBrainz
    /// doesn't know it).
    ///
    /// # Errors
    ///
    /// When the request fails.
    pub async fn release_info(&self, release: &str) -> anyhow::Result<Option<ReleaseInfo>> {
        let path = format!(
            "release/{release}?inc=artist-credits+recordings+release-groups+labels+isrcs+media&fmt=json"
        );
        Ok(self.get(&path).await?.as_ref().and_then(parse_release_info))
    }

    /// A release group's releases, with their tracklists.
    ///
    /// # Errors
    ///
    /// When a request fails.
    pub async fn releases_of_group(&self, group: &str) -> anyhow::Result<Vec<Release>> {
        // Tracklists make answers big: MusicBrainz caps these pages lower.
        const RELEASES_PAGE: usize = 25;
        let mut releases = Vec::new();
        let mut seen = 0;
        loop {
            let path = format!(
                "release?release-group={group}&inc=recordings+media&limit={RELEASES_PAGE}&offset={seen}&fmt=json"
            );
            let Some(json) = self.get(&path).await? else {
                return Ok(releases);
            };
            let page = json["releases"].as_array().cloned().unwrap_or_default();
            let total = json["release-count"]
                .as_u64()
                .and_then(|n| usize::try_from(n).ok())
                .unwrap_or(0);
            seen += page.len();
            releases.extend(page.iter().filter_map(parse_release));
            if page.len() < RELEASES_PAGE || seen >= total {
                return Ok(releases);
            }
        }
    }
}

fn parse_credit(json: &Value) -> Credit {
    let mut credit = Credit::default();
    for part in json.as_array().into_iter().flatten() {
        let artist = &part["artist"];
        let name = part["name"]
            .as_str()
            .or_else(|| artist["name"].as_str())
            .unwrap_or_default();
        let join = part["joinphrase"].as_str().unwrap_or_default();
        credit.name.push_str(name);
        credit.name.push_str(join);
        credit
            .sort
            .push_str(artist["sort-name"].as_str().unwrap_or(name));
        credit.sort.push_str(join);
        if let Some(id) = artist["id"].as_str() {
            credit.ids.push(id.into());
        }
        credit
            .names
            .push(artist["name"].as_str().unwrap_or(name).into());
    }
    credit
}

/// A release lookup's answer (`inc=artist-credits+recordings+release-groups
/// +labels+isrcs+media`).
fn parse_release_info(json: &Value) -> Option<ReleaseInfo> {
    let text = |v: &Value| v.as_str().filter(|s| !s.is_empty()).map(str::to_string);
    let group = &json["release-group"];
    let mut types: Vec<String> = text(&group["primary-type"]).into_iter().collect();
    types.extend(
        group["secondary-types"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|t| t.as_str().map(str::to_string)),
    );
    let label = json["label-info"].as_array().and_then(|l| l.first());
    let mut info = ReleaseInfo {
        mbid: text(&json["id"])?,
        title: text(&json["title"]).unwrap_or_default(),
        artist: parse_credit(&json["artist-credit"]),
        date: text(&json["date"]),
        country: text(&json["country"]),
        status: text(&json["status"]),
        barcode: text(&json["barcode"]),
        script: text(&json["text-representation"]["script"]),
        language: text(&json["text-representation"]["language"]),
        label: label.and_then(|l| text(&l["label"]["name"])),
        catalog_number: label.and_then(|l| text(&l["catalog-number"])),
        group_mbid: text(&group["id"]).unwrap_or_default(),
        original_date: text(&group["first-release-date"]),
        types: types.iter().map(|t| t.to_lowercase()).collect(),
        ..ReleaseInfo::default()
    };
    for (i, medium) in json["media"].as_array().into_iter().flatten().enumerate() {
        let disc = medium["position"]
            .as_u64()
            .and_then(|p| u32::try_from(p).ok())
            .unwrap_or_else(|| u32::try_from(i + 1).unwrap_or(u32::MAX));
        info.media
            .push(text(&medium["format"]).unwrap_or_else(|| "Unknown medium".into()));
        let tracks = medium["tracks"].as_array().cloned().unwrap_or_default();
        info.track_counts
            .push(u32::try_from(tracks.len()).unwrap_or(u32::MAX));
        for track in &tracks {
            let recording = &track["recording"];
            info.tracks.push(TrackInfo {
                mbid: text(&track["id"])?,
                recording_mbid: text(&recording["id"]).unwrap_or_default(),
                disc,
                position: track["position"]
                    .as_u64()
                    .and_then(|p| u32::try_from(p).ok())
                    .unwrap_or(0),
                title: text(&track["title"]).unwrap_or_default(),
                artist: if track["artist-credit"].is_array() {
                    parse_credit(&track["artist-credit"])
                } else {
                    parse_credit(&recording["artist-credit"])
                },
                isrcs: recording["isrcs"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter_map(|i| i.as_str().map(str::to_string))
                    .collect(),
                length_ms: track["length"]
                    .as_u64()
                    .or_else(|| recording["length"].as_u64()),
            });
        }
    }
    Some(info)
}

/// A release in a browse answer (with `inc=recordings+media`).
fn parse_release(json: &Value) -> Option<Release> {
    let text = |v: &Value| v.as_str().filter(|s| !s.is_empty()).map(str::to_string);
    let media = json["media"].as_array().cloned().unwrap_or_default();
    let mut formats: Vec<(String, usize)> = Vec::new();
    let mut tracks = Vec::new();
    for (i, medium) in media.iter().enumerate() {
        let format = text(&medium["format"]).unwrap_or_else(|| "Unknown medium".into());
        match formats.last_mut() {
            Some((last, n)) if *last == format => *n += 1,
            _ => formats.push((format, 1)),
        }
        let disc = medium["position"]
            .as_u64()
            .and_then(|p| u32::try_from(p).ok())
            .unwrap_or_else(|| u32::try_from(i + 1).unwrap_or(u32::MAX));
        for track in medium["tracks"].as_array().into_iter().flatten() {
            tracks.push(ReleaseTrack {
                mbid: text(&track["id"])?,
                recording_mbid: text(&track["recording"]["id"]).unwrap_or_default(),
                disc,
                position: track["position"]
                    .as_u64()
                    .and_then(|p| u32::try_from(p).ok())
                    .unwrap_or(0),
                title: text(&track["title"]).unwrap_or_default(),
                length_ms: track["length"]
                    .as_u64()
                    .or_else(|| track["recording"]["length"].as_u64()),
            });
        }
    }
    Some(Release {
        mbid: text(&json["id"])?,
        title: text(&json["title"]).unwrap_or_default(),
        date: text(&json["date"]),
        country: text(&json["country"]),
        status: text(&json["status"]),
        disambiguation: text(&json["disambiguation"]),
        formats: formats
            .iter()
            .map(|(f, n)| {
                if *n > 1 {
                    format!("{n}×{f}")
                } else {
                    f.clone()
                }
            })
            .collect::<Vec<_>>()
            .join(" + "),
        tracks,
    })
}

fn parse_artist(json: &Value) -> ArtistInfo {
    let text = |v: &Value| v.as_str().unwrap_or_default().to_string();
    let name = text(&json["name"]);
    let members: Vec<(String, String)> = json["relations"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|r| r["type"] == "member of band" && r["direction"] == "backward")
        .filter_map(|r| {
            Some((
                r["artist"]["id"].as_str()?.into(),
                text(&r["artist"]["name"]),
            ))
        })
        .collect();
    let joined_of = if json["type"] == "Group" {
        joined_members(&name, &members)
    } else {
        Vec::new()
    };
    ArtistInfo {
        sort_name: text(&json["sort-name"]),
        name,
        joined_of,
    }
}

/// `text` as a search server query: Lucene's special characters escaped.
fn lucene_escape(text: &str) -> String {
    let mut out = String::new();
    for c in text.chars() {
        if "+-&|!(){}[]^\"~*?:\\/".contains(c) {
            out.push('\\');
        }
        out.push(c);
    }
    out
}

/// An artist in the search server's answer.
fn parse_search_hit(json: &Value) -> Option<ArtistHit> {
    let text = |v: &Value| v.as_str().filter(|s| !s.is_empty()).map(str::to_string);
    Some(ArtistHit {
        mbid: text(&json["id"])?,
        name: text(&json["name"])?,
        sort_name: text(&json["sort-name"]).unwrap_or_default(),
        disambiguation: text(&json["disambiguation"]),
        area: text(&json["area"]["name"]),
    })
}

/// An artist in the database search's answer (`/ws/js/artist?direct=true`;
/// its last item is paging, not an artist).
fn parse_direct_hit(json: &Value) -> Option<ArtistHit> {
    let text = |v: &Value| v.as_str().filter(|s| !s.is_empty()).map(str::to_string);
    Some(ArtistHit {
        mbid: text(&json["gid"])?,
        name: text(&json["name"])?,
        sort_name: text(&json["sort_name"]).unwrap_or_default(),
        disambiguation: text(&json["comment"]),
        area: text(&json["area"]["name"]),
    })
}

/// Words that join artists' names in a joined artist's.
const JOINERS: [&str; 13] = [
    "&", "and", "e", "y", "et", "und", "+", "x", "/", "vs", "vs.", "with", "com",
];

/// The members a group's `name` is made of ("A & B", "A, B e C"), when
/// every part of it is one of them; none otherwise (a band).
#[must_use]
pub fn joined_members(name: &str, members: &[(String, String)]) -> Vec<(String, String)> {
    let key = |s: &str| -> String {
        deunicode::deunicode(s)
            .to_lowercase()
            .chars()
            .filter(char::is_ascii_alphanumeric)
            .collect()
    };
    let mut parts: Vec<String> = vec![String::new()];
    for word in name.replace('/', " / ").split_whitespace() {
        let (word, comma) = match word.strip_suffix(',') {
            Some(w) => (w, true),
            None => (word, false),
        };
        if JOINERS.contains(&word.to_lowercase().as_str()) {
            parts.push(String::new());
        } else {
            let last = parts.last_mut().expect("there's always one");
            last.push(' ');
            last.push_str(word);
        }
        if comma {
            parts.push(String::new());
        }
    }
    let parts: Vec<String> = parts
        .iter()
        .map(|p| key(p))
        .filter(|p| !p.is_empty())
        .collect();
    if parts.len() < 2 {
        return Vec::new();
    }
    let mut out = Vec::new();
    for part in &parts {
        match members.iter().find(|(_, n)| key(n) == *part) {
            Some(member) if !out.contains(member) => out.push(member.clone()),
            _ => return Vec::new(),
        }
    }
    out
}

/// A release group from a browse answer; `artist` is always one of its
/// artists.
fn parse_group(json: &Value, artist: &str) -> Option<NewReleaseGroup> {
    let mbid = json["id"].as_str()?.to_string();
    let credits = json["artist-credit"].as_array();
    let artist_credit = credits.map(|credits| {
        credits.iter().fold(String::new(), |mut text, c| {
            text.push_str(c["name"].as_str().unwrap_or_default());
            text.push_str(c["joinphrase"].as_str().unwrap_or_default());
            text
        })
    });
    let mut artist_mbids: Vec<String> = credits
        .into_iter()
        .flatten()
        .filter_map(|c| c["artist"]["id"].as_str().map(str::to_string))
        .collect();
    if !artist_mbids.iter().any(|a| a == artist) {
        artist_mbids.push(artist.to_string());
    }
    Some(NewReleaseGroup {
        mbid,
        title: json["title"].as_str().unwrap_or_default().to_string(),
        artist_credit: artist_credit.filter(|c| !c.is_empty()),
        primary_type: json["primary-type"].as_str().map(str::to_string),
        secondary_types: json["secondary-types"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|s| s.as_str().map(str::to_string))
            .collect(),
        first_release_date: json["first-release-date"]
            .as_str()
            .filter(|d| !d.is_empty())
            .map(str::to_string),
        artist_mbids,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use wiremock::matchers::{method, path, query_param};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    #[tokio::test]
    async fn artist_search_falls_back_to_the_database_without_a_search_server() {
        let server = MockServer::start().await;
        // A mirror without Solr: asked once, not retried.
        Mock::given(method("GET"))
            .and(path("/ws/2/artist"))
            .and(query_param("query", "K\\-Salaam"))
            .respond_with(ResponseTemplate::new(503).set_body_json(serde_json::json!({
                "error": "Can't connect to search:8983"
            })))
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/ws/js/artist"))
            .and(query_param("q", "K-Salaam"))
            .and(query_param("direct", "true"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!([
                {"gid": "c61b606d-5eb0-45ed-81af-37b970305e47", "name": "K-Salaam",
                 "sort_name": "K-Salaam", "comment": "", "typeName": "Person", "area": null},
                {"gid": "71658785-384d-4b86-9922-01dbce8aefab", "name": "K-Salaam & Beatnick",
                 "comment": "duo", "typeName": "Group", "area": {"name": "United States"}},
                {"current": 1, "pages": 1}
            ])))
            .mount(&server)
            .await;
        let http = Http::new(None);
        let uri = server.uri();
        let mb = MusicBrainz {
            http: &http,
            server: &uri,
        };
        let started = std::time::Instant::now();
        let hits = mb.search_artists("K-Salaam", 10).await.unwrap();
        assert!(
            started.elapsed() < std::time::Duration::from_secs(1),
            "no backing off"
        );
        assert_eq!(hits.len(), 2, "{hits:?}");
        assert_eq!(hits[0].name, "K-Salaam");
        assert_eq!(hits[0].disambiguation, None);
        assert_eq!(hits[1].disambiguation.as_deref(), Some("duo"));
        assert_eq!(hits[1].area.as_deref(), Some("United States"));
    }

    #[tokio::test]
    async fn artist_search_uses_the_search_server_when_there_is_one() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/ws/2/artist"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "artists": [{"id": "8a338e06-d182-46f2-bd16-30a09bc840ba", "name": "Elvis Costello",
                  "sort-name": "Costello, Elvis", "type": "Person", "area": {"name": "United Kingdom"}}]
            })))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/ws/js/artist"))
            .respond_with(ResponseTemplate::new(500))
            .expect(0)
            .mount(&server)
            .await;
        let http = Http::new(None);
        let uri = server.uri();
        let mb = MusicBrainz {
            http: &http,
            server: &uri,
        };
        let hits = mb.search_artists("Elvis Costello", 10).await.unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].sort_name, "Costello, Elvis");
        assert_eq!(hits[0].area.as_deref(), Some("United Kingdom"));
    }

    #[test]
    fn joined_artists_are_groups_named_after_their_members() {
        let m = |names: &[&str]| -> Vec<(String, String)> {
            names
                .iter()
                .enumerate()
                .map(|(i, n)| (format!("id{i}"), (*n).to_string()))
                .collect()
        };
        let duo = m(&["Beatnick", "K-Salaam"]);
        let names = |found: Vec<(String, String)>| -> Vec<String> {
            found.into_iter().map(|(_, n)| n).collect()
        };
        assert_eq!(
            names(joined_members("K-Salaam & Beatnick", &duo)),
            ["K-Salaam", "Beatnick"]
        );
        assert_eq!(names(joined_members("Beatnick/K-Salaam", &duo)).len(), 2);
        let trio = m(&["Caetano Veloso", "Gilberto Gil", "Gal Costa"]);
        assert_eq!(
            names(joined_members(
                "Caetano Veloso, Gilberto Gil e Gal Costa",
                &trio
            ))
            .len(),
            3
        );
        // Bands.
        let attractions = m(&["Elvis Costello", "Steve Nieve", "Bruce Thomas"]);
        assert!(joined_members("Elvis Costello & The Attractions", &attractions).is_empty());
        assert!(joined_members("Coheed and Cambria", &m(&["Claudio Sanchez"])).is_empty());
        assert!(joined_members("Beatnick", &duo).is_empty());
        assert!(joined_members("K-Salaam & Beatnick", &[]).is_empty());
    }

    fn group(id: &str, title: &str) -> Value {
        serde_json::json!({
            "id": id, "title": title, "primary-type": "Album", "secondary-types": [],
            "first-release-date": "2020-01-01",
            "artist-credit": [
                {"name": "A", "joinphrase": " & ", "artist": {"id": "a"}},
                {"name": "B", "joinphrase": "", "artist": {"id": "b"}}
            ]
        })
    }

    #[tokio::test]
    async fn browses_every_page() {
        let server = MockServer::start().await;
        let first: Vec<Value> = (0..100).map(|i| group(&format!("g{i}"), "T")).collect();
        Mock::given(method("GET"))
            .and(path("/ws/2/release-group"))
            .and(query_param("offset", "0"))
            .respond_with(ResponseTemplate::new(200).set_body_json(
                serde_json::json!({"release-groups": first, "release-group-count": 101}),
            ))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/ws/2/release-group"))
            .and(query_param("offset", "100"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "release-groups": [group("last", "Last")], "release-group-count": 101
            })))
            .mount(&server)
            .await;
        let http = Http::new(None);
        let uri = server.uri();
        let mb = MusicBrainz {
            http: &http,
            server: &uri,
        };
        let groups = mb.release_groups("a").await.unwrap();
        assert_eq!(groups.len(), 101);
        let last = &groups[100];
        assert_eq!(last.title, "Last");
        assert_eq!(last.artist_credit.as_deref(), Some("A & B"));
        assert_eq!(last.artist_mbids, ["a", "b"]);
    }

    #[tokio::test]
    async fn artists_and_releases() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/ws/2/artist/a"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "name": "The A", "sort-name": "A, The", "relations": []
            })))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/ws/2/release/r1"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(serde_json::json!({"release-group": {"id": "g1"}})),
            )
            .mount(&server)
            .await;
        let http = Http::new(None);
        let uri = server.uri();
        let mb = MusicBrainz {
            http: &http,
            server: &uri,
        };
        let artist = mb.artist("a").await.unwrap().unwrap();
        assert_eq!(
            artist,
            ArtistInfo {
                name: "The A".into(),
                sort_name: "A, The".into(),
                joined_of: Vec::new(),
            }
        );
        assert_eq!(mb.artist("nobody").await.unwrap(), None);
        assert_eq!(
            mb.release_group_of("r1").await.unwrap().as_deref(),
            Some("g1")
        );
        assert_eq!(mb.release_group_of("r2").await.unwrap(), None);
    }
}
