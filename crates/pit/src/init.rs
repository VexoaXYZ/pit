//! `pit init`: writes a starter `pit.config.ts` from what the repo contains.

use std::path::Path;

use anyhow::{bail, Result};

use crate::import::{ts_string, TsTask};

pub async fn init(dir: &Path, force: bool) -> Result<i32> {
    let root = crate::git::toplevel(dir).await.unwrap_or_else(|_| dir.to_path_buf());
    let dest = root.join(crate::config::CONFIG_TS);
    if dest.exists() && !force {
        bail!("{} already exists (use --force to overwrite)", dest.display());
    }
    let tasks = detect(&root);
    let source = render(&[("ci".to_string(), vec!["linux".to_string()], tasks)]);
    std::fs::write(&dest, source)?;
    println!("wrote {}", dest.display());
    println!("next: `pit up` in another terminal, then `pit run`");
    Ok(0)
}

const SOURCE_INPUTS: &[&str] = &["**", "!**/*.md", "!docs/**", "!.github/**"];

pub fn detect(root: &Path) -> Vec<TsTask> {
    let mut tasks = Vec::new();
    if let Ok(pkg) = std::fs::read_to_string(root.join("package.json")) {
        let pkg: serde_json::Value = serde_json::from_str(&pkg).unwrap_or_default();
        let (pm, install, lock) = if root.join("bun.lock").exists() || root.join("bun.lockb").exists() {
            ("bun", "bun install --frozen-lockfile", if root.join("bun.lock").exists() { "bun.lock" } else { "bun.lockb" })
        } else if root.join("pnpm-lock.yaml").exists() {
            ("pnpm", "pnpm install --frozen-lockfile", "pnpm-lock.yaml")
        } else if root.join("yarn.lock").exists() {
            ("yarn", "yarn install --frozen-lockfile", "yarn.lock")
        } else {
            ("npm", "npm ci", "package-lock.json")
        };
        tasks.push(TsTask {
            name: "install".into(),
            run: install.into(),
            deps: vec![],
            inputs: Some(vec!["**/package.json".into(), lock.into()]),
            outputs: vec![],
            cache: Some("workspace".into()),
            comments: vec!["Dependencies stay installed in the warm workspace; this reruns only when manifests change.".into()],
        });
        let scripts = pkg["scripts"].as_object().cloned().unwrap_or_default();
        for script in ["lint", "check", "typecheck", "test", "build"] {
            if scripts.contains_key(script) {
                let outputs = if script == "build" { vec!["dist/**".to_string()] } else { vec![] };
                tasks.push(TsTask {
                    name: script.into(),
                    run: format!("{pm} run {script}"),
                    deps: vec!["install".into()],
                    inputs: Some(SOURCE_INPUTS.iter().map(|s| s.to_string()).collect()),
                    outputs,
                    cache: None,
                    comments: vec![],
                });
            }
        }
    }
    if root.join("Cargo.toml").exists() {
        let rust_inputs: Vec<String> = vec!["**/*.rs".into(), "**/Cargo.toml".into(), "Cargo.lock".into()];
        for (name, run) in [
            ("fmt", "cargo fmt --all --check"),
            ("clippy", "cargo clippy --all-targets -- -D warnings"),
            ("test", "cargo test"),
        ] {
            let name = if tasks.iter().any(|t| t.name == name) { format!("cargo-{name}") } else { name.to_string() };
            tasks.push(TsTask {
                name,
                run: run.into(),
                deps: vec![],
                inputs: Some(rust_inputs.clone()),
                outputs: vec![],
                cache: None,
                comments: vec![],
            });
        }
    }
    if root.join("go.mod").exists() {
        for (name, run) in [("vet", "go vet ./..."), ("test", "go test ./...")] {
            let name = if tasks.iter().any(|t| t.name == name) { format!("go-{name}") } else { name.to_string() };
            tasks.push(TsTask {
                name,
                run: run.into(),
                deps: vec![],
                inputs: Some(vec!["**/*.go".into(), "go.mod".into(), "go.sum".into()]),
                outputs: vec![],
                cache: None,
                comments: vec![],
            });
        }
    }
    if tasks.is_empty() {
        tasks.push(TsTask {
            name: "hello".into(),
            run: "echo 'Edit pit.config.ts to add your tasks'".into(),
            deps: vec![],
            inputs: None,
            outputs: vec![],
            cache: None,
            comments: vec![],
        });
    }
    tasks
}

/// Renders groups of tasks as a `pit.config.ts`.
pub fn render(groups: &[(String, Vec<String>, Vec<TsTask>)]) -> String {
    let mut s = String::new();
    s.push_str("// Pit config. Docs: https://github.com/VexoaXYZ/pit#config\n");
    s.push_str("// Each group runs in one warm workspace. Tasks hash their `inputs`, so unchanged\n");
    s.push_str("// tasks are served from the cache instead of running again.\n");
    s.push_str("import type { PitConfig } from \"@pit-ci/config\";\n\n");
    s.push_str("export default {\n  groups: {\n");
    for (gname, os, tasks) in groups {
        s.push_str(&format!("    {}: {{\n", ts_key(gname)));
        s.push_str(&format!("      os: [{}],\n", os.iter().map(|o| ts_string(o)).collect::<Vec<_>>().join(", ")));
        s.push_str("      tasks: {\n");
        for t in tasks {
            for c in &t.comments {
                s.push_str(&format!("        // {c}\n"));
            }
            s.push_str(&format!("        {}: {{\n", ts_key(&t.name)));
            s.push_str(&format!("          run: {},\n", ts_string(&t.run)));
            if !t.deps.is_empty() {
                s.push_str(&format!("          deps: [{}],\n", t.deps.iter().map(|d| ts_string(d)).collect::<Vec<_>>().join(", ")));
            }
            if let Some(inputs) = &t.inputs {
                s.push_str(&format!("          inputs: [{}],\n", inputs.iter().map(|d| ts_string(d)).collect::<Vec<_>>().join(", ")));
            }
            if !t.outputs.is_empty() {
                s.push_str(&format!("          outputs: [{}],\n", t.outputs.iter().map(|d| ts_string(d)).collect::<Vec<_>>().join(", ")));
            }
            if let Some(cache) = &t.cache {
                s.push_str(&format!("          cache: {},\n", ts_string(cache)));
            }
            s.push_str("        },\n");
        }
        s.push_str("      },\n    },\n");
    }
    s.push_str("  },\n} satisfies PitConfig;\n");
    s
}

fn ts_key(k: &str) -> String {
    let ident = !k.is_empty()
        && k.chars().all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '$')
        && !k.starts_with(|c: char| c.is_ascii_digit());
    if ident { k.to_string() } else { ts_string(k) }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_bun_scripts() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("package.json"), r#"{"scripts":{"lint":"x","test":"y","dev":"z"}}"#).unwrap();
        std::fs::write(dir.path().join("bun.lock"), "").unwrap();
        let tasks = detect(dir.path());
        let names: Vec<_> = tasks.iter().map(|t| t.name.as_str()).collect();
        assert_eq!(names, vec!["install", "lint", "test"]);
        assert_eq!(tasks[1].run, "bun run lint");
        let src = render(&[("ci".into(), vec!["linux".into()], tasks)]);
        assert!(src.contains("cache: \"workspace\""));
    }
}
