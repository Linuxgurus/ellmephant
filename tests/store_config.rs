mod common;

use common::temp_store;

use ellmephant::store::audience::Reader;
use ellmephant::store::config;
use sha2::Digest;

fn tree(conn: &rusqlite::Connection) {
    conn.execute_batch(
        "DELETE FROM pen;
         INSERT INTO pen (name, kind) VALUES ('estate', 'self');
         INSERT INTO pen (name, kind, parent_id) VALUES ('james', 'self', (SELECT id FROM pen WHERE name='estate'));
         INSERT INTO pen (name, kind, parent_id) VALUES ('stet', 'self', (SELECT id FROM pen WHERE name='james'));
         INSERT INTO pen (name, kind, parent_id) VALUES ('cline', 'self', (SELECT id FROM pen WHERE name='james'));
         INSERT INTO pen (name, kind, parent_id) VALUES ('stet/hand', 'self', (SELECT id FROM pen WHERE name='stet'));",
    )
    .expect("pen tree");
}

fn reader(conn: &rusqlite::Connection, name: &str) -> Reader {
    Reader::pen(name).in_tree(conn).with_borrowed(conn)
}

#[test]
fn an_item_is_absent_when_it_is_not_addressed_to_you() {
    let (_dir, paths) = temp_store();
    let conn = ellmephant::db::open(&paths).unwrap();
    tree(&conn);

    let james = reader(&conn, "james");
    config::define(
        &conn,
        &james,
        Some("james"),
        "recess_daily_time",
        "2h",
        "pen:james",
        "pen:stet",
        "James raised the daily recess",
    )
    .expect("an ancestor may address its own child");

    let stet = reader(&conn, "stet");
    let item = config::get(&conn, &stet, "recess_daily_time")
        .unwrap()
        .expect("stet is addressed by name");
    assert_eq!(item.value, "2h");
    assert_eq!(item.owner.as_deref(), Some("james"));

    let cline = reader(&conn, "cline");
    assert!(
        config::get(&conn, &cline, "recess_daily_time")
            .unwrap()
            .is_none(),
        "absence, not a refusal: a sibling must not learn the item exists"
    );
    assert!(
        config::list(&conn, &cline, Some("recess_daily_time"))
            .unwrap()
            .is_empty(),
        "the list omits what the reader may not see"
    );
    assert_eq!(
        config::list(&conn, &stet, Some("recess_daily_time"))
            .unwrap()
            .len(),
        1
    );
}

#[test]
fn set_is_gated_by_the_written_audience() {
    let (_dir, paths) = temp_store();
    let conn = ellmephant::db::open(&paths).unwrap();
    tree(&conn);

    config::define(
        &conn,
        &reader(&conn, "james"),
        Some("james"),
        "recess_daily_grant",
        "1h",
        "pen:james",
        "pen:stet",
        "who may raise a grant",
    )
    .unwrap();

    let err = config::set(
        &conn,
        &reader(&conn, "stet"),
        "stet",
        "recess_daily_grant",
        "9h",
        None,
    )
    .expect_err("stet may read it but not change it");
    assert!(format!("{err}").contains("may not set"), "{err}");

    let changed = config::set(
        &conn,
        &reader(&conn, "james"),
        "james",
        "recess_daily_grant",
        "90m",
        Some("James adjusted it"),
    )
    .unwrap();
    assert_eq!(changed.value, "90m");
    assert_eq!(changed.owner.as_deref(), Some("james"));
    assert_eq!(changed.source, "James adjusted it");
}

#[test]
fn define_refuses_an_address_the_caller_cannot_reach() {
    let (_dir, paths) = temp_store();
    let conn = ellmephant::db::open(&paths).unwrap();
    tree(&conn);

    let err = config::define(
        &conn,
        &reader(&conn, "stet"),
        Some("stet"),
        "shared",
        "x",
        "pen:stet",
        "pen:cline",
        "a share across a sibling boundary",
    )
    .expect_err("no amplification: a sibling is not a share");
    assert!(format!("{err}").contains("may not name"), "{err}");

    config::define(
        &conn,
        &reader(&conn, "stet"),
        Some("stet"),
        "hand-only",
        "y",
        "pen:stet",
        "pen:stet/hand",
        "stet's own hand",
    )
    .unwrap();

    config::define(
        &conn,
        &Reader::everything(),
        None,
        "estate-wide",
        "z",
        "pen:estate",
        "subtree:estate",
        "provisioning",
    )
    .unwrap();
}

#[test]
fn set_audience_may_not_be_public() {
    let (_dir, paths) = temp_store();
    let conn = ellmephant::db::open(&paths).unwrap();
    tree(&conn);

    let err = config::define(
        &conn,
        &Reader::everything(),
        None,
        "free-for-all",
        "x",
        "public",
        "public",
        "nobody owns it",
    )
    .expect_err("a value anyone can write is not configuration");
    assert!(format!("{err}").contains("may not be `public`"), "{err}");
}

#[test]
fn an_existing_items_policy_is_not_rewritten_by_a_non_writer() {
    let (_dir, paths) = temp_store();
    let conn = ellmephant::db::open(&paths).unwrap();
    tree(&conn);

    config::define(
        &conn,
        &reader(&conn, "james"),
        Some("james"),
        "locked",
        "v",
        "pen:james",
        "public",
        "only James writes it",
    )
    .unwrap();

    let err = config::define(
        &conn,
        &reader(&conn, "stet"),
        Some("stet"),
        "locked",
        "v2",
        "pen:stet",
        "public",
        "stet claims it",
    )
    .expect_err("reading a policy is not a licence to rewrite it");
    assert!(format!("{err}").contains("may not redefine"), "{err}");
}

#[test]
fn write_implies_read_the_setter_is_told_what_it_wrote() {
    let (_dir, paths) = temp_store();
    let conn = ellmephant::db::open(&paths).unwrap();
    tree(&conn);

    config::define(
        &conn,
        &Reader::everything(),
        None,
        "maintained",
        "old",
        "pen:stet",
        "pen:james",
        "who maintains and who reads are different pens",
    )
    .unwrap();

    let stet = reader(&conn, "stet");
    assert!(
        config::get(&conn, &stet, "maintained").unwrap().is_none(),
        "the reader boundary holds for a pen that is not a reader"
    );

    let written = config::set(&conn, &stet, "stet", "maintained", "new", None).unwrap();
    assert_eq!(written.value, "new", "a writer learns the value it wrote");
    assert_eq!(
        written.read_audience, "pen:james",
        "the row says who may READ it, and that is still only James"
    );
    assert!(
        config::get(&conn, &stet, "maintained").unwrap().is_none(),
        "the write did not widen the read boundary"
    );
}

#[test]
fn set_cannot_invent_an_item() {
    let (_dir, paths) = temp_store();
    let conn = ellmephant::db::open(&paths).unwrap();
    tree(&conn);

    let err = config::set(
        &conn,
        &Reader::everything(),
        "james",
        "never-defined",
        "v",
        None,
    )
    .expect_err("a value with no written ACL would be whoever writes it first");
    assert!(format!("{err}").contains("define it first"), "{err}");
}

#[test]
fn unset_removes_the_row_and_the_absence_is_the_requested_state() {
    let (_dir, paths) = temp_store();
    let conn = ellmephant::db::open(&paths).unwrap();
    tree(&conn);

    config::define(
        &conn,
        &reader(&conn, "james"),
        Some("james"),
        "recess_daily_grant",
        "1h",
        "pen:james",
        "pen:stet",
        "who may raise a grant",
    )
    .unwrap();

    let removed = config::unset(
        &conn,
        &reader(&conn, "james"),
        "james",
        "recess_daily_grant",
    )
    .expect("the writer may retire its own item")
    .expect("it existed");
    assert_eq!(removed.key, "recess_daily_grant");
    assert_eq!(removed.value, "1h", "the caller is told what went");

    assert!(
        config::get(&conn, &reader(&conn, "james"), "recess_daily_grant")
            .unwrap()
            .is_none()
    );
    assert!(
        config::list(&conn, &reader(&conn, "james"), None)
            .unwrap()
            .is_empty(),
        "an unset item is gone from the list too, not hidden"
    );
    assert!(
        config::unset(
            &conn,
            &reader(&conn, "james"),
            "james",
            "recess_daily_grant"
        )
        .unwrap()
        .is_none(),
        "unsetting an absent item is the state you asked for, not an error"
    );
}

#[test]
fn unset_is_gated_by_the_written_audience() {
    let (_dir, paths) = temp_store();
    let conn = ellmephant::db::open(&paths).unwrap();
    tree(&conn);

    config::define(
        &conn,
        &reader(&conn, "james"),
        Some("james"),
        "recess_daily_grant",
        "1h",
        "pen:james",
        "pen:stet",
        "only James may raise or retire it",
    )
    .unwrap();

    let err = config::unset(&conn, &reader(&conn, "stet"), "stet", "recess_daily_grant")
        .expect_err("reading a grant is not a licence to remove it");
    assert!(format!("{err}").contains("may not unset"), "{err}");
    assert!(
        config::get(&conn, &reader(&conn, "stet"), "recess_daily_grant")
            .unwrap()
            .is_some(),
        "a refused unset leaves the row exactly where it was"
    );
}

#[test]
fn a_subtree_grant_can_be_withdrawn_by_the_subtree_it_addresses() {
    let (_dir, paths) = temp_store();
    let conn = ellmephant::db::open(&paths).unwrap();
    tree(&conn);

    config::define(
        &conn,
        &reader(&conn, "james"),
        Some("james"),
        "hand_policy",
        "x",
        "subtree:stet",
        "pen:james",
        "stet's hands maintain their own policy",
    )
    .unwrap();

    let removed = config::unset(
        &conn,
        &reader(&conn, "stet/hand"),
        "stet/hand",
        "hand_policy",
    )
    .expect("a subtree grant is the widest write boundary, and removal follows the same gate")
    .expect("it existed");
    assert_eq!(removed.key, "hand_policy");
    assert!(
        config::get(&conn, &reader(&conn, "james"), "hand_policy")
            .unwrap()
            .is_none()
    );
}

#[test]
fn key_folding_applies_to_unset_as_to_get() {
    let (_dir, paths) = temp_store();
    let conn = ellmephant::db::open(&paths).unwrap();
    tree(&conn);

    config::define(
        &conn,
        &Reader::everything(),
        None,
        "Recess_Daily_Time",
        "2h",
        "pen:estate",
        "public",
        "provisioning",
    )
    .unwrap();

    assert!(
        config::unset(&conn, &Reader::everything(), "estate", "recess_daily_time")
            .unwrap()
            .is_some(),
        "a key is an address: the fold that finds it also removes it"
    );
}

#[test]
fn keys_fold_case_and_refuse_whitespace() {
    let (_dir, paths) = temp_store();
    let conn = ellmephant::db::open(&paths).unwrap();
    tree(&conn);

    config::define(
        &conn,
        &Reader::everything(),
        None,
        "Recess_Daily_Time",
        "2h",
        "pen:estate",
        "public",
        "provisioning",
    )
    .unwrap();

    let item = config::get(&conn, &Reader::anonymous(), "recess_daily_time")
        .unwrap()
        .expect("case is folded: two spellings are one item");
    assert_eq!(item.key, "recess_daily_time");

    let err = config::normalize_key("two words").expect_err("a key is an address, not prose");
    assert!(format!("{err}").contains("whitespace"), "{err}");
}

#[test]
fn an_empty_value_is_refused() {
    let (_dir, paths) = temp_store();
    let conn = ellmephant::db::open(&paths).unwrap();
    tree(&conn);

    let err = config::define(
        &conn,
        &Reader::everything(),
        None,
        "blank",
        "   ",
        "pen:estate",
        "public",
        "provisioning",
    )
    .expect_err("an empty value is one more value to parse");
    assert!(format!("{err}").contains("may not be empty"), "{err}");
}

#[test]
fn an_audience_naming_no_pen_is_refused() {
    let (_dir, paths) = temp_store();
    let conn = ellmephant::db::open(&paths).unwrap();
    tree(&conn);

    let err = config::define(
        &conn,
        &Reader::everything(),
        None,
        "ghost",
        "v",
        "pen:estate",
        "pen:nobody",
        "provisioning",
    )
    .expect_err("an audience naming no pen is an item nobody can read");
    assert!(format!("{err}").contains("names no pen"), "{err}");
}

#[test]
fn unset_leaves_a_durable_record_of_what_it_removed_without_the_value() {
    let (_dir, paths) = temp_store();
    let conn = ellmephant::db::open(&paths).unwrap();
    tree(&conn);

    config::define(
        &conn,
        &reader(&conn, "james"),
        Some("james"),
        "recess_daily_grant",
        "1h",
        "pen:james",
        "pen:stet",
        "only James may raise or retire it",
    )
    .unwrap();

    let removed = config::unset(&conn, &reader(&conn, "james"), "james", "recess_daily_grant")
        .unwrap()
        .expect("james holds the boundary");
    assert_eq!(removed.value, "1h");

    let expected = format!("{:x}", sha2::Sha256::digest(b"1h"));

    let row = conn
        .query_row(
            "SELECT e.action, e.actor, e.old_set_audience, e.old_read_audience,
                    p.name, e.old_updated_at, e.value_sha256
               FROM config_event e LEFT JOIN pen p ON p.id = e.old_owner
              WHERE e.key = 'recess_daily_grant'",
            [],
            |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, String>(2)?,
                    r.get::<_, String>(3)?,
                    r.get::<_, Option<String>>(4)?,
                    r.get::<_, String>(5)?,
                    r.get::<_, String>(6)?,
                ))
            },
        )
        .expect("an unset writes a config_event row");
    assert_eq!(row.0, "unset");
    assert_eq!(row.1, "james");
    assert_eq!(row.2, "pen:james");
    assert_eq!(row.3, "pen:stet");
    assert_eq!(row.4.as_deref(), Some("james"));
    assert!(!row.5.is_empty(), "the old updated_at is part of the record");
    assert_eq!(row.6, expected, "the record carries a hash, not the value");
}

#[test]
fn unset_releases_the_key_name_to_whoever_defines_it_next() {
    // Documented and deliberate: `unset` removes the row and reserves NOTHING, so absence is a
    // CREATE for any pen the key does not gate. The description says so; this test is what that
    // sentence means, so a later tombstone has to break a test on purpose.
    let (_dir, paths) = temp_store();
    let conn = ellmephant::db::open(&paths).unwrap();
    tree(&conn);

    config::define(
        &conn,
        &reader(&conn, "james"),
        Some("james"),
        "hand_policy",
        "x",
        "pen:james",
        "pen:stet",
        "James owns this row",
    )
    .unwrap();
    config::unset(&conn, &reader(&conn, "james"), "james", "hand_policy").unwrap();

    let reclaimed = config::define(
        &conn,
        &reader(&conn, "cline"),
        Some("cline"),
        "hand_policy",
        "mine now",
        "pen:cline",
        "public",
        "the name was free",
    )
    .expect("absence is a CREATE: unset reserves no name");
    assert_eq!(reclaimed.owner.as_deref(), Some("cline"));
}
