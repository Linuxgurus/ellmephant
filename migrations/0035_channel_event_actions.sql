-- THE EVENT TABLE MUST ACCEPT EVERY ACTION THE CODE WRITES (#159/#165 review).
--
-- 0032 was edited by later PRs to widen its CHECK, which is a deployment hazard: a store that
-- migrated at the earlier version never re-runs it, so a later action (retention, expiry, and now a
-- membership change) fails the CHECK forever — silently, because the expiry write is best-effort.
--
-- ★ MIGRATIONS ARE IMMUTABLE ONCE THEY CAN MERGE. SQLite cannot ALTER a CHECK, so widening it is a
-- table rebuild: create, copy, drop, rename, re-index. This migration is the one place the action set
-- lives, and adding an action is a new migration, never an edit to an old file.
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
