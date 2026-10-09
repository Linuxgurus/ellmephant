//! The only network code in the binary (SPEC §6.2).
//!
//! [`Fetcher`] is a trait so the whole cache — resolve, download, verify — is testable
//! without a socket. Production uses [`UreqFetcher`] (ureq + rustls).

use std::io::Write;

use crate::error::{Error, Result};

/// Default Hub host; `HF_ENDPOINT` overrides it (mirrors, air-gapped proxies).
pub const DEFAULT_ENDPOINT: &str = "https://huggingface.co";

/// URL of one file at a revision: `<endpoint>/<repo>/resolve/<rev>/<file>`.
pub fn file_url(endpoint: &str, repo: &str, rev: &str, file: &str) -> String {
    format!(
        "{}/{}/resolve/{}/{}",
        endpoint.trim_end_matches('/'),
        repo,
        rev,
        file
    )
}

/// URL of the model-info API used to resolve a ref to a commit sha.
pub fn revision_api_url(endpoint: &str, repo: &str, reference: &str) -> String {
    format!(
        "{}/api/models/{}/revision/{}",
        endpoint.trim_end_matches('/'),
        repo,
        reference
    )
}

/// A source of bytes and revisions. Implemented by [`UreqFetcher`] and, in tests, by a
/// in-memory fake.
pub trait Fetcher {
    /// Stream `url`'s body into `sink`; returns the number of bytes written.
    fn download(&self, url: &str, sink: &mut dyn Write) -> Result<u64>;
    /// GET `url` and read its JSON `sha` field (a ref resolved to a commit).
    fn resolve_ref(&self, url: &str) -> Result<String>;
}

/// Blocking HTTP fetcher. Reads `HF_ENDPOINT`/`HF_TOKEN`-style config at construction.
pub struct UreqFetcher {
    token: Option<String>,
    agent: ureq::Agent,
}

impl UreqFetcher {
    /// Build from `HF_TOKEN` / `HUGGING_FACE_HUB_TOKEN` (the repo is public, so optional).
    pub fn from_env() -> Self {
        let token = std::env::var("HF_TOKEN")
            .ok()
            .or_else(|| std::env::var("HUGGING_FACE_HUB_TOKEN").ok())
            .filter(|t| !t.is_empty());
        Self {
            token,
            agent: ureq::Agent::new_with_defaults(),
        }
    }

    /// Build with an explicit token (`None` for anonymous).
    pub fn with_token(token: Option<String>) -> Self {
        Self {
            token,
            agent: ureq::Agent::new_with_defaults(),
        }
    }

    fn get(&self, url: &str) -> Result<ureq::http::Response<ureq::Body>> {
        let mut request = self.agent.get(url);
        if let Some(token) = &self.token {
            request = request.header("Authorization", &format!("Bearer {token}"));
        }
        request.call().map_err(|e| Error::Http(e.to_string()))
    }
}

impl Fetcher for UreqFetcher {
    fn download(&self, url: &str, sink: &mut dyn Write) -> Result<u64> {
        let body = self.get(url)?.into_body();
        let n = std::io::copy(&mut body.into_reader(), sink)?;
        Ok(n)
    }

    fn resolve_ref(&self, url: &str) -> Result<String> {
        let mut body = self.get(url)?.into_body();
        let text = body
            .read_to_string()
            .map_err(|e| Error::Http(e.to_string()))?;
        let value: serde_json::Value = serde_json::from_str(&text)?;
        value
            .get("sha")
            .and_then(|s| s.as_str())
            .map(str::to_string)
            .ok_or_else(|| Error::Model(format!("no `sha` in response from {url}")))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn urls_are_well_formed() {
        assert_eq!(
            file_url(
                "https://huggingface.co",
                "Qwen/Qwen3-Embedding-0.6B",
                "abc",
                "config.json"
            ),
            "https://huggingface.co/Qwen/Qwen3-Embedding-0.6B/resolve/abc/config.json"
        );
        // trailing slash on the endpoint is tolerated
        assert_eq!(
            file_url("https://mirror/", "o/r", "sha", "f"),
            "https://mirror/o/r/resolve/sha/f"
        );
        assert_eq!(
            revision_api_url("https://huggingface.co", "o/r", "main"),
            "https://huggingface.co/api/models/o/r/revision/main"
        );
    }
}
