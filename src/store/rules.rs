//! ★ THE WRITE RULES, ORDERED BY WHAT THEY READ (soma/ellmephant#238).
//!
//! `store::merge` used to be one 233-line function with six guards inline. The order of those guards
//! is load-bearing and was held by comment alone: `guard_protected` and `guard_audience` produce
//! different sentences for different row states, so running them before the authority check let a pen
//! that may not write a row learn that the row exists and is not public — a read disguised as a
//! refusal (#192). A well-meaning reorder would have restored that defect silently.
//!
//! So the order is data now, and the split is by **what a rule reads**, not by taste:
//!
//! | list | reads | runs |
//! | --- | --- | --- |
//! | [`RULES_PRE`] | the incoming `Patch` only | before the authority check |
//! | [`RULES_POST`] | the existing row and the caller | after it |
//!
//! `may_write` is not a rule: it is the boundary. It sits between the two lists, and the only way to
//! get a rule on the wrong side of it is to add it to the wrong list — which the tests name.
//!
//! ★ A RULE IS A PURE OBSERVER. A function pointer carries no state, so a rule cannot write
//! mid-pipeline; every rule asks "may this proceed?" and the single mutation (`add_impl`) is the last
//! step. That is what makes it impossible for the authority check to have had a side effect before it
//! decided anything.

use super::shape;
use super::{Memory, Patch, guard_audience, guard_protected};
use crate::error::{Error, Result};
use rusqlite::Connection;

/// What a rule may look at. Deliberately read-only: no `&mut`, no connection handle that could write.
pub(crate) struct WriteCtx<'a> {
    pub(crate) conn: &'a Connection,
    pub(crate) patch: &'a Patch,
    /// The resolved key — after slugging — because a refusal must name the key it acted on.
    pub(crate) key: &'a str,
    /// The row as it stands, if there is one. `None` is a create.
    pub(crate) existing: Option<&'a Memory>,
    /// The stored tags, so `protected` can tell "is" from "becomes".
    pub(crate) was_tags: &'a [String],
    /// The pen that ACTS — never the credit. Autority has been checked by the time `RULES_POST` runs,
    /// so a rule may name it in a refusal without leaking anything it should not know.
    pub(crate) writer: Option<&'a str>,
}

/// One rule: a name, why it exists, and what it checks.
///
/// All three are static, so the list costs nothing and a reorder shows up as a named failing test
/// with its rationale attached rather than a mystery 403 a year later.
pub(crate) struct Rule {
    /// Stable identifier — tests pin the order of these.
    pub(crate) name: &'static str,
    /// Why the rule exists, in the code where a later reader will need it.
    pub(crate) why: &'static str,
    pub(crate) run: fn(&WriteCtx<'_>) -> Result<()>,
}

impl std::fmt::Debug for Rule {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Rule").field("name", &self.name).finish()
    }
}

/// Rules that read only the incoming patch. Safe to run before anyone has decided whether the caller
/// may write at all, because they cannot answer a question about the row.
pub(crate) static RULES_PRE: &[Rule] = &[
    Rule {
        name: "shape",
        why: "★ THE DOOR THAT STOPS THE TAP (#217). A body whose SHAPE is a run record is refused, \
              and the refusal names the verb and arguments it inferred. `force: true` writes it \
              anyway: an agent that read the refusal may still be right.",
        run: shape_gate,
    },
    Rule {
        name: "contradiction",
        why: "★ TWO INSTRUCTIONS THAT CONTRADICT ARE REFUSED, NOT RESOLVED (#189 D5). `private` \
              derives an audience from the proven pen; an explicit `audience` names others. Keeping \
              one and dropping the other silently made a row invisible to the pen that wrote it. \
              Only `private: true` contradicts an audience: `private: false` says \"not private\", \
              and any audience satisfies that.",
        run: contradiction_gate,
    },
    Rule {
        name: "actor-not-credit",
        why: "★ `private` AND `as:` CONTRADICT (hostile review of #195, round 3). `private: true` \
              addresses the row to the pen that ACTS; `as:` credits a different pen. Both cannot be \
              true, and the failure was ugly rather than refused: the CREDITED pen could no longer \
              read words it was credited for. Like the rule above, this reads only the patch.",
        run: actor_not_credit_gate,
    },
];

/// Rules that read the existing row and the caller. These MUST run after the authority check: their
/// sentences name row state, so running one first turns a refusal into a read.
pub(crate) static RULES_POST: &[Rule] = &[
    Rule {
        name: "protected",
        why: "A `protected` memory is writable only under a live lease on its own key, and a write \
              that would ADD or REMOVE the tag passes the same gate — so a stranger can neither \
              ratchet a memory shut nor quietly unlock one.",
        run: protected_gate,
    },
    Rule {
        name: "audience",
        why: "The audience is a boundary, not a preference, and boundaries are not editable by the \
              party they exclude. This must come after `protected` and after authority, or a pen \
              that cannot read a row can learn it is private by being told so.",
        run: audience_gate,
    },
    Rule {
        name: "field",
        why: "★ AN EMPTY FIELD IS AN INSTRUCTION, NOT AN ERASURE. A patch that would leave a stored \
              title or body blank is refused rather than written: \"empty\" here means the caller \
              named the field and gave it nothing. It reads the row because a patch may omit a \
              field to keep the stored one — which is why it is POST, not PRE.",
        run: field_gate,
    },
];

/// Run one list in order, stopping at the first refusal.
///
/// The refusal is logged with the rule's name and rationale, so a write refused in production says
/// **which** rule refused and why the rule exists — the same two facts a later reader needs when
/// deciding whether it may move.
pub(crate) fn run(rules: &'static [Rule], ctx: &WriteCtx<'_>) -> Result<()> {
    for rule in rules {
        if let Err(e) = (rule.run)(ctx) {
            tracing::debug!(
                rule = rule.name,
                why = rule.why,
                "a write rule refused; the ORDER of these lists is part of the law (#238)"
            );
            return Err(e);
        }
    }
    Ok(())
}

fn shape_gate(ctx: &WriteCtx<'_>) -> Result<()> {
    if let Some(hint) = ctx
        .patch
        .body
        .as_deref()
        .filter(|_| !ctx.patch.force)
        .and_then(|body| shape::record_hint(body, ctx.key))
    {
        return Err(Error::Invalid(shape::refusal(&hint, ctx.key)));
    }
    Ok(())
}

fn contradiction_gate(ctx: &WriteCtx<'_>) -> Result<()> {
    if ctx.patch.private == Some(true) && ctx.patch.audience.is_some() {
        return Err(Error::Invalid(
            "`private: true` and `audience` contradict each other: private addresses the row to the \
             writing pen ALONE, audience names other pens. Pass one. To address several pens INCLUDING \
             yourself, use `audience: \"pen:<you>,pen:<other>\"` and no `private`; `private: false` \
             needs no audience, and any audience satisfies it."
                .into(),
        ));
    }
    Ok(())
}

fn actor_not_credit_gate(ctx: &WriteCtx<'_>) -> Result<()> {
    match (
        ctx.patch.private,
        ctx.patch.asserted_by.as_deref(),
        ctx.patch.acted_by.as_deref(),
    ) {
        (Some(true), Some(credited), Some(actor)) if !credited.eq_ignore_ascii_case(actor) => {
            Err(Error::Invalid(
                "`private: true` addresses the row to the pen that ACTS, and `as:` credits a \
                 different pen — they contradict each other. Pass an explicit `audience` (e.g. \
                 `pen:<you>,pen:<them>`) if the row is for both, or drop one of them."
                    .into(),
            ))
        }
        _ => Ok(()),
    }
}

fn protected_gate(ctx: &WriteCtx<'_>) -> Result<()> {
    guard_protected(ctx.conn, ctx.key, ctx.was_tags, ctx.patch, ctx.writer)
}

fn audience_gate(ctx: &WriteCtx<'_>) -> Result<()> {
    guard_audience(ctx.conn, ctx.existing, ctx.patch, ctx.writer)
}

fn field_gate(ctx: &WriteCtx<'_>) -> Result<()> {
    let (title, body) = match ctx.existing {
        Some(old) => (
            ctx.patch.title.clone().unwrap_or_else(|| old.title.clone()),
            ctx.patch.body.clone().unwrap_or_else(|| old.body.clone()),
        ),
        None => (
            ctx.patch.title.clone().unwrap_or_default(),
            ctx.patch.body.clone().unwrap_or_default(),
        ),
    };
    if ctx.existing.is_some() {
        if title.trim().is_empty() {
            return Err(Error::Invalid(
                "`title` cannot be empty; omit it to leave the stored title alone".into(),
            ));
        }
        if body.trim().is_empty() {
            return Err(Error::Invalid(
                "`body` cannot be empty; omit it to leave the stored body alone".into(),
            ));
        }
        return Ok(());
    }
    if title.trim().is_empty() || body.trim().is_empty() {
        return Err(Error::Invalid(
            "a new memory needs both a `title` and a `body`; an existing key can be patched \
             field by field"
                .into(),
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn names(rules: &'static [Rule]) -> Vec<&'static str> {
        rules.iter().map(|r| r.name).collect()
    }

    /// ★ THE ORDER IS THE CONTRACT. If this test fails, a refusal has started leaking row state —
    /// read `why` on the rule that moved.
    #[test]
    fn the_lists_are_in_the_documented_order() {
        assert_eq!(
            names(RULES_PRE),
            vec!["shape", "contradiction", "actor-not-credit"]
        );
        assert_eq!(names(RULES_POST), vec!["protected", "audience", "field"]);
    }

    /// A rule may not be in both lists: it reads the row or it does not, and "both" means its
    /// classification is a guess.
    #[test]
    fn no_rule_appears_in_both_lists() {
        for pre in names(RULES_PRE) {
            assert!(
                !names(RULES_POST).contains(&pre),
                "`{pre}` is in both lists, so the split by what-it-reads is not a split"
            );
        }
    }

    /// Every rule carries a reason. A rule without one is a rule nobody can safely move.
    #[test]
    fn every_rule_says_why_it_exists() {
        for rule in RULES_PRE.iter().chain(RULES_POST.iter()) {
            assert!(
                rule.why.len() > 40,
                "`{}` has no real rationale — a later reader will not know whether it may move it",
                rule.name
            );
        }
    }
}
