-- An album's last upgrade: the quality it was, the one it got, and when.
ALTER TABLE upgrades ADD COLUMN upgraded_from TEXT;
ALTER TABLE upgrades ADD COLUMN upgraded_to TEXT;
ALTER TABLE upgrades ADD COLUMN upgraded_at TEXT;
UPDATE upgrades
   SET upgraded_from = substr(result, length('upgraded from ') + 1),
       upgraded_to = quality,
       upgraded_at = checked_at
 WHERE result LIKE 'upgraded from %';
