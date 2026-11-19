//! Folders found and queued without you. When a download fails, another
//! source for the album: a new search is made, and the best folder it finds
//! that's lossless, matches one of the album's releases exactly, and wasn't
//! tried for it before is queued. For an upgrade, the untried exact match
//! that's better than the album you have, closest to the target quality.

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
    search_and_queue(search, stopping, |folders, releases| {
        pick(folders, releases, search.tried)
    })
    .await
}

/// Search for a better copy of the album (now at `current`), and queue
/// the untried exact match closest to `target` that meets it, else the best
/// one better than `current`. Folders that match one of `prefer` (the
/// releases you have) come first. `None`: nothing better.
///
/// # Errors
///
/// As [`queue_another`].
pub async fn queue_upgrade(
    search: &Search<'_>,
    current: &crate::quality::Quality,
    target: crate::quality::Tier,
    prefer: &[String],
    stopping: &dyn Fn() -> bool,
) -> anyhow::Result<Option<Queued>> {
    search_and_queue(search, stopping, |folders, releases| {
        pick_upgrade(folders, releases, search.tried, current, target, prefer)
    })
    .await
}

/// Search for the album, and queue the folder `pick` picks.
async fn search_and_queue(
    search: &Search<'_>,
    stopping: &dyn Fn() -> bool,
    pick: impl FnOnce(Vec<Folder>, &[Release]) -> Option<(Folder, Match)>,
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
    let Some((folder, m)) = pick(folders, &releases) else {
        tracing::info!("slskd: nothing that'll do for {text:?}");
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
        "slskd: queued {} files from {} ({}) for {}",
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

/// The untried exact matches among `folders`, those of `prefer`'s releases
/// first; of those, the one [`crate::quality::pick`] picks.
fn pick_upgrade(
    folders: Vec<Folder>,
    releases: &[Release],
    tried: &[(String, String)],
    current: &crate::quality::Quality,
    target: crate::quality::Tier,
    prefer: &[String],
) -> Option<(Folder, Match)> {
    let mut exact: Vec<((Folder, Match), crate::quality::Quality)> = folders
        .into_iter()
        .filter(|f| !tried.iter().any(|(u, d)| *u == f.username && *d == f.dir))
        .filter_map(|f| {
            let m = matching::match_folder(&f.candidates(), releases);
            (m.kind == Kind::Exact).then(|| {
                let audio = f.audio.clone();
                ((f, m), audio)
            })
        })
        .collect();
    let preferred = |m: &Match| {
        m.release
            .and_then(|i| releases.get(i))
            .is_some_and(|r| prefer.contains(&r.mbid))
    };
    // Stable: slskd's order stays within each.
    exact.sort_by_key(|((_, m), _)| !preferred(m));
    crate::quality::pick(exact, current, target)
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

    #[test]
    fn an_upgrade_is_the_untried_exact_match_closest_to_the_target() {
        use crate::quality::{Quality, Tier};
        let with = |mut f: Folder, depth: Option<u32>, kbps: Option<u32>| {
            f.audio = Quality {
                format: if f.lossless { "FLAC" } else { "MP3" }.into(),
                lossless: f.lossless,
                bit_depth: depth,
                sample_rate: Some(44_100),
                bitrate: kbps,
            };
            f
        };
        let current = Quality {
            format: "MP3".into(),
            bitrate: Some(192),
            ..Quality::default()
        };
        let folders = vec![
            with(folder("ann", "A", true, [200, 180]), Some(24), None),
            with(folder("bob", "B", false, [200, 180]), None, Some(320)),
            with(folder("cid", "C", true, [260, 180]), Some(16), None),
            with(folder("dee", "D", true, [200, 180]), Some(16), None),
            with(folder("eve", "E", true, [200, 180]), Some(16), None),
        ];
        let tried = vec![("dee".to_string(), "D".to_string())];
        let pick = |target, prefer: &[String]| {
            pick_upgrade(
                folders.clone(),
                &[release()],
                &tried,
                &current,
                target,
                prefer,
            )
            .map(|(f, _)| f.username)
        };
        assert_eq!(pick(Tier::Lossless, &[]).as_deref(), Some("eve"));
        assert_eq!(pick(Tier::HiRes, &[]).as_deref(), Some("ann"));
        assert_eq!(pick(Tier::R320, &[]).as_deref(), Some("bob"));
        let better = Quality {
            lossless: true,
            bit_depth: Some(24),
            ..Quality::default()
        };
        assert!(
            pick_upgrade(folders, &[release()], &tried, &better, Tier::HiRes, &[]).is_none(),
            "nothing beats what you have"
        );
    }
}
