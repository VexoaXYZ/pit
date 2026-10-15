//! Types shared by the server, workers, CLI and dashboard. Everything here is
//! serialized as JSON on the wire.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

/// The evaluated `pit.config.ts`. Unknown fields are errors, so a typo such
/// as `imputs` fails loudly instead of silently meaning "every file".
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct PitConfig {
    /// Groups of tasks. A group is the unit a worker claims: every task in a
    /// group runs in the same warm workspace, so setup work is done once.
    #[serde(default)]
    pub groups: BTreeMap<String, GroupDef>,
    /// Shorthand for a single group named `default`.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub tasks: BTreeMap<String, TaskDef>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct GroupDef {
    /// Operating systems to run this group on: `linux`, `macos`, `windows` or
    /// `any`. One group instance is planned per entry.
    #[serde(default)]
    pub os: Vec<String>,
    /// Default execution backend for tasks in this group.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub backend: Option<Backend>,
    /// Default container image for the `docker` backend.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub image: Option<String>,
    #[serde(default)]
    pub tasks: BTreeMap<String, TaskDef>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct TaskDef {
    /// Shell command.
    pub run: String,
    /// Tasks in the same group that must pass first.
    #[serde(default)]
    pub deps: Vec<String>,
    /// Globs of tracked files this task reads. Defaults to every file.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub inputs: Option<Vec<String>>,
    /// Globs of files this task produces. Restored from the cache on a hit.
    #[serde(default)]
    pub outputs: Vec<String>,
    /// Environment variable names that are passed through and hashed.
    #[serde(default)]
    pub env: Vec<String>,
    /// Environment variable names passed through as secrets: masked in logs
    /// (including base64-encoded forms) and left out of the cache key, so
    /// rotating one does not invalidate cached results.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub secrets: Vec<String>,
    #[serde(default)]
    pub cache: CacheMode,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub backend: Option<Backend>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub image: Option<String>,
    /// Allow network access in isolated backends. Defaults to true.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub network: Option<bool>,
    /// Timeout in seconds.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeout: Option<u64>,
    /// Extra attempts after a failure. A task that passes on a retry is
    /// reported as flaky.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub retries: Option<u32>,
}

#[derive(Debug, Clone, Copy, Default, Serialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum CacheMode {
    /// Results are shared across every worker and `pit run` through the CAS.
    #[default]
    Remote,
    /// Results are remembered per warm workspace. Use for setup tasks such as
    /// dependency installs whose outputs are too big to ship around.
    Workspace,
    /// Always run.
    Off,
}

/// Accepts `cache: false` and `cache: true` as well as the string forms.
impl<'de> Deserialize<'de> for CacheMode {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        match serde_json::Value::deserialize(d)? {
            serde_json::Value::Bool(false) => Ok(CacheMode::Off),
            serde_json::Value::Bool(true) | serde_json::Value::Null => Ok(CacheMode::Remote),
            serde_json::Value::String(s) => match s.as_str() {
                "remote" => Ok(CacheMode::Remote),
                "workspace" => Ok(CacheMode::Workspace),
                "off" => Ok(CacheMode::Off),
                other => Err(serde::de::Error::custom(format!(
                    "unknown cache mode {other:?}, expected remote, workspace or off"
                ))),
            },
            other => Err(serde::de::Error::custom(format!(
                "invalid cache mode {other}"
            ))),
        }
    }
}

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq, Hash)]
#[serde(rename_all = "lowercase")]
pub enum Backend {
    /// Run directly on the worker host.
    #[default]
    Host,
    /// Linux namespaces (pid, ipc, uts, optional net) without a daemon.
    Sandbox,
    /// A container through the local Docker daemon.
    Docker,
}

impl Backend {
    pub fn as_str(&self) -> &'static str {
        match self {
            Backend::Host => "host",
            Backend::Sandbox => "sandbox",
            Backend::Docker => "docker",
        }
    }
}

/// A file change on top of a base commit. `digest: None` means deleted.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord)]
pub struct OverlayEntry {
    pub path: String,
    pub digest: Option<String>,
    #[serde(default)]
    pub executable: bool,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Overlay {
    pub entries: Vec<OverlayEntry>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CreateRun {
    pub repo: String,
    /// Clone URL or local path. Registers the repo when it is new.
    #[serde(default)]
    pub url: Option<String>,
    pub sha: String,
    #[serde(default)]
    pub overlay: Option<String>,
    /// Only run these tasks (and their deps). Empty means all.
    #[serde(default)]
    pub tasks: Vec<String>,
    #[serde(default)]
    pub trigger: Option<String>,
    #[serde(default, rename = "ref")]
    pub git_ref: Option<String>,
    #[serde(default)]
    pub no_cache: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WorkerInfo {
    pub name: String,
    pub os: String,
    pub arch: String,
    pub cpus: usize,
    pub backends: Vec<Backend>,
    pub kvm: bool,
}

/// A group handed to a worker.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Job {
    pub group_id: String,
    /// Identifies this claim of the group. Updates without the current lease
    /// are rejected, so a worker that was presumed dead cannot overwrite the
    /// result of the worker that took the group over.
    pub lease: String,
    pub run_id: String,
    pub repo: String,
    pub url: String,
    pub sha: String,
    pub overlay: Option<String>,
    pub group: String,
    pub os: String,
    pub no_cache: bool,
    /// Automatic retries for every task. Merge queue runs get one, so a flaky
    /// test does not eject a change that did not cause it.
    #[serde(default)]
    pub flake_retries: u32,
    pub tasks: Vec<JobTask>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct JobTask {
    pub id: String,
    pub name: String,
    pub def: TaskDef,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "camelCase")]
pub enum ClaimResponse {
    Job(Box<Job>),
    Idle,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum Status {
    Queued,
    Running,
    Passed,
    Failed,
    Cached,
    Skipped,
    Error,
}

impl Status {
    pub fn as_str(&self) -> &'static str {
        match self {
            Status::Queued => "queued",
            Status::Running => "running",
            Status::Passed => "passed",
            Status::Failed => "failed",
            Status::Cached => "cached",
            Status::Skipped => "skipped",
            Status::Error => "error",
        }
    }

    pub fn parse(s: &str) -> Status {
        match s {
            "running" => Status::Running,
            "passed" => Status::Passed,
            "failed" => Status::Failed,
            "cached" => Status::Cached,
            "skipped" => Status::Skipped,
            "error" => Status::Error,
            _ => Status::Queued,
        }
    }

    pub fn is_done(&self) -> bool {
        !matches!(self, Status::Queued | Status::Running)
    }

    pub fn is_ok(&self) -> bool {
        matches!(self, Status::Passed | Status::Cached)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TaskStart {
    pub action_key: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TaskResult {
    pub status: Status,
    pub exit_code: Option<i32>,
    pub action_key: Option<String>,
    pub duration_ms: u64,
    pub log_digest: Option<String>,
    #[serde(default)]
    pub message: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GroupResult {
    pub status: Status,
    #[serde(default)]
    pub message: Option<String>,
    /// Milliseconds spent preparing the workspace (fetch, checkout, overlay).
    #[serde(default)]
    pub prepare_ms: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct OutputFile {
    pub path: String,
    pub digest: String,
    #[serde(default)]
    pub executable: bool,
}

/// What the task cache stores for a successful action.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CacheEntry {
    pub log_digest: Option<String>,
    pub outputs: Vec<OutputFile>,
    pub duration_ms: u64,
    /// CAS digest of the run's `graph::KeyInputs`, used to explain later misses.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub inputs: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LogChunk {
    pub text: String,
}

/// A live event, sent over SSE to the CLI, dashboard and MCP clients.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Event {
    pub kind: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub run_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub group_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub task_id: Option<String>,
    #[serde(default)]
    pub data: serde_json::Value,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RunView {
    pub id: String,
    pub repo: String,
    pub sha: String,
    #[serde(rename = "ref")]
    pub git_ref: Option<String>,
    pub trigger: String,
    pub status: Status,
    pub overlay_files: usize,
    pub message: Option<String>,
    pub created_at: i64,
    pub started_at: Option<i64>,
    pub finished_at: Option<i64>,
    pub groups: Vec<GroupView>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GroupView {
    pub id: String,
    pub name: String,
    pub os: String,
    pub status: Status,
    pub worker: Option<String>,
    pub prepare_ms: Option<u64>,
    pub message: Option<String>,
    pub started_at: Option<i64>,
    pub finished_at: Option<i64>,
    pub tasks: Vec<TaskView>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TaskView {
    pub id: String,
    pub name: String,
    pub deps: Vec<String>,
    pub status: Status,
    pub action_key: Option<String>,
    pub exit_code: Option<i32>,
    pub duration_ms: Option<u64>,
    pub log_digest: Option<String>,
    pub message: Option<String>,
    pub started_at: Option<i64>,
    pub finished_at: Option<i64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct QueueAdd {
    pub repo: String,
    #[serde(default)]
    pub url: Option<String>,
    /// Branch name to merge.
    #[serde(default)]
    pub branch: Option<String>,
    /// GitHub pull request number.
    #[serde(default)]
    pub pr: Option<u64>,
    #[serde(default)]
    pub target: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct QueueEntryView {
    pub id: String,
    pub repo: String,
    pub source: String,
    pub head_sha: String,
    pub target: String,
    pub status: String,
    pub lane: Option<String>,
    pub position: Option<i64>,
    pub affected: Vec<String>,
    pub candidate_sha: Option<String>,
    pub run_id: Option<String>,
    pub landed_sha: Option<String>,
    pub reason: Option<String>,
    pub created_at: i64,
    pub updated_at: i64,
}

pub fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

pub fn new_id(prefix: &str) -> String {
    let id = uuid::Uuid::new_v4().simple().to_string();
    format!("{prefix}_{}", &id[..12])
}

pub fn current_os() -> &'static str {
    match std::env::consts::OS {
        "macos" => "macos",
        "windows" => "windows",
        _ => "linux",
    }
}
