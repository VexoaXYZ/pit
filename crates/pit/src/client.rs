//! HTTP client for the Pit API, used by workers, the CLI and the MCP server.

use std::time::Duration;

use anyhow::{bail, Context, Result};
use futures::StreamExt;
use serde::de::DeserializeOwned;
use serde::Serialize;

use crate::model::*;

#[derive(Clone)]
pub struct Client {
    pub base: String,
    token: Option<String>,
    /// Sent as `x-pit-lease` on job updates (see `Job::lease`).
    lease: Option<String>,
    /// Sent when claiming jobs (see `ServerOpts::worker_token`).
    worker_token: Option<String>,
    http: reqwest::Client,
}

/// Percent-encodes one URL path segment, so caller-supplied ids cannot
/// change which endpoint is called. `.` and `..` are rejected outright: URL
/// parsers treat even their encoded forms as dot segments.
pub fn seg(s: &str) -> Result<String> {
    if s.is_empty()
        || s == "."
        || s == ".."
        || s.eq_ignore_ascii_case("%2e")
        || s.eq_ignore_ascii_case("%2e%2e")
    {
        bail!("invalid identifier {s:?}");
    }
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b'~') {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    Ok(out)
}

pub const LEASE_HEADER: &str = "x-pit-lease";
pub const WORKER_TOKEN_HEADER: &str = "x-pit-worker-token";

impl Client {
    /// A client whose requests prove they come from the current claim of a group.
    pub fn with_worker_token(&self, token: Option<String>) -> Client {
        Client {
            worker_token: token,
            ..self.clone()
        }
    }

    pub fn with_lease(&self, lease: &str) -> Client {
        Client {
            lease: Some(lease.to_string()),
            ..self.clone()
        }
    }

    pub fn new(base: &str, token: Option<String>) -> Result<Client> {
        Ok(Client {
            base: base.trim_end_matches('/').to_string(),
            token,
            lease: None,
            worker_token: None,
            http: reqwest::Client::builder()
                .connect_timeout(Duration::from_secs(5))
                .pool_idle_timeout(Duration::from_secs(90))
                .build()?,
        })
    }

    fn req(&self, method: reqwest::Method, path: &str) -> reqwest::RequestBuilder {
        let mut r = self.http.request(method, format!("{}{path}", self.base));
        if let Some(lease) = &self.lease {
            r = r.header(LEASE_HEADER, lease);
        }
        match &self.token {
            Some(t) => r.bearer_auth(t),
            None => r,
        }
    }

    async fn check(resp: reqwest::Response) -> Result<reqwest::Response> {
        let status = resp.status();
        if status.is_success() {
            return Ok(resp);
        }
        let body = resp.text().await.unwrap_or_default();
        let msg = serde_json::from_str::<serde_json::Value>(&body)
            .ok()
            .and_then(|v| v["error"].as_str().map(String::from))
            .unwrap_or(body);
        bail!("{status}: {msg}")
    }

    pub async fn get<T: DeserializeOwned>(&self, path: &str) -> Result<T> {
        let resp = self
            .req(reqwest::Method::GET, path)
            .send()
            .await
            .with_context(|| self.unreachable())?;
        Ok(Self::check(resp).await?.json().await?)
    }

    pub async fn get_opt<T: DeserializeOwned>(&self, path: &str) -> Result<Option<T>> {
        let resp = self
            .req(reqwest::Method::GET, path)
            .send()
            .await
            .with_context(|| self.unreachable())?;
        if resp.status() == reqwest::StatusCode::NOT_FOUND {
            return Ok(None);
        }
        Ok(Some(Self::check(resp).await?.json().await?))
    }

    pub async fn get_text(&self, path: &str) -> Result<String> {
        let resp = self
            .req(reqwest::Method::GET, path)
            .send()
            .await
            .with_context(|| self.unreachable())?;
        Ok(Self::check(resp).await?.text().await?)
    }

    pub async fn post<B: Serialize, T: DeserializeOwned>(&self, path: &str, body: &B) -> Result<T> {
        let resp = self
            .req(reqwest::Method::POST, path)
            .json(body)
            .send()
            .await
            .with_context(|| self.unreachable())?;
        Ok(Self::check(resp).await?.json().await?)
    }

    pub async fn post_empty<B: Serialize>(&self, path: &str, body: &B) -> Result<()> {
        let resp = self
            .req(reqwest::Method::POST, path)
            .json(body)
            .send()
            .await
            .with_context(|| self.unreachable())?;
        Self::check(resp).await?;
        Ok(())
    }

    pub async fn delete<T: DeserializeOwned>(&self, path: &str) -> Result<T> {
        let resp = self
            .req(reqwest::Method::DELETE, path)
            .send()
            .await
            .with_context(|| self.unreachable())?;
        Ok(Self::check(resp).await?.json().await?)
    }

    pub async fn put_json<B: Serialize>(&self, path: &str, body: &B) -> Result<()> {
        let resp = self
            .req(reqwest::Method::PUT, path)
            .json(body)
            .send()
            .await
            .with_context(|| self.unreachable())?;
        Self::check(resp).await?;
        Ok(())
    }

    fn unreachable(&self) -> String {
        format!(
            "could not reach the Pit server at {} (start one with `pit up`)",
            self.base
        )
    }

    /// Long-poll for work. Uses its own timeout, longer than the server's.
    pub async fn claim(&self, info: &WorkerInfo) -> Result<ClaimResponse> {
        let resp = self
            .req(reqwest::Method::POST, "/api/workers/claim")
            .header(
                WORKER_TOKEN_HEADER,
                self.worker_token.as_deref().unwrap_or_default(),
            )
            .timeout(Duration::from_secs(40))
            .json(info)
            .send()
            .await
            .with_context(|| self.unreachable())?;
        Ok(Self::check(resp).await?.json().await?)
    }

    pub async fn cas_missing(&self, digests: &[String]) -> Result<Vec<String>> {
        if digests.is_empty() {
            return Ok(vec![]);
        }
        self.post("/api/cas/missing", &digests).await
    }

    pub async fn cas_put(&self, digest: &str, bytes: Vec<u8>) -> Result<()> {
        let resp = self
            .req(reqwest::Method::PUT, &format!("/api/cas/{digest}"))
            .body(bytes)
            .send()
            .await
            .with_context(|| self.unreachable())?;
        Self::check(resp).await?;
        Ok(())
    }

    pub async fn cas_get(&self, digest: &str) -> Result<Vec<u8>> {
        let resp = self
            .req(reqwest::Method::GET, &format!("/api/cas/{digest}"))
            .send()
            .await
            .with_context(|| self.unreachable())?;
        let bytes = Self::check(resp).await?.bytes().await?;
        if crate::cas::digest(&bytes) != digest {
            bail!("blob {digest} failed verification");
        }
        Ok(bytes.to_vec())
    }

    /// Uploads blobs the server does not have yet, a few at a time.
    pub async fn upload_missing(&self, blobs: Vec<(String, Vec<u8>)>) -> Result<usize> {
        let digests: Vec<String> = blobs.iter().map(|(d, _)| d.clone()).collect();
        let missing: std::collections::HashSet<String> =
            self.cas_missing(&digests).await?.into_iter().collect();
        let uploads = blobs.into_iter().filter(|(d, _)| missing.contains(d));
        let results: Vec<Result<()>> = futures::stream::iter(uploads)
            .map(|(d, b)| async move { self.cas_put(&d, b).await })
            .buffer_unordered(8)
            .collect()
            .await;
        for r in &results {
            if let Err(e) = r {
                bail!("upload failed: {e:#}");
            }
        }
        Ok(results.len())
    }

    /// Streams server-sent events from `path`, calling `f` for each until it
    /// returns false or the stream ends.
    pub async fn events(&self, path: &str, mut f: impl FnMut(Event) -> bool) -> Result<()> {
        let resp = self
            .req(reqwest::Method::GET, path)
            .header("accept", "text/event-stream")
            .send()
            .await
            .with_context(|| self.unreachable())?;
        let mut stream = Self::check(resp).await?.bytes_stream();
        // Bytes, not text: a multi-byte character can straddle two chunks.
        let mut buf: Vec<u8> = Vec::new();
        while let Some(chunk) = stream.next().await {
            buf.extend_from_slice(&chunk?);
            while let Some(end) = buf.windows(2).position(|w| w == b"\n\n") {
                let frame_bytes: Vec<u8> = buf.drain(..end + 2).collect();
                let frame = String::from_utf8_lossy(&frame_bytes);
                let data: String = frame
                    .lines()
                    .filter_map(|l| l.strip_prefix("data:"))
                    .map(|l| l.strip_prefix(' ').unwrap_or(l))
                    .collect::<Vec<_>>()
                    .join("\n");
                if data.is_empty() {
                    continue;
                }
                if let Ok(event) = serde_json::from_str::<Event>(&data) {
                    if !f(event) {
                        return Ok(());
                    }
                }
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn path_segments_cannot_escape() {
        use super::seg;
        assert_eq!(seg("ci.test").unwrap(), "ci.test");
        assert_eq!(seg("a?b#c").unwrap(), "a%3Fb%23c");
        assert!(seg("..").is_err() && seg(".").is_err() && seg("").is_err());
        // What matters is the URL that is actually requested.
        for (input, want) in [
            ("x/../../queue", "/api/runs/x%2F..%2F..%2Fqueue/log"),
            ("..a", "/api/runs/..a/log"),
        ] {
            let url =
                reqwest::Url::parse(&format!("http://h/api/runs/{}/log", seg(input).unwrap()))
                    .unwrap();
            assert_eq!(url.path(), want);
        }
    }
}
