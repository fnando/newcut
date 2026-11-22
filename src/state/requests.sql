-- Albums you asked for: the requester searches Soulseek for each until you
-- have it (or take the request back).
CREATE TABLE requests (
  rg_mbid TEXT PRIMARY KEY,
  requested_at TEXT NOT NULL,
  checked_at TEXT,
  next_at TEXT,                  -- not looked for again before this
  result TEXT                    -- what the last look found
);
