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

pub async fn local(client: &Client, dir: &Path) -> Result<Snapshot> {
    let root = git::toplevel(dir).await.context("not inside a git repository")?;
    let root = std::fs::canonicalize(&root).unwrap_or(root);
    let sha = git::rev_parse(&root, "HEAD")
        .await?
        .context("the repository has no commits yet; commit once so Pit has a base")?;
    let branch = git::git(&root, &["rev-parse", "--abbrev-ref", "HEAD"])
        .await
        .ok()
        .map(|b| b.trim().to_string())
        .filter(|b| b != "HEAD");
    let repo = root
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_else(|| "repo".into());

    let mut paths: Vec<String> = Vec::new();
    for args in [
        &["diff", "--name-only", "--no-renames", "-z", "HEAD"][..],
        &["ls-files", "--others", "--exclude-standard", "-z"][..],
    ] {
        let out = git::git(&root, args).await?;
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
                entries.push(OverlayEntry { path, digest: Some(digest.clone()), executable: crate::worker::files::is_executable(&abs) });
                blobs.push((digest, bytes));
            }
            Ok(_) => {} // Directories (submodules) and symlinks are not shipped.
            Err(_) => entries.push(OverlayEntry { path, digest: None, executable: false }),
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
        url: root.to_string_lossy().to_string(),
        sha,
        branch,
        overlay,
        files,
        uploaded,
    })
}
