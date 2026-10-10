# ellmephant

**Long-term memory for AI agents: one binary, one SQLite file, local-first.**

ellmephant gives AI assistants a shared, persistent memory that runs entirely on your machine:

- the [Qwen3-Embedding](https://huggingface.co/Qwen/Qwen3-Embedding-0.6B) encoder is compiled
  into the binary; no Python or external runtime is required;
- one SQLite file is the source of truth, with rebuildable vector and link indexes;
- it speaks [MCP](https://modelcontextprotocol.io) over Streamable HTTP (or stdio), so Cline,
  TypingMind, and other MCP clients read and write the same memory;
- there is an interactive REPL and a one-shot CLI for scripting and cron jobs.

It contacts the network once, to fetch model weights, and runs offline afterwards.

## Documentation

- **Install** → [`INSTALL.md`](INSTALL.md)
- **Engineering / internals** → [`ENGINEERING.md`](ENGINEERING.md)
- **Development / releases** → [`DEVELOPMENT.md`](DEVELOPMENT.md)
- **Specification** → [`SPEC.md`](SPEC.md)
- **Design plan / history** → [`PLAN.md`](PLAN.md)

## Why

Agents forget, and the tools used to make them remember are the wrong shape:

- **`grep` over Markdown does not scale.** It matches strings, not meaning, and returns whole
  files rather than the one fact you wanted.
- **Memory trapped inside one client is not shared.** What you store in one assistant is
  invisible to the next.
- **Cloud memory and hosted vector databases add a network hop, an account, and a bill** to a
  problem that fits in a single local file.

ellmephant is the local answer: one file, one process, one shared memory.

## Quick start

```sh
ellmephant doctor            # create ~/.ellmephant/ and the schema
ellmephant model fetch       # one-time ~1.2 GB weights download
ellmephant add --title "Gateway hostname" \
  --body "gateway = portainer-notes-gw.internal :8443" --tag network
ellmephant search "which host is the internal gateway"
ellmephant                   # REPL
```

See [`INSTALL.md`](INSTALL.md) for building the binary and for Docker setup.

## Usage on a host

### CLI and REPL

Every command works one-shot and is mirrored in the REPL. Global flags — `--home DIR`,
`--offline`, `--model-dir DIR`, `--device cpu|metal`, `--precision bf16|f16` — may appear before
any subcommand. `--json` prints the machine-readable form where a command provides one.

### Command reference

**Memory**

```sh
ellmephant add    --title T --body B [--key K] [--source SRC] [--tag TAG]...
                                                   # --body - reads stdin; upsert by key
ellmephant search ["PHRASE"] [--k N] [--max-distance D] [--expand N] [--json]
ellmephant show   KEY [--json]
ellmephant rm     KEY
ellmephant list   [--json]
ellmephant tags   [--json]
ellmephant stats  [--json]      # digest: counts, size, tags, links, activity (alias summary)
```

`add` upserts by `key`; running it twice with the same key yields one memory. Write in
**smart-caveman**: drop filler, articles and glue words; keep high-signal words. One fact per
row, `--tag` the aspect of the subject (`storage`, `gotchas`), and put provenance in `--source`.

**Maintenance**

```sh
ellmephant reembed                 # recompute every vector; needed after a model/dim change
ellmephant rename OLD NEW          # rewrite [[OLD]] in bodies and repoint link edges
ellmephant links [--json]          # list dangling [[key]] targets
ellmephant doctor [--verify]       # paths, schema, index health; --verify re-hashes weights
```

**Model, embeddings, benchmarks, interfaces**

```sh
ellmephant model status [--verify] [--repo R] [--revision REV]
ellmephant model fetch  [--force]  [--repo R] [--revision REV]
ellmephant model rm     [--all]    [--repo R] [--revision REV]
ellmephant embed "TEXT" [--query]          # debug: dimension + latency + norm
ellmephant bench [--golden] [--golden-file F] [--sweep] [--k N]
ellmephant mcp                             # MCP over HTTP at http://127.0.0.1:8888/mcp
ellmephant mcp --stdio                     # MCP over stdio
ellmephant                                 # REPL
```

### Configuration

`~/.ellmephant/config.toml` is optional. Flags override it, and it overrides the built-in defaults.
Start from `config.example.toml`:

```toml
model       = "Qwen/Qwen3-Embedding-0.6B"   # HF repo id, or a dir name under models/
revision    = "main"                         # branch/tag (resolved once to a sha) or a sha
cache_dir   = ""                             # weights root; empty = ~/.ellmephant/models
offline     = false                          # true = never open a socket
dim         = 1024                           # frozen at CREATE; MRL allows 32..=1024
precision   = "bf16"                         # bf16 | f16 (CPU is always f32)
device      = "cpu"                          # cpu | metal
instruction = "Given a question, retrieve relevant memories"   # query side only
default_k   = 5                              # search result count
default_max_distance = 0.6                   # cosine-distance ceiling; null disables it
port        = 8888                           # MCP HTTP port (`mcp --stdio` ignores it)
```

An invalid `device`, `precision`, or `dim = 0` is rejected before any model work.

### Model weights

Weights are fetched on first use into `~/.ellmephant/models/`, or supplied with `--model-dir`. The
cache is flat and sha-pinned. Once warm, runs need no network.

```sh
ellmephant model status              # report what is cached (no network)
ellmephant model status --verify     # re-hash every file
ellmephant model fetch               # download or repair; idempotent
ellmephant model rm --all            # remove cached revisions
ellmephant --offline doctor          # open no socket
```

`--model-dir DIR` uses an existing flat weights directory and implies `--offline`;
`ELLMEPHANT_OFFLINE=1` forces offline globally. Fetching is the only network operation.

### Search

A search matches a single **semantic phrase** — there are no key or tag filters and no regex
leg. Each memory is embedded as one **document** (`key` + tags + `title` + body), so a phrase
that shares words with the key or a tag can surface the memory too. `--k` caps the results
within `--max-distance`; with no phrase, results are unranked (key order). A result carries
`key`, `title`, `tags` and `distance` — never the body; fetch it with `show`, which is also how
you read a known key directly.

```sh
ellmephant search "where do the logs live"                  # semantic, k=5
ellmephant search "portainer-notes-gw"                      # key/tag tokens are in the embedding
ellmephant search "backups" --expand 1                      # + linked neighbours
```

### Wiki links

Author links inline in the body as `[[key]]`. They become link edges on write; `--expand` walks
them and `ellmephant links` reports dangling targets. Links are by key, never by id, so they
survive a `reembed` or a `rename`.

### MCP over HTTP (default)

`ellmephant mcp` listens on `127.0.0.1:8888` and serves the MCP tool surface at `/mcp`. The port
comes from `port` in `config.toml`, or `--port` / `ELLMEPHANT_MCP_PORT`; the interface comes from
`--host` / `ELLMEPHANT_MCP_HOST`. Multiple clients are served from the one store.

```sh
ellmephant mcp                                       # http://127.0.0.1:8888/mcp
ellmephant mcp --host 0.0.0.0 --port 9000            # bind all interfaces / another port
```

Point a client at `http://<host>:8888/mcp`. The default bind is localhost. There is no
authentication or TLS, so bind `0.0.0.0` or a public address only on a trusted network.

### MCP over stdio

For clients that launch the server themselves, use `--stdio`: the client starts the binary and
communicates over stdin/stdout.

```sh
ellmephant mcp --stdio
```

```jsonc
{
  "mcpServers": {
    "ellmephant": { "command": "/absolute/path/to/ellmephant", "args": ["mcp", "--stdio"] }
  }
}
```

### MCP tools

`memory_add`, `memory_update`, `memory_delete`, `memory_search`, `memory_get`,
`memory_backlinks`, `memory_list`, `memory_useful`. Read tools are annotated read-only and
`memory_delete` is annotated destructive. Each `memory_get` counts a `visits` read on the row;
`memory_useful` bumps a `was_useful` counter so an agent can flag a memory that earned its keep
(the count is independent of a fetch — a great title alone can earn it). `memory_search` matches a semantic `phrase` (the whole document — key,
tags, title, body — is embedded), with `k` and `max_distance`; there are **no key/tag filters
and no regex leg**, and hits carry titles, tags and distances, never bodies. The list-shaped
reads return compact **positional JSON** — a top-level `cols` header names the fields and each
row is a bare array, **one row per line** (so a raw result is scannable, not one blob), meaning
field names aren't repeated per row while the payload stays self-describing. `memory_search` →
`{"cols":["key","title","tags","distance"],"hits":[[…],…]}`;
`memory_list` → `{"cols":["key","title","tags"],"count":N,"memories":[[…],…]}`;
`memory_backlinks` → `{"cols":["key","title"],"backlinks":[[…],…]}`. The server also advertises
the write policy and the search strategy as its `instructions`.

## Usage in Docker

Build the image first (see [`INSTALL.md`](INSTALL.md)). The image's default command is the MCP
server; name a command to run anything else. The store is the `ellmephant-data` volume at `/data`.

### One-off commands

```sh
docker run --rm -v ellmephant-data:/data ellmephant:dev doctor
docker run --rm -v ellmephant-data:/data ellmephant:dev model fetch
docker run --rm -v ellmephant-data:/data ellmephant:dev search "internal gateway hostname"
docker run --rm -it -v ellmephant-data:/data ellmephant:dev      # REPL
```

### MCP over HTTP

Inside the container the server binds `0.0.0.0:8888`; publish the port and point a client at
`http://<host>:8888/mcp`:

```sh
docker run --rm -p 8888:8888 -v ellmephant-data:/data ellmephant:dev
```

### Logging

The image ships `RUST_LOG=ellmephant=info,rmcp=info,warn`, so `docker logs` shows a startup
banner (version, store path, model, dim, device), each `memory_search` (the query, hit count and
`elapsed_ms`) with every returned hit — `key`, `tags`, `distance` — plus the serialized response
size in bytes, each write
(`memory_add`/`memory_update`/`memory_delete`, with `key`, `tags`, `body_bytes`) and each
`memory_list` (count). The first semantic operation logs `loading encoder` /
`encoder loaded` around the ~1.2 GB weight load, and every tool failure is logged as a
`tool error`. `rmcp` at info surfaces client connect/disconnect. Override `RUST_LOG` to
quieten or deepen it, e.g. `-e RUST_LOG=warn` or `-e RUST_LOG=ellmephant=debug`.

### MCP over stdio

Keep stdin attached and do not allocate a TTY:

```sh
docker run --rm -i -v ellmephant-data:/data ellmephant:dev mcp --stdio
```

A client can launch that directly:

```jsonc
{
  "mcpServers": {
    "ellmephant": {
      "command": "docker",
      "args": ["run", "--rm", "-i", "-v", "ellmephant-data:/data", "ellmephant:dev", "mcp", "--stdio"]
    }
  }
}
```

### Docker Compose

```yaml
services:
  ellmephant:
    build: .
    image: ellmephant:dev
    ports:
      - "8888:8888"          # MCP over HTTP at http://localhost:8888/mcp
    volumes:
      - ellmephant-data:/data
    command: ["mcp"]         # default; HTTP transport
    # stdio instead: `command: ["mcp", "--stdio"]` with `stdin_open: true`
    stdin_open: true

volumes:
  ellmephant-data:
```

## FAQ

**Is it a vector database?** It includes a vector index, but it is also a relational store and a
link graph. The `memory` table is the source of truth; the indexes are derived and rebuildable.

**Does it send my data anywhere?** No. The only outbound request is the one-time weights
download, which can be disabled with `--offline` and a local cache.

**How do I back up or move the store?** Copy `~/.ellmephant/memory.db` (with any `-wal`/`-shm`
sidecars); it is the whole store. Everything else there is a rebuildable cache.

**Why are the weights fetched separately?** They are ~1.2 GB of data, not code, so they live in
the store directory rather than in the binary.

## Licence

MIT — see [`LICENSE`](LICENSE). The software is provided without warranty of any kind.
