-- 0002_telemetry — usage counters on `memory`.
--
-- `visits` counts reads (a `memory_get`/`show` of the row); `was_useful` is explicit
-- feedback an agent bumps when a memory actually helped its task. Both are runtime
-- properties, not schema-meaningful, so they are columns rather than an events table:
-- the counters are cheap to increment and read back with the row.

ALTER TABLE memory ADD COLUMN visits INTEGER NOT NULL DEFAULT 0;
ALTER TABLE memory ADD COLUMN was_useful INTEGER NOT NULL DEFAULT 0;
