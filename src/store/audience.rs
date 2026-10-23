//! WHO may be shown a row: the `private` read tier (soma/ellmephant#54, #55).
//!
//! The ladder James named: **plain** (any reader) → **private** (the pen that wrote it, plus the
//! owner for forensics) → **secret** (key holders; encryption, `secret-tiering`). `protected` is
//! NOT on it — that is a write gate under a lease, live since 2026-10-17, and a row can be either
//! without the other.
//!
//! ★ ONE PREDICATE, ONE PLACE. `private` FAILS OPEN by construction (the row is in the file in
//! plaintext and it is embedded in a shared index), so every surface that can name a document has to
//! filter. The filter must therefore be expressed ONCE — [`Reader::sees`] and [`Reader::sql`] — and
//! every read path must go through it. A handler that can build a query against `memory` without a
//! [`Reader`] is a handler that can leak, which is why the unscoped reads are named `*_unscoped` and
//! are crate-private.
//!
//! ★ ABSENCE, NOT REFUSAL. For a caller who may not see a row, the row does not exist: no hit, no
//! title, no count, no backlink, no dangling edge. A refusal that says "this is private" confirms the
//! row's existence, which is the disclosure. [`Reader::sees`] is a question about a row the caller has
//! ALREADY been handed; the read paths must ask it before returning anything at all.
//!
//! ★ THE OWNER IS NOT A READER. James's requirement is forensics — "if something goes badly wrong I
//! must be able to look" — and that is a different act from reading: it is [`Reader::forensic`],
//! explicit, one row, and recorded ([`crate::store::forensic`]). An owner who was simply exempt
//! would make the tier a word.

/// The audience value every row starts with and most rows keep.
pub const PUBLIC: &str = "public";

/// The prefix of a pen-scoped audience.
pub const PEN: &str = "pen:";

/// One pen's audience value. Lowercased, so `Stet` and `stet` are one pen — the same rule the claim
/// table already applies, and the alternative is two spellings of one privacy boundary.
pub fn pen(name: &str) -> String {
    format!("{PEN}{}", name.trim().to_ascii_lowercase())
}

/// The value a row is stored with: an unset audience is a public one. `Memory::default` leaves the
/// string empty, and an empty audience would be a row nobody could see — the worst failure direction.
pub fn effective(value: &str) -> &str {
    if value.trim().is_empty() {
        PUBLIC
    } else {
        value
    }
}

/// The serde default: an omitted audience is a public one.
pub fn default_audience() -> String {
    PUBLIC.to_string()
}

/// Is this a well-formed audience? Refused on write, so a stored row cannot carry a value no reader
/// can interpret — the failure mode of an unparseable audience would be a row nobody can see.
pub fn is_valid(value: &str) -> bool {
    value == PUBLIC
        || value
            .strip_prefix(PEN)
            .is_some_and(|name| !name.is_empty() && !name.contains(','))
}

/// The audience a write carries: either public, or private to exactly the pen that is writing.
///
/// Derived from the caller's PROVEN identity, never from an argument — the containment law shared
/// with namespaces and with every other permission in this store: a boundary a caller can name is a
/// boundary an injection picks.
pub fn for_write(private: bool, who: Option<&str>) -> crate::error::Result<String> {
    if !private {
        return Ok(PUBLIC.to_string());
    }
    match who {
        Some(name) if !name.eq_ignore_ascii_case(crate::identity::ANONYMOUS) => Ok(pen(name)),
        _ => Err(crate::error::Error::Invalid(
            "a `private` write needs a PROVEN pen: it is private TO SOMEONE, and an anonymous \
             connection is nobody. Send the identity pair, or write it public."
                .into(),
        )),
    }
}

/// Who is reading. Resolved once per request from the connection's proven identity.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Reader {
    /// The proven pen, already lowercased into an audience value. `None` is an anonymous reader.
    pen: Option<String>,
    /// The addresses this pen reaches DOWN into — its descendants' audiences, resolved from the pen
    /// tree at the moment of use. Parentage is the grant (#109): a pen may read what its subtree
    /// addressed to itself, which is what makes cluster-wide hygiene possible for a doctor who is
    /// not the author of anything. Empty for a pen with no descendants, and for a reader built
    /// without a connection — the narrow answer, never the wide one by accident.
    down: Vec<String>,
    /// Every row, private ones included. ONLY for a process that already holds the file — the CLI,
    /// maintenance, tests. A tier that claims to stop someone holding the SQLite file would be a lie
    /// (#54 says so out loud), and pretending the CLI is subject to it would only hide that.
    all: bool,
}

impl Reader {
    /// A proven pen.
    pub fn pen(name: &str) -> Self {
        Reader {
            pen: Some(pen(name)),
            down: Vec::new(),
            all: false,
        }
    }

    /// ★ Widen a pen reader to its SUBTREE, read from the pen table now — not at login, so a pen
    /// that gained a child since the session opened reaches it, and a retired one stops. Any failure
    /// leaves the reader narrow: a store whose tree cannot be read is a store where nobody's reach
    /// may be guessed (`a hobbled Cline is better than no Cline`).
    pub fn in_tree(mut self, conn: &rusqlite::Connection) -> Self {
        let Some(p) = self.pen.clone() else {
            return self;
        };
        let name = p.strip_prefix(PEN).unwrap_or(&p).to_string();
        // `UNION`, not `UNION ALL`: a cycle in provisioning must not hang a read.
        let sql = "WITH RECURSIVE sub(id) AS (
                       SELECT id FROM pen WHERE lower(name) = lower(?1)
                       UNION
                       SELECT c.id FROM pen c JOIN sub s ON c.parent_id = s.id
                   )
                   SELECT DISTINCT 'pen:' || lower(p.name) FROM pen p JOIN sub s ON p.id = s.id";
        let Ok(mut stmt) = conn.prepare(sql) else {
            return self;
        };
        let Ok(rows) = stmt.query_map(rusqlite::params![name], |r| r.get::<_, String>(0)) else {
            return self;
        };
        for row in rows.flatten() {
            if Some(&row) != self.pen.as_ref() {
                self.down.push(row);
            }
        }
        self
    }

    /// How many descendant addresses this reader reaches down into (0 for a leaf).
    pub fn down(&self) -> usize {
        self.down.len()
    }

    /// The proven pen's NAME, without the audience prefix — what a boot address is spelled from.
    pub fn pen_name(&self) -> Option<&str> {
        self.pen.as_deref().and_then(|p| p.strip_prefix(PEN))
    }

    /// A reader with no pen: sees public rows only.
    pub fn anonymous() -> Self {
        Reader::default()
    }

    /// The process holding the file: the CLI, the maintenance paths, tests. Not a pen, and not a
    /// forensic act — it is the honest limit of the tier, written down rather than implied.
    pub fn everything() -> Self {
        Reader {
            pen: None,
            down: Vec::new(),
            all: true,
        }
    }

    /// The pen as an audience value (`pen:<x>`), when there is one.
    pub fn as_audience(&self) -> Option<&str> {
        self.pen.as_deref()
    }

    /// May this reader be shown a row with this audience?
    ///
    /// Written as an allow-list, not a deny-list: an audience value nobody understands is visible to
    /// nobody. The opposite choice would make a typo in a writer a silent publication.
    pub fn sees(&self, audience: &str) -> bool {
        self.all
            || audience == PUBLIC
            || Some(audience) == self.pen.as_deref()
            || self.down.iter().any(|a| a == audience)
    }

    /// The SQL predicate over `memory.audience`, and the parameters it binds.
    ///
    /// The shape is the same question as [`Reader::sees`], so the two cannot drift: `?` is the
    /// reader's own audience, bound once. A caller composing a query passes the returned fragment as
    /// `AND {predicate}` and appends the returned binds in the order they are returned.
    pub fn sql(&self) -> (String, Vec<String>) {
        if self.all {
            return ("1 = 1".to_string(), Vec::new());
        }
        match &self.pen {
            Some(p) => {
                let mut binds = vec![p.clone()];
                binds.extend(self.down.iter().cloned());
                let holes = std::iter::repeat_n("?", binds.len())
                    .collect::<Vec<_>>()
                    .join(", ");
                (
                    format!("(audience = 'public' OR audience IN ({holes}))"),
                    binds,
                )
            }
            None => ("audience = 'public'".to_string(), Vec::new()),
        }
    }
}

/// A forensic read: the owner looking at a row they may not otherwise see.
///
/// It carries the REASON, because an exemption without a why is indistinguishable from a backdoor in
/// the record. The reader's pen and this reason are both written to `private_read` (see
/// [`crate::store::forensic`]) — the audit is the point, not a courtesy.
#[derive(Debug, Clone)]
pub struct Forensic<'a> {
    /// The owner's proven pen.
    pub pen: &'a str,
    /// Why the exemption is being used. Required, and stored.
    pub reason: &'a str,
}

impl Reader {
    /// The ids this reader may see, or `None` when **nothing is hidden** — the common case today.
    ///
    /// `None` is not an optimisation dressed as semantics: it is the statement "this store has no
    /// private rows, so every query over it is already exact". When it is `Some`, the set is the
    /// allow-list every candidate list must be intersected with BEFORE `k` truncation, or a private
    /// row would consume a slot and the caller's page would come back thin for no visible reason.
    pub fn visible_ids(
        &self,
        conn: &rusqlite::Connection,
    ) -> crate::error::Result<Option<std::collections::HashSet<i64>>> {
        let total: i64 = conn.query_row("SELECT count(*) FROM memory", [], |r| r.get(0))?;
        if self.all {
            return Ok(None);
        }
        let (predicate, binds) = self.sql();
        let visible: i64 = conn.query_row(
            &format!("SELECT count(*) FROM memory WHERE {predicate}"),
            rusqlite::params_from_iter(binds.iter()),
            |r| r.get(0),
        )?;
        if visible == total {
            return Ok(None);
        }
        let mut stmt = conn.prepare(&format!("SELECT id FROM memory WHERE {predicate}"))?;
        let rows = stmt.query_map(rusqlite::params_from_iter(binds.iter()), |r| {
            r.get::<_, i64>(0)
        })?;
        let mut set = std::collections::HashSet::new();
        for row in rows {
            set.insert(row?);
        }
        Ok(Some(set))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn public_is_everyones_and_a_pen_is_one_pen() {
        assert!(Reader::anonymous().sees(PUBLIC));
        assert!(Reader::pen("stet").sees(PUBLIC));
        assert!(Reader::pen("stet").sees("pen:stet"));
        assert!(!Reader::pen("stet").sees("pen:james"));
        assert!(
            !Reader::anonymous().sees("pen:stet"),
            "a penless reader is not a pen: it sees public rows only"
        );
    }

    #[test]
    fn audience_values_are_one_spelling() {
        assert_eq!(pen("Stet"), "pen:stet");
        assert_eq!(pen(" STET "), "pen:stet");
        assert!(is_valid("public"));
        assert!(is_valid("pen:stet"));
        assert!(!is_valid("private"));
        assert!(!is_valid("pen:"));
        assert!(!is_valid("pen:a,b"));
    }

    #[test]
    fn a_private_write_needs_a_proven_pen() {
        assert_eq!(for_write(false, None).unwrap(), PUBLIC);
        assert_eq!(for_write(true, Some("Stet")).unwrap(), "pen:stet");
        assert!(
            for_write(true, None).is_err(),
            "private to nobody is not a tier"
        );
        assert!(
            for_write(true, Some(crate::identity::ANONYMOUS)).is_err(),
            "anonymous is a truth, not a pen"
        );
    }

    #[test]
    fn the_sql_predicate_says_the_same_thing_as_sees() {
        let (sql, binds) = Reader::anonymous().sql();
        assert_eq!(sql, "audience = 'public'");
        assert!(binds.is_empty());
        // A pen with no tree behind it binds exactly its own address — the narrow answer is what a
        // reader built without a connection gets, and it must not widen by accident.
        let (sql, binds) = Reader::pen("stet").sql();
        assert!(sql.contains("audience IN"), "{sql}");
        assert_eq!(binds, vec!["pen:stet".to_string()]);
        assert_eq!(Reader::pen("stet").down(), 0);

        // One bind per address: the predicate's holes and its binds are one list, built together, so
        // they cannot drift into a `SQLITE_RANGE` at runtime.
        assert_eq!(sql.matches('?').count(), binds.len());
    }
}
