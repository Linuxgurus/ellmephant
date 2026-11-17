//! WHO is speaking: `X-Stet-From` names a self, `X-Stet-Auth` proves it.
//!
//! Provenance is the whole point: a claim needs an author, and an author needs proof. The store
//! stamps only what a connection **proved**, never what it asserted, so a forged name lands as the
//! claim it is rather than as fact — and `anonymous` is the truth rather than an insult.
//!
//! ★ THE PROOF IS A ROW (2026-10-30). It used to be a process variable (`ELLMEPHANT_IDENTITIES`,
//! `name:secret`, re-read every call), which meant a retired pen could still authenticate and a
//! rotation needed a deploy. The secret now lives in `pen_credential`, owned by a pen, so
//! retirement revokes it and rotation takes effect on the next call. Nothing is cached here.
//!
//! ★ THE SECRET IS NEVER LOGGED, and the comparison is constant-time. Length still leaks — an
//! attacker who can measure it learns how long the secret is, and if that is fatal the secret was
//! too short.

use axum::http::HeaderMap;

use crate::config::Paths;

/// The claimed speaker.
pub const FROM: &str = "x-stet-from";
/// The proof that the claim is true.
pub const AUTH: &str = "x-stet-auth";
/// What a write is attributed to when nothing was proved.
pub const ANONYMOUS: &str = "anonymous";

/// Why a claim did not become an identity.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Denied {
    /// A name was claimed and its proof is missing or wrong. Carries the claim so the refusal can
    /// name it.
    Forged(String),
    /// The store could not be read. Distinct from [`Denied::Forged`] on purpose: a database that is
    /// unavailable is not a caller who lied, and saying so is the difference between a retry and a
    /// debugging session.
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

/// Resolve the caller's identity from the header pair, against the store's credential rows.
///
/// * no `X-Stet-From` → `Ok(None)`: nothing was claimed, so nothing is attributed.
/// * a claim that does not verify (missing proof, wrong proof, retired pen, no credential) → `Err`.
/// * the store cannot be read → `Err(Denied::Unavailable)`.
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
    // A short-lived read-only connection: authentication must not queue behind a long write, and it
    // must see a rotation on the very next call. No cache, exactly like sudo.
    let conn = crate::db::open_read_only(paths).map_err(|e| Denied::Unavailable(e.to_string()))?;
    match crate::store::credential::verify(&conn, claimed, proof) {
        // The STORED spelling wins, so `james` and `James` are one identity (migration 0027).
        Ok(Some(name)) => Ok(Some(name)),
        Ok(None) => Err(Denied::Forged(claimed.to_string())),
        Err(e) => Err(Denied::Unavailable(e.to_string())),
    }
}

/// Why a write was refused an identity.
///
/// The two cases are deliberately one refusal at the call site: `Caller::pen()` fails on BOTH, so a
/// door cannot accidentally treat "forged" and "absent" differently. They stay distinguishable in
/// the message, because a caller that sent a bad proof and one that sent none need different fixes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Refusal {
    /// A name was claimed and could not be proved.
    Denied(Denied),
    /// Nothing was claimed. Carries the sentence the route wants: `mint` and `lend` name the verb.
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

/// ★ ONE CALLER, BUILT ONCE (soma/ellmephant#232). Every door used to carry its own ladder —
/// `identity_of`, `reader_of`, `pen_name`, `proven_pen`, `reader_from`, `proven` — and the copies
/// disagreed: on HTTP a write could proceed with no pen (`.ok().flatten()` collapsed *forged* and
/// *missing* into `None`), while MCP refused both. The divergence was not in the store; it was in
/// having six answers to one question.
///
/// So the question is asked once, here, and the answer is a value the doors pass around.
#[derive(Debug, Clone, Default)]
pub struct Caller {
    /// The proven pen. `Some` is the only way to write.
    proven: Option<crate::names::Pen>,
    /// Why a claim failed, if one was made and refused.
    denied: Option<Denied>,
    /// A pen DECLARED by a transport with no request (stdio, in-process): as trusted as the process
    /// that started it. Ignored on HTTP, where the headers decide.
    declared: Option<crate::names::Pen>,
    /// The BORROWED authority — `sudo` and `pen_lend` let a caller act as another pen, and the store
    /// already keeps that beside the assertion as `acted_by`. Carried here now, so a Command credits
    /// through one value instead of two positional `Option<&str>` (see #239).
    via: Option<crate::names::Pen>,
}

impl Caller {
    /// The one HTTP constructor: read the pair from the request headers.
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

    /// A transport with no request at all (stdio, in-process): it may declare its pen once.
    pub fn transport(declared: Option<&str>) -> Self {
        Caller {
            declared: declared.and_then(|p| crate::names::Pen::new(p).ok()),
            ..Caller::default()
        }
    }

    /// Let a declaring transport name itself. HTTP transports never call this: their headers decide.
    pub fn declare(mut self, declared: Option<&str>) -> Self {
        self.declared = declared.and_then(|p| crate::names::Pen::new(p).ok());
        self
    }

    /// The proven pen, or the declared one for a trusted transport — never a forged claim.
    pub fn proven(&self) -> Option<&crate::names::Pen> {
        self.proven.as_ref().or_else(|| {
            if self.denied.is_none() {
                self.declared.as_ref()
            } else {
                None
            }
        })
    }

    /// Why a claim was refused, if one was. Reads use it only to stay narrow.
    pub fn denied(&self) -> Option<&Denied> {
        self.denied.as_ref()
    }

    /// ★ THE WRITE GATE: a pen, or a refusal. Fails on BOTH *forged* and *missing* — the whole point
    /// of the type, and the difference between this and `.ok().flatten()`.
    pub fn pen(&self) -> Result<&crate::names::Pen, Refusal> {
        self.pen_or(
            "refused: a write requires a proven identity — send `X-Stet-From` and `X-Stet-Auth` \
             (reads need neither). `whoami` reports what this connection is.",
        )
    }

    /// [`Caller::pen`] with the route's own sentence for the missing case, so `mint` can say
    /// "minting needs a proven pen" without a second ladder.
    pub fn pen_or(&self, missing: &str) -> Result<&crate::names::Pen, Refusal> {
        if let Some(denied) = &self.denied {
            return Err(Refusal::Denied(denied.clone()));
        }
        self.proven()
            .ok_or_else(|| Refusal::Missing(missing.to_string()))
    }

    /// Who this request reads as. Never an error: a read is open, it is just narrow.
    pub fn reader(&self) -> crate::store::audience::Reader {
        match self.proven() {
            Some(name) => crate::store::audience::Reader::pen(name.as_str()),
            None => crate::store::audience::Reader::anonymous(),
        }
    }

    /// The pen this caller acts AS when one is lent or taken by `sudo`.
    pub fn via(&self) -> Option<&crate::names::Pen> {
        self.via.as_ref()
    }

    /// Record borrowed authority — the store stamps it as `acted_by`.
    pub fn acting_as(mut self, pen: Option<&str>) -> Self {
        self.via = pen.and_then(|p| crate::names::Pen::new(p).ok());
        self
    }
}

/// The identities allowed to take a LEASE, from `ELLMEPHANT_LEASERS` (comma-separated names).
///
/// ★ UNSET MEANS OPEN. A lease is how the hands do ordinary work, so refusing every lease the
/// moment the code lands would stop the mail hand dead. Configure the list and it becomes a
/// control; leave it empty and it stays exactly as it was.
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

/// Whether this identity may take a lease at all.
///
/// `PROTECTED` is only a control if the people it protects against cannot take the lease out from
/// under it — otherwise an injected model simply claims the key, writes, and releases, and the audit
/// trail reads like a legitimate edit.
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

    /// A store with one pen (`James`) and one credential. The file is the fixture; no env.
    fn store() -> (tempfile::TempDir, Paths, String) {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths::from_root(dir.path());
        let conn = crate::db::open(&paths).unwrap();
        // ★ James hangs from the root, not at it (2026-10-30): a root cannot hold a credential, so a
        // fixture that makes its pen a root is testing a state the code now refuses.
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
        // An empty name is not a claim, so it is not a refusal either.
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
        // A claim with no proof is FORGED, not anonymous: silence is honest, a name is not.
        assert_eq!(
            resolve(&paths, &headers(Some("James"), None)),
            Err(Denied::Forged("James".into()))
        );
        // …and the right proof still works after the refusals.
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

    /// ★ THE WHOLE POINT OF THE TYPE (soma/ellmephant#232): `pen()` fails on BOTH *missing* and
    /// *forged*. The door that used `.ok().flatten()` treated them identically by mistake — it
    /// turned both into `None` and then wrote.
    #[test]
    fn pen_refuses_both_a_missing_and_a_forged_claim() {
        let (_dir, paths, secret) = store();

        let absent = Caller::from_headers(&paths, &headers(None, None));
        assert_eq!(absent.proven(), None);
        assert!(matches!(absent.pen(), Err(Refusal::Missing(_))));
        // A read by the same caller is still legal; it is just narrow.
        assert_eq!(absent.reader().pen_name(), None);

        let forged = Caller::from_headers(&paths, &headers(Some("James"), Some("wrong")));
        assert!(matches!(
            forged.pen(),
            Err(Refusal::Denied(Denied::Forged(_)))
        ));
        assert_eq!(forged.reader().pen_name(), None);

        let proved = Caller::from_headers(&paths, &headers(Some("James"), Some(&secret)));
        assert_eq!(proved.pen().unwrap().as_str(), "James");
        // ★ A DECLARATION MAY STAND IN ONLY WHEN NOTHING WAS CLAIMED — never over a refusal. That
        // is the difference between a trusted transport and a forged header.
        assert!(forged.declare(Some("James")).pen().is_err());
        assert_eq!(
            absent.declare(Some("James")).pen().unwrap().as_str(),
            "James"
        );
    }
}
