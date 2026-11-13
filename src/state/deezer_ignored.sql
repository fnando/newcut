-- Deezer albums you don't want, like ignored release groups.
ALTER TABLE deezer_albums ADD COLUMN ignored_at TEXT;
