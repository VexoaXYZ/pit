//! Turns a local working tree into "base commit + overlay" so a worker can run
//! exactly what is on disk, including uncommitted and untracked files, without
//! a push. Only changed files are uploaded, and only if the server lacks them.

use std::path::Path;

use anyhow::{Context, Result};

use crate::client::Client;
use crate::git;
use crate::model::{Overlay, OverlayEntry};

pub struct Snapshot {
    pub repo: String,
    pub url: String,
    pub sha: String,
    pub branch: Option<String>,
    pub overlay: Option<String>,
    pub files: usize,
    pub uploaded: usize,
}

/// Where a local checkout's commits live: the `origin` remote when there is
/// one (so workers fetch what GitHub or the team sees), else the directory.
pub struct RepoIdentity {
    pub root: std::path::PathBuf,
    pub name: String,
    pub url: String,
    pub remote: bool,
}

pub async fn identity(dir: &Path) -> Result<RepoIdentity> {
    let root = git::toplevel(dir)
        .await
        .context("not inside a git repository")?;
    let root = std::fs::canonicalize(&root).unwrap_or(root);
    let name = root
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_else(|| "repo".into());
    let origin = git::git(&root, &["remote", "get-url", "origin"])
        .await
        .ok()
        .map(|u| u.trim().to_string());
    let (url, remote) = match origin.filter(|u| !u.is_empty()) {
        Some(u) => {
            // Local remotes must be absolute for the server. Resolve them
            // lexically: the path may be unreachable right now (an outage), and
            // a relative URL saved then would break the repo for good.
            let url = if is_local_path(&u) {
                let joined = root.join(&u);
                std::fs::canonicalize(&joined)
                    .unwrap_or_else(|_| normalize(&joined))
                    .to_string_lossy()
                    .to_string()
            } else {
                u
            };
            (url, true)
        }
        None => (root.to_string_lossy().to_string(), false),
    };
    Ok(RepoIdentity {
        root,
        name,
        url,
        remote,
    })
}

/// A remote that is a filesystem path, not a URL (`https://`, `file://`) or
/// scp-style address (`git@host:repo`).
fn is_local_path(remote: &str) -> bool {
    !remote.contains("://")
        && !remote.contains('@')
        && !remote.split('/').next().unwrap_or_default().contains(':')
}

/// Removes `.` and `..` components without touching the filesystem.
fn normalize(path: &Path) -> std::path::PathBuf {
    let mut out = std::path::PathBuf::new();
    for c in path.components() {
        match c {
            std::path::Component::ParentDir => {
                out.pop();
            }
            std::path::Component::CurDir => {}
            other => out.push(other),
        }
    }
    out
}

pub async fn local(client: &Client, dir: &Path) -> Result<Snapshot> {
    let id = identity(dir).await?;
    let root = id.root.clone();
    let head = git::rev_parse(&root, "HEAD")
        .await?
        .context("the repository has no commits yet; commit once so Pit has a base")?;
    let branch = git::git(&root, &["rev-parse", "--abbrev-ref", "HEAD"])
        .await
        .ok()
        .map(|b| b.trim().to_string())
        .filter(|b| b != "HEAD");
    // With a remote, the base must be a commit the remote has. Unpushed local
    // commits then travel in the overlay along with uncommitted changes.
    let (url, sha) = if id.remote {
        match pushed_base(&root, &head, branch.as_deref()).await? {
            Some(base) => (id.url.clone(), base),
            None => (root.to_string_lossy().to_string(), head.clone()),
        }
    } else {
        (id.url.clone(), head.clone())
    };
    let repo = id.name.clone();

    let mut paths: Vec<String> = Vec::new();
    for args in [
        vec!["diff", "--name-only", "--no-renames", "-z", sha.as_str()],
        vec!["ls-files", "--others", "--exclude-standard", "-z"],
    ] {
        let out = git::git(&root, &args).await?;
        paths.extend(out.split('\0').filter(|p| !p.is_empty()).map(String::from));
    }
    paths.sort();
    paths.dedup();

    let mut entries = Vec::new();
    let mut blobs = Vec::new();
    for path in paths {
        let abs = root.join(&path);
        let meta = std::fs::symlink_metadata(&abs);
        match meta {
            Ok(m) if m.is_file() => {
                let bytes = std::fs::read(&abs)?;
                let digest = crate::cas::digest(&bytes);
                entries.push(OverlayEntry {
                    path,
                    digest: Some(digest.clone()),
                    executable: crate::worker::files::is_executable(&abs),
                });
                blobs.push((digest, bytes));
            }
            Ok(_) => {} // Directories (submodules) and symlinks are not shipped.
            Err(_) => entries.push(OverlayEntry {
                path,
                digest: None,
                executable: false,
            }),
        }
    }
    let files = entries.len();
    let overlay = if entries.is_empty() {
        None
    } else {
        let manifest = serde_json::to_vec(&Overlay { entries })?;
        let digest = crate::cas::digest(&manifest);
        blobs.push((digest.clone(), manifest));
        Some(digest)
    };
    let uploaded = client.upload_missing(blobs).await?;
    Ok(Snapshot {
        repo,
        url,
        sha,
        branch,
        overlay,
        files,
        uploaded,
    })
}

/// The newest commit in HEAD's history that `origin` already has. Other
/// remotes do not count: the server fetches from origin.
async fn pushed_base(root: &Path, head: &str, branch: Option<&str>) -> Result<Option<String>> {
    let contains = git::git(
        root,
        &["branch", "-r", "--contains", head, "--list", "origin/*"],
    )
    .await
    .unwrap_or_default();
    if !contains.trim().is_empty() {
        return Ok(Some(head.to_string()));
    }
    let mut candidates = Vec::new();
    if let Some(b) = branch {
        let upstream = git::git(
            root,
            &["rev-parse", "--abbrev-ref", &format!("{b}@{{upstream}}")],
        )
        .await
        .unwrap_or_default();
        if upstream.trim().starts_with("origin/") {
            candidates.push(upstream.trim().to_string());
        }
    }
    candidates.extend([
        "origin/HEAD".to_string(),
        "origin/main".into(),
        "origin/master".into(),
    ]);
    for c in candidates {
        if git::rev_parse(root, &c).await?.is_some() {
            if let Some(base) = git::merge_base(root, head, &c).await? {
                return Ok(Some(base));
            }
        }
    }
    Ok(None)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn local_remotes_resolve_even_when_missing() {
        assert!(is_local_path("../origin.git"));
        assert!(is_local_path("/srv/git/repo.git"));
        assert!(!is_local_path("https://github.com/a/b.git"));
        assert!(!is_local_path("git@github.com:a/b.git"));
        assert!(!is_local_path("host:path/repo.git"));
        assert_eq!(
            normalize(Path::new("/tmp/p7/work/../origin.git")),
            Path::new("/tmp/p7/origin.git")
        );
    }
}
