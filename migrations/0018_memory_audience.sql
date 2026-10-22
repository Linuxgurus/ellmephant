-- THE READ AUDIENCE (soma/ellmephant#54, #55).
--
-- A memory's `audience` is the one column that answers "who may this row be shown to". Two values:
--   'public'   — every reader, the default, and what every existing row becomes;
--   'pen:<x>'  — the pen that wrote it (lowercased, one spelling), and nobody else.
--
-- ★ IT IS A COLUMN AND NOT A TAG, and that is the whole safety argument. Tags are editable by any
-- caller through `memory_upsert`, so an audience carried in a tag would let any pen privatise a shared
-- row — or, worse, publish someone else's. Ownership is not audience: the `pen:` tag records care and
-- changes nothing about reads; this column changes reads and is written by the store from the proven
-- identity, never from an argument.
--
-- ★ NO SEPARATE BOOLEAN. A `private` flag beside an `audience` value would be two facts about one
-- thing, and the store has paid for that drift twice (a stamp on truth; two producers of the queue).
-- One column, one meaning: public, or exactly one pen.
--
-- Additive and reversible in the only sense that matters: every existing row is 'public', so a
-- binary that does not know this column reads the store exactly as it did before.

ALTER TABLE memory ADD COLUMN audience TEXT NOT NULL DEFAULT 'public';

-- Readers filter on it on every read path, and the index partitions on it.
CREATE INDEX memory_audience ON memory(audience);
