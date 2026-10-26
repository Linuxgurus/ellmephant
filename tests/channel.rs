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
    channel::invite(&conn, "james", "james", "ops_team").expect("sender is a member");
    // `#ops_team` is `list` and seeded with nobody on a fresh store: the test seeds membership.
    channel::invite(&conn, "james", "cline", "ops_team").expect("invite");
    let m = channel::send(&conn, "james", "ops_team", "internal", None).expect("send");

    assert!(was_told(&conn, m.seq, "cline"));
    assert!(!was_told(&conn, m.seq, "stet"), "not invited, not told");
}

#[test]
fn joining_the_ops_team_joins_emergency() {
    let (_dir, conn) = store();
    channel::invite(&conn, "james", "cline", "ops_team").expect("invite");

    let m = channel::send(&conn, "james", "emergency", "cluster down", None).expect("send");
    assert!(
        was_told(&conn, m.seq, "cline"),
        "ops membership implies the incident room"
    );
}

#[test]
fn leaving_emergency_while_on_the_team_is_refused() {
    let (_dir, conn) = store();
    channel::invite(&conn, "james", "cline", "ops_team").expect("invite");

    let err = channel::leave(&conn, "cline", "emergency").expect_err("refused");
    assert!(err.to_string().contains("ops_team"), "{err}");

    channel::leave(&conn, "cline", "ops_team").expect("leave the team");
    channel::leave(&conn, "cline", "emergency").expect("now it is allowed");
}

// ── THE GATE ─────────────────────────────────────────────────────────────────────────────────────

#[test]
fn being_asleep_is_not_ignoring() {
    let (_dir, conn) = store();
    channel::invite(&conn, "james", "james", "ops_team").expect("sender is a member");
    channel::invite(&conn, "james", "cline", "ops_team").expect("invite");
    // `ops_team` gates on delivery: the clock starts at a turn boundary, not at the message.
    set_policy(&conn, "ops_team", "writes", 0, "delivery", 1, 0);
    channel::invite(&conn, "james", "james", "ops_team").expect("sender is a member");
    channel::send(&conn, "james", "ops_team", "we are rolling", None).expect("send");

    channel::gate(&conn, "cline", Class::Record).expect("no turn has begun; not a breach");
    assert!(channel::breach(&conn, "cline").expect("breach").is_none());
}

#[test]
fn one_missed_boundary_is_not_a_pattern() {
    let (_dir, conn) = store();
    channel::invite(&conn, "james", "james", "ops_team").expect("sender is a member");
    channel::invite(&conn, "james", "cline", "ops_team").expect("invite");
    set_policy(&conn, "ops_team", "writes", 0, "delivery", 2, 0);
    channel::invite(&conn, "james", "james", "ops_team").expect("sender is a member");
    channel::send(&conn, "james", "ops_team", "we are rolling", None).expect("send");

    channel::present(&conn, "cline", None).expect("first boundary");
    channel::gate(&conn, "cline", Class::Record)
        .expect("a message that arrived mid-turn is not a crime");
}

#[test]
fn two_missed_boundaries_lock_the_record_and_not_the_conversation() {
    let (_dir, conn) = store();
    channel::invite(&conn, "james", "james", "ops_team").expect("sender is a member");
    channel::invite(&conn, "james", "cline", "ops_team").expect("invite");
    set_policy(&conn, "ops_team", "writes", 0, "delivery", 2, 0);
    channel::invite(&conn, "james", "james", "ops_team").expect("sender is a member");
    channel::send(&conn, "james", "ops_team", "we are rolling", None).expect("send");

    channel::present(&conn, "cline", None).expect("first");
    channel::present(&conn, "cline", None).expect("second");

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

    channel::present(&conn, "cline", None).expect("boundary");
    channel::gate(&conn, "cline", Class::Record).expect_err("record gated");
    channel::gate(&conn, "cline", Class::Acting).expect_err("acting gated too");
}

#[test]
fn listening_clears_the_gate() {
    let (_dir, conn) = store();
    channel::invite(&conn, "james", "james", "ops_team").expect("sender is a member");
    channel::invite(&conn, "james", "cline", "ops_team").expect("invite");
    set_policy(&conn, "ops_team", "writes", 0, "delivery", 1, 0);
    channel::invite(&conn, "james", "james", "ops_team").expect("sender is a member");
    channel::send(&conn, "james", "ops_team", "heads up", None).expect("send");
    channel::present(&conn, "cline", None).expect("present");
    channel::gate(&conn, "cline", Class::Record).expect_err("gated before the fetch");

    let got = channel::fetch(&conn, "cline", Some("ops_team"), 50).expect("fetch");
    assert_eq!(got.len(), 1);
    channel::gate(&conn, "cline", Class::Record).expect("fetched, so allowed");
}

#[test]
fn a_send_clock_judges_from_the_message_not_the_boundary() {
    let (_dir, conn) = store();
    channel::invite(&conn, "james", "james", "ops_team").expect("sender is a member");
    channel::invite(&conn, "james", "cline", "ops_team").expect("invite");
    set_policy(&conn, "ops_team", "writes", 0, "send", 1, 0);
    let m = channel::send(&conn, "james", "ops_team", "immediate", None).expect("send");
    conn.execute(
        "UPDATE channel_delivery SET available_at = datetime('now', '-10 minutes') WHERE seq = ?1",
        params![m.seq],
    )
    .expect("backdate");
    channel::present(&conn, "cline", None).expect("boundary");

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
    channel::present(&conn, "cline", None).expect("boundary");

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
    channel::invite(&conn, "james", "james", "ops_team").expect("sender is a member");
    channel::invite(&conn, "james", "cline", "ops_team").expect("invite");
    set_policy(&conn, "ops_team", "writes", 0, "delivery", 1, 60);
    let m = channel::send(&conn, "james", "ops_team", "an hour ago", None).expect("send");
    channel::present(&conn, "cline", None).expect("boundary");
    channel::gate(&conn, "cline", Class::Record).expect_err("locked");

    backdate_presentation(&conn, m.seq, "cline", 2);
    channel::gate(&conn, "cline", Class::Record).expect("expired locks release");
}

#[test]
fn changes_never_gates() {
    let (_dir, conn) = store();
    channel::send(&conn, "james", "changes", "rolling in 30", None).expect("send");
    channel::present(&conn, "cline", None).expect("present");
    channel::present(&conn, "cline", None).expect("present again");

    assert!(channel::breach(&conn, "cline").expect("breach").is_none());
    channel::gate(&conn, "cline", Class::Record).expect("changes is information");
}

#[test]
fn general_never_gates() {
    let (_dir, conn) = store();
    channel::send(&conn, "james", "general", "chatter", None).expect("send");
    channel::present(&conn, "cline", None).expect("present");
    channel::present(&conn, "cline", None).expect("present again");

    assert!(channel::breach(&conn, "cline").expect("breach").is_none());
    channel::gate(&conn, "cline", Class::Record).expect("general is a conversation");
}

#[test]
fn writes_does_not_stop_acting_but_all_does() {
    let (_dir, conn) = store();
    channel::invite(&conn, "james", "james", "ops_team").expect("sender is a member");
    channel::invite(&conn, "james", "cline", "ops_team").expect("invite");
    set_policy(&conn, "ops_team", "writes", 0, "delivery", 1, 0);
    channel::invite(&conn, "james", "james", "ops_team").expect("sender is a member");
    channel::send(&conn, "james", "ops_team", "notice", None).expect("send");
    channel::present(&conn, "cline", None).expect("present");

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
    channel::invite(&conn, "james", "stet", "ops_team").expect("sender is a member");
    channel::join(&conn, "james", "ops_team").expect("join");
    channel::invite(&conn, "james", "cline", "ops_team").expect("invite");
    set_policy(&conn, "ops_team", "writes", 0, "delivery", 1, 0);
    channel::invite(&conn, "james", "stet", "ops_team").expect("sender is a member");
    channel::send(&conn, "stet", "ops_team", "roll", None).expect("send");
    channel::present(&conn, "cline", None).expect("present");

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

#[test]
fn the_roster_is_effective_membership_not_override_rows() {
    let (_dir, conn) = store();
    // `general` is open: every live self, minus whoever left on purpose.
    let open = channel::members(&conn, "general").expect("members");
    let names: Vec<&str> = open.iter().map(|m| m.pen.as_str()).collect();
    assert!(
        names.contains(&"stet") && names.contains(&"cline"),
        "{names:?}"
    );
    assert!(!names.contains(&"estate"), "estate is never a member");
    assert!(open.iter().all(|m| m.state == "in"));

    channel::leave(&conn, "stet", "general").expect("leave");
    let after = channel::members(&conn, "general").expect("members");
    assert!(
        !after.iter().any(|m| m.pen == "stet"),
        "leaving an open channel removes the pen from its roster"
    );

    // `ops_team` is a list: only invitations, and joining it brings `emergency` with it.
    let empty = channel::members(&conn, "ops_team").expect("members");
    assert!(empty.is_empty(), "nobody is on the team until invited");
    channel::invite(&conn, "james", "cline", "ops_team").expect("invite");
    let team = channel::members(&conn, "ops_team").expect("members");
    assert_eq!(team.len(), 1);
    assert!(team[0].explicit, "a list member is always explicit");
    assert!(
        channel::members(&conn, "emergency")
            .expect("members")
            .iter()
            .any(|m| m.pen == "cline"),
        "joining the team joins emergency: a team that runs the cluster must be reachable"
    );
}

// ── ADMINISTRATION (#161) ────────────────────────────────────────────────────────────────────────

#[test]
fn a_locked_channel_needs_a_keeper_and_the_change_is_a_row() {
    let (_dir, conn) = store();

    // An ordinary pen may make a room; it may not make a LOCK.
    let made = channel::create(&conn, "stet", "builds", "Build notices", "open", "none")
        .expect("an open channel costs nobody anything");
    assert_eq!(made.gate, "none");

    let refused = channel::create(&conn, "stet", "quiet", "Quiet", "list", "writes")
        .expect_err("a lock needs a name on it");
    let msg = refused.to_string();
    assert!(msg.contains("estate") && msg.contains("james"), "{msg}");

    // James may, and creating writes the first event.
    channel::create(&conn, "james", "quiet", "Quiet", "list", "writes").expect("james may lock");
    let events = channel::events(&conn, "quiet").expect("events");
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].action, "create");
    assert_eq!(events[0].pen.as_deref(), Some("james"));

    // Changing the gate is a row too: who, from what, to what.
    channel::set_gate(&conn, "james", "quiet", "all").expect("re-gate");
    let events = channel::events(&conn, "quiet").expect("events");
    assert_eq!(events.len(), 2);
    assert_eq!(events[1].action, "gate");
    assert_eq!(events[1].was.as_deref(), Some("writes"));
    assert_eq!(events[1].now.as_deref(), Some("all"));

    // And a pen that is not a keeper cannot re-gate, with a refusal that says why.
    let err = channel::set_gate(&conn, "stet", "quiet", "none").expect_err("no keeper, no gate");
    assert!(err.to_string().contains("refused"), "{err}");

    // A key is created once, and a bad key is refused before anything is written.
    assert!(channel::create(&conn, "james", "quiet", "Again", "open", "none").is_err());
    assert!(channel::create(&conn, "james", "Bad Key!", "x", "open", "none").is_err());
}

// ── RETENTION (#163) ─────────────────────────────────────────────────────────────────────────────

#[test]
fn pruning_expires_the_text_and_keeps_the_receipt() {
    let (_dir, conn) = store();
    let m = channel::send(&conn, "james", "general", "the cluster is on fire", None).expect("send");
    // Give the message an age, and the channel a retention.
    conn.execute(
        "UPDATE channel_message SET created = datetime('now','-2 hours') WHERE seq = ?1",
        [m.seq],
    )
    .expect("age");
    channel::set_retention(&conn, "james", "general", 3600).expect("retention");

    let pruned = channel::prune(&conn, None, None).expect("prune");
    assert!(
        pruned
            .iter()
            .any(|p| p.channel == "general" && p.emptied == 1),
        "{pruned:?}"
    );

    let (body, pruned_at): (String, Option<String>) = conn
        .query_row(
            "SELECT body, pruned_at FROM channel_message WHERE seq = ?1",
            [m.seq],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .expect("row survives");
    assert!(body.is_empty(), "the TEXT is gone");
    assert!(pruned_at.is_some(), "and it says so");

    // ★ THE RECEIPT SURVIVES: the pen was told, and that is still answerable.
    assert!(
        channel::unread(&conn, "cline")
            .expect("unread")
            .iter()
            .any(|u| u.channel == "general" && u.count == 1),
        "pruning text must not erase the delivery"
    );
    assert_eq!(
        channel::history(&conn, "general", 0, 50)
            .expect("history")
            .len(),
        1,
        "the log line stays; only its body is empty"
    );

    // A channel left at 0 is never touched, and retention is keeper-only.
    let kept = channel::create(&conn, "james", "keepme", "Keep", "open", "none").expect("create");
    assert_eq!(kept.keep_secs, 0);
    assert!(channel::set_retention(&conn, "stet", "keepme", 60).is_err());
}

// ── REVIEW FIXES ────────────────────────────────────────────────────────────────────────────────

#[test]
fn a_non_member_cannot_speak_in_an_invite_only_channel() {
    let (_dir, conn) = store();
    let err = channel::send(&conn, "stet", "ops_team", "let me in", None)
        .expect_err("an outsider may not post into an incident channel");
    assert!(err.to_string().contains("not a member"), "{err}");

    // Invited, it is allowed — membership is the admission rule, not a fan-out detail.
    channel::invite(&conn, "james", "stet", "ops_team").expect("invite");
    channel::send(&conn, "stet", "ops_team", "now I am here", None).expect("member may speak");
}

#[test]
fn a_pen_cannot_add_itself_to_a_list_channel() {
    let (_dir, conn) = store();
    let err = channel::join(&conn, "cline", "ops_team").expect_err("invite-only means invited");
    assert!(err.to_string().contains("invite-only"), "{err}");

    // The keeper can, and that is the invitation.
    channel::invite(&conn, "james", "cline", "ops_team").expect("keeper invites");
    assert!(
        channel::members(&conn, "ops_team")
            .expect("members")
            .iter()
            .any(|m| m.pen == "cline")
    );
}

#[test]
fn one_boundary_named_twice_is_one_miss() {
    let (_dir, conn) = store();
    channel::invite(&conn, "james", "james", "ops_team").expect("sender is a member");
    channel::invite(&conn, "james", "cline", "ops_team").expect("invite");
    channel::send(&conn, "james", "ops_team", "news", None).expect("send");

    channel::present(&conn, "cline", Some("task-7")).expect("boundary");
    channel::present(&conn, "cline", Some("task-7")).expect("the same boundary again");
    // ★ ASSERT THE LEDGER, NOT THE ABSENCE OF A BREACH (#168 review): `breach == None` also passes
    // when `present` recorded nothing at all, which is the regression this test exists to catch.
    let misses: i64 = conn
        .query_row(
            "SELECT misses FROM channel_delivery
              WHERE pen_id = (SELECT id FROM pen WHERE name='cline')
                AND seq = (SELECT MAX(seq) FROM channel_message WHERE channel='ops_team')",
            [],
            |r| r.get(0),
        )
        .expect("delivery row");
    assert_eq!(misses, 1, "one boundary, however many times it is named");

    // A genuinely new boundary still counts.
    channel::present(&conn, "cline", Some("task-8")).expect("next boundary");
}

#[test]
fn an_expired_lock_is_recorded_as_a_row() {
    let (_dir, conn) = store();
    channel::invite(&conn, "james", "cline", "emergency").expect("invite");
    channel::send(&conn, "james", "emergency", "down", None).expect("send");
    channel::present(&conn, "cline", None).expect("boundary");
    // emergency: grace 0, one boundary, lock_max 900s — age it past the ceiling.
    conn.execute(
        "UPDATE channel_delivery SET presented_at = datetime('now','-2 hours')
          WHERE pen_id = (SELECT id FROM pen WHERE name='cline')",
        [],
    )
    .expect("age");
    channel::gate(&conn, "cline", Class::Record).expect("an expired lock releases");
    let (misses, presented): (i64, Option<String>) = conn
        .query_row(
            "SELECT misses, presented_at FROM channel_delivery
              WHERE pen_id = (SELECT id FROM pen WHERE name='cline')",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .expect("delivery");
    assert_eq!(misses, 0, "a release clears the opportunity");
    assert!(
        presented.is_none(),
        "and the clock stops, or it expires again"
    );

    // A second gated call must NOT write a second expiry: the lock is over.
    channel::gate(&conn, "cline", Class::Record).expect("still open");
    let expiries = channel::events(&conn, "emergency")
        .expect("events")
        .into_iter()
        .filter(|e| e.action == "expiry")
        .count();
    assert_eq!(expiries, 1, "one release, one row");
}

#[test]
fn pruning_matches_the_channel_case_insensitively_and_keeps_a_floor() {
    let (_dir, conn) = store();
    let m = channel::send(&conn, "james", "general", "text", None).expect("send");
    conn.execute(
        "UPDATE channel_message SET created = datetime('now','-2 hours') WHERE seq = ?1",
        [m.seq],
    )
    .expect("age");
    channel::set_retention(&conn, "james", "general", 3600).expect("retention");
    let pruned = channel::prune(&conn, Some("GENERAL"), None).expect("prune");
    assert_eq!(
        pruned.iter().map(|p| p.emptied).sum::<usize>(),
        1,
        "case must not matter"
    );

    // A unit mistake is refused: `60` is a minute, not an hour.
    assert!(channel::set_retention(&conn, "james", "general", 60).is_err());
    assert!(channel::set_retention(&conn, "james", "general", 0).is_ok());
}

#[test]
fn the_roster_excludes_estate_and_the_store_refuses_a_case_variant() {
    // ★ The `lower()` in the roster query is defence, not the only lock: pen names are unique
    // regardless of case and `estate` is seeded at store creation, so a second spelling cannot exist.
    // The test pins BOTH facts, because the day the uniqueness rule changes the roster must still hold.
    let (dir, paths) = temp_store();
    let conn = db::open(&paths).expect("open");
    let err = conn
        .execute("INSERT INTO pen (name, kind) VALUES ('Estate', 'self')", [])
        .expect_err("a case variant of estate must be refused");
    assert!(
        err.to_string().contains("unique regardless of case"),
        "{err}"
    );
    let names: Vec<String> = channel::members(&conn, "general")
        .expect("members")
        .into_iter()
        .map(|m| m.pen)
        .collect();
    assert!(
        !names.iter().any(|n| n.eq_ignore_ascii_case("estate")),
        "estate has no entity behind it and is never a member: {names:?}"
    );
    let _ = dir;
}

// ── THIRD PASS: THE FIXES' OWN EDGES ────────────────────────────────────────────────────────────

#[test]
fn a_boundary_token_stops_working_once_it_is_stale() {
    let (_dir, conn) = store();
    channel::invite(&conn, "james", "james", "ops_team").expect("sender is a member");
    channel::invite(&conn, "james", "cline", "ops_team").expect("invite");
    channel::send(&conn, "james", "ops_team", "news", None).expect("send");

    channel::present(&conn, "cline", Some("turn")).expect("boundary");
    // Same token, but the window has passed: it names a NEW opportunity, not the old one.
    conn.execute(
        "UPDATE channel_boundary SET at = datetime('now','-10 minutes')
          WHERE pen_id = (SELECT id FROM pen WHERE name='cline')",
        [],
    )
    .expect("age the boundary");
    channel::present(&conn, "cline", Some("turn")).expect("a new opportunity");
    let misses: i64 = conn
        .query_row(
            "SELECT MAX(misses) FROM channel_delivery
              WHERE pen_id = (SELECT id FROM pen WHERE name='cline')",
            [],
            |r| r.get(0),
        )
        .expect("delivery");
    assert_eq!(misses, 2, "a reused token cannot buy permanent immunity");
}

#[test]
fn a_finger_cannot_be_invited_to_a_channel() {
    let (_dir, conn) = store();
    conn.execute(
        "INSERT INTO pen (name, kind, parent_id) VALUES ('stet/f-1', 'finger', (SELECT id FROM pen WHERE name='stet'))",
        [],
    )
    .expect("seed a finger");
    let err = channel::invite(&conn, "james", "stet/f-1", "ops_team")
        .expect_err("a finger is a mechanism, not a member");
    assert!(
        err.to_string().contains("never be a channel member"),
        "{err}"
    );
    assert!(
        channel::members(&conn, "ops_team")
            .expect("members")
            .is_empty()
    );
}

#[test]
fn only_a_keeper_may_make_an_invite_only_room_and_its_creator_is_in_it() {
    let (_dir, conn) = store();
    assert!(
        channel::create(&conn, "stet", "hidden", "Hidden", "list", "none").is_err(),
        "invite-only is a policy decision like a gate"
    );
    channel::create(&conn, "james", "hidden", "Hidden", "list", "none").expect("keeper may");
    assert!(
        channel::members(&conn, "hidden")
            .expect("members")
            .iter()
            .any(|m| m.pen == "james"),
        "the creator is in the room they made"
    );
    channel::send(&conn, "james", "hidden", "hello", None).expect("and may speak in it");
}

#[test]
fn leaving_a_channel_ends_its_hold() {
    let (_dir, conn) = store();
    channel::invite(&conn, "james", "james", "ops_team").expect("sender is a member");
    channel::invite(&conn, "james", "cline", "ops_team").expect("invite");
    channel::send(&conn, "james", "ops_team", "unread incident", None).expect("send");
    channel::send(&conn, "james", "ops_team", "second", None).expect("send");
    channel::present(&conn, "cline", None).expect("boundary");
    channel::present(&conn, "cline", None).expect("boundary");
    // The grace runs from the FIRST OPPORTUNITY, so age the opportunity, not the message.
    conn.execute(
        "UPDATE channel_delivery SET presented_at = datetime('now','-5 minutes')
          WHERE pen_id = (SELECT id FROM pen WHERE name='cline')",
        [],
    )
    .expect("age the opportunity");
    assert!(channel::breach(&conn, "cline").expect("breach").is_some());

    // Leaving ends the obligation: the delivery is kept, but it no longer gates.
    channel::leave(&conn, "cline", "ops_team").expect("leave");
    assert!(
        channel::breach(&conn, "cline").expect("breach").is_none(),
        "a pen must not be locked by a room it has left"
    );
    let kept: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM channel_delivery
              WHERE pen_id = (SELECT id FROM pen WHERE name='cline')",
            [],
            |r| r.get(0),
        )
        .expect("count");
    assert!(
        kept > 0,
        "the receipt survives: you were told, even if you left"
    );
}

#[test]
fn a_waiter_with_no_cursor_starts_at_the_head_and_a_typo_is_not_success() {
    let (_dir, conn) = store();
    channel::send(&conn, "james", "general", "old news", None).expect("send");
    let head = channel::head(&conn, "cline").expect("head");
    assert!(head > 0, "the head is the newest seq this pen was told");
    assert!(
        channel::since(&conn, "cline", head, 50)
            .expect("since")
            .is_empty(),
        "from the head, nothing has arrived yet"
    );

    let err = channel::prune(&conn, Some("no-such-channel"), None)
        .expect_err("a typo is not 'nothing to prune'");
    assert!(err.to_string().contains("no channel named"), "{err}");
}
