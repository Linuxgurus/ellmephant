use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};

use crate::error::{Error, Result};

pub const MAX_BODY_BYTES: usize = 1 << 20;

pub const MAX_WAIT_SECONDS: u64 = 25;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Envelope {
    pub seq: i64,
    pub at: String,
    pub sender: String,
    pub subject: String,
    pub encoding: String,
    pub bytes: i64,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub sent_by: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub sent_under: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Letter {
    #[serde(flatten)]
    pub envelope: Envelope,
    pub recipient: String,
    pub body: String,
}

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

pub fn prune(conn: &Connection, keep_days: i64) -> Result<usize> {
    if keep_days <= 0 {
        return Err(Error::Invalid("`keep_days` must be positive".into()));
    }
    Ok(conn.execute(
        "DELETE FROM mail WHERE at < datetime('now', ?1)",
        params![format!("-{keep_days} days")],
    )?)
}

pub fn head(conn: &Connection, recipient: &str) -> Result<i64> {
    Ok(conn.query_row(
        "SELECT COALESCE(MAX(seq), 0) FROM mail WHERE recipient = ?1",
        params![recipient.trim().to_ascii_lowercase()],
        |r| r.get(0),
    )?)
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Cursor {
    pub reader: String,
    pub recipient: String,
    pub seq: i64,
    pub updated: String,
    pub head: i64,
    pub unread: i64,
    pub moved: i64,
}

fn normalized(s: &str) -> String {
    s.trim().to_ascii_lowercase()
}

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

pub fn cursor_or_zero(conn: &Connection, reader: &str, recipient: &str) -> Result<i64> {
    Ok(cursor(conn, reader, recipient)?.map(|c| c.seq).unwrap_or(0))
}

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

const SCOPES: [&str; 3] = ["send", "read", "ack"];

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

pub fn revoke(conn: &Connection, grantor: &str, surrogate: &str) -> Result<usize> {
    Ok(conn.execute(
        "UPDATE mail_surrogate SET revoked_at = datetime('now')
          WHERE lower(grantor) = lower(?1) AND lower(surrogate) = lower(?2)
            AND revoked_at IS NULL",
        params![grantor.trim(), surrogate.trim()],
    )?)
}

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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BoxScope {
    Read,
    Ack,
}

impl BoxScope {
    pub fn as_str(self) -> &'static str {
        match self {
            BoxScope::Read => "read",
            BoxScope::Ack => "ack",
        }
    }
}

pub fn may_open_box(
    conn: &Connection,
    reader: &str,
    requested: &str,
    scope: BoxScope,
) -> Result<()> {
    let allowed = match scope {
        BoxScope::Read => may_read_box(conn, reader, requested)?,
        BoxScope::Ack => may_ack_box(conn, reader, requested)?,
    };
    if allowed {
        return Ok(());
    }
    let verb = scope.as_str();
    Err(Error::Forbidden(format!(
        "`{reader}` may not {verb} `{requested}`'s mail — only the recipient, or a live `{verb}` \
         grant, may"
    )))
}

pub fn may_read_box(conn: &Connection, reader: &str, requested: &str) -> Result<bool> {
    if reader.eq_ignore_ascii_case(requested) {
        return Ok(true);
    }
    live_grant(conn, requested, reader, "read")
}

pub fn may_ack_box(conn: &Connection, reader: &str, requested: &str) -> Result<bool> {
    if reader.eq_ignore_ascii_case(requested) {
        return Ok(true);
    }
    live_grant(conn, requested, reader, "ack")
}

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
