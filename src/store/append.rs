use rusqlite::{Connection, OptionalExtension, params};
use serde::Serialize;

use crate::embed::projection;
use crate::error::{Error, Result};
use crate::store::links::Kind;

#[derive(Debug, Clone, Serialize)]
pub struct Appended {
    pub key: String,
    pub appended: bool,
    pub line: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub version: Option<i64>,
}

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
    let tx = conn.unchecked_transaction()?;
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
    if !force && let Some(hint) = crate::store::shape::record_hint(&next, &key) {
        return Err(Error::Invalid(crate::store::shape::refusal(&hint, &key)));
    }
    super::guard_protected(&tx, &key, &tags, &crate::store::Patch::default(), acted_by)?;
    if next.chars().count() > crate::store::MAX_BODY_CHARS {
        return Err(Error::TooLarge(format!(
            "{key} would pass {} chars: a subject this large is several subjects — write the new one \
             as its own memory and link the two with `[[{key}]]`",
            crate::store::MAX_BODY_CHARS
        )));
    }

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

fn carries(body: &str, line: &str) -> bool {
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
