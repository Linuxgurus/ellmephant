-- MAIL SURROGATES (#153): a pen may lend its NAME to another pen, and every use is visible.
--
-- The defect this closes (2026-10-25, seq 566): `mail_send` took one sender field, and the MCP door
-- discarded the argument in favour of the proven pen. A letter written by James on Cline's
-- connection therefore went out as Cline's — silently — and the only correction was an addendum.
-- A refusal would have named the problem; a silent override hid it.
--
-- The shape: a grant is a ROW, not an environment variable. The environment is for secrets; who may
-- speak as whom is a relationship, and relationships need a grantor, a date, a scope, and a way to
-- be revoked without a redeploy.
--
-- No `sent_by` column on `mail`: `asserted_by` already means "the pen that proved it spoke", and it
-- has been on every row since 0023. A delegated letter is `sender = james` with
-- `asserted_by = cline`; a second column would be the same fact twice.

CREATE TABLE mail_surrogate (
    grantor    TEXT NOT NULL,                 -- whose name is lent (the `sender` on the letter)
    surrogate  TEXT NOT NULL,                 -- who may use it (the proven pen)
    -- Comma list of: send, read, ack. A grant to send is not a grant to read the box.
    scopes     TEXT NOT NULL DEFAULT 'send',
    granted_at TEXT NOT NULL DEFAULT (datetime('now')),
    expires_at TEXT,                          -- NULL = standing until revoked
    revoked_at TEXT,
    note       TEXT,
    PRIMARY KEY (grantor, surrogate)
);

CREATE INDEX mail_surrogate_by_surrogate ON mail_surrogate(surrogate, revoked_at);
