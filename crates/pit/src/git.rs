//! Thin wrappers around the git CLI. Git is on every CI host already, and its
//! CLI is the most battle-tested implementation of checkout and merge.

use std::path::Path;
use std::process::Stdio;

use anyhow::{bail, Context, Result};
use tokio::process::Command;

pub async fn git(dir: &Path, args: &[&str]) -> Result<String> {
    let out = git_raw(dir, args).await?;
    if !out.status.success() {
        bail!(
            "git {} failed in {}: {}",
            args.join(" "),
            dir.display(),
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

pub async fn git_bytes(dir: &Path, args: &[&str]) -> Result<Vec<u8>> {
    let out = git_raw(dir, args).await?;
    if !out.status.success() {
        bail!(
            "git {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(out.stdout)
}

pub async fn git_raw(dir: &Path, args: &[&str]) -> Result<std::process::Output> {
    Command::new("git")
        .args(args)
        .current_dir(dir)
        .env("GIT_TERMINAL_PROMPT", "0")
        .stdin(Stdio::null())
        .output()
        .await
        .with_context(|| format!("running git {}", args.join(" ")))
}

pub async fn git_ok(dir: &Path, args: &[&str]) -> bool {
    git_raw(dir, args)
        .await
        .map(|o| o.status.success())
        .unwrap_or(false)
}

pub async fn has_commit(repo: &Path, sha: &str) -> bool {
    git_ok(repo, &["cat-file", "-e", &format!("{sha}^{{commit}}")]).await
}

/// Refspecs a Pit mirror tracks: branches, Pit queue candidates, GitHub PRs.
pub const FETCH_REFSPECS: &[&str] = &[
    "+refs/heads/*:refs/heads/*",
    "+refs/pit/*:refs/pit/*",
    "+refs/pull/*/head:refs/pull/*/head",
];

/// Creates or updates a bare mirror of `url` at `dir`. It is a plain bare clone
/// with explicit refspecs, never `--mirror`, so pushes from it are explicit.
pub async fn ensure_mirror(dir: &Path, url: &str) -> Result<()> {
    if !dir.join("HEAD").exists() {
        std::fs::create_dir_all(dir)?;
        git(dir, &["init", "--bare", "-q"]).await?;
        git(dir, &["remote", "add", "origin", url]).await?;
        for spec in FETCH_REFSPECS {
            git(dir, &["config", "--add", "remote.origin.fetch", spec]).await?;
        }
        // Large CI fleets fetch a lot; keep objects loose-ish and skip gc.
        git(dir, &["config", "gc.auto", "0"]).await?;
    } else {
        let current = git(dir, &["remote", "get-url", "origin"]).await?;
        if current.trim() != url {
            git(dir, &["remote", "set-url", "origin", url]).await?;
        }
    }
    Ok(())
}

/// Fetches from origin, retrying a failed fetch after 0.5 s, 1 s and 2 s, so
/// a network blip does not fail a run.
pub async fn fetch(dir: &Path) -> Result<()> {
    retry(|| async {
        git(dir, &["fetch", "-q", "--prune", "--no-tags", "origin"])
            .await
            .map(|_| ())
    })
    .await
}

pub async fn retry<F, Fut, T>(mut attempt: F) -> Result<T>
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = Result<T>>,
{
    let delays = [500u64, 1000, 2000];
    for delay in delays {
        match attempt().await {
            Ok(v) => return Ok(v),
            Err(e) => {
                tracing::warn!("{e:#}; retrying in {delay}ms");
                tokio::time::sleep(std::time::Duration::from_millis(delay)).await;
            }
        }
    }
    attempt().await
}

/// Makes sure the mirror has `sha`, fetching only when it does not.
pub async fn ensure_commit(dir: &Path, url: &str, sha: &str) -> Result<()> {
    ensure_mirror(dir, url).await?;
    if has_commit(dir, sha).await {
        return Ok(());
    }
    fetch(dir).await?;
    if !has_commit(dir, sha).await {
        // Not reachable from any ref we track; ask for it directly.
        let _ = git(dir, &["fetch", "-q", "--no-tags", "origin", sha]).await;
    }
    if !has_commit(dir, sha).await {
        bail!("commit {sha} not found in {url}");
    }
    Ok(())
}

pub async fn rev_parse(dir: &Path, rev: &str) -> Result<Option<String>> {
    let out = git_raw(
        dir,
        &["rev-parse", "--verify", "-q", &format!("{rev}^{{commit}}")],
    )
    .await?;
    if out.status.success() {
        Ok(Some(
            String::from_utf8_lossy(&out.stdout).trim().to_string(),
        ))
    } else {
        Ok(None)
    }
}

pub async fn show_file(dir: &Path, sha: &str, path: &str) -> Result<Option<Vec<u8>>> {
    let out = git_raw(dir, &["show", &format!("{sha}:{path}")]).await?;
    Ok(out.status.success().then_some(out.stdout))
}

pub async fn changed_files(dir: &Path, from: &str, to: &str) -> Result<Vec<String>> {
    let out = git(dir, &["diff", "--name-only", "-z", from, to]).await?;
    Ok(out
        .split('\0')
        .filter(|s| !s.is_empty())
        .map(String::from)
        .collect())
}

pub async fn merge_base(dir: &Path, a: &str, b: &str) -> Result<Option<String>> {
    let out = git_raw(dir, &["merge-base", a, b]).await?;
    Ok(out
        .status
        .success()
        .then(|| String::from_utf8_lossy(&out.stdout).trim().to_string()))
}

pub async fn is_ancestor(dir: &Path, ancestor: &str, descendant: &str) -> bool {
    git_ok(dir, &["merge-base", "--is-ancestor", ancestor, descendant]).await
}

/// Merges `theirs` into `ours` without a worktree and returns the merge commit,
/// or `None` on conflict. Needs git 2.38+ for `merge-tree --write-tree`.
pub async fn merge_commit(
    dir: &Path,
    ours: &str,
    theirs: &str,
    message: &str,
) -> Result<Option<String>> {
    if is_ancestor(dir, theirs, ours).await {
        return Ok(Some(ours.to_string()));
    }
    let out = git_raw(dir, &["merge-tree", "--write-tree", ours, theirs]).await?;
    match out.status.code() {
        Some(0) => {}
        Some(1) => return Ok(None),
        _ => bail!(
            "git merge-tree failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        ),
    }
    let tree = String::from_utf8_lossy(&out.stdout)
        .lines()
        .next()
        .unwrap_or_default()
        .trim()
        .to_string();
    let commit = Command::new("git")
        .args([
            "commit-tree",
            &tree,
            "-p",
            ours,
            "-p",
            theirs,
            "-m",
            message,
        ])
        .current_dir(dir)
        .env("GIT_AUTHOR_NAME", "Pit")
        .env("GIT_AUTHOR_EMAIL", "pit@localhost")
        .env("GIT_COMMITTER_NAME", "Pit")
        .env("GIT_COMMITTER_EMAIL", "pit@localhost")
        .stdin(Stdio::null())
        .output()
        .await?;
    if !commit.status.success() {
        bail!(
            "git commit-tree failed: {}",
            String::from_utf8_lossy(&commit.stderr).trim()
        );
    }
    Ok(Some(
        String::from_utf8_lossy(&commit.stdout).trim().to_string(),
    ))
}

/// Pushes `sha` to `refname` on origin. Non-forced, so a moved branch fails.
pub async fn push(dir: &Path, sha: &str, refname: &str, force: bool) -> Result<bool> {
    let spec = format!("{}{sha}:{refname}", if force { "+" } else { "" });
    let out = git_raw(dir, &["push", "-q", "origin", &spec]).await?;
    if !out.status.success() {
        tracing::warn!(
            "push {spec} rejected: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(out.status.success())
}

/// One entry from `git ls-files -s`.
#[derive(Debug, Clone)]
pub struct IndexEntry {
    pub path: String,
    pub oid: String,
    pub executable: bool,
}

pub async fn ls_files(dir: &Path) -> Result<Vec<IndexEntry>> {
    let out = git_bytes(dir, &["ls-files", "-s", "-z"]).await?;
    let mut entries = Vec::new();
    for rec in out.split(|b| *b == 0) {
        if rec.is_empty() {
            continue;
        }
        let rec = String::from_utf8_lossy(rec);
        // "<mode> <oid> <stage>\t<path>"
        let Some((meta, path)) = rec.split_once('\t') else {
            continue;
        };
        let mut parts = meta.split(' ');
        let mode = parts.next().unwrap_or_default();
        let oid = parts.next().unwrap_or_default();
        entries.push(IndexEntry {
            path: path.to_string(),
            oid: oid.to_string(),
            executable: mode == "100755",
        });
    }
    Ok(entries)
}

pub async fn toplevel(dir: &Path) -> Result<std::path::PathBuf> {
    let out = git(dir, &["rev-parse", "--show-toplevel"]).await?;
    Ok(std::path::PathBuf::from(out.trim()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test(start_paused = true)]
    async fn retry_survives_a_blip_and_gives_up_eventually() {
        let mut calls = 0;
        let out = retry(|| {
            calls += 1;
            let n = calls;
            async move {
                if n < 3 {
                    anyhow::bail!("blip {n}")
                } else {
                    Ok(n)
                }
            }
        })
        .await;
        assert_eq!(out.unwrap(), 3);
        let mut calls = 0;
        let out: Result<()> = retry(|| {
            calls += 1;
            async { anyhow::bail!("down") }
        })
        .await;
        assert!(out.is_err());
        assert_eq!(calls, 4);
    }
}
