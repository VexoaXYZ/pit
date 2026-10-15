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

/// Server and worker in one process.
pub async fn up(
    addr: SocketAddr,
    data: PathBuf,
    slots: usize,
    parallelism: usize,
    token: Option<String>,
) -> Result<()> {
    let public = format!("http://{addr}");
    let app = crate::server::App::new(&data.join("server"), token.clone(), public.clone())?;
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
    }));
    tokio::select! {
        r = server => r??,
        r = worker => r??,
        _ = tokio::signal::ctrl_c() => {}
    }
    Ok(())
}

pub struct Submitted {
    pub run_id: String,
    pub url: String,
    pub snapshot: crate::snapshot::Snapshot,
}

/// Snapshots the working tree and starts a run on it.
pub async fn submit(
    client: &Client,
    dir: &Path,
    tasks: Vec<String>,
    no_cache: bool,
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
                no_cache,
            },
        )
        .await?;
    Ok(Submitted {
        run_id: created["id"].as_str().unwrap_or_default().to_string(),
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

pub async fn run(
    client: &Client,
    dir: &Path,
    tasks: Vec<String>,
    no_cache: bool,
    verbose: bool,
    json: bool,
) -> Result<i32> {
    let started = Instant::now();
    let sub = submit(client, dir, tasks, no_cache).await?;
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
                let name = e.data["name"].as_str().unwrap_or_default();
                let ms = e.data["durationMs"].as_u64().unwrap_or(0);
                let detail = match status {
                    Status::Cached => style.paint("36", "cached"),
                    Status::Skipped => style.paint("33", "skipped"),
                    Status::Failed => style.paint(
                        "31",
                        &format!(
                            "{} exit {}",
                            secs(ms),
                            e.data["exitCode"]
                                .as_i64()
                                .map(|c| c.to_string())
                                .unwrap_or("?".into())
                        ),
                    ),
                    _ => secs(ms),
                };
                let _ = writeln!(stderr, "    {} {name:<24} {detail}", style.status(status));
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
                        let lines: Vec<&str> = log.lines().collect();
                        for line in &lines[lines.len().saturating_sub(40)..] {
                            eprintln!("    {} {line}", style.dim("│"));
                        }
                    }
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

pub async fn logs(client: &Client, run: &str, task: &str) -> Result<i32> {
    let text = client
        .get_text(&format!("/api/runs/{run}/tasks/{task}/log"))
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
