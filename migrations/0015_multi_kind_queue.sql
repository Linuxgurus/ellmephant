
DROP TABLE IF EXISTS encode_queue;

CREATE TABLE encode_queue (
  keyslug   TEXT PRIMARY KEY,
  content   TEXT NOT NULL,
  priority  INTEGER NOT NULL DEFAULT 3,
  queued_at TEXT NOT NULL DEFAULT (datetime('now'))
);
