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

Idempotent, safe to re-run, and the only thing it touches is `pen.parent_id`:

```sql
-- estate is the root and keeps parent_id NULL.
UPDATE pen SET parent_id = (SELECT id FROM pen WHERE name = 'estate') WHERE name = 'james';
UPDATE pen SET parent_id = (SELECT id FROM pen WHERE name = 'james')  WHERE name IN ('stet', 'cline');
```

Then check, which should read **`1 root(s)`**:

```console
$ ellmephant --home /data doctor | grep pens
  pens           5 pen(s), 1 root(s)
```

## `anonymous` is never a pen

`anonymous` means **no currently existing pen wrote this** (James, 2026-10-23). It is a statement
about the REGISTRY, not about the writing: the row may have been written by nobody at all, or by a pen
that no longer exists — the store cannot tell, and neither should a reader pretend to. It is a value,
never an address: a pen owns rows, answers for them and can be retired, and a pen called `anonymous`
would make every "unattributed" count a lie. Migration `0021` excludes it from the seed, and `0025` repairs any store that ran the earlier
version — un-owning what the ghost had (the rows become unattributed, i.e. claimable), and deleting
its sessions, policies and ledger references rather than pretending to reassign them.

What those rows want is an **owner to claim them** (`POST /memory/{key}/adopt` style), not a
migration that invents an answerer.

## Why this is written down

The pre-roll rehearsal on 2026-10-23 migrated a copy of the live store (918 memories, v20 → v25). It
found both of these by looking: 249 rows had been handed to a pen called `anonymous`, and the "tree"
was six roots with no edges. Neither would have failed a test; both would have shipped silently, and
the second would have made every ancestry rule a no-op on a store that reported itself healthy.
