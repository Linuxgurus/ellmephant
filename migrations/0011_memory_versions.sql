
CREATE TABLE memory_version (
  type       TEXT    NOT NULL,
  key        TEXT    NOT NULL,
  n          INTEGER NOT NULL,
  written_at TEXT    NOT NULL DEFAULT (datetime('now')),
  encoding   TEXT    NOT NULL DEFAULT 'json',
  note       TEXT,
  content    TEXT    NOT NULL,
  PRIMARY KEY (type, key, n)
);
