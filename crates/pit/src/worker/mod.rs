//! The worker daemon. It keeps warm state between jobs: a bare mirror per repo,
//! one persistent workspace per slot (ignored files such as `node_modules` and
//! `target` survive between runs), and a node-local blob cache.
//!
//! A job is a whole group of tasks. They run in one workspace, in dependency
//! order, with independent tasks in parallel.

pub mod exec;
pub mod files;

use std::collections::HashMap;
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
}

pub struct Worker {
    client: Client,
    info: WorkerInfo,
    data_dir: PathBuf,
    cas: Cas,
    parallelism: usize,
    mirror_locks: Mutex<HashMap<String, Arc<tokio::sync::Mutex<()>>>>,
    workspaces: Mutex<HashMap<String, Vec<WorkspaceSlot>>>,
}

/// A persistent workspace. Jobs get the warmest free one for their repo.
struct WorkspaceSlot {
    path: PathBuf,
    busy: bool,
    last_used: Instant,
}

/// Returns its workspace to the pool when the job ends.
struct WorkspaceLease<'a> {
    worker: &'a Worker,
    repo: String,
    path: PathBuf,
}

impl Drop for WorkspaceLease<'_> {
    fn drop(&mut self) {
        if let Some(slots) = self.worker.workspaces.lock().get_mut(&self.repo) {
            if let Some(s) = slots.iter_mut().find(|s| s.path == self.path) {
                s.busy = false;
                s.last_used = Instant::now();
            }
        }
    }
}

pub async fn run(opts: WorkerOpts) -> Result<()> {
    let worker = Arc::new(
        Worker::new(
            opts.server.as_str(),
            opts.token.clone(),
            opts.name.clone(),
            &opts.data_dir,
            opts.parallelism,
        )
        .await?,
    );
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
    let mut loops = JoinSet::new();
    for slot in 0..opts.slots.max(1) {
        let w = worker.clone();
        loops.spawn(async move { w.claim_loop(slot).await });
    }
    while loops.join_next().await.is_some() {}
    Ok(())
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
        })
    }

    /// The most recently used free workspace for `repo`, so dependencies and
    /// build output from the last run are still there. Creates one if all are busy.
    fn lease_workspace(&self, repo: &str) -> WorkspaceLease<'_> {
        let mut pools = self.workspaces.lock();
        let slots = pools.entry(repo.to_string()).or_default();
        if slots.is_empty() {
            // Pick up workspaces left by a previous worker process.
            let dir = self.data_dir.join("workspaces").join(safe_name(repo));
            if let Ok(entries) = std::fs::read_dir(&dir) {
                for e in entries.flatten() {
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
                let path = self
                    .data_dir
                    .join("workspaces")
                    .join(safe_name(repo))
                    .join(index.to_string());
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
            repo: repo.to_string(),
            path,
        }
    }

    async fn claim_loop(self: Arc<Self>, slot: usize) {
        let mut backoff = Duration::from_millis(500);
        loop {
            match self.client.claim(&self.info).await {
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
        let hb = {
            let client = self.client.clone();
            let gid = job.group_id.clone();
            tokio::spawn(async move {
                loop {
                    tokio::time::sleep(Duration::from_secs(5)).await;
                    let _ = client
                        .post_empty(&format!("/api/groups/{gid}/heartbeat"), &())
                        .await;
                }
            })
        };
        let lease = self.lease_workspace(&job.repo);
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
            Ok(files) => {
                let status = self.clone().run_tasks(&job, &ws, Arc::new(files)).await;
                GroupResult {
                    status,
                    message: None,
                    prepare_ms,
                }
            }
        };
        hb.abort();
        drop(lease);
        let status = result.status;
        self.client
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
    /// files warm. Returns the input file index used for action keys.
    async fn prepare(&self, job: &Job, ws: &Path) -> Result<Vec<(String, String)>> {
        let mirror = self
            .data_dir
            .join("mirrors")
            .join(format!("{}.git", safe_name(&job.repo)));
        {
            let lock = self.mirror_lock(&job.repo);
            let _g = lock.lock().await;
            git::ensure_commit(&mirror, &job.url, &job.sha).await?;
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
        git::git(ws, &["checkout", "-q", "--force", "--detach", &job.sha]).await?;
        // Untracked files go, ignored ones (dependencies, build dirs) stay warm.
        git::git(ws, &["clean", "-q", "-ffd"]).await?;

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
        Ok(files)
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
    ) -> Status {
        let warm_path = warm_file(ws);
        let warm: Arc<Mutex<HashMap<String, String>>> = Arc::new(Mutex::new(
            std::fs::read(&warm_path)
                .ok()
                .and_then(|b| serde_json::from_slice(&b).ok())
                .unwrap_or_default(),
        ));
        let mut state: HashMap<String, (Status, Option<String>)> = HashMap::new();
        let mut started: std::collections::HashSet<String> = Default::default();
        let mut running: JoinSet<(String, Status, Option<String>)> = JoinSet::new();
        loop {
            for t in &job.tasks {
                if started.contains(&t.name) {
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
                    let _ = self
                        .client
                        .post_empty(&format!("/api/tasks/{}/finish", t.id), &r)
                        .await;
                    continue;
                }
                if deps.iter().all(|d| d.is_some()) && running.len() < self.parallelism {
                    started.insert(t.name.clone());
                    let dep_keys: Vec<String> = deps
                        .iter()
                        .filter_map(|d| d.and_then(|(_, k)| k.clone()))
                        .collect();
                    let w = self.clone();
                    let (task, job, ws, files, warm) = (
                        t.clone(),
                        job.clone(),
                        ws.to_path_buf(),
                        files.clone(),
                        warm.clone(),
                    );
                    running.spawn(async move {
                        let (status, key) =
                            match w.run_task(&job, &task, &ws, &files, &dep_keys, &warm).await {
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
        let failed = state
            .values()
            .any(|(s, _)| matches!(s, Status::Failed | Status::Error));
        if failed {
            Status::Failed
        } else {
            Status::Passed
        }
    }

    async fn run_task(
        &self,
        job: &Job,
        task: &JobTask,
        ws: &Path,
        files: &[(String, String)],
        dep_keys: &[String],
        warm: &Mutex<HashMap<String, String>>,
    ) -> Result<(Status, Option<String>)> {
        let def = &task.def;
        let key = crate::graph::action_key(def, &job.os, files, dep_keys)?;
        self.client
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
                    if let Some(entry) = self
                        .client
                        .get_opt::<CacheEntry>(&format!("/api/cache/{key}"))
                        .await?
                    {
                        let t = Instant::now();
                        files::clear_outputs(ws, &def.outputs)?;
                        for out in &entry.outputs {
                            let dest = files::safe_join(ws, &out.path)?;
                            files::write_file(
                                &dest,
                                &self.blob(&out.digest).await?,
                                out.executable,
                            )?;
                        }
                        let msg = format!(
                            "cache hit, saved {:.1}s ({} output file(s) restored in {}ms)",
                            entry.duration_ms as f64 / 1000.0,
                            entry.outputs.len(),
                            t.elapsed().as_millis()
                        );
                        let r = finish(
                            Status::Cached,
                            Some(0),
                            t.elapsed().as_millis() as u64,
                            entry.log_digest.clone(),
                            Some(msg),
                        );
                        self.client
                            .post_empty(&format!("/api/tasks/{}/finish", task.id), &r)
                            .await?;
                        return Ok((Status::Cached, Some(key)));
                    }
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
                        self.client
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

        let backend = def.backend.unwrap_or_default();
        let mut env: Vec<(String, String)> = vec![
            ("CI".into(), "true".into()),
            ("PIT".into(), "1".into()),
            ("PIT_TASK".into(), task.name.clone()),
            ("PIT_RUN_ID".into(), job.run_id.clone()),
            ("PIT_SHA".into(), job.sha.clone()),
        ];
        for k in &def.env {
            if let Ok(v) = std::env::var(k) {
                env.push((k.clone(), v));
            }
        }
        let (tx, rx) = mpsc::unbounded_channel::<String>();
        let forwarder = tokio::spawn(forward_logs(self.client.clone(), task.id.clone(), rx));
        let cmd = exec::command(def, backend, ws, &env);
        let outcome = exec::run(cmd, def.timeout.map(Duration::from_secs), tx).await;
        let _ = forwarder.await;
        let outcome = outcome?;
        let duration_ms = outcome.duration.as_millis() as u64;
        let log_bytes = outcome.log.into_bytes();
        let log_digest = crate::cas::digest(&log_bytes);
        self.client
            .upload_missing(vec![(log_digest.clone(), log_bytes)])
            .await?;

        let ok = outcome.exit_code == Some(0) && !outcome.timed_out;
        let mut message = outcome.timed_out.then(|| "timed out".to_string());
        if ok && !job.no_cache {
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
                    self.client.upload_missing(blobs).await?;
                    let entry = CacheEntry {
                        log_digest: Some(log_digest.clone()),
                        outputs: manifest,
                        duration_ms,
                    };
                    self.client
                        .put_json(&format!("/api/cache/{key}"), &entry)
                        .await?;
                }
                CacheMode::Workspace => {
                    warm.lock().insert(task.name.clone(), key.clone());
                }
                CacheMode::Off => {}
            }
        }
        if !ok && message.is_none() {
            message = Some(format!(
                "exited with {}",
                outcome
                    .exit_code
                    .map(|c| c.to_string())
                    .unwrap_or_else(|| "a signal".into())
            ));
        }
        let status = if ok { Status::Passed } else { Status::Failed };
        let r = finish(
            status,
            outcome.exit_code,
            duration_ms,
            Some(log_digest),
            message,
        );
        self.client
            .post_empty(&format!("/api/tasks/{}/finish", task.id), &r)
            .await?;
        Ok((status, Some(key)))
    }
}

fn warm_file(ws: &Path) -> PathBuf {
    ws.join(".git").join("pit-warm.json")
}

/// Which workspace-cached tasks this workspace has run, by action key.
fn save_warm(ws: &Path, warm: &HashMap<String, String>) {
    if let Ok(bytes) = serde_json::to_vec(warm) {
        let _ = std::fs::write(warm_file(ws), bytes);
    }
}

/// Batches log lines to the server every 50ms so the CLI and dashboard see
/// output live without a request per line.
async fn forward_logs(client: Client, task_id: String, mut rx: mpsc::UnboundedReceiver<String>) {
    let path = format!("/api/tasks/{task_id}/log");
    let mut buf = String::new();
    loop {
        tokio::select! {
            line = rx.recv() => match line {
                Some(l) => {
                    buf.push_str(&l);
                    if buf.len() < 64 * 1024 {
                        continue;
                    }
                }
                None => {
                    if !buf.is_empty() {
                        let _ = client.post_empty(&path, &LogChunk { text: std::mem::take(&mut buf) }).await;
                    }
                    return;
                }
            },
            _ = tokio::time::sleep(Duration::from_millis(50)), if !buf.is_empty() => {}
        }
        if !buf.is_empty() {
            let _ = client
                .post_empty(
                    &path,
                    &LogChunk {
                        text: std::mem::take(&mut buf),
                    },
                )
                .await;
        }
    }
}
