-- Deezer support is gone: its albums, and what artists had of it.
DROP INDEX IF EXISTS deezer_albums_artist;
DROP TABLE IF EXISTS deezer_albums;
ALTER TABLE artists DROP COLUMN deezer_id;
ALTER TABLE artists DROP COLUMN deezer_baseline_at;
ALTER TABLE artists DROP COLUMN deezer_checked_at;
ALTER TABLE artists DROP COLUMN deezer_next_at;
