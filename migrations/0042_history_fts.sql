CREATE VIRTUAL TABLE IF NOT EXISTS history_fts USING fts5(
    body,
    key UNINDEXED,
    n   UNINDEXED,
    tokenize='unicode61'
);

INSERT INTO history_fts (body, key, n)
SELECT json_extract(content, '$.body'), key, n
  FROM memory_version
 WHERE type = 'memory' AND content IS NOT NULL;
