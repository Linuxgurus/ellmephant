
ALTER TABLE memory ADD COLUMN last_written_by TEXT;

UPDATE memory SET last_written_by = asserted_by WHERE last_written_by IS NULL;
