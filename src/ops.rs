//! THE LAW, ONCE (soma/ellmephant#231, #234).
//!
//! Every verb used to be implemented twice — once per door — and the copies disagreed (see
//! `tests/authority_contract.rs`). An `ops::` function is the operation itself: who may do it, what
//! gets stamped, which gate applies. A door's job is extract → call → render, and nothing else.
//!
//! ★ THE ORDER IS PART OF THE LAW, not an accident of writing: borrow the acting pen, then stamp
//! authorship, then gate, then act. A gate that runs after the store has written is not a gate.

use crate::cli::session::Session;
use crate::error::Result;
use crate::names::Pen;
use crate::store::{MergeOutcome, Patch};

/// One upsert, as a value: the patch and the pen it CREDITS (if it credits a descendant rather than
/// itself). `credit` is a `Pen`, not a `&str`, because the two pens in this call — the one spending
/// the authority and the one taking the credit — are the transposition #239 exists to prevent.
pub struct Upsert {
    pub patch: Patch,
    pub credit: Option<Pen>,
}

/// Store one memory, on either door.
///
/// `caller` must be the PROVEN pen: the door refuses before it gets here (`Caller::pen`), so this
/// function cannot be handed an unidentified writer at all.
pub fn memory_upsert(session: &Session, caller: &Pen, cmd: Upsert) -> Result<MergeOutcome> {
    let mut patch = cmd.patch;
    // The key travels as a value, and the store's own slug rule is the one that decides. A key is
    // lowercased and collapsed by `resolve_key`; doing it here means the template carries the type
    // that later issues are plumbed with (soma/ellmephant#239).
    patch.key = patch
        .key
        .as_deref()
        .map(|key| crate::names::Key::slug(key).as_str().to_string());

    // ★ THE CREDIT IS NOT A RENAME OF THE WRITER (soma/ellmephant#192): the actor is always the
    // proven pen, and a credit only applies when it names somebody else.
    let credited = cmd.credit.filter(|credit| !credit.is(caller.as_str()));
    patch.asserted_by = Some(
        credited
            .as_ref()
            .map(|c| c.as_str().to_string())
            .unwrap_or_else(|| caller.as_str().to_string()),
    );
    patch.acted_by = Some(caller.as_str().to_string());

    session.channel_gate(caller.as_str(), crate::store::channel::Class::Record)?;
    if let Some(credit) = &credited {
        session.may_act_as(caller.as_str(), credit.as_str())?;
    }
    session.memory_merge(&patch)
}

/// Remove one memory. `None` means there was nothing to remove — a result, not an error.
///
/// The gate is unconditional: it used to run only `if let Some(caller)`, which is exactly how an
/// unidentified caller skipped a `gate: writes` channel.
pub fn memory_delete(session: &Session, caller: &Pen, key: &str) -> Result<Option<i64>> {
    session.channel_gate(caller.as_str(), crate::store::channel::Class::Record)?;
    if !session.rm(key, Some(caller.as_str()))? {
        return Ok(None);
    }
    Ok(Some(session.version_count(key)?))
}

/// Continue a subject with one line.
pub fn memory_append(
    session: &Session,
    caller: &Pen,
    key: &str,
    line: &str,
    force: bool,
) -> Result<crate::store::append::Appended> {
    session.channel_gate(caller.as_str(), crate::store::channel::Class::Record)?;
    session.append(key, line, Some(caller.as_str()), force)
}

/// Merge FROM into INTO. Destructive to the source, so the pen is the one whose word it is.
pub fn memory_merge(
    session: &Session,
    caller: &Pen,
    from: &str,
    into: &str,
) -> Result<crate::store::links::MergeInto> {
    session.channel_gate(caller.as_str(), crate::store::channel::Class::Record)?;
    session.merge_into(from, into, Some(caller.as_str()))
}

/// Fold FROM's lines into INTO without deleting the source — and without taking the keeper's write
/// authority, which is the whole point of the verb.
pub fn memory_fold(
    session: &Session,
    caller: &Pen,
    from: &str,
    into: &str,
) -> Result<crate::store::links::FoldOutcome> {
    session.channel_gate(caller.as_str(), crate::store::channel::Class::Record)?;
    session.fold_into(from, into, Some(caller.as_str()))
}

/// Store a project. The same gate as a memory write: a project is a record.
pub fn project_upsert(
    session: &Session,
    caller: &Pen,
    patch: &crate::store::project::Patch,
) -> Result<crate::store::project::MergeOutcome> {
    session.channel_gate(caller.as_str(), crate::store::channel::Class::Record)?;
    session.project_merge(patch)
}
