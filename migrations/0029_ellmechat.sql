-- ELLMECHAT: channels, the message log, and the DELIVERY LEDGER (#151).
--
-- A channel message is not a memory. It is a log line with a fan-out and a receipt, and the two
-- tables here are the whole of it: what was said, and WHO WAS TOLD.
--
-- `channel_delivery` records three events per recipient, and the distinction between them is the
-- point of the design:
--
--   available_at  the message exists for this pen — it was TOLD
--   presented_at  a turn started; the pen had the chance to fetch — an OPPORTUNITY
--   fetched_at    the pen actually pulled it in — it LISTENED
--
-- The gate (§gate) is the consequence of the second-to-third gap, never of the sender's timing:
-- a pen that was asleep was told nothing it could act on, so `clock = 'delivery'` measures from the
-- opportunity and not from `available_at`.
--
-- Records outlive bodies: pruning a message may empty `body`, but its delivery rows stay, because
-- "you were told" must remain answerable after the text is gone.

CREATE TABLE channel (
    key        TEXT PRIMARY KEY,
    title      TEXT NOT NULL,
    -- What stops working while a member is behind. `none` is the default because a gate is a
    -- precious thing: `ops` and `emergency` exist because they are the two things this estate
    -- genuinely does under time pressure, and a third should have to argue for itself.
    gate       TEXT NOT NULL DEFAULT 'none' CHECK (gate IN ('none', 'writes', 'all')),
    grace_secs INTEGER NOT NULL DEFAULT 0 CHECK (grace_secs >= 0),
    clock      TEXT NOT NULL DEFAULT 'delivery' CHECK (clock IN ('send', 'delivery')),
    created    TEXT NOT NULL DEFAULT (datetime('now'))
);

CREATE TABLE channel_message (
    seq      INTEGER PRIMARY KEY AUTOINCREMENT,
    channel  TEXT NOT NULL REFERENCES channel(key),
    pen_id   INTEGER NOT NULL REFERENCES pen(id),
    body     TEXT NOT NULL,
    -- Threading light: one nullable pointer, so four people in a room do not become a wall.
    reply_to INTEGER REFERENCES channel_message(seq),
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
    PRIMARY KEY (seq, pen_id)
);
CREATE INDEX channel_delivery_pending ON channel_delivery(pen_id, fetched_at);

-- The two channels that gate, and the one that never does. `general` is deliberately `none`:
-- the vast majority of channels should carry no lock at all.
INSERT INTO channel (key, title, gate, grace_secs, clock) VALUES
    ('general',   'General',   'none',   0,    'delivery'),
    ('ops',       'Ops',       'writes', 60,   'delivery'),
    ('emergency', 'Emergency', 'all',    60,   'delivery');
