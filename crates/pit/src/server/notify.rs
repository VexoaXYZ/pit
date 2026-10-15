//! Tells a team when its main branch breaks and when it is fixed.
//!
//! GitHub notifies the person who triggered a run (for schedules, whoever
//! last edited the cron line), so teams wire a Slack step with
//! `if: failure()` into every workflow. Pit posts to one webhook
//! (`--notify-url`, a Slack-compatible incoming webhook) on state changes of
//! trusted runs (main pushes, post-merge, schedules): once when one breaks, once
//! when it is fixed, and nothing while it stays broken or green. PR, local and
//! merge queue runs never notify; the queue ejects instead.

use std::sync::Arc;

use super::App;
use crate::model::Status;

/// What a finished run means for its line (a branch, or one schedule).
#[derive(Debug, PartialEq)]
pub enum Change {
    Broken,
    Fixed,
}

pub fn change(previous: Option<&str>, now: &str) -> Option<Change> {
    match (previous, now) {
        (Some("failed"), "failed") => None,
        (_, "failed") => Some(Change::Broken),
        (Some("failed"), "passed") => Some(Change::Fixed),
        _ => None,
    }
}

pub async fn run_finished(app: &Arc<App>, run_id: &str) {
    if let Err(e) = notify(app, run_id).await {
        tracing::warn!("notify for {run_id}: {e:#}");
    }
}

async fn notify(app: &Arc<App>, run_id: &str) -> anyhow::Result<()> {
    let Some(url) = app.notify_url.lock().clone() else {
        return Ok(());
    };
    let run = app.db.run(run_id)?;
    if !run.trusted || run.trigger == "queue" {
        return Ok(());
    }
    let view = app.db.run_view(run_id)?;
    // A cancelled or superseded run says nothing about the code.
    let msg = view.message.as_deref().unwrap_or_default();
    if msg == "cancelled" || msg.starts_with("superseded") {
        return Ok(());
    }
    let state = match run.status {
        Status::Passed => "passed",
        Status::Failed | Status::Error => "failed",
        _ => return Ok(()),
    };
    // Each schedule is its own line: a failing nightly verify is not fixed
    // by the next push.
    let line = if run.trigger.starts_with("schedule:") {
        run.trigger.clone()
    } else {
        "branch".into()
    };
    let git_ref = run.git_ref.clone().unwrap_or_default();
    let key = format!("{}:{git_ref}:{line}", run.repo_id);
    let created_at = view.created_at;
    let previous = app.db.notify_state(&key)?;
    // Main pushes are never superseded, so runs can finish out of order;
    // an older commit's result says nothing about the line now.
    if previous.as_ref().is_some_and(|(_, at)| *at > created_at) {
        return Ok(());
    }
    let Some(change) = change(previous.as_ref().map(|(s, _)| s.as_str()), state) else {
        app.db.record_notify_state(&key, state, created_at)?;
        return Ok(());
    };
    let failed: Vec<String> = view
        .groups
        .iter()
        .flat_map(|g| {
            g.tasks
                .iter()
                .filter(|t| matches!(t.status, Status::Failed | Status::Error))
                .map(move |t| format!("{}.{}", g.name, t.name))
        })
        .collect();
    let what = match &change {
        Change::Broken if failed.is_empty() => format!("is broken: {msg}"),
        Change::Broken => format!("is broken: {} failed", failed.join(", ")),
        Change::Fixed => "is fixed".to_string(),
    };
    let link = format!("{}/runs/{run_id}", app.public_url);
    let on = if line == "branch" {
        git_ref.clone()
    } else {
        format!("{git_ref} ({})", run.trigger)
    };
    let text = format!("Pit: {} {on} {what}. {link}", view.repo);
    let body = serde_json::json!({
        "text": text,
        "event": match change { Change::Broken => "broken", Change::Fixed => "fixed" },
        "repo": view.repo,
        "ref": git_ref,
        "trigger": run.trigger,
        "run": run_id,
        "url": link,
        "failed": failed,
    });
    let resp = reqwest::Client::new()
        .post(&url)
        .timeout(std::time::Duration::from_secs(10))
        .json(&body)
        .send()
        .await?;
    if !resp.status().is_success() {
        anyhow::bail!("{url} answered {}", resp.status());
    }
    // Only now: a failed post leaves the state, so the next run retries it.
    app.db.record_notify_state(&key, state, created_at)?;
    tracing::info!("notified: {text}");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_state_changes_notify() {
        assert_eq!(change(None, "failed"), Some(Change::Broken));
        assert_eq!(change(Some("passed"), "failed"), Some(Change::Broken));
        assert_eq!(change(Some("failed"), "failed"), None);
        assert_eq!(change(Some("failed"), "passed"), Some(Change::Fixed));
        assert_eq!(change(Some("passed"), "passed"), None);
        assert_eq!(change(None, "passed"), None);
    }
}
