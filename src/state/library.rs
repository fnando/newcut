//! The library in the state DB: files and their tags, artists, release
//! groups (discographies), and what's ignored.

use std::collections::{HashMap, HashSet};

use jiff::Timestamp;
use rusqlite::{Connection, OptionalExtension, params};

/// MusicBrainz's special artists, never followed: Various Artists, [unknown],
/// [no artist], [anonymous], [traditional], [data].
pub const SPECIAL_ARTISTS: [&str; 6] = [
    "89ad4ac3-39f7-470e-963a-56509c546377",
    "125ec42a-7229-4250-afc5-e057484327fe",
    "eec63d3c-3b81-4ad4-b1e4-7c147d4d2b61",
    "f731ccc4-e22a-43af-a747-64213329e088",
    "9be7f096-97ec-4615-8957-8d40b5dcbc41",
    "33cf029c-63b0-41a0-9855-be2a3665fb3b",
];

// ---------- files ----------

/// An audio file as the scanner read it.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ScannedFile {
    pub path: String,
    pub library: String,
    pub dir: String,
    pub mtime: i64,
    pub size: i64,
    pub album: Option<String>,
    pub album_artist: Option<String>,
    pub release_mbid: Option<String>,
    pub release_group_mbid: Option<String>,
    pub artist_mbids: Vec<String>,
    pub quality: crate::quality::Quality,
}

/// What's known of each file in `library`: path → (mtime, size).
///
/// # Errors
///
/// On database errors.
pub fn known_files(
    conn: &Connection,
    library: &str,
) -> rusqlite::Result<HashMap<String, (i64, i64)>> {
    let mut stmt = conn.prepare("SELECT path, mtime, size FROM files WHERE library = ?1")?;
    stmt.query_map([library], |r| Ok((r.get(0)?, (r.get(1)?, r.get(2)?))))?
        .collect()
}

/// Record a scanned file (and its artists, followed from now on).
///
/// # Errors
///
/// On database errors.
pub fn upsert_file(conn: &Connection, file: &ScannedFile, now: Timestamp) -> rusqlite::Result<()> {
    // A release already looked up gives the release group right away.
    let group = match (&file.release_group_mbid, &file.release_mbid) {
        (Some(group), _) => Some(group.clone()),
        (None, Some(release)) => conn
            .query_row(
                "SELECT release_group_mbid FROM releases WHERE mbid = ?1",
                [release],
                |r| r.get::<_, Option<String>>(0),
            )
            .optional()?
            .flatten(),
        (None, None) => None,
    };
    conn.execute(
        "INSERT INTO files (path, library, dir, mtime, size, album, album_artist, release_mbid,
           release_group_mbid, scanned_at, format, lossless, bit_depth, sample_rate, bitrate)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15)
         ON CONFLICT (path) DO UPDATE SET library = excluded.library, dir = excluded.dir,
           mtime = excluded.mtime, size = excluded.size, album = excluded.album,
           album_artist = excluded.album_artist, release_mbid = excluded.release_mbid,
           release_group_mbid = excluded.release_group_mbid, scanned_at = excluded.scanned_at,
           format = excluded.format, lossless = excluded.lossless,
           bit_depth = excluded.bit_depth, sample_rate = excluded.sample_rate,
           bitrate = excluded.bitrate",
        params![
            file.path,
            file.library,
            file.dir,
            file.mtime,
            file.size,
            file.album,
            file.album_artist,
            file.release_mbid,
            group,
            now.to_string(),
            file.quality.format,
            file.quality.lossless,
            file.quality.bit_depth,
            file.quality.sample_rate,
            file.quality.bitrate,
        ],
    )?;
    conn.execute("DELETE FROM file_artists WHERE path = ?1", [&file.path])?;
    // A name from the tags only when it can't be someone else's.
    let name = (file.artist_mbids.len() == 1)
        .then_some(file.album_artist.as_deref())
        .flatten();
    for mbid in &file.artist_mbids {
        if SPECIAL_ARTISTS.contains(&mbid.as_str()) {
            continue;
        }
        conn.execute(
            "INSERT INTO file_artists (path, artist_mbid) VALUES (?1, ?2)
             ON CONFLICT DO NOTHING",
            params![file.path, mbid],
        )?;
        // A joined artist's albums are their members' too.
        conn.execute(
            "INSERT INTO file_artists (path, artist_mbid)
             SELECT ?1, member_mbid FROM artist_members WHERE artist_mbid = ?2
             ON CONFLICT DO NOTHING",
            params![file.path, mbid],
        )?;
        conn.execute(
            "INSERT INTO artists (mbid, name, created_at) VALUES (?1, ?2, ?3)
             ON CONFLICT (mbid) DO UPDATE SET name = coalesce(artists.name, excluded.name)",
            params![mbid, name, now.to_string()],
        )?;
    }
    Ok(())
}

/// Forget files that are gone.
///
/// # Errors
///
/// On database errors.
pub fn remove_files(conn: &Connection, paths: &[String]) -> rusqlite::Result<()> {
    let mut stmt = conn.prepare("DELETE FROM files WHERE path = ?1")?;
    for path in paths {
        stmt.execute([path])?;
    }
    Ok(())
}

/// Forget the files in folder `dir` and those under it (gone: an upgrade
/// replaced them).
///
/// # Errors
///
/// On database errors.
pub fn remove_files_under(conn: &Connection, dir: &str) -> rusqlite::Result<usize> {
    let dir = dir.trim_end_matches('/');
    let prefix = format!("{dir}/");
    conn.execute(
        "DELETE FROM files WHERE dir = ?1 OR substr(dir, 1, length(?2)) = ?2",
        params![dir, prefix],
    )
}

/// Forget the files of libraries no longer in the config.
///
/// # Errors
///
/// On database errors.
pub fn remove_other_libraries(conn: &Connection, libraries: &[String]) -> rusqlite::Result<usize> {
    let known: Vec<String> = {
        let mut stmt = conn.prepare("SELECT DISTINCT library FROM files")?;
        stmt.query_map([], |r| r.get(0))?
            .collect::<rusqlite::Result<_>>()?
    };
    let mut removed = 0;
    for library in known.iter().filter(|l| !libraries.contains(l)) {
        removed += conn.execute("DELETE FROM files WHERE library = ?1", [library])?;
    }
    Ok(removed)
}

/// Releases on files with no release group, not looked up yet.
///
/// # Errors
///
/// On database errors.
pub fn releases_to_look_up(conn: &Connection) -> rusqlite::Result<Vec<String>> {
    let mut stmt = conn.prepare(
        "SELECT DISTINCT f.release_mbid FROM files f
         LEFT JOIN releases r ON r.mbid = f.release_mbid
         WHERE f.release_group_mbid IS NULL AND f.release_mbid IS NOT NULL AND r.mbid IS NULL",
    )?;
    stmt.query_map([], |r| r.get(0))?.collect()
}

/// A release's release group (`None`: MusicBrainz doesn't know the release).
///
/// # Errors
///
/// On database errors.
pub fn set_release_group(
    conn: &Connection,
    release: &str,
    group: Option<&str>,
    now: Timestamp,
) -> rusqlite::Result<()> {
    conn.execute(
        "INSERT INTO releases (mbid, release_group_mbid, looked_up_at) VALUES (?1, ?2, ?3)
         ON CONFLICT (mbid) DO UPDATE SET release_group_mbid = excluded.release_group_mbid,
           looked_up_at = excluded.looked_up_at",
        params![release, group, now.to_string()],
    )?;
    if group.is_some() {
        conn.execute(
            "UPDATE files SET release_group_mbid = ?2
             WHERE release_mbid = ?1 AND release_group_mbid IS NULL",
            params![release, group],
        )?;
    }
    Ok(())
}

// ---------- artists ----------

/// Artists MusicBrainz wasn't asked about yet.
///
/// # Errors
///
/// On database errors.
pub fn artists_to_resolve(conn: &Connection) -> rusqlite::Result<Vec<String>> {
    let mut stmt =
        conn.prepare("SELECT mbid FROM artists WHERE resolved_at IS NULL ORDER BY name")?;
    stmt.query_map([], |r| r.get(0))?.collect()
}

/// What MusicBrainz says about an artist.
///
/// # Errors
///
/// On database errors.
pub fn set_resolved(
    conn: &Connection,
    mbid: &str,
    name: &str,
    sort_name: &str,
    now: Timestamp,
) -> rusqlite::Result<()> {
    conn.execute(
        "UPDATE artists SET name = ?2, sort_name = ?3, resolved_at = ?4, last_error = NULL
         WHERE mbid = ?1",
        params![mbid, name, sort_name, now.to_string()],
    )?;
    Ok(())
}

/// A lookup for an artist failed.
///
/// # Errors
///
/// On database errors.
pub fn set_artist_error(conn: &Connection, mbid: &str, error: &str) -> rusqlite::Result<()> {
    conn.execute(
        "UPDATE artists SET last_error = ?2 WHERE mbid = ?1",
        params![mbid, error],
    )?;
    Ok(())
}

/// Followed artists, and when each is checked next.
///
/// # Errors
///
/// On database errors.
pub fn followed_schedule(conn: &Connection) -> rusqlite::Result<Vec<(String, Option<Timestamp>)>> {
    let mut stmt = conn.prepare(
        "SELECT mbid, next_check_at FROM artists
         WHERE followed = 1 AND resolved_at IS NOT NULL ORDER BY sort_name, name",
    )?;
    stmt.query_map([], |r| {
        let next: Option<String> = r.get(1)?;
        Ok((r.get(0)?, next.and_then(|t| t.parse().ok())))
    })?
    .collect()
}

/// An artist's discography was checked at `now`; next at `next`. The first
/// check is the baseline: nothing it found counts as new.
///
/// # Errors
///
/// On database errors.
pub fn set_checked(
    conn: &Connection,
    mbid: &str,
    now: Timestamp,
    next: Timestamp,
) -> rusqlite::Result<()> {
    conn.execute(
        "UPDATE artists SET last_checked_at = ?2, next_check_at = ?3,
           baseline_at = coalesce(baseline_at, ?2), last_error = NULL
         WHERE mbid = ?1",
        params![mbid, now.to_string(), next.to_string()],
    )?;
    Ok(())
}

/// Check an artist again now.
///
/// # Errors
///
/// On database errors.
pub fn check_now(conn: &Connection, mbid: &str) -> rusqlite::Result<bool> {
    Ok(conn.execute(
        "UPDATE artists SET next_check_at = NULL WHERE mbid = ?1",
        [mbid],
    )? > 0)
}

/// A release group's title and artist credit, when it's known.
///
/// # Errors
///
/// On database errors.
pub fn release_group_title(
    conn: &Connection,
    mbid: &str,
) -> rusqlite::Result<Option<(String, Option<String>)>> {
    conn.query_row(
        "SELECT title, artist_credit FROM release_groups WHERE mbid = ?1",
        [mbid],
        |r| Ok((r.get(0)?, r.get(1)?)),
    )
    .optional()
}

/// The artists of release group `mbid`.
///
/// # Errors
///
/// On database errors.
pub fn group_artist_ids(conn: &Connection, mbid: &str) -> rusqlite::Result<Vec<String>> {
    let mut stmt =
        conn.prepare("SELECT artist_mbid FROM release_group_artists WHERE rg_mbid = ?1")?;
    stmt.query_map([mbid], |r| r.get(0))?.collect()
}

/// Follow or stop following an artist.
///
/// # Errors
///
/// On database errors.
pub fn set_followed(conn: &Connection, mbid: &str, followed: bool) -> rusqlite::Result<bool> {
    Ok(conn.execute(
        "UPDATE artists SET followed = ?2 WHERE mbid = ?1",
        params![mbid, followed],
    )? > 0)
}

/// Followed artists' MusicBrainz ids.
///
/// # Errors
///
/// On database errors.
pub fn followed_ids(conn: &Connection) -> rusqlite::Result<HashSet<String>> {
    let mut stmt = conn.prepare("SELECT mbid FROM artists WHERE followed = 1")?;
    stmt.query_map([], |r| r.get(0))?.collect()
}

/// An artist, as pages show it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Artist {
    pub mbid: String,
    pub name: String,
    pub sort_name: Option<String>,
    pub followed: bool,
    pub baseline_at: Option<Timestamp>,
    pub last_checked_at: Option<Timestamp>,
    pub next_check_at: Option<Timestamp>,
    pub last_error: Option<String>,
    /// A joined artist: hidden, their albums credited to their members.
    pub joined: bool,
}

const ARTIST_COLUMNS: &str = "mbid, coalesce(name, mbid), sort_name, followed,
    baseline_at, last_checked_at, next_check_at, last_error,
    EXISTS (SELECT 1 FROM artist_members m WHERE m.artist_mbid = artists.mbid)";

/// SQL: artist `a` isn't a joined artist.
const NOT_JOINED: &str = "NOT EXISTS (SELECT 1 FROM artist_members m WHERE m.artist_mbid = a.mbid)";

fn artist_row(r: &rusqlite::Row<'_>) -> rusqlite::Result<Artist> {
    let time = |s: Option<String>| s.and_then(|t| t.parse().ok());
    Ok(Artist {
        mbid: r.get(0)?,
        name: r.get(1)?,
        sort_name: r.get(2)?,
        followed: r.get(3)?,
        baseline_at: time(r.get(4)?),
        last_checked_at: time(r.get(5)?),
        next_check_at: time(r.get(6)?),
        last_error: r.get(7)?,
        joined: r.get(8)?,
    })
}

/// One artist.
///
/// # Errors
///
/// On database errors.
pub fn artist(conn: &Connection, mbid: &str) -> rusqlite::Result<Option<Artist>> {
    conn.query_row(
        &format!("SELECT {ARTIST_COLUMNS} FROM artists WHERE mbid = ?1"),
        [mbid],
        artist_row,
    )
    .optional()
}

/// Follow artist `mbid`, files or not (found by a search); they're looked
/// up and checked like the others. Returns false for MusicBrainz's special
/// artists, which aren't followed.
///
/// # Errors
///
/// On database errors.
pub fn add_artist(
    conn: &Connection,
    mbid: &str,
    name: &str,
    sort_name: &str,
    now: Timestamp,
) -> rusqlite::Result<bool> {
    if SPECIAL_ARTISTS.contains(&mbid) {
        return Ok(false);
    }
    conn.execute(
        "INSERT INTO artists (mbid, name, sort_name, created_at) VALUES (?1, ?2, ?3, ?4)
         ON CONFLICT (mbid) DO UPDATE SET followed = 1",
        params![
            mbid,
            name,
            Some(sort_name).filter(|s| !s.is_empty()),
            now.to_string()
        ],
    )?;
    Ok(true)
}

/// Artist `mbid` is a joined artist of `members` (ids and names), or not
/// one (empty). Members become artists, and the joined artist's albums and
/// release groups are theirs too.
///
/// # Errors
///
/// On database errors.
pub fn set_members(
    conn: &Connection,
    mbid: &str,
    members: &[(String, String)],
    now: Timestamp,
) -> rusqlite::Result<()> {
    conn.execute("DELETE FROM artist_members WHERE artist_mbid = ?1", [mbid])?;
    for (member, name) in members {
        conn.execute(
            "INSERT INTO artist_members (artist_mbid, member_mbid) VALUES (?1, ?2)",
            params![mbid, member],
        )?;
        conn.execute(
            "INSERT INTO artists (mbid, name, created_at) VALUES (?1, ?2, ?3)
             ON CONFLICT (mbid) DO NOTHING",
            params![member, name, now.to_string()],
        )?;
        conn.execute(
            "INSERT INTO file_artists (path, artist_mbid)
             SELECT path, ?2 FROM file_artists WHERE artist_mbid = ?1
             ON CONFLICT DO NOTHING",
            params![mbid, member],
        )?;
        conn.execute(
            "INSERT INTO release_group_artists (rg_mbid, artist_mbid)
             SELECT rg_mbid, ?2 FROM release_group_artists WHERE artist_mbid = ?1
             ON CONFLICT DO NOTHING",
            params![mbid, member],
        )?;
    }
    Ok(())
}

/// A joined artist's members (ids and names).
///
/// # Errors
///
/// On database errors.
pub fn members(conn: &Connection, mbid: &str) -> rusqlite::Result<Vec<(String, String)>> {
    let mut stmt = conn.prepare(
        "SELECT m.member_mbid, coalesce(a.name, m.member_mbid) FROM artist_members m
         LEFT JOIN artists a ON a.mbid = m.member_mbid
         WHERE m.artist_mbid = ?1 ORDER BY lower(coalesce(a.sort_name, a.name))",
    )?;
    stmt.query_map([mbid], |r| Ok((r.get(0)?, r.get(1)?)))?
        .collect()
}

/// Every artist but joined ones, by sort name.
///
/// # Errors
///
/// On database errors.
pub fn artists(conn: &Connection) -> rusqlite::Result<Vec<Artist>> {
    let mut stmt = conn.prepare(&format!(
        "SELECT {ARTIST_COLUMNS} FROM artists
         WHERE NOT EXISTS (SELECT 1 FROM artist_members m WHERE m.artist_mbid = artists.mbid)
         ORDER BY lower(coalesce(sort_name, name, mbid))"
    ))?;
    stmt.query_map([], artist_row)?.collect()
}

// ---------- release groups ----------

/// A release group as MusicBrainz or ListenBrainz describe it.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct NewReleaseGroup {
    pub mbid: String,
    pub title: String,
    pub artist_credit: Option<String>,
    pub primary_type: Option<String>,
    pub secondary_types: Vec<String>,
    pub first_release_date: Option<String>,
    pub artist_mbids: Vec<String>,
}

/// Record a release group (keeping when and how it was first found).
/// Returns whether it wasn't known before.
///
/// # Errors
///
/// On database errors.
pub fn upsert_release_group(
    conn: &Connection,
    group: &NewReleaseGroup,
    source: &str,
    now: Timestamp,
) -> rusqlite::Result<bool> {
    let known: bool = conn.query_row(
        "SELECT count(*) FROM release_groups WHERE mbid = ?1",
        [&group.mbid],
        |r| r.get(0),
    )?;
    let secondary = serde_json::to_string(&group.secondary_types).expect("strings serialize");
    // An empty date from one source doesn't erase another's.
    conn.execute(
        "INSERT INTO release_groups (mbid, title, artist_credit, primary_type, secondary_types,
           first_release_date, source, discovered_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)
         ON CONFLICT (mbid) DO UPDATE SET title = excluded.title,
           artist_credit = coalesce(excluded.artist_credit, release_groups.artist_credit),
           primary_type = coalesce(excluded.primary_type, release_groups.primary_type),
           secondary_types = excluded.secondary_types,
           first_release_date = coalesce(nullif(excluded.first_release_date, ''),
             release_groups.first_release_date)",
        params![
            group.mbid,
            group.title,
            group.artist_credit,
            group.primary_type,
            secondary,
            group
                .first_release_date
                .as_deref()
                .filter(|d| !d.is_empty()),
            source,
            now.to_string()
        ],
    )?;
    for artist in &group.artist_mbids {
        conn.execute(
            "INSERT INTO release_group_artists (rg_mbid, artist_mbid) VALUES (?1, ?2)
             ON CONFLICT DO NOTHING",
            params![group.mbid, artist],
        )?;
        conn.execute(
            "INSERT INTO release_group_artists (rg_mbid, artist_mbid)
             SELECT ?1, member_mbid FROM artist_members WHERE artist_mbid = ?2
             ON CONFLICT DO NOTHING",
            params![group.mbid, artist],
        )?;
    }
    Ok(!known)
}

/// A release group with what the library knows of it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReleaseGroup {
    pub mbid: String,
    pub title: String,
    pub artist_credit: Option<String>,
    pub primary_type: Option<String>,
    pub secondary_types: Vec<String>,
    pub first_release_date: Option<String>,
    pub source: String,
    pub discovered_at: Timestamp,
    /// Folders with it.
    pub owned_in: Vec<String>,
    pub ignored: bool,
    /// Its artists (MusicBrainz ids) and their baselines.
    pub artists: Vec<(String, Option<Timestamp>)>,
}

/// Release groups: an artist's, or every followed artist's (`None`).
///
/// # Errors
///
/// On database errors.
pub fn release_groups(
    conn: &Connection,
    artist: Option<&str>,
) -> rusqlite::Result<Vec<ReleaseGroup>> {
    match artist {
        Some(artist) => query_groups(
            conn,
            "WHERE rg.mbid IN (SELECT rg_mbid FROM release_group_artists WHERE artist_mbid = ?1)",
            Some(artist),
        ),
        None => query_groups(
            conn,
            &format!(
                "WHERE rg.mbid IN (SELECT rga.rg_mbid FROM release_group_artists rga
                   JOIN artists a ON a.mbid = rga.artist_mbid
                   WHERE a.followed = 1 AND {NOT_JOINED})"
            ),
            None,
        ),
    }
}

/// Every release group, followed artists' or not.
///
/// # Errors
///
/// On database errors.
pub fn release_groups_all(conn: &Connection) -> rusqlite::Result<Vec<ReleaseGroup>> {
    query_groups(conn, "", None)
}

fn query_groups(
    conn: &Connection,
    filter: &str,
    param: Option<&str>,
) -> rusqlite::Result<Vec<ReleaseGroup>> {
    let mut stmt = conn.prepare(&format!(
        "SELECT rg.mbid, rg.title, rg.artist_credit, rg.primary_type, rg.secondary_types,
           rg.first_release_date, rg.source, rg.discovered_at,
           (SELECT count(*) FROM ignored i WHERE i.rg_mbid = rg.mbid)
         FROM release_groups rg {filter}
         ORDER BY coalesce(rg.first_release_date, '9999') DESC, rg.title"
    ))?;
    let map = |r: &rusqlite::Row<'_>| {
        let secondary: String = r.get(4)?;
        let discovered: String = r.get(7)?;
        Ok(ReleaseGroup {
            mbid: r.get(0)?,
            title: r.get(1)?,
            artist_credit: r.get(2)?,
            primary_type: r.get(3)?,
            secondary_types: serde_json::from_str(&secondary).unwrap_or_default(),
            first_release_date: r.get(5)?,
            source: r.get(6)?,
            discovered_at: discovered.parse().unwrap_or(Timestamp::UNIX_EPOCH),
            owned_in: Vec::new(),
            ignored: r.get::<_, i64>(8)? > 0,
            artists: Vec::new(),
        })
    };
    let mut groups: Vec<ReleaseGroup> = match param {
        Some(p) => stmt.query_map([p], map)?.collect::<rusqlite::Result<_>>()?,
        None => stmt.query_map([], map)?.collect::<rusqlite::Result<_>>()?,
    };
    let owned = owned_dirs(conn)?;
    let artists = group_artists(conn)?;
    for group in &mut groups {
        group.owned_in = owned.get(&group.mbid).cloned().unwrap_or_default();
        group.artists = artists.get(&group.mbid).cloned().unwrap_or_default();
    }
    Ok(groups)
}

/// Release group → the folders that have it.
fn owned_dirs(conn: &Connection) -> rusqlite::Result<HashMap<String, Vec<String>>> {
    let mut stmt = conn.prepare(
        "SELECT DISTINCT release_group_mbid, dir FROM files
         WHERE release_group_mbid IS NOT NULL ORDER BY dir",
    )?;
    let mut owned: HashMap<String, Vec<String>> = HashMap::new();
    for row in stmt.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))? {
        let (group, dir) = row?;
        owned.entry(group).or_default().push(dir);
    }
    Ok(owned)
}

/// The folders that have release group `mbid`.
///
/// # Errors
///
/// On database errors.
pub fn dirs_with(conn: &Connection, mbid: &str) -> rusqlite::Result<Vec<String>> {
    let mut stmt =
        conn.prepare("SELECT DISTINCT dir FROM files WHERE release_group_mbid = ?1 ORDER BY dir")?;
    stmt.query_map([mbid], |r| r.get(0))?.collect()
}

/// Where artist `mbid`'s albums are: each folder (and its library) with
/// files that name them as an album artist.
///
/// # Errors
///
/// On database errors.
pub fn artist_album_dirs(conn: &Connection, mbid: &str) -> rusqlite::Result<Vec<(String, String)>> {
    let mut stmt = conn.prepare(
        "SELECT DISTINCT f.library, f.dir FROM files f
         JOIN file_artists fa ON fa.path = f.path
         WHERE fa.artist_mbid = ?1 ORDER BY f.dir",
    )?;
    stmt.query_map([mbid], |r| Ok((r.get(0)?, r.get(1)?)))?
        .collect()
}

/// The tagged files under folder `dir`: each one's album folder and
/// artists, by album folder and path (untagged ones, like a backdrop video,
/// are no one's).
///
/// # Errors
///
/// On database errors.
pub fn tagged_files_under(
    conn: &Connection,
    dir: &str,
) -> rusqlite::Result<Vec<(String, Vec<String>)>> {
    let prefix = format!("{}/", dir.trim_end_matches('/'));
    let mut stmt = conn.prepare(
        "SELECT f.dir, group_concat(fa.artist_mbid, ' ') FROM files f
         JOIN file_artists fa ON fa.path = f.path
         WHERE substr(f.dir, 1, length(?1)) = ?1
         GROUP BY f.path ORDER BY f.dir, f.path",
    )?;
    stmt.query_map([prefix], |r| {
        let artists: String = r.get(1)?;
        Ok((r.get(0)?, artists.split(' ').map(str::to_string).collect()))
    })?
    .collect()
}

/// The artists with files in the libraries.
///
/// # Errors
///
/// On database errors.
pub fn artists_with_files(conn: &Connection) -> rusqlite::Result<HashSet<String>> {
    let mut stmt = conn.prepare("SELECT DISTINCT artist_mbid FROM file_artists")?;
    stmt.query_map([], |r| r.get(0))?.collect()
}

type Baselines = HashMap<String, Vec<(String, Option<Timestamp>)>>;

/// Release group → its artists and their baselines.
fn group_artists(conn: &Connection) -> rusqlite::Result<Baselines> {
    let mut stmt = conn.prepare(&format!(
        "SELECT rga.rg_mbid, rga.artist_mbid, a.baseline_at FROM release_group_artists rga
             JOIN artists a ON a.mbid = rga.artist_mbid WHERE {NOT_JOINED}"
    ))?;
    let mut map: Baselines = HashMap::new();
    for row in stmt.query_map([], |r| {
        Ok((
            r.get::<_, String>(0)?,
            r.get::<_, String>(1)?,
            r.get::<_, Option<String>>(2)?,
        ))
    })? {
        let (group, artist, baseline) = row?;
        map.entry(group)
            .or_default()
            .push((artist, baseline.and_then(|t| t.parse().ok())));
    }
    Ok(map)
}

/// Owned release groups for an artist that MusicBrainz's discography
/// doesn't list (yet): folders whose tags name the artist.
///
/// # Errors
///
/// On database errors.
pub fn owned_albums(conn: &Connection, artist: &str) -> rusqlite::Result<Vec<OwnedAlbum>> {
    let mut stmt = conn.prepare(
        "SELECT f.dir, max(f.album), f.release_group_mbid, count(*) FROM files f
         JOIN file_artists fa ON fa.path = f.path
         WHERE fa.artist_mbid = ?1
         GROUP BY f.dir, f.release_group_mbid ORDER BY f.dir",
    )?;
    stmt.query_map([artist], |r| {
        Ok(OwnedAlbum {
            dir: r.get(0)?,
            album: r.get(1)?,
            release_group_mbid: r.get(2)?,
            tracks: r.get(3)?,
        })
    })?
    .collect()
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OwnedAlbum {
    pub dir: String,
    pub album: Option<String>,
    pub release_group_mbid: Option<String>,
    pub tracks: i64,
}

/// Ignore (or stop ignoring) a release group.
///
/// # Errors
///
/// On database errors.
pub fn set_ignored(
    conn: &Connection,
    mbid: &str,
    ignored: bool,
    now: Timestamp,
) -> rusqlite::Result<()> {
    if ignored {
        conn.execute(
            "INSERT INTO ignored (rg_mbid, created_at) VALUES (?1, ?2) ON CONFLICT DO NOTHING",
            params![mbid, now.to_string()],
        )?;
    } else {
        conn.execute("DELETE FROM ignored WHERE rg_mbid = ?1", [mbid])?;
    }
    Ok(())
}

// ---------- quality ----------

/// An album you have, and how good it is.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AlbumQuality {
    pub release_group_mbid: String,
    /// Its best copy's folders (a copy: its files in one library).
    pub dirs: Vec<String>,
    /// The releases its files are tagged as.
    pub release_mbids: Vec<String>,
    /// Its best copy's, as good as its worst file.
    pub quality: crate::quality::Quality,
}

/// Each album you have (by release group; `only`: just that one), with
/// its quality. With copies in more than one library (a lossy one for your
/// phone, say), the best counts, and is the one an upgrade replaces.
///
/// # Errors
///
/// On database errors.
pub fn album_qualities(
    conn: &Connection,
    only: Option<&str>,
) -> rusqlite::Result<Vec<AlbumQuality>> {
    use crate::quality::Quality;
    /// A copy: its folders and files' qualities.
    type Copy = (Vec<String>, Vec<Quality>);
    /// A release group, its releases, and each library's copy.
    type Album = (String, Vec<String>, Vec<(String, Copy)>);
    let mut stmt = conn.prepare(
        "SELECT release_group_mbid, library, dir, release_mbid, format, lossless, bit_depth,
                sample_rate, bitrate
         FROM files WHERE release_group_mbid IS NOT NULL
           AND (?1 IS NULL OR release_group_mbid = ?1)
         ORDER BY release_group_mbid, library, dir, path",
    )?;
    let rows = stmt.query_map([only], |r| {
        Ok((
            r.get::<_, String>(0)?,
            r.get::<_, String>(1)?,
            r.get::<_, String>(2)?,
            r.get::<_, Option<String>>(3)?,
            Quality {
                format: r.get::<_, Option<String>>(4)?.unwrap_or_default(),
                lossless: r.get(5)?,
                bit_depth: r.get(6)?,
                sample_rate: r.get(7)?,
                bitrate: r.get(8)?,
            },
        ))
    })?;
    let mut albums: Vec<Album> = Vec::new();
    for row in rows {
        let (group, library, dir, release, quality) = row?;
        if albums.last().is_none_or(|(g, _, _)| *g != group) {
            albums.push((group, Vec::new(), Vec::new()));
        }
        let (_, releases, copies) = albums.last_mut().expect("just pushed");
        if let Some(release) = release
            && !releases.contains(&release)
        {
            releases.push(release);
        }
        if copies.last().is_none_or(|(l, _)| *l != library) {
            copies.push((library, (Vec::new(), Vec::new())));
        }
        let (_, (dirs, files)) = copies.last_mut().expect("just pushed");
        if !dirs.contains(&dir) {
            dirs.push(dir);
        }
        files.push(quality);
    }
    Ok(albums
        .into_iter()
        .filter_map(|(group, release_mbids, copies)| {
            let (dirs, quality) = copies
                .into_iter()
                .filter_map(|(_, (dirs, files))| Some((dirs, Quality::worst(&files)?)))
                .max_by(|(_, a), (_, b)| a.cmp_rank(b))?;
            Some(AlbumQuality {
                release_group_mbid: group,
                dirs,
                release_mbids,
                quality,
            })
        })
        .collect())
}

// ---------- totals ----------

/// Untagged album folders: files with no release (group) id.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Untagged {
    pub library: String,
    pub dir: String,
    pub album: Option<String>,
    pub artist: Option<String>,
    /// The artist's page to link to: from the files' artist ids, else the
    /// one artist with `artist`'s name.
    pub artist_mbid: Option<String>,
    pub files: i64,
    /// You'd rather not see it.
    pub ignored: bool,
}

/// Folders with files that have no release or release group id.
///
/// # Errors
///
/// On database errors.
pub fn untagged(conn: &Connection) -> rusqlite::Result<Vec<Untagged>> {
    let mut stmt = conn.prepare(
        "SELECT library, dir, album, artist, files,
           coalesce(
             (SELECT min(fa.artist_mbid) FROM files f
              JOIN file_artists fa ON fa.path = f.path
              WHERE f.library = u.library AND f.dir = u.dir),
             (SELECT min(a.mbid) FROM artists a
              WHERE a.name = u.artist COLLATE NOCASE
              HAVING count(*) = 1)),
           EXISTS (SELECT 1 FROM ignored_dirs i
                   WHERE i.library = u.library AND i.dir = u.dir)
         FROM (SELECT library, dir, max(album) AS album, max(album_artist) AS artist,
                 count(*) AS files
               FROM files
               WHERE release_mbid IS NULL AND release_group_mbid IS NULL
               GROUP BY library, dir) u
         ORDER BY library, dir",
    )?;
    stmt.query_map([], |r| {
        Ok(Untagged {
            library: r.get(0)?,
            dir: r.get(1)?,
            album: r.get(2)?,
            artist: r.get(3)?,
            artist_mbid: r.get(5)?,
            files: r.get(4)?,
            ignored: r.get(6)?,
        })
    })?
    .collect()
}

/// Leave the untagged folder `dir` (of `library`) out of the Untagged
/// page's list and counts, or put it back.
///
/// # Errors
///
/// On database errors.
pub fn set_dir_ignored(
    conn: &Connection,
    library: &str,
    dir: &str,
    ignored: bool,
    now: Timestamp,
) -> rusqlite::Result<()> {
    if ignored {
        conn.execute(
            "INSERT INTO ignored_dirs (library, dir, ignored_at) VALUES (?1, ?2, ?3)
             ON CONFLICT DO NOTHING",
            params![library, dir, now.to_string()],
        )?;
    } else {
        conn.execute(
            "DELETE FROM ignored_dirs WHERE library = ?1 AND dir = ?2",
            params![library, dir],
        )?;
    }
    Ok(())
}

/// A library's totals.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct LibraryTotals {
    pub files: i64,
    pub albums: i64,
    pub artists: i64,
    pub untagged: i64,
}

/// Totals for each library.
///
/// # Errors
///
/// On database errors.
pub fn library_totals(conn: &Connection) -> rusqlite::Result<HashMap<String, LibraryTotals>> {
    let mut stmt = conn.prepare(
        "SELECT library, count(*), count(DISTINCT release_group_mbid),
           (SELECT count(DISTINCT fa.artist_mbid) FROM file_artists fa
              JOIN files f2 ON f2.path = fa.path WHERE f2.library = f.library),
           count(DISTINCT CASE WHEN release_mbid IS NULL AND release_group_mbid IS NULL
                                AND NOT EXISTS (SELECT 1 FROM ignored_dirs i
                                  WHERE i.library = f.library AND i.dir = f.dir)
                          THEN dir END)
         FROM files f GROUP BY library",
    )?;
    stmt.query_map([], |r| {
        Ok((
            r.get(0)?,
            LibraryTotals {
                files: r.get(1)?,
                albums: r.get(2)?,
                artists: r.get(3)?,
                untagged: r.get(4)?,
            },
        ))
    })?
    .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::State;

    fn file(
        path: &str,
        release: Option<&str>,
        group: Option<&str>,
        artists: &[&str],
    ) -> ScannedFile {
        ScannedFile {
            path: path.into(),
            library: "/music".into(),
            dir: path.rsplit_once('/').unwrap().0.into(),
            mtime: 1,
            size: 2,
            album: Some("Album".into()),
            album_artist: Some("Someone".into()),
            release_mbid: release.map(Into::into),
            release_group_mbid: group.map(Into::into),
            artist_mbids: artists.iter().map(|a| (*a).to_string()).collect(),
            quality: crate::quality::Quality::default(),
        }
    }

    #[test]
    fn an_album_is_as_good_as_its_worst_file() {
        use crate::quality::Quality;
        let state = State::open_in_memory().unwrap();
        let conn = &state.conn;
        let now = Timestamp::now();
        let mp3 = |kbps| Quality {
            format: "MP3".into(),
            bitrate: Some(kbps),
            ..Quality::default()
        };
        for (path, kbps) in [("/music/a/cd1/1.mp3", 320), ("/music/a/cd2/1.mp3", 192)] {
            let mut f = file(path, Some("r1"), Some("rg"), &["x"]);
            f.quality = mp3(kbps);
            upsert_file(conn, &f, now).unwrap();
        }
        upsert_file(
            conn,
            &file("/music/b/1.flac", None, Some("other"), &["x"]),
            now,
        )
        .unwrap();
        let albums = album_qualities(conn, Some("rg")).unwrap();
        assert_eq!(albums.len(), 1);
        assert_eq!(albums[0].quality, mp3(192));
        assert_eq!(albums[0].dirs, ["/music/a/cd1", "/music/a/cd2"]);
        assert_eq!(albums[0].release_mbids, ["r1"]);
        assert_eq!(album_qualities(conn, None).unwrap().len(), 2);
        // A better copy in another library: that's the one that counts.
        let mut f = file("/flac/a/1.flac", None, Some("rg"), &["x"]);
        f.library = "/flac".into();
        f.quality = Quality {
            format: "FLAC".into(),
            lossless: true,
            ..Quality::default()
        };
        upsert_file(conn, &f, now).unwrap();
        let albums = album_qualities(conn, Some("rg")).unwrap();
        assert!(albums[0].quality.lossless);
        assert_eq!(albums[0].dirs, ["/flac/a"]);
        remove_files(conn, &["/flac/a/1.flac".into()]).unwrap();
        assert_eq!(remove_files_under(conn, "/music/a/").unwrap(), 2);
        assert!(album_qualities(conn, Some("rg")).unwrap().is_empty());
    }

    #[test]
    fn files_bring_their_artists_and_releases_find_their_groups() {
        let state = State::open_in_memory().unwrap();
        let now = Timestamp::now();
        let conn = &state.conn;
        upsert_file(
            conn,
            &file(
                "/music/a/1.flac",
                Some("r1"),
                None,
                &["x", SPECIAL_ARTISTS[0]],
            ),
            now,
        )
        .unwrap();
        upsert_file(
            conn,
            &file("/music/a/2.flac", Some("r1"), None, &["x"]),
            now,
        )
        .unwrap();
        upsert_file(
            conn,
            &file("/music/b/1.flac", None, Some("g2"), &["x"]),
            now,
        )
        .unwrap();
        upsert_file(conn, &file("/music/c/1.flac", None, None, &[]), now).unwrap();

        assert_eq!(
            artists_to_resolve(conn).unwrap(),
            ["x"],
            "Various Artists isn't followed"
        );
        assert_eq!(releases_to_look_up(conn).unwrap(), ["r1"]);
        set_release_group(conn, "r1", Some("g1"), now).unwrap();
        assert!(releases_to_look_up(conn).unwrap().is_empty());
        // Scanned again: the release's group is known now.
        upsert_file(
            conn,
            &file("/music/a/1.flac", Some("r1"), None, &["x"]),
            now,
        )
        .unwrap();
        let groups: Vec<_> = owned_albums(conn, "x")
            .unwrap()
            .into_iter()
            .map(|a| a.release_group_mbid)
            .collect();
        assert_eq!(groups, [Some("g1".into()), Some("g2".into())]);
        let found = untagged(conn).unwrap();
        assert_eq!(found.len(), 1);
        let totals = &library_totals(conn).unwrap()["/music"];
        assert_eq!(
            (totals.files, totals.albums, totals.artists, totals.untagged),
            (4, 2, 1, 1)
        );
        set_dir_ignored(conn, "/music", &found[0].dir, true, now).unwrap();
        assert!(untagged(conn).unwrap()[0].ignored);
        assert_eq!(library_totals(conn).unwrap()["/music"].untagged, 0);
        set_dir_ignored(conn, "/music", &found[0].dir, false, now).unwrap();
        assert!(!untagged(conn).unwrap()[0].ignored);

        remove_files(conn, &["/music/c/1.flac".into()]).unwrap();
        assert!(untagged(conn).unwrap().is_empty());
        assert_eq!(remove_other_libraries(conn, &["/other".into()]).unwrap(), 3);
    }

    #[test]
    fn release_groups_keep_how_they_were_found() {
        let state = State::open_in_memory().unwrap();
        let conn = &state.conn;
        let first: Timestamp = "2026-01-01T00:00:00Z".parse().unwrap();
        let later: Timestamp = "2026-02-01T00:00:00Z".parse().unwrap();
        upsert_file(
            conn,
            &file("/music/a/1.flac", None, Some("g1"), &["x"]),
            first,
        )
        .unwrap();
        let group = NewReleaseGroup {
            mbid: "g1".into(),
            title: "One".into(),
            primary_type: Some("Album".into()),
            first_release_date: Some("2025-05-01".into()),
            artist_mbids: vec!["x".into()],
            ..NewReleaseGroup::default()
        };
        assert!(upsert_release_group(conn, &group, "listenbrainz", first).unwrap());
        let again = NewReleaseGroup {
            first_release_date: Some(String::new()),
            title: "One!".into(),
            ..group
        };
        assert!(!upsert_release_group(conn, &again, "musicbrainz", later).unwrap());
        let groups = release_groups(conn, Some("x")).unwrap();
        assert_eq!(groups.len(), 1);
        let g = &groups[0];
        assert_eq!(
            (g.title.as_str(), g.source.as_str()),
            ("One!", "listenbrainz")
        );
        assert_eq!(g.first_release_date.as_deref(), Some("2025-05-01"));
        assert_eq!(g.discovered_at, first);
        assert_eq!(g.owned_in, ["/music/a"]);
        assert_eq!(release_groups(conn, None).unwrap().len(), 1, "followed");
        set_followed(conn, "x", false).unwrap();
        assert!(release_groups(conn, None).unwrap().is_empty());
        set_ignored(conn, "g1", true, later).unwrap();
        assert!(release_groups(conn, Some("x")).unwrap()[0].ignored);
    }
}
