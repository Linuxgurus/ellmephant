-- SUDO: authority borrowed, bounded, and revocable (#109, round 6).
--
-- The tree gives a pen its own subtree by right. Everything wider must be BORROWED — and the widest
-- of all is `estate`, which is a real pen with no entity behind it: no identity entry, no credential,
-- none coming. So `estate` can never be a caller. Its universal reach is only usable by borrowing,
-- which is what this table pair is for, and why a session has a duration, a revocation, and a chain.
--
-- ★ TWO TABLES, TWO QUESTIONS. `sudoers` is POLICY (who may borrow from whom, and for how long);
-- `active_sudo` is a SESSION (a borrowing that is happening now and will stop on its own). Keeping
-- them apart is what makes "kill the policy" and "kill the session" different acts with different
-- consequences.

CREATE TABLE sudoers (
    id                INTEGER PRIMARY KEY AUTOINCREMENT,
    powerful          INTEGER NOT NULL REFERENCES pen(id),
    weak              INTEGER NOT NULL REFERENCES pen(id),
    -- A session can never outlive the policy that permits it. Capped rather than defaulted: a
    -- borrowing with no ceiling is a credential with a friendlier name.
    max_duration_secs INTEGER NOT NULL CHECK (max_duration_secs > 0 AND max_duration_secs <= 86400),
    -- WHERE THIS CAME FROM, because the configuration is the trust root: whoever can write it can
    -- hand out authority, and that fact should be visible in the row rather than inferred.
    source            TEXT NOT NULL,
    granted_at        TEXT NOT NULL DEFAULT (datetime('now')),
    -- A pen borrowing from itself is not an act, it is a typo with a session attached.
    CHECK (powerful <> weak),
    UNIQUE (powerful, weak)
);

CREATE TABLE active_sudo (
    id         INTEGER PRIMARY KEY AUTOINCREMENT,
    powerful   INTEGER NOT NULL REFERENCES pen(id),
    weak       INTEGER NOT NULL REFERENCES pen(id),
    started_at TEXT NOT NULL DEFAULT (datetime('now')),
    expires_at TEXT NOT NULL,
    -- The session this one was PASSED from, so revocation can walk back down the chain: killing a
    -- hand's session must kill every finger's session that exists only because of it.
    via        INTEGER REFERENCES active_sudo(id),
    -- ★ WHY THIS SESSION EXISTS, recorded on the row rather than inferred (2026-10-23). `policy` is a
    -- borrow granted by a `sudoers` row; `lend` is a pen lending its OWN authority down, which needs
    -- no policy because parentage already grants it. Without this column "who can act as whom, and on
    -- whose authority?" can only be answered by joining back to a policy row that may since have been
    -- deleted — and the audit would lose the distinction exactly when it matters.
    source     TEXT NOT NULL CHECK (source IN ('policy', 'lend')),
    CHECK (powerful <> weak)
);
CREATE INDEX active_sudo_weak ON active_sudo(weak, powerful);
CREATE INDEX active_sudo_via ON active_sudo(via);
