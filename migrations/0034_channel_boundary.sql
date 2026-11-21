CREATE TABLE channel_boundary (
    pen_id   INTEGER PRIMARY KEY REFERENCES pen(id),
    boundary TEXT NOT NULL,
    at       TEXT NOT NULL DEFAULT (datetime('now'))
);
