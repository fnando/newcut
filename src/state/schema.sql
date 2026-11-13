-- Config revisions: the daemon runs with the latest.
CREATE TABLE config_revisions (
  rev INTEGER PRIMARY KEY,
  text TEXT NOT NULL,
  origin TEXT NOT NULL,          -- file, web, rollback to N
  created_at TEXT NOT NULL
);

-- Every audio file found in the libraries, with its MusicBrainz tags.
CREATE TABLE files (
  path TEXT PRIMARY KEY,
  library TEXT NOT NULL,
  dir TEXT NOT NULL,             -- the album's folder
  mtime INTEGER NOT NULL,
  size INTEGER NOT NULL,
  album TEXT,
  album_artist TEXT,
  release_mbid TEXT,
  release_group_mbid TEXT,       -- from the tags, or looked up from the release
  scanned_at TEXT NOT NULL
);
CREATE INDEX files_release_group ON files (release_group_mbid);
CREATE INDEX files_release ON files (release_mbid);
CREATE INDEX files_library_dir ON files (library, dir);

-- A file's album artists (MUSICBRAINZ_ALBUMARTISTID, else the track's).
CREATE TABLE file_artists (
  path TEXT NOT NULL REFERENCES files (path) ON DELETE CASCADE,
  artist_mbid TEXT NOT NULL,
  PRIMARY KEY (path, artist_mbid)
);
CREATE INDEX file_artists_artist ON file_artists (artist_mbid);

CREATE TABLE artists (
  mbid TEXT PRIMARY KEY,
  name TEXT,                     -- from the tags until MusicBrainz says
  sort_name TEXT,
  deezer_id INTEGER,
  followed INTEGER NOT NULL DEFAULT 1,
  resolved_at TEXT,              -- name and Deezer id looked up
  baseline_at TEXT,              -- first discography check: nothing before is new
  last_checked_at TEXT,
  next_check_at TEXT,
  deezer_baseline_at TEXT,
  deezer_checked_at TEXT,
  deezer_next_at TEXT,
  last_error TEXT,
  created_at TEXT NOT NULL
);

CREATE TABLE release_groups (
  mbid TEXT PRIMARY KEY,
  title TEXT NOT NULL,
  artist_credit TEXT,
  primary_type TEXT,
  secondary_types TEXT NOT NULL DEFAULT '[]',   -- JSON
  first_release_date TEXT,       -- YYYY, YYYY-MM or YYYY-MM-DD
  source TEXT NOT NULL,          -- musicbrainz, listenbrainz
  discovered_at TEXT NOT NULL
);

CREATE TABLE release_group_artists (
  rg_mbid TEXT NOT NULL REFERENCES release_groups (mbid) ON DELETE CASCADE,
  artist_mbid TEXT NOT NULL,
  PRIMARY KEY (rg_mbid, artist_mbid)
);
CREATE INDEX release_group_artists_artist ON release_group_artists (artist_mbid);

-- Release → release group, for files tagged with the release only.
CREATE TABLE releases (
  mbid TEXT PRIMARY KEY,
  release_group_mbid TEXT,       -- NULL: MusicBrainz doesn't know it
  looked_up_at TEXT NOT NULL
);

-- Release groups you don't want: they leave Missing and the feed.
CREATE TABLE ignored (
  rg_mbid TEXT PRIMARY KEY,
  created_at TEXT NOT NULL
);

CREATE TABLE deezer_albums (
  id INTEGER PRIMARY KEY,
  artist_mbid TEXT NOT NULL,
  title TEXT NOT NULL,
  release_date TEXT,
  record_type TEXT,              -- album, ep, single, compile
  link TEXT,
  cover TEXT,
  matched_rg_mbid TEXT,          -- the release group it is, once MusicBrainz has it
  discovered_at TEXT NOT NULL
);
CREATE INDEX deezer_albums_artist ON deezer_albums (artist_mbid);

-- The daemon: its schedule, its services, and requests from the CLI.
CREATE TABLE daemon_state (key TEXT PRIMARY KEY, value TEXT);
CREATE TABLE daemon_services (
  name TEXT PRIMARY KEY,
  paused INTEGER NOT NULL DEFAULT 0,
  status TEXT,                   -- running, idle, paused, off, stopped
  activity TEXT,                 -- what it's doing
  started_at TEXT,
  restarts INTEGER NOT NULL DEFAULT 0,
  last_error TEXT,
  last_error_at TEXT,
  updated_at TEXT
);
CREATE TABLE daemon_requests (
  id INTEGER PRIMARY KEY,
  service TEXT,                  -- NULL: every service
  action TEXT NOT NULL,          -- restart, pause, resume, run
  created_at TEXT NOT NULL,
  done_at TEXT,
  result TEXT
);
