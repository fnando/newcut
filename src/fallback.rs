//! When a download fails: another source for the album, found and queued
//! without you. A new search is made, and the best folder it finds that's
//! lossless, matches one of the album's releases exactly, and wasn't tried
//! for it before is queued.

use std::time::Duration;

use crate::matching::{self, Kind, Match};
use crate::musicbrainz::Release;
use crate::slskd::{Folder, Slskd};
use crate::state::downloads::Matched;

/// After this many failed downloads of an album, it's yours to sort out.
pub const MAX_FAILURES: usize = 3;

/// How long a search gets to finish.
const SEARCH_FOR: Duration = Duration::from_secs(180);
const POLL_EVERY: Duration = Duration::from_secs(3);

/// What was queued instead.
#[derive(Debug, Clone)]
pub struct Queued {
    pub folder: Folder,
    pub matched: Matched,
    /// Where slskd puts it (under its downloads folder), when it's filed.
    pub destination: Option<String>,
}

/// What a search for the album needs.
pub struct Search<'a> {
    pub client: &'a Slskd,
    pub http: &'a crate::http::Http,
    pub musicbrainz_server: &'a str,
    /// The release group's MBID.
    pub key: &'a str,
    pub artist: &'a str,
    pub album: &'a str,
    /// The folders tried for it before: (user, folder).
    pub tried: &'a [(String, String)],
    /// Finished downloads get filed: each gets a folder of its own.
    pub filing: bool,
}

/// Search for the album again and queue the best folder that'll do.
/// `None`: nothing will.
///
/// # Errors
///
/// When slskd or MusicBrainz can't be reached, or the search doesn't
/// finish in time.
pub async fn queue_another(
    search: &Search<'_>,
    stopping: &dyn Fn() -> bool,
) -> anyhow::Result<Option<Queued>> {
    let releases = crate::musicbrainz::MusicBrainz {
        http: search.http,
        server: search.musicbrainz_server,
    }
    .releases_of_group(search.key)
    .await?;
    if releases.is_empty() {
        return Ok(None);
    }
    let text = crate::slskd::search_text(search.artist, search.album);
    let client = search.client;
    let id = client.start_search(&text).await?;
    let started = std::time::Instant::now();
    loop {
        if stopping() {
            anyhow::bail!("stopped while searching slskd");
        }
        match client.search(&id).await? {
            Some(state) if state.complete => break,
            Some(_) => {}
            None => anyhow::bail!("slskd forgot the search for {text:?}"),
        }
        if started.elapsed() > SEARCH_FOR {
            anyhow::bail!("slskd's search for {text:?} didn't finish in time");
        }
        tokio::time::sleep(POLL_EVERY).await;
    }
    let folders = crate::slskd::folders(&client.responses(&id).await?);
    let Some((folder, m)) = pick(folders, &releases, search.tried) else {
        tracing::info!("slskd: no other source for {text:?}");
        return Ok(None);
    };
    let destination = search
        .filing
        .then(|| format!("newcut/{}", crate::web::random_hex(8)));
    match &destination {
        Some(destination) => {
            client
                .enqueue_into(&folder.username, &folder.files, destination)
                .await?;
        }
        None => client.enqueue(&folder.username, &folder.files).await?,
    }
    tracing::info!(
        "slskd: queued {} files from {} ({}) for {}, instead",
        folder.files.len(),
        folder.username,
        folder.dir,
        search.key
    );
    Ok(Some(Queued {
        matched: matching::matched(&m, &releases),
        folder,
        destination,
    }))
}

/// The first of `folders` (best first) that's lossless, matches one of
/// `releases` exactly and isn't one of `tried`.
fn pick(
    folders: Vec<Folder>,
    releases: &[Release],
    tried: &[(String, String)],
) -> Option<(Folder, Match)> {
    folders
        .into_iter()
        .filter(|f| f.lossless)
        .filter(|f| !tried.iter().any(|(u, d)| *u == f.username && *d == f.dir))
        .find_map(|f| {
            let m = matching::match_folder(&f.candidates(), releases);
            (m.kind == Kind::Exact).then_some((f, m))
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::musicbrainz::ReleaseTrack;

    fn release() -> Release {
        Release {
            mbid: "r1".into(),
            tracks: [200, 180]
                .iter()
                .enumerate()
                .map(|(i, s)| ReleaseTrack {
                    mbid: format!("t{i}"),
                    disc: 1,
                    position: u32::try_from(i + 1).unwrap(),
                    title: format!("Song {i}"),
                    length_ms: Some(s * 1000),
                    ..ReleaseTrack::default()
                })
                .collect(),
            ..Release::default()
        }
    }

    fn folder(user: &str, dir: &str, lossless: bool, lengths: [u32; 2]) -> Folder {
        let ext = if lossless { "flac" } else { "mp3" };
        Folder {
            username: user.into(),
            dir: dir.into(),
            files: vec![
                (format!("{dir}\\01. Song 0.{ext}"), 10),
                (format!("{dir}\\02. Song 1.{ext}"), 10),
            ],
            lengths: lengths.iter().map(|l| Some(*l)).collect(),
            lossless,
            ..Folder::default()
        }
    }

    #[test]
    fn picks_the_best_untried_lossless_exact_match() {
        let tried = vec![("ann".to_string(), "A\\Album".to_string())];
        let folders = vec![
            folder("ann", "A\\Album", true, [200, 180]),
            folder("bob", "B\\Album", false, [200, 180]),
            folder("cid", "C\\Album", true, [260, 180]),
            folder("dee", "D\\Album", true, [201, 179]),
            folder("eve", "E\\Album", true, [200, 180]),
        ];
        let (picked, m) = pick(folders, &[release()], &tried).unwrap();
        assert_eq!(
            picked.username, "dee",
            "tried, lossy and off by a minute are skipped"
        );
        assert_eq!(m.kind, Kind::Exact);
        assert!(
            pick(
                vec![folder("bob", "B", false, [200, 180])],
                &[release()],
                &[]
            )
            .is_none()
        );
    }
}
