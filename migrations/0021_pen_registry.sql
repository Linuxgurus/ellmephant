-- The pen registry: a pen becomes a ROW, not an environment variable.
--
-- Everything ancestry needs was missing because identity lived in `ELLMEPHANT_IDENTITIES`
-- (name:secret, re-read every call). A name in an env var cannot have a parent, cannot be
-- tombstoned, and cannot be the target of a foreign key — so `owner` had nowhere to point and a
-- recalled pen could not leave a record that it had ever existed (soma/ellmephant#109).
--
-- This migration adds the row and seeds it from the authors the store already knows about. It
-- deliberately changes NO behaviour: nothing reads `pen` yet. `owner`, the responsibility ledger
-- and the tree land on top of this.
--
-- DELIBERATELY FEW COLUMNS. A retired pen is `ceased_at IS NOT NULL` — a `status` column would be
-- the same fact written twice, and every derived field is a field that can disagree with its
-- source. The memorial text belongs in `pen_event.detail`, where the act is recorded, not on the
-- pen row. Session context is the scarce resource here: a field that a reader never needs is a
-- field that costs tokens every time somebody loads the row.

CREATE TABLE pen (
    id        INTEGER PRIMARY KEY AUTOINCREMENT,
    name      TEXT NOT NULL UNIQUE,
    parent_id INTEGER REFERENCES pen(id),
    created   TEXT NOT NULL DEFAULT (datetime('now')),
    ceased_at TEXT
);

CREATE INDEX pen_parent ON pen(parent_id);

-- Every name that has ever spoken in this store, from memories and from mail.
INSERT OR IGNORE INTO pen (name)
SELECT name FROM (
    SELECT DISTINCT asserted_by AS name FROM memory WHERE asserted_by <> ''
    UNION
    SELECT DISTINCT asserted_by AS name FROM mail WHERE asserted_by <> ''
);

-- A finger writes as "<parent>/finger-<n>". The parent is a real pen; make it one, so the finger
-- always resolves through something that outlives it.
INSERT OR IGNORE INTO pen (name)
SELECT DISTINCT substr(name, 1, instr(name, '/') - 1)
FROM pen
WHERE instr(name, '/') > 0 AND substr(name, 1, instr(name, '/') - 1) <> '';

UPDATE pen
SET parent_id = (
    SELECT p2.id FROM pen p2
    WHERE p2.name = substr(pen.name, 1, instr(pen.name, '/') - 1)
)
WHERE instr(name, '/') > 0;
