//! The repo's JavaScript workspace packages, handed to a `pit.config.ts` that
//! exports a function, so a monorepo config can generate one task per package
//! instead of one task for everything.
//!
//! A single `typecheck` task over a whole monorepo reruns every package when
//! any file changes. Per-package tasks rerun only the packages a change can
//! affect, the way `nx affected` or `turbo --affected` do, but a config that
//! lists them by hand drifts from the repo. Pit evaluates the config on its
//! own, without a checkout, so it reads the workspace layout for it:
//! `pnpm-workspace.yaml` or the root `package.json` `workspaces`, then each
//! package's `package.json`.

use std::collections::{BTreeMap, BTreeSet};

use globset::{Glob, GlobSetBuilder};
use serde::Serialize;

#[derive(Debug, Clone, Serialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct Package {
    /// `name` from its `package.json`.
    pub name: String,
    /// Directory relative to the repo root, e.g. `packages/shared`.
    pub dir: String,
    /// Names of its `scripts`.
    pub scripts: Vec<String>,
    /// Workspace packages it depends on directly (any dependency field).
    pub deps: Vec<String>,
    /// Input globs for a task on this package: its own directory and those
    /// of every workspace package it depends on, directly or not.
    pub inputs: Vec<String>,
}

/// What a config function receives.
#[derive(Debug, Clone, Default, Serialize)]
pub struct Context {
    pub packages: Vec<Package>,
}

/// Finds the workspace packages among `files` (every path in the tree),
/// reading files through `read`.
pub fn packages(files: &[String], read: impl Fn(&str) -> Option<Vec<u8>>) -> Vec<Package> {
    let manifests = manifest_paths(files, &read);
    package_list(&manifests, read)
}

/// The `package.json` paths of the workspace packages, from the root
/// manifests alone, so a caller can fetch just these.
pub fn manifest_paths(files: &[String], read: impl Fn(&str) -> Option<Vec<u8>>) -> Vec<String> {
    let patterns = workspace_globs(&read);
    if patterns.is_empty() {
        return vec![];
    }
    let (mut include, mut exclude) = (GlobSetBuilder::new(), GlobSetBuilder::new());
    for p in &patterns {
        let (set, p) = match p.strip_prefix('!') {
            Some(rest) => (&mut exclude, rest),
            None => (&mut include, p.as_str()),
        };
        let p = p.trim_start_matches("./").trim_end_matches('/');
        if let Ok(g) = Glob::new(&format!("{p}/package.json")) {
            set.add(g);
        }
    }
    let (Ok(include), Ok(exclude)) = (include.build(), exclude.build()) else {
        return vec![];
    };
    files
        .iter()
        .filter(|p| !p.contains("node_modules/") && include.is_match(p) && !exclude.is_match(p))
        .cloned()
        .collect()
}

/// Packages from their manifests (see `manifest_paths`).
pub fn package_list(manifests: &[String], read: impl Fn(&str) -> Option<Vec<u8>>) -> Vec<Package> {
    let mut found: BTreeMap<String, (String, Vec<String>, Vec<String>)> = BTreeMap::new();
    for path in manifests {
        let Some(json) =
            read(path).and_then(|b| serde_json::from_slice::<serde_json::Value>(&b).ok())
        else {
            continue;
        };
        let Some(name) = json["name"].as_str() else {
            continue;
        };
        let dir = path.trim_end_matches("package.json").trim_end_matches('/');
        let scripts = json["scripts"]
            .as_object()
            .map(|s| s.keys().cloned().collect())
            .unwrap_or_default();
        let deps: Vec<String> = [
            "dependencies",
            "devDependencies",
            "peerDependencies",
            "optionalDependencies",
        ]
        .iter()
        .filter_map(|f| json[*f].as_object())
        .flat_map(|o| o.keys().cloned())
        .collect();
        found.insert(name.to_string(), (dir.to_string(), scripts, deps));
    }
    let names: BTreeSet<String> = found.keys().cloned().collect();
    let direct: BTreeMap<String, Vec<String>> = found
        .iter()
        .map(|(n, (_, _, deps))| {
            let mut d: Vec<String> = deps
                .iter()
                .filter(|d| names.contains(*d) && *d != n)
                .cloned()
                .collect();
            d.sort();
            d.dedup();
            (n.clone(), d)
        })
        .collect();
    found
        .iter()
        .map(|(name, (dir, scripts, _))| {
            // Every package reachable through workspace dependencies.
            let mut seen = BTreeSet::from([name.clone()]);
            let mut stack = vec![name.clone()];
            while let Some(n) = stack.pop() {
                for d in &direct[&n] {
                    if seen.insert(d.clone()) {
                        stack.push(d.clone());
                    }
                }
            }
            // Its own directory first, then its dependencies'.
            let mut dep_dirs: Vec<&String> = seen
                .iter()
                .filter(|n| *n != name)
                .map(|n| &found[n].0)
                .collect();
            dep_dirs.sort();
            let inputs = std::iter::once(dir)
                .chain(dep_dirs)
                .map(|d| format!("{d}/**"))
                .collect();
            Package {
                name: name.clone(),
                dir: dir.clone(),
                scripts: scripts.clone(),
                deps: direct[name].clone(),
                inputs,
            }
        })
        .collect()
}

fn workspace_globs(read: &impl Fn(&str) -> Option<Vec<u8>>) -> Vec<String> {
    if let Some(yaml) = read("pnpm-workspace.yaml")
        .and_then(|b| serde_yaml::from_slice::<serde_yaml::Value>(&b).ok())
    {
        if let Some(list) = yaml["packages"].as_sequence() {
            return list
                .iter()
                .filter_map(|v| v.as_str().map(String::from))
                .collect();
        }
    }
    let Some(root) =
        read("package.json").and_then(|b| serde_json::from_slice::<serde_json::Value>(&b).ok())
    else {
        return vec![];
    };
    // `"workspaces": [...]` (npm, Yarn, Bun) or `{ "packages": [...] }` (Yarn).
    let list = root["workspaces"]
        .as_array()
        .or_else(|| root["workspaces"]["packages"].as_array());
    list.map(|l| {
        l.iter()
            .filter_map(|v| v.as_str().map(String::from))
            .collect()
    })
    .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    #[test]
    fn finds_packages_and_their_transitive_inputs() {
        let tree: HashMap<&str, &str> = HashMap::from([
            (
                "pnpm-workspace.yaml",
                "packages:\n  - apps/*\n  - packages/*\n  - '!packages/skip'\n",
            ),
            ("package.json", r#"{"name":"root"}"#),
            (
                "apps/web/package.json",
                r#"{"name":"@x/web","scripts":{"test":"t","typecheck":"tc"},"dependencies":{"@x/ui":"workspace:*","react":"19"}}"#,
            ),
            (
                "packages/ui/package.json",
                r#"{"name":"@x/ui","devDependencies":{"@x/shared":"workspace:*"}}"#,
            ),
            ("packages/shared/package.json", r#"{"name":"@x/shared"}"#),
            ("packages/skip/package.json", r#"{"name":"@x/skip"}"#),
            (
                "packages/ui/node_modules/dep/package.json",
                r#"{"name":"dep"}"#,
            ),
            ("docs/package.json", r#"{"name":"docs"}"#),
        ]);
        let files: Vec<String> = tree.keys().map(|k| k.to_string()).collect();
        let pkgs = packages(&files, |p| tree.get(p).map(|s| s.as_bytes().to_vec()));
        let names: Vec<&str> = pkgs.iter().map(|p| p.name.as_str()).collect();
        assert_eq!(names, ["@x/shared", "@x/ui", "@x/web"]);
        let web = &pkgs[2];
        assert_eq!(web.dir, "apps/web");
        assert_eq!(web.scripts, ["test", "typecheck"]);
        assert_eq!(web.deps, ["@x/ui"]);
        assert_eq!(
            web.inputs,
            ["apps/web/**", "packages/shared/**", "packages/ui/**"]
        );
    }

    #[test]
    fn reads_package_json_workspaces() {
        let tree: HashMap<&str, &str> = HashMap::from([
            ("package.json", r#"{"workspaces":{"packages":["libs/*"]}}"#),
            ("libs/a/package.json", r#"{"name":"a"}"#),
        ]);
        let files: Vec<String> = tree.keys().map(|k| k.to_string()).collect();
        let pkgs = packages(&files, |p| tree.get(p).map(|s| s.as_bytes().to_vec()));
        assert_eq!(pkgs.len(), 1);
        assert_eq!(pkgs[0].inputs, ["libs/a/**"]);
    }
}
