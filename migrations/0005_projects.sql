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
--
-- ONE tie table, not two. A relation earns a table only when its invariant is CROSS-ROW.
-- `contains` has one -- a memory has at most one home, anywhere, which a column cannot express
-- because a column cannot see other rows -- so it gets a table. `preload` has none (a memory
-- may sit in many projects' lists) and its order is a property of the LIST rather than of each
-- edge, so it is a column: no `position` column shuffling a rowset back into order. A key is
-- `[a-z0-9-]` (slug.rs), so a comma can never occur inside one -- the delimiter is safe by
-- construction, not by convention.

-- `owner` is a SET of names, comma-joined: 'Cline,James'. FREE TEXT -- no vocabulary, no `me`/`us`.
-- The store does not know who exists and must not pretend to: `me` resolves differently for every
-- reader, and a shared store cannot carry an indexical. `status` is validated in Rust, and `owner`
-- is *not*, because a CHECK cannot validate the *elements* of a comma list -- the same reason `tags`
-- has no CHECK either. Names are trimmed, deduped case-insensitively and sorted by the codec; a
-- blank `owner` means unassigned, which is a fact about the project, not a missing field.

CREATE TABLE project (
  key     TEXT PRIMARY KEY,                     -- 'prj-…'; identity and link target
  title   TEXT NOT NULL,                        -- smart-caveman one-liner
  goal    TEXT NOT NULL DEFAULT '',             -- what this is for
  state   TEXT NOT NULL DEFAULT '',             -- mutable current-state prose
  status  TEXT NOT NULL DEFAULT 'todo',         -- todo|active|paused|done|aborted
  owner   TEXT NOT NULL DEFAULT '',             -- comma-joined names, e.g. 'Cline,James'
  tags    TEXT NOT NULL DEFAULT '',             -- comma-joined, sorted (the memory codec)
  preload TEXT NOT NULL DEFAULT '',             -- comma-joined keys, ORDERED by priority
  created TEXT NOT NULL DEFAULT (datetime('now')),
  updated TEXT NOT NULL DEFAULT (datetime('now')),
  CHECK (status IN ('todo', 'active', 'paused', 'done', 'aborted'))
);

-- `preload` — project -> memory, ORDERED (the `preload` column above). About ATTENTION: "to work
-- in here, load these." A project preloads memories it did NOT author (conventions, boot notes);
-- it is the boot set, generalized to every project.

-- `contains` — memory -> project. About PROVENANCE: "born here / belongs here." One home, not
-- many, which is why `memory_key` is the PRIMARY KEY: a second home is a constraint failure,
-- not a judgement call. (`preload` and `contains` overlap but neither contains the other:
-- collapse them and you get noise or starvation.)
CREATE TABLE project_memory (
  memory_key  TEXT PRIMARY KEY,
  project_key TEXT NOT NULL
);
CREATE INDEX idx_project_memory_project ON project_memory(project_key);
