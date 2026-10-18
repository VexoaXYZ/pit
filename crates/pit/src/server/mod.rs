//! The control plane: run planning, dispatch, results, events, merge queue,
//! GitHub integration, remote caches and the dashboard.

pub mod api;
pub mod db;
pub mod gc;
pub mod github;
pub mod notify;
pub mod oidc;
pub mod queue;
pub mod remote_cache;
pub mod resolve;
pub mod schedule;
pub mod shards;
pub mod web;

use std::collections::{HashMap, HashSet};
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
    /// Heads the last catch-up pass found without a run (see
    /// `github::reconcile`).
    pub catch_up_missing: parking_lot::Mutex<HashSet<String>>,
    /// Signs OIDC tokens for tasks; set only by `pit server --oidc`.
    pub oidc: std::sync::OnceLock<oidc::Issuer>,
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
    /// Issue OIDC tokens to tasks (see `oidc`).
    pub oidc: bool,
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
        if token.is_none() {
            db.forget_shared_trusted_demotion()?;
        } else if db.demote_shared_trusted_results_once()? {
            tracing::info!(
                "trusted cache results from before per-repo trust (or from a time without \
                 an API token) were kept for untrusted runs only; each repo's next trusted \
                 run rebuilds them"
            );
        }
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
            catch_up_missing: Default::default(),
            oidc: Default::default(),
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

    /// Whether a repo's `pit.config.ts` must be evaluated in the sandbox
    /// rather than on the server. On a shared server (one with an API token),
    /// only a trusted run of a repo registered by the GitHub webhook is
    /// evaluated here: anyone with the API token can register a repo or send
    /// an overlay, and a config evaluated on the server can read anything
    /// the server can (its OIDC signing key, the GitHub App's key). A local
    /// `pit up` without a token has one user, who already runs the server.
    pub fn config_sandboxed(&self, repo: &db::Repo, trusted: bool) -> bool {
        self.token.is_some() && !(trusted && repo.github.is_some())
    }

    /// Where a run's cached results go and which it may read. On a shared
    /// server a repo registered through the API (anyone with the token can)
    /// still gets trusted runs on its own default branch, so they write the
    /// repo's own trusted namespace, never the one every repo's main reads.
    pub fn cache_scope(&self, repo_id: &str, github: bool, trusted: bool) -> db::CacheScope {
        db::CacheScope {
            trusted,
            repo: (self.token.is_some() && !github).then(|| repo_id.to_string()),
        }
    }

    /// Loads the config for a commit, preferring a version from the overlay.
    pub async fn load_config(
        &self,
        repo: &db::Repo,
        mirror: &Path,
        sha: &str,
        overlay: Option<&Overlay>,
        untrusted_code: bool,
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
            return crate::config::parse(name, &src, &scratch, &ctx, untrusted_code).await;
        }
        // The operator's config for a GitHub repo whose commits carry none,
        // so Pit can be tried on a repo without changing it.
        if let Some(path) = repo.github.as_deref().and_then(operator_config) {
            let src =
                std::fs::read(&path).with_context(|| format!("reading {}", path.display()))?;
            return crate::config::parse(
                crate::config::CONFIG_JSON,
                &src,
                &scratch,
                &Default::default(),
                untrusted_code,
            )
            .await;
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
        // Root manifests first, then only the packages they name: a repo
        // can have hundreds of unrelated package.json files (fixtures,
        // vendored code), and reading them all made planning slow.
        let roots = [
            "package.json".to_string(),
            "pnpm-workspace.yaml".to_string(),
        ];
        let root = self.read_files(mirror, sha, overlay, &roots).await?;
        let manifests = crate::workspace::manifest_paths(&files, |p| root.get(p).cloned());
        let contents = self.read_files(mirror, sha, overlay, &manifests).await?;
        Ok(crate::workspace::Context {
            packages: crate::workspace::package_list(&manifests, |p| contents.get(p).cloned()),
        })
    }

    /// Files at a commit, with the overlay's versions where it has them.
    async fn read_files(
        &self,
        mirror: &Path,
        sha: &str,
        overlay: Option<&Overlay>,
        paths: &[String],
    ) -> Result<HashMap<String, Vec<u8>>> {
        let mut from_git = Vec::new();
        let mut out = HashMap::new();
        for path in paths {
            match overlay.and_then(|o| o.entries.iter().find(|e| &e.path == path)) {
                Some(e) => {
                    let bytes = e.digest.as_ref().map(|d| self.cas.get(d)).transpose()?;
                    if let Some(b) = bytes.flatten() {
                        out.insert(path.clone(), b);
                    }
                }
                None => from_git.push(path.clone()),
            }
        }
        out.extend(crate::git::read_blobs(mirror, sha, &from_git).await?);
        Ok(out)
    }

    /// Turns on the OIDC issuer. Only for a server whose data dir no task can
    /// read: never under `pit up`, where the worker shares its user.
    pub fn enable_oidc(&self) -> Result<()> {
        if self.token.is_none() || self.worker_token.lock().is_none() {
            bail!(
                "--oidc needs PIT_TOKEN and PIT_WORKER_TOKEN: without them anyone who can reach \
                 the server could claim a job and get a token for it"
            );
        }
        let _ = self.oidc.set(oidc::Issuer::load_or_create(&self.data_dir)?);
        Ok(())
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
        if req.debug {
            self.db.set_run_debug(&run_id)?;
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
        if let Some(git_ref) = req
            .git_ref
            .as_deref()
            .filter(|r| supersedes(&trigger, r, trusted))
        {
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

    /// The commit tasks diff against to find what changed (`nx affected`,
    /// `turbo --affected`), as `nrwl/nx-set-shas` picks it: the newest
    /// commit of the default branch whose trusted run passed and that this
    /// commit contains. Otherwise where this commit left the default branch,
    /// or, on the default branch itself, its parent.
    async fn base_commit(&self, mirror: &Path, repo: &db::Repo, sha: &str) -> Option<String> {
        // The mirror's HEAD is the remote's default branch (`develop` too);
        // main, master or trunk otherwise.
        let head = crate::git::git(mirror, &["symbolic-ref", "--short", "HEAD"])
            .await
            .ok()
            .map(|b| b.trim().to_string())
            .filter(|b| !b.is_empty());
        let branch = match head {
            Some(b)
                if crate::git::rev_parse(mirror, &format!("refs/heads/{b}"))
                    .await
                    .ok()
                    .flatten()
                    .is_some() =>
            {
                b
            }
            _ => match queue::default_branch(mirror).await {
                Ok(b) => b,
                Err(_) => {
                    return crate::git::rev_parse(mirror, &format!("{sha}^"))
                        .await
                        .ok()
                        .flatten()
                }
            },
        };
        let greens = match self.db.green_commits(&repo.id, &branch, 20) {
            Ok(g) => g,
            Err(e) => {
                tracing::warn!("finding the last green commit: {e:#}");
                vec![]
            }
        };
        for green in greens {
            if green != sha && crate::git::is_ancestor(mirror, &green, sha).await {
                return Some(green);
            }
        }
        let fork_point = crate::git::merge_base(mirror, sha, &format!("refs/heads/{branch}"))
            .await
            .ok()
            .flatten()?;
        if fork_point != sha {
            return Some(fork_point);
        }
        crate::git::rev_parse(mirror, &format!("{sha}^"))
            .await
            .ok()
            .flatten()
    }

    async fn plan_run(
        self: &Arc<Self>,
        run_id: &str,
        repo: &db::Repo,
        req: &CreateRun,
    ) -> Result<()> {
        let t0 = std::time::Instant::now();
        let mirror = self.ensure_commit(repo, &req.sha).await?;
        let t_commit = t0.elapsed();
        // `[skip ci]` and friends in a pushed commit (a release bot's version
        // bump), as GitHub reads them: the run is skipped and reports
        // nothing, so a required check waits, as on GitHub, rather than
        // passing; three words in a commit message must not get a change
        // past CI. Never for the merge queue, schedules, reruns or `pit run`.
        if matches!(req.trigger.as_deref(), Some("push" | "pr")) {
            let message = crate::git::git(&mirror, &["log", "-1", "--format=%B", &req.sha])
                .await
                .unwrap_or_default();
            if let Some(marker) = skip_marker(&message) {
                // Only if still active: it may have been superseded meanwhile.
                if self
                    .db
                    .skip_run_because(run_id, &format!("{marker} in the commit message"))?
                {
                    self.run_finished(run_id).await;
                }
                return Ok(());
            }
        }
        if let Some(base) = self.base_commit(&mirror, repo, &req.sha).await {
            self.db.set_run_base(run_id, &base)?;
        }
        let overlay = req
            .overlay
            .as_deref()
            .map(|d| self.load_overlay(d))
            .transpose()?;
        let config = self
            .load_config(
                repo,
                &mirror,
                &req.sha,
                overlay.as_ref(),
                // `trusted` as `create_run` stored it (never with an overlay).
                req.no_secrets || self.config_sandboxed(repo, req.trusted && req.overlay.is_none()),
            )
            .await?;
        let t_config = t0.elapsed();
        if req.draft && config.drafts == Some(false) {
            if self.db.skip_run_because(
                run_id,
                "draft pull request: runs when it is marked ready for review (`drafts: false`)",
            )? {
                self.run_finished(run_id).await;
            }
            return Ok(());
        }
        // `on`: which groups this kind of run includes. Tasks named on the
        // command line or in a schedule always run; a merge queue lane names
        // its tasks too, and still follows `on`.
        let run_row = self.db.run(run_id)?;
        let trigger = req.trigger.as_deref().unwrap_or("api");
        let kinds: Vec<String> = match &req.counts_as {
            Some(k) => k.clone(),
            None => run_kinds(trigger, run_row.trusted, req.git_ref.as_deref())
                .into_iter()
                .map(String::from)
                .collect(),
        };
        let kind_refs: Vec<&str> = kinds.iter().map(String::as_str).collect();
        self.db.set_run_kinds(run_id, &kind_refs)?;
        let filter: &[&str] = if !req.tasks.is_empty() && trigger != "queue" {
            &[]
        } else {
            &kind_refs
        };
        let kinds = filter;
        let mut groups = crate::graph::plan_for(&config, &req.tasks, kinds)?;
        let run = self.db.run(run_id)?;
        if run.status.is_done() {
            return Ok(()); // Cancelled while planning.
        }
        let trusted = run.trusted;
        let scope = self.cache_scope(&repo.id, repo.github.is_some(), trusted);
        if groups.is_empty() {
            self.db
                .set_run_status(run_id, Status::Passed, Some("no tasks to run"))?;
            self.run_finished(run_id).await;
            return Ok(());
        }
        let sharded = groups
            .iter()
            .any(|g| g.not_in.is_none() && g.tasks.iter().any(|(_, t)| !t.shard_files.is_empty()));
        let index = if sharded || !req.no_cache {
            match resolve::file_index(&mirror, &req.sha, overlay.as_ref()).await {
                Ok(index) => Some(index),
                // Without the files there is nothing to deal, and shards
                // with no files pass without running.
                Err(e) if sharded => return Err(e.context("listing the files to deal to shards")),
                Err(_) => None,
            }
        } else {
            None
        };
        if let (true, Some(index)) = (sharded, &index) {
            shards::plan(self, &repo.id, &mut groups, index)?;
        }
        // Groups the cache answers completely never need a worker.
        let files = if req.no_cache { None } else { index };
        let mut resolved = 0;
        for g in &groups {
            if let Some(why) = &g.not_in {
                let skipped: Vec<TaskResult> = g
                    .tasks
                    .iter()
                    .map(|_| TaskResult {
                        status: Status::Skipped,
                        exit_code: None,
                        action_key: None,
                        duration_ms: 0,
                        log_digest: None,
                        message: Some(why.clone()),
                    })
                    .collect();
                let gid = self
                    .db
                    .insert_finished_group(run_id, &g.name, &g.os, &g.tasks, &skipped, None, why)?;
                self.emit_cached_group(run_id, &gid)?;
                resolved += 1;
                continue;
            }
            if let Some(results) = files
                .as_deref()
                .map(|f| resolve::resolve(self, g, f, &scope))
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
            // A fork's tasks run in containers only, and services are
            // containers.
            if req.no_secrets || g.tasks.iter().any(|(_, t)| !t.services.is_empty()) {
                backends.push(Backend::Docker);
            }
            backends.sort_by_key(|b| b.as_str());
            backends.dedup();
            // Locks are per repo: one repo's runs cannot hold another's.
            let lock = g.lock.as_ref().map(|l| format!("{}:{l}", repo.id));
            self.db.insert_locked_group(
                run_id,
                &g.name,
                &g.os,
                &g.arch,
                g.min_cpus,
                &backends,
                &g.tasks,
                lock.as_deref(),
            )?;
        }
        self.db.set_run_planned(run_id)?;
        tracing::debug!(
            "planned {run_id} in {:?} (commit {:?}, config {:?}, rest {:?})",
            t0.elapsed(),
            t_commit,
            t_config - t_commit,
            t0.elapsed() - t_config
        );
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
            serde_json::json!({ "name": g.name, "worker": g.worker }),
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
    pub fn supersedes_for_test(trigger: &str, git_ref: &str, trusted: bool) -> bool {
        supersedes(trigger, git_ref, trusted)
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
/// Whether a newer run of this ref cancels the older ones. Never for a
/// trusted run (a default-branch push, whatever the branch is called): a
/// deploy of every commit must run, one after another under a `lock`.
fn supersedes(trigger: &str, git_ref: &str, trusted: bool) -> bool {
    match trigger {
        "pr" | "local" => true,
        "push" => !trusted && !matches!(git_ref, "main" | "master" | "trunk"),
        _ => false,
    }
}

pub async fn serve(opts: ServerOpts) -> Result<()> {
    let public_url = opts
        .public_url
        .clone()
        .unwrap_or_else(|| format!("http://{}", opts.addr));
    let app = App::new(&opts.data_dir, opts.token.clone(), public_url)?;
    if opts.token.is_none() && !opts.addr.ip().is_loopback() {
        tracing::warn!(
            "listening on {} without PIT_TOKEN: anyone who can reach it can start runs, \
             and their pit.config.ts is evaluated on this host; set PIT_TOKEN",
            opts.addr
        );
    }
    *app.worker_token.lock() = opts.worker_token.clone();
    *app.notify_url.lock() = opts.notify_url.clone();
    app.cache_max_bytes
        .store(opts.cache_max_bytes, std::sync::atomic::Ordering::Relaxed);
    if opts.oidc {
        app.enable_oidc()?;
    }
    serve_app(app, opts.addr).await
}

pub async fn serve_app(app: Arc<App>, addr: SocketAddr) -> Result<()> {
    let listener = tokio::net::TcpListener::bind(addr)
        .await
        .with_context(|| format!("binding {addr}"))?;
    serve_listener(app, listener).await
}

/// The kinds of run (`GroupDef::on`) a run counts as.
pub fn run_kinds(trigger: &str, trusted: bool, git_ref: Option<&str>) -> Vec<&'static str> {
    let pr = git_ref.is_some_and(|r| r.starts_with("pull/"));
    match trigger {
        "pr" | "retest" => vec!["pr"],
        "rerun" if pr => vec!["pr"],
        "push" | "rerun" if trusted => vec!["push", "main"],
        "push" | "rerun" => vec!["push"],
        "post-merge" if trusted => vec!["push", "main"],
        "post-merge" => vec!["push"],
        "queue" => vec!["queue"],
        t if t.starts_with("schedule:") => vec!["schedule"],
        // `pit run`, the API, MCP.
        _ => vec!["local"],
    }
}

/// Why no online worker can take a group, if none can: the GitHub "Waiting
/// for a runner to pick up this job" that never says what is missing.
pub fn waiting_reason(
    os: &str,
    arch: &str,
    min_cpus: u32,
    needs: &[Backend],
    online: &[WorkerInfo],
) -> Option<String> {
    let fits = |w: &WorkerInfo| {
        (os == "any" || w.os == os)
            && (arch == "any" || crate::model::canonical_arch(&w.arch) == arch)
            && w.cpus >= min_cpus as usize
            && needs.iter().all(|b| w.backends.contains(b))
    };
    if online.iter().any(fits) {
        return None;
    }
    let mut need = vec![format!("os {os}")];
    if arch != "any" {
        need.push(format!("arch {arch}"));
    }
    if min_cpus > 0 {
        need.push(format!("{min_cpus}+ cpus"));
    }
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
                format!(
                    "{} ({} {}, {} cpus; {})",
                    w.name,
                    w.os,
                    w.arch,
                    w.cpus,
                    b.join(", ")
                )
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
            let reason =
                waiting_reason(&g.os, &g.arch, g.min_cpus, &g.needs, &online).or_else(|| {
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

/// The skip marker in a commit message, if any: GitHub's, plus `[skip pit]`.
/// Case-insensitive.
fn skip_marker(message: &str) -> Option<&'static str> {
    const MARKERS: &[&str] = &[
        "[skip ci]",
        "[ci skip]",
        "[no ci]",
        "[skip actions]",
        "[actions skip]",
        "[skip pit]",
    ];
    let lower = message.to_ascii_lowercase();
    MARKERS.iter().copied().find(|m| lower.contains(m))
}

/// `PIT_CONFIG_<OWNER>_<NAME>`: a `pit.config.json` on the server for that
/// GitHub repo, used when a commit has no config of its own.
pub fn operator_config(full_name: &str) -> Option<PathBuf> {
    let key = format!(
        "PIT_CONFIG_{}",
        full_name.replace(['/', '-', '.'], "_").to_uppercase()
    );
    std::env::var_os(key).map(PathBuf::from)
}

/// `PIT_HISTORY_DAYS`: how long finished runs are kept (default 90; `0`
/// forever, returned as `None`).
pub fn history_days() -> Option<i64> {
    let days: i64 = std::env::var("PIT_HISTORY_DAYS")
        .ok()
        .and_then(|d| d.parse().ok())
        .unwrap_or(90);
    (days > 0).then_some(days)
}

/// Finished runs are kept `PIT_HISTORY_DAYS` (default 90, `0` forever); their
/// logs only 14 days anyway. Without this the database grows with every run.
fn prune_history(app: &App) {
    if let Some(days) = history_days() {
        match app.db.prune_runs(now_ms() - days * 86_400_000) {
            Ok(0) => {}
            Ok(n) => tracing::info!("deleted {n} run(s) older than {days} days"),
            Err(e) => tracing::warn!("pruning old runs: {e:#}"),
        }
    }
    match app.db.reclaim_space() {
        Ok(0) => {}
        Ok(b) => tracing::info!("gave {:.1} MB of database space back", b as f64 / 1e6),
        Err(e) => tracing::warn!("reclaiming database space: {e:#}"),
    }
}

pub async fn serve_listener(app: Arc<App>, listener: tokio::net::TcpListener) -> Result<()> {
    // Webhooks missed while down: caught up shortly after start, then every
    // ten minutes (for deliveries that failed while it was up).
    if app.github.is_some() {
        let app = app.clone();
        tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_secs(10)).await;
            let mut at_start = true;
            loop {
                let pass = github::reconcile(&app, at_start).await;
                at_start = false;
                match pass {
                    Ok(0) => {}
                    Ok(n) => tracing::info!("caught up on {n} missed push(es) and pull request(s)"),
                    Err(e) => tracing::warn!("catching up on missed webhooks: {e:#}"),
                }
                tokio::time::sleep(std::time::Duration::from_secs(600)).await;
            }
        });
    }
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
            match tokio::task::spawn_blocking(move || {
                prune_history(&app);
                gc::collect(&app, false)
            })
            .await
            {
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
mod config_sandbox_tests {
    use super::*;

    #[test]
    fn skip_markers_are_found_anywhere_any_case() {
        assert_eq!(
            skip_marker("chore(release): 1.0.1 [skip ci]"),
            Some("[skip ci]")
        );
        assert_eq!(skip_marker("docs\n\n[CI SKIP]"), Some("[ci skip]"));
        assert_eq!(skip_marker("bump [skip pit]"), Some("[skip pit]"));
        assert_eq!(skip_marker("skip ci in prose, no brackets"), None);
        assert_eq!(skip_marker("fix: [skip] tests"), None);
    }

    #[test]
    fn shared_servers_evaluate_only_trusted_github_configs_on_the_host() {
        let dir = tempfile::tempdir().unwrap();
        let repo = |github: Option<&str>| db::Repo {
            id: "r".into(),
            name: "r".into(),
            url: "u".into(),
            github: github.map(String::from),
        };
        let (gh, local) = (repo(Some("acme/web")), repo(None));
        let shared = App::with_github(
            &dir.path().join("a"),
            Some("tok".into()),
            "http://x".into(),
            None,
        )
        .unwrap();
        assert!(
            !shared.config_sandboxed(&gh, true),
            "main of a webhook repo"
        );
        assert!(shared.config_sandboxed(&gh, false), "an API run or overlay");
        assert!(
            shared.config_sandboxed(&local, true),
            "a repo any API caller registered"
        );
        // `pit up` without a token: its one user already runs the server.
        let solo = App::with_github(&dir.path().join("b"), None, "http://x".into(), None).unwrap();
        assert!(!solo.config_sandboxed(&local, false));
    }
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
            protocol: crate::model::PROTOCOL,
        };
        assert_eq!(
            waiting_reason(
                "linux",
                "any",
                0,
                &[Backend::Host],
                std::slice::from_ref(&linux)
            ),
            None
        );
        assert_eq!(
            waiting_reason("any", "any", 0, &[], std::slice::from_ref(&linux)),
            None
        );
        let mac = waiting_reason(
            "macos",
            "any",
            0,
            &[Backend::Host],
            std::slice::from_ref(&linux),
        )
        .unwrap();
        assert!(
            mac.contains("needs os macos")
                && mac.contains("local (linux x86_64, 8 cpus; host, sandbox)"),
            "{mac}"
        );
        let arm = waiting_reason("linux", "aarch64", 0, &[], std::slice::from_ref(&linux)).unwrap();
        assert!(arm.contains("needs os linux, arch aarch64"), "{arm}");
        let big = waiting_reason("linux", "any", 16, &[], std::slice::from_ref(&linux)).unwrap();
        assert!(big.contains("needs os linux, 16+ cpus"), "{big}");
        assert_eq!(
            waiting_reason("linux", "any", 8, &[], std::slice::from_ref(&linux)),
            None
        );
        let docker = waiting_reason("linux", "any", 0, &[Backend::Docker], &[linux]).unwrap();
        assert!(docker.contains("backend docker"), "{docker}");
        assert!(waiting_reason("linux", "any", 0, &[], &[])
            .unwrap()
            .contains("no workers are online"));
    }
}

#[cfg(test)]
mod kind_tests {
    use super::run_kinds;

    #[test]
    fn runs_count_as_the_kinds_on_names() {
        assert_eq!(run_kinds("pr", false, Some("pull/3")), ["pr"]);
        assert_eq!(run_kinds("retest", false, Some("pull/3")), ["pr"]);
        assert_eq!(run_kinds("rerun", false, Some("pull/3")), ["pr"]);
        assert_eq!(run_kinds("push", true, Some("main")), ["push", "main"]);
        assert_eq!(run_kinds("push", false, Some("feature")), ["push"]);
        assert_eq!(
            run_kinds("post-merge", true, Some("main")),
            ["push", "main"]
        );
        assert_eq!(run_kinds("post-merge", false, Some("release/x")), ["push"]);
        assert_eq!(run_kinds("queue", true, Some("queue/pr/3")), ["queue"]);
        assert_eq!(
            run_kinds("schedule:nightly", true, Some("main")),
            ["schedule"]
        );
        assert_eq!(run_kinds("local", false, None), ["local"]);
    }
}
