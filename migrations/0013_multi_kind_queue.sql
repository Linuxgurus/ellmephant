-- 0013_multi_kind_queue — the queue stops being a list of memories.
--
-- WHY: 0008 keyed the queue on `key` alone while carrying a `kind` column, so the key was the whole
-- identity and `kind` was dead. 0005 says the collision is NORMAL — "a project and a memory may share
-- one" ("sessions" is both) — so as soon as a second kind is encodable the two collide in the queue.
-- The identity becomes a KEYSLUG, `kind:key`, which is also the spelling a reference will use.
--
-- The queue also stops carrying a HASH and starts carrying the TEXT. `claim` used to JOIN `memory` to
-- re-render the document, which made the queue structurally memory-only whatever `kind` said; a queue
-- holding the rendered text needs no truth-table read and no per-kind branch in the worker.
--
-- CONSEQUENCE, deliberate: the dequeue guard stops being "does this hash still match" and becomes
-- "is this still the text that was claimed". Same protection (a write landing mid-embed survives),
-- and a re-request carrying identical text settles the job already in flight instead of spawning one.
--
-- `attempts` and `last_error` go. The queue holds no failure history: an embed that fails drops the
-- row and logs it, and the worker's refill rule (queue empty -> queue what has no vector) is the
-- retry. A queue that remembers failure is a database wearing a queue's clothes.
--
-- PRIORITY: two classes, not a scheduler. 3 = a live write, 5 = a bulk re-encode. Ascending, so a
-- live edit jumps every row of a running rebuild; `queued_at` breaks ties oldest-first inside a
-- class. Enqueue must never DEMOTE: `min(priority, …)` — which is why the write path is an upsert and
-- not INSERT OR REPLACE (REPLACE resets every column it is not given).
--
-- The queue is DERIVED and TRANSIENT, so it is dropped rather than migrated: a pending row costs one
-- re-embed and the refill rule re-derives anything actually missing.

DROP TABLE IF EXISTS encode_queue;

CREATE TABLE encode_queue (
  keyslug   TEXT PRIMARY KEY,              -- 'memory:foo' | 'project:bar'
  content   TEXT NOT NULL,                 -- the RENDERED document, frozen at enqueue
  priority  INTEGER NOT NULL DEFAULT 3,    -- 3 = live, 5 = bulk
  queued_at TEXT NOT NULL DEFAULT (datetime('now'))
);
