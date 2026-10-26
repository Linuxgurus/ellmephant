-- ★ WHO ENDED A LEASE THEY DID NOT HOLD (soma/spine#43 fix 4, the audit half).
--
-- An ancestor may release a descendant's lease so a dead process cannot lock the estate from the
-- inside — but an escape hatch with no record is indistinguishable from a leak. The claim row is
-- DELETEed on release, so the attribution cannot live there; this table is the durable half of the
-- answer `release_by` already returns.
CREATE TABLE claim_release_log (
    id          INTEGER PRIMARY KEY AUTOINCREMENT,
    at          TEXT NOT NULL DEFAULT (datetime('now')),
    name        TEXT NOT NULL,
    -- Who held it when it was ended.
    owner       TEXT NOT NULL,
    -- Who ended it — always an ANCESTOR of the owner; a holder's own release writes nothing here.
    released_by TEXT NOT NULL,
    -- Required, never empty: the reason is the difference between an act and an accident.
    reason      TEXT NOT NULL
);

CREATE INDEX claim_release_log_at ON claim_release_log(at);
CREATE INDEX claim_release_log_name ON claim_release_log(name);
