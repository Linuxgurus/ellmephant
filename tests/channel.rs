//! Ellmechat's ledger: fan-out, listening, and the gate (#151).
//!
//! The rules these tests exist to protect, in James's words: the gate is the consequence of **not
//! listening**, never of being asleep, and it is the pen the effect is attributed to that carries it.

use ellmephant::db;
use ellmephant::store::channel::{self, Class};
use rusqlite::{Connection, params};

mod common;
use common::temp_store;

fn store() -> (tempfile::TempDir, Connection) {
    let (dir, paths) = temp_store();
    let conn = db::open(&paths).expect("open");
    seed(&conn, "estate", None);
    seed(&conn, "james", Some("estate"));
    seed(&conn, "stet", Some("james"));
    seed(&conn, "cline", Some("james"));
    (dir, conn)
}

fn seed(conn: &Connection, name: &str, parent: Option<&str>) {
    conn.execute(
        "INSERT OR IGNORE INTO pen (name, kind, parent_id)
         VALUES (?1, 'self', (SELECT id FROM pen WHERE name = ?2))",
        params![name, parent],
    )
    .expect("seed pen");
}

fn set_policy(conn: &Connection, key: &str, gate: &str, grace: i64, clock: &str) {
    conn.execute(
        "UPDATE channel SET gate = ?2, grace_secs = ?3, clock = ?4 WHERE key = ?1",
        params![key, gate, grace, clock],
    )
    .expect("set policy");
}

fn delivered(conn: &Connection, seq: i64, pen: &str) -> (Option<String>, Option<String>) {
    conn.query_row(
        "SELECT d.presented_at, d.fetched_at
           FROM channel_delivery d JOIN pen p ON p.id = d.pen_id
          WHERE d.seq = ?1 AND p.name = ?2",
        params![seq, pen],
        |r| Ok((r.get(0)?, r.get(1)?)),
    )
    .expect("delivery row")
}

// ── FAN-OUT ──────────────────────────────────────────────────────────────────────────────────────

#[test]
fn a_message_is_told_to_every_member_except_its_sender() {
    let (_dir, conn) = store();
    let m = channel::send(&conn, "james", "general", "roll in five", None).expect("send");

    assert_eq!(m.pen, "james");
    // stet and cline were told; the sender and `estate` were not.
    delivered(&conn, m.seq, "stet");
    delivered(&conn, m.seq, "cline");
    let own: i64 = conn
        .query_row(
            "SELECT count(*) FROM channel_delivery d
               JOIN pen p ON p.id = d.pen_id
              WHERE d.seq = ?1 AND p.name = 'james'",
            params![m.seq],
            |r| r.get(0),
        )
        .expect("count");
    assert_eq!(own, 0, "a pen must not be behind on its own words");
}

#[test]
fn fetch_delivers_once_and_history_still_reads() {
    let (_dir, conn) = store();
    channel::send(&conn, "james", "general", "one", None).expect("send");
    channel::send(&conn, "james", "general", "two", None).expect("send");

    let inbox = channel::fetch(&conn, "cline", None, 50).expect("fetch");
    assert_eq!(inbox.len(), 2);
    assert_eq!(inbox[0].body, "one");

    let again = channel::fetch(&conn, "cline", None, 50).expect("fetch again");
    assert!(again.is_empty(), "fetching is listening; it happens once");

    let log = channel::history(&conn, "general", 0, 50).expect("history");
    assert_eq!(log.len(), 2, "history is the log, and is always readable");
}

#[test]
fn a_reply_must_belong_to_the_same_channel() {
    let (_dir, conn) = store();
    let m = channel::send(&conn, "james", "general", "root", None).expect("send");

    let ok = channel::send(&conn, "cline", "general", "re", Some(m.seq)).expect("reply");
    assert_eq!(ok.reply_to, Some(m.seq));

    let err = channel::send(&conn, "cline", "ops", "cross", Some(m.seq)).expect_err("refused");
    assert!(err.to_string().contains("not #ops"), "{err}");
}

// ── THE GATE ─────────────────────────────────────────────────────────────────────────────────────

#[test]
fn being_asleep_is_not_ignoring() {
    let (_dir, conn) = store();
    // `ops` gates on delivery: the clock starts when a turn starts, not when the message was sent.
    set_policy(&conn, "ops", "writes", 0, "delivery");
    channel::send(&conn, "james", "ops", "we are rolling", None).expect("send");

    channel::gate(&conn, "cline", Class::Record).expect("no turn has begun; not a breach");
    assert!(channel::breach(&conn, "cline").expect("breach").is_none());
}

#[test]
fn an_opportunity_that_passes_gates_the_record_and_not_the_conversation() {
    let (_dir, conn) = store();
    set_policy(&conn, "ops", "writes", 0, "delivery");
    channel::send(&conn, "james", "ops", "we are rolling", None).expect("send");

    channel::present(&conn, "cline").expect("present");
    let b = channel::breach(&conn, "cline")
        .expect("breach")
        .expect("a breach");
    assert_eq!(b.channel, "ops");

    let err = channel::gate(&conn, "cline", Class::Record).expect_err("record is gated");
    assert!(err.to_string().contains("#ops"), "{err}");
    assert!(err.to_string().contains("not been fetched"), "{err}");
}

#[test]
fn listening_clears_the_gate() {
    let (_dir, conn) = store();
    set_policy(&conn, "ops", "writes", 0, "delivery");
    channel::send(&conn, "james", "ops", "heads up", None).expect("send");
    channel::present(&conn, "cline").expect("present");
    channel::gate(&conn, "cline", Class::Record).expect_err("gated before the fetch");

    let got = channel::fetch(&conn, "cline", Some("ops"), 50).expect("fetch");
    assert_eq!(got.len(), 1);
    channel::gate(&conn, "cline", Class::Record).expect("fetched, so allowed");
}

#[test]
fn a_send_clock_judges_from_the_message_not_the_turn() {
    let (_dir, conn) = store();
    set_policy(&conn, "ops", "writes", 0, "send");
    channel::send(&conn, "james", "ops", "immediate", None).expect("send");

    // No `present` at all: on a send clock the pen is late from the moment it was told.
    let err = channel::gate(&conn, "cline", Class::Record).expect_err("gated");
    assert!(
        err.to_string().contains("immediate") || err.to_string().contains("#ops"),
        "{err}"
    );
}

#[test]
fn general_never_gates() {
    let (_dir, conn) = store();
    channel::send(&conn, "james", "general", "chatter", None).expect("send");
    channel::present(&conn, "cline").expect("present");

    assert!(channel::breach(&conn, "cline").expect("breach").is_none());
    channel::gate(&conn, "cline", Class::Record).expect("general is a conversation");
}

#[test]
fn writes_does_not_stop_acting_but_all_does() {
    let (_dir, conn) = store();
    set_policy(&conn, "ops", "writes", 0, "delivery");
    channel::send(&conn, "james", "ops", "notice", None).expect("send");
    channel::present(&conn, "cline").expect("present");

    channel::gate(&conn, "cline", Class::Acting).expect("writes leaves acting alone");
    channel::gate(&conn, "cline", Class::Record).expect_err("writes gates the record");

    // Now the hard lock.
    conn.execute("UPDATE channel SET gate = 'all' WHERE key = 'ops'", [])
        .expect("raise gate");
    channel::gate(&conn, "cline", Class::Acting).expect_err("all stops acting too");
}

#[test]
fn an_unreadable_ledger_fails_open() {
    let (_dir, conn) = store();
    // A pen that does not exist has no ledger to read. The gate must not invent a lock it cannot
    // confirm — a false lock is indistinguishable from a broken store.
    channel::gate(&conn, "nobody-at-all", Class::Record).expect("fails open");
}

#[test]
fn the_gate_follows_the_pen_the_effect_lands_on() {
    let (_dir, conn) = store();
    set_policy(&conn, "ops", "writes", 0, "delivery");
    channel::send(&conn, "james", "ops", "roll", None).expect("send");
    channel::present(&conn, "cline").expect("present");

    // `james` was the sender and has no receipt, so `james` is not behind even though `cline` is.
    channel::gate(&conn, "james", Class::Record).expect("the sender is not behind");
    channel::gate(&conn, "cline", Class::Record).expect_err("cline is behind");
}
