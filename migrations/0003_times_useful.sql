-- 0003_times_useful — rename `was_useful` to `times_useful`.
--
-- Same counter, honest name: an integer count of explicit "this helped" marks. `visits`
-- stays as-is (a read count). SQLite >= 3.25 renames the column in place, data intact.

ALTER TABLE memory RENAME COLUMN was_useful TO times_useful;
