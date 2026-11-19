-- Each file's audio quality: format (FLAC, MP3, …), lossless or not, bit
-- depth, sample rate (Hz) and bitrate (kbps).
ALTER TABLE files ADD COLUMN format TEXT;
ALTER TABLE files ADD COLUMN lossless INTEGER NOT NULL DEFAULT 0;
ALTER TABLE files ADD COLUMN bit_depth INTEGER;
ALTER TABLE files ADD COLUMN sample_rate INTEGER;
ALTER TABLE files ADD COLUMN bitrate INTEGER;
-- Read every file again at the next scan, for its quality.
UPDATE files SET mtime = -1;
-- An upgrade: JSON, the album folders it replaces (NULL: not an upgrade).
ALTER TABLE downloads ADD COLUMN replaces TEXT;
-- An upgrade's qualities: the album's, and the folder's.
ALTER TABLE downloads ADD COLUMN upgrade_from TEXT;
ALTER TABLE downloads ADD COLUMN upgrade_to TEXT;
-- What an upgrade of each album found: the quality it was, and when to
-- look again.
CREATE TABLE upgrades (
  rg_mbid TEXT PRIMARY KEY,
  quality TEXT,                  -- the album's, when last checked
  checked_at TEXT,
  next_at TEXT,                  -- not before this
  result TEXT,                   -- what the check found
  requested_at TEXT              -- you asked for one: first in line
);
