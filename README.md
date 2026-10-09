# llmphant

Single-binary, SQLite-backed long-term memory for AI agents. The embedder engine is
compiled in, one SQLite file is the source of truth, and a *rebuildable* vector +
full-text + link index sits on top of it. It speaks MCP (so Cline, TypingMind and
anything else share one memory) and ships a REPL for the human.

- **`SPEC.md`** — the decision record: what we decided and why.
- **`PLAN.md`** — the build plan: verified crates, phases, acceptance criteria.

## Status

Phases 1, 2, 2.5, 3, 4 and 5 are real and tested: bootstrap, schema + FTS triggers, the
`sqlite-vec` (`vec0`) index, the model weights cache, the candle Qwen3 encoder, the write
path, and hybrid search. REPL/MCP and the portability layers are still typed stubs that
return `NotImplemented`. `cargo test` needs no model and no network.

| Area | State |
|---|---|
| `config`, `error`, `db` (pragmas + migration runner) | done |
| `migrations/0001_init.sql` (schema, FTS triggers, links, meta) | done |
| `index::vec` — in-process `sqlite-vec` (`vec0`), KNN | done |
| `model` — weights cache: resolve, fetch, verify, offline (SPEC §6) | done |
| `embed::qwen3` — candle encoder: last-token pool, L2, query instruction (P3) | done |
| `store` — write path (upsert, tags, links, vec) | done |
| `search` — hybrid BM25 ∪ KNN fused with RRF, `--expand`, `--max-distance` (P5) | done |
| REPL (P6), MCP (P7), export/import (P8), bench (P10) | stubs |

The encoder is behind the default-on `encoder` cargo feature; build with
`--no-default-features` for a store-only binary that links no ML stack.

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

## Embeddings

The encoder is Qwen3-Embedding-0.6B on candle — 1024-d, **last-token** pooling with left
padding, L2-normalised (`SPEC.md` §6). Queries get the `Instruct: <task>\nQuery: <q>`
prefix; documents are bare. `--device cpu|metal` and `--precision bf16|f16` override the
config.

Two realities (PLAN §1 F4): the Hub file names tensors without candle's `model.` prefix
(bridged in `embed::qwen3`), and candle's CPU matmul runs through Accelerate, which is
**f32-only** — so CPU always runs f32, and `--precision` applies to `--device metal`.

```sh
llmphant embed "where do the logs live" --query   # dim + load/embed latency + norm
```

`cargo test` never loads the model: the store/search layers inject the deterministic
`HashEmbedder`, and the real-weights check is `#[ignore]`d (`tests/embed.rs`).

## Search

`hybrid` (the default) runs both legs and fuses them with RRF (k=60); `--mode vec|fts` runs
one. `--mode fts` needs no weights and no network. `--expand N` appends the N-hop link
neighbourhood; `--max-distance` caps the KNN leg; `--scope` prefilters both legs.

```sh
llmphant search "where do the logs live"            # hybrid, k=10
llmphant search "portainer-notes-gw" --mode fts     # exact identifier, no model
llmphant search "gateway" --scope infra --json
llmphant search "backups" --expand 1                # + linked neighbours
```

Every hit carries its fused `score` (and `distance` when the KNN leg contributed) so the
caller can judge confidence — the read policy is snippets + scores, never dumps (SPEC §10).

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
  loads weights. Real-model checks are `#[ignore]`d (`tests/embed.rs`).
- **Golden eval** (`tests/golden/queries.jsonl`): retrieval-quality gate for P10.
