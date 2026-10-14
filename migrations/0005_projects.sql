-- 0005_projects — the project tables: the *structures* half of the store.
--
-- A memory (`memory`) is one body of closely related facts: written terse, one document vector
-- over it, matched by meaning -- and so voted, ranked and judged for coherence.
--
-- A project (`project`) is an activity completed over time: work with a beginning, a current state
-- and a place to stand, made of identity, fields and relations. Listed by field, read whole, and
-- never matched by meaning.
--
-- Those two are what the store knows. Everything else is DERIVED from them and can be rebuilt at
-- any time: `memory_vec` (the KNN index over the document vectors) and `memory_link` (the `[[key]]`
-- graph) are computed from the rows and hold no fact of their own. The project tables are truth in
-- the same sense `memory` is -- `project` carries a project's own fields, `project_memory` its one
-- relation to memories -- so nothing else needs keeping in sync.
--
-- Keys are PLAIN NAMES: a project and a memory may share one ("sessions" is a project; "sessions" is
-- also what the memories about it get called), so a key never carries its type. What carries it is
-- the *table* a row lives in, and the *reference* that names one -- `[[prj:x]]` in a memory body,
-- `memory_link.to_type` on an edge. See `0006` for the marker's one unambiguous spelling.
--
-- Two kinds of thing, two sets of tables, split where the machinery changes: "project = a memory
-- with a type" would ship cheaply and model nothing, because the machinery a fact needs -- votes,
-- a coherence verdict, an embedding -- is exactly what a structure has no use for.
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
-- edge, so it is a column (`mem_preload`): no `position` column shuffling a rowset back into
-- order. A key is `[a-z0-9-]` (slug.rs), so a comma can never occur inside one -- the delimiter
-- is safe by construction, not by convention.

-- `start_date`/`end_date` are the PROJECT's own dates (ISO, `YYYY-MM-DD`), distinct from the row's
-- `created`/`updated`: a project written down three weeks late started three weeks ago, and
-- `updated` moves on any write, so it cannot answer "when did this finish". `end_date` is neutral
-- about the ending -- `status` says HOW (done or aborted), the date only says WHEN -- which is why
-- it is not called `finished`/`completed`: an aborted project was never completed. Both are stamped
-- when not given and cleared/corrected by rule, in Rust (see store::project::upsert).
--
-- `owner` is a SET of names, comma-joined: 'Cline,James'. FREE TEXT -- no vocabulary, no `me`/`us`.
-- The store does not know who exists and must not pretend to: `me` resolves differently for every
-- reader, and a shared store cannot carry an indexical. `status` is validated in Rust, and `owner`
-- is *not*, because a CHECK cannot validate the *elements* of a comma list -- the same reason `tags`
-- has no CHECK either. Names are trimmed, deduped case-insensitively and sorted by the codec; a
-- blank `owner` means unassigned, which is a fact about the project, not a missing field.

CREATE TABLE project (
  key         TEXT PRIMARY KEY,                 -- a plain name; identity and link target
  title       TEXT NOT NULL,                    -- smart-caveman one-liner
  goal        TEXT NOT NULL DEFAULT '',         -- what this is for
  state       TEXT NOT NULL DEFAULT '',         -- mutable current-state prose
  status      TEXT NOT NULL DEFAULT 'todo',     -- todo|active|paused|done|aborted
  owner       TEXT NOT NULL DEFAULT '',         -- comma-joined names, e.g. 'Cline,James'
  tags        TEXT NOT NULL DEFAULT '',         -- comma-joined, sorted (the memory codec)
  mem_preload TEXT NOT NULL DEFAULT '',         -- comma-joined memory keys, ORDERED by priority
  created     TEXT NOT NULL DEFAULT (datetime('now')),
  updated     TEXT NOT NULL DEFAULT (datetime('now')),
  start_date  TEXT NOT NULL DEFAULT '',         -- when the WORK started (created = when the row did)
  end_date    TEXT NOT NULL DEFAULT '',         -- when it ENDED: done or aborted alike
  CHECK (status IN ('todo', 'active', 'paused', 'done', 'aborted'))
);

-- `preload` — project -> memory, ORDERED (the `mem_preload` column above). About ATTENTION: "to work
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
