# The pen tree: two doors, and the step nobody owns yet

A pen is a row in `pen`. **`estate` is the root**; every other pen hangs below it, and the ancestry
rules (ancestor read, ancestor write, adopt/disclaim by an ancestor, `whois`) only mean anything once
the edges exist.

## Two doors, two acts

| door | who | how | what it can say |
|---|---|---|---|
| **runtime** | a running pen | `pen_mint` (MCP) / `POST /pen/mint` | "here is a pen under me" — parent is the CALLER |
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
