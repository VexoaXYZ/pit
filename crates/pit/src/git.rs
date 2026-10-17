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

/// Reads many files at one commit with a single `git cat-file --batch`,
/// rather than a process per file. Missing paths are left out.
pub async fn read_blobs(
    dir: &Path,
    sha: &str,
    paths: &[String],
) -> Result<std::collections::HashMap<String, Vec<u8>>> {
    use tokio::io::AsyncWriteExt;
    let mut out = std::collections::HashMap::new();
    if paths.is_empty() {
        return Ok(out);
    }
    let mut child = Command::new("git")
        .args(["cat-file", "--batch"])
        .current_dir(dir)
        .env("GIT_TERMINAL_PROMPT", "0")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .context("running git cat-file --batch")?;
    let mut stdin = child.stdin.take().context("cat-file stdin")?;
    // The batch protocol is line based: a path with a newline in it cannot
    // be asked for, and would shift every answer after it onto the wrong
    // path.
    let paths: Vec<&String> = paths.iter().filter(|p| !p.contains('\n')).collect();
    let input: String = paths.iter().map(|p| format!("{sha}:{p}\n")).collect();
    // Write while reading, or a large batch fills the pipes and deadlocks.
    let writer = tokio::spawn(async move {
        let _ = stdin.write_all(input.as_bytes()).await;
    });
    let output = child.wait_with_output().await?;
    let _ = writer.await;
    if !output.status.success() {
        bail!("git cat-file --batch failed ({})", output.status);
    }
    let data = output.stdout;
    let mut pos = 0;
    for path in paths {
        let Some(nl) = data
            .get(pos..)
            .and_then(|rest| rest.iter().position(|b| *b == b'\n'))
        else {
            break;
        };
        let header = String::from_utf8_lossy(&data[pos..pos + nl]).to_string();
        pos += nl + 1;
        // "<oid> <type> <size>", or "<input> missing".
        let mut parts = header.split(' ');
        let (Some(_), Some(_), Some(size)) = (parts.next(), parts.next(), parts.next()) else {
            continue;
        };
        let Ok(size) = size.parse::<usize>() else {
            continue;
        };
        if pos + size > data.len() {
            break;
        }
        out.insert((*path).clone(), data[pos..pos + size].to_vec());
        pos += size + 1;
    }
    Ok(out)
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

/// A submodule URL as git resolves it: `./x` and `../x` are relative to the
/// superproject's own URL.
pub fn resolve_submodule_url(superproject: &str, url: &str) -> String {
    if !(url.starts_with("./") || url.starts_with("../")) {
        return url.to_string();
    }
    let mut base = superproject.trim_end_matches('/').to_string();
    let mut rest = url;
    loop {
        if let Some(r) = rest.strip_prefix("./") {
            rest = r;
        } else if let Some(r) = rest.strip_prefix("../") {
            rest = r;
            match base.rfind(['/', ':']) {
                Some(i) => base.truncate(i),
                None => base.clear(),
            }
        } else {
            break;
        }
    }
    let sep = if base.ends_with(':') { "" } else { "/" };
    format!("{base}{sep}{rest}")
}

/// Whether a submodule URL uses a transport Pit fetches: http(s), ssh, git,
/// scp-style `user@host:path`, or a path. Nothing that git could read as an
/// option (`-…`) or run as a command (`ext::`).
pub fn is_fetchable_url(url: &str) -> bool {
    if url.is_empty() || url.starts_with('-') || url.contains("::") {
        return false;
    }
    // Parsers disagree on where a host ends around these: `?`, `#` and `\`
    // (`https://evil#@github.com/x` goes to `evil`), and for ssh:// and
    // git:// git decodes `%2f` and strips `[…]` before picking the host
    // (`ssh://evil%2f@github.com/x`, `[evil]@github.com:x`). A repository
    // URL needs none of them, so IPv6 literals are refused too.
    if url.chars().any(|c| {
        matches!(c, '?' | '#' | '\\' | '%' | '[' | ']') || c.is_whitespace() || c.is_control()
    }) {
        return false;
    }
    if let Some((scheme, rest)) = url.split_once("://") {
        return matches!(scheme, "https" | "http" | "ssh" | "git" | "file")
            && !rest.starts_with('-');
    }
    true
}

/// Whether a URL is a path on this machine rather than a remote.
/// As git decides it: a URL is remote only with a `scheme://` (other than
/// `file://`) or in scp form, a `:` before any `/` (`git@host:repo`);
/// everything else, `objects/../../x.git` included, is a path.
pub fn is_local_url(url: &str) -> bool {
    if let Some((scheme, _)) = url.split_once("://") {
        return scheme == "file";
    }
    let b = url.as_bytes();
    // `C:\x` or `C:/x` is a drive, not a host called `C`.
    let drive = b.len() > 2 && b[0].is_ascii_alphabetic() && b[1] == b':';
    let scp = match (url.find(':'), url.find('/')) {
        (Some(c), Some(s)) => c < s,
        (Some(_), None) => true,
        (None, _) => false,
    };
    drive || !scp
}

/// The host a remote URL points at (`github.com` for
/// `https://github.com/a/b`, `git@github.com:a/b` and `ssh://git@github.com/a`).
pub fn url_host(url: &str) -> Option<String> {
    let (host, port) = match url.split_once("://") {
        Some((_, rest)) => {
            let authority = rest.split('/').next()?;
            let hostport = authority.rsplit('@').next()?;
            match hostport.split_once(':') {
                Some((h, p)) => (h, Some(p)),
                None => (hostport, None),
            }
        }
        // scp form: `user@host:path`.
        None => (url.split(':').next()?.rsplit('@').next()?, None),
    };
    // Strict, or None: git keeps `github.com:1.evil.com` whole as a host
    // name where a lax parse would read `github.com`.
    let port_ok =
        port.is_none_or(|p| (1..=5).contains(&p.len()) && p.bytes().all(|b| b.is_ascii_digit()));
    let host_ok = !host.is_empty()
        && !host.starts_with(['-', '.'])
        && host
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'.' || b == b'-');
    (port_ok && host_ok).then(|| host.to_ascii_lowercase())
}

/// Whether the commit's `.gitattributes` send any path through Git LFS.
pub async fn uses_lfs(dir: &Path, sha: &str) -> bool {
    git_ok(
        dir,
        &[
            "grep",
            "-q",
            "-F",
            "filter=lfs",
            sha,
            "--",
            ":(glob)**/.gitattributes",
        ],
    )
    .await
}

/// Whether `git lfs` is installed.
pub async fn lfs_available() -> bool {
    git_ok(Path::new("."), &["lfs", "version"]).await
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
    #[tokio::test]
    async fn reads_many_files_in_one_batch() {
        let dir = tempfile::tempdir().unwrap();
        let d = dir.path();
        std::fs::create_dir_all(d.join("a")).unwrap();
        std::fs::write(d.join("a/package.json"), "{\"name\":\"a\"}\n").unwrap();
        std::fs::write(d.join("b.txt"), "line 1\nline 2\n").unwrap();
        std::fs::write(d.join("empty"), "").unwrap();
        for args in [
            &["init", "-q", "-b", "main"][..],
            &["-c", "user.email=t@t", "-c", "user.name=t", "add", "-A"],
            &[
                "-c",
                "user.email=t@t",
                "-c",
                "user.name=t",
                "commit",
                "-qm",
                "x",
            ],
        ] {
            git(d, args).await.unwrap();
        }
        // A newline in a path cannot shift later answers onto other paths.
        let paths: Vec<String> = [
            "a/package.json",
            "evil\nb.txt",
            "missing.txt",
            "b.txt",
            "empty",
        ]
        .iter()
        .map(|s| s.to_string())
        .collect();
        let got = read_blobs(d, "HEAD", &paths).await.unwrap();
        assert_eq!(got["a/package.json"], b"{\"name\":\"a\"}\n");
        assert_eq!(got["b.txt"], b"line 1\nline 2\n");
        assert_eq!(got["empty"], b"");
        assert!(!got.contains_key("missing.txt"));
        assert!(!got.contains_key("evil\nb.txt"));
        // A failing git is an error, not "no files".
        assert!(read_blobs(&d.join("nope"), "HEAD", &paths).await.is_err());
    }

    use super::*;

    #[test]
    fn submodule_urls_resolve_like_git() {
        let r = resolve_submodule_url;
        assert_eq!(
            r("https://github.com/acme/app.git", "../lib.git"),
            "https://github.com/acme/lib.git"
        );
        assert_eq!(
            r("https://github.com/acme/app", "./sub"),
            "https://github.com/acme/app/sub"
        );
        assert_eq!(
            r("git@github.com:acme/app.git", "../lib.git"),
            "git@github.com:acme/lib.git"
        );
        assert_eq!(
            r("/srv/git/app.git", "../../other/lib.git"),
            "/srv/other/lib.git"
        );
        assert_eq!(r("https://x/a.git", "https://y/b.git"), "https://y/b.git");
        assert!(is_local_url("/tmp/x.git") && is_local_url("file:///x") && is_local_url("C:\\x"));
        assert!(!is_local_url("git@github.com:a/b") && !is_local_url("https://github.com/a/b"));
        // Paths git takes as local though they look relative or hostless.
        for local in [
            "objects/../../x.git",
            "a/b:c",
            "lib.git",
            "C:/x",
            "file:///x",
        ] {
            assert!(is_local_url(local), "{local}");
        }
        assert_eq!(
            url_host("https://GitHub.com/a/b.git").as_deref(),
            Some("github.com")
        );
        assert_eq!(
            url_host("git@github.com:a/b").as_deref(),
            Some("github.com")
        );
        assert_eq!(
            url_host("ssh://git@gh.example:22/a").as_deref(),
            Some("gh.example")
        );
        assert_eq!(url_host("https://[::1/x"), None);
        for bad in [
            "-oProxyCommand=x",
            "ext::sh -c id",
            "ssh://-oProxyCommand=x/a",
            "fd::3",
            "gopher://x/a",
            "https://evil.example#@github.com/x",
            "https://evil.example?@github.com/x",
            "git://127.0.0.1:19418%2f@github.com/acme/x",
            "ssh://127.0.0.1%2f@github.com/acme/x",
            "ssh://evil.example%3a2222%2f@github.com/x",
            "ssh://[evil.example]@github.com/acme/x",
            "git://[127.0.0.1]@github.com/acme/x",
            "[evil.example]@github.com:acme/x",
            "https://evil.example\\@github.com/x",
        ] {
            assert!(!is_fetchable_url(bad), "{bad}");
        }
        for good in [
            "https://github.com/a/b.git",
            "git@github.com:a/b",
            "ssh://git@h/a",
            "/srv/a.git",
        ] {
            assert!(is_fetchable_url(good), "{good}");
        }
    }

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
