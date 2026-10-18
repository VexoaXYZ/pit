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
    /// Stopped for printing nothing for the idle limit (`timed_out` is set too).
    pub idle: bool,
    /// Stopped because the run was cancelled or superseded.
    pub cancelled: bool,
    /// The signal that ended it, if one did (Unix).
    pub signal: Option<i32>,
    /// The kernel's OOM killer ran while it did; `None` where that cannot be
    /// known.
    pub oom: Option<bool>,
    /// The most memory its process group was seen to use, in bytes, sampled
    /// (Linux; a docker task's container is not in the group).
    pub peak_memory: u64,
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
/// The image a `docker` task without one runs in.
pub const DEFAULT_IMAGE: &str = "ubuntu:24.04";

/// Which of the worker's own environment a task inherits.
#[derive(Clone, Default)]
pub struct EnvPolicy {
    /// Names never inherited (the worker's secrets file).
    pub hide: Vec<String>,
    /// Inherit only `MINIMAL_ENV`, for code nobody trusts (fork PRs).
    pub minimal: bool,
    /// The network a `docker` task joins: its group's services'.
    pub docker_network: Option<String>,
    /// A worker-owned directory a `docker` task sees at `SUMMARY_MOUNT`
    /// (`$PIT_SUMMARY` lives there, outside the workspace).
    pub summary_dir: Option<std::path::PathBuf>,
    /// A worker-owned directory a `docker` task gets as `$HOME`
    /// (`HOME_MOUNT`), so tool caches (`~/.npm`, `~/.cache/pip`, `~/.cargo`)
    /// survive between runs. `None`: a throwaway `/tmp`.
    pub docker_home: Option<std::path::PathBuf>,
}

/// Where a container sees `EnvPolicy::docker_home`.
pub const HOME_MOUNT: &str = "/pit-home";

/// Where a container sees `EnvPolicy::summary_dir`.
pub const SUMMARY_MOUNT: &str = "/pit-summary";

/// What a task gets under `EnvPolicy::minimal`: enough to find tools and a
/// temp dir, nothing an operator would put a credential in.
const MINIMAL_ENV: &[&str] = &[
    "PATH",
    "HOME",
    "USER",
    "LOGNAME",
    "SHELL",
    "LANG",
    "LC_ALL",
    "LC_CTYPE",
    "TERM",
    "TZ",
    "TMPDIR",
    "TMP",
    "TEMP",
    "SYSTEMROOT",
    "SYSTEMDRIVE",
    "WINDIR",
    "COMSPEC",
    "PATHEXT",
    "USERPROFILE",
    "APPDATA",
    "LOCALAPPDATA",
    "PROGRAMDATA",
    "PROGRAMFILES",
    "NUMBER_OF_PROCESSORS",
    "PROCESSOR_ARCHITECTURE",
    "OS",
    // Where the docker client finds its daemon (rootless, remote, contexts).
    // Only the client sees these; the container gets none of them.
    "DOCKER_HOST",
    "DOCKER_CONTEXT",
    "DOCKER_CONFIG",
    "DOCKER_CERT_PATH",
    "DOCKER_TLS_VERIFY",
    "XDG_RUNTIME_DIR",
];

pub fn command(
    task: &TaskDef,
    backend: Backend,
    workdir: &Path,
    env: &[(String, String)],
) -> TaskCommand {
    command_with(task, backend, workdir, env, &EnvPolicy::default())
}

pub fn command_with(
    task: &TaskDef,
    backend: Backend,
    workdir: &Path,
    env: &[(String, String)],
    policy: &EnvPolicy,
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
            if let Some(d) = &policy.summary_dir {
                a.extend(["-v".into(), format!("{}:{SUMMARY_MOUNT}", d.display())]);
            }
            if !network {
                a.extend(["--network".into(), "none".into()]);
            } else if let Some(n) = &policy.docker_network {
                a.extend(["--network".into(), n.clone()]);
            }
            if let Some(m) = &task.memory {
                a.extend([
                    "--memory".into(),
                    m.clone(),
                    "--memory-swap".into(),
                    m.clone(),
                ]);
            }
            // Files written to the warm workspace must stay owned by the worker,
            // or the next checkout in it fails.
            #[cfg(unix)]
            if let Ok(meta) = std::fs::metadata(workdir) {
                use std::os::unix::fs::MetadataExt;
                a.extend(["--user".into(), format!("{}:{}", meta.uid(), meta.gid())]);
                // That user has no home in the image; tools need a writable one.
                match &policy.docker_home {
                    Some(home) => a.extend([
                        "-v".into(),
                        format!("{}:{HOME_MOUNT}", home.display()),
                        "-e".into(),
                        format!("HOME={HOME_MOUNT}"),
                    ]),
                    None => a.extend(["-e".into(), "HOME=/tmp".into()]),
                }
            }
            container = Some(name);
            // By name only: the docker client passes values from its own
            // environment, so they never show up in `ps` on the worker.
            for (k, _) in env {
                a.extend(["-e".into(), k.clone()]);
            }
            a.push(task.image.clone().unwrap_or_else(|| DEFAULT_IMAGE.into()));
            a.extend(["sh".into(), "-c".into(), task.run.clone()]);
            ("docker".into(), a)
        }
    };
    let mut cmd = Command::new(program);
    if policy.minimal {
        cmd.env_clear();
        for (key, value) in std::env::vars_os() {
            let upper = key.to_string_lossy().to_ascii_uppercase();
            if MINIMAL_ENV.contains(&upper.as_str()) {
                cmd.env(key, value);
            }
        }
    }
    // Pit's own settings (server URL, API token, GitHub credentials) are not
    // the task's business: a task holding PIT_TOKEN could write to the cache.
    // Nor are secrets the task did not ask for.
    for (key, _) in std::env::vars_os() {
        let name = key.to_string_lossy();
        if name.to_ascii_uppercase().starts_with("PIT_")
            || policy.hide.iter().any(|h| h.eq_ignore_ascii_case(&name))
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
/// When to give up on a task.
#[derive(Clone, Copy, Default)]
pub struct Limits {
    /// Total run time.
    pub timeout: Option<Duration>,
    /// Time without any output.
    pub idle: Option<Duration>,
}

pub async fn run(
    task: TaskCommand,
    limits: Limits,
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
    let oom_before = super::memory::oom_kills();
    let mut child = cmd.spawn().context("spawning task")?;
    let pid = child.id();
    let (peak, sampler) = super::memory::sample(pid);
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

    // Milliseconds since start of the last output, for the idle limit.
    let last_output = std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0));
    let seen = last_output.clone();
    let collector = tokio::spawn(async move {
        let mut log = String::new();
        let mut dropped = 0usize;
        while let Some(line) = rx.recv().await {
            seen.store(
                started.elapsed().as_millis() as u64,
                std::sync::atomic::Ordering::Relaxed,
            );
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
        Idle,
        Cancelled,
    }
    let deadline = async {
        match limits.timeout {
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
    let idle = async {
        let Some(limit) = limits.idle else {
            return std::future::pending().await;
        };
        loop {
            let last =
                Duration::from_millis(last_output.load(std::sync::atomic::Ordering::Relaxed));
            let quiet_until = started + last + limit;
            if Instant::now() >= quiet_until {
                return;
            }
            tokio::time::sleep_until(quiet_until.into()).await;
        }
    };
    let ended = tokio::select! {
        s = child.wait() => Ended::Exited(s?),
        _ = deadline => Ended::TimedOut,
        _ = idle => Ended::Idle,
        _ = cancelled => Ended::Cancelled,
    };
    sampler.abort();
    let oom = match (oom_before, super::memory::oom_kills()) {
        (Some(a), Some(b)) => Some(b > a),
        _ => None,
    };
    let went_idle = matches!(ended, Ended::Idle);
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
                matches!(other, Ended::TimedOut | Ended::Idle),
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
    if went_idle {
        let secs = limits.idle.map(|d| d.as_secs()).unwrap_or_default();
        log.push_str(&format!(
            "\n[pit: no output for {secs}s; task stopped (idleTimeout)]\n"
        ));
    } else if timed_out {
        let secs = limits.timeout.map(|d| d.as_secs()).unwrap_or_default();
        log.push_str(&format!(
            "\n[pit: task ran longer than {secs}s; stopped (timeout)]\n"
        ));
    }
    if was_cancelled {
        log.push_str("\n[pit: run cancelled; task stopped]\n");
    }
    #[cfg(unix)]
    let signal = status.and_then(|s| std::os::unix::process::ExitStatusExt::signal(&s));
    #[cfg(not(unix))]
    let signal = None;
    Ok(Outcome {
        signal,
        oom,
        peak_memory: peak.load(std::sync::atomic::Ordering::Relaxed),
        exit_code: status.and_then(|s| s.code()),
        log,
        duration: started.elapsed(),
        timed_out,
        idle: went_idle,
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
    async fn hidden_and_minimal_environments() {
        let dir = tempfile::tempdir().unwrap();
        std::env::set_var("PIT_TEST_UNRELATED_SECRET_X", "s1");
        std::env::set_var("SOME_TOKEN_FOR_TEST_X", "s2");
        let task = TaskDef {
            run: "echo \"[$SOME_TOKEN_FOR_TEST_X][$GIVEN][${PATH:+path}]\"".into(),
            ..Default::default()
        };
        let given = [("GIVEN".to_string(), "g".to_string())];
        let out = |policy: EnvPolicy| {
            let (tx, _rx) = mpsc::unbounded_channel();
            let cmd = command_with(&task, Backend::Host, dir.path(), &given, &policy);
            async move { run(cmd, Limits::default(), tx, never()).await.unwrap().log }
        };
        assert!(out(EnvPolicy::default()).await.contains("[s2][g][path]"));
        let hidden = EnvPolicy {
            hide: vec!["some_token_for_test_x".into()],
            ..Default::default()
        };
        assert!(out(hidden).await.contains("[][g][path]"));
        let minimal = EnvPolicy {
            minimal: true,
            ..Default::default()
        };
        assert!(out(minimal).await.contains("[][g][path]"));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn a_silent_task_is_stopped_by_the_idle_limit() {
        let dir = tempfile::tempdir().unwrap();
        // Prints, then goes quiet: the idle clock starts at the last line.
        let task = TaskDef {
            run: "echo working; sleep 30".into(),
            ..Default::default()
        };
        let (tx, _rx) = mpsc::unbounded_channel();
        let started = Instant::now();
        let limits = Limits {
            timeout: None,
            idle: Some(Duration::from_millis(500)),
        };
        let out = run(
            command(&task, Backend::Host, dir.path(), &[]),
            limits,
            tx,
            never(),
        )
        .await
        .unwrap();
        assert!(out.idle && out.timed_out);
        assert!(out.log.contains("no output for"));
        assert!(started.elapsed() < Duration::from_secs(5));
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
            Limits::default(),
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
            Limits::default(),
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
            Limits {
                timeout: Some(Duration::from_millis(300)),
                idle: None,
            },
            tx,
            never(),
        )
        .await
        .unwrap();
        assert!(out.timed_out);
        assert!(started.elapsed() < Duration::from_secs(5));
    }
}
