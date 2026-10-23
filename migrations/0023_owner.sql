-- `owner`: whose row this is — responsibility, not authorship (#109).
--
-- `asserted_by` answers "who typed it" (kept, never consulted for authority). `edited_by` answers
-- "who wrote the state being read". Neither answers the question a reader actually has: WHO IS
-- ANSWERABLE FOR THIS. That is `owner`, and it is nullable on purpose: `NULL` means nobody is —
-- the commons, where `protected` and `private` no longer apply, anyone may write, and anyone may
-- adopt the row and stand behind it.

ALTER TABLE memory ADD COLUMN owner INTEGER REFERENCES pen(id);
CREATE INDEX memory_owner ON memory(owner);

-- The ledger of RESPONSIBILITY changes, not of writes. A row's content history lives in
-- `memory_version`; this table answers "who took it, who released it, who handed it on, since when".
CREATE TABLE pen_event (
    id       INTEGER PRIMARY KEY AUTOINCREMENT,
    at       TEXT NOT NULL DEFAULT (datetime('now')),
    key      TEXT NOT NULL,
    action   TEXT NOT NULL CHECK (action IN ('adopt', 'transfer', 'disclaim', 'retire', 'delete')),
    pen_id   INTEGER REFERENCES pen(id),   -- who acted
    from_pen INTEGER REFERENCES pen(id),   -- who held it before (NULL = nobody)
    to_pen   INTEGER REFERENCES pen(id),   -- who holds it after (NULL = nobody)
    detail   TEXT
);
CREATE INDEX pen_event_key ON pen_event(key, at);

-- Backfill: a row's owner is the pen that wrote it — EXCEPT a finger, whose parent owns. A finger
-- is a mechanism by which one self does more work; it is not a party to ownership.
UPDATE memory
SET owner = COALESCE(
    -- a FINGER (a name with a `/` in it) is owned by its parent...
    (SELECT parent.id
       FROM pen author
       JOIN pen parent ON author.parent_id = parent.id
      WHERE author.name = memory.asserted_by AND instr(author.name, '/') > 0),
    -- ...any other pen owns what it wrote.
    (SELECT id FROM pen WHERE name = memory.asserted_by)
);
