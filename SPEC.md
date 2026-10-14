# ellmephant — Design Spec (decision record)

> **Status:** decision record, v1 implemented. This captures *what we decided and why*; the
> code is the authority for behaviour, and every phase of `PLAN.md` (P1–P10) has shipped.
> §15 lists the questions that were open when this was written, each now resolved.
>
> **Name:** the crate/binary is **`ellmephant`** (LLM + elephant — "never forgets"). The crate
> name is **free on crates.io** (`elephant` was taken). Personal store root: `~/.ellmephant`.

## 0. One-liner

ellmephant is a **single-binary, SQLite-backed long-term memory for AI agents**: the
embedder engine is compiled into the binary, one SQLite file is the sole source of truth,
and a *rebuildable* vector + link index sits on top of it. It speaks MCP (so
any client — Cline, TypingMind — shares the same memory) and serves that one store to all of
them. The name is LLM + elephant; the animal is the aspiration, the software is the
attempt.

## 1. Why build it

- Agents forget expensively. Retrieval today is `grep`-plus-guessing over markdown files;
  cost is dominated by *hunting* and *reading whole files*, not by the content itself.
- Memory should be **shared across tools**, not trapped per-client.
- The data should be **owned**: one file on disk, no cloud, no interpreter, no service.

## 2. Goals / Non-goals

**Goals**

- Hermetic Rust binary: no Python/Node runtime, no external service, no cloud.
- SQLite is the single source of truth (one file, `~/.ellmephant/memory.db`).
- Embedder *engine* in-binary; *weights* external (disk or HuggingFace).
- Retrieval by **meaning** (a semantic phrase, KNN) or by **literal** (a substring/regex `pattern`),
  with optional structured filters (tags, size, coherence); `key`/`tags` remain identity and lookup.
- Wiki-style links with backlinks and one-hop graph expansion.
- Multi-client via MCP: one shared memory, many clients (Cline, TypingMind, anything that speaks MCP).

**Non-goals**

- Not markdown-on-disk (rejected — see §4).
- Not a cloud/sync service; not multi-tenant; no auth.
- Not an LLM extraction/summarization pipeline.
- **No stored generalizations / no autonomous consolidation** (see §9).

## 3. Prior art & positioning

| Project | What it is | Why it isn't this |
|---|---|---|
| **Basic Memory** (4.1k★, Python) | MCP memory; wiki `[[links]]`, backlinks, graph traversal (`memory://`), hybrid vector+fulltext, local | Truth = **Markdown files** + SQLite *index* (inverse of our call); Python; FastEmbed/ONNX |
| **mcp-memory-service** (2.0k★, Python) | MCP+REST; **sqlite-vec**; local **ONNX** embeddings; typed KG edges (`causes/fixes/contradicts`); `tag_match=AND/OR` | Python + ONNX (not engine-compiled-in); no wiki-links; **autonomous consolidation** (stored generalizations) |
| **Mem0 / Zep / Letta / Cognee** | Managed memory layers / frameworks | Service- or vector-DB-backed, cloud-heavy, LLM extraction pipelines |
| **Official MCP `memory` server** | Knowledge graph memory | JSON-file store, no embeddings, no vectors, no wiki-links |

**The gap:** nobody combines **SQLite-as-truth · `sqlite-vec` KNN · wiki-link graph ·
engine-compiled-into-a-Rust-binary · one MCP server shared across clients.** Each half
exists; the intersection (and the no-interpreter posture) does not. Two shipped projects
independently validate our design (Basic Memory → graph/backlinks/traversal;
mcp-memory-service → typed edges + tag AND/OR).

## 4. Locked decisions (with rationale)

| # | Decision | Rationale |
|---|---|---|
| 1 | **Rust**, single binary | Pure-Rust inference + `bundled` SQLite ⇒ no interpreter, no sidecar. Go loses: no pure-Go BERT inference |
| 2 | **SQLite = truth**; no markdown on disk | One authoritative file you can copy. Markdown loses atomic updates, normalised tags, and flat retrieval as it grows |
| 3 | **`rusqlite` (`bundled`) + `sqlite-vec`** as auto-extension | SQLite **and** `vec0` statically linked into the binary (`sqlite3_vec_init` via `sqlite3_auto_extension`). No `.dylib`, no `load_extension` |
| 4 | **`candle`** for inference (not `ort`/ONNX) | `ort` binds C++ ONNX Runtime (default: downloaded shared lib) — a sidecar. `candle` is pure Rust ⇒ truly in-binary |
| 5 | **Qwen3-Embedding-0.6B**, 1024d, cosine | Best MTEB-multilingual of the candidates we compared (64.33 vs bge-m3 59.56), 32k context, MRL-capable, instruction-aware, and the only one with a **native candle path** (`fastembed`'s `Qwen3TextEmbedding`) |
| 6 | **bf16** weights (f16 fallback) | ~1.2 GB resident; bf16 is fine under Metal; CPU kernels may emulate bf16 ⇒ `--precision f16` escape hatch |
| 7 | Weights **external**: our own flat cache under `~/.ellmephant/models/`, or `--model-dir`; `--offline`; sha-pinned revision (§6.1–6.5) | Keeps the binary small; weights are data. We own the layout (not `HF_HOME`) so it is inspectable, `gc`-able, and integrity-checked |
| 8 | **cosine**, L2-normalised both sides; **last-token pooling + left-pad** | Qwen3 is a decoder embedder — mean pooling (the candle BERT example) gives silently worse vectors |
| 9 | Root `~/.ellmephant`; DB `memory.db`; `config.toml`; models in `models/` | Single dotdir, matches tooling conventions |
| 10 | **Tags** name aspects of a subject (e.g. `storage`, `gotchas`); no `scope`, no `type` | Tags are a facet and part of the embedded document, so a tag token is retrievable; a bucket/type taxonomy adds vocabulary without adding recall |
| 11 | `key` = **global-unique slug**, optional to supply | Identity (upsert target) and link target; auto-slugged from the title when omitted |
| 12 | **Smart-caveman** writing; **one** document embedding over `key` + tags + `title` + `body` | Terse text embeds sharply, and one vector per row is simpler to build and rebuild; terseness keeps the body from swamping the title |

## 5. Architecture

- **Language:** Rust (edition 2024+).
- **Crates:** `rusqlite` (feature `bundled`), `sqlite-vec` (auto-extension), the `encoder` feature
  (`candle-core`/`candle-nn`/`candle-transformers` + `tokenizers`; `--no-default-features` drops it),
  `ureq` (weights fetch, rustls), `rmcp` + `axum`/`tokio` (the MCP server), `clap` (three verbs),
  `serde`/`serde_json`, `toml` + `serde` config, `sha2`, `regex`, `thiserror`,
  `tracing`/`tracing-subscriber`. (No `hf-hub` — see §6.2.)
- **Process model:** one **long-lived** process — the MCP server — holds the model warm
  (~1.2 GB resident) and one writer connection. (The REPL and the one-shot flags that each paid a
  cold start were removed; see §11.)
  A shared daemon is *optional* and only if bare one-shot latency annoys us (see §15).
- **Indexes are derived:** the document vectors (`memory_vec`) and links (`memory_link`) are all
  rebuildable from the rows. The rows are truth — `memory` and the project tables (§7.1) — and there
  is no FTS index or tag table, so nothing else needs keeping in sync.

## 6. Embedder

- **Model:** `Qwen/Qwen3-Embedding-0.6B`, output **1024** (MRL allows 32–1024; frozen at
  `CREATE`, choose 1024 — rebuildable later).
- **Engine:** candle, pure Rust. **Weights:** our own flat cache under `~/.ellmephant/models/`
  (not `HF_HOME`), or `--model-dir`; fetched on first run; `--offline` forbids network. See §6.1–6.5.
- **Precision:** bf16 default; `--precision f16` for native ARM CPU path.
- **Pooling:** last-token with **left** padding, then L2-normalise.
- **Query instruction:** `Instruct: <task>\nQuery: <q>` on the query side; documents bare
  (+1–5%). Task string stored in `meta`.
- **store ≠ embed:** the *row* keeps the full literal text; one **document** is embedded — the
  `key`, the tags, the title and the link-stripped body (`[[links]]` removed), one non-empty
  part per line. One vector per row.

### 6.1 Weights are data: the model cache

The encoder needs **three required files** from `Qwen/Qwen3-Embedding-0.6B`: `config.json`,
`tokenizer.json`, `model.safetensors` (~1.2 GB bf16). Optional: `tokenizer_config.json`,
`generation_config.json`, `vocab.json`, `merges.txt`. Weights are **data, not code**: never
baked into the binary, and never required at runtime *to run* — only to *fetch*.

One flat dir per resolved revision, so `--model-dir` is a first-class alternative:

```
~/.ellmephant/models/<owner>--<repo>/
  refs.json              # {"main": "<sha>"} — a ref resolved once, then pinned
  <sha>/
    manifest.json        # {repo, revision, resolved, created, files:{name:{sha256,size}}}
    config.json  tokenizer.json  tokenizer_config.json  model.safetensors
    .lock                # advisory lock held for the duration of a fetch
```

- `<owner>--<repo>`: `Qwen/Qwen3-Embedding-0.6B` → `Qwen--Qwen3-Embedding-0.6B`.
- `<revision>`: the **commit sha**, not `main`. `main` is resolved once and recorded, so an
  upstream move never silently changes our vectors.
- `--model-dir <dir>` bypasses this entirely: a flat dir that must already hold the files
  (implies `--offline`).
- `cache_dir` overrides the root (default `~/.ellmephant/models`). We do **not** reuse
  `HF_HOME`/`HF_HUB_CACHE`: our layout, our manifest, one place to look (§12).

### 6.2 Fetch protocol

Fetching is **first-run, opt-in, and the only network code in the binary**.

- Files come from `https://huggingface.co/<repo>/resolve/<sha>/<file>` (`HF_ENDPOINT`
  overrides the host), sequentially, with a size-carrying progress line.
- **Atomic per file**: download to `<file>.partial`, `fsync`, `rename(2)` into place. A crash
  leaves no half-valid cache; `manifest.json` is written **last**, after every file verifies.
- **Concurrency**: an advisory `.lock` (flock) is held for the fetch; a second process waits
  or exits with a clear message. No resume in v1 — a partial file restarts.
- Transport is `ureq` + **rustls**: pure-Rust TLS, so still no OpenSSL `.dylib` (§4 #4).
  `HF_TOKEN` is sent when set (the repo is public, so it is optional).
- We deliberately **do not depend on `hf-hub`**: `1.0` pulls in tokio+reqwest+hyper, `0.5` is
  the legacy API, and `candle-core` 0.11 ships no `hub_load_safetensors` — the loader is ours
  anyway. ~200 LOC buys a hermetic binary and a cache we can `doctor` and `gc`.

### 6.3 Integrity

- `manifest.json` records **sha256 + size** per file and the resolved revision.
- Download-time: every file is hashed and compared *before* the manifest is written.
- Load-time: **no full re-hash** — sha256 over 1.2 GB each start would erase the cold-start
  budget (§13). We check size and rely on safetensors' self-describing header failing loudly
  on truncation/corruption. `ellmephant doctor --verify` does the full hash on demand.
- A cache that fails to load names the file and prints the fix: `ellmephant model fetch --force`.

### 6.4 Offline & failure modes

- `--offline` / `ELLMEPHANT_OFFLINE=1` / `offline = true` (§12): **never** opens a socket; a
  missing or incomplete cache is a hard error naming the file and the online command.
- `--model-dir` is always offline. Online (the default) uses the cache when complete, else
  fetches once.
- Disk-full mid-fetch → `.partial` discarded, no manifest, retried next run.
- Revision is pinned by the recorded sha; moving `main` upstream is a no-op until `--force`.
- Runtime network is **never required** once cached, so the §8 DoD holds.

### 6.5 Surface

- `ellmephant model status` — repo, resolved revision, cache path, per-file presence/size, total
  bytes, manifest ok/absent. This is the "model-cache status" `doctor` prints from P1 on.
- `ellmephant model fetch [--repo][--revision][--force]` — prefetch / repair.
- `ellmephant model rm [--revision]` — drop a revision to reclaim space.
- `doctor` prints the same one-line summary.

## 7. Data model

**A memory is one body of text holding closely related facts.** It is identified by a `key` (a
global-unique slug) and gisted by its `title`; the facts live in the `body`, written in
smart-caveman. It carries `tags` — a normalised, comma-joined list of aspects — and a `source`
(where it came from: a command, a path, a repo, a quote). A memory is **found** by meaning (the
semantic `query`) or by a literal/`regex` `pattern`, and narrowed by structured filters (tags,
size, coherence); it is **read whole** by `memory_get`; and it **points** at other rows with
`[[key]]` — another memory's key, or a project's (`[[prj:key]]`; see §8). Its row also holds runtime
bookkeeping (`visits`, `times_useful`, `coherent`) that is never written by hand and never
embedded.

```sql
PRAGMA journal_mode = WAL;
PRAGMA foreign_keys = ON;
PRAGMA user_version = 1;

CREATE TABLE memory (
  id       INTEGER PRIMARY KEY,
  key      TEXT NOT NULL UNIQUE,              -- slug; upsert + link target
  title    TEXT NOT NULL,                     -- smart-caveman one-liner
  body     TEXT NOT NULL,                     -- smart-caveman fact
  tags     TEXT NOT NULL DEFAULT '',          -- normalised names, comma-joined, sorted
  source   TEXT,                              -- provenance: command/path/repo/quote
  doc_hash TEXT NOT NULL,                     -- sha256(model|dim|embedded document)
  created  TEXT NOT NULL DEFAULT (datetime('now')),
  updated  TEXT NOT NULL DEFAULT (datetime('now'))
);

-- Derived, created programmatically by `index::vec::ensure` (dim is runtime, not schema):
CREATE VIRTUAL TABLE memory_vec USING vec0(
  memory_id INTEGER PRIMARY KEY,
  embedding float[1024] distance_metric=cosine);

CREATE TABLE memory_link (
  from_id INTEGER NOT NULL REFERENCES memory(id) ON DELETE CASCADE,
  to_key  TEXT    NOT NULL,                     -- target KEY, not id: a plain name
  to_type TEXT    NOT NULL DEFAULT 'memory',    -- memory|project (v6): WHICH table that name is in
  rel     TEXT    NOT NULL DEFAULT 'see',        -- see|supersedes|part-of|contradicts
  PRIMARY KEY (from_id, to_key, rel)
) WITHOUT ROWID;
CREATE INDEX idx_link_to ON memory_link(to_key);

CREATE TABLE meta (k TEXT PRIMARY KEY, v TEXT NOT NULL);
-- model, dim, instruction, schema_version
```

Notes: `memory_vec` is **app-written** (SQLite can't embed). `tags` is a normalised, comma-joined
column on `memory` (filtered in Rust, rewritten on rename); there is no FTS index or tag table.

Later migrations add runtime columns to `memory`: `visits` / `times_useful` (0002–0003, usage
counters) and `coherent` (0004) — the §9 split-review verdict, a **nullable bool** (`NULL` =
unjudged, `TRUE` = one topic, `FALSE` = a grab-bag worth splitting). `coherent` is not part of
the embedded document, and an upsert resets it to `NULL` (the row changed, so re-judge it).

### 7.1 Projects — the structures half (v5)

A project is **an activity completed over time** — work with a beginning, a current state, and a
place to stand. It has a `key` (a plain name, which a memory may share), a `title`, a `goal` (what it is for), a `state` (mutable
prose: where it stands, what's next), a `status` (`todo`|`active`|`paused`|`done`|`aborted`), an
`owner` (a set of names), `tags`, a `preload` (the `mem_preload` column — the **ordered** list of
memories to load as context to work in here, the boot set generalized to every project), and two
dates: `start_date` (when the work started) and `end_date` (when it ended, if it has). It is
**listed** by field (`project_list`: status, tags, a literal/regex pattern) and **read whole** by
`project_get`, which resolves both of its relations to memories. Projects are never embedded: a
shape is not something you match by meaning.

Memories and projects are the two halves of one store, and the line between them is drawn where the
machinery changes. A memory is a **fact** — voted, ranked, judged for coherence, matched by meaning.
A project is a **structure** — identity, fields, relations — so none of that machinery applies to
it. That is why they are separate tables, and why a **key is a plain name**: a project and a memory
may share one (`sessions` the project, `sessions` the memories about it — normal, not pathological),
so the *table* is a row's type and the **reference** carries it. A memory body writes `[[prj:chronos]]`
to cross into the other graph, and `memory_link.to_type` records which table an edge names, because
`to_key` is a `TEXT` key and not a foreign key.

```sql
CREATE TABLE project (             -- the key IS the identity: no rowid-derived id, no vector
  key         TEXT PRIMARY KEY,
  title       TEXT NOT NULL,
  goal        TEXT NOT NULL DEFAULT '',         -- what this is for
  state       TEXT NOT NULL DEFAULT '',         -- mutable current-state prose
  status      TEXT NOT NULL DEFAULT 'todo',     -- todo|active|paused|done|aborted
  owner       TEXT NOT NULL DEFAULT '',         -- comma-joined names, e.g. 'Cline,James'
  tags        TEXT NOT NULL DEFAULT '',
  mem_preload TEXT NOT NULL DEFAULT '',         -- comma-joined memory keys, ORDERED by priority
  created     TEXT NOT NULL DEFAULT (datetime('now')),
  updated     TEXT NOT NULL DEFAULT (datetime('now')),
  start_date  TEXT NOT NULL DEFAULT '',         -- when the WORK started
  end_date    TEXT NOT NULL DEFAULT '',         -- when it ENDED: done or aborted alike
  CHECK (status IN ('todo', 'active', 'paused', 'done', 'aborted'))));

CREATE TABLE project_memory (      -- RETIRED in 0007; see "Membership is a TAG" below
  memory_key  TEXT PRIMARY KEY,
  project_key TEXT NOT NULL);
```
> **0007 dropped `project_memory`.** It modelled a memory as belonging to at most one project; that
> was wrong twice over (a per-project set is a garbage pail to curate forever, and "one home"
> invents a membership claim for a fact bearing on two projects). Membership is a tag now —
> `prj:<key>` — and the DDL above is kept only as the record of what was removed.

- **`owner` is a set of names, and the names are free text.** Comma-joined, trimmed, de-duped
  case-insensitively and sorted (`James`, not `james`; one person, one entry) — but never checked
  against a list, because the code has no list to check against. It cannot: the store does not know
  who exists, and `me`/`us` would be worse than useless in a store two agents share, since they mean
  whoever happens to be reading. A name nobody recognises is a typo, not a schema violation — and a
  blank `owner` is unassigned, which is a fact rather than a missing field. A `CHECK` could not
  validate the *elements* of a comma list anyway, the same reason `tags` has none.

- **Two dates, and they are the project's, not the row's.** `start_date` is the caller's if stated;
  otherwise it is stamped when the project stops being `todo` — work cannot have started while it is
  only a todo — and it is **never cleared**, so a start on a `todo` project records that the project
  was worked on once and parked. `end_date` is stamped when `status` becomes `done` or
  `aborted`, never moves while the project stays ended (an unrelated later write does not touch it),
  and *is* cleared when the project is reopened: it tracks a current state, where `start_date`
  records a past fact. Neither is `created`/`updated`: a project written down
  three weeks late *started* three weeks ago, and `updated` moves on any write, so it cannot answer
  "when did this finish". The column is `end_date` rather than `finished`/`completed` because
  `status` says **how** it ended and an aborted project was never *completed* — a date that tried to
  say how would have to be wrong about one of them. Dates are ISO (`YYYY-MM-DD`) and shape-checked,
  because a date that does not sort as it reads breaks every "what ended in September" query
  silently.

- **No tie table: a relation earns a table only when its invariant is cross-row, and `preload`'s
  is not one.** A memory may sit in many projects' lists, and its **order is a property of the
  list**, not of each edge — so it is a column (`mem_preload`), with no `position` column
  shuffling a rowset back into order. A key is `[a-z0-9-]` (`slug::slugify`), so a comma can never
  occur inside one: the delimiter is safe by construction, not by convention.
- **One relation: `preload`.** project → memory, **ordered** — *"to work in here, load these"* — a
  project preloads memories it did not author (conventions, boot notes). Its only unique job is
  order; the *set* comes from tags.
- **Membership is a TAG, not a relation** (0007 — the reversal). A memory belongs to a project by
  carrying the `prj:<key>` tag, intersected with topic tags by the ordinary `tags` filter:
  `prj:seaweedfs` + `storage` is *"the storage memories belonging to seaweedfs"*. There is no
  `project_memory` table and no home to set, because a per-project memory set is a garbage pail
  that must be curated forever, and "one home" manufactures a membership claim for a fact that
  legitimately bears on two projects. A tag is decided once, when the memory is written, and
  nothing can dangle. The table 0007 dropped was always empty — nothing needed converting.
- **`prj:<key>` marks a project in a tag; every other tag is a topic.** The colon is visible
  against a dash-heavy tag vocabulary (`rust-lang`, `wash-sales`, `cline-home`), and `:` cannot
  occur in a slug, so no name can squat it.
- **No foreign keys, exactly like `memory_link.to_key`:** a `preload` reference is a forward
  reference, allowed, and rot is *detected* (`store::project::dangling`, `project_get` reports a
  null title, `doctor` counts it) rather than raising a broken-FK error.
- **A link to a project is not membership.** `[[prj:x]]` is a `see` edge (navigation, attention);
  membership is the tag. So `links::dangling` resolves a target against the table its `to_type`
  names — and nothing else: a `prj:sessions` link is not fixed by a *memory* called `sessions`.
- **No delete.** A project ends with `status = done` or `aborted`; the `preload` list cannot
  outlive one.

## 8. Cross-links (wiki semantics)

- **Author:** inline `[[key]]` in `body` (canonical, greppable, exportable). `[[prj:key]]` crosses
  into the structures half: links live in memory bodies, so a bare token is a *memory* and only the
  other kind needs saying. `:` cannot occur in a slug (`[a-z0-9-]+`), so no name can squat on the
  marker — and `[[prj-foo]]` is a memory *called* `prj-foo`, not a project.
- **Index:** parse to `memory_link(from_id, to_key, to_type, rel)` at write time — a *derived* index.
  The **type rides on the edge**, never on the key, because one name may exist in both tables
  (`sessions` the project, `sessions` the memories about it).
- **By key, never by id:** survives a `reembed`; supports forward refs; dangling links are
  detected (non-fatal) rather than broken FKs. Resolution is **per kind**, so a missing project and
  a missing memory are different reports:
  `SELECT to_key, to_type FROM memory_link l WHERE (l.to_type = 'memory' AND l.to_key NOT IN
  (SELECT key FROM memory)) OR (l.to_type = 'project' AND l.to_key NOT IN (SELECT key FROM project));`
- **Backlinks:** `SELECT from_id FROM memory_link WHERE to_key = ? AND to_type = 'memory'` → powers
  one-hop graph expansion in search and the "derived profile" assembly of §9. A project edge is not
  a backlink of a memory that happens to share its name.
- **Rename:** keys are near-immutable; `rename` deliberately rewrites `[[oldkey]]` across
  bodies **and** repoints edges — memory edges only, so a project's incoming links are untouched.
  **No interface exposes it today**: the CLI verb was its only caller and went with the REPL, so
  `store::links::rename` is reachable only from tests. A `memory_rename` tool is the obvious way back;
  the decision is open.
- **Embedding:** `[[...]]` stripped from the embed projection.
- Key charset `[a-z0-9-]+`; the link regex is `\[\[((?:prj:)?[a-z0-9-]+)\]\]`.

## 9. Memory philosophy (how memories are "tuned")

The store is **atomics-first**; generalization is derived, not stored.

- **Atomic memory = the storage *and* embedding unit.** One **topic** — a fact/decision/gotcha/
  signpost, or several *close* ones, one per line — terse, self-contained. A coherent topic
  embeds sharply and edits surgically; a grab-bag smears the vector and rots. Split facts that
  are genuinely distinct.
- **General views are computed at query time** (fan-out / one-hop expansion), never written.
  Store the atoms, synthesize the whole.
- **A sparse pointer tier** (`*-overview` notes) is allowed for routing/navigation only —
  pointers, not content; governed by "one memory per fact, cross-link don't duplicate."
- **Write policy** (guided via the tool descriptions; the **smart-caveman** style is defined
  once in the server instructions, the rest on `memory_upsert`):
  **smart-caveman** — drop filler, articles and glue words, keep high-signal words ·
  one **topic** per row (close facts, one per line; distinct facts split) · **tags** name
  aspects of the subject · **provenance** in `source` (store the *derivation command*, not live
  state) · upsert-by-`key` (whole-body replace, **no auto-merge**) · **merge redundant
  memories by hand** (fold both bodies, upsert the survivor, delete the twin) · trim junk on edit
  — deepen the topic, never add a second (**coherence, not size** — a topic accrues facts over
  time; # of chars is not the signal, the `coherent` verdict is) · never store cheaply-derivable
  state.
- **Split review (`coherent`).** No size or geometry heuristic detects a grab-bag — the judgement
  is a **read**. Review a fat row with `memory_search` (`min_chars`, `coherent: "!true"` — the rows
  not proven coherent), split it by hand, then stamp the verdict (`memory_coherent`). An upsert
  resets the verdict to unjudged, so an edit re-opens review.
- **Read policy:** ask for **keys plus titles, tags and scores** (`fields`) — never dumps (tokens
  regress otherwise); the body is a deliberate `memory_get` away. Recall runs **narrow-to-wide**: a semantic phrase
  at a small `k`, judged from titles/tags/distances, `memory_get` only the hit(s) you'll use,
  widening (`k`) only if none fit.

## 10. Search

A search has two independent **legs** — a semantic **phrase** (`query`) and a literal/regex
**pattern** — plus optional structured **filters**. **Everything intersects** — a hit must satisfy
every leg and filter given. With none, it is a pure filter over the store (every row, `key` order,
uncapped). The two legs don't fuse: a lexical-only query is unranked (`key` order), while a phrase
plus a pattern keeps the semantic ranking and merely drops the rows the pattern rejects.

- The phrase is embedded (query-side instruction) and the KNN of the whole store is taken,
  within a cosine-distance ceiling. One vector per row (§6). Filters are applied to the ranked
  candidate set **before** the `k` cap, so a filtered result is exact (the KNN already spans the
  whole store): filter, then truncate.
- **Params:** `query` (the semantic phrase), `pattern` (a literal/regex over the raw `key`/`title`/
  `body` — for a half-remembered string), `mode` (`literal`, the substring default, or `regex`;
  both case-insensitive), `k` (default `5`; caps the ranked case only), `max_distance` (optional,
  falling back to `default_max_distance`, `0.6`; `null` disables), `expand`, `fields` (the extra hit
  columns to return; default = `key` only), and the filters:
  - `tags` — every listed tag must be present (AND).
  - `min_chars` / `max_chars` — bounds on the **embedded-document** length (chars).
  - `coherent` — the §9 split-review verdict: `true`, `false`, `null`, or negate with `!`
    (`!true` = false-or-null = the review queue; also `!false`, `!null`); omit = any.
  - the **lexical leg** is a plain scan of the store's raw text — **no FTS index, no `LIKE`** — so
    nothing needs keeping in sync (`memory` is the only truth, §5/§7) and a malformed regex is a
    clean error. `key`/`tags` stay identity/lookup; a tag match is the `tags` filter, not a pattern.
  The KNN is taken over the whole store and `k` truncates the ranked
  result, so **a result shorter than `k` means the ceiling, not `k`, cut it** — widen by
  loosening `max_distance`, else by raising `k`.
- **Returns:** `key` **alone by default** — **never the body** (fetch it with `memory_get`/`show`).
  The caller names any extra `fields` it wants (`title`, `tags`, `coherent`, `chars`, `distance`);
  they are added in that **canonical order**, `key` always leading. So a hit says *which* memory,
  and nothing more unless asked — the lean default keeps a search from bloating context. Over MCP
  this is emitted as **positional JSON**: a top-level `cols` header names the returned fields and
  each row is a bare array (§11); `--json`/core keep the keyed form.
- **`--expand N`:** pull N-hop link neighbourhoods into the result bundle (derived profiles);
  neighbours keep their (snipped) bodies.

```sql
-- Semantic phrase: KNN over the document vectors
SELECT memory_id, distance FROM memory_vec WHERE embedding MATCH ?1 AND k = ?2 ORDER BY distance;
-- Lexical leg: a plain scan (no FTS index — `memory` is the only truth), matched in Rust:
--   SELECT id, key, title, body FROM memory;  -- literal substring or regex over key/title/body
```

## 11. Interfaces

**MCP tools** (served over Streamable HTTP by default, `--stdio` optional): `memory_upsert`,
`memory_delete`, `memory_search`,
`memory_get`, `memory_backlinks`, `memory_coherent`, `memory_upvote`, `memory_stats`,
`memory_dupes`, `memory_reembed`, `project_upsert`, `project_get`, `project_list`.

- **Each tool states its own law in its own description** — that is what the model actually
  reads, so nothing is written twice. The §9 write policy rides on `memory_upsert`'s
  description; the read policy on `memory_search`'s; the feedback *trigger* on `memory_upvote`'s,
  with its firing cue on `memory_get`'s (the read that hands back a body the model consumes).
  The cue rides **both** `memory_get`'s description **and** its returned payload (a `_nudge`
  field): the model's attention sits on the body it just read, so the ask must live there — a
  trigger fires only where attention sits at the moment of use. Curation is announced once as a
  cross-tool duty in the server-level **instructions**, alongside the identity and the shared
  **smart-caveman** style (defined once there, referenced by the write tool).
- **A tool's description carries policy; its parameter schemas carry the per-field
  definitions** — the two never restate each other. So `memory_upsert`'s description holds the
  write *rules* (one topic, trim, merge); each field's `key`/`title`/`body`/`source`/`tags`
  meaning lives in that field's schema.
- `memory_search` takes a semantic `query`, a lexical `pattern` (+ `mode`), the ranking knobs
  `k`/`max_distance`/`expand`, the `fields` projection (default `key` only), and the structured
  filters `tags` (AND), `min_chars`/`max_chars` and `coherent` (§10); there is still no `key` leg —
  `memory_get` reads a known key.
- The list-shaped reads return **positional JSON** to keep the LLM-facing payload small: a
  top-level `cols` header names the fields and the rows are bare arrays, **one row per line**
  (whitespace is insignificant to a parser, so a raw result is scannable, not one blob). The
  `cols` header is self-describing (the model can't mis-map columns) while field names aren't
  repeated per row; distances are rounded to 4 decimals. The convention is announced **once**,
  in the server **instructions** — individual tool descriptions don't restate it. `memory_search` →
  `{"cols":["key",…],"hits":[[…],…]}` (`key` default; `fields` adds columns per §10); link `neighbors`
  (with `expand>0`) carry their own `cols`/`rows` table. `memory_backlinks` →
  `{"cols":["key","title"],"backlinks":[[…],…]}`. `--json`/core keep the keyed form.
- `memory_coherent` records the §9 split-review verdict for a key (`true` = one topic, `false` =
  a grab-bag, omit/`null` = unjudged); an upsert resets it to unjudged.
- `memory_upvote` takes one or more `keys` (`{keys:[…]}`) and bumps `times_useful` for each that
  exists — a feedback sweep in one call. It returns `{status, upvoted:[{key,times_useful}], missing?}`;
  a key that does not exist is reported in `missing` (not fatal), but a sweep that hits **nothing**
  (or an empty list) is a tool error, so a typo is surfaced rather than silently zero.
- `memory_stats` returns the read-only store digest (counts, size, tag/link shape, top tags, usage
  counters, most-useful memories, **hot-but-unvoted** candidates, age) as one keyed JSON object —
  the same `Stats` the `stats` CLI command emits (`store::stats::compute`); no bodies. `unvoted_hot`
  lists `(key, visits)` for memories read at least twice but never marked useful — the store's own
  blind spot, surfaced so it can be judged.
- `memory_get` is the **only** read that returns a body, and the way to read a known `key`. Its
  result carries a `_nudge` field prompting `memory_upvote` when the memory shaped the task.
- `memory_dupes` is the **finder for the §9 merge rule**: a read-only audit returning memory
  *pairs* whose document embeddings sit within a tight cosine ceiling (default `0.05`), nearest
  first, `key`/title per side (`store::dupes::find` — one KNN pass per memory, pairs deduped).
  A **finder, not a judge**: because `key`+tags are folded into the document, merely-related
  memories cluster too, so candidates are judged (`memory_get` both) before merging. No bodies.

**CLI** — the shipped verbs, and nothing else:

- `ellmephant` (no subcommand) / `ellmephant mcp [--stdio]` → the **MCP server**; HTTP on the
  configured port (8888) unless `--stdio`. This is the whole read/write path; the tool surface is
  §9–§10.
- `ellmephant doctor` → paths, schema version and derived-index health: vectors, links, tags,
  projects, and every kind of rot.
- `ellmephant model` → weights cache: `status` | `fetch` | `rm` (§6.5).

An interactive **REPL** and the one-shot store verbs (`add`, `search`, `show`, `list`, `stats`,
`coherent`, `tags`, `links`, `rename`, `reembed`) were **removed**, deliberately: a human-facing
CLI is not a goal (§2), MCP is the read/write path, and anything scriptable is better served as a
client of it. `doctor` is the read-only digest that survives; the `stats` digest of §14 is the
`memory_stats` tool.
- `ellmephant mcp` → MCP server; Streamable HTTP on `port` (default 8888) at `/mcp`; `--stdio` for stdio.

**Portability:** the store is one file (`~/.ellmephant/memory.db`), so moving a machine is a copy.
`reembed` rebuilds the vectors for a new model or dimension; `rename` fixes keys.

## 12. Config & paths

- `~/.ellmephant/memory.db` — the store.
- `~/.ellmephant/config.toml` — model, dim, precision, instruction, defaults (flags override).
- `~/.ellmephant/models/` — weights cache, laid out per §6.1 (`<owner>--<repo>/<sha>/`). Not
  `HF_HOME`: we own this layout so `doctor`/`gc` and `--model-dir` stay simple.
- `meta` table mirrors model/dim/instruction for drift detection at startup.

Model config keys (all optional; flags override, and `ELLMEPHANT_OFFLINE=1` overrides `offline`):

```toml
model      = "Qwen/Qwen3-Embedding-0.6B"  # HF repo id, or a dir name under models/
revision   = "main"                       # resolved once to a commit sha and pinned
cache_dir  = ""                           # default ~/.ellmephant/models
offline    = false                        # true == never open a socket (§6.4)
precision  = "bf16"                       # bf16 | f16
device     = "cpu"                        # cpu | metal
instruction = "Given a question, retrieve relevant memories"
dim        = 1024
default_k  = 5
default_max_distance = 0.6                # KNN cosine-distance ceiling; null = off
port       = 8888                         # MCP HTTP port; `mcp --stdio` ignores it
```

Validated at load: an unknown `device`/`precision` or `dim = 0` fails with one message before
any model is touched; flags are re-checked the same way after they override the file.

## 13. Performance expectations

- Embed: ~0.3–1 s/short text on CPU, ~50–150 ms under Metal. Fine — writes are rare, search
  embeds only the query.
- **Cold start is the real cost:** ~1–3 s to load ~1.2 GB + build the graph. Hence the
  long-lived process. Warmup one throwaway embed at startup.
- KNN at our scale (10^3–10^6 vectors) is sub-ms — the index is never the bottleneck.
- **`bench` (P10)** replaces these estimates with numbers: it prints load time, embed latency
  and semantic-search latency (mean/p50/p95) at the store's current row count. `bench --golden`
  scores a JSONL query set (recall@k + MRR, gate recall@3 ≥ 0.8); `bench --sweep` walks `k`. The
  seed set lives in `tests/golden/`; the shipped default is `k=5`.

## 14. Risks

- **False-memory hazard:** semantic neighbors can be confidently *wrong*. Mitigate: return the
  distance, keep tags honest, don't over-trust, use one-hop expansion for context.
- **Fat returns regress token cost** below plain `grep` — keys-not-bodies is a *requirement*, and
  even `title`/`tags` are asked for (`fields`), never sent unbidden.
- **Write quality governs everything** — low friction means more (and sloppier) writes.
- **bf16 CPU emulation** may be slow → `--precision f16`.
- **Loss of greppability** (SQLite isn't `rg`) — retrieval is semantic; the store is one copyable
  file, so external tools can still read it (or the JSON export).
- **Key collisions / renames** — need deliberate keys and a real `rename` path.
- **Model cache drift/corruption** — `main` moving, a truncated 1.2 GB download, a bad disk.
  Mitigate: sha-pinned revision + per-file sha256 in the manifest, atomic per-file writes,
  load-time size check with `doctor --verify` for the full hash (§6.3–6.4).

## 15. Open questions (resolved during the build)

1. **Golden eval set + `bench`** — shipped (P10): `tests/golden/` (corpus + ≥20 questions),
   `bench` / `--golden` / `--sweep`; default `k=10`; recall@3 gate 0.8. Grown from real usage.
2. **MCP tool schema + annotations** — shipped (P7): 9 tools with JSON-Schema params and
   read-only / destructive / idempotent hints (`src/mcp`).
3. **Rename implementation + collision policy** — shipped (P9) in the store layer: one transaction
   rewrites `[[old]]` and repoints edges; refuses to clobber an existing key
   (`store::links::rename`). No interface calls it (§8).
4. **Daemon vs not** — decided: no daemon in v1. The MCP server keeps the encoder warm, which is
   what the daemon would have bought (PLAN §2 #4).
5. **Migration mechanics** — shipped (P1): `user_version` steps in `db::migrate`. A fresh store
   applies `0001…0005` in order, so it opens at the current schema (**v5** today); an existing store
   is stepped up to it on open.
6. **Tags** — decided: stored on each row (comma-joined) and folded into the document embedding;
   the vocabulary and its counts come from the `memory_stats` tool (and `doctor`). Dangling
   `[[key]]` targets are counted by `doctor`.
7. **Persist `embed_text` vs recompute** — decided: recompute. The per-row `hash`
   (`store::row_hash`) decides when a re-embed is needed; nothing extra is stored.
8. **`--expand` hop semantics + bundle shape** — shipped (P5): breadth-first over
   `memory_link` in both directions, surfaced as `search::Bundle { hits, neighbors }`.
9. **Metal wiring** — shipped (P3): `--device cpu|metal`; CPU runs f32 (Accelerate), so
   `--precision` applies to Metal only.
10. **Concurrency** — shipped: one writer connection per `Session`; the MCP server serialises
    requests through a mutex (SPEC §13 #10).

## 16. Proposed build order

1. Crate skeleton, config, paths, `meta`, logging.
2. Migration 0001 (schema, indexes).
3. **Model cache** — fetch/pin/verify weights, offline, `model status|fetch|rm` (§6).
4. Embedder: candle Qwen3 load + embed projection + pooling.
5. Write path: upsert, tags, links, one document vector (skip the re-embed when the document hash is unchanged).
6. Read path: semantic phrase → KNN → hydrate.
7. CLI REPL + one-shot flags — **built, then removed** (MCP is the interface; §11).
8. MCP server (HTTP default, stdio option).
9. `reembed` and the dangling-link audit — shipped; `rename` lives in the store layer with no
   interface today.
10. `bench` + golden queries; tune.

## 17. Glossary

- **Atomic / atom** — one memory row; a single fact. The storage + embedding unit.
- **Memory** — one body of text holding closely related facts: a `key` (identity *and* link
  target), a `title` (the gist), a `body` (the facts), `tags` and a `source`. Found by meaning or
  by a literal/regex pattern, read whole by `memory_get`, cross-linked with `[[key]]`.
- **Project** — an activity completed over time: a plain-name `key` (shared with `memory` if it
  wants one), a `title`, a `goal`, mutable
  `state` prose, a `status`, an `owner` set, `tags`, an ordered `preload`, and `start_date`/`end_date`
  (the project's own dates; `status` says how it ended, `end_date` only says when). Listed by field
  (`project_list`), read whole by `project_get`. Never embedded (§7.1).
- **Preload** — a project's ordered list of memory keys to load as context for working in it
  ("to work in here, load these"): the boot set, generalized. The order is the payload. The
  *relation* is `preload`; the column that stores it is `mem_preload`.
- **Membership** — a memory belongs to a project by carrying the `prj:<key>` tag, intersected with
  topic tags by the ordinary `tags` filter (`prj:seaweedfs` + `storage` is "the storage memories
  belonging to seaweedfs"). No relation, no per-memory limit, nothing to maintain (0007).
- **Derived view / profile** — a composed answer built at query time from atoms; never stored.
- **Key** — the global-unique slug; identity *and* link target (wiki semantics).
- **Title / body** — the one-liner and the fact; stored literally and folded into the document embedding.
- **Tag** — an aspect of the subject (`storage`, `gotchas`); stored on the row and folded into the embedding.
- **Smart-caveman** — the writing style: drop filler, articles and glue words; keep signal.
- **Document** — the text embedded for a row: `key`, tags, title and the link-stripped body.
- **store ≠ embed** — the row keeps full text; the vector is over a link-stripped projection.
- **Semantic** — the phrase-match method: embed the query and take the KNN. The ranked leg; the
  literal/regex `pattern` is the other one, and the two do not fuse (§10).
- **Model cache** — the external weights dir (`~/.ellmephant/models/<owner>--<repo>/<sha>/`), a
  flat, sha-pinned, manifest-backed copy of the encoder's files; data, not code (§6.1).
