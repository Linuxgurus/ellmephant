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
