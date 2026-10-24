-- A THIRD KIND OF BOUNDARY READ (adversarial review, 2026-10-24).
--
-- `private_read.reason` allowed `subtree`, `search` and `forensic`. When sessions became reach, a pen
-- could read a row that NO ancestry explains — the reach came from authority it was lent or had
-- borrowed, and it expires. Recording that as `subtree` would be a log claiming "an ancestor read down
-- its own tree" for an act the tree does not account for.
--
-- SQLite cannot alter a CHECK constraint, so this is the textbook rebuild: copy, drop, rename, reindex.
-- Every existing row is preserved verbatim — no reason is rewritten, and no row is re-interpreted.

CREATE TABLE private_read_new (
    id     INTEGER PRIMARY KEY AUTOINCREMENT,
    at     TEXT NOT NULL DEFAULT (datetime('now')),
    -- Who looked. A name, deliberately not an id.
    pen    TEXT NOT NULL,
    -- The row looked at; NULL when the act was a search that returned several.
    key    TEXT,
    -- `subtree` — an ancestor read a descendant's private row; `search` — a search returned them;
    -- `forensic` — an explicit exemption (capability NOT BUILT); `borrowed` — a held session, not the
    -- shape of the tree, let the read through.
    reason TEXT NOT NULL CHECK (reason IN ('subtree', 'search', 'forensic', 'borrowed')),
    -- How many, which sample, or why — whatever makes the row usable without a second query.
    detail TEXT
);

INSERT INTO private_read_new (id, at, pen, key, reason, detail)
     SELECT id, at, pen, key, reason, detail FROM private_read;

DROP TABLE private_read;

ALTER TABLE private_read_new RENAME TO private_read;

CREATE INDEX private_read_at ON private_read(at);
CREATE INDEX private_read_pen ON private_read(pen, at);
