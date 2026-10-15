//! `pit import`: a best-effort, one-way translation of GitHub Actions
//! workflows into `pit.config.ts`. Each job becomes a group, each `run` step a
//! task chained to the previous one. Anything Pit does natively (checkout,
//! toolchain setup, caching) is dropped; anything else is left as a TODO.

use std::collections::BTreeMap;
use std::path::Path;

use anyhow::{bail, Context, Result};
use serde_yaml::Value;

#[derive(Debug, Clone)]
pub struct TsTask {
    pub name: String,
    pub run: String,
    pub deps: Vec<String>,
    pub inputs: Option<Vec<String>>,
    pub outputs: Vec<String>,
    pub cache: Option<String>,
    pub comments: Vec<String>,
}

pub fn ts_string(s: &str) -> String {
    serde_json::to_string(s).unwrap_or_else(|_| "\"\"".into())
}

/// Actions Pit replaces natively.
const NATIVE_ACTIONS: &[&str] = &[
    "actions/checkout",
    "actions/cache",
    "actions/setup-node",
    "actions/setup-python",
    "actions/setup-go",
    "actions/setup-java",
    "oven-sh/setup-bun",
    "pnpm/action-setup",
    "dtolnay/rust-toolchain",
    "actions-rs/toolchain",
    "Swatinem/rust-cache",
    "voidzero-dev/setup-vp",
];

pub fn import(dir: &Path, stdout: bool, force: bool) -> Result<i32> {
    let wf_dir = dir.join(".github").join("workflows");
    let mut files: Vec<_> = std::fs::read_dir(&wf_dir)
        .with_context(|| format!("reading {}", wf_dir.display()))?
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|e| e == "yml" || e == "yaml"))
        .collect();
    files.sort();
    let mut groups = Vec::new();
    let mut skipped = Vec::new();
    for f in &files {
        let src = std::fs::read_to_string(f)?;
        let stem = f.file_stem().unwrap().to_string_lossy().to_string();
        match translate(&stem, &src) {
            Ok(g) if g.is_empty() => skipped.push(format!("{stem}: no run steps")),
            Ok(g) => groups.extend(g),
            Err(e) => skipped.push(format!("{stem}: {e:#}")),
        }
    }
    if groups.is_empty() {
        bail!("no translatable jobs in {}", wf_dir.display());
    }
    let mut source = crate::init::render(&groups);
    if !skipped.is_empty() {
        source.push_str("\n// Not imported:\n");
        for s in &skipped {
            source.push_str(&format!("// - {s}\n"));
        }
    }
    if stdout {
        print!("{source}");
        return Ok(0);
    }
    let dest = dir.join(crate::config::CONFIG_TS);
    if dest.exists() && !force {
        bail!("{} already exists (use --force, or --stdout to preview)", dest.display());
    }
    std::fs::write(&dest, source)?;
    println!("wrote {} with {} group(s) from {} workflow(s)", dest.display(), groups.len(), files.len());
    for s in skipped {
        println!("  skipped {s}");
    }
    println!("review the TODO comments, then tighten each task's `inputs` so unrelated changes hit the cache");
    Ok(0)
}

fn slug(s: &str) -> String {
    let mut out = String::new();
    for c in s.chars() {
        if c.is_ascii_alphanumeric() {
            out.push(c.to_ascii_lowercase());
        } else if !out.ends_with('-') && !out.is_empty() {
            out.push('-');
        }
    }
    out.trim_end_matches('-').to_string()
}

fn os_of(runs_on: &str) -> &'static str {
    let r = runs_on.to_ascii_lowercase();
    if r.contains("windows") {
        "windows"
    } else if r.contains("macos") || r.contains("mac-") {
        "macos"
    } else {
        "linux"
    }
}

/// Translates one workflow file into `(group, os list, tasks)` entries.
pub fn translate(stem: &str, src: &str) -> Result<Vec<(String, Vec<String>, Vec<TsTask>)>> {
    let doc: Value = serde_yaml::from_str(src)?;
    let jobs = doc.get("jobs").and_then(Value::as_mapping).context("no jobs")?;
    let mut out = Vec::new();
    for (id, job) in jobs {
        let id = id.as_str().unwrap_or("job");
        let mut oses: Vec<String> = Vec::new();
        let runs_on = match job.get("runs-on") {
            Some(Value::String(s)) => s.clone(),
            Some(Value::Sequence(seq)) => seq.iter().filter_map(Value::as_str).collect::<Vec<_>>().join(" "),
            _ => String::new(),
        };
        if runs_on.contains("matrix.") {
            let matrix_os = job
                .get("strategy")
                .and_then(|s| s.get("matrix"))
                .and_then(|m| m.get("os"))
                .and_then(Value::as_sequence)
                .cloned()
                .unwrap_or_default();
            for o in matrix_os.iter().filter_map(Value::as_str) {
                let os = os_of(o).to_string();
                if !oses.contains(&os) {
                    oses.push(os);
                }
            }
        }
        if oses.is_empty() {
            oses.push(os_of(&runs_on).to_string());
        }

        let mut tasks: Vec<TsTask> = Vec::new();
        let mut pending_comments: Vec<String> = Vec::new();
        if let Some(needs) = job.get("needs") {
            let needs: Vec<String> = match needs {
                Value::String(s) => vec![s.clone()],
                Value::Sequence(v) => v.iter().filter_map(Value::as_str).map(String::from).collect(),
                _ => vec![],
            };
            if !needs.is_empty() {
                pending_comments.push(format!(
                    "TODO: job needed {} first. Pit groups run in parallel; move shared steps into this group if order matters.",
                    needs.join(", ")
                ));
            }
        }
        let steps = job.get("steps").and_then(Value::as_sequence).cloned().unwrap_or_default();
        let mut names = BTreeMap::<String, usize>::new();
        for (i, step) in steps.iter().enumerate() {
            let label = step.get("name").and_then(Value::as_str).map(String::from);
            if let Some(uses) = step.get("uses").and_then(Value::as_str) {
                let action = uses.split('@').next().unwrap_or(uses);
                if !NATIVE_ACTIONS.contains(&action) {
                    pending_comments.push(format!("TODO: `uses: {uses}` was not translated."));
                }
                continue;
            }
            let Some(run) = step.get("run").and_then(Value::as_str) else { continue };
            let mut run = run.trim().to_string();
            if let Some(dir) = step.get("working-directory").and_then(Value::as_str) {
                run = format!("cd {} && {run}", shell_quote(dir));
            }
            let mut base = slug(label.as_deref().unwrap_or(""));
            if base.is_empty() {
                base = format!("step-{}", i + 1);
            }
            let n = names.entry(base.clone()).or_insert(0);
            *n += 1;
            let name = if *n > 1 { format!("{base}-{n}") } else { base };
            let mut comments = std::mem::take(&mut pending_comments);
            if run.contains("${{") {
                comments.push("TODO: uses a GitHub expression (${{ }}); replace it with a literal or an env var.".into());
            }
            let install = ["npm ci", "npm install", "bun install", "pnpm install", "yarn install", "yarn --frozen", "vp install"]
                .iter()
                .any(|p| run.starts_with(p));
            let deps = tasks.last().map(|t: &TsTask| vec![t.name.clone()]).unwrap_or_default();
            tasks.push(TsTask {
                name,
                run,
                deps,
                inputs: None,
                outputs: vec![],
                cache: install.then(|| "workspace".to_string()),
                comments,
            });
        }
        if tasks.is_empty() {
            continue;
        }
        let group = if jobs.len() == 1 && slug(id) == "build" { stem.to_string() } else { id.to_string() };
        out.push((group, oses, tasks));
    }
    Ok(out)
}

fn shell_quote(s: &str) -> String {
    if s.chars().all(|c| c.is_ascii_alphanumeric() || "/._-".contains(c)) {
        s.to_string()
    } else {
        format!("'{}'", s.replace('\'', "'\\''"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const WF: &str = r#"
name: CI
on: [push]
jobs:
  test:
    runs-on: ${{ matrix.os }}
    strategy:
      matrix:
        os: [ubuntu-latest, windows-latest]
    steps:
      - uses: actions/checkout@v4
      - uses: oven-sh/setup-bun@v2
      - run: bun install --frozen-lockfile
      - name: Lint code
        run: bun run lint
      - name: Test
        working-directory: packages/app
        run: bun test
      - uses: some/deploy@v1
  release:
    runs-on: macos-14
    needs: test
    steps:
      - run: echo ${{ github.sha }}
"#;

    #[test]
    fn translates_jobs_steps_and_matrix() {
        let groups = translate("ci", WF).unwrap();
        assert_eq!(groups.len(), 2);
        let (name, os, tasks) = &groups[0];
        assert_eq!(name, "test");
        assert_eq!(os, &vec!["linux".to_string(), "windows".to_string()]);
        let names: Vec<_> = tasks.iter().map(|t| t.name.as_str()).collect();
        assert_eq!(names, vec!["step-3", "lint-code", "test"]);
        assert_eq!(tasks[0].cache.as_deref(), Some("workspace"));
        assert_eq!(tasks[1].deps, vec!["step-3"]);
        assert_eq!(tasks[2].run, "cd packages/app && bun test");
        let (_, os, tasks) = &groups[1];
        assert_eq!(os, &vec!["macos".to_string()]);
        assert!(tasks[0].comments.iter().any(|c| c.contains("needed test")));
        assert!(tasks[0].comments.iter().any(|c| c.contains("expression")));
    }
}
