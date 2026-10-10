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

- **Atoms, not essays — in smart-caveman.** One memory = one fact, terse and self-contained:
  drop filler, articles and glue words. Focused text embeds sharply and edits surgically; broad
  summaries rot. Derived views are computed at query time, not stored.
- **`store ≠ embed`.** The row keeps the full, literal text; one **document** (`key` + tags +
  title + link-stripped body) is embedded. The embedding strategy can change without losing what
  was written.
- **Everything but `memory` is derived.** The vector index and the link graph are rebuildable
  from the rows (`ellmephant reembed`). The relational store must never lie; there is no FTS index
  and no tag table to keep in sync.
- **Local-first, offline-capable.** `--offline` opens no socket. Weights are data fetched once,
  not code compiled in.
- **Titles, not dumps.** Search returns `key`/`title`/`tags` and (for a semantic phrase) the
  cosine distance, so the caller can judge confidence and control token cost; the body is a
  deliberate `memory_get` away.
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
              search  —  one semantic phrase (embed query → KNN)
                                  ▼
                   index::vec  (one document vector per row)
                                  ▼
      SQLite  memory.db  —  `memory` rows are the source of truth
      derived & rebuildable:  memory_vec · memory_link
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
| Read path | `search`, `index/vec` | one semantic phrase → KNN over the document vectors |
| Interfaces | `cli`, `mcp` | REPL + one-shot, MCP server (HTTP default, stdio option) |
| Tooling | `model`, `bench` | weights cache, latency + golden-eval measurements |

## Storage model

One SQLite file is the source of truth. The `memory` table holds the rows; two derived indexes
sit on top and can be rebuilt from the rows at any time:

- `memory_vec` — `sqlite-vec` `vec0` KNN index over the document embeddings.
- `memory_link` — the `[[key]]` link graph, forward edges and backlinks.

A `meta` table mirrors `dim` / `model` / `instruction` for drift detection at startup, and the
per-row `doc_hash` decides when a row needs re-embedding (any change to the key, tags, title or
body changes the document). Ids are local; `key` is the stable identity (upsert target and link
target). Dimensions are frozen at `CREATE`; changing the embedder's width is a deliberate
`reembed`, not an implicit migration.

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
ellmephant embed "where do the logs live" --query   # dim + load/embed latency + norm
```

## Search

A search matches a single **semantic phrase**: the query is embedded (with the instruction
prefix) and the KNN of the whole store is taken, capped by `--max-distance` and truncated to
`-k`. `--expand N` appends the N-hop link neighbourhood. There are no key/tag filters and no
regex leg — `key`/`tags` are identity and lookup, and are part of the embedded document.

```sh
ellmephant search "where do the logs live"             # semantic, k=5
ellmephant search "portainer-notes-gw"                 # key/tag tokens are in the embedding
ellmephant search "backups" --expand 1                 # + linked neighbours
```

Results are ordered by ascending distance. Each hit carries `key`, `title`, `tags` and
`distance` — never the body. The read policy is titles and tags, never full dumps; a body is a
`show`/`memory_get` away.

## Wiki links

Links are authored inline in the body as `[[key]]`. At write time they become a derived
`memory_link` edge; search finds forward links and backlinks, and `--expand` walks them. Links
are **by key, never by id**, so they survive a `reembed` and forward references. A `[[key]]`
with no target is reported by `ellmephant links` as a warning rather than an error. `ellmephant
rename OLD NEW` rewrites the tokens and repoints the edges in one transaction.

## Maintenance

The `memory` table is the source of truth; every index on top of it is rebuildable.

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

`bench` measures load/embed/search latency and gates retrieval quality.

```sh
ellmephant bench                            # load / embed / search latency at the current row count
ellmephant bench --golden                   # + recall@k & MRR over <home>/golden/queries.jsonl
ellmephant bench --golden --golden-file Q   # score a specific golden set
ellmephant bench --sweep                    # walk k
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
├── index/             the vec0 (KNN) document-vector table
├── search/            the semantic phrase search over the store
├── cli/               subcommands, repl, session, output formatting
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
