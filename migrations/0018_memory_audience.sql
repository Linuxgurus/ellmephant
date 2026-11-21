
ALTER TABLE memory ADD COLUMN audience TEXT NOT NULL DEFAULT 'public';

CREATE INDEX memory_audience ON memory(audience);
