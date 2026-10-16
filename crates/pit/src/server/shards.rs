//! Balances sharded tasks by recorded duration.
//!
//! Test runners split shards by file count (`vitest --shard`, Playwright,
//! Jest), so one shard can collect the slow files and set the wall time
//! while the others idle. With `shardFiles`, Pit deals the files out itself:
//! longest first, each to the shard with the least time so far, using
//! per-file durations from earlier runs. Files without a recorded duration
//! count as the average. The split is made once, at planning, so every shard
//! sees the same one.

use std::collections::HashMap;

use anyhow::Result;

use super::App;
use crate::graph::PlannedGroup;

/// Greedy longest-processing-time packing. Deterministic: ties go to the
/// lower shard, and files are ordered by path within a shard.
pub fn assign(files: &[String], weights: &HashMap<String, f64>, shards: u32) -> Vec<Vec<String>> {
    let shards = shards.max(1) as usize;
    let known: Vec<f64> = files
        .iter()
        .filter_map(|f| weights.get(f).copied())
        .collect();
    let average = if known.is_empty() {
        1.0
    } else {
        known.iter().sum::<f64>() / known.len() as f64
    };
    let mut items: Vec<(f64, &String)> = files
        .iter()
        .map(|f| (weights.get(f).copied().unwrap_or(average), f))
        .collect();
    items.sort_by(|a, b| b.0.total_cmp(&a.0).then_with(|| a.1.cmp(b.1)));
    let mut load = vec![0.0f64; shards];
    let mut out = vec![Vec::new(); shards];
    for (w, f) in items {
        let k = (0..shards)
            .min_by(|&a, &b| load[a].total_cmp(&load[b]))
            .unwrap_or(0);
        load[k] += w;
        out[k].push(f.clone());
    }
    for list in &mut out {
        list.sort();
    }
    out
}

/// Fills in `shard_assigned` for every sharded task with `shard_files`.
pub fn plan(
    app: &App,
    repo_id: &str,
    groups: &mut [PlannedGroup],
    files: &[(String, String)],
) -> Result<()> {
    let mut splits: HashMap<String, Vec<Vec<String>>> = HashMap::new();
    for g in groups.iter_mut() {
        for (name, def) in g.tasks.iter_mut() {
            let (Some(k), Some(n)) = (def.shard, def.shards) else {
                continue;
            };
            if def.shard_files.is_empty() {
                continue;
            }
            if !splits.contains_key(name) {
                let set = crate::graph::globset(&def.shard_files)?;
                let matching: Vec<String> = files
                    .iter()
                    .map(|(p, _)| p.clone())
                    .filter(|p| set.is_match(p))
                    .collect();
                let weights = app.db.shard_timings(repo_id, name)?;
                splits.insert(name.clone(), assign(&matching, &weights, n));
            }
            def.shard_assigned = splits[name.as_str()]
                .get(k as usize - 1)
                .cloned()
                .unwrap_or_default();
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn deals_longest_first_to_the_least_loaded_shard() {
        let files: Vec<String> = ["a", "b", "c", "d", "e", "f"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        let weights = HashMap::from([
            ("a".to_string(), 10.0),
            ("b".to_string(), 1.0),
            ("c".to_string(), 1.0),
            ("d".to_string(), 1.0),
            ("e".to_string(), 1.0),
            ("f".to_string(), 1.0),
        ]);
        let split = assign(&files, &weights, 3);
        // The slow file gets a shard to itself; the rest share the others.
        assert_eq!(split[0], ["a"]);
        assert_eq!(split.iter().map(Vec::len).sum::<usize>(), 6);
        // A count split would have put `a` with a second file.
        let even = assign(&files, &HashMap::new(), 3);
        assert!(even.iter().all(|s| s.len() == 2));
        // Unknown files count as the average and every file lands once.
        let partial = HashMap::from([("a".to_string(), 10.0)]);
        let mut all: Vec<String> = assign(&files, &partial, 2).concat();
        all.sort();
        assert_eq!(all, files);
    }
}
