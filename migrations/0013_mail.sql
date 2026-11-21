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

CREATE INDEX idx_mail_inbox ON mail(recipient, seq);
