# llmphant

Single-binary, SQLite-backed long-term memory for AI agents. The embedder engine is
compiled in, one SQLite file is the source of truth, and a *rebuildable* vector +
full-text + link index sits on top of it. It speaks MCP (so Cline, TypingMind and
anything else share one memory) and ships a REPL for the human.

- **`SPEC.md`** — the decision record: what we decided and why.
- **`PLAN.md`** — the build plan: verified crates, phases, acceptance criteria.

## Status

Phases 1, 2, 2.5, 3, 4, 5, 6, 7, 8 and 9 are real and tested: bootstrap, schema + FTS triggers,
the `sqlite-vec` (`vec0`) index, the model weights cache, the candle Qwen3 encoder, the write
path, hybrid search, the CLI (one-shot `add`/`search`/`show`/`rm`/`tags`/`rename`/`reembed`/
`links`/`export`/`import` plus an interactive REPL with history and tab-completion), the MCP
stdio server (shared by Cline, TypingMind and anything else), the JSONL portability layer, and
maintenance (`reembed`, `rename`, the dangling-link audit, `tags prune`, a deep `doctor`).
`bench` is still ahead (P10). `cargo test` needs no model and no network.

| Area | State |
|---|---|
| `config`, `error`, `db` (pragmas + migration runner) | done |
| `migrations/0001_init.sql` (schema, FTS triggers, links, meta) | done |
| `index::vec` — in-process `sqlite-vec` (`vec0`), KNN | done |
| `model` — weights cache: resolve, fetch, verify, offline (SPEC §6) | done |
| `embed::qwen3` — candle encoder: last-token pool, L2, query instruction (P3) | done |
| `store` — write path (upsert, tags, links, vec) | done |
| `search` — hybrid BM25 ∪ KNN fused with RRF, `--expand`, `--max-distance` (P5) | done |
| `cli` — one-shot `add`/`show`/`rm`/`tags`, REPL with history + completion (P6) | done |
| `mcp` — MCP stdio server: 8 tools, schemas, annotations, write policy (P7) | done |
| `portable` — JSONL export/import, upsert-by-key, re-embed on hash change (P8) | done |
| maintenance — `reembed`, `rename`, link audit, `tags prune`, deep `doctor` (P9) | done |
| bench (P10) | stub |

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

## Portable

Export/import is JSONL of canonical fields (`key,scope,type,title,body,source,tags,created,updated`)
— no ids, no vectors, so a dump survives a different machine, model revision or dimension.

```sh
llmphant export memory.jsonl            # no model needed
llmphant import memory.jsonl --dry-run  # report pending changes, write nothing
llmphant import memory.jsonl            # upsert by key; re-embed only on hash change
```

Import re-embeds, so it loads the encoder; a second import of the same file is a no-op
(it returns `0` changes). Export is ordered by `key`, so equal content exports byte-identical.

## Maintenance

The `memory` table is the source of truth; every index on top of it is rebuildable (P9).

```sh
llmphant reembed                        # recompute every vector; survives a model/dim change
llmphant rename OLD NEW                 # rewrite [[OLD]] in bodies + repoint link edges
llmphant links                          # audit the graph for dangling [[key]] targets
llmphant links --json
llmphant tags prune [--dry-run]         # drop tags no memory references
llmphant doctor                         # + link/tag/vector health
llmphant doctor --verify                # + full re-hash of the cached weights (SPEC §6.3)
```

`reembed` is the deliberate path that the frozen-dimension guard in `index::vec::ensure`
refuses: it drops and recreates `memory_vec` when the embedder's width changes, rewrites each
row's `hash`/`model`/`dim`, and records `dim`/`model`/`instruction` in `meta`. `rename` is one
transaction — `[[…]]` is stripped from the embed projection, so it never invalidates a stored
vector — and refuses to clobber an existing key.

## MCP

`llmphant mcp` serves the same store to any MCP client over stdio, so Cline, TypingMind and
the REPL all read and write one memory. Register it once per client (point `command` at the
built binary; the client launches it):

```jsonc
{
  "mcpServers": {
    "llmphant": { "command": "/absolute/path/to/llmphant", "args": ["mcp"] }
  }
}
```

Tools: `memory_add`, `memory_update`, `memory_delete`, `memory_search`, `memory_get`,
`memory_backlinks`, `memory_export`, `memory_import`. Read tools carry a read-only
annotation, `memory_delete` is marked destructive, and the `memory_add`/`memory_update`
descriptions carry the SPEC §9 write policy (one fact per row, provenance in `source`,
upsert-by-key, trim-on-edit). `memory_search` returns scored snippets; the KNN leg loads the
model lazily, so `mode=fts` runs with no weights and no network.

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
├── db/                open, pragmas, migrate(), meta key/value
├── embed/             Embedder trait, qwen3, tokenize, projection
├── store/             add/get + derived indexes (slug, tags, links) + maintain (reembed)
├── index/             vec0 (KNN) and fts5 (BM25) legs
├── search/            hybrid orchestration + RRF
├── cli/               subcommands, repl, session, output formatting
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
