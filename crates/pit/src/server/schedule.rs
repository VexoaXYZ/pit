//! Scheduled runs: `schedules` in the config on a repo's default branch.
//!
//! GitHub's scheduled workflows are best effort. They "can be delayed during
//! periods of high loads", some are dropped, and in public repos they are
//! disabled after 60 days without activity. These run on the server's own
//! clock: each slot fires once, on the branch tip at that moment, as a trusted
//! run. A slot missed while the server was down is skipped, not replayed.

use std::collections::{BTreeMap, HashMap};
use std::sync::{Arc, OnceLock};

use anyhow::Result;
use parking_lot::Mutex;

use super::App;
use crate::model::{CreateRun, Schedule};

/// Each repo's schedules, by the branch tip they were read at, so the config
/// is evaluated once per commit rather than every tick.
type ReadCache = Mutex<HashMap<String, (String, BTreeMap<String, Schedule>)>>;

fn read_cache() -> &'static ReadCache {
    static CACHE: OnceLock<ReadCache> = OnceLock::new();
    CACHE.get_or_init(Default::default)
}

/// Starts the runs whose cron matched any minute since the last tick (at most
/// the last ten, so a long outage does not replay a backlog), up to the minute
/// containing `now_ms`.
pub async fn tick(app: &Arc<App>, now_ms: i64) -> Result<()> {
    let now = now_ms.div_euclid(60_000);
    let first = {
        let mut last = app.schedule_minute.lock();
        let first = match *last {
            Some(l) if l < now => (l + 1).max(now - 9),
            _ => now,
        };
        *last = Some(now);
        first
    };
    for repo in app.db.repos()? {
        if let Err(e) = repo_tick(app, &repo, first, now).await {
            tracing::warn!("schedules of {}: {e:#}", repo.name);
        }
    }
    Ok(())
}

async fn repo_tick(app: &Arc<App>, repo: &super::db::Repo, first: i64, now: i64) -> Result<()> {
    let mirror = app.mirror_dir(&repo.id);
    if !mirror.exists() {
        return Ok(());
    }
    let Ok(branch) = super::queue::default_branch(&mirror).await else {
        return Ok(());
    };
    let head = format!("refs/heads/{branch}");
    let Some(tip) = crate::git::rev_parse(&mirror, &head).await? else {
        return Ok(());
    };
    let cached = read_cache()
        .lock()
        .get(&repo.id)
        .filter(|(at, _)| *at == tip)
        .map(|(_, s)| s.clone());
    let schedules = match cached {
        Some(s) => s,
        // Errors are not cached: a timeout under load must not switch the
        // schedules off until the next commit.
        None => match app.load_config(&mirror, &tip, None, false).await {
            Ok(c) => {
                read_cache()
                    .lock()
                    .insert(repo.id.clone(), (tip.clone(), c.schedules.clone()));
                c.schedules
            }
            Err(e) => {
                tracing::warn!(
                    "schedules of {}: reading the config at {tip}: {e:#}",
                    repo.name
                );
                return Ok(());
            }
        },
    };
    for (name, s) in &schedules {
        let Ok(cron) = crate::cron::Cron::parse(&s.cron) else {
            continue;
        };
        let Some(minute) = (first..=now).rev().find(|m| cron.matches(*m)) else {
            continue;
        };
        if !app
            .db
            .claim_schedule_slot(&format!("{}:{name}", repo.id), minute)?
        {
            continue;
        }
        if let Err(e) = start(app, repo, &mirror, &branch, &tip, name, s).await {
            tracing::warn!("schedule {name} on {}: {e:#}", repo.name);
        }
    }
    Ok(())
}

async fn start(
    app: &Arc<App>,
    repo: &super::db::Repo,
    mirror: &std::path::Path,
    branch: &str,
    tip: &str,
    name: &str,
    s: &Schedule,
) -> Result<()> {
    let head = format!("refs/heads/{branch}");
    // Run what is on the branch now, not what the mirror last saw.
    let sha = match app.fetch_mirror(repo).await {
        Ok(_) => crate::git::rev_parse(mirror, &head)
            .await?
            .unwrap_or_else(|| tip.to_string()),
        Err(e) => {
            tracing::warn!("schedule {name}: fetching {}: {e:#}", repo.url);
            tip.to_string()
        }
    };
    let id = app
        .create_run(
            CreateRun {
                repo: repo.name.clone(),
                url: None,
                sha,
                overlay: None,
                tasks: s.tasks.clone(),
                trigger: Some(format!("schedule:{name}")),
                git_ref: Some(branch.to_string()),
                no_cache: s.no_cache,
                fresh: s.fresh,
                debug: false,
                no_secrets: false,
                trusted: true,
            },
            None,
        )
        .await?;
    tracing::info!("schedule {name} on {}: started {id}", repo.name);
    Ok(())
}
