
CREATE TABLE memory_fold (
    id           INTEGER PRIMARY KEY AUTOINCREMENT,
    keeper_id    INTEGER NOT NULL REFERENCES memory(id) ON DELETE CASCADE,
    absorbed_key TEXT    NOT NULL,
    actor        TEXT,
    at           TEXT    NOT NULL DEFAULT (datetime('now'))
);

CREATE INDEX memory_fold_keeper ON memory_fold(keeper_id, at);
