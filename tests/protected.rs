//! Integration: `PROTECTED` — a memory writable only under a live lease.
//!
//! The tag says WHICH rows need a lease; the lease is the control. What is proved here is that the
//! gate closes for a plain write, opens for a holder, attributes a proven writer, and that the same
//! gate covers deletion and the tag itself.

mod common;

use common::temp_store;
use ellmephant::db;
use ellmephant::embed::HashEmbedder;
use ellmephant::store::{self, Memory};
use rusqlite::Connection;

const DIM: usize = 8;

fn seeded_with(key: &str, tags: Vec<String>) -> (tempfile::TempDir, Connection, HashEmbedder) {
    let (dir, paths) = temp_store();
    let conn = db::open(&paths).expect("open");
    let embedder = HashEmbedder::new(DIM);
    let mem = Memory {
        key: key.to_string(),
        title: format!("{key} title"),
        body: format!("{key} body"),
        tags,
        ..Default::default()
    };
    common::add_now(&conn, &embedder, &mem).expect("seed a memory");
    (dir, conn, embedder)
}

fn seeded(key: &str) -> (tempfile::TempDir, Connection, HashEmbedder) {
    seeded_with(key, vec![store::PROTECTED.to_string()])
}

fn patch(key: &str, body: &str, asserted_by: Option<&str>) -> store::Patch {
    store::Patch {
        key: Some(key.to_string()),
        body: Some(body.to_string()),
        asserted_by: asserted_by.map(str::to_string),
        ..Default::default()
    }
}

#[test]
fn a_protected_memory_refuses_a_write_with_no_lease() {
    let (_dir, conn, embedder) = seeded("vault");
    let err = common::merge_now(&conn, &embedder, &patch("vault", "new", None))
        .expect_err("a protected row must refuse an unleased write");
    let msg = err.to_string();
    assert!(
        msg.contains("PROTECTED") && msg.contains("claim_acquire"),
        "the refusal must say what to do: {msg}"
    );
    let body: String = conn
        .query_row("SELECT body FROM memory WHERE key = 'vault'", [], |r| {
            r.get(0)
        })
        .expect("row still there");
    assert_eq!(body, "vault body", "the refused write changed nothing");
}

#[test]
fn holding_the_lease_lets_the_holder_write_it() {
    let (_dir, conn, embedder) = seeded("vault");
    store::claim::acquire(&conn, "vault", "hands-mail-1:1", None).expect("take the lease");

    // An unproven writer that holds the lease may write: the lease IS the statement.
    common::merge_now(&conn, &embedder, &patch("vault", "leased write", None))
        .expect("the holder may write");

    // A PROVEN writer must be the holder.
    let err = common::merge_now(
        &conn,
        &embedder,
        &patch("vault", "someone else", Some("James")),
    )
    .expect_err("a proven non-holder must be refused");
    assert!(err.to_string().contains("does not hold it"), "{err}");

    store::claim::release(&conn, "vault", "hands-mail-1:1").expect("release");
    let body: String = conn
        .query_row("SELECT body FROM memory WHERE key = 'vault'", [], |r| {
            r.get(0)
        })
        .expect("row");
    assert_eq!(body, "leased write");
}

#[test]
fn deleting_a_protected_memory_needs_the_lease_too() {
    let (_dir, conn, _embedder) = seeded("vault");
    assert!(
        store::delete(&conn, "vault", None).is_err(),
        "an unleased delete must be refused"
    );
    store::claim::acquire(&conn, "vault", "hands-mail-1:1", None).expect("lease");
    assert!(store::delete(&conn, "vault", None).expect("leased delete"));
}

#[test]
fn tagging_a_memory_protected_also_needs_a_lease() {
    let (_dir, conn, embedder) = seeded_with("plain", vec!["aspect".to_string()]);

    // Ratcheting a row shut is a write like any other, so it needs the lease.
    let mut ratchet = patch("plain", "plain body", None);
    ratchet.tags = Some(vec!["aspect".to_string(), store::PROTECTED.to_string()]);
    let err = common::merge_now(&conn, &embedder, &ratchet)
        .expect_err("protecting a memory must need a lease");
    assert!(err.to_string().contains("PROTECTED"), "{err}");

    store::claim::acquire(&conn, "plain", "owner", None).expect("lease to protect");
    common::merge_now(&conn, &embedder, &ratchet).expect("under a lease it may be protected");

    let tags: String = conn
        .query_row("SELECT tags FROM memory WHERE key = 'plain'", [], |r| {
            r.get(0)
        })
        .expect("row");
    assert!(tags.contains(store::PROTECTED), "the tag landed: {tags}");
}
