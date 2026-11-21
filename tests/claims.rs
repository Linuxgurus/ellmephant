mod common;

use std::sync::{Arc, Barrier};

use common::temp_store;
use ellmephant::Result;
use ellmephant::db;
use ellmephant::store::claim;

#[test]
fn a_race_has_exactly_one_winner() -> Result<()> {
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

#[test]
fn names_and_owners_are_case_insensitive() -> Result<()> {
    let (_dir, paths) = temp_store();
    let a = db::open(&paths)?;

    let won = claim::acquire(&a, "bank", "hand-a", Some(300))?;
    assert!(won.won);
    assert_eq!(won.claim.name, "bank", "the name is stored folded");
    assert_eq!(won.claim.owner, "hand-a");

    let renew = claim::acquire(&a, "Bank", "Hand-A", Some(300))?;
    assert!(renew.won, "the holder renews through a different spelling");
    assert_eq!(renew.claim.name, "bank");
    assert_eq!(renew.claim.owner, "hand-a");

    let other = claim::acquire(&a, "BANK", "hand-b", Some(300))?;
    assert!(
        !other.won,
        "a different owner of the same spelling is refused"
    );
    assert_eq!(other.claim.owner, "hand-a");

    let found = claim::get(&a, "Bank")?.expect("case-folded lookup finds it");
    assert_eq!(found.owner, "hand-a");
    Ok(())
}

#[test]
fn acquired_at_survives_a_renewal() -> Result<()> {
    let (_dir, paths) = temp_store();
    let conn = db::open(&paths)?;
    claim::acquire(&conn, "bank", "hand-a", Some(300))?;

    conn.execute(
        "UPDATE claim SET acquired_at = '2020-01-01 00:00:00' WHERE name = 'bank'",
        [],
    )?;
    let renewed = claim::acquire(&conn, "bank", "hand-a", Some(600))?;
    assert!(renewed.won);
    assert_eq!(
        renewed.claim.acquired_at, "2020-01-01 00:00:00",
        "a renewal must not reset the first acquisition"
    );
    Ok(())
}

#[test]
fn a_release_racing_an_acquire_never_errors() -> Result<()> {
    let (_dir, paths) = temp_store();
    drop(db::open(&paths)?);

    const ROUNDS: usize = 2_000;
    let holder = {
        let paths = paths.clone();
        std::thread::spawn(move || -> Result<()> {
            let conn = db::open(&paths)?;
            for _ in 0..ROUNDS {
                claim::acquire(&conn, "bank", "hand-a", Some(300))?;
                claim::release(&conn, "bank", "hand-a")?;
            }
            Ok(())
        })
    };

    let contender = db::open(&paths)?;
    for _ in 0..ROUNDS {
        claim::acquire(&contender, "bank", "hand-b", Some(300))?;
    }
    holder.join().expect("join")?;
    Ok(())
}
