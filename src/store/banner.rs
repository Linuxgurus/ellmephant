//! The banner: a row whose FIRST LINE announces that the row is dead.
//!
//! `SUPERSEDED`, `RETRACTED`, `OBSOLETE`, `MOOT` — an annotation a writer adds instead of doing
//! the thing that actually removes the wrong answer from search. Search quotes an excerpt, and a
//! reader who lands mid-body never sees line one: the banner is invisible to everyone except the
//! pen that wrote it. See `supersede-by-replacement-not-by-banner`.

/// The banner word a body opens with, if any.
///
/// The FIRST non-empty line is the only place a banner can be: a body that mentions "superseded"
/// in its fourth paragraph is describing a supersession, which is legitimate prose. Case is
/// irrelevant, and the word must END there — a letter, a digit or an underscore continues the
/// word (`MOOTNESS` is not `MOOT`, `SUPERSEDED_ROW` is a key). Punctuation and space end it, so
/// `SUPERSEDED-BY-THE-NEW-ROW` IS a banner: naming what replaced the row while leaving it in
/// search is the worst form of the rot, not an exception to it.
pub fn word(body: &str) -> Option<&'static str> {
    let first = body.lines().map(str::trim).find(|line| !line.is_empty())?;
    WORDS.iter().copied().find(|word| opens_with(first, word))
}

const WORDS: [&str; 4] = ["SUPERSEDED", "RETRACTED", "OBSOLETE", "MOOT"];

fn opens_with(line: &str, word: &str) -> bool {
    let Some(rest) = line.get(..word.len()) else {
        return false;
    };
    if !rest.eq_ignore_ascii_case(word) {
        return false;
    }
    match line.as_bytes().get(word.len()) {
        None => true,
        Some(b) => !b.is_ascii_alphanumeric() && *b != b'_',
    }
}

/// The refusal: name the banner, then the process that replaces it.
///
/// The exit deliberately does NOT say "point a `[[wikilink]]` at the replacement". An edge to a
/// deleted row is a broken edge (`memory_dangling`), and the doctrine is explicit: name the
/// deleted key by its PLAIN name in the good row, because a pointer that resolves to nothing is
/// not a reference.
pub fn refusal(word: &str, key: &str) -> String {
    format!(
        "this body opens `{word}`, which is a BANNER, not a memory — and the banner is the one part \
of the row search never quotes. A reader who lands mid-body sees four paragraphs of instructions \
that are wrong NOW, and no note saying so: the excerpt IS the memory as far as search is \
concerned. Delete wrong rows; never annotate them.\n\
\n\
THE PROCESS, in order:\n\
\n   1. Keep what is still worth knowing — usually the LESSON — in a GOOD row whose title names it \
as a lesson, never in the row that described the wrong mechanics.\n\
   2. `memory_delete` `{key}`. The versions are snapshotted (`versions_remaining`), so the words \
stay recoverable with `memory_history` and stop answering searches. Deletion is the archival act.\n\
   3. Name `{key}` in the good row by its PLAIN name — `the old key \\`{key}\\` (deleted \
<date>)` — and NOT as a `[[wikilink]]`: an edge to a deleted row is a broken edge \
(`memory_dangling`), and a pointer that resolves to nothing is not a reference.\n\
\n\
If the replacement already exists, name ITS key the same way; a `[[wikilink]]` is fine only while \
the target is live and readable by the pens who will read this row.\n\
\n\
If this really is a memory — prose that happens to open with one of those words — pass \
`force: true` and it will be written unchanged."
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn each_banner_word_is_recognised_on_the_first_line() {
        for banner in WORDS {
            let body = format!("{banner}: the numbers moved\nand the old row is wrong");
            assert_eq!(word(body.as_str()), Some(banner), "`{banner}` did not fire");
        }
    }

    #[test]
    fn case_and_leading_blank_lines_do_not_hide_a_banner() {
        assert_eq!(
            word("\n\n  superseded — see the new row\nbody"),
            Some("SUPERSEDED")
        );
    }

    #[test]
    fn a_banner_word_must_end_where_it_ends() {
        assert_eq!(
            word("SUPERSEDED-BY-THE-NEW-ROW names its replacement and is still a banner"),
            Some("SUPERSEDED"),
            "naming the replacement does not make the dead row searchable-safe"
        );
        assert_eq!(
            word("MOOTNESS is not a banner"),
            None,
            "`MOOTNESS` is a different word"
        );
        assert_eq!(
            word("SUPERSEDED_ROW"),
            None,
            "an underscore continues the word"
        );
    }

    #[test]
    fn a_word_later_in_the_body_is_prose_not_a_banner() {
        let body = "A lesson about deploys\n\nThe old checklist was SUPERSEDED in 2026-10 and this \
is what replaced it.";
        assert_eq!(word(body), None, "only the FIRST line can be a banner");
    }

    #[test]
    fn an_empty_body_has_no_banner() {
        assert_eq!(word(""), None);
        assert_eq!(word("\n \n\t\n"), None);
    }

    #[test]
    fn the_refusal_names_the_process_and_forbids_the_dangling_edge() {
        let msg = refusal("SUPERSEDED", "old-mechanics");
        assert!(msg.contains("SUPERSEDED"));
        assert!(msg.contains("memory_delete"));
        assert!(msg.contains("versions_remaining"), "deletion is archival");
        assert!(
            msg.contains("NOT as a `[[wikilink]]`"),
            "the exit must not create a dangling edge: {msg}"
        );
        assert!(msg.contains("force: true"), "the override is named");
    }
}
