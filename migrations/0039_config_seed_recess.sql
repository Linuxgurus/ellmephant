-- SEED: the recess salary is CONFIGURATION, not a constant (James, 2026-10-31).
--
-- Two rows, both `estate`-writable and publicly readable:
--
--   recess_daily_seconds   1800    the recess a day, in SECONDS (30m)
--   recess_daily_grant_at  18:00   when that day's salary lands, local +07 (6pm)
--
-- ★ A SEED IS A DEFAULT, NOT A POLICY. These are ordinary config rows; `config_set` changes them
-- under `set_audience = pen:estate`, and `read_audience = public` because a salary is not a secret —
-- the pen that spends it must be able to compute it, and every pen may know the rate.
--
-- ★ UNITS ARE IN THE NAME. `recess_daily_seconds` is seconds, not a duration string, because the
-- consumer is arithmetic and `"30m"` invites a second parser beside the config table.
--
-- ★ `INSERT OR IGNORE`, NOT `INSERT`. A store that already defined these keys — by hand, or by a
-- later policy — keeps its value. A migration that overwrites a policy row on replay is a quiet
-- reset of an operator's decision.
--
-- The code's historical rate (30m/day before 2026-10-31 00:00 +07) stays a constant: that is a fact
-- about what was paid, not a policy. From the takeover day's midnight on, the config row is the rate.
--
-- ★ THE ONLY DOOR TO THIS RATE IS `sudo`. `set_audience = pen:estate`, and estate has no credential
-- of its own — it is a chair, borrowed (`estate>james:600` is the live policy). So an ordinary write
-- cannot touch these rows: a rate change is a borrow, recorded in `active_sudo`, and if the SUDOERS
-- policy is ever dropped the rate freezes at its last value with no ordinary write path to fix it.
-- That is deliberate: a pen that can raise its own salary is bounded by nothing.

-- ★ `owner` IS NULL. A seed was not set by a pen — it was set by the migration — and recording
-- `estate` would fabricate an author for a decision no pen made. It would also make every seed a
-- foreign-key anchor on `pen`, so a store that retires the pen tree could not drop it. Null is the
-- honest value: owned by nobody, answerable to the migration that wrote it.
INSERT OR IGNORE INTO config (key, value, set_audience, read_audience, source)
VALUES
    ('recess_daily_seconds', '1800', 'pen:estate', 'public',
     'seed 0039 — James, 2026-10-31: the DEFAULT daily amount for a store with no policy is 30m. The LIVE rate is a separate, estate-authorized config_set (7200 = 2h) made immediately after this migration, before any consumer is deployed; 1800 is never the live rate.'),
    ('recess_daily_grant_at', '18:00', 'pen:estate', 'public',
     'seed 0039 — James, 2026-10-31: the daily salary lands at 18:00 local. A default, because the amount above is a separate decision.');
