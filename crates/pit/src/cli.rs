//! Command implementations for the `pit` binary.

use std::collections::HashMap;
use std::io::{IsTerminal, Write};
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::time::Instant;

use anyhow::{Context, Result};

use crate::client::Client;
use crate::model::*;

pub fn client(server: &str, token: Option<String>) -> Result<Client> {
    Client::new(server, token)
}

pub struct UpOpts {
    pub addr: SocketAddr,
    pub data: PathBuf,
    pub slots: usize,
    pub parallelism: usize,
    pub token: Option<String>,
    pub cache_max_bytes: u64,
    pub workspace_max_bytes: u64,
    pub worker_token: Option<String>,
    pub notify_url: Option<String>,
    pub secrets_file: Option<PathBuf>,
    /// For links; the built-in worker still uses `addr`.
    pub public_url: Option<String>,
}

/// Server and worker in one process.
pub async fn up(opts: UpOpts) -> Result<()> {
    let UpOpts {
        addr,
        data,
        slots,
        parallelism,
        token,
        cache_max_bytes,
        workspace_max_bytes,
        worker_token,
        notify_url,
        secrets_file,
        public_url,
    } = opts;
    // The built-in worker always proves itself; other workers need the same
    // PIT_WORKER_TOKEN to join.
    let worker_token = worker_token.unwrap_or_else(|| uuid::Uuid::new_v4().simple().to_string());
    let public = format!("http://{addr}");
    // Before anything starts: a bad secrets file stops the built-in worker,
    // and `pit up` should not say it is up first.
    if let Some(p) = &secrets_file {
        let text = std::fs::read_to_string(p)
            .with_context(|| format!("reading secrets file {}", p.display()))?;
        crate::worker::parse_secrets(&text)
            .with_context(|| format!("reading secrets file {}", p.display()))?;
    }
    let links = public_url
        .map(|u| u.trim_end_matches('/').to_string())
        .filter(|u| !u.is_empty())
        .unwrap_or_else(|| public.clone());
    let app = crate::server::App::new(&data.join("server"), token.clone(), links)?;
    app.cache_max_bytes
        .store(cache_max_bytes, std::sync::atomic::Ordering::Relaxed);
    *app.worker_token.lock() = Some(worker_token.clone());
    *app.notify_url.lock() = notify_url;
    let server = tokio::spawn(crate::server::serve_app(app, addr));
    // Wait for the listener before the worker starts polling.
    let probe = Client::new(&public, token.clone())?;
    for _ in 0..100 {
        if probe.get::<serde_json::Value>("/api/health").await.is_ok() {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    eprintln!("pit is up: dashboard {public}  ·  data {}", data.display());
    eprintln!("run `pit run` in any git repository with a pit.config.ts");
    let worker = tokio::spawn(crate::worker::run(crate::worker::WorkerOpts {
        server: public,
        token,
        name: Some("local".into()),
        data_dir: data.join("worker"),
        slots,
        parallelism,
        workspace_max_bytes,
        worker_token: Some(worker_token),
        secrets_file,
    }));
    // Ctrl-C or SIGTERM drains the worker (running groups finish; a second
    // one hands them back), and then `pit up` exits.
    tokio::select! {
        r = server => r??,
        r = worker => r??,
    }
    Ok(())
}

pub struct Submitted {
    pub run_id: String,
    pub url: String,
    pub snapshot: crate::snapshot::Snapshot,
}

/// Snapshots the working tree and starts a run on it.
/// How a local run treats earlier state.
#[derive(Clone, Copy, Default)]
pub struct RunOpts {
    /// Ignore cached results.
    pub no_cache: bool,
    /// Use a new, empty workspace instead of a warm one.
    pub fresh: bool,
    /// Keep a failed task's workspace and write a shell script into it.
    pub debug: bool,
}

pub async fn submit(
    client: &Client,
    dir: &Path,
    tasks: Vec<String>,
    opts: RunOpts,
) -> Result<Submitted> {
    let snapshot = crate::snapshot::local(client, dir).await?;
    let created: serde_json::Value = client
        .post(
            "/api/runs",
            &CreateRun {
                repo: snapshot.repo.clone(),
                url: Some(snapshot.url.clone()),
                sha: snapshot.sha.clone(),
                overlay: snapshot.overlay.clone(),
                tasks,
                trigger: Some("local".into()),
                git_ref: snapshot.branch.clone(),
                no_cache: opts.no_cache,
                fresh: opts.fresh,
                debug: opts.debug,
                counts_as: None,
                no_secrets: false,
                trusted: false,
                draft: false,
            },
        )
        .await?;
    let run_id = created["id"]
        .as_str()
        .filter(|id| !id.is_empty())
        .ok_or_else(|| anyhow::anyhow!("server did not return a run id: {created}"))?
        .to_string();
    Ok(Submitted {
        run_id,
        url: created["url"].as_str().unwrap_or_default().to_string(),
        snapshot,
    })
}

/// Waits for a run to finish, calling `on_event` for every live event.
pub async fn wait(
    client: &Client,
    run_id: &str,
    mut on_event: impl FnMut(&Event),
) -> Result<RunView> {
    // A proxy in front of the server (Traefik, a load balancer) can drop a
    // long-lived stream, or end it on a reload. The run goes on: reconnect,
    // and the new stream's snapshot fills in what happened meanwhile. Give
    // up only after a minute without a connection.
    let mut failing_since: Option<std::time::Instant> = None;
    loop {
        let mut done = false;
        let streamed = client
            .events(&format!("/api/runs/{run_id}/events"), |e| {
                on_event(&e);
                done = e.kind == "run.finished"
                    || (e.kind == "snapshot"
                        && serde_json::from_value::<RunView>(e.data.clone())
                            .is_ok_and(|v| v.status.is_done()));
                if !done {
                    failing_since = None;
                }
                !done
            })
            .await;
        if done {
            break;
        }
        let since = *failing_since.get_or_insert_with(std::time::Instant::now);
        if since.elapsed() > std::time::Duration::from_secs(60) {
            let e = streamed
                .err()
                .unwrap_or_else(|| anyhow::anyhow!("the stream kept closing"));
            return Err(e.context(format!("following {run_id} (it may still be running)")));
        }
        tokio::time::sleep(std::time::Duration::from_secs(1)).await;
    }
    client.get(&format!("/api/runs/{run_id}")).await
}

struct Style {
    color: bool,
}

impl Style {
    fn paint(&self, code: &str, s: &str) -> String {
        if self.color {
            format!("\x1b[{code}m{s}\x1b[0m")
        } else {
            s.to_string()
        }
    }
    fn dim(&self, s: &str) -> String {
        self.paint("2", s)
    }
    fn status(&self, s: Status) -> String {
        match s {
            Status::Passed => self.paint("32", "✓"),
            Status::Cached => self.paint("36", "◆"),
            Status::Failed | Status::Error => self.paint("31", "✗"),
            Status::Skipped => self.paint("33", "○"),
            _ => self.dim("·"),
        }
    }
}

fn secs(ms: u64) -> String {
    if ms < 1000 {
        format!("{ms}ms")
    } else {
        format!("{:.1}s", ms as f64 / 1000.0)
    }
}

/// One line of `pit run` output for a finished task.
fn task_line(
    style: &Style,
    status: Status,
    name: &str,
    ms: u64,
    exit_code: Option<i64>,
    message: Option<&str>,
) -> String {
    let detail = match status {
        Status::Cached => style.paint("36", "cached"),
        Status::Skipped => style.paint("33", "skipped"),
        Status::Failed => style.paint(
            "31",
            &format!(
                "{} exit {}",
                secs(ms),
                exit_code.map(|c| c.to_string()).unwrap_or("?".into())
            ),
        ),
        _ => secs(ms),
    };
    // Why it ran (cache miss reason), that it was flaky or much slower than
    // usual; failures print their message below.
    let why = message
        .filter(|m| {
            m.starts_with("ran because") || m.starts_with("flaky:") || m.starts_with("slow:")
        })
        .map(|m| format!("  {}", style.dim(m)))
        .unwrap_or_default();
    format!("    {} {name:<24} {detail}{why}", style.status(status))
}

pub async fn run(
    client: &Client,
    dir: &Path,
    tasks: Vec<String>,
    opts: RunOpts,
    verbose: bool,
    json: bool,
) -> Result<i32> {
    let started = Instant::now();
    let sub = submit(client, dir, tasks, opts).await?;
    let style = Style {
        color: std::io::stderr().is_terminal() && std::env::var_os("NO_COLOR").is_none(),
    };
    let snap = &sub.snapshot;
    if !json {
        let overlay = if snap.files > 0 {
            format!(" +{} changed file(s)", snap.files)
        } else {
            String::new()
        };
        eprintln!(
            "{} {} {}@{}{} {}",
            style.paint("1", "pit"),
            sub.run_id,
            snap.repo,
            &snap.sha[..snap.sha.len().min(10)],
            overlay,
            style.dim(&sub.url)
        );
    }
    // task id -> (group, name)
    let mut names: HashMap<String, (String, String)> = HashMap::new();
    let mut groups: HashMap<String, (String, String)> = HashMap::new();
    // What was printed, so a reconnect's snapshot (see `wait`) adds only
    // what was missed: group headers by name, task lines by id.
    let mut headed: std::collections::HashSet<String> = Default::default();
    let mut printed: std::collections::HashSet<String> = Default::default();
    let mut stderr = std::io::stderr();
    let watch = wait(client, &sub.run_id, |e| {
        match e.kind.as_str() {
            "snapshot" => {
                if let Ok(v) = serde_json::from_value::<RunView>(e.data.clone()) {
                    for g in v.groups {
                        groups.insert(g.id.clone(), (g.name.clone(), g.os.clone()));
                        // Groups finished before streaming began (answered by
                        // the cache) get no live events; print them now.
                        if !json && g.status.is_done() {
                            let who = g.worker.as_deref().unwrap_or("?");
                            if headed.insert(g.name.clone()) {
                                let _ = writeln!(
                                    stderr,
                                    "  {} {}",
                                    style.paint("1", &g.name),
                                    style.dim(&format!("on {who}"))
                                );
                            }
                            for t in &g.tasks {
                                if !printed.insert(t.id.clone()) {
                                    continue;
                                }
                                let line = task_line(
                                    &style,
                                    t.status,
                                    &t.name,
                                    t.duration_ms.unwrap_or(0),
                                    t.exit_code.map(i64::from),
                                    t.message.as_deref(),
                                );
                                let _ = writeln!(stderr, "{line}");
                            }
                        }
                        for t in g.tasks {
                            names.insert(t.id, (g.name.clone(), t.name));
                        }
                    }
                }
            }
            "group.started" if !json => {
                let name = e.data["name"].as_str().unwrap_or_default();
                let worker = e.data["worker"].as_str().unwrap_or_default();
                headed.insert(name.to_string());
                let _ = writeln!(
                    stderr,
                    "  {} {}",
                    style.paint("1", name),
                    style.dim(&format!("on {worker}"))
                );
            }
            "group.waiting" if !json => {
                if let Some(msg) = e.data["message"].as_str() {
                    let name = e.data["name"].as_str().unwrap_or_default();
                    let os = e.data["os"].as_str().unwrap_or_default();
                    let _ = writeln!(
                        stderr,
                        "  {} {}",
                        style.paint("1", name),
                        style.dim(&format!("({os}) {msg}"))
                    );
                }
            }
            "group.finished" if !json => {
                let name = e.data["name"].as_str().unwrap_or_default();
                let prep = e.data["prepareMs"].as_u64().unwrap_or(0);
                let _ = writeln!(
                    stderr,
                    "  {} {}",
                    style.dim(name),
                    style.dim(&format!("workspace ready in {}", secs(prep)))
                );
            }
            "task.log" if verbose && !json => {
                let text = e.data["text"].as_str().unwrap_or_default();
                let label = e
                    .task_id
                    .as_ref()
                    .and_then(|t| names.get(t))
                    .map(|(_, n)| n.clone())
                    .unwrap_or_default();
                for line in text.lines() {
                    let _ = writeln!(stderr, "    {} {line}", style.dim(&format!("{label} │")));
                }
            }
            "task.finished" if !json => {
                if let Some(id) = &e.task_id {
                    printed.insert(id.clone());
                }
                let status: Status =
                    serde_json::from_value(e.data["status"].clone()).unwrap_or(Status::Error);
                let line = task_line(
                    &style,
                    status,
                    e.data["name"].as_str().unwrap_or_default(),
                    e.data["durationMs"].as_u64().unwrap_or(0),
                    e.data["exitCode"].as_i64(),
                    e.data["message"].as_str(),
                );
                let _ = writeln!(stderr, "{line}");
            }
            _ => {}
        }
        if e.kind == "task.started" {
            if let (Some(id), Some(name)) = (&e.task_id, e.data["name"].as_str()) {
                names.insert(id.clone(), (String::new(), name.to_string()));
            }
        }
    });
    tokio::pin!(watch);
    // Interrupting `pit run` (Ctrl-C, or SIGTERM from a timeout or an
    // agent) cancels its run, so its tasks stop instead of holding a worker
    // until they time out. A second interrupt quits without waiting.
    let mut interrupted = false;
    let view = tokio::select! {
        v = &mut watch => v?,
        () = interrupt() => {
            interrupted = true;
            eprintln!("cancelling {} (interrupt again to quit now)", sub.run_id);
            if let Err(e) = cancel_run(client, &sub.run_id).await {
                eprintln!("could not cancel {}: {e:#}", sub.run_id);
                return Ok(130);
            }
            tokio::select! {
                v = &mut watch => v?,
                () = interrupt() => return Ok(130),
                () = tokio::time::sleep(std::time::Duration::from_secs(15)) => return Ok(130),
            }
        }
    };
    let elapsed = started.elapsed().as_millis() as u64;

    if json {
        let mut v = serde_json::to_value(&view)?;
        v["elapsedMs"] = elapsed.into();
        v["url"] = sub.url.clone().into();
        println!("{}", serde_json::to_string_pretty(&v)?);
    } else {
        // Failure logs, unless they were already streamed.
        for g in &view.groups {
            for t in g
                .tasks
                .iter()
                .filter(|t| matches!(t.status, Status::Failed | Status::Error))
            {
                if let Some(m) = &t.message {
                    eprintln!("\n  {} {}.{}: {m}", style.status(t.status), g.name, t.name);
                }
                if !verbose {
                    if let Ok(log) = client.get_text(&format!("/api/tasks/{}/log", t.id)).await {
                        for line in crate::excerpt::excerpt(&log, 40).lines() {
                            eprintln!("    {} {line}", style.dim("│"));
                        }
                    }
                }
            }
        }
        // What tasks wrote to `$PIT_SUMMARY`, as they wrote it.
        for g in &view.groups {
            for t in &g.tasks {
                if let Some(md) = t.summary.as_deref().filter(|m| !m.trim().is_empty()) {
                    eprintln!("\n  {} {}.{}", style.dim("summary"), g.name, t.name);
                    for line in md.lines().take(200) {
                        eprintln!("    {} {line}", style.dim("│"));
                    }
                }
            }
        }
        // A pass that took far longer than usual is easy to miss in a green
        // run; name it once more next to the verdict.
        for g in &view.groups {
            for t in &g.tasks {
                if let Some(slow) = t
                    .message
                    .as_deref()
                    .and_then(|m| m.split(" · ").find(|p| p.starts_with("slow:")))
                {
                    eprintln!(
                        "  {} {}.{}: {}",
                        style.paint("33", "!"),
                        g.name,
                        t.name,
                        slow
                    );
                }
            }
        }
        let all: Vec<&TaskView> = view.groups.iter().flat_map(|g| &g.tasks).collect();
        let ran = all
            .iter()
            .filter(|t| matches!(t.status, Status::Passed | Status::Failed))
            .count();
        let cached = all.iter().filter(|t| t.status == Status::Cached).count();
        let verdict = if view.status.is_ok() {
            style.paint("32;1", "passed")
        } else {
            style.paint("31;1", view.status.as_str())
        };
        let mut line = format!(
            "{verdict} in {} · {ran} ran, {cached} cached",
            secs(elapsed)
        );
        if let Some(m) = &view.message {
            line.push_str(&format!(" · {m}"));
        }
        eprintln!("{line}");
    }
    Ok(if interrupted {
        130
    } else if view.status.is_ok() {
        0
    } else {
        1
    })
}

/// Resolves on Ctrl-C or, on Unix, SIGTERM; never if neither can be caught.
async fn interrupt() {
    #[cfg(unix)]
    let term = async {
        match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
            Ok(mut s) => {
                s.recv().await;
            }
            Err(_) => std::future::pending().await,
        }
    };
    #[cfg(not(unix))]
    let term = std::future::pending::<()>();
    let int = async {
        if tokio::signal::ctrl_c().await.is_err() {
            std::future::pending::<()>().await;
        }
    };
    tokio::select! {
        () = int => {}
        () = term => {}
    }
}

/// Whether the call cancelled the run, and the run's status after it.
async fn cancel_run(client: &Client, run: &str) -> Result<(bool, String)> {
    let v: serde_json::Value = client.post(&format!("/api/runs/{run}/cancel"), &()).await?;
    Ok((
        v["cancelled"].as_bool().unwrap_or(false),
        v["status"].as_str().unwrap_or_default().to_string(),
    ))
}

pub async fn cancel(client: &Client, run: &str) -> Result<i32> {
    let (cancelled, status) = cancel_run(client, run).await?;
    if cancelled {
        eprintln!("cancelled {run}; its running tasks are being stopped");
        Ok(0)
    } else {
        eprintln!("{run} had already finished ({status})");
        Ok(1)
    }
}

pub async fn runs(client: &Client, limit: usize) -> Result<i32> {
    let runs: Vec<RunView> = client.get(&format!("/api/runs?limit={limit}")).await?;
    for r in runs {
        let tasks: Vec<&TaskView> = r.groups.iter().flat_map(|g| &g.tasks).collect();
        let cached = tasks.iter().filter(|t| t.status == Status::Cached).count();
        let dur = match (r.created_at, r.finished_at) {
            (c, Some(f)) => secs((f - c).max(0) as u64),
            _ => "…".into(),
        };
        println!(
            "{:<18} {:<8} {:<8} {:<20} {}@{} {} tasks ({} cached) {}",
            r.id,
            r.status.as_str(),
            r.trigger,
            r.git_ref.clone().unwrap_or_default(),
            r.repo,
            &r.sha[..r.sha.len().min(8)],
            tasks.len(),
            cached,
            dur
        );
    }
    Ok(0)
}

/// Tasks that got slower over the last `days`, worst first.
pub async fn slow(client: &Client, days: u32) -> Result<i32> {
    let tasks: Vec<SlowTask> = client.get(&format!("/api/slow?days={days}")).await?;
    if tasks.is_empty() {
        println!("no task got slower in the last {days} day(s)");
        return Ok(0);
    }
    println!(
        "{:<40} {:>9} {:>9} {:>7} {:>8}  last run",
        "task", "before", "recent", "change", "samples"
    );
    for t in tasks {
        println!(
            "{:<40} {:>8.1}s {:>8.1}s {:>6.1}x {:>8}  {}",
            format!("{} {}.{} ({})", t.repo, t.group, t.task, t.os),
            t.before_ms as f64 / 1000.0,
            t.recent_ms as f64 / 1000.0,
            t.recent_ms as f64 / t.before_ms.max(1) as f64,
            t.samples,
            t.last_run
        );
    }
    println!("\n\"recent\": median of the last 5 passing runs; \"before\": median of the ones before them.");
    Ok(0)
}

/// Flaky tests (inside tasks' JUnit reports) over the last `days`.
pub async fn flaky_tests(client: &Client, days: u32) -> Result<i32> {
    let tests: Vec<FlakyTest> = client.get(&format!("/api/flaky/tests?days={days}")).await?;
    if tests.is_empty() {
        println!("no flaky tests in the last {days} day(s) (tasks need `reports` for this)");
        return Ok(0);
    }
    println!(
        "{:<56} {:>8} {:>13} {:>12}  last run",
        "test", "failed", "passed retry", "same inputs"
    );
    for t in tests {
        println!(
            "{:<56} {:>8} {:>13} {:>12}  {}",
            format!("{} {}.{}: {}", t.repo, t.group, t.task, t.test),
            t.failures,
            t.flaky_passes,
            t.mixed_inputs,
            t.last_run
        );
    }
    println!("\n\"passed retry\": failed, then passed when the task was retried. \"same inputs\": failed where the same inputs passed elsewhere.");
    Ok(0)
}

/// Flaky tasks over the last `days`, worst first.
pub async fn flaky(client: &Client, days: u32) -> Result<i32> {
    let tasks: Vec<FlakyTask> = client.get(&format!("/api/flaky?days={days}")).await?;
    if tasks.is_empty() {
        println!("no flaky tasks in the last {days} day(s)");
        return Ok(0);
    }
    println!(
        "{:<32} {:>8} {:>8} {:>13} {:>12}  last run",
        "task", "finished", "failed", "passed retry", "same inputs"
    );
    for t in tasks {
        println!(
            "{:<32} {:>8} {:>8} {:>13} {:>12}  {}",
            format!("{} {}.{}", t.repo, t.group, t.task),
            t.runs,
            t.failures,
            t.flaky_passes,
            t.mixed_inputs,
            t.last_run
        );
    }
    println!("\n\"same inputs\": input sets that both passed and failed. Quarantine a task with `quarantine: true` while it is fixed.");
    Ok(0)
}

/// Where an artifact goes under `out`: refused if the group, task or path
/// (from a run's config and workspace, a fork's included) would leave it.
fn artifact_dest(out: &Path, group: &str, task: &str, path: &str) -> Result<PathBuf> {
    crate::worker::files::safe_join(out, &format!("{group}/{task}/{path}"))
}

/// Downloads a run's artifacts into `out/<group>/<task>/<path>`.
pub async fn artifacts(client: &Client, run: &str, task: Option<&str>, out: &Path) -> Result<i32> {
    let view: RunView = client.get(&format!("/api/runs/{run}")).await?;
    let mut count = 0;
    let mut failed = 0;
    let mut bytes = 0u64;
    for g in &view.groups {
        for t in &g.tasks {
            let full = format!("{}.{}", g.name, t.name);
            if task.is_some_and(|want| want != t.name && want != full) {
                continue;
            }
            for a in &t.artifacts {
                // One checked path: names and paths come from the run's config
                // and workspace, a fork's included.
                let rel = format!("{}/{}/{}", g.name, t.name, a.path);
                let fetched = match artifact_dest(out, &g.name, &t.name, &a.path) {
                    Ok(dest) => match client.cas_get(&a.digest).await {
                        Ok(data) => {
                            crate::worker::files::write_file(&dest, &data, false).map(|_| dest)
                        }
                        Err(e) => Err(e),
                    },
                    Err(e) => Err(e),
                };
                match fetched {
                    Ok(dest) => {
                        count += 1;
                        bytes += a.size;
                        println!("{}", dest.display());
                    }
                    Err(e) => {
                        eprintln!("skipped {rel}: {e:#}");
                        failed += 1;
                    }
                }
            }
        }
    }
    if count == 0 && failed == 0 {
        eprintln!("no artifacts in {run} (tasks keep them with `artifacts: [...]`)");
        return Ok(1);
    }
    eprintln!("{count} file(s), {:.1} MB", bytes as f64 / 1e6);
    Ok(i32::from(failed > 0))
}

pub async fn logs(client: &Client, run: &str, task: &str) -> Result<i32> {
    let text = client
        .get_text(&format!(
            "/api/runs/{}/tasks/{}/log",
            crate::client::seg(run)?,
            crate::client::seg(task)?
        ))
        .await?;
    print!("{text}");
    Ok(0)
}

pub async fn queue_add(
    client: &Client,
    dir: &Path,
    branch: &str,
    target: Option<String>,
) -> Result<i32> {
    let id = crate::snapshot::identity(dir).await?;
    if !id.remote {
        eprintln!(
            "note: {} has no `origin` remote, so Pit will push merges into this checkout",
            id.name
        );
    }
    let entry: QueueEntryView = client
        .post(
            "/api/queue",
            &QueueAdd {
                repo: id.name,
                url: Some(id.url),
                branch: Some(branch.into()),
                pr: None,
                target,
                sha: None,
            },
        )
        .await?;
    println!(
        "{} queued: {} → {} ({})",
        entry.id, entry.source, entry.target, entry.status
    );
    if !entry.affected.is_empty() {
        println!("affected: {}", entry.affected.join(", "));
    }
    Ok(0)
}

pub async fn queue_remove(client: &Client, dir: &Path, entry: &str) -> Result<i32> {
    // Branch names are scoped to this repo; entry ids work from anywhere.
    // The URL identifies the repo even when another checkout shares its name.
    let repo = crate::snapshot::identity(dir).await.map(|id| id.url).ok();
    let mut path = format!("/api/queue/{}", crate::client::seg(entry)?);
    if let Some(repo) = repo {
        if let Ok(repo) = crate::client::seg(&repo) {
            path.push_str(&format!("?repo={repo}"));
        }
    }
    let resp: serde_json::Value = client.delete(&path).await?;
    println!(
        "removed {} entr{}",
        resp["removed"],
        if resp["removed"] == 1 { "y" } else { "ies" }
    );
    Ok(0)
}

pub async fn queue_list(client: &Client) -> Result<i32> {
    let entries: Vec<QueueEntryView> = client.get("/api/queue").await?;
    for e in entries {
        println!(
            "{:<16} {:<8} {:<20} → {:<10} lane {:<16} {} {}",
            e.id,
            e.status,
            e.source,
            e.target,
            e.lane.clone().unwrap_or_default(),
            e.affected.join(","),
            e.reason.clone().unwrap_or_default()
        );
    }
    Ok(0)
}

/// A time in a zone (the machine's when `None`), with how far off it is.
fn local_time(ms: i64, now_ms: i64, zone: Option<&str>) -> String {
    let tz = zone
        .and_then(|z| jiff::tz::TimeZone::get(z).ok())
        .unwrap_or_else(jiff::tz::TimeZone::system);
    let at = jiff::Timestamp::from_millisecond(ms)
        .map(|t| t.to_zoned(tz).strftime("%Y-%m-%d %H:%M %Z").to_string())
        .unwrap_or_default();
    let mins = (ms - now_ms).abs() / 60_000;
    let span = match mins {
        0 => "now".to_string(),
        m if m < 60 => format!("{m}m"),
        m if m < 48 * 60 => format!("{}h {}m", m / 60, m % 60),
        m => format!("{}d", m / (24 * 60)),
    };
    match (mins, ms > now_ms) {
        (0, _) => format!("{at} (now)"),
        (_, true) => format!("{at} (in {span})"),
        (_, false) => format!("{at} ({span} ago)"),
    }
}

pub async fn schedules(client: &Client) -> Result<i32> {
    let list: Vec<serde_json::Value> = client.get("/api/schedules").await?;
    let now = now_ms();
    let mut broken = false;
    if list.is_empty() {
        eprintln!("no schedules (the server reads `schedules` from each repo's default branch)");
    }
    for s in &list {
        let repo = s["repo"].as_str().unwrap_or_default();
        if let Some(e) = s["error"].as_str() {
            broken = true;
            println!("{repo}: schedules not read, so none will fire: {e}");
            continue;
        }
        let tz = s["timezone"]
            .as_str()
            .map(|t| format!(" {t}"))
            .unwrap_or_else(|| " UTC".into());
        println!(
            "{repo} {} · {}{tz} · on {}",
            s["name"].as_str().unwrap_or_default(),
            s["cron"].as_str().unwrap_or_default(),
            s["branch"].as_str().unwrap_or_default(),
        );
        match s["nextAt"].as_i64() {
            // In the zone the cron is written in.
            Some(at) => println!(
                "  next  {}",
                local_time(at, now, Some(s["timezone"].as_str().unwrap_or("UTC")))
            ),
            None => println!("  next  none in the next year"),
        }
        match s["lastRun"].as_object() {
            Some(r) => println!(
                "  last  {} {} {}",
                r.get("id").and_then(|v| v.as_str()).unwrap_or_default(),
                r.get("status").and_then(|v| v.as_str()).unwrap_or_default(),
                local_time(
                    r.get("createdAt").and_then(|v| v.as_i64()).unwrap_or(0),
                    now,
                    None
                ),
            ),
            None => println!("  last  never ran"),
        }
    }
    Ok(i32::from(broken))
}

pub async fn schedule_run(client: &Client, name: &str, repo: Option<&str>) -> Result<i32> {
    let v: serde_json::Value = client
        .post(
            "/api/schedules/run",
            &serde_json::json!({ "name": name, "repo": repo }),
        )
        .await?;
    eprintln!(
        "started {} {}",
        v["id"].as_str().unwrap_or_default(),
        v["url"].as_str().unwrap_or_default()
    );
    Ok(0)
}

pub async fn stats(client: &Client) -> Result<i32> {
    let s: serde_json::Value = client.get("/api/stats").await?;
    println!("{}", serde_json::to_string_pretty(&s)?);
    Ok(0)
}

#[cfg(test)]
mod artifact_path_tests {
    use super::artifact_dest;
    use std::path::Path;

    #[test]
    fn downloads_never_leave_the_output_directory() {
        let out = Path::new("/tmp/out");
        assert_eq!(
            artifact_dest(out, "ci", "typecheck:@acme/web", "report/a.png").unwrap(),
            Path::new("/tmp/out/ci/typecheck:@acme/web/report/a.png")
        );
        for (g, t, p) in [
            ("/home/u/sub", "..", ".bashrc"),
            ("ci", "..", "../x"),
            ("..", "t", "x"),
            ("ci", "t", "a/../../x"),
        ] {
            assert!(artifact_dest(out, g, t, p).is_err(), "{g} {t} {p}");
        }
        // A leading `/` in the path is just an empty component: still inside.
        assert!(artifact_dest(out, "ci", "t", "/etc/passwd")
            .unwrap()
            .starts_with("/tmp/out/ci/t"));
    }
}
