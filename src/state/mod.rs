//! The SQLite state store.

use std::fs::OpenOptions;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::Path;

use rusqlite::Connection;
use rusqlite_migration::{M, Migrations};

pub mod daemon;
pub mod downloads;
pub mod library;
pub mod requests;
pub mod upgrades;

const MIGRATION_LIST: &[M<'static>] = &[
    M::up(include_str!("schema.sql")),
    M::up(include_str!("deezer_ignored.sql")),
    M::up(include_str!("joined_artists.sql")),
    M::up(include_str!("downloads.sql")),
    M::up(include_str!("download_matches.sql")),
    M::up(include_str!("download_import.sql")),
    M::up(include_str!("no_deezer.sql")),
    M::up(include_str!("upgrades.sql")),
    M::up(include_str!("untagged_ignored.sql")),
    M::up(include_str!("upgraded.sql")),
    M::up(include_str!("requests.sql")),
];
const MIGRATIONS: Migrations<'static> = Migrations::from_slice(MIGRATION_LIST);

#[derive(Debug, thiserror::Error)]
pub enum StateError {
    #[error("state database: {0}")]
    Sqlite(#[from] rusqlite::Error),
    #[error("state database migration: {0}")]
    Migration(#[from] rusqlite_migration::Error),
    #[error("cannot create {path}: {source}")]
    Io {
        path: String,
        source: std::io::Error,
    },
}

/// An open state database.
#[derive(Debug)]
pub struct State {
    pub conn: Connection,
}

impl State {
    /// Open (creating if needed) the DB at `path`. The file is owner-only
    /// (0600); WAL mode, migrations applied.
    ///
    /// # Errors
    ///
    /// [`StateError::Io`] if the file or its directory can't be created or
    /// restricted, or a DB error if it can't be opened or migrated.
    pub fn open(path: &Path) -> Result<Self, StateError> {
        let io = |source| StateError::Io {
            path: path.display().to_string(),
            source,
        };
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir).map_err(io)?;
            std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700)).map_err(io)?;
        }
        OpenOptions::new()
            .create(true)
            .append(true)
            .mode(0o600)
            .open(path)
            .map_err(io)?;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600)).map_err(io)?;
        let conn = Connection::open(path)?;
        conn.pragma_update(None, "journal_mode", "WAL")?;
        Self::init(conn)
    }

    /// An in-memory DB, for tests.
    ///
    /// # Errors
    ///
    /// If it can't be opened or migrated.
    pub fn open_in_memory() -> Result<Self, StateError> {
        Self::init(Connection::open_in_memory()?)
    }

    /// A write transaction. It takes the write lock up front, so a busy DB
    /// (another service or process writing) is waited for (`busy_timeout`)
    /// instead of failing when a read inside it turns into a write.
    ///
    /// # Errors
    ///
    /// If the write lock can't be taken (still busy after the timeout).
    pub fn write_transaction(conn: &mut Connection) -> rusqlite::Result<rusqlite::Transaction<'_>> {
        conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
    }

    fn init(mut conn: Connection) -> Result<Self, StateError> {
        conn.pragma_update(None, "foreign_keys", "ON")?;
        conn.busy_timeout(std::time::Duration::from_secs(5))?;
        MIGRATIONS.to_latest(&mut conn)?;
        Ok(Self { conn })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn migrations_are_valid() {
        assert!(MIGRATIONS.validate().is_ok());
    }

    #[test]
    fn concurrent_writers_wait_for_each_other() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.db");
        State::open(&path).unwrap();
        let writers: Vec<_> = (0..4)
            .map(|_| {
                let path = path.clone();
                std::thread::spawn(move || {
                    let mut state = State::open(&path).unwrap();
                    for _ in 0..50 {
                        let tx = State::write_transaction(&mut state.conn).unwrap();
                        // Read, then write: the pattern that fails without
                        // the write lock taken up front.
                        let n: i64 = tx
                            .query_row("SELECT count(*) FROM ignored", [], |r| r.get(0))
                            .unwrap();
                        tx.execute(
                            "INSERT INTO ignored (rg_mbid, created_at) VALUES (?1, 'now')",
                            [n.to_string()],
                        )
                        .unwrap();
                        tx.commit().unwrap();
                    }
                })
            })
            .collect();
        for writer in writers {
            writer.join().expect("a writer failed");
        }
        let state = State::open(&path).unwrap();
        let n: i64 = state
            .conn
            .query_row("SELECT count(*) FROM ignored", [], |r| r.get(0))
            .unwrap();
        assert_eq!(n, 200);
    }

    #[test]
    fn creates_owner_only_db_with_all_tables() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("lib/state.db");
        let state = State::open(&path).unwrap();
        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
        let mut stmt = state
            .conn
            .prepare("SELECT name FROM sqlite_master WHERE type = 'table' ORDER BY name")
            .unwrap();
        let tables: Vec<String> = stmt
            .query_map([], |r| r.get(0))
            .unwrap()
            .map(Result::unwrap)
            .collect();
        drop(stmt);
        for t in [
            "config_revisions",
            "files",
            "file_artists",
            "artists",
            "release_groups",
            "release_group_artists",
            "releases",
            "ignored",
            "daemon_services",
            "daemon_requests",
        ] {
            assert!(tables.iter().any(|x| x == t), "missing {t}: {tables:?}");
        }
        assert!(!tables.iter().any(|x| x == "deezer_albums"), "{tables:?}");
        let mode: String = state
            .conn
            .pragma_query_value(None, "journal_mode", |r| r.get(0))
            .unwrap();
        assert_eq!(mode, "wal");
        drop(state);
        State::open(&path).expect("reopening is idempotent");
    }
}
