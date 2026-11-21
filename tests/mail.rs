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

    let inbox = mail::inbox(&conn, "twine", 0, 50)?;
    assert_eq!(inbox.len(), 1);
    assert_eq!(inbox[0].seq, sent.seq);
    Ok(())
}

#[test]
fn the_cursor_shows_only_what_is_after_it() -> Result<()> {
    let (_dir, paths) = temp_store();
    let conn = db::open(&paths)?;

    let first = mail::send(&conn, "stet", "stet", "twine", "one", "first", "text")?;
    let second = mail::send(&conn, "stet", "stet", "twine", "two", "second", "text")?;
    let _other = mail::send(
        &conn,
        "stet",
        "stet",
        "james",
        "elsewhere",
        "not twine's",
        "text",
    )?;

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

    let sent = mail::send(
        &conn,
        "stet/finger-1",
        "stet/finger-1",
        "stet",
        "report",
        &body,
        "text",
    )?;
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
        mail::send(&conn, "", "", "twine", "s", "b", "text").is_err(),
        "no sender"
    );
    assert!(
        mail::send(&conn, "stet", "stet", "  ", "s", "b", "text").is_err(),
        "no recipient"
    );
    assert!(
        mail::send(&conn, "stet", "stet", "twine", "s", "   ", "text").is_err(),
        "no body"
    );

    let huge = "x".repeat(mail::MAX_BODY_BYTES + 1);
    let err = mail::send(&conn, "stet", "stet", "twine", "big", &huge, "text").unwrap_err();
    let msg = err.to_string();
    assert!(
        msg.contains("cap"),
        "the refusal says what the cap is: {msg}"
    );
    Ok(())
}

#[test]
fn a_prune_cannot_make_old_mail_look_new() -> Result<()> {
    let (_dir, paths) = temp_store();
    let conn = db::open(&paths)?;

    let first = mail::send(&conn, "stet", "stet", "twine", "one", "first", "text")?;
    let second = mail::send(&conn, "stet", "stet", "twine", "two", "second", "text")?;

    conn.execute("UPDATE mail SET at = datetime('now', '-30 days')", [])?;
    assert_eq!(mail::prune(&conn, 7)?, 2, "both aged out");
    assert_eq!(mail::head(&conn, "twine")?, 0, "the box is empty");

    let third = mail::send(&conn, "stet", "stet", "twine", "three", "third", "text")?;
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
    let sent = mail::send(
        &conn,
        "stet",
        "stet",
        "twine",
        "a patch",
        "H4sIAAAA",
        "gzip+base64",
    )?;
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

#[test]
fn a_short_page_reports_its_own_top_not_the_boxs() -> Result<()> {
    let (_dir, paths) = temp_store();
    let conn = db::open(&paths)?;

    for n in 0..40 {
        mail::send(
            &conn,
            "james",
            "james",
            "stet",
            &format!("letter {n}"),
            "body",
            "text",
        )?;
    }

    let page = mail::inbox(&conn, "stet", 0, 10)?;
    let page_top = page.iter().map(|e| e.seq).max().expect("a full page");
    let box_top = mail::head(&conn, "stet")?;

    assert_eq!(page.len(), 10, "a page is a page");
    assert!(
        page_top < box_top,
        "the page's top ({page_top}) is NOT the box's top ({box_top}) — reporting one as the other \
         is the defect, and the answer now carries both"
    );
    assert_eq!(page_top, page[9].seq, "the page top is its last letter");
    assert_eq!(
        box_top, 40,
        "the box top is every letter ever addressed to it"
    );
    Ok(())
}

#[test]
fn a_cursor_belongs_to_one_reader_and_only_moves_forward() -> Result<()> {
    let (_dir, paths) = temp_store();
    let conn = db::open(&paths)?;

    let one = mail::send(&conn, "james", "james", "stet", "one", "first", "text")?;
    let two = mail::send(&conn, "james", "james", "stet", "two", "second", "text")?;

    assert_eq!(mail::cursor(&conn, "stet", "stet")?, None);
    assert_eq!(mail::cursor_or_zero(&conn, "stet", "stet")?, 0);

    let hand = mail::advance(&conn, "hands", "stet", one.seq)?;
    assert_eq!(hand.moved, one.seq, "it skipped the first letter");
    assert_eq!(hand.head, two.seq);
    assert_eq!(hand.unread, 1, "one letter left in the box");

    let me = mail::advance(&conn, "Stet", "stet", 0)?;
    assert_eq!(me.reader, "stet", "names are one box however typed");
    assert_eq!(me.unread, 2, "the self has read nothing");
    assert_eq!(
        mail::cursor_or_zero(&conn, "hands", "stet")?,
        one.seq,
        "the hand's place did not move when the self advanced"
    );

    let err = mail::advance(&conn, "hands", "stet", 0).unwrap_err();
    let msg = err.to_string();
    assert!(msg.contains("already at"), "{msg}");
    assert!(msg.contains("only moves forward"), "{msg}");

    let again = mail::advance(&conn, "hands", "stet", one.seq)?;
    assert_eq!(again.moved, 0);
    assert_eq!(again.seq, one.seq);
    assert_eq!(mail::cursor(&conn, "hands", "stet")?.unwrap().seq, one.seq);
    Ok(())
}

#[test]
fn an_unauthorized_sender_is_refused_by_name() -> Result<()> {
    let (_dir, paths) = temp_store();
    let conn = db::open(&paths)?;

    let err = mail::send(&conn, "cline", "james", "stet", "s", "b", "text").unwrap_err();
    let msg = err.to_string();
    assert!(msg.contains("may not send as"), "{msg}");
    assert!(msg.contains("james"), "{msg}");
    Ok(())
}

#[test]
fn a_granted_surrogate_sends_and_the_letter_says_who_typed_it() -> Result<()> {
    let (_dir, paths) = temp_store();
    let conn = db::open(&paths)?;
    let grant = mail::grant(&conn, "james", "cline", "send", None, Some("asleep"))?;

    let env = mail::send(&conn, "cline", "james", "stet", "s", "b", "text")?;
    assert_eq!(env.sender, "james", "the letter is from the grantor");
    assert_eq!(
        env.sent_by.as_deref(),
        Some("cline"),
        "and it says who typed it"
    );

    assert_eq!(
        env.sent_under.as_deref(),
        Some(grant.granted_at.as_str()),
        "the receipt names the grant that authorised it"
    );

    let letter = mail::read(&conn, env.seq)?.expect("letter");
    assert_eq!(letter.envelope.sent_by.as_deref(), Some("cline"));
    assert_eq!(
        letter.envelope.sent_under.as_deref(),
        Some(grant.granted_at.as_str())
    );
    Ok(())
}

#[test]
fn revoking_a_grant_refuses_the_very_next_letter() -> Result<()> {
    let (_dir, paths) = temp_store();
    let conn = db::open(&paths)?;
    mail::grant(&conn, "james", "cline", "send", None, None)?;
    assert!(mail::send(&conn, "cline", "james", "stet", "s", "b", "text").is_ok());

    assert_eq!(mail::revoke(&conn, "james", "cline")?, 1);
    let err = mail::send(&conn, "cline", "james", "stet", "s", "b", "text").unwrap_err();
    assert!(err.to_string().contains("may not send as"), "{err}");
    Ok(())
}

#[test]
fn an_expired_grant_is_refused() -> Result<()> {
    let (_dir, paths) = temp_store();
    let conn = db::open(&paths)?;
    mail::grant(
        &conn,
        "james",
        "cline",
        "send",
        Some("2000-01-01 00:00:00"),
        None,
    )?;

    let err = mail::send(&conn, "cline", "james", "stet", "s", "b", "text").unwrap_err();
    assert!(err.to_string().contains("may not send as"), "{err}");
    Ok(())
}

#[test]
fn a_grant_is_scoped() -> Result<()> {
    let (_dir, paths) = temp_store();
    let conn = db::open(&paths)?;
    mail::grant(&conn, "james", "cline", "read", None, None)?;

    assert!(mail::may_read_box(&conn, "cline", "james")?, "read scope");
    assert!(
        !mail::may_send_as(&conn, "cline", "james").is_ok(),
        "not a send grant"
    );
    Ok(())
}

#[test]
fn unknown_scopes_are_refused() -> Result<()> {
    let (_dir, paths) = temp_store();
    let conn = db::open(&paths)?;
    let err = mail::grant(&conn, "james", "cline", "write", None, None).unwrap_err();
    assert!(err.to_string().contains("scopes"), "{err}");
    Ok(())
}

#[test]
fn ordinary_letters_carry_no_sent_by() -> Result<()> {
    let (_dir, paths) = temp_store();
    let conn = db::open(&paths)?;
    let env = mail::send(&conn, "stet", "stet", "twine", "s", "b", "text")?;
    assert!(env.sent_by.is_none());
    assert!(
        env.sent_under.is_none(),
        "nothing authorised, nothing to cite"
    );
    Ok(())
}

#[test]
fn an_ack_grant_does_not_confer_reading() -> Result<()> {
    let (_dir, paths) = temp_store();
    let conn = db::open(&paths)?;
    mail::grant(&conn, "james", "cline", "ack", None, None)?;

    assert!(mail::may_ack_box(&conn, "cline", "james")?, "ack scope");
    assert!(
        !mail::may_read_box(&conn, "cline", "james")?,
        "but not read"
    );
    Ok(())
}

#[test]
fn env_pairs_import_once_and_a_revocation_survives_a_reimport() -> Result<()> {
    let (_dir, paths) = temp_store();
    let conn = db::open(&paths)?;

    unsafe { std::env::set_var("ELLMEPHANT_MAIL_SURROGATES", "hands=stet") };
    assert!(mail::import_env_surrogates(&conn)? >= 1);
    assert!(mail::may_read_box(&conn, "hands", "stet")?);

    assert_eq!(mail::import_env_surrogates(&conn)?, 0);
    mail::revoke(&conn, "stet", "hands")?;
    let _ = mail::import_env_surrogates(&conn);
    assert!(
        !mail::may_read_box(&conn, "hands", "stet")?,
        "revoked stays revoked"
    );

    unsafe { std::env::remove_var("ELLMEPHANT_MAIL_SURROGATES") };
    Ok(())
}
