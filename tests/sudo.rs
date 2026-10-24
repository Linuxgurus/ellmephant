//! Integration: SUDO — authority borrowed, bounded, revocable (soma/ellmephant#109, round 6).
//!
//! The tree gives a pen its own subtree by right; anything wider must be borrowed. `estate` is the
//! widest case and the reason the mechanism exists: it is a real pen with no entity behind it, so it
//! can never be a caller — its reach is only ever someone else's borrow.
//!
//! Each test names one check and the failure it prevents.

mod common;

use common::temp_store;

use ellmephant::store::sudo;

/// `estate` → `james` → {`stet`, `cline`} → `stet`'s hand → its finger.
fn tree(conn: &rusqlite::Connection) {
    conn.execute_batch(
        // 0021 seeds `estate`; the tree here is the whole registry, so start from an empty one.
        "DELETE FROM pen;
         INSERT INTO pen (name, kind) VALUES ('estate', 'self');
         INSERT INTO pen (name, kind, parent_id) VALUES ('james', 'self', (SELECT id FROM pen WHERE name='estate'));
         INSERT INTO pen (name, kind, parent_id) VALUES ('stet', 'self', (SELECT id FROM pen WHERE name='james'));
         INSERT INTO pen (name, kind, parent_id) VALUES ('cline', 'self', (SELECT id FROM pen WHERE name='james'));
         INSERT INTO pen (name, kind, parent_id) VALUES ('stet/hand', 'self', (SELECT id FROM pen WHERE name='stet'));
         INSERT INTO pen (name, kind, parent_id) VALUES ('stet/hand/finger-1', 'finger', (SELECT id FROM pen WHERE name='stet/hand'));",
    )
    .expect("pen tree");
}

#[test]
fn a_session_needs_a_policy_it_cannot_assume_one() {
    let (_dir, paths) = temp_store();
    let conn = ellmephant::db::open(&paths).unwrap();
    tree(&conn);

    let err = sudo::sudo(&conn, "stet", "estate", 60).expect_err("no policy, no borrow");
    assert!(
        format!("{err}").contains("no sudo policy"),
        "authority is granted, never assumed: {err}"
    );
}

#[test]
fn a_session_cannot_outlive_the_policy_that_permits_it() {
    let (_dir, paths) = temp_store();
    let conn = ellmephant::db::open(&paths).unwrap();
    tree(&conn);
    sudo::grant(&conn, "estate", "stet", 60, "test").unwrap();

    let err = sudo::sudo(&conn, "stet", "estate", 120).expect_err("120s > 60s ceiling");
    assert!(format!("{err}").contains("at most 60s"), "{err}");

    // And the ceiling is within the ceiling: a policy is not a way around the cap.
    let err = sudo::grant(&conn, "estate", "james", 999_999, "test").unwrap_err();
    assert!(format!("{err}").contains("86400"), "{err}");

    let held = sudo::sudo(&conn, "stet", "estate", 60).expect("inside the ceiling");
    assert_eq!(held.powerful, "estate");
    assert_eq!(held.weak, "stet");
    assert!(
        held.via.is_none(),
        "a first session was passed from nothing"
    );
    assert!(sudo::live(&conn, "stet", "estate").unwrap().is_some());
}

#[test]
fn a_pen_cannot_borrow_from_itself() {
    let (_dir, paths) = temp_store();
    let conn = ellmephant::db::open(&paths).unwrap();
    tree(&conn);

    let err = sudo::grant(&conn, "stet", "stet", 60, "test").unwrap_err();
    assert!(format!("{err}").contains("itself"), "{err}");
    let err = sudo::sudo(&conn, "stet", "stet", 60).unwrap_err();
    assert!(format!("{err}").contains("itself"), "{err}");
}

#[test]
fn an_expired_session_stops_counting_the_moment_it_expires() {
    let (_dir, paths) = temp_store();
    let conn = ellmephant::db::open(&paths).unwrap();
    tree(&conn);
    sudo::grant(&conn, "estate", "stet", 60, "test").unwrap();
    sudo::sudo(&conn, "stet", "estate", 60).unwrap();

    // No sleeping: move the clock in the row, which is the same fact from the other side.
    conn.execute(
        "UPDATE active_sudo SET expires_at = datetime('now', '-1 second')",
        [],
    )
    .unwrap();
    assert!(
        sudo::live(&conn, "stet", "estate").unwrap().is_none(),
        "an expired session is not a narrow permission, it is none"
    );
    assert!(sudo::held(&conn, "stet").unwrap().is_empty());
}

#[test]
fn a_session_is_passed_down_never_sideways_or_up() {
    let (_dir, paths) = temp_store();
    let conn = ellmephant::db::open(&paths).unwrap();
    tree(&conn);
    sudo::grant(&conn, "estate", "stet", 600, "test").unwrap();
    sudo::sudo(&conn, "stet", "estate", 600).unwrap();

    let handed = sudo::pass(&conn, "stet", "estate", "stet/hand", 300).expect("down to a child");
    assert_eq!(handed.weak, "stet/hand");
    assert!(
        handed.via.is_some(),
        "a passed session names where it came from"
    );

    let err = sudo::pass(&conn, "stet", "estate", "james", 60).unwrap_err();
    assert!(
        format!("{err}").contains("not a descendant"),
        "up is not down: {err}"
    );

    let err = sudo::pass(&conn, "stet", "estate", "cline", 60).unwrap_err();
    assert!(
        format!("{err}").contains("not a descendant"),
        "a peer is not a child: {err}"
    );

    // And one who holds nothing can lend nothing.
    let err = sudo::pass(&conn, "cline", "estate", "cline/x", 60).unwrap_err();
    assert!(format!("{err}").contains("holds no live"), "{err}");
}

#[test]
fn a_passed_session_cannot_outlive_the_one_it_came_from() {
    let (_dir, paths) = temp_store();
    let conn = ellmephant::db::open(&paths).unwrap();
    tree(&conn);
    sudo::grant(&conn, "estate", "stet", 600, "test").unwrap();
    sudo::sudo(&conn, "stet", "estate", 60).unwrap();

    let err = sudo::pass(&conn, "stet", "estate", "stet/hand", 300).unwrap_err();
    assert!(format!("{err}").contains("cannot outlive"), "{err}");

    sudo::pass(&conn, "stet", "estate", "stet/hand", 30).expect("inside what is left");
}

#[test]
fn releasing_a_session_kills_everything_it_passed_down() {
    let (_dir, paths) = temp_store();
    let conn = ellmephant::db::open(&paths).unwrap();
    tree(&conn);
    sudo::grant(&conn, "estate", "stet", 600, "test").unwrap();
    sudo::sudo(&conn, "stet", "estate", 600).unwrap();
    sudo::pass(&conn, "stet", "estate", "stet/hand", 300).unwrap();
    sudo::pass(&conn, "stet/hand", "estate", "stet/hand/finger-1", 120).expect("down again");
    assert!(!sudo::held(&conn, "stet/hand/finger-1").unwrap().is_empty());

    // The hand lets go: the finger's session existed only because of it, so it goes too. A
    // revocation that leaves a copy running is not a revocation.
    let killed = sudo::release(&conn, "stet", "estate").unwrap();
    assert_eq!(killed, 3, "the session, the hand's, and the finger's");
    assert!(sudo::held(&conn, "stet/hand").unwrap().is_empty());
    assert!(sudo::held(&conn, "stet/hand/finger-1").unwrap().is_empty());
}

#[test]
fn revoking_the_policy_kills_the_sessions_it_authorised() {
    let (_dir, paths) = temp_store();
    let conn = ellmephant::db::open(&paths).unwrap();
    tree(&conn);
    sudo::grant(&conn, "estate", "stet", 600, "test").unwrap();
    sudo::sudo(&conn, "stet", "estate", 600).unwrap();

    let killed = sudo::revoke(&conn, "estate", "stet").unwrap();
    assert_eq!(killed, 1);
    assert!(
        sudo::live(&conn, "stet", "estate").unwrap().is_none(),
        "a policy removed while its sessions ran would be permission wearing a tombstone"
    );
    assert!(sudo::sudo(&conn, "stet", "estate", 60).is_err());
}

#[test]
fn a_boot_spec_applies_what_it_can_and_names_what_it_cannot() {
    let (_dir, paths) = temp_store();
    let conn = ellmephant::db::open(&paths).unwrap();
    tree(&conn);

    let spec =
        "estate>stet:600, estate>james:3600, estate->cline:60, estate>stet:99999, , nonsense";
    let (loaded, rejected) = sudo::load(&conn, spec, "configmap");

    assert_eq!(loaded, 2, "two entries are usable");
    assert_eq!(
        rejected.len(),
        3,
        "the rest are named, not swallowed: {rejected:?}"
    );
    assert!(
        rejected.iter().any(|r| r.contains("99999")),
        "a bad duration is rejected, not clamped: {rejected:?}"
    );
    assert!(sudo::live(&conn, "stet", "estate").unwrap().is_none());

    // The loading is a policy write; borrowing still needs the verb.
    sudo::sudo(&conn, "james", "estate", 3600).expect("the good entries landed");
}

#[test]
fn a_lend_sets_the_deadline_and_never_stacks_one() {
    let (_dir, paths) = temp_store();
    let conn = ellmephant::db::open(&paths).unwrap();
    tree(&conn);

    // A long lend, then a short one for the same pair: the SHORT one is the truth.
    sudo::lend(&conn, "stet", "stet/hand", 600).unwrap();
    let short = sudo::lend(&conn, "stet", "stet/hand", 30).unwrap();
    let live = sudo::live(&conn, "stet/hand", "stet")
        .unwrap()
        .expect("live");
    assert_eq!(live.id, short.id, "the newest lend is the one that governs");

    // ★ THE HAZARD THIS EXISTS FOR: if the long row survived, the short lend would expire and then
    // the 600s one would silently let the finger write again. Push the clock past the short lend and
    // past nothing else — permission must be GONE, not resurrected.
    conn.execute(
        "UPDATE active_sudo SET expires_at = datetime('now', '-1 second') WHERE id = ?1",
        rusqlite::params![short.id],
    )
    .unwrap();
    assert!(
        sudo::live(&conn, "stet/hand", "stet").unwrap().is_none(),
        "a shortened lend must not come back when it lapses"
    );

    // And re-lending ends anything passed down from the lend it replaces: a child may not outlive
    // the grant it was passed under.
    sudo::lend(&conn, "stet", "stet/hand", 600).unwrap();
    sudo::pass(&conn, "stet/hand", "stet", "stet/hand/finger-1", 60).expect("pass down");
    assert!(!sudo::held(&conn, "stet/hand/finger-1").unwrap().is_empty());
    sudo::lend(&conn, "stet", "stet/hand", 300).unwrap();
    assert!(
        sudo::held(&conn, "stet/hand/finger-1").unwrap().is_empty(),
        "the finger's session was passed from the lend that just ended"
    );
}

#[test]
fn retiring_a_pen_ends_the_authority_it_was_given() {
    let (_dir, paths) = temp_store();
    let conn = ellmephant::db::open(&paths).unwrap();
    tree(&conn);

    // A lend to the hand, and one passed further down: both exist because the hand does.
    sudo::lend(&conn, "stet", "stet/hand", 600).unwrap();
    sudo::pass(&conn, "stet/hand", "stet", "stet/hand/finger-1", 300).expect("pass down");
    assert!(!sudo::held(&conn, "stet/hand").unwrap().is_empty());

    // ★ DISPOSAL INCLUDES THE AUTHORITY (found reviewing #126): retiring the hand must end its lend
    // AND the finger's passed session, or a retired pen keeps acting until the TTL lapses.
    ellmephant::store::retire(
        &conn,
        "stet/hand",
        "stet",
        ellmephant::store::Rows::Inherit,
        ellmephant::store::History::Memorialize,
    )
    .expect("retire");

    assert!(
        sudo::held(&conn, "stet/hand").unwrap().is_empty(),
        "the hand's lend is gone"
    );
    assert!(
        sudo::held(&conn, "stet/hand/finger-1").unwrap().is_empty(),
        "and the session passed down from it"
    );
}

#[test]
fn authority_cannot_be_lent_to_or_credited_for_a_ceased_pen() {
    let (_dir, paths) = temp_store();
    let conn = ellmephant::db::open(&paths).unwrap();
    tree(&conn);
    sudo::lend(&conn, "stet", "stet/hand", 600).unwrap();
    ellmephant::store::retire(
        &conn,
        "stet/hand",
        "stet",
        ellmephant::store::Rows::Inherit,
        ellmephant::store::History::Memorialize,
    )
    .expect("retire");

    // Lending to a pen that has ceased is lending to nobody.
    let err = sudo::lend(&conn, "stet", "stet/hand", 60).unwrap_err();
    assert!(format!("{err}").contains("retired"), "{err}");

    // And it cannot be credited for new work, whatever rows survive.
    let err = ellmephant::store::may_act_as(&conn, "stet", "stet/hand").unwrap_err();
    assert!(
        format!("{err}").contains("ceased pen cannot be credited"),
        "{err}"
    );
}

#[test]
fn borrowing_again_replaces_the_session_instead_of_stacking_one() {
    let (_dir, paths) = temp_store();
    let conn = ellmephant::db::open(&paths).unwrap();
    tree(&conn);
    sudo::grant(&conn, "estate", "stet", 3600, "test").unwrap();

    let long = sudo::sudo(&conn, "stet", "estate", 600).unwrap();
    let short = sudo::sudo(&conn, "stet", "estate", 30).unwrap();
    assert_eq!(
        sudo::live(&conn, "stet", "estate").unwrap().unwrap().id,
        short.id,
        "the newest borrow governs"
    );

    // The hazard is the same one `lend` had: if the long row survived, shortening a borrow would
    // silently restore the longer grant when the short one lapsed.
    conn.execute(
        "UPDATE active_sudo SET expires_at = datetime('now', '-1 second') WHERE id = ?1",
        rusqlite::params![short.id],
    )
    .unwrap();
    assert!(
        sudo::live(&conn, "stet", "estate").unwrap().is_none(),
        "a shortened borrow must not come back when it lapses (the 600s row was {})",
        long.id
    );
}

#[test]
fn a_session_records_why_it_exists() {
    let (_dir, paths) = temp_store();
    let conn = ellmephant::db::open(&paths).unwrap();
    tree(&conn);

    // A LEND: a pen spending its own authority downward. No policy row exists for the pair.
    let lent = sudo::lend(&conn, "stet", "stet/hand", 60).expect("lend");
    assert_eq!(lent.source, "lend");
    let policies: i64 = conn
        .query_row("SELECT count(*) FROM sudoers", [], |r| r.get(0))
        .unwrap();
    assert_eq!(policies, 0, "a lend needs no policy, and writes none");

    // A BORROW: someone else's authority, under a policy.
    sudo::grant(&conn, "estate", "stet", 600, "test").unwrap();
    let borrowed = sudo::sudo(&conn, "stet", "estate", 60).expect("sudo");
    assert_eq!(borrowed.source, "policy");

    // A PASSED session carries the authority it was passed from, however far down it travels.
    let passed = sudo::pass(&conn, "stet", "estate", "stet/hand", 30).expect("pass");
    assert_eq!(passed.source, "policy", "a borrow stays a borrow");
    sudo::lend(&conn, "stet/hand", "stet/hand/finger-1", 30).expect("lend");
    let lent = sudo::live(&conn, "stet/hand/finger-1", "stet/hand")
        .unwrap()
        .unwrap();
    assert_eq!(lent.source, "lend");

    // And the question the column exists to answer, without a join that can lose its subject:
    let kinds: Vec<String> = conn
        .prepare("SELECT DISTINCT source FROM active_sudo ORDER BY source")
        .unwrap()
        .query_map([], |r| r.get(0))
        .unwrap()
        .collect::<rusqlite::Result<_>>()
        .unwrap();
    assert_eq!(kinds, vec!["lend".to_string(), "policy".to_string()]);
}

#[test]
fn a_lender_may_end_its_fingers_session_but_a_peer_may_not() {
    let (_dir, paths) = temp_store();
    let conn = ellmephant::db::open(&paths).unwrap();
    tree(&conn);

    // stet lends to its hand; the hand passes it to its finger.
    sudo::lend(&conn, "stet", "stet/hand", 600).unwrap();
    sudo::pass(&conn, "stet/hand", "stet", "stet/hand/finger-1", 300).expect("pass down");
    assert!(!sudo::held(&conn, "stet/hand/finger-1").unwrap().is_empty());

    // ★ A PEER MAY NOT (found reviewing #127): cline is not the finger's ancestor.
    let err = sudo::release_by(&conn, "cline", "stet/hand/finger-1", "stet").unwrap_err();
    assert!(
        format!("{err}").contains("only that pen or one of its ancestors"),
        "{err}"
    );

    // But the ancestor CAN, and the whole chain went with raising the hand's session — here the
    // finger's own session, which the finger could never end itself: it has no credential.
    let killed =
        sudo::release_by(&conn, "stet/hand", "stet/hand/finger-1", "stet").expect("release");
    assert_eq!(killed, 1);
    assert!(sudo::held(&conn, "stet/hand/finger-1").unwrap().is_empty());
}

#[test]
fn a_ceased_pen_can_neither_borrow_nor_be_borrowed_from_and_its_policy_dies_with_it() {
    let (_dir, paths) = temp_store();
    let conn = ellmephant::db::open(&paths).unwrap();
    tree(&conn);
    sudo::grant(&conn, "estate", "stet/hand", 600, "test").unwrap();

    ellmephant::store::retire(
        &conn,
        "stet/hand",
        "stet",
        ellmephant::store::Rows::Inherit,
        ellmephant::store::History::Memorialize,
    )
    .expect("retire");

    // The policy went with the pen: no ghost authority left to borrow.
    let policies: i64 = conn
        .query_row("SELECT count(*) FROM sudoers", [], |r| r.get(0))
        .unwrap();
    assert_eq!(
        policies, 0,
        "a retired pen's policy is authority with no pen behind it"
    );

    // And the request is refused by name rather than by its absence.
    let err = sudo::sudo(&conn, "stet/hand", "estate", 60).unwrap_err();
    assert!(format!("{err}").contains("retired"), "{err}");
}
