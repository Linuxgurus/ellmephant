

CREATE TABLE project (
  key         TEXT PRIMARY KEY,
  title       TEXT NOT NULL,
  goal        TEXT NOT NULL DEFAULT '',
  state       TEXT NOT NULL DEFAULT '',
  status      TEXT NOT NULL DEFAULT 'todo',
  owner       TEXT NOT NULL DEFAULT '',
  tags        TEXT NOT NULL DEFAULT '',
  mem_preload TEXT NOT NULL DEFAULT '',
  created     TEXT NOT NULL DEFAULT (datetime('now')),
  updated     TEXT NOT NULL DEFAULT (datetime('now')),
  start_date  TEXT NOT NULL DEFAULT '',
  end_date    TEXT NOT NULL DEFAULT '',
  CHECK (status IN ('todo', 'active', 'paused', 'done', 'aborted'))
);


CREATE TABLE project_memory (
  memory_key  TEXT PRIMARY KEY,
  project_key TEXT NOT NULL
);
CREATE INDEX idx_project_memory_project ON project_memory(project_key);
