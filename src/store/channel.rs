//! ELLMECHAT (#151): channels, the message log, and the gate that follows a pen.
//!
//! A channel message is not a memory. It is a log line with a fan-out and a receipt — and the
//! receipt is the interesting half, because it is the only thing that can answer "were you told?"
//!
//! Three events per recipient (see `migrations/0029_ellmechat.sql`):
//!
//! - `available_at` — the message exists for this pen: it was **told**;
//! - `presented_at` — the harness offered the inbox at a turn boundary: an **opportunity**;
//! - `fetched_at` — it actually pulled it in: it **listened**.
//!
//! The gate is the consequence of the gap between *opportunity* and *listened*. Never of the
//! sender's timing: a pen that was asleep was not ignoring anyone, so a channel on
//! `clock = 'delivery'` measures from `presented_at`, and a message that was never presented never
//! gates.
//!
//! ★ IT IS A SEATBELT, NOT A PENALTY. The unread message from a locking channel is exactly the news
//! a pen needs before it acts — a deployment went out, a server crashed, an incident is live. An
//! agent writing the record on a world it has not been told about is the thing this prevents. The
//! normal path is present-then-fetch in the same turn, so a well-behaved harness never meets the
//! lock; it exists for the turn that offered nothing.
//!
//! ★ `misses` makes one boundary a coincidence and two a pattern: `ops` tolerates one (a message can
//! arrive mid-turn), `emergency` does not.
//!
//! ★ The gate fails open on its own errors. Only a *confirmed* unread message past its grace holds
//! the lock — a false lock is indistinguishable from a broken store, and the pen cannot tell which
//! it is. It also expires: a lock that only a fetch or an ancestor can clear is a lock forever when
//! both are asleep, so `lock_max_secs` releases it loudly.
//!
//! ★ The gate is evaluated on the pen the effect is **attributed to**, not the one holding the
//! keyboard. A `sudo`-borrowed write therefore cannot launder a breach. That rule lives at the call
//! site (the tool wrapper); this module is the ledger it reads.

use rusqlite::{Connection, OptionalExtension, params};
use serde::Serialize;

use crate::error::{Error, Result};

/// No lock: the channel is conversation.
pub const GATE_NONE: &str = "none";
/// The record is blocked (`memory_upsert`, delete, merge, adopt, project writes); talking is not.
pub const GATE_WRITES: &str = "writes";
/// Everything except recovery is blocked. Reserved for a live incident.
pub const GATE_ALL: &str = "all";

/// The ops team's room. Membership is `list`, and joining it implies `#emergency`.
pub const OPS_TEAM: &str = "ops_team";
/// The channel every member must be reachable in if they run the cluster.
pub const EMERGENCY: &str = "emergency";

/// Which tools a breach is allowed to stop.
///
/// Two classes, not five: the five in the issue are the **tool** classes, and they collapse to
/// these two the moment the gate asks the question. Liveness and communication are never gated —
/// a locked-out mind must be able to ask what happened — so they have no class here at all.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Class {
    /// Changes the record: `memory_upsert`, `memory_delete`, `memory_merge`, `memory_adopt`,
    /// `project_upsert`. Gated by `writes` and by `all`.
    Record,
    /// Acts on the world: dispatch, handoff, reindex/rebuild. Gated by `all` only.
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

/// One channel and its policy.
#[derive(Debug, Clone, Serialize)]
pub struct Channel {
    pub key: String,
    pub title: String,
    /// `open` (every live self unless opted out) or `list` (explicit members only).
    pub members: String,
    pub gate: String,
    pub grace_secs: i64,
    pub clock: String,
    pub misses_to_gate: i64,
    pub lock_max_secs: i64,
}

/// One message as a reader sees it.
#[derive(Debug, Clone, Serialize)]
pub struct Message {
    pub seq: i64,
    pub channel: String,
    pub pen: String,
    pub body: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reply_to: Option<i64>,
    /// Set when the body was pruned. The receipt survives the text.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pruned_at: Option<String>,
    pub created: String,
}

/// The oldest message that is holding a pen behind, and why it qualifies.
#[derive(Debug, Clone, Serialize)]
pub struct Breach {
    pub channel: String,
    pub seq: i64,
    pub gate: String,
    /// The instant the clock started: `available_at` on `send`, `presented_at` on `delivery`.
    pub since: String,
    /// Turn boundaries that offered this message without a fetch.
    pub misses: i64,
    /// Seconds since `since`, measured by the store's own clock.
    pub age_secs: i64,
    /// Seconds since the lock could first have bitten: from the first opportunity, never from the
    /// message. An expiry measured from `since` would release an old message before a sleeping pen
    /// ever woke, which is the opposite of protecting it.
    pub lock_age_secs: i64,
    /// 0 = the lock never expires.
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
        "SELECT key, title, members, gate, grace_secs, clock, misses_to_gate, lock_max_secs
           FROM channel WHERE lower(key) = lower(?1)",
        params![key],
        |r| {
            Ok(Channel {
                key: r.get(0)?,
                title: r.get(1)?,
                members: r.get(2)?,
                gate: r.get(3)?,
                grace_secs: r.get(4)?,
                clock: r.get(5)?,
                misses_to_gate: r.get(6)?,
                lock_max_secs: r.get(7)?,
            })
        },
    )
    .optional()?
    .ok_or_else(|| Error::Invalid(format!("no channel named {key}")))
}

/// Every channel, in creation order.
pub fn channels(conn: &Connection) -> Result<Vec<Channel>> {
    let mut stmt = conn.prepare(
        "SELECT key, title, members, gate, grace_secs, clock, misses_to_gate, lock_max_secs
           FROM channel ORDER BY rowid",
    )?;
    let rows = stmt.query_map([], |r| {
        Ok(Channel {
            key: r.get(0)?,
            title: r.get(1)?,
            members: r.get(2)?,
            gate: r.get(3)?,
            grace_secs: r.get(4)?,
            clock: r.get(5)?,
            misses_to_gate: r.get(6)?,
            lock_max_secs: r.get(7)?,
        })
    })?;
    Ok(rows.collect::<std::result::Result<Vec<_>, _>>()?)
}

/// Who is in one channel: the live selves, filtered by the channel's membership kind.
///
/// Not fingers — a finger is a mechanism by which a self does more work, not a party to a
/// conversation — and not `estate`, which has no entity behind it and can never authenticate to
/// read anything.
///
/// `open` is opt-out: a live self is a member unless a `state = 'out'` row says otherwise.
/// `list` is opt-in: only a `state = 'in'` row makes a member. One table serves both, and a channel
/// changes kind without a migration.
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

/// Join a channel. Joining `#ops_team` joins `#emergency` too — a team that runs the cluster must be
/// reachable when it falls over, and the coupling belongs to the membership rather than to a check
/// somewhere later.
pub fn join(conn: &Connection, pen: &str, channel: &str) -> Result<()> {
    let id = pen_id(conn, pen)?;
    let ch = channel_row(conn, channel)?;
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
    }
    tx.commit()?;
    Ok(())
}

/// Leave a channel. Leaving `#emergency` while on the ops team is refused — the graceful path is to
/// leave `#ops_team` first, which makes the reason the caller reads match the rule.
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
    conn.execute(
        "INSERT INTO channel_member (channel, pen_id, state) VALUES (?1, ?2, 'out')
         ON CONFLICT(channel, pen_id) DO UPDATE SET state = 'out', at = datetime('now')",
        params![ch.key, id],
    )?;
    Ok(())
}

/// Post a message and fan out a receipt to every member except the sender.
///
/// The sender is excluded deliberately: their own message must not be something they are "behind"
/// on, and a pen gated by its own words would be a comedy.
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

    // Fan out at send time: a receipt exists from the moment the message exists for that pen, which
    // is what makes `available_at` an honest answer to "were you told".
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

/// Mark that the harness offered the inbox at a turn boundary.
///
/// Called by whatever owns the turn (the spine, the harness), never by the model, and normally
/// followed by [`fetch`] in the same turn. Each call is one **miss** while the message stays
/// unfetched; a channel decides how many misses it tolerates before the lock.
///
/// A pen that is never presented is never judged — absence is not a crime — which is why this is
/// separate from [`fetch`].
pub fn present(conn: &Connection, pen: &str) -> Result<usize> {
    let id = pen_id(conn, pen)?;
    let n = conn.execute(
        "UPDATE channel_delivery
            SET presented_at = COALESCE(presented_at, datetime('now')),
                misses = misses + 1
          WHERE pen_id = ?1 AND fetched_at IS NULL",
        params![id],
    )?;
    Ok(n)
}

/// Read the inbox: everything this pen has been told and has not yet listened to.
///
/// Fetching *is* listening — the rows it returns are marked `fetched_at`, which is what clears the
/// gate. Re-reading history is [`history`], which does not touch the ledger.
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

/// What this pen has been TOLD and has not yet LISTENED to, per channel.
///
/// Read-only in the strictest sense: it moves no receipt. `fetch` is the only thing that marks a
/// message fetched, because fetching is listening and a count is not.
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

/// WHO is in a channel, as EFFECTIVE membership rather than as a list of override rows.
///
/// `open` channels are opt-out: every live self is a member unless it has an `out` row. `list`
/// channels are opt-in: only `in` rows are members. Fingers are never members — a finger is a
/// mechanism, not a party — and `estate` is not either: it has no entity behind it and can never
/// authenticate to read.
#[derive(Debug, Clone, Serialize)]
pub struct Member {
    pub pen: String,
    /// `in` for every effective member. A member by `open` and a member by invitation look the same
    /// in the answer, because the question is "who is here", not "why".
    pub state: String,
    /// Set when this pen has an EXPLICIT row (an invitation or an opt-out): the audit trail.
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

    // open: every live self, minus those who left on purpose.
    let mut stmt = conn.prepare(
        "SELECT p.name,
                (SELECT m.at FROM channel_member m
                  WHERE m.channel = ?1 AND m.pen_id = p.id AND m.state = 'out'),
                EXISTS (SELECT 1 FROM channel_member m2
                         WHERE m2.channel = ?1 AND m2.pen_id = p.id AND m2.state = 'in')
           FROM pen p
          WHERE p.kind = 'self' AND p.ceased_at IS NULL AND p.name <> 'estate'
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
            continue; // left on purpose
        }
        out.push(Member {
            pen,
            state: "in".to_string(),
            explicit: out_at.is_some() || invited,
            since: None,
        });
    }
    Ok(out)
}

/// What this pen was TOLD after `after_seq`, oldest first. Moves no receipt.
///
/// This is the delivery primitive a body waits on: a long poll reads from here, so "wait for
/// something to arrive" never has to become "fetch and mark everything read".
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

/// The log: what was said, regardless of whether this pen has listened. Does not touch receipts.
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

/// The oldest message holding this pen behind, if any.
///
/// `clock = 'send'` measures from `available_at`; `clock = 'delivery'` measures from
/// `presented_at` and ignores a message that was never presented. A channel's own
/// `misses_to_gate` decides how many offered-but-unfetched boundaries are tolerated.
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

/// Refuse a gated class of action, or allow it.
///
/// Fail open: a ledger this module cannot read does not gate. That is not leniency — a pen cannot
/// distinguish "you are late" from "the store is broken", and punishing the second as if it were the
/// first is how a chat bug locks the record.
///
/// Expire loudly: a lock past its channel's `lock_max_secs` releases, but says so, because an
/// emergency lock that aged out in silence is worse than one that held.
pub fn gate(conn: &Connection, pen: &str, class: Class) -> Result<()> {
    match breach(conn, pen) {
        Ok(Some(b)) if class.gated_by(&b.gate) => {
            if b.lock_max_secs > 0 && b.lock_age_secs >= b.lock_max_secs {
                tracing::warn!(
                    pen,
                    channel = %b.channel,
                    seq = b.seq,
                    lock_age_secs = b.lock_age_secs,
                    "ellmechat lock expired without a fetch; releasing and recording the expiry"
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
