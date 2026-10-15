//! The worker daemon. It keeps warm state between jobs: a bare mirror per repo,
//! one persistent workspace per slot (ignored files such as `node_modules` and
//! `target` survive between runs), and a node-local blob cache.
//!
//! A job is a whole group of tasks. They run in one workspace, in dependency
//! order, with independent tasks in parallel.

pub mod exec;
pub mod files;
pub mod mask;

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
    /// `runtime` command outputs, per job (`group id \0 command`).
    runtime_memo: Mutex<HashMap<String, String>>,
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
    pool: String,
    path: PathBuf,
}

impl Drop for WorkspaceLease<'_> {
    fn drop(&mut self) {
        if let Some(slots) = self.worker.workspaces.lock().get_mut(&self.pool) {
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
            runtime_memo: Default::default(),
        })
    }

    /// The most recently used free workspace for this repo and group, so the
    /// group's dependencies and build output from its last run are still
    /// there. Groups do different work (a Rust build, a JS install), so they
    /// never share warm state. Creates a workspace if all are busy.
    fn lease_workspace(&self, repo: &str, group: &str) -> WorkspaceLease<'_> {
        let dir = self
            .data_dir
            .join("workspaces")
            .join(safe_name(repo))
            .join(safe_name(group));
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
        // Every update for this job carries its lease, so a worker that lost
        // the group (reaped, then re-claimed elsewhere) cannot overwrite it.
        let client = self.client.with_lease(&job.lease);
        // A heartbeat refused with 409 means the group is no longer ours: the
        // run was cancelled or superseded (or we were presumed dead). Then the
        // running tasks are stopped.
        let (cancel_tx, cancel_rx) = tokio::sync::watch::channel(false);
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
        let lease = self.lease_workspace(&job.repo, &job.group);
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
                let status = self
                    .clone()
                    .run_tasks(&job, &ws, Arc::new(files), cancel_rx)
                    .await;
                GroupResult {
                    status,
                    message: None,
                    prepare_ms,
                }
            }
        };
        hb.abort();
        drop(lease);
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
                if deps.iter().all(|d| d.is_some()) && running.len() < self.parallelism {
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
        let failed = state
            .values()
            .any(|(s, _)| matches!(s, Status::Failed | Status::Error));
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
                    if let Some(entry) = client
                        .get_opt::<CacheEntry>(&format!("/api/cache/{key}"))
                        .await?
                    {
                        let t = Instant::now();
                        // The server may evict the outputs between the lookup
                        // and the restore. Then the task just runs.
                        if let Err(e) = self.restore_outputs(ws, def, &entry).await {
                            restore_failed =
                                Some(format!("its cached outputs could not be restored ({e:#})"));
                        } else {
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

        let backend = def.backend.unwrap_or_default();
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
        // Never hand Pit's own settings (API token) back through `env`. Windows
        // variable names are case-insensitive, so compare uppercased.
        let mut secret_values = Vec::new();
        let wanted = def
            .env
            .iter()
            .map(|k| (k, false))
            .chain(def.secrets.iter().map(|k| (k, true)));
        for (k, secret) in wanted {
            if k.to_ascii_uppercase().starts_with("PIT_") {
                continue;
            }
            if let Ok(v) = std::env::var(k) {
                if secret {
                    secret_values.push(v.clone());
                }
                env.push((k.clone(), v));
            }
        }
        // Secrets never reach the server's logs, live or stored.
        let masker = Arc::new(mask::Masker::new(secret_values));
        // The task's own retries, plus the job's automatic ones (merge queue).
        let attempts = 1 + def.retries.unwrap_or(0) + job.flake_retries;
        let mut log = String::new();
        let mut failures: Vec<String> = Vec::new();
        let mut duration_ms = 0u64;
        let mut attempt = 0;
        let outcome = loop {
            attempt += 1;
            let (tx, rx) = mpsc::unbounded_channel::<String>();
            let forwarder = tokio::spawn(forward_logs(
                client.clone(),
                task.id.clone(),
                rx,
                masker.clone(),
            ));
            let cmd = exec::command(def, backend, ws, &env);
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
        if ok && !job.no_cache && failures.is_empty() {
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
            message = Some(format!(
                "exited with {}",
                outcome
                    .exit_code
                    .map(|c| c.to_string())
                    .unwrap_or_else(|| "a signal".into())
            ));
        }
        // Say why the task ran instead of coming from the cache.
        if let Some(note) = miss_note {
            message = Some(match message {
                Some(m) => format!("{m} · ran because {note}"),
                None => format!("ran because {note}"),
            });
        }
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

fn warm_file(ws: &Path) -> PathBuf {
    ws.join(".git").join("pit-warm.json")
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
        for cmd in &def.runtime {
            let memo_key = format!("{}\0{cmd}", job.group_id);
            if let Some(v) = self.runtime_memo.lock().get(&memo_key).cloned() {
                out.insert(cmd.clone(), v);
                continue;
            }
            let (shell, flag) = if cfg!(windows) {
                ("cmd", "/C")
            } else {
                ("sh", "-c")
            };
            let run = tokio::process::Command::new(shell)
                .args([flag, cmd])
                .current_dir(ws)
                .stdin(std::process::Stdio::null())
                .output();
            let value = match tokio::time::timeout(Duration::from_secs(15), run).await {
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
        Some(crate::graph::explain_miss(&prev, inputs, deps))
    }
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
