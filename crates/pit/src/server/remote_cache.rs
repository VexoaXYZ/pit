//! Remote cache protocols for existing build tools, stored in Pit's CAS:
//! Turborepo (`TURBO_API=<pit>`) and Bazel's HTTP cache
//! (`--remote_cache=<pit>/bazel`).

use std::sync::Arc;

use axum::body::Bytes;
use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, HeaderValue, Method, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::Deserialize;
use sha2::Digest;

use super::api::{not_found, ApiError};
use super::App;

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
    Json(q): Json<HashesQuery>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let mut out = serde_json::Map::new();
    for h in q.hashes.iter().filter(|h| valid_key(h)) {
        if let Some(meta) = app.db.kv_get(&team.ns(), h)? {
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
) -> Result<Response, ApiError> {
    if !valid_key(&hash) {
        return Err(not_found("artifact"));
    }
    let meta = app
        .db
        .kv_get(&team.ns(), &hash)?
        .ok_or_else(|| not_found("artifact"))?;
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
    let digest = app.cas.put(&body)?;
    app.db
        .kv_put(&team.ns(), &hash, &format!("{digest}:{duration}"))?;
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
) -> Result<Response, ApiError> {
    let ns = bazel_ns(&kind)?;
    let digest = app.db.kv_get(ns, &hash)?.ok_or_else(|| not_found("blob"))?;
    if method == Method::HEAD {
        return Ok(StatusCode::OK.into_response());
    }
    let bytes = app.cas.get(&digest)?.ok_or_else(|| not_found("blob"))?;
    Ok(([("content-type", "application/octet-stream")], bytes).into_response())
}

async fn bazel_put(
    State(app): State<Arc<App>>,
    Path((kind, hash)): Path<(String, String)>,
    body: Bytes,
) -> Result<StatusCode, ApiError> {
    let ns = bazel_ns(&kind)?;
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
    app.db.kv_put(ns, &hash.to_ascii_lowercase(), &digest)?;
    Ok(StatusCode::OK)
}
