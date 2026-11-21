CREATE TABLE private_read (
    id     INTEGER PRIMARY KEY AUTOINCREMENT,
    at     TEXT NOT NULL DEFAULT (datetime('now')),
    pen    TEXT NOT NULL,
    key    TEXT,
    reason TEXT NOT NULL CHECK (reason IN ('subtree', 'search', 'forensic')),
    detail TEXT
);

CREATE INDEX private_read_at ON private_read(at);
CREATE INDEX private_read_pen ON private_read(pen, at);
