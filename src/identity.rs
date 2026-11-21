use axum::http::HeaderMap;

use crate::config::Paths;

pub const FROM: &str = "x-stet-from";
pub const AUTH: &str = "x-stet-auth";
pub const ANONYMOUS: &str = "anonymous";

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Denied {
    Forged(String),
    Unavailable(String),
}

impl std::fmt::Display for Denied {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Denied::Forged(claimed) => write!(
                f,
                "`{FROM}: {claimed}` was refused: the `{AUTH}` proof is missing or wrong. A write is \
                 stamped with the connection's proven identity, so an unproven name would be a lie \
                 with a signature. Send the pair, or omit `{FROM}` and write as `{ANONYMOUS}`."
            ),
            Denied::Unavailable(why) => {
                write!(f, "authentication is unavailable: {why}")
            }
        }
    }
}

impl std::error::Error for Denied {}

pub fn resolve(paths: &Paths, headers: &HeaderMap) -> Result<Option<String>, Denied> {
    let Some(raw) = headers.get(FROM) else {
        return Ok(None);
    };
    let claimed = raw.to_str().map(str::trim).unwrap_or_default();
    if claimed.is_empty() {
        return Ok(None);
    }
    let proof = headers
        .get(AUTH)
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default();
    let conn = crate::db::open_read_only(paths).map_err(|e| Denied::Unavailable(e.to_string()))?;
    match crate::store::credential::verify(&conn, claimed, proof) {
        Ok(Some(name)) => Ok(Some(name)),
        Ok(None) => Err(Denied::Forged(claimed.to_string())),
        Err(e) => Err(Denied::Unavailable(e.to_string())),
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Refusal {
    Denied(Denied),
    Missing(String),
}

impl std::fmt::Display for Refusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Refusal::Denied(d) => d.fmt(f),
            Refusal::Missing(why) => write!(f, "{why}"),
        }
    }
}

impl std::error::Error for Refusal {}

#[derive(Debug, Clone, Default)]
pub struct Caller {
    proven: Option<crate::names::Pen>,
    denied: Option<Denied>,
    declared: Option<crate::names::Pen>,
    via: Option<crate::names::Pen>,
}

impl Caller {
    pub fn from_headers(paths: &Paths, headers: &HeaderMap) -> Self {
        match resolve(paths, headers) {
            Ok(proven) => Caller {
                proven: proven
                    .as_deref()
                    .and_then(|p| crate::names::Pen::new(p).ok()),
                ..Caller::default()
            },
            Err(denied) => Caller {
                denied: Some(denied),
                ..Caller::default()
            },
        }
    }

    pub fn transport(declared: Option<&str>) -> Self {
        Caller {
            declared: declared.and_then(|p| crate::names::Pen::new(p).ok()),
            ..Caller::default()
        }
    }

    pub fn declare(mut self, declared: Option<&str>) -> Self {
        self.declared = declared.and_then(|p| crate::names::Pen::new(p).ok());
        self
    }

    pub fn proven(&self) -> Option<&crate::names::Pen> {
        self.proven.as_ref().or_else(|| {
            if self.denied.is_none() {
                self.declared.as_ref()
            } else {
                None
            }
        })
    }

    pub fn denied(&self) -> Option<&Denied> {
        self.denied.as_ref()
    }

    pub fn pen(&self) -> Result<&crate::names::Pen, Refusal> {
        self.pen_or(
            "refused: a write requires a proven identity — send `X-Stet-From` and `X-Stet-Auth` \
             (reads need neither). `whoami` reports what this connection is.",
        )
    }

    pub fn pen_or(&self, missing: &str) -> Result<&crate::names::Pen, Refusal> {
        if let Some(denied) = &self.denied {
            return Err(Refusal::Denied(denied.clone()));
        }
        self.proven()
            .ok_or_else(|| Refusal::Missing(missing.to_string()))
    }

    pub fn reader(&self) -> crate::store::audience::Reader {
        match self.proven() {
            Some(name) => crate::store::audience::Reader::pen(name.as_str()),
            None => crate::store::audience::Reader::anonymous(),
        }
    }

    pub fn via(&self) -> Option<&crate::names::Pen> {
        self.via.as_ref()
    }

    pub fn acting_as(mut self, pen: Option<&str>) -> Self {
        self.via = pen.and_then(|p| crate::names::Pen::new(p).ok());
        self
    }
}

fn leasers() -> Vec<String> {
    let Ok(raw) = std::env::var("ELLMEPHANT_LEASERS") else {
        return Vec::new();
    };
    raw.split(',')
        .map(str::trim)
        .filter(|n| !n.is_empty())
        .map(str::to_string)
        .collect()
}

pub fn may_lease(name: Option<&str>) -> Result<(), String> {
    let leasers = leasers();
    if leasers.is_empty() {
        return Ok(());
    }
    match name {
        Some(n) if leasers.iter().any(|l| l.eq_ignore_ascii_case(n)) => Ok(()),
        Some(n) => Err(format!(
            "`{n}` may not take a lease: `ELLMEPHANT_LEASERS` does not name it."
        )),
        None => Err(
            "a lease needs a PROVEN identity: send `X-Stet-From` and `X-Stet-Auth` together."
                .to_string(),
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::{HeaderValue, header::HeaderName};

    fn headers(from: Option<&str>, auth: Option<&str>) -> HeaderMap {
        let mut h = HeaderMap::new();
        if let Some(v) = from {
            h.insert(
                HeaderName::from_static(FROM),
                HeaderValue::from_str(v).unwrap(),
            );
        }
        if let Some(v) = auth {
            h.insert(
                HeaderName::from_static(AUTH),
                HeaderValue::from_str(v).unwrap(),
            );
        }
        h
    }

    fn store() -> (tempfile::TempDir, Paths, String) {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths::from_root(dir.path());
        let conn = crate::db::open(&paths).unwrap();
        conn.execute(
            "INSERT INTO pen (name, kind, parent_id)
             VALUES ('James', 'self', (SELECT id FROM pen WHERE name = 'estate'))",
            [],
        )
        .unwrap();
        let secret = crate::store::credential::issue(&conn, "James", None).unwrap();
        (dir, paths, secret)
    }

    #[test]
    fn no_claim_attributes_nothing() {
        let (_dir, paths, _secret) = store();
        assert_eq!(resolve(&paths, &headers(None, None)), Ok(None));
        assert_eq!(resolve(&paths, &headers(Some("  "), None)), Ok(None));
    }

    #[test]
    fn a_proved_claim_returns_the_stored_spelling() {
        let (_dir, paths, secret) = store();
        assert_eq!(
            resolve(&paths, &headers(Some("james"), Some(&secret))),
            Ok(Some("James".to_string()))
        );
    }

    #[test]
    fn a_forged_claim_is_refused_and_names_itself() {
        let (_dir, paths, secret) = store();
        assert_eq!(
            resolve(&paths, &headers(Some("James"), Some("hunter2"))),
            Err(Denied::Forged("James".into()))
        );
        assert_eq!(
            resolve(&paths, &headers(Some("James"), None)),
            Err(Denied::Forged("James".into()))
        );
        assert_eq!(
            resolve(&paths, &headers(Some("James"), Some(&secret))),
            Ok(Some("James".to_string()))
        );
    }

    #[test]
    fn a_retired_pen_stops_proving_its_old_secret() {
        let (_dir, paths, secret) = store();
        {
            let conn = crate::db::open(&paths).unwrap();
            conn.execute(
                "UPDATE pen SET ceased_at = datetime('now') WHERE name = 'James'",
                [],
            )
            .unwrap();
        }
        assert_eq!(
            resolve(&paths, &headers(Some("James"), Some(&secret))),
            Err(Denied::Forged("James".into()))
        );
    }

    #[test]
    fn pen_refuses_both_a_missing_and_a_forged_claim() {
        let (_dir, paths, secret) = store();

        let absent = Caller::from_headers(&paths, &headers(None, None));
        assert_eq!(absent.proven(), None);
        assert!(matches!(absent.pen(), Err(Refusal::Missing(_))));
        assert_eq!(absent.reader().pen_name(), None);

        let forged = Caller::from_headers(&paths, &headers(Some("James"), Some("wrong")));
        assert!(matches!(
            forged.pen(),
            Err(Refusal::Denied(Denied::Forged(_)))
        ));
        assert_eq!(forged.reader().pen_name(), None);

        let proved = Caller::from_headers(&paths, &headers(Some("James"), Some(&secret)));
        assert_eq!(proved.pen().unwrap().as_str(), "James");
        assert!(forged.declare(Some("James")).pen().is_err());
        assert_eq!(
            absent.declare(Some("James")).pen().unwrap().as_str(),
            "James"
        );
    }
}
