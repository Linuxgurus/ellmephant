-- CASE-INSENSITIVE, CASE-PRESERVING (James, 2026-10-24).
--
-- A pen's NAME is already compared case-insensitively by every layer above this table: the identity
-- table matches claims with `eq_ignore_ascii_case`, and `audience::pen()` lowercases before building an
-- audience. `pen.name UNIQUE` was not. So `James` and `james` could BOTH exist — two rows that every
-- door treats as one pen, neither of which can be addressed unambiguously, and the row you authenticated
-- as need not be the row that owns your memories.
--
-- The fix is not to fold the column: a name is an address, and people spell addresses their own way.
-- The STORED value keeps its spelling; only UNIQUENESS folds.
--
-- ★ TRIGGERS, NOT A UNIQUE INDEX, and this is the part worth explaining. A unique index would be
-- cheaper, but creating it FAILS on a store that already holds such a pair, and a failed migration is a
-- store that will not open — James's law: a hobbled Cline and Stet beats no Cline and Stet. A trigger is
-- created without validating existing rows, so the invariant binds every FUTURE write while an old
-- collision remains visible in `doctor` for a human to resolve (which of the two answers for the rows is
-- a decision, not a migration).
--
-- The unique index is added after by `store::ensure_name_uniqueness` — guarded, logged, and only once the
-- table is actually clean.

-- ★ `AND name <> NEW.name` is deliberate: an EXACT duplicate is the UNIQUE constraint's job (and
-- `INSERT OR IGNORE` is allowed to ignore it, which seeding relies on). This trigger exists only for the
-- pair the schema could not see — two spellings of one name.
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
