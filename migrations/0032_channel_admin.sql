-- CHANNEL ADMINISTRATION (#161): creating a room, and changing what a gate costs.
--
-- ★ "EVERY CHANGE TO A GATE IS A ROW" (James, #151). Until now a gate was an `UPDATE` in SQL: it
-- left no trace of WHO made a channel dangerous, and a lock is exactly the kind of thing that must
-- be attributable afterwards. `channel_event` is that row, and `set_gate` writes it in the same
-- transaction as the change.
--
-- `create` rows exist for the same reason: a channel that appeared without a creator is either a
-- migration or an accident, and the difference matters.

CREATE TABLE channel_event (
    id      INTEGER PRIMARY KEY AUTOINCREMENT,
    at      TEXT NOT NULL DEFAULT (datetime('now')),
    channel TEXT NOT NULL,
    -- who acted; NULL only for a row written by a migration
    pen_id  INTEGER REFERENCES pen(id),
    action  TEXT NOT NULL CHECK (action IN ('create', 'gate', 'members', 'retention')),
    -- the value BEFORE and AFTER, as text: `none` -> `writes`, `open` -> `list`, etc.
    was     TEXT,
    now     TEXT
);

CREATE INDEX channel_event_channel ON channel_event (channel, id);
