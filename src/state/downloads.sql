-- Albums queued in slskd: which folder, from whom, and its files (so their
-- transfers can be found in slskd later).
CREATE TABLE downloads (
    id INTEGER PRIMARY KEY,
    -- A release group's MBID, or `deezer-<id>`.
    release_key TEXT NOT NULL,
    username TEXT NOT NULL,
    dir TEXT NOT NULL,
    -- JSON: [[filename, size], ...]
    files TEXT NOT NULL,
    queued_at TEXT NOT NULL
);
CREATE INDEX downloads_release ON downloads (release_key, queued_at);
