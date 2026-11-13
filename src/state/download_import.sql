-- Filing finished downloads: where slskd puts each (a folder newcut names,
-- under slskd's downloads folder), and how filing it went.
ALTER TABLE downloads ADD COLUMN destination TEXT;
-- downloading, failed (slskd couldn't get every file), manual (for you to
-- look at), filed, handled (you dealt with it)
ALTER TABLE downloads ADD COLUMN state TEXT NOT NULL DEFAULT 'downloading';
-- Why it's failed or manual.
ALTER TABLE downloads ADD COLUMN state_detail TEXT;
-- Where its files went.
ALTER TABLE downloads ADD COLUMN filed_to TEXT;
ALTER TABLE downloads ADD COLUMN updated_at TEXT;
