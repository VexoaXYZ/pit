//! The task graph: validation, planning groups per OS, selecting tasks,
//! affected-task computation and action keys.

use std::collections::{BTreeMap, BTreeSet};

use anyhow::{bail, Result};
use globset::{Glob, GlobSet, GlobSetBuilder};
use serde::Serialize;

use crate::model::{PitConfig, TaskDef};

/// Checks that deps exist within their group and that there are no cycles.
pub fn validate(config: &PitConfig) -> Result<()> {
    for (gname, group) in &config.groups {
        for (tname, task) in &group.tasks {
            for dep in &task.deps {
                if !group.tasks.contains_key(dep) {
                    bail!(
                        "task {gname}.{tname} depends on {dep:?}, which is not a task in group {gname} \
                         (deps must be in the same group)"
                    );
                }
            }
            for pattern in task.inputs.iter().flatten().chain(&task.outputs) {
                Glob::new(pattern.strip_prefix('!').unwrap_or(pattern))
                    .map_err(|e| anyhow::anyhow!("task {gname}.{tname}: bad glob {pattern:?}: {e}"))?;
            }
        }
        topo_order(&group.tasks).map_err(|e| anyhow::anyhow!("group {gname}: {e}"))?;
    }
    Ok(())
}

/// Tasks in dependency order. Errors on a cycle.
pub fn topo_order(tasks: &BTreeMap<String, TaskDef>) -> Result<Vec<String>> {
    let mut order = Vec::new();
    let mut state: BTreeMap<&str, u8> = BTreeMap::new(); // 1 = visiting, 2 = done
    fn visit<'a>(
        name: &'a str,
        tasks: &'a BTreeMap<String, TaskDef>,
        state: &mut BTreeMap<&'a str, u8>,
        order: &mut Vec<String>,
        stack: &mut Vec<&'a str>,
    ) -> Result<()> {
        match state.get(name) {
            Some(2) => return Ok(()),
            Some(1) => {
                stack.push(name);
                bail!("dependency cycle: {}", stack.join(" -> "));
            }
            _ => {}
        }
        state.insert(name, 1);
        stack.push(name);
        if let Some(task) = tasks.get(name) {
            for dep in &task.deps {
                visit(dep, tasks, state, order, stack)?;
            }
        }
        stack.pop();
        state.insert(name, 2);
        order.push(name.to_string());
        Ok(())
    }
    for name in tasks.keys() {
        visit(name, tasks, &mut state, &mut order, &mut Vec::new())?;
    }
    Ok(order)
}

/// A group instance to schedule: one per group per OS.
#[derive(Debug, Clone, Serialize)]
pub struct PlannedGroup {
    pub name: String,
    pub os: String,
    /// Task name and definition, in dependency order.
    pub tasks: Vec<(String, TaskDef)>,
}

/// Expands the config into schedulable groups. `selected` filters tasks by
/// name (`task` or `group.task`); their deps are always included.
pub fn plan(config: &PitConfig, selected: &[String]) -> Result<Vec<PlannedGroup>> {
    let mut groups = Vec::new();
    let mut matched: BTreeSet<&str> = BTreeSet::new();
    for (gname, group) in &config.groups {
        let mut keep: BTreeSet<String> = BTreeSet::new();
        if selected.is_empty() {
            keep.extend(group.tasks.keys().cloned());
        } else {
            for sel in selected {
                let (g, t) = match sel.split_once('.') {
                    Some((g, t)) => (Some(g), t),
                    None => (None, sel.as_str()),
                };
                if g.is_some_and(|g| g != gname) || !group.tasks.contains_key(t) {
                    continue;
                }
                matched.insert(sel);
                add_with_deps(t, &group.tasks, &mut keep);
            }
        }
        if keep.is_empty() {
            continue;
        }
        let order = topo_order(&group.tasks)?;
        let tasks: Vec<(String, TaskDef)> = order
            .into_iter()
            .filter(|t| keep.contains(t))
            .map(|t| (t.clone(), group.tasks[&t].clone()))
            .collect();
        for os in &group.os {
            let name = if group.os.len() > 1 { format!("{gname}@{os}") } else { gname.clone() };
            groups.push(PlannedGroup { name, os: os.clone(), tasks: tasks.clone() });
        }
    }
    if let Some(missing) = selected.iter().find(|s| !matched.contains(s.as_str())) {
        bail!("no task named {missing:?} in the pit config");
    }
    Ok(groups)
}

fn add_with_deps(name: &str, tasks: &BTreeMap<String, TaskDef>, keep: &mut BTreeSet<String>) {
    if !keep.insert(name.to_string()) {
        return;
    }
    for dep in &tasks[name].deps {
        add_with_deps(dep, tasks, keep);
    }
}

pub fn globset(patterns: &[String]) -> Result<GlobSet> {
    let mut b = GlobSetBuilder::new();
    for p in patterns {
        b.add(Glob::new(p)?);
    }
    Ok(b.build()?)
}

/// Input globs with gitignore-style exclusions: `["**", "!docs/**"]`.
pub struct InputMatcher {
    include: GlobSet,
    exclude: GlobSet,
}

impl InputMatcher {
    pub fn new(patterns: &[String]) -> Result<InputMatcher> {
        let (neg, pos): (Vec<String>, Vec<String>) = patterns.iter().cloned().partition(|p| p.starts_with('!'));
        let neg: Vec<String> = neg.into_iter().map(|p| p[1..].to_string()).collect();
        Ok(InputMatcher { include: globset(&pos)?, exclude: globset(&neg)? })
    }

    pub fn for_task(task: &TaskDef) -> Result<InputMatcher> {
        InputMatcher::new(&input_globs(task))
    }

    pub fn is_match(&self, path: &str) -> bool {
        self.include.is_match(path) && !self.exclude.is_match(path)
    }
}

/// Inputs default to every tracked file, which is always correct, just less
/// cacheable.
pub fn input_globs(task: &TaskDef) -> Vec<String> {
    task.inputs.clone().unwrap_or_else(|| vec!["**".to_string()])
}

/// Tasks (`group.task`) whose results can change because of `changed` files:
/// tasks with a matching input, plus everything that depends on them.
pub fn affected(config: &PitConfig, changed: &[String]) -> Result<BTreeSet<String>> {
    let mut out = BTreeSet::new();
    for (gname, group) in &config.groups {
        let mut hit: BTreeSet<String> = BTreeSet::new();
        for (tname, task) in &group.tasks {
            let set = InputMatcher::for_task(task)?;
            if changed.iter().any(|f| set.is_match(f)) {
                hit.insert(tname.clone());
            }
        }
        // Propagate to dependents until nothing changes.
        loop {
            let before = hit.len();
            for (tname, task) in &group.tasks {
                if task.deps.iter().any(|d| hit.contains(d)) {
                    hit.insert(tname.clone());
                }
            }
            if hit.len() == before {
                break;
            }
        }
        out.extend(hit.into_iter().map(|t| format!("{gname}.{t}")));
    }
    // A config change can change any task.
    if changed.iter().any(|f| crate::config::CONFIG_FILES.contains(&f.as_str())) {
        for (gname, group) in &config.groups {
            out.extend(group.tasks.keys().map(|t| format!("{gname}.{t}")));
        }
    }
    Ok(out)
}

/// Identity of one input file: a git blob oid for tracked files, or a BLAKE3
/// digest for overlay files.
#[derive(Debug, Clone, Serialize, PartialEq, Eq, PartialOrd, Ord)]
pub struct InputFile<'a> {
    pub path: &'a str,
    pub id: &'a str,
}

#[derive(Serialize)]
struct KeyMaterial<'a> {
    v: u32,
    run: &'a str,
    os: &'a str,
    arch: &'a str,
    inputs: &'a [String],
    outputs: &'a [String],
    backend: Option<&'a str>,
    image: Option<&'a str>,
    env: Vec<(&'a str, String)>,
    deps: Vec<&'a str>,
    files: Vec<InputFile<'a>>,
}

/// The action key: a hash of everything that can change a task's result.
pub fn action_key(
    task: &TaskDef,
    os: &str,
    files: &[(String, String)],
    dep_keys: &[String],
) -> Result<String> {
    let inputs = input_globs(task);
    let set = InputMatcher::new(&inputs)?;
    let mut matched: Vec<InputFile> = files
        .iter()
        .filter(|(p, _)| set.is_match(p))
        .map(|(p, id)| InputFile { path: p, id })
        .collect();
    matched.sort();
    let mut env: Vec<(&str, String)> = task
        .env
        .iter()
        .map(|k| (k.as_str(), std::env::var(k).unwrap_or_default()))
        .collect();
    env.sort();
    let mut deps: Vec<&str> = dep_keys.iter().map(String::as_str).collect();
    deps.sort();
    let material = KeyMaterial {
        v: 1,
        run: &task.run,
        os,
        arch: std::env::consts::ARCH,
        inputs: &inputs,
        outputs: &task.outputs,
        backend: task.backend.map(|b| b.as_str()),
        image: task.image.as_deref(),
        env,
        deps,
        files: matched,
    };
    Ok(crate::cas::digest(&serde_json::to_vec(&material)?))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::GroupDef;

    fn task(run: &str, deps: &[&str], inputs: Option<&[&str]>) -> TaskDef {
        TaskDef {
            run: run.into(),
            deps: deps.iter().map(|s| s.to_string()).collect(),
            inputs: inputs.map(|i| i.iter().map(|s| s.to_string()).collect()),
            ..Default::default()
        }
    }

    fn sample() -> PitConfig {
        let mut tasks = BTreeMap::new();
        tasks.insert("install".into(), task("bun i", &[], Some(&["package.json", "bun.lock"])));
        tasks.insert("lint".into(), task("lint", &["install"], Some(&["src/**"])));
        tasks.insert("build".into(), task("build", &["install"], Some(&["src/**", "vite.config.ts"])));
        tasks.insert("docs".into(), task("docs", &[], Some(&["docs/**"])));
        let mut groups = BTreeMap::new();
        groups.insert(
            "ci".into(),
            GroupDef { os: vec!["linux".into(), "windows".into()], tasks, ..Default::default() },
        );
        PitConfig { groups, ..Default::default() }
    }

    #[test]
    fn plans_one_group_per_os_in_dependency_order() {
        let groups = plan(&sample(), &[]).unwrap();
        assert_eq!(groups.len(), 2);
        assert_eq!(groups[0].name, "ci@linux");
        let names: Vec<_> = groups[0].tasks.iter().map(|(n, _)| n.as_str()).collect();
        let pos = |n: &str| names.iter().position(|x| *x == n).unwrap();
        assert!(pos("install") < pos("lint"));
        assert!(pos("install") < pos("build"));
    }

    #[test]
    fn selecting_a_task_pulls_in_its_deps_only() {
        let groups = plan(&sample(), &["lint".into()]).unwrap();
        let names: Vec<_> = groups[0].tasks.iter().map(|(n, _)| n.clone()).collect();
        assert_eq!(names, vec!["install", "lint"]);
        assert!(plan(&sample(), &["nope".into()]).is_err());
    }

    #[test]
    fn affected_follows_inputs_and_dependents() {
        let c = sample();
        let a = affected(&c, &["docs/intro.md".into()]).unwrap();
        assert_eq!(a.into_iter().collect::<Vec<_>>(), vec!["ci.docs"]);
        let a = affected(&c, &["bun.lock".into()]).unwrap();
        assert_eq!(a.len(), 3, "install change affects lint and build: {a:?}");
        assert!(affected(&c, &["README.md".into()]).unwrap().is_empty());
        assert_eq!(affected(&c, &["pit.config.ts".into()]).unwrap().len(), 4);
    }

    #[test]
    fn input_exclusions() {
        let m = InputMatcher::new(&["**".into(), "!**/*.md".into(), "!docs/**".into()]).unwrap();
        assert!(m.is_match("src/a.ts"));
        assert!(!m.is_match("README.md"));
        assert!(!m.is_match("docs/x.ts"));
    }

    #[test]
    fn rejects_cycles_and_cross_group_deps() {
        let mut c = sample();
        let g = c.groups.get_mut("ci").unwrap();
        g.tasks.get_mut("install").unwrap().deps.push("build".into());
        assert!(validate(&c).unwrap_err().to_string().contains("cycle"));
        let mut c = sample();
        c.groups.get_mut("ci").unwrap().tasks.get_mut("lint").unwrap().deps.push("other".into());
        assert!(validate(&c).is_err());
    }

    #[test]
    fn action_key_only_changes_with_matching_inputs() {
        let t = task("build", &[], Some(&["src/**"]));
        let files = vec![("src/a.ts".to_string(), "1".to_string()), ("README.md".to_string(), "1".to_string())];
        let k1 = action_key(&t, "linux", &files, &[]).unwrap();
        let readme_changed = vec![("src/a.ts".to_string(), "1".to_string()), ("README.md".to_string(), "2".to_string())];
        assert_eq!(k1, action_key(&t, "linux", &readme_changed, &[]).unwrap());
        let src_changed = vec![("src/a.ts".to_string(), "2".to_string())];
        assert_ne!(k1, action_key(&t, "linux", &src_changed, &[]).unwrap());
        assert_ne!(k1, action_key(&t, "linux", &files, &["dep".into()]).unwrap());
        assert_ne!(k1, action_key(&t, "windows", &files, &[]).unwrap());
    }
}
