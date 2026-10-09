# llmphant

Single-binary, SQLite-backed long-term memory for AI agents. The embedder engine is
compiled in, one SQLite file is the source of truth, and a *rebuildable* vector +
full-text + link index sits on top of it. It speaks MCP (so Cline, TypingMind and
anything else share one memory) and ships a REPL for the human.

- **`SPEC.md`** — the decision record: what we decided and why.
- **`PLAN.md`** — the build plan: verified crates, phases, acceptance criteria.

## Status

Phases 1, 2, 2.5 and 4 are real and tested: bootstrap, schema + FTS triggers, the
`sqlite-vec` (`vec0`) index, the model weights cache, and the write path. The candle
Qwen3 encoder (P3), search (P5), REPL/MCP and portability layers are still typed stubs
that return `NotImplemented`. `cargo test` needs no model and no network.

| Area | State |
|---|---|
| `config`, `error`, `db` (pragmas + migration runner) | done |
| `migrations/0001_init.sql` (schema, FTS triggers, links, meta) | done |
| `index::vec` — in-process `sqlite-vec` (`vec0`), KNN | done |
| `model` — weights cache: resolve, fetch, verify, offline (SPEC §6) | done |
| `store` — write path (upsert, tags, links, vec) | done |
| candle Qwen3 encoder (P3) | stub |
| search (P5), REPL (P6), MCP (P7), export/import (P8), bench (P10) | stubs |

`memory_vec` is a *derived* index created on demand by `index::vec::ensure` (its
dimension is an embedder property, not schema); keeping it out of `0001_init.sql` is
what lets `cargo test` run offline with no extension.

## Model weights

Weights are *data*: fetched on first use into `~/.llmphant/models/`, or supplied with
`--model-dir`. The cache is flat and sha-pinned (`SPEC.md` §6), so once warm, runs need
no network.

```sh
llmphant model status              # what is cached (never touches the network)
llmphant model status --verify     # re-hash every file (slow)
llmphant model fetch               # download / repair; idempotent
llmphant model rm --all            # reclaim space
llmphant --offline doctor          # opens no socket
```

`--model-dir DIR` uses an existing flat directory of weights and implies `--offline`;
`LLMPHANT_OFFLINE=1` forces it globally. Fetching is the only network code in the binary.

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
*data*, not code: mount or `llmphant model fetch` them rather than baking ~1.2 GB into the
image. The flat, sha-pinned cache layout is specified in `SPEC.md` §6.

## Layout

```
src/
├── main.rs            clap dispatch
├── lib.rs             module tree
├── error.rs           thiserror
├── config.rs          ~/.llmphant paths, config.toml
├── model/             weights cache: resolve, fetch, verify (SPEC §6)
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
