//! Sequential migrations driven by `PRAGMA user_version`.
//!
//! Each entry is applied inside its own transaction, and the version is bumped in
//! the same transaction so a failed migration leaves the file untouched.

use rusqlite::Connection;

use crate::error::Result;

/// `(version, sql, label)` in ascending order. SQL is embedded at compile time so
/// the binary carries its own schema.
const MIGRATIONS: &[(i32, &str, &str)] = &[
    (
        1,
        include_str!("../../migrations/0001_init.sql"),
        "init: memory, links, meta",
    ),
    (
        2,
        include_str!("../../migrations/0002_telemetry.sql"),
        "telemetry: visits, was_useful",
    ),
    (
        3,
        include_str!("../../migrations/0003_times_useful.sql"),
        "rename was_useful -> times_useful",
    ),
    (
        4,
        include_str!("../../migrations/0004_coherent.sql"),
        "coherent: split-review verdict",
    ),
    (
        5,
        include_str!("../../migrations/0005_projects.sql"),
        "projects: project, project_memory",
    ),
    (
        6,
        include_str!("../../migrations/0006_link_type.sql"),
        "link type: bare keys, memory_link.to_type",
    ),
    (
        7,
        include_str!("../../migrations/0007_drop_project_memory.sql"),
        "retire the one-home association: drop project_memory",
    ),
    (
        8,
        include_str!("../../migrations/0008_encode_queue.sql"),
        "encode queue: the documents that still owe the vector index",
    ),
    (
        9,
        include_str!("../../migrations/0009_drop_coherent.sql"),
        "drop coherent: the split-review verdict leaves the schema",
    ),
    (
        10,
        include_str!("../../migrations/0010_rename_doc_hash.sql"),
        "rename doc_hash -> encode_hash: the column says it is this row's embedding state",
    ),
    (
        11,
        include_str!("../../migrations/0011_memory_versions.sql"),
        "memory versions: the state a document change replaces",
    ),
    (
        12,
        include_str!("../../migrations/0012_claims.sql"),
        "claims: an atomic lease over a name",
    ),
    (
        13,
        include_str!("../../migrations/0013_mail.sql"),
        "mail: addressed rows, read by the recipient's cursor",
    ),
    (
        14,
        include_str!("../../migrations/0014_provenance.sql"),
        "provenance: asserted_by (stamped by the store) + origin (declared by the caller)",
    ),
    (
        15,
        include_str!("../../migrations/0015_multi_kind_queue.sql"),
        "the queue keys on a keyslug and carries the rendered text: no hash, no kind column, no retries",
    ),
    (
        16,
        include_str!("../../migrations/0016_drop_encode_hash.sql"),
        "drop memory.encode_hash: the stamp describing a derived index leaves the truth row",
    ),
    (
        17,
        include_str!("../../migrations/0017_memory_fts.sql"),
        "the word index: FTS5 over key/title/body, with all three triggers (soma/ellmephant#62)",
    ),
    (
        18,
        include_str!("../../migrations/0018_memory_audience.sql"),
        "the read audience: public, or exactly one pen (#54/#55)",
    ),
    (
        19,
        include_str!("../../migrations/0019_mail_cursor.sql"),
        "mail cursor: one row per (reader, box), reader resolved from the proven pen",
    ),
    (
        20,
        include_str!("../../migrations/0020_last_written_by.sql"),
        "authorship is write-once; `last_written_by` carries the writer that moves (#107)",
    ),
    (
        21,
        include_str!("../../migrations/0021_pen_registry.sql"),
        "the pen registry: identity as a row, with a parent and a tombstone, so `owner` can exist (#109)",
    ),
    (
        22,
        include_str!("../../migrations/0022_edited_by.sql"),
        "`edited_by`: the author of the state in each snapshot — every record says who wrote it (#109)",
    ),
    (
        23,
        include_str!("../../migrations/0023_owner.sql"),
        "`owner`: responsibility (nullable = commons) plus the ledger of adopt/disclaim/transfer (#109)",
    ),
    (
        24,
        include_str!("../../migrations/0024_sudo.sql"),
        "`sudoers` + `active_sudo`: authority borrowed, bounded and revocable — `estate` has no identity, so its reach is only ever a borrow (#109)",
    ),
    (
        25,
        include_str!("../../migrations/0025_pen_repair.sql"),
        "repair: `anonymous` is a value meaning nobody, never a pen — un-own what the seed gave it (#109)",
    ),
    (
        26,
        include_str!("../../migrations/0026_read_audit.sql"),
        "`private_read`: a read that crosses a boundary is RECORDED, not merely allowed (#129, half of #120)",
    ),
    (
        27,
        include_str!("../../migrations/0027_pen_name_nocase.sql"),
        "pen names are unique CASE-INSENSITIVELY while keeping their spelling — `James` and `james` are one pen",
    ),
    (
        28,
        include_str!("../../migrations/0028_read_borrowed.sql"),
        "`private_read.reason` gains `borrowed`: a read let through by a HELD SESSION is not a subtree read",
    ),
    (
        29,
        include_str!("../../migrations/0029_ellmechat.sql"),
        "ellmechat: channels, the message log, and the delivery ledger — told, presented, listened (#151)",
    ),
    (
        30,
        include_str!("../../migrations/0030_mail_surrogate.sql"),
        "mail surrogates are a GRANT with a scope and a revocation, not an environment variable (#153)",
    ),
    (
        31,
        include_str!("../../migrations/0031_mail_sent_under.sql"),
        "`mail.sent_under`: which grant authorised a surrogate letter — a receipt a reader can check (#154)",
    ),
    (
        32,
        include_str!("../../migrations/0032_channel_admin.sql"),
        "`channel_event`: who created a channel and every gate change after it — a lock gets a name (#161)",
    ),
    (
        33,
        include_str!("../../migrations/0033_channel_retention.sql"),
        "`channel.keep_secs`: retention as a policy on the row; 0 = forever (#163)",
    ),
    (
        34,
        include_str!("../../migrations/0034_channel_boundary.sql"),
        "`channel_boundary`: a turn boundary names itself, so a retry is not a second miss (#159 review)",
    ),
    (
        35,
        include_str!("../../migrations/0035_channel_event_actions.sql"),
        "`channel_event` accepts every action the code writes, via a rebuild — a migration is immutable once it can merge (#168 review)",
    ),
    (
        36,
        include_str!("../../migrations/0036_claim_release_log.sql"),
        "claim release log: who ended a lease they did not hold, and why",
    ),
    (
        37,
        include_str!("../../migrations/0037_pen_credential.sql"),
        "pen credentials: a pen's secret is a hashed row, not an environment variable",
    ),
    (
        38,
        include_str!("../../migrations/0038_config.sql"),
        "`config`: a stored value with a written ACL — who may set it and who may read it, in columns",
    ),
    (
        39,
        include_str!("../../migrations/0039_config_seed_recess.sql"),
        "seed: the daily recess amount and grant time are config rows (estate writes, public reads)",
    ),
    (
        40,
        include_str!("../../migrations/0040_config_seed_sitting.sql"),
        "seed: one recess sitting is 20m; the 2x hard ceiling is derived, and reaching it is a bug",
    ),
];

/// Current schema version recorded in the file.
pub fn version(conn: &Connection) -> Result<i32> {
    Ok(conn.query_row("PRAGMA user_version", [], |r| r.get(0))?)
}

/// Highest version this binary knows how to create.
pub fn latest() -> i32 {
    MIGRATIONS.iter().map(|(v, _, _)| *v).max().unwrap_or(0)
}

/// Every version this binary knows, in order.
fn versions() -> Vec<i32> {
    MIGRATIONS.iter().map(|(v, _, _)| *v).collect()
}

/// The escape hatch for a ROLLBACK, and only for a rollback.
///
/// Forward migrations are one-way, so the previous image can meet a store a newer binary already
/// migrated. Refusing there turns "roll back the bad deploy" into "restore from backup" — during the
/// incident, at the moment it hurts most. So a store **newer** than this binary can be entered
/// deliberately, by an operator typing this during an incident: never by default, and never for a gap.
pub const ALLOW_STORE_AHEAD: &str = "ELLMEPHANT_ALLOW_STORE_AHEAD";

/// Apply every migration newer than the file's `user_version`.
pub fn run(conn: &Connection) -> Result<()> {
    let allow_ahead = std::env::var(ALLOW_STORE_AHEAD)
        .map(|v| v == "1")
        .unwrap_or(false);
    run_with(conn, allow_ahead)
}

/// [`run`], with the override passed in so both sides of it can be tested.
///
/// Two states look identical from a bare counter and must not be treated alike:
///
/// - **the store is NEWER than this binary** — a rollback. Allowed with [`ALLOW_STORE_AHEAD`], loudly,
///   because a guard intended for bugs must not make a bad deploy unrecoverable.
/// - **a version is missing from the middle of the list** — a skipped migration. Refused with **no**
///   override: the migration never ran, so proceeding reads a schema this binary believes it created,
///   which is the silent no-op this whole check exists to end.
pub fn run_with(conn: &Connection, allow_ahead: bool) -> Result<()> {
    let mut current = version(conn)?;
    check_version(current, &versions(), allow_ahead)?;
    for (version, sql, _label) in MIGRATIONS {
        if *version <= current {
            continue;
        }
        let tx = conn.unchecked_transaction()?;
        tx.execute_batch(sql)?;
        tx.pragma_update(None, "user_version", version)?;
        tx.commit()?;
        current = *version;
    }
    Ok(())
}

/// The decision, split from the loop so both sides can be tested against a synthetic version list.
///
/// Two states look identical from a bare counter and must not be treated alike — see [`run_with`].
fn check_version(current: i32, known: &[i32], allow_ahead: bool) -> Result<()> {
    if current == 0 {
        return Ok(());
    }
    let max = known.iter().copied().max().unwrap_or(0);
    if current > max {
        if !allow_ahead {
            return Err(crate::error::Error::Config(format!(
                "STORE IS NEWER THAN THIS BINARY: the store is at schema v{current} and this binary \
                 knows up to v{max}. That is what a rollback looks like. Refusing to read a schema \
                 it cannot honour; if this is deliberate, set {ALLOW_STORE_AHEAD}=1 and it will \
                 proceed with a loud warning."
            )));
        }
        tracing::warn!(
            store = current,
            binary = max,
            "proceeding on a store NEWER than this binary: a rollback is in progress, and any \
             migration this binary is missing has NOT been undone"
        );
    } else if !known.contains(&current) {
        return Err(crate::error::Error::Config(format!(
            "the store is at schema v{current}, which is MISSING from this binary's migration list \
             (it knows {}). A migration was skipped and never applied. This is refused with no \
             override: proceeding would read a schema this binary believes it created.",
            known
                .iter()
                .map(|v| v.to_string())
                .collect::<Vec<_>>()
                .join(",")
        )));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A list with a HOLE in it: 3 was never applied, so a store at 3 is a skipped migration.
    /// This is the case the real list cannot express while it is contiguous — which is exactly why the
    /// decision is a function of the list rather than of the compiled one.
    #[test]
    fn a_hole_in_the_list_is_refused_even_with_the_rollback_override() {
        let holey = [1, 2, 4];
        for allow in [false, true] {
            let err = check_version(3, &holey, allow)
                .expect_err("a skipped migration must be refused, override or not");
            let text = format!("{err}");
            assert!(
                text.contains("MISSING") && text.contains("no override"),
                "{text}"
            );
        }
        // A version the list DOES contain is fine, and so is a fresh store.
        assert!(check_version(4, &holey, false).is_ok());
        assert!(check_version(0, &holey, false).is_ok());
    }

    /// The store being AHEAD of the binary is a rollback, not a bug, and a bad deploy must stay
    /// recoverable — but only by an act, never by default.
    #[test]
    fn a_store_ahead_is_refused_by_default_and_allowed_by_an_act() {
        let holey = [1, 2, 4];
        let err = check_version(9, &holey, false).expect_err("refused by default");
        let text = format!("{err}");
        assert!(
            text.contains("NEWER") && text.contains(ALLOW_STORE_AHEAD),
            "{text}"
        );
        assert!(
            check_version(9, &holey, true).is_ok(),
            "the override proceeds"
        );
    }
}
