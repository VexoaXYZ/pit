//! GitHub integration: webhooks in, check runs (GitHub App) or commit statuses
//! (token) out. GitHub stays the source of truth; Pit replaces Actions.
//!
//! Configuration (environment):
//! - `PIT_GITHUB_APP_ID` + `PIT_GITHUB_PRIVATE_KEY` (PEM or a path to one): App mode.
//! - `PIT_GITHUB_TOKEN`: token mode, reports commit statuses.
//! - `PIT_GITHUB_WEBHOOK_SECRET`: required to accept webhooks.
//! - `PIT_GITHUB_API_URL`: defaults to https://api.github.com (GHES, tests).

use std::sync::Arc;
use std::time::Duration;

use anyhow::{bail, Context, Result};
use axum::body::Bytes;
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::Json;
use hmac::{Hmac, Mac};
use parking_lot::Mutex;
use serde::Deserialize;
use sha2::Sha256;

use super::App;
use crate::model::*;

pub const MERGE_LABEL: &str = "pit:merge";
/// A maintainer's go-ahead to run a pull request's current commit, for
/// pull requests from forks, which never run on their own.
pub const RUN_LABEL: &str = "pit:run";

enum Auth {
    App {
        app_id: String,
        key: jsonwebtoken::EncodingKey,
    },
    Token(String),
}

/// A response, or `RateLimited` if GitHub asked to wait (403/429 with
/// Retry-After), or the HTTP error.
fn limited_or_ok(resp: reqwest::Response) -> Result<reqwest::Response> {
    let status = resp.status();
    if matches!(status.as_u16(), 403 | 429) {
        if let Some(wait) = resp
            .headers()
            .get("retry-after")
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.parse::<u64>().ok())
        {
            return Err(RateLimited(wait).into());
        }
    }
    Ok(resp.error_for_status()?)
}

/// GitHub asked to wait this many seconds (a secondary rate limit).
#[derive(Debug)]
struct RateLimited(u64);

impl std::fmt::Display for RateLimited {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "GitHub rate limit: retry in {}s", self.0)
    }
}

impl std::error::Error for RateLimited {}

pub struct GitHub {
    api: String,
    auth: Auth,
    webhook_secret: Option<String>,
    http: reqwest::Client,
    /// Installation tokens by repo full name, with expiry (ms).
    tokens: Mutex<std::collections::HashMap<String, (String, i64)>>,
    /// What each group's check last said, so a report that would post the
    /// same again is skipped: re-posting every group on every event is
    /// O(groups²) calls per run, and GitHub's secondary rate limits stop it.
    reported: Mutex<std::collections::HashMap<String, u64>>,
    /// Left-out copies (`on`) posted per commit and group name: a real
    /// group's check must be posted again after one.
    left_out_posts: Mutex<std::collections::HashMap<String, u64>>,
    /// One report of a run at a time (claims report concurrently), so two
    /// reports cannot both see a stale `reported` and post the same twice.
    run_locks: Mutex<std::collections::HashMap<String, std::sync::Arc<tokio::sync::Mutex<()>>>>,
    /// Runs with a report waiting out a rate limit.
    retrying: Mutex<std::collections::HashSet<String>>,
}

impl GitHub {
    pub fn from_env() -> Result<Option<GitHub>> {
        let api =
            std::env::var("PIT_GITHUB_API_URL").unwrap_or_else(|_| "https://api.github.com".into());
        let webhook_secret = std::env::var("PIT_GITHUB_WEBHOOK_SECRET")
            .ok()
            .filter(|s| !s.is_empty());
        let auth = if let Ok(app_id) = std::env::var("PIT_GITHUB_APP_ID") {
            let raw = std::env::var("PIT_GITHUB_PRIVATE_KEY")
                .context("PIT_GITHUB_APP_ID needs PIT_GITHUB_PRIVATE_KEY")?;
            let pem = if raw.contains("BEGIN") {
                raw
            } else {
                std::fs::read_to_string(&raw).with_context(|| format!("reading {raw}"))?
            };
            Auth::App {
                app_id,
                key: jsonwebtoken::EncodingKey::from_rsa_pem(pem.as_bytes())?,
            }
        } else if let Ok(token) = std::env::var("PIT_GITHUB_TOKEN") {
            Auth::Token(token)
        } else {
            return Ok(None);
        };
        Ok(Some(GitHub {
            api: api.trim_end_matches('/').to_string(),
            auth,
            webhook_secret,
            http: reqwest::Client::builder()
                .timeout(Duration::from_secs(20))
                .user_agent("pit-ci")
                .build()?,
            tokens: Default::default(),
            reported: Default::default(),
            left_out_posts: Default::default(),
            run_locks: Default::default(),
            retrying: Default::default(),
        }))
    }

    /// App mode with explicit settings (GHES, tests).
    pub fn with_app(api: &str, app_id: &str, pem: &str) -> Result<GitHub> {
        Ok(GitHub {
            api: api.trim_end_matches('/').to_string(),
            auth: Auth::App {
                app_id: app_id.to_string(),
                key: jsonwebtoken::EncodingKey::from_rsa_pem(pem.as_bytes())?,
            },
            webhook_secret: None,
            http: reqwest::Client::builder()
                .timeout(Duration::from_secs(20))
                .user_agent("pit-ci")
                .build()?,
            tokens: Default::default(),
            reported: Default::default(),
            left_out_posts: Default::default(),
            run_locks: Default::default(),
            retrying: Default::default(),
        })
    }

    /// Token mode with explicit settings (GHES, tests).
    pub fn with_token(api: &str, token: &str, webhook_secret: Option<String>) -> Result<GitHub> {
        Ok(GitHub {
            api: api.trim_end_matches('/').to_string(),
            auth: Auth::Token(token.to_string()),
            webhook_secret,
            http: reqwest::Client::builder()
                .timeout(Duration::from_secs(20))
                .user_agent("pit-ci")
                .build()?,
            tokens: Default::default(),
            reported: Default::default(),
            left_out_posts: Default::default(),
            run_locks: Default::default(),
            retrying: Default::default(),
        })
    }

    fn app_jwt(&self) -> Result<String> {
        let Auth::App { app_id, key } = &self.auth else {
            bail!("not an app")
        };
        let now = now_ms() / 1000;
        let claims = serde_json::json!({ "iat": now - 60, "exp": now + 540, "iss": app_id });
        Ok(jsonwebtoken::encode(
            &jsonwebtoken::Header::new(jsonwebtoken::Algorithm::RS256),
            &claims,
            key,
        )?)
    }

    async fn token_for(&self, full_name: &str) -> Result<String> {
        match &self.auth {
            Auth::Token(t) => Ok(t.clone()),
            Auth::App { .. } => {
                if let Some((tok, exp)) = self.tokens.lock().get(full_name).cloned() {
                    if exp - now_ms() > 120_000 {
                        return Ok(tok);
                    }
                }
                let jwt = self.app_jwt()?;
                let inst: serde_json::Value = self
                    .http
                    .get(format!("{}/repos/{full_name}/installation", self.api))
                    .bearer_auth(&jwt)
                    .header("accept", "application/vnd.github+json")
                    .send()
                    .await
                    .map(limited_or_ok)??
                    .json()
                    .await?;
                let id = inst["id"].as_u64().context("installation id")?;
                let tok: serde_json::Value = self
                    .http
                    .post(format!("{}/app/installations/{id}/access_tokens", self.api))
                    .bearer_auth(&jwt)
                    .header("accept", "application/vnd.github+json")
                    .send()
                    .await
                    .map(limited_or_ok)??
                    .json()
                    .await?;
                let token = tok["token"]
                    .as_str()
                    .context("installation token")?
                    .to_string();
                // Installation tokens last an hour.
                self.tokens
                    .lock()
                    .insert(full_name.to_string(), (token.clone(), now_ms() + 3_300_000));
                Ok(token)
            }
        }
    }

    async fn call(
        &self,
        full_name: &str,
        method: reqwest::Method,
        path: &str,
        body: serde_json::Value,
    ) -> Result<serde_json::Value> {
        let token = self.token_for(full_name).await?;
        let req = self
            .http
            .request(method, format!("{}{path}", self.api))
            .bearer_auth(token)
            .header("accept", "application/vnd.github+json");
        // A GET carries no body.
        let req = if body.is_null() { req } else { req.json(&body) };
        let resp = req.send().await?;
        let status = resp.status();
        let retry_after = resp
            .headers()
            .get("retry-after")
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.parse::<u64>().ok());
        let text = resp.text().await.unwrap_or_default();
        // GitHub's secondary limits (80 content-creating requests a minute):
        // a 403 or 429 that says so, or carries Retry-After.
        let limited = matches!(status.as_u16(), 403 | 429)
            && (retry_after.is_some() || text.to_ascii_lowercase().contains("rate limit"));
        if limited {
            return Err(RateLimited(retry_after.unwrap_or(60)).into());
        }
        if !status.is_success() {
            bail!("GitHub {path} returned {status}: {text}");
        }
        Ok(serde_json::from_str(&text).unwrap_or(serde_json::Value::Null))
    }

    /// An authenticated https remote for pushing merge queue results.
    pub async fn push_url(&self, full_name: &str) -> Result<String> {
        let token = self.token_for(full_name).await?;
        let host = if self.api == "https://api.github.com" {
            "github.com".to_string()
        } else {
            self.api
                .trim_start_matches("https://")
                .trim_start_matches("http://")
                .split('/')
                .next()
                .unwrap_or("github.com")
                .to_string()
        };
        Ok(format!(
            "https://x-access-token:{token}@{host}/{full_name}.git"
        ))
    }

    /// Mirrors a run's groups to GitHub. Errors are logged, never fatal.
    pub async fn report_run(&self, app: &Arc<App>, run_id: &str) {
        if let Some(wait) = self.report_once(app, run_id).await {
            self.schedule_retry(app, run_id, wait);
        }
    }

    /// Rate limited: report the run again later (the latest state, whatever
    /// it is by then), once per run, without holding up the caller. Jittered,
    /// so runs limited together do not all come back at once.
    fn schedule_retry(&self, app: &Arc<App>, run_id: &str, wait: u64) {
        if !self.retrying.lock().insert(run_id.to_string()) {
            return;
        }
        let (app, run_id) = (app.clone(), run_id.to_string());
        tokio::spawn(async move {
            let mut wait = wait;
            loop {
                let jitter = (uuid::Uuid::new_v4().as_u128() as u64) % (wait / 4 + 2);
                tokio::time::sleep(Duration::from_secs(wait.clamp(1, 300) + jitter)).await;
                let Some(gh) = &app.github else { return };
                match gh.report_once(&app, &run_id).await {
                    Some(again) => wait = again,
                    None => {
                        gh.retrying.lock().remove(&run_id);
                        return;
                    }
                }
            }
        });
    }

    /// One report of the run; `Some(seconds)` if GitHub asked to wait.
    async fn report_once(&self, app: &Arc<App>, run_id: &str) -> Option<u64> {
        let lock = {
            let mut locks = self.run_locks.lock();
            if locks.len() > 10_000 {
                locks.retain(|_, l| std::sync::Arc::strong_count(l) > 1);
            }
            locks.entry(run_id.to_string()).or_default().clone()
        };
        let _one_at_a_time = lock.lock().await;
        match self.try_report_run(app, run_id).await {
            Ok(()) => None,
            Err(e) => match e.downcast_ref::<RateLimited>() {
                Some(RateLimited(wait)) => {
                    tracing::warn!("GitHub rate limit reporting {run_id}; retrying in {wait}s");
                    Some(*wait)
                }
                None => {
                    tracing::warn!("reporting {run_id} to GitHub failed: {e:#}");
                    None
                }
            },
        }
    }

    async fn try_report_run(&self, app: &Arc<App>, run_id: &str) -> Result<()> {
        let run = app.db.run(run_id)?;
        // Local overlay runs are not commits on GitHub.
        if run.overlay.is_some() {
            return Ok(());
        }
        let repo = app.db.repo(&run.repo_id)?;
        let Some(full) = repo.github.clone() else {
            return Ok(());
        };
        let view = app.db.run_view(run_id)?;
        let details = format!("{}/runs/{run_id}", app.public_url);
        // Skipped on purpose (`[skip ci]`, a draft with `drafts: false`):
        // nothing, as GitHub posts nothing for a workflow it skips. A
        // "skipped" or "failure" check would show a red X on a release bot's
        // commit, and a passing one could satisfy a required check.
        if view.groups.is_empty() && view.status == Status::Skipped {
            return Ok(());
        }
        if view.groups.is_empty() && view.status.is_done() {
            // Planning failed: one check explaining why.
            return self
                .report_one(
                    app,
                    &full,
                    &run.sha,
                    None,
                    "plan",
                    view.status,
                    view.message.as_deref().unwrap_or(""),
                    &details,
                    &[],
                )
                .await;
        }
        for g in &view.groups {
            let summary = summarize(g, run_id, run.no_secrets);
            // Left out of this kind of run (`on`): reported "skipped", which
            // GitHub counts as passing for a required check, as for a job an
            // `if:` skips. Unless another run of this commit really runs that
            // group: its check is the one that counts, and must not be
            // overwritten.
            let left_out = app.db.group(&g.id).map(|r| r.left_out).unwrap_or(false);
            let real_elsewhere = |app: &Arc<App>| {
                app.db
                    .real_group_on_commit(&run.repo_id, &run.sha, &g.name, run_id)
            };
            if left_out && real_elsewhere(app)? {
                continue;
            }
            let status = if left_out { Status::Skipped } else { g.status };
            let annotations = if g.status.is_done() {
                app.db.group_annotations(&g.id)?
            } else {
                vec![]
            };
            let copies_key = format!("{}\0{}\0{}", run.repo_id, run.sha, g.name);
            let copies = self
                .left_out_posts
                .lock()
                .get(&copies_key)
                .copied()
                .unwrap_or(0);
            // A newer left-out check run (App auth) must be posted over.
            let newest_copy = if left_out {
                None
            } else {
                app.db
                    .newest_left_out_check(&run.repo_id, &run.sha, &g.name)?
            };
            let said = {
                use std::hash::{Hash, Hasher};
                let mut h = std::collections::hash_map::DefaultHasher::new();
                // What GitHub is actually sent: a commit status carries the
                // state and the summary's first line only.
                let shown = match self.auth {
                    Auth::Token(_) => summary.lines().next().unwrap_or_default(),
                    Auth::App { .. } => summary.as_str(),
                };
                (
                    status.as_str(),
                    shown,
                    left_out,
                    annotations.len(),
                    copies,
                    newest_copy,
                )
                    .hash(&mut h);
                h.finish()
            };
            if self.reported.lock().get(&g.id) == Some(&said) {
                continue;
            }
            self.report_one(
                app,
                &full,
                &run.sha,
                Some(&g.id),
                &g.name,
                status,
                &summary,
                &details,
                &annotations,
            )
            .await?;
            {
                let mut reported = self.reported.lock();
                if reported.len() > 50_000 {
                    reported.clear();
                    // Losing this is like a restart: reposts happen anyway.
                    self.left_out_posts.lock().clear();
                }
                reported.insert(g.id.clone(), said);
            }
            if left_out {
                *self.left_out_posts.lock().entry(copies_key).or_default() += 1;
            }
            // A run that really runs the group may have appeared while this
            // left-out copy was being posted: report it again, so its check
            // is the newest (it posts a new check run over this one).
            if left_out && real_elsewhere(app)? {
                for other in app.db.real_group_runs(&run.repo_id, &run.sha, &g.name)? {
                    // Not under `other`'s run lock: it may be reporting
                    // this run the same way, and would wait for ours. If the
                    // repost is rate limited, `other` is retried, not us.
                    match Box::pin(self.try_report_run(app, &other)).await {
                        Ok(()) => {}
                        Err(e) => match e.downcast_ref::<RateLimited>() {
                            Some(RateLimited(wait)) => self.schedule_retry(app, &other, *wait),
                            None => return Err(e),
                        },
                    }
                }
            }
        }
        // A sharded group's checks are named by shard (`pit / test#1` …), so
        // changing `shards` renames them and a branch rule requiring them
        // waits forever. One more check, `pit / test`, keeps a stable name:
        // pending while any shard runs, passed only when all passed.
        let mut sharded: std::collections::BTreeMap<&str, Vec<&GroupView>> = Default::default();
        for g in &view.groups {
            if let Some((base, k)) = g.name.rsplit_once('#') {
                if !base.is_empty() && k.parse::<u32>().is_ok() {
                    sharded.entry(base).or_default().push(g);
                }
            }
        }
        for (base, shards) in sharded {
            let left_out = |g: &GroupView| app.db.group(&g.id).map(|r| r.left_out).unwrap_or(false);
            // Left out of this kind of run: each shard already says so.
            if shards.iter().all(|g| left_out(g)) {
                continue;
            }
            let done = shards.iter().filter(|g| g.status.is_done()).count();
            let passed = shards.iter().filter(|g| g.status.is_ok()).count();
            let status = if done < shards.len() {
                Status::Running
            } else if passed == shards.len() {
                Status::Passed
            } else {
                Status::Failed
            };
            let summary = format!("{passed} of {} shards passed", shards.len());
            let key = format!("shards\0{run_id}\0{base}");
            let said = {
                use std::hash::{Hash, Hasher};
                let mut h = std::collections::hash_map::DefaultHasher::new();
                (status.as_str(), summary.as_str()).hash(&mut h);
                h.finish()
            };
            if self.reported.lock().get(&key) == Some(&said) {
                continue;
            }
            self.report_one(
                app,
                &full,
                &run.sha,
                None,
                base,
                status,
                &summary,
                &details,
                &[],
            )
            .await?;
            self.reported.lock().insert(key, said);
        }
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    async fn report_one(
        &self,
        app: &Arc<App>,
        full: &str,
        sha: &str,
        group_id: Option<&str>,
        name: &str,
        status: Status,
        summary: &str,
        details: &str,
        annotations: &[Annotation],
    ) -> Result<()> {
        let check_name = format!("pit / {name}");
        let not_in_run = status == Status::Skipped
            && group_id
                .and_then(|g| app.db.group(g).ok())
                .is_some_and(|g| g.left_out);
        match &self.auth {
            Auth::Token(_) => {
                let state = match status {
                    Status::Queued | Status::Running => "pending",
                    s if s.is_ok() || not_in_run => "success",
                    Status::Error => "error",
                    _ => "failure",
                };
                let description: String = summary
                    .lines()
                    .next()
                    .unwrap_or("")
                    .chars()
                    .take(138)
                    .collect();
                self.call(
                    full,
                    reqwest::Method::POST,
                    &format!("/repos/{full}/statuses/{sha}"),
                    serde_json::json!({ "state": state, "context": check_name, "description": description, "target_url": details }),
                )
                .await?;
            }
            Auth::App { .. } => {
                let mut body = serde_json::json!({
                    "name": check_name,
                    "head_sha": sha,
                    "details_url": details,
                    "external_id": group_id,
                    "output": { "title": format!("{name}: {}", status.as_str()), "summary": summary },
                });
                match status {
                    Status::Queued => body["status"] = "queued".into(),
                    Status::Running => body["status"] = "in_progress".into(),
                    s => {
                        body["status"] = "completed".into();
                        body["conclusion"] = match s {
                            Status::Passed | Status::Cached => "success",
                            Status::Skipped if not_in_run => "skipped",
                            Status::Skipped => "cancelled",
                            _ => "failure",
                        }
                        .into();
                    }
                }
                // Failed tests show on the pull request's diff, once: GitHub
                // appends a check's annotations on every update, at most 50
                // per request.
                let annotate = match group_id {
                    Some(g) if status.is_done() && !annotations.is_empty() => {
                        app.db.kv_claim("annotated", g)?
                    }
                    _ => false,
                };
                let mut chunks = annotations.chunks(50).map(|c| {
                    c.iter()
                        .map(|a| {
                            serde_json::json!({
                                "path": a.path, "start_line": a.line, "end_line": a.line,
                                "annotation_level": "failure", "title": a.title, "message": a.message,
                            })
                        })
                        .collect::<Vec<_>>()
                });
                if annotate {
                    body["output"]["annotations"] = chunks.next().unwrap_or_default().into();
                }
                let row = group_id.and_then(|g| app.db.group(g).ok());
                // GitHub takes the newest check run of a name as the check. If
                // a left-out copy of this group (another run of the commit,
                // `on`) posted one after ours, ours must be posted anew to
                // count again.
                let newer_left_out = match &row {
                    Some(r) if !r.left_out => {
                        let run = app.db.run(&r.run_id)?;
                        app.db
                            .newest_left_out_check(&run.repo_id, sha, &r.name)?
                            .is_some_and(|l| r.check_run_id.is_none_or(|own| l > own))
                    }
                    _ => false,
                };
                let existing = row.and_then(|g| g.check_run_id).filter(|_| !newer_left_out);
                let sent = match existing {
                    Some(id) => self
                        .call(
                            full,
                            reqwest::Method::PATCH,
                            &format!("/repos/{full}/check-runs/{id}"),
                            body.clone(),
                        )
                        .await
                        .map(|_| Some(id)),
                    None => self
                        .call(
                            full,
                            reqwest::Method::POST,
                            &format!("/repos/{full}/check-runs"),
                            body.clone(),
                        )
                        .await
                        .map(|created| created["id"].as_u64()),
                };
                // The claim is only spent once GitHub has the first chunk; a
                // failed request leaves them for the next report.
                let id = match sent {
                    Ok(id) => id,
                    Err(e) => {
                        if let (true, Some(g)) = (annotate, group_id) {
                            app.db.kv_delete("annotated", g)?;
                        }
                        return Err(e);
                    }
                };
                if let (None, Some(g), Some(id)) = (existing, group_id, id) {
                    app.db.set_check_run(g, id)?;
                }
                if let (true, Some(id)) = (annotate, id) {
                    for chunk in chunks {
                        let sent = self
                            .call(
                                full,
                                reqwest::Method::PATCH,
                                &format!("/repos/{full}/check-runs/{id}"),
                                serde_json::json!({ "output": {
                                    "title": body["output"]["title"], "summary": summary, "annotations": chunk,
                                } }),
                            )
                            .await;
                        // Not all were sent: let the next report send them
                        // (repeating some is better than losing the rest).
                        if let Err(e) = sent {
                            if let Some(g) = group_id {
                                app.db.kv_delete("annotated", g)?;
                            }
                            return Err(e);
                        }
                    }
                }
            }
        }
        Ok(())
    }

    pub fn verify_signature(&self, body: &[u8], signature: Option<&str>) -> bool {
        let Some(secret) = &self.webhook_secret else {
            return false;
        };
        let Some(sig) = signature.and_then(|s| s.strip_prefix("sha256=")) else {
            return false;
        };
        let Ok(expected) = hex::decode(sig) else {
            return false;
        };
        let mut mac =
            Hmac::<Sha256>::new_from_slice(secret.as_bytes()).expect("hmac accepts any key");
        mac.update(body);
        mac.verify_slice(&expected).is_ok()
    }
}

fn summarize(g: &GroupView, run_id: &str, untrusted: bool) -> String {
    let done = g.tasks.iter().filter(|t| t.status.is_done()).count();
    let cached = g
        .tasks
        .iter()
        .filter(|t| t.status == Status::Cached)
        .count();
    let mut s = format!("{done}/{} tasks done, {cached} from cache", g.tasks.len());
    if let Some(ms) = g.prepare_ms {
        s.push_str(&format!(", workspace ready in {ms}ms"));
    }
    s.push_str("\n\n| Task | Status | Time | Note |\n| --- | --- | --- | --- |\n");
    for t in &g.tasks {
        let time = t
            .duration_ms
            .map(|d| format!("{:.1}s", d as f64 / 1000.0))
            .unwrap_or_default();
        s.push_str(&format!(
            "| {} | {} | {} | {} |\n",
            heading_name(&t.name).trim_matches('`').replace('|', "\\|"),
            t.status.as_str(),
            time,
            note(t)
        ));
    }
    // What tasks wrote to `$PIT_SUMMARY` / `$GITHUB_STEP_SUMMARY`. A fork's
    // markdown goes in a fence: shown, but unable to pose as Pit's own text
    // or swallow what follows.
    for t in &g.tasks {
        let Some(md) = t.summary.as_deref().filter(|m| !m.trim().is_empty()) else {
            continue;
        };
        let name = heading_name(&t.name);
        let body = if untrusted {
            format!(
                "~~~~~~~~text\n{}\n~~~~~~~~",
                md.trim_end().replace("~~~~~~~~", "~~~~ ~~~~")
            )
        } else {
            md.trim_end().to_string()
        };
        let block = format!("\n### {name}\n\n{body}\n");
        let note = format!(
            "\n### {name}\n\n_(summary too long for the check: `GET /api/runs/{run_id}` on the Pit server has it)_\n"
        );
        let room = CHECK_SUMMARY_ROOM.saturating_sub(s.len());
        if block.len() <= room {
            s.push_str(&block);
        } else if note.len() <= room {
            s.push_str(&note);
        }
    }
    // GitHub rejects an update whose summary is over 65535 characters.
    if s.len() > CHECK_SUMMARY_ROOM + 4_000 {
        let mut cut = CHECK_SUMMARY_ROOM + 4_000;
        while !s.is_char_boundary(cut) {
            cut -= 1;
        }
        s.truncate(cut);
    }
    s
}

/// A task name on one line, as plain text: names are config keys, and a
/// fork's could otherwise break out of the table or the summary fence.
fn heading_name(name: &str) -> String {
    let flat: String = name
        .chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .take(100)
        .collect();
    format!("`{}`", flat.replace('`', "'"))
}

/// Room for task summaries in a check's summary (GitHub's limit is 65535).
const CHECK_SUMMARY_ROOM: usize = 60_000;

/// What a check's table says about a task: why it failed, or that a pass was
/// flaky or much slower than usual.
fn note(t: &crate::model::TaskView) -> String {
    let m = t.message.as_deref().unwrap_or_default();
    let text = if matches!(t.status, Status::Failed | Status::Error) {
        m.to_string()
    } else {
        m.split(" · ")
            .filter(|p| p.starts_with("slow:") || p.starts_with("flaky:"))
            .collect::<Vec<_>>()
            .join("; ")
    };
    let line = text.lines().next().unwrap_or_default();
    line.chars()
        .take(200)
        .collect::<String>()
        .replace('|', "\\|")
}

#[derive(Deserialize)]
struct RepoPayload {
    full_name: String,
    clone_url: String,
    #[serde(default)]
    default_branch: Option<String>,
}

/// `POST /webhooks/github`
pub async fn webhook(
    State(app): State<Arc<App>>,
    headers: HeaderMap,
    body: Bytes,
) -> (StatusCode, Json<serde_json::Value>) {
    let Some(gh) = &app.github else {
        return (
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({ "error": "GitHub integration is not configured" })),
        );
    };
    let sig = headers
        .get("x-hub-signature-256")
        .and_then(|v| v.to_str().ok());
    if !gh.verify_signature(&body, sig) {
        return (
            StatusCode::UNAUTHORIZED,
            Json(
                serde_json::json!({ "error": "bad or missing signature (set PIT_GITHUB_WEBHOOK_SECRET)" }),
            ),
        );
    }
    let event = headers
        .get("x-github-event")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    match handle_event(&app, event, &body).await {
        Ok(v) => (StatusCode::ACCEPTED, Json(v)),
        Err(e) => (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({ "error": format!("{e:#}") })),
        ),
    }
}

async fn handle_event(app: &Arc<App>, event: &str, body: &[u8]) -> Result<serde_json::Value> {
    let payload: serde_json::Value = serde_json::from_slice(body)?;
    let repo_payload =
        || -> Result<RepoPayload> { Ok(serde_json::from_value(payload["repository"].clone())?) };
    match event {
        "ping" => Ok(serde_json::json!({ "ok": true })),
        "push" => {
            let r = repo_payload()?;
            let after = payload["after"].as_str().unwrap_or_default();
            if payload["deleted"].as_bool() == Some(true) || after.chars().all(|c| c == '0') {
                return Ok(serde_json::json!({ "ignored": "branch deleted" }));
            }
            let git_ref = payload["ref"]
                .as_str()
                .map(|r| r.trim_start_matches("refs/heads/").to_string());
            let repo = register(app, &r)?;
            let id = app
                .create_run(
                    CreateRun {
                        repo: repo.name,
                        url: None,
                        sha: after.to_string(),
                        overlay: None,
                        tasks: vec![],
                        trigger: Some("push".into()),
                        trusted: git_ref
                            .as_deref()
                            .is_some_and(|b| super::trusted_branch(b, r.default_branch.as_deref())),
                        git_ref,
                        no_cache: false,
                        fresh: false,
                        debug: false,
                        counts_as: None,
                        no_secrets: false,
                        draft: false,
                    },
                    None,
                )
                .await?;
            Ok(serde_json::json!({ "run": id }))
        }
        "pull_request" => {
            let action = payload["action"].as_str().unwrap_or_default();
            let r = repo_payload()?;
            let pr = &payload["pull_request"];
            let number = pr["number"].as_u64().context("pull_request.number")?;
            let sha = pr["head"]["sha"]
                .as_str()
                .context("pull_request.head.sha")?
                .to_string();
            let repo = register(app, &r)?;
            let fork = pr["head"]["repo"]["full_name"].as_str() != Some(r.full_name.as_str());
            // Remembered so a pull request queued through the API is pinned
            // unless it is known to come from the repo itself.
            app.db.kv_put(
                "pr-origin",
                &format!("{}/{number}", repo.id),
                if fork { "fork" } else { "same" },
            )?;
            if action == "labeled" && payload["label"]["name"].as_str() == Some(MERGE_LABEL) {
                let entry = super::queue::add(
                    app,
                    QueueAdd {
                        repo: repo.name,
                        url: None,
                        branch: None,
                        pr: Some(number),
                        target: pr["base"]["ref"].as_str().map(String::from),
                        // A fork's label approves the commit it was added
                        // on, not whatever the fork pushes next.
                        sha: fork.then(|| sha.clone()),
                    },
                    false,
                )
                .await?;
                return Ok(serde_json::json!({ "queued": entry.id }));
            }
            // The label approves the commit it was added on. A fork's later
            // pushes need it again, so nobody can swap in new code after a
            // maintainer looked.
            if action == "labeled" && payload["label"]["name"].as_str() == Some(RUN_LABEL) {
                let id = app
                    .create_run(
                        CreateRun {
                            repo: repo.name,
                            url: None,
                            sha,
                            overlay: None,
                            tasks: vec![],
                            trigger: Some("pr".into()),
                            git_ref: Some(format!("pull/{number}")),
                            no_cache: false,
                            // Fork code gets a throwaway workspace and no secrets.
                            fresh: fork,
                            debug: false,
                            counts_as: None,
                            no_secrets: fork,
                            trusted: false,
                            draft: false,
                        },
                        Some(number),
                    )
                    .await?;
                return Ok(serde_json::json!({ "run": id }));
            }
            let unlabeled =
                action == "unlabeled" && payload["label"]["name"].as_str() == Some(MERGE_LABEL);
            if unlabeled || action == "closed" {
                let reason = if unlabeled {
                    "merge label removed"
                } else {
                    "pull request closed"
                };
                let n =
                    super::queue::remove(app, &format!("pr/{number}"), Some(&repo.name), reason)?;
                return Ok(serde_json::json!({ "dequeued": n }));
            }
            // Back to draft, with `drafts: false`: the PR's runs still in
            // progress stop, as its next pushes would be skipped. The config
            // comes from the commit Pit last ran for the PR, already in the
            // mirror; without that setting nothing changes.
            if action == "converted_to_draft" {
                let Some(last) = app.db.latest_pr_run(&repo.id, number)? else {
                    return Ok(serde_json::json!({ "ignored": action }));
                };
                let sandboxed = fork || app.config_sandboxed(&repo, false);
                let skips = app
                    .load_config(&app.mirror_dir(&repo.id), &last.sha, None, sandboxed)
                    .await
                    .is_ok_and(|c| c.drafts == Some(false));
                if !skips {
                    return Ok(serde_json::json!({ "ignored": action }));
                }
                let mut cancelled = 0;
                for id in app
                    .db
                    .superseded_runs(&repo.id, "pr", &format!("pull/{number}"), "")?
                {
                    if app
                        .db
                        .cancel_run_because(&id, "pull request converted to draft")?
                    {
                        app.run_finished(&id).await;
                        cancelled += 1;
                    }
                }
                return Ok(serde_json::json!({ "cancelled": cancelled }));
            }
            // `ready_for_review`: a draft marked ready runs then, if the
            // config skips drafts (`drafts: false`).
            if !matches!(
                action,
                "opened" | "synchronize" | "reopened" | "ready_for_review"
            ) {
                return Ok(serde_json::json!({ "ignored": action }));
            }
            let draft = pr["draft"].as_bool().unwrap_or(false);
            // A fork's code (including its pit.config.ts) is untrusted: Pit does
            // not run it on its own. A maintainer runs a commit with the run
            // label, or queues it with the merge label. A deleted fork shows up
            // as a missing head repo.
            if fork {
                return Ok(serde_json::json!({
                    "ignored": format!("pull request from a fork: add the {RUN_LABEL} label to run this commit")
                }));
            }
            let id = app
                .create_run(
                    CreateRun {
                        repo: repo.name,
                        url: None,
                        sha,
                        overlay: None,
                        tasks: vec![],
                        trigger: Some("pr".into()),
                        git_ref: Some(format!("pull/{number}")),
                        no_cache: false,
                        fresh: false,
                        debug: false,
                        counts_as: None,
                        no_secrets: false,
                        trusted: false,
                        draft,
                    },
                    Some(number),
                )
                .await?;
            Ok(serde_json::json!({ "run": id }))
        }
        "check_run" | "check_suite" if payload["action"].as_str() == Some("rerequested") => {
            let r = repo_payload()?;
            let sha = payload[event]["head_sha"]
                .as_str()
                .context("head_sha")?
                .to_string();
            let repo = register(app, &r)?;
            // GitHub shows a Re-run button on every check Pit posts, fork
            // commits included. The rerun keeps the restrictions the commit
            // ran under; a commit Pit never ran is not Pit's to run.
            let Some(prior) = app.db.prior_run(&repo.id, &sha)? else {
                return Ok(serde_json::json!({ "ignored": "Pit has not run this commit" }));
            };
            // The run whose check was re-run (Pit puts the group in the
            // check's `external_id`): the rerun repeats it, ref and kind
            // included. Restrictions still come from `prior`.
            let owner = payload["check_run"]["external_id"]
                .as_str()
                .and_then(|g| app.db.group(g).ok())
                .and_then(|g| app.db.run(&g.run_id).ok())
                .filter(|r| r.repo_id == repo.id && r.sha == sha);
            // "Re-run all checks" (a check suite) names no check: it counts as
            // every kind this commit ran as, so no group it showed is left out.
            let suite = owner.is_none();
            let shape = owner.unwrap_or_else(|| prior.clone());
            let counts_as = Some(if suite {
                app.db.commit_kinds(&repo.id, &sha)?
            } else {
                shape.kinds.clone().unwrap_or_else(|| {
                    super::run_kinds(&shape.trigger, shape.trusted, shape.git_ref.as_deref())
                        .into_iter()
                        .map(String::from)
                        .collect()
                })
            });
            let id = app
                .create_run(
                    CreateRun {
                        repo: repo.name,
                        url: None,
                        sha,
                        overlay: None,
                        tasks: vec![],
                        trigger: Some("rerun".into()),
                        git_ref: shape.git_ref,
                        no_cache: false,
                        fresh: prior.fresh || prior.no_secrets,
                        debug: false,
                        counts_as,
                        no_secrets: prior.no_secrets,
                        trusted: false,
                        draft: false,
                    },
                    shape.github_pr,
                )
                .await?;
            Ok(serde_json::json!({ "run": id }))
        }
        // `/retest` in a pull request comment, the Prow convention: reruns the
        // pull request's last commit Pit ran. Passed tasks come back from the
        // cache, so only what failed runs again.
        "issue_comment" if payload["action"].as_str() == Some("created") => {
            let comment = &payload["comment"];
            let body = comment["body"].as_str().unwrap_or_default().trim();
            let first = body.lines().next().unwrap_or_default().trim();
            if !matches!(first, "/retest" | "/pit retest") {
                return Ok(serde_json::json!({ "ignored": "not a command" }));
            }
            let Some(number) = payload["issue"]["pull_request"]
                .is_object()
                .then(|| payload["issue"]["number"].as_u64())
                .flatten()
            else {
                return Ok(serde_json::json!({ "ignored": "not a pull request" }));
            };
            // Owners, organisation members and collaborators (read-only ones
            // included), not drive-by accounts. A rerun only repeats a commit
            // Pit already ran, with the restrictions it ran under.
            let who = comment["author_association"].as_str().unwrap_or_default();
            if !matches!(who, "OWNER" | "MEMBER" | "COLLABORATOR") {
                return Ok(serde_json::json!({
                    "ignored": format!("/retest needs a collaborator, not {who}")
                }));
            }
            let r = repo_payload()?;
            let repo = register(app, &r)?;
            let Some(prior) = app.db.latest_pr_run(&repo.id, number)? else {
                return Ok(serde_json::json!({ "ignored": "Pit has not run this pull request" }));
            };
            // The same commit, counted as a pull request run, with the
            // restrictions it ran under (a fork's commit stays a fork's; its
            // next push still needs the label).
            let counts_as = Some(vec!["pr".to_string()]);
            let prior = app.db.prior_run(&repo.id, &prior.sha)?.unwrap_or(prior);
            let id = app
                .create_run(
                    CreateRun {
                        repo: repo.name,
                        url: None,
                        sha: prior.sha,
                        overlay: None,
                        tasks: vec![],
                        trigger: Some("retest".into()),
                        git_ref: prior.git_ref,
                        no_cache: false,
                        fresh: prior.fresh || prior.no_secrets,
                        debug: false,
                        counts_as,
                        no_secrets: prior.no_secrets,
                        trusted: false,
                        draft: false,
                    },
                    Some(number),
                )
                .await?;
            // A reaction says the command was heard; best effort.
            if let (Some(gh), Some(cid)) = (&app.github, comment["id"].as_u64()) {
                let _ = gh
                    .call(
                        &r.full_name,
                        reqwest::Method::POST,
                        &format!("/repos/{}/issues/comments/{cid}/reactions", r.full_name),
                        serde_json::json!({ "content": "rocket" }),
                    )
                    .await;
            }
            Ok(serde_json::json!({ "run": id }))
        }
        other => Ok(serde_json::json!({ "ignored": other })),
    }
}

/// Pushes and pull requests whose webhook never arrived (the server was down
/// or unreachable, and GitHub does not redeliver): for each GitHub repo Pit
/// has run before, the default branch's head (if committed in the last three
/// days) and the head of each open, same-repo pull request updated in the
/// last three days get a run if they have none, as their webhook would have
/// started. Forks still wait for the label; drafts and skip markers apply as
/// usual. Returns how many runs.
///
/// `at_start`: the first pass after the server starts, which acts at once
/// (the down-time case). A later pass only starts a head that was already
/// missing on the pass before: GitHub's API often shows a new head before
/// its webhook is handled, and a live webhook must not race this into a
/// second run (on the default branch, runs are never superseded). Passes on
/// one App are not meant to overlap: the server runs them one at a time.
pub async fn reconcile(app: &Arc<App>, at_start: bool) -> Result<usize> {
    let Some(gh) = &app.github else {
        return Ok(0);
    };
    let previous = std::mem::take(&mut *app.catch_up_missing.lock());
    let mut now_missing = std::collections::HashSet::new();
    let mut started = 0;
    for repo in app.db.repos()? {
        let Some(full) = repo.github.clone() else {
            continue;
        };
        // A repo Pit never ran: installing Pit does not run every open PR.
        if !app.db.repo_has_runs(&repo.id)? {
            continue;
        }
        let mut ready = |key: String| {
            let go = at_start || previous.contains(&key);
            if !go {
                now_missing.insert(key);
            }
            go
        };
        match reconcile_repo(app, gh, &repo, &full, &mut ready).await {
            Ok(n) => started += n,
            Err(e) => tracing::warn!("catching up on {full}: {e:#}"),
        }
    }
    *app.catch_up_missing.lock() = now_missing;
    Ok(started)
}

fn millis(t: &serde_json::Value) -> i64 {
    t.as_str()
        .and_then(|t| t.parse::<jiff::Timestamp>().ok())
        .map(|t| t.as_millisecond())
        .unwrap_or(0)
}

async fn reconcile_repo(
    app: &Arc<App>,
    gh: &GitHub,
    repo: &super::db::Repo,
    full: &str,
    ready: &mut impl FnMut(String) -> bool,
) -> Result<usize> {
    let get = reqwest::Method::GET;
    let none = serde_json::Value::Null;
    // Three days, and never more than the run history kept: a head whose
    // run was pruned must not look unrun.
    let days = super::history_days().map_or(3, |d| d.min(3));
    let cutoff = now_ms() - days * 86_400_000;
    let mut started = 0;
    let info = gh
        .call(full, get.clone(), &format!("/repos/{full}"), none.clone())
        .await?;
    // Not guessed: the head of the wrong branch would run as trusted.
    let default = info["default_branch"]
        .as_str()
        .context("the repository has no default_branch")?
        .to_string();
    let branch = gh
        .call(
            full,
            get.clone(),
            &format!("/repos/{full}/branches/{default}"),
            none.clone(),
        )
        .await?;
    // Only a recent head: an old one's run may just have been pruned from
    // history (`PIT_HISTORY_DAYS`), and must not run again as trusted.
    let recent = millis(&branch["commit"]["commit"]["committer"]["date"]) >= cutoff;
    if let (Some(sha), true) = (branch["commit"]["sha"].as_str(), recent) {
        if !app.db.has_run(&repo.id, sha, &default)?
            && ready(format!("{}:{default}:{sha}", repo.id))
        {
            let id = app
                .create_run(
                    CreateRun {
                        repo: repo.name.clone(),
                        url: None,
                        sha: sha.to_string(),
                        overlay: None,
                        tasks: vec![],
                        trigger: Some("push".into()),
                        trusted: super::trusted_branch(&default, Some(&default)),
                        git_ref: Some(default.clone()),
                        no_cache: false,
                        fresh: false,
                        debug: false,
                        counts_as: None,
                        no_secrets: false,
                        draft: false,
                    },
                    None,
                )
                .await?;
            tracing::info!("{full}: {default} at {sha} had no run (missed webhook?): started {id}");
            started += 1;
        }
    }
    // Most recently updated first, so one page covers the window even on a
    // repo with hundreds of open pull requests.
    let pulls = gh
        .call(
            full,
            get,
            &format!("/repos/{full}/pulls?state=open&sort=updated&direction=desc&per_page=100"),
            none,
        )
        .await?;
    for pr in pulls.as_array().into_iter().flatten() {
        if millis(&pr["updated_at"]) < cutoff {
            break;
        }
        let same_repo = pr["head"]["repo"]["full_name"].as_str() == Some(full);
        let (Some(number), Some(sha), true) =
            (pr["number"].as_u64(), pr["head"]["sha"].as_str(), same_repo)
        else {
            continue;
        };
        let git_ref = format!("pull/{number}");
        if app.db.has_run(&repo.id, sha, &git_ref)?
            || !ready(format!("{}:{git_ref}:{sha}", repo.id))
        {
            continue;
        }
        let id = app
            .create_run(
                CreateRun {
                    repo: repo.name.clone(),
                    url: None,
                    sha: sha.to_string(),
                    overlay: None,
                    tasks: vec![],
                    trigger: Some("pr".into()),
                    git_ref: Some(git_ref),
                    no_cache: false,
                    fresh: false,
                    debug: false,
                    counts_as: None,
                    no_secrets: false,
                    trusted: false,
                    draft: pr["draft"].as_bool().unwrap_or(false),
                },
                Some(number),
            )
            .await?;
        tracing::info!(
            "{full}: pull request #{number} at {sha} had no run (missed webhook?): started {id}"
        );
        started += 1;
    }
    Ok(started)
}

/// Finds the repo for a GitHub full name, registering it on first sight.
/// `PIT_REPO_<OWNER>_<NAME>=<url or path>` overrides the clone URL.
fn register(app: &App, r: &RepoPayload) -> Result<super::db::Repo> {
    if let Some(repo) = app.db.repo_by_github(&r.full_name)? {
        return Ok(repo);
    }
    let env_key = format!(
        "PIT_REPO_{}",
        r.full_name.replace(['/', '-', '.'], "_").to_uppercase()
    );
    let url = std::env::var(env_key).unwrap_or_else(|_| r.clone_url.clone());
    let _ = &r.default_branch;
    // On a shared server, a row an API caller registered with this URL is
    // theirs: adopting it would make its pending queue entries and runs the
    // GitHub repo's, trusted. A local server without a token has one user.
    if app.token.is_some() {
        return app.db.insert_github_repo(&r.full_name, &url, &r.full_name);
    }
    app.db
        .upsert_repo(&r.full_name, Some(&url), Some(&r.full_name))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn check_tables_name_slow_passes_and_failures() {
        let task = |name: &str, status: Status, message: Option<&str>| TaskView {
            id: name.into(),
            name: name.into(),
            deps: vec![],
            status,
            action_key: None,
            exit_code: None,
            duration_ms: Some(4_000),
            log_digest: None,
            message: message.map(String::from),
            started_at: None,
            finished_at: None,
            summary: None,
            artifacts: vec![],
        };
        let g: GroupView = serde_json::from_value(serde_json::json!({
            "id": "g", "name": "ci", "os": "linux", "status": "failed", "worker": null,
            "prepareMs": null, "message": null, "startedAt": null, "finishedAt": null, "tasks": [],
        }))
        .unwrap();
        let g = GroupView {
            tasks: vec![
                task(
                    "lint",
                    Status::Passed,
                    Some("ran because x · slow: took 4.0s, usually 1.0s"),
                ),
                task("test", Status::Failed, Some("1 of 2 tests failed: a|b")),
                task("build", Status::Passed, Some("ran because inputs changed")),
            ],
            ..g
        };
        let table = summarize(&g, "run_1", false);
        assert!(
            table.contains("| lint | passed | 4.0s | slow: took 4.0s, usually 1.0s |"),
            "{table}"
        );
        assert!(
            table.contains("| test | failed | 4.0s | 1 of 2 tests failed: a\\|b |"),
            "{table}"
        );
        assert!(table.contains("| build | passed | 4.0s |  |"), "{table}");
        // Task summaries follow the table, within GitHub's size limit.
        let mut g = g;
        g.tasks[0].summary = Some("## Coverage\n91%".into());
        g.tasks[1].summary = Some("x".repeat(70_000));
        let text = summarize(&g, "run_1", false);
        assert!(text.contains("### `lint`\n\n## Coverage\n91%"), "{text}");
        assert!(
            text.contains("### `test`\n\n_(summary too long for the check: `GET /api/runs/run_1`"),
            "{}",
            &text[..200]
        );
        // A fork's markdown is fenced.
        let fork = summarize(&g, "run_1", true);
        assert!(
            fork.contains("### `lint`\n\n~~~~~~~~text\n## Coverage\n91%\n~~~~~~~~"),
            "{fork}"
        );
        assert!(text.len() < 65_535);
    }

    #[test]
    fn verifies_github_signatures() {
        let gh = GitHub {
            api: "http://localhost".into(),
            auth: Auth::Token("t".into()),
            webhook_secret: Some("It's a Secret to Everybody".into()),
            http: reqwest::Client::new(),
            tokens: Default::default(),
            reported: Default::default(),
            left_out_posts: Default::default(),
            run_locks: Default::default(),
            retrying: Default::default(),
        };
        // Example from GitHub's webhook documentation.
        let sig = "sha256=757107ea0eb2509fc211221cce984b8a37570b6d7586c22c46f4379c8b043e17";
        assert!(gh.verify_signature(b"Hello, World!", Some(sig)));
        assert!(!gh.verify_signature(b"Hello, World?", Some(sig)));
        assert!(!gh.verify_signature(b"Hello, World!", None));
    }
}
