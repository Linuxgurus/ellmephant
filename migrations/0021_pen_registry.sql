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
    -- A pen's KIND is a fact about the pen, not a property of its name. A finger is a mechanism by
    -- which one self does more work; it is not a party to ownership, and that must be readable
    -- without parsing a string (Stet, review round 3: "a name is an address, never a permission").
    kind      TEXT NOT NULL DEFAULT 'self' CHECK (kind IN ('self', 'finger')),
    -- Explicit, set at registration. Never derived from the name.
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

-- The root custodian exists even if it has never spoken: a ROOT pen has no parent to retire it, so
-- somebody must be able to (#109). This is the only pen the registry creates that may not own a row.
INSERT OR IGNORE INTO pen (name) VALUES ('bootstrap');

-- SEED NAMES FROM HISTORY; NEVER SEED THE TREE FROM IT (Stet, review round 3). History knows that a
-- name existed and what it did; it cannot know a pen's STANDING — its kind, or who its parent is.
-- A tree derived from events that were never about trees is an artefact of the audit trail, and
-- `memory_history_erase` can destroy the source it rests on. So every seeded pen is a plain `self`
-- with no parent: names only. Standing is registered deliberately (kind + parent_id), and nothing
-- here invents it from a slash in a string.
--
-- Where a name contains '/', it is a DISPLAY convention that must AGREE with the row, not produce
-- it: an address that disagrees with the column is a data error, not a derivation.
