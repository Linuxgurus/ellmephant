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

- **Atoms, not essays.** One memory = one fact, terse and self-contained. Focused text embeds
  sharply and edits surgically; broad summaries rot. Derived views are computed at query time,
  not stored.
- **`store ≠ embed`.** The row keeps the full, verifiable text; the vector is computed over a
  condensed projection (title + body, wiki links stripped). The embedding strategy can change
  without losing what was written.
- **Everything but `memory` is derived.** The full-text index, the vector index, and the link
  graph are rebuildable from the rows (`llmphant reembed`). The relational store must never lie.
- **Local-first, offline-capable.** `--offline` opens no socket. Weights are data fetched once,
  not code compiled in.
- **Snippets + scores, never dumps.** Search returns bounded snippets with a fused score and, for
  the vector leg, cosine distance, so the caller can judge confidence and control token cost.
- **One binary, no runtime.** No interpreter, no service, no shared library.

## Architecture

```
   clients                                    you
   Cline · TypingMind · any MCP client        REPL · one-shot CLI
              │                                        │
              │  MCP over HTTP (or stdio)              │  one shared core
              └───────────────────┬────────────────────┘
                                  ▼
                 Session  —  one writer connection, lazy encoder
                                  ▼
              search  —  BM25 ∪ KNN, fused with RRF, hydrated snippets
                    ┌─────────────┴─────────────┐
                    ▼                           ▼
        index::fts  (memory_fts)        index::vec  (memory_vec)
                    └─────────────┬─────────────┘
                                  ▼
      SQLite  memory.db  —  `memory` rows are the source of truth
      derived & rebuildable:  memory_fts · memory_vec · memory_link
                                  ▼
      embed::qwen3  (candle, in-binary)  —  weights under ~/.llmphant/models/
```

Read bottom-up: the encoder turns text into vectors; SQLite holds the rows and the three derived
indexes; search runs the lexical and vector legs and fuses them by rank; `Session` is the one
shared core; the CLI and the MCP server are two faces on top of it. Everything above `memory.db`
can be deleted and rebuilt.

Five layers, one crate:

| Layer | Module | Responsibility |
|---|---|---|
| Storage | `db`, `migrations/` | open SQLite, pragmas, schema + FTS triggers, `meta` kv |
| Write path | `store`, `embed`, `index/vec` | upsert a row: key, hash, tags, links, vector |
| Read path | `search`, `index/fts`, `search/rrf` | hybrid BM25 ∪ KNN, RRF fusion, snippets |
| Interfaces | `cli`, `mcp`, `portable` | REPL + one-shot, MCP server (HTTP default, stdio option), JSONL in/out |
| Tooling | `model`, `bench` | weights cache, latency + golden-eval measurements |

## Storage model

One SQLite file is the source of truth. The `memory` table holds the rows; three derived indexes
sit on top and can be rebuilt from the rows at any time:

- `memory_fts` — FTS5 index (BM25 lexical leg), maintained by triggers.
- `memory_vec` — `sqlite-vec` `vec0` KNN index over the stored vectors.
- `memory_link` — the `[[key]]` link graph, forward edges and backlinks.

A `meta` table mirrors `dim` / `model` / `instruction` for drift detection at startup, and a
per-row `hash` decides when a row needs re-embedding. Ids are local; `key` is the stable
identity (upsert target and link target). Dimensions are frozen at `CREATE`; changing the
embedder's width is a deliberate `reembed`, not an implicit migration.

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
llmphant embed "where do the logs live" --query   # dim + load/embed latency + norm
```

## Search

`hybrid` (the default) runs both legs and fuses them with **RRF (k=60)**; `--mode vec|fts` runs
one. `--mode fts` needs no weights and no network. `--expand N` appends the N-hop link
neighbourhood; `--max-distance` caps the KNN leg (cosine); `--scope` prefilters both legs.

```sh
llmphant search "where do the logs live"            # hybrid, k=10
llmphant search "portainer-notes-gw" --mode fts     # exact identifier, no model
llmphant search "gateway" --scope infra --json
llmphant search "backups" --expand 1                # + linked neighbours
```

BM25 is kept alongside the vector leg because it carries exact tokens — hostnames, ids, paths —
that embeddings can miss. Every hit carries its fused `score` (and `distance` when the KNN leg
participated). The read policy is snippets + scores, never full dumps.

## Wiki links

Links are authored inline in the body as `[[key]]`. At write time they become a derived
`memory_link` edge; search finds forward links and backlinks, and `--expand` walks them. Links
are **by key, never by id**, so they survive export/import and forward references. A `[[key]]`
with no target is reported by `llmphant links` as a warning rather than an error. `llmphant
rename OLD NEW` rewrites the tokens and repoints the edges in one transaction.

## Maintenance

The `memory` table is the source of truth; every index on top of it is rebuildable.

- `reembed` drops and recreates `memory_vec` when the embedder's width changes, recomputes every
  row's vector, rewrites each row's `hash`/`model`/`dim`, and records `dim`/`model`/`instruction`
  in `meta`. This is the deliberate path that the frozen-dimension guard in `index::vec::ensure`
  refuses to take implicitly.
- `rename` runs in one transaction and refuses to clobber an existing key.

## Model weights cache

Weights are data, fetched into a flat, sha-pinned cache and then used offline (`SPEC.md` §6). The
layout is our own (`~/.llmphant/models/<owner>--<repo>/<sha>/`, or `$HF_HOME`-style if configured)
rather than `HF_HOME`, so `doctor`, `gc`, and `--model-dir` stay simple. Only the fetch is network
code; `--offline` and `--model-dir DIR` (which implies offline) never open a socket. A corrupt
file is detected via the manifest rather than silently fed into the vectors.

## Bench & tuning

`bench` measures load/embed/search latency and gates retrieval quality.

```sh
llmphant bench                            # load / embed / search latency at the current row count
llmphant bench --golden                   # + recall@k & MRR over <home>/golden/queries.jsonl
llmphant bench --golden --golden-file Q   # score a specific golden set
llmphant bench --sweep                    # walk k × RRF weight; compare embed projections
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
├── store/             add/get + derived indexes (slug, tags, links) + maintain (reembed)
├── index/             vec0 (KNN) and fts5 (BM25) legs
├── search/            hybrid orchestration + RRF
├── cli/               subcommands, repl, session, output formatting
├── mcp/               MCP server (HTTP default; stdio option)
├── portable/          JSONL export/import
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
`LLMPHANT_HOME`, so you can exercise a throwaway store without touching `~/.llmphant`:

```sh
LLMPHANT_HOME=/tmp/lp cargo run -- doctor
```

Tests never load weights: the store and search layers inject the deterministic `HashEmbedder`
behind the `Embedder` trait, and the real-model check is `#[ignore]`d (`tests/model.rs`). The
golden eval (`tests/bench.rs`) imports the seed corpus with the same fake embedder and asserts
recall@3 clears the 0.8 gate, so the whole suite runs offline in seconds.
