# ellmephant

**Long-term memory for AI agents: one binary, one SQLite file, local-first.**

ellmephant gives your AI assistants a shared memory that lives entirely on your own machine. Save
a fact once, and later any assistant hooked up to Ellmephant can dig it back up *by meaning* — you
don't have to remember the exact words you used.

- Under the hood, an **embedder** (Qwen3-Embedding by default) turns everything into vectors. That's
  what makes "search by meaning" actually work.
- One SQLite file is the source of truth. The vector index and the link graph are just derived from
  it — nuke them and they rebuild themselves.
- It speaks **MCP**, so any chatbot that can reach it (Cline, say) can talk to it. Web-based
  chatbots like TypingMind work too, as long as whatever MCP connector they rely on can reach
  Ellmephant.


## Contents

- [Getting started](#getting-started) — follow these in order
- [Connect an AI assistant](#connect-an-ai-assistant)
- [Don't lose your data](#dont-lose-your-data)
- [Optional: Podman Compose](#optional-podman-compose)
- [Using TypingMind](#using-typingmind)
- [Troubleshooting](#troubleshooting)
- [FAQ](#faq)
- [For the curious and the engineers](#for-the-curious-and-the-engineers)

---

## Getting started

We use **Podman** below, but it all works exactly the same with Docker — just swap the command
name (see Step 0). Set aside about 15 minutes, most of which is downloads.

### Step 0 — Install Podman (skip if you already have Docker)

- **macOS / Windows:** install [Podman Desktop](https://podman-desktop.io/) and start it — it sets
  up the little Linux VM Podman needs.
- **Linux:** `sudo apt install podman` (Debian/Ubuntu) or `sudo dnf install podman` (Fedora).

On macOS and Windows you only have to kick that VM awake once:

```sh
podman machine init      # first time only; Podman Desktop may already have done this
podman machine start
```

Confirm it's alive:

```sh
podman --version
```

> **Already have Docker?** Everything below works as-is if you swap `podman` for `docker` —
> `docker pull`, `docker run`, `docker compose`, and so on. Podman is a drop-in replacement, so
> the rest of this guide applies verbatim.

### Running Ellmephant

**IMPORTANT** Ellmephant really wants a data volume to keep its memories. Without one it runs in
ephemeral mode, and everything it remembers vanishes the moment it stops.

You can run it either way: use the [Podman Compose setup](#optional-podman-compose) described
below, or just run this command:

```sh
podman run -d --rm -p 8888:8888 -v ellmephant-data:/data ghcr.io/jdblack/ellmephant:latest
# Or docker, if you're not on podman yet
```

That first start takes a few minutes — Ellmephant has to set up its data storage and pull down the
LLM embedder it uses to encode your memories.

The embedder — the model that turns memories into vectors — defaults to
**`Qwen/Qwen3-Embedding-0.6B`** (~1.2 GB) and downloads once, on first use. You never have to
fetch, verify, or rebuild it by hand; the server takes care of it. If you like to tinker, the
advanced knobs live in [`ENGINEERING.md`](ENGINEERING.md) and
[`config.example.toml`](config.example.toml).

---

## Connect an AI assistant

MCP (the Model Context Protocol) is how clients read and write the store. There are two
transports: HTTP, where the client connects to a server that's already running, and stdio, where
the client launches the server itself. Our examples use Podman, so we'll stick with HTTP.

### MCP over HTTP

With the server running (see [Running Ellmephant](#running-ellmephant)), point your client at
`http://localhost:8888/mcp`, set it to the **Remote** / **Streamable HTTP** transport, and leave
authentication off.

**Cline CLI** — run `cline mcp`, pick *Add server*, and fill in the fields:

![Adding Ellmephant as a remote MCP server with the Cline CLI](Docs/Cline%20CLI%20setup.png)

**Cline UI** — open *Settings → MCP Servers*, hit *Add MCP Server*, and use the same URL:

![Adding Ellmephant as a remote MCP server in the Cline UI](Docs/Cline%20UI%20setup.png)

### MCP over stdio (e.g. standard in and out)

If your client would rather launch the server itself, drop this into its MCP config — no
long-running container needed:

```jsonc
{
  "mcpServers": {
    "ellmephant": {
      "command": "podman",
      "args": ["run", "--rm", "-i", "-v", "ellmephant-data:/data",
               "ghcr.io/jdblack/ellmephant:latest", "mcp", "--stdio"]
    }
  }
}
```

### What the assistant gets

Your assistant gets a whole toolbox for memories — it can add, update and delete them as you talk.
You never run any of it yourself; just say things like "What do you remember about kubernetes?" or
"Save the important parts of this conversation to memory!" and it takes care of the rest.
Maintenance is the same story: adding, updating, linking, deleting — it all happens through the
assistant, in conversation. There are no commands for you to run.

Alongside the memories sit **projects**: an activity over time — a goal, where it stands, who owns
it, when it started and when it ended — that carries its own context and holds the memories written
for it. "What am I working on?" and "what did I finish last month?" come out of the same store.

---

## Don't lose your data

Your store is the Podman volume `ellmephant-data`, mounted at `/data`. Two rules:

1. Always pass `-v ellmephant-data:/data`.
2. Never run `podman volume rm ellmephant-data` unless you actually mean to erase everything.

---

## Optional: Podman Compose

If you'd rather describe the container in a file than type long commands, Compose is your friend.
(On Docker, the same file works with `docker compose`.) The repo ships a ready-made
[`compose.yaml`](compose.yaml) with the server, the published port and the `ellmephant-data`
volume already wired up. So the whole workflow shrinks to:

```sh
podman compose up -d     # start the server
podman compose down      # stop the server
```

> `podman compose` needs a provider installed — the `podman-compose` package or `docker-compose`.
> Most desktop installs already include one.

---

## Using TypingMind

TypingMind can use Ellmephant, but it needs a connector to do it — and the safe way is a private
connector running next to Ellmephant, **not** TypingMind's hosted one. We ship a two-container
stack for exactly that. The full walkthrough — starting it, wiring it into TypingMind, and the
usual gotchas — lives in [`TYPINGMIND.md`](TYPINGMIND.md).

---

## Troubleshooting

**`Cannot connect to the Podman socket`, or `command not found: podman`.**
Podman isn't running. Fire up Podman Desktop, or on macOS/Windows run `podman machine start`. If
the command itself is missing, Podman isn't installed — see Step 0.

**Memories "disappeared".**
Nine times out of ten it's a missing `-v ellmephant-data:/data`. Your data is still in the volume;
add the flag back and it reappears.

**The first memory operation pauses for a while.**
That's the embedder. The very first time it's downloading ~1.2 GB (a few minutes); after that it's
just loading into memory (10–20 seconds). One-time either way — the logs say `loading encoder` /
`encoder loaded` around it.

**The client can't reach `localhost:8888`.**
Check the server container is still up (`podman ps`). If port 8888 is already taken, publish a
different host port — `-p 8899:8888` — and use `http://localhost:8899/mcp`.

**On Apple Silicon.**
The published image is multi-arch (`linux/amd64` + `linux/arm64`), so an M-series Mac pulls the
native arm64 build automatically — no emulation.

**Logs.**
The container's output records startup, every search and every write. The image logs at
`RUST_LOG=ellmephant=info,rmcp=info,warn`; stick `-e RUST_LOG=warn` on the run command to quieten
it, or `-e RUST_LOG=ellmephant=debug` to get nosy.

**Still stuck?**
Start with the logs above; if that doesn't explain it, open an issue and paste what you see.

---

## FAQ

**Does my data leave the machine?**
Only to and from wherever you host Ellmephant. Run it on localhost and your memories stay on your
machine; host it in a k8s cluster and, of course, they go there =) The one other network call is
the one-time ~1.2 GB embedder download, which `--offline` disables.

**Is this a database I have to administer?**
Nope. It's one volume. Nothing to install, tune, or babysit.

**Why is the embedder a separate download?**
It's ~1.2 GB of data, not code. Keeping it out of the binary keeps the image small and lets you
swap in your own weights.

**Can several assistants share one memory?**
Yes — that's the whole point. Point as many clients as you like at the same server; they all share
one store.

**How much disk does it use?**
Roughly 1.2 GB for the weights, plus the store (one small SQLite file).

**Can I move or restore a store?**
Yep. Copy `memory.db` (along with any `-wal`/`-shm` sidecars) into a volume — that file *is* the
whole store. Everything else in the volume is a rebuildable cache.

---

## For the curious and the engineers

- **Install without a container / build from source** → [`INSTALL.md`](INSTALL.md)
- **How it works inside** → [`ENGINEERING.md`](ENGINEERING.md)
- **Development and releases** → [`DEVELOPMENT.md`](DEVELOPMENT.md)
- **Specification** → [`SPEC.md`](SPEC.md)
- **Design plan and history** → [`PLAN.md`](PLAN.md)

### Building the image yourself

The published image is what everyone pulls. Want your own? Grab a checkout and build it:

```sh
git clone https://github.com/jdblack/ellmephant.git && cd ellmephant
podman build -t ellmephant:dev .
```

Then use `ellmephant:dev` wherever this guide says `ghcr.io/jdblack/ellmephant:latest`. Fair
warning: that first build compiles the whole ML stack (the SQLite/`sqlite-vec` C sources, the
candle stack, ~400 crates) and can take 5–20 minutes. [`DEVELOPMENT.md`](DEVELOPMENT.md) has the
full workflow.

### Notes for those who like the details

- One static binary, one SQLite file. The Qwen3 encoder runs in-process via candle — no Python, no
  ONNX Runtime, no sidecar.
- The rows are the source of truth — `memory`, and the `project` table behind projects. The vector
  index (`sqlite-vec` `vec0`) and the link graph are derived and rebuildable.
- Store directory is `/data` (`ELLMEPHANT_HOME=/data`); the image's default command is the MCP
  server, binding `0.0.0.0:8888`.
- Weights cache at `<home>/models/`, sha-pinned per revision; `--offline` forbids network and
  `--model-dir` uses a pre-fetched flat directory.
- No auth or TLS on the MCP server — only publish its port on a trusted network.

## Licence

MIT — see [`LICENSE`](LICENSE). Provided without warranty of any kind.

