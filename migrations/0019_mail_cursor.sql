
CREATE TABLE mail_cursor (
    reader    TEXT NOT NULL,
    recipient TEXT NOT NULL,
    seq       INTEGER NOT NULL,
    updated   TEXT NOT NULL DEFAULT (datetime('now')),
    PRIMARY KEY (reader, recipient)
);
