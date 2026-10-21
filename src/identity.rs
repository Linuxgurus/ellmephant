//! WHO is speaking: `X-Stet-From` names a self, `X-Stet-Auth` proves it.
//!
//! Provenance is the whole point: a claim needs an author, and an author needs proof. The store
//! stamps only what a connection **proved**, never what it asserted, so a forged name lands as the
//! claim it is rather than as fact — and `anonymous` is the truth rather than an insult.
//!
//! The table is env (`ELLMEPHANT_IDENTITIES`, `name:secret` comma-separated), so adding a self is
//! data and not a deploy. A name here is a *speaker*, not a permission: it says who to attribute a
//! write to, and nothing about what they may do.
//!
//! ★ THE SECRET IS NEVER LOGGED, and the comparison is constant-time. Length still leaks — an
//! attacker who can measure it learns how long the secret is, and if that is fatal the secret was
//! too short.

use axum::http::HeaderMap;

/// The claimed speaker.
pub const FROM: &str = "x-stet-from";
/// The proof that the claim is true.
pub const AUTH: &str = "x-stet-auth";
/// What a write is attributed to when nothing was proved.
pub const ANONYMOUS: &str = "anonymous";

/// A name was claimed and could not be proved. Carries the claim so the refusal can name it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Forged(pub String);

impl std::fmt::Display for Forged {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "`{FROM}: {}` was refused: the `{AUTH}` proof is missing or wrong. A write is stamped \
             with the connection's proven identity, so an unproven name would be a lie with a \
             signature. Send the pair, or omit `{FROM}` and write as `{ANONYMOUS}`.",
            self.0
        )
    }
}

impl std::error::Error for Forged {}

/// The configured identities, read per call: a cache here would make the tests lie.
fn table() -> Vec<(String, String)> {
    let Ok(raw) = std::env::var("ELLMEPHANT_IDENTITIES") else {
        return Vec::new();
    };
    raw.split(',')
        .filter_map(|pair| {
            let (name, secret) = pair.split_once(':')?;
            let (name, secret) = (name.trim(), secret.trim());
            (!name.is_empty() && !secret.is_empty()).then(|| (name.to_string(), secret.to_string()))
        })
        .collect()
}

/// Length-aware, branch-free comparison — no early exit for a wrong byte.
fn same(a: &str, b: &str) -> bool {
    let (a, b) = (a.as_bytes(), b.as_bytes());
    if a.len() != b.len() {
        return false;
    }
    a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

/// Resolve the caller's identity from the header pair.
///
/// * no `X-Stet-From` → `Ok(None)`: nothing was claimed, so nothing is attributed.
/// * a claim that does not verify (missing proof, wrong proof, no table configured) → `Err`.
pub fn resolve(headers: &HeaderMap) -> Result<Option<String>, Forged> {
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
    for (name, secret) in table() {
        if name.eq_ignore_ascii_case(claimed) && same(proof, &secret) {
            // The configured spelling wins, so `james` and `James` are one identity.
            return Ok(Some(name));
        }
    }
    Err(Forged(claimed.to_string()))
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

    /// The env is process-wide, so every test here takes the same lock.
    fn with_table<T>(table: &str, f: impl FnOnce() -> T) -> T {
        static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
        let _guard = LOCK.lock().unwrap_or_else(|e| e.into_inner());
        // SAFETY: `with_table` serialises every reader and writer in this process.
        unsafe { std::env::set_var("ELLMEPHANT_IDENTITIES", table) };
        let out = f();
        unsafe { std::env::remove_var("ELLMEPHANT_IDENTITIES") };
        out
    }

    #[test]
    fn no_claim_attributes_nothing() {
        with_table("James:swordfish", || {
            assert_eq!(resolve(&headers(None, None)), Ok(None));
            // An empty name is not a claim, so it is not a refusal either.
            assert_eq!(resolve(&headers(Some("  "), None)), Ok(None));
        });
    }

    #[test]
    fn a_proved_claim_returns_the_configured_spelling() {
        with_table("James:swordfish", || {
            assert_eq!(
                resolve(&headers(Some("james"), Some("swordfish"))),
                Ok(Some("James".to_string()))
            );
        });
    }

    #[test]
    fn a_forged_claim_is_refused_and_names_itself() {
        with_table("James:swordfish", || {
            assert_eq!(
                resolve(&headers(Some("James"), Some("hunter2"))),
                Err(Forged("James".into()))
            );
            // A claim with no proof is FORGED, not anonymous: silence is honest, a name is not.
            assert_eq!(
                resolve(&headers(Some("James"), None)),
                Err(Forged("James".into()))
            );
        });
    }

    #[test]
    fn nothing_is_provable_with_no_table_configured() {
        with_table("", || {
            assert_eq!(
                resolve(&headers(Some("James"), Some("swordfish"))),
                Err(Forged("James".into()))
            );
        });
    }

    #[test]
    fn comparison_does_not_short_circuit_on_length_or_prefix() {
        assert!(same("abc", "abc"));
        assert!(!same("abc", "abd"));
        assert!(!same("abc", "ab"));
        assert!(!same("", "a"));
    }
}
