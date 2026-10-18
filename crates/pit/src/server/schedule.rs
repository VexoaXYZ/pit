//! Scheduled runs: `schedules` in the config on a repo's default branch.
//!
//! GitHub's scheduled workflows are best effort. They "can be delayed during
//! periods of high loads", some are dropped, and in public repos they are
//! disabled after 60 days without activity. These run on the server's own
//! clock: each slot fires once, on the branch tip at that moment, as a trusted
//! run. A slot missed while the server was down is skipped, not replayed.
//! `pit schedules` lists them with their next and last runs, and
//! `pit schedules run` starts one now, to try it without waiting.

use std::collections::{BTreeMap, HashMap};
use std::sync::{Arc, OnceLock};

use anyhow::{Context, Result};
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

/// A repo's default branch and the schedules its tip's config declares.
struct Declared {
    mirror: std::path::PathBuf,
    branch: String,
    tip: String,
    schedules: BTreeMap<String, Schedule>,
}

/// `Ok(None)` when the repo has no default branch to read yet (never
/// fetched, or empty) or no config on it; `Err` when its config does not
/// load.
async fn declared(app: &Arc<App>, repo: &super::db::Repo) -> Result<Option<Declared>> {
    let mirror = app.mirror_dir(&repo.id);
    if !mirror.exists() {
        return Ok(None);
    }
    let Ok(branch) = super::queue::default_branch(&mirror).await else {
        return Ok(None);
    };
    let head = format!("refs/heads/{branch}");
    let Some(tip) = crate::git::rev_parse(&mirror, &head).await? else {
        return Ok(None);
    };
    let cached = read_cache()
        .lock()
        .get(&repo.id)
        .filter(|(at, _)| *at == tip)
        .map(|(_, s)| s.clone());
    let schedules = match cached {
        Some(s) if s.is_empty() => return Ok(None),
        Some(s) => s,
        // Errors are not cached: a timeout under load must not switch the
        // schedules off until the next commit.
        None => {
            // No config on the branch at all: no schedules, not an error.
            let mut any = false;
            for name in crate::config::CONFIG_FILES {
                if crate::git::show_file(&mirror, &tip, name).await?.is_some() {
                    any = true;
                    break;
                }
            }
            if !any {
                read_cache()
                    .lock()
                    .insert(repo.id.clone(), (tip.clone(), BTreeMap::new()));
                return Ok(None);
            }
            let c = app
                .load_config(&mirror, &tip, None, false)
                .await
                .with_context(|| format!("reading the config at {tip}"))?;
            read_cache()
                .lock()
                .insert(repo.id.clone(), (tip.clone(), c.schedules.clone()));
            c.schedules
        }
    };
    Ok(Some(Declared {
        mirror,
        branch,
        tip,
        schedules,
    }))
}

async fn repo_tick(app: &Arc<App>, repo: &super::db::Repo, first: i64, now: i64) -> Result<()> {
    let Some(d) = declared(app, repo).await? else {
        return Ok(());
    };
    for (name, s) in &d.schedules {
        let Ok(cron) = crate::cron::Schedule::parse(&s.cron, s.timezone.as_deref()) else {
            continue;
        };
        let Some(minute) = (first..=now).rev().find(|m| cron.fires(*m)) else {
            continue;
        };
        if !app
            .db
            .claim_schedule_slot(&format!("{}:{name}", repo.id), minute)?
        {
            continue;
        }
        if let Err(e) = start(app, repo, &d, name, s, true).await {
            tracing::warn!("schedule {name} on {}: {e:#}", repo.name);
        }
    }
    Ok(())
}

/// Every repo's schedules, when each fires next and how its last run went,
/// so a schedule that never runs says why instead of staying silent.
pub async fn list(app: &Arc<App>, now_ms: i64) -> Result<Vec<serde_json::Value>> {
    let now = now_ms.div_euclid(60_000);
    let mut out = Vec::new();
    for repo in app.db.repos()? {
        let d = match declared(app, &repo).await {
            Ok(Some(d)) => d,
            Ok(None) => continue,
            Err(e) => {
                out.push(serde_json::json!({ "repo": repo.name, "error": format!("{e:#}") }));
                continue;
            }
        };
        for (name, s) in &d.schedules {
            let next = crate::cron::Schedule::parse(&s.cron, s.timezone.as_deref())
                .ok()
                .and_then(|c| c.next_after(now))
                .map(|m| m * 60_000);
            let last = app
                .db
                .latest_run_by_trigger(&repo.id, &format!("schedule:{name}"))?
                .map(|(id, status, created_at)| {
                    serde_json::json!({ "id": id, "status": status, "createdAt": created_at })
                });
            out.push(serde_json::json!({
                "repo": repo.name,
                "branch": d.branch,
                "tip": d.tip,
                "name": name,
                "cron": s.cron,
                "timezone": s.timezone,
                "nextAt": next,
                "lastRun": last,
            }));
        }
    }
    Ok(out)
}

/// Starts a schedule now, as its cron would: the default branch's tip (as
/// last fetched, or fetched now with `repo_name`), as a trusted
/// `schedule:<name>` run, with the settings that tip's config declares. Does
/// not use up the next slot. The caller chooses when, and which repo: any
/// API caller can register a repo (`pit run` does), so this is no more than
/// what a `* * * * *` schedule in that repo already does within a minute.
pub async fn run_now(app: &Arc<App>, repo_name: Option<&str>, name: &str) -> Result<String> {
    let mut found = Vec::new();
    let mut matched_repo = false;
    for repo in app.db.repos()? {
        if repo_name.is_some_and(|r| r != repo.name) {
            continue;
        }
        // A schedule just pushed is what people try first. Only a named
        // repo is fetched: fetching every repo for a typo takes minutes.
        if repo_name.is_some() {
            matched_repo = true;
            if let Err(e) = app.fetch_mirror(&repo).await {
                tracing::warn!("schedules of {}: fetching {}: {e:#}", repo.name, repo.url);
            }
        }
        let d = match declared(app, &repo).await {
            Ok(Some(d)) => d,
            // A typo in the config just pushed: say so, not "no schedule".
            Err(e) if repo_name.is_some() => {
                return Err(e.context(format!("schedules of {}", repo.name)))
            }
            _ => continue,
        };
        if let Some(s) = d.schedules.get(name).cloned() {
            found.push((repo, d, s));
        }
    }
    let (repo, d, s) = match found.len() {
        0 => match repo_name {
            Some(r) if !matched_repo => anyhow::bail!("no repo {r:?} on this server"),
            Some(r) => anyhow::bail!("no schedule {name:?} on {r}'s default branch"),
            None => anyhow::bail!(
                "no schedule {name:?} on the default branch of any repo, as last fetched; \
                 pushed it just now? --repo <name> fetches that repo first"
            ),
        },
        1 => found.remove(0),
        _ => anyhow::bail!(
            "schedule {name:?} is on {}; say which with --repo",
            found
                .iter()
                .map(|(r, ..)| r.name.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        ),
    };
    // The tip whose config declared it, not one fetched since.
    start(app, &repo, &d, name, &s, false).await
}

async fn start(
    app: &Arc<App>,
    repo: &super::db::Repo,
    d: &Declared,
    name: &str,
    s: &Schedule,
    fetch: bool,
) -> Result<String> {
    let (mirror, branch, tip) = (&d.mirror, &d.branch, &d.tip);
    let head = format!("refs/heads/{branch}");
    // Run what is on the branch now, not what the mirror last saw.
    let sha = if !fetch {
        tip.to_string()
    } else {
        match app.fetch_mirror(repo).await {
            Ok(_) => crate::git::rev_parse(mirror, &head)
                .await?
                .unwrap_or_else(|| tip.to_string()),
            Err(e) => {
                tracing::warn!("schedule {name}: fetching {}: {e:#}", repo.url);
                tip.to_string()
            }
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
                counts_as: None,
                no_secrets: false,
                trusted: true,
            },
            None,
        )
        .await?;
    tracing::info!("schedule {name} on {}: started {id}", repo.name);
    Ok(id)
}
