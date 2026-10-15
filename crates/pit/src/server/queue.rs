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
    let repos: BTreeMap<String, String> = app.db.repos()?.into_iter().map(|r| (r.id, r.name)).collect();
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

pub async fn add(app: &Arc<App>, req: QueueAdd) -> Result<QueueEntryView> {
    let repo = app.db.upsert_repo(&req.repo, req.url.as_deref(), None)?;
    let mirror = app.fetch_mirror(&repo).await?;
    let source = match (&req.branch, req.pr) {
        (Some(b), None) => b.clone(),
        (None, Some(n)) => format!("pr/{n}"),
        _ => bail!("queue a branch or a pull request, not both"),
    };
    let head = git::rev_parse(&mirror, &source_ref(&source))
        .await?
        .with_context(|| format!("{source} not found in {}", repo.url))?;
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
    let id = app.db.queue_insert(&repo.id, &source, req.pr, &head, &target)?;
    app.emit("queue.updated", None, None, None, serde_json::json!({ "id": id }));
    tick(app).await?;
    let row = app
        .db
        .queue_all(500)?
        .into_iter()
        .find(|r| r.id == id)
        .context("queue entry vanished")?;
    Ok(view(&row, repo.name))
}

async fn default_branch(mirror: &Path) -> Result<String> {
    for b in ["main", "master", "trunk"] {
        if git::rev_parse(mirror, &format!("refs/heads/{b}")).await?.is_some() {
            return Ok(b.to_string());
        }
    }
    bail!("could not find a default branch; pass --target")
}

/// Advances every queue: collects results, lands, and starts new candidates.
pub async fn tick(app: &Arc<App>) -> Result<()> {
    let _guard = app.queue_lock.lock().await;
    let active = app.db.queue_active()?;
    let mut by_target: BTreeMap<(String, String), Vec<QueueRow>> = BTreeMap::new();
    for e in active {
        by_target.entry((e.repo_id.clone(), e.target.clone())).or_default().push(e);
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
}

async fn tick_target(app: &Arc<App>, repo: &Repo, target: &str, entries: Vec<QueueRow>) -> Result<()> {
    let mirror = app.fetch_mirror(repo).await?;
    if let Some(gh) = repo.github.as_deref().and(app.github.as_ref()) {
        let url = gh.push_url(repo.github.as_deref().unwrap()).await?;
        git::git(&mirror, &["config", "remote.origin.pushurl", &url]).await?;
    }
    let tip = git::rev_parse(&mirror, &format!("refs/heads/{target}"))
        .await?
        .with_context(|| format!("target branch {target} not found"))?;
    let mut q = Queue { app, repo, mirror, target, tip, entries };
    q.collect_results().await?;
    q.land().await?;
    q.start_candidates().await?;
    Ok(())
}

impl Queue<'_> {
    fn save(&self, e: &QueueRow) -> Result<()> {
        self.app.db.queue_update(e)?;
        self.app.emit("queue.updated", None, None, None, serde_json::json!({ "id": e.id, "status": e.status }));
        Ok(())
    }

    /// Puts an entry back in line (dropping its candidate) and cancels its run.
    fn requeue(&mut self, idx: usize, reason: &str) -> Result<()> {
        let e = &mut self.entries[idx];
        if let Some(run) = e.run_id.take() {
            let _ = self.app.db.cancel_run(&run);
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
            .filter(|(_, e)| e.lane.as_deref() == Some(lane) && e.position.unwrap_or(0) > pos && e.status != "landed")
            .map(|(i, _)| i)
            .collect();
        for i in idxs {
            self.requeue(i, reason)?;
        }
        Ok(())
    }

    async fn collect_results(&mut self) -> Result<()> {
        // Entries whose branch moved since they were queued are retested.
        for i in 0..self.entries.len() {
            let source = self.entries[i].source.clone();
            let Some(head) = git::rev_parse(&self.mirror, &source_ref(&source)).await? else { continue };
            if head != self.entries[i].head_sha && self.entries[i].status != "landed" {
                let (lane, pos) = (self.entries[i].lane.clone(), self.entries[i].position.unwrap_or(0));
                self.entries[i].head_sha = head;
                self.requeue(i, "branch updated, retesting")?;
                if let Some(lane) = lane {
                    self.requeue_after(&lane, pos, "an earlier change in this lane was updated")?;
                }
            }
        }
        let mut order: Vec<usize> = (0..self.entries.len()).collect();
        order.sort_by_key(|&i| (self.entries[i].lane.clone(), self.entries[i].position));
        for i in order {
            if self.entries[i].status != "testing" {
                continue;
            }
            let Some(run_id) = self.entries[i].run_id.clone() else { continue };
            let run = self.app.db.run(&run_id)?;
            if !run.status.is_done() {
                continue;
            }
            if run.status.is_ok() {
                self.entries[i].status = "passed".into();
                self.entries[i].reason = None;
                let e = self.entries[i].clone();
                self.save(&e)?;
            } else {
                let e = &mut self.entries[i];
                e.status = "failed".into();
                e.reason = Some(format!("tests failed in run {run_id}"));
                let (lane, pos) = (e.lane.clone().unwrap_or_default(), e.position.unwrap_or(0));
                let e = e.clone();
                self.save(&e)?;
                self.requeue_after(&lane, pos, "an earlier change in this lane failed; retesting without it")?;
            }
        }
        Ok(())
    }

    async fn land(&mut self) -> Result<()> {
        let lanes: BTreeSet<String> = self.entries.iter().filter_map(|e| e.lane.clone()).collect();
        for lane in lanes {
            loop {
                // The first entry of the lane that has not landed yet.
                let next = self
                    .entries
                    .iter()
                    .enumerate()
                    .filter(|(_, e)| e.lane.as_deref() == Some(lane.as_str()) && matches!(e.status.as_str(), "testing" | "passed"))
                    .min_by_key(|(_, e)| e.position)
                    .map(|(i, _)| i);
                let Some(i) = next else { break };
                if self.entries[i].status != "passed" {
                    break;
                }
                if !self.land_one(i, "tested").await? {
                    break;
                }
            }
        }
        Ok(())
    }

    /// Pushes an entry to the target. Returns false if it could not land now.
    async fn land_one(&mut self, i: usize, how: &str) -> Result<bool> {
        let e = self.entries[i].clone();
        let candidate = e.candidate_sha.clone().context("passed entry without a candidate")?;
        let commit = if git::is_ancestor(&self.mirror, &self.tip, &candidate).await {
            candidate
        } else {
            // Another lane landed first. Its tasks are disjoint from this one,
            // so re-merging onto the new tip keeps this lane's result valid.
            let msg = merge_message(&e);
            match git::merge_commit(&self.mirror, &self.tip, &e.head_sha, &msg).await? {
                Some(c) => c,
                None => {
                    let lane = e.lane.clone().unwrap_or_default();
                    self.requeue(i, "conflicts with a change that landed first; retesting")?;
                    self.requeue_after(&lane, e.position.unwrap_or(0), "an earlier change in this lane is being retested")?;
                    return Ok(false);
                }
            }
        };
        if !git::push(&self.mirror, &commit, &format!("refs/heads/{}", self.target), false).await? {
            return Ok(false);
        }
        self.tip = commit.clone();
        let e = &mut self.entries[i];
        e.status = "landed".into();
        e.landed_sha = Some(commit.clone());
        e.reason = Some(how.to_string());
        let e = e.clone();
        self.save(&e)?;
        self.app.emit("queue.landed", None, None, None, serde_json::json!({ "id": e.id, "sha": commit }));
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
                },
                None,
            )
            .await?;
        Ok(true)
    }

    async fn start_candidates(&mut self) -> Result<()> {
        let mut waiting: Vec<usize> = (0..self.entries.len()).filter(|&i| self.entries[i].status == "queued").collect();
        waiting.sort_by_key(|&i| self.entries[i].created_at);
        for i in waiting {
            let e = self.entries[i].clone();
            let msg = merge_message(&e);
            let Some(merged) = git::merge_commit(&self.mirror, &self.tip, &e.head_sha, &msg).await? else {
                let e = &mut self.entries[i];
                e.status = "failed".into();
                e.reason = Some(format!("conflicts with {}; rebase and queue again", self.target));
                let e = e.clone();
                self.save(&e)?;
                continue;
            };
            let changed = git::changed_files(&self.mirror, &self.tip, &merged).await?;
            let config = match self.app.load_config(&self.mirror, &merged, None).await {
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
            let affected: Vec<String> = crate::graph::affected(&config, &changed)?.into_iter().collect();
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
                let (Some(lane), true) = (&other.lane, matches!(other.status.as_str(), "testing" | "passed")) else { continue };
                let slot = lanes.entry(lane.clone()).or_default();
                slot.0.extend(other.affected.iter().cloned());
                if slot.1.is_none_or(|k| self.entries[k].position < other.position) {
                    slot.1 = Some(j);
                }
            }
            let mine: BTreeSet<String> = affected.iter().cloned().collect();
            let overlapping: Vec<(&String, &(BTreeSet<String>, Option<usize>))> =
                lanes.iter().filter(|(_, (set, _))| !set.is_disjoint(&mine)).collect();

            let (lane, position, base, candidate, mut tasks) = match overlapping.as_slice() {
                [] => (e.id.clone(), 0, self.tip.clone(), merged, mine.clone()),
                [(lane, (set, Some(last)))] => {
                    let prev = &self.entries[*last];
                    let base = prev.candidate_sha.clone().context("lane entry without candidate")?;
                    let Some(candidate) = git::merge_commit(&self.mirror, &base, &e.head_sha, &msg).await? else {
                        continue; // Conflicts with the lane; wait for it to land.
                    };
                    let mut tasks = (*set).clone();
                    tasks.extend(mine.iter().cloned());
                    ((*lane).clone(), prev.position.unwrap_or(0) + 1, base, candidate, tasks)
                }
                _ => continue, // Overlaps several lanes: wait for them to drain.
            };
            // Candidates must be fetchable by workers, which clone from origin.
            let candidate_ref = format!("refs/pit/queue/{}", e.id);
            if !git::push(&self.mirror, &candidate, &candidate_ref, true).await? {
                bail!("could not push candidate {candidate_ref} to {}", self.repo.url);
            }
            tasks.retain(|t| !t.is_empty());
            let run_id = self
                .app
                .create_run(
                    CreateRun {
                        repo: self.repo.name.clone(),
                        url: None,
                        sha: candidate.clone(),
                        overlay: None,
                        tasks: tasks.into_iter().collect(),
                        trigger: Some("queue".into()),
                        git_ref: Some(format!("queue/{}", e.source)),
                        no_cache: false,
                    },
                    e.pr,
                )
                .await?;
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

fn merge_message(e: &QueueRow) -> String {
    match e.pr {
        Some(n) => format!("Merge pull request #{n} via Pit merge queue"),
        None => format!("Merge {} via Pit merge queue", e.source),
    }
}
