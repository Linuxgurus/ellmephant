ALTER TABLE memory ADD COLUMN asserted_by TEXT NOT NULL DEFAULT 'anonymous';
ALTER TABLE memory ADD COLUMN origin TEXT;

UPDATE memory SET origin = source WHERE source IS NOT NULL AND source <> '';

CREATE INDEX idx_memory_asserted_by ON memory(asserted_by);

ALTER TABLE mail ADD COLUMN asserted_by TEXT NOT NULL DEFAULT 'anonymous';
UPDATE mail SET asserted_by = sender;
