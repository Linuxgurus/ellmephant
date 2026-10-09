# llmphant

Single-binary, SQLite-backed long-term memory for AI agents. The embedder engine is
compiled in, one SQLite file is the source of truth, and a *rebuildable* vector +
full-text + link index sits on top of it. It speaks MCP (so Cline, TypingMind and
anything else share one memory) and ships a REPL for the human.

- **`SPEC.md`** — the decision record: what we decided and why.
- **`PLAN.md`** — the build plan: verified crates, phases, acceptance criteria.

## Status

Phase 1–2 skeleton. The tree, the store bootstrap, the schema and its FTS triggers
are real and tested; the embedder, search, REPL, MCP server and portability layers
are typed stubs that return `NotImplemented` until P3+ lands. Nothing here needs a
model download or a network connection.

| Area | State |
|---|---|
| `config`, `error`, `db` (pragmas + migration runner) | done |
| `migrations/0001_init.sql` (schema, FTS triggers, links, meta) | done |
| `embed::projection`, `store::{slug,links,tags}`, `search::rrf`, `HashEmbedder` | done |
| candle Qwen3 encoder (P3), `sqlite-vec` / `vec0` (P2, P5) | stubs |
| search, REPL, MCP, export/import, bench | stubs |

`memory_vec` deliberately lives in a *later* migration than `0001_init.sql`: it needs
the `sqlite-vec` extension registered in-process, and keeping the pure-SQLite schema
separate is what lets `cargo test` run offline.

## Build & test

```sh
cargo build                 # or: make build
cargo test                  # unit + integration, offline, no model
cargo run -- doctor         # paths, schema version, index status
cargo run --example db_bootstrap
```

`doctor` (and every command) honours `LLMPHANT_HOME`, so you can exercise a throwaway
store without touching `~/.llmphant`:

```sh
LLMPHANT_HOME=/tmp/lp cargo run -- doctor
```

## Docker

```sh
make docker-build           # multi-stage; runtime is debian-slim + one binary
docker run --rm -it -v llmphant-data:/data llmphant:dev doctor
```

The store lives at `/data` inside the container (`LLMPHANT_HOME=/data`). Weights are
*data*, not code: mount or download them rather than baking ~1.2 GB into the image.

## Layout

```
src/
├── main.rs            clap dispatch
├── lib.rs             module tree
├── error.rs           thiserror
├── config.rs          ~/.llmphant paths, config.toml
├── db/                open, pragmas, migrate()
├── embed/             Embedder trait, qwen3, tokenize, projection
├── store/             add/get over memory + derived indexes (slug, tags, links)
├── index/             vec0 (KNN) and fts5 (BM25) legs
├── search/            hybrid orchestration + RRF
├── cli/               one-shot subcommands, repl, output formatting
├── mcp/               MCP stdio server
└── portable/          JSONL export/import
migrations/            NNNN_*.sql, embedded with include_str!
tests/                 integration tests + tests/golden/queries.jsonl
examples/              spikes and throwaway probes
```

## Testing

- **Unit** (`src/`): projection, slug, link extraction, tag normalisation, RRF merge,
  the deterministic `HashEmbedder`.
- **Integration** (`tests/`): a real temp-file SQLite store — schema introspection,
  pragmas, migration idempotence, FTS trigger synchronisation, FK cascades, UNIQUE
  keys; plus black-box CLI tests via `CARGO_BIN_EXE_llmphant`.
- **Fake embedder**: store/search tests inject `HashEmbedder`, so `cargo test` never
  loads weights. Real-model and oracle comparisons are `#[ignore]`d and land in P3.
- **Golden eval** (`tests/golden/queries.jsonl`): retrieval-quality gate for P10.
