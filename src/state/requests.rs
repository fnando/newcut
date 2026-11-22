//! Requests: albums you asked for (out or still to come), and what looking
//! for them on Soulseek found.

use std::collections::HashMap;

use jiff::Timestamp;
use rusqlite::{Connection, params};

/// An album you asked for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Request {
    pub rg_mbid: String,
    pub requested_at: Timestamp,
    pub checked_at: Option<Timestamp>,
    /// Not looked for again before this (`None`: at the next run).
    pub next_at: Option<Timestamp>,
    pub result: Option<String>,
}

impl Request {
    /// Whether it's time to look for it.
    #[must_use]
    pub fn due(&self, now: Timestamp) -> bool {
        self.next_at.is_none_or(|n| n <= now)
    }
}

fn time(text: Option<String>) -> Option<Timestamp> {
    text.and_then(|t| t.parse().ok())
}

/// Every request, by release group.
///
/// # Errors
///
/// On database errors.
pub fn all(conn: &Connection) -> rusqlite::Result<HashMap<String, Request>> {
    let mut stmt =
        conn.prepare("SELECT rg_mbid, requested_at, checked_at, next_at, result FROM requests")?;
    stmt.query_map([], |r| {
        Ok(Request {
            rg_mbid: r.get(0)?,
            requested_at: time(r.get(1)?).unwrap_or_default(),
            checked_at: time(r.get(2)?),
            next_at: time(r.get(3)?),
            result: r.get(4)?,
        })
    })?
    .map(|c| c.map(|c| (c.rg_mbid.clone(), c)))
    .collect()
}

/// Ask for `rg`: looked for at the next run (now, again, when asked before).
///
/// # Errors
///
/// On database errors.
pub fn add(conn: &Connection, rg: &str, now: Timestamp) -> rusqlite::Result<()> {
    conn.execute(
        "INSERT INTO requests (rg_mbid, requested_at) VALUES (?1, ?2)
         ON CONFLICT (rg_mbid) DO UPDATE SET next_at = NULL",
        params![rg, now.to_string()],
    )?;
    Ok(())
}

/// Take the request for `rg` back.
///
/// # Errors
///
/// On database errors.
pub fn remove(conn: &Connection, rg: &str) -> rusqlite::Result<()> {
    conn.execute("DELETE FROM requests WHERE rg_mbid = ?1", [rg])?;
    Ok(())
}

/// Forget the requests for albums you have now. Returns how many.
///
/// # Errors
///
/// On database errors.
pub fn drop_owned(conn: &Connection) -> rusqlite::Result<usize> {
    conn.execute(
        "DELETE FROM requests WHERE rg_mbid IN (
           SELECT release_group_mbid FROM files WHERE release_group_mbid IS NOT NULL)",
        [],
    )
}

/// `rg` was looked for and `result` found; not again before `next_at`.
///
/// # Errors
///
/// On database errors.
pub fn checked(
    conn: &Connection,
    rg: &str,
    result: &str,
    next_at: Timestamp,
    now: Timestamp,
) -> rusqlite::Result<()> {
    conn.execute(
        "UPDATE requests SET checked_at = ?2, next_at = ?3, result = ?4 WHERE rg_mbid = ?1",
        params![rg, now.to_string(), next_at.to_string(), result],
    )?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::State;

    #[test]
    fn remembers_requests_and_checks() {
        let state = State::open_in_memory().unwrap();
        let conn = &state.conn;
        let now: Timestamp = "2026-11-22T00:00:00Z".parse().unwrap();
        let later: Timestamp = "2026-11-22T06:00:00Z".parse().unwrap();
        add(conn, "rg", now).unwrap();
        let request = all(conn).unwrap().remove("rg").unwrap();
        assert_eq!((request.requested_at, request.next_at), (now, None));
        assert!(request.due(now));
        checked(conn, "rg", "nothing yet", later, now).unwrap();
        let request = all(conn).unwrap().remove("rg").unwrap();
        assert_eq!(request.result.as_deref(), Some("nothing yet"));
        assert!(!request.due(now));
        assert!(request.due(later));
        // Asked again: due now, still asked for since the first time.
        add(conn, "rg", later).unwrap();
        let request = all(conn).unwrap().remove("rg").unwrap();
        assert_eq!((request.requested_at, request.next_at), (now, None));
        remove(conn, "rg").unwrap();
        assert!(all(conn).unwrap().is_empty());
    }
}
