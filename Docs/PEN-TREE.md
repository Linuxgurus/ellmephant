# The pen tree: two doors, and the step nobody owns yet

A pen is a row in `pen`. **`estate` is the root**; every other pen hangs below it, and the ancestry
rules (ancestor read, ancestor write, adopt/disclaim by an ancestor, `whois`) only mean anything once
the edges exist.

## Two doors, two acts

| door | who | how | what it can say |
|---|---|---|---|
| **runtime** | a running pen | `pen_mint` (MCP) / `POST /pen/mint` | "here is a pen under me" — parent is the CALLER, and its secret is returned once ([[CREDENTIALS]]) |
| **provisioning** | whoever can write the database | SQL, terraform, a ConfigMap | "here is the standing of a pen" — kind, parent, and that it exists at all |

The seed in migration `0021` takes **names from history** (`memory.asserted_by`, `mail.asserted_by`)
and nothing else. History knows a name existed; it cannot know that pen's *standing*. So **every
seeded pen arrives as a root** — `james` is a root, `stet` is a root — and until somebody attaches
them, `estate` has no descendants and a parent-less forest is what the store will report:

```
pens           5 pen(s), 5 root(s)   <- NOT ATTACHED: seeded names, no parents; ancestry reaches nothing
```

That line is `ellmephant doctor`'s way of saying the tree is a list. **One root is the invariant.**

## Attaching the tree (provisioning)

**Standing lives in the `pen` table.** Nothing else is authoritative for it — not a ConfigMap, not a
manifest, not this document. A pen's `parent_id` is a fact about that pen, and facts belong in the row,
for the same reason ownership does: the row answers, the configuration only admits.

That matters most at the moment nobody is watching. If provisioning re-declared the tree on every
boot, then a deliberate act — a pen re-parented, a branch adopted or disclaimed — would be silently
undone by the next restart, and "who answers for this?" would be a question answered by whichever
deployment file was applied last.

### The seed (bootstrap only)

A brand-new store's pens are NAMES, and the attach has no door: the image carries no `sqlite3`, the PVC
cannot be mounted beside a running brain, and no API sets a parent. So the first attach may arrive as
configuration:

```
ELLMEPHANT_PEN_TREE="James=estate,stet=James,cline=James,hands=stet"
```

It is a **seed, not a declaration**, and the difference is enforceable, not stylistic:

| state of the child | what the seed does |
|---|---|
| `parent_id IS NULL` | attaches it — this is what a seed is for |
| already points at the seed's parent | agrees, writes nothing |
| points somewhere else | **stands down and logs the disagreement by name** — the table wins |
| unknown child, unknown parent, itself, a loop, a retired pen | refused by name; the rest still apply |

An entry that disagrees is drift worth *seeing*, never worth obeying. To change standing on purpose,
use `set_parent` — the overwriting act — knowingly.

### Doing it by hand

Or with SQL, when you have the database open and mean it:

```sql
-- estate is the root and keeps parent_id NULL.
UPDATE pen SET parent_id = (SELECT id FROM pen WHERE name = 'estate') WHERE name = 'James';
UPDATE pen SET parent_id = (SELECT id FROM pen WHERE name = 'James')  WHERE name IN ('stet', 'cline');
```

Then check, which should read **`1 root(s)`**:

```console
$ ellmephant --home /data doctor | grep pens
  pens           5 pen(s), 1 root(s)
```

## Who may read a row: the audience grammar

A row's `audience` is a **list of addresses**. Two kinds, and the difference between them is the whole
point:

| address | reaches |
|---|---|
| `pen:<name>` | **exactly that pen** — nobody above it, nobody below it |
| `subtree:<name>` | that pen **and everything below it** |

Comma-separated, at most eight addresses, duplicates refused, every name must exist in the registry.
`public` is the unset case. Anything else is refused **at write time**: an audience no reader can parse
would be a row nobody can see, and failing closed silently is the worst direction.

```console
# "my tax papers: me and Cline, nobody else"
audience: "pen:james,pen:cline"

# "everything under Stet's branch"
audience: "subtree:stet"
```

### What changed on 2026-10-24

`pen:` used to mean *"that pen and every pen above it"* — parentage was the read grant. James removed
that: **a pen above you does not read your private rows by being above you.** If an ancestor should
read a row, the row says so — `subtree:<the ancestor>` — or the ancestor names itself. The reasons are
the same ones that made `private` worth having: a boundary you did not ask for is not a boundary you
chose.

Consequences worth knowing before you write one:

- **A finger's private note is the finger's.** Its hand cannot read it unless the row says
  `subtree:<hand>`. That follows from `pen:` being exact, and it is the reason an agent that wants its
  mechanism's work readable should address the row to the branch.
- **The writer is not special.** A pen that creates a row addressed to somebody else cannot read it
  either. That is a share, and a share is one-way on purpose.
- **An ancestor may no longer publish what it cannot read.** `publish=true` releases a private row to
  the commons; releasing words you have never seen is a leak with extra steps, so it is refused unless
  the audience includes you.

### The root still sees everything

`estate` is the break-glass: a session to a parentless pen reads **every** row, whatever its audience,
and the read is recorded as `borrowed` rather than `subtree`. That is a deliberate, explicit bypass —
*"estate can always peek"* (James) — and it is why the honest sentence is *"private to us, and to
anyone the estate asks"*, not *"private"*.

Nothing here is encryption. The row is plaintext in the file, in the snapshot and in the off-cluster
copy, and it is embedded in a shared vector index: the tier protects against another **agent using the
tools**, and against nothing else.
