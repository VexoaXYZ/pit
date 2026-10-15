//! HTTP routes for the CLI, workers and dashboard.

use std::convert::Infallible;
use std::sync::Arc;
use std::time::Duration;

use axum::body::Bytes;
use axum::extract::{DefaultBodyLimit, Path, Query, Request, State};
use axum::http::{header, HeaderMap, StatusCode};
use axum::middleware::{self, Next};
use axum::response::sse::{Event as SseEvent, KeepAlive, Sse};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};

use serde::Deserialize;
use tokio_stream::wrappers::BroadcastStream;
use tokio_stream::StreamExt;

use super::App;
use crate::model::*;

pub struct ApiError(StatusCode, anyhow::Error);

impl<E: Into<anyhow::Error>> From<E> for ApiError {
    fn from(e: E) -> Self {
        ApiError(StatusCode::BAD_REQUEST, e.into())
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        (self.0, Json(serde_json::json!({ "error": format!("{:#}", self.1) }))).into_response()
    }
}

pub fn not_found(what: impl std::fmt::Display) -> ApiError {
    ApiError(StatusCode::NOT_FOUND, anyhow::anyhow!("{what} not found"))
}

type ApiResult<T> = Result<T, ApiError>;

pub fn router(app: Arc<App>) -> Router {
    let api = Router::new()
        .route("/api/health", get(health))
        .route("/api/stats", get(stats))
        .route("/api/repos", get(repos))
        .route("/api/runs", get(list_runs).post(create_run))
        .route("/api/runs/{id}", get(get_run))
        .route("/api/runs/{id}/cancel", post(cancel_run))
        .route("/api/runs/{id}/events", get(run_events))
        .route("/api/runs/{id}/tasks/{name}/log", get(run_task_log))
        .route("/api/events", get(all_events))
        .route("/api/tasks/{id}/log", get(task_log).post(append_log))
        .route("/api/tasks/{id}/start", post(start_task))
        .route("/api/tasks/{id}/finish", post(finish_task))
        .route("/api/groups/{id}/heartbeat", post(heartbeat))
        .route("/api/groups/{id}/finish", post(finish_group))
        .route("/api/workers", get(workers))
        .route("/api/workers/claim", post(claim))
        .route("/api/cache/{key}", get(cache_get).put(cache_put))
        .route("/api/cas/missing", post(cas_missing))
        .route("/api/cas/{digest}", get(cas_get).put(cas_put))
        .route("/api/queue", get(queue_list).post(queue_add))
        .merge(super::remote_cache::routes())
        .layer(middleware::from_fn_with_state(app.clone(), auth));
    Router::new()
        .merge(api)
        .route("/webhooks/github", post(super::github::webhook))
        .fallback(super::web::static_handler)
        .layer(DefaultBodyLimit::max(2 * 1024 * 1024 * 1024))
        .with_state(app)
}

#[derive(Deserialize)]
struct TokenQuery {
    token: Option<String>,
}

async fn auth(State(app): State<Arc<App>>, Query(q): Query<TokenQuery>, req: Request, next: Next) -> Response {
    let header = req.headers().get(header::AUTHORIZATION).and_then(|v| v.to_str().ok());
    if req.uri().path() == "/api/health" || app.authorized(header, q.token.as_deref()) {
        next.run(req).await
    } else {
        (StatusCode::UNAUTHORIZED, Json(serde_json::json!({ "error": "missing or invalid token" }))).into_response()
    }
}

async fn health(State(app): State<Arc<App>>) -> Json<serde_json::Value> {
    Json(serde_json::json!({
        "ok": true,
        "version": env!("CARGO_PKG_VERSION"),
        "auth": app.token.is_some(),
        "github": app.github.is_some(),
    }))
}

async fn stats(State(app): State<Arc<App>>) -> ApiResult<Json<serde_json::Value>> {
    let mut s = app.db.stats()?;
    let (blobs, bytes) = app.cas.stats();
    s["casBlobs"] = blobs.into();
    s["casBytes"] = bytes.into();
    Ok(Json(s))
}

async fn repos(State(app): State<Arc<App>>) -> ApiResult<Json<serde_json::Value>> {
    let repos: Vec<_> = app
        .db
        .repos()?
        .into_iter()
        .map(|r| serde_json::json!({ "id": r.id, "name": r.name, "url": r.url, "github": r.github }))
        .collect();
    Ok(Json(repos.into()))
}

async fn create_run(State(app): State<Arc<App>>, Json(req): Json<CreateRun>) -> ApiResult<Json<serde_json::Value>> {
    if let Some(d) = &req.overlay {
        if !app.cas.contains(d) {
            return Err(anyhow::anyhow!("overlay {d} is not in the CAS; upload it first").into());
        }
    }
    let id = app.create_run(req, None).await?;
    let url = format!("{}/runs/{id}", app.public_url);
    Ok(Json(serde_json::json!({ "id": id, "url": url })))
}

#[derive(Deserialize)]
struct ListQuery {
    limit: Option<usize>,
    repo: Option<String>,
}

async fn list_runs(State(app): State<Arc<App>>, Query(q): Query<ListQuery>) -> ApiResult<Json<Vec<RunView>>> {
    let ids = app.db.list_runs(q.limit.unwrap_or(50).min(500), q.repo.as_deref())?;
    let runs = ids.iter().map(|id| app.db.run_view(id)).collect::<anyhow::Result<_>>()?;
    Ok(Json(runs))
}

async fn get_run(State(app): State<Arc<App>>, Path(id): Path<String>) -> ApiResult<Json<RunView>> {
    app.db.run_view(&id).map(Json).map_err(|_| not_found(format!("run {id}")))
}

async fn cancel_run(State(app): State<Arc<App>>, Path(id): Path<String>) -> ApiResult<Json<serde_json::Value>> {
    app.db.cancel_run(&id)?;
    app.emit("run.updated", Some(&id), None, None, serde_json::Value::Null);
    app.group_finished(&id).await?;
    Ok(Json(serde_json::json!({ "ok": true })))
}


fn to_sse(e: &Event) -> SseEvent {
    SseEvent::default().data(serde_json::to_string(e).unwrap_or_default())
}

/// Live events for one run. Sends a `snapshot` first, so a client that
/// connects late still sees the full state, and closes after `run.finished`.
async fn run_events(State(app): State<Arc<App>>, Path(id): Path<String>) -> ApiResult<Response> {
    let rx = app.events.subscribe();
    let view = app.db.run_view(&id).map_err(|_| not_found(format!("run {id}")))?;
    let done = view.status.is_done();
    let snapshot = Event {
        kind: "snapshot".into(),
        run_id: Some(id.clone()),
        group_id: None,
        task_id: None,
        data: serde_json::to_value(&view)?,
    };
    let first = futures::stream::once(async move { Ok::<_, Infallible>(to_sse(&snapshot)) });
    if done {
        return Ok(Sse::new(first).into_response());
    }
    let live = async_stream::stream! {
        let mut rx = rx;
        loop {
            match rx.recv().await {
                Ok(e) if e.run_id.as_deref() == Some(id.as_str()) => {
                    let last = e.kind == "run.finished";
                    yield Ok(to_sse(&e));
                    if last {
                        break;
                    }
                }
                Ok(_) => {}
                // Fell behind: resend the whole state instead of a partial view.
                Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {
                    if let Ok(view) = app.db.run_view(&id) {
                        let done = view.status.is_done();
                        let e = Event {
                            kind: "snapshot".into(),
                            run_id: Some(id.clone()),
                            group_id: None,
                            task_id: None,
                            data: serde_json::to_value(&view).unwrap_or_default(),
                        };
                        yield Ok(to_sse(&e));
                        if done {
                            break;
                        }
                    }
                }
                Err(_) => break,
            }
        }
    };
    Ok(Sse::new(first.chain(live)).keep_alive(KeepAlive::default()).into_response())
}

async fn all_events(State(app): State<Arc<App>>) -> Response {
    let live = BroadcastStream::new(app.events.subscribe())
        .filter_map(|e| e.ok())
        .map(|e| Ok::<_, Infallible>(to_sse(&e)));
    Sse::new(live).keep_alive(KeepAlive::default()).into_response()
}

async fn task_log(State(app): State<Arc<App>>, Path(id): Path<String>) -> ApiResult<Response> {
    let text = read_task_log(&app, &id)?;
    Ok(([(header::CONTENT_TYPE, "text/plain; charset=utf-8")], text).into_response())
}

async fn run_task_log(State(app): State<Arc<App>>, Path((run, name)): Path<(String, String)>) -> ApiResult<Response> {
    let id = app.db.find_task(&run, &name)?.ok_or_else(|| not_found(format!("task {name} in run {run}")))?;
    task_log(State(app), Path(id)).await
}

fn read_task_log(app: &App, task_id: &str) -> anyhow::Result<String> {
    if let Some(d) = app.db.task_log_digest(task_id)? {
        if let Some(bytes) = app.cas.get(&d)? {
            return Ok(String::from_utf8_lossy(&bytes).into_owned());
        }
    }
    app.db.live_log(task_id)
}

async fn append_log(State(app): State<Arc<App>>, Path(id): Path<String>, Json(chunk): Json<LogChunk>) -> ApiResult<StatusCode> {
    let (group_id, run_id, _) = app.db.task_group(&id)?;
    app.db.append_log(&id, &chunk.text)?;
    app.emit("task.log", Some(&run_id), Some(&group_id), Some(&id), serde_json::json!({ "text": chunk.text }));
    Ok(StatusCode::NO_CONTENT)
}

async fn start_task(State(app): State<Arc<App>>, Path(id): Path<String>, Json(req): Json<TaskStart>) -> ApiResult<StatusCode> {
    let (group_id, run_id, name) = app.db.task_group(&id)?;
    app.db.start_task(&id, req.action_key.as_deref())?;
    app.emit("task.started", Some(&run_id), Some(&group_id), Some(&id), serde_json::json!({ "name": name }));
    Ok(StatusCode::NO_CONTENT)
}

async fn finish_task(State(app): State<Arc<App>>, Path(id): Path<String>, Json(r): Json<TaskResult>) -> ApiResult<StatusCode> {
    let (group_id, run_id, name) = app.db.task_group(&id)?;
    app.db.finish_task(&id, &r)?;
    if r.log_digest.is_some() {
        app.db.drop_live_log(&id)?;
    }
    app.emit(
        "task.finished",
        Some(&run_id),
        Some(&group_id),
        Some(&id),
        serde_json::json!({ "name": name, "status": r.status, "durationMs": r.duration_ms, "exitCode": r.exit_code }),
    );
    Ok(StatusCode::NO_CONTENT)
}

async fn heartbeat(State(app): State<Arc<App>>, Path(id): Path<String>) -> ApiResult<StatusCode> {
    app.db.heartbeat_group(&id)?;
    Ok(StatusCode::NO_CONTENT)
}

async fn finish_group(State(app): State<Arc<App>>, Path(id): Path<String>, Json(r): Json<GroupResult>) -> ApiResult<StatusCode> {
    let group = app.db.group(&id)?;
    app.db.finish_group(&id, &r)?;
    app.emit(
        "group.finished",
        Some(&group.run_id),
        Some(&id),
        None,
        serde_json::json!({ "name": group.name, "status": r.status, "prepareMs": r.prepare_ms }),
    );
    app.group_finished(&group.run_id).await?;
    Ok(StatusCode::NO_CONTENT)
}

async fn workers(State(app): State<Arc<App>>) -> ApiResult<Json<serde_json::Value>> {
    let now = now_ms();
    let list: Vec<_> = app
        .db
        .workers()?
        .into_iter()
        .map(|(info, seen, run)| {
            let mut v = serde_json::to_value(&info).unwrap_or_default();
            v["lastSeen"] = seen.into();
            v["online"] = (now - seen < 45_000).into();
            v["currentRun"] = run.into();
            v
        })
        .collect();
    Ok(Json(list.into()))
}

/// Long-poll: returns a job the moment one is claimable, or `idle` after 20s.
async fn claim(State(app): State<Arc<App>>, Json(info): Json<WorkerInfo>) -> ApiResult<Json<ClaimResponse>> {
    app.db.touch_worker(&info)?;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(20);
    loop {
        let notified = app.work.notified();
        tokio::pin!(notified);
        notified.as_mut().enable();
        if let Some(job) = app.db.claim_group(&info)? {
            app.db.set_run_status(&job.run_id, Status::Running, None)?;
            app.emit(
                "group.started",
                Some(&job.run_id),
                Some(&job.group_id),
                None,
                serde_json::json!({ "name": job.group, "worker": info.name }),
            );
            if app.github.is_some() {
                let app = app.clone();
                let run_id = job.run_id.clone();
                tokio::spawn(async move {
                    if let Some(gh) = &app.github {
                        gh.report_run(&app, &run_id).await;
                    }
                });
            }
            return Ok(Json(ClaimResponse::Job(Box::new(job))));
        }
        if tokio::time::timeout_at(deadline, notified).await.is_err() {
            app.db.touch_worker(&info)?;
            return Ok(Json(ClaimResponse::Idle));
        }
    }
}

async fn cache_get(State(app): State<Arc<App>>, Path(key): Path<String>) -> ApiResult<Json<CacheEntry>> {
    app.db.cache_get(&key)?.map(Json).ok_or_else(|| not_found("cache entry"))
}

async fn cache_put(State(app): State<Arc<App>>, Path(key): Path<String>, Json(entry): Json<CacheEntry>) -> ApiResult<StatusCode> {
    for d in entry.outputs.iter().map(|o| &o.digest).chain(entry.log_digest.iter()) {
        if !app.cas.contains(d) {
            return Err(anyhow::anyhow!("cache entry references {d}, which is not in the CAS").into());
        }
    }
    app.db.cache_put(&key, &entry)?;
    Ok(StatusCode::NO_CONTENT)
}

async fn cas_missing(State(app): State<Arc<App>>, Json(digests): Json<Vec<String>>) -> Json<Vec<String>> {
    Json(digests.into_iter().filter(|d| !app.cas.contains(d)).collect())
}

async fn cas_get(State(app): State<Arc<App>>, Path(digest): Path<String>) -> ApiResult<Response> {
    let bytes = app.cas.get(&digest)?.ok_or_else(|| not_found("blob"))?;
    Ok(([(header::CONTENT_TYPE, "application/octet-stream")], bytes).into_response())
}

async fn cas_put(State(app): State<Arc<App>>, Path(digest): Path<String>, body: Bytes) -> ApiResult<StatusCode> {
    app.cas.put_checked(&digest, &body)?;
    Ok(StatusCode::NO_CONTENT)
}

async fn queue_list(State(app): State<Arc<App>>) -> ApiResult<Json<Vec<QueueEntryView>>> {
    Ok(Json(super::queue::list(&app)?))
}

async fn queue_add(State(app): State<Arc<App>>, _h: HeaderMap, Json(req): Json<QueueAdd>) -> ApiResult<Json<QueueEntryView>> {
    Ok(Json(super::queue::add(&app, req).await?))
}
