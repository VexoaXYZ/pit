//! Command implementations for the `pit` binary.

use std::collections::HashMap;
use std::io::{IsTerminal, Write};
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::time::Instant;

use anyhow::Result;

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
    } = opts;
    // The built-in worker always proves itself; other workers need the same
    // PIT_WORKER_TOKEN to join.
    let worker_token = worker_token.unwrap_or_else(|| uuid::Uuid::new_v4().simple().to_string());
    let public = format!("http://{addr}");
    let app = crate::server::App::new(&data.join("server"), token.clone(), public.clone())?;
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
                no_secrets: false,
                trusted: false,
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
    client
        .events(&format!("/api/runs/{run_id}/events"), |e| {
            on_event(&e);
            !(e.kind == "run.finished"
                || (e.kind == "snapshot"
                    && serde_json::from_value::<RunView>(e.data.clone())
                        .is_ok_and(|v| v.status.is_done())))
        })
        .await?;
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
    let mut stderr = std::io::stderr();
    let view = wait(client, &sub.run_id, |e| {
        match e.kind.as_str() {
            "snapshot" => {
                if let Ok(v) = serde_json::from_value::<RunView>(e.data.clone()) {
                    for g in v.groups {
                        groups.insert(g.id.clone(), (g.name.clone(), g.os.clone()));
                        // Groups finished before streaming began (answered by
                        // the cache) get no live events; print them now.
                        if !json && g.status.is_done() {
                            let who = g.worker.as_deref().unwrap_or("?");
                            let _ = writeln!(
                                stderr,
                                "  {} {}",
                                style.paint("1", &g.name),
                                style.dim(&format!("on {who}"))
                            );
                            for t in &g.tasks {
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
    })
    .await?;
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
    Ok(if view.status.is_ok() { 0 } else { 1 })
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

pub async fn stats(client: &Client) -> Result<i32> {
    let s: serde_json::Value = client.get("/api/stats").await?;
    println!("{}", serde_json::to_string_pretty(&s)?);
    Ok(0)
}
