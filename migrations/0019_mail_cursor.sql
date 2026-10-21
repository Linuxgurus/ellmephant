-- 0017_mail_cursor — a cursor is a POINTER, and a pointer has exactly one reader and one writer.
--
-- WHY A TABLE: before this there were three cursors for two readers, in three shapes, and two of them
-- were named as if they were the same thing (`mail-last-read` was the HAND's, in a memory;
-- `mail-last-read-james` was James's; the spine's lived in a file on the PVC). Two of the three were
-- not versioned, not searchable, and invisible to each other, and one of them had the unqualified name.
-- During the outage that name was one step from being advanced to fix a reader that it did not belong
-- to — a fix that would have looked applied and changed nothing (soma/ellmephant#79).
--
-- The shape follows the law: one row per (READER, BOX), where the reader is the PROVEN pen resolved
-- server-side from the connection — never a tool argument. A caller cannot advance someone else's
-- cursor, and a surrogate reading a box advances the surrogate's own row, which is what "the hand read
-- this" means.
--
-- `seq` is the highest letter the reader has DEALT WITH. Advancing it is the commit: a reader that dies
-- first sees the letter again. Monotonic per row (the store refuses a backward move), because a cursor
-- is a number, not a clock.

CREATE TABLE mail_cursor (
    reader    TEXT NOT NULL,
    recipient TEXT NOT NULL,
    seq       INTEGER NOT NULL,
    updated   TEXT NOT NULL DEFAULT (datetime('now')),
    PRIMARY KEY (reader, recipient)
);
