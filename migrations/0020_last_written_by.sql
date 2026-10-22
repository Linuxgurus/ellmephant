-- 0020_last_written_by — AUTHORSHIP IS IMMUTABLE; the writer is the fact that moves (soma/ellmephant#107).
--
-- Since 0014 there has been ONE provenance column, `asserted_by`, and the store stamped it on every
-- write. For a body edit that was hidden by the version snapshot that write also took. For a TAG edit
-- it was not hidden at all: tags create no version, so re-filing a row silently moved its authorship
-- to whoever filed it, with no record and no way back (measured 2026-10-22: a tag pass over 19 rows
-- restamped them all).
--
-- ★ TWO FACTS, TWO COLUMNS. "Who asserted this" is a claim about the DOCUMENT and does not move; "who
-- wrote it last" moves with every write. Storing them in one column forced a choice between lying
-- about the author and lying about the writer — and the store chose the author, silently.
--
-- ★ `asserted_by` IS NOW WRITE-ONCE (set at INSERT, preserved on UPDATE by the merge path). This
-- column carries the rest: a tag pass, a re-file, a correction — every write names itself here.
--
-- The backfill is honest rather than invented: before this migration every write restamped
-- `asserted_by`, so the stored author IS the last writer. That is the bug's footprint, and it is the
-- only value this column can truthfully take for existing rows.
--
-- Additive: a binary that does not know this column reads and writes the store as it did before.

ALTER TABLE memory ADD COLUMN last_written_by TEXT;

UPDATE memory SET last_written_by = asserted_by WHERE last_written_by IS NULL;
