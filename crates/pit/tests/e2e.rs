//! End-to-end tests: a real server and worker in-process, real git repos.
#![cfg(unix)]

use std::path::Path;
use std::time::Duration;

use pit::client::Client;
use pit::model::*;

struct Pit {
    _data: tempfile::TempDir,
    client: Client,
    app: std::sync::Arc<pit::server::App>,
}

async fn start() -> Pit {
    let data = tempfile::tempdir().unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let app = pit::server::App::new(&data.path().join("server"), None, base.clone()).unwrap();
    tokio::spawn(pit::server::serve_listener(app.clone(), listener));
    tokio::spawn(pit::worker::run(pit::worker::WorkerOpts {
        server: base.clone(),
        token: None,
        name: Some("test".into()),
        data_dir: data.path().join("worker"),
        slots: 4,
        parallelism: 4,
    }));
    let client = Client::new(&base, None).unwrap();
    for _ in 0..100 {
        if client.get::<serde_json::Value>("/api/health").await.is_ok() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    Pit {
        _data: data,
        client,
        app,
    }
}

fn sh(dir: &Path, script: &str) -> String {
    let out = std::process::Command::new("sh")
        .arg("-c")
        .arg(script)
        .current_dir(dir)
        .env("GIT_AUTHOR_NAME", "t")
        .env("GIT_AUTHOR_EMAIL", "t@t")
        .env("GIT_COMMITTER_NAME", "t")
        .env("GIT_COMMITTER_EMAIL", "t@t")
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{script}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).into_owned()
}

fn write(dir: &Path, path: &str, body: &str) {
    let p = dir.join(path);
    std::fs::create_dir_all(p.parent().unwrap()).unwrap();
    std::fs::write(p, body).unwrap();
}

fn statuses(view: &RunView) -> Vec<(String, Status)> {
    let mut v: Vec<_> = view
        .groups
        .iter()
        .flat_map(|g| g.tasks.iter().map(|t| (t.name.clone(), t.status)))
        .collect();
    v.sort_by(|a, b| a.0.cmp(&b.0));
    v
}

async fn run(pit: &Pit, dir: &Path) -> RunView {
    let sub = pit::cli::submit(&pit.client, dir, vec![], false)
        .await
        .unwrap();
    tokio::time::timeout(
        Duration::from_secs(60),
        pit::cli::wait(&pit.client, &sub.run_id, |_| {}),
    )
    .await
    .expect("run timed out")
    .unwrap()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn local_runs_cache_restore_outputs_and_ship_overlays() {
    let pit = start().await;
    let repo = tempfile::tempdir().unwrap();
    let dir = repo.path();
    write(
        dir,
        "pit.config.json",
        r#"{"tasks":{
            "install":{"run":"mkdir -p node_modules && echo $RANDOM > node_modules/stamp","inputs":["package.json"],"cache":"workspace"},
            "build":{"run":"mkdir -p dist && cat src/*.txt > dist/out.txt","deps":["install"],"inputs":["src/**"],"outputs":["dist/**"]},
            "test":{"run":"grep -q hello dist/out.txt && ! grep -q BUG dist/out.txt && echo checked","deps":["build"],"inputs":["src/**","test/**"]}
        }}"#,
    );
    write(dir, "package.json", "{}");
    write(dir, "src/a.txt", "hello\n");
    write(dir, "README.md", "readme\n");
    write(dir, ".gitignore", "node_modules\ndist\n");
    sh(
        dir,
        "git init -q -b main && git add -A && git commit -qm init",
    );

    let first = run(&pit, dir).await;
    assert_eq!(first.status, Status::Passed, "{first:?}");
    assert!(statuses(&first).iter().all(|(_, s)| *s == Status::Passed));

    // Nothing changed: everything is served from cache, including the
    // `dist` output that `test` needs.
    let second = run(&pit, dir).await;
    assert_eq!(second.status, Status::Passed);
    assert!(
        statuses(&second).iter().all(|(_, s)| *s == Status::Cached),
        "{:?}",
        statuses(&second)
    );

    // An uncommitted change outside every task's inputs changes nothing.
    write(dir, "README.md", "edited\n");
    let third = run(&pit, dir).await;
    assert_eq!(third.overlay_files, 1);
    assert!(statuses(&third).iter().all(|(_, s)| *s == Status::Cached));

    // An uncommitted, untracked source file reruns build and test only.
    write(dir, "src/b.txt", "more\n");
    let fourth = run(&pit, dir).await;
    assert_eq!(
        statuses(&fourth),
        vec![
            ("build".into(), Status::Passed),
            ("install".into(), Status::Cached),
            ("test".into(), Status::Passed)
        ]
    );

    // Reruns say why they missed the cache.
    let why = |v: &RunView, name: &str| {
        v.groups[0]
            .tasks
            .iter()
            .find(|t| t.name == name)
            .and_then(|t| t.message.clone())
            .unwrap_or_default()
    };
    assert_eq!(why(&fourth, "build"), "ran because src/b.txt added");
    assert_eq!(
        why(&fourth, "test"),
        "ran because src/b.txt added; build changed"
    );

    // A bug fails the run, and the log says why.
    write(dir, "src/b.txt", "BUG\n");
    let fifth = run(&pit, dir).await;
    assert_eq!(fifth.status, Status::Failed);
    let test = fifth.groups[0]
        .tasks
        .iter()
        .find(|t| t.name == "test")
        .unwrap();
    assert_eq!(test.status, Status::Failed);
    assert_eq!(test.exit_code, Some(1));

    // A failing dependency skips its dependents.
    write(dir, "src/b.txt", "fine\n");
    std::fs::write(
        dir.join("pit.config.json"),
        r#"{"tasks":{"build":{"run":"exit 7","inputs":["src/**"]},"test":{"run":"true","deps":["build"]}}}"#,
    )
    .unwrap();
    let sixth = run(&pit, dir).await;
    assert_eq!(
        statuses(&sixth),
        vec![
            ("build".into(), Status::Failed),
            ("test".into(), Status::Skipped)
        ]
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn merge_queue_lands_disjoint_lanes_in_parallel_and_ejects_failures() {
    let pit = start().await;
    let tmp = tempfile::tempdir().unwrap();
    let origin = tmp.path().join("origin.git");
    let work = tmp.path().join("work");
    std::fs::create_dir_all(&work).unwrap();
    sh(tmp.path(), "git init -q --bare -b main origin.git");
    write(
        &work,
        "pit.config.json",
        r#"{"tasks":{
            "a":{"run":"sleep 1 && ! grep -rq FAIL a","inputs":["a/**"]},
            "b":{"run":"sleep 1 && ! grep -rq FAIL b","inputs":["b/**"]}
        }}"#,
    );
    write(&work, "a/base.txt", "a\n");
    write(&work, "b/base.txt", "b\n");
    write(&work, "README.md", "hi\n");
    sh(&work, "git init -q -b main && git add -A && git commit -qm init && git remote add origin ../origin.git && git push -q origin main");
    let branch = |name: &str, path: &str, body: &str| {
        sh(&work, &format!("git checkout -q -b {name} main"));
        write(&work, path, body);
        sh(&work, &format!("git add -A && git commit -qm {name} && git push -q origin {name} && git checkout -q main"));
    };
    branch("docs", "README.md", "docs only\n");
    branch("feat-a1", "a/one.txt", "1\n");
    branch("feat-b1", "b/one.txt", "1\n");
    branch("feat-a2", "a/two.txt", "FAIL\n");
    branch("feat-a3", "a/three.txt", "3\n");

    let url = origin.to_string_lossy().to_string();
    for b in ["docs", "feat-a1", "feat-b1", "feat-a2", "feat-a3"] {
        let _: QueueEntryView = pit
            .client
            .post(
                "/api/queue",
                &QueueAdd {
                    repo: "q".into(),
                    url: Some(url.clone()),
                    branch: Some(b.into()),
                    pr: None,
                    target: None,
                },
            )
            .await
            .unwrap();
    }

    let entries = tokio::time::timeout(Duration::from_secs(90), async {
        loop {
            let entries: Vec<QueueEntryView> = pit.client.get("/api/queue").await.unwrap();
            if entries
                .iter()
                .all(|e| matches!(e.status.as_str(), "landed" | "failed"))
            {
                return entries;
            }
            tokio::time::sleep(Duration::from_millis(200)).await;
        }
    })
    .await
    .expect("queue did not settle");
    let get = |s: &str| entries.iter().find(|e| e.source == s).unwrap().clone();

    assert_eq!(get("docs").status, "landed");
    assert_eq!(get("docs").reason.as_deref(), Some("no tasks affected"));
    assert_eq!(get("feat-a1").status, "landed");
    assert_eq!(get("feat-b1").status, "landed");
    assert_eq!(get("feat-a2").status, "failed");
    assert_eq!(get("feat-a3").status, "landed");
    assert_eq!(get("feat-a1").affected, vec!["default.a"]);
    assert_eq!(get("feat-b1").affected, vec!["default.b"]);

    // a1 and b1 had disjoint tasks: separate lanes, tested at the same time.
    assert_ne!(get("feat-a1").lane, get("feat-b1").lane);
    let ra: RunView = pit
        .client
        .get(&format!("/api/runs/{}", get("feat-a1").run_id.unwrap()))
        .await
        .unwrap();
    let rb: RunView = pit
        .client
        .get(&format!("/api/runs/{}", get("feat-b1").run_id.unwrap()))
        .await
        .unwrap();
    assert!(
        ra.started_at.unwrap() < rb.finished_at.unwrap()
            && rb.started_at.unwrap() < ra.finished_at.unwrap(),
        "lanes did not overlap"
    );
    // a2 overlapped a1, so it was tested speculatively on top of it.
    assert_eq!(get("feat-a2").lane, get("feat-a1").lane);

    // main has every passing change and not the failing one.
    let main_files = sh(&origin, "git ls-tree -r --name-only main");
    for f in ["a/one.txt", "a/three.txt", "b/one.txt"] {
        assert!(
            main_files.contains(f),
            "{f} missing from main:\n{main_files}"
        );
    }
    assert!(!main_files.contains("a/two.txt"));
    assert_eq!(sh(&origin, "git show main:README.md"), "docs only\n");
}

/// GitHub sends a signed webhook; Pit runs the commit and reports statuses.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn github_webhook_runs_commit_and_reports_statuses() {
    use axum::routing::post;
    use hmac::Mac;
    use std::sync::{Arc, Mutex};

    // A fake GitHub API that records commit status calls.
    let calls: Arc<Mutex<Vec<(String, serde_json::Value)>>> = Default::default();
    let rec = calls.clone();
    let gh_api = axum::Router::new().route(
        "/repos/{owner}/{repo}/statuses/{sha}",
        post(
            move |axum::extract::Path((o, r, sha)): axum::extract::Path<(
                String,
                String,
                String,
            )>,
                  axum::Json(body): axum::Json<serde_json::Value>| {
                let rec = rec.clone();
                async move {
                    rec.lock().unwrap().push((format!("{o}/{r}@{sha}"), body));
                    axum::Json(serde_json::json!({ "id": 1 }))
                }
            },
        ),
    );
    let gh_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let gh_url = format!("http://{}", gh_listener.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(gh_listener, gh_api).await });

    let data = tempfile::tempdir().unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let github =
        pit::server::github::GitHub::with_token(&gh_url, "test-token", Some("s3cret".into()))
            .unwrap();
    let app = pit::server::App::with_github(
        &data.path().join("server"),
        None,
        base.clone(),
        Some(github),
    )
    .unwrap();
    tokio::spawn(pit::server::serve_listener(app, listener));
    tokio::spawn(pit::worker::run(pit::worker::WorkerOpts {
        server: base.clone(),
        token: None,
        name: Some("gh".into()),
        data_dir: data.path().join("worker"),
        slots: 2,
        parallelism: 2,
    }));

    let repo = tempfile::tempdir().unwrap();
    write(
        repo.path(),
        "pit.config.json",
        r#"{"groups":{"ci":{"tasks":{"check":{"run":"echo ok"}}}}}"#,
    );
    sh(
        repo.path(),
        "git init -q -b main && git add -A && git commit -qm init",
    );
    let sha = sh(repo.path(), "git rev-parse HEAD").trim().to_string();

    let payload = serde_json::json!({
        "ref": "refs/heads/main",
        "after": sha,
        "repository": { "full_name": "acme/widgets", "clone_url": repo.path().to_string_lossy(), "default_branch": "main" }
    })
    .to_string();
    let mut mac = hmac::Hmac::<sha2::Sha256>::new_from_slice(b"s3cret").unwrap();
    mac.update(payload.as_bytes());
    let sig = format!("sha256={}", hex::encode(mac.finalize().into_bytes()));
    let http = reqwest::Client::new();

    // Unsigned or wrongly signed webhooks are rejected.
    let bad = http
        .post(format!("{base}/webhooks/github"))
        .header("x-github-event", "push")
        .body(payload.clone())
        .send()
        .await
        .unwrap();
    assert_eq!(bad.status(), 401);

    let resp = http
        .post(format!("{base}/webhooks/github"))
        .header("x-github-event", "push")
        .header("x-hub-signature-256", sig)
        .body(payload)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 202);
    let run_id = resp.json::<serde_json::Value>().await.unwrap()["run"]
        .as_str()
        .unwrap()
        .to_string();
    let client = Client::new(&base, None).unwrap();
    let view = tokio::time::timeout(
        Duration::from_secs(30),
        pit::cli::wait(&client, &run_id, |_| {}),
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(view.status, Status::Passed);
    assert_eq!(view.trigger, "push");

    // The final status for the group is success, on the right commit.
    tokio::time::sleep(Duration::from_millis(300)).await;
    let calls = calls.lock().unwrap().clone();
    assert!(!calls.is_empty(), "no statuses reported");
    assert!(calls
        .iter()
        .all(|(target, _)| *target == format!("acme/widgets@{sha}")));
    let last = &calls.last().unwrap().1;
    assert_eq!(last["state"], "success", "{calls:?}");
    assert_eq!(last["context"], "pit / ci");
    assert!(
        calls.iter().any(|(_, b)| b["state"] == "pending"),
        "expected a pending status first: {calls:?}"
    );
}

/// Turborepo and Bazel can use Pit as their remote cache.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn turbo_and_bazel_remote_cache_protocols() {
    use sha2::Digest;
    let data = tempfile::tempdir().unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let app = pit::server::App::with_github(
        &data.path().join("server"),
        Some("tok".into()),
        base.clone(),
        None,
    )
    .unwrap();
    tokio::spawn(pit::server::serve_listener(app, listener));
    tokio::time::sleep(Duration::from_millis(100)).await;
    let http = reqwest::Client::new();

    // Turborepo: auth required, PUT then GET/HEAD, query existence.
    let unauth = http
        .get(format!("{base}/v8/artifacts/status"))
        .send()
        .await
        .unwrap();
    assert_eq!(unauth.status(), 401);
    let status: serde_json::Value = http
        .get(format!("{base}/v8/artifacts/status"))
        .bearer_auth("tok")
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(status["status"], "enabled");
    let art = b"tarball bytes".to_vec();
    let put = http
        .put(format!("{base}/v8/artifacts/abc123?teamId=team_1"))
        .bearer_auth("tok")
        .header("x-artifact-duration", "4200")
        .body(art.clone())
        .send()
        .await
        .unwrap();
    assert_eq!(put.status(), 202);
    let got = http
        .get(format!("{base}/v8/artifacts/abc123?teamId=team_1"))
        .bearer_auth("tok")
        .send()
        .await
        .unwrap();
    assert_eq!(got.headers()["x-artifact-duration"], "4200");
    assert_eq!(got.bytes().await.unwrap().to_vec(), art);
    let other_team = http
        .get(format!("{base}/v8/artifacts/abc123?teamId=team_2"))
        .bearer_auth("tok")
        .send()
        .await
        .unwrap();
    assert_eq!(other_team.status(), 404);
    let q: serde_json::Value = http
        .post(format!("{base}/v8/artifacts?teamId=team_1"))
        .bearer_auth("tok")
        .json(&serde_json::json!({ "hashes": ["abc123", "missing"] }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(q["abc123"]["taskDurationMs"], 4200);
    assert!(q.get("missing").is_none());

    // Bazel HTTP cache: CAS entries are verified against their sha256.
    let blob = b"bazel output".to_vec();
    let h = hex::encode(sha2::Sha256::digest(&blob));
    let ok = http
        .put(format!("{base}/bazel/cas/{h}"))
        .basic_auth("pit", Some("tok"))
        .body(blob.clone())
        .send()
        .await
        .unwrap();
    assert_eq!(ok.status(), 200);
    let wrong = http
        .put(format!(
            "{base}/bazel/cas/{}",
            hex::encode(sha2::Sha256::digest(b"x"))
        ))
        .basic_auth("pit", Some("tok"))
        .body(blob.clone())
        .send()
        .await
        .unwrap();
    assert_eq!(wrong.status(), 400);
    let back = http
        .get(format!("{base}/bazel/cas/{h}"))
        .basic_auth("pit", Some("tok"))
        .send()
        .await
        .unwrap();
    assert_eq!(back.bytes().await.unwrap().to_vec(), blob);
    let ac_key = "a".repeat(64);
    http.put(format!("{base}/bazel/ac/{ac_key}"))
        .basic_auth("pit", Some("tok"))
        .body(b"action result".to_vec())
        .send()
        .await
        .unwrap();
    let head = http
        .head(format!("{base}/bazel/ac/{ac_key}"))
        .basic_auth("pit", Some("tok"))
        .send()
        .await
        .unwrap();
    assert_eq!(head.status(), 200);
}

/// A run cancelled before any worker picks it up must fail, never pass:
/// the merge queue lands on green runs.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cancelled_run_is_not_green() {
    // Server only, no worker, so the group stays queued.
    let data = tempfile::tempdir().unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let app = pit::server::App::with_github(&data.path().join("server"), None, base.clone(), None)
        .unwrap();
    tokio::spawn(pit::server::serve_listener(app, listener));
    tokio::time::sleep(Duration::from_millis(100)).await;
    let client = Client::new(&base, None).unwrap();

    let repo = tempfile::tempdir().unwrap();
    write(
        repo.path(),
        "pit.config.json",
        r#"{"tasks":{"t":{"run":"true"}}}"#,
    );
    sh(
        repo.path(),
        "git init -q -b main && git add -A && git commit -qm init",
    );
    let sub = pit::cli::submit(&client, repo.path(), vec![], false)
        .await
        .unwrap();
    // Wait for planning, then cancel.
    for _ in 0..100 {
        let v: RunView = client
            .get(&format!("/api/runs/{}", sub.run_id))
            .await
            .unwrap();
        if !v.groups.is_empty() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let _: serde_json::Value = client
        .post(&format!("/api/runs/{}/cancel", sub.run_id), &())
        .await
        .unwrap();
    let v: RunView = client
        .get(&format!("/api/runs/{}", sub.run_id))
        .await
        .unwrap();
    assert_eq!(v.status, Status::Failed, "{v:?}");
    assert_eq!(v.message.as_deref(), Some("cancelled"));
}

/// Tasks never see Pit's own settings, such as the API token.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn tasks_do_not_inherit_pit_secrets() {
    std::env::set_var("PIT_TEST_SECRET", "hunter2");
    let pit = start().await;
    let repo = tempfile::tempdir().unwrap();
    write(
        repo.path(),
        "pit.config.json",
        r#"{"tasks":{"env":{"run":"test -z \"$PIT_TEST_SECRET\" && test \"$PIT\" = 1","cache":false}}}"#,
    );
    sh(
        repo.path(),
        "git init -q -b main && git add -A && git commit -qm init",
    );
    let view = run(&pit, repo.path()).await;
    assert_eq!(view.status, Status::Passed, "task saw PIT_TEST_SECRET");
}

/// Removing a queued change stops it from landing, and the change behind it
/// in its lane is retested and lands without it.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn removed_queue_entry_never_lands() {
    let pit = start().await;
    let tmp = tempfile::tempdir().unwrap();
    let origin = tmp.path().join("origin.git");
    let work = tmp.path().join("work");
    std::fs::create_dir_all(&work).unwrap();
    sh(tmp.path(), "git init -q --bare -b main origin.git");
    write(
        &work,
        "pit.config.json",
        r#"{"tasks":{"a":{"run":"sleep 2","inputs":["a/**"]}}}"#,
    );
    write(&work, "a/base.txt", "a\n");
    sh(&work, "git init -q -b main && git add -A && git commit -qm init && git remote add origin ../origin.git && git push -q origin main");
    for (name, file) in [("first", "a/first.txt"), ("second", "a/second.txt")] {
        sh(&work, &format!("git checkout -q -b {name} main"));
        write(&work, file, "x\n");
        sh(&work, &format!("git add -A && git commit -qm {name} && git push -q origin {name} && git checkout -q main"));
    }
    let url = origin.to_string_lossy().to_string();
    let mut ids = vec![];
    for b in ["first", "second"] {
        let e: QueueEntryView = pit
            .client
            .post(
                "/api/queue",
                &QueueAdd {
                    repo: "rm".into(),
                    url: Some(url.clone()),
                    branch: Some(b.into()),
                    pr: None,
                    target: None,
                },
            )
            .await
            .unwrap();
        ids.push(e.id);
    }
    // Both overlap on task `a`: second is tested on top of first.
    let removed: serde_json::Value = pit
        .client
        .delete(&format!("/api/queue/{}", ids[0]))
        .await
        .unwrap();
    assert_eq!(removed["removed"], 1);

    let entries = tokio::time::timeout(Duration::from_secs(60), async {
        loop {
            let entries: Vec<QueueEntryView> = pit.client.get("/api/queue").await.unwrap();
            if entries
                .iter()
                .all(|e| matches!(e.status.as_str(), "landed" | "failed" | "removed"))
            {
                return entries;
            }
            tokio::time::sleep(Duration::from_millis(200)).await;
        }
    })
    .await
    .expect("queue did not settle");
    let get = |s: &str| entries.iter().find(|e| e.source == s).unwrap().clone();
    assert_eq!(get("first").status, "removed");
    assert_eq!(get("second").status, "landed", "{entries:?}");
    let main_files = sh(&origin, "git ls-tree -r --name-only main");
    assert!(main_files.contains("a/second.txt"));
    assert!(!main_files.contains("a/first.txt"), "removed change landed");
    // Removing something that is not queued is a 404.
    assert!(pit
        .client
        .delete::<serde_json::Value>(&format!("/api/queue/{}", ids[0]))
        .await
        .is_err());
}

/// Only the current claim of a group may report on it: a worker that was
/// presumed dead and replaced cannot overwrite the result.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn stale_claims_cannot_report() {
    let data = tempfile::tempdir().unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let app = pit::server::App::with_github(&data.path().join("server"), None, base.clone(), None)
        .unwrap();
    tokio::spawn(pit::server::serve_listener(app, listener));
    tokio::time::sleep(Duration::from_millis(100)).await;
    let client = Client::new(&base, None).unwrap();

    let repo = tempfile::tempdir().unwrap();
    write(
        repo.path(),
        "pit.config.json",
        r#"{"tasks":{"t":{"run":"true"}}}"#,
    );
    sh(
        repo.path(),
        "git init -q -b main && git add -A && git commit -qm init",
    );
    let sub = pit::cli::submit(&client, repo.path(), vec![], false)
        .await
        .unwrap();

    // Act as a worker by hand.
    let info = WorkerInfo {
        name: "fake".into(),
        os: pit::model::current_os().into(),
        arch: "x".into(),
        cpus: 1,
        backends: vec![Backend::Host],
        kvm: false,
    };
    let job = loop {
        if let ClaimResponse::Job(job) = client.claim(&info).await.unwrap() {
            break job;
        }
    };
    assert_eq!(job.run_id, sub.run_id);
    let result = GroupResult {
        status: Status::Passed,
        message: None,
        prepare_ms: 0,
    };
    let path = format!("/api/groups/{}/finish", job.group_id);

    let stale = client.with_lease("lease_stale");
    let err = stale
        .post_empty(&path, &result)
        .await
        .unwrap_err()
        .to_string();
    assert!(err.contains("409"), "{err}");
    let task_path = format!("/api/tasks/{}/start", job.tasks[0].id);
    assert!(stale
        .post_empty(&task_path, &TaskStart { action_key: None })
        .await
        .is_err());
    assert!(
        client.post_empty(&path, &result).await.is_err(),
        "no lease must be refused"
    );

    client
        .with_lease(&job.lease)
        .post_empty(&path, &result)
        .await
        .unwrap();
    let v: RunView = client
        .get(&format!("/api/runs/{}", sub.run_id))
        .await
        .unwrap();
    assert_eq!(v.status, Status::Passed);
}

/// A flaky test must not eject an innocent change from the merge queue: the
/// queue retries once and lands it, flagged as flaky. Outside the queue the
/// task stays strict, and the flaky pass is not served from the cache.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn merge_queue_retries_flakes_once_and_flags_them() {
    let pit = start().await;
    let tmp = tempfile::tempdir().unwrap();
    let origin = tmp.path().join("origin.git");
    let work = tmp.path().join("work");
    let marks = tmp.path().join("marks");
    std::fs::create_dir_all(&work).unwrap();
    std::fs::create_dir_all(&marks).unwrap();
    sh(tmp.path(), "git init -q --bare -b main origin.git");
    // Fails the first attempt of every run, passes the second.
    let flaky = format!(
        r#"f={}/$PIT_RUN_ID; if [ -f $f ]; then exit 0; else touch $f; exit 1; fi"#,
        marks.display()
    );
    let config =
        serde_json::json!({ "tasks": { "flaky": { "run": flaky, "inputs": ["app/**"] } } });
    write(&work, "pit.config.json", &config.to_string());
    write(&work, "app/a.txt", "a\n");
    sh(&work, "git init -q -b main && git add -A && git commit -qm init && git remote add origin ../origin.git && git push -q origin main");
    sh(&work, "git checkout -q -b innocent && echo b > app/b.txt && git add -A && git commit -qm b && git push -q origin innocent && git checkout -q main");

    let url = origin.to_string_lossy().to_string();
    let _: QueueEntryView = pit
        .client
        .post(
            "/api/queue",
            &QueueAdd {
                repo: "flaky".into(),
                url: Some(url),
                branch: Some("innocent".into()),
                pr: None,
                target: None,
            },
        )
        .await
        .unwrap();
    let entry = tokio::time::timeout(Duration::from_secs(60), async {
        loop {
            let entries: Vec<QueueEntryView> = pit.client.get("/api/queue").await.unwrap();
            let e = entries
                .into_iter()
                .find(|e| e.source == "innocent")
                .unwrap();
            if matches!(e.status.as_str(), "landed" | "failed") {
                return e;
            }
            tokio::time::sleep(Duration::from_millis(200)).await;
        }
    })
    .await
    .expect("queue did not settle");
    assert_eq!(
        entry.status, "landed",
        "a flake ejected an innocent change: {entry:?}"
    );
    let queued: RunView = pit
        .client
        .get(&format!("/api/runs/{}", entry.run_id.unwrap()))
        .await
        .unwrap();
    let task = &queued.groups[0].tasks[0];
    assert!(
        task.message
            .as_deref()
            .unwrap_or_default()
            .starts_with("flaky: attempt 1: exit 1, passed on attempt 2"),
        "{task:?}"
    );

    // The same tree outside the queue: strict, and not served from the cache.
    sh(&work, "git pull -q origin main");
    let local = run(&pit, &work).await;
    assert_eq!(local.status, Status::Failed);
}

/// The cache stays under its size limit: least recently used results are
/// evicted, a rerun says its result was evicted, and recent results survive.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn cache_is_kept_under_its_size_limit() {
    use std::sync::atomic::Ordering;
    let pit = start().await;
    pit.app.cache_max_bytes.store(3_500_000, Ordering::Relaxed);
    pit.app.gc_grace_ms.store(0, Ordering::Relaxed);
    let repo = tempfile::tempdir().unwrap();
    let dir = repo.path();
    write(
        dir,
        "pit.config.json",
        r#"{"tasks":{"artifact":{"run":"mkdir -p out && head -c 1000000 /dev/urandom > out/blob.bin","inputs":["src/**"],"outputs":["out/**"]}}}"#,
    );
    write(dir, "src/v", "0\n");
    write(dir, ".gitignore", "out\n");
    sh(
        dir,
        "git init -q -b main && git add -A && git commit -qm init",
    );
    for v in 1..=6 {
        write(dir, "src/v", &format!("{v}\n"));
        assert_eq!(run(&pit, dir).await.status, Status::Passed);
    }
    let report: serde_json::Value = pit.client.post("/api/gc", &()).await.unwrap();
    assert!(report["evictedEntries"].as_u64().unwrap() > 0, "{report}");
    assert!(
        report["bytesAfter"].as_u64().unwrap() <= 3_150_000,
        "{report}"
    );

    // The newest result is still cached.
    let newest = run(&pit, dir).await;
    assert_eq!(newest.groups[0].tasks[0].status, Status::Cached, "{report}");
    // The oldest was evicted: it runs again instead of coming from the cache.
    write(dir, "src/v", "1\n");
    let oldest = run(&pit, dir).await;
    assert_eq!(oldest.groups[0].tasks[0].status, Status::Passed);
    // When the task's latest result itself is evicted, the rerun says so.
    pit.app.cache_max_bytes.store(1, Ordering::Relaxed);
    let _: serde_json::Value = pit.client.post("/api/gc", &()).await.unwrap();
    let rerun = run(&pit, dir).await;
    let t = &rerun.groups[0].tasks[0];
    assert_eq!(t.status, Status::Passed);
    assert_eq!(
        t.message.as_deref(),
        Some("ran because its earlier result was evicted to keep the cache under its size limit")
    );
    let stats: serde_json::Value = pit.client.get("/api/stats").await.unwrap();
    assert!(stats["evictedEntries"].as_u64().unwrap() > 0);
}

/// Blobs inside the grace period cannot be freed yet; the collector must not
/// evict the whole cache trying to make room for them.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn gc_does_not_empty_the_cache_for_blobs_it_cannot_free_yet() {
    use std::sync::atomic::Ordering;
    let pit = start().await;
    let repo = tempfile::tempdir().unwrap();
    let dir = repo.path();
    write(
        dir,
        "pit.config.json",
        r#"{"tasks":{"artifact":{"run":"mkdir -p out && head -c 1000000 /dev/urandom > out/blob.bin","inputs":["src/**"],"outputs":["out/**"]}}}"#,
    );
    write(dir, ".gitignore", "out\n");
    write(dir, "src/v", "0\n");
    sh(
        dir,
        "git init -q -b main && git add -A && git commit -qm init",
    );
    for v in 1..=4 {
        write(dir, "src/v", &format!("{v}\n"));
        run(&pit, dir).await;
    }
    // Every blob is young and the store is far over the limit.
    pit.app.cache_max_bytes.store(1_000_000, Ordering::Relaxed);
    let report: serde_json::Value = pit.client.post("/api/gc", &()).await.unwrap();
    assert_eq!(report["evictedEntries"], 0, "{report}");
    let newest = run(&pit, dir).await;
    assert_eq!(newest.groups[0].tasks[0].status, Status::Cached);
}

/// When origin is down (a GitHub outage), queueing still works from the last
/// fetched state, the entry says why it is waiting, and it lands once origin
/// is back.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn merge_queue_waits_out_an_origin_outage() {
    let pit = start().await;
    let tmp = tempfile::tempdir().unwrap();
    let origin = tmp.path().join("origin.git");
    let down = tmp.path().join("origin.git.down");
    let work = tmp.path().join("work");
    std::fs::create_dir_all(&work).unwrap();
    sh(tmp.path(), "git init -q --bare -b main origin.git");
    write(
        &work,
        "pit.config.json",
        r#"{"tasks":{"t":{"run":"true","inputs":["src/**"]}}}"#,
    );
    write(&work, "src/a.txt", "a\n");
    sh(&work, "git init -q -b main && git add -A && git commit -qm init && git remote add origin ../origin.git && git push -q origin main");
    sh(&work, "git checkout -q -b feature && echo b > src/b.txt && git add -A && git commit -qm b && git push -q origin feature && git checkout -q main");
    let url = origin.to_string_lossy().to_string();

    // Warm the server's mirror while origin is up.
    let sha = sh(&work, "git rev-parse main").trim().to_string();
    let created: serde_json::Value = pit
        .client
        .post(
            "/api/runs",
            &serde_json::json!({ "repo": "outage", "url": url, "sha": sha }),
        )
        .await
        .unwrap();
    pit::cli::wait(&pit.client, created["id"].as_str().unwrap(), |_| {})
        .await
        .unwrap();

    std::fs::rename(&origin, &down).unwrap();
    let entry: QueueEntryView = pit
        .client
        .post(
            "/api/queue",
            &QueueAdd {
                repo: "outage".into(),
                url: Some(url.clone()),
                branch: Some("feature".into()),
                pr: None,
                target: None,
            },
        )
        .await
        .expect("queueing must work during an outage");
    assert_eq!(entry.status, "queued");
    assert!(
        entry
            .reason
            .as_deref()
            .unwrap_or_default()
            .starts_with("waiting for origin"),
        "{entry:?}"
    );

    std::fs::rename(&down, &origin).unwrap();
    let landed = tokio::time::timeout(Duration::from_secs(30), async {
        loop {
            let entries: Vec<QueueEntryView> = pit.client.get("/api/queue").await.unwrap();
            let e = entries.into_iter().find(|e| e.id == entry.id).unwrap();
            if e.status == "landed" {
                return e;
            }
            tokio::time::sleep(Duration::from_millis(200)).await;
        }
    })
    .await
    .expect("entry did not land after origin came back");
    assert!(landed.landed_sha.is_some());
    assert!(sh(&origin, "git ls-tree -r --name-only main").contains("src/b.txt"));
}

/// A newer push to the same branch supersedes older runs, and their running
/// tasks are actually stopped, not left to finish.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn newer_pushes_supersede_and_stop_older_runs() {
    let pit = start().await;
    let tmp = tempfile::tempdir().unwrap();
    let marks = tmp.path().join("marks");
    std::fs::create_dir_all(&marks).unwrap();
    let repo = tmp.path().join("repo");
    std::fs::create_dir_all(&repo).unwrap();
    let task = format!("sleep 4 && touch {}/$PIT_RUN_ID", marks.display());
    write(
        &repo,
        "pit.config.json",
        &serde_json::json!({ "tasks": { "slow": { "run": task, "cache": false } } }).to_string(),
    );
    sh(
        &repo,
        "git init -q -b main && git add -A && git commit -qm init",
    );
    let url = repo.to_string_lossy().to_string();
    let mut runs = vec![];
    for i in 0..3 {
        write(&repo, "v.txt", &i.to_string());
        sh(&repo, &format!("git add -A && git commit -qm push{i}"));
        let sha = sh(&repo, "git rev-parse HEAD").trim().to_string();
        let created: serde_json::Value = pit
            .client
            .post("/api/runs", &serde_json::json!({ "repo": "sup", "url": url, "sha": sha, "trigger": "push", "ref": "feature" }))
            .await
            .unwrap();
        runs.push(created["id"].as_str().unwrap().to_string());
        tokio::time::sleep(Duration::from_millis(700)).await;
    }
    let last = pit::cli::wait(&pit.client, &runs[2], |_| {}).await.unwrap();
    assert_eq!(last.status, Status::Passed);
    // Each push supersedes the run before it.
    for (old, newer) in runs.iter().zip(runs.iter().skip(1)) {
        let v: RunView = pit.client.get(&format!("/api/runs/{old}")).await.unwrap();
        assert_eq!(v.status, Status::Failed);
        assert_eq!(v.message, Some(format!("superseded by {newer}")));
    }
    // Long enough for an unstopped old task to have finished.
    tokio::time::sleep(Duration::from_secs(3)).await;
    assert!(marks.join(&runs[2]).exists());
    for old in &runs[..2] {
        assert!(
            !marks.join(old).exists(),
            "superseded run {old} kept running to the end"
        );
    }
    // Pushes to main are never superseded.
    assert!(!pit::server::App::supersedes_for_test("push", "main"));
}
