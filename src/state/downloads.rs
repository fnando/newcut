//! Albums queued in slskd: the folder chosen for each, so its transfers can
//! be followed.

use std::collections::HashSet;

use jiff::Timestamp;
use rusqlite::{Connection, params};

/// A folder queued for an album.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Download {
    pub id: i64,
    /// Its release group's MBID.
    pub release_key: String,
    pub username: String,
    pub dir: String,
    /// Full names (on the user's side) and sizes.
    pub files: Vec<(String, u64)>,
    pub matched: Matched,
    /// The folder slskd puts it in, under its downloads folder (none when
    /// queued before newcut chose).
    pub destination: Option<String>,
    /// downloading, failed, manual, filed or handled.
    pub state: String,
    /// Why it's failed or manual.
    pub detail: Option<String>,
    /// Where its files went.
    pub filed_to: Option<String>,
    pub queued_at: Timestamp,
}

/// Still downloading, or slskd couldn't get every file (it may yet).
pub const DOWNLOADING: &str = "downloading";
pub const FAILED: &str = "failed";
/// Downloaded, but not filed: for you to look at.
pub const MANUAL: &str = "manual";
/// Tagged and moved into the library.
pub const FILED: &str = "filed";
/// You dealt with it.
pub const HANDLED: &str = "handled";

/// The release a queued folder matched.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Matched {
    pub kind: String,
    pub release_mbid: Option<String>,
    /// What the release is: "Digital Media · 2026-11-06".
    pub release_label: Option<String>,
    /// Why it isn't exact.
    pub reason: Option<String>,
    /// Each file's track MBID on the release, in `files` order.
    pub tracks: Vec<Option<String>>,
}

/// A folder to remember as queued.
pub struct NewDownload<'a> {
    pub release_key: &'a str,
    pub username: &'a str,
    pub dir: &'a str,
    pub files: &'a [(String, u64)],
    pub matched: &'a Matched,
    pub destination: Option<&'a str>,
}

/// Remember a queued folder; returns its id.
///
/// # Errors
///
/// On database errors.
pub fn add(conn: &Connection, new: &NewDownload, now: Timestamp) -> rusqlite::Result<i64> {
    let files = serde_json::to_string(new.files).expect("file names serialize");
    let tracks = serde_json::to_string(&new.matched.tracks).expect("ids serialize");
    conn.execute(
        "INSERT INTO downloads (release_key, username, dir, files, queued_at,
             release_mbid, release_label, match_kind, match_reason, tracks, destination,
             state, updated_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, 'downloading', ?5)",
        params![
            new.release_key,
            new.username,
            new.dir,
            files,
            now.to_string(),
            new.matched.release_mbid,
            new.matched.release_label,
            new.matched.kind,
            new.matched.reason,
            tracks,
            new.destination,
        ],
    )?;
    Ok(conn.last_insert_rowid())
}

const COLUMNS: &str = "id, release_key, username, dir, files, queued_at, release_mbid,
    release_label, match_kind, match_reason, tracks, destination, state, state_detail, filed_to";

fn from_row(row: &rusqlite::Row) -> rusqlite::Result<Download> {
    let files: String = row.get(4)?;
    let queued_at: String = row.get(5)?;
    Ok(Download {
        id: row.get(0)?,
        release_key: row.get(1)?,
        username: row.get(2)?,
        dir: row.get(3)?,
        files: serde_json::from_str(&files).unwrap_or_default(),
        queued_at: queued_at.parse().unwrap_or_default(),
        matched: Matched {
            release_mbid: row.get(6)?,
            release_label: row.get(7)?,
            kind: row.get(8)?,
            reason: row.get(9)?,
            tracks: serde_json::from_str(&row.get::<_, String>(10)?).unwrap_or_default(),
        },
        destination: row.get(11)?,
        state: row.get(12)?,
        detail: row.get(13)?,
        filed_to: row.get(14)?,
    })
}

fn select(
    conn: &Connection,
    filter: &str,
    args: impl rusqlite::Params,
) -> rusqlite::Result<Vec<Download>> {
    let mut stmt = conn.prepare(&format!(
        "SELECT {COLUMNS} FROM downloads {filter} ORDER BY queued_at DESC, id DESC"
    ))?;
    stmt.query_map(args, from_row)?.collect()
}

/// The folders queued for `release_key`, latest first.
///
/// # Errors
///
/// On database errors.
pub fn for_release(conn: &Connection, release_key: &str) -> rusqlite::Result<Vec<Download>> {
    select(conn, "WHERE release_key = ?1", [release_key])
}

/// Every download, latest first.
///
/// # Errors
///
/// On database errors.
pub fn all(conn: &Connection) -> rusqlite::Result<Vec<Download>> {
    select(conn, "", [])
}

/// Download `id`.
///
/// # Errors
///
/// On database errors.
pub fn get(conn: &Connection, id: i64) -> rusqlite::Result<Option<Download>> {
    Ok(select(conn, "WHERE id = ?1", [id])?.into_iter().next())
}

/// What the importer follows: downloading, or failed (slskd may retry).
///
/// # Errors
///
/// On database errors.
pub fn pending(conn: &Connection) -> rusqlite::Result<Vec<Download>> {
    select(conn, "WHERE state IN ('downloading', 'failed')", [])
}

/// How many are waiting for you.
///
/// # Errors
///
/// On database errors.
pub fn manual_count(conn: &Connection) -> rusqlite::Result<i64> {
    conn.query_row(
        "SELECT COUNT(*) FROM downloads WHERE state = 'manual'",
        [],
        |row| row.get(0),
    )
}

/// Set download `id`'s state; returns whether it changed.
///
/// # Errors
///
/// On database errors.
pub fn set_state(
    conn: &Connection,
    id: i64,
    state: &str,
    detail: Option<&str>,
    filed_to: Option<&str>,
    now: Timestamp,
) -> rusqlite::Result<bool> {
    Ok(conn.execute(
        "UPDATE downloads SET state = ?2, state_detail = ?3, filed_to = ?4, updated_at = ?5
         WHERE id = ?1 AND (state IS NOT ?2 OR state_detail IS NOT ?3 OR filed_to IS NOT ?4)",
        params![id, state, detail, filed_to, now.to_string()],
    )? > 0)
}

/// Download `id` is `matched` after all (you said which release it is).
///
/// # Errors
///
/// On database errors.
pub fn set_matched(conn: &Connection, id: i64, matched: &Matched) -> rusqlite::Result<()> {
    let tracks = serde_json::to_string(&matched.tracks).expect("ids serialize");
    conn.execute(
        "UPDATE downloads SET release_mbid = ?2, release_label = ?3, match_kind = ?4,
             match_reason = ?5, tracks = ?6 WHERE id = ?1",
        params![
            id,
            matched.release_mbid,
            matched.release_label,
            matched.kind,
            matched.reason,
            tracks
        ],
    )?;
    Ok(())
}

/// The releases with something queued.
///
/// # Errors
///
/// On database errors.
pub fn queued_releases(conn: &Connection) -> rusqlite::Result<HashSet<String>> {
    let mut stmt = conn.prepare("SELECT DISTINCT release_key FROM downloads")?;
    stmt.query_map([], |row| row.get(0))?.collect()
}

/// Each release's latest download's state (what lists show).
///
/// # Errors
///
/// On database errors.
pub fn release_states(
    conn: &Connection,
) -> rusqlite::Result<std::collections::HashMap<String, String>> {
    let mut stmt = conn.prepare(
        "SELECT release_key, state FROM downloads d
         WHERE id = (SELECT id FROM downloads WHERE release_key = d.release_key
                     ORDER BY queued_at DESC, id DESC LIMIT 1)",
    )?;
    stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?
        .collect()
}

/// Forget download `id` of `release_key` (slskd isn't told).
///
/// # Errors
///
/// On database errors.
pub fn remove(conn: &Connection, release_key: &str, id: i64) -> rusqlite::Result<bool> {
    Ok(conn.execute(
        "DELETE FROM downloads WHERE id = ?1 AND release_key = ?2",
        params![id, release_key],
    )? > 0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::State;

    #[test]
    fn remembers_queued_folders_per_release() {
        let state = State::open_in_memory().unwrap();
        let conn = &state.conn;
        let at = |s: &str| s.parse::<Timestamp>().unwrap();
        let files = vec![("a\\b\\01.flac".to_string(), 10)];
        let matched = Matched {
            kind: "exact".into(),
            release_mbid: Some("rel".into()),
            release_label: Some("CD · 2021".into()),
            reason: None,
            tracks: vec![Some("t1".into())],
        };
        let none = Matched::default();
        let new = |key, user, dir, files, matched| NewDownload {
            release_key: key,
            username: user,
            dir,
            files,
            matched,
            destination: Some("newcut/x"),
        };
        let first = add(
            conn,
            &new("rg", "one", "a\\b", &files, &matched),
            at("2026-11-01T00:00:00Z"),
        )
        .unwrap();
        let second = add(
            conn,
            &new("rg", "two", "c\\d", &[], &none),
            at("2026-11-02T00:00:00Z"),
        )
        .unwrap();
        add(
            conn,
            &new("other", "one", "x", &[], &none),
            at("2026-11-03T00:00:00Z"),
        )
        .unwrap();
        let found = for_release(conn, "rg").unwrap();
        assert_eq!(
            found.iter().map(|d| d.id).collect::<Vec<_>>(),
            [second, first]
        );
        assert_eq!(found[1].files, files);
        assert_eq!(found[1].matched, matched);
        assert_eq!(found[1].username, "one");
        assert_eq!(found[1].state, DOWNLOADING);
        assert_eq!(pending(conn).unwrap().len(), 3);

        let now = at("2026-11-04T00:00:00Z");
        assert!(set_state(conn, first, MANUAL, Some("why"), None, now).unwrap());
        assert!(
            !set_state(conn, first, MANUAL, Some("why"), None, now).unwrap(),
            "no change"
        );
        assert_eq!(manual_count(conn).unwrap(), 1);
        assert_eq!(pending(conn).unwrap().len(), 2);
        let got = get(conn, first).unwrap().unwrap();
        assert_eq!(
            (got.state.as_str(), got.detail.as_deref()),
            (MANUAL, Some("why"))
        );
        set_matched(conn, second, &matched).unwrap();
        assert_eq!(get(conn, second).unwrap().unwrap().matched, matched);
        assert_eq!(
            queued_releases(conn).unwrap(),
            HashSet::from(["rg".to_string(), "other".to_string()])
        );
        assert!(!remove(conn, "other", first).unwrap(), "only the release's");
        assert!(remove(conn, "rg", first).unwrap());
        assert_eq!(for_release(conn, "rg").unwrap().len(), 1);
    }
}
