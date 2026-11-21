
CREATE TABLE pen_credential (
    pen_id     INTEGER PRIMARY KEY REFERENCES pen(id) ON DELETE CASCADE,
    hash       TEXT NOT NULL,
    issued_at  TEXT NOT NULL DEFAULT (datetime('now')),
    issued_by  INTEGER REFERENCES pen(id),
    rotated_at TEXT
);
