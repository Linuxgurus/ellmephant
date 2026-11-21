
CREATE TABLE private_read_new (
    id     INTEGER PRIMARY KEY AUTOINCREMENT,
    at     TEXT NOT NULL DEFAULT (datetime('now')),
    pen    TEXT NOT NULL,
    key    TEXT,
    reason TEXT NOT NULL CHECK (reason IN ('subtree', 'search', 'forensic', 'borrowed')),
    detail TEXT
);

INSERT INTO private_read_new (id, at, pen, key, reason, detail)
     SELECT id, at, pen, key, reason, detail FROM private_read;

DROP TABLE private_read;

ALTER TABLE private_read_new RENAME TO private_read;

CREATE INDEX private_read_at ON private_read(at);
CREATE INDEX private_read_pen ON private_read(pen, at);
