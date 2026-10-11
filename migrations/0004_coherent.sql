-- 0004_coherent — the split-review verdict on `memory` (SPEC §9).
--
-- NULL = not yet judged; 1 = judged one coherent topic; 0 = judged a grab-bag worth
-- splitting. A nullable bool, not a two-state flag: an unjudged row is not the same as a
-- "coherent" one. An upsert resets it to NULL (the row's shape changed, so re-judge it).

ALTER TABLE memory ADD COLUMN coherent INTEGER CHECK (coherent IN (0, 1) OR coherent IS NULL);
