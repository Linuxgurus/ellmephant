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
| `GET /stats`, `GET /dupes`, `GET /projects` | the read surfaces | planned |
| `GET /memory/{key}`, `GET /memory/{key}/backlinks` | one memory, body included | planned |
| `GET /search?q=&k=&tags=` | the query surface | planned |
| `POST /memory`, `DELETE /memory/{key}`, `POST /memory/{key}/merge` | writes — **auth first** | planned |
| `POST /memory/{key}/upvote`, `PUT /memory/{key}/coherent`, `POST /project` | writes — **auth first** | planned |
| `POST /reembed` | the heaviest op; last, because it holds the store lock | planned |

**`/bootstrap` is a view, not a dump.** It carries the resident set a cold agent needs — the `cline-self`
keys, the boot body, hot, todos, open projects, stats, health, links — and an index of the routes
below, so everything else is one call away. It does **not** carry every body (there are hundreds): that
is not a boot, it is a context fill. If a fact is not meant to be resident, `/bootstrap` must not
smuggle it in.

### Parity with MCP — the requirement, not a nice-to-have

The MCP surface is 14 tools (`TOOLS` in `src/mcp/mod.rs`). A second door that cannot reach the same
core is a door with a queue in front of it: boot over HTTP, still write over MCP, and a wedged MCP
leaves the agent **read-only**. So every tool gets a route over the same `Session` call, and MCP
becomes a compatibility shim for clients that speak it rather than a dependency of the agent.

| MCP tool | HTTP | state |
| --- | --- | --- |
| `memory_get` | `GET /memory/{key}` | planned |
| `memory_search` | `GET /search` | planned |
| `memory_backlinks` | `GET /memory/{key}/backlinks` | planned |
| `memory_stats` | `GET /stats` (also folded into `/health` and `/bootstrap`) | planned |
| `memory_dupes` | `GET /dupes` | planned |
| `project_get` / `project_list` | `GET /project/{key}` / `GET /projects` | planned |
| `memory_upsert` | `POST /memory` | planned — auth first |
| `memory_delete` | `DELETE /memory/{key}` | planned — auth first |
| `memory_merge` | `POST /memory/{key}/merge` | planned — auth first |
| `memory_upvote` | `POST /memory/{key}/upvote` | planned — auth first |
| `memory_coherent` | `PUT /memory/{key}/coherent` | planned — auth first |
| `project_upsert` | `POST /project` | planned — auth first |
| `memory_reembed` | `POST /reembed` | planned — **last** |
| — | `GET /health` | **implemented** |

`/health` is the only row with no MCP equivalent: it is new capability, not a translation.

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

## Slice 2 — the full surface (design)

Every one of the 14 MCP tools gets a route over the same `Session` call, plus `/bootstrap`, so the rules
file can point at a URL and the agent's whole existence travels over HTTPS.

### Three invariants, in order of strength

1. **Reads are pure.** No `GET` may write — not a counter, not a `visits` bump, not a migration. A read
   that writes is not safely retryable, and retryability is half the reason this exists. **Consequence:**
   in MCP, `memory_get` bumps `visits`; over HTTP it must not, or a probe or a monitor inflates the
   usefulness signal. Counting moves to `POST /memory/{key}/visit`, where a write belongs. `POST` is
   still safe to retry blind, because the counter is idempotent per caller-reason rather than per call.
2. **Writes take the store lock; reads do not.** `Session` is one connection behind one mutex, so a read
   that needs it queues behind a writer. `/health` and `/bootstrap` therefore answer from a **second,
   read-only connection** (`SQLITE_OPEN_READ_ONLY`), never from `Session`. Slice 1 proved the pattern.
3. **Auth before writes, never after.** Every mutating route needs a bearer token. The 401 must carry
   **no OAuth `resource_metadata` challenge** — a client that finds one invents an auth flow, and that
   invention is exactly the wedge (`404 Session not found` → doomed OAuth) this project removes.

### Routes

Reads — no auth, pure, retryable, `?format=json` accepted everywhere:

| method | path | maps to | parameters |
| --- | --- | --- | --- |
| GET | `/health` | *(new)* | `?format=json` |
| GET | `/bootstrap` | *(new)* | `?format=json`, `?hydrate=keys\|snippet\|full` |
| GET | `/stats` | `memory_stats` | `?tags=true` |
| GET | `/memory/{key}` | `memory_get` | `?visit=false` (no bump by default) |
| GET | `/memory/{key}/backlinks` | `memory_backlinks` | — |
| GET | `/search` | `memory_search` | `?q=&k=&max_distance=&expand=&min_chars=&max_chars=&coherent=&fields=a,b&tag=x&tag=y` |
| GET | `/dupes` | `memory_dupes` | `?max_distance=&limit=` |
| GET | `/projects` | `project_list` | `?status=&pattern=&mode=&tag=` |
| GET | `/project/{key}` | `project_get` | `?hydrate=none\|snippet\|full` |

Writes — **auth required**, all of them:

| method | path | maps to | body / parameters |
| --- | --- | --- | --- |
| POST | `/memory` | `memory_upsert` | text front-matter, or JSON if `Content-Type: application/json` |
| POST | `/memory/{key}/visit` | *(new)* | — (the split-out counter) |
| POST | `/memory/{key}/upvote` | `memory_upvote` | — |
| POST | `/memory/{key}/merge` | `memory_merge` | `?into=<key>` |
| PUT | `/memory/{key}/coherent` | `memory_coherent` | `?value=true\|false\|null` |
| DELETE | `/memory/{key}` | `memory_delete` | — |
| POST | `/project` | `project_upsert` | JSON (structured) or text front-matter |
| POST | `/reembed` | `memory_reembed` | — **last**: heaviest lock holder, and `/health` watches it |

**Body formats, by Content-Type.** A memory body is *prose*, so `text/plain` front-matter wins there —
that is the format that removes JSON-escaping of multi-line bodies, which is where a hand-written call
actually fails. A project patch is *structured* (owner list, preload list, tags), where JSON is the
honest shape. So both are accepted, and the caller picks by `Content-Type`. `?format=` governs the
*response* only.

### Auth: build the seam, one value today

One bearer token, read from `ELLMEPHANT_WRITE_TOKEN` (env; a k8s Secret in the deployment). Absent or
wrong → `401` with `WWW-Authenticate: Bearer`, **no** `resource_metadata`, and a one-line text body.

The real design is per-area credentials mapping to a namespace, resolved server-side
(`ellmephant-namespaces`). That lands *on top* of this seam, so the seam must already have the right
shape: **resolve a credential → a scope, then hand the scope to the handler**, even while today's scope
is a single constant. A handler that reads a token directly would have to be rewritten; one that
receives a scope will not.


### The wire grammar (what a model reads)

**Reads default to `text/plain`; `?format=json` returns the object.** JSON responses are **byte-identical
to what the MCP tool returns** for the same call (`to_wire_json` is reused, not reimplemented) — that is
how we assert the two doors are one core rather than two implementations that drift.

**Writes accept `text/plain` front-matter**, with no escaping anywhere:

```
Key: zz-probe            (optional here if the key is in the path; required for POST /memory)
Title: one sharp line
Tags: a, b, c
Source: the command that derived this
---
the body, verbatim, on as many lines as needed -- quotes, colons, braces, even a line of "---".
```

`Tags:` is a comma list (empty = clear). An **unknown field is a 400 naming the field** rather than a
silent drop: a typo that still writes is worse than a rejection. Everything after the first `---` is the
body, byte-for-byte, with a trailing newline trimmed. Write responses are one line
(`stored zz-probe (reembedded=true)`, `deleted zz-probe`, `merged a -> b (3 refs repointed)`), or the
MCP tool's own object under `?format=json`.

### `GET /bootstrap` — the document

A view, not a dump. Sections, in this order:

```
BOOT ellmephant 0.19.0  2026-10-15T13:47:32Z
  ready      yes — projection current, 275/275 vectors, schema v12
  writer     free | busy 41.2s (memory_reembed)      <- the witness, always visible
  store      275 memories, 275 vectors, 18 projects, 512 links (1 dangling), 40 tags

SELF (tag cline-self) — 12 keys; bodies on demand: GET /memory/{key}
  bootstrap-self   BOOT: read me first — who I am + how to talk to James    [1240 chars]
  ...

BOOT NOTE — bootstrap-self                       <- the ONE body that must be resident
  <body verbatim>

OPEN (tag todo) — 9
  todo-page-james-silent — the emergency page is silent AT NIGHT

ACTIVE PROJECTS — 8
  ellmephant-http — the whole MCP surface over REST, and a /bootstrap my rules can point at

HOT — recently useful
  bootstrap-self (visits 38, useful 19)
  ...

ROUTES
  GET  /bootstrap  /health  /stats  /search?q=  /memory/{key}  /projects  /dupes
  POST /memory  /memory/{key}/visit|upvote|merge  /project  /reembed        (auth: bearer)
```

Three deliberate choices. **`SELF` lists keys, titles and sizes, not bodies** — so the agent chooses what
to pull, and `?hydrate=full` is there when it wants everything. **Exactly one body is resident**, the boot
note, because a boot that carries every body is a context fill. And **`writer` is on the first screen**:
the thing that stalls us should be visible before anything else.

**Boot counting is deliberately absent.** A counter would make `/bootstrap` a writer, which breaks
invariant 1. Boot observability already exists — the gateway's access log records every `GET /bootstrap`
— so the count belongs to the log, not to the store.

### Error semantics

| status | when | body |
| --- | --- | --- |
| 400 | bad front-matter, unknown field, unparseable param | one line naming the field |
| 401 | write route with no/mismatched bearer | one line; `WWW-Authenticate: Bearer` — **nothing more** |
| 404 | unknown key, or unknown path (our own `fallback`, never the gateway's) | `not found: <what>` |
| 405 | wrong method on a known path | `method not allowed` |
| 503 | `/health` unready (schema behind, projection stale, vectors short) | the health body, with the reason |

Under `?format=json` every error is `{"error": "...", "status": N}`, so a script never parses prose.

### Tests (`tests/http.rs`, offline, fake embedder)

1. **Parity is enforced, not asserted by hand**: every name in `TOOLS` must appear in the route table and
   the table's length must equal `TOOLS.len()`. A tool added without a route fails the suite.
2. **Read purity**: `GET /memory/{key}` leaves `visits` unchanged (compare before/after).
3. **One core**: for each read route the JSON body equals the MCP tool's output for the same call.
4. **Auth**: every write route → 401 bare, 401 on a wrong token, and `WWW-Authenticate` is exactly
   `Bearer` — **no `resource_metadata`**, because that is the header that starts an OAuth flow.
5. **The text round-trip** (the reason the format exists): a body containing `"quotes"`, `: colons`, `{}`
   and a line of `---` is stored and read back **byte-identical**.
6. **Front-matter strictness**: an unknown field is a 400 that names it, and nothing is written.
7. **`/bootstrap` shape**: exactly one BOOT NOTE body; a non-resident memory shows its title and not its
   body; `writer` and `ROUTES` present.
8. **`/bootstrap` is read-only**: it changes no `visits` and no counts.
9. **Failures**: 404 unknown key, 405 wrong method, unknown path hits our fallback with a text body.

### The rules stub (criterion 2) — a different repo, so a separate change

`~/.cline/rules/main.md` lives in `cline-home`, not here, so it cannot ride in this PR. The target text is
three lines: point at `GET /bootstrap`, keep the secrets rule local, and say the rest is served. It ships
**after** `/bootstrap` is live in the cluster, and it is a one-way door to be entered deliberately: a boot
whose rules arrive over the network is a boot that fails when the network does. The fallback is the local
file — which is why the *pointer*, not the content, is what stays.

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
