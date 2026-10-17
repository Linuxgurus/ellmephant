//! Integration: the MAILBOX — addressed rows, a per-reader cursor, and letters that expire.
//!
//! Runs offline against a real store file. The assertions that matter are the ones a green tick would
//! hide: a letter with no `seq` regression (the cursor is a number, not a clock), an inbox that sees
//! only what is after the cursor, and a body that is NOT copied into an envelope.

mod common;

use common::temp_store;
use ellmephant::store::mail;
use ellmephant::{Result, db};

#[test]
fn a_letter_is_stored_addressed_and_stamped() -> Result<()> {
    let (_dir, paths) = temp_store();
    let conn = db::open(&paths)?;

    let sent = mail::send(
        &conn,
        "stet",
        "Twine",
        "the claim race",
        "I think the retry is wrong. Object?",
        "",
    )?;
    assert!(sent.seq > 0, "a letter gets a cursor");
    assert!(!sent.at.is_empty(), "and a stamp");
    assert_eq!(sent.sender, "stet");
    assert_eq!(sent.subject, "the claim race");
    assert_eq!(sent.encoding, "text", "encoding defaults to text");
    assert_eq!(
        sent.bytes,
        "I think the retry is wrong. Object?".len() as i64
    );

    // The box is addressed case-insensitively, so a name is one box however it is typed.
    let inbox = mail::inbox(&conn, "twine", 0, 50)?;
    assert_eq!(inbox.len(), 1);
    assert_eq!(inbox[0].seq, sent.seq);
    Ok(())
}

#[test]
fn the_cursor_shows_only_what_is_after_it() -> Result<()> {
    let (_dir, paths) = temp_store();
    let conn = db::open(&paths)?;

    let first = mail::send(&conn, "stet", "twine", "one", "first", "text")?;
    let second = mail::send(&conn, "stet", "twine", "two", "second", "text")?;
    // Another box, to prove boxes do not leak into each other.
    let _other = mail::send(&conn, "stet", "james", "elsewhere", "not twine's", "text")?;

    let all = mail::inbox(&conn, "twine", 0, 50)?;
    assert_eq!(all.len(), 2, "only twine's box");
    assert_eq!(all[0].seq, first.seq, "oldest first");
    assert_eq!(all[1].seq, second.seq);

    let unread = mail::inbox(&conn, "twine", first.seq, 50)?;
    assert_eq!(unread.len(), 1, "everything after the cursor");
    assert_eq!(unread[0].seq, second.seq);

    assert_eq!(
        mail::head(&conn, "twine")?,
        second.seq,
        "head is the newest"
    );
    assert_eq!(
        mail::head(&conn, "nobody")?,
        0,
        "an empty box is 0, not an error"
    );
    Ok(())
}

#[test]
fn the_body_is_fetched_on_demand_never_copied_into_an_envelope() -> Result<()> {
    let (_dir, paths) = temp_store();
    let conn = db::open(&paths)?;
    let body = "a report that is not a memory yet ".repeat(100);

    let sent = mail::send(&conn, "stet/finger-1", "stet", "report", &body, "text")?;
    assert_eq!(sent.bytes, body.len() as i64);

    let letter = mail::read(&conn, sent.seq)?.expect("the letter");
    assert_eq!(letter.body, body, "the whole body, verbatim");
    assert_eq!(letter.recipient, "stet");
    assert_eq!(
        letter.envelope.sender, "stet/finger-1",
        "provenance survives"
    );

    assert!(
        mail::read(&conn, sent.seq + 999)?.is_none(),
        "a missing seq is None"
    );
    Ok(())
}

#[test]
fn a_letter_is_refused_when_it_is_not_a_letter() -> Result<()> {
    let (_dir, paths) = temp_store();
    let conn = db::open(&paths)?;

    assert!(
        mail::send(&conn, "", "twine", "s", "b", "text").is_err(),
        "no sender"
    );
    assert!(
        mail::send(&conn, "stet", "  ", "s", "b", "text").is_err(),
        "no recipient"
    );
    assert!(
        mail::send(&conn, "stet", "twine", "s", "   ", "text").is_err(),
        "no body"
    );

    let huge = "x".repeat(mail::MAX_BODY_BYTES + 1);
    let err = mail::send(&conn, "stet", "twine", "big", &huge, "text").unwrap_err();
    let msg = err.to_string();
    assert!(
        msg.contains("cap"),
        "the refusal says what the cap is: {msg}"
    );
    Ok(())
}

/// ★ `seq` IS THE CURSOR, so it must never go BACKWARDS. SQLite reuses the largest rowid after the
/// highest row is deleted unless the table is declared `AUTOINCREMENT`; a prune would then hand out a
/// number a reader had already passed, and old mail would silently look read.
#[test]
fn a_prune_cannot_make_old_mail_look_new() -> Result<()> {
    let (_dir, paths) = temp_store();
    let conn = db::open(&paths)?;

    let first = mail::send(&conn, "stet", "twine", "one", "first", "text")?;
    let second = mail::send(&conn, "stet", "twine", "two", "second", "text")?;

    // Age both, then prune them.
    conn.execute("UPDATE mail SET at = datetime('now', '-30 days')", [])?;
    assert_eq!(mail::prune(&conn, 7)?, 2, "both aged out");
    assert_eq!(mail::head(&conn, "twine")?, 0, "the box is empty");

    let third = mail::send(&conn, "stet", "twine", "three", "third", "text")?;
    assert!(
        third.seq > second.seq,
        "a new letter must be NEWER than the pruned ones ({} !> {})",
        third.seq,
        second.seq
    );
    assert!(third.seq > first.seq);
    Ok(())
}

#[test]
fn an_encoded_body_keeps_its_encoding() -> Result<()> {
    let (_dir, paths) = temp_store();
    let conn = db::open(&paths)?;
    let sent = mail::send(&conn, "stet", "twine", "a patch", "H4sIAAAA", "gzip+base64")?;
    assert_eq!(sent.encoding, "gzip+base64");
    assert_eq!(
        mail::read(&conn, sent.seq)?
            .expect("letter")
            .envelope
            .encoding,
        "gzip+base64",
        "the reader is told how to read it"
    );
    Ok(())
}
