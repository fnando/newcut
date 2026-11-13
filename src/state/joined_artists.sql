-- Joined artists (a MusicBrainz group named after its members, like
-- "K-Salaam & Beatnick"): hidden, their albums credited to each member.
CREATE TABLE artist_members (
  artist_mbid TEXT NOT NULL,
  member_mbid TEXT NOT NULL,
  PRIMARY KEY (artist_mbid, member_mbid)
);

-- Look every artist up again, for their members.
UPDATE artists SET resolved_at = NULL;
