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
| `POST /memory/{key}/upvote`, `PUT /memory/{key}/coherent`, `POST /project` | writes | **implemented** |
| `POST /reembed` | the heaviest op; last, because it holds the store lock | **implemented** |

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
| GET | `/bootstrap` | *(new)* | `?format=json`, `?hydrate=keys\|snippet\|full` |
| GET | `/stats` | `memory_stats` | `?tags=true` |
| GET | `/memory/{key}` | `memory_get` | counts a `visits` read, exactly as MCP does |
| GET | `/memory/{key}/backlinks` | `memory_backlinks` | — |
| GET | `/search` | `memory_search` | `?q=&k=&max_distance=&expand=&min_chars=&max_chars=&coherent=&fields=a,b&tag=x&tag=y` |
| GET | `/dupes` | `memory_dupes` | `?max_distance=&limit=` |
| GET | `/projects` | `project_list` | `?status=&pattern=&mode=&tag=` |
| GET | `/project/{key}` | `project_get` | `?hydrate=none\|snippet\|full` |

Writes — **auth required**, all of them:

| method | path | maps to | body / parameters |
| --- | --- | --- | --- |
| POST | `/memory` | `memory_upsert` | text front-matter, or JSON if `Content-Type: application/json` |
| POST | `/memory/{key}/upvote` | `memory_upvote` | — |
| POST | `/memory/{key}/merge` | `memory_merge` | `?into=<key>` |
| PUT | `/memory/{key}/coherent` | `memory_coherent` | `?value=true\|false\|null` |
| DELETE | `/memory/{key}` | `memory_delete` | — |
| POST | `/project` | `project_upsert` | JSON — a project patch is structured (owner / preload / tags) and front-matter has no grammar for it |
| POST | `/reembed` | `memory_reembed` | — **last**: heaviest lock holder, and `/health` watches it |

**Body formats, by Content-Type.** A memory body is *prose*, so `text/plain` front-matter wins there —
that is the format that removes JSON-escaping of multi-line bodies, which is where a hand-written call
actually fails. A project patch is *structured* (owner list, preload list, tags), where JSON is the
honest shape. So both are accepted, and the caller picks by `Content-Type`. `?format=` governs the
*response* only.

### Auth: open until a token is set, over a seam that is already the right shape

One bearer token, read from `ELLMEPHANT_WRITE_TOKEN` (env; a k8s Secret in the deployment). A **wrong**
token → `401` with `WWW-Authenticate: Bearer`, **no** `resource_metadata`, and a one-line text body.

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
(`stored zz-probe (reembedded=true)`, `deleted zz-probe`, `merged a -> b (3 refs repointed)`), or the
MCP tool's own object under `?format=json`.

### `GET /bootstrap` — the document

**The MCP boot, with HTTP verbs.** An MCP client is handed `initialize` (`serverInfo`,
`capabilities`, `instructions`) and then `tools/list` (the tool cards). This is those two merged into
one read, with `method` + `path` where MCP carries `name`. **Nothing more, nothing less** — no counts,
no resident set, no route index. The unit being replicated is the *API surface*; a boot that also
carries content is a boot you cannot trust to describe the same API.

```
BOOT ellmephant 1.2.0   capabilities: tools

INSTRUCTIONS
  ellmephant = Memory storage in smart-caveman style — big words in, small words out; ...

TOOLS — 14; HTTP verbs in place of MCP verbs, arguments and descriptions unchanged

  GET /memory/{key}/backlinks
      List memories whose body point at `key` with `[[…]]`.
      args: key: string (required) — Memory key. Slug.

  PUT /memory/{key}/coherent
      Record a memory's split-review verdict: `true` = one coherent topic, — ...
      args: coherent: boolean (optional) — true = one topic; false = a grab-bag; null = unjudged.; key: string (required) — Memory key. Slug.

  DELETE /memory/{key}
      Kill one memory by `key`. No undo. Gone. — ...
      args: key: string (required) — Memory key. Slug.

  ... (14, alphabetical by tool — the same order `tools/list` uses)
```

`?format=json` is the same document in MCP's own shape, so a client moves over by reading
`method`/`path` where it read `name`:

```json
{
  "serverInfo": { "name": "ellmephant", "version": "1.2.0" },
  "capabilities": { "tools": {} },
  "instructions": "ellmephant = Memory storage in smart-caveman style — ...",
  "tools": [
    {
      "method": "GET",
      "path": "/memory/{key}",
      "description": "Pull one memory whole by `key` — ...",
      "inputSchema": { "$schema": "...", "properties": { "key": { "type": "string" } }, "required": ["key"], "type": "object" },
      "annotations": { "title": "Get memory", "readOnlyHint": true, "destructiveHint": false, "idempotentHint": true, "openWorldHint": false }
    }
  ]
}
```

The fields are MCP's, verbatim — `inputSchema` and `annotations` included, `$defs` and all. (MCP
*omits* a `title` it does not have rather than sending `""`, so the cards have no top-level `title`
either; the human name rides `annotations.title`, where rmcp puts it.)

**`protocolVersion` is the one field an MCP boot has that this drops.** It names a *transport* version,
and over HTTP there is no MCP protocol to negotiate. Omitted rather than faked.

**Where the content went.** The resident set this document used to carry — the `cline-self` keys, the
boot note, todos, active projects, hot — is reachable with the commands above, exactly as it is over
MCP: `GET /memory/bootstrap-self`, `GET /search?tag=cline-self`, `GET /search?tag=todo`,
`GET /projects?status=active`, `GET /stats`. Readiness is `/health`'s job, and `/health` is the only
surface that reports it. **A boot is not a status page.**

**It needs no store.** The document is built from code — `mcp::instructions` and `mcp::tool_catalog`
joined to the `ROUTES` table — so it reads no database and takes no lock. A boot still describes the
API when the store is missing, stalled, or behind a writer, which is exactly when a caller needs to
know what it may ask for. That is also why it cannot drift: the cards come from the same router
`tools/list` uses, and a tool the router carries but no route reaches is dropped from the boot and
**fails `every_tool_has_a_route`**.

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
2. **Telemetry**: `GET /memory/{key}` advances `visits` by one, exactly as `memory_get` does — the one
   write a read may make. No other `GET` touches a counter.
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
