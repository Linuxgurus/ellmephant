-- 0001_init — the store schema (SPEC §7), minus the vec0 table.
--
-- `memory_vec` (vec0) is a derived index created programmatically by `index::vec::ensure`:
-- its dimension is a runtime property of the embedder, not schema. Keeping the pure-SQLite
-- schema separate means `cargo test` runs with no extension, no dylib, offline.
--
-- One memory embeds a single document — `key` + `tags` + `title` + `body` — so a single
-- `doc_hash` governs whether the row needs (re-)embedding.

CREATE TABLE memory (
  id         INTEGER PRIMARY KEY,
  key        TEXT NOT NULL UNIQUE,              -- slug; upsert + link target
  title      TEXT NOT NULL,                     -- smart-caveman one-liner
  body       TEXT NOT NULL,                     -- smart-caveman fact
  tags       TEXT NOT NULL DEFAULT '',          -- normalised names, comma-joined, sorted
  source     TEXT,                              -- provenance: command/path/repo/quote
  doc_hash   TEXT NOT NULL,                     -- sha256(model|dim|embedded document)
  created    TEXT NOT NULL DEFAULT (datetime('now')),
  updated    TEXT NOT NULL DEFAULT (datetime('now'))
);

CREATE TABLE memory_link (
  from_id INTEGER NOT NULL REFERENCES memory(id) ON DELETE CASCADE,
  to_key  TEXT    NOT NULL,                     -- target KEY, not id
  rel     TEXT    NOT NULL DEFAULT 'see',       -- see|supersedes|part-of|contradicts
  PRIMARY KEY (from_id, to_key, rel)
) WITHOUT ROWID;
CREATE INDEX idx_link_to ON memory_link(to_key);

CREATE TABLE meta (
  k TEXT PRIMARY KEY,
  v TEXT NOT NULL
);

