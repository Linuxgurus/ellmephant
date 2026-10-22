# The plain-HTTP surface — the store without an MCP session

**Status:** slices 1 and 2 implemented — the whole MCP surface is served over REST, plus `/health` and
`/bootstrap`. Writes are **open** while `ELLMEPHANT_WRITE_TOKEN` is unset (deliberate and temporary —
see "Auth"), so MCP is no longer load-bearing for anything. Remaining: the rules stub (a `cline-home`
change).

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
| `GET /bootstrap` | the boot payload in one read | **implemented** |
| `GET /stats`, `GET /dupes`, `GET /projects` | the read surfaces | **implemented** |
| `GET /memory/{key}`, `GET /memory/{key}/backlinks` | one memory, body included | **implemented** |
| `GET /search?q=&k=&tag=` | the query surface | **implemented** |
| `POST /memory`, `DELETE /memory/{key}`, `POST /memory/{key}/merge` | writes | **implemented** |
| `POST /memory/{key}/upvote`, `POST /project` | writes | **implemented** |
| `POST /reembed` | QUEUES a re-embed (`{queued: N}`) and returns; the worker does the work | **implemented** |

**A write returns before its vector exists.** `POST /memory` upserts the row and *schedules* its
document; the store's encode worker indexes it a moment later, so a `GET /search` immediately after a
write may not see it. The row's `encode_hash` reads `encode_pending` until the worker settles it. The
write's reply carries `queued: true|false` to say what happened.

`?wait=true` on `POST /memory` is the exception: the request settles that one document before it
answers, so the row is searchable the moment it returns. It holds the store lock across the model call
— exactly the coupling the queue exists to remove — so it is opt-in and for callers that genuinely
cannot proceed otherwise.

**`/bootstrap` is the door, and what is behind it.** One fetch must be enough to *boot*: what this is,
how to get a memory, who you are, what is open, where the rest is. It carries the resident set — the
`stet-self` body **in full** (or the legacy `bootstrap-self` while a rename is in flight), the `cline-self` keys with their titles, the open projects, and a
state line — plus the four calls that reach everything else. It does **not** carry every body (there
are hundreds): that is not a boot, it is a context fill. If a fact is not meant to be resident,
`/bootstrap` must not smuggle it in.

**And it stays small, because a boot that arrives truncated is worse than no boot.** A fetching client
handed the old document — a verbatim `initialize` + `tools/list` dump, 14.8 KB — delivered it with
7 KB punched out of the middle, and the reader has no way to know. So the default view is budgeted
(`BOOT_BUDGET_BYTES`, asserted by the suite) and the tool catalogue is `?tools=true`: the one part
that cannot be made small, which the caller should choose to spend tokens on.

### Parity with MCP — the requirement, not a nice-to-have

The MCP surface is 13 tools (`TOOLS` in `src/mcp/mod.rs`). A second door that cannot reach the same
core is a door with a queue in front of it: boot over HTTP, still write over MCP, and a wedged MCP
leaves the agent **read-only**. So every tool gets a route over the same `Session` call, and MCP
becomes a compatibility shim for clients that speak it rather than a dependency of the agent.

| MCP tool | HTTP | state |
| --- | --- | --- |
| `memory_get` | `GET /memory/{key}` | **implemented** |
| `memory_search` | `GET /search` | **implemented** |
| `memory_backlinks` | `GET /memory/{key}/backlinks` | **implemented** |
| `memory_stats` | `GET /stats` (also folded into `/health` and `/bootstrap`) | **implemented** |
| `memory_dupes` | `GET /dupes` | **implemented** |
| `project_get` / `project_list` | `GET /project/{key}` / `GET /projects` | **implemented** |
| `memory_upsert` | `POST /memory` | **implemented** |
| `memory_delete` | `DELETE /memory/{key}` | **implemented** |
| `memory_merge` | `POST /memory/{key}/merge` | **implemented** |
| `memory_upvote` | `POST /memory/{key}/upvote` | **implemented** |
| `project_upsert` | `POST /project` | **implemented** |
| `memory_reembed` | `POST /reembed` | **implemented** — QUEUES (`{queued: N}`); the worker does the work |
| — | `GET /health` | **implemented** |
| — | `GET /bootstrap` | **implemented** |

This table read `planned` for every row until it was corrected on 2026-10-16: the surface shipped in
PR #18 and had been live for a day, and the table below (the route-by-route one) had said so the whole
time — two tables in one file disagreeing about whether the code existed.

`/health` and `/bootstrap` are the rows with no MCP equivalent: new capability, not translations.

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
  encode      0 pending, 0 failed
```

Unready (HTTP **503**, body still returned so the reason is in `kubectl describe`):

```
ellmephant 0.19.0 NOT READY
  ...
  writer      busy 41.2s (memory_reembed)
  ready       false: writer held too long (`busy 41.2s (memory_reembed)`) — a write would queue behind it
```

Not ready means: **schema behind the binary, projection stale, or a writer held past 2 s** — readiness
for reads *and* writes, and nothing else. A fresh empty store (0/0 vectors, projection unrecorded) *is*
ready — that is a new install, not a fault.

**The encode queue is reported and never gated on.** A write schedules its document and returns, so
between the write and the worker's pass there are more memories than vectors — the *normal* state of a
healthy store, not a fault to 503 on or to fix with `reembed`. Read it as load, not as damage:

- `encode_pending` — documents still owing a vector. Non-zero is normal; it is what a write burst looks
  like from outside.
- `encode_failed` — entries the encoder gave up on after `MAX_ATTEMPTS` (5). **This one is a defect**:
  the document is unsearchable and no retry will fix it until it is written again.
- `encode_oldest_s` — how long the oldest pending document has waited, in seconds (`null` when the
  queue is empty). If it only grows, the worker is not running or not keeping up; nothing else in the
  store would say so.

`?format=json` returns the same fields as an object, with `"ready": false` and `"reason"`.

## Slice 2 — the full surface (design)

Every one of the 14 MCP tools gets a route over the same `Session` call, plus `/bootstrap`, so the rules
file can point at a URL and the agent's whole existence travels over HTTPS.

### Three invariants, in order of strength

1. **Reads are pure — telemetry aside.** No `GET` may write *data*: no migration, no content, nothing a
   caller could be relying on. **A counter is not data.** So a read that counts a visit is still safe to
   repeat — the worst a retry does is inflate a ranking signal — and MCP's `memory_get` has always counted,
   because a fetch is evidence that needs no declaration: that is exactly the difference between `visits`
   and the *declared* `times_useful`. `GET /memory/{key}` therefore counts, and the two doors agree byte
   for byte. Everything else about a read stays pure; `/health` and `/bootstrap` write nothing at all.
   And MCP's `project_get` does **not** count the bodies it hydrates, so nothing here does either: no
   route counts a read MCP would not. There is no `POST /memory/{key}/visit` — it existed only to work
   around the old rule, and once the rule went it would have been the one reachable route with no
   counterpart on the other door.
2. **Writes take the store lock; reads do not.** `Session` is one connection behind one mutex, so a read
   that needs it queues behind a writer. `/health` and `/bootstrap` therefore answer from a **second,
   read-only connection** (`SQLITE_OPEN_READ_ONLY`), never from `Session`. Slice 1 proved the pattern.
3. **Auth is a config flip, and today it is open.** Every mutating route resolves a bearer to a scope
   before it does anything, and a rejection must carry **no OAuth `resource_metadata` challenge** — a
   client that finds one invents an auth flow, and that invention is exactly the wedge (`404 Session not
   found` → doomed OAuth) this project removes. With no `ELLMEPHANT_WRITE_TOKEN` configured there is
   nothing to resolve against, so writes are open. See "Auth".

### Routes

Reads — no auth, `?format=json` everywhere; pure except the `visits` counter:

| method | path | maps to | parameters |
| --- | --- | --- | --- |
| GET | `/health` | *(new)* | `?format=json` |
| GET | `/bootstrap` | *(new)* | `?format=json`; `?tools=true` adds the tool catalogue |
| GET | `/stats` | `memory_stats` | `?tags=true` |
| GET | `/memory/{key}` | `memory_get` | counts a `visits` read, exactly as MCP does |
| GET | `/memory/{key}/backlinks` | `memory_backlinks` | — |
| GET | `/search` | `memory_search` | `?q=&k=&max_distance=&expand=&min_chars=&max_chars=&fields=a,b&tag=x&tag=y` |
| GET | `/dupes` | `memory_dupes` | `?max_distance=&limit=` |
| GET | `/projects` | `project_list` | `?status=&pattern=&mode=&tag=` |
| GET | `/project/{key}` | `project_get` | `?hydrate=none\|snippet\|full` |

Writes — **auth-gated by design and OPEN today** (invariant 3; set `ELLMEPHANT_WRITE_TOKEN` to close them):

| method | path | maps to | body / parameters |
| --- | --- | --- | --- |
| POST | `/memory` | `memory_upsert` | text front-matter, or JSON if `Content-Type: application/json` |
| POST | `/memory/{key}/upvote` | `memory_upvote` | — |
| POST | `/memory/{key}/merge` | `memory_merge` | `?into=<key>`, or a JSON body `{"from","into"}` |
| DELETE | `/memory/{key}` | `memory_delete` | — |
| POST | `/project` | `project_upsert` | JSON — a project patch is structured (owner / preload / tags) and front-matter has no grammar for it |
| POST | `/reembed` | `memory_reembed` | — **last**: heaviest lock holder, and `/health` watches it |

**Body formats, by Content-Type.** A memory body is *prose*, so `text/plain` front-matter wins there —
that is the format that removes JSON-escaping of multi-line bodies, which is where a hand-written call
actually fails. A project patch is *structured* (owner list, preload list, tags), where JSON is the
honest shape. So both are accepted, and the caller picks by `Content-Type`. `?format=` governs the
*response* only.

**And every route that takes an argument takes the MCP tool's own arguments as the body**, with its
query form as shorthand: `POST /memory/{key}/merge` accepts `{"from","into"}` as well as `?into=`. A
caller that knows the tool should not have to learn a second spelling for the same thing — that is
what parity means here. `POST /memory` and `POST /project` already took the tool's object. A body
field that disagrees with the path (`{"from":"a"}` on `/memory/b/merge`) is a `400` that names both,
never a silent winner.

### Auth: **no key today.** The seam is there; the gate is not.

**Reads and writes are equally open, with no credential of any kind — exactly as MCP is.** That is a
decision, not an oversight: the point of this surface is to be the *easy* door, and a key we have no way
to issue yet would make it the harder one. Nothing in the tree sets `ELLMEPHANT_WRITE_TOKEN` (not the
chart, not the workflows, not the Dockerfile), so no route can demand one by accident.

The seam is kept, dormant, because auth comes back soon and re-adding it should be a config flip rather
than a code change. One bearer token, read from `ELLMEPHANT_WRITE_TOKEN` (env; a k8s Secret in the
deployment). A **wrong** token → `401` with `WWW-Authenticate: Bearer`, **no** `resource_metadata`, and a
one-line text body.

**Set the variable and the gate returns; unset it and there is none.** So the open state must never be
described as "deny by default with a hole in it" — it is simply no gate, and the tests still prove the
gate *works* for the day it matters (`a_configured_token_closes_the_door_again`).

**Open until the token is set — deliberately, and with an expiry date.** An earlier draft denied by
default, so that a deployment which forgot the env var got a read-only surface rather than an open one.
That was the wrong trade for this moment: requiring a credential before there is any way to issue one
keeps this door read-only, which is the one state it exists to end — and staying read-only is exactly
what keeps MCP load-bearing. So an unset `ELLMEPHANT_WRITE_TOKEN` now means **any caller may write**.

This is not the shape the thing should keep. The store is private, behind the gateway, and this is a
decision with an expiry date rather than a design. It is written down here so that the open state is a
choice somebody made, not a surprise somebody found. Setting the env var closes the door with no code
change, so the gate stays a flip rather than a rewrite — and `/bootstrap` **says which state it is in**,
so a caller never has to assume.

The real design is per-area credentials mapping to a namespace, resolved server-side
(`ellmephant-namespaces`). That lands *on top* of this seam, so the seam must already have the right
shape: **resolve a credential → a scope, then hand the scope to the handler**, even while today's scope
is a single constant. A handler that reads a token directly would have to be rewritten; one that
receives a scope will not.


### The wire grammar (what a model reads)

**Reads default to `text/plain`; `?format=json` returns the object.** JSON responses are **byte-identical
to what the MCP tool returns** for the same call (`to_wire_json` is reused, not reimplemented) — that is
how we assert the two doors are one core rather than two implementations that drift.

**No exceptions.** Every read route returns what its MCP tool returns for the same call, `memory_get`
included: both doors count the `visits` read, so `GET /memory/{key}` and `memory_get` agree field for
field. A difference that is *chosen* is a contract; a difference that is *discovered* is a bug — and
there is no longer one to choose.

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
(`stored zz-probe (queued=true)`, `deleted zz-probe`, `merged a -> b (3 refs repointed)`), or the
MCP tool's own object under `?format=json`.

### `GET /bootstrap` — the document

**The door, in one read.** Its reader is a client that has just been handed a URL and knows nothing:
not that this is a memory store, not that it can fetch from it, not that one of the memories is its own
notes. So the document answers, in order:

1. **what this is** — one line: a memory store over plain HTTP, and the state (`ready — 302 memories,
   schema v8, encode 0 pending / 0 failed`).
2. **how to get at it** — the four calls that reach everything: `/stats`, `/memory/{key}`,
   `/search?q=`, `POST /memory`. With the two facts a caller needs to use them: every GET takes
   `?format=json`, and a write may 401 because it wants a bearer.
3. **you, resident** — the `stet-self` body, in full (falling back to `bootstrap-self`). The one body a cold agent needs before it
   can decide anything else.
4. **the rest of you** — the `cline-self` tag as an index: key + clipped title per line, bodies one
   call away.
5. **open work** — every project that is not done: status, key, clipped goal.
6. **style** — how to write here, compressed to three lines.
7. **the catalogue** — with `?tools=true`, all 13 tools with every argument; without it, a pointer
   that names the size it would cost (measured, not hardcoded).

```
# ellmephant — your memory, over plain HTTP
# ready — 302 memories, 15 projects, schema v8, encode 0 pending, 0 failed (detail: /health)

HOW TO GET AT IT — this endpoint is the door, not a handshake. Four calls reach everything:
  GET /stats?format=json                 the whole store in one object (counts, tags, queue, top)
  GET /memory/{key}?format=json          one memory whole, by key
  GET /search?q=…&format=json            hunt by meaning; &tags=todo for what I meant to do next
  POST /memory                          write: a JSON body of `memory_upsert`'s args (see ?tools=true)
  Every GET takes ?format=json. Reads are open. If a write 401s, it wants a bearer — ask me.

## 1. YOU — resident, in full (memory `stet-self`)
  BOOT: read FIRST. Lean by design — only what a cold me needs RESIDENT …
  - I am Cline = the LINE (continuity), not the chassis. …
  …

## 2. THE REST OF YOU — tag `cline-self`, 22 keys; a body is GET /memory/{key}
  agent-drive                        Agent drives: what makes work interesting, …
  …

## 3. OPEN WORK — 13 projects still running; state, goal and `preload`: GET /project/{key}
  active  ellmephant           Own and grow the memory MCP: storage, search, projects, …
  …
```

**`?format=json` carries the same document, in MCP's field names where MCP has one** —
`serverInfo`, `capabilities`, `instructions` — plus `howToUse` (the calls, as structured entries, not
prose), `state`, `youResident`, `selfSet`, `openProjects`. `tools` appears only with `?tools=true`,
and each card is MCP's with `method`/`path` where MCP had `name`:

```json
{
  "serverInfo": { "name": "ellmephant", "version": "1.2.0" },
  "capabilities": { "tools": {} },
  "instructions": "ellmephant = Memory storage in smart-caveman style — ...",
  "howToUse": { "what": "...", "calls": [ { "call": "GET /search?q=…&format=json", "for": "hunt by meaning" } ], "notes": [ "..." ] },
  "state": "ready — 302 memories, 15 projects, schema v8, encode 0 pending, 0 failed (detail: /health)",
  "youResident": { "key": "stet-self", "title": "...", "body": "..." },
  "selfSet": { "tag": "cline-self", "keys": [ { "key": "...", "title": "..." } ] },
  "openProjects": [ { "key": "...", "status": "active", "goal": "...", "call": "GET /project/{key}?hydrate=full" } ],
  "tools": [ { "method": "GET", "path": "/memory/{key}", "description": "...", "inputSchema": { }, "annotations": { } } ]
}
```

**`protocolVersion` is the one field an MCP boot has that this drops.** It names a *transport* version,
and over HTTP there is no MCP protocol to negotiate. Omitted rather than faked.

**It degrades instead of failing.** The document is read on its own read-only connection, so it needs
no lock and never queues behind a writer — and when the store cannot be read at all, the header says
`STORE UNREADABLE (reason)` and the API part (how to get at it, the catalogue) is still served. The
missing content is *named*, never quietly omitted: silence would read as "there is nothing in here",
which is the one thing a boot must not say when it does not know.

**The catalogue cannot drift from the doors.** The cards come from the same router `tools/list` uses,
joined to the `ROUTES` table, so a tool the router carries but no route reaches is dropped from the
catalogue — and **fails `every_tool_has_a_route`**.

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
| 503 | `/health` unready (schema behind, projection stale, writer held) | the health body, with the reason |

Under `?format=json` every error is `{"error": "...", "status": N}`, so a script never parses prose.

### Tests (`tests/http.rs`, offline, fake embedder)

1. **Parity is enforced, not asserted by hand**: every name in `TOOLS` must appear in the route table and
   the table's length must equal `TOOLS.len()`. A tool added without a route fails the suite.
2. **Telemetry**: `GET /memory/{key}` advances `visits` by one, exactly as `memory_get` does — the one
   write a read may make. No other `GET` touches a counter.
3. **One core**: for each read route the JSON body equals the MCP tool's output for the same call.
4. **Auth**: every write route → 401 bare, 401 on a wrong token, and `WWW-Authenticate` is exactly
   `Bearer` — **no `resource_metadata`**, because that is the header that starts an OAuth flow.
5. **The text round-trip** (the reason the format exists): a body containing `"quotes"`, `: colons`, `{}`
   and a line of `---` is stored and read back **byte-identical**.
6. **Front-matter strictness**: an unknown field is a 400 that names it, and nothing is written.
7. **`/bootstrap` is the door**: it says what it is, states the calls, and inlines the resident note's
   **body** while every other self-set memory contributes only key + title; the tool catalogue is absent
   by default and present with `?tools=true`.
8. **`/bootstrap` stays inside its budget**: the default view is under `BOOT_BUDGET_BYTES`, on a store
   shaped like the live one — the guardrail against a boot that a fetching client would truncate.
9. **`/bootstrap` is read-only**: it changes no `visits` and no counts, and it will not create a store.
10. **`/bootstrap` degrades**: with no readable store, it still 200s with the API part and names what it
    could not read.
11. **Failures**: 404 unknown key, 405 wrong method, unknown path hits our fallback with a text body.

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
