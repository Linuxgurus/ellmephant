
CREATE TABLE memory (
  id         INTEGER PRIMARY KEY,
  key        TEXT NOT NULL UNIQUE,
  title      TEXT NOT NULL,
  body       TEXT NOT NULL,
  tags       TEXT NOT NULL DEFAULT '',
  source     TEXT,
  doc_hash   TEXT NOT NULL,
  created    TEXT NOT NULL DEFAULT (datetime('now')),
  updated    TEXT NOT NULL DEFAULT (datetime('now'))
);

CREATE TABLE memory_link (
  from_id INTEGER NOT NULL REFERENCES memory(id) ON DELETE CASCADE,
  to_key  TEXT    NOT NULL,
  rel     TEXT    NOT NULL DEFAULT 'see',
  PRIMARY KEY (from_id, to_key, rel)
) WITHOUT ROWID;
CREATE INDEX idx_link_to ON memory_link(to_key);

CREATE TABLE meta (
  k TEXT PRIMARY KEY,
  v TEXT NOT NULL
);

