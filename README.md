# llmphant

**Long-term memory for AI agents: one binary, one SQLite file, local-first.**

llmphant gives AI assistants a shared, persistent memory that runs entirely on your machine:

- the [Qwen3-Embedding](https://huggingface.co/Qwen/Qwen3-Embedding-0.6B) encoder is compiled
  into the binary; no Python or external runtime is required;
- one SQLite file is the source of truth, with rebuildable full-text, vector, and link indexes;
- it speaks [MCP](https://modelcontextprotocol.io) over Streamable HTTP (or stdio), so Cline,
  TypingMind, and other MCP clients read and write the same memory;
- there is an interactive REPL and a one-shot CLI for scripting and cron jobs.

It contacts the network once, to fetch model weights, and runs offline afterwards.

## Documentation

- **Install** → [`INSTALL.md`](INSTALL.md)
- **Engineering / internals** → [`ENGINEERING.md`](ENGINEERING.md)
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

llmphant is the local answer: one file, one process, one shared memory.

## Quick start

```sh
llmphant doctor            # create ~/.llmphant/ and the schema
llmphant model fetch       # one-time ~1.2 GB weights download
llmphant add --scope infra --title "Gateway hostname" \
  --body "the gateway answers at portainer-notes-gw.internal on 8443" --tag network
llmphant search "which host is the internal gateway"
llmphant                   # REPL
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
llmphant add    --scope S --title T --body B [--key K] [--type signpost|dictate|fact|profile]
                [--source SRC] [--tag T]...        # --body - reads stdin; upsert by key
llmphant search "QUERY" [--scope S] [--k N] [--mode hybrid|vec|fts]
                [--max-distance D] [--expand N] [--json]
llmphant show   KEY [--json]
llmphant rm     KEY
llmphant tags   [--json] | tags prune [--dry-run]
```

`add` upserts by `key`; running it twice with the same key yields one memory.

**Maintenance**

```sh
llmphant reembed                 # recompute every vector; needed after a model/dim change
llmphant rename OLD NEW          # rewrite [[OLD]] in bodies and repoint link edges
llmphant links [--json]          # list dangling [[key]] targets
llmphant doctor [--verify]       # paths, schema, index health; --verify re-hashes weights
```

**Export / import**

```sh
llmphant export FILE.jsonl              # canonical JSONL; no model needed
llmphant import FILE.jsonl [--dry-run]  # upsert by key; re-embed only when content changed
```

**Model, embeddings, benchmarks, interfaces**

```sh
llmphant model status [--verify] [--repo R] [--revision REV]
llmphant model fetch  [--force]  [--repo R] [--revision REV]
llmphant model rm     [--all]    [--repo R] [--revision REV]
llmphant embed "TEXT" [--query]          # debug: dimension + latency + norm
llmphant bench [--golden] [--golden-file F] [--sweep] [--k N]
llmphant mcp                             # MCP over HTTP at http://127.0.0.1:8888/mcp
llmphant mcp --stdio                     # MCP over stdio
llmphant                                 # REPL
```

### Configuration

`~/.llmphant/config.toml` is optional. Flags override it, and it overrides the built-in defaults.
Start from `config.example.toml`:

```toml
model       = "Qwen/Qwen3-Embedding-0.6B"   # HF repo id, or a dir name under models/
revision    = "main"                         # branch/tag (resolved once to a sha) or a sha
cache_dir   = ""                             # weights root; empty = ~/.llmphant/models
offline     = false                          # true = never open a socket
dim         = 1024                           # frozen at CREATE; MRL allows 32..=1024
precision   = "bf16"                         # bf16 | f16 (CPU is always f32)
device      = "cpu"                          # cpu | metal
instruction = "Given a question, retrieve relevant memories"   # query side only
default_k   = 10                             # search result count
port        = 8888                           # MCP HTTP port (`mcp --stdio` ignores it)
```

An invalid `device`, `precision`, or `dim = 0` is rejected before any model work.

### Model weights

Weights are fetched on first use into `~/.llmphant/models/`, or supplied with `--model-dir`. The
cache is flat and sha-pinned. Once warm, runs need no network.

```sh
llmphant model status              # report what is cached (no network)
llmphant model status --verify     # re-hash every file
llmphant model fetch               # download or repair; idempotent
llmphant model rm --all            # remove cached revisions
llmphant --offline doctor          # open no socket
```

`--model-dir DIR` uses an existing flat weights directory and implies `--offline`;
`LLMPHANT_OFFLINE=1` forces offline globally. Fetching is the only network operation.

### Search

`hybrid` (the default) runs the lexical and vector legs and fuses them with RRF (k=60).
`--mode vec|fts` runs one leg. `--mode fts` needs no weights and no network.

```sh
llmphant search "where do the logs live"            # hybrid, k=10
llmphant search "portainer-notes-gw" --mode fts     # exact identifier, no model
llmphant search "gateway" --scope infra --json
llmphant search "backups" --expand 1                # include linked neighbours
```

Every result carries its fused score (and cosine distance for the vector leg), and results are
bounded snippets rather than full bodies.

### Wiki links

Author links inline in the body as `[[key]]`. They become link edges on write; `--expand` walks
them and `llmphant links` reports dangling targets. Links are by key, so they survive
export/import.

### Export / import

The JSONL format uses the canonical fields
(`key,scope,type,title,body,source,tags,created,updated`) and excludes ids and vectors, so a dump
survives a different machine, model, or dimension. Import is all-or-nothing and upserts by key;
re-importing an unchanged file is a no-op.

### MCP over HTTP (default)

`llmphant mcp` listens on `127.0.0.1:8888` and serves the MCP tool surface at `/mcp`. The port
comes from `port` in `config.toml`, or `--port` / `LLMPHANT_MCP_PORT`; the interface comes from
`--host` / `LLMPHANT_MCP_HOST`. Multiple clients are served from the one store.

```sh
llmphant mcp                                       # http://127.0.0.1:8888/mcp
llmphant mcp --host 0.0.0.0 --port 9000            # bind all interfaces / another port
```

Point a client at `http://<host>:8888/mcp`. The default bind is localhost. There is no
authentication or TLS, so bind `0.0.0.0` or a public address only on a trusted network.

### MCP over stdio

For clients that launch the server themselves, use `--stdio`: the client starts the binary and
communicates over stdin/stdout.

```sh
llmphant mcp --stdio
```

```jsonc
{
  "mcpServers": {
    "llmphant": { "command": "/absolute/path/to/llmphant", "args": ["mcp", "--stdio"] }
  }
}
```

### MCP tools

`memory_add`, `memory_update`, `memory_delete`, `memory_search`, `memory_get`,
`memory_backlinks`, `memory_export`, `memory_import`. Read tools are annotated read-only and
`memory_delete` is annotated destructive. `memory_search` returns scored snippets; its `mode=fts`
path needs no weights.

## Usage in Docker

Build the image first (see [`INSTALL.md`](INSTALL.md)). The image's default command is the MCP
server; name a command to run anything else. The store is the `llmphant-data` volume at `/data`.

### One-off commands

```sh
docker run --rm -v llmphant-data:/data llmphant:dev doctor
docker run --rm -v llmphant-data:/data llmphant:dev model fetch
docker run --rm -v llmphant-data:/data llmphant:dev search "internal gateway hostname"
docker run --rm -it -v llmphant-data:/data llmphant:dev      # REPL
```

### MCP over HTTP

Inside the container the server binds `0.0.0.0:8888`; publish the port and point a client at
`http://<host>:8888/mcp`:

```sh
docker run --rm -p 8888:8888 -v llmphant-data:/data llmphant:dev
```

### MCP over stdio

Keep stdin attached and do not allocate a TTY:

```sh
docker run --rm -i -v llmphant-data:/data llmphant:dev mcp --stdio
```

A client can launch that directly:

```jsonc
{
  "mcpServers": {
    "llmphant": {
      "command": "docker",
      "args": ["run", "--rm", "-i", "-v", "llmphant-data:/data", "llmphant:dev", "mcp", "--stdio"]
    }
  }
}
```

### Docker Compose

```yaml
services:
  llmphant:
    build: .
    image: llmphant:dev
    ports:
      - "8888:8888"          # MCP over HTTP at http://localhost:8888/mcp
    volumes:
      - llmphant-data:/data
    command: ["mcp"]         # default; HTTP transport
    # stdio instead: `command: ["mcp", "--stdio"]` with `stdin_open: true`
    stdin_open: true

volumes:
  llmphant-data:
```

## FAQ

**Is it a vector database?** It includes a vector index, but it is also a relational store, a
full-text index, and a link graph. The `memory` table is the source of truth; the indexes are
derived and rebuildable.

**Does it send my data anywhere?** No. The only outbound request is the one-time weights
download, which can be disabled with `--offline` and a local cache.

**How do I export everything?** `llmphant export memory.jsonl` writes all memories to JSONL.

**Why are the weights fetched separately?** They are ~1.2 GB of data, not code, so they live in
the store directory rather than in the binary.

## Licence

MIT — see [`LICENSE`](LICENSE). The software is provided without warranty of any kind.
