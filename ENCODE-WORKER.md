# The encode worker — the write path stops embedding

**Status:** implemented and live (P0–P5, PR #20 — `dev` on `edge`, 2026-10-15). Supersedes the inline
embed in `store::add_impl`. P6 (the `/health` side of the same decision) has since landed too: the
readiness predicate no longer counts vectors, and the queue is reported as a metric. Phase 0 changed no
behaviour; the phases were independent commits so the suite stayed green at each step.

## Why

Every write today embeds **under the session mutex**. `add_impl` is careful to embed *before* it
opens the transaction (`store/mod.rs:179` — "inference is slow and must not hold the write lock"),
but it is still inside `with_store`, which holds the single `Arc<Mutex<Session>>` for the whole
call. So the careful part is the *transaction*, and the lock — the thing every other request
queues on — is held across a model call anyway.

The consequence, measured on the live store 2026-10-15:

- One write is one store-wide stall. `memory_upsert` measured 0.4–0.6 s warm; a 16 KB body is
  51 s. Every other request waits out that number, reads included (they survive only because
  `with_read` bypasses the lock entirely — `eda7aae`).
- `memory_reembed` holds the same lock for its whole run, so a rebuild is a **total write outage**.
  That is the incident: a ~45-minute reembed made every write time out at 60 s, and the store
  honestly reported `ready: true` beside `writer: busy 1239.1s`.
- **A timed-out write is not a cancelled write.** `with_store` runs on `spawn_blocking`, so the
  client's cancel cannot reach it; the queued write commits when the holder releases. I concluded
  "not a phantom success" from a read-back taken during the outage, and the key showed up later.

The write path's real problem is not that embedding is slow. It is that **embedding is not part of
the write**. The row is the truth; the vector is a cache we happen to have coupled to it by a lock.

## The shape

```
   POST /memory · memory_upsert
            │
            ▼
   ┌──────────────────────────┐   ONE transaction (one SQLite file)
   │  upsert row              │   · memory row
   │  INSERT INTO encode_queue│   · queue row       ← atomic, so no work is ever lost
   │  reply {queued: true}    │
   └──────────────────────────┘
            │  notify
            ▼
   ┌──────────────────────────────────────────────┐
   │  worker (one, in-process)                    │
   │  1. claim one row      ← short lock          │
   │     ........release.......                   │
   │  2. embed              ← NO lock held        │
   │     ........release.......                   │
   │  3. write vector, set doc_hash, dequeue      │
   │     ← short lock, conditional on doc_hash    │
   │  4. sleep `encode_interval_ms`               │
   └──────────────────────────────────────────────┘
```

The lock is taken twice per job, for two transactions. **Step 2 — the model call — holds nothing.**
That is the whole change: the store is held for milliseconds per job instead of for one embed per
row, and an interactive write can slip between jobs.

## The law this keeps

`store ≠ embed` (ENGINEERING.md) already says the row keeps the literal text and one *document*
is embedded. This extends it: **the row and its queue entry are written together; the vector is
written later, by someone else.** One file, one transaction, so there is no dual-write hole — the
enqueue cannot be lost while the row survives, which is exactly what a second database could not
promise.

Corollary, and the reason this is safe to lose: the vector is a **cache**. `encode_queue` is
derived state, `memory_vec` is derived state, and both rebuild from `memory`. Nothing here is
allowed to become un-derivable.

## The queue — `migrations/0008_encode_queue.sql`

```sql
-- One pending document to embed. Derived state: the rows are the truth, so a lost queue
-- costs a re-embed, never a memory.
CREATE TABLE encode_queue (
  key        TEXT PRIMARY KEY,          -- the memory; UNIQUE is what coalesces a re-enqueue
  kind       TEXT NOT NULL DEFAULT 'memory',
  priority   INTEGER NOT NULL DEFAULT 0, -- 0 = interactive, 1 = backfill
  doc_hash   TEXT NOT NULL,             -- the document THIS entry is for; the dequeue guard
  queued_at  TEXT NOT NULL DEFAULT (datetime('now')),
  attempts   INTEGER NOT NULL DEFAULT 0,
  last_error TEXT
);
CREATE INDEX encode_queue_due ON encode_queue (priority, queued_at);
```

`key` is the primary key on purpose: re-enqueuing a key is one row, not two, so a burst of edits
to one memory costs one embed. `kind` is in from the start — it is the seam that lets projects
(or anything else encodable) join without a second table, and it costs nothing now.

`doc_hash` is what makes the worker safe against a concurrent write, and it is not optional:

- the row is enqueued with the hash of the document *as it was written*;
- the worker embeds the row's **current** text, writes the vector, sets `doc_hash`, then runs
  `DELETE FROM encode_queue WHERE key = ? AND doc_hash = ?`;
- if a newer write landed while the worker was embedding, its enqueue bumped `doc_hash`, the
  delete **misses**, the row stays queued, and the newer text is embedded on the next pass.

Without the guard, the worker's dequeue would swallow a write that arrived after it claimed. With
it, the queue is self-correcting and the worker can be killed at any point.

## The write path

### Phase 0 — take the embedder off the hash (`row_hash`)

`row_hash` takes a loaded `&dyn Embedder` (`store/mod.rs:79`) only to read `model_id()` and
`dim()`. Both are **config**, so it is forced to load 1.2 GB of weights to compute a sha. Under
this design the write path must never load the encoder, so the hash has to come from `Config`:

```rust
/// The identity of the vector space a row's `doc_hash` is written in — everything `row_hash`
/// needs that does not require loading the model.
#[derive(Debug, Clone)]
pub struct VecSpec {
    pub model_id: String, // cfg.model
    pub dim: usize,       // cfg.dim
}

/// `sha256(model|dim|text)` — unchanged output, no embedder.
pub fn row_hash(spec: &VecSpec, text: &str) -> String;
```

Call sites: `store/mod.rs:331`, `store/links.rs:404`, `store/maintain.rs:98`, `tests/links.rs:127`
and `:137`, plus the unit tests at `store/mod.rs:586-589`. Mechanical. Note `model::Spec` does
**not** carry `dim`/`model_id` (verified) — build `VecSpec` from `Config`; do not reach for
`model::Spec`. This commit changes no behaviour, and it is the only reason the write path can be
encoder-free.

### `add_impl` → the row and the enqueue

```rust
/// Upsert a memory and *schedule* its document. No encoder is touched: the vector is written
/// later, by the worker. Returns whether the document moved (and so whether work was queued).
fn add_impl(
    conn: &Connection,
    spec: &VecSpec,
    mem: &Memory,
    reset_coherent: bool,
) -> Result<(i64, String, bool)>;   // (id, key, queued)
```

Inside the existing transaction, after `apply`:

```sql
INSERT INTO encode_queue (key, kind, priority, doc_hash) VALUES (?1, 'memory', ?2, ?3)
ON CONFLICT(key) DO UPDATE SET
  doc_hash   = excluded.doc_hash,
  priority   = min(priority, excluded.priority),   -- 0 is high: never demote a hot key
  queued_at  = datetime('now'),
  attempts   = 0,
  last_error = NULL;
```

and it runs **iff `prepared.needs_embed()`**. That one condition keeps the whole "filing, not
meaning" class free — a tag-only edit, a `source` change, `mark_coherent`, `record_visit`,
`record_useful`, a project merge — because none of them move the document, so none of them queue an
embed.

**`add_impl` also stops creating the vec table**, which `index::vec::ensure` used to do here. That
table's width is a runtime property of the **embedder**, and a write no longer holds one (§ P0) — so
the only code that can honestly create it is the code holding a vector, `settle_encoded`. Deriving the
width from config would pin the store to whatever `config.toml` said rather than to what the encoder
actually emits, and those two are allowed to differ (a test does exactly that).

### `doc_hash` keeps its exact meaning — the part to get right

Today `apply` always writes `doc_hash` and the vector is written in the same call, so the column
means "the text whose vector is stored" and cannot lie.

Under the queue it must keep meaning exactly that, or `needs_embed` and `projection_stale` silently
go wrong:

- **the row write does NOT set `doc_hash`** (except when a vector is supplied inline — see
  `?wait=true`);
- **the worker sets it**, when it writes the vector, to `row_hash(spec, text_it_embedded)`;
- a row that has never been embedded carries `doc_hash = ''`.

So empty is "no vector yet" and non-empty is "the vector for *this* text", while `prepare`'s
`existing doc_hash != new hash` test keeps working untouched. `insert_row` must admit `''` — the
column is `NOT NULL` today and stays `NOT NULL`.

**But `doc_hash` is not evidence that a vector EXISTS**, and that gap bit twice. A killed rebuild or a
hand-deleted vector leaves the hash saying "indexed" while the index is torn. So:

- [`claim`](src/store/encode.rs) asks the index as well — `indexed = doc_hash matches && the vector is
  there` — one extra SELECT per job, buying a `needs_embed` that means what it says;
- `reembed` compares against `index::vec::ids` rather than trusting the hash when deciding what to
  queue. Without both, a rebuild would queue a torn row whose job then declined to do itself: nothing
  embedded, nothing repaired, and no error anywhere.

The general lesson, and it is the sort that is invisible in review: **a derived index needs a stamp
that is *derived from the index*, or it will eventually lie about it.** `doc_hash` describes the text;
it never claimed to describe the presence of the row in `memory_vec`.

### The edges the queue must also reach

Every path that moves or kills a key carries its queue row with it, in the same transaction:

| path | action |
| --- | --- |
| `store::delete` | `DELETE FROM encode_queue WHERE key = ?` — a dead key must not be embedded |
| `links::merge_into` (retires `from`) | same, for `from` |
| `links::rename(old, new)` | re-enqueue `new`: the **key is part of the projection** (`embed_document` embeds it), so a rename moves the vector. Today it re-embeds inline (`links.rs:404`); it becomes an enqueue |
| `store::maintain::reembed` | becomes a **producer** of enqueues — P4. It rebuilds the `vec0` table on a dim change (the one step that cannot be a swap) and queues every row that owes a vector: rebuilt, moved, **or missing from the index entirely**. `index::vec::ids` is what sees the last case; `doc_hash` cannot |

## The job

```rust
/// A claimed document: the text to embed, plus the hash to clear when its vector is written.
pub struct Claim { /* key, id, doc, doc_hash, claimed_hash, indexed */ }

impl Claim {
    pub fn doc(&self) -> &str;
    /// False when the row already carries this text's vector (a duplicate job, or a re-enqueue whose
    /// text never moved): the entry is still settled, but nothing is embedded.
    pub fn needs_embed(&self) -> bool;
}

/// The READ half: take the head of `key`'s entry and build its document. Embeds nothing.
pub fn claim(conn: &Connection, spec: &VecSpec, key: &str) -> Result<Option<Claim>>;

/// The WRITE half: store `vector` and settle the entry. `vector` is `None` exactly when the claim
/// needed no embed.
pub fn settle_encoded(conn: &Connection, claim: &Claim, vector: Option<&[f32]>) -> Result<()>;

/// Both halves plus the model call — for a caller holding NO store lock: tests, `?wait=true`.
pub fn encode_one(conn: &Connection, embedder: &dyn Embedder, spec: &VecSpec, key: &str)
    -> Result<bool>;

/// Encode up to `limit` entries, highest priority first. The synchronous door, and the tests'.
pub fn drain(conn: &Connection, embedder: &dyn Embedder, spec: &VecSpec, limit: usize)
    -> Result<usize>;
```

**The two halves are the design, not a tidy-up.** The obvious shape — one function that reads, embeds
and writes — is wrong here, and wrong for the exact reason this document exists: a caller that holds
the store lock would hold it across the model call, which is the stall being removed. `claim` and
`settle_encoded` are what let the worker lock, claim, **unlock**, embed, lock, settle. `encode_one` is
the composition of the three, and it is only safe for a caller holding *no* lock.

Both halves are plain functions over a `Connection`: unit-testable with `HashEmbedder`, idempotent by
construction, and carrying no state, no thread and no lock of their own.

The worker does **not** use `drain`. Composing the halves itself is what buys it the released lock
*and* a different retry shape — one attempt per pass with the sleep as backoff — where a wide `drain`
burns the retries back to back. That is fine for the synchronous door: each failure is a local error,
and `MAX_ATTEMPTS` bounds the total.

## The worker

```rust
pub struct EncodeWorker { /* JoinHandle + Arc<AtomicBool>, stops on drop */ }

impl MemoryServer {
    /// Spawn the one worker for this store. Exactly one per process — call it from `serve()`,
    /// never from a clone (`http_router` clones the server per MCP session, so a spawn in the
    /// constructor would mean one worker per client, all fighting for the writer).
    pub fn spawn_encode_worker(&self, interval: Duration) -> EncodeWorker;
}
```

Call site: `serve()` (`mcp/mod.rs:1361`), inside the `runtime.block_on` arm, for **both**
transports. `serve()` builds the server once, so that is the one place a singleton belongs. Tests
keep using `drain` and never spawn it.

One pass:

1. **Idle check on a read-only connection** (`db::open_read_only`, held for the worker's life):
   `SELECT key FROM encode_queue ORDER BY priority, queued_at LIMIT 1`. A read-only connection takes
   **no session lock**, so an idle worker costs one indexed SELECT per interval and contends with
   nothing. Empty → sleep, look again.
2. Claim through `with_store("memory_encode", …)` — a short lock — calling `encode::claim`, which
   reads the row's current text and builds the document.
3. **Release.** `embedder.embed_one(claim.doc())` with the lock held *nowhere* in the path. This is
   where the 1.2 GB cold load happens: on the worker's thread, blocking the worker and not a request.
4. `with_store("memory_encode_write", …)` — a short lock — `encode::settle_encoded`, which writes the
   vector, advances `doc_hash`, and conditionally dequeues.
5. Sleep `encode_interval_ms`, then look again.

**Deliberately a poll, not a `Notify`.** A lost wake-up is a stall nobody can see, and a read-only
SELECT every second is free. A `Notify` can be added later as a pure optimisation, changing no
semantics.

**A plain OS thread, not a tokio task** — which is what releasing the lock costs. A pass is blocking
SQLite calls with a model call between them, and a thread says "hold nothing across step 3" by
dropping a guard; `spawn_blocking`'s `'static` closures make keeping the connection across the call
awkward, and the whole point of the split is that the reference must be droppable. Being a thread also
makes the singleton structural: it is started from `serve`, never from a clone (`http_router` clones
the server per MCP session), and a wedged transport cannot take the encoder down with it.

**One worker, one process, one replica** — so there is no claim/lease/visibility-timeout machinery,
because there is exactly one consumer. A crash mid-embed leaves the row queued for the next boot,
which is the correct recovery and needs no code.

**Priority.** Interactive writes enqueue at 0; a backfill (Phase 3) at 1, so `ORDER BY priority,
queued_at` means a fresh edit always jumps a rebuild. The bound worth stating once: under
*continuous* interactive writes a p1 entry waits. Acceptable — it is a rebuild, and it is not a
hazard.

**Failures.** An `embed_one` error increments `attempts`, stores `last_error`, and does **not**
delete the row; the loop moves on to the next key rather than wedging on one bad document. After
`MAX_ATTEMPTS` (5) the entry is left in place and skipped — counted by `encode_failed` in `/health`,
`/stats` and `memory_stats`, and announced once, at the moment it gives up, by an `ERROR` from the
worker. It is not retried forever in a hot loop, and it is not silent either.

## What visibly changes — the breaking part

This is a semantics change to the write contract, so it needs the docs that *are* the spec:

| surface | before | after |
| --- | --- | --- |
| `memory_upsert` reply | `reembedded: true` — "I embedded it" | `queued: true` — "I stored it and scheduled the embed" |
| `POST /memory` reply | same | same |
| `memory_reembed` | blocks for the whole rebuild, then reports `rows` | returns `{status:"queued", queued:N}` **immediately** |
| `POST /reembed` | 45-minute hold, or a 60 s timeout | immediate, unless `?wait=true` |
| a search just after a write | the row is findable | findable after up to `encode_interval_ms` (~1 s) |

Files to edit alongside the code: `SPEC.md` §7 (the write contract), `HTTP.md` (the route table and
its "state" column), `ENGINEERING.md` ("Maintenance" — `reembed` is no longer a blocking pass), the
`UPSERT_DESCRIPTION` / `REEMBED` tool strings in `mcp/mod.rs` (the tool description **is** the spec
for a client), and the `reembedded` field name in `http_surface.rs`.

`reembedded: true` disappearing is the honest part of the change. It was never a promise the store
could keep — it was a promise that the *lock* had been held for this long.

## What this does NOT fix

Stated once, so it is not mistaken for a cure:

- **The encoder still loads 1.2 GB.** The worker absorbs that, but a *semantic search* still
  embeds its query and can pay the cold load on the request path. Warming the weights at startup is
  the fix, and it belongs to the health phase.
- **A dimension change still blocks.** `index::vec::recreate` is a `DROP` + `CREATE` on vec0 — DDL
  that cannot be a swap. Seconds, not minutes, but real. `reembed` must still run it before it
  enqueues anything.
- **A wedged store is still a wedged store.** Disk full, read-only remount, OOM, an SQLite lock —
  the worker does not paper over any of those, and should not.
- **It does not, by itself, make `/health` honest.** That was P6, and it has landed — see below.

## Config

One new key, documented in `config.example.toml` and SPEC §12 (the file rejects unknown keys, so a
new key is a real edit, not an addition):

```toml
# How long the encode worker sleeps between jobs, in milliseconds. This is also the idle poll
# interval, so it bounds how long a freshly written memory is invisible to *semantic* search.
encode_interval_ms = 1000
```

One key, not two. A separate "between jobs" and "idle" interval would be a knob nobody can set
correctly, and the idle sleep is the one that matters — it is the search-latency bound, so it should
be the visible one. `ENCODE_MAX_ATTEMPTS` stays a private `const`: it is a safety valve, not a
tuning surface.

## The store-only build must still compile

`cargo test --locked --no-default-features` is a CI check (ENGINEERING.md), and the encoder is
already feature-gated (`qwen3::stub`). `encode_one` / `drain` take `&dyn Embedder`, so they compile
either way; **the worker is what needs the gate** — `spawn_encode_worker` and its loop go behind
`#[cfg(feature = "encoder")]`, and `serve()` calls it only under that feature. A store-only build
returns 503 from `/health` for `encoder_unavailable`, which is the truth for that build.

## Tests

The suite is the point of the design, so it is specified with it.

**Unit (`src/store/`)**
- `add` queues and writes **no** vector; `drain` writes it and empties the queue.
- two writes to one key → **one** queue row, the later `doc_hash` (coalescing).
- `encode_one` then `encode_one` again → no work the second time (idempotent).
- **the guard**: enqueue, rewrite the row, `encode_one` → the queue entry **survives** with the
  newer hash. This is the race the whole design turns on; it gets a written-down assertion.
- a tag-only write and `mark_coherent`/`record_visit`/`record_useful` enqueue **nothing**.
- `delete` and `merge_into` drop the queue row; `rename` re-enqueues the new key.
- priority: a p0 enqueued after a p1 is claimed first.
- `row_hash(&spec, …)` output is **unchanged** by the Phase 0 refactor (pin it against a literal,
  so the refactor cannot silently re-embed every row in the store).

**Integration (`tests/`, the existing suite)**
- The suite drives the queue synchronously: `tests/common` gains `settle` / `add_now` /
  `merge_now`, and `tests/http.rs` + `tests/mcp.rs` gain a local `settle(dir)` that opens its own
  connection to the store file (the one-writer rule is the *service's*, not the file's).
- **The fixture has to describe the embedder.** `tests/http.rs` and `tests/mcp.rs` used to pair
  `Config::default()` (`Qwen…`, dim 1024) with a 64-dim `HashEmbedder` — invisible while the write
  path ignored config for both the vector width and the hash. Under the queue the config *is* the
  write path's view of the vector space, so a mismatch makes every later write look like a document
  change and the queue never converges. Both fixtures now echo the fake (`model = "fake-hash-v1"`,
  `dim = 64`).
- **The regression test for this whole document**: a slow fake embedder (a sleeping `Embedder`,
  as `the_encoder_does_not_serialize_its_callers` already uses) plus a running worker, then a
  `memory_upsert` must return in the **milliseconds** it takes to write a row while the worker is
  mid-embed. Today that same call waits out the embed × every queued row. Written as an arithmetic
  threshold so it cannot be mis-tuned, and named after the property, not the mechanism.
- `?wait=true` on a write returns only once the row is searchable.
- a worker killed mid-drain leaves the queue intact and a fresh worker finishes it (restart
  recovery, no code).

## The plan — six commits, green at each

Ordered so that no commit leaves the tree worse and the *behavioural* flip is isolated to one small
diff that can be reviewed on its own.

| # | commit | behaviour change | why here |
| --- | --- | --- | --- |
| **P0** | `row_hash` takes `VecSpec` instead of a loaded `&dyn Embedder` | none | It is the enabler: without it the write path cannot avoid loading the model. Machine-checkable — the hash output must not move. |
| **P1** | `migrations/0008_encode_queue.sql`, plus `encode_one` + `drain` and their unit tests | none | **A migration ships before the code that needs it**, always: a rollback must not find code expecting a table the old binary never made. Nothing enqueues yet. |
| **P2** | the write path: `add_impl` enqueues iff `needs_embed`, stops embedding, stops setting `doc_hash`; `insert_row` admits `''`; tests gain `drain` | **the flip** | The one to review hard — and it is small. After this, a write returns without a vector until something drains. |
| **P3** | `EncodeWorker` + `spawn_encode_worker` from `serve()`, both transports; `encode_interval_ms`; `?wait=true` | a vector appears ~1 s after a write, from the worker | Only now does the queue drain itself in production. Until P3, `drain` is the only consumer, so the tree is still correct without a thread. |
| **P4** | `reembed` becomes a producer (`index::vec::recreate` if the dim changed, then enqueue every stale key at p1); `memory_reembed` / `POST /reembed` reply `{queued:N}` | `reembed` stops blocking | The outage actually ends here, and only after P3 exists to consume what it enqueues. |
| **P5** | docs ride with their commits: `SPEC.md` §7, `HTTP.md` route table, `ENGINEERING.md` "Maintenance". A `PLAN.md` row for the phase history | none | The tool strings in `mcp/mod.rs` are a client's only spec, so they change in P2/P4, not here. |

P6 was the health gate (Phase 4 in the decision already recorded) and it has since **landed**, as its
own change on top of P0–P5:

- the readiness predicate **dropped `vectors >= memories`**. With a queue that clause is not a safety
  net, it is a lie that flips on every write: a write leaves N memories and N−1 vectors for as long as
  the worker takes, so `/health` answered `NOT READY — run \`reembed\`` for the store's normal state.
- `ready` is now `schema == latest && !projection_stale && writer not held past 2 s` — readiness for
  reads **and** writes, with nothing derived-but-lagging in it.
- the queue is **reported, never gated on**: `encode_pending`, `encode_failed`, `encode_oldest_s` in
  `/health` (both formats), `/stats` and `memory_stats`. The last one is the number that distinguishes
  a slow worker from an absent one, and the reason a stalled queue is now visible without a DB copy.
- the worker logs each job (`encode worker: encoded`, with `pending`/`failed` after the settle) and an
  `ERROR` when it gives up on a document. A worker that only logged failures was indistinguishable
  from one that was not running.

**One term of the original predicate was not implemented as written.** The decision included
`encoder_warm`, on the grounds that a cold encoder stalled the first *write*. The worker removes that
coupling — the 1.2 GB load lands on the worker holding nothing — so a gate on it would 503 a healthy
store for the ~7 s after a restart, and again whenever nobody has asked for an embedding yet. It is
not a readiness fact any more; warming the weights at startup (so the *first search* does not pay it)
remains open, and so does wiring the k8s readiness probe to `/health` — one replica behind a Service
means a 503 leaves zero endpoints, which is its own decision and not this one.

## How we will know it worked

Not a feeling, three facts:

1. the arithmetic regression test above passes — an `upsert` returns in milliseconds while the
   worker is mid-embed, where today it waits out the embed;
2. `POST /reembed` on the live store returns in **seconds** with `{queued: 291}`, where today it
   holds for ~45 minutes and every write during it is a 60 s timeout;
3. `/health` shows `encode_pending` draining to 0 after a write burst, with `writer` never
   `busy` for more than a job.

## Open — your call, not mine

1. **Is a single write really async?** P2 makes every write asynchronous, with `?wait=true` as the
   opt-in. An alternative is the inverse default (sync for a one-key write, async for a backfill),
   which keeps `reembedded`'s promise for the common case but reinstates the coupling this document
   exists to remove. I am for async-by-default; say so if you are not.
2. **`dupes` over a partly-encoded store.** It reads stored vectors, so a pending row is invisible
   to a duplicate scan. Warn-and-proceed, or have `dupes` drain first (making a read take a write)?
   I would warn: a dupe scan is a curation aid, and stalling a read to answer it is the coupling
   again.
3. **A permanently-failing document.** Skip after `MAX_ATTEMPTS` and surface it in `/health` (my
   proposal), or fail the write loudly? **Taken: the first.** Implemented — the entry is skipped,
   counted as `encode_failed` in `/health`, `/stats` and `memory_stats`, and announced once by an
   `ERROR` from the worker. The former is honest and non-blocking; the latter would block a store on
   one bad string.
