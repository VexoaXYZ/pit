//! Keeps a worker's warm workspaces from filling its disk.
//!
//! Warm workspaces are the point of a worker (`node_modules` and `target`
//! survive between runs), but they only ever grow: a repo nobody builds any
//! more, a renamed group, or a burst that opened extra workspaces all leave
//! gigabytes behind. Long-lived self-hosted runners hit "No space left on
//! device" this way and then fail every job they take. So idle workspaces
//! unused for two weeks are removed, and past a size budget the least
//! recently used idle ones go first. A removed workspace only costs a cold
//! run.

use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

/// Idle workspaces unused this long are removed whatever their size.
pub const STALE_AFTER: Duration = Duration::from_secs(14 * 24 * 60 * 60);

/// A file Pit keeps about a workspace. It sits next to the workspace, not
/// in it: tasks (a fork's, in a container) can write anything inside the
/// workspace, `.git` included, and a symlink planted there would make the
/// worker write through it to any file its user can.
pub fn meta_file(ws: &Path, what: &str) -> PathBuf {
    let name = ws.file_name().unwrap_or_default().to_string_lossy();
    ws.with_file_name(format!(".{name}.{what}"))
}

/// Every file `meta_file` may have made for this workspace.
pub fn remove_meta(ws: &Path) {
    for what in ["used", "warm.json"] {
        let _ = std::fs::remove_file(meta_file(ws, what));
    }
}

/// Marks when a workspace was last used; its mtime survives restarts.
pub fn used_file(ws: &Path) -> PathBuf {
    meta_file(ws, "used")
}

pub fn touch_used(ws: &Path) {
    let _ = std::fs::write(used_file(ws), b"");
}

#[derive(Debug, Clone)]
pub struct Workspace {
    pub path: PathBuf,
    pub bytes: u64,
    pub last_used: SystemTime,
}

/// The containers' `$HOME` directories (`homes/<repo>/<pool>`), with when
/// each was last used.
pub fn scan_homes(root: &Path) -> Vec<Workspace> {
    let mut found = Vec::new();
    let Ok(repos) = std::fs::read_dir(root) else {
        return found;
    };
    // `.trash` holds homes being removed.
    for repo in repos.flatten().filter(|e| {
        e.file_type().is_ok_and(|t| t.is_dir()) && !e.file_name().to_string_lossy().starts_with('.')
    }) {
        let Ok(pools) = std::fs::read_dir(repo.path()) else {
            continue;
        };
        for pool in pools.flatten() {
            let name = pool.file_name();
            if name.to_string_lossy().starts_with('.')
                || !pool.file_type().is_ok_and(|t| t.is_dir())
            {
                continue;
            }
            let path = pool.path();
            found.push(Workspace {
                bytes: dir_size(&path),
                last_used: last_used(&path),
                path,
            });
        }
    }
    found
}

/// Every workspace under `root`: a directory with a `.git`, found at most
/// three levels down (`<repo>/<group>/<n>`, or older layouts above it).
pub fn scan(root: &Path) -> Vec<Workspace> {
    let mut found = Vec::new();
    find(root, 0, &mut found);
    found
        .into_iter()
        .map(|path| Workspace {
            bytes: dir_size(&path),
            last_used: last_used(&path),
            path,
        })
        .collect()
}

fn find(dir: &Path, depth: usize, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for e in entries.flatten() {
        let name = e.file_name();
        if name.to_string_lossy().starts_with('.') || !e.file_type().is_ok_and(|t| t.is_dir()) {
            continue;
        }
        let path = e.path();
        if path.join(".git").exists() {
            out.push(path);
        } else if depth < 2 {
            find(&path, depth + 1, out);
        }
    }
}

fn last_used(ws: &Path) -> SystemTime {
    [used_file(ws), meta_file(ws, "warm.json"), ws.to_path_buf()]
        .iter()
        .find_map(|p| std::fs::metadata(p).and_then(|m| m.modified()).ok())
        .unwrap_or(SystemTime::UNIX_EPOCH)
}

/// Space used on disk, without following symlinks.
pub fn dir_size(path: &Path) -> u64 {
    let mut total = 0;
    let mut stack = vec![path.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for e in entries.flatten() {
            let Ok(meta) = e.metadata() else { continue };
            if meta.is_dir() {
                stack.push(e.path());
            } else {
                total += on_disk(&meta);
            }
        }
    }
    total
}

#[cfg(unix)]
fn on_disk(meta: &std::fs::Metadata) -> u64 {
    use std::os::unix::fs::MetadataExt;
    meta.blocks() * 512
}

#[cfg(not(unix))]
fn on_disk(meta: &std::fs::Metadata) -> u64 {
    meta.len()
}

/// Which idle workspaces to remove: stale ones, then least recently used
/// until the total fits `max_bytes` (0 means no size limit). Busy
/// workspaces are never chosen but count toward the total.
pub fn plan(
    workspaces: &[Workspace],
    is_busy: impl Fn(&Path) -> bool,
    max_bytes: u64,
    now: SystemTime,
) -> Vec<PathBuf> {
    let mut idle: Vec<&Workspace> = workspaces.iter().filter(|w| !is_busy(&w.path)).collect();
    idle.sort_by_key(|w| w.last_used);
    let mut total: u64 = workspaces.iter().map(|w| w.bytes).sum();
    let mut out = Vec::new();
    for w in idle {
        let stale = now
            .duration_since(w.last_used)
            .is_ok_and(|age| age >= STALE_AFTER);
        let over = max_bytes > 0 && total > max_bytes;
        if !stale && !over {
            // Sorted oldest first: nothing later is stale either.
            break;
        }
        total -= w.bytes;
        out.push(w.path.clone());
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ws(name: &str, gb: u64, days_ago: u64) -> Workspace {
        Workspace {
            path: PathBuf::from(name),
            bytes: gb << 30,
            last_used: SystemTime::now() - Duration::from_secs(days_ago * 24 * 3600),
        }
    }

    #[test]
    fn container_homes_are_found_and_aged() {
        let root = tempfile::tempdir().unwrap();
        let home = root.path().join("acme_web-1234").join("untrusted-default");
        std::fs::create_dir_all(&home).unwrap();
        std::fs::write(home.join("big"), vec![0u8; 8192]).unwrap();
        touch_used(&home);
        let found = scan_homes(root.path());
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].path, home);
        assert!(found[0].bytes >= 8192);
        // Over budget and idle: chosen; used recently (busy): kept.
        let now = SystemTime::now() + STALE_AFTER;
        assert_eq!(plan(&found, |_| false, 1, now), std::slice::from_ref(&home));
        assert!(plan(&found, |_| true, 1, now).is_empty());
    }

    #[test]
    fn evicts_stale_then_least_recently_used_idle_workspaces() {
        let all = [
            ws("old-repo", 1, 30),
            ws("app/build/1", 5, 3),
            ws("app/build/0", 6, 1),
            ws("app/test/0", 4, 2),
        ];
        let now = SystemTime::now();
        // Under budget: only the stale one goes.
        assert_eq!(
            plan(&all, |_| false, 100 << 30, now),
            [PathBuf::from("old-repo")]
        );
        // 16 GB in use, budget 10: stale first, then oldest until it fits.
        assert_eq!(
            plan(&all, |_| false, 10 << 30, now),
            [PathBuf::from("old-repo"), PathBuf::from("app/build/1")]
        );
        // A busy workspace is never removed, but still counts: with it
        // skipped, the next oldest go until 16 GB fits in 12.
        let busy = |p: &Path| p == Path::new("app/build/1");
        assert_eq!(
            plan(&all, busy, 12 << 30, now),
            [PathBuf::from("old-repo"), PathBuf::from("app/test/0")]
        );
        // No size limit: stale only.
        assert_eq!(plan(&all, |_| false, 0, now), [PathBuf::from("old-repo")]);
    }

    #[test]
    fn finds_workspaces_in_any_layout() {
        let root = tempfile::tempdir().unwrap();
        for dir in [
            "app/build/0/.git",
            "app/build/1/.git",
            "legacy/0/.git",
            "app/build/0/sub/.git",
        ] {
            std::fs::create_dir_all(root.path().join(dir)).unwrap();
        }
        std::fs::write(root.path().join("app/build/0/big"), vec![1u8; 100_000]).unwrap();
        let mut found = scan(root.path());
        found.sort_by(|a, b| a.path.cmp(&b.path));
        let names: Vec<_> = found
            .iter()
            .map(|w| w.path.strip_prefix(root.path()).unwrap().to_path_buf())
            .collect();
        assert_eq!(
            names,
            [
                PathBuf::from("app/build/0"),
                PathBuf::from("app/build/1"),
                PathBuf::from("legacy/0")
            ]
        );
        assert!(found[0].bytes >= 100_000);
    }
}
