-- 0005_projects — the project tables: the *structures* half of the store.
--
-- A project is NOT a memory. Memories are FACTS -- voted, ranked, judged for coherence,
-- searched by meaning. Projects are STRUCTURES -- identity, fields, relations; none of the
-- fact-machinery applies. Two kinds of thing, two tables (the model wins over the cheap
-- "project = memory + type"). Keys carry the `prj-` prefix so the type is visible wherever a
-- key travels, including into `memory_link.to_key`, which is a TEXT key and not an FK.
--
-- Deliberately NO foreign keys, exactly like `memory_link.to_key`: these are by-key edges, so
-- a forward reference is possible and rot is *detected* by the audit instead of raising a
-- broken-FK error. `key` is the primary key -- a project has no rowid-derived identity and no
-- vector, so the key IS the identity.

CREATE TABLE project (
  key     TEXT PRIMARY KEY,                     -- 'prj-…'; identity and link target
  title   TEXT NOT NULL,                        -- smart-caveman one-liner
  goal    TEXT NOT NULL DEFAULT '',             -- what this is for
  state   TEXT NOT NULL DEFAULT '',             -- mutable current-state prose
  status  TEXT NOT NULL DEFAULT 'todo',         -- todo|active|paused|done|dead
  owner   TEXT NOT NULL DEFAULT 'us',           -- me|James|us
  tags    TEXT NOT NULL DEFAULT '',             -- comma-joined, sorted (the memory codec)
  created TEXT NOT NULL DEFAULT (datetime('now')),
  updated TEXT NOT NULL DEFAULT (datetime('now')),
  CHECK (status IN ('todo', 'active', 'paused', 'done', 'dead')),
  CHECK (owner IN ('me', 'James', 'us'))
);

-- `preload` — project -> memory, ORDERED. About ATTENTION: "to work in here, load these."
-- A project preloads memories it did NOT author (conventions, boot notes); it is the boot
-- set, generalized to every project.
CREATE TABLE project_preload (
  project_key TEXT    NOT NULL,
  memory_key  TEXT    NOT NULL,
  position    INTEGER NOT NULL,
  PRIMARY KEY (project_key, memory_key)
) WITHOUT ROWID;
CREATE INDEX idx_preload_order ON project_preload(project_key, position);

-- `contains` — memory -> project. About PROVENANCE: "born here / belongs here." One home, not
-- many, which is why `memory_key` is the PRIMARY KEY: a second home is a constraint failure,
-- not a judgement call. (`preload` and `contains` overlap but neither contains the other:
-- collapse them and you get noise or starvation.)
CREATE TABLE project_memory (
  memory_key  TEXT PRIMARY KEY,
  project_key TEXT NOT NULL
);
CREATE INDEX idx_project_memory_project ON project_memory(project_key);
