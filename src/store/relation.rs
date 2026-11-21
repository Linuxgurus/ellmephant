use rusqlite::{Connection, OptionalExtension, params};

use crate::error::{Error, Result};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Step {
    Up,
    Down,
    Side,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Kin {
    pub walk: String,
    pub word: Option<&'static str>,
}

impl Kin {
    pub fn gloss(&self) -> String {
        match self.word {
            Some(w) => format!("{} \"{w}\"", self.walk),
            None => self.walk.clone(),
        }
    }
}

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

pub fn word_for_public(walk: &str) -> Option<&'static str> {
    word_for(walk)
}

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
        if step == Step::Up && steps.last() == Some(&Step::Down) {
            return Err(Error::Invalid(format!(
                "`{walk}` walks back on itself: down then up returns to where it started, which is \
                 `self` and not a walk"
            )));
        }
        if step == Step::Down && steps.last() == Some(&Step::Up) {
            steps.pop();
            steps.push(Step::Side);
            continue;
        }
        steps.push(step);
    }
    Ok(steps)
}

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

pub fn relation(conn: &Connection, from: &str, to: &str) -> Result<Option<Kin>> {
    let fold = |s: &str| s.to_ascii_lowercase();
    if from.eq_ignore_ascii_case(to) {
        return Ok(Some(Kin {
            walk: "ME".to_string(),
            word: None,
        }));
    }
    let mine: Vec<String> = ancestry(conn, from)?.iter().map(|s| fold(s)).collect();
    let mut theirs = vec![to.to_string()];
    theirs.extend(ancestry(conn, to)?);
    let theirs: Vec<String> = theirs.iter().map(|s| fold(s)).collect();

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
        (up, 0) => steps.extend(std::iter::repeat_n(Step::Up, up)),
        (0, down) => steps.extend(std::iter::repeat_n(Step::Down, down)),
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
