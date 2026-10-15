# The plain-HTTP surface — the store without an MCP session

**Status:** slice 1 (`GET /health`) implemented; `/bootstrap` planned.

## Why

MCP is a *transport*. The store is the product. Today every read of the store — including the boot
read that reconstructs the agent — has to travel through an MCP session, and that has a cost we keep
paying:

- **A roll strands the client.** A new pod dies the old `Mcp-Session-Id`, and the server answers the
  stale one with `404 Not Found: Session not found` — an *application* error. A reconnect supervisor
  that fires on transport errors never sees it, the client wedges, and it then attempts an OAuth flow
  against a server that has no auth. Every upgrade needs a human because of this.
- **Reads cannot be retried.** A timed-out *read* is safe to repeat; a timed-out *write* is not, and
  MCP gives us no way to tell a caller which it is holding.
- **Everything must be an MCP client.** `curl`, `cron`, a Forgejo job, a Grafana datasource and a
  Kubernetes probe cannot reach the store. So each consumer grows its own client — including a
  hand-written one in a shell script, which is the smell that started this.
- **There is no health endpoint at all.** The deployment's readiness probe reports "ready" for a pod
  whose index is stale, because nothing answers the question.

Plain HTTPS has none of these properties: no session, so no session to strand; reads are `GET`s and
therefore retryable; anything with a TCP stack can call it; and a read-only `GET` can answer the
readiness question honestly.

**MCP stays.** TypingMind and other clients speak it, `/mcp` is untouched, and both surfaces are thin
wrappers over the same `Session`. This adds a door; it does not move one.

## The surface

| route | semantics | state |
| --- | --- | --- |
| `GET /health` | read-only readiness + the writer witness | **implemented** |
| `GET /bootstrap` | the boot payload in one read | planned |
| `GET /memory/{key}` | one memory, body included | planned |
| `GET /search?q=&k=&tags=` | the query surface | planned |
| `POST /memory`, `DELETE /memory/{key}` | writes — **auth first** | planned |

### Formats: text-first, and that is a design decision, not a preference

The consumer of `/bootstrap` is a language model, not `jq`. So:

- **Default `text/plain`**, `?format=json` for machines. Prose is the densest shape for a model to
  read, and it is the shape the memories already have. JSON spends tokens on braces, quotes and keys
  and forces parse-then-reason.
- **Writes are front-matter text** (`Title:` / `Tags:` / `Source:`, a `---`, then the body) — because
  the real tax today is a hand-written JSON tool call with escaped newlines inside a multi-line body.
- **`gzip` via `Accept-Encoding` only.** No base64: it is for binary in text, and would be a flat 33%
  tax on a payload that is already text.
- A stable JSON envelope still exists, so a test or a script never has to parse prose.

## `GET /health` (slice 1)

Two jobs: answer readiness *honestly*, and be the end of the 60-second mystery.

**It must not migrate.** `db::open` runs migrations; a probe that migrates can turn a stale binary
into a half-migrated store, and a k8s probe fires on a schedule. So `/health` opens its **own
read-only connection** (`SQLITE_OPEN_READ_ONLY`) and runs only `SELECT`s. The store is in WAL mode,
so a reader never blocks the writer.

**It must not take the store lock.** `Session` is one SQLite connection behind one `Mutex`, served
serially. A health check that queues behind a stall is useless precisely when it is needed. `/health`
touches no `Session` — it opens a second connection and reads the shared witness below instead.

**The writer witness.** `MemoryServer::with_store` runs every tool on a blocking thread under that one
lock and *already* measures `wait_ms — store lock acquired` — into a log line, which is invisible to a
stalled client and dies with the pod. Slice 1 makes the same fact an in-process observable: a
`LockWatch` records which tool holds the lock and since when, and `/health` reports it.

That is the whole point. Measured on the live store, embeddings are **sub-second** (semantic search
0.5 s; upsert 0.4 s) — so a 60-second tool call is not compute. It is a *queue*: one long holder, and
every other request behind it, including the writes that then appear to hang and commit after the
client has given up. `/health` reporting `writer busy 41.2s (memory_reembed)` names the culprit from
outside the process, with no MCP session involved.

```
ellmephant 0.19.0 ready
  uptime      3641s
  schema      v12 (binary v12)
  memories    272
  vectors     272/272
  projection  current
  links       481 edge(s), 1 dangling
  projects    18 project(s), 0 dangling preload key(s)
  writer      free
```

Unready (HTTP **503**, body still returned so the reason is in `kubectl describe`):

```
ellmephant 0.19.0 NOT READY
  ...
  writer      busy 41.2s (memory_reembed)
  ready       false: projection STALE — run `reembed`
```

Not ready means: schema behind the binary, projection stale, or vectors short of memories. A fresh
empty store (0/0 vectors, projection unrecorded) *is* ready — that is a new install, not a fault.

`?format=json` returns the same fields as an object, with `"ready": false` and `"reason"`.

## Boundaries and risks

- **Unauthenticated.** `/health` exposes counts and schema, no bodies — safe to expose read-only
  behind the gateway, and it must be reachable by a probe that holds no credential. The first *write*
  route needs per-area credentials (credential → namespace) landed first.
- **Read-only-ness is a promise the test enforces.** `tests/http.rs` asserts that probing a store
  whose file does not exist leaves no file behind and returns 503 — i.e. that `/health` cannot create
  or migrate a store.
- **Two doors to keep in step.** Mitigated by both being thin wrappers over `Session`; the same
  discipline that keeps one implementation of what a memory *is*.
- **This witnesses the stall; it does not fix it.** Marking the holder is slice 1 because it can be
  shipped and trusted in an afternoon. The fix (shorter critical sections, or a queue that reports
  depth and age) follows from what the witness catches.

## Revert

Additive: two `axum` routes on a router that already exists. `git revert` the commit, or drop the
routes from `http_router`. `/mcp` never changed.
