
CREATE TABLE config_event (
    id                INTEGER PRIMARY KEY AUTOINCREMENT,
    at                TEXT NOT NULL DEFAULT (datetime('now')),
    key               TEXT NOT NULL,
    action            TEXT NOT NULL CHECK (action IN ('unset')),
    actor             TEXT NOT NULL,
    pen_id            INTEGER REFERENCES pen(id),
    old_set_audience  TEXT NOT NULL,
    old_read_audience TEXT NOT NULL,
    old_owner         INTEGER REFERENCES pen(id),
    old_updated_at    TEXT NOT NULL,
    value_sha256      TEXT NOT NULL
);

CREATE INDEX config_event_key ON config_event (key, id);
