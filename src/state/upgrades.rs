//! Upgrades: what looking for a better copy of each album you have found,
//! and when to look again.

use std::collections::HashMap;

use jiff::Timestamp;
use rusqlite::{Connection, params};

/// An album's last look for an upgrade.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Check {
    pub rg_mbid: String,
    /// The album's quality then.
    pub quality: Option<String>,
    pub checked_at: Option<Timestamp>,
    /// Not looked for again before this.
    pub next_at: Option<Timestamp>,
    pub result: Option<String>,
    /// You asked: first in line.
    pub requested_at: Option<Timestamp>,
    /// Its last upgrade.
    pub upgraded: Option<Upgraded>,
}

/// An album's upgrade: from one quality to another.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Upgraded {
    pub from: String,
    pub to: String,
    pub at: Timestamp,
}

fn time(text: Option<String>) -> Option<Timestamp> {
    text.and_then(|t| t.parse().ok())
}

/// Every album's, by release group.
///
/// # Errors
///
/// On database errors.
pub fn all(conn: &Connection) -> rusqlite::Result<HashMap<String, Check>> {
    let mut stmt = conn.prepare(
        "SELECT rg_mbid, quality, checked_at, next_at, result, requested_at,
           upgraded_from, upgraded_to, upgraded_at
         FROM upgrades",
    )?;
    stmt.query_map([], |r| {
        Ok(Check {
            rg_mbid: r.get(0)?,
            quality: r.get(1)?,
            checked_at: time(r.get(2)?),
            next_at: time(r.get(3)?),
            result: r.get(4)?,
            requested_at: time(r.get(5)?),
            upgraded: match (r.get(6)?, r.get(7)?, time(r.get(8)?)) {
                (Some(from), Some(to), Some(at)) => Some(Upgraded { from, to, at }),
                _ => None,
            },
        })
    })?
    .map(|c| c.map(|c| (c.rg_mbid.clone(), c)))
    .collect()
}

/// Whether you asked for an upgrade not looked for yet.
///
/// # Errors
///
/// On database errors.
pub fn any_requested(conn: &Connection) -> rusqlite::Result<bool> {
    conn.query_row(
        "SELECT EXISTS (SELECT 1 FROM upgrades WHERE requested_at IS NOT NULL)",
        [],
        |r| r.get(0),
    )
}

/// Forget the requests for albums not in `keep` (nothing to upgrade:
/// they're good enough, or one's on its way).
///
/// # Errors
///
/// On database errors.
pub fn drop_requests<S: std::hash::BuildHasher>(
    conn: &Connection,
    keep: &std::collections::HashSet<&str, S>,
) -> rusqlite::Result<()> {
    let requested: Vec<String> = {
        let mut stmt =
            conn.prepare("SELECT rg_mbid FROM upgrades WHERE requested_at IS NOT NULL")?;
        stmt.query_map([], |r| r.get(0))?
            .collect::<rusqlite::Result<_>>()?
    };
    for rg in requested.iter().filter(|rg| !keep.contains(rg.as_str())) {
        conn.execute(
            "UPDATE upgrades SET requested_at = NULL WHERE rg_mbid = ?1",
            [rg],
        )?;
    }
    Ok(())
}

/// You asked for an upgrade of `rg`: it's looked for first, now.
///
/// # Errors
///
/// On database errors.
pub fn request(conn: &Connection, rg: &str, now: Timestamp) -> rusqlite::Result<()> {
    conn.execute(
        "INSERT INTO upgrades (rg_mbid, requested_at) VALUES (?1, ?2)
         ON CONFLICT (rg_mbid) DO UPDATE SET requested_at = ?2, next_at = NULL",
        params![rg, now.to_string()],
    )?;
    Ok(())
}

/// `rg` was looked for: at `quality`, it found `result`; not again before
/// `next_at`.
///
/// # Errors
///
/// On database errors.
pub fn checked(
    conn: &Connection,
    rg: &str,
    quality: &str,
    result: &str,
    next_at: Timestamp,
    now: Timestamp,
) -> rusqlite::Result<()> {
    conn.execute(
        "INSERT INTO upgrades (rg_mbid, quality, checked_at, next_at, result)
         VALUES (?1, ?2, ?3, ?4, ?5)
         ON CONFLICT (rg_mbid) DO UPDATE SET quality = ?2, checked_at = ?3, next_at = ?4,
           result = ?5, requested_at = NULL",
        params![rg, quality, now.to_string(), next_at.to_string(), result],
    )?;
    Ok(())
}

/// `rg` was upgraded `from` one quality `to` another.
///
/// # Errors
///
/// On database errors.
pub fn upgraded(
    conn: &Connection,
    rg: &str,
    from: &str,
    to: &str,
    now: Timestamp,
) -> rusqlite::Result<()> {
    conn.execute(
        "INSERT INTO upgrades (rg_mbid, upgraded_from, upgraded_to, upgraded_at)
         VALUES (?1, ?2, ?3, ?4)
         ON CONFLICT (rg_mbid) DO UPDATE SET upgraded_from = ?2, upgraded_to = ?3,
           upgraded_at = ?4",
        params![rg, from, to, now.to_string()],
    )?;
    Ok(())
}

/// How many albums were upgraded.
///
/// # Errors
///
/// On database errors.
pub fn upgraded_count(conn: &Connection) -> rusqlite::Result<i64> {
    conn.query_row(
        "SELECT count(*) FROM upgrades WHERE upgraded_at IS NOT NULL",
        [],
        |r| r.get(0),
    )
}

/// Look for `rg` again at the next run (its upgrade failed: another
/// source may do).
///
/// # Errors
///
/// On database errors.
pub fn again(conn: &Connection, rg: &str, result: &str) -> rusqlite::Result<()> {
    conn.execute(
        "UPDATE upgrades SET next_at = NULL, result = ?2 WHERE rg_mbid = ?1",
        params![rg, result],
    )?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::State;

    #[test]
    fn remembers_checks_and_requests() {
        let state = State::open_in_memory().unwrap();
        let conn = &state.conn;
        let now: Timestamp = "2026-11-19T00:00:00Z".parse().unwrap();
        let later: Timestamp = "2026-12-03T00:00:00Z".parse().unwrap();
        checked(conn, "rg", "MP3 192 kbps", "nothing better", later, now).unwrap();
        let check = all(conn).unwrap().remove("rg").unwrap();
        assert_eq!(check.next_at, Some(later));
        assert_eq!(check.quality.as_deref(), Some("MP3 192 kbps"));
        assert!(!any_requested(conn).unwrap());
        request(conn, "rg", now).unwrap();
        assert!(any_requested(conn).unwrap());
        let check = all(conn).unwrap().remove("rg").unwrap();
        assert_eq!((check.next_at, check.requested_at), (None, Some(now)));
        checked(conn, "rg", "MP3 192 kbps", "queued", later, now).unwrap();
        again(conn, "rg", "failed").unwrap();
        let check = all(conn).unwrap().remove("rg").unwrap();
        assert_eq!((check.next_at, check.requested_at), (None, None));
        assert_eq!(check.result.as_deref(), Some("failed"));
        assert_eq!(check.upgraded, None);
        assert_eq!(upgraded_count(conn).unwrap(), 0);
        upgraded(conn, "rg", "MP3 192 kbps", "FLAC 16-bit/44.1 kHz", later).unwrap();
        let check = all(conn).unwrap().remove("rg").unwrap();
        assert_eq!(
            check.upgraded,
            Some(Upgraded {
                from: "MP3 192 kbps".into(),
                to: "FLAC 16-bit/44.1 kHz".into(),
                at: later,
            })
        );
        assert_eq!(upgraded_count(conn).unwrap(), 1);
    }

    #[test]
    fn earlier_upgrades_are_remembered_as_upgrades() {
        let state = State::open_in_memory().unwrap();
        let conn = &state.conn;
        let now: Timestamp = "2026-11-19T00:00:00Z".parse().unwrap();
        // As the migration finds them: only the check's result says so.
        checked(conn, "rg", "FLAC", "upgraded from MP3 192 kbps", now, now).unwrap();
        conn.execute(
            "UPDATE upgrades SET upgraded_from = NULL, upgraded_to = NULL, upgraded_at = NULL",
            [],
        )
        .unwrap();
        conn.execute_batch(
            include_str!("upgraded.sql")
                .split_once("UPDATE")
                .map(|(_, u)| format!("UPDATE{u}"))
                .unwrap()
                .as_str(),
        )
        .unwrap();
        let check = all(conn).unwrap().remove("rg").unwrap();
        assert_eq!(
            check.upgraded,
            Some(Upgraded {
                from: "MP3 192 kbps".into(),
                to: "FLAC".into(),
                at: now,
            })
        );
    }
}
