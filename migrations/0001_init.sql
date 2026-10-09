-- 0001_init — SPEC §7, minus the vec0 table.
--
-- `memory_vec` (vec0) is added by a later migration, once the sqlite-vec extension is
-- registered in-process (PLAN §5, S0.1). Keeping the pure-SQLite schema separate means
-- `cargo test` runs with no extension, no dylib, offline.

CREATE TABLE memory (
  id      INTEGER PRIMARY KEY,
  key     TEXT NOT NULL UNIQUE,                 -- slug; upsert + link target
  scope   TEXT NOT NULL,                        -- single
  type    TEXT NOT NULL DEFAULT 'fact',         -- signpost|dictate|fact|profile
  title   TEXT NOT NULL,                        -- salient line
  body    TEXT NOT NULL,                        -- full, verifiable
  source  TEXT,                                 -- provenance: command/path/repo/quote
  hash    TEXT NOT NULL,                        -- sha256(model|dim|embed_text)
  model   TEXT,
  dim     INTEGER,
  created TEXT NOT NULL DEFAULT (datetime('now')),
  updated TEXT NOT NULL DEFAULT (datetime('now'))
);
CREATE INDEX idx_memory_scope ON memory(scope);
CREATE INDEX idx_memory_type  ON memory(type);

CREATE TABLE tag (
  id   INTEGER PRIMARY KEY,
  name TEXT NOT NULL UNIQUE
);

CREATE TABLE memory_tag (
  memory_id INTEGER NOT NULL REFERENCES memory(id) ON DELETE CASCADE,
  tag_id    INTEGER NOT NULL REFERENCES tag(id)    ON DELETE CASCADE,
  PRIMARY KEY (memory_id, tag_id)
) WITHOUT ROWID;
CREATE INDEX idx_mtag_tag ON memory_tag(tag_id);

-- external-content FTS: no text copy, kept in sync by the three triggers below
CREATE VIRTUAL TABLE memory_fts USING fts5(
  title, body, content='memory', content_rowid='id',
  tokenize='unicode61 remove_diacritics 2');

CREATE TRIGGER memory_ai AFTER INSERT ON memory BEGIN
  INSERT INTO memory_fts(rowid, title, body) VALUES (new.id, new.title, new.body);
END;

CREATE TRIGGER memory_ad AFTER DELETE ON memory BEGIN
  INSERT INTO memory_fts(memory_fts, rowid, title, body)
    VALUES ('delete', old.id, old.title, old.body);
END;

CREATE TRIGGER memory_au AFTER UPDATE ON memory BEGIN
  INSERT INTO memory_fts(memory_fts, rowid, title, body)
    VALUES ('delete', old.id, old.title, old.body);
  INSERT INTO memory_fts(rowid, title, body) VALUES (new.id, new.title, new.body);
END;

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
