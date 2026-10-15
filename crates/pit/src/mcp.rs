//! `pit mcp`: a Model Context Protocol server over stdio, so coding agents can
//! run CI on their uncommitted changes and read results without pushing.

use std::path::PathBuf;

use anyhow::{Context, Result};
use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

use crate::client::Client;
use crate::model::*;

pub async fn serve(client: Client) -> Result<()> {
    let stdin = BufReader::new(tokio::io::stdin());
    let mut lines = stdin.lines();
    let mut stdout = tokio::io::stdout();
    while let Some(line) = lines.next_line().await? {
        if line.trim().is_empty() {
            continue;
        }
        let Ok(msg) = serde_json::from_str::<Value>(&line) else {
            write(&mut stdout, json!({ "jsonrpc": "2.0", "id": null, "error": { "code": -32700, "message": "parse error" } })).await?;
            continue;
        };
        let Some(id) = msg.get("id").cloned() else {
            continue;
        }; // Notifications need no reply.
        let method = msg["method"].as_str().unwrap_or_default();
        let reply = match handle(&client, method, &msg["params"]).await {
            Ok(result) => json!({ "jsonrpc": "2.0", "id": id, "result": result }),
            Err(e) => {
                json!({ "jsonrpc": "2.0", "id": id, "error": { "code": -32603, "message": format!("{e:#}") } })
            }
        };
        write(&mut stdout, reply).await?;
    }
    Ok(())
}

async fn write(out: &mut tokio::io::Stdout, v: Value) -> Result<()> {
    let mut s = serde_json::to_string(&v)?;
    s.push('\n');
    out.write_all(s.as_bytes()).await?;
    out.flush().await?;
    Ok(())
}

fn tools() -> Value {
    json!([
        {
            "name": "pit_run",
            "description": "Run CI tasks on the current working tree, including uncommitted and untracked changes, on a warm Pit worker. Unchanged tasks come from the cache. Returns a summary with logs of failed tasks. Use after editing code instead of pushing and waiting for CI.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "path": { "type": "string", "description": "Repository directory. Defaults to the server's working directory." },
                    "tasks": { "type": "array", "items": { "type": "string" }, "description": "Task names (`task` or `group.task`). Default: all tasks." },
                    "noCache": { "type": "boolean", "description": "Ignore cached results." }
                }
            }
        },
        {
            "name": "pit_runs",
            "description": "List recent Pit runs with their status.",
            "inputSchema": { "type": "object", "properties": { "limit": { "type": "number" } } }
        },
        {
            "name": "pit_logs",
            "description": "Get the full log of one task in a run.",
            "inputSchema": {
                "type": "object",
                "properties": { "runId": { "type": "string" }, "task": { "type": "string", "description": "`task` or `group.task`" } },
                "required": ["runId", "task"]
            }
        },
        {
            "name": "pit_queue_add",
            "description": "Add a branch to the Pit merge queue. It is tested against the latest target and merged automatically when it passes.",
            "inputSchema": {
                "type": "object",
                "properties": { "path": { "type": "string" }, "branch": { "type": "string" }, "target": { "type": "string" } },
                "required": ["branch"]
            }
        },
        {
            "name": "pit_queue",
            "description": "Show the merge queue: lanes, statuses, affected tasks.",
            "inputSchema": { "type": "object", "properties": {} }
        }
    ])
}

async fn handle(client: &Client, method: &str, params: &Value) -> Result<Value> {
    match method {
        "initialize" => Ok(json!({
            "protocolVersion": params["protocolVersion"].as_str().unwrap_or("2025-06-18"),
            "capabilities": { "tools": {} },
            "serverInfo": { "name": "pit", "version": env!("CARGO_PKG_VERSION") },
            "instructions": "Pit runs CI on uncommitted changes in seconds. Call pit_run after making changes to check lint, types, tests and builds."
        })),
        "ping" => Ok(json!({})),
        "tools/list" => Ok(json!({ "tools": tools() })),
        "tools/call" => {
            let name = params["name"].as_str().unwrap_or_default();
            let args = &params["arguments"];
            match call(client, name, args).await {
                Ok((text, is_error)) => Ok(
                    json!({ "content": [{ "type": "text", "text": text }], "isError": is_error }),
                ),
                Err(e) => Ok(
                    json!({ "content": [{ "type": "text", "text": format!("{e:#}") }], "isError": true }),
                ),
            }
        }
        other => anyhow::bail!("method not found: {other}"),
    }
}

fn path_arg(args: &Value) -> PathBuf {
    args["path"]
        .as_str()
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."))
}

async fn call(client: &Client, name: &str, args: &Value) -> Result<(String, bool)> {
    match name {
        "pit_run" => {
            let tasks: Vec<String> = args["tasks"]
                .as_array()
                .map(|a| {
                    a.iter()
                        .filter_map(|v| v.as_str().map(String::from))
                        .collect()
                })
                .unwrap_or_default();
            let no_cache = args["noCache"].as_bool().unwrap_or(false);
            let started = std::time::Instant::now();
            let sub = crate::cli::submit(client, &path_arg(args), tasks, no_cache).await?;
            let view = crate::cli::wait(client, &sub.run_id, |_| {}).await?;
            let text = summarize(
                client,
                &view,
                started.elapsed().as_millis() as u64,
                &sub.url,
            )
            .await;
            Ok((text, !view.status.is_ok()))
        }
        "pit_runs" => {
            let limit = args["limit"].as_u64().unwrap_or(10);
            let runs: Vec<RunView> = client.get(&format!("/api/runs?limit={limit}")).await?;
            let lines: Vec<String> = runs
                .iter()
                .map(|r| {
                    format!(
                        "{} {} {} {}@{}",
                        r.id,
                        r.status.as_str(),
                        r.trigger,
                        r.repo,
                        &r.sha[..r.sha.len().min(8)]
                    )
                })
                .collect();
            Ok((lines.join("\n"), false))
        }
        "pit_logs" => {
            let run = args["runId"].as_str().context("runId is required")?;
            let task = args["task"].as_str().context("task is required")?;
            Ok((
                client
                    .get_text(&format!(
                        "/api/runs/{}/tasks/{}/log",
                        crate::client::seg(run)?,
                        crate::client::seg(task)?
                    ))
                    .await?,
                false,
            ))
        }
        "pit_queue_add" => {
            let branch = args["branch"].as_str().context("branch is required")?;
            let id = crate::snapshot::identity(&path_arg(args)).await?;
            let e: QueueEntryView = client
                .post(
                    "/api/queue",
                    &QueueAdd {
                        repo: id.name,
                        url: Some(id.url),
                        branch: Some(branch.into()),
                        pr: None,
                        target: args["target"].as_str().map(String::from),
                    },
                )
                .await?;
            Ok((
                format!(
                    "{} {} -> {}: {} (affected: {})",
                    e.id,
                    e.source,
                    e.target,
                    e.status,
                    e.affected.join(", ")
                ),
                false,
            ))
        }
        "pit_queue" => {
            let entries: Vec<QueueEntryView> = client.get("/api/queue").await?;
            let lines: Vec<String> = entries
                .iter()
                .map(|e| {
                    format!(
                        "{} {} {} -> {} lane={} affected=[{}] {}",
                        e.id,
                        e.status,
                        e.source,
                        e.target,
                        e.lane.clone().unwrap_or_default(),
                        e.affected.join(","),
                        e.reason.clone().unwrap_or_default()
                    )
                })
                .collect();
            Ok((
                if lines.is_empty() {
                    "queue is empty".into()
                } else {
                    lines.join("\n")
                },
                false,
            ))
        }
        other => anyhow::bail!("unknown tool {other}"),
    }
}

async fn summarize(client: &Client, view: &RunView, elapsed_ms: u64, url: &str) -> String {
    let mut s = format!(
        "Run {} {} in {:.1}s ({}@{}{}). {url}\n",
        view.id,
        view.status.as_str(),
        elapsed_ms as f64 / 1000.0,
        view.repo,
        &view.sha[..view.sha.len().min(10)],
        if view.overlay_files > 0 {
            format!(" +{} changed files", view.overlay_files)
        } else {
            String::new()
        }
    );
    if let Some(m) = &view.message {
        s.push_str(&format!("{m}\n"));
    }
    let mut failed = Vec::new();
    for g in &view.groups {
        s.push_str(&format!("{} ({}):\n", g.name, g.os));
        for t in &g.tasks {
            let time = t
                .duration_ms
                .map(|d| format!(" {:.1}s", d as f64 / 1000.0))
                .unwrap_or_default();
            s.push_str(&format!("  {} {}{time}\n", t.status.as_str(), t.name));
            if matches!(t.status, Status::Failed | Status::Error) {
                failed.push((
                    format!("{}.{}", g.name, t.name),
                    t.id.clone(),
                    t.message.clone(),
                ));
            }
        }
    }
    for (name, id, message) in failed {
        s.push_str(&format!(
            "\n--- {name}: {} ---\n",
            message.unwrap_or_default()
        ));
        if let Ok(log) = client.get_text(&format!("/api/tasks/{id}/log")).await {
            let lines: Vec<&str> = log.lines().collect();
            s.push_str(&lines[lines.len().saturating_sub(80)..].join("\n"));
            s.push('\n');
        }
    }
    s
}
