//! WHO may be shown a row: the `private` read tier (soma/ellmephant#54, #55).
//!
//! The ladder James named: **plain** (any reader) → **private** (the pen that wrote it **and every
//! pen above it** — parentage is the grant, so an ancestor reads down into its subtree, #120) →
//! **secret** (key holders; encryption, `secret-tiering`). `protected` is
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
//! must be able to look" — and that is a different act from reading: explicit, one row, and recorded.
//!
//! ⚠️ **AND IT IS NOT BUILT.** This module used to describe a [`Reader::forensic`] and a
//! `private_read` table as though they existed; they do not — there is no `forensic` constructor, no
//! table and no `store::forensic` module (checked 2026-10-23). So today the tier ENFORCES two things
//! and records neither: a pen reads its own rows, and an ancestor reads its subtree, both silently.
//! The enforcement is real; the audit is a promise. Naming that is better than a citation that
//! resolves to nothing, and it is the same class as the parity table that said 13 tools when there
//! were 32 — prose is not guarded by any test.
//!
//! What a real forensic path needs deciding before it is built: what a record holds (pen, key, when,
//! reason), whether ONE row or a whole subtree, and retention — an audit table that grows forever is
//! a second store nobody maintains.

use rusqlite::OptionalExtension as _;

/// The audience value every row starts with and most rows keep.
pub const PUBLIC: &str = "public";

/// The prefix of a pen-scoped audience: EXACTLY that pen.
pub const PEN: &str = "pen:";

/// The prefix of a subtree audience: that pen AND everything below it.
pub const SUBTREE: &str = "subtree:";

/// How many addresses one private row may carry. Beyond this it is a mailing list, not a secret.
pub const MAX_TARGETS: usize = 8;

/// Which of the two ways a pen can be named in an audience.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Kind {
    /// Exactly that pen.
    Pen,
    /// That pen and every pen below it.
    Subtree,
}

/// One address in an audience string.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Target {
    pub kind: Kind,
    /// The pen's name, lowercased — one spelling per boundary.
    pub name: String,
}

impl Target {
    /// The token as it is stored: `pen:<name>` or `subtree:<name>`.
    pub fn token(&self) -> String {
        match self.kind {
            Kind::Pen => format!("{PEN}{}", self.name),
            Kind::Subtree => format!("{SUBTREE}{}", self.name),
        }
    }
}

/// Parse an audience. `None` means a value no reader can interpret, which a write must refuse: an
/// unparseable audience is a row nobody can see, and failing open would be worse.
///
/// Grammar: `public`, or one to [`MAX_TARGETS`] comma-separated `pen:<name>` / `subtree:<name>`
/// tokens. Names are lowercased; empty names, whitespace, and duplicates are refused.
pub fn parse(value: &str) -> Option<Vec<Target>> {
    let value = value.trim();
    if value == PUBLIC {
        return Some(Vec::new());
    }
    let mut out: Vec<Target> = Vec::new();
    for raw in value.split(',') {
        let token = raw.trim().to_ascii_lowercase();
        // Prefixes are matched case-insensitively (the name is lowercased anyway): `Pen:Stet` is the
        // same boundary as `pen:stet`, and refusing it would be a spelling test, not a security one.
        let (kind, name) = token
            .strip_prefix(PEN)
            .map(|n| (Kind::Pen, n))
            .or_else(|| token.strip_prefix(SUBTREE).map(|n| (Kind::Subtree, n)))?;
        let name = name.trim().to_ascii_lowercase();
        if name.is_empty()
            || name.contains(char::is_whitespace)
            || name == crate::identity::ANONYMOUS
        {
            return None;
        }
        let t = Target { kind, name };
        if out.iter().any(|x| x.token() == t.token()) {
            return None; // a duplicate address is a typo, not a second grant
        }
        out.push(t);
    }
    if out.is_empty() || out.len() > MAX_TARGETS {
        return None;
    }
    Some(out)
}

/// Normalise an audience for storage: the parsed tokens, joined, in the order given.
pub fn normalise(value: &str) -> crate::error::Result<String> {
    parse(value)
        .map(|targets| {
            if targets.is_empty() {
                PUBLIC.to_string()
            } else {
                targets.iter().map(Target::token).collect::<Vec<_>>().join(",")
            }
        })
        .ok_or_else(|| {
            crate::error::Error::Invalid(format!(
                "`{value}` is not an audience: use `public`, or up to {MAX_TARGETS} `pen:<name>` /                  `subtree:<name>` addresses, comma-separated, each naming a pen that exists"
            ))
        })
}

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
    parse(value).is_some()
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

/// Normalise and CHECK an audience a caller supplied: every address must name a pen that exists.
///
/// ★ A SHARE IS AN ARGUMENT; A PRIVACY BOUNDARY IS NOT (2026-10-24). `private: true` is derived from
/// the proven pen and cannot name anyone else. This is the other half: an explicit audience DOES name
/// others, so it must be checked at write time — an audience naming a pen that does not exist is a row
/// nobody can see, which fails CLOSED and silently.
pub fn validated(conn: &rusqlite::Connection, spec: &str) -> crate::error::Result<String> {
    let normal = normalise(spec)?;
    if normal == PUBLIC {
        return Ok(normal);
    }
    for target in parse(spec).unwrap_or_default() {
        let exists: Option<i64> = conn
            .query_row(
                "SELECT id FROM pen WHERE lower(name) = lower(?1)",
                rusqlite::params![&target.name],
                |r| r.get(0),
            )
            .optional()?;
        if exists.is_none() {
            return Err(crate::error::Error::Invalid(format!(
                "`{}` names no pen: an audience must name a pen that exists, or it is a row nobody \
                 can ever read",
                target.name
            )));
        }
    }
    Ok(normal)
}

/// Who is reading. Resolved once per request from the connection's proven identity.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Reader {
    /// The proven pen, already lowercased into an audience value. `None` is an anonymous reader.
    pen: Option<String>,
    /// The audience TOKENS this reader may match besides its own `pen:<name>`: `subtree:<ancestor>`
    /// for each pen above it, and `pen:<p>` / `subtree:<a>` for each pen whose authority it holds.
    /// With `pen:` now meaning EXACTLY one pen (James, 2026-10-24), an ancestor is no longer implied:
    /// it has to arrive as an explicit `subtree:` address here.
    extra: Vec<String>,
    /// The `subtree:` tokens this reader holds by TREE (its own ancestors), kept apart from borrowed
    /// ones so the audit can say which mechanism let a read through.
    own_subtree: Vec<String>,
    /// The pens whose authority this reader currently holds (live sessions, validated at use).
    borrowed_pens: Vec<String>,
    /// Acting as a ROOT pen (the estate, or a session to it). A root sees every row: that is the
    /// break-glass James named — "estate can always peek" — and it is explicit rather than a side
    /// effect of ancestry.
    root: bool,
    /// The addresses this pen reaches DOWN into — its descendants' audiences, resolved from the pen
    /// tree at the moment of use. Parentage is the grant (#109): a pen may read what its subtree
    /// addressed to itself, which is what makes cluster-wide hygiene possible for a doctor who is
    /// not the author of anything. Empty for a pen with no descendants, and for a reader built
    /// without a connection — the narrow answer, never the wide one by accident.
    down: Vec<String>,
    /// Addresses reached by BORROWING somebody's authority rather than by parentage (2026-10-24).
    /// Kept apart from `down` so the audit can say which mechanism let the read through: "an ancestor
    /// read down its subtree" and "a pen spent a session it was lent" are different acts.
    borrowed: Vec<String>,
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
            extra: Vec::new(),
            own_subtree: Vec::new(),
            borrowed_pens: Vec::new(),
            root: false,
            down: Vec::new(),
            borrowed: Vec::new(),
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
        // ★ ANCESTORS, NOT DESCENDANTS (2026-10-24). `pen:` is exact now, so what a tree gives a
        // reader is the right to match `subtree:<ancestor>` — a share aimed at a whole branch. A
        // descendant's private row addressed `pen:<them>` is theirs, and reading it needs either an
        // explicit `subtree:` on the row or the borrowed root.
        let sql = "WITH RECURSIVE up(id, parent_id) AS (
                       SELECT id, parent_id FROM pen WHERE lower(name) = lower(?1)
                       UNION
                       SELECT p.id, p.parent_id FROM pen p JOIN up u ON p.id = u.parent_id
                   )
                   SELECT DISTINCT 'subtree:' || lower(p.name) FROM pen p JOIN up u ON p.id = u.id";
        let Ok(mut stmt) = conn.prepare(sql) else {
            return self;
        };
        let Ok(rows) = stmt.query_map(rusqlite::params![name], |r| r.get::<_, String>(0)) else {
            return self;
        };
        for row in rows.flatten() {
            if !self.extra.contains(&row) {
                self.own_subtree.push(row.clone());
                self.extra.push(row);
            }
        }
        self
    }

    /// ★ A HELD SESSION IS REACH (2026-10-24). A pen holding `powerful` for a while reads what
    /// `powerful` reads — the whole point of borrowing the root, and the reason `active_sudo` stops being
    /// a ledger nothing consults. Resolved at use, like [`Reader::in_tree`]: a released, expired or
    /// revoked session stops answering on the next read, and nothing is cached in the reader.
    ///
    /// A failure leaves the reader NARROW — a store whose sessions cannot be read is not a store where
    /// anyone's extra reach may be assumed.
    pub fn with_borrowed(mut self, conn: &rusqlite::Connection) -> Self {
        let Some(p) = self.pen.clone() else {
            return self;
        };
        let me = p.strip_prefix(PEN).unwrap_or(&p).to_string();
        // ★ VALIDATED AT USE, through the same door the permission checks use (`sudo::reachable`): a
        // lend whose parentage has since moved grants nothing, and an expired or released row stops
        // answering on the next read. Resolving this with raw SQL here would let the reader and the
        // writer disagree about what a session is worth.
        let Ok(held) = crate::store::sudo::reachable(conn, &me) else {
            return self;
        };
        for (pid, name, _source) in held {
            let bare = name.trim().to_ascii_lowercase();
            // Holding P's authority means reading rows addressed to P (exact), and rows shared to a
            // subtree that CONTAINS P (so each of P's ancestors, as a `subtree:` token).
            let token = format!("{PEN}{bare}");
            if !self.extra.contains(&token) {
                self.extra.push(token.clone());
            }
            if !self.borrowed_pens.contains(&bare) {
                self.borrowed_pens.push(bare.clone());
                self.borrowed.push(token);
            }
            // A root pen is the estate: holding it is the break-glass, and it reads everything.
            let parentless: Option<bool> = conn
                .query_row(
                    "SELECT parent_id IS NULL FROM pen WHERE id = ?1",
                    rusqlite::params![pid],
                    |r| r.get(0),
                )
                .ok();
            if parentless == Some(true) {
                self.root = true;
            }
            let sql = "WITH RECURSIVE up(id, parent_id) AS (
                           SELECT id, parent_id FROM pen WHERE id = ?1
                           UNION
                           SELECT p.id, p.parent_id FROM pen p JOIN up u ON p.id = u.parent_id
                       )
                       SELECT DISTINCT lower(p.name) FROM pen p JOIN up u ON p.id = u.id";
            if let Ok(mut stmt) = conn.prepare(sql)
                && let Ok(rows) = stmt.query_map(rusqlite::params![pid], |r| r.get::<_, String>(0))
            {
                for row in rows.flatten() {
                    let t = format!("{SUBTREE}{row}");
                    if !self.extra.contains(&t) {
                        self.extra.push(t.clone());
                        if !self.borrowed.contains(&t) {
                            self.borrowed.push(t);
                        }
                    }
                }
            }
        }
        self
    }

    /// The pen whose authority was spent to reach `audience`, if it was reached by borrowing rather than
    /// by a `subtree:` share from your own tree. Used by the audit: a borrowed read is not a subtree read.
    pub fn is_borrowed(&self, audience: &str) -> bool {
        if self.exact(audience) {
            return false;
        }
        // A ROOT's read is always a break-glass read, whether the row names the root or not: the
        // record should say "borrowed the estate", never "read down its own subtree".
        if Self::matches(audience, &self.own_subtree) {
            return false; // reached by the shape of the tree, not by spending anything
        }
        self.root || Self::matches(audience, &self.borrowed)
    }

    /// Act as a ROOT pen: see every row (the estate's break-glass). Used by the process holding the
    /// file, and by a session to a parentless pen — never inferred from being high in the tree.
    pub fn as_root(mut self) -> Self {
        self.root = true;
        self
    }

    /// How many extra addresses this reader holds besides its own name (ancestor `subtree:` shares,
    /// and the pens it is borrowing). 0 for a pen with no tree and no session.
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
            extra: Vec::new(),
            own_subtree: Vec::new(),
            borrowed_pens: Vec::new(),
            root: true,
            borrowed: Vec::new(),
            down: Vec::new(),
            all: true,
        }
    }

    /// The pen as an audience value (`pen:<x>`), when there is one.
    pub fn as_audience(&self) -> Option<&str> {
        self.pen.as_deref()
    }

    /// Of these keys, which are private to a pen BELOW this reader? One query, because a search
    /// returns a page and the audit wants a count rather than a round trip per hit.
    pub fn crossed_keys(
        conn: &rusqlite::Connection,
        reader: &Reader,
        keys: &[&str],
    ) -> crate::error::Result<Vec<String>> {
        if keys.is_empty() || reader.all || reader.down.is_empty() {
            return Ok(Vec::new());
        }
        let mut out = Vec::new();
        let mut stmt = conn.prepare("SELECT audience FROM memory WHERE key = ?1")?;
        for key in keys {
            let audience: Option<String> = stmt
                .query_row(rusqlite::params![key], |r| r.get(0))
                .optional()?;
            if audience.is_some_and(|a| reader.crosses(&a)) {
                out.push((*key).to_string());
            }
        }
        Ok(out)
    }

    /// Is this row PRIVATE TO SOMEBODY ELSE — i.e. did showing it mean crossing a boundary?
    ///
    /// True only for a row that is not public and not addressed to this reader, which is exactly the
    /// case an audit needs: an ancestor reading down its subtree (#120), or an exemption (#129). A
    /// pen reading its own rows crossed nothing, and the process holding the file (`all`) is the
    /// honest limit of the tier rather than an exemption within it.
    pub fn crosses(&self, audience: &str) -> bool {
        !self.all && audience != PUBLIC && self.sees(audience) && !self.exact(audience)
    }

    /// Is this row addressed to ME by name — the one route that is not a boundary crossing?
    pub fn exact(&self, audience: &str) -> bool {
        match &self.pen {
            Some(p) => parse(audience).is_some_and(|t| t.iter().any(|x| x.token() == *p)),
            None => false,
        }
    }

    /// May this reader be shown a row with this audience?
    ///
    /// Written as an allow-list, not a deny-list: an audience value nobody understands is visible to
    /// nobody. The opposite choice would make a typo in a writer a silent publication.
    ///
    /// A row's audience is a list of ADDRESSES (`pen:<name>` exact, `subtree:<name>` inclusive). This
    /// reader may see it if it matches any address it holds: its own name exactly, a `subtree:` token
    /// from its ancestors, or one of a borrowed pen's. A ROOT reader sees everything — the break-glass.
    pub fn sees(&self, audience: &str) -> bool {
        if self.all || self.root || audience == PUBLIC {
            return true;
        }
        let Some(targets) = parse(audience) else {
            return false; // unreadable audience: nobody, never everybody
        };
        self.permits(&targets)
    }

    /// Does any of `targets` match a token this reader holds?
    fn permits(&self, targets: &[Target]) -> bool {
        let Some(me) = &self.pen else {
            return false;
        };
        targets.iter().any(|t| {
            let token = t.token();
            token == *me || self.extra.contains(&token)
        })
    }

    /// Does any of `tokens` appear in `audience`?
    fn matches(audience: &str, tokens: &[String]) -> bool {
        parse(audience).is_some_and(|targets| {
            targets
                .iter()
                .any(|t| tokens.iter().any(|x| *x == t.token()))
        })
    }

    /// The SQL predicate over `memory.audience`, and the parameters it binds.
    ///
    /// The shape is the same question as [`Reader::sees`], so the two cannot drift. An audience is a
    /// comma-separated LIST, so a membership test is a LIKE over the padded string with the address
    /// as a whole token — `audience IN (…)` would miss `pen:a,pen:b`, and a substring match without
    /// the commas would let `pen:anne` answer for `pen:ann`.
    pub fn sql(&self) -> (String, Vec<String>) {
        if self.all || self.root {
            return ("1 = 1".to_string(), Vec::new());
        }
        let Some(me) = &self.pen else {
            return ("audience = 'public'".to_string(), Vec::new());
        };
        let mut tokens = vec![me.clone()];
        tokens.extend(self.extra.iter().cloned());
        let mut binds = Vec::new();
        let mut clauses = Vec::new();
        for t in tokens {
            // ★ `instr`, NOT `LIKE` (adversarial review, 2026-10-24): `_` and `%` are LIKE wildcards,
            // and a pen name may legitimately contain them — `pen:under_score` would have matched
            // `pen:underXscore` and leaked a row. A substring position has no metacharacters.
            binds.push(format!(",{t},"));
            clauses.push("instr(',' || lower(audience) || ',', ?) > 0".to_string());
        }
        (
            format!("(audience = 'public' OR {})", clauses.join(" OR ")),
            binds,
        )
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
        assert!(sql.contains("instr"), "{sql}");
        assert_eq!(binds, vec![",pen:stet,".to_string()]);
        assert_eq!(Reader::pen("stet").down(), 0);

        // One bind per address: the predicate's holes and its binds are one list, built together, so
        // they cannot drift into a `SQLITE_RANGE` at runtime.
        assert_eq!(sql.matches('?').count(), binds.len());
    }

    #[test]
    fn an_audience_is_a_list_of_exact_or_subtree_addresses() {
        assert!(is_valid("public"));
        assert!(is_valid("pen:stet"));
        assert!(is_valid("pen:james,pen:cline"));
        assert!(is_valid("subtree:james"));
        assert!(is_valid("pen:james,subtree:stet"));
        // Refusals
        assert!(
            !is_valid("private"),
            "a word we do not know is not an audience"
        );
        assert!(!is_valid("pen:"), "an empty name names nobody");
        assert!(
            !is_valid("pen:a,pen:a"),
            "a duplicate is a typo, not a second grant"
        );
        assert!(
            is_valid("pen:james , pen:cline"),
            "whitespace AROUND a token is tolerated"
        );
        assert!(!is_valid("pen:ja mes"), "whitespace INSIDE a name is not");
        assert!(
            !is_valid("pen:anonymous"),
            "anonymous is a truth, not a pen"
        );
        let too_many: String = (0..9)
            .map(|i| format!("pen:p{i}"))
            .collect::<Vec<_>>()
            .join(",");
        assert!(
            !is_valid(&too_many),
            "more than MAX_TARGETS is a mailing list"
        );
        // Normalisation
        assert_eq!(normalise("PEN:Stet").unwrap(), "pen:stet");
        assert_eq!(
            normalise("pen:stet,subtree:james").unwrap(),
            "pen:stet,subtree:james"
        );
        assert_eq!(normalise("public").unwrap(), "public");
    }

    #[test]
    fn a_named_pen_is_exact_and_subtree_reaches_down() {
        // A reader with no tree: exactly itself.
        let stet = Reader::pen("stet");
        assert!(stet.sees("pen:stet"));
        assert!(
            stet.sees("pen:james,pen:stet"),
            "a list matches if any address is mine"
        );
        assert!(!stet.sees("pen:james"));
        assert!(
            !stet.sees("subtree:james"),
            "a pen with no tree reaches nothing above it"
        );

        // A reader with `subtree:` tokens from its ancestors (what `in_tree` resolves).
        let mut stet = Reader::pen("stet");
        stet.extra.push("subtree:estate".to_string());
        stet.extra.push("subtree:james".to_string());
        assert!(stet.sees("subtree:james"), "a share aimed at my branch");
        assert!(
            stet.sees("pen:stet,subtree:estate"),
            "any address on the row is enough"
        );
        assert!(
            !stet.sees("subtree:someoneelse"),
            "and only the tokens are real"
        );

        // A ROOT reader sees everything — the estate's break-glass, explicit.
        let mut root = Reader::pen("estate");
        root.root = true;
        assert!(root.sees("pen:stet"));
        assert!(root.sees("subtree:james,pen:hands"));

        // Anything unparseable is visible to NOBODY (the allow-list, not the deny-list).
        assert!(!stet.sees("everyone-i-like"));
        assert!(!Reader::anonymous().sees("pen:stet"));
    }
}
