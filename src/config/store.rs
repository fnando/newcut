//! Config revisions. The daemon runs with the latest revision; a changed
//! config file becomes a new one once it checks out (the daemon looks every
//! few seconds), and so do saves and rollbacks from the web UI. A broken
//! edit never stops a running daemon.

use std::path::{Path, PathBuf};

use jiff::Timestamp;
use rusqlite::{Connection, OptionalExtension, params};

use super::{Loaded, check_text};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Revision {
    pub rev: i64,
    pub text: String,
    /// "file", "web", "rollback to 3"
    pub origin: String,
    pub created_at: String,
}

fn row(r: &rusqlite::Row<'_>) -> rusqlite::Result<Revision> {
    Ok(Revision {
        rev: r.get(0)?,
        text: r.get(1)?,
        origin: r.get(2)?,
        created_at: r.get(3)?,
    })
}

/// The latest revision.
///
/// # Errors
///
/// On database errors.
pub fn current(conn: &Connection) -> rusqlite::Result<Option<Revision>> {
    conn.query_row(
        "SELECT rev, text, origin, created_at FROM config_revisions ORDER BY rev DESC LIMIT 1",
        [],
        row,
    )
    .optional()
}

/// Revision `rev`.
///
/// # Errors
///
/// On database errors.
pub fn get(conn: &Connection, rev: i64) -> rusqlite::Result<Option<Revision>> {
    conn.query_row(
        "SELECT rev, text, origin, created_at FROM config_revisions WHERE rev = ?1",
        [rev],
        row,
    )
    .optional()
}

/// The latest `limit` revisions, newest first.
///
/// # Errors
///
/// On database errors.
pub fn list(conn: &Connection, limit: i64) -> rusqlite::Result<Vec<Revision>> {
    let mut stmt = conn.prepare(
        "SELECT rev, text, origin, created_at FROM config_revisions ORDER BY rev DESC LIMIT ?1",
    )?;
    stmt.query_map([limit], row)?.collect()
}

/// Store `text` as a new revision, unless it's the current one already.
/// Returns the revision number and whether it's new.
///
/// # Errors
///
/// On database errors.
pub fn add(
    conn: &Connection,
    text: &str,
    origin: &str,
    now: Timestamp,
) -> rusqlite::Result<(i64, bool)> {
    if let Some(current) = current(conn)?
        && current.text == text
    {
        return Ok((current.rev, false));
    }
    conn.execute(
        "INSERT INTO config_revisions (text, origin, created_at) VALUES (?1, ?2, ?3)",
        params![text, origin, now.to_string()],
    )?;
    Ok((conn.last_insert_rowid(), true))
}

/// What happened when the config file was looked at.
#[derive(Debug, PartialEq, Eq)]
pub enum FileCheck {
    /// Same as the current revision.
    Same(i64),
    /// Stored as a new revision.
    New(i64),
    /// Changed, but not valid: the current revision stays.
    Invalid(Vec<String>),
}

/// Store the config file as a new revision when it changed and checks out.
///
/// # Errors
///
/// When the file can't be read, or on database errors.
pub fn import_file(conn: &Connection, path: &Path, home: &Path) -> anyhow::Result<FileCheck> {
    let text = std::fs::read_to_string(path)
        .map_err(|e| anyhow::anyhow!("cannot read {}: {e}", path.display()))?;
    if let Some(current) = current(conn)?
        && current.text == text
    {
        return Ok(FileCheck::Same(current.rev));
    }
    if let Err(errors) = check_text(&text, &path.display().to_string(), home) {
        return Ok(FileCheck::Invalid(errors));
    }
    let (rev, _) = add(conn, &text, "file", Timestamp::now())?;
    Ok(FileCheck::New(rev))
}

/// The config to run with: the latest revision.
///
/// # Errors
///
/// When there's no revision yet, or it no longer checks out (a library
/// path's variable went away).
pub fn load_current(conn: &Connection, home: &Path) -> anyhow::Result<(i64, Loaded)> {
    let Some(revision) = current(conn)? else {
        anyhow::bail!("no config imported yet; run `newcut config import` or start the daemon");
    };
    let loaded = check_text(&revision.text, &format!("revision {}", revision.rev), home)
        .map_err(|errors| anyhow::anyhow!(errors.join("\n")))?;
    Ok((revision.rev, loaded))
}

/// Write the config file safely: a backup of the old one (`.bak`), then
/// the new text in place at once, keeping the file's permissions (a new
/// one is private: it may hold a token). Returns the backup's path.
///
/// # Errors
///
/// When writing fails.
pub fn write_file(path: &Path, text: &str) -> std::io::Result<Option<PathBuf>> {
    let with = |suffix: &str| {
        let mut name = path.as_os_str().to_owned();
        name.push(suffix);
        PathBuf::from(name)
    };
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let permissions = std::fs::metadata(path).map(|m| m.permissions()).ok();
    let backup = if path.exists() {
        let backup = with(".bak");
        std::fs::copy(path, &backup)?;
        Some(backup)
    } else {
        None
    };
    let staged = with(".newcut-partial");
    std::fs::write(&staged, text)?;
    let permissions = permissions.unwrap_or_else(|| {
        use std::os::unix::fs::PermissionsExt;
        std::fs::Permissions::from_mode(0o600)
    });
    std::fs::set_permissions(&staged, permissions)?;
    std::fs::rename(&staged, path)?;
    Ok(backup)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::State;

    #[test]
    fn the_file_becomes_a_revision_when_it_changes_and_checks_out() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        let state = State::open_in_memory().unwrap();
        let home = dir.path();

        std::fs::write(&path, "version = 1\n").unwrap();
        assert_eq!(
            import_file(&state.conn, &path, home).unwrap(),
            FileCheck::New(1)
        );
        assert_eq!(
            import_file(&state.conn, &path, home).unwrap(),
            FileCheck::Same(1)
        );
        std::fs::write(&path, "version = 1\nnope = 1\n").unwrap();
        assert!(matches!(
            import_file(&state.conn, &path, home).unwrap(),
            FileCheck::Invalid(_)
        ));
        assert_eq!(load_current(&state.conn, home).unwrap().0, 1);
        std::fs::write(&path, "version = 1\n[server]\nenabled = false\n").unwrap();
        assert_eq!(
            import_file(&state.conn, &path, home).unwrap(),
            FileCheck::New(2)
        );
        let (_, loaded) = load_current(&state.conn, home).unwrap();
        assert!(!loaded.config.server.enabled);
        assert_eq!(list(&state.conn, 10).unwrap().len(), 2);
    }
}
