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
pub const PLACEHOLDERS: [&str; 14] = [
    "artist",
    "artistid",
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
    check_placeholders(template, &PLACEHOLDERS)
}

/// What `slskd.after_filing.args` can say: [`PLACEHOLDERS`], and
/// `album_dir` (the album's folder).
#[must_use]
pub fn arg_placeholders() -> Vec<&'static str> {
    let mut all = PLACEHOLDERS.to_vec();
    all.push("album_dir");
    all
}

/// Whether `template`'s placeholders are all `known`; names the first
/// that isn't one, or a `%{` left open.
///
/// # Errors
///
/// As said.
pub fn check_placeholders(template: &str, known: &[&str]) -> Result<(), String> {
    let mut rest = template;
    while let Some(start) = rest.find("%{") {
        let after = &rest[start + 2..];
        let Some(end) = after.find('}') else {
            return Err(format!("{template:?} has a %{{ that isn't closed"));
        };
        let name = &after[..end];
        if !known.contains(&name) {
            return Err(format!(
                "%{{{name}}} isn't a placeholder; use {}",
                known
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
        // As Picard's %musicbrainz_albumartistid%: each, in credit order.
        "artistid" => release.artist.ids.join("; "),
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
    PathBuf::from(fill(template, |name| safe(&value(name, release))))
}

/// `template` with each `%{name}` as `value` says.
fn fill(template: &str, value: impl Fn(&str) -> String) -> String {
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
        out.push_str(&value(&after[..end]));
        rest = &after[end + 1..];
    }
    out.push_str(rest);
    out
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
    /// An upgrade: the album folders it replaces, and where their files go.
    pub replacing: Option<Replacing<'a>>,
}

/// The album an upgrade replaces.
pub struct Replacing<'a> {
    /// Its folders.
    pub dirs: &'a [PathBuf],
    /// Where their files go (a folder of their own, made when needed).
    pub trash: PathBuf,
}

/// File a finished download: tagged as the release it matched, and moved
/// under `slskd.output_dir`, with the release's cover. Unless `forced` (you
/// said which release it is), only an exact match is. Then
/// `slskd.after_filing` runs on it. Returns the album's folder.
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
    shutdown: &crate::shutdown::Shutdown,
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
    let replaced: Vec<PathBuf> = download
        .upgrade
        .as_ref()
        .map_or_else(Vec::new, |u| u.replaces.iter().map(PathBuf::from).collect());
    let replacing = download.upgrade.as_ref().map(|_| Replacing {
        dirs: &replaced,
        trash: paths.replaced.join(format!(
            "{}-{}",
            jiff::Zoned::now().strftime("%Y%m%d-%H%M%S"),
            download.release_key
        )),
    });
    let dir = file(&Filing {
        source: paths.downloads.join(&destination),
        files: &download.files,
        tracks: &download.matched.tracks,
        release: &info,
        template: &paths.output,
        partial: forced,
        replacing,
    })?;
    save_cover(http, &info.mbid, &dir).await;
    if let Some(hook) = &paths.after_filing {
        // Filed either way: what it said is logged.
        match after_filing(hook, &dir, &info, shutdown).await {
            Ok(()) => tracing::info!("{} ran on {}", hook.bin.display(), dir.display()),
            Err(e) => tracing::warn!("slskd.after_filing on {}: {e:#}", dir.display()),
        }
    }
    Ok(dir)
}

/// How long `slskd.after_filing` gets between SIGTERM and SIGKILL.
const KILL_GRACE: std::time::Duration = std::time::Duration::from_secs(5);

/// Run `hook` on album folder `dir`: its `%{…}` arguments filled in
/// (`%{album_dir}` is `dir`), run in `dir`, with the release in its
/// environment too.
///
/// # Errors
///
/// When it can't start, exits with an error, or runs out of time.
pub async fn after_filing(
    hook: &crate::config::load::AfterFiling,
    dir: &Path,
    release: &ReleaseInfo,
    shutdown: &crate::shutdown::Shutdown,
) -> anyhow::Result<()> {
    let mut command = tokio::process::Command::new(&hook.bin);
    let album_dir = dir.to_string_lossy();
    let args = hook.args.iter().map(|arg| {
        fill(arg, |name| match name {
            "album_dir" => album_dir.to_string(),
            name => value(name, release),
        })
    });
    command
        .args(args)
        .current_dir(dir)
        .env("NEWCUT_ALBUM_DIR", dir)
        .env("NEWCUT_ARTIST", &release.artist.name)
        .env("NEWCUT_ARTIST_MBID", release.artist.ids.join("; "))
        .env("NEWCUT_ALBUM", &release.title)
        .env("NEWCUT_RELEASE_MBID", &release.mbid)
        .env("NEWCUT_RELEASE_GROUP_MBID", &release.group_mbid);
    let output = crate::process::output_with_input(
        &mut command,
        Vec::new(),
        hook.timeout,
        KILL_GRACE,
        shutdown,
    )
    .await
    .map_err(|e| anyhow::anyhow!("{}: {e}", hook.bin.display()))?;
    if output.status.success() {
        return Ok(());
    }
    anyhow::bail!(crate::process::failure(&hook.bin, &output))
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
        // An upgrade's old files are set aside first: checked after.
        if filing.replacing.is_none() && to.exists() {
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
    let aside = match &filing.replacing {
        Some(replacing) => Some(SetAside::new(replacing)?),
        None => None,
    };
    let undo = |why: String| {
        if let Some(aside) = &aside {
            aside.restore();
        }
        Err(why)
    };
    if let Some(taken) = moves.iter().find(|(_, to, _)| to.exists()) {
        return undo(format!("{} is there already", taken.1.display()));
    }
    if let Err(e) = std::fs::create_dir_all(&target) {
        return undo(format!("can't create {}: {e}", target.display()));
    }
    for (i, (from, to, _)) in moves.iter().enumerate() {
        if let Err(e) = move_file(from, to) {
            // Back where they came from, for the old ones to come back.
            for (from, to, _) in &moves[..i] {
                let _ = move_file(to, from);
            }
            return undo(format!(
                "can't move {} to {}: {e}",
                from.display(),
                to.display()
            ));
        }
    }
    if let Some(aside) = &aside {
        aside.finish(&target);
    }
    // Only what newcut asked for was in it; leave it if anything else is.
    let _ = std::fs::remove_dir(&filing.source);
    Ok(target)
}

/// The audio files an upgrade replaces, moved into its trash folder (each
/// album folder's into one named after it).
struct SetAside {
    /// Each replaced folder, and where its files went.
    dirs: Vec<(PathBuf, PathBuf)>,
    /// Each file: where it was, and where it is.
    moved: Vec<(PathBuf, PathBuf)>,
    trash: PathBuf,
}

impl SetAside {
    fn new(replacing: &Replacing) -> Result<Self, String> {
        let mut aside = Self {
            dirs: Vec::new(),
            moved: Vec::new(),
            trash: replacing.trash.clone(),
        };
        for dir in replacing.dirs.iter().filter(|d| d.is_dir()) {
            let name = dir
                .file_name()
                .map_or_else(|| "album".into(), |n| n.to_string_lossy().into_owned());
            let mut to = replacing.trash.join(&name);
            let mut n = 2;
            while to.exists() || aside.dirs.iter().any(|(_, t)| *t == to) {
                to = replacing.trash.join(format!("{name} ({n})"));
                n += 1;
            }
            let files = match audio_files(dir) {
                Ok(files) => files,
                Err(e) => {
                    aside.restore();
                    return Err(format!("can't read {}: {e}", dir.display()));
                }
            };
            if let Err(e) = std::fs::create_dir_all(&to) {
                aside.restore();
                return Err(format!("can't create {}: {e}", to.display()));
            }
            for file in files {
                let into = to.join(file.file_name().unwrap_or_default());
                if let Err(e) = move_file(&file, &into) {
                    aside.restore();
                    return Err(format!(
                        "can't move {} to {}: {e}",
                        file.display(),
                        into.display()
                    ));
                }
                aside.moved.push((file, into));
            }
            aside.dirs.push((dir.clone(), to));
        }
        Ok(aside)
    }

    /// Put the files back.
    fn restore(&self) {
        for (was, is) in self.moved.iter().rev() {
            if let Err(e) = move_file(is, was) {
                tracing::error!("can't put {} back in {}: {e}", is.display(), was.display());
            }
        }
        for (_, to) in &self.dirs {
            let _ = std::fs::remove_dir(to);
        }
        let _ = std::fs::remove_dir(&self.trash);
    }

    /// The new album's in `target`: the replaced folders left without audio
    /// (but `target`) go along with their files, covers and all.
    fn finish(&self, target: &Path) {
        for (dir, to) in &self.dirs {
            if dir == target || audio_files(dir).is_ok_and(|f| !f.is_empty()) {
                continue;
            }
            let Ok(entries) = std::fs::read_dir(dir) else {
                continue;
            };
            for entry in entries.flatten() {
                let into = to.join(entry.file_name());
                if !into.exists()
                    && let Err(e) = std::fs::rename(entry.path(), &into)
                {
                    tracing::warn!(
                        "can't move {} to {}: {e}",
                        entry.path().display(),
                        into.display()
                    );
                }
            }
            let _ = std::fs::remove_dir(dir);
        }
        if !self.dirs.is_empty() {
            tracing::info!("the files replaced are in {}", self.trash.display());
        }
    }
}

/// The audio files right in `dir`.
fn audio_files(dir: &Path) -> std::io::Result<Vec<PathBuf>> {
    let mut files = Vec::new();
    for entry in std::fs::read_dir(dir)? {
        let path = entry?.path();
        if path.is_file()
            && path
                .extension()
                .and_then(|e| e.to_str())
                .is_some_and(|e| crate::scan::AUDIO.contains(&e.to_ascii_lowercase().as_str()))
        {
            files.push(path);
        }
    }
    files.sort();
    Ok(files)
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

    #[tokio::test]
    async fn after_filing_runs_on_the_album_folder() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let album = root.join("album");
        std::fs::create_dir(&album).unwrap();
        let bin = root.join("hook.sh");
        std::fs::write(
            &bin,
            "#!/bin/sh\necho \"$#:$1 $2 $3|$(pwd -P)|$NEWCUT_ALBUM|$NEWCUT_RELEASE_GROUP_MBID\" > ran\n\
             [ \"$1\" = fail ] && { echo nope >&2; exit 3; }\nexit 0\n",
        )
        .unwrap();
        std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755)).unwrap();
        let mut release = release();
        release.group_mbid = "rg".into();
        let mut hook = crate::config::load::AfterFiling {
            bin: bin.clone(),
            args: vec![
                "--dir".into(),
                "%{album_dir}".into(),
                "%{artist} - %{album} [%{releasegroupid}] %{artistid}".into(),
            ],
            timeout: std::time::Duration::from_secs(5),
        };
        let shutdown = crate::shutdown::Shutdown::default();
        after_filing(&hook, &album, &release, &shutdown)
            .await
            .unwrap();
        assert_eq!(
            std::fs::read_to_string(album.join("ran")).unwrap().trim(),
            format!(
                "3:--dir {} A & B - Future: Nausea? [rg] a; b|{}|Future: Nausea?|rg",
                album.display(),
                album.display()
            )
        );

        hook.args = vec!["fail".into()];
        let error = after_filing(&hook, &album, &release, &shutdown)
            .await
            .unwrap_err()
            .to_string();
        assert!(error.contains("exit status: 3: nope"), "{error}");
    }
}
