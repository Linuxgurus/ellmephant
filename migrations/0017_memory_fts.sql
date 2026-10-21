-- THE WORD INDEX (soma/ellmephant#62).
--
-- The store has a semantic leg and a lexical leg that is a SCAN over key/title/body. This adds the
-- third thing: FTS5 over the same columns, so a name-shaped query ("cargo", "pr53-verification",
-- "issue 53") is answered by a word index instead of by 0.58-vs-0.59 of cosine noise.
--
-- ★ EXTERNAL CONTENT, WITH ALL THREE TRIGGERS. This is the trap spine already paid for (v7): an
-- FTS5 table created with `content=` does NOT update itself, and a missing UPDATE trigger leaves the
-- index serving text that no longer exists — silently, for weeks, because a stale hit looks like a
-- hit. INSERT alone is not enough either: a delete without the 'delete' command leaves a row that
-- can never be matched but still occupies the index.
--
-- ★ THE DELETE/UPDATE FORM IS THE 'delete' COMMAND, not a plain DELETE: with external content the
-- index stores only tokens, so FTS5 needs the OLD VALUES to remove them. A plain `DELETE FROM
-- memory_fts` is an error on an external-content table.
--
-- Tokenizer: unicode61 — the same choice spine made, and the only one that handles the punctuation
-- our keys are full of (`pr53-verification` tokenises to `pr53` + `verification`, which is exactly
-- the matching behaviour we want).

CREATE VIRTUAL TABLE memory_fts USING fts5(
  key,
  title,
  body,
  content = 'memory',
  content_rowid = 'id',
  tokenize = 'unicode61'
);

-- Fill from the rows that already exist. A fresh store gets an empty table; a live store gets its
-- whole corpus indexed in one statement (measured: milliseconds for ~750 rows).
INSERT INTO memory_fts (rowid, key, title, body)
  SELECT id, key, title, body FROM memory;

CREATE TRIGGER memory_fts_ai AFTER INSERT ON memory BEGIN
  INSERT INTO memory_fts (rowid, key, title, body)
    VALUES (new.id, new.key, new.title, new.body);
END;

CREATE TRIGGER memory_fts_ad AFTER DELETE ON memory BEGIN
  INSERT INTO memory_fts (memory_fts, rowid, key, title, body)
    VALUES ('delete', old.id, old.key, old.title, old.body);
END;

CREATE TRIGGER memory_fts_au AFTER UPDATE ON memory BEGIN
  INSERT INTO memory_fts (memory_fts, rowid, key, title, body)
    VALUES ('delete', old.id, old.key, old.title, old.body);
  INSERT INTO memory_fts (rowid, key, title, body)
    VALUES (new.id, new.key, new.title, new.body);
END;
