-- 0043_memory_fold — a fold that may only APPEND (soma/ellmephant#199).
--
-- `memory_merge` is a patch: to fold Stet's duplicate into Cline's keeper it takes write authority on
-- Cline's row, and the other direction takes authority on Stet's. Two siblings have none on each
-- other, so a dedupe pass across two pens has no lawful direction.
--
-- The exemption cannot be "let a peer merge": that is a blanket write exemption on every field of the
-- survivor — the #190 hole with a tidy excuse. A FOLD is the narrower act: append the source's lines
-- verbatim, change nothing that already exists, and record who folded what. This table is that
-- record. It is not a link table: a fold names where lines came FROM, and a row that is later deleted
-- must not leave an edge behind.
--
-- The keeper is the only row touched. The absorbed row keeps existing, keeps its owner, and can only
-- be removed by the pen that answers for it — deleting another pen's row stays stopped.

CREATE TABLE memory_fold (
    id           INTEGER PRIMARY KEY AUTOINCREMENT,
    keeper_id    INTEGER NOT NULL REFERENCES memory(id) ON DELETE CASCADE,
    absorbed_key TEXT    NOT NULL,
    actor        TEXT,                       -- the proven pen that folded; NULL = unproven client
    at           TEXT    NOT NULL DEFAULT (datetime('now'))
);

CREATE INDEX memory_fold_keeper ON memory_fold(keeper_id, at);
