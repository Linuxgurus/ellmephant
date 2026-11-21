
CREATE TABLE channel_event (
    id      INTEGER PRIMARY KEY AUTOINCREMENT,
    at      TEXT NOT NULL DEFAULT (datetime('now')),
    channel TEXT NOT NULL,
    pen_id  INTEGER REFERENCES pen(id),
    action  TEXT NOT NULL CHECK (action IN ('create', 'gate', 'members', 'retention')),
    was     TEXT,
    now     TEXT
);

CREATE INDEX channel_event_channel ON channel_event (channel, id);
