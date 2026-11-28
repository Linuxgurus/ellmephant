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
///
/// ★ MARKDOWN DECORATION IS STRIPPED FIRST (#269 review). A banner is a banner however it is
/// dressed, and the dressed forms are the COMMON ones in a markdown body: `**SUPERSEDED**`,
/// `# SUPERSEDED`, `- SUPERSEDED`, `> SUPERSEDED`, `[SUPERSEDED]`, `_SUPERSEDED_`. A matcher
/// anchored at byte 0 of the trimmed line saw only the bare form, and the bare form is the LEAST
/// likely spelling. The strip touches only the FRONT (and a matched emphasis prefix), so
/// `MOOTNESS` and `SUPERSEDED_ROW` still do not fire.
pub fn word(body: &str) -> Option<&'static str> {
    let first = body.lines().map(str::trim).find(|line| !line.is_empty())?;
    let line = undecorate(first);
    WORDS
        .iter()
        .copied()
        .find(|word| opens_with(line, word) || emphasis_inner(line, word))
}

const WORDS: [&str; 4] = ["SUPERSEDED", "RETRACTED", "OBSOLETE", "MOOT"];

/// The marks that can pair around the word as emphasis (`_MOOT_`, `*MOOT*`).
const PAIRED: [char; 4] = ['_', '*', '`', '~'];

fn is_front_mark(c: char) -> bool {
    c.is_whitespace() || matches!(c, '#' | '>' | '*' | '+' | '-' | '`' | '~' | '[' | '(')
}

/// Strip the decoration an author puts IN FRONT of the word: a list or header marker, a
/// blockquote marker, emphasis, or an opening bracket. Only the front, so the word-boundary rule
/// that refuses `SUPERSEDED_ROW` is untouched.
///
/// The underscore is deliberately NOT a front mark: a leading `_` is handled only as the opening
/// half of a matched emphasis pair, which is what italic markdown is, while an underscore in
/// `SUPERSEDED_ROW` continues the word and must keep doing so.
fn undecorate(line: &str) -> &str {
    let mut s = line.trim();
    loop {
        let t = s.trim_start_matches(is_front_mark);
        if t.len() == s.len() {
            break;
        }
        s = t;
    }
    s
}

/// `_MOOT_ by the new row` — the word is inside a matched emphasis pair that the front-strip left
/// in place, because `_` is not a front mark. The inner text must be a single token: emphasis
/// wraps a word, it does not wrap a sentence.
fn emphasis_inner(line: &str, word: &str) -> bool {
    for mark in PAIRED {
        let Some(rest) = line.strip_prefix(mark) else {
            continue;
        };
        if let Some(end) = rest.find(mark) {
            let inner = &rest[..end];
            if !inner.is_empty() && !inner.contains(char::is_whitespace) && opens_with(inner, word)
            {
                return true;
            }
        }
    }
    false
}

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

    /// #269 review: six of eight realistic spellings of a banner are DRESSED, and a matcher
    /// anchored at byte 0 of the trimmed line saw none of them.
    #[test]
    fn markdown_decoration_does_not_hide_a_banner() {
        for (body, expected) in [
            ("**SUPERSEDED** by the new row", "SUPERSEDED"),
            ("# SUPERSEDED by the new row", "SUPERSEDED"),
            ("- SUPERSEDED by the new row", "SUPERSEDED"),
            ("* SUPERSEDED by the new row", "SUPERSEDED"),
            ("> SUPERSEDED by the new row", "SUPERSEDED"),
            ("[SUPERSEDED] by the new row", "SUPERSEDED"),
            ("_SUPERSEDED_ by the new row", "SUPERSEDED"),
            ("*SUPERSEDED*", "SUPERSEDED"),
            ("#### **`RETRACTED`** — this row was wrong", "RETRACTED"),
            ("> - [MOOT] the budget line moved", "MOOT"),
            ("  - OBSOLETE: the old script", "OBSOLETE"),
            (
                "**SUPERSEDED-BY-THE-NEW-ROW** names its replacement",
                "SUPERSEDED",
            ),
        ] {
            assert_eq!(
                word(body),
                Some(expected),
                "decoration hid the banner: `{body}`"
            );
        }
    }

    /// The strip touches only the front, so the negatives that make the rule mean something stay
    /// negative — and a decorated KEY is still a key.
    #[test]
    fn decoration_stripping_keeps_the_word_boundary_rule() {
        assert_eq!(word("**MOOTNESS** is not a banner"), None);
        assert_eq!(
            word("- SUPERSEDED_ROW"),
            None,
            "an underscore continues the word"
        );
        assert_eq!(word("`MOOTED`"), None, "a decorated different word");
        assert_eq!(word("# MOOTNESS_ROW"), None);
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
