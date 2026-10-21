-- 0014_provenance — WHO asserted it, and WHERE it came from.
--
-- WHY. `source` was free text the writer composed: it could say anything, and nothing could tell
-- "James told me this" from "a model guessed, then typed a plausible-looking provenance". That is
-- survivable while one writer fills the store and fatal the moment a browser chat and a hand write
-- into it together — a claim needs an author.
--
-- `asserted_by` is STAMPED BY THE STORE from the connection's resolved identity (`X-Stet-From`,
-- proved by `X-Stet-Auth`) — never taken from an argument, so a forged header lands as the claim it
-- is rather than as fact. `origin` is what the caller DECLARES about where the information came
-- from: a URL, a file, a command, "James directly", "my inference". A relayer names the truth's
-- home. It is a promise, not a proof, and the two columns are kept apart for exactly that reason.
--
-- `source` stays. It is the human-readable composition these two were always trying to be, and
-- every tool that reads it keeps working.
ALTER TABLE memory ADD COLUMN asserted_by TEXT NOT NULL DEFAULT 'anonymous';
ALTER TABLE memory ADD COLUMN origin TEXT;

-- Backfill: whatever `source` said is the best available guess at an origin. The author was never
-- recorded, so `anonymous` is the truth rather than an insult.
UPDATE memory SET origin = source WHERE source IS NOT NULL AND source <> '';

-- "what did a human tell me" is the query this exists for; it should not scan the store.
CREATE INDEX idx_memory_asserted_by ON memory(asserted_by);

-- The mailbox's `sender` was ASSERTED, not authenticated (0013 says so in as many words). Keep it,
-- and record who the store believes sent it. Today that is the same string, so this backfill is
-- honest; future writes set both from the resolved identity.
ALTER TABLE mail ADD COLUMN asserted_by TEXT NOT NULL DEFAULT 'anonymous';
UPDATE mail SET asserted_by = sender;
