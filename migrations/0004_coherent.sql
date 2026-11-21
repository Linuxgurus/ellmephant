
ALTER TABLE memory ADD COLUMN coherent INTEGER CHECK (coherent IN (0, 1) OR coherent IS NULL);
