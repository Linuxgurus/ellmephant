-- 0006_link_type — keys become plain names, and a LINK says which kind it points at.
--
-- WHY: a project and a memory may share a name -- "sessions" is a project, and "sessions" is also
-- what memories about it get called. That is normal, not pathological, so the answer cannot be to
-- forbid it. The type leaves the NAME and moves to the REFERENCE: a key is a plain name, each
-- table IS its row's type, and `memory_link.to_type` records which table a `to_key` names.
--
-- KEYS: `project.key` loses its old `prj-` prefix here, and so does every key that described a
-- project (`project_memory.project_key`, and each `memory_link.to_key` typed as one). **This is the
-- last moment that rewrite is possible**: this migration is the only thing that knows `prj-` meant
-- "project". From here on, `prj-foo` is just a memory called `prj-foo`.
--
-- IN PROSE the marker survives in its unambiguous form, `[[prj:chronos]]`. Links live in memory
-- bodies, so a bare `[[key]]` means a memory and only a project needs saying -- and `:` can never
-- occur in a slug (`[a-z0-9-]`, see slug.rs), so no name can ever squat on it.

ALTER TABLE memory_link ADD COLUMN to_type TEXT NOT NULL DEFAULT 'memory'
  CHECK (to_type IN ('memory', 'project'));

-- Existing edges: nothing but a project ever produced a `prj-` key, so the typing is exact.
UPDATE memory_link SET to_type = 'project' WHERE to_key LIKE 'prj-%';
UPDATE memory_link SET to_key = substr(to_key, 5) WHERE to_type = 'project';

-- The structures side, for the same reason: a store written by an earlier v5 build holds prefixed
-- project keys, and the code now expects plain ones.
UPDATE project SET key = substr(key, 5) WHERE key LIKE 'prj-%';
UPDATE project_memory SET project_key = substr(project_key, 5) WHERE project_key LIKE 'prj-%';
