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

/// Starts the runs whose cron matches the minute containing `now_ms`.
pub async fn tick(app: &Arc<App>, now_ms: i64) -> Result<()> {
    let minute = now_ms.div_euclid(60_000);
    for repo in app.db.repos()? {
        let mirror = app.mirror_dir(&repo.id);
        if !mirror.exists() {
            continue;
        }
        let Ok(branch) = super::queue::default_branch(&mirror).await else {
            continue;
        };
        let head = format!("refs/heads/{branch}");
        let Some(tip) = crate::git::rev_parse(&mirror, &head).await? else {
            continue;
        };
        let cached = read_cache()
            .lock()
            .get(&repo.id)
            .filter(|(at, _)| *at == tip)
            .map(|(_, s)| s.clone());
        let schedules = match cached {
            Some(s) => s,
            None => {
                // A broken config is reported by the runs that use it.
                let s = app
                    .load_config(&mirror, &tip, None)
                    .await
                    .map(|c| c.schedules)
                    .unwrap_or_default();
                read_cache()
                    .lock()
                    .insert(repo.id.clone(), (tip.clone(), s.clone()));
                s
            }
        };
        for (name, s) in &schedules {
            let Ok(cron) = crate::cron::Cron::parse(&s.cron) else {
                continue;
            };
            if !cron.matches(minute) {
                continue;
            }
            if !app
                .db
                .claim_schedule_slot(&format!("{}:{name}", repo.id), minute)?
            {
                continue;
            }
            // Run what is on the branch now, not what the mirror last saw.
            let sha = match app.fetch_mirror(&repo).await {
                Ok(_) => crate::git::rev_parse(&mirror, &head)
                    .await?
                    .unwrap_or(tip.clone()),
                Err(e) => {
                    tracing::warn!("schedule {name}: fetching {}: {e:#}", repo.url);
                    tip.clone()
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
                        git_ref: Some(branch.clone()),
                        no_cache: s.no_cache,
                        fresh: s.fresh,
                        trusted: true,
                    },
                    None,
                )
                .await?;
            tracing::info!("schedule {name} on {}: started {id}", repo.name);
        }
    }
    Ok(())
}
