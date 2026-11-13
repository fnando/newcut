//! ListenBrainz's fresh releases: what came out recently (or is coming),
//! from MusicBrainz's data, in one request for every artist.

use std::collections::HashSet;
use std::time::Duration;

use serde_json::Value;

use crate::http::{Answer, Http, host};
use crate::state::library::NewReleaseGroup;

pub const DEFAULT_SERVER: &str = "https://api.listenbrainz.org";
/// How far back (and ahead) to ask for; ListenBrainz allows up to 90 days.
pub const DAYS: u64 = 30;

/// Fresh releases by any of `artists`, as release groups.
///
/// # Errors
///
/// When the request fails or the answer isn't what ListenBrainz sends.
pub async fn fresh_releases<S: std::hash::BuildHasher>(
    http: &Http,
    server: &str,
    artists: &HashSet<String, S>,
) -> anyhow::Result<Vec<NewReleaseGroup>> {
    let url = format!(
        "{}/1/explore/fresh-releases/?days={DAYS}&past=true&future=true&sort=release_date",
        server.trim_end_matches('/')
    );
    let name = format!("ListenBrainz ({})", host(server));
    let Answer::Json(json) = http
        .get_json(&name, host(server), Duration::from_millis(500), &url)
        .await?
    else {
        anyhow::bail!("{name} has no fresh releases at {url}");
    };
    let Some(releases) = json["payload"]["releases"].as_array() else {
        anyhow::bail!("{name}: unexpected answer (no payload.releases)");
    };
    Ok(pick(releases, artists))
}

/// The releases by `artists`, one per release group.
fn pick<S: std::hash::BuildHasher>(
    releases: &[Value],
    artists: &HashSet<String, S>,
) -> Vec<NewReleaseGroup> {
    let mut seen = HashSet::new();
    releases
        .iter()
        .filter_map(|r| {
            let mbid = r["release_group_mbid"].as_str()?.to_string();
            let artist_mbids: Vec<String> = r["artist_mbids"]
                .as_array()?
                .iter()
                .filter_map(|a| a.as_str().map(str::to_string))
                .collect();
            if !artist_mbids.iter().any(|a| artists.contains(a)) || !seen.insert(mbid.clone()) {
                return None;
            }
            let secondary = match &r["release_group_secondary_type"] {
                Value::String(s) if !s.is_empty() => vec![s.clone()],
                Value::Array(list) => list
                    .iter()
                    .filter_map(|s| s.as_str().map(str::to_string))
                    .collect(),
                _ => Vec::new(),
            };
            Some(NewReleaseGroup {
                mbid,
                title: r["release_name"].as_str().unwrap_or_default().to_string(),
                artist_credit: r["artist_credit_name"].as_str().map(str::to_string),
                primary_type: r["release_group_primary_type"].as_str().map(str::to_string),
                secondary_types: secondary,
                first_release_date: r["release_date"].as_str().map(str::to_string),
                artist_mbids,
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keeps_followed_artists_releases_once() {
        let releases = serde_json::json!([
            {"release_group_mbid": "g1", "release_name": "One", "artist_mbids": ["a", "z"],
             "artist_credit_name": "A & Z", "release_date": "2026-11-07",
             "release_group_primary_type": "Album", "release_group_secondary_type": null},
            {"release_group_mbid": "g1", "release_name": "One (vinyl)", "artist_mbids": ["a"]},
            {"release_group_mbid": "g2", "release_name": "Other", "artist_mbids": ["z"]},
            {"release_group_mbid": "g3", "release_name": "Live", "artist_mbids": ["a"],
             "release_group_primary_type": "Album", "release_group_secondary_type": "Live"}
        ]);
        let artists = HashSet::from(["a".to_string()]);
        let groups = pick(releases.as_array().unwrap(), &artists);
        let got: Vec<_> = groups
            .iter()
            .map(|g| (g.mbid.as_str(), g.title.as_str(), g.secondary_types.clone()))
            .collect();
        assert_eq!(
            got,
            [
                ("g1", "One", vec![]),
                ("g3", "Live", vec!["Live".to_string()])
            ]
        );
        assert_eq!(groups[0].artist_credit.as_deref(), Some("A & Z"));
    }
}
