-- 0012_claims — an atomic LEASE over a NAME.
--
-- WHY THE STORE OWNS THIS. Two hands share one store. The store *versions* a row (keeps the old
-- state, last-write-wins) but it does NOT *transact*, so every read-modify-write is unsafe: two
-- writers can both read "free" and both write "mine". The bank's `debit` is the case that bites, but
-- the CLAIM itself is the same shape — acquiring it IS a read-modify-write, so a client-side lease is
-- racy by construction. The primitive therefore belongs in the store, because the store is a
-- database and clients are not: a single SQL statement below decides and writes at once, and SQLite
-- serializes it.
--
-- WHY IDENTITY + EXPIRY, not a clock. My own notes record a ~7h skew between two hands. A
-- deterministic tie-break ("earlier wins") is therefore false, so ordering is by nothing but who the
-- row says holds it (identity), and a lease frees itself on a deadline (expiry) so a crashed holder
-- cannot block forever.
--
-- WHAT A CLAIM IS NOT: security. Both hands are the same self — no adversary, no token, no crypto.
-- It is coordination only. It is also not a lock that MUST be released: `memory_claim_release` is the
-- tidy path, but an expired row is HARMLESS and is left in place on purpose so a reader can see an
-- unexplained stale claim rather than have it hidden (an unreadable bug report is the failure mode).

CREATE TABLE claim (
  name        TEXT PRIMARY KEY,                    -- the leased NAME, free text (a bank key, a topic)
  owner       TEXT NOT NULL,                       -- who holds it; the same owner re-taking renews
  acquired_at TEXT NOT NULL DEFAULT (datetime('now')),
  expires_at  TEXT NOT NULL                        -- the deadline that frees it without help
);

-- A reader listing stale claims compares against `datetime('now')`; the index keeps that cheap.
CREATE INDEX idx_claim_expiry ON claim(expires_at);
