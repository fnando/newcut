//! Filing a finished download: its files tagged as MusicBrainz Picard tags
//! them (from the release they matched), named `1. Title.flac` (`1-01.
//! Title.flac` on releases of more than one medium), and moved into
//! `slskd.output_dir` (a path with `%{…}` placeholders).

use std::path::{Path, PathBuf};

use lofty::config::WriteOptions;
use lofty::file::TaggedFileExt;
use lofty::tag::{ItemKey, ItemValue, Tag, TagExt, TagItem};

use crate::musicbrainz::{ReleaseInfo, TrackInfo};

/// What `slskd.output_dir` can say.
pub const PLACEHOLDERS: [&str; 13] = [
    "artist",
    "artist_sort",
    "album",
    "year",
    "date",
    "release_year",
    "country",
    "media",
    "label",
    "catalog_number",
    "type",
    "releaseid",
    "releasegroupid",
];

/// Check `template`'s placeholders.
///
/// # Errors
///
/// Names the first that isn't one, or a `%{` left open.
pub fn check_template(template: &str) -> Result<(), String> {
    let mut rest = template;
    while let Some(start) = rest.find("%{") {
        let after = &rest[start + 2..];
        let Some(end) = after.find('}') else {
            return Err(format!("{template:?} has a %{{ that isn't closed"));
        };
        let name = &after[..end];
        if !PLACEHOLDERS.contains(&name) {
            return Err(format!(
                "%{{{name}}} isn't a placeholder; use {}",
                PLACEHOLDERS
                    .iter()
                    .map(|p| format!("%{{{p}}}"))
                    .collect::<Vec<_>>()
                    .join(", ")
            ));
        }
        rest = &after[end + 1..];
    }
    Ok(())
}

/// A placeholder's value for `release`.
fn value(name: &str, release: &ReleaseInfo) -> String {
    let year = |date: Option<&String>| {
        date.and_then(|d| d.get(..4))
            .unwrap_or_default()
            .to_string()
    };
    let text = |v: &Option<String>| v.clone().unwrap_or_default();
    match name {
        "artist" => release.artist.name.clone(),
        "artist_sort" => release.artist.sort.clone(),
        "album" => release.title.clone(),
        "year" => year(release.original_date.as_ref().or(release.date.as_ref())),
        "date" => text(&release.date),
        "release_year" => year(release.date.as_ref()),
        "country" => text(&release.country),
        "media" => media(release),
        "label" => text(&release.label),
        "catalog_number" => text(&release.catalog_number),
        "type" => release.types.first().cloned().unwrap_or_default(),
        "releaseid" => release.mbid.clone(),
        "releasegroupid" => release.group_mbid.clone(),
        _ => String::new(),
    }
}

/// "CD", "2×CD", "CD + DVD".
fn media(release: &ReleaseInfo) -> String {
    let mut groups: Vec<(String, usize)> = Vec::new();
    for format in &release.media {
        match groups.last_mut() {
            Some((last, n)) if last == format => *n += 1,
            _ => groups.push((format.clone(), 1)),
        }
    }
    groups
        .iter()
        .map(|(f, n)| {
            if *n > 1 {
                format!("{n}×{f}")
            } else {
                f.clone()
            }
        })
        .collect::<Vec<_>>()
        .join(" + ")
}

/// `template` with `release`'s values (each made safe as a file name).
#[must_use]
pub fn album_dir(template: &str, release: &ReleaseInfo) -> PathBuf {
    let mut out = String::new();
    let mut rest = template;
    while let Some(start) = rest.find("%{") {
        out.push_str(&rest[..start]);
        let after = &rest[start + 2..];
        let Some(end) = after.find('}') else {
            out.push_str(&rest[start..]);
            rest = "";
            break;
        };
        out.push_str(&safe(&value(&after[..end], release)));
        rest = &after[end + 1..];
    }
    out.push_str(rest);
    PathBuf::from(out)
}

/// `text` as a file name: no slashes, nor what Windows and SMB shares
/// refuse; no leading or trailing spaces and dots.
#[must_use]
pub fn safe(text: &str) -> String {
    let replaced: String = text
        .chars()
        .map(|c| match c {
            '/' | '\\' | ':' | '*' | '?' | '"' | '<' | '>' | '|' => '_',
            c if c.is_control() => '_',
            c => c,
        })
        .collect();
    let trimmed = replaced.trim_matches(|c: char| c == '.' || c.is_whitespace());
    if trimmed.is_empty() {
        "_".into()
    } else {
        trimmed.into()
    }
}

/// A track's file name: `7. Title.flac`, or `2-07. Title.flac` when the
/// release has more than one medium.
#[must_use]
pub fn track_file_name(release: &ReleaseInfo, track: &TrackInfo, extension: &str) -> String {
    let number = if release.media.len() > 1 {
        format!("{}-{:02}", track.disc, track.position)
    } else {
        track.position.to_string()
    };
    format!(
        "{}.{}",
        safe(&format!("{number}. {}", track.title)),
        extension.to_ascii_lowercase()
    )
}

/// Where slskd puts a file it downloads into `dir`: its own name (Unix
/// can't have `/` nor NUL in one).
#[must_use]
pub fn downloaded_path(dir: &Path, remote: &str) -> PathBuf {
    let name = remote.rsplit('\\').next().unwrap_or(remote);
    dir.join(name.replace(['/', '\0'], "_"))
}

/// A download to file.
pub struct Filing<'a> {
    /// Where slskd put the files.
    pub source: PathBuf,
    /// The files as queued (remote names and sizes).
    pub files: &'a [(String, u64)],
    /// Each file's track id on `release`, in `files` order.
    pub tracks: &'a [Option<String>],
    pub release: &'a ReleaseInfo,
    /// `slskd.output_dir`.
    pub template: &'a str,
    /// File the files that are there (some never came): you said so.
    pub partial: bool,
}

/// File a finished download: tagged as the release it matched, and moved
/// under `slskd.output_dir`, with the release's cover. Unless `forced` (you
/// said which release it is), only an exact match is. Returns the album's
/// folder.
///
/// # Errors
///
/// Why it can't be: for the manual queue.
pub async fn file_download(
    http: &crate::http::Http,
    musicbrainz: &str,
    download: &crate::state::downloads::Download,
    paths: &crate::config::load::ImportPaths,
    forced: bool,
) -> Result<PathBuf, String> {
    // Queued before newcut chose: where slskd puts downloads by default, a
    // folder named after the remote one.
    let destination = download.destination.clone().unwrap_or_else(|| {
        let name = download.dir.rsplit('\\').next().unwrap_or(&download.dir);
        name.replace(['/', '\0'], "_")
    });
    if !forced {
        let reason = download
            .matched
            .reason
            .as_deref()
            .unwrap_or("no reason given");
        match download.matched.kind.as_str() {
            // "chosen": you said which release it is.
            "exact" | "chosen" => {}
            "likely" => return Err(format!("only a likely match: {reason}")),
            _ => return Err(format!("no exact match: {reason}")),
        }
    }
    let Some(release) = &download.matched.release_mbid else {
        return Err("no release to tag it as".into());
    };
    let mb = crate::musicbrainz::MusicBrainz {
        http,
        server: musicbrainz,
    };
    let info = mb
        .release_info(release)
        .await
        .map_err(|e| format!("couldn't look up the release: {e:#}"))?
        .ok_or_else(|| format!("MusicBrainz doesn't know release {release}"))?;
    let dir = file(&Filing {
        source: paths.downloads.join(&destination),
        files: &download.files,
        tracks: &download.matched.tracks,
        release: &info,
        template: &paths.output,
        partial: forced,
    })?;
    save_cover(http, &info.mbid, &dir).await;
    Ok(dir)
}

#[cfg(not(test))]
const COVER_ART_ARCHIVE: &str = "https://coverartarchive.org";
/// Tests don't go out: nothing listens there.
#[cfg(test)]
const COVER_ART_ARCHIVE: &str = "http://127.0.0.1:9";

/// The release's front cover from the Cover Art Archive, as `cover.jpg`
/// (as `covers.local` reads them), when there's one.
async fn save_cover(http: &crate::http::Http, release: &str, dir: &Path) {
    let path = dir.join("cover.jpg");
    if path.exists() {
        return;
    }
    let url = format!("{COVER_ART_ARCHIVE}/release/{release}/front-500");
    match http.get_bytes("the Cover Art Archive", &url).await {
        Ok(Some(bytes)) => {
            if let Err(e) = std::fs::write(&path, bytes) {
                tracing::warn!("can't write {}: {e}", path.display());
            }
        }
        Ok(None) => tracing::debug!("no cover for release {release}"),
        Err(e) => tracing::warn!("cover of release {release}: {e:#}"),
    }
}

/// Tag and move a download's files; returns the album's folder.
///
/// Nothing is moved unless every file is there (with `partial`, at least
/// one; those that aren't are left out), has its track, and its place is
/// free; a file that can't be tagged stops it before any move.
///
/// # Errors
///
/// Says what's wrong, for the manual queue.
pub fn file(filing: &Filing) -> Result<PathBuf, String> {
    let release = filing.release;
    let target = album_dir(filing.template, release);
    let mut moves = Vec::new();
    for (i, (remote, _)) in filing.files.iter().enumerate() {
        let from = downloaded_path(&filing.source, remote);
        if !from.is_file() {
            if filing.partial {
                continue;
            }
            return Err(format!("{} isn't there", from.display()));
        }
        let track = filing
            .tracks
            .get(i)
            .and_then(Option::as_deref)
            .and_then(|id| release.tracks.iter().find(|t| t.mbid == id))
            .ok_or_else(|| format!("which track {} is isn't known", from.display()))?;
        let extension = from
            .extension()
            .and_then(|e| e.to_str())
            .unwrap_or_default()
            .to_string();
        let to = target.join(track_file_name(release, track, &extension));
        if to.exists() {
            return Err(format!("{} is there already", to.display()));
        }
        moves.push((from, to, track));
    }
    if moves.is_empty() {
        return Err(format!(
            "none of its files are in {}",
            filing.source.display()
        ));
    }
    for (from, _, track) in &moves {
        write_tags(from, release, track)
            .map_err(|e| format!("can't tag {}: {e}", from.display()))?;
    }
    std::fs::create_dir_all(&target)
        .map_err(|e| format!("can't create {}: {e}", target.display()))?;
    for (from, to, _) in &moves {
        move_file(from, to)
            .map_err(|e| format!("can't move {} to {}: {e}", from.display(), to.display()))?;
    }
    // Only what newcut asked for was in it; leave it if anything else is.
    let _ = std::fs::remove_dir(&filing.source);
    Ok(target)
}

fn move_file(from: &Path, to: &Path) -> std::io::Result<()> {
    if std::fs::rename(from, to).is_ok() {
        return Ok(());
    }
    // Another file system: copy, then remove.
    std::fs::copy(from, to)?;
    std::fs::remove_file(from)
}

/// Replace `path`'s tags with `track`'s on `release`.
///
/// # Errors
///
/// When the file can't be read or written.
pub fn write_tags(path: &Path, release: &ReleaseInfo, track: &TrackInfo) -> Result<(), String> {
    let tagged = lofty::read_from_path(path).map_err(|e| e.to_string())?;
    let kind = tagged.primary_tag_type();
    for tag in tagged.tags() {
        tag.tag_type()
            .remove_from_path(path, WriteOptions::default())
            .map_err(|e| e.to_string())?;
    }
    let mut tag = Tag::new(kind);
    let mut set = |key: ItemKey, value: &str| {
        if !value.is_empty() {
            tag.push(TagItem::new(key, ItemValue::Text(value.to_string())));
        }
    };
    let opt = |v: &Option<String>| v.clone().unwrap_or_default();
    set(ItemKey::TrackTitle, &track.title);
    set(ItemKey::TrackArtist, &track.artist.name);
    set(ItemKey::TrackArtistSortOrder, &track.artist.sort);
    for name in &track.artist.names {
        set(ItemKey::TrackArtists, name);
    }
    set(ItemKey::AlbumTitle, &release.title);
    set(ItemKey::AlbumArtist, &release.artist.name);
    set(ItemKey::AlbumArtistSortOrder, &release.artist.sort);
    for name in &release.artist.names {
        set(ItemKey::AlbumArtists, name);
    }
    set(ItemKey::TrackNumber, &track.position.to_string());
    let disc_tracks = release
        .track_counts
        .get(usize::try_from(track.disc.saturating_sub(1)).unwrap_or(0))
        .copied()
        .unwrap_or(0);
    set(ItemKey::TrackTotal, &disc_tracks.to_string());
    set(ItemKey::DiscNumber, &track.disc.to_string());
    set(ItemKey::DiscTotal, &release.media.len().to_string());
    set(ItemKey::RecordingDate, &opt(&release.date));
    set(
        ItemKey::OriginalReleaseDate,
        &opt(&release
            .original_date
            .clone()
            .or_else(|| release.date.clone())),
    );
    set(ItemKey::ReleaseCountry, &opt(&release.country));
    set(ItemKey::Label, &opt(&release.label));
    set(ItemKey::CatalogNumber, &opt(&release.catalog_number));
    set(ItemKey::Barcode, &opt(&release.barcode));
    set(ItemKey::Script, &opt(&release.script));
    set(ItemKey::Language, &opt(&release.language));
    let medium = usize::try_from(track.disc.saturating_sub(1)).unwrap_or(0);
    set(
        ItemKey::OriginalMediaType,
        release.media.get(medium).map_or("", String::as_str),
    );
    for kind in &release.types {
        set(ItemKey::MusicBrainzReleaseType, kind);
    }
    for isrc in &track.isrcs {
        set(ItemKey::Isrc, isrc);
    }
    set(ItemKey::MusicBrainzRecordingId, &track.recording_mbid);
    set(ItemKey::MusicBrainzTrackId, &track.mbid);
    set(ItemKey::MusicBrainzReleaseId, &release.mbid);
    set(ItemKey::MusicBrainzReleaseGroupId, &release.group_mbid);
    for id in &track.artist.ids {
        set(ItemKey::MusicBrainzArtistId, id);
    }
    for id in &release.artist.ids {
        set(ItemKey::MusicBrainzReleaseArtistId, id);
    }
    tag.save_to_path(path, WriteOptions::default())
        .map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::musicbrainz::Credit;

    fn release() -> ReleaseInfo {
        ReleaseInfo {
            mbid: "rel".into(),
            title: "Future: Nausea?".into(),
            artist: Credit {
                name: "A & B".into(),
                sort: "A & B, The".into(),
                ids: vec!["a".into(), "b".into()],
                names: vec!["A".into(), "B".into()],
            },
            date: Some("2026-11-06".into()),
            original_date: Some("2025-01-01".into()),
            group_mbid: "rg".into(),
            media: vec!["CD".into(), "CD".into()],
            types: vec!["album".into()],
            ..ReleaseInfo::default()
        }
    }

    #[test]
    fn checks_and_fills_placeholders() {
        assert!(check_template("/m/%{artist}/%{album} (%{year})").is_ok());
        let error = check_template("/m/%{albm}").unwrap_err();
        assert!(error.starts_with("%{albm} isn't a placeholder"), "{error}");
        assert!(check_template("/m/%{album").is_err());
        assert_eq!(
            album_dir(
                "/m/%{artist}/%{album} (%{year}, %{release_year}, %{media})",
                &release()
            ),
            PathBuf::from("/m/A & B/Future_ Nausea_ (2025, 2026, 2×CD)")
        );
        assert_eq!(safe(" ..AC/DC. "), "AC_DC");
        assert_eq!(safe("..."), "_");
    }

    #[test]
    fn names_tracks_like_the_library() {
        let mut r = release();
        let track = TrackInfo {
            disc: 2,
            position: 7,
            title: "What/Ever".into(),
            ..TrackInfo::default()
        };
        assert_eq!(track_file_name(&r, &track, "FLAC"), "2-07. What_Ever.flac");
        r.media.pop();
        assert_eq!(track_file_name(&r, &track, "flac"), "7. What_Ever.flac");
        assert_eq!(
            downloaded_path(Path::new("/d"), "@@x\\Album\\01 a/b.flac"),
            PathBuf::from("/d/01 a_b.flac")
        );
    }
}
