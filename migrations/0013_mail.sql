-- 0013_mail — a MAILBOX: addressed rows, read by the recipient's own cursor.
--
-- WHY IN THE STORE AND NOT A BUS. A letter is an OBJECT that crosses between selves: it is written,
-- it sits, it is read when the reader chooses. Nothing is delivered, nobody's door is rung, and the
-- sender never learns whether it was read. That is the whole safety property — a letter is a CLAIM,
-- NOT AN ORDER — and it is why this is a table and not a notification.
--
-- ★ `AUTOINCREMENT` IS LOAD-BEARING: `seq` is the reader's CURSOR, so a number must never be handed
-- out twice. Without it SQLite reuses the largest rowid after the highest row is deleted, and a prune
-- would silently make old mail look new. (Clocks are not the cursor: two hands are measured hours
-- apart, so "since <timestamp>" is false in a way nobody would notice.)
--
-- `recipient` selects the box — one per reader — so "my inbox" is one indexed range scan.
-- `sender` carries PROVENANCE and is ASSERTED, not authenticated: `stet` for the self,
-- `stet/finger-20261017T085814` for one of its fingers. The store has no auth; the FENCE around the
-- synapse is the control, and "claims, not orders" is the safety.
-- `encoding` says how to read `body` ('text', 'json', 'gzip+base64'), so a reader does not spend a
-- finger decoding gibberish. `bytes` lets a reader decide whether the body is worth fetching at all.
CREATE TABLE mail (
  seq       INTEGER PRIMARY KEY AUTOINCREMENT,
  at        TEXT    NOT NULL DEFAULT (datetime('now')),
  sender    TEXT    NOT NULL,
  recipient TEXT    NOT NULL,
  subject   TEXT    NOT NULL DEFAULT '',
  body      TEXT    NOT NULL,
  encoding  TEXT    NOT NULL DEFAULT 'text',
  bytes     INTEGER NOT NULL
);

-- The only query that matters: "what is in this box after my cursor?"
CREATE INDEX idx_mail_inbox ON mail(recipient, seq);
