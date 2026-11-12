-- ★ THE THIRD AXIS (soma/ellmephant#115): exact words over HISTORY.
--
-- Address (`memory_history KEY`, `memory_get version=N`) and meaning (`document_vec_deleted`,
-- `semantic_search(deleted: true)`) already have their axes. What was missing is "which document
-- EVER said this?" — the axis that answers hashes, keys and negations, where embeddings are blind.
--
-- A separate FTS5 table, never folded into `memory_fts`: BM25 over versions must not leak into
-- `memory_search`'s ranking (the legs there are fused; a third would change every page).
--
-- Insert on snapshot, delete on erase. Versions are immutable, so there is NO update trigger —
-- unlike `memory_fts`, which needs `au` because a body can be rewritten.
CREATE VIRTUAL TABLE IF NOT EXISTS history_fts USING fts5(
    body,
    key UNINDEXED,
    n   UNINDEXED,
    tokenize='unicode61'
);

-- Every version already held is indexed once, here: the body lives inside the JSON snapshot, and
-- json_extract reads it without a rebuild of anything else. ~2k rows on the live store.
INSERT INTO history_fts (body, key, n)
SELECT json_extract(content, '$.body'), key, n
  FROM memory_version
 WHERE type = 'memory' AND content IS NOT NULL;
