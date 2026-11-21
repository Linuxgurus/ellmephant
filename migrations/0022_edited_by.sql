
ALTER TABLE memory RENAME COLUMN last_written_by TO edited_by;

ALTER TABLE memory_version ADD COLUMN edited_by INTEGER REFERENCES pen(id);
CREATE INDEX memory_version_editor ON memory_version(edited_by);

ALTER TABLE memory_version ADD COLUMN synthetic INTEGER NOT NULL DEFAULT 0;

INSERT INTO memory_version (type, key, n, encoding, note, content, edited_by, synthetic)
SELECT 'memory', key, 1, 'json',
       'SYNTHETIC snapshot: the last writer before 0022, not the author of this edit',
       json_object('key', key, 'title', title, 'body', body, 'tags', tags, 'source', source,
                   'created', created, 'updated', updated),
       (SELECT id FROM pen WHERE name = memory.asserted_by),
       1
FROM memory
WHERE NOT EXISTS (
    SELECT 1 FROM memory_version v WHERE v.type = 'memory' AND v.key = memory.key
);
