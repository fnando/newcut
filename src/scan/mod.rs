//! Scanning the libraries: every audio file's MusicBrainz tags, read again
//! only when the file changed (its size or modification time).

pub mod tags;

use std::collections::HashSet;
use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

use jiff::Timestamp;
use rusqlite::Connection;

use crate::state::State;
use crate::state::library::{self, ScannedFile};

/// Audio file extensions (lowercase). Not `mp4`: in music folders, that's
/// video (an artist's backdrop), not an album.
pub const AUDIO: [&str; 15] = [
    "flac", "mp3", "m4a", "aac", "alac", "ogg", "oga", "opus", "wv", "ape", "wav", "aif", "aiff",
    "dsf", "mpc",
];

/// Write this many files per transaction.
const BATCH: usize = 200;

/// What a scan did.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Report {
    /// Audio files found.
    pub files: usize,
    /// Read (new or changed).
    pub read: usize,
    /// Gone since last time.
    pub removed: usize,
    /// Couldn't be read.
    pub unreadable: usize,
    /// Libraries that aren't there (their files are kept).
    pub missing: Vec<String>,
}

impl Report {
    #[must_use]
    pub fn summary(&self) -> String {
        let mut text = format!(
            "{} files; {} read, {} gone",
            self.files, self.read, self.removed
        );
        if self.unreadable > 0 {
            let _ = write!(text, ", {} unreadable", self.unreadable);
        }
        if !self.missing.is_empty() {
            let _ = write!(text, "; not found: {}", self.missing.join(", "));
        }
        text
    }
}

/// Scan `libraries`. `stopping` is asked between files; `progress` hears
/// what's going on now and then.
///
/// # Errors
///
/// On database errors.
pub fn scan(
    conn: &mut Connection,
    libraries: &[PathBuf],
    stopping: &dyn Fn() -> bool,
    progress: &mut dyn FnMut(&str),
) -> anyhow::Result<Report> {
    let mut report = Report::default();
    let names: Vec<String> = libraries.iter().map(|l| display(l)).collect();
    for (library, name) in libraries.iter().zip(&names) {
        if !library.is_dir() {
            tracing::warn!("library {name} isn't there; keeping what was found in it before");
            report.missing.push(name.clone());
            continue;
        }
        let known = library::known_files(conn, name)?;
        let Some(seen) = walk(conn, name, library, &known, &mut report, stopping, progress)? else {
            return Ok(report);
        };
        forget(conn, known.into_keys(), &seen, &mut report)?;
    }
    report.removed += library::remove_other_libraries(conn, &names)?;
    Ok(report)
}

/// Scan only `dirs` (folders in `library`): what's new or changed in them
/// is read, and what's gone from them is forgotten; the rest of the
/// library is left as it was.
///
/// # Errors
///
/// On database errors.
pub fn scan_dirs(
    conn: &mut Connection,
    library: &Path,
    dirs: &[PathBuf],
) -> anyhow::Result<Report> {
    let mut report = Report::default();
    let name = display(library);
    let known = library::known_files(conn, &name)?;
    for dir in dirs.iter().filter(|d| d.starts_with(library) && d.is_dir()) {
        let prefix = format!("{}/", display(dir));
        let known: std::collections::HashMap<String, (i64, i64)> = known
            .iter()
            .filter(|(path, _)| path.starts_with(&prefix))
            .map(|(path, at)| (path.clone(), *at))
            .collect();
        let seen = walk(
            conn,
            &name,
            dir,
            &known,
            &mut report,
            &|| false,
            &mut |_| {},
        )?
        .unwrap_or_default();
        forget(conn, known.into_keys(), &seen, &mut report)?;
    }
    Ok(report)
}

/// Scan `dir` (an album just put there) when it's in one of `libraries`;
/// returns whether it is.
///
/// # Errors
///
/// On database errors.
pub fn scan_new_album(
    conn: &mut Connection,
    libraries: &[PathBuf],
    dir: &Path,
) -> anyhow::Result<bool> {
    let Some(library) = libraries.iter().find(|l| dir.starts_with(l)) else {
        tracing::warn!(
            "{} isn't in any of your libraries, so it won't count as yours",
            dir.display()
        );
        return Ok(false);
    };
    let report = scan_dirs(conn, library, &[dir.to_path_buf()])?;
    tracing::info!("scanned {}: {}", dir.display(), report.summary());
    Ok(true)
}

/// Read the audio files under `root` (in library `name`) that aren't
/// `known` as they are. Returns the files seen, or `None` when stopped.
fn walk(
    conn: &mut Connection,
    name: &str,
    root: &Path,
    known: &std::collections::HashMap<String, (i64, i64)>,
    report: &mut Report,
    stopping: &dyn Fn() -> bool,
    progress: &mut dyn FnMut(&str),
) -> anyhow::Result<Option<HashSet<String>>> {
    let mut seen = HashSet::new();
    let mut batch = Vec::new();
    for entry in walkdir::WalkDir::new(root)
        .follow_links(true)
        .into_iter()
        .filter_map(|e| match e {
            Ok(entry) => Some(entry),
            Err(error) => {
                tracing::warn!("{error}");
                None
            }
        })
    {
        if stopping() {
            return Ok(None);
        }
        if !entry.file_type().is_file() || !is_audio(entry.path()) {
            continue;
        }
        let path = display(entry.path());
        report.files += 1;
        seen.insert(path.clone());
        let Ok(meta) = entry.metadata() else {
            report.unreadable += 1;
            continue;
        };
        let size = i64::try_from(meta.len()).unwrap_or(i64::MAX);
        let mtime = meta
            .modified()
            .ok()
            .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
            .map_or(0, |d| i64::try_from(d.as_secs()).unwrap_or(i64::MAX));
        if known.get(&path) == Some(&(mtime, size)) {
            continue;
        }
        match tags::read(entry.path()) {
            Ok(tags) => {
                report.read += 1;
                batch.push(ScannedFile {
                    dir: entry.path().parent().map_or_default(display),
                    path,
                    library: name.to_string(),
                    mtime,
                    size,
                    album: tags.album,
                    album_artist: tags.album_artist,
                    release_mbid: tags.release_mbid,
                    release_group_mbid: tags.release_group_mbid,
                    artist_mbids: tags.artist_mbids,
                });
            }
            Err(error) => {
                tracing::debug!("{path}: {error}");
                report.unreadable += 1;
            }
        }
        if batch.len() >= BATCH {
            write(conn, &mut batch)?;
            progress(&format!(
                "{name}: {} files, {} read",
                report.files, report.read
            ));
        }
    }
    write(conn, &mut batch)?;
    Ok(Some(seen))
}

/// Forget the `known` files that weren't `seen`.
fn forget(
    conn: &mut Connection,
    known: impl Iterator<Item = String>,
    seen: &HashSet<String>,
    report: &mut Report,
) -> anyhow::Result<()> {
    let gone: Vec<String> = known.filter(|path| !seen.contains(path)).collect();
    report.removed += gone.len();
    let tx = State::write_transaction(conn)?;
    library::remove_files(&tx, &gone)?;
    tx.commit()?;
    Ok(())
}

fn write(conn: &mut Connection, batch: &mut Vec<ScannedFile>) -> anyhow::Result<()> {
    if batch.is_empty() {
        return Ok(());
    }
    let now = Timestamp::now();
    let tx = State::write_transaction(conn)?;
    for file in batch.drain(..) {
        library::upsert_file(&tx, &file, now)?;
    }
    tx.commit()?;
    Ok(())
}

fn is_audio(path: &Path) -> bool {
    path.extension()
        .and_then(|e| e.to_str())
        .is_some_and(|e| AUDIO.contains(&e.to_ascii_lowercase().as_str()))
}

fn display(path: &Path) -> String {
    path.to_string_lossy().into_owned()
}
