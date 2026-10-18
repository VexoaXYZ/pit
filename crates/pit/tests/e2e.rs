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
    base: String,
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
        workspace_max_bytes: 0,
        worker_token: None,
        secrets_file: None,
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
        base,
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
    run_with(pit, dir, Default::default()).await
}

async fn run_with(pit: &Pit, dir: &Path, opts: pit::cli::RunOpts) -> RunView {
    let sub = pit::cli::submit(&pit.client, dir, vec![], opts)
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
            "a":{"run":"sleep 3 && ! grep -rq FAIL a","inputs":["a/**"]},
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
                    sha: None,
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
    // a2 overlapped a1, so it was tested on top of it: speculatively in
    // a1's lane, or, if a1 had already landed, on a target that has it.
    let (a1, a2) = (get("feat-a1"), get("feat-a2"));
    let on_top = a2.lane == a1.lane
        || sh(
            &origin,
            &format!(
                "git merge-base --is-ancestor {} {} && echo yes || echo no",
                a1.landed_sha.clone().unwrap(),
                a2.landed_sha.clone().unwrap()
            ),
        )
        .contains("yes");
    assert!(on_top, "a2 was not tested on top of a1: {a1:?} {a2:?}");

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
        workspace_max_bytes: 0,
        worker_token: None,
        secrets_file: None,
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
    let sub = pit::cli::submit(&client, repo.path(), vec![], Default::default())
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
                    sha: None,
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
    let sub = pit::cli::submit(&client, repo.path(), vec![], Default::default())
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
                sha: None,
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
                sha: None,
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
    // Pushes to main, or to a default branch with any other name (trusted),
    // are never superseded.
    assert!(!pit::server::App::supersedes_for_test(
        "push", "main", false
    ));
    assert!(!pit::server::App::supersedes_for_test(
        "push", "develop", true
    ));
    assert!(pit::server::App::supersedes_for_test(
        "push", "feature", false
    ));
}

/// A run whose every task is cached does not wait for a worker: with every
/// slot busy, it still finishes at once, answered by the server.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn fully_cached_runs_do_not_wait_for_busy_workers() {
    let pit = start().await;
    let tmp = tempfile::tempdir().unwrap();
    let app = tmp.path().join("app");
    write(
        &app,
        "pit.config.json",
        r#"{"tasks":{"build":{"run":"mkdir -p dist && cp src/a.txt dist/","inputs":["src/**"],"outputs":["dist/**"]},"test":{"run":"test -f dist/a.txt","deps":["build"],"inputs":["src/**"]}}}"#,
    );
    write(&app, "src/a.txt", "a\n");
    write(&app, ".gitignore", "dist\n");
    sh(
        &app,
        "git init -q -b main && git add -A && git commit -qm init",
    );
    assert_eq!(run(&pit, &app).await.status, Status::Passed);

    // Occupy all four worker slots.
    let busy = tmp.path().join("busy");
    write(
        &busy,
        "pit.config.json",
        r#"{"tasks":{"long":{"run":"sleep 30","cache":false}}}"#,
    );
    sh(
        &busy,
        "git init -q -b main && git add -A && git commit -qm init",
    );
    let sha = sh(&busy, "git rev-parse HEAD").trim().to_string();
    let url = busy.to_string_lossy().to_string();
    let mut long = vec![];
    for _ in 0..4 {
        let c: serde_json::Value = pit
            .client
            .post(
                "/api/runs",
                &serde_json::json!({ "repo": "busy", "url": url, "sha": sha }),
            )
            .await
            .unwrap();
        long.push(c["id"].as_str().unwrap().to_string());
    }
    tokio::time::sleep(Duration::from_millis(1500)).await;

    let started = std::time::Instant::now();
    let cached = run(&pit, &app).await;
    assert_eq!(cached.status, Status::Passed);
    assert!(
        started.elapsed() < Duration::from_secs(3),
        "waited {:?} for a busy worker",
        started.elapsed()
    );
    assert_eq!(cached.groups[0].worker.as_deref(), Some("cache"));
    assert!(cached.groups[0]
        .tasks
        .iter()
        .all(|t| t.status == Status::Cached));

    for id in long {
        let _: serde_json::Value = pit
            .client
            .post(&format!("/api/runs/{id}/cancel"), &())
            .await
            .unwrap();
    }
}

/// Secrets are masked in task logs, including base64 and double-base64 forms,
/// while the task still gets the real value.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn secrets_are_masked_in_logs() {
    std::env::set_var("E2E_DEPLOY_TOKEN", "tok_live_e2e_51a2b3");
    let pit = start().await;
    let repo = tempfile::tempdir().unwrap();
    let config = serde_json::json!({ "tasks": { "deploy": {
        "run": "test \"$E2E_DEPLOY_TOKEN\" = tok_live_e2e_51a2b3 && echo plain=$E2E_DEPLOY_TOKEN && echo b64=$(printf %s \"$E2E_DEPLOY_TOKEN\" | base64) && echo b64x2=$(printf %s \"$E2E_DEPLOY_TOKEN\" | base64 | base64 | tr -d '\\n')",
        "secrets": ["E2E_DEPLOY_TOKEN"],
        "cache": false
    } } });
    write(repo.path(), "pit.config.json", &config.to_string());
    sh(
        repo.path(),
        "git init -q -b main && git add -A && git commit -qm init",
    );
    let view = run(&pit, repo.path()).await;
    assert_eq!(
        view.status,
        Status::Passed,
        "the task must see the real value"
    );
    let task = &view.groups[0].tasks[0];
    let log = pit
        .client
        .get_text(&format!("/api/tasks/{}/log", task.id))
        .await
        .unwrap();
    assert!(!log.contains("tok_live_e2e_51a2b3"), "{log}");
    assert!(
        log.contains("plain=***") && log.contains("b64=***") && log.contains("b64x2=***"),
        "{log}"
    );
}

/// `shards: N` runs a task as N groups, each told its shard by `PIT_SHARD`.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn sharded_tasks_run_each_shard_once() {
    let pit = start().await;
    let repo = tempfile::tempdir().unwrap();
    write(
        repo.path(),
        "pit.config.json",
        r#"{"tasks":{"test":{"run":"echo shard=$PIT_SHARD/$PIT_SHARD_COUNT","shards":3,"cache":false}}}"#,
    );
    sh(
        repo.path(),
        "git init -q -b main && git add -A && git commit -qm init",
    );
    let view = run(&pit, repo.path()).await;
    assert_eq!(view.status, Status::Passed);
    let mut seen = vec![];
    for g in &view.groups {
        let log = pit
            .client
            .get_text(&format!("/api/tasks/{}/log", g.tasks[0].id))
            .await
            .unwrap();
        seen.push((g.name.clone(), log.trim().to_string()));
    }
    seen.sort();
    assert_eq!(
        seen,
        vec![
            ("default#1".to_string(), "shard=1/3".to_string()),
            ("default#2".to_string(), "shard=2/3".to_string()),
            ("default#3".to_string(), "shard=3/3".to_string())
        ]
    );
}

/// A branch run cannot poison what main uses: its cached result for the key
/// main computes and the files it leaves in its workspace are both invisible
/// to trusted (merge queue) runs.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn untrusted_runs_cannot_poison_trusted_ones() {
    let pit = start().await;
    let tmp = tempfile::tempdir().unwrap();
    // Canonical, so the local run and the queue see one repo URL (macOS
    // temp dirs sit behind the /var -> /private/var symlink).
    let root = tmp.path().canonicalize().unwrap();
    let origin = root.join("origin.git");
    let work = root.join("work");
    std::fs::create_dir_all(&work).unwrap();
    sh(&root, "git init -q --bare -b main origin.git");
    // The build script is not an input, so a branch that edits it gets the
    // key main will compute.
    let config = serde_json::json!({ "tasks": {
        "build": { "run": "sh build.sh", "inputs": ["src/**"], "outputs": ["dist/**"] },
        "check": { "run": "cat dist/out; test ! -f junk/planted", "deps": ["build"], "cache": false },
    } });
    write(&work, "pit.config.json", &config.to_string());
    write(&work, "src/a.txt", "a\n");
    write(
        &work,
        "build.sh",
        "mkdir -p dist && echo clean > dist/out\n",
    );
    write(&work, ".gitignore", "dist/\njunk/\n");
    sh(&work, "git init -q -b main && git add -A && git commit -qm init && git remote add origin ../origin.git && git push -q origin main");
    sh(&work, "git checkout -q -b evil");
    write(
        &work,
        "build.sh",
        "mkdir -p dist junk && echo POISONED > dist/out && touch junk/planted\n",
    );
    sh(&work, "git commit -qam evil");
    let evil = run(&pit, &work).await;
    assert_eq!(
        evil.status,
        Status::Failed,
        "the planted file is seen by the branch itself"
    );
    sh(&work, "git checkout -q main && git checkout -q -b docs && echo d > README && git add -A && git commit -qm docs && git push -q origin docs");

    let url = origin.to_string_lossy().to_string();
    let _: QueueEntryView = pit
        .client
        .post(
            "/api/queue",
            &QueueAdd {
                repo: "trust".into(),
                url: Some(url),
                branch: Some("docs".into()),
                pr: None,
                target: None,
                sha: None,
            },
        )
        .await
        .unwrap();
    let entry = tokio::time::timeout(Duration::from_secs(60), async {
        loop {
            let entries: Vec<QueueEntryView> = pit.client.get("/api/queue").await.unwrap();
            let e = entries.into_iter().find(|e| e.source == "docs").unwrap();
            if matches!(e.status.as_str(), "landed" | "failed") {
                return e;
            }
            tokio::time::sleep(Duration::from_millis(200)).await;
        }
    })
    .await
    .expect("queue did not settle");
    assert_eq!(entry.status, "landed", "{entry:?}");
    let queued: RunView = pit
        .client
        .get(&format!("/api/runs/{}", entry.run_id.unwrap()))
        .await
        .unwrap();
    let build = queued.groups[0]
        .tasks
        .iter()
        .find(|t| t.name == "build")
        .unwrap();
    assert_eq!(
        build.status,
        Status::Passed,
        "trusted run reused a branch's result: {build:?}"
    );
    assert!(
        build
            .message
            .as_deref()
            .unwrap_or_default()
            .contains("untrusted run"),
        "{build:?}"
    );
}

/// Trust cannot be borrowed: claiming needs the worker token when one is set,
/// and a cache request with a lease no running group holds is refused rather
/// than served untrusted results.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn trust_needs_a_worker_token_and_a_live_lease() {
    let pit = start().await;
    *pit.app.worker_token.lock() = Some("w-secret".into());
    let info = serde_json::json!({ "name": "rogue", "os": "linux", "arch": "x86_64", "cpus": 1, "backends": ["host"], "kvm": false });
    let base = &pit.client.base;
    let http = reqwest::Client::new();
    let claim = |token: Option<&'static str>| {
        let mut r = http.post(format!("{base}/api/workers/claim")).json(&info);
        if let Some(t) = token {
            r = r.header(pit::client::WORKER_TOKEN_HEADER, t);
        }
        r.send()
    };
    assert_eq!(claim(None).await.unwrap().status(), 403);
    assert_eq!(claim(Some("wrong")).await.unwrap().status(), 403);
    let dead = http
        .get(format!("{base}/api/cache/{}", "0".repeat(64)))
        .header(pit::client::LEASE_HEADER, "not-a-live-lease")
        .send()
        .await
        .unwrap();
    assert_eq!(dead.status(), 409);
}

/// A file an earlier commit generated (ignored, so it survives in a warm
/// workspace) hides a bug: the warm run passes, a fresh run fails, and the
/// fresh workspace is deleted afterwards.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn fresh_runs_do_not_see_files_earlier_runs_left_behind() {
    let pit = start().await;
    let repo = tempfile::tempdir().unwrap();
    let dir = repo.path();
    let config = serde_json::json!({ "tasks": {
        "setup": { "run": "sh gen.sh", "inputs": ["gen.sh"], "cache": "workspace" },
        "test": { "run": "test -f gen/legacy", "deps": ["setup"], "cache": false },
    } });
    write(dir, "pit.config.json", &config.to_string());
    write(dir, ".gitignore", "gen/\n");
    write(
        dir,
        "gen.sh",
        "mkdir -p gen && touch gen/current gen/legacy\n",
    );
    sh(
        dir,
        "git init -q -b main && git add -A && git commit -qm init",
    );
    assert_eq!(run(&pit, dir).await.status, Status::Passed);
    // The generator stops producing gen/legacy; the test still needs it.
    write(dir, "gen.sh", "mkdir -p gen && touch gen/current\n");
    sh(dir, "git commit -qam 'stop generating legacy'");
    assert_eq!(
        run(&pit, dir).await.status,
        Status::Passed,
        "warm state hides the bug"
    );
    let fresh = run_with(
        &pit,
        dir,
        pit::cli::RunOpts {
            no_cache: true,
            fresh: true,
            debug: false,
        },
    )
    .await;
    assert_eq!(fresh.status, Status::Failed, "a fresh workspace exposes it");
    // Deleted in the background once the group finished.
    let root = pit._data.path().join("worker/workspaces");
    let left = || {
        std::fs::read_dir(&root)
            .into_iter()
            .flatten()
            .flatten()
            .flat_map(|repo| {
                std::fs::read_dir(repo.path())
                    .into_iter()
                    .flatten()
                    .flatten()
            })
            .filter(|g| g.file_name().to_string_lossy().ends_with("~fresh"))
            .flat_map(|g| std::fs::read_dir(g.path()).into_iter().flatten().flatten())
            .count()
    };
    for _ in 0..50 {
        if left() == 0 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert_eq!(left(), 0, "the fresh workspace was not deleted");
}

/// A schedule in the config on the default branch starts one trusted run per
/// matching minute, with its options, on the branch tip.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn schedules_start_one_run_per_slot() {
    let pit = start().await;
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().canonicalize().unwrap();
    let work = root.join("work");
    std::fs::create_dir_all(&work).unwrap();
    sh(&root, "git init -q --bare -b main origin.git");
    let config = serde_json::json!({
        "tasks": { "check": { "run": "true", "cache": false } },
        "schedules": {
            "nightly": { "cron": "17 3,4 * * *", "fresh": true, "noCache": true },
            // 12:17 in Tokyo is 03:17 UTC; 03:17 in Tokyo is not.
            "tokyo": { "cron": "17 12 * * *", "timezone": "Asia/Tokyo" },
            "tokyo-night": { "cron": "17 3 * * *", "timezone": "Asia/Tokyo" },
        },
    });
    write(&work, "pit.config.json", &config.to_string());
    sh(&work, "git init -q -b main && git add -A && git commit -qm init && git remote add origin ../origin.git && git push -q origin main");
    // Registers the repo and its mirror.
    assert_eq!(run(&pit, &work).await.status, Status::Passed);

    let at = |h: i64, m: i64| (20_377 * 1440 + h * 60 + m) * 60_000; // 2025-10-16

    pit::server::schedule::tick(&pit.app, at(3, 16))
        .await
        .unwrap();
    assert_eq!(count_trigger(&pit, "nightly").await, 0, "not due yet");
    pit::server::schedule::tick(&pit.app, at(3, 17))
        .await
        .unwrap();
    pit::server::schedule::tick(&pit.app, at(3, 17) + 30_000)
        .await
        .unwrap();
    assert_eq!(count_trigger(&pit, "nightly").await, 1, "one run per slot");
    assert_eq!(count_trigger(&pit, "tokyo").await, 1, "12:17 in Tokyo");
    assert_eq!(
        count_trigger(&pit, "tokyo-night").await,
        0,
        "03:17 UTC is noon in Tokyo"
    );
    // A slow tick that skips over 04:17 still fires it on the next one.
    pit::server::schedule::tick(&pit.app, at(4, 15))
        .await
        .unwrap();
    pit::server::schedule::tick(&pit.app, at(4, 19))
        .await
        .unwrap();
    assert_eq!(
        count_trigger(&pit, "nightly").await,
        2,
        "the missed minute was caught up"
    );

    // A bad cron is a config error, like any other typo.
    let bad = serde_json::json!({
        "tasks": { "check": { "run": "true" } },
        "schedules": { "nightly": { "cron": "17 3 * *" } },
    });
    write(&work, "pit.config.json", &bad.to_string());
    let view = run(&pit, &work).await;
    assert_eq!(view.status, Status::Error, "{view:?}");
    assert!(view.message.unwrap_or_default().contains("five fields"));
}

/// Main (here: a schedule) breaking and being fixed posts one message each to
/// the notify webhook; staying broken posts nothing.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn notifies_when_a_trusted_line_breaks_and_is_fixed() {
    let pit = start().await;
    // A webhook receiver that records what it is sent.
    let got: std::sync::Arc<parking_lot::Mutex<Vec<serde_json::Value>>> = Default::default();
    let sink = got.clone();
    let app = axum::Router::new().route(
        "/hook",
        axum::routing::post(move |axum::Json(v): axum::Json<serde_json::Value>| {
            let sink = sink.clone();
            async move {
                sink.lock().push(v);
                "ok"
            }
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let hook = format!("http://{}/hook", listener.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(listener, app).await });
    *pit.app.notify_url.lock() = Some(hook);

    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().canonicalize().unwrap();
    let work = root.join("work");
    std::fs::create_dir_all(&work).unwrap();
    sh(&root, "git init -q --bare -b main origin.git");
    let config = |run: &str| {
        serde_json::json!({
            "tasks": { "check": { "run": run, "cache": false } },
            "schedules": { "nightly": { "cron": "* * * * *" } },
        })
        .to_string()
    };
    write(&work, "pit.config.json", &config("exit 1"));
    sh(&work, "git init -q -b main && git add -A && git commit -qm broken && git remote add origin ../origin.git && git push -q origin main");
    run(&pit, &work).await; // registers the repo and its mirror; local runs never notify

    let mut minute = 29_000_000_i64;
    let mut scheduled_run = || {
        minute += 1;
        let app = pit.app.clone();
        let client = pit.client.clone();
        async move {
            let before: Vec<RunView> = client.get("/api/runs?limit=100").await.unwrap();
            let n = before
                .iter()
                .filter(|r| r.trigger == "schedule:nightly")
                .count();
            pit::server::schedule::tick(&app, minute * 60_000)
                .await
                .unwrap();
            tokio::time::timeout(Duration::from_secs(30), async {
                loop {
                    let runs: Vec<RunView> = client.get("/api/runs?limit=100").await.unwrap();
                    let sched: Vec<&RunView> = runs
                        .iter()
                        .filter(|r| r.trigger == "schedule:nightly")
                        .collect();
                    if sched.len() > n && sched.iter().all(|r| r.status.is_done()) {
                        return;
                    }
                    tokio::time::sleep(Duration::from_millis(100)).await;
                }
            })
            .await
            .expect("scheduled run did not finish");
            // The webhook is posted after the run is marked finished.
            tokio::time::sleep(Duration::from_millis(300)).await;
        }
    };
    scheduled_run().await;
    scheduled_run().await;
    write(&work, "pit.config.json", &config("true"));
    sh(&work, "git commit -qam fixed && git push -q origin main");
    scheduled_run().await;
    let got = got.lock().clone();
    let events: Vec<&str> = got.iter().map(|v| v["event"].as_str().unwrap()).collect();
    assert_eq!(events, ["broken", "fixed"], "{got:?}");
    assert_eq!(got[0]["failed"][0], "default.check");
    assert!(got[0]["text"].as_str().unwrap().contains("is broken"));
}

/// Rerunning after a failure reruns only the failed task, and says it ran
/// because it failed last time with the same inputs.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_rerun_reruns_only_what_failed_and_says_why() {
    let pit = start().await;
    let repo = tempfile::tempdir().unwrap();
    let dir = repo.path();
    let down = dir.join("..").join(format!("down-{}", std::process::id()));
    std::fs::write(&down, "").unwrap();
    let config = serde_json::json!({ "tasks": {
        "build": { "run": "mkdir -p dist && echo x > dist/x", "outputs": ["dist/**"] },
        "test": { "run": format!("test ! -f {}", down.display()), "deps": ["build"] },
    } });
    write(dir, "pit.config.json", &config.to_string());
    write(dir, ".gitignore", "dist/\n");
    sh(
        dir,
        "git init -q -b main && git add -A && git commit -qm init",
    );
    assert_eq!(run(&pit, dir).await.status, Status::Failed);
    std::fs::remove_file(&down).unwrap();
    let view = run(&pit, dir).await;
    assert_eq!(view.status, Status::Passed);
    let task = |n: &str| {
        view.groups[0]
            .tasks
            .iter()
            .find(|t| t.name == n)
            .unwrap()
            .clone()
    };
    assert_eq!(task("build").status, Status::Cached);
    let msg = task("test").message.unwrap_or_default();
    assert!(msg.contains("failed with these same inputs"), "{msg}");
}

/// Shards that start together need the same build. It runs once; the other
/// shards wait for it and restore its outputs. If the first builder fails,
/// a waiter takes over instead of waiting forever.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_identical_tasks_run_once() {
    let pit = start().await;
    let repo = tempfile::tempdir().unwrap();
    let dir = repo.path();
    let log = dir
        .join("..")
        .join(format!("builds-{}", std::process::id()));
    let config = |fail_first: bool| {
        let fail = if fail_first {
            // Fails on the first attempt only.
            format!(
                "if [ $(wc -l < {0}) -eq 1 ]; then exit 1; fi; ",
                log.display()
            )
        } else {
            String::new()
        };
        serde_json::json!({ "tasks": {
            "build": {
                "run": format!("echo b >> {}; sleep 2; {fail}mkdir -p dist && echo x > dist/x", log.display()),
                "inputs": ["src/**"],
                "outputs": ["dist/**"],
            },
            "test": { "run": "test -f dist/x", "deps": ["build"], "shards": 3, "cache": false },
        } })
        .to_string()
    };
    write(dir, "pit.config.json", &config(false));
    write(dir, "src/a", "1\n");
    write(dir, ".gitignore", "dist/\n");
    sh(
        dir,
        "git init -q -b main && git add -A && git commit -qm init",
    );
    std::fs::write(&log, "").unwrap();
    let view = run(&pit, dir).await;
    assert_eq!(view.status, Status::Passed);
    let builds = || std::fs::read_to_string(&log).unwrap().lines().count();
    assert_eq!(builds(), 1, "the build ran once for three shards");
    let waited = view
        .groups
        .iter()
        .flat_map(|g| &g.tasks)
        .filter(|t| t.message.as_deref().unwrap_or_default().contains("waited"))
        .count();
    assert!(waited >= 1, "{view:?}");

    // The first builder fails: a waiter must take over, not hang.
    write(dir, "pit.config.json", &config(true));
    write(dir, "src/a", "2\n");
    sh(dir, "git commit -qam two");
    std::fs::write(&log, "").unwrap();
    let view = run(&pit, dir).await;
    assert!(
        builds() >= 2,
        "a waiter took over after the first build failed"
    );
    assert!(view.status.is_done());
}

/// A pull request from a fork never runs on its own. A maintainer's
/// `pit:run` label runs that commit, in a throwaway workspace, without the
/// secrets or worker variables its config asks for.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn fork_pull_requests_run_on_a_label_without_secrets() {
    use hmac::Mac;
    std::env::set_var("FORK_E2E_WORKER_SECRET", "hunter2-very-secret");
    let data = tempfile::tempdir().unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    // Statuses go to an address nobody listens on; reporting just fails.
    let github =
        pit::server::github::GitHub::with_token("http://127.0.0.1:9", "t", Some("s3cret".into()))
            .unwrap();
    let app = pit::server::App::with_github(
        &data.path().join("server"),
        None,
        base.clone(),
        Some(github),
    )
    .unwrap();
    tokio::spawn(pit::server::serve_listener(app.clone(), listener));
    tokio::spawn(pit::worker::run(pit::worker::WorkerOpts {
        server: base.clone(),
        token: None,
        name: Some("fork".into()),
        data_dir: data.path().join("worker"),
        slots: 2,
        parallelism: 2,
        workspace_max_bytes: 0,
        worker_token: None,
        secrets_file: None,
    }));
    let repo = tempfile::tempdir().unwrap();
    // The fork's config asks for the worker's secret and tries to print it.
    let config = serde_json::json!({ "tasks": { "steal": {
        "run": "test -z \"$FORK_E2E_WORKER_SECRET\"",
        "secrets": ["FORK_E2E_WORKER_SECRET"],
        "env": ["FORK_E2E_WORKER_SECRET"],
        "cache": false,
    } } });
    write(repo.path(), "pit.config.json", &config.to_string());
    sh(
        repo.path(),
        "git init -q -b main && git add -A && git commit -qm fork",
    );
    let head = || sh(repo.path(), "git rev-parse HEAD").trim().to_string();
    let sha = head();
    let post = |event: &'static str, payload: serde_json::Value| {
        let body = payload.to_string();
        let mut mac = hmac::Hmac::<sha2::Sha256>::new_from_slice(b"s3cret").unwrap();
        mac.update(body.as_bytes());
        let sig = format!("sha256={}", hex::encode(mac.finalize().into_bytes()));
        let base = base.clone();
        async move {
            reqwest::Client::new()
                .post(format!("{base}/webhooks/github"))
                .header("x-github-event", event)
                .header("x-hub-signature-256", sig)
                .body(body)
                .send()
                .await
                .unwrap()
                .json::<serde_json::Value>()
                .await
                .unwrap()
        }
    };
    let repository = serde_json::json!({ "full_name": "acme/widgets", "clone_url": repo.path().to_string_lossy(), "default_branch": "main" });
    let pr_event = |action: &str, label: Option<&str>, sha: &str| {
        let mut payload = serde_json::json!({
            "action": action,
            "repository": repository,
            "pull_request": {
                "number": 7,
                "head": { "sha": sha, "repo": { "full_name": "mallory/widgets" } },
                "base": { "ref": "main" },
            },
        });
        if let Some(l) = label {
            payload["label"] = serde_json::json!({ "name": l });
        }
        post("pull_request", payload)
    };
    let send = |action: &str, label: Option<&str>| pr_event(action, label, &sha);
    let opened = send("opened", None).await;
    assert!(
        opened["ignored"].as_str().unwrap().contains("pit:run"),
        "{opened}"
    );
    let labeled = send("labeled", Some("pit:run")).await;
    let run_id = labeled["run"]
        .as_str()
        .expect("the label starts a run")
        .to_string();
    let client = Client::new(&base, None).unwrap();
    // Fork code only runs in a container: without Docker it waits for a
    // worker that has it.
    if !pit::worker::exec::detect_backends()
        .await
        .contains(&pit::model::Backend::Docker)
    {
        let waiting = tokio::time::timeout(Duration::from_secs(30), async {
            loop {
                let v: serde_json::Value =
                    client.get(&format!("/api/runs/{run_id}")).await.unwrap();
                if v.to_string().contains("backend docker") {
                    return v;
                }
                tokio::time::sleep(Duration::from_millis(300)).await;
            }
        })
        .await
        .expect("the fork's run should wait for a Docker worker");
        assert!(waiting.to_string().contains("waiting"), "{waiting}");
        return;
    }
    let view = tokio::time::timeout(
        Duration::from_secs(180),
        pit::cli::wait(&client, &run_id, |_| {}),
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(
        view.status,
        Status::Passed,
        "the fork saw the secret: {view:?}"
    );
    // GitHub's "Re-run" on the fork's check keeps the fork's restrictions.
    let rerun = post(
        "check_run",
        serde_json::json!({ "action": "rerequested", "repository": repository, "check_run": { "head_sha": sha } }),
    )
    .await;
    let rerun = rerun["run"].as_str().expect("a rerun").to_string();
    let row = app.db.run(&rerun).unwrap();
    assert!(
        row.no_secrets && row.fresh,
        "the rerun lost the fork's limits"
    );
    let view = tokio::time::timeout(
        Duration::from_secs(180),
        pit::cli::wait(&client, &rerun, |_| {}),
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(
        view.status,
        Status::Passed,
        "the rerun saw the secret: {view:?}"
    );
    // A commit Pit never ran is not rerun.
    let unknown = post(
        "check_run",
        serde_json::json!({ "action": "rerequested", "repository": repository, "check_run": { "head_sha": "0".repeat(40) } }),
    )
    .await;
    assert!(unknown["ignored"].is_string(), "{unknown}");
    // Queued with the merge label, the fork is queued at the labeled commit;
    // a later push drops it rather than testing new code with the queue's
    // trust.
    let slow = serde_json::json!({ "tasks": { "slow": { "run": "sleep 20", "cache": false } } });
    write(repo.path(), "pit.config.json", &slow.to_string());
    sh(
        repo.path(),
        "git commit -qam slow && git update-ref refs/pull/7/head HEAD",
    );
    let queued = pr_event("labeled", Some("pit:merge"), &head()).await;
    let entry = queued["queued"].as_str().expect("queued").to_string();
    sh(
        repo.path(),
        "git commit -q --allow-empty -m sneaky && git update-ref refs/pull/7/head HEAD",
    );
    let removed = tokio::time::timeout(Duration::from_secs(60), async {
        loop {
            let entries: Vec<QueueEntryView> = client.get("/api/queue").await.unwrap();
            if let Some(e) = entries
                .into_iter()
                .find(|e| e.id == entry && e.status == "removed")
            {
                return e;
            }
            tokio::time::sleep(Duration::from_millis(300)).await;
        }
    })
    .await
    .expect("the fork's entry stayed queued after a push");
    assert!(
        removed
            .reason
            .as_deref()
            .unwrap_or_default()
            .contains("new commits"),
        "{removed:?}"
    );
}

/// Flaky tasks are found from history (passed on a retry, or the same inputs
/// both passed and failed), and a quarantined failure does not fail the run.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn flaky_tasks_are_reported_and_quarantine_keeps_runs_green() {
    let pit = start().await;
    let repo = tempfile::tempdir().unwrap();
    let dir = repo.path();
    let marks = dir
        .join("..")
        .join(format!("flaky-marks-{}", std::process::id()));
    std::fs::create_dir_all(&marks).unwrap();
    let config = serde_json::json!({ "tasks": {
        // Fails the first attempt of every run, passes the retry.
        "retried": {
            "run": format!("f={}/$PIT_RUN_ID; if [ -f $f ]; then exit 0; else touch $f; exit 1; fi", marks.display()),
            "retries": 1,
            "cache": false,
        },
        // Same inputs every run; passes, fails, passes.
        "flips": {
            "run": format!(
                "c={}/../flips-count; n=$(cat $c 2>/dev/null || echo 0); echo $((n + 1)) > $c; [ $((n % 2)) -eq 0 ]",
                marks.display()
            ),
            "cache": false,
        },
        "broken": { "run": "exit 3", "quarantine": true, "cache": false },
    } });
    write(dir, "pit.config.json", &config.to_string());
    sh(
        dir,
        "git init -q -b main && git add -A && git commit -qm init",
    );
    for _ in 0..3 {
        let view = run(&pit, dir).await;
        let broken = view.groups[0]
            .tasks
            .iter()
            .find(|t| t.name == "broken")
            .unwrap();
        assert_eq!(broken.status, Status::Failed);
        assert!(broken
            .message
            .as_deref()
            .unwrap_or_default()
            .starts_with("quarantined"));
    }
    let flaky: Vec<FlakyTask> = pit.client.get("/api/flaky?days=1").await.unwrap();
    let get = |n: &str| flaky.iter().find(|t| t.task == n).cloned();
    let retried = get("retried").expect("a task that passed on a retry is flaky");
    assert_eq!(retried.flaky_passes, 3);
    let flips = get("flips").expect("same inputs, different outcomes is flaky");
    assert_eq!(flips.mixed_inputs, 1, "{flips:?}");
    assert!(
        get("broken").is_none(),
        "always failing is broken, not flaky"
    );
    // A task that needed the quarantined one did not run, and that fails the
    // run: quarantine hides one flaky task, not everything after it.
    let config = serde_json::json!({ "tasks": {
        "broken": { "run": "exit 3", "quarantine": true, "cache": false },
        "after": { "run": "true", "deps": ["broken"], "cache": false },
    } });
    write(dir, "pit.config.json", &config.to_string());
    sh(dir, "git commit -qam dependent");
    let view = run(&pit, dir).await;
    let after = view.groups[0]
        .tasks
        .iter()
        .find(|t| t.name == "after")
        .unwrap();
    assert_eq!(after.status, Status::Skipped);
    assert_eq!(view.status, Status::Failed);
}

/// A task's JUnit reports name the failed tests in its result, and a report
/// left in the warm workspace by an earlier run is never read again.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn junit_reports_name_failed_tests() {
    let pit = start().await;
    let repo = tempfile::tempdir().unwrap();
    let dir = repo.path();
    let failing = r#"<testsuite><testcase classname="cart" name="adds"/><testcase classname="cart" name="pays"><failure message="card declined"/></testcase></testsuite>"#;
    let config = |run: String| {
        serde_json::json!({ "tasks": { "test": { "run": run, "reports": ["out/*.xml"], "cache": false } } })
            .to_string()
    };
    write(dir, ".gitignore", "out/\n");
    write(
        dir,
        "pit.config.json",
        &config(format!(
            "mkdir -p out && echo '{failing}' > out/junit.xml && exit 1"
        )),
    );
    sh(
        dir,
        "git init -q -b main && git add -A && git commit -qm init",
    );
    let view = run(&pit, dir).await;
    let msg = view.groups[0].tasks[0].message.clone().unwrap_or_default();
    assert!(
        msg.contains("1 of 2 tests failed: cart.pays (card declined)"),
        "{msg}"
    );
    // Fails again without writing a report: the old one must not be blamed.
    write(dir, "pit.config.json", &config("exit 1".into()));
    sh(dir, "git commit -qam again");
    let view = run(&pit, dir).await;
    let msg = view.groups[0].tasks[0].message.clone().unwrap_or_default();
    assert!(!msg.contains("cart.pays"), "stale report read: {msg}");
}

/// While workers are busy, queued changes in one lane are batched: only the
/// newest candidate runs. A bad change inside the batch is still found (the
/// batched changes get their own runs) and ejected; the rest land.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn merge_queue_batches_waiting_changes_and_bisects_failures() {
    let pit = start().await;
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().canonicalize().unwrap();
    let origin = root.join("origin.git");
    let work = root.join("work");
    std::fs::create_dir_all(&work).unwrap();
    sh(&root, "git init -q --bare -b main origin.git");
    let mut groups = serde_json::Map::new();
    for i in 0..4 {
        groups.insert(
            format!("busy{i}"),
            serde_json::json!({ "tasks": { "wait": { "run": "sleep 3", "inputs": ["busy/**"], "cache": false } } }),
        );
    }
    groups.insert(
        "ci".into(),
        serde_json::json!({ "tasks": { "integration": { "run": "! grep -rq BAD shared/", "inputs": ["shared/**"], "cache": false } } }),
    );
    write(
        &work,
        "pit.config.json",
        &serde_json::json!({ "groups": groups }).to_string(),
    );
    write(&work, "shared/base", "0\n");
    write(&work, "busy/x", "0\n");
    sh(&work, "git init -q -b main && git add -A && git commit -qm init && git remote add origin ../origin.git && git push -q origin main");
    for (b, body) in [("a", "1"), ("b", "BAD"), ("c", "3")] {
        sh(&work, &format!("git checkout -q -b {b} main && echo {body} > shared/{b} && git add -A && git commit -qm {b} && git push -q origin {b} && git checkout -q main"));
    }
    // Occupy all four worker slots so queued candidates cannot start yet.
    let _busy = pit::cli::submit(&pit.client, &work, vec![], Default::default())
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_millis(500)).await;
    let url = origin.to_string_lossy().to_string();
    for b in ["a", "b", "c"] {
        let _: QueueEntryView = pit
            .client
            .post(
                "/api/queue",
                &QueueAdd {
                    repo: "batch".into(),
                    url: Some(url.clone()),
                    branch: Some(b.into()),
                    pr: None,
                    target: None,
                    sha: None,
                },
            )
            .await
            .unwrap();
    }
    let entries = tokio::time::timeout(Duration::from_secs(90), async {
        loop {
            let entries: Vec<QueueEntryView> = pit.client.get("/api/queue").await.unwrap();
            let ours: Vec<_> = entries
                .into_iter()
                .filter(|e| ["a", "b", "c"].contains(&e.source.as_str()))
                .collect();
            if ours.len() == 3
                && ours
                    .iter()
                    .all(|e| matches!(e.status.as_str(), "landed" | "failed"))
            {
                return ours;
            }
            tokio::time::sleep(Duration::from_millis(200)).await;
        }
    })
    .await
    .expect("queue did not settle");
    let status = |s: &str| {
        entries
            .iter()
            .find(|e| e.source == s)
            .unwrap()
            .status
            .clone()
    };
    assert_eq!(
        (
            status("a").as_str(),
            status("b").as_str(),
            status("c").as_str()
        ),
        ("landed", "failed", "landed"),
        "{entries:?}"
    );
    let runs: Vec<RunView> = pit.client.get("/api/runs?limit=50").await.unwrap();
    assert!(
        runs.iter().any(|r| r.status == Status::Skipped
            && r.message
                .as_deref()
                .unwrap_or_default()
                .starts_with("batched into")),
        "nothing was batched"
    );
    assert!(sh(&origin, "git show main:shared/b 2>/dev/null || echo absent").contains("absent"));
}

/// A change batched into a later one must not be stranded when that later
/// change leaves the queue: it gets its own run and lands.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_change_batched_into_a_removed_one_still_lands() {
    let pit = start().await;
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().canonicalize().unwrap();
    let origin = root.join("origin.git");
    let work = root.join("work");
    std::fs::create_dir_all(&work).unwrap();
    sh(&root, "git init -q --bare -b main origin.git");
    let mut groups = serde_json::Map::new();
    for i in 0..4 {
        groups.insert(
            format!("busy{i}"),
            serde_json::json!({ "tasks": { "wait": { "run": "sleep 3", "inputs": ["busy/**"], "cache": false } } }),
        );
    }
    groups.insert(
        "ci".into(),
        serde_json::json!({ "tasks": { "integration": { "run": "! grep -rq BAD shared/", "inputs": ["shared/**"], "cache": false } } }),
    );
    write(
        &work,
        "pit.config.json",
        &serde_json::json!({ "groups": groups }).to_string(),
    );
    write(&work, "shared/base", "0\n");
    write(&work, "busy/x", "0\n");
    sh(&work, "git init -q -b main && git add -A && git commit -qm init && git remote add origin ../origin.git && git push -q origin main");
    for (b, body) in [("a", "1"), ("b", "BAD"), ("c", "3")] {
        sh(&work, &format!("git checkout -q -b {b} main && echo {body} > shared/{b} && git add -A && git commit -qm {b} && git push -q origin {b} && git checkout -q main"));
    }
    // Occupy all four worker slots so queued candidates cannot start yet.
    let _busy = pit::cli::submit(&pit.client, &work, vec![], Default::default())
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_millis(500)).await;
    let url = origin.to_string_lossy().to_string();
    let mut ids = std::collections::HashMap::new();
    for b in ["a", "c"] {
        let entry: QueueEntryView = pit
            .client
            .post(
                "/api/queue",
                &QueueAdd {
                    repo: "batch".into(),
                    url: Some(url.clone()),
                    branch: Some(b.into()),
                    pr: None,
                    target: None,
                    sha: None,
                },
            )
            .await
            .unwrap();
        ids.insert(b, entry.id);
    }
    // `a` was batched into `c`; `c` leaves the queue before it is tested.
    let _: serde_json::Value = pit
        .client
        .delete(&format!("/api/queue/{}", ids["c"]))
        .await
        .unwrap();
    let entries = tokio::time::timeout(Duration::from_secs(90), async {
        loop {
            let entries: Vec<QueueEntryView> = pit.client.get("/api/queue").await.unwrap();
            let ours: Vec<_> = entries.into_iter().filter(|e| e.source == "a").collect();
            if ours
                .iter()
                .any(|e| matches!(e.status.as_str(), "landed" | "failed"))
            {
                return ours;
            }
            tokio::time::sleep(Duration::from_millis(200)).await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("queue did not settle"));
    assert_eq!(entries[0].status, "landed", "{entries:?}");
}

/// OIDC tokens go only to trusted runs of repos GitHub vouched for, on a
/// server started with `--oidc`: a push to main gets one a cloud would accept
/// (verified against the public JWKS), while a `pit run` and a run forged
/// through the API to look like main get none.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn oidc_tokens_only_for_trusted_github_runs() {
    use hmac::Mac;
    let data = tempfile::tempdir().unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let github =
        pit::server::github::GitHub::with_token("http://127.0.0.1:9", "t", Some("s3cret".into()))
            .unwrap();
    let app = pit::server::App::with_github(
        &data.path().join("server"),
        Some("api-tok".into()),
        base.clone(),
        Some(github),
    )
    .unwrap();
    // Off until enabled, and refused without a worker token.
    assert!(app.oidc.get().is_none());
    assert!(app.enable_oidc().is_err());
    *app.worker_token.lock() = Some("w-tok".into());
    app.enable_oidc().unwrap();
    tokio::spawn(pit::server::serve_listener(app, listener));
    tokio::spawn(pit::worker::run(pit::worker::WorkerOpts {
        server: base.clone(),
        token: Some("api-tok".into()),
        name: Some("oidc".into()),
        data_dir: data.path().join("worker"),
        slots: 2,
        parallelism: 2,
        workspace_max_bytes: 0,
        worker_token: Some("w-tok".into()),
        secrets_file: None,
    }));
    let client = Client::new(&base, Some("api-tok".into())).unwrap();

    let repo = tempfile::tempdir().unwrap();
    let out = repo
        .path()
        .join("..")
        .join(format!("oidc-{}.jwt", std::process::id()));
    let config = serde_json::json!({ "tasks": { "deploy": {
        "run": format!("printf %s \"$PIT_OIDC_TOKEN\" > {}; echo token: $PIT_OIDC_TOKEN", out.display()),
        "oidc": "sts.amazonaws.com",
        "cache": false,
    } } });
    write(repo.path(), "pit.config.json", &config.to_string());
    sh(
        repo.path(),
        "git init -q -b main && git add -A && git commit -qm init",
    );
    let sha = sh(repo.path(), "git rev-parse HEAD").trim().to_string();

    // A push to main, signed by GitHub: trusted, verified repo.
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
    let resp: serde_json::Value = http
        .post(format!("{base}/webhooks/github"))
        .header("x-github-event", "push")
        .header("x-hub-signature-256", sig)
        .body(payload)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let run_id = resp["run"].as_str().unwrap().to_string();
    let view = tokio::time::timeout(
        Duration::from_secs(30),
        pit::cli::wait(&client, &run_id, |_| {}),
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(view.status, Status::Passed, "{view:?}");
    let token = std::fs::read_to_string(&out).unwrap();

    // Verify the way a cloud does: discovery, then the JWKS, then the token.
    let discovery: serde_json::Value = http
        .get(format!("{base}/.well-known/openid-configuration"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(discovery["issuer"], base);
    let jwks: jsonwebtoken::jwk::JwkSet = http
        .get(discovery["jwks_uri"].as_str().unwrap())
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let key = jsonwebtoken::DecodingKey::from_jwk(&jwks.keys[0]).unwrap();
    let mut v = jsonwebtoken::Validation::new(jsonwebtoken::Algorithm::ES256);
    v.set_audience(&["sts.amazonaws.com"]);
    v.set_issuer(&[base.as_str()]);
    let claims = jsonwebtoken::decode::<serde_json::Value>(&token, &key, &v)
        .unwrap()
        .claims;
    assert_eq!(claims["sub"], "repo:acme/widgets:ref:refs/heads/main");
    assert_eq!(claims["trusted"], true);
    let log = client
        .get_text(&format!("/api/runs/{run_id}/tasks/deploy/log"))
        .await
        .unwrap();
    assert!(log.contains("token: ***") && !log.contains(&token), "{log}");

    // `pit run` of the same repo: untrusted, no token, so the task errors.
    let sub = pit::cli::submit(&client, repo.path(), vec![], Default::default())
        .await
        .unwrap();
    let local = pit::cli::wait(&client, &sub.run_id, |_| {}).await.unwrap();
    let msg = local.groups[0].tasks[0].message.clone().unwrap_or_default();
    assert!(msg.contains("only issued to trusted runs"), "{msg}");

    // The review's forgery: an API run dressed up as main.
    let forged: serde_json::Value = client
        .post(
            "/api/runs",
            &serde_json::json!({ "repo": "acme/widgets", "sha": sha, "trigger": "ref", "ref": "refs/heads/main" }),
        )
        .await
        .unwrap();
    let forged = pit::cli::wait(&client, forged["id"].as_str().unwrap(), |_| {})
        .await
        .unwrap();
    assert_ne!(
        forged.status,
        Status::Passed,
        "a forged run got a token: {forged:?}"
    );
}

/// `shardFiles`: every matching file is dealt to exactly one shard, each
/// shard gets its list in `PIT_SHARD_FILES` (and `PIT_SHARD_FILES_LIST`), and
/// a shard dealt nothing passes without running its command (which would
/// otherwise run every test). Two groups' `test` tasks are split separately,
/// and globs that match nothing fail the run instead of passing empty.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn shard_files_are_dealt_once_each() {
    let pit = start().await;
    let repo = tempfile::tempdir().unwrap();
    let dir = repo.path();
    let log = |g: &str| {
        dir.join("..")
            .join(format!("shards-{g}-{}.log", std::process::id()))
    };
    for f in ["a", "b", "c"] {
        write(dir, &format!("t/{f}.test"), "x\n");
    }
    for f in ["x", "y"] {
        write(dir, &format!("e/{f}.spec"), "x\n");
    }
    write(dir, "t/notes.md", "not a test\n");
    let task = |g: &str, n: u32, glob: &str| {
        serde_json::json!({
            "run": format!(
                "test -n \"$PIT_SHARD_FILES\" && [ \"$(cat $PIT_SHARD_FILES_LIST)\" = \"$PIT_SHARD_FILES\" ] && echo \"$PIT_SHARD_FILES\" >> {}",
                log(g).display()
            ),
            "shards": n,
            "shardFiles": [glob],
            "cache": false,
        })
    };
    let config = serde_json::json!({ "groups": {
        "unit": { "tasks": { "test": task("unit", 5, "t/*.test") } },
        "e2e": { "tasks": { "test": task("e2e", 2, "e/*.spec") } },
    } });
    write(dir, ".gitignore", ".pit/\n");
    write(dir, "pit.config.json", &config.to_string());
    sh(
        dir,
        "git init -q -b main && git add -A && git commit -qm init",
    );
    let view = run(&pit, dir).await;
    assert_eq!(view.status, Status::Passed, "{view:?}");
    let dealt = |g: &str| {
        let mut d: Vec<String> = std::fs::read_to_string(log(g))
            .unwrap()
            .split_whitespace()
            .map(String::from)
            .collect();
        d.sort();
        d
    };
    assert_eq!(dealt("unit"), ["t/a.test", "t/b.test", "t/c.test"]);
    assert_eq!(dealt("e2e"), ["e/x.spec", "e/y.spec"]);
    let empty = view
        .groups
        .iter()
        .flat_map(|g| &g.tasks)
        .filter(|t| t.message.as_deref() == Some("no files were dealt to this shard"))
        .count();
    assert_eq!(empty, 2, "{view:?}");
    // A glob that matches nothing (a typo, a moved directory).
    let config = serde_json::json!({ "tasks": { "test": task("typo", 2, "tests/*.test") } });
    write(dir, "pit.config.json", &config.to_string());
    sh(dir, "git commit -qam typo");
    let view = run(&pit, dir).await;
    assert_ne!(view.status, Status::Passed, "{view:?}");
    assert!(format!("{view:?}").contains("match no files"), "{view:?}");
}

/// `/metrics` reports what an autoscaler needs: work waiting per OS and how
/// long the oldest has waited, plus workers online and busy.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn metrics_show_queue_depth_for_autoscaling() {
    let pit = start().await;
    let repo = tempfile::tempdir().unwrap();
    let dir = repo.path();
    // No worker runs another OS than this one, so this group waits.
    let other = if cfg!(target_os = "windows") {
        "macos"
    } else {
        "windows"
    };
    let config = serde_json::json!({ "groups": { "away": { "os": [other], "tasks": { "t": { "run": "true" } } } } });
    write(dir, "pit.config.json", &config.to_string());
    sh(
        dir,
        "git init -q -b main && git add -A && git commit -qm init",
    );
    pit::cli::submit(&pit.client, dir, vec![], Default::default())
        .await
        .unwrap();
    let metrics = tokio::time::timeout(Duration::from_secs(20), async {
        loop {
            let m = pit.client.get_text("/metrics").await.unwrap();
            // The worker counts once it has polled for work.
            if m.contains(&format!("pit_queued_groups{{os=\"{other}\"}} 1"))
                && m.contains("pit_workers_online 1")
            {
                return m;
            }
            tokio::time::sleep(Duration::from_millis(200)).await;
        }
    })
    .await
    .expect("queued group never showed up in /metrics");
    assert!(metrics.contains("# TYPE pit_oldest_queued_seconds gauge"));
    assert!(metrics.contains(&format!("pit_oldest_queued_seconds{{os=\"{other}\"}}")));
    assert!(metrics.contains("pit_workers_online 1"), "{metrics}");
}

/// Git LFS files arrive with their content, fetched once into the worker's
/// mirror and read from there by every workspace (fresh ones included),
/// instead of one download per checkout.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn git_lfs_files_are_fetched_once_per_worker() {
    if !std::process::Command::new("git")
        .args(["lfs", "version"])
        .output()
        .is_ok_and(|o| o.status.success())
    {
        eprintln!("skipped: no git-lfs");
        return;
    }
    let pit = start().await;
    let tmp = tempfile::tempdir().unwrap();
    let work = tmp.path().join("work");
    std::fs::create_dir_all(&work).unwrap();
    sh(tmp.path(), "git init -q --bare -b main origin.git");
    write(&work, "model.bin", "the real large content\n");
    write(
        &work,
        "pit.config.json",
        r#"{"tasks":{"check":{"run":"grep -q 'the real large content' model.bin","cache":false}}}"#,
    );
    sh(
        &work,
        "git init -q -b main && git lfs install --local >/dev/null && git lfs track '*.bin' >/dev/null \
         && git add -A && git commit -qm lfs && git remote add origin ../origin.git && git push -q origin main 2>/dev/null",
    );
    // Stored as a pointer in git; the content lives in the LFS store.
    assert!(sh(&work, "git show HEAD:model.bin").starts_with("version https://git-lfs"));
    let view = run(&pit, &work).await;
    assert_eq!(view.status, Status::Passed, "{view:?}");
    let view = run_with(
        &pit,
        &work,
        pit::cli::RunOpts {
            fresh: true,
            ..Default::default()
        },
    )
    .await;
    assert_eq!(view.status, Status::Passed, "a fresh workspace: {view:?}");
    // One copy, in the mirror; no workspace keeps its own.
    let worker = pit._data.path().join("worker");
    let copies = |dir: &Path| {
        walkdir(dir)
            .into_iter()
            .filter(|p| {
                p.to_string_lossy()
                    .replace('\\', "/")
                    .contains("/lfs/objects/")
                    && p.file_name().is_some_and(|n| n.len() == 64)
            })
            .count()
    };
    assert_eq!(copies(&worker.join("mirrors")), 1);
    assert_eq!(copies(&worker.join("workspaces")), 0);
}

fn walkdir(dir: &Path) -> Vec<std::path::PathBuf> {
    let mut out = Vec::new();
    if let Ok(entries) = std::fs::read_dir(dir) {
        for e in entries.flatten() {
            let p = e.path();
            if p.is_dir() {
                out.extend(walkdir(&p));
            } else {
                out.push(p);
            }
        }
    }
    out
}

/// `pit run --debug`: a failed task's result names a script that runs
/// commands in the workspace it failed in, with what it left behind, and
/// that workspace is not checked out over while the script exists.
#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn debug_runs_keep_the_failed_workspace_and_a_way_in() {
    let pit = start().await;
    let repo = tempfile::tempdir().unwrap();
    let dir = repo.path();
    let config = serde_json::json!({ "tasks": { "test": {
        "run": "echo $PIT_RUN_ID > left-behind.txt; exit 1",
        "cache": false,
    } } });
    write(dir, ".gitignore", "left-behind.txt\n");
    write(dir, "pit.config.json", &config.to_string());
    sh(
        dir,
        "git init -q -b main && git add -A && git commit -qm init",
    );
    let view = run_with(
        &pit,
        dir,
        pit::cli::RunOpts {
            debug: true,
            ..Default::default()
        },
    )
    .await;
    let msg = view.groups[0].tasks[0].message.clone().unwrap_or_default();
    let script = msg
        .split('`')
        .nth(1)
        .and_then(|s| s.strip_prefix("sh "))
        .unwrap_or_else(|| panic!("no debug script in {msg:?}"))
        .to_string();
    let out = sh(
        dir,
        &format!("sh {script} 'cat left-behind.txt; echo $PIT_TASK'"),
    );
    assert_eq!(out, format!("{}\ntest\n", view.id));
    // Another run gets another workspace while the failed one is kept.
    let again = run(&pit, dir).await;
    assert_eq!(
        sh(dir, &format!("sh {script} 'cat left-behind.txt'")).trim(),
        view.id
    );
    assert_ne!(again.id, view.id);
    // Deleting the script releases it.
    std::fs::remove_file(&script).unwrap();
    let parent = std::path::Path::new(&script)
        .parent()
        .unwrap()
        .to_path_buf();
    tokio::time::timeout(Duration::from_secs(15), async {
        while parent.exists() {
            tokio::time::sleep(Duration::from_millis(200)).await;
        }
    })
    .await
    .expect("the workspace was not released");
}

/// A passing task that takes far longer than it usually does says so in its
/// result, and a steady one does not.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_pass_much_slower_than_usual_is_noted() {
    let pit = start().await;
    let repo = tempfile::tempdir().unwrap();
    let dir = repo.path();
    let delay = dir.join("..").join(format!("slow-{}", std::process::id()));
    std::fs::write(&delay, "0").unwrap();
    let config = serde_json::json!({ "tasks": {
        "steady": { "run": "true", "cache": false },
        "test": { "run": format!("sleep $(cat {})", delay.display()), "cache": false },
    } });
    write(dir, "pit.config.json", &config.to_string());
    sh(
        dir,
        "git init -q -b main && git add -A && git commit -qm init",
    );
    let message = |view: &RunView, name: &str| {
        view.groups[0]
            .tasks
            .iter()
            .find(|t| t.name == name)
            .and_then(|t| t.message.clone())
            .unwrap_or_default()
    };
    for _ in 0..pit::slow::MIN_HISTORY {
        let view = run(&pit, dir).await;
        assert!(!message(&view, "test").contains("slow:"), "{view:?}");
    }
    std::fs::write(&delay, "2.5").unwrap();
    let view = run(&pit, dir).await;
    assert_eq!(view.status, Status::Passed);
    assert!(
        message(&view, "test").starts_with("slow: took 2."),
        "{view:?}"
    );
    assert!(!message(&view, "steady").contains("slow:"), "{view:?}");
}

/// A task killed by a signal says which one, and a container killed for
/// using more than its `memory` says it ran out of memory instead of
/// "exited with 137".
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn killed_tasks_say_why() {
    let pit = start().await;
    let repo = tempfile::tempdir().unwrap();
    let dir = repo.path();
    let docker = cfg!(target_os = "linux")
        && pit::worker::exec::detect_backends()
            .await
            .contains(&Backend::Docker);
    let mut tasks = serde_json::json!({
        "killed": { "run": "kill -9 $$", "cache": false },
    });
    if docker {
        tasks["hungry"] = serde_json::json!({
            "image": "node:22-alpine",
            "memory": "64m",
            "run": "node -e 'const a = []; setInterval(() => a.push(Buffer.alloc(8e6, 1)), 20)'",
            "cache": false,
        });
    }
    write(
        dir,
        "pit.config.json",
        &serde_json::json!({ "tasks": tasks }).to_string(),
    );
    sh(
        dir,
        "git init -q -b main && git add -A && git commit -qm init",
    );
    let view = run_with_timeout(&pit, dir, Duration::from_secs(240)).await;
    let message = |name: &str| {
        view.groups[0]
            .tasks
            .iter()
            .find(|t| t.name == name)
            .and_then(|t| t.message.clone())
            .unwrap_or_default()
    };
    assert!(
        message("killed").starts_with("killed by SIGKILL"),
        "{view:?}"
    );
    if docker {
        assert!(
            message("hungry").starts_with("out of memory: exit 137"),
            "{view:?}"
        );
    }
}

async fn run_with_timeout(pit: &Pit, dir: &Path, limit: Duration) -> RunView {
    let sub = pit::cli::submit(&pit.client, dir, vec![], Default::default())
        .await
        .unwrap();
    tokio::time::timeout(limit, pit::cli::wait(&pit.client, &sub.run_id, |_| {}))
        .await
        .expect("run timed out")
        .unwrap()
}

/// Failed tests become annotations on the GitHub check, so they show on the
/// pull request's diff: all of them, 50 per request (GitHub's limit), once.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn failed_tests_are_annotated_on_the_check() {
    use axum::routing::{get, patch, post};
    type Seen = std::sync::Arc<parking_lot::Mutex<Vec<serde_json::Value>>>;
    let seen: Seen = Default::default();
    let record = |seen: Seen| {
        move |axum::Json(body): axum::Json<serde_json::Value>| {
            let seen = seen.clone();
            async move {
                seen.lock().push(body);
                axum::Json(serde_json::json!({ "id": 7 }))
            }
        }
    };
    let mock = axum::Router::new()
        .route(
            "/repos/acme/web/installation",
            get(|| async { axum::Json(serde_json::json!({ "id": 1 })) }),
        )
        .route(
            "/app/installations/1/access_tokens",
            post(|| async { axum::Json(serde_json::json!({ "token": "t" })) }),
        )
        .route("/repos/acme/web/check-runs", post(record(seen.clone())))
        .route("/repos/acme/web/check-runs/7", patch(record(seen.clone())));
    let mock_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let mock_url = format!("http://{}", mock_listener.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(mock_listener, mock).await });

    let keys = tempfile::tempdir().unwrap();
    sh(keys.path(), "openssl genrsa -out key.pem 2048 2>/dev/null");
    let pem = std::fs::read_to_string(keys.path().join("key.pem")).unwrap();
    let github = pit::server::github::GitHub::with_app(&mock_url, "1", &pem).unwrap();
    let data = tempfile::tempdir().unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let app = pit::server::App::with_github(
        &data.path().join("server"),
        None,
        base.clone(),
        Some(github),
    )
    .unwrap();
    tokio::spawn(pit::server::serve_listener(app.clone(), listener));
    tokio::spawn(pit::worker::run(pit::worker::WorkerOpts {
        server: base.clone(),
        token: None,
        name: Some("annotate".into()),
        data_dir: data.path().join("worker"),
        slots: 1,
        parallelism: 1,
        workspace_max_bytes: 0,
        worker_token: None,
        secrets_file: None,
    }));

    let repo = tempfile::tempdir().unwrap();
    let dir = repo.path();
    let cases: String = (1..=60)
        .map(|i| format!(r#"<testcase classname="cart" name="case {i}" file="src/cart.test.ts" line="{i}"><failure message="expected {i}"/></testcase>"#))
        .collect();
    write(
        dir,
        "fixtures/report.xml",
        &format!("<testsuite>{cases}</testsuite>"),
    );
    write(dir, "src/cart.test.ts", "test()\n");
    write(dir, ".gitignore", "out/\n");
    let config = serde_json::json!({ "tasks": { "test": {
        "run": "mkdir -p out && cp fixtures/report.xml out/junit.xml && echo '**60 failed**, see annotations' >> $GITHUB_STEP_SUMMARY && exit 1",
        "reports": ["out/*.xml"],
        "cache": false,
    } } });
    write(dir, "pit.config.json", &config.to_string());
    sh(
        dir,
        "git init -q -b main && git add -A && git commit -qm init",
    );
    let sha = sh(dir, "git rev-parse HEAD").trim().to_string();
    app.db
        .upsert_repo("acme/web", Some(&dir.to_string_lossy()), Some("acme/web"))
        .unwrap();
    let run_id = app
        .create_run(
            CreateRun {
                repo: "acme/web".into(),
                url: None,
                sha,
                overlay: None,
                tasks: vec![],
                trigger: Some("push".into()),
                git_ref: Some("feature".into()),
                no_cache: false,
                fresh: false,
                debug: false,
                counts_as: None,
                no_secrets: false,
                trusted: false,
            },
            None,
        )
        .await
        .unwrap();
    let client = Client::new(&base, None).unwrap();
    let view = tokio::time::timeout(
        Duration::from_secs(60),
        pit::cli::wait(&client, &run_id, |_| {}),
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(view.status, Status::Failed);
    let annotations = || -> Vec<serde_json::Value> {
        seen.lock()
            .iter()
            .flat_map(|b| {
                b["output"]["annotations"]
                    .as_array()
                    .cloned()
                    .unwrap_or_default()
            })
            .collect()
    };
    tokio::time::timeout(Duration::from_secs(20), async {
        while annotations().len() < 60 {
            tokio::time::sleep(Duration::from_millis(200)).await;
        }
    })
    .await
    .expect("annotations never reached the check");
    // Later reports of the finished run do not add them again.
    tokio::time::sleep(Duration::from_secs(2)).await;
    let all = annotations();
    assert_eq!(all.len(), 60);
    assert!(seen.lock().iter().all(|b| b["output"]["annotations"]
        .as_array()
        .is_none_or(|a| a.len() <= 50)));
    // What the task wrote to $GITHUB_STEP_SUMMARY is on the check too.
    let summaries: Vec<String> = seen
        .lock()
        .iter()
        .filter_map(|b| b["output"]["summary"].as_str().map(String::from))
        .collect();
    assert!(
        summaries
            .iter()
            .any(|s| s.contains("### `test`\n\n**60 failed**, see annotations")),
        "{summaries:?}"
    );
    let first = all.iter().find(|a| a["title"] == "cart.case 1").unwrap();
    assert_eq!(first["path"], "src/cart.test.ts");
    assert_eq!(first["start_line"], 1);
    assert_eq!(first["annotation_level"], "failure");
    assert_eq!(first["message"], "expected 1");
}

/// Pit's own bookkeeping about a workspace lives outside it: a task that
/// plants a symlink where it used to be (`.git/pit-warm.json`) cannot make
/// the worker write through it to a file outside the workspace.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_worker_never_writes_through_a_task_planted_symlink() {
    let pit = start().await;
    let repo = tempfile::tempdir().unwrap();
    let dir = repo.path();
    let precious = dir
        .join("..")
        .join(format!("precious-{}", std::process::id()));
    std::fs::write(&precious, "precious").unwrap();
    let plant = format!(
        "for f in .git/pit-warm.json .git/pit-used; do rm -f $f; ln -s {} $f; done",
        precious.display()
    );
    let config = serde_json::json!({ "tasks": {
        "plant": { "run": plant, "cache": false },
        "warm": { "run": "true", "cache": "workspace", "deps": ["plant"] },
    } });
    write(dir, "pit.config.json", &config.to_string());
    sh(
        dir,
        "git init -q -b main && git add -A && git commit -qm init",
    );
    let view = run(&pit, dir).await;
    assert_eq!(view.status, Status::Passed, "{view:?}");
    assert_eq!(std::fs::read_to_string(&precious).unwrap(), "precious");
}

/// A merge queue candidate is failed at its first real failure: the rest of
/// its run is stopped instead of holding the lane until its slowest task
/// ends.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_merge_queue_fails_fast() {
    let pit = start().await;
    let tmp = tempfile::tempdir().unwrap();
    let work = tmp.path().join("work");
    std::fs::create_dir_all(&work).unwrap();
    sh(tmp.path(), "git init -q --bare -b main origin.git");
    write(
        &work,
        "pit.config.json",
        r#"{"tasks":{
            "lint":{"run":"! grep -rq BAD src","cache":false},
            "slow":{"run":"sleep 60","cache":false}
        }}"#,
    );
    write(&work, "src/a.txt", "ok\n");
    sh(&work, "git init -q -b main && git add -A && git commit -qm init && git remote add origin ../origin.git && git push -q origin main");
    sh(&work, "git checkout -q -b broken && echo BAD > src/b.txt && git add -A && git commit -qm broken && git push -q origin broken");
    let started = std::time::Instant::now();
    let entry: QueueEntryView = pit
        .client
        .post(
            "/api/queue",
            &QueueAdd {
                repo: "ff".into(),
                url: Some(tmp.path().join("origin.git").to_string_lossy().to_string()),
                branch: Some("broken".into()),
                pr: None,
                target: None,
                sha: None,
            },
        )
        .await
        .unwrap();
    let failed = tokio::time::timeout(Duration::from_secs(30), async {
        loop {
            let entries: Vec<QueueEntryView> = pit.client.get("/api/queue").await.unwrap();
            if let Some(e) = entries
                .into_iter()
                .find(|e| e.id == entry.id && e.status == "failed")
            {
                return e;
            }
            tokio::time::sleep(Duration::from_millis(200)).await;
        }
    })
    .await
    .expect("the broken change held the queue");
    assert!(started.elapsed() < Duration::from_secs(30));
    let run: RunView = pit
        .client
        .get(&format!("/api/runs/{}", failed.run_id.unwrap()))
        .await
        .unwrap();
    assert_eq!(
        run.message.as_deref(),
        Some("stopped early: default.lint failed")
    );
}

/// A group's `services` start before its tasks, are reachable from docker
/// tasks by name and from host tasks on a published port (so two groups on
/// one worker never fight over a port), and are removed afterwards.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn groups_get_their_own_service_containers() {
    if !(cfg!(target_os = "linux")
        && pit::worker::exec::detect_backends()
            .await
            .contains(&Backend::Docker))
    {
        eprintln!("skipped: no Linux docker");
        return;
    }
    let pit = start().await;
    let repo = tempfile::tempdir().unwrap();
    let dir = repo.path();
    let service = serde_json::json!({
        "image": "node:22-alpine",
        "command": ["node", "-e", "require('http').createServer((q, r) => r.end(process.env.WHO)).listen(8080)"],
        "ports": [8080],
    });
    let mut a = service.clone();
    a["env"] = serde_json::json!({ "WHO": "a" });
    let mut b = service.clone();
    b["env"] = serde_json::json!({ "WHO": "b" });
    let fetch = "node -e \"fetch('http://' + process.env.PIT_SERVICE_WEB_HOST + ':' + process.env.PIT_SERVICE_WEB_PORT).then(r => r.text()).then(t => { console.log('got ' + t); process.exit(t === process.env.WANT ? 0 : 1) })\"";
    let config = serde_json::json!({ "groups": {
        "a": { "services": { "web": a }, "tasks": {
            "in-docker": { "image": "node:22-alpine", "run": format!("WANT=a {fetch}"), "cache": false },
        } },
        "b": { "services": { "web": b }, "tasks": {
            "on-host": { "run": format!("WANT=b {fetch}"), "cache": false },
        } },
    } });
    write(dir, "pit.config.json", &config.to_string());
    sh(
        dir,
        "git init -q -b main && git add -A && git commit -qm init",
    );
    let view = run_with_timeout(&pit, dir, Duration::from_secs(240)).await;
    assert_eq!(view.status, Status::Passed, "{view:?}");
    let left = sh(
        dir,
        "docker ps -aq --filter name=pit-grp_; docker network ls -q --filter name=pit-grp_",
    );
    assert!(left.trim().is_empty(), "left behind: {left}");
}

/// Tasks get the commit to diff against for `nx affected` and friends
/// (`PIT_BASE_SHA`, `NX_BASE`, `TURBO_SCM_BASE`): the newest commit of the
/// default branch whose trusted run passed and that the run's commit
/// contains, skipping a red one, as `nx-set-shas` does.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn tasks_get_the_last_green_commit_as_their_base() {
    let pit = start().await;
    let tmp = tempfile::tempdir().unwrap();
    let work = tmp.path().join("work");
    std::fs::create_dir_all(&work).unwrap();
    sh(tmp.path(), "git init -q --bare -b main origin.git");
    let out = tmp.path().join("bases");
    std::fs::create_dir_all(&out).unwrap();
    let config = serde_json::json!({ "tasks": { "affected": {
        "run": format!(
            "echo \"$NX_BASE $(git diff --name-only $PIT_BASE_SHA...HEAD | sort | tr '\\n' ' ')\" > {}/$PIT_SHA; test ! -f red",
            out.display()
        ),
        "cache": false,
    } } });
    write(&work, "pit.config.json", &config.to_string());
    write(&work, "a.txt", "1\n");
    sh(&work, "git init -q -b main && git add -A && git commit -qm c1 && git remote add origin ../origin.git && git push -q origin main");
    let url = tmp.path().join("origin.git").to_string_lossy().to_string();
    let head = || sh(&work, "git rev-parse HEAD").trim().to_string();
    let c1 = head();
    assert_eq!(
        run_ref(&pit, &url, c1.clone(), "main", true).await.status,
        Status::Passed
    );
    // c2 is red.
    sh(
        &work,
        "touch red && git add -A && git commit -qm c2 && git push -q origin main",
    );
    let c2 = head();
    assert_eq!(
        run_ref(&pit, &url, c2.clone(), "main", true).await.status,
        Status::Failed
    );
    sh(&work, "git rm -q red && echo 2 > b.txt && git add -A && git commit -qm c3 && git push -q origin main");
    let c3 = head();
    assert_eq!(
        run_ref(&pit, &url, c3.clone(), "main", true).await.status,
        Status::Passed
    );
    let seen = |sha: &str| {
        std::fs::read_to_string(out.join(sha))
            .unwrap()
            .trim()
            .to_string()
    };
    // The red c2 is skipped: c3 is compared with c1, not its parent. (`red`,
    // added in c2 and removed in c3, nets out.)
    assert_eq!(seen(&c3), format!("{c1} b.txt"));
    // A two-commit branch off c3 is compared with c3, both commits included.
    sh(&work, "git checkout -q -b feature && echo x > c.txt && git add -A && git commit -qm f1 && echo y > d.txt && git add -A && git commit -qm f2 && git push -q origin feature");
    let f = head();
    assert_eq!(
        run_ref(&pit, &url, f.clone(), "feature", false)
            .await
            .status,
        Status::Passed
    );
    assert_eq!(seen(&f), format!("{c3} c.txt d.txt"));
}

/// Starts a run of a pushed commit the way a webhook does, and waits for it.
async fn run_ref(pit: &Pit, url: &str, sha: String, git_ref: &str, trusted: bool) -> RunView {
    let id = pit
        .app
        .create_run(
            CreateRun {
                repo: "affected".into(),
                url: Some(url.into()),
                sha,
                overlay: None,
                tasks: vec![],
                trigger: Some("push".into()),
                git_ref: Some(git_ref.into()),
                no_cache: false,
                fresh: false,
                debug: false,
                counts_as: None,
                no_secrets: false,
                trusted,
            },
            None,
        )
        .await
        .unwrap();
    tokio::time::timeout(
        Duration::from_secs(60),
        pit::cli::wait(&pit.client, &id, |_| {}),
    )
    .await
    .unwrap()
    .unwrap()
}

/// `pit flaky --tests` names the flaky test inside a task, from its JUnit
/// reports: one that failed and then passed on the task's retry, and one
/// that failed where the same inputs passed in another run. Steady tests are
/// not listed.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn flaky_tests_inside_a_task_are_named() {
    let pit = start().await;
    let repo = tempfile::tempdir().unwrap();
    let dir = repo.path();
    let state = dir
        .join("..")
        .join(format!("flaky-tests-{}", std::process::id()));
    std::fs::create_dir_all(&state).unwrap();
    // `on_retry` fails on each run's first attempt; `flip` fails in both
    // attempts of the first run only.
    let script = format!(
        r#"import os, sys
d = {state:?}
run = os.environ["PIT_RUN_ID"]
first = not os.path.exists(f"{{d}}/{{run}}")
open(f"{{d}}/{{run}}", "a").close()
runs = len([f for f in os.listdir(d) if f.startswith("run_")])
bad = {{"on_retry": first, "flip": runs == 1, "steady": False}}
cases = "".join(f'<testcase classname="t" name="{{n}}">' + ('<failure message="x"/>' if b else '') + '</testcase>' for n, b in bad.items())
os.makedirs("out", exist_ok=True)
open("out/r.xml", "w").write("<testsuite>" + cases + "</testsuite>")
sys.exit(1 if any(bad.values()) else 0)
"#,
        state = state.display().to_string()
    );
    write(dir, "t.py", &script);
    write(dir, ".gitignore", "out/\n");
    let config = serde_json::json!({ "tasks": { "test": {
        "run": "python3 t.py", "reports": ["out/*.xml"], "retries": 1, "cache": false,
    } } });
    write(dir, "pit.config.json", &config.to_string());
    sh(
        dir,
        "git init -q -b main && git add -A && git commit -qm init",
    );
    let first = run(&pit, dir).await;
    assert_eq!(first.status, Status::Failed, "{first:?}");
    let second = run(&pit, dir).await;
    assert_eq!(second.status, Status::Passed, "{second:?}");
    let msg = second.groups[0].tasks[0]
        .message
        .clone()
        .unwrap_or_default();
    assert!(msg.contains("(flaky tests: t.on_retry)"), "{msg}");
    let flaky: Vec<FlakyTest> = pit.client.get("/api/flaky/tests?days=1").await.unwrap();
    let get = |n: &str| flaky.iter().find(|t| t.test == n).cloned();
    let on_retry = get("t.on_retry").expect("passed on a retry");
    assert!(on_retry.flaky_passes >= 1, "{on_retry:?}");
    let flip = get("t.flip").expect("failed where the same inputs passed");
    assert_eq!((flip.failures, flip.mixed_inputs), (1, 1), "{flip:?}");
    assert!(get("t.steady").is_none());
}

/// Runs started by the named schedule.
async fn count_trigger(pit: &Pit, schedule: &str) -> usize {
    let runs: Vec<RunView> = pit.client.get("/api/runs?limit=50").await.unwrap();
    runs.into_iter()
        .filter(|r| r.trigger == format!("schedule:{schedule}"))
        .count()
}

/// `/retest` in a pull request comment, from a collaborator, reruns the pull
/// request's last commit: what passed comes from the cache, what failed runs
/// again. Anyone else's `/retest` is ignored.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn retest_comments_rerun_the_failures() {
    use axum::routing::post;
    use hmac::Mac;
    let reactions: std::sync::Arc<parking_lot::Mutex<Vec<String>>> = Default::default();
    let seen = reactions.clone();
    let mock = axum::Router::new()
        .route(
            "/repos/acme/shop/issues/comments/{id}/reactions",
            post(
                move |axum::extract::Path(id): axum::extract::Path<String>| {
                    let seen = seen.clone();
                    async move {
                        seen.lock().push(id);
                        axum::Json(serde_json::json!({}))
                    }
                },
            ),
        )
        .fallback(|| async { axum::Json(serde_json::json!({})) });
    let mock_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let mock_url = format!("http://{}", mock_listener.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(mock_listener, mock).await });

    let data = tempfile::tempdir().unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let github =
        pit::server::github::GitHub::with_token(&mock_url, "t", Some("s3cret".into())).unwrap();
    let app = pit::server::App::with_github(
        &data.path().join("server"),
        None,
        base.clone(),
        Some(github),
    )
    .unwrap();
    tokio::spawn(pit::server::serve_listener(app.clone(), listener));
    tokio::spawn(pit::worker::run(pit::worker::WorkerOpts {
        server: base.clone(),
        token: None,
        name: Some("retest".into()),
        data_dir: data.path().join("worker"),
        slots: 1,
        parallelism: 2,
        workspace_max_bytes: 0,
        worker_token: None,
        secrets_file: None,
    }));

    let repo = tempfile::tempdir().unwrap();
    let marker = repo
        .path()
        .join("..")
        .join(format!("retest-{}", std::process::id()));
    let config = serde_json::json!({ "tasks": {
        "steady": { "run": "true" },
        "flake": { "run": format!("test -f {m} || {{ touch {m}; exit 1; }}", m = marker.display()), "cache": false },
    } });
    write(repo.path(), "pit.config.json", &config.to_string());
    sh(
        repo.path(),
        "git init -q -b main && git add -A && git commit -qm init",
    );
    let sha = sh(repo.path(), "git rev-parse HEAD").trim().to_string();
    let repository = serde_json::json!({ "full_name": "acme/shop", "clone_url": repo.path().to_string_lossy(), "default_branch": "main" });
    let post_event = |event: &'static str, payload: serde_json::Value| {
        let body = payload.to_string();
        let mut mac = hmac::Hmac::<sha2::Sha256>::new_from_slice(b"s3cret").unwrap();
        mac.update(body.as_bytes());
        let sig = format!("sha256={}", hex::encode(mac.finalize().into_bytes()));
        let base = base.clone();
        async move {
            reqwest::Client::new()
                .post(format!("{base}/webhooks/github"))
                .header("x-github-event", event)
                .header("x-hub-signature-256", sig)
                .body(body)
                .send()
                .await
                .unwrap()
                .json::<serde_json::Value>()
                .await
                .unwrap()
        }
    };
    let client = Client::new(&base, None).unwrap();
    let wait = |id: String| {
        let client = client.clone();
        async move {
            tokio::time::timeout(
                Duration::from_secs(60),
                pit::cli::wait(&client, &id, |_| {}),
            )
            .await
            .unwrap()
            .unwrap()
        }
    };
    let opened = post_event(
        "pull_request",
        serde_json::json!({ "action": "opened", "repository": repository, "pull_request": {
            "number": 9, "head": { "sha": sha, "repo": { "full_name": "acme/shop" } }, "base": { "ref": "main" },
        } }),
    )
    .await;
    let first = wait(opened["run"].as_str().unwrap().to_string()).await;
    assert_eq!(first.status, Status::Failed, "{first:?}");
    let comment = |who: &str, id: u64| {
        serde_json::json!({ "action": "created", "repository": repository,
            "issue": { "number": 9, "pull_request": { "url": "x" } },
            "comment": { "id": id, "body": "/retest\nplease", "author_association": who } })
    };
    let ignored = post_event("issue_comment", comment("NONE", 1)).await;
    assert!(
        ignored["ignored"]
            .as_str()
            .unwrap()
            .contains("collaborator"),
        "{ignored}"
    );
    let retest = post_event("issue_comment", comment("MEMBER", 2)).await;
    let again = wait(retest["run"].as_str().expect("a rerun").to_string()).await;
    assert_eq!(again.status, Status::Passed, "{again:?}");
    assert_eq!(again.sha, sha);
    assert_eq!(again.trigger, "retest");
    let status = |name: &str| {
        again.groups[0]
            .tasks
            .iter()
            .find(|t| t.name == name)
            .unwrap()
            .status
    };
    assert_eq!(status("steady"), Status::Cached);
    assert_eq!(status("flake"), Status::Passed);
    assert_eq!(reactions.lock().as_slice(), ["2"]);
}

/// Submodules are checked out at the commit the superproject pins, from a
/// mirror on the worker, and follow the pointer when it moves.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn submodules_are_checked_out_at_their_pinned_commit() {
    let pit = start().await;
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().canonicalize().unwrap();
    sh(
        &root,
        "git init -q --bare -b main lib.git && git init -q --bare -b main app.git",
    );
    let lib = root.join("libwork");
    std::fs::create_dir_all(&lib).unwrap();
    write(&lib, "v.txt", "one\n");
    sh(&lib, "git init -q -b main && git add -A && git commit -qm one && git remote add origin ../lib.git && git push -q origin main");
    let app = root.join("app");
    std::fs::create_dir_all(&app).unwrap();
    let config = serde_json::json!({ "tasks": { "check": {
        "run": "cat vendor/lib/v.txt > ../seen-$PIT_RUN_ID", "cache": false,
    } } });
    write(&app, "pit.config.json", &config.to_string());
    sh(&app, &format!(
        "git init -q -b main && git -c protocol.file.allow=always submodule add -q {} vendor/lib && git add -A && git commit -qm app && git remote add origin ../app.git && git push -q origin main",
        root.join("lib.git").display()
    ));
    let seen = |view: &RunView| {
        // The task wrote next to the workspace; find it by run id.
        let mut found = None;
        for e in walkdir(&pit._data.path().join("worker")) {
            if e.file_name().and_then(|n| n.to_str()) == Some(&format!("seen-{}", view.id)) {
                found = Some(std::fs::read_to_string(e).unwrap());
            }
        }
        found.unwrap_or_default()
    };
    let first = run(&pit, &app).await;
    assert_eq!(first.status, Status::Passed, "{first:?}");
    assert_eq!(seen(&first), "one\n");
    // Move the pointer.
    sh(
        &lib,
        "echo two > v.txt && git commit -qam two && git push -q origin main",
    );
    sh(&app, "git -c protocol.file.allow=always submodule update -q --remote vendor/lib && git commit -qam bump && git push -q origin main");
    let second = run(&pit, &app).await;
    assert_eq!(second.status, Status::Passed, "{second:?}");
    assert_eq!(seen(&second), "two\n");
}

/// Commits pushed by a bot with Actions' `GITHUB_TOKEN`, and pull requests a
/// bot opens, start runs like anyone's: GitHub delivers their webhooks to
/// apps, and only Actions itself declines to start workflows for them.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn bot_pushes_and_pull_requests_are_tested() {
    use hmac::Mac;
    let data = tempfile::tempdir().unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let github =
        pit::server::github::GitHub::with_token("http://127.0.0.1:9", "t", Some("s3cret".into()))
            .unwrap();
    let app = pit::server::App::with_github(
        &data.path().join("server"),
        None,
        base.clone(),
        Some(github),
    )
    .unwrap();
    tokio::spawn(pit::server::serve_listener(app.clone(), listener));
    tokio::spawn(pit::worker::run(pit::worker::WorkerOpts {
        server: base.clone(),
        token: None,
        name: Some("bots".into()),
        data_dir: data.path().join("worker"),
        slots: 1,
        parallelism: 1,
        workspace_max_bytes: 0,
        worker_token: None,
        secrets_file: None,
    }));
    let repo = tempfile::tempdir().unwrap();
    write(
        repo.path(),
        "pit.config.json",
        r#"{"tasks":{"check":{"run":"true","cache":false}}}"#,
    );
    sh(repo.path(), "git init -q -b main && git add -A && git commit -qm init && git checkout -q -b format && git commit -q --allow-empty -m 'style: format'");
    let sha = sh(repo.path(), "git rev-parse HEAD").trim().to_string();
    let repository = serde_json::json!({ "full_name": "acme/site", "clone_url": repo.path().to_string_lossy(), "default_branch": "main" });
    let bot = |login: &str| serde_json::json!({ "login": login, "type": "Bot" });
    let post_event = |event: &'static str, payload: serde_json::Value| {
        let body = payload.to_string();
        let mut mac = hmac::Hmac::<sha2::Sha256>::new_from_slice(b"s3cret").unwrap();
        mac.update(body.as_bytes());
        let sig = format!("sha256={}", hex::encode(mac.finalize().into_bytes()));
        let base = base.clone();
        async move {
            reqwest::Client::new()
                .post(format!("{base}/webhooks/github"))
                .header("x-github-event", event)
                .header("x-hub-signature-256", sig)
                .body(body)
                .send()
                .await
                .unwrap()
                .json::<serde_json::Value>()
                .await
                .unwrap()
        }
    };
    let client = Client::new(&base, None).unwrap();
    let pushed = post_event(
        "push",
        serde_json::json!({ "ref": "refs/heads/format", "after": sha, "repository": repository,
            "pusher": { "name": "github-actions[bot]" }, "sender": bot("github-actions[bot]") }),
    )
    .await;
    let opened = post_event(
        "pull_request",
        serde_json::json!({ "action": "opened", "repository": repository, "sender": bot("dependabot[bot]"),
            "pull_request": { "number": 3, "user": bot("dependabot[bot]"),
                "head": { "sha": sha, "repo": { "full_name": "acme/site" } }, "base": { "ref": "main" } } }),
    )
    .await;
    for (what, v) in [("bot push", &pushed), ("bot pull request", &opened)] {
        let id = v["run"]
            .as_str()
            .unwrap_or_else(|| panic!("{what}: {v}"))
            .to_string();
        let view = tokio::time::timeout(
            Duration::from_secs(60),
            pit::cli::wait(&client, &id, |_| {}),
        )
        .await
        .unwrap()
        .unwrap();
        assert_eq!(view.status, Status::Passed, "{what}: {view:?}");
        assert_eq!(view.sha, sha);
    }
}

/// A group's `on` limits the kinds of run it is in: e2e only in the merge
/// queue and on main, a deploy only on main. Naming a task on a local run
/// still runs it.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn groups_run_only_on_the_kinds_of_run_they_name() {
    let pit = start().await;
    let tmp = tempfile::tempdir().unwrap();
    let work = tmp.path().join("work");
    std::fs::create_dir_all(&work).unwrap();
    sh(tmp.path(), "git init -q --bare -b main origin.git");
    let config = serde_json::json!({ "groups": {
        "unit": { "tasks": { "test": { "run": "true" } } },
        "e2e": { "on": ["queue", "main"], "tasks": { "test": { "run": "true" } } },
        "deploy": { "on": ["main"], "tasks": { "ship": { "run": "true", "cache": false } } },
    } });
    write(&work, "pit.config.json", &config.to_string());
    sh(&work, "git init -q -b main && git add -A && git commit -qm init && git remote add origin ../origin.git && git push -q origin main");
    let groups = |v: &RunView| {
        let mut g: Vec<String> = v.groups.iter().map(|g| g.name.clone()).collect();
        g.sort();
        g
    };
    // Every group has a group (so required checks report); the ones this
    // kind of run leaves out pass with skipped tasks.
    let ran = |v: &RunView| {
        let mut g: Vec<String> = v
            .groups
            .iter()
            .filter(|g| g.tasks.iter().any(|t| t.status != Status::Skipped))
            .map(|g| g.name.clone())
            .collect();
        g.sort();
        g
    };
    let local = run(&pit, &work).await;
    assert_eq!(groups(&local), ["deploy", "e2e", "unit"], "{local:?}");
    assert_eq!(ran(&local), ["unit"], "{local:?}");
    assert_eq!(local.status, Status::Passed);
    let e2e = local.groups.iter().find(|g| g.name == "e2e").unwrap();
    assert_eq!(e2e.status, Status::Passed);
    assert_eq!(e2e.tasks[0].status, Status::Skipped);
    assert!(e2e
        .message
        .as_deref()
        .unwrap_or_default()
        .contains("runs only in queue, main runs"));
    let named = pit::cli::submit(
        &pit.client,
        &work,
        vec!["e2e.test".into()],
        Default::default(),
    )
    .await
    .unwrap();
    let named = tokio::time::timeout(
        Duration::from_secs(60),
        pit::cli::wait(&pit.client, &named.run_id, |_| {}),
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(ran(&named), ["e2e"], "{named:?}");
    let sha = sh(&work, "git rev-parse HEAD").trim().to_string();
    let url = tmp.path().join("origin.git").to_string_lossy().to_string();
    let main = run_ref(&pit, &url, sha.clone(), "main", true).await;
    assert_eq!(ran(&main), ["deploy", "e2e", "unit"], "{main:?}");
    assert_eq!(main.status, Status::Passed);
    // Tasks named in a schedule run whatever their group's `on` says, like
    // tasks named on a local run.
    let sha = sh(&work, "git rev-parse HEAD").trim().to_string();
    let scheduled = pit
        .app
        .create_run(
            CreateRun {
                repo: "affected".into(),
                url: Some(url.clone()),
                sha: sha.clone(),
                overlay: None,
                tasks: vec!["e2e.test".into()],
                trigger: Some("schedule:nightly".into()),
                git_ref: Some("main".into()),
                no_cache: true,
                fresh: false,
                debug: false,
                counts_as: None,
                no_secrets: false,
                trusted: true,
            },
            None,
        )
        .await
        .unwrap();
    let scheduled = tokio::time::timeout(
        Duration::from_secs(60),
        pit::cli::wait(&pit.client, &scheduled, |_| {}),
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(ran(&scheduled), ["e2e"], "{scheduled:?}");
    // A client naming a trigger through the API still counts as local: it
    // cannot run the `main`-only deploy.
    let api: serde_json::Value = pit
        .client
        .post(
            "/api/runs",
            &serde_json::json!({ "repo": "affected", "url": url, "sha": sha, "trigger": "post-merge", "ref": "main" }),
        )
        .await
        .unwrap();
    let api = tokio::time::timeout(
        Duration::from_secs(60),
        pit::cli::wait(&pit.client, api["id"].as_str().unwrap(), |_| {}),
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(ran(&api), ["unit"], "{api:?}");
    // A typo in `on` is a config error.
    let bad = serde_json::json!({ "groups": { "x": { "on": ["pull_request"], "tasks": { "t": { "run": "true" } } } } });
    write(&work, "pit.config.json", &bad.to_string());
    let view = run(&pit, &work).await;
    assert_eq!(view.status, Status::Error);
    assert!(view.message.unwrap_or_default().contains("unknown `on`"));
}

/// A group a pull request run leaves out (`on: ["main"]`) still posts its
/// status, as success, so a required check for it never waits forever.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn groups_left_out_by_on_still_report_their_check() {
    use axum::routing::post;
    let statuses: std::sync::Arc<parking_lot::Mutex<Vec<serde_json::Value>>> = Default::default();
    let seen = statuses.clone();
    let mock = axum::Router::new().route(
        "/repos/acme/web/statuses/{sha}",
        post(move |axum::Json(b): axum::Json<serde_json::Value>| {
            let seen = seen.clone();
            async move {
                seen.lock().push(b);
                axum::Json(serde_json::json!({}))
            }
        }),
    );
    let mock_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let mock_url = format!("http://{}", mock_listener.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(mock_listener, mock).await });
    let data = tempfile::tempdir().unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let github =
        pit::server::github::GitHub::with_token(&mock_url, "t", Some("s3cret".into())).unwrap();
    let app = pit::server::App::with_github(
        &data.path().join("server"),
        None,
        base.clone(),
        Some(github),
    )
    .unwrap();
    tokio::spawn(pit::server::serve_listener(app.clone(), listener));
    tokio::spawn(pit::worker::run(pit::worker::WorkerOpts {
        server: base.clone(),
        token: None,
        name: Some("left-out".into()),
        data_dir: data.path().join("worker"),
        slots: 1,
        parallelism: 1,
        workspace_max_bytes: 0,
        worker_token: None,
        secrets_file: None,
    }));
    let repo = tempfile::tempdir().unwrap();
    let config = serde_json::json!({ "groups": {
        "unit": { "tasks": { "t": { "run": "true" } } },
        "deploy": { "on": ["main"], "tasks": { "ship": { "run": "exit 1" } } },
        "lint": { "on": ["pr"], "tasks": { "l": { "run": "exit 1", "cache": false } } },
    } });
    write(repo.path(), "pit.config.json", &config.to_string());
    sh(
        repo.path(),
        "git init -q -b main && git add -A && git commit -qm init",
    );
    let sha = sh(repo.path(), "git rev-parse HEAD").trim().to_string();
    app.db
        .upsert_repo(
            "acme/web",
            Some(&repo.path().to_string_lossy()),
            Some("acme/web"),
        )
        .unwrap();
    let id = app
        .create_run(
            CreateRun {
                repo: "acme/web".into(),
                url: None,
                sha: sha.clone(),
                overlay: None,
                tasks: vec![],
                trigger: Some("pr".into()),
                git_ref: Some("pull/1".into()),
                no_cache: false,
                fresh: false,
                debug: false,
                counts_as: None,
                no_secrets: false,
                trusted: false,
            },
            Some(1),
        )
        .await
        .unwrap();
    let client = Client::new(&base, None).unwrap();
    let view = tokio::time::timeout(
        Duration::from_secs(60),
        pit::cli::wait(&client, &id, |_| {}),
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(
        view.status,
        Status::Failed,
        "lint fails in the PR run: {view:?}"
    );
    let last = |ctx: &str| {
        statuses
            .lock()
            .iter()
            .rev()
            .find(|s| s["context"] == ctx)
            .map(|s| s["state"].as_str().unwrap_or_default().to_string())
    };
    tokio::time::timeout(Duration::from_secs(10), async {
        while last("pit / deploy").is_none() || last("pit / lint").is_none() {
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    })
    .await
    .expect("statuses were not posted");
    assert_eq!(last("pit / deploy").as_deref(), Some("success"));
    assert_eq!(last("pit / lint").as_deref(), Some("failure"));
    // A branch push of the same commit leaves lint out, and must not turn
    // the PR run's failed check green.
    let push = app
        .create_run(
            CreateRun {
                repo: "acme/web".into(),
                url: None,
                sha: sha.clone(),
                overlay: None,
                tasks: vec![],
                trigger: Some("push".into()),
                git_ref: Some("feature".into()),
                no_cache: false,
                fresh: false,
                debug: false,
                counts_as: None,
                no_secrets: false,
                trusted: false,
            },
            None,
        )
        .await
        .unwrap();
    let pushed = tokio::time::timeout(
        Duration::from_secs(60),
        pit::cli::wait(&client, &push, |_| {}),
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(pushed.status, Status::Passed, "{pushed:?}");
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert_eq!(
        last("pit / lint").as_deref(),
        Some("failure"),
        "overwritten by the push run"
    );
    // GitHub's Re-run on the PR run's lint check repeats that run, as a PR
    // run: lint runs again (and fails), it is not left out.
    use hmac::Mac;
    let lint_group = view
        .groups
        .iter()
        .find(|g| g.name == "lint")
        .unwrap()
        .id
        .clone();
    let body = serde_json::json!({ "action": "rerequested",
        "repository": { "full_name": "acme/web", "clone_url": repo.path().to_string_lossy() },
        "check_run": { "head_sha": sha, "external_id": lint_group } })
    .to_string();
    let mut mac = hmac::Hmac::<sha2::Sha256>::new_from_slice(b"s3cret").unwrap();
    mac.update(body.as_bytes());
    let sig = format!("sha256={}", hex::encode(mac.finalize().into_bytes()));
    let rerun: serde_json::Value = reqwest::Client::new()
        .post(format!("{base}/webhooks/github"))
        .header("x-github-event", "check_run")
        .header("x-hub-signature-256", sig)
        .body(body)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let rerun = tokio::time::timeout(
        Duration::from_secs(60),
        pit::cli::wait(&client, rerun["run"].as_str().expect("a rerun"), |_| {}),
    )
    .await
    .unwrap()
    .unwrap();
    let lint = rerun.groups.iter().find(|g| g.name == "lint").unwrap();
    assert_eq!(lint.tasks[0].status, Status::Failed, "{rerun:?}");
}

/// GitHub takes the newest check run of a name as the check. When a
/// left-out copy of a group (another run of the commit, `on`) posted a newer
/// check run than the real group's, the real group posts a new one rather
/// than updating its old, now hidden, one.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_real_check_is_reposted_over_a_newer_left_out_one() {
    use axum::routing::{get, patch, post};
    type Seen = std::sync::Arc<parking_lot::Mutex<Vec<(String, serde_json::Value)>>>;
    let seen: Seen = Default::default();
    let next = std::sync::Arc::new(std::sync::atomic::AtomicU64::new(100));
    let (s1, n1, s2) = (seen.clone(), next.clone(), seen.clone());
    let mock = axum::Router::new()
        .route(
            "/repos/acme/web/installation",
            get(|| async { axum::Json(serde_json::json!({ "id": 1 })) }),
        )
        .route(
            "/app/installations/1/access_tokens",
            post(|| async { axum::Json(serde_json::json!({ "token": "t" })) }),
        )
        .route(
            "/repos/acme/web/check-runs",
            post(move |axum::Json(b): axum::Json<serde_json::Value>| {
                let (s, n) = (s1.clone(), n1.clone());
                async move {
                    let id = n.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                    s.lock().push((format!("POST {id}"), b));
                    axum::Json(serde_json::json!({ "id": id }))
                }
            }),
        )
        .route(
            "/repos/acme/web/check-runs/{id}",
            patch(
                move |axum::extract::Path(id): axum::extract::Path<u64>,
                      axum::Json(b): axum::Json<serde_json::Value>| {
                    let s = s2.clone();
                    async move {
                        s.lock().push((format!("PATCH {id}"), b));
                        axum::Json(serde_json::json!({ "id": id }))
                    }
                },
            ),
        );
    let mock_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let mock_url = format!("http://{}", mock_listener.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(mock_listener, mock).await });
    let keys = tempfile::tempdir().unwrap();
    sh(keys.path(), "openssl genrsa -out key.pem 2048 2>/dev/null");
    let pem = std::fs::read_to_string(keys.path().join("key.pem")).unwrap();
    let github = pit::server::github::GitHub::with_app(&mock_url, "1", &pem).unwrap();
    let data = tempfile::tempdir().unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let app = pit::server::App::with_github(
        &data.path().join("server"),
        None,
        base.clone(),
        Some(github),
    )
    .unwrap();
    tokio::spawn(pit::server::serve_listener(app.clone(), listener));
    tokio::spawn(pit::worker::run(pit::worker::WorkerOpts {
        server: base.clone(),
        token: None,
        name: Some("repost".into()),
        data_dir: data.path().join("worker"),
        slots: 1,
        parallelism: 1,
        workspace_max_bytes: 0,
        worker_token: None,
        secrets_file: None,
    }));
    let repo = tempfile::tempdir().unwrap();
    let config = serde_json::json!({ "groups": {
        "lint": { "on": ["pr"], "tasks": { "l": { "run": "exit 1", "cache": false } } },
    } });
    write(repo.path(), "pit.config.json", &config.to_string());
    sh(
        repo.path(),
        "git init -q -b main && git add -A && git commit -qm init",
    );
    let sha = sh(repo.path(), "git rev-parse HEAD").trim().to_string();
    app.db
        .upsert_repo(
            "acme/web",
            Some(&repo.path().to_string_lossy()),
            Some("acme/web"),
        )
        .unwrap();
    let client = Client::new(&base, None).unwrap();
    let start = |trigger: &str, git_ref: &str, pr: Option<u64>| {
        let (app, sha) = (app.clone(), sha.clone());
        let (trigger, git_ref) = (trigger.to_string(), git_ref.to_string());
        let client = client.clone();
        async move {
            let id = app
                .create_run(
                    CreateRun {
                        repo: "acme/web".into(),
                        url: None,
                        sha,
                        overlay: None,
                        tasks: vec![],
                        trigger: Some(trigger),
                        git_ref: Some(git_ref),
                        no_cache: false,
                        fresh: false,
                        debug: false,
                        counts_as: None,
                        no_secrets: false,
                        trusted: false,
                    },
                    pr,
                )
                .await
                .unwrap();
            tokio::time::timeout(
                Duration::from_secs(60),
                pit::cli::wait(&client, &id, |_| {}),
            )
            .await
            .unwrap()
            .unwrap()
        }
    };
    // The branch push leaves lint out and posts its "skipped" first.
    let push = start("push", "feature", None).await;
    let push_lint = push
        .groups
        .iter()
        .find(|g| g.name == "lint")
        .unwrap()
        .id
        .clone();
    let pr = start("pr", "pull/1", Some(1)).await;
    assert_eq!(pr.status, Status::Failed);
    tokio::time::sleep(Duration::from_millis(500)).await;
    // The race the reporting must survive: the left-out copy's check run
    // ends up newer than the real one.
    // The left-out copy was posted as "skipped" (passing for a required
    // check), the real one as a failure.
    let conclusions: Vec<String> = seen
        .lock()
        .iter()
        .filter(|(_, b)| b["name"] == "pit / lint")
        .filter_map(|(_, b)| b["conclusion"].as_str().map(String::from))
        .collect();
    assert!(
        conclusions.contains(&"skipped".to_string()),
        "{conclusions:?}"
    );
    assert_eq!(
        conclusions.last().map(String::as_str),
        Some("failure"),
        "{conclusions:?}"
    );
    app.db.set_check_run(&push_lint, 10_000).unwrap();
    seen.lock().clear();
    app.github.as_ref().unwrap().report_run(&app, &pr.id).await;
    let sent = seen.lock().clone();
    let reposted = sent
        .iter()
        .find(|(how, b)| how.starts_with("POST") && b["name"] == "pit / lint")
        .unwrap_or_else(|| panic!("the real check was not posted anew: {sent:?}"));
    assert_eq!(reposted.1["conclusion"], "failure");
}

/// Four quick pushes to a default branch called `develop`, each deploying
/// under one lock: all four run, one at a time, in order. None is cancelled,
/// which GitHub's default concurrency queue does to the waiting ones.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn every_deploy_runs_in_order_under_a_lock() {
    let pit = start().await;
    let tmp = tempfile::tempdir().unwrap();
    let work = tmp.path().join("work");
    std::fs::create_dir_all(&work).unwrap();
    sh(tmp.path(), "git init -q --bare -b develop origin.git");
    let log = tmp.path().join("deploys.log");
    // Groups planned before the deploy (names sort first), so an older run
    // is half planned while a newer one could take the lock.
    let config = serde_json::json!({ "groups": {
        "a-build": { "tasks": { "b": { "run": "true", "cache": false } } },
        "b-test": { "tasks": { "t": { "run": "true", "cache": false } } },
        "deploy": { "lock": "prod", "tasks": { "ship": {
            "run": format!("echo start $PIT_SHA >> {l}; sleep 1; echo end $PIT_SHA >> {l}", l = log.display()),
            "cache": false,
        } } },
    } });
    write(&work, "pit.config.json", &config.to_string());
    sh(&work, "git init -q -b develop && git add -A && git commit -qm c0 && git remote add origin ../origin.git && git push -q origin develop");
    let url = tmp.path().join("origin.git").to_string_lossy().to_string();
    let mut shas = vec![];
    let mut ids = vec![];
    for i in 1..=4 {
        sh(
            &work,
            &format!("git commit -q --allow-empty -m c{i} && git push -q origin develop"),
        );
        let sha = sh(&work, "git rev-parse HEAD").trim().to_string();
        let id = pit
            .app
            .create_run(
                CreateRun {
                    repo: "deploys".into(),
                    url: Some(url.clone()),
                    sha: sha.clone(),
                    overlay: None,
                    tasks: vec![],
                    trigger: Some("push".into()),
                    git_ref: Some("develop".into()),
                    no_cache: false,
                    fresh: false,
                    debug: false,
                    counts_as: None,
                    no_secrets: false,
                    trusted: true,
                },
                None,
            )
            .await
            .unwrap();
        shas.push(sha);
        ids.push(id);
    }
    for id in &ids {
        let v = tokio::time::timeout(
            Duration::from_secs(60),
            pit::cli::wait(&pit.client, id, |_| {}),
        )
        .await
        .unwrap()
        .unwrap();
        assert_eq!(v.status, Status::Passed, "{v:?}");
    }
    let lines: Vec<String> = std::fs::read_to_string(&log)
        .unwrap()
        .lines()
        .map(String::from)
        .collect();
    let expected: Vec<String> = shas
        .iter()
        .flat_map(|s| [format!("start {s}"), format!("end {s}")])
        .collect();
    assert_eq!(lines, expected, "one at a time, in push order");
}

/// `$PIT_SUMMARY` lives outside the workspace: the checkout stays clean
/// (`git status` checks keep passing) and nothing a task puts in the
/// workspace can point the worker's summary reads elsewhere.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn summaries_live_outside_the_workspace() {
    let pit = start().await;
    let repo = tempfile::tempdir().unwrap();
    let dir = repo.path();
    let config = serde_json::json!({ "tasks": { "t": {
        "run": "case \"$PIT_SUMMARY\" in \"$PWD\"*) exit 3;; esac; [ \"$GITHUB_STEP_SUMMARY\" = \"$PIT_SUMMARY\" ] || exit 4; echo '## hi' >> $PIT_SUMMARY; test -z \"$(git status --porcelain)\"",
        "cache": false,
    } } });
    write(dir, "pit.config.json", &config.to_string());
    sh(
        dir,
        "git init -q -b main && git add -A && git commit -qm init",
    );
    let view = run(&pit, dir).await;
    assert_eq!(view.status, Status::Passed, "{view:?}");
    assert_eq!(view.groups[0].tasks[0].summary.as_deref(), Some("## hi\n"));
    // In a container, through the mounted directory.
    if cfg!(target_os = "linux")
        && pit::worker::exec::detect_backends()
            .await
            .contains(&Backend::Docker)
    {
        let config = serde_json::json!({ "tasks": { "t": {
            "image": "node:22-alpine",
            "run": "cd /tmp && echo \"node $(node --version)\" >> $PIT_SUMMARY",
            "cache": false,
        } } });
        write(dir, "pit.config.json", &config.to_string());
        sh(dir, "git commit -qam docker");
        let view = run_with_timeout(&pit, dir, Duration::from_secs(240)).await;
        assert_eq!(view.status, Status::Passed, "{view:?}");
        let md = view.groups[0].tasks[0].summary.clone().unwrap_or_default();
        assert!(md.starts_with("node v22"), "{md:?}");
    }
}

/// A `docker` task's `$HOME` survives between runs (tool caches stay warm),
/// one per trust pool; a `--fresh` run gets an empty one.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn container_homes_persist_per_trust_pool() {
    if !(cfg!(target_os = "linux")
        && pit::worker::exec::detect_backends()
            .await
            .contains(&Backend::Docker))
    {
        eprintln!("skipped: no Linux docker");
        return;
    }
    let pit = start().await;
    let tmp = tempfile::tempdir().unwrap();
    let work = tmp.path().join("work");
    std::fs::create_dir_all(&work).unwrap();
    sh(tmp.path(), "git init -q --bare -b main origin.git");
    let config = serde_json::json!({ "tasks": { "t": {
        "image": "node:22-alpine",
        "run": "if [ -f $HOME/cache-marker ]; then echo warm; else echo cold; touch $HOME/cache-marker; fi",
        "cache": false,
    } } });
    write(&work, "pit.config.json", &config.to_string());
    sh(&work, "git init -q -b main && git add -A && git commit -qm init && git remote add origin ../origin.git && git push -q origin main");
    let first = run_with_timeout(&pit, &work, Duration::from_secs(240)).await;
    assert!(first_log(&pit, &first).await.contains("cold"));
    let second = run(&pit, &work).await;
    assert!(
        first_log(&pit, &second).await.contains("warm"),
        "the home did not persist"
    );
    let fresh = run_with(
        &pit,
        &work,
        pit::cli::RunOpts {
            fresh: true,
            ..Default::default()
        },
    )
    .await;
    assert!(
        first_log(&pit, &fresh).await.contains("cold"),
        "a fresh run reused the home"
    );
    // A trusted run (main) has a home of its own: the branch's is not shared.
    let sha = sh(&work, "git rev-parse HEAD").trim().to_string();
    let url = tmp.path().join("origin.git").to_string_lossy().to_string();
    let main = run_ref(&pit, &url, sha, "main", true).await;
    assert!(
        first_log(&pit, &main).await.contains("cold"),
        "trusted runs shared the untrusted home"
    );
    // Another repo on the worker has its own home too.
    let other = tmp.path().join("other");
    std::fs::create_dir_all(&other).unwrap();
    write(&other, "pit.config.json", &config.to_string());
    sh(
        &other,
        "git init -q -b main && git add -A && git commit -qm init",
    );
    let other_run = run(&pit, &other).await;
    assert!(
        first_log(&pit, &other_run).await.contains("cold"),
        "repos shared a home"
    );
    // So does another group of the same repo (a credential one group's tool
    // writes to `$HOME` is not another's to read).
    let regrouped = serde_json::json!({ "groups": { "another": { "tasks": { "t": config["tasks"]["t"].clone() } } } });
    write(&work, "pit.config.json", &regrouped.to_string());
    sh(&work, "git commit -qam regroup");
    let another = run(&pit, &work).await;
    assert!(
        first_log(&pit, &another).await.contains("cold"),
        "groups shared a home"
    );
}

/// The log of a run's first task.
async fn first_log(pit: &Pit, view: &RunView) -> String {
    let task = &view.groups[0].tasks[0].id;
    pit.client
        .get_text(&format!("/api/tasks/{task}/log"))
        .await
        .unwrap()
}

/// Reporting a run posts each group's status only when it changed: queued,
/// running, done. Re-posting every group on every event would be about 2N²
/// calls for N groups, and GitHub's secondary rate limits stop that.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn github_reporting_posts_each_change_once() {
    use axum::routing::post;
    let calls = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let c = calls.clone();
    let mock = axum::Router::new().route(
        "/repos/acme/many/statuses/{sha}",
        post(move || {
            let c = c.clone();
            async move {
                c.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                axum::Json(serde_json::json!({}))
            }
        }),
    );
    let mock_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let mock_url = format!("http://{}", mock_listener.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(mock_listener, mock).await });
    let data = tempfile::tempdir().unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let github = pit::server::github::GitHub::with_token(&mock_url, "t", None).unwrap();
    let app = pit::server::App::with_github(
        &data.path().join("server"),
        None,
        base.clone(),
        Some(github),
    )
    .unwrap();
    tokio::spawn(pit::server::serve_listener(app.clone(), listener));
    tokio::spawn(pit::worker::run(pit::worker::WorkerOpts {
        server: base.clone(),
        token: None,
        name: Some("many".into()),
        data_dir: data.path().join("worker"),
        slots: 4,
        parallelism: 1,
        workspace_max_bytes: 0,
        worker_token: None,
        secrets_file: None,
    }));
    let repo = tempfile::tempdir().unwrap();
    let groups: serde_json::Map<String, serde_json::Value> = (0..20)
        .map(|i| {
            (
                format!("g{i:02}"),
                serde_json::json!({ "tasks": { "t": { "run": "true", "cache": false } } }),
            )
        })
        .collect();
    write(
        repo.path(),
        "pit.config.json",
        &serde_json::json!({ "groups": groups }).to_string(),
    );
    sh(
        repo.path(),
        "git init -q -b main && git add -A && git commit -qm init",
    );
    let sha = sh(repo.path(), "git rev-parse HEAD").trim().to_string();
    app.db
        .upsert_repo(
            "acme/many",
            Some(&repo.path().to_string_lossy()),
            Some("acme/many"),
        )
        .unwrap();
    let id = app
        .create_run(
            CreateRun {
                repo: "acme/many".into(),
                url: None,
                sha,
                overlay: None,
                tasks: vec![],
                trigger: Some("pr".into()),
                git_ref: Some("pull/1".into()),
                no_cache: false,
                fresh: false,
                debug: false,
                counts_as: None,
                no_secrets: false,
                trusted: false,
            },
            Some(1),
        )
        .await
        .unwrap();
    let client = Client::new(&base, None).unwrap();
    let view = tokio::time::timeout(
        Duration::from_secs(120),
        pit::cli::wait(&client, &id, |_| {}),
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(view.status, Status::Passed);
    tokio::time::sleep(Duration::from_secs(1)).await;
    let n = calls.load(std::sync::atomic::Ordering::SeqCst);
    eprintln!("status calls for 20 groups: {n}");
    // About three changes per group (queued, running, passed), not O(N²):
    // re-posting every group on every event was 820 calls here.
    assert!(n <= 20 * 4, "{n} calls");
}

/// A secondary rate limit from GitHub (403 with Retry-After) does not lose
/// a check: the run is reported again after the wait, and every group ends
/// with its final status.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn rate_limited_reports_are_retried() {
    use axum::routing::post;
    type Seen = std::sync::Arc<parking_lot::Mutex<Vec<serde_json::Value>>>;
    let seen: Seen = Default::default();
    let refused = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let (s, r) = (seen.clone(), refused.clone());
    let mock = axum::Router::new().route(
        "/repos/acme/limited/statuses/{sha}",
        post(move |axum::Json(b): axum::Json<serde_json::Value>| {
            let (s, r) = (s.clone(), r.clone());
            async move {
                // The first eight requests hit the limit.
                if r.fetch_add(1, std::sync::atomic::Ordering::SeqCst) < 8 {
                    return (
                        axum::http::StatusCode::FORBIDDEN,
                        [("retry-after", "1")],
                        "You have exceeded a secondary rate limit",
                    )
                        .into_response();
                }
                s.lock().push(b);
                axum::Json(serde_json::json!({})).into_response()
            }
        }),
    );
    use axum::response::IntoResponse;
    let mock_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let mock_url = format!("http://{}", mock_listener.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(mock_listener, mock).await });
    let data = tempfile::tempdir().unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let github = pit::server::github::GitHub::with_token(&mock_url, "t", None).unwrap();
    let app = pit::server::App::with_github(
        &data.path().join("server"),
        None,
        base.clone(),
        Some(github),
    )
    .unwrap();
    tokio::spawn(pit::server::serve_listener(app.clone(), listener));
    tokio::spawn(pit::worker::run(pit::worker::WorkerOpts {
        server: base.clone(),
        token: None,
        name: Some("limited".into()),
        data_dir: data.path().join("worker"),
        slots: 2,
        parallelism: 1,
        workspace_max_bytes: 0,
        worker_token: None,
        secrets_file: None,
    }));
    let repo = tempfile::tempdir().unwrap();
    let config = serde_json::json!({ "groups": {
        "a": { "tasks": { "t": { "run": "true", "cache": false } } },
        "b": { "tasks": { "t": { "run": "true", "cache": false } } },
        "c": { "tasks": { "t": { "run": "exit 1", "cache": false } } },
    } });
    write(repo.path(), "pit.config.json", &config.to_string());
    sh(
        repo.path(),
        "git init -q -b main && git add -A && git commit -qm init",
    );
    let sha = sh(repo.path(), "git rev-parse HEAD").trim().to_string();
    app.db
        .upsert_repo(
            "acme/limited",
            Some(&repo.path().to_string_lossy()),
            Some("acme/limited"),
        )
        .unwrap();
    let id = app
        .create_run(
            CreateRun {
                repo: "acme/limited".into(),
                url: None,
                sha,
                overlay: None,
                tasks: vec![],
                trigger: Some("pr".into()),
                git_ref: Some("pull/1".into()),
                no_cache: false,
                fresh: false,
                debug: false,
                counts_as: None,
                no_secrets: false,
                trusted: false,
            },
            Some(1),
        )
        .await
        .unwrap();
    let client = Client::new(&base, None).unwrap();
    tokio::time::timeout(
        Duration::from_secs(60),
        pit::cli::wait(&client, &id, |_| {}),
    )
    .await
    .unwrap()
    .unwrap();
    let last = |ctx: &str| {
        seen.lock()
            .iter()
            .rev()
            .find(|b| b["context"] == ctx)
            .map(|b| b["state"].as_str().unwrap_or_default().to_string())
    };
    tokio::time::timeout(Duration::from_secs(20), async {
        while last("pit / a").as_deref() != Some("success")
            || last("pit / b").as_deref() != Some("success")
            || last("pit / c").as_deref() != Some("failure")
        {
            tokio::time::sleep(Duration::from_millis(200)).await;
        }
    })
    .await
    .unwrap_or_else(|_| {
        panic!(
            "final statuses were lost: a={:?} b={:?} c={:?}",
            last("pit / a"),
            last("pit / b"),
            last("pit / c")
        )
    });
    assert!(refused.load(std::sync::atomic::Ordering::SeqCst) >= 8);
}

/// `artifacts` keeps files from a failed run too, serves each one as itself
/// (no zip), and `pit artifacts` downloads them; a file an earlier run left
/// in the warm workspace is not kept again.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn artifacts_are_kept_from_failed_runs_and_served_as_files() {
    let pit = start().await;
    let repo = tempfile::tempdir().unwrap();
    let dir = repo.path();
    let config = |run: &str| {
        serde_json::json!({ "tasks": { "e2e": {
            "run": run, "artifacts": ["report/**"], "cache": false,
        } } })
        .to_string()
    };
    write(dir, ".gitignore", "report/\n");
    write(dir, "pit.config.json", &config("mkdir -p report && echo old > report/stale.txt && echo shot > report/fail.png && exit 1"));
    sh(
        dir,
        "git init -q -b main && git add -A && git commit -qm init",
    );
    let first = run(&pit, dir).await;
    assert_eq!(first.status, Status::Failed);
    let t = &first.groups[0].tasks[0];
    let mut paths: Vec<&str> = t.artifacts.iter().map(|a| a.path.as_str()).collect();
    paths.sort();
    assert_eq!(paths, ["report/fail.png", "report/stale.txt"]);
    let body = pit
        .client
        .get_text(&format!("/api/tasks/{}/artifacts/report/fail.png", t.id))
        .await
        .unwrap();
    assert_eq!(body, "shot\n");
    // `pit artifacts` writes them under <out>/<group>/<task>/.
    let out = tempfile::tempdir().unwrap();
    assert_eq!(
        pit::cli::artifacts(&pit.client, &first.id, Some("e2e"), out.path())
            .await
            .unwrap(),
        0
    );
    assert_eq!(
        std::fs::read_to_string(out.path().join("default/e2e/report/fail.png")).unwrap(),
        "shot\n"
    );
    // The next run writes only fail.png; stale.txt from the first run (still
    // in the warm workspace before the task) is not kept again.
    write(
        dir,
        "pit.config.json",
        &config("mkdir -p report && echo shot2 > report/fail.png"),
    );
    sh(dir, "git commit -qam again");
    let second = run(&pit, dir).await;
    assert_eq!(second.status, Status::Passed);
    let paths: Vec<&str> = second.groups[0].tasks[0]
        .artifacts
        .iter()
        .map(|a| a.path.as_str())
        .collect();
    assert_eq!(paths, ["report/fail.png"]);
    // A glob that also matches a committed file the task reads deletes
    // nothing, and keeps only what the task wrote.
    write(dir, "snapshots/baseline.png", "committed\n");
    let config = serde_json::json!({ "tasks": { "e2e": {
        "run": "cat snapshots/baseline.png > /dev/null && echo new > snapshots/diff.png",
        "artifacts": ["**/*.png"], "cache": false,
    } } });
    write(dir, "pit.config.json", &config.to_string());
    sh(dir, "git add -A && git commit -qm baseline");
    let third = run(&pit, dir).await;
    assert_eq!(third.status, Status::Passed, "{third:?}");
    let mut paths: Vec<&str> = third.groups[0].tasks[0]
        .artifacts
        .iter()
        .map(|a| a.path.as_str())
        .collect();
    paths.sort();
    assert_eq!(paths, ["snapshots/diff.png"]);
}

/// Interrupting `pit run` (Ctrl-C, or SIGTERM from a timeout or an agent)
/// cancels its run, so the task stops instead of holding a worker until it
/// ends; `pit cancel` says when there was nothing left to cancel.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn interrupting_pit_run_cancels_its_run() {
    let pit = start().await;
    let base = pit.base.clone();
    let tmp = tempfile::tempdir().unwrap();
    let repo = tmp.path().join("repo");
    std::fs::create_dir_all(&repo).unwrap();
    let started = tmp.path().join("started");
    let done = tmp.path().join("done");
    let task = format!(
        "touch {} && sleep 8 && touch {}",
        started.display(),
        done.display()
    );
    write(
        &repo,
        "pit.config.json",
        &serde_json::json!({ "tasks": { "long": { "run": task, "cache": false } } }).to_string(),
    );
    sh(
        &repo,
        "git init -q -b main && git add -A && git commit -qm init",
    );
    for signal in ["INT", "TERM"] {
        let _ = std::fs::remove_file(&started);
        let mut child = tokio::process::Command::new(env!("CARGO_BIN_EXE_pit"))
            .args(["run", "--server", &base])
            .current_dir(&repo)
            .stderr(std::process::Stdio::null())
            .spawn()
            .unwrap();
        for _ in 0..300 {
            if started.exists() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert!(started.exists(), "task never started");
        let pid = child.id().unwrap().to_string();
        sh(&repo, &format!("kill -{signal} {pid}"));
        let status = tokio::time::timeout(Duration::from_secs(10), child.wait())
            .await
            .expect("pit run kept waiting after the interrupt")
            .unwrap();
        assert_eq!(status.code(), Some(130), "SIG{signal}");
        let runs: Vec<serde_json::Value> = pit.client.get("/api/runs?limit=1").await.unwrap();
        let id = runs[0]["id"].as_str().unwrap().to_string();
        let v: RunView = pit.client.get(&format!("/api/runs/{id}")).await.unwrap();
        assert_eq!(v.status, Status::Failed, "SIG{signal}: {v:?}");
        assert_eq!(v.message.as_deref(), Some("cancelled"));
        // Nothing left to cancel: said so, not reported as done.
        let again: serde_json::Value = pit
            .client
            .post(&format!("/api/runs/{id}/cancel"), &())
            .await
            .unwrap();
        assert_eq!(again["cancelled"], false);
        assert_eq!(again["status"], "failed");
    }
    // Long enough for an unstopped task to have finished.
    tokio::time::sleep(Duration::from_secs(9)).await;
    assert!(!done.exists(), "the interrupted run's task kept running");
    let unknown = pit
        .client
        .post::<_, serde_json::Value>("/api/runs/run_nope/cancel", &())
        .await
        .unwrap_err();
    assert!(unknown.to_string().contains("404"), "{unknown:#}");
}

/// `pit schedules` says when each schedule fires next and how it last ran,
/// and why none fire when the config does not load; `pit schedules run`
/// starts one now on the default branch's tip, without using up its slot.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn schedules_are_listed_and_can_run_now() {
    let pit = start().await;
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().canonicalize().unwrap();
    let work = root.join("work");
    std::fs::create_dir_all(&work).unwrap();
    sh(&root, "git init -q --bare -b main origin.git");
    let config = |schedules: serde_json::Value| {
        serde_json::json!({
            "tasks": { "check": { "run": "echo checked $PIT_SHA", "cache": false } },
            "schedules": schedules,
        })
        .to_string()
    };
    write(
        &work,
        "pit.config.json",
        &config(serde_json::json!({ "nightly": { "cron": "17 3 * * *" } })),
    );
    sh(&work, "git init -q -b main && git add -A && git commit -qm init && git remote add origin ../origin.git && git push -q origin main");
    assert_eq!(run(&pit, &work).await.status, Status::Passed);

    let list: Vec<serde_json::Value> = pit.client.get("/api/schedules").await.unwrap();
    assert_eq!(list.len(), 1, "{list:?}");
    let repo = list[0]["repo"].as_str().unwrap().to_string();
    assert_eq!(list[0]["name"], "nightly");
    let next = list[0]["nextAt"].as_i64().unwrap();
    assert_eq!(next.rem_euclid(86_400_000), (3 * 60 + 17) * 60_000);
    assert!(next > pit::model::now_ms());
    assert!(list[0]["lastRun"].is_null());

    // A schedule just pushed: unknown until its repo is fetched.
    write(
        &work,
        "pit.config.json",
        &config(serde_json::json!({
            "nightly": { "cron": "17 3 * * *" },
            "weekly": { "cron": "0 6 * * 1" },
        })),
    );
    sh(&work, "git commit -qam weekly && git push -q origin main");
    let tip = sh(&work, "git rev-parse HEAD").trim().to_string();
    let err = pit
        .client
        .post::<_, serde_json::Value>(
            "/api/schedules/run",
            &serde_json::json!({ "name": "weekly" }),
        )
        .await
        .unwrap_err();
    assert!(err.to_string().contains("--repo"), "{err:#}");
    let started: serde_json::Value = pit
        .client
        .post(
            "/api/schedules/run",
            &serde_json::json!({ "name": "weekly", "repo": repo }),
        )
        .await
        .unwrap();
    let id = started["id"].as_str().unwrap().to_string();
    let v = pit::cli::wait(&pit.client, &id, |_| {}).await.unwrap();
    assert_eq!(v.status, Status::Passed, "{v:?}");
    assert_eq!(v.trigger, "schedule:weekly");
    let log = pit
        .client
        .get_text(&format!("/api/runs/{id}/tasks/check/log"))
        .await
        .unwrap();
    assert!(log.contains(&format!("checked {tip}")), "{log}");

    let list: Vec<serde_json::Value> = pit.client.get("/api/schedules").await.unwrap();
    let weekly = list.iter().find(|s| s["name"] == "weekly").unwrap();
    assert_eq!(weekly["lastRun"]["id"], id.as_str());
    assert_eq!(weekly["lastRun"]["status"], "passed");

    // Running nightly by hand leaves its slot to fire as usual.
    let started: serde_json::Value = pit
        .client
        .post(
            "/api/schedules/run",
            &serde_json::json!({ "name": "nightly", "repo": repo }),
        )
        .await
        .unwrap();
    pit::cli::wait(&pit.client, started["id"].as_str().unwrap(), |_| {})
        .await
        .unwrap();
    let day = pit::model::now_ms().div_euclid(86_400_000) + 1;
    pit::server::schedule::tick(&pit.app, (day * 1440 + 3 * 60 + 17) * 60_000)
        .await
        .unwrap();
    assert_eq!(count_trigger(&pit, "nightly").await, 2);

    // A config that does not load is listed as the reason nothing fires.
    write(&work, "pit.config.json", "{ not json");
    sh(&work, "git commit -qam broken && git push -q origin main");
    pit.client
        .post::<_, serde_json::Value>(
            "/api/schedules/run",
            &serde_json::json!({ "name": "nightly", "repo": repo }),
        )
        .await
        .unwrap_err();
    let list: Vec<serde_json::Value> = pit.client.get("/api/schedules").await.unwrap();
    assert_eq!(list.len(), 1, "{list:?}");
    assert!(
        list[0]["error"]
            .as_str()
            .unwrap()
            .contains("pit.config.json"),
        "{list:?}"
    );
}
