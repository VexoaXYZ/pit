//! Loads `pit.config.ts`. The file is evaluated by the JavaScript runtime that
//! is already on the machine (Node 22.6+ strips types natively, or Bun), so the
//! Pit binary does not embed a JS engine. `pit.config.json` works without one.

use std::path::Path;
use std::process::Stdio;

use anyhow::{bail, Context, Result};
use parking_lot::Mutex;
use std::collections::HashMap;
use std::sync::OnceLock;

use crate::model::PitConfig;

pub const CONFIG_TS: &str = "pit.config.ts";
pub const CONFIG_JSON: &str = "pit.config.json";
pub const CONFIG_FILES: &[&str] = &[CONFIG_TS, CONFIG_JSON];

const EVAL_SCRIPT: &str = r#"
const { pathToFileURL } = require("node:url");
const ctx = JSON.parse(require("node:fs").readFileSync(process.argv[process.argv.length - 1], "utf8"));
import(pathToFileURL(process.argv[process.argv.length - 2]).href).then(
  (m) => {
    let c = m.default ?? m.config ?? m;
    if (typeof c === "function") c = c(ctx);
    return Promise.resolve(c).then((v) => process.stdout.write(JSON.stringify(v)));
  },
  (e) => { console.error(String(e && e.stack || e)); process.exit(1); },
);
"#;

/// The only variables a config evaluation sees.
const EVAL_ENV: &[&str] = &[
    "PATH",
    "HOME",
    "USERPROFILE",
    "SYSTEMROOT",
    "TEMP",
    "TMP",
    "APPDATA",
    "LOCALAPPDATA",
];

/// The image an untrusted config (a fork's) is evaluated in, unless
/// `PIT_CONFIG_IMAGE` names another with Node 22.6+.
pub const SANDBOX_IMAGE: &str = "node:22-alpine";

fn cache() -> &'static Mutex<HashMap<String, PitConfig>> {
    static CACHE: OnceLock<Mutex<HashMap<String, PitConfig>>> = OnceLock::new();
    CACHE.get_or_init(Default::default)
}

/// Parses config source. `name` is the file name it came from, which decides
/// whether it is JSON or TypeScript. A TypeScript config that exports a
/// function is called with `ctx` (see `workspace`). Results are memoized by
/// content hash. A `sandboxed` config (a fork's) runs in a container with no
/// network, no capabilities and nothing of the host but itself: evaluating
/// it on the server would hand the fork the server's files, signing key
/// included.
pub async fn parse(
    name: &str,
    source: &[u8],
    scratch: &Path,
    ctx: &crate::workspace::Context,
    sandboxed: bool,
) -> Result<PitConfig> {
    let ctx = serde_json::to_vec(ctx)?;
    let key = format!(
        "{name}:{}:{}",
        crate::cas::digest(source),
        crate::cas::digest(&ctx)
    );
    if let Some(c) = cache().lock().get(&key) {
        return Ok(c.clone());
    }
    let config = if name.ends_with(".json") {
        serde_json::from_slice::<PitConfig>(source).context("parsing pit.config.json")?
    } else {
        eval_ts(source, &ctx, scratch, sandboxed).await?
    };
    let config = normalize(config)?;
    cache().lock().insert(key, config.clone());
    Ok(config)
}

async fn eval_ts(source: &[u8], ctx: &[u8], scratch: &Path, sandboxed: bool) -> Result<PitConfig> {
    std::fs::create_dir_all(scratch)?;
    // `.mts` makes both Node and Bun treat the file as an ES module.
    // Unique per evaluation: two plans of the same commit may run at once.
    let id = crate::model::new_id("pit-config");
    let file = scratch.join(format!("{id}.mts"));
    let ctx_file = scratch.join(format!("{id}.json"));
    std::fs::write(&file, source)?;
    std::fs::write(&ctx_file, ctx)?;
    let result = if sandboxed {
        eval_sandboxed(&file, &ctx_file).await
    } else {
        eval_files(&file.to_string_lossy(), &ctx_file.to_string_lossy()).await
    };
    let _ = std::fs::remove_file(&file);
    let _ = std::fs::remove_file(&ctx_file);
    result
}

async fn eval_files(file_arg: &str, ctx_arg: &str) -> Result<PitConfig> {
    let mut last_err = String::new();
    for (bin, args) in runtimes() {
        let mut cmd = tokio::process::Command::new(&bin);
        cmd.args(&args).arg(EVAL_SCRIPT).arg(file_arg).arg(ctx_arg);
        cmd.stdin(Stdio::null()).kill_on_drop(true);
        // The config is repository code. It gets none of Pit's secrets.
        cmd.env_clear();
        for key in EVAL_ENV {
            if let Some(v) = std::env::var_os(key) {
                cmd.env(key, v);
            }
        }
        let out = match tokio::time::timeout(std::time::Duration::from_secs(30), cmd.output()).await
        {
            Ok(Ok(out)) => out,
            Ok(Err(e)) => {
                last_err = format!("{bin}: {e}");
                continue;
            }
            Err(_) => {
                bail!("evaluating {CONFIG_TS} with {bin} timed out")
            }
        };
        if !out.status.success() {
            bail!(
                "evaluating {CONFIG_TS} with {bin} failed:\n{}",
                String::from_utf8_lossy(&out.stderr).trim()
            );
        }
        return serde_json::from_slice(&out.stdout)
            .with_context(|| format!("{CONFIG_TS} must default-export a config object"));
    }
    bail!(
        "no JavaScript runtime found to evaluate {CONFIG_TS} (tried node and bun; set PIT_JS_RUNTIME, \
         or use {CONFIG_JSON}): {last_err}"
    )
}

async fn eval_sandboxed(file: &Path, ctx: &Path) -> Result<PitConfig> {
    // The container's user is nobody; the files hold repository data only.
    #[cfg(unix)]
    for f in [file, ctx] {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(f, std::fs::Permissions::from_mode(0o644))?;
    }
    let image = std::env::var("PIT_CONFIG_IMAGE").unwrap_or_else(|_| SANDBOX_IMAGE.into());
    let name = crate::model::new_id("pit-config");
    let mount =
        |src: &Path, dst: &str| format!("type=bind,src={},dst={dst},readonly", src.display());
    let mut cmd = tokio::process::Command::new("docker");
    cmd.args(["run", "--rm", "--name", &name])
        .args(["--network", "none", "--read-only", "--tmpfs", "/tmp"])
        .args(["--cap-drop", "ALL", "--security-opt", "no-new-privileges"])
        .args([
            "--user",
            "65534:65534",
            "--memory",
            "512m",
            "--pids-limit",
            "64",
        ])
        .arg("--mount")
        .arg(mount(file, "/pit/config.mts"))
        .arg("--mount")
        .arg(mount(ctx, "/pit/ctx.json"))
        .arg(&image)
        .args(["node", "--experimental-strip-types", "--no-warnings", "-e"])
        .args([EVAL_SCRIPT, "/pit/config.mts", "/pit/ctx.json"]);
    // The docker client passes nothing of its environment into the
    // container unless asked; it only needs to find the daemon.
    cmd.stdin(Stdio::null()).kill_on_drop(true);
    // A first use pulls the image.
    let out = match tokio::time::timeout(std::time::Duration::from_secs(120), cmd.output()).await {
        Ok(Ok(out)) => out,
        Ok(Err(e)) => bail!(
            "a fork's {CONFIG_TS} is evaluated in a container, and docker is not available ({e}); \
             use {CONFIG_JSON} for fork pull requests"
        ),
        Err(_) => {
            let _ = tokio::process::Command::new("docker")
                .args(["rm", "-f", &name])
                .output()
                .await;
            bail!("evaluating {CONFIG_TS} in {image} timed out")
        }
    };
    if !out.status.success() {
        bail!(
            "evaluating {CONFIG_TS} in {image} failed:\n{}",
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    serde_json::from_slice(&out.stdout)
        .with_context(|| format!("{CONFIG_TS} must default-export a config object"))
}

fn runtimes() -> Vec<(String, Vec<String>)> {
    let node_args = vec![
        "--experimental-strip-types".to_string(),
        "--no-warnings".to_string(),
        "-e".to_string(),
    ];
    if let Ok(bin) = std::env::var("PIT_JS_RUNTIME") {
        let args = if bin.contains("bun") {
            vec!["-e".to_string()]
        } else {
            node_args
        };
        return vec![(bin, args)];
    }
    vec![
        ("node".into(), node_args),
        ("bun".into(), vec!["-e".into()]),
    ]
}

/// Folds the `tasks` shorthand into a `default` group and fills defaults.
pub fn normalize(mut config: PitConfig) -> Result<PitConfig> {
    for (name, s) in &config.schedules {
        crate::cron::Cron::parse(&s.cron).with_context(|| format!("schedule {name}"))?;
    }
    if !config.tasks.is_empty() {
        if config.groups.contains_key("default") {
            bail!("pit config has both top-level `tasks` and a group named `default`");
        }
        let tasks = std::mem::take(&mut config.tasks);
        config.groups.insert(
            "default".into(),
            crate::model::GroupDef {
                tasks,
                ..Default::default()
            },
        );
    }
    for (gname, group) in config.groups.iter_mut() {
        if group.os.is_empty() {
            group.os.push("any".into());
        }
        for os in &group.os {
            if !matches!(os.as_str(), "any" | "linux" | "macos" | "windows") {
                bail!("group {gname}: unknown os {os:?} (use linux, macos, windows or any)");
            }
        }
        for (tname, task) in group.tasks.iter_mut() {
            if task.run.trim().is_empty() {
                bail!("task {gname}.{tname} has an empty `run`");
            }
            if task.backend.is_none() {
                task.backend = group.backend;
            }
            if task.image.is_none() {
                task.image = group.image.clone();
            }
            if task.image.as_deref().is_some_and(|i| i.starts_with('-')) {
                bail!(
                    "task {gname}.{tname}: image {:?} is not an image name",
                    task.image
                );
            }
            if !task.shard_files.is_empty() && task.shards.unwrap_or(1) < 2 {
                bail!("task {gname}.{tname}: shardFiles needs shards of 2 or more");
            }
            if task.image.is_some() && task.backend.is_none() {
                task.backend = Some(crate::model::Backend::Docker);
            }
        }
    }
    crate::graph::validate(&config)?;
    Ok(config)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn typos_in_the_config_are_errors() {
        let dir = tempfile::tempdir().unwrap();
        let bad = br#"{"tasks":{"test":{"run":"true","imputs":["src/**"]}}}"#;
        let err = format!(
            "{:#}",
            parse(CONFIG_JSON, bad, dir.path(), &Default::default(), false)
                .await
                .unwrap_err()
        );
        assert!(err.contains("unknown field `imputs`"), "{err}");
        let bad_group = br#"{"groups":{"ci":{"oss":["linux"],"tasks":{}}}}"#;
        assert!(parse(
            CONFIG_JSON,
            bad_group,
            dir.path(),
            &Default::default(),
            false
        )
        .await
        .is_err());
        for bad in [
            &br#"{"tasks":{"t":{"run":"true","shardFiles":["src/**"]}}}"#[..],
            br#"{"tasks":{"t":{"run":"true","shards":1,"shardFiles":["src/**"]}}}"#,
            br#"{"tasks":{"t":{"run":"true","image":"--privileged"}}}"#,
        ] {
            assert!(
                parse(CONFIG_JSON, bad, dir.path(), &Default::default(), false)
                    .await
                    .is_err(),
                "{}",
                String::from_utf8_lossy(bad)
            );
        }
        let good = br#"{"tasks":{"test":{"run":"true","inputs":["src/**"],"cache":false}}}"#;
        assert!(
            parse(CONFIG_JSON, good, dir.path(), &Default::default(), false)
                .await
                .is_ok()
        );
    }

    #[tokio::test]
    async fn a_sandboxed_config_sees_nothing_of_the_host() {
        // The sandbox image is a Linux one (Windows runners have Windows
        // containers only).
        if !cfg!(target_os = "linux")
            || !crate::worker::exec::detect_backends()
                .await
                .contains(&crate::model::Backend::Docker)
        {
            eprintln!("skipped: no Linux docker");
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let secret = dir.path().join("server-key");
        std::fs::write(&secret, "hunter2").unwrap();
        // The config reports what it could reach in the task's command.
        let src = format!(
            r#"
import {{ readFileSync }} from "node:fs";
let file = "blocked";
try {{ file = readFileSync({secret:?}, "utf8"); }} catch {{}}
let net = "blocked";
try {{ await fetch("http://1.1.1.1", {{ signal: AbortSignal.timeout(3000) }}); net = "open"; }} catch {{}}
export default {{ tasks: {{ t: {{ run: `${{file}} ${{net}} ${{process.env.PIT_TOKEN ?? "none"}}` }} }} }};
"#
        );
        std::env::set_var("PIT_TOKEN", "server-token");
        let config = parse(
            CONFIG_TS,
            src.as_bytes(),
            dir.path(),
            &Default::default(),
            true,
        )
        .await
        .unwrap();
        assert_eq!(
            config.groups["default"].tasks["t"].run,
            "blocked blocked none"
        );
    }

    #[tokio::test]
    async fn a_config_function_gets_the_workspace_packages() {
        if runtimes().iter().all(|(bin, _)| {
            std::process::Command::new(bin)
                .arg("--version")
                .output()
                .is_err()
        }) {
            eprintln!("skipped: no JavaScript runtime");
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let src = br#"
import type { PitConfig, PitContext } from "@pit-ci/config";
export default ({ packages }: PitContext): PitConfig => ({
  tasks: Object.fromEntries(
    packages
      .filter((p) => p.scripts.includes("typecheck"))
      .map((p) => [`typecheck:${p.name}`, { run: "npm run typecheck", inputs: p.inputs }]),
  ),
});
"#;
        let ctx = crate::workspace::Context {
            packages: vec![crate::workspace::Package {
                name: "web".into(),
                dir: "apps/web".into(),
                scripts: vec!["typecheck".into()],
                deps: vec![],
                inputs: vec!["apps/web/**".into()],
            }],
        };
        let config = parse(CONFIG_TS, src, dir.path(), &ctx, false)
            .await
            .unwrap();
        let task = &config.groups["default"].tasks["typecheck:web"];
        assert_eq!(
            task.inputs.as_deref(),
            Some(&["apps/web/**".to_string()][..])
        );
    }
}
