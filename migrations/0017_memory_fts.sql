
CREATE VIRTUAL TABLE memory_fts USING fts5(
  key,
  title,
  body,
  content = 'memory',
  content_rowid = 'id',
  tokenize = 'unicode61'
);

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
