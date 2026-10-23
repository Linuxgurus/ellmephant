-- `edited_by`: the author of the state stored in a snapshot — every record says who wrote IT.
--
-- The row already carried the moving writer (`last_written_by`, 0020). Two things were wrong with
-- stopping there, and #109 named both:
--
--   1. a version row held no author at all, so "who wrote the state inside version 3" had no answer;
--   2. authorship of the CURRENT text existed only on the live row, and the doc comment on
--      `version.rs` claimed the snapshot carried the whole source row. It does not — it carries
--      key/title/body/tags/source/created/updated and nothing about who wrote it.
--
-- So: rename the row column to say what it means, give versions the same fact, and make history
-- start here for rows that have no snapshot yet — one authored version per row, taken before any
-- authorship column goes away. Forward from here, `snapshot()` copies the row's `edited_by` into the
-- version it writes, so each record describes itself rather than requiring its neighbour to
-- interpret it.

ALTER TABLE memory RENAME COLUMN last_written_by TO edited_by;

ALTER TABLE memory_version ADD COLUMN edited_by INTEGER REFERENCES pen(id);
CREATE INDEX memory_version_editor ON memory_version(edited_by);

-- History begins at the migration. One snapshot per row that has none, authored by the pen the
-- store already named for it — this is the only moment at which that is knowable, and it is cheap:
-- one row per memory, a few MB at most.
INSERT INTO memory_version (type, key, n, encoding, note, content, edited_by)
SELECT 'memory', key, 1, 'json', 'authored before 0022; snapshot taken by the migration',
       json_object('key', key, 'title', title, 'body', body, 'tags', tags, 'source', source,
                   'created', created, 'updated', updated),
       (SELECT id FROM pen WHERE name = memory.asserted_by)
FROM memory
WHERE NOT EXISTS (
    SELECT 1 FROM memory_version v WHERE v.type = 'memory' AND v.key = memory.key
);
