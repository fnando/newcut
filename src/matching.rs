//! Matching a folder of audio files (a Soulseek search result) to one of a
//! release group's releases: the same number of tracks, each as long as
//! the release's (within a few seconds), in track-number order.

use crate::musicbrainz::Release;

/// How far a file's length may be from the track's.
const TOLERANCE_MS: u64 = 3_000;

/// A file to match: its full name, and its length when the sharer says.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Candidate {
    pub filename: String,
    pub length: Option<u32>,
}

/// How well a folder matches.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Kind {
    /// Every track's there, as long as the release's.
    Exact,
    /// As many tracks, and the titles match, but some lengths aren't known.
    Likely,
    /// It doesn't match any release.
    None,
}

impl Kind {
    #[must_use]
    pub fn key(self) -> &'static str {
        match self {
            Self::Exact => "exact",
            Self::Likely => "likely",
            Self::None => "none",
        }
    }

    #[must_use]
    pub fn from_key(key: &str) -> Self {
        match key {
            "exact" => Self::Exact,
            "likely" => Self::Likely,
            _ => Self::None,
        }
    }
}

/// A folder's best match.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Match {
    pub kind: Kind,
    /// The release (an index into the releases matched against), unless
    /// it's [`Kind::None`].
    pub release: Option<usize>,
    /// How many other releases match as well (alike tracklists).
    pub alike: usize,
    /// For each file (in the order given), its track's id on the release.
    pub tracks: Vec<Option<String>>,
    /// Why it isn't exact (for [`Kind::Likely`] and [`Kind::None`]).
    pub reason: Option<String>,
}

impl Match {
    fn none(files: usize, reason: String) -> Self {
        Self {
            kind: Kind::None,
            release: None,
            alike: 0,
            tracks: vec![None; files],
            reason: Some(reason),
        }
    }
}

/// How one release fits.
struct Fit {
    kind: Kind,
    /// The total length difference (to rank alike fits).
    off_ms: u64,
    tracks: Vec<Option<String>>,
    reason: Option<String>,
}

/// The release in `releases` that `files` (one folder's) match best.
#[must_use]
pub fn match_folder(files: &[Candidate], releases: &[Release]) -> Match {
    if releases.is_empty() {
        return Match::none(files.len(), "MusicBrainz has no releases for it".into());
    }
    let order = track_order(files);
    let fits: Vec<(usize, Fit)> = releases
        .iter()
        .enumerate()
        .filter(|(_, r)| r.tracks.len() == files.len())
        .map(|(i, r)| (i, fit(files, &order, r)))
        .collect();
    if fits.is_empty() {
        let mut counts: Vec<usize> = releases.iter().map(|r| r.tracks.len()).collect();
        counts.sort_unstable();
        counts.dedup();
        let counts = counts
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>()
            .join(", ");
        let s = if files.len() == 1 { "" } else { "s" };
        return Match::none(
            files.len(),
            format!("{} file{s}; its releases have {counts} tracks", files.len()),
        );
    }
    let best = fits
        .iter()
        .min_by(|(a, fa), (b, fb)| {
            fa.kind
                .cmp(&fb.kind)
                .then(fa.off_ms.cmp(&fb.off_ms))
                .then(preference(&releases[*a]).cmp(&preference(&releases[*b])))
        })
        .expect("there's one");
    let (index, fit) = best;
    if fit.kind == Kind::None {
        return Match::none(files.len(), fit.reason.clone().unwrap_or_default());
    }
    let alike = fits
        .iter()
        .filter(|(i, f)| i != index && f.kind == fit.kind)
        .count();
    Match {
        kind: fit.kind,
        release: Some(*index),
        alike,
        tracks: fit.tracks.clone(),
        reason: fit.reason.clone(),
    }
}

/// `files` as `release`'s tracks, whatever their lengths (you said it's
/// this release): in track order when there are as many, or by their
/// titles or numbers when there are fewer (part of it).
///
/// # Errors
///
/// When there are more files than tracks, or a file's track can't be told.
pub fn assign(files: &[Candidate], release: &Release) -> Result<Vec<Option<String>>, String> {
    if files.len() > release.tracks.len() {
        return Err(format!(
            "{} files, but that release has {} tracks",
            files.len(),
            release.tracks.len()
        ));
    }
    if files.len() < release.tracks.len() {
        return place(files, release);
    }
    let mut tracks = vec![None; files.len()];
    for (&file, track) in track_order(files).iter().zip(&release.tracks) {
        tracks[file] = Some(track.mbid.clone());
    }
    Ok(tracks)
}

/// Some of `release`'s tracks: each file is the track whose title its name
/// has (the longest, when it has more than one), or else the one its
/// number says. Every file has to land on a track of its own.
fn place(files: &[Candidate], release: &Release) -> Result<Vec<Option<String>>, String> {
    let mut tracks: Vec<Option<String>> = vec![None; files.len()];
    for (file, candidate) in files.iter().enumerate() {
        let by_title = release
            .tracks
            .iter()
            .filter(|t| title_matches(&candidate.filename, &t.title))
            .max_by_key(|t| plain(&t.title).len());
        let by_number = track_number(&candidate.filename).and_then(|(disc, n)| {
            release
                .tracks
                .iter()
                .find(|t| t.disc == disc && t.position == n)
        });
        let track = by_title.or(by_number).ok_or_else(|| {
            format!(
                "can't tell which track {} is: neither its name nor its number says",
                basename(&candidate.filename)
            )
        })?;
        if let Some(other) = tracks
            .iter()
            .position(|t| t.as_deref() == Some(&track.mbid))
        {
            return Err(format!(
                "{} and {} both look like {:?}",
                basename(&files[other].filename),
                basename(&candidate.filename),
                track.title
            ));
        }
        tracks[file] = Some(track.mbid.clone());
    }
    Ok(tracks)
}

/// Which release to pick among alike ones: official ones, then the
/// earliest.
fn preference(release: &Release) -> (bool, String, String) {
    (
        release.status.as_deref() != Some("Official"),
        release.date.clone().unwrap_or_else(|| "9999".into()),
        release.mbid.clone(),
    )
}

/// How `files` fit `release` (as many tracks), taking them in `order`.
fn fit(files: &[Candidate], order: &[usize], release: &Release) -> Fit {
    let mut tracks = vec![None; files.len()];
    let mut off_ms = 0;
    let mut unknown = 0;
    let mut titles = 0;
    let mut wrong = None;
    for (n, (&file, track)) in order.iter().zip(&release.tracks).enumerate() {
        let candidate = &files[file];
        tracks[file] = Some(track.mbid.clone());
        if title_matches(&candidate.filename, &track.title) {
            titles += 1;
        }
        match (candidate.length, track.length_ms) {
            (Some(seconds), Some(ms)) => {
                let diff = (u64::from(seconds) * 1000).abs_diff(ms);
                off_ms += diff;
                if diff > TOLERANCE_MS && wrong.is_none() {
                    wrong = Some(format!(
                        "track {} is {} here, {} on MusicBrainz",
                        n + 1,
                        clock(u64::from(seconds)),
                        clock(ms.div_ceil(1000)),
                    ));
                }
            }
            _ => unknown += 1,
        }
    }
    let count = files.len();
    let (kind, reason) = if let Some(wrong) = wrong {
        (Kind::None, Some(wrong))
    } else if unknown == 0 {
        (Kind::Exact, None)
    } else if titles * 10 >= count * 8 {
        let s = if unknown == 1 { "" } else { "s" };
        (
            Kind::Likely,
            Some(format!("{unknown} length{s} unknown; the titles match")),
        )
    } else {
        (
            Kind::None,
            Some("lengths unknown, and the titles don't match".into()),
        )
    };
    Fit {
        kind,
        off_ms,
        tracks: if kind == Kind::None {
            vec![None; count]
        } else {
            tracks
        },
        reason,
    }
}

/// The order of `files` as tracks: by the disc and track numbers their
/// names start with, when every name has distinct ones; else by name.
fn track_order(files: &[Candidate]) -> Vec<usize> {
    let numbers: Option<Vec<(u32, u32)>> =
        files.iter().map(|f| track_number(&f.filename)).collect();
    let mut order: Vec<usize> = (0..files.len()).collect();
    match numbers {
        Some(numbers)
            if {
                let mut sorted = numbers.clone();
                sorted.sort_unstable();
                sorted.dedup();
                sorted.len() == numbers.len()
            } =>
        {
            order.sort_by_key(|&i| numbers[i]);
        }
        _ => order.sort_by(|&a, &b| files[a].filename.cmp(&files[b].filename)),
    }
    order
}

/// The disc and track number a file name starts with: `01 Title`,
/// `1-01 Title`, `101 Title` (disc 1, when there are three digits), `A1`
/// isn't one.
fn track_number(filename: &str) -> Option<(u32, u32)> {
    let name = basename(filename);
    let digits: String = name.chars().take_while(char::is_ascii_digit).collect();
    if digits.is_empty() {
        return None;
    }
    let rest = &name[digits.len()..];
    if let Some(rest) = rest.strip_prefix(['-', '.']) {
        let track: String = rest.chars().take_while(char::is_ascii_digit).collect();
        if !track.is_empty() {
            return Some((digits.parse().ok()?, track.parse().ok()?));
        }
    }
    let n: u32 = digits.parse().ok()?;
    Some(if digits.len() == 3 {
        (n / 100, n % 100)
    } else {
        (1, n)
    })
}

fn basename(filename: &str) -> &str {
    filename.rsplit(['\\', '/']).next().unwrap_or(filename)
}

/// Letters and digits only, lowercase, without accents.
fn plain(text: &str) -> String {
    deunicode::deunicode(text)
        .to_lowercase()
        .chars()
        .filter(char::is_ascii_alphanumeric)
        .collect()
}

fn title_matches(filename: &str, title: &str) -> bool {
    let title = plain(title);
    !title.is_empty() && plain(basename(filename)).contains(&title)
}

fn clock(seconds: u64) -> String {
    format!("{}:{:02}", seconds / 60, seconds % 60)
}

/// What a release is, in a few words: "CD · US · 2021-03-05 · deluxe".
#[must_use]
pub fn release_label(release: &Release) -> String {
    [
        Some(release.formats.clone()).filter(|f| !f.is_empty()),
        release.country.clone(),
        release.date.clone(),
        release.disambiguation.clone(),
    ]
    .into_iter()
    .flatten()
    .collect::<Vec<_>>()
    .join(" · ")
}

/// A folder's match (against `releases`), to store with its download.
#[must_use]
pub fn matched(m: &Match, releases: &[Release]) -> crate::state::downloads::Matched {
    let release = m.release.map(|i| &releases[i]);
    crate::state::downloads::Matched {
        kind: m.kind.key().into(),
        release_mbid: release.map(|r| r.mbid.clone()),
        release_label: release.map(release_label),
        reason: m.reason.clone(),
        tracks: m.tracks.clone(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::musicbrainz::ReleaseTrack;

    fn release(mbid: &str, date: &str, tracks: &[(&str, Option<u64>)]) -> Release {
        Release {
            mbid: mbid.into(),
            title: "Album".into(),
            date: Some(date.into()),
            country: None,
            status: Some("Official".into()),
            disambiguation: None,
            formats: "CD".into(),
            tracks: tracks
                .iter()
                .enumerate()
                .map(|(i, (title, ms))| ReleaseTrack {
                    mbid: format!("{mbid}-t{}", i + 1),
                    recording_mbid: String::new(),
                    disc: 1,
                    position: u32::try_from(i + 1).unwrap(),
                    title: (*title).into(),
                    length_ms: *ms,
                })
                .collect(),
        }
    }

    fn files(names: &[(&str, Option<u32>)]) -> Vec<Candidate> {
        names
            .iter()
            .map(|(n, l)| Candidate {
                filename: format!("@@x\\Album\\{n}"),
                length: *l,
            })
            .collect()
    }

    const TRACKS: [(&str, Option<u64>); 3] = [
        ("Intro", Some(61_400)),
        ("Café Society", Some(200_000)),
        ("Outro", Some(180_900)),
    ];

    #[test]
    fn matches_by_count_and_lengths_in_track_order() {
        let releases = vec![
            release("deluxe", "2020", &[("Intro", Some(61_000)); 5]),
            release("cd", "2021", &TRACKS),
            release("digital", "2020", &TRACKS),
        ];
        // Out of order by name, but numbered.
        let folder = files(&[
            ("03 Outro.flac", Some(181)),
            ("01 Intro.flac", Some(61)),
            ("02 Cafe Society.flac", Some(199)),
        ]);
        let m = match_folder(&folder, &releases);
        assert_eq!(m.kind, Kind::Exact);
        assert_eq!(m.release, Some(2), "the earliest of the alike ones");
        assert_eq!(m.alike, 1);
        assert_eq!(
            m.tracks,
            [
                Some("digital-t3".to_string()),
                Some("digital-t1".to_string()),
                Some("digital-t2".to_string())
            ]
        );
    }

    #[test]
    fn says_why_it_does_not_match() {
        let releases = vec![release("cd", "2021", &TRACKS)];
        let m = match_folder(&files(&[("01 Intro.flac", Some(61))]), &releases);
        assert_eq!(m.kind, Kind::None);
        assert_eq!(
            m.reason.as_deref(),
            Some("1 file; its releases have 3 tracks")
        );

        let m = match_folder(
            &files(&[
                ("01 Intro.flac", Some(61)),
                ("02 Cafe Society (Live).flac", Some(240)),
                ("03 Outro.flac", Some(181)),
            ]),
            &releases,
        );
        assert_eq!(m.kind, Kind::None);
        assert_eq!(
            m.reason.as_deref(),
            Some("track 2 is 4:00 here, 3:20 on MusicBrainz")
        );
        assert_eq!(m.tracks, [None, None, None]);
    }

    #[test]
    fn titles_stand_in_for_unknown_lengths() {
        let releases = vec![release("cd", "2021", &TRACKS)];
        let m = match_folder(
            &files(&[
                ("01 Intro.mp3", None),
                ("02 Café Society.mp3", None),
                ("03 Outro.mp3", Some(181)),
            ]),
            &releases,
        );
        assert_eq!(m.kind, Kind::Likely);
        assert_eq!(
            m.reason.as_deref(),
            Some("2 lengths unknown; the titles match")
        );
        let m = match_folder(
            &files(&[("01 a.mp3", None), ("02 b.mp3", None), ("03 c.mp3", None)]),
            &releases,
        );
        assert_eq!(m.kind, Kind::None);
    }

    #[test]
    fn assigns_tracks_when_told_which_release() {
        let release = release("cd", "2021", &TRACKS);
        let folder = files(&[
            ("02 b.flac", Some(999)),
            ("01 a.flac", Some(1)),
            ("03 c.flac", None),
        ]);
        assert_eq!(
            assign(&folder, &release).unwrap(),
            [
                Some("cd-t2".to_string()),
                Some("cd-t1".to_string()),
                Some("cd-t3".to_string())
            ]
        );
        // Part of it: by their numbers.
        assert_eq!(
            assign(&folder[..2], &release).unwrap(),
            [Some("cd-t2".to_string()), Some("cd-t1".to_string())]
        );
        let more = files(&[("01", None), ("02", None), ("03", None), ("04", None)]);
        assert_eq!(
            assign(&more, &release).unwrap_err(),
            "4 files, but that release has 3 tracks"
        );
    }

    #[test]
    fn reads_track_numbers() {
        assert_eq!(track_number("x\\01 Intro.flac"), Some((1, 1)));
        assert_eq!(track_number("x\\2-07 - Song.flac"), Some((2, 7)));
        assert_eq!(track_number("x\\1.03 Song.flac"), Some((1, 3)));
        assert_eq!(track_number("x\\204 Song.flac"), Some((2, 4)));
        assert_eq!(track_number("x\\A1 Song.flac"), None);
    }

    #[test]
    fn part_of_a_release_lands_on_its_tracks() {
        let split = release(
            "r",
            "2014",
            &[
                ("Dishonest", None),
                ("Wonder", None),
                ("Pete Rose and Babe Ruth", None),
                ("Pawn Takes Rook", None),
            ],
        );
        let names = |n: &[&str]| files(&n.iter().map(|n| (*n, None)).collect::<Vec<_>>());
        // By title, whatever the numbers say.
        assert_eq!(
            assign(
                &names(&["01 Pawn Takes Rook.flac", "02 Pete Rose and Babe Ruth.flac"]),
                &split
            )
            .unwrap(),
            vec![Some("r-t4".into()), Some("r-t3".into())]
        );
        // By number, when the titles aren't there.
        assert_eq!(
            assign(&names(&["03.flac", "04.flac"]), &split).unwrap(),
            vec![Some("r-t3".into()), Some("r-t4".into())]
        );
        let error = assign(&names(&["track.flac"]), &split).unwrap_err();
        assert!(
            error.contains("can't tell which track track.flac is"),
            "{error}"
        );
        let error = assign(&names(&["03 Wonder.flac", "02.flac"]), &split).unwrap_err();
        assert!(error.contains("both look like \"Wonder\""), "{error}");
    }
}
