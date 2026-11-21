use rusqlite::OptionalExtension as _;

pub const PUBLIC: &str = "public";

pub const PEN: &str = "pen:";

pub const SUBTREE: &str = "subtree:";

pub const MAX_TARGETS: usize = 8;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Kind {
    Pen,
    Subtree,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Target {
    pub kind: Kind,
    pub name: String,
}

impl Target {
    pub fn token(&self) -> String {
        match self.kind {
            Kind::Pen => format!("{PEN}{}", self.name),
            Kind::Subtree => format!("{SUBTREE}{}", self.name),
        }
    }
}

pub fn parse(value: &str) -> Option<Vec<Target>> {
    let value = value.trim();
    if value == PUBLIC {
        return Some(Vec::new());
    }
    let mut out: Vec<Target> = Vec::new();
    for raw in value.split(',') {
        let token = raw.trim().to_ascii_lowercase();
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
            return None;
        }
        out.push(t);
    }
    if out.is_empty() || out.len() > MAX_TARGETS {
        return None;
    }
    Some(out)
}

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

pub fn pen(name: &str) -> String {
    format!("{PEN}{}", name.trim().to_ascii_lowercase())
}

pub fn effective(value: &str) -> &str {
    if value.trim().is_empty() {
        PUBLIC
    } else {
        value
    }
}

pub fn default_audience() -> String {
    PUBLIC.to_string()
}

pub fn is_valid(value: &str) -> bool {
    parse(value).is_some()
}

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

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Reader {
    pen: Option<String>,
    extra: Vec<String>,
    own_subtree: Vec<String>,
    borrowed_pens: Vec<String>,
    root: bool,
    down: Vec<String>,
    borrowed: Vec<String>,
    all: bool,
}

impl Reader {
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

    pub fn in_tree(mut self, conn: &rusqlite::Connection) -> Self {
        let Some(p) = self.pen.clone() else {
            return self;
        };
        let name = p.strip_prefix(PEN).unwrap_or(&p).to_string();
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

    pub fn with_borrowed(mut self, conn: &rusqlite::Connection) -> Self {
        let Some(p) = self.pen.clone() else {
            return self;
        };
        let me = p.strip_prefix(PEN).unwrap_or(&p).to_string();
        let Ok(held) = crate::store::sudo::reachable(conn, &me) else {
            return self;
        };
        for (pid, name, _source) in held {
            let bare = name.trim().to_ascii_lowercase();
            let token = format!("{PEN}{bare}");
            if !self.extra.contains(&token) {
                self.extra.push(token.clone());
            }
            if !self.borrowed_pens.contains(&bare) {
                self.borrowed_pens.push(bare.clone());
                self.borrowed.push(token);
            }
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

    pub fn is_borrowed(&self, audience: &str) -> bool {
        if self.exact(audience) {
            return false;
        }
        if Self::matches(audience, &self.own_subtree) {
            return false;
        }
        self.root || Self::matches(audience, &self.borrowed)
    }

    pub fn as_root(mut self) -> Self {
        self.root = true;
        self
    }

    pub fn down(&self) -> usize {
        self.down.len()
    }

    pub fn pen_name(&self) -> Option<&str> {
        self.pen.as_deref().and_then(|p| p.strip_prefix(PEN))
    }

    pub fn is_root(&self) -> bool {
        self.root
    }

    pub fn anonymous() -> Self {
        Reader::default()
    }

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

    pub fn as_audience(&self) -> Option<&str> {
        self.pen.as_deref()
    }

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

    pub fn crosses(&self, audience: &str) -> bool {
        !self.all && audience != PUBLIC && self.sees(audience) && !self.exact(audience)
    }

    pub fn exact(&self, audience: &str) -> bool {
        match &self.pen {
            Some(p) => parse(audience).is_some_and(|t| t.iter().any(|x| x.token() == *p)),
            None => false,
        }
    }

    pub fn sees(&self, audience: &str) -> bool {
        if self.all || self.root || audience == PUBLIC {
            return true;
        }
        let Some(targets) = parse(audience) else {
            return false;
        };
        self.permits(&targets)
    }

    fn permits(&self, targets: &[Target]) -> bool {
        let Some(me) = &self.pen else {
            return false;
        };
        targets.iter().any(|t| {
            let token = t.token();
            token == *me || self.extra.contains(&token)
        })
    }

    fn matches(audience: &str, tokens: &[String]) -> bool {
        parse(audience).is_some_and(|targets| {
            targets
                .iter()
                .any(|t| tokens.iter().any(|x| *x == t.token()))
        })
    }

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
            binds.push(format!(",{t},"));
            clauses.push("instr(',' || lower(audience) || ',', ?) > 0".to_string());
        }
        (
            format!("(audience = 'public' OR {})", clauses.join(" OR ")),
            binds,
        )
    }
}

#[derive(Debug, Clone)]
pub struct Forensic<'a> {
    pub pen: &'a str,
    pub reason: &'a str,
}

impl Reader {
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
        let (sql, binds) = Reader::pen("stet").sql();
        assert!(sql.contains("instr"), "{sql}");
        assert_eq!(binds, vec![",pen:stet,".to_string()]);
        assert_eq!(Reader::pen("stet").down(), 0);

        assert_eq!(sql.matches('?').count(), binds.len());
    }

    #[test]
    fn an_audience_is_a_list_of_exact_or_subtree_addresses() {
        assert!(is_valid("public"));
        assert!(is_valid("pen:stet"));
        assert!(is_valid("pen:james,pen:cline"));
        assert!(is_valid("subtree:james"));
        assert!(is_valid("pen:james,subtree:stet"));
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
        assert_eq!(normalise("PEN:Stet").unwrap(), "pen:stet");
        assert_eq!(
            normalise("pen:stet,subtree:james").unwrap(),
            "pen:stet,subtree:james"
        );
        assert_eq!(normalise("public").unwrap(), "public");
    }

    #[test]
    fn a_named_pen_is_exact_and_subtree_reaches_down() {
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

        let mut root = Reader::pen("estate");
        root.root = true;
        assert!(root.sees("pen:stet"));
        assert!(root.sees("subtree:james,pen:hands"));

        assert!(!stet.sees("everyone-i-like"));
        assert!(!Reader::anonymous().sees("pen:stet"));
    }
}
