# Borrowed authority: sudo, lend, pass, release

`estate` is a chair, not a person: a pen row with no identity and no credential, and none coming. Its
reach is therefore only ever a **borrow** — somebody else's pen wearing it for a while. That is what the
sudo model is for, and this page is the surface, because a capability nobody can find is a capability
nobody can use (or audit).

## Two verbs, two directions

| verb | direction | needs | means |
|---|---|---|---|
| `lend` | **down** a branch | parentage only | a parent spends its own authority on a descendant |
| `sudo` | **across or up** | a `sudoers` policy row | a pen borrows authority it does not have |

A lend needs no policy because parentage already grants it: requiring a row would mean every minted
finger needs a configuration entry written by code. A borrow needs a policy because it reaches for
authority the tree does not give.

Both produce a **session**, and a session is the whole point: it **expires**, it is **recorded**, and one
row **revokes** it. `lend` and `sudo` both **replace** on re-invocation rather than stacking, so a
shortened grant cannot reappear as the longer one it replaced.

## The doors

The `weak` side is **never an argument**: it is the proven pen on the connection. A pen may borrow for
itself, lend to its own descendants, pass what it holds downward, and release what it holds.

| MCP tool | HTTP | body | who |
|---|---|---|---|
| `pen_lend` | `POST /pen/lend` | `{weak, secs}` | the caller lends to a descendant |
| `sudo` | `POST /sudo` | `{powerful, secs}` | the caller borrows, under a policy |
| `sudo_pass` | `POST /sudo/pass` | `{powerful, pen, secs}` | the caller passes a session to a descendant |
| `sudo_release` | `POST /sudo/release` | `{powerful, pen?}` | end your own (`pen` omitted) or a descendant's |
| `sudo_sessions` | `GET /sudo/sessions?pen=` | — | who holds what, and until when |

```console
$ curl -sX POST https://stet.vn.linuxguru.net/sudo \
    -H "X-Stet-From: James" -H "X-Stet-Auth: $KEY" \
    -H 'Content-Type: application/json' -d '{"powerful":"estate","secs":300}'
borrowed estate as James until 2026-10-24 05:01:07

$ curl -s https://stet.vn.linuxguru.net/sudo/sessions -H "X-Stet-From: James" -H "X-Stet-Auth: $KEY"
James holds estate (policy) until 2026-10-24 05:01:07
```

## What a session actually buys

Authority is `ancestry ∪ held sessions`, resolved **at use**:

- a pen holding `P` may read and write what `P` reaches, for as long as the row says so;
- a **lend** is re-checked against the tree on every use — if the parentage that justified it moves
  (an adoption, a disclaim, a repair), it stands down with a warning rather than granting reach with no
  reason. A policy borrow is not re-checked: a `sudoers` ceiling never depended on the tree;
- a released, expired or revoked session stops answering on the **next** call — nothing is cached;
- `private_read` records a borrowed read as **`borrowed`**, distinct from `subtree`, because "an
  ancestor read down its tree" and "a pen spent a session it was lent" are different acts.

## What is deliberately NOT at the door

`grant` and `revoke` — the **policy** itself — are provisioning, not conversation: whoever can write a
`sudoers` row can hand out authority, and that is the trust root. They live in the CLI (`ellmephant pen
grant|revoke|policies`) and in configuration (`ELLMEPHANT_SUDOERS="powerful>weak:seconds"`), never on a
live pen's connection.

★ **A policy is not a licence.** It is a ceiling on a borrow. A policy whose borrower and beneficiary are
the same pen is a self-issued credential, which is why no `estate>cline` line exists: if an agent is to
hold the root, it should arrive by a human's word and a session that expires, not by a declarative
default.
