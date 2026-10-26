//! Ellmechat's ledger: fan-out, membership, listening, and the gate (#151).
//!
//! The rules these tests exist to protect, in James's words: the gate is a seatbelt — the unread
//! message is the news a pen needs before it acts — and it is the pen the effect is attributed to
//! that carries it.

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

/// Rewrite a channel's policy. Explicit on every axis so a test reads as the rule it checks.
fn set_policy(
    conn: &Connection,
    key: &str,
    gate: &str,
    grace: i64,
    clock: &str,
    misses_to_gate: i64,
    lock_max_secs: i64,
) {
    conn.execute(
        "UPDATE channel
            SET gate = ?2, grace_secs = ?3, clock = ?4,
                misses_to_gate = ?5, lock_max_secs = ?6
          WHERE key = ?1",
        params![key, gate, grace, clock, misses_to_gate, lock_max_secs],
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

fn was_told(conn: &Connection, seq: i64, pen: &str) -> bool {
    let n: i64 = conn
        .query_row(
            "SELECT count(*) FROM channel_delivery d JOIN pen p ON p.id = d.pen_id
              WHERE d.seq = ?1 AND p.name = ?2",
            params![seq, pen],
            |r| r.get(0),
        )
        .expect("count");
    n > 0
}

fn backdate_presentation(conn: &Connection, seq: i64, pen: &str, minutes: i64) {
    conn.execute(
        "UPDATE channel_delivery
            SET presented_at = datetime('now', ?3)
          WHERE seq = ?1 AND pen_id = (SELECT id FROM pen WHERE name = ?2)",
        params![seq, pen, format!("-{minutes} minutes")],
    )
    .expect("backdate");
}

// ── FAN-OUT ──────────────────────────────────────────────────────────────────────────────────────

#[test]
fn a_message_is_told_to_every_member_except_its_sender() {
    let (_dir, conn) = store();
    let m = channel::send(&conn, "james", "general", "roll in five", None).expect("send");

    assert_eq!(m.pen, "james");
    delivered(&conn, m.seq, "stet");
    delivered(&conn, m.seq, "cline");

    assert!(
        !was_told(&conn, m.seq, "james"),
        "a pen is not behind on its own words"
    );
    assert!(
        !was_told(&conn, m.seq, "estate"),
        "estate cannot authenticate to read"
    );
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

    let err = channel::send(&conn, "cline", "changes", "cross", Some(m.seq)).expect_err("refused");
    assert!(err.to_string().contains("not #changes"), "{err}");
}

// ── MEMBERSHIP ───────────────────────────────────────────────────────────────────────────────────

#[test]
fn an_open_channel_can_be_left_and_that_is_the_only_way_out() {
    let (_dir, conn) = store();
    channel::leave(&conn, "cline", "general").expect("leave");
    let m = channel::send(&conn, "james", "general", "anyone?", None).expect("send");

    assert!(!was_told(&conn, m.seq, "cline"), "opted out");
    assert!(was_told(&conn, m.seq, "stet"), "still in by default");

    channel::join(&conn, "cline", "general").expect("rejoin");
    let m2 = channel::send(&conn, "james", "general", "back?", None).expect("send");
    assert!(was_told(&conn, m2.seq, "cline"));
}

#[test]
fn a_list_channel_reaches_only_its_members() {
    let (_dir, conn) = store();
    // `#ops_team` is `list` and seeded with nobody on a fresh store: the test seeds membership.
    channel::join(&conn, "cline", "ops_team").expect("join");
    let m = channel::send(&conn, "james", "ops_team", "internal", None).expect("send");

    assert!(was_told(&conn, m.seq, "cline"));
    assert!(!was_told(&conn, m.seq, "stet"), "not invited, not told");
}

#[test]
fn joining_the_ops_team_joins_emergency() {
    let (_dir, conn) = store();
    channel::join(&conn, "cline", "ops_team").expect("join");

    let m = channel::send(&conn, "james", "emergency", "cluster down", None).expect("send");
    assert!(
        was_told(&conn, m.seq, "cline"),
        "ops membership implies the incident room"
    );
}

#[test]
fn leaving_emergency_while_on_the_team_is_refused() {
    let (_dir, conn) = store();
    channel::join(&conn, "cline", "ops_team").expect("join");

    let err = channel::leave(&conn, "cline", "emergency").expect_err("refused");
    assert!(err.to_string().contains("ops_team"), "{err}");

    channel::leave(&conn, "cline", "ops_team").expect("leave the team");
    channel::leave(&conn, "cline", "emergency").expect("now it is allowed");
}

// ── THE GATE ─────────────────────────────────────────────────────────────────────────────────────

#[test]
fn being_asleep_is_not_ignoring() {
    let (_dir, conn) = store();
    channel::join(&conn, "cline", "ops_team").expect("join");
    // `ops_team` gates on delivery: the clock starts at a turn boundary, not at the message.
    set_policy(&conn, "ops_team", "writes", 0, "delivery", 1, 0);
    channel::send(&conn, "james", "ops_team", "we are rolling", None).expect("send");

    channel::gate(&conn, "cline", Class::Record).expect("no turn has begun; not a breach");
    assert!(channel::breach(&conn, "cline").expect("breach").is_none());
}

#[test]
fn one_missed_boundary_is_not_a_pattern() {
    let (_dir, conn) = store();
    channel::join(&conn, "cline", "ops_team").expect("join");
    set_policy(&conn, "ops_team", "writes", 0, "delivery", 2, 0);
    channel::send(&conn, "james", "ops_team", "we are rolling", None).expect("send");

    channel::present(&conn, "cline").expect("first boundary");
    channel::gate(&conn, "cline", Class::Record)
        .expect("a message that arrived mid-turn is not a crime");
}

#[test]
fn two_missed_boundaries_lock_the_record_and_not_the_conversation() {
    let (_dir, conn) = store();
    channel::join(&conn, "cline", "ops_team").expect("join");
    set_policy(&conn, "ops_team", "writes", 0, "delivery", 2, 0);
    channel::send(&conn, "james", "ops_team", "we are rolling", None).expect("send");

    channel::present(&conn, "cline").expect("first");
    channel::present(&conn, "cline").expect("second");

    let b = channel::breach(&conn, "cline")
        .expect("breach")
        .expect("a breach");
    assert_eq!(b.channel, "ops_team");
    assert_eq!(b.misses, 2);

    let err = channel::gate(&conn, "cline", Class::Record).expect_err("record is gated");
    assert!(err.to_string().contains("#ops_team"), "{err}");
    assert!(err.to_string().contains("has not been fetched"), "{err}");
}

#[test]
fn an_emergency_locks_after_a_single_boundary() {
    let (_dir, conn) = store();
    set_policy(&conn, "emergency", "all", 0, "delivery", 1, 0);
    channel::send(&conn, "james", "emergency", "cluster down in five", None).expect("send");

    channel::present(&conn, "cline").expect("boundary");
    channel::gate(&conn, "cline", Class::Record).expect_err("record gated");
    channel::gate(&conn, "cline", Class::Acting).expect_err("acting gated too");
}

#[test]
fn listening_clears_the_gate() {
    let (_dir, conn) = store();
    channel::join(&conn, "cline", "ops_team").expect("join");
    set_policy(&conn, "ops_team", "writes", 0, "delivery", 1, 0);
    channel::send(&conn, "james", "ops_team", "heads up", None).expect("send");
    channel::present(&conn, "cline").expect("present");
    channel::gate(&conn, "cline", Class::Record).expect_err("gated before the fetch");

    let got = channel::fetch(&conn, "cline", Some("ops_team"), 50).expect("fetch");
    assert_eq!(got.len(), 1);
    channel::gate(&conn, "cline", Class::Record).expect("fetched, so allowed");
}

#[test]
fn a_send_clock_judges_from_the_message_not_the_boundary() {
    let (_dir, conn) = store();
    channel::join(&conn, "cline", "ops_team").expect("join");
    set_policy(&conn, "ops_team", "writes", 0, "send", 1, 0);
    let m = channel::send(&conn, "james", "ops_team", "immediate", None).expect("send");
    conn.execute(
        "UPDATE channel_delivery SET available_at = datetime('now', '-10 minutes') WHERE seq = ?1",
        params![m.seq],
    )
    .expect("backdate");
    channel::present(&conn, "cline").expect("boundary");

    let b = channel::breach(&conn, "cline")
        .expect("breach")
        .expect("breach");
    assert_eq!(b.seq, m.seq);
    assert!(
        b.age_secs >= 600,
        "the clock must start at available_at, got {}",
        b.age_secs
    );
}

#[test]
fn an_old_message_still_locks_when_the_opportunity_is_new() {
    let (_dir, conn) = store();
    set_policy(&conn, "emergency", "all", 0, "send", 1, 60);
    let m = channel::send(&conn, "james", "emergency", "incident at 02:00", None).expect("send");
    // The message is an hour old, the boundary is now. The lock's expiry must count from the
    // opportunity, not the message: otherwise the pen wakes, is told the incident, and is never
    // gated because the message "aged out" before it opened its eyes.
    conn.execute(
        "UPDATE channel_delivery SET available_at = datetime('now', '-60 minutes') WHERE seq = ?1",
        params![m.seq],
    )
    .expect("backdate");
    channel::present(&conn, "cline").expect("boundary");

    let b = channel::breach(&conn, "cline")
        .expect("breach")
        .expect("breach");
    assert!(b.age_secs >= 3600, "the message is old");
    assert!(b.lock_age_secs < 60, "the lock is new: {}", b.lock_age_secs);
    channel::gate(&conn, "cline", Class::Record).expect_err("a fresh opportunity locks");
}

#[test]
fn a_lock_expires_and_says_so() {
    let (_dir, conn) = store();
    channel::join(&conn, "cline", "ops_team").expect("join");
    set_policy(&conn, "ops_team", "writes", 0, "delivery", 1, 60);
    let m = channel::send(&conn, "james", "ops_team", "an hour ago", None).expect("send");
    channel::present(&conn, "cline").expect("boundary");
    channel::gate(&conn, "cline", Class::Record).expect_err("locked");

    backdate_presentation(&conn, m.seq, "cline", 2);
    channel::gate(&conn, "cline", Class::Record).expect("expired locks release");
}

#[test]
fn changes_never_gates() {
    let (_dir, conn) = store();
    channel::send(&conn, "james", "changes", "rolling in 30", None).expect("send");
    channel::present(&conn, "cline").expect("present");
    channel::present(&conn, "cline").expect("present again");

    assert!(channel::breach(&conn, "cline").expect("breach").is_none());
    channel::gate(&conn, "cline", Class::Record).expect("changes is information");
}

#[test]
fn general_never_gates() {
    let (_dir, conn) = store();
    channel::send(&conn, "james", "general", "chatter", None).expect("send");
    channel::present(&conn, "cline").expect("present");
    channel::present(&conn, "cline").expect("present again");

    assert!(channel::breach(&conn, "cline").expect("breach").is_none());
    channel::gate(&conn, "cline", Class::Record).expect("general is a conversation");
}

#[test]
fn writes_does_not_stop_acting_but_all_does() {
    let (_dir, conn) = store();
    channel::join(&conn, "cline", "ops_team").expect("join");
    set_policy(&conn, "ops_team", "writes", 0, "delivery", 1, 0);
    channel::send(&conn, "james", "ops_team", "notice", None).expect("send");
    channel::present(&conn, "cline").expect("present");

    channel::gate(&conn, "cline", Class::Acting).expect("writes leaves acting alone");
    channel::gate(&conn, "cline", Class::Record).expect_err("writes gates the record");

    conn.execute("UPDATE channel SET gate = 'all' WHERE key = 'ops_team'", [])
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
    channel::join(&conn, "james", "ops_team").expect("join");
    channel::join(&conn, "cline", "ops_team").expect("join");
    set_policy(&conn, "ops_team", "writes", 0, "delivery", 1, 0);
    channel::send(&conn, "stet", "ops_team", "roll", None).expect("send");
    channel::present(&conn, "cline").expect("present");

    // `stet` was the sender and has no receipt, so `stet` is not behind even though `cline` is.
    channel::gate(&conn, "stet", Class::Record).expect("the sender is not behind");
    channel::gate(&conn, "cline", Class::Record).expect_err("cline is behind");
}

// ── THE DOOR'S COUNT ─────────────────────────────────────────────────────────────────────────────

#[test]
fn unread_counts_what_was_told_and_only_fetching_clears_it() {
    let (_dir, conn) = store();
    channel::send(&conn, "james", "general", "one", None).expect("send");
    channel::send(&conn, "james", "general", "two", None).expect("send");

    let before = channel::unread(&conn, "cline").expect("unread");
    assert_eq!(before.len(), 1);
    assert_eq!(before[0].channel, "general");
    assert_eq!(before[0].count, 2);
    assert_eq!(
        before[0].oldest_seq + 1,
        before[0].latest_seq,
        "the range names the oldest and newest unread message"
    );

    channel::history(&conn, "general", 0, 50).expect("history");
    let after_history = channel::unread(&conn, "cline").expect("unread");
    assert_eq!(
        after_history[0].count, 2,
        "reading the log is not listening: only fetch moves a receipt"
    );

    channel::fetch(&conn, "cline", None, 50).expect("fetch");
    assert!(
        channel::unread(&conn, "cline").expect("unread").is_empty(),
        "fetching is listening, so the count empties"
    );

    assert!(
        !channel::unread(&conn, "james")
            .expect("unread")
            .iter()
            .any(|u| u.channel == "general"),
        "a pen is not behind on its own words"
    );
}

#[test]
fn since_returns_what_arrived_after_a_cursor_without_moving_a_receipt() {
    let (_dir, conn) = store();
    let one = channel::send(&conn, "james", "general", "one", None).expect("send");
    let two = channel::send(&conn, "james", "general", "two", None).expect("send");

    let after_one = channel::since(&conn, "cline", one.seq, 50).expect("since");
    assert_eq!(after_one.len(), 1, "only what arrived after the cursor");
    assert_eq!(after_one[0].seq, two.seq);

    let none = channel::since(&conn, "cline", two.seq, 50).expect("since");
    assert!(none.is_empty(), "nothing after the head");

    // ★ a WAIT is not a LISTEN: the delivery rows are untouched, so the pen is still behind.
    let still_unread = channel::unread(&conn, "cline").expect("unread");
    assert_eq!(still_unread[0].count, 2);
}
