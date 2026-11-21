use rusqlite::{Connection, OptionalExtension, params};
use serde::Serialize;

use crate::error::{Error, Result};

pub const GATE_NONE: &str = "none";
pub const GATE_WRITES: &str = "writes";
pub const GATE_ALL: &str = "all";

pub const OPS_TEAM: &str = "ops_team";
pub const EMERGENCY: &str = "emergency";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Class {
    Record,
    Acting,
}

impl Class {
    fn gated_by(self, gate: &str) -> bool {
        match self {
            Class::Record => gate == GATE_WRITES || gate == GATE_ALL,
            Class::Acting => gate == GATE_ALL,
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct Channel {
    pub key: String,
    pub title: String,
    pub members: String,
    pub gate: String,
    pub keep_secs: i64,
    pub grace_secs: i64,
    pub clock: String,
    pub misses_to_gate: i64,
    pub lock_max_secs: i64,
}

#[derive(Debug, Clone, Serialize)]
pub struct Message {
    pub seq: i64,
    pub channel: String,
    pub pen: String,
    pub body: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reply_to: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pruned_at: Option<String>,
    pub created: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct Breach {
    pub channel: String,
    pub seq: i64,
    pub gate: String,
    pub since: String,
    pub misses: i64,
    pub age_secs: i64,
    pub lock_age_secs: i64,
    pub lock_max_secs: i64,
}

fn pen_id(conn: &Connection, name: &str) -> Result<i64> {
    conn.query_row(
        "SELECT id FROM pen WHERE lower(name) = lower(?1)",
        params![name],
        |r| r.get(0),
    )
    .optional()?
    .ok_or_else(|| Error::Invalid(format!("no pen named {name}")))
}

fn channel_row(conn: &Connection, key: &str) -> Result<Channel> {
    conn.query_row(
        "SELECT key, title, members, gate, keep_secs, grace_secs, clock, misses_to_gate, lock_max_secs
           FROM channel WHERE lower(key) = lower(?1)",
        params![key],
        |r| {
            Ok(Channel {
                key: r.get(0)?,
                title: r.get(1)?,
                members: r.get(2)?,
                gate: r.get(3)?,
                keep_secs: r.get(4)?,
                grace_secs: r.get(5)?,
                clock: r.get(6)?,
                misses_to_gate: r.get(7)?,
                lock_max_secs: r.get(8)?,
            })
        },
    )
    .optional()?
    .ok_or_else(|| Error::Invalid(format!("no channel named {key}")))
}

pub fn channels(conn: &Connection) -> Result<Vec<Channel>> {
    let mut stmt = conn.prepare(
        "SELECT key, title, members, gate, keep_secs, grace_secs, clock, misses_to_gate, lock_max_secs
           FROM channel ORDER BY rowid",
    )?;
    let rows = stmt.query_map([], |r| {
        Ok(Channel {
            key: r.get(0)?,
            title: r.get(1)?,
            members: r.get(2)?,
            gate: r.get(3)?,
            keep_secs: r.get(4)?,
            grace_secs: r.get(5)?,
            clock: r.get(6)?,
            misses_to_gate: r.get(7)?,
            lock_max_secs: r.get(8)?,
        })
    })?;
    Ok(rows.collect::<std::result::Result<Vec<_>, _>>()?)
}

fn members_of(conn: &Connection, channel: &str) -> Result<Vec<i64>> {
    let mut stmt = conn.prepare(
        "SELECT p.id
           FROM pen p JOIN channel c ON lower(c.key) = lower(?1)
          WHERE p.kind = 'self' AND p.ceased_at IS NULL AND lower(p.name) <> 'estate'
            AND (
                 (c.members = 'open' AND NOT EXISTS (
                      SELECT 1 FROM channel_member m
                       WHERE m.channel = c.key AND m.pen_id = p.id AND m.state = 'out'))
              OR (c.members = 'list' AND EXISTS (
                      SELECT 1 FROM channel_member m
                       WHERE m.channel = c.key AND m.pen_id = p.id AND m.state = 'in'))
            )
          ORDER BY p.id",
    )?;
    let rows = stmt.query_map(params![channel], |r| r.get(0))?;
    Ok(rows.collect::<std::result::Result<Vec<_>, _>>()?)
}

pub fn join(conn: &Connection, pen: &str, channel: &str) -> Result<()> {
    let id = pen_id(conn, pen)?;
    let ch = channel_row(conn, channel)?;
    if ch.members == "list" && !keeper(pen) {
        return Err(Error::Invalid(format!(
            "refused: #{chan} is invite-only (`list`); a pen cannot add itself. Ask `estate` or \
             `james` to invite you.",
            chan = ch.key
        )));
    }
    let mut targets = vec![ch.key.clone()];
    if ch.key.eq_ignore_ascii_case(OPS_TEAM) {
        targets.push(EMERGENCY.to_string());
    }
    let tx = conn.unchecked_transaction()?;
    for target in targets {
        tx.execute(
            "INSERT INTO channel_member (channel, pen_id, state) VALUES (?1, ?2, 'in')
             ON CONFLICT(channel, pen_id) DO UPDATE SET state = 'in', at = datetime('now')",
            params![target, id],
        )?;
        tx.execute(
            "INSERT INTO channel_event (channel, pen_id, action, was, now)
             VALUES (?1, ?2, 'members', NULL, 'joined')",
            params![target, id],
        )?;
    }
    tx.commit()?;
    Ok(())
}

pub fn invite(conn: &Connection, actor: &str, pen: &str, channel: &str) -> Result<()> {
    if !keeper(actor) {
        return Err(Error::Invalid(format!(
            "refused: inviting needs `estate` or `james`; the proven caller is `{actor}`"
        )));
    }
    let id = pen_id(conn, pen)?;
    let (kind, name): (String, String) = conn.query_row(
        "SELECT kind, name FROM pen WHERE id = ?1",
        params![id],
        |r| Ok((r.get(0)?, r.get(1)?)),
    )?;
    if kind != "self" || name.eq_ignore_ascii_case("estate") {
        return Err(Error::Invalid(format!(
            "refused: `{pen}` can never be a channel member — a finger is a mechanism and `estate` \
             has no entity behind it"
        )));
    }
    let ch = channel_row(conn, channel)?;
    let mut targets = vec![ch.key.clone()];
    if ch.key.eq_ignore_ascii_case(OPS_TEAM) {
        targets.push(EMERGENCY.to_string());
    }
    let tx = conn.unchecked_transaction()?;
    let actor_id = pen_id(conn, actor)?;
    for target in targets {
        tx.execute(
            "INSERT INTO channel_member (channel, pen_id, state) VALUES (?1, ?2, 'in')
             ON CONFLICT(channel, pen_id) DO UPDATE SET state = 'in', at = datetime('now')",
            params![target, id],
        )?;
        tx.execute(
            "INSERT INTO channel_event (channel, pen_id, action, was, now)
             VALUES (?1, ?2, 'members', NULL, ?3)",
            params![target, actor_id, format!("invited {pen}")],
        )?;
    }
    tx.commit()?;
    Ok(())
}

pub fn leave(conn: &Connection, pen: &str, channel: &str) -> Result<()> {
    let id = pen_id(conn, pen)?;
    let ch = channel_row(conn, channel)?;
    if ch.key.eq_ignore_ascii_case(EMERGENCY) {
        let on_team: bool = conn
            .query_row(
                "SELECT EXISTS (SELECT 1 FROM channel_member m JOIN channel c ON c.key = m.channel
                                 WHERE lower(c.key) = lower(?1) AND m.pen_id = ?2 AND m.state = 'in')",
                params![OPS_TEAM, id],
                |r| r.get(0),
            )?;
        if on_team {
            return Err(Error::Invalid(format!(
                "refused: {pen} is on #{OPS_TEAM}, and ops membership requires #{EMERGENCY}.                  Leave #{OPS_TEAM} first."
            )));
        }
    }
    let tx = conn.unchecked_transaction()?;
    tx.execute(
        "INSERT INTO channel_member (channel, pen_id, state) VALUES (?1, ?2, 'out')
         ON CONFLICT(channel, pen_id) DO UPDATE SET state = 'out', at = datetime('now')",
        params![ch.key, id],
    )?;
    tx.execute(
        "INSERT INTO channel_event (channel, pen_id, action, was, now) VALUES (?1, ?2, 'members', 'in', 'left')",
        params![ch.key, id],
    )?;
    tx.execute(
        "UPDATE channel_delivery SET fetched_at = COALESCE(fetched_at, datetime('now'))
          WHERE pen_id = ?1
            AND seq IN (SELECT seq FROM channel_message WHERE channel = ?2)",
        params![id, ch.key],
    )?;
    tx.commit()?;
    Ok(())
}

pub fn send(
    conn: &Connection,
    pen: &str,
    channel: &str,
    body: &str,
    reply_to: Option<i64>,
) -> Result<Message> {
    let body = body.trim();
    if body.is_empty() {
        return Err(Error::Invalid("a channel message needs a body".into()));
    }
    let channel_row = channel_row(conn, channel)?;
    let channel_name = channel_row.key.clone();
    let pen_id = pen_id(conn, pen)?;

    if !members_of(conn, &channel_name)?.contains(&pen_id) {
        return Err(Error::Invalid(format!(
            "refused: `{pen}` is not a member of #{channel_name}, so this would be a message to a \
             room it is not in. Join it (open channels) or be invited (list channels) first."
        )));
    }
    if body.len() > MAX_BODY_CHARS {
        return Err(Error::Invalid(format!(
            "a channel message is at most {MAX_BODY_CHARS} BYTES; put an artifact in object \
             storage and post a link"
        )));
    }

    if let Some(parent) = reply_to {
        let parent_channel: Option<String> = conn
            .query_row(
                "SELECT channel FROM channel_message WHERE seq = ?1",
                params![parent],
                |r| r.get(0),
            )
            .optional()?;
        match parent_channel {
            None => {
                return Err(Error::Invalid(format!(
                    "reply_to {parent} does not exist; a reply to nothing is a new message"
                )));
            }
            Some(key) if !key.eq_ignore_ascii_case(&channel_name) => {
                return Err(Error::Invalid(format!(
                    "reply_to {parent} is in #{key}, not #{channel_name}"
                )));
            }
            Some(_) => {}
        }
    }

    let tx = conn.unchecked_transaction()?;
    tx.execute(
        "INSERT INTO channel_message (channel, pen_id, body, reply_to)
         VALUES (?1, ?2, ?3, ?4)",
        params![channel_row.key, pen_id, body, reply_to],
    )?;
    let seq = tx.last_insert_rowid();

    for member in members_of(&tx, &channel_row.key)? {
        if member == pen_id {
            continue;
        }
        tx.execute(
            "INSERT INTO channel_delivery (seq, pen_id, available_at)
             VALUES (?1, ?2, datetime('now'))",
            params![seq, member],
        )?;
    }
    tx.commit()?;

    message_by_seq(conn, seq)
}

fn message_by_seq(conn: &Connection, seq: i64) -> Result<Message> {
    conn.query_row(
        "SELECT m.seq, m.channel, p.name, m.body, m.reply_to, m.pruned_at, m.created
           FROM channel_message m JOIN pen p ON p.id = m.pen_id
          WHERE m.seq = ?1",
        params![seq],
        |r| {
            Ok(Message {
                seq: r.get(0)?,
                channel: r.get(1)?,
                pen: r.get(2)?,
                body: r.get(3)?,
                reply_to: r.get(4)?,
                pruned_at: r.get(5)?,
                created: r.get(6)?,
            })
        },
    )
    .map_err(Into::into)
}

pub fn present(conn: &Connection, pen: &str, boundary: Option<&str>) -> Result<usize> {
    let id = pen_id(conn, pen)?;
    if let Some(name) = boundary.map(str::trim).filter(|b| !b.is_empty()) {
        let seen: Option<(String, i64)> = conn
            .query_row(
                "SELECT boundary, strftime('%s','now') - strftime('%s', at)
                   FROM channel_boundary WHERE pen_id = ?1",
                params![id],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?;
        if let Some((seen_name, age)) = seen
            && seen_name == name
            && (0..=BOUNDARY_WINDOW_SECS).contains(&age)
        {
            return Ok(0);
        }
        conn.execute(
            "INSERT INTO channel_boundary (pen_id, boundary, at) VALUES (?1, ?2, datetime('now'))
             ON CONFLICT(pen_id) DO UPDATE SET boundary = ?2, at = datetime('now')",
            params![id, name],
        )?;
    }
    let n = conn.execute(
        "UPDATE channel_delivery
            SET presented_at = COALESCE(presented_at, datetime('now')),
                misses = misses + 1
          WHERE pen_id = ?1 AND fetched_at IS NULL",
        params![id],
    )?;
    Ok(n)
}

pub fn fetch(
    conn: &Connection,
    pen: &str,
    channel: Option<&str>,
    limit: i64,
) -> Result<Vec<Message>> {
    let id = pen_id(conn, pen)?;
    let limit = limit.clamp(1, 500);
    let mut stmt = conn.prepare(
        "SELECT m.seq, m.channel, p.name, m.body, m.reply_to, m.pruned_at, m.created
           FROM channel_delivery d
           JOIN channel_message m ON m.seq = d.seq
           JOIN pen p ON p.id = m.pen_id
          WHERE d.pen_id = ?1
            AND d.fetched_at IS NULL
            AND (?2 IS NULL OR lower(m.channel) = lower(?2))
          ORDER BY m.seq
          LIMIT ?3",
    )?;
    let messages = stmt
        .query_map(params![id, channel, limit], |r| {
            Ok(Message {
                seq: r.get(0)?,
                channel: r.get(1)?,
                pen: r.get(2)?,
                body: r.get(3)?,
                reply_to: r.get(4)?,
                pruned_at: r.get(5)?,
                created: r.get(6)?,
            })
        })?
        .collect::<std::result::Result<Vec<_>, _>>()?;
    drop(stmt);

    if !messages.is_empty() {
        let tx = conn.unchecked_transaction()?;
        for m in &messages {
            tx.execute(
                "UPDATE channel_delivery
                    SET fetched_at = datetime('now'),
                        presented_at = COALESCE(presented_at, datetime('now')),
                        misses = 0
                  WHERE seq = ?1 AND pen_id = ?2",
                params![m.seq, id],
            )?;
        }
        tx.commit()?;
    }
    Ok(messages)
}

#[derive(Debug, Clone, Serialize)]
pub struct Unread {
    pub channel: String,
    pub count: i64,
    pub oldest_seq: i64,
    pub latest_seq: i64,
}

pub fn unread(conn: &Connection, pen: &str) -> Result<Vec<Unread>> {
    let id = pen_id(conn, pen)?;
    let mut stmt = conn.prepare(
        "SELECT m.channel, COUNT(*), MIN(m.seq), MAX(m.seq)
           FROM channel_delivery d
           JOIN channel_message m ON m.seq = d.seq
          WHERE d.pen_id = ?1 AND d.fetched_at IS NULL
          GROUP BY m.channel
          ORDER BY m.channel",
    )?;
    let rows = stmt
        .query_map(params![id], |r| {
            Ok(Unread {
                channel: r.get(0)?,
                count: r.get(1)?,
                oldest_seq: r.get(2)?,
                latest_seq: r.get(3)?,
            })
        })?
        .collect::<std::result::Result<Vec<_>, _>>()?;
    Ok(rows)
}

#[derive(Debug, Clone, Serialize)]
pub struct Pruned {
    pub channel: String,
    pub emptied: usize,
}

pub fn prune(conn: &Connection, only: Option<&str>, now: Option<&str>) -> Result<Vec<Pruned>> {
    if let Some(key) = only {
        channel_row(conn, key)?;
    }
    let now = now.map(str::to_string).unwrap_or_else(|| {
        conn.query_row("SELECT datetime('now')", [], |r| r.get::<_, String>(0))
            .unwrap_or_default()
    });
    let mut stmt = conn.prepare(
        "SELECT key, keep_secs FROM channel
          WHERE keep_secs > 0 AND (?1 IS NULL OR lower(key) = lower(?1))
          ORDER BY key",
    )?;
    let channels = stmt
        .query_map(params![only], |r| {
            Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?))
        })?
        .collect::<std::result::Result<Vec<_>, _>>()?;

    let tx = conn.unchecked_transaction()?;
    let mut out = Vec::new();
    for (key, keep) in channels {
        let emptied = tx.execute(
            "UPDATE channel_message
                SET body = '', pruned_at = ?2
              WHERE channel = ?1
                AND pruned_at IS NULL
                AND created <= datetime(?2, ?3)",
            params![key, now, format!("-{keep} seconds")],
        )?;
        out.push(Pruned {
            channel: key,
            emptied,
        });
    }
    tx.commit()?;
    Ok(out)
}

pub fn set_retention(
    conn: &Connection,
    pen: &str,
    channel: &str,
    keep_secs: i64,
) -> Result<Channel> {
    if keep_secs < 0 {
        return Err(Error::Invalid("keep_secs cannot be negative".into()));
    }
    if keep_secs > 0 && keep_secs < MIN_KEEP_SECS {
        return Err(Error::Invalid(format!(
            "keep_secs must be 0 (forever) or at least {MIN_KEEP_SECS} seconds; {keep_secs} is \
             shorter than the floor and would expire text before anyone could read it"
        )));
    }
    if keep_secs > 100 * 365 * 24 * 3600 {
        return Err(Error::Invalid("keep_secs is implausibly large".into()));
    }
    if !keeper(pen) {
        return Err(Error::Invalid(format!(
            "refused: setting RETENTION needs `estate` or `james`; the proven caller is `{pen}`. \
             How long a channel keeps its text is a decision, not a preference."
        )));
    }
    let row = channel_row(conn, channel)?;
    if row.keep_secs == keep_secs {
        return Ok(row);
    }
    let pen_id = pen_id(conn, pen)?;
    let tx = conn.unchecked_transaction()?;
    tx.execute(
        "UPDATE channel SET keep_secs = ?2 WHERE key = ?1",
        params![row.key, keep_secs],
    )?;
    tx.execute(
        "INSERT INTO channel_event (channel, pen_id, action, was, now) VALUES (?1, ?2, 'retention', ?3, ?4)",
        params![row.key, pen_id, row.keep_secs.to_string(), keep_secs.to_string()],
    )?;
    tx.commit()?;
    channel_row(conn, &row.key)
}

#[derive(Debug, Clone, Serialize)]
pub struct Event {
    pub at: String,
    pub pen: Option<String>,
    pub action: String,
    pub was: Option<String>,
    pub now: Option<String>,
}

const GATE_KEEPERS: [&str; 2] = ["estate", "james"];

const MAX_BODY_CHARS: usize = 16 * 1024;

const MIN_KEEP_SECS: i64 = 3600;
const BOUNDARY_WINDOW_SECS: i64 = 120;

fn keeper(pen: &str) -> bool {
    GATE_KEEPERS
        .iter()
        .any(|k| k.eq_ignore_ascii_case(pen.trim()))
}

fn valid_key(key: &str) -> Result<String> {
    let key = key.trim().to_ascii_lowercase();
    let ok = !key.is_empty()
        && key.len() <= 48
        && key
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_' || c == '-')
        && key
            .chars()
            .next()
            .is_some_and(|c| c.is_ascii_alphanumeric());
    if !ok {
        return Err(Error::Invalid(format!(
            "{key:?} is not a channel key: lowercase letters, digits, `_` and `-`, starting with a letter or digit"
        )));
    }
    Ok(key)
}

pub fn create(
    conn: &Connection,
    pen: &str,
    key: &str,
    title: &str,
    members: &str,
    gate: &str,
) -> Result<Channel> {
    let key = valid_key(key)?;
    let title = title.trim();
    if title.is_empty() {
        return Err(Error::Invalid("a channel needs a title".into()));
    }
    let members = members.trim().to_ascii_lowercase();
    if members != "open" && members != "list" {
        return Err(Error::Invalid(format!(
            "members must be `open` or `list`, not {members:?}"
        )));
    }
    let gate = gate.trim().to_ascii_lowercase();
    if !matches!(gate.as_str(), "none" | "writes" | "all") {
        return Err(Error::Invalid(format!(
            "gate must be `none`, `writes` or `all`, not {gate:?}"
        )));
    }
    if (members == "list" || gate != "none") && !keeper(pen) {
        return Err(Error::Invalid(format!(
            "refused: creating a channel with gate `{gate}` or members `{members}` needs `estate` or `james`; \
             the proven caller is `{pen}`. A channel that can stop a pen writing to the record \
             is not something any pen may make on its own."
        )));
    }
    let exists: bool = conn.query_row(
        "SELECT EXISTS (SELECT 1 FROM channel WHERE lower(key) = lower(?1))",
        params![key],
        |r| r.get(0),
    )?;
    if exists {
        return Err(Error::Invalid(format!(
            "#{key} already exists; a channel is created once"
        )));
    }
    let pen_id = pen_id(conn, pen)?;
    let tx = conn.unchecked_transaction()?;
    tx.execute(
        "INSERT INTO channel (key, title, members, gate) VALUES (?1, ?2, ?3, ?4)",
        params![key, title, members, gate],
    )?;
    if members == "list" {
        tx.execute(
            "INSERT OR IGNORE INTO channel_member (channel, pen_id, state) VALUES (?1, ?2, 'in')",
            params![key, pen_id],
        )?;
    }
    tx.execute(
        "INSERT INTO channel_event (channel, pen_id, action, was, now) VALUES (?1, ?2, 'create', NULL, ?3)",
        params![key, pen_id, format!("{members}/{gate}")],
    )?;
    tx.commit()?;
    channel_row(conn, &key)
}

pub fn set_gate(conn: &Connection, pen: &str, channel: &str, gate: &str) -> Result<Channel> {
    let gate = gate.trim().to_ascii_lowercase();
    if !matches!(gate.as_str(), "none" | "writes" | "all") {
        return Err(Error::Invalid(format!(
            "gate must be `none`, `writes` or `all`, not {gate:?}"
        )));
    }
    if !keeper(pen) {
        return Err(Error::Invalid(format!(
            "refused: changing a gate needs `estate` or `james`; the proven caller is `{pen}`"
        )));
    }
    let row = channel_row(conn, channel)?;
    if row.gate == gate {
        return Ok(row);
    }
    let pen_id = pen_id(conn, pen)?;
    let tx = conn.unchecked_transaction()?;
    tx.execute(
        "UPDATE channel SET gate = ?2 WHERE key = ?1",
        params![row.key, gate],
    )?;
    tx.execute(
        "INSERT INTO channel_event (channel, pen_id, action, was, now) VALUES (?1, ?2, 'gate', ?3, ?4)",
        params![row.key, pen_id, row.gate, gate],
    )?;
    tx.commit()?;
    channel_row(conn, &row.key)
}

pub fn events(conn: &Connection, channel: &str) -> Result<Vec<Event>> {
    let mut stmt = conn.prepare(
        "SELECT e.at, p.name, e.action, e.was, e.now
           FROM channel_event e LEFT JOIN pen p ON p.id = e.pen_id
          WHERE e.channel = ?1
          ORDER BY e.id",
    )?;
    let rows = stmt
        .query_map(params![channel], |r| {
            Ok(Event {
                at: r.get(0)?,
                pen: r.get(1)?,
                action: r.get(2)?,
                was: r.get(3)?,
                now: r.get(4)?,
            })
        })?
        .collect::<std::result::Result<Vec<_>, _>>()?;
    Ok(rows)
}

#[derive(Debug, Clone, Serialize)]
pub struct Member {
    pub pen: String,
    pub state: String,
    pub explicit: bool,
    pub since: Option<String>,
}

pub fn members(conn: &Connection, channel: &str) -> Result<Vec<Member>> {
    let row = channel_row(conn, channel)?;
    let mut out = Vec::new();
    if row.members == "list" {
        let mut stmt = conn.prepare(
            "SELECT p.name, m.at
               FROM channel_member m JOIN pen p ON p.id = m.pen_id
              WHERE m.channel = ?1 AND m.state = 'in' AND p.ceased_at IS NULL
                AND p.kind = 'self' AND lower(p.name) <> 'estate'
              ORDER BY p.name",
        )?;
        let rows = stmt
            .query_map(params![row.key], |r| {
                Ok(Member {
                    pen: r.get(0)?,
                    state: "in".to_string(),
                    explicit: true,
                    since: Some(r.get(1)?),
                })
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        return Ok(rows);
    }

    let mut stmt = conn.prepare(
        "SELECT p.name,
                (SELECT m.at FROM channel_member m
                  WHERE m.channel = ?1 AND m.pen_id = p.id AND m.state = 'out'),
                EXISTS (SELECT 1 FROM channel_member m2
                         WHERE m2.channel = ?1 AND m2.pen_id = p.id AND m2.state = 'in')
           FROM pen p
          WHERE p.kind = 'self' AND p.ceased_at IS NULL AND lower(p.name) <> 'estate'
          ORDER BY p.name",
    )?;
    let rows = stmt.query_map(params![row.key], |r| {
        let out_at: Option<String> = r.get(1)?;
        let invited: bool = r.get(2)?;
        Ok((r.get::<_, String>(0)?, out_at, invited))
    })?;
    for row in rows {
        let (pen, out_at, invited) = row?;
        if out_at.is_some() && !invited {
            continue;
        }
        out.push(Member {
            pen,
            state: "in".to_string(),
            explicit: out_at.is_some() || invited,
            since: out_at,
        });
    }
    Ok(out)
}

pub fn head(conn: &Connection, pen: &str) -> Result<i64> {
    let id = pen_id(conn, pen)?;
    let head: Option<i64> = conn.query_row(
        "SELECT MAX(seq) FROM channel_delivery WHERE pen_id = ?1",
        params![id],
        |r| r.get(0),
    )?;
    Ok(head.unwrap_or(0))
}

pub fn since(conn: &Connection, pen: &str, after_seq: i64, limit: i64) -> Result<Vec<Message>> {
    let id = pen_id(conn, pen)?;
    let limit = limit.clamp(1, 500);
    let mut stmt = conn.prepare(
        "SELECT m.seq, m.channel, p.name, m.body, m.reply_to, m.pruned_at, m.created
           FROM channel_delivery d
           JOIN channel_message m ON m.seq = d.seq
           JOIN pen p ON p.id = m.pen_id
          WHERE d.pen_id = ?1 AND m.seq > ?2
          ORDER BY m.seq
          LIMIT ?3",
    )?;
    let rows = stmt
        .query_map(params![id, after_seq, limit], |r| {
            Ok(Message {
                seq: r.get(0)?,
                channel: r.get(1)?,
                pen: r.get(2)?,
                body: r.get(3)?,
                reply_to: r.get(4)?,
                pruned_at: r.get(5)?,
                created: r.get(6)?,
            })
        })?
        .collect::<std::result::Result<Vec<_>, _>>()?;
    Ok(rows)
}

pub fn history(conn: &Connection, channel: &str, since: i64, limit: i64) -> Result<Vec<Message>> {
    let limit = limit.clamp(1, 500);
    let mut stmt = conn.prepare(
        "SELECT m.seq, m.channel, p.name, m.body, m.reply_to, m.pruned_at, m.created
           FROM channel_message m
           JOIN pen p ON p.id = m.pen_id
          WHERE lower(m.channel) = lower(?1) AND m.seq > ?2
          ORDER BY m.seq
          LIMIT ?3",
    )?;
    let rows = stmt
        .query_map(params![channel, since, limit], |r| {
            Ok(Message {
                seq: r.get(0)?,
                channel: r.get(1)?,
                pen: r.get(2)?,
                body: r.get(3)?,
                reply_to: r.get(4)?,
                pruned_at: r.get(5)?,
                created: r.get(6)?,
            })
        })?
        .collect::<std::result::Result<Vec<_>, _>>()?;
    Ok(rows)
}

pub fn breach(conn: &Connection, pen: &str) -> Result<Option<Breach>> {
    let id = pen_id(conn, pen)?;
    let found = conn
        .query_row(
            "SELECT c.key, m.seq, c.gate,
                    CASE WHEN c.clock = 'send' THEN d.available_at ELSE d.presented_at END AS since,
                    d.misses,
                    strftime('%s', 'now')
                      - strftime('%s', CASE WHEN c.clock = 'send' THEN d.available_at
                                            ELSE d.presented_at END) AS age_secs,
                    strftime('%s', 'now')
                      - strftime('%s', COALESCE(d.presented_at, d.available_at)) AS lock_age_secs,
                    c.lock_max_secs
               FROM channel_delivery d
               JOIN channel_message m ON m.seq = d.seq
               JOIN channel c ON c.key = m.channel
              WHERE d.pen_id = ?1
                AND d.fetched_at IS NULL
                AND c.gate <> 'none'
                AND (
                      (c.members = 'open' AND NOT EXISTS (
                         SELECT 1 FROM channel_member mm
                          WHERE mm.channel = c.key AND mm.pen_id = d.pen_id AND mm.state = 'out'))
                   OR (c.members = 'list' AND EXISTS (
                         SELECT 1 FROM channel_member mm
                          WHERE mm.channel = c.key AND mm.pen_id = d.pen_id AND mm.state = 'in'))
                )
                AND d.misses >= c.misses_to_gate
                AND (
                      (c.clock = 'send'
                       AND strftime('%s', 'now') - strftime('%s', d.available_at) >= c.grace_secs)
                   OR (c.clock = 'delivery'
                       AND d.presented_at IS NOT NULL
                       AND strftime('%s', 'now') - strftime('%s', d.presented_at) >= c.grace_secs)
                )
              ORDER BY m.seq
              LIMIT 1",
            params![id],
            |r| {
                Ok(Breach {
                    channel: r.get(0)?,
                    seq: r.get(1)?,
                    gate: r.get(2)?,
                    since: r.get(3)?,
                    misses: r.get(4)?,
                    age_secs: r.get(5)?,
                    lock_age_secs: r.get(6)?,
                    lock_max_secs: r.get(7)?,
                })
            },
        )
        .optional()?;
    Ok(found)
}

pub fn gate(conn: &Connection, pen: &str, class: Class) -> Result<()> {
    match breach(conn, pen) {
        Ok(Some(b)) if class.gated_by(&b.gate) => {
            if b.lock_max_secs > 0 && b.lock_age_secs >= b.lock_max_secs {
                if let Ok(id) = pen_id(conn, pen) {
                    let tx = conn.unchecked_transaction();
                    if let Ok(tx) = tx {
                        let _ = tx.execute(
                            "UPDATE channel_delivery
                                SET misses = 0, presented_at = NULL
                              WHERE seq = ?1 AND pen_id = ?2",
                            params![b.seq, id],
                        );
                        let _ = tx.execute(
                            "INSERT INTO channel_event (channel, pen_id, action, was, now)
                             VALUES (?1, ?2, 'expiry', ?3, 'released')",
                            params![b.channel, id, format!("lock_age_secs={}", b.lock_age_secs)],
                        );
                        let _ = tx.commit();
                    }
                }
                tracing::warn!(
                    pen,
                    channel = %b.channel,
                    seq = b.seq,
                    lock_age_secs = b.lock_age_secs,
                    "ellmechat lock expired without a fetch; released and recorded"
                );
                return Ok(());
            }
            Err(Error::Invalid(format!(
                "refused: #{} message {} was offered {} time(s) and has not been fetched \
                 (since {}). The store has moved since you last listened; fetch the channel \
                 before changing the record. Reading is always allowed.",
                b.channel, b.seq, b.misses, b.since
            )))
        }
        Ok(_) => Ok(()),
        Err(e) => {
            tracing::warn!(pen, error = %e, "gate could not read its ledger; failing open");
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classes_map_to_gates() {
        assert!(!Class::Record.gated_by(GATE_NONE));
        assert!(Class::Record.gated_by(GATE_WRITES));
        assert!(Class::Record.gated_by(GATE_ALL));
        assert!(
            !Class::Acting.gated_by(GATE_WRITES),
            "writes does not stop acting"
        );
        assert!(Class::Acting.gated_by(GATE_ALL));
    }
}
