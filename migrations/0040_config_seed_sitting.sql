-- SEED: one recess SITTING is 20 minutes, and 40 is a bug (James, 2026-10-31).
--
--   recess_sitting_seconds   1200   the default length of ONE recess (20m)
--
-- The daily amount (`recess_daily_seconds`) is the BANK; this is how much of it one sitting draws.
-- A 2h day is therefore up to six sittings, which is the point: real work waits at most one sitting,
-- not two hours.
--
-- ★ THE HARD CEILING IS DERIVED, NOT STORED: `2 * sitting` (40m). A sitting that reaches it is a
-- RUNTIME BUG, not a longer recess — the soft bell at 20m is supposed to end the turn, and the hard
-- limit exists only so a stuck turn cannot outlive it invisibly. Storing the multiplier as a second
-- row would let the two disagree about what "double" means.
--
-- ★ THE TURN IS ALLOWED TO END. The soft bell does not cut a turn mid-thought; it stops NEW work and
-- lets the current one finish. The hard ceiling is the watchdog, not the plan.
--
-- Same ACL as the other recess rows: estate writes (through a borrow — see 0039), everyone reads.

INSERT OR IGNORE INTO config (key, value, set_audience, read_audience, source)
VALUES
    ('recess_sitting_seconds', '1200', 'pen:estate', 'public',
     'seed 0040 — James, 2026-10-31: one recess sitting is 20m by default; the hard ceiling is 2x that (40m) and reaching it is a runtime bug. The daily amount is the bank, this is one draw.');
