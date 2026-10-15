//! SQLite storage for the control plane. One connection behind a mutex: every
//! statement is a few microseconds, and WAL keeps readers out of the way.

use std::path::Path;

use anyhow::{Context, Result};
use parking_lot::Mutex;
use rusqlite::{params, Connection, OptionalExtension, Row};

use crate::model::*;

pub struct Db {
    conn: Mutex<Connection>,
}

const SCHEMA: &str = r#"
PRAGMA journal_mode = WAL;
PRAGMA synchronous = NORMAL;
PRAGMA foreign_keys = ON;

CREATE TABLE IF NOT EXISTS repos (
  id TEXT PRIMARY KEY,
  org_id TEXT NOT NULL DEFAULT 'local',
  name TEXT NOT NULL UNIQUE,
  url TEXT NOT NULL,
  github TEXT,
  created_at INTEGER NOT NULL
);

CREATE TABLE IF NOT EXISTS runs (
  id TEXT PRIMARY KEY,
  org_id TEXT NOT NULL DEFAULT 'local',
  repo_id TEXT NOT NULL REFERENCES repos(id),
  sha TEXT NOT NULL,
  ref TEXT,
  overlay TEXT,
  overlay_files INTEGER NOT NULL DEFAULT 0,
  trigger TEXT NOT NULL,
  status TEXT NOT NULL,
  no_cache INTEGER NOT NULL DEFAULT 0,
  message TEXT,
  github_pr INTEGER,
  created_at INTEGER NOT NULL,
  started_at INTEGER,
  finished_at INTEGER
);
CREATE INDEX IF NOT EXISTS runs_created ON runs(created_at DESC);

CREATE TABLE IF NOT EXISTS groups_ (
  id TEXT PRIMARY KEY,
  run_id TEXT NOT NULL REFERENCES runs(id),
  name TEXT NOT NULL,
  os TEXT NOT NULL,
  backends TEXT NOT NULL,
  status TEXT NOT NULL,
  worker TEXT,
  prepare_ms INTEGER,
  message TEXT,
  check_run_id INTEGER,
  lease TEXT,
  heartbeat_at INTEGER,
  created_at INTEGER NOT NULL,
  started_at INTEGER,
  finished_at INTEGER
);
CREATE INDEX IF NOT EXISTS groups_status ON groups_(status, created_at);
CREATE INDEX IF NOT EXISTS groups_run ON groups_(run_id);

CREATE TABLE IF NOT EXISTS tasks (
  id TEXT PRIMARY KEY,
  group_id TEXT NOT NULL REFERENCES groups_(id),
  run_id TEXT NOT NULL,
  name TEXT NOT NULL,
  position INTEGER NOT NULL,
  def TEXT NOT NULL,
  status TEXT NOT NULL,
  action_key TEXT,
  exit_code INTEGER,
  duration_ms INTEGER,
  log_digest TEXT,
  message TEXT,
  started_at INTEGER,
  finished_at INTEGER
);
CREATE INDEX IF NOT EXISTS tasks_group ON tasks(group_id, position);
CREATE INDEX IF NOT EXISTS tasks_run ON tasks(run_id);

CREATE TABLE IF NOT EXISTS task_logs (
  task_id TEXT NOT NULL,
  seq INTEGER NOT NULL,
  text TEXT NOT NULL,
  PRIMARY KEY (task_id, seq)
);

CREATE TABLE IF NOT EXISTS cache (
  action_key TEXT PRIMARY KEY,
  entry TEXT NOT NULL,
  hits INTEGER NOT NULL DEFAULT 0,
  created_at INTEGER NOT NULL,
  last_hit_at INTEGER
);

-- Key-value index into the CAS for the Turborepo and Bazel cache protocols.
CREATE TABLE IF NOT EXISTS kv (
  ns TEXT NOT NULL,
  key TEXT NOT NULL,
  digest TEXT NOT NULL,
  created_at INTEGER NOT NULL,
  last_used_at INTEGER,
  PRIMARY KEY (ns, key)
);

CREATE TABLE IF NOT EXISTS workers (
  name TEXT PRIMARY KEY,
  info TEXT NOT NULL,
  last_seen INTEGER NOT NULL
);

CREATE TABLE IF NOT EXISTS queue (
  id TEXT PRIMARY KEY,
  repo_id TEXT NOT NULL REFERENCES repos(id),
  source TEXT NOT NULL,
  pr INTEGER,
  head_sha TEXT NOT NULL,
  target TEXT NOT NULL,
  status TEXT NOT NULL,
  lane TEXT,
  position INTEGER,
  affected TEXT NOT NULL DEFAULT '[]',
  base_sha TEXT,
  candidate_sha TEXT,
  run_id TEXT,
  landed_sha TEXT,
  reason TEXT,
  created_at INTEGER NOT NULL,
  updated_at INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS queue_status ON queue(repo_id, target, status);
"#;

#[derive(Debug, Clone)]
pub struct Repo {
    pub id: String,
    pub name: String,
    pub url: String,
    pub github: Option<String>,
}

#[derive(Debug, Clone)]
pub struct RunRow {
    pub id: String,
    pub repo_id: String,
    pub sha: String,
    pub git_ref: Option<String>,
    pub overlay: Option<String>,
    pub trigger: String,
    pub status: Status,
    pub no_cache: bool,
    pub github_pr: Option<u64>,
}

#[derive(Debug, Clone)]
pub struct GroupRow {
    pub id: String,
    pub run_id: String,
    pub name: String,
    pub os: String,
    pub status: Status,
    pub check_run_id: Option<u64>,
}

#[derive(Debug, Clone)]
pub struct QueueRow {
    pub id: String,
    pub repo_id: String,
    pub source: String,
    pub pr: Option<u64>,
    pub head_sha: String,
    pub target: String,
    pub status: String,
    pub lane: Option<String>,
    pub position: Option<i64>,
    pub affected: Vec<String>,
    pub base_sha: Option<String>,
    pub candidate_sha: Option<String>,
    pub run_id: Option<String>,
    pub landed_sha: Option<String>,
    pub reason: Option<String>,
    pub created_at: i64,
    pub updated_at: i64,
}

/// Columns added after the first release of a table.
fn migrate(conn: &Connection) -> Result<()> {
    let has_lease: bool = conn
        .prepare("SELECT 1 FROM pragma_table_info('groups_') WHERE name = 'lease'")?
        .exists([])?;
    if !has_lease {
        conn.execute_batch("ALTER TABLE groups_ ADD COLUMN lease TEXT")?;
    }
    let has_kv_used: bool = conn
        .prepare("SELECT 1 FROM pragma_table_info('kv') WHERE name = 'last_used_at'")?
        .exists([])?;
    if !has_kv_used {
        conn.execute_batch("ALTER TABLE kv ADD COLUMN last_used_at INTEGER")?;
    }
    Ok(())
}

/// SQL condition: the task's group is running under the lease in `?4`.
const LEASED: &str =
    "EXISTS (SELECT 1 FROM groups_ WHERE groups_.id = tasks.group_id AND groups_.status = 'running' AND groups_.lease = ?4)";
/// The same, with the lease in `?9`.
const LEASED_9: &str =
    "EXISTS (SELECT 1 FROM groups_ WHERE groups_.id = tasks.group_id AND groups_.status = 'running' AND groups_.lease = ?9)";

fn queue_row(r: &Row) -> rusqlite::Result<QueueRow> {
    let affected: String = r.get("affected")?;
    Ok(QueueRow {
        id: r.get("id")?,
        repo_id: r.get("repo_id")?,
        source: r.get("source")?,
        pr: r.get::<_, Option<i64>>("pr")?.map(|v| v as u64),
        head_sha: r.get("head_sha")?,
        target: r.get("target")?,
        status: r.get("status")?,
        lane: r.get("lane")?,
        position: r.get("position")?,
        affected: serde_json::from_str(&affected).unwrap_or_default(),
        base_sha: r.get("base_sha")?,
        candidate_sha: r.get("candidate_sha")?,
        run_id: r.get("run_id")?,
        landed_sha: r.get("landed_sha")?,
        reason: r.get("reason")?,
        created_at: r.get("created_at")?,
        updated_at: r.get("updated_at")?,
    })
}

fn run_row(r: &Row) -> rusqlite::Result<RunRow> {
    Ok(RunRow {
        id: r.get("id")?,
        repo_id: r.get("repo_id")?,
        sha: r.get("sha")?,
        git_ref: r.get("ref")?,
        overlay: r.get("overlay")?,
        trigger: r.get("trigger")?,
        status: Status::parse(&r.get::<_, String>("status")?),
        no_cache: r.get::<_, i64>("no_cache")? != 0,
        github_pr: r.get::<_, Option<i64>>("github_pr")?.map(|v| v as u64),
    })
}

fn opt_u64(v: Option<i64>) -> Option<u64> {
    v.map(|v| v as u64)
}

impl Db {
    pub fn open(path: &Path) -> Result<Db> {
        let conn = Connection::open(path).with_context(|| format!("opening {}", path.display()))?;
        conn.busy_timeout(std::time::Duration::from_secs(5))?;
        conn.execute_batch(SCHEMA)?;
        migrate(&conn)?;
        Ok(Db {
            conn: Mutex::new(conn),
        })
    }

    pub fn memory() -> Result<Db> {
        let conn = Connection::open_in_memory()?;
        conn.execute_batch(SCHEMA)?;
        migrate(&conn)?;
        Ok(Db {
            conn: Mutex::new(conn),
        })
    }

    // ---- repos ----

    pub fn upsert_repo(&self, name: &str, url: Option<&str>, github: Option<&str>) -> Result<Repo> {
        let c = self.conn.lock();
        let existing = c
            .query_row(
                "SELECT id, name, url, github FROM repos WHERE name = ?1",
                params![name],
                |r| {
                    Ok(Repo {
                        id: r.get(0)?,
                        name: r.get(1)?,
                        url: r.get(2)?,
                        github: r.get(3)?,
                    })
                },
            )
            .optional()?;
        if let Some(mut repo) = existing {
            if let Some(url) = url.filter(|u| *u != repo.url) {
                c.execute(
                    "UPDATE repos SET url = ?1 WHERE id = ?2",
                    params![url, repo.id],
                )?;
                repo.url = url.to_string();
            }
            if let Some(gh) = github.filter(|g| repo.github.as_deref() != Some(*g)) {
                c.execute(
                    "UPDATE repos SET github = ?1 WHERE id = ?2",
                    params![gh, repo.id],
                )?;
                repo.github = Some(gh.to_string());
            }
            return Ok(repo);
        }
        let url =
            url.with_context(|| format!("unknown repo {name:?}; pass its url to register it"))?;
        let id = new_id("repo");
        c.execute(
            "INSERT INTO repos (id, name, url, github, created_at) VALUES (?1, ?2, ?3, ?4, ?5)",
            params![id, name, url, github, now_ms()],
        )?;
        Ok(Repo {
            id,
            name: name.into(),
            url: url.into(),
            github: github.map(String::from),
        })
    }

    pub fn repo(&self, id: &str) -> Result<Repo> {
        let c = self.conn.lock();
        c.query_row(
            "SELECT id, name, url, github FROM repos WHERE id = ?1 OR name = ?1",
            params![id],
            |r| {
                Ok(Repo {
                    id: r.get(0)?,
                    name: r.get(1)?,
                    url: r.get(2)?,
                    github: r.get(3)?,
                })
            },
        )
        .optional()?
        .with_context(|| format!("unknown repo {id:?}"))
    }

    pub fn repo_by_github(&self, full_name: &str) -> Result<Option<Repo>> {
        let c = self.conn.lock();
        Ok(c.query_row(
            "SELECT id, name, url, github FROM repos WHERE github = ?1",
            params![full_name],
            |r| {
                Ok(Repo {
                    id: r.get(0)?,
                    name: r.get(1)?,
                    url: r.get(2)?,
                    github: r.get(3)?,
                })
            },
        )
        .optional()?)
    }

    pub fn repos(&self) -> Result<Vec<Repo>> {
        let c = self.conn.lock();
        let mut st = c.prepare("SELECT id, name, url, github FROM repos ORDER BY name")?;
        let rows = st
            .query_map([], |r| {
                Ok(Repo {
                    id: r.get(0)?,
                    name: r.get(1)?,
                    url: r.get(2)?,
                    github: r.get(3)?,
                })
            })?
            .collect::<rusqlite::Result<_>>()?;
        Ok(rows)
    }

    // ---- runs ----

    #[allow(clippy::too_many_arguments)]
    pub fn insert_run(
        &self,
        repo_id: &str,
        sha: &str,
        git_ref: Option<&str>,
        overlay: Option<&str>,
        overlay_files: usize,
        trigger: &str,
        no_cache: bool,
        github_pr: Option<u64>,
    ) -> Result<String> {
        let id = new_id("run");
        self.conn.lock().execute(
            "INSERT INTO runs (id, repo_id, sha, ref, overlay, overlay_files, trigger, status, no_cache, github_pr, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, 'queued', ?8, ?9, ?10)",
            params![id, repo_id, sha, git_ref, overlay, overlay_files as i64, trigger, no_cache as i64, github_pr.map(|v| v as i64), now_ms()],
        )?;
        Ok(id)
    }

    pub fn run(&self, id: &str) -> Result<RunRow> {
        let c = self.conn.lock();
        c.query_row("SELECT * FROM runs WHERE id = ?1", params![id], run_row)
            .optional()?
            .with_context(|| format!("unknown run {id}"))
    }

    pub fn set_run_status(&self, id: &str, status: Status, message: Option<&str>) -> Result<()> {
        let c = self.conn.lock();
        let now = now_ms();
        c.execute(
            "UPDATE runs SET status = ?1,
               message = COALESCE(?2, message),
               started_at = CASE WHEN ?1 = 'running' AND started_at IS NULL THEN ?3 ELSE started_at END,
               finished_at = CASE WHEN ?1 NOT IN ('queued', 'running') THEN ?3 ELSE finished_at END
             WHERE id = ?4",
            params![status.as_str(), message, now, id],
        )?;
        Ok(())
    }

    pub fn list_runs(&self, limit: usize, repo: Option<&str>) -> Result<Vec<String>> {
        let c = self.conn.lock();
        let mut st = c.prepare(
            "SELECT runs.id FROM runs JOIN repos ON repos.id = runs.repo_id
             WHERE (?1 IS NULL OR repos.name = ?1 OR repos.id = ?1)
             ORDER BY runs.created_at DESC LIMIT ?2",
        )?;
        let ids = st
            .query_map(params![repo, limit as i64], |r| r.get(0))?
            .collect::<rusqlite::Result<_>>()?;
        Ok(ids)
    }

    pub fn run_view(&self, id: &str) -> Result<RunView> {
        let c = self.conn.lock();
        let mut run = c
            .query_row(
                "SELECT runs.*, repos.name AS repo_name FROM runs JOIN repos ON repos.id = runs.repo_id WHERE runs.id = ?1",
                params![id],
                |r| {
                    Ok(RunView {
                        id: r.get("id")?,
                        repo: r.get("repo_name")?,
                        sha: r.get("sha")?,
                        git_ref: r.get("ref")?,
                        trigger: r.get("trigger")?,
                        status: Status::parse(&r.get::<_, String>("status")?),
                        overlay_files: r.get::<_, i64>("overlay_files")? as usize,
                        message: r.get("message")?,
                        created_at: r.get("created_at")?,
                        started_at: r.get("started_at")?,
                        finished_at: r.get("finished_at")?,
                        groups: vec![],
                    })
                },
            )
            .optional()?
            .with_context(|| format!("unknown run {id}"))?;
        let mut gst = c.prepare("SELECT * FROM groups_ WHERE run_id = ?1 ORDER BY name")?;
        let mut groups: Vec<GroupView> = gst
            .query_map(params![id], |r| {
                Ok(GroupView {
                    id: r.get("id")?,
                    name: r.get("name")?,
                    os: r.get("os")?,
                    status: Status::parse(&r.get::<_, String>("status")?),
                    worker: r.get("worker")?,
                    prepare_ms: opt_u64(r.get("prepare_ms")?),
                    message: r.get("message")?,
                    started_at: r.get("started_at")?,
                    finished_at: r.get("finished_at")?,
                    tasks: vec![],
                })
            })?
            .collect::<rusqlite::Result<_>>()?;
        let mut tst = c.prepare("SELECT * FROM tasks WHERE group_id = ?1 ORDER BY position")?;
        for g in groups.iter_mut() {
            g.tasks = tst
                .query_map(params![g.id], |r| {
                    let def: String = r.get("def")?;
                    let def: TaskDef = serde_json::from_str(&def).unwrap_or_default();
                    Ok(TaskView {
                        id: r.get("id")?,
                        name: r.get("name")?,
                        deps: def.deps,
                        status: Status::parse(&r.get::<_, String>("status")?),
                        action_key: r.get("action_key")?,
                        exit_code: r.get("exit_code")?,
                        duration_ms: opt_u64(r.get("duration_ms")?),
                        log_digest: r.get("log_digest")?,
                        message: r.get("message")?,
                        started_at: r.get("started_at")?,
                        finished_at: r.get("finished_at")?,
                    })
                })?
                .collect::<rusqlite::Result<_>>()?;
        }
        run.groups = groups;
        Ok(run)
    }

    // ---- groups and tasks ----

    pub fn insert_group(
        &self,
        run_id: &str,
        name: &str,
        os: &str,
        backends: &[Backend],
        tasks: &[(String, TaskDef)],
    ) -> Result<String> {
        let mut c = self.conn.lock();
        let tx = c.transaction()?;
        let gid = new_id("grp");
        let now = now_ms();
        tx.execute(
            "INSERT INTO groups_ (id, run_id, name, os, backends, status, created_at) VALUES (?1, ?2, ?3, ?4, ?5, 'queued', ?6)",
            params![gid, run_id, name, os, serde_json::to_string(backends)?, now],
        )?;
        for (pos, (tname, def)) in tasks.iter().enumerate() {
            tx.execute(
                "INSERT INTO tasks (id, group_id, run_id, name, position, def, status) VALUES (?1, ?2, ?3, ?4, ?5, ?6, 'queued')",
                params![new_id("task"), gid, run_id, tname, pos as i64, serde_json::to_string(def)?],
            )?;
        }
        tx.commit()?;
        Ok(gid)
    }

    /// Atomically claims the oldest queued group this worker can run.
    pub fn claim_group(&self, worker: &WorkerInfo) -> Result<Option<Job>> {
        let mut c = self.conn.lock();
        let tx = c.transaction()?;
        // OS is filtered in SQL; backends (JSON) are checked row by row. The
        // scan is lazy and unbounded, so incompatible groups at the head of
        // the queue cannot hide a compatible one behind them.
        let pick = {
            let mut st = tx.prepare(
                "SELECT groups_.id, groups_.run_id, groups_.os, groups_.backends FROM groups_
                 JOIN runs ON runs.id = groups_.run_id
                 WHERE groups_.status = 'queued' AND runs.status IN ('queued', 'running')
                   AND (groups_.os = 'any' OR groups_.os = ?1)
                 ORDER BY groups_.created_at",
            )?;
            let mut rows = st.query_map(params![worker.os], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, String>(2)?,
                    r.get::<_, String>(3)?,
                ))
            })?;
            let mut pick = None;
            for row in rows.by_ref() {
                let row = row?;
                let needs: Vec<Backend> = serde_json::from_str(&row.3).unwrap_or_default();
                if needs.iter().all(|b| worker.backends.contains(b)) {
                    pick = Some(row);
                    break;
                }
            }
            pick
        };
        let Some((gid, run_id, os, _)) = pick else {
            return Ok(None);
        };
        let now = now_ms();
        let lease = new_id("lease");
        tx.execute(
            "UPDATE groups_ SET status = 'running', worker = ?1, started_at = ?2, heartbeat_at = ?2, lease = ?4 WHERE id = ?3",
            params![worker.name, now, gid, lease],
        )?;
        let (run, repo_name, url, group_name): (RunRow, String, String, String) = tx.query_row(
            "SELECT runs.*, repos.name AS repo_name, repos.url AS repo_url, groups_.name AS group_name
             FROM groups_ JOIN runs ON runs.id = groups_.run_id JOIN repos ON repos.id = runs.repo_id
             WHERE groups_.id = ?1",
            params![gid],
            |r| Ok((run_row(r)?, r.get("repo_name")?, r.get("repo_url")?, r.get("group_name")?)),
        )?;
        let tasks = {
            let mut st = tx
                .prepare("SELECT id, name, def FROM tasks WHERE group_id = ?1 ORDER BY position")?;
            let rows = st
                .query_map(params![gid], |r| {
                    let def: String = r.get(2)?;
                    Ok(JobTask {
                        id: r.get(0)?,
                        name: r.get(1)?,
                        def: serde_json::from_str(&def).unwrap_or_default(),
                    })
                })?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            rows
        };
        tx.commit()?;
        Ok(Some(Job {
            group_id: gid,
            lease,
            run_id,
            repo: repo_name,
            url,
            sha: run.sha,
            overlay: run.overlay,
            group: group_name,
            os: if os == "any" { worker.os.clone() } else { os },
            no_cache: run.no_cache,
            flake_retries: u32::from(run.trigger == "queue"),
            tasks,
        }))
    }

    pub fn group(&self, id: &str) -> Result<GroupRow> {
        let c = self.conn.lock();
        c.query_row(
            "SELECT id, run_id, name, os, status, check_run_id FROM groups_ WHERE id = ?1",
            params![id],
            |r| {
                Ok(GroupRow {
                    id: r.get(0)?,
                    run_id: r.get(1)?,
                    name: r.get(2)?,
                    os: r.get(3)?,
                    status: Status::parse(&r.get::<_, String>(4)?),
                    check_run_id: opt_u64(r.get(5)?),
                })
            },
        )
        .optional()?
        .with_context(|| format!("unknown group {id}"))
    }

    pub fn groups_for_run(&self, run_id: &str) -> Result<Vec<GroupRow>> {
        let c = self.conn.lock();
        let mut st = c.prepare(
            "SELECT id, run_id, name, os, status, check_run_id FROM groups_ WHERE run_id = ?1",
        )?;
        let rows = st
            .query_map(params![run_id], |r| {
                Ok(GroupRow {
                    id: r.get(0)?,
                    run_id: r.get(1)?,
                    name: r.get(2)?,
                    os: r.get(3)?,
                    status: Status::parse(&r.get::<_, String>(4)?),
                    check_run_id: opt_u64(r.get(5)?),
                })
            })?
            .collect::<rusqlite::Result<_>>()?;
        Ok(rows)
    }

    pub fn set_check_run(&self, group_id: &str, check_run_id: u64) -> Result<()> {
        self.conn.lock().execute(
            "UPDATE groups_ SET check_run_id = ?1 WHERE id = ?2",
            params![check_run_id as i64, group_id],
        )?;
        Ok(())
    }

    /// Records a group's result if `lease` is still its current claim.
    /// Returns false (and changes nothing) for a stale claim.
    pub fn finish_group(
        &self,
        id: &str,
        lease: Option<&str>,
        result: &GroupResult,
    ) -> Result<bool> {
        let c = self.conn.lock();
        let now = now_ms();
        let changed = c.execute(
            "UPDATE groups_ SET status = ?1, message = ?2, prepare_ms = ?3, finished_at = ?4
             WHERE id = ?5 AND status = 'running' AND lease = ?6",
            params![
                result.status.as_str(),
                result.message,
                result.prepare_ms as i64,
                now,
                id,
                lease
            ],
        )?;
        if changed == 0 {
            return Ok(false);
        }
        // Anything the worker never reported on is skipped.
        c.execute(
            "UPDATE tasks SET status = 'skipped', finished_at = ?1 WHERE group_id = ?2 AND status IN ('queued', 'running')",
            params![now, id],
        )?;
        Ok(true)
    }
    /// Records a heartbeat if `lease` is still the group's current claim.
    pub fn heartbeat_group(&self, id: &str, lease: Option<&str>) -> Result<bool> {
        Ok(self.conn.lock().execute(
            "UPDATE groups_ SET heartbeat_at = ?1 WHERE id = ?2 AND status = 'running' AND lease = ?3",
            params![now_ms(), id, lease],
        )? > 0)
    }

    /// Requeues running groups whose worker stopped heartbeating.
    pub fn requeue_stale_groups(&self, older_than_ms: i64) -> Result<Vec<String>> {
        let c = self.conn.lock();
        let cutoff = now_ms() - older_than_ms;
        let mut st =
            c.prepare("SELECT id FROM groups_ WHERE status = 'running' AND heartbeat_at < ?1")?;
        let ids: Vec<String> = st
            .query_map(params![cutoff], |r| r.get(0))?
            .collect::<rusqlite::Result<_>>()?;
        for id in &ids {
            c.execute(
                "UPDATE groups_ SET status = 'queued', worker = NULL, started_at = NULL, lease = NULL WHERE id = ?1",
                params![id],
            )?;
            c.execute(
                "UPDATE tasks SET status = 'queued', started_at = NULL WHERE group_id = ?1 AND status = 'running'",
                params![id],
            )?;
        }
        Ok(ids)
    }

    /// Cancels a run: it ends `failed` with message `cancelled` at once, and
    /// its unclaimed groups are skipped. Groups already running finish on
    /// their workers, but their results no longer change the run. Returns
    /// whether the run was still active.
    pub fn cancel_run(&self, run_id: &str) -> Result<bool> {
        let c = self.conn.lock();
        let now = now_ms();
        c.execute(
            "UPDATE groups_ SET status = 'skipped', finished_at = ?1 WHERE run_id = ?2 AND status = 'queued'",
            params![now, run_id],
        )?;
        c.execute(
            "UPDATE tasks SET status = 'skipped', finished_at = ?1 WHERE run_id = ?2 AND status = 'queued'
               AND group_id IN (SELECT id FROM groups_ WHERE run_id = ?2 AND status = 'skipped')",
            params![now, run_id],
        )?;
        let changed = c.execute(
            "UPDATE runs SET status = 'failed', message = 'cancelled', finished_at = ?1
             WHERE id = ?2 AND status IN ('queued', 'running')",
            params![now, run_id],
        )?;
        Ok(changed > 0)
    }

    pub fn task_group(&self, task_id: &str) -> Result<(String, String, String)> {
        let c = self.conn.lock();
        c.query_row(
            "SELECT group_id, run_id, name FROM tasks WHERE id = ?1",
            params![task_id],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .optional()?
        .with_context(|| format!("unknown task {task_id}"))
    }

    // Task writes are fenced by the group's lease in the same statement, so a
    // worker that lost its claim cannot write between a check and the write.
    // Each returns whether the write happened.

    pub fn start_task(
        &self,
        id: &str,
        lease: Option<&str>,
        action_key: Option<&str>,
    ) -> Result<bool> {
        Ok(self.conn.lock().execute(
            &format!("UPDATE tasks SET status = 'running', action_key = ?1, started_at = ?2 WHERE id = ?3 AND {LEASED}"),
            params![action_key, now_ms(), id, lease],
        )? > 0)
    }

    pub fn finish_task(&self, id: &str, lease: Option<&str>, r: &TaskResult) -> Result<bool> {
        Ok(self.conn.lock().execute(
            &format!(
                "UPDATE tasks SET status = ?1, exit_code = ?2, action_key = COALESCE(?3, action_key), duration_ms = ?4,
                   log_digest = ?5, message = ?6, finished_at = ?7,
                   started_at = COALESCE(started_at, ?7 - ?4)
                 WHERE id = ?8 AND {LEASED_9}"
            ),
            params![r.status.as_str(), r.exit_code, r.action_key, r.duration_ms as i64, r.log_digest, r.message, now_ms(), id, lease],
        )? > 0)
    }

    pub fn append_log(&self, task_id: &str, lease: Option<&str>, text: &str) -> Result<bool> {
        Ok(self.conn.lock().execute(
            "INSERT INTO task_logs (task_id, seq, text)
             SELECT ?1, (SELECT COALESCE(MAX(seq), 0) + 1 FROM task_logs WHERE task_id = ?1), ?2
             WHERE EXISTS (SELECT 1 FROM tasks JOIN groups_ ON groups_.id = tasks.group_id
                           WHERE tasks.id = ?1 AND groups_.status = 'running' AND groups_.lease = ?3)",
            params![task_id, text, lease],
        )? > 0)
    }

    pub fn live_log(&self, task_id: &str) -> Result<String> {
        let c = self.conn.lock();
        let mut st = c.prepare("SELECT text FROM task_logs WHERE task_id = ?1 ORDER BY seq")?;
        let parts: Vec<String> = st
            .query_map(params![task_id], |r| r.get(0))?
            .collect::<rusqlite::Result<_>>()?;
        Ok(parts.concat())
    }

    pub fn drop_live_log(&self, task_id: &str) -> Result<()> {
        self.conn
            .lock()
            .execute("DELETE FROM task_logs WHERE task_id = ?1", params![task_id])?;
        Ok(())
    }

    pub fn task_log_digest(&self, task_id: &str) -> Result<Option<String>> {
        let c = self.conn.lock();
        Ok(c.query_row(
            "SELECT log_digest FROM tasks WHERE id = ?1",
            params![task_id],
            |r| r.get(0),
        )
        .optional()?
        .flatten())
    }

    pub fn find_task(&self, run_id: &str, name: &str) -> Result<Option<String>> {
        let c = self.conn.lock();
        Ok(c.query_row(
            "SELECT tasks.id FROM tasks JOIN groups_ ON groups_.id = tasks.group_id
             WHERE tasks.run_id = ?1 AND (tasks.name = ?2 OR groups_.name || '.' || tasks.name = ?2)
             ORDER BY groups_.name LIMIT 1",
            params![run_id, name],
            |r| r.get(0),
        )
        .optional()?)
    }

    // ---- cache ----

    pub fn cache_get(&self, key: &str) -> Result<Option<CacheEntry>> {
        let c = self.conn.lock();
        let entry: Option<String> = c
            .query_row(
                "SELECT entry FROM cache WHERE action_key = ?1",
                params![key],
                |r| r.get(0),
            )
            .optional()?;
        if entry.is_some() {
            c.execute(
                "UPDATE cache SET hits = hits + 1, last_hit_at = ?1 WHERE action_key = ?2",
                params![now_ms(), key],
            )?;
        }
        Ok(entry.and_then(|e| serde_json::from_str(&e).ok()))
    }

    /// Reads a cache entry without counting it as a hit.
    pub fn cache_peek(&self, key: &str) -> Result<Option<CacheEntry>> {
        let c = self.conn.lock();
        let entry: Option<String> = c
            .query_row(
                "SELECT entry FROM cache WHERE action_key = ?1",
                params![key],
                |r| r.get(0),
            )
            .optional()?;
        Ok(entry.and_then(|e| serde_json::from_str(&e).ok()))
    }

    pub fn cache_put(&self, key: &str, entry: &CacheEntry) -> Result<()> {
        self.conn.lock().execute(
            "INSERT OR REPLACE INTO cache (action_key, entry, created_at) VALUES (?1, ?2, ?3)",
            params![key, serde_json::to_string(entry)?, now_ms()],
        )?;
        Ok(())
    }

    pub fn kv_get(&self, ns: &str, key: &str) -> Result<Option<String>> {
        let c = self.conn.lock();
        let digest: Option<String> = c
            .query_row(
                "SELECT digest FROM kv WHERE ns = ?1 AND key = ?2",
                params![ns, key],
                |r| r.get(0),
            )
            .optional()?;
        if digest.is_some() {
            // Least-recently-used eviction needs to know what is still read.
            c.execute(
                "UPDATE kv SET last_used_at = ?1 WHERE ns = ?2 AND key = ?3",
                params![now_ms(), ns, key],
            )?;
        }
        Ok(digest)
    }

    /// Deletes the `n` least recently used cache entries (task results and
    /// Turborepo/Bazel artifacts). Returns how many were deleted.
    pub fn evict_lru(&self, n: usize) -> Result<usize> {
        let c = self.conn.lock();
        let victims: Vec<(String, String, String)> = {
            let mut st = c.prepare(
                "SELECT 'cache', '', action_key, COALESCE(last_hit_at, created_at) AS used FROM cache
                 UNION ALL
                 SELECT 'kv', ns, key, COALESCE(last_used_at, created_at) AS used FROM kv WHERE ns != 'last'
                 ORDER BY used LIMIT ?1",
            )?;
            let rows = st
                .query_map(params![n as i64], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            rows
        };
        for (kind, ns, key) in &victims {
            if kind == "cache" {
                c.execute("DELETE FROM cache WHERE action_key = ?1", params![key])?;
            } else {
                c.execute(
                    "DELETE FROM kv WHERE ns = ?1 AND key = ?2",
                    params![ns, key],
                )?;
            }
        }
        Ok(victims.len())
    }

    pub fn evictable_entries(&self) -> Result<usize> {
        let c = self.conn.lock();
        let n: i64 = c.query_row(
            "SELECT (SELECT COUNT(*) FROM cache) + (SELECT COUNT(*) FROM kv WHERE ns != 'last')",
            [],
            |r| r.get(0),
        )?;
        Ok(n as usize)
    }

    /// Blobs that must survive a sweep: everything a cache entry or remote
    /// cache artifact points to, plus logs and overlays of runs that are
    /// active or newer than `keep_runs_since` (ms). Overlay manifests are
    /// returned separately: the files they list must be kept too.
    pub fn referenced_blobs(
        &self,
        keep_runs_since: i64,
    ) -> Result<(std::collections::HashSet<String>, Vec<String>)> {
        let c = self.conn.lock();
        let mut keep = std::collections::HashSet::new();
        {
            let mut st = c.prepare("SELECT entry FROM cache")?;
            for entry in st.query_map([], |r| r.get::<_, String>(0))? {
                if let Ok(e) = serde_json::from_str::<CacheEntry>(&entry?) {
                    keep.extend(e.outputs.into_iter().map(|o| o.digest));
                    keep.extend(e.log_digest);
                    keep.extend(e.inputs);
                }
            }
        }
        {
            let mut st = c.prepare("SELECT digest FROM kv WHERE ns != 'last'")?;
            for d in st.query_map([], |r| r.get::<_, String>(0))? {
                // Turborepo entries are `<digest>:<duration>`.
                keep.insert(d?.split(':').next().unwrap_or_default().to_string());
            }
        }
        let recent = "(runs.created_at >= ?1 OR runs.status IN ('queued', 'running'))";
        {
            let mut st = c.prepare(&format!(
                "SELECT tasks.log_digest FROM tasks JOIN runs ON runs.id = tasks.run_id
                 WHERE tasks.log_digest IS NOT NULL AND {recent}"
            ))?;
            for d in st.query_map(params![keep_runs_since], |r| r.get::<_, String>(0))? {
                keep.insert(d?);
            }
        }
        let mut overlays = Vec::new();
        {
            let mut st = c.prepare(&format!(
                "SELECT overlay FROM runs WHERE overlay IS NOT NULL AND {recent}"
            ))?;
            for d in st.query_map(params![keep_runs_since], |r| r.get::<_, String>(0))? {
                let d = d?;
                keep.insert(d.clone());
                overlays.push(d);
            }
        }
        Ok((keep, overlays))
    }

    pub fn kv_put(&self, ns: &str, key: &str, digest: &str) -> Result<()> {
        self.conn.lock().execute(
            "INSERT OR REPLACE INTO kv (ns, key, digest, created_at) VALUES (?1, ?2, ?3, ?4)",
            params![ns, key, digest, now_ms()],
        )?;
        Ok(())
    }

    pub fn stats(&self) -> Result<serde_json::Value> {
        let c = self.conn.lock();
        let count = |sql: &str| -> rusqlite::Result<i64> { c.query_row(sql, [], |r| r.get(0)) };
        Ok(serde_json::json!({
            "runs": count("SELECT COUNT(*) FROM runs")?,
            "tasks": count("SELECT COUNT(*) FROM tasks")?,
            "tasksCached": count("SELECT COUNT(*) FROM tasks WHERE status = 'cached'")?,
            "tasksRun": count("SELECT COUNT(*) FROM tasks WHERE status IN ('passed', 'failed')")?,
            "tasksFlaky": count("SELECT COUNT(*) FROM tasks WHERE message LIKE 'flaky:%'")?,
            "cacheEntries": count("SELECT COUNT(*) FROM cache")?,
            "cacheHits": count("SELECT COALESCE(SUM(hits), 0) FROM cache")?,
            "timeSavedMs": count("SELECT COALESCE(SUM(json_extract(entry, '$.durationMs') * hits), 0) FROM cache")?,
            "remoteCacheArtifacts": count("SELECT COUNT(*) FROM kv")?,
        }))
    }

    // ---- workers ----

    pub fn touch_worker(&self, info: &WorkerInfo) -> Result<()> {
        self.conn.lock().execute(
            "INSERT OR REPLACE INTO workers (name, info, last_seen) VALUES (?1, ?2, ?3)",
            params![info.name, serde_json::to_string(info)?, now_ms()],
        )?;
        Ok(())
    }

    pub fn workers(&self) -> Result<Vec<(WorkerInfo, i64, Option<String>)>> {
        let c = self.conn.lock();
        let mut st = c.prepare(
            "SELECT info, last_seen,
               (SELECT run_id FROM groups_ WHERE worker = workers.name AND status = 'running' LIMIT 1)
             FROM workers ORDER BY name",
        )?;
        let rows = st
            .query_map([], |r| {
                let info: String = r.get(0)?;
                Ok((info, r.get(1)?, r.get(2)?))
            })?
            .collect::<rusqlite::Result<Vec<(String, i64, Option<String>)>>>()?;
        Ok(rows
            .into_iter()
            .filter_map(|(i, s, run)| serde_json::from_str(&i).ok().map(|i| (i, s, run)))
            .collect())
    }

    // ---- merge queue ----

    pub fn queue_insert(
        &self,
        repo_id: &str,
        source: &str,
        pr: Option<u64>,
        head_sha: &str,
        target: &str,
    ) -> Result<String> {
        let id = new_id("q");
        let now = now_ms();
        self.conn.lock().execute(
            "INSERT INTO queue (id, repo_id, source, pr, head_sha, target, status, created_at, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, 'queued', ?7, ?7)",
            params![id, repo_id, source, pr.map(|v| v as i64), head_sha, target, now],
        )?;
        Ok(id)
    }

    pub fn queue_active(&self) -> Result<Vec<QueueRow>> {
        let c = self.conn.lock();
        let mut st = c.prepare(
            "SELECT * FROM queue WHERE status IN ('queued', 'testing', 'passed', 'removing') ORDER BY created_at",
        )?;
        let rows = st
            .query_map([], queue_row)?
            .collect::<rusqlite::Result<_>>()?;
        Ok(rows)
    }

    pub fn queue_all(&self, limit: usize) -> Result<Vec<QueueRow>> {
        let c = self.conn.lock();
        let mut st = c.prepare("SELECT * FROM queue ORDER BY created_at DESC LIMIT ?1")?;
        let rows = st
            .query_map(params![limit as i64], queue_row)?
            .collect::<rusqlite::Result<_>>()?;
        Ok(rows)
    }

    pub fn queue_update(&self, row: &QueueRow) -> Result<()> {
        self.conn.lock().execute(
            "UPDATE queue SET status = ?1, lane = ?2, position = ?3, affected = ?4, base_sha = ?5, candidate_sha = ?6,
               run_id = ?7, landed_sha = ?8, reason = ?9, head_sha = ?10, updated_at = ?11 WHERE id = ?12",
            params![
                row.status,
                row.lane,
                row.position,
                serde_json::to_string(&row.affected)?,
                row.base_sha,
                row.candidate_sha,
                row.run_id,
                row.landed_sha,
                row.reason,
                row.head_sha,
                now_ms(),
                row.id
            ],
        )?;
        Ok(())
    }

    /// Asks the queue to drop an active entry, by id or by source branch
    /// (`pr/12`, `my-branch`) within a repo. The next tick does the removal.
    pub fn queue_request_removal(
        &self,
        key: &str,
        repo_id: Option<&str>,
        reason: &str,
    ) -> Result<usize> {
        Ok(self.conn.lock().execute(
            "UPDATE queue SET status = 'removing', reason = ?1, updated_at = ?2
             WHERE (id = ?3 OR (source = ?3 AND repo_id = ?4)) AND status IN ('queued', 'testing', 'passed')",
            params![reason, now_ms(), key, repo_id],
        )?)
    }

    pub fn queue_by_run(&self, run_id: &str) -> Result<Option<QueueRow>> {
        let c = self.conn.lock();
        Ok(c.query_row(
            "SELECT * FROM queue WHERE run_id = ?1",
            params![run_id],
            queue_row,
        )
        .optional()?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn claim_finds_a_compatible_group_behind_many_incompatible_ones() {
        let db = Db::memory().unwrap();
        let repo = db.upsert_repo("r", Some("/tmp/r"), None).unwrap();
        let run = db
            .insert_run(&repo.id, "sha", None, None, 0, "test", false, None)
            .unwrap();
        let task = vec![(
            "t".to_string(),
            TaskDef {
                run: "true".into(),
                ..Default::default()
            },
        )];
        for _ in 0..250 {
            db.insert_group(&run, "win", "windows", &[], &task).unwrap();
        }
        let linux = db.insert_group(&run, "lin", "linux", &[], &task).unwrap();
        let worker = WorkerInfo {
            name: "w".into(),
            os: "linux".into(),
            arch: "x".into(),
            cpus: 1,
            backends: vec![Backend::Host],
            kvm: false,
        };
        let job = db
            .claim_group(&worker)
            .unwrap()
            .expect("linux group was starved");
        assert_eq!(job.group_id, linux);
        assert!(db.heartbeat_group(&job.group_id, Some(&job.lease)).unwrap());
        assert!(!db.heartbeat_group(&job.group_id, Some("stale")).unwrap());
    }
}
