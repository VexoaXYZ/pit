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

pub struct GitHub {
    api: String,
    auth: Auth,
    webhook_secret: Option<String>,
    http: reqwest::Client,
    /// Installation tokens by repo full name, with expiry (ms).
    tokens: Mutex<std::collections::HashMap<String, (String, i64)>>,
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
                    .await?
                    .error_for_status()?
                    .json()
                    .await?;
                let id = inst["id"].as_u64().context("installation id")?;
                let tok: serde_json::Value = self
                    .http
                    .post(format!("{}/app/installations/{id}/access_tokens", self.api))
                    .bearer_auth(&jwt)
                    .header("accept", "application/vnd.github+json")
                    .send()
                    .await?
                    .error_for_status()?
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
        let resp = self
            .http
            .request(method, format!("{}{path}", self.api))
            .bearer_auth(token)
            .header("accept", "application/vnd.github+json")
            .json(&body)
            .send()
            .await?;
        let status = resp.status();
        let text = resp.text().await.unwrap_or_default();
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
        if let Err(e) = self.try_report_run(app, run_id).await {
            tracing::warn!("reporting {run_id} to GitHub failed: {e:#}");
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
            let summary = summarize(g);
            let annotations = if g.status.is_done() {
                app.db.group_annotations(&g.id)?
            } else {
                vec![]
            };
            self.report_one(
                app,
                &full,
                &run.sha,
                Some(&g.id),
                &g.name,
                g.status,
                &summary,
                &details,
                &annotations,
            )
            .await?;
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
        match &self.auth {
            Auth::Token(_) => {
                let state = match status {
                    Status::Queued | Status::Running => "pending",
                    s if s.is_ok() => "success",
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
                let existing = group_id
                    .and_then(|g| app.db.group(g).ok())
                    .and_then(|g| g.check_run_id);
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
                        self.call(
                            full,
                            reqwest::Method::PATCH,
                            &format!("/repos/{full}/check-runs/{id}"),
                            serde_json::json!({ "output": {
                                "title": body["output"]["title"], "summary": summary, "annotations": chunk,
                            } }),
                        )
                        .await?;
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

fn summarize(g: &GroupView) -> String {
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
            t.name,
            t.status.as_str(),
            time,
            note(t)
        ));
    }
    s
}

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
                        no_secrets: false,
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
                            no_secrets: fork,
                            trusted: false,
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
            if !matches!(action, "opened" | "synchronize" | "reopened") {
                return Ok(serde_json::json!({ "ignored": action }));
            }
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
                        no_secrets: false,
                        trusted: false,
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
            let id = app
                .create_run(
                    CreateRun {
                        repo: repo.name,
                        url: None,
                        sha,
                        overlay: None,
                        tasks: vec![],
                        trigger: Some("rerun".into()),
                        git_ref: prior.git_ref,
                        no_cache: false,
                        fresh: prior.fresh || prior.no_secrets,
                        debug: false,
                        no_secrets: prior.no_secrets,
                        trusted: false,
                    },
                    prior.github_pr,
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
            // People who could push a commit anyway; not drive-by commenters.
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
            // The same commit, with the restrictions it ran under (a fork's
            // commit stays a fork's; its next push still needs the label).
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
                        no_secrets: prior.no_secrets,
                        trusted: false,
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
        let table = summarize(&g);
        assert!(
            table.contains("| lint | passed | 4.0s | slow: took 4.0s, usually 1.0s |"),
            "{table}"
        );
        assert!(
            table.contains("| test | failed | 4.0s | 1 of 2 tests failed: a\\|b |"),
            "{table}"
        );
        assert!(table.contains("| build | passed | 4.0s |  |"), "{table}");
    }

    #[test]
    fn verifies_github_signatures() {
        let gh = GitHub {
            api: "http://localhost".into(),
            auth: Auth::Token("t".into()),
            webhook_secret: Some("It's a Secret to Everybody".into()),
            http: reqwest::Client::new(),
            tokens: Default::default(),
        };
        // Example from GitHub's webhook documentation.
        let sig = "sha256=757107ea0eb2509fc211221cce984b8a37570b6d7586c22c46f4379c8b043e17";
        assert!(gh.verify_signature(b"Hello, World!", Some(sig)));
        assert!(!gh.verify_signature(b"Hello, World?", Some(sig)));
        assert!(!gh.verify_signature(b"Hello, World!", None));
    }
}
