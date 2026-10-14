# Engineering notes

Internals, design rationale, and development workflow. For the normative specification see
[`SPEC.md`](SPEC.md); for build and install steps see [`INSTALL.md`](INSTALL.md).

- [Design principles](#design-principles)
- [Architecture](#architecture)
- [Storage model](#storage-model)
- [Embeddings](#embeddings)
- [Search](#search)
- [Wiki links](#wiki-links)
- [Maintenance](#maintenance)
- [Model weights cache](#model-weights-cache)
- [Bench & tuning](#bench--tuning)
- [Repository layout](#repository-layout)
- [Development & testing](#development--testing)

## Design principles

The decisions the code obeys:

- **Topics, not essays — in smart-caveman.** One memory = one topic: terse and self-contained,
  close facts one per line; distinct facts split. Drop filler, articles and glue words. A
  coherent topic embeds sharply and edits surgically; grab-bag summaries smear the vector and
  rot. Derived views are computed at query time, not stored.
- **`store ≠ embed`.** The row keeps the full, literal text; one **document** (`key` + tags +
  title + link-stripped body) is embedded. The embedding strategy can change without losing what
  was written.
- **A memory is one body of closely related facts**, embedded as one document and matched by
  meaning — which is what makes voting, ranking and the coherence verdict meaningful.
- **A project is an activity completed over time.** Work with a beginning, a current state and a
  place to stand: identity, fields, and its relations to memories. Listed by field and read whole —
  which is what makes embedding it pointless, so it carries no vector, no votes and no verdict of
  its own.
- **Everything else is derived.** `memory_vec` (the KNN index) and `memory_link` (the `[[key]]`
  graph) are computed from the rows and rebuildable at any time (the `memory_reembed` tool). The
  relational store must never lie, so there is no FTS index and no tag table to keep in sync.
- **The two kinds live in different tables, split where the machinery changes** — not on a hunch:
  "project = memory + type" would ship cheaply and model nothing (SPEC §7.1). **Inside a structure,
  a relation earns its own table only when its invariant is cross-row:** `contains` has one (a memory
  has one home, *anywhere* — a column cannot see other rows), `preload` does not (ordered, and order
  is a property of the list, so it is a column). A project's key is a **plain name** — a memory may
  share it — so the seam that keeps this one store is the *reference*: `[[prj:x]]` in a body, and
  `memory_link.to_type` on the edge.
- **Local-first, offline-capable.** `--offline` opens no socket. Weights are data fetched once,
  not code compiled in.
- **Keys, not dumps.** A hit is `key` alone — the caller names the extra `fields` it wants
  (`title`, `tags`, `coherent`, `chars`, `distance`) — so a search can never bloat context, and the
  body is always a deliberate `memory_get` away.
- **One binary, no runtime.** No interpreter, no service, no shared library.

## Architecture

```
   clients                                    you
   Cline · TypingMind · any MCP client        doctor · model (ops only)
              │                                        │
              │  MCP over HTTP (or stdio)              │  one shared core
              └───────────────────┬────────────────────┘
                                  ▼
                 Session  —  one writer connection, lazy encoder
                                  ▼
              search  —  one semantic phrase (embed query → KNN)
                                  ▼
                   index::vec  (one document vector per row)
                                  ▼
      SQLite  memory.db  —  the truth:  `memory` (facts) · `project` (structures)
      everything else derived & rebuildable:  `memory_vec` · `memory_link`
                                  ▼
      embed::qwen3  (candle, in-binary)  —  weights under ~/.ellmephant/models/
```

Read bottom-up: the encoder turns text into vectors; SQLite holds the rows and the derived
index; search embeds a phrase and takes the KNN of the whole store; `Session` is the one shared
core; the CLI and the MCP server are two faces on top of it. Everything above `memory.db` can be
deleted and rebuilt.

Five layers, one crate:

| Layer | Module | Responsibility |
|---|---|---|
| Storage | `db`, `migrations/` | open SQLite, pragmas, schema, `meta` kv |
| Write path | `store`, `embed`, `index/vec` | upsert a row: key, one hash, tags, links, one vector |
| Structures | `store::project` | projects: identity, the ordered preload (membership is a tag) |
| Read path | `search`, `index/vec` | one semantic phrase → KNN over the document vectors |
| Interfaces | `cli`, `mcp` | MCP server (HTTP default, stdio option); `doctor` + `model` for operations |
| Tooling | `model`, `bench` | weights cache, latency + golden-eval measurements |

## Storage model

One SQLite file holds two kinds of thing, and both are truth:

- **`memory`** — memories: one body of closely related facts each, embedded as one document and
  matched by meaning.
- **`project`** — projects (v5): an activity completed over time, with its own fields and its
  ordered `mem_preload` list as a column. Its one relation to memories is *preload*; membership is
  not a relation at all — a memory joins a project by carrying the `prj:<key>` tag (v7 dropped the
  `project_memory` table that used to enforce a single home; see SPEC §7.1). A project carries no
  embedding, no telemetry and no coherence verdict — those belong to facts, the thing matched by
  meaning.

Everything else is **derived** from those rows and can be rebuilt at any time:

- `memory_vec` — `sqlite-vec` `vec0` KNN index over the document embeddings.
- `memory_link` — the `[[key]]` link graph, forward edges and backlinks.

A `meta` table mirrors `dim` / `model` / `instruction` for drift detection at startup, and the
per-row `doc_hash` decides when a row needs re-embedding (any change to the key, tags, title or
body changes the document). Ids are local; `key` is the stable identity (upsert target and link
target). Dimensions are frozen at `CREATE`; changing the embedder's width is a deliberate
`reembed`, not an implicit migration.

Inside a structure, a relation earns its own table only when its invariant is cross-row: `contains`
has one (a memory has one home, *anywhere* — a column cannot see other rows), `preload` does not
(a memory may sit in many projects' lists, and order is a property of the list, so it is a column
rather than a `position`-ordered rowset). Project keys are plain names, so what lets a key cross
between the two graphs is the reference: `[[prj:x]]` in a body, and `memory_link.to_type` naming the
table the key lives in. `links::dangling` resolves per kind.

## Embeddings

The encoder is Qwen3-Embedding-0.6B on candle: 1024 dimensions, **last-token pooling** with left
padding, L2-normalised. Queries get the `Instruct: <task>\nQuery: <q>` prefix; documents are
embedded bare.

Two implementation notes:

- The Hub stores tensors without candle's `model.` prefix; the mapping is bridged in
  `embed::qwen3`.
- candle's CPU matmul runs through Accelerate, which is **f32-only**, so CPU always runs f32 and
  `--precision` (bf16/f16) only affects `--device metal`.

`[[wiki links]]` are stripped from the embedding projection, so renaming a link target never
invalidates a stored vector.

```sh
# There is no "embed one string" entry point: the encoder is reachable only through `memory_search`
# and `memory_reembed`, which reports the dimension and model id it has just written with.
```

## Search

A search is a **semantic phrase plus optional structured filters**, which **intersect** (a hit
must satisfy every one given). The phrase is embedded (with the query-side instruction prefix)
and the KNN of the whole store is taken, capped by `max_distance` and truncated to `k`;
**with no phrase it is a pure filter** over the store (every matching row, key order, uncapped).
Filters apply *before* the `k` cap, so a result shorter than `k` means the distance ceiling cut
it, not `k`. There is no regex/key leg — `key`/`tags` are part of the embedded document.

```jsonc
// `memory_search` parameters. Every one is optional but the query — or omit the query entirely
// and the filters stand alone, returning every matching row in key order.
{ "query": "where do the logs live" }                   // semantic, k=5 by default
{ "query": "portainer-notes-gw" }                       // key/tag tokens are in the embedding
{ "query": "backups", "expand": 1 }                     // + linked neighbours
{ "tags": ["k8s", "gotchas"] }                          // phrase-less: AND filter, all rows
{ "min_chars": 1000, "coherent": "!true" }              // the §9 split-review queue
```

Results are ordered by ascending distance. A hit carries no body: the terminal shows
`key`/`title`/`tags`/`distance`; the MCP wire returns **`key` alone by default** unless the
caller names extra `fields` (§10 of `SPEC.md`). The read policy is **keys by default, titles and
tags and distances when asked, never dumps**; a body is a `show`/`memory_get` away.

## Wiki links

Links are authored inline in the body as `[[key]]`, or `[[prj:key]]` to point at a project: links
live in memory bodies, so a bare token means a memory and only the other kind needs saying. `:`
cannot occur in a slug, so no name can squat on the marker, and the type rides on the **edge**
(`memory_link.to_type`) rather than on the key — which is what lets a project and a memory share a
name. At write time a link becomes a derived
`memory_link` edge; search finds forward links and backlinks, and `--expand` walks them. Links
are **by key, never by id**, so they survive a `reembed` and forward references. A `[[key]]`
with no target is reported by `doctor` as a count rather than an error (per kind: a
missing memory and a missing project are different rot). `ellmephant
rename OLD NEW` rewrites the tokens and repoints the memory edges in one transaction.

## Maintenance

The rows are the source of truth — `memory`, `project`; every index on top of them
is rebuildable.

- `reembed` drops and recreates the vec0 table when the embedder's width changes, recomputes
  every row's document vector, rewrites each row's `doc_hash`, and records
  `dim`/`model`/`instruction` in `meta`. This is the deliberate path that the frozen-dimension
  guard in `index::vec::ensure` refuses to take implicitly.
- `rename` runs in one transaction and refuses to clobber an existing key.

## Model weights cache

Weights are data, fetched into a flat, sha-pinned cache and then used offline (`SPEC.md` §6). The
layout is our own (`~/.ellmephant/models/<owner>--<repo>/<sha>/`, or `$HF_HOME`-style if configured)
rather than `HF_HOME`, so `doctor`, `gc`, and `--model-dir` stay simple. Only the fetch is network
code; `--offline` and `--model-dir DIR` (which implies offline) never open a socket. A corrupt
file is detected via the manifest rather than silently fed into the vectors.

## Bench & tuning

The `bench` integration test measures load/embed/search latency and gates retrieval quality.

```sh
cargo test --test bench                    # latency + recall@k & MRR over tests/golden/
cargo test --test bench -- --nocapture     # print the detail rather than just the verdict
```

The golden file is JSONL, one object per line: `{"query": "…", "expect": ["key", …], "k": 3}`
(blank lines and `#` comments are ignored). It loads the encoder, so it needs warm weights (or
`--offline` against a populated cache). A seed corpus and ≥20 questions live in `tests/golden/`.

## Repository layout

```
src/
├── main.rs            clap dispatch
├── lib.rs             module tree
├── config.rs          paths + config.toml merge       error.rs    thiserror types
├── model/             weights cache: resolve, fetch, verify
├── db/                open, pragmas, migrate(), meta key/value
├── embed/             Embedder trait, qwen3, tokenize, projection
├── store/             add/get + projects + derived indexes (slug, tags, links) + maintain (reembed)
├── index/             the vec0 (KNN) document-vector table
├── search/            the semantic phrase search over the store
├── cli/               doctor, model, session (the MCP server is the interface)
├── mcp/               MCP server (HTTP default; stdio option)
└── bench/             timings, golden eval (recall@k + MRR), sweeps
migrations/            NNNN_*.sql, embedded with include_str!
tests/                 integration tests + tests/golden/{memories,queries.jsonl}
examples/              spikes and throwaway probes
```

## Development & testing

```sh
cargo build                 # or: make build
cargo test --locked         # unit + integration, offline, no model
cargo test --locked --no-default-features   # the store-only build compiles
cargo clippy --all-targets --all-features -- -D warnings
cargo fmt --all
cargo run -- doctor         # paths, schema version, index status
cargo run --example db_bootstrap
```

CI runs the same checks plus a Docker build. `doctor` (and every command) honours
`ELLMEPHANT_HOME`, so you can exercise a throwaway store without touching `~/.ellmephant`:

```sh
ELLMEPHANT_HOME=/tmp/lp cargo run -- doctor
```

Tests never load weights: the store and search layers inject the deterministic `HashEmbedder`
behind the `Embedder` trait, and the real-model check is `#[ignore]`d (`tests/model.rs`). The
golden eval (`tests/bench.rs`) imports the seed corpus with the same fake embedder and asserts
recall@3 clears the 0.8 gate, so the whole suite runs offline in seconds.
