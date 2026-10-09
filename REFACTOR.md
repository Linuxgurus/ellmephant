# REFACTOR.md — dupe-removal + layer cleanup (executable plan)

> **Status: applied** (steps 1–9). `cargo test --locked`, `cargo fmt --all --check`
> and `cargo clippy --all-targets --all-features -- -D warnings` are all green.
> Kept as the record of *what* changed and *why*; each step below is still the
> recipe to reproduce or extend it.

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

