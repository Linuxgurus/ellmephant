# llmphant — Build Plan

> Companion to `SPEC.md`. This is the *how*: verified facts, resolved open questions, repo
> layout, dependencies, and a phased build with acceptance criteria. Target is **v1** = the
> spec's Goals (§2), nothing more.

## 0. Scope of v1

Everything in SPEC §2 Goals. Deferred: daemon, cloud/sync, reranking, MRL/multi-dim re-index,
any TUI panes, multi-tenant/auth.

## 1. Verified environment facts (checked, not assumed)

| Thing | Version / finding |
|---|---|
| `rusqlite` | 0.40.2 — `bundled` gives SQLite **in-binary** |
| `sqlite-vec` | 0.1.10-**alpha**.4 — pin exact; the C extension we register in-process |
| `candle-core` / `-nn` / `-transformers` | 0.11.0 (must all match) |
| `candle-transformers::models` | has **`qwen3`**, `qwen2`, `bert`, `quantized_qwen3` — native Qwen3 |
| `fastembed` | 7.1.1 — Qwen3 behind the **`qwen3` feature (candle backend)** |
| `tokenizers` / `safetensors` / `hf-hub` | 1.0-rc / 0.8.0 / 1.0.0 (use `tokenizers` 0.23.x) |
| `rmcp` | 3.5.1 — official Rust MCP SDK exists |
| `rustyline` / `tabled` / `clap` | 18.0 / 0.22 / 4.6 |

### Two findings that change the build

**F1 — `fastembed` drags in `ort` unconditionally.** `ort = { version = "=2.0.0-rc.13", ... }`
is a **non-optional** dependency of `fastembed`; only `qwen3` is feature-gated. Even with
`default-features = false, features = ["qwen3"]`, cargo still compiles `ort` (ONNX Runtime) —
exactly the C++ sidecar SPEC decision #4 exists to avoid.
→ **Decision: do NOT depend on `fastembed` in the shipping binary.** Implement the Qwen3
encoder directly on `candle-transformers::models::qwen3` + `tokenizers` + `safetensors` +
`hf-hub`. Keep `fastembed` **only as a dev-dependency oracle** to cross-check vectors.

**F2 — `fastembed`'s Qwen3 wrapper mean-pools.** Its `Qwen3TextEmbedding` docs claim it handles
"tokenization, batching, **mean-pooling**, and L2-normalization", but Qwen's model card
specifies **last-token** pooling. The wrapper would silently deviate.
→ **Decision: last-token pooling with left padding** (SPEC §6); validate in Phase 0.

Both findings push the same way: **own the encoder.** ~300–500 lines more code, but it is the
only path that is both hermetic *and* reference-correct.

## 2. Resolved open questions (SPEC §15)

| # | Spec question | Decision |
|---|---|---|
| 1 | Golden eval + bench | `tests/golden/queries.jsonl`; `llmphant bench [--golden]`; a gate, not a vibe |
| 2 | MCP tool schema | `rmcp` tools, explicit schemas + annotations: read-only on `search/get/backlinks/export`, destructive on `delete` |
| 3 | Rename | `rename <old> <new>` in one tx: update `memory.key`, `UPDATE memory SET body=replace(body,'[[old]]','[[new]]')`, repoint `memory_link.to_key` |
| 4 | Daemon vs not | **No daemon in v1.** REPL + MCP hold the model warm; one-shot pays one cold start. Revisit only if painful |
| 5 | Migrations | `migrations/NNNN_*.sql` via `include_str!`, applied against `user_version` inside a tx |
| 6 | Tag governance | prune orphan tags on delete; `llmphant tags` (counts), `tags prune`, `tags rename` |
| 7 | Persist `embed_text`? | **No** — recompute deterministically from (`title`,`body`); persist only `hash` |
| 8 | `--expand` semantics | N hops both directions over `memory_link`; bundle `{hits:[…], neighbors:[{hop, via, memory}]}` |
| 9 | Metal / threads | candle `metal` + `accelerate` features (Apple); `--device cpu\|metal`, `--threads N` |
| 10 | Concurrency | one writer conn + WAL + `busy_timeout=5000`; readers open their own conn |

## 3. Repo layout

Single crate (no workspace) — the binary is the product.

```
llmphant/
├── Cargo.toml
├── SPEC.md  PLAN.md  README.md
├── migrations/0001_init.sql
├── src/
│   ├── main.rs            # clap dispatch: repl | <one-shot> | mcp | bench | doctor
│   ├── lib.rs
│   ├── error.rs           # thiserror
│   ├── config.rs          # config.toml + ~/.llmphant paths, flag merge
│   ├── db/
│   │   ├── mod.rs         # open, pragmas, migrate()
│   │   └── migrate.rs
│   ├── embed/
│   │   ├── mod.rs         # `Embedder` trait, warmup, Device/DType selection
│   │   ├── qwen3.rs       # candle Qwen3 encoder
│   │   ├── tokenize.rs    # left-pad batching + attention mask
│   │   └── projection.rs  # embed_text (strip [[links]])
│   ├── store/
│   │   ├── mod.rs         # add/update/delete/get, upsert, hash
│   │   ├── slug.rs
│   │   ├── tags.rs
│   │   └── links.rs       # extract/backlinks/rename/dangling
│   ├── index/
│   │   ├── vec.rs         # vec0 write + KNN        (encapsulates sqlite-vec)
│   │   └── fts.rs         # fts5/BM25 query
│   ├── search/
│   │   ├── mod.rs         # hybrid orchestration + hydrate
│   │   └── rrf.rs
│   ├── cli/
│   │   ├── mod.rs         # one-shot subcommands
│   │   ├── repl.rs        # rustyline
│   │   └── format.rs      # tabled output + --json
│   ├── mcp/mod.rs         # rmcp stdio server + tools
│   └── portable/{export.rs, import.rs}
├── tests/
│   ├── db.rs  search.rs  portable.rs  links.rs
│   └── golden/queries.jsonl
└── examples/              # Phase-0 spikes (throwaway)
```

## 4. Dependencies (pinned intent)

```toml
[dependencies]
rusqlite            = { version = "0.40", features = ["bundled"] }
sqlite-vec          = "=0.1.10-alpha.4"        # alpha: pin exact
candle-core         = { version = "0.11", features = ["metal", "accelerate"] } # no-ops off Apple
candle-nn           = "0.11"
candle-transformers = "0.11"
tokenizers          = "0.23"
safetensors         = "0.8"
hf-hub              = { version = "0.5", features = ["ureq"] }
rmcp                = "3.5"
rustyline           = "18"
tabled              = "0.22"
clap                = { version = "4.6", features = ["derive"] }
serde               = { version = "1", features = ["derive"] }
serde_json          = "1"
toml                = "1"
thiserror           = "2"
sha2                = "0.10"
regex               = "1"
tracing             = "0.1"
tracing-subscriber  = "0.3"

[dev-dependencies]
fastembed           = { version = "7", default-features = false, features = ["qwen3"] } # oracle only
tempfile            = "3"
```

Alignment: `candle-{core,nn,transformers}` all 0.11.x; `tokenizers` 0.23.x (candle ecosystem,
avoid the 1.0-rc). `metal`/`accelerate` are no-ops on non-Apple targets.

## 5. Phase 0 — spikes (de-risk the two unknowns)

Throwaway code in `examples/`. **Gate: do not start Phase 1 until S0.1 and S0.2 pass.**

- **S0.1 — sqlite-vec in-process.** Register `sqlite3_vec_init` via `sqlite3_auto_extension`
  on a fresh `rusqlite::Connection`; `CREATE VIRTUAL TABLE t USING vec0(...)`; insert three
  vectors; KNN returns the nearest first. **Accept:** runs with no `.dylib`, no `load_extension`.
- **S0.2 — Qwen3 embedder.** Load `Qwen/Qwen3-Embedding-0.6B` via candle (`qwen3`), tokenize
  with **left** padding, **last-token** pool, L2-normalise. **Accept:** dim == 1024; self-cosine
  ~= 1.0; deterministic across runs; vectors within tolerance of the oracle (record the number).
- **S0.3 — timing.** Cold load + per-embed (short and ~200-token text), cpu vs metal. Replaces
  the SPEC §13 estimates with real numbers.
- **S0.4 — pooling sanity.** Last-token vs mean on a small query set; confirm the choice (or
  document the delta) and lock it.

**Why first:** DB/vector integration and model integration are the two things that can
invalidate the architecture. Everything after is ordinary application code.

## 6. Phases 1–10

Each phase: goal · tasks · acceptance · commit boundary.

### P1 — Skeleton & bootstrap
- **Goal:** the binary runs and creates a store.
- **Tasks:** crate; clap scaffold; `config.rs` (paths, `config.toml` load/merge, flags override);
  `error.rs`; `tracing`; db open with pragmas (WAL, FK, `busy_timeout`); `migrate()` runner.
- **Accept:** `llmphant --version`; `llmphant doctor` prints paths, db version, model-cache
  status; `~/.llmphant/` created; `user_version` = 1.
- **Commit:** `feat: crate skeleton, config, db bootstrap`

### P2 — Migration 0001
- **Goal:** full schema.
- **Tasks:** `migrations/0001_init.sql` exactly per SPEC §7 (tables, indexes, the three FTS
  triggers, `vec0`, links, meta); embed via `include_str!`; seed `meta` (model, dim, instruction).
- **Accept:** `tests/db.rs` — schema introspects; insert/update/delete on `memory` keep
  `memory_fts` in sync (assert via MATCH); FK cascade removes tags/links on delete.
- **Commit:** `feat: schema v1 + fts triggers`

### P3 — Embedder (production)
- **Goal:** a working `Embedder` over the P0 spike code.
- **Tasks:** `qwen3.rs` (load from `--model-dir` | `hf-hub` into `~/.llmphant/models/`; DType
  bf16/f16; `--offline`); `tokenize.rs` (batch, left-pad, attention mask); last-token pool + L2;
  `projection.rs` (strip `[[…]]`, join title+body); warmup on startup; `llmphant embed "…"`
  debug command + timing.
- **Accept:** `llmphant embed` prints dim + ms; loads from cache with the network off; unit test
  that the projection strips links.
- **Design note:** `Embedder` is a **trait** — store/search tests inject a deterministic fake
  (hashed bag-of-words), so the DB layers are testable without loading 1.2 GB.
- **Commit:** `feat: qwen3 embedder (candle, last-token pool)`

### P4 — Write path
- **Goal:** `add`/`update`/`delete` are correct and atomic.
- **Tasks:** `store::add` upsert-by-key (SPEC §7 SQL); slug gen + `-2` collision; tags
  upsert+link; `[[key]]` extraction -> `memory_link`; `hash = sha256(model|dim|projection)`;
  re-embed only when `hash` changes; vec write inside the same tx; `delete` cascades + vec delete.
- **Accept:** integration test — add -> row+fts+vec+tags+links exist; re-add identical -> no
  re-embed (assert vec unchanged); re-add changed -> re-embed; delete -> all gone.
- **Commit:** `feat: write path (upsert, tags, links, vec)`

### P5 — Read path
- **Goal:** hybrid search.
- **Tasks:** `index::fts` (BM25, scope/tag prefilter); `index::vec` (KNN, k); `search::rrf`
  (union by id, RRF k=60); hydrate `{id,key,scope,type,title,body,tags,score,distance}`;
  `mode=hybrid|vec|fts`; `--max-distance`; `--expand N`.
- **Accept:** `tests/search.rs` — a paraphrase query returns the expected key in top-3 (vector
  leg); an exact-identifier query hits via BM25; scope filter excludes others; RRF order is
  unit-tested and stable.
- **Commit:** `feat: hybrid search (bm25 + knn + rrf)`

### P6 — CLI
- **Goal:** REPL + one-shot.
- **Tasks:** `repl.rs` (rustyline; history at `~/.llmphant/history`; tab-complete commands+keys);
  commands `add search show rm tags link reembed export import help quit`; `format.rs` (tabled
  tables + `--json`); one-shot subcommands mirroring the REPL.
- **Accept:** interactive add->search->show->rm round-trips; `llmphant search "…" --json` emits
  valid JSON; history persists across sessions.
- **Commit:** `feat: repl + one-shot cli`

### P7 — MCP server
- **Goal:** agents can use it.
- **Tasks:** `mcp/mod.rs` on `rmcp` stdio; tools `memory_add/update/delete/search/get/
  backlinks/export/import` with schemas + annotations; **tool descriptions carry the SPEC §9
  write policy** (one fact/row, provenance in `source`, upsert-by-key, never store derivable
  state) — this is where we tune the LLM.
- **Accept:** connects from Cline and TypingMind; a `memory_search` and a `memory_add` round-trip;
  annotations present.
- **Commit:** `feat: mcp stdio server`

### P8 — Portable
- **Goal:** export/import.
- **Tasks:** `export.rs` JSONL (canonical fields, no ids/vectors) + optional Markdown bundle;
  `import.rs` upsert-by-key, re-embed on hash change, `--dry-run`.
- **Accept:** `tests/portable.rs` — export -> new db -> import -> identical `memory` content
  (minus ids); a second import is a no-op (idempotent).
- **Commit:** `feat: jsonl export/import`

### P9 — Maintenance
- **Goal:** rebuildability.
- **Tasks:** `reembed` (walk rows, recompute, rebuild `memory_vec`; handles model/dim change);
  `rename` (SPEC §15 #3); dangling-link audit; `tags prune`; deep `doctor`.
- **Accept:** rename updates `[[…]]` tokens in bodies **and** repoints edges; `reembed` with a
  different dim recreates `memory_vec` and updates `meta`.
- **Commit:** `feat: reembed, rename, link audit`

### P10 — Bench & tuning
- **Goal:** turn estimates into numbers and tune.
- **Tasks:** `bench` (embed latency, load time, search latency at N rows);
  `tests/golden/queries.jsonl` (>=20 real questions -> expected keys); `bench --golden` reports
  recall@k / MRR; sweep `k`, RRF weights, projection.
- **Accept:** bench runs; golden recall@3 above an agreed threshold; chosen defaults written back
  into SPEC/config.
- **Commit:** `feat: bench + golden eval`

## 7. Testing strategy

- **Unit:** projection (link stripping), slug + collision, link regex, RRF merge, hash stability.
- **Integration (`tests/`):** temp-file SQLite + full schema + vec auto-extension. Covers db,
  search, portable, links. This is the real safety net.
- **Fake embedder:** store/search/link tests inject the deterministic fake via the `Embedder`
  trait — no model, fast, no network.
- **Oracle (optional, `#[ignore]`):** compare our vectors against `fastembed` within tolerance.
- **Golden eval:** regression gate for retrieval quality.
- **Constraint:** `cargo test` passes **offline**; only `#[ignore]` tests touch the real model.

## 8. Definition of done (v1)

- [ ] `cargo build --release` -> one binary, no dylibs, no runtime network requirement.
- [ ] `llmphant doctor` green; `~/.llmphant/memory.db` at schema v1.
- [ ] add / search / update / delete round-trip; hybrid returns **scored snippets**.
- [ ] wiki links + backlinks + `--expand` work.
- [ ] REPL + one-shot + `mcp` all functional; MCP consumed by **Cline and TypingMind**.
- [ ] JSONL export/import round-trips; `reembed` + `rename` work.
- [ ] `bench` prints real numbers; golden eval passes.
- [ ] `cargo test` green offline.

## 9. Risks & mitigations

| Risk | Mitigation |
|---|---|
| `sqlite-vec` is 0.1.10-**alpha** | pin exact; S0.1 validates; encapsulate in `index::vec` so it can be swapped |
| Qwen3 pooling/padding mismatch | S0.4 + oracle test; last-token per the model card |
| candle Qwen3 forward quirks (RoPE, sliding window) | S0.2 vs oracle; fall back to the `qwen2` arch if needed |
| Model download UX (1.2 GB) | progress bar + `--model-dir` + `--offline`; document it |
| bf16 slow on CPU | `--precision f16`; S0.3 measures |
| Embedder makes tests heavy | `Embedder` trait + fake; real-model tests `#[ignore]` |
| `fastembed` temptation | dev-only dependency; see F1 |

## 10. Critical path & first PR

**Critical path:** P0 (S0.1, S0.2) -> P1 -> P2 -> P3 -> P4 -> P5 -> P6 -> P7.
P8 / P9 / P10 parallelise after P5.

**Suggested first PR:** Phase 0 spikes + P1 skeleton — it proves the hard part (a single
binary that links `sqlite-vec` **and** candle and loads the model). Then P2–P5 is the
"it actually stores and finds things" milestone, which is the first genuinely useful build.

**Session kickoff order:** S0.1 -> S0.2 -> (S0.3/S0.4) -> P1 -> P2 -> P3 -> P4 -> P5.
