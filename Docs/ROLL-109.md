# Rolling the pen/owner/sudo set onto the live store (v20 → v26)

Written after doing every step once on a copy. The numbers are from that rehearsal, not from hope.

## What is running now

| | |
|---|---|
| live store | image `sha-8efea40` (#113), **schema v20**, 934 memories |
| `dev` | **v26** (pen registry, owner, sudo, read audit) |
| the gap | six migrations: 0021–0026 |
| served at | `https://stet.vn.linuxguru.net/mcp` — **one store, two pens** (cline and James) |

## 1. Rehearse on a copy (done 2026-10-23, repeat if `dev` moved)

```console
POD=$(kubectl -n agent-stet get pods -l app.kubernetes.io/name=ellmephant -o jsonpath='{.items[0].metadata.name}')
kubectl -n agent-stet cp "$POD:/data/snapshot/memory.db" /tmp/canary/memory.db -c ellmephant
kubectl -n agent-stet cp "$POD:/data/config.toml"     /tmp/canary/config.toml -c ellmephant
ellmephant --home /tmp/canary --offline doctor        # migrates the COPY, v20 -> v26
sqlite3 /tmp/canary/memory.db "PRAGMA integrity_check;"
```

**Expect** (measured 2026-10-23 20:58Z): `memories 934 · links 2587 · mail 497` unchanged, `private_read 0`,
`active_sudo 0`, `pen 5`, **no pen called `anonymous`**, **252 rows ownerless**, integrity `ok`.
A version count that GROWS is expected — 0022 snapshots every row once.

## 2. Attach the tree (provisioning — do this at or before the roll)

The seed gives names, not standing, so a fresh roll lands **five roots** and says so:

```
pens           5 pen(s), 5 root(s)   <- NOT ATTACHED: seeded names, no parents; ancestry reaches nothing
```

```sql
UPDATE pen SET parent_id = (SELECT id FROM pen WHERE name = 'estate') WHERE name = 'james';
UPDATE pen SET parent_id = (SELECT id FROM pen WHERE name = 'james')  WHERE name IN ('stet', 'cline');
```

Then `doctor` must read **`1 root(s)`**. Until it does, ancestor read, the ancestor write rule and
`whois` all reach nothing while the store reports itself ready. See `Docs/PEN-TREE.md`.

## 3. Roll

Image tag = `dev`'s head; roll the `agent-stet` stateful workload, then confirm:

```console
curl -s https://stet.vn.linuxguru.net/health?format=json | jq '.schema,.ready'
```

`ready: true` and `schema: 26`. **This store is shared** — rolling it moves cline's and James's rows at
once, which is why the rehearsal exists.

## 4. Bake — what to watch, not merely "wait"

`~/bin/bake-probe` encodes the five signs of a bad bake (errors on commands, mixed identity,
corruption, confusion, errors in various places). Baseline it BEFORE the roll; a delta is the signal.
It also fails if the pen tree has more than one root.

**The class of thing a bad bake looks like is a NEW REFUSAL**, because three land with this roll:

1. `memory_delete` by a pen that is neither the owner nor an ancestor;
2. `memory_upsert as: <descendant>` without a live lend;
3. an HTTP delete of an owned row with no proven pen.

And the quieter changes: 669 rows acquire owners, so writes by anyone else start being refused;
HTTP writes now stamp `asserted_by` instead of landing in the commons; ancestor read means a pen's
searches return **more**.

## 5. Twine last

`stet/estate-twine` is Stet's, at **v14**, and the jump is ten migrations. Same procedure: copy, migrate
the copy, compare counts, then his store — with his word, not on a schedule I invented.

## The one thing not in this document

`ELLMEPHANT_SUDOERS` has no ConfigMap key yet, so no policy exists and **nobody can `sudo` anything** in
production. That is deliberate — policy is the trust root and does not belong behind a bearer token —
but it means the sudo verbs are inert until provisioning writes a policy.
