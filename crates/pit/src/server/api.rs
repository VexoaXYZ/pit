//! HTTP routes for the CLI, workers and dashboard.

use std::collections::HashMap;
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
        (
            self.0,
            Json(serde_json::json!({ "error": format!("{:#}", self.1) })),
        )
            .into_response()
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
        .route("/api/tasks/{id}/annotations", post(set_annotations))
        .route("/api/tasks/{id}/finish", post(finish_task))
        .route("/api/groups/{id}/heartbeat", post(heartbeat))
        .route("/api/groups/{id}/release", post(release_group))
        .route("/api/groups/{id}/finish", post(finish_group))
        .route("/api/workers", get(workers))
        .route("/api/workers/claim", post(claim))
        .route("/api/cache/{key}", get(cache_get).put(cache_put))
        .route("/api/cache/last/{scope}", get(cache_last))
        .route("/api/cache/failed/{key}", get(cache_failed))
        .route("/api/cache/{key}/claim", post(cache_claim))
        .route("/api/flaky", get(flaky))
        .route("/api/slow", get(slow))
        .route("/metrics", get(metrics))
        .route("/api/timings", post(record_timings))
        .route("/api/oidc/token", post(oidc_token))
        .route("/.well-known/openid-configuration", get(oidc_discovery))
        .route("/.well-known/jwks.json", get(oidc_jwks))
        .route("/api/gc", post(gc_now))
        .route("/api/cas/missing", post(cas_missing))
        .route("/api/cas/{digest}", get(cas_get).put(cas_put))
        .route("/api/queue", get(queue_list).post(queue_add))
        .route("/api/queue/{key}", axum::routing::delete(queue_remove))
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

async fn auth(
    State(app): State<Arc<App>>,
    Query(q): Query<TokenQuery>,
    req: Request,
    next: Next,
) -> Response {
    let header = req
        .headers()
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok());
    // OIDC discovery and keys are public: clouds fetch them to verify tokens.
    let path = req.uri().path();
    if path == "/api/health"
        || path.starts_with("/.well-known/")
        || app.authorized(header, q.token.as_deref())
    {
        next.run(req).await
    } else {
        (
            StatusCode::UNAUTHORIZED,
            Json(serde_json::json!({ "error": "missing or invalid token" })),
        )
            .into_response()
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
    let load = |a: &std::sync::atomic::AtomicU64| a.load(std::sync::atomic::Ordering::Relaxed);
    s["cacheLimitBytes"] = load(&app.cache_max_bytes).into();
    s["evictedEntries"] = load(&app.evicted_entries).into();
    s["evictedBytes"] = load(&app.freed_bytes).into();
    Ok(Json(s))
}

/// `POST /api/groups/{id}/release`: a worker that is being stopped hands its
/// group back, so another worker can start it now rather than after the
/// heartbeat timeout.
async fn release_group(
    State(app): State<Arc<App>>,
    Path(id): Path<String>,
    headers: HeaderMap,
) -> ApiResult<StatusCode> {
    let lease = lease(&headers).ok_or_else(|| stale(&id))?;
    if !app.db.requeue_group(&id, lease)? {
        return Err(stale(&id));
    }
    tracing::info!("group {id} released by its worker; requeued");
    app.work.notify_waiters();
    Ok(StatusCode::NO_CONTENT)
}

/// `GET /metrics`: Prometheus text format. The queue gauges are what to
/// scale workers on (KEDA, an HPA, or a script that starts `pit worker`):
/// work waiting, and for how long.
async fn metrics(
    State(app): State<Arc<App>>,
) -> ApiResult<([(header::HeaderName, &'static str); 1], String)> {
    use std::fmt::Write;
    let now = crate::model::now_ms();
    let mut out = String::new();
    let mut gauge = |name: &str, help: &str, rows: &[(String, f64)]| {
        let _ = writeln!(out, "# HELP {name} {help}\n# TYPE {name} gauge");
        for (labels, v) in rows {
            let _ = writeln!(out, "{name}{labels} {v}");
        }
    };
    let depth = app.db.queue_depth()?;
    let queued: Vec<(String, f64)> = depth
        .iter()
        .map(|(os, n, _)| (format!("{{os=\"{os}\"}}"), *n as f64))
        .collect();
    let oldest: Vec<(String, f64)> = depth
        .iter()
        .map(|(os, _, at)| {
            (
                format!("{{os=\"{os}\"}}"),
                ((now - at).max(0) as f64) / 1000.0,
            )
        })
        .collect();
    gauge(
        "pit_queued_groups",
        "Groups waiting for a worker, by OS.",
        &queued,
    );
    gauge(
        "pit_oldest_queued_seconds",
        "How long the oldest waiting group has waited, by OS.",
        &oldest,
    );
    gauge(
        "pit_running_groups",
        "Groups running on workers.",
        &[(String::new(), app.db.running_groups()? as f64)],
    );
    let workers = app.db.workers()?;
    let online = workers
        .iter()
        .filter(|(_, seen, _)| now - seen < 30_000)
        .count();
    let busy = workers
        .iter()
        .filter(|(_, seen, run)| now - seen < 30_000 && run.is_some())
        .count();
    gauge(
        "pit_workers_online",
        "Workers seen in the last 30s.",
        &[(String::new(), online as f64)],
    );
    gauge(
        "pit_workers_busy",
        "Online workers running a group.",
        &[(String::new(), busy as f64)],
    );
    let s = app.db.stats()?;
    let counter = |k: &str| s[k].as_f64().unwrap_or(0.0);
    let _ = writeln!(out, "# HELP pit_cache_hits_total Task results served from the cache.\n# TYPE pit_cache_hits_total counter\npit_cache_hits_total {}", counter("cacheHits"));
    let _ = writeln!(out, "# HELP pit_tasks_run_total Tasks executed.\n# TYPE pit_tasks_run_total counter\npit_tasks_run_total {}", counter("tasksRun"));
    let _ = writeln!(out, "# HELP pit_time_saved_seconds_total Task time saved by cache hits.\n# TYPE pit_time_saved_seconds_total counter\npit_time_saved_seconds_total {}", counter("timeSavedMs") / 1000.0);
    Ok(([(header::CONTENT_TYPE, "text/plain; version=0.0.4")], out))
}

async fn repos(State(app): State<Arc<App>>) -> ApiResult<Json<serde_json::Value>> {
    let repos: Vec<_> = app
        .db
        .repos()?
        .into_iter()
        .map(
            |r| serde_json::json!({ "id": r.id, "name": r.name, "url": r.url, "github": r.github }),
        )
        .collect();
    Ok(Json(repos.into()))
}

async fn create_run(
    State(app): State<Arc<App>>,
    Json(req): Json<CreateRun>,
) -> ApiResult<Json<serde_json::Value>> {
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

async fn list_runs(
    State(app): State<Arc<App>>,
    Query(q): Query<ListQuery>,
) -> ApiResult<Json<Vec<RunView>>> {
    let ids = app
        .db
        .list_runs(q.limit.unwrap_or(50).min(500), q.repo.as_deref())?;
    let runs = ids
        .iter()
        .map(|id| app.db.run_view(id))
        .collect::<anyhow::Result<_>>()?;
    Ok(Json(runs))
}

async fn get_run(State(app): State<Arc<App>>, Path(id): Path<String>) -> ApiResult<Json<RunView>> {
    app.db
        .run_view(&id)
        .map(Json)
        .map_err(|_| not_found(format!("run {id}")))
}

async fn cancel_run(
    State(app): State<Arc<App>>,
    Path(id): Path<String>,
) -> ApiResult<Json<serde_json::Value>> {
    if app.db.cancel_run(&id)? {
        app.run_finished(&id).await;
    }
    Ok(Json(serde_json::json!({ "ok": true })))
}

fn to_sse(e: &Event) -> SseEvent {
    SseEvent::default().data(serde_json::to_string(e).unwrap_or_default())
}

/// Live events for one run. Sends a `snapshot` first, so a client that
/// connects late still sees the full state, and closes after `run.finished`.
async fn run_events(State(app): State<Arc<App>>, Path(id): Path<String>) -> ApiResult<Response> {
    let rx = app.events.subscribe();
    let view = app
        .db
        .run_view(&id)
        .map_err(|_| not_found(format!("run {id}")))?;
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
    Ok(Sse::new(first.chain(live))
        .keep_alive(KeepAlive::default())
        .into_response())
}

async fn all_events(State(app): State<Arc<App>>) -> Response {
    let live = BroadcastStream::new(app.events.subscribe())
        .filter_map(|e| e.ok())
        .map(|e| Ok::<_, Infallible>(to_sse(&e)));
    Sse::new(live)
        .keep_alive(KeepAlive::default())
        .into_response()
}

async fn task_log(State(app): State<Arc<App>>, Path(id): Path<String>) -> ApiResult<Response> {
    let text = read_task_log(&app, &id)?;
    Ok(([(header::CONTENT_TYPE, "text/plain; charset=utf-8")], text).into_response())
}

async fn run_task_log(
    State(app): State<Arc<App>>,
    Path((run, name)): Path<(String, String)>,
) -> ApiResult<Response> {
    let id = app
        .db
        .find_task(&run, &name)?
        .ok_or_else(|| not_found(format!("task {name} in run {run}")))?;
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

fn lease(headers: &HeaderMap) -> Option<&str> {
    headers
        .get(crate::client::LEASE_HEADER)
        .and_then(|v| v.to_str().ok())
}

/// Job updates must come from the group's current claim.
fn stale(group_id: &str) -> ApiError {
    ApiError(
        StatusCode::CONFLICT,
        anyhow::anyhow!("group {group_id} is no longer claimed by this worker"),
    )
}

async fn append_log(
    State(app): State<Arc<App>>,
    Path(id): Path<String>,
    headers: HeaderMap,
    Json(chunk): Json<LogChunk>,
) -> ApiResult<StatusCode> {
    let (group_id, run_id, _) = app.db.task_group(&id)?;
    if !app.db.append_log(&id, lease(&headers), &chunk.text)? {
        return Err(stale(&group_id));
    }
    app.emit(
        "task.log",
        Some(&run_id),
        Some(&group_id),
        Some(&id),
        serde_json::json!({ "text": chunk.text }),
    );
    Ok(StatusCode::NO_CONTENT)
}

/// Annotations a task keeps at most.
const MAX_ANNOTATIONS: usize = 200;

/// `POST /api/tasks/{id}/annotations`: where its failed tests are, from the
/// running job.
async fn set_annotations(
    State(app): State<Arc<App>>,
    Path(id): Path<String>,
    headers: HeaderMap,
    Json(mut annotations): Json<Vec<Annotation>>,
) -> ApiResult<StatusCode> {
    let (group_id, _, _) = app.db.task_group(&id)?;
    annotations.truncate(MAX_ANNOTATIONS);
    for a in &mut annotations {
        a.title = a.title.chars().take(255).collect();
        a.message = a.message.chars().take(1000).collect();
    }
    if !app.db.set_annotations(&id, lease(&headers), &annotations)? {
        return Err(stale(&group_id));
    }
    Ok(StatusCode::NO_CONTENT)
}

async fn start_task(
    State(app): State<Arc<App>>,
    Path(id): Path<String>,
    headers: HeaderMap,
    Json(req): Json<TaskStart>,
) -> ApiResult<StatusCode> {
    let (group_id, run_id, name) = app.db.task_group(&id)?;
    if !app
        .db
        .start_task(&id, lease(&headers), req.action_key.as_deref())?
    {
        return Err(stale(&group_id));
    }
    app.emit(
        "task.started",
        Some(&run_id),
        Some(&group_id),
        Some(&id),
        serde_json::json!({ "name": name }),
    );
    Ok(StatusCode::NO_CONTENT)
}

async fn finish_task(
    State(app): State<Arc<App>>,
    Path(id): Path<String>,
    headers: HeaderMap,
    Json(mut r): Json<TaskResult>,
) -> ApiResult<StatusCode> {
    let (group_id, run_id, name) = app.db.task_group(&id)?;
    // A pass that took far longer than this task usually does says so: CI
    // that gets slower otherwise goes unnoticed until everything is slow.
    // Not for a pass that needed a retry (its time is every attempt's) or a
    // cold workspace (`--fresh`, forks), which warm history would always
    // call slow.
    let flaky = r
        .message
        .as_deref()
        .is_some_and(|m| m.starts_with("flaky:"));
    let run = app.db.run(&run_id)?;
    if r.status == Status::Passed && !flaky && !run.fresh && !run.no_secrets {
        let (a, task) = (app.clone(), id.clone());
        let history =
            tokio::task::spawn_blocking(move || a.db.recent_durations(&task, crate::slow::HISTORY))
                .await
                .map_err(anyhow::Error::from)??;
        if let Some(note) = crate::slow::note(r.duration_ms as i64, &history) {
            r.message = Some(match r.message.take() {
                Some(m) => format!("{m} · {note}"),
                None => note,
            });
        }
    }
    if !app.db.finish_task(&id, lease(&headers), &r)? {
        return Err(stale(&group_id));
    }
    if r.log_digest.is_some() {
        app.db.drop_live_log(&id)?;
    }
    // Its single-flight claims (see `cache_claim`) end with it.
    app.inflight.lock().retain(|_, holder| *holder != id);
    app.emit(
        "task.finished",
        Some(&run_id),
        Some(&group_id),
        Some(&id),
        serde_json::json!({ "name": name, "status": r.status, "durationMs": r.duration_ms, "exitCode": r.exit_code, "message": r.message }),
    );
    Ok(StatusCode::NO_CONTENT)
}

async fn heartbeat(
    State(app): State<Arc<App>>,
    Path(id): Path<String>,
    headers: HeaderMap,
) -> ApiResult<StatusCode> {
    if !app.db.heartbeat_group(&id, lease(&headers))? {
        return Err(stale(&id));
    }
    Ok(StatusCode::NO_CONTENT)
}

async fn finish_group(
    State(app): State<Arc<App>>,
    Path(id): Path<String>,
    headers: HeaderMap,
    Json(r): Json<GroupResult>,
) -> ApiResult<StatusCode> {
    let group = app.db.group(&id)?;
    let lease = headers
        .get(crate::client::LEASE_HEADER)
        .and_then(|v| v.to_str().ok());
    // Checked in the same statement as the update, so a group requeued in
    // between cannot be finished by its old worker.
    if !app.db.finish_group(&id, lease, &r)? {
        return Err(ApiError(
            StatusCode::CONFLICT,
            anyhow::anyhow!("group {id} is no longer claimed by this worker"),
        ));
    }
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
async fn claim(
    State(app): State<Arc<App>>,
    headers: HeaderMap,
    Json(info): Json<WorkerInfo>,
) -> ApiResult<Json<ClaimResponse>> {
    let required = app.worker_token.lock().clone();
    if let Some(required) = required {
        let given = headers
            .get(crate::client::WORKER_TOKEN_HEADER)
            .and_then(|v| v.to_str().ok());
        if given != Some(required.as_str()) {
            return Err(ApiError(
                StatusCode::FORBIDDEN,
                anyhow::anyhow!("claiming jobs needs the worker token (PIT_WORKER_TOKEN)"),
            ));
        }
    }
    app.db.touch_worker(&info)?;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(20);
    loop {
        let notified = app.work.notified();
        tokio::pin!(notified);
        notified.as_mut().enable();
        let (job, held) = app.db.claim_group(&info)?;
        if let Some(job) = job {
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
        // A group held for its warm worker becomes ours after a short wait,
        // and no event announces that, so look again soon.
        let wake = if held {
            deadline.min(tokio::time::Instant::now() + Duration::from_millis(250))
        } else {
            deadline
        };
        if tokio::time::timeout_at(wake, notified).await.is_err()
            && tokio::time::Instant::now() >= deadline
        {
            app.db.touch_worker(&info)?;
            return Ok(Json(ClaimResponse::Idle));
        }
    }
}

/// Trust comes from the job's lease, never from the request: only a worker
/// running a trusted run's group holds one. A lease that is no longer live
/// (the run was cancelled or requeued) is refused rather than treated as
/// untrusted, which would let a trusted job read untrusted results.
fn cache_trust(app: &App, headers: &HeaderMap) -> ApiResult<bool> {
    match lease(headers) {
        None => Ok(false),
        Some(l) => match app.db.lease_trust(l)? {
            Some(trusted) => Ok(trusted),
            None => Err(ApiError(
                StatusCode::CONFLICT,
                anyhow::anyhow!("this job's lease is no longer live"),
            )),
        },
    }
}

async fn cache_get(
    State(app): State<Arc<App>>,
    Path(key): Path<String>,
    headers: HeaderMap,
) -> ApiResult<Json<CacheEntry>> {
    let trusted = cache_trust(&app, &headers)?;
    app.db
        .cache_find(&key, trusted)?
        .map(|stored| app.db.cache_get(&stored))
        .transpose()?
        .flatten()
        .map(Json)
        .ok_or_else(|| not_found("cache entry"))
}

#[derive(Deserialize)]
struct ScopeQuery {
    /// Identifies the task (repo, group, task, OS) so its latest cached run
    /// can be found when a later run misses.
    scope: Option<String>,
}

async fn cache_put(
    State(app): State<Arc<App>>,
    Path(key): Path<String>,
    Query(q): Query<ScopeQuery>,
    headers: HeaderMap,
    Json(entry): Json<CacheEntry>,
) -> ApiResult<StatusCode> {
    for d in entry
        .outputs
        .iter()
        .map(|o| &o.digest)
        .chain(entry.log_digest.iter())
        .chain(entry.inputs.iter())
    {
        if !app.cas.contains(d) {
            return Err(
                anyhow::anyhow!("cache entry references {d}, which is not in the CAS").into(),
            );
        }
    }
    let trusted = cache_trust(&app, &headers)?;
    let stored = app.db.cache_put(&key, trusted, &entry)?;
    // Whoever was waiting on this key finds the result now.
    app.inflight
        .lock()
        .remove(&format!("{}:{key}", u8::from(trusted)));
    if let Some(scope) = q.scope.filter(|s| crate::cas::is_digest(s)) {
        app.db.kv_put("last", &scope, &stored)?;
    }
    Ok(StatusCode::NO_CONTENT)
}

/// The latest cached run of a task, to explain why a new run missed.
/// 410 Gone when it existed but has been evicted.
async fn cache_last(
    State(app): State<Arc<App>>,
    Path(scope): Path<String>,
) -> ApiResult<Json<CacheEntry>> {
    let key = app
        .db
        .kv_get("last", &scope)?
        .ok_or_else(|| not_found("previous run"))?;
    app.db.cache_peek(&key)?.map(Json).ok_or_else(|| {
        ApiError(
            StatusCode::GONE,
            anyhow::anyhow!("previous result was evicted"),
        )
    })
}

async fn oidc_discovery(State(app): State<Arc<App>>) -> ApiResult<Json<serde_json::Value>> {
    let issuer = app.oidc.get().ok_or_else(|| not_found("OIDC issuer"))?;
    Ok(Json(issuer.discovery(&app.public_url)))
}

async fn oidc_jwks(State(app): State<Arc<App>>) -> ApiResult<Json<serde_json::Value>> {
    let issuer = app.oidc.get().ok_or_else(|| not_found("OIDC issuer"))?;
    Ok(Json(issuer.jwks()))
}

#[derive(Deserialize)]
struct OidcRequest {
    audience: String,
}

/// `POST /api/oidc/token`: a token for the job holding this lease, for
/// trusted runs of repos GitHub has vouched for (registered by a signed
/// webhook) only. The claims come from the run as the server recorded it,
/// never from the request.
async fn oidc_token(
    State(app): State<Arc<App>>,
    headers: HeaderMap,
    Json(req): Json<OidcRequest>,
) -> ApiResult<Json<serde_json::Value>> {
    let forbidden = |m: &str| ApiError(StatusCode::FORBIDDEN, anyhow::anyhow!("{m}"));
    let issuer = app
        .oidc
        .get()
        .ok_or_else(|| forbidden("this server does not issue OIDC tokens (pit server --oidc)"))?;
    let run_id = match lease(&headers) {
        Some(l) => app.db.lease_run(l)?,
        None => None,
    }
    .ok_or_else(|| forbidden("OIDC tokens are only issued to a running job"))?;
    let run = app.db.run(&run_id)?;
    // Pull requests, other branches, `pit run`, API-created runs and forks:
    // code nobody has merged gets no cloud identity.
    if !run.trusted || run.no_secrets {
        return Err(forbidden(
            "OIDC tokens are only issued to trusted runs (the default branch, its merge queue and schedules)",
        ));
    }
    let repo = app.db.repo(&run.repo_id)?;
    let Some(full_name) = repo.github else {
        return Err(forbidden(
            "OIDC tokens are only issued for repos registered by a GitHub webhook",
        ));
    };
    if req.audience.is_empty() {
        return Err(anyhow::anyhow!("audience is required").into());
    }
    let sub = super::oidc::subject(&full_name, &run.trigger, run.git_ref.as_deref())
        .ok_or_else(|| forbidden("this run has no OIDC subject"))?;
    let now = crate::model::now_ms() / 1000;
    let claims = serde_json::json!({
        "iss": app.public_url,
        "aud": req.audience,
        "sub": sub,
        "iat": now,
        "nbf": now - 5,
        "exp": now + super::oidc::TOKEN_SECS,
        "jti": crate::model::new_id("jwt"),
        "repo": full_name,
        "ref": run.git_ref,
        "sha": run.sha,
        "run_id": run_id,
        "trigger": run.trigger,
        "trusted": run.trusted,
    });
    Ok(Json(serde_json::json!({ "token": issuer.sign(&claims)? })))
}

#[derive(Deserialize)]
struct TimingsBody {
    task: String,
    /// Per-file seconds, when the task's reports name files.
    #[serde(default)]
    times: HashMap<String, f64>,
    /// Otherwise the shard's total and its files: the total is split in
    /// proportion to what is known of each file, so a slow file sharing a
    /// shard with fast ones does not make them look slow.
    #[serde(default)]
    total: Option<f64>,
    #[serde(default)]
    files: Vec<String>,
}

/// `POST /api/timings`: per-file durations from a sharded task, for the
/// next split. Only from a running job, for its own repo.
async fn record_timings(
    State(app): State<Arc<App>>,
    headers: HeaderMap,
    Json(body): Json<TimingsBody>,
) -> ApiResult<StatusCode> {
    // Only for a task of the job's own group, keyed the way planning reads
    // it: two groups' `test` tasks are different tasks.
    let (repo_id, group, no_secrets) = lease(&headers)
        .map(|l| app.db.lease_task(l, &body.task))
        .transpose()?
        .flatten()
        .ok_or_else(|| {
            ApiError(
                StatusCode::CONFLICT,
                anyhow::anyhow!("no live job runs that task"),
            )
        })?;
    // A fork's numbers would steer the split of trusted runs.
    if no_secrets {
        return Ok(StatusCode::NO_CONTENT);
    }
    let task = super::shards::timing_key(&group, &body.task);
    let mut times = body.times;
    if let (Some(total), false) = (
        body.total.filter(|t| t.is_finite() && *t >= 0.0),
        body.files.is_empty(),
    ) {
        let known = app.db.shard_timings(&repo_id, &task)?;
        let average = if known.is_empty() {
            1.0
        } else {
            known.values().sum::<f64>() / known.len() as f64
        };
        let weight = |f: &String| known.get(f).copied().unwrap_or(average).max(0.001);
        let sum: f64 = body.files.iter().map(weight).sum();
        for f in body.files.iter().take(10_000) {
            times.insert(f.clone(), total * weight(f) / sum);
        }
    }
    let times: HashMap<String, f64> = times
        .into_iter()
        .filter(|(_, s)| s.is_finite() && *s >= 0.0)
        .take(10_000)
        .collect();
    app.db.record_shard_timings(&repo_id, &task, &times)?;
    Ok(StatusCode::NO_CONTENT)
}

#[derive(Deserialize)]
struct FlakyQuery {
    days: Option<i64>,
}

/// `GET /api/slow?days=14`: see `Db::slow_tasks`.
async fn slow(
    State(app): State<Arc<App>>,
    Query(q): Query<FlakyQuery>,
) -> ApiResult<Json<Vec<SlowTask>>> {
    let days = q.days.unwrap_or(14).clamp(1, 90);
    let since = crate::model::now_ms() - days * 86_400_000;
    let rows = tokio::task::spawn_blocking(move || app.db.slow_tasks(since))
        .await
        .map_err(anyhow::Error::from)??;
    Ok(Json(rows))
}

/// `GET /api/flaky?days=14`: see `Db::flaky_tasks`.
async fn flaky(
    State(app): State<Arc<App>>,
    Query(q): Query<FlakyQuery>,
) -> ApiResult<Json<Vec<FlakyTask>>> {
    let days = q.days.unwrap_or(14).clamp(1, 90);
    // A scan of weeks of history: keep it off the async threads that answer
    // workers' heartbeats.
    let since = crate::model::now_ms() - days * 86_400_000;
    let rows = tokio::task::spawn_blocking(move || app.db.flaky_tasks(since))
        .await
        .map_err(anyhow::Error::from)??;
    Ok(Json(rows))
}

#[derive(Deserialize)]
struct ClaimBody {
    task: String,
}

/// Single flight for cache misses. Shards that start together, or two runs
/// of the same commit, would each compute the same action. The first task to
/// claim a key computes it. Others are told to wait for its result, for as
/// long as that task is still running; if it fails or dies, the next claim
/// takes over. Keys are scoped by trust, since untrusted results are never
/// served to trusted runs (P20).
async fn cache_claim(
    State(app): State<Arc<App>>,
    Path(key): Path<String>,
    headers: HeaderMap,
    Json(body): Json<ClaimBody>,
) -> ApiResult<Json<serde_json::Value>> {
    let trusted = cache_trust(&app, &headers)?;
    let slot = format!("{}:{key}", u8::from(trusted));
    // Check and claim under one guard, so two claims cannot both win.
    let mut inflight = app.inflight.lock();
    // The result may have landed since the caller looked: go get it.
    if app.db.cache_find(&key, trusted)?.is_some() {
        return Ok(Json(serde_json::json!({ "wait": true })));
    }
    if let Some(holder) = inflight.get(&slot).filter(|h| **h != body.task) {
        if let Some(run) = app.db.running_task_run(holder)? {
            return Ok(Json(serde_json::json!({ "wait": true, "run": run })));
        }
    }
    inflight.insert(slot, body.task);
    Ok(Json(serde_json::json!({ "wait": false })))
}

/// The run where a task with these exact inputs last failed. Failures are
/// not cached, so this is how a rerun can say why it ran again.
async fn cache_failed(
    State(app): State<Arc<App>>,
    Path(key): Path<String>,
) -> ApiResult<Json<serde_json::Value>> {
    app.db
        .last_failure(&key)?
        .map(|run| Json(serde_json::json!({ "runId": run })))
        .ok_or_else(|| not_found("failure"))
}

/// `POST /api/gc`: collect now. Under the limit this only deletes blobs
/// nothing refers to.
async fn gc_now(State(app): State<Arc<App>>) -> ApiResult<Json<super::gc::GcReport>> {
    let report = tokio::task::spawn_blocking(move || super::gc::collect(&app, true)).await??;
    Ok(Json(report))
}

async fn cas_missing(
    State(app): State<Arc<App>>,
    Json(digests): Json<Vec<String>>,
) -> Json<Vec<String>> {
    Json(
        digests
            .into_iter()
            .filter(|d| !app.cas.contains(d))
            .collect(),
    )
}

async fn cas_get(State(app): State<Arc<App>>, Path(digest): Path<String>) -> ApiResult<Response> {
    let bytes = app.cas.get(&digest)?.ok_or_else(|| not_found("blob"))?;
    Ok(([(header::CONTENT_TYPE, "application/octet-stream")], bytes).into_response())
}

async fn cas_put(
    State(app): State<Arc<App>>,
    Path(digest): Path<String>,
    body: Bytes,
) -> ApiResult<StatusCode> {
    app.cas.put_checked(&digest, &body)?;
    Ok(StatusCode::NO_CONTENT)
}

async fn queue_list(State(app): State<Arc<App>>) -> ApiResult<Json<Vec<QueueEntryView>>> {
    Ok(Json(super::queue::list(&app)?))
}

async fn queue_add(
    State(app): State<Arc<App>>,
    _h: HeaderMap,
    Json(req): Json<QueueAdd>,
) -> ApiResult<Json<QueueEntryView>> {
    Ok(Json(super::queue::add(&app, req).await?))
}

#[derive(Deserialize)]
struct RepoQuery {
    repo: Option<String>,
}

/// `DELETE /api/queue/{id or branch}?repo=<name>`
async fn queue_remove(
    State(app): State<Arc<App>>,
    Path(key): Path<String>,
    Query(q): Query<RepoQuery>,
) -> ApiResult<Json<serde_json::Value>> {
    let n = super::queue::remove(&app, &key, q.repo.as_deref(), "removed by request")?;
    if n == 0 {
        return Err(not_found(format!("active queue entry {key}")));
    }
    Ok(Json(serde_json::json!({ "removed": n })))
}
