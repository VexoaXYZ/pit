//! `pit run --debug`: when a task fails, the worker keeps its workspace and
//! writes a script that reopens the task's environment there.
//!
//! Debugging a CI failure usually means guessing from the log, or adding an
//! SSH step (action-tmate) and pushing again to get a shell on the runner
//! before it times out. The script is that shell, without the round trip:
//! the same directory, backend (the task's container), variables and
//! network setting. With arguments it runs them instead of a shell, so a
//! coding agent can use it too.

use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::Result;

use super::exec;
use crate::model::{Backend, TaskDef};

/// How long a failed workspace is kept, unless its scripts are deleted first.
pub const HOLD: Duration = Duration::from_secs(30 * 60);

/// Stands in for the task's command while the debug command line is built.
const PLACEHOLDER: &str = "__pit_debug_command__";

/// Where a group's debug scripts go: outside the workspace, so tasks and
/// cache keys never see them.
pub fn dir(data_dir: &Path, group_id: &str) -> PathBuf {
    data_dir.join("debug").join(group_id)
}

fn quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', r"'\''"))
}

fn comment(text: &str) -> String {
    text.lines()
        .map(|l| format!("#   {l}\n"))
        .collect::<String>()
}

/// Writes `<dir>/<task>.sh` and returns its path. Secret values are left
/// out: the script names the secrets the task had instead.
#[allow(clippy::too_many_arguments)]
pub fn write_script(
    dir: &Path,
    task: &str,
    def: &TaskDef,
    backend: Backend,
    ws: &Path,
    env: &[(String, String)],
    policy: &exec::EnvPolicy,
    failure: Option<&str>,
) -> Result<PathBuf> {
    let secret = |k: &str| def.secrets.iter().any(|s| s == k) || k == "PIT_OIDC_TOKEN";
    let env: Vec<(String, String)> = env.iter().filter(|(k, _)| !secret(k)).cloned().collect();
    let withheld: Vec<&str> = def.secrets.iter().map(String::as_str).collect();
    let mut probe = def.clone();
    probe.run = PLACEHOLDER.into();
    let built = exec::command_with(&probe, backend, ws, &env, policy);
    let std = built.cmd.as_std();
    let mut words = vec![quote(&std.get_program().to_string_lossy())];
    let mut args = std.get_args().map(|a| a.to_string_lossy().to_string());
    while let Some(a) = args.next() {
        match a.as_str() {
            PLACEHOLDER => words.push("\"$PIT_DEBUG_CMD\"".into()),
            // A fixed name would clash with the task's own container.
            "--name" if backend == Backend::Docker => {
                args.next();
            }
            "run" if backend == Backend::Docker && words.len() == 1 => {
                words.push("run".into());
                words.push("-i".into());
                words.push("$tty".into());
            }
            _ => words.push(quote(&a)),
        }
    }
    let mut s = String::from("#!/bin/sh\n");
    // Names and messages go into comments: a newline in them must not
    // start a line the shell runs.
    let one_line = |t: &str| t.replace(['\n', '\r'], " ");
    s.push_str(&format!(
        "# Pit debug shell for task {}, in the workspace where it failed.\n",
        one_line(task)
    ));
    if let Some(f) = failure {
        s.push_str("# It failed with:\n");
        s.push_str(&comment(f));
    }
    s.push_str("# Its command:\n");
    s.push_str(&comment(&def.run));
    s.push_str(
        "# No arguments: an interactive shell there, as the task ran (backend,\n\
         # container, variables). With arguments: runs them there instead.\n",
    );
    if !withheld.is_empty() {
        s.push_str(&format!(
            "# Not included (secrets): {}. Export them first if you need them.\n",
            one_line(&withheld.join(", "))
        ));
    }
    if policy.minimal {
        s.push_str("# The task saw only a minimal environment; this shell sees yours.\n");
    }
    s.push_str(&format!(
        "# The worker keeps this workspace for {} minutes, or until this file is deleted.\n",
        HOLD.as_secs() / 60
    ));
    s.push_str(&format!("cd {} || exit 1\n", quote(&ws.to_string_lossy())));
    for (k, v) in &env {
        s.push_str(&format!("export {k}={}\n", quote(v)));
    }
    s.push_str("if [ $# -eq 0 ]; then PIT_DEBUG_CMD='exec sh -i'; else PIT_DEBUG_CMD=\"$*\"; fi\n");
    if backend == Backend::Docker {
        s.push_str("tty=; [ -t 0 ] && tty=-t\n");
    }
    s.push_str(&format!("exec {}\n", words.join(" ")));
    // The worker's user only, from the start: the script holds the task's
    // variables, `env` values from the worker included.
    let mut dirs = std::fs::DirBuilder::new();
    dirs.recursive(true);
    let mut file = std::fs::OpenOptions::new();
    file.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
        dirs.mode(0o700);
        file.mode(0o700);
    }
    dirs.create(dir)?;
    let path = dir.join(format!("{}.sh", safe_file(task)));
    let _ = std::fs::remove_file(&path);
    std::io::Write::write_all(&mut file.open(&path)?, s.as_bytes())?;
    Ok(path)
}

fn safe_file(name: &str) -> String {
    name.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect()
}

/// Whether any task of the group left a script.
pub fn has_scripts(dir: &Path) -> bool {
    scripts(dir) > 0
}

fn scripts(dir: &Path) -> usize {
    std::fs::read_dir(dir)
        .map(|d| {
            d.flatten()
                .filter(|e| e.path().extension().is_some_and(|x| x == "sh"))
                .count()
        })
        .unwrap_or(0)
}

/// Waits until every script is deleted or the hold runs out, then removes
/// the rest.
pub async fn hold(dir: &Path, limit: Duration) {
    let until = tokio::time::Instant::now() + limit;
    while tokio::time::Instant::now() < until && scripts(dir) > 0 {
        tokio::time::sleep(Duration::from_secs(2)).await;
    }
    let _ = std::fs::remove_dir_all(dir);
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    fn def(run: &str) -> TaskDef {
        TaskDef {
            run: run.into(),
            ..Default::default()
        }
    }

    #[test]
    fn the_script_runs_commands_where_the_task_ran() {
        let tmp = tempfile::tempdir().unwrap();
        let ws = tmp.path().join("ws");
        std::fs::create_dir_all(&ws).unwrap();
        std::fs::write(ws.join("left-behind.txt"), "state").unwrap();
        let mut d = def("exit 1");
        d.secrets = vec!["API_KEY".into()];
        let env = vec![
            ("PIT_TASK".to_string(), "test".to_string()),
            ("API_KEY".to_string(), "hunter2".to_string()),
            ("QUOTED".to_string(), "it's".to_string()),
        ];
        let path = write_script(
            &tmp.path().join("debug"),
            "unit:test",
            &d,
            Backend::Host,
            &ws,
            &env,
            &Default::default(),
            Some("exited with 1"),
        )
        .unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(!text.contains("hunter2"), "{text}");
        assert!(text.contains("Not included (secrets): API_KEY"), "{text}");
        let out = std::process::Command::new("sh")
            .arg(&path)
            .arg("cat left-behind.txt; echo \" $PIT_TASK $QUOTED\"")
            .env_remove("API_KEY")
            .output()
            .unwrap();
        assert_eq!(String::from_utf8_lossy(&out.stdout), "state test it's\n");
    }

    #[tokio::test]
    async fn the_hold_ends_when_the_scripts_are_deleted() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("g");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("t.sh"), "").unwrap();
        assert!(has_scripts(&dir));
        let d = dir.clone();
        let held = tokio::spawn(async move { hold(&d, Duration::from_secs(60)).await });
        tokio::time::sleep(Duration::from_millis(300)).await;
        assert!(!held.is_finished());
        std::fs::remove_file(dir.join("t.sh")).unwrap();
        tokio::time::timeout(Duration::from_secs(10), held)
            .await
            .expect("released after the script was deleted")
            .unwrap();
        assert!(!dir.exists());
    }
}
