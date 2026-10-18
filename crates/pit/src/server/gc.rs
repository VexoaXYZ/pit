//! Keeps the cache under a size limit, the way bazel-remote does: when the
//! CAS grows past the limit, the least recently used cache entries (task
//! results and Turborepo/Bazel artifacts) are evicted and blobs nothing refers
//! to any more are deleted, until usage is back under 90% of the limit.
//!
//! Blobs younger than the grace period are never deleted, because an upload
//! that is about to be referenced by a cache entry looks unreferenced for a
//! moment. Logs and overlays of runs from the last 14 days are kept.

use std::sync::atomic::Ordering;
use std::time::{Duration, SystemTime};

use anyhow::Result;
use serde::Serialize;

use super::App;
use crate::model::{now_ms, Overlay};

/// Logs and overlays of finished runs older than this may be deleted.
const RUN_RETENTION_MS: i64 = 14 * 24 * 60 * 60 * 1000;

#[derive(Debug, Default, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct GcReport {
    pub limit_bytes: u64,
    pub bytes_before: u64,
    pub bytes_after: u64,
    pub evicted_entries: usize,
    pub deleted_blobs: usize,
    pub freed_bytes: u64,
}

/// Runs a collection if the CAS is over its limit (or always, if `force`).
pub fn collect(app: &App, force: bool) -> Result<GcReport> {
    let bytes_before = app.cas.total_bytes();
    // Lowered while the disk is below the free-space floor (see
    // `disk::min_free_bytes`), so a cache under its limit can't fill it.
    let limit = crate::worker::disk::limit_for(
        bytes_before,
        app.cache_max_bytes.load(Ordering::Relaxed),
        crate::worker::disk::shortfall(app.cas.root()),
    );
    let mut report = GcReport {
        limit_bytes: limit,
        bytes_before,
        ..Default::default()
    };
    let over = limit > 0 && report.bytes_before > limit;
    if !force && !over {
        report.bytes_after = report.bytes_before;
        return Ok(report);
    }
    let target = limit / 10 * 9;
    loop {
        // Young blobs cannot be deleted yet, so evicting more entries to make
        // room for them would only empty the cache. Stop once everything that
        // can be freed now is under the target; the next collection does the rest.
        let young = sweep(app, &mut report)?;
        if limit == 0 || app.cas.total_bytes().saturating_sub(young) <= target {
            break;
        }
        // Evict a tenth of what is left at a time, oldest first, then sweep
        // again to see what that freed.
        let batch = (app.db.evictable_entries()? / 10).max(1);
        let evicted = app.db.evict_lru(batch)?;
        if evicted == 0 {
            break; // Only recent logs and in-flight uploads remain.
        }
        report.evicted_entries += evicted;
    }
    report.bytes_after = app.cas.total_bytes();
    if report.evicted_entries > 0 || report.deleted_blobs > 0 {
        app.evicted_entries
            .fetch_add(report.evicted_entries as u64, Ordering::Relaxed);
        app.freed_bytes
            .fetch_add(report.freed_bytes, Ordering::Relaxed);
        tracing::info!(
            "cache gc: evicted {} entries, deleted {} blobs, freed {} bytes ({} -> {}, limit {})",
            report.evicted_entries,
            report.deleted_blobs,
            report.freed_bytes,
            report.bytes_before,
            report.bytes_after,
            limit
        );
        app.emit(
            "cache.evicted",
            None,
            None,
            None,
            serde_json::to_value(&report)?,
        );
    }
    Ok(report)
}

/// Deletes blobs that nothing refers to and that are older than the grace
/// period. Returns the bytes held by blobs still inside the grace period.
fn sweep(app: &App, report: &mut GcReport) -> Result<u64> {
    // Artifacts get a quarter of the cache budget: past it, the oldest runs'
    // go first, before any build cache entry would.
    let budget = app.cache_max_bytes.load(Ordering::Relaxed) / 4;
    let (mut keep, overlays) = app
        .db
        .referenced_blobs(now_ms() - RUN_RETENTION_MS, budget)?;
    for d in overlays {
        if let Some(bytes) = app.cas.get(&d)? {
            if let Ok(overlay) = serde_json::from_slice::<Overlay>(&bytes) {
                keep.extend(overlay.entries.into_iter().filter_map(|e| e.digest));
            }
        }
    }
    let grace = Duration::from_millis(app.gc_grace_ms.load(Ordering::Relaxed));
    let cutoff = SystemTime::now()
        .checked_sub(grace)
        .unwrap_or(SystemTime::UNIX_EPOCH);
    let mut young = 0;
    for blob in app.cas.blobs() {
        if blob.modified > cutoff {
            young += blob.size;
            continue;
        }
        if keep.contains(&blob.digest) {
            continue;
        }
        let freed = app.cas.remove(&blob.digest);
        if freed > 0 || blob.size == 0 {
            report.deleted_blobs += 1;
            report.freed_bytes += freed;
        }
    }
    Ok(young)
}
