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
                )
                .await;
        }
        for g in &view.groups {
            let summary = summarize(g);
            self.report_one(
                app,
                &full,
                &run.sha,
                Some(&g.id),
                &g.name,
                g.status,
                &summary,
                &details,
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
                let existing = group_id
                    .and_then(|g| app.db.group(g).ok())
                    .and_then(|g| g.check_run_id);
                match existing {
                    Some(id) => {
                        self.call(
                            full,
                            reqwest::Method::PATCH,
                            &format!("/repos/{full}/check-runs/{id}"),
                            body,
                        )
                        .await?;
                    }
                    None => {
                        let created = self
                            .call(
                                full,
                                reqwest::Method::POST,
                                &format!("/repos/{full}/check-runs"),
                                body,
                            )
                            .await?;
                        if let (Some(g), Some(id)) = (group_id, created["id"].as_u64()) {
                            app.db.set_check_run(g, id)?;
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
    s.push_str("\n\n| Task | Status | Time |\n| --- | --- | --- |\n");
    for t in &g.tasks {
        let time = t
            .duration_ms
            .map(|d| format!("{:.1}s", d as f64 / 1000.0))
            .unwrap_or_default();
        s.push_str(&format!(
            "| {} | {} | {} |\n",
            t.name,
            t.status.as_str(),
            time
        ));
    }
    s
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
                        git_ref,
                        no_cache: false,
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
            // A fork's code (including its pit.config.ts) is untrusted: Pit
            // does not run it automatically.
            let head_repo = pr["head"]["repo"]["full_name"].as_str();
            if head_repo.is_some_and(|h| h != r.full_name) {
                return Ok(serde_json::json!({ "ignored": "pull request from a fork" }));
            }
            let sha = pr["head"]["sha"]
                .as_str()
                .context("pull_request.head.sha")?
                .to_string();
            let repo = register(app, &r)?;
            if action == "labeled" && payload["label"]["name"].as_str() == Some(MERGE_LABEL) {
                let entry = super::queue::add(
                    app,
                    QueueAdd {
                        repo: repo.name,
                        url: None,
                        branch: None,
                        pr: Some(number),
                        target: pr["base"]["ref"].as_str().map(String::from),
                    },
                )
                .await?;
                return Ok(serde_json::json!({ "queued": entry.id }));
            }
            if !matches!(action, "opened" | "synchronize" | "reopened") {
                return Ok(serde_json::json!({ "ignored": action }));
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
            let id = app
                .create_run(
                    CreateRun {
                        repo: repo.name,
                        url: None,
                        sha,
                        overlay: None,
                        tasks: vec![],
                        trigger: Some("rerun".into()),
                        git_ref: None,
                        no_cache: false,
                    },
                    None,
                )
                .await?;
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
