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
import(pathToFileURL(process.argv[process.argv.length - 1]).href).then(
  (m) => {
    let c = m.default ?? m.config ?? m;
    if (typeof c === "function") c = c();
    return Promise.resolve(c).then((v) => process.stdout.write(JSON.stringify(v)));
  },
  (e) => { console.error(String(e && e.stack || e)); process.exit(1); },
);
"#;

fn cache() -> &'static Mutex<HashMap<String, PitConfig>> {
    static CACHE: OnceLock<Mutex<HashMap<String, PitConfig>>> = OnceLock::new();
    CACHE.get_or_init(Default::default)
}

/// Parses config source. `name` is the file name it came from, which decides
/// whether it is JSON or TypeScript. Results are memoized by content hash.
pub async fn parse(name: &str, source: &[u8], scratch: &Path) -> Result<PitConfig> {
    let key = format!("{name}:{}", crate::cas::digest(source));
    if let Some(c) = cache().lock().get(&key) {
        return Ok(c.clone());
    }
    let config = if name.ends_with(".json") {
        serde_json::from_slice::<PitConfig>(source).context("parsing pit.config.json")?
    } else {
        eval_ts(source, scratch).await?
    };
    let config = normalize(config)?;
    cache().lock().insert(key, config.clone());
    Ok(config)
}

async fn eval_ts(source: &[u8], scratch: &Path) -> Result<PitConfig> {
    std::fs::create_dir_all(scratch)?;
    // `.mts` makes both Node and Bun treat the file as an ES module.
    let file = scratch.join(format!(
        "pit-config-{}.mts",
        &crate::cas::digest(source)[..16]
    ));
    std::fs::write(&file, source)?;
    let file_arg = file.to_string_lossy().to_string();

    let mut last_err = String::new();
    for (bin, args) in runtimes() {
        let mut cmd = tokio::process::Command::new(&bin);
        cmd.args(&args).arg(EVAL_SCRIPT).arg(&file_arg);
        cmd.stdin(Stdio::null()).kill_on_drop(true);
        let out = match tokio::time::timeout(std::time::Duration::from_secs(30), cmd.output()).await
        {
            Ok(Ok(out)) => out,
            Ok(Err(e)) => {
                last_err = format!("{bin}: {e}");
                continue;
            }
            Err(_) => bail!("evaluating {CONFIG_TS} with {bin} timed out"),
        };
        let _ = std::fs::remove_file(&file);
        if !out.status.success() {
            bail!(
                "evaluating {CONFIG_TS} with {bin} failed:\n{}",
                String::from_utf8_lossy(&out.stderr).trim()
            );
        }
        return serde_json::from_slice(&out.stdout)
            .with_context(|| format!("{CONFIG_TS} must default-export a config object"));
    }
    let _ = std::fs::remove_file(&file);
    bail!(
        "no JavaScript runtime found to evaluate {CONFIG_TS} (tried node and bun; set PIT_JS_RUNTIME, \
         or use {CONFIG_JSON}): {last_err}"
    )
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
            if task.image.is_some() && task.backend.is_none() {
                task.backend = Some(crate::model::Backend::Docker);
            }
        }
    }
    crate::graph::validate(&config)?;
    Ok(config)
}

/// Reads the config from a checked-out directory.
pub async fn load_dir(dir: &Path, scratch: &Path) -> Result<PitConfig> {
    for name in CONFIG_FILES {
        let path = dir.join(name);
        if path.exists() {
            let src = std::fs::read(&path)?;
            return parse(name, &src, scratch).await;
        }
    }
    bail!("no {CONFIG_TS} or {CONFIG_JSON} in {}", dir.display())
}
