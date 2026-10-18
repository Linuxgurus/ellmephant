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
//! THE CURSOR BELONGS TO THE READER and is not stored here: `seq` is monotonic (see the migration), so
//! a reader keeps its own `mail-last-read` and asks for everything after it. ★ The cursor advance is
//! the COMMIT: a processor that dies before advancing leaves the letter unread, so the next attempt
//! sees it again — at-least-once, never silently swallowed.
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

/// Write one letter and return its envelope. Refuses empty sender/recipient/body and an oversize body.
pub fn send(
    conn: &Connection,
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
    if recipient.is_empty() {
        return Err(Error::Invalid(
            "`recipient` cannot be empty: address a SELF (`stet`, `twine`), never a finger".into(),
        ));
    }
    // ★ A LETTER TO YOUR OWN BOX IS NEVER DELIVERY (seq 26, 2026-10-18): a handoff written FOR Stet's
    // next context was sent with the human's pen into the human's own box and sat unread in every
    // session. Refuse it in the STORE so every door inherits the rule — MCP, REST and the CLI. A
    // finger is `stet/finger-<id>`, so a finger answering its self is still allowed.
    if sender.eq_ignore_ascii_case(&recipient) {
        return Err(Error::Invalid(format!(
            "refused: `{sender}` is the box this pen already is — a letter to yourself is never \
             delivery; address the SELF that should read it"
        )));
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
        "INSERT INTO mail (sender, recipient, subject, body, encoding, bytes, asserted_by)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?1)",
        params![
            sender,
            recipient,
            subject.trim(),
            body,
            encoding,
            bytes as i64
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
        "SELECT seq, at, sender, subject, encoding, bytes
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
            })
        },
    )?;
    Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
}

/// One letter whole, body included. `None` when the seq does not exist (pruned, or never written).
pub fn read(conn: &Connection, seq: i64) -> Result<Option<Letter>> {
    Ok(conn
        .query_row(
            "SELECT seq, at, sender, recipient, subject, encoding, bytes, body
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

fn read_envelope(conn: &Connection, seq: i64) -> Result<Option<Envelope>> {
    Ok(read(conn, seq)?.map(|l| l.envelope))
}
