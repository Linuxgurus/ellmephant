//! KINSHIP: how one pen stands to another, as a walk of single letters (#109).
//!
//! James's notation, settled 2026-10-23 over several corrections, and the final shape is stronger
//! than the first:
//!
//! ```text
//! P  up     C  down     S  sibling        requestor → target, always that way round
//! ```
//!
//! - **`S` is ALWAYS written.** It is a step, not an abbreviation, which is why `PS` is an uncle and
//!   not "some ancestor". A reversal that does not name the sibling is refused: write `PSC`, not
//!   `PC`.
//! - **`whois` returns the SHORTEST path** and never walks back on itself, so `PC` is *presented as*
//!   `S` — a sibling is one token, not two — and `CP` is invalid outright, because a pen has one
//!   parent and down-then-up can only retrace your own edge.
//! - **Yourself is `ME`** — not a walk, and the one answer that needs no word.
//! - **English is a courtesy.** The letters are the fact; a word in quotes is printed only where it
//!   is exact, and stops where English starts approximating ("once removed").
//!
//! The canonical shape is therefore `Pᵃ S Cᵇ` through the nearest common ancestor — `P` for a
//! parent, `PS` uncle, `PSC` cousin, `PPSCC` second cousin, `S` sibling — and a direct line is `Pᵃ`
//! or `Cᵇ` with no `S` at all.

use rusqlite::{Connection, OptionalExtension, params};

use crate::error::{Error, Result};

/// One step of a walk. `P` up, `C` down, `S` sideways.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Step {
    /// To the parent.
    Up,
    /// To a child.
    Down,
    /// To a sibling — always written, never implied.
    Side,
}

/// What one pen is to another: the walk, and (when it is exact) the word for it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Kin {
    /// The letters, shortest form, never doubling back.
    pub walk: String,
    /// The English word, only where English is exact.
    pub word: Option<&'static str>,
}

impl Kin {
    /// The line a reader sees: `PS "uncle"`, or just `PPSCC` once English gives out.
    pub fn gloss(&self) -> String {
        match self.word {
            Some(w) => format!("{} \"{w}\"", self.walk),
            None => self.walk.clone(),
        }
    }
}

/// The words we will stand behind. Deliberately short: past a second cousin the family names are
/// folklore, and a ledger cannot carry a word that is approximately right.
fn word_for(walk: &str) -> Option<&'static str> {
    Some(match walk {
        "P" => "parent",
        "PP" => "grandparent",
        "PPP" => "great-grandparent",
        "C" => "child",
        "CC" => "grandchild",
        "CCC" => "great-grandchild",
        "S" => "sibling",
        "PS" => "uncle",
        "SC" => "nephew",
        "PSC" => "cousin",
        "PPSCC" => "second cousin",

        _ => return None,
    })
}

/// The word for a walk, for surfaces that already hold one — the same table [`relation`] uses.
pub fn word_for_public(walk: &str) -> Option<&'static str> {
    word_for(walk)
}

/// Read a walk of letters.
///
/// `PC` is accepted and normalised to `S` (the sibling is the only pen it can name, and one token is
/// shorter). `CP` is REFUSED: down-then-up returns to where it started, which is `self` and not a
/// walk at all. Everything else is refused with the letter it did not understand.
pub fn parse(walk: &str) -> Result<Vec<Step>> {
    let mut steps: Vec<Step> = Vec::new();
    for ch in walk.chars() {
        let step = match ch {
            'P' => Step::Up,
            'C' => Step::Down,
            'S' => Step::Side,
            other => {
                return Err(Error::Invalid(format!(
                    "`{other}` is not a step: a walk is made of P (up), C (down) and S (sibling)"
                )));
            }
        };
        // A pen has ONE parent, so down-then-up can only retrace the edge it came down.
        if step == Step::Up && steps.last() == Some(&Step::Down) {
            return Err(Error::Invalid(format!(
                "`{walk}` walks back on itself: down then up returns to where it started, which is \
                 `self` and not a walk"
            )));
        }
        // Up-then-down names a sibling: the parent's OTHER child. One token says it.
        if step == Step::Down && steps.last() == Some(&Step::Up) {
            steps.pop();
            steps.push(Step::Side);
            continue;
        }
        steps.push(step);
    }
    Ok(steps)
}

/// Render steps as a walk. Steps are emitted as given; the canonical form comes from [`relation`].
pub fn render(steps: &[Step]) -> String {
    steps
        .iter()
        .map(|s| match s {
            Step::Up => 'P',
            Step::Down => 'C',
            Step::Side => 'S',
        })
        .collect()
}

/// The parent chain of `name`, nearest first. Empty for a root.
fn ancestry(conn: &Connection, name: &str) -> Result<Vec<String>> {
    let mut out = Vec::new();
    let mut cur = name.to_string();
    for _ in 0..64 {
        let parent: Option<String> = conn
            .query_row(
                "SELECT p.name FROM pen c JOIN pen p ON p.id = c.parent_id WHERE lower(c.name) = lower(?1)",
                params![cur],
                |r| r.get(0),
            )
            .optional()?;
        match parent {
            Some(p) => {
                out.push(p.clone());
                cur = p;
            }
            None => break,
        }
    }
    Ok(out)
}

/// **The shortest walk from one pen to another**, or `None` when they share no root.
///
/// The shape is forced by the tree: up to the nearest common ancestor, sideways ONE step, then down.
/// A sibling is therefore `S` and never `PS C`; an uncle is `PS` and never `P` + something. There is
/// no path that doubles back, so `CP` cannot be produced, and `PC` is collapsed to `S` before it
/// could be printed.
pub fn relation(conn: &Connection, from: &str, to: &str) -> Result<Option<Kin>> {
    // ★ CASE IS NOT PART OF A NAME (soma/ellmephant#191, 2026-10-26). The spec is case-INSENSITIVE and
    // case-PRESERVING: `James` and `james` are one pen, and the stored spelling is the only thing that
    // may be displayed. This function used to resolve the tree case-insensitively and then compare the
    // two chains with exact string equality — so `whois James` answered `P "parent"` and `whois james`
    // answered `PS "uncle"` for the same row, because `to` is seeded with the spelling the CALLER typed
    // while `mine` holds the spelling the DATABASE stored. One fold, once, at the comparison.
    // ★ ASCII FOLDING, DELIBERATELY (adversarial review of #191). SQLite's `lower()` — which every pen
    // lookup and every audience comparison already uses — folds ASCII only, so folding more here
    // (`to_lowercase()`) would make this function disagree with the lookup that produced the names:
    // `Ä` would match in one and not the other. Matching the database's own rule is the invariant;
    // a pen named with non-ASCII letters is case-SENSITIVE everywhere today, which is a limitation of
    // the stores's `lower()`, not of this function, and it should be fixed there or nowhere.
    let fold = |s: &str| s.to_ascii_lowercase();
    // ★ YOURSELF IS `ME` (James, 2026-10-23) — not a walk at all, and not a word in quotes: it is
    // the one answer that needs no gloss.
    if from.eq_ignore_ascii_case(to) {
        return Ok(Some(Kin {
            walk: "ME".to_string(),
            word: None,
        }));
    }
    let mine: Vec<String> = ancestry(conn, from)?.iter().map(|s| fold(s)).collect();
    // `to` ITSELF may be the common ancestor (a parent, a grandparent), so it belongs in the chain —
    // leaving it out makes every direct ancestor look like kin through someone else's branch.
    let mut theirs = vec![to.to_string()];
    theirs.extend(ancestry(conn, to)?);
    let theirs: Vec<String> = theirs.iter().map(|s| fold(s)).collect();

    // Where do the two chains meet? `to` itself is the ancestor of interest when it is above us.
    let (lca, up_to_lca) = if theirs.contains(&fold(from)) {
        (fold(from), 0)
    } else {
        let mut found = None;
        for (i, ancestor) in mine.iter().enumerate() {
            if theirs.contains(ancestor) {
                found = Some((ancestor.clone(), i + 1));
                break;
            }
        }
        match found {
            Some(pair) => pair,
            None => return Ok(None),
        }
    };

    // Distance from the LCA down to `to`: 0 when `to` IS the LCA.
    let mut down = 0usize;
    let mut cur = fold(to);
    while cur != lca {
        let parent: Option<String> = conn
            .query_row(
                "SELECT p.name FROM pen c JOIN pen p ON p.id = c.parent_id WHERE lower(c.name) = lower(?1)",
                params![cur],
                |r| r.get(0),
            )
            .optional()?;
        match parent {
            Some(p) => {
                down += 1;
                cur = fold(&p);
            }
            None => return Ok(None),
        }
    }

    let mut steps = Vec::new();
    match (up_to_lca, down) {
        // The target is above us: a direct line up, no sibling involved.
        (up, 0) => steps.extend(std::iter::repeat_n(Step::Up, up)),
        // The target is below us: a direct line down.
        (0, down) => steps.extend(std::iter::repeat_n(Step::Down, down)),
        // Kin through a common ancestor: up to it, ONE sibling step, then down.
        (up, down) => {
            steps.extend(std::iter::repeat_n(Step::Up, up - 1));
            steps.push(Step::Side);
            steps.extend(std::iter::repeat_n(Step::Down, down - 1));
        }
    }
    let walk = render(&steps);
    Ok(Some(Kin {
        word: word_for(&walk),
        walk,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_walk_reads_its_letters() {
        assert_eq!(parse("P").unwrap(), vec![Step::Up]);
        assert_eq!(
            parse("PSC").unwrap(),
            vec![Step::Up, Step::Side, Step::Down]
        );
        // Up-then-down names a sibling, and one token says it.
        assert_eq!(parse("PC").unwrap(), vec![Step::Side]);
        assert_eq!(parse("PS").unwrap(), vec![Step::Up, Step::Side]);
    }

    #[test]
    fn a_walk_may_not_double_back() {
        let err = parse("CP").unwrap_err();
        assert!(format!("{err}").contains("walks back on itself"), "{err}");
        let err = parse("PX").unwrap_err();
        assert!(format!("{err}").contains("is not a step"), "{err}");
    }

    #[test]
    fn the_words_stop_where_english_starts_approximating() {
        assert_eq!(word_for("PSC"), Some("cousin"));
        assert_eq!(word_for("PPSCC"), Some("second cousin"));
        assert_eq!(
            word_for("PPSCCC"),
            None,
            "no word is better than a wrong one"
        );
        assert_eq!(
            Kin {
                walk: "PS".into(),
                word: word_for("PS")
            }
            .gloss(),
            "PS \"uncle\""
        );
        assert_eq!(
            Kin {
                walk: "PPSCCC".into(),
                word: None
            }
            .gloss(),
            "PPSCCC"
        );
    }
}
