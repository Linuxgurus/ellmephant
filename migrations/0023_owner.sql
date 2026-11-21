
ALTER TABLE memory ADD COLUMN owner INTEGER REFERENCES pen(id);
CREATE INDEX memory_owner ON memory(owner);

CREATE TABLE pen_event (
    id       INTEGER PRIMARY KEY AUTOINCREMENT,
    at       TEXT NOT NULL DEFAULT (datetime('now')),
    key      TEXT NOT NULL,
    action   TEXT NOT NULL CHECK (action IN ('adopt', 'transfer', 'disclaim', 'publish', 'retire', 'delete')),
    pen_id   INTEGER REFERENCES pen(id),
    from_pen INTEGER REFERENCES pen(id),
    to_pen   INTEGER REFERENCES pen(id),
    detail   TEXT
);
CREATE INDEX pen_event_key ON pen_event(key, at);

UPDATE memory
SET owner = COALESCE(
    (SELECT parent.id
       FROM pen author
       JOIN pen parent ON author.parent_id = parent.id
      WHERE author.name = memory.asserted_by AND instr(author.name, '/') > 0),
    (SELECT id FROM pen WHERE name = memory.asserted_by)
);
