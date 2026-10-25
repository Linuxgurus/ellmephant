-- ELLMECHAT: channels, the message log, and the DELIVERY LEDGER (#151).
--
-- A channel message is not a memory. It is a log line with a fan-out and a receipt, and the two
-- tables here are the whole of it: what was said, and WHO WAS TOLD.
--
-- `channel_delivery` records three events per recipient, and the distinction between them is the
-- point of the design:
--
--   available_at  the message exists for this pen — it was TOLD
--   presented_at  the harness OFFERED the inbox at a turn boundary — an OPPORTUNITY
--   fetched_at    the pen actually pulled it in — it LISTENED
--
-- `presented_at` is set by whatever owns the turn (the spine, the harness), not by the model, and
-- the normal path is present-then-fetch in the same turn: a turn should never *begin* gated. The
-- gate is the backstop for the turn that offered nothing — which is exactly the turn that should be
-- stopped, because the unread message is the news it is missing.
--
-- `misses` counts how many turn boundaries offered this message without a fetch, and a channel
-- decides how many are tolerated before the lock. One is a coincidence (or a message that arrived
-- mid-turn); two is a pattern.
--
-- Records outlive bodies: pruning a message may empty `body` and set `pruned_at`, but its delivery
-- rows stay, because "you were told" must remain answerable after the text is gone.

CREATE TABLE channel (
    key        TEXT PRIMARY KEY,
    title      TEXT NOT NULL,
    -- `open` = every live self is in unless it opted out; `list` = only explicit members. This is
    -- also the invite-only switch: a channel becomes invite-only by flipping this to `list`.
    members    TEXT NOT NULL DEFAULT 'open' CHECK (members IN ('open', 'list')),
    -- What stops working while a member is behind. `none` is the default because a gate is a
    -- precious thing: `ops_team` and `emergency` exist because they are the two things this estate
    -- genuinely does under time pressure, and a third should have to argue for itself.
    gate       TEXT NOT NULL DEFAULT 'none' CHECK (gate IN ('none', 'writes', 'all')),
    grace_secs INTEGER NOT NULL DEFAULT 0 CHECK (grace_secs >= 0),
    clock      TEXT NOT NULL DEFAULT 'delivery' CHECK (clock IN ('send', 'delivery')),
    -- How many turn boundaries may offer a message without a fetch before it locks. 1 = the first
    -- miss is enough (`emergency`); 2 = a mid-turn arrival is not a crime (`ops_team`).
    misses_to_gate  INTEGER NOT NULL DEFAULT 1 CHECK (misses_to_gate >= 1),
    -- A lock that can only be cleared by a fetch or an ancestor is a lock forever if both are
    -- asleep. 0 = no expiry; anything else releases the lock after that many seconds, loudly.
    lock_max_secs   INTEGER NOT NULL DEFAULT 0 CHECK (lock_max_secs >= 0),
    created    TEXT NOT NULL DEFAULT (datetime('now'))
);

-- Membership is a set of OVERRIDES, not a roster: `in` forces membership of a `list` channel,
-- `out` removes it from an `open` one. That is one table for invite-only and one for opting out,
-- and no migration when a channel changes kind.
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
    -- Threading light: one nullable pointer, so four people in a room do not become a wall.
    reply_to INTEGER REFERENCES channel_message(seq),
    -- Set when the body was pruned; the receipt and the seq survive.
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
    -- Consecutive turn boundaries that offered this message without it being fetched.
    misses       INTEGER NOT NULL DEFAULT 0 CHECK (misses >= 0),
    PRIMARY KEY (seq, pen_id)
);
CREATE INDEX channel_delivery_pending ON channel_delivery(pen_id, fetched_at);

-- The channels that exist at MVP. `general` and `changes` carry no lock at all: most channels
-- should not, and `changes` is information ("we are rolling in 30 minutes"), not an obligation.
--
-- `ops_team` is the ops team's room — the merged home of operations and security incident response,
-- which is why it is `list` rather than open. Its members are also in `emergency` by rule (see the
-- store's `join`), because a team that runs the cluster must be reachable when it falls over.
--
-- `emergency` is `open`: being told the cluster is going down should not require a title.
INSERT INTO channel (key, title, members, gate, grace_secs, clock, misses_to_gate, lock_max_secs) VALUES
    ('general',   'General',   'open', 'none',   0,  'delivery', 1, 0),
    ('changes',   'Changes',   'open', 'none',   0,  'delivery', 1, 0),
    ('emergency', 'Emergency', 'open', 'all',    0,  'delivery', 1, 900),
    ('ops_team',  'Ops Team',  'list', 'writes', 60, 'delivery', 2, 1800);

-- Membership is NOT seeded here. Who is on the ops team is a decision, not a schema fact, and a
-- migration that quietly enrolled someone would be the same mistake as a migration that quietly
-- granted a role. Rows are added by hand (or by an admin call) until there is a reason to automate.
