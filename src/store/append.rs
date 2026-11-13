//! `append` — continue a subject in ONE call, without reading and resending its body.
//!
//! ★ THE ECONOMICS WERE BACKWARDS (2026-11-13). Writing a new memory was one call of words the
//! writer already had; continuing an existing one was `memory_get` (the whole body back), an edit in
//! the model's head, and `memory_upsert` with the entire body resent. Two round trips and a payment
//! for text that did not change, so pens wrote a new row per thought instead — 24 rows for one PR
//! review. This store has already watched an instruction lose that fight: "do not write a row per
//! hour" was written down and alert rows stayed at 23 a day until the pen was given a cheap
//! continuation verb (upsert today's ledger row), at which point it became one row a day in a single
//! day. Behaviour follows the affordance.
//!
//! So: one call, one line, no read, no title, no tags, no body resend. The store does the rest inside
//! one transaction — snapshot the state being replaced, append, re-derive the edges the new line
//! implies, queue the re-embed.
//!
//! **It cannot create.** A missing key is refused and names `memory_upsert`: an append continues a
//! subject, and a subject that does not exist yet needs a title, which is exactly what a create is.
//! Without that rule, `append` would quietly become the cheapest way to write a new row.
//!
//! **It cannot change** the title, the audience, the tags, or any existing line — only append. And it
//! takes the same reach as an edit ([`crate::store::may_write`]), because it changes the body.

use rusqlite::{Connection, OptionalExtension, params};
use serde::Serialize;

use crate::embed::projection;
use crate::error::{Error, Result};
use crate::store::links::Kind;

/// What an [`append`] did.
#[derive(Debug, Clone, Serialize)]
pub struct Appended {
    /// The resolved key.
    pub key: String,
    /// False when the body already carried that line — a no-op, not an error, so a retry is safe.
    pub appended: bool,
    /// The line, trimmed.
    pub line: String,
    /// The version this write superseded, when it wrote. `memory_history KEY` lists it;
    /// `memory_get KEY version=N` reads it back.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub version: Option<i64>,
}

/// Append ONE line to an existing memory. See the module docs for why this verb exists.
///
/// Refusals, each naming what to do instead:
/// - an empty line, or text containing a newline (one line per call, so each is a fact the row carries);
/// - a key that does not exist — `memory_upsert` starts a subject;
/// - a caller that may not write the row — the same reach an edit takes;
/// - a body that would pass [`crate::store::MAX_BODY_CHARS`] — a subject that large is several subjects.
pub fn append(
    conn: &Connection,
    key: &str,
    line: &str,
    acted_by: Option<&str>,
    force: bool,
) -> Result<Appended> {
    let key = crate::store::slug::slugify(key);
    let line = line.trim();
    if line.is_empty() {
        return Err(Error::Invalid(
            "append needs a line: an empty append writes nothing".into(),
        ));
    }
    if line.contains('\n') {
        return Err(Error::Invalid(
            "append takes ONE line: send one call per line, so each line is a fact the row can \
             carry and a reader can quote"
                .into(),
        ));
    }
    // ★ THE READ IS INSIDE THE WRITE'S TRANSACTION (Stet, review of #229, point 4). Reading the body
    // before `BEGIN` is a read-modify-write: two pens appending to one subject row both read, both
    // write, and one line is lost with a 200. A deferred transaction that reads and then writes
    // refuses to upgrade once another writer has committed (SQLITE_BUSY_SNAPSHOT) — an error the
    // caller retries, which is the correct failure for a lost update. This verb exists to be used by
    // several pens on ONE row, so the window is the common case, not the corner.
    let tx = conn.unchecked_transaction()?;
    // ★ THE SAME REACH AN EDIT TAKES. An append changes the document — it is a body write, and the
    // permission question is identical. (Cross-pen appending is a separate question, soma/ellmephant#199.)
    crate::store::may_write(&tx, &key, acted_by)?;

    let row: Option<(i64, String, String, Vec<String>)> = tx
        .query_row(
            "SELECT id, title, body, tags FROM memory WHERE key = ?1",
            params![key],
            |r| {
                Ok((
                    r.get(0)?,
                    r.get(1)?,
                    r.get(2)?,
                    crate::store::tags::decode(&r.get::<_, String>(3)?),
                ))
            },
        )
        .optional()?;
    let Some((id, title, body, tags)) = row else {
        return Err(Error::NotFound(format!(
            "{key} — an append CONTINUES a subject. To start one, `memory_upsert` with a key, a \
             title and a body; the next line can then be an append."
        )));
    };
    if carries(&body, line) {
        return Ok(Appended {
            key,
            appended: false,
            line: line.to_string(),
            version: None,
        });
    }
    let next = format!("{}\n{line}", body.trim_end());
    // ★ THE SHAPE GATE APPLIES TO THE RESULT, NOT TO THE LINE (Stet, review of #229). A pen that
    // cannot write a play-by-play in one call must not be able to write one in three: three appends
    // of an `HH:MMZ ...` line produce exactly the body `memory_upsert` refuses. Refused with the same
    // sentence, and `force` is the same deliberate override.
    if !force {
        if let Some(hint) = crate::store::shape::record_hint(&next, &key) {
            return Err(Error::Invalid(crate::store::shape::refusal(&hint, &key)));
        }
    }
    // ★ A PROTECTED ROW KEEPS ITS LEASE GATE (Stet, same review). `may_write` is the reach; the lease
    // is the half that matters for the rows that matter, and an append with no patch to inspect is
    // exactly the write `guard_protected` exists for. Same rule, same sentence as `merge`.
    super::guard_protected(&tx, &key, &tags, &crate::store::Patch::default(), acted_by)?;
    if next.chars().count() > crate::store::MAX_BODY_CHARS {
        return Err(Error::TooLarge(format!(
            "{key} would pass {} chars: a subject this large is several subjects — write the new one \
             as its own memory and link the two with `[[{key}]]`",
            crate::store::MAX_BODY_CHARS
        )));
    }

    // The state this append supersedes is history like any other write's.
    let version = crate::store::version::snapshot(
        &tx,
        id,
        &key,
        Some(&format!("appended one line: {line}")),
    )?;
    tx.execute(
        "UPDATE memory SET body = ?2, edited_by = ?3, updated = datetime('now') WHERE id = ?1",
        params![id, next, acted_by],
    )?;
    // The document moved, so its outgoing edges and its vector did too — the same two writes any
    // other body change makes, through the one home for each.
    crate::store::write_links(&tx, id, &key, &next)?;
    let doc = projection::embed_document(&key, &title, &next);
    crate::store::encode::enqueue(
        &tx,
        &Kind::Memory.keyslug(&key),
        &doc,
        crate::store::encode::PRIORITY_LIVE,
    )?;
    tx.commit()?;

    Ok(Appended {
        key,
        appended: true,
        line: line.to_string(),
        version: Some(version),
    })
}

/// Does the body already carry this line? Whitespace- and case-insensitive, per line: an append that
/// would duplicate a line is a retry, not a fact.
fn carries(body: &str, line: &str) -> bool {
    // ★ WHITESPACE ONLY (Stet, review of #229). Case is part of the fact for a route, a verb, an
    // identifier or a SQL keyword: folding it silently dropped `post /memory ...` as "already there".
    let squash = |s: &str| s.split_whitespace().collect::<Vec<_>>().join(" ");
    let want = squash(line);
    body.lines().map(squash).any(|have| have == want)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_line_already_present_is_a_retry_and_whitespace_does_not_hide_it() {
        assert!(carries("one fact\nsecond  fact", "second fact"));
        assert!(
            !carries("One Fact", "one  fact"),
            "case is part of the fact"
        );
        assert!(carries("One  Fact", "One Fact"));
        assert!(!carries("one fact", "one fact and more"));
        assert!(!carries("", "anything"));
    }
}
