-- 0011_memory_versions — the source half gets the property only the derived half had: UNDO.
--
-- WHY. A row that was wrong once and then corrected kept only the correction. There is no history,
-- no undo, and every hygiene rule built on that fact is a workaround — "FLAG rather than rewrite",
-- because a rewrite destroys the only evidence of what was there. This table keeps the state a write
-- REPLACES, so CORRECT becomes recoverable and a flag can go back to meaning uncertainty rather than
-- loss.
--
-- WHAT A VERSION IS: a full JSON SNAPSHOT of the source row (key, title, body, tags, source, created,
-- updated) as it stood immediately BEFORE a change to its DOCUMENT — `title` + `body`, the fields the
-- embedder sees. Snapshot rather than delta: one row reads as a complete state, with nothing to
-- replay. `n` counts states per key, so `n = 1` is the first state that was replaced. `written_at` is
-- when the snapshot was taken; the replaced state's own timestamps ride inside `content`.
--
-- WHAT IT DELIBERATELY IS NOT, and none of these is an oversight:
--   * nothing for an INSERT — nothing was replaced;
--   * nothing for a TAG or SOURCE write — filing is not meaning, which is exactly the `encode_hash`
--     predicate the write path already computes;
--   * NOT searchable by meaning: a version is never embedded (no vector, no queue entry), so an old
--     state can never be returned as current;
--   * NOT cascaded from `memory` — there is no FK here on purpose, so history SURVIVES a delete.
--     Erasure is therefore a POLICY (`memory_history_erase`) rather than an accident of the schema;
--   * NOT compressed yet: `encoding` says how `content` is stored, so a future format cannot orphan
--     rows written now.

CREATE TABLE memory_version (
  type       TEXT    NOT NULL,                       -- document kind: 'memory' (a project's prose can follow)
  key        TEXT    NOT NULL,                       -- the ADDRESS the version belongs to
  n          INTEGER NOT NULL,                       -- 1-based, per (type, key)
  written_at TEXT    NOT NULL DEFAULT (datetime('now')),
  encoding   TEXT    NOT NULL DEFAULT 'json',
  note       TEXT,                                   -- why the change was made, when the writer knows
  content    TEXT    NOT NULL,                       -- the whole source row, as JSON
  PRIMARY KEY (type, key, n)
);
