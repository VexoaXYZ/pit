//! Execution backends and process supervision with live log streaming.

use std::path::Path;
use std::process::Stdio;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use tokio::io::{AsyncBufReadExt, AsyncRead, BufReader};
use tokio::process::Command;
use tokio::sync::mpsc;

use crate::model::{Backend, TaskDef};

/// Logs above this size keep the head and tail only.
const MAX_LOG: usize = 8 * 1024 * 1024;

pub struct Outcome {
    pub exit_code: Option<i32>,
    pub log: String,
    pub duration: Duration,
    pub timed_out: bool,
    /// Stopped because the run was cancelled or superseded.
    pub cancelled: bool,
}

/// Which isolation backends work on this host.
pub async fn detect_backends() -> Vec<Backend> {
    let mut backends = vec![Backend::Host];
    if cfg!(target_os = "linux")
        && probe(
            "unshare",
            &[
                "--user",
                "--map-root-user",
                "--pid",
                "--fork",
                "--mount-proc",
                "true",
            ],
        )
        .await
    {
        backends.push(Backend::Sandbox);
    }
    if probe("docker", &["info", "--format", "{{.ServerVersion}}"]).await {
        backends.push(Backend::Docker);
    }
    backends
}

async fn probe(bin: &str, args: &[&str]) -> bool {
    let fut = Command::new(bin)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
    matches!(tokio::time::timeout(Duration::from_secs(10), fut).await, Ok(Ok(s)) if s.success())
}

pub fn kvm_available() -> bool {
    Path::new("/dev/kvm").exists()
}

fn shell(script: &str) -> (String, Vec<String>) {
    if cfg!(windows) {
        ("cmd".into(), vec!["/C".into(), script.into()])
    } else {
        ("sh".into(), vec!["-c".into(), script.into()])
    }
}

/// A task's process, plus the container to remove if it has to be killed.
pub struct TaskCommand {
    pub cmd: Command,
    pub container: Option<String>,
}

/// Builds the command for a task on the chosen backend.
pub fn command(
    task: &TaskDef,
    backend: Backend,
    workdir: &Path,
    env: &[(String, String)],
) -> TaskCommand {
    let network = task.network.unwrap_or(true);
    let mut container = None;
    let (program, args): (String, Vec<String>) = match backend {
        Backend::Host => shell(&task.run),
        Backend::Sandbox => {
            let mut a: Vec<String> = [
                "--user",
                "--map-root-user",
                "--pid",
                "--fork",
                "--mount-proc",
                "--ipc",
                "--uts",
            ]
            .iter()
            .map(|s| s.to_string())
            .collect();
            if !network {
                a.push("--net".into());
            }
            a.extend(["sh".into(), "-c".into(), task.run.clone()]);
            ("unshare".into(), a)
        }
        Backend::Docker => {
            let name = crate::model::new_id("pit");
            let mut a = vec![
                "run".to_string(),
                "--rm".into(),
                "--init".into(),
                "--name".into(),
                name.clone(),
                "-v".into(),
                format!("{}:/workspace", workdir.display()),
                "-w".into(),
                "/workspace".into(),
            ];
            if !network {
                a.extend(["--network".into(), "none".into()]);
            }
            // Files written to the warm workspace must stay owned by the worker,
            // or the next checkout in it fails.
            #[cfg(unix)]
            if let Ok(meta) = std::fs::metadata(workdir) {
                use std::os::unix::fs::MetadataExt;
                a.extend(["--user".into(), format!("{}:{}", meta.uid(), meta.gid())]);
                // That user has no home in the image; tools need a writable one.
                a.extend(["-e".into(), "HOME=/tmp".into()]);
            }
            container = Some(name);
            for (k, v) in env {
                a.extend(["-e".into(), format!("{k}={v}")]);
            }
            a.push(task.image.clone().unwrap_or_else(|| "ubuntu:24.04".into()));
            a.extend(["sh".into(), "-c".into(), task.run.clone()]);
            ("docker".into(), a)
        }
    };
    let mut cmd = Command::new(program);
    // Pit's own settings (server URL, API token, GitHub credentials) are not
    // the task's business: a task holding PIT_TOKEN could write to the cache.
    for (key, _) in std::env::vars_os() {
        if key
            .to_string_lossy()
            .to_ascii_uppercase()
            .starts_with("PIT_")
        {
            cmd.env_remove(key);
        }
    }
    cmd.args(args)
        .current_dir(workdir)
        .envs(env.iter().cloned());
    TaskCommand { cmd, container }
}

/// Runs a command, sending log text to `live` as it arrives.
pub async fn run(
    task: TaskCommand,
    timeout: Option<Duration>,
    live: mpsc::UnboundedSender<String>,
    mut cancel: tokio::sync::watch::Receiver<bool>,
) -> Result<Outcome> {
    let TaskCommand { mut cmd, container } = task;
    let started = Instant::now();
    cmd.stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    #[cfg(unix)]
    cmd.process_group(0);
    let mut child = cmd.spawn().context("spawning task")?;
    let pid = child.id();
    let (tx, mut rx) = mpsc::unbounded_channel::<String>();
    let out = child
        .stdout
        .take()
        .map(|s| tokio::spawn(pump(s, tx.clone())));
    let err = child
        .stderr
        .take()
        .map(|s| tokio::spawn(pump(s, tx.clone())));
    drop(tx);

    let collector = tokio::spawn(async move {
        let mut log = String::new();
        let mut dropped = 0usize;
        while let Some(line) = rx.recv().await {
            if log.len() + line.len() <= MAX_LOG {
                log.push_str(&line);
            } else {
                dropped += line.len();
            }
            let _ = live.send(line);
        }
        if dropped > 0 {
            log.push_str(&format!(
                "\n[pit: {dropped} bytes of log output truncated]\n"
            ));
        }
        log
    });

    enum Ended {
        Exited(std::process::ExitStatus),
        TimedOut,
        Cancelled,
    }
    let deadline = async {
        match timeout {
            Some(t) => tokio::time::sleep(t).await,
            None => std::future::pending().await,
        }
    };
    let cancelled = async {
        loop {
            if *cancel.borrow_and_update() {
                return;
            }
            if cancel.changed().await.is_err() {
                std::future::pending::<()>().await;
            }
        }
    };
    let ended = tokio::select! {
        s = child.wait() => Ended::Exited(s?),
        _ = deadline => Ended::TimedOut,
        _ = cancelled => Ended::Cancelled,
    };
    let (status, timed_out, was_cancelled) = match ended {
        Ended::Exited(s) => (Some(s), false, false),
        other => {
            kill_tree(pid);
            let _ = child.kill().await;
            // Killing the docker client leaves the container running.
            if let Some(name) = &container {
                let _ = Command::new("docker")
                    .args(["rm", "-f", name])
                    .stdin(Stdio::null())
                    .stdout(Stdio::null())
                    .stderr(Stdio::null())
                    .status()
                    .await;
            }
            (
                None,
                matches!(other, Ended::TimedOut),
                matches!(other, Ended::Cancelled),
            )
        }
    };
    // A backgrounded grandchild can hold the pipes open; do not wait forever.
    for mut h in [out, err].into_iter().flatten() {
        if tokio::time::timeout(Duration::from_secs(2), &mut h)
            .await
            .is_err()
        {
            h.abort();
        }
    }
    kill_tree(pid);
    let mut log = collector.await.unwrap_or_default();
    if timed_out {
        log.push_str("\n[pit: task timed out]\n");
    }
    if was_cancelled {
        log.push_str("\n[pit: run cancelled; task stopped]\n");
    }
    Ok(Outcome {
        exit_code: status.and_then(|s| s.code()),
        log,
        duration: started.elapsed(),
        timed_out,
        cancelled: was_cancelled,
    })
}

async fn pump(stream: impl AsyncRead + Unpin, tx: mpsc::UnboundedSender<String>) {
    let mut reader = BufReader::new(stream);
    let mut buf = Vec::new();
    loop {
        buf.clear();
        match reader.read_until(b'\n', &mut buf).await {
            Ok(0) | Err(_) => break,
            Ok(_) => {
                if tx.send(String::from_utf8_lossy(&buf).into_owned()).is_err() {
                    break;
                }
            }
        }
    }
}

/// Kills the task's whole process group so nothing outlives it.
fn kill_tree(pid: Option<u32>) {
    #[cfg(unix)]
    if let Some(pid) = pid {
        // SAFETY: plain syscall; a negative pid targets the process group we created.
        unsafe {
            libc_kill(-(pid as i32), 9);
        }
    }
    #[cfg(not(unix))]
    let _ = pid;
}

#[cfg(unix)]
extern "C" {
    #[link_name = "kill"]
    fn libc_kill(pid: i32, sig: i32) -> i32;
}

#[cfg(test)]
mod tests {
    use super::*;

    fn never() -> tokio::sync::watch::Receiver<bool> {
        let (tx, rx) = tokio::sync::watch::channel(false);
        std::mem::forget(tx);
        rx
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn cancellation_stops_the_task_and_its_children() {
        let dir = tempfile::tempdir().unwrap();
        let task = TaskDef {
            run: "sleep 30 & sleep 30".into(),
            ..Default::default()
        };
        let (tx, _rx) = mpsc::unbounded_channel();
        let (cancel_tx, cancel_rx) = tokio::sync::watch::channel(false);
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(300)).await;
            let _ = cancel_tx.send(true);
        });
        let started = Instant::now();
        let out = run(
            command(&task, Backend::Host, dir.path(), &[]),
            None,
            tx,
            cancel_rx,
        )
        .await
        .unwrap();
        assert!(out.cancelled && !out.timed_out);
        assert!(started.elapsed() < Duration::from_secs(5));
    }

    #[tokio::test]
    async fn streams_output_and_reports_exit_codes() {
        let dir = tempfile::tempdir().unwrap();
        let task = TaskDef {
            run: if cfg!(windows) {
                "echo hi && exit 3".into()
            } else {
                "echo hi; echo err >&2; exit 3".into()
            },
            ..Default::default()
        };
        let (tx, mut rx) = mpsc::unbounded_channel();
        let out = run(
            command(&task, Backend::Host, dir.path(), &[]),
            None,
            tx,
            never(),
        )
        .await
        .unwrap();
        assert_eq!(out.exit_code, Some(3));
        assert!(out.log.contains("hi"));
        let mut live = String::new();
        while let Ok(l) = rx.try_recv() {
            live.push_str(&l);
        }
        assert!(live.contains("hi"));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn times_out_and_kills_children() {
        let dir = tempfile::tempdir().unwrap();
        let task = TaskDef {
            run: "sleep 30 & sleep 30".into(),
            ..Default::default()
        };
        let (tx, _rx) = mpsc::unbounded_channel();
        let started = Instant::now();
        let out = run(
            command(&task, Backend::Host, dir.path(), &[]),
            Some(Duration::from_millis(300)),
            tx,
            never(),
        )
        .await
        .unwrap();
        assert!(out.timed_out);
        assert!(started.elapsed() < Duration::from_secs(5));
    }
}
