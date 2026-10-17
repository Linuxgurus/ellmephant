//! Integration: CLAIMS — the atomic lease over a name (migration `0012`).
//!
//! Runs offline against a real store file. The point of the primitive is that TWO writers cannot both
//! win, so the central test races real connections rather than trusting the SQL by inspection.

mod common;

use std::sync::{Arc, Barrier};

use common::temp_store;
use ellmephant::Result;
use ellmephant::db;
use ellmephant::store::claim;

/// Two writers race one name; exactly one may win. This is the atomicity the primitive exists for —
/// a client-side lease would let both read "free" and both write "mine".
#[test]
fn a_race_has_exactly_one_winner() -> Result<()> {
    // Create the schema once, so the threads below race the CLAIM and not the migration.
    let (_dir, paths) = temp_store();
    drop(db::open(&paths)?);

    const THREADS: usize = 8;
    let barrier = Arc::new(Barrier::new(THREADS));
    let mut handles = Vec::new();
    for i in 0..THREADS {
        let paths = paths.clone();
        let barrier = Arc::clone(&barrier);
        handles.push(std::thread::spawn(move || {
            let conn = db::open(&paths).expect("open");
            let owner = format!("hand-{i}");
            // All threads reach the starting line before any writes, so the attempts overlap.
            barrier.wait();
            claim::acquire(&conn, "bank", &owner, Some(300))
                .expect("acquire")
                .won
        }));
    }
    let wins = handles
        .into_iter()
        .map(|h| h.join().expect("join"))
        .filter(|won| *won)
        .count();
    assert_eq!(wins, 1, "exactly one writer may hold a live lease");
    Ok(())
}

/// The lease lives in the STORE, not in the process that took it: a second connection sees the holder
/// and is refused. This is the cross-machine property a local file lock cannot give.
#[test]
fn a_lease_is_visible_from_another_connection() -> Result<()> {
    let (_dir, paths) = temp_store();
    let a = db::open(&paths)?;
    let b = db::open(&paths)?;

    let won = claim::acquire(&a, "bank", "hand-a", Some(300))?;
    assert!(won.won, "the first connection takes it");

    let seen = claim::get(&b, "bank")?.expect("visible across connections");
    assert_eq!(seen.owner, "hand-a");
    assert!(seen.live);

    let lost = claim::acquire(&b, "bank", "hand-b", Some(300))?;
    assert!(!lost.won, "the second connection is refused");
    assert_eq!(lost.claim.owner, "hand-a", "and told who holds it");
    Ok(())
}
