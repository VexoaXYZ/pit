//! Workspace file helpers: safe paths, writing blobs, and finding outputs.

use std::path::{Component, Path, PathBuf};

use anyhow::{bail, Result};

/// Joins a repo-relative path onto the workspace, refusing anything that
/// could escape it.
pub fn safe_join(root: &Path, rel: &str) -> Result<PathBuf> {
    let p = Path::new(rel);
    if rel.is_empty()
        || p.is_absolute()
        || p.components().any(|c| !matches!(c, Component::Normal(_)))
    {
        bail!("refusing unsafe path {rel:?}");
    }
    if p.components()
        .next()
        .is_some_and(|c| c.as_os_str() == ".git")
    {
        bail!("refusing to write inside .git: {rel:?}");
    }
    // The checkout is repository code: a committed symlink (`out -> /home`)
    // must not turn `out/x` into a path outside the workspace. The last
    // component may be one; callers replace or skip it rather than follow it.
    let mut dir = root.to_path_buf();
    let parts: Vec<_> = p.components().collect();
    for c in &parts[..parts.len() - 1] {
        dir.push(c);
        match std::fs::symlink_metadata(&dir) {
            Ok(m) if m.file_type().is_symlink() => {
                bail!(
                    "refusing to follow the symlink {} in {rel:?}",
                    dir.display()
                )
            }
            Ok(_) => {}
            Err(_) => break,
        }
    }
    Ok(root.join(p))
}

/// A file a task wrote, read without following a symlink (a task can swap
/// one in for `/dev/zero`), regular files only, at most `max` bytes.
pub fn read_task_file(path: &Path, max: u64) -> Option<Vec<u8>> {
    use std::io::Read;
    let mut open = std::fs::OpenOptions::new();
    open.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        open.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
    }
    let file = open.open(path).ok()?;
    let meta = file.metadata().ok()?;
    if !meta.is_file() || meta.len() > max {
        return None;
    }
    let mut bytes = Vec::new();
    file.take(max).read_to_end(&mut bytes).ok()?;
    Some(bytes)
}

pub fn write_file(dest: &Path, bytes: &[u8], executable: bool) -> Result<()> {
    if let Some(parent) = dest.parent() {
        std::fs::create_dir_all(parent)?;
    }
    // Replace rather than truncate, in case the old file is a hard link.
    let _ = std::fs::remove_file(dest);
    std::fs::write(dest, bytes)?;
    set_executable(dest, executable)?;
    Ok(())
}

#[cfg(unix)]
fn set_executable(path: &Path, executable: bool) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let mode = if executable { 0o755 } else { 0o644 };
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode))?;
    Ok(())
}

#[cfg(not(unix))]
fn set_executable(_: &Path, _: bool) -> Result<()> {
    Ok(())
}

#[cfg(unix)]
pub fn is_executable(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(path)
        .map(|m| m.permissions().mode() & 0o111 != 0)
        .unwrap_or(false)
}

#[cfg(not(unix))]
pub fn is_executable(_: &Path) -> bool {
    false
}

/// The directory a glob can match under: its leading literal components.
fn literal_prefix(pattern: &str) -> &str {
    let mut end = 0;
    for (i, part) in pattern.split('/').enumerate() {
        if part.contains(['*', '?', '[', '{']) {
            break;
        }
        end += part.len() + usize::from(i > 0);
    }
    &pattern[..end.min(pattern.len())]
}

/// Files in the workspace matching any output glob, as (repo-relative, absolute).
/// Only walks below each glob's literal prefix, so `dist/**` never scans
/// `node_modules`.
pub fn collect_outputs(ws: &Path, globs: &[String]) -> Result<Vec<(String, PathBuf)>> {
    if globs.is_empty() {
        return Ok(vec![]);
    }
    let set = crate::graph::globset(globs)?;
    let mut roots: Vec<&str> = globs.iter().map(|g| literal_prefix(g)).collect();
    roots.sort();
    roots.dedup();
    let mut out = Vec::new();
    let mut seen = std::collections::HashSet::new();
    for root in roots {
        let start = if root.is_empty() {
            ws.to_path_buf()
        } else {
            safe_join(ws, root)?
        };
        walk(ws, &start, &mut |rel, abs| {
            if set.is_match(rel) && seen.insert(rel.to_string()) {
                out.push((rel.to_string(), abs.to_path_buf()));
            }
        })?;
    }
    out.sort();
    Ok(out)
}

/// Removes files matching output globs, so a cache restore never mixes with
/// stale outputs from an older run in this warm workspace.
pub fn clear_outputs(ws: &Path, globs: &[String]) -> Result<()> {
    for (_, abs) in collect_outputs(ws, globs)? {
        let _ = std::fs::remove_file(abs);
    }
    Ok(())
}

fn walk(ws: &Path, path: &Path, f: &mut dyn FnMut(&str, &Path)) -> Result<()> {
    let meta = match std::fs::symlink_metadata(path) {
        Ok(m) => m,
        Err(_) => return Ok(()),
    };
    if meta.is_file() {
        if let Ok(rel) = path.strip_prefix(ws) {
            let rel = rel.to_string_lossy().replace('\\', "/");
            f(&rel, path);
        }
        return Ok(());
    }
    if !meta.is_dir() {
        return Ok(());
    }
    if path.file_name().is_some_and(|n| n == ".git") {
        return Ok(());
    }
    for entry in std::fs::read_dir(path)? {
        walk(ws, &entry?.path(), f)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_escaping_paths() {
        let root = Path::new("/ws");
        assert!(safe_join(root, "src/a.ts").is_ok());
        for bad in ["../x", "/etc/passwd", "a/../../x", ".git/config", ""] {
            assert!(safe_join(root, bad).is_err(), "{bad}");
        }
    }

    #[cfg(unix)]
    #[test]
    fn never_follows_a_symlinked_directory_out_of_the_workspace() {
        let outside = tempfile::tempdir().unwrap();
        std::fs::write(outside.path().join("keep"), "precious").unwrap();
        let ws = tempfile::tempdir().unwrap();
        std::os::unix::fs::symlink(outside.path(), ws.path().join("r")).unwrap();
        assert!(safe_join(ws.path(), "r/keep").is_err());
        // A fork's `reports: ["r/**"]` deletes nothing outside, and a cached
        // output is not written through the link.
        assert!(clear_outputs(ws.path(), &["r/**".into()]).is_ok());
        std::fs::create_dir(outside.path().join("pit")).unwrap();
        std::fs::write(outside.path().join("pit/db"), "precious").unwrap();
        assert!(clear_outputs(ws.path(), &["r/pit/**".into()]).is_err());
        assert!(outside.path().join("pit/db").exists());
        assert!(collect_outputs(ws.path(), &["r/keep".into()]).is_err());
        assert_eq!(
            std::fs::read_to_string(outside.path().join("keep")).unwrap(),
            "precious"
        );
        // The link itself, as the last component, is fine to replace.
        assert!(safe_join(ws.path(), "r").is_ok());
        assert!(safe_join(ws.path(), "new/dir/file").is_ok());
    }

    #[test]
    fn literal_prefixes() {
        assert_eq!(literal_prefix("dist/**"), "dist");
        assert_eq!(
            literal_prefix("packages/web/dist/**/*.js"),
            "packages/web/dist"
        );
        assert_eq!(literal_prefix("**/*.d.ts"), "");
        assert_eq!(literal_prefix("out.txt"), "out.txt");
    }

    #[test]
    fn collects_only_matching_outputs() {
        let dir = tempfile::tempdir().unwrap();
        let ws = dir.path();
        write_file(&ws.join("dist/a.js"), b"a", false).unwrap();
        write_file(&ws.join("dist/sub/b.js"), b"b", false).unwrap();
        write_file(&ws.join("src/c.ts"), b"c", false).unwrap();
        let out = collect_outputs(ws, &["dist/**".into()]).unwrap();
        let rels: Vec<_> = out.iter().map(|(r, _)| r.as_str()).collect();
        assert_eq!(rels, vec!["dist/a.js", "dist/sub/b.js"]);
        clear_outputs(ws, &["dist/**".into()]).unwrap();
        assert!(!ws.join("dist/a.js").exists());
        assert!(ws.join("src/c.ts").exists());
    }
}
