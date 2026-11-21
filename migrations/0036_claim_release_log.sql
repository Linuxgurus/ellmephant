CREATE TABLE claim_release_log (
    id          INTEGER PRIMARY KEY AUTOINCREMENT,
    at          TEXT NOT NULL DEFAULT (datetime('now')),
    name        TEXT NOT NULL,
    owner       TEXT NOT NULL,
    released_by TEXT NOT NULL,
    reason      TEXT NOT NULL
);

CREATE INDEX claim_release_log_at ON claim_release_log(at);
CREATE INDEX claim_release_log_name ON claim_release_log(name);
