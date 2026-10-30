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
        conn.execute("INSERT INTO pen (name, kind) VALUES ('James', 'self')", [])
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
}
