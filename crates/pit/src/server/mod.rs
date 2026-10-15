//! The control plane: run planning, dispatch, results, events, merge queue,
//! GitHub integration, remote caches and the dashboard.

pub mod api;
pub mod db;
pub mod gc;
pub mod github;
pub mod notify;
pub mod queue;
pub mod remote_cache;
pub mod resolve;
pub mod schedule;
pub mod web;

use std::collections::HashMap;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicU64;
use std::sync::Arc;

use anyhow::{bail, Context, Result};
use tokio::sync::{broadcast, Notify};

use crate::cas::Cas;
use crate::model::*;
use db::Db;

pub struct App {
    pub db: Db,
    pub cas: Cas,
    pub data_dir: PathBuf,
    pub events: broadcast::Sender<Event>,
    /// Woken whenever a group becomes claimable.
    pub work: Notify,
    pub token: Option<String>,
    pub github: Option<github::GitHub>,
    pub public_url: String,
    mirror_locks: parking_lot::Mutex<HashMap<String, Arc<tokio::sync::Mutex<()>>>>,
    pub queue_lock: tokio::sync::Mutex<()>,
    /// Wakes the merge queue loop (see `serve_app`).
    pub queue_kick: Notify,
    /// CAS size limit in bytes; 0 means unlimited. See `gc`.
    pub cache_max_bytes: AtomicU64,
    /// Blobs younger than this are never garbage collected.
    pub gc_grace_ms: AtomicU64,
    /// Totals since start, for `/api/stats`.
    pub evicted_entries: AtomicU64,
    pub freed_bytes: AtomicU64,
    /// See `ServerOpts::worker_token`.
    pub worker_token: parking_lot::Mutex<Option<String>>,
    /// See `ServerOpts::notify_url`.
    pub notify_url: parking_lot::Mutex<Option<String>>,
    /// Action keys being computed right now: trust scope and key to the
    /// task doing it. See `api::cache_claim`.
    pub inflight: parking_lot::Mutex<HashMap<String, String>>,
    /// The last minute `schedule::tick` evaluated.
    pub schedule_minute: parking_lot::Mutex<Option<i64>>,
}

pub struct ServerOpts {
    pub data_dir: PathBuf,
    pub addr: SocketAddr,
    pub token: Option<String>,
    /// Required to claim jobs, if set. Claiming hands out leases, and a
    /// trusted run's lease writes the cache main reads, so without it every
    /// API token holder can act as a trusted worker.
    pub worker_token: Option<String>,
    /// Webhook (Slack-compatible) told when main or a schedule breaks or is
    /// fixed. See `notify`.
    pub notify_url: Option<String>,
    pub public_url: Option<String>,
    /// CAS size limit in bytes; 0 means unlimited.
    pub cache_max_bytes: u64,
}

impl App {
    pub fn new(data_dir: &Path, token: Option<String>, public_url: String) -> Result<Arc<App>> {
        Self::with_github(data_dir, token, public_url, github::GitHub::from_env()?)
    }

    pub fn with_github(
        data_dir: &Path,
        token: Option<String>,
        public_url: String,
        github: Option<github::GitHub>,
    ) -> Result<Arc<App>> {
        std::fs::create_dir_all(data_dir)?;
        let db = Db::open(&data_dir.join("pit.db"))?;
        let cas = Cas::open(data_dir.join("cas"))?;
        let (events, _) = broadcast::channel(4096);
        Ok(Arc::new(App {
            db,
            cas,
            data_dir: data_dir.to_path_buf(),
            events,
            work: Notify::new(),
            token,
            github,
            public_url,
            mirror_locks: Default::default(),
            queue_lock: Default::default(),
            queue_kick: Notify::new(),
            cache_max_bytes: AtomicU64::new(0),
            worker_token: Default::default(),
            notify_url: Default::default(),
            inflight: Default::default(),
            schedule_minute: Default::default(),
            gc_grace_ms: AtomicU64::new(2 * 60 * 1000),
            evicted_entries: AtomicU64::new(0),
            freed_bytes: AtomicU64::new(0),
        }))
    }

    pub fn emit(
        &self,
        kind: &str,
        run_id: Option<&str>,
        group_id: Option<&str>,
        task_id: Option<&str>,
        data: serde_json::Value,
    ) {
        let _ = self.events.send(Event {
            kind: kind.to_string(),
            run_id: run_id.map(String::from),
            group_id: group_id.map(String::from),
            task_id: task_id.map(String::from),
            data,
        });
    }

    pub fn mirror_dir(&self, repo_id: &str) -> PathBuf {
        self.data_dir.join("mirrors").join(format!("{repo_id}.git"))
    }

    /// Ensures the server's mirror of a repo has `sha`. Serialized per repo.
    pub async fn ensure_commit(&self, repo: &db::Repo, sha: &str) -> Result<PathBuf> {
        let dir = self.mirror_dir(&repo.id);
        let lock = self.mirror_lock(&repo.id);
        let _g = lock.lock().await;
        crate::git::ensure_commit(&dir, &repo.url, sha).await?;
        Ok(dir)
    }

    pub async fn fetch_mirror(&self, repo: &db::Repo) -> Result<PathBuf> {
        let dir = self.mirror_dir(&repo.id);
        let lock = self.mirror_lock(&repo.id);
        let _g = lock.lock().await;
        crate::git::ensure_mirror(&dir, &repo.url).await?;
        crate::git::fetch(&dir).await?;
        Ok(dir)
    }

    pub fn mirror_lock(&self, repo_id: &str) -> Arc<tokio::sync::Mutex<()>> {
        self.mirror_locks
            .lock()
            .entry(repo_id.to_string())
            .or_default()
            .clone()
    }

    /// Loads the config for a commit, preferring a version from the overlay.
    pub async fn load_config(
        &self,
        mirror: &Path,
        sha: &str,
        overlay: Option<&Overlay>,
    ) -> Result<PitConfig> {
        let scratch = self.data_dir.join("tmp");
        for name in crate::config::CONFIG_FILES {
            let src = match overlay.and_then(|o| o.entries.iter().find(|e| e.path == *name)) {
                Some(entry) => {
                    let Some(d) = &entry.digest else { continue };
                    self.cas
                        .get(d)?
                        .context("overlay config blob missing from CAS")?
                }
                None => match crate::git::show_file(mirror, sha, name).await? {
                    Some(src) => src,
                    None => continue,
                },
            };
            let ctx = if name.ends_with(".ts") {
                self.workspace_context(mirror, sha, overlay).await?
            } else {
                Default::default()
            };
            return crate::config::parse(name, &src, &scratch, &ctx).await;
        }
        bail!("no {} at {sha}", crate::config::CONFIG_TS)
    }

    /// The workspace packages at this commit plus overlay, for configs that
    /// export a function.
    async fn workspace_context(
        &self,
        mirror: &Path,
        sha: &str,
        overlay: Option<&Overlay>,
    ) -> Result<crate::workspace::Context> {
        let files: Vec<String> = resolve::file_index(mirror, sha, overlay)
            .await?
            .into_iter()
            .map(|(p, _)| p)
            .collect();
        // Only the workspace manifests are read, so fetch just those.
        let wanted: Vec<&String> = files
            .iter()
            .filter(|p| {
                !p.contains("node_modules/")
                    && (p.ends_with("package.json") || *p == "pnpm-workspace.yaml")
            })
            .collect();
        let mut contents = HashMap::new();
        for path in wanted {
            let from_overlay = overlay.and_then(|o| o.entries.iter().find(|e| &e.path == path));
            let bytes = match from_overlay.and_then(|e| e.digest.as_ref()) {
                Some(d) => self.cas.get(d)?,
                None => crate::git::show_file(mirror, sha, path).await?,
            };
            if let Some(b) = bytes {
                contents.insert(path.clone(), b);
            }
        }
        Ok(crate::workspace::Context {
            packages: crate::workspace::packages(&files, |p| contents.get(p).cloned()),
        })
    }

    pub fn load_overlay(&self, digest: &str) -> Result<Overlay> {
        let bytes = self
            .cas
            .get(digest)?
            .context("overlay manifest missing from CAS")?;
        Ok(serde_json::from_slice(&bytes)?)
    }

    pub async fn create_run(
        self: &Arc<Self>,
        req: CreateRun,
        github_pr: Option<u64>,
    ) -> Result<String> {
        let repo = self.db.upsert_repo(&req.repo, req.url.as_deref(), None)?;
        let overlay_files = match &req.overlay {
            Some(d) => self.load_overlay(d)?.entries.len(),
            None => 0,
        };
        let trigger = req.trigger.clone().unwrap_or_else(|| "api".into());
        // Uncommitted files are never trusted, whoever sends them.
        let trusted = req.trusted && req.overlay.is_none();
        let run_id = self.db.insert_run(
            &repo.id,
            &req.sha,
            req.git_ref.as_deref(),
            req.overlay.as_deref(),
            overlay_files,
            &trigger,
            req.no_cache,
            github_pr,
            trusted,
        )?;
        if req.fresh {
            self.db.set_run_fresh(&run_id)?;
        }
        if req.no_secrets {
            self.db.set_run_no_secrets(&run_id)?;
        }
        self.emit(
            "run.created",
            Some(&run_id),
            None,
            None,
            serde_json::json!({ "repo": repo.name, "sha": req.sha, "trigger": trigger }),
        );
        // A newer push to a branch, PR update or local run makes the older
        // ones pointless: stop them instead of finishing them. Main-branch
        // pushes, merge queue and post-merge runs always finish.
        if let Some(git_ref) = req.git_ref.as_deref().filter(|r| supersedes(&trigger, r)) {
            for old in self
                .db
                .superseded_runs(&repo.id, &trigger, git_ref, &run_id)?
            {
                if self
                    .db
                    .cancel_run_because(&old, &format!("superseded by {run_id}"))?
                {
                    self.run_finished(&old).await;
                }
            }
        }
        let app = self.clone();
        let id = run_id.clone();
        tokio::spawn(async move {
            if let Err(e) = app.plan_run(&id, &repo, &req).await {
                tracing::warn!("planning {id} failed: {e:#}");
                let _ = app
                    .db
                    .set_run_status(&id, Status::Error, Some(&format!("{e:#}")));
                app.run_finished(&id).await;
            }
        });
        Ok(run_id)
    }

    async fn plan_run(
        self: &Arc<Self>,
        run_id: &str,
        repo: &db::Repo,
        req: &CreateRun,
    ) -> Result<()> {
        let mirror = self.ensure_commit(repo, &req.sha).await?;
        let overlay = req
            .overlay
            .as_deref()
            .map(|d| self.load_overlay(d))
            .transpose()?;
        let config = self
            .load_config(&mirror, &req.sha, overlay.as_ref())
            .await?;
        let groups = crate::graph::plan(&config, &req.tasks)?;
        let run = self.db.run(run_id)?;
        if run.status.is_done() {
            return Ok(()); // Cancelled while planning.
        }
        let trusted = run.trusted;
        if groups.is_empty() {
            self.db
                .set_run_status(run_id, Status::Passed, Some("no tasks to run"))?;
            self.run_finished(run_id).await;
            return Ok(());
        }
        // Groups the cache answers completely never need a worker.
        let files = if req.no_cache {
            None
        } else {
            resolve::file_index(&mirror, &req.sha, overlay.as_ref())
                .await
                .ok()
        };
        let mut resolved = 0;
        for g in &groups {
            if let Some(results) = files
                .as_deref()
                .map(|f| resolve::resolve(self, g, f, trusted))
                .transpose()?
                .flatten()
            {
                let gid = self
                    .db
                    .insert_cached_group(run_id, &g.name, &g.os, &g.tasks, &results)?;
                self.emit_cached_group(run_id, &gid)?;
                resolved += 1;
                continue;
            }
            let mut backends: Vec<Backend> = g
                .tasks
                .iter()
                .filter_map(|(_, t)| t.backend)
                .filter(|b| *b != Backend::Host)
                .collect();
            backends.sort_by_key(|b| b.as_str());
            backends.dedup();
            // Locks are per repo: one repo's runs cannot hold another's.
            let lock = g.lock.as_ref().map(|l| format!("{}:{l}", repo.id));
            self.db.insert_locked_group(
                run_id,
                &g.name,
                &g.os,
                &backends,
                &g.tasks,
                lock.as_deref(),
            )?;
        }
        self.emit(
            "run.planned",
            Some(run_id),
            None,
            None,
            serde_json::json!({ "groups": groups.len(), "resolvedFromCache": resolved }),
        );
        if resolved > 0 {
            self.group_finished(run_id).await?;
        }
        if let Some(gh) = &self.github {
            gh.report_run(self, run_id).await;
        }
        self.work.notify_waiters();
        Ok(())
    }

    /// Emits the events a worker would for a group the cache answered, so the
    /// CLI, dashboard and MCP clients see it like any other group.
    fn emit_cached_group(&self, run_id: &str, group_id: &str) -> Result<()> {
        let view = self.db.run_view(run_id)?;
        let Some(g) = view.groups.iter().find(|g| g.id == group_id) else {
            return Ok(());
        };
        self.emit(
            "group.started",
            Some(run_id),
            Some(group_id),
            None,
            serde_json::json!({ "name": g.name, "worker": "cache" }),
        );
        for t in &g.tasks {
            self.emit(
                "task.finished",
                Some(run_id),
                Some(group_id),
                Some(&t.id),
                serde_json::json!({ "name": t.name, "status": t.status, "durationMs": 0, "exitCode": t.exit_code, "message": t.message }),
            );
        }
        self.emit(
            "group.finished",
            Some(run_id),
            Some(group_id),
            None,
            serde_json::json!({ "name": g.name, "status": g.status, "prepareMs": 0 }),
        );
        Ok(())
    }

    /// Called whenever a group finishes; closes the run when all are done.
    pub async fn group_finished(self: &Arc<Self>, run_id: &str) -> Result<()> {
        if self.db.run(run_id)?.status.is_done() {
            return Ok(());
        }
        let groups = self.db.groups_for_run(run_id)?;
        if groups.is_empty() {
            return Ok(()); // Still planning.
        }
        if groups.iter().all(|g| g.status.is_done()) {
            // Workers never report a skipped group; only a cancel does, and a
            // cancelled run must never count as green.
            let cancelled = groups.iter().any(|g| g.status == Status::Skipped);
            let failed = groups.iter().any(|g| !g.status.is_ok());
            let status = if failed {
                Status::Failed
            } else {
                Status::Passed
            };
            self.db
                .set_run_status(run_id, status, cancelled.then_some("cancelled"))?;
            self.run_finished(run_id).await;
        } else if let Some(gh) = &self.github {
            gh.report_run(self, run_id).await;
        }
        Ok(())
    }

    pub async fn run_finished(self: &Arc<Self>, run_id: &str) {
        let status = self
            .db
            .run(run_id)
            .map(|r| r.status.as_str())
            .unwrap_or("error");
        self.emit(
            "run.finished",
            Some(run_id),
            None,
            None,
            serde_json::json!({ "status": status }),
        );
        if let Some(gh) = &self.github {
            gh.report_run(self, run_id).await;
        }
        notify::run_finished(self, run_id).await;
        self.queue_kick.notify_one();
    }

    pub fn authorized(&self, header: Option<&str>, query_token: Option<&str>) -> bool {
        let Some(token) = &self.token else {
            return true;
        };
        let bearer = header
            .and_then(|h| h.strip_prefix("Bearer "))
            .map(str::trim);
        let basic_ok = header
            .and_then(|h| h.strip_prefix("Basic "))
            .and_then(|b| {
                use base64::Engine;
                base64::engine::general_purpose::STANDARD
                    .decode(b.trim())
                    .ok()
            })
            .map(|raw| {
                String::from_utf8_lossy(&raw)
                    .split_once(':')
                    .map(|(_, p)| p == token)
                    .unwrap_or(false)
            })
            .unwrap_or(false);
        bearer == Some(token.as_str()) || query_token == Some(token.as_str()) || basic_ok
    }
}

impl App {
    #[doc(hidden)]
    pub fn supersedes_for_test(trigger: &str, git_ref: &str) -> bool {
        supersedes(trigger, git_ref)
    }
}

/// Whether runs for this branch are trusted: they write the cache that every
/// run reads and get their own warm workspaces. Untrusted runs (PRs, other
/// branches, `pit run`) read trusted results but write a separate cache that
/// trusted runs never read, so a branch cannot plant a result or a file that
/// main later picks up (cache poisoning, CVE-2025-36852).
///
/// Only the default branch is trusted. When it is not known, `main`, `master`
/// and `trunk` are.
pub fn trusted_branch(branch: &str, default_branch: Option<&str>) -> bool {
    match default_branch {
        Some(d) => branch == d,
        None => matches!(branch, "main" | "master" | "trunk"),
    }
}

/// Whether a new run with this trigger and ref replaces older ones.
fn supersedes(trigger: &str, git_ref: &str) -> bool {
    match trigger {
        "pr" | "local" => true,
        "push" => !matches!(git_ref, "main" | "master" | "trunk"),
        _ => false,
    }
}

pub async fn serve(opts: ServerOpts) -> Result<()> {
    let public_url = opts
        .public_url
        .clone()
        .unwrap_or_else(|| format!("http://{}", opts.addr));
    let app = App::new(&opts.data_dir, opts.token.clone(), public_url)?;
    *app.worker_token.lock() = opts.worker_token.clone();
    *app.notify_url.lock() = opts.notify_url.clone();
    app.cache_max_bytes
        .store(opts.cache_max_bytes, std::sync::atomic::Ordering::Relaxed);
    serve_app(app, opts.addr).await
}

pub async fn serve_app(app: Arc<App>, addr: SocketAddr) -> Result<()> {
    let listener = tokio::net::TcpListener::bind(addr)
        .await
        .with_context(|| format!("binding {addr}"))?;
    serve_listener(app, listener).await
}

/// Why no online worker can take a group, if none can: the GitHub "Waiting
/// for a runner to pick up this job" that never says what is missing.
pub fn waiting_reason(os: &str, needs: &[Backend], online: &[WorkerInfo]) -> Option<String> {
    let fits = |w: &WorkerInfo| {
        (os == "any" || w.os == os) && needs.iter().all(|b| w.backends.contains(b))
    };
    if online.iter().any(fits) {
        return None;
    }
    let mut need = vec![format!("os {os}")];
    need.extend(
        needs
            .iter()
            .filter(|b| **b != Backend::Host)
            .map(|b| format!("backend {}", b.as_str())),
    );
    let have = if online.is_empty() {
        "no workers are online".to_string()
    } else {
        let list: Vec<String> = online
            .iter()
            .map(|w| {
                let b: Vec<&str> = w.backends.iter().map(|b| b.as_str()).collect();
                format!("{} ({}; {})", w.name, w.os, b.join(", "))
            })
            .collect();
        format!("online: {}", list.join(", "))
    };
    Some(format!(
        "waiting: no online worker can run it (needs {}); {have}. Start a matching `pit worker`.",
        need.join(", ")
    ))
}

impl App {
    /// Marks queued groups that no online worker can run, and clears the
    /// mark once one can. They keep waiting: a worker may still join.
    pub fn explain_waiting(&self) -> Result<()> {
        let now = now_ms();
        let online: Vec<WorkerInfo> = self
            .db
            .workers()?
            .into_iter()
            .filter(|(_, seen, _)| now - seen < 30_000)
            .map(|(w, _, _)| w)
            .collect();
        for g in self.db.waiting_groups(now - 3_000)? {
            let reason = waiting_reason(&g.os, &g.needs, &online).or_else(|| {
                let (lock, holder) = (g.lock.as_ref()?, g.lock_holder.as_ref()?);
                let lock = lock.split_once(':').map_or(lock.as_str(), |(_, l)| l);
                Some(format!("waiting: lock `{lock}` is held by {holder}"))
            });
            let ours = g
                .message
                .as_deref()
                .is_some_and(|m| m.starts_with("waiting:"));
            if (reason.is_none() && !ours) || reason == g.message {
                continue;
            }
            self.db.set_queued_group_message(&g.id, reason.as_deref())?;
            self.emit(
                "group.waiting",
                Some(&g.run_id),
                Some(&g.id),
                None,
                serde_json::json!({ "name": g.name, "os": g.os, "message": reason }),
            );
        }
        Ok(())
    }
}

pub async fn serve_listener(app: Arc<App>, listener: tokio::net::TcpListener) -> Result<()> {
    // Requeue work from workers that went away.
    let reaper = app.clone();
    tokio::spawn(async move {
        loop {
            tokio::time::sleep(std::time::Duration::from_secs(10)).await;
            match reaper.db.requeue_stale_groups(60_000) {
                Ok(ids) if !ids.is_empty() => {
                    tracing::warn!("requeued {} group(s) from silent workers", ids.len());
                    reaper.work.notify_waiters();
                }
                Err(e) => tracing::warn!("reaper: {e:#}"),
                _ => {}
            }
        }
    });
    // Scheduled runs (`schedules` in the config), checked twice a minute so
    // no minute is missed.
    let sched = app.clone();
    tokio::spawn(async move {
        loop {
            if let Err(e) = schedule::tick(&sched, now_ms()).await {
                tracing::warn!("schedules: {e:#}");
            }
            tokio::time::sleep(std::time::Duration::from_secs(30)).await;
        }
    });
    // Say so when a group has no worker that can run it.
    let waiting = app.clone();
    tokio::spawn(async move {
        loop {
            tokio::time::sleep(std::time::Duration::from_secs(2)).await;
            if let Err(e) = waiting.explain_waiting() {
                tracing::warn!("checking waiting groups: {e:#}");
            }
        }
    });
    // Keep the cache under its size limit.
    let gc_app = app.clone();
    tokio::spawn(async move {
        loop {
            tokio::time::sleep(std::time::Duration::from_secs(60)).await;
            let app = gc_app.clone();
            match tokio::task::spawn_blocking(move || gc::collect(&app, false)).await {
                Ok(Err(e)) => tracing::warn!("cache gc failed: {e:#}"),
                Err(e) => tracing::warn!("cache gc panicked: {e}"),
                _ => {}
            }
        }
    });
    // The merge queue advances when a run finishes, and every few seconds to
    // notice branches that moved.
    let queue_app = app.clone();
    tokio::spawn(async move {
        loop {
            let _ = tokio::time::timeout(
                std::time::Duration::from_secs(5),
                queue_app.queue_kick.notified(),
            )
            .await;
            if let Err(e) = queue::tick(&queue_app).await {
                tracing::warn!("merge queue tick failed: {e:#}");
            }
        }
    });
    let router = api::router(app.clone());
    tracing::info!("pit server listening on {}", app.public_url);
    axum::serve(listener, router).await?;
    Ok(())
}

#[cfg(test)]
mod trust_tests {
    use super::trusted_branch;

    #[test]
    fn only_the_default_branch_is_trusted() {
        assert!(trusted_branch("develop", Some("develop")));
        // A known default branch overrides the usual names.
        assert!(!trusted_branch("master", Some("main")));
        assert!(!trusted_branch("trunk", Some("main")));
        assert!(trusted_branch("main", None));
        assert!(!trusted_branch("feature", None));
    }
}

#[cfg(test)]
mod waiting_tests {
    use super::*;

    #[test]
    fn says_what_no_online_worker_provides() {
        let linux = WorkerInfo {
            name: "local".into(),
            os: "linux".into(),
            arch: "x86_64".into(),
            cpus: 8,
            backends: vec![Backend::Host, Backend::Sandbox],
            kvm: false,
        };
        assert_eq!(
            waiting_reason("linux", &[Backend::Host], std::slice::from_ref(&linux)),
            None
        );
        assert_eq!(
            waiting_reason("any", &[], std::slice::from_ref(&linux)),
            None
        );
        let mac = waiting_reason("macos", &[Backend::Host], std::slice::from_ref(&linux)).unwrap();
        assert!(
            mac.contains("needs os macos") && mac.contains("local (linux; host, sandbox)"),
            "{mac}"
        );
        let docker = waiting_reason("linux", &[Backend::Docker], &[linux]).unwrap();
        assert!(docker.contains("backend docker"), "{docker}");
        assert!(waiting_reason("linux", &[], &[])
            .unwrap()
            .contains("no workers are online"));
    }
}
