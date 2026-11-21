
CREATE TABLE sudoers (
    id                INTEGER PRIMARY KEY AUTOINCREMENT,
    powerful          INTEGER NOT NULL REFERENCES pen(id),
    weak              INTEGER NOT NULL REFERENCES pen(id),
    max_duration_secs INTEGER NOT NULL CHECK (max_duration_secs > 0 AND max_duration_secs <= 86400),
    source            TEXT NOT NULL,
    granted_at        TEXT NOT NULL DEFAULT (datetime('now')),
    CHECK (powerful <> weak),
    UNIQUE (powerful, weak)
);

CREATE TABLE active_sudo (
    id         INTEGER PRIMARY KEY AUTOINCREMENT,
    powerful   INTEGER NOT NULL REFERENCES pen(id),
    weak       INTEGER NOT NULL REFERENCES pen(id),
    started_at TEXT NOT NULL DEFAULT (datetime('now')),
    expires_at TEXT NOT NULL,
    via        INTEGER REFERENCES active_sudo(id),
    source     TEXT NOT NULL CHECK (source IN ('policy', 'lend')),
    CHECK (powerful <> weak)
);
CREATE INDEX active_sudo_weak ON active_sudo(weak, powerful);
CREATE INDEX active_sudo_via ON active_sudo(via);
