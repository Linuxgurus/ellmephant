use crate::error::Result;
use crate::names::Pen;
use crate::store::session::Session;
use crate::store::{MergeOutcome, Patch};

pub struct Upsert {
    pub patch: Patch,
    pub credit: Option<Pen>,
}

pub fn memory_upsert(session: &Session, caller: &Pen, cmd: Upsert) -> Result<MergeOutcome> {
    let mut patch = cmd.patch;
    patch.key = patch
        .key
        .as_deref()
        .map(|key| crate::names::Key::slug(key).as_str().to_string());

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

pub fn memory_delete(session: &Session, caller: &Pen, key: &str) -> Result<Option<i64>> {
    session.channel_gate(caller.as_str(), crate::store::channel::Class::Record)?;
    if !session.rm(key, Some(caller.as_str()))? {
        return Ok(None);
    }
    Ok(Some(session.version_count(key)?))
}

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

pub fn memory_merge(
    session: &Session,
    caller: &Pen,
    from: &str,
    into: &str,
) -> Result<crate::store::links::MergeInto> {
    session.channel_gate(caller.as_str(), crate::store::channel::Class::Record)?;
    session.merge_into(from, into, Some(caller.as_str()))
}

pub fn memory_fold(
    session: &Session,
    caller: &Pen,
    from: &str,
    into: &str,
) -> Result<crate::store::links::FoldOutcome> {
    session.channel_gate(caller.as_str(), crate::store::channel::Class::Record)?;
    session.fold_into(from, into, Some(caller.as_str()))
}

pub fn project_upsert(
    session: &Session,
    caller: &Pen,
    patch: &crate::store::project::Patch,
) -> Result<crate::store::project::MergeOutcome> {
    session.channel_gate(caller.as_str(), crate::store::channel::Class::Record)?;
    session.project_merge(patch)
}

pub fn memory_adopt(
    session: &Session,
    caller: &Pen,
    key: &str,
    to: Option<&str>,
    force: bool,
) -> Result<()> {
    session.channel_gate(caller.as_str(), crate::store::channel::Class::Record)?;
    match to {
        Some(target) => session.assign(key, caller.as_str(), target, force),
        None => session.adopt(key, caller.as_str(), force),
    }
}

pub fn channel_prune(
    session: &Session,
    caller: &Pen,
    channel: Option<&str>,
) -> Result<Vec<crate::store::channel::Pruned>> {
    session.channel_gate(caller.as_str(), crate::store::channel::Class::Acting)?;
    session.channel_prune(channel)
}
