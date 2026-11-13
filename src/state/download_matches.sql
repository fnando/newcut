-- The release a queued folder matched, and which track each file is.
ALTER TABLE downloads ADD COLUMN release_mbid TEXT;
-- What it is, to show: "Digital Media · 2026-11-06".
ALTER TABLE downloads ADD COLUMN release_label TEXT;
-- exact, likely or none
ALTER TABLE downloads ADD COLUMN match_kind TEXT NOT NULL DEFAULT 'none';
-- Why it isn't exact.
ALTER TABLE downloads ADD COLUMN match_reason TEXT;
-- JSON: each file's track MBID on the release (or null), in `files` order.
ALTER TABLE downloads ADD COLUMN tracks TEXT NOT NULL DEFAULT '[]';
