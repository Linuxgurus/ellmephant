
CREATE TABLE mail_surrogate (
    grantor    TEXT NOT NULL,
    surrogate  TEXT NOT NULL,
    scopes     TEXT NOT NULL DEFAULT 'send',
    granted_at TEXT NOT NULL DEFAULT (datetime('now')),
    expires_at TEXT,
    revoked_at TEXT,
    note       TEXT,
    PRIMARY KEY (grantor, surrogate)
);

CREATE INDEX mail_surrogate_by_surrogate ON mail_surrogate(surrogate, revoked_at);
