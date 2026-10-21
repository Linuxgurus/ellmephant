# Multi-kind embeddings — the index moves out of the truth tables

**Status:** proposal, ready for review (2026-10-21). No code changed. Supersedes the `encode_hash` /
`doc_hash` / `encode_pending` machinery and the `encode_queue.key` identity. Companion to
`ENCODE-WORKER.md` (which removed the model call from under the lock; this removes the *bookkeeping*
from the truth rows). Evidence that the design's assumptions hold is in `tests/vec_kind_filter.rs`
and in "Verified" at the end.

## Why — three defects, one cause

1. **`encode_queue` cannot hold two kinds.** `0008` shipped
   `key TEXT PRIMARY KEY, kind TEXT NOT NULL DEFAULT 'memory'` — the key is the whole identity, so
   `kind` is a dead column. `0005` states the collision will happen: *"a project and a memory may
   share one"* (`sessions` is a project; `sessions` is also what the memories about it get called).
2. **`claim` joins `memory`.** `FROM encode_queue q JOIN memory m ON m.key = q.key` re-renders the
   document from the truth table, so the queue is *structurally* memory-only regardless of `kind`.
3. **The stamp lives on a truth row.** `memory.encode_hash` plus the `encode_pending` sentinel is a
   derived stamp written on truth, and that is what produced two silent no-ops (a reset queue that
   drained embedding nothing; a hash that claimed "indexed" over a torn index). The lesson already
   recorded — *a stamp describing a derived index must be derived FROM that index* — is answered
   better by **no stamp at all** than by a better stamp.

Multiple document types at one time is the feature; these three are the reason it cannot be bolted on.

## The design: three tables, one job each

| table | job | may claim |
|---|---|---|
| `memory`, `project` | truth | what is true |
| `encode_queue` | work | this text needs embedding, since about then |
| vec0 index | derived index | this vector exists |

None of them describes another. Every fact is stored once, where it is authoritative.

### 1. Truth tables (unchanged, minus one column)

`memory` loses `encode_hash` entirely. Nothing in the truth layer records encoding state.

### 2. The queue — `(keyslug, content, priority, queued_at)`

```sql
CREATE TABLE encode_queue (
  keyslug   TEXT PRIMARY KEY,              -- 'memory:foo' | 'project:bar'
  content   TEXT NOT NULL,                 -- RENDERED text, frozen at enqueue
  priority  INTEGER NOT NULL DEFAULT 3,    -- 3 = live write, 5 = bulk re-encode
  queued_at TEXT NOT NULL DEFAULT (datetime('now'))
);

-- enqueue: coalesces to one row per document; never DEMOTES an urgent row to bulk
INSERT INTO encode_queue(keyslug, content, priority) VALUES(?1, ?2, ?3)
ON CONFLICT(keyslug) DO UPDATE
  SET content   = excluded.content,
      priority  = min(priority, excluded.priority),
      queued_at = datetime('now');

-- pop: live work before bulk work, oldest first inside each class
SELECT keyslug, content FROM encode_queue ORDER BY priority, queued_at LIMIT 1;

-- settle: only if this is still the work item that was claimed
DELETE FROM encode_queue WHERE keyslug = ?1 AND content = ?2;
```

- **Two classes, not a scheduling system.** `PRIORITY_LIVE = 3`, `PRIORITY_BULK = 5`; ascending, so a
  live edit jumps every row of a running rebuild. 3 leaves room on both sides for a future "more
  urgent than normal" caller. Within a class it is oldest-first — the fairness that LIFO lost.
- **Consequence: no chunking.** A rebuild may put all ~300 rows in the queue at priority 5 and a live
  write still goes first. The queue's size is bounded by *what is missing*, not by a batch size, and
  the previous 16-document producer is deleted.
- **`INSERT OR REPLACE` is wrong here** (measured): it resets every column, so a bulk sweep would
  demote a live row from 3 to 5. The explicit upsert with `min(priority, …)` preserves it.
- `keyslug` is safe **by construction**: keys are `[a-z0-9-]+` (`store/slug.rs`), so `:` cannot occur
  inside one. (The `prj:` marker in a *reference* already uses the same spelling.)
- `content` is the **rendered** text, not a body: rendering at enqueue keeps the per-kind branch out
  of the worker, and there is no truth-table read on the claim path at all.
- The settle guard is **content equality, not a key match**: an embed runs outside the lock, so a
  write can land mid-flight. Content differs ⇒ the DELETE misses ⇒ the row survives and is embedded
  next pass. A write can never be swallowed. If the re-request carries *identical* text, the guard
  matches and the in-flight embed settles it — no duplicate embed, no lost write.
- No `kind`, no `doc_hash`, no `attempts`, no `last_error`, no `rev`, no per-row `projection`, no
  index. `COUNT(*)` is the backlog; `MIN(queued_at)` is the age of the oldest outstanding request.
- **Exactly one consumer.** There is no in-flight marker, so two drains would embed the same row
  twice. One drain loop owns the queue — today `spawn_encode_worker` in `src/mcp/mod.rs`. This is a
  constraint, not an oversight.

### 3. Rendering is each kind's own decision

A kind knows what its own text *means*; the worker does not, and must not. So each kind renders its
own document, beside its own row type:

```rust
pub fn document(title: &str, goal: &str, state: &str) -> String   // project: no status, no preload
pub fn document(key: &str, title: &str, body: &str) -> String     // memory:  no tags
```

Exposed through one `Document` trait — `kind()`, `document()`, `fingerprint()` — so the queue and the
worker stay kind-blind.

**The signature is the enforcement, not the comment.** The existing memory projection can claim
"tags are FILING, not meaning" because there is *no tag argument to leak one in*. Every renderer
keeps that discipline or each kind slowly starts embedding its own filing data. For a project the
tempting mistakes are `status`, `owner`, the dates, and above all `mem_preload` — a list of *other
documents' keys*, so including it makes a project's vector partly a bag of its neighbours' slugs.

**Fingerprint per kind.** Today `fingerprint()` hashes the *output* of the projection on a fixed
probe, so it follows the code with no version constant to remember. Per kind that is
`meta.PROJECTION:<kind>`, each hashing its own probe — and the probe must populate **every** field
the renderer reads (including one that exercises link-stripping), or a change to an optional field
goes undeclared. A renderer must stay **pure**: no clock, no external lookup; the text is a function
of the row.

One consequence for search: results from different kinds are now produced by different renderers, and
remain comparable only because they share a model and dimension. That is the intent — each kind
renders its *semantic* fields, and those are what a phrase should match.

### 4. The index — the type is a **partition key**

`vec0` takes an **INTEGER** primary key and freezes its dimension at `CREATE` (`index/vec.rs`), so
`(kind, key)` cannot be the vec0 key directly. And a plain column is not enough: **measured
2026-10-21 against the pinned 0.1.9**, filtering a KNN on an *auxiliary* column is refused —

```
An illegal WHERE constraint was provided on a vec0 auxiliary column in a KNN query.
```

The filter has to be a **partition key**, which `0.1.9` supports:

```sql
CREATE VIRTUAL TABLE document_vec USING vec0(
  vector_id INTEGER PRIMARY KEY,
  embedding float[N] distance_metric=cosine,
  type      text PARTITION KEY,   -- 'memory' | 'project' | ...
  +key      TEXT                  -- auxiliary: stored and updated, never filtered on
);
```

**A deleted document's vector is moved aside, not dropped** (James, 2026-10-21: the embedding is
semantic, not the source of truth — stale costs relevance, never correctness):

```sql
CREATE VIRTUAL TABLE document_vec_deleted USING vec0(   -- the tombstone index
  vector_id INTEGER PRIMARY KEY,
  embedding float[N] distance_metric=cosine,
  type      text PARTITION KEY,
  +key      TEXT,
  +deleted_at TEXT
);
```

A sibling table rather than a `+deleted` flag, because of the measurement above: an auxiliary column
**cannot** be filtered inside a KNN, so excluding tombstones from normal search would mean
over-fetching and dropping them — which degrades exactly when the tombstones are numerous. The live
table stays exact; the deleted search gets its own table. A delete becomes *read the vector, insert
the tombstone, remove the live row*, inside the document's transaction, and the copy path is already
measured.

Measured semantics, all four checked:

- `... WHERE embedding MATCH ? AND k = ? AND type = 'project'` → `k` is **per partition**, so a
  filtered search gets the true nearest *within* the type. (Not a post-filter.)
- `... WHERE embedding MATCH ? AND k = ?` (no type) → `k` is **global**. Cross-kind search is honest.
- `type` cannot be UPDATEd (*"UPDATE on partition key columns are not supported yet"*) — harmless,
  since a keyslug's kind never changes. A row that must change kind is deleted and re-inserted.
- The keyslug is derivable in SQL, `type || ':' || key`, so the table stores no redundant identity.

One vec0 table = **one dimension and one distance metric**. A per-kind *renderer* does not force a
second table — same width, same metric, same model — so **one live table is the default**, plus its
tombstone sibling (same width, same metric; the split is liveness, not kind).
What isolation requires is not a table but a **per-kind fingerprint**: `meta.PROJECTION:<type>`, so
editing the project renderer re-enqueues projects and nothing else. Derive it from the kind — never
store it per row. A separate table per kind is warranted only when a kind needs a different **model or
dimension**, and that rebuild lands on all kinds at once (accepted: not worth splitting for).

### 5. Keyslug spelling

`keyslug = format!("{}:{}", kind.as_str(), key)` where `Kind` is the enum already in
`store/links.rs` — `as_str()` returns the **table names** (`memory`, `project`). So the spelling is not
a new convention: it is the existing declaration, and a kind is *a table that holds documents* (has a
`key`, a renderer, a delete path). `memory_link`, `memory_version`, `encode_queue` and the index
tables are excluded by that definition rather than by a list to maintain.

Prose references (`[[key]]`, `[[prj:key]]`) are a **separate migration and not part of this work** —
agreed with James, 2026-10-21. When it happens the marker becomes `[[type:key]]`, identical to a
keyslug, which makes the reference grammar and the queue identity the same string. One consequence to
know now: `LINK_RE` does not match `[[type:key]]` today, and `strip_links` therefore would not strip
it — so that patch also moves the memory renderer's fingerprint, which correctly forces a memory
rebuild.

## The consistency contract

- **`memory_get` / `project_get` by key are never stale** — they read truth, and truth is written
  before any queue row exists.
- **Search is eventually consistent.** The honest statement is *"lag is bounded by the queue"*, not
  *"a few seconds"*: ~5–11 s per document embed (debug, measured), and the first model load is
  1.2 GB. A fresh write is findable by search once its vector exists, and not before.
- **Live before bulk, oldest first inside a class.** A rebuild may dump every missing document into
  the queue at `PRIORITY_BULK`; live writes at `PRIORITY_LIVE` still go first, so nothing is held up
  and no batch size is needed.
- **Nothing owns the backfill — the worker does, statelessly.** One rule: *when the queue is empty,
  find documents with no vector and queue all of them at bulk priority.* A rebuild is therefore
  **delete that type's vectors**; the drain loop refills and stops on its own when nothing is
  missing. No cursor, no saved position, no resume logic: a crash or a restore just looks again and
  continues. The same rule repairs a torn index and catches a vector deleted by hand, so one
  mechanism replaces three.
- **A failed embed keeps its entry; the caller steps over it.** The entry is the record that a vector
  is owed, so removing it would hide a stale vector (see *Failure handling*). The worker holds the
  failed keyslugs in memory and skips them, so a permanent failure becomes a backlog that does not
  drain — visible — rather than a hot loop or a silence. In memory only, so a restart retries.
- **A rebuild never makes a document unsearchable.** It enqueues replacements at bulk priority and
  overwrites on settle; the old vector answers until the new one lands. The single exception is a
  **dimension change**, where the old vectors are unreadable by width and the table must be recreated.
- **A delete does not erase knowledge of the document.** The vector moves to the tombstone index and
  stops appearing in normal search; it is reachable only by asking — `semantic_search(..., deleted =
  true)`, which returns keys and never bodies. A kind with a retention promise (`mail`) purges instead.
- **`?wait=true`** stays: the synchronous door for a caller that needs a vector now. It is what makes
  eventual consistency safe rather than a gamble.

## Failure handling — the entry stays, the caller steps over it

★ **Corrected 2026-10-21 after review (stet): an entry is KEPT when its embed fails.** Dropping it was
wrong, and wrong in the exact way this design is supposed to abolish: **"no vector" is not "stale
vector".** The refill rule asks whether a document *has* a vector; a document whose text moved and
whose embed then failed has one — the previous text's — so `missing()` cannot see it. The entry was
therefore the only record that the vector was owed, and dropping it left a stale vector with nothing
anywhere saying so. That is the recidivism `encode_hash` used to prevent, reintroduced by deleting it.

So: **nothing removes an entry except a successful settle**, and the invariant is
**a document's vector is current iff no queue entry names it**. That is stronger than the hash it
replaces, because it survives a torn index too — the index loses a vector, the refill rule re-names it.

What stops a permanent failure from spinning: the caller keeps the failed keyslugs **in memory** and
skips them (`next(conn, skip)` steps over them), so a restart retries at once and a hot loop is
impossible. `attempts`/`last_error` still do not exist: the cost of a failure is a backlog that will
not drain — which `/health` reports as an age that never resets — plus one WARN per pass. Never the
document's content in the log; a letter's text must not land in a log.

## Search: what an unfiltered query means

★ Clarified 2026-10-21 after review (stet): `k` is per-partition when a `type` is given and global
when it is not, so an unfiltered search would be **silently dominated by the largest kind** —
memories vastly outnumber projects, and the query would become a memory search wearing a store search
costume. The rule, stated so nobody has to infer it:

- **`semantic_search(query, type)`** — that kind's `k`, that kind's distance floor.
- **`semantic_search(query)`** — **per-kind**: the same `k` *within each kind*, each kind's own floor,
  then merged by distance. Every kind gets asked; none can crowd another out.
- `memory_search` remains the memory-only alias, unchanged.

Cost: N queries instead of one, N being the number of kinds (2 today). Accepted, because the
alternative is a ranking that silently stops meaning "the whole store".

## Rebuilds are additive; only a width change purges

Two different operations, and conflating them was the flaw:

- **A rebuild** (a renderer changed) **enqueues every document of that type at `PRIORITY_BULK` and
  overwrites as they land.** Nothing is deleted first. A document keeps the vector it had until its
  replacement arrives, so it is *never* unsearchable during a rebuild — and a rebuild that stalls or
  fails leaves the store exactly as searchable as it was. The visible cost is honest: for the duration
  the index mixes old and new vectors, so ordering near the boundary is muddled. Not wrong — muddled.
- **A dimension or model change** is the one case that *must* purge: vectors of the wrong width cannot
  be read, so the table is recreated. That is a schema event, not a bookkeeping one, and it is the
  only path that empties the index.

Because a stale vector is kept, **"vector count < document count" no longer measures rebuild
progress** — it is only ever a *missing* count. Progress comes from the queue: the rebuild enqueued N
bulk rows, and the backlog draining to zero is the completion signal. One `meta.REBUILD:<type>` row
(started, requested) records the intent, so an interrupted rebuild is visible rather than inferred
from absence.

## Audit and sweep — one set difference, no model

```
index keys MINUS document keys   →  MOVE to the tombstone index (never drop)
document keys MINUS index keys   →  enqueue the missing document
```

Serves three jobs: a delete whose caller did not tombstone, missing vectors after a crash or a
dropped queue row, and torn indexes after a killed rebuild — **and it is the backfill producer**: run
at the drain loop whenever the queue is empty, queueing everything missing at `PRIORITY_BULK`. Pure
set arithmetic, no model, so it is cheap enough to be routine.

Consequence: **`reembed` stops being a producer.** Its remaining job is *enqueue every document of
this type at bulk priority* (and record the fingerprint); the refill rule does the rest, and stops on
its own when nothing is missing. One code path for rebuild, repair and catch-up — and `reembed`
cannot get stuck holding a position it forgot.

## Delete tombstones the vector; it does not purge it

The caller moves the vector in the **same transaction** as the document row, so normal search stops
returning it immediately. That is a foreign-key-shaped invariant enforced in code, and `0005`
deliberately refuses foreign keys ("rot is *detected* by the audit instead of raising a broken-FK
error") — so the sweep is the detection half, not belt-and-braces: an orphan in the live index is
moved to the tombstone index rather than dropped.

**Retention is declared per kind, and PURGE is the default for anything a person expects gone.**
★ Strengthened 2026-10-21 after review (stet): *"deleted" must mean unreachable by meaning, not merely
absent from a default view* — and disclosure is the worse failure, because an unrecoverable delete is
visible while a disclosure is not. ★ Refined the same day (James): unreachable means **unreachable
without asking**. The rule therefore has three tiers, and every kind must sit in one of them:

- `memory` → **tombstone, reachable by an explicit deleted search**: its content may be wanted as
  history, the store already refuses to lose what it deletes (versions, snapshots), and recovery being
  one search away is what makes deleting it a responsible act rather than an irreversible one.
- `mail` → **purge** (vector, queue row, log line): a letter's deletion is a promise to the person,
  not a filing decision.
- `project` → undeclared until it has a delete path; the default until then is **purge**, because a
  kind that has not argued for retention has not earned it.

A tombstone must be unreachable by meaning through every normal path — the sibling table guarantees
that structurally, since no default query touches it — and reachable only by asking, through a verb
that says what it is. The tombstone chain also has a deliberate end: `memory_history_erase` is the act
that truly destroys, so "gone" has a name and it is not a side effect of deleting.

## What dies

`encode_queue.kind`, `.doc_hash`, `.attempts`, `.last_error`; `memory.encode_hash`; the
`ENCODE_PENDING` sentinel; `row_hash`; the `encode_queue_due` index; the `claim` JOIN to `memory`;
the per-row projection copy; and the single `embed_document(key, title, body)` that every kind would
otherwise be squeezed through. Three bug classes are removed **by deletion**, which is the only kind
of fix that stays fixed.

## Cost, stated plainly

- **A rebuild is always-full, but never destructive.** With no per-row stamp there is no "nothing
  changed" skip: ~300 × 5–11 s ≈ 0.5–1 h of background work, none of it under the store lock. The
  index stays answerable throughout, and a stalled rebuild costs freshness, not availability.
- **A stale vector can outlive a wrong one.** An un-embeddable document keeps its previous vector, so
  it stays searchable where the old design made it unsearchable; the in-memory failure set only stops
  the loop.
- Per-kind renderers ⇒ per-kind fingerprints and targeted rebuilds; a separate table only if a
  model or dimension diverges.

## Implementation shape — the touchpoints

One tree: the encode worker lives in **this** repo (`src/mcp/mod.rs:1201`, `spawn_encode_worker`), not
in spine. The work, by file:

| file | change |
|---|---|
| `migrations/0015_*.sql` | `document_vec` + `document_vec_deleted` (copied from `memory_vec`), drop `memory_vec`, new `encode_queue`, drop `encode_hash`, `PROJECTION` → `PROJECTION:memory` |
| `src/store/encode.rs` | queue rewrite: `enqueue(keyslug, content, priority)`, `next` by `priority, queued_at`, settle by content; no `claim` join, no hashes |
| `src/mcp/mod.rs` | the worker gains the **refill rule** and the in-memory recent-failure set; `semantic_search`; `memory_search` alias; `memory_get` drops `encode_hash` |
| `src/store/maintain.rs` | `reembed` becomes *enqueue every document of this type at bulk priority + record the fingerprint*; the producer loop goes, and nothing is deleted |
| `src/embed/projection.rs` | becomes the **memory** renderer behind a `Document` trait; `fingerprint()` takes a kind |
| `src/store/project.rs` | the **project** renderer — no `status`, no `owner`, no dates, no `preload` |
| `src/index/vec.rs` | `document_vec` + `document_vec_deleted`, both `type` partition key + `+key`; copy/move path; per-type counts |
| `src/search/mod.rs` | KNN with an optional `type`; per-type `max_distance`; `deleted` queries the tombstone table alone |
| `src/store/mod.rs` | drop `ENCODE_PENDING` and `row_hash`; the delete path **moves** the vector to the tombstone index in the same tx |
| `src/cli/doctor.rs`, `src/http.rs` | per-type vectors vs documents; per-type projection staleness; queue backlog/age |
| `tests/` | `vec_kind_filter.rs` (kept — it is the contract with sqlite-vec), plus `search`, `store`, `merge`, `dupes` updated for keyslugs |

## Migration — one step, because derived things can be replaced

The queue and the index are **derived**, so neither is migrated: the queue is dropped and recreated
(any pending row is re-queued by the refill rule), and `memory_vec`'s **vectors are copied**, not
recomputed — measured: `INSERT INTO document_vec SELECT memory_id, embedding, 'memory', key FROM
memory_vec` copies a vector intact and answers a KNN at distance `0.0`. Dimension and model are
unchanged, so nothing needs the encoder.

`0015` therefore does the whole thing without a dual-write window or a flip:

1. Create `document_vec` (partition key `type`, `+key`) and `document_vec_deleted`, and copy every
   `memory_vec` row into the live table with `type = 'memory'`.
2. Drop `memory_vec`.
3. Drop and recreate `encode_queue` with the four-column shape.
4. `ALTER TABLE memory DROP COLUMN encode_hash`; rename `meta.PROJECTION` → `meta.PROJECTION:memory`
   (`meta.DIM` and `meta.MODEL` are unchanged).
5. Deploy the API change (`semantic_search`, `memory_search` alias, `get` stops reporting state).

No re-embedding, no hour of backfill, no window where search is wrong. The only cost is that the
store must be **quiesced** for the duration (one write lock; the copy of ~300 vectors is
milliseconds).

★ **Take the number clash; never take a gap.** 0013 collides with the unmerged mailbox branch
(`0013_mail.sql`); whoever lands second renumbers. Skipping to 0015 to be polite would be worse: the
store would jump to 15 and #13/#14 would never apply, silently and permanently. A clash is visible and
renumberable; a gap is not.

★ **And make a gap loud (stet, 2026-10-21).** `user_version` is currently the only record of what ran —
a counter with no independent check. A `schema_applied(version, label, applied_at)` table, written in
the same transaction as each migration and backfilled for 1..12 as "assumed", lets a startup assertion
say *"version N implies N rows, contiguous"*. Follow-up, not this work: it is migration machinery, not
embeddings.

## API change (deliberate, not incidental)

**Retrieval tools stop reporting index state.** `memory_get` currently advertises `encode_hash` —
*"the hash of the embedded document, or `encode_pending` while the background encode worker still
owes one"*. That field goes and **nothing replaces it**: `[document]_get` answers from truth, and
whether a vector exists is a question for `doctor` / `/health`. It also removes a lookup from every
read.

**`semantic_search(query, type?, deleted?)`** is a new tool; `type` is optional and absent means
per-kind `k` merged by distance (see *Search* above). `deleted = true` (default `false`) searches the
**tombstone index** and nothing else — never both, never merged, so the flag cannot quietly pollute a
normal search. ★ Decided 2026-10-21 (James): an **argument**, not a dedicated verb — the API surface
stays one search tool, and the naming stays one word per thing (`document_vec_deleted`, `deleted_at`,
`deleted`). `deleted` also filters by `type` when both are given, because the tombstone index carries
the same partition key.

Deleted hits are **keys, not bodies**: `type`, `key`, `distance`, `deleted_at`. That is deliberate and
it is what makes the recovery honest — the tombstone's job is to turn *"I remember something about X"*
into *"the key was `foo`"*, and **`memory_history foo` already holds the body**, because the delete
snapshots the state it removes. Two mechanisms, each keeping its own fact: the vector is *how you find
it when you have forgotten the name*, the version is *what it said*.

★ **Refined 2026-10-21 (James): "unreachable" means unreachable WITHOUT ASKING, not unreachable.**
A deleted document must never appear in a normal search, and must never appear in another reader's
results at all. But the model that deleted it may legitimately need it back — missing information it
knows it once had and can no longer name. Keeping it one explicit search away is what makes deleting
*responsible*: a reversible-by-meaning act rather than a leap. The boundary that does not move: where
deletion is a promise to a **different** person (`mail`), there is no deleted search at all — it purges.

`memory_search` stays **permanently** as a thin alias for `semantic_search(type = "memory")` —
removing a tool that callers depend on buys nothing and breaks them. It forwards `deleted`, since
memory is the kind that has a graveyard.

`max_distance` becomes a **per-type, tunable** default: 0.45 to start (the value chosen for
memories), overridable per type without a code change. Project text is short and title-headed so its
distance distribution differs — measure after the first backfill and tune; until then a cross-kind
query has no dependable "no match" floor.

`memory_dupes` is **same-kind only** — a memory's duplicate is another memory.

**`mail` is a kind, but not yet.** It brings two requirements to keep the door open, not to build
now: (a) a kind is **not automatically searchable by everyone** — the search path needs a visibility
predicate, not just a type filter, which is the privacy complexity that blocks it; (b) an expiring
kind needs a purge hook covering the queue, the index, and the failure log (never log a letter's
content). Nothing in the design assumes "every kind is public".

## The decision (resolved 2026-10-21)

**Each document type decides what within its own object is worth embedding.** One vec0 table with
`type` as a partition key, one queue; a renderer and a `meta.PROJECTION:<type>` fingerprint per kind.
A second table appears only if a kind ever needs a different model or dimension — and that rebuild
lands on every kind at once, which is accepted as not worth splitting for.

Consequence to accept: a change to one renderer now rebuilds *one kind*, which is the point — the
blast radius is the kind, not the store.

**A renderer change never triggers a rebuild by itself.** That is today's behaviour and it is kept:
`projection_stale()` is *reported* (`/health`, `doctor`: "STALE — run reembed") and an explicit
`reembed` does the work. Automatic rebuild-on-deploy would change search results silently and start
an hour of background work because someone edited a format string.

## Not in scope

- **Prose reference typing** (`[[type:key]]`) — a separate migration; see §5.
- **`mail` as a kind** — blocked on visibility/privacy, not on schema; see the API section.
- **Backups** — a devops concern. The snapshot excludes nothing; the queue is a small pile of dirt
  nobody minds.
- **Per-kind tables for a diverging model** — accepted as a global rebuild instead.

## Verified before writing this

- `CURRENT_TIMESTAMP` and `datetime('now')` produce the identical string (`same=1`, measured).
- On a real `sqlite3`: the upsert coalesces to one row; a **stale** settle (`content` = previous
  text) deletes **0** rows; a **live** settle deletes **1**. The lost-write guard behaves as
  designed.
- **`tests/vec_kind_filter.rs`** (pinned 0.1.9, real `vec0`): a KNN cannot filter on an auxiliary
  column (*"An illegal WHERE constraint was provided on a vec0 auxiliary column in a KNN query"*);
  it **can** filter on a `partition key`, where `k` is per-partition; without a type, `k` is global;
  a partition key cannot be UPDATEd; `type || ':' || key` derives the keyslug in SQL.
- On a real `sqlite3`: `priority = min(priority, excluded.priority)` keeps a live row at 3 when a
  bulk sweep at 5 touches it, the default of 3 applies to a plain insert, and ascending
  `priority, queued_at` returns the live row first. Also measured: `INSERT OR REPLACE` would reset
  the row to 5, which is why the explicit upsert is used.
- **`tests/vec_kind_filter.rs`**: `INSERT INTO new_vec SELECT id, embedding, … FROM old_vec` copies a
  vector between vec0 tables and the copy answers a KNN at distance `0.0` — so the migration needs no
  encoder and no backfill window.
- **`tests/`**, the failure contract: `a_failed_embed_keeps_its_entry_so_the_staleness_stays_visible`
  pins the vector unchanged, `missing()` **empty** (so the refill cannot mask it) and the debt still
  queued; `the_queue_steps_over_a_document_that_has_just_failed` pins that skipping removes nothing.
- **`tests/vec_kind_filter.rs`**, tombstone move: copy to `gone` + `DELETE FROM live` leaves the live
  KNN with one hit (the survivor) and the tombstone KNN with the deleted one, `deleted_at` readable as
  an auxiliary column. So a delete can be a move, and "search the deleted" is a real query.
