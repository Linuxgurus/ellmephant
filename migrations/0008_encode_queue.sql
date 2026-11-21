CREATE TABLE encode_queue (
  key        TEXT PRIMARY KEY,
  kind       TEXT NOT NULL DEFAULT 'memory',
  priority   INTEGER NOT NULL DEFAULT 0,
  doc_hash   TEXT NOT NULL,
  queued_at  TEXT NOT NULL DEFAULT (datetime('now')),
  attempts   INTEGER NOT NULL DEFAULT 0,
  last_error TEXT
);

CREATE INDEX encode_queue_due ON encode_queue (priority, queued_at);
