-- CREDENTIALS ARE ROWS (2026-10-30): a pen's secret leaves the environment.
--
-- Until now a name was proved by a process variable, `ELLMEPHANT_IDENTITIES` (name:secret pairs,
-- re-read every call). That gave two registries of one name: `pen` answered existence and ancestry,
-- the env answered authentication — and they could disagree. `twine` has a row and no secret;
-- `estate` has a row and must never have one. Worse, a RETIRED pen kept its env secret, because
-- nothing on the authentication path ever looked at `ceased_at`.
--
-- This table is the second registry folded into the first. A credential belongs to a pen, so it can
-- be revoked by the pen's retirement, rotated in place, and counted without parsing a string.
--
-- ★ THE SECRET IS NEVER STORED. `hash` is the lowercase hex of SHA-256 over the secret, and the
-- plaintext exists only in the response that issued it. The secrets are 32 random bytes, so entropy
-- is the defence and a password KDF would only make a per-request auth path expensive.
--
-- ★ A MISSING ROW IS THE HONEST STATE of a pen that cannot authenticate: `estate` (a chair, only
-- borrowed) and any name a migration seeded from history that has stopped writing.
--
-- ★ WHY NOT COLUMNS ON `pen`: `SELECT * FROM pen` is the obvious debugging query, and auth material
-- must not ride along into a log line or a paste. One row per pen, one fact, no null-heavy columns.

CREATE TABLE pen_credential (
    pen_id     INTEGER PRIMARY KEY REFERENCES pen(id) ON DELETE CASCADE,
    -- lowercase hex sha256 of the 32-byte secret.
    hash       TEXT NOT NULL,
    issued_at  TEXT NOT NULL DEFAULT (datetime('now')),
    -- WHO issued it: the minting parent, or NULL for provisioning / the one-shot env import.
    issued_by  INTEGER REFERENCES pen(id),
    -- NULL until the first rotation. A rotation replaces the hash; the old secret dies on the next
    -- call because authentication reads this row every time and caches nothing.
    rotated_at TEXT
);
