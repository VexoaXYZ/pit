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
    http: reqwest::Client,
}

impl Client {
    pub fn new(base: &str, token: Option<String>) -> Result<Client> {
        Ok(Client {
            base: base.trim_end_matches('/').to_string(),
            token,
            http: reqwest::Client::builder()
                .connect_timeout(Duration::from_secs(5))
                .pool_idle_timeout(Duration::from_secs(90))
                .build()?,
        })
    }

    fn req(&self, method: reqwest::Method, path: &str) -> reqwest::RequestBuilder {
        let r = self.http.request(method, format!("{}{path}", self.base));
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
        let resp = self.req(reqwest::Method::GET, path).send().await.with_context(|| self.unreachable())?;
        Ok(Self::check(resp).await?.json().await?)
    }

    pub async fn get_opt<T: DeserializeOwned>(&self, path: &str) -> Result<Option<T>> {
        let resp = self.req(reqwest::Method::GET, path).send().await.with_context(|| self.unreachable())?;
        if resp.status() == reqwest::StatusCode::NOT_FOUND {
            return Ok(None);
        }
        Ok(Some(Self::check(resp).await?.json().await?))
    }

    pub async fn get_text(&self, path: &str) -> Result<String> {
        let resp = self.req(reqwest::Method::GET, path).send().await.with_context(|| self.unreachable())?;
        Ok(Self::check(resp).await?.text().await?)
    }

    pub async fn post<B: Serialize, T: DeserializeOwned>(&self, path: &str, body: &B) -> Result<T> {
        let resp = self.req(reqwest::Method::POST, path).json(body).send().await.with_context(|| self.unreachable())?;
        Ok(Self::check(resp).await?.json().await?)
    }

    pub async fn post_empty<B: Serialize>(&self, path: &str, body: &B) -> Result<()> {
        let resp = self.req(reqwest::Method::POST, path).json(body).send().await.with_context(|| self.unreachable())?;
        Self::check(resp).await?;
        Ok(())
    }

    pub async fn put_json<B: Serialize>(&self, path: &str, body: &B) -> Result<()> {
        let resp = self.req(reqwest::Method::PUT, path).json(body).send().await.with_context(|| self.unreachable())?;
        Self::check(resp).await?;
        Ok(())
    }

    fn unreachable(&self) -> String {
        format!("could not reach the Pit server at {} (start one with `pit up`)", self.base)
    }

    /// Long-poll for work. Uses its own timeout, longer than the server's.
    pub async fn claim(&self, info: &WorkerInfo) -> Result<ClaimResponse> {
        let resp = self
            .req(reqwest::Method::POST, "/api/workers/claim")
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
        let resp = self.req(reqwest::Method::GET, &format!("/api/cas/{digest}")).send().await.with_context(|| self.unreachable())?;
        let bytes = Self::check(resp).await?.bytes().await?;
        if crate::cas::digest(&bytes) != digest {
            bail!("blob {digest} failed verification");
        }
        Ok(bytes.to_vec())
    }

    /// Uploads blobs the server does not have yet, a few at a time.
    pub async fn upload_missing(&self, blobs: Vec<(String, Vec<u8>)>) -> Result<usize> {
        let digests: Vec<String> = blobs.iter().map(|(d, _)| d.clone()).collect();
        let missing: std::collections::HashSet<String> = self.cas_missing(&digests).await?.into_iter().collect();
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
        let mut buf = String::new();
        while let Some(chunk) = stream.next().await {
            buf.push_str(&String::from_utf8_lossy(&chunk?));
            while let Some(end) = buf.find("\n\n") {
                let frame: String = buf.drain(..end + 2).collect();
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
