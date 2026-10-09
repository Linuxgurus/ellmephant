# REFACTOR.md — dupe-removal + layer cleanup (executable plan)

> **Status: applied** (steps 1–9). `cargo test --locked`, `cargo fmt --all --check`
> and `cargo clippy --all-targets --all-features -- -D warnings` are all green.
> Kept as the record of *what* changed and *why*; each step below is still the
> recipe to reproduce or extend it.
>
> **Part 2** (steps 10–15, complexity reduction) follows at the end of this file.

Mechanical refactors only; **no behaviour change**. Every step is independently
verifiable. Target audience: a smaller model — do the steps in order, run the
verify command after each, and stop if anything fails.

## Ground rules

- Do **not** change public behaviour, output strings, SQL, or error messages.
- One step = one focused edit set. After each step run the *verify* command.
- `cargo test` is offline (no model, no network); it must stay green.
- Never edit `Cargo.toml` for these steps unless a step says so (it never does).
- Comments: keep them 1 line, terse. Match existing style (see `src/store/mod.rs`).

## Prereqs

```sh
cargo test --locked        # must be green before you start
cargo fmt --all            # do this at the very end, not mid-step
```

The verify command used throughout is:

```sh
cargo test --locked && cargo clippy --all-targets --all-features
```

CI sets `RUSTFLAGS=-D warnings`, so **any** warning fails. Watch for `dead_code`.

---

## Step 1 — one `hex()` (was duplicated)

**Problem:** lowercase hex is implemented twice, byte-for-byte:
`src/store/mod.rs` (`fn hex`, private) and `src/model/manifest.rs` (`fn hex`, pub).

**Do:**

1. Create `src/hex.rs`:

```rust
//! Lowercase hex of a byte slice — one home for the sha256 digests in `store` and `model`.

/// Encode `bytes` as lowercase hex.
pub fn encode(bytes: &[u8]) -> String {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        out.push(DIGITS[(b >> 4) as usize] as char);
        out.push(DIGITS[(b & 0x0f) as usize] as char);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn is_lowercase_and_padded() {
        assert_eq!(encode(&[0x00, 0x0f, 0xff]), "000fff");
        assert_eq!(encode(&[]), "");
    }
}
```

2. `src/lib.rs`: add `pub mod hex;` to the module list (alphabetical: after `error`).

3. `src/store/mod.rs`: delete the local `fn hex` block; in `row_hash` change
   `hex(&h.finalize())` → `crate::hex::encode(&h.finalize())`.

4. `src/model/manifest.rs`: delete the local `fn hex` block and its
   `hex_is_lowercase_and_padded` test; add near the top (so `manifest::hex`
   still exists for `sha256_file` and the module tests):
   `pub use crate::hex::encode as hex;`

Verify: `cargo test --locked`

---

## Step 2 — one `[[key]]` regex (was duplicated)

**Problem:** the wiki-link regex exists twice with the same key charset:
`src/store/links.rs` (with a capture group) and `src/embed/projection.rs` (no group).

**Do:**

1. `src/store/links.rs`: add a public strip helper next to `extract_links`:

```rust
/// Remove every `[[key]]` token from `s` (the embed projection needs this; SPEC §8).
pub fn strip_links(s: &str) -> String {
    LINK_RE.replace_all(s, "").to_string()
}
```

2. `src/embed/projection.rs`:
   - Delete `use std::sync::LazyLock;`, `use regex::Regex;` and the
     `static LINK_RE` block.
   - Replace the `strip_links` body so it delegates:

```rust
/// Remove every `[[key]]` token (owned by `store::links`; the syntax is a store concept).
pub fn strip_links(s: &str) -> String {
    crate::store::links::strip_links(s)
}
```

> This makes `embed` depend on `store`. Fine inside one crate (Rust allows
> mutually-referential modules); `store` already depends on `embed`.

Verify: `cargo test --locked`

---

## Step 3 — `Memory` draft via `Default` (kill the boilerplate)

**Problem:** the CLI (`src/cli/mod.rs`, `Command::Add` arm) and MCP
(`src/mcp/mod.rs`, `AddParams::into_memory`) both hand-write
`id: None, hash: String::new(), created: String::new(), updated: String::new()`.

**Do:**

1. `src/store/mod.rs`: add `Default` to the derive on `Memory`:

```rust
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Memory {
```

2. `src/cli/mod.rs`, `Command::Add` arm — replace the construction with:

```rust
            let mem = Memory {
                key: key.unwrap_or_default(),
                scope,
                type_: MemoryType::parse(&r#type),
                title,
                body: read_body(&body)?,
                source,
                tags,
                ..Default::default()
            };
```

3. `src/mcp/mod.rs`, `AddParams::into_memory`:

```rust
        Memory {
            key: self.key.unwrap_or_default(),
            scope: self.scope,
            type_: MemoryType::parse(self.type_.as_deref().unwrap_or("fact")),
            title: self.title,
            body: self.body,
            source: self.source,
            tags: self.tags,
            ..Default::default()
        }
```

4. (Optional) In the integration tests, replace the explicit empty-field tails
   in the per-file `memory(...)` helpers with `..Default::default()`
   (`tests/store.rs`, `search.rs`, `links.rs`, `maintain.rs`, `portable.rs`).

Verify: `cargo test --locked`


---

## Step 4 — `Spec::with_overrides` (3× copy-paste)

**Problem:** `src/cli/mod.rs::model_cmd` clones `spec` and patches `repo`/`revision`
in all three arms.

**Do:**

1. `src/model/mod.rs`, add to `impl Spec`:

```rust
    /// A copy with `repo`/`revision` overridden when supplied (the `model …` flags).
    pub fn with_overrides(&self, repo: Option<String>, revision: Option<String>) -> Self {
        let mut spec = self.clone();
        if let Some(repo) = repo {
            spec.repo = repo;
        }
        if let Some(revision) = revision {
            spec.revision = revision;
        }
        spec
    }
```

2. `src/cli/mod.rs::model_cmd`: in each arm replace the
   `let mut spec = spec.clone(); if let Some(repo) = repo {…}` block with
   `let spec = spec.with_overrides(repo, revision);` (the `Fetch` arm then also
   sets `spec.force = force;`).

Verify: `cargo test --locked`

---

## Step 5 — `meta` key constants

**Problem:** `"dim"`, `"model"`, `"instruction"` are literalled in
`src/index/vec.rs` and `src/store/maintain.rs`.

**Do:**

1. `src/db/meta.rs`: add

```rust
/// Vector dimensionality of the `vec0` index (frozen at `CREATE`).
pub const DIM: &str = "dim";
/// Embedder id the stored vectors correspond to.
pub const MODEL: &str = "model";
/// Query-side instruction the stored vectors were built with.
pub const INSTRUCTION: &str = "instruction";
```

2. `src/index/vec.rs`: in `stored_dim`, `ensure`, `recreate` replace `"dim"`
   with `meta::DIM` (the module already does `use crate::db::meta;`).

3. `src/store/maintain.rs`: replace `"dim"` → `meta::DIM`, `"model"` →
   `meta::MODEL`, `"instruction"` → `meta::INSTRUCTION`.

Verify: `cargo test --locked`

---

## Step 6 — move raw SQL out of the transport/CLI layers

**Problem:** SQL lives outside `store`: `src/cli/session.rs` (`tags`, `keys`) and
`src/mcp/mod.rs` (`backlinks`). Mirror the existing collect style
(`src/store/tags.rs::orphans`).

**Do:**

1. `src/store/tags.rs`: add

```rust
/// Every tag with its reference count, alphabetical.
pub fn list(conn: &Connection) -> Result<Vec<(String, i64)>> {
    let mut stmt = conn.prepare(
        "SELECT t.name, count(mt.memory_id)
           FROM tag t LEFT JOIN memory_tag mt ON mt.tag_id = t.id
          GROUP BY t.id ORDER BY t.name",
    )?;
    let rows = stmt.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?)))?;
    Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
}
```

2. `src/store/mod.rs`: add

```rust
/// Every key in the store, alphabetical — the REPL's tab-completion vocabulary.
pub fn keys(conn: &Connection) -> Result<Vec<String>> {
    let mut stmt = conn.prepare("SELECT key FROM memory ORDER BY key")?;
    let rows = stmt.query_map([], |r| r.get::<_, String>(0))?;
    Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
}
```

3. `src/store/links.rs`: move the `Backlink` struct and `backlinks` fn here from
   `src/mcp/mod.rs` (keep `#[derive(Debug, Serialize)]`, same SQL, same field order).

4. `src/cli/session.rs`: delegate the two methods:

```rust
    /// Every tag with its reference count, alphabetical.
    pub fn tags(&self) -> Result<Vec<(String, i64)>> {
        tags::list(&self.conn)
    }

    /// Every key in the store, alphabetical — the REPL's tab-completion vocabulary.
    pub fn keys(&self) -> Result<Vec<String>> {
        store::keys(&self.conn)
    }
```

5. `src/mcp/mod.rs`: delete the local `Backlink` struct and `backlinks` fn; in
   `memory_backlinks` call
   `self.with_store(|s| crate::store::links::backlinks(s.conn(), &p.key))`.
   Drop `use serde::Serialize;` only if nothing else needs it (check first).

Verify: `cargo test --locked`


---

## Step 7 — split `src/cli/mod.rs` (666 lines)

Move whole functions out; keep the clap tree, `run`, `execute`, `emit`,
`read_body` in `mod.rs`.

**Do:**

1. `src/cli/format.rs`: add the byte-humaniser (move `human` from `cli/mod.rs`):

```rust
/// Bytes -> a short human string (`1.2 GiB`).
pub fn human_bytes(bytes: u64) -> String {
    const UNITS: [&str; 4] = ["B", "KiB", "MiB", "GiB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit + 1 < UNITS.len() {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} B")
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}
```

2. Create `src/cli/model.rs`: `pub fn cmd(spec: &model::Spec, command: ModelCommand)
   -> Result<()>` (the current `model_cmd` body) + `print_model_status`, using
   `super::format::human_bytes` and `super::{ModelCommand, Error}` + `crate::model`.

3. Create `src/cli/doctor.rs`: `pub fn run(paths, cfg, spec, verify)` — the
   current `doctor` body verbatim, using `super::format::human_bytes`.

4. Create `src/cli/embed.rs`: `pub(crate) fn load_embedder` + `pub fn run(spec,
   cfg, text, query)` — the current `load_embedder` + `embed` bodies.

5. `src/cli/mod.rs`:
   - add `pub mod doctor; pub mod embed; pub mod model;` beside the existing
     `pub mod format; pub mod repl; pub mod session;`.
   - delete `load_embedder`, `embed`, `model_cmd`, `print_model_status`, `human`,
     `doctor`; add the imports the moved code needs are now in their files.
   - dispatch: `doctor::run(...)`, `model::cmd(...)`, `embed::run(...)`.
   - `session.rs`: `super::load_embedder(...)` → `super::embed::load_embedder(...)`.

Verify: `cargo test --locked`

---

## Step 8 — prune dead code

1. `src/error.rs`: delete the `NonUtf8Path(PathBuf)` variant and the now-unused
   `use std::path::PathBuf;` (grep first: `rg NonUtf8Path`).

2. `src/cli/format.rs`: `render_hits` is only ever called with `json = false` and
   `hits_to_json` is prod-unused. Simplify:
   - delete `hits_to_json`;
   - `pub fn render_hits(hits: &[Hit], json: bool)` → `fn render_hits(hits: &[Hit])
     -> Result<String>`, dropping the `if json` branch;
   - `render_bundle` calls `render_hits(&bundle.hits)?`;
   - fix the two tests (`json_roundtrips`, `empty_is_explicit`) to exercise
     `render_bundle(&Bundle { hits: vec![hit()], ..Default::default() }, true)`.

> Leave `Command::Bench` / `Error::NotImplemented` alone — that is P10.

Verify: `cargo test --locked`

---

## Step 9 — shared integration-test helpers

**Problem:** `temp_store()` (6×) and `count()` (4×) are copied across `tests/*.rs`.

**Do:**

1. Create `tests/common/mod.rs`:

```rust
//! Shared integration-test helpers. `#![allow(dead_code)]` because each test
//! binary compiles this module separately and uses only part of it.
#![allow(dead_code)]

use llmphant::config::Paths;
use rusqlite::Connection;
use tempfile::TempDir;

/// A throwaway store root plus its resolved [`Paths`].
pub fn temp_store() -> (TempDir, Paths) {
    let dir = tempfile::tempdir().expect("tempdir");
    let paths = Paths::from_root(dir.path());
    (dir, paths)
}

/// Shorthand for a one-row `SELECT count(...)`.
pub fn count(conn: &Connection, sql: &str) -> i64 {
    conn.query_row(sql, [], |r| r.get(0)).expect("count query")
}
```

2. In `tests/store.rs`, `search.rs`, `links.rs`, `maintain.rs`, `portable.rs`,
   `db.rs`: add `mod common;` after the doc comment; delete the local `temp_store`
   (and `count` where present); call `common::temp_store()` / `common::count(...)`.
   Do **not** touch the per-file `memory(...)` helpers (their signatures differ).

Verify: `cargo test --locked`

---

## Acceptance

```sh
cargo fmt --all
cargo test --locked
cargo clippy --all-targets --all-features   # must be warning-free
```

Definition of done:

- [x] `rg 'fn hex' src` is empty — the single implementation is
      `src/hex.rs::encode` (re-exported as `model::manifest::hex`).
- [x] `rg 'LINK_RE' src` shows only `src/store/links.rs`.
- [x] `rg NonUtf8Path` is empty.
- [x] `src/cli/mod.rs` shrank 666 → 418 lines (only the clap tree, `run`,
      `execute`, `emit`, `read_body` remain); `doctor`/`model`/`embed` live in
      their own files.
- [x] `Session`/`MemoryServer` contain no inline SQL (it moved into `store`).
- [x] `cargo test` green; `cargo clippy -D warnings` clean; `cargo fmt --check` clean.

### Bonus (not in the original plan)

- `src/index/vec.rs::from_blob` used `chunks_exact(4)`; rustc 1.99's clippy lint
  `chunks_exact_to_as_chunks` fails CI under `-D warnings`. Switched to
  `as_chunks::<4>()` (stable since 1.88). Pre-existing issue, behaviour-identical.


# Part 2 — cyclomatic-complexity reduction (steps 10–15)

> **Status: applied.** Same ground rules as Part 1: mechanical only, **no behaviour
> change** — identical output strings, SQL, error messages and public API. After each
> step run the Part 1 *verify* command.

## Method

Measured with two tools, cross-checked:

```sh
rust-code-analysis-cli -m -O json -p src -o /tmp/rca      # true cyclomatic, per function
cargo clippy --all-targets --all-features -- -W clippy::cognitive_complexity
# (with a local `clippy.toml`: cognitive-complexity-threshold = 1, deleted after)
```

`rust-code-analysis` counts every `?` and every closure body as a decision point, so
Rust cyclomatic numbers run high; clippy's cognitive score is nesting-weighted and is
the better readability signal. `clippy`'s default cognitive threshold (25) is not
reached anywhere in `src/`, so this is trimming, not rescue.

## Baseline (src/, 383 functions/closures)

median cyclomatic 1, mean 2.95, p90 7; **11** functions ≥ 15, **23** ≥ 10.

| cyclomatic | cognitive | sloc | location |
|---|---|---|---|
| 38 | 7 | 127 | `src/cli/mod.rs` `execute` |
| 33 | 9 | 90 | `src/cli/doctor.rs` `run` |
| 24 | 4 | 45 | `src/store/maintain.rs` `reembed` |
| 20 | 4 | 81 | `src/store/mod.rs` `add` |
| 17 | 13 | 46 | `src/model/mod.rs` `remove` |
| 17 | 8 | 61 | `src/search/mod.rs` `run` |
| 12 | 11 | 50 | `src/cli/model.rs` `cmd` (dup verify block) |
| 10 | 13 | 30 | `src/search/mod.rs` `expand` |

---

## Step 10 — one weight-verify reporter (was duplicated)

**Problem:** the `model::verify` report — the `ok` line, the `MISMATCH` loop and the
`N file(s) failed verification` error — is copy-pasted in `src/cli/doctor.rs::run` and
`src/cli/model.rs::cmd`. The two differ only in what a hashing *error* does: `doctor`
prints `skipped (…)` and carries on; `model status` propagates it.

**Do:**

1. `src/cli/model.rs`: add

```rust
/// Report a `model::verify` run (SPEC §6.3/§6.5): print the `ok` or `MISMATCH` lines,
/// then fail on any mismatch. `lenient` (doctor) turns a hashing error into a
/// `skipped` line; the strict caller (`model status`) propagates it.
pub(crate) fn verify_weights(spec: &model::Spec, lenient: bool) -> Result<()> {
    let bad = match model::verify(spec) {
        Ok(bad) => bad,
        Err(err) if lenient => {
            println!("  verify         skipped ({err})");
            return Ok(());
        }
        Err(err) => return Err(err),
    };
    if bad.is_empty() {
        println!("  verify         ok (all sha256 match)");
        return Ok(());
    }
    for line in &bad {
        println!("  verify         MISMATCH {line}");
    }
    Err(Error::Model(format!(
        "{} file(s) failed verification",
        bad.len()
    )))
}
```

2. `src/cli/model.rs::cmd`, the `Status` arm: replace the inline `if verify { … }` with

```rust
            if verify {
                verify_weights(&spec, false)?;
            }
```

3. `src/cli/doctor.rs::run`: replace the whole `if verify { match model::verify … }`
   block with `verify_weights(spec, true)?;` and add
   `use super::model::verify_weights;` (keep `use crate::model;` — different name).

Verify: `cargo test --locked`

---


## Step 11 — split `cli::doctor::run`

**Problem:** one function gathers stats, prints ~15 lines, then verifies — 90 sloc.

**Do:** keep `run` as the orchestrator and extract three printers (verbatim bodies):

```rust
pub fn run(paths: &Paths, cfg: &Config, spec: &model::Spec, verify: bool) -> Result<()> {
    paths.ensure()?;
    let conn = db::open(paths)?;
    let schema = db::migrate::version(&conn)?;
    let objects: i64 = conn.query_row(
        "SELECT count(*) FROM sqlite_master WHERE type IN ('table','view','trigger')",
        [],
        |r| r.get(0),
    )?;
    print_env(paths, cfg, schema, objects);
    print_weights(spec)?;
    print_index_health(&conn)?;
    if verify {
        verify_weights(spec, true)?;
    }
    Ok(())
}
```

- `fn print_env(paths: &Paths, cfg: &Config, schema: i64, objects: i64)` — the
  root/db/config/models/schema/objects/model lines.
- `fn print_weights(spec: &model::Spec) -> Result<()>` — the weights-cache summary.
- `fn print_index_health(conn: &Connection) -> Result<()>` — memories/vec0/links/tags.

Verify: `cargo test --locked`

---


## Step 12 — split `cli::execute` into per-command handlers

**Problem:** `execute` is a 15-arm match that also does the work and the printing
(127 sloc). Keep it the dispatcher; move each store command's body out.

**Do:**

1. In `src/cli/mod.rs`, add private handlers (all ≤ 3 args, so no
   `too_many_arguments`):

```rust
fn cmd_add(session: &mut Session, mem: &Memory) -> Result<()> {
    let key = session.add(mem)?;
    println!("added {key}");
    Ok(())
}

fn cmd_search(session: &mut Session, q: Query, json: bool) -> Result<()> {
    let bundle = session.search(&q)?;
    emit(format::render_bundle(&bundle, json)?);
    Ok(())
}

fn cmd_show(session: &Session, key: &str, json: bool) -> Result<()> {
    let mem = session.show(key)?;
    emit(format::render_memory(&mem, json)?);
    Ok(())
}

fn cmd_rm(session: &Session, key: &str) -> Result<()> {
    if session.rm(key)? {
        println!("removed {key}");
        Ok(())
    } else {
        Err(Error::NotFound(key.to_string()))
    }
}
```


```rust
fn cmd_tags(session: &Session, json: bool, command: Option<TagCommand>) -> Result<()> {
    match command {
        Some(TagCommand::Prune { dry_run }) => {
            if dry_run {
                let orphans = session.orphans()?.len();
                println!("{orphans} orphan tag(s) would be pruned");
            } else {
                let pruned = session.prune_tags()?;
                println!("pruned {pruned} orphan tag(s)");
            }
            Ok(())
        }
        None => {
            let tags = session.tags()?;
            emit(format::render_tags(&tags, json)?);
            Ok(())
        }
    }
}

fn cmd_rename(session: &Session, old: &str, new: &str) -> Result<()> {
    let outcome = session.rename(old, new)?;
    println!(
        "renamed {} -> {} ({} body/bodies rewritten, {} edge(s) repointed)",
        old, outcome.key, outcome.bodies, outcome.edges
    );
    Ok(())
}

fn cmd_reembed(session: &mut Session) -> Result<()> {
    let report = session.reembed()?;
    println!(
        "re-embedded {} row(s) at dim {}{} ({})",
        report.rows,
        report.dim,
        if report.rebuilt {
            "; vector index recreated"
        } else {
            ""
        },
        report.model
    );
    Ok(())
}

fn cmd_links(session: &Session, json: bool) -> Result<()> {
    let dangling = session.dangling()?;
    emit(format::render_dangling(&dangling, json)?);
    Ok(())
}

fn cmd_export(session: &Session, path: &Path) -> Result<()> {
    let count = session.export(path)?;
    println!("exported {count} memories to {}", path.display());
    Ok(())
}

fn cmd_import(session: &mut Session, path: &Path, dry_run: bool) -> Result<()> {
    let count = session.import(path, dry_run)?;
    if dry_run {
        println!("{count} memory change(s) pending from {}", path.display());
    } else {
        println!("imported {count} memory change(s) from {}", path.display());
    }
    Ok(())
}
```

> `Error::NotFound` takes a `String`: the `Rm` arm currently passes the moved `key`;
> the handler takes `&str` and does `key.to_string()` — same error value.


2. `execute` becomes just construction + dispatch (the `Query` build stays here so
   `cmd_search` needs no 8-arg signature):

```rust
pub fn execute(session: &mut Session, command: Command) -> Result<()> {
    match command {
        Command::Add {
            key, scope, title, body, r#type, source, tags,
        } => {
            let mem = Memory {
                key: key.unwrap_or_default(),
                scope,
                type_: MemoryType::parse(&r#type),
                title,
                body: read_body(&body)?,
                source,
                tags,
                ..Default::default()
            };
            cmd_add(session, &mem)
        }
        Command::Search {
            query, scope, k, mode, max_distance, json, expand,
        } => {
            let q = Query {
                text: query,
                scope,
                k: k.unwrap_or_else(|| session.default_k()),
                mode: Mode::parse(&mode)?,
                max_distance,
                expand,
            };
            cmd_search(session, q, json)
        }
        Command::Show { key, json } => cmd_show(session, &key, json),
        Command::Rm { key } => cmd_rm(session, &key),
        Command::Tags { json, command } => cmd_tags(session, json, command),
        Command::Rename { old, new } => cmd_rename(session, &old, &new),
        Command::Reembed => cmd_reembed(session),
        Command::Links { json } => cmd_links(session, json),
        Command::Export { path } => cmd_export(session, &path),
        Command::Import { path, dry_run } => cmd_import(session, &path, dry_run),
        Command::Repl
        | Command::Doctor { .. }
        | Command::Model { .. }
        | Command::Embed { .. }
        | Command::Mcp => Err(Error::Config("command is not available here".into())),
        Command::Bench { .. } => Err(Error::NotImplemented("bench (PLAN.md P10)")),
    }
}
```

Verify: `cargo test --locked`

---


## Step 13 — split `store::add` (update vs insert)

**Problem:** `add` inlines two large `params!` lists behind the
`Some(existing) => UPDATE / None => INSERT` arms.

**Do:** in `src/store/mod.rs`, bind `let model = embedder.model_id();` after computing
`hash`, then:

```rust
    let id = match existing {
        Some((id, _)) => update_row(&tx, id, mem, &hash, dim, model)?,
        None => insert_row(&tx, &key, mem, &hash, dim, model)?,
    };
```

and add the two helpers (SQL moved verbatim; `params!` borrows `mem`'s fields as before):

```rust
/// The `UPDATE` half of [`add`]: the key already exists.
fn update_row(
    tx: &Connection,
    id: i64,
    mem: &Memory,
    hash: &str,
    dim: i64,
    model: &str,
) -> Result<()> {
    tx.execute(
        "UPDATE memory SET scope = ?2, type = ?3, title = ?4, body = ?5, source = ?6,
                hash = ?7, model = ?8, dim = ?9,
                updated = COALESCE(NULLIF(?10, ''), datetime('now'))
          WHERE id = ?1",
        params![
            id,
            mem.scope,
            mem.type_.as_str(),
            mem.title,
            mem.body,
            mem.source,
            hash,
            model,
            dim,
            mem.updated
        ],
    )?;
    Ok(())
}

/// The `INSERT` half of [`add`]: a new key; returns its rowid.
fn insert_row(
    tx: &Connection,
    key: &str,
    mem: &Memory,
    hash: &str,
    dim: i64,
    model: &str,
) -> Result<i64> {
    tx.execute(
        "INSERT INTO memory (key, scope, type, title, body, source, hash, model, dim,
                             created, updated)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9,
                 COALESCE(NULLIF(?10, ''), datetime('now')),
                 COALESCE(NULLIF(?11, ''), datetime('now')))",
        params![
            key,
            mem.scope,
            mem.type_.as_str(),
            mem.title,
            mem.body,
            mem.source,
            hash,
            model,
            dim,
            mem.created,
            mem.updated
        ],
    )?;
    Ok(tx.last_insert_rowid())
}
```

Verify: `cargo test --locked`

---


## Step 14 — split `model::remove` (all vs one revision)

**Problem:** `remove` handles both the "drop every revision" and the "resolve one
revision from refs" paths; highest cognitive score in `src/` (13).

**Do:** in `src/model/mod.rs` keep `remove` as the guard + chooser and extract the two
halves verbatim:

```rust
pub fn remove(spec: &Spec, all: bool) -> Result<usize> {
    if spec.model_dir.is_some() {
        return Err(Error::Model("refusing to remove a --model-dir".into()));
    }
    let repo_dir = cache::repo_dir(&spec.cache_root, &spec.repo);
    if !repo_dir.is_dir() {
        return Ok(0);
    }
    if all {
        remove_all(spec, &repo_dir)
    } else {
        remove_one(spec)
    }
}
```

- `fn remove_all(spec: &Spec, repo_dir: &Path) -> Result<usize>` — the `read_dir` sweep
  plus the `refs` cleanup.
- `fn remove_one(spec: &Spec) -> Result<usize>` — resolve `spec.revision` (sha-or-ref)
  and drop the one revision dir.

Add `use std::path::Path;` if not already imported.

Verify: `cargo test --locked`

---

## Step 15 — split `search::expand` BFS level

**Problem:** three nested loops (`hop → frontier → adjacent`) with a 2-deep `?` body.

**Do:** in `src/search/mod.rs` factor one BFS level out:

```rust
pub fn expand(conn: &Connection, seeds: &[Hit], hops: u8) -> Result<Vec<Neighbor>> {
    let mut visited: HashSet<i64> = seeds.iter().map(|h| h.id).collect();
    let mut frontier: Vec<(i64, String)> = seeds.iter().map(|h| (h.id, h.key.clone())).collect();
    let mut out = Vec::new();

    for hop in 1..=hops.max(1) {
        frontier = expand_frontier(conn, &frontier, hop, &mut visited, &mut out)?;
        if frontier.is_empty() {
            break;
        }
    }
    Ok(out)
}

/// One BFS level: append every unseen neighbour of `frontier` to `out` and return the
/// frontier for the next hop.
fn expand_frontier(
    conn: &Connection,
    frontier: &[(i64, String)],
    hop: u8,
    visited: &mut HashSet<i64>,
    out: &mut Vec<Neighbor>,
) -> Result<Vec<(i64, String)>> {
    let mut next = Vec::new();
    for (id, key) in frontier {
        for (nid, nkey) in adjacent(conn, *id, key)? {
            if !visited.insert(nid) {
                continue;
            }
            let Some(memory) = store::get_by_id(conn, nid)? else {
                continue;
            };
            out.push(Neighbor {
                hop,
                via: key.clone(),
                memory,
            });
            next.push((nid, nkey));
        }
    }
    Ok(next)
}
```

Verify: `cargo test --locked`

---

## Acceptance (Part 2)

```sh
cargo fmt --all
cargo test --locked
cargo clippy --all-targets --all-features   # must be warning-free
```

- [x] `execute` / `doctor::run` / `add` / `remove` / `expand` no longer own their
      nested work; each helper is single-purpose.
- [x] The `model::verify` report exists once (`cli::model::verify_weights`).
- [x] No output string, SQL, or error message changed.
- [x] `cargo test` green; `cargo clippy -D warnings` clean; `cargo fmt --check` clean.


---

# Part 3 — correctness & best-practice fixes (steps 16–24)

> **Status: applied.** Unlike Parts 1–2 these **do change behaviour** (a bounded search
> body, a cosine distance metric, atomic import/export), because each step closes a
> gap between the code and `SPEC.md`. Output *strings* and the public API stay
> compatible; one test gains a tolerance for the metric change (called out in the step).
> Same verify command as Part 1.

## Why

Findings from a full read of `src/` against `SPEC.md`:

| # | Severity | Location | Violation |
|---|---|---|---|
| 1 | high | `search::run` | returns full bodies — SPEC §9/§10/§14 require "snippets + scores, never dumps" |
| 2 | high | `index::vec::create_sql` | omits `distance_metric=cosine` (SPEC §7); exposed `distance`/`--max-distance` are squared-L2 |
| 3 | high | `store::add`, `maintain::reembed` | model inference runs *inside* the write transaction |
| 4 | high | `portable::import` | not atomic (per-line tx) and embeds one text at a time |
| 5 | med | `portable::export` | writes the target in place (no temp+rename) and queries N+1 |
| 6 | med | `store::resolve_key` | loads *every* key to pick a collision suffix |
| 7 | med | `index::vec::upsert` | delete+insert with no transaction of its own |
| 8 | med | `cli::session` | `expect()` on a reachable branch; a panic poisons the MCP mutex |
| 9 | low | `index::vec::register_extension` | returns a `Result` that can never be `Err` |

Deferred (noted, not done — both need design work, see *Follow-ups* at the end):
MCP tool bodies block a tokio worker; CI never builds `--no-default-features`.

---

## Step 16 — bound the search body (snippets, not dumps)

**Problem:** `src/search/mod.rs:174` returns `mem.body` verbatim while the field doc at
`:93` claims it is "already bounded"; `memory_search` serialises the whole bundle, so MCP
hands the model full memories. SPEC §9: "return **snippets + scores**, never dumps".

**Do:**

1. In `src/search/mod.rs`, after `pub mod rrf;`, add:

```rust
/// Characters of a body returned per search hit (SPEC §9/§14: snippets, not dumps).
pub const SNIPPET_CHARS: usize = 280;

/// First `max` characters of `s`, with a trailing `…` when it was truncated.
pub fn snippet(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let mut out: String = s.chars().take(max).collect();
    out.push('…');
    out
}
```

2. Change the `Hit::body` doc and the value at `:174` to `body: snippet(&mem.body, SNIPPET_CHARS)`.
3. In `expand_frontier`, bind `get_by_id` as `mut memory` and set
   `memory.body = snippet(&memory.body, SNIPPET_CHARS)` before pushing the `Neighbor`.
4. Add a unit test for `snippet` (short unchanged, long truncated on a char boundary).

Verify: `cargo test --locked`

## Step 17 — cosine distance metric

**Problem:** `src/index/vec.rs::create_sql` omits `distance_metric`, so `vec0` uses its
default L2. SPEC §7 specifies cosine, and `--max-distance`/the `distance` field are
documented (CLI + MCP) as *cosine*.

**Do:** add `distance_metric=cosine` to the `embedding float[{dim}]` column in `create_sql`.

**Note:** an existing `memory_vec` keeps L2 until `reembed` recreates it — acceptable, the
index is derived and rebuildable. `tests/search.rs::max_distance_filters_the_vector_leg`
still passed with its old `0.0` ceiling, but that relied on float-exactness; it now uses a
separating `0.5` (cosine distance is `1 - cos`: exact ≈ 0, unrelated = 1) and asserts both
the survivor and the filtered-out document.

Verify: `cargo test --locked`

## Step 18 — embed outside the write transaction

**Problem:** `src/store/mod.rs::add` opens the transaction, then calls `embedder.embed_one`
inside it — a ~1 s CPU hold on the write lock (SPEC §13).

**Do:** split `add` into a read/plan phase and a write phase:

- `struct Prepared { key, text, hash, existing, dim, model }` with
  `pub(crate) fn needs_embed(&self) -> bool`.
- `pub(crate) fn prepare(conn, embedder, mem) -> Result<Prepared>` — resolves the key,
  computes the projection + hash, reads the existing `(id, hash)`; **no model call**.
- `pub(crate) fn apply(tx, mem, prepared, vector: Option<&[f32]>) -> Result<i64>` — the
  former insert/update + tags + links + vec writes.
- `add` = `prepare` → `index::vec::ensure` → embed → open tx → `apply` → commit.

`maintain::reembed` already batches; leave the snapshot-then-write shape but comment that
the batch embed stays inside the tx deliberately (a full rebuild wants one atomic swap).

Verify: `cargo test --locked`

## Step 19 — atomic, batched, fail-fast import

**Problem:** `src/portable/import.rs` writes row-by-row, each in its own transaction, so a
bad line halfway leaves a partial store; embeddings are one text at a time.

**Do:**

1. Read + validate the whole file first (`read_records`) — a bad/missing-key line fails
   before anything is written.
2. `store::prepare` each record (reads only), collecting `(index, text)` for rows that
   need embedding.
3. Embed those texts in `BATCH` (16) sized `embedder.embed` calls — outside the tx.
4. `index::vec::ensure`, then one `unchecked_transaction` applying every `store::apply`.
5. `dry_run` returns the pending count without writing (unchanged semantics).

Verify: `cargo test --locked`

## Step 20 — atomic export, two queries

**Problem:** `src/portable/export.rs` selects ids then hydrates each (`get_by_id` = 2
queries/row → N+1) and writes the destination directly.

**Do:**

1. Add `store::all(conn) -> Result<Vec<Memory>>`: one row query ordered by `key` + one
   `memory_tag`/`tag` query, grouped in memory (a shared `row_to_memory` helper backs both
   `get_by_id` and `all`).
2. Rewrite `to_jsonl`: `store::all` → write to `path` with a `.tmp` extension → flush +
   `sync_all` → `rename`. Same bytes, same count, but a failed export never leaves a torn
   dump.

Verify: `cargo test --locked`



## Step 21 — bounded key-collision probe

**Problem:** `store::resolve_key` builds a `HashSet` of *all* keys per auto-key insert.

**Do:** replace `taken_keys` with `taken_variants(conn, base)` — `SELECT key FROM memory
WHERE key LIKE '<base>%'` (base is `[a-z0-9-]`, so no wildcard escapes needed) — and feed
that to the existing `slug::unique_slug`.

Verify: `cargo test --locked`

## Step 22 — atomic `vec0` upsert

**Problem:** `index::vec::upsert` is `DELETE` then `INSERT`; atomic only because callers
wrap it.

**Do:** wrap the pair in `SAVEPOINT vec_upsert` / `RELEASE`, rolling back on failure, so a
standalone call cannot drop a vector.

Verify: `cargo test --locked`

## Step 23 — `expect()` → error in the session

**Problem:** `src/cli/session.rs` uses `expect("encoder was just loaded")` in `add` /
`import` / `reembed`; a panic there poisons the MCP `Mutex<Session>`.

**Do:** `.ok_or_else(|| Error::Config("encoder was not loaded".into()))?`.

Verify: `cargo test --locked`

## Step 24 — `register_extension` needs no `Result`

**Problem:** `index::vec::register_extension` always returns `Ok(())`; callers carry a
meaningless `?`.

**Do:** return `()`. Update `db::open` and the `vec` unit test. (Keep the `unsafe` block;
annotate the `transmute` target type in place.)

Verify: `cargo test --locked`

---

## Acceptance (Part 3)

```sh
cargo fmt --all
cargo test --locked
cargo clippy --all-targets --all-features   # must be warning-free
cargo check --no-default-features           # store-only build still compiles
```

- [x] `memory_search` / `--json` return snippets, not full bodies.
- [x] `memory_vec` is cosine; the `distance` field and `--max-distance` mean what they say.
- [x] No model call runs while a write transaction is open.
- [x] Import is all-or-nothing and batches its embeds.
- [x] Export is atomic and does not query N+1.
- [x] No `expect()` on a reachable branch in the CLI session.
- [x] `cargo test` green; `cargo clippy -D warnings` clean; `cargo fmt --check` clean.

## Follow-ups (not in this part)

- **MCP blocks the tokio executor.** `src/mcp/mod.rs::with_store` holds a `std::sync::Mutex`
  and runs synchronous SQLite/model work on a runtime worker. `block_in_place` is out
  (`tests/mcp.rs` uses a current-thread runtime, where it panics), so this wants
  `spawn_blocking` with an async tool body — a real change to the `#[tool]` fns.
- **Config validation up front.** `device`/`precision`/`dim` are only checked at encoder
  load; one clear message in `Config::load` would be friendlier.

## Incidental (outside the numbered steps)

- **CI feature matrix** (was a follow-up): added `cargo test --locked --no-default-features`
  to `.github/workflows/ci.yml` so the advertised store-only build can't rot.
- **Example hermeticity**: `examples/db_bootstrap.rs` now uses `tempfile::tempdir()` instead
  of a fixed `$TMPDIR/llmphant-example` path.
- **Docker healthcheck**: `Dockerfile` now probes `llmphant --version` instead of `doctor`,
  which opened and migrated the store on every 30 s poll.
