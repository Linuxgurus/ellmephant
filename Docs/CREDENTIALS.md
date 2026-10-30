# Credentials: a pen's secret is a row

A pen proves who it is with `X-Stet-From` (the name) and `X-Stet-Auth` (the secret). Until
2026-10-30 that proof lived in a process variable — `ELLMEPHANT_IDENTITIES`, `name:secret` pairs,
re-read on every call. It is now a table, `pen_credential`, owned by `pen`.

## Why it moved

The old table and the pen registry could disagree, and did:

- `twine` had a `pen` row and no secret — standing, but nothing could prove it.
- `estate` had a row and must never have one: it is a chair, used only by borrowing.
- A **retired** pen kept its secret, because the authentication path never looked at `ceased_at`.
  Retiring a pen removed its rows and its policy, but its credential still opened the door.
- Rotating one secret meant editing a Kubernetes Secret and restarting every consumer.

A credential belongs to a pen, so it belongs in the pen's store: revocable by retirement, replaceable
in place, and countable without parsing a string.

## The table

```sql
CREATE TABLE pen_credential (
    pen_id     INTEGER PRIMARY KEY REFERENCES pen(id) ON DELETE CASCADE,
    hash       TEXT NOT NULL,              -- lowercase hex sha256 of the 32-byte secret
    issued_at  TEXT NOT NULL DEFAULT (datetime('now')),
    issued_by  INTEGER REFERENCES pen(id), -- the minter, or NULL for provisioning / import
    rotated_at TEXT                        -- NULL until the first rotation
);
```

★ **The plaintext is never stored.** `hash` is SHA-256 over a 32-byte random token. Entropy is the
defence, so no password KDF is used: the comparison path is per-request, and argon2 there would be a
denial-of-service surface rather than a security win.

★ **A missing row is an honest state**, not a bug: `estate`, and every name a migration seeded from
history that has stopped writing.

★ **Why not columns on `pen`:** `SELECT * FROM pen` is the obvious debugging query. Auth material must
not ride along into a log line or a paste.

## Issuing and rotating

| verb | who | what it does |
|---|---|---|
| `pen_mint` (MCP) / `POST /pen/mint` | a proven pen | creates a pen UNDER the caller and returns its **secret once** |
| `pen_rotate` (MCP) / `POST /pen/rotate` | self or an ancestor | replaces a secret and returns the new one **once** |
| `ellmephant pen mint NAME --parent P` | provisioning | same, from the CLI |
| `ellmephant pen rotate NAME` | provisioning | same, from the CLI |

Minting is downward only: the parent is the caller, never an argument. A pen may rotate itself or a
descendant — never a peer and never an ancestor, because authority is granted downward and borrowed
only by policy.

★ **The minter knows the child's secret until the child rotates.** That is inherent in "whoever
creates a credential sees it". `pen_rotate` is the remedy: a rotation makes the parent's copy dead on
the next call. There is no read-back verb; a lost secret is recovered by rotating again.

★ **A minted secret is a second door past the lend rule.** `may_act_as` requires a live lend to
write `as:` a descendant. But a minter holds the child's plaintext, so it can authenticate *as* the
child directly — the child's name on the proof, no lend at all. That is inherent in "mint returns a
secret", not a defect in lending: revoking a lend does not revoke the copy the minter was given. The
remedy is the same rotation. Read the lend rule as a guarantee about the `as:` path, not as a
guarantee that authority cannot be exercised.

★ **The root never takes a credential.** `issue` and `rotate` refuse a pen whose `parent_id IS NULL`,
symmetric with `retire`: the root is where identities borrow from, never a party that acts. The
refusal is a sentence, not a `NULL` that a config happened to omit. The one-shot import follows the
same rule: a name with no pen row is attached **under** the existing root (so it keeps working and
stays retirable), and the root itself is skipped with a warning. There is never a second root.

★ **No cache, exactly like sudo.** Every authenticated call reads the row, so a rotation or a
retirement takes effect on the very next call.

## Retirement

`pen retire` sets `ceased_at` and deletes the credential row. Authentication joins `pen` and requires
`ceased_at IS NULL`, so even a credential left in a config somewhere stops working at that moment.

## The one-shot import

An existing store keeps working across the upgrade: on open, `ELLMEPHANT_IDENTITIES` is hashed into
`pen_credential` once, and the `meta` marker `identities_imported` is written. Later boots skip it.

★ **The marker is what stops a backdoor.** Without it, every boot would re-create a credential an
ancestor deliberately deleted. If you remove the env var and later set it again, nothing is imported.

The intended end state is that `ELLMEPHANT_IDENTITIES` is deleted from the deployment after one boot.
A fresh store has no credentials; provisioning issues the first one:

```console
$ kubectl -n agent-stet exec deploy/stet-memory -- \
      ellmephant pen rotate James
```

## What stays in the environment

These are seeds into tables, not the authority itself, and are unaffected by this change:

- `ELLMEPHANT_SUDOERS` → `sudoers` (read at boot, never fatal)
- `ELLMEPHANT_PEN_TREE` → `pen.parent_id` (a seed only; the table wins on disagreement)
- `ELLMEPHANT_LEASERS` → **still env-only.** It gates who may take a lease; the natural next step is
  a column or capability table, not part of this change.
