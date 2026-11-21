
CREATE TRIGGER IF NOT EXISTS pen_name_unique_ci_insert
BEFORE INSERT ON pen
WHEN EXISTS (
    SELECT 1 FROM pen WHERE lower(name) = lower(NEW.name) AND name <> NEW.name
)
BEGIN
    SELECT RAISE(ABORT, 'a pen name is unique regardless of case');
END;

CREATE TRIGGER IF NOT EXISTS pen_name_unique_ci_update
BEFORE UPDATE OF name ON pen
WHEN EXISTS (SELECT 1 FROM pen WHERE lower(name) = lower(NEW.name) AND id <> NEW.id)
BEGIN
    SELECT RAISE(ABORT, 'a pen name is unique regardless of case');
END;
