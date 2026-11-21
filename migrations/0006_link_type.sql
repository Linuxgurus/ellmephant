
ALTER TABLE memory_link ADD COLUMN to_type TEXT NOT NULL DEFAULT 'memory'
  CHECK (to_type IN ('memory', 'project'));

UPDATE memory_link SET to_type = 'project' WHERE to_key LIKE 'prj-%';
UPDATE memory_link SET to_key = substr(to_key, 5) WHERE to_type = 'project';

UPDATE project SET key = substr(key, 5) WHERE key LIKE 'prj-%';
UPDATE project_memory SET project_key = substr(project_key, 5) WHERE project_key LIKE 'prj-%';
