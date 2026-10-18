//! Remote cache protocols for existing build tools, stored in Pit's CAS:
//! Turborepo (`TURBO_API=<pit>`) and Bazel's HTTP cache
//! (`--remote_cache=<pit>/bazel`).
//!
//! A task in a Pit run authenticates with its job's `PIT_CACHE_TOKEN` and
//! gets that run's cache scope, like Pit's own action cache: a branch's or a
//! pull request's builds never write what main reads (cache poisoning,
//! CVE-2025-36852). Requests with the API token keep full access. Bazel's
//! CAS is shared whatever the scope: each blob is checked against its
//! sha256, so nobody can plant one under another's name.

use std::sync::Arc;

use axum::body::Bytes;
use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, HeaderValue, Method, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::Deserialize;
use sha2::Digest;

use super::api::{not_found, unauthorized, ApiError};
use super::db::CacheScope;
use super::App;

/// The prefix of a job's cache token (see `Db::claim`).
pub const CACHE_TOKEN_PREFIX: &str = "pitc_";

fn bearer(headers: &HeaderMap) -> Option<&str> {
    headers
        .get(axum::http::header::AUTHORIZATION)?
        .to_str()
        .ok()?
        .strip_prefix("Bearer ")
        .map(str::trim)
}

/// A job's cache token gives its run's scope; one that is no longer live is
/// refused, never treated as the API token. Anything else got past auth with
/// the API token (or the server has none), which is trusted, as before.
fn scope(app: &App, headers: &HeaderMap) -> Result<CacheScope, ApiError> {
    match bearer(headers).filter(|b| b.starts_with(CACHE_TOKEN_PREFIX)) {
        None => Ok(CacheScope {
            trusted: true,
            repo: None,
        }),
        Some(token) => match app.db.cache_token_trust(token)? {
            Some((trusted, repo, github)) => Ok(app.cache_scope(&repo, github, trusted)),
            None => Err(unauthorized("this job's cache token is no longer live")),
        },
    }
}

/// The first entry under `ns` this scope may read.
fn find(app: &App, scope: &CacheScope, ns: &str, key: &str) -> Result<Option<String>, ApiError> {
    for ns in scope.read_keys(ns) {
        if let Some(v) = app.db.kv_get(&ns, key)? {
            return Ok(Some(v));
        }
    }
    Ok(None)
}

pub fn routes() -> Router<Arc<App>> {
    Router::new()
        .route("/v8/artifacts/status", get(turbo_status))
        .route("/v8/artifacts/events", post(turbo_events))
        .route("/v8/artifacts", post(turbo_query))
        .route(
            "/v8/artifacts/{hash}",
            get(turbo_get).head(turbo_get).put(turbo_put),
        )
        .route(
            "/bazel/{kind}/{hash}",
            get(bazel_get).head(bazel_get).put(bazel_put),
        )
}

#[derive(Deserialize)]
struct TeamQuery {
    #[serde(rename = "teamId")]
    team_id: Option<String>,
    slug: Option<String>,
}

impl TeamQuery {
    fn ns(&self) -> String {
        format!(
            "turbo:{}",
            self.team_id
                .as_deref()
                .or(self.slug.as_deref())
                .unwrap_or("default")
        )
    }
}

fn valid_key(k: &str) -> bool {
    !k.is_empty()
        && k.len() <= 128
        && k.bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

async fn turbo_status() -> Json<serde_json::Value> {
    Json(serde_json::json!({ "status": "enabled" }))
}

async fn turbo_events() -> StatusCode {
    StatusCode::OK
}

#[derive(Deserialize)]
struct HashesQuery {
    hashes: Vec<String>,
}

async fn turbo_query(
    State(app): State<Arc<App>>,
    Query(team): Query<TeamQuery>,
    headers: HeaderMap,
    Json(q): Json<HashesQuery>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let scope = scope(&app, &headers)?;
    let mut out = serde_json::Map::new();
    for h in q.hashes.iter().filter(|h| valid_key(h)) {
        if let Some(meta) = find(&app, &scope, &team.ns(), h)? {
            let (digest, duration) = split_meta(&meta);
            let size = app
                .cas
                .path(digest)
                .metadata()
                .map(|m| m.len())
                .unwrap_or(0);
            out.insert(
                h.clone(),
                serde_json::json!({ "size": size, "taskDurationMs": duration, "tag": null }),
            );
        }
    }
    Ok(Json(out.into()))
}

/// The kv value is `<digest>` or `<digest>:<duration ms>`.
fn split_meta(meta: &str) -> (&str, u64) {
    match meta.split_once(':') {
        Some((d, ms)) => (d, ms.parse().unwrap_or(0)),
        None => (meta, 0),
    }
}

async fn turbo_get(
    State(app): State<Arc<App>>,
    method: Method,
    Path(hash): Path<String>,
    Query(team): Query<TeamQuery>,
    req_headers: HeaderMap,
) -> Result<Response, ApiError> {
    if !valid_key(&hash) {
        return Err(not_found("artifact"));
    }
    let scope = scope(&app, &req_headers)?;
    let meta = find(&app, &scope, &team.ns(), &hash)?.ok_or_else(|| not_found("artifact"))?;
    let (digest, duration) = split_meta(&meta);
    let mut headers = HeaderMap::new();
    headers.insert(
        "content-type",
        HeaderValue::from_static("application/octet-stream"),
    );
    headers.insert("x-artifact-duration", HeaderValue::from(duration));
    if method == Method::HEAD {
        return Ok((headers, ()).into_response());
    }
    let bytes = app.cas.get(digest)?.ok_or_else(|| not_found("artifact"))?;
    Ok((headers, bytes).into_response())
}

async fn turbo_put(
    State(app): State<Arc<App>>,
    Path(hash): Path<String>,
    Query(team): Query<TeamQuery>,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Response, ApiError> {
    if !valid_key(&hash) {
        return Err(anyhow::anyhow!("invalid artifact hash").into());
    }
    let duration: u64 = headers
        .get("x-artifact-duration")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.parse().ok())
        .unwrap_or(0);
    let scope = scope(&app, &headers)?;
    let digest = app.cas.put(&body)?;
    app.db.kv_put(
        &scope.write_key(&team.ns()),
        &hash,
        &format!("{digest}:{duration}"),
    )?;
    Ok((
        StatusCode::ACCEPTED,
        Json(serde_json::json!({ "urls": [] })),
    )
        .into_response())
}

fn bazel_ns(kind: &str) -> Result<&'static str, ApiError> {
    match kind {
        "ac" => Ok("bazel:ac"),
        "cas" => Ok("bazel:cas"),
        _ => Err(not_found("bazel cache kind")),
    }
}

async fn bazel_get(
    State(app): State<Arc<App>>,
    method: Method,
    Path((kind, hash)): Path<(String, String)>,
    headers: HeaderMap,
) -> Result<Response, ApiError> {
    let ns = bazel_ns(&kind)?;
    let digest = if ns == "bazel:cas" {
        app.db.kv_get(ns, &hash)?
    } else {
        find(&app, &scope(&app, &headers)?, ns, &hash)?
    }
    .ok_or_else(|| not_found("blob"))?;
    if method == Method::HEAD {
        return Ok(StatusCode::OK.into_response());
    }
    let bytes = app.cas.get(&digest)?.ok_or_else(|| not_found("blob"))?;
    Ok(([("content-type", "application/octet-stream")], bytes).into_response())
}

async fn bazel_put(
    State(app): State<Arc<App>>,
    Path((kind, hash)): Path<(String, String)>,
    headers: HeaderMap,
    body: Bytes,
) -> Result<StatusCode, ApiError> {
    let ns = bazel_ns(&kind)?;
    // Checked before anything is stored, CAS writes included.
    let scope = scope(&app, &headers)?;
    if hash.len() != 64 || !hash.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err(anyhow::anyhow!("bazel keys are sha256 hex").into());
    }
    // CAS entries are addressed by their own sha256, so check it. Action cache
    // entries are keyed by the action digest, which is not the content hash.
    if ns == "bazel:cas" {
        let actual = hex::encode(sha2::Sha256::digest(&body));
        if actual != hash.to_ascii_lowercase() {
            return Err(anyhow::anyhow!("content sha256 {actual} does not match {hash}").into());
        }
    }
    let digest = app.cas.put(&body)?;
    let ns = if ns == "bazel:cas" {
        ns.to_string()
    } else {
        scope.write_key(ns)
    };
    app.db.kv_put(&ns, &hash.to_ascii_lowercase(), &digest)?;
    Ok(StatusCode::OK)
}
