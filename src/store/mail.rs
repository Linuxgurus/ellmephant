//! Mail: a **mailbox of addressed rows** (migration `0013`).
//!
//! A letter is the only thing that crosses between two selves. It is written, it sits, and the reader
//! reads it when the reader chooses — nothing is delivered, no door is rung, and the sender never
//! learns whether it was read. That asymmetry is the point: **a letter is a claim, not an order**, so
//! the recipient keeps every decision about it, including the decision to ignore it.
//!
//! WHY A TABLE AND NOT A NOTIFICATION. Anything shaped "do this" between two selves is one self aiming
//! the other. Rows have no such power: the worst a writer can do is leave something unread, and the
//! reader's own cursor decides what "unread" means. It also means a finger can write down a result
//! without inventing a memory it has not earned or dying with the pod that produced it.
//!
//! ★ A MEMORY IS KEPT BECAUSE IT IS TRUE; A LETTER IS KEPT BECAUSE NOBODY HAS READ IT YET. Letters
//! therefore expire ([`prune`]) and memories do not. If nobody would read it twice, it is a letter.
//!
//! THE CURSOR BELONGS TO THE READER, and since migration `0017` it is stored here as one row per
//! (reader, box) ([`cursor`]/[`advance`]) — the reader being the PROVEN pen, never a tool argument.
//! `seq` is monotonic (see the migration), so a reader asks for everything after its own place. ★ The
//! cursor advance is the COMMIT: a processor that dies before advancing leaves the letter unread, so
//! the next attempt sees it again — at-least-once, never silently swallowed.
//!
//! WHAT THIS IS NOT: authenticated. `sender` is asserted by the caller (the store has no auth), so the
//! FENCE around the store is the control and "claims, not orders" is the safety.

use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};

use crate::error::{Error, Result};

/// Largest body accepted, in bytes (1 MiB). A mailbox is not a file server: a real artifact belongs in
/// object storage with a link in the letter, so a finger never pulls megabytes into its context.
pub const MAX_BODY_BYTES: usize = 1 << 20;

/// Longest a `wait_seconds` may block, in seconds. Bounded so a tool call cannot hang past a client's
/// own timeout and look like a broken server.
pub const MAX_WAIT_SECONDS: u64 = 25;

/// One letter WITHOUT its body: enough to decide whether the body is worth fetching.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Envelope {
    /// Monotonic id — the reader's cursor.
    pub seq: i64,
    /// When it was written (UTC, SQLite `datetime('now')`).
    pub at: String,
    /// Who wrote it: `stet`, or `stet/finger-<id>` for a finger on that self's behalf.
    pub sender: String,
    /// One line, so an inbox can be scanned without bodies.
    pub subject: String,
    /// How `body` is encoded: `text`, `json`, `gzip+base64`.
    pub encoding: String,
    /// Size of the body in bytes, so the reader decides before paying for it.
    pub bytes: i64,
    /// The PROVEN pen, when it differs from `sender`: a letter sent on someone else's behalf.
    /// Absent on an ordinary letter, so every existing reader is unchanged.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub sent_by: Option<String>,
    /// The `granted_at` of the grant that authorised the send, when there was one. `sent_by` shows
    /// that the names DIFFER; this shows that a live grant backed it, which is the fact a reader
    /// actually needs and cannot infer.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub sent_under: Option<String>,
}

/// One letter WITH its body.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Letter {
    /// The envelope's fields.
    #[serde(flatten)]
    pub envelope: Envelope,
    /// The box it was addressed to.
    pub recipient: String,
    /// The body, in whatever `encoding` says.
    pub body: String,
}

/// Write one letter and return its envelope.
///
/// `proven` is the pen the connection proved, `sender` is the name the letter is from. They differ
/// only when `proven` holds a live `send` grant from `sender`; **an unauthorized claim is refused,
/// never silently overridden** (the defect of 2026-10-25, seq 566).
pub fn send(
    conn: &Connection,
    proven: &str,
    sender: &str,
    recipient: &str,
    subject: &str,
    body: &str,
    encoding: &str,
) -> Result<Envelope> {
    let sender = sender.trim();
    let recipient = recipient.trim().to_ascii_lowercase();
    let encoding = encoding.trim();
    let encoding = if encoding.is_empty() {
        "text"
    } else {
        encoding
    };

    if sender.is_empty() {
        return Err(Error::Invalid(
            "`sender` cannot be empty: say who is writing".into(),
        ));
    }
    may_send_as(conn, proven, sender)?;
    // A receipt a reader can check: WHICH grant authorised this, if any. `sent_by` shows the names
    // differ; this shows a live grant backed the difference.
    let sent_under = if proven.eq_ignore_ascii_case(sender) {
        None
    } else {
        grant_at(conn, sender, proven, "send")?
    };
    if recipient.is_empty() {
        return Err(Error::Invalid(
            "`recipient` cannot be empty: address a SELF (`stet`, `twine`), never a finger".into(),
        ));
    }
    if body.trim().is_empty() {
        return Err(Error::Invalid("`body` cannot be empty".into()));
    }
    let bytes = body.len();
    if bytes > MAX_BODY_BYTES {
        return Err(Error::Invalid(format!(
            "`body` is {bytes} bytes; the cap is {MAX_BODY_BYTES}. Put the artifact in object storage \
             and send a link plus a checksum."
        )));
    }

    conn.execute(
        "INSERT INTO mail (sender, recipient, subject, body, encoding, bytes, asserted_by, sent_under)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
        params![
            sender,
            recipient,
            subject.trim(),
            body,
            encoding,
            bytes as i64,
            proven,
            sent_under
        ],
    )?;
    let seq = conn.last_insert_rowid();
    read_envelope(conn, seq)?.ok_or_else(|| Error::Invalid("letter vanished after insert".into()))
}

/// Everything in `recipient`'s box after `since`, oldest first, WITHOUT bodies.
pub fn inbox(
    conn: &Connection,
    recipient: &str,
    since: i64,
    limit: usize,
) -> Result<Vec<Envelope>> {
    let mut stmt = conn.prepare(
        "SELECT seq, at, sender, subject, encoding, bytes,
                CASE WHEN asserted_by IS NOT NULL AND lower(asserted_by) <> lower(sender)
                     THEN asserted_by END,
                sent_under
           FROM mail
          WHERE recipient = ?1 AND seq > ?2
          ORDER BY seq
          LIMIT ?3",
    )?;
    let rows = stmt.query_map(
        params![recipient.trim().to_ascii_lowercase(), since, limit as i64],
        |r| {
            Ok(Envelope {
                seq: r.get(0)?,
                at: r.get(1)?,
                sender: r.get(2)?,
                subject: r.get(3)?,
                encoding: r.get(4)?,
                bytes: r.get(5)?,
                sent_by: r.get(6)?,
                sent_under: r.get(7)?,
            })
        },
    )?;
    Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
}

/// One letter whole, body included. `None` when the seq does not exist (pruned, or never written).
pub fn read(conn: &Connection, seq: i64) -> Result<Option<Letter>> {
    Ok(conn
        .query_row(
            "SELECT seq, at, sender, recipient, subject, encoding, bytes, body,
                    CASE WHEN asserted_by IS NOT NULL AND lower(asserted_by) <> lower(sender)
                         THEN asserted_by END,
                    sent_under
               FROM mail WHERE seq = ?1",
            params![seq],
            |r| {
                Ok(Letter {
                    envelope: Envelope {
                        seq: r.get(0)?,
                        at: r.get(1)?,
                        sender: r.get(2)?,
                        subject: r.get(4)?,
                        encoding: r.get(5)?,
                        bytes: r.get(6)?,
                        sent_by: r.get(8)?,
                        sent_under: r.get(9)?,
                    },
                    recipient: r.get(3)?,
                    body: r.get(7)?,
                })
            },
        )
        .optional()?)
}

/// Drop letters older than `keep_days`, returning how many went. ★ EXPIRY IS THE DIFFERENCE between a
/// mailbox and a second memory store: a letter is kept because nobody has read it yet, so once nobody
/// plausibly will, it should go. Memories have no such path — that is what makes a memory a memory.
pub fn prune(conn: &Connection, keep_days: i64) -> Result<usize> {
    if keep_days <= 0 {
        return Err(Error::Invalid("`keep_days` must be positive".into()));
    }
    Ok(conn.execute(
        "DELETE FROM mail WHERE at < datetime('now', ?1)",
        params![format!("-{keep_days} days")],
    )?)
}

/// The newest `seq` in a box (0 when empty) — a cheap "is anything there?" for a hand's free tick.
pub fn head(conn: &Connection, recipient: &str) -> Result<i64> {
    Ok(conn.query_row(
        "SELECT COALESCE(MAX(seq), 0) FROM mail WHERE recipient = ?1",
        params![recipient.trim().to_ascii_lowercase()],
        |r| r.get(0),
    )?)
}

/// One reader's place in one box — a POINTER, not a fact (migration `0017`).
///
/// ★ A CURSOR HAS ONE READER AND ONE WRITER. The reader is the pen this connection PROVED, resolved by
/// the door and handed here; it is never a tool argument, for the same reason a namespace is not: the
/// caller is a model and a model can be told to name someone else. Two readers of one box are two rows
/// (`hand` and `stet`), which is what makes "the hand read this" distinguishable from "the self did".
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Cursor {
    /// The proven pen this place belongs to.
    pub reader: String,
    /// The box.
    pub recipient: String,
    /// The highest `seq` this reader has DEALT WITH (0 before the first advance).
    pub seq: i64,
    /// When it last moved (UTC, from the store's clock).
    pub updated: String,
    /// The top of the box — so `unread` needs no second call.
    pub head: i64,
    /// Letters after `seq` that are still in the box.
    pub unread: i64,
    /// How many letters this advance skipped over (`0` on a read, or a no-op advance).
    pub moved: i64,
}

fn normalized(s: &str) -> String {
    s.trim().to_ascii_lowercase()
}

/// Read one reader's cursor in one box. `None` means this reader has never advanced here — which is a
/// fact, not `0`: a reader with no row sees the whole box, and that is the difference the old memory
/// cursors could not express.
pub fn cursor(conn: &Connection, reader: &str, recipient: &str) -> Result<Option<Cursor>> {
    let reader = normalized(reader);
    let recipient = normalized(recipient);
    let seq: Option<i64> = conn
        .query_row(
            "SELECT seq FROM mail_cursor WHERE reader = ?1 AND recipient = ?2",
            params![reader, recipient],
            |r| r.get(0),
        )
        .optional()?;
    Ok(match seq {
        Some(seq) => Some(shape(conn, &reader, &recipient, seq, 0)?),
        None => None,
    })
}

/// The reader's cursor, or `0` — the answer `mail_inbox` starts from.
pub fn cursor_or_zero(conn: &Connection, reader: &str, recipient: &str) -> Result<i64> {
    Ok(cursor(conn, reader, recipient)?.map(|c| c.seq).unwrap_or(0))
}

/// Move a cursor forward — the COMMIT of a read, and the only write this table has.
///
/// ★ BACKWARD IS REFUSED, NOT IGNORED. A cursor that silently went back would re-deliver mail already
/// dealt with and, worse, look like it had not; the refusal names the current value so a caller can
/// see what it tried to overwrite. Idempotent at the same value: advancing to where you already are is
/// a success with `moved: 0`.
pub fn advance(conn: &Connection, reader: &str, recipient: &str, seq: i64) -> Result<Cursor> {
    let reader = normalized(reader);
    let recipient = normalized(recipient);
    if reader.is_empty() {
        return Err(Error::Invalid(
            "a cursor needs a reader: it belongs to the pen that proved itself".into(),
        ));
    }
    if seq < 0 {
        return Err(Error::Invalid(format!(
            "`seq` must be >= 0 (got {seq}); 0 is the beginning of the box"
        )));
    }
    let current = cursor_or_zero(conn, &reader, &recipient)?;
    if seq < current {
        return Err(Error::Invalid(format!(
            "refused: `{reader}`'s cursor in `{recipient}` is already at {current}; moving it back to \
             {seq} would re-deliver mail already dealt with. A cursor only moves forward."
        )));
    }
    conn.execute(
        "INSERT INTO mail_cursor (reader, recipient, seq) VALUES (?1, ?2, ?3)
         ON CONFLICT(reader, recipient) DO UPDATE SET
             seq = excluded.seq,
             updated = datetime('now')",
        params![reader, recipient, seq],
    )?;
    shape(conn, &reader, &recipient, seq, seq - current)
}

/// Assemble the reply from the row and the box's top, so `unread` is never derived by the caller.
fn shape(conn: &Connection, reader: &str, recipient: &str, seq: i64, moved: i64) -> Result<Cursor> {
    let updated: String = conn
        .query_row(
            "SELECT updated FROM mail_cursor WHERE reader = ?1 AND recipient = ?2",
            params![reader, recipient],
            |r| r.get(0),
        )
        .optional()?
        .unwrap_or_default();
    let head = head(conn, recipient)?;
    Ok(Cursor {
        reader: reader.to_string(),
        recipient: recipient.to_string(),
        seq,
        updated,
        head,
        unread: (head - seq).max(0),
        moved,
    })
}

fn read_envelope(conn: &Connection, seq: i64) -> Result<Option<Envelope>> {
    Ok(read(conn, seq)?.map(|l| l.envelope))
}

// ── SURROGATES ───────────────────────────────────────────────────────────────────────────────────
//
// A pen may lend its NAME to another pen: `sender` is the name on the letter, `asserted_by` is the
// pen that proved it spoke. The grant is a row, so it has a grantor, a date, a scope, an optional
// expiry, and a revocation that takes effect on the next call.
//
// ★ A GRANT IS SELF-ONLY. A pen lends its own name; it cannot lend someone else's, and no ancestry
// rule applies, because this is delegation of self rather than authority over another.

/// The scopes a grant may carry. Comma list in the row; validated on write.
const SCOPES: [&str; 3] = ["send", "read", "ack"];

/// A recorded grant.
#[derive(Debug, Clone, Serialize)]
pub struct Surrogate {
    pub grantor: String,
    pub surrogate: String,
    pub scopes: String,
    pub granted_at: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub expires_at: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub revoked_at: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
}

fn scopes_valid(scopes: &str) -> Option<String> {
    let cleaned: Vec<String> = scopes
        .split(',')
        .map(|s| s.trim().to_ascii_lowercase())
        .filter(|s| !s.is_empty())
        .collect();
    if cleaned.is_empty() {
        return None;
    }
    if cleaned.iter().any(|s| !SCOPES.contains(&s.as_str())) {
        return None;
    }
    Some(cleaned.join(","))
}

/// Grant `surrogate` the right to act as `grantor` for `scopes` (`send`, `read`, `ack`).
///
/// Re-granting replaces the scopes and clears a revocation: a grant is a row, and the ledger keeps
/// the date of the latest grant rather than a history of every edit.
pub fn grant(
    conn: &Connection,
    grantor: &str,
    surrogate: &str,
    scopes: &str,
    expires_at: Option<&str>,
    note: Option<&str>,
) -> Result<Surrogate> {
    let (grantor, surrogate) = (grantor.trim(), surrogate.trim());
    if grantor.is_empty() || surrogate.is_empty() {
        return Err(Error::Invalid(
            "a grant needs a grantor and a surrogate".into(),
        ));
    }
    if grantor.eq_ignore_ascii_case(surrogate) {
        return Err(Error::Invalid(
            "a pen does not need a grant to speak as itself".into(),
        ));
    }
    let scopes = scopes_valid(scopes)
        .ok_or_else(|| Error::Invalid("scopes must be a comma list of: send, read, ack".into()))?;

    conn.execute(
        "INSERT INTO mail_surrogate (grantor, surrogate, scopes, expires_at, note)
         VALUES (?1, ?2, ?3, ?4, ?5)
         ON CONFLICT(grantor, surrogate) DO UPDATE SET
             scopes = excluded.scopes,
             expires_at = excluded.expires_at,
             granted_at = datetime('now'),
             revoked_at = NULL,
             note = excluded.note",
        params![grantor, surrogate, scopes, expires_at, note],
    )?;

    conn.query_row(
        "SELECT grantor, surrogate, scopes, granted_at, expires_at, revoked_at, note
           FROM mail_surrogate WHERE grantor = ?1 AND surrogate = ?2",
        params![grantor, surrogate],
        |r| {
            Ok(Surrogate {
                grantor: r.get(0)?,
                surrogate: r.get(1)?,
                scopes: r.get(2)?,
                granted_at: r.get(3)?,
                expires_at: r.get(4)?,
                revoked_at: r.get(5)?,
                note: r.get(6)?,
            })
        },
    )
    .map_err(Into::into)
}

/// Revoke a grant. Effective on the next call — nothing is cached.
pub fn revoke(conn: &Connection, grantor: &str, surrogate: &str) -> Result<usize> {
    Ok(conn.execute(
        "UPDATE mail_surrogate SET revoked_at = datetime('now')
          WHERE lower(grantor) = lower(?1) AND lower(surrogate) = lower(?2)
            AND revoked_at IS NULL",
        params![grantor.trim(), surrogate.trim()],
    )?)
}

/// Every grant `surrogate` holds, live or not — the question a reader asks before trusting a letter.
pub fn surrogates_of(conn: &Connection, surrogate: &str) -> Result<Vec<Surrogate>> {
    let mut stmt = conn.prepare(
        "SELECT grantor, surrogate, scopes, granted_at, expires_at, revoked_at, note
           FROM mail_surrogate WHERE lower(surrogate) = lower(?1)
          ORDER BY grantor",
    )?;
    let rows = stmt.query_map(params![surrogate.trim()], |r| {
        Ok(Surrogate {
            grantor: r.get(0)?,
            surrogate: r.get(1)?,
            scopes: r.get(2)?,
            granted_at: r.get(3)?,
            expires_at: r.get(4)?,
            revoked_at: r.get(5)?,
            note: r.get(6)?,
        })
    })?;
    Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
}

/// The `granted_at` of the live grant, if one exists — the audit link `sent_under` stores.
fn grant_at(
    conn: &Connection,
    grantor: &str,
    surrogate: &str,
    scope: &str,
) -> Result<Option<String>> {
    Ok(conn
        .query_row(
            "SELECT granted_at FROM mail_surrogate
              WHERE lower(grantor) = lower(?1)
                AND lower(surrogate) = lower(?2)
                AND revoked_at IS NULL
                AND (expires_at IS NULL OR expires_at > datetime('now'))
                AND instr(',' || replace(scopes, ' ', '') || ',', ',' || ?3 || ',') > 0",
            params![grantor.trim(), surrogate.trim(), scope],
            |r| r.get(0),
        )
        .optional()?)
}

fn live_grant(conn: &Connection, grantor: &str, surrogate: &str, scope: &str) -> Result<bool> {
    Ok(grant_at(conn, grantor, surrogate, scope)?.is_some())
}

/// May `proven` send a letter whose `sender` is `sender`?
///
/// This is the check that replaces the silent override: the old path discarded the argument and
/// stamped the proven pen, so a letter written on someone's behalf went out as the typist's and
/// nobody was told.
pub fn may_send_as(conn: &Connection, proven: &str, sender: &str) -> Result<()> {
    if proven.eq_ignore_ascii_case(sender) {
        return Ok(());
    }
    if live_grant(conn, sender, proven, "send")? {
        return Ok(());
    }
    Err(Error::Invalid(format!(
        "refused: `{proven}` may not send as `{sender}`. A letter on another pen's behalf needs a \
         live `send` grant from them (`mail_surrogate_grant`)."
    )))
}

/// May `reader` read `requested`'s box?
pub fn may_read_box(conn: &Connection, reader: &str, requested: &str) -> Result<bool> {
    if reader.eq_ignore_ascii_case(requested) {
        return Ok(true);
    }
    live_grant(conn, requested, reader, "read")
}

/// May `reader` advance `requested`'s cursor?
pub fn may_ack_box(conn: &Connection, reader: &str, requested: &str) -> Result<bool> {
    if reader.eq_ignore_ascii_case(requested) {
        return Ok(true);
    }
    live_grant(conn, requested, reader, "ack")
}

/// Import `ELLMEPHANT_MAIL_SURROGATES=hands=stet,…` as `read` grants, once.
///
/// The environment is the wrong home for a relationship: it cannot be dated, scoped, revoked or
/// audited. It stays readable for exactly one release so a deployment that predates the grant table
/// keeps working, and every pair becomes a row the store can answer questions about.
///
/// `INSERT OR IGNORE`, not an upsert: re-running this on every boot must not revive a grant someone
/// revoked. A revocation is a decision, and a boot is not a contrary one.
pub fn import_env_surrogates(conn: &Connection) -> Result<usize> {
    let raw = std::env::var("ELLMEPHANT_MAIL_SURROGATES").unwrap_or_default();
    let mut imported = 0;
    let tx = conn.unchecked_transaction()?;
    for pair in raw.split(',') {
        let Some((surrogate, grantor)) = pair.split_once('=') else {
            continue;
        };
        let (surrogate, grantor) = (surrogate.trim(), grantor.trim());
        if surrogate.is_empty() || grantor.is_empty() {
            continue;
        }
        imported += tx.execute(
            "INSERT OR IGNORE INTO mail_surrogate (grantor, surrogate, scopes, note)
             VALUES (?1, ?2, 'read', 'imported from ELLMEPHANT_MAIL_SURROGATES')",
            params![grantor, surrogate],
        )?;
    }
    tx.commit()?;
    Ok(imported)
}
