CREATE TABLE channel_event_new (
    id      INTEGER PRIMARY KEY AUTOINCREMENT,
    at      TEXT NOT NULL DEFAULT (datetime('now')),
    channel TEXT NOT NULL,
    pen_id  INTEGER REFERENCES pen(id),
    action  TEXT NOT NULL CHECK (action IN ('create', 'gate', 'members', 'retention', 'expiry')),
    was     TEXT,
    now     TEXT
);

INSERT INTO channel_event_new (id, at, channel, pen_id, action, was, now)
    SELECT id, at, channel, pen_id, action, was, now FROM channel_event;

DROP TABLE channel_event;
ALTER TABLE channel_event_new RENAME TO channel_event;
CREATE INDEX channel_event_channel ON channel_event (channel, id);
