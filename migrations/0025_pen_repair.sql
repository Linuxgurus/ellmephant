

DELETE FROM active_sudo WHERE powerful IN (SELECT id FROM pen WHERE lower(name) = 'anonymous')
                           OR weak     IN (SELECT id FROM pen WHERE lower(name) = 'anonymous');
DELETE FROM sudoers    WHERE powerful IN (SELECT id FROM pen WHERE lower(name) = 'anonymous')
                           OR weak     IN (SELECT id FROM pen WHERE lower(name) = 'anonymous');

UPDATE memory_version SET edited_by = NULL
 WHERE edited_by IN (SELECT id FROM pen WHERE lower(name) = 'anonymous');

UPDATE pen_event SET pen_id  = NULL WHERE pen_id  IN (SELECT id FROM pen WHERE lower(name) = 'anonymous');
UPDATE pen_event SET from_pen = NULL WHERE from_pen IN (SELECT id FROM pen WHERE lower(name) = 'anonymous');
UPDATE pen_event SET to_pen  = NULL WHERE to_pen  IN (SELECT id FROM pen WHERE lower(name) = 'anonymous');

UPDATE memory SET owner = NULL
 WHERE owner IN (SELECT id FROM pen WHERE lower(name) = 'anonymous');

DELETE FROM pen WHERE lower(name) = 'anonymous';
