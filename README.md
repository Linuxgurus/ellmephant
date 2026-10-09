# llmphant

**Long-term memory for AI agents. One binary, one SQLite file, no cloud, no nonsense.**

Your AI assistant has the short-term memory of a goldfish that has recently sustained a mild
concussion. Every session it meets you for the first time. Every "where do we keep the deploy
keys?" sends you spelunking through a folder of Markdown you wrote two jobs ago. llmphant is
the elephant in the corner that remembers all of it:

- the [Qwen3-Embedding](https://huggingface.co/Qwen/Qwen3-Embedding-0.6B) encoder is
  **compiled into the binary** — no Python, no `pip install` roulette, no `requirements.txt`
  older than some nation-states;
- one **SQLite file** is the sole source of truth, wearing a *rebuildable* full-text + vector
  + link index like a tasteful hat;
- it speaks **[MCP](https://modelcontextprotocol.io)** over stdio, so Cline, TypingMind and
  whatever you adopt next quarter all read and write the **same** memory instead of keeping
  three private diaries about you;
- there's a **REPL** for you and a **one-shot CLI** for your scripts, because you contain
  multitudes (and at least one cron job).

It touches the network exactly once — to fetch the model weights — and after that it lives
offline like a hermit who has finally found peace.

Docs in this repo: **`SPEC.md`** (the decision record — what we decided and, more usefully,
*why*), **`PLAN.md`** (the build plan), **`LICENSE`** (MIT — do whatever you like; just don't
blame us).

---

## Why

The problem is small, boring, and expensive: **agents forget, and the tools we hand them to
remember are the wrong shape.**

- **`grep` over Markdown does not scale.** It finds strings, not meaning. It cannot connect
  "where do the logs live" to a note that says "stdout goes to Loki". And every hit is a whole
  file, so you pay for *reading* rather than for the one fact you wanted — which is a bit like
  buying a library to look up a phone number.
- **Memory trapped inside one client is not memory; it's a hostage situation.** What you told
  Cline is invisible to TypingMind. The next assistant you talk to knows strictly *less* about
  you than the last one, which is an unusual direction for a relationship to travel.
- **Cloud memory owns your data; a vector database is another thing to feed and water.** Both
  add a network hop, an account, a bill, and an on-call rotation to a problem that fits
  comfortably inside a single file.

llmphant is the boring answer: **one local file, one process, one shared brain.**

## Why is it called llmphant? (and other questions you didn't ask)

**`llmphant` = LLM + elephant.** Elephants are celebrated for two things: remembering
everything, and being enormous. We only need the first one, but we respect the range.

**But I already have a notes app.** You have several, and you can't find any of them. This one
is reachable by a robot.

**Can't I just paste all my notes into the context window?** Certainly, right up until it
doesn't fit, or costs more than your rent, or both. Then you're back here, reading a README with
jokes in it.

**Is it a vector database?** It contains one. It is also a relational database, a full-text
index, a link graph, and a small pile of opinions. Calling it "a vector database" is like
calling a restaurant "a place with a bin".

**Does it phone home with my secrets?** No. The only outbound request is the one-time weights
download, and even that you can switch off with `--offline` and a borrowed cache. Your notes
live in a file you can `cp`, `rsync`, back up, or lose spectacularly yourself — the full
spectrum of ownership.

**What if I fall out of love with it in a year?** `llmphant export memory.jsonl` hands
everything back in a format that `grep` can read. No lock-in, no "export your data (please allow
3–5 business days)".

**Why does the elephant never forget but I still have to run `model fetch`?** Because the
elephant is a metaphor, and the weights are 1.2 GB of matrices. One of these two things has to
live on disk.

## Design principles

The decisions the code actually obeys (the long, self-important version is in `SPEC.md`):

- **Atoms, not essays.** One memory = one fact, terse and self-contained. Focused text embeds
  sharply and edits surgically. Grand summaries rot; a fact from 2019 is still a fact. If you
  want a sweeping "derived view" of the world, compute it at query time and forget it
  immediately, like a normal person.
- **`store ≠ embed`.** The row keeps the full, verifiable text; the vector is computed over a
  condensed projection (title + body, wiki links stripped). You may change how things are
  embedded without losing a byte of what you actually wrote — a luxury most memory systems
  discover they wanted only after they've lost it.
- **Everything but `memory` is derived.** The FTS index, the vector index and the link graph
  are rebuildable from the rows (`llmphant reembed`). The relational store is the one thing
  that must never lie.
- **Local-first, offline-capable.** `--offline` opens no socket. Weights are *data* you fetch
  once and then guard jealously, not code welded into the binary.
- **Snippets + scores, never dumps.** Search returns bounded snippets with their fused score
  and cosine distance, so the caller can judge confidence and your token budget survives the
  encounter.
- **One binary, no runtime.** No interpreter. No service. No `.so` with an attitude. Copy it to
  a server and it simply stands there, remembering.

## Architecture (40,000 feet — roughly the altitude where peanuts live)

```
   clients                                    you
   Cline · TypingMind · any MCP client        REPL · one-shot CLI
              │                                        │
              │  MCP over stdio                        │  one shared core
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

Read it bottom-up: the **encoder** turns text into vectors; **SQLite** holds the rows and the
three derived indexes; **search** runs the lexical and vector legs and fuses them by rank;
**`Session`** is the one shared core; **the CLI and MCP** are two faces bolted onto it.
Everything above `memory.db` can be deleted and rebuilt, so when you inevitably poke something
with a stick, the worst case is a `reembed` and an apology.

Five layers, one crate:

| Layer | Module | Responsibility |
|---|---|---|
| Storage | `db`, `migrations/` | open SQLite, pragmas, schema + FTS triggers, `meta` kv |
| Write path | `store`, `embed`, `index/vec` | upsert a row: key, hash, tags, links, vector |
| Read path | `search`, `index/fts`, `search/rrf` | hybrid BM25 ∪ KNN, RRF fusion, snippets |
| Interfaces | `cli`, `mcp`, `portable` | REPL + one-shot, MCP stdio server, JSONL in/out |
| Tooling | `model`, `bench` | weights cache, latency + golden-eval measurements |

---

## Installation

### Prerequisites

- **Rust 1.88+** (edition 2024) with `cargo` — grab it from [rustup](https://rustup.rs).
- **A C compiler / linker.** SQLite and `sqlite-vec` are compiled from source, so you need
  `cc`, `clang` or `gcc` (Debian/Ubuntu: `apt install build-essential`). Yes, even the
  pure-Rust dream needs a C compiler, because SQLite is not written in Rust and we are guests
  in its house.
- **~3 GB of disk** to build, plus **~1.2 GB** for the model weights (fetched once, then
  hoarded).
- **macOS or Linux.** Apple Silicon can use `--device metal`; everything else runs on the CPU
  with dignity. Windows is untested — WSL2 should behave.

### Console (build from source)

```sh
git clone <this-repo> && cd llmphant
cargo build --release --locked        # or: make release
./target/release/llmphant --version
```

The first build is long: the SQLite/`sqlite-vec` C, the entire candle ML stack, and roughly
four hundred crates all compile at once. This is normal. Go and make tea; the second build is
incremental and quick, because the compiler remembers even if your agent does not.

Put the binary on your `PATH` — via cargo, or with the time-honoured shrug of `install`:

```sh
cargo install --path .                # -> ~/.cargo/bin/llmphant
# or, without cargo:
install -m 0755 target/release/llmphant ~/.local/bin/llmphant
```

**Store-only build** (no ML stack, no `candle`): for a machine that only reads a store or runs
JSONL export. `--mode fts` search still works; the encoder becomes a polite stub that errors
at runtime when asked to do the impossible, and the KNN leg is simply unavailable.

```sh
cargo build --release --locked --no-default-features
```


### Create the store

Every command honours `LLMPHANT_HOME`; without it the store is `~/.llmphant`.

```sh
llmphant doctor            # creates ~/.llmphant/ and the schema; prints health
llmphant model fetch       # one-time ~1.2 GB download into ~/.llmphant/models/
llmphant doctor --verify   # optional: re-hash every weight file, for the suspicious
```

After that, runs need no network. `llmphant --offline <cmd>` never opens a socket, which is
exactly the kind of antisocial behaviour you want from a program holding your personal notes.

### Docker

The image is a multi-stage build: ~400 crates compile in `rust:1-bookworm`, and the runtime is
`debian:bookworm-slim` plus one binary that takes up almost no space and asks for almost
nothing. The store lives on a volume at `/data` (`LLMPHANT_HOME=/data`).

```sh
make docker-build                             # docker build -t llmphant:dev .
# or:
docker build -t llmphant:dev .

# one-off commands (the image's default CMD is `doctor`, its resting state)
docker run --rm -v llmphant-data:/data llmphant:dev doctor
docker run --rm -v llmphant-data:/data llmphant:dev model fetch
docker run --rm -v llmphant-data:/data llmphant:dev search "internal gateway hostname"
docker run --rm -it -v llmphant-data:/data llmphant:dev      # REPL
```

**Weights are data, not code.** Do not bake ~1.2 GB into the image, where it will sit in every
registry layer until the heat death of the container ecosystem. Fetch them into the volume (as
above), or mount a warm cache read-only:

```sh
docker run --rm -it -v llmphant-data:/data \
                    -v ~/.llmphant/models:/data/models:ro \
                    llmphant:dev search "where do the logs live"
```

**MCP from Docker.** MCP is stdio, so keep stdin attached and don't allocate a TTY — that last
`-t` is the classic way to have a perfectly good protocol fail to speak.

```sh
docker run --rm -i -v llmphant-data:/data llmphant:dev mcp
```

A client can launch it directly — the whole incantation *is* the server:

```jsonc
{
  "mcpServers": {
    "llmphant": {
      "command": "docker",
      "args": ["run", "--rm", "-i", "-v", "llmphant-data:/data", "llmphant:dev", "mcp"]
    }
  }
}
```

`docker-compose.yml`, for when you want the same three lines in YAML:

```yaml
services:
  llmphant:
    build: .
    image: llmphant:dev
    volumes:
      - llmphant-data:/data
    # MCP server: keep stdin open
    stdin_open: true
    command: ["mcp"]

volumes:
  llmphant-data:
```

---

## Quick start

```sh
# 1. bootstrap and warm the weights cache (first run downloads ~1.2 GB)
llmphant doctor
llmphant model fetch

# 2. write a memory (upsert by key; the key is generated from scope + title if omitted)
llmphant add --scope infra \
  --title "Gateway hostname" \
  --body "the gateway answers at portainer-notes-gw.internal on 8443" \
  --tag network

# 3. find it by meaning or by exact token — no more folder archaeology
llmphant search "which host is the internal gateway"     # hybrid BM25 ∪ KNN
llmphant search "portainer-notes-gw"                     # exact identifier
llmphant search "gateway" --scope infra --json

# 4. or just open the REPL and browse like a person with time on their hands
llmphant
```


---

## Command reference

Every command works one-shot for scripting and is mirrored in the REPL. Global flags —
`--home DIR`, `--offline`, `--model-dir DIR`, `--device cpu|metal`, `--precision bf16|f16` —
may appear before any subcommand. `--json` is the stable, machine-readable face of the
commands that have one, for when you want output that doesn't require eyes.

**Memory**

```sh
llmphant add    --scope S --title T --body B [--key K] [--type signpost|dictate|fact|profile]
                [--source SRC] [--tag T]...        # --body - reads stdin; upsert by key
llmphant search "QUERY" [--scope S] [--k N] [--mode hybrid|vec|fts]
                [--max-distance D] [--expand N] [--json]
llmphant show   KEY [--json]
llmphant rm     KEY
llmphant tags   [--json] | tags prune [--dry-run]
```

`add` upserts by key: run it twice with the same key and you get one memory, not two. This is
either "idempotent" or "the only thing here that is well-behaved", depending on your mood.

**Maintenance** (rows are the source of truth; these rebuild the derived indexes)

```sh
llmphant reembed                 # recompute every vector; survives a model/dim change
llmphant rename OLD NEW          # rewrite [[OLD]] in bodies + repoint link edges
llmphant links [--json]          # audit the graph for dangling [[key]] targets
llmphant doctor [--verify]       # paths, schema, index health; --verify re-hashes weights
```

**Portability**

```sh
llmphant export FILE.jsonl              # canonical JSONL; no model needed
llmphant import FILE.jsonl [--dry-run]  # upsert by key; re-embed only on hash change
```

**Model, embeddings, benchmark, interfaces**

```sh
llmphant model status [--verify] [--repo R] [--revision REV]
llmphant model fetch  [--force]  [--repo R] [--revision REV]
llmphant model rm     [--all]    [--repo R] [--revision REV]
llmphant embed "TEXT" [--query]          # debug: dim + load/embed latency + norm
llmphant bench [--golden] [--golden-file F] [--sweep] [--k N]
llmphant mcp                             # MCP stdio server
llmphant                                 # REPL
```

## Configuration

`~/.llmphant/config.toml` is optional. Flags override it, and it overrides the built-in
defaults — a hierarchy of authority that has never once caused confusion, and if you believed
that sentence there's a bridge in Brooklyn you might like. Copy `config.example.toml` to begin:

```toml
model       = "Qwen/Qwen3-Embedding-0.6B"   # HF repo id, or a dir name under models/
revision    = "main"                         # branch/tag (resolved once to a sha) or a sha
cache_dir   = ""                             # weights root; empty = ~/.llmphant/models
offline     = false                          # true = never open a socket
dim         = 1024                           # frozen at CREATE; MRL allows 32..=1024
precision   = "bf16"                         # bf16 | f16 (CPU is always f32; see below)
device      = "cpu"                          # cpu | metal
instruction = "Given a question, retrieve relevant memories"   # query side only
default_k   = 10                             # search result count
```

A bad `device`/`precision` or `dim = 0` is rejected up front, before any model work — so you
get one clear error instead of a stack trace that mentions tensor shapes and ends in a shrug.


## Model weights

Weights are **data**, fetched on first use into `~/.llmphant/models/`, or supplied with
`--model-dir`. The cache is flat and sha-pinned (`SPEC.md` §6): once warm, runs need no
network, and a corrupt file is caught rather than fed into your vectors as confident nonsense.

```sh
llmphant model status              # what is cached (never touches the network)
llmphant model status --verify     # re-hash every file (slow; for the paranoid, and the burned)
llmphant model fetch               # download / repair; idempotent
llmphant model rm --all            # reclaim space
llmphant --offline doctor          # opens no socket
```

`--model-dir DIR` uses an existing flat directory of weights and implies `--offline`;
`LLMPHANT_OFFLINE=1` forces offline globally. **Fetching is the only network code in the
binary** — a boast we mention often, because everything else in this directory of software is
quietly phoning home, and we would like a little credit.

## Embeddings

The encoder is Qwen3-Embedding-0.6B on candle — 1024 dimensions, **last-token** pooling with
left padding, L2-normalised. Queries get the `Instruct: <task>\nQuery: <q>` prefix; documents
go in bare, because asking a document a question is how you get weird vectors.

Two realities worth knowing: the Hub names tensors without candle's `model.` prefix (bridged
in `embed::qwen3`), and candle's CPU matmul runs through Accelerate, which is **f32-only** — so
CPU always runs f32 and `--precision` only affects `--device metal`. A helpful constraint
delivered with the usual amount of warning.

```sh
llmphant embed "where do the logs live" --query   # dim + load/embed latency + norm
```

`[[wiki links]]` are stripped from the projection, so a rename never invalidates a stored
vector. The link is for navigation; the meaning is in the words.

## Search

`hybrid` (the default) runs both legs and fuses them with **RRF (k=60)**; `--mode vec|fts`
runs one. `--mode fts` needs no weights and no network. `--expand N` appends the N-hop link
neighbourhood; `--max-distance` caps the KNN leg (cosine); `--scope` prefilters both legs.

```sh
llmphant search "where do the logs live"            # hybrid, k=10
llmphant search "portainer-notes-gw" --mode fts     # exact identifier, no model
llmphant search "gateway" --scope infra --json
llmphant search "backups" --expand 1                # + linked neighbours
```

BM25 is not optional: it carries the exact tokens — hostnames, IDs, paths — that embeddings
cheerfully fumble into the sea of "semantically similar". Every hit carries its fused `score`
(and `distance` when the KNN leg participated), so the caller can tell a confident answer from
a plausible hallucination with good posture. The read policy is **snippets + scores, never
dumps**.

## Wiki links

Author links inline in the body as `[[key]]` (canonical, greppable, exportable — pick your
favourite of those three). At write time they become a derived `memory_link` edge; searching
finds forward links and backlinks, and `--expand` walks them. Links are **by key, never by
id**, so they survive export/import and forward references; a `[[key]]` with no target is
reported by `llmphant links` as a warning rather than an error, which is the correct attitude
towards a link you haven't written yet. `llmphant rename OLD NEW` rewrites the tokens *and*
repoints the edges, so you get to change your mind about naming things — the fundamental
human right.

## Portability

Export/import is JSONL of canonical fields
(`key,scope,type,title,body,source,tags,created,updated`) — no ids, no vectors — so a dump
survives a different machine, model revision or dimension. Vectors are the one thing you can
always recompute; your actual words are the thing you cannot.

```sh
llmphant export memory.jsonl            # no model needed; atomic (temp + rename)
llmphant import memory.jsonl --dry-run  # report pending changes, write nothing
llmphant import memory.jsonl            # upsert by key; re-embed only on hash change
```

Import is all-or-nothing: a bad line fails before anything is written, so a half-imported
store never becomes an archaeological site. A second import of the same file is a no-op (it
returns `0`), which is a nice way of saying "nothing to do, and I checked." Export is ordered
by `key`, so equal content exports byte-identical — diffable, testable, and mercifully dull.


## Maintenance

The `memory` table is the source of truth; every index on top of it is rebuildable. This is the
difference between a database and a tantrum — you can always throw away the derived bits and
rebuild them.

`reembed` is the deliberate path that the frozen-dimension guard in `index::vec::ensure`
refuses: it drops and recreates `memory_vec` when the embedder's width changes, rewrites each
row's `hash`/`model`/`dim`, and records `dim`/`model`/`instruction` in `meta`. `rename` runs in
one transaction and refuses to clobber an existing key, on the sound principle that silently
destroying someone's note is not a feature.

## Bench & tuning

`bench` replaces estimates with numbers, and gates retrieval quality so that "the search feels
fine" stops being a load-bearing opinion.

```sh
llmphant bench                            # load / embed / search latency at the current row count
llmphant bench --golden                   # + recall@k & MRR over <home>/golden/queries.jsonl
llmphant bench --golden --golden-file Q   # score a specific golden set
llmphant bench --sweep                    # walk k × RRF weight; compare embed projections
```

The golden file is JSONL, one object per line — `{"query": "…", "expect": ["key", …], "k": 3}`
(blank lines and `#` comments ignored). It loads the encoder, so it needs warm weights (or
`--offline` against a populated cache). A seed corpus and ≥20 questions live in `tests/golden/`.
Grow them from real usage: synthetic queries measure how well your search matches the questions
you invented, which is a lovely hobby and tells you almost nothing.

## MCP

`llmphant mcp` serves the same store to any MCP client over stdio, so Cline, TypingMind and the
REPL all read and write one memory. Register it once per client (point `command` at the built
binary; the client launches it and keeps it on a leash):

```jsonc
{
  "mcpServers": {
    "llmphant": { "command": "/absolute/path/to/llmphant", "args": ["mcp"] }
  }
}
```

Tools: `memory_add`, `memory_update`, `memory_delete`, `memory_search`, `memory_get`,
`memory_backlinks`, `memory_export`, `memory_import`. Read tools carry a read-only annotation,
`memory_delete` is marked destructive, and the write tools' descriptions carry the write policy
(one fact per row, provenance in `source`, upsert-by-key, trim-on-edit). `memory_search` returns
scored snippets; the KNN leg loads the model lazily, so `mode=fts` runs with no weights and no
network. Requests are serialised through one store connection, and each tool runs its
synchronous SQLite/model work on a blocking thread (`tokio::task::spawn_blocking`), so it never
occupies a tokio worker — an async runtime is a precious resource and we are not the ones who
get to hog it.

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
├── mcp/               MCP stdio server
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

Tests never load weights: the store/search layers inject the deterministic `HashEmbedder`
behind the `Embedder` trait, and the real-model check is `#[ignore]`d (`tests/embed.rs`). The
golden eval (`tests/bench.rs`) imports the seed corpus with the same fake and asserts recall@3
clears the 0.8 gate. In other words, the entire suite runs offline in seconds, and you will
spend more time reading this sentence than the tests take to pass.

## Licence

MIT — see [`LICENSE`](LICENSE). Use it at your own risk. The software comes with **no warranty**
of any kind, and the authors accept no liability for any loss or damage arising from its use.
In practice, the worst thing that will happen is that your agent remembers something slightly
wrong with total confidence — which, to be fair, is also most of your colleagues.
