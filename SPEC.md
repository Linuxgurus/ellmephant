# llmphant — Design Spec (decision record)

> **Status:** decision record, v1 implemented. This captures *what we decided and why*; the
> code is the authority for behaviour, and every phase of `PLAN.md` (P1–P10) has shipped.
> §15 lists the questions that were open when this was written, each now resolved.
>
> **Name:** the crate/binary is **`llmphant`** (LLM + elephant — "never forgets"). The crate
> name is **free on crates.io** (`elephant` was taken). Personal store root: `~/.llmphant`.

## 0. One-liner

llmphant is a **single-binary, SQLite-backed long-term memory for AI agents**: the
embedder engine is compiled into the binary, one SQLite file is the sole source of truth,
and a *rebuildable* vector + full-text + link index sits on top of it. It speaks MCP (so
any client — Cline, TypingMind — shares the same memory) and ships an interactive REPL for
the human. The name is LLM + elephant; the animal is the aspiration, the software is the
attempt.

## 1. Why build it

- Agents forget expensively. Retrieval today is `grep`-plus-guessing over markdown files;
  cost is dominated by *hunting* and *reading whole files*, not by the content itself.
- Memory should be **shared across tools**, not trapped per-client.
- The data should be **owned**: one file on disk, no cloud, no interpreter, no service.

## 2. Goals / Non-goals

**Goals**

- Hermetic Rust binary: no Python/Node runtime, no external service, no cloud.
- SQLite is the single source of truth (one file, `~/.llmphant/memory.db`).
- Embedder *engine* in-binary; *weights* external (disk or HuggingFace).
- Retrieval as a single **phrase** matched semantically (KNN) or by regex over `title`/`body` —
  `key`/`tags` are identity and lookup, not search.
- Wiki-style links with backlinks and one-hop graph expansion.
- Multi-client via MCP; interactive REPL for humans.

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
| 7 | Weights **external**: our own flat cache under `~/.llmphant/models/`, or `--model-dir`; `--offline`; sha-pinned revision (§6.1–6.5) | Keeps the binary small; weights are data. We own the layout (not `HF_HOME`) so it is inspectable, `gc`-able, and integrity-checked |
| 8 | **cosine**, L2-normalised both sides; **last-token pooling + left-pad** | Qwen3 is a decoder embedder — mean pooling (the candle BERT example) gives silently worse vectors |
| 9 | Root `~/.llmphant`; DB `memory.db`; `config.toml`; models in `models/` | Single dotdir, matches tooling conventions |
| 10 | **Tags** name aspects of a subject (e.g. `storage`, `gotchas`); no `scope`, no `type` | Tags are a facet and part of the embedded document, so a tag token is retrievable; a bucket/type taxonomy adds vocabulary without adding recall |
| 11 | `key` = **global-unique slug**, optional to supply | Identity (upsert target) and link target; auto-slugged from the title when omitted |
| 12 | **Smart-caveman** writing; **one** document embedding over `key` + tags + `title` + `body` | Terse text embeds sharply, and one vector per row is simpler to build and rebuild; terseness keeps the body from swamping the title |

## 5. Architecture

- **Language:** Rust (edition 2024+).
- **Crates:** `rusqlite` (feature `bundled`), `sqlite-vec` (auto-extension), `candle-core`
  + `candle-transformers` (encoder plumbing), `tokenizers` 0.22, `safetensors`,
  `ureq` (weights fetch, rustls), `rustyline` (REPL), `tabled`/`comfy-table` (output), `clap`,
  `serde`/`serde_json`, `toml` + `serde` config, `sha2`, an MCP server (`rmcp`, HTTP default).
  (No `hf-hub` — see §6.2.)
- **Process model:** one **long-lived** process (REPL or `mcp`) holds the model warm
  (~1.2 GB resident) and one writer connection. One-shot CLI flags pay one cold start.
  A shared daemon is *optional* and only if bare one-shot latency annoys us (see §15).
- **Indexes are derived:** the document vectors (`memory_vec`) and links (`memory_link`) are all
  rebuildable from `memory`. Only `memory` is truth — no FTS index, no tag tables.

## 6. Embedder

- **Model:** `Qwen/Qwen3-Embedding-0.6B`, output **1024** (MRL allows 32–1024; frozen at
  `CREATE`, choose 1024 — rebuildable later).
- **Engine:** candle, pure Rust. **Weights:** our own flat cache under `~/.llmphant/models/`
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
~/.llmphant/models/<owner>--<repo>/
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
- `cache_dir` overrides the root (default `~/.llmphant/models`). We do **not** reuse
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
  on truncation/corruption. `llmphant doctor --verify` does the full hash on demand.
- A cache that fails to load names the file and prints the fix: `llmphant model fetch --force`.

### 6.4 Offline & failure modes

- `--offline` / `LLMPHANT_OFFLINE=1` / `offline = true` (§12): **never** opens a socket; a
  missing or incomplete cache is a hard error naming the file and the online command.
- `--model-dir` is always offline. Online (the default) uses the cache when complete, else
  fetches once.
- Disk-full mid-fetch → `.partial` discarded, no manifest, retried next run.
- Revision is pinned by the recorded sha; moving `main` upstream is a no-op until `--force`.
- Runtime network is **never required** once cached, so the §8 DoD holds.

### 6.5 Surface

- `llmphant model status` — repo, resolved revision, cache path, per-file presence/size, total
  bytes, manifest ok/absent. This is the "model-cache status" `doctor` prints from P1 on.
- `llmphant model fetch [--repo][--revision][--force]` — prefetch / repair.
- `llmphant model rm [--revision]` — drop a revision to reclaim space.
- `doctor` prints the same one-line summary.

## 7. Data model

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
  to_key  TEXT    NOT NULL,                     -- target KEY, not id
  rel     TEXT    NOT NULL DEFAULT 'see',        -- see|supersedes|part-of|contradicts
  PRIMARY KEY (from_id, to_key, rel)
) WITHOUT ROWID;
CREATE INDEX idx_link_to ON memory_link(to_key);

CREATE TABLE meta (k TEXT PRIMARY KEY, v TEXT NOT NULL);
-- model, dim, instruction, schema_version
```

Notes: `memory_vec` is **app-written** (SQLite can't embed). FTS is external-content
(no text copy). Tags normalised for join-based filtering and rename.

## 8. Cross-links (wiki semantics)

- **Author:** inline `[[key]]` in `body` (canonical, greppable, exportable).
- **Index:** parse to `memory_link(from_id, to_key, rel)` at write time — a *derived* index.
- **By key, never by id:** survives a `reembed`; supports forward refs; dangling links are
  detected (non-fatal) rather than broken FKs:
  `SELECT DISTINCT to_key FROM memory_link WHERE to_key NOT IN (SELECT key FROM memory);`
- **Backlinks:** `SELECT from_id FROM memory_link WHERE to_key = ?` → powers one-hop graph
  expansion in search and the "derived profile" assembly of §9.
- **Rename:** keys are near-immutable; `rename` deliberately rewrites `[[oldkey]]` across
  bodies **and** repoints edges.
- **Embedding:** `[[...]]` stripped from the embed projection.
- Key charset `[a-z0-9-]+` so the link regex is `\[\[([a-z0-9-]+)\]\]`.

## 9. Memory philosophy (how memories are "tuned")

The store is **atomics-first**; generalization is derived, not stored.

- **Atomic memory = the storage *and* embedding unit.** One fact/decision/gotcha/signpost,
  terse, self-contained. Focused text embeds sharply; edits are surgical; no staleness cascade.
- **General views are computed at query time** (fan-out / one-hop expansion), never written.
  Store the atoms, synthesize the whole.
- **A sparse pointer tier** (`*-overview` notes) is allowed for routing/navigation only —
  pointers, not content; governed by "one home per fact, cross-link don't duplicate."
- **Write policy** (guided via tool descriptions + a system instruction):
  **smart-caveman** — drop filler, articles and glue words, keep high-signal words ·
  one fact per row · **tags** name aspects of the subject · **provenance** in `source`
  (store the *derivation command*, not live state) · upsert-by-`key` · trim-on-edit
  (never grow) · never store cheaply-derivable state.
- **Read policy:** return **titles, tags and scores**, never dumps (tokens regress otherwise);
  the body is a deliberate `memory_get` away. Recall runs **narrow-to-wide**: a semantic phrase
  at a small `k`, judged from titles/tags/distances, `memory_get` only the hit(s) you'll use,
  widening (`k`) only if none fit.

## 10. Search

A search is a single **semantic phrase**. There are no key/tag constraints and no literal
(regex) leg — `key` and `tags` are identity and lookup, and both are folded into the embedded
document, so a phrase reaches them semantically.

- The phrase is embedded (query-side instruction) and the KNN of the whole store is taken,
  within a cosine-distance ceiling. One vector per row (§6).
- **Params:** default `k=5`; `max_distance` optional, falling back to the configured
  `default_max_distance` (`0.6`; `null` disables the cap). With no phrase, results are ordered
  by `key` and uncapped. The KNN is taken over the whole store and `k` truncates the ranked
  result, so **a result shorter than `k` means the ceiling, not `k`, cut it** — widen by
  loosening `max_distance`, else by raising `k`.
- **Returns:** `{key, title, tags, distance}` — **never the body** (fetch it with
  `memory_get`/`show`). Over MCP this is emitted as **positional JSON**: a top-level `cols`
  header names the fields and each row is a bare array (§11); `--json`/core keep the keyed form.
- **`--expand N`:** pull N-hop link neighbourhoods into the result bundle (derived profiles);
  neighbours keep their (snipped) bodies.

```sql
-- Semantic phrase: KNN over the document vectors
SELECT memory_id, distance FROM memory_vec WHERE embedding MATCH ?1 AND k = ?2 ORDER BY distance;
```

## 11. Interfaces

**MCP tools** (served over Streamable HTTP by default, `--stdio` optional): `memory_add`, `memory_update`,
`memory_delete`, `memory_search`,
`memory_get`, `memory_backlinks`, `memory_list`.

- The server also advertises the §9 write policy as its **instructions**, and each write tool
  carries it in its description — that is what the model actually reads.
- `memory_search` takes a semantic `phrase` and the ranking knobs `k`, `max_distance` and
  `expand`; there are **no key/tag filters and no regex leg** — `key`/`tags` are not search
  constraints, and `memory_get` reads a known key.
- The list-shaped reads return **positional JSON** to keep the LLM-facing payload small: a
  top-level `cols` header names the fields and the rows are bare arrays, **one row per line**
  (whitespace is insignificant to a parser, so a raw result is scannable, not one blob). The
  `cols` header is self-describing (the model can't mis-map columns) while field names aren't
  repeated per row; distances are rounded to 4 decimals. `memory_search` →
  `{"cols":["key","title","tags","distance"],"hits":[[…],…]}`; link `neighbors` (with `expand>0`)
  carry their own `cols`/`rows` table. `memory_list` →
  `{"cols":["key","title","tags"],"count":N,"memories":[[…],…]}`. `memory_backlinks` →
  `{"cols":["key","title"],"backlinks":[[…],…]}`. `--json`/core keep the keyed form.
- `memory_list` returns the store index (`key`, `tags`, `title`, no bodies) — cheap browsing (§9).
- `memory_get` is the **only** read that returns a body, and the way to read a known `key`.

**CLI** — three faces over one core:

- `llmphant` → **REPL** (rustyline: history at `~/.llmphant/history`, completion;
  bordered tables for results). Commands: `add`, `search`, `show`, `rm`, `list`, `tags`,
  `rename`, `reembed`, `links`, `model`, `help`, `quit`.
- One-shot flags for scripting: `llmphant search "..." --json`.
- `llmphant list [--json]` → the memory index (`key`, `tags`, `title`, no bodies).
- `llmphant model` → weights cache: `status` | `fetch` | `rm` (§6.5).
- `llmphant mcp` → MCP server; Streamable HTTP on `port` (default 8888) at `/mcp`; `--stdio` for stdio.

**Portability:** the store is one file (`~/.llmphant/memory.db`), so moving a machine is a copy.
`reembed` rebuilds the vectors for a new model or dimension; `rename` fixes keys.

## 12. Config & paths

- `~/.llmphant/memory.db` — the store.
- `~/.llmphant/config.toml` — model, dim, precision, instruction, defaults (flags override).
- `~/.llmphant/models/` — weights cache, laid out per §6.1 (`<owner>--<repo>/<sha>/`). Not
  `HF_HOME`: we own this layout so `doctor`/`gc` and `--model-dir` stay simple.
- `~/.llmphant/history` — REPL history.
- `meta` table mirrors model/dim/instruction for drift detection at startup.

Model config keys (all optional; flags override, and `LLMPHANT_OFFLINE=1` overrides `offline`):

```toml
model      = "Qwen/Qwen3-Embedding-0.6B"  # HF repo id, or a dir name under models/
revision   = "main"                       # resolved once to a commit sha and pinned
cache_dir  = ""                           # default ~/.llmphant/models
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
- **Fat returns regress token cost** below plain `grep` — titles-not-bodies is a *requirement*.
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
2. **MCP tool schema + annotations** — shipped (P7): 7 tools with JSON-Schema params and
   read-only / destructive / idempotent hints (`src/mcp`).
3. **Rename implementation + collision policy** — shipped (P9): one transaction rewrites
   `[[old]]` and repoints edges; refuses to clobber an existing key (`store::links::rename`).
4. **Daemon vs not** — decided: no daemon in v1. The REPL and the MCP server both keep the
   encoder warm, which is what the daemon would have bought (PLAN §2 #4).
5. **Migration mechanics** — shipped (P1): `user_version` steps in `db::migrate`; the first
   run bootstraps schema v1.
6. **Tags** — decided: stored on each row (comma-joined) and folded into the document embedding;
   `llmphant tags` derives the vocabulary and counts. Dangling `[[key]]` targets are audited by
   `links`.
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
7. CLI REPL + one-shot flags.
8. MCP server (HTTP default, stdio option).
9. `reembed`, dangling-link audit, `rename`.
10. `bench` + golden queries; tune.

## 17. Glossary

- **Atomic / atom** — one memory row; a single fact. The storage + embedding unit.
- **Derived view / profile** — a composed answer built at query time from atoms; never stored.
- **Key** — the global-unique slug; identity *and* link target (wiki semantics).
- **Title / body** — the one-liner and the fact; stored literally and folded into the document embedding.
- **Tag** — an aspect of the subject (`storage`, `gotchas`); stored on the row and folded into the embedding.
- **Smart-caveman** — the writing style: drop filler, articles and glue words; keep signal.
- **Document** — the text embedded for a row: `key`, tags, title and the link-stripped body.
- **store ≠ embed** — the row keeps full text; the vector is over a link-stripped projection.
- **Semantic** — the phrase-match method: embed the query and take the KNN (the only leg).
- **Model cache** — the external weights dir (`~/.llmphant/models/<owner>--<repo>/<sha>/`), a
  flat, sha-pinned, manifest-backed copy of the encoder's files; data, not code (§6.1).
