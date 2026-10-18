//! The worker daemon. It keeps warm state between jobs: a bare mirror per repo,
//! one persistent workspace per slot (ignored files such as `node_modules` and
//! `target` survive between runs), and a node-local blob cache.
//!
//! A job is a whole group of tasks. They run in one workspace, in dependency
//! order, with independent tasks in parallel.

pub mod debug;
pub mod disk;
pub mod exec;
pub mod files;
pub mod junit;
pub mod mask;
pub mod memory;
pub mod services;

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use parking_lot::Mutex;
use tokio::sync::mpsc;
use tokio::task::JoinSet;

use crate::cas::Cas;
use crate::client::Client;
use crate::git;
use crate::model::*;

pub struct WorkerOpts {
    pub server: String,
    pub token: Option<String>,
    pub name: Option<String>,
    pub data_dir: PathBuf,
    /// Groups this worker runs at the same time.
    pub slots: usize,
    /// Tasks run at the same time within one group.
    pub parallelism: usize,
    /// Disk budget for warm workspaces; least recently used idle ones are
    /// removed past it. 0 means no size limit.
    pub workspace_max_bytes: u64,
    /// Sent when claiming jobs, if the server requires one.
    pub worker_token: Option<String>,
    /// `KEY=VALUE` lines. These values are given only to tasks that list the
    /// key in `secrets`, and never to fork PR runs; unlike variables in the
    /// worker's own environment, no other task can read them.
    pub secrets_file: Option<PathBuf>,
}

pub struct Worker {
    client: Client,
    info: WorkerInfo,
    data_dir: PathBuf,
    cas: Cas,
    parallelism: usize,
    mirror_locks: Mutex<HashMap<String, Arc<tokio::sync::Mutex<()>>>>,
    workspaces: Mutex<HashMap<String, Vec<WorkspaceSlot>>>,
    workspace_max_bytes: u64,
    /// See `WorkerOpts::secrets_file`.
    secrets: HashMap<String, String>,
    /// Set by SIGTERM or Ctrl-C: claim nothing new, finish what is running.
    draining: std::sync::atomic::AtomicBool,
    /// Groups running now, by id: their lease, to hand back on a hard stop,
    /// and the switch that stops their tasks.
    running: Mutex<HashMap<String, Running>>,
    /// Service containers of the groups running now, by lease: started by
    /// the first task that runs (not a cache hit), shared by the rest.
    services: Mutex<HashMap<String, ServiceSlot>>,
    /// Identifies this worker's data dir across restarts, unlike its name
    /// (the hostname by default, shared by workers on one machine). Its
    /// service containers are labelled with it.
    instance: String,
    /// `runtime` command outputs, per job (`group id \0 command`).
    runtime_memo: Mutex<HashMap<String, String>>,
}

/// A group in progress: its lease and the switch that stops its tasks.
type Running = (String, Arc<tokio::sync::watch::Sender<bool>>);

/// A persistent workspace. Jobs get the warmest free one for their repo.
struct WorkspaceSlot {
    path: PathBuf,
    busy: bool,
    last_used: Instant,
}

/// Returns its workspace to the pool when the job ends. A fresh workspace
/// leaves the pool instead; the caller deletes it.
struct WorkspaceLease<'a> {
    worker: &'a Worker,
    pool: String,
    path: PathBuf,
    fresh: bool,
}

impl Drop for WorkspaceLease<'_> {
    fn drop(&mut self) {
        self.worker
            .release_workspace(&self.pool, &self.path, self.fresh);
    }
}

impl Worker {
    fn release_workspace(&self, pool: &str, path: &Path, fresh: bool) {
        if let Some(slots) = self.workspaces.lock().get_mut(pool) {
            if fresh {
                slots.retain(|s| s.path != path);
                return;
            }
            if let Some(s) = slots.iter_mut().find(|s| s.path == path) {
                s.busy = false;
                s.last_used = Instant::now();
            }
        }
        disk::touch_used(path);
    }
}

pub async fn run(opts: WorkerOpts) -> Result<()> {
    // Debug scripts from an earlier process point at workspaces it no longer
    // holds; the next job may check out over them.
    let _ = std::fs::remove_dir_all(opts.data_dir.join("debug"));
    let _ = std::fs::remove_dir_all(opts.data_dir.join("summaries"));
    let mut worker = Worker::new(
        opts.server.as_str(),
        opts.token.clone(),
        opts.name.clone(),
        &opts.data_dir,
        opts.parallelism,
    )
    .await?;
    worker.workspace_max_bytes = opts.workspace_max_bytes;
    if let Some(path) = &opts.secrets_file {
        let text = std::fs::read_to_string(path)
            .with_context(|| format!("reading secrets file {}", path.display()))?;
        worker.secrets = parse_secrets(&text);
        tracing::info!("{} secret(s) from {}", worker.secrets.len(), path.display());
    }
    worker.client = worker.client.with_worker_token(opts.worker_token.clone());
    if worker.info.backends.contains(&Backend::Docker) {
        services::clean_up_after(&worker.instance).await;
    }
    let worker = Arc::new(worker);
    tracing::info!(
        "pit worker {} ({} {}, {} cpus, backends: {}) serving {} with {} slot(s)",
        worker.info.name,
        worker.info.os,
        worker.info.arch,
        worker.info.cpus,
        worker
            .info
            .backends
            .iter()
            .map(|b| b.as_str())
            .collect::<Vec<_>>()
            .join(", "),
        opts.server,
        opts.slots,
    );
    // Autoscalers stop workers with SIGTERM. The first signal drains: no new
    // claims, running groups finish, then the process exits. A second
    // signal hands the running groups back to the server and exits at once.
    {
        let w = worker.clone();
        tokio::spawn(async move {
            // One listener for both signals: a second SIGTERM that arrives
            // before a new listener would exist must not be lost.
            let mut signals = Shutdown::new();
            signals.next().await;
            w.draining.store(true, std::sync::atomic::Ordering::SeqCst);
            let n = w.running.lock().len();
            tracing::info!("draining: finishing {n} running group(s), claiming nothing new");
            signals.next().await;
            let running: Vec<(String, Running)> = w.running.lock().drain().collect();
            // Hand the groups back first: the server requeues them and
            // revokes their leases, so the dying jobs cannot report a result.
            for (gid, (lease, _)) in &running {
                let _ = w
                    .client
                    .with_lease(lease)
                    .post_empty(&format!("/api/groups/{gid}/release"), &())
                    .await;
            }
            // Then stop their tasks (whole process groups), so nothing keeps
            // running in a workspace nobody owns, and give them a moment.
            for (_, (_, cancel)) in &running {
                let _ = cancel.send(true);
            }
            // Their services go too, not left for the next start.
            let started: Vec<Arc<services::Running>> = w
                .services
                .lock()
                .values()
                .filter_map(|slot| slot.get().and_then(|r| r.as_ref().ok().cloned()))
                .collect();
            for s in started {
                s.stop().await;
            }
            tokio::time::sleep(Duration::from_secs(3)).await;
            tracing::info!("stopped: running groups handed back to the server");
            std::process::exit(130);
        });
    }
    let mut loops = JoinSet::new();
    {
        let w = worker.clone();
        loops.spawn(async move { w.workspace_gc_loop().await });
    }
    for slot in 0..opts.slots.max(1) {
        let w = worker.clone();
        loops.spawn(async move { w.claim_loop(slot).await });
    }
    // Claim loops end only when draining; the workspace cleanup loop never
    // does, so wait for the claim loops alone.
    let slots = opts.slots.max(1);
    let mut done = 0;
    while let Some(_r) = loops.join_next().await {
        done += 1;
        if done == slots {
            break;
        }
    }
    tracing::info!("drained; exiting");
    Ok(())
}

/// SIGTERM or Ctrl-C, as a stream: each call waits for the next one.
struct Shutdown {
    #[cfg(unix)]
    term: Option<tokio::signal::unix::Signal>,
}

impl Shutdown {
    fn new() -> Shutdown {
        Shutdown {
            #[cfg(unix)]
            term: tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()).ok(),
        }
    }

    async fn next(&mut self) {
        #[cfg(unix)]
        {
            let term = async {
                match self.term.as_mut() {
                    Some(t) => {
                        t.recv().await;
                    }
                    None => std::future::pending::<()>().await,
                }
            };
            tokio::select! {
                _ = term => {}
                _ = tokio::signal::ctrl_c() => {}
            }
        }
        #[cfg(not(unix))]
        {
            let _ = tokio::signal::ctrl_c().await;
        }
    }
}

fn safe_name(s: &str) -> String {
    s.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '.' {
                c
            } else {
                '_'
            }
        })
        .collect()
}

impl Worker {
    pub async fn new(
        server: &str,
        token: Option<String>,
        name: Option<String>,
        data_dir: &Path,
        parallelism: usize,
    ) -> Result<Worker> {
        std::fs::create_dir_all(data_dir)?;
        let host = std::env::var("HOSTNAME")
            .ok()
            .or_else(|| std::env::var("COMPUTERNAME").ok())
            .unwrap_or_else(|| "worker".into());
        let info = WorkerInfo {
            name: name.unwrap_or(host),
            os: current_os().to_string(),
            arch: std::env::consts::ARCH.to_string(),
            cpus: num_cpus::get(),
            backends: exec::detect_backends().await,
            kvm: exec::kvm_available(),
        };
        Ok(Worker {
            client: Client::new(server, token)?,
            info,
            data_dir: data_dir.to_path_buf(),
            cas: Cas::open(data_dir.join("cas"))?,
            parallelism: parallelism.max(1),
            mirror_locks: Default::default(),
            workspaces: Default::default(),
            workspace_max_bytes: 0,
            secrets: Default::default(),
            draining: Default::default(),
            running: Default::default(),
            services: Default::default(),
            instance: instance_id(data_dir),
            runtime_memo: Default::default(),
        })
    }

    /// The most recently used free workspace for this repo and group, so the
    /// group's dependencies and build output from its last run are still
    /// there. Groups do different work (a Rust build, a JS install), so they
    /// never share warm state. Neither do trusted and untrusted runs: files
    /// a branch leaves behind (a planted `node_modules` entry) must not be
    /// there when main runs. Creates a workspace if all are busy.
    fn lease_workspace(&self, repo: &str, group: &str, trusted: bool) -> WorkspaceLease<'_> {
        // Both are suffixed: workspaces from before trust existed were
        // shared with branch runs, so neither pool reuses them (they age out,
        // see `disk`).
        let pool = format!(
            "{}~{}",
            safe_name(group),
            if trusted { "trusted" } else { "untrusted" }
        );
        let dir = self
            .data_dir
            .join("workspaces")
            .join(safe_name(repo))
            .join(pool);
        let key = dir.to_string_lossy().to_string();
        let mut pools = self.workspaces.lock();
        let slots = pools.entry(key.clone()).or_default();
        if slots.is_empty() {
            // Pick up workspaces left by a previous worker process.
            if let Ok(entries) = std::fs::read_dir(&dir) {
                // Slots are numbered; anything else is not ours.
                let numbered = entries
                    .flatten()
                    .filter(|e| e.file_name().to_string_lossy().parse::<usize>().is_ok());
                for e in numbered {
                    slots.push(WorkspaceSlot {
                        path: e.path(),
                        busy: false,
                        last_used: Instant::now(),
                    });
                }
            }
        }
        let path = match slots
            .iter_mut()
            .filter(|s| !s.busy)
            .max_by_key(|s| s.last_used)
        {
            Some(s) => {
                s.busy = true;
                s.path.clone()
            }
            None => {
                let index = (0..)
                    .find(|i: &usize| {
                        !slots
                            .iter()
                            .any(|s| s.path.file_name() == Some(i.to_string().as_ref()))
                    })
                    .unwrap_or_default();
                let path = dir.join(index.to_string());
                slots.push(WorkspaceSlot {
                    path: path.clone(),
                    busy: true,
                    last_used: Instant::now(),
                });
                path
            }
        };
        WorkspaceLease {
            worker: self,
            pool: key,
            path,
            fresh: false,
        }
    }

    /// A new, empty workspace for one job (`pit run --fresh`, `pit verify`).
    /// It is listed as busy, so workspace cleanup leaves it alone while the
    /// job runs.
    fn lease_fresh_workspace(&self, repo: &str, group: &str) -> WorkspaceLease<'_> {
        let dir = self
            .data_dir
            .join("workspaces")
            .join(safe_name(repo))
            .join(format!("{}~fresh", safe_name(group)));
        let key = dir.to_string_lossy().to_string();
        let path = dir.join(uuid::Uuid::new_v4().simple().to_string());
        self.workspaces
            .lock()
            .entry(key.clone())
            .or_default()
            .push(WorkspaceSlot {
                path: path.clone(),
                busy: true,
                last_used: Instant::now(),
            });
        WorkspaceLease {
            worker: self,
            pool: key,
            path,
            fresh: true,
        }
    }

    /// Removes stale workspaces, and least recently used ones past the disk
    /// budget, every ten minutes (see `disk`).
    async fn workspace_gc_loop(self: Arc<Self>) {
        loop {
            let w = self.clone();
            match tokio::task::spawn_blocking(move || w.collect_workspaces()).await {
                Ok(Ok(())) => {}
                Ok(Err(e)) => tracing::warn!("workspace cleanup: {e:#}"),
                Err(e) => tracing::warn!("workspace cleanup: {e}"),
            }
            tokio::time::sleep(Duration::from_secs(600)).await;
        }
    }

    fn collect_workspaces(&self) -> Result<()> {
        let root = self.data_dir.join("workspaces");
        let trash = root.join(".trash");
        // Left over from a cleanup that was interrupted.
        let _ = std::fs::remove_dir_all(&trash);
        let found = disk::scan(&root);
        let now = std::time::SystemTime::now();
        let mut doomed = Vec::new();
        {
            // Holding the pool lock: nothing can lease a workspace while it
            // is chosen and moved out of the way.
            let mut pools = self.workspaces.lock();
            let busy = |p: &Path| pools.values().flatten().any(|s| s.busy && s.path == p);
            let chosen = disk::plan(&found, busy, self.workspace_max_bytes, now);
            for path in chosen {
                let dest = trash.join(uuid::Uuid::new_v4().simple().to_string());
                std::fs::create_dir_all(&trash)?;
                if std::fs::rename(&path, &dest).is_err() {
                    continue;
                }
                for slots in pools.values_mut() {
                    slots.retain(|s| s.path != path);
                }
                disk::remove_meta(&path);
                doomed.push((path, dest));
            }
        }
        let total: u64 = found.iter().map(|w| w.bytes).sum();
        let mut freed = 0;
        for (path, dest) in doomed {
            let w = found.iter().find(|w| w.path == path);
            let bytes = w.map(|w| w.bytes).unwrap_or_default();
            let idle_days = w
                .and_then(|w| now.duration_since(w.last_used).ok())
                .map(|d| d.as_secs() / 86_400)
                .unwrap_or_default();
            tracing::info!(
                "removed workspace {} ({:.1} GB, unused for {idle_days} day(s))",
                path.strip_prefix(&root).unwrap_or(&path).display(),
                bytes as f64 / 1e9,
            );
            std::fs::remove_dir_all(&dest)?;
            freed += bytes;
        }
        if freed > 0 {
            tracing::info!(
                "workspaces: {:.1} GB → {:.1} GB",
                total as f64 / 1e9,
                (total - freed) as f64 / 1e9
            );
        }
        Ok(())
    }

    async fn claim_loop(self: Arc<Self>, slot: usize) {
        let mut backoff = Duration::from_millis(500);
        loop {
            if self.draining.load(std::sync::atomic::Ordering::SeqCst) {
                return;
            }
            match self.client.claim(&self.info).await {
                // A claim long-polls for up to 20s: one that was in flight
                // when draining began goes straight back to the queue.
                Ok(ClaimResponse::Job(job))
                    if self.draining.load(std::sync::atomic::Ordering::SeqCst) =>
                {
                    let _ = self
                        .client
                        .with_lease(&job.lease)
                        .post_empty(&format!("/api/groups/{}/release", job.group_id), &())
                        .await;
                    tracing::info!("draining: handed back {} without starting it", job.group);
                    return;
                }
                Ok(ClaimResponse::Job(job)) => {
                    backoff = Duration::from_millis(500);
                    let label = format!("{} {} [{}]", job.run_id, job.group, job.repo);
                    tracing::info!("slot {slot}: running {label}");
                    match self.clone().execute(*job).await {
                        Ok(status) => tracing::info!("slot {slot}: {label} {}", status.as_str()),
                        Err(e) => tracing::warn!("slot {slot}: {label} failed to report: {e:#}"),
                    }
                }
                Ok(ClaimResponse::Idle) => {}
                Err(e) => {
                    tracing::warn!("claim failed: {e:#}");
                    tokio::time::sleep(backoff).await;
                    backoff = (backoff * 2).min(Duration::from_secs(10));
                }
            }
        }
    }

    pub async fn execute(self: Arc<Self>, job: Job) -> Result<Status> {
        let (cancel_tx, cancel_rx) = tokio::sync::watch::channel(false);
        let cancel_tx = Arc::new(cancel_tx);
        self.running
            .lock()
            .insert(job.group_id.clone(), (job.lease.clone(), cancel_tx.clone()));
        let result = self.clone().execute_job(&job, cancel_tx, cancel_rx).await;
        self.running.lock().remove(&job.group_id);
        result
    }

    async fn execute_job(
        self: Arc<Self>,
        job: &Job,
        cancel_tx: Arc<tokio::sync::watch::Sender<bool>>,
        cancel_rx: tokio::sync::watch::Receiver<bool>,
    ) -> Result<Status> {
        let job = job.clone();
        // Every update for this job carries its lease, so a worker that lost
        // the group (reaped, then re-claimed elsewhere) cannot overwrite it.
        let client = self.client.with_lease(&job.lease);
        // A heartbeat refused with 409 means the group is no longer ours: the
        // run was cancelled or superseded (or we were presumed dead). Then the
        // running tasks are stopped.
        let hb = {
            let client = client.clone();
            let gid = job.group_id.clone();
            tokio::spawn(async move {
                loop {
                    tokio::time::sleep(Duration::from_secs(2)).await;
                    if let Err(e) = client
                        .post_empty(&format!("/api/groups/{gid}/heartbeat"), &())
                        .await
                    {
                        if e.to_string().contains("409") {
                            let _ = cancel_tx.send(true);
                            return;
                        }
                    }
                }
            })
        };
        let lease = if job.fresh {
            self.lease_fresh_workspace(&job.repo, &job.group)
        } else {
            self.lease_workspace(&job.repo, &job.group, job.trusted)
        };
        let ws = lease.path.clone();
        let prep_start = Instant::now();
        let prepared = self.prepare(&job, &ws).await;
        let prepare_ms = prep_start.elapsed().as_millis() as u64;
        let result = match prepared {
            Err(e) => GroupResult {
                status: Status::Error,
                message: Some(format!("preparing workspace: {e:#}")),
                prepare_ms,
            },
            Ok((files, note)) => {
                if job.tasks.iter().any(|t| !t.def.services.is_empty()) {
                    self.services
                        .lock()
                        .insert(job.lease.clone(), Default::default());
                }
                let status = self
                    .clone()
                    .run_tasks(&job, &ws, Arc::new(files), cancel_rx)
                    .await;
                GroupResult {
                    status,
                    message: note,
                    prepare_ms,
                }
            }
        };
        let services = self
            .services
            .lock()
            .remove(&job.lease)
            .and_then(|slot| slot.get().and_then(|r| r.as_ref().ok().cloned()));
        let summaries = self.data_dir.join("summaries").join(&job.lease);
        hb.abort();
        let debug_dir = debug::dir(&self.data_dir, &job.group_id);
        if job.debug && debug::has_scripts(&debug_dir) {
            // Keep the failed workspace as it is for the debug scripts: still
            // busy, so no other job checks out over it.
            let (pool, fresh) = (lease.pool.clone(), lease.fresh);
            std::mem::forget(lease);
            let (w, ws) = (self.clone(), ws.clone());
            tokio::spawn(async move {
                debug::hold(&debug_dir, debug::HOLD).await;
                // The debug shell writes `$PIT_SUMMARY` there too.
                let _ = std::fs::remove_dir_all(&summaries);
                // The debug shell can still reach the services until then.
                if let Some(s) = services {
                    s.stop().await;
                }
                w.release_workspace(&pool, &ws, fresh);
                if fresh {
                    disk::remove_meta(&ws);
                    let _ = tokio::task::spawn_blocking(move || std::fs::remove_dir_all(ws)).await;
                }
            });
        } else {
            let _ = std::fs::remove_dir_all(&summaries);
            if let Some(s) = services {
                s.stop().await;
            }
            drop(lease);
            if job.fresh {
                let ws = ws.clone();
                disk::remove_meta(&ws);
                tokio::task::spawn_blocking(move || std::fs::remove_dir_all(ws));
            }
        }
        let prefix = format!("{}\0", job.group_id);
        self.runtime_memo
            .lock()
            .retain(|k, _| !k.starts_with(&prefix));
        let status = result.status;
        client
            .post_empty(&format!("/api/groups/{}/finish", job.group_id), &result)
            .await?;
        Ok(status)
    }

    fn mirror_lock(&self, repo: &str) -> Arc<tokio::sync::Mutex<()>> {
        self.mirror_locks
            .lock()
            .entry(repo.to_string())
            .or_default()
            .clone()
    }

    /// Gets the workspace to exactly `sha` plus the overlay, keeping ignored
    /// files warm. Returns the input file index used for action keys, and a
    /// note for the group's result.
    async fn prepare(
        &self,
        job: &Job,
        ws: &Path,
    ) -> Result<(Vec<(String, String)>, Option<String>)> {
        let mirror = self
            .data_dir
            .join("mirrors")
            .join(format!("{}.git", safe_name(&job.repo)));
        // Git LFS: the mirror fetches the commit's large files, once per
        // worker, and every workspace reads them from there. Each checkout
        // would otherwise download its own copy (CI checkouts are what use
        // up an LFS bandwidth quota), and a workspace cloned from the mirror
        // cannot download them at all: its remote is the mirror.
        let mut note = None;
        let mut lfs = false;
        {
            let lock = self.mirror_lock(&job.repo);
            let _g = lock.lock().await;
            git::ensure_commit(&mirror, &job.url, &job.sha).await?;
            if git::uses_lfs(&mirror, &job.sha).await {
                let skip = std::env::var_os("GIT_LFS_SKIP_SMUDGE").is_some_and(|v| v == "1");
                if !skip && git::lfs_available().await {
                    git::git(&mirror, &["lfs", "fetch", "origin", &job.sha])
                        .await
                        .context("fetching Git LFS files")?;
                    lfs = true;
                } else {
                    note = Some(
                        "this commit uses Git LFS, but git-lfs is not installed on this worker \
                         (or GIT_LFS_SKIP_SMUDGE=1): LFS files are pointer files"
                            .to_string(),
                    );
                }
            }
        }
        if !ws.join(".git").exists() {
            std::fs::create_dir_all(ws.parent().unwrap())?;
            let ws_str = ws.to_string_lossy().to_string();
            let mirror_str = mirror.to_string_lossy().to_string();
            // --shared borrows the mirror's objects: no copy, instant clone.
            git::git(
                &self.data_dir,
                &[
                    "clone",
                    "-q",
                    "--shared",
                    "--no-checkout",
                    &mirror_str,
                    &ws_str,
                ],
            )
            .await?;
        }
        if lfs {
            let storage = mirror.join("lfs");
            git::git(ws, &["lfs", "install", "--local", "--skip-repo"]).await?;
            git::git(ws, &["config", "lfs.storage", &storage.to_string_lossy()]).await?;
        }
        git::git(ws, &["checkout", "-q", "--force", "--detach", &job.sha]).await?;
        // Untracked files go, ignored ones (dependencies, build dirs) stay warm.
        git::git(ws, &["clean", "-q", "-ffd"]).await?;
        if ws.join(".gitmodules").exists() {
            if job.no_secrets {
                // A fork's `.gitmodules` could point at another repo's mirror
                // on this worker, or use its credentials.
                let msg = "submodules are not fetched for pull requests from forks";
                note = Some(match note.take() {
                    Some(n) => format!("{n}; {msg}"),
                    None => msg.into(),
                });
            } else {
                self.prepare_submodules(ws, &job.url, 0).await?;
            }
        }

        let mut index: HashMap<String, String> = git::ls_files(ws)
            .await?
            .into_iter()
            .map(|e| (e.path, e.oid))
            .collect();
        if let Some(d) = &job.overlay {
            let overlay: Overlay = serde_json::from_slice(&self.blob(d).await?)?;
            for e in &overlay.entries {
                let dest = files::safe_join(ws, &e.path)?;
                match &e.digest {
                    None => {
                        let _ = std::fs::remove_file(&dest);
                        index.remove(&e.path);
                    }
                    Some(digest) => {
                        files::write_file(&dest, &self.blob(digest).await?, e.executable)?;
                        index.insert(e.path.clone(), format!("b3:{digest}"));
                    }
                }
            }
        }
        let mut files: Vec<(String, String)> = index.into_iter().collect();
        files.sort();
        Ok((files, note))
    }

    /// Checks out a workspace's submodules at the commits it pins, each from
    /// a mirror of its own on this worker: fetched once, then shared by every
    /// workspace and kept warm, instead of cloned per job.
    fn prepare_submodules<'a>(
        &'a self,
        ws: &'a Path,
        url: &'a str,
        depth: usize,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<()>> + Send + 'a>> {
        Box::pin(async move {
            if depth > 3 || !ws.join(".gitmodules").exists() {
                return Ok(());
            }
            let listed = git::git(
                ws,
                &[
                    "config",
                    "-f",
                    ".gitmodules",
                    "--get-regexp",
                    r"^submodule\..*\.(path|url)$",
                ],
            )
            .await
            .unwrap_or_default();
            let mut subs: std::collections::BTreeMap<String, (Option<String>, Option<String>)> =
                Default::default();
            for line in listed.lines() {
                let Some((key, value)) = line.split_once(' ') else {
                    continue;
                };
                let key = key.trim_start_matches("submodule.");
                if let Some(name) = key.strip_suffix(".path") {
                    subs.entry(name.to_string()).or_default().0 = Some(value.to_string());
                } else if let Some(name) = key.strip_suffix(".url") {
                    subs.entry(name.to_string()).or_default().1 = Some(value.to_string());
                }
            }
            for (name, (path, sub_url)) in subs {
                let (Some(path), Some(sub_url)) = (path, sub_url) else {
                    continue;
                };
                // The commit the superproject pins (a gitlink, mode 160000).
                let entry = git::git(ws, &["ls-tree", "HEAD", "--", &path]).await?;
                let mut fields = entry.split_whitespace();
                let (Some("160000"), Some(_), Some(commit)) =
                    (fields.next(), fields.next(), fields.next())
                else {
                    continue;
                };
                let resolved = git::resolve_submodule_url(url, &sub_url);
                // A path on this machine only for a superproject that is one
                // too (a local setup): a repo hosted elsewhere must not read
                // other repos' mirrors through its `.gitmodules`.
                if !git::is_fetchable_url(&resolved) {
                    anyhow::bail!("submodule {name}: {resolved:?} is not a URL Pit fetches");
                }
                // A hosted repo's submodules come from its own host, or one the
                // operator allows (`PIT_SUBMODULE_HOSTS`): `.gitmodules` is
                // repository code, and the worker fetches with its
                // credentials. A local repo (a dev setup) may use paths.
                if !git::is_local_url(url) && !submodule_host_allowed(url, &resolved) {
                    anyhow::bail!(
                        "submodule {name}: {resolved} is not on this repo's host; \
                         allow its host with PIT_SUBMODULE_HOSTS"
                    );
                }
                let mirror = self.data_dir.join("mirrors").join(format!(
                    "sub-{}.git",
                    &crate::cas::digest(resolved.as_bytes())[..16]
                ));
                {
                    let lock = self.mirror_lock(&resolved);
                    let _g = lock.lock().await;
                    git::ensure_commit(&mirror, &resolved, commit)
                        .await
                        .with_context(|| format!("fetching submodule {name} ({resolved})"))?;
                }
                let mirror_url = mirror.to_string_lossy().to_string();
                git::git(
                    ws,
                    &["config", &format!("submodule.{name}.url"), &mirror_url],
                )
                .await?;
                git::git(
                    ws,
                    &[
                        "-c",
                        "protocol.file.allow=always",
                        "submodule",
                        "update",
                        "--init",
                        "--force",
                        "--checkout",
                        "--",
                        &path,
                    ],
                )
                .await?;
                let sub = ws.join(&path);
                git::git(&sub, &["clean", "-q", "-ffd"]).await?;
                self.prepare_submodules(&sub, &resolved, depth + 1).await?;
            }
            Ok(())
        })
    }

    /// A blob from the node-local cache, fetched from the server on a miss.
    async fn blob(&self, digest: &str) -> Result<Vec<u8>> {
        if let Some(b) = self.cas.get(digest)? {
            return Ok(b);
        }
        let bytes = self.client.cas_get(digest).await?;
        self.cas.put_checked(digest, &bytes)?;
        Ok(bytes)
    }

    async fn run_tasks(
        self: Arc<Self>,
        job: &Job,
        ws: &Path,
        files: Arc<Vec<(String, String)>>,
        cancel: tokio::sync::watch::Receiver<bool>,
    ) -> Status {
        // Every update for this job carries its lease, so a worker that lost
        // the group (reaped, then re-claimed elsewhere) cannot overwrite it.
        let client = self.client.with_lease(&job.lease);
        let warm_path = warm_file(ws);
        let warm: Arc<Mutex<HashMap<String, String>>> = Arc::new(Mutex::new(
            std::fs::read(&warm_path)
                .ok()
                .and_then(|b| serde_json::from_slice(&b).ok())
                .unwrap_or_default(),
        ));
        // A fork's tasks run one at a time. Between them no fork code is
        // running (a task's container goes with it), so nothing can swap a
        // checked directory for a symlink while the worker clears outputs,
        // collects them or reads reports.
        let parallelism = if job.no_secrets { 1 } else { self.parallelism };
        let mut state: HashMap<String, (Status, Option<String>)> = HashMap::new();
        let mut started: std::collections::HashSet<String> = Default::default();
        let mut running: JoinSet<(String, Status, Option<String>)> = JoinSet::new();
        loop {
            for t in &job.tasks {
                // Cancelled: start nothing new, let running tasks stop.
                if started.contains(&t.name) || *cancel.borrow() {
                    continue;
                }
                let deps: Vec<Option<&(Status, Option<String>)>> =
                    t.def.deps.iter().map(|d| state.get(d)).collect();
                if deps.iter().any(|d| d.is_some_and(|(s, _)| !s.is_ok())) {
                    started.insert(t.name.clone());
                    state.insert(t.name.clone(), (Status::Skipped, None));
                    let r = TaskResult {
                        status: Status::Skipped,
                        exit_code: None,
                        action_key: None,
                        duration_ms: 0,
                        log_digest: None,
                        message: Some("a dependency failed".into()),
                    };
                    let _ = client
                        .post_empty(&format!("/api/tasks/{}/finish", t.id), &r)
                        .await;
                    continue;
                }
                if deps.iter().all(|d| d.is_some()) && running.len() < parallelism {
                    started.insert(t.name.clone());
                    let dep_keys: Vec<String> = deps
                        .iter()
                        .filter_map(|d| d.and_then(|(_, k)| k.clone()))
                        .collect();
                    let w = self.clone();
                    let (task, job, ws, files, warm, cancel) = (
                        t.clone(),
                        job.clone(),
                        ws.to_path_buf(),
                        files.clone(),
                        warm.clone(),
                        cancel.clone(),
                    );
                    running.spawn(async move {
                        let (status, key) = match w
                            .run_task(&job, &task, &ws, &files, &dep_keys, &warm, cancel)
                            .await
                        {
                            Ok(r) => r,
                            Err(e) => {
                                let r = TaskResult {
                                    status: Status::Error,
                                    exit_code: None,
                                    action_key: None,
                                    duration_ms: 0,
                                    log_digest: None,
                                    message: Some(format!("{e:#}")),
                                };
                                let _ = w
                                    .client
                                    .with_lease(&job.lease)
                                    .post_empty(&format!("/api/tasks/{}/finish", task.id), &r)
                                    .await;
                                (Status::Error, None)
                            }
                        };
                        (task.name, status, key)
                    });
                }
            }
            match running.join_next().await {
                Some(Ok((name, status, key))) => {
                    state.insert(name, (status, key));
                }
                Some(Err(e)) => tracing::error!("task panicked: {e}"),
                None => break,
            }
        }
        save_warm(ws, &warm.lock());
        let quarantined: HashSet<&str> = job
            .tasks
            .iter()
            .filter(|t| t.def.quarantine)
            .map(|t| t.name.as_str())
            .collect();
        // A task skipped because its dependency failed did not pass either:
        // a quarantined `build` must not turn the `test` after it into a
        // green run that tested nothing.
        let failed = state.iter().any(|(name, (s, _))| {
            matches!(s, Status::Failed | Status::Error | Status::Skipped)
                && !quarantined.contains(name.as_str())
        });
        if failed {
            Status::Failed
        } else {
            Status::Passed
        }
    }

    #[allow(clippy::too_many_arguments)]
    async fn run_task(
        &self,
        job: &Job,
        task: &JobTask,
        ws: &Path,
        files: &[(String, String)],
        dep_keys: &[String],
        warm: &Mutex<HashMap<String, String>>,
        cancel: tokio::sync::watch::Receiver<bool>,
    ) -> Result<(Status, Option<String>)> {
        // Every update for this job carries its lease, so a worker that lost
        // the group (reaped, then re-claimed elsewhere) cannot overwrite it.
        let client = self.client.with_lease(&job.lease);
        let def = &task.def;
        let runtime = self.measure_runtime(job, ws, def).await;
        let inputs =
            crate::graph::key_inputs_with_runtime(def, &job.os, files, dep_keys, &runtime)?;
        let key = inputs.key();
        // Identifies "this task" across runs, to find its last cached result.
        let scope = crate::cas::digest(
            format!("{}\0{}\0{}\0{}", job.repo, job.group, task.name, job.os).as_bytes(),
        );
        let mut miss_note: Option<String> = None;
        let mut restore_failed: Option<String> = None;
        client
            .post_empty(
                &format!("/api/tasks/{}/start", task.id),
                &TaskStart {
                    action_key: Some(key.clone()),
                },
            )
            .await?;
        let finish = |status: Status,
                      exit_code: Option<i32>,
                      duration_ms: u64,
                      log_digest: Option<String>,
                      message: Option<String>| TaskResult {
            status,
            exit_code,
            action_key: Some(key.clone()),
            duration_ms,
            log_digest,
            message,
        };

        if !job.no_cache {
            match def.cache {
                CacheMode::Remote => {
                    // The same action may be running right now elsewhere (a
                    // sibling shard, another run of this commit). Wait for
                    // its result rather than doing the work twice.
                    let mut waited: Option<(Instant, String)> = None;
                    let found = loop {
                        let entry = client
                            .get_opt::<CacheEntry>(&format!("/api/cache/{key}"))
                            .await?;
                        // A fork's run writes no entry, so others must not
                        // wait for it.
                        if entry.is_some() || *cancel.borrow() || job.no_secrets {
                            break entry;
                        }
                        let claim: serde_json::Value = match client
                            .post(
                                &format!("/api/cache/{key}/claim"),
                                &serde_json::json!({ "task": task.id }),
                            )
                            .await
                        {
                            Ok(v) => v,
                            Err(_) => break None,
                        };
                        if claim["wait"].as_bool() != Some(true) {
                            break None;
                        }
                        if waited.is_none() {
                            let run = claim["run"].as_str().unwrap_or("another run");
                            waited = Some((Instant::now(), run.to_string()));
                        }
                        tokio::time::sleep(Duration::from_millis(300)).await;
                    };
                    if let Some(entry) = found {
                        let t = Instant::now();
                        // The server may evict the outputs between the lookup
                        // and the restore. Then the task just runs.
                        if let Err(e) = self.restore_outputs(ws, def, &entry).await {
                            restore_failed =
                                Some(format!("its cached outputs could not be restored ({e:#})"));
                        } else {
                            let mut msg = format!(
                                "cache hit, saved {:.1}s ({} output file(s) restored in {}ms)",
                                entry.duration_ms as f64 / 1000.0,
                                entry.outputs.len(),
                                t.elapsed().as_millis()
                            );
                            if let Some((since, run)) = &waited {
                                msg.push_str(&format!(
                                    "; waited {:.1}s for the same task in {run} instead of running it again",
                                    since.elapsed().as_secs_f64()
                                ));
                            }
                            let r = finish(
                                Status::Cached,
                                Some(0),
                                t.elapsed().as_millis() as u64,
                                entry.log_digest.clone(),
                                Some(msg),
                            );
                            client
                                .post_empty(&format!("/api/tasks/{}/finish", task.id), &r)
                                .await?;
                            return Ok((Status::Cached, Some(key)));
                        }
                    }
                    // Deps run first and all passed, so their keys line up with `def.deps`.
                    let dep_names: Vec<(String, String)> = def
                        .deps
                        .iter()
                        .cloned()
                        .zip(dep_keys.iter().cloned())
                        .collect();
                    miss_note = match restore_failed {
                        Some(note) => Some(note),
                        None => {
                            self.explain_miss(&client, &scope, &inputs, &dep_names)
                                .await
                        }
                    };
                }
                CacheMode::Workspace => {
                    if warm.lock().get(&task.name) == Some(&key) {
                        let r = finish(
                            Status::Cached,
                            Some(0),
                            0,
                            None,
                            Some("unchanged since this warm workspace last ran it".into()),
                        );
                        client
                            .post_empty(&format!("/api/tasks/{}/finish", task.id), &r)
                            .await?;
                        return Ok((Status::Cached, Some(key)));
                    }
                }
                CacheMode::Off => {}
            }
        }

        // About to change the workspace: forget the warm result until this
        // run succeeds, so a failed or partial run is never treated as warm.
        if def.cache == CacheMode::Workspace && warm.lock().remove(&task.name).is_some() {
            save_warm(ws, &warm.lock());
        }
        // Outputs left by an older run must not be stored under this key.
        if def.cache == CacheMode::Remote && !job.no_cache {
            files::clear_outputs(ws, &def.outputs)?;
        }

        // A shard dealt no files must not run its command with an empty
        // list: most test runners would then run every test.
        if !def.shard_files.is_empty() && def.shard_assigned.is_empty() {
            let r = finish(
                Status::Passed,
                Some(0),
                0,
                None,
                Some("no files were dealt to this shard".into()),
            );
            client
                .post_empty(&format!("/api/tasks/{}/finish", task.id), &r)
                .await?;
            return Ok((Status::Passed, Some(key)));
        }
        // Fork code on the host or in the sandbox would share the worker's
        // filesystem, and with it the secrets file, other workspaces and the
        // worker's own /proc. It runs in a container that sees only its
        // workspace, or not at all.
        let backend = if job.no_secrets {
            if !self.info.backends.contains(&Backend::Docker) {
                let r = finish(
                    Status::Error,
                    None,
                    0,
                    None,
                    Some(
                        "pull request from a fork: it only runs in the docker backend, \
                         which this worker does not have"
                            .into(),
                    ),
                );
                client
                    .post_empty(&format!("/api/tasks/{}/finish", task.id), &r)
                    .await?;
                return Ok((Status::Error, Some(key)));
            }
            Backend::Docker
        } else {
            def.backend.unwrap_or_default()
        };
        let mut env: Vec<(String, String)> = vec![
            ("CI".into(), "true".into()),
            ("PIT".into(), "1".into()),
            ("PIT_TASK".into(), task.name.clone()),
            ("PIT_RUN_ID".into(), job.run_id.clone()),
            ("PIT_SHA".into(), job.sha.clone()),
            ("PIT_SHARD".into(), def.shard.unwrap_or(1).to_string()),
            (
                "PIT_SHARD_COUNT".into(),
                def.shards.unwrap_or(1).to_string(),
            ),
        ];
        // The same names `nx-set-shas` and Turborepo use.
        if let Some(base) = &job.base_sha {
            for k in ["PIT_BASE_SHA", "NX_BASE", "TURBO_SCM_BASE"] {
                env.push((k.into(), base.clone()));
            }
            // A head makes them diff `base...head` only. `pit run` changes
            // are in the working tree, not in a commit, and must count:
            // without a head the tools diff the working tree.
            if job.overlay.is_none() {
                for k in ["NX_HEAD", "TURBO_SCM_HEAD"] {
                    env.push((k.into(), job.sha.clone()));
                }
            }
        }
        // `$PIT_SUMMARY`, also as `$GITHUB_STEP_SUMMARY`: markdown the task
        // appends to (tools that write Actions job summaries work as is),
        // shown on its check. A path the task can open, host or container.
        // Outside the workspace, in a directory of the worker's own: a task
        // (a fork's, in a container) controls everything in the workspace,
        // and a symlink there could aim the worker's reads and writes at
        // other files. Named by task id, which is unique.
        let summary_dir = self.data_dir.join("summaries").join(&job.lease);
        std::fs::create_dir_all(&summary_dir)?;
        let summary_file = format!("{}.md", task.id);
        let summary_path = summary_dir.join(&summary_file);
        let summary_env = if backend == Backend::Docker {
            format!("{}/{summary_file}", exec::SUMMARY_MOUNT)
        } else {
            summary_path.to_string_lossy().into_owned()
        };
        for k in ["PIT_SUMMARY", "GITHUB_STEP_SUMMARY"] {
            env.push((k.into(), summary_env.clone()));
        }
        // One file per line, so names with spaces survive `xargs -d '\n'`.
        // The whole list is always in a file too: a big suite's list can be
        // more than the OS lets a process environment hold.
        if !def.shard_files.is_empty() {
            let list = def.shard_assigned.join("\n");
            let rel = shard_list(&task.name);
            let at = files::safe_join(ws, &rel)?;
            files::write_file(&at, format!("{list}\n").as_bytes(), false)?;
            env.push(("PIT_SHARD_FILES_LIST".into(), rel));
            if list.len() <= SHARD_ENV_BYTES {
                env.push(("PIT_SHARD_FILES".into(), list));
            }
        }
        // Never hand Pit's own settings (API token) back through `env`. Windows
        // variable names are case-insensitive, so compare uppercased.
        let mut secret_values = Vec::new();
        let wanted = def
            .env
            .iter()
            .map(|k| (k, false))
            .chain(def.secrets.iter().map(|k| (k, true)));
        // A fork's config could ask for anything: it gets nothing.
        for (k, secret) in wanted.filter(|_| !job.no_secrets) {
            if k.to_ascii_uppercase().starts_with("PIT_") {
                continue;
            }
            let value = if secret {
                self.secrets
                    .get(k)
                    .cloned()
                    .or_else(|| std::env::var(k).ok())
            } else {
                std::env::var(k).ok()
            };
            if let Some(v) = value {
                if secret {
                    secret_values.push(v.clone());
                }
                env.push((k.clone(), v));
            }
        }
        if job.no_secrets && !(def.env.is_empty() && def.secrets.is_empty()) {
            let _ = client
                .post_empty(
                    &format!("/api/tasks/{}/log", task.id),
                    &LogChunk {
                        text: "[pit: pull request from a fork; `env` and `secrets` are withheld]\n"
                            .into(),
                    },
                )
                .await;
        }
        // A short-lived OIDC token for the cloud the task talks to.
        if let (Some(audience), false) = (&def.oidc, job.no_secrets) {
            let token: serde_json::Value = client
                .post(
                    "/api/oidc/token",
                    &serde_json::json!({ "audience": audience }),
                )
                .await
                .context("requesting an OIDC token")?;
            let token = token["token"].as_str().unwrap_or_default().to_string();
            secret_values.push(token.clone());
            env.push(("PIT_OIDC_TOKEN".into(), token));
        }
        // Where the group's services are, from this task's backend. The
        // first task that gets here starts them; a group answered by the
        // cache never pulls or starts a database.
        let services = self.group_services(job, def, cancel.clone()).await?;
        if let Some(s) = &services {
            env.extend(s.env(backend));
        }
        let policy = exec::EnvPolicy {
            hide: self.secrets.keys().cloned().collect(),
            minimal: job.no_secrets,
            docker_network: services.as_ref().map(|s| s.network.clone()),
            summary_dir: Some(summary_dir.clone()),
        };
        // Secrets never reach the server's logs, live or stored.
        let masker = Arc::new(mask::Masker::new(secret_values));
        // The task's own retries, plus the job's automatic ones (merge queue).
        let attempts = 1 + def.retries.unwrap_or(0) + job.flake_retries;
        let mut log = String::new();
        let mut failures: Vec<String> = Vec::new();
        let mut duration_ms = 0u64;
        let mut attempt = 0;
        // Tests that failed in an attempt that was then retried.
        let mut retried_tests: std::collections::BTreeSet<String> = Default::default();
        let outcome = loop {
            attempt += 1;
            files::clear_outputs(ws, &def.reports)?;
            // Each attempt starts a summary of its own.
            files::write_file(&summary_path, b"", false)?;
            let (tx, rx) = mpsc::unbounded_channel::<String>();
            let forwarder = tokio::spawn(forward_logs(
                client.clone(),
                task.id.clone(),
                rx,
                masker.clone(),
            ));
            let cmd = exec::command_with(def, backend, ws, &env, &policy);
            let result = exec::run(cmd, limits(def), tx, cancel.clone()).await;
            let _ = forwarder.await;
            let outcome = result?;
            duration_ms += outcome.duration.as_millis() as u64;
            log.push_str(&masker.mask(&outcome.log));
            if outcome.cancelled {
                // The group is no longer ours; nothing to report or cache.
                return Ok((Status::Skipped, None));
            }
            if (outcome.exit_code == Some(0) && !outcome.timed_out) || attempt >= attempts {
                break outcome;
            }
            let why = match (outcome.timed_out, outcome.exit_code) {
                (true, _) if outcome.idle => "no output for too long".to_string(),
                (true, _) => "timed out".to_string(),
                (_, Some(code)) => format!("exit {code}"),
                _ => "killed by a signal".to_string(),
            };
            failures.push(format!("attempt {attempt}: {why}"));
            // Read now: the next attempt clears the reports.
            if let Ok(reports) = files::collect_outputs(ws, &def.reports) {
                let paths: Vec<&std::path::PathBuf> = reports.iter().map(|(_, a)| a).collect();
                if let Some(s) = junit::summarize(&paths) {
                    retried_tests.extend(s.failed_names().map(|n| masker.mask(n)));
                }
            }
            let note = format!("\n[pit: attempt {attempt} failed ({why}); retrying]\n");
            log.push_str(&note);
            let _ = client
                .post_empty(
                    &format!("/api/tasks/{}/log", task.id),
                    &LogChunk { text: note },
                )
                .await;
            if def.cache == CacheMode::Remote && !job.no_cache {
                files::clear_outputs(ws, &def.outputs)?;
            }
        };
        let log_bytes = log.into_bytes();
        let log_digest = crate::cas::digest(&log_bytes);
        client
            .upload_missing(vec![(log_digest.clone(), log_bytes)])
            .await?;

        let ok = outcome.exit_code == Some(0) && !outcome.timed_out;
        let lim = limits(def);
        let mut message = if outcome.idle {
            Some(format!(
                "no output for {}s; stopped (set idleTimeout to change, 0 to disable)",
                lim.idle.map(|d| d.as_secs()).unwrap_or_default()
            ))
        } else {
            outcome.timed_out.then(|| {
                format!(
                    "ran longer than {}s; stopped (set timeout to change, 0 to disable)",
                    lim.timeout.map(|d| d.as_secs()).unwrap_or_default()
                )
            })
        };
        if ok && !failures.is_empty() {
            // Passed only on a retry: say so loudly so the flake gets fixed.
            message = Some(format!(
                "flaky: {}, passed on attempt {attempt}",
                failures.join(", ")
            ));
        }
        // A pass that needed a retry is not reused: later runs of the same
        // inputs must run it again, so the flake keeps showing until fixed.
        // A fork's results are not shared at all: same-repo pull requests,
        // which get secrets, read the same untrusted entries, and a task
        // whose `inputs` miss a file the fork controls would hand them its
        // output.
        if ok && !job.no_cache && !job.no_secrets && failures.is_empty() {
            match def.cache {
                CacheMode::Remote => {
                    let outputs = files::collect_outputs(ws, &def.outputs)?;
                    let mut blobs = Vec::new();
                    let mut manifest = Vec::new();
                    for (rel, abs) in outputs {
                        let bytes =
                            std::fs::read(&abs).with_context(|| format!("reading output {rel}"))?;
                        let digest = self.cas.put(&bytes)?;
                        manifest.push(OutputFile {
                            path: rel,
                            digest: digest.clone(),
                            executable: files::is_executable(&abs),
                        });
                        blobs.push((digest, bytes));
                    }
                    let inputs_bytes = serde_json::to_vec(&inputs)?;
                    let inputs_digest = crate::cas::digest(&inputs_bytes);
                    blobs.push((inputs_digest.clone(), inputs_bytes));
                    client.upload_missing(blobs).await?;
                    let entry = CacheEntry {
                        log_digest: Some(log_digest.clone()),
                        outputs: manifest,
                        duration_ms,
                        inputs: Some(inputs_digest),
                    };
                    client
                        .put_json(&format!("/api/cache/{key}?scope={scope}"), &entry)
                        .await?;
                }
                CacheMode::Workspace => {
                    warm.lock().insert(task.name.clone(), key.clone());
                }
                CacheMode::Off => {}
            }
        }
        if !ok && message.is_none() {
            message = Some(
                memory::explain(
                    outcome.exit_code,
                    outcome.signal,
                    outcome.oom,
                    // A container's processes are not in the task's process
                    // group; only the docker client is.
                    if backend == Backend::Docker {
                        0
                    } else {
                        outcome.peak_memory
                    },
                )
                .unwrap_or_else(|| {
                    format!(
                        "exited with {}",
                        outcome
                            .exit_code
                            .map(|c| c.to_string())
                            .unwrap_or_else(|| "a signal".into())
                    )
                }),
            );
        }
        // Name the failed tests, from the task's JUnit reports.
        let reports = files::collect_outputs(ws, &def.reports)?;
        let paths: Vec<&std::path::PathBuf> = reports.iter().map(|(_, abs)| abs).collect();
        let mut summary = junit::summarize(&paths);
        // A quarantined failure is no news for the pull request's diff.
        if let Some(s) = summary.as_mut().filter(|_| !ok && !def.quarantine) {
            let known: std::collections::HashSet<&str> =
                files.iter().map(|(p, _)| p.as_str()).collect();
            s.resolve(&known, ws);
            // Where each failed test is, for an annotation on the pull request.
            let annotations: Vec<Annotation> = s
                .failures
                .iter()
                .filter_map(|(name, msg)| {
                    let (path, line) = s.locations.get(name)?;
                    // Test output can hold a secret ("expected ghp_… to
                    // equal …"); annotations are public on public repos.
                    Some(Annotation {
                        path: path.clone(),
                        line: *line,
                        title: masker.mask(name),
                        message: if msg.is_empty() {
                            "failed".into()
                        } else {
                            masker.mask(msg)
                        },
                    })
                })
                .collect();
            if !annotations.is_empty() {
                let _ = client
                    .post_empty(&format!("/api/tasks/{}/annotations", task.id), &annotations)
                    .await;
            }
        }
        // Per-file durations for the next split: from the reports when they
        // name files, otherwise this shard's time spread over its files.
        if ok && !def.shard_assigned.is_empty() {
            let reported: HashMap<String, f64> = summary
                .iter()
                .flat_map(|s| s.file_times.iter())
                .filter_map(|(name, t)| {
                    def.shard_assigned
                        .iter()
                        .find(|f| {
                            *f == name
                                || f.ends_with(&format!("/{name}"))
                                || name.ends_with(f.as_str())
                        })
                        .map(|f| (f.clone(), *t))
                })
                .collect();
            let body = if reported.is_empty() {
                serde_json::json!({
                    "task": task.name,
                    "total": duration_ms as f64 / 1000.0,
                    "files": def.shard_assigned,
                })
            } else {
                serde_json::json!({ "task": task.name, "times": reported })
            };
            let _ = client.post_empty("/api/timings", &body).await;
        }
        // Which tests failed, and which failed only before a retry, for
        // `pit flaky --tests`.
        let failed_now: std::collections::BTreeSet<String> = summary
            .iter()
            .flat_map(|s| s.failed_names())
            .map(|n| masker.mask(n))
            .collect();
        // Without a report from the last attempt (it crashed before writing
        // one), nothing is known to have passed on the retry.
        let passed_on_retry: Vec<String> = if summary.is_some() {
            retried_tests.difference(&failed_now).cloned().collect()
        } else {
            vec![]
        };
        // Posted even when empty: it replaces what an earlier attempt of this
        // task (requeued) recorded.
        if !def.reports.is_empty() {
            if ok && !passed_on_retry.is_empty() {
                let names: Vec<&str> = passed_on_retry.iter().take(5).map(String::as_str).collect();
                message = message.map(|m| format!("{m} (flaky tests: {})", names.join(", ")));
            }
            let _ = client
                .post_empty(
                    &format!("/api/tasks/{}/tests", task.id),
                    &TestOutcomes {
                        failed: failed_now.into_iter().collect(),
                        passed_on_retry,
                    },
                )
                .await;
        }
        if let Some(summary) = summary {
            let text = masker.mask(&summary.message());
            let _ = client
                .post_empty(
                    &format!("/api/tasks/{}/log", task.id),
                    &LogChunk {
                        text: format!("[pit: {text}]\n"),
                    },
                )
                .await;
            if !ok && summary.failed() > 0 {
                message = message.map(|m| format!("{m}; {text}"));
            }
        } else if !def.reports.is_empty() {
            let _ = client
                .post_empty(
                    &format!("/api/tasks/{}/log", task.id),
                    &LogChunk {
                        text: "[pit: no test report matched `reports`]\n".into(),
                    },
                )
                .await;
        }
        if !ok && def.quarantine {
            message = message.map(|m| format!("quarantined, so this does not fail the run: {m}"));
        }
        // Say why the task ran instead of coming from the cache.
        if let Some(note) = miss_note {
            message = Some(match message {
                Some(m) => format!("{m} · ran because {note}"),
                None => format!("ran because {note}"),
            });
        }
        // `pit run --debug`: a way back into this workspace, as the task saw it.
        if !ok && job.debug && cfg!(unix) {
            let script = debug::write_script(
                &debug::dir(&self.data_dir, &job.group_id),
                &task.name,
                def,
                backend,
                ws,
                &env,
                &policy,
                message.as_deref(),
            )?;
            let how = format!(
                "debug: `sh {}` opens a shell where it ran, or runs a command there \
                 (workspace kept {}m; delete the script to release it)",
                script.display(),
                debug::HOLD.as_secs() / 60
            );
            message = Some(match message {
                Some(m) => format!("{m} · {how}"),
                None => how,
            });
        }
        // Posted even when empty: it replaces what an earlier attempt of this
        // task (requeued) left.
        let cap = crate::server::api::MAX_SUMMARY_BYTES as u64;
        let too_big = std::fs::symlink_metadata(&summary_path).is_ok_and(|m| m.len() > cap);
        let md = if too_big {
            let note = format!("\n[pit: summary over {} KiB left out]\n", cap >> 10);
            let _ = client
                .post_empty(
                    &format!("/api/tasks/{}/log", task.id),
                    &LogChunk { text: note },
                )
                .await;
            String::new()
        } else {
            files::read_task_file(&summary_path, cap)
                .map(|b| String::from_utf8_lossy(&b).into_owned())
                .unwrap_or_default()
        };
        let _ = client
            .post_empty(
                &format!("/api/tasks/{}/summary", task.id),
                &serde_json::json!({ "markdown": masker.mask(&md) }),
            )
            .await;
        let _ = std::fs::remove_file(&summary_path);
        let message = message.map(|m| masker.mask(&m));
        let status = if ok { Status::Passed } else { Status::Failed };
        let r = finish(
            status,
            outcome.exit_code,
            duration_ms,
            Some(log_digest),
            message,
        );
        client
            .post_empty(&format!("/api/tasks/{}/finish", task.id), &r)
            .await?;
        Ok((status, Some(key)))
    }
}

impl Worker {
    async fn group_services(
        &self,
        job: &Job,
        def: &TaskDef,
        cancel: tokio::sync::watch::Receiver<bool>,
    ) -> Result<Option<Arc<services::Running>>> {
        if def.services.is_empty() {
            return Ok(None);
        }
        let Some(slot) = self.services.lock().get(&job.lease).cloned() else {
            return Ok(None);
        };
        // Names carry the lease: a group reclaimed on the same docker host
        // never collides with, or removes, an earlier claim's services.
        let prefix = format!("{}-{}", job.group_id, &job.lease[..job.lease.len().min(8)]);
        let started = slot
            .get_or_init(|| async {
                services::start(&self.instance, &prefix, &def.services, cancel)
                    .await
                    .map(Arc::new)
                    .map_err(|e| format!("{e:#}"))
            })
            .await;
        match started {
            Ok(s) => Ok(Some(s.clone())),
            Err(e) => Err(anyhow::anyhow!("{e}")),
        }
    }
}

/// Whether a hosted superproject may fetch this submodule URL: a remote on
/// its own host, or on one listed in `PIT_SUBMODULE_HOSTS` (comma separated).
fn submodule_host_allowed(superproject: &str, submodule: &str) -> bool {
    if git::is_local_url(submodule) {
        return false;
    }
    let Some(host) = git::url_host(submodule) else {
        return false;
    };
    if git::url_host(superproject).as_deref() == Some(host.as_str()) {
        return true;
    }
    std::env::var("PIT_SUBMODULE_HOSTS").is_ok_and(|list| {
        list.split(',')
            .any(|h| h.trim().eq_ignore_ascii_case(&host))
    })
}

#[cfg(test)]
mod submodule_tests {
    #[test]
    fn hosted_repos_fetch_submodules_from_their_own_host_only() {
        let ok = super::submodule_host_allowed;
        let app = "https://github.com/acme/app.git";
        assert!(ok(app, "https://github.com/acme/lib.git"));
        assert!(ok(app, "git@github.com:acme/lib.git"));
        assert!(!ok(app, "https://evil.example/x.git"));
        assert!(!ok(app, "objects/../../other.git"));
        assert!(!ok(app, "/srv/mirrors/other.git"));
        assert!(!ok("https://[fe80::1]/a.git", "https://[fe80::2]/b.git"));
        // `ssh://evil%2f@github.com/x` reads as github.com but git goes to
        // `evil`: refused before the host check (`is_fetchable_url`).
        for sneaky in ["ssh://evil%2f@github.com/x", "[evil]@github.com:x"] {
            assert!(!crate::git::is_fetchable_url(sneaky), "{sneaky}");
        }
        // git resolves `github.com:1.evil.com` as one name, in evil.com.
        for sneaky in [
            "git://github.com:1.evil.com/x",
            "ssh://git@github.com:1.evil.com/x",
        ] {
            assert!(!ok(app, sneaky), "{sneaky}");
        }
        assert!(ok(app, "ssh://git@github.com:22/acme/lib.git"));
    }
}

type ServiceSlot = Arc<tokio::sync::OnceCell<std::result::Result<Arc<services::Running>, String>>>;

/// A random id kept in the data dir, made on first start.
fn instance_id(data_dir: &Path) -> String {
    let path = data_dir.join("instance-id");
    if let Ok(id) = std::fs::read_to_string(&path) {
        if !id.trim().is_empty() {
            return id.trim().to_string();
        }
    }
    let id = uuid::Uuid::new_v4().simple().to_string();
    let _ = std::fs::create_dir_all(data_dir);
    let _ = std::fs::write(&path, &id);
    id
}

fn warm_file(ws: &Path) -> PathBuf {
    disk::meta_file(ws, "warm.json")
}

/// Which workspace-cached tasks this workspace has run, by action key.
fn save_warm(ws: &Path, warm: &HashMap<String, String>) {
    if let Ok(bytes) = serde_json::to_vec(warm) {
        let _ = std::fs::write(warm_file(ws), bytes);
    }
}

impl Worker {
    /// Output of each of the task's `runtime` commands, run once per job.
    async fn measure_runtime(
        &self,
        job: &Job,
        ws: &Path,
        def: &TaskDef,
    ) -> HashMap<String, String> {
        let mut out = HashMap::new();
        // The image a container task runs in is part of its key, by ID: the
        // tag alone can point at different images on different workers.
        let image = (def.backend == Some(Backend::Docker) || def.image.is_some() || job.no_secrets)
            .then(|| {
                def.image
                    .clone()
                    .unwrap_or_else(|| exec::DEFAULT_IMAGE.into())
            });
        if let Some(image) = image {
            let memo_key = format!("{}\0image {image}", job.group_id);
            let cached = self.runtime_memo.lock().get(&memo_key).cloned();
            let id = match cached {
                Some(id) => id,
                None => {
                    let id = image_id(&image).await;
                    self.runtime_memo.lock().insert(memo_key, id.clone());
                    id
                }
            };
            out.insert(format!("{}{image}", crate::graph::IMAGE_INPUT), id);
        }
        // `nx affected` on the same files does different work against a
        // different base: a task that uses the base keys on it, or a cache
        // hit could replay a run that tested fewer projects.
        let uses_base = ["PIT_BASE_SHA", "NX_BASE", "TURBO_SCM_BASE", "affected"]
            .iter()
            .any(|w| def.run.contains(w));
        if let (true, Some(base)) = (uses_base, &job.base_sha) {
            out.insert("affected base".into(), base.clone());
        }
        // A test's result depends on the database it ran against: each
        // service's image (by ID), command and variables are inputs too.
        for (name, svc) in &def.services {
            let memo_key = format!("{}\0service {name}", job.group_id);
            let cached = self.runtime_memo.lock().get(&memo_key).cloned();
            let id = match cached {
                Some(id) => id,
                None => {
                    let id = image_id(&svc.image).await;
                    self.runtime_memo.lock().insert(memo_key, id.clone());
                    id
                }
            };
            out.insert(
                format!("service {name}"),
                format!(
                    "{} {id} command={:?} env={:?} ports={:?}",
                    svc.image, svc.command, svc.env, svc.ports
                ),
            );
        }
        for cmd in &def.runtime {
            let memo_key = format!("{}\0{cmd}", job.group_id);
            if let Some(v) = self.runtime_memo.lock().get(&memo_key).cloned() {
                out.insert(cmd.clone(), v);
                continue;
            }
            // A runtime command is repository code like the task: the same
            // environment rules, and a fork's runs in its container, offline,
            // instead of on the worker.
            let mut probe = def.clone();
            probe.run = cmd.clone();
            let backend = if job.no_secrets {
                probe.network = Some(false);
                Backend::Docker
            } else {
                Backend::Host
            };
            let policy = exec::EnvPolicy {
                hide: self.secrets.keys().cloned().collect(),
                minimal: job.no_secrets,
                docker_network: None,
                summary_dir: None,
            };
            let exec::TaskCommand {
                cmd: mut command,
                container,
            } = exec::command_with(&probe, backend, ws, &[], &policy);
            command
                .stdin(std::process::Stdio::null())
                .kill_on_drop(true);
            let limit = Duration::from_secs(if container.is_some() { 60 } else { 15 });
            let result = tokio::time::timeout(limit, command.output()).await;
            if let (Err(_), Some(name)) = (&result, &container) {
                let _ = tokio::process::Command::new("docker")
                    .args(["rm", "-f", name])
                    .output()
                    .await;
            }
            let value = match result {
                Ok(Ok(o)) if o.status.success() => {
                    String::from_utf8_lossy(&o.stdout).trim().to_string()
                }
                Ok(Ok(o)) => format!(
                    "exit {}: {}",
                    o.status.code().unwrap_or(-1),
                    String::from_utf8_lossy(&o.stderr).trim()
                ),
                Ok(Err(e)) => format!("error: {e}"),
                Err(_) => "timed out".to_string(),
            };
            self.runtime_memo.lock().insert(memo_key, value.clone());
            out.insert(cmd.clone(), value);
        }
        out
    }

    async fn restore_outputs(&self, ws: &Path, def: &TaskDef, entry: &CacheEntry) -> Result<()> {
        // Fetch everything first so a missing blob leaves the workspace untouched.
        let mut blobs = Vec::with_capacity(entry.outputs.len());
        for out in &entry.outputs {
            blobs.push(self.blob(&out.digest).await?);
        }
        files::clear_outputs(ws, &def.outputs)?;
        for (out, bytes) in entry.outputs.iter().zip(blobs) {
            files::write_file(&files::safe_join(ws, &out.path)?, &bytes, out.executable)?;
        }
        Ok(())
    }

    /// Compares this run's key inputs with the task's last cached run.
    /// Best effort: any lookup failure just means no explanation.
    async fn explain_miss(
        &self,
        client: &Client,
        scope: &str,
        inputs: &crate::graph::KeyInputs,
        deps: &[(String, String)],
    ) -> Option<String> {
        // A rerun after a failure: failures are not cached, and the inputs
        // matter more than how they differ from some older success.
        if let Ok(Some(v)) = client
            .get_opt::<serde_json::Value>(&format!("/api/cache/failed/{}", inputs.key()))
            .await
        {
            return Some(format!(
                "it failed with these same inputs in {} (failures are not cached)",
                v["runId"].as_str().unwrap_or("an earlier run")
            ));
        }
        let last = match client
            .get_opt::<CacheEntry>(&format!("/api/cache/last/{scope}"))
            .await
        {
            Ok(Some(entry)) => entry,
            Ok(None) => return Some("nothing was cached for it yet".into()),
            Err(e) if e.to_string().contains("410") => {
                return Some(
                    "its earlier result was evicted to keep the cache under its size limit".into(),
                )
            }
            Err(_) => return None,
        };
        let prev: crate::graph::KeyInputs =
            serde_json::from_slice(&self.blob(last.inputs.as_deref()?).await.ok()?).ok()?;
        if prev.v != inputs.v {
            return Some("Pit's cache format changed".into());
        }
        if prev.key() == inputs.key() {
            return Some(
                "its only cached result came from an untrusted run (a PR, branch or `pit run`), \
                 which trusted runs do not reuse"
                    .into(),
            );
        }
        Some(crate::graph::explain_miss(&prev, inputs, deps))
    }
}

/// The local ID of a container image, pulling it first if this worker does
/// not have it (the run would pull it anyway).
/// Where a shard's dealt files are listed, relative to the workspace: one
/// file per task, since two sharded tasks of a group run side by side.
/// A task name as one safe file name.
fn safe_task_file(task: &str) -> String {
    task.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || "._-".contains(c) {
                c
            } else {
                '_'
            }
        })
        .collect()
}

fn shard_list(task: &str) -> String {
    let safe = safe_task_file(task);
    format!(".pit/shards/{safe}")
}
/// Lists longer than this go only in the `shard_list` file, not the
/// environment.
const SHARD_ENV_BYTES: usize = 64 << 10;

async fn image_id(image: &str) -> String {
    let inspect = || async {
        tokio::process::Command::new("docker")
            .args(["image", "inspect", "--format", "{{.Id}}", image])
            .stdin(std::process::Stdio::null())
            .output()
            .await
            .ok()
            .filter(|o| o.status.success())
            .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
    };
    if let Some(id) = inspect().await {
        return id;
    }
    let _ = tokio::process::Command::new("docker")
        .args(["pull", "-q", image])
        .stdin(std::process::Stdio::null())
        .output()
        .await;
    inspect().await.unwrap_or_else(|| "unavailable".into())
}

/// `KEY=VALUE` lines; blank lines and `#` comments are skipped, and a value
/// may be wrapped in matching quotes.
fn parse_secrets(text: &str) -> HashMap<String, String> {
    text.lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.starts_with('#'))
        .filter_map(|l| l.split_once('='))
        .map(|(k, v)| {
            let v = v.trim();
            let unquoted = ['"', '\'']
                .iter()
                .find_map(|q| v.strip_prefix(*q).and_then(|v| v.strip_suffix(*q)))
                .unwrap_or(v);
            (k.trim().to_string(), unquoted.to_string())
        })
        .collect()
}

/// A task's time limits: its own settings, or an hour in total and fifteen
/// minutes without output, so a hung task never holds a worker forever.
/// `0` turns a limit off.
fn limits(def: &TaskDef) -> exec::Limits {
    let pick = |v: Option<u64>, default: u64| match v.unwrap_or(default) {
        0 => None,
        n => Some(Duration::from_secs(n)),
    };
    exec::Limits {
        timeout: pick(def.timeout, 60 * 60),
        idle: pick(def.idle_timeout, 15 * 60),
    }
}

/// Batches log lines to the server every 50ms so the CLI and dashboard see
/// output live without a request per line.
async fn forward_logs(
    client: Client,
    task_id: String,
    mut rx: mpsc::UnboundedReceiver<String>,
    masker: Arc<mask::Masker>,
) {
    let path = format!("/api/tasks/{task_id}/log");
    let mut buf = String::new();
    // Set when the buffer gets its first line, so steady output cannot keep
    // pushing the flush back.
    let mut deadline: Option<tokio::time::Instant> = None;
    loop {
        let flush_at = deadline;
        tokio::select! {
            line = rx.recv() => match line {
                Some(l) => {
                    buf.push_str(&l);
                    deadline.get_or_insert_with(|| tokio::time::Instant::now() + Duration::from_millis(50));
                    if buf.len() < 64 * 1024 {
                        continue;
                    }
                }
                None => {
                    if !buf.is_empty() {
                        let text = masker.mask(&std::mem::take(&mut buf));
                        let _ = client.post_empty(&path, &LogChunk { text }).await;
                    }
                    return;
                }
            },
            _ = async { tokio::time::sleep_until(flush_at.unwrap()).await }, if flush_at.is_some() => {}
        }
        deadline = None;
        if !buf.is_empty() {
            let text = masker.mask(&std::mem::take(&mut buf));
            let _ = client.post_empty(&path, &LogChunk { text }).await;
        }
    }
}

#[cfg(test)]
mod secrets_file_tests {
    use super::parse_secrets;

    #[test]
    fn parses_dotenv_style_lines() {
        let s = parse_secrets(
            "# deploy\nDEPLOY_TOKEN=abc=123\n\n NPM_TOKEN = \"q v\" \nBAD LINE\nX='y'\n",
        );
        assert_eq!(s["DEPLOY_TOKEN"], "abc=123");
        assert_eq!(s["NPM_TOKEN"], "q v");
        assert_eq!(s["X"], "y");
        assert_eq!(s.len(), 3);
    }
}
