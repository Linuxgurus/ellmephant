-- A READ THAT CROSSES A BOUNDARY IS RECORDED (soma/ellmephant#129, the missing half of #120).
--
-- The private tier ENFORCES two things and until now recorded neither: a pen reads its own rows, and
-- an ancestor reads its whole subtree. `audience.rs` described a `private_read` table and a
-- `Reader::forensic` constructor as though they existed; neither did. This is the table, and the
-- recorder that fills it.
--
-- ★ THE PEN IS A NAME, NOT A FOREIGN KEY. An audit that dies with the registry is an audit of the
-- registry, not of the acts: a retired pen's reads must still be readable afterwards, and a pen
-- deleted from `pen` must not take its record with it (or the deletion becomes a way to erase what
-- it did).
--
-- ★ RETENTION IS A DECISION, NOT AN ACCIDENT: rows are pruned by age (`audit::prune`), the way
-- letters are (`mail_prune`) — an audit table that grows forever is a second store nobody maintains.
CREATE TABLE private_read (
    id     INTEGER PRIMARY KEY AUTOINCREMENT,
    at     TEXT NOT NULL DEFAULT (datetime('now')),
    -- Who looked. A name, deliberately not an id.
    pen    TEXT NOT NULL,
    -- The row looked at; NULL when the act was a search that returned several.
    key    TEXT,
    -- `subtree` — an ancestor read a descendant's private row; `search` — a search returned them;
    -- `forensic` — an explicit exemption (capability NOT BUILT; the reason is reserved so that the
    -- rows written later are not ambiguous about which mechanism produced them).
    reason TEXT NOT NULL CHECK (reason IN ('subtree', 'search', 'forensic')),
    -- How many, which sample, or why — whatever makes the row usable without a second query.
    detail TEXT
);

CREATE INDEX private_read_at ON private_read(at);
CREATE INDEX private_read_pen ON private_read(pen, at);
