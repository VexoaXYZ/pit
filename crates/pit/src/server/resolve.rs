//! Resolves fully cached groups on the server, without a worker.
//!
//! A group whose every task is a cache hit does no work, yet it would still
//! wait for a free worker slot just to report those hits; when every slot is
//! busy with long jobs that wait can be minutes. The server has the commit's
//! file tree (its mirror) and the overlay, so it computes the same action keys
//! a worker would and, if every task hits, finishes the group on the spot.
//!
//! Anything the server cannot vouch for goes to a worker as usual: tasks with
//! caching off, `--no-cache` runs, and workspace-cached setup tasks that no
//! cached task depends on (only a worker knows if its workspace ran them).

use std::collections::HashMap;
use std::path::Path;

use anyhow::Result;

use super::App;
use crate::graph::{key_inputs, PlannedGroup};
use crate::model::{current_os, CacheMode, Overlay, Status, TaskResult};

/// The input file index a worker would build: git blob ids of the commit's
/// tree, with overlay files as BLAKE3 digests.
pub async fn file_index(
    mirror: &Path,
    sha: &str,
    overlay: Option<&Overlay>,
) -> Result<Vec<(String, String)>> {
    let out = crate::git::git_bytes(mirror, &["ls-tree", "-r", "-z", "--full-tree", sha]).await?;
    let mut index: HashMap<String, String> = HashMap::new();
    for rec in out.split(|b| *b == 0).filter(|r| !r.is_empty()) {
        let rec = String::from_utf8_lossy(rec);
        // "<mode> <type> <oid>\t<path>"
        let Some((meta, path)) = rec.split_once('\t') else {
            continue;
        };
        if let Some(oid) = meta.split(' ').nth(2) {
            index.insert(path.to_string(), oid.to_string());
        }
    }
    for e in overlay.map(|o| o.entries.as_slice()).unwrap_or_default() {
        match &e.digest {
            Some(d) => index.insert(e.path.clone(), format!("b3:{d}")),
            None => index.remove(&e.path),
        };
    }
    let mut files: Vec<(String, String)> = index.into_iter().collect();
    files.sort();
    Ok(files)
}

/// Task results for a group if every task can be answered from the cache.
pub fn resolve(
    app: &App,
    group: &PlannedGroup,
    files: &[(String, String)],
) -> Result<Option<Vec<TaskResult>>> {
    // An `any` group's keys depend on the OS of the worker that would run it;
    // try the server's own, which is what a single-OS fleet uses.
    let os = if group.os == "any" {
        current_os().to_string()
    } else {
        group.os.clone()
    };
    let mut keys: HashMap<&str, String> = HashMap::new();
    let mut results = Vec::with_capacity(group.tasks.len());
    for (name, def) in &group.tasks {
        let dep_keys: Vec<String> = def
            .deps
            .iter()
            .filter_map(|d| keys.get(d.as_str()).cloned())
            .collect();
        let key = key_inputs(def, &os, files, &dep_keys)?.key();
        let result = match def.cache {
            CacheMode::Off => return Ok(None),
            CacheMode::Remote => match app.db.cache_peek(&key)? {
                Some(entry) => TaskResult {
                    status: Status::Cached,
                    exit_code: Some(0),
                    action_key: Some(key.clone()),
                    duration_ms: 0,
                    log_digest: entry.log_digest,
                    message: Some(format!(
                        "cache hit, resolved by the server without a worker (saved {:.1}s)",
                        entry.duration_ms as f64 / 1000.0
                    )),
                },
                None => return Ok(None),
            },
            CacheMode::Workspace => {
                // Setup for other tasks. Only skippable if something cached
                // depends on it; a leaf workspace task must really run.
                if !group.tasks.iter().any(|(_, t)| t.deps.contains(name)) {
                    return Ok(None);
                }
                TaskResult {
                    status: Status::Cached,
                    exit_code: Some(0),
                    action_key: Some(key.clone()),
                    duration_ms: 0,
                    log_digest: None,
                    message: Some(
                        "not needed: every task that depends on it came from the cache".into(),
                    ),
                }
            }
        };
        keys.insert(name.as_str(), key);
        results.push(result);
    }
    // Count the hits only once the whole group is known to resolve.
    for (r, (_, def)) in results.iter().zip(&group.tasks) {
        if def.cache == CacheMode::Remote {
            if let Some(key) = &r.action_key {
                app.db.cache_get(key)?;
            }
        }
    }
    Ok(Some(results))
}
