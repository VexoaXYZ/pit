//! The grouped speculative merge queue.
//!
//! Each queued change gets an affected task set: the tasks whose inputs it
//! touches (plus their dependents), computed against the target tip.
//!
//! - **No tasks affected** (docs, comments): it lands at once.
//! - **Disjoint lanes**: a change whose affected set overlaps no active lane
//!   opens its own lane, tested on the current tip. Lanes test and land
//!   independently, so unrelated changes never wait for each other.
//! - **Speculative chains**: a change that overlaps one lane is appended to
//!   it, tested on top of the previous candidate (`tip+A`, `tip+A+B`, ...)
//!   in parallel. If a candidate fails, that change is ejected and every
//!   later change in the lane is retested without it.
//! - A change overlapping several lanes waits until they drain.
//!
//! Landing pushes the tested merge commit to the target branch (non-forced).
//! When another lane landed first, the change is re-merged onto the new tip,
//! which is safe because the lanes share no affected tasks. A post-merge run
//! on the target is the backstop for undeclared inputs.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;
use std::sync::Arc;

use anyhow::{bail, Context, Result};

use super::db::{QueueRow, Repo};
use super::App;
use crate::git;
use crate::model::*;

pub fn list(app: &App) -> Result<Vec<QueueEntryView>> {
    let repos: BTreeMap<String, String> = app
        .db
        .repos()?
        .into_iter()
        .map(|r| (r.id, r.name))
        .collect();
    Ok(app
        .db
        .queue_all(200)?
        .into_iter()
        .map(|r| view(&r, repos.get(&r.repo_id).cloned().unwrap_or_default()))
        .collect())
}

fn view(r: &QueueRow, repo: String) -> QueueEntryView {
    QueueEntryView {
        id: r.id.clone(),
        repo,
        source: r.source.clone(),
        head_sha: r.head_sha.clone(),
        target: r.target.clone(),
        status: r.status.clone(),
        lane: r.lane.clone(),
        position: r.position,
        affected: r.affected.clone(),
        candidate_sha: r.candidate_sha.clone(),
        run_id: r.run_id.clone(),
        landed_sha: r.landed_sha.clone(),
        reason: r.reason.clone(),
        created_at: r.created_at,
        updated_at: r.updated_at,
    }
}

fn source_ref(source: &str) -> String {
    match source.strip_prefix("pr/") {
        Some(n) => format!("refs/pull/{n}/head"),
        None => format!("refs/heads/{source}"),
    }
}

/// Requests removal of an entry (by id, or by branch / `pr/N` within `repo`)
/// and wakes the queue to carry it out. Returns how many entries matched.
pub fn remove(app: &App, key: &str, repo: Option<&str>, reason: &str) -> Result<usize> {
    let repo_id = match repo {
        Some(r) => Some(app.db.repo(r)?.id),
        None => None,
    };
    let n = app
        .db
        .queue_request_removal(key, repo_id.as_deref(), reason)?;
    if n > 0 {
        app.queue_kick.notify_one();
    }
    Ok(n)
}

/// Refused: on a shared server a GitHub repo's queue is entered only through
/// the `pit:merge` label.
#[derive(Debug)]
pub struct GithubOnlyLabel(pub String);

impl std::fmt::Display for GithubOnlyLabel {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{} is a GitHub repo: queue its pull requests with the `pit:merge` label",
            self.0
        )
    }
}

impl std::error::Error for GithubOnlyLabel {}

/// `from_api`: sent by an API caller, not by the GitHub webhook.
pub async fn add(app: &Arc<App>, req: QueueAdd, from_api: bool) -> Result<QueueEntryView> {
    let repo = app.db.upsert_repo(&req.repo, req.url.as_deref(), None)?;
    // A queued change runs as trusted, config included, and the mirror has
    // every pull request, forks' too. On a shared server, a GitHub repo's
    // queue is entered only through the `pit:merge` label, which a
    // maintainer applies after reading the change. Checked on the repo the
    // request resolved to: a new name with that repo's URL resolves to it.
    if from_api && app.token.is_some() && repo.github.is_some() {
        return Err(GithubOnlyLabel(repo.name).into());
    }
    // If origin is down, queue from the last fetched state of the branch; the
    // queue lands it once origin is back.
    let mut offline: Option<String> = None;
    let mirror = match app.fetch_mirror(&repo).await {
        Ok(m) => m,
        Err(e) if app.mirror_dir(&repo.id).join("HEAD").exists() => {
            let why = origin_error(&e);
            tracing::warn!("queueing from the last fetched state: {why}");
            offline = Some(why);
            app.mirror_dir(&repo.id)
        }
        Err(e) => bail!("cannot reach {}: {}", repo.url, origin_error(&e)),
    };
    let source = match (&req.branch, req.pr) {
        (Some(b), None) => b.clone(),
        (None, Some(n)) => format!("pr/{n}"),
        _ => bail!("queue a branch or a pull request, not both"),
    };
    let head = git::rev_parse(&mirror, &source_ref(&source))
        .await?
        .with_context(|| match &offline {
            Some(why) => format!(
                "{source} is not known to Pit yet, and {} is unreachable ({why}); \
                 Pit can only queue branches it fetched before the outage",
                repo.url
            ),
            None => format!("{source} not found in {}", repo.url),
        })?;
    // A fork's pull request is queued at this commit only: a later push
    // drops it rather than testing new code with the queue's trust. Unless
    // a webhook said the pull request comes from the repo itself, it is
    // treated as a fork: a missed webhook pins, it never trusts.
    let same_repo = req.pr.is_some_and(|n| {
        matches!(
            app.db.kv_get("pr-origin", &format!("{}/{n}", repo.id)),
            Ok(Some(o)) if o == "same"
        )
    });
    let pinned = req.sha.is_some() || (req.pr.is_some() && !same_repo);
    if let Some(approved) = &req.sha {
        if *approved != head {
            bail!("{source} moved to {head} after {approved} was approved; add the label again");
        }
    }
    let target = match req.target {
        Some(t) => t,
        None => default_branch(&mirror).await?,
    };
    if source == target {
        bail!("cannot queue {source} into itself");
    }
    if app
        .db
        .queue_active()?
        .iter()
        .any(|e| e.repo_id == repo.id && e.source == source && e.target == target)
    {
        bail!("{source} is already in the queue for {target}");
    }
    let id = app
        .db
        .queue_insert(&repo.id, &source, req.pr, &head, &target, pinned)?;
    app.emit(
        "queue.updated",
        None,
        None,
        None,
        serde_json::json!({ "id": id }),
    );
    tick(app).await?;
    let row = app
        .db
        .queue_all(500)?
        .into_iter()
        .find(|r| r.id == id)
        .context("queue entry vanished")?;
    Ok(view(&row, repo.name))
}

pub(crate) async fn default_branch(mirror: &Path) -> Result<String> {
    for b in ["main", "master", "trunk"] {
        if git::rev_parse(mirror, &format!("refs/heads/{b}"))
            .await?
            .is_some()
        {
            return Ok(b.to_string());
        }
    }
    bail!("could not find a default branch; pass --target")
}

/// Advances every queue: collects results, lands, and starts new candidates.
/// The useful line of a failed fetch, without git's boilerplate.
fn origin_error(e: &anyhow::Error) -> String {
    let text = format!("{e:#}");
    for marker in ["fatal: ", "error: "] {
        if let Some(i) = text.find(marker) {
            let rest = &text[i + marker.len()..];
            return rest.lines().next().unwrap_or(rest).trim().to_string();
        }
    }
    text.lines().next().unwrap_or_default().trim().to_string()
}

pub async fn tick(app: &Arc<App>) -> Result<()> {
    let _guard = app.queue_lock.lock().await;
    let active = app.db.queue_active()?;
    let mut by_target: BTreeMap<(String, String), Vec<QueueRow>> = BTreeMap::new();
    for e in active {
        by_target
            .entry((e.repo_id.clone(), e.target.clone()))
            .or_default()
            .push(e);
    }
    for ((repo_id, target), entries) in by_target {
        let repo = app.db.repo(&repo_id)?;
        if let Err(e) = tick_target(app, &repo, &target, entries).await {
            tracing::warn!("merge queue {}:{target}: {e:#}", repo.name);
        }
    }
    Ok(())
}

struct Queue<'a> {
    app: &'a Arc<App>,
    repo: &'a Repo,
    mirror: std::path::PathBuf,
    target: &'a str,
    tip: String,
    entries: Vec<QueueRow>,
    /// Runs cancelled this tick, closed once the tick is done.
    cancelled: Vec<String>,
}

async fn tick_target(
    app: &Arc<App>,
    repo: &Repo,
    target: &str,
    mut entries: Vec<QueueRow>,
) -> Result<()> {
    let mirror = match app.fetch_mirror(repo).await {
        Ok(m) => m,
        Err(e) => {
            // Origin is down (a GitHub outage, a network blip). Nothing can be
            // tested against the latest target or landed, so wait, and say why.
            let reason = format!("waiting for origin to come back ({})", origin_error(&e));
            for entry in entries
                .iter_mut()
                .filter(|e| e.reason.as_deref() != Some(reason.as_str()))
            {
                entry.reason = Some(reason.clone());
                app.db.queue_update(entry)?;
            }
            app.emit(
                "queue.updated",
                None,
                None,
                None,
                serde_json::json!({ "waiting": reason }),
            );
            return Ok(());
        }
    };
    if let Some(gh) = repo.github.as_deref().and(app.github.as_ref()) {
        let url = gh.push_url(repo.github.as_deref().unwrap()).await?;
        git::git(&mirror, &["config", "remote.origin.pushurl", &url]).await?;
    }
    let tip = git::rev_parse(&mirror, &format!("refs/heads/{target}"))
        .await?
        .with_context(|| format!("target branch {target} not found"))?;
    let mut q = Queue {
        app,
        repo,
        mirror,
        target,
        tip,
        entries,
        cancelled: vec![],
    };
    let result = async {
        q.collect_results().await?;
        q.land().await?;
        q.start_candidates().await
    }
    .await;
    for run in std::mem::take(&mut q.cancelled) {
        app.run_finished(&run).await;
    }
    result
}

impl Queue<'_> {
    fn save(&self, e: &QueueRow) -> Result<()> {
        self.app.db.queue_update(e)?;
        self.app.emit(
            "queue.updated",
            None,
            None,
            None,
            serde_json::json!({ "id": e.id, "status": e.status }),
        );
        Ok(())
    }

    /// Puts an entry back in line (dropping its candidate) and cancels its run.
    fn requeue(&mut self, idx: usize, reason: &str) -> Result<()> {
        let e = &mut self.entries[idx];
        if let Some(run) = e.run_id.take() {
            if self.app.db.cancel_run(&run).unwrap_or(false) {
                self.cancelled.push(run);
            }
        }
        e.status = "queued".into();
        e.lane = None;
        e.position = None;
        e.base_sha = None;
        e.candidate_sha = None;
        e.reason = Some(reason.to_string());
        let e = e.clone();
        self.save(&e)
    }

    /// Requeues every entry after `pos` in `lane`: their candidates contain
    /// a change that is no longer going in.
    fn requeue_after(&mut self, lane: &str, pos: i64, reason: &str) -> Result<()> {
        let idxs: Vec<usize> = self
            .entries
            .iter()
            .enumerate()
            .filter(|(_, e)| {
                e.lane.as_deref() == Some(lane)
                    && e.position.unwrap_or(0) > pos
                    && matches!(e.status.as_str(), "testing" | "passed")
            })
            .map(|(i, _)| i)
            .collect();
        for i in idxs {
            self.requeue(i, reason)?;
        }
        Ok(())
    }

    /// Takes an entry out of the queue for good. Later changes in its lane
    /// were tested with it, so they are retested without it.
    fn remove_entry(&mut self, i: usize, reason: &str) -> Result<()> {
        let (lane, pos) = (
            self.entries[i].lane.clone(),
            self.entries[i].position.unwrap_or(0),
        );
        self.requeue(i, reason)?;
        self.entries[i].status = "removed".into();
        let e = self.entries[i].clone();
        self.save(&e)?;
        if let Some(lane) = lane {
            self.requeue_after(
                &lane,
                pos,
                "an earlier change in this lane was removed; retesting without it",
            )?;
        }
        Ok(())
    }

    async fn collect_results(&mut self) -> Result<()> {
        for i in 0..self.entries.len() {
            if self.entries[i].status == "removing" {
                let reason = self.entries[i]
                    .reason
                    .clone()
                    .unwrap_or_else(|| "removed".into());
                self.remove_entry(i, &reason)?;
            }
        }
        // Entries whose branch moved since they were queued are retested;
        // entries whose branch is gone are dropped.
        for i in 0..self.entries.len() {
            if !matches!(
                self.entries[i].status.as_str(),
                "queued" | "testing" | "passed"
            ) {
                continue;
            }
            let source = self.entries[i].source.clone();
            let Some(head) = git::rev_parse(&self.mirror, &source_ref(&source)).await? else {
                self.remove_entry(i, "branch deleted")?;
                continue;
            };
            if head != self.entries[i].head_sha && self.entries[i].pinned {
                // Only the approved commit may run with the queue's trust.
                self.remove_entry(
                    i,
                    "new commits were pushed after it was queued; add the merge label again",
                )?;
                continue;
            }
            if head != self.entries[i].head_sha && self.entries[i].status != "landed" {
                let (lane, pos) = (
                    self.entries[i].lane.clone(),
                    self.entries[i].position.unwrap_or(0),
                );
                self.entries[i].head_sha = head;
                self.requeue(i, "branch updated, retesting")?;
                if let Some(lane) = lane {
                    self.requeue_after(&lane, pos, "an earlier change in this lane was updated")?;
                }
            }
        }
        // A lane's newest testing entry needs a run of its own. It can lack
        // one when the change it was batched into left the lane (removed,
        // updated, requeued); that run covers every batched entry before it.
        let lanes: BTreeSet<String> = self.entries.iter().filter_map(|e| e.lane.clone()).collect();
        for lane in lanes {
            let newest = (0..self.entries.len())
                .filter(|&j| {
                    self.entries[j].lane.as_deref() == Some(lane.as_str())
                        && self.entries[j].status == "testing"
                })
                .max_by_key(|&j| self.entries[j].position);
            if let Some(j) = newest.filter(|&j| self.entries[j].run_id.is_none()) {
                let o = self.entries[j].clone();
                let run = self.queue_run(&o, self.lane_tasks(&o)).await?;
                let o = &mut self.entries[j];
                o.run_id = Some(run);
                o.reason =
                    Some("testing again: the change it was batched with left the queue".into());
                let o = o.clone();
                self.save(&o)?;
            }
        }
        let mut order: Vec<usize> = (0..self.entries.len()).collect();
        order.sort_by_key(|&i| (self.entries[i].lane.clone(), self.entries[i].position));
        for i in order {
            if self.entries[i].status != "testing" {
                continue;
            }
            let Some(run_id) = self.entries[i].run_id.clone() else {
                continue;
            };
            let mut run = self.app.db.run(&run_id)?;
            // A candidate is decided by its first real failure: nothing later
            // in its run can make it pass, so the run stops there and the
            // queue moves on, instead of holding the lane until its slowest
            // task ends (GitHub's fail-fast, for the merge gate only; other
            // runs report every failure).
            if !run.status.is_done() {
                if let Some(task) = self.app.db.run_failed_task(&run_id)? {
                    let why = format!("stopped early: {task} failed");
                    if self.app.db.cancel_run_because(&run_id, &why)? {
                        self.app.db.fail_groups_with_failures(&run_id)?;
                        self.cancelled.push(run_id.clone());
                    }
                    run = self.app.db.run(&run_id)?;
                }
            }
            if !run.status.is_done() {
                continue;
            }
            if run.status.is_ok() {
                self.entries[i].status = "passed".into();
                self.entries[i].reason = None;
                let e = self.entries[i].clone();
                self.save(&e)?;
                // This candidate contains every earlier change in its lane, so
                // their own runs are no longer needed.
                let (lane, pos) = (e.lane.clone(), e.position);
                for j in 0..self.entries.len() {
                    let o = &self.entries[j];
                    if o.lane != lane || o.position >= pos || o.status != "testing" {
                        continue;
                    }
                    // A change whose own run already failed is not rescued
                    // by a later one: it fails, and the later ones are
                    // retested without it.
                    let own = match &o.run_id {
                        Some(r) => Some((r.clone(), self.app.db.run(r)?.status)),
                        None => None,
                    };
                    if own.as_ref().is_some_and(|(_, s)| s.is_done() && !s.is_ok()) {
                        continue;
                    }
                    // Saved first: a crash before the skip must not leave a
                    // skipped run on an entry that still counts it.
                    let o = &mut self.entries[j];
                    o.status = "passed".into();
                    o.reason = Some(format!("passed together with later changes in {run_id}"));
                    let o = o.clone();
                    self.save(&o)?;
                    if let Some((r, _)) = own {
                        if self
                            .app
                            .db
                            .skip_run_because(&r, &format!("covered by {run_id}"))?
                        {
                            self.app.run_finished(&r).await;
                        }
                    }
                }
            } else {
                // A candidate contains every earlier change in its lane. Until
                // those have passed, the failure may not be this change's fault.
                let (lane, pos) = (self.entries[i].lane.clone(), self.entries[i].position);
                // Changes batched into this candidate get their own runs now,
                // to find which one broke it.
                for j in 0..self.entries.len() {
                    let o = &self.entries[j];
                    if o.lane == lane
                        && o.position < pos
                        && o.status == "testing"
                        && o.run_id.is_none()
                    {
                        let o = o.clone();
                        let run = self.queue_run(&o, self.lane_tasks(&o)).await?;
                        let o = &mut self.entries[j];
                        o.run_id = Some(run);
                        o.reason =
                            Some("testing on its own: a batch with later changes failed".into());
                        let o = o.clone();
                        self.save(&o)?;
                    }
                }
                let earlier_untested = self
                    .entries
                    .iter()
                    .any(|o| o.lane == lane && o.position < pos && o.status == "testing");
                if earlier_untested {
                    continue;
                }
                let e = &mut self.entries[i];
                e.status = "failed".into();
                e.reason = Some(format!("tests failed in run {run_id}"));
                let (lane, pos) = (e.lane.clone().unwrap_or_default(), e.position.unwrap_or(0));
                let e = e.clone();
                self.save(&e)?;
                self.requeue_after(
                    &lane,
                    pos,
                    "an earlier change in this lane failed; retesting without it",
                )?;
            }
        }
        Ok(())
    }

    async fn land(&mut self) -> Result<()> {
        let lanes: BTreeSet<String> = self.entries.iter().filter_map(|e| e.lane.clone()).collect();
        for lane in lanes {
            // The leading run of passed entries lands as one push of the
            // newest, which contains the others: the target never sits on a
            // commit that was not itself tested.
            let mut pending: Vec<usize> = (0..self.entries.len())
                .filter(|&i| {
                    self.entries[i].lane.as_deref() == Some(lane.as_str())
                        && matches!(self.entries[i].status.as_str(), "testing" | "passed")
                })
                .collect();
            pending.sort_by_key(|&i| self.entries[i].position);
            let batch: Vec<usize> = pending
                .into_iter()
                .take_while(|&i| self.entries[i].status == "passed")
                .collect();
            if !batch.is_empty() {
                self.land_batch(&batch, "tested").await?;
            }
        }
        Ok(())
    }

    /// Pushes an entry to the target. Returns false if it could not land now.
    async fn land_one(&mut self, i: usize, how: &str) -> Result<bool> {
        self.land_batch(&[i], how).await
    }

    /// Pushes consecutive passed entries of one lane as a single commit: the
    /// newest one's candidate, which contains the others.
    async fn land_batch(&mut self, batch: &[usize], how: &str) -> Result<bool> {
        let (&first, &last) = (
            batch.first().context("empty batch")?,
            batch.last().context("empty batch")?,
        );
        let e = self.entries[last].clone();
        let candidate = e
            .candidate_sha
            .clone()
            .context("passed entry without a candidate")?;
        let commit = if git::is_ancestor(&self.mirror, &self.tip, &candidate).await {
            candidate
        } else {
            // Another lane landed first. Its tasks are disjoint from this one,
            // so re-merging onto the new tip keeps this lane's result valid.
            let mut c = self.tip.clone();
            for &i in batch {
                let o = &self.entries[i];
                match git::merge_commit(&self.mirror, &c, &o.head_sha, &merge_message(o)).await? {
                    Some(m) => c = m,
                    None => {
                        let lane = self.entries[first].lane.clone().unwrap_or_default();
                        let pos = self.entries[first].position.unwrap_or(0);
                        self.requeue(
                            first,
                            "conflicts with a change that landed first; retesting",
                        )?;
                        self.requeue_after(
                            &lane,
                            pos,
                            "an earlier change in this lane is being retested",
                        )?;
                        return Ok(false);
                    }
                }
            }
            c
        };
        if !git::push(
            &self.mirror,
            &commit,
            &format!("refs/heads/{}", self.target),
            false,
        )
        .await?
        {
            return Ok(false);
        }
        self.tip = commit.clone();
        for &i in batch {
            let e = &mut self.entries[i];
            e.status = "landed".into();
            e.landed_sha = Some(commit.clone());
            e.reason = Some(how.to_string());
            let e = e.clone();
            self.save(&e)?;
            self.app.emit(
                "queue.landed",
                None,
                None,
                None,
                serde_json::json!({ "id": e.id, "sha": commit }),
            );
        }
        // Backstop: the landed commit gets a full run on the target branch.
        self.app
            .create_run(
                CreateRun {
                    repo: self.repo.name.clone(),
                    url: None,
                    sha: commit,
                    overlay: None,
                    tasks: vec![],
                    trigger: Some("post-merge".into()),
                    git_ref: Some(self.target.to_string()),
                    no_cache: false,
                    fresh: false,
                    debug: false,
                    counts_as: None,
                    no_secrets: false,
                    trusted: super::trusted_branch(
                        self.target,
                        default_branch(&self.mirror).await.ok().as_deref(),
                    ),
                    draft: false,
                },
                None,
            )
            .await?;
        Ok(true)
    }

    async fn start_candidates(&mut self) -> Result<()> {
        let mut waiting: Vec<usize> = (0..self.entries.len())
            .filter(|&i| self.entries[i].status == "queued")
            .collect();
        waiting.sort_by_key(|&i| self.entries[i].created_at);
        for i in waiting {
            let e = self.entries[i].clone();
            let msg = merge_message(&e);
            let Some(merged) =
                git::merge_commit(&self.mirror, &self.tip, &e.head_sha, &msg).await?
            else {
                let e = &mut self.entries[i];
                e.status = "failed".into();
                e.reason = Some(format!(
                    "conflicts with {}; rebase and queue again",
                    self.target
                ));
                let e = e.clone();
                self.save(&e)?;
                continue;
            };
            let changed = git::changed_files(&self.mirror, &self.tip, &merged).await?;
            let config = match self
                .app
                .load_config(
                    self.repo,
                    &self.mirror,
                    &merged,
                    None,
                    self.app.config_sandboxed(self.repo, true),
                )
                .await
            {
                Ok(c) => c,
                Err(err) => {
                    let e = &mut self.entries[i];
                    e.status = "failed".into();
                    e.reason = Some(format!("{err:#}"));
                    let e = e.clone();
                    self.save(&e)?;
                    continue;
                }
            };
            let affected: Vec<String> = crate::graph::affected(&config, &changed)?
                .into_iter()
                .collect();
            self.entries[i].affected = affected.clone();

            if affected.is_empty() {
                // Nothing to test: land the merge directly.
                self.entries[i].candidate_sha = Some(merged);
                self.entries[i].base_sha = Some(self.tip.clone());
                self.entries[i].lane = Some(self.entries[i].id.clone());
                self.entries[i].position = Some(0);
                if !self.land_one(i, "no tasks affected").await? {
                    self.entries[i].lane = None;
                    self.entries[i].position = None;
                    self.entries[i].candidate_sha = None;
                    let e = self.entries[i].clone();
                    self.save(&e)?;
                }
                continue;
            }

            // Lanes in flight and the union of their affected tasks.
            let mut lanes: BTreeMap<String, (BTreeSet<String>, Option<usize>)> = BTreeMap::new();
            for (j, other) in self.entries.iter().enumerate() {
                let (Some(lane), true) = (
                    &other.lane,
                    matches!(other.status.as_str(), "testing" | "passed"),
                ) else {
                    continue;
                };
                let slot = lanes.entry(lane.clone()).or_default();
                slot.0.extend(other.affected.iter().cloned());
                if slot
                    .1
                    .is_none_or(|k| self.entries[k].position < other.position)
                {
                    slot.1 = Some(j);
                }
            }
            let mine: BTreeSet<String> = affected.iter().cloned().collect();
            let overlapping: Vec<_> = lanes
                .iter()
                .filter(|(_, (set, _))| !set.is_disjoint(&mine))
                .collect();

            let (lane, position, base, candidate, mut tasks) = match overlapping.as_slice() {
                [] => (e.id.clone(), 0, self.tip.clone(), merged, mine.clone()),
                [(lane, (set, Some(last)))] => {
                    let prev = &self.entries[*last];
                    let base = prev
                        .candidate_sha
                        .clone()
                        .context("lane entry without candidate")?;
                    let Some(candidate) =
                        git::merge_commit(&self.mirror, &base, &e.head_sha, &msg).await?
                    else {
                        continue; // Conflicts with the lane; wait for it to land.
                    };
                    let mut tasks = (*set).clone();
                    tasks.extend(mine.iter().cloned());
                    (
                        (*lane).clone(),
                        prev.position.unwrap_or(0) + 1,
                        base,
                        candidate,
                        tasks,
                    )
                }
                _ => continue, // Overlaps several lanes: wait for them to drain.
            };
            // Candidates must be fetchable by workers, which clone from origin.
            let candidate_ref = format!("refs/pit/queue/{}", e.id);
            if !git::push(&self.mirror, &candidate, &candidate_ref, true).await? {
                bail!(
                    "could not push candidate {candidate_ref} to {}",
                    self.repo.url
                );
            }
            tasks.retain(|t| !t.is_empty());
            let mut probe = e.clone();
            probe.candidate_sha = Some(candidate.clone());
            let run_id = self.queue_run(&probe, tasks).await?;
            // Batching: the change before this one in the lane, if no worker
            // has started on it, is tested by this candidate (which contains
            // it) instead of on its own. If this candidate fails, it gets its
            // own run again (see `collect_results`).
            if position > 0 {
                let prev = self.entries.iter().position(|o| {
                    o.lane.as_deref() == Some(lane.as_str())
                        && o.position == Some(position - 1)
                        && o.status == "testing"
                });
                if let Some(p) = prev {
                    if let Some(r) = self.entries[p].run_id.clone() {
                        if self.app.db.run_untouched(&r)? {
                            // Saved first: a crash before the skip must not
                            // leave a skipped run on an entry that counts it.
                            let o = &mut self.entries[p];
                            o.run_id = None;
                            o.reason = Some(format!(
                                "batched: tested together with later changes in {run_id}"
                            ));
                            let o = o.clone();
                            self.save(&o)?;
                            if self
                                .app
                                .db
                                .skip_run_because(&r, &format!("batched into {run_id}"))?
                            {
                                self.app.run_finished(&r).await;
                            }
                        }
                    }
                }
            }
            let entry = &mut self.entries[i];
            entry.status = "testing".into();
            entry.lane = Some(lane);
            entry.position = Some(position);
            entry.base_sha = Some(base);
            entry.candidate_sha = Some(candidate);
            entry.run_id = Some(run_id);
            entry.reason = None;
            let entry = entry.clone();
            self.save(&entry)?;
        }
        Ok(())
    }
}

impl Queue<'_> {
    /// Starts a merge queue run of an entry's candidate.
    async fn queue_run(&self, e: &QueueRow, tasks: BTreeSet<String>) -> Result<String> {
        self.app
            .create_run(
                CreateRun {
                    repo: self.repo.name.clone(),
                    url: None,
                    sha: e
                        .candidate_sha
                        .clone()
                        .context("entry without a candidate")?,
                    overlay: None,
                    tasks: tasks.into_iter().collect(),
                    trigger: Some("queue".into()),
                    git_ref: Some(format!("queue/{}", e.source)),
                    no_cache: false,
                    fresh: false,
                    debug: false,
                    counts_as: None,
                    no_secrets: false,
                    // Queueing is the decision to land it on the target.
                    trusted: super::trusted_branch(
                        self.target,
                        default_branch(&self.mirror).await.ok().as_deref(),
                    ),
                    draft: false,
                },
                e.pr,
            )
            .await
    }

    /// The tasks a lane has affected up to and including this entry.
    fn lane_tasks(&self, e: &QueueRow) -> BTreeSet<String> {
        self.entries
            .iter()
            .filter(|o| o.lane == e.lane && o.position <= e.position)
            .flat_map(|o| o.affected.iter().cloned())
            .filter(|t| !t.is_empty())
            .collect()
    }
}

fn merge_message(e: &QueueRow) -> String {
    match e.pr {
        Some(n) => format!("Merge pull request #{n} via Pit merge queue"),
        None => format!("Merge {} via Pit merge queue", e.source),
    }
}
