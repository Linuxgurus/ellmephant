
CREATE TABLE config (
    key           TEXT PRIMARY KEY COLLATE NOCASE,
    value         TEXT NOT NULL,
    set_audience  TEXT NOT NULL DEFAULT 'pen:estate',
    read_audience TEXT NOT NULL DEFAULT 'public',
    owner         INTEGER REFERENCES pen(id),
    updated_at    TEXT NOT NULL DEFAULT (datetime('now')),
    source        TEXT NOT NULL DEFAULT ''
);

CREATE INDEX config_set_audience ON config(set_audience);
CREATE INDEX config_read_audience ON config(read_audience);
