use std::io::Write;
use std::time::Duration;

use crate::error::{Error, Result};

pub const DEFAULT_ENDPOINT: &str = "https://huggingface.co";

const RESOLVE_TIMEOUT: Duration = Duration::from_secs(10);
const CONNECT_TIMEOUT: Duration = Duration::from_secs(30);
const RESPONSE_TIMEOUT: Duration = Duration::from_secs(60);
const REQUEST_TIMEOUT: Duration = Duration::from_secs(60 * 60);

pub fn file_url(endpoint: &str, repo: &str, rev: &str, file: &str) -> String {
    format!(
        "{}/{}/resolve/{}/{}",
        endpoint.trim_end_matches('/'),
        repo,
        rev,
        file
    )
}

pub fn revision_api_url(endpoint: &str, repo: &str, reference: &str) -> String {
    format!(
        "{}/api/models/{}/revision/{}",
        endpoint.trim_end_matches('/'),
        repo,
        reference
    )
}

pub trait Fetcher {
    fn download(&self, url: &str, sink: &mut dyn Write) -> Result<u64>;
    fn resolve_ref(&self, url: &str) -> Result<String>;
}

fn build_agent() -> ureq::Agent {
    let config = ureq::Agent::config_builder()
        .timeout_resolve(Some(RESOLVE_TIMEOUT))
        .timeout_connect(Some(CONNECT_TIMEOUT))
        .timeout_recv_response(Some(RESPONSE_TIMEOUT))
        .timeout_global(Some(REQUEST_TIMEOUT))
        .build();
    ureq::Agent::new_with_config(config)
}

pub struct UreqFetcher {
    token: Option<String>,
    agent: ureq::Agent,
}

impl UreqFetcher {
    pub fn from_env() -> Self {
        let token = std::env::var("HF_TOKEN")
            .ok()
            .or_else(|| std::env::var("HUGGING_FACE_HUB_TOKEN").ok())
            .filter(|t| !t.is_empty());
        Self {
            token,
            agent: build_agent(),
        }
    }

    pub fn with_token(token: Option<String>) -> Self {
        Self {
            token,
            agent: build_agent(),
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
