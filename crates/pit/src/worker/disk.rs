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

/// Gives every directory under `root` (and `root`) its owner's write and
/// search bits back, without following symlinks. Tasks leave read-only trees
/// behind (Go's module cache, `chmod -R a-w`), and the worker, not running
/// as root, then cannot clean or delete them: every later run of that
/// workspace would fail.
pub fn make_dirs_writable(root: &Path) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut stack = vec![root.to_path_buf()];
        while let Some(dir) = stack.pop() {
            let Ok(meta) = std::fs::symlink_metadata(&dir) else {
                continue;
            };
            if !meta.is_dir() {
                continue;
            }
            let mode = meta.permissions().mode();
            if mode & 0o700 != 0o700 {
                let _ =
                    std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(mode | 0o700));
            }
            let Ok(entries) = std::fs::read_dir(&dir) else {
                continue;
            };
            for e in entries.flatten() {
                if e.file_type().is_ok_and(|t| t.is_dir()) {
                    stack.push(e.path());
                }
            }
        }
    }
    #[cfg(not(unix))]
    let _ = root;
}

/// `remove_dir_all`, which also removes read-only trees a task left (see
/// `make_dirs_writable`). Never follows symlinks.
pub fn remove_tree(path: &Path) -> std::io::Result<()> {
    match std::fs::remove_dir_all(path) {
        Err(e) if e.kind() != std::io::ErrorKind::NotFound => {
            make_dirs_writable(path);
            std::fs::remove_dir_all(path)
        }
        r => r,
    }
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

/// Space free to an unprivileged user on the filesystem holding `path`.
#[cfg(unix)]
#[allow(clippy::unnecessary_cast)] // the field types differ between platforms
pub fn free_bytes(path: &Path) -> Option<u64> {
    use std::os::unix::ffi::OsStrExt;
    let c = std::ffi::CString::new(path.as_os_str().as_bytes()).ok()?;
    // SAFETY: `c` is a valid C string and `s` a valid place for the result.
    let mut s: libc::statvfs = unsafe { std::mem::zeroed() };
    if unsafe { libc::statvfs(c.as_ptr(), &mut s) } != 0 {
        return None;
    }
    Some(s.f_bavail as u64 * s.f_frsize as u64)
}

#[cfg(not(unix))]
pub fn free_bytes(_path: &Path) -> Option<u64> {
    None
}

/// The free space Pit leaves on its disk, whatever its size limits say:
/// `PIT_MIN_FREE_GB` (default 5, `0` for none). A cache and workspaces under
/// their limits can still fill a small disk shared with other services.
pub fn min_free_bytes() -> u64 {
    std::env::var("PIT_MIN_FREE_GB")
        .ok()
        .and_then(|v| v.trim().parse::<f64>().ok())
        .map_or(5_000_000_000, |gb| (gb.max(0.0) * 1e9) as u64)
}

/// How far the disk holding `path` is below that floor (0 when it isn't,
/// or its free space is unknown).
pub fn shortfall(path: &Path) -> u64 {
    let floor = min_free_bytes();
    if floor == 0 {
        return 0;
    }
    free_bytes(path).map_or(0, |free| floor.saturating_sub(free))
}

/// A size limit that also frees `short` bytes from `total`: `max_bytes` (0
/// for none) lowered to fit. Never 0, which would mean no limit.
pub fn limit_for(total: u64, max_bytes: u64, short: u64) -> u64 {
    if short == 0 {
        return max_bytes;
    }
    let room = total.saturating_sub(short).max(1);
    if max_bytes == 0 {
        room
    } else {
        max_bytes.min(room)
    }
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

    #[cfg(unix)]
    #[test]
    fn read_only_trees_are_cleaned_up() {
        use std::os::unix::fs::PermissionsExt;
        let root = tempfile::tempdir().unwrap();
        let tree = root.path().join("modcache");
        let pkg = tree.join("golang.org/x/text@v0.14.0");
        std::fs::create_dir_all(&pkg).unwrap();
        std::fs::write(pkg.join("doc.go"), "package text").unwrap();
        // A symlink to a directory outside is never followed.
        let outside = root.path().join("outside");
        std::fs::create_dir(&outside).unwrap();
        std::fs::set_permissions(&outside, std::fs::Permissions::from_mode(0o500)).unwrap();
        std::os::unix::fs::symlink(&outside, tree.join("link")).unwrap();
        for d in [
            &pkg,
            &tree.join("golang.org/x"),
            &tree.join("golang.org"),
            &tree,
        ] {
            std::fs::set_permissions(d, std::fs::Permissions::from_mode(0o555)).unwrap();
        }
        make_dirs_writable(&tree);
        let mode = |p: &Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode(&pkg), 0o755);
        assert_eq!(mode(&tree), 0o755);
        assert_eq!(mode(&outside), 0o500, "followed a symlink");
        for d in [&pkg, &tree] {
            std::fs::set_permissions(d, std::fs::Permissions::from_mode(0o555)).unwrap();
        }
        remove_tree(&tree).unwrap();
        assert!(!tree.exists());
        assert!(outside.exists());
        assert!(
            remove_tree(&tree).is_err(),
            "a missing tree is still an error"
        );
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
    fn a_disk_short_of_its_floor_lowers_the_limit() {
        assert_eq!(limit_for(10, 0, 0), 0);
        assert_eq!(limit_for(10, 50, 0), 50);
        assert_eq!(limit_for(10, 0, 4), 6);
        assert_eq!(limit_for(10, 5, 4), 5);
        assert_eq!(
            limit_for(3, 0, 4),
            1,
            "everything idle goes, never 'no limit'"
        );
        assert!(free_bytes(Path::new(".")).is_none_or(|b| b > 0));
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
