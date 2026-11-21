
CREATE TABLE pen (
    id        INTEGER PRIMARY KEY AUTOINCREMENT,
    name      TEXT NOT NULL UNIQUE,
    kind      TEXT NOT NULL DEFAULT 'self' CHECK (kind IN ('self', 'finger')),
    parent_id INTEGER REFERENCES pen(id),
    created   TEXT NOT NULL DEFAULT (datetime('now')),
    ceased_at TEXT
);

CREATE INDEX pen_parent ON pen(parent_id);

INSERT OR IGNORE INTO pen (name)
SELECT name FROM (
    SELECT DISTINCT asserted_by AS name FROM memory
     WHERE asserted_by <> '' AND lower(asserted_by) <> 'anonymous'
    UNION
    SELECT DISTINCT asserted_by AS name FROM mail
     WHERE asserted_by <> '' AND lower(asserted_by) <> 'anonymous'
);

INSERT OR IGNORE INTO pen (name) VALUES ('estate');
