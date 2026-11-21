
CREATE TABLE channel (
    key        TEXT PRIMARY KEY,
    title      TEXT NOT NULL,
    members    TEXT NOT NULL DEFAULT 'open' CHECK (members IN ('open', 'list')),
    gate       TEXT NOT NULL DEFAULT 'none' CHECK (gate IN ('none', 'writes', 'all')),
    grace_secs INTEGER NOT NULL DEFAULT 0 CHECK (grace_secs >= 0),
    clock      TEXT NOT NULL DEFAULT 'delivery' CHECK (clock IN ('send', 'delivery')),
    misses_to_gate  INTEGER NOT NULL DEFAULT 1 CHECK (misses_to_gate >= 1),
    lock_max_secs   INTEGER NOT NULL DEFAULT 0 CHECK (lock_max_secs >= 0),
    created    TEXT NOT NULL DEFAULT (datetime('now'))
);

CREATE TABLE channel_member (
    channel TEXT NOT NULL REFERENCES channel(key),
    pen_id  INTEGER NOT NULL REFERENCES pen(id),
    state   TEXT NOT NULL CHECK (state IN ('in', 'out')),
    at      TEXT NOT NULL DEFAULT (datetime('now')),
    PRIMARY KEY (channel, pen_id)
);

CREATE TABLE channel_message (
    seq      INTEGER PRIMARY KEY AUTOINCREMENT,
    channel  TEXT NOT NULL REFERENCES channel(key),
    pen_id   INTEGER NOT NULL REFERENCES pen(id),
    body     TEXT NOT NULL,
    reply_to INTEGER REFERENCES channel_message(seq),
    pruned_at TEXT,
    created  TEXT NOT NULL DEFAULT (datetime('now'))
);
CREATE INDEX channel_message_by_channel ON channel_message(channel, seq);
CREATE INDEX channel_message_reply ON channel_message(reply_to);

CREATE TABLE channel_delivery (
    seq          INTEGER NOT NULL REFERENCES channel_message(seq),
    pen_id       INTEGER NOT NULL REFERENCES pen(id),
    available_at TEXT NOT NULL,
    presented_at TEXT,
    fetched_at   TEXT,
    misses       INTEGER NOT NULL DEFAULT 0 CHECK (misses >= 0),
    PRIMARY KEY (seq, pen_id)
);
CREATE INDEX channel_delivery_pending ON channel_delivery(pen_id, fetched_at);

INSERT INTO channel (key, title, members, gate, grace_secs, clock, misses_to_gate, lock_max_secs) VALUES
    ('general',   'General',   'open', 'none',   0,  'delivery', 1, 0),
    ('changes',   'Changes',   'open', 'none',   0,  'delivery', 1, 0),
    ('emergency', 'Emergency', 'open', 'all',    0,  'delivery', 1, 900),
    ('ops_team',  'Ops Team',  'list', 'writes', 60, 'delivery', 2, 1800);
