-- Untagged album folders you don't want listed (on the Untagged page or in
-- its counts).
CREATE TABLE ignored_dirs (
  library TEXT NOT NULL,
  dir TEXT NOT NULL,
  ignored_at TEXT NOT NULL,
  PRIMARY KEY (library, dir)
);
